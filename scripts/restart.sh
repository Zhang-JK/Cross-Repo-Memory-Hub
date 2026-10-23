#!/usr/bin/env bash
# Update, rebuild and (re)start the memhub MCP server in the background.
#
#   scripts/restart.sh           git pull, build, stop old server, start new one
#   scripts/restart.sh stop      stop the running server
#   scripts/restart.sh status    show whether the server is running
#
# Environment overrides:
#   MEMHUB_PORT        (default 8010)
#   MEMHUB_HOST        (default 0.0.0.0, i.e. reachable from other machines)
#   MEMHUB_DATA_DIR    (default <repo>/memhub-data)
#   MEMHUB_RUN_DIR     (default <repo>/.run; pid file, server log, binary copy)
#   MEMHUB_EXTRA_ARGS  extra `memhub serve` flags, e.g. "--trust-proxy --idle-timeout-secs 3600"
#   SKIP_PULL=1        do not run git pull
#   PULL_TIMEOUT       seconds before git pull is abandoned (default 60)

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${MEMHUB_PORT:-8010}"
HOST="${MEMHUB_HOST:-0.0.0.0}"
DATA_DIR="${MEMHUB_DATA_DIR:-$REPO_DIR/memhub-data}"
RUN_DIR="${MEMHUB_RUN_DIR:-$REPO_DIR/.run}"
EXTRA_ARGS="${MEMHUB_EXTRA_ARGS:-}"
PULL_TIMEOUT="${PULL_TIMEOUT:-60}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"

PID_FILE="$RUN_DIR/memhub.pid"
LOG_FILE="$RUN_DIR/memhub.log"
BIN="$RUN_DIR/memhub"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# PID of our running server, or empty. Verifies the PID still belongs to
# memhub so a stale pid file never makes us signal an unrelated process.
running_pid() {
    [[ -f "$PID_FILE" ]] || return 0
    local pid
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
        && tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q "memhub.* serve"; then
        echo "$pid"
    fi
}

port_in_use() {
    ss -Hltn "sport = :$PORT" 2>/dev/null | grep -q .
}

health() {
    curl --noproxy '*' -fsS --max-time 2 "http://127.0.0.1:$PORT/health" 2>/dev/null
}

pull() {
    if [[ -n "${SKIP_PULL:-}" ]]; then
        info "Skipping git pull (SKIP_PULL set)"
        return
    fi
    if ! git -C "$REPO_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        warn "$REPO_DIR is not a git repository; skipping git pull"
        return
    fi
    info "Pulling latest version"
    if ! GIT_TERMINAL_PROMPT=0 timeout "$PULL_TIMEOUT" git -C "$REPO_DIR" pull --ff-only; then
        die "git pull failed or timed out after ${PULL_TIMEOUT}s; the running server was not touched (retry, or use SKIP_PULL=1)"
    fi
    info "At commit $(git -C "$REPO_DIR" log -1 --format='%h %s')"
}

build() {
    command -v cargo >/dev/null || die "cargo not found in PATH"
    info "Building release binary"
    (cd "$REPO_DIR" && cargo build --release -p memhub-mcp --bin memhub)
}

stop() {
    local pid
    pid="$(running_pid)"
    if [[ -z "$pid" ]]; then
        info "No memhub server running"
        rm -f "$PID_FILE"
        return
    fi
    # SIGINT triggers graceful shutdown: open sessions are logged as disconnects.
    info "Stopping memhub server (pid $pid)"
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

start() {
    if port_in_use; then
        die "port $PORT is already in use by another process (check: ss -ltnp 'sport = :$PORT')"
    fi
    mkdir -p "$RUN_DIR" "$DATA_DIR"
    # Run a copy so later rebuilds never touch the binary of the live server.
    install -m 755 "$CARGO_TARGET_DIR/release/memhub" "$BIN"

    info "Starting memhub on $HOST:$PORT"
    # setsid detaches from this shell's session so closing the terminal or SSH
    # connection does not take the server down with it.
    # shellcheck disable=SC2086 # EXTRA_ARGS is intentionally word-split
    setsid nohup "$BIN" --data-dir "$DATA_DIR" serve --bind "$HOST:$PORT" $EXTRA_ARGS \
        >>"$LOG_FILE" 2>&1 < /dev/null &
    local pid=$!
    echo "$pid" > "$PID_FILE"

    for _ in $(seq 1 50); do
        if ! kill -0 "$pid" 2>/dev/null; then
            rm -f "$PID_FILE"
            echo "----- last log lines ($LOG_FILE) -----" >&2
            tail -n 20 "$LOG_FILE" >&2
            die "server exited during startup"
        fi
        health >/dev/null && break
        sleep 0.2
    done
    health >/dev/null || die "server (pid $pid) is not answering on port $PORT; see $LOG_FILE"
    status
}

status() {
    local pid
    pid="$(running_pid)"
    if [[ -z "$pid" ]]; then
        echo "memhub: not running"
        return 1
    fi
    local lan_ip
    lan_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    local url_host="127.0.0.1"
    [[ "$HOST" != "0.0.0.0" && "$HOST" != "::" ]] && url_host="$HOST"
    [[ "$HOST" == "0.0.0.0" && -n "$lan_ip" ]] && url_host="$lan_ip"
    # Read by scripts/test-mcp.sh.
    echo "http://$url_host:$PORT/mcp" > "$RUN_DIR/mcp_url"

    cat <<EOF

memhub is running
  PID        : $pid
  Version    : $("$BIN" --version 2>/dev/null || echo unknown)
  Started    : $(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/^ *//')
  Listen     : $HOST:$PORT
  MCP URL    : http://$url_host:$PORT/mcp
  Admin UI   : http://$url_host:$PORT/admin$([[ -n "${MEMHUB_ADMIN_TOKEN:-}" ]] && echo "  (token required)")
  Health     : $(health || echo "not responding")
  Data dir   : $DATA_DIR
  Server log : $LOG_FILE
  Audit logs : $DATA_DIR/logs/

  Follow log : tail -f $LOG_FILE
  Activity   : $BIN --data-dir $DATA_DIR logs --tail 50
  Self-test  : $(dirname "$0")/test-mcp.sh
  Stop       : $0 stop

MCP client config (project .cursor/mcp.json; Cursor fills in the \${...} variables):
  { "mcpServers": { "memhub": { "url": "http://$url_host:$PORT/mcp",
      "headers": { "X-Memhub-Agent": "<agent-name>", "X-Memhub-Role": "<role>",
                   "X-Memhub-Instance": "\${env:USER}-\${workspaceFolderBasename}" } } } }
EOF
}

case "${1:-restart}" in
    restart | start | "")
        pull
        build
        stop
        start
        ;;
    stop) stop ;;
    status) status ;;
    *) die "usage: $0 [restart|stop|status]" ;;
esac
