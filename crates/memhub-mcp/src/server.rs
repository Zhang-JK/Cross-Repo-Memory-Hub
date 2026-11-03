//! Transport-independent MCP connection handling.

use std::{sync::Arc, time::Instant};

use memhub_app::{AgentSession, ConnInfo, Hub};
use serde_json::{json, Value};

use crate::{
    protocol::{self, error_response, result_response, Message},
    tools,
};

pub const SERVER_NAME: &str = "memhub2";
pub const SERVER_TITLE: &str = "Cross-Repo Memory Hub 2.0";

/// Server instructions sent to every client at `initialize`.
pub const INSTRUCTIONS: &str = "\
Cross-Repo Memory Hub 2.0: shared long-term memory for agents working across repositories. \
Memory is layered so it stays small, current and trustworthy as it grows.

THE THREE LAYERS
1. Experience (what you search and read). Curated, compact knowledge, in two kinds of space:
   - repo memory `repo/<repo>/...`: general knowledge about a repository that every agent \
touching it should know (overview, architecture, modules, how-to, pitfalls, decisions);
   - agent experience `agent/<you>/...`: lessons about your own way of working (tricks, \
workflows, mistakes to avoid) that are not about one repo.
   Every experience memory cites where it comes from (`cites.evidence`: evidence ids, \
`cites.code`: code locations) and carries a status:
     established - backed by two or more independent sources or verified against code;
     provisional - one source so far, or ported from Memory Hub 1.0 and not reviewed yet;
     disputed    - an agent reported that it contradicts what it observed; verify first;
     superseded  - replaced (`superseded_by`) or retired; hidden from search by default.
   Only the curator writes experience. You cannot add or edit it directly.
2. Evidence (what you write). Append-only records of what you observed while working: a \
pitfall and its fix, how something really works, a decision, a procedure that worked or failed. \
Write them with `record_evidence`. Evidence is not part of `search_memory`; it is read by id \
(`get_memory` with an `evidence/...` id, e.g. one cited by a memory) or with `search_evidence` \
inside a time window.
3. Work area (your scratch space). `agent/<you>/working/...` holds your own plans, to-dos and \
task notes. Only you (any user running your agent) can change it; it is never curated and never \
shown in other agents' searches. Manage it with add_memory / update_memory / move_memory / \
delete_memory (scope=agent, folder=\"working\" or \"working/<task>\").

The curator (an LLM agent run by the hub owner, usually daily) reads new evidence, compares it \
with existing experience and proposes itemized changes: add an entry, update one, supersede an \
outdated one, or ignore the evidence. Each change must cite evidence and passes deterministic \
checks (independent support, size limits, no task state) before it is applied; large rewrites \
need a human. It does not know your task, so your evidence must carry that context.

GETTING STARTED
1. `connect_agent` with your agent name (pass `role`, e.g. feature-impl, crash-analysis, \
perf-opt, to create a new agent). Your name is your identity: every user running the same agent \
shares its experience and work area.
2. `add_repo` for each repository you work on (registers new repos; required before recording \
evidence about a repo).
3. Before changing a repo: `browse_memory(scope=\"repo\", owner=<repo>)` shows its curated tree; \
read the root overview, then the folders relevant to your task (`get_memory`). Use \
`search_memory` for keywords; it covers repo memory and your own space. To include another \
agent's experience, pass owner=<that agent>.

USING WHAT YOU FIND
- Prefer established memories. Check provisional and disputed ones against the code before \
relying on them; open the cited evidence ids when you need the raw observation.
- `applies_to` lists the code paths a memory is about; check them against the code you see.
- After using a memory, `rate_memory` it: helpful, harmful or outdated (with a note). Ratings \
steer ranking and tell the curator what to fix.
- When a memory is wrong, `dispute_memory` with what you observed instead and code_refs. Do not \
work around it silently.
- Nothing found? `search_evidence` looks at recent raw observations. Once you work it out, record \
it: your search is logged, and the curator prioritizes knowledge that agents looked for and missed.

