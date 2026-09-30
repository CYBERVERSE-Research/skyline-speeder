#!/usr/bin/env bash
#
# Build skyline-speederd and ssctl for musl, the C library of Alpine, in an
# Alpine container: the control plane of a release's -musl artifact. The
# release workflow runs this, and so does CI; anyone with docker gets the same
# binaries.
#
#   infra/build-musl.sh        # -> target/musl/release/{skyline-speederd,ssctl}
#                              #    and target/musl/BUILT_IN (for MANIFEST)
#
# Alpine 3.21, the oldest Alpine whose kernel meets the 6.12 floor. A binary
# built against a musl runs on that musl and every later one, not on an earlier
# one, so the oldest Alpine that can run skyline_cc at all is the one to build
# on. (3.21 to 3.23 all ship musl 1.2.5, which install.sh checks for as
# PREBUILT_MUSL_MIN.)
#
# The binaries link dynamically, as .cargo/config.toml makes every musl build
# of this tree do: statically, the link fails on -lz. At run time they need
# libelf, zlib, zstd and libgcc_s besides musl -- RUNTIME below, which
# install.sh installs on the apk prebuilt path; keep the two lists in step.
# Once built, the binaries are run in clean containers that have those
# packages and nothing else, the oldest Alpine and the newest: an artifact
# that needs one more library, or a package that changed its name, fails here
# and not on an operator's host.
#
# No bpftool, clang or kernel BTF: the -musl artifact carries the BPF objects
# the glibc artifact does, built once, byte for byte.
set -euo pipefail

BUILD_IMAGE=alpine:3.21
RUN_IMAGES=(alpine:3.21 alpine:latest)
BUILD_PKGS=(build-base pkgconf elfutils-dev zlib-dev linux-headers curl)
RUNTIME=(libelf zlib zstd-libs libgcc)
TARGET=target/musl

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
    -e TARGET="$TARGET" -e OWNER="$(id -u):$(id -g)" \
    "$BUILD_IMAGE" sh -euc '
        apk add --no-progress -q "$@"
        # rust-toolchain.toml pins the compiler; rustup installs exactly that
        # on the first cargo run inside the tree.
        curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y -q --profile minimal --default-toolchain none
        . "$HOME/.cargo/env"
        cargo build --workspace --release --locked --target-dir "$TARGET"
        # What the binaries were built in, for the MANIFEST of the artifact.
        printf "Alpine %s, musl %s, %s\n" "$(cat /etc/alpine-release)" \
            "$(/lib/ld-musl-*.so.1 2>&1 | sed -n "s/^Version //p")" \
            "$(rustc --version)" >"$TARGET/BUILT_IN"
        chown -R "$OWNER" "$TARGET"
    ' sh "${BUILD_PKGS[@]}"

for image in "${RUN_IMAGES[@]}"; do
    docker run --rm -v "$REPO_ROOT/$TARGET/release:/built:ro" "$image" sh -euc '
        apk add --no-progress -q "$@"
        printf "%s, musl %s:\n" "$(cat /etc/alpine-release)" \
            "$(/lib/ld-musl-*.so.1 2>&1 | sed -n "s/^Version //p")"
        for b in skyline-speederd ssctl; do
            printf "  %s  needs %s\n" "$(/built/$b --version)" \
                "$(scanelf --nobanner -F "%n#F" /built/$b)"
        done
    ' sh "${RUNTIME[@]}"
done
