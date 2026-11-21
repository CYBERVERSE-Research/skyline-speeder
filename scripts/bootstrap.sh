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
# than 2.34, a musl older than 1.2.5) is told why and builds this tree from
# source instead. Forwarding --source builds from source unconditionally; --release <tag> pins the artifact. Either way the
# install.sh that runs comes from --ref, which defaults to the latest release
# -- its signed source tarball, checked against the release key below before
# anything in it runs -- while the artifact comes from the latest release (or
# --release): --ref picks the installer, not the version installed. --ref main
# takes the development tree instead, which is not signed: every commit on
# main would otherwise run as root on every new install the moment it lands.
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
#   --ref <git-ref>     tag, branch or commit to install (default: the latest
#                       release; main for the development tree)
#   --repo <owner/name> repository to fetch the tree and take releases from
#                       (default: CYBERVERSE-Research/skyline-speeder)
#   --src-dir <path>    where the tree lands (default: /usr/local/src/skyline-speeder)
#
# Any other argument is forwarded to install.sh verbatim (on Alpine the pipe
# ends in `| sh -s -- ...` instead):
#
#   ... | sudo bash -s -- --source      # build from source on this host instead
#   ... | sudo bash -s -- --release v0.4.0
#                                       # install that release, not the latest
#   ... | sudo bash -s -- --check       # preflight only: installs only what the
#                                       #   download needs, places no tree
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

# See install.sh: nothing written as root here -- the tree install.sh is then
# run from, as root -- is left writable by group or other, whatever the
# caller's umask. A stricter one is kept.
umask "$(printf '%04o' $(( 0$(umask) | 022 )))"

# --- error reports -------------------------------------------------------
# A failure of this script's own -- installing what the download needs, the
# download, its digest, unpacking, placing the tree -- asks, once, whether to
# send an error report, as install.sh does for its steps and ssctl for its
# commands (crates/ssctl/src/feedback.rs): the same question, the same
# contents, the same scrub; keep the three in step. Nothing is sent but on a
# y typed on the terminal, or with SKYLINE_FEEDBACK=yes; with no terminal
# nothing is asked or sent; SKYLINE_FEEDBACK=no never asks. A wrong argument,
# not being root, a kernel below the floor (a host the requirements rule out,
# as install.sh's `unsupported` does), --check, an interrupt and everything
# once install.sh has taken over ask nothing here: install.sh asks about its
# own.
#
# The DSN is sealed as in install.sh (ChaCha20 under the public key and nonce
# below); infra/seal-feedback-dsn.py writes this copy too, and
# `cargo test -p ssctl` checks it agrees with install.sh's and runs these
# functions under sh. POSIX sh, like the rest of this file: no `local`, so
# every variable here starts with fb_.
FEEDBACK_SEAL_KEY=9a505fc64734e2f2c801a1c0e3d031c5ff2f5662cd1ba51624c75dac9555de1a
FEEDBACK_SEAL_NONCE=a482acf480d0928b3c37c730
FEEDBACK_SEALED=0b3e66e80afc3e71da442d2fc2e1d3fb4d2ab7dc54af25af4d7e43ea395f3983406754d3aed5c0f1c1c1103080f52fa26529a7c855bf36888fd7415fa62f81e8e11217

# fb_qr <a> <b> <c> <d>: one ChaCha20 quarter round on fb_x0 .. fb_x15. Shell
# arithmetic is at least 64 bits wide on every host this installs on, so a
# 32-bit word shifted left by up to 16 still fits before the mask.
fb_qr() {
    eval "fb_a=\$fb_x$1 fb_b=\$fb_x$2 fb_c=\$fb_x$3 fb_d=\$fb_x$4"
    fb_a=$(( (fb_a + fb_b) & 0xffffffff )); fb_t=$(( fb_d ^ fb_a ))
    fb_d=$(( ((fb_t << 16) | (fb_t >> 16)) & 0xffffffff ))
    fb_c=$(( (fb_c + fb_d) & 0xffffffff )); fb_t=$(( fb_b ^ fb_c ))
    fb_b=$(( ((fb_t << 12) | (fb_t >> 20)) & 0xffffffff ))
    fb_a=$(( (fb_a + fb_b) & 0xffffffff )); fb_t=$(( fb_d ^ fb_a ))
    fb_d=$(( ((fb_t << 8) | (fb_t >> 24)) & 0xffffffff ))
    fb_c=$(( (fb_c + fb_d) & 0xffffffff )); fb_t=$(( fb_b ^ fb_c ))
    fb_b=$(( ((fb_t << 7) | (fb_t >> 25)) & 0xffffffff ))
    eval "fb_x$1=\$fb_a fb_x$2=\$fb_b fb_x$3=\$fb_c fb_x$4=\$fb_d"
}

