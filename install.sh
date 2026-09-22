#!/usr/bin/env bash
#
# Skyline Speeder -- one-click installer for Debian / Ubuntu.
#
# Installs the build toolchain, compiles the three CO-RE BPF objects against
# THIS machine's kernel BTF, builds the Rust control plane, installs the systemd
# units, and activates skyline_cc across reboots. Installs no proxy, no network
# service, and opens no port.
#
#   sudo ./install.sh              # build from source, install and activate
#   sudo ./install.sh --prebuilt   # install published artifacts, no toolchain
#   sudo ./install.sh --check      # preflight only, change nothing
#   sudo ./install.sh --no-enable  # install but leave skyline_cc detached
#   sudo ./install.sh --uninstall  # remove
#
# --prebuilt downloads the release artifacts instead of compiling. It needs no
# clang, no LLVM, no bpftool and no Rust: the BPF objects were built against a
# pinned reference header from the oldest supported kernel, and CO-RE fixes the
# field offsets against THIS kernel when they load. Everything else about the
# install is identical.
#
set -euo pipefail

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

CSI=$'\033'; RED="${CSI}[31m"; GRN="${CSI}[32m"; YLW="${CSI}[33m"; BLD="${CSI}[1m"; RST="${CSI}[0m"
info() { printf '%s==>%s %s\n' "$BLD" "$RST" "$*"; }
ok()   { printf '%s  ok%s  %s\n' "$GRN" "$RST" "$*"; }
warn() { printf '%s warn%s %s\n' "$YLW" "$RST" "$*" >&2; }
die()  { printf '%serror%s %s\n' "$RED" "$RST" "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check) MODE=check; shift ;;
        --no-enable) ENABLE=0; shift ;;
        --uninstall) MODE=uninstall; shift ;;
        --prebuilt) SOURCE=prebuilt; shift ;;
        --release) [ "$#" -ge 2 ] || die "--release needs a tag"; SOURCE=prebuilt; RELEASE_TAG="$2"; shift 2 ;;
        -h|--help) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
    STATE=/etc/skyline-speeder/pre-install-state
    if [ -r "$STATE" ]; then
        # shellcheck disable=SC1090  # a generated two-line key=value file
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

# --- 4. packages -----------------------------------------------------------
export DEBIAN_FRONTEND=noninteractive
if [ "$SOURCE" = prebuilt ]; then
    # The whole point of --prebuilt: no compiler, no LLVM, no bpftool, no Rust.
    # Only what it takes to fetch and unpack an archive.
    PKGS=(curl ca-certificates tar)
else
    PKGS=(build-essential pkg-config clang llvm libbpf-dev libelf-dev zlib1g-dev bpftool curl)
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
    info "preflight complete"
    exit 0
fi

if [ "$SOURCE" = prebuilt ]; then
    info "installing fetch prerequisites"
else
    info "installing build toolchain"
fi
# `-qq` silences apt but not dpkg, which still prints an unpack line per
# package plus a "Reading database" progress bar. Dpkg::Use-Pty=0 stops the
# progress redraw; the log keeps the detail for when something actually fails.
APT_LOG=$(mktemp)
apt_quiet() { apt-get -y -qq -o Dpkg::Use-Pty=0 "$@" >>"$APT_LOG" 2>&1; }
apt_quiet update || { cat "$APT_LOG" >&2; die "apt-get update failed"; }
apt_quiet install "${PKGS[@]}" || { cat "$APT_LOG" >&2; die "failed to install build prerequisites"; }
rm -f "$APT_LOG"
ok "prerequisites installed"

# --- 5. Rust ---------------------------------------------------------------
# Skipped entirely for a prebuilt install -- the binaries are already built.
if [ "$SOURCE" = build ]; then
# rust-toolchain.toml pins the channel; rustup honours it automatically inside
# the repo, so only the rustup installation itself is handled here.
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"
    else
        info "installing the Rust toolchain via rustup"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --component rustfmt >/dev/null
        PATH="$HOME/.cargo/bin:$PATH"
    fi
fi
export PATH
command -v cargo >/dev/null 2>&1 || die "cargo is still not on PATH after rustup install"
ok "rust: $(cargo --version)"
fi

