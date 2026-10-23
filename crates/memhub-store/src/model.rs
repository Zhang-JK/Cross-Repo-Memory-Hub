use std::{collections::BTreeMap, fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, StoreError};

/// Which memory space a memory lives in.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Shared knowledge about a repository: what it does, how it is built,
    /// what to watch out for when changing it.
    #[default]
    Repo,
    /// Private notebook of a single agent. Readable by everyone, writable
    /// only by the owning agent.
    Agent,
}

impl Scope {
    pub const ALL: [Scope; 2] = [Scope::Repo, Scope::Agent];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Repo => "repo",
            Scope::Agent => "agent",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "repo" | "repos" => Ok(Scope::Repo),
            "agent" | "agents" => Ok(Scope::Agent),
            _ => Err(StoreError::Invalid(format!(
                "unknown scope '{s}', expected 'repo' or 'agent'"
            ))),
        }
    }
}

/// Validates identifiers that end up as path components (repo ids, agent
/// names, slugs).
pub fn validate_name(kind: &'static str, value: &str) -> Result<()> {
    let err = |reason| StoreError::InvalidName {
        kind,
        value: value.to_string(),
        reason,
    };
    if value.is_empty() {
        return Err(err("must not be empty"));
    }
    if value.len() > 64 {
        return Err(err("must be at most 64 characters"));
    }
    if !value.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(err("must start with an ASCII letter or digit"));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(err(
            "only ASCII letters, digits, '-', '_' and '.' are allowed",
        ));
    }
    if value.contains("..") {
        return Err(err("must not contain '..'"));
    }
    Ok(())
}

/// Validates free-form labels such as tags, groups and kinds.
pub fn validate_label(kind: &'static str, value: &str) -> Result<()> {
    let err = |reason| StoreError::InvalidName {
        kind,
        value: value.to_string(),
        reason,
    };
    if value.trim().is_empty() {
        return Err(err("must not be empty"));
    }
    if value.chars().count() > 64 {
        return Err(err("must be at most 64 characters"));
    }
    if value.chars().any(char::is_control) {
        return Err(err("must not contain control characters"));
    }
    Ok(())
}

pub(crate) fn normalize_labels(kind: &'static str, labels: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(labels.len());
    for label in labels {
        let label = label.trim().to_string();
        validate_label(kind, &label)?;
        if !out.iter().any(|l| l.eq_ignore_ascii_case(&label)) {
            out.push(label);
        }
    }
    Ok(out)
}

/// Turns a title into a filesystem-safe slug.
pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "memory".to_string()
    } else {
        slug.to_string()
    }
}

/// Deepest folder nesting below an owner (`a/b/c/d/e/f`).
pub const MAX_FOLDER_DEPTH: usize = 6;

/// Normalizes a folder path such as `/modules/auth/` to `modules/auth`.
/// The empty string is the owner's root folder.
pub fn normalize_folder(folder: &str) -> Result<String> {
    let segments: Vec<&str> = folder
        .trim()
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() > MAX_FOLDER_DEPTH {
        return Err(StoreError::Invalid(format!(
            "folder '{folder}' is nested too deeply (at most {MAX_FOLDER_DEPTH} levels)"
        )));
    }
    for segment in &segments {
        validate_name("folder name", segment)?;
    }
    Ok(segments.join("/"))
}

/// Address of a memory: `<scope>/<owner>/[<folder>/]<slug>`, mirroring its
/// location on disk. `folder` may itself contain `/` (nested folders) and is
/// empty for memories in the owner's root folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryId {
    pub scope: Scope,
    pub owner: String,
    pub folder: String,
    pub slug: String,
}

impl MemoryId {
    /// A memory in the owner's root folder.
    pub fn new(scope: Scope, owner: impl Into<String>, slug: impl Into<String>) -> Result<Self> {
        Self::in_folder(scope, owner, "", slug)
    }

    pub fn in_folder(
        scope: Scope,
        owner: impl Into<String>,
        folder: &str,
        slug: impl Into<String>,
    ) -> Result<Self> {
        let id = MemoryId {
            scope,
            owner: owner.into(),
            folder: normalize_folder(folder)?,
            slug: slug.into(),
        };
        validate_name(owner_kind(scope), &id.owner)?;
        validate_name("memory slug", &id.slug)?;
        Ok(id)
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// `true` if the memory lives in `folder` or one of its subfolders.
    pub fn is_under(&self, folder: &str) -> bool {
        is_under(&self.folder, folder)
    }
}

/// `true` if `path` equals `folder` or is nested inside it. Every path is
/// under the root folder (`""`).
pub fn is_under(path: &str, folder: &str) -> bool {
    folder.is_empty()
        || path == folder
        || (path.starts_with(folder) && path.as_bytes().get(folder.len()) == Some(&b'/'))
}

pub(crate) fn owner_kind(scope: Scope) -> &'static str {
    match scope {
        Scope::Repo => "repo id",
        Scope::Agent => "agent name",
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/", self.scope(), self.owner)?;
        if !self.folder.is_empty() {
            write!(f, "{}/", self.folder)?;
        }
        f.write_str(&self.slug)
    }
}

impl FromStr for MemoryId {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim().trim_end_matches(".md");
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() < 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(StoreError::InvalidId(s.to_string()));
        }
        let scope = parts[0]
            .parse::<Scope>()
            .map_err(|_| StoreError::InvalidId(s.to_string()))?;
        let folder = parts[2..parts.len() - 1].join("/");
        MemoryId::in_folder(scope, parts[1], &folder, parts[parts.len() - 1])
    }
}

