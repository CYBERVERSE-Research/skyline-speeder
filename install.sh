#!/usr/bin/env bash
#
# Skyline Speeder -- one-click installer for Debian / Ubuntu.
#
# Exclusively sponsored by Skyline Connect -- https://www.skylineconnect.io
#
# Installs the build toolchain, compiles the three CO-RE BPF objects against
# THIS machine's kernel BTF, builds the Rust control plane, installs the systemd
# units, and activates skyline_cc across reboots. Installs no proxy, no network
# service, and opens no port.
#
#   sudo ./install.sh                  # build from source, install and activate
#   sudo ./install.sh --prebuilt       # install published artifacts, no toolchain
#   sudo ./install.sh --release <tag>  # --prebuilt, pinned to that release
#   sudo ./install.sh --check          # preflight only, change nothing
#   sudo ./install.sh --no-enable      # install, but attach nothing that was not attached
#   sudo ./install.sh --verbose        # show every command's output (or SKYLINE_VERBOSE=1)
#   sudo ./install.sh --uninstall      # remove it, put this host on bbr + fq, and
#                                      # remove the packages this installer added
#   sudo ./install.sh --uninstall --restore-pre-install
#                                      # ... but restore the cc/qdisc from before
#                                      # the install instead of bbr + fq
#
# The terminal shows one progress line. Everything the steps print goes to
# /var/log/skyline-speeder-install.log, which is kept; if a step fails, its
# last lines are shown together with that path.
#
# --uninstall removes the build toolchain it installed (clang, LLVM, bpftool,
# rustup and what came with them) and nothing else: only packages recorded as
# added by an install of this host, never iproute2, curl, ca-certificates or
# tar, never anything one of those still needs, and never one dpkg calls
# required or important. apt plans the removal first; a package something else
# on this host now needs is kept, named, and the rest is removed, and nothing
# at all is removed when no reduced plan stays inside the recorded list. It
# keeps /etc/skyline-speeder. bbr + fq is set for that boot; no file under
# /etc/sysctl.d is written or edited, so those files decide again after a
# reboot.
#
# Running it again on an installed host upgrades it: the new objects must pass
# the kernel verifier first, and only then is skyline-speederd restarted (live
# flows are drained for up to 60 s, then skyline_cc is attached again).
#
# Once skyline_cc is attached, skyline-speederd also keeps fq as the qdisc on
# the egress NIC (config [guard]). A bbr / cake / fq_pie left behind by a
# "one-click BBR" script is reported and overridden; its files are not edited.
#
# --prebuilt downloads the release artifacts instead of compiling. It needs no
# clang, no LLVM, no bpftool and no Rust: the BPF objects were built against a
# pinned reference header from the oldest supported kernel, and CO-RE fixes the
# field offsets against THIS kernel when they load. Everything else about the
# install is identical.
#
set -euo pipefail

# Read before the pin below: the progress spinner draws braille dots only when
# the caller's own locale says the terminal speaks UTF-8.
CALLER_LOCALE=${LC_ALL:-${LC_CTYPE:-${LANG:-}}}

# A caller's LC_ALL often names a locale this machine does not have generated,
# and apt/perl then emit a screenful of "Setting locale failed" before doing the
# work correctly anyway. Pin it so installer output is deterministic and the
# real messages are not buried.
export LC_ALL=C.UTF-8 LANG=C.UTF-8 LANGUAGE=

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
KVER="$(uname -r)"
MODE=install
ENABLE=1
SOURCE=build          # build | prebuilt
RELEASE_TAG=          # empty means "latest"
RESTORE=bbr-fq        # bbr-fq | pre-install: what --uninstall leaves behind
REPO_SLUG=${SKYLINE_REPO:-CYBERVERSE-Research/skyline-speeder}
case "${SKYLINE_VERBOSE:-0}" in 1|yes|true) VERBOSE=1 ;; *) VERBOSE=0 ;; esac
LOG=/var/log/skyline-speeder-install.log
CFG=/etc/skyline-speeder/speeder.toml
STATE=/etc/skyline-speeder/pre-install-state
# What an install added to this host, so --uninstall can take exactly that
# away again and nothing else. Separate files, not more keys in $STATE: that
# one is written once and then closed forever (it must never record
# skyline_cc as the "original"), while a later upgrade run can add packages.
ADDED_PKGS=/etc/skyline-speeder/added-packages
ADDED_RUSTUP=/etc/skyline-speeder/added-rustup
# Recorded as added when they were, but never removed again: iproute2 and
# curl are how an operator reaches the network and reads a qdisc, tar and
# ca-certificates are what --prebuilt needed and what half the host uses.
# A package dpkg calls required or important is skipped for the same reason,
# whatever its name.
KEEP_PKGS=(iproute2 curl ca-certificates tar)
ADDED_COUNT=0      # packages recorded in $ADDED_PKGS after this run
ADDED_REMOVABLE=0  # of those, the ones an uninstall would actually remove
GUIDE_URL=https://github.com/CYBERVERSE-Research/skyline-speeder/blob/main/docs/usage.md

# `-qq` silences apt but not dpkg, which still prints an unpack line per
# package plus a "Reading database" progress bar. Dpkg::Use-Pty=0 stops the
# progress redraw; the log keeps the detail for when something actually fails.
# A fresh cloud VM is often still running unattended-upgrades: wait for the
# dpkg lock instead of failing on it. confdef/confold answer a changed-conffile
# prompt the way an operator almost always would -- keep their file -- since
# nobody can answer it from behind a progress bar. Up here rather than beside
# the package step, because --uninstall removes packages with it too.
APT_OPTS=(-y -qq -o Dpkg::Use-Pty=0 -o DPkg::Lock::Timeout=300
          -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold)

# --- output ----------------------------------------------------------------
# An install used to scroll a screenful of apt, rustup, make and cargo output
# past the one line that mattered. Now the terminal gets a single progress line
# redrawn in place (or, when stdout is not a terminal, one plain line as each
# step starts and ends), plus warnings, the summary and the guide. Everything
# the steps print goes to $LOG.
#
# fd 3 and 4 are the terminal's stdout and stderr, saved here so progress,
# warnings and errors still reach the operator from inside a block whose output
# is redirected into the log.
exec 3>&1 4>&2
MAIN_PID=$$
if [ -t 1 ]; then TTY=1; else TTY=0; fi
if [ "$TTY" -eq 1 ] && [ "${TERM:-}" != dumb ]; then
    CSI=$'\033'; RED="${CSI}[31m"; GRN="${CSI}[32m"; YLW="${CSI}[33m"; BLD="${CSI}[1m"; RST="${CSI}[0m"
else
    # Piped into a file or a CI log, or a terminal that cannot do it: no
    # escape codes at all.
    RED= GRN= YLW= BLD= RST=
fi
case "$CALLER_LOCALE" in
    *[Uu][Tt][Ff]-8*|*[Uu][Tt][Ff]8*) SPIN=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏) ;;
    *) SPIN=('|' '/' '-' "\\") ;;
esac
QUIET=0         # 1: step messages go to $LOG only (install mode without --verbose)
BAR=0           # 1: the redrawn progress line is in use (QUIET on a terminal)
LOG_READY=0     # 1: $LOG has been started for this run
BAR_SHOWN=0     # 1: the progress line is on screen and must be cleared first
STEP_NO=0 STEP_TOTAL=0 STEP_NAME= STEP_T0=0 STEP_OPEN=0 STEP_RAN=0 STEP_LOG_LINE=0
SPIN_I=0 COLS=80 BG_PID= WORK= FAILED=0 INTERRUPTED=0 APT_SIM=

# Skyline Speeder has no revenue of its own. Every run of this script, and
# every ssctl command, names who funds it -- once on the way in and once on
# the way out, so it is there whether the operator watches the whole run or
# only its last screen.
SPONSOR_TEXT='Exclusively sponsored by Skyline Connect'
SPONSOR_URL='https://www.skylineconnect.io'
sponsor() { printf ' %s %s  %s%s%s\n' "$1" "$SPONSOR_TEXT" "$BLD" "$SPONSOR_URL" "$RST" >&3; }

log()  { if [ "$LOG_READY" -eq 1 ]; then printf '%s\n' "$*" >>"$LOG"; fi; }
info() { log "==> $*"; [ "$QUIET" -eq 1 ] || printf '%s==>%s %s\n' "$BLD" "$RST" "$*" >&3; }
ok()   { log "  ok  $*"; [ "$QUIET" -eq 1 ] || printf '%s  ok%s  %s\n' "$GRN" "$RST" "$*" >&3; }
warn() { log " warn $*"; bar_clear; printf '%s warn%s %s\n' "$YLW" "$RST" "$*" >&4; bar_draw; }

die() {
    if [ "${BASHPID:-$$}" != "$MAIN_PID" ]; then
        # A subshell (a command substitution, a background job) does not own
        # the terminal. Leave the message for the main shell, which reports it
        # when it sees this step fail.
        if [ -n "$WORK" ]; then printf '%s\n' "$*" >"$WORK/failure"; fi
        exit 1
    fi
    fail_report "$*" "$STEP_RAN"
    exit 1
}

# fail_report <message> <show-log-tail: 0|1>
fail_report() {
    FAILED=1
    bar_clear
    printf '%serror%s %s\n' "$RED" "$RST" "$1" >&4
    [ "$LOG_READY" -eq 1 ] || return 0
    local lines
    lines=$(wc -l <"$LOG" 2>/dev/null || echo 0)
    # Only what the failing step itself wrote, and not twice under --verbose,
    # where it has just scrolled past.
    if [ "${2:-0}" -eq 1 ] && [ "$VERBOSE" -eq 0 ] && [ "$lines" -gt "$STEP_LOG_LINE" ]; then
        printf '\n  last lines of the log:\n' >&4
        { tail -n +"$((STEP_LOG_LINE + 1))" "$LOG" | tail -n 25 | sed 's/^/    /' >&4; } || true
    fi
    printf '\n  full log: %s\n' "$LOG" >&4
    log "error $1"
}

term_cols() {
    local c
    c=$(tput cols 2>/dev/null || true)
    case "$c" in ''|*[!0-9]*) c=80 ;; esac
    [ "$c" -ge 20 ] || c=20
    COLS=$c
}

# fmt_secs <seconds>: "42s" or "3m05s" into REPLY (no subshell: this runs
# several times a second while a build is in progress).
fmt_secs() {
    if [ "$1" -ge 60 ]; then
        printf -v REPLY '%dm%02ds' $(($1 / 60)) $(($1 % 60))
    else
        REPLY="${1}s"
    fi
}

