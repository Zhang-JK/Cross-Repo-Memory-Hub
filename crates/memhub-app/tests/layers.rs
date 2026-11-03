use std::sync::Arc;

use memhub_app::{
    admin::AdminNewMemory,
    curation::{CurationOp, ProposalState, Target, Verdict},
    store::{Cites, EvidenceId, MemoryId, Outcome, Scope, SearchQuery, Source, Status},
    AgentSession, AppError, ConnInfo, EvidenceSearch, Fetched, Hub, HubConfig, Rating,
    RecordEvidence,
};

fn hub() -> (tempfile::TempDir, Arc<Hub>) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(HubConfig::new(dir.path())).unwrap();
    (dir, hub)
}

fn session(hub: &Arc<Hub>, name: &str, curator: bool) -> AgentSession {
    let mut s = hub.open_session(ConnInfo {
        transport: "test".into(),
        ip: "10.0.0.1".into(),
        client: None,
        instance: None,
    });
    s.connect(name, Some("feature-impl"), None).unwrap();
    if curator {
        s.grant_curator();
    }
    s
}

fn observe(s: &AgentSession, title: &str, task: &str) -> String {
    s.record_evidence(RecordEvidence {
        title: title.into(),
        body: format!("{title}: the refund webhook retries twice"),
        repos: vec!["shop".into()],
        task: Some(task.into()),
        outcome: Outcome::Success,
        candidate_lesson: Some("refund webhooks must be idempotent".into()),
        ..Default::default()
    })
    .unwrap()
    .meta
    .id
    .to_string()
}

fn add_op(cites: Vec<String>, status: Status) -> CurationOp {
    add_custom(
        "Refund webhooks retry",
        "pitfall",
        "shop",
        "The refund webhook is delivered up to three times; dedupe on the event id.",
        cites,
        status,
    )
}

fn add_custom(title: &str, kind: &str, owner: &str, body: &str, cites: Vec<String>, status: Status) -> CurationOp {
    CurationOp::Add {
        scope: Scope::Repo,
        owner: owner.into(),
        folder: "pitfalls".into(),
        title: title.into(),
        kind: Some(kind.into()),
        body: body.into(),
        tags: vec!["refunds".into()],
        applies_to: vec!["src/refunds/**".into()],
        cites: Cites { evidence: cites, code: vec!["src/refunds/webhook.rs:42".into()] },
        status: Some(status),
        verified_commit: None,
        slug: None,
    }
}

fn approve_all(curator: &AgentSession, id: &str, n: usize, human: bool) -> memhub_app::curation::Proposal {
    let verdicts: Vec<Verdict> = (0..n)
        .map(|index| Verdict { index, approve: true, reason: "ok".into() })
        .collect();
    curator.review_changes(id, &verdicts, human).unwrap()
}

#[test]
fn evidence_is_append_only_and_outside_experience_search() {
    let (_dir, hub) = hub();
    let mut alice = session(&hub, "alice", false);
    assert!(matches!(
        alice.record_evidence(RecordEvidence {
            title: "x".into(),
            body: "y".into(),
            repos: vec!["shop".into()],
            ..Default::default()
        }),
        Err(AppError::UnknownRepo(_))
    ));
    alice.add_repo("shop", None, None).unwrap();
    let bob = session(&hub, "bob", false);
    let mut bob_in_other = session(&hub, "carol", false);
    bob_in_other.add_repo("other", None, None).unwrap();
    assert!(matches!(
        bob.record_evidence(RecordEvidence {
            title: "x".into(),
            body: "y".into(),
            repos: vec!["shop".into()],
            ..Default::default()
        }),
        Err(AppError::Forbidden(_))
    ), "only repos the agent joined");

    let id = observe(&alice, "Duplicate refunds", "fix refunds");
    assert!(id.starts_with("evidence/"), "{id}");
    let evidence = bob.get_evidence(&id.parse().unwrap()).unwrap();
    assert_eq!(evidence.meta.author, "alice");
    assert_eq!(evidence.meta.source, Source::Observed);
    assert!(matches!(bob.fetch(&id).unwrap(), Fetched::Evidence { .. }));

    let hits = bob.search(&SearchQuery { text: Some("refund".into()), ..Default::default() }).unwrap();
    assert!(hits.is_empty(), "evidence never shows up in experience search");
    let hits = bob
        .search_evidence(&EvidenceSearch { text: Some("refund".into()), ..Default::default() })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].meta.id.to_string(), id);

    assert!(matches!(alice.delete_evidence(&id.parse().unwrap()), Err(AppError::Forbidden(_))));
    assert!(alice
        .record_evidence(RecordEvidence {
            title: "x".into(),
            body: "y".into(),
            source: Some(Source::Legacy),
            ..Default::default()
        })
        .is_err());
    let probe = session(&hub, "memhub-selftest", false);
    let own = probe
        .record_evidence(RecordEvidence { title: "probe".into(), body: "p".into(), ..Default::default() })
        .unwrap();
    probe.delete_evidence(&own.meta.id).unwrap();
}

