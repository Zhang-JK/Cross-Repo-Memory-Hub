#!/usr/bin/env bash
# Run the Memory Hub 2.0 curator: an LLM turns new evidence into curated
# experience (and reviews memories ported from 1.0). Meant to run daily.
#
#   scripts/memhub2-curate.sh [curate flags]   one run, e.g. --dry-run, --target repo:shop,
#                                              --no-backfill, --backfill-only
#   scripts/memhub2-curate.sh init             write config/memhub2-curator.yaml to fill in
#   scripts/memhub2-curate.sh cron             print a crontab line for a daily run
#   scripts/memhub2-curate.sh proposals ...    list / show / approve / reject proposals
#                                              (e.g. `proposals list`, `proposals approve <id> --op 0 --op 2`)
#
# The LLM endpoint, model and API key live in config/memhub2-curator.yaml
# (git-ignored; `init` creates it from the commented example). Any
# OpenAI-compatible server works: Ollama, vLLM, llama.cpp, LM Studio, OpenAI.
# The curator token comes from <run dir>/curator-token, written by
# scripts/memhub2-deploy.sh / scripts/memhub2-restart.sh.
#
# Every run writes a JSON report to <run dir>/reports/ and appends to
# <run dir>/curate.log. Runs never overlap (flock).
#
# Environment overrides:
#   MEMHUB2_PORT            port of the local 2.0 server (default 8020)
#   MEMHUB2_URL             2.0 MCP endpoint (default http://127.0.0.1:$MEMHUB2_PORT/mcp)
#   MEMHUB2_CURATOR_CONFIG  (default <base>/config/memhub2-curator.yaml)
#   MEMHUB2_DATA_DIR        (default <base>/memhub2-data; used by `proposals`)
#   MEMHUB2_RUN_DIR         (default <base>/.run2)
#   MEMHUB2_CURATOR_TOKEN   (default: contents of <run dir>/curator-token)
#   MEMHUB2_LLM_BASE_URL, MEMHUB2_LLM_MODEL, MEMHUB2_LLM_API_KEY  override the config file

set -euo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
PORT="${MEMHUB2_PORT:-8020}"
RUN_DIR="${MEMHUB2_RUN_DIR:-$BASE_DIR/.run2}"
DATA_DIR="${MEMHUB2_DATA_DIR:-$BASE_DIR/memhub2-data}"
CONFIG="${MEMHUB2_CURATOR_CONFIG:-$BASE_DIR/config/memhub2-curator.yaml}"
TOKEN_FILE="$RUN_DIR/curator-token"
export MEMHUB2_URL="${MEMHUB2_URL:-http://127.0.0.1:$PORT/mcp}"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

find_bin() {
    local candidate
    for candidate in "$RUN_DIR/memhub2" "$BASE_DIR/target/release/memhub2" "$(command -v memhub2 || true)"; do
        if [[ -n "$candidate" && -x "$candidate" ]]; then
            echo "$candidate"
            return
        fi
    done
    die "memhub2 binary not found; deploy first (scripts/memhub2-deploy.sh or scripts/memhub2-restart.sh)"
}
BIN="$(find_bin)"

init_config() {
    if [[ -f "$CONFIG" ]]; then
        info "$CONFIG already exists"
        return
    fi
    mkdir -p "$(dirname "$CONFIG")"
    "$BIN" init-config --path "$CONFIG"
    chmod 600 "$CONFIG"
    cat <<EOF

Edit $CONFIG and set at least:
  llm.base_url  OpenAI-compatible endpoint, e.g. http://127.0.0.1:11434/v1 (Ollama)
  llm.model     model name, e.g. qwen2.5:32b-instruct
  llm.api_key   token for the endpoint (leave "" if it needs none)
Then try: $SCRIPT --dry-run
EOF
}

curator_token() {
    if [[ -z "${MEMHUB2_CURATOR_TOKEN:-}" ]]; then
        [[ -s "$TOKEN_FILE" ]] \
            || die "no curator token: start the 2.0 server with scripts/memhub2-deploy.sh or scripts/memhub2-restart.sh first (they create $TOKEN_FILE)"
        MEMHUB2_CURATOR_TOKEN="$(tr -d ' \r\n' < "$TOKEN_FILE")"
    fi
    export MEMHUB2_CURATOR_TOKEN
}

run() {
    if [[ ! -f "$CONFIG" ]]; then
        init_config
        die "fill in the LLM settings in $CONFIG, then run $SCRIPT again"
    fi
    curator_token
    curl --noproxy '*' -fsS --max-time 5 "${MEMHUB2_URL%/mcp}/health" >/dev/null 2>&1 \
        || die "Memory Hub 2.0 is not answering at $MEMHUB2_URL (scripts/memhub2-deploy.sh status)"
    mkdir -p "$RUN_DIR/reports"
    exec 9>"$RUN_DIR/curate.lock"
    flock -n 9 || die "another curator run is in progress (lock: $RUN_DIR/curate.lock)"

    local report
    report="$RUN_DIR/reports/curate-$(date +%Y%m%d-%H%M%S)-$$.json"
    info "Curating $MEMHUB2_URL with $CONFIG (report: $report)"
    # The group runs in a subshell (it feeds tee); its exit status is the run's.
    if ! {
        echo "===== $(date -Is) curate $*"
        rc=0
        "$BIN" curate --config "$CONFIG" --report "$report" "$@" 2>&1 || rc=$?
        echo "===== exit $rc"
        exit "$rc"
    } | tee -a "$RUN_DIR/curate.log"; then
        die "curation failed; see $RUN_DIR/curate.log and $report"
    fi
    info "Pending human decisions: $SCRIPT proposals list"
}

case "${1:-}" in
    init) init_config ;;
    cron)
        cat <<EOF
# Memory Hub 2.0 curator, daily at 02:30 server time (crontab -e):
30 2 * * * $SCRIPT >> $RUN_DIR/curate-cron.log 2>&1
EOF
        ;;
    proposals)
        shift
        "$BIN" --data-dir "$DATA_DIR" proposals "$@"
        ;;
    -h | --help) sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//' ;;
    *) run "$@" ;;
esac
