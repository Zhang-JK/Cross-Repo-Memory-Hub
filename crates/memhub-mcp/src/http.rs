//! MCP Streamable HTTP transport (single `/mcp` endpoint).
//!
//! - `POST /mcp` carries JSON-RPC messages. `initialize` opens a session and
//!   returns its id in the `Mcp-Session-Id` header; later requests must send it.
//!   Responses are plain `application/json` (no server-initiated streams).
//! - `DELETE /mcp` ends the session (audited as a disconnect).
//! - `GET /mcp` returns 405: this server never pushes messages.
//!
//! Sessions idle for longer than the configured timeout are closed and
//! audited with reason `idle_timeout`. The agent identity is bound to the
//! session, so an expired session is remembered for
//! [`HttpConfig::resume_ttl`]: when the client uses its id again, the session
//! is reopened under the same id and identity instead of answering 404 (which
//! makes clients start a fresh, unidentified session). Sessions open at
//! shutdown are saved to [`HttpConfig::resume_file`] and resumable after a
//! restart the same way.
//!
//! The same router serves the management UI ([`crate::admin`]) and `/health`.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
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
use memhub_app::{clean_instance, ConnInfo, Hub};
use serde::{Deserialize, Serialize};
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
/// Optional label of the running agent copy (e.g. `alice-laptop`), so that
/// several copies of one agent behind one IP can be told apart and counted.
pub const INSTANCE_HEADER: &str = "x-memhub-instance";

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
    /// How long an expired session can be resumed by its id.
    pub resume_ttl: Duration,
    /// Where sessions open at shutdown are saved for resumption after a
    /// restart. `None` keeps them in memory only.
    pub resume_file: Option<PathBuf>,
}

/// Longest list of resumable sessions kept; the oldest are dropped first.
const MAX_RESUMABLE: usize = 10_000;

/// What is needed to reopen an expired session under its old id.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Resumable {
    agent: Option<String>,
    client: Option<String>,
    instance: Option<String>,
    protocol_version: String,
    expired_at: DateTime<Utc>,
}

impl Resumable {
    fn of(conn: &McpConnection) -> Self {
        let session = conn.session();
        Self {
            agent: session.agent().map(str::to_string),
            client: session.conn().client.clone(),
            instance: session.conn().instance.clone(),
            protocol_version: conn.protocol_version().to_string(),
            expired_at: Utc::now(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            trust_proxy: false,
            allowed_origins: Vec::new(),
            idle_timeout: Duration::from_secs(60 * 60),
            admin_token: None,
            resume_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            resume_file: None,
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
    pub instance: Option<String>,
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
    resumable: Mutex<HashMap<String, Resumable>>,
    started_at: DateTime<Utc>,
}

impl HttpState {
    pub fn new(hub: Arc<Hub>, config: HttpConfig) -> Arc<Self> {
        let resumable = config
            .resume_file
            .as_ref()
            .map(load_resumable)
            .unwrap_or_default();
        let state = Arc::new(Self {
            hub,
            config,
            sessions: Mutex::new(HashMap::new()),
            resumable: Mutex::new(resumable),
            started_at: Utc::now(),
        });
        state.prune_resumable();
        state
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
                    instance: c.session().conn().instance.clone(),
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
                    instance: None,
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
        let expired: Vec<(String, SharedConnection)> = {
            let mut sessions = self.sessions();
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, conn)| {
                    conn.try_lock()
                        .is_ok_and(|c| now.duration_since(c.last_seen()) > self.config.idle_timeout)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| sessions.remove_entry(id))
                .collect()
        };
        for (id, conn) in expired {
            self.remember(&id, &conn);
            close_shared(conn, "idle_timeout");
        }
        self.prune_resumable();
    }

    /// Closes every open session (on server shutdown). They stay resumable
    /// and are saved to the resume file, if one is configured.
    /// Also writes pending read counts.
    pub fn close_all(&self, reason: &str) {
        let all: Vec<(String, SharedConnection)> = self.sessions().drain().collect();
        for (id, conn) in all {
            self.remember(&id, &conn);
            close_shared(conn, reason);
        }
        self.prune_resumable();
        if let Some(path) = &self.config.resume_file {
            if let Err(e) = save_resumable(path, &self.resumable_sessions()) {
                tracing::warn!("cannot save resumable sessions to {}: {e}", path.display());
            }
        }
        self.hub.reads().flush();
    }

    /// Number of expired sessions that can still be resumed.
    pub fn resumable_count(&self) -> usize {
        self.resumable_sessions().len()
    }

    fn remember(&self, id: &str, conn: &SharedConnection) {
        if let Ok(c) = conn.try_lock() {
            self.resumable_sessions()
                .insert(id.to_string(), Resumable::of(&c));
        }
    }

    fn prune_resumable(&self) {
        let ttl =
            chrono::Duration::from_std(self.config.resume_ttl).unwrap_or(chrono::Duration::MAX);
        let cutoff = Utc::now() - ttl;
        let mut resumable = self.resumable_sessions();
        resumable.retain(|_, r| r.expired_at > cutoff);
        if resumable.len() > MAX_RESUMABLE {
            let mut by_age: Vec<(DateTime<Utc>, String)> = resumable
                .iter()
                .map(|(id, r)| (r.expired_at, id.clone()))
                .collect();
            by_age.sort();
            let excess = resumable.len() - MAX_RESUMABLE;
            for (_, id) in by_age.into_iter().take(excess) {
                resumable.remove(&id);
            }
        }
    }

    /// Takes the resumable record for `id`, if it has not outlived the TTL.
    fn take_resumable(&self, id: &str) -> Option<Resumable> {
        let ttl =
            chrono::Duration::from_std(self.config.resume_ttl).unwrap_or(chrono::Duration::MAX);
        self.resumable_sessions()
            .remove(id)
            .filter(|r| r.expired_at > Utc::now() - ttl)
    }

    fn resumable_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Resumable>> {
        self.resumable.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn load_resumable(path: &PathBuf) -> HashMap<String, Resumable> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(
                "cannot read resumable sessions from {}: {e}",
                path.display()
            );
            return HashMap::new();
        }
    };
    // Loaded once: a session resumed after this start must not be resumable
    // again from a stale copy after the next restart.
    let _ = std::fs::remove_file(path);
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!("ignoring malformed {}: {e}", path.display());
        HashMap::new()
    })
}

