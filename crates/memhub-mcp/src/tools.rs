//! MCP tool catalogue and dispatch onto [`AgentSession`].

use memhub_app::{
    store::{self, Filter, GroupKey, MemoryId, MemoryMeta, MemoryPatch, Scope, SearchQuery},
    AddMemory, AgentSession, AppError, Browse,
};
use schemars::{generate::SchemaSettings, JsonSchema};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

// ---- argument types ------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ScopeArg {
    Repo,
    Agent,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Repo => Scope::Repo,
            ScopeArg::Agent => Scope::Agent,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum GroupKeyArg {
    Group,
    Kind,
    Tag,
    Owner,
    Scope,
    Author,
    Folder,
}

impl From<GroupKeyArg> for GroupKey {
    fn from(k: GroupKeyArg) -> Self {
        match k {
            GroupKeyArg::Group => GroupKey::Group,
            GroupKeyArg::Kind => GroupKey::Kind,
            GroupKeyArg::Tag => GroupKey::Tag,
            GroupKeyArg::Owner => GroupKey::Owner,
            GroupKeyArg::Scope => GroupKey::Scope,
            GroupKeyArg::Author => GroupKey::Author,
            GroupKeyArg::Folder => GroupKey::Folder,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum GroupAction {
    #[default]
    Add,
    Remove,
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct ConnectArgs {
    /// Agent name, e.g. "crash-analyzer": ASCII letters, digits, '-', '_' or '.'.
    /// Every user running the same agent should use the same name to share its memory.
    name: String,
    /// The agent's task specialty, e.g. "feature-impl", "crash-analysis", "perf-opt".
    /// Required only when the agent does not exist yet.
    #[serde(default)]
    role: Option<String>,
    /// What this agent is responsible for (used when the agent is created).
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct AddRepoArgs {
    /// Repository id: ASCII letters, digits, '-', '_' or '.', e.g. "payment-service".
    repo: String,
    /// One-line description of the repository (used when the repo is new).
    #[serde(default)]
    description: Option<String>,
    /// Local path or remote URL of the repository.
    #[serde(default)]
    location: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RepoArgs {
    /// Repository id.
    repo: String,
}

#[derive(Deserialize, JsonSchema)]
struct FilterArgs {
    /// Restrict to repo memory or agent memory.
    #[serde(default)]
    scope: Option<ScopeArg>,
    /// Repo id (scope=repo) or agent name (scope=agent).
    #[serde(default)]
    owner: Option<String>,
    /// Only memories in this folder or its subfolders, e.g. "modules/auth".
    /// Usually combined with scope and owner.
    #[serde(default)]
    folder: Option<String>,
    /// Memory kind, e.g. overview, architecture, pitfall, task-log.
    #[serde(default)]
    kind: Option<String>,
    /// Only memories carrying all of these tags.
    #[serde(default)]
    tags: Vec<String>,
    /// Only memories in this group.
    #[serde(default)]
    group: Option<String>,
    /// Only memories created or last updated by this agent.
    #[serde(default)]
    author: Option<String>,
}

impl TryFrom<FilterArgs> for Filter {
    type Error = ToolError;

    fn try_from(f: FilterArgs) -> Result<Self, ToolError> {
        let folder = f
            .folder
            .as_deref()
            .map(store::normalize_folder)
            .transpose()?
            .filter(|f| !f.is_empty());
        Ok(Filter {
            scope: f.scope.map(Into::into),
            owner: f.owner.filter(|o| !o.trim().is_empty()),
            folder,
            kind: f.kind.filter(|k| !k.trim().is_empty()),
            tags: f.tags,
            group: f.group.filter(|g| !g.trim().is_empty()),
            author: f.author.filter(|a| !a.trim().is_empty()),
        })
    }
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Keywords; every term is matched case-insensitively against title, folder, tags, kind and body.
    /// Omit to list the most recently updated memories matching the filters.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: FilterArgs,
    /// Maximum number of hits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct ListArgs {
    #[serde(flatten)]
    filter: FilterArgs,
    /// Bucket the result by this property instead of returning a flat list.
    #[serde(default)]
    group_by: Option<GroupKeyArg>,
}

#[derive(Deserialize, JsonSchema)]
struct IdArgs {
    /// Memory id, e.g. "repo/payment-service/pitfalls/retry-policy".
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct BrowseArgs {
    /// "repo" (default) or "agent".
    #[serde(default)]
    scope: Option<ScopeArg>,
    /// Repo id (scope=repo, required) or agent name (scope=agent, defaults to yourself).
    #[serde(default)]
    owner: Option<String>,
    /// Show only this folder and its subfolders, e.g. "modules".
    #[serde(default)]
    folder: Option<String>,
    /// List memory titles under each folder (default true). Set false for a folders-only outline.
    #[serde(default)]
    memories: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct CreateFolderArgs {
    /// "repo" for a repository you joined, "agent" for your own space.
    scope: ScopeArg,
    /// Repository id; required when scope is "repo".
    #[serde(default)]
    repo: Option<String>,
    /// Folder path below the repo / agent root, e.g. "modules/auth". Missing parents are created.
    folder: String,
    /// One line on what belongs in this folder, shown by browse_memory. Replaces the current
    /// description of an existing folder; "" removes it.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MoveMemoryArgs {
    /// Memory id to move.
    id: String,
    /// Target folder in the same repo / agent space, e.g. "pitfalls"; "" for the root.
    /// Missing folders are created.
    folder: String,
    /// New slug (file name); keeps the current one if omitted.
    #[serde(default)]
    slug: Option<String>,
    /// Fail if the memory's current version differs.
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct AddMemoryArgs {
    /// "repo" for shared repository knowledge, "agent" for your own notebook.
    scope: ScopeArg,
    /// Repository id; required when scope is "repo". You must have joined it via add_repo.
    #[serde(default)]
    repo: Option<String>,
    /// Folder for the memory, e.g. "architecture", "modules/auth", "pitfalls". Pick the most
    /// specific existing folder (see browse_memory); missing folders are created. Leave empty
    /// only for the repo's top-level overview.
    #[serde(default)]
    folder: Option<String>,
    /// Short descriptive title.
    title: String,
    /// Markdown content.
    body: String,
    /// Category. Repo: overview, architecture, implementation, pitfall. Agent: task-log, insight, todo.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// Groups to put the memory in.
    #[serde(default)]
    groups: Vec<String>,
    /// Explicit slug for the id; derived from the title if omitted.
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct UpdateMemoryArgs {
    /// Memory id.
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    /// Replaces all tags.
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// Replaces all groups.
    #[serde(default)]
    groups: Option<Vec<String>>,
    /// Replaces the whole body.
    #[serde(default)]
    body: Option<String>,
    /// Appended to the body as a new paragraph.
    #[serde(default)]
    append: Option<String>,
    /// Fail if the memory's current version differs (optimistic locking).
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteMemoryArgs {
    /// Memory id.
    id: String,
    /// Fail if the memory's current version differs.
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct GroupArgs {
    /// Group name.
    group: String,
    /// Memory ids to add to or remove from the group.
    ids: Vec<String>,
    /// "add" (default) or "remove".
    #[serde(default)]
    action: GroupAction,
}

// ---- catalogue -------------------------------------------------------------

struct ToolDef {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    read_only: bool,
    destructive: bool,
}

const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "connect_agent",
        title: "Connect agent",
        description: "Identify this connection by agent name; call this first. If the agent does not exist yet, pass 'role' to create it.",
        schema: schema::<ConnectArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "whoami",
        title: "Who am I",
        description: "Show the identified agent, its working repos and connection details.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "list_agents",
        title: "List agents",
        description: "List all registered agents with their roles and working repos.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "add_repo",
        title: "Add working repo",
        description: "Add a repository to your working set, registering it if it is new. Required before writing repo memory.",
        schema: schema::<AddRepoArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "leave_repo",
        title: "Leave repo",
        description: "Remove a repository from your working set. Its memories are kept.",
        schema: schema::<RepoArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "list_repos",
        title: "List repos",
        description: "List registered repositories with members and memory counts.",
        schema: schema::<NoArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "browse_memory",
        title: "Browse memory tree",
        description: "Show the folder tree of a repo's (or agent's) memory with folder descriptions, counts and memory titles. Start here when you begin work on a repo, and before add_memory to pick the right folder.",
        schema: schema::<BrowseArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "search_memory",
        title: "Search memory",
        description: "Keyword search over all memories (repo and agent), ranked by relevance, with optional filters.",
        schema: schema::<SearchArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "list_memories",
        title: "List memories",
        description: "List memories matching filters (including a folder), optionally grouped by group, kind, tag, owner, scope, author or folder.",
        schema: schema::<ListArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "get_memory",
        title: "Get memory",
        description: "Fetch the full Markdown content and properties of a memory.",
        schema: schema::<IdArgs>,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "add_memory",
        title: "Add memory",
        description: "Create a memory in a repo you joined (scope=repo) or in your own agent space (scope=agent). Put it in the most specific fitting folder (e.g. folder=\"modules/auth\"); keep the root for the overview only.",
        schema: schema::<AddMemoryArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "update_memory",
        title: "Update memory",
        description: "Change a memory's title, kind, tags, groups or body (replace or append). Only fields you pass are changed.",
        schema: schema::<UpdateMemoryArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "create_folder",
        title: "Create folder",
        description: "Create a memory folder (missing parents too) with a one-line description of what belongs in it, or update an existing folder's description.",
        schema: schema::<CreateFolderArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "move_memory",
        title: "Move memory",
        description: "Move a memory to another folder of the same repo / agent space, optionally renaming it. Use it to tidy a crowded root or folder; the memory id changes.",
        schema: schema::<MoveMemoryArgs>,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "delete_memory",
        title: "Delete memory",
        description: "Delete a memory (moved to the hub's trash).",
        schema: schema::<DeleteMemoryArgs>,
        read_only: false,
        destructive: true,
    },
    ToolDef {
        name: "group_memories",
        title: "Group memories",
        description: "Add memories to, or remove them from, a named group. Use list_memories with group_by=group to view groups.",
        schema: schema::<GroupArgs>,
        read_only: false,
        destructive: false,
    },
];

pub fn definitions() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.title,
                "description": t.description,
                "inputSchema": (t.schema)(),
                "annotations": {
                    "title": t.title,
                    "readOnlyHint": t.read_only,
                    "destructiveHint": t.destructive,
                    "openWorldHint": false,
                },
            })
        })
        .collect()
}

fn schema<T: JsonSchema>() -> Value {
    let mut settings = SchemaSettings::draft07();
    settings.inline_subschemas = true;
    settings.meta_schema = None;
    let mut value = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    simplify(&mut value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.insert("type".into(), json!("object"));
        obj.entry("properties").or_insert_with(|| json!({}));
    }
    value
}

/// Collapses `Option<T>` encodings (`["T","null"]`, `anyOf [T, null]`) into
/// plain `T`; optionality is already expressed by `required`. Keeps schemas
/// digestible for clients with limited JSON-Schema support.
fn simplify(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type") {
                let non_null: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if non_null.len() == 1 {
                    map.insert("type".into(), non_null[0].clone());
                }
            }
            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(variants)) = map.get(key) {
                    let non_null: Vec<Value> = variants
                        .iter()
                        .filter(|v| v.get("type") != Some(&json!("null")))
                        .cloned()
                        .collect();
                    if non_null.len() == 1 {
                        map.remove(key);
                        if let Value::Object(inner) = &non_null[0] {
                            for (k, v) in inner {
                                map.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                    }
                }
            }
            if let Some(Value::Array(values)) = map.get_mut("enum") {
                values.retain(|v| !v.is_null());
            }
            if map.get("default") == Some(&Value::Null) {
                map.remove("default");
            }
            for child in map.values_mut() {
                simplify(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(simplify),
        _ => {}
    }
}

// ---- dispatch ----------------------------------------------------------------

struct ToolError {
    code: &'static str,
    message: String,
}

impl From<AppError> for ToolError {
    fn from(e: AppError) -> Self {
        ToolError {
            code: e.code(),
            message: e.to_string(),
        }
    }
}

impl From<store::StoreError> for ToolError {
    fn from(e: store::StoreError) -> Self {
        AppError::from(e).into()
    }
}

struct ToolOutput {
    structured: Value,
    /// Human-oriented text; defaults to pretty-printed `structured`.
    text: Option<String>,
}

impl From<Value> for ToolOutput {
    fn from(structured: Value) -> Self {
        ToolOutput {
            structured,
            text: None,
        }
    }
}

/// Runs a tool and returns the `tools/call` result object, or `None` if the
/// tool does not exist.
pub fn call(session: &mut AgentSession, name: &str, args: Value) -> Option<Value> {
    let result = match name {
        "connect_agent" => connect_agent(session, args),
        "whoami" => session
            .whoami()
            .map_err(Into::into)
            .map(|w| json!(w).into()),
        "list_agents" => session
            .list_agents()
            .map_err(Into::into)
            .map(|a| json!({ "agents": a }).into()),
        "add_repo" => add_repo(session, args),
        "leave_repo" => leave_repo(session, args),
        "list_repos" => session
            .list_repos()
            .map_err(Into::into)
            .map(|r| json!({ "repos": r }).into()),
        "browse_memory" => browse_memory(session, args),
        "search_memory" => search_memory(session, args),
        "list_memories" => list_memories(session, args),
        "get_memory" => get_memory(session, args),
        "add_memory" => add_memory(session, args),
        "update_memory" => update_memory(session, args),
        "create_folder" => create_folder(session, args),
        "move_memory" => move_memory(session, args),
        "delete_memory" => delete_memory(session, args),
        "group_memories" => group_memories(session, args),
        _ => return None,
    };
    if let Err(e) = &result {
        tracing::debug!(tool = name, agent = ?session.agent(), "tool failed: {}", e.message);
    }
    Some(match result {
        Ok(out) => {
            let text = out.text.unwrap_or_else(|| {
                serde_json::to_string_pretty(&out.structured).unwrap_or_default()
            });
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": out.structured,
                "isError": false,
            })
        }
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("error [{}]: {}", e.code, e.message) }],
            "isError": true,
        }),
    })
}

