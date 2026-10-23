use std::{collections::BTreeMap, sync::Arc, time::Instant};

use chrono::{DateTime, Utc};
use memhub_audit::{Actor, AuditEvent, MemoryAction};
use memhub_store::{
    validate_label, validate_name, Change, Filter, GroupKey, Memory, MemoryId, MemoryMeta,
    MemoryPatch, NewMemory, Scope, SearchHit, SearchQuery,
};
use serde::Serialize;

use crate::{
    diff::unified_diff,
    error::{AppError, Result},
    hub::Hub,
    registry::{AgentProfile, RepoProfile},
};

/// Transport-level facts about a connection.
#[derive(Debug, Clone)]
pub struct ConnInfo {
    /// `http`, `stdio`, ...
    pub transport: String,
    /// Remote IP of the connector.
    pub ip: String,
    /// Client implementation name/version, if reported.
    pub client: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connected {
    pub agent: AgentProfile,
    /// The agent did not exist before and was created by this call.
    pub created: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WhoAmI {
    pub agent: AgentProfile,
    pub session_id: String,
    pub ip: String,
    pub transport: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoJoin {
    pub repo: RepoProfile,
    pub created: bool,
    pub working_repos: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoSummary {
    #[serde(flatten)]
    pub profile: RepoProfile,
    pub members: Vec<String>,
    pub memory_count: usize,
    /// Whether the calling agent has joined this repo (and may write to it).
    pub joined: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AddMemory {
    pub scope: Scope,
    /// Required for `Scope::Repo`; ignored for `Scope::Agent` (the caller's
    /// own space is always used).
    pub repo: Option<String>,
    pub title: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: String,
    pub slug: Option<String>,
}

/// One agent connection. Identity is established with [`connect`]; every
/// memory operation is permission-checked and audited.
///
/// Dropping a session without [`close`] still logs a disconnect.
///
/// [`connect`]: AgentSession::connect
/// [`close`]: AgentSession::close
pub struct AgentSession {
    hub: Arc<Hub>,
    id: String,
    conn: ConnInfo,
    agent: Option<String>,
    opened: Instant,
    started_at: DateTime<Utc>,
    closed: bool,
}

impl AgentSession {
    pub(crate) fn open(hub: Arc<Hub>, conn: ConnInfo) -> Self {
        let session = Self {
            hub,
            id: uuid::Uuid::new_v4().simple().to_string(),
            conn,
            agent: None,
            opened: Instant::now(),
            started_at: Utc::now(),
            closed: false,
        };
        session.hub.audit().record(AuditEvent::Connect {
            actor: session.actor(),
            client: session.conn.client.clone(),
        });
        session
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }

    pub fn conn(&self) -> &ConnInfo {
        &self.conn
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    pub fn actor(&self) -> Actor {
        Actor {
            session_id: self.id.clone(),
            agent: self.agent.clone(),
            ip: self.conn.ip.clone(),
            transport: self.conn.transport.clone(),
        }
    }

    /// Records the disconnect. `reason` examples: `client_closed`,
    /// `idle_timeout`, `stdio_eof`, `server_shutdown`.
    pub fn close(mut self, reason: &str) {
        self.log_disconnect(reason);
    }

    fn log_disconnect(&mut self, reason: &str) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.hub.audit().record(AuditEvent::Disconnect {
            actor: self.actor(),
            reason: reason.to_string(),
            duration_secs: self.opened.elapsed().as_secs_f64(),
        });
    }

    // ---- identity -------------------------------------------------------

    /// Identifies this session by agent name. The name is the identity: any
    /// user running the same agent shares its memory space. An unknown name
    /// is created when `role` is given; for an existing agent `role` and
    /// `description` are ignored.
    pub fn connect(
        &mut self,
        name: &str,
        role: Option<&str>,
        description: Option<&str>,
    ) -> Result<Connected> {
        let name = name.trim();
        if let Some(current) = &self.agent {
            if current != name {
                return Err(AppError::AlreadyIdentified(current.clone()));
            }
            return Ok(Connected {
                agent: self.profile()?,
                created: false,
            });
        }
        validate_name("agent name", name)?;

        let mut created = false;
        if self.hub.registry().agent(name)?.is_none() {
            let role = role
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .ok_or_else(|| AppError::UnknownAgent(name.to_string()))?;
            validate_label("role", role)?;
            let now = Utc::now();
            // `false` means a concurrent connection created it first; join that one.
            created = self.hub.registry().create_agent(&AgentProfile {
                name: name.to_string(),
                role: role.to_string(),
                description: description.unwrap_or_default().trim().to_string(),
                repos: Vec::new(),
                created_at: now,
                last_seen_at: Some(now),
            })?;
        }

        self.agent = Some(name.to_string());
        if created {
            self.hub.audit().record(AuditEvent::AgentRegistered {
                actor: self.actor(),
                role: role.unwrap_or_default().trim().to_string(),
            });
        } else {
            self.hub.touch_agent(name)?;
            self.hub.audit().record(AuditEvent::AgentLogin {
                actor: self.actor(),
            });
        }
        Ok(Connected {
            agent: self.profile()?,
            created,
        })
    }

    pub fn whoami(&self) -> Result<WhoAmI> {
        Ok(WhoAmI {
            agent: self.profile()?,
            session_id: self.id.clone(),
            ip: self.conn.ip.clone(),
            transport: self.conn.transport.clone(),
        })
    }

    pub fn list_agents(&self) -> Result<Vec<AgentProfile>> {
        self.require_agent()?;
        self.hub.registry().agents()
    }

    fn require_agent(&self) -> Result<&str> {
        self.agent.as_deref().ok_or(AppError::NotIdentified)
    }

    fn profile(&self) -> Result<AgentProfile> {
        let name = self.require_agent()?;
        self.hub
            .registry()
            .agent(name)?
            .ok_or_else(|| AppError::UnknownAgent(name.to_string()))
    }

    // ---- repos ----------------------------------------------------------

    /// Adds a repo to the caller's working set, registering the repo first
    /// if nobody has yet.
    pub fn add_repo(
        &mut self,
        repo: &str,
        description: Option<&str>,
        location: Option<&str>,
    ) -> Result<RepoJoin> {
        let agent = self.require_agent()?.to_string();
        let repo = repo.trim();
        validate_name("repo id", repo)?;
        let registry = self.hub.registry();
        let _guard = registry.lock();

        let candidate = RepoProfile {
            id: repo.to_string(),
            description: description.unwrap_or_default().trim().to_string(),
            location: location.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()),
            created_by: agent.clone(),
            created_at: Utc::now(),
        };
        let created = registry.create_repo(&candidate)?;
        let mut profile = registry
            .repo(repo)?
            .ok_or_else(|| AppError::Internal(format!("repo '{repo}' vanished")))?;
        if !created {
            // Fill in details that the original registrant left blank.
            let mut dirty = false;
            if profile.description.is_empty() && !candidate.description.is_empty() {
                profile.description = candidate.description;
                dirty = true;
            }
            if profile.location.is_none() && candidate.location.is_some() {
                profile.location = candidate.location;
                dirty = true;
            }
            if dirty {
                registry.save_repo(&profile)?;
            }
        }

        let mut me = self.profile()?;
        if !me.works_on(repo) {
            me.repos.push(repo.to_string());
            registry.save_agent(&me)?;
        }
        self.hub.audit().record(AuditEvent::RepoJoined {
            actor: self.actor(),
            repo: repo.to_string(),
            created,
        });
        Ok(RepoJoin {
            repo: profile,
            created,
            working_repos: me.repos,
        })
    }

    /// Removes a repo from the caller's working set. Repo memories stay.
    pub fn leave_repo(&mut self, repo: &str) -> Result<Vec<String>> {
        self.require_agent()?;
        let registry = self.hub.registry();
        let _guard = registry.lock();
        let mut me = self.profile()?;
        if !me.works_on(repo) {
            return Err(AppError::Invalid(format!(
                "repo '{repo}' is not in your working repos"
            )));
        }
        me.repos.retain(|r| r != repo);
        registry.save_agent(&me)?;
        self.hub.audit().record(AuditEvent::RepoLeft {
            actor: self.actor(),
            repo: repo.to_string(),
        });
        Ok(me.repos)
    }

    pub fn list_repos(&self) -> Result<Vec<RepoSummary>> {
        let me = self.profile()?;
        let agents = self.hub.registry().agents()?;
        Ok(self
            .hub
            .registry()
            .repos()?
            .into_iter()
            .map(|profile| RepoSummary {
                members: agents
                    .iter()
                    .filter(|a| a.works_on(&profile.id))
                    .map(|a| a.name.clone())
                    .collect(),
                memory_count: self.hub.memory_count(Scope::Repo, &profile.id),
                joined: me.works_on(&profile.id),
                profile,
            })
            .collect())
    }

    // ---- memory: reads ----------------------------------------------------

    pub fn get_memory(&self, id: &MemoryId) -> Result<Memory> {
        self.require_agent()?;
        Ok(self.hub.store().get(id)?)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        self.require_agent()?;
        Ok(self.hub.store().search(query)?)
    }

    pub fn list(&self, filter: &Filter) -> Result<Vec<MemoryMeta>> {
        self.require_agent()?;
        Ok(self
            .hub
            .store()
            .list(filter)?
            .into_iter()
            .map(|m| m.meta)
            .collect())
    }

    pub fn group_by(
        &self,
        filter: &Filter,
        key: GroupKey,
    ) -> Result<BTreeMap<String, Vec<MemoryMeta>>> {
        self.require_agent()?;
        Ok(self.hub.store().group_by(filter, key)?)
    }

    // ---- memory: writes ---------------------------------------------------

    pub fn add_memory(&self, input: AddMemory) -> Result<Memory> {
        let me = self.require_agent()?.to_string();
        let owner = match input.scope {
            Scope::Agent => me.clone(),
            Scope::Repo => input
                .repo
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .ok_or_else(|| AppError::Invalid("'repo' is required for repo-scoped memory".into()))?
                .to_string(),
        };
        self.check_write(input.scope, &owner)?;

        let memory = self.hub.store().add(NewMemory {
            scope: input.scope,
            owner,
            title: input.title,
            kind: input.kind,
            tags: input.tags,
            groups: input.groups,
            body: input.body,
            author: me,
            slug: input.slug,
        })?;
        self.audit_change(
            MemoryAction::Add,
            &memory,
            None,
            Some(memory.meta.version),
            Vec::new(),
            None,
            unified_diff("", &memory.body),
        );
        Ok(memory)
    }

    pub fn update_memory(&self, id: &MemoryId, patch: MemoryPatch) -> Result<Change> {
        let me = self.require_agent()?.to_string();
        self.check_write(id.scope, &id.owner)?;
        let change = self.hub.store().update(id, patch, &me)?;
        if !change.is_noop() {
            self.audit_change(
                MemoryAction::Update,
                &change.after,
                Some(change.before.meta.version),
                Some(change.after.meta.version),
                change.changed_fields.iter().map(|f| f.to_string()).collect(),
                None,
                unified_diff(&change.before.body, &change.after.body),
            );
        }
        Ok(change)
    }

    pub fn delete_memory(&self, id: &MemoryId, expected_version: Option<u64>) -> Result<Memory> {
        self.require_agent()?;
        self.check_write(id.scope, &id.owner)?;
        let memory = self.hub.store().delete(id, expected_version)?;
        self.audit_change(
            MemoryAction::Delete,
            &memory,
            Some(memory.meta.version),
            None,
            Vec::new(),
            None,
            unified_diff(&memory.body, ""),
        );
        Ok(memory)
    }

    /// Adds memories to (or removes them from) a named group. All memories
    /// must be writable by the caller. Returns the ids that changed.
    pub fn group(&self, ids: &[MemoryId], group: &str, add: bool) -> Result<Vec<MemoryId>> {
        let me = self.require_agent()?.to_string();
        if ids.is_empty() {
            return Err(AppError::Invalid("'ids' must not be empty".into()));
        }
        for id in ids {
            self.check_write(id.scope, &id.owner)?;
        }
        let changes = self.hub.store().set_group(ids, group, add, &me)?;
        let action = if add {
            MemoryAction::GroupAdd
        } else {
            MemoryAction::GroupRemove
        };
        for change in &changes {
            self.audit_change(
                action,
                &change.after,
                Some(change.before.meta.version),
                Some(change.after.meta.version),
                vec!["groups".into()],
                Some(group.trim().to_string()),
                None,
            );
        }
        Ok(changes.into_iter().map(|c| c.after.meta.id).collect())
    }

    /// Agent memory: only its owner may write. Repo memory: any agent that
    /// has joined the repo.
    fn check_write(&self, scope: Scope, owner: &str) -> Result<()> {
        let me = self.profile()?;
        match scope {
            Scope::Agent if owner == me.name => Ok(()),
            Scope::Agent => Err(AppError::Forbidden(format!(
                "memory of agent '{owner}' is read-only for other agents"
            ))),
            Scope::Repo => {
                validate_name("repo id", owner)?;
                if self.hub.registry().repo(owner)?.is_none() {
                    return Err(AppError::UnknownRepo(owner.to_string()));
                }
                if me.works_on(owner) {
                    Ok(())
                } else {
                    Err(AppError::Forbidden(format!(
                        "repo '{owner}' is not in your working repos; call add_repo first"
                    )))
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn audit_change(
        &self,
        action: MemoryAction,
        memory: &Memory,
        version_before: Option<u64>,
        version_after: Option<u64>,
        changed_fields: Vec<String>,
        group: Option<String>,
        diff: Option<String>,
    ) {
        self.hub.audit().record(AuditEvent::MemoryChanged {
            actor: self.actor(),
            action,
            memory_id: memory.meta.id.to_string(),
            title: memory.meta.title.clone(),
            version_before,
            version_after,
            changed_fields,
            group,
            diff,
        });
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.log_disconnect("dropped");
    }
}
