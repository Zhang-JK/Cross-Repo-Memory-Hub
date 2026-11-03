#!/usr/bin/env bash
# End-to-end check of a running Memory Hub 2.0 server (memhub2): speaks MCP
# over HTTP with curl and exercises the agent tools of the layered memory
# (evidence, work area, read-only experience) plus the curator gate. Needs
# only bash, curl and grep.
#
#   scripts/memhub2-test-mcp.sh [MCP_URL] [-v]
#
# MCP URL, first match wins:
#   1. the MCP_URL argument, e.g. http://10.0.0.5:8020/mcp
#   2. $MEMHUB2_MCP_URL
#   3. <run dir>/mcp_url, written by scripts/memhub2-restart.sh / scripts/memhub2-deploy.sh
#   4. interactive prompt (default http://127.0.0.1:${MEMHUB2_PORT:-8020}/mcp)
#
# The test connects as agent "memhub-selftest" (its evidence is never
# curated) and joins repo "memhub-selftest" (both stay registered
# afterwards). Evidence and notes it creates are deleted at the end. With a
# curator token (MEMHUB2_CURATOR_TOKEN or <run dir>/curator-token) it also
# checks that a curator session sees the curation tools.
#
# Environment overrides:
#   MEMHUB2_TEST_AGENT    (default memhub-selftest)
#   MEMHUB2_TEST_REPO     (default memhub-selftest)
#   MEMHUB2_RUN_DIR       (default <base>/.run2)
#   MEMHUB2_TEST_PROXY=1  send requests through http(s)_proxy (default: bypass proxies)
#   MEMHUB2_TLS_VERIFY=1  verify TLS certificates for https URLs (default: curl -k)

set -uo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${MEMHUB2_RUN_DIR:-$BASE_DIR/.run2}"
AGENT="${MEMHUB2_TEST_AGENT:-memhub-selftest}"
REPO="${MEMHUB2_TEST_REPO:-memhub-selftest}"

VERBOSE=0
URL=""
for arg in "$@"; do
    case "$arg" in
        -v | --verbose) VERBOSE=1 ;;
        -h | --help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) URL="$arg" ;;
    esac
done

if [[ -z "$URL" && -n "${MEMHUB2_MCP_URL:-}" ]]; then
    URL="$MEMHUB2_MCP_URL"
fi
if [[ -z "$URL" && -s "$RUN_DIR/mcp_url" ]]; then
    URL="$(head -n 1 "$RUN_DIR/mcp_url")"
    echo "Using MCP URL saved by the launch script ($RUN_DIR/mcp_url)"
fi
if [[ -z "$URL" ]]; then
    default="http://127.0.0.1:${MEMHUB2_PORT:-8020}/mcp"
    if [[ -t 0 ]]; then
        read -r -p "MCP server URL [$default]: " URL
    fi
    URL="${URL:-$default}"
