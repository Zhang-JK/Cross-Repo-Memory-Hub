//! MCP tool catalogue and dispatch onto [`AgentSession`].
//!
//! Every session sees the agent tools. Sessions holding the curator token
//! also see the curation and import tools.

use chrono::{DateTime, NaiveDate, Utc};
use memhub_app::{
    curation::{CurationOp, ProposalState, Target, Verdict},
    import::v1_memory_from_json,
    store::{
        self, EvidenceHit, EvidenceId, EvidenceMeta, Filter, GroupKey, MemoryId, MemoryMeta,
        MemoryPatch, Outcome, Scope, SearchQuery, Source, Status, WORK_FOLDER,
    },
    AddMemory, AgentProfile, AgentSession, AppError, Browse, EvidenceSearch, ExperienceHit,
    Fetched, Rating, RecordEvidence, RepoProfile, SearchOptions,
};
use schemars::{generate::SchemaSettings, JsonSchema};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Map, Value};

// ---- argument types ------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ScopeArg {
    Repo,
    Agent,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Repo => Scope::Repo,
            ScopeArg::Agent => Scope::Agent,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StatusArg {
    Provisional,
    Established,
    Disputed,
    Superseded,
}

impl From<StatusArg> for Status {
    fn from(s: StatusArg) -> Self {
        match s {
            StatusArg::Provisional => Status::Provisional,
            StatusArg::Established => Status::Established,
            StatusArg::Disputed => Status::Disputed,
            StatusArg::Superseded => Status::Superseded,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum GroupKeyArg {
    Group,
    Kind,
    Tag,
    Owner,
    Scope,
    Author,
    Folder,
}

impl From<GroupKeyArg> for GroupKey {
    fn from(k: GroupKeyArg) -> Self {
        match k {
            GroupKeyArg::Group => GroupKey::Group,
            GroupKeyArg::Kind => GroupKey::Kind,
            GroupKeyArg::Tag => GroupKey::Tag,
            GroupKeyArg::Owner => GroupKey::Owner,
            GroupKeyArg::Scope => GroupKey::Scope,
            GroupKeyArg::Author => GroupKey::Author,
            GroupKeyArg::Folder => GroupKey::Folder,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum GroupAction {
    #[default]
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OutcomeArg {
    Success,
    Failure,
    Partial,
    Unknown,
}

impl From<OutcomeArg> for Outcome {
    fn from(o: OutcomeArg) -> Self {
        match o {
            OutcomeArg::Success => Outcome::Success,
            OutcomeArg::Failure => Outcome::Failure,
            OutcomeArg::Partial => Outcome::Partial,
            OutcomeArg::Unknown => Outcome::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SourceArg {
    Observed,
    FromMemory,
}

impl From<SourceArg> for Source {
    fn from(s: SourceArg) -> Self {
        match s {
            SourceArg::Observed => Source::Observed,
            SourceArg::FromMemory => Source::FromMemory,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum RatingArg {
    Helpful,
    Harmful,
    Outdated,
}

impl From<RatingArg> for Rating {
    fn from(r: RatingArg) -> Self {
        match r {
            RatingArg::Helpful => Rating::Helpful,
            RatingArg::Harmful => Rating::Harmful,
            RatingArg::Outdated => Rating::Outdated,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ProposalStateArg {
    Pending,
    Applied,
    Rejected,
    Partial,
}

impl From<ProposalStateArg> for ProposalState {
    fn from(s: ProposalStateArg) -> Self {
        match s {
            ProposalStateArg::Pending => ProposalState::Pending,
            ProposalStateArg::Applied => ProposalState::Applied,
            ProposalStateArg::Rejected => ProposalState::Rejected,
            ProposalStateArg::Partial => ProposalState::Partial,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct ConnectArgs {
    /// Agent name, e.g. "crash-analyzer": ASCII letters, digits, '-', '_' or '.'.
    /// Every user running the same agent should use the same name to share its memory.
    name: String,
    /// The agent's task specialty, e.g. "feature-impl", "crash-analysis", "perf-opt".
    /// Required only when the agent does not exist yet.
    #[serde(default)]
    role: Option<String>,
    /// What this agent is responsible for (used when the agent is created).
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddRepoArgs {
    /// Repository id: ASCII letters, digits, '-', '_' or '.', e.g. "payment-service".
    repo: String,
    /// One-line description of the repository (used when the repo is new).
    #[serde(default)]
    description: Option<String>,
    /// Local path or remote URL of the repository.
    #[serde(default)]
    location: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RepoArgs {
    /// Repository id.
    repo: String,
}

#[derive(Deserialize, JsonSchema)]
struct FilterArgs {
    /// Restrict to repo memory or agent memory.
    #[serde(default)]
    scope: Option<ScopeArg>,
    /// Repo id (scope=repo) or agent name (scope=agent). Naming another agent is the only way
    /// to include that agent's experience.
    #[serde(default)]
    owner: Option<String>,
    /// Only memories in this folder or its subfolders, e.g. "modules/auth".
    /// Usually combined with scope and owner.
    #[serde(default)]
    folder: Option<String>,
    /// Memory kind, e.g. overview, architecture, pitfall, decision, how-to.
    #[serde(default)]
    kind: Option<String>,
    /// Only memories carrying all of these tags.
    #[serde(default)]
    tags: Vec<String>,
    /// Only memories in this group.
    #[serde(default)]
    group: Option<String>,
    /// Only memories created or last updated by this agent.
    #[serde(default)]
    author: Option<String>,
    /// Only memories with this status.
    #[serde(default)]
    status: Option<StatusArg>,
}

impl TryFrom<FilterArgs> for Filter {
    type Error = ToolError;

    fn try_from(f: FilterArgs) -> Result<Self, ToolError> {
        let folder = f
            .folder
            .as_deref()
            .map(store::normalize_folder)
            .transpose()?
            .filter(|f| !f.is_empty());
        Ok(Filter {
            scope: f.scope.map(Into::into),
            owner: f.owner.filter(|o| !o.trim().is_empty()),
            folder,
            kind: f.kind.filter(|k| !k.trim().is_empty()),
            tags: f.tags,
            group: f.group.filter(|g| !g.trim().is_empty()),
            author: f.author.filter(|a| !a.trim().is_empty()),
            status: f.status.map(Into::into),
        })
    }
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Keywords; every term is matched case-insensitively against title, folder, tags, kind,
    /// applies_to and body. Omit to list the most recently updated memories matching the filters.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: FilterArgs,
    /// Also return memories that were superseded or retired (default false).
    #[serde(default)]
    include_superseded: bool,
    /// Maximum number of hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ListArgs {
    #[serde(flatten)]
    filter: FilterArgs,
    /// Bucket the result by this property instead of returning a flat list.
    #[serde(default)]
    group_by: Option<GroupKeyArg>,
}

#[derive(Deserialize, JsonSchema)]
struct GetArgs {
    /// A memory id ("repo/payment-service/pitfalls/retry-policy") or an evidence id
    /// ("evidence/2026-11-03/crash-bot/091512-refund-dup"). Old 1.0 ids and ids of moved
    /// memories are redirected.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct BrowseArgs {
    /// "repo" (default) or "agent".
    #[serde(default)]
    scope: Option<ScopeArg>,
    /// Repo id (scope=repo, required) or agent name (scope=agent, defaults to yourself).
    #[serde(default)]
    owner: Option<String>,
    /// Show only this folder and its subfolders, e.g. "modules".
    #[serde(default)]
    folder: Option<String>,
    /// List memory titles under each folder (default true). Set false for a folders-only outline.
    #[serde(default)]
    memories: Option<bool>,
    /// Also show superseded (replaced or retired) memories; hidden by default.
    #[serde(default)]
    include_superseded: bool,
}

#[derive(Deserialize, JsonSchema)]
struct RecordEvidenceArgs {
    /// One-line summary of what happened, ASCII (English), e.g. "Refund webhook delivered twice".
    title: String,
    /// What you observed, in enough detail for someone without your context: the symptom,
    /// what you tried, what worked or failed, error messages, commands. Markdown, any language.
    body: String,
    /// Repos this is about (ids you joined with add_repo). Leave empty for lessons about your
    /// own way of working that are not tied to a repo.
    #[serde(default)]
    repos: Vec<String>,
    /// observation (default), success, failure, decision, workaround, question, ...
    #[serde(default)]
    kind: Option<String>,
    /// The task you were working on, in one line, e.g. "PAY-123 fix refund rounding". The
    /// curator counts one source per agent and task, so keep it stable within a task.
    #[serde(default)]
    task: Option<String>,
    /// How the attempt ended (default unknown).
    #[serde(default)]
    outcome: Option<OutcomeArg>,
    /// observed: you saw it yourself (ran it, read the code, hit the bug). from_memory: you are
    /// repeating what a memory said without checking it. Default: from_memory when derived_from
    /// is given, otherwise observed.
    #[serde(default)]
    source: Option<SourceArg>,
    /// Memory ids you relied on for this, e.g. ["repo/shop/pitfalls/refund-retries"].
    #[serde(default)]
    derived_from: Vec<String>,
    /// Code locations backing this: "path", "path:line", "path:10-20", optionally "@commit".
    #[serde(default)]
    code_refs: Vec<String>,
    /// Why this matters and to whom, e.g. "anyone touching refunds will double-charge". This is
    /// the context the curator lacks; it decides whether the knowledge is kept.
    #[serde(default)]
    reason: Option<String>,
    /// 1 (trivia) .. 5 (would have saved hours). Default 3.
    #[serde(default)]
    importance: Option<u8>,
    /// The lesson in one sentence, as you would tell your successor.
    #[serde(default)]
    candidate_lesson: Option<String>,
    /// An earlier evidence id of yours that this one corrects.
    #[serde(default)]
    supersedes: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SearchEvidenceArgs {
    /// Keywords matched against title, body, task, reason, lesson and tags.
    #[serde(default)]
    query: Option<String>,
    /// Only evidence about this repo.
    #[serde(default)]
    repo: Option<String>,
    /// Only evidence recorded by this agent.
    #[serde(default)]
    author: Option<String>,
    /// Only evidence of this kind (observation, failure, dispute, ...).
    #[serde(default)]
    kind: Option<String>,
    /// Start of the window: "2026-11-01" or an RFC 3339 time. Default: 30 days before 'until'.
    #[serde(default)]
    since: Option<String>,
    /// End of the window (same formats). Default: now. The window is at most 366 days.
    #[serde(default)]
    until: Option<String>,
    /// Also search the memories ported from Memory Hub 1.0 (evidence/legacy/...).
    #[serde(default)]
    include_legacy: bool,
    /// Maximum number of hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct RateArgs {
    /// Experience memory id.
    id: String,
    /// helpful: it saved you time. harmful: following it caused a problem. outdated: no longer
    /// true. For a concrete contradiction use dispute_memory instead.
    rating: RatingArg,
    /// What happened, in one or two sentences.
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct DisputeArgs {
    /// Experience memory id that is wrong.
    id: String,
    /// What you observed instead, in one or two sentences.
    reason: String,
    /// Longer details: commands, output, versions (recorded as the dispute evidence body).
    #[serde(default)]
    details: Option<String>,
    /// Code locations proving it, "path:line@commit".
    #[serde(default)]
    code_refs: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateFolderArgs {
    /// "agent" for your work area. Curators: "repo" or "agent".
    scope: ScopeArg,
    /// Repository id (scope=repo); curators may name an agent here for scope=agent.
    #[serde(default)]
    repo: Option<String>,
    /// Folder path, e.g. "working/refund-bug". Agents may only create folders under "working/".
    folder: String,
    /// One line on what belongs in this folder, shown by browse_memory (any Unicode). Replaces
    /// the current description of an existing folder; "" removes it.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MoveMemoryArgs {
    /// Memory id to move.
    id: String,
    /// Target folder in the same repo / agent space; agents: a folder under "working/".
    /// Missing folders are created.
    folder: String,
    /// New slug (file name); keeps the current one if omitted.
    #[serde(default)]
    slug: Option<String>,
    /// Fail if the memory's current version differs.
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct AddMemoryArgs {
    /// "agent" for a note in your own work area. Writing elsewhere (scope=repo, or an agent
    /// folder outside "working/") is curator-only; for agents it is recorded as evidence instead.
    scope: ScopeArg,
    /// Repository id when scope is "repo".
    #[serde(default)]
    repo: Option<String>,
    /// Folder; for agents "working" or a subfolder such as "working/pay-123". Defaults to
    /// "working" for scope=agent.
    #[serde(default)]
    folder: Option<String>,
    /// Short descriptive title in ASCII (English letters, digits, spaces, punctuation), e.g.
    /// "Refund bug plan". It also becomes the file name. Non-ASCII titles are rejected.
    title: String,
    /// Markdown content. Any Unicode is welcome: Chinese, Japanese, emoji, symbols, code.
    body: String,
    /// Category, e.g. todo, plan, notes, scratch.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// Groups to put the memory in.
    #[serde(default)]
    groups: Vec<String>,
    /// Explicit slug for the id; derived from the title if omitted.
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateMemoryArgs {
    /// Memory id (agents: a note in their own work area).
    id: String,
    /// New title; ASCII only (English letters, digits, spaces, punctuation).
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    /// Replaces all tags (any Unicode).
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// Replaces all groups.
    #[serde(default)]
    groups: Option<Vec<String>>,
    /// Replaces the whole body (Markdown, any Unicode).
    #[serde(default)]
    body: Option<String>,
    /// Appended to the body as a new paragraph (Markdown, any Unicode).
    #[serde(default)]
    append: Option<String>,
    /// Fail if the memory's current version differs (optimistic locking).
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteMemoryArgs {
    /// Memory id (agents: a note in their own work area).
    id: String,
    /// Fail if the memory's current version differs.
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct GroupArgs {
    /// Group name.
    group: String,
    /// Memory ids to add to or remove from the group.
    ids: Vec<String>,
    /// "add" (default) or "remove".
    #[serde(default)]
    action: GroupAction,
}

// ---- curator argument types ------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct TargetArgs {
    /// "repo:<repo id>" or "agent:<agent name>".
    target: String,
    /// Maximum number of items (evidence: default 25, backfill: default 10).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ProposeArgs {
    /// "repo:<repo id>" or "agent:<agent name>".
    target: String,
    /// Operations, each an object with an "op" field; see the tool description.
    #[schemars(with = "Vec<Map<String, Value>>")]
    ops: Vec<Value>,
    /// The batch cursor from curation_batch; the target's watermark advances to it. Omit for
    /// backfill proposals.
    #[serde(default)]
    cursor: Option<String>,
    /// Short summary of the proposal for reviewers.
    #[serde(default)]
    note: Option<String>,
    /// Only run the checks: nothing is stored and the watermark stays.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize, JsonSchema)]
struct VerdictArg {
    /// Operation index in the proposal (0-based).
    index: usize,
    approve: bool,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReviewArgs {
    /// Proposal id from propose_changes.
    proposal_id: String,
    /// One verdict per operation. Operations marked needs_human are applied only when a human
    /// approves them with `memhub2 proposals approve`.
    verdicts: Vec<VerdictArg>,
}

#[derive(Deserialize, JsonSchema)]
struct ProposalsArgs {
    /// Only proposals in this state.
    #[serde(default)]
    state: Option<ProposalStateArg>,
}

#[derive(Deserialize, JsonSchema)]
struct ProposalIdArgs {
    /// Proposal id.
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct ImportRegistryArgs {
    /// Agent profiles as returned by the 1.0 list_agents tool.
    #[serde(default)]
    #[schemars(with = "Vec<Map<String, Value>>")]
    agents: Vec<Value>,
    /// Repo profiles as returned by the 1.0 list_repos tool.
    #[serde(default)]
    #[schemars(with = "Vec<Map<String, Value>>")]
    repos: Vec<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct FolderArg {
    path: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ImportFoldersArgs {
    scope: ScopeArg,
    /// Repo id or agent name.
    owner: String,
    /// Folders as returned by the 1.0 browse_memory tool.
    folders: Vec<FolderArg>,
}

#[derive(Deserialize, JsonSchema)]
struct ImportMemoryArgs {
    /// The structuredContent of a 1.0 get_memory result (properties, body and read_count).
    #[schemars(with = "Map<String, Value>")]
    memory: Value,
    /// Only report where the memory would go.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize, JsonSchema)]
struct EvidenceIdArgs {
    /// Evidence id.
    id: String,
}

// ---- catalogue -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Audience {
    Everyone,
    Curator,
}

struct ToolDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    read_only: bool,
    destructive: bool,
    audience: Audience,
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "connect_agent",
        title: "Connect agent",
        description: "Identify this connection by agent name; call this first. If the agent does \
                      not exist yet, pass 'role' to create it. Your name is your identity: every \
                      user running the same agent shares its experience and work area.",
        schema: schema::<ConnectArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "whoami",
        title: "Who am I",
        description: "Show the identified agent, its working repos and connection details.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "list_agents",
        title: "List agents",
        description: "List all registered agents with their roles and working repos.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "add_repo",
        title: "Add working repo",
        description: "Add a repository to your working set, registering it if it is new. Required \
                      before recording evidence about the repo.",
        schema: schema::<AddRepoArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "leave_repo",
        title: "Leave repo",
        description: "Remove a repository from your working set. Its memories and evidence are kept.",
        schema: schema::<RepoArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "list_repos",
        title: "List repos",
        description: "List registered repositories with members and memory counts.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "browse_memory",
        title: "Browse experience tree",
        description: "Show the curated folder tree of a repo (or an agent's experience and work \
                      area) with folder descriptions, counts and memory titles with their status. \
                      Start here when you begin work on a repo: read the root overview, then the \
                      folders relevant to your task.",
        schema: schema::<BrowseArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "search_memory",
        title: "Search experience",
        description: "Keyword search over curated experience: repo memory plus your own agent \
                      space (experience and work area). Another agent's experience is included \
                      only when you name it in 'owner'; evidence is never included (use \
                      search_evidence). Superseded memories are left out unless \
                      include_superseded=true. Hits are ranked by relevance weighted by status \
                      (established > provisional > disputed) and by agents' ratings, and show \
                      status, support (independent sources) and feedback. Results with status \
                      provisional or disputed deserve a check before you rely on them.",
        schema: schema::<SearchArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "list_memories",
        title: "List memories",
        description: "List memories matching filters (folder, kind, status, ...), optionally \
                      grouped by group, kind, tag, owner, scope, author or folder.",
        schema: schema::<ListArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "get_memory",
        title: "Get memory or evidence",
        description: "Fetch the full content of an experience memory or an evidence record by id. \
                      A memory comes with its status, citations (cites.evidence: evidence ids; \
                      cites.code: code locations) and validity; open a cited evidence id with \
                      this tool to see the raw observation behind a claim. Old 1.0 ids and ids of \
                      moved memories are redirected. Each memory fetch counts as one read.",
        schema: schema::<GetArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "record_evidence",
        title: "Record evidence",
        description: "Record what you observed while working: this is how you add knowledge. \
                      Evidence is append-only and is not searched directly; the curator reviews \
                      new evidence daily and distills it into repo memory (when it is general \
                      knowledge about a repo) or into your agent experience (lessons about your \
                      way of working), citing your record. Record when you: found a pitfall or \
                      its fix, learned how something really works, made or learned a design \
                      decision, found a build/test/debug procedure, or found a memory missing or \
                      wrong (then also dispute_memory). Give the curator the context it lacks: \
                      task, outcome, reason (why it matters and to whom), candidate_lesson (one \
                      sentence), code_refs, and derived_from for memories you relied on. Be \
                      specific and factual; one finding per record. Do not record routine \
                      progress, status updates or to-dos (use your work area for those).",
        schema: schema::<RecordEvidenceArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "search_evidence",
        title: "Search evidence",
        description: "Search raw evidence records inside a time window (default: the last 30 \
                      days). Use it when curated experience has no answer yet but someone may have \
                      seen it recently, or to see what was observed about a repo. Evidence is \
                      unreviewed: treat it as a lead, not as fact.",
        schema: schema::<SearchEvidenceArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "rate_memory",
        title: "Rate memory",
        description: "Tell the hub whether an experience memory helped: helpful, harmful or \
                      outdated, with a short note. Ratings steer search ranking and tell the \
                      curator what to revise. Rate memories you actually used.",
        schema: schema::<RateArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "dispute_memory",
        title: "Dispute memory",
        description: "Report that an experience memory contradicts what you observed. Records \
                      your observation as evidence (kind dispute), marks the memory 'disputed' \
                      (searches rank it lower and flag it) and puts it on the curator's list. \
                      Include what is true instead and code_refs that prove it.",
        schema: schema::<DisputeArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "add_memory",
        title: "Add work note",
        description: "Create a note in your own work area (scope=agent, folder \"working\" or \
                      \"working/<task>\"): plans, to-dos, scratch notes and progress for your \
                      current tasks. Work notes are yours to edit, are found by your own searches \
                      and are never curated or shown to other agents' searches. Knowledge others \
                      should get belongs in record_evidence; calls that target repo memory or \
                      your curated experience are recorded as evidence instead.",
        schema: schema::<AddMemoryArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "update_memory",
        title: "Update work note",
        description: "Change a note in your work area: title, kind, tags, groups or body (replace \
                      or append). Only fields you pass are changed. Experience memories cannot be \
                      edited by agents: dispute_memory when one is wrong, record_evidence (with \
                      derived_from) when you have something to add.",
        schema: schema::<UpdateMemoryArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "create_folder",
        title: "Create folder",
        description: "Create a folder in your work area (below \"working/\"), with a one-line \
                      description, or update its description.",
        schema: schema::<CreateFolderArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "move_memory",
        title: "Move work note",
        description: "Move a note to another folder of your work area, optionally renaming it. \
                      The id changes; the old id keeps redirecting.",
        schema: schema::<MoveMemoryArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "delete_memory",
        title: "Delete work note",
        description: "Delete a note from your work area (moved to the hub's trash). Delete notes \
                      of finished tasks after recording what was worth keeping as evidence.",
        schema: schema::<DeleteMemoryArgs>,
        read_only: false,
        destructive: true,
        audience: Audience::Everyone,
    },
    ToolDef {
        name: "group_memories",
        title: "Group memories",
        description: "Add notes of your work area to, or remove them from, a named group. Use \
                      list_memories with group_by=group to view groups.",
        schema: schema::<GroupArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Everyone,
    },
    // ---- curator ----
    ToolDef {
        name: "curation_status",
        title: "Curation status",
        description: "Curator: every repo and agent target with its pending evidence, unreviewed \
                      1.0 memories (backfill), open proposals and last run.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "curation_batch",
        title: "Curation batch",
        description: "Curator: the next evidence of a target since its watermark (oldest first), \
                      each with the target's most related experience memories, plus searches \
                      that found nothing (what agents need but memory lacks) and negative \
                      feedback or disputes. Pass the returned cursor to propose_changes.",
        schema: schema::<TargetArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "curation_backfill",
        title: "Curation backfill",
        description: "Curator: memories ported from 1.0 into a target that were not reviewed yet \
                      (origin v1, provisional), with their 1.0 read counts. Review each with an \
                      update (keeps it, marks it curated), supersede (split or rewrite into new \
                      entries citing its legacy evidence) or retire.",
        schema: schema::<TargetArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "propose_changes",
        title: "Propose changes",
        description: "Curator: submit itemized changes for a target; nothing is applied until \
                      review_changes. Each op is checked (citations exist, support counted from \
                      independent evidence, established needs 2 sources or verified code cites, \
                      body limits 3000 chars for entries / 10000 for overview, architecture, \
                      implementation, module, guide, runbook; task state kinds rejected). Ops: \
                      {op:\"add\", scope, owner, folder, title, kind, body, tags, applies_to, \
                      cites:{evidence:[ids], code:[\"path:line@commit\"]}, status, \
                      verified_commit, slug} | {op:\"update\", id, expected_version, title?, \
                      kind?, body?, tags?, applies_to?, add_cites?, status?, verified_commit?} | \
                      {op:\"supersede\", id, by: id or \"#<index of an add op>\", reason} | \
                      {op:\"retire\", id, reason} | {op:\"move\", id, folder} | \
                      {op:\"describe_folder\", scope, owner, folder, description} | {op:\"noop\", \
                      evidence:[ids], reason}. Pass the batch cursor so the watermark advances.",
        schema: schema::<ProposeArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "review_changes",
        title: "Review changes",
        description: "Curator: approve or reject operations of a proposal; approved ones are \
                      re-checked and applied through the audited write path. Operations marked \
                      needs_human stay pending for `memhub2 proposals approve`.",
        schema: schema::<ReviewArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "list_proposals",
        title: "List proposals",
        description: "Curator: change proposals, oldest first, optionally by state.",
        schema: schema::<ProposalsArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "get_proposal",
        title: "Get proposal",
        description: "Curator: one proposal with its operations, checks and decisions.",
        schema: schema::<ProposalIdArgs>,
        read_only: true,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "import_registry",
        title: "Import 1.0 registry",
        description: "Curator (1.0 port): create agents and repos that do not exist yet from 1.0 \
                      list_agents / list_repos results, and add 1.0 working repos to existing agents.",
        schema: schema::<ImportRegistryArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "import_folders",
        title: "Import 1.0 folders",
        description: "Curator (1.0 port): recreate 1.0 folders with their descriptions. Agent \
                      todo/ folders move into working/; task-logs/ folders are skipped.",
        schema: schema::<ImportFoldersArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "import_memory",
        title: "Import 1.0 memory",
        description: "Curator (1.0 port): port one 1.0 memory. Its text is kept as legacy \
                      evidence (evidence/legacy/<1.0 id>); repo memory and agent insights become \
                      provisional experience under the same id, agent task logs stay evidence \
                      only, agent to-dos become work notes. Old ids redirect. Idempotent.",
        schema: schema::<ImportMemoryArgs>,
        read_only: false,
        destructive: false,
        audience: Audience::Curator,
    },
    ToolDef {
        name: "delete_evidence",
        title: "Delete evidence",
        description: "Curator: delete an evidence record (moved to the hub's trash), e.g. one \
                      holding a secret. Evidence is otherwise append-only.",
        schema: schema::<EvidenceIdArgs>,
        read_only: false,
        destructive: true,
        audience: Audience::Curator,
    },
];

/// The tools offered to a session; curators also get the curation tools.
pub fn definitions(curator: bool) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|t| curator || t.audience == Audience::Everyone)
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": {
                    "title": t.title,
                    "readOnlyHint": t.read_only,
                    "destructiveHint": t.destructive,
                    "openWorldHint": false,
                },
            })
        })
        .collect()
}

fn schema<T: JsonSchema>() -> Value {
    let mut settings = SchemaSettings::draft07();
    settings.inline_subschemas = true;
    settings.meta_schema = None;
    let mut value = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    simplify(&mut value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.insert("type".into(), json!("object"));
        obj.entry("properties").or_insert_with(|| json!({}));
    }
    value
}

/// Collapses `Option<T>` encodings (`["T","null"]`, `anyOf [T, null]`) into
/// plain `T`; optionality is already expressed by `required`. Keeps schemas
/// digestible for clients with limited JSON-Schema support.
fn simplify(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type") {
                let non_null: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if non_null.len() == 1 {
                    map.insert("type".into(), non_null[0].clone());
                }
            }
            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(variants)) = map.get(key) {
                    let non_null: Vec<Value> = variants
                        .iter()
                        .filter(|v| v.get("type") != Some(&json!("null")))
                        .cloned()
                        .collect();
                    if non_null.len() == 1 {
                        map.remove(key);
                        if let Value::Object(inner) = &non_null[0] {
                            for (k, v) in inner {
                                map.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                    }
                }
            }
            if let Some(Value::Array(values)) = map.get_mut("enum") {
                values.retain(|v| !v.is_null());
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            for child in map.values_mut() {
                simplify(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(simplify),
        _ => {}
    }
}

// ---- dispatch ----------------------------------------------------------------

#[derive(Debug)]
struct ToolError {
    code: &'static str,
    message: String,
}

impl From<AppError> for ToolError {
    fn from(e: AppError) -> Self {
        ToolError {
            code: e.code(),
            message: e.to_string(),
        }
    }
}

impl From<store::StoreError> for ToolError {
    fn from(e: store::StoreError) -> Self {
        AppError::from(e).into()
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError {
        code: "invalid_input",
        message: message.into(),
    }
}

struct ToolOutput {
    structured: Value,
    /// Human-oriented text; defaults to pretty-printed `structured`.
    text: Option<String>,
}

impl From<Value> for ToolOutput {
    fn from(structured: Value) -> Self {
        ToolOutput {
            structured,
            text: None,
        }
    }
}

/// Runs a tool and returns the `tools/call` result object, or `None` if the
/// tool does not exist.
pub fn call(session: &mut AgentSession, name: &str, args: Value) -> Option<Value> {
    let result = match name {
        "connect_agent" => connect_agent(session, args),
        "whoami" => session
            .whoami()
            .map_err(Into::into)
            .map(|w| json!(w).into()),
        "list_agents" => session
            .list_agents()
            .map_err(Into::into)
            .map(|a| json!({ "agents": a }).into()),
        "add_repo" => add_repo(session, args),
        "leave_repo" => leave_repo(session, args),
        "list_repos" => session
            .list_repos()
            .map_err(Into::into)
            .map(|r| json!({ "repos": r }).into()),
        "browse_memory" => browse_memory(session, args),
        "search_memory" => search_memory(session, args),
        "list_memories" => list_memories(session, args),
        "get_memory" => get_memory(session, args),
        "record_evidence" => record_evidence(session, args),
        "search_evidence" => search_evidence(session, args),
        "rate_memory" => rate_memory(session, args),
        "dispute_memory" => dispute_memory(session, args),
        "add_memory" => add_memory(session, args),
        "update_memory" => update_memory(session, args),
        "create_folder" => create_folder(session, args),
        "move_memory" => move_memory(session, args),
        "delete_memory" => delete_memory(session, args),
        "group_memories" => group_memories(session, args),
        "curation_status" => session
            .curation_status()
            .map_err(Into::into)
            .map(|t| json!({ "targets": t }).into()),
        "curation_batch" => curation_batch(session, args),
        "curation_backfill" => curation_backfill(session, args),
        "propose_changes" => propose_changes(session, args),
        "review_changes" => review_changes(session, args),
        "list_proposals" => list_proposals(session, args),
        "get_proposal" => get_proposal(session, args),
        "import_registry" => import_registry(session, args),
        "import_folders" => import_folders(session, args),
        "import_memory" => import_memory(session, args),
        "delete_evidence" => delete_evidence(session, args),
        _ => return None,
    };
    if let Err(e) = &result {
        tracing::debug!(tool = name, agent = ?session.agent(), "tool failed: {}", e.message);
    }
    Some(match result {
        Ok(out) => {
            let text = out.text.unwrap_or_else(|| {
                serde_json::to_string_pretty(&out.structured).unwrap_or_default()
            });
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": out.structured,
                "isError": false,
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("error [{}]: {}", e.code, e.message) }],
            "isError": true,
        }),
    })
}

type ToolResult = Result<ToolOutput, ToolError>;

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| invalid(format!("invalid arguments: {e}")))
}

fn memory_id(id: &str) -> Result<MemoryId, ToolError> {
    Ok(id.trim().trim_end_matches(".md").parse::<MemoryId>()?)
}

fn target(s: &str) -> Result<Target, ToolError> {
    Ok(s.parse::<Target>()?)
}

fn summary(meta: &MemoryMeta) -> Value {
    let mut v = json!({
        "id": meta.id,
        "folder": meta.folder(),
        "title": meta.title,
        "kind": meta.kind,
        "tags": meta.tags,
        "groups": meta.groups,
        "author": meta.author,
        "updated_by": meta.updated_by,
        "version": meta.version,
        "updated_at": meta.updated_at,
    });
    let x = &meta.experience;
    if meta.is_work_note() {
        v["work_note"] = json!(true);
    }
    if let Some(status) = x.status {
        v["status"] = json!(status);
        v["support"] = json!(x.support);
    }
    if !x.applies_to.is_empty() {
        v["applies_to"] = json!(x.applies_to);
    }
    if let Some(by) = &x.superseded_by {
        v["superseded_by"] = json!(by);
    }
    if let Some(verified) = &x.last_verified {
        v["last_verified"] = json!(verified);
    }
    if x.origin.is_some() && x.curated_at.is_none() {
        v["origin"] = json!(x.origin);
    }
    v
}

fn evidence_summary(meta: &EvidenceMeta) -> Value {
    let mut v = json!({
        "id": meta.id,
        "title": meta.title,
        "kind": meta.kind,
        "author": meta.author,
        "created_at": meta.created_at,
        "outcome": meta.outcome,
        "source": meta.source,
    });
    if !meta.repos.is_empty() {
        v["repos"] = json!(meta.repos);
    }
    if let Some(t) = &meta.task {
        v["task"] = json!(t);
    }
    if let Some(l) = &meta.candidate_lesson {
        v["candidate_lesson"] = json!(l);
    }
    v
}

fn connect_agent(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ConnectArgs = parse(args)?;
    let connected = session.connect(&a.name, a.role.as_deref(), a.description.as_deref())?;
    let mut out = json!(connected);
    out["next_step"] = json!(if connected.agent.repos.is_empty() {
        "Call add_repo for each repository you work on, then browse_memory(scope=\"repo\", owner=<repo>)."
            .to_string()
    } else {
        format!(
            "Your working repos: {}. Browse one with browse_memory(scope=\"repo\", owner=<repo>); \
             your own experience and work area: browse_memory(scope=\"agent\").",
            connected.agent.repos.join(", ")
        )
    });
    Ok(out.into())
}

fn add_repo(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: AddRepoArgs = parse(args)?;
    let join = session.add_repo(&a.repo, a.description.as_deref(), a.location.as_deref())?;
    let mut out = json!(join);
    let tree = session.browse(Scope::Repo, Some(&join.repo.id), "")?;
    out["memory_count"] = json!(tree.memories.len());
    out["next_step"] = json!(repo_next_step(&tree));
    Ok(out.into())
}

fn leave_repo(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: RepoArgs = parse(args)?;
    let repos = session.leave_repo(&a.repo)?;
    Ok(json!({ "working_repos": repos }).into())
}

fn hit_json(h: &ExperienceHit) -> Value {
    let mut v = summary(&h.hit.meta);
    v["score"] = json!((h.hit.score * 1000.0).round() / 1000.0);
    v["snippet"] = json!(h.hit.snippet);
    let fb = &h.feedback;
    if !fb.is_empty() {
        v["feedback"] = json!({
            "helpful": fb.helpful,
            "harmful": fb.harmful,
            "outdated": fb.outdated,
            "disputes": fb.disputes,
        });
    }
    let cites = &h.hit.meta.experience.cites;
    if !cites.is_empty() {
        v["cites"] = json!(cites.evidence.len() + cites.code.len());
    }
    v
}

fn search_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: SearchArgs = parse(args)?;
    let text = a.query.filter(|q| !q.trim().is_empty());
    let searched = text.is_some();
    let hits = session.search_experience(
        &SearchQuery {
            text,
            filter: a.filter.try_into()?,
            limit: a.limit,
        },
        SearchOptions {
            include_superseded: a.include_superseded,
        },
    )?;
    let items: Vec<Value> = hits.iter().map(hit_json).collect();
    let mut out = json!({ "count": items.len(), "hits": items });
    let mut hints = Vec::new();
    if searched && hits.is_empty() {
        hints.push(
            "No curated experience matches. Try fewer or other keywords, or search_evidence for \
             recent raw observations. If you work it out, record_evidence so the next agent finds it."
                .to_string(),
        );
    }
    if hits.iter().any(|h| h.hit.meta.status() == Some(Status::Disputed)) {
        hints.push(
            "Some hits are disputed: an agent reported they contradict what it observed. Check \
             them (get_memory shows the dispute evidence in cites) before relying on them."
                .to_string(),
        );
    }
    if !hints.is_empty() {
        out["hints"] = json!(hints);
    }
    Ok(out.into())
}

fn list_memories(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ListArgs = parse(args)?;
    let filter: Filter = a.filter.try_into()?;
    match a.group_by {
        Some(key) => {
            let groups = session.group_by(&filter, key.into())?;
            let groups: Map<String, Value> = groups
                .into_iter()
                .map(|(k, metas)| (k, Value::Array(metas.iter().map(summary).collect())))
                .collect();
            Ok(json!({ "group_by": key_name(key), "groups": groups }).into())
        }
        None => {
            let metas = session.list(&filter)?;
            let items: Vec<Value> = metas.iter().map(summary).collect();
            Ok(json!({ "count": items.len(), "memories": items }).into())
        }
    }
}

fn key_name(key: GroupKeyArg) -> &'static str {
    match key {
        GroupKeyArg::Group => "group",
        GroupKeyArg::Kind => "kind",
        GroupKeyArg::Tag => "tag",
        GroupKeyArg::Owner => "owner",
        GroupKeyArg::Scope => "scope",
        GroupKeyArg::Author => "author",
        GroupKeyArg::Folder => "folder",
    }
}

/// Status notices shown above a memory's text.
fn memory_notices(meta: &MemoryMeta) -> Vec<String> {
    let x = &meta.experience;
    let mut out = Vec::new();
    match x.status {
        Some(Status::Disputed) => out.push(
            "DISPUTED: an agent reported that this contradicts what it observed (see the dispute \
             evidence in cites.evidence). Verify before relying on it."
                .to_string(),
        ),
        Some(Status::Superseded) => out.push(match &x.superseded_by {
            Some(by) => format!("SUPERSEDED by {by}; read that one instead. Kept for history."),
            None => "RETIRED: no longer true. Kept for history.".to_string(),
        }),
        Some(Status::Provisional) if x.origin.is_some() && x.curated_at.is_none() => out.push(
            "PORTED from Memory Hub 1.0 and not reviewed yet (provisional). Verify against the \
             code before relying on it."
                .to_string(),
        ),
        Some(Status::Provisional) => out.push(format!(
            "provisional: backed by {} independent source(s) so far.",
            x.support
        )),
        _ => {}
    }
    out
}

fn get_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: GetArgs = parse(args)?;
    match session.fetch(&a.id)? {
        Fetched::Memory {
            memory,
            redirected_from,
        } => {
            let mut structured = json!(memory);
            structured["read_count"] = json!(session.hub().reads().get(&memory.meta.id).count);
            structured["layer"] = json!(if memory.meta.is_work_note() {
                "work_note"
            } else {
                "experience"
            });
            let mut notices = memory_notices(&memory.meta);
            if let Some(from) = &redirected_from {
                notices.insert(0, format!("{from} now lives at {}.", memory.meta.id));
                structured["redirected_from"] = json!(from);
            }
            let mut text = String::new();
            for n in &notices {
                text.push_str(&format!("[memhub] {n}\n"));
            }
            if !notices.is_empty() {
                structured["notices"] = json!(notices);
                text.push('\n');
            }
            text.push_str(&store::render_memory(&memory)?);
            Ok(ToolOutput {
                text: Some(text),
                structured,
            })
        }
        Fetched::Evidence {
            evidence,
            redirected_from,
        } => {
            let mut structured = json!(evidence);
            structured["layer"] = json!("evidence");
            let mut text = String::new();
            if let Some(from) = &redirected_from {
                text.push_str(&format!(
                    "[memhub] {from} was ported from Memory Hub 1.0 as evidence {}.\n",
                    evidence.meta.id
                ));
                structured["redirected_from"] = json!(from);
            }
            text.push_str(
                "[memhub] Evidence: a raw, unreviewed observation by one agent at one time.\n\n",
            );
            text.push_str(&store::render_evidence(&evidence)?);
            Ok(ToolOutput {
                text: Some(text),
                structured,
            })
        }
    }
}

fn record_evidence(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: RecordEvidenceArgs = parse(args)?;
    let evidence = session.record_evidence(RecordEvidence {
        title: a.title,
        body: a.body,
        kind: a.kind,
        repos: a.repos,
        task: a.task,
        outcome: a.outcome.map(Into::into).unwrap_or_default(),
        source: a.source.map(Into::into),
        derived_from: a.derived_from,
        code_refs: a.code_refs,
        reason: a.reason,
        importance: a.importance,
        candidate_lesson: a.candidate_lesson,
        supersedes: a.supersedes,
        tags: a.tags,
    })?;
    let m = &evidence.meta;
    let mut hints = Vec::new();
    if m.reason.is_none() && m.candidate_lesson.is_none() {
        hints.push(
            "Next time add 'reason' (why it matters, to whom) and 'candidate_lesson' (one \
             sentence); the curator decides what to keep from them."
                .to_string(),
        );
    }
    if m.repos.is_empty() {
        hints.push(
            "No repos given: this is curated into your own agent experience only. Pass repos=[...] \
             when it is knowledge about a repository."
                .to_string(),
        );
    }
    let mut out = json!({
        "recorded": evidence_summary(m),
        "next": "The curator reviews new evidence daily and distills it into experience that cites this id.",
    });
    if !hints.is_empty() {
        out["hints"] = json!(hints);
    }
    Ok(out.into())
}

fn parse_time(s: Option<&str>, end_of_day: bool) -> Result<Option<DateTime<Utc>>, ToolError> {
    let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(Some(t.with_timezone(&Utc)));
    }
    let day = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| invalid(format!("invalid time '{s}': use 2026-11-01 or an RFC 3339 time")))?;
    let time = if end_of_day {
        day.and_hms_milli_opt(23, 59, 59, 999)
    } else {
        day.and_hms_opt(0, 0, 0)
    };
    Ok(time.map(|t| t.and_utc()))
}

fn search_evidence(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: SearchEvidenceArgs = parse(args)?;
    let hits: Vec<EvidenceHit> = session.search_evidence(&EvidenceSearch {
        text: a.query,
        repo: a.repo,
        author: a.author,
        kind: a.kind,
        since: parse_time(a.since.as_deref(), false)?,
        until: parse_time(a.until.as_deref(), true)?,
        include_legacy: a.include_legacy,
        limit: a.limit,
    })?;
    let items: Vec<Value> = hits
        .iter()
        .map(|h| {
            let mut v = evidence_summary(&h.meta);
            v["snippet"] = json!(h.snippet);
            v
        })
        .collect();
    Ok(json!({
        "count": items.len(),
        "hits": items,
        "note": "Evidence is unreviewed; open a record with get_memory(id) for the full text.",
    })
    .into())
}

fn rate_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: RateArgs = parse(args)?;
    let id = memory_id(&a.id)?;
    let feedback = session.rate_memory(&id, a.rating.into(), a.note.as_deref())?;
    Ok(json!({ "id": id, "feedback": feedback }).into())
}

fn dispute_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: DisputeArgs = parse(args)?;
    let id = memory_id(&a.id)?;
    let (evidence, memory) =
        session.dispute_memory(&id, &a.reason, a.details.as_deref(), a.code_refs)?;
    Ok(json!({
        "disputed": summary(&memory.meta),
        "evidence": evidence_summary(&evidence.meta),
        "next": "The curator will re-check the memory against your evidence.",
    })
    .into())
}

/// Whether an agent (non-curator) write lands in its own work area.
fn in_work_area(session: &AgentSession, scope: Scope, owner: Option<&str>, folder: &str) -> bool {
    scope == Scope::Agent
        && owner.is_none_or(|o| Some(o) == session.agent())
        && store::is_under(folder, WORK_FOLDER)
}

fn add_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: AddMemoryArgs = parse(args)?;
    let scope: Scope = a.scope.into();
    let folder = match (scope, a.folder.as_deref().map(str::trim)) {
        (Scope::Agent, None | Some("")) if !session.is_curator() => WORK_FOLDER.to_string(),
        (_, f) => store::normalize_folder(f.unwrap_or(""))?,
    };
    if !session.is_curator() && !in_work_area(session, scope, None, &folder) {
        return add_as_evidence(session, scope, a, &folder);
    }
    let owner = match scope {
        Scope::Repo => a.repo.clone(),
        Scope::Agent => session.agent().map(str::to_string),
    };
    let new_folders = owner
        .as_deref()
        .map(|o| session.missing_folders(scope, o.trim(), &folder).unwrap_or_default())
        .unwrap_or_default();
    let memory = session.add_memory(AddMemory {
        scope,
        repo: a.repo,
        owner: None,
        folder,
        title: a.title,
        kind: a.kind,
        tags: a.tags,
        groups: a.groups,
        body: a.body,
        slug: a.slug,
        experience: Default::default(),
    })?;
    let mut out = json!({ "created": summary(&memory.meta) });
    let tree = session.browse(scope, Some(memory.meta.owner()), "")?;
    let hints = add_hints(&tree, &memory.meta, &new_folders);
    if !hints.is_empty() {
        out["hints"] = json!(hints);
    }
    Ok(out.into())
}

/// A 1.0-style write into curated memory, kept as evidence so the knowledge
/// reaches the curator.
fn add_as_evidence(session: &mut AgentSession, scope: Scope, a: AddMemoryArgs, folder: &str) -> ToolResult {
    let repos = match scope {
        Scope::Repo => vec![a
            .repo
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| invalid("'repo' is required for scope=repo"))?],
        Scope::Agent => Vec::new(),
    };
    let mut tags = a.tags;
    if !folder.is_empty() {
        tags.push(format!("folder:{folder}"));
    }
    let evidence = session.record_evidence(RecordEvidence {
        title: a.title,
        body: a.body,
        kind: a.kind,
        repos,
        tags,
        ..Default::default()
    })?;
    let place = match scope {
        Scope::Repo => format!("repo memory of '{}'", evidence.meta.repos.join(",")),
        Scope::Agent => "your curated agent experience".to_string(),
    };
    Ok(json!({
        "layer": "evidence",
        "recorded": evidence_summary(&evidence.meta),
        "hints": [format!(
            "Agents do not write {place} directly in Memory Hub 2.0: this was recorded as evidence {} \
             and the curator will distill it. Next time call record_evidence (with task, reason and \
             candidate_lesson). For your own plans and notes use add_memory(scope=\"agent\", \
             folder=\"{WORK_FOLDER}/...\").",
            evidence.meta.id
        )],
    })
    .into())
}

fn browse_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: BrowseArgs = parse(args)?;
    let scope: Scope = a.scope.map(Into::into).unwrap_or(Scope::Repo);
    let mut tree = session.browse(scope, a.owner.as_deref(), a.folder.as_deref().unwrap_or(""))?;
    let hidden = if a.include_superseded {
        0
    } else {
        hide_superseded(&mut tree)
    };
    let mut text = render_tree(&tree, a.memories.unwrap_or(true));
    if hidden > 0 {
        text.push_str(&format!(
            "({hidden} superseded {} hidden; include_superseded=true shows them)\n",
            if hidden == 1 { "memory" } else { "memories" }
        ));
    }
    let folders: Vec<Value> = tree
        .folders
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "description": f.description,
                "memory_count": f.memory_count,
                "total_count": f.total_count,
            })
        })
        .collect();
    let memories: Vec<Value> = tree
        .memories
        .iter()
        .map(|m| {
            let mut v = json!({ "id": m.id, "folder": m.folder(), "title": m.title, "kind": m.kind });
            if let Some(s) = m.status() {
                v["status"] = json!(s);
            }
            v
        })
        .collect();
    let mut structured = json!({
        "scope": tree.scope,
        "owner": tree.owner,
        "folder": tree.folder,
        "description": tree.owner_description,
        "folders": folders,
        "memories": memories,
    });
    if hidden > 0 {
        structured["superseded_hidden"] = json!(hidden);
    }
    Ok(ToolOutput {
        structured,
        text: Some(text),
    })
}

/// Drops superseded memories from a tree and from its folder counts.
/// Returns how many were dropped.
fn hide_superseded(tree: &mut Browse) -> usize {
    let (gone, kept): (Vec<MemoryMeta>, Vec<MemoryMeta>) = std::mem::take(&mut tree.memories)
        .into_iter()
        .partition(|m| m.experience.is_superseded());
    tree.memories = kept;
    for m in &gone {
        for f in tree.folders.iter_mut() {
            if f.path == m.folder() {
                f.memory_count = f.memory_count.saturating_sub(1);
            }
            if store::is_under(m.folder(), &f.path) {
                f.total_count = f.total_count.saturating_sub(1);
            }
        }
    }
    gone.len()
}

fn create_folder(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: CreateFolderArgs = parse(args)?;
    let result = session.create_folder(
        a.scope.into(),
        a.repo.as_deref(),
        &a.folder,
        a.description.as_deref(),
    )?;
    let mut out = json!(result);
    if result.description.is_none() {
        out["hints"] = json!([format!(
            "Folder '{}' has no description. Pass a one-line description so browse_memory explains it.",
            result.folder
        )]);
    }
    Ok(out.into())
}

fn move_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: MoveMemoryArgs = parse(args)?;
    let change = session.move_memory(
        &memory_id(&a.id)?,
        &a.folder,
        a.slug.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        a.expected_version,
    )?;
    Ok(json!({
        "moved": summary(&change.after.meta),
        "previous_id": change.before.meta.id,
        "changed_fields": change.changed_fields,
    })
    .into())
}

fn update_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: UpdateMemoryArgs = parse(args)?;
    let change = session.update_memory(
        &memory_id(&a.id)?,
        MemoryPatch {
            title: a.title,
            kind: a.kind,
            tags: a.tags,
            groups: a.groups,
            body: a.body,
            append: a.append,
            expected_version: a.expected_version,
            ..Default::default()
        },
    )?;
    Ok(json!({
        "updated": summary(&change.after.meta),
        "changed_fields": change.changed_fields,
        "previous_version": change.before.meta.version,
    })
    .into())
}

fn delete_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: DeleteMemoryArgs = parse(args)?;
    let memory = session.delete_memory(&memory_id(&a.id)?, a.expected_version)?;
    Ok(json!({ "deleted": summary(&memory.meta) }).into())
}

fn group_memories(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: GroupArgs = parse(args)?;
    let ids = a
        .ids
        .iter()
        .map(|id| memory_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let add = matches!(a.action, GroupAction::Add);
    let changed = session.group(&ids, &a.group, add)?;
    Ok(json!({
        "group": a.group.trim(),
        "action": if add { "add" } else { "remove" },
        "changed": changed,
    })
    .into())
}

// ---- curator tools --------------------------------------------------------------

fn curation_batch(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: TargetArgs = parse(args)?;
    let batch = session.curation_batch(&target(&a.target)?, a.limit)?;
    Ok(json!(batch).into())
}

fn curation_backfill(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: TargetArgs = parse(args)?;
    let batch = session.curation_backfill(&target(&a.target)?, a.limit)?;
    Ok(json!(batch).into())
}

fn propose_changes(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ProposeArgs = parse(args)?;
    let mut ops = Vec::with_capacity(a.ops.len());
    let mut errors = Vec::new();
    for (i, op) in a.ops.into_iter().enumerate() {
        match serde_json::from_value::<CurationOp>(op) {
            Ok(op) => ops.push(op),
            Err(e) => errors.push(format!("ops[{i}]: {e}")),
        }
    }
    if !errors.is_empty() {
        return Err(invalid(format!(
            "malformed operations (nothing was stored): {}",
            errors.join("; ")
        )));
    }
    let target = target(&a.target)?;
    if a.dry_run {
        let checks = session.check_changes(&target, &ops)?;
        return Ok(json!({ "dry_run": true, "checks": checks }).into());
    }
    let proposal = session.propose_changes(&target, ops, a.cursor.as_deref(), a.note.as_deref())?;
    Ok(json!(proposal).into())
}

fn review_changes(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ReviewArgs = parse(args)?;
    let verdicts: Vec<Verdict> = a
        .verdicts
        .into_iter()
        .map(|v| Verdict {
            index: v.index,
            approve: v.approve,
            reason: v.reason,
        })
        .collect();
    let proposal = session.review_changes(&a.proposal_id, &verdicts, false)?;
    Ok(json!(proposal).into())
}

fn list_proposals(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ProposalsArgs = parse(args)?;
    let proposals = session.list_proposals(a.state.map(Into::into))?;
    let items: Vec<Value> = proposals
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "target": p.target,
                "created_at": p.created_at,
                "state": p.state,
                "ops": p.ops.len(),
                "undecided": p.undecided().len(),
                "note": p.note,
            })
        })
        .collect();
    Ok(json!({ "count": items.len(), "proposals": items }).into())
}