type ToolResult = Result<ToolOutput, ToolError>;

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() { json!({}) } else { args };
    serde_json::from_value(args).map_err(|e| ToolError {
        code: "invalid_input",
        message: format!("invalid arguments: {e}"),
    })
}

fn memory_id(id: &str) -> Result<MemoryId, ToolError> {
    Ok(id.parse::<MemoryId>()?)
}

fn summary(meta: &MemoryMeta) -> Value {
    json!({
        "id": meta.id,
        "folder": meta.folder(),
        "title": meta.title,
        "kind": meta.kind,
        "tags": meta.tags,
        "groups": meta.groups,
        "author": meta.author,
        "updated_by": meta.updated_by,
        "version": meta.version,
        "updated_at": meta.updated_at,
    })
}

fn connect_agent(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ConnectArgs = parse(args)?;
    let connected = session.connect(&a.name, a.role.as_deref(), a.description.as_deref())?;
    Ok(json!(connected).into())
}

fn add_repo(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: AddRepoArgs = parse(args)?;
    let join = session.add_repo(&a.repo, a.description.as_deref(), a.location.as_deref())?;
    let mut out = json!(join);
    let tree = session.browse(Scope::Repo, Some(&join.repo.id), "")?;
    out["memory_count"] = json!(tree.memories.len());
    out["next_step"] = json!(repo_next_step(&tree));
    Ok(out.into())
}

