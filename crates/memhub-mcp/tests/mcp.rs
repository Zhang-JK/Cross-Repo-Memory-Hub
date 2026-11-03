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

const CURATOR_TOKEN: &str = "test-curator-token";

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
        let value = (!bytes.is_empty()).then(|| {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        });
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

    /// Calls a tool that must succeed and returns its structuredContent.
    async fn ok(&mut self, name: &str, args: Value) -> Value {
        let (err, out) = self.tool(name, args).await;
        assert!(!err, "{name}: {out}");
        out
    }

    /// Calls a tool that must fail and returns the error text.
    async fn fails(&mut self, name: &str, args: Value) -> String {
        let (err, out) = self.tool(name, args).await;
        assert!(err, "{name} should fail: {out}");
        out.as_str().unwrap().to_string()
    }

    async fn tool_names(&mut self) -> Vec<String> {
        self.next_id += 1;
        let (_, list) = self
            .send(
                json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "tools/list" }),
                &[],
            )
            .await;
        list.unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
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
            curator_token: Some(CURATOR_TOKEN.into()),
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

/// A curator session (the hub's LLM curator, or a test standing in for it).
async fn curator(state: &Arc<HttpState>, ip: &str) -> (Client, Value) {
    let mut c = client(state, ip);
    let (status, init) = c
        .initialize(&[
            ("x-memhub-agent", "memhub-curator"),
            ("x-memhub-role", "curator"),
            ("x-memhub-curator-token", CURATOR_TOKEN),
        ])
        .await;
    assert_eq!(status, StatusCode::OK, "{init:?}");
    (c, init.unwrap())
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
async fn instructions_describe_the_layers() {
    let (_dir, _hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.9");
    let (_, init) = a
        .initialize(&[("x-memhub-agent", "doc-bot"), ("x-memhub-role", "docs")])
        .await;
    let result = init.unwrap()["result"].clone();
    assert_eq!(result["serverInfo"]["name"], "memhub2");
    let instructions = result["instructions"].as_str().unwrap();
    for needle in [
        "THE THREE LAYERS",
        "Experience (what you search and read)",
        "Evidence (what you write)",
        "Work area (your scratch space)",
        "record_evidence",
        "search_evidence",
        "rate_memory",
        "dispute_memory",
        "established",
        "provisional",
        "derived_from",
        "candidate_lesson",
        "agent/<you>/working/",
        "accept any Unicode",
    ] {
        assert!(instructions.contains(needle), "instructions mention {needle}");
    }
    assert!(!instructions.contains("CURATOR SESSION"));

    let (_, init) = curator(&state, "10.2.0.10").await;
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("CURATOR SESSION"));
    assert!(instructions.contains("curation_batch"));
}

#[tokio::test]
async fn curator_token_gates_curation_tools() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));

    let mut agent = client(&state, "10.3.0.1");
    agent
        .initialize(&[("x-memhub-agent", "dev-bot"), ("x-memhub-role", "dev")])
        .await;
    let names = agent.tool_names().await;
    for name in ["record_evidence", "search_evidence", "rate_memory", "dispute_memory", "add_memory"] {
        assert!(names.contains(&name.to_string()), "{name}");
    }
    for name in ["curation_batch", "propose_changes", "review_changes", "import_memory"] {
        assert!(!names.contains(&name.to_string()), "{name}");
    }
    let msg = agent
        .fails("curation_status", json!({}))
        .await;
    assert!(msg.contains("forbidden") || msg.contains("curator"), "{msg}");

    let (mut cur, _) = curator(&state, "10.3.0.2").await;
    let names = cur.tool_names().await;
    for name in ["curation_status", "curation_batch", "curation_backfill", "propose_changes",
                 "review_changes", "list_proposals", "get_proposal", "import_memory"] {
        assert!(names.contains(&name.to_string()), "{name}");
    }
    let me = cur.ok("whoami", json!({})).await;
    assert_eq!(me["agent"]["name"], "memhub-curator");

    // A wrong token is refused outright instead of silently giving an agent session.
    let mut bad = client(&state, "10.3.0.3");
    let (status, _) = bad
        .initialize(&[
            ("x-memhub-agent", "memhub-curator"),
            ("x-memhub-role", "curator"),
            ("x-memhub-curator-token", "guess"),
        ])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Without a configured token there are no curator sessions at all.
    let closed = HttpState::new(hub, HttpConfig::default());
    let mut c = client(&closed, "10.3.0.4");
    let (status, _) = c
        .initialize(&[("x-memhub-agent", "x"), ("x-memhub-role", "y"), ("x-memhub-curator-token", CURATOR_TOKEN)])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn folder_hierarchy_workflow() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.1");
    a.initialize(&[("x-memhub-agent", "doc-bot"), ("x-memhub-role", "docs")])
        .await;
    let join = a
        .ok("add_repo", json!({ "repo": "shop", "description": "Online shop" }))
        .await;
    assert!(join["next_step"].as_str().unwrap().contains("record_evidence"), "{join}");

    // The curator builds the repo tree: overview at the root, the rest in folders.
    let (mut c, _) = curator(&state, "10.2.0.2").await;
    let added = c
        .ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "title": "Shop overview", "kind": "overview",
                    "body": "Start here." }),
        )
        .await;
    assert!(added.get("hints").is_none(), "{added}");
    let folder = c
        .ok(
            "create_folder",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "description": "Cart, payment and order placement" }),
        )
        .await;
    assert_eq!(folder["created"], true);
    let added = c
        .ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "title": "Payment retries", "kind": "implementation", "body": "Retries 3x." }),
        )
        .await;
    assert_eq!(added["created"]["id"], "repo/shop/modules/checkout/payment-retries");
    assert_eq!(added["created"]["folder"], "modules/checkout");
    let added = c
        .ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "pitfalls",
                    "title": "Double charge", "kind": "pitfall", "body": "Idempotency keys!" }),
        )
        .await;
    let hints = added["hints"].to_string();
    assert!(hints.contains("Created folder 'pitfalls'"), "{hints}");
    assert!(hints.contains(r#"scope=\"repo\", repo=\"shop\""#), "{hints}");
    c.ok(
        "add_memory",
        json!({ "scope": "repo", "repo": "shop", "title": "Loose note", "kind": "pitfall", "body": "x" }),
    )
    .await;
    let moved = c
        .ok(
            "move_memory",
            json!({ "id": "repo/shop/loose-note", "folder": "pitfalls", "expected_version": 1 }),
        )
        .await;
    assert_eq!(moved["moved"]["id"], "repo/shop/pitfalls/loose-note");
    assert_eq!(moved["previous_id"], "repo/shop/loose-note");
    assert_eq!(moved["moved"]["version"], 2);

    // The tree is the newcomer's map.
    let tree = a
        .tool_text("browse_memory", json!({ "scope": "repo", "owner": "shop" }))
        .await;
    assert!(
        tree.starts_with("repo/shop: Online shop (4 memories)\n  shop-overview  Shop overview [overview]\n"),
        "{tree}"
    );
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
    let msg = a
        .fails("browse_memory", json!({ "owner": "shop", "folder": "nope" }))
        .await;
    assert!(msg.contains("not_found"), "{msg}");
    // The old id of a moved memory redirects.
    let text = a
        .tool_text("get_memory", json!({ "id": "repo/shop/loose-note" }))
        .await;
    assert!(text.starts_with("[memhub] repo/shop/loose-note now lives at repo/shop/pitfalls/loose-note."), "{text}");

    // Folder filters and grouping.
    let listed = a
        .ok("list_memories", json!({ "scope": "repo", "owner": "shop", "folder": "pitfalls" }))
        .await;
    assert_eq!(listed["count"], 2);
    let hits = a.ok("search_memory", json!({ "query": "checkout" })).await;
    assert_eq!(hits["hits"][0]["id"], "repo/shop/modules/checkout/payment-retries");
    let grouped = a
        .ok("list_memories", json!({ "owner": "shop", "group_by": "folder" }))
        .await;
    assert_eq!(grouped["groups"]["repo/shop/pitfalls"].as_array().unwrap().len(), 2);

    // Agents cannot reshape curated memory.
    let msg = a
        .fails("create_folder", json!({ "scope": "repo", "repo": "shop", "folder": "x" }))
        .await;
    assert!(msg.contains("forbidden") && msg.contains("record_evidence"), "{msg}");
    let msg = a
        .fails("move_memory", json!({ "id": "repo/shop/pitfalls/double-charge", "folder": "" }))
        .await;
    assert!(msg.contains("forbidden"), "{msg}");
    let msg = a
        .fails(
            "group_memories",
            json!({ "group": "payments", "ids": ["repo/shop/pitfalls/double-charge"] }),
        )
        .await;
    assert!(msg.contains("forbidden"), "{msg}");
    let msg = a
        .fails(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "../escape", "title": "t", "body": "b" }),
        )
        .await;
    assert!(msg.contains("invalid"), "{msg}");

    // The work area: agent notes default to working/, subfolders per task.
    let note = a
        .ok("add_memory", json!({ "scope": "agent", "title": "Docs plan", "kind": "plan", "body": "1. api" }))
        .await;
    assert_eq!(note["created"]["id"], "agent/doc-bot/working/docs-plan");
    assert_eq!(note["created"]["work_note"], true);
    let note = a
        .ok(
            "add_memory",
            json!({ "scope": "agent", "folder": "working/api-docs", "title": "Open questions",
                    "kind": "notes", "body": "auth?" }),
        )
        .await;
    let hints = note["hints"].to_string();
    assert!(hints.contains(r#"create_folder(scope=\"agent\", folder=\"working/api-docs\""#), "{hints}");
    a.ok(
        "create_folder",
        json!({ "scope": "agent", "folder": "working/api-docs", "description": "API docs pass" }),
    )
    .await;
    a.ok(
        "move_memory",
        json!({ "id": "agent/doc-bot/working/docs-plan", "folder": "working/api-docs" }),
    )
    .await;
    let mine = a.tool_text("browse_memory", json!({ "scope": "agent" })).await;
    assert!(mine.starts_with("agent/doc-bot: docs (2 memories)"), "{mine}");
    assert!(mine.contains("api-docs/  API docs pass (2)"), "{mine}");

    // Another agent can read the work area but not change it, and does not
    // find it by searching.
    let mut b = client(&state, "10.2.0.3");
    b.initialize(&[("x-memhub-agent", "other-bot"), ("x-memhub-role", "qa")])
        .await;
    let msg = b
        .fails(
            "update_memory",
            json!({ "id": "agent/doc-bot/working/api-docs/open-questions", "append": "hijack" }),
        )
        .await;
    assert!(msg.contains("writable only by that agent"), "{msg}");
    let hits = b
        .ok("search_memory", json!({ "query": "questions", "owner": "doc-bot" }))
        .await;
    assert_eq!(hits["count"], 0, "{hits}");

    // Retired memories drop out of the tree unless asked for.
    let proposal = c
        .ok(
            "propose_changes",
            json!({ "target": "repo:shop", "ops": [
                { "op": "retire", "id": "repo/shop/pitfalls/loose-note", "reason": "no content" }
            ] }),
        )
        .await;
    let reviewed = c
        .ok(
            "review_changes",
            json!({ "proposal_id": proposal["id"], "verdicts": [{ "index": 0, "approve": true }] }),
        )
        .await;
    assert_eq!(reviewed["state"], "applied", "{reviewed}");
    let tree = a
        .tool_text("browse_memory", json!({ "owner": "shop", "folder": "pitfalls" }))
        .await;
    assert_eq!(
        tree,
        "repo/shop/pitfalls (1 memory)\n  double-charge  Double charge [pitfall]\n\
         (1 superseded memory hidden; include_superseded=true shows them)\n"
    );
    let tree = a
        .tool_text(
            "browse_memory",
            json!({ "owner": "shop", "folder": "pitfalls", "include_superseded": true }),
        )
        .await;
    assert!(tree.contains("loose-note  Loose note [pitfall, superseded]"), "{tree}");

    let events: Vec<String> = hub
        .audit()
        .read(&LogQuery::default())
        .unwrap()
        .iter()
        .filter(|r| r["event"] == "folder_created")
        .map(|r| format!("{} {}", r["owner"].as_str().unwrap(), r["folder"].as_str().unwrap()))
        .collect();
    assert_eq!(
        events,
        vec![
            "shop modules",
            "shop modules/checkout",
            "shop pitfalls",
            "doc-bot working",
            "doc-bot working/api-docs",
        ]
    );
}

#[tokio::test]
async fn agent_writes_outside_the_work_area_become_evidence() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.5");
    a.initialize(&[("x-memhub-agent", "old-habits"), ("x-memhub-role", "dev")])
        .await;
    a.ok("add_repo", json!({ "repo": "shop" })).await;

    let out = a
        .ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "pitfalls", "title": "Refund webhook twice",
                    "kind": "pitfall", "body": "Deduplicate by event id.", "tags": ["refund"] }),
        )
        .await;
    assert_eq!(out["layer"], "evidence");
    let id = out["recorded"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("evidence/") && id.contains("/old-habits/"), "{id}");
    assert_eq!(out["recorded"]["repos"], json!(["shop"]));
    let hints = out["hints"].to_string();
    assert!(hints.contains("record_evidence"), "{hints}");
    let out = a
        .ok(
            "add_memory",
            json!({ "scope": "agent", "folder": "insights", "title": "Fast test loop", "body": "-run Billing" }),
        )
        .await;
    assert_eq!(out["layer"], "evidence");
    assert!(out["recorded"].get("repos").is_none(), "{out}");

    let ev = a.ok("get_memory", json!({ "id": id })).await;
    assert_eq!(ev["layer"], "evidence");
    let tags = ev["tags"].to_string();
    assert!(tags.contains("folder:pitfalls"), "{ev}");

    // Nothing reached curated memory; evidence is not in search_memory.
    let hits = a.ok("search_memory", json!({ "query": "refund" })).await;
    assert_eq!(hits["count"], 0);
    let found = a
        .ok("search_evidence", json!({ "query": "refund", "repo": "shop" }))
        .await;
    assert_eq!(found["count"], 1);
    assert!(hub.store().list(&Default::default()).unwrap().is_empty());
}

