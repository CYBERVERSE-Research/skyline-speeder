#!/bin/sh
#
# Skyline Speeder -- remote bootstrap.
#
#   curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
#
# On Alpine, which has neither curl nor bash (nor sudo) until somebody installs
# them, as root:
#
#   wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
#
# Fetches the source tree to a stable location and hands off to install.sh.
# By default install.sh then installs the latest published release: CO-RE
# objects built against a pinned reference header, whose field offsets are
# fixed against this kernel's BTF at load time, and a prebuilt control plane,
# with no build toolchain on this host -- on Alpine, the release's musl build.
# A host that cannot run the published binaries (not x86_64, a glibc older
# than 2.38) is told why and builds from source instead. Forwarding --source builds from source
# unconditionally; --release <tag> pins the artifact. Either way the
# install.sh that runs comes from --ref, which defaults to main, while the
# artifact comes from the latest release (or --release): --ref picks the
# installer, not the version installed.
#
# POSIX sh, not bash, on purpose: this is what installs bash on a host that
# has none, before install.sh (a bash script) can run. Keep it that way -- no
# arrays, no [[ ]], no $'...', no `local`; CI parses it with dash.
#
# Everything is wrapped in main() and invoked on the last line on purpose: if
# the download dies mid-transfer, the shell executes whatever bytes arrived.
# With the body in a function, a truncated download simply never reaches the
# call and nothing runs. Do not "simplify" this into top-level statements.
#
#   --ref <git-ref>     tag, branch or commit to install (default: main)
#   --repo <owner/name> source repository (default: CYBERVERSE-Research/skyline-speeder)
#   --src-dir <path>    where the tree lands (default: /usr/local/src/skyline-speeder)
#
# Any other argument is forwarded to install.sh verbatim (on Alpine the pipe
# ends in `| sh -s -- ...` instead):
#
#   ... | sudo bash -s -- --source      # build from source on this host instead
#   ... | sudo bash -s -- --release v0.4.0
#                                       # install that release, not the latest
#   ... | sudo bash -s -- --check       # preflight only
#   ... | sudo bash -s -- --no-enable   # install without attaching skyline_cc
#   ... | sudo bash -s -- --verbose     # show every build command's output
#   ... | sudo bash -s -- --uninstall   # remove, put the host on bbr + fq, and
#                                       #   remove the packages the install added
#   ... | sudo bash -s -- --uninstall --restore-pre-install
#                                       # ... but restore the cc/qdisc from
#                                       #   before the install instead
#
set -eu

# See install.sh for why: a caller's LC_ALL often names a locale this machine
# has not generated, and apt/perl bury the real output under locale warnings.
export LC_ALL=C.UTF-8 LANG=C.UTF-8 LANGUAGE=

