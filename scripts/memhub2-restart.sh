#!/usr/bin/env bash
# Update, rebuild and (re)start the Memory Hub 2.0 server (memhub2) in the
# background.
#
#   scripts/memhub2-restart.sh           git pull, build, stop old 2.0 server, start new one
#   scripts/memhub2-restart.sh stop      stop the running 2.0 server
#   scripts/memhub2-restart.sh status    show whether the 2.0 server is running
#
# Runs next to Memory Hub 1.0 (scripts/restart.sh) without touching it: its own
# binary, port, run directory and data directory. Shares the pid file with
# scripts/memhub2-deploy.sh.
#
# Environment overrides:
#   MEMHUB2_PORT          (default 8020)
#   MEMHUB2_HOST          (default 0.0.0.0, i.e. reachable from other machines)
#   MEMHUB2_DATA_DIR      (default <repo>/memhub2-data)
#   MEMHUB2_RUN_DIR       (default <repo>/.run2; pid file, server log, binary copy, curator token)
#   MEMHUB2_CURATOR_TOKEN curator token (default: <run dir>/curator-token, generated once)
#   MEMHUB2_ADMIN_TOKEN   token for the management UI (/admin); unset leaves it open
#   MEMHUB2_EXTRA_ARGS    extra `memhub2 serve` flags, e.g. "--trust-proxy --idle-timeout-secs 3600"
#   SKIP_PULL=1           do not run git pull
#   PULL_TIMEOUT          seconds before git pull is abandoned (default 60)

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${MEMHUB2_PORT:-8020}"
HOST="${MEMHUB2_HOST:-0.0.0.0}"
DATA_DIR="${MEMHUB2_DATA_DIR:-$REPO_DIR/memhub2-data}"
RUN_DIR="${MEMHUB2_RUN_DIR:-$REPO_DIR/.run2}"
EXTRA_ARGS="${MEMHUB2_EXTRA_ARGS:-}"
PULL_TIMEOUT="${PULL_TIMEOUT:-60}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"

PID_FILE="$RUN_DIR/memhub2.pid"
LOG_FILE="$RUN_DIR/memhub2.log"
BIN="$RUN_DIR/memhub2"
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
    (cd "$REPO_DIR" && cargo build --release -p memhub-mcp --bin memhub2)
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

start() {
    if port_in_use; then
        die "port $PORT is already in use by another process (check: ss -ltnp 'sport = :$PORT')"
    fi
    mkdir -p "$RUN_DIR" "$DATA_DIR"
    # Run a copy so later rebuilds never touch the binary of the live server.
    install -m 755 "$CARGO_TARGET_DIR/release/memhub2" "$BIN"
    git -C "$REPO_DIR" rev-parse HEAD > "$RUN_DIR/memhub2.commit" 2>/dev/null || true
    curator_token

    info "Starting memhub2 on $HOST:$PORT"
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
        echo "memhub2: not running"
        return 1
    fi
    local lan_ip
    lan_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
    local url_host="127.0.0.1"
    [[ "$HOST" != "0.0.0.0" && "$HOST" != "::" ]] && url_host="$HOST"
    [[ "$HOST" == "0.0.0.0" && -n "$lan_ip" ]] && url_host="$lan_ip"
    # Read by scripts/memhub2-test-mcp.sh.
    echo "http://$url_host:$PORT/mcp" > "$RUN_DIR/mcp_url"
    local config="${MEMHUB2_CURATOR_CONFIG:-$REPO_DIR/config/memhub2-curator.yaml}"
    local config_state="$config"
    [[ -f "$config" ]] || config_state="$config (missing: scripts/memhub2-curate.sh init)"

    cat <<EOF

memhub2 (Memory Hub 2.0) is running
  PID        : $pid
  Version    : $("$BIN" --version 2>/dev/null || echo unknown)
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

MCP client config (project .cursor/mcp.json; Cursor fills in the \${...} variables):
  { "mcpServers": { "memhub2": { "url": "http://$url_host:$PORT/mcp",
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
