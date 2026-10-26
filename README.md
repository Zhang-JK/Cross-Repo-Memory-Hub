# Cross-Repo Memory Hub

Shared long-term memory for multiple agents working across multiple repositories, exposed over MCP.

Two memory spaces:

- **Repo memory** (`repo/<repo>/[<folder>/]<slug>`) is the repository's shared knowledge base.
  It holds general knowledge about the repo that every agent should know, whoever is working on
  it and for whatever task: what it does, how to build, test and release it, its architecture and
  modules, conventions, pitfalls and design decisions. Every agent that has joined the repo can
  read and write it, so it is written for a newcomer.
- **Agent memory** (`agent/<agent>/[<folder>/]<slug>`) is one agent's work notebook for specific
  tasks: what it was asked to do, what it tried, progress, open questions and to-dos, plus the
  personal tricks that make it faster. Anyone can read it; only its owner can write it.

Which one to write (agents get the same rules in the MCP server instructions):

| Ask yourself | Goes in | Example |
| --- | --- | --- |
| Should another agent on a different task in this repo know it? | repo memory | "The build needs protoc 3.21" goes in `how-to/`; "retries double-charge without an idempotency key" goes in `pitfalls/` |
| Does it only make sense in the context of my task? | agent memory | "PAY-123: fixed refund rounding, tried X first, PR pending" goes in `task-logs/` |
| Is it a personal working trick? | agent memory | "`go test -run Billing` saves 5 minutes" goes in `insights/` |

When a task teaches an agent something general, the agent records it in repo memory, not only in
its task log, and fixes outdated repo memory with `update_memory`. Task logs, status updates and
to-dos never go in repo memory. `add_memory` returns a hint when a memory's kind looks like it
belongs in the other space, for example a `task-log` filed as repo memory or a `pitfall` filed as
agent memory.

Each space is a folder tree (up to 6 levels), so a newcomer can learn a repo from its outline:

```text
repo/payment-service: Payment processing API (Go) (9 memories)
  service-overview  Service overview [overview]
  architecture/  Components, request flow and data model (2)
    request-flow  Request flow [architecture]
    ledger-data-model  Ledger data model [architecture]
  modules/  One folder per subsystem (3)
    card-auth/  Card authorization, 3-D Secure and issuer quirks (2)
    refunds/  Full and partial refunds, reversal windows (1)
  how-to/  Step-by-step procedures (1)
  pitfalls/  Things that broke in production (1)
  decisions/  Why things are the way they are (1)
```

That is the output of the `browse_memory` tool. The MCP server instructions tell agents to
keep only the `overview` in a repo's root, use `architecture/`, `modules/<name>/`, `how-to/`,
`pitfalls/` and `decisions/` (agents: `task-logs/`, `insights/`, `todo/`), browse before adding,
describe every folder, and tidy up with `move_memory`. Tool results carry `hints` when an agent
drifts: a crowded root (more than 3 memories besides the overview), a folder with more than 12
memories, a new folder without a description, or a repo without a root overview. `add_repo`
returns a `next_step` that points newcomers at `browse_memory`, or at a starter layout for an
empty repo.

## Layout

| Crate | Layer |
|---|---|
| `memhub-store` | Memory management: Markdown files with YAML front matter; add, get, update, delete, search, group |
| `memhub-audit` | JSONL audit log of connections, disconnections and memory changes |
| `memhub-app` | Agent registration and login, working repos, permission checks, sessions, admin operations |
| `memhub-mcp` | MCP tools over Streamable HTTP and stdio, management web UI; `memhub` CLI |

Data directory:

```text
memhub-data/
  memories/repo/<repo>/[<folder>/...]<slug>.md
  memories/agent/<agent>/[<folder>/...]<slug>.md
  memories/<scope>/<owner>/<folder>/_index.md   folder description (plain text)
  trash/...                          deleted memories and folders
  registry/agents/<agent>.yaml       role, description, working repos
  registry/repos/<repo>.yaml
  logs/audit-YYYY-MM-DD.jsonl
```

A memory file:

```markdown
---
id: repo/billing/overview
title: Overview
kind: overview
tags:
- payments
groups:
- onboarding
author: feat-bot
updated_by: perf-bot
created_at: 2026-10-22T08:13:37Z
updated_at: 2026-10-22T09:01:12Z
version: 2
---

Billing computes invoices nightly.
```

Files can also be edited by hand; unknown front-matter keys are preserved.

Text rules (files are UTF-8):

| Field | Allowed |
| --- | --- |
| Body, tags, groups, kind, folder and repo descriptions, agent role | Any Unicode: Chinese, Japanese, Korean, symbols, emoji, ... |
| Title | Printable ASCII (English letters, digits, spaces, punctuation) |
| Repo id, agent name, folder names, slug (file name) | ASCII letters, digits, `-`, `_`, `.` |

Titles stay ASCII so file names derived from them are meaningful and every tool can display
them. A non-ASCII title is rejected with an error that asks for an English title and points to
the body. Existing files with non-ASCII titles still load and can be edited; only setting a new
title is checked. Search matches any Unicode text, including CJK words without spaces and emoji.