fn leave_repo(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: RepoArgs = parse(args)?;
    let repos = session.leave_repo(&a.repo)?;
    Ok(json!({ "working_repos": repos }).into())
}

fn search_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: SearchArgs = parse(args)?;
    let hits = session.search(&SearchQuery {
        text: a.query.filter(|q| !q.trim().is_empty()),
        filter: a.filter.try_into()?,
        limit: a.limit,
    })?;
    let hits: Vec<Value> = hits
        .iter()
        .map(|h| {
            let mut v = summary(&h.meta);
            v["score"] = json!(h.score);
            v["snippet"] = json!(h.snippet);
            v
        })
        .collect();
    Ok(json!({ "count": hits.len(), "hits": hits }).into())
}

fn list_memories(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: ListArgs = parse(args)?;
    let filter: Filter = a.filter.try_into()?;
    match a.group_by {
        Some(key) => {
            let groups = session.group_by(&filter, key.into())?;
            let groups: serde_json::Map<String, Value> = groups
                .into_iter()
                .map(|(k, metas)| (k, Value::Array(metas.iter().map(summary).collect())))
                .collect();
            Ok(json!({ "group_by": key_name(key), "groups": groups }).into())
        }
        None => {
            let metas = session.list(&filter)?;
            let items: Vec<Value> = metas.iter().map(summary).collect();
            Ok(json!({ "count": items.len(), "memories": items }).into())
        }
    }
}

