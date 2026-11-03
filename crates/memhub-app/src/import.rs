//! Porting memories from Memory Hub 1.0 (curator sessions only).
//!
//! Every 1.0 memory becomes a legacy evidence record
//! (`evidence/legacy/<1.0 id>`) holding its original text. Then, by kind and
//! folder:
//!
//! | 1.0 memory                                   | 2.0                                         |
//! |----------------------------------------------|---------------------------------------------|
//! | repo memory                                  | experience, same id, `provisional`, cites the legacy record |
//! | agent `task-logs/`, kind task-log / progress | evidence only (redirect from the 1.0 id)    |
//! | agent `todo/`, kind todo / plan              | work note `agent/<a>/working/todo/...` (redirect) |
//! | other agent memory (insights, tricks)        | agent experience, same id, `provisional`    |
//!
//! Experience ported this way carries `origin: v1` until the curator reviews
//! it (`memhub2 curate --backfill`). Memories whose kind looks misfiled get
//! `curation_hint: reclassify`. Importing the same memory twice is a no-op.

use std::collections::BTreeMap;

use memhub_audit::AuditEvent;
use memhub_store::{
    is_under, normalize_folder, validate_name, Cites, EvidenceId, ExperienceMeta, Memory, MemoryId,
    NewEvidence, Scope, Source, Status, WORK_FOLDER,
};
use serde::Serialize;

use crate::{
    error::{AppError, Result},
    registry::{AgentProfile, RepoProfile},
    session::AgentSession,
};

pub const ORIGIN_V1: &str = "v1";
const HINT_KEY: &str = "curation_hint";

const TASK_STATE_KINDS: &[&str] = &["task-log", "log", "worklog", "progress", "status"];
const PLAN_KINDS: &[&str] = &["todo", "plan"];
const REPO_KINDS: &[&str] = &[
    "overview",
    "architecture",
    "design",
    "implementation",
    "module",
    "how-to",
    "howto",
    "guide",
    "runbook",
    "pitfall",
    "gotcha",
    "decision",
    "adr",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportClass {
    Experience,
    WorkNote,
    EvidenceOnly,
}

impl ImportClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportClass::Experience => "experience",
            ImportClass::WorkNote => "work_note",
            ImportClass::EvidenceOnly => "evidence_only",
        }
    }
}

/// Where a 1.0 memory goes in 2.0.
#[derive(Debug, Clone, Serialize)]
pub struct Placement {
    pub class: ImportClass,
    /// New memory id (`None` for evidence only).
    pub new_id: Option<MemoryId>,
    pub evidence_id: EvidenceId,
    /// e.g. `reclassify` for memories whose kind looks misfiled.
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub source_id: String,
    #[serde(flatten)]
    pub placement: Placement,
    /// What was written; empty on a dry run or when skipped.
    pub written: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RegistryImport {
    pub agents_created: Vec<String>,
    pub agents_updated: Vec<String>,
    pub repos_created: Vec<String>,
}

fn kind_in(kind: &str, list: &[&str]) -> bool {
    list.iter().any(|k| k.eq_ignore_ascii_case(kind.trim()))
}

/// Maps a 1.0 agent folder into the work area: `todo/x` -> `working/todo/x`.
fn work_folder(folder: &str) -> String {
    if folder.is_empty() {
        format!("{WORK_FOLDER}/todo")
    } else {
        format!("{WORK_FOLDER}/{folder}")
    }
}

/// Decides where a 1.0 memory goes, see the module docs.
pub fn place(id: &MemoryId, kind: &str) -> Result<Placement> {
    let evidence_id = EvidenceId::legacy(id);
    let mut hints = Vec::new();
    let (class, new_id) = match id.scope {
        Scope::Repo => {
            if kind_in(kind, TASK_STATE_KINDS) || kind_in(kind, PLAN_KINDS) {
                hints.push("reclassify".to_string());
            }
            (ImportClass::Experience, Some(id.clone()))
        }
        Scope::Agent => {
            let folder = id.folder.as_str();
            if is_under(folder, WORK_FOLDER) {
                (ImportClass::WorkNote, Some(id.clone()))
            } else if is_under(folder, "task-logs") || is_under(folder, "logs") || kind_in(kind, TASK_STATE_KINDS) {
                (ImportClass::EvidenceOnly, None)
            } else if is_under(folder, "todo") || kind_in(kind, PLAN_KINDS) {
                let target = MemoryId::in_folder(Scope::Agent, &id.owner, &work_folder(folder), id.slug.clone())?;
                (ImportClass::WorkNote, Some(target))
            } else {
                if kind_in(kind, REPO_KINDS) {
                    hints.push("reclassify".to_string());
                }
                (ImportClass::Experience, Some(id.clone()))
            }
        }
    };
    Ok(Placement {
        class,
        new_id,
        evidence_id,
        hints,
    })
}

/// A valid evidence author for a 1.0 author name such as `(admin)`.
fn author_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let cleaned = cleaned.trim_start_matches(['-', '_', '.']).to_string();
    if validate_name("agent name", &cleaned).is_ok() {
        cleaned
    } else {
        "unknown".to_string()
    }
}