impl Serialize for MemoryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MemoryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

fn default_kind() -> String {
    "note".to_string()
}

fn default_version() -> u64 {
    1
}

fn default_author() -> String {
    "unknown".to_string()
}

/// The properties stored in a memory file's YAML front matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryMeta {
    /// Derived from the file location; the front-matter copy is informational.
    #[serde(default)]
    pub id: MemoryId,
    #[serde(default)]
    pub title: String,
    /// Free-form category, e.g. `overview`, `architecture`, `pitfall`,
    /// `task-log`, `insight`.
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default = "default_author")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_by: String,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default = "default_version")]
    pub version: u64,
    /// Unknown front-matter keys are preserved round-trip.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl MemoryMeta {
    pub fn scope(&self) -> Scope {
        self.id.scope()
    }

    pub fn owner(&self) -> &str {
        &self.id.owner
    }

    pub fn folder(&self) -> &str {
        &self.id.folder
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t.eq_ignore_ascii_case(tag))
    }

    pub fn in_group(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g.eq_ignore_ascii_case(group))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(flatten)]
    pub meta: MemoryMeta,
    pub body: String,
}

impl Memory {
    pub fn id(&self) -> &MemoryId {
        &self.meta.id
    }
}

/// Input for creating a memory.
#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    pub scope: Scope,
    pub owner: String,
    /// Folder below the owner (`modules/auth`); empty for the root folder.
    /// Missing folders are created.
    pub folder: String,
    pub title: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: String,
    pub author: String,
    /// Preferred slug; derived from the title when absent. A numeric suffix
    /// is appended if the slug is already taken.
    pub slug: Option<String>,
}

/// Partial update. `None` fields are left untouched.
#[derive(Debug, Clone, Default)]
pub struct MemoryPatch {
    pub title: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
    /// Replaces the body.
    pub body: Option<String>,
    /// Appended to the body (after `body` replacement, if both are given).
    pub append: Option<String>,
    /// Optimistic concurrency guard.
    pub expected_version: Option<u64>,
}

/// Before/after snapshot of a mutation.
#[derive(Debug, Clone)]
pub struct Change {
    pub before: Memory,
    pub after: Memory,
    pub changed_fields: Vec<&'static str>,
}

impl Change {
    pub fn is_noop(&self) -> bool {
        self.changed_fields.is_empty()
    }
}

/// Restricts which memories are considered by list/search/group operations.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub scope: Option<Scope>,
    pub owner: Option<String>,
    /// Normalized folder path; matches the folder and all its subfolders.
    pub folder: Option<String>,
    pub kind: Option<String>,
    /// All listed tags must be present (case-insensitive).
    pub tags: Vec<String>,
    pub group: Option<String>,
    pub author: Option<String>,
}

impl Filter {
    pub fn matches(&self, meta: &MemoryMeta) -> bool {
        if self.scope.is_some_and(|s| s != meta.scope()) {
            return false;
        }
        if self.owner.as_ref().is_some_and(|o| o != meta.owner()) {
            return false;
        }
        if self.folder.as_ref().is_some_and(|f| !meta.id.is_under(f)) {
            return false;
        }
        if self
            .kind
            .as_ref()
            .is_some_and(|k| !k.eq_ignore_ascii_case(&meta.kind))
        {
            return false;
        }
        if !self.tags.iter().all(|t| meta.has_tag(t)) {
            return false;
        }
        if self.group.as_ref().is_some_and(|g| !meta.in_group(g)) {
            return false;
        }
        if self
            .author
            .as_ref()
            .is_some_and(|a| a != &meta.author && a != &meta.updated_by)
        {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub text: Option<String>,
    pub filter: Filter,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub meta: MemoryMeta,
    pub score: f64,
    pub snippet: String,
}

/// Dimension used by [`crate::MemoryStore::group_by`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupKey {
    Group,
    Kind,
    Tag,
    Owner,
    Scope,
    Author,
    /// `<scope>/<owner>/<folder>`, e.g. `repo/memhub/modules/auth`.
    Folder,
}

impl FromStr for GroupKey {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "group" | "groups" => GroupKey::Group,
            "kind" => GroupKey::Kind,
            "tag" | "tags" => GroupKey::Tag,
            "owner" => GroupKey::Owner,
            "scope" => GroupKey::Scope,
            "author" => GroupKey::Author,
            "folder" | "folders" => GroupKey::Folder,
            _ => {
                return Err(StoreError::Invalid(format!(
                    "unknown group key '{s}', expected one of group, kind, tag, owner, scope, author, folder"
                )))
            }
        })
    }
}

/// One folder of an owner's memory tree, as returned by
/// [`crate::MemoryStore::folders`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FolderInfo {
    /// Normalized path below the owner; empty for the root folder.
    pub path: String,
    /// Text of the folder's `_index.md`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Memories directly in this folder.
    pub memory_count: usize,
    /// Memories in this folder and all subfolders.
    pub total_count: usize,
    /// Most recent `updated_at` of any memory in the subtree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl FolderInfo {
    pub fn depth(&self) -> usize {
        if self.path.is_empty() {
            0
        } else {
            self.path.split('/').count()
        }
    }

    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
}
