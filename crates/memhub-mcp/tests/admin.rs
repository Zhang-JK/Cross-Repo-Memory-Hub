use std::{io::Write, net::SocketAddr};

use axum::{
    body::Body,
    extract::connect_info::MockConnectInfo,
    http::{Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use memhub_app::{Hub, HubConfig};
use memhub_mcp::http::{router, HttpConfig, HttpState};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app(token: Option<&str>) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let state = HttpState::new(
        hub,
        HttpConfig {
            admin_token: token.map(str::to_string),
            ..Default::default()
        },
    );
    let addr: SocketAddr = "10.1.2.3:5000".parse().unwrap();
    (dir, router(state).layer(MockConnectInfo(addr)))
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn serves_the_ui_with_security_headers() {
    let (_dir, app) = app(None);
    for (path, ty) in [
        ("/admin", "text/html"),
        ("/admin/app.js", "text/javascript"),
        ("/admin/app.css", "text/css"),
    ] {
        let res = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let h = res.headers();
        assert!(
            h["content-type"].to_str().unwrap().starts_with(ty),
            "{path}"
        );
        assert!(h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'"));
        assert_eq!(h["x-frame-options"], "DENY");
    }
    let res = app
        .clone()
        .oneshot(Request::get("/admin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let page = res.into_body().collect().await.unwrap().to_bytes();
    let page = String::from_utf8(page.to_vec()).unwrap();
    assert!(
        page.contains("/admin/app.js?v="),
        "assets are fingerprinted"
    );
    assert!(!page.contains("__ASSET_VERSION__"));

    let res = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(res.headers()["location"], "/admin");
}

#[tokio::test]
async fn token_protects_the_api() {
    let (_dir, app) = app(Some("s3cret"));
    let (status, info) = call(&app, "GET", "/api/admin/auth", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info, json!({ "token_required": true, "authorized": false }));

    let (status, _) = call(&app, "GET", "/api/admin/stats", None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        "GET",
        "/api/admin/stats",
        None,
        &[("authorization", "Bearer wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let auth = [("authorization", "Bearer s3cret")];
    let (_, info) = call(&app, "GET", "/api/admin/auth", None, &auth).await;
    assert_eq!(info["authorized"], true);
    let (status, stats) = call(&app, "GET", "/api/admin/stats", None, &auth).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stats["server"]["token_required"], true);

    let (status, _) = call(
        &app,
        "POST",
        "/api/admin/folders",
        Some(json!({ "scope": "repo", "owner": "x" })),
        &[],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "writes need the token too"
    );
}

#[tokio::test]
async fn nested_folders_over_http() {
    let (_dir, app) = app(None);
    let (status, res) = call(
        &app,
        "POST",
        "/api/admin/folders",
        Some(
            json!({ "scope": "repo", "owner": "shop", "folder": "/modules/checkout/",
                     "description": "Cart and payment" }),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["folder"], "modules/checkout");
    assert_eq!(res["created"], true);

    let (status, res) = call(
        &app,
        "POST",
        "/api/admin/memories",
        Some(
            json!({ "scope": "repo", "owner": "shop", "folder": "modules/checkout",
                     "title": "Payment retries", "body": "3x" }),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{res}");
    let id = "repo/shop/modules/checkout/payment-retries";
    assert_eq!(res["created"]["id"], id);

    let (status, detail) = call(&app, "GET", &format!("/api/admin/memories/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["path"], format!("memories/{id}.md"));

    let (_, folders) = call(&app, "GET", "/api/admin/folders", None, &[]).await;
    let shop = &folders["folders"][0];
    assert_eq!(shop["children"][1]["path"], "modules/checkout");
    assert_eq!(shop["children"][1]["description"], "Cart and payment");
    assert_eq!(shop["children"][0]["total_count"], 1);

    let (_, list) = call(
        &app,
        "GET",
        "/api/admin/memories?scope=repo&owner=shop&folder=modules",
        None,
        &[],
    )
    .await;
    assert_eq!(list["count"], 1);

    let (status, moved) = call(
        &app,
        "POST",
        "/api/admin/memories/move",
        Some(json!({ "id": id, "folder": "pitfalls", "slug": "" })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["moved"]["id"], "repo/shop/pitfalls/payment-retries");
    assert_eq!(moved["previous_id"], id);
    let (status, _) = call(&app, "GET", &format!("/api/admin/memories/{id}"), None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        "GET",
        "/api/admin/memories/repo/shop/..%2F..%2Fx/y",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        "POST",
        "/api/admin/folders",
        Some(json!({ "scope": "repo", "owner": "shop", "folder": "a/../../b" })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, res) = call(
        &app,
        "DELETE",
        "/api/admin/folders/repo/shop?folder=pitfalls",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(
        res["deleted"],
        json!(["repo/shop/pitfalls/payment-retries"])
    );
    let (_, folders) = call(&app, "GET", "/api/admin/folders", None, &[]).await;
    let paths: Vec<&str> = folders["folders"][0]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["modules", "modules/checkout"]);

    let (_, updates) = call(&app, "GET", "/api/admin/updates?action=move", None, &[]).await;
    assert_eq!(updates["updates"][0]["previous_id"], id);
}

#[tokio::test]
async fn manage_folders_and_memories() {
    let (_dir, app) = app(None);
    let (status, res) = call(
        &app,
        "POST",
        "/api/admin/folders",
        Some(json!({ "scope": "repo", "owner": "engine" })),
        &[
            ("origin", "http://hub.local:8010"),
            ("host", "hub.local:8010"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["created"], true);

    let (status, _) = call(
        &app,
        "POST",
        "/api/admin/folders",
        Some(json!({ "scope": "repo", "owner": "evil" })),
        &[
            ("origin", "http://evil.example"),
            ("host", "hub.local:8010"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, res) = call(
        &app,
        "POST",
        "/api/admin/memories",
        Some(json!({
            "scope": "repo", "owner": "engine", "title": "Build notes",
            "kind": "architecture", "tags": ["cmake", " "], "body": "Use ninja."
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(res["created"]["id"], "repo/engine/build-notes");
    assert_eq!(res["created"]["tags"], json!(["cmake"]));

    let (status, res) = call(
        &app,
        "POST",
        "/api/admin/memories",
        Some(json!({ "scope": "repo", "owner": "engine", "title": "x", "slug": "build-notes" })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{res}");

    let (_, list) = call(
        &app,
        "GET",
        "/api/admin/memories?scope=repo&owner=engine",
        None,
        &[],
    )
    .await;
    assert_eq!(list["count"], 1);
    let (_, found) = call(&app, "GET", "/api/admin/memories?q=ninja", None, &[]).await;
    assert_eq!(found["memories"][0]["id"], "repo/engine/build-notes");
    assert!(found["memories"][0]["snippet"].is_string());

    let (status, detail) = call(
        &app,
        "GET",
        "/api/admin/memories/repo/engine/build-notes",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["memory"]["body"], "Use ninja.\n");
    assert!(detail["markdown"]
        .as_str()
        .unwrap()
        .contains("kind: architecture"));

    let (status, _) = call(
        &app,
        "GET",
        "/api/admin/memories/repo/..%2Fetc/passwd",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, folders) = call(&app, "GET", "/api/admin/folders", None, &[]).await;
    assert_eq!(folders["folders"][0]["owner"], "engine");
    assert_eq!(folders["folders"][0]["memory_count"], 1);

    let (status, res) = call(&app, "DELETE", "/api/admin/folders/repo/engine", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["deleted"], json!(["repo/engine/build-notes"]));
    let (status, _) = call(
        &app,
        "GET",
        "/api/admin/memories/repo/engine/build-notes",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, updates) = call(&app, "GET", "/api/admin/updates", None, &[]).await;
    let events: Vec<&str> = updates["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        vec![
            "folder_deleted",
            "memory_changed",
            "memory_changed",
            "folder_created"
        ]
    );
    let first = &updates["updates"][0];
    assert_eq!(first["agent"], "(admin)");
    assert_eq!(first["ip"], "10.1.2.3");
}

#[tokio::test]
async fn connection_log_marks_live_sessions() {
    let (_dir, app) = app(None);
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "probe", "version": "1" } }
    });
    let res = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .header("x-memhub-agent", "crash-bot")
                .header("x-memhub-role", "crash-analysis")
                .body(Body::from(init.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sid = res.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();

    let (_, conns) = call(&app, "GET", "/api/admin/connections", None, &[]).await;
    let s = &conns["sessions"][0];
    assert_eq!(s["session_id"], sid.as_str());
    assert_eq!(s["agent"], "crash-bot");
    assert_eq!(s["client"], "probe 1");
    assert_eq!(s["ip"], "10.1.2.3");
    assert_eq!(s["live"], true);
    assert_eq!(s["registered"], true);

    let (_, stats) = call(&app, "GET", "/api/admin/stats", None, &[]).await;
    assert_eq!(stats["live_sessions"][0]["agent"], "crash-bot");
    assert_eq!(stats["connections_today"], 1);

    let res = app
        .clone()
        .oneshot(
            Request::delete("/mcp")
                .header("mcp-session-id", &sid)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let (_, conns) = call(&app, "GET", "/api/admin/connections", None, &[]).await;
    assert_eq!(conns["sessions"][0]["live"], false);
    assert_eq!(conns["sessions"][0]["reason"], "client_closed");

    let (_, events) = call(&app, "GET", "/api/admin/connections?view=events", None, &[]).await;
    let names: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["disconnect", "agent_registered", "connect"]);
    assert_eq!(events["events"][2]["agent"], "crash-bot");
}

fn append_log(dir: &std::path::Path, days_ago: i64, event: &str, rest: &str) {
    let ts = Utc::now() - Duration::days(days_ago);
    let path = dir
        .join("logs")
        .join(format!("audit-{}.jsonl", ts.format("%Y-%m-%d")));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(
        file,
        r#"{{"ts":"{}","event":"{event}",{rest}}}"#,
        ts.to_rfc3339()
    )
    .unwrap();
}

fn xlsx_xml(bytes: &[u8]) -> String {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = String::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).unwrap();
        if !file.name().ends_with(".xml") {
            continue;
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text).unwrap();
        out.push_str(&text);
    }
    out
}

#[tokio::test]
async fn connection_summary_and_year_export() {
    let (dir, router) = app(None);
    let rest = |id: &str, ip: &str, agent: &str, client: &str| {
        format!(
            r#""session_id":"{id}","ip":"{ip}","transport":"http","agent":"{agent}","client":"{client}""#
        )
    };
    let month = rest("month-session", "10.9.1.1", "month-bot", "a<b>&c");
    append_log(dir.path(), 20, "connect", &month);
    append_log(
        dir.path(),
        20,
        "disconnect",
        &format!(r#"{month},"reason":"client_closed","duration_secs":2.0"#),
    );
    let ancient = rest("ancient-session", "10.8.8.8", "ancient-bot", "old");
    append_log(dir.path(), 400, "connect", &ancient);
    append_log(
        dir.path(),
        400,
        "disconnect",
        &format!(r#"{ancient},"reason":"client_closed","duration_secs":1.0"#),
    );

    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "probe", "version": "1" } }
    });
    // Two copies of crash-bot on one IP, told apart by X-Memhub-Instance.
    for instance in ["alice-laptop", "bob-desktop"] {
        let res = router
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("content-type", "application/json")
                    .header("x-memhub-agent", "crash-bot")
                    .header("x-memhub-role", "crash-analysis")
                    .header("x-memhub-instance", instance)
                    .body(Body::from(init.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    let (_, summary) = call(&router, "GET", "/api/admin/connections/summary", None, &[]).await;
    assert_eq!(summary["live"], 2);
    assert_eq!(summary["live_instances"], 2);
    assert_eq!(summary["live_unlabeled_instances"], 0);
    assert_eq!(summary["week"]["connections"], 2);
    assert_eq!(summary["week"]["ips"], 1);
    assert_eq!(summary["week"]["agents"], 1);
    assert_eq!(summary["week"]["instances"], 2);
    assert_eq!(summary["week"]["unlabeled_instances"], 0);
    assert_eq!(summary["month"]["connections"], 3);
    assert_eq!(summary["month"]["ips"], 2);
    assert_eq!(summary["month"]["agents"], 2);
    assert_eq!(summary["month"]["instances"], 3);
    assert_eq!(summary["month"]["unlabeled_instances"], 1);

    let (_, live) = call(&router, "GET", "/api/admin/sessions", None, &[]).await;
    let mut labels: Vec<&str> = live["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["instance"].as_str().unwrap())
        .collect();
    labels.sort_unstable();
    assert_eq!(labels, ["alice-laptop", "bob-desktop"]);
    let (_, conns) = call(
        &router,
        "GET",
        "/api/admin/connections?agent=crash-bot",
        None,
        &[],
    )
    .await;
    assert_eq!(conns["sessions"].as_array().unwrap().len(), 2);
    assert!(conns["sessions"][0]["instance"].is_string());

    let res = router
        .clone()
        .oneshot(
            Request::get("/api/admin/connections/export")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()["content-type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    let disposition = res.headers()["content-disposition"].to_str().unwrap();
    assert!(disposition.contains("memhub-connections-"));
    assert!(disposition.ends_with(".xlsx\""));
    assert!(res.headers().get("x-export-truncated").is_none());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"PK\x03\x04"));
    let xml = xlsx_xml(&bytes);
    assert!(xml.contains("Session ID"));
    assert!(xml.contains("Connected (UTC)"));
    assert!(xml.contains(">Instance<"));
    assert!(xml.contains("alice-laptop") && xml.contains("bob-desktop"));
    assert!(xml.contains("month-session"));
    assert!(xml.contains("crash-bot"));
    assert!(
        xml.contains("a&lt;b&gt;&amp;c"),
        "cell text must be escaped"
    );
    assert!(!xml.contains("ancient-session"));

    let filtered = router
        .clone()
        .oneshot(
            Request::get("/api/admin/connections/export?agent=month-bot&days=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let bytes = filtered.into_body().collect().await.unwrap().to_bytes();
    let xml = xlsx_xml(&bytes);
    assert!(xml.contains("month-session"));
    assert!(
        !xml.contains("crash-bot"),
        "agent filter applies; days=1 is ignored"
    );

    let (_dir, locked) = app(Some("s3cret"));
    let denied = locked
        .clone()
        .oneshot(
            Request::get("/api/admin/connections/export")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let (status, summary) = call(
        &locked,
        "GET",
        "/api/admin/connections/summary",
        None,
        &[("authorization", "Bearer s3cret")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["live"], 0);
    assert_eq!(summary["month"]["connections"], 0);
}

#[tokio::test]
async fn selftest_agent_is_omitted_from_the_admin_api() {
    let (_dir, router) = app(None);
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "probe", "version": "1" } }
    });
    let res = router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("content-type", "application/json")
                .header("x-memhub-agent", "memhub-selftest")
                .header("x-memhub-role", "selftest")
                .body(Body::from(init.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let (_, summary) = call(&router, "GET", "/api/admin/connections/summary", None, &[]).await;
    assert_eq!(summary["live"], 0);
    assert_eq!(summary["week"]["connections"], 0);
    let (_, conns) = call(&router, "GET", "/api/admin/connections", None, &[]).await;
    assert_eq!(conns["sessions"].as_array().unwrap().len(), 0);
    let (_, stats) = call(&router, "GET", "/api/admin/stats", None, &[]).await;
    assert_eq!(stats["live_sessions"].as_array().unwrap().len(), 0);
    assert_eq!(stats["connections_today"], 0);
}