main() {
    REPO="${SKYLINE_REPO:-CYBERVERSE-Research/skyline-speeder}"
    REF="${SKYLINE_REF:-main}"
    SRC_DIR="${SKYLINE_SRC_DIR:-/usr/local/src/skyline-speeder}"

    CSI=$(printf '\033') RED='' GRN='' YLW='' BLD='' RST=''
    # No escape codes when the output goes to a file or a CI log (install.sh
    # follows the same rule).
    if [ -t 1 ]; then
        RED="${CSI}[31m"; GRN="${CSI}[32m"; YLW="${CSI}[33m"; BLD="${CSI}[1m"; RST="${CSI}[0m"
    fi
    info() { printf '%s==>%s %s\n' "$BLD" "$RST" "$*"; }
    ok()   { printf '%s  ok%s  %s\n' "$GRN" "$RST" "$*"; }
    warn() { printf '%s warn%s %s\n' "$YLW" "$RST" "$*" >&2; }
    die()  { printf '%serror%s %s\n' "$RED" "$RST" "$*" >&2; exit 1; }

    # The arguments this script takes are consumed here; every other one is
    # moved to the end of "$@", in order, so that once the loop has seen them
    # all "$@" is exactly what install.sh gets. (sh has no arrays; "$@" is the
    # one list there is.)
    n=$#
    while [ "$n" -gt 0 ]; do
        case "$1" in
            --ref|--repo|--src-dir)
                [ "$n" -ge 2 ] || die "$1 needs a value"
                case "$1" in
                    --ref)     REF=$2 ;;
                    --repo)    REPO=$2 ;;
                    --src-dir) SRC_DIR=$2 ;;
                esac
                shift 2; n=$((n - 2)) ;;
            # The header comment, to its end, rather than a hand-counted line
            # range that truncates the moment a flag is documented above.
            -h|--help) awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
            *)         set -- "$@" "$1"; shift; n=$((n - 1)) ;;
        esac
    done

    [ "$(id -u)" -eq 0 ] || die "must run as root (pipe into 'sudo bash', not 'bash'; on Alpine, run it as root)"
    # The commands printed at the end carry sudo only where there is one.
    if command -v sudo >/dev/null 2>&1; then SUDO="sudo "; else SUDO=''; fi

    # Uninstalling from an existing tree needs no download. Re-fetching the
    # source just to delete the installation would be absurd, and would fail on
    # a host that has since lost network access.
    # Anywhere in the forwarded arguments, not just first: --uninstall now takes
    # a companion flag, and `--restore-pre-install --uninstall` means the same
    # thing to install.sh as the other order.
    case " $* " in *" --uninstall "*) UNINSTALLING=1 ;; *) UNINSTALLING=0 ;; esac
    if [ "$UNINSTALLING" -eq 1 ] && [ -x "$SRC_DIR/install.sh" ]; then
        info "using existing tree at $SRC_DIR"
        exec "$SRC_DIR/install.sh" "$@"
    fi

    TMP="$(mktemp -d)"
    # shellcheck disable=SC2064  # $TMP must expand now, not at trap time
    trap "rm -rf '$TMP'" EXIT

    # --- fetch prerequisites -----------------------------------------------
    # curl got this script here, but the pipe may have been wget (Alpine and a
    # cloud Debian ship no curl), and tar is not guaranteed on a minimal image.
    # bash is what install.sh runs in, and Alpine has none until asked. Install
    # only what is missing; the full build toolchain is install.sh's job, not
    # ours.
    NEED=''
    command -v curl >/dev/null 2>&1 || NEED="$NEED curl"
    command -v tar  >/dev/null 2>&1 || NEED="$NEED tar"
    command -v bash >/dev/null 2>&1 || NEED="$NEED bash"
    NEED=${NEED# }
    if [ -n "$NEED" ]; then
        info "installing fetch prerequisites: $NEED"
        # Same quiet style as install.sh: the package manager's chatter goes
        # to a log that is only shown when something actually fails.
        # $NEED unquoted on purpose: it is a list of package names.
        # shellcheck disable=SC2086
        if command -v apt-get >/dev/null 2>&1; then
            if ! { DEBIAN_FRONTEND=noninteractive apt-get update -qq \
                   && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Use-Pty=0 \
                        $NEED; } >"$TMP/pkg.log" 2>&1 </dev/null; then
                cat "$TMP/pkg.log" >&2
                die "failed to install $NEED
   An interrupted dpkg run blocks every install until it is finished:
   'sudo dpkg --configure -a', then run this again. install.sh repairs that
   itself at its package step; this bootstrap deliberately stays small."
            fi
        elif command -v dnf >/dev/null 2>&1; then
            if ! dnf -y -q install $NEED >"$TMP/pkg.log" 2>&1 </dev/null; then
                cat "$TMP/pkg.log" >&2
                die "failed to install $NEED with dnf"
            fi
        elif command -v apk >/dev/null 2>&1; then
            # --update-cache: a fresh image may never have fetched an index.
            if ! apk add --no-progress --update-cache $NEED >"$TMP/pkg.log" 2>&1 </dev/null; then
                cat "$TMP/pkg.log" >&2
                die "failed to install $NEED with apk"
            fi
        else
            die "missing $NEED and no apt-get, dnf or apk to install them
   (Debian/Ubuntu, Fedora and the RHEL family, and Alpine are supported)"
        fi
        ok "installed $NEED"
    fi

    # --- download -----------------------------------------------------------
    URL="https://codeload.github.com/${REPO}/tar.gz/${REF}"
    info "fetching ${REPO}@${REF}"
    curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/src.tar.gz" "$URL" \
        || die "download failed: $URL
   Check the repository and ref exist, and that this host can reach github.com."

    # Optional integrity pin. Publish the digest alongside a release tag and
    # operators can verify what they are about to build and run as root:
    #   SKYLINE_SHA256=<digest> curl ... | sudo -E bash
    if [ -n "${SKYLINE_SHA256:-}" ]; then
        GOT="$(sha256sum "$TMP/src.tar.gz" | awk '{print $1}')"
        [ "$GOT" = "$SKYLINE_SHA256" ] \
            || die "sha256 mismatch
   expected: $SKYLINE_SHA256
   actual:   $GOT"
        ok "sha256 verified"
    else
        warn "no SKYLINE_SHA256 given; the tarball is trusted on TLS alone"
    fi

    tar -xzf "$TMP/src.tar.gz" -C "$TMP" || die "extract failed"
    UNPACKED="$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -1)"
    if [ -z "$UNPACKED" ] || [ ! -x "$UNPACKED/install.sh" ]; then
        die "unexpected tarball layout: no executable install.sh at the top level"
    fi

    # --- place the tree -----------------------------------------------------
    # A previous tree is moved aside, never deleted: it may carry local edits,
    # and an installer that silently destroys an operator's working copy is not
    # a trade anyone agreed to. Say where it went.
    if [ -e "$SRC_DIR" ]; then
        BACKUP="${SRC_DIR}.bak-$(date +%Y%m%d%H%M%S)"
        mv "$SRC_DIR" "$BACKUP"
        warn "existing tree moved to $BACKUP"
    fi
    mkdir -p "$(dirname "$SRC_DIR")"
    mv "$UNPACKED" "$SRC_DIR"
    ok "source tree at $SRC_DIR"

    # --- hand off -----------------------------------------------------------
    # install.sh owns every check that matters (distribution, kernel floor, BTF,
    # cgroup v2, verifier pass). Duplicating them here would mean two copies to
    # keep in sync, so this script deliberately checks none of them.
    info "handing off to install.sh"
    echo
    "$SRC_DIR/install.sh" "$@"

    echo
    ok "installer kept at $SRC_DIR -- upgrade or remove from there:"
    echo "     ${SUDO}$SRC_DIR/install.sh               # upgrade to the latest release"
    echo "     ${SUDO}$SRC_DIR/install.sh --uninstall   # also puts this host on bbr + fq"
}

main "$@"
