use std::{collections::BTreeMap, sync::Arc, time::Instant};

use chrono::{DateTime, Utc};
use memhub_audit::{Actor, AuditEvent, MemoryAction};
use memhub_store::{
    is_under, normalize_folder, validate_label, validate_name, Change, Evidence, EvidenceHit,
    EvidenceId, EvidenceQuery, ExperienceMeta, Filter, FolderInfo, GroupKey, Memory, MemoryId,
    MemoryMeta, MemoryPatch, NewEvidence, NewMemory, Outcome, Scope, SearchHit, SearchQuery,
    Source, Status, StoreError, WORK_FOLDER,
};
use serde::Serialize;

use crate::{
    admin::is_internal_agent,
    diff::unified_diff,
    error::{AppError, Result},
    feedback::{Feedback, Rating},
    hub::Hub,
    registry::{AgentProfile, RepoProfile},
};

/// Longest evidence search window, in days.
pub const MAX_EVIDENCE_WINDOW_DAYS: i64 = 366;
/// Evidence search window when none is given, in days.
pub const DEFAULT_EVIDENCE_WINDOW_DAYS: i64 = 30;

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
    /// own space is used, except that a curator may name another agent in
    /// `owner`).
    pub repo: Option<String>,
    /// Agent space to write for `Scope::Agent` (curators only).
    pub owner: Option<String>,
    /// Folder below the repo / agent space, e.g. `modules/auth`. Empty for
    /// the root folder; missing folders are created.
    pub folder: String,
    pub title: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: String,
    pub slug: Option<String>,
    /// Experience properties; curators only (ignored for work notes).
    pub experience: ExperienceMeta,
}

/// Input for [`AgentSession::record_evidence`].
#[derive(Debug, Clone, Default)]
pub struct RecordEvidence {
    pub title: String,
    pub body: String,
    pub kind: Option<String>,
    pub repos: Vec<String>,
    pub task: Option<String>,
    pub outcome: Outcome,
    /// Defaults to `from_memory` when `derived_from` is given, else `observed`.
    pub source: Option<Source>,
    pub derived_from: Vec<String>,
    pub code_refs: Vec<String>,
    pub reason: Option<String>,
    pub importance: Option<u8>,
    pub candidate_lesson: Option<String>,
    pub supersedes: Option<String>,
    pub tags: Vec<String>,
}

/// Input for [`AgentSession::search_evidence`].
#[derive(Debug, Clone, Default)]
pub struct EvidenceSearch {
    pub text: Option<String>,
    pub repo: Option<String>,
    pub author: Option<String>,
    pub kind: Option<String>,
    /// Defaults to [`DEFAULT_EVIDENCE_WINDOW_DAYS`] days before `until`.
    pub since: Option<DateTime<Utc>>,
    /// Defaults to now.
    pub until: Option<DateTime<Utc>>,
    pub include_legacy: bool,
    pub limit: Option<usize>,
}

/// Options of [`AgentSession::search_experience`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions {
    /// Include memories replaced by newer ones.
    pub include_superseded: bool,
}

/// An experience search hit with the feedback agents gave on it.
#[derive(Debug, Clone, Serialize)]
pub struct ExperienceHit {
    #[serde(flatten)]
    pub hit: SearchHit,
    pub feedback: Feedback,
}

/// A memory or evidence record fetched by id, see [`AgentSession::fetch`].
#[derive(Debug, Clone)]
pub enum Fetched {
    Memory {
        memory: Memory,
        /// The requested id, when it was redirected to `memory`.
        redirected_from: Option<String>,
    },
    Evidence {
        evidence: Evidence,
        redirected_from: Option<String>,
    },
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
    /// Presented the curator token: may write experience memories.
    curator: bool,
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
            curator: false,
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

    /// Lets this session write curated experience (repo memory and agent
    /// experience). The transport grants it after checking the curator token.
    pub fn grant_curator(&mut self) {
        self.curator = true;
    }