# [#########-----------]  45%  (5/11) Building the Rust control plane   1m12s ⠼
# The step that is running does not count as done yet.
bar_draw() {
    [ "$BAR" -eq 1 ] && [ "$STEP_TOTAL" -gt 0 ] || return 0
    local done=$((STEP_NO - STEP_OPEN)) fill hashes dashes head tail name=$STEP_NAME room
    fill=$((done * 20 / STEP_TOTAL))
    printf -v hashes '%*s' "$fill" ''
    printf -v dashes '%*s' "$((20 - fill))" ''
    printf -v head '[%s%s] %3d%%  (%d/%d) ' "${hashes// /#}" "${dashes// /-}" \
        $((done * 100 / STEP_TOTAL)) "$STEP_NO" "$STEP_TOTAL"
    fmt_secs $((SECONDS - STEP_T0))
    tail="   $REPLY ${SPIN[SPIN_I % ${#SPIN[@]}]}"
    # One column short of the width (some terminals wrap on the last one), and
    # the step name is what gives way: the clock and the spinner are what show
    # a long step is still alive.
    room=$((COLS - 1 - ${#head} - ${#tail}))
    if [ "${#name}" -gt "$room" ]; then
        if [ "$room" -gt 3 ]; then name="${name:0:room-3}"; name="${name%[ ,]}..."; else name=; fi
    fi
    head="$head$name$tail"
    printf '\r\033[K%s' "${head:0:COLS-1}" >&3
    BAR_SHOWN=1
}

bar_clear() {
    [ "$BAR_SHOWN" -eq 1 ] || return 0
    printf '\r\033[K' >&3
    BAR_SHOWN=0
}

step() {
    step_end
    STEP_NO=$((STEP_NO + 1)) STEP_NAME=$1 STEP_T0=$SECONDS STEP_OPEN=1 STEP_RAN=0
    log ""
    log "=== ($STEP_NO/$STEP_TOTAL) $STEP_NAME  [$(date '+%F %T')]"
    STEP_LOG_LINE=$(wc -l <"$LOG")
    if [ "$BAR" -eq 1 ]; then
        term_cols
        bar_draw
    elif [ "$QUIET" -eq 1 ]; then
        printf '[%2d/%d] %s\n' "$STEP_NO" "$STEP_TOTAL" "$STEP_NAME" >&3
    else
        printf '%s==>%s (%d/%d) %s\n' "$BLD" "$RST" "$STEP_NO" "$STEP_TOTAL" "$STEP_NAME" >&3
    fi
}

step_end() {
    [ "$STEP_OPEN" -eq 1 ] || return 0
    STEP_OPEN=0
    fmt_secs $((SECONDS - STEP_T0))
    log "=== ($STEP_NO/$STEP_TOTAL) done in $REPLY"
    if [ "$BAR" -eq 0 ] && [ "$QUIET" -eq 1 ]; then
        printf '[%2d/%d] done in %s\n' "$STEP_NO" "$STEP_TOTAL" "$REPLY" >&3
    fi
}

# run <command...>: one EXTERNAL command, its output in $LOG (and on screen
# under --verbose). On a terminal it runs in the background so the progress
# line keeps moving through a multi-minute build. Never a shell function: in
# the background that runs in a subshell, nothing it sets survives, and since
# every caller writes `run ... || die`, `set -e` would be silently off in it.
#
# setsid: a background job of a non-interactive shell ignores SIGINT, so Ctrl-C
# would stop the installer and leave cargo building behind the operator's back.
# In a session of its own the whole job can be killed as one group instead.
run() {
    STEP_RAN=1
    if [ "$VERBOSE" -eq 1 ]; then
        "$@" </dev/null 3>&- 4>&- 2>&1 | tee -a "$LOG" >&3
        return
    fi
    if [ "$BAR" -eq 0 ]; then
        "$@" </dev/null >>"$LOG" 2>&1 3>&- 4>&-
        return
    fi
    if command -v setsid >/dev/null 2>&1; then
        setsid -w "$@" </dev/null >>"$LOG" 2>&1 3>&- 4>&- &
    else
        "$@" </dev/null >>"$LOG" 2>&1 3>&- 4>&- &
    fi
    BG_PID=$!
    while kill -0 "$BG_PID" 2>/dev/null; do
        SPIN_I=$((SPIN_I + 1))
        [ $((SPIN_I % 25)) -ne 0 ] || term_cols
        bar_draw
        sleep 0.2
    done
    local rc=0
    wait "$BG_PID" || rc=$?
    BG_PID=
    return "$rc"
}

# quietly <function>: a shell function's output into $LOG, in THIS shell, so
# `set -e` stays in force and what it sets is still there afterwards.
quietly() {
    STEP_RAN=1
    if [ "$VERBOSE" -eq 1 ]; then "$@"; else "$@" >>"$LOG" 2>&1; fi
}

on_interrupt() {
    trap - INT TERM
    if [ -n "$BG_PID" ]; then
        kill -TERM -- "-$BG_PID" 2>/dev/null || kill -TERM "$BG_PID" 2>/dev/null || true
    fi
    INTERRUPTED=1
    exit 130
}

on_exit() {
    local rc=$?
    set +e
    if [ "$rc" -ne 0 ] && [ "$FAILED" -eq 0 ] && [ "$LOG_READY" -eq 1 ]; then
        if [ "$INTERRUPTED" -eq 1 ]; then
            fail_report "interrupted during step $STEP_NO/$STEP_TOTAL ($STEP_NAME); nothing after it ran" 0
        elif [ -s "$WORK/failure" ]; then
            fail_report "$(cat "$WORK/failure")" 1
        else
            # `set -e` stopped the script on a command that had no `|| die`.
            fail_report "step $STEP_NO/$STEP_TOTAL ($STEP_NAME) failed (exit status $rc)" 1
        fi
    fi
    bar_clear
    [ "$BAR" -eq 0 ] || printf '\033[?25h' >&3
    [ -z "$WORK" ] || rm -rf "$WORK"
    [ -z "$APT_SIM" ] || rm -f "$APT_SIM"
}
trap on_exit EXIT
trap on_interrupt INT TERM

# --- what is in place now ----------------------------------------------------
# Root qdisc of an interface as one word: "fq", "cake", "fq_codel", or "mq/"
# followed by the distinct kinds under an mq root, sorted ("mq/fq_codel",
# "mq/cake,fq"). The same format `ssctl status --json` shows as guard.live.
# interface_qdisc. A default mq has handle 0 and its children print as
# "parent :N"; one created by tc has a real handle and "parent 8001:N".
# clsact/ingress hang off ffff:fff1 / ffff:, not the root, and are not part of
# the answer -- skyline_tc lives on clsact. Empty when `tc` is missing or the
# interface does not exist.
root_qdisc_summary() {
    [ -n "${1:-}" ] && [ -e "/sys/class/net/$1" ] && command -v tc >/dev/null 2>&1 || return 0
    tc qdisc show dev "$1" 2>/dev/null | awk '
        $1 != "qdisc" { next }
        $4 == "root"   { root = $2; handle = $3; next }
        $4 == "parent" { n++; kind[n] = $2; parent[n] = $5 }
        END {
            if (root == "") exit
            if (root != "mq") { print root; exit }
            prefix = (handle == "0:") ? ":" : handle
            for (i = 1; i <= n; i++)
                if (index(parent[i], prefix) == 1 && !seen[kind[i]]++) list[++m] = kind[i]
            # Insertion sort: mawk, the default awk on Debian, has no asort().
            for (i = 2; i <= m; i++) {
                v = list[i]
                for (j = i - 1; j >= 1 && list[j] > v; j--) list[j + 1] = list[j]
                list[j + 1] = v
            }
            out = "mq"
            for (i = 1; i <= m; i++) out = out (i == 1 ? "/" : ",") list[i]
            print out
        }' | tr -cd 'A-Za-z0-9_/,.-' || true
}

# A cake that shapes -- `bandwidth` other than unlimited, or autorate-ingress
# -- at the root or directly under the root mq: its option, e.g. "bandwidth
# 90Mbit". The guard leaves such a cake alone like htb (guard.rs): somebody
# set a rate on purpose. An unshaped cake is what a "one-click BBR" script
# leaves behind, and that one is replaced.
root_qdisc_shaping() {
    [ -n "${1:-}" ] && [ -e "/sys/class/net/$1" ] && command -v tc >/dev/null 2>&1 || return 0
    tc qdisc show dev "$1" 2>/dev/null | awk '
        $1 != "qdisc" { next }
        $4 == "root" { root = $2; handle = $3 }
        $2 == "cake" {
            s = ""
            for (i = 5; i <= NF; i++) {
                if ($i == "bandwidth" && i < NF && $(i + 1) != "unlimited") s = "bandwidth " $(i + 1)
                if ($i == "autorate-ingress" && s == "") s = "autorate-ingress"
            }
            if (s != "") { n++; at[n] = ($4 == "root") ? "root" : $5; opt[n] = s }
        }
        END {
            prefix = (handle == "0:") ? ":" : handle
            for (i = 1; i <= n; i++)
                if (at[i] == "root" || (root == "mq" && index(at[i], prefix) == 1)) { print opt[i]; exit }
        }' | tr -cd 'A-Za-z0-9_., -' || true
}

# Whether the guard replaces a root qdisc with this summary rather than leave
# it alone: the root, or every child under mq, is fq or one of guard.rs's
# REPLACEABLE_KINDS (keep the two lists in step). A shaped cake is the
# exception root_qdisc_shaping finds.
qdisc_replaceable() {
    local -a kinds
    local k
    case "$1" in
        mq/*) IFS=, read -ra kinds <<<"${1#mq/}" ;;
        mq|'') return 1 ;;
        *) kinds=("$1") ;;
    esac
    for k in "${kinds[@]}"; do
        case "$k" in
            fq|pfifo_fast|pfifo|bfifo|pfifo_head_drop|fq_codel|codel|cake|fq_pie|pie|sfq|red|sfb|choke|hhf) ;;
            *) return 1 ;;
        esac
    done
}

# Every device a default route leaves through, one per line: IPv4 routes
# first, then IPv6 (`ip route` lists IPv4 only, and an IPv6-only host has no
# IPv4 default at all), each in the order ip prints them; a multipath route
# names several. Only unicast defaults ("unreachable default dev lo" is not a
# way out), each device once.
default_route_devices() {
    { ip -o route show default 2>/dev/null || true
      ip -6 -o route show default 2>/dev/null || true; } \
        | awk '$1 == "default" {
                   for (i = 2; i < NF; i++)
                       if ($i == "dev" && $(i + 1) != "lo" && !seen[$(i + 1)]++) print $(i + 1)
               }' || true
}

default_route_interface() {
    default_route_devices | awk 'NR == 1' || true
}

# The device a fresh install points runtime.tc_interface at: the first
# default-route device that is Ethernet (/sys/class/net/<d>/type 1,
# ARPHRD_ETHER; a VLAN, bond or bridge is too). skyline_tc parses an Ethernet
# header at offset 0, and the daemon refuses anything else. Behind an L3
# tunnel (WireGuard/WARP, tun, gre, ppp) the NIC whose qdisc matters is not
# the route's device, so that case is left to the operator.
ethernet_route_interface() {
    local d
    while IFS= read -r d; do
        if [ "$(cat "/sys/class/net/$d/type" 2>/dev/null || true)" = 1 ]; then
            printf '%s\n' "$d"
            return 0
        fi
    done < <(default_route_devices)
}

configured_interface() {
    awk -F'"' '/^tc_interface[[:space:]]*=/ { print $2; exit }' "$CFG" 2>/dev/null || true
}

# The interface skyline-speederd will guard: runtime.tc_interface, as it will
# read once the fix-up below has replaced the template's placeholder. Empty when
# an existing config has no tc_interface (then no interface is touched).
planned_interface() {
    if [ -r "$CFG" ]; then
        local dev
        dev=$(configured_interface)
        if [ "$dev" != data0 ]; then printf '%s' "$dev"; return 0; fi
    fi
    ethernet_route_interface
}

# managed_devices <tc_interface>: the devices whose root qdisc the guard
# manages, one "device|root qdisc summary|label" line each. This MIRRORS
# managed_devices() in crates/skyline-speederd/src/guard.rs; keep the two in
# step:
#   - tc_interface's root is not noqueue: tc_interface itself.
#   - it is noqueue (the kernel default on a VLAN, bond, bridge, macvlan or
#     WireGuard device, where it says nothing about intent): follow
#     /sys/class/net/<d>/lower_* down, at most 4 levels, each device once.
#     Through another noqueue device keep going. A lower with any other root
#     AND a `device` link -- a physical or virtio NIC: a bond slave, a VLAN's
#     real device, a bridge's physical port -- is managed. One without that
#     link (tap, veth, ifb: a VM's or a container's port) never is; it is not
#     ours. A noqueue device with no lowers but a `device` link is a NIC
#     somebody set to noqueue: managed, and so reported as left alone.
# The label names the path, nearest first: "eth0 (under bond0 under vmbr0)".
# Nothing for tc_interface empty or missing, for a noqueue device with no NIC
# visible under it (wg, a bridge of taps only), or for a device whose
# root cannot be read here: it would have nothing to record.
managed_devices() {
    local top=${1:-} root
    [ -n "$top" ] || return 0
    root=$(root_qdisc_summary "$top")
    case "$root" in
        '') return 0 ;;
        noqueue) ;;
        *) printf '%s|%s|%s\n' "$top" "$root" "$top"; return 0 ;;
    esac
    local -A md_seen=(["$top"]=1)
    managed_under "$top" "" 0
    return 0
}

# managed_under <noqueue device> <devices above it, nearest first> <depth>:
# managed_devices' walk. md_seen is its caller's (bash scoping is dynamic).
managed_under() {
    local dev=$1 uppers=$2 depth=$3 below l x xr
    local -a lowers=()
    for l in "/sys/class/net/$dev"/lower_*; do
        [ -e "$l" ] || [ -L "$l" ] || continue      # the glob matched nothing
        lowers+=("${l##*/lower_}")
    done
    if [ "${#lowers[@]}" -eq 0 ]; then
        if [ -n "$uppers" ] && [ -e "/sys/class/net/$dev/device" ]; then
            printf '%s|noqueue|%s (under %s)\n' "$dev" "$dev" "$uppers"
        elif [ -e "/sys/class/net/$dev/device" ]; then
            printf '%s|noqueue|%s\n' "$dev" "$dev"
        fi
        return 0
    fi
    [ "$depth" -lt 4 ] || return 0
    below="$dev${uppers:+ under $uppers}"
    for x in "${lowers[@]}"; do
        [ -z "${md_seen[$x]:-}" ] || continue
        md_seen[$x]=1
        xr=$(root_qdisc_summary "$x")
        if [ "$xr" = noqueue ]; then
            managed_under "$x" "$below" $((depth + 1))
        elif [ -n "$xr" ] && [ -e "/sys/class/net/$x/device" ]; then
            printf '%s|%s|%s (under %s)\n' "$x" "$xr" "$x" "$below"
        fi
    done
    return 0
}

# load_managed <tc_interface>: managed_devices into three parallel arrays,
# MANAGED_DEVS, MANAGED_ROOTS and MANAGED_LABELS, in this shell.
load_managed() {
    MANAGED_DEVS=() MANAGED_ROOTS=() MANAGED_LABELS=()
    local d r l
    while IFS='|' read -r d r l; do
        [ -n "$d" ] || continue
        MANAGED_DEVS+=("$d") MANAGED_ROOTS+=("$r") MANAGED_LABELS+=("$l")
    done < <(managed_devices "$1")
}

# A managed device's name the way the guard words it in its corrections --
# "eth0", "eth0 (under bond0)" -- into REPLY.
dev_label() {
    local i
    for i in "${!BEFORE_DEVS[@]}"; do
        if [ "${BEFORE_DEVS[i]}" = "$1" ]; then REPLY=${BEFORE_LABELS[i]}; return 0; fi
    done
    if [ "$1" = "${DEV:-}" ]; then REPLY=$1; else REPLY="$1 (under $DEV)"; fi
}

# One line on what runtime.tc_interface ($DEV) and the devices under it carry
# now, from BEFORE_DEVS/BEFORE_ROOTS; into REPLY.
describe_egress() {
    local i list= route
    if [ -z "${DEV:-}" ]; then
        route=$(default_route_interface)
        # A config that simply names no tc_interface says so first; the
        # placeholder (kept on a tunnel-routed host) still gets the route.
        if [ -r "$CFG" ] && [ -z "$(configured_interface)" ]; then
            REPLY="no runtime.tc_interface configured"
        elif [ -n "$route" ]; then
            REPLY="no Ethernet egress interface (the default route leaves through $route)"
        else
            REPLY="no default route (IPv4 or IPv6) found"
        fi
        return 0
    fi
    if [ ! -e "/sys/class/net/$DEV" ]; then REPLY="$DEV (no such interface on this host)"; return 0; fi
    if [ "${#BEFORE_DEVS[@]}" -eq 1 ] && [ "${BEFORE_DEVS[0]}" = "$DEV" ]; then
        REPLY="$DEV root qdisc ${BEFORE_ROOTS[0]}"
        return 0
    fi
    for i in "${!BEFORE_DEVS[@]}"; do
        list="${list:+$list, }${BEFORE_DEVS[i]} ${BEFORE_ROOTS[i]}"
    done
    REPLY="$DEV root qdisc ${BEFORE_ROOT:-unknown}"
    if [ "${#BEFORE_DEVS[@]}" -gt 1 ]; then
        REPLY="$REPLY; the NICs under it: $list"
    elif [ -n "$list" ]; then
        REPLY="$REPLY; the NIC under it: $list"
    elif [ "$BEFORE_ROOT" = noqueue ]; then
        REPLY="$REPLY (a virtual device; no NIC visible under it)"
    fi
}

# The files systemd-sysctl applies at boot, in the order it applies them, one
# per line, a symlink resolved to the file it names. Same rules as
# systemd-sysctl: a name in /etc hides the same name in /run, /usr/local/lib
# and /usr/lib -- also when the /etc entry is a link to /dev/null or an empty
# file, which is how a vendor file is masked -- and files apply in name order.
sysctl_boot_files() {
    local -A seen=()
    local -a entries=()
    local d f name
    for d in /etc/sysctl.d /run/sysctl.d /usr/local/lib/sysctl.d /usr/lib/sysctl.d /lib/sysctl.d; do
        for f in "$d"/*.conf; do
            [ -e "$f" ] || [ -L "$f" ] || continue      # the glob matched nothing
            name=${f##*/}
            [ -z "${seen[$name]:-}" ] || continue
            seen[$name]=1                               # a mask hides lower dirs too
            if [ ! -f "$f" ] || [ ! -s "$f" ]; then continue; fi
            entries+=("$name|$(readlink -f -- "$f" 2>/dev/null || printf '%s' "$f")")
        done
    done
    [ "${#entries[@]}" -gt 0 ] || return 0
    printf '%s\n' "${entries[@]}" | LC_ALL=C sort -t'|' -k1,1 | cut -d'|' -f2- || true
}

# The two keys the daemon owns, as a set of files leaves them: one
# "key|value|file" line per key any of them sets, the last assignment winning.
sysctl_key_settings() {
    [ "$#" -gt 0 ] || return 0
    awk '
        {
            line = $0
            sub(/^[ \t]*-?/, "", line)          # "-key = v": ignore a failed write
            if (line ~ /^[#;]/ || index(line, "=") == 0) next
            key = line; sub(/[ \t]*=.*/, "", key); gsub(/\//, ".", key)
            val = line; sub(/^[^=]*=[ \t]*/, "", val); sub(/[ \t\r]*$/, "", val)
            if (key == "net.ipv4.tcp_congestion_control" || key == "net.core.default_qdisc") {
                value[key] = val; file[key] = FILENAME
            }
        }
        END {
            n = split("net.ipv4.tcp_congestion_control net.core.default_qdisc", keys, " ")
            for (i = 1; i <= n; i++)
                if (keys[i] in value) print keys[i] "|" value[keys[i]] "|" file[keys[i]]
        }' "$@" 2>/dev/null || true
}

# What systemd-sysctl leaves in net.ipv4.tcp_congestion_control and
# net.core.default_qdisc at every boot, and which file says so ("key|value|
# file"). /etc/sysctl.conf counts only through a sysctl.d entry that links to
# it (Debian 12 and older, Ubuntu: 99-sysctl.conf), in that entry's place in
# the order; systemd-sysctl does not read it otherwise (Debian 13 dropped the
# link). Reported, never edited: those files belong to the operator (or to a
# "one-click BBR" script the operator ran), and skyline-speederd overrides
# them after boot anyway.
boot_sysctl_settings() {
    local -a files=()
    mapfile -t files < <(sysctl_boot_files)
    sysctl_key_settings "${files[@]}"
}

# The same keys as set in /etc/sysctl.conf when no sysctl.d entry links to it:
# applied by `sysctl -p` or `sysctl --system` (which read it last), not at boot.
conf_sysctl_settings() {
    local conf f
    [ -f /etc/sysctl.conf ] || return 0
    conf=$(readlink -f -- /etc/sysctl.conf 2>/dev/null || echo /etc/sysctl.conf)
    while IFS= read -r f; do
        [ "$f" != "$conf" ] || return 0
    done < <(sysctl_boot_files)
    sysctl_key_settings /etc/sysctl.conf
}

# `ssctl status --json` pretty-prints one "key": value per line. awk rather than a
# JSON parser for the same reason as the release lookup below: python3/jq are
# not guaranteed on a minimal server image. First match only; no early `exit`,
# which could SIGPIPE the printf and, under pipefail, fail the whole script.
# status_value <key> [json]: from $STATUS_JSON unless a reply is given.
STATUS_JSON=
status_value() {
    printf '%s\n' "${2-$STATUS_JSON}" | awk -v k="\"$1\":" '
        !found && $1 == k { v = $2; gsub(/[",]/, "", v); print v; found = 1 }'
}

# guard.live.devices from $STATUS_JSON: the managed set as the daemon itself
# resolved it, one "name|qdisc" line each (qdisc empty when it was null).
# serde_json's pretty printer gives every key its own line and an empty list
# as `"devices": []`.
status_devices() {
    printf '%s\n' "$STATUS_JSON" | awk '
        $1 == "\"devices\":" { inlist = ($2 != "[]" && $2 != "[],"); next }
        !inlist { next }
        $1 ~ /^\]/ { inlist = 0; next }
        $1 == "{" { name = ""; q = "" }
        $1 == "\"name\":" { name = $2; gsub(/[",]/, "", name) }
        $1 == "\"qdisc\":" { q = $2; gsub(/[",]/, "", q); if (q == "null") q = "" }
        ($1 == "}" || $1 == "},") && name != "" { print name "|" q }'
}

# HAS_GUARD from $STATUS_JSON: 1 when the daemon has the guard. A release
# older than it (0.2.0, which --prebuilt still fetches until a newer release
# exists, or `--release v0.2.0`) has no "guard" in its status and no
# --version either, which is the fallback when status did not answer. A
# `case`, not `printf | grep -q`: grep exiting at the first match can SIGPIPE
# the printf, and pipefail would make the test fail on a match.
classify_guard() {
    case "$STATUS_JSON" in
        *'"guard":'*) HAS_GUARD=1 ;;
        '') if [ -n "${NEW_VERSION:-}" ]; then HAS_GUARD=1; else HAS_GUARD=0; fi ;;
        *) HAS_GUARD=0 ;;
    esac
}

# ssctl 0.3.0 and newer print a readable report by default and keep the JSON
# this script parses behind --json. An older ssctl -- the one already on the
# host during an upgrade -- does not know the flag, exits with a usage error
# and prints nothing on stdout; its plain output is that same JSON. So: ask
# for --json, and fall back to the bare command when the answer is not an
# object. Trying the new form first means a host that has both never pays for
# the old one.
ssctl_status_json() {
    local out
    out=$(timeout 10 /usr/local/bin/ssctl status --json 2>>"$LOG" || true)
    case "$out" in
        '{'*) printf '%s' "$out"; return 0 ;;
    esac
    timeout 10 /usr/local/bin/ssctl status 2>>"$LOG" || true
}

read_status() {
    STATUS_JSON=$(ssctl_status_json)
    printf '%s\n' "$STATUS_JSON" >>"$LOG"
}

daemon_version() {
    /usr/local/sbin/skyline-speederd --version 2>/dev/null | awk '{ print $2 }' || true
}

# restore_root_qdisc <dev> <summary before install> <default_qdisc to leave>
restore_root_qdisc() {
    local dev=$1 want=$2 dflt=$3 now
    if [ ! -e "/sys/class/net/$dev" ] || ! command -v tc >/dev/null 2>&1; then
        warn "$dev root qdisc not restored to $want: no such interface, or no tc"
        return 0
    fi
    now=$(root_qdisc_summary "$dev")
    [ "$now" != "$want" ] || return 0
    # Only undo what skyline-speederd itself leaves behind. Anything else was
    # put there after the install by someone, on purpose.
    case "$now" in
        fq|mq/fq) ;;
        *) warn "$dev root qdisc is ${now:-unknown}, not what Skyline Speeder sets; left alone (it was $want before the install)"
           return 0 ;;
    esac
    case "$want" in
        mq/*,*)
            warn "$dev root qdisc was $want (mixed kinds under mq); left as $now"
            return 0 ;;
        mq/*)
            # mq's children are created from default_qdisc, so that goes
            # first. Then delete the root: over an mq that tc created -- which
            # is what skyline-speederd leaves on a multi-queue NIC -- that
            # makes the kernel attach its own default mq afresh, built from
            # default_qdisc. `replace root mq` would NOT do it there: with the
            # same kind and no options it is a "change" the kernel accepts and
            # ignores (exit 0, the fq children stay; checked on 6.12). Only
            # the kernel's own mq (handle 0) cannot be deleted ("Cannot delete
            # qdisc with handle of zero"), and over that one `replace root mq`
            # does create a fresh mq from default_qdisc.
            sysctl -qw "net.core.default_qdisc=${want#mq/}" 2>/dev/null || true
            tc qdisc del dev "$dev" root 2>/dev/null \
                || tc qdisc replace dev "$dev" root mq 2>/dev/null || true
            sysctl -qw "net.core.default_qdisc=$dflt" 2>/dev/null || true ;;
        *)
            tc qdisc replace dev "$dev" root "$want" 2>/dev/null || true ;;
    esac
    now=$(root_qdisc_summary "$dev")
    if [ "$now" = "$want" ]; then
        ok "$dev root qdisc restored to $want"
        # An mq comes back exactly as the kernel builds it by default, and so
        # does a single root of the default kind (or pfifo_fast); any other
        # single root may have been hand-built with settings nothing recorded.
        case "$want" in
            mq/*|pfifo_fast|"$dflt") ;;
            *) warn "only the kind was restored, with default parameters; custom settings of a hand-built qdisc (a cake bandwidth, say) were not" ;;
        esac
    else
        warn "could not restore $dev root qdisc to $want (it is ${now:-unknown})"
    fi
}

# tx_queue_count <device>: its transmit queues, at least 1. More than one and
# the root has to be mq over fq rather than a bare fq, the same choice the
# guard makes (crates/skyline-speederd/src/guard.rs).
tx_queue_count() {
    local n=0 q
    for q in "/sys/class/net/$1/queues"/tx-*; do
        [ -d "$q" ] || continue
        n=$((n + 1))
    done
    [ "$n" -ge 1 ] || n=1
    printf '%s' "$n"
}

# ensure_fq_root <device>: leave fq on one NIC's root. Normally there is
# nothing to do -- skyline-speederd's guard was holding fq there until the
# drain a moment ago. A root that looks built on purpose (htb, tbf, netem, a
# cake with a bandwidth) is left exactly alone and named, for the reason the
# guard never replaces one either: overwriting it would silently destroy an
# operator's shaping.
ensure_fq_root() {
    local dev=$1 now shaping queues
    if [ ! -e "/sys/class/net/$dev" ]; then
        warn "$dev: no such interface any more; no qdisc set there"
        return 0
    fi
    if ! command -v tc >/dev/null 2>&1; then
        warn "no tc (iproute2) on this host; $dev root qdisc left as it is"
        return 0
    fi
    now=$(root_qdisc_summary "$dev")
    case "$now" in
        '')      warn "$dev: tc reports no root qdisc; left alone"; return 0 ;;
        noqueue) log "$dev root qdisc is noqueue (it queues nothing); left alone"; return 0 ;;
        fq|mq/fq) ok "$dev root qdisc is already $now"; return 0 ;;
    esac
    shaping=$(root_qdisc_shaping "$dev")
    if [ -n "$shaping" ] || ! qdisc_replaceable "$now"; then
        warn "$dev root qdisc is $now${shaping:+ ($shaping)}; it looks built on purpose, so it was left alone"
        warn "  to put fq there: tc qdisc replace dev $dev root fq"
        return 0
    fi
    queues=$(tx_queue_count "$dev")
    if [ "$queues" -gt 1 ]; then
        # A multi-queue NIC gets mq, whose children the kernel creates from
        # net.core.default_qdisc -- so only while that actually reads fq, the
        # same condition guard.rs's replace_plan insists on (default_is_fq).
        # Without it a failed sysctl write above would turn into an mq of
        # something else, which is worse than the root that is there now.
        if [ "$(sysctl -n net.core.default_qdisc 2>/dev/null || true)" != fq ]; then
            warn "$dev has $queues transmit queues and net.core.default_qdisc is not fq;"
            warn "  its root qdisc ($now) was left alone. Set it by hand with:"
            warn "  sysctl -w net.core.default_qdisc=fq && tc qdisc replace dev $dev root mq"
            return 0
        fi
        # `replace root mq` over an mq that tc created -- which is what
        # skyline-speederd leaves on a multi-queue NIC -- is a same-kind
        # no-option "change" the kernel accepts and ignores (exit 0, the
        # children stay; checked on 6.12). Deleting the root is what makes the
        # kernel build a fresh mq from default_qdisc; its own handle-0 mq
        # cannot be deleted, and there the replace is what works. Same two
        # steps, same order, as restore_root_qdisc.
        tc qdisc del dev "$dev" root >/dev/null 2>&1 \
            || tc qdisc replace dev "$dev" root mq >/dev/null 2>&1 || true
    else
        # A single-queue NIC names fq itself, so the default does not matter.
        tc qdisc replace dev "$dev" root fq >/dev/null 2>&1 || true
    fi
    now=$(root_qdisc_summary "$dev")
    case "$now" in
        fq|mq/fq) ok "$dev root qdisc set to $now" ;;
        *) warn "could not set $dev root qdisc to fq (it is ${now:-unknown})" ;;
    esac
}

# restore_bbr_fq: leave the host on bbr + fq. This is what --uninstall does
# unless --restore-pre-install asks for the snapshot instead, because a host
# that installed Skyline Speeder almost always arrived from a "one-click BBR"
# setup: dropping it to the daemon's fallback_cc (cubic) on the way out would
# be a downgrade nobody asked for and nothing would report it.
#
# For this boot only. No file under /etc/sysctl.d is written or edited -- the
# same rule the guard follows while it is installed -- so after a reboot those
# files decide again, and the caller says so in as many words.
restore_bbr_fq() {
    local avail cc=bbr dev want
    avail=$(sysctl -n net.ipv4.tcp_available_congestion_control 2>/dev/null || true)
    case " $avail " in
        *" bbr "*) ;;
        # tcp_bbr is a module on most distribution kernels, and on a host that
        # has been running skyline_cc nothing has asked for it yet. Writing
        # the sysctl does not load it: the kernel only accepts the name of an
        # algorithm that is already registered.
        *) modprobe tcp_bbr >/dev/null 2>&1 || true
           avail=$(sysctl -n net.ipv4.tcp_available_congestion_control 2>/dev/null || true) ;;
    esac
    case " $avail " in
        *" bbr "*) ;;
        *) # No point insisting on a name this kernel does not have. Prefer
           # what the host ran before the install, then the usual suspects.
           cc=
           for want in "${PRE_INSTALL_CC:-}" cubic reno; do
               [ -n "$want" ] || continue
               case " $avail " in *" $want "*) cc=$want; break ;; esac
           done
           if [ -n "$cc" ]; then
               warn "bbr is not available on kernel $KVER; using $cc instead"
           else
               warn "bbr is not available on kernel $KVER and no fallback of ours is either;"
               warn "  congestion control left at $(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)"
           fi ;;
    esac
    if [ -n "$cc" ]; then
        sysctl -qw "net.ipv4.tcp_congestion_control=$cc" 2>/dev/null \
            && ok "congestion control set to $cc" \
            || warn "could not set congestion control to $cc"
    fi
    # Writing this is also what makes the kernel load sch_fq
    # (qdisc_set_default -> request_module), so there is no modprobe for it.
    sysctl -qw net.core.default_qdisc=fq 2>/dev/null \
        && ok "default qdisc set to fq" \
        || warn "could not set net.core.default_qdisc to fq"
    # default_qdisc only shapes qdiscs created after it, so the NICs that
    # carried skyline-speederd's fq are set explicitly. They are resolved the
    # way the guard resolved them, from the config --uninstall keeps; the
    # snapshot's list is the fallback for a host whose config or NIC has since
    # changed.
    dev=$(planned_interface)
    load_managed "$dev"
    if [ "${#MANAGED_DEVS[@]}" -eq 0 ] && [ -n "${PRE_INSTALL_QDISC_DEV:-}" ]; then
        read -ra MANAGED_DEVS <<<"$PRE_INSTALL_QDISC_DEV"
    fi
    if [ "${#MANAGED_DEVS[@]}" -eq 0 ]; then
        warn "no egress interface is known here, so no NIC's root qdisc was set to fq"
        warn "  net.core.default_qdisc = fq still applies to qdiscs created from now on"
        return 0
    fi
    for dev in "${MANAGED_DEVS[@]}"; do
        [ -n "$dev" ] || continue
        ensure_fq_root "$dev"
    done
}

# restore_pre_install: put back exactly what the host ran before the install,
# from the snapshot $STATE. What --uninstall did before bbr + fq became its
# default, and still what --restore-pre-install asks for -- on a host that was
# deliberately on something else (cubic for a comparison, a shaped cake), bbr
# and fq would be as wrong as cubic is on the usual one. The caller has
# sourced $STATE already.
restore_pre_install() {
    local -a devs=() roots=()
    local dflt i
    if [ -n "${PRE_INSTALL_CC:-}" ]; then
        if sysctl -n net.ipv4.tcp_available_congestion_control 2>/dev/null \
            | grep -qw -- "$PRE_INSTALL_CC"; then
            sysctl -qw "net.ipv4.tcp_congestion_control=$PRE_INSTALL_CC"
            ok "congestion control restored to $PRE_INSTALL_CC"
        else
            warn "cannot restore '$PRE_INSTALL_CC': no longer available on this kernel"
        fi
    fi
    if [ -n "${PRE_INSTALL_QDISC:-}" ]; then
        sysctl -qw "net.core.default_qdisc=$PRE_INSTALL_QDISC" 2>/dev/null \
            && ok "default qdisc restored to $PRE_INSTALL_QDISC" \
            || warn "could not restore default qdisc to $PRE_INSTALL_QDISC"
    fi
    # default_qdisc only shapes qdiscs created later; the root qdiscs are
    # what skyline-speederd actually replaced: runtime.tc_interface's, or
    # those of the NICs under it (a VLAN, bond or bridge). Two parallel,
    # space-separated lists. Snapshots written before the guard existed
    # have neither key, and one that knew no interface has them empty;
    # both are skipped.
    [ -n "${PRE_INSTALL_QDISC_DEV:-}" ] && [ -n "${PRE_INSTALL_ROOT_QDISC:-}" ] || return 0
    read -ra devs <<<"$PRE_INSTALL_QDISC_DEV"
    read -ra roots <<<"$PRE_INSTALL_ROOT_QDISC"
    if [ "${#devs[@]}" -ne "${#roots[@]}" ]; then
        warn "$STATE lists ${#devs[@]} interface(s) but ${#roots[@]} root qdisc(s); no root qdisc restored"
        return 0
    fi
    dflt=$(sysctl -n net.core.default_qdisc 2>/dev/null || echo "${PRE_INSTALL_QDISC:-fq_codel}")
    for i in "${!devs[@]}"; do
        restore_root_qdisc "${devs[i]}" "${roots[i]}" "$dflt"
    done
}

# The packages dpkg has fully installed, one per line, sorted -- the input to
# the difference an install records in $ADDED_PKGS.
dpkg_installed_set() {
    dpkg-query -W -f '${Package} ${Status}\n' 2>/dev/null \
        | awk '$NF == "installed" { print $1 }' | sort || true
}

pkg_is_keeper() {
    local k
    for k in "${KEEP_PKGS[@]}"; do
        [ "$k" != "$1" ] || return 0
    done
    return 1
}

# pkg_closure <package...>: every installed package those still need,
# transitively, themselves included, one per line. apt-cache prints a package
# name at the start of a line and its relations indented; an architecture
# suffix (`dpkg:i386`) and a virtual package's angle brackets are stripped.
# Recommends and Suggests are deliberately excluded -- "would be nice to have"
# is not a reason to keep a compiler.
pkg_closure() {
    [ "$#" -gt 0 ] || return 0
    command -v apt-cache >/dev/null 2>&1 || return 0
    apt-cache depends --recurse --installed --no-recommends --no-suggests \
        --no-conflicts --no-breaks --no-replaces --no-enhances "$@" 2>/dev/null \
        | awk '/^[^[:space:]]/ { sub(/:[^:]*$/, ""); gsub(/[<>]/, ""); if ($0 != "") print }' \
        | sort -u || true
}

# pkg_in_list <package> <list...>
pkg_in_list() {
    local needle=$1
    shift
    local p
    for p in "$@"; do
        [ "$p" != "$needle" ] || return 0
    done
    return 1
}

# record_added_packages <file holding the installed set from before apt ran>:
# append what apt added to $ADDED_PKGS, as the union over every run (an
# upgrade run can add more). It is the difference of two dpkg installed sets,
# not apt's "Inst" lines, so it names the dependencies apt pulled in as well
# and never names a package the host already had -- which is what makes it
# safe for --uninstall to purge the list without an autoremove.
#
# Intersected with the plan apt accepted for THIS request, though, because the
# difference spans a window in which another apt client is expected to be
# running: a fresh cloud VM is often still running unattended-upgrades, which
# is why APT_OPTS waits five minutes for the dpkg lock. Whatever that installed
# in the meantime is in the difference and is not ours to remove. Inst lines
# name apt's dependency closure too, so nothing of ours is lost by this.
record_added_packages() {
    local before=$1 added tmp planned
    [ -r "$before" ] || return 0
    added=$(dpkg_installed_set | comm -13 "$before" - || true)
    if [ -n "$added" ] && [ -n "${APT_SIM:-}" ] && [ -r "${APT_SIM:-/nonexistent}" ]; then
        planned=$(mktemp)
        sed -n 's/^Inst \([^ :]*\).*/\1/p' "$APT_SIM" | sort -u >"$planned"
        if [ -s "$planned" ]; then
            REPLY=$(printf '%s\n' "$added" | grep -xF -f "$planned" || true)
            if [ "$REPLY" != "$added" ]; then
                log "not recorded (installed by something else while apt ran): $(printf '%s\n' "$added" | grep -vxF -f "$planned" | tr '\n' ' ' || true)"
            fi
            added=$REPLY
        fi
        rm -f "$planned"
    fi
    if [ -z "$added" ]; then
        log "apt added no package that was not installed already"
        [ ! -r "$ADDED_PKGS" ] || ADDED_COUNT=$(grep -cv '^#' "$ADDED_PKGS" || true)
        count_removable_packages
        return 0
    fi
    install -d /etc/skyline-speeder
    tmp=$(mktemp)
    { [ ! -r "$ADDED_PKGS" ] || sed 's/#.*//' "$ADDED_PKGS"
      printf '%s\n' "$added"; } | sed '/^[[:space:]]*$/d' | sort -u >"$tmp"
    { printf '%s\n' \
        "# Packages an install of Skyline Speeder added to this host, one per line," \
        "# as the union over every run of this installer. --uninstall removes them" \
        "# again; delete a line to keep that package. iproute2, curl, ca-certificates" \
        "# and tar are never removed even when listed here, and neither is anything" \
        "# dpkg calls required or important."
      cat "$tmp"; } >"$ADDED_PKGS"
    ADDED_COUNT=$(grep -cv '^#' "$ADDED_PKGS" || true)
    count_removable_packages
    log "packages added by this run: $(tr '\n' ' ' <<<"$added")"
    rm -f "$tmp"
}

# How many recorded packages an uninstall would actually remove, into
# $ADDED_REMOVABLE. A --prebuilt install adds only curl, ca-certificates, tar
# and iproute2, every one of them a keeper, so the closing guide must not tell
# that operator their packages will be removed.
count_removable_packages() {
    local p
    ADDED_REMOVABLE=0
    [ -r "$ADDED_PKGS" ] || return 0
    while IFS= read -r p; do
        p=${p%%#*}
        p=${p//[[:space:]]/}
        [ -n "$p" ] || continue
        pkg_is_keeper "$p" && continue
        pkg_priority_protected "$p" && continue
        ADDED_REMOVABLE=$((ADDED_REMOVABLE + 1))
    done <"$ADDED_PKGS"
}

# pkg_installed <package>: whether dpkg has it installed, for any architecture.
# `${Status}\n`, not `${Status}`: without the newline dpkg concatenates one
# status per installed architecture into a single word, and a test over the
# result then answers about a string no instance actually has. awk rather than
# `grep -q` so nothing exits early on a producer this script pipes into --
# SIGPIPE plus pipefail is the trap this file documents in four other places.
pkg_installed() {
    dpkg-query -W -f '${Status}\n' "$1" 2>/dev/null \
        | awk '/ installed$/ { found = 1 } END { exit !found }'
}

# pkg_priority_protected <package>: whether dpkg calls it required or important
# for any architecture -- in which case an uninstall never removes it, whatever
# the record says. Same newline story as pkg_installed: measured on a host with
# i386 enabled, `dpkg-query -W -f '${Priority}' libbz2-1.0` answers
# "optionalimportant", which matches neither word and quietly turned this rail
# off for every multi-arch package.
pkg_priority_protected() {
    local prio
    prio=" $(dpkg-query -W -f '${Priority}\n' "$1" 2>/dev/null | tr '\n' ' ') "
    case "$prio" in
        *" required "*|*" important "*) return 0 ;;
    esac
    return 1
}

# purge_toolchain: remove the packages an install of this host added, and
# nothing else. $ADDED_PKGS already names the dependencies apt pulled in, so
# there is no autoremove here -- that would also take orphans this installer
# never created.
#
# Three rails, because an uninstall must not become a way to lose a package
# the host needs. KEEP_PKGS and dpkg's required/important priorities are never
# touched. And apt plans the removal first: if the plan would take anything
# that is not on the list, nothing is removed and the command is printed
# instead. Measured while writing this, on a host where libelf1 happened to be
# recorded: purging it would have taken 29 packages with it, iproute2,
# ifupdown, isc-dhcp-client and cloud-init among them. By this point Skyline
# Speeder itself is already gone, so every failure here is a warning and a
# command to run by hand, never an error that stops the uninstall.
purge_toolchain() {
    local -a want=() extra=() keep=()
    local p sim out protected round kept
    if [ ! -r "$ADDED_PKGS" ]; then
        log "no $ADDED_PKGS: this host has no record of packages an install added"
        return 0
    fi
    if ! command -v apt-get >/dev/null 2>&1; then
        warn "no apt-get here; the packages listed in $ADDED_PKGS were left installed"
        return 0
    fi
    kept=0
    while IFS= read -r p; do
        p=${p%%#*}
        p=${p//[[:space:]]/}
        [ -n "$p" ] || continue
        if pkg_is_keeper "$p"; then
            kept=$((kept + 1))
            continue
        fi
        # Not installed any more: somebody else removed it, or a previous
        # uninstall did.
        pkg_installed "$p" || continue
        if pkg_priority_protected "$p"; then
            kept=$((kept + 1))
            continue
        fi
        want+=("$p")
    done <"$ADDED_PKGS"
    # Keeping curl while removing libcurl4 is not a thing apt can do, and it
    # would resolve the contradiction by taking curl. So everything a package
    # we keep still needs is kept too -- measured on a test host, where this is
    # the difference between removing the whole toolchain and removing nothing
    # at all.
    #
    # The closure goes to a file, and grep reads that file: `printf | grep -q`
    # would let grep exit at the first match and SIGPIPE the printf, which
    # under pipefail makes the whole test read as false -- silently dropping
    # the protection this is here to apply.
    protected=$(mktemp)
    pkg_closure "${KEEP_PKGS[@]}" >"$protected"
    if [ -s "$protected" ]; then
        keep=()
        for p in "${want[@]}"; do
            if grep -qxF -- "$p" "$protected"; then
                kept=$((kept + 1))
            else
                keep+=("$p")
            fi
        done
        want=(${keep[@]+"${keep[@]}"})
    fi
    # One line rather than one per package: in uninstall mode $LOG is not
    # started, so a `log` here would reach nobody at all, and ninety of them
    # would bury the summary.
    [ "$kept" -eq 0 ] || info "keeping $kept recorded package(s): a keeper, something a keeper needs, or required/important"
    if [ "${#want[@]}" -eq 0 ]; then
        ok "no packages to remove: nothing this installer added is still installed"
        rm -f "$ADDED_PKGS" "$protected"
        return 0
    fi
    sim=$(mktemp) out=$(mktemp)
    # Ask apt to plan it, and keep asking: a package of ours that something the
    # operator installed later depends on drags that something into the plan,
    # and the answer is to leave that one package alone rather than to abandon
    # the whole removal. Bounded, because each round must make the set smaller.
    round=0
    while :; do
        round=$((round + 1))
        extra=()
        # `--purge remove` prints "Purg <pkg> [ver]" in a simulation, and
        # "Remv" when a package is removed without purging its configuration;
        # both count as a removal.
        if ! apt-get -y -s --purge remove "${want[@]}" >"$sim" 2>&1; then
            warn "apt cannot plan removing the packages this installer added, so none were removed:"
            # No `sed | head -1`: head exits at the first line, sed dies of
            # SIGPIPE, and under pipefail this bare assignment would end the
            # uninstall before the message that explains it -- the trap this
            # file documents beside apt_unmet. awk reads to the end instead.
            REPLY=$(awk '!done && /^E: / { sub(/^E: /, ""); print; done = 1 }' "$sim")
            [ -z "$REPLY" ] || warn "  apt: $REPLY"
            warn "  to remove them by hand: apt-get --purge remove ${want[*]}"
            rm -f "$sim" "$out" "$protected"
            return 0
        fi
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            pkg_in_list "$p" "${want[@]}" || extra+=("$p")
        done < <(sed -n 's/^\(Purg\|Remv\) \([^ ]*\).*/\2/p' "$sim" | sort -u)
        [ "${#extra[@]}" -gt 0 ] || break
        if [ "$round" -ge 3 ]; then
            warn "removing what this installer added would also remove: ${extra[*]}"
            warn "  Something on this host depends on them now, so nothing was removed."
            warn "  If that is what you want: apt-get --purge remove ${want[*]}"
            rm -f "$sim" "$out" "$protected"
            return 0
        fi
        # Whatever those extras still need, inside our own set, is what forces
        # them out. Leave exactly those alone and plan again.
        pkg_closure "${extra[@]}" >"$protected"
        keep=()
        for p in "${want[@]}"; do
            if grep -qxF -- "$p" "$protected"; then
                warn "keeping $p: ${extra[0]}$([ "${#extra[@]}" -gt 1 ] && printf ' and %d other package(s)' "$((${#extra[@]} - 1))") on this host still needs it"
            else
                keep+=("$p")
            fi
        done
        if [ "${#keep[@]}" -eq "${#want[@]}" ]; then
            warn "removing what this installer added would also remove: ${extra[*]}"
            warn "  apt does not say which of ours they need, so nothing was removed."
            warn "  If that is what you want: apt-get --purge remove ${want[*]}"
            rm -f "$sim" "$out" "$protected"
            return 0
        fi
        want=(${keep[@]+"${keep[@]}"})
        if [ "${#want[@]}" -eq 0 ]; then
            ok "no packages left to remove: this host needs all of them"
            rm -f "$sim" "$out" "$protected"
            return 0
        fi
    done
    info "removing ${#want[@]} package(s) this installer added"
    if apt-get "${APT_OPTS[@]}" --purge remove "${want[@]}" >"$out" 2>&1; then
        ok "removed: ${want[*]}"
        rm -f "$ADDED_PKGS"
    else
        warn "apt failed to remove these, and left them installed: ${want[*]}"
        while IFS= read -r p; do warn "  $p"; done < <(tail -n 5 "$out")
    fi
    rm -f "$sim" "$out" "$protected"
}

# purge_rustup: undo the rustup installation an install of this host made.
# Only that one: the paths come from $ADDED_RUSTUP, which is written only on
# the run that installed rustup, so a toolchain the operator had before is
# never touched. `rustup self uninstall` removes both directories itself; the
# rm is the fallback for a broken installation, and it insists on directories
# that still look like rustup's so a hand-edited file cannot point it at
# something else.
purge_rustup() {
    # The file sets RUSTUP_HOME and CARGO_HOME; copied into locals of another
    # name so the call below passes them as an environment without also
    # expanding the same names in its own words.
    local RUSTUP_HOME= CARGO_HOME= rustup_home cargo_home removed
    [ -r "$ADDED_RUSTUP" ] || return 0
    # shellcheck disable=SC1090  # a generated key=value file
    . "$ADDED_RUSTUP"
    rustup_home=${RUSTUP_HOME:-} cargo_home=${CARGO_HOME:-}
    if [ -z "$cargo_home" ] || [ -z "$rustup_home" ]; then
        warn "$ADDED_RUSTUP names no rustup directories; the Rust toolchain was left in place"
        return 0
    fi
    if [ -x "$cargo_home/bin/rustup" ]; then
        if env RUSTUP_HOME="$rustup_home" CARGO_HOME="$cargo_home" \
            "$cargo_home/bin/rustup" self uninstall -y >/dev/null 2>&1; then
            ok "Rust toolchain removed ($cargo_home)"
            rm -f "$ADDED_RUSTUP"
            return 0
        fi
        warn "rustup self uninstall failed; removing its directories instead"
    fi
    # A hand-edited file must not turn this into an arbitrary rm: an absolute
    # path of at least two segments, and it still has to look like what rustup
    # leaves behind.
    case "$cargo_home" in /?*/?*) ;; *) warn "refusing to remove CARGO_HOME=$cargo_home"; return 0 ;; esac
    case "$rustup_home" in /?*/?*) ;; *) warn "refusing to remove RUSTUP_HOME=$rustup_home"; return 0 ;; esac
    # BOTH marks, not either: `bin/` alone is true of /usr, /usr/local, /opt and
    # half the filesystem, so `||` would let a hand-edited record aim this rm at
    # one of them. `env` is a file only rustup writes.
    removed=0
    if [ -e "$cargo_home/env" ] && [ -d "$cargo_home/bin" ]; then
        rm -rf "$cargo_home"
        removed=1
    else
        warn "$cargo_home does not look like a cargo home; left in place"
    fi
    if [ -d "$rustup_home/toolchains" ]; then
        rm -rf "$rustup_home"
        removed=1
    else
        warn "$rustup_home does not look like a rustup home; left in place"
    fi
    # Nothing was recognised, so nothing was removed: saying otherwise, and
    # throwing away the record, would hide it from the next uninstall too.
    if [ "$removed" -eq 1 ]; then
        ok "Rust toolchain directories removed"
        rm -f "$ADDED_RUSTUP"
    else
        warn "$ADDED_RUSTUP was kept, so a later uninstall can try again"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check) MODE=check; shift ;;
        --no-enable) ENABLE=0; shift ;;
        --uninstall) MODE=uninstall; shift ;;
        --restore-pre-install) RESTORE=pre-install; shift ;;
        --prebuilt) SOURCE=prebuilt; shift ;;
        --release) [ "$#" -ge 2 ] || die "--release needs a tag"; SOURCE=prebuilt; RELEASE_TAG="$2"; shift 2 ;;
        --verbose) VERBOSE=1; shift ;;
        # The header comment, to its end, rather than a hand-counted line
        # range that goes stale the moment a flag is documented above.
        -h|--help) awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0)"

