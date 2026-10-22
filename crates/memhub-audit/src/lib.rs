//! Append-only audit log of agent connections and memory activity.
//!
//! Records are JSON lines in `<dir>/audit-YYYY-MM-DD.jsonl` (UTC date), one
//! file per day. Every record is also emitted through `tracing` under the
//! `memhub::audit` target.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Who performed an action, and over which connection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub session_id: String,
    /// `None` until the connection has registered or logged in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Remote IP of the connector (`"local"` for stdio).
    pub ip: String,
    pub transport: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    Add,
    Update,
    Delete,
    GroupAdd,
    GroupRemove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    Connect {
        #[serde(flatten)]
        actor: Actor,
        /// MCP client implementation (`clientInfo` name/version).
        #[serde(skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    Disconnect {
        #[serde(flatten)]
        actor: Actor,
        reason: String,
        duration_secs: f64,
    },
    AgentRegistered {
        #[serde(flatten)]
        actor: Actor,
        role: String,
    },
    AgentLogin {
        #[serde(flatten)]
        actor: Actor,
    },
    TokenReset {
        #[serde(flatten)]
        actor: Actor,
    },
    AuthFailed {
        #[serde(flatten)]
        actor: Actor,
        claimed_agent: String,
        reason: String,
    },
    RepoJoined {
        #[serde(flatten)]
        actor: Actor,
        repo: String,
        /// The repo did not exist before and was created by this call.
        created: bool,
    },
    RepoLeft {
        #[serde(flatten)]
        actor: Actor,
        repo: String,
    },
    MemoryChanged {
        #[serde(flatten)]
        actor: Actor,
        action: MemoryAction,
        memory_id: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version_before: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        version_after: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changed_fields: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// Unified diff of the body (truncated for very large edits).
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
}

impl AuditEvent {
    pub fn name(&self) -> &'static str {
        match self {
            AuditEvent::Connect { .. } => "connect",
            AuditEvent::Disconnect { .. } => "disconnect",
            AuditEvent::AgentRegistered { .. } => "agent_registered",
            AuditEvent::AgentLogin { .. } => "agent_login",
            AuditEvent::TokenReset { .. } => "token_reset",
            AuditEvent::AuthFailed { .. } => "auth_failed",
            AuditEvent::RepoJoined { .. } => "repo_joined",
            AuditEvent::RepoLeft { .. } => "repo_left",
            AuditEvent::MemoryChanged { .. } => "memory_changed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: AuditEvent,
}

/// Filter for [`AuditLog::read`].
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub agent: Option<String>,
    pub event: Option<String>,
    pub session_id: Option<String>,
    pub memory_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
    /// Keep only the newest `limit` matching records.
    pub limit: Option<usize>,
}

pub struct AuditLog {
    dir: PathBuf,
    current: Mutex<Option<(NaiveDate, File)>>,
}

impl AuditLog {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            current: Mutex::new(None),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends a record. Logging failures are reported via `tracing` and
    /// never propagated, so auditing cannot break the operation it records.
    pub fn record(&self, event: AuditEvent) {
        let record = AuditRecord {
            ts: Utc::now(),
            event,
        };
        let line = match serde_json::to_string(&record) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("cannot serialize audit record: {e}");
                return;
            }
        };
        tracing::info!(target: "memhub::audit", "{line}");
        if let Err(e) = self.append(record.ts.date_naive(), &line) {
            tracing::error!("cannot write audit log in {}: {e}", self.dir.display());
        }
    }

    fn append(&self, date: NaiveDate, line: &str) -> io::Result<()> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().map(|(d, _)| *d) != Some(date) {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.file_for(date))?;
            *current = Some((date, file));
        }
        let (_, file) = current.as_mut().expect("file opened above");
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        // Single write of a whole line keeps O_APPEND writes from several
        // processes from interleaving.
        file.write_all(&buf)?;
        file.flush()
    }

    pub fn file_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!("audit-{}.jsonl", date.format("%Y-%m-%d")))
    }

    /// Reads matching records, oldest first. Unparseable lines are skipped.
    pub fn read(&self, query: &LogQuery) -> io::Result<Vec<serde_json::Value>> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
            })
            .collect();
        files.sort();
        if let Some(since) = query.since {
            let since_file = self.file_for(since.date_naive());
            files.retain(|f| *f >= since_file);
        }

        let field = |v: &serde_json::Value, key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_owned);
        let mut out = Vec::new();
        for path in files {
            for line in BufReader::new(File::open(&path)?).lines() {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line?) else {
                    continue;
                };
                let matches = |key: &str, want: &Option<String>| {
                    want.as_ref().is_none_or(|w| field(&value, key).as_deref() == Some(w))
                };
                if !matches("agent", &query.agent)
                    || !matches("event", &query.event)
                    || !matches("session_id", &query.session_id)
                    || !matches("memory_id", &query.memory_id)
                {
                    continue;
                }
                if let Some(since) = query.since {
                    let ts = field(&value, "ts").and_then(|t| t.parse::<DateTime<Utc>>().ok());
                    if ts.is_none_or(|t| t < since) {
                        continue;
                    }
                }
                out.push(value);
            }
        }
        if let Some(limit) = query.limit {
            let skip = out.len().saturating_sub(limit);
            out.drain(..skip);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(agent: Option<&str>) -> Actor {
        Actor {
            session_id: "s1".into(),
            agent: agent.map(Into::into),
            ip: "10.0.0.7".into(),
            transport: "http".into(),
        }
    }

    #[test]
    fn records_are_flat_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        log.record(AuditEvent::Connect {
            actor: actor(None),
            client: Some("cursor 1.0".into()),
        });
        log.record(AuditEvent::MemoryChanged {
            actor: actor(Some("alice")),
            action: MemoryAction::Update,
            memory_id: "repo/x/y".into(),
            title: "Y".into(),
            version_before: Some(1),
            version_after: Some(2),
            changed_fields: vec!["body".into()],
            group: None,
            diff: Some("-a\n+b\n".into()),
        });

        let all = log.read(&LogQuery::default()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["event"], "connect");
        assert_eq!(all[0]["ip"], "10.0.0.7");
        assert!(all[0].get("agent").is_none());
        assert_eq!(all[1]["event"], "memory_changed");
        assert_eq!(all[1]["action"], "update");
        assert_eq!(all[1]["agent"], "alice");

        let alice = log
            .read(&LogQuery {
                agent: Some("alice".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(alice.len(), 1);

        let record: AuditRecord = serde_json::from_value(all[1].clone()).unwrap();
        assert_eq!(record.event.name(), "memory_changed");
    }

    #[test]
    fn limit_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        for i in 0..5 {
            log.record(AuditEvent::RepoJoined {
                actor: actor(Some("a")),
                repo: format!("r{i}"),
                created: false,
            });
        }
        let last = log
            .read(&LogQuery {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(last.len(), 2);
        assert_eq!(last[1]["repo"], "r4");
    }
}