fn key_name(key: GroupKeyArg) -> &'static str {
    match key {
        GroupKeyArg::Group => "group",
        GroupKeyArg::Kind => "kind",
        GroupKeyArg::Tag => "tag",
        GroupKeyArg::Owner => "owner",
        GroupKeyArg::Scope => "scope",
        GroupKeyArg::Author => "author",
        GroupKeyArg::Folder => "folder",
    }
}

fn get_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: IdArgs = parse(args)?;
    let memory = session.get_memory(&memory_id(&a.id)?)?;
    Ok(ToolOutput {
        text: Some(store::render_memory(&memory)?),
        structured: json!(memory),
    })
}

fn add_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: AddMemoryArgs = parse(args)?;
    let scope: Scope = a.scope.into();
    let folder = store::normalize_folder(a.folder.as_deref().unwrap_or(""))?;
    let new_folders = match (scope, a.repo.as_deref(), session.agent()) {
        (Scope::Repo, Some(repo), _) | (Scope::Agent, _, Some(repo)) => session
            .missing_folders(scope, repo.trim(), &folder)
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let memory = session.add_memory(AddMemory {
        scope,
        repo: a.repo,
        folder,
        title: a.title,
        kind: a.kind,
        tags: a.tags,
        groups: a.groups,
        body: a.body,
        slug: a.slug,
    })?;
    let mut out = json!({ "created": summary(&memory.meta) });
    let tree = session.browse(scope, Some(memory.meta.owner()), "")?;
    let hints = add_hints(&tree, &memory.meta, &new_folders);
    if !hints.is_empty() {
        out["hints"] = json!(hints);
    }
    Ok(out.into())
}