#[tokio::test]
async fn unicode_content_and_ascii_titles() {
    let (_dir, _hub, state) = setup(Duration::from_secs(600));
    let mut a = client(&state, "10.2.0.3");
    a.initialize(&[("x-memhub-agent", "i18n-bot"), ("x-memhub-role", "docs")])
        .await;
    a.ok("add_repo", json!({ "repo": "shop" })).await;
    let (mut c, _) = curator(&state, "10.2.0.4").await;

    let body = "结账流程 🛒\nリトライは 3 回まで ✅\n→ ≥ € 👨‍👩‍👧";
    let added = c
        .ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": "modules/checkout",
                    "title": "Checkout flow", "body": body, "tags": ["结账", "🛒"] }),
        )
        .await;
    let id = added["created"]["id"].as_str().unwrap().to_string();
    assert_eq!(id, "repo/shop/modules/checkout/checkout-flow");
    assert_eq!(added["created"]["tags"], json!(["结账", "🛒"]));

    let text = a.tool_text("get_memory", json!({ "id": id })).await;
    assert!(text.contains(body), "{text}");
    let hits = a.ok("search_memory", json!({ "query": "リトライ" })).await;
    assert_eq!(hits["hits"][0]["id"], id.as_str());
    c.ok("update_memory", json!({ "id": id, "append": "追加 🎉" })).await;

    let msg = c
        .fails(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "title": "结账流程", "body": "x" }),
        )
        .await;
    assert!(msg.contains("title must be ASCII"), "{msg}");
    assert!(msg.contains("put Chinese, Japanese, emoji"), "{msg}");
    let msg = c
        .fails("update_memory", json!({ "id": id, "title": "Checkout 🛒" }))
        .await;
    assert!(msg.contains("title must be ASCII"), "{msg}");

    // Evidence and work notes follow the same rule.
    let msg = a
        .fails(
            "record_evidence",
            json!({ "title": "结账 bug", "body": "x", "repos": ["shop"] }),
        )
        .await;
    assert!(msg.contains("title must be ASCII"), "{msg}");
    let out = a
        .ok(
            "record_evidence",
            json!({ "title": "Checkout retry limit", "body": body, "repos": ["shop"],
                    "candidate_lesson": "リトライは 3 回まで", "tags": ["结账"] }),
        )
        .await;
    let ev = a
        .tool_text("get_memory", json!({ "id": out["recorded"]["id"] }))
        .await;
    assert!(ev.contains(body) && ev.contains("リトライは 3 回まで"), "{ev}");

    a.next_id += 1;
    let (_, list) = a
        .send(json!({ "jsonrpc": "2.0", "id": a.next_id, "method": "tools/list" }), &[])
        .await;
    let list = list.unwrap();
    let tool = |name: &str| {
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()
            .clone()
    };
    for name in ["add_memory", "record_evidence"] {
        let props = tool(name)["inputSchema"]["properties"].clone();
        assert!(props["title"]["description"].as_str().unwrap().contains("ASCII"), "{name}");
        let body = props["body"]["description"].as_str().unwrap();
        assert!(body.contains("Unicode") || body.contains("any language"), "{name}");
    }
}

