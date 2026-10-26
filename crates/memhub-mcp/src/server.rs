//! Transport-independent MCP connection handling.

use std::{sync::Arc, time::Instant};

use memhub_app::{AgentSession, ConnInfo, Hub};
use serde_json::{json, Value};

use crate::{
    protocol::{self, error_response, result_response, Message},
    tools,
};

pub const SERVER_NAME: &str = "memhub";

const INSTRUCTIONS: &str = "\
Cross-Repo Memory Hub: shared long-term memory for agents working across repositories.

Getting started:
1. Call `connect_agent` with your agent name. Your name is your identity: every user running \
the same agent shares its memory. If the agent does not exist yet, also pass your role \
(e.g. feature-impl, crash-analysis, perf-opt) to create it.
2. Call `add_repo` for each repository you work on. This registers the repo if it is new and \
grants you write access to its shared memory.
3. Before changing a repo, call `browse_memory` (scope=repo, owner=<repo>) to see its memory \
tree with folder descriptions. Read the root `overview` first, then the folders relevant to your \
task (`get_memory`). Use `search_memory` for keyword lookup across everything.
4. Record what you learn with `add_memory` / `update_memory`, choosing the scope as below.

Repo memory vs agent memory: decide this before every write or update.
- Repo memory (scope=repo, owner=<repo>) is the repository's shared knowledge base. It holds \
general knowledge about the repo that is true regardless of who is working on it or why, and \
that every agent touching the repo should know: what it does, how it is built, tested and \
released, its architecture and modules, conventions, pitfalls, and design decisions. It is \
shared by all agents and writable by every agent that joined the repo, so write it for a \
newcomer: self-contained, current and free of task chatter. Kinds: overview, architecture, \
implementation, how-to, pitfall, decision.
- Agent memory (scope=agent, owner=<your agent name>) is your own work notebook for specific \
tasks: what you were asked to do, what you tried, progress and status, open questions, \
follow-ups, and personal tricks and shortcuts that help you work. Other agents can read it but \
only you can change it. Kinds: task-log, insight, todo.
- Rule of thumb: if another agent starting a different task in this repo tomorrow should know \
it, it is repo memory. If it only makes sense in the context of your task, it is agent memory.
- Examples: \"the build needs protoc 3.21\" goes in repo how-to/. \"Retries cause double \
charges unless an idempotency key is set\" goes in repo pitfalls/. \"Ticket PAY-123: fixed \
refund rounding, tried X first, PR pending review\" goes in agent task-logs/. \"Running only \
the billing tests with -run Billing saves 5 minutes\" goes in agent insights/.
- When a task teaches you something general (a gotcha, a build step, how a module really \
works), record it in repo memory, not only in your task log. Your task log can link to it by id.
- When repo memory is wrong or outdated, fix it with `update_memory`. Don't leave the correction \
in your agent notebook only.
- Never put task logs, status updates or to-dos in repo memory. Never keep general repo \
knowledge only in agent memory.

Keep memory organized as a folder tree, so a newcomer can catch up from `browse_memory` alone:
- A repo's root folder holds only its `overview` memory: what the repo does, how to build and \
test it, and which folder holds what. Everything else goes into a folder.
- Suggested repo layout (create only what you need):
    architecture/      components, data flow, key abstractions
    modules/<name>/    one folder per subsystem, crate, package or service
    how-to/            build, test, release and debugging procedures
    pitfalls/          gotchas, past incidents, things that break easily
    decisions/         why things are the way they are
- Suggested agent layout: task-logs/, insights/, todo/.
- Before `add_memory`, browse the tree and pass the most specific fitting `folder` \
(e.g. folder=\"modules/auth\"). Prefer updating an existing memory over adding a near-duplicate.
- Create a folder (`create_folder`, always with a one-line description) when two or more related \
memories have no fitting home. Keep nesting shallow (2-3 levels).
- When the root or a folder gets crowded (about 10 or more memories) or a memory is misplaced, \
reorganize with `move_memory`, and keep the overview's folder map current.
- Tool results may carry `hints` about organization; follow them.

Text: titles, folder names, slugs, repo ids and agent names are ASCII only, because they \
become file and folder names. Write titles in English (e.g. \"Payment retry policy\"). Memory \
bodies, tags, kinds and descriptions accept any Unicode, so write the content in whatever \
language and notation is clearest: Chinese, Japanese, emoji, symbols, code.

Memory ids include the folder path: `repo/<repo>/[<folder>/]<slug>`, e.g. \
`repo/payment-service/pitfalls/retry-storms`, or `agent/<agent>/[<folder>/]<slug>`. Moving a \
memory changes its id. Pass `expected_version` when updating to avoid overwriting concurrent edits.";

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
    /// filled in from `clientInfo`.
    pub fn initialize(hub: &Arc<Hub>, mut conn: ConnInfo, msg: &Message) -> (Self, Value) {
        let init = InitRequest::from_message(msg);
        conn.client = init.client.clone();
        let session = hub.open_session(conn);
        let response = result_response(
            msg.id.clone().unwrap_or(Value::Null),
            json!({
                "protocolVersion": init.protocol_version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "title": "Cross-Repo Memory Hub",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": INSTRUCTIONS,
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
            "tools/list" => result_response(id, json!({ "tools": tools::definitions() })),
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
