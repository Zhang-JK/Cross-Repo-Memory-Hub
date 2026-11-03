//! End to end: a fake Memory Hub 1.0 server (answering with the shapes the
//! real 1.0 server sends), an in-process 2.0 server and a mock
//! OpenAI-compatible LLM, driven by the `memhub-curator` port and curate code
//! over real HTTP.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use memhub_app::{Hub, HubConfig};
use memhub_curator::{
    connect_hub, port, Config, Curator, LlmClient, McpClient, PortOptions,
    RunOptions,
};
use memhub_mcp::http::{router, HttpConfig, HttpState};
use serde_json::{json, Value};

const TOKEN: &str = "e2e-curator-token";

fn v1_memory(id: &str, kind: &str, title: &str, body: &str, reads: u64) -> Value {
    json!({
        "id": id, "title": title, "kind": kind, "author": "pay-bot",
        "body": format!("{body}\n"), "created_at": "2026-10-01T08:00:00Z",
        "updated_at": "2026-10-02T08:00:00Z", "version": 2, "read_count": reads,
    })
}

/// The 1.0 data set: id -> `get_memory` result.
fn v1_memories() -> Vec<Value> {
    let mut decision = v1_memory(
        "repo/shop/decisions/idempotency-keys",
        "decision",
        "Idempotency keys",
        "所有退款请求都带幂等键 🔑",
        0,
    );
    decision["status"] = json!("done");
    decision["owner_team"] = json!("payments");
    vec![
        v1_memory("repo/shop/shop-overview", "overview", "Shop overview", "Rust service.", 5),
        v1_memory(
            "repo/shop/pitfalls/webhook-twice",
            "pitfall",
            "Webhook delivered twice",
            "Deduplicate by event id.",
            3,
        ),
        decision,
        v1_memory("agent/pay-bot/task-logs/pay-1", "task-log", "PAY-1 log", "Done.", 0),
        v1_memory("agent/pay-bot/todo/follow-ups", "todo", "Follow ups", "- metrics", 0),
        v1_memory("agent/pay-bot/insights/fast-tests", "insight", "Fast tests", "cargo test -p billing", 1),
    ]
}

fn v1_browse(scope: &str, owner: &str) -> Value {
    let prefix = format!("{scope}/{owner}/");
    let memories: Vec<Value> = v1_memories()
        .into_iter()
        .filter(|m| m["id"].as_str().unwrap().starts_with(&prefix))
        .map(|m| {
            let id = m["id"].as_str().unwrap();
            let rest = &id[prefix.len()..];
            let folder = rest.rsplit_once('/').map_or("", |(f, _)| f);
            json!({ "id": id, "folder": folder, "title": m["title"], "kind": m["kind"] })
        })
        .collect();
    let mut folders = vec![json!({ "path": "", "description": null, "memory_count": 1, "total_count": memories.len() })];
    if scope == "repo" {
        folders.push(json!({ "path": "pitfalls", "description": "Things that bit us", "memory_count": 1, "total_count": 1 }));
        folders.push(json!({ "path": "decisions", "description": null, "memory_count": 1, "total_count": 1 }));
    }
    json!({ "scope": scope, "owner": owner, "folder": "", "description": "", "folders": folders, "memories": memories })
}

fn v1_tool(name: &str, args: &Value) -> Value {
    match name {
        "list_agents" => json!({ "agents": [
            { "name": "pay-bot", "role": "feature-impl", "description": "", "repos": ["shop"],
              "created_at": "2026-09-01T00:00:00Z", "last_seen_at": "2026-10-01T00:00:00Z" },
            { "name": "memhub-selftest", "role": "selftest", "description": "", "repos": [],
              "created_at": "2026-09-01T00:00:00Z", "last_seen_at": "2026-10-01T00:00:00Z" }
        ] }),
        "list_repos" => json!({ "repos": [
            { "id": "shop", "description": "Online shop", "created_by": "pay-bot",
              "created_at": "2026-09-01T00:00:00Z", "joined": false, "members": ["pay-bot"], "memory_count": 3 }
        ] }),
        "browse_memory" => v1_browse(args["scope"].as_str().unwrap(), args["owner"].as_str().unwrap()),
        "get_memory" => v1_memories()
            .into_iter()
            .find(|m| m["id"] == args["id"])
            .unwrap(),
        other => panic!("unexpected 1.0 tool {other}"),
    }
}

