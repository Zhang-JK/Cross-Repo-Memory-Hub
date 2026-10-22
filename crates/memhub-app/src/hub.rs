use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use chrono::Utc;
use memhub_audit::{Actor, AuditEvent, AuditLog};
use memhub_store::{validate_name, MemoryStore, Scope};

use crate::{
    error::{AppError, Result},
    registry::{hash_token, new_token, Registry},
    session::{AgentSession, ConnInfo},
};

/// How agents prove their identity when connecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// `register_agent` issues a token that `connect_agent` must present.
    #[default]
    Token,
    /// Agents connect by name only. Suitable for trusted single-user setups.
    Open,
}

impl FromStr for AuthMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "token" => Ok(AuthMode::Token),
            "open" | "none" => Ok(AuthMode::Open),
            _ => Err(format!("unknown auth mode '{s}', expected 'token' or 'open'")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub data_root: PathBuf,
    pub auth: AuthMode,
}

impl HubConfig {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            auth: AuthMode::default(),
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

    /// Admin operation: issues a fresh token for an agent that lost its own.
    pub fn reset_token(&self, name: &str) -> Result<String> {
        validate_name("agent name", name)?;
        let _guard = self.registry.lock();
        let mut profile = self
            .registry
            .agent(name)?
            .ok_or_else(|| AppError::UnknownAgent(name.to_string()))?;
        let token = new_token();
        profile.token_hash = hash_token(&token);
        self.registry.save_agent(&profile)?;
        self.audit.record(AuditEvent::TokenReset {
            actor: Actor {
                session_id: "admin".into(),
                agent: Some(name.to_string()),
                ip: "local".into(),
                transport: "cli".into(),
            },
        });
        Ok(token)
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
