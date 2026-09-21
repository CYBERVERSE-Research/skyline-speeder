#!/usr/bin/env bash
# Head-to-head: bbr vs tcp-brutal vs skyline_cc, on one real path that drifts.
#
#   SERVER=root@sender CLIENT=root@receiver \
#   SERVER_ADDR=<server address as the client reaches it> \
#   CLIENT_PREFIX=<client address as the server sees it>/32 \
#       ./run-matrix.sh <new-output-dir>
#
# Needs key-based SSH to both hosts: BatchMode is on, because a password prompt
# would hang the matrix in the middle of a rotation. Prepare the server first with
# server-setup.sh. The client needs python3 and nothing else.
#
# Three things this design does on purpose:
#   1. Rotates the algorithm order every rotation (Latin square). With a fixed
#      order, a path that degrades within a rotation systematically favours
#      whoever goes first -- a bias that would otherwise be read as a result.
#   2. Probes the path before every rotation and records it next to the runs, so
#      drift is data rather than an unmeasured confound.
#   3. Bounds every bulk transfer with a deadline, so one collapsed run cannot eat
#      the schedule and starve the other algorithms of repetitions.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
: "${SERVER:?set SERVER=user@host for the sending side}"
: "${CLIENT:?set CLIENT=user@host for the receiving side}"
: "${SERVER_ADDR:?set SERVER_ADDR to the address the client uses to reach the server}"
: "${CLIENT_PREFIX:?set CLIENT_PREFIX to the client address as the server sees it, e.g. 203.0.113.2/32}"
ROT=${ROT:-8}
BRUTAL_MBPS=${BRUTAL_MBPS:-200}
BENCH_DIR=${BENCH_DIR:-/srv/skyline-bench}
CLIENT_BENCH=${CLIENT_BENCH:-/tmp/skyline-bench.py}
OUT=${1:?usage: run-matrix.sh <new-output-dir>}

# Same rule as run_matrix.py: results are never overwritten.
if [ -e "$OUT" ]; then
    echo "refusing to write into existing $OUT" >&2
    exit 1
fi
mkdir -p "$OUT"

SSH=(ssh -o BatchMode=yes -o ConnectTimeout=20)
srv() { "${SSH[@]}" "$SERVER" "$@"; }
cli() { "${SSH[@]}" "$CLIENT" "$@"; }

scp -q -o BatchMode=yes "$HERE/bench.py" "$CLIENT:$CLIENT_BENCH"

ALGOS=("bbr|bbr" "brutal|brutal $BRUTAL_MBPS" "skyline_cc|skyline_cc")

run_one() {
    local label=$1 algoargs=$2 scen=$3 rot=$4 rec ns
    srv "DST=$CLIENT_PREFIX $BENCH_DIR/setalgo.sh $algoargs" >/dev/null
    srv "nstat -n" >/dev/null
    sleep 1
    # A run that dies is recorded as missing rather than allowed to stop the
    # matrix: the other algorithms in this rotation still need their turn.
    rec=$(cli "SKYLINE_BENCH_HOST=$SERVER_ADDR python3 $CLIENT_BENCH $scen --label $label --rep $rot" \
        2>/dev/null | grep '^{' || true)
    ns=$(srv "nstat 2>/dev/null | awk 'NR>1 {printf \"%s=%s \", \$1, \$2}'" 2>/dev/null || true)
    if [ -z "$rec" ]; then
        echo "  FAIL $label/${scen%% *} rot$rot"
        return 0
    fi
    printf '%s\n' "$rec" | NS="$ns" python3 -c '
import json, os, sys
r = json.load(sys.stdin)
srv = {}
for kv in os.environ.get("NS", "").split():
    k, _, v = kv.partition("=")
    if v.lstrip("-").isdigit():
        srv[k] = int(v)
r["server"] = srv
print(json.dumps(r))' >> "$OUT/head-to-head.jsonl"
    echo "  ok   $label/${scen%% *} rot$rot"
}

for rot in $(seq 1 "$ROT"); do
    p=$(cli "ping -c 20 -i 0.2 -q $SERVER_ADDR 2>/dev/null | tail -2" 2>/dev/null || true)
    loss=$(printf '%s' "$p" | grep -o '[0-9.]*% packet loss' | grep -o '^[0-9.]*' || true)
    rtt=$(printf '%s' "$p" | awk -F'/' '/rtt|round-trip/ {print $5}')
    printf '{"rot":%d,"ts":%d,"loss_pct":%s,"rtt_avg_ms":%s}\n' \
        "$rot" "$(date +%s)" "${loss:-null}" "${rtt:-null}" >> "$OUT/path-probe.jsonl"
    echo "### rotation $rot/$ROT  $(date +%H:%M:%S)  rtt=${rtt:-?}ms loss=${loss:-?}%"

    n=${#ALGOS[@]}
    for k in $(seq 0 $((n - 1))); do
        a=${ALGOS[$(( (rot - 1 + k) % n ))]}
        label=${a%%|*}
        args=${a##*|}
        run_one "$label" "$args" "game --duration 30 --rate 60" "$rot"
        run_one "$label" "$args" "web --repeats 2" "$rot"
        run_one "$label" "$args" "video --chunks 15 --chunk-mb 6 --period 1.0" "$rot"
        run_one "$label" "$args" "bulk --mb 60 --streams 1 --deadline 90" "$rot"
        run_one "$label" "$args" "bulk --mb 60 --streams 4 --deadline 90" "$rot"
    done
done
echo "### done $(date +%H:%M:%S): $(wc -l < "$OUT/head-to-head.jsonl") records in $OUT"
