//! Memory Hub 1.0 -> 2.0 port over MCP: reads everything from the 1.0
//! server and imports it through the 2.0 curator tools.
//!
//! 1. agents and repos (`list_agents`, `list_repos` -> `import_registry`);
//! 2. per repo and agent: folders (`browse_memory` -> `import_folders`);
//! 3. every memory (`get_memory` -> `import_memory`), which keeps its text as
//!    legacy evidence and places it by class (see `memhub_app::import`).
//!
//! The port is idempotent: memories already ported are skipped, so it can be
//! re-run after 1.0 received more writes.

use std::{collections::BTreeMap, time::Duration};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{config::Config, mcp::McpClient};

#[derive(Debug, Clone, Default)]
pub struct PortOptions {
    /// Report placements without writing anything to 2.0.
    pub dry_run: bool,
    /// Only these owners (`repo:<id>` / `agent:<name>`); empty for all.
    pub only: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PortReport {
    pub v1_url: String,
    pub v2_url: String,
    pub dry_run: bool,
    pub agents_created: Vec<String>,
    pub agents_updated: Vec<String>,
    pub repos_created: Vec<String>,
    pub folders: usize,
    pub memories: usize,
    /// Count per class: experience, work_note, evidence_only.
    pub by_class: BTreeMap<String, usize>,
    pub skipped: usize,
    /// Memories flagged for the curator, e.g. `reclassify`.
    pub hinted: Vec<String>,
    pub errors: Vec<String>,
    pub items: Vec<Value>,
}

impl PortReport {
    pub fn summary(&self) -> String {
        let classes: Vec<String> = self
            .by_class
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!(
            "{}{} memories from {} -> {}: {} (skipped {}), {} folders, agents created {} / updated {}, repos created {}, {} flagged for reclassification, {} errors\n",
            if self.dry_run { "[dry run] " } else { "" },
            self.memories,
            self.v1_url,
            self.v2_url,
            if classes.is_empty() { "-".to_string() } else { classes.join(" ") },
            self.skipped,
            self.folders,
            self.agents_created.len(),
            self.agents_updated.len(),
            self.repos_created.len(),
            self.hinted.len(),
            self.errors.len(),
        )
    }
}

/// Connects to the 1.0 server as `port.v1_agent`.
pub fn connect_v1(config: &Config) -> Result<McpClient> {
    let headers = vec![
        ("x-memhub-agent".to_string(), config.port.v1_agent.clone()),
        ("x-memhub-role".to_string(), "selftest".to_string()),
        ("x-memhub-instance".to_string(), "memhub2-port".to_string()),
    ];
    let v1 = McpClient::connect(&config.port.v1_url, headers, Duration::from_secs(120))
        .context("cannot connect to the Memory Hub 1.0 server")?;
    if v1.server_name == "memhub2" {
        bail!(
            "{} is a Memory Hub 2.0 server; port.v1_url must point at the 1.0 server",
            config.port.v1_url
        );
    }
    Ok(v1)
}

pub fn run(
    v1: &mut McpClient,
    v2: &mut McpClient,
    config: &Config,
    options: &PortOptions,
) -> Result<PortReport> {
    let mut report = PortReport {
        v1_url: config.port.v1_url.clone(),
        v2_url: config.hub.url.clone(),
        dry_run: options.dry_run,
        ..Default::default()
    };
    let agents = v1.call_tool("list_agents", json!({}))?["agents"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let repos = v1.call_tool("list_repos", json!({}))?["repos"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let internal = |name: &str| name == config.port.v1_agent || name == "memhub-selftest";
    let agents: Vec<Value> = agents
        .into_iter()
        .filter(|a| !a["name"].as_str().is_some_and(internal))
        .collect();
    let wanted = |key: String| options.only.is_empty() || options.only.contains(&key);

    if !options.dry_run {
        let imported = v2.call_tool(
            "import_registry",
            json!({ "agents": agents, "repos": repos }),
        )?;
        let names = |k: &str| -> Vec<String> {
            imported[k]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        report.agents_created = names("agents_created");
        report.agents_updated = names("agents_updated");
        report.repos_created = names("repos_created");
    }

    let mut owners: Vec<(&str, String)> = Vec::new();
    for r in &repos {
        if let Some(id) = r["id"].as_str() {
            owners.push(("repo", id.to_string()));
        }
    }
    for a in &agents {
        if let Some(name) = a["name"].as_str() {
            owners.push(("agent", name.to_string()));
        }
    }
    for (scope, owner) in owners {
        if !wanted(format!("{scope}:{owner}")) {
            continue;
        }
        if let Err(e) = port_owner(v1, v2, scope, &owner, options, &mut report) {
            report.errors.push(format!("{scope}/{owner}: {e:#}"));
        }
    }
    Ok(report)
}

fn port_owner(
    v1: &mut McpClient,
    v2: &mut McpClient,
    scope: &str,
    owner: &str,
    options: &PortOptions,
    report: &mut PortReport,
) -> Result<()> {
    let tree = match v1.call_tool("browse_memory", json!({ "scope": scope, "owner": owner })) {
        Ok(t) => t,
        Err(e) if e.to_string().contains("not_found") || e.to_string().contains("unknown") => {
            return Ok(())
        }
        Err(e) => return Err(e),
    };
    let folders: Vec<Value> = tree["folders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| !f["path"].as_str().unwrap_or("").is_empty())
        .map(|f| json!({ "path": f["path"], "description": f["description"] }))
        .collect();
    if !options.dry_run && !folders.is_empty() {
        let written = v2.call_tool(
            "import_folders",
            json!({ "scope": scope, "owner": owner, "folders": folders }),
        )?;
        report.folders += written["folders"].as_array().map_or(0, Vec::len);
    }
    for m in tree["memories"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        let result = v1
            .call_tool("get_memory", json!({ "id": id }))
            .and_then(|memory| {
                v2.call_tool(
                    "import_memory",
                    json!({ "memory": memory, "dry_run": options.dry_run }),
                )
            });
        match result {
            Ok(outcome) => {
                report.memories += 1;
                if outcome["skipped"].is_string() {
                    report.skipped += 1;
                } else {
                    let class = outcome["class"].as_str().unwrap_or("?").to_string();
                    *report.by_class.entry(class).or_default() += 1;
                }
                if outcome["hints"].as_array().is_some_and(|h| !h.is_empty()) {
                    report.hinted.push(id.to_string());
                }
                report.items.push(outcome);
            }
            Err(e) => report.errors.push(format!("{id}: {e:#}")),
        }
    }
    Ok(())
}
