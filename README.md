# Cross-Repo Memory Hub

Shared long-term memory for multiple agents working across multiple repositories, exposed over MCP.

Two memory spaces:

- **Repo memory** (`repo/<repo>/<slug>`): what a repository does, how it is implemented, what to watch out for when changing it. Writable by every agent that has joined the repo.
- **Agent memory** (`agent/<agent>/<slug>`): each agent's own notebook (task logs, insights). Readable by everyone, writable only by its owner.

## Layout

| Crate | Layer |
|---|---|
| `memhub-store` | Memory management: Markdown files with YAML front matter; add, get, update, delete, search, group |
| `memhub-audit` | JSONL audit log of connections, disconnections and memory changes |
| `memhub-app` | Agent registration and login, working repos, permission checks, sessions |
| `memhub-mcp` | MCP tools over Streamable HTTP and stdio; `memhub` CLI |

Data directory:

```text
memhub-data/
  memories/repo/<repo>/<slug>.md
  memories/agent/<agent>/<slug>.md
  trash/...                          deleted memories
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

Global option: `--data-dir` (`MEMHUB_DATA_DIR`).
`serve` options: `--idle-timeout-secs`, `--trust-proxy` (use `X-Forwarded-For`), `--allowed-origin`.

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
| `search_memory` | Ranked keyword search with scope/owner/kind/tag/group/author filters |
| `list_memories` | Filtered listing, optionally `group_by` group/kind/tag/owner/scope/author |
| `get_memory` | Full Markdown content |
| `add_memory`, `update_memory`, `delete_memory` | Mutations (`expected_version` for optimistic locking) |
| `group_memories` | Add memories to / remove them from a named group |

## Audit log

Every record carries `ts`, `event`, `session_id`, `agent` (once identified), `ip` and `transport`.
Events: `connect` (with MCP client name), `disconnect` (`client_closed`, `idle_timeout`,
`stdio_eof`, `server_shutdown`, `connect_failed`, ...), `agent_registered` (first connection
of a new agent), `agent_login`, `repo_joined`, `repo_left`, and `memory_changed` with
`action` (`add`/`update`/`delete`/`group_add`/`group_remove`), versions, changed fields and a
unified diff of the body.
