#!/bin/sh
# Teardown counterpart of boot-enable.sh.
#
# Order matters: stop dispatching NEW flows to skyline_cc before draining,
# otherwise drain races against connections created while it waits.
#
# `ssctl drain` does exactly that itself: under the guard's lock it disarms the
# guard (config [guard]) and writes fallback_cc, and only then waits. So while
# the daemon is alive, drain goes first and nothing is written before it.
# Writing fallback_cc here first, as this script once did, left a window in
# which the still-armed guard saw a default that was not skyline_cc and put
# skyline_cc back, logging a misleading "something else on this host changed
# it".
#
# The sysctl write after it is not redundant: this path also has to work when
# the daemon is already gone and the control socket with it (or drain could not
# reach it), which is exactly when the machine would otherwise be left with
# skyline_cc as its default and nothing registered under that name. It only
# fires while the default is still skyline_cc, so after a drain that ran it
# leaves alone what drain wrote.
#
# What it writes is the daemon's own fallback_cc, read from the config the
# units start the daemon with (a top-level key, so before the first table):
# install.sh sets it to the algorithm the host boots into, and a hard-coded
# cubic here took a bbr host to cubic exactly when the daemon had died.
# SKYLINE_FALLBACK_CC still overrides it; cubic only when neither names one.
set -u
CONFIG=${SKYLINE_CONFIG:-/etc/skyline-speeder/speeder.toml}
SOCKET=${SKYLINE_SOCKET:-/run/skyline-speeder/speeder.sock}
FALLBACK=${SKYLINE_FALLBACK_CC:-$(sed -n -e '/^\[/q' \
    -e "s/^fallback_cc[[:space:]]*=[[:space:]]*[\"']\([A-Za-z0-9_-]*\)[\"'].*/\1/p" "$CONFIG" 2>/dev/null)}
FALLBACK=${FALLBACK:-cubic}

[ -S "$SOCKET" ] && /usr/local/bin/ssctl drain --timeout 60 >/dev/null 2>&1
if [ "$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null)" = skyline_cc ]; then
    sysctl -qw "net.ipv4.tcp_congestion_control=$FALLBACK" 2>/dev/null || true
fi
exit 0
