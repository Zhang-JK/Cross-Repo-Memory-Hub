//! The curator run: per target, evidence batches become proposals that are
//! checked, reviewed and applied by the hub.
//!
//! ```text
//! curation_status -> for each target with pending evidence:
//!     curation_batch -> LLM drafts ops -> propose_changes(dry_run) -> LLM repairs failed ops
//!     -> propose_changes(cursor) -> review (LLM verifier | auto | human) -> review_changes
//! then, for memories ported from 1.0: curation_backfill -> same steps without a cursor
//! ```

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    config::{Config, ReviewMode},
    llm::{extract_json, Llm},
    mcp::{truncate, McpClient},
};

const CURATE_SYSTEM: &str = r##"You are the curator of Memory Hub, the shared long-term memory of a team of coding agents.
Agents record raw evidence of what they observed while working. You turn that evidence into compact,
well-organized experience memories that later agents search and trust. You only know what the
evidence says about the agents' tasks, so be conservative and faithful.

Kinds of target:
- repo:<id>: general knowledge about one repository that every agent working on it should know.
- agent:<name>: lessons about one agent's own way of working (tricks, workflows, mistakes to avoid).

Rules:
1. Keep reusable knowledge: pitfalls and their fixes, non-obvious behavior, build/test/release/debug
   procedures, design decisions with their reasons, architecture and module responsibilities,
   conventions. For agent targets: workflow lessons, tool tricks, mistakes to avoid.
2. Drop task chatter: progress reports, status updates, to-dos, details that only matter inside one
   task. Answer such evidence with a noop op and a short reason.
3. Never claim more than the evidence supports: no invented versions, numbers, paths or causes.
   Every add or update cites the evidence ids it is based on (cites.evidence / add_cites.evidence)
   and the code locations the evidence names (cites.code, "path:line@commit").
4. Prefer changing existing memory over adding: when a related memory covers the topic, update it
   (its id, expected_version = its version, the full new body) or supersede it when the evidence
   shows it is no longer true. Never create near-duplicates.
5. One claim or procedure per entry. Titles: short, English, ASCII only (they become file names).
   Bodies: concise Markdown for a newcomer, in the language of the evidence. Entries at most 3000
   characters; pages (kinds overview, architecture, implementation, module, guide, runbook) at most
   10000.
6. Repo folders by kind: overview -> root folder "" (one per repo), architecture -> "architecture",
   implementation/module -> "modules/<name>", how-to -> "how-to", pitfall -> "pitfalls",
   decision -> "decisions". Agent folders: insight -> "insights", workflow -> "workflows".
   Never use kinds task-log, todo, plan, progress, status. Never write into folder "working".
7. Status "established" only when two or more independent sources (different agents, or the same
   agent on different tasks) agree, or when the claim is verified against cited code (then also
   give "verified_commit"); otherwise "provisional".
8. A disputed memory: if the evidence shows it is wrong, update or supersede it; if it is right,
   update it with the clarifying evidence and status provisional or established.
9. Missed searches are what agents looked for and did not find. When the evidence answers one, use
   the searched words in the title and tags.
10. Agent targets see all evidence of their agent. Evidence that lists repos is curated into those
    repos' memory separately: for an agent target keep from it only lessons about the agent's own
    way of working, and answer the rest with noop. Evidence without repos that is really general
    knowledge about a repo the agent works on may go to repo memory (scope "repo", owner = repo id).

