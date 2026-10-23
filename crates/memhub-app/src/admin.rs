//! Operator access used by the management web UI.
//!
//! Unlike [`AgentSession`](crate::AgentSession), admin operations are not
//! bound to an agent and skip the per-agent write rules: an operator may add
//! or delete memories and whole memory folders of any repo or agent. Every
//! change is still audited, with the agent name [`ADMIN_ACTOR`] and transport
//! `admin`.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use memhub_audit::{Actor, AuditEvent, LogQuery, MemoryAction};
use memhub_store::{
    normalize_folder, parse_memory, render_memory, slugify, Filter, FolderInfo, Memory, MemoryId,
    MemoryMeta, NewMemory, Scope, SearchQuery,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    diff::unified_diff,
    error::{AppError, Result},
    hub::Hub,
};

/// Agent name recorded for admin actions. Not a valid agent name, so it can
/// never collide with a real agent.
pub const ADMIN_ACTOR: &str = "(admin)";

/// Agent name used by `scripts/test-mcp.sh`. The script is an on-demand
/// probe (there is no background watchdog): it connects, exercises every
/// tool, then deletes what it wrote. The management logs leave it out so a
/// demo is not filled with that traffic.
pub const SELFTEST_AGENT: &str = "memhub-selftest";

const CONNECTION_EVENTS: [&str; 4] = ["connect", "disconnect", "agent_login", "agent_registered"];
const UPDATE_EVENTS: [&str; 4] = [
    "memory_changed",
    "folder_created",
    "folder_described",
    "folder_deleted",
];
const ACTIVITY_DAYS: i64 = 14;
const WEEK_DAYS: i64 = 7;
const MONTH_DAYS: i64 = 30;

/// The memory space of one repo or agent: `memories/<scope>/<owner>/`.
#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub scope: Scope,
    pub owner: String,
    /// All memories of the owner, including subfolders.
    pub memory_count: usize,
    pub updated_at: Option<DateTime<Utc>>,
    /// The repo / agent is known to the registry.
    pub registered: bool,
    /// Repo description or agent role, if registered.
    pub description: String,
    /// Memories directly in the owner's root folder.
    pub root_count: usize,
    /// Subfolders, parents before children.
    pub children: Vec<FolderInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryDetail {
    pub memory: Memory,
    /// The file exactly as stored (front matter + body).
    pub markdown: String,
    /// Path relative to the data directory.
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryListItem {
    #[serde(flatten)]
    pub meta: MemoryMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Input for [`Admin::add_memory`]. With `markdown`, the file's front matter
/// supplies any property not given explicitly.
#[derive(Debug, Clone, Default)]
pub struct AdminNewMemory {
    pub scope: Scope,
    pub owner: String,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: Option<String>,
    pub slug: Option<String>,
    pub markdown: Option<String>,
    /// Folder below the owner; empty for the root folder.
    pub folder: String,
}

/// Filter for the connection and update logs.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    /// Exact agent name.
    pub agent: Option<String>,
    /// Substring of the client IP.
    pub ip: Option<String>,
    /// Update log: memory action (`add`, `update`, ...) or folder event.
    pub action: Option<String>,
    /// Update log: substring of the memory id or title.
    pub memory: Option<String>,
    /// Only the last N days (`None` = everything).
    pub days: Option<u32>,
    /// Newest N entries.
    pub limit: usize,
}

/// One MCP connection reconstructed from the audit log.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnectionSession {
    pub session_id: String,
    pub agent: Option<String>,
    pub ip: String,
    pub transport: String,
    pub client: Option<String>,
    /// `X-Memhub-Instance` label sent by the client, if any.
    pub instance: Option<String>,
    pub connected_at: Option<String>,
    pub identified_at: Option<String>,
    /// The agent was registered by this connection.
    pub registered: bool,
    pub disconnected_at: Option<String>,
    pub reason: Option<String>,
    pub duration_secs: Option<f64>,
    /// Memory changes made during the connection.
    pub changes: usize,
}

