#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--cc cubic] [--rtt-ms 80] [--loss-pct 0.5] [--rate-mbit 100] [--duration 5] [--runs 5] [--parallel 1] [--ipv6] [--output results.csv]"
}

CC=cubic
RTT_MS=80
LOSS_PCT=0
RATE_MBIT=100
DURATION=5
RUNS=5
PARALLEL=1
OUTPUT=results.csv
INSIDE=0
IPV6=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --cc) CC=$2; shift 2 ;;
        --rtt-ms) RTT_MS=$2; shift 2 ;;
        --loss-pct) LOSS_PCT=$2; shift 2 ;;
        --rate-mbit) RATE_MBIT=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --runs) RUNS=$2; shift 2 ;;
        --parallel) PARALLEL=$2; shift 2 ;;
        --output) OUTPUT=$2; shift 2 ;;
        --inside) INSIDE=1; shift ;;
        # Mirrors infra/topology.sh's fd20:1::/64 (server-side) / fd20:2::/64
        # (client-side) ULA numbering, purely for a quick local dual-stack
        # smoke check -- v4 stays the default so every existing invocation
        # (and CI usage) is unaffected.
        --ipv6) IPV6=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

for command in unshare nsenter ip tc iperf3 jq sysctl; do
    command -v "$command" >/dev/null || {
        echo "Missing required command: $command" >&2
        exit 1
    }
done

if [ "$INSIDE" -eq 0 ]; then
    mkdir -p "$(dirname "$OUTPUT")"
    OUTPUT=$(realpath -m "$OUTPUT")
    EXTRA_ARGS=()
    [ "$IPV6" -eq 1 ] && EXTRA_ARGS+=(--ipv6)
    exec unshare --user --map-root-user --net "$0" \
        --inside --cc "$CC" --rtt-ms "$RTT_MS" --loss-pct "$LOSS_PCT" \
        --rate-mbit "$RATE_MBIT" --duration "$DURATION" --runs "$RUNS" \
        --parallel "$PARALLEL" --output "$OUTPUT" "${EXTRA_ARGS[@]}"
fi

case " $(sysctl -n net.ipv4.tcp_available_congestion_control) " in
    *" $CC "*) ;;
    *)
        echo "Congestion control '$CC' is unavailable in this kernel namespace" >&2
        exit 1
        ;;
esac

CLIENT_PID=
SERVER_PID=
IPERF_PID=
JSON_RESULT=

