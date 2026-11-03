//! Curation: how evidence becomes experience.
//!
//! The curator (a session holding the curator token, usually the
//! `memhub2 curate` LLM agent) works per *target*, one repo (`repo:<id>`) or
//! one agent (`agent:<name>`):
//!
//! 1. [`AgentSession::curation_batch`] returns the evidence recorded since the
//!    target's watermark, each with the experience memories most related to
//!    it, plus searches that found nothing and feedback on existing memories.
//! 2. [`AgentSession::propose_changes`] submits itemized operations (add,
//!    update, supersede, retire, move, describe_folder, noop). Every operation
//!    is checked deterministically: citations must exist, support is counted
//!    from independent evidence only, `established` needs two independent
//!    sources or a verified code citation, large rewrites need a human. The
//!    proposal is stored and the watermark advances.
//! 3. [`AgentSession::review_changes`] approves or rejects operations (an LLM
//!    verifier, auto-approval, or a human via `memhub2 proposals`) and
//!    applies the approved ones through the normal audited write path.
//!
//! ```text
//! <data_root>/curation/state.json              watermarks and last runs per target
//! <data_root>/curation/proposals/<id>.json     proposals with checks and decisions
//! ```

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    str::FromStr,
};

use chrono::{DateTime, Duration, Utc};
use memhub_audit::{AuditEvent, LogQuery};
use memhub_store::{
    is_under, normalize_folder, validate_code_ref, Cites, Evidence, EvidenceId, EvidenceQuery,
    ExperienceMeta, Filter, FolderInfo, Memory, MemoryId, MemoryPatch, Scope, SearchQuery, Status,
    StoreError, Verified, WORK_FOLDER,
};
use serde::{Deserialize, Serialize};

use crate::{
    admin::is_internal_agent,
    error::{AppError, Result},
    feedback::{load_json, save_json, Feedback},
    session::{AddMemory, AgentSession},
};

/// Evidence records per batch when the caller gives no limit.
pub const DEFAULT_BATCH_SIZE: usize = 25;
/// Operations accepted in one proposal.
pub const MAX_OPS: usize = 50;
/// Longest body of an atomic experience entry (pitfall, decision, ...).
pub const MAX_ENTRY_CHARS: usize = 3000;
/// Longest body of a curated page (overview, architecture, module, guide).
pub const MAX_PAGE_CHARS: usize = 10_000;
/// An update that shrinks a body by more than this fraction needs a human.
pub const MAX_SHRINK: f64 = 0.4;
/// Bodies shorter than this are exempt from the shrink guard.
const SHRINK_GUARD_MIN_CHARS: usize = 400;
/// Lookback for searches and feedback when a target was never curated.
const FIRST_RUN_LOOKBACK_DAYS: i64 = 30;
const RELATED_PER_EVIDENCE: usize = 3;

const PAGE_KINDS: &[&str] = &[
    "overview",
    "architecture",
    "implementation",
    "module",
    "guide",
    "runbook",
];
const NON_EXPERIENCE_KINDS: &[&str] = &[
    "task-log", "log", "worklog", "todo", "plan", "progress", "status",
];

/// A unit of curation: one repo's memory or one agent's experience.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    Repo(String),
    Agent(String),
}

impl Target {
    pub fn scope(&self) -> Scope {
        match self {
            Target::Repo(_) => Scope::Repo,
            Target::Agent(_) => Scope::Agent,
        }
    }

    pub fn owner(&self) -> &str {
        match self {
            Target::Repo(o) | Target::Agent(o) => o,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Repo(r) => write!(f, "repo:{r}"),
            Target::Agent(a) => write!(f, "agent:{a}"),
        }
    }
}

impl FromStr for Target {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self> {
        let (kind, owner) = s.trim().split_once(':').ok_or_else(|| {
            AppError::Invalid(format!("invalid target '{s}', expected 'repo:<id>' or 'agent:<name>'"))
        })?;
        let owner = owner.trim().to_string();
        memhub_store::validate_name("target owner", &owner)?;
        match kind.trim() {
            "repo" => Ok(Target::Repo(owner)),
            "agent" => Ok(Target::Agent(owner)),
            other => Err(AppError::Invalid(format!(
                "invalid target kind '{other}', expected 'repo' or 'agent'"
            ))),
        }
    }
}

impl Serialize for Target {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Watermark {
    pub at: DateTime<Utc>,
    pub id: String,
}

impl Watermark {
    /// `"<rfc3339>|<evidence id>"`, the form handed to the curator as a cursor.
    pub fn cursor(&self) -> String {
        format!("{}|{}", self.at.to_rfc3339(), self.id)
    }

