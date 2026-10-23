//! Management web UI (`/admin`) and its JSON API (`/api/admin/*`).
//!
//! The UI is a static page embedded in the binary. When the server runs with
//! an admin token, every API call must carry `Authorization: Bearer <token>`.
//! State-changing calls must also come from the page's own origin (or from a
//! non-browser client that sends no `Origin`).

use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{rejection::JsonRejection, ConnectInfo, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get},
    Json, Router,
};
use memhub_app::{
    admin::{AdminNewMemory, LogFilter},
    store::{MemoryId, Scope},
    Admin, AppError,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{client_ip, HttpState};

const INDEX_HTML: &str = include_str!("admin/index.html");
const APP_JS: &str = include_str!("admin/app.js");
const APP_CSS: &str = include_str!("admin/app.css");

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

const DEFAULT_LOG_LIMIT: usize = 200;
const MAX_LOG_LIMIT: usize = 5000;

type AppState = Arc<HttpState>;
type ApiResult = Result<Response, ApiError>;
type JsonBody = Result<Json<Value>, JsonRejection>;

pub fn routes(state: AppState) -> Router<AppState> {
    let api = Router::new()
        .route("/stats", get(stats))
        .route("/sessions", get(sessions))
        .route("/folders", get(folders).post(create_folder))
        .route("/folders/{scope}/{owner}", delete(delete_folder))
        .route("/memories", get(list_memories).post(add_memory))
        .route(
            "/memories/{scope}/{owner}/{slug}",
            get(get_memory).delete(delete_memory),
        )
        .route("/connections", get(connections))
        .route("/updates", get(updates))
        .layer(middleware::from_fn_with_state(state, guard));

    Router::new()
        .route("/", get(|| async { Redirect::temporary("/admin") }))
        .route(
            "/admin",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route("/admin/", get(|| async { Redirect::permanent("/admin") }))
        .route(
            "/admin/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/admin/app.css",
            get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }),
        )
        .route("/api/admin/auth", get(auth_info))
        .nest("/api/admin", api)
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, CSP),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    res
}

// ---- errors -------------------------------------------------------------------

/// JSON error body: `{"error": <code>, "message": <text>}`.
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
    }

    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        let status = match e.code() {
            "not_found" | "unknown_repo" | "unknown_agent" => StatusCode::NOT_FOUND,
            "already_exists" | "version_conflict" => StatusCode::CONFLICT,
            "invalid_input" => StatusCode::BAD_REQUEST,
            "forbidden" => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("admin request failed: {e}");
        }
        Self::new(status, e.code(), e.to_string())
    }
}

impl From<memhub_app::store::StoreError> for ApiError {
    fn from(e: memhub_app::store::StoreError) -> Self {
        AppError::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.code, "message": self.message });
        (self.status, Json(body)).into_response()
    }
}

// ---- access control -------------------------------------------------------------

async fn auth_info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    Json(json!({
        "token_required": state.config().admin_token.is_some(),
        "authorized": token_ok(&state, &headers),
    }))
    .into_response()
}