fn browse_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: BrowseArgs = parse(args)?;
    let scope: Scope = a.scope.map(Into::into).unwrap_or(Scope::Repo);
    let tree = session.browse(scope, a.owner.as_deref(), a.folder.as_deref().unwrap_or(""))?;
    let text = render_tree(&tree, a.memories.unwrap_or(true));
    let folders: Vec<Value> = tree
        .folders
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "description": f.description,
                "memory_count": f.memory_count,
                "total_count": f.total_count,
            })
        })
        .collect();
    let memories: Vec<Value> = tree
        .memories
        .iter()
        .map(|m| json!({ "id": m.id, "folder": m.folder(), "title": m.title, "kind": m.kind }))
        .collect();
    Ok(ToolOutput {
        structured: json!({
            "scope": tree.scope,
            "owner": tree.owner,
            "folder": tree.folder,
            "description": tree.owner_description,
            "folders": folders,
            "memories": memories,
        }),
        text: Some(text),
    })
}

fn create_folder(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: CreateFolderArgs = parse(args)?;
    let result = session.create_folder(
        a.scope.into(),
        a.repo.as_deref(),
        &a.folder,
        a.description.as_deref(),
    )?;
    let mut out = json!(result);
    if result.description.is_none() {
        out["hints"] = json!([format!(
            "Folder '{}' has no description. Pass a one-line description so newcomers know what belongs here.",
            result.folder
        )]);
    }
    Ok(out.into())
}

fn move_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: MoveMemoryArgs = parse(args)?;
    let change = session.move_memory(
        &memory_id(&a.id)?,
        &a.folder,
        a.slug.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        a.expected_version,
    )?;
    Ok(json!({
        "moved": summary(&change.after.meta),
        "previous_id": change.before.meta.id,
        "changed_fields": change.changed_fields,
    })
    .into())
}

