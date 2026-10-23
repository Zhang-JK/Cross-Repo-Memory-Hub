//! MCP Streamable HTTP transport (single `/mcp` endpoint).
//!
//! - `POST /mcp` carries JSON-RPC messages. `initialize` opens a session and
//!   returns its id in the `Mcp-Session-Id` header; later requests must send it.
//!   Responses are plain `application/json` (no server-initiated streams).
//! - `DELETE /mcp` ends the session (audited as a disconnect).
//! - `GET /mcp` returns 405: this server never pushes messages.
//!
//! Sessions idle for longer than the configured timeout are closed and
//! audited with reason `idle_timeout`.
//!
//! The same router serves the management UI ([`crate::admin`]) and `/health`.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use memhub_app::{ConnInfo, Hub};
use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    protocol::{self, error_response, Message},
    server::McpConnection,
};

pub const SESSION_HEADER: &str = "mcp-session-id";
/// Optional identification at `initialize` time (same as calling `connect_agent`).
pub const AGENT_HEADER: &str = "x-memhub-agent";
/// Role used to create the agent if `x-memhub-agent` names a new one.
pub const ROLE_HEADER: &str = "x-memhub-role";

#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Take the client IP from `X-Forwarded-For` / `X-Real-IP`.
    pub trust_proxy: bool,
    /// Extra browser origins allowed besides localhost. Requests without an
    /// `Origin` header (non-browser clients) are always allowed.
    pub allowed_origins: Vec<String>,
    pub idle_timeout: Duration,
    /// Required (as `Authorization: Bearer <token>`) by the admin API when set.
    pub admin_token: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            trust_proxy: false,
            allowed_origins: Vec::new(),
            idle_timeout: Duration::from_secs(30 * 60),
            admin_token: None,
        }
    }
}

/// A currently open MCP session, as shown in the admin UI.
#[derive(Debug, Clone, Serialize)]
pub struct LiveSession {
    pub session_id: String,
    pub agent: Option<String>,
    pub ip: String,
    pub client: Option<String>,
    pub protocol_version: Option<&'static str>,
    pub started_at: Option<DateTime<Utc>>,
    pub idle_secs: Option<u64>,
    /// A request is being processed; details are unavailable until it ends.
    pub busy: bool,
}

type SharedConnection = Arc<tokio::sync::Mutex<McpConnection>>;

pub struct HttpState {
    hub: Arc<Hub>,
    config: HttpConfig,
    sessions: Mutex<HashMap<String, SharedConnection>>,
    started_at: DateTime<Utc>,
}

