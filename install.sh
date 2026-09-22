#!/usr/bin/env bash
#
# Skyline Speeder -- one-click installer for Debian / Ubuntu.
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
#   sudo ./install.sh --uninstall      # remove, and put back the pre-install cc/qdisc
#
# The terminal shows one progress line. Everything the steps print goes to
# /var/log/skyline-speeder-install.log, which is kept; if a step fails, its
# last lines are shown together with that path.
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
REPO_SLUG=${SKYLINE_REPO:-CYBERVERSE-Research/skyline-speeder}
case "${SKYLINE_VERBOSE:-0}" in 1|yes|true) VERBOSE=1 ;; *) VERBOSE=0 ;; esac
LOG=/var/log/skyline-speeder-install.log
CFG=/etc/skyline-speeder/speeder.toml
STATE=/etc/skyline-speeder/pre-install-state
GUIDE_URL=https://github.com/CYBERVERSE-Research/skyline-speeder/blob/main/docs/usage.md

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
SPIN_I=0 COLS=80 BG_PID= WORK= FAILED=0 INTERRUPTED=0

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
}
trap on_exit EXIT
trap on_interrupt INT TERM

# --- what is in place now ----------------------------------------------------
# Root qdisc of an interface as one word: "fq", "cake", "fq_codel", or "mq/"
# followed by the distinct kinds under an mq root, sorted ("mq/fq_codel",
# "mq/cake,fq"). The same format `ssctl status` shows as guard.live.
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

# `ssctl status` pretty-prints one "key": value per line. awk rather than a
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

read_status() {
    STATUS_JSON=$(timeout 10 /usr/local/bin/ssctl status 2>>"$LOG" || true)
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

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check) MODE=check; shift ;;
        --no-enable) ENABLE=0; shift ;;
        --uninstall) MODE=uninstall; shift ;;
        --prebuilt) SOURCE=prebuilt; shift ;;
        --release) [ "$#" -ge 2 ] || die "--release needs a tag"; SOURCE=prebuilt; RELEASE_TAG="$2"; shift 2 ;;
        --verbose) VERBOSE=1; shift ;;
        -h|--help) sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "must run as root (try: sudo $0)"

if [ "$MODE" = uninstall ]; then
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

    # Put back what was in place before the install. Without this an operator
    # who ran BBR before installing is silently left on `fallback_cc` (cubic)
    # after uninstalling -- a downgrade nobody asked for, and a confusing one
    # because nothing reports it. The snapshot lives under /etc/skyline-speeder,
    # which uninstall deliberately keeps.
    if [ -r "$STATE" ]; then
        # shellcheck disable=SC1090  # a generated key=value file
        . "$STATE"
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
        if [ -n "${PRE_INSTALL_QDISC_DEV:-}" ] && [ -n "${PRE_INSTALL_ROOT_QDISC:-}" ]; then
            read -ra RESTORE_DEVS <<<"$PRE_INSTALL_QDISC_DEV"
            read -ra RESTORE_ROOTS <<<"$PRE_INSTALL_ROOT_QDISC"
            if [ "${#RESTORE_DEVS[@]}" -ne "${#RESTORE_ROOTS[@]}" ]; then
                warn "$STATE lists ${#RESTORE_DEVS[@]} interface(s) but ${#RESTORE_ROOTS[@]} root qdisc(s); no root qdisc restored"
            else
                DFLT_NOW=$(sysctl -n net.core.default_qdisc 2>/dev/null || echo "${PRE_INSTALL_QDISC:-fq_codel}")
                for i in "${!RESTORE_DEVS[@]}"; do
                    restore_root_qdisc "${RESTORE_DEVS[i]}" "${RESTORE_ROOTS[i]}" "$DFLT_NOW"
                done
            fi
        fi
    else
        warn "no pre-install snapshot at $STATE; leaving sysctls as they are."
        warn "Current: cc=$(sysctl -n net.ipv4.tcp_congestion_control) qdisc=$(sysctl -n net.core.default_qdisc)"
    fi
    # /etc/skyline-speeder is left in place on purpose: it holds operator-tuned
    # configuration that a reinstall should not silently discard.
    ok "removed (configuration kept at /etc/skyline-speeder)"
    # Report, do not "fix": force-detaching a struct_ops that live sockets still
    # use is not something an uninstaller should do behind the operator's back.
    LEFT=$(bpftool struct_ops show 2>/dev/null | grep -c skyline_cc || true)
    if [ "${LEFT:-0}" -gt 0 ]; then
        warn "$LEFT skyline_cc struct_ops map(s) are still held by established connections."
        warn "This is normal reference-counting behaviour, not a failure: the kernel frees"
        warn "them once those connections close, or at the next reboot. New connections"
        warn "already use $(sysctl -n net.ipv4.tcp_congestion_control)."
    fi
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
    log "upgrade: skyline-speederd ${OLD_VERSION:-(version unknown)} is running; its ssctl status:"
    OLD_STATUS=$(timeout 10 /usr/local/bin/ssctl status 2>>"$LOG" || true)
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
# `-qq` silences apt but not dpkg, which still prints an unpack line per
# package plus a "Reading database" progress bar. Dpkg::Use-Pty=0 stops the
# progress redraw; the log keeps the detail for when something actually fails.
# A fresh cloud VM is often still running unattended-upgrades: wait for the
# dpkg lock instead of failing on it. confdef/confold answer a changed-conffile
# prompt the way an operator almost always would -- keep their file -- since
# nobody can answer it from behind a progress bar.
APT_OPTS=(-y -qq -o Dpkg::Use-Pty=0 -o DPkg::Lock::Timeout=300
          -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold)
run apt-get "${APT_OPTS[@]}" update || die "apt-get update failed"
run apt-get "${APT_OPTS[@]}" install "${PKGS[@]}" || die "failed to install build prerequisites"
ok "prerequisites installed"

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
    warn "net.core.default_qdisc is $AFTER_DQ, not fq. See \"guard\" in: sudo ssctl status"
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
                    warn "$label root qdisc is still $now, not fq. See \"last_error\" under \"guard\" in: sudo ssctl status"
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
    '') GUARD_ROW="skyline-speederd (see \"guard\" in ssctl status)" ;;
    0)  GUARD_ROW="skyline-speederd, when it attaches only ([guard] interval_s = 0)" ;;
    *)  GUARD_ROW="skyline-speederd, re-checked every $GUARD_INTERVAL s (\"guard\" in ssctl status)" ;;
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
STATUS_WHAT="modules, live coefficients, counters"
[ "$HAS_GUARD" -eq 0 ] || STATUS_WHAT="$STATUS_WHAT, guard"

# A few of the common parameters docs/usage.md section 4 explains, each shown
# with the value the daemon runs right now (module_tuning in ssctl status).
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
   sudo ssctl flows                connections on skyline_cc right now
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
                copy the current values from sudo ssctl status first. Lost
                when skyline-speederd restarts.
                sudo ssctl reset-module-config    back to the file's values
   Persistent:  edit $CFG, check it with
                sudo skyline-speederd --config $CFG --validate-only
                then sudo systemctl restart skyline-speederd

 ${BLD}Note${RST}  dynamic RTO (the sockops policy; off by default) only sees processes in
       /sys/fs/cgroup/skyline-speeder. Opt a service in with:
         sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <command...>
EOF