/// Distinct connections, client IPs, agents and agent instances active in one
/// time window.
///
/// A connection counts when it connected, identified itself, or disconnected
/// inside the window. Unidentified connections and the `(admin)` actor are
/// not counted as agents or instances.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WindowCounts {
    pub connections: usize,
    pub ips: usize,
    pub agents: usize,
    #[serde(flatten)]
    pub instances: InstanceCount,
}

/// Distinct running agent copies, see [`instance_key`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InstanceCount {
    pub instances: usize,
    /// Instances without an `X-Memhub-Instance` label, estimated as one per
    /// agent and IP (so a lower bound).
    pub unlabeled_instances: usize,
}

/// Identity of a running agent copy: the agent name plus its
/// `X-Memhub-Instance` label, or plus its IP when no label was sent.
/// `None` for unidentified connections and the admin actor.
pub fn instance_key(agent: Option<&str>, instance: Option<&str>, ip: &str) -> Option<String> {
    let agent = agent.filter(|a| !a.is_empty() && *a != ADMIN_ACTOR)?;
    Some(match instance.filter(|i| !i.is_empty()) {
        Some(label) => format!("{agent}\u{0}label\u{0}{label}"),
        None => format!("{agent}\u{0}ip\u{0}{ip}"),
    })
}

/// Counts distinct instances among `(agent, instance label, ip)` triples.
pub fn count_instances<'a>(
    connections: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>, &'a str)>,
) -> InstanceCount {
    let mut labeled = HashSet::new();
    let mut unlabeled = HashSet::new();
    for (agent, instance, ip) in connections {
        if let Some(key) = instance_key(agent, instance, ip) {
            if instance.is_some_and(|i| !i.is_empty()) {
                labeled.insert(key);
            } else {
                unlabeled.insert(key);
            }
        }
    }
    InstanceCount {
        instances: labeled.len() + unlabeled.len(),
        unlabeled_instances: unlabeled.len(),
    }
}

/// Rolling connection activity. `week` is the last 7 days and `month` the
/// last 30 days, both ending now (UTC).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ConnectionSummary {
    pub week: WindowCounts,
    pub month: WindowCounts,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DayActivity {
    pub date: NaiveDate,
    pub connections: usize,
    pub changes: usize,
    pub agents: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub memories: usize,
    pub repo_memories: usize,
    pub agent_memories: usize,
    pub repo_folders: usize,
    pub agent_folders: usize,
    pub registered_agents: usize,
    pub registered_repos: usize,
    /// Memory count per kind, largest first.
    pub kinds: Vec<(String, usize)>,
    /// Largest folders (`scope/owner`, count).
    pub top_folders: Vec<(String, usize)>,
    /// Memory changes per agent over the activity window, most active first.
    pub top_agents: Vec<(String, usize)>,
    /// One entry per day, oldest first, ending today (UTC).
    pub activity: Vec<DayActivity>,
    pub connections_today: usize,
    pub changes_today: usize,
    pub active_agents_today: usize,
    pub recent_updates: Vec<Value>,
}

/// Admin handle; one per request so each action carries the caller's IP.
pub struct Admin {
    hub: Arc<Hub>,
    actor: Actor,
}

impl Hub {
    pub fn admin(self: &Arc<Self>, ip: &str) -> Admin {
        Admin {
            hub: self.clone(),
            actor: Actor {
                session_id: format!("admin-{}", uuid::Uuid::new_v4().simple()),
                agent: Some(ADMIN_ACTOR.to_string()),
                ip: ip.to_string(),
                transport: "admin".to_string(),
            },
        }
    }
}

impl Admin {
    // ---- memories -------------------------------------------------------

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let store = self.hub.store();
        let registry = self.hub.registry();
        let repos: HashMap<String, String> = registry
            .repos()?
            .into_iter()
            .map(|r| (r.id, r.description))
            .collect();
        let agents: HashMap<String, String> = registry
            .agents()?
            .into_iter()
            .map(|a| (a.name, a.role))
            .collect();

