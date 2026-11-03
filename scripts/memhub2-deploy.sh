#!/usr/bin/env bash
# Download a prebuilt Memory Hub 2.0 binary (memhub2) from GitHub Releases and
# (re)start it in the background. No Rust toolchain needed. Self-contained:
# this file can be copied to a server on its own.
#
#   scripts/memhub2-deploy.sh            download; restart only if the version changed
#   scripts/memhub2-deploy.sh stop       stop the running 2.0 server
#   scripts/memhub2-deploy.sh status     show whether the 2.0 server is running
#   scripts/memhub2-deploy.sh rollback   switch back to the previously deployed binary
#
# Memory Hub 2.0 runs next to 1.0 without touching it: its own binary
# (memhub2), port (8020), run directory (.run2) and data directory
# (memhub2-data). Shares the pid file with scripts/memhub2-restart.sh, so
# either script can stop/replace a server started by the other.
#
# The server gets a curator token (MEMHUB2_CURATOR_TOKEN, generated once into
# <run dir>/curator-token) that scripts/memhub2-curate.sh and
# scripts/memhub2-port.sh use to connect as curator.
#
# Environment overrides:
#   MEMHUB2_RELEASE      release to deploy: "nightly2" (default, latest main build),
#                        "latest" (newest v* release) or a tag such as "v2.0.0"
#   MEMHUB2_GITHUB_REPO  (default Zhang-JK/Cross-Repo-Memory-Hub)
#   MEMHUB2_BASE_URL     download assets from "$MEMHUB2_BASE_URL/<asset>" instead of GitHub
#   MEMHUB2_CURL_OPTS    extra curl flags, e.g. "--proxy http://proxy:8080"
#                        (curl also honors https_proxy / HTTPS_PROXY)
#   MEMHUB2_TLS_VERIFY=1 verify TLS certificates. Off by default (curl -k) because
#                        the company proxy re-signs HTTPS with an untrusted CA.
#   MEMHUB2_PORT         (default 8020)
#   MEMHUB2_HOST         (default 0.0.0.0, i.e. reachable from other machines)
#   MEMHUB2_DATA_DIR     (default <base>/memhub2-data)
#   MEMHUB2_RUN_DIR      (default <base>/.run2; binaries, pid file, server log, curator token)
#   MEMHUB2_CURATOR_TOKEN  curator token (default: <run dir>/curator-token, generated once)
#   MEMHUB2_ADMIN_TOKEN  token for the management UI (/admin); unset leaves it open
#   MEMHUB2_EXTRA_ARGS   extra `memhub2 serve` flags, e.g. "--trust-proxy"
#   FORCE=1              restart even if the downloaded version is already running
#
# <base> is the parent directory of this script's directory.

set -euo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${MEMHUB2_PORT:-8020}"
HOST="${MEMHUB2_HOST:-0.0.0.0}"
DATA_DIR="${MEMHUB2_DATA_DIR:-$BASE_DIR/memhub2-data}"
RUN_DIR="${MEMHUB2_RUN_DIR:-$BASE_DIR/.run2}"
EXTRA_ARGS="${MEMHUB2_EXTRA_ARGS:-}"
RELEASE="${MEMHUB2_RELEASE:-nightly2}"
GITHUB_REPO="${MEMHUB2_GITHUB_REPO:-Zhang-JK/Cross-Repo-Memory-Hub}"
CURL_OPTS="${MEMHUB2_CURL_OPTS:-}"
[[ "${MEMHUB2_TLS_VERIFY:-0}" == "1" ]] || CURL_OPTS="-k $CURL_OPTS"

PID_FILE="$RUN_DIR/memhub2.pid"
LOG_FILE="$RUN_DIR/memhub2.log"
BIN="$RUN_DIR/memhub2"
COMMIT_FILE="$RUN_DIR/memhub2.commit"
TOKEN_FILE="$RUN_DIR/curator-token"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# PID of our running server, or empty. Verifies the PID still belongs to
# memhub2 so a stale pid file never makes us signal an unrelated process.
running_pid() {
    [[ -f "$PID_FILE" ]] || return 0
    local pid
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
        && tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "memhub2.* serve"; then
        echo "$pid"
    fi
}

port_in_use() {
    ss -Hltn "sport = :$PORT" 2>/dev/null | grep -q .
}

health() {
    curl --noproxy '*' -fsS --max-time 2 "http://127.0.0.1:$PORT/health" 2>/dev/null
}

