use memhub_store::*;

fn store() -> (tempfile::TempDir, MemoryStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path()).unwrap();
    (dir, store)
}

fn new(scope: Scope, owner: &str, title: &str, body: &str) -> NewMemory {
    NewMemory {
        scope,
        owner: owner.into(),
        title: title.into(),
        body: body.into(),
        author: "alice".into(),
        ..Default::default()
    }
}

#[test]
fn add_get_update_delete() {
    let (dir, store) = store();
    let m = store
        .add(NewMemory {
            kind: Some("pitfall".into()),
            tags: vec!["ffi".into(), "FFI".into(), "unsafe".into()],
            ..new(Scope::Repo, "engine", "FFI boundary rules", "Never panic across FFI.")
        })
        .unwrap();
    assert_eq!(m.meta.id.to_string(), "repo/engine/ffi-boundary-rules");
    assert_eq!(m.meta.tags, vec!["ffi", "unsafe"], "tags are de-duplicated case-insensitively");
    assert!(dir
        .path()
        .join("memories/repo/engine/ffi-boundary-rules.md")
        .is_file());

    let fetched = store.get(m.id()).unwrap();
    assert_eq!(fetched, m);

    let change = store
        .update(
            m.id(),
            MemoryPatch {
                append: Some("Use catch_unwind.".into()),
                expected_version: Some(1),
                ..Default::default()
            },
            "bob",
        )
        .unwrap();
    assert_eq!(change.changed_fields, vec!["body"]);
    assert_eq!(change.after.meta.version, 2);
    assert_eq!(change.after.meta.updated_by, "bob");
    assert_eq!(change.after.body, "Never panic across FFI.\n\nUse catch_unwind.\n");

    let err = store
        .update(
            m.id(),
            MemoryPatch {
                title: Some("x".into()),
                expected_version: Some(1),
                ..Default::default()
            },
            "bob",
        )
        .unwrap_err();
    assert!(matches!(err, StoreError::VersionConflict { current: 2, .. }));

    let deleted = store.delete(m.id(), Some(2)).unwrap();
    assert_eq!(deleted.meta.version, 2);
    assert!(matches!(store.get(m.id()), Err(StoreError::NotFound(_))));
    assert_eq!(
        std::fs::read_dir(dir.path().join("trash/repo/engine")).unwrap().count(),
        1
    );
}

#[test]
fn duplicate_titles_get_suffixed_slugs() {
    let (_dir, store) = store();
    let a = store.add(new(Scope::Agent, "alice", "Notes", "a")).unwrap();
    let b = store.add(new(Scope::Agent, "alice", "Notes", "b")).unwrap();
    assert_eq!(a.meta.id.slug, "notes");
    assert_eq!(b.meta.id.slug, "notes-2");

    let explicit = store.add(NewMemory {
        slug: Some("notes".into()),
        ..new(Scope::Agent, "alice", "Other", "c")
    });
    assert!(matches!(explicit, Err(StoreError::AlreadyExists(_))));
}

