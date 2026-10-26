use std::{collections::BTreeMap, sync::Arc, time::Instant};

use chrono::{DateTime, Utc};
use memhub_audit::{Actor, AuditEvent, MemoryAction};
use memhub_store::{
    normalize_folder, validate_label, validate_name, Change, Filter, FolderInfo, GroupKey, Memory,
    MemoryId, MemoryMeta, MemoryPatch, NewMemory, Scope, SearchHit, SearchQuery, StoreError,
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
    /// Self-chosen label of the running agent copy (`X-Memhub-Instance`),
    /// cleaned with [`clean_instance`].
    pub instance: Option<String>,
}

/// Longest instance label kept, in characters.
pub const MAX_INSTANCE_LEN: usize = 128;

/// Normalizes an instance label: control characters are dropped, the label
/// is trimmed and cut to [`MAX_INSTANCE_LEN`] characters; empty means none.
/// A label still holding a `${...}` placeholder means the client did not
/// interpolate its config; every copy would send the same text and collapse
/// into one instance, so it is treated as no label.
pub fn clean_instance(raw: &str) -> Option<String> {
    if raw.contains("${") {
        return None;
    }
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_INSTANCE_LEN)
        .collect();
    let cleaned = cleaned.trim_end().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
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
    /// Folder below the repo / agent space, e.g. `modules/auth`. Empty for
    /// the root folder; missing folders are created.
    pub folder: String,
    pub title: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: String,
    pub slug: Option<String>,
}

/// One owner's memory tree (or a subtree of it), as returned by
/// [`AgentSession::browse`].
#[derive(Debug, Clone, Serialize)]
pub struct Browse {
    pub scope: Scope,
    pub owner: String,
    /// Root of the returned subtree; empty for the whole tree.
    pub folder: String,
    /// Repo description or agent role from the registry.
    pub owner_description: String,
    /// Folders of the subtree, parents before children, starting with
    /// `folder` itself.
    pub folders: Vec<FolderInfo>,
    /// Memories of the subtree, ordered by folder, overviews first, then slug.
    pub memories: Vec<MemoryMeta>,
}

