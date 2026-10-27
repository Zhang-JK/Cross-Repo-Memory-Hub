//! How often each memory is read.
//!
//! A read is an agent fetching the full memory (`get_memory`); listings,
//! searches and the management UI do not count. Counts live next to the
//! memories, not in them, so reading never changes a memory's file, version
//! or `updated_at`:
//!
//! ```text
//! <data_root>/stats/reads.json   { "<memory id>": ReadStat, ... }
//! ```
//!
//! Changes are applied in memory at once and written at most every
//! [`FLUSH_INTERVAL`], on [`ReadStats::flush`] and when the hub is dropped. A
//! write re-reads the file and replays the changes made since the last write
//! on top of it, so several processes sharing a data directory (stdio
//! servers) add up their counts instead of overwriting each other.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use chrono::{DateTime, Days, NaiveDate, Utc};
use memhub_store::MemoryId;
use serde::{Deserialize, Serialize};

/// Daily counts are kept for this many days (today included).
pub const READ_HISTORY_DAYS: u32 = 30;
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Read statistics of one memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStat {
    /// Reads since counting started (or since the memory was created).
    pub count: u64,
    pub last_read_at: Option<DateTime<Utc>>,
    pub last_reader: Option<String>,
    /// Reads per agent name.
    #[serde(default)]
    pub by_agent: BTreeMap<String, u64>,
    /// Reads per UTC day, for the last [`READ_HISTORY_DAYS`] days.
    #[serde(default)]
    pub days: BTreeMap<NaiveDate, u64>,
}

impl ReadStat {
    /// Reads in the last `days` days (today included); `None` = all time.
    pub fn count_since(&self, days: Option<u32>) -> u64 {
        match days {
            None => self.count,
            Some(days) => {
                let first = window_start(days);
                self.days.range(first..).map(|(_, n)| n).sum()
            }
        }
    }

    /// Distinct agents that read the memory (all time).
    pub fn readers(&self) -> usize {
        self.by_agent.len()
    }

    fn add(&mut self, agent: &str, at: DateTime<Utc>) {
        self.count += 1;
        *self.by_agent.entry(agent.to_string()).or_default() += 1;
        *self.days.entry(at.date_naive()).or_default() += 1;
        if self.last_read_at.is_none_or(|last| at >= last) {
            self.last_read_at = Some(at);
            self.last_reader = Some(agent.to_string());
        }
    }

    fn merge(&mut self, other: ReadStat) {
        self.count += other.count;
        for (agent, n) in other.by_agent {
            *self.by_agent.entry(agent).or_default() += n;
        }
        for (day, n) in other.days {
            *self.days.entry(day).or_default() += n;
        }
        if other.last_read_at > self.last_read_at {
            self.last_read_at = other.last_read_at;
            self.last_reader = other.last_reader;
        }
    }
}

fn window_start(days: u32) -> NaiveDate {
    let today = Utc::now().date_naive();
    today
        .checked_sub_days(Days::new(u64::from(days.max(1) - 1)))
        .unwrap_or(today)
}

enum Op {
    Read {
        id: String,
        agent: String,
        at: DateTime<Utc>,
    },
    Forget(String),
    Rename {
        from: String,
        to: String,
    },
}

type Stats = HashMap<String, ReadStat>;

fn apply(stats: &mut Stats, op: &Op) {
    match op {
        Op::Read { id, agent, at } => stats.entry(id.clone()).or_default().add(agent, *at),
        Op::Forget(id) => {
            stats.remove(id);
        }
        Op::Rename { from, to } => {
            if let Some(stat) = stats.remove(from) {
                stats.entry(to.clone()).or_default().merge(stat);
            }
        }
    }
}

fn trim_history(stats: &mut Stats) {
    let first = window_start(READ_HISTORY_DAYS);
    for stat in stats.values_mut() {
        stat.days.retain(|day, _| *day >= first);
    }
}

struct State {
    /// The file's content after the last write, plus `pending`.
    stats: Stats,
    /// Changes not written yet, oldest first.
    pending: Vec<Op>,
    last_flush: Instant,
}

pub struct ReadStats {
    path: PathBuf,
    state: Mutex<State>,
}

impl ReadStats {
    pub(crate) fn open(data_root: &Path) -> Self {
        let path = data_root.join("stats").join("reads.json");
        Self {
            state: Mutex::new(State {
                stats: load(&path),
                pending: Vec::new(),
                last_flush: Instant::now(),
            }),
            path,
        }
    }