# Exports MEMHUB2_CURATOR_TOKEN: from the environment, else from the token
# file, else a new random one saved there (mode 600). It reaches the server
# through the environment, never the command line (visible in ps).
curator_token() {
    if [[ -z "${MEMHUB2_CURATOR_TOKEN:-}" ]]; then
        if [[ ! -s "$TOKEN_FILE" ]]; then
            info "Generating curator token in $TOKEN_FILE"
            (umask 077 && od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$TOKEN_FILE")
        fi
        MEMHUB2_CURATOR_TOKEN="$(tr -d ' \r\n' < "$TOKEN_FILE")"
    fi
    export MEMHUB2_CURATOR_TOKEN
}

target_triple() {
    case "$(uname -m)" in
        x86_64 | amd64) echo "x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) echo "aarch64-unknown-linux-musl" ;;
        *) die "no prebuilt binary for architecture $(uname -m)" ;;
    esac
}

asset_url() {
    local asset=$1
    if [[ -n "${MEMHUB2_BASE_URL:-}" ]]; then
        echo "${MEMHUB2_BASE_URL%/}/$asset"
    elif [[ "$RELEASE" == "latest" ]]; then
        echo "https://github.com/$GITHUB_REPO/releases/latest/download/$asset"
    else
        echo "https://github.com/$GITHUB_REPO/releases/download/$RELEASE/$asset"
    fi
}

fetch() {
    local url=$1 out=$2
    # shellcheck disable=SC2086 # CURL_OPTS is intentionally word-split
    curl -fsSL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 300 \
        $CURL_OPTS -o "$out" "$url" \
        || die "download failed: $url (check proxy settings: https_proxy / MEMHUB2_CURL_OPTS)"
}

stop() {
    local pid
    pid="$(running_pid)"
    if [[ -z "$pid" ]]; then
        info "No memhub2 server running"
        rm -f "$PID_FILE"
        return
    fi
    # SIGINT triggers graceful shutdown: open sessions are logged as disconnects.
    info "Stopping memhub2 server (pid $pid)"
    kill -INT "$pid"
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        warn "Server did not exit within 10s; sending SIGKILL"
        kill -KILL "$pid" 2>/dev/null || true
    fi
    rm -f "$PID_FILE"
    info "Stopped"
}

# Starts $BIN; returns non-zero (instead of exiting) so callers can roll back.
start() {
    if port_in_use; then
        warn "port $PORT is already in use by another process (check: ss -ltnp 'sport = :$PORT')"
        return 1
    fi
    mkdir -p "$DATA_DIR"
    curator_token
    info "Starting memhub2 on $HOST:$PORT"
    # setsid detaches from this shell's session so closing the terminal or SSH
    # connection does not take the server down with it. `9>&-` keeps the
    # server from inheriting (and forever holding) the deploy lock.
    # shellcheck disable=SC2086 # EXTRA_ARGS is intentionally word-split
    setsid nohup "$BIN" --data-dir "$DATA_DIR" serve --bind "$HOST:$PORT" $EXTRA_ARGS \
        >>"$LOG_FILE" 2>&1 < /dev/null 9>&- &
    local pid=$!
    echo "$pid" > "$PID_FILE"

    for _ in $(seq 1 50); do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$PID_FILE"
            echo "----- last log lines ($LOG_FILE) -----" >&2
            tail -n 20 "$LOG_FILE" >&2
            warn "server exited during startup"
            return 1
        fi
        health >/dev/null && return 0
        sleep 0.2
    done
    warn "server (pid $pid) is not answering on port $PORT; see $LOG_FILE"
    return 1
}

installed_commit() {
    cat "$COMMIT_FILE" 2>/dev/null || echo "unknown"
}

update() {
    local target asset tmp
    target="$(target_triple)"
    asset="memhub2-$target.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    info "Downloading $asset ($RELEASE)"
    fetch "$(asset_url "$asset")" "$tmp/$asset"
    fetch "$(asset_url "$asset.sha256")" "$tmp/$asset.sha256"
    (cd "$tmp" && sha256sum --quiet -c "$asset.sha256") || die "checksum mismatch for $asset"
    tar -xzf "$tmp/$asset" -C "$tmp"

    local new_bin="$tmp/memhub2-$target/memhub2"
    local new_commit
    new_commit="$(cat "$tmp/memhub2-$target/COMMIT" 2>/dev/null || echo unknown)"
    "$new_bin" --version >/dev/null 2>&1 || die "downloaded binary does not run on this machine"
    info "Downloaded $("$new_bin" --version), commit ${new_commit:0:12}"

    if [[ -z "${FORCE:-}" && "$new_commit" != "unknown" && "$new_commit" == "$(installed_commit)" \
        && -n "$(running_pid)" ]]; then
        info "Already running commit ${new_commit:0:12}; nothing to do (FORCE=1 to restart anyway)"
        status
        return
    fi

    stop
    if [[ -x "$BIN" ]]; then
        cp -p "$BIN" "$BIN.prev"
        installed_commit > "$COMMIT_FILE.prev"
    fi
    install -m 755 "$new_bin" "$BIN"
    echo "$new_commit" > "$COMMIT_FILE"

    if start; then
        status
        return
    fi
    if [[ -x "$BIN.prev" ]]; then
        warn "new version failed to start; rolling back to the previous binary"
        swap_previous
        # Do not keep the failed build around as a rollback target.
        rm -f "$BIN.prev" "$COMMIT_FILE.prev"
        start || die "previous binary failed to start as well; see $LOG_FILE"
        status
        exit 1
    fi
    die "server failed to start; see $LOG_FILE"
}