impl AgentSession {
    fn require_importer(&self) -> Result<()> {
        if !self.is_curator() {
            return Err(AppError::Forbidden(
                "importing needs a curator session (connect with the curator token)".into(),
            ));
        }
        Ok(())
    }

    /// Creates agents and repos that do not exist yet, keeping their 1.0
    /// profiles; adds 1.0 working repos to existing agents.
    pub fn import_registry(&self, agents: Vec<AgentProfile>, repos: Vec<RepoProfile>) -> Result<RegistryImport> {
        self.require_importer()?;
        let registry = self.hub().registry();
        let _guard = registry.lock();
        let mut out = RegistryImport::default();
        for repo in repos {
            validate_name("repo id", &repo.id)?;
            if registry.create_repo(&repo)? {
                out.repos_created.push(repo.id.clone());
                self.hub().store().create_owner(Scope::Repo, &repo.id)?;
            }
        }
        for agent in agents {
            validate_name("agent name", &agent.name)?;
            match registry.agent(&agent.name)? {
                None => {
                    registry.create_agent(&agent)?;
                    out.agents_created.push(agent.name.clone());
                }
                Some(mut existing) => {
                    let before = existing.repos.len();
                    for r in &agent.repos {
                        if !existing.works_on(r) {
                            existing.repos.push(r.clone());
                        }
                    }
                    if existing.repos.len() != before {
                        registry.save_agent(&existing)?;
                        out.agents_updated.push(agent.name.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    /// Recreates 1.0 folders with their descriptions. Agent `todo/` folders
    /// move into the work area; `task-logs/` folders are not recreated (their
    /// memories become evidence). Returns the folders created or described.
    pub fn import_folders(&self, scope: Scope, owner: &str, folders: &[(String, Option<String>)]) -> Result<Vec<String>> {
        self.require_importer()?;
        let store = self.hub().store();
        store.create_owner(scope, owner)?;
        let mut out = Vec::new();
        for (path, description) in folders {
            let path = normalize_folder(path)?;
            if path.is_empty() {
                continue;
            }
            let path = match scope {
                Scope::Agent if is_under(&path, "task-logs") || is_under(&path, "logs") => continue,
                Scope::Agent if is_under(&path, "todo") => work_folder(&path),
                _ => path,
            };
            let description = description.as_deref().map(str::trim).filter(|d| !d.is_empty());
            let existed = store.folder_exists(scope, owner, &path)?;
            let before = store.folder_description(scope, owner, &path)?;
            if !existed || (description.is_some() && before.is_none()) {
                store.create_folder(scope, owner, &path, if before.is_none() { description } else { None })?;
                out.push(path);
            }
        }
        Ok(out)
    }

    /// Ports one 1.0 memory (as returned by 1.0 `get_memory`), see the module
    /// docs. `read_count` is its 1.0 read total. A dry run only reports the
    /// placement.
    pub fn import_memory(&self, memory: Memory, read_count: u64, dry_run: bool) -> Result<ImportOutcome> {
        self.require_importer()?;
        let source_id = memory.meta.id.to_string();
        let placement = place(&memory.meta.id, &memory.meta.kind)?;
        let mut outcome = ImportOutcome {
            source_id: source_id.clone(),
            placement: placement.clone(),
            written: Vec::new(),
            skipped: None,
        };
        let hub = self.hub();
        let evidence_exists = hub.evidence().exists(&placement.evidence_id);
        let target_exists = match &placement.new_id {
            Some(id) => hub.store().exists(id),
            None => evidence_exists,
        };
        if evidence_exists && target_exists {
            outcome.skipped = Some("already imported".into());
            return Ok(outcome);
        }
        if !evidence_exists && target_exists {
            outcome.skipped = Some(format!(
                "{} already exists in 2.0 and was not ported by this tool; left untouched",
                placement.new_id.as_ref().map(|i| i.to_string()).unwrap_or_default()
            ));
            return Ok(outcome);
        }
        if dry_run {
            return Ok(outcome);
        }

        let meta = &memory.meta;
        if !evidence_exists {
            let mut extra = BTreeMap::new();
            let mut put = |k: &str, v: serde_yaml::Value| {
                extra.insert(k.to_string(), v);
            };
            put("v1_id", source_id.clone().into());
            put("v1_kind", meta.kind.clone().into());
            put("v1_version", serde_yaml::Value::from(meta.version));
            put("v1_updated_at", meta.updated_at.to_rfc3339().into());
            if !meta.updated_by.is_empty() {
                put("v1_updated_by", meta.updated_by.clone().into());
            }
            if !meta.groups.is_empty() {
                put("v1_groups", serde_yaml::to_value(&meta.groups).unwrap_or_default());
            }
            put("v1_reads", serde_yaml::Value::from(read_count));
            let title = if memhub_store::validate_title(meta.title.trim()).is_ok() {
                meta.title.trim().to_string()
            } else {
                meta.id.slug.clone()
            };
            let evidence = hub.evidence().add(NewEvidence {
                author: author_name(&meta.author),
                title,
                kind: Some("legacy".into()),
                body: memory.body.clone(),
                repos: match meta.id.scope {
                    Scope::Repo => vec![meta.id.owner.clone()],
                    Scope::Agent => Vec::new(),
                },
                source: Source::Legacy,
                tags: meta.tags.clone(),
                extra,
                created_at: Some(meta.created_at),
                path: Some(placement.evidence_id.path().to_string()),
                ..Default::default()
            })?;
            self.audit_evidence(&evidence);
            outcome.written.push(evidence.meta.id.to_string());
        }

        if let Some(new_id) = &placement.new_id {
            let mut ported = memory.clone();
            ported.meta.id = new_id.clone();
            if placement.class == ImportClass::Experience {
                ported.meta.experience = ExperienceMeta {
                    status: Some(Status::Provisional),
                    cites: Cites {
                        evidence: vec![placement.evidence_id.to_string()],
                        code: Vec::new(),
                    },
                    support: 1,
                    valid_from: Some(meta.created_at),
                    origin: Some(ORIGIN_V1.to_string()),
                    ..Default::default()
                };
                if !placement.hints.is_empty() {
                    ported
                        .meta
                        .extra
                        .insert(HINT_KEY.to_string(), placement.hints.join(",").into());
                }
            } else {
                ported.meta.experience = ExperienceMeta::default();
            }
            hub.store().import(&ported)?;
            hub.reads().import(new_id, read_count);
            outcome.written.push(new_id.to_string());
        }
        let redirect_to = match &placement.new_id {
            Some(id) if *id != meta.id => Some(id.to_string()),
            Some(_) => None,
            None => Some(placement.evidence_id.to_string()),
        };
        if let Some(to) = redirect_to {
            hub.redirects().set(&source_id, &to);
        }
        hub.audit().record(AuditEvent::MemoryImported {
            actor: self.actor(),
            source_id,
            class: placement.class.as_str().to_string(),
            new_id: placement.new_id.as_ref().map(|i| i.to_string()),
            evidence_id: placement.evidence_id.to_string(),
        });
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> MemoryId {
        s.parse().unwrap()
    }

    #[test]
    fn placement_by_scope_kind_and_folder() {
        let p = place(&id("repo/engine/pitfalls/x"), "pitfall").unwrap();
        assert_eq!(p.class, ImportClass::Experience);
        assert_eq!(p.new_id.unwrap().to_string(), "repo/engine/pitfalls/x");
        assert_eq!(p.evidence_id.to_string(), "evidence/legacy/repo/engine/pitfalls/x");
        assert!(p.hints.is_empty());

        let p = place(&id("repo/engine/notes"), "task-log").unwrap();
        assert_eq!(p.class, ImportClass::Experience);
        assert_eq!(p.hints, vec!["reclassify"]);

        let p = place(&id("agent/bot/task-logs/2026/run"), "note").unwrap();
        assert_eq!(p.class, ImportClass::EvidenceOnly);
        assert!(p.new_id.is_none());
        let p = place(&id("agent/bot/progress"), "progress").unwrap();
        assert_eq!(p.class, ImportClass::EvidenceOnly);

        let p = place(&id("agent/bot/todo/next"), "note").unwrap();
        assert_eq!(p.class, ImportClass::WorkNote);
        assert_eq!(p.new_id.unwrap().to_string(), "agent/bot/working/todo/next");
        let p = place(&id("agent/bot/plan-a"), "plan").unwrap();
        assert_eq!(p.new_id.unwrap().to_string(), "agent/bot/working/todo/plan-a");

        let p = place(&id("agent/bot/insights/fast-path"), "insight").unwrap();
        assert_eq!(p.class, ImportClass::Experience);
        let p = place(&id("agent/bot/retry-gotcha"), "pitfall").unwrap();
        assert_eq!(p.hints, vec!["reclassify"]);
    }

    #[test]
    fn author_names_are_made_valid() {
        assert_eq!(author_name("(admin)"), "admin");
        assert_eq!(author_name("crash-bot"), "crash-bot");
        assert_eq!(author_name("???"), "unknown");
    }
}