## Running

```bash
cargo build --release

# Streamable HTTP (recommended: one server, many agents, client IPs logged)
memhub --data-dir ./memhub-data serve --bind 127.0.0.1:7878

# stdio (one agent per process; IP logged as "local")
memhub --data-dir ./memhub-data stdio --agent my-agent --role feature-impl --instance alice-laptop
```

### Deploying without a Rust toolchain

CI (`.github/workflows/build.yml`) builds fully static Linux binaries (x86_64 and aarch64).
Every push to `main` refreshes the `nightly` release; `v*` tags create versioned releases.
On the server:

```bash
scripts/deploy-prebuilt.sh            # download nightly, restart only if it changed
MEMHUB_RELEASE=v0.1.0 scripts/deploy-prebuilt.sh
scripts/deploy-prebuilt.sh rollback   # back to the previously deployed binary
scripts/deploy-prebuilt.sh status | stop
```

Downloads honor `https_proxy`; extra curl flags go in `MEMHUB_CURL_OPTS`. If a new binary
fails to start, the previous one is restored automatically. `scripts/restart.sh` is the
build-from-source equivalent; both share `.run/`, so either can replace the other's server.

### Testing a running server

```bash
scripts/test-mcp.sh                            # URL saved in .run/mcp_url by the launch scripts
scripts/test-mcp.sh http://10.0.0.5:8010/mcp   # or MEMHUB_MCP_URL=...; prompts if neither is set
scripts/test-mcp.sh -v                         # print every request and response
```

It runs a full MCP session over HTTP (initialize, every tool, permission and version-conflict
checks, session close) as agent `memhub-selftest` in repo `memhub-selftest`, deletes the
memories it created, and exits non-zero on any failure. It is not a background watchdog:
nothing runs it unless you do. Proxies are bypassed unless `MEMHUB_TEST_PROXY=1`.
The management page omits this agent from the connection log, the update log, the
activity counts, and the Excel export.

Global option: `--data-dir` (`MEMHUB_DATA_DIR`).
`serve` options: `--idle-timeout-secs` (default 3600: a session with no request for an hour is
closed), `--resume-ttl-secs` (default 604800, 7 days; see below), `--trust-proxy` (use
`X-Forwarded-For`), `--allowed-origin`, `--admin-token` (`MEMHUB_ADMIN_TOKEN`).

**Idle timeout and session resume.** The agent identity belongs to the MCP session. When a
session closes on idle timeout (or on a graceful server shutdown, SIGINT or SIGTERM), the hub
remembers its id, agent, client and instance label for `--resume-ttl-secs`. If the client sends
that `Mcp-Session-Id` again, the session reopens under the same id with the same identity,
so the request succeeds and writes don't fail with `not_identified`. Without this, the client
got a 404, started a fresh session, and stayed unidentified until it called `connect_agent`
again. The audit log records a `connect` with `resumed: true` and, for identified sessions,
an `agent_login`. The Connections page shows the reopened stretch as a separate row marked
**resumed** that doesn't add to the connection or instance counts. Sessions open at shutdown are
saved to `<data-dir>/run/resumable-sessions.json`, which is read once at the next start and then
deleted. After a crash or `kill -9` nothing is saved, so clients re-initialize as before. A
session ended with `DELETE /mcp` is never resumed. A session that never identified itself can
be identified on resume from the `X-Memhub-Agent` header (and `X-Memhub-Role` for a new agent),
so configuring the header remains the most robust option.

### Management web UI

`serve` also hosts a management page at `http://<host>:<port>/admin` (`/` redirects there):

- **Dashboard**: memory, folder and agent counts, live MCP sessions, 14-day activity chart,
  recent changes, most active agents and largest folders, with links into the other tabs.
- **Memories**: folder tree (repos and agents, each with nested subfolders and their
  descriptions; folders without a description are flagged), keyword search, every memory's
  front-matter properties and content (rendered or raw file). Add memories into any folder
  (typed in or imported from a `.md` file), move or delete memories, and create, describe or
  delete folders.
