//! Application layer: agent identification, working repos, permission
//! checks and auditing on top of [`memhub_store`].
//!
//! Agents are identified by name only, so every user running the same agent
//! shares that agent's memory space.
//!
//! Memory is layered:
//! - **evidence**: append-only records of what agents observed
//!   ([`AgentSession::record_evidence`]); not part of experience search;
//! - **experience**: curated repo memory and agent experience, each claim
//!   citing evidence; written by the curator ([`curation`]);
//! - **work area**: `agent/<name>/working/...`, the agent's own mutable notes.
//!
//! Access rules:
//! - every memory and evidence record is readable by every identified agent;
//! - agents append evidence about repos they joined, and write their own work area;
//! - only curator sessions write experience.

pub mod admin;
pub mod curation;
mod diff;
mod error;
pub mod feedback;
mod hub;
pub mod import;
pub mod reads;
mod redirects;
mod registry;
mod session;

pub use admin::{Admin, ADMIN_ACTOR};
pub use error::{AppError, Result};
pub use hub::{Hub, HubConfig};
pub use reads::{ReadStat, ReadStats, READ_HISTORY_DAYS};
pub use registry::{AgentProfile, RepoProfile};
pub use feedback::{Feedback, FeedbackStats, Rating};
pub use redirects::Redirects;
pub use session::{
    clean_instance, AddMemory, AgentSession, Browse, ConnInfo, Connected, EvidenceSearch,
    ExperienceHit, Fetched, FolderResult, RecordEvidence, RepoJoin, RepoSummary, SearchOptions,
    WhoAmI, DEFAULT_EVIDENCE_WINDOW_DAYS, MAX_EVIDENCE_WINDOW_DAYS, MAX_INSTANCE_LEN,
};

pub use memhub_audit as audit;
pub use memhub_store as store;