printf '\n %sSkyline Speeder%s\n' "$BLD" "$RST" >&3
sponsor '--'

# Said here rather than ignored: a flag that does nothing on the mode it was
# given with reads as a request that was honoured.
if [ "$RESTORE" = pre-install ] && [ "$MODE" != uninstall ]; then
    die "--restore-pre-install only applies to --uninstall"
fi

if [ "$MODE" = uninstall ]; then
    # Nothing of an install here: say so and change nothing at all. Run on the
    # wrong host -- or twice -- this must not reconfigure a NIC that Skyline
    # Speeder never touched. A host still defaulting to skyline_cc counts as
    # installed however little else is left, since that is the one state an
    # uninstall has to get out of.
    # Two questions, not one. ACTIVE: something is in place that changed how
    # this host sends -- units, binaries, the objects, or skyline_cc still being
    # the default. Only that earns a write to a sysctl or a qdisc, which is what
    # makes running --uninstall a second time change nothing: the first run
    # deliberately keeps /etc/skyline-speeder, and a lone configuration
    # directory has changed nothing about the network.
    # INSTALLED: anything at all, that directory included, because a half
    # finished install leaves packages recorded there that must still be
    # removable.
    ACTIVE=0
    for path in /etc/systemd/system/skyline-speederd.service \
                /etc/systemd/system/skyline-speeder-enable.service \
                /usr/local/sbin/skyline-speederd /usr/local/bin/ssctl \
                /opt/skyline-speeder; do
        [ ! -e "$path" ] || { ACTIVE=1; break; }
    done
    if [ "$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || true)" = skyline_cc ]; then
        ACTIVE=1
    fi
    INSTALLED=$ACTIVE
    [ ! -e /etc/skyline-speeder ] || INSTALLED=1
    if [ "$INSTALLED" -eq 0 ]; then
        ok "nothing of Skyline Speeder is installed here; nothing was changed"
        sponsor '--'
        exit 0
    fi
    info "removing Skyline Speeder"
    # Drain first so established flows migrate off skyline_cc before the daemon
    # goes away. A struct_ops map stays alive as long as any socket still
    # references it, so skipping this leaves a stale (harmless but confusing)
    # entry in `bpftool struct_ops show` until those connections end -- the same
    # reference-counting behaviour a kernel module shows when it is "in use".
    # Drain also stops skyline-speederd's guard, so nothing puts skyline_cc or
    # fq back while the steps below restore what was there before.
    if [ -S /run/skyline-speeder/speeder.sock ] && command -v ssctl >/dev/null 2>&1; then
        info "draining active flows (up to 60s)"
        ssctl drain --timeout 60 >/dev/null 2>&1 || true
    fi
    systemctl disable --now skyline-speeder-enable.service >/dev/null 2>&1 || true
    systemctl disable --now skyline-speederd.service >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/skyline-speederd.service \
          /etc/systemd/system/skyline-speeder-enable.service \
          /usr/local/sbin/skyline-speederd /usr/local/bin/ssctl
    rm -rf /opt/skyline-speeder
    systemctl daemon-reload

    # Counted HERE, before the toolchain goes: bpftool is one of the packages an
    # install added, so asking after the purge would always answer "none" on the
    # very hosts that have something to report. Reported further down, once the
    # host is back on a congestion control this can name.
    LEFT=$(bpftool struct_ops show 2>/dev/null | grep -c skyline_cc || true)

    # Leave the host on something deliberate, and say which. By default bbr +
    # fq: where a host that installed Skyline Speeder almost certainly came
    # from, and not the daemon's `fallback_cc` (cubic), which is a downgrade
    # nobody asked for that nothing would report. --restore-pre-install asks
    # for the snapshot instead. Either way the snapshot is read first -- even
    # the bbr path uses PRE_INSTALL_CC when this kernel has no bbr, and its
    # device list as the fallback for a host whose config has since changed.
    if [ -r "$STATE" ]; then
        # shellcheck disable=SC1090  # a generated key=value file
        . "$STATE"
    elif [ "$RESTORE" = pre-install ]; then
        warn "no pre-install snapshot at $STATE, so there is nothing to restore from."
        warn "Current: cc=$(sysctl -n net.ipv4.tcp_congestion_control) qdisc=$(sysctl -n net.core.default_qdisc)"
        warn "Leaving both as they are; run without --restore-pre-install for bbr + fq."
        RESTORE=none
    fi
    if [ "$ACTIVE" -eq 0 ]; then
        # Only the kept configuration directory was here: nothing of ours was
        # in the path, so nothing of the host's networking is ours to rewrite.
        RESTORE=none
        info "only $(dirname "$CFG") was left here; no sysctl or qdisc was changed"
    fi
    case "$RESTORE" in
        bbr-fq)      restore_bbr_fq ;;
        pre-install) restore_pre_install ;;
        *)           ;;
    esac

    # The build toolchain this installer put here, and the rustup it may have
    # installed: removed by default, because leaving clang, LLVM and a Rust
    # toolchain behind on a host somebody has finished with is not "removed".
    # Both functions only ever touch what an install of this host recorded.
    export DEBIAN_FRONTEND=noninteractive
    purge_toolchain
    purge_rustup

    # /etc/skyline-speeder is left in place on purpose: it holds operator-tuned
    # configuration that a reinstall should not silently discard. Only claimed
    # when it is actually there -- a half-installed host may have none.
    if [ -d /etc/skyline-speeder ]; then
        ok "removed (configuration kept at /etc/skyline-speeder)"
    else
        ok "removed"
    fi
    if [ "$RESTORE" = bbr-fq ]; then
        # Nothing under /etc/sysctl.d was written or edited -- the rule the
        # guard follows while it is installed -- so those files, not this run,
        # decide what the host comes back as.
        info "cc and qdisc are set for this boot only; after a reboot /etc/sysctl.d decides"
    fi
    # Report, do not "fix": force-detaching a struct_ops that live sockets still
    # use is not something an uninstaller should do behind the operator's back.
    # $LEFT was counted above, while bpftool was still installed.
    if [ "${LEFT:-0}" -gt 0 ]; then
        warn "$LEFT skyline_cc struct_ops map(s) are still held by established connections."
        warn "This is normal reference-counting behaviour, not a failure: the kernel frees"
        warn "them once those connections close, or at the next reboot. New connections"
        warn "already use $(sysctl -n net.ipv4.tcp_congestion_control)."
    fi
    printf '\n' >&3
    sponsor '--'
    exit 0
