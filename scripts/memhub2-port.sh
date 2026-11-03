#!/usr/bin/env bash
# Port Memory Hub 1.0 into Memory Hub 2.0, server to server over MCP.
#
#   scripts/memhub2-port.sh [OLD_MCP_URL] [NEW_MCP_URL] [--yes] [--only repo:<id>|agent:<name> ...]
#
#   OLD_MCP_URL  the running 1.0 server (default http://127.0.0.1:${MEMHUB_PORT:-8010}/mcp)
#   NEW_MCP_URL  the running 2.0 server (default http://127.0.0.1:${MEMHUB2_PORT:-8020}/mcp)
#
# Steps: checks both servers, shows a dry run (where every memory goes), asks
# for confirmation (--yes or YES=1 skips it), backs up the 2.0 data dir, then
# imports agents, repos, folders and memories:
#   - repo memories and agent knowledge -> experience (status provisional,
#     citing their original text as evidence/legacy/...);
#   - agent todo / plan memories -> the agent's work area (working/...);
#   - task logs and status notes -> evidence only.
# Old ids keep working: get_memory redirects them. 1.0 is only read (as agent
# memhub-selftest, so its read counts are untouched) and keeps running.
#
# The port is idempotent: run it again to bring over memories written in 1.0
# since (memories already ported are skipped, also when changed in 1.0 since).
# Afterwards let the curator review the ported memories:
#   scripts/memhub2-curate.sh --backfill-only --dry-run; scripts/memhub2-curate.sh
#
# Environment overrides:
#   MEMHUB1_URL, MEMHUB2_URL  instead of the positional URLs
#   MEMHUB2_CURATOR_TOKEN     (default: contents of <run dir>/curator-token)
#   MEMHUB2_CURATOR_CONFIG    (default <base>/config/memhub2-curator.yaml; optional here)
#   MEMHUB2_DATA_DIR          (default <base>/memhub2-data; backed up before the port)
#   MEMHUB2_RUN_DIR           (default <base>/.run2)
#   YES=1                     do not ask for confirmation
#   SKIP_BACKUP=1             do not back up the 2.0 data dir

set -euo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${MEMHUB2_RUN_DIR:-$BASE_DIR/.run2}"
DATA_DIR="${MEMHUB2_DATA_DIR:-$BASE_DIR/memhub2-data}"
CONFIG="${MEMHUB2_CURATOR_CONFIG:-$BASE_DIR/config/memhub2-curator.yaml}"
TOKEN_FILE="$RUN_DIR/curator-token"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

normalize_url() {
    local url=$1
    [[ "$url" == *://* ]] || url="http://$url"
    url="${url%/}"
    [[ "$url" == */mcp ]] || url="$url/mcp"
    echo "$url"
}

OLD_URL="${MEMHUB1_URL:-}"
NEW_URL="${MEMHUB2_URL:-}"
YES="${YES:-}"
ONLY=()
positional=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes | -y) YES=1 ;;
        --only) shift; ONLY+=(--only "${1:?--only needs repo:<id> or agent:<name>}") ;;
        -h | --help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) positional+=("$1") ;;
    esac
    shift
done
[[ ${#positional[@]} -ge 1 ]] && OLD_URL="${positional[0]}"
[[ ${#positional[@]} -ge 2 ]] && NEW_URL="${positional[1]}"
OLD_URL="$(normalize_url "${OLD_URL:-http://127.0.0.1:${MEMHUB_PORT:-8010}/mcp}")"
NEW_URL="$(normalize_url "${NEW_URL:-http://127.0.0.1:${MEMHUB2_PORT:-8020}/mcp}")"
[[ "$OLD_URL" != "$NEW_URL" ]] || die "old and new server are the same URL ($OLD_URL)"
export MEMHUB1_URL="$OLD_URL" MEMHUB2_URL="$NEW_URL"

BIN=""
for candidate in "$RUN_DIR/memhub2" "$BASE_DIR/target/release/memhub2" "$(command -v memhub2 || true)"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then BIN="$candidate"; break; fi
done
[[ -n "$BIN" ]] || die "memhub2 binary not found; deploy 2.0 first (scripts/memhub2-deploy.sh)"

if [[ -z "${MEMHUB2_CURATOR_TOKEN:-}" ]]; then
    [[ -s "$TOKEN_FILE" ]] \
        || die "no curator token: start 2.0 with scripts/memhub2-deploy.sh or scripts/memhub2-restart.sh (they create $TOKEN_FILE), or set MEMHUB2_CURATOR_TOKEN"
    MEMHUB2_CURATOR_TOKEN="$(tr -d ' \r\n' < "$TOKEN_FILE")"
fi
export MEMHUB2_CURATOR_TOKEN

check_health() {
    local name=$1 url=$2
    curl --noproxy '*' -fsS --max-time 5 "${url%/mcp}/health" >/dev/null 2>&1 \
        || die "$name is not answering at $url"
    info "$name is up at $url"
}
check_health "Memory Hub 1.0 (old)" "$OLD_URL"
check_health "Memory Hub 2.0 (new)" "$NEW_URL"

mkdir -p "$RUN_DIR/reports"
STAMP="$(date +%Y%m%d-%H%M%S)-$$"
DRY_REPORT="$RUN_DIR/reports/port-dry-$STAMP.json"
REPORT="$RUN_DIR/reports/port-$STAMP.json"

info "Dry run: where every 1.0 memory would go"
"$BIN" port --config "$CONFIG" --dry-run --report "$DRY_REPORT" "${ONLY[@]}"
echo "  details: $DRY_REPORT"

if [[ -z "$YES" ]]; then
    [[ -t 0 ]] || die "not a terminal; pass --yes (or YES=1) to port without asking"
    read -r -p "Port these memories into $NEW_URL? [y/N] " answer
    [[ "$answer" == [yY]* ]] || die "aborted; nothing was written"
fi

if [[ -z "${SKIP_BACKUP:-}" && -d "$DATA_DIR" ]]; then
    mkdir -p "$RUN_DIR/backups"
    backup="$RUN_DIR/backups/memhub2-data-before-port-$STAMP.tar.gz"
    info "Backing up $DATA_DIR to $backup"
    tar -czf "$backup" -C "$(dirname "$DATA_DIR")" "$(basename "$DATA_DIR")"
fi

info "Porting $OLD_URL -> $NEW_URL"
"$BIN" port --config "$CONFIG" --report "$REPORT" "${ONLY[@]}"
echo "  details: $REPORT"

cat <<EOF

Done. Next steps:
  1. Check 2.0:        scripts/memhub2-test-mcp.sh $NEW_URL
  2. Review the port:  scripts/memhub2-curate.sh --backfill-only --dry-run
                       scripts/memhub2-curate.sh            (then daily: scripts/memhub2-curate.sh cron)
  3. Point agents at 2.0: MCP url $NEW_URL (same X-Memhub-Agent / X-Memhub-Role headers).
  Memories written to 1.0 in the meantime: run this script again.
EOF
