#!/usr/bin/env bash
# End-to-end check of a running memhub MCP server: speaks MCP over HTTP with
# curl and exercises every tool. Needs only bash, curl and grep.
#
#   scripts/test-mcp.sh [MCP_URL] [-v]
#
# MCP URL, first match wins:
#   1. the MCP_URL argument, e.g. http://10.0.0.5:8010/mcp
#   2. $MEMHUB_MCP_URL
#   3. <run dir>/mcp_url, written by scripts/restart.sh / scripts/deploy-prebuilt.sh
#   4. interactive prompt (default http://127.0.0.1:${MEMHUB_PORT:-8010}/mcp)
#
# The test connects as agent "memhub-selftest" and joins repo "memhub-selftest"
# (both stay registered afterwards). Memories it creates are deleted at the end.
#
# Environment overrides:
#   MEMHUB_TEST_AGENT    (default memhub-selftest)
#   MEMHUB_TEST_REPO     (default memhub-selftest)
#   MEMHUB_RUN_DIR       (default <base>/.run)
#   MEMHUB_TEST_PROXY=1  send requests through http(s)_proxy (default: bypass proxies)
#   MEMHUB_TLS_VERIFY=1  verify TLS certificates for https URLs (default: curl -k)

set -uo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${MEMHUB_RUN_DIR:-$BASE_DIR/.run}"
AGENT="${MEMHUB_TEST_AGENT:-memhub-selftest}"
REPO="${MEMHUB_TEST_REPO:-memhub-selftest}"

VERBOSE=0
URL=""
for arg in "$@"; do
    case "$arg" in
        -v | --verbose) VERBOSE=1 ;;
        -h | --help) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) URL="$arg" ;;
    esac
done

if [[ -z "$URL" && -n "${MEMHUB_MCP_URL:-}" ]]; then
    URL="$MEMHUB_MCP_URL"
fi
if [[ -z "$URL" && -s "$RUN_DIR/mcp_url" ]]; then
    URL="$(head -n 1 "$RUN_DIR/mcp_url")"
    echo "Using MCP URL saved by the launch script ($RUN_DIR/mcp_url)"
fi
if [[ -z "$URL" ]]; then
    default="http://127.0.0.1:${MEMHUB_PORT:-8010}/mcp"
    if [[ -t 0 ]]; then
        read -r -p "MCP server URL [$default]: " URL
    fi
    URL="${URL:-$default}"