fn update_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: UpdateMemoryArgs = parse(args)?;
    let change = session.update_memory(
        &memory_id(&a.id)?,
        MemoryPatch {
            title: a.title,
            kind: a.kind,
            tags: a.tags,
            groups: a.groups,
            body: a.body,
            append: a.append,
            expected_version: a.expected_version,
        },
    )?;
    Ok(json!({
        "updated": summary(&change.after.meta),
        "changed_fields": change.changed_fields,
        "previous_version": change.before.meta.version,
    })
    .into())
}

fn delete_memory(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: DeleteMemoryArgs = parse(args)?;
    let memory = session.delete_memory(&memory_id(&a.id)?, a.expected_version)?;
    Ok(json!({ "deleted": summary(&memory.meta) }).into())
}

fn group_memories(session: &mut AgentSession, args: Value) -> ToolResult {
    let a: GroupArgs = parse(args)?;
    let ids = a
        .ids
        .iter()
        .map(|id| memory_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let add = matches!(a.action, GroupAction::Add);
    let changed = session.group(&ids, &a.group, add)?;
    Ok(json!({
        "group": a.group.trim(),
        "action": if add { "add" } else { "remove" },
        "changed": changed,
    })
    .into())
}

// ---- organization guidance -------------------------------------------------------

/// Memories allowed in an owner's root folder before add_memory starts
/// suggesting folders.
const ROOT_SOFT_LIMIT: usize = 3;
/// Memories directly in one subfolder before add_memory suggests splitting it.
const FOLDER_SOFT_LIMIT: usize = 12;

/// Conventional folder for a memory kind, used in hints.
fn folder_for_kind(scope: Scope, kind: &str) -> Option<&'static str> {
    let kind = kind.to_ascii_lowercase();
    Some(match (scope, kind.as_str()) {
        (Scope::Repo, "architecture" | "design") => "architecture",
        (Scope::Repo, "implementation" | "module" | "component") => "modules/<component>",
        (Scope::Repo, "pitfall" | "gotcha" | "incident" | "bug") => "pitfalls",
        (Scope::Repo, "how-to" | "howto" | "guide" | "runbook" | "procedure") => "how-to",
        (Scope::Repo, "decision" | "adr") => "decisions",
        (Scope::Agent, "task-log" | "log") => "task-logs",
        (Scope::Agent, "insight" | "lesson") => "insights",
        (Scope::Agent, "todo" | "plan") => "todo",
        _ => return None,
    })
}

fn top_level_folders(tree: &Browse) -> Vec<String> {
    tree.folders
        .iter()
        .filter(|f| f.depth() == 1)
        .map(|f| format!("{}/", f.path))
        .collect()
}

fn add_hints(tree: &Browse, meta: &MemoryMeta, new_folders: &[String]) -> Vec<String> {
    let mut hints = Vec::new();
    let owner = format!("{}/{}", tree.scope, tree.owner);
    let folder = meta.folder();
    let info = tree.folders.iter().find(|f| f.path == folder);
    let count = info.map_or(0, |f| f.memory_count);
    let is_overview = meta.kind.eq_ignore_ascii_case("overview");
    if folder.is_empty() && !is_overview && count > ROOT_SOFT_LIMIT {
        let suggestion = folder_for_kind(tree.scope, &meta.kind)
            .map(|f| format!(" A memory of kind '{}' usually goes in '{f}/'.", meta.kind))
            .unwrap_or_default();
        let existing = top_level_folders(tree);
        let existing = if existing.is_empty() {
            " There are no folders yet: create a few (see the layout in the server instructions)."
                .to_string()
        } else {
            format!(" Existing folders: {}.", existing.join(", "))
        };
        hints.push(format!(
            "The root of {owner} now holds {count} memories. Keep only the overview there so newcomers can find their way; \
             move this one into a folder with move_memory(id=\"{}\", folder=...).{suggestion}{existing}",
            meta.id
        ));
    } else if folder.is_empty() && !is_overview && tree.scope == Scope::Repo {
        if let Some(f) = folder_for_kind(tree.scope, &meta.kind) {
            hints.push(format!(
                "Tip: memories of kind '{}' are easier to find in '{f}/'. Pass folder=... next time, or move_memory this one.",
                meta.kind
            ));
        }
    }
    if !folder.is_empty() && count > FOLDER_SOFT_LIMIT {
        hints.push(format!(
            "Folder '{folder}' holds {count} memories directly. Consider grouping them into subfolders \
             (create_folder + move_memory)."
        ));
    }
    for f in new_folders {
        hints.push(format!(
            "Created folder '{f}'. Describe it with create_folder(folder=\"{f}\", description=...) so browse_memory explains what belongs here."
        ));
    }
    let has_overview = tree
        .memories
        .iter()
        .any(|m| m.folder().is_empty() && m.kind.eq_ignore_ascii_case("overview"));
    if tree.scope == Scope::Repo && !has_overview {
        hints.push(format!(
            "{owner} has no root overview yet. Add one (kind=overview, no folder) that says what the repo does, \
             how to build and test it, and which folder holds what."
        ));
    }
    hints
}