    pub fn is_curator(&self) -> bool {
        self.curator
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

    /// Fetches a memory and counts the read (see [`crate::reads`]).
    pub fn get_memory(&self, id: &MemoryId) -> Result<Memory> {
        let me = self.require_agent()?;
        let memory = self.hub.store().get(id)?;
        if !is_internal_agent(me) && !self.curator {
            self.hub.reads().record(&memory.meta.id, me);
        }
        Ok(memory)
    }

    /// Fetches a memory or evidence record by id, following redirects left
    /// by moves and by the 1.0 port.
    pub fn fetch(&self, id: &str) -> Result<Fetched> {
        self.require_agent()?;
        let id = id.trim().trim_end_matches(".md");
        match self.fetch_direct(id) {
            Err(AppError::Store(StoreError::NotFound(_))) => {
                let Some(target) = self.hub.redirects().resolve(id) else {
                    return self.fetch_direct(id);
                };
                Ok(match self.fetch_direct(&target)? {
                    Fetched::Memory { memory, .. } => Fetched::Memory {
                        memory,
                        redirected_from: Some(id.to_string()),
                    },
                    Fetched::Evidence { evidence, .. } => Fetched::Evidence {
                        evidence,
                        redirected_from: Some(id.to_string()),
                    },
                })
            }
            other => other,
        }
    }

    fn fetch_direct(&self, id: &str) -> Result<Fetched> {
        if EvidenceId::looks_like(id) {
            let evidence = self.hub.evidence().get(&id.parse()?)?;
            return Ok(Fetched::Evidence {
                evidence,
                redirected_from: None,
            });
        }
        Ok(Fetched::Memory {
            memory: self.get_memory(&id.parse()?)?,
            redirected_from: None,
        })
    }

    pub fn get_evidence(&self, id: &EvidenceId) -> Result<Evidence> {
        self.require_agent()?;
        Ok(self.hub.evidence().get(id)?)
    }

    /// Experience search with default options, see [`Self::search_experience`].
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        Ok(self
            .search_experience(query, SearchOptions::default())?
            .into_iter()
            .map(|h| h.hit)
            .collect())
    }

    /// Searches the experience layer: repo memory, plus the caller's own
    /// agent space (work area included). Another agent's experience is
    /// searched only when the filter names that agent; other agents' work
    /// areas and evidence are never searched. Superseded memories are left
    /// out unless asked for. Hits are ranked by keyword relevance weighted by
    /// status and by the feedback agents gave.
    pub fn search_experience(
        &self,
        query: &SearchQuery,
        options: SearchOptions,
    ) -> Result<Vec<ExperienceHit>> {
        let me = self.require_agent()?.to_string();
        let limit = query.limit.unwrap_or(20).max(1);
        let named_agent = match query.filter.scope {
            Some(Scope::Agent) | None => query.filter.owner.clone(),
            Some(Scope::Repo) => None,
        };
        let want_superseded =
            options.include_superseded || query.filter.status == Some(Status::Superseded);
        let hits = self.hub.store().search(&SearchQuery {
            limit: Some(usize::MAX),
            ..query.clone()
        })?;
        let feedback = self.hub.feedback().all();
        let ranked = query.text.as_deref().is_some_and(|t| !t.trim().is_empty());
        let mut out: Vec<ExperienceHit> = hits
            .into_iter()
            .filter(|h| {
                let m = &h.meta;
                let foreign = m.scope() == Scope::Agent && m.owner() != me;
                if foreign && (named_agent.as_deref() != Some(m.owner()) || m.is_work_note()) {
                    return false;
                }
                want_superseded || !m.experience.is_superseded()
            })
            .map(|mut hit| {
                let fb = feedback.get(&hit.meta.id.to_string()).cloned().unwrap_or_default();
                if ranked {
                    let status = hit.meta.status().map_or(1.0, Status::search_weight);
                    hit.score *= status * fb.search_weight();
                }
                ExperienceHit { hit, feedback: fb }
            })
            .collect();
        if ranked {
            out.sort_by(|a, b| b.hit.score.total_cmp(&a.hit.score));
        }
        out.truncate(limit);
        if let Some(text) = query.text.as_deref().filter(|t| !t.trim().is_empty()) {
            self.log_search("experience", text, &query.filter, out.iter().map(|h| h.hit.meta.id.to_string()));
        }
        Ok(out)
    }

    fn log_search(
        &self,
        layer: &str,
        text: &str,
        filter: &Filter,
        ids: impl Iterator<Item = String>,
    ) {
        if self.curator {
            return;
        }
        let ids: Vec<String> = ids.collect();
        self.hub.audit().record(AuditEvent::MemorySearched {
            actor: self.actor(),
            layer: layer.to_string(),
            query: text.trim().chars().take(300).collect(),
            scope: filter.scope.map(|s| s.to_string()),
            owner: filter.owner.clone(),
            hits: ids.len(),
            top: ids.into_iter().take(5).collect(),
        });
    }

