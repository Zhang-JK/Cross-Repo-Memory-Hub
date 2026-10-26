use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::connect_info::MockConnectInfo,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use memhub_app::{audit::LogQuery, Hub, HubConfig};
use memhub_mcp::{
    http::{router, HttpConfig, HttpState},
    stdio::{serve, StdioLogin},
};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Client {
    app: Router,
    session: Option<String>,
    next_id: u64,
}

impl Client {
    async fn send(&mut self, body: Value, extra: &[(&str, &str)]) -> (StatusCode, Option<Value>) {
        let mut req = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        if let Some(id) = res.headers().get("mcp-session-id") {
            self.session = Some(id.to_str().unwrap().to_string());
        }
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = (!bytes.is_empty()).then(|| serde_json::from_slice(&bytes).unwrap());
        (status, value)
    }

    async fn initialize(&mut self, extra: &[(&str, &str)]) -> (StatusCode, Option<Value>) {
        let r = self
            .send(
                json!({
                    "jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "test-client", "version": "1.0" }
                    }
                }),
                extra,
            )
            .await;
        if r.0 == StatusCode::OK {
            let (status, _) = self
                .send(
                    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                    &[],
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED);
        }
        r
    }

    /// Calls a tool and returns (isError, structuredContent or text).
    async fn tool(&mut self, name: &str, args: Value) -> (bool, Value) {
        self.next_id += 1;
        let (status, body) = self
            .send(
                json!({
                    "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                    "params": { "name": name, "arguments": args }
                }),
                &[],
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let result = body.unwrap()["result"].clone();
        let is_error = result["isError"].as_bool().unwrap();
        let payload = if is_error {
            result["content"][0]["text"].clone()
        } else {
            result["structuredContent"].clone()
        };
        (is_error, payload)
    }

    async fn delete(&mut self) -> StatusCode {
        let req = Request::delete("/mcp")
            .header("mcp-session-id", self.session.as_deref().unwrap())
            .body(Body::empty())
            .unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }
}

fn setup(idle: Duration) -> (tempfile::TempDir, Arc<Hub>, Arc<HttpState>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let state = HttpState::new(
        hub.clone(),
        HttpConfig {
            idle_timeout: idle,
            ..Default::default()
        },
    );
    (dir, hub, state)
}

fn client(state: &Arc<HttpState>, ip: &str) -> Client {
    let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
    Client {
        app: router(state.clone()).layer(MockConnectInfo(addr)),
        session: None,
        next_id: 0,
    }
}