#[test]
fn ratings_and_disputes_reach_the_curator() {
    let (_dir, hub) = hub();
    let mut alice = session(&hub, "alice", false);
    alice.add_repo("shop", None, None).unwrap();
    let curator = session(&hub, "memhub-curator", true);
    let first = observe(&alice, "Duplicate refunds", "fix refunds");
    let p = curator
        .propose_changes(&Target::Repo("shop".into()), vec![add_op(vec![first], Status::Provisional)], None, None)
        .unwrap();
    let p = approve_all(&curator, &p.id, 1, false);
    let id: MemoryId = p.decisions[0].applied_id.clone().unwrap().parse().unwrap();

    let fb = alice.rate_memory(&id, Rating::Helpful, Some("saved me")).unwrap();
    assert_eq!(fb.helpful, 1);
    let (evidence, memory) = alice
        .dispute_memory(&id, "retries are five now", None, vec!["src/refunds/webhook.rs:50".into()])
        .unwrap();
    assert_eq!(evidence.meta.kind, "dispute");
    assert_eq!(memory.meta.experience.status, Some(Status::Disputed));
    assert!(memory.meta.experience.cites.evidence.contains(&evidence.meta.id.to_string()));

    let batch = curator.curation_batch(&Target::Repo("shop".into()), None).unwrap();
    assert!(batch.feedback.iter().any(|f| f.id == id && f.feedback.disputes == 1));
    assert!(batch.evidence.iter().any(|e| e.evidence.meta.id == evidence.meta.id));
}