# fb_chacha <key hex> <nonce hex> <data hex>: ChaCha20 (RFC 8439, the block
# counter from 1) over the data, as hex on stdout; it seals and opens alike.
fb_chacha() {
    # The fb_i words are read through eval, which shellcheck cannot follow.
    # shellcheck disable=SC2034
    fb_i0=1634760805 fb_i1=857760878 fb_i2=2036477234 fb_i3=1797285236  # "expand 32-byte k"
    fb_k=4
    # Key words 4-11, then (12 is the counter) nonce words 13-15; each word
    # is four bytes, little-endian.
    for fb_w in $(printf '%s' "$1$2" | sed 's/\(..\)\(..\)\(..\)\(..\)/\4\3\2\1 /g'); do
        [ "$fb_k" -ne 12 ] || fb_k=13
        eval "fb_i$fb_k=\$(( 0x$fb_w ))"
        fb_k=$((fb_k + 1))
    done
    # Kept apart: the keystream takes the positional parameters below.
    fb_data=$3
    fb_stream= fb_block=1 fb_need=$(( ${#fb_data} / 2 ))
    while [ $(( (fb_block - 1) * 64 )) -lt "$fb_need" ]; do
        # shellcheck disable=SC2034  # read through eval
        fb_i12=$fb_block
        for fb_k in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do eval "fb_x$fb_k=\$fb_i$fb_k"; done
        # shellcheck disable=SC2034  # ten double rounds; the counter is not read
        for fb_round in 1 2 3 4 5 6 7 8 9 10; do
            fb_qr 0 4 8 12; fb_qr 1 5 9 13; fb_qr 2 6 10 14; fb_qr 3 7 11 15
            fb_qr 0 5 10 15; fb_qr 1 6 11 12; fb_qr 2 7 8 13; fb_qr 3 4 9 14
        done
        for fb_k in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
            eval "fb_t=\$(( (fb_x$fb_k + fb_i$fb_k) & 0xffffffff ))"
            fb_stream="$fb_stream$(printf '%02x %02x %02x %02x ' $((fb_t & 255)) $((fb_t >> 8 & 255)) \
                $((fb_t >> 16 & 255)) $((fb_t >> 24 & 255)))"
        done
        fb_block=$((fb_block + 1))
    done
    # The keystream bytes as the positional parameters, used up from the
    # front, one per data byte.
    # shellcheck disable=SC2086  # one word per byte
    set -- $fb_stream
    fb_out=
    for fb_byte in $(printf '%s' "$fb_data" | sed 's/../& /g'); do
        fb_out="$fb_out$(printf '%02x' $(( 0x$fb_byte ^ 0x$1 )))"
        shift
    done
    printf '%s' "$fb_out"
}

# fb_dsn: FEEDBACK_SEALED opened into FEEDBACK_PUBLIC (the DSN's public key)
# and FEEDBACK_URL (its envelope endpoint). Fails when none is sealed or it
# does not open to an https DSN.
fb_dsn() {
    [ -n "$FEEDBACK_SEALED" ] || return 1
    fb_octal=
    for fb_byte in $(fb_chacha "$FEEDBACK_SEAL_KEY" "$FEEDBACK_SEAL_NONCE" "$FEEDBACK_SEALED" \
                     | sed 's/../& /g'); do
        fb_octal="$fb_octal\\$(printf '%03o' $(( 0x$fb_byte )))"
    done
    # Octal escapes, as POSIX printf has no \x; every byte is one, so the
    # format holds no % of the DSN's own.
    # shellcheck disable=SC2059
    fb_parsed=$(printf "$fb_octal\\n" | sed -nE \
        's#^https://([0-9a-f]{32})@([A-Za-z0-9.-]+(:[0-9]+)?)((/[A-Za-z0-9._-]+)*)/([0-9]+)$#\1 https://\2\4/api/\6/envelope/#p')
    [ -n "$fb_parsed" ] || return 1
    FEEDBACK_PUBLIC=${fb_parsed%% *}
    FEEDBACK_URL=${fb_parsed#* }
}

# fb_replace <text> <name> <with>: every <name> in <text> replaced, literally
# (no pattern), as bash's ${text//name/with} does in install.sh.
fb_replace() {
    printf '%s\n' "$1" | awk -v from="$2" -v to="$3" '{
        out = ""; rest = $0
        while ((at = index(rest, from)) > 0) {
            out = out substr(rest, 1, at - 1) to; rest = substr(rest, at + length(from))
        }
        print out rest }'
}

# fb_scrub: stdin to stdout without what names this host, its network or its
# people -- install.sh's feedback_scrub, expression for expression.
fb_scrub() {
    fb_text=$(sed -E \
        -e 's#://[^/@[:space:]]+@#://<credentials>@#g' \
        -e 's#[[:alnum:]._%+-]+@[[:alnum:]-]+(\.[[:alnum:]-]+)*\.[[:alpha:]]{2,}#<email>#g' \
        -e 's#(^|[^[:alnum:]_])(gh[pousr]|github_pat|sntry[su]|sntrs)_[[:alnum:]_+/=-]{16,}#\1<token>#g' \
        -e 's#(([Tt]oken|TOKEN|[Ss]ecret|SECRET|[Pp]ass(wd|word)|PASS(WD|WORD))[=:])[^[:space:]]+#\1<redacted>#g' \
        -e 's#[0-9]{1,3}(\.[0-9]{1,3}){3}#<ipv4>#g' \
        -e 's#([0-9A-Fa-f]{1,4}:){3,7}[0-9A-Fa-f]{1,4}#<ipv6-or-mac>#g' \
        -e 's#([0-9A-Fa-f]{2}-){5}[0-9A-Fa-f]{2}#<mac>#g' \
        -e ':v6' \
        -e 's#(^|[^[:alnum:]_:])[0-9A-Fa-f]{0,4}(:[0-9A-Fa-f]{1,4})*::[0-9A-Fa-f:]*($|[^[:alnum:]_:])#\1<ipv6>\3#' \
        -e 'tv6' \
        -e 's#/home/[^/[:space:]]+#/home/<user>#g' 2>/dev/null) || fb_text=
    # Not `hostname -f`: that asks the resolver, which hangs on a host whose
    # DNS is broken -- one reason a download fails.
    fb_host=$(cat /proc/sys/kernel/hostname 2>/dev/null || true)
    for fb_name in "$fb_host" "${fb_host%%.*}"; do
        [ "${#fb_name}" -lt 3 ] || fb_text=$(fb_replace "$fb_text" "$fb_name" "<host>")
    done
    for fb_name in "${SUDO_USER:-}" "${LOGNAME:-}"; do
        [ "${#fb_name}" -lt 3 ] || [ "$fb_name" = root ] || fb_text=$(fb_replace "$fb_text" "$fb_name" "<user>")
    done
    printf '%s' "$fb_text"
}

# fb_json <text>: the text as a JSON string, quotes included.
fb_json() {
    fb_tab=$(printf '\t')
    printf '"%s"' "$(printf '%s' "$1" | LC_ALL=C tr -d '\001-\010\013-\037\177' \
        | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e "s/$fb_tab/\\\\t/g" \
        | awk 'NR > 1 { printf "\\n" } { printf "%s", $0 }')"
}

# fb_curl_quoted: stdin as one quoted value of a curl configuration, which
# takes \\, \" and \n -- every line ends in \n, the last one included.
fb_curl_quoted() {
    sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{ printf "%s\\n", $0 }'
}

# fb_build: the report on this run's failure, as a Sentry event into
# FEEDBACK_EVENT and as the lines `v` shows into FEEDBACK_VIEW. Built from the
# same values, so what is shown is what would be sent.
fb_build() {
    fb_message=$(printf '%s' "$FAIL_MESSAGE" | fb_scrub)
    fb_log=
    if [ -n "${TMP:-}" ] && [ -r "$TMP/pkg.log" ]; then
        fb_log=$(tail -n 40 "$TMP/pkg.log" | fb_scrub)
    fi
    fb_os_id=$( . /etc/os-release 2>/dev/null; printf '%s' "${ID:-unknown}")
    fb_os_version=$( . /etc/os-release 2>/dev/null; printf '%s' "${VERSION_ID:-}")
    fb_os_name=$( . /etc/os-release 2>/dev/null; printf '%s' "${PRETTY_NAME:-${ID:-unknown}}")
    fb_kernel=$(uname -r)
    fb_arch=$(uname -m)
    if fb_glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null); then
        fb_libc="glibc ${fb_glibc#* }"
    elif ls /lib/ld-musl-*.so.1 >/dev/null 2>&1; then
        fb_libc=musl
    else
        fb_libc=unknown
    fi
    fb_pkg=none
    for fb_name in apt-get dnf apk; do
        if command -v "$fb_name" >/dev/null 2>&1; then fb_pkg=${fb_name%-get}; break; fi
    done
    fb_kind=install
    [ ! -x /usr/local/sbin/skyline-speederd ] || fb_kind=upgrade
    # name=value pairs, the tags Sentry filters by; none of them names the
    # host -- the ref, which the operator chose, is scrubbed like the rest.
    # One word each: the values have no spaces (libc is added apart).
    fb_ref=$(printf '%s' "$REF" | fb_scrub)
    fb_tags="component=bootstrap.sh mode=$fb_kind os=$fb_os_id os_version=$fb_os_version"
    fb_tags="$fb_tags kernel=$fb_kernel arch=$fb_arch package_manager=$fb_pkg ref=$fb_ref"
    fb_json_tags=
    for fb_tag in $fb_tags; do
        fb_json_tags="$fb_json_tags,\"${fb_tag%%=*}\":$(fb_json "${fb_tag#*=}")"
    done
    fb_json_tags="$fb_json_tags,\"libc\":$(fb_json "$fb_libc")"
    FEEDBACK_EVENT_ID=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    FEEDBACK_EVENT="{\"event_id\":\"$FEEDBACK_EVENT_ID\",\"timestamp\":$(date -u +%s)"
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"platform\":\"other\",\"level\":\"error\",\"logger\":\"bootstrap.sh\""
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"environment\":\"production\",\"tags\":{${fb_json_tags#,}}"
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"exception\":{\"values\":[{\"type\":\"$fb_kind failed\",\"value\":$(fb_json "$fb_message")}]}"
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"contexts\":{\"os\":{\"name\":$(fb_json "$fb_os_name"),\"kernel_version\":$(fb_json "$fb_kernel")}}"
    # Grouped by where it failed and what it said, numbers aside.
    fb_first=$(printf '%s\n' "$fb_message" | head -n 1 | sed -E 's/[0-9]+/0/g')
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"fingerprint\":[\"bootstrap.sh\",\"$fb_kind\",$(fb_json "$STAGE"),$(fb_json "$fb_first")]"
    FEEDBACK_EVENT="$FEEDBACK_EVENT,\"extra\":{\"step\":$(fb_json "bootstrap.sh: $STAGE"),\"log_tail\":$(fb_json "$fb_log")}}"

    FEEDBACK_VIEW=$(
        printf '   error    %s\n' "$fb_kind failed: $fb_message"
        printf '   step     %s\n' "bootstrap.sh: $STAGE"
        printf '   host     %s\n' "$fb_os_name, kernel $fb_kernel, $fb_arch, $fb_libc, $fb_pkg"
        printf '   tags     %s\n' "$fb_tags libc=$fb_libc"
        if [ -n "$fb_log" ]; then
            printf '   log      the last lines of the package manager:\n'
            printf '%s\n' "$fb_log" | sed 's/^/     | /'
        fi
    )
}

# fb_send: FEEDBACK_EVENT to the DSN's project, through curl with its
# configuration on stdin (-K -), so no command line `ps` shows names the
# project; -q first, so no ~/.curlrc takes part.
fb_send() {
    command -v curl >/dev/null 2>&1 || { FEEDBACK_ERROR="curl is not installed"; return 1; }
    fb_envelope=$(printf '%s\n%s\n%s\n' \
        "{\"event_id\":\"$FEEDBACK_EVENT_ID\",\"sent_at\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}" \
        '{"type":"event"}' "$FEEDBACK_EVENT" | fb_curl_quoted)
    FEEDBACK_ERROR=$(printf '%s\n' \
        "url = \"$FEEDBACK_URL\"" \
        "header = \"X-Sentry-Auth: Sentry sentry_version=7, sentry_client=skyline-speeder-bootstrap/1, sentry_key=$FEEDBACK_PUBLIC\"" \
        "header = \"Content-Type: application/x-sentry-envelope\"" \
        "data-binary = \"$fb_envelope\"" \
        | curl -q -fsS --proto '=https' --tlsv1.2 --max-time 20 -o /dev/null -K - 2>&1) \
        || { FEEDBACK_ERROR=${FEEDBACK_ERROR:-curl failed}; return 1; }
}

# fb_read_answer: one line from the terminal (fd 5) into fb_answer, giving up
# after five minutes as install.sh's `read -t 300` does. POSIX read has no
# timeout, so timeout(1) runs it: with --foreground where that is coreutils'
# -- without it the read would be a background job, stopped by SIGTTIN --
# and as it is where it is busybox's, which keeps it in the foreground. With
# no timeout(1) at all, the question waits as long as it takes.
fb_read_answer() {
    fb_answer=
    if timeout --foreground 1 true >/dev/null 2>&1; then
        fb_answer=$(timeout --foreground 300 sh -c 'IFS= read -r a && printf "%s" "$a"' <&5) \
            || printf '\n' >&5
    elif command -v timeout >/dev/null 2>&1; then
        fb_answer=$(timeout 300 sh -c 'IFS= read -r a && printf "%s" "$a"' <&5) || printf '\n' >&5
    else
        IFS= read -r fb_answer <&5 || printf '\n' >&5
    fi
}

# fb_offer: the question, after the error has been printed.
fb_offer() {
    case "${SKYLINE_FEEDBACK:-ask}" in
        1|y|yes|true) fb_mode=yes ;;
        0|n|no|false|never) return 0 ;;
        *) fb_mode=ask ;;
    esac
    fb_dsn || return 0
    if ! command -v curl >/dev/null 2>&1; then
        printf '   (no error report can be sent from here: curl is not installed)\n' >&2
        return 0
    fi
    fb_build
    if [ "$fb_mode" = ask ]; then
        # Tried in a subshell first: a failed redirection on exec ends a POSIX
        # shell outright.
        ( exec 5<>/dev/tty ) 2>/dev/null || return 0
        exec 5<>/dev/tty
        printf '\n %sHelp us fix this?%s Send an error report to the Skyline Speeder developers.\n' "$BLD" "$RST" >&5
        printf '%s\n' \
            "   It holds the error above, the step, the versions, the distribution, the" \
            "   kernel and the package manager's last lines, with IP, MAC and e-mail" \
            "   addresses, the host name and user names removed." \
            "   Nothing is sent unless you answer y." >&5
        while :; do
            printf '   Send it? [y/N, v to view it first] ' >&5
            fb_read_answer
            case "$fb_answer" in
                v|V) printf '%s\n' "$FEEDBACK_VIEW" >&5 ;;
                y|Y|yes|YES|Yes) break ;;
                *) printf '   not sent\n' >&5; exec 5>&-; return 0 ;;
            esac
        done
    else
        exec 5>&2
    fi
    if fb_send; then
        printf '   %s  sent (report %s), thank you\n' "${GRN}ok${RST}" \
            "$(printf '%s' "$FEEDBACK_EVENT_ID" | cut -c1-8)" >&5
    else
        printf '   %s the report could not be sent: %s\n' "${YLW}warn${RST}" "$FEEDBACK_ERROR" >&5
    fi
    exec 5>&-
}

