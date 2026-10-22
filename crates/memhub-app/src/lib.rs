//! Application layer: agent registration and login, working repos,
//! permission checks and auditing on top of [`memhub_store`].
//!
//! Access rules:
//! - every memory is readable by every identified agent;
//! - agent memory is writable only by the agent that owns it;
//! - repo memory is writable by agents that added the repo to their working set.

mod diff;
mod error;
mod hub;
mod registry;
mod session;

pub use error::{AppError, Result};
pub use hub::{AuthMode, Hub, HubConfig};
pub use registry::{AgentInfo, AgentProfile, RepoProfile};
pub use session::{AddMemory, AgentSession, ConnInfo, Registration, RepoJoin, RepoSummary, WhoAmI};

pub use memhub_audit as audit;
pub use memhub_store as store;
