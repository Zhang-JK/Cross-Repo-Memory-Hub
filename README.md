# Cross-Repo Memory Hub 2.0

Shared long-term memory for multiple agents working across multiple repositories, exposed over MCP.

Memory Hub 2.0 (`memhub2`) layers memory so it stays small, current and trustworthy as it grows:
agents write raw observations (**evidence**), an LLM **curator** turns them into compact,
cited **experience** once a day, and search only ever sees experience.

```text
 working agents                      curator (daily, LLM)                 working agents
 record_evidence  ──►  evidence/  ──►  proposals ──► checks ──► review ──►  experience  ◄── search_memory
 (append-only, not searched)           (cite evidence, itemized ops)        (repo/..., agent/...)
```

It runs next to Memory Hub 1.0 without touching it:

| | Memory Hub 1.0 | Memory Hub 2.0 |
| --- | --- | --- |
| Binary | `memhub` | `memhub2` |
| Default bind of `serve` | `127.0.0.1:7878` | `127.0.0.1:7880` |
| Port used by the launch scripts | 8010 | 8020 |
| Data directory | `memhub-data/` | `memhub2-data/` |
| Run directory (pid, log, binary copy) | `.run/` | `.run2/` |
| Scripts | `scripts/restart.sh`, `deploy-prebuilt.sh`, `test-mcp.sh` | `scripts/memhub2-*.sh` |
| Prebuilt release | `nightly` (frozen) | `nightly2` |
| Environment variables | `MEMHUB_*` | `MEMHUB2_*` |
| MCP server name | `memhub` | `memhub2` |

Quick start on a server that already runs 1.0:

```bash
scripts/memhub2-deploy.sh              # download nightly2 and start 2.0 on :8020 (or memhub2-restart.sh to build from source)
scripts/memhub2-curate.sh init         # write config/memhub2-curator.yaml, then fill in the LLM url, model and token
scripts/memhub2-port.sh                # copy 1.0 (:8010) into 2.0 (:8020): dry run, confirm, backup, port
scripts/memhub2-curate.sh              # first curator run (reviews the ported memories)
scripts/memhub2-curate.sh cron         # prints the crontab line for the daily run
scripts/memhub2-test-mcp.sh            # end-to-end self-test of the running 2.0 server
```

