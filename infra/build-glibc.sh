#!/usr/bin/env bash
#
# Build skyline-speederd and ssctl for glibc in a container whose glibc is
# older than any host's this project is installed on: the control plane of a
# release's glibc artifact. The release workflow runs this, and so does CI;
# anyone with docker gets the same binaries.
#
#   infra/build-glibc.sh       # -> target/glibc/release/{skyline-speederd,ssctl}
#                              #    and target/glibc/BUILT_IN (for MANIFEST)
#
# A binary built against a glibc runs on that glibc and every later one, not
# on an earlier one: the libbpf that libbpf-sys compiles picks up whatever
# symbol versions the build host's headers offer. Built on the release runner
# (Ubuntu 24.04) skyline-speederd asked for GLIBC_2.38, and Debian 12 (2.36),
# Ubuntu 22.04 (2.35) and the RHEL 9 family (2.34) -- hosts that run a 6.12
# kernel from backports, HWE or ELRepo -- had to install a toolchain and
# compile instead: three of the first seven error reports of failed installs
# came out of exactly that build. AlmaLinux 9 has the oldest glibc among the
# distributions install.sh supports, 2.34, which install.sh checks for as
# PREBUILT_GLIBC_MIN: keep the two in step, and FLOOR below with them.
#
# The binaries link libelf.so.1 and libz.so.1 besides glibc, under the same
# names on every one of those distributions. install.sh gets them on the
# prebuilt path with iproute2 under apt (it depends on libelf, and zlib is
# part of every Debian) and by name under dnf. Once built, the binaries are
# run in clean containers that have those packages and nothing else, the
# oldest of each family and a current one: an artifact that needs a newer
# glibc or one more library fails here and not on an operator's host.
#
# No bpftool, clang or kernel BTF: the BPF objects are built once, on the
# runner, and both artifacts carry the same files.
set -euo pipefail

BUILD_IMAGE=almalinux:9
FLOOR=2.34
RUN_IMAGES=(almalinux:9 ubuntu:22.04 debian:12 fedora:latest)
BUILD_PKGS=(gcc make pkgconf-pkg-config elfutils-libelf-devel zlib-devel binutils)
TARGET=target/glibc

case "${1:-}" in
    '') ;;
    -h|--help) awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
    *) echo "usage: $0" >&2; exit 2 ;;
esac
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# As root in the container, then the output handed back to whoever ran this:
# otherwise a CI runner's next step, or the next cargo run here, meets a
# target directory it cannot write.
docker run --rm -v "$REPO_ROOT:/src" -w /src \
    -e TARGET="$TARGET" -e FLOOR="$FLOOR" -e OWNER="$(id -u):$(id -g)" \
    "$BUILD_IMAGE" sh -euc '
        dnf -q -y --setopt=install_weak_deps=False install "$@"
        # rust-toolchain.toml pins the compiler; rustup installs exactly that
        # on the first cargo run inside the tree.
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y -q --profile minimal --default-toolchain none
        . "$HOME/.cargo/env"
        cargo build --workspace --release --locked --target-dir "$TARGET"
        # The newest glibc symbol version either binary asks for is the oldest
        # glibc it runs on. It cannot exceed this image, but an image that
        # moved on would raise it silently, and install.sh would go on sending
        # hosts below it a release they cannot run.
        needs=$(objdump -T "$TARGET/release/skyline-speederd" "$TARGET/release/ssctl" \
            | grep -o "GLIBC_[0-9][0-9.]*" | sed "s/^GLIBC_//" | sort -V | tail -n 1)
        if [ "$(printf "%s\n%s\n" "$needs" "$FLOOR" | sort -V | tail -n 1)" != "$FLOOR" ]; then
            echo "the binaries need glibc $needs, newer than the $FLOOR this build promises" >&2
            exit 1
        fi
        # What the binaries were built in, for the MANIFEST of the artifact.
        printf "%s, glibc %s (needs %s), %s\n" "$(. /etc/os-release && echo "$PRETTY_NAME")" \
            "$(getconf GNU_LIBC_VERSION | cut -d" " -f2)" "$needs" \
            "$(rustc --version)" >"$TARGET/BUILT_IN"
        chown -R "$OWNER" "$TARGET"
    ' sh "${BUILD_PKGS[@]}"
cat "$REPO_ROOT/$TARGET/BUILT_IN"

for image in "${RUN_IMAGES[@]}"; do
    docker run --rm -v "$REPO_ROOT/$TARGET/release:/built:ro" "$image" sh -euc '
        if command -v apt-get >/dev/null 2>&1; then
            export DEBIAN_FRONTEND=noninteractive
            apt-get -qq update
            apt-get -qq -y --no-install-recommends install iproute2 >/dev/null
        else
            dnf -q -y --setopt=install_weak_deps=False install \
                "libelf.so.1()(64bit)" "libz.so.1()(64bit)" >/dev/null
        fi
        printf "%s, glibc %s:\n" "$(. /etc/os-release && echo "$PRETTY_NAME")" \
            "$(getconf GNU_LIBC_VERSION | cut -d" " -f2)"
        for b in skyline-speederd ssctl; do
            printf "  %s  needs %s\n" "$(/built/$b --version)" \
                "$(ldd /built/$b | awk "\$2 == \"=>\" { printf \"%s%s\", s, \$1; s = \",\" }")"
        done
    ' sh
done
