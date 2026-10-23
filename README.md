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

## Running

```bash
cargo build --release

# Streamable HTTP (recommended: one server, many agents, client IPs logged)
memhub --data-dir ./memhub-data serve --bind 127.0.0.1:7878

# stdio (one agent per process; IP logged as "local")
memhub --data-dir ./memhub-data stdio --agent my-agent --role feature-impl
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
`serve` options: `--idle-timeout-secs`, `--trust-proxy` (use `X-Forwarded-For`), `--allowed-origin`,
`--admin-token` (`MEMHUB_ADMIN_TOKEN`).

### Management web UI

`serve` also hosts a management page at `http://<host>:<port>/admin` (`/` redirects there):

- **Dashboard**: memory, folder and agent counts, live MCP sessions, 14-day activity chart,
  recent changes, most active agents and largest folders, with links into the other tabs.
- **Memories**: folder tree (repos and agents, each with nested subfolders and their
  descriptions; folders without a description are flagged), keyword search, every memory's
  front-matter properties and content (rendered or raw file). Add memories into any folder
  (typed in or imported from a `.md` file), move or delete memories, and create, describe or
  delete folders.
- **Connections**: live connection count plus rolling 7-day and 30-day counts
  (connections, distinct IPs, distinct agents), then one row per MCP session
  (agent, client IP, client, duration, disconnect reason, live or closed), or the
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

HTTP client configuration (e.g. Cursor `mcp.json`):

```json
{
  "mcpServers": {
    "memhub": {
      "url": "http://127.0.0.1:7878/mcp",
      "headers": { "X-Memhub-Agent": "crash-bot", "X-Memhub-Role": "crash-analysis" }
    }
  }
}
```

The headers are optional. Without them, the agent calls `connect_agent` with its name.
If the agent does not exist yet, a `role` (header or tool argument) is required to create it;
for an existing agent the role is ignored.

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
Events: `connect` (with MCP client name), `disconnect` (`client_closed`, `idle_timeout`,
`stdio_eof`, `server_shutdown`, `connect_failed`, ...), `agent_registered` (first connection
of a new agent), `agent_login`, `repo_joined`, `repo_left`, `memory_changed` with
`action` (`add`/`update`/`move`/`delete`/`group_add`/`group_remove`), versions, changed fields,
a unified diff of the body and, for moves, `previous_id`, and `folder_created`,
`folder_described`, `folder_deleted` with `scope`, `owner` and `folder` (empty for a whole
repo / agent space).