#[test]
fn curation_turns_evidence_into_experience() {
    let (_dir, hub) = hub();
    let mut alice = session(&hub, "alice", false);
    alice.add_repo("shop", Some("web shop"), None).unwrap();
    let mut bob = session(&hub, "bob", false);
    bob.add_repo("shop", None, None).unwrap();
    let curator = session(&hub, "memhub-curator", true);
    let shop = Target::Repo("shop".into());

    assert!(matches!(alice.curation_batch(&shop, None), Err(AppError::Forbidden(_))));
    assert!(matches!(alice.propose_changes(&shop, vec![], None, None), Err(AppError::Forbidden(_))));

    let a1 = observe(&alice, "Duplicate refunds", "fix refunds");
    let a2 = observe(&alice, "Duplicate refunds again", "Fix refunds");
    let b1 = observe(&bob, "Refund retried", "refund audit");
    let derived = bob
        .record_evidence(RecordEvidence {
            title: "Read the refund memory".into(),
            body: "it said retries".into(),
            repos: vec!["shop".into()],
            derived_from: vec!["repo/shop/nothing".into()],
            ..Default::default()
        });
    assert!(derived.is_err(), "derived_from must name an existing memory");

    let batch = curator.curation_batch(&shop, Some(2)).unwrap();
    assert_eq!(batch.description, "web shop");
    assert_eq!(batch.evidence.len(), 2);
    assert!(batch.more);

    // One agent on one task is one source, however many records it wrote.
    let p = curator
        .propose_changes(&shop, vec![add_op(vec![a1.clone(), a2.clone()], Status::Established)], batch.cursor.as_deref(), None)
        .unwrap();
    assert_eq!(p.state, ProposalState::Pending);
    assert!(p.checks[0].ok, "{:?}", p.checks[0]);
    assert_eq!(p.checks[0].support, Some(1));
    assert_eq!(p.checks[0].status, Some(Status::Provisional));
    assert!(!p.checks[0].warnings.is_empty());
    let batch = curator.curation_batch(&shop, None).unwrap();
    assert_eq!(batch.evidence.len(), 1, "watermark advanced past the first two");
    assert_eq!(batch.evidence[0].evidence.meta.id.to_string(), b1);

    let p2 = curator
        .propose_changes(&shop, vec![add_op(vec![a1.clone(), b1.clone()], Status::Established)], batch.cursor.as_deref(), Some("second source"))
        .unwrap();
    assert_eq!(p2.checks[0].support, Some(2));
    assert_eq!(p2.checks[0].status, Some(Status::Established));
    assert!(curator.curation_batch(&shop, None).unwrap().evidence.is_empty());

    curator
        .review_changes(&p.id, &[Verdict { index: 0, approve: false, reason: "duplicate of the next proposal".into() }], false)
        .unwrap();
    assert_eq!(curator.get_proposal(&p.id).unwrap().state, ProposalState::Rejected);
    let p2 = approve_all(&curator, &p2.id, 1, false);
    assert_eq!(p2.state, ProposalState::Applied);
    let id: MemoryId = p2.decisions[0].applied_id.clone().unwrap().parse().unwrap();
    assert_eq!(id.to_string(), "repo/shop/pitfalls/refund-webhooks-retry");
    let memory = alice.get_memory(&id).unwrap();
    assert_eq!(memory.meta.experience.status, Some(Status::Established));
    assert_eq!(memory.meta.experience.support, 2);
    assert_eq!(memory.meta.author, "memhub-curator");
    let hits = alice.search(&SearchQuery { text: Some("refund webhook".into()), ..Default::default() }).unwrap();
    assert_eq!(hits[0].meta.id, id);

    // Checks reject task state, uncited adds and missing evidence.
    let bad = curator
        .propose_changes(
            &shop,
            vec![
                add_custom("Refund webhooks retry", "task-log", "shop", "The refund webhook is delivered up to three times; dedupe on the event id.", vec![a1.clone()], Status::Provisional),
                add_op(vec![], Status::Provisional),
                add_op(vec!["evidence/2020-01-01/alice/000000-nope".into()], Status::Provisional),
                add_custom("Refund webhooks retry", "pitfall", "elsewhere", "The refund webhook is delivered up to three times; dedupe on the event id.", vec![a1.clone()], Status::Provisional),
            ],
            None,
            None,
        )
        .unwrap();
    assert!(bad.checks.iter().all(|c| !c.ok), "{:?}", bad.checks);
    let bad = approve_all(&curator, &bad.id, 4, false);
    assert_eq!(bad.state, ProposalState::Rejected, "failing checks are never applied");

    // Supersede by a new entry; superseded memories leave search.
    let v = alice.get_memory(&id).unwrap().meta.version;
    let p3 = curator
        .propose_changes(
            &shop,
            vec![
                add_custom(
                    "Refund webhooks retry five times",
                    "pitfall",
                    "shop",
                    "Since v2 of the provider the webhook retries five times.",
                    vec![b1.clone()],
                    Status::Provisional,
                ),
                CurationOp::Supersede { id: id.to_string(), by: "#0".into(), reason: "provider changed".into() },
                CurationOp::Update {
                    id: id.to_string(),
                    expected_version: v + 10,
                    title: None,
                    kind: None,
                    body: None,
                    tags: Some(vec!["x".into()]),
                    applies_to: None,
                    add_cites: Cites::default(),
                    status: None,
                    verified_commit: None,
                },
            ],
            None,
            None,
        )
        .unwrap();
    assert!(p3.checks[0].ok && p3.checks[1].ok, "{:?}", p3.checks);
    assert!(!p3.checks[2].ok, "stale expected_version");
    let p3 = approve_all(&curator, &p3.id, 3, false);
    assert_eq!(p3.state, ProposalState::Partial);
    let old = alice.get_memory(&id).unwrap();
    assert_eq!(old.meta.experience.status, Some(Status::Superseded));
    assert_eq!(old.meta.experience.superseded_by, p3.decisions[0].applied_id);
    let hits = alice.search(&SearchQuery { text: Some("refund webhook".into()), ..Default::default() }).unwrap();
    assert!(hits.iter().all(|h| h.meta.id != id));
}

#[test]
fn large_rewrites_need_a_human() {
    let (_dir, hub) = hub();
    let mut alice = session(&hub, "alice", false);
    alice.add_repo("shop", None, None).unwrap();
    let curator = session(&hub, "memhub-curator", true);
    let shop = Target::Repo("shop".into());
    let e = observe(&alice, "Duplicate refunds", "fix refunds");
    let long = "Refunds are retried by the provider. ".repeat(20);
    let p = curator
        .propose_changes(&shop, vec![add_custom("Refund webhooks retry", "pitfall", "shop", &long, vec![e.clone()], Status::Provisional)], None, None)
        .unwrap();
    let p = approve_all(&curator, &p.id, 1, false);
    let id = p.decisions[0].applied_id.clone().unwrap();

    let p = curator
        .propose_changes(
            &shop,
            vec![CurationOp::Update {
                id: id.clone(),
                expected_version: 1,
                title: None,
                kind: None,
                body: Some("Refunds retry.".into()),
                tags: None,
                applies_to: None,
                add_cites: Cites::default(),
                status: None,
                verified_commit: None,
            }],
            None,
            None,
        )
        .unwrap();
    assert!(p.checks[0].ok && p.checks[0].needs_human, "{:?}", p.checks[0]);
    let after = approve_all(&curator, &p.id, 1, false);
    assert_eq!(after.state, ProposalState::Pending, "LLM approval is not enough");
    assert!(after.decisions.is_empty());
    let after = approve_all(&curator, &p.id, 1, true);
    assert_eq!(after.state, ProposalState::Applied);
    assert_eq!(alice.get_memory(&id.parse().unwrap()).unwrap().body, "Refunds retry.\n");
}