fi

if [ "$MODE" = install ]; then
    QUIET=$((1 - VERBOSE))
    # A terminal that cannot clear a line (TERM=dumb) gets the plain
    # one-line-per-step output instead of a smeared bar.
    if [ "$TTY" -eq 1 ] && [ "$VERBOSE" -eq 0 ] && [ "${TERM:-}" != dumb ]; then BAR=1; fi
    WORK=$(mktemp -d)
    : >"$LOG" || die "cannot write $LOG"
    LOG_READY=1
    log "Skyline Speeder install, $(date '+%F %T %z'), source=$SOURCE${RELEASE_TAG:+ release=$RELEASE_TAG} enable=$ENABLE"
    if [ "$SOURCE" = build ]; then STEP_TOTAL=11; else STEP_TOTAL=9; fi
    [ "$ENABLE" -eq 1 ] || STEP_TOTAL=$((STEP_TOTAL - 2))
    # No SIGWINCH trap for a resize: a trapped signal makes a pending `wait`
    # return early with 128+n, which run() would take for a failed step. The
    # width is re-read at each step and every few seconds while one runs.
    if [ "$BAR" -eq 1 ]; then
        term_cols
        printf '\033[?25l' >&3      # cursor off while drawing; on_exit turns it back on
    fi
    step "Checking the system"
