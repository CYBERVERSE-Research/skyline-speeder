#!/usr/bin/env bash
# Prepare, start or stop the sending side of the single-ended comparison.
#
#   sudo ./server-setup.sh start   # install into $BENCH_DIR and start serving
#   sudo ./server-setup.sh stop    # stop everything this script started
#
# Needs skyline-speeder installed and enabled (for the sockops cgroup and ssctl)
# and tcp-brutal installed (for brutalctl), plus nginx and python3.
#
# The HTTP server is a private nginx instance with its own config and pid file,
# never the system nginx: this runs on hosts that may serve real traffic, and
# stopping or reconfiguring their web server to run a benchmark is not ours to do.
#
# Both listeners (HTTP on $HTTP_PORT, game echo on $GAME_PORT) are reachable from
# anywhere the firewall allows. Stop them when the matrix is done.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
# Not under /root: nginx workers run as an unprivileged user and could not read
# test content inside a 0700 home directory -- every request would be a 403.
BENCH_DIR=${BENCH_DIR:-/srv/skyline-bench}
HTTP_PORT=${HTTP_PORT:-8080}
GAME_PORT=${GAME_PORT:-9999}
# Every sending process must live in the skyline cgroup, or skyline_policy.bpf.c
# (M1's dynamic RTO) never sees its flows -- the silent failure CLAUDE.md lists
# first, visible only as rack_rto.stats.applied staying at 0.
CGROUP_RUN=${CGROUP_RUN:-/opt/skyline-speeder/infra/run-in-skyline-cgroup.sh}

stop() {
    for name in nginx gameserver; do
        pidfile="$BENCH_DIR/$name.pid"
        if [ -s "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
            kill "$(cat "$pidfile")" && echo "stopped $name"
        fi
        rm -f "$pidfile"
    done
}

start() {
    command -v brutalctl >/dev/null || { echo "brutalctl not found: install tcp-brutal first" >&2; exit 1; }
    command -v ssctl >/dev/null || { echo "ssctl not found: install skyline-speeder first" >&2; exit 1; }
    [ -x "$CGROUP_RUN" ] || { echo "missing $CGROUP_RUN" >&2; exit 1; }

    install -d "$BENCH_DIR/www" "$BENCH_DIR/nginx-temp"
    install -m 0755 "$HERE/setalgo.sh" "$HERE/gameserver.py" "$BENCH_DIR/"

    # Random bytes, so no layer along the way can compress its way to a result.
    for spec in 10k:10 50k:50 100k:100; do
        f="$BENCH_DIR/www/${spec%%:*}.bin"
        [ -f "$f" ] || head -c $(( ${spec##*:} * 1024 )) /dev/urandom > "$f"
    done
    [ -f "$BENCH_DIR/www/bulk200m.bin" ] \
        || head -c $(( 200 * 1024 * 1024 )) /dev/urandom > "$BENCH_DIR/www/bulk200m.bin"

    cat > "$BENCH_DIR/nginx.conf" <<CONF
pid $BENCH_DIR/nginx.pid;
error_log $BENCH_DIR/nginx-error.log;
worker_processes auto;
events { worker_connections 1024; }
http {
    access_log off;
    client_body_temp_path $BENCH_DIR/nginx-temp;
    sendfile on;
    tcp_nodelay on;
    tcp_nopush on;
    gzip off;
    keepalive_requests 10000;
    server {
        listen $HTTP_PORT;
        root $BENCH_DIR/www;
    }
}
CONF

    stop >/dev/null
    nginx -t -c "$BENCH_DIR/nginx.conf" -q
    # daemon off so the process stays a child of the cgroup wrapper, then
    # backgrounded here; nginx writes its own pid file.
    # env -u SUDO_USER: run-in-skyline-cgroup.sh re-executes as $SUDO_USER when it
    # is set, and an nginx master that is not root cannot write its pid file here.
    # Clearing it makes "sudo ./server-setup.sh" and a root shell behave the same.
    nohup env -u SUDO_USER "$CGROUP_RUN" nginx -c "$BENCH_DIR/nginx.conf" -g 'daemon off;' \
        >"$BENCH_DIR/nginx.out" 2>&1 &
    nohup env -u SUDO_USER "$CGROUP_RUN" python3 "$BENCH_DIR/gameserver.py" "$GAME_PORT" \
        >"$BENCH_DIR/gameserver.out" 2>&1 &
    echo $! > "$BENCH_DIR/gameserver.pid"
    sleep 2
    ss -ltn "( sport = :$HTTP_PORT or sport = :$GAME_PORT )"
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
