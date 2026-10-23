use std::{io::Write, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use memhub_app::{
    admin::{AdminNewMemory, LogFilter, WindowCounts},
    audit::LogQuery,
    store::{MemoryId, Scope},
    AddMemory, AgentSession, ConnInfo, Hub, HubConfig, ADMIN_ACTOR,
};

fn hub() -> (tempfile::TempDir, Arc<Hub>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    (dir, hub)
}

fn agent(hub: &Arc<Hub>, name: &str, ip: &str) -> AgentSession {
    let mut s = hub.open_session(ConnInfo {
        transport: "http".into(),
        ip: ip.into(),
        client: Some("test-client 1.0".into()),
    });
    s.connect(name, Some("feature-impl"), None).unwrap();
    s
}

fn logs(limit: usize) -> LogFilter {
    LogFilter {
        limit,
        ..Default::default()
    }
}

#[test]
fn admin_adds_and_deletes_memories_anywhere_with_audit() {
    let (_dir, hub) = hub();
    let admin = hub.admin("192.168.1.9");

    let m = admin
        .add_memory(AdminNewMemory {
            scope: Scope::Agent,
            owner: "crash-bot".into(),
            title: Some("Seeded note".into()),
            kind: Some("insight".into()),
            tags: vec!["seed".into()],
            body: Some("hello".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(m.meta.id.to_string(), "agent/crash-bot/seeded-note");
    assert_eq!(m.meta.author, ADMIN_ACTOR);

    let detail = admin.get(&m.meta.id).unwrap();
    assert!(detail.markdown.starts_with("---\n"));
    assert_eq!(detail.path, "memories/agent/crash-bot/seeded-note.md");

    admin.delete_memory(&m.meta.id).unwrap();
    assert!(admin.get(&m.meta.id).is_err());

    let records = hub.audit().read(&LogQuery::default()).unwrap();
    let actions: Vec<_> = records
        .iter()
        .map(|r| r["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, vec!["add", "delete"]);
    for r in &records {
        assert_eq!(r["agent"], ADMIN_ACTOR);
        assert_eq!(r["ip"], "192.168.1.9");
        assert_eq!(r["transport"], "admin");
    }
}

#[test]
fn markdown_import_fills_properties_from_front_matter() {
    let (_dir, hub) = hub();
    let admin = hub.admin("127.0.0.1");
    let markdown = "---\ntitle: Imported pitfall\nkind: pitfall\ntags: [ffi]\ngroups: [g1]\n---\n\nDo not panic across FFI.\n";

    let m = admin
        .add_memory(AdminNewMemory {
            scope: Scope::Repo,
            owner: "engine".into(),
            markdown: Some(markdown.into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(m.meta.id.to_string(), "repo/engine/imported-pitfall");
    assert_eq!(m.meta.kind, "pitfall");
    assert_eq!(m.meta.tags, vec!["ffi"]);
    assert_eq!(m.meta.groups, vec!["g1"]);
    assert_eq!(m.body, "Do not panic across FFI.\n");

    let explicit = admin
        .add_memory(AdminNewMemory {
            scope: Scope::Repo,
            owner: "engine".into(),
            title: Some("Override".into()),
            markdown: Some("# Heading title\n\nplain body".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(explicit.meta.title, "Override");
    assert_eq!(explicit.meta.kind, "note");
    assert_eq!(explicit.body, "# Heading title\n\nplain body\n");

    assert!(admin
        .add_memory(AdminNewMemory {
            scope: Scope::Repo,
            owner: "engine".into(),
            ..Default::default()
        })
        .is_err());
}

#[test]
fn folders_create_list_and_delete() {
    let (_dir, hub) = hub();
    let s = agent(&hub, "perf-bot", "10.0.0.2");
    let mut s = s;
    s.add_repo("engine", Some("game engine"), None).unwrap();
    s.add_memory(AddMemory {
        scope: Scope::Repo,
        repo: Some("engine".into()),
        title: "Hot loop".into(),
        body: "x".into(),
        ..Default::default()
    })
    .unwrap();

    let admin = hub.admin("127.0.0.1");
    assert!(admin
        .create_folder(Scope::Repo, "empty-repo", "", None)
        .unwrap());
    assert!(!admin
        .create_folder(Scope::Repo, "empty-repo", "", None)
        .unwrap());
    assert!(admin
        .create_folder(Scope::Repo, "bad/name", "", None)
        .is_err());

    let folders = admin.folders().unwrap();
    let engine = folders.iter().find(|f| f.owner == "engine").unwrap();
    assert_eq!(engine.memory_count, 1);
    assert!(engine.registered);
    assert_eq!(engine.description, "game engine");
    let empty = folders.iter().find(|f| f.owner == "empty-repo").unwrap();
    assert_eq!(empty.memory_count, 0);
    assert!(!empty.registered);

    let removed = admin.delete_folder(Scope::Repo, "engine", "").unwrap();
    assert_eq!(
        removed,
        vec!["repo/engine/hot-loop".parse::<MemoryId>().unwrap()]
    );
    assert!(admin.folders().unwrap().iter().all(|f| f.owner != "engine"));
    assert!(admin.delete_folder(Scope::Repo, "engine", "").is_err());

    let updates = admin.updates(&logs(10)).unwrap();
    let events: Vec<String> = updates
        .iter()
        .map(|r| {
            format!(
                "{}:{}",
                r["event"].as_str().unwrap(),
                r["action"].as_str().unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        events,
        vec![
            "folder_deleted:-",
            "memory_changed:delete",
            "folder_created:-",
            "memory_changed:add",
        ],
        "newest first"
    );
    assert_eq!(updates[0]["memory_count"], 1);

    let by_agent = admin
        .updates(&LogFilter {
            agent: Some("perf-bot".into()),
            ..logs(10)
        })
        .unwrap();
    assert_eq!(by_agent.len(), 1);
    let folder_events = admin
        .updates(&LogFilter {
            action: Some("folder_created".into()),
            ..logs(10)
        })
        .unwrap();
    assert_eq!(folder_events.len(), 1);
    let by_memory = admin
        .updates(&LogFilter {
            memory: Some("HOT".into()),
            ..logs(10)
        })
        .unwrap();
    assert_eq!(by_memory.len(), 2, "matches the title case-insensitively");
}

#[test]
fn admin_manages_subfolders_and_moves() {
    let (_dir, hub) = hub();
    let admin = hub.admin("127.0.0.1");
    let m = admin
        .add_memory(AdminNewMemory {
            scope: Scope::Repo,
            owner: "engine".into(),
            folder: "modules/render".into(),
            title: Some("Frame pacing".into()),
            body: Some("vsync".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        m.meta.id.to_string(),
        "repo/engine/modules/render/frame-pacing"
    );
    assert_eq!(
        admin.get(&m.meta.id).unwrap().path,
        "memories/repo/engine/modules/render/frame-pacing.md"
    );
    assert!(admin
        .create_folder(
            Scope::Repo,
            "engine",
            "pitfalls",
            Some("Things that bit us")
        )
        .unwrap());
    assert!(!admin
        .create_folder(Scope::Repo, "engine", "pitfalls", Some("Known traps"))
        .unwrap());

    let folders = admin.folders().unwrap();
    let engine = folders.iter().find(|f| f.owner == "engine").unwrap();
    assert_eq!((engine.memory_count, engine.root_count), (1, 0));
    let paths: Vec<&str> = engine.children.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["modules", "modules/render", "pitfalls"]);
    assert_eq!(
        engine.children[2].description.as_deref(),
        Some("Known traps")
    );

    let in_modules = admin
        .list(Some(Scope::Repo), Some("engine"), Some("modules"), None)
        .unwrap();
    assert_eq!(in_modules.len(), 1);
    assert!(admin
        .list(Some(Scope::Repo), Some("engine"), Some("pitfalls"), None)
        .unwrap()
        .is_empty());

    let moved = admin
        .move_memory(&m.meta.id, "pitfalls", Some("pacing"))
        .unwrap();
    assert_eq!(moved.meta.id.to_string(), "repo/engine/pitfalls/pacing");

    let removed = admin
        .delete_folder(Scope::Repo, "engine", "pitfalls")
        .unwrap();
    assert_eq!(removed, vec![moved.meta.id.clone()]);
    let engine_left = admin.folders().unwrap();
    let engine_left = engine_left.iter().find(|f| f.owner == "engine").unwrap();
    assert_eq!(engine_left.children.len(), 2);

    let updates = admin.updates(&logs(20)).unwrap();
    let events: Vec<String> = updates
        .iter()
        .rev()
        .map(|r| {
            format!(
                "{}:{}:{}",
                r["event"].as_str().unwrap(),
                r["action"].as_str().unwrap_or("-"),
                r["folder"].as_str().unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        events,
        vec![
            "folder_created:-:modules",
            "folder_created:-:modules/render",
            "memory_changed:add:-",
            "folder_created:-:pitfalls",
            "folder_described:-:pitfalls",
            "memory_changed:move:-",
            "memory_changed:delete:-",
            "folder_deleted:-:pitfalls",
        ]
    );
    let moved_record = updates.iter().find(|r| r["action"] == "move").unwrap();
    assert_eq!(
        moved_record["previous_id"],
        "repo/engine/modules/render/frame-pacing"
    );
    let by_old_id = admin
        .updates(&LogFilter {
            memory: Some("modules/render/frame".into()),
            ..logs(20)
        })
        .unwrap();
    assert_eq!(by_old_id.len(), 2, "add and move match the old id");
}

#[test]
fn connections_are_reconstructed_per_session() {
    let (_dir, hub) = hub();
    let mut a = agent(&hub, "crash-bot", "10.0.0.1");
    a.add_repo("engine", None, None).unwrap();
    a.add_memory(AddMemory {
        scope: Scope::Agent,
        title: "note".into(),
        body: "b".into(),
        ..Default::default()
    })
    .unwrap();
    a.close("client_closed");
    let _b = agent(&hub, "crash-bot", "10.0.0.7");
    let anonymous = hub.open_session(ConnInfo {
        transport: "http".into(),
        ip: "10.0.0.9".into(),
        client: None,
    });
    anonymous.close("idle_timeout");

    let admin = hub.admin("127.0.0.1");
    let sessions = admin.connections(&logs(10)).unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].ip, "10.0.0.9", "newest first");
    assert_eq!(sessions[0].agent, None);
    assert_eq!(sessions[0].reason.as_deref(), Some("idle_timeout"));

    let open = &sessions[1];
    assert_eq!(open.agent.as_deref(), Some("crash-bot"));
    assert!(!open.registered);
    assert!(open.disconnected_at.is_none());

    let first = &sessions[2];
    assert_eq!(first.agent.as_deref(), Some("crash-bot"));
    assert!(first.registered);
    assert_eq!(first.client.as_deref(), Some("test-client 1.0"));
    assert_eq!(first.reason.as_deref(), Some("client_closed"));
    assert_eq!(first.changes, 1);
    assert!(first.connected_at.is_some() && first.identified_at.is_some());

    let filtered = admin
        .connections(&LogFilter {
            ip: Some("10.0.0.7".into()),
            ..logs(10)
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);

    let events = admin
        .connection_events(&LogFilter {
            agent: Some("crash-bot".into()),
            ..logs(50)
        })
        .unwrap();
    let connect = events
        .iter()
        .find(|e| e["event"] == "connect" && e["ip"] == "10.0.0.1")
        .unwrap();
    assert_eq!(connect["agent"], "crash-bot", "filled in from the session");
    assert!(events.iter().all(|e| e["event"] != "memory_changed"));
}

#[test]
fn stats_summarize_memories_and_activity() {
    let (_dir, hub) = hub();
    let mut a = agent(&hub, "crash-bot", "10.0.0.1");
    a.add_repo("engine", None, None).unwrap();
    for title in ["one", "two"] {
        a.add_memory(AddMemory {
            scope: Scope::Repo,
            repo: Some("engine".into()),
            title: title.into(),
            kind: Some("pitfall".into()),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    }
    a.add_memory(AddMemory {
        scope: Scope::Agent,
        title: "mine".into(),
        body: "b".into(),
        ..Default::default()
    })
    .unwrap();

    let stats = hub.admin("127.0.0.1").stats().unwrap();
    assert_eq!(stats.memories, 3);
    assert_eq!(stats.repo_memories, 2);
    assert_eq!(stats.agent_memories, 1);
    assert_eq!((stats.repo_folders, stats.agent_folders), (1, 1));
    assert_eq!((stats.registered_agents, stats.registered_repos), (1, 1));
    assert_eq!(stats.kinds[0], ("pitfall".to_string(), 2));
    assert_eq!(stats.top_folders[0], ("repo/engine".to_string(), 2));
    assert_eq!(stats.top_agents, vec![("crash-bot".to_string(), 3)]);
    assert_eq!(stats.activity.len(), 14);
    assert_eq!(stats.connections_today, 1);
    assert_eq!(stats.changes_today, 3);
    assert_eq!(stats.active_agents_today, 1);
    assert_eq!(stats.recent_updates.len(), 3);
    assert!(stats.recent_updates[0].get("diff").is_none());
}

fn append_audit(hub: &Hub, ts: DateTime<Utc>, event: &str, rest: &str) {
    let path = hub.audit().file_for(ts.date_naive());
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

fn connect_pair(hub: &Hub, ts: DateTime<Utc>, id: &str, ip: &str, agent: Option<&str>) {
    let agent_field = agent
        .map(|a| format!(r#","agent":"{a}""#))
        .unwrap_or_default();
    append_audit(
        hub,
        ts,
        "connect",
        &format!(r#""session_id":"{id}","ip":"{ip}","transport":"http"{agent_field}"#),
    );
    append_audit(
        hub,
        ts + Duration::seconds(2),
        "disconnect",
        &format!(
            r#""session_id":"{id}","ip":"{ip}","transport":"http"{agent_field},"reason":"client_closed","duration_secs":2.0"#
        ),
    );
}

#[test]
fn connection_summary_uses_rolling_windows() {
    let (_dir, hub) = hub();
    let now = Utc::now();
    connect_pair(
        &hub,
        now - Duration::days(20),
        "month-only",
        "10.9.1.1",
        Some("month-bot"),
    );
    connect_pair(
        &hub,
        now - Duration::days(3),
        "week-one",
        "10.9.2.2",
        Some("week-bot"),
    );
    connect_pair(&hub, now - Duration::days(3), "week-anon", "10.9.2.2", None);
    connect_pair(
        &hub,
        now - Duration::days(3),
        "week-admin",
        "10.9.3.3",
        Some(ADMIN_ACTOR),
    );
    connect_pair(
        &hub,
        now - Duration::days(400),
        "ancient",
        "10.8.8.8",
        Some("ancient-bot"),
    );
    // Connected outside the month window, disconnected inside the week.
    append_audit(
        &hub,
        now - Duration::days(40),
        "connect",
        r#""session_id":"spanning","ip":"10.9.4.4","transport":"http","agent":"span-bot""#,
    );
    append_audit(
        &hub,
        now - Duration::days(2),
        "disconnect",
        r#""session_id":"spanning","ip":"10.9.4.4","transport":"http","agent":"span-bot","reason":"client_closed","duration_secs":1.0"#,
    );

    let summary = hub.admin("127.0.0.1").connection_summary().unwrap();
    assert_eq!(
        summary.week,
        WindowCounts {
            connections: 4,
            ips: 3,
            agents: 2
        },
        "week counts the shared IP once and skips anonymous and (admin)"
    );
    assert_eq!(
        summary.month,
        WindowCounts {
            connections: 5,
            ips: 4,
            agents: 3
        }
    );

    let admin = hub.admin("127.0.0.1");
    let year = admin
        .connections(&LogFilter {
            days: Some(365),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert!(year.iter().all(|s| s.session_id != "ancient"));
    assert!(year.iter().any(|s| s.session_id == "month-only"));
    let all = admin.connections(&logs(100)).unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(all[0].session_id, "week-admin", "newest first");
}

#[test]
fn selftest_traffic_is_hidden_from_the_management_logs() {
    let (_dir, hub) = hub();
    let probe = agent(&hub, "memhub-selftest", "10.9.9.9");
    probe
        .add_memory(AddMemory {
            scope: Scope::Agent,
            title: "probe".into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    probe.close("client_closed");
    let real = agent(&hub, "feature-builder", "10.1.1.1");
    real.add_memory(AddMemory {
        scope: Scope::Agent,
        title: "real".into(),
        body: "b".into(),
        ..Default::default()
    })
    .unwrap();
    real.close("client_closed");

    let admin = hub.admin("127.0.0.1");
    let sessions = admin.connections(&logs(50)).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent.as_deref(), Some("feature-builder"));

    let events = admin.connection_events(&logs(50)).unwrap();
    assert!(events.iter().all(|e| e["agent"] != "memhub-selftest"));
    assert!(events.iter().any(|e| e["agent"] == "feature-builder"));

    let updates = admin.updates(&logs(50)).unwrap();
    assert!(updates.iter().all(|u| u["agent"] != "memhub-selftest"));
    assert_eq!(updates.len(), 1);

    let summary = admin.connection_summary().unwrap();
    assert_eq!(
        summary.week,
        WindowCounts {
            connections: 1,
            ips: 1,
            agents: 1
        }
    );

    let stats = admin.stats().unwrap();
    assert_eq!(stats.connections_today, 1);
    assert_eq!(stats.changes_today, 1);
    assert_eq!(stats.active_agents_today, 1);
    assert_eq!(stats.top_agents, vec![("feature-builder".to_string(), 1)]);
    assert!(stats
        .recent_updates
        .iter()
        .all(|u| u["agent"] != "memhub-selftest"));
}
