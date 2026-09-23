#!/usr/bin/env bash
#
# Skyline Speeder -- remote bootstrap.
#
#   curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
#
# Fetches the source tree to a stable location and hands off to install.sh.
# By default install.sh then builds from source on this host. Forwarding
# --prebuilt (or --release <tag>) makes it install the published release
# artifacts instead: CO-RE objects built against a pinned reference header,
# whose field offsets are fixed against this kernel's BTF at load time. Either
# way the install.sh that runs comes from --ref, which defaults to main, not to
# the latest release.
#
# Everything is wrapped in main() and invoked on the last line on purpose: if
# `curl` dies mid-transfer, bash executes whatever bytes arrived. With the body
# in a function, a truncated download simply never reaches the call and nothing
# runs. Do not "simplify" this into top-level statements.
#
#   --ref <git-ref>     tag, branch or commit to install (default: main)
#   --repo <owner/name> source repository (default: CYBERVERSE-Research/skyline-speeder)
#   --src-dir <path>    where the tree lands (default: /usr/local/src/skyline-speeder)
#
# Any other argument is forwarded to install.sh verbatim:
#
#   ... | sudo bash -s -- --prebuilt    # install published artifacts, no toolchain
#   ... | sudo bash -s -- --check       # preflight only
#   ... | sudo bash -s -- --no-enable   # install without attaching skyline_cc
#   ... | sudo bash -s -- --verbose     # show every build command's output
#   ... | sudo bash -s -- --uninstall   # remove
#
set -euo pipefail

# See install.sh for why: a caller's LC_ALL often names a locale this machine
# has not generated, and apt/perl bury the real output under locale warnings.
export LC_ALL=C.UTF-8 LANG=C.UTF-8 LANGUAGE=

main() {
    local REPO="${SKYLINE_REPO:-CYBERVERSE-Research/skyline-speeder}"
    local REF="${SKYLINE_REF:-main}"
    local SRC_DIR="${SKYLINE_SRC_DIR:-/usr/local/src/skyline-speeder}"
    local -a FORWARD=()

    local CSI=$'\033' RED= GRN= YLW= BLD= RST=
    # No escape codes when the output goes to a file or a CI log (install.sh
    # follows the same rule).
    if [ -t 1 ]; then
        RED="${CSI}[31m"; GRN="${CSI}[32m"; YLW="${CSI}[33m"; BLD="${CSI}[1m"; RST="${CSI}[0m"
    fi
    info() { printf '%s==>%s %s\n' "$BLD" "$RST" "$*"; }
    ok()   { printf '%s  ok%s  %s\n' "$GRN" "$RST" "$*"; }
    warn() { printf '%s warn%s %s\n' "$YLW" "$RST" "$*" >&2; }
    die()  { printf '%serror%s %s\n' "$RED" "$RST" "$*" >&2; exit 1; }

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --ref)     [ "$#" -ge 2 ] || die "--ref needs a value";     REF="$2";     shift 2 ;;
            --repo)    [ "$#" -ge 2 ] || die "--repo needs a value";    REPO="$2";    shift 2 ;;
            --src-dir) [ "$#" -ge 2 ] || die "--src-dir needs a value"; SRC_DIR="$2"; shift 2 ;;
            -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
            *)         FORWARD+=("$1"); shift ;;
        esac
    done

    [ "$(id -u)" -eq 0 ] || die "must run as root (pipe into 'sudo bash', not 'bash')"

    # Uninstalling from an existing tree needs no download. Re-fetching the
    # source just to delete the installation would be absurd, and would fail on
    # a host that has since lost network access.
    if [ "${FORWARD[0]:-}" = --uninstall ] && [ -x "$SRC_DIR/install.sh" ]; then
        info "using existing tree at $SRC_DIR"
        exec "$SRC_DIR/install.sh" "${FORWARD[@]}"
    fi

    local TMP
    TMP="$(mktemp -d)"
    # shellcheck disable=SC2064  # $TMP must expand now, not at trap time
    trap "rm -rf '$TMP'" EXIT

    # --- fetch prerequisites -----------------------------------------------
    # curl got this script here, but the pipe may have been wget, and tar is
    # not guaranteed on a minimal image. Install only what is missing; the full
    # build toolchain is install.sh's job, not ours.
    local -a NEED=()
    command -v curl >/dev/null 2>&1 || NEED+=(curl)
    command -v tar  >/dev/null 2>&1 || NEED+=(tar)
    if [ "${#NEED[@]}" -gt 0 ]; then
        info "installing fetch prerequisites: ${NEED[*]}"
        command -v apt-get >/dev/null 2>&1 \
            || die "missing ${NEED[*]} and no apt-get to install them (Debian/Ubuntu only)"
        # Same quiet style as install.sh: apt's and dpkg's chatter goes to a
        # log that is only shown when something actually fails.
        if ! { DEBIAN_FRONTEND=noninteractive apt-get update -qq \
               && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Use-Pty=0 \
                    "${NEED[@]}"; } >"$TMP/apt.log" 2>&1 </dev/null; then
            cat "$TMP/apt.log" >&2
            die "failed to install ${NEED[*]}
   An interrupted dpkg run blocks every install until it is finished:
   'sudo dpkg --configure -a', then run this again. install.sh repairs that
   itself at its package step; this bootstrap deliberately stays small."
        fi
        ok "installed ${NEED[*]}"
    fi

    # --- download -----------------------------------------------------------
    local URL="https://codeload.github.com/${REPO}/tar.gz/${REF}"
    info "fetching ${REPO}@${REF}"
    curl -fsSL --proto '=https' --tlsv1.2 -o "$TMP/src.tar.gz" "$URL" \
        || die "download failed: $URL
   Check the repository and ref exist, and that this host can reach github.com."

    # Optional integrity pin. Publish the digest alongside a release tag and
    # operators can verify what they are about to build and run as root:
    #   SKYLINE_SHA256=<digest> curl ... | sudo -E bash
    if [ -n "${SKYLINE_SHA256:-}" ]; then
        local GOT
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
    local UNPACKED
    UNPACKED="$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -1)"
    if [ -z "$UNPACKED" ] || [ ! -x "$UNPACKED/install.sh" ]; then
        die "unexpected tarball layout: no executable install.sh at the top level"
    fi

    # --- place the tree -----------------------------------------------------
    # A previous tree is moved aside, never deleted: it may carry local edits,
    # and an installer that silently destroys an operator's working copy is not
    # a trade anyone agreed to. Say where it went.
    if [ -e "$SRC_DIR" ]; then
        local BACKUP
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
    "$SRC_DIR/install.sh" "${FORWARD[@]+"${FORWARD[@]}"}"

    echo
    ok "source kept at $SRC_DIR -- rebuild or remove from there:"
    echo "     sudo $SRC_DIR/install.sh --uninstall"
}

main "$@"