#[test]
fn v1_memories_are_ported_by_class() {
    let (_old_dir, old) = hub();
    let admin = old.admin("127.0.0.1");
    let mk = |scope, owner: &str, folder: &str, title: &str, kind: &str| {
        admin
            .add_memory(AdminNewMemory {
                scope,
                owner: owner.into(),
                folder: folder.into(),
                title: Some(title.into()),
                kind: Some(kind.into()),
                body: Some(format!("{title} body")),
                ..Default::default()
            })
            .unwrap()
    };
    let pitfall = mk(Scope::Repo, "shop", "pitfalls", "Refund retries", "pitfall");
    let log = mk(Scope::Agent, "alice", "task-logs/2026", "Run 12", "task-log");
    let todo = mk(Scope::Agent, "alice", "todo", "Next steps", "todo");
    let insight = mk(Scope::Agent, "alice", "", "Fast path", "insight");

    let (_dir, hub) = hub();
    let mut alice = session(&hub, "alice", false);
    alice.add_repo("shop", None, None).unwrap();
    let curator = session(&hub, "memhub-curator", true);
    assert!(matches!(alice.import_memory(pitfall.clone(), 0, false), Err(AppError::Forbidden(_))));

    let dry = curator.import_memory(pitfall.clone(), 7, true).unwrap();
    assert!(dry.written.is_empty());
    assert!(!hub.store().exists(&pitfall.meta.id));

    let out = curator.import_memory(pitfall.clone(), 7, false).unwrap();
    assert_eq!(out.written.len(), 2);
    let ported = alice.get_memory(&pitfall.meta.id).unwrap();
    assert_eq!(ported.meta.experience.status, Some(Status::Provisional));
    assert_eq!(ported.meta.experience.origin.as_deref(), Some("v1"));
    assert_eq!(ported.meta.experience.cites.evidence, vec!["evidence/legacy/repo/shop/pitfalls/refund-retries"]);
    assert_eq!(hub.reads().get(&pitfall.meta.id).count, 7 + 1, "1.0 reads carried over, plus this read");
    let again = curator.import_memory(pitfall.clone(), 7, false).unwrap();
    assert_eq!(again.skipped.as_deref(), Some("already imported"));

    let out = curator.import_memory(log.clone(), 0, false).unwrap();
    assert!(out.placement.new_id.is_none());
    match alice.fetch(&log.meta.id.to_string()).unwrap() {
        Fetched::Evidence { evidence, redirected_from } => {
            assert_eq!(evidence.meta.id, EvidenceId::legacy(&log.meta.id));
            assert_eq!(evidence.meta.source, Source::Legacy);
            assert_eq!(redirected_from.as_deref(), Some("agent/alice/task-logs/2026/run-12"));
        }
        other => panic!("expected evidence, got {other:?}"),
    }

    curator.import_memory(todo.clone(), 0, false).unwrap();
    let Fetched::Memory { memory, redirected_from } = alice.fetch(&todo.meta.id.to_string()).unwrap() else {
        panic!("todo must become a work note");
    };
    assert_eq!(memory.meta.id.to_string(), "agent/alice/working/todo/next-steps");
    assert!(redirected_from.is_some());
    assert!(memory.meta.experience.is_empty());

    curator.import_memory(insight.clone(), 0, false).unwrap();
    let backfill = curator.curation_backfill(&Target::Agent("alice".into()), None).unwrap();
    let ids: Vec<String> = backfill.memories.iter().map(|m| m.memory.meta.id.to_string()).collect();
    assert_eq!(ids, vec!["agent/alice/fast-path"]);

    // Reviewing a ported memory marks it curated; legacy evidence is not in batches.
    let p = curator
        .propose_changes(
            &Target::Agent("alice".into()),
            vec![CurationOp::Update {
                id: "agent/alice/fast-path".into(),
                expected_version: insight.meta.version,
                title: None,
                kind: None,
                body: None,
                tags: None,
                applies_to: None,
                add_cites: Cites::default(),
                status: None,
                verified_commit: None,
            }],
            None,
            None,
        )
        .unwrap();
    approve_all(&curator, &p.id, 1, false);
    assert!(curator.curation_backfill(&Target::Agent("alice".into()), None).unwrap().memories.is_empty());
    assert!(curator.curation_batch(&Target::Agent("alice".into()), None).unwrap().evidence.is_empty());
}