swap_previous() {
    mv "$BIN" "$BIN.swap" && mv "$BIN.prev" "$BIN" && mv "$BIN.swap" "$BIN.prev"
    local cur prev
    cur="$(installed_commit)"
    prev="$(cat "$COMMIT_FILE.prev" 2>/dev/null || echo unknown)"
    echo "$prev" > "$COMMIT_FILE"
    echo "$cur" > "$COMMIT_FILE.prev"
}

rollback() {
    [[ -x "$BIN.prev" ]] || die "no previous binary to roll back to"
    stop
    swap_previous
    info "Rolled back to commit $(installed_commit | cut -c1-12)"
    if ! start; then
        warn "rolled-back binary failed to start; restoring the version that was running"
        swap_previous
        start || die "could not restart the original binary either; see $LOG_FILE"
        status
        exit 1
    fi
    status
}

status() {
    local pid
    pid="$(running_pid)"
    if [[ -z "$pid" ]]; then
        echo "memhub2: not running"
        return 1
    fi
    local lan_ip url_host="127.0.0.1"
    lan_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    [[ "$HOST" != "0.0.0.0" && "$HOST" != "::" ]] && url_host="$HOST"
    [[ "$HOST" == "0.0.0.0" && -n "$lan_ip" ]] && url_host="$lan_ip"
    # Read by scripts/memhub2-test-mcp.sh.
    echo "http://$url_host:$PORT/mcp" > "$RUN_DIR/mcp_url"
    local config="${MEMHUB2_CURATOR_CONFIG:-$BASE_DIR/config/memhub2-curator.yaml}"
    local config_state="$config"
    [[ -f "$config" ]] || config_state="$config (missing: scripts/memhub2-curate.sh init)"

    cat <<EOF

memhub2 (Memory Hub 2.0) is running
  PID        : $pid
  Version    : $("$BIN" --version 2>/dev/null || echo unknown)
  Commit     : $(installed_commit)
  Started    : $(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/^ *//')
  Listen     : $HOST:$PORT
  MCP URL    : http://$url_host:$PORT/mcp
  Admin UI   : http://$url_host:$PORT/admin$([[ -n "${MEMHUB2_ADMIN_TOKEN:-}" ]] && echo "  (token required)")
  Health     : $(health || echo "not responding")
  Data dir   : $DATA_DIR
  Server log : $LOG_FILE
  Audit logs : $DATA_DIR/logs/
  Curator    : token in $TOKEN_FILE, config $config_state

  Follow log : tail -f $LOG_FILE
  Activity   : $BIN --data-dir $DATA_DIR logs --tail 50
  Self-test  : $(dirname "$0")/memhub2-test-mcp.sh
  Port 1.0   : $(dirname "$0")/memhub2-port.sh
  Curate     : $(dirname "$0")/memhub2-curate.sh      (daily: $(dirname "$0")/memhub2-curate.sh cron)
  Stop       : $0 stop
  Rollback   : $0 rollback

MCP client config (project .cursor/mcp.json; Cursor fills in the \${...} variables):
  { "mcpServers": { "memhub2": { "url": "http://$url_host:$PORT/mcp",
      "headers": { "X-Memhub-Agent": "<agent-name>", "X-Memhub-Role": "<role>",
                   "X-Memhub-Instance": "\${env:USER}-\${workspaceFolderBasename}" } } } }
EOF
}

mkdir -p "$RUN_DIR"
exec 9>"$RUN_DIR/deploy.lock"
flock -n 9 || die "another deploy is already running (lock: $RUN_DIR/deploy.lock)"

case "${1:-update}" in
    update | "") update ;;
    stop) stop ;;
    status) status ;;
    rollback) rollback ;;
    *) die "usage: $0 [update|stop|status|rollback]" ;;
esac
