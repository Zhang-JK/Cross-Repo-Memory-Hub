//! The evidence layer: append-only records of what agents observed while
//! working. Agents write here; curated experience memories cite these
//! records. Evidence is never edited and is excluded from experience
//! search; it is read by id or searched explicitly within a time range.
//!
//! ```text
//! <data_root>/evidence/<yyyy-mm-dd>/<agent>/<hhmmss>-<slug>.md
//! <data_root>/evidence/legacy/<scope>/<owner>/[<folder>/]<slug>.md   ported from 1.0
//! ```
//!
//! Ids mirror the path: `evidence/2026-11-03/crash-bot/091512-refund-dup`.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, MutexGuard},
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    error::{IoContext, Result, StoreError},
    experience::validate_code_ref,
    frontmatter,
    model::{normalize_labels, slugify, validate_label, validate_name, validate_title, MemoryId},
    search,
};

pub const EVIDENCE_PREFIX: &str = "evidence/";
/// First path segment of evidence ported from Memory Hub 1.0.
pub const LEGACY_SEGMENT: &str = "legacy";
/// Longest evidence body accepted, in characters.
pub const MAX_EVIDENCE_CHARS: usize = 20_000;
const MAX_ID_SEGMENTS: usize = 10;

/// Address of an evidence record, `evidence/<path>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceId {
    path: String,
}

impl EvidenceId {
    pub fn from_path(path: &str) -> Result<Self> {
        let path = path.trim().trim_matches('/');
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() < 2 || segments.len() > MAX_ID_SEGMENTS {
            return Err(StoreError::Invalid(format!(
                "invalid evidence id 'evidence/{path}': expected 'evidence/<date>/<agent>/<name>'"
            )));
        }
        for s in &segments {
            validate_name("evidence id segment", s)?;
        }
        Ok(Self {
            path: segments.join("/"),
        })
    }

    /// The part after `evidence/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn is_legacy(&self) -> bool {
        self.path.starts_with("legacy/")
    }

    /// Recording day, for non-legacy ids.
    pub fn date(&self) -> Option<NaiveDate> {
        let first = self.path.split('/').next()?;
        NaiveDate::parse_from_str(first, "%Y-%m-%d").ok()
    }

    /// Legacy id of a 1.0 memory.
    pub fn legacy(memory: &MemoryId) -> Self {
        Self {
            path: format!("{LEGACY_SEGMENT}/{memory}"),
        }
    }

    /// `true` for strings that look like evidence ids.
    pub fn looks_like(s: &str) -> bool {
        s.trim().starts_with(EVIDENCE_PREFIX)
    }
}

impl fmt::Display for EvidenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{EVIDENCE_PREFIX}{}", self.path)
    }
}

impl FromStr for EvidenceId {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().trim_end_matches(".md");
        let path = s.strip_prefix(EVIDENCE_PREFIX).ok_or_else(|| {
            StoreError::Invalid(format!("invalid evidence id '{s}': must start with 'evidence/'"))
        })?;
        Self::from_path(path)
    }
}

impl Serialize for EvidenceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EvidenceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty() {
            return Ok(Self::default());
        }
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// How the task the evidence comes from ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Failure,
    Partial,
    #[default]
    Unknown,
}

impl FromStr for Outcome {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "success" | "succeeded" | "ok" => Outcome::Success,
            "failure" | "failed" | "fail" => Outcome::Failure,
            "partial" => Outcome::Partial,
            "unknown" | "" => Outcome::Unknown,
            other => {
                return Err(StoreError::Invalid(format!(
                    "unknown outcome '{other}', expected success, failure, partial or unknown"
                )))
            }
        })
    }
}

/// Whether the evidence is first-hand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The agent saw it itself (ran the code, read the source, hit the bug).
    #[default]
    Observed,
    /// Repeats what a memory said without checking it independently. Does
    /// not count as independent support for that memory.
    FromMemory,
    /// Ported from a Memory Hub 1.0 memory.
    Legacy,
}

impl FromStr for Source {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "observed" => Source::Observed,
            "from_memory" | "memory" => Source::FromMemory,
            "legacy" => Source::Legacy,
            other => {
                return Err(StoreError::Invalid(format!(
                    "unknown source '{other}', expected observed or from_memory"
                )))
            }
        })
    }
}

fn default_importance() -> u8 {
    3
}

fn default_kind() -> String {
    "observation".to_string()
}

fn is_default_importance(n: &u8) -> bool {
    *n == default_importance()
}

