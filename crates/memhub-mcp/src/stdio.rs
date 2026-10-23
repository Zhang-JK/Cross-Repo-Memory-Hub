//! MCP stdio transport: newline-delimited JSON-RPC on stdin/stdout.
//! One process serves one agent; logs go to stderr.

use std::{
    io::{self, BufRead, Write},
    sync::Arc,
};

use memhub_app::{clean_instance, ConnInfo, Hub};
use serde_json::Value;

use crate::{
    protocol::{self, error_response, Message},
    server::McpConnection,
};

/// Agent identity applied right after `initialize`.
#[derive(Debug, Clone, Default)]
pub struct StdioLogin {
    pub agent: Option<String>,
    /// Used to create the agent if it does not exist yet.
    pub role: Option<String>,
    /// Label of this running copy, recorded with the connection.
    pub instance: Option<String>,
}

pub fn run(hub: Arc<Hub>, login: StdioLogin) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(hub, login, stdin.lock(), stdout.lock())
}

pub fn serve(
    hub: Arc<Hub>,
    login: StdioLogin,
    input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut conn: Option<McpConnection> = None;

    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let responses = match serde_json::from_str::<Value>(&line) {
            Err(e) => vec![error_response(
                Value::Null,
                protocol::PARSE_ERROR,
                &format!("parse error: {e}"),
            )],
            Ok(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| handle(&hub, &login, &mut conn, v))
                .collect(),
            Ok(v) => handle(&hub, &login, &mut conn, v).into_iter().collect(),
        };
        for response in responses {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
        }
        output.flush()?;
    }

    if let Some(conn) = conn {
        conn.close("stdio_eof");
    }
    Ok(())
}

fn handle(
    hub: &Arc<Hub>,
    login: &StdioLogin,
    conn: &mut Option<McpConnection>,
    value: Value,
) -> Option<Value> {
    let msg = match Message::parse(value) {
        Ok(m) => m,
        Err(err) => return Some(err),
    };
    match conn {
        Some(c) => c.handle(msg),
        None if msg.is_initialize() => {
            let info = ConnInfo {
                transport: "stdio".into(),
                ip: "local".into(),
                client: None,
                instance: login.instance.as_deref().and_then(clean_instance),
            };
            let (mut c, response) = McpConnection::initialize(hub, info, &msg);
            if let Some(agent) = &login.agent {
                if let Err(e) = c.session_mut().connect(agent, login.role.as_deref(), None) {
                    tracing::warn!("automatic connect as '{agent}' failed: {e}");
                }
            }
            *conn = Some(c);
            Some(response)
        }
        None if msg.method.as_deref() == Some("ping") => {
            Some(protocol::result_response(msg.id?, serde_json::json!({})))
        }
        None => msg.is_request().then(|| {
            error_response(
                msg.id.clone().unwrap_or(Value::Null),
                protocol::NOT_INITIALIZED,
                "send initialize first",
            )
        }),
    }
}