fi
[[ "$URL" == *://* ]] || URL="http://$URL"
URL="${URL%/}"
[[ "$URL" == */mcp ]] || URL="$URL/mcp"
HEALTH_URL="${URL%/mcp}/health"

CURATOR_TOKEN="${MEMHUB2_CURATOR_TOKEN:-}"
if [[ -z "$CURATOR_TOKEN" && -s "$RUN_DIR/curator-token" ]]; then
    CURATOR_TOKEN="$(tr -d ' \r\n' < "$RUN_DIR/curator-token")"
fi

CURL=(curl -sS --connect-timeout 5 --max-time 20)
[[ "${MEMHUB2_TEST_PROXY:-0}" == "1" ]] || CURL+=(--noproxy '*')
[[ "${MEMHUB2_TLS_VERIFY:-0}" == "1" ]] || CURL+=(-k)

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
EXTRA_HEADERS=()

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
    for h in "${EXTRA_HEADERS[@]}"; do args+=(-H "$h"); done
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

initialize() {
    rpc initialize '{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"memhub2-test-script","version":"1"}}'
    SID="$(grep -i '^mcp-session-id:' <<<"$HEADERS" | head -n 1 | cut -d: -f2 | tr -d ' \r')"
}

# First id with the given prefix in the last response.
id_with_prefix() {
    grep -oE "\"id\":\"$1[^\"]*\"" <<<"$BODY" | head -n 1 | sed 's/^"id":"//; s/"$//'
}

summary() {
    printf '\n%s%d passed, %d failed%s  (server %s)\n' "$BOLD" "$PASSED" "$FAILED" "$RESET" "$URL"
    [[ "$FAILED" -eq 0 ]]
}

STAMP="$(date +%Y%m%d%H%M%S)-$$"
TOKEN="selftest${STAMP//-/}"
echo "Testing Memory Hub 2.0 MCP server at $URL"

section "Transport"
request GET "$HEALTH_URL"
check "GET /health answers ok" eval 'code_is 200 && has "\"ok\""'
if ! code_is 200; then
    echo "Server is not reachable; aborting." >&2
    summary
    exit 1
fi

initialize
check "initialize returns a session id" eval 'code_is 200 && [[ -n "$SID" ]]'
check "initialize negotiates protocol 2025-06-18" has '"protocolVersion":"2025-06-18"'
check "the server is memhub2 (Memory Hub 2.0)" has '"name":"memhub2"'
check "instructions explain the three layers" eval 'has "THE THREE LAYERS" && has "record_evidence" && has "working/"'
check "instructions explain feedback" eval 'has "rate_memory" && has "dispute_memory"'
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
check "tools/list returns $TOOL_COUNT tools" eval '[[ $TOOL_COUNT -ge 20 ]]'
for t in connect_agent add_repo browse_memory search_memory get_memory record_evidence search_evidence rate_memory dispute_memory add_memory update_memory move_memory delete_memory; do
    check "tool '$t' is listed" has "\"name\":\"$t\""
done
check "curation tools are hidden from agents" eval 'lacks "\"name\":\"curation_batch\"" && lacks "\"name\":\"propose_changes\""'

section "Identity"
tool search_memory '{"query":"x"}'
check "memory tools refuse before connect_agent" err_tool not_identified

tool connect_agent "{\"name\":\"$AGENT\",\"role\":\"selftest\",\"description\":\"memhub2 test script\"}"
check "connect_agent as '$AGENT'" ok_tool

tool whoami
check "whoami reports the agent" eval 'ok_tool && has "\"name\":\"$AGENT\""'

section "Repos"
tool add_repo "{\"repo\":\"$REPO\",\"description\":\"memhub self-test repo\"}"
check "add_repo '$REPO'" eval 'ok_tool && has "\"working_repos\""'

tool list_repos
check "list_repos shows the repo as joined" eval 'ok_tool && has "\"id\":\"$REPO\"" && has "\"joined\":true"'

section "Evidence (what agents write)"
tool record_evidence "{\"title\":\"Self test $STAMP\",\"body\":\"Unique marker $TOKEN.\\n\\nWritten by memhub2-test-mcp.sh.\",\"repos\":[\"$REPO\"],\"task\":\"selftest $STAMP\",\"outcome\":\"success\",\"reason\":\"checks the evidence layer\",\"candidate_lesson\":\"The self-test can write evidence.\",\"code_refs\":[\"scripts/memhub2-test-mcp.sh:1\"]}"
EVID="$(id_with_prefix "evidence/")"
check "record_evidence -> ${EVID:-<no id>}" eval 'ok_tool && [[ -n "$EVID" ]]'

tool get_memory "{\"id\":\"$EVID\"}"
check "get_memory reads evidence by id" eval 'ok_tool && has "\"layer\":\"evidence\"" && has "$TOKEN"'

tool search_evidence "{\"query\":\"$TOKEN\",\"repo\":\"$REPO\"}"
check "search_evidence finds it" eval 'ok_tool && has "\"count\":1" && has "\"id\":\"$EVID\""'

tool search_memory "{\"query\":\"$TOKEN\"}"
check "search_memory does not search evidence" eval 'ok_tool && has "\"count\":0"'

tool add_memory "{\"scope\":\"repo\",\"repo\":\"$REPO\",\"title\":\"Old habit $STAMP\",\"body\":\"Converted marker $TOKEN\"}"
EVID2="$(id_with_prefix "evidence/")"
check "add_memory into repo memory is recorded as evidence -> ${EVID2:-<no id>}" eval 'ok_tool && has "\"layer\":\"evidence\"" && [[ -n "$EVID2" ]]'

section "Experience is curated"
tool create_folder "{\"scope\":\"repo\",\"repo\":\"$REPO\",\"folder\":\"selftest\"}"
check "agents cannot create repo folders (forbidden)" err_tool forbidden

tool update_memory '{"id":"agent/some-other-agent/working/whatever","append":"x"}'
check "another agent's work area is read-only" err_tool forbidden

tool browse_memory "{\"scope\":\"repo\",\"owner\":\"$REPO\"}"
check "browse_memory shows the repo tree" eval 'ok_tool && has "repo/$REPO"'

section "Work area (agent/<you>/working)"
tool add_memory "{\"scope\":\"agent\",\"title\":\"Self test plan $STAMP\",\"kind\":\"plan\",\"tags\":[\"selftest\"],\"body\":\"Plan marker $TOKEN\"}"
NOTE="$(id_with_prefix "agent/$AGENT/working/")"
check "add_memory (scope agent) lands in the work area -> ${NOTE:-<no id>}" eval 'ok_tool && [[ -n "$NOTE" ]] && has "\"work_note\":true"'

tool update_memory "{\"id\":\"$NOTE\",\"append\":\"Appended $TOKEN-v2.\",\"expected_version\":1}"
check "update_memory with expected_version 1 -> version 2" eval 'ok_tool && has "\"version\":2"'

tool update_memory "{\"id\":\"$NOTE\",\"title\":\"stale write\",\"expected_version\":1}"
check "stale update is rejected (version_conflict)" err_tool version_conflict

tool get_memory "{\"id\":\"$NOTE\"}"
check "get_memory returns markdown with front matter and the edit" eval 'ok_tool && has "\"text\":\"---\\n" && has "$TOKEN-v2"'

tool search_memory "{\"query\":\"$TOKEN\"}"
check "search_memory finds your own work notes" eval 'ok_tool && has "\"count\":1" && has "\"id\":\"$NOTE\""'

tool create_folder "{\"scope\":\"agent\",\"folder\":\"working/selftest-$STAMP\",\"description\":\"Scratch space of memhub2-test-mcp.sh\"}"
check "create_folder in the work area with a description" eval 'ok_tool && has "\"folder\":\"working/selftest-$STAMP\""'

tool move_memory "{\"id\":\"$NOTE\",\"folder\":\"working/selftest-$STAMP\",\"expected_version\":2}"
MOVED="$(id_with_prefix "agent/$AGENT/working/selftest-$STAMP/")"
check "move_memory into the task folder -> ${MOVED:-<no id>}" eval 'ok_tool && [[ -n "$MOVED" ]] && has "\"previous_id\":\"$NOTE\""'
if [[ -n "$MOVED" ]]; then
    tool get_memory "{\"id\":\"$NOTE\"}"
    check "the old id redirects to the new place" eval 'ok_tool && has "now lives at $MOVED"'
    NOTE="$MOVED"
fi

tool browse_memory '{"scope":"agent"}'
check "browse_memory shows the work area with the folder description" eval 'ok_tool && has "Scratch space of memhub2-test-mcp.sh"'

if [[ -n "$CURATOR_TOKEN" ]]; then
    section "Curator gate"
    MAIN_SID="$SID"
    SID=""
    EXTRA_HEADERS=("x-memhub-curator-token: wrong-$STAMP" "x-memhub-agent: memhub-curator" "x-memhub-role: curator")
    initialize
    check "a wrong curator token is refused (403)" code_is 403
    SID=""
    EXTRA_HEADERS=("x-memhub-curator-token: $CURATOR_TOKEN" "x-memhub-agent: memhub-curator" "x-memhub-role: curator")
    initialize
    check "the curator token opens a curator session" eval 'code_is 200 && has "CURATOR SESSION"'
    rpc tools/list
    check "curator sessions see the curation tools" eval 'has "\"name\":\"curation_batch\"" && has "\"name\":\"propose_changes\""'
    tool curation_status
    check "curation_status answers" eval 'ok_tool && has "\"targets\""'
    tool curation_status
    check "self-test evidence is never queued for curation" eval 'ok_tool && lacks "\"target\":\"repo:$REPO\",\"pending_evidence\":1"'
    request DELETE "$URL"
    EXTRA_HEADERS=()
    SID="$MAIN_SID"
else
    section "Curator gate"
    echo "  skipped: no curator token (MEMHUB2_CURATOR_TOKEN or $RUN_DIR/curator-token)"
fi

section "Cleanup"
if [[ -n "$NOTE" ]]; then
    tool delete_memory "{\"id\":\"$NOTE\"}"
    check "delete_memory $NOTE" ok_tool
    tool get_memory "{\"id\":\"$NOTE\"}"
    check "deleted note is gone (not_found)" err_tool not_found
fi
for id in "$EVID" "$EVID2"; do
    [[ -n "$id" ]] || continue
    tool delete_evidence "{\"id\":\"$id\"}"
    check "delete_evidence $id (the self-test's own records only)" ok_tool
done

request DELETE "$URL"
check "DELETE ends the session (204)" code_is 204
rpc ping
check "the closed session is rejected (404)" code_is 404

summary