        let mut out = Vec::new();
        for scope in Scope::ALL {
            let known = match scope {
                Scope::Repo => &repos,
                Scope::Agent => &agents,
            };
            for owner in store.owners(scope)? {
                let mut tree = store.folders(scope, &owner)?;
                let root = if tree.first().is_some_and(|f| f.path.is_empty()) {
                    Some(tree.remove(0))
                } else {
                    None
                };
                out.push(Folder {
                    scope,
                    registered: known.contains_key(&owner),
                    description: known.get(&owner).cloned().unwrap_or_default(),
                    owner,
                    memory_count: root.as_ref().map_or(0, |r| r.total_count),
                    updated_at: root.as_ref().and_then(|r| r.updated_at),
                    root_count: root.as_ref().map_or(0, |r| r.memory_count),
                    children: tree,
                });
            }
        }
        Ok(out)
    }

    /// Memories in a scope/folder, newest first; with `query`, ranked by
    /// keyword relevance instead.
    pub fn list(
        &self,
        scope: Option<Scope>,
        owner: Option<&str>,
        folder: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<MemoryListItem>> {
        let folder = folder.map(normalize_folder).transpose()?;
        let filter = Filter {
            scope,
            owner: owner.map(str::to_string),
            folder: folder.filter(|f| !f.is_empty()),
            ..Default::default()
        };
        let store = self.hub.store();
        match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(text) => Ok(store
                .search(&SearchQuery {
                    text: Some(text.to_string()),
                    filter,
                    limit: Some(usize::MAX),
                })?
                .into_iter()
                .map(|h| MemoryListItem {
                    meta: h.meta,
                    score: Some(h.score),
                    snippet: Some(h.snippet),
                })
                .collect()),
            None => Ok(store
                .list(&filter)?
                .into_iter()
                .map(|m| MemoryListItem {
                    meta: m.meta,
                    score: None,
                    snippet: None,
                })
                .collect()),
        }
    }

    pub fn get(&self, id: &MemoryId) -> Result<MemoryDetail> {
        let memory = self.hub.store().get(id)?;
        Ok(MemoryDetail {
            markdown: render_memory(&memory)?,
            path: format!("memories/{id}.md"),
            memory,
        })
    }

    pub fn add_memory(&self, input: AdminNewMemory) -> Result<Memory> {
        let non_empty =
            |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let title = non_empty(input.title);
        let slug = non_empty(input.slug);
        let body = input.body.filter(|b| !b.trim().is_empty());

        let mut new = NewMemory {
            scope: input.scope,
            owner: input.owner.trim().to_string(),
            folder: normalize_folder(&input.folder)?,
            title: title.clone().unwrap_or_default(),
            kind: non_empty(input.kind),
            tags: input.tags,
            groups: input.groups,
            body: body.clone().unwrap_or_default(),
            author: ADMIN_ACTOR.to_string(),
            slug: slug.clone(),
        };
        if let Some(markdown) = input.markdown.filter(|m| !m.trim().is_empty()) {
            let placeholder = slug
                .clone()
                .or_else(|| title.as_deref().map(slugify))
                .unwrap_or_else(|| "imported".to_string());
            let id = MemoryId::new(input.scope, &new.owner, placeholder)?;
            let parsed = parse_memory(&markdown, id, Path::new("upload.md"))?;
            if title.is_none() {
                new.title = parsed.meta.title;
            }
            if new.kind.is_none() {
                new.kind = Some(parsed.meta.kind);
            }
            if new.tags.is_empty() {
                new.tags = parsed.meta.tags;
            }
            if new.groups.is_empty() {
                new.groups = parsed.meta.groups;
            }
            if body.is_none() {
                new.body = parsed.body;
            }
        }

        let new_folders = self.hub.missing_folders(new.scope, &new.owner, &new.folder)?;
        let memory = self.hub.store().add(new)?;
        for folder in new_folders {
            self.record_folder_created(memory.meta.scope(), memory.meta.owner(), &folder, None);
        }
        self.record_change(
            MemoryAction::Add,
            &memory,
            None,
            Some(memory.meta.version),
            unified_diff("", &memory.body),
        );
        Ok(memory)
    }

    /// Moves a memory to another folder of the same owner, optionally
    /// renaming it.
    pub fn move_memory(&self, id: &MemoryId, folder: &str, slug: Option<&str>) -> Result<Memory> {
        let slug = slug.map(str::trim).filter(|s| !s.is_empty());
        let new_folders = self.hub.missing_folders(id.scope, &id.owner, folder)?;
        let change = self
            .hub
            .store()
            .move_memory(id, folder, slug, None, ADMIN_ACTOR)?;
        for path in new_folders {
            self.record_folder_created(id.scope, &id.owner, &path, None);
        }
        if !change.is_noop() {
            self.hub.audit().record(AuditEvent::MemoryChanged {
                actor: self.actor.clone(),
                action: MemoryAction::Move,
                memory_id: change.after.meta.id.to_string(),
                title: change.after.meta.title.clone(),
                version_before: Some(change.before.meta.version),
                version_after: Some(change.after.meta.version),
                changed_fields: change
                    .changed_fields
                    .iter()
                    .map(|f| f.to_string())
                    .collect(),
                group: None,
                diff: None,
                previous_id: Some(id.to_string()),
            });
        }
        Ok(change.after)
    }

    pub fn delete_memory(&self, id: &MemoryId) -> Result<Memory> {
        let memory = self.hub.store().delete(id, None)?;
        self.record_change(
            MemoryAction::Delete,
            &memory,
            Some(memory.meta.version),
            None,
            unified_diff(&memory.body, ""),
        );
        Ok(memory)
    }

    /// Creates the root folder of a repo / agent (empty `folder`) or a
    /// subfolder. For an existing subfolder only the description is updated
    /// (when given). Returns `false` if the folder already existed.
    pub fn create_folder(
        &self,
        scope: Scope,
        owner: &str,
        folder: &str,
        description: Option<&str>,
    ) -> Result<bool> {
        let owner = owner.trim();
        let folder = normalize_folder(folder)?;
        let store = self.hub.store();
        if folder.is_empty() {
            let created = store.create_owner(scope, owner)?;
            if created {
                self.record_folder_created(scope, owner, "", None);
            }
            return Ok(created);
        }
        let before = store.folder_description(scope, owner, &folder)?;
        let missing = self.hub.missing_folders(scope, owner, &folder)?;
        if !store.owners(scope)?.iter().any(|o| o == owner) {
            self.record_folder_created(scope, owner, "", None);
        }
        let created = store.create_folder(scope, owner, &folder, description)?;
        let after = store.folder_description(scope, owner, &folder)?;
        for path in &missing {
            let description = if *path == folder { after.clone() } else { None };
            self.record_folder_created(scope, owner, path, description);
        }
        if !created && before != after {
            self.hub.audit().record(AuditEvent::FolderDescribed {
                actor: self.actor.clone(),
                scope: scope.to_string(),
                owner: owner.to_string(),
                folder: folder.clone(),
                description: after.unwrap_or_default(),
            });
        }
        Ok(created)
    }

    fn record_folder_created(
        &self,
        scope: Scope,
        owner: &str,
        folder: &str,
        description: Option<String>,
    ) {
        self.hub.audit().record(AuditEvent::FolderCreated {
            actor: self.actor.clone(),
            scope: scope.to_string(),
            owner: owner.to_string(),
            folder: folder.to_string(),
            description,
        });
    }

    /// Moves a folder (the whole repo / agent space when `folder` is empty)
    /// to the trash. Each contained memory is also audited as a deletion
    /// (with its body), so it can be recovered from the log.
    pub fn delete_folder(&self, scope: Scope, owner: &str, folder: &str) -> Result<Vec<MemoryId>> {
        let folder = normalize_folder(folder)?;
        let memories = self.hub.store().delete_folder(scope, owner, &folder)?;
        for memory in &memories {
            self.record_change(
                MemoryAction::Delete,
                memory,
                Some(memory.meta.version),
                None,
                unified_diff(&memory.body, ""),
            );
        }
        self.hub.audit().record(AuditEvent::FolderDeleted {
            actor: self.actor.clone(),
            scope: scope.to_string(),
            owner: owner.to_string(),
            folder,
            memory_count: memories.len(),
        });
        Ok(memories.into_iter().map(|m| m.meta.id).collect())
    }

    fn record_change(
        &self,
        action: MemoryAction,
        memory: &Memory,
        version_before: Option<u64>,
        version_after: Option<u64>,
        diff: Option<String>,
    ) {
        self.hub.audit().record(AuditEvent::MemoryChanged {
            actor: self.actor.clone(),
            action,
            memory_id: memory.meta.id.to_string(),
            title: memory.meta.title.clone(),
            version_before,
            version_after,
            changed_fields: Vec::new(),
            group: None,
            diff,
            previous_id: None,
        });
    }

    // ---- logs -------------------------------------------------------------

    fn read_log(&self, days: Option<u32>) -> Result<Vec<Value>> {
        let since = days.map(|d| Utc::now() - Duration::days(i64::from(d)));
        self.hub
            .audit()
            .read(&LogQuery {
                since,
                ..Default::default()
            })
            .map_err(|e| AppError::Internal(format!("cannot read audit log: {e}")))
    }

    /// Every MCP session in the window, oldest first, before display filters.
    fn load_sessions(&self, days: Option<u32>) -> Result<Vec<ConnectionSession>> {
        let mut sessions: Vec<ConnectionSession> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for record in self.read_log(days)? {
            let event = str_field(&record, "event");
            let Some(sid) = record.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            if event == "memory_changed" {
                if let Some(&i) = index.get(sid) {
                    sessions[i].changes += 1;
                }
                continue;
            }
            if !CONNECTION_EVENTS.contains(&event) {
                continue;
            }
            let i = *index.entry(sid.to_string()).or_insert_with(|| {
                sessions.push(ConnectionSession {
                    session_id: sid.to_string(),
                    ip: str_field(&record, "ip").to_string(),
                    transport: str_field(&record, "transport").to_string(),
                    ..Default::default()
                });
                sessions.len() - 1
            });
            let s = &mut sessions[i];
            let ts = record.get("ts").and_then(Value::as_str).map(str::to_string);
            if let Some(agent) = record.get("agent").and_then(Value::as_str) {
                s.agent = Some(agent.to_string());
            }
            match event {
                "connect" => {
                    s.connected_at = ts;
                    s.client = record
                        .get("client")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    s.instance = record
                        .get("instance")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                "agent_login" | "agent_registered" => {
                    s.identified_at = s.identified_at.take().or(ts);
                    s.registered |= event == "agent_registered";
                }
                "disconnect" => {
                    s.disconnected_at = ts;
                    s.reason = record
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    s.duration_secs = record.get("duration_secs").and_then(Value::as_f64);
                }
                _ => {}
            }
        }
        sessions.retain(|s| s.agent.as_deref().is_none_or(|a| !is_internal_agent(a)));
        Ok(sessions)
    }

    /// Connections reconstructed per MCP session, newest first.
    pub fn connections(&self, filter: &LogFilter) -> Result<Vec<ConnectionSession>> {
        let mut out: Vec<ConnectionSession> = self
            .load_sessions(filter.days)?
            .into_iter()
            .filter(|s| {
                filter
                    .agent
                    .as_ref()
                    .is_none_or(|a| s.agent.as_deref() == Some(a.as_str()))
                    && filter
                        .ip
                        .as_ref()
                        .is_none_or(|ip| s.ip.contains(ip.as_str()))
            })
            .collect();
        out.reverse();
        out.truncate(filter.limit);
        Ok(out)
    }

    /// Live-window counts for the connection page. "Now" is not included:
    /// only the HTTP server knows which sessions are still open.
    pub fn connection_summary(&self) -> Result<ConnectionSummary> {
        let now = Utc::now();
        let sessions = self.load_sessions(Some(MONTH_DAYS as u32))?;
        Ok(ConnectionSummary {
            week: window_counts(&sessions, now - Duration::days(WEEK_DAYS)),
            month: window_counts(&sessions, now - Duration::days(MONTH_DAYS)),
        })
    }

    /// Raw connection events (connect, identify, disconnect), newest first.
    /// `connect` records are written before the agent identifies itself, so
    /// their agent is filled in from the same session's later records.
    pub fn connection_events(&self, filter: &LogFilter) -> Result<Vec<Value>> {
        let records = self.read_log(filter.days)?;
        let agents: HashMap<String, String> = records
            .iter()
            .filter_map(|r| {
                Some((
                    r.get("session_id")?.as_str()?.to_string(),
                    r.get("agent")?.as_str()?.to_string(),
                ))
            })
            .collect();
        let mut out: Vec<Value> = records
            .into_iter()
            .filter(|r| CONNECTION_EVENTS.contains(&str_field(r, "event")))
            .map(|mut r| {
                if r.get("agent").is_none() {
                    if let Some(agent) = agents.get(str_field(&r, "session_id")) {
                        r["agent"] = Value::String(agent.clone());
                    }
                }
                r
            })
            .filter(|r| {
                !is_internal_agent(str_field(r, "agent"))
                    && filter
                        .agent
                        .as_ref()
                        .is_none_or(|a| str_field(r, "agent") == a)
                    && filter
                        .ip
                        .as_ref()
                        .is_none_or(|ip| str_field(r, "ip").contains(ip.as_str()))
            })
            .collect();
        out.reverse();
        out.truncate(filter.limit);
        Ok(out)
    }

    /// Memory and folder changes, newest first.
    pub fn updates(&self, filter: &LogFilter) -> Result<Vec<Value>> {
        let needle = filter.memory.as_ref().map(|m| m.to_lowercase());
        let mut out: Vec<Value> = self
            .read_log(filter.days)?
            .into_iter()
            .filter(|r| {
                let event = str_field(r, "event");
                if !UPDATE_EVENTS.contains(&event) || is_internal_agent(str_field(r, "agent")) {
                    return false;
                }
                let action = if event == "memory_changed" {
                    str_field(r, "action")
                } else {
                    event
                };
                let target = if event == "memory_changed" {
                    format!(
                        "{} {} {}",
                        str_field(r, "memory_id"),
                        str_field(r, "title"),
                        str_field(r, "previous_id")
                    )
                } else {
                    format!(
                        "{}/{}/{}",
                        str_field(r, "scope"),
                        str_field(r, "owner"),
                        str_field(r, "folder")
                    )
                };
                filter
                    .agent
                    .as_ref()
                    .is_none_or(|a| str_field(r, "agent") == a)
                    && filter.action.as_ref().is_none_or(|a| action == a)
                    && needle
                        .as_ref()
                        .is_none_or(|n| target.to_lowercase().contains(n.as_str()))
            })
            .collect();
        out.reverse();
        out.truncate(filter.limit);
        Ok(out)
    }

    // ---- statistics ---------------------------------------------------------

    pub fn stats(&self) -> Result<Stats> {
        let mut stats = Stats::default();
        let registry = self.hub.registry();
        stats.registered_agents = registry.agents()?.len();
        stats.registered_repos = registry.repos()?.len();

        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        let mut folders: BTreeMap<String, usize> = BTreeMap::new();
        for memory in self.hub.store().list(&Filter::default())? {
            let meta = &memory.meta;
            stats.memories += 1;
            match meta.scope() {
                Scope::Repo => stats.repo_memories += 1,
                Scope::Agent => stats.agent_memories += 1,
            }
            *kinds.entry(meta.kind.clone()).or_default() += 1;
            *folders
                .entry(format!("{}/{}", meta.scope(), meta.owner()))
                .or_default() += 1;
        }
        stats.repo_folders = self.hub.store().owners(Scope::Repo)?.len();
        stats.agent_folders = self.hub.store().owners(Scope::Agent)?.len();
        stats.kinds = sorted_desc(kinds, usize::MAX);
        stats.top_folders = sorted_desc(folders, 8);

        let today = Utc::now().date_naive();
        let first = today - Duration::days(ACTIVITY_DAYS - 1);
        let mut days: BTreeMap<NaiveDate, (DayActivity, Vec<String>)> = BTreeMap::new();
        let mut date = first;
        while date <= today {
            days.insert(
                date,
                (
                    DayActivity {
                        date,
                        ..Default::default()
                    },
                    Vec::new(),
                ),
            );
            date += Duration::days(1);
        }
        let mut agent_changes: BTreeMap<String, usize> = BTreeMap::new();
        let mut recent = Vec::new();
        let records = self.read_log(Some(ACTIVITY_DAYS as u32))?;
        // `connect` is written before the agent identifies itself, so the name
        // is taken from a later record of the same session.
        let session_agents: HashMap<String, String> = records
            .iter()
            .filter_map(|r| {
                Some((
                    r.get("session_id")?.as_str()?.to_string(),
                    r.get("agent")?.as_str()?.to_string(),
                ))
            })
            .collect();
        for record in records {
            let Some(day) = record
                .get("ts")
                .and_then(Value::as_str)
                .and_then(|t| t.parse::<DateTime<Utc>>().ok())
                .map(|t| t.date_naive())
            else {
                continue;
            };
            let Some((activity, agents)) = days.get_mut(&day) else {
                continue;
            };
            let event = str_field(&record, "event");
            let named = record
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    session_agents
                        .get(str_field(&record, "session_id"))
                        .cloned()
                });
            if named.as_deref().is_some_and(is_internal_agent) {
                continue;
            }
            let agent = named.as_deref();
            match event {
                "connect" => activity.connections += 1,
                "memory_changed" => {
                    activity.changes += 1;
                    if let Some(agent) = agent {
                        *agent_changes.entry(agent.to_string()).or_default() += 1;
                    }
                }
                _ => {}
            }
            if let Some(agent) = agent.filter(|a| *a != ADMIN_ACTOR) {
                if !agents.iter().any(|a| a == agent) {
                    agents.push(agent.to_string());
                }
            }
            if UPDATE_EVENTS.contains(&event) {
                recent.push(record);
            }
        }
        stats.activity = days
            .into_values()
            .map(|(mut a, agents)| {
                a.agents = agents.len();
                a
            })
            .collect();
        if let Some(t) = stats.activity.last() {
            stats.connections_today = t.connections;
            stats.changes_today = t.changes;
            stats.active_agents_today = t.agents;
        }
        stats.top_agents = sorted_desc(agent_changes, 8);
        recent.reverse();
        recent.truncate(8);
        for r in &mut recent {
            if let Some(obj) = r.as_object_mut() {
                obj.remove("diff");
            }
        }
        stats.recent_updates = recent;
        Ok(stats)
    }
}