#[tokio::test]
async fn full_http_workflow() {
    let (_dir, hub, state) = setup(Duration::from_secs(600));

    // ---- crash agent is created, joins a repo, records evidence
    let mut crash = client(&state, "10.1.0.5");
    let (status, init) = crash.initialize(&[]).await;
    assert_eq!(status, StatusCode::OK);
    let init = init.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("connect_agent"));
    assert_eq!(crash.tool_names().await.len(), 20);

    let msg = crash.fails("search_memory", json!({ "query": "x" })).await;
    assert!(msg.contains("not_identified"));
    let msg = crash.fails("connect_agent", json!({ "name": "crash-bot" })).await;
    assert!(msg.contains("pass 'role'"), "{msg}");
    let reg = crash
        .ok("connect_agent", json!({ "name": "crash-bot", "role": "crash-analysis" }))
        .await;
    assert_eq!(reg["created"], true);
    let join = crash
        .ok("add_repo", json!({ "repo": "engine", "description": "Game engine" }))
        .await;
    assert_eq!(join["created"], true);

    let recorded = crash
        .ok(
            "record_evidence",
            json!({
                "title": "Renderer is main-thread only", "kind": "failure",
                "body": "Resizing from a worker thread crashed in wgpu (CRASH-1234). Moving the resize to the main loop fixed it.",
                "repos": ["engine"], "task": "CRASH-1234 resize crash", "outcome": "success",
                "reason": "anyone touching the renderer from jobs will crash",
                "candidate_lesson": "Only touch the renderer from the main thread.",
                "code_refs": ["src/render/mod.rs:42@abc123"], "importance": 4,
                "tags": ["render", "threads"]
            }),
        )
        .await;
    let evidence_id = recorded["recorded"]["id"].as_str().unwrap().to_string();
    assert!(evidence_id.starts_with("evidence/") && evidence_id.contains("/crash-bot/"), "{evidence_id}");
    assert_eq!(recorded["recorded"]["source"], "observed");
    assert!(recorded.get("hints").is_none(), "{recorded}");
    let msg = crash
        .fails("record_evidence", json!({ "title": "x", "body": "y", "repos": ["other"] }))
        .await;
    assert!(msg.contains("add_repo"), "{msg}");

    let note = crash
        .ok(
            "add_memory",
            json!({ "scope": "agent", "title": "Crash 1234 analysis", "kind": "notes",
                    "body": "Root cause: resize from worker thread." }),
        )
        .await;
    let work_note = note["created"]["id"].as_str().unwrap().to_string();
    assert_eq!(work_note, "agent/crash-bot/working/crash-1234-analysis");
    assert_eq!(crash.delete().await, StatusCode::NO_CONTENT);

    // ---- perf agent (created via headers): no curated memory yet, but evidence is there
    let mut perf = client(&state, "10.1.0.6");
    let (status, _) = perf
        .initialize(&[("x-memhub-agent", "perf-bot"), ("x-memhub-role", "perf-opt")])
        .await;
    assert_eq!(status, StatusCode::OK);
    let hits = perf.ok("search_memory", json!({ "query": "renderer thread" })).await;
    assert_eq!(hits["count"], 0);
    assert!(hits["hints"].to_string().contains("search_evidence"), "{hits}");
    let found = perf.ok("search_evidence", json!({ "query": "renderer" })).await;
    assert_eq!(found["count"], 1);
    assert_eq!(found["hits"][0]["id"], evidence_id.as_str());
    let text = perf.tool_text("get_memory", json!({ "id": evidence_id })).await;
    assert!(text.starts_with("[memhub] Evidence: a raw, unreviewed observation"), "{text}");
    assert!(text.contains("main loop"), "{text}");
    let msg = perf
        .fails("update_memory", json!({ "id": work_note, "append": "hijack" }))
        .await;
    assert!(msg.contains("forbidden"), "{msg}");

    // ---- the curator turns the evidence into repo memory
    let (mut cur, _) = curator(&state, "10.1.0.10").await;
    let status = cur.ok("curation_status", json!({})).await;
    let engine = status["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["target"] == "repo:engine")
        .cloned()
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(engine["pending_evidence"], 1, "{status}");
    let batch = cur.ok("curation_batch", json!({ "target": "repo:engine" })).await;
    assert_eq!(batch["evidence"].as_array().unwrap().len(), 1, "{batch}");
    assert_eq!(batch["evidence"][0]["id"], evidence_id.as_str());
    let cursor = batch["cursor"].as_str().unwrap().to_string();
    let op = json!({
        "op": "add", "scope": "repo", "owner": "engine", "folder": "pitfalls",
        "title": "Renderer threading model", "kind": "pitfall",
        "body": "The renderer must only be touched from the main thread; resize from a worker crashes.",
        "applies_to": ["src/render/"],
        "cites": { "evidence": [evidence_id], "code": ["src/render/mod.rs:42@abc123"] }
    });
    let dry = cur
        .ok("propose_changes", json!({ "target": "repo:engine", "ops": [op], "dry_run": true }))
        .await;
    assert_eq!(dry["checks"][0]["ok"], true, "{dry}");
    let msg = cur
        .fails("propose_changes", json!({ "target": "repo:engine", "ops": [{ "op": "rewrite" }] }))
        .await;
    assert!(msg.contains("malformed operations"), "{msg}");
    let proposal = cur
        .ok(
            "propose_changes",
            json!({ "target": "repo:engine", "ops": [op], "cursor": cursor, "note": "renderer pitfall" }),
        )
        .await;
    let proposal_id = proposal["id"].as_str().unwrap().to_string();
    let reviewed = cur
        .ok(
            "review_changes",
            json!({ "proposal_id": proposal_id, "verdicts": [{ "index": 0, "approve": true }] }),
        )
        .await;
    assert_eq!(reviewed["state"], "applied", "{reviewed}");
    let repo_mem = reviewed["decisions"][0]["applied_id"].as_str().unwrap().to_string();
    assert_eq!(repo_mem, "repo/engine/pitfalls/renderer-threading-model");
    let batch = cur.ok("curation_batch", json!({ "target": "repo:engine" })).await;
    assert!(batch["evidence"].as_array().unwrap().is_empty(), "watermark advanced");

    // ---- agents find it, rate it and dispute it
    let hits = perf.ok("search_memory", json!({ "query": "renderer thread" })).await;
    assert_eq!(hits["count"], 1);
    assert_eq!(hits["hits"][0]["id"], repo_mem.as_str());
    assert_eq!(hits["hits"][0]["status"], "provisional");
    assert_eq!(hits["hits"][0]["cites"], 2);
    let text = perf.tool_text("get_memory", json!({ "id": repo_mem })).await;
    assert!(text.starts_with("[memhub] provisional: backed by 1 independent source(s)"), "{text}");
    assert!(text.contains(&evidence_id), "{text}");
    let msg = perf
        .fails("update_memory", json!({ "id": repo_mem, "append": "perf note" }))
        .await;
    assert!(msg.contains("add_repo") || msg.contains("forbidden"), "{msg}");
    perf.ok("add_repo", json!({ "repo": "engine" })).await;
    let msg = perf
        .fails("update_memory", json!({ "id": repo_mem, "append": "perf note" }))
        .await;
    assert!(msg.contains("forbidden") && msg.contains("dispute_memory"), "{msg}");

    let rated = perf
        .ok("rate_memory", json!({ "id": repo_mem, "rating": "helpful", "note": "saved a crash" }))
        .await;
    assert_eq!(rated["feedback"]["helpful"], 1);
    let disputed = perf
        .ok(
            "dispute_memory",
            json!({ "id": repo_mem, "reason": "Resize is safe from workers since the render queue landed.",
                    "code_refs": ["src/render/queue.rs:10@def456"] }),
        )
        .await;
    assert_eq!(disputed["disputed"]["status"], "disputed");
    let dispute_id = disputed["evidence"]["id"].as_str().unwrap().to_string();
    let hits = perf.ok("search_memory", json!({ "query": "renderer" })).await;
    assert!(hits["hints"].to_string().contains("disputed"), "{hits}");
    let text = perf.tool_text("get_memory", json!({ "id": repo_mem })).await;
    assert!(text.starts_with("[memhub] DISPUTED"), "{text}");
    let batch = cur.ok("curation_batch", json!({ "target": "repo:engine" })).await;
    assert_eq!(batch["evidence"][0]["id"], dispute_id.as_str(), "{batch}");
    assert!(!batch["feedback"].as_array().unwrap().is_empty(), "{batch}");

    // ---- another user of crash-bot connects by name via header
    let mut crash2 = client(&state, "10.1.0.7");
    let (status, _) = crash2.initialize(&[("x-memhub-agent", "ghost-bot")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown agent without role");
    let (status, _) = crash2.initialize(&[("x-memhub-agent", "crash-bot")]).await;
    assert_eq!(status, StatusCode::OK);
    let me = crash2.ok("whoami", json!({})).await;
    assert_eq!(me["agent"]["name"], "crash-bot");
    assert_eq!(me["ip"], "10.1.0.7");
    assert_eq!(me["agent"]["repos"], json!(["engine"]));
    crash2
        .ok("update_memory", json!({ "id": work_note, "append": "Shared by every crash-bot user." }))
        .await;

    // ---- another user of perf-bot connects by name via the tool
    let mut perf2 = client(&state, "10.1.0.8");
    perf2.initialize(&[]).await;
    let connected = perf2.ok("connect_agent", json!({ "name": "perf-bot" })).await;
    assert_eq!(connected["created"], false);
    assert_eq!(connected["agent"]["role"], "perf-opt");

    // ---- protocol errors
    let (status, _) = client(&state, "10.1.0.9")
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), &[])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut ghost = client(&state, "10.1.0.9");
    ghost.session = Some("nope".into());
    let (status, _) = ghost
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), &[])
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
    let (_, hidden) = perf2
        .send(
            json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                    "params": { "name": "curation_batch", "arguments": { "target": "repo:engine" } } }),
            &[],
        )
        .await;
    let hidden = hidden.unwrap();
    assert!(hidden["result"]["isError"] == true || hidden.get("error").is_some(), "{hidden}");

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
        vec!["10.1.0.5", "10.1.0.6", "10.1.0.10", "10.1.0.7", "10.1.0.7", "10.1.0.8"]
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
    assert_eq!(disconnects.len(), 6, "every connect has a disconnect");
    assert!(disconnects.contains(&("crash-bot".into(), "client_closed".into())));
    assert!(disconnects.contains(&("-".into(), "connect_failed".into())));
    assert!(disconnects.contains(&("perf-bot".into(), "server_shutdown".into())));
    assert!(disconnects.contains(&("memhub-curator".into(), "server_shutdown".into())));

    let events = |name: &str| -> Vec<Value> {
        log.iter().filter(|r| r["event"] == name).cloned().collect()
    };
    let recorded: Vec<String> = events("evidence_recorded")
        .iter()
        .map(|r| r["agent"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(recorded, ["crash-bot", "perf-bot"]);
    assert_eq!(events("memory_rated").len(), 1);
    assert_eq!(events("memory_disputed").len(), 1);
    assert_eq!(events("curation_proposed").len(), 1);
    assert_eq!(events("curation_reviewed").len(), 1);
    let searches = events("memory_searched");
    assert!(searches.iter().any(|r| r["hits"] == 0 && r["agent"] == "perf-bot"), "{searches:?}");
    let changes: Vec<(String, String)> = events("memory_changed")
        .iter()
        .map(|r| {
            (
                r["agent"].as_str().unwrap().to_string(),
                r["action"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(changes[0], ("crash-bot".to_string(), "add".to_string()));
    assert!(changes.contains(&("memhub-curator".to_string(), "add".to_string())), "{changes:?}");
    assert!(changes.contains(&("crash-bot".to_string(), "update".to_string())), "{changes:?}");
    let update = log
        .iter()
        .find(|r| r["event"] == "memory_changed" && r["action"] == "update" && r["agent"] == "crash-bot")
        .unwrap();
    assert!(update["diff"].as_str().unwrap().contains("+Shared by every crash-bot user."));
    assert_eq!(update["ip"], "10.1.0.7");
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

    // The client keeps using its session id: the session is reopened under
    // the same id, still identified, instead of answering 404.
    let old_id = c.session.clone();
    let (is_error, created) = c
        .tool(
            "add_memory",
            json!({ "scope": "agent", "title": "After idle", "body": "still me" }),
        )
        .await;
    assert!(!is_error, "{created}");
    assert_eq!(c.session, old_id);
    assert_eq!(state.session_count(), 1);
    assert_eq!(state.resumable_count(), 0);

    let connects = hub
        .audit()
        .read(&LogQuery {
            event: Some("connect".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(connects.len(), 2);
    assert!(connects[0].get("resumed").is_none());
    assert_eq!(connects[1]["resumed"], true);
    assert_eq!(connects[1]["session_id"], connects[0]["session_id"]);
    assert_eq!(connects[1]["client"], "test-client 1.0");
    let change = hub
        .audit()
        .read(&LogQuery {
            event: Some("memory_changed".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(change[0]["agent"], "sleepy");
    assert_eq!(change[0]["session_id"], connects[0]["session_id"]);

    // Expire again, then end it explicitly: the resumable record goes away.
    tokio::time::sleep(Duration::from_millis(120)).await;
    state.reap_idle();
    assert_eq!(state.resumable_count(), 1);
    assert_eq!(c.delete().await, StatusCode::NO_CONTENT);
    assert_eq!(state.resumable_count(), 0);
    let (status, _) = c
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resume_fills_identity_from_agent_header() {
    let (_dir, _hub, state) = setup(Duration::from_millis(50));
    let mut c = client(&state, "192.168.0.3");
    c.initialize(&[]).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    state.reap_idle();

    // The old session was never identified; the header (re-sent with every
    // request by MCP clients) identifies the resumed one.
    c.next_id += 1;
    let (status, body) = c
        .send(
            json!({ "jsonrpc": "2.0", "id": c.next_id, "method": "tools/call",
                    "params": { "name": "whoami", "arguments": {} } }),
            &[
                ("x-memhub-agent", "late-name"),
                ("x-memhub-role", "feature-impl"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let text = body.unwrap()["result"].to_string();
    assert!(text.contains("late-name"), "{text}");
}

#[tokio::test]
async fn expired_sessions_outliving_resume_ttl_are_gone() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let state = HttpState::new(
        hub,
        HttpConfig {
            idle_timeout: Duration::from_millis(50),
            resume_ttl: Duration::ZERO,
            ..Default::default()
        },
    );
    let mut c = client(&state, "192.168.0.4");
    c.initialize(&[]).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    state.reap_idle();
    assert_eq!(state.resumable_count(), 0);
    let (status, _) = c
        .send(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_resume_after_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let resume_file = dir.path().join("run").join("resumable-sessions.json");
    let config = HttpConfig {
        resume_file: Some(resume_file.clone()),
        ..Default::default()
    };
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let state = HttpState::new(hub.clone(), config.clone());
    let mut c = client(&state, "192.168.0.5");
    c.initialize(&[("x-memhub-instance", "alice-laptop")]).await;
    c.tool(
        "connect_agent",
        json!({ "name": "survivor", "role": "feature-impl" }),
    )
    .await;
    state.close_all("server_shutdown");
    drop(state);
    assert!(resume_file.exists());

    let restarted = HttpState::new(hub.clone(), config.clone());
    assert!(!resume_file.exists(), "the resume file is read once");
    assert_eq!(restarted.resumable_count(), 1);
    c.app = router(restarted.clone()).layer(MockConnectInfo(
        "192.168.0.5:40000".parse::<SocketAddr>().unwrap(),
    ));
    let (is_error, created) = c
        .tool(
            "add_memory",
            json!({ "scope": "agent", "title": "After restart", "body": "same agent" }),
        )
        .await;
    assert!(!is_error, "{created}");

    let connects = hub
        .audit()
        .read(&LogQuery {
            event: Some("connect".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(connects.len(), 2);
    assert_eq!(connects[1]["resumed"], true);
    assert_eq!(connects[1]["instance"], "alice-laptop");
    let logins = hub
        .audit()
        .read(&LogQuery {
            event: Some("agent_login".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0]["agent"], "survivor");
    assert_eq!(logins[0]["session_id"], connects[0]["session_id"]);

    // A third start without a clean shutdown has nothing left to resume.
    drop(restarted);
    let again = HttpState::new(hub, config);
    assert_eq!(again.resumable_count(), 0);
}

impl Client {
    async fn admin_get(&self, uri: &str) -> Value {
        let res = self
            .app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

async fn read(c: &mut Client, id: &str) -> (bool, Value) {
    c.tool("get_memory", json!({ "id": id })).await
}

#[tokio::test]
async fn memory_reads_are_counted_and_ranked() {
    let (dir, hub, state) = setup(Duration::from_secs(3600));
    let mut a = client(&state, "10.0.0.1");
    a.initialize(&[("x-memhub-agent", "alice"), ("x-memhub-role", "dev")])
        .await;
    let mut b = client(&state, "10.0.0.2");
    b.initialize(&[("x-memhub-agent", "bob"), ("x-memhub-role", "dev")])
        .await;
    a.tool("add_repo", json!({ "repo": "shop" })).await;
    let (mut c, _) = curator(&state, "10.0.0.4").await;
    for (folder, title) in [
        ("", "Overview"),
        ("modules/cart", "Cart rules"),
        ("modules/cart", "Cart pitfalls"),
        ("modules/auth", "Login flow"),
    ] {
        c.ok(
            "add_memory",
            json!({ "scope": "repo", "repo": "shop", "folder": folder, "title": title, "body": "x" }),
        )
        .await;
    }

    for _ in 0..3 {
        read(&mut a, "repo/shop/modules/cart/cart-rules").await;
    }
    let (_, out) = read(&mut b, "repo/shop/modules/cart/cart-rules").await;
    assert_eq!(out["read_count"], 4);
    read(&mut b, "repo/shop/modules/auth/login-flow").await;
    read(&mut b, "repo/shop/overview").await;
    read(&mut a, "repo/shop/overview").await;

    // Listing, browsing and searching are not reads; nor is the admin UI,
    // nor the curator's reads.
    a.tool("list_memories", json!({})).await;
    a.tool("search_memory", json!({ "query": "cart" })).await;
    let (_, out) = read(&mut c, "repo/shop/modules/cart/cart-rules").await;
    assert_eq!(out["read_count"], 4);
    let detail = a
        .admin_get("/api/admin/memories/repo/shop/modules/cart/cart-rules")
        .await;
    assert_eq!(detail["reads"]["count"], 4);
    assert_eq!(detail["reads"]["by_agent"]["alice"], 3);
    assert_eq!(detail["reads"]["by_agent"]["bob"], 1);
    assert_eq!(detail["reads"]["last_reader"], "bob");
    assert_eq!(detail["memory"]["version"], 1, "reads leave the file alone");

    let list = a
        .admin_get("/api/admin/memories?scope=repo&owner=shop")
        .await;
    let reads_of = |id: &str| {
        list["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()["reads"]
            .clone()
    };
    assert_eq!(reads_of("repo/shop/modules/cart/cart-rules"), 4);
    assert_eq!(reads_of("repo/shop/modules/cart/cart-pitfalls"), 0);

    // Global ranking: memories and folders (folders include subfolders).
    let global = a.admin_get("/api/admin/reads").await;
    assert_eq!(global["total_reads"], 7);
    assert_eq!(global["memory_count"], 4);
    assert_eq!(global["unread_count"], 1);
    let ids: Vec<&str> = global["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "repo/shop/modules/cart/cart-rules",
            "repo/shop/overview",
            "repo/shop/modules/auth/login-flow"
        ]
    );
    assert_eq!(global["memories"][0]["rank"], 1);
    assert_eq!(global["memories"][0]["readers"], 2);
    let folders: Vec<(String, u64)> = global["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["folder"].as_str().unwrap().to_string(),
                f["reads"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        folders,
        [
            ("".to_string(), 7),
            ("modules".to_string(), 5),
            ("modules/cart".to_string(), 4),
            ("modules/auth".to_string(), 1),
        ]
    );

    // Local ranking inside one folder.
    let local = a
        .admin_get("/api/admin/reads?scope=repo&owner=shop&folder=modules/cart&days=7")
        .await;
    assert_eq!(local["days"], 7);
    assert_eq!(local["total_reads"], 4);
    assert_eq!(local["memory_count"], 2);
    assert_eq!(local["unread_count"], 1);
    assert_eq!(local["memories"].as_array().unwrap().len(), 1);
    assert_eq!(local["folders"].as_array().unwrap().len(), 1);
    assert_eq!(local["folders"][0]["folder"], "modules/cart");
    assert_eq!(local["folders"][0]["read_memories"], 1);

    // Moving keeps the count; deleting drops it.
    c.ok(
        "move_memory",
        json!({ "id": "repo/shop/modules/cart/cart-rules", "folder": "modules/checkout" }),
    )
    .await;
    let (_, out) = read(&mut a, "repo/shop/modules/checkout/cart-rules").await;
    assert_eq!(out["read_count"], 5);
    c.ok("delete_memory", json!({ "id": "repo/shop/overview" })).await;
    let global = a.admin_get("/api/admin/reads").await;
    assert_eq!(global["total_reads"], 6);
    assert_eq!(global["memory_count"], 3);

    // The self-test agent's reads are not counted.
    let mut probe = client(&state, "10.0.0.3");
    probe
        .initialize(&[
            ("x-memhub-agent", "memhub-selftest"),
            ("x-memhub-role", "probe"),
        ])
        .await;
    let (_, out) = read(&mut probe, "repo/shop/modules/auth/login-flow").await;
    assert_eq!(out["read_count"], 1);
    read(&mut b, "repo/shop/modules/cart/cart-pitfalls").await;

    // Deleting a folder from the admin UI drops the counts inside it.
    let res = probe
        .app
        .clone()
        .oneshot(
            Request::delete("/api/admin/folders/repo/shop?folder=modules/cart")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let global = probe.admin_get("/api/admin/reads").await;
    assert_eq!(global["total_reads"], 6);
    assert!(hub
        .reads()
        .all()
        .keys()
        .all(|k| !k.contains("modules/cart/")));

    // Counts survive a restart.
    drop((a, b, c, probe));
    state.close_all("server_shutdown");
    drop(state);
    drop(hub);
    let stats: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("stats").join("reads.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stats["repo/shop/modules/checkout/cart-rules"]["count"], 5);
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let id = "repo/shop/modules/auth/login-flow".parse().unwrap();
    assert_eq!(hub.reads().get(&id).count, 1);
}

fn stdio_lines(hub: &Arc<Hub>, login: StdioLogin, input: &[Value]) -> Vec<Value> {
    let input = input
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    serve(hub.clone(), login, input.as_bytes(), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn stdio_transport() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let responses = stdio_lines(
        &hub,
        StdioLogin::default(),
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize",
                    "params": { "protocolVersion": "2024-11-05", "clientInfo": { "name": "cli" } } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": "connect_agent", "arguments": { "name": "solo", "role": "feature-impl" } } }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": { "name": "add_memory", "arguments": { "scope": "agent", "title": "Hello", "body": "world" } } }),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }),
        ],
    );
    assert_eq!(responses.len(), 5);
    assert_eq!(responses[0]["error"]["code"], -32002);
    assert_eq!(responses[1]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(responses[1]["result"]["serverInfo"]["name"], "memhub2");
    assert_eq!(responses[2]["result"]["isError"], false);
    assert_eq!(
        responses[3]["result"]["structuredContent"]["created"]["id"],
        "agent/solo/working/hello"
    );
    let tools = responses[4]["result"]["tools"].to_string();
    assert!(tools.contains("record_evidence") && !tools.contains("curation_batch"));

    let log = hub.audit().read(&LogQuery::default()).unwrap();
    assert_eq!(log.first().unwrap()["event"], "connect");
    assert_eq!(log.first().unwrap()["ip"], "local");
    assert_eq!(log.last().unwrap()["event"], "disconnect");
    assert_eq!(log.last().unwrap()["reason"], "stdio_eof");
    assert_eq!(log.last().unwrap()["agent"], "solo");

    // A second process for the same agent, identified via --agent.
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                       "params": { "protocolVersion": "2025-06-18", "clientInfo": { "name": "cli" } } });
    let responses = stdio_lines(
        &hub,
        StdioLogin {
            agent: Some("solo".into()),
            role: None,
            instance: Some("  ci-runner-7\n".into()),
            curator: false,
        },
        &[
            init.clone(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "whoami", "arguments": {} } }),
        ],
    );
    assert_eq!(responses[1]["result"]["structuredContent"]["agent"]["name"], "solo");
    let log = hub.audit().read(&LogQuery::default()).unwrap();
    let connect = log.iter().rev().find(|r| r["event"] == "connect").unwrap();
    assert_eq!(connect["instance"], "ci-runner-7");

    // `memhub2 stdio --curator` gives a curator session.
    let responses = stdio_lines(
        &hub,
        StdioLogin {
            agent: Some("memhub-curator".into()),
            role: Some("curator".into()),
            instance: None,
            curator: true,
        },
        &[init, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })],
    );
    assert!(responses[0]["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("CURATOR SESSION"));
    assert!(responses[1]["result"]["tools"].to_string().contains("curation_batch"));
}
