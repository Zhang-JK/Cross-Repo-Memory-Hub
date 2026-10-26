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
            ..new(
                Scope::Repo,
                "engine",
                "FFI boundary rules",
                "Never panic across FFI.",
            )
        })
        .unwrap();
    assert_eq!(m.meta.id.to_string(), "repo/engine/ffi-boundary-rules");
    assert_eq!(
        m.meta.tags,
        vec!["ffi", "unsafe"],
        "tags are de-duplicated case-insensitively"
    );
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
    assert_eq!(
        change.after.body,
        "Never panic across FFI.\n\nUse catch_unwind.\n"
    );

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
    assert!(matches!(
        err,
        StoreError::VersionConflict { current: 2, .. }
    ));

    let deleted = store.delete(m.id(), Some(2)).unwrap();
    assert_eq!(deleted.meta.version, 2);
    assert!(matches!(store.get(m.id()), Err(StoreError::NotFound(_))));
    assert_eq!(
        std::fs::read_dir(dir.path().join("trash/repo/engine"))
            .unwrap()
            .count(),
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
            ..new(
                Scope::Repo,
                "engine",
                "Render loop hot path",
                "The render loop allocates per frame.",
            )
        })
        .unwrap();
    store
        .add(new(
            Scope::Repo,
            "engine",
            "Build notes",
            "Run cargo build; render docs separately.",
        ))
        .unwrap();
    store
        .add(new(
            Scope::Agent,
            "alice",
            "Render crash",
            "Crash in render thread on resize.",
        ))
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

    store
        .set_group(&ids[..1], "release-2.0", false, "alice")
        .unwrap();
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
    assert!("repo/a/../c".parse::<MemoryId>().is_err());
    assert!("repo/a//c".parse::<MemoryId>().is_err());
    assert!("repo/a/.hidden/c".parse::<MemoryId>().is_err());
    assert!("repo/a/_index".parse::<MemoryId>().is_err());
    assert!("repo/engine/note".parse::<MemoryId>().is_ok());
    assert!(store
        .add(NewMemory {
            folder: "ok/../../escape".into(),
            ..new(Scope::Repo, "engine", "x", "")
        })
        .is_err());
    assert!(
        store
            .create_folder(Scope::Repo, "engine", "a/b/c/d/e/f/g", None)
            .is_err(),
        "depth is limited"
    );
}

#[test]
fn nested_ids_parse_and_display() {
    let id: MemoryId = "repo/engine/modules/auth/token-refresh".parse().unwrap();
    assert_eq!(id.owner, "engine");
    assert_eq!(id.folder, "modules/auth");
    assert_eq!(id.slug, "token-refresh");
    assert_eq!(id.to_string(), "repo/engine/modules/auth/token-refresh");
    assert!(id.is_under("modules"));
    assert!(id.is_under("modules/auth"));
    assert!(!id.is_under("modules/au"));
    assert!(!id.is_under("mod"));
    let root: MemoryId = "agent/alice/note".parse().unwrap();
    assert_eq!(root.folder, "");
    assert_eq!(
        normalize_folder("/modules//auth/ ").unwrap(),
        "modules/auth"
    );
}