fn str_field<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn is_internal_agent(name: &str) -> bool {
    name == SELFTEST_AGENT
}

/// True when the session connected, identified, or disconnected at or after `start`.
fn session_active_since(session: &ConnectionSession, start: DateTime<Utc>) -> bool {
    [
        &session.connected_at,
        &session.identified_at,
        &session.disconnected_at,
    ]
    .into_iter()
    .flatten()
    .any(|ts| ts.parse::<DateTime<Utc>>().is_ok_and(|t| t >= start))
}

fn window_counts(sessions: &[ConnectionSession], start: DateTime<Utc>) -> WindowCounts {
    let mut ips = HashSet::new();
    let mut agents = HashSet::new();
    let mut connections = 0;
    let active: Vec<&ConnectionSession> = sessions
        .iter()
        .filter(|s| session_active_since(s, start))
        .collect();
    let instances = count_instances(
        active
            .iter()
            .map(|s| (s.agent.as_deref(), s.instance.as_deref(), s.ip.as_str())),
    );
    for session in active {
        connections += 1;
        if !session.ip.is_empty() {
            ips.insert(session.ip.clone());
        }
        if let Some(agent) = session
            .agent
            .as_deref()
            .filter(|a| !a.is_empty() && *a != ADMIN_ACTOR)
        {
            agents.insert(agent.to_string());
        }
    }
    WindowCounts {
        connections,
        ips: ips.len(),
        agents: agents.len(),
        instances,
    }
}

fn sorted_desc(map: BTreeMap<String, usize>, limit: usize) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}