fn get_proposal(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ProposalIdArgs = parse(args)?;
    Ok(json!(session.get_proposal(a.id.trim())?).into())
}

fn import_registry(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ImportRegistryArgs = parse(args)?;
    let agents = a
        .agents
        .into_iter()
        .map(serde_json::from_value::<AgentProfile>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("invalid agent profile: {e}")))?;
    let repos = a
        .repos
        .into_iter()
        .map(serde_json::from_value::<RepoProfile>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("invalid repo profile: {e}")))?;
    Ok(json!(session.import_registry(agents, repos)?).into())
}

fn import_folders(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ImportFoldersArgs = parse(args)?;
    let folders: Vec<(String, Option<String>)> =
        a.folders.into_iter().map(|f| (f.path, f.description)).collect();
    let written = session.import_folders(a.scope.into(), a.owner.trim(), &folders)?;
    Ok(json!({ "folders": written }).into())
}

fn import_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ImportMemoryArgs = parse(args)?;
    let (memory, read_count) = v1_memory_from_json(a.memory)?;
    Ok(json!(session.import_memory(memory, read_count, a.dry_run)?).into())
}

fn delete_evidence(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: EvidenceIdArgs = parse(args)?;
    let id: EvidenceId = a.id.parse()?;
    let evidence = session.delete_evidence(&id)?;
    Ok(json!({ "deleted": evidence_summary(&evidence.meta) }).into())
}