fi

# --- 1. distribution -------------------------------------------------------
[ -r /etc/os-release ] || die "/etc/os-release missing; unsupported system"
# shellcheck disable=SC1091
. /etc/os-release
case "${ID:-}:${ID_LIKE:-}" in
    debian:*|ubuntu:*|*:*debian*|*:*ubuntu*) ok "distribution: ${PRETTY_NAME:-$ID}" ;;
    *) die "this installer supports Debian/Ubuntu only (found ID=${ID:-unknown})" ;;
esac

# --- 2. kernel version -----------------------------------------------------
# Hard ABI floor. skyline_cc hangs off tcp_congestion_ops.cong_control declared
# with four arguments (sk, ack, flag, rs). That signature only exists from
# v6.10; on v6.9 and earlier the function pointer takes two and the BPF verifier
# rejects the program outright at load time. 6.12 LTS is the supported floor.
KMAJ=${KVER%%.*}; KREST=${KVER#*.}; KMIN=${KREST%%.*}
if [ "$KMAJ" -lt 6 ] || { [ "$KMAJ" -eq 6 ] && [ "$KMIN" -lt 12 ]; }; then
    die "kernel $KVER is not supported.
   Skyline Speeder requires 6.12 LTS or newer (hard ABI floor is 6.10).
   The 6.1.x and 6.6.x LTS branches use the older 2-argument cong_control
   signature and the verifier will reject skyline_cc on them."
fi
ok "kernel: $KVER"

# --- 3. runtime prerequisites ---------------------------------------------
[ -r /sys/kernel/btf/vmlinux ] || die "/sys/kernel/btf/vmlinux is missing.
   CO-RE needs kernel BTF; rebuild the kernel with CONFIG_DEBUG_INFO_BTF=y."
ok "kernel BTF present"

if [ ! -d /sys/fs/cgroup ] || ! grep -qw cgroup2 /proc/filesystems; then
    die "cgroup v2 is required (the sockops policy attaches to a cgroup v2 path)"
fi
ok "cgroup v2 available"

# --- 4. what the host runs now ---------------------------------------------
# Recorded before anything changes: the summary reports "was", the pre-install
# snapshot below is built from it, and --uninstall puts it back.
# BEFORE_ROOT is runtime.tc_interface's own root; BEFORE_DEVS/_ROOTS/_LABELS are
# the devices the guard will manage (managed_devices), their roots and labels.
BEFORE_CC=$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)
BEFORE_DQ=$(sysctl -n net.core.default_qdisc 2>/dev/null || echo unknown)
DEV=$(planned_interface)
BEFORE_ROOT=$(root_qdisc_summary "$DEV")
load_managed "$DEV"
BEFORE_DEVS=("${MANAGED_DEVS[@]}") BEFORE_ROOTS=("${MANAGED_ROOTS[@]}") BEFORE_LABELS=("${MANAGED_LABELS[@]}")
BOOT_SETTINGS=$(boot_sysctl_settings)
CONF_SETTINGS=$(conf_sysctl_settings)
describe_egress
log "before: tcp_congestion_control=$BEFORE_CC default_qdisc=$BEFORE_DQ egress: $REPLY"
[ -z "$BOOT_SETTINGS" ] || log "set at boot (key|value|file):" "$BOOT_SETTINGS"
[ -z "$CONF_SETTINGS" ] || log "set in /etc/sysctl.conf, not read at boot (key|value|file):" "$CONF_SETTINGS"

# --- 5. packages -----------------------------------------------------------
export DEBIAN_FRONTEND=noninteractive
# iproute2 on both paths: skyline-speederd runs `tc` to put fq on the egress
# interface, and the steps below use `ip` and `tc` to find and report it.
if [ "$SOURCE" = prebuilt ]; then
    # The whole point of --prebuilt: no compiler, no LLVM, no bpftool, no Rust.
    # Only what it takes to fetch and unpack an archive.
    PKGS=(curl ca-certificates tar iproute2)
else
    PKGS=(build-essential pkg-config clang llvm libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2)
fi

APT_SPEC=()     # $PKGS, carrying an explicit version where this host needs one
APT_PINNED=0    # 1 once a version of our own choosing is in APT_SPEC

# apt_sim <package|package=version ...>: have apt plan the install without
# performing it, leaving the plan in $APT_SIM. A request that cannot be
# satisfied fails here, before anything on the host has changed, and the
# rejected plan names the package apt could not place -- which is what
# apt_plan works from.
#
# Not "${APT_OPTS[@]}": `-qq` reduces a failure to its last line,
# "E: Unable to correct problems, you have held broken packages", and throws
# away the "pkg : Depends: ..." lines that say which package and why.
apt_sim() {
    local rc=0
    STEP_RAN=1
    [ -n "$APT_SIM" ] || APT_SIM=$(mktemp)
    log "apt: planning $*"
    apt-get -y -s install "$@" >"$APT_SIM" 2>&1 || rc=$?
    [ "$LOG_READY" -eq 0 ] || cat "$APT_SIM" >>"$LOG"
    return "$rc"
}

# The first package in a rejected plan that this installer may do something
# about: one it asked for, or one apt would have to install anyway. apt prints
# one line per relationship it could not satisfy --
#
#   libelf-dev : Depends: libelf1 (= 0.188-2.1) but 0.192-4~bpo12+1 is to be installed
#
# -- and the same block also names packages that are merely installed and
# would break if apt went ahead --
#
#   linux-headers-6.12.95+deb12-cloud-amd64 : Depends: libelf1 (= 0.192-4~bpo12+1) but 0.188-2.1 is to be installed
#
# -- and giving one of those a version would mean upgrading somebody's kernel
# headers to get a toolchain installed. Never that; skip it and take the next.
#
# awk over the file rather than `sed ... | head -1`: head leaves the producer
# to die of SIGPIPE, `pipefail` turns that into status 141, and
# `pkg=$(apt_unmet)` would then end the script under `set -e` -- the trap the
# qdisc helpers above document.
apt_unmet() {
    [ -n "$APT_SIM" ] || return 0
    local p
    # A package this run asked for comes first, whatever order apt listed the
    # block in: that is the one a version may be chosen for.
    for p in $(awk '$2 == ":" && $3 ~ /:$/ { print $1 }' "$APT_SIM"); do
        case " ${APT_SPEC[*]} " in
            *" $p "*|*" $p="*) printf '%s\n' "$p"; return 0 ;;
        esac
    done
    # Then one apt would have to install anyway -- a dependency of the request
    # that is not on the host yet. An installed one is left where it is.
    for p in $(awk '$2 == ":" && $3 ~ /:$/ { print $1 }' "$APT_SIM"); do
        if [ "$(dpkg-query -W -f='${db:Status-Status}' "$p" 2>/dev/null || true)" != installed ]; then
            printf '%s\n' "$p"; return 0
        fi
    done
}

# The first error in the plan apt last rejected, without its "E: " prefix.
apt_error() { awk 'sub(/^E: /, "") { print; exit }' "$APT_SIM" 2>/dev/null; }

# Every version of <package> this host can reach, newest first. `apt-cache
# madison` prints "name | version | origin"; a deb-src entry and the dpkg
# status line do not end in "Packages" and are not versions apt can install.
apt_versions() {
    apt-cache madison "$1" 2>/dev/null |
        awk -F '|' '$3 ~ /Packages/ { gsub(/^[ \t]+|[ \t]+$/, "", $2); if (!seen[$2]++) print $2 }'
}

# apt_pin <package> <version>: ask for that exact version in APT_SPEC. A
# package that was only being pulled in as a dependency is added to the
# request -- naming it is the only way to make apt take a version its
# priorities would not have picked on their own (a backports one sits at
# priority 100, below the 500 of the suite the host tracks).
apt_pin() {
    local i found=0
    APT_PINNED=1
    for i in "${!APT_SPEC[@]}"; do
        case "${APT_SPEC[$i]}" in
            "$1"|"$1"=*) APT_SPEC[$i]="$1=$2"; found=1 ;;
        esac
    done
    [ "$found" -eq 1 ] || APT_SPEC+=("$1=$2")
}

# Finish what an earlier interrupted apt/dpkg run started. A VPS reset from
# the provider's console mid-upgrade, or an unattended-upgrades killed with
# it, leaves packages unpacked but unconfigured, and every install after that
# fails until dpkg is allowed to finish. Neither branch runs unless dpkg or
# apt says this host is in that state. --no-remove keeps the repair to
# installing what is missing: deleting a package the operator installed, to
# make the tree consistent, is not a decision an installer may take behind
# their back -- if that is what it would take, this one stops and says so.
apt_repair() {
    if [ "$MODE" = check ]; then return 0; fi   # preflight changes nothing
    if [ -n "$(dpkg -C 2>/dev/null || true)" ]; then
        warn "an earlier apt/dpkg run on this host never finished; completing it first"
        # DPkg::Lock::Timeout is an apt-level wait, so dpkg called directly
        # gives up at once against a running unattended-upgrades. Say so and
        # go on: the install below waits for that lock, and if the state is
        # still unfinished by then apt reports it.
        run dpkg --configure -a \
            || warn "dpkg could not finish; something else may hold its lock"
    fi
    if ! apt-get check >>"$LOG" 2>&1; then
        warn "this host has unsatisfied package dependencies; letting apt repair them first"
        run apt-get "${APT_OPTS[@]}" --no-remove -f install || true
    fi
}

# The first error apt just wrote to the log, without its "E: " ("The
# repository 'http://deb.debian.org/debian nosuchsuite Release' does not have
# a Release file"). Not its warnings: apt carries on by itself past a source
# it could not reach, and only fails the run when the failure changed what the
# host can install.
apt_update_error() {
    awk -v from="$STEP_LOG_LINE" 'NR > from && sub(/^E: /, "") { print; exit }' \
        "$LOG" 2>/dev/null
}

# apt_plan <package...>: leave in APT_SPEC a request apt says it can satisfy,
# or return 1 with the plan it rejected in $APT_SIM and in the log.
#
# Two host shapes land here regularly, neither of them the operator's mistake:
#
#   1. The interrupted apt/dpkg run apt_repair above finishes.
#
#   2. A library that came from a different suite than the one apt would take
#      the matching -dev package from. Debian 12 with a 6.12 kernel out of
#      bookworm-backports is the common one -- the kernel this project needs
#      on a bookworm host. Installing linux-headers-cloud-amd64 from backports
#      (for DKMS modules, or just because a guide said to) brings libelf1
#      0.192 with it, bookworm's libelf-dev depends on libelf1 (= 0.188-2.1),
#      and the request is then not satisfiable at all:
#
#        libelf-dev : Depends: libelf1 (= 0.188-2.1) but 0.192-4~bpo12+1 is to be installed
#        E: Unable to correct problems, you have held broken packages.
#
#      The answer is to take that one package from the suite the library on
#      the host came from. The loop finds it by asking apt which package it
#      could not place, then offering that package's other versions, newest
#      first, until the whole request plans cleanly.
#
# Deliberately not `apt-get -t bookworm-backports install ...`, the remedy
# that gets passed around for this: it re-resolves the entire request against
# backports and, on that same host, changes 55 packages -- curl, iproute2,
# bpftool and libbpf1 among them -- to fix one. Nothing here names a suite or
# a distribution, so the same loop covers an Ubuntu host whose libraries came
# from an HWE stack or a PPA.
apt_plan() {
    local pkg ver fitted moved
    APT_SPEC=("$@") APT_PINNED=0
    if apt_sim "${APT_SPEC[@]}"; then return 0; fi
    apt_repair
    if apt_sim "${APT_SPEC[@]}"; then return 0; fi
    # One package per round, three rounds: a host that needs more than three
    # substitutions is in a state an installer must not paper over.
    for _ in 1 2 3; do
        pkg=$(apt_unmet)
        [ -n "$pkg" ] || return 1
        # Asked for by version already and still unplaceable: nothing left to
        # try for it, and repeating the round would loop.
        case " ${APT_SPEC[*]} " in *" $pkg="*) return 1 ;; esac
        fitted= moved=
        for ver in $(apt_versions "$pkg"); do
            apt_pin "$pkg" "$ver"
            if apt_sim "${APT_SPEC[@]}"; then fitted=$ver; break; fi
            # Not the whole request, but apt no longer trips over THIS
            # package: remember the first version that gets that far, in case
            # the host has a second library out of the same other suite.
            if [ -z "$moved" ] && [ "$(apt_unmet)" != "$pkg" ]; then moved=$ver; fi
        done
        ver=${fitted:-$moved}
        [ -n "$ver" ] || return 1
        warn "this host needs $pkg $ver, the version matching libraries it already has"
        apt_pin "$pkg" "$ver"
        if [ -n "$fitted" ]; then return 0; fi
        if apt_sim "${APT_SPEC[@]}"; then return 0; fi
    done
    return 1
}

