//! Management web UI (`/admin`) and its JSON API (`/api/admin/*`).
//!
//! The UI is a static page embedded in the binary. When the server runs with
//! an admin token, every API call must carry `Authorization: Bearer <token>`.
//! State-changing calls must also come from the page's own origin (or from a
//! non-browser client that sends no `Origin`).

use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, OnceLock},
};

use axum::{
    extract::{rejection::JsonRejection, ConnectInfo, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use memhub_app::{
    admin::{count_instances, is_internal_agent, AdminNewMemory, ConnectionSession, LogFilter},
    store::{normalize_folder, MemoryId, Scope},
    Admin, AppError, READ_HISTORY_DAYS,
};
use rust_xlsxwriter::{Color, Format, Workbook, Worksheet, XlsxError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{client_ip, HttpState, LiveSession};

const INDEX_HTML: &str = include_str!("admin/index.html");
const APP_JS: &str = include_str!("admin/app.js");
const APP_CSS: &str = include_str!("admin/app.css");

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

const DEFAULT_RANKING_LIMIT: usize = 100;
const MAX_RANKING_LIMIT: usize = 1000;
const DEFAULT_LOG_LIMIT: usize = 200;
const MAX_LOG_LIMIT: usize = 5000;
/// Excel export covers at most this many days, and at most this many sessions
/// (the newest ones, when the year is larger than that).
const EXPORT_DAYS: u32 = 365;
const EXPORT_MAX: usize = 100_000;

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
        .route("/memories/move", post(move_memory))
        .route("/reads", get(read_ranking))
        .route("/memories/{*id}", get(get_memory).delete(delete_memory))
        .route("/connections", get(connections))
        .route("/connections/summary", get(connection_summary))
        .route("/connections/export", get(export_connections))
        .route("/updates", get(updates))
        .layer(middleware::from_fn_with_state(state, guard));

    Router::new()
        .route("/", get(|| async { Redirect::temporary("/admin") }))
        .route(
            "/admin",
            get(|| async { asset(index_html(), "text/html; charset=utf-8") }),
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

/// The page with asset URLs fingerprinted by content, so browsers fetch the
/// new script and stylesheet right after an upgrade.
fn index_html() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        // FNV-1a over both assets.
        let hash = APP_JS
            .bytes()
            .chain(APP_CSS.bytes())
            .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
        INDEX_HTML.replace("__ASSET_VERSION__", &format!("{hash:016x}"))
    })
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

/// Live MCP sessions, without the self-test probe.
fn shown_live(state: &HttpState) -> Vec<LiveSession> {
    state
        .live_sessions()
        .into_iter()
        .filter(|s| s.agent.as_deref().is_none_or(|a| !is_internal_agent(a)))
        .collect()
}

fn parse_json<T: DeserializeOwned>(body: JsonBody) -> Result<T, ApiError> {
    let Json(value) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    serde_json::from_value(value).map_err(|e| ApiError::bad_request(e.to_string()))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn memory_id(id: &str) -> Result<MemoryId, ApiError> {
    Ok(id.parse::<MemoryId>()?)
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
    v["live_sessions"] = json!(shown_live(&state));
    v["server"] = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.started_at(),
        "data_dir": state.hub().data_root().display().to_string(),
        "token_required": state.config().admin_token.is_some(),
    });
    ok(v)
}

async fn sessions(State(state): State<AppState>) -> ApiResult {
    ok(json!({ "sessions": shown_live(&state) }))
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
    /// Subfolder below the owner; empty or absent for the owner's root.
    #[serde(default)]
    folder: String,
    description: Option<String>,
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
    let folder = normalize_folder(&body.folder)?;
    let (name, path) = (owner.clone(), folder.clone());
    let description = body.description;
    let created = run(&state, ip, move |admin| {
        admin.create_folder(scope, &name, &path, description.as_deref())
    })
    .await?;
    ok(json!({ "scope": scope, "owner": owner, "folder": folder, "created": created }))
}

#[derive(Deserialize)]
struct FolderQuery {
    #[serde(default)]
    folder: String,
}

async fn delete_folder(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((scope, owner)): Path<(String, String)>,
    Query(query): Query<FolderQuery>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let scope: Scope = scope.parse()?;
    let folder = normalize_folder(&query.folder)?;
    let (name, path) = (owner.clone(), folder.clone());
    let deleted = run(&state, ip, move |admin| {
        admin.delete_folder(scope, &name, &path)
    })
    .await?;
    ok(json!({ "scope": scope, "owner": owner, "folder": folder, "deleted": deleted }))
}

#[derive(Deserialize)]
struct ListQuery {
    scope: Option<String>,
    owner: Option<String>,
    folder: Option<String>,
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
    let folder = non_empty(query.folder);
    let q = non_empty(query.q);
    let memories = run(&state, ip, move |admin| {
        admin.list(scope, owner.as_deref(), folder.as_deref(), q.as_deref())
    })
    .await?;
    ok(json!({ "count": memories.len(), "memories": memories }))
}

#[derive(Deserialize)]
struct RankingQuery {
    scope: Option<String>,
    owner: Option<String>,
    folder: Option<String>,
    /// Window in days (at most [`READ_HISTORY_DAYS`]); missing or 0 = all time.
    days: Option<u32>,
    limit: Option<usize>,
}

async fn read_ranking(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<RankingQuery>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let scope = match non_empty(query.scope) {
        Some(s) => Some(s.parse::<Scope>()?),
        None => None,
    };
    let owner = non_empty(query.owner);
    let folder = non_empty(query.folder);
    let days = query
        .days
        .filter(|d| *d > 0)
        .map(|d| d.min(READ_HISTORY_DAYS));
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RANKING_LIMIT)
        .clamp(1, MAX_RANKING_LIMIT);
    ok(run(&state, ip, move |admin| {
        admin.read_ranking(scope, owner.as_deref(), folder.as_deref(), days, limit)
    })
    .await?)
}

