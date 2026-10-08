#!/usr/bin/env bash
set -euo pipefail

MODE=${1:-development}
FORMAL_MIN_CPUS=${SKYLINE_FORMAL_MIN_CPUS:-16}
FORMAL_MIN_MEM_GIB=${SKYLINE_FORMAL_MIN_MEM_GIB:-32}
FORMAL_MIN_DISK_GIB=${SKYLINE_FORMAL_MIN_DISK_GIB:-120}
TCG_MIN_CPUS=${SKYLINE_TCG_MIN_CPUS:-12}
TCG_MIN_MEM_GIB=${SKYLINE_TCG_MIN_MEM_GIB:-16}
TCG_MIN_DISK_GIB=${SKYLINE_TCG_MIN_DISK_GIB:-60}
FAILED=0
PYTHON=${SKYLINE_PYTHON:-.venv/bin/python}

case "$MODE" in
    development|formal|tcg-validation) ;;
    *) echo "Usage: $0 [development|formal|tcg-validation]" >&2; exit 2 ;;
esac

check_command() {
    local command_name=$1
    if command -v "$command_name" >/dev/null 2>&1; then
        printf 'ok      command %-24s %s\n' \
            "$command_name" "$(command -v "$command_name")"
    else
        printf 'missing command %s\n' "$command_name"
        FAILED=1
    fi
}

check_optional() {
    local command_name=$1
    if command -v "$command_name" >/dev/null 2>&1; then
        printf 'ok      optional %-23s %s\n' \
            "$command_name" "$(command -v "$command_name")"
    else
        printf 'warning optional command %s is unavailable\n' "$command_name"
    fi
}

check_bpftool() {
    # Deliberately not just `command -v`: Ubuntu's linux-tools-common
    # package installs bpftool as a wrapper that re-execs
    # /usr/lib/linux-tools/$(uname -r)/bpftool -- present on PATH and
    # passing `command -v` even when there is no matching linux-tools
    # package for this host's running kernel, in which case invoking it
    # fails outright. A functional check (does it actually run) is the
    # only way to catch that; honors BPFTOOL the same way Makefile does,
    # so a repo-local build (see DEPLOY.md section 3) is accepted.
    local bpftool_bin=${BPFTOOL:-bpftool}
    if ! command -v "$bpftool_bin" >/dev/null 2>&1; then
        printf 'missing command %s\n' "$bpftool_bin"
        FAILED=1
        return
    fi
    if "$bpftool_bin" version >/dev/null 2>&1; then
        printf 'ok      command %-24s %s\n' \
            "$bpftool_bin" "$(command -v "$bpftool_bin")"
    else
        printf 'missing command %s (found on PATH but failed to run -- on Ubuntu this usually means the linux-tools package for `uname -r` is not installed; build a repo-local bpftool from the target kernel source and pass BPFTOOL=<path>)\n' \
            "$bpftool_bin"
        FAILED=1
    fi
}

for command_name in ip tc jq ssh awk clang cargo; do
    check_command "$command_name"
done
check_bpftool
if [ -x "$PYTHON" ]; then
    PYTHON_PATH=$(CDPATH= cd -- "$(dirname -- "$PYTHON")" && pwd)/$(basename -- "$PYTHON")
    printf 'ok      project Python           %s (%s)\n' \
        "$PYTHON_PATH" "$($PYTHON --version 2>&1)"
else
    echo "missing project Python $PYTHON; run make venv" >&2
    FAILED=1
fi

TC=${SKYLINE_TC:-tc}
if command -v "$TC" >/dev/null 2>&1; then
    TC=$(realpath -- "$(command -v "$TC")")
    NETEM_HELP=$("$TC" qdisc add dev lo root netem help 2>&1 || true)
    if grep -qw seed <<<"$NETEM_HELP"; then
        echo "ok      NetEm fixed seed          $TC"
    elif [ "$MODE" = formal ] || [ "$MODE" = tcg-validation ]; then
        echo "missing NetEm fixed-seed support; run infra/build-iproute2.sh" >&2
        FAILED=1
    else
        echo "warning NetEm fixed seed is unavailable"
    fi
fi