async fn fake_v1(Json(msg): Json<Value>) -> Response {
    let Some(id) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match msg["method"].as_str().unwrap() {
        "initialize" => json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} },
                                "serverInfo": { "name": "memhub", "version": "0.1.0" } }),
        "tools/call" => {
            let out = v1_tool(msg["params"]["name"].as_str().unwrap(), &msg["params"]["arguments"]);
            json!({ "content": [{ "type": "text", "text": out.to_string() }],
                    "structuredContent": out, "isError": false })
        }
        other => panic!("unexpected 1.0 method {other}"),
    };
    (
        [("mcp-session-id", "v1-session")],
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
        .into_response()
}

#[derive(Default)]
struct LlmLog {
    requests: Vec<Value>,
}

fn section<'a>(text: &'a str, start: &str) -> &'a str {
    text.split_once(start).map_or("", |(_, rest)| rest)
}

fn field<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
}

fn mock_answer(system: &str, user: &str) -> Value {
    if system.starts_with("You review") {
        let verdicts: Vec<Value> = user
            .lines()
            .filter_map(|l| l.strip_prefix('[')?.split_once("] ")?.0.parse::<u64>().ok())
            .map(|i| json!({ "index": i, "approve": true, "reason": "supported" }))
            .collect();
        return json!({ "verdicts": verdicts });
    }
    if system.contains("ported from Memory Hub 1.0") {
        let ops: Vec<Value> = section(user, "Memories ported from 1.0:")
            .split("\n### ")
            .skip(1)
            .map(|block| {
                let id = block.lines().next().unwrap().trim();
                let version: u64 = field(block, "version").unwrap().parse().unwrap();
                json!({ "op": "update", "id": id, "expected_version": version, "tags": ["reviewed"] })
            })
            .collect();
        return json!({ "note": "backfill", "ops": ops });
    }
    let target = field(user, "Target").unwrap();
    let (scope, owner) = target.split_once(':').unwrap();
    let ops: Vec<Value> = section(user, "New evidence (oldest first):")
        .split("\n### ")
        .skip(1)
        .map(|block| {
            let id = block.lines().next().unwrap().trim();
            if scope == "agent" && field(block, "repos").is_some() {
                return json!({ "op": "noop", "evidence": [id], "reason": "repo knowledge" });
            }
            json!({
                "op": "add", "scope": scope, "owner": owner,
                "folder": if scope == "repo" { "pitfalls" } else { "insights" },
                "title": field(block, "title").unwrap(),
                "kind": if scope == "repo" { "pitfall" } else { "insight" },
                "body": field(block, "candidate_lesson").unwrap_or("see evidence"),
                "cites": { "evidence": [id] },
            })
        })
        .collect();
    json!({ "note": "curated", "ops": ops })
}

async fn mock_llm(
    State(log): State<Arc<Mutex<LlmLog>>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let mut entry = req.clone();
    entry["authorization"] = json!(headers.get("authorization").map(|v| v.to_str().unwrap()));
    log.lock().unwrap().requests.push(entry);
    if req.get("response_format").is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "message": "response_format is not supported" } })),
        )
            .into_response();
    }
    let system = req["messages"][0]["content"].as_str().unwrap();
    let user = req["messages"][1]["content"].as_str().unwrap();
    let answer = mock_answer(system, user);
    let content = format!("<think>let me see</think>\n```json\n{answer}\n```");
    Json(json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": content } }] }))
        .into_response()
}

async fn listen(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    addr
}