    pub fn parse_cursor(cursor: &str) -> Result<Self> {
        let (at, id) = cursor
            .split_once('|')
            .ok_or_else(|| AppError::Invalid(format!("invalid cursor '{cursor}'")))?;
        Ok(Self {
            at: at
                .parse()
                .map_err(|_| AppError::Invalid(format!("invalid cursor time in '{cursor}'")))?,
            id: id.to_string(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    watermarks: BTreeMap<String, Watermark>,
    #[serde(default)]
    last_run: BTreeMap<String, DateTime<Utc>>,
}

/// An experience memory related to a piece of evidence.
#[derive(Debug, Clone, Serialize)]
pub struct Related {
    pub id: MemoryId,
    pub title: String,
    pub kind: String,
    pub status: Option<Status>,
    pub version: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchEvidence {
    #[serde(flatten)]
    pub evidence: Evidence,
    /// Experience memories of the target most related to this evidence
    /// (including those it was derived from).
    pub related: Vec<Related>,
}

/// Searches that found nothing, grouped by query text.
#[derive(Debug, Clone, Serialize)]
pub struct MissedQuery {
    pub query: String,
    pub count: usize,
    pub agents: Vec<String>,
    pub last_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedbackItem {
    pub id: MemoryId,
    pub title: String,
    pub status: Option<Status>,
    pub version: u64,
    pub feedback: Feedback,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurationBatch {
    pub target: Target,
    /// Repo description or agent role.
    pub description: String,
    /// The target's folder tree (paths and descriptions).
    pub folders: Vec<FolderInfo>,
    /// Experience memories the target holds (superseded excluded).
    pub experience_count: usize,
    pub evidence: Vec<BatchEvidence>,
    /// More evidence is waiting after this batch.
    pub more: bool,
    /// Pass to [`AgentSession::propose_changes`] so the watermark advances
    /// past this batch. `None` when the batch has no evidence.
    pub cursor: Option<String>,
    pub missed_queries: Vec<MissedQuery>,
    pub feedback: Vec<FeedbackItem>,
}

impl CurationBatch {
    pub fn is_empty(&self) -> bool {
        self.evidence.is_empty() && self.missed_queries.is_empty() && self.feedback.is_empty()
    }
}

/// A memory ported from 1.0 and not reviewed by the curator yet.
#[derive(Debug, Clone, Serialize)]
pub struct BackfillItem {
    #[serde(flatten)]
    pub memory: Memory,
    pub read_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackfillBatch {
    pub target: Target,
    pub description: String,
    pub folders: Vec<FolderInfo>,
    pub memories: Vec<BackfillItem>,
    pub remaining: usize,
}

/// One itemized change proposed by the curator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CurationOp {
    /// A new experience memory. Must cite at least one independent evidence.
    Add {
        scope: Scope,
        owner: String,
        #[serde(default)]
        folder: String,
        title: String,
        #[serde(default)]
        kind: Option<String>,
        body: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        applies_to: Vec<String>,
        #[serde(default)]
        cites: Cites,
        /// Requested status; downgraded to `provisional` unless justified.
        #[serde(default)]
        status: Option<Status>,
        /// Commit at which the code citations were checked.
        #[serde(default)]
        verified_commit: Option<String>,
        #[serde(default)]
        slug: Option<String>,
    },
    /// Changes an existing memory. A body change must keep or add citations.
    Update {
        id: String,
        expected_version: u64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        applies_to: Option<Vec<String>>,
        #[serde(default)]
        add_cites: Cites,
        #[serde(default)]
        status: Option<Status>,
        #[serde(default)]
        verified_commit: Option<String>,
    },
    /// Replaces a memory by a newer one: an existing id, or `#<n>` for the
    /// add operation at index n of this proposal.
    Supersede { id: String, by: String, reason: String },
    /// Marks a memory as no longer true, without a replacement.
    Retire { id: String, reason: String },
    Move { id: String, folder: String },
    DescribeFolder {
        scope: Scope,
        owner: String,
        folder: String,
        description: String,
    },
    /// Evidence considered and deliberately not turned into experience.
    Noop {
        #[serde(default)]
        evidence: Vec<String>,
        #[serde(default)]
        reason: String,
    },
}

impl CurationOp {
    pub fn name(&self) -> &'static str {
        match self {
            CurationOp::Add { .. } => "add",
            CurationOp::Update { .. } => "update",
            CurationOp::Supersede { .. } => "supersede",
            CurationOp::Retire { .. } => "retire",
            CurationOp::Move { .. } => "move",
            CurationOp::DescribeFolder { .. } => "describe_folder",
            CurationOp::Noop { .. } => "noop",
        }
    }
}

/// Result of the deterministic checks of one operation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpCheck {
    pub index: usize,
    pub op: String,
    /// Memory the operation writes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub ok: bool,
    /// Allowed, but only a human may approve it (large rewrite, retiring an
    /// established memory).
    #[serde(default)]
    pub needs_human: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Independent sources among the cited evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<u32>,
    /// Status the memory will get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Applied,
    Rejected,
    /// Some operations approved, some rejected.
    Partial,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub index: usize,
    pub approved: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub by: String,
    pub at: DateTime<Utc>,
    /// Memory written when the operation was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_id: Option<String>,
    /// Why an approved operation could not be applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub target: Target,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub ops: Vec<CurationOp>,
    pub checks: Vec<OpCheck>,
    pub state: ProposalState,
    #[serde(default)]
    pub decisions: Vec<Decision>,
}

impl Proposal {
    pub fn decision(&self, index: usize) -> Option<&Decision> {
        self.decisions.iter().find(|d| d.index == index)
    }

    /// Operations without a decision yet.
    pub fn undecided(&self) -> Vec<usize> {
        (0..self.ops.len()).filter(|i| self.decision(*i).is_none()).collect()
    }
}

/// A reviewer's verdict on one operation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Verdict {
    pub index: usize,
    pub approve: bool,
    #[serde(default)]
    pub reason: String,
}

/// Overview of one target for the curator's run loop.
#[derive(Debug, Clone, Serialize)]
pub struct TargetStatus {
    pub target: Target,
    pub pending_evidence: usize,
    pub backfill: usize,
    pub open_proposals: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
}

/// One retrieval check: `query` should find one of `expect` in the top k.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvalCase {
    pub query: String,
    #[serde(default)]
    pub repo: Option<String>,
    pub expect: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalResult {
    pub query: String,
    /// 1-based rank of the first expected memory, if found in the top k.
    pub rank: Option<usize>,
    pub top: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub k: usize,
    pub cases: Vec<EvalResult>,
    /// Share of cases whose expected memory is in the top k.
    pub recall: f64,
}

fn is_page_kind(kind: &str) -> bool {
    PAGE_KINDS.iter().any(|k| k.eq_ignore_ascii_case(kind))
}

fn body_limit(kind: &str) -> usize {
    if is_page_kind(kind) {
        MAX_PAGE_CHARS
    } else {
        MAX_ENTRY_CHARS
    }
}

fn new_proposal_id(target: &Target) -> String {
    let tag: String = target
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .take(40)
        .collect();
    format!(
        "{}-{tag}-{}",
        Utc::now().format("%Y%m%dT%H%M%S"),
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    )
}

impl AgentSession {
    fn require_curator(&self) -> Result<String> {
        let me = self.actor_name()?;
        if !self.is_curator() {
            return Err(AppError::Forbidden(
                "curation tools need a curator session (connect with the curator token)".into(),
            ));
        }
        Ok(me)
    }

    fn curation_state(&self) -> State {
        load_json(&self.hub().curation_dir().join("state.json"))
    }

    fn save_curation_state(&self, state: &State) -> Result<()> {
        save_json(&self.hub().curation_dir().join("state.json"), state)
            .map_err(|e| AppError::Internal(format!("cannot save curation state: {e}")))
    }

    fn target_description(&self, target: &Target) -> Result<String> {
        let registry = self.hub().registry();
        Ok(match target {
            Target::Repo(r) => registry
                .repo(r)?
                .ok_or_else(|| AppError::UnknownRepo(r.clone()))?
                .description,
            Target::Agent(a) => registry
                .agent(a)?
                .ok_or_else(|| AppError::UnknownAgent(a.clone()))?
                .role,
        })
    }

    fn target_evidence_query(&self, target: &Target, after: Option<&Watermark>) -> EvidenceQuery {
        EvidenceQuery {
            since: after.map(|w| w.at),
            author: match target {
                Target::Agent(a) => Some(a.clone()),
                Target::Repo(_) => None,
            },
            repo: match target {
                Target::Repo(r) => Some(r.clone()),
                Target::Agent(_) => None,
            },
            after: after.map(|w| (w.at, w.id.clone())),
            ..Default::default()
        }
    }

    fn pending_evidence(&self, target: &Target, after: Option<&Watermark>) -> Result<Vec<Evidence>> {
        Ok(self
            .hub()
            .evidence()
            .list(&self.target_evidence_query(target, after))?
            .into_iter()
            .filter(|e| !is_internal_agent(&e.meta.author))
            .collect())
    }

    fn target_memories(&self, target: &Target) -> Result<Vec<Memory>> {
        Ok(self
            .hub()
            .store()
            .list(&Filter {
                scope: Some(target.scope()),
                owner: Some(target.owner().to_string()),
                ..Default::default()
            })?
            .into_iter()
            .filter(|m| !m.meta.is_work_note())
            .collect())
    }

    /// Every repo and agent with something to curate, plus the rest.
    pub fn curation_status(&self) -> Result<Vec<TargetStatus>> {
        self.require_curator()?;
        let state = self.curation_state();
        let proposals = self.list_proposals(Some(ProposalState::Pending))?;
        let mut targets: Vec<Target> = self
            .hub()
            .registry()
            .repos()?
            .into_iter()
            .map(|r| Target::Repo(r.id))
            .collect();
        targets.extend(
            self.hub()
                .registry()
                .agents()?
                .into_iter()
                .filter(|a| !is_internal_agent(&a.name))
                .map(|a| Target::Agent(a.name)),
        );
        let mut out = Vec::new();
        for target in targets {
            let key = target.to_string();
            let pending = self.pending_evidence(&target, state.watermarks.get(&key))?.len();
            let backfill = self
                .target_memories(&target)?
                .iter()
                .filter(|m| m.meta.experience.origin.is_some() && m.meta.experience.curated_at.is_none())
                .count();
            out.push(TargetStatus {
                open_proposals: proposals.iter().filter(|p| p.target == target).count(),
                last_run: state.last_run.get(&key).copied(),
                target,
                pending_evidence: pending,
                backfill,
            });
        }
        Ok(out)
    }

    /// The next batch of work for a target, see the module docs.
    pub fn curation_batch(&self, target: &Target, limit: Option<usize>) -> Result<CurationBatch> {
        self.require_curator()?;
        let description = self.target_description(target)?;
        let state = self.curation_state();
        let key = target.to_string();
        let limit = limit.unwrap_or(DEFAULT_BATCH_SIZE).clamp(1, 200);
        let mut pending = self.pending_evidence(target, state.watermarks.get(&key))?;
        let more = pending.len() > limit;
        pending.truncate(limit);
        let cursor = pending.last().map(|e| {
            Watermark {
                at: e.meta.created_at,
                id: e.meta.id.to_string(),
            }
            .cursor()
        });

        let store = self.hub().store();
        let evidence = pending
            .into_iter()
            .map(|e| {
                let related = self.related(target, &e);
                BatchEvidence {
                    evidence: e,
                    related,
                }
            })
            .collect();

        let since = state
            .last_run
            .get(&key)
            .copied()
            .unwrap_or_else(|| Utc::now() - Duration::days(FIRST_RUN_LOOKBACK_DAYS));
        let memories = self.target_memories(target)?;
        let all_feedback = self.hub().feedback().all();
        let feedback = memories
            .iter()
            .filter_map(|m| {
                let fb = all_feedback.get(&m.meta.id.to_string()).cloned().unwrap_or_default();
                let disputed = m.meta.status() == Some(Status::Disputed);
                let negative = fb.harmful + fb.outdated + fb.disputes > 0
                    && fb.last_at.is_some_and(|t| t >= since);
                (disputed || negative).then(|| FeedbackItem {
                    id: m.meta.id.clone(),
                    title: m.meta.title.clone(),
                    status: m.meta.status(),
                    version: m.meta.version,
                    feedback: fb,
                })
            })
            .take(20)
            .collect();

        Ok(CurationBatch {
            target: target.clone(),
            description,
            folders: store.folders(target.scope(), target.owner()).unwrap_or_default(),
            experience_count: memories.iter().filter(|m| !m.meta.experience.is_superseded()).count(),
            evidence,
            more,
            cursor,
            missed_queries: self.missed_queries(target, since)?,
            feedback,
        })
    }

    fn related(&self, target: &Target, evidence: &Evidence) -> Vec<Related> {
        let m = &evidence.meta;
        let mut text = m.title.clone();
        for extra in [&m.candidate_lesson, &m.task].into_iter().flatten() {
            text.push(' ');
            text.push_str(extra);
        }
        let words: Vec<&str> = text
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|w| w.chars().count() > 3)
            .take(12)
            .collect();
        let mut out: Vec<Related> = Vec::new();
        let mut push = |memory: &Memory, snippet: String| {
            if !out.iter().any(|r| r.id == memory.meta.id) {
                out.push(Related {
                    id: memory.meta.id.clone(),
                    title: memory.meta.title.clone(),
                    kind: memory.meta.kind.clone(),
                    status: memory.meta.status(),
                    version: memory.meta.version,
                    snippet,
                });
            }
        };
        for d in &m.derived_from {
            if let Ok(memory) = d.parse::<MemoryId>().and_then(|id| self.hub().store().get(&id)) {
                let snippet = memory.body.chars().take(200).collect();
                push(&memory, snippet);
            }
        }
        // Any-term matching: score() ranks multi-term matches first.
        let mut scored: HashMap<MemoryId, (f64, Memory, String)> = HashMap::new();
        for word in words {
            let Ok(hits) = self.hub().store().search(&SearchQuery {
                text: Some(word.to_string()),
                filter: Filter {
                    scope: Some(target.scope()),
                    owner: Some(target.owner().to_string()),
                    ..Default::default()
                },
                limit: Some(10),
            }) else {
                continue;
            };
            for hit in hits {
                if hit.meta.is_work_note() || hit.meta.experience.is_superseded() {
                    continue;
                }
                let entry = scored.entry(hit.meta.id.clone()).or_insert_with(|| {
                    let memory = self.hub().store().get(&hit.meta.id).ok();
                    (0.0, memory.unwrap_or(Memory { meta: hit.meta.clone(), body: String::new() }), hit.snippet.clone())
                });
                entry.0 += hit.score;
            }
        }
        let mut ranked: Vec<(f64, Memory, String)> = scored.into_values().collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.meta.id.cmp(&b.1.meta.id)));
        for (_, memory, snippet) in ranked.into_iter().take(RELATED_PER_EVIDENCE) {
            push(&memory, snippet);
        }
        out
    }

    fn missed_queries(&self, target: &Target, since: DateTime<Utc>) -> Result<Vec<MissedQuery>> {
        let records = self
            .hub()
            .audit()
            .read(&LogQuery {
                event: Some("memory_searched".into()),
                since: Some(since),
                ..Default::default()
            })
            .map_err(|e| AppError::Internal(format!("cannot read audit log: {e}")))?;
        let members: BTreeSet<String> = match target {
            Target::Repo(r) => self
                .hub()
                .registry()
                .agents()?
                .into_iter()
                .filter(|a| a.works_on(r))
                .map(|a| a.name)
                .collect(),
            Target::Agent(a) => [a.clone()].into(),
        };
        let mut groups: BTreeMap<String, MissedQuery> = BTreeMap::new();
        for r in records {
            let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("");
            if s("layer") != "experience" || r.get("hits").and_then(|v| v.as_u64()) != Some(0) {
                continue;
            }
            let agent = s("agent").to_string();
            if is_internal_agent(&agent) {
                continue;
            }
            let relevant = match target {
                Target::Repo(repo) => {
                    s("owner") == repo || (s("owner").is_empty() && members.contains(&agent))
                }
                Target::Agent(a) => agent == *a,
            };
            if !relevant {
                continue;
            }
            let query = s("query").trim().to_lowercase();
            if query.is_empty() {
                continue;
            }
            let at = s("ts").parse().unwrap_or_else(|_| Utc::now());
            let entry = groups.entry(query.clone()).or_insert_with(|| MissedQuery {
                query,
                count: 0,
                agents: Vec::new(),
                last_at: at,
            });
            entry.count += 1;
            entry.last_at = entry.last_at.max(at);
            if !entry.agents.contains(&agent) {
                entry.agents.push(agent);
            }
        }
        let mut out: Vec<MissedQuery> = groups.into_values().collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| b.last_at.cmp(&a.last_at)));
        out.truncate(20);
        Ok(out)
    }

