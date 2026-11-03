//! What agents said about memories after using them.
//!
//! ```text
//! <data_root>/stats/feedback.json   { "<memory id>": Feedback, ... }
//! ```
//!
//! Like read counts, feedback lives next to the memories, not in them, so a
//! vote never changes a memory's version. Changes re-read the file first, so
//! several processes sharing a data directory do not overwrite each other.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use memhub_store::MemoryId;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Notes kept per memory, newest last.
const MAX_NOTES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    /// The memory helped with the task.
    Helpful,
    /// Following the memory led the agent astray.
    Harmful,
    /// The memory was true once but no longer matches the code.
    Outdated,
}

impl Rating {
    pub fn as_str(self) -> &'static str {
        match self {
            Rating::Helpful => "helpful",
            Rating::Harmful => "harmful",
            Rating::Outdated => "outdated",
        }
    }
}

impl fmt::Display for Rating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Rating {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Self, AppError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "helpful" | "useful" | "up" => Ok(Rating::Helpful),
            "harmful" | "wrong" | "misleading" | "down" => Ok(Rating::Harmful),
            "outdated" | "stale" => Ok(Rating::Outdated),
            other => Err(AppError::Invalid(format!(
                "unknown rating '{other}', expected helpful, harmful or outdated"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackNote {
    pub at: DateTime<Utc>,
    pub agent: String,
    /// `helpful`, `harmful`, `outdated` or `dispute`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    #[serde(default)]
    pub helpful: u32,
    #[serde(default)]
    pub harmful: u32,
    #[serde(default)]
    pub outdated: u32,
    #[serde(default)]
    pub disputes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<FeedbackNote>,
}

impl Feedback {
    pub fn is_empty(&self) -> bool {
        self.helpful == 0 && self.harmful == 0 && self.outdated == 0 && self.disputes == 0
    }

    /// Ranking multiplier for experience search: helpful votes lift a memory
    /// a little, harmful / outdated votes and disputes push it down more.
    pub fn search_weight(&self) -> f64 {
        let w = 1.0 + f64::from(self.helpful.min(10)) * 0.03
            - f64::from(self.harmful) * 0.15
            - f64::from(self.outdated) * 0.1
            - f64::from(self.disputes) * 0.1;
        w.clamp(0.2, 1.3)
    }

    fn note(&mut self, agent: &str, kind: &str, note: Option<String>) {
        let at = Utc::now();
        self.last_at = Some(at);
        self.recent.push(FeedbackNote {
            at,
            agent: agent.to_string(),
            kind: kind.to_string(),
            note,
        });
        let excess = self.recent.len().saturating_sub(MAX_NOTES);
        self.recent.drain(..excess);
    }
}

type All = HashMap<String, Feedback>;

pub struct FeedbackStats {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FeedbackStats {
    pub(crate) fn open(data_root: &Path) -> Self {
        Self {
            path: data_root.join("stats").join("feedback.json"),
            lock: Mutex::new(()),
        }
    }

    pub fn rate(&self, id: &MemoryId, agent: &str, rating: Rating, note: Option<String>) -> Feedback {
        self.change(id, |f| {
            match rating {
                Rating::Helpful => f.helpful += 1,
                Rating::Harmful => f.harmful += 1,
                Rating::Outdated => f.outdated += 1,
            }
            f.note(agent, rating.as_str(), note);
        })
    }

    pub fn dispute(&self, id: &MemoryId, agent: &str, reason: &str) -> Feedback {
        self.change(id, |f| {
            f.disputes += 1;
            f.note(agent, "dispute", Some(reason.to_string()));
        })
    }

    pub fn get(&self, id: &MemoryId) -> Feedback {
        load(&self.path).remove(&id.to_string()).unwrap_or_default()
    }

    pub fn all(&self) -> All {
        load(&self.path)
    }

    pub fn rename(&self, from: &MemoryId, to: &MemoryId) {
        if from == to {
            return;
        }
        let _guard = self.lock();
        let mut all = load(&self.path);
        if let Some(f) = all.remove(&from.to_string()) {
            all.insert(to.to_string(), f);
            self.save(&all);
        }
    }

    pub fn forget(&self, id: &MemoryId) {
        let _guard = self.lock();
        let mut all = load(&self.path);
        if all.remove(&id.to_string()).is_some() {
            self.save(&all);
        }
    }

    fn change(&self, id: &MemoryId, f: impl FnOnce(&mut Feedback)) -> Feedback {
        let _guard = self.lock();
        let mut all = load(&self.path);
        let entry = all.entry(id.to_string()).or_default();
        f(entry);
        let out = entry.clone();
        self.save(&all);
        out
    }

    fn save(&self, all: &All) {
        if let Err(e) = save_json(&self.path, &all.iter().collect::<BTreeMap<_, _>>()) {
            tracing::warn!("cannot save feedback to {}: {e}", self.path.display());
        }
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring malformed {}: {e}", path.display());
            T::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            T::default()
        }
    }
}

pub(crate) fn save_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn load(path: &Path) -> All {
    load_json(path)
}

#[cfg(test)]
mod tests {
    use memhub_store::Scope;

    use super::*;

    #[test]
    fn votes_weights_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let stats = FeedbackStats::open(dir.path());
        let id = MemoryId::new(Scope::Repo, "r", "a").unwrap();
        stats.rate(&id, "bot", Rating::Helpful, None);
        let f = stats.rate(&id, "bot", Rating::Harmful, Some("wrong flag".into()));
        assert_eq!((f.helpful, f.harmful), (1, 1));
        assert!(f.search_weight() < 1.0);
        let to = MemoryId::new(Scope::Repo, "r", "b").unwrap();
        stats.rename(&id, &to);
        assert!(stats.get(&id).is_empty());
        assert_eq!(stats.get(&to).recent.len(), 2);
        for _ in 0..20 {
            stats.dispute(&to, "bot", "nope");
        }
        let f = stats.get(&to);
        assert_eq!(f.recent.len(), MAX_NOTES);
        assert_eq!(f.search_weight(), 0.2);
    }
}