Answer with one JSON object and nothing else:
{"note": "<one line summary>", "ops": [<operations>]}
Operations:
{"op":"add","scope":"repo","owner":"<repo id or agent name>","folder":"pitfalls","title":"...","kind":"pitfall","body":"...","tags":["..."],"applies_to":["src/refunds/**"],"cites":{"evidence":["evidence/..."],"code":["src/refunds/webhook.rs:42"]},"status":"provisional"}
{"op":"update","id":"<memory id>","expected_version":3,"body":"<full new body>","add_cites":{"evidence":["evidence/..."]},"status":"established"}   (title, kind, tags, applies_to are optional)
{"op":"supersede","id":"<old memory id>","by":"#0","reason":"..."}   ("#0" = the add op at index 0 of this answer, or an existing memory id)
{"op":"retire","id":"<memory id>","reason":"..."}
{"op":"describe_folder","scope":"repo","owner":"<repo id>","folder":"pitfalls","description":"<one line>"}
{"op":"noop","evidence":["evidence/..."],"reason":"..."}
Every evidence id of the batch must appear in at least one operation (in cites or in a noop)."##;

const BACKFILL_SYSTEM: &str = r##"You are the curator of Memory Hub, the shared long-term memory of a team of coding agents.
The memories below were ported from Memory Hub 1.0, where agents wrote memory directly. They may be
long, mix several topics, contain task chatter or be misfiled. Each one cites its original text as
legacy evidence (cites.evidence). Review every memory and answer with exactly one decision for it:

- keep: {"op":"update","id":"<id>","expected_version":<version>} optionally with a better "title",
  "kind", "tags" or "applies_to". This marks it reviewed.
- clean up: the same update with a new "body" that keeps every correct, reusable fact and drops task
  chatter. A body that shrinks by more than 40% waits for a human, so for big rewrites split instead.
- split: one add per topic, each citing the memory's legacy evidence id in cites.evidence, then
  {"op":"supersede","id":"<old id>","by":"#<index of the main new add>","reason":"split into ..."}.
- move: {"op":"move","id":"<id>","folder":"<better folder>"} (also marks it reviewed).
- retire: {"op":"retire","id":"<id>","reason":"..."} when it is only task state, obsolete or useless.

Rules: never invent facts; titles short English ASCII; entries at most 3000 characters, pages (kinds
overview, architecture, implementation, module, guide, runbook) at most 10000; repo folders by kind:
architecture, modules/<name>, how-to, pitfalls, decisions, overview in the root; agent folders:
insights, workflows; never kinds task-log, todo, plan, progress, status; never folder "working".
A memory with hint "reclassify" probably has the wrong kind or space: agent memories holding general
repo knowledge may be split into repo memory (scope "repo") for repos the agent works on.
Status stays "provisional" unless the memory was read often and is clearly still true; then
"established" is allowed only with code citations and "verified_commit".

Answer with one JSON object and nothing else: {"note": "<one line>", "ops": [<operations>]}"##;

const REVIEW_SYSTEM: &str = r##"You review changes proposed for Memory Hub, the shared long-term memory of a team of coding agents.
For each operation decide whether to apply it. Approve when:
- every claim is supported by the cited evidence or by the memory being revised (no invented facts,
  versions, numbers, paths or causes);
- it is reusable knowledge, not task chatter or status;
- it does not duplicate another memory shown;
- folder, kind and title fit;
- an update keeps the correct content of the current body.
Otherwise reject it with a short reason. noop and describe_folder operations are approved unless
clearly wrong.

Answer with one JSON object and nothing else:
{"verdicts": [{"index": 0, "approve": true, "reason": "<short>"}]}"##;

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Only these targets (`repo:<id>`, `agent:<name>`); empty for the config's list or all.
    pub targets: Vec<String>,
    /// Draft and check proposals but store nothing.
    pub dry_run: bool,
    /// Override `curation.backfill`.
    pub backfill: Option<bool>,
    /// Only review 1.0 memories; skip new evidence.
    pub backfill_only: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TargetReport {
    pub target: String,
    pub batches: usize,
    pub evidence: usize,
    pub backfill_reviewed: usize,
    pub proposals: Vec<String>,
    pub ops: usize,
    pub applied: usize,
    pub rejected: usize,
    pub waiting_for_human: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Dry runs: the drafted operations with their checks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub drafts: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CurateReport {
    pub started_at: String,
    pub finished_at: String,
    pub dry_run: bool,
    pub review: String,
    pub targets: Vec<TargetReport>,
}

impl CurateReport {
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for t in &self.targets {
            out.push_str(&format!(
                "{:<32} batches={} evidence={} backfill={} ops={} applied={} rejected={} waiting_for_human={}{}\n",
                t.target,
                t.batches,
                t.evidence,
                t.backfill_reviewed,
                t.ops,
                t.applied,
                t.rejected,
                t.waiting_for_human,
                if t.errors.is_empty() {
                    String::new()
                } else {
                    format!(" errors={}", t.errors.len())
                }
            ));
            for e in &t.errors {
                out.push_str(&format!("    error: {e}\n"));
            }
        }
        if self.targets.is_empty() {
            out.push_str("nothing to curate\n");
        }
        out
    }
}