# --- 6. obtain and install the artifacts -----------------------------------
install_prebuilt() {
    local api="https://api.github.com/repos/${REPO_SLUG}/releases"
    local arch; arch=$(uname -m)
    local staging; staging=$(mktemp -d)
    # shellcheck disable=SC2064  # expand $staging now, not at trap time
    trap "rm -rf '$staging'" RETURN

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
    curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/release.json" "$api" \
        || die "cannot reach the release API. Check network access, or use the
   source build (drop --prebuilt), or pass --release <tag>."

    # grep rather than a JSON parser: python3/jq are not guaranteed on a
    # minimal server image, and needing one to install would undercut the
    # point of a toolchain-free path.
    url=$(grep -o "https://[^\"]*skyline-speeder-[^\"]*-${arch}\.tar\.gz" \
          "$staging/release.json" | head -1)
    [ -n "$url" ] || die "no prebuilt artifact for $arch in that release.
   Published artifacts are per-architecture; build from source instead."
    fi

    if [ -n "$local_artifact" ]; then
        cp -- "$local_artifact" "$staging/artifact.tar.gz"
        [ -r "${local_artifact}.sha256" ] \
            && cp -- "${local_artifact}.sha256" "$staging/artifact.sha256"
    else
        info "downloading $(basename -- "$url")"
        curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/artifact.tar.gz" "$url" \
            || die "artifact download failed"
        curl -fsSL --proto '=https' --tlsv1.2 -o "$staging/artifact.sha256" \
            "${url}.sha256" 2>/dev/null || true
    fi

    # The digest is published beside the artifact. Verify it: this tarball is
    # about to be unpacked into /usr/local and /opt and run as root.
    if [ -r "$staging/artifact.sha256" ]; then
        local want got
        want=$(cut -d' ' -f1 < "$staging/artifact.sha256")
        got=$(sha256sum "$staging/artifact.tar.gz" | cut -d' ' -f1)
        [ "$want" = "$got" ] || die "sha256 mismatch
   expected: $want
   actual:   $got"
        ok "sha256 verified"
    else
        warn "no published .sha256 beside the artifact; trusting TLS alone"
    fi

    tar -xzf "$staging/artifact.tar.gz" -C "$staging" || die "artifact did not unpack"
    local root
    root=$(find "$staging" -mindepth 1 -maxdepth 1 -type d -name 'skyline-speeder-*' | head -1)
    [ -n "$root" ] && [ -x "$root/bin/skyline-speederd" ] \
        || die "unexpected artifact layout: no bin/skyline-speederd at the top level"

    if [ -r "$root/MANIFEST" ]; then
        info "artifact provenance"
        sed -n '1,6p' -- "$root/MANIFEST" | sed 's/^/     /'
    fi

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
    install_prebuilt
else
    info "building BPF objects and the Rust control plane (this takes a few minutes)"
    "$REPO_ROOT/infra/install-guest.sh" --confirm-install
fi

# --- 7. point the TC program at the real egress interface ------------------
# The shipped template targets the reference test bed's interface name, which
# almost never matches a real host. Fix it up on first install only -- an
# existing operator-edited config is never rewritten.
CFG=/etc/skyline-speeder/speeder.toml
DEV=$(ip -o route show default 2>/dev/null | awk '{print $5; exit}')
if [ -n "$DEV" ] && grep -q '^tc_interface = "data0"' "$CFG" 2>/dev/null; then
    sed -i "s|^tc_interface = \"data0\"|tc_interface = \"$DEV\"|" "$CFG"
    ok "runtime.tc_interface set to $DEV"
elif [ -n "$DEV" ]; then
    ok "runtime.tc_interface left as configured: $(grep '^tc_interface' "$CFG" 2>/dev/null || echo unknown)"
else
    warn "no default route found; set runtime.tc_interface in $CFG by hand"
fi

# --- 8. validate before starting anything ----------------------------------
# --validate-only --verify-bpf pushes all three objects through the verifier and
# exits without leaving runtime state, so a kernel/BTF mismatch surfaces here
# rather than as a half-started daemon.
info "validating configuration and BPF objects against this kernel"
# The failure is not necessarily the verifier's: a missing kernel capability
# stops the run before any object is loaded. Hand over the whole command --
# someone who came in through `curl | bash` never typed a `>/dev/null` to drop.
/usr/local/sbin/skyline-speederd --config "$CFG" --validate-only --verify-bpf >/dev/null \
    || die "validation failed (the reason is on the 'Error:' line above).
   For the capability report and the verifier log, run:
     /usr/local/sbin/skyline-speederd --config $CFG --validate-only --verify-bpf"
ok "all BPF objects passed the kernel verifier"

# Snapshot before anything starts changing sysctls: the daemon writes
# fallback_cc on load and `ssctl enable` writes skyline_cc after it attaches.
# Only written once -- a reinstall must not record skyline_cc as the "original".
STATE=/etc/skyline-speeder/pre-install-state
if [ ! -e "$STATE" ]; then
    install -d /etc/skyline-speeder
    cat > "$STATE" <<EOF
PRE_INSTALL_CC=$(sysctl -n net.ipv4.tcp_congestion_control)
PRE_INSTALL_QDISC=$(sysctl -n net.core.default_qdisc)
EOF
    ok "recorded pre-install state: cc=$(sysctl -n net.ipv4.tcp_congestion_control) qdisc=$(sysctl -n net.core.default_qdisc)"
fi

systemctl enable --now skyline-speederd.service
ok "skyline-speederd.service started"

if [ "$ENABLE" -eq 0 ]; then
    echo
    info "installed, but skyline_cc is NOT attached (--no-enable)."
    echo "  Attach it now with:   sudo systemctl enable --now skyline-speeder-enable.service"
    exit 0
fi

# --- 9. attach and persist -------------------------------------------------
info "attaching skyline_cc and enabling it at boot"
systemctl enable --now skyline-speeder-enable.service

ACTIVE=$(sysctl -n net.ipv4.tcp_congestion_control 2>/dev/null || echo unknown)
echo
if [ "$ACTIVE" = skyline_cc ]; then
    ok "active congestion control: $ACTIVE"
else
    die "expected skyline_cc to be active, got '$ACTIVE'. Check: journalctl -u skyline-speeder-enable.service"
fi
ok "default qdisc: $(sysctl -n net.core.default_qdisc)"
echo
info "Skyline Speeder is installed."
echo "  ssctl status    runtime state, capabilities and counters"
echo "  ssctl flows     number of active skyline_cc flows"
echo "  ssctl drain     graceful detach"
echo
echo "  Note: the sockops policy (dynamic RTO) only sees processes inside"
echo "  /sys/fs/cgroup/skyline-speeder. Wrap a service to opt it in:"
echo "      sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <command...>"
