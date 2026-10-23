//! Application layer: agent identification, working repos, permission
//! checks and auditing on top of [`memhub_store`].
//!
//! Agents are identified by name only, so every user running the same agent
//! shares that agent's memory space.
//!
//! Access rules:
//! - every memory is readable by every identified agent;
//! - agent memory is writable only by the agent that owns it;
//! - repo memory is writable by agents that added the repo to their working set.

pub mod admin;
mod diff;
mod error;
mod hub;
mod registry;
mod session;

pub use admin::{Admin, ADMIN_ACTOR};
pub use error::{AppError, Result};
pub use hub::{Hub, HubConfig};
pub use registry::{AgentProfile, RepoProfile};
pub use session::{
    AddMemory, AgentSession, Browse, ConnInfo, Connected, FolderResult, RepoJoin, RepoSummary,
    WhoAmI,
};

pub use memhub_audit as audit;
pub use memhub_store as store;
