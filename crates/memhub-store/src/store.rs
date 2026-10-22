use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use chrono::Utc;

use crate::{
    error::{IoContext, Result, StoreError},
    frontmatter,
    model::{
        normalize_labels, owner_kind, slugify, validate_label, validate_name, Change, Filter,
        GroupKey, Memory, MemoryId, MemoryMeta, MemoryPatch, NewMemory, Scope, SearchHit,
        SearchQuery,
    },
    search,
};

const DEFAULT_SEARCH_LIMIT: usize = 20;

/// File-backed memory store.
///
/// Layout under the data root:
///
/// ```text
/// memories/repo/<repo-id>/<slug>.md
/// memories/agent/<agent-name>/<slug>.md
/// trash/<scope>/<owner>/<slug>.<timestamp>.md   (deleted memories)
/// ```
///
/// The filesystem is the source of truth (no in-memory cache), so several
/// processes may share one data root. Writes are atomic (temp file + rename)
/// and guarded by optimistic version checks.
pub struct MemoryStore {
    memories: PathBuf,
    trash: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub fn open(data_root: impl AsRef<Path>) -> Result<Self> {
        let root = data_root.as_ref();
        let memories = root.join("memories");
        for scope in Scope::ALL {
            let dir = memories.join(scope.as_str());
            fs::create_dir_all(&dir).at(&dir)?;
        }
        Ok(Self {
            memories,
            trash: root.join("trash"),
            write_lock: Mutex::new(()),
        })
    }

    pub fn path_of(&self, id: &MemoryId) -> PathBuf {
        self.memories
            .join(id.scope.as_str())
            .join(&id.owner)
            .join(format!("{}.md", id.slug))
    }

    pub fn exists(&self, id: &MemoryId) -> bool {
        self.path_of(id).is_file()
    }