# fb_on_exit: the EXIT trap. A failure while the report is armed (FB_ARMED,
# set once the run is past what asks nothing) is offered one; the download
# directory goes either way, after the question, so the package log it reads
# is still there.
fb_on_exit() {
    fb_rc=$?
    set +e
    if [ "$fb_rc" -ne 0 ] && [ "${FB_ARMED:-0}" -eq 1 ] && [ "${FB_INTERRUPTED:-0}" -eq 0 ]; then
        [ -n "${FAIL_MESSAGE:-}" ] \
            || FAIL_MESSAGE="bootstrap stopped unexpectedly (exit status $fb_rc) while $STAGE"
        fb_offer
    fi
    [ -z "${TMP:-}" ] || rm -rf "$TMP"
    exit "$fb_rc"
}
# --- end of error reports --------------------------------------------------

# The release key, as install.sh carries it (CI's shell job checks the two
# copies are the same lines): a release from v0.6.0 on publishes
# skyline-speeder-<tag>-source.tar.gz and its `ssh-keygen -Y sign` signature,
# and a tree fetched for a release tag is unpacked only once that verifies.
# Only the releases from before signing, by name, may come without one.
RELEASE_SIGNING_KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMHUlrw08jlUCQxQWBhNglTOtpuAKOrBsBCnyUkWG0ex'
RELEASE_SIGNING_NAMESPACE=skyline-speeder-release
UNSIGNED_RELEASES="v0.1.0 v0.2.0 v0.3.0 v0.4.0 v0.4.1 v0.4.2 v0.4.3 v0.4.4 v0.5.0 v0.5.1 v0.5.2 v0.5.3 v0.5.4 v0.5.5"