/// Front matter of an evidence record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceMeta {
    #[serde(default)]
    pub id: EvidenceId,
    #[serde(default)]
    pub title: String,
    /// `observation`, `success`, `failure`, `decision`, `dispute`, `legacy`, ...
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    /// Repos the evidence is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// The task the agent was working on, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub outcome: Outcome,
    #[serde(default)]
    pub source: Source,
    /// Memory ids the agent relied on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<String>,
    /// Code locations, `path[:line][@commit]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_refs: Vec<String>,
    /// Why this matters and to whom: the context the curator does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 1 (trivia) .. 5 (would have saved hours).
    #[serde(default = "default_importance", skip_serializing_if = "is_default_importance")]
    pub importance: u8,
    /// One-sentence lesson the agent would tell its successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_lesson: Option<String>,
    /// An earlier evidence record this one corrects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(flatten)]
    pub meta: EvidenceMeta,
    pub body: String,
}

impl Evidence {
    pub fn id(&self) -> &EvidenceId {
        &self.meta.id
    }

    /// Key identifying one independent source: the same agent on the same
    /// task (or day) counts once, however many records it wrote.
    pub fn source_key(&self) -> Option<String> {
        if self.meta.source == Source::FromMemory {
            return None;
        }
        let context = self
            .meta
            .task
            .as_deref()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.meta.created_at.date_naive().to_string());
        Some(format!("{}\u{0}{context}", self.meta.author))
    }
}

/// Input for [`EvidenceStore::add`].
#[derive(Debug, Clone, Default)]
pub struct NewEvidence {
    pub author: String,
    pub title: String,
    pub kind: Option<String>,
    pub body: String,
    pub repos: Vec<String>,
    pub task: Option<String>,
    pub outcome: Outcome,
    pub source: Source,
    pub derived_from: Vec<String>,
    pub code_refs: Vec<String>,
    pub reason: Option<String>,
    pub importance: Option<u8>,
    pub candidate_lesson: Option<String>,
    pub supersedes: Option<String>,
    pub tags: Vec<String>,
    pub extra: BTreeMap<String, serde_yaml::Value>,
    /// Recording time; now when absent.
    pub created_at: Option<DateTime<Utc>>,
    /// Explicit path below `evidence/` (legacy import); derived from date,
    /// author and title when absent.
    pub path: Option<String>,
}