fn save_resumable(path: &PathBuf, sessions: &HashMap<String, Resumable>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(sessions)?)?;
    std::fs::rename(tmp, path)
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
            state.hub.reads().flush();
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
    let existing = state.sessions().get(session_id).cloned();
    let conn = match existing {
        Some(conn) => conn,
        None => match resume(&state, session_id, &headers, peer).await {
            Some(conn) => conn,
            None => {
                return rpc_error(
                    StatusCode::NOT_FOUND,
                    protocol::NOT_INITIALIZED,
                    "unknown or expired session; re-initialize",
                )
            }
        },
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
    let instance = header_str(headers, INSTANCE_HEADER).and_then(clean_instance);
    let hub = state.hub.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let conn_info = ConnInfo {
            transport: "http".into(),
            ip,
            client: None,
            instance,
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
        instance = ?conn.session().conn().instance,
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

/// Reopens an expired session that is still resumable, under the same id and
/// agent. The agent header fills in an identity the old session never had.
async fn resume(
    state: &Arc<HttpState>,
    session_id: &str,
    headers: &HeaderMap,
    peer: SocketAddr,
) -> Option<SharedConnection> {
    let Some(record) = state.take_resumable(session_id) else {
        // A concurrent request may have resumed it a moment ago.
        return state.sessions().get(session_id).cloned();
    };
    let conn_info = ConnInfo {
        transport: "http".into(),
        ip: client_ip(&state.config, headers, peer),
        client: record.client.clone(),
        instance: header_str(headers, INSTANCE_HEADER)
            .and_then(clean_instance)
            .or(record.instance.clone()),
    };
    let agent = record
        .agent
        .clone()
        .or_else(|| header_str(headers, AGENT_HEADER).map(str::to_owned));
    let role = header_str(headers, ROLE_HEADER).map(str::to_owned);
    let hub = state.hub.clone();
    let id = session_id.to_string();
    let conn = tokio::task::spawn_blocking(move || {
        McpConnection::resume(
            &hub,
            &id,
            conn_info,
            &record.protocol_version,
            agent.as_deref(),
            role.as_deref(),
        )
    })
    .await
    .ok()?;
    tracing::info!(
        session = %session_id,
        agent = ?conn.session().agent(),
        "mcp session resumed"
    );
    let shared = Arc::new(tokio::sync::Mutex::new(conn));
    Some(
        state
            .sessions()
            .entry(session_id.to_string())
            .or_insert(shared)
            .clone(),
    )
}

async fn handle_delete(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    let Some(session_id) = header_str(&headers, SESSION_HEADER) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(conn) = state.sessions().remove(session_id) else {
        return if state.take_resumable(session_id).is_some() {
            StatusCode::NO_CONTENT.into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        };
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