fn agent(url: &str, name: &str) -> McpClient {
    McpClient::connect(
        url,
        vec![
            ("x-memhub-agent".into(), name.into()),
            ("x-memhub-role".into(), "feature-impl".into()),
        ],
        std::time::Duration::from_secs(30),
    )
    .unwrap()
}

#[test]
fn port_then_curate_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    let llm_log = Arc::new(Mutex::new(LlmLog::default()));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (v1_addr, v2_addr, llm_addr) = rt.block_on(async {
        let state = HttpState::new(
            hub.clone(),
            HttpConfig {
                curator_token: Some(TOKEN.into()),
                ..Default::default()
            },
        );
        (
            listen(Router::new().route("/mcp", post(fake_v1).delete(|| async { StatusCode::NO_CONTENT }))).await,
            listen(router(state)).await,
            listen(
                Router::new()
                    .route("/v1/chat/completions", post(mock_llm))
                    .with_state(llm_log.clone()),
            )
            .await,
        )
    });
    let v2_url = format!("http://{v2_addr}/mcp");
    let mut config = Config::default();
    config.apply_env(|k| match k {
        "MEMHUB2_URL" => Some(v2_url.clone()),
        "MEMHUB2_CURATOR_TOKEN" => Some(TOKEN.into()),
        "MEMHUB1_URL" => Some(format!("http://{v1_addr}/mcp")),
        "MEMHUB2_LLM_BASE_URL" => Some(format!("http://{llm_addr}/v1")),
        "MEMHUB2_LLM_MODEL" => Some("mock-model".into()),
        "MEMHUB2_LLM_API_KEY" => Some("sk-local".into()),
        _ => None,
    });
    config.validate_for_curation().unwrap();

    // ---- a wrong token or a 1.0 server as hub is refused
    let mut wrong = config.clone();
    wrong.hub.curator_token = "nope".into();
    let err = connect_hub(&wrong).err().unwrap();
    assert!(format!("{err:#}").contains("invalid curator token"), "{err:#}");
    let mut swapped = config.clone();
    swapped.hub.url = config.port.v1_url.clone();
    let err = connect_hub(&swapped).err().unwrap();
    assert!(format!("{err:#}").contains("not a Memory Hub 2.0 server"), "{err:#}");

    // ---- port 1.0 -> 2.0
    let mut v2 = connect_hub(&config).unwrap();
    let mut v1 = port::connect_v1(&config).unwrap();
    let dry = port::run(&mut v1, &mut v2, &config, &PortOptions { dry_run: true, only: vec![] }).unwrap();
    assert_eq!(dry.memories, 6);
    assert!(hub.store().list(&Default::default()).unwrap().is_empty(), "dry run writes nothing");
    let report = port::run(&mut v1, &mut v2, &config, &PortOptions::default()).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.agents_created, ["pay-bot"], "the self-test agent is not ported");
    assert_eq!(report.repos_created, ["shop"]);
    assert_eq!(report.by_class["experience"], 4);
    assert_eq!(report.by_class["work_note"], 1);
    assert_eq!(report.by_class["evidence_only"], 1);
    let again = port::run(&mut v1, &mut v2, &config, &PortOptions::default()).unwrap();
    assert_eq!(again.skipped, 6, "the port is idempotent");

    let decision = v2
        .call_tool("get_memory", json!({ "id": "repo/shop/decisions/idempotency-keys" }))
        .unwrap();
    assert_eq!(decision["status"], "provisional");
    assert_eq!(decision["v1_status"], "done");
    assert_eq!(decision["owner_team"], "payments");
    assert!(decision["body"].as_str().unwrap().contains("幂等键 🔑"));
    assert_eq!(
        decision["cites"]["evidence"],
        json!(["evidence/legacy/repo/shop/decisions/idempotency-keys"])
    );
    let todo = v2
        .call_tool("get_memory", json!({ "id": "agent/pay-bot/todo/follow-ups" }))
        .unwrap();
    assert_eq!(todo["id"], "agent/pay-bot/working/todo/follow-ups");
    let log = v2
        .call_tool("get_memory", json!({ "id": "agent/pay-bot/task-logs/pay-1" }))
        .unwrap();
    assert_eq!(log["layer"], "evidence");
    let reads = hub.reads().get(&"repo/shop/shop-overview".parse().unwrap());
    assert_eq!(reads.count, 5, "1.0 read counts are carried over");

    // ---- agents work in 2.0
    let mut pay = agent(&v2_url, "pay-bot");
    pay.call_tool(
        "record_evidence",
        json!({ "title": "Sandbox rejects tiny refunds", "body": "E_MIN_AMOUNT below 0.50",
                "repos": ["shop"], "task": "PAY-2", "outcome": "failure",
                "candidate_lesson": "Use 0.50 or more in sandbox refund tests." }),
    )
    .unwrap();
    pay.call_tool(
        "record_evidence",
        json!({ "title": "Grep the PSP docs first", "body": "Saved an hour.", "task": "PAY-2",
                "candidate_lesson": "Search the PSP docs before debugging." }),
    )
    .unwrap();

    // ---- curate with the mock LLM (which rejects json_mode, so the client falls back)
    let llm = LlmClient::new(&config.llm);
    let dry = Curator::new(&mut v2, &llm, &config, RunOptions { dry_run: true, ..Default::default() })
        .run()
        .unwrap();
    assert!(dry.targets.iter().all(|t| t.applied == 0 && !t.drafts.is_empty()), "{}", dry.summary());
    let report = Curator::new(&mut v2, &llm, &config, RunOptions::default()).run().unwrap();
    let summary = report.summary();
    let by_target = |t: &str| report.targets.iter().find(|r| r.target == t).unwrap_or_else(|| panic!("{summary}"));
    let shop = by_target("repo:shop");
    assert!(shop.errors.is_empty(), "{summary}");
    assert_eq!(shop.evidence, 1);
    assert_eq!(shop.backfill_reviewed, 3);
    assert_eq!(shop.applied, 4, "{summary}");
    let me = by_target("agent:pay-bot");
    assert!(me.errors.is_empty(), "{summary}");
    assert_eq!(me.evidence, 2);
    assert_eq!(me.backfill_reviewed, 1);

    let hits = pay
        .call_tool("search_memory", json!({ "query": "sandbox refunds" }))
        .unwrap();
    assert_eq!(hits["hits"][0]["id"], "repo/shop/pitfalls/sandbox-rejects-tiny-refunds", "{hits}");
    assert_eq!(hits["hits"][0]["author"], "memhub-curator");
    let mine = pay
        .call_tool("browse_memory", json!({ "scope": "agent" }))
        .unwrap();
    let ids: Vec<&str> = mine["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"agent/pay-bot/insights/grep-the-psp-docs-first"), "{ids:?}");
    assert!(!ids.iter().any(|i| i.contains("sandbox")), "repo evidence is not duplicated into agent memory");
    let overview = pay
        .call_tool("get_memory", json!({ "id": "repo/shop/shop-overview" }))
        .unwrap();
    assert!(overview["curated_at"].is_string(), "backfill marks ported memories reviewed");
    assert!(!overview["notices"].to_string().contains("PORTED"), "{overview}");

    let again = Curator::new(&mut v2, &llm, &config, RunOptions::default()).run().unwrap();
    assert!(again.targets.is_empty(), "{}", again.summary());

    let log = llm_log.lock().unwrap();
    assert!(log.requests[0].get("response_format").is_some());
    assert!(log.requests[1..].iter().all(|r| r.get("response_format").is_none()), "fallback sticks");
    assert!(log.requests.iter().all(|r| r["authorization"] == "Bearer sk-local" && r["model"] == "mock-model"));
    drop(log);
    drop((v1, v2, pay));
    rt.shutdown_background();
}