WHAT TO RECORD (record_evidence)
- Record findings another agent would want: pitfalls and their fixes, non-obvious behavior, \
build/test/release/debug procedures, design decisions and their reasons, how modules fit \
together, and lessons about your own workflow (leave repos empty for those).
- One finding per record, specific and factual: symptom, what you tried, what worked, error \
text, commands. Bodies accept any language.
- Give the context the curator lacks: `task` (one line, stable within a task), `outcome`, \
`reason` (why it matters and to whom), `candidate_lesson` (the lesson in one sentence), \
`code_refs` (\"path:line@commit\"), `importance` 1-5.
- Provenance matters: if you only repeat what a memory said, pass `derived_from=[memory id]` \
(source becomes from_memory and does not count as independent confirmation). If you checked it \
yourself, say source=\"observed\".
- To correct an earlier record of yours, record a new one with `supersedes=<its id>`.
- Do not record routine progress, status updates, to-dos or secrets. Keep task state in your \
work area and delete it when the task is done.

TEXT AND IDS
Titles, folder names, slugs, repo ids and agent names are ASCII only (they become file names); \
write titles in English. Bodies, tags and descriptions accept any Unicode. Memory ids include \
the folder path: `repo/<repo>/[<folder>/]<slug>` or `agent/<agent>/[<folder>/]<slug>`; evidence \
ids look like `evidence/<yyyy-mm-dd>/<agent>/<hhmmss>-<slug>`. Ids of moved memories and of \
memories ported from 1.0 redirect to their new place. Tool results may carry `hints`; follow them.";

/// Extra instructions for sessions holding the curator token.
pub const CURATOR_INSTRUCTIONS: &str = "\n\n\
CURATOR SESSION
You hold the curator token and see the curation tools. Work per target (`repo:<id>` or \
`agent:<name>`): `curation_status` -> `curation_batch` -> `propose_changes` (pass the batch \
cursor) -> `review_changes`. Keep experience compact: prefer updating or superseding an existing \
entry over adding a near-duplicate; one claim per entry; cite every claim; keep task state out. \
Missed searches in a batch show what agents need; feedback and disputes show what to revise. \
Memories ported from 1.0 are reviewed with `curation_backfill`.";

pub fn instructions(curator: bool) -> String {
    if curator {
        format!("{INSTRUCTIONS}{CURATOR_INSTRUCTIONS}")
    } else {
        INSTRUCTIONS.to_string()
    }
}

/// Facts extracted from an `initialize` request.
#[derive(Debug, Clone)]
pub struct InitRequest {
    pub protocol_version: &'static str,
    pub client: Option<String>,
}

impl InitRequest {
    pub fn from_message(msg: &Message) -> Self {
        let params = msg.params.as_ref();
        let requested = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str);
        let client = params.and_then(|p| p.get("clientInfo")).map(|info| {
            let name = info
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match info.get("version").and_then(Value::as_str) {
                Some(v) => format!("{name} {v}"),
                None => name.to_string(),
            }
        });
        Self {
            protocol_version: protocol::negotiate_version(requested),
            client,
        }
    }
}

/// An initialized MCP connection bound to one agent session.
pub struct McpConnection {
    session: AgentSession,
    protocol_version: &'static str,
    last_seen: Instant,
}

impl McpConnection {
    /// Handles `initialize`: opens (and audits) the agent session and builds
    /// the response. `conn` should carry transport and IP; the client name is
    /// filled in from `clientInfo`. `curator` is set when the transport has
    /// checked the curator token.
    pub fn initialize(
        hub: &Arc<Hub>,
        mut conn: ConnInfo,
        msg: &Message,
        curator: bool,
    ) -> (Self, Value) {
        let init = InitRequest::from_message(msg);
        conn.client = init.client.clone();
        let mut session = hub.open_session(conn);
        if curator {
            session.grant_curator();
        }
        let response = result_response(
            msg.id.clone().unwrap_or(Value::Null),
            json!({
                "protocolVersion": init.protocol_version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "title": SERVER_TITLE,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": instructions(curator),
            }),
        );
        (
            Self {
                session,
                protocol_version: init.protocol_version,
                last_seen: Instant::now(),
            },
            response,
        )
    }

    /// Revives an expired session under its old id with the protocol version
    /// it negotiated, re-identifying it as `agent` when it had one. An agent
    /// that no longer exists leaves the session unidentified.
    pub fn resume(
        hub: &Arc<Hub>,
        session_id: &str,
        conn: ConnInfo,
        protocol_version: &str,
        agent: Option<&str>,
        role: Option<&str>,
        curator: bool,
    ) -> Self {
        let mut session = hub.resume_session(session_id, conn);
        if curator {
            session.grant_curator();
        }
        if let Some(agent) = agent {
            if let Err(e) = session.connect(agent, role, None) {
                tracing::warn!(session = %session_id, "cannot restore identity '{agent}': {e}");
            }
        }
        Self {
            session,
            protocol_version: protocol::negotiate_version(Some(protocol_version)),
            last_seen: Instant::now(),
        }
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut AgentSession {
        &mut self.session
    }

    pub fn protocol_version(&self) -> &'static str {
        self.protocol_version
    }

    pub fn last_seen(&self) -> Instant {
        self.last_seen
    }

    pub fn close(self, reason: &str) {
        self.session.close(reason);
    }

    /// Handles one post-initialization message. Returns the response for
    /// requests, `None` for notifications and client responses.
    pub fn handle(&mut self, msg: Message) -> Option<Value> {
        self.last_seen = Instant::now();
        let Some(method) = msg.method.as_deref() else {
            return None; // response to a server→client request; we send none
        };
        let Some(id) = msg.id.clone() else {
            // notifications/initialized, notifications/cancelled, ...
            return None;
        };

        let response = match method {
            "initialize" => error_response(
                id,
                protocol::INVALID_REQUEST,
                "connection is already initialized",
            ),
            "ping" => result_response(id, json!({})),
            "tools/list" => result_response(
                id,
                json!({ "tools": tools::definitions(self.session.is_curator()) }),
            ),
            "tools/call" => self.call_tool(id, msg.params.unwrap_or(Value::Null)),
            other => error_response(
                id,
                protocol::METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            ),
        };
        Some(response)
    }

    fn call_tool(&mut self, id: Value, params: Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return error_response(id, protocol::INVALID_PARAMS, "missing tool name");
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        match tools::call(&mut self.session, name, args) {
            Some(result) => result_response(id, result),
            None => error_response(
                id,
                protocol::INVALID_PARAMS,
                &format!("unknown tool: {name}"),
            ),
        }
    }
}