impl HttpState {
    pub fn new(hub: Arc<Hub>, config: HttpConfig) -> Arc<Self> {
        Arc::new(Self {
            hub,
            config,
            sessions: Mutex::new(HashMap::new()),
            started_at: Utc::now(),
        })
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    pub fn config(&self) -> &HttpConfig {
        &self.config
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn live_sessions(&self) -> Vec<LiveSession> {
        let now = Instant::now();
        let mut out: Vec<LiveSession> = self
            .sessions()
            .iter()
            .map(|(id, conn)| match conn.try_lock() {
                Ok(c) => LiveSession {
                    session_id: id.clone(),
                    agent: c.session().agent().map(str::to_string),
                    ip: c.session().conn().ip.clone(),
                    client: c.session().conn().client.clone(),
                    protocol_version: Some(c.protocol_version()),
                    started_at: Some(c.session().started_at()),
                    idle_secs: Some(now.duration_since(c.last_seen()).as_secs()),
                    busy: false,
                },
                Err(_) => LiveSession {
                    session_id: id.clone(),
                    agent: None,
                    ip: String::new(),
                    client: None,
                    protocol_version: None,
                    started_at: None,
                    idle_secs: None,
                    busy: true,
                },
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        out
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SharedConnection>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn session_count(&self) -> usize {
        self.sessions().len()
    }

    /// Closes sessions idle for longer than the timeout. Busy sessions are
    /// skipped (they are, by definition, not idle).
    pub fn reap_idle(&self) {
        let now = Instant::now();
        let expired: Vec<SharedConnection> = {
            let mut sessions = self.sessions();
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, conn)| {
                    conn.try_lock()
                        .is_ok_and(|c| now.duration_since(c.last_seen()) > self.config.idle_timeout)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for conn in expired {
            close_shared(conn, "idle_timeout");
        }
    }

    /// Closes every open session (on server shutdown).
    pub fn close_all(&self, reason: &str) {
        let all: Vec<SharedConnection> = self.sessions().drain().map(|(_, c)| c).collect();
        for conn in all {
            close_shared(conn, reason);
        }
    }
}

/// If an in-flight request still holds the connection, the session's `Drop`
/// logs the disconnect once that request finishes.
fn close_shared(conn: SharedConnection, reason: &str) {
    if let Ok(mutex) = Arc::try_unwrap(conn) {
        mutex.into_inner().close(reason);
    }
}

pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .merge(crate::admin::routes(state.clone()))
        .with_state(state)
}

/// Background task that periodically reaps idle sessions.
pub fn spawn_reaper(state: Arc<HttpState>) -> tokio::task::JoinHandle<()> {
    let period =
        (state.config.idle_timeout / 4).clamp(Duration::from_secs(1), Duration::from_secs(60));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        loop {
            tick.tick().await;
            state.reap_idle();
        }
    })
}

pub(crate) fn client_ip(config: &HttpConfig, headers: &HeaderMap, peer: SocketAddr) -> String {
    if config.trust_proxy {
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
            .map(str::trim)
            .and_then(|ip| ip.parse::<IpAddr>().ok());
        if let Some(ip) = forwarded {
            return ip.to_string();
        }
    }
    peer.ip().to_canonical().to_string()
}

fn origin_allowed(config: &HttpConfig, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        || config.allowed_origins.iter().any(|o| o == origin)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    json_response(status, error_response(Value::Null, code, message))
}

async fn handle_post(
    State(state): State<Arc<HttpState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&state.config, &headers) {
        return rpc_error(
            StatusCode::FORBIDDEN,
            protocol::INVALID_REQUEST,
            "origin not allowed",
        );
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                protocol::PARSE_ERROR,
                &format!("parse error: {e}"),
            )
        }
    };
    let batch = payload.is_array();
    let raw: Vec<Value> = match payload {
        Value::Array(items) if items.is_empty() => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                protocol::INVALID_REQUEST,
                "empty batch",
            )
        }
        Value::Array(items) => items,
        other => vec![other],
    };

    let mut responses = Vec::new();
    let mut messages = Vec::new();
    for value in raw {
        match Message::parse(value) {
            Ok(m) => messages.push(m),
            Err(err) => responses.push(err),
        }
    }

    if messages.iter().any(Message::is_initialize) {
        if batch || messages.len() != 1 {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                protocol::INVALID_REQUEST,
                "initialize must not be part of a batch",
            );
        }
        let msg = messages.pop().expect("one message");
        let ip = client_ip(&state.config, &headers, peer);
        return initialize(state, ip, &headers, msg).await;
    }

    let Some(session_id) = header_str(&headers, SESSION_HEADER) else {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            protocol::NOT_INITIALIZED,
            "missing Mcp-Session-Id header; send initialize first",
        );
    };
    let Some(conn) = state.sessions().get(session_id).cloned() else {
        return rpc_error(
            StatusCode::NOT_FOUND,
            protocol::NOT_INITIALIZED,
            "unknown or expired session; re-initialize",
        );
    };

    let guard = conn.lock_owned().await;
    let handled = tokio::task::spawn_blocking(move || {
        let mut guard = guard;
        messages
            .into_iter()
            .filter_map(|m| guard.handle(m))
            .collect::<Vec<Value>>()
    })
    .await;
    match handled {
        Ok(r) => responses.extend(r),
        Err(e) => {
            tracing::error!("request handler panicked: {e}");
            return rpc_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                protocol::INTERNAL_ERROR,
                "internal error",
            );
        }
    }

    match (responses.len(), batch) {
        (0, _) => StatusCode::ACCEPTED.into_response(),
        (_, true) => json_response(StatusCode::OK, Value::Array(responses)),
        (_, false) => json_response(StatusCode::OK, responses.pop().expect("one response")),
    }
}

async fn initialize(
    state: Arc<HttpState>,
    ip: String,
    headers: &HeaderMap,
    msg: Message,
) -> Response {
    let agent = header_str(headers, AGENT_HEADER).map(str::to_owned);
    let role = header_str(headers, ROLE_HEADER).map(str::to_owned);
    let hub = state.hub.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let conn_info = ConnInfo {
            transport: "http".into(),
            ip,
            client: None,
        };
        let (mut conn, response) = McpConnection::initialize(&hub, conn_info, &msg);
        if let Some(agent) = agent {
            if let Err(e) = conn.session_mut().connect(&agent, role.as_deref(), None) {
                conn.close("connect_failed");
                return Err(e);
            }
        }
        Ok((conn, response))
    })
    .await;

    let (conn, response) = match opened {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                protocol::INVALID_REQUEST,
                &e.to_string(),
            );
        }
        Err(e) => {
            tracing::error!("initialize panicked: {e}");
            return rpc_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                protocol::INTERNAL_ERROR,
                "internal error",
            );
        }
    };

    let session_id = conn.session().id().to_string();
    tracing::info!(
        session = %session_id,
        ip = %conn.session().conn().ip,
        agent = ?conn.session().agent(),
        "mcp session opened"
    );
    state
        .sessions()
        .insert(session_id.clone(), Arc::new(tokio::sync::Mutex::new(conn)));

    let mut res = json_response(StatusCode::OK, response);
    res.headers_mut().insert(
        SESSION_HEADER,
        HeaderValue::from_str(&session_id).expect("session id is ASCII"),
    );
    res
}

async fn handle_delete(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    let Some(session_id) = header_str(&headers, SESSION_HEADER) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(conn) = state.sessions().remove(session_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Wait for any in-flight request on this session before closing it.
    drop(conn.lock().await);
    tokio::task::spawn_blocking(move || close_shared(conn, "client_closed"))
        .await
        .ok();
    StatusCode::NO_CONTENT.into_response()
}

async fn handle_get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
        "this server does not offer a server-to-client event stream",
    )
        .into_response()
}