#[test]
fn search_ranks_and_filters() {
    let (_dir, store) = store();
    store
        .add(NewMemory {
            tags: vec!["perf".into()],
            ..new(Scope::Repo, "engine", "Render loop hot path", "The render loop allocates per frame.")
        })
        .unwrap();
    store
        .add(new(Scope::Repo, "engine", "Build notes", "Run cargo build; render docs separately."))
        .unwrap();
    store
        .add(new(Scope::Agent, "alice", "Render crash", "Crash in render thread on resize."))
        .unwrap();

    let hits = store
        .search(&SearchQuery {
            text: Some("render loop".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].meta.title, "Render loop hot path");
    assert_eq!(hits.len(), 3);
    assert!(hits[0].snippet.contains("render loop"));

    let hits = store
        .search(&SearchQuery {
            text: Some("render".into()),
            filter: Filter {
                scope: Some(Scope::Agent),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].meta.id.to_string(), "agent/alice/render-crash");

    let hits = store
        .search(&SearchQuery {
            filter: Filter {
                tags: vec!["PERF".into()],
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);

    let none = store
        .search(&SearchQuery {
            text: Some("nonexistent-term".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn grouping() {
    let (_dir, store) = store();
    let a = store.add(new(Scope::Repo, "engine", "A", "a")).unwrap();
    let b = store.add(new(Scope::Repo, "engine", "B", "b")).unwrap();
    let ids = vec![a.meta.id.clone(), b.meta.id.clone()];

    let changes = store.set_group(&ids, "release-2.0", true, "alice").unwrap();
    assert_eq!(changes.len(), 2);
    let again = store.set_group(&ids, "release-2.0", true, "alice").unwrap();
    assert!(again.is_empty(), "re-adding is a no-op");

    let groups = store.group_by(&Filter::default(), GroupKey::Group).unwrap();
    assert_eq!(groups["release-2.0"].len(), 2);

    let in_group = store
        .list(&Filter {
            group: Some("release-2.0".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(in_group.len(), 2);

    store.set_group(&ids[..1], "release-2.0", false, "alice").unwrap();
    let groups = store.group_by(&Filter::default(), GroupKey::Group).unwrap();
    assert_eq!(groups["release-2.0"].len(), 1);

    let by_kind = store.group_by(&Filter::default(), GroupKey::Kind).unwrap();
    assert_eq!(by_kind["note"].len(), 2);
}

#[test]
fn rejects_path_traversal() {
    let (_dir, store) = store();
    assert!(store.add(new(Scope::Repo, "../etc", "x", "")).is_err());
    assert!("repo/../passwd".parse::<MemoryId>().is_err());
    assert!("repo/a/b/c".parse::<MemoryId>().is_err());
    assert!("repo/engine/note".parse::<MemoryId>().is_ok());
}

#[test]
fn create_and_delete_owner_folders() {
    let (dir, store) = store();
    assert!(store.create_owner(Scope::Repo, "engine").unwrap());
    assert!(!store.create_owner(Scope::Repo, "engine").unwrap(), "second create is a no-op");
    assert_eq!(store.owners(Scope::Repo).unwrap(), vec!["engine"]);
    assert!(store.create_owner(Scope::Agent, "../x").is_err());

    store.add(new(Scope::Repo, "engine", "One", "1")).unwrap();
    store.add(new(Scope::Repo, "engine", "Two", "2")).unwrap();
    store.add(new(Scope::Repo, "other", "Kept", "3")).unwrap();

    let removed = store.delete_owner(Scope::Repo, "engine").unwrap();
    let mut ids: Vec<String> = removed.iter().map(|m| m.meta.id.to_string()).collect();
    ids.sort();
    assert_eq!(ids, vec!["repo/engine/one", "repo/engine/two"]);
    assert_eq!(store.owners(Scope::Repo).unwrap(), vec!["other"]);
    assert_eq!(store.list(&Filter::default()).unwrap().len(), 1);

    let trashed: Vec<_> = std::fs::read_dir(dir.path().join("trash/repo/engine"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(trashed.len(), 1);
    assert!(trashed[0].join("one.md").is_file(), "folder is kept in the trash");

    assert!(matches!(
        store.delete_owner(Scope::Repo, "engine"),
        Err(StoreError::NotFound(_))
    ));
    store.add(new(Scope::Repo, "engine", "Again", "")).unwrap();
}

#[test]
fn hand_written_files_are_picked_up() {
    let (dir, store) = store();
    let path = dir.path().join("memories/repo/engine");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("manual.md"), "# Manual note\n\nwritten by a human\n").unwrap();
    std::fs::write(path.join("broken.md"), "---\ntags: [\n---\n").unwrap();

    let all = store.list(&Filter::default()).unwrap();
    assert_eq!(all.len(), 1, "malformed files are skipped");
    assert_eq!(all[0].meta.title, "Manual note");
}