check_deb_package() {
    # Some of infra/kernel/build.sh's `make bindeb-pkg` dependencies (e.g.
    # debhelper, libdw-dev) provide no command of their own, or provide one
    # under an unrelated name -- `command -v` cannot see them at all, and a
    # missing one only surfaces hours into a kernel build as an opaque
    # dpkg-buildpackage error. Debian/Ubuntu-specific by design; the
    # documented formal workflow only targets Ubuntu guests/hosts.
    local package_name=$1
    if ! command -v dpkg-query >/dev/null 2>&1; then
        printf 'warning cannot verify package %s (dpkg-query unavailable)\n' \
            "$package_name"
        return
    fi
    if dpkg-query --show --showformat='${Status}' "$package_name" 2>/dev/null \
        | grep -q '^install ok installed$'; then
        printf 'ok      package %s\n' "$package_name"
    else
        printf 'missing package %s (needed by infra/kernel/build.sh'"'"'s make bindeb-pkg)\n' \
            "$package_name"
        FAILED=1
    fi
}

if [ "$MODE" = formal ] || [ "$MODE" = tcg-validation ]; then
    for command_name in qemu-system-x86_64 qemu-img cloud-localds iperf3 \
        perf ethtool curl sha256sum taskset bc bison flex pahole \
        dpkg-buildpackage fakeroot; do
        check_command "$command_name"
    done
    for package_name in debhelper libdw-dev; do
        check_deb_package "$package_name"
    done
else
    for command_name in qemu-system-x86_64 qemu-img cloud-localds iperf3 \
        perf ethtool taskset; do
        check_optional "$command_name"
    done
fi

CPU_COUNT=$(getconf _NPROCESSORS_ONLN)
MEM_GIB=$(awk '/MemTotal:/ { printf "%d", $2 / 1024 / 1024 }' /proc/meminfo)
DISK_GIB=$(df -Pk . | awk 'NR == 2 { printf "%d", $4 / 1024 / 1024 }')
KERNEL_RELEASE=$(uname -r)

printf 'info    kernel                   %s\n' "$KERNEL_RELEASE"
printf 'info    cpu                      %s logical CPUs\n' "$CPU_COUNT"
printf 'info    memory                   %s GiB\n' "$MEM_GIB"
printf 'info    workspace disk available %s GiB\n' "$DISK_GIB"

if [ -r /sys/kernel/btf/vmlinux ]; then
    echo "ok      kernel BTF"
else
    # Not a hard requirement: the double-VM formal/tcg-validation workflow
    # always builds BPF against a GUEST kernel's vmlinux via explicit
    # `make VMLINUX_BTF=<path> bpf` (see DEPLOY.md section 3 and
    # research/experiments/README.md), never against the host's own
    # running kernel. Host BTF only matters if something on this host will
    # run `make bpf` with no VMLINUX_BTF override at all.
    echo "warning /sys/kernel/btf/vmlinux is unavailable -- fine if BPF is built against a guest kernel via VMLINUX_BTF=<path>, required only for building against this host's own kernel"
fi

if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    echo "ok      writable /dev/kvm"
    KVM_OK=1
else
    echo "warning writable /dev/kvm is unavailable"
    KVM_OK=0
fi
if grep -Eq '(^| )(vmx|svm)( |$)' /proc/cpuinfo; then
    echo "ok      CPU virtualization flag"
else
    echo "warning CPU virtualization flag vmx/svm is not exposed"
    KVM_OK=0
fi

check_resources() {
    local label=$1
    local min_cpus=$2
    local min_mem=$3
    local min_disk=$4
    if [ "$CPU_COUNT" -lt "$min_cpus" ]; then
        echo "$label requires at least $min_cpus logical CPUs" >&2
        FAILED=1
    fi
    if [ "$MEM_GIB" -lt "$min_mem" ]; then
        echo "$label requires at least $min_mem GiB RAM" >&2
        FAILED=1
    fi
    if [ "$DISK_GIB" -lt "$min_disk" ]; then
        echo "$label requires at least $min_disk GiB free disk" >&2
        FAILED=1
    fi
}

if [ "$MODE" = formal ]; then
    if [ "$KVM_OK" -ne 1 ]; then
        echo "formal testing requires nested KVM or two cloud hosts" >&2
        FAILED=1
    fi
    check_resources formal "$FORMAL_MIN_CPUS" "$FORMAL_MIN_MEM_GIB" \
        "$FORMAL_MIN_DISK_GIB"
elif [ "$MODE" = tcg-validation ]; then
    check_resources tcg-validation "$TCG_MIN_CPUS" "$TCG_MIN_MEM_GIB" \
        "$TCG_MIN_DISK_GIB"
    echo "info    execution class          tcg-validation"
    echo "info    performance validity     false"
fi

if [ "$FAILED" -ne 0 ]; then
    echo "preflight failed" >&2
    exit 1
fi
echo "preflight passed for $MODE mode"