if [ "$MODE" = check ]; then
    info "preflight only; no changes will be made"
    if [ "$SOURCE" = prebuilt ]; then
        MISSING=()
        for c in curl tar; do command -v "$c" >/dev/null 2>&1 || MISSING+=("$c"); done
        [ ${#MISSING[@]} -eq 0 ] && ok "fetch tools present" || warn "missing: ${MISSING[*]}"
        ok "prebuilt install needs no build toolchain on this host"
    else
        MISSING=()
        for c in clang llvm-config bpftool cargo; do
            command -v "$c" >/dev/null 2>&1 || MISSING+=("$c")
        done
        [ ${#MISSING[@]} -eq 0 ] && ok "toolchain present" || warn "missing: ${MISSING[*]}"
    fi
    # Whether apt can place those packages at all, which is a different
    # question from whether they are installed: on a host whose libraries came
    # from another suite the distribution's own -dev packages cannot be
    # installed as they stand. An install works around it (apt_plan); a
    # preflight only reports what it sees, and changes nothing while it looks.
    if apt_plan "${PKGS[@]}"; then
        ok "apt can install the packages the install needs"
    else
        warn "apt cannot install ${PKGS[*]} on this host as it stands:"
        warn "$(apt_error)"
    fi
    describe_egress
    info "now: congestion control $BEFORE_CC, default_qdisc $BEFORE_DQ, $REPLY"
    while IFS='|' read -r key value file; do
        [ -n "$key" ] || continue
        info "set at every boot by $file: ${key##*.} = $value"
    done <<<"$BOOT_SETTINGS"
    while IFS='|' read -r key value file; do
        [ -n "$key" ] || continue
        info "set in $file (by sysctl -p / sysctl --system, not at boot): ${key##*.} = $value"
    done <<<"$CONF_SETTINGS"
    info "preflight complete"
    printf '\n' >&3
    sponsor '--'
    exit 0
fi

# An active daemon means this run is an upgrade: it gets restarted, not merely
# started, once the new objects have passed the verifier. Its version
# is read now, before the binary is replaced; a 0.2.0 binary has no --version.
UPGRADE=0; HAD_BINARY=0; OLD_VERSION=; OLD_STATUS=; BARE_ATTACHED=0; REATTACHED=0
ENABLE_UNIT_WAS_ACTIVE=0
if [ -x /usr/local/sbin/skyline-speederd ]; then
    HAD_BINARY=1
    OLD_VERSION=$(daemon_version)
fi
if systemctl is-active --quiet skyline-speederd.service 2>/dev/null; then
    UPGRADE=1
    # The restart drops every override made with ssctl (they live only in the
    # daemon's memory). Keep the old state in the log so it can be re-applied.
    log "upgrade: skyline-speederd ${OLD_VERSION:-(version unknown)} is running; its ssctl status --json:"
    OLD_STATUS=$(ssctl_status_json)
    printf '%s\n' "$OLD_STATUS" >>"$LOG"
    # skyline_cc attached with a bare `ssctl enable` (a --no-enable install
    # attached by hand), not by skyline-speeder-enable.service: then no
    # ExecStop drains it when the daemon restarts, and the daemon's own stop
    # deliberately writes no sysctl. The default would go on naming the old
    # daemon's skyline_cc, unregistered and unguarded, and the new daemon
    # would not have attached anything. The installer drains it first itself
    # (below) and, under --no-enable, attaches again afterwards.
    #
    # "Attached" is read from the live host default, not from the old
    # daemon's `enabled`: a drain that timed out (over SSH it always does)
    # has already written fallback_cc but leaves `"enabled": true` behind.
    # That host was detached on purpose, and must not be attached again.
    if systemctl is-active --quiet skyline-speeder-enable.service 2>/dev/null; then
        ENABLE_UNIT_WAS_ACTIVE=1
    elif [ "$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || true)" = skyline_cc ]; then
        BARE_ATTACHED=1
        log "upgrade: skyline_cc is attached without skyline-speeder-enable.service (a bare ssctl enable)"
    fi
fi

step "Installing packages"
if ! run apt-get "${APT_OPTS[@]}" update; then
    # One unusable source fails the whole update even when every suite that
    # matters refreshed -- a suite that no longer exists ("does not have a
    # Release file"), a PPA for a release the host has left behind, a
    # "one-click BBR" script's leftover repository. Say what apt said and
    # carry on: the plan below is what decides whether this host can install
    # what the build needs, and it fails loudly if it cannot. (A source apt
    # merely could not reach does not even get here: apt keeps the lists it
    # has and exits 0.)
    REPLY=$(apt_update_error)
    warn "apt-get update failed${REPLY:+: $REPLY}"
    warn "continuing with the package lists this host already has"
fi
# The plan first, then the install: apt_plan turns the two failures an
# installer can do something about -- an interrupted dpkg run, a library from
# another suite -- into a request that resolves, and everything else into this
# message with apt's own reasoning in the log under it.
if ! apt_plan "${PKGS[@]}"; then
    # "held broken packages" is apt's wording whether or not anything is
    # actually held, so say which packages really are: one held by hand is a
    # cause this installer will not touch.
    HELD=$(apt-mark showhold 2>/dev/null | tr '\n' ' '); HELD=${HELD% }
    die "no set of packages apt can install covers what the build needs.
   apt: $(apt_error)
   The plan it rejected is below and in the full log. A host in this state
   usually carries libraries from a suite its -dev packages do not match: a
   vendor or backports kernel, or a third-party repository.${HELD:+
   Held on this host, and left that way: $HELD}"
fi
# A version taken from the suite this host's libraries came from must not
# quietly take something else off the host with it. On the plain request that
# is apt resolving a conflict as it always has here, and it is only reported;
# when a substitution of ours is what costs the operator a package, the trade
# is not one an installer may make on its own.
if grep -q '^Remv ' "$APT_SIM" 2>/dev/null; then
    REPLY=$(sed -n 's/^Remv \([^ ]*\).*/\1/p' "$APT_SIM" | tr '\n' ' '); REPLY=${REPLY% }
    if [ "$APT_PINNED" -eq 1 ]; then
        die "installing the build prerequisites would remove: $REPLY
   That is what it would cost to take a package from the suite this host's
   libraries came from, and it is not a trade this installer may make for you.
   Install the development packages by hand, or take the repository that put
   the mismatched library here out of the picture, and run this again."
    fi
    warn "apt will remove: $REPLY"
fi
# What dpkg has now, so what apt adds below can be recorded: --uninstall
# removes exactly that set, and a package the host already had must never be
# in it.
dpkg_installed_set >"$WORK/pkgs-before"
run apt-get "${APT_OPTS[@]}" install "${APT_SPEC[@]}" || die "failed to install build prerequisites"
record_added_packages "$WORK/pkgs-before"
if [ "$ADDED_COUNT" -gt 0 ]; then
    ok "prerequisites installed ($ADDED_COUNT package(s) recorded; --uninstall removes them again)"
else
    ok "prerequisites installed (this host already had all of them)"
fi

# --- 6. Rust ---------------------------------------------------------------
# Skipped entirely for a prebuilt install -- the binaries are already built.
if [ "$SOURCE" = build ]; then
step "Preparing the Rust toolchain"
# rust-toolchain.toml pins the channel; rustup honours it automatically inside
# the repo, so only the rustup installation itself is handled here.
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
    else
        info "installing the Rust toolchain via rustup"
        run bash -c "set -euo pipefail; curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --component rustfmt" \
            || die "installing the Rust toolchain via rustup failed"
        PATH="$HOME/.cargo/bin:$PATH"
        # Only this branch installed it, so only this branch records it, and
        # --uninstall therefore never removes a toolchain the operator had
        # before. rustup honours RUSTUP_HOME/CARGO_HOME; nothing here sets
        # them, so what it used is what it defaults to.
        install -d /etc/skyline-speeder
        printf 'RUSTUP_HOME=%q\nCARGO_HOME=%q\n' \
            "${RUSTUP_HOME:-$HOME/.rustup}" "${CARGO_HOME:-$HOME/.cargo}" >"$ADDED_RUSTUP"
        log "recorded the rustup installation at ${CARGO_HOME:-$HOME/.cargo} for --uninstall"
    fi
fi
export PATH
command -v cargo >/dev/null 2>&1 || die "cargo is still not on PATH after rustup install"
# The first cargo run inside the tree is where rustup fetches the pinned
# toolchain. Make it happen here, so the download is not hidden inside the build.
run env -C "$REPO_ROOT" cargo --version \
    || die "cargo cannot run the toolchain rust-toolchain.toml pins"
ok "rust: $(env -C "$REPO_ROOT" cargo --version 2>/dev/null || cargo --version)"

step "Building the BPF objects"
run make -C "$REPO_ROOT" bpf || die "building the BPF objects failed"

step "Building the Rust control plane"
run env -C "$REPO_ROOT" cargo build --workspace --release \
    || die "building the Rust control plane failed"
fi

# --- 7. obtain and install the artifacts -----------------------------------
PREBUILT_ROOT=     # the unpacked artifact's top directory
PROVENANCE=        # one line for the summary; MANIFEST itself goes to the log

fetch_prebuilt() {
    local api="https://api.github.com/repos/${REPO_SLUG}/releases"
    local arch; arch=$(uname -m)
    local staging="$WORK/staging"
    mkdir -p "$staging"

    # An explicit artifact skips release resolution entirely. Two forms, because
    # there are two situations: a mirror or internal artifact store (an https
    # URL), and a host with no route to the internet at all, where the operator
    # copies the tarball over by hand and points at the file. The second form is
    # a plain path rather than file://, because Debian builds curl with the file
    # protocol disabled and --proto '=https' would reject it anyway -- and that
    # restriction is worth keeping.
    local url=${SKYLINE_ARTIFACT_URL:-}
    local local_artifact=
    if [ -n "$url" ] && [ -f "$url" ]; then
        local_artifact=$url
        url=
        info "using local artifact: $local_artifact"
    elif [ -n "$url" ]; then
        info "using SKYLINE_ARTIFACT_URL"
    else

    if [ -n "$RELEASE_TAG" ]; then
        api="$api/tags/$RELEASE_TAG"
    else
        api="$api/latest"
    fi

    info "resolving release from ${REPO_SLUG}"
    run curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/release.json" "$api" \
        || die "cannot reach the release API. Check network access, or use the
   source build (drop --prebuilt), or pass --release <tag>."

    # grep rather than a JSON parser: python3/jq are not guaranteed on a
    # minimal server image, and needing one to install would undercut the
    # point of a toolchain-free path. `|| true`: no match makes grep, and
    # under pipefail the assignment, fail, and set -e would then exit here
    # before the one message that says why.
    url=$(grep -o "https://[^\"]*skyline-speeder-[^\"]*-${arch}\.tar\.gz" \
          "$staging/release.json" | head -1) || true
    [ -n "$url" ] || die "no prebuilt artifact for $arch in that release.
   Published artifacts are per-architecture; build from source instead."
    fi

    if [ -n "$local_artifact" ]; then
        cp -- "$local_artifact" "$staging/artifact.tar.gz"
        [ -r "${local_artifact}.sha256" ] \
            && cp -- "${local_artifact}.sha256" "$staging/artifact.sha256"
    else
        info "downloading $(basename -- "$url")"
        run curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/artifact.tar.gz" "$url" \
            || die "artifact download failed"
        run curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/artifact.sha256" \
            "${url}.sha256" || rm -f "$staging/artifact.sha256"
    fi

    # The digest is published beside the artifact. Verify it: this tarball is
    # about to be unpacked into /usr/local and /opt and run as root.
    local verified="sha256 NOT verified"
    if [ -r "$staging/artifact.sha256" ]; then
        local want got
        want=$(cut -d' ' -f1 < "$staging/artifact.sha256")
        got=$(sha256sum "$staging/artifact.tar.gz" | cut -d' ' -f1)
        [ "$want" = "$got" ] || die "sha256 mismatch
   expected: $want
   actual:   $got"
        ok "sha256 verified"
        verified="sha256 verified"
    else
        warn "no published .sha256 beside the artifact; trusting TLS alone"
    fi

    run tar -xzf "$staging/artifact.tar.gz" -C "$staging" || die "artifact did not unpack"
    PREBUILT_ROOT=$(find "$staging" -mindepth 1 -maxdepth 1 -type d -name 'skyline-speeder-*' | head -1)
    [ -n "$PREBUILT_ROOT" ] && [ -x "$PREBUILT_ROOT/bin/skyline-speederd" ] \
        || die "unexpected artifact layout: no bin/skyline-speederd at the top level"

    PROVENANCE="${PREBUILT_ROOT##*/} ($verified)"
    if [ -r "$PREBUILT_ROOT/MANIFEST" ]; then
        log "artifact provenance (MANIFEST):"
        sed 's/^/     /' -- "$PREBUILT_ROOT/MANIFEST" >>"$LOG"
        local commit
        commit=$(sed -n 's/^[[:space:]]*commit=//p' -- "$PREBUILT_ROOT/MANIFEST" | cut -c1-12)
        [ -z "$commit" ] || PROVENANCE="${PREBUILT_ROOT##*/}, commit $commit ($verified)"
    fi
}

install_prebuilt_files() {
    local root=$PREBUILT_ROOT
    install -d /opt/skyline-speeder/bpf /opt/skyline-speeder/infra \
        /etc/skyline-speeder /run/skyline-speeder /sys/fs/bpf/skyline-speeder
    install -m 0755 "$root/bin/skyline-speederd" /usr/local/sbin/skyline-speederd
    install -m 0755 "$root/bin/ssctl" /usr/local/bin/ssctl
    install -m 0644 "$root"/bpf/*.bpf.o /opt/skyline-speeder/bpf/
    install -m 0755 "$root"/infra/*.sh /opt/skyline-speeder/infra/
    install -m 0644 "$root"/packaging/*.service /etc/systemd/system/
    [ -e /etc/skyline-speeder/speeder.toml ] \
        || install -m 0644 "$root/config/speeder.toml" /etc/skyline-speeder/speeder.toml
    mkdir -p /sys/fs/cgroup/skyline-speeder
    systemctl daemon-reload
    ok "prebuilt artifacts installed (no toolchain was used)"
}

if [ "$SOURCE" = prebuilt ]; then
    step "Downloading the release artifact"
    fetch_prebuilt
    step "Installing files"
    quietly install_prebuilt_files
else
    step "Installing files"
    # The make and cargo runs inside install-guest.sh find everything already
    # built by the two steps above and do nothing.
    run "$REPO_ROOT/infra/install-guest.sh" --confirm-install || die "installing the files failed"
fi

# --- 8. point the TC program at the real egress interface ------------------
# The shipped template targets the reference test bed's interface name, which
# almost never matches a real host. Fix it up on first install only -- an
# existing operator-edited config is never rewritten. The replacement is the
# first Ethernet device a default route (IPv4, then IPv6) leaves through; see
# ethernet_route_interface for why a tunnel is not taken.
step "Configuring the egress interface"
ROUTE_DEV=$(default_route_interface)
ETH_DEV=$(ethernet_route_interface)
PLACEHOLDER=0
if grep -q '^tc_interface = "data0"' "$CFG" 2>/dev/null; then PLACEHOLDER=1; fi
if [ "$PLACEHOLDER" -eq 1 ] && [ -n "$ETH_DEV" ]; then
    sed -i "s|^tc_interface = \"data0\"|tc_interface = \"$ETH_DEV\"|" "$CFG"
    PLACEHOLDER=0
    ok "runtime.tc_interface set to $ETH_DEV"
elif [ "$PLACEHOLDER" -eq 0 ]; then
    ok "runtime.tc_interface left as configured: $(grep '^tc_interface' "$CFG" 2>/dev/null || echo unknown)"
fi
# Nothing has touched a qdisc yet, so a late look is still a "before": `tc`
# may only have arrived with iproute2 in the packages step, and an existing
# config may name another interface than the default route.
NEW_DEV=$(configured_interface)
if [ "$NEW_DEV" != "$DEV" ] || [ -z "$BEFORE_ROOT" ]; then
    DEV=$NEW_DEV
    BEFORE_ROOT=$(root_qdisc_summary "$DEV")
    load_managed "$DEV"
    BEFORE_DEVS=("${MANAGED_DEVS[@]}") BEFORE_ROOTS=("${MANAGED_ROOTS[@]}") BEFORE_LABELS=("${MANAGED_LABELS[@]}")
    describe_egress
    log "egress interface: $REPLY"
fi
# A tc_interface that names no device here (the placeholder kept, a renamed
# NIC, or the bogus "link" 0.2.0's route parser wrote for `default dev wg0
# scope link`) silently leaves skyline_tc detached and no NIC qdisc managed,
# and skyline-speederd refuses a non-Ethernet one. Say which, once.
TC_FIX="set it in $CFG, then: sudo systemctl restart skyline-speederd"
TC_TYPE=
[ -z "$DEV" ] || TC_TYPE=$(cat "/sys/class/net/$DEV/type" 2>/dev/null || true)
if [ -z "$DEV" ]; then
    ok "runtime.tc_interface is not set: skyline_tc and the qdisc guard leave every interface alone"
elif [ "$PLACEHOLDER" -eq 1 ] && [ -z "$ROUTE_DEV" ]; then
    warn "runtime.tc_interface is still the template's placeholder \"$DEV\" and no default route (IPv4 or IPv6) was found to replace it; $TC_FIX"
elif [ "$PLACEHOLDER" -eq 1 ]; then
    warn "runtime.tc_interface is still the template's placeholder \"$DEV\": the default route leaves through $ROUTE_DEV, which is not an Ethernet device (type $(cat "/sys/class/net/$ROUTE_DEV/type" 2>/dev/null || echo unknown): a tunnel such as WireGuard/WARP, tun, gre or ppp). skyline_tc and the qdisc guard need the NIC that carries its traffic; $TC_FIX"
elif [ ! -e "/sys/class/net/$DEV" ]; then
    warn "runtime.tc_interface $DEV does not exist on this host (default route: ${ROUTE_DEV:-none}); until it names the NIC, skyline_tc is not attached and no NIC qdisc is managed: $TC_FIX"
elif [ -n "$TC_TYPE" ] && [ "$TC_TYPE" != 1 ]; then
    warn "runtime.tc_interface $DEV is not an Ethernet device (type $TC_TYPE, e.g. a tunnel): skyline-speederd does not attach skyline_tc to it, and the qdisc guard does not reach the NIC that carries its traffic. Point it at the NIC: $TC_FIX"
fi

# --- 9. validate before starting anything ----------------------------------
# --validate-only --verify-bpf pushes all three objects through the verifier and
# exits without leaving runtime state, so a kernel/BTF mismatch surfaces here
# rather than as a half-started daemon.
step "Running the kernel verifier"
# The failure is not necessarily the verifier's: a missing kernel capability
# stops the run before any object is loaded. Hand over the whole command --
# someone who came in through `curl | bash` has no idea where its output went.
run /usr/local/sbin/skyline-speederd --config "$CFG" --validate-only --verify-bpf \
    || die "validation failed (the reason is on the 'Error:' line in the log below).
   For the capability report and the verifier log, run:
     /usr/local/sbin/skyline-speederd --config $CFG --validate-only --verify-bpf"
ok "all BPF objects passed the kernel verifier"

# Snapshot before anything starts changing sysctls: the daemon writes
# fallback_cc on load and `ssctl enable` writes skyline_cc after it attaches.
# Only written once -- a reinstall must not record skyline_cc as the "original".
# The root qdiscs are those of the devices the guard will manage: two
# parallel, space-separated lists (one device: just its name and summary).
ROOTS_SEEN=
for i in "${!BEFORE_DEVS[@]}"; do
    ROOTS_SEEN="${ROOTS_SEEN:+$ROOTS_SEEN, }${BEFORE_DEVS[i]} ${BEFORE_ROOTS[i]}"
done
if [ ! -e "$STATE" ]; then
    install -d /etc/skyline-speeder
    cat > "$STATE" <<EOF
PRE_INSTALL_CC=$BEFORE_CC
PRE_INSTALL_QDISC=$BEFORE_DQ
EOF
    # Both root keys, always, empty when no device is known: the elif below
    # must only ever fill in a snapshot from before the guard existed. Filling
    # in one written here later could record the guard's own fq as the
    # "original".
    printf 'PRE_INSTALL_QDISC_DEV=%q\nPRE_INSTALL_ROOT_QDISC=%q\n' \
        "${BEFORE_DEVS[*]}" "${BEFORE_ROOTS[*]}" >>"$STATE"
    ok "recorded pre-install state: cc=$BEFORE_CC qdisc=$BEFORE_DQ root: ${ROOTS_SEEN:-none}"
elif ! grep -q '^PRE_INSTALL_ROOT_QDISC=' "$STATE"; then
    # Only a snapshot from before the guard existed (0.2.0) has no such line
    # at all. Those versions never touched a root qdisc, so what is there now
    # is still the original -- but only on this, the first run of an
    # installer that knows about the guard: from here on the guard may have
    # replaced it. So the snapshot is closed now, with empty lists when no
    # managed device is known yet (say, a stale tc_interface the operator
    # fixes later); uninstall skips empty keys, and a later run must never
    # fill it in with the guard's own fq.
    printf 'PRE_INSTALL_QDISC_DEV=%q\nPRE_INSTALL_ROOT_QDISC=%q\n' \
        "${BEFORE_DEVS[*]}" "${BEFORE_ROOTS[*]}" >>"$STATE"
    if [ -n "$ROOTS_SEEN" ]; then
        ok "added the root qdisc ($ROOTS_SEEN) to the pre-install snapshot"
    else
        log "closed the pre-install snapshot without a root qdisc (no managed device known)"
    fi
fi

SOCKET=$(awk -F'"' '/^socket_path[[:space:]]*=/ { print $2; exit }' "$CFG" 2>/dev/null || true)
SOCKET=${SOCKET:-/run/skyline-speeder/speeder.sock}

# skyline-speederd.service has no readiness notification: `systemctl start`
# returns once the process exists, not once the control socket does.
wait_for_daemon() {
    local deadline=$((SECONDS + 60)) state
    while [ ! -S "$SOCKET" ]; do
        # "activating" is Restart=on-failure between attempts: keep waiting.
        state=$(systemctl is-active skyline-speederd.service 2>/dev/null || true)
        case "$state" in failed|inactive) return 1 ;; esac
        [ "$SECONDS" -lt "$deadline" ] || return 1
        SPIN_I=$((SPIN_I + 1)); bar_draw; sleep 0.2
    done
}

daemon_failed() {
    journalctl -u skyline-speederd.service -n 40 --no-pager >>"$LOG" 2>&1 || true
    STEP_RAN=1
    die "$1. Check: journalctl -u skyline-speederd.service"
}

if [ "$UPGRADE" -eq 1 ]; then
    # Requires= in the enable unit makes systemd restart it with the daemon:
    # boot-disable.sh drains (up to 60 s; over SSH it always runs out, which is
    # harmless), the new daemon starts, boot-enable.sh re-attaches skyline_cc.
    # That only happens while the enable unit is active; see BARE_ATTACHED.
    step "Draining live flows (up to 60 s), restarting skyline-speederd"
    if [ "$BARE_ATTACHED" -eq 1 ]; then
        # The drain boot-disable.sh would have run, run here instead: `ssctl
        # drain` switches the default to fallback_cc first, then waits for
        # the flows. This is the installer doing the documented manual step,
        # not the daemon writing a sysctl on its own stop (which it
        # deliberately never does). Over SSH the wait always runs out -- the
        # session is one of the flows -- and that error is harmless; the
        # outer timeout only bounds an old daemon that does not answer.
        run timeout 75 /usr/local/bin/ssctl drain --timeout 60 \
            || log "drain ended with an error (over SSH a timeout is expected; fallback_cc was written first)"
    fi
    run systemctl enable skyline-speederd.service || die "systemctl enable skyline-speederd.service failed"
    run systemctl restart skyline-speederd.service || daemon_failed "restarting skyline-speederd failed"
else
    step "Starting skyline-speederd"
    run systemctl enable --now skyline-speederd.service || daemon_failed "starting skyline-speederd failed"
fi
wait_for_daemon || daemon_failed "skyline-speederd did not open $SOCKET within 60 s"
ok "skyline-speederd.service running"
NEW_VERSION=$(daemon_version)
if [ "$ENABLE" -eq 0 ] && [ "$ENABLE_UNIT_WAS_ACTIVE" -eq 1 ]; then
    # The restart propagated to the enable unit, but `systemctl restart` only
    # waits for skyline-speederd's own job: boot-enable.sh may still be
    # polling for the socket. `start` joins that pending job and waits for it
    # (and attaches nothing that was not attached before the upgrade), so the
    # summary below reads the state the host actually ends up in.
    run timeout 130 systemctl start skyline-speeder-enable.service \
        || warn "skyline-speeder-enable.service did not come back after the restart; check: journalctl -u skyline-speeder-enable.service"
fi
if [ "$BARE_ATTACHED" -eq 1 ] && [ "$ENABLE" -eq 0 ]; then
    # Leave the host as it was: attached by hand, and so, like before, not
    # at the next boot. (With the enable unit, the step below attaches.)
    if run /usr/local/bin/ssctl enable; then
        REATTACHED=1
        ok "skyline_cc attached again, as it was before the upgrade"
    else
        warn "attaching skyline_cc again after the upgrade failed; run: sudo ssctl enable"
    fi
fi
read_status
classify_guard
if [ "$HAS_GUARD" -eq 0 ]; then
    warn "this skyline-speederd${NEW_VERSION:+ ($NEW_VERSION)} is a release older than the guard: it attaches skyline_cc, but does not keep it the host default and manages no qdisc. A later release, or a source build (without --prebuilt/--release), has the guard."
fi

VERSION_ROW=
if [ "$HAD_BINARY" -eq 1 ]; then
    if [ -z "$OLD_VERSION" ]; then
        # A binary without --version: the published v0.2.0 release or older.
        # (A build of main between releases may carry the same number as the
        # last release, so the number alone does not tell the two apart.)
        VERSION_ROW="from a release without --version (v0.2.0 or older)${NEW_VERSION:+ to $NEW_VERSION}"
    elif [ "$OLD_VERSION" != "$NEW_VERSION" ]; then
        VERSION_ROW="$OLD_VERSION -> ${NEW_VERSION:-unknown}"
    fi
fi

# Settings made with ssctl live only in the old daemon's memory.
RESTART_ROW="live ssctl settings did not carry over (old status in the log)"

# --- 10. summary helpers ----------------------------------------------------
row() { printf '     %-19s %s\n' "$1" "$2" >&3; }
was() { if [ -n "$2" ] && [ "$1" != "$2" ]; then printf ' (was %s)' "$2"; fi; }

# The pre-install root of a managed device, into REPLY (empty if unknown).
before_of() {
    local i
    REPLY=
    for i in "${!BEFORE_DEVS[@]}"; do
        if [ "${BEFORE_DEVS[i]}" = "$1" ]; then REPLY=${BEFORE_ROOTS[i]}; return 0; fi
    done
}

# "eth0", or "eth0, eth1 (under bond0)": the devices the guard keeps fq on.
managed_names() {
    local IFS=,
    REPLY="${BEFORE_DEVS[*]}"
    REPLY=${REPLY//,/, }
    if [ "${#BEFORE_DEVS[@]}" -gt 0 ] && [ "${BEFORE_DEVS[0]}" != "$DEV" ]; then
        REPLY="$REPLY (under $DEV)"
    fi
}

finish_bar() {
    step_end
    fmt_secs "$SECONDS"
    if [ "$BAR" -eq 1 ]; then
        printf '\r\033[K[####################] 100%%  (%d/%d) done in %s\n' \
            "$STEP_TOTAL" "$STEP_TOTAL" "$REPLY" >&3
        BAR_SHOWN=0
    elif [ "$QUIET" -eq 1 ]; then
        printf 'all %d steps done in %s\n' "$STEP_TOTAL" "$REPLY" >&3
    fi
    printf '\n' >&3
}

GUARD_QDISC=$(status_value qdisc)
GUARD_INTERVAL=$(status_value interval_s)
FALLBACK=$(status_value fallback_cc)

if [ "$ENABLE" -eq 0 ]; then
    # --no-enable never disables anything: an earlier install's enable unit,
    # or an attach made by hand, stays in place. So report what IS in place
    # -- the new daemon's own "enabled" and the host default -- rather than
    # what a unit file says.
    NOW_CC=$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)
    NOW_ENABLED=$(status_value enabled)
    UNIT_ACTIVE=0 UNIT_ENABLED=0
    if systemctl is-active --quiet skyline-speeder-enable.service 2>/dev/null; then UNIT_ACTIVE=1; fi
    if systemctl is-enabled --quiet skyline-speeder-enable.service 2>/dev/null; then UNIT_ENABLED=1; fi
    UNIT_ON=$((UNIT_ACTIVE | UNIT_ENABLED))
    # Over an enable unit that is already active, `enable --now` does nothing.
    if [ "$UNIT_ACTIVE" -eq 0 ]; then
        ATTACH_CMD="sudo systemctl enable --now skyline-speeder-enable.service"
    elif [ "$UNIT_ENABLED" -eq 1 ]; then
        ATTACH_CMD="sudo systemctl restart skyline-speeder-enable.service"
    else
        ATTACH_CMD="sudo systemctl enable skyline-speeder-enable.service && sudo systemctl restart skyline-speeder-enable.service"
    fi
    if [ -z "$STATUS_JSON" ]; then
        ATTACH_STATE="ssctl status did not answer, so whether skyline_cc is attached is unknown"
    elif [ "$NOW_ENABLED" = true ] && [ "$UNIT_ON" -eq 1 ]; then
        ATTACH_STATE="skyline_cc stays attached (skyline-speeder-enable.service, from an earlier install)"
    elif [ "$NOW_ENABLED" = true ] && [ "$REATTACHED" -eq 1 ]; then
        ATTACH_STATE="skyline_cc is attached again, as it was before the upgrade"
    elif [ "$NOW_ENABLED" = true ]; then
        ATTACH_STATE="skyline_cc is attached"
    elif [ "$NOW_CC" = skyline_cc ]; then
        ATTACH_STATE="skyline_cc is not attached by it, yet the host default still names skyline_cc"
    else
        ATTACH_STATE="skyline_cc is not attached (--no-enable)"
    fi
    finish_bar
    printf ' %sok%s  Skyline Speeder %sis installed; %s.\n' \
        "$GRN" "$RST" "${NEW_VERSION:+$NEW_VERSION }" "$ATTACH_STATE" >&3
    [ -z "$VERSION_ROW" ] || row "upgraded" "$VERSION_ROW"
    [ "$UPGRADE" -eq 0 ] || row "restarted" "$RESTART_ROW"
    [ -z "$PROVENANCE" ] || row "artifact" "$PROVENANCE"
    row "congestion control" "$NOW_CC (host default)"
    row "install log" "$LOG"
    printf '\n' >&3
    if [ -z "$STATUS_JSON" ]; then
        printf '     Check: sudo ssctl status; journalctl -u skyline-speederd.service\n' >&3
    elif [ "$NOW_ENABLED" = true ] && [ "$UNIT_ON" -eq 1 ]; then
        printf '     To detach it: sudo systemctl disable --now skyline-speeder-enable.service\n' >&3
    elif [ "$NOW_ENABLED" = true ]; then
        printf '     Like before, it is not attached again after a reboot. To attach it at\n' >&3
        printf '     every boot:  sudo systemctl enable --now skyline-speeder-enable.service\n' >&3
        printf '     To detach:   sudo ssctl drain --timeout 60\n' >&3
    elif [ "$NOW_CC" = skyline_cc ]; then
        # The state BARE_ATTACHED prevents, reached some other way (say, the
        # daemon was killed out of band): the default names a skyline_cc that
        # nothing controls any more.
        printf '     That skyline_cc was left by a skyline-speederd that is gone: new\n' >&3
        printf '     connections still get it, but nothing controls or tunes it any more.\n' >&3
        if [ "$UNIT_ON" -eq 1 ]; then
            printf '     Attach this build:  %s\n' "$ATTACH_CMD" >&3
            printf '     or detach it:       sudo systemctl disable --now skyline-speeder-enable.service\n' >&3
        else
            printf '     Attach this build:  sudo ssctl enable   (and at every boot:\n' >&3
            printf '                         %s)\n' "$ATTACH_CMD" >&3
            printf '     or detach it:       sudo ssctl drain --timeout 60\n' >&3
        fi
    else
        printf '     New connections use %s. Attach skyline_cc now and at every boot' "$NOW_CC" >&3
        if [ "$HAS_GUARD" -eq 1 ] && [ "$GUARD_QDISC" != false ]; then
            managed_names
            if [ -n "$REPLY" ]; then
                printf '\n     (skyline-speederd then also keeps fq on %s)' "$REPLY" >&3
            else
                printf '\n     (skyline-speederd then also keeps net.core.default_qdisc at fq)' >&3
            fi
        fi
        printf ':\n       %s\n' "$ATTACH_CMD" >&3
    fi
    printf '\n' >&3
    sponsor '--'
    exit 0
fi

# --- 11. attach and persist ------------------------------------------------
step "Attaching skyline_cc"
run systemctl enable --now skyline-speeder-enable.service || {
    journalctl -u skyline-speeder-enable.service -n 40 --no-pager >>"$LOG" 2>&1 || true
    die "attaching skyline_cc failed. Check: journalctl -u skyline-speeder-enable.service"
}

step "Verifying"
# Ask the daemon, not only the sysctl: `enable --now` above is a no-op when
# the enable unit is already active. After skyline-speederd was killed out of
# band, for one, its enable unit stays "active (exited)", the fresh daemon
# has attached nothing, and the default still names the dead one's
# skyline_cc -- the cc alone would look fine while nothing guards or tunes it.
attach_problem() {
    read_status
    classify_guard
    ACTIVE=$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)
    ATTACH_PROBLEM=
    if [ -z "$STATUS_JSON" ]; then
        ATTACH_PROBLEM="ssctl status did not answer"
    elif [ "$(status_value enabled)" != true ]; then
        ATTACH_PROBLEM="skyline-speederd reports \"enabled\": false"
    elif [ "$HAS_GUARD" -eq 1 ] && [ "$(status_value armed)" != true ]; then
        ATTACH_PROBLEM="skyline-speederd reports its guard not armed"
    elif [ "$ACTIVE" != skyline_cc ]; then
        ATTACH_PROBLEM="the default congestion control is '$ACTIVE'"
    fi
}
attach_problem
if [ -n "$ATTACH_PROBLEM" ]; then
    log "not attached after the enable unit ran ($ATTACH_PROBLEM); restarting skyline-speeder-enable.service once"
    run systemctl restart skyline-speeder-enable.service || true
    attach_problem
fi
if [ -n "$ATTACH_PROBLEM" ]; then
    journalctl -u skyline-speeder-enable.service -n 40 --no-pager >>"$LOG" 2>&1 || true
    STEP_RAN=1
    die "skyline_cc is not attached: $ATTACH_PROBLEM.
   Attach it by hand:  sudo ssctl enable
   Why it failed:      journalctl -u skyline-speeder-enable.service"
fi
ok "active congestion control: $ACTIVE"
GUARD_QDISC=$(status_value qdisc)
GUARD_INTERVAL=$(status_value interval_s)
FALLBACK=$(status_value fallback_cc)
AFTER_DQ=$(sysctl -n net.core.default_qdisc 2>/dev/null || echo unknown)
AFTER_ROOT=$(root_qdisc_summary "$DEV")

# The "after" side: the devices as the daemon itself resolved them
# (guard.live.devices) when it reports them, else the same rule run here.
AFTER_DEVS=() AFTER_ROOTS=()
case "$STATUS_JSON" in
    *'"devices":'*) DAEMON_DEVS=1 ;;
    *) DAEMON_DEVS=0 ;;
esac
if [ "$HAS_GUARD" -eq 1 ] && [ "$GUARD_QDISC" != false ] && [ "$DAEMON_DEVS" -eq 1 ]; then
    while IFS='|' read -r d q; do
        [ -n "$d" ] || continue
        AFTER_DEVS+=("$d")
        AFTER_ROOTS+=("${q:-$(root_qdisc_summary "$d")}")
    done < <(status_devices)
else
    load_managed "$DEV"
    AFTER_DEVS=("${MANAGED_DEVS[@]}") AFTER_ROOTS=("${MANAGED_ROOTS[@]}")
fi
AFTER_SEEN=
for i in "${!AFTER_DEVS[@]}"; do
    AFTER_SEEN="${AFTER_SEEN:+$AFTER_SEEN, }${AFTER_DEVS[i]} ${AFTER_ROOTS[i]:-unknown}"
done
ok "default qdisc: $AFTER_DQ; ${DEV:-no egress interface} root qdisc: ${AFTER_ROOT:-unknown}${AFTER_SEEN:+; managed: $AFTER_SEEN}"

# One row per managed device -- what it carries now, and what it was or why it
# was left alone -- with the default qdisc added to the first.
QDISC_ROWS=()
if [ "$HAS_GUARD" -eq 1 ] && [ "$GUARD_QDISC" != false ] && [ "$AFTER_DQ" != fq ]; then
    warn "net.core.default_qdisc is $AFTER_DQ, not fq. See DRIFT GUARD in: sudo ssctl status"
fi
for i in "${!AFTER_DEVS[@]}"; do
    d=${AFTER_DEVS[i]} now=${AFTER_ROOTS[i]}
    dev_label "$d"; label=$REPLY
    before_of "$d"; prev=$REPLY
    if [ "$HAS_GUARD" -eq 0 ] || [ "$GUARD_QDISC" = false ]; then
        text="${now:-unknown} on $label$(was "${now:-unknown}" "$prev")"
    else
        case "$now" in
            fq|mq/fq) text="$now on $label$(was "$now" "$prev")" ;;
            '') text="unknown on $label" ;;
            *)
                shaping=$(root_qdisc_shaping "$d")
                if [ -n "$shaping" ]; then
                    text="$now on $label ($shaping: it shapes, so it was left alone)"
                elif qdisc_replaceable "$now"; then
                    text="$now on $label (not replaced)"
                    warn "$label root qdisc is still $now, not fq. See \"last error\" under DRIFT GUARD in: sudo ssctl status"
                else
                    text="$now on $label (looks built on purpose, so it was left alone)"
                fi ;;
        esac
    fi
    QDISC_ROWS+=("$text")
done
DQ_TEXT="default $AFTER_DQ$(was "$AFTER_DQ" "$BEFORE_DQ")"
if [ "${#QDISC_ROWS[@]}" -gt 0 ]; then
    QDISC_ROWS[0]="${QDISC_ROWS[0]}, $DQ_TEXT"
elif [ -z "$DEV" ]; then
    QDISC_ROWS=("$DQ_TEXT (no runtime.tc_interface to put fq on)")
elif [ ! -e "/sys/class/net/$DEV" ]; then
    QDISC_ROWS=("$DQ_TEXT (runtime.tc_interface $DEV does not exist; no interface qdisc managed)")
elif [ "$AFTER_ROOT" = noqueue ]; then
    QDISC_ROWS=("$DQ_TEXT" "$DEV is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked")
else
    QDISC_ROWS=("$DQ_TEXT, ${AFTER_ROOT:-unknown} on $DEV")
fi
if [ "$HAS_GUARD" -eq 0 ]; then
    QDISC_ROWS[0]="${QDISC_ROWS[0]} (not managed by this release)"
elif [ "$GUARD_QDISC" = false ]; then
    QDISC_ROWS[0]="left alone ([guard] qdisc = false): ${QDISC_ROWS[0]}"
fi
case "$GUARD_INTERVAL" in
    '') GUARD_ROW="skyline-speederd (see DRIFT GUARD in ssctl status)" ;;
    0)  GUARD_ROW="skyline-speederd, when it attaches only ([guard] interval_s = 0)" ;;
    *)  GUARD_ROW="skyline-speederd, re-checked every $GUARD_INTERVAL s (DRIFT GUARD in ssctl status)" ;;
esac

# --- 12. summary and guide ---------------------------------------------------
finish_bar
printf ' %sok%s  Skyline Speeder %sis installed and running -- ready to use.\n' \
    "$GRN" "$RST" "${NEW_VERSION:+$NEW_VERSION }" >&3
row "congestion control" "skyline_cc$(was skyline_cc "$BEFORE_CC") -- every new TCP connection on this host"
row "qdisc" "${QDISC_ROWS[0]}"
for text in "${QDISC_ROWS[@]:1}"; do row "" "$text"; done
[ "$HAS_GUARD" -eq 0 ] || row "kept in place by" "$GUARD_ROW"
[ -z "$VERSION_ROW" ] || row "upgraded" "$VERSION_ROW"
[ "$UPGRADE" -eq 0 ] || row "restarted" "$RESTART_ROW"
[ -z "$PROVENANCE" ] || row "artifact" "$PROVENANCE"
row "install log" "$LOG"

# Files that set something else. Only the qdisc ones the daemon actually
# overrides are worth a word; a default_qdisc file is not fighting anyone
# when [guard] qdisc = false, or under a release without the guard.
declare -A NOTE_SETS=() NOTE_WHEN=()
NOTE_FILES=()
while IFS='|' read -r when key value file; do
    case "$key" in
        net.ipv4.tcp_congestion_control) [ "$value" != skyline_cc ] || continue ;;
        net.core.default_qdisc)
            [ "$value" != fq ] && [ "$HAS_GUARD" -eq 1 ] && [ "$GUARD_QDISC" != false ] || continue ;;
        *) continue ;;
    esac
    if [ -z "${NOTE_SETS[$file]:-}" ]; then NOTE_FILES+=("$file"); NOTE_WHEN[$file]=$when; fi
    NOTE_SETS[$file]="${NOTE_SETS[$file]:+${NOTE_SETS[$file]} }${key##*.}=$value"
done < <(
    [ -z "$BOOT_SETTINGS" ] || printf 'boot|%s\n' "$BOOT_SETTINGS" | sed '2,$s/^/boot|/'
    [ -z "$CONF_SETTINGS" ] || printf 'manual|%s\n' "$CONF_SETTINGS" | sed '2,$s/^/manual|/'
)
if [ "${#NOTE_FILES[@]}" -gt 0 ]; then
    if [ "$HAS_GUARD" -eq 0 ] || [ "${GUARD_INTERVAL:-5}" = 0 ]; then KEEP="each time it attaches skyline_cc"
    else KEEP="once it attaches skyline_cc, and keeps them overridden"; fi
    printf '\n %snote%s  These files set something else and are left as is; skyline-speederd\n' "$YLW" "$RST" >&3
    printf '       overrides them %s:\n' "$KEEP" >&3
    for file in "${NOTE_FILES[@]}"; do
        case "${NOTE_WHEN[$file]}:$file" in
            manual:*) origin="  (by sysctl -p / sysctl --system only, not at boot)" ;;
            *:/usr/lib/*|*:/lib/*) origin="  (every boot; distribution default)" ;;
            *) origin="  (every boot)" ;;
        esac
        printf '       %s  %s%s\n' "$file" "${NOTE_SETS[$file]}" "$origin" >&3
    done
fi
STATUS_WHAT="is it attached, is it the host default, kernel support"
[ "$HAS_GUARD" -eq 0 ] || STATUS_WHAT="$STATUS_WHAT, guard"

# The path this run came from, so the uninstall command in the guide is one the
# operator can paste. scripts/bootstrap.sh leaves the tree in
# /usr/local/src/skyline-speeder; somebody who piped this file straight into
# bash has no tree at all and is told that is what it takes.
if [ -r "$REPO_ROOT/install.sh" ]; then
    UNINSTALL_CMD="sudo $REPO_ROOT/install.sh --uninstall"
else
    UNINSTALL_CMD="sudo <a Skyline Speeder source tree>/install.sh --uninstall"
fi

# What --restore-pre-install would put back, named value by value: on a host
# that was already on bbr with default_qdisc fq, the NIC's root qdisc is the
# only thing that differs, and "bbr + fq instead of bbr + fq" would read as a
# bug rather than as the truth.
#
# Read from $STATE, in a subshell, because that is the file the restore itself
# reads -- and on a reinstall it holds the FIRST install's values, which are not
# what this run sees now. Without a snapshot yet, this run's own observation is
# what is about to be written into one.
RESTORE_DESC=$(
    PRE_INSTALL_CC=$BEFORE_CC PRE_INSTALL_QDISC=$BEFORE_DQ
    PRE_INSTALL_QDISC_DEV="${BEFORE_DEVS[*]}" PRE_INSTALL_ROOT_QDISC="${BEFORE_ROOTS[*]}"
    # shellcheck disable=SC1090  # a generated key=value file
    [ ! -r "$STATE" ] || . "$STATE"
    desc="congestion control ${PRE_INSTALL_CC:-unknown}, default_qdisc ${PRE_INSTALL_QDISC:-unknown}"
    read -ra devs <<<"${PRE_INSTALL_QDISC_DEV:-}"
    read -ra roots <<<"${PRE_INSTALL_ROOT_QDISC:-}"
    if [ "${#devs[@]}" -eq "${#roots[@]}" ]; then
        for i in "${!devs[@]}"; do desc="$desc, ${devs[i]} root ${roots[i]}"; done
    fi
    printf '%s' "$desc"
)

# A few of the common parameters docs/usage.md section 4 explains, each shown
# with the value the daemon runs right now (module_tuning in ssctl status --json).
# Every description here is that guide's, shortened; keep them in step.
knob() {
    local value
    value=$(status_value "$2")
    printf -v REPLY '%s %s' "$1" "${value:-<n>}"
    printf '   %-31s %s\n' "$REPLY" "$3" >&3
}

cat >&3 <<EOF

 ${BLD}Everyday commands${RST}
   sudo ssctl status               $STATUS_WHAT
   sudo ssctl flows                the accelerated connections, the coefficients
                                   in force on them, and what the algorithm did
                                   (--json on either one for the raw reply)
   sudo ssctl drain --timeout 60   graceful detach: new connections use ${FALLBACK:-cubic};
                                   over SSH it ends in a harmless timeout error
   sudo ssctl enable               attach again after a drain
   sudo systemctl restart skyline-speederd
                                   apply speeder.toml edits (drains, re-attaches)

 ${BLD}Tuning${RST}  (full guide: $GUIDE_URL)
   The shipped values are already tuned; most hosts need no change. The guide
   has four ready-made presets. The knobs worth knowing, with current values:
EOF
knob --cruise-pacing-gain cruise_pacing_gain "steady rate = this x estimated bandwidth"
knob --guardrail-gain guardrail_gain "on real congestion, slow to this share"
knob --loss-inflation-max-ratio loss_inflation_max_ratio "loss made up for, at most (0.10 ~ +11%)"
knob --max-queue-delay-ms max_queue_delay_ms "queueing delay treated as congestion"
knob --max-pacing-mbps max_pacing_mbps "pacing cap per connection, not host-wide"
cat >&3 <<EOF
   Live:        sudo ssctl set-module-config --max-pacing-mbps 1200 ...
                Absolute: each flag left out is sent as its built-in default --
                copy the current values from sudo ssctl flows first. Lost
                when skyline-speederd restarts.
                sudo ssctl reset-module-config    back to the file's values
   Persistent:  edit $CFG, check it with
                sudo skyline-speederd --config $CFG --validate-only
                then sudo systemctl restart skyline-speederd
EOF

# Printed rather than put in the heredoc above: what it can honestly say about
# packages depends on how many there are to remove, and the pre-install values
# are worth naming one by one -- on the usual host the root qdisc is the only
# one that differs from bbr + fq.
printf '\n %sUninstall%s\n   %s\n' "$BLD" "$RST" "$UNINSTALL_CMD" >&3
printf '%s\n' \
    "                drains live flows, removes the units, binaries and BPF" \
    "                objects, and puts this host on bbr + fq" >&3
if [ "$ADDED_REMOVABLE" -gt 0 ]; then
    printf '%s\n' \
        "                It also removes the $ADDED_REMOVABLE package(s) this install added." \
        "                iproute2, curl, ca-certificates and tar are never among" \
        "                them; the list is in" \
        "                $ADDED_PKGS" >&3
else
    printf '%s\n' \
        "                This install added no package an uninstall would remove." >&3
fi
printf '   %s\n' "$UNINSTALL_CMD --restore-pre-install" >&3
printf '%s\n' \
    "                the same, but puts back what this host ran before the" \
    "                install instead of bbr + fq:" \
    "                $RESTORE_DESC" >&3
printf '%s\n' \
    "   The configuration is kept either way, so a reinstall keeps your settings." \
    "   bbr and fq are set for that boot only: no file under /etc/sysctl.d is" \
    "   written, so those files decide again after a reboot." >&3

cat >&3 <<EOF

 ${BLD}Note${RST}  dynamic RTO (the sockops policy; off by default) only sees processes in
       /sys/fs/cgroup/skyline-speeder. Opt a service in with:
         sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <command...>
EOF

printf '\n' >&3
sponsor '--'