    /// Memories ported from 1.0 that the curator has not reviewed yet.
    pub fn curation_backfill(&self, target: &Target, limit: Option<usize>) -> Result<BackfillBatch> {
        self.require_curator()?;
        let description = self.target_description(target)?;
        let mut pending: Vec<Memory> = self
            .target_memories(target)?
            .into_iter()
            .filter(|m| m.meta.experience.origin.is_some() && m.meta.experience.curated_at.is_none())
            .collect();
        pending.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
        let limit = limit.unwrap_or(10).clamp(1, 100);
        let remaining = pending.len().saturating_sub(limit);
        pending.truncate(limit);
        Ok(BackfillBatch {
            target: target.clone(),
            description,
            folders: self.hub().store().folders(target.scope(), target.owner()).unwrap_or_default(),
            memories: pending
                .into_iter()
                .map(|memory| BackfillItem {
                    read_count: self.hub().reads().get(&memory.meta.id).count,
                    memory,
                })
                .collect(),
            remaining,
        })
    }

    // ---- proposals ------------------------------------------------------------

    /// Checks and stores a proposal; advances the target's watermark to
    /// `cursor`. Nothing is applied until [`Self::review_changes`].
    pub fn propose_changes(
        &self,
        target: &Target,
        ops: Vec<CurationOp>,
        cursor: Option<&str>,
        note: Option<&str>,
    ) -> Result<Proposal> {
        let me = self.require_curator()?;
        self.target_description(target)?;
        if ops.len() > MAX_OPS {
            return Err(AppError::Invalid(format!(
                "at most {MAX_OPS} operations per proposal; split the batch"
            )));
        }
        let watermark = cursor
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(Watermark::parse_cursor)
            .transpose()?;
        let checks = self.check_ops(target, &ops);
        let proposal = Proposal {
            id: new_proposal_id(target),
            target: target.clone(),
            created_at: Utc::now(),
            created_by: me,
            note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            cursor: watermark.as_ref().map(Watermark::cursor),
            state: if ops.is_empty() {
                ProposalState::Applied
            } else {
                ProposalState::Pending
            },
            ops,
            checks,
            decisions: Vec::new(),
        };
        self.save_proposal(&proposal)?;

        let mut state = self.curation_state();
        let key = target.to_string();
        if let Some(w) = watermark {
            let advance = state.watermarks.get(&key).is_none_or(|cur| w > *cur);
            if advance {
                state.watermarks.insert(key.clone(), w);
            }
        }
        state.last_run.insert(key, Utc::now());
        self.save_curation_state(&state)?;

        self.hub().audit().record(AuditEvent::CurationProposed {
            actor: self.actor(),
            proposal_id: proposal.id.clone(),
            target: proposal.target.to_string(),
            ops: proposal.ops.len(),
            valid: proposal.checks.iter().filter(|c| c.ok).count(),
            needs_human: proposal.checks.iter().filter(|c| c.ok && c.needs_human).count(),
        });
        Ok(proposal)
    }