    /// Searches evidence records inside a time window (default: the last
    /// [`DEFAULT_EVIDENCE_WINDOW_DAYS`] days, at most
    /// [`MAX_EVIDENCE_WINDOW_DAYS`]).
    pub fn search_evidence(&self, search: &EvidenceSearch) -> Result<Vec<EvidenceHit>> {
        self.require_agent()?;
        let until = search.until.unwrap_or_else(Utc::now);
        let since = search
            .since
            .unwrap_or(until - chrono::Duration::days(DEFAULT_EVIDENCE_WINDOW_DAYS));
        if since > until {
            return Err(AppError::Invalid("'since' must be before 'until'".into()));
        }
        if until - since > chrono::Duration::days(MAX_EVIDENCE_WINDOW_DAYS) {
            return Err(AppError::Invalid(format!(
                "evidence search window is limited to {MAX_EVIDENCE_WINDOW_DAYS} days; narrow since/until"
            )));
        }
        let non_empty = |v: &Option<String>| v.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let text = non_empty(&search.text);
        let hits = self.hub.evidence().search(&EvidenceQuery {
            since: Some(since),
            until: Some(until),
            author: non_empty(&search.author),
            repo: non_empty(&search.repo),
            kind: non_empty(&search.kind),
            text: text.clone(),
            include_legacy: search.include_legacy,
            after: None,
            limit: search.limit,
        })?;
        if let Some(text) = text {
            let filter = Filter {
                owner: non_empty(&search.repo),
                ..Default::default()
            };
            self.log_search("evidence", &text, &filter, hits.iter().map(|h| h.meta.id.to_string()));
        }
        Ok(hits)
    }

    // ---- evidence and feedback -----------------------------------------------