cleanup() {
    [ -n "${IPERF_PID:-}" ] && kill "$IPERF_PID" 2>/dev/null || true
    [ -n "${CLIENT_PID:-}" ] && kill "$CLIENT_PID" 2>/dev/null || true
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

unshare --net sh -c 'exec sleep 86400' & CLIENT_PID=$!
unshare --net sh -c 'exec sleep 86400' & SERVER_PID=$!
sleep 0.1

ip link add cr type veth peer name cc
ip link add sr type veth peer name ss
ip link set cc netns "$CLIENT_PID"
ip link set ss netns "$SERVER_PID"

ip addr add 10.20.1.1/24 dev cr
ip addr add 10.20.2.1/24 dev sr
ip link set lo up
ip link set cr up
ip link set sr up

nsenter -t "$CLIENT_PID" -n ip addr add 10.20.1.2/24 dev cc
nsenter -t "$CLIENT_PID" -n ip link set lo up
nsenter -t "$CLIENT_PID" -n ip link set cc up
nsenter -t "$CLIENT_PID" -n ip route add default via 10.20.1.1

nsenter -t "$SERVER_PID" -n ip addr add 10.20.2.2/24 dev ss
nsenter -t "$SERVER_PID" -n ip link set lo up
nsenter -t "$SERVER_PID" -n ip link set ss up
nsenter -t "$SERVER_PID" -n ip route add default via 10.20.2.1

# One packet per skb at both ends, or --loss-pct is not a packet-loss rate.
# TCP builds GSO batches of up to 64 KiB whatever a device's offloads say
# (sk_setup_caps() gives every TCP socket NETIF_F_GSO), a veth hands them on
# whole, and so the netem on the router's far side drops a batch at a time:
# measured on 6.12 at 0.5% loss, 8.4 segments per drop instead of 1, and
# far fewer loss events for the same loss rate. With gso_max_segs 1 TCP
# builds one-segment skbs from the start -- what the two-VM test bed gets
# from offload = "off" (docs/04-performance-report.md section 2.3).
nsenter -t "$CLIENT_PID" -n ip link set dev cc gso_max_segs 1
nsenter -t "$SERVER_PID" -n ip link set dev ss gso_max_segs 1

sysctl -qw net.ipv4.ip_forward=1

if [ "$IPV6" -eq 1 ]; then
    # Same fd20:1::/64 (server side) / fd20:2::/64 (client side) ULA
    # numbering as infra/topology.sh, added alongside the v4 addresses
    # above (dual-stack), not instead of them.
    #
    # accept_dad=0 first: these are point-to-point veth links inside a
    # brand-new netns with addresses this script itself just picked, so
    # Duplicate Address Detection has nothing real to detect -- without
    # this, an address stays "tentative" (unusable for outbound traffic)
    # until DAD's neighbor-solicitation round trip completes, and iperf3
    # connecting immediately after `ip addr add` loses that race often
    # enough to hang until it disconnects with "the client has terminated".
    # Per-interface, not just .all -- accept_dad is latched per-interface
    # at the time each link came up (already "up" from the v4 setup above,
    # before .all was touched here), so only .all wouldn't reliably apply.
    sysctl -qw net.ipv6.conf.cr.accept_dad=0 net.ipv6.conf.sr.accept_dad=0
    nsenter -t "$CLIENT_PID" -n sysctl -qw net.ipv6.conf.cc.accept_dad=0
    nsenter -t "$SERVER_PID" -n sysctl -qw net.ipv6.conf.ss.accept_dad=0
    ip -6 addr add fd20:1::1/64 dev cr
    ip -6 addr add fd20:2::1/64 dev sr
    nsenter -t "$CLIENT_PID" -n ip -6 addr add fd20:1::2/64 dev cc
    nsenter -t "$CLIENT_PID" -n ip -6 route add default via fd20:1::1
    nsenter -t "$SERVER_PID" -n ip -6 addr add fd20:2::2/64 dev ss
    nsenter -t "$SERVER_PID" -n ip -6 route add default via fd20:2::1
    sysctl -qw net.ipv6.conf.all.forwarding=1
fi

# Half of the requested RTT is placed on each router egress. LOSS_PCT is a
# per-direction packet-loss probability, so ACK loss is included as it would be
# on a symmetric impaired path.
HALF_RTT=$(awk -v rtt="$RTT_MS" 'BEGIN { printf "%.3f", rtt / 2.0 }')
BDP_PACKETS=$(awk -v rate="$RATE_MBIT" -v rtt="$RTT_MS" 'BEGIN {
    packets = (rate * 1000000 * rtt / 1000) / (8 * 1500)
    limit = int(packets * 2 + 100)
    if (limit < 1000) limit = 1000
    print limit
}')

tc qdisc add dev cr root netem limit "$BDP_PACKETS" delay "${HALF_RTT}ms" \
    loss random "${LOSS_PCT}%" rate "${RATE_MBIT}mbit"
tc qdisc add dev sr root netem limit "$BDP_PACKETS" delay "${HALF_RTT}ms" \
    loss random "${LOSS_PCT}%" rate "${RATE_MBIT}mbit"

if [ ! -s "$OUTPUT" ]; then
    echo 'timestamp,cc,rtt_ms,loss_pct,rate_mbit,parallel,duration_s,run,sender_mbps,receiver_mbps,retransmits,mean_rtt_ms,sender_cpu_pct,receiver_cpu_pct' > "$OUTPUT"
fi

SERVER_DATA_ADDR=10.20.2.2
IPERF_FAMILY_FLAG=-4
if [ "$IPV6" -eq 1 ]; then
    SERVER_DATA_ADDR=fd20:2::2
    IPERF_FAMILY_FLAG=-6
fi

for run in $(seq 1 "$RUNS"); do
    nsenter -t "$SERVER_PID" -n iperf3 -s -1 "$IPERF_FAMILY_FLAG" >/dev/null 2>&1 & IPERF_PID=$!
    sleep 0.15
    JSON_RESULT=$(nsenter -t "$CLIENT_PID" -n iperf3 -c "$SERVER_DATA_ADDR" "$IPERF_FAMILY_FLAG" -C "$CC" \
        -t "$DURATION" -P "$PARALLEL" -J)
    wait "$IPERF_PID"
    IPERF_PID=

    timestamp=$(jq -r '.start.timestamp.time' <<<"$JSON_RESULT")
    sender_mbps=$(jq -r '(.end.sum_sent.bits_per_second // 0) / 1000000' <<<"$JSON_RESULT")
    receiver_mbps=$(jq -r '(.end.sum_received.bits_per_second // 0) / 1000000' <<<"$JSON_RESULT")
    retransmits=$(jq -r '.end.sum_sent.retransmits // 0' <<<"$JSON_RESULT")
    mean_rtt_ms=$(jq -r '([.end.streams[].sender.mean_rtt // empty] | if length == 0 then 0 else add / length / 1000 end)' <<<"$JSON_RESULT")
    sender_cpu=$(jq -r '.end.cpu_utilization_percent.host_total // 0' <<<"$JSON_RESULT")
    receiver_cpu=$(jq -r '.end.cpu_utilization_percent.remote_total // 0' <<<"$JSON_RESULT")

    printf '"%s",%s,%s,%s,%s,%s,%s,%s,%.6f,%.6f,%s,%.6f,%.6f,%.6f\n' \
        "$timestamp" "$CC" "$RTT_MS" "$LOSS_PCT" "$RATE_MBIT" "$PARALLEL" \
        "$DURATION" "$run" "$sender_mbps" "$receiver_mbps" "$retransmits" \
        "$mean_rtt_ms" "$sender_cpu" "$receiver_cpu" >> "$OUTPUT"

    JSON_RESULT=
done
