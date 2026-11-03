//! Clients that drive a Memory Hub 2.0 server as curator:
//!
//! - [`curate`]: the LLM curator turning evidence into experience;
//! - [`port`]: the Memory Hub 1.0 -> 2.0 migration;
//! - [`config`]: `config/memhub2-curator.yaml` (LLM endpoint, model, token).

pub mod config;
pub mod curate;
pub mod llm;
pub mod mcp;
pub mod port;

pub use config::Config;
pub use curate::{connect_hub, CurateReport, Curator, RunOptions};
pub use llm::{extract_json, Llm, LlmClient};
pub use mcp::McpClient;
pub use port::{PortOptions, PortReport};
