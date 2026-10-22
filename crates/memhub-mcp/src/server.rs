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
3. Before changing a repo, `search_memory` (or `list_memories` with scope=repo, owner=<repo>) \
to learn what it does, how it is implemented and what to watch out for.
4. Record durable knowledge with `add_memory` / `update_memory`:
   - scope=repo: facts about a repository (kinds: overview, architecture, implementation, pitfall).
   - scope=agent: your own notebook (kinds: task-log, insight, todo). Other agents can read it \
but only you can change it.
Memory ids look like `repo/<repo>/<slug>` or `agent/<agent>/<slug>`. Pass `expected_version` \
when updating to avoid overwriting concurrent edits.";

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