// ---- organization guidance -------------------------------------------------------

/// Notes allowed directly in the work area before add_memory suggests
/// per-task folders.
const WORK_ROOT_SOFT_LIMIT: usize = 8;
/// Memories directly in one subfolder before add_memory suggests splitting it.
const FOLDER_SOFT_LIMIT: usize = 12;

fn add_hints(tree: &Browse, meta: &MemoryMeta, new_folders: &[String]) -> Vec<String> {
    let mut hints = Vec::new();
    let folder = meta.folder();
    let count = tree
        .folders
        .iter()
        .find(|f| f.path == folder)
        .map_or(0, |f| f.memory_count);
    if folder == WORK_FOLDER && count > WORK_ROOT_SOFT_LIMIT {
        hints.push(format!(
            "Your work area holds {count} notes directly. Keep one folder per task \
             (e.g. folder=\"{WORK_FOLDER}/<task>\") and delete notes of finished tasks after \
             recording what was worth keeping with record_evidence."
        ));
    } else if folder != WORK_FOLDER && count > FOLDER_SOFT_LIMIT {
        hints.push(format!(
            "Folder '{folder}' holds {count} memories directly. Consider subfolders (create_folder + move_memory)."
        ));
    }
    let place = match meta.scope() {
        Scope::Repo => format!("scope=\"repo\", repo=\"{}\"", meta.owner()),
        Scope::Agent => "scope=\"agent\"".to_string(),
    };
    for f in new_folders.iter().filter(|f| f.as_str() != WORK_FOLDER) {
        hints.push(format!(
            "Created folder '{f}'. Describe it with create_folder({place}, folder=\"{f}\", description=...)."
        ));
    }
    hints
}