#[test]
fn memories_in_nested_folders() {
    let (dir, store) = store();
    let overview = store
        .add(new(Scope::Repo, "engine", "Overview", "start here"))
        .unwrap();
    let auth = store
        .add(NewMemory {
            folder: "modules/auth".into(),
            ..new(
                Scope::Repo,
                "engine",
                "Token refresh",
                "tokens expire hourly",
            )
        })
        .unwrap();
    let render = store
        .add(NewMemory {
            folder: "/modules/render/".into(),
            ..new(Scope::Repo, "engine", "Frame pacing", "vsync")
        })
        .unwrap();
    assert_eq!(
        auth.meta.id.to_string(),
        "repo/engine/modules/auth/token-refresh"
    );
    assert_eq!(render.meta.id.folder, "modules/render");
    assert!(dir
        .path()
        .join("memories/repo/engine/modules/auth/token-refresh.md")
        .is_file());
    assert_eq!(store.get(&auth.meta.id).unwrap(), auth);

    assert_eq!(store.list(&Filter::default()).unwrap().len(), 3);
    let modules = store
        .list(&Filter {
            folder: Some("modules".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(modules.len(), 2, "folder filter includes subfolders");
    let only_auth = store
        .list(&Filter {
            folder: Some("modules/auth".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(only_auth.len(), 1);

    let hits = store
        .search(&SearchQuery {
            text: Some("auth".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].meta.id, auth.meta.id, "folder names are searchable");

    let by_folder = store
        .group_by(&Filter::default(), GroupKey::Folder)
        .unwrap();
    assert_eq!(by_folder["repo/engine"][0].id, overview.meta.id);
    assert_eq!(by_folder["repo/engine/modules/auth"].len(), 1);

    store.delete(&auth.meta.id, None).unwrap();
    assert_eq!(
        std::fs::read_dir(dir.path().join("trash/repo/engine/modules/auth"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn folder_tree_with_descriptions() {
    let (dir, store) = store();
    store
        .add(new(Scope::Repo, "engine", "Overview", ""))
        .unwrap();
    store
        .add(NewMemory {
            folder: "modules/auth".into(),
            ..new(Scope::Repo, "engine", "Tokens", "")
        })
        .unwrap();
    assert!(store
        .create_folder(
            Scope::Repo,
            "engine",
            "pitfalls",
            Some("Things that bit us")
        )
        .unwrap());
    assert!(
        !store
            .create_folder(
                Scope::Repo,
                "engine",
                "modules",
                Some("One folder per crate")
            )
            .unwrap(),
        "existing folder only gets a description"
    );
    store
        .create_folder(Scope::Repo, "engine", "modules-old", None)
        .unwrap();
    assert!(dir
        .path()
        .join("memories/repo/engine/pitfalls/_index.md")
        .is_file());
    assert!(store
        .create_folder(Scope::Repo, "engine", "", None)
        .is_err());

    let tree = store.folders(Scope::Repo, "engine").unwrap();
    let paths: Vec<&str> = tree.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["", "modules", "modules/auth", "modules-old", "pitfalls"]
    );
    assert_eq!((tree[0].memory_count, tree[0].total_count), (1, 2));
    assert_eq!((tree[1].memory_count, tree[1].total_count), (0, 1));
    assert_eq!(tree[1].description.as_deref(), Some("One folder per crate"));
    assert_eq!(tree[2].depth(), 2);
    assert_eq!(tree[2].name(), "auth");
    assert_eq!(tree[4].total_count, 0, "empty folders are listed");
    assert!(tree[4].updated_at.is_none());
    assert_eq!(
        store.list(&Filter::default()).unwrap().len(),
        2,
        "_index.md is not a memory"
    );

    store
        .create_folder(Scope::Repo, "engine", "pitfalls", Some(""))
        .unwrap();
    assert_eq!(
        store
            .folder_description(Scope::Repo, "engine", "pitfalls")
            .unwrap(),
        None
    );

    let removed = store
        .delete_folder(Scope::Repo, "engine", "modules")
        .unwrap();
    assert_eq!(removed.len(), 1);
    assert!(!store
        .folder_exists(Scope::Repo, "engine", "modules")
        .unwrap());
    assert!(matches!(
        store.delete_folder(Scope::Repo, "engine", "modules"),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(store.list(&Filter::default()).unwrap().len(), 1);
}

#[test]
fn move_memory_between_folders() {
    let (dir, store) = store();
    let a = store.add(new(Scope::Repo, "engine", "Notes", "a")).unwrap();
    let b = store
        .add(NewMemory {
            folder: "pitfalls".into(),
            ..new(Scope::Repo, "engine", "Notes", "b")
        })
        .unwrap();

    let change = store
        .move_memory(&a.meta.id, "pitfalls", None, Some(1), "bob")
        .unwrap();
    assert_eq!(
        change.after.meta.id.to_string(),
        "repo/engine/pitfalls/notes-2",
        "slug is suffixed on collision"
    );
    assert_eq!(change.changed_fields, vec!["folder", "slug"]);
    assert_eq!(change.after.meta.version, 2);
    assert_eq!(change.after.meta.updated_by, "bob");
    assert_eq!(change.after.body, "a\n");
    assert!(!dir.path().join("memories/repo/engine/notes.md").exists());
    assert_eq!(store.get(&change.after.meta.id).unwrap(), change.after);

    let err = store
        .move_memory(&change.after.meta.id, "", Some("notes"), Some(1), "bob")
        .unwrap_err();
    assert!(matches!(err, StoreError::VersionConflict { .. }));
    let err = store
        .move_memory(
            &change.after.meta.id,
            "pitfalls",
            Some("notes"),
            None,
            "bob",
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyExists(_)),
        "explicit slugs are not suffixed"
    );

    let back = store
        .move_memory(
            &change.after.meta.id,
            "how-to/deep",
            Some("renamed"),
            None,
            "bob",
        )
        .unwrap();
    assert_eq!(
        back.after.meta.id.to_string(),
        "repo/engine/how-to/deep/renamed"
    );
    let noop = store
        .move_memory(&b.meta.id, "/pitfalls/", None, None, "bob")
        .unwrap();
    assert!(noop.is_noop());
    assert_eq!(store.list(&Filter::default()).unwrap().len(), 2);
}

#[test]
fn create_and_delete_owner_folders() {
    let (dir, store) = store();
    assert!(store.create_owner(Scope::Repo, "engine").unwrap());
    assert!(
        !store.create_owner(Scope::Repo, "engine").unwrap(),
        "second create is a no-op"
    );
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
    assert!(
        trashed[0].join("one.md").is_file(),
        "folder is kept in the trash"
    );

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
    std::fs::write(
        path.join("manual.md"),
        "# Manual note\n\nwritten by a human\n",
    )
    .unwrap();
    std::fs::write(path.join("broken.md"), "---\ntags: [\n---\n").unwrap();

    let all = store.list(&Filter::default()).unwrap();
    assert_eq!(all.len(), 1, "malformed files are skipped");
    assert_eq!(all[0].meta.title, "Manual note");
}

const UNICODE_BODY: &str = "支付重试策略：最多 3 次 🔁\n\
日本語: リトライは指数バックオフ ✅\n\
한국어 메모, Ελληνικά, العربية\n\
Symbols: → ≥ ≤ € £ © ™ «» — “quotes” ½ ∑ √\n\
Emoji: 🚀 👍🏽 👨‍👩‍👧 🇯🇵\n";

#[test]
fn content_accepts_any_unicode() {
    let (dir, store) = store();
    let m = store
        .add(NewMemory {
            kind: Some("落とし穴".into()),
            tags: vec!["支付".into(), "🚀".into(), "café".into()],
            groups: vec!["チーム".into()],
            ..new(Scope::Repo, "engine", "Payment retry policy", UNICODE_BODY)
        })
        .unwrap();
    assert_eq!(m.meta.id.to_string(), "repo/engine/payment-retry-policy");
    assert_eq!(m.body, UNICODE_BODY);

    let on_disk = std::fs::read_to_string(
        dir.path()
            .join("memories/repo/engine/payment-retry-policy.md"),
    )
    .unwrap();
    assert!(on_disk.contains("👨‍👩‍👧"), "stored as UTF-8, not escaped");
    assert!(on_disk.contains("kind: 落とし穴"));

    let fetched = store.get(m.id()).unwrap();
    assert_eq!(fetched, m);
    assert_eq!(fetched.meta.tags, vec!["支付", "🚀", "café"]);

    let change = store
        .update(
            m.id(),
            MemoryPatch {
                append: Some("追加: 絵文字 🎉".into()),
                tags: Some(vec!["Café".into(), "café".into(), "決済".into()]),
                ..Default::default()
            },
            "bob",
        )
        .unwrap();
    assert!(change.after.body.ends_with("追加: 絵文字 🎉\n"));
    assert_eq!(change.after.meta.tags, vec!["Café", "決済"]);

    for (query, expected) in [
        ("重试", 1),
        ("リトライ", 1),
        ("🚀", 1),
        ("€", 1),
        ("絵文字", 1),
        ("决济", 0),
    ] {
        let hits = store
            .search(&SearchQuery {
                text: Some(query.into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), expected, "search {query:?}");
    }

    let long = format!("{}關鍵字在最後", "很长的一行".repeat(200));
    store
        .add(new(Scope::Repo, "engine", "Long line", &long))
        .unwrap();
    let hits = store
        .search(&SearchQuery {
            text: Some("很长".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].snippet.ends_with('…'),
        "long CJK lines are cut on a character boundary"
    );

    assert!(store
        .create_folder(Scope::Repo, "engine", "pitfalls", Some("坑和事故 🔥"))
        .unwrap());
    let tree = store.folders(Scope::Repo, "engine").unwrap();
    let pitfalls = tree.iter().find(|f| f.path == "pitfalls").unwrap();
    assert_eq!(pitfalls.description.as_deref(), Some("坑和事故 🔥"));
}

#[test]
fn titles_must_be_ascii() {
    let (_dir, store) = store();
    for title in ["支付重试", "Retry 🔁", "Café notes", "naïve"] {
        let err = store
            .add(new(Scope::Repo, "engine", title, "body"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("title must be ASCII"), "{title}: {err}");
        assert!(err.contains("body"), "the error points to the body: {err}");
    }
    let ok = store
        .add(new(
            Scope::Repo,
            "engine",
            "C++ & Rust: FFI rules (v2) #1 ~ 100%",
            "printable ASCII punctuation is fine",
        ))
        .unwrap();
    let err = store
        .update(
            ok.id(),
            MemoryPatch {
                title: Some("FFI 规则".into()),
                ..Default::default()
            },
            "bob",
        )
        .unwrap_err();
    assert!(err.to_string().contains("title must be ASCII"));
    assert!(store
        .add(new(Scope::Repo, "engine", "tab\there", "b"))
        .is_err());
}

#[test]
fn existing_non_ascii_titles_stay_readable() {
    let (dir, store) = store();
    let path = dir.path().join("memories/repo/engine");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join("legacy.md"),
        "---\ntitle: 旧的标题 🔁\nkind: note\nversion: 1\n---\n\n旧内容\n",
    )
    .unwrap();
    let id: MemoryId = "repo/engine/legacy".parse().unwrap();
    let m = store.get(&id).unwrap();
    assert_eq!(m.meta.title, "旧的标题 🔁");
    let change = store
        .update(
            &id,
            MemoryPatch {
                append: Some("新内容 ✅".into()),
                ..Default::default()
            },
            "bob",
        )
        .unwrap();
    assert_eq!(
        change.after.meta.title, "旧的标题 🔁",
        "untouched titles are kept"
    );
    assert!(change.after.body.contains("新内容 ✅"));
}