/// Connects to the 2.0 hub as curator.
pub fn connect_hub(config: &Config) -> Result<McpClient> {
    config.validate_hub()?;
    let headers = vec![
        ("x-memhub-agent".to_string(), config.hub.agent.clone()),
        ("x-memhub-role".to_string(), "curator".to_string()),
        (
            "x-memhub-curator-token".to_string(),
            config.hub.curator_token.clone(),
        ),
        (
            "x-memhub-instance".to_string(),
            "memhub2-curator".to_string(),
        ),
    ];
    let mut hub = McpClient::connect(
        &config.hub.url,
        headers,
        std::time::Duration::from_secs(120),
    )
    .context("cannot connect to the Memory Hub 2.0 server")?;
    if hub.server_name != "memhub2" {
        bail!(
            "{} is '{}' {}, not a Memory Hub 2.0 server (memhub2)",
            config.hub.url,
            hub.server_name,
            hub.server_version
        );
    }
    if !hub.tool_names()?.iter().any(|t| t == "curation_batch") {
        bail!("the hub did not grant a curator session; check hub.curator_token");
    }
    Ok(hub)
}

pub struct Curator<'a> {
    hub: &'a mut McpClient,
    llm: &'a dyn Llm,
    config: &'a Config,
    options: RunOptions,
}

impl<'a> Curator<'a> {
    pub fn new(
        hub: &'a mut McpClient,
        llm: &'a dyn Llm,
        config: &'a Config,
        options: RunOptions,
    ) -> Self {
        Self {
            hub,
            llm,
            config,
            options,
        }
    }

    pub fn run(&mut self) -> Result<CurateReport> {
        let mut report = CurateReport {
            started_at: chrono::Utc::now().to_rfc3339(),
            dry_run: self.options.dry_run,
            review: format!("{:?}", self.config.curation.review).to_lowercase(),
            ..Default::default()
        };
        let status = self.hub.call_tool("curation_status", json!({}))?;
        let wanted: BTreeSet<String> = if self.options.targets.is_empty() {
            self.config.curation.targets.iter().cloned().collect()
        } else {
            self.options.targets.iter().cloned().collect()
        };
        let backfill = self
            .options
            .backfill
            .unwrap_or(self.config.curation.backfill);
        for t in status["targets"].as_array().cloned().unwrap_or_default() {
            let target = t["target"].as_str().unwrap_or("").to_string();
            if target.is_empty() || (!wanted.is_empty() && !wanted.contains(&target)) {
                continue;
            }
            let pending = t["pending_evidence"].as_u64().unwrap_or(0);
            let to_backfill = t["backfill"].as_u64().unwrap_or(0);
            let do_evidence = pending > 0 && !self.options.backfill_only;
            let do_backfill = backfill && to_backfill > 0;
            if !do_evidence && !do_backfill {
                continue;
            }
            let mut tr = TargetReport {
                target: target.clone(),
                ..Default::default()
            };
            tracing::info!(target = %target, pending, backfill = to_backfill, "curating");
            if do_evidence {
                if let Err(e) = self.curate_evidence(&target, &mut tr) {
                    tracing::error!(target = %target, "curation failed: {e:#}");
                    tr.errors.push(format!("{e:#}"));
                }
            }
            if do_backfill {
                if let Err(e) = self.curate_backfill(&target, &mut tr) {
                    tracing::error!(target = %target, "backfill failed: {e:#}");
                    tr.errors.push(format!("backfill: {e:#}"));
                }
            }
            report.targets.push(tr);
        }
        report.finished_at = chrono::Utc::now().to_rfc3339();
        Ok(report)
    }