    /// Counts one read of `id` by `agent`.
    pub fn record(&self, id: &MemoryId, agent: &str) {
        self.change(Op::Read {
            id: id.to_string(),
            agent: agent.to_string(),
            at: Utc::now(),
        });
    }

    /// Drops the statistics of a deleted memory.
    pub fn forget(&self, id: &MemoryId) {
        self.change(Op::Forget(id.to_string()));
    }

    /// Carries the statistics of a moved / renamed memory over to its new id.
    pub fn rename(&self, from: &MemoryId, to: &MemoryId) {
        if from != to {
            self.change(Op::Rename {
                from: from.to_string(),
                to: to.to_string(),
            });
        }
    }

    pub fn get(&self, id: &MemoryId) -> ReadStat {
        self.lock()
            .stats
            .get(&id.to_string())
            .cloned()
            .unwrap_or_default()
    }

    /// Statistics of every memory read at least once, by memory id.
    pub fn all(&self) -> HashMap<String, ReadStat> {
        self.lock().stats.clone()
    }

    /// Writes pending changes now.
    pub fn flush(&self) {
        let mut state = self.lock();
        self.write(&mut state);
    }

    fn change(&self, op: Op) {
        let mut state = self.lock();
        apply(&mut state.stats, &op);
        state.pending.push(op);
        if state.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.write(&mut state);
        }
    }

    fn write(&self, state: &mut State) {
        state.last_flush = Instant::now();
        if state.pending.is_empty() {
            return;
        }
        let mut stats = load(&self.path);
        for op in &state.pending {
            apply(&mut stats, op);
        }
        trim_history(&mut stats);
        if let Err(e) = save(&self.path, &stats) {
            // Keep the changes pending and retry on the next write.
            tracing::warn!("cannot save read counts to {}: {e}", self.path.display());
            return;
        }
        state.pending.clear();
        state.stats = stats;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for ReadStats {
    fn drop(&mut self) {
        self.flush();
    }
}

fn load(path: &Path) -> Stats {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring malformed {}: {e}", path.display());
            Stats::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stats::new(),
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            Stats::new()
        }
    }
}

fn save(path: &Path, stats: &Stats) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let sorted: BTreeMap<&String, &ReadStat> = stats.iter().collect();
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut tmp, &sorted)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use memhub_store::Scope;

    use super::*;

    fn id(slug: &str) -> MemoryId {
        MemoryId::new(Scope::Repo, "r", slug).unwrap()
    }

    #[test]
    fn counts_moves_forgets_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let reads = ReadStats::open(dir.path());
        reads.record(&id("a"), "alice");
        reads.record(&id("a"), "bob");
        reads.record(&id("a"), "alice");
        reads.record(&id("b"), "bob");
        let a = reads.get(&id("a"));
        assert_eq!(a.count, 3);
        assert_eq!(a.count_since(Some(1)), 3);
        assert_eq!(a.count_since(Some(7)), 3);
        assert_eq!(a.readers(), 2);
        assert_eq!(a.by_agent["alice"], 2);
        assert_eq!(a.last_reader.as_deref(), Some("alice"));

        reads.rename(&id("a"), &id("c"));
        assert_eq!(reads.get(&id("a")).count, 0);
        assert_eq!(reads.get(&id("c")).count, 3);
        reads.forget(&id("b"));
        assert_eq!(reads.get(&id("b")), ReadStat::default());
        drop(reads);

        let reopened = ReadStats::open(dir.path());
        assert_eq!(reopened.get(&id("c")).count, 3);
        assert_eq!(reopened.all().len(), 1);
    }

    #[test]
    fn processes_sharing_a_data_dir_add_up() {
        let dir = tempfile::tempdir().unwrap();
        let one = ReadStats::open(dir.path());
        let two = ReadStats::open(dir.path());
        one.record(&id("a"), "alice");
        two.record(&id("a"), "bob");
        two.record(&id("a"), "bob");
        one.flush();
        two.flush();
        assert_eq!(two.get(&id("a")).count, 3);
        drop((one, two));
        assert_eq!(ReadStats::open(dir.path()).get(&id("a")).count, 3);
    }

    #[test]
    fn windows_ignore_old_days() {
        let mut stat = ReadStat::default();
        let old = Utc::now() - chrono::Duration::days(10);
        stat.add("alice", old);
        stat.add("alice", Utc::now());
        assert_eq!(stat.count_since(None), 2);
        assert_eq!(stat.count_since(Some(7)), 1);
        assert_eq!(stat.count_since(Some(30)), 2);
        assert_eq!(
            stat.last_read_at.unwrap().date_naive(),
            Utc::now().date_naive()
        );
    }
}