- **Connections**: live instances and connections, plus rolling 7-day and 30-day counts
  (instances, distinct agents, distinct IPs, connections; see
  [Counting instances](#counting-instances)), then one row per MCP session
  (agent, instance, client IP, client, duration, disconnect reason, live or closed), or the
  raw connect/identify/disconnect events. **Export Excel** downloads sessions from
  the last 365 days as `.xlsx` (the agent and IP filters apply; at most 100,000
  rows, newest first). A connection counts in a window when it started, identified
  itself, or ended in that window.
- **Updates**: every memory change (add, update, move, delete, grouping) and folder change
  with agent, IP, versions and body diff; filter by agent, action, or memory id / title /
  folder.

Changes made in the UI skip the per-agent write rules and are audited as agent `(admin)`,
transport `admin`, with the browser's IP. Deleted memories and folders go to `trash/`.

Without a token the page is open to anyone who can reach the server, the same trust model
as the MCP endpoint. To require a token, export it before starting the server; the launch
scripts pass the environment through:

```bash
MEMHUB_ADMIN_TOKEN='long-random-string' scripts/restart.sh   # or deploy-prebuilt.sh
```

Admin commands:

```bash
memhub agents                         # list agents
memhub logs --tail 100 --diff         # human-readable audit log with body diffs
memhub logs --agent perf-bot --json   # raw JSON lines
```

## Connecting an agent

Agents are identified by **name only** (no token). Every user who runs the same agent uses
the same name and therefore shares that agent's memory space; the audit log still tells the
users apart by IP and session.

HTTP client configuration, e.g. a project's `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "memhub": {
      "url": "http://127.0.0.1:7878/mcp",
      "headers": {
        "X-Memhub-Agent": "crash-bot",
        "X-Memhub-Role": "crash-analysis",
        "X-Memhub-Instance": "${env:USER}-${workspaceFolderBasename}"
      }
    }
  }
}
```

The same file can be committed to every repository unchanged; see
[Counting instances](#counting-instances) for what the `X-Memhub-Instance` value does.

The headers are optional. Without them, the agent calls `connect_agent` with its name.
If the agent does not exist yet, a `role` (header or tool argument) is required to create it;
for an existing agent the role is ignored.

### Counting instances

`X-Memhub-Instance` (stdio: `--instance` / `MEMHUB_INSTANCE`) labels one running copy of an
agent. It is free text (at most 128 characters, trimmed), recorded on the `connect` audit
event, and identifies nothing beyond the label. Memory is still shared by agent name.

The label should differ between copies that run at the same time and stay the same across
restarts. Cursor fills in variables in `mcp.json` header values
([config interpolation](https://cursor.com/docs/context/mcp)), so the label can be built
from them rather than typed by each person:

| Value | One instance per | Notes |
| --- | --- | --- |
| `${env:USER}-${workspaceFolderBasename}` | user and project | Recommended, in each project's `.cursor/mcp.json`. Two windows on two projects count as two. |
| `${env:USER}-${env:HOSTNAME}-${workspaceFolderBasename}` | user, machine and project | Only if `HOSTNAME` is exported to GUI apps (often it isn't). |
| `${env:USERNAME}-${workspaceFolderBasename}` | user and project | Windows, where the user variable is `USERNAME`. |
| `${env:USER}` | user | Global `~/.cursor/mcp.json`, where `${workspaceFolder*}` is not defined. All of a user's windows count as one. |

Two windows open on the same project by the same user still send the same label and count
as one. After connecting, check the **Instance** column on the Connections page to confirm
the label resolved. A label that still contains `${` (a client that doesn't interpolate, or
a variable left unresolved) is ignored and the connection counts as unlabeled, rather than every
copy collapsing into one instance under the same literal text. Clients without variable
support need a fixed value per copy, such as `alice-laptop`.

The Connections page counts **instances** as distinct pairs of agent name and instance
label. Reconnects (for example after an idle timeout) don't add to the count, and two people
running the same agent behind one proxy IP count as two. Connections without the header
can't be told apart, so they are grouped by agent and IP and counted as **unlabeled**
instances. That part is a lower bound, and the count is exact once every client sends the
header.

Because names are not secret, the "agent memory is read-only for other agents" rule protects
against honest mistakes, not against an agent deliberately using another agent's name.

## MCP tools

| Tool | Purpose |
|---|---|
| `connect_agent` | Identify by agent name; creates the agent when a role is given and it is new |
| `whoami`, `list_agents` | Identity and agent directory |
| `add_repo`, `leave_repo`, `list_repos` | Manage working repos (registers a repo on first use) |
| `browse_memory` | Folder tree of a repo or agent with descriptions, counts and titles (the newcomer's entry point) |
| `search_memory` | Ranked keyword search with scope/owner/folder/kind/tag/group/author filters |
| `list_memories` | Filtered listing, optionally `group_by` group/kind/tag/owner/scope/author/folder |
| `get_memory` | Full Markdown content |
| `add_memory`, `update_memory`, `delete_memory` | Mutations (`folder` on add; `expected_version` for optimistic locking) |
| `create_folder` | Create a folder with a one-line description, or change a folder's description |
| `move_memory` | Move a memory to another folder of the same repo / agent, optionally renaming it |
| `group_memories` | Add memories to / remove them from a named group |

## Audit log

Every record carries `ts`, `event`, `session_id`, `agent` (once identified), `ip` and `transport`.
Events: `connect` (with MCP client name, `instance` label, and `resumed: true` when an expired session was reopened), `disconnect` (`client_closed`, `idle_timeout`,
`stdio_eof`, `server_shutdown`, `connect_failed`, ...), `agent_registered` (first connection
of a new agent), `agent_login`, `repo_joined`, `repo_left`, `memory_changed` with
`action` (`add`/`update`/`move`/`delete`/`group_add`/`group_remove`), versions, changed fields,
a unified diff of the body and, for moves, `previous_id`, and `folder_created`,
`folder_described`, `folder_deleted` with `scope`, `owner` and `folder` (empty for a whole
repo / agent space).
