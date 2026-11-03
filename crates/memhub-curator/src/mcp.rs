//! Minimal blocking MCP client over Streamable HTTP, enough to drive a
//! Memory Hub (1.0 or 2.0) server: `initialize`, `tools/call`, `DELETE`.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SESSION_HEADER: &str = "mcp-session-id";
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

pub struct McpClient {
    http: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    session: Option<String>,
    next_id: u64,
    /// `serverInfo.name` from `initialize`, e.g. `memhub` or `memhub2`.
    pub server_name: String,
    pub server_version: String,
}

impl McpClient {
    /// Connects and initializes. `headers` go with every request (agent,
    /// role, curator token, ...).
    pub fn connect(url: &str, headers: Vec<(String, String)>, timeout: Duration) -> Result<Self> {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        let mut client = Self {
            http: ureq::Agent::new_with_config(config),
            url: url.trim().to_string(),
            headers,
            session: None,
            next_id: 1,
            server_name: String::new(),
            server_version: String::new(),
        };
        let result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "memhub2-curator", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .with_context(|| format!("cannot initialize MCP session at {url}"))?;
        client.server_name = result["serverInfo"]["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        client.server_version = result["serverInfo"]["version"]
            .as_str()
            .unwrap_or("")
            .to_string();
        client.notify("notifications/initialized")?;
        Ok(client)
    }

    /// Calls a tool and returns its structured result (or its text, parsed
    /// as JSON when possible). A tool error becomes `Err`.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )?;
        let text = result["content"]
            .as_array()
            .and_then(|c| c.iter().find_map(|item| item["text"].as_str()))
            .unwrap_or("")
            .to_string();
        if result["isError"].as_bool() == Some(true) {
            bail!("{name}: {text}");
        }
        if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
            return Ok(structured.clone());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// Names of the tools the server offers this session.
    pub fn tool_names(&mut self) -> Result<Vec<String>> {
        let result = self.request("tools/list", json!({}))?;
        Ok(result["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .post(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?
            .ok_or_else(|| anyhow!("{method}: server sent no response"))?;
        let message = find_response(response, id)
            .ok_or_else(|| anyhow!("{method}: no response with id {id}"))?;
        if let Some(error) = message.get("error") {
            bail!(
                "{method}: {}",
                error["message"].as_str().unwrap_or(&error.to_string())
            );
        }
        Ok(message.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.post(&json!({ "jsonrpc": "2.0", "method": method }))?;
        Ok(())
    }

    fn post(&mut self, body: &Value) -> Result<Option<Value>> {
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(s) = &self.session {
            req = req.header(SESSION_HEADER, s.as_str());
        }
        let mut res = req
            .send(body.to_string())
            .with_context(|| format!("cannot reach {}", self.url))?;
        if let Some(s) = res
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            self.session = Some(s.to_string());
        }
        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = res
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .context("cannot read MCP response")?;
        if status == 202 || (status == 200 && text.trim().is_empty()) {
            return Ok(None);
        }
        let parsed = if content_type.starts_with("text/event-stream") {
            parse_sse(&text)
        } else {
            serde_json::from_str(&text).ok()
        };
        match parsed {
            Some(v) if (200..300).contains(&status) || v.get("error").is_some() => Ok(Some(v)),
            _ => bail!("HTTP {status} from {}: {}", self.url, truncate(&text, 500)),
        }
    }

    /// Ends the session on the server.
    pub fn close(&mut self) {
        if let Some(s) = self.session.take() {
            let mut req = self
                .http
                .delete(&self.url)
                .header(SESSION_HEADER, s.as_str());
            for (k, v) in &self.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let _ = req.call();
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// The JSON-RPC response with `id` from a single message or a batch.
fn find_response(value: Value, id: u64) -> Option<Value> {
    match value {
        Value::Array(items) => items.into_iter().find(|m| m["id"] == json!(id)),
        v if v["id"] == json!(id) || v.get("error").is_some() => Some(v),
        _ => None,
    }
}

/// Collects the JSON-RPC messages of an event stream; a single message is
/// returned as is, several as a batch.
fn parse_sse(text: &str) -> Option<Value> {
    let mut messages = Vec::new();
    let mut data = String::new();
    let flush = |data: &mut String, messages: &mut Vec<Value>| {
        if !data.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                messages.push(v);
            }
            data.clear();
        }
    };
    for line in text.lines() {
        if line.is_empty() {
            flush(&mut data, &mut messages);
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    flush(&mut data, &mut messages);
    match messages.len() {
        0 => None,
        1 => messages.pop(),
        _ => Some(Value::Array(messages)),
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_are_collected() {
        let text = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\n data: \"result\":{}}\n\n: ping\n\n";
        // The second data line continues the first event.
        let v = parse_sse(&text.replace("\n data", "\ndata")).unwrap();
        assert_eq!(v["id"], 1);
        let two = "data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        let v = parse_sse(two).unwrap();
        assert_eq!(find_response(v, 2).unwrap()["id"], 2);
        assert!(parse_sse(": keepalive\n\n").is_none());
    }
}