fi
[[ "$URL" == *://* ]] || URL="http://$URL"
URL="${URL%/}"
[[ "$URL" == */mcp ]] || URL="$URL/mcp"
HEALTH_URL="${URL%/mcp}/health"

CURL=(curl -sS --connect-timeout 5 --max-time 20)
[[ "${MEMHUB_TEST_PROXY:-0}" == "1" ]] || CURL+=(--noproxy '*')
[[ "${MEMHUB_TLS_VERIFY:-0}" == "1" ]] || CURL+=(-k)

if [[ -t 1 ]]; then
    GREEN=$'\033[32m' RED=$'\033[31m' BOLD=$'\033[1m' RESET=$'\033[0m'
else
    GREEN="" RED="" BOLD="" RESET=""
fi

PASSED=0
FAILED=0
SID=""
RPC_ID=0
CODE=""
BODY=""
HEADERS=""

pass() { PASSED=$((PASSED + 1)); printf '  %sPASS%s  %s\n' "$GREEN" "$RESET" "$1"; }
fail() {
    FAILED=$((FAILED + 1))
    printf '  %sFAIL%s  %s\n' "$RED" "$RESET" "$1"
    printf '        HTTP %s: %s\n' "$CODE" "$(head -c 400 <<<"$BODY")"
}
check() {
    local desc=$1
    shift
    if "$@"; then pass "$desc"; else fail "$desc"; fi
}
has() { grep -qF -- "$1" <<<"$BODY"; }
lacks() { ! has "$1"; }
code_is() { [[ "$CODE" == "$1" ]]; }
ok_tool() { code_is 200 && has '"isError":false'; }
err_tool() { code_is 200 && has '"isError":true' && has "$1"; }
section() { printf '\n%s%s%s\n' "$BOLD" "$1" "$RESET"; }

# request METHOD URL [JSON] -> sets CODE, BODY, HEADERS
request() {
    local method=$1 url=$2 data=${3:-} hdr out
    hdr="$(mktemp)"
    local args=(-X "$method" -D "$hdr" -w $'\n%{http_code}'
        -H 'content-type: application/json'
        -H 'accept: application/json, text/event-stream')
    [[ -n "$SID" ]] && args+=(-H "mcp-session-id: $SID")
    [[ -n "$data" ]] && args+=(--data-binary "$data")
    out="$("${CURL[@]}" "${args[@]}" "$url" 2>&1)" || true
    CODE="${out##*$'\n'}"
    BODY="${out%$'\n'*}"
    [[ "$BODY" == "$CODE" ]] && BODY=""
    HEADERS="$(cat "$hdr")"
    rm -f "$hdr"
    if [[ "$VERBOSE" == 1 ]]; then
        printf '        > %s %s\n' "$method" "${data:0:300}"
        printf '        < %s %s\n' "$CODE" "${BODY:0:600}"
    fi
}

rpc() { # rpc METHOD [PARAMS_JSON]
    RPC_ID=$((RPC_ID + 1))
    request POST "$URL" "{\"jsonrpc\":\"2.0\",\"id\":$RPC_ID,\"method\":\"$1\",\"params\":${2:-{\}}}"
}

tool() { # tool NAME [ARGS_JSON]
    rpc tools/call "{\"name\":\"$1\",\"arguments\":${2:-{\}}}"
}

# First memory id of the form <scope>/<owner>/<slug> in the last response.
memory_id() {
    grep -oE "\"id\":\"$1/[^\"]+\"" <<<"$BODY" | head -n 1 | sed 's/^"id":"//; s/"$//'
}

summary() {
    printf '\n%s%d passed, %d failed%s  (server %s)\n' "$BOLD" "$PASSED" "$FAILED" "$RESET" "$URL"
    [[ "$FAILED" -eq 0 ]]
}

STAMP="$(date +%Y%m%d%H%M%S)-$$"
TOKEN="selftest${STAMP//-/}"
echo "Testing memhub MCP server at $URL"

section "Transport"
request GET "$HEALTH_URL"
check "GET /health answers ok" eval 'code_is 200 && has "\"ok\""'
if ! code_is 200; then
    echo "Server is not reachable; aborting." >&2
    summary
    exit 1
fi

rpc initialize '{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"memhub-test-script","version":"1"}}'
SID="$(grep -i '^mcp-session-id:' <<<"$HEADERS" | head -n 1 | cut -d: -f2 | tr -d ' \r')"
check "initialize returns a session id" eval 'code_is 200 && [[ -n "$SID" ]]'
check "initialize negotiates protocol 2025-06-18" has '"protocolVersion":"2025-06-18"'
check "initialize includes agent instructions" has 'connect_agent'
if [[ -z "$SID" ]]; then
    echo "No MCP session; aborting." >&2
    summary
    exit 1
fi

request POST "$URL" '{"jsonrpc":"2.0","method":"notifications/initialized"}'
check "notifications/initialized is accepted (202)" code_is 202

rpc ping
check "ping" eval 'code_is 200 && has "\"result\":{}"'

rpc tools/list
TOOL_COUNT="$(grep -o '"inputSchema"' <<<"$BODY" | wc -l)"
check "tools/list returns $TOOL_COUNT tools" eval '[[ $TOOL_COUNT -ge 13 ]]'
for t in connect_agent add_repo search_memory list_memories get_memory add_memory update_memory delete_memory group_memories; do
    check "tool '$t' is listed" has "\"name\":\"$t\""
done

section "Identity"
tool search_memory '{"query":"x"}'
check "memory tools refuse before connect_agent" err_tool not_identified

tool connect_agent "{\"name\":\"$AGENT\",\"role\":\"selftest\",\"description\":\"memhub test script\"}"
check "connect_agent as '$AGENT'" ok_tool

tool whoami
check "whoami reports the agent" eval 'ok_tool && has "\"name\":\"$AGENT\""'

tool list_agents
check "list_agents includes the agent" eval 'ok_tool && has "\"name\":\"$AGENT\""'

section "Repos"
tool add_repo "{\"repo\":\"$REPO\",\"description\":\"memhub self-test repo\"}"
check "add_repo '$REPO'" eval 'ok_tool && has "\"working_repos\""'

tool list_repos
check "list_repos shows the repo as joined" eval 'ok_tool && has "\"id\":\"$REPO\"" && has "\"joined\":true"'

section "Memory"
tool add_memory "{\"scope\":\"repo\",\"repo\":\"$REPO\",\"title\":\"Self test $STAMP\",\"kind\":\"pitfall\",\"tags\":[\"selftest\"],\"body\":\"Unique marker $TOKEN.\\n\\nWritten by test-mcp.sh.\"}"
REPO_MEM="$(memory_id "repo/$REPO")"
check "add_memory (repo scope) -> ${REPO_MEM:-<no id>}" eval 'ok_tool && [[ -n "$REPO_MEM" ]]'

tool add_memory "{\"scope\":\"agent\",\"title\":\"Self test notes $STAMP\",\"kind\":\"task-log\",\"body\":\"Agent note $TOKEN\"}"
AGENT_MEM="$(memory_id "agent/$AGENT")"
check "add_memory (agent scope) -> ${AGENT_MEM:-<no id>}" eval 'ok_tool && [[ -n "$AGENT_MEM" ]]'

tool search_memory "{\"query\":\"$TOKEN\"}"
check "search_memory across all memory finds both" eval 'ok_tool && has "\"count\":2"'

tool search_memory "{\"query\":\"$TOKEN\",\"scope\":\"repo\",\"owner\":\"$REPO\"}"
check "search_memory limited to the repo finds one" eval 'ok_tool && has "\"count\":1" && has "\"id\":\"$REPO_MEM\""'

tool update_memory "{\"id\":\"$REPO_MEM\",\"append\":\"Appended $TOKEN-v2.\",\"expected_version\":1}"
check "update_memory with expected_version 1 -> version 2" eval 'ok_tool && has "\"version\":2"'

tool update_memory "{\"id\":\"$REPO_MEM\",\"title\":\"stale write\",\"expected_version\":1}"
check "stale update is rejected (version_conflict)" err_tool version_conflict

tool get_memory "{\"id\":\"$REPO_MEM\"}"
check "get_memory returns markdown with front matter and the edit" eval 'ok_tool && has "\"text\":\"---\\n" && has "$TOKEN-v2"'

tool group_memories "{\"group\":\"selftest-$STAMP\",\"ids\":[\"$REPO_MEM\",\"$AGENT_MEM\"]}"
check "group_memories adds both to a group" eval 'ok_tool && has "\"changed\":[\""'

tool list_memories "{\"group_by\":\"group\",\"group\":\"selftest-$STAMP\"}"
check "list_memories group_by=group shows the group" eval 'ok_tool && has "\"selftest-$STAMP\":["'

tool list_memories "{\"scope\":\"repo\",\"owner\":\"$REPO\",\"kind\":\"pitfall\"}"
check "list_memories with filters" eval 'ok_tool && has "\"id\":\"$REPO_MEM\""'

tool update_memory '{"id":"agent/some-other-agent/whatever","append":"x"}'
check "writing another agent's memory is forbidden" err_tool forbidden

section "Cleanup"
for id in "$REPO_MEM" "$AGENT_MEM"; do
    [[ -n "$id" ]] || continue
    tool delete_memory "{\"id\":\"$id\"}"
    check "delete_memory $id" ok_tool
done
if [[ -n "$REPO_MEM" ]]; then
    tool get_memory "{\"id\":\"$REPO_MEM\"}"
    check "deleted memory is gone (not_found)" err_tool not_found
fi

request DELETE "$URL"
check "DELETE ends the session (204)" code_is 204
rpc ping
check "the closed session is rejected (404)" code_is 404

summary