impl Client {
    /// Calls a tool and returns its text content.
    async fn tool_text(&mut self, name: &str, args: Value) -> String {
        self.next_id += 1;
        let (_, body) = self
            .send(
                json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                        "params": { "name": name, "arguments": args } }),
                &[],
            )
            .await;
        let result = body.unwrap()["result"].clone();
        assert_eq!(result["isError"], false, "{result}");
        result["content"][0]["text"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn folder_hierarchy_workflow() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.1");
    let (_, init) = a
        .initialize(&[("x-memhub-agent", "doc-bot"), ("x-memhub-role", "docs")])
        .await;
    let instructions = init.unwrap()["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_string();
    for needle in [
        "browse_memory",
        "modules/<name>/",
        "pitfalls/",
        "move_memory",
        "overview",
        "Repo memory vs agent memory",
        "Rule of thumb",
        "Never put task logs",
    ] {
        assert!(
            instructions.contains(needle),
            "instructions mention {needle}"
        );
    }

    let (err, join) = a
        .tool(
            "add_repo",
            json!({ "repo": "shop", "description": "Online shop" }),
        )
        .await;
    assert!(!err, "{join}");
    assert!(join["next_step"].as_str().unwrap().contains("overview"));

    // overview at the root, everything else in folders
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "title": "Shop overview", "kind": "overview",
                    "body": "Start here." }),
        )
        .await;
    assert!(!err, "{added}");
    assert!(added.get("hints").is_none(), "{added}");

    let (err, folder) = a
        .tool(
            "create_folder",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "description": "Cart, payment and order placement" }),
        )
        .await;
    assert!(!err, "{folder}");
    assert_eq!(folder["created"], true);

    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "title": "Payment retries", "kind": "implementation", "body": "Retries 3x." }),
        )
        .await;
    assert!(!err, "{added}");
    assert_eq!(
        added["created"]["id"],
        "repo/shop/modules/checkout/payment-retries"
    );
    assert_eq!(added["created"]["folder"], "modules/checkout");
    assert!(added.get("hints").is_none(), "{added}");

    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "pitfalls",
                    "title": "Double charge", "kind": "pitfall", "body": "Idempotency keys!" }),
        )
        .await;
    assert!(!err, "{added}");
    let hints = added["hints"].to_string();
    assert!(hints.contains("Created folder 'pitfalls'"), "{hints}");

    // dumping into the root is flagged once it gets crowded
    let mut last = Value::Null;
    for i in 0..4 {
        let (err, added) = a
            .tool(
                "add_memory",
                json!({ "scope": "repo", "repo": "shop", "title": format!("Loose note {i}"),
                        "kind": "pitfall", "body": "x" }),
            )
            .await;
        assert!(!err, "{added}");
        last = added;
    }
    let hints = last["hints"].to_string();
    assert!(hints.contains("Keep only the overview there"), "{hints}");
    assert!(hints.contains("pitfalls/"), "{hints}");
    assert!(hints.contains("move_memory"), "{hints}");

    let (err, moved) = a
        .tool(
            "move_memory",
            json!({ "id": "repo/shop/loose-note-0", "folder": "pitfalls", "expected_version": 1 }),
        )
        .await;
    assert!(!err, "{moved}");
    assert_eq!(moved["moved"]["id"], "repo/shop/pitfalls/loose-note-0");
    assert_eq!(moved["previous_id"], "repo/shop/loose-note-0");
    assert_eq!(moved["moved"]["version"], 2);

    // the tree is the newcomer's map
    let tree = a
        .tool_text("browse_memory", json!({ "scope": "repo", "owner": "shop" }))
        .await;
    assert!(
        tree.starts_with("repo/shop: Online shop (7 memories)\n"),
        "{tree}"
    );
    assert!(tree.contains("repo/shop: Online shop (7 memories)\n  shop-overview  Shop overview [overview]\n  loose-note-1"), "{tree}");
    assert!(tree.contains("\n  modules/ (1)\n    checkout/  Cart, payment and order placement (1)\n      payment-retries  Payment retries [implementation]\n"), "{tree}");
    assert!(tree.contains("\n  pitfalls/ (2)\n"), "{tree}");
    let outline = a
        .tool_text(
            "browse_memory",
            json!({ "owner": "shop", "folder": "modules", "memories": false }),
        )
        .await;
    assert_eq!(
        outline,
        "repo/shop/modules (1 memory)\n  checkout/  Cart, payment and order placement (1)\n"
    );
    let (err, msg) = a
        .tool(
            "browse_memory",
            json!({ "owner": "shop", "folder": "nope" }),
        )
        .await;
    assert!(err && msg.as_str().unwrap().contains("not_found"), "{msg}");

    // folder filters and grouping
    let (_, listed) = a
        .tool(
            "list_memories",
            json!({ "scope": "repo", "owner": "shop", "folder": "pitfalls" }),
        )
        .await;
    assert_eq!(listed["count"], 2);
    let (_, hits) = a
        .tool("search_memory", json!({ "query": "checkout" }))
        .await;
    assert_eq!(
        hits["hits"][0]["id"],
        "repo/shop/modules/checkout/payment-retries"
    );
    let (_, grouped) = a
        .tool(
            "list_memories",
            json!({ "owner": "shop", "group_by": "folder" }),
        )
        .await;
    assert_eq!(
        grouped["groups"]["repo/shop/pitfalls"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // agent space folders; other agents' spaces are read-only
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "agent", "folder": "task-logs/2026", "title": "Docs pass",
                    "kind": "task-log", "body": "done" }),
        )
        .await;
    assert!(!err, "{added}");
    assert_eq!(
        added["created"]["id"],
        "agent/doc-bot/task-logs/2026/docs-pass"
    );
    let mine = a
        .tool_text("browse_memory", json!({ "scope": "agent" }))
        .await;
    assert!(mine.starts_with("agent/doc-bot: docs (1 memory)"), "{mine}");
    assert!(!added["hints"].to_string().contains("scope="), "{added}");

    // memories filed in the wrong scope are flagged
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "title": "Sprint 12 progress",
                    "kind": "task-log", "body": "half done" }),
        )
        .await;
    assert!(!err, "{added}");
    let hints = added["hints"].to_string();
    assert!(hints.contains("belong in your agent notebook"), "{hints}");
    assert!(hints.contains("scope=agent"), "{hints}");
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "agent", "title": "Webhook replay gotcha",
                    "kind": "pitfall", "body": "replays are not signed" }),
        )
        .await;
    assert!(!err, "{added}");
    let hints = added["hints"].to_string();
    assert!(hints.contains("scope=repo, 'pitfalls/'"), "{hints}");
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "agent", "folder": "insights", "title": "Fast test loop",
                    "kind": "insight", "body": "-run Billing" }),
        )
        .await;
    assert!(!err, "{added}");
    assert!(!added["hints"].to_string().contains("scope="), "{added}");

    let mut b = client(&state, "10.2.0.2");
    b.initialize(&[("x-memhub-agent", "other-bot"), ("x-memhub-role", "qa")])
        .await;
    let (err, msg) = b
        .tool(
            "move_memory",
            json!({ "id": "repo/shop/pitfalls/double-charge", "folder": "" }),
        )
        .await;
    assert!(err && msg.as_str().unwrap().contains("add_repo"), "{msg}");
    let (err, msg) = b
        .tool(
            "create_folder",
            json!({ "scope": "repo", "repo": "shop", "folder": "x" }),
        )
        .await;
    assert!(err && msg.as_str().unwrap().contains("forbidden"), "{msg}");
    let (err, msg) = b
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "../escape", "title": "t", "body": "b" }),
        )
        .await;
    assert!(err && msg.as_str().unwrap().contains("invalid"), "{msg}");
    let (err, join) = b.tool("add_repo", json!({ "repo": "shop" })).await;
    assert!(!err);
    assert!(
        join["next_step"]
            .as_str()
            .unwrap()
            .contains("browse_memory"),
        "{join}"
    );
    let (err, moved) = b
        .tool(
            "move_memory",
            json!({ "id": "repo/shop/loose-note-3", "folder": "archive/2026" }),
        )
        .await;
    assert!(!err, "{moved}");

    let events: Vec<String> = hub
        .audit()
        .read(&LogQuery::default())
        .unwrap()
        .iter()
        .filter(|r| r["event"].as_str().unwrap().starts_with("folder_") || r["action"] == "move")
        .map(|r| {
            format!(
                "{} {}",
                r["event"].as_str().unwrap(),
                r["folder"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        events,
        vec![
            "folder_created modules",
            "folder_created modules/checkout",
            "folder_created pitfalls",
            "memory_changed ",
            "folder_created task-logs",
            "folder_created task-logs/2026",
            "folder_created insights",
            "folder_created archive",
            "folder_created archive/2026",
            "memory_changed ",
        ]
    );
}

#[tokio::test]
async fn unicode_content_and_ascii_titles() {
    let (_dir, _hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.3");
    let (_, init) = a
        .initialize(&[("x-memhub-agent", "i18n-bot"), ("x-memhub-role", "docs")])
        .await;
    let instructions = init.unwrap()["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        instructions.contains("accept any Unicode"),
        "{instructions}"
    );
    let (err, _) = a.tool("add_repo", json!({ "repo": "shop" })).await;
    assert!(!err);

    let body = "结账流程 🛒\nリトライは 3 回まで ✅\n→ ≥ € 👨‍👩‍👧";
    let (err, added) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "title": "Checkout flow", "body": body, "tags": ["结账", "🛒"] }),
        )
        .await;
    assert!(!err, "{added}");
    let id = added["created"]["id"].as_str().unwrap().to_string();
    assert_eq!(id, "repo/shop/modules/checkout/checkout-flow");
    assert_eq!(added["created"]["tags"], json!(["结账", "🛒"]));

    let text = a.tool_text("get_memory", json!({ "id": id })).await;
    assert!(text.contains(body), "{text}");
    let (_, hits) = a
        .tool("search_memory", json!({ "query": "リトライ" }))
        .await;
    assert_eq!(hits["hits"][0]["id"], id.as_str());
    let (err, _) = a
        .tool("update_memory", json!({ "id": id, "append": "追加 🎉" }))
        .await;
    assert!(!err);

    let (err, msg) = a
        .tool(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "title": "结账流程", "body": "x" }),
        )
        .await;
    assert!(err);
    let msg = msg.as_str().unwrap();
    assert!(msg.contains("title must be ASCII"), "{msg}");
    assert!(msg.contains("put Chinese, Japanese, emoji"), "{msg}");
    let (err, msg) = a
        .tool("update_memory", json!({ "id": id, "title": "Checkout 🛒" }))
        .await;
    assert!(
        err && msg.as_str().unwrap().contains("title must be ASCII"),
        "{msg}"
    );

    let (_, list) = a
        .send(
            json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list" }),
            &[],
        )
        .await;
    let list = list.unwrap();
    let add = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "add_memory")
        .unwrap()
        .clone();
    let props = &add["inputSchema"]["properties"];
    assert!(props["title"]["description"]
        .as_str()
        .unwrap()
        .contains("ASCII"));
    assert!(props["body"]["description"]
        .as_str()
        .unwrap()
        .contains("Unicode"));
}