fn repo_next_step(tree: &Browse) -> String {
    if tree.memories.is_empty() {
        format!(
            "Repo '{}' has no curated memory yet. As you learn how it works (build, architecture, \
             pitfalls, decisions), record_evidence(repos=[\"{}\"], ...); the curator builds the \
             repo memory from it.",
            tree.owner, tree.owner
        )
    } else {
        format!(
            "Repo '{}' has {} memories. Call browse_memory(scope=\"repo\", owner=\"{}\") to see the \
             folder tree, read the root overview first, then the folders relevant to your task.",
            tree.owner,
            tree.memories.len(),
            tree.owner
        )
    }
}

/// Indented outline of a memory tree, e.g.
///
/// ```text
/// repo/payment-service: Payment backend (5 memories)
///   overview  Payment service overview [overview]
///   architecture/  How requests flow through the service (2)
///     request-flow  Request flow [architecture, established]
/// ```
fn render_tree(tree: &Browse, with_memories: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let total = tree.root().map_or(0, |f| f.total_count);
    let _ = write!(out, "{}/{}", tree.scope, tree.owner);
    if !tree.folder.is_empty() {
        let _ = write!(out, "/{}", tree.folder);
    }
    let description = if tree.folder.is_empty() {
        Some(tree.owner_description.as_str()).filter(|d| !d.is_empty())
    } else {
        tree.root().and_then(|f| f.description.as_deref())
    };
    if let Some(d) = description {
        let _ = write!(out, ": {d}");
    }
    let _ = writeln!(
        out,
        " ({total} {})",
        if total == 1 { "memory" } else { "memories" }
    );
    let base = tree.root().map_or(0, |f| f.depth());
    let write_memories = |out: &mut String, path: &str, indent: usize| {
        for m in tree.memories.iter().filter(|m| m.folder() == path) {
            let status = match m.status() {
                Some(s) if s != Status::Provisional => format!(", {s}"),
                _ => String::new(),
            };
            let _ = writeln!(
                out,
                "{:indent$}{}  {} [{}{status}]",
                "",
                m.id.slug,
                m.title,
                m.kind,
                indent = indent
            );
        }
    };
    if with_memories {
        write_memories(&mut out, &tree.folder, 2);
    }
    for f in tree.folders.iter().filter(|f| f.path != tree.folder) {
        let indent = (f.depth() - base) * 2;
        let _ = write!(out, "{:indent$}{}/", "", f.name(), indent = indent);
        if let Some(d) = &f.description {
            let _ = write!(out, "  {d}");
        } else if tree.scope == Scope::Agent && f.path == WORK_FOLDER {
            let _ = write!(out, "  work area: the agent's own notes (not curated)");
        }
        let _ = writeln!(out, " ({})", f.total_count);
        if with_memories {
            write_memories(&mut out, &f.path, indent + 2);
        }
    }
    if total == 0 {
        out.push_str("  (empty)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_flat_objects() {
        for tool in definitions(true) {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{}", tool["name"]);
            let text = schema.to_string();
            assert!(!text.contains("\"null\""), "{}: {text}", tool["name"]);
            assert!(!text.contains("$ref"), "{}: {text}", tool["name"]);
        }
        let add = definitions(false)
            .into_iter()
            .find(|t| t["name"] == "add_memory")
            .unwrap();
        let schema = &add["inputSchema"];
        assert_eq!(
            schema["properties"]["scope"]["enum"],
            json!(["repo", "agent"])
        );
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort();
        assert_eq!(required, vec!["body", "scope", "title"]);

        let search = definitions(false)
            .into_iter()
            .find(|t| t["name"] == "search_memory")
            .unwrap();
        assert!(
            search["inputSchema"]["properties"]["owner"].is_object(),
            "flattened filter"
        );
        let propose = definitions(true)
            .into_iter()
            .find(|t| t["name"] == "propose_changes")
            .unwrap();
        assert_eq!(propose["inputSchema"]["properties"]["ops"]["items"]["type"], "object");
    }

    #[test]
    fn curator_tools_are_listed_for_curators_only() {
        let names = |curator| -> Vec<String> {
            definitions(curator)
                .into_iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        let agent = names(false);
        let curator = names(true);
        assert!(agent.contains(&"record_evidence".to_string()));
        assert!(!agent.iter().any(|n| n.starts_with("curation_") || n.starts_with("import_")));
        assert!(curator.contains(&"propose_changes".to_string()));
        assert!(curator.len() > agent.len());
    }

    #[test]
    fn times_accept_days_and_rfc3339() {
        let since = parse_time(Some("2026-11-01"), false).unwrap().unwrap();
        assert_eq!(since.to_rfc3339(), "2026-11-01T00:00:00+00:00");
        let until = parse_time(Some("2026-11-01"), true).unwrap().unwrap();
        assert!(until > since);
        assert!(parse_time(Some("2026-11-01T08:00:00+08:00"), false).unwrap().is_some());
        assert!(parse_time(Some("yesterday"), false).is_err());
        assert!(parse_time(None, false).unwrap().is_none());
    }
}
