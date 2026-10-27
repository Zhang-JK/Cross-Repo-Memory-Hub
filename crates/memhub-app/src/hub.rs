use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use memhub_audit::AuditLog;
use memhub_store::{MemoryStore, Scope};

use crate::{
    error::{AppError, Result},
    reads::ReadStats,
    registry::Registry,
    session::{AgentSession, ConnInfo},
};

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub data_root: PathBuf,
}

impl HubConfig {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }
}

/// Shared state behind all agent sessions.
///
/// ```text
/// <data_root>/memories/...   memory files (see memhub_store)
/// <data_root>/trash/...      deleted memories
/// <data_root>/registry/...   agent and repo profiles
/// <data_root>/logs/...       audit log
/// <data_root>/stats/...      read counts (see [`ReadStats`])
/// ```
pub struct Hub {
    config: HubConfig,
    store: MemoryStore,
    registry: Registry,
    audit: AuditLog,
    reads: ReadStats,
}

impl Hub {
    pub fn open(config: HubConfig) -> Result<Arc<Self>> {
        let root = &config.data_root;
        let store = MemoryStore::open(root)?;
        let registry = Registry::open(root)?;
        let audit = AuditLog::open(root.join("logs"))
            .map_err(|e| AppError::Internal(format!("cannot open audit log: {e}")))?;
        let reads = ReadStats::open(root);
        Ok(Arc::new(Self {
            config,
            store,
            registry,
            audit,
            reads,
        }))
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    pub fn data_root(&self) -> &Path {
        &self.config.data_root
    }

    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    pub fn reads(&self) -> &ReadStats {
        &self.reads
    }

    /// Starts a new connection and records a `connect` audit event.
    pub fn open_session(self: &Arc<Self>, conn: ConnInfo) -> AgentSession {
        AgentSession::open(self.clone(), conn)
    }

    /// Opens a session under the id of one that expired, so a client that
    /// keeps using that id carries on instead of starting over unidentified.
    /// The caller restores the identity with [`AgentSession::connect`].
    pub fn resume_session(self: &Arc<Self>, id: &str, conn: ConnInfo) -> AgentSession {
        AgentSession::resume(self.clone(), conn, id.to_string())
    }

    pub(crate) fn touch_agent(&self, name: &str) -> Result<()> {
        let _guard = self.registry.lock();
        if let Some(mut profile) = self.registry.agent(name)? {
            profile.last_seen_at = Some(Utc::now());
            self.registry.save_agent(&profile)?;
        }
        Ok(())
    }

    /// Subfolders on the way to `folder` (inclusive) that do not exist yet,
    /// parents first. Empty for an unknown or invalid owner.
    pub(crate) fn missing_folders(
        &self,
        scope: Scope,
        owner: &str,
        folder: &str,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut path = String::new();
        for segment in memhub_store::normalize_folder(folder)?.split('/') {
            if segment.is_empty() {
                continue;
            }
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(segment);
            if !self
                .store
                .folder_exists(scope, owner, &path)
                .unwrap_or(true)
            {
                out.push(path.clone());
            }
        }
        Ok(out)
    }

    pub(crate) fn memory_count(&self, scope: Scope, owner: &str) -> usize {
        self.store
            .list(&memhub_store::Filter {
                scope: Some(scope),
                owner: Some(owner.to_string()),
                ..Default::default()
            })
            .map(|v| v.len())
            .unwrap_or(0)
    }
}
