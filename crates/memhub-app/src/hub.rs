use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use memhub_audit::AuditLog;
use memhub_store::{MemoryStore, Scope};

use crate::{
    error::{AppError, Result},
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
/// ```
pub struct Hub {
    config: HubConfig,
    store: MemoryStore,
    registry: Registry,
    audit: AuditLog,
}

impl Hub {
    pub fn open(config: HubConfig) -> Result<Arc<Self>> {
        let root = &config.data_root;
        let store = MemoryStore::open(root)?;
        let registry = Registry::open(root)?;
        let audit = AuditLog::open(root.join("logs"))
            .map_err(|e| AppError::Internal(format!("cannot open audit log: {e}")))?;
        Ok(Arc::new(Self {
            config,
            store,
            registry,
            audit,
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

    /// Starts a new connection and records a `connect` audit event.
    pub fn open_session(self: &Arc<Self>, conn: ConnInfo) -> AgentSession {
        AgentSession::open(self.clone(), conn)
    }

    pub(crate) fn touch_agent(&self, name: &str) -> Result<()> {
        let _guard = self.registry.lock();
        if let Some(mut profile) = self.registry.agent(name)? {
            profile.last_seen_at = Some(Utc::now());
            self.registry.save_agent(&profile)?;
        }
        Ok(())
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