    /// Runs the checks of [`Self::propose_changes`] without storing anything
    /// or moving the watermark.
    pub fn check_changes(&self, target: &Target, ops: &[CurationOp]) -> Result<Vec<OpCheck>> {
        self.require_curator()?;
        self.target_description(target)?;
        Ok(self.check_ops(target, ops))
    }

    pub fn get_proposal(&self, id: &str) -> Result<Proposal> {
        self.require_curator()?;
        self.load_proposal(id)
    }

    pub fn list_proposals(&self, state: Option<ProposalState>) -> Result<Vec<Proposal>> {
        self.require_curator()?;
        let dir = self.hub().curation_dir().join("proposals");
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(AppError::Internal(format!("{}: {e}", dir.display()))),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path).map(|t| serde_json::from_str::<Proposal>(&t)) {
                Ok(Ok(p)) if state.is_none_or(|s| s == p.state) => out.push(p),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("skipping malformed proposal {}: {e}", path.display()),
                Err(e) => tracing::warn!("cannot read {}: {e}", path.display()),
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    fn load_proposal(&self, id: &str) -> Result<Proposal> {
        memhub_store::validate_name("proposal id", id)?;
        let path = self.hub().curation_dir().join("proposals").join(format!("{id}.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AppError::Store(StoreError::NotFound(format!("proposal {id}"))),
            _ => AppError::Internal(format!("{}: {e}", path.display())),
        })?;
        serde_json::from_str(&text).map_err(|e| AppError::Internal(format!("malformed proposal {id}: {e}")))
    }

    fn save_proposal(&self, proposal: &Proposal) -> Result<()> {
        let path = self
            .hub()
            .curation_dir()
            .join("proposals")
            .join(format!("{}.json", proposal.id));
        save_json(&path, proposal).map_err(|e| AppError::Internal(format!("cannot save proposal: {e}")))
    }

    /// Applies approved operations of a proposal and records rejections.
    /// Operations a human must approve stay undecided unless `human` is set.
    /// Operations without a verdict stay undecided.
    pub fn review_changes(&self, proposal_id: &str, verdicts: &[Verdict], human: bool) -> Result<Proposal> {
        let me = self.require_curator()?;
        let _guard = self.hub().curation_lock();
        let mut proposal = self.load_proposal(proposal_id)?;
        let mut by_index: BTreeMap<usize, &Verdict> = BTreeMap::new();
        for v in verdicts {
            if v.index >= proposal.ops.len() {
                return Err(AppError::Invalid(format!(
                    "proposal {proposal_id} has no operation {}",
                    v.index
                )));
            }
            by_index.insert(v.index, v);
        }
        // Ids written by earlier add operations, for `#<n>` references.
        let mut added: HashMap<usize, String> = proposal
            .decisions
            .iter()
            .filter_map(|d| Some((d.index, d.applied_id.clone()?)))
            .collect();
        let mut applied_now = Vec::new();
        let mut approved_now = 0;
        let mut rejected_now = 0;
        for (index, verdict) in by_index {
            if proposal.decision(index).is_some() {
                continue;
            }
            let check = &proposal.checks[index];
            let mut decision = Decision {
                index,
                approved: verdict.approve,
                reason: verdict.reason.trim().to_string(),
                by: me.clone(),
                at: Utc::now(),
                applied_id: None,
                error: None,
            };
            if verdict.approve {
                if check.needs_human && !human {
                    continue;
                }
                let recheck = self.check_op(&proposal.target, index, &proposal.ops, &added);
                if !recheck.ok {
                    decision.approved = false;
                    decision.error = Some(recheck.errors.join("; "));
                } else {
                    match self.apply_op(&proposal.ops[index], &recheck, &added) {
                        Ok(id) => {
                            if let Some(id) = &id {
                                applied_now.push(id.clone());
                                added.insert(index, id.clone());
                            }
                            decision.applied_id = id;
                        }
                        Err(e) => {
                            decision.approved = false;
                            decision.error = Some(e.to_string());
                        }
                    }
                }
            }
            if decision.approved {
                approved_now += 1;
            } else {
                rejected_now += 1;
            }
            proposal.decisions.push(decision);
        }
        proposal.decisions.sort_by_key(|d| d.index);
        if proposal.undecided().is_empty() {
            let approved = proposal.decisions.iter().filter(|d| d.approved).count();
            proposal.state = match approved {
                0 => ProposalState::Rejected,
                n if n == proposal.ops.len() => ProposalState::Applied,
                _ => ProposalState::Partial,
            };
        }
        self.save_proposal(&proposal)?;
        self.hub().audit().record(AuditEvent::CurationReviewed {
            actor: self.actor(),
            proposal_id: proposal.id.clone(),
            target: proposal.target.to_string(),
            approved: approved_now,
            rejected: rejected_now,
            applied: applied_now,
        });
        Ok(proposal)
    }

    // ---- checks -----------------------------------------------------------------

    fn check_ops(&self, target: &Target, ops: &[CurationOp]) -> Vec<OpCheck> {
        let none = HashMap::new();
        (0..ops.len()).map(|i| self.check_op(target, i, ops, &none)).collect()
    }

    /// Checks operation `index`. `added` maps indexes of already applied add
    /// operations to the ids they created.
    fn check_op(
        &self,
        target: &Target,
        index: usize,
        ops: &[CurationOp],
        added: &HashMap<usize, String>,
    ) -> OpCheck {
        let op = &ops[index];
        let mut c = OpCheck {
            index,
            op: op.name().to_string(),
            ..Default::default()
        };
        if let Err(e) = self.check_op_inner(target, index, ops, added, &mut c) {
            c.errors.push(e.to_string());
        }
        c.ok = c.errors.is_empty();
        c
    }

    fn check_op_inner(
        &self,
        target: &Target,
        index: usize,
        ops: &[CurationOp],
        added: &HashMap<usize, String>,
        c: &mut OpCheck,
    ) -> Result<()> {
        match &ops[index] {
            CurationOp::Add {
                scope,
                owner,
                folder,
                title,
                kind,
                body,
                cites,
                status,
                verified_commit,
                applies_to,
                ..
            } => {
                self.check_owner(target, *scope, owner, c)?;
                let folder = normalize_folder(folder)?;
                if *scope == Scope::Agent && is_under(&folder, WORK_FOLDER) {
                    c.errors.push("experience cannot be written into an agent's work area".into());
                }
                memhub_store::validate_title(title.trim())?;
                let kind = kind.as_deref().map(str::trim).filter(|k| !k.is_empty()).unwrap_or("note");
                self.check_kind_and_body(kind, body, c);
                self.check_applies_to(applies_to, c);
                let support = self.check_cites(cites, c)?;
                if cites.evidence.is_empty() {
                    c.errors.push("an added memory must cite at least one evidence id (cites.evidence)".into());
                } else if support == 0 {
                    c.errors.push(
                        "every cited evidence is memory-derived (source from_memory); cite at least one \
                         first-hand observation"
                            .into(),
                    );
                }
                c.support = Some(support);
                c.status = Some(self.effective_status(*status, support, cites, verified_commit.as_deref(), c));
                c.target = Some(format!("{scope}/{owner}/{}", if folder.is_empty() { String::new() } else { format!("{folder}/") }));
            }
            CurationOp::Update {
                id,
                expected_version,
                kind,
                body,
                add_cites,
                status,
                verified_commit,
                applies_to,
                title,
                ..
            } => {
                let memory = self.check_existing(target, id, c)?;
                if memory.meta.version != *expected_version {
                    c.errors.push(format!(
                        "{id} is at version {}, not {expected_version}; fetch it again and rebase the change",
                        memory.meta.version
                    ));
                }
                if let Some(t) = title {
                    memhub_store::validate_title(t.trim())?;
                }
                let kind = kind.as_deref().unwrap_or(&memory.meta.kind);
                if let Some(b) = body {
                    self.check_kind_and_body(kind, b, c);
                    let old = memory.body.chars().count();
                    let new = b.chars().count();
                    if old >= SHRINK_GUARD_MIN_CHARS && (new as f64) < old as f64 * (1.0 - MAX_SHRINK) {
                        c.needs_human = true;
                        c.warnings.push(format!(
                            "body shrinks from {old} to {new} characters; a human must approve large \
                             rewrites. Prefer supersede (new entry) or a targeted edit"
                        ));
                    }
                }
                if let Some(a) = applies_to {
                    self.check_applies_to(a, c);
                }
                let mut merged = memory.meta.experience.cites.clone();
                merged.merge(add_cites);
                let support = self.check_cites(&merged, c)?;
                if body.is_some() && merged.evidence.is_empty() {
                    c.errors.push("a body change must be backed by evidence (add_cites.evidence)".into());
                }
                c.support = Some(support);
                let requested = status.or(memory.meta.status()).or(Some(Status::Provisional));
                if requested == Some(Status::Superseded) {
                    c.errors.push("use supersede or retire to end a memory".into());
                }
                c.status = Some(self.effective_status(requested, support, &merged, verified_commit.as_deref(), c));
                c.target = Some(memory.meta.id.to_string());
            }
            CurationOp::Supersede { id, by, reason } => {
                let memory = self.check_existing(target, id, c)?;
                if memory.meta.experience.is_superseded() {
                    c.errors.push(format!("{id} is already superseded"));
                }
                if reason.trim().is_empty() {
                    c.errors.push("'reason' is required".into());
                }
                if let Some(n) = by.trim().strip_prefix('#') {
                    let n: usize = n
                        .parse()
                        .map_err(|_| AppError::Invalid(format!("invalid reference '{by}'")))?;
                    if n >= index || !matches!(ops.get(n), Some(CurationOp::Add { .. })) {
                        c.errors.push(format!(
                            "'{by}' must refer to an earlier add operation of this proposal"
                        ));
                    } else if !added.is_empty() && !added.contains_key(&n) {
                        c.errors.push(format!("add operation #{n} was not applied"));
                    }
                } else {
                    let by_id: MemoryId = by.parse()?;
                    let replacement = self.hub().store().get(&by_id)?;
                    if replacement.meta.id == memory.meta.id {
                        c.errors.push("a memory cannot supersede itself".into());
                    }
                    if replacement.meta.experience.is_superseded() {
                        c.errors.push(format!("replacement {by} is itself superseded"));
                    }
                }
                c.status = Some(Status::Superseded);
                c.target = Some(memory.meta.id.to_string());
            }
            CurationOp::Retire { id, reason } => {
                let memory = self.check_existing(target, id, c)?;
                if memory.meta.experience.is_superseded() {
                    c.errors.push(format!("{id} is already superseded"));
                }
                if reason.trim().is_empty() {
                    c.errors.push("'reason' is required".into());
                }
                if memory.meta.status() == Some(Status::Established) {
                    c.needs_human = true;
                    c.warnings.push("retiring an established memory needs a human".into());
                }
                c.status = Some(Status::Superseded);
                c.target = Some(memory.meta.id.to_string());
            }
            CurationOp::Move { id, folder } => {
                let memory = self.check_existing(target, id, c)?;
                let folder = normalize_folder(folder)?;
                if memory.meta.scope() == Scope::Agent && is_under(&folder, WORK_FOLDER) {
                    c.errors.push("experience cannot move into an agent's work area".into());
                }
                c.target = Some(memory.meta.id.to_string());
            }
            CurationOp::DescribeFolder {
                scope,
                owner,
                folder,
                description,
            } => {
                self.check_owner(target, *scope, owner, c)?;
                let folder = normalize_folder(folder)?;
                if folder.is_empty() {
                    c.errors.push("the root folder has no description; describe a subfolder".into());
                }
                if *scope == Scope::Agent && is_under(&folder, WORK_FOLDER) {
                    c.errors.push("the work area belongs to the agent".into());
                }
                if description.chars().count() > 2000 {
                    c.errors.push("description must be at most 2000 characters".into());
                }
            }
            CurationOp::Noop { evidence, .. } => {
                for e in evidence {
                    match e.parse::<EvidenceId>() {
                        Ok(id) if self.hub().evidence().exists(&id) => {}
                        _ => c.warnings.push(format!("unknown evidence '{e}'")),
                    }
                }
            }
        }
        Ok(())
    }

    fn check_owner(&self, target: &Target, scope: Scope, owner: &str, c: &mut OpCheck) -> Result<()> {
        let allowed = match (target, scope) {
            (Target::Repo(r), Scope::Repo) => r == owner,
            (Target::Agent(a), Scope::Agent) => a == owner,
            // Promoting an agent's lesson into a repo it works on.
            (Target::Agent(a), Scope::Repo) => self
                .hub()
                .registry()
                .agent(a)?
                .is_some_and(|p| p.works_on(owner)),
            (Target::Repo(_), Scope::Agent) => false,
        };
        if !allowed {
            c.errors.push(format!(
                "{scope}/{owner} is outside target {target} (agent targets may also write repos the agent works on)"
            ));
        }
        if scope == Scope::Repo && self.hub().registry().repo(owner)?.is_none() {
            c.errors.push(format!("unknown repo '{owner}'"));
        }
        Ok(())
    }

    fn check_existing(&self, target: &Target, id: &str, c: &mut OpCheck) -> Result<Memory> {
        let id: MemoryId = id.parse()?;
        self.check_owner(target, id.scope, &id.owner, c)?;
        if id.is_work_note() {
            c.errors.push("work-area notes belong to their agent; the curator does not edit them".into());
        }
        Ok(self.hub().store().get(&id)?)
    }

    fn check_kind_and_body(&self, kind: &str, body: &str, c: &mut OpCheck) {
        if NON_EXPERIENCE_KINDS.iter().any(|k| k.eq_ignore_ascii_case(kind)) {
            c.errors.push(format!(
                "kind '{kind}' is task state, not experience; it stays in evidence or the agent's work area"
            ));
        }
        let limit = body_limit(kind);
        let len = body.chars().count();
        if body.trim().is_empty() {
            c.errors.push("body must not be empty".into());
        } else if len > limit {
            c.errors.push(format!(
                "body has {len} characters; kind '{kind}' allows {limit}. Split it into atomic entries"
            ));
        }
    }

    fn check_applies_to(&self, applies_to: &[String], c: &mut OpCheck) {
        for a in applies_to {
            if a.trim().is_empty() || a.chars().count() > 200 || a.contains('\n') {
                c.errors.push(format!("invalid applies_to entry '{a}'"));
            }
        }
    }

    /// Validates citations and returns the number of independent sources.
    fn check_cites(&self, cites: &Cites, c: &mut OpCheck) -> Result<u32> {
        let mut keys = BTreeSet::new();
        for e in &cites.evidence {
            let id: EvidenceId = match e.parse() {
                Ok(id) => id,
                Err(err) => {
                    c.errors.push(err.to_string());
                    continue;
                }
            };
            match self.hub().evidence().get(&id) {
                Ok(evidence) => {
                    if let Some(k) = evidence.source_key() {
                        keys.insert(k);
                    }
                }
                Err(_) => c.errors.push(format!("cited evidence '{e}' does not exist")),
            }
        }
        for r in &cites.code {
            if let Err(e) = validate_code_ref(r) {
                c.errors.push(e.to_string());
            }
        }
        Ok(keys.len() as u32)
    }

    fn effective_status(
        &self,
        requested: Option<Status>,
        support: u32,
        cites: &Cites,
        verified_commit: Option<&str>,
        c: &mut OpCheck,
    ) -> Status {
        match requested.unwrap_or(Status::Provisional) {
            Status::Established => {
                let verified = verified_commit.is_some_and(|v| !v.trim().is_empty()) && !cites.code.is_empty();
                if support >= 2 || verified {
                    Status::Established
                } else {
                    c.warnings.push(format!(
                        "'established' needs two independent sources or code citations verified at a \
                         commit (verified_commit); has {support} source(s). Kept provisional"
                    ));
                    Status::Provisional
                }
            }
            Status::Superseded => Status::Superseded,
            other => other,
        }
    }

    // ---- apply ------------------------------------------------------------------

    fn apply_op(&self, op: &CurationOp, check: &OpCheck, added: &HashMap<usize, String>) -> Result<Option<String>> {
        let me = self.actor_name()?;
        let now = Utc::now();
        let verified = |commit: &Option<String>| {
            commit.as_ref().map(|c| c.trim()).filter(|c| !c.is_empty()).map(|c| Verified {
                at: now,
                commit: Some(c.to_string()),
                by: me.clone(),
            })
        };
        match op {
            CurationOp::Add {
                scope,
                owner,
                folder,
                title,
                kind,
                body,
                tags,
                applies_to,
                cites,
                verified_commit,
                slug,
                ..
            } => {
                let memory = self.add_memory(AddMemory {
                    scope: *scope,
                    repo: (*scope == Scope::Repo).then(|| owner.clone()),
                    owner: Some(owner.clone()),
                    folder: folder.clone(),
                    title: title.clone(),
                    kind: kind.clone(),
                    tags: tags.clone(),
                    groups: Vec::new(),
                    body: body.clone(),
                    slug: slug.clone().filter(|s| !s.trim().is_empty()),
                    experience: ExperienceMeta {
                        status: check.status,
                        cites: cites.clone(),
                        support: check.support.unwrap_or(0),
                        applies_to: applies_to.clone(),
                        valid_from: Some(now),
                        last_verified: verified(verified_commit),
                        curated_at: Some(now),
                        ..Default::default()
                    },
                })?;
                Ok(Some(memory.meta.id.to_string()))
            }
            CurationOp::Update {
                id,
                expected_version,
                title,
                kind,
                body,
                tags,
                applies_to,
                add_cites,
                verified_commit,
                ..
            } => {
                let id: MemoryId = id.parse()?;
                let current = self.hub().store().get(&id)?;
                let mut experience = current.meta.experience.clone();
                experience.cites.merge(add_cites);
                experience.status = check.status;
                experience.support = check.support.unwrap_or(experience.support);
                if let Some(a) = applies_to {
                    experience.applies_to = a.clone();
                }
                if let Some(v) = verified(verified_commit) {
                    experience.last_verified = Some(v);
                }
                experience.curated_at = Some(now);
                let change = self.update_memory(
                    &id,
                    MemoryPatch {
                        title: title.clone(),
                        kind: kind.clone(),
                        tags: tags.clone(),
                        body: body.clone(),
                        experience: Some(experience),
                        expected_version: Some(*expected_version),
                        ..Default::default()
                    },
                )?;
                Ok(Some(change.after.meta.id.to_string()))
            }
            CurationOp::Supersede { id, by, .. } => {
                let by = match by.trim().strip_prefix('#') {
                    Some(n) => added
                        .get(&n.parse::<usize>().unwrap_or(usize::MAX))
                        .cloned()
                        .ok_or_else(|| AppError::Invalid(format!("add operation {by} was not applied")))?,
                    None => by.trim().to_string(),
                };
                self.end_memory(id, Some(by), now).map(Some)
            }
            CurationOp::Retire { id, .. } => self.end_memory(id, None, now).map(Some),
            CurationOp::Move { id, folder } => {
                let change = self.move_memory(&id.parse()?, folder, None, None)?;
                let moved = change.after;
                if moved.meta.experience.curated_at.is_none() && !moved.meta.is_work_note() {
                    let mut experience = moved.meta.experience.clone();
                    experience.curated_at = Some(now);
                    self.update_memory(
                        &moved.meta.id,
                        MemoryPatch {
                            experience: Some(experience),
                            expected_version: Some(moved.meta.version),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(Some(moved.meta.id.to_string()))
            }
            CurationOp::DescribeFolder {
                scope,
                owner,
                folder,
                description,
            } => {
                self.create_folder(*scope, Some(owner), folder, Some(description))?;
                Ok(None)
            }
            CurationOp::Noop { .. } => Ok(None),
        }
    }

    fn end_memory(&self, id: &str, by: Option<String>, now: DateTime<Utc>) -> Result<String> {
        let id: MemoryId = id.parse()?;
        let current = self.hub().store().get(&id)?;
        let mut experience = current.meta.experience.clone();
        experience.status = Some(Status::Superseded);
        experience.superseded_by = by;
        experience.invalid_at = Some(now);
        experience.curated_at = Some(now);
        let change = self.update_memory(
            &id,
            MemoryPatch {
                experience: Some(experience),
                expected_version: Some(current.meta.version),
                ..Default::default()
            },
        )?;
        Ok(change.after.meta.id.to_string())
    }

    // ---- evaluation ---------------------------------------------------------------

    /// Runs retrieval checks against experience search.
    pub fn evaluate(&self, cases: &[EvalCase], k: usize) -> Result<EvalReport> {
        let k = k.max(1);
        let mut results = Vec::new();
        for case in cases {
            let hits = self.search_experience(
                &SearchQuery {
                    text: Some(case.query.clone()),
                    filter: Filter {
                        scope: case.repo.as_ref().map(|_| Scope::Repo),
                        owner: case.repo.clone(),
                        ..Default::default()
                    },
                    limit: Some(k),
                },
                Default::default(),
            )?;
            let top: Vec<String> = hits.iter().map(|h| h.hit.meta.id.to_string()).collect();
            let rank = top.iter().position(|id| case.expect.contains(id)).map(|p| p + 1);
            results.push(EvalResult {
                query: case.query.clone(),
                rank,
                top,
            });
        }
        let found = results.iter().filter(|r| r.rank.is_some()).count();
        Ok(EvalReport {
            k,
            recall: if results.is_empty() {
                1.0
            } else {
                found as f64 / results.len() as f64
            },
            cases: results,
        })
    }
}