/// Selection for [`EvidenceStore::list`] / [`EvidenceStore::search`].
#[derive(Debug, Clone, Default)]
pub struct EvidenceQuery {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub author: Option<String>,
    pub repo: Option<String>,
    pub kind: Option<String>,
    pub text: Option<String>,
    /// Also consider evidence ported from 1.0 (`evidence/legacy/...`).
    pub include_legacy: bool,
    /// Only records strictly after this `(created_at, id)` position.
    pub after: Option<(DateTime<Utc>, String)>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceHit {
    #[serde(flatten)]
    pub meta: EvidenceMeta,
    pub score: f64,
    pub snippet: String,
}

pub struct EvidenceStore {
    root: PathBuf,
    trash: PathBuf,
    lock: Mutex<()>,
}

impl EvidenceStore {
    pub fn open(data_root: impl AsRef<Path>) -> Result<Self> {
        let root = data_root.as_ref().join("evidence");
        fs::create_dir_all(&root).at(&root)?;
        Ok(Self {
            root,
            trash: data_root.as_ref().join("trash").join("evidence"),
            lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, id: &EvidenceId) -> PathBuf {
        let (dir, name) = id.path().rsplit_once('/').expect("evidence ids have 2+ segments");
        let mut p = self.root.clone();
        for s in dir.split('/') {
            p.push(s);
        }
        p.join(format!("{name}.md"))
    }

    pub fn exists(&self, id: &EvidenceId) -> bool {
        self.path_of(id).is_file()
    }

    pub fn get(&self, id: &EvidenceId) -> Result<Evidence> {
        let path = self.path_of(id);
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_string()))
            }
            other => other.at(&path)?,
        };
        parse(&text, id.clone(), &path)
    }

    pub fn add(&self, new: NewEvidence) -> Result<Evidence> {
        validate_name("agent name", &new.author)?;
        let title = new.title.trim().to_string();
        validate_title(&title)?;
        let kind = new
            .kind
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(default_kind);
        validate_label("kind", &kind)?;
        if new.source != Source::Legacy && new.body.chars().count() > MAX_EVIDENCE_CHARS {
            return Err(StoreError::Invalid(format!(
                "evidence body must be at most {MAX_EVIDENCE_CHARS} characters; record separate \
                 observations as separate evidence"
            )));
        }
        let importance = new.importance.unwrap_or(default_importance());
        if !(1..=5).contains(&importance) {
            return Err(StoreError::Invalid("importance must be between 1 and 5".into()));
        }
        for repo in &new.repos {
            validate_name("repo id", repo)?;
        }
        for r in &new.code_refs {
            validate_code_ref(r)?;
        }
        for d in &new.derived_from {
            d.parse::<MemoryId>()?;
        }
        if let Some(s) = &new.supersedes {
            s.parse::<EvidenceId>()?;
        }
        let one_line = |label: &str, v: Option<String>, max: usize| -> Result<Option<String>> {
            let v = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            if let Some(s) = &v {
                if s.chars().count() > max {
                    return Err(StoreError::Invalid(format!(
                        "{label} must be at most {max} characters"
                    )));
                }
            }
            Ok(v)
        };
        let task = one_line("task", new.task, 300)?;
        let reason = one_line("reason", new.reason, 1000)?;
        let candidate_lesson = one_line("candidate_lesson", new.candidate_lesson, 1000)?;
        let tags = normalize_labels("tag", new.tags)?;
        let created_at = new.created_at.unwrap_or_else(Utc::now);

        let meta = |id: EvidenceId| EvidenceMeta {
            id,
            title: title.clone(),
            kind: kind.clone(),
            author: new.author.clone(),
            created_at,
            repos: new.repos.clone(),
            task: task.clone(),
            outcome: new.outcome,
            source: new.source,
            derived_from: new.derived_from.clone(),
            code_refs: new.code_refs.clone(),
            reason: reason.clone(),
            importance,
            candidate_lesson: candidate_lesson.clone(),
            supersedes: new.supersedes.clone(),
            tags: tags.clone(),
            extra: new.extra.clone(),
        };
        let body = frontmatter::normalize_body(&new.body);

        let _guard = self.lock();
        if let Some(path) = &new.path {
            let id = EvidenceId::from_path(path)?;
            let evidence = Evidence {
                meta: meta(id.clone()),
                body,
            };
            self.write_new(&evidence)?;
            return Ok(evidence);
        }
        let base = format!(
            "{}/{}/{}-{}",
            created_at.format("%Y-%m-%d"),
            new.author,
            created_at.format("%H%M%S"),
            slugify(&title).chars().take(40).collect::<String>().trim_end_matches('-')
        );
        for n in 1u32.. {
            let path = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let id = EvidenceId::from_path(&path)?;
            let evidence = Evidence {
                meta: meta(id),
                body: body.clone(),
            };
            match self.write_new(&evidence) {
                Ok(()) => return Ok(evidence),
                Err(StoreError::AlreadyExists(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("evidence suffix space exhausted")
    }

    /// Moves a record to `trash/evidence/`. Evidence is append-only; this is
    /// for operators and the self-test only.
    pub fn delete(&self, id: &EvidenceId) -> Result<Evidence> {
        let _guard = self.lock();
        let evidence = self.get(id)?;
        let source = self.path_of(id);
        let (parent, name) = id.path().rsplit_once('/').expect("evidence ids have 2+ segments");
        let mut dir = self.trash.clone();
        for s in parent.split('/') {
            dir.push(s);
        }
        fs::create_dir_all(&dir).at(&dir)?;
        let target = dir.join(format!("{name}.{}.md", Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
        fs::rename(&source, &target).at(&source)?;
        Ok(evidence)
    }

    /// Matching records, oldest first.
    pub fn list(&self, query: &EvidenceQuery) -> Result<Vec<Evidence>> {
        let since_day = query.since.map(|t| t.date_naive());
        let until_day = query.until.map(|t| t.date_naive());
        let mut out = Vec::new();
        for entry in read_dir(&self.root)? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !path.is_dir() {
                continue;
            }
            if name == LEGACY_SEGMENT {
                if query.include_legacy {
                    self.walk(&path, name, query, &mut out)?;
                }
                continue;
            }
            let Ok(day) = NaiveDate::parse_from_str(name, "%Y-%m-%d") else {
                continue;
            };
            if since_day.is_some_and(|d| day < d) || until_day.is_some_and(|d| day > d) {
                continue;
            }
            match &query.author {
                Some(author) => {
                    let dir = path.join(author);
                    if dir.is_dir() {
                        self.walk(&dir, &format!("{name}/{author}"), query, &mut out)?;
                    }
                }
                None => self.walk(&path, name, query, &mut out)?,
            }
        }
        out.sort_by(|a, b| {
            a.meta
                .created_at
                .cmp(&b.meta.created_at)
                .then_with(|| a.meta.id.cmp(&b.meta.id))
        });
        if let Some(limit) = query.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// Keyword search within the selection, best first. Without text, the
    /// newest records come first.
    pub fn search(&self, query: &EvidenceQuery) -> Result<Vec<EvidenceHit>> {
        let terms = query.text.as_deref().map(search::terms).unwrap_or_default();
        let limit = query.limit.unwrap_or(20).max(1);
        let mut all = self.list(&EvidenceQuery {
            limit: None,
            ..query.clone()
        })?;
        all.reverse();
        let mut hits: Vec<EvidenceHit> = all
            .into_iter()
            .filter_map(|e| {
                let score = if terms.is_empty() {
                    0.0
                } else {
                    score(&e, &terms)?
                };
                let snippet = search::snippet_of(&e.body, &terms);
                Some(EvidenceHit {
                    meta: e.meta,
                    score,
                    snippet,
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Number of non-legacy and legacy records.
    pub fn count(&self) -> Result<(usize, usize)> {
        let all = self.list(&EvidenceQuery {
            include_legacy: true,
            ..Default::default()
        })?;
        let legacy = all.iter().filter(|e| e.meta.id.is_legacy()).count();
        Ok((all.len() - legacy, legacy))
    }

    fn walk(&self, dir: &Path, prefix: &str, query: &EvidenceQuery, out: &mut Vec<Evidence>) -> Result<()> {
        for entry in read_dir(dir)? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_dir() {
                if validate_name("evidence id segment", name).is_ok() {
                    self.walk(&path, &format!("{prefix}/{name}"), query, out)?;
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            let Ok(id) = EvidenceId::from_path(&format!("{prefix}/{stem}")) else {
                continue;
            };
            match self.get(&id) {
                Ok(e) if matches(query, &e) => out.push(e),
                Ok(_) | Err(StoreError::NotFound(_)) => {}
                Err(e) => tracing::warn!("skipping unreadable evidence {id}: {e}"),
            }
        }
        Ok(())
    }

    fn write_new(&self, evidence: &Evidence) -> Result<()> {
        let path = self.path_of(&evidence.meta.id);
        let dir = path.parent().expect("evidence path has a parent");
        fs::create_dir_all(dir).at(dir)?;
        let text = frontmatter::render_parts(&evidence.meta, &evidence.body)?;
        crate::store::write_new(&path, &text)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn matches(q: &EvidenceQuery, e: &Evidence) -> bool {
    let m = &e.meta;
    if q.since.is_some_and(|s| m.created_at < s) || q.until.is_some_and(|u| m.created_at > u) {
        return false;
    }
    if q.author.as_ref().is_some_and(|a| a != &m.author) {
        return false;
    }
    if q.repo.as_ref().is_some_and(|r| !m.repos.iter().any(|x| x == r)) {
        return false;
    }
    if q.kind.as_ref().is_some_and(|k| !k.eq_ignore_ascii_case(&m.kind)) {
        return false;
    }
    if let Some((ts, id)) = &q.after {
        let key = (m.created_at, m.id.to_string());
        if key <= (*ts, id.clone()) {
            return false;
        }
    }
    true
}

fn score(e: &Evidence, terms: &[String]) -> Option<f64> {
    let m = &e.meta;
    let title = m.title.to_lowercase();
    let body = e.body.to_lowercase();
    let side = [
        m.task.as_deref().unwrap_or(""),
        m.reason.as_deref().unwrap_or(""),
        m.candidate_lesson.as_deref().unwrap_or(""),
    ]
    .join(" ")
    .to_lowercase();
    let refs = m.code_refs.join(" ").to_lowercase();
    let mut total = 0.0;
    let mut matched = 0usize;
    for term in terms {
        let mut s = title.matches(term.as_str()).count() as f64 * 5.0;
        s += side.matches(term.as_str()).count().min(5) as f64 * 2.0;
        if refs.contains(term.as_str()) {
            s += 3.0;
        }
        if m.tags.iter().any(|t| t.to_lowercase().contains(term.as_str())) {
            s += 4.0;
        }
        s += body.matches(term.as_str()).count().min(20) as f64;
        if s > 0.0 {
            matched += 1;
            total += s;
        }
    }
    if matched == 0 {
        return None;
    }
    let coverage = matched as f64 / terms.len() as f64;
    Some(total * coverage * coverage + if matched == terms.len() { 10.0 } else { 0.0 })
}

fn parse(text: &str, id: EvidenceId, path: &Path) -> Result<Evidence> {
    let (yaml, body) = frontmatter::split(text.strip_prefix('\u{feff}').unwrap_or(text));
    let mut meta: EvidenceMeta = match yaml {
        Some(yaml) if !yaml.trim().is_empty() => {
            serde_yaml::from_str(yaml).map_err(|e| StoreError::Malformed {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?
        }
        _ => serde_yaml::from_str("{}").expect("empty front matter deserializes"),
    };
    meta.id = id;
    Ok(Evidence {
        meta,
        body: frontmatter::normalize_body(body),
    })
}

/// The file text of a record, as stored.
pub fn render_evidence(evidence: &Evidence) -> Result<String> {
    frontmatter::render_parts(&evidence.meta, &evidence.body)
}

fn read_dir(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        other => other.at(dir)?.map(|e| e.at(dir)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn new(author: &str, title: &str) -> NewEvidence {
        NewEvidence {
            author: author.into(),
            title: title.into(),
            body: "saw it".into(),
            repos: vec!["engine".into()],
            ..Default::default()
        }
    }

    #[test]
    fn ids_round_trip() {
        let id: EvidenceId = "evidence/2026-11-03/bot/091512-x".parse().unwrap();
        assert_eq!(id.to_string(), "evidence/2026-11-03/bot/091512-x");
        assert_eq!(id.date(), NaiveDate::from_ymd_opt(2026, 11, 3));
        assert!(!id.is_legacy());
        assert!("evidence/../x".parse::<EvidenceId>().is_err());
        assert!("repo/x/y".parse::<EvidenceId>().is_err());
        let legacy = EvidenceId::legacy(&"repo/engine/pitfalls/x".parse().unwrap());
        assert_eq!(legacy.to_string(), "evidence/legacy/repo/engine/pitfalls/x");
        assert!(legacy.is_legacy());
    }

    #[test]
    fn append_list_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();
        let a = store.add(new("bot", "Refund retries duplicate")).unwrap();
        let b = store.add(new("bot", "Refund retries duplicate")).unwrap();
        assert_ne!(a.meta.id, b.meta.id, "same second and title get a suffix");
        let mut old = new("other", "Old observation");
        old.created_at = Some(Utc::now() - Duration::days(40));
        old.repos = vec!["web".into()];
        store.add(old).unwrap();

        let fetched = store.get(&a.meta.id).unwrap();
        assert_eq!(fetched, a);

        let recent = store
            .list(&EvidenceQuery {
                since: Some(Utc::now() - Duration::days(7)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(recent.len(), 2);
        let web = store
            .list(&EvidenceQuery {
                repo: Some("web".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(web.len(), 1);
        let after = store
            .list(&EvidenceQuery {
                after: Some((a.meta.created_at, a.meta.id.to_string())),
                ..Default::default()
            })
            .unwrap();
        assert!(after.iter().all(|e| e.meta.id != a.meta.id));

        let hits = store
            .search(&EvidenceQuery {
                text: Some("refund".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn legacy_paths_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();
        let mut legacy = new("alice", "Old note");
        legacy.path = Some("legacy/repo/engine/pitfalls/old-note".into());
        legacy.source = Source::Legacy;
        let e = store.add(legacy.clone()).unwrap();
        assert!(e.meta.id.is_legacy());
        assert!(matches!(store.add(legacy), Err(StoreError::AlreadyExists(_))));
        assert!(store.list(&EvidenceQuery::default()).unwrap().is_empty());
        assert_eq!(
            store
                .list(&EvidenceQuery {
                    include_legacy: true,
                    ..Default::default()
                })
                .unwrap()
                .len(),
            1
        );

        let mut bad = new("bot", "x");
        bad.code_refs = vec!["has space.rs".into()];
        assert!(store.add(bad).is_err());
        let mut bad = new("bot", "x");
        bad.importance = Some(9);
        assert!(store.add(bad).is_err());
        assert_eq!(store.count().unwrap(), (0, 1));
    }

    #[test]
    fn source_keys_ignore_memory_derived_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();
        let mut a = new("bot", "A");
        a.task = Some("Fix refunds".into());
        let a = store.add(a).unwrap();
        let mut b = new("bot", "B");
        b.task = Some("fix refunds".into());
        let b = store.add(b).unwrap();
        let mut c = new("bot", "C");
        c.source = Source::FromMemory;
        let c = store.add(c).unwrap();
        assert_eq!(a.source_key(), b.source_key());
        assert_eq!(c.source_key(), None);
    }
}
