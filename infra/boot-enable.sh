#!/bin/sh
# Boot-time activation for Skyline Speeder.
#
# skyline-speederd.service only keeps the daemon resident: it deliberately does NOT
# attach the skyline_cc struct_ops, because attaching is an explicit operator
# action (see docs/01-deployment-guide.md section 7). Without this step a reboot
# comes back with the daemon running and every new flow on `fallback_cc` -- a
# silent regression, since nothing reports an error.
#
# `ssctl enable` owns all of activation: it attaches the struct_ops, then
# switches net.ipv4.tcp_congestion_control to skyline_cc, and -- with [guard]
# qdisc = true, the default -- sets net.core.default_qdisc=fq and puts fq on the
# root of runtime.tc_interface (or, when that is a VLAN, bond or bridge, of the
# NICs under it). skyline-speederd then keeps all of it in place until the next
# drain. This script therefore writes NO sysctl: two places writing the same
# setting is how they drift apart. (It used to write default_qdisc=fq itself,
# once, here. That only shapes qdiscs created afterwards, so the NIC kept
# whatever root qdisc it had come up with, and a later `sysctl --system`
# re-applying a "one-click BBR" file undid it anyway.)
#
# skyline-speederd.service has no systemd readiness notification, so the control
# socket may not exist yet when this runs; wait for it rather than racing it.
set -eu

SOCKET=${SKYLINE_SOCKET:-/run/skyline-speeder/speeder.sock}
TIMEOUT=${SKYLINE_SOCKET_TIMEOUT:-60}
DEADLINE=$(( $(date +%s) + TIMEOUT ))

while [ ! -S "$SOCKET" ]; do
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "timed out after ${TIMEOUT}s waiting for $SOCKET" >&2
        exit 1
    fi
    sleep 1
done

/usr/local/bin/ssctl enable >/dev/null

# Read the sysctl back rather than trusting the exit status: this is the line
# that would catch `ssctl enable` succeeding while the default silently stayed
# where it was.
ACTIVE=$(sysctl -n net.ipv4.tcp_congestion_control)
if [ "$ACTIVE" != skyline_cc ]; then
    echo "ssctl enable returned success but the default congestion control is '$ACTIVE'" >&2
    exit 1
fi

echo "Skyline Speeder enabled: cc=$ACTIVE default_qdisc=$(sysctl -n net.core.default_qdisc)"
