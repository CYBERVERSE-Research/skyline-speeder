#!/usr/bin/env bash
#
# Cross-kernel CO-RE portability check.
#
# CO-RE resolves struct field offsets at LOAD time, against the running
# kernel's own BTF -- bpftool-generated vmlinux.h carries
# `#pragma clang attribute push (__attribute__((preserve_access_index)))`, so
# every direct field access in the BPF sources emits a relocation record rather
# than a baked-in offset. An object built against one kernel's types is
# therefore expected to load on another.
#
# "Expected to" is not "verified", and shipping prebuilt objects rests entirely
# on this holding, so this script makes the check reproducible instead of a
# one-off somebody did by hand once.
#
# Two phases, separate because they necessarily run on different kernels:
#
#   freeze   on the BUILD kernel: copy the objects aside and record their
#            digests together with the kernel they were compiled against
#   verify   on the TARGET kernel: prove the bytes are unchanged, then push
#            them through that kernel's verifier
#
# How the frozen directory travels between the two is the operator's business
# -- reboot into another kernel, copy it to another machine, boot a VM. This
# script knows nothing about hosts or transports on purpose: baking in a
# transport would bake in an environment, and this has to work for whatever
# kernel pair someone needs to check.
#
#   infra/kernel/core-portability.sh freeze
#   <get to the other kernel>
#   infra/kernel/core-portability.sh verify
#
set -euo pipefail

REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
FROZEN_DIR=${SKYLINE_FROZEN_DIR:-$REPOSITORY_ROOT/build/core-portability}
OBJECTS_DIR=${SKYLINE_BPF_DIR:-$REPOSITORY_ROOT/build/bpf}
MANIFEST_NAME=manifest.env
DIGESTS_NAME=objects.sha256

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

find_speederd() {
    if [ -n "${SKYLINE_SPEEDERD:-}" ]; then
        printf '%s' "$SKYLINE_SPEEDERD"
        return
    fi
    # An unpacked artifact's own daemon, when the set was frozen from one and
    # this is the same machine: the objects and the daemon that loads them
    # come from the same build.
    for candidate in \
        "${ARTIFACT_SPEEDERD:-}" \
        "$REPOSITORY_ROOT/target/release/skyline-speederd" \
        "$REPOSITORY_ROOT/target/debug/skyline-speederd" \
        /usr/local/sbin/skyline-speederd
    do
        [ -n "$candidate" ] && [ -x "$candidate" ] && { printf '%s' "$candidate"; return; }
    done
    printf ''
}

