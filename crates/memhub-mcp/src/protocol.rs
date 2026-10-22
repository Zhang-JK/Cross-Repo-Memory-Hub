//! JSON-RPC 2.0 framing used by MCP.

use serde::Deserialize;
use serde_json::{json, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// Server-defined: request rejected because the connection is not initialized.
pub const NOT_INITIALIZED: i64 = -32002;

/// Protocol revisions this server speaks, newest first.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const DEFAULT_VERSION: &str = "2025-06-18";

/// A client→server message: request (has `id`), notification (no `id`),
/// or a response to a server request (has `result`/`error`, ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

impl Message {
    pub fn parse(value: Value) -> Result<Self, Value> {
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let msg: Message = serde_json::from_value(value)
            .map_err(|e| error_response(id.clone(), INVALID_REQUEST, &format!("invalid request: {e}")))?;
        if msg.jsonrpc.as_deref() != Some("2.0") {
            return Err(error_response(id, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
        }
        Ok(msg)
    }

    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    pub fn is_initialize(&self) -> bool {
        self.method.as_deref() == Some("initialize")
    }
}

pub fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_VERSIONS.iter().find(|v| **v == r).copied())
        .unwrap_or(DEFAULT_VERSION)
}
