//! Agent and repository registry, stored as YAML files:
//!
//! ```text
//! registry/agents/<name>.yaml
//! registry/repos/<repo-id>.yaml
//! ```

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    /// The agent's specialty, e.g. `feature-impl`, `crash-analysis`, `perf-opt`.
    pub role: String,
    #[serde(default)]
    pub description: String,
    /// Repos the agent works on; grants write access to their repo memory.
    #[serde(default)]
    pub repos: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// SHA-256 of the agent's access token.
    #[serde(default)]
    pub token_hash: String,
}

/// Public view of an agent (no credentials).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub role: String,
    pub description: String,
    pub repos: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl AgentProfile {
    pub fn info(&self) -> AgentInfo {
        AgentInfo {
            name: self.name.clone(),
            role: self.role.clone(),
            description: self.description.clone(),
            repos: self.repos.clone(),
            created_at: self.created_at,
            last_seen_at: self.last_seen_at,
        }
    }

    pub fn works_on(&self, repo: &str) -> bool {
        self.repos.iter().any(|r| r == repo)
    }

    pub fn token_matches(&self, token: &str) -> bool {
        let candidate = hash_token(token);
        // Constant-time comparison of the hex digests.
        candidate.len() == self.token_hash.len()
            && candidate
                .bytes()
                .zip(self.token_hash.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoProfile {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Local path or remote URL of the repository, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

pub fn new_token() -> String {
    format!(
        "mh_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub struct Registry {
    agents: PathBuf,
    repos: PathBuf,
    lock: Mutex<()>,
}

impl Registry {
    pub fn open(data_root: &Path) -> Result<Self> {
        let agents = data_root.join("registry").join("agents");
        let repos = data_root.join("registry").join("repos");
        for dir in [&agents, &repos] {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }
        Ok(Self {
            agents,
            repos,
            lock: Mutex::new(()),
        })
    }

    /// Serializes read-modify-write sequences on registry files.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn agent(&self, name: &str) -> Result<Option<AgentProfile>> {
        read_yaml(&self.agents.join(format!("{name}.yaml")))
    }

    pub fn agents(&self) -> Result<Vec<AgentProfile>> {
        list_yaml(&self.agents)
    }

    /// Fails with [`AppError::AgentExists`] if the name is taken.
    pub fn create_agent(&self, profile: &AgentProfile) -> Result<()> {
        let path = self.agents.join(format!("{}.yaml", profile.name));
        if write_yaml(&path, profile, true)? {
            Ok(())
        } else {
            Err(AppError::AgentExists(profile.name.clone()))
        }
    }

    pub fn save_agent(&self, profile: &AgentProfile) -> Result<()> {
        write_yaml(&self.agents.join(format!("{}.yaml", profile.name)), profile, false)?;
        Ok(())
    }

    pub fn repo(&self, id: &str) -> Result<Option<RepoProfile>> {
        read_yaml(&self.repos.join(format!("{id}.yaml")))
    }

    pub fn repos(&self) -> Result<Vec<RepoProfile>> {
        list_yaml(&self.repos)
    }

    /// Returns `false` if the repo already existed.
    pub fn create_repo(&self, profile: &RepoProfile) -> Result<bool> {
        write_yaml(&self.repos.join(format!("{}.yaml", profile.id)), profile, true)
    }

    pub fn save_repo(&self, profile: &RepoProfile) -> Result<()> {
        write_yaml(&self.repos.join(format!("{}.yaml", profile.id)), profile, false)?;
        Ok(())
    }
}

fn io_err(path: &Path, e: io::Error) -> AppError {
    AppError::Internal(format!("{}: {e}", path.display()))
}

fn read_yaml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_yaml::from_str(&text)
            .map(Some)
            .map_err(|e| AppError::Internal(format!("malformed {}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

fn list_yaml<T: DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| io_err(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        match read_yaml(&path) {
            Ok(Some(v)) => out.push(v),
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping registry entry: {e}"),
        }
    }
    Ok(out)
}

/// Atomically writes `value`. With `create_new`, returns `false` instead of
/// overwriting an existing file.
fn write_yaml<T: Serialize>(path: &Path, value: &T, create_new: bool) -> Result<bool> {
    let text = serde_yaml::to_string(value).map_err(|e| AppError::Internal(e.to_string()))?;
    let dir = path.parent().expect("registry path has a parent");
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .suffix(".yaml.part")
        .tempfile_in(dir)
        .map_err(|e| io_err(dir, e))?;
    tmp.write_all(text.as_bytes()).map_err(|e| io_err(path, e))?;
    let persisted = if create_new {
        tmp.persist_noclobber(path)
    } else {
        tmp.persist(path)
    };
    match persisted {
        Ok(_) => Ok(true),
        Err(e) if create_new && e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io_err(path, e.error)),
    }
}