    pub fn get(&self, id: &MemoryId) -> Result<Memory> {
        let path = self.path_of(id);
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_string()))
            }
            other => other.at(&path)?,
        };
        frontmatter::parse(&text, id.clone(), &path)
    }

    pub fn add(&self, new: NewMemory) -> Result<Memory> {
        validate_name(owner_kind(new.scope), &new.owner)?;
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(StoreError::Invalid("title must not be empty".into()));
        }
        if new.author.trim().is_empty() {
            return Err(StoreError::Invalid("author must not be empty".into()));
        }
        let kind = new
            .kind
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| "note".to_string());
        validate_label("kind", &kind)?;
        let tags = normalize_labels("tag", new.tags)?;
        let groups = normalize_labels("group", new.groups)?;
        let explicit_slug = new.slug.is_some();
        let base = match new.slug {
            Some(slug) => {
                validate_name("memory slug", &slug)?;
                slug
            }
            None => slugify(&title),
        };

        let dir = self.memories.join(new.scope.as_str()).join(&new.owner);
        fs::create_dir_all(&dir).at(&dir)?;
        let now = Utc::now();

        let _guard = self.lock();
        for n in 1u32.. {
            let slug = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let id = MemoryId::new(new.scope, &new.owner, slug)?;
            let memory = Memory {
                meta: MemoryMeta {
                    id: id.clone(),
                    title: title.clone(),
                    kind: kind.clone(),
                    tags: tags.clone(),
                    groups: groups.clone(),
                    author: new.author.clone(),
                    updated_by: String::new(),
                    created_at: now,
                    updated_at: now,
                    version: 1,
                    extra: BTreeMap::new(),
                },
                body: frontmatter::normalize_body(&new.body),
            };
            match write_new(&self.path_of(&id), &frontmatter::render(&memory)?) {
                Ok(()) => return Ok(memory),
                Err(StoreError::AlreadyExists(_)) if !explicit_slug => continue,
                Err(StoreError::AlreadyExists(_)) => {
                    return Err(StoreError::AlreadyExists(id.to_string()))
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("slug suffix space exhausted")
    }

    pub fn update(&self, id: &MemoryId, patch: MemoryPatch, editor: &str) -> Result<Change> {
        let expected = patch.expected_version;
        self.modify(id, expected, editor, |m| apply_patch(m, patch))
    }

    /// Moves the memory to the trash directory and returns its last content.
    pub fn delete(&self, id: &MemoryId, expected_version: Option<u64>) -> Result<Memory> {
        let _guard = self.lock();
        let memory = self.get(id)?;
        check_version(&memory, expected_version)?;
        let dir = self.trash.join(id.scope.as_str()).join(&id.owner);
        fs::create_dir_all(&dir).at(&dir)?;
        let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let target = dir.join(format!("{}.{stamp}.md", id.slug));
        let source = self.path_of(id);
        fs::rename(&source, &target).at(&source)?;
        Ok(memory)
    }

    /// All memories matching the filter, most recently updated first.
    pub fn list(&self, filter: &Filter) -> Result<Vec<Memory>> {
        let mut out = self.scan(filter)?;
        out.sort_by(|a, b| {
            b.meta
                .updated_at
                .cmp(&a.meta.updated_at)
                .then_with(|| a.meta.id.cmp(&b.meta.id))
        });
        Ok(out)
    }

    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let terms = query.text.as_deref().map(search::terms).unwrap_or_default();
        let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).max(1);

        let mut hits: Vec<SearchHit> = self
            .list(&query.filter)?
            .into_iter()
            .filter_map(|memory| {
                let score = if terms.is_empty() {
                    0.0
                } else {
                    search::score(&memory, &terms)?
                };
                let snippet = search::snippet(&memory, &terms);
                Some(SearchHit {
                    meta: memory.meta,
                    score,
                    snippet,
                })
            })
            .collect();
        // `list` already sorted by recency; a stable sort keeps that as the tie-breaker.
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Adds (`add = true`) or removes the memories from a named group.
    /// Returns only the memories that actually changed.
    pub fn set_group(
        &self,
        ids: &[MemoryId],
        group: &str,
        add: bool,
        editor: &str,
    ) -> Result<Vec<Change>> {
        let group = group.trim();
        validate_label("group", group)?;
        for id in ids {
            if !self.exists(id) {
                return Err(StoreError::NotFound(id.to_string()));
            }
        }
        let mut changes = Vec::new();
        for id in ids {
            let change = self.modify(id, None, editor, |m| {
                if add {
                    if !m.meta.in_group(group) {
                        m.meta.groups.push(group.to_string());
                    }
                } else {
                    m.meta.groups.retain(|g| !g.eq_ignore_ascii_case(group));
                }
                Ok(())
            })?;
            if !change.is_noop() {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    /// Buckets matching memories by the given key. Memories with several
    /// tags/groups appear in several buckets; ones with none are omitted for
    /// `Tag`/`Group`.
    pub fn group_by(&self, filter: &Filter, key: GroupKey) -> Result<BTreeMap<String, Vec<MemoryMeta>>> {
        let mut out: BTreeMap<String, Vec<MemoryMeta>> = BTreeMap::new();
        for memory in self.list(filter)? {
            let meta = memory.meta;
            let keys: Vec<String> = match key {
                GroupKey::Group => meta.groups.clone(),
                GroupKey::Tag => meta.tags.clone(),
                GroupKey::Kind => vec![meta.kind.clone()],
                GroupKey::Owner => vec![format!("{}/{}", meta.scope(), meta.owner())],
                GroupKey::Scope => vec![meta.scope().to_string()],
                GroupKey::Author => vec![meta.author.clone()],
            };
            for k in keys {
                out.entry(k).or_default().push(meta.clone());
            }
        }
        Ok(out)
    }

    /// Owners (repo ids or agent names) that have a memory directory.
    pub fn owners(&self, scope: Scope) -> Result<Vec<String>> {
        let dir = self.memories.join(scope.as_str());
        let mut out = Vec::new();
        for entry in read_dir(&dir)? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if validate_name(owner_kind(scope), name).is_ok() {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn modify(
        &self,
        id: &MemoryId,
        expected_version: Option<u64>,
        editor: &str,
        f: impl FnOnce(&mut Memory) -> Result<()>,
    ) -> Result<Change> {
        let _guard = self.lock();
        let before = self.get(id)?;
        check_version(&before, expected_version)?;
        let mut after = before.clone();
        f(&mut after)?;
        let changed_fields = changed_fields(&before, &after);
        if !changed_fields.is_empty() {
            after.meta.version = before.meta.version + 1;
            after.meta.updated_at = Utc::now();
            after.meta.updated_by = editor.to_string();
            write_replace(&self.path_of(id), &frontmatter::render(&after)?)?;
        }
        Ok(Change {
            before,
            after,
            changed_fields,
        })
    }

    fn scan(&self, filter: &Filter) -> Result<Vec<Memory>> {
        let scopes: Vec<Scope> = match filter.scope {
            Some(s) => vec![s],
            None => Scope::ALL.to_vec(),
        };
        let mut out = Vec::new();
        for scope in scopes {
            let owners = match &filter.owner {
                Some(owner) => vec![owner.clone()],
                None => self.owners(scope)?,
            };
            for owner in owners {
                if validate_name(owner_kind(scope), &owner).is_err() {
                    continue;
                }
                let dir = self.memories.join(scope.as_str()).join(&owner);
                for entry in read_dir(&dir)? {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let Ok(id) = MemoryId::new(scope, &owner, slug) else {
                        continue;
                    };
                    match self.get(&id) {
                        Ok(memory) if filter.matches(&memory.meta) => out.push(memory),
                        Ok(_) | Err(StoreError::NotFound(_)) => {}
                        Err(e) => tracing::warn!("skipping unreadable memory {id}: {e}"),
                    }
                }
            }
        }
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn check_version(memory: &Memory, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if expected != memory.meta.version => Err(StoreError::VersionConflict {
            id: memory.meta.id.to_string(),
            expected,
            current: memory.meta.version,
        }),
        _ => Ok(()),
    }
}

fn apply_patch(memory: &mut Memory, patch: MemoryPatch) -> Result<()> {
    let meta = &mut memory.meta;
    if let Some(title) = patch.title {
        let title = title.trim();
        if title.is_empty() {
            return Err(StoreError::Invalid("title must not be empty".into()));
        }
        meta.title = title.to_string();
    }
    if let Some(kind) = patch.kind {
        let kind = kind.trim();
        validate_label("kind", kind)?;
        meta.kind = kind.to_string();
    }
    if let Some(tags) = patch.tags {
        meta.tags = normalize_labels("tag", tags)?;
    }
    if let Some(groups) = patch.groups {
        meta.groups = normalize_labels("group", groups)?;
    }
    if let Some(body) = patch.body {
        memory.body = body;
    }
    if let Some(append) = patch.append {
        let body = &mut memory.body;
        if !body.trim().is_empty() {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push('\n');
        }
        body.push_str(append.trim_end());
    }
    memory.body = frontmatter::normalize_body(&memory.body);
    Ok(())
}

fn changed_fields(before: &Memory, after: &Memory) -> Vec<&'static str> {
    let (b, a) = (&before.meta, &after.meta);
    let mut out = Vec::new();
    if b.title != a.title {
        out.push("title");
    }
    if b.kind != a.kind {
        out.push("kind");
    }
    if b.tags != a.tags {
        out.push("tags");
    }
    if b.groups != a.groups {
        out.push("groups");
    }
    if b.extra != a.extra {
        out.push("extra");
    }
    if before.body != after.body {
        out.push("body");
    }
    out
}

fn read_dir(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        other => other.at(dir)?.map(|e| e.at(dir)).collect(),
    }
}

fn temp_in(path: &Path, text: &str) -> Result<tempfile::NamedTempFile> {
    let dir = path.parent().expect("memory path has a parent");
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .suffix(".md.part")
        .tempfile_in(dir)
        .at(dir)?;
    tmp.write_all(text.as_bytes()).at(tmp.path().to_path_buf())?;
    tmp.as_file().sync_all().at(tmp.path().to_path_buf())?;
    Ok(tmp)
}

fn write_new(path: &Path, text: &str) -> Result<()> {
    temp_in(path, text)?.persist_noclobber(path).map_err(|e| {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            StoreError::AlreadyExists(path.display().to_string())
        } else {
            StoreError::Io {
                path: path.to_path_buf(),
                source: e.error,
            }
        }
    })?;
    Ok(())
}

fn write_replace(path: &Path, text: &str) -> Result<()> {
    temp_in(path, text)?
        .persist(path)
        .map_err(|e| StoreError::Io {
            path: path.to_path_buf(),
            source: e.error,
        })?;
    Ok(())
}