Then point agents at `http://<server>:8020/mcp` (see [Connecting an agent](#connecting-an-agent)).

## The layers

**Experience** is what agents search and read. It lives in two kinds of space:

- **Repo memory** (`repo/<repo>/[<folder>/]<slug>`): general knowledge about a repository that
  every agent touching it should know: overview, architecture, modules, how-to, pitfalls,
  decisions.
- **Agent experience** (`agent/<agent>/[<folder>/]<slug>`): lessons about one agent's own way of
  working (tricks, workflows, mistakes to avoid) that are not about one repo.

Every experience memory cites its sources (`cites.evidence`, `cites.code`), counts its
independent `support`, lists the code it is about (`applies_to`) and has a status:

| Status | Meaning |
| --- | --- |
| `established` | Two or more independent sources, or verified against code |
| `provisional` | One source so far, or ported from 1.0 and not reviewed yet |
| `disputed` | An agent reported that it contradicts what it observed; verify before relying on it |
| `superseded` | Replaced (`superseded_by`) or retired; hidden from search and browse by default |

Only curator sessions write experience. When an agent calls `add_memory` for repo memory or its
own experience, the content is recorded as evidence instead and the result says so
(`layer: "evidence"` plus a hint).

**Evidence** is what agents write with `record_evidence`: append-only records of one finding
each (a pitfall and its fix, how something really works, a decision, a procedure that worked or
failed) under `evidence/<yyyy-mm-dd>/<agent>/<hhmmss>-<slug>`. Each record carries the context
the curator lacks: `task`, `outcome`, `reason`, `candidate_lesson`, `code_refs`, `importance`,
the `repos` it is about, and its provenance (`source: observed`, or `derived_from` a memory,
which does not count as independent confirmation). Evidence is not searched by `search_memory`.
It is read by id (`get_memory` on an id cited by a memory) or with `search_evidence` inside a
time window. Records are never edited; a new record with `supersedes=<id>` corrects an old one.

**Work area** (`agent/<agent>/working/...`) is an agent's scratch space for plans, to-dos and task
notes. Only that agent can change it; it is never curated and never appears in other agents'
searches. Agents manage it with `add_memory` / `update_memory` / `move_memory` /
`delete_memory` (`scope: "agent"`, `folder: "working"` or `"working/<task>"`).

### Why this does not drift

The curator does not know the working agent's task, so the design puts the context in the
evidence and keeps the curator's room for invention small:

- Every proposed change is an itemized operation (`add`, `update`, `supersede`, `retire`,
  `move`, `describe_folder`, `noop`) that must cite evidence ids. The server checks every
  operation deterministically: cited evidence exists and at least one cite is a first-hand
  observation; the change stays inside the target's spaces (an agent target may also write
  repos the agent works on) and out of work areas; the version it edits is current;
  `established` needs two independent sources or a verified commit (evidence derived from a
  memory does not count), otherwise it is downgraded to `provisional`; bodies stay within a
  size limit per kind; task-state kinds (to-dos, task logs, status) are refused.
- A second LLM pass reviews each operation against its evidence (`review: llm`), or every
  checked operation is applied (`auto`), or nothing is (`human`). Large rewrites and retiring
  an established memory always wait for a human (`memhub2 proposals approve`).
- Old knowledge is superseded, not overwritten: the replaced memory keeps its text and points
  at its successor, and the audit log has the diff.
- Usage closes the loop: agents `rate_memory` (helpful, harmful, outdated) and `dispute_memory`
  (with what they observed instead). Searches that find nothing are logged. The curator sees
  ratings, disputes and missed searches with its next batch, so it fixes what agents actually
  use and adds what they looked for.

The MCP server instructions (sent at `initialize`) explain all of this to agents in detail:
the layers, how to read statuses, what to record and with which fields, and the work area.

## Layout

| Crate | Layer |
|---|---|
| `memhub-store` | Markdown files with YAML front matter: memories, append-only evidence, folders, search |
| `memhub-audit` | JSONL audit log of connections, evidence, memory changes, searches and curation |
| `memhub-app` | Agents and repos, sessions, write rules, feedback, curation (batches, checks, proposals), 1.0 import |
| `memhub-curator` | Curator client: MCP client, OpenAI-compatible LLM client, config file, curate loop, 1.0 port |
| `memhub-mcp` | MCP tools over Streamable HTTP and stdio, management web UI; `memhub2` CLI |

Data directory:

```text
memhub2-data/
  memories/repo/<repo>/[<folder>/...]<slug>.md     repo memory (experience)
  memories/agent/<agent>/[<folder>/...]<slug>.md   agent experience
  memories/agent/<agent>/working/...               the agent's work area
  memories/<scope>/<owner>/<folder>/_index.md      folder description (plain text)
  evidence/<yyyy-mm-dd>/<agent>/<hhmmss>-<slug>.md evidence (append-only)
  evidence/legacy/...                              original text of every memory ported from 1.0
  curation/state.json                              curator cursor per target
  curation/proposals/<id>.json                     proposals with checks and decisions
  redirects.json                                   old ids (moves, 1.0 ids) -> current ids
  registry/agents/<agent>.yaml, registry/repos/<repo>.yaml
  stats/reads.json, stats/feedback.json            read counts, ratings and disputes
  logs/audit-YYYY-MM-DD.jsonl
  trash/...                                        deleted memories, folders and evidence
```

An experience memory:

```markdown
---
id: repo/payment-service/pitfalls/refund-webhook-delivered-twice
title: Refund webhook can be delivered twice
kind: pitfall
author: memhub-curator
created_at: 2026-11-03T03:43:48Z
updated_at: 2026-11-03T03:57:52Z
version: 2
status: established
cites:
  evidence:
  - evidence/2026-11-02/pay-bot/101500-refund-webhook-retried
  - evidence/2026-11-03/crash-bot/083012-duplicate-refund-crash
  code:
  - src/refunds/webhook.go:88@4f2a91c
support: 2
applies_to:
- src/refunds/
curated_at: 2026-11-03T03:57:52Z
---

The PSP retries the refund webhook when we answer slower than 5 s, so handlers must be idempotent
on `refund_id`.
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
the body. Search matches any Unicode text, including CJK words without spaces and emoji.

## Running

```bash
cargo build --release -p memhub-mcp --bin memhub2

# Streamable HTTP (recommended: one server, many agents, client IPs logged)
MEMHUB2_CURATOR_TOKEN=... memhub2 --data-dir ./memhub2-data serve --bind 127.0.0.1:7880

# stdio (one agent per process; IP logged as "local")
memhub2 --data-dir ./memhub2-data stdio --agent my-agent --role feature-impl --instance alice-laptop
```

Global option: `--data-dir` (`MEMHUB2_DATA_DIR`, default `./memhub2-data`).
`serve` options: `--bind` (`MEMHUB2_BIND`), `--curator-token` (`MEMHUB2_CURATOR_TOKEN`; without
it, curator sessions over HTTP are disabled), `--admin-token` (`MEMHUB2_ADMIN_TOKEN`),
`--idle-timeout-secs` (default 3600), `--resume-ttl-secs` (default 604800, 7 days),
`--trust-proxy` (use `X-Forwarded-For`), `--allowed-origin`.

Other subcommands: `curate`, `port`, `proposals`, `init-config`, `agents`, `logs`, `eval`
(`memhub2 <command> --help`).

### Launch scripts

Both launch scripts run the server in the background on port 8020 (`MEMHUB2_PORT`), bound to
`0.0.0.0` (`MEMHUB2_HOST`), with `.run2/` and `memhub2-data/` next to the scripts' directory.
They share the pid file, so either can stop or replace a server started by the other, and
they only ever stop the process in `.run2/memhub2.pid` after checking that it is a
`memhub2 serve`; a 1.0 server is never touched.

```bash
scripts/memhub2-deploy.sh            # download the nightly2 build; restart only if it changed
MEMHUB2_RELEASE=v2.0.0 scripts/memhub2-deploy.sh
scripts/memhub2-deploy.sh rollback   # back to the previously deployed binary
scripts/memhub2-deploy.sh status | stop

scripts/memhub2-restart.sh           # git pull, cargo build --release, stop the old 2.0 server, start the new one
scripts/memhub2-restart.sh status | stop
```

`memhub2-deploy.sh` needs no Rust toolchain. CI (`.github/workflows/build.yml`) builds static
Linux binaries (x86_64 and aarch64); every push to `main` refreshes the `nightly2` release and
`v*` tags create versioned releases. The 1.0 release `nightly` is no longer updated, so 1.0
servers deployed with `scripts/deploy-prebuilt.sh` stay on 1.0. Downloads honor `https_proxy`;
extra curl flags go in `MEMHUB2_CURL_OPTS`, and `MEMHUB2_BASE_URL` downloads from a mirror. If a
new binary fails to start, the previous one is restored automatically.

On first start both scripts generate a curator token in `.run2/curator-token` (mode 600) and
pass it to the server through the environment. The curate, port and self-test scripts read it
from there. Set `MEMHUB2_CURATOR_TOKEN` to use your own.

### Curator configuration (local LLM)

The curator and the port talk to an OpenAI-compatible chat completions endpoint: Ollama, vLLM,
llama.cpp server, LM Studio or OpenAI itself. Its settings live in
`config/memhub2-curator.yaml`, which is git-ignored because it holds the API key:

```bash
scripts/memhub2-curate.sh init       # copies the commented example (config/memhub2-curator.example.yaml), mode 600
```

| Key | Meaning |
| --- | --- |
| `llm.base_url` | Endpoint without `/chat/completions`, e.g. `http://127.0.0.1:11434/v1` (Ollama) |
| `llm.model` | Model name as the endpoint knows it, e.g. `qwen2.5:32b-instruct` |
| `llm.api_key` | Sent as `Authorization: Bearer <api_key>`; empty if the endpoint needs none |
| `llm.json_mode` | Ask for `response_format: json_object`. If the endpoint rejects it, the curator retries without it and extracts the JSON from the text (also from `<think>` / fenced output) |
| `llm.max_prompt_chars` | Prompt budget; lower it for small context windows |
| `hub.url`, `hub.curator_token` | The 2.0 server; the scripts fill both in |
| `curation.review` | `llm` (second LLM pass, recommended), `auto` or `human` |
| `curation.backfill` | Also review memories ported from 1.0 |
| `port.v1_url` | The 1.0 server, default `http://127.0.0.1:8010/mcp` |

Environment variables win over the file: `MEMHUB2_LLM_BASE_URL`, `MEMHUB2_LLM_MODEL`,
`MEMHUB2_LLM_API_KEY`, `MEMHUB2_URL`, `MEMHUB2_CURATOR_TOKEN`, `MEMHUB1_URL`, and
`MEMHUB2_CURATOR_CONFIG` for the file's path.

### Porting 1.0 memory

```bash
scripts/memhub2-port.sh [OLD_MCP_URL] [NEW_MCP_URL] [--yes] [--only repo:<id>|agent:<name> ...]
# defaults: http://127.0.0.1:8010/mcp (1.0) and http://127.0.0.1:8020/mcp (2.0)
```

The script checks that both servers answer, prints a dry run of where every memory goes, asks
for confirmation, backs up `memhub2-data/` to `.run2/backups/`, then copies agents, repos,
folders and memories server to server over MCP. 1.0 is only read (as `memhub-selftest`, so
its read counts and logs are untouched) and keeps running.

| 1.0 memory | Becomes in 2.0 |
| --- | --- |
| Repo memory | Repo memory, same id, status `provisional`, `origin: v1` |
| Agent memory (insights, tricks, ...) | Agent experience, same id, `provisional` |
| Agent `todo` / `plan` memories | The agent's work area (`working/todo/...`); the old id redirects |
| Agent task logs and status notes | Evidence only; the old id redirects to it |

Every ported memory also keeps its original text as evidence under `evidence/legacy/<1.0 id>`,
which the experience memory cites. Read counts are carried over. Front-matter keys that 2.0
reserves (`status`, `support`, ...) are kept as `v1_<key>`. The port is idempotent: run it again
to bring over memories written in 1.0 since; already ported memories are skipped (also when they
changed in 1.0 since). Reports go to `.run2/reports/`.

The first curator run then reviews the ported memories (`curation.backfill`): it confirms,
rewrites, merges or retires them, and memories that are really task logs are flagged for
reclassification. Preview it with `scripts/memhub2-curate.sh --backfill-only --dry-run`.

### Curating

```bash
scripts/memhub2-curate.sh                    # one run over every repo and agent with new evidence
scripts/memhub2-curate.sh --dry-run          # draft and check one batch per target, store nothing
scripts/memhub2-curate.sh --target repo:shop # one target (repeatable)
scripts/memhub2-curate.sh cron               # crontab line for a daily run at 02:30
scripts/memhub2-curate.sh proposals list     # changes waiting for a human
scripts/memhub2-curate.sh proposals show <id>
scripts/memhub2-curate.sh proposals approve <id> [--op 0 --op 2]
scripts/memhub2-curate.sh proposals reject <id> --reason "..."
```

A run processes each target (`repo:<id>`, `agent:<name>`) from where the last run stopped:
it reads a batch of new evidence with the related experience, ratings, disputes and missed
searches; asks the LLM for operations; submits them as a proposal (checked by the server);
has them reviewed; and moves the cursor. Runs never overlap (`flock`), write a JSON report to
`.run2/reports/` and append to `.run2/curate.log`. Evidence of `memhub-selftest` is never
curated.

### Testing a running server

```bash
scripts/memhub2-test-mcp.sh                            # URL saved in .run2/mcp_url by the launch scripts
scripts/memhub2-test-mcp.sh http://10.0.0.5:8020/mcp   # or MEMHUB2_MCP_URL=...; prompts if neither is set
scripts/memhub2-test-mcp.sh -v                         # print every request and response
```

It runs a full MCP session over HTTP as agent `memhub-selftest` in repo `memhub-selftest`:
evidence (record, read, `search_evidence`, not found by `search_memory`, `add_memory` into a
repo becomes evidence), the write rules (experience is read-only, another agent's work area is
read-only), the work area (add, versioned update, search, folders, move and redirect), and, when
the curator token is available, the curator gate. It deletes what it created and exits non-zero
on any failure. Proxies are bypassed unless `MEMHUB2_TEST_PROXY=1`. The management page omits
this agent from the connection log, the update log, the activity counts, and the Excel export.

**Idle timeout and session resume.** The agent identity belongs to the MCP session. When a
session closes on idle timeout (or on a graceful server shutdown, SIGINT or SIGTERM), the hub
remembers its id, agent, client and instance label for `--resume-ttl-secs`. If the client sends
that `Mcp-Session-Id` again, the session reopens under the same id with the same identity,
so the request succeeds and writes don't fail with `not_identified`. The audit log records a
`connect` with `resumed: true` and, for identified sessions, an `agent_login`. The Connections
page shows the reopened stretch as a separate row marked **resumed** that doesn't add to the
connection or instance counts. Sessions open at shutdown are saved to
`<data-dir>/run/resumable-sessions.json`, which is read once at the next start and then
deleted. After a crash or `kill -9` nothing is saved, so clients re-initialize. A session ended
with `DELETE /mcp` is never resumed. A session that never identified itself can be identified
on resume from the `X-Memhub-Agent` header (and `X-Memhub-Role` for a new agent), so
configuring the header remains the most robust option. A resumed session is a curator session
only if the resuming request carries a valid curator token.

### Management web UI

`serve` also hosts a management page at `http://<host>:<port>/admin` (`/` redirects there):

- **Dashboard**: memory, folder and agent counts, live MCP sessions, 14-day activity chart,
  recent changes, most active agents and largest folders, with links into the other tabs.
- **Memories**: folder tree (repos and agents, each with nested subfolders and their
  descriptions; folders without a description are flagged), keyword search, every memory's
  front-matter properties and content (rendered or raw file). Add memories into any folder
  (typed in or imported from a `.md` file), move or delete memories, and create, describe or
  delete folders. Each memory shows its read count. **Ranking** ranks memories and folders by
  agent reads, see [Read counts](#read-counts).
- **Connections**: live instances and connections, plus rolling 7-day and 30-day counts
  (instances, distinct agents, distinct IPs, connections; see
  [Counting instances](#counting-instances)), then one row per MCP session
  (agent, instance, client IP, client, duration, disconnect reason, live or closed), or the
  raw connect/identify/disconnect events. **Export Excel** downloads sessions from
  the last 365 days as `.xlsx` (the agent and IP filters apply; at most 100,000
  rows, newest first).
- **Updates**: every memory change (add, update, move, delete, grouping) and folder change
  with agent, IP, versions and body diff; filter by agent, action, or memory id / title /
  folder.

Changes made in the UI skip the per-agent write rules and are audited as agent `(admin)`,
transport `admin`, with the browser's IP. Deleted memories and folders go to `trash/`.

Without a token the page is open to anyone who can reach the server, the same trust model
as the MCP endpoint. To require a token, export it before starting the server; the launch
scripts pass the environment through:

```bash
MEMHUB2_ADMIN_TOKEN='long-random-string' scripts/memhub2-restart.sh   # or memhub2-deploy.sh
```

#### Read counts

Every memory has a read count: each successful `get_memory` by an agent counts once. Listing,
browsing and searching don't count, and neither do the curator, the management UI or the
`memhub-selftest` probe. Counts are kept in `stats/reads.json`, not in the memory file, so
reading never changes a memory's version or `updated_at`. A memory moved by an agent or the
curator keeps its count (moves applied with `memhub2 proposals approve` while the server runs
do not carry it over); a deleted one loses it. Counts ported from 1.0 are added in. Per memory
the hub stores the total, the last read time and reader, reads per agent, and daily counts for
the last 30 days. `get_memory` returns the total as `read_count`.

The **Ranking** view (Memories, **Ranking** toggle) ranks by reads in a window (all time, the
last 30 days or the last 7 days) for all memories or one repo, agent or subfolder, and counts
the memories never read in the window, useful when pruning. The API is
`GET /api/admin/reads?scope=&owner=&folder=&days=&limit=`.

Counts are updated in memory immediately and written to disk at most every 5 seconds when
there are new reads, periodically by the server, and at shutdown. A crash can lose the last few
seconds.

Admin commands:

```bash
memhub2 agents                               # list agents
memhub2 logs --tail 100 --diff               # human-readable audit log with body diffs
memhub2 logs --event memory_searched --json  # e.g. what agents searched for
```

## Connecting an agent

Agents are identified by **name only** (no token). Every user who runs the same agent uses
the same name and therefore shares that agent's experience and work area; the audit log still
tells the users apart by IP and session.

HTTP client configuration, e.g. a project's `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "memhub2": {
      "url": "http://127.0.0.1:8020/mcp",
      "headers": {
        "X-Memhub-Agent": "crash-bot",
        "X-Memhub-Role": "crash-analysis",
        "X-Memhub-Instance": "${env:USER}-${workspaceFolderBasename}"
      }
    }
  }
}
```

The headers are the same as for 1.0, so moving an agent to 2.0 means changing the URL (and
the server name). The same file can be committed to every repository unchanged; see
[Counting instances](#counting-instances) for what the `X-Memhub-Instance` value does.

The headers are optional. Without them, the agent calls `connect_agent` with its name.
If the agent does not exist yet, a `role` (header or tool argument) is required to create it;
for an existing agent the role is ignored.

The curator connects with the extra header `X-Memhub-Curator-Token: <token>` (stdio:
`--curator`). A wrong token is refused with 403. Only curator sessions see the curation tools
and the curator part of the server instructions.

### Counting instances

`X-Memhub-Instance` (stdio: `--instance` / `MEMHUB2_INSTANCE`) labels one running copy of an
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
as one. A label that still contains `${` (a client that doesn't interpolate, or a variable
left unresolved) is ignored and the connection counts as unlabeled. Clients without variable
support need a fixed value per copy, such as `alice-laptop`.

The Connections page counts **instances** as distinct pairs of agent name and instance
label. Reconnects (for example after an idle timeout) don't add to the count, and two people
running the same agent behind one proxy IP count as two. Connections without the header
can't be told apart, so they are grouped by agent and IP and counted as **unlabeled**
instances.

Because names are not secret, the "work areas are read-only for other agents" rule protects
against honest mistakes, not against an agent deliberately using another agent's name. The
curator token is the only secret: it guards who can write experience.

## MCP tools

Agent tools:

| Tool | Purpose |
|---|---|
| `connect_agent` | Identify by agent name; creates the agent when a role is given and it is new |
| `whoami`, `list_agents` | Identity and agent directory |
| `add_repo`, `leave_repo`, `list_repos` | Manage working repos (registers a repo on first use) |
| `browse_memory` | Folder tree of a repo or agent with descriptions, counts, titles and statuses (superseded memories hidden unless `include_superseded`) |
| `search_memory` | Ranked search over experience (repo memory, your own space, and another agent's experience with `owner`); never evidence |
| `list_memories` | Filtered listing, optionally `group_by` group/kind/tag/owner/scope/author/folder |
| `get_memory` | Full Markdown of a memory or an evidence record (counts as one read; follows redirects) |
| `record_evidence` | Write an observation: the only way agents add knowledge |
| `search_evidence` | Search raw evidence in a time window, by agent, repo or text |
| `rate_memory` | Rate a memory you used: helpful, harmful or outdated |
| `dispute_memory` | Report that a memory contradicts what you observed, with code refs |
| `add_memory`, `update_memory`, `delete_memory` | Notes in your work area (`expected_version` for optimistic locking); elsewhere `add_memory` records evidence |
| `create_folder`, `move_memory` | Organize your work area |
| `group_memories` | Add memories to / remove them from a named group |

Curator tools (curator sessions only): `curation_status`, `curation_batch`,
`curation_backfill`, `propose_changes` (with `dry_run`), `review_changes`, `list_proposals`,
`get_proposal`, `import_registry`, `import_folders`, `import_memory`, `delete_evidence`; the
curator can also use `add_memory`, `update_memory`, `move_memory`, `create_folder` and
`delete_memory` on experience directly.

## Audit log

Every record carries `ts`, `event`, `session_id`, `agent` (once identified), `ip` and `transport`.
Events: `connect` (with MCP client name, `instance` label, and `resumed: true` when an expired
session was reopened), `disconnect` (`client_closed`, `idle_timeout`, `stdio_eof`,
`server_shutdown`, `connect_failed`, ...), `agent_registered`, `agent_login`, `repo_joined`,
`repo_left`, `evidence_recorded`, `memory_changed` with `action`
(`add`/`update`/`move`/`delete`/`group_add`/`group_remove`), versions, changed fields, a unified
diff of the body and, for moves, `previous_id`; `memory_searched` (query, filters, hit count);
`memory_rated`, `memory_disputed`; curation events; and `folder_created`, `folder_described`,
`folder_deleted` with `scope`, `owner` and `folder`.