async fn get_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let id = memory_id(&id)?;
    ok(run(&state, ip, move |admin| admin.get(&id)).await?)
}

async fn delete_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let id = memory_id(&id)?;
    let memory = run(&state, ip, move |admin| admin.delete_memory(&id)).await?;
    ok(json!({ "deleted": memory.meta }))
}

#[derive(Deserialize)]
struct MoveBody {
    id: String,
    #[serde(default)]
    folder: String,
    slug: Option<String>,
}

async fn move_memory(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: JsonBody,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let body: MoveBody = parse_json(body)?;
    let id = memory_id(&body.id)?;
    let previous = id.to_string();
    let memory = run(&state, ip, move |admin| {
        admin.move_memory(&id, &body.folder, body.slug.as_deref())
    })
    .await?;
    ok(json!({ "moved": memory.meta, "previous_id": previous }))
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
    #[serde(default)]
    folder: String,
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
        folder: body.folder,
    };
    let memory = run(&state, ip, move |admin| admin.add_memory(input)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "created": memory.meta }))).into_response())
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
    let live: Vec<String> = shown_live(&state)
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    let sessions: Vec<Value> = run(&state, ip, move |admin| admin.connections(&filter))
        .await?
        .into_iter()
        .map(|s| {
            let mut v = json!(s);
            v["live"] = json!(is_live(&s, |id| live.iter().any(|l| l == id)));
            v
        })
        .collect();
    ok(json!({ "sessions": sessions }))
}

async fn connection_summary(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let summary = run(&state, ip, |admin| admin.connection_summary()).await?;
    let live = shown_live(&state);
    let live_instances = count_instances(
        live.iter()
            .map(|s| (s.agent.as_deref(), s.instance.as_deref(), s.ip.as_str())),
    );
    ok(json!({
        "live": live.len(),
        "live_instances": live_instances.instances,
        "live_unlabeled_instances": live_instances.unlabeled_instances,
        "week": summary.week,
        "month": summary.month,
    }))
}

#[derive(Deserialize)]
struct ExportParams {
    agent: Option<String>,
    ip: Option<String>,
}

/// Sessions from the last [`EXPORT_DAYS`] days, as an `.xlsx` file.
/// `agent` and `ip` narrow it the same way as the connection table. `days`
/// is not accepted: the file is always the trailing year, capped at
/// [`EXPORT_MAX`] rows.
async fn export_connections(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<ExportParams>,
) -> ApiResult {
    let ip = caller(&state, &headers, peer);
    let filter = LogFilter {
        agent: non_empty(params.agent),
        ip: non_empty(params.ip),
        days: Some(EXPORT_DAYS),
        limit: EXPORT_MAX + 1,
        ..Default::default()
    };
    let live: HashSet<String> = shown_live(&state)
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    let (bytes, truncated) = run(&state, ip, move |admin| {
        let mut sessions = admin.connections(&filter)?;
        let truncated = sessions.len() > EXPORT_MAX;
        if truncated {
            sessions.truncate(EXPORT_MAX);
        }
        let bytes = connections_xlsx(&sessions, &live)
            .map_err(|e| AppError::Internal(format!("cannot build workbook: {e}")))?;
        Ok((bytes, truncated))
    })
    .await?;
    let filename = format!("memhub-connections-{}.xlsx", Utc::now().format("%Y-%m-%d"));
    Ok(xlsx_response(bytes, &filename, truncated))
}