fn token_ok(state: &HttpState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.config().admin_token.as_deref() else {
        return true;
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    constant_time_eq(given.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Origin` must name the host the request was sent to.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin_host = origin.split_once("://").map(|(_, rest)| rest);
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !token_ok(&state, req.headers()) {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or wrong admin token",
        )
        .into_response();
    }
    if req.method() != Method::GET && !same_origin(req.headers()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "cross-origin request rejected",
        )
        .into_response();
    }
    next.run(req).await
}

// ---- helpers --------------------------------------------------------------------

/// Runs a filesystem-bound admin operation off the async runtime, as the
/// calling IP.
async fn run<T, F>(state: &AppState, ip: String, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(Admin) -> memhub_app::Result<T> + Send + 'static,
{
    let admin = state.hub().admin(&ip);
    match tokio::task::spawn_blocking(move || f(admin)).await {
        Ok(result) => Ok(result?),
        Err(e) => {
            tracing::error!("admin handler panicked: {e}");
            Err(ApiError::internal())
        }
    }
}

fn ok<T: Serialize>(value: T) -> ApiResult {
    Ok(Json(value).into_response())
}

fn caller(state: &HttpState, headers: &HeaderMap, peer: SocketAddr) -> String {
    client_ip(state.config(), headers, peer)
}

fn parse_json<T: DeserializeOwned>(body: JsonBody) -> Result<T, ApiError> {
    let Json(value) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    serde_json::from_value(value).map_err(|e| ApiError::bad_request(e.to_string()))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn memory_id(scope: &str, owner: &str, slug: &str) -> Result<MemoryId, ApiError> {
    Ok(format!("{scope}/{owner}/{slug}").parse::<MemoryId>()?)
}

// ---- handlers -------------------------------------------------------------------

async fn stats(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let stats = run(&state, ip, |admin| admin.stats()).await?;
    let mut v = serde_json::to_value(stats).map_err(|_| ApiError::internal())?;
    v["live_sessions"] = json!(state.live_sessions());
    v["server"] = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.started_at(),
        "data_dir": state.hub().data_root().display().to_string(),
        "token_required": state.config().admin_token.is_some(),
    });
    ok(v)
}

async fn sessions(State(state): State<AppState>) -> ApiResult {
    ok(json!({ "sessions": state.live_sessions() }))
}

async fn folders(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let folders = run(&state, ip, |admin| admin.folders()).await?;
    ok(json!({ "folders": folders }))
}

#[derive(Deserialize)]
struct FolderBody {
    scope: String,
    owner: String,
}

async fn create_folder(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: JsonBody,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let body: FolderBody = parse_json(body)?;
    let scope: Scope = body.scope.parse()?;
    let owner = body.owner.trim().to_string();
    let name = owner.clone();
    let created = run(&state, ip, move |admin| admin.create_folder(scope, &name)).await?;
    ok(json!({ "scope": scope, "owner": owner, "created": created }))
}

async fn delete_folder(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scope, owner)): Path<(String, String)>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let scope: Scope = scope.parse()?;
    let name = owner.clone();
    let deleted = run(&state, ip, move |admin| admin.delete_folder(scope, &name)).await?;
    ok(json!({ "scope": scope, "owner": owner, "deleted": deleted }))
}

#[derive(Deserialize)]
struct ListQuery {
    scope: Option<String>,
    owner: Option<String>,
    q: Option<String>,
}

async fn list_memories(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let scope = match non_empty(query.scope) {
        Some(s) => Some(s.parse::<Scope>()?),
        None => None,
    };
    let owner = non_empty(query.owner);
    let q = non_empty(query.q);
    let memories = run(&state, ip, move |admin| {
        admin.list(scope, owner.as_deref(), q.as_deref())
    })
    .await?;
    ok(json!({ "count": memories.len(), "memories": memories }))
}

async fn get_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scope, owner, slug)): Path<(String, String, String)>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let id = memory_id(&scope, &owner, &slug)?;
    ok(run(&state, ip, move |admin| admin.get(&id)).await?)
}

async fn delete_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scope, owner, slug)): Path<(String, String, String)>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let id = memory_id(&scope, &owner, &slug)?;
    let memory = run(&state, ip, move |admin| admin.delete_memory(&id)).await?;
    ok(json!({ "deleted": memory.meta }))
}

#[derive(Deserialize)]
struct AddBody {
    scope: String,
    owner: String,
    title: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
    body: Option<String>,
    slug: Option<String>,
    markdown: Option<String>,
}

async fn add_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: JsonBody,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let body: AddBody = parse_json(body)?;
    let clean = |v: Vec<String>| -> Vec<String> {
        v.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let input = AdminNewMemory {
        scope: body.scope.parse()?,
        owner: body.owner,
        title: body.title,
        kind: body.kind,
        tags: clean(body.tags),
        groups: clean(body.groups),
        body: body.body,
        slug: body.slug,
        markdown: body.markdown,
    };
    let memory = run(&state, ip, move |admin| admin.add_memory(input)).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "created": memory.meta })),
    )
        .into_response())
}

#[derive(Deserialize)]
struct LogParams {
    view: Option<String>,
    agent: Option<String>,
    ip: Option<String>,
    action: Option<String>,
    memory: Option<String>,
    days: Option<u32>,
    limit: Option<usize>,
}

impl LogParams {
    fn filter(self) -> (Option<String>, LogFilter) {
        let filter = LogFilter {
            agent: non_empty(self.agent),
            ip: non_empty(self.ip),
            action: non_empty(self.action),
            memory: non_empty(self.memory),
            days: self.days.filter(|d| *d > 0),
            limit: self
                .limit
                .unwrap_or(DEFAULT_LOG_LIMIT)
                .clamp(1, MAX_LOG_LIMIT),
        };
        (non_empty(self.view), filter)
    }
}

async fn connections(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<LogParams>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let (view, filter) = params.filter();
    if view.as_deref() == Some("events") {
        let events = run(&state, ip, move |admin| admin.connection_events(&filter)).await?;
        return ok(json!({ "events": events }));
    }
    let live: Vec<String> = state
        .live_sessions()
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    let sessions: Vec<Value> = run(&state, ip, move |admin| admin.connections(&filter))
        .await?
        .into_iter()
        .map(|s| {
            let mut v = json!(s);
            v["live"] = json!(live.contains(&s.session_id));
            v
        })
        .collect();
    ok(json!({ "sessions": sessions }))
}

async fn updates(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<LogParams>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let (_, filter) = params.filter();
    let updates = run(&state, ip, move |admin| admin.updates(&filter)).await?;
    ok(json!({ "updates": updates }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn same_origin_compares_origin_with_host() {
        assert!(same_origin(&headers(&[])));
        assert!(same_origin(&headers(&[
            ("origin", "http://10.0.0.5:8010"),
            ("host", "10.0.0.5:8010"),
        ])));
        assert!(!same_origin(&headers(&[
            ("origin", "http://evil.example"),
            ("host", "10.0.0.5:8010"),
        ])));
        assert!(!same_origin(&headers(&[("origin", "null"), ("host", "x")])));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