phase_freeze() {
    [ -d "$OBJECTS_DIR" ] || die "no objects at $OBJECTS_DIR (run 'make bpf' first)"
    # shellcheck disable=SC2312  # the glob is checked on the next line
    set -- "$OBJECTS_DIR"/*.bpf.o
    [ -e "$1" ] || die "no *.bpf.o below $OBJECTS_DIR"

    rm -rf -- "$FROZEN_DIR"
    mkdir -p -- "$FROZEN_DIR/bpf"
    cp -- "$@" "$FROZEN_DIR/bpf/"

    # Relative paths in the digest file so `sha256sum -c` works from the frozen
    # directory regardless of where it ends up.
    ( cd -- "$FROZEN_DIR/bpf" && sha256sum -- *.bpf.o > "../$DIGESTS_NAME" )

    # The running kernel at freeze time is NOT necessarily where the types came
    # from: a release build compiles against a header generated elsewhere
    # (PREBUILT_VMLINUX_H). Record which header it was, so the provenance of
    # the type definitions is recoverable later instead of being guessed from
    # whatever kernel happened to be booted -- and only where it is known.
    # This tree's own vmlinux.h says nothing about objects from somewhere
    # else; an unpacked artifact names its header in MANIFEST, as the digest
    # of the .h.gz release.yml fetched (not of the header itself).
    local header=$REPOSITORY_ROOT/bpf/include/vmlinux.h
    local header_digest=unknown reference_digest=unknown speederd=
    local artifact_root
    artifact_root=$(CDPATH= cd -- "$OBJECTS_DIR/.." && pwd)
    if [ -r "$artifact_root/MANIFEST" ]; then
        reference_digest=$(sed -n 's/^[[:space:]]*reference_vmlinux_sha256=//p' \
            -- "$artifact_root/MANIFEST" | head -n 1)
        reference_digest=${reference_digest:-unknown}
        [ ! -x "$artifact_root/bin/skyline-speederd" ] \
            || speederd=$artifact_root/bin/skyline-speederd
    elif [ "$(CDPATH= cd -- "$OBJECTS_DIR" && pwd)" = "$REPOSITORY_ROOT/build/bpf" ] \
        && [ -r "$header" ]; then
        header_digest=$(sha256sum -- "$header" | cut -d' ' -f1)
    fi

    cat > "$FROZEN_DIR/$MANIFEST_NAME" <<EOF
BUILT_UNDER_KERNEL=$(uname -r)
BUILT_ON_ARCH=$(uname -m)
VMLINUX_H_SHA256=$header_digest
REFERENCE_VMLINUX_GZ_SHA256=$reference_digest
ARTIFACT_SPEEDERD=$(printf '%q' "$speederd")
FROZEN_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
OBJECT_COUNT=$#
EOF

    echo "Frozen $# object(s) on $(uname -r):"
    sed 's/^/  /' -- "$FROZEN_DIR/$DIGESTS_NAME"
    echo
    echo "Directory: $FROZEN_DIR"
    echo "Get to the target kernel, then run: $0 verify"
}

phase_verify() {
    [ -r "$FROZEN_DIR/$MANIFEST_NAME" ] || die "no frozen set at $FROZEN_DIR (run '$0 freeze' first)"
    # shellcheck disable=SC1090  # path is computed; the file is generated by phase_freeze
    . "$FROZEN_DIR/$MANIFEST_NAME"

    echo "Frozen on   : ${BUILT_UNDER_KERNEL:-unknown} (the kernel running when they were frozen)"
    echo "vmlinux.h   : ${VMLINUX_H_SHA256:-unknown}"
    [ "${REFERENCE_VMLINUX_GZ_SHA256:-unknown}" = unknown ] \
        || echo "header .gz : $REFERENCE_VMLINUX_GZ_SHA256 (the artifact's MANIFEST)"
    echo "Running on  : $(uname -r)"
    # The objects are built for one architecture (-D__TARGET_ARCH_x86 in the
    # Makefile); on another, a failure would say nothing about CO-RE.
    if [ -n "${BUILT_ON_ARCH:-}" ] && [ "$BUILT_ON_ARCH" != "$(uname -m)" ]; then
        die "frozen on $BUILT_ON_ARCH, running on $(uname -m): the objects are built for one architecture"
    fi
    if [ "${BUILT_UNDER_KERNEL:-}" = "$(uname -r)" ]; then
        echo "Note: same kernel on both sides. This is the control case -- it"
        echo "      exercises the verifier but proves nothing about portability."
    fi

    # Unchanged bytes is the whole point: without this the result could just be
    # a rebuild against the current kernel, which would prove nothing.
    echo
    echo "== byte identity =="
    ( cd -- "$FROZEN_DIR/bpf" && sha256sum -c -- "../$DIGESTS_NAME" ) \
        || die "frozen objects changed since they were recorded"

    local speederd
    speederd=$(find_speederd)
    [ -n "$speederd" ] || die "no skyline-speederd binary found; set SKYLINE_SPEEDERD=<path>"

    local config=${SKYLINE_CONFIG:-$REPOSITORY_ROOT/config/speeder.toml}
    [ -r "$config" ] || die "config not readable: $config"

    # Point the daemon at the frozen objects rather than wherever the config
    # normally looks, without editing the operator's config file. Exactly one
    # bpf_dir line, or nothing: with none rewritten, the run verified whatever
    # objects the config named and reported them as the frozen ones.
    local generated="$FROZEN_DIR/verify.toml"
    awk -v dir="$FROZEN_DIR/bpf" '
        /^[[:space:]]*bpf_dir[[:space:]]*=/ { print "bpf_dir = \"" dir "\""; n++; next }
        { print }
        END { exit (n == 1 ? 0 : 1) }' "$config" > "$generated" \
        || die "$config needs exactly one line bpf_dir = \"...\" to point at the frozen objects"

    echo
    echo "== kernel verifier on $(uname -r) =="
    echo "  binary: $speederd"
    if "$speederd" --config "$generated" --validate-only --verify-bpf > "$FROZEN_DIR/verify.log" 2>&1; then
        echo "  PASS -- objects frozen on ${BUILT_UNDER_KERNEL:-unknown} load on $(uname -r)"
        echo "  full output: $FROZEN_DIR/verify.log"
    else
        echo "  FAIL -- see $FROZEN_DIR/verify.log" >&2
        tail -20 -- "$FROZEN_DIR/verify.log" >&2
        exit 1
    fi
}

case "${1:-}" in
    freeze) phase_freeze ;;
    verify) phase_verify ;;
    *)
        sed -n '2,33p' -- "$0" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