#[tokio::test]
async fn full_http_workflow() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));

    // ---- crash agent is created, joins a repo, writes memory
    let mut crash = client(&state, "10.1.0.5");
    let (status, init) = crash.initialize(&[]).await;
    assert_eq!(status, StatusCode::OK);
    let init = init.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("connect_agent"));

    let (_, tools) = crash
        .send(
            json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list" }),
            &[],
        )
        .await;
    assert_eq!(
        tools.unwrap()["result"]["tools"].as_array().unwrap().len(),
        16
    );

    let (err, msg) = crash.tool("search_memory", json!({ "query": "x" })).await;
    assert!(err);
    assert!(msg.as_str().unwrap().contains("not_identified"));

    let (err, msg) = crash
        .tool("connect_agent", json!({ "name": "crash-bot" }))
        .await;
    assert!(err);
    assert!(msg.as_str().unwrap().contains("pass 'role'"), "{msg}");

    let (err, reg) = crash
        .tool(
            "connect_agent",
            json!({ "name": "crash-bot", "role": "crash-analysis" }),
        )
        .await;
    assert!(!err, "{reg}");
    assert_eq!(reg["created"], true);

    let (err, join) = crash
        .tool(
            "add_repo",
            json!({ "repo": "engine", "description": "Game engine" }),
        )
        .await;
    assert!(!err, "{join}");
    assert_eq!(join["created"], true);

    let (err, added) = crash
        .tool(
            "add_memory",
            json!({
                "scope": "repo", "repo": "engine", "title": "Renderer threading model",
                "kind": "pitfall", "tags": ["render", "threads"],
                "body": "The renderer must only be touched from the main thread."
            }),
        )
        .await;
    assert!(!err, "{added}");
    let repo_mem = added["created"]["id"].as_str().unwrap().to_string();
    assert_eq!(repo_mem, "repo/engine/renderer-threading-model");

    let (err, added) = crash
        .tool(
            "add_memory",
            json!({ "scope": "agent", "title": "Crash 1234 analysis", "kind": "task-log",
                    "body": "Root cause: resize from worker thread." }),
        )
        .await;
    assert!(!err, "{added}");
    let agent_mem = added["created"]["id"].as_str().unwrap().to_string();
    assert_eq!(crash.delete().await, StatusCode::NO_CONTENT);

    // ---- perf agent (created via headers) can read but not write others' memory
    let mut perf = client(&state, "10.1.0.6");
    let (status, _) = perf
        .initialize(&[
            ("x-memhub-agent", "perf-bot"),
            ("x-memhub-role", "perf-opt"),
        ])
        .await;
    assert_eq!(status, StatusCode::OK);

    let (err, hits) = perf
        .tool("search_memory", json!({ "query": "thread" }))
        .await;
    assert!(!err);
    assert_eq!(hits["count"], 2);

    let (err, msg) = perf
        .tool(
            "update_memory",
            json!({ "id": agent_mem, "append": "hijack" }),
        )
        .await;
    assert!(err);
    assert!(msg.as_str().unwrap().contains("forbidden"), "{msg}");

    let (err, msg) = perf
        .tool(
            "update_memory",
            json!({ "id": repo_mem, "append": "perf note" }),
        )
        .await;
    assert!(err);
    assert!(msg.as_str().unwrap().contains("add_repo"), "{msg}");

    perf.tool("add_repo", json!({ "repo": "engine" })).await;
    let (err, upd) = perf
        .tool(
            "update_memory",
            json!({ "id": repo_mem, "append": "Batch draw calls.", "expected_version": 1 }),
        )
        .await;
    assert!(!err, "{upd}");
    assert_eq!(upd["updated"]["version"], 2);

    let (err, msg) = perf
        .tool(
            "update_memory",
            json!({ "id": repo_mem, "title": "x", "expected_version": 1 }),
        )
        .await;
    assert!(err);
    assert!(msg.as_str().unwrap().contains("version_conflict"));

    let (err, grouped) = perf
        .tool(
            "group_memories",
            json!({ "group": "rendering", "ids": [repo_mem] }),
        )
        .await;
    assert!(!err, "{grouped}");
    let (_, groups) = perf
        .tool("list_memories", json!({ "group_by": "group" }))
        .await;
    assert_eq!(groups["groups"]["rendering"][0]["id"], repo_mem);

    // get_memory returns the markdown file as text
    perf.next_id += 1;
    let (_, body) = perf
        .send(
            json!({ "jsonrpc": "2.0", "id": perf.next_id, "method": "tools/call",
                    "params": { "name": "get_memory", "arguments": { "id": repo_mem } } }),
            &[],
        )
        .await;
    let text = body.unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.starts_with("---\n"));
    assert!(text.contains("Batch draw calls."));

    // ---- another user of crash-bot connects by name via header
    let mut crash2 = client(&state, "10.1.0.7");
    let (status, _) = crash2.initialize(&[("x-memhub-agent", "ghost-bot")]).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown agent without role"
    );
    let (status, _) = crash2.initialize(&[("x-memhub-agent", "crash-bot")]).await;
    assert_eq!(status, StatusCode::OK);
    let (err, me) = crash2.tool("whoami", json!({})).await;
    assert!(!err);
    assert_eq!(me["agent"]["name"], "crash-bot");
    assert_eq!(me["ip"], "10.1.0.7");
    assert_eq!(me["agent"]["repos"], json!(["engine"]));

    // ---- another user of perf-bot connects by name via the tool
    let mut perf2 = client(&state, "10.1.0.8");
    perf2.initialize(&[]).await;
    let (err, connected) = perf2
        .tool("connect_agent", json!({ "name": "perf-bot" }))
        .await;
    assert!(!err);
    assert_eq!(connected["created"], false);
    assert_eq!(connected["agent"]["role"], "perf-opt");

    // ---- protocol errors
    let (status, _) = client(&state, "10.1.0.9")
        .send(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut ghost = client(&state, "10.1.0.9");
    ghost.session = Some("nope".into());
    let (status, _) = ghost
        .send(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, unknown) = perf2
        .send(
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": { "name": "nope", "arguments": {} } }),
            &[],
        )
        .await;
    assert_eq!(unknown.unwrap()["error"]["code"], -32602);

    state.close_all("server_shutdown");

    // ---- audit trail
    let log = hub.audit().read(&LogQuery::default()).unwrap();
    let connects: Vec<&str> = log
        .iter()
        .filter(|r| r["event"] == "connect")
        .map(|r| r["ip"].as_str().unwrap())
        .collect();
    assert_eq!(
        connects,
        vec!["10.1.0.5", "10.1.0.6", "10.1.0.7", "10.1.0.7", "10.1.0.8"]
    );
    assert!(log
        .iter()
        .filter(|r| r["event"] == "connect")
        .all(|r| r["client"] == "test-client 1.0" && r["transport"] == "http"));

    let disconnects: Vec<(String, String)> = log
        .iter()
        .filter(|r| r["event"] == "disconnect")
        .map(|r| {
            (
                r["agent"].as_str().unwrap_or("-").to_string(),
                r["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(disconnects.len(), 5, "every connect has a disconnect");
    assert!(disconnects.contains(&("crash-bot".into(), "client_closed".into())));
    assert!(disconnects.contains(&("-".into(), "connect_failed".into())));
    assert!(disconnects.contains(&("perf-bot".into(), "server_shutdown".into())));

    let changes: Vec<(String, String)> = log
        .iter()
        .filter(|r| r["event"] == "memory_changed")
        .map(|r| {
            (
                r["agent"].as_str().unwrap().to_string(),
                r["action"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        changes,
        vec![
            ("crash-bot".into(), "add".into()),
            ("crash-bot".into(), "add".into()),
            ("perf-bot".into(), "update".into()),
            ("perf-bot".into(), "group_add".into()),
        ]
    );
    let update = log.iter().find(|r| r["action"] == "update").unwrap();
    assert!(update["diff"]
        .as_str()
        .unwrap()
        .contains("+Batch draw calls."));
    assert_eq!(update["ip"], "10.1.0.6");
}

#[tokio::test]
async fn idle_sessions_are_reaped() {
    let (_dir, hub, state) = setup(Duration::from_millis(50));
    let mut c = client(&state, "192.168.0.2");
    c.initialize(&[]).await;
    c.tool(
        "connect_agent",
        json!({ "name": "sleepy", "role": "feature-impl" }),
    )
    .await;
    assert_eq!(state.session_count(), 1);

    tokio::time::sleep(Duration::from_millis(120)).await;
    state.reap_idle();
    assert_eq!(state.session_count(), 0);

    let log = hub
        .audit()
        .read(&LogQuery {
            event: Some("disconnect".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["reason"], "idle_timeout");
    assert_eq!(log[0]["agent"], "sleepy");
    assert_eq!(log[0]["ip"], "192.168.0.2");

    let (status, _) = c
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn stdio_transport() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let input = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "clientInfo": { "name": "cli" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "connect_agent", "arguments": { "name": "solo", "role": "feature-impl" } } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "add_memory", "arguments": { "scope": "agent", "title": "Hello", "body": "world" } } }),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    let mut out = Vec::new();
    serve(
        hub.clone(),
        StdioLogin::default(),
        input.as_bytes(),
        &mut out,
    )
    .unwrap();
    let responses: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["error"]["code"], -32002);
    assert_eq!(responses[1]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(responses[2]["result"]["isError"], false);
    assert_eq!(
        responses[3]["result"]["structuredContent"]["created"]["id"],
        "agent/solo/hello"
    );

    let log = hub.audit().read(&LogQuery::default()).unwrap();
    assert_eq!(log.first().unwrap()["event"], "connect");
    assert_eq!(log.first().unwrap()["ip"], "local");
    assert_eq!(log.last().unwrap()["event"], "disconnect");
    assert_eq!(log.last().unwrap()["reason"], "stdio_eof");
    assert_eq!(log.last().unwrap()["agent"], "solo");

    // A second process for the same agent, identified via --agent.
    let input = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "cli" } } }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "whoami", "arguments": {} } }),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let mut out = Vec::new();
    let login = StdioLogin {
        agent: Some("solo".into()),
        role: None,
        instance: Some("  ci-runner-7\n".into()),
    };
    serve(hub.clone(), login, input.as_bytes(), &mut out).unwrap();
    let whoami: Value =
        serde_json::from_str(String::from_utf8(out).unwrap().lines().nth(1).unwrap()).unwrap();
    assert_eq!(
        whoami["result"]["structuredContent"]["agent"]["name"],
        "solo"
    );
    let log = hub.audit().read(&LogQuery::default()).unwrap();
    let connect = log.iter().rev().find(|r| r["event"] == "connect").unwrap();
    assert_eq!(connect["instance"], "ci-runner-7");
}
