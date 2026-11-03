use std::sync::Arc;

use memhub_app::{
    audit::LogQuery,
    store::{Filter, MemoryPatch, Scope, SearchQuery},
    AddMemory, AgentSession, AppError, ConnInfo, Hub, HubConfig,
};

fn hub() -> (tempfile::TempDir, Arc<Hub>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    (dir, hub)
}

fn connect(hub: &Arc<Hub>, ip: &str) -> AgentSession {
    hub.open_session(ConnInfo {
        transport: "test".into(),
        ip: ip.into(),
        client: None,
        instance: None,
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

fn work_note(title: &str, body: &str) -> AddMemory {
    AddMemory {
        scope: Scope::Agent,
        folder: "working".into(),
        title: title.into(),
        body: body.into(),
        ..Default::default()
    }
}

#[test]
fn connect_by_name_creates_then_reuses_agent() {
    let (_dir, hub) = hub();
    let mut s = connect(&hub, "10.0.0.1");
    assert!(matches!(s.list_agents(), Err(AppError::NotIdentified)));
    assert!(matches!(
        s.connect("crash-bot", None, None),
        Err(AppError::UnknownAgent(_))
    ));
    assert!(s.connect("../evil", Some("x"), None).is_err());

    let first = s
        .connect("crash-bot", Some("crash-analysis"), Some("triages crashes"))
        .unwrap();
    assert!(first.created);
    assert_eq!(s.agent(), Some("crash-bot"));
    assert!(s.connect("crash-bot", None, None).is_ok(), "reconnecting as self is a no-op");
    assert!(matches!(
        s.connect("other", Some("x"), None),
        Err(AppError::AlreadyIdentified(_))
    ));

    // A different user running the same agent: name alone is enough, and a
    // differing role does not overwrite the existing profile.
    let mut s2 = connect(&hub, "10.0.0.2");
    let second = s2.connect("crash-bot", Some("something-else"), None).unwrap();
    assert!(!second.created);
    assert_eq!(second.agent.role, "crash-analysis");
    assert_eq!(second.agent.description, "triages crashes");
    assert!(second.agent.last_seen_at.is_some());
}

#[test]
fn users_of_same_agent_share_its_work_area() {
    let (_dir, hub) = hub();
    let mut user_a = connect(&hub, "10.0.0.1");
    user_a.connect("perf-bot", Some("perf-opt"), None).unwrap();
    let note = user_a
        .add_memory(work_note("Profiling setup", "use perf record"))
        .unwrap();
    assert_eq!(note.meta.id.to_string(), "agent/perf-bot/working/profiling-setup");

    let mut user_b = connect(&hub, "10.0.0.2");
    user_b.connect("perf-bot", None, None).unwrap();
    let change = user_b
        .update_memory(
            note.id(),
            MemoryPatch { append: Some("and flamegraph".into()), ..Default::default() },
        )
        .unwrap();
    assert_eq!(change.after.meta.updated_by, "perf-bot");

    let log = hub
        .audit()
        .read(&LogQuery { event: Some("memory_changed".into()), ..Default::default() })
        .unwrap();
    let ips: Vec<&str> = log.iter().map(|r| r["ip"].as_str().unwrap()).collect();
    assert_eq!(ips, vec!["10.0.0.1", "10.0.0.2"], "the log still tells users apart");
}

#[test]
fn experience_is_written_by_curators_only() {
    let (_dir, hub) = hub();
    let mut alice = connect(&hub, "1.1.1.1");
    alice.connect("alice", Some("feature-impl"), None).unwrap();
    let mut bob = connect(&hub, "2.2.2.2");
    bob.connect("bob", Some("perf-opt"), None).unwrap();
    let mut curator = connect(&hub, "3.3.3.3");
    curator.connect("memhub-curator", Some("curator"), None).unwrap();
    curator.grant_curator();

    assert!(matches!(
        alice.add_memory(repo_note("engine", "x")),
        Err(AppError::UnknownRepo(_))
    ));
    let join = alice
        .add_repo("engine", Some("game engine"), Some("/src/engine"))
        .unwrap();
    assert!(join.created);
    assert_eq!(join.working_repos, vec!["engine"]);

    // Joining a repo does not make its memory writable: agents send evidence.
    let Err(AppError::Forbidden(msg)) = alice.add_memory(repo_note("engine", "Overview")) else {
        panic!("repo memory must be curator-only");
    };
    assert!(msg.contains("record_evidence(repos=[\"engine\"]"), "{msg}");
    assert!(!msg.contains("  "), "no stray whitespace: {msg}");
    assert!(matches!(
        alice.create_folder(Scope::Repo, Some("engine"), "pitfalls", None),
        Err(AppError::Forbidden(_))
    ));

    let note = curator.add_memory(repo_note("engine", "Overview")).unwrap();
    assert_eq!(note.meta.author, "memhub-curator");
    assert!(matches!(
        bob.update_memory(note.id(), MemoryPatch { body: Some("hijack".into()), ..Default::default() }),
        Err(AppError::Forbidden(_))
    ));
    bob.add_repo("engine", None, None).unwrap();
    assert!(matches!(
        bob.update_memory(note.id(), MemoryPatch { append: Some("x".into()), ..Default::default() }),
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(bob.delete_memory(note.id(), None), Err(AppError::Forbidden(_))));

    // An agent's own experience is curated too; its work area is its own.
    let Err(AppError::Forbidden(msg)) = alice.add_memory(AddMemory {
        scope: Scope::Agent,
        title: "Lesson".into(),
        body: "x".into(),
        ..Default::default()
    }) else {
        panic!("agent experience must be curator-only");
    };
    assert!(msg.contains("folder \"working/...\""), "{msg}");
    let private = alice.add_memory(work_note("My task log", "step 1")).unwrap();
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
    assert!(matches!(
        curator.update_memory(private.id(), MemoryPatch { body: Some("x".into()), ..Default::default() }),
        Err(AppError::Forbidden(_))
    ), "the curator does not edit work areas either");

    // The curator writes agent experience into a named agent space.
    let lesson = curator
        .add_memory(AddMemory {
            scope: Scope::Agent,
            owner: Some("alice".into()),
            title: "Retry the flaky build".into(),
            body: "once".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(lesson.meta.id.to_string(), "agent/alice/retry-the-flaky-build");

    let repos = bob.list_repos().unwrap();
    assert_eq!(repos[0].members, vec!["alice", "bob"]);
    assert_eq!(repos[0].memory_count, 1);
    assert!(repos[0].joined);
    curator.delete_memory(note.id(), None).unwrap();
}

#[test]
fn search_sees_repo_memory_and_own_space_only() {
    let (_dir, hub) = hub();
    let mut alice = connect(&hub, "1.1.1.1");
    alice.connect("alice", Some("feature-impl"), None).unwrap();
    let mut bob = connect(&hub, "2.2.2.2");
    bob.connect("bob", Some("perf-opt"), None).unwrap();
    let mut curator = connect(&hub, "3.3.3.3");
    curator.connect("memhub-curator", Some("curator"), None).unwrap();
    curator.grant_curator();
    alice.add_repo("engine", None, None).unwrap();

    curator
        .add_memory(AddMemory { body: "shader cache pitfall".into(), ..repo_note("engine", "Shader cache") })
        .unwrap();
    curator
        .add_memory(AddMemory {
            scope: Scope::Agent,
            owner: Some("alice".into()),
            title: "Shader lesson".into(),
            body: "shader warmup".into(),
            ..Default::default()
        })
        .unwrap();
    alice.add_memory(work_note("Shader todo", "shader next")).unwrap();

    let ids = |s: &AgentSession, filter: Filter| -> Vec<String> {
        let mut v: Vec<String> = s
            .search(&SearchQuery { text: Some("shader".into()), filter, ..Default::default() })
            .unwrap()
            .into_iter()
            .map(|h| h.meta.id.to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        ids(&alice, Filter::default()),
        vec!["agent/alice/shader-lesson", "agent/alice/working/shader-todo", "repo/engine/shader-cache"]
    );
    assert_eq!(ids(&bob, Filter::default()), vec!["repo/engine/shader-cache"]);
    assert_eq!(
        ids(&bob, Filter { owner: Some("alice".into()), ..Default::default() }),
        vec!["agent/alice/shader-lesson"],
        "named agents' experience, never their work area"
    );

    let searched = hub
        .audit()
        .read(&LogQuery { event: Some("memory_searched".into()), ..Default::default() })
        .unwrap();
    assert_eq!(searched.len(), 3);
    assert_eq!(searched[0]["layer"], "experience");
    assert_eq!(searched[0]["hits"], 3);
}

#[test]
fn audit_trail() {
    let (_dir, hub) = hub();
    {
        let mut s = connect(&hub, "192.168.1.20");
        s.connect("perf-bot", Some("perf-opt"), None).unwrap();
        s.grant_curator();
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
        s.connect("perf-bot", None, None).unwrap();
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
            "agent_login",
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
    assert_eq!(all[9]["agent"], "perf-bot");
    assert_eq!(all[9]["ip"], "192.168.1.99");
    assert_eq!(all[10]["reason"], "dropped");
}
