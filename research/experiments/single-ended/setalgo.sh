#!/usr/bin/env bash
# Point one destination prefix at one congestion control, the same way for every
# algorithm under test. brutal needs a rule (rate) plus the route; everything else
# needs only the route. Both land as proto 233 routes, so `ip route show proto 233`
# is the one place to read the current state and nothing else on the host changes.
#
#   DST=<client prefix> setalgo.sh <bbr|brutal|skyline_cc|cubic> [brutal_rate_mbps]
#
# skyline_policy.bpf.c (M1's dynamic RTO floor) is scoped to the
# /sys/fs/cgroup/skyline-speeder cgroup, NOT to the congestion control, and every
# benchmark sender lives in that cgroup. Left on, it would hand bbr and brutal the
# same RTO tuning and quietly understate the difference being measured, so it is
# switched with the algorithm: on for skyline_cc, off for everything else.
#
# The rack_rto values are config/speeder.toml's [rack_rto] block. They have to be
# sent explicitly: the daemon never applies [rack_rto] from its config file at
# startup -- RTO tuning stays zeroed until an explicit set-rack-rto
# (docs/usage.md, section 5) -- and the installed production template,
# speeder-guest.toml, leaves the block out on purpose, because the experiment
# matrix relies on reset-rack-rto meaning "off". The rto_max half is sent as 0:
# it needs TCP_RTO_MAX_MS (Linux 6.15+), and on the 6.12 kernel this was measured
# on every call was rejected. set-rack-rto is absolute-replace, so every field is
# sent on every call.
set -euo pipefail
export LC_ALL=C.UTF-8

DST=${DST:?set DST to the client prefix, e.g. DST=203.0.113.2/32}
ALGO=${1:?usage: DST=<prefix> setalgo.sh <bbr|brutal|skyline_cc|cubic> [rate_mbps]}
RATE=${2:-200}

brutalctl flush >/dev/null 2>&1 || true
ip route del "$DST" proto 233 2>/dev/null || true

# Reuse the next hop the kernel would pick anyway, as brutalctl does, so the
# override changes the congestion control and never the path.
route=$(ip -o route get "${DST%/*}")
GW=${GW:-$(awk '{for (i = 1; i < NF; i++) if ($i == "via") print $(i + 1)}' <<<"$route")}
DEV=${DEV:-$(awk '{for (i = 1; i < NF; i++) if ($i == "dev") print $(i + 1)}' <<<"$route")}

rack_rto_off() { ssctl set-rack-rto --disable >/dev/null; }

case "$ALGO" in
    brutal)
        rack_rto_off
        brutalctl add "$DST" "$RATE" >/dev/null
        ;;
    skyline_cc)
        ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 \
            --warmup-samples 4 --rto-max-normal-permille 0 \
            --rto-max-congested-permille 0 --rto-max-congestion-ratio-permille 0 >/dev/null
        ip route replace "$DST" ${GW:+via "$GW"} dev "$DEV" congctl lock "$ALGO" proto 233
        ;;
    *)
        rack_rto_off
        ip route replace "$DST" ${GW:+via "$GW"} dev "$DEV" congctl lock "$ALGO" proto 233
        ;;
esac
echo "algo=$ALGO dst=$DST rate=${RATE}Mbps"