fn repo_next_step(tree: &Browse) -> String {
    if tree.memories.is_empty() {
        format!(
            "Repo '{}' has no memories yet. Start with a root overview (add_memory kind=overview, no folder), \
             then file knowledge into folders such as architecture/, modules/<name>/, how-to/, pitfalls/ and decisions/ \
             (create_folder with a one-line description).",
            tree.owner
        )
    } else {
        format!(
            "Repo '{}' has {} memories. Call browse_memory(scope=\"repo\", owner=\"{}\") to see the folder tree, \
             read the root overview first, then the folders relevant to your task.",
            tree.owner,
            tree.memories.len(),
            tree.owner
        )
    }
}

/// Indented outline of a memory tree, e.g.
///
/// ```text
/// repo/payment-service: Payment backend (5 memories)
///   overview  Payment service overview [overview]
///   architecture/  How requests flow through the service (2)
///     request-flow  Request flow [architecture]
/// ```
fn render_tree(tree: &Browse, with_memories: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let total = tree.root().map_or(0, |f| f.total_count);
    let _ = write!(out, "{}/{}", tree.scope, tree.owner);
    if !tree.folder.is_empty() {
        let _ = write!(out, "/{}", tree.folder);
    }
    let description = if tree.folder.is_empty() {
        Some(tree.owner_description.as_str()).filter(|d| !d.is_empty())
    } else {
        tree.root().and_then(|f| f.description.as_deref())
    };
    if let Some(d) = description {
        let _ = write!(out, ": {d}");
    }
    let _ = writeln!(
        out,
        " ({total} {})",
        if total == 1 { "memory" } else { "memories" }
    );
    let base = tree.root().map_or(0, |f| f.depth());
    let write_memories = |out: &mut String, path: &str, indent: usize| {
        for m in tree.memories.iter().filter(|m| m.folder() == path) {
            let _ = writeln!(
                out,
                "{:indent$}{}  {} [{}]",
                "",
                m.id.slug,
                m.title,
                m.kind,
                indent = indent
            );
        }
    };
    if with_memories {
        write_memories(&mut out, &tree.folder, 2);
    }
    for f in tree.folders.iter().filter(|f| f.path != tree.folder) {
        let indent = (f.depth() - base) * 2;
        let _ = write!(out, "{:indent$}{}/", "", f.name(), indent = indent);
        if let Some(d) = &f.description {
            let _ = write!(out, "  {d}");
        }
        let _ = writeln!(out, " ({})", f.total_count);
        if with_memories {
            write_memories(&mut out, &f.path, indent + 2);
        }
    }
    if total == 0 {
        out.push_str("  (empty)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_flat_objects() {
        for tool in definitions() {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{}", tool["name"]);
            let text = schema.to_string();
            assert!(!text.contains("\"null\""), "{}: {text}", tool["name"]);
            assert!(!text.contains("$ref"), "{}: {text}", tool["name"]);
        }
        let add = definitions()
            .into_iter()
            .find(|t| t["name"] == "add_memory")
            .unwrap();
        let schema = &add["inputSchema"];
        assert_eq!(
            schema["properties"]["scope"]["enum"],
            json!(["repo", "agent"])
        );
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        required.sort();
        assert_eq!(required, vec!["body", "scope", "title"]);

        let search = definitions()
            .into_iter()
            .find(|t| t["name"] == "search_memory")
            .unwrap();
        assert!(
            search["inputSchema"]["properties"]["owner"].is_object(),
            "flattened filter"
        );
    }
}