    /// Appends an evidence record: what the caller observed, with enough
    /// context (task, outcome, reason, code refs) for the curator to judge it.
    pub fn record_evidence(&self, input: RecordEvidence) -> Result<Evidence> {
        let me = self.profile()?;
        let mut repos: Vec<String> = Vec::new();
        for repo in input.repos.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
            validate_name("repo id", repo)?;
            if self.hub.registry().repo(repo)?.is_none() {
                return Err(AppError::UnknownRepo(repo.to_string()));
            }
            if !me.works_on(repo) && !self.curator {
                return Err(AppError::Forbidden(format!(
                    "repo '{repo}' is not in your working repos; call add_repo first"
                )));
            }
            if !repos.iter().any(|r| r == repo) {
                repos.push(repo.to_string());
            }
        }
        let mut derived_from = Vec::new();
        for d in input.derived_from.iter().map(|d| d.trim()).filter(|d| !d.is_empty()) {
            let id = self.resolve_reference(d)?;
            if !derived_from.contains(&id) {
                derived_from.push(id);
            }
        }
        if let Some(s) = input.supersedes.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let id: EvidenceId = s.parse()?;
            if !self.hub.evidence().exists(&id) {
                return Err(StoreError::NotFound(id.to_string()).into());
            }
        }
        let source = match input.source {
            Some(Source::Legacy) => {
                return Err(AppError::Invalid("source 'legacy' is reserved for the 1.0 port".into()))
            }
            Some(s) => s,
            None if derived_from.is_empty() => Source::Observed,
            None => Source::FromMemory,
        };
        let evidence = self.hub.evidence().add(NewEvidence {
            author: me.name.clone(),
            title: input.title,
            kind: input.kind,
            body: input.body,
            repos,
            task: input.task,
            outcome: input.outcome,
            source,
            derived_from,
            code_refs: input.code_refs.into_iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect(),
            reason: input.reason,
            importance: input.importance,
            candidate_lesson: input.candidate_lesson,
            supersedes: input.supersedes.filter(|s| !s.trim().is_empty()),
            tags: input.tags,
            ..Default::default()
        })?;
        self.audit_evidence(&evidence);
        Ok(evidence)
    }

    pub(crate) fn audit_evidence(&self, evidence: &Evidence) {
        let m = &evidence.meta;
        self.hub.audit().record(AuditEvent::EvidenceRecorded {
            actor: self.actor(),
            evidence_id: m.id.to_string(),
            title: m.title.clone(),
            kind: m.kind.clone(),
            repos: m.repos.clone(),
            source: serde_json::to_value(m.source)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            importance: m.importance,
        });
    }

    /// The current id of an existing memory, following redirects.
    fn resolve_reference(&self, id: &str) -> Result<String> {
        let parsed: MemoryId = id.parse()?;
        if self.hub.store().exists(&parsed) {
            return Ok(parsed.to_string());
        }
        match self.hub.redirects().resolve(id) {
            Some(target) if target.parse::<MemoryId>().is_ok_and(|t| self.hub.store().exists(&t)) => {
                Ok(target)
            }
            _ => Err(StoreError::NotFound(id.to_string()).into()),
        }
    }

    /// Records whether a memory helped. Ratings steer search ranking and
    /// tell the curator which memories to revise.
    pub fn rate_memory(&self, id: &MemoryId, rating: Rating, note: Option<&str>) -> Result<Feedback> {
        let me = self.require_agent()?.to_string();
        let memory = self.hub.store().get(id)?;
        if memory.meta.is_work_note() {
            return Err(AppError::Invalid("work notes are not rated; rate experience memories".into()));
        }
        let note = note.map(|n| n.trim().chars().take(1000).collect::<String>()).filter(|n| !n.is_empty());
        let feedback = self.hub.feedback().rate(id, &me, rating, note.clone());
        self.hub.audit().record(AuditEvent::MemoryRated {
            actor: self.actor(),
            memory_id: id.to_string(),
            rating: rating.to_string(),
            note,
        });
        Ok(feedback)
    }

    /// Reports that a memory contradicts what the caller observed: records
    /// the observation as evidence and marks the memory `disputed` until the
    /// curator resolves it.
    pub fn dispute_memory(
        &self,
        id: &MemoryId,
        reason: &str,
        details: Option<&str>,
        code_refs: Vec<String>,
    ) -> Result<(Evidence, Memory)> {
        let me = self.profile()?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(AppError::Invalid("'reason' must say what you observed instead".into()));
        }
        let memory = self.hub.store().get(id)?;
        if memory.meta.is_work_note() {
            return Err(AppError::Invalid("work notes cannot be disputed; only experience memories".into()));
        }
        let repos = match id.scope {
            Scope::Repo => vec![id.owner.clone()],
            Scope::Agent => Vec::new(),
        };
        let evidence = self.hub.evidence().add(NewEvidence {
            author: me.name.clone(),
            title: format!("Dispute: {}", memory.meta.title),
            kind: Some("dispute".into()),
            body: details.unwrap_or(reason).to_string(),
            repos,
            source: Source::Observed,
            derived_from: vec![id.to_string()],
            code_refs,
            reason: Some(reason.to_string()),
            importance: Some(4),
            ..Default::default()
        })?;
        self.audit_evidence(&evidence);
        let mut current = memory;
        if !current.meta.experience.is_superseded() {
            let mut experience = current.meta.experience.clone();
            experience.status = Some(Status::Disputed);
            experience.cites.merge(&memhub_store::Cites {
                evidence: vec![evidence.meta.id.to_string()],
                code: Vec::new(),
            });
            let change = self.hub.store().update(
                id,
                MemoryPatch {
                    experience: Some(experience),
                    ..Default::default()
                },
                &me.name,
            )?;
            if !change.is_noop() {
                self.audit_change(
                    MemoryAction::Update,
                    &change.after,
                    Some(change.before.meta.version),
                    Some(change.after.meta.version),
                    change.changed_fields.iter().map(|f| f.to_string()).collect(),
                    None,
                    None,
                    None,
                );
            }
            current = change.after;
        }
        self.hub.feedback().dispute(id, &me.name, reason);
        self.hub.audit().record(AuditEvent::MemoryDisputed {
            actor: self.actor(),
            memory_id: id.to_string(),
            evidence_id: evidence.meta.id.to_string(),
            reason: reason.chars().take(500).collect(),
        });
        Ok((evidence, current))
    }

    /// Deletes an evidence record. Evidence is append-only: only the
    /// self-test agent (for its own records) and curators may do this.
    pub fn delete_evidence(&self, id: &EvidenceId) -> Result<Evidence> {
        let me = self.require_agent()?.to_string();
        let evidence = self.hub.evidence().get(id)?;
        let own_selftest = is_internal_agent(&me) && evidence.meta.author == me;
        if !own_selftest && !self.curator {
            return Err(AppError::Forbidden(
                "evidence is append-only; record a new evidence with 'supersedes' to correct it".into(),
            ));
        }
        Ok(self.hub.evidence().delete(id)?)
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
        let owner = match scope {
            Scope::Agent if self.curator => repo.map(str::trim).filter(|o| !o.is_empty()).unwrap_or(&me).to_string(),
            _ => self.resolve_owner(scope, repo, &me, true)?,
        };
        let folder = normalize_folder(folder)?;
        self.check_write(scope, &owner, &folder)?;
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
        self.check_write(id.scope, &id.owner, &id.folder)?;
        self.check_write(id.scope, &id.owner, &normalize_folder(folder)?)?;
        let new_folders = self.missing_folders(id.scope, &id.owner, folder)?;
        let change = self
            .hub
            .store()
            .move_memory(id, folder, slug, expected_version, &me)?;
        self.hub.reads().rename(id, &change.after.meta.id);
        self.hub.feedback().rename(id, &change.after.meta.id);
        if !change.is_noop() {
            self.hub
                .redirects()
                .set(&id.to_string(), &change.after.meta.id.to_string());
        }
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
        let owner = match input.scope {
            Scope::Agent if self.curator => input.owner.clone().filter(|o| !o.trim().is_empty()).unwrap_or(me.clone()),
            _ => self.resolve_owner(input.scope, input.repo.as_deref().or(input.owner.as_deref()), &me, true)?,
        };
        let folder = normalize_folder(&input.folder)?;
        self.check_write(input.scope, &owner, &folder)?;
        let work_note = input.scope == Scope::Agent && is_under(&folder, WORK_FOLDER);
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
            experience: if work_note || !self.curator {
                ExperienceMeta::default()
            } else {
                input.experience
            },
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

    pub fn update_memory(&self, id: &MemoryId, mut patch: MemoryPatch) -> Result<Change> {
        let me = self.require_agent()?.to_string();
        self.check_write(id.scope, &id.owner, &id.folder)?;
        if !self.curator || id.is_work_note() {
            patch.experience = None;
        }
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
        self.check_write(id.scope, &id.owner, &id.folder)?;
        let memory = self.hub.store().delete(id, expected_version)?;
        self.hub.reads().forget(&memory.meta.id);
        self.hub.feedback().forget(&memory.meta.id);
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
            self.check_write(id.scope, &id.owner, &id.folder)?;
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

    /// Write rules of the layered memory:
    /// - an agent's work area (`agent/<me>/working/...`) is writable by that
    ///   agent only;
    /// - experience (repo memory, and agent memory outside the work area) is
    ///   written by curators only; agents contribute through evidence.
    fn check_write(&self, scope: Scope, owner: &str, folder: &str) -> Result<()> {
        let me = self.profile()?;
        let work = is_under(folder, WORK_FOLDER);
        match scope {
            Scope::Agent if work && owner == me.name => Ok(()),
            Scope::Agent if work => Err(AppError::Forbidden(format!(
                "the work area of agent '{owner}' is writable only by that agent"
            ))),
            Scope::Agent if self.curator => Ok(()),
            Scope::Agent if owner == me.name => Err(AppError::Forbidden(format!(
                "agent/{owner}/{folder} is curated experience, written only by the curator. \
                 Record what you learned with record_evidence (it is distilled into your agent \
                 experience), or keep task notes in your work area: folder \"{WORK_FOLDER}/...\""
            ))),
            Scope::Agent => Err(AppError::Forbidden(format!(
                "memory of agent '{owner}' is read-only for other agents"
            ))),
            Scope::Repo => {
                validate_name("repo id", owner)?;
                if self.hub.registry().repo(owner)?.is_none() {
                    return Err(AppError::UnknownRepo(owner.to_string()));
                }
                if self.curator {
                    Ok(())
                } else {
                    Err(AppError::Forbidden(format!(
                        "repo memory of '{owner}' is curated experience, written only by the \
                         curator. Record what you observed with record_evidence(repos=[\"{owner}\"], \
                         ...); the curator distills evidence into repo memory. To correct a wrong \
                         memory use dispute_memory"
                    )))
                }
            }
        }
    }

    pub(crate) fn actor_name(&self) -> Result<String> {
        Ok(self.require_agent()?.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn audit_change(
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