impl Browse {
    pub fn root(&self) -> Option<&FolderInfo> {
        self.folders.iter().find(|f| f.path == self.folder)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderResult {
    pub scope: Scope,
    pub owner: String,
    pub folder: String,
    /// The folder did not exist before.
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
        Self::start(hub, conn, uuid::Uuid::new_v4().simple().to_string(), false)
    }

    /// Reopens an expired session under its old id (see [`Hub::resume_session`]).
    pub(crate) fn resume(hub: Arc<Hub>, conn: ConnInfo, id: String) -> Self {
        Self::start(hub, conn, id, true)
    }

    fn start(hub: Arc<Hub>, conn: ConnInfo, id: String, resumed: bool) -> Self {
        let session = Self {
            hub,
            id,
            conn,
            agent: None,
            opened: Instant::now(),
            started_at: Utc::now(),
            closed: false,
        };
        session.hub.audit().record(AuditEvent::Connect {
            actor: session.actor(),
            client: session.conn.client.clone(),
            instance: session.conn.instance.clone(),
            resumed,
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
            location: location
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
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

    /// The folder tree of a repo or agent space with folder descriptions,
    /// counts and the memories in it. `owner` defaults to the caller's own
    /// agent space for `Scope::Agent`.
    pub fn browse(&self, scope: Scope, owner: Option<&str>, folder: &str) -> Result<Browse> {
        let me = self.require_agent()?.to_string();
        let owner = self.resolve_owner(scope, owner, &me, false)?;
        let folder = normalize_folder(folder)?;
        let registry = self.hub.registry();
        let owner_description = match scope {
            Scope::Repo => registry.repo(&owner)?.map(|r| r.description),
            Scope::Agent => registry.agent(&owner)?.map(|a| a.role),
        };
        let store = self.hub.store();
        let mut folders = store.folders(scope, &owner)?;
        if owner_description.is_none() && folders.is_empty() {
            return Err(match scope {
                Scope::Repo => AppError::UnknownRepo(owner),
                Scope::Agent => AppError::UnknownAgent(owner),
            });
        }
        if folders.is_empty() {
            folders.push(FolderInfo {
                path: String::new(),
                description: None,
                memory_count: 0,
                total_count: 0,
                updated_at: None,
            });
        }
        folders.retain(|f| memhub_store::is_under(&f.path, &folder));
        if folders.is_empty() {
            return Err(StoreError::NotFound(format!("folder {scope}/{owner}/{folder}")).into());
        }
        let mut memories: Vec<MemoryMeta> = store
            .list(&Filter {
                scope: Some(scope),
                owner: Some(owner.clone()),
                folder: Some(folder.clone()),
                ..Default::default()
            })?
            .into_iter()
            .map(|m| m.meta)
            .collect();
        memories.sort_by(|a, b| {
            a.folder()
                .split('/')
                .cmp(b.folder().split('/'))
                .then_with(|| {
                    let not_overview = |m: &MemoryMeta| !m.kind.eq_ignore_ascii_case("overview");
                    not_overview(a).cmp(&not_overview(b))
                })
                .then_with(|| a.id.slug.cmp(&b.id.slug))
        });
        Ok(Browse {
            scope,
            owner,
            folder,
            owner_description: owner_description.unwrap_or_default(),
            folders,
            memories,
        })
    }

    // ---- memory: writes ---------------------------------------------------

    /// Owner of a write or browse target: the caller for agent scope (unless
    /// another agent is named and `own_only` is false), the named repo for
    /// repo scope.
    fn resolve_owner(
        &self,
        scope: Scope,
        owner: Option<&str>,
        me: &str,
        own_only: bool,
    ) -> Result<String> {
        let owner = owner.map(str::trim).filter(|o| !o.is_empty());
        Ok(match scope {
            Scope::Agent if own_only => me.to_string(),
            Scope::Agent => owner.unwrap_or(me).to_string(),
            Scope::Repo => owner
                .ok_or_else(|| {
                    AppError::Invalid("'repo' is required for repo-scoped memory".into())
                })?
                .to_string(),
        })
    }

    /// Creates a folder in a repo or the caller's agent space, or updates
    /// the description of an existing one.
    pub fn create_folder(
        &self,
        scope: Scope,
        repo: Option<&str>,
        folder: &str,
        description: Option<&str>,
    ) -> Result<FolderResult> {
        let me = self.require_agent()?.to_string();
        let owner = self.resolve_owner(scope, repo, &me, true)?;
        self.check_write(scope, &owner)?;
        let folder = normalize_folder(folder)?;
        let store = self.hub.store();
        let before = store.folder_description(scope, &owner, &folder)?;
        let missing = self.missing_folders(scope, &owner, &folder)?;
        let created = store.create_folder(scope, &owner, &folder, description)?;
        let after = store.folder_description(scope, &owner, &folder)?;
        for path in missing {
            self.hub.audit().record(AuditEvent::FolderCreated {
                actor: self.actor(),
                scope: scope.to_string(),
                owner: owner.clone(),
                description: if path == folder { after.clone() } else { None },
                folder: path,
            });
        }
        if !created && before != after {
            self.hub.audit().record(AuditEvent::FolderDescribed {
                actor: self.actor(),
                scope: scope.to_string(),
                owner: owner.clone(),
                folder: folder.clone(),
                description: after.clone().unwrap_or_default(),
            });
        }
        Ok(FolderResult {
            scope,
            owner,
            folder,
            created,
            description: after,
        })
    }

    /// Moves a memory to another folder of the same repo / agent space,
    /// optionally renaming its slug.
    pub fn move_memory(
        &self,
        id: &MemoryId,
        folder: &str,
        slug: Option<&str>,
        expected_version: Option<u64>,
    ) -> Result<Change> {
        let me = self.require_agent()?.to_string();
        self.check_write(id.scope, &id.owner)?;
        let new_folders = self.missing_folders(id.scope, &id.owner, folder)?;
        let change = self
            .hub
            .store()
            .move_memory(id, folder, slug, expected_version, &me)?;
        self.record_new_folders(id.scope, &id.owner, new_folders);
        if !change.is_noop() {
            self.audit_change(
                MemoryAction::Move,
                &change.after,
                Some(change.before.meta.version),
                Some(change.after.meta.version),
                change
                    .changed_fields
                    .iter()
                    .map(|f| f.to_string())
                    .collect(),
                None,
                None,
                Some(change.before.meta.id.to_string()),
            );
        }
        Ok(change)
    }

    pub fn add_memory(&self, input: AddMemory) -> Result<Memory> {
        let me = self.require_agent()?.to_string();
        let owner = self.resolve_owner(input.scope, input.repo.as_deref(), &me, true)?;
        self.check_write(input.scope, &owner)?;
        let folder = normalize_folder(&input.folder)?;
        let new_folders = self.missing_folders(input.scope, &owner, &folder)?;

        let memory = self.hub.store().add(NewMemory {
            scope: input.scope,
            owner: owner.clone(),
            folder,
            title: input.title,
            kind: input.kind,
            tags: input.tags,
            groups: input.groups,
            body: input.body,
            author: me,
            slug: input.slug,
        })?;
        self.record_new_folders(input.scope, &owner, new_folders);
        self.audit_change(
            MemoryAction::Add,
            &memory,
            None,
            Some(memory.meta.version),
            Vec::new(),
            None,
            unified_diff("", &memory.body),
            None,
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
                change
                    .changed_fields
                    .iter()
                    .map(|f| f.to_string())
                    .collect(),
                None,
                unified_diff(&change.before.body, &change.after.body),
                None,
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
            None,
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
                None,
            );
        }
        Ok(changes.into_iter().map(|c| c.after.meta.id).collect())
    }

    /// Subfolders on the way to `folder` (inclusive) that do not exist yet,
    /// parents first.
    pub fn missing_folders(&self, scope: Scope, owner: &str, folder: &str) -> Result<Vec<String>> {
        self.hub.missing_folders(scope, owner, folder)
    }

    fn record_new_folders(&self, scope: Scope, owner: &str, folders: Vec<String>) {
        for folder in folders {
            self.hub.audit().record(AuditEvent::FolderCreated {
                actor: self.actor(),
                scope: scope.to_string(),
                owner: owner.to_string(),
                folder,
                description: None,
            });
        }
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
        previous_id: Option<String>,
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
            previous_id,
        });
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.log_disconnect("dropped");
    }
}