fn xlsx_response(bytes: Vec<u8>, filename: &str, truncated: bool) -> Response {
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| {
            HeaderValue::from_static("attachment; filename=\"memhub-connections.xlsx\"")
        });
    let mut res = bytes.into_response();
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if truncated {
        headers.insert(
            HeaderName::from_static("x-export-truncated"),
            HeaderValue::from_static("1"),
        );
    }
    res
}

const XLSX_COLUMNS: &[(&str, f64)] = &[
    ("Session ID", 34.0),
    ("Agent", 22.0),
    ("IP", 16.0),
    ("Transport", 12.0),
    ("Client", 24.0),
    ("Instance", 22.0),
    ("Connected (UTC)", 21.0),
    ("Identified (UTC)", 21.0),
    ("Registered", 12.0),
    ("Disconnected (UTC)", 21.0),
    ("End reason", 16.0),
    ("Duration (seconds)", 18.0),
    ("Memory changes", 16.0),
    ("Live", 10.0),
    ("Resumed", 10.0),
];

/// A resumed session id has an entry per stretch; only the open one is live.
fn is_live(session: &ConnectionSession, open: impl Fn(&str) -> bool) -> bool {
    session.disconnected_at.is_none() && open(&session.session_id)
}

fn connections_xlsx(
    sessions: &[ConnectionSession],
    live: &HashSet<String>,
) -> Result<Vec<u8>, XlsxError> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name("Connections")?;
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xEEF0FF));
    let seconds = Format::new().set_num_format("0.0");
    for (col, (name, width)) in XLSX_COLUMNS.iter().enumerate() {
        let col = u16::try_from(col).unwrap_or(0);
        sheet.write_string_with_format(0, col, *name, &header)?;
        sheet.set_column_width(col, *width)?;
    }
    let now = Utc::now();
    for (i, session) in sessions.iter().enumerate() {
        write_session_row(
            sheet,
            u32::try_from(i + 1).unwrap_or(u32::MAX),
            session,
            is_live(session, |id| live.contains(id)),
            now,
            &seconds,
        )?;
    }
    let last_col = u16::try_from(XLSX_COLUMNS.len() - 1).unwrap_or(0);
    let last_row = u32::try_from(sessions.len()).unwrap_or(0);
    sheet.autofilter(0, 0, last_row, last_col)?;
    sheet.set_freeze_panes(1, 0)?;
    workbook.save_to_buffer()
}

fn write_session_row(
    sheet: &mut Worksheet,
    row: u32,
    session: &ConnectionSession,
    live: bool,
    now: DateTime<Utc>,
    seconds: &Format,
) -> Result<(), XlsxError> {
    sheet.write_string(row, 0, &session.session_id)?;
    write_some(sheet, row, 1, session.agent.as_deref())?;
    sheet.write_string(row, 2, &session.ip)?;
    sheet.write_string(row, 3, &session.transport)?;
    write_some(sheet, row, 4, session.client.as_deref())?;
    write_some(sheet, row, 5, session.instance.as_deref())?;
    write_some(
        sheet,
        row,
        6,
        excel_time(session.connected_at.as_deref()).as_deref(),
    )?;
    write_some(
        sheet,
        row,
        7,
        excel_time(session.identified_at.as_deref()).as_deref(),
    )?;
    sheet.write_string(row, 8, if session.registered { "yes" } else { "no" })?;
    write_some(
        sheet,
        row,
        9,
        excel_time(session.disconnected_at.as_deref()).as_deref(),
    )?;
    write_some(sheet, row, 10, session.reason.as_deref())?;
    if let Some(duration) = session_duration(session, live, now) {
        sheet.write_number_with_format(row, 11, duration, seconds)?;
    }
    sheet.write_number(row, 12, session.changes as f64)?;
    sheet.write_string(row, 13, if live { "yes" } else { "no" })?;
    sheet.write_string(row, 14, if session.resumed { "yes" } else { "no" })?;
    Ok(())
}

fn write_some(
    sheet: &mut Worksheet,
    row: u32,
    col: u16,
    value: Option<&str>,
) -> Result<(), XlsxError> {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        sheet.write_string(row, col, value)?;
    }
    Ok(())
}

fn excel_time(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    Some(match raw.parse::<DateTime<Utc>>() {
        Ok(ts) => ts.format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => raw.to_string(),
    })
}

fn session_duration(session: &ConnectionSession, live: bool, now: DateTime<Utc>) -> Option<f64> {
    if let Some(duration) = session.duration_secs {
        return Some(duration);
    }
    if !live {
        return None;
    }
    let start = session
        .connected_at
        .as_deref()?
        .parse::<DateTime<Utc>>()
        .ok()?;
    Some(((now.timestamp_millis() - start.timestamp_millis()) as f64 / 1000.0).max(0.0))
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