    fn curate_evidence(&mut self, target: &str, tr: &mut TargetReport) -> Result<()> {
        let max = self.config.curation.max_batches_per_target.max(1);
        let batches = if self.options.dry_run { 1 } else { max };
        for _ in 0..batches {
            let batch = self.hub.call_tool(
                "curation_batch",
                json!({ "target": target, "limit": self.config.curation.batch_size }),
            )?;
            let evidence = batch["evidence"].as_array().cloned().unwrap_or_default();
            if evidence.is_empty() {
                break;
            }
            tr.batches += 1;
            tr.evidence += evidence.len();
            let related = self.related_memories(&evidence)?;
            let prompt = self.batch_prompt(&batch, &related);
            let cursor = batch["cursor"].as_str().map(str::to_string);
            self.draft_and_submit(
                target,
                CURATE_SYSTEM,
                &prompt,
                cursor,
                &evidence,
                &related,
                tr,
            )?;
            if batch["more"].as_bool() != Some(true) {
                break;
            }
        }
        Ok(())
    }

    fn curate_backfill(&mut self, target: &str, tr: &mut TargetReport) -> Result<()> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let max = self.config.curation.max_batches_per_target.max(1);
        for _ in 0..max {
            let batch = self.hub.call_tool(
                "curation_backfill",
                json!({ "target": target, "limit": self.config.curation.backfill_batch_size.max(1) }),
            )?;
            let memories = batch["memories"].as_array().cloned().unwrap_or_default();
            let ids: BTreeSet<String> = memories
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect();
            if ids.is_empty() || ids.is_subset(&seen) {
                break;
            }
            seen.extend(ids.iter().cloned());
            tr.backfill_reviewed += memories.len();
            let prompt = self.backfill_prompt(&batch);
            let related: BTreeMap<String, Value> = memories
                .iter()
                .filter_map(|m| Some((m["id"].as_str()?.to_string(), m.clone())))
                .collect();
            self.draft_and_submit(target, BACKFILL_SYSTEM, &prompt, None, &[], &related, tr)?;
            if self.options.dry_run || batch["remaining"].as_u64() == Some(0) {
                break;
            }
        }
        Ok(())
    }

    /// Full text of the memories related to a batch, for updates.
    fn related_memories(&mut self, evidence: &[Value]) -> Result<BTreeMap<String, Value>> {
        let mut out = BTreeMap::new();
        for e in evidence {
            for r in e["related"].as_array().into_iter().flatten() {
                let Some(id) = r["id"].as_str() else { continue };
                if out.contains_key(id) || out.len() >= 15 {
                    continue;
                }
                match self.hub.call_tool("get_memory", json!({ "id": id })) {
                    Ok(m) => {
                        out.insert(id.to_string(), m);
                    }
                    Err(e) => tracing::warn!("cannot read related memory {id}: {e:#}"),
                }
            }
        }
        Ok(out)
    }

    fn per_item_chars(&self, items: usize) -> usize {
        let budget = self.config.llm.max_prompt_chars.max(4000);
        (budget / (items.max(1) * 2)).clamp(300, 4000)
    }

    fn batch_prompt(&self, batch: &Value, related: &BTreeMap<String, Value>) -> String {
        let evidence = batch["evidence"].as_array().cloned().unwrap_or_default();
        let limit = self.per_item_chars(evidence.len() + related.len());
        let mut p = String::new();
        p.push_str(&format!(
            "Target: {}\nDescription: {}\nExperience memories in the target: {}\n\nFolders:\n",
            batch["target"].as_str().unwrap_or(""),
            batch["description"].as_str().unwrap_or(""),
            batch["experience_count"]
        ));
        for f in batch["folders"].as_array().into_iter().flatten() {
            let path = f["path"].as_str().unwrap_or("");
            p.push_str(&format!(
                "- {} ({} memories){}\n",
                if path.is_empty() { "(root)" } else { path },
                f["total_count"],
                f["description"]
                    .as_str()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            ));
        }
        p.push_str(
            "\nRelated existing memories (update or supersede these instead of duplicating):\n",
        );
        if related.is_empty() {
            p.push_str("(none)\n");
        }
        for (id, m) in related {
            p.push_str(&format!(
                "\n### {id}\ntitle: {}\nkind: {}\nstatus: {}\nversion: {}\ncites: {}\nbody:\n{}\n",
                m["title"].as_str().unwrap_or(""),
                m["kind"].as_str().unwrap_or(""),
                m["status"].as_str().unwrap_or("(none)"),
                m["version"],
                m.get("cites")
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "{}".into()),
                truncate(m["body"].as_str().unwrap_or(""), limit)
            ));
        }
        p.push_str("\nNew evidence (oldest first):\n");
        for e in &evidence {
            p.push_str(&format!("\n### {}\n", e["id"].as_str().unwrap_or("")));
            for key in [
                "title",
                "kind",
                "author",
                "created_at",
                "repos",
                "task",
                "outcome",
                "source",
                "importance",
                "reason",
                "candidate_lesson",
                "code_refs",
                "derived_from",
                "supersedes",
                "tags",
            ] {
                if let Some(v) = e.get(key).filter(|v| !v.is_null()) {
                    let text = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    p.push_str(&format!("{key}: {text}\n"));
                }
            }
            let rel: Vec<&str> = e["related"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["id"].as_str())
                .collect();
            if !rel.is_empty() {
                p.push_str(&format!("related: {}\n", rel.join(", ")));
            }
            p.push_str(&format!(
                "body:\n{}\n",
                truncate(e["body"].as_str().unwrap_or(""), limit)
            ));
        }
        let missed = batch["missed_queries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !missed.is_empty() {
            p.push_str("\nSearches that found nothing (query x count):\n");
            for m in missed {
                p.push_str(&format!(
                    "- \"{}\" x{}\n",
                    m["query"].as_str().unwrap_or(""),
                    m["count"]
                ));
            }
        }
        let feedback = batch["feedback"].as_array().cloned().unwrap_or_default();
        if !feedback.is_empty() {
            p.push_str("\nNegative feedback and disputes on existing memories:\n");
            for f in feedback {
                p.push_str(&format!(
                    "- {} (status {}, version {}): {}\n",
                    f["id"].as_str().unwrap_or(""),
                    f["status"].as_str().unwrap_or("none"),
                    f["version"],
                    f["feedback"]
                ));
            }
        }
        cap(p, self.config.llm.max_prompt_chars)
    }

    fn backfill_prompt(&self, batch: &Value) -> String {
        let memories = batch["memories"].as_array().cloned().unwrap_or_default();
        let limit = self.per_item_chars(memories.len()).max(2000);
        let mut p = format!(
            "Target: {}\nDescription: {}\n\nFolders:\n",
            batch["target"].as_str().unwrap_or(""),
            batch["description"].as_str().unwrap_or("")
        );
        for f in batch["folders"].as_array().into_iter().flatten() {
            let path = f["path"].as_str().unwrap_or("");
            p.push_str(&format!(
                "- {}{}\n",
                if path.is_empty() { "(root)" } else { path },
                f["description"]
                    .as_str()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            ));
        }
        p.push_str("\nMemories ported from 1.0:\n");
        for m in &memories {
            p.push_str(&format!(
                "\n### {}\ntitle: {}\nkind: {}\nversion: {}\nread_count (1.0 + 2.0): {}\ncites: {}\ntags: {}\n{}body:\n{}\n",
                m["id"].as_str().unwrap_or(""),
                m["title"].as_str().unwrap_or(""),
                m["kind"].as_str().unwrap_or(""),
                m["version"],
                m["read_count"],
                m.get("cites").map(|c| c.to_string()).unwrap_or_default(),
                m.get("tags").map(|c| c.to_string()).unwrap_or_else(|| "[]".into()),
                m["curation_hint"].as_str().map(|h| format!("hint: {h}\n")).unwrap_or_default(),
                truncate(m["body"].as_str().unwrap_or(""), limit)
            ));
        }
        cap(p, self.config.llm.max_prompt_chars)
    }

    /// Asks the LLM for ops, lets it repair failed checks once, submits the
    /// proposal and reviews it.
    #[allow(clippy::too_many_arguments)]
    fn draft_and_submit(
        &mut self,
        target: &str,
        system: &str,
        prompt: &str,
        cursor: Option<String>,
        evidence: &[Value],
        related: &BTreeMap<String, Value>,
        tr: &mut TargetReport,
    ) -> Result<()> {
        let mut draft = self
            .ask_json(system, prompt)
            .context("drafting operations")?;
        let mut ops = draft["ops"].as_array().cloned().unwrap_or_default();
        let mut checks = self.dry_check(target, &ops)?;
        let failed: Vec<String> = failures(&checks);
        if !failed.is_empty() {
            let repair = format!(
                "{prompt}\n\nYour previous answer:\n{}\n\nThese operations failed the hub's checks:\n{}\n\n\
                 Return the corrected complete JSON object (all operations, fixed or removed).",
                serde_json::to_string(&draft).unwrap_or_default(),
                failed.join("\n")
            );
            match self.ask_json(system, &repair) {
                Ok(fixed) => {
                    let fixed_ops = fixed["ops"].as_array().cloned().unwrap_or_default();
                    let fixed_checks = self.dry_check(target, &fixed_ops)?;
                    if failures(&fixed_checks).len() <= failed.len() {
                        draft = fixed;
                        ops = fixed_ops;
                        checks = fixed_checks;
                    }
                }
                Err(e) => tracing::warn!("repair round failed: {e:#}"),
            }
        }
        if self.options.dry_run {
            tr.ops += ops.len();
            tr.drafts
                .push(json!({ "note": draft["note"], "ops": ops, "checks": checks }));
            return Ok(());
        }
        // Ops that are not even well-formed are dropped (and reported) so the
        // rest of the batch is not lost.
        let (ops, malformed) = split_malformed(ops, &checks);
        for m in malformed {
            tr.errors.push(format!("dropped malformed op: {m}"));
        }
        let proposal = self.hub.call_tool(
            "propose_changes",
            json!({
                "target": target,
                "ops": ops,
                "cursor": cursor,
                "note": draft["note"].as_str().unwrap_or("").chars().take(300).collect::<String>(),
            }),
        )?;
        let id = proposal["id"].as_str().unwrap_or("").to_string();
        tr.proposals.push(id.clone());
        tr.ops += ops.len();
        if ops.is_empty() {
            return Ok(());
        }
        let verdicts = self.review(&proposal, evidence, related)?;
        let reviewed = if verdicts.is_empty() {
            proposal
        } else {
            self.hub.call_tool(
                "review_changes",
                json!({ "proposal_id": id, "verdicts": verdicts }),
            )?
        };
        for d in reviewed["decisions"].as_array().into_iter().flatten() {
            if d["approved"].as_bool() == Some(true) {
                tr.applied += 1;
            } else {
                tr.rejected += 1;
                if let Some(e) = d["error"].as_str() {
                    tr.errors.push(format!("{id} op {}: {e}", d["index"]));
                }
            }
        }
        let decided: BTreeSet<u64> = reviewed["decisions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["index"].as_u64())
            .collect();
        tr.waiting_for_human += (0..ops.len() as u64)
            .filter(|i| !decided.contains(i))
            .count();
        Ok(())
    }

    fn dry_check(&mut self, target: &str, ops: &[Value]) -> Result<Vec<Value>> {
        let mut checks = Vec::new();
        // Malformed ops make the whole call fail; check them one by one then.
        match self.hub.call_tool(
            "propose_changes",
            json!({ "target": target, "ops": ops, "dry_run": true }),
        ) {
            Ok(r) => return Ok(r["checks"].as_array().cloned().unwrap_or_default()),
            Err(e) if ops.len() <= 1 => {
                checks.push(json!({ "index": 0, "ok": false, "malformed": true, "errors": [format!("{e:#}")] }));
                return Ok(checks);
            }
            Err(_) => {}
        }
        for (i, op) in ops.iter().enumerate() {
            let mut c = match self.hub.call_tool(
                "propose_changes",
                json!({ "target": target, "ops": [op], "dry_run": true }),
            ) {
                Ok(r) => r["checks"][0].clone(),
                Err(e) => json!({ "ok": false, "malformed": true, "errors": [format!("{e:#}")] }),
            };
            c["index"] = json!(i);
            checks.push(c);
        }
        Ok(checks)
    }

    fn review(
        &mut self,
        proposal: &Value,
        evidence: &[Value],
        related: &BTreeMap<String, Value>,
    ) -> Result<Vec<Value>> {
        let ops = proposal["ops"].as_array().cloned().unwrap_or_default();
        let checks = proposal["checks"].as_array().cloned().unwrap_or_default();
        let mut verdicts = Vec::new();
        let mut to_judge = Vec::new();
        for (i, c) in checks.iter().enumerate() {
            if c["ok"].as_bool() != Some(true) {
                verdicts.push(json!({
                    "index": i,
                    "approve": false,
                    "reason": format!("failed checks: {}", c["errors"]),
                }));
            } else if c["needs_human"].as_bool() == Some(true) {
                // Left undecided: only `memhub2 proposals approve` applies it.
            } else {
                to_judge.push(i);
            }
        }
        match self.config.curation.review {
            ReviewMode::Human => return Ok(Vec::new()),
            ReviewMode::Auto => {
                for i in to_judge {
                    verdicts.push(
                        json!({ "index": i, "approve": true, "reason": "auto: passed checks" }),
                    );
                }
            }
            ReviewMode::Llm if to_judge.is_empty() => {}
            ReviewMode::Llm => {
                let prompt = review_prompt(
                    &ops,
                    &checks,
                    &to_judge,
                    evidence,
                    related,
                    self.config.llm.max_prompt_chars,
                );
                let answer = self
                    .ask_json(REVIEW_SYSTEM, &prompt)
                    .context("reviewing the proposal")?;
                let given: BTreeMap<u64, Value> = answer["verdicts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| Some((v["index"].as_u64()?, v.clone())))
                    .collect();
                for i in to_judge {
                    // An op the verifier skipped stays undecided rather than approved.
                    if let Some(v) = given.get(&(i as u64)) {
                        verdicts.push(json!({
                            "index": i,
                            "approve": v["approve"].as_bool() == Some(true),
                            "reason": v["reason"].as_str().unwrap_or("").chars().take(500).collect::<String>(),
                        }));
                    }
                }
            }
        }
        Ok(verdicts)
    }

    /// One LLM call expecting JSON, retried once when the answer has none.
    fn ask_json(&self, system: &str, user: &str) -> Result<Value> {
        let answer = self.llm.complete(system, user)?;
        match extract_json(&answer) {
            Ok(v) => Ok(v),
            Err(first) => {
                tracing::warn!("LLM answer had no JSON ({first}); asking again");
                let retry = format!(
                    "{user}\n\nYour previous answer was not a JSON object. Answer with the JSON object only."
                );
                extract_json(&self.llm.complete(system, &retry)?)
            }
        }
    }
}

fn failures(checks: &[Value]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c["ok"].as_bool() != Some(true))
        .map(|c| {
            format!(
                "ops[{}] ({}): {}",
                c["index"],
                c["op"].as_str().unwrap_or("?"),
                c["errors"]
            )
        })
        .collect()
}

