use std::sync::Arc;

use memhub_app::{
    audit::LogQuery,
    store::{MemoryPatch, Scope, SearchQuery},
    AddMemory, AgentSession, AppError, AuthMode, ConnInfo, Hub, HubConfig,
};

fn hub(auth: AuthMode) -> (tempfile::TempDir, Arc<Hub>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig {
        data_root: dir.path().to_path_buf(),
        auth,
    })
    .unwrap();
    (dir, hub)
}

fn connect(hub: &Arc<Hub>, ip: &str) -> AgentSession {
    hub.open_session(ConnInfo {
        transport: "test".into(),
        ip: ip.into(),
        client: None,
    })
}

fn repo_note(repo: &str, title: &str) -> AddMemory {
    AddMemory {
        scope: Scope::Repo,
        repo: Some(repo.into()),
        title: title.into(),
        body: "body".into(),
        ..Default::default()
    }
}

#[test]
fn register_then_reconnect_with_token() {
    let (_dir, hub) = hub(AuthMode::Token);
    let mut s = connect(&hub, "10.0.0.1");
    assert!(matches!(s.list_agents(), Err(AppError::NotIdentified)));

    let reg = s.register("crash-bot", "crash-analysis", "triages crashes").unwrap();
    assert_eq!(s.agent(), Some("crash-bot"));
    assert!(reg.token.starts_with("mh_"));
    s.close("client_closed");

    let mut s2 = connect(&hub, "10.0.0.2");
    assert!(matches!(
        s2.register("crash-bot", "x", ""),
        Err(AppError::AgentExists(_))
    ));
    assert!(matches!(
        s2.login("crash-bot", Some("wrong")),
        Err(AppError::AuthFailed(_))
    ));
    assert!(matches!(s2.login("crash-bot", None), Err(AppError::AuthFailed(_))));
    assert!(matches!(s2.login("ghost", None), Err(AppError::UnknownAgent(_))));
    let info = s2.login("crash-bot", Some(&reg.token)).unwrap();
    assert_eq!(info.role, "crash-analysis");
    assert!(info.last_seen_at.is_some());

    let token = hub.reset_token("crash-bot").unwrap();
    let mut s3 = connect(&hub, "10.0.0.3");
    assert!(s3.login("crash-bot", Some(&reg.token)).is_err());
    s3.login("crash-bot", Some(&token)).unwrap();
}

#[test]
fn open_mode_skips_tokens() {
    let (_dir, hub) = hub(AuthMode::Open);
    connect(&hub, "1.1.1.1").register("a", "feature-impl", "").unwrap();
    connect(&hub, "1.1.1.1").login("a", None).unwrap();
}

#[test]
fn permissions() {
    let (_dir, hub) = hub(AuthMode::Open);
    let mut alice = connect(&hub, "1.1.1.1");
    alice.register("alice", "feature-impl", "").unwrap();
    let mut bob = connect(&hub, "2.2.2.2");
    bob.register("bob", "perf-opt", "").unwrap();

    // Repo memory requires a registered repo that the agent has joined.
    assert!(matches!(
        alice.add_memory(repo_note("engine", "x")),
        Err(AppError::UnknownRepo(_))
    ));
    let join = alice
        .add_repo("engine", Some("game engine"), Some("/src/engine"))
        .unwrap();
    assert!(join.created);
    assert_eq!(join.working_repos, vec!["engine"]);
    let note = alice.add_memory(repo_note("engine", "Overview")).unwrap();
    assert_eq!(note.meta.author, "alice");

    assert!(matches!(
        bob.update_memory(note.id(), MemoryPatch { body: Some("hijack".into()), ..Default::default() }),
        Err(AppError::Forbidden(_))
    ));
    let join = bob.add_repo("engine", None, None).unwrap();
    assert!(!join.created);
    assert_eq!(join.repo.description, "game engine");
    let change = bob
        .update_memory(note.id(), MemoryPatch { append: Some("bob was here".into()), ..Default::default() })
        .unwrap();
    assert_eq!(change.after.meta.updated_by, "bob");

    // Agent memory: readable by all, writable only by the owner.
    let private = alice
        .add_memory(AddMemory {
            scope: Scope::Agent,
            title: "My task log".into(),
            body: "step 1".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(private.meta.id.to_string(), "agent/alice/my-task-log");
    assert_eq!(bob.get_memory(private.id()).unwrap().body, "step 1\n");
    assert!(matches!(
        bob.update_memory(private.id(), MemoryPatch { body: Some("x".into()), ..Default::default() }),
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(bob.delete_memory(private.id(), None), Err(AppError::Forbidden(_))));
    assert!(matches!(
        bob.group(&[private.id().clone()], "g", true),
        Err(AppError::Forbidden(_))
    ));

    // Bob's search sees alice's memories too.
    let hits = bob
        .search(&SearchQuery { text: Some("task".into()), ..Default::default() })
        .unwrap();
    assert_eq!(hits.len(), 1);

    let repos = bob.list_repos().unwrap();
    assert_eq!(repos[0].members, vec!["alice", "bob"]);
    assert_eq!(repos[0].memory_count, 1);
    assert!(repos[0].joined);

    bob.leave_repo("engine").unwrap();
    assert!(matches!(bob.delete_memory(note.id(), None), Err(AppError::Forbidden(_))));
    alice.delete_memory(note.id(), None).unwrap();
}

#[test]
fn audit_trail() {
    let (_dir, hub) = hub(AuthMode::Token);
    {
        let mut s = connect(&hub, "192.168.1.20");
        s.register("perf-bot", "perf-opt", "").unwrap();
        s.add_repo("engine", None, None).unwrap();
        let m = s.add_memory(repo_note("engine", "Hot path")).unwrap();
        s.update_memory(
            m.id(),
            MemoryPatch { body: Some("new body".into()), ..Default::default() },
        )
        .unwrap();
        s.group(&[m.id().clone()], "perf", true).unwrap();
        s.delete_memory(m.id(), None).unwrap();
        s.close("client_closed");
    }
    {
        let mut s = connect(&hub, "192.168.1.99");
        let _ = s.login("perf-bot", Some("bad"));
        // dropped without close
    }

    let events: Vec<String> = hub
        .audit()
        .read(&LogQuery::default())
        .unwrap()
        .iter()
        .map(|v| {
            let e = v["event"].as_str().unwrap().to_string();
            match v.get("action").and_then(|a| a.as_str()) {
                Some(a) => format!("{e}:{a}"),
                None => e,
            }
        })
        .collect();
    assert_eq!(
        events,
        vec![
            "connect",
            "agent_registered",
            "repo_joined",
            "memory_changed:add",
            "memory_changed:update",
            "memory_changed:group_add",
            "memory_changed:delete",
            "disconnect",
            "connect",
            "auth_failed",
            "disconnect",
        ]
    );

    let all = hub.audit().read(&LogQuery::default()).unwrap();
    assert_eq!(all[0]["ip"], "192.168.1.20");
    assert!(all[0].get("agent").is_none(), "unknown at connect time");
    assert_eq!(all[7]["agent"], "perf-bot");
    assert_eq!(all[7]["reason"], "client_closed");
    let update = &all[4];
    assert_eq!(update["version_before"], 1);
    assert_eq!(update["version_after"], 2);
    assert!(update["diff"].as_str().unwrap().contains("+new body"));
    assert_eq!(all[9]["claimed_agent"], "perf-bot");
    assert_eq!(all[10]["reason"], "dropped");
}