main() {
    # What the error report says this script was doing, the message it
    # stopped with, and whether a failure now is worth a report at all.
    STAGE="checking the arguments" FAIL_MESSAGE='' FB_ARMED=0 FB_INTERRUPTED=0 TMP=''
    trap fb_on_exit EXIT
    trap 'FB_INTERRUPTED=1; exit 130' INT TERM
    REPO="${SKYLINE_REPO:-CYBERVERSE-Research/skyline-speeder}"
    # Empty: the latest release, looked up once curl is there.
    REF="${SKYLINE_REF:-}"
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
    die()  { printf '%serror%s %s\n' "$RED" "$RST" "$*" >&2; FAIL_MESSAGE="$*"; exit 1; }
    # A host the documented requirements rule out: said, and no report asked
    # for (install.sh's `unsupported`).
    refuse() { FB_ARMED=0; die "$@"; }

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
            # Piped into a shell -- the way this script is meant to run --
            # $0 is that shell, not a file to read.
            -h|--help)
                case "$0" in
                    *bootstrap.sh) awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0" ;;
                    *) echo "Usage: the comment at the top of https://github.com/$REPO/blob/main/scripts/bootstrap.sh" ;;
                esac
                exit 0 ;;
            *)         set -- "$@" "$1"; shift; n=$((n - 1)) ;;
        esac
    done
    # install.sh looks releases up in SKYLINE_REPO; it has no --repo. Without
    # this, --repo fetched that repository's installer, which then installed
    # the default repository's release.
    export SKYLINE_REPO="$REPO"
    # Both go into the download URL, where curl resolves a ../ -- into another
    # repository's tree. A ref may be a branch with a / in it.
    case "$REPO" in
        */*/*|*..*|/*|*/|*[!A-Za-z0-9_./-]*) die "--repo must be <owner>/<name>: $REPO" ;;
        */*) ;;
        *) die "--repo must be <owner>/<name>: $REPO" ;;
    esac
    if [ -n "$REF" ]; then
        case "$REF" in
            -*|/*|*..*|*[!A-Za-z0-9_./-]*) die "not a git ref: $REF" ;;
        esac
    fi

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

    # The kernel floor, before anything is installed or fetched for a host
    # that cannot use it. The one check of install.sh's that is repeated here
    # (see "hand off" below): it is the refusal most hosts that are refused
    # get, it needs nothing but uname, and it is the same for every tree this
    # script could fetch -- skyline_cc's cong_control has the four arguments
    # the kernel gives it from 6.10, and 6.12 LTS is the supported floor.
    # install.sh asks again, for the runs that do not come through here.
    if [ "$UNINSTALLING" -eq 0 ]; then
        KVER="$(uname -r)"
        KMAJ="${KVER%%.*}"; KREST="${KVER#*.}"; KMIN="${KREST%%.*}"
        case "$KMAJ$KMIN" in
            ''|*[!0-9]*) ;;     # not a version this can read: install.sh decides
            *)
                if [ "$KMAJ" -lt 6 ] || { [ "$KMAJ" -eq 6 ] && [ "$KMIN" -lt 12 ]; }; then
                    refuse "kernel $KVER is not supported.
   Skyline Speeder requires 6.12 LTS or newer (hard ABI floor is 6.10).
   The 6.1.x and 6.6.x LTS branches use the older 2-argument cong_control
   signature and the verifier will reject skyline_cc on them.
   Nothing was downloaded or installed."
                fi ;;
        esac
    fi

    # From here on a failure is this script's own, and worth a report --
    # but not in a preflight, which exists to say what a host lacks.
    case " $* " in *" --check "*) CHECKING=1 ;; *) CHECKING=0 ;; esac
    [ "$CHECKING" -eq 1 ] || FB_ARMED=1

    # Removed by fb_on_exit, after any question about a failure.
    STAGE="creating a work directory"
    TMP="$(mktemp -d)"

    # --- fetch prerequisites -----------------------------------------------
    # curl got this script here, but the pipe may have been wget (Alpine and a
    # cloud Debian ship no curl), and tar is not guaranteed on a minimal image.
    # bash is what install.sh runs in, and Alpine has none until asked. Install
    # only what is missing; the full build toolchain is install.sh's job, not
    # ours.
    STAGE="installing what the download needs"
    NEED=''
    command -v curl >/dev/null 2>&1 || NEED="$NEED curl"
    command -v tar  >/dev/null 2>&1 || NEED="$NEED tar"
    command -v bash >/dev/null 2>&1 || NEED="$NEED bash"
    # ssh-keygen checks a release's signature, when a release's tree is what
    # is fetched; its package is named differently by each package manager.
    case "$REF" in
        ''|v[0-9]*.[0-9]*) command -v ssh-keygen >/dev/null 2>&1 || NEED="$NEED ssh-keygen" ;;
    esac
    NEED=${NEED# }
    if [ -n "$NEED" ]; then
        info "installing fetch prerequisites: $NEED"
        # Same quiet style as install.sh: the package manager's chatter goes
        # to a log that is only shown when something actually fails.
        # $NEED unquoted on purpose: it is a list of package names.
        # shellcheck disable=SC2086
        if command -v apt-get >/dev/null 2>&1; then
            NEED=$(printf '%s' "$NEED" | sed 's/ssh-keygen/openssh-client/')
            if ! { DEBIAN_FRONTEND=noninteractive apt-get update -qq \
                   && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Use-Pty=0 \
                        $NEED; } >"$TMP/pkg.log" 2>&1 </dev/null; then
                cat "$TMP/pkg.log" >&2
                die "failed to install $NEED
   An interrupted dpkg run blocks every install until it is finished:
   '${SUDO}dpkg --configure -a', then run this again. install.sh repairs that
   itself at its package step; this bootstrap deliberately stays small."
            fi
        elif command -v dnf >/dev/null 2>&1; then
            NEED=$(printf '%s' "$NEED" | sed 's/ssh-keygen/openssh-clients/')
            if ! dnf -y -q install $NEED >"$TMP/pkg.log" 2>&1 </dev/null; then
                cat "$TMP/pkg.log" >&2
                die "failed to install $NEED with dnf"
            fi
        elif command -v apk >/dev/null 2>&1; then
            NEED=$(printf '%s' "$NEED" | sed 's/ssh-keygen/openssh-keygen/')
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

    # --- the latest release -------------------------------------------------
    # From the redirect github.com answers /releases/latest with, not from the
    # API: no rate limit, and nothing to parse but a URL.
    if [ -z "$REF" ]; then
        STAGE="finding the latest release"
        LATEST=$(curl -q -fsSI --proto '=https' --tlsv1.2 --max-time 30 -o /dev/null \
                 -w '%{redirect_url}' "https://github.com/${REPO}/releases/latest") || LATEST=''
        case "$LATEST" in
            */releases/tag/*) REF=${LATEST##*/releases/tag/} ;;
        esac
        case "$REF" in
            ''|-*|/*|*..*|*[!A-Za-z0-9_./-]*)
                die "could not find the latest release of $REPO (${LATEST:-no answer from github.com}).
   Pass --ref <tag> for a release, or --ref main for the development tree." ;;
        esac
        info "latest release: $REF"
    fi

    # --- download -----------------------------------------------------------
    # -q first: not the caller's ~/.curlrc (see install.sh).
    # A release tag's tree comes from the release itself, signed; every other
    # ref -- main, a branch, a commit -- and a release from before signing
    # come from codeload, on TLS alone (and SKYLINE_SHA256 when given).
    STAGE="downloading the source"
    SIGNED=0
    case "$REF" in
        v[0-9]*.[0-9]*)
            ASSET="https://github.com/${REPO}/releases/download/${REF}/skyline-speeder-${REF}-source.tar.gz"
            info "fetching ${REPO} ${REF} (signed source)"
            # curl's own 404 stays quiet: a release from before signing has
            # none, and the lines below say what that means.
            if curl -q -fsSL --proto '=https' --tlsv1.2 -o "$TMP/src.tar.gz" "$ASSET" 2>/dev/null \
               && curl -q -fsSL --proto '=https' --tlsv1.2 -o "$TMP/src.tar.gz.sig" "$ASSET.sig" 2>/dev/null; then
                printf '%s namespaces="%s" %s\n' "$RELEASE_SIGNING_NAMESPACE" \
                    "$RELEASE_SIGNING_NAMESPACE" "$RELEASE_SIGNING_KEY" >"$TMP/release-signers"
                if ! ssh-keygen -Y verify -f "$TMP/release-signers" -I "$RELEASE_SIGNING_NAMESPACE" \
                       -n "$RELEASE_SIGNING_NAMESPACE" -s "$TMP/src.tar.gz.sig" \
                       <"$TMP/src.tar.gz" >"$TMP/verify.log" 2>&1; then
                    cat "$TMP/verify.log" >&2
                    die "the signature of skyline-speeder-${REF}-source.tar.gz does not verify against the release key:
   it is not the tree the release published. Nothing was unpacked."
                fi
                ok "signature verified (${RELEASE_SIGNING_NAMESPACE} key)"
                SIGNED=1
            else
                case " $UNSIGNED_RELEASES " in
                    *" $REF "*) warn "$REF was published before releases were signed (v0.6.0): its tree comes from codeload" ;;
                    *) die "no signed source for $REF: $ASSET or its .sig could not be downloaded.
   Every release from v0.6.0 on publishes both. Check that this host can reach github.com;
   for the development tree instead, pass --ref main." ;;
                esac
            fi ;;
    esac
    if [ "$SIGNED" -eq 0 ]; then
        URL="https://codeload.github.com/${REPO}/tar.gz/${REF}"
        info "fetching ${REPO}@${REF}"
        curl -q -fsSL --proto '=https' --tlsv1.2 -o "$TMP/src.tar.gz" "$URL" \
            || die "download failed: $URL
   Check the repository and ref exist, and that this host can reach github.com."
    fi

    # Optional integrity pin. Publish the digest alongside a release tag and
    # operators can verify what they are about to build and run as root. The
    # variable goes to the shell sudo starts, not to curl:
    #   curl ... | sudo SKYLINE_SHA256=<digest> bash -s -- --ref <tag>
    STAGE="checking the download"
    if [ -n "${SKYLINE_SHA256:-}" ]; then
        GOT="$(sha256sum "$TMP/src.tar.gz" | awk '{print $1}')"
        [ "$GOT" = "$SKYLINE_SHA256" ] \
            || die "sha256 mismatch
   expected: $SKYLINE_SHA256
   actual:   $GOT"
        ok "sha256 verified"
    elif [ "$SIGNED" -eq 0 ]; then
        warn "no SKYLINE_SHA256 given; the tarball is trusted on TLS alone"
    fi

    STAGE="unpacking the source"
    tar -xzf "$TMP/src.tar.gz" -C "$TMP" || die "extract failed"
    # Unpacking as root keeps the archive's modes, and GitHub's tarballs make
    # files group-writable (git archive's tar.umask 0002): not for a tree whose
    # install.sh, Makefile and scripts root runs again at every upgrade.
    chmod -R go-w "$TMP"
    UNPACKED="$(find "$TMP" -mindepth 1 -maxdepth 1 -type d | head -1)"
    if [ -z "$UNPACKED" ] || [ ! -x "$UNPACKED/install.sh" ]; then
        die "unexpected tarball layout: no executable install.sh at the top level"
    fi

    # A preflight changes nothing, and an uninstall has nothing to keep a tree
    # for: both run the installer straight from the download. Placing it as an
    # install does moved an operator's tree aside and then advertised "upgrade
    # or remove from there" -- after --check on a host with nothing installed,
    # after --uninstall of the very installation it pointed at. bash rather
    # than the file itself: /tmp may be mounted noexec.
    if [ "$CHECKING" -eq 1 ] || [ "$UNINSTALLING" -eq 1 ]; then
        # install.sh reports its own failures.
        FB_ARMED=0
        info "handing off to install.sh (nothing is placed in $SRC_DIR)"
        echo
        bash "$UNPACKED/install.sh" "$@"
        exit 0
    fi

    # --- place the tree -----------------------------------------------------
    # A previous tree is moved aside, never deleted: it may carry local edits,
    # and an installer that silently destroys an operator's working copy is not
    # a trade anyone agreed to. Say where it went.
    STAGE="placing the tree"
    if [ -e "$SRC_DIR" ]; then
        BACKUP="${SRC_DIR}.bak-$(date +%Y%m%d%H%M%S)"
        mv "$SRC_DIR" "$BACKUP"
        warn "existing tree moved to $BACKUP"
        # One more on every upgrade, each with a source build's target/ and
        # build/ when there was one. Never removed here (see above): counted.
        KEPT=0
        for b in "${SRC_DIR}".bak-*; do [ -e "$b" ] && KEPT=$((KEPT + 1)); done
        [ "$KEPT" -le 1 ] \
            || warn "$KEPT earlier trees are kept as ${SRC_DIR}.bak-*; remove the ones you no longer need"
    fi
    mkdir -p "$(dirname "$SRC_DIR")"
    mv "$UNPACKED" "$SRC_DIR"
    ok "source tree at $SRC_DIR"

    # --- hand off -----------------------------------------------------------
    # install.sh owns every check that matters (distribution, kernel floor, BTF,
    # cgroup v2, verifier pass). Duplicating them here would mean two copies to
    # keep in sync, so this script deliberately checks none of them -- but for
    # the kernel floor above, which never changes and saves a refused host the
    # packages and the download.
    FB_ARMED=0  # install.sh reports its own failures
    info "handing off to install.sh"
    echo
    "$SRC_DIR/install.sh" "$@"

    # A later run of the kept installer has none of this environment, so a
    # --repo install says which repository to keep upgrading from.
    REPO_ENV=''
    [ "$REPO" = CYBERVERSE-Research/skyline-speeder ] || REPO_ENV="SKYLINE_REPO=$REPO "
    echo
    ok "installer kept at $SRC_DIR -- upgrade or remove from there:"
    echo "     ${SUDO}${REPO_ENV}$SRC_DIR/install.sh               # upgrade to the latest release"
    echo "     ${SUDO}$SRC_DIR/install.sh --uninstall   # also puts this host on bbr + fq"
}

main "$@"