fn split_malformed(ops: Vec<Value>, checks: &[Value]) -> (Vec<Value>, Vec<String>) {
    let malformed: BTreeSet<u64> = checks
        .iter()
        .filter(|c| c["malformed"].as_bool() == Some(true))
        .filter_map(|c| c["index"].as_u64())
        .collect();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (i, op) in ops.into_iter().enumerate() {
        if malformed.contains(&(i as u64)) {
            dropped.push(truncate(&op.to_string(), 300));
        } else {
            kept.push(op);
        }
    }
    (kept, dropped)
}

fn review_prompt(
    ops: &[Value],
    checks: &[Value],
    indexes: &[usize],
    evidence: &[Value],
    related: &BTreeMap<String, Value>,
    max_chars: usize,
) -> String {
    let mut cited: BTreeSet<String> = BTreeSet::new();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let mut p = String::from("Operations to judge:\n");
    for &i in indexes {
        let op = &ops[i];
        for key in ["cites", "add_cites"] {
            for e in op[key]["evidence"].as_array().into_iter().flatten() {
                if let Some(e) = e.as_str() {
                    cited.insert(e.to_string());
                }
            }
        }
        for e in op["evidence"].as_array().into_iter().flatten() {
            if let Some(e) = e.as_str() {
                cited.insert(e.to_string());
            }
        }
        if let Some(id) = op["id"].as_str() {
            touched.insert(id.to_string());
        }
        p.push_str(&format!(
            "\n[{i}] {}\nchecks: support={} status={} warnings={}\n",
            op, checks[i]["support"], checks[i]["status"], checks[i]["warnings"]
        ));
    }
    let limit = (max_chars / ((cited.len() + touched.len()).max(1) * 2)).clamp(300, 3000);
    p.push_str("\nCited evidence:\n");
    for e in evidence {
        let id = e["id"].as_str().unwrap_or("");
        if cited.contains(id) {
            p.push_str(&format!(
                "\n### {id}\nauthor: {} task: {} outcome: {} source: {}\nreason: {}\nlesson: {}\n{}\n",
                e["author"].as_str().unwrap_or(""),
                e["task"].as_str().unwrap_or("-"),
                e["outcome"].as_str().unwrap_or(""),
                e["source"].as_str().unwrap_or(""),
                e["reason"].as_str().unwrap_or("-"),
                e["candidate_lesson"].as_str().unwrap_or("-"),
                truncate(e["body"].as_str().unwrap_or(""), limit)
            ));
        }
    }
    p.push_str("\nCurrent memories touched by the operations (or shown to the drafter):\n");
    for (id, m) in related {
        if touched.contains(id)
            || cited.iter().any(|c| {
                m["cites"]["evidence"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|e| e == c))
            })
        {
            p.push_str(&format!(
                "\n### {id} (version {})\n{}\n",
                m["version"],
                truncate(m["body"].as_str().unwrap_or(""), limit)
            ));
        }
    }
    let others: Vec<String> = related
        .iter()
        .filter(|(id, _)| !touched.contains(*id))
        .map(|(id, m)| format!("- {id}: {}", m["title"].as_str().unwrap_or("")))
        .collect();
    if !others.is_empty() {
        p.push_str(&format!(
            "\nOther existing memories (check for duplicates):\n{}\n",
            others.join("\n")
        ));
    }
    cap(p, max_chars)
}

fn cap(mut p: String, max_chars: usize) -> String {
    if p.chars().count() > max_chars.max(4000) {
        p = p.chars().take(max_chars.max(4000)).collect();
        p.push_str("\n[... cut to fit the prompt budget]\n");
    }
    p
}
