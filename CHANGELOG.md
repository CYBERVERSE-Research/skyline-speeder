# Changelog

Notable changes to Skyline Speeder. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`SKYLINE_ABI_VERSION` is versioned separately from the release number: it tracks
the layout of `bpf/include/skyline_abi.h` and any bump to it is a breaking change
for anyone holding a prebuilt `.bpf.o`.

## [Unreleased]

## [0.1.0] - 2026-09-19

First public release.

Everything below is in this release. Nothing was published before it, so there
is no "changed since" to report: work that happened between the first commit and
the tag is release content, not a changelog of revisions, and is recorded as such
rather than as fixes to a version nobody could have installed.

### Added

**Data path**

- `skyline_cc` — eBPF struct_ops congestion control with four independently
  switchable modules: adaptive cwnd (M2), loss-rate compensation (M3), pacing
  (M4), and early-loss observation (M1).
- `skyline_policy` — cgroup sockops program: per-connection congestion-control
  selection, plus a per-flow dynamic RTO floor and ceiling.
- `skyline_tc` — TC egress program: packet/byte/GSO accounting and retransmit
  DSCP marking for IPv4 and IPv6.

**Control plane**

- `skyline-speederd` / `ssctl` — capability probing, online reconfiguration
  through a double-slot generation counter, graceful drain, and a
  `--validate-only --verify-bpf` health check that leaves no runtime state.
- `ssctl enable` activates host-wide: on a successful struct_ops attach it
  writes `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP
  connection on the machine uses it, not only connections from processes inside
  the cgroup. The sysctl write happens after the attach, because the kernel
  rejects an algorithm name it has not seen registered.
- `ssctl drain` is the symmetric reverse: the sysctl goes back to `fallback_cc`
  first, then the cgroup dispatch path closes, then it waits, then it
  unregisters. A drain that times out leaves the struct_ops attached but the
  default already back on `fallback_cc`.

**Installation**

- `install.sh` — one-click Debian/Ubuntu installer with `--prebuilt`, `--check`,
  `--no-enable` and `--uninstall`.
- `--prebuilt` installs published artifacts instead of compiling: no clang, no
  LLVM, no bpftool and no Rust on the target host, only `curl` and `tar`.
  `--release <tag>` pins a version; `SKYLINE_ARTIFACT_URL` takes an https URL
  (mirror, internal artifact store) or a local path, for hosts with no route to
  github.com.
- The installer snapshots the congestion control and default qdisc to
  `/etc/skyline-speeder/pre-install-state` before it starts anything, and
  `--uninstall` restores both. A host that ran BBR comes back on BBR.
- `scripts/bootstrap.sh` — remote installer entry point for `curl | sudo bash`,
  with optional `SKYLINE_SHA256` tarball pinning.

**Build and release**

- `.github/workflows/release.yml` builds and publishes artifacts on a `v*` tag,
  compiling against a pinned reference header rather than the runner's own
  kernel, and refusing to build when that header is not configured.
- `infra/kernel/make-reference-vmlinux.sh` generates that header on a host
  running the oldest supported kernel and prints the digest it is pinned by.
- `infra/kernel/core-portability.sh` — a repeatable two-phase check that objects
  built against one kernel's types load on another: `freeze` records the objects
  and their digests on the build kernel, `verify` proves the bytes are unchanged
  and pushes them through the target kernel's verifier. It knows nothing about
  hosts or transports, so it works for whatever kernel pair needs checking.
- `make PREBUILT_VMLINUX_H=<path> bpf` builds against a header generated
  elsewhere, with no bpftool and no `/sys/kernel/btf/vmlinux` required.
- CI asserts every built object carries a `.BTF.ext` section — without it an
  object has no CO-RE relocation records and is bound to its build kernel — and
  uploads the objects as a build artifact.

**Research**

- Two-VM experiment harness under `research/experiments/` and the performance
  report it produces.

### Verified

- Kernels `6.12.101`, `6.18.42` and `7.1.6`, including IPv4/IPv6 data-path smoke
  tests. `6.1.180` and `6.6.148` are rejected at load, as expected.
- **CO-RE portability, both directions**, on Debian 13 with the objects checked
  byte-identical at each step: built under 6.12.63, the verifier passes on
  6.19.14 and the objects attach and carry real traffic there; built on 6.19.14
  against a 6.12 header, the verifier passes on 6.12.63.
- Neutrality: with all modules off, within 0.01% of stock kernel CUBIC on a
  zero-loss link.
- Retransmit DSCP marking: about 97,000 marked segments across IPv4 and IPv6
  captures, zero false positives.
- The one-click install, the prebuilt install and the uninstall path, end to end
  on a clean Debian 13 host running 6.12.63.

### Licensing

- Copyright **CYBERVERSE LLC**, **GPL-2.0-only** throughout, with an
  `SPDX-License-Identifier` on every source file. `LICENSE` holds the complete
  GPL-2.0 text; `NOTICE` records why GPL-2.0 is required, the upstream kernel
  attributions, and the trademark terms.

### Known issues and limitations

- **Congestive bottlenecks are out of scope.** Loss is assumed to carry no
  congestion information; on a genuinely congested path this keeps pushing and
  harms both itself and everything sharing the path. The queueing-delay/ECN
  guardrail is the only self-protection and is not a general safety net.
- **Reordering is a weak spot.** The `reorder-dsack` scenario measures roughly
  9% below CUBIC and 13% below BBR.
- **Bandwidth validated only to 100 Mbit/s** — a structural ceiling of the test
  bed, not a property of the algorithm.
- **No cross-flow coordination.** Each flow estimates bandwidth independently and
  applies its own gain, so several flows sharing one bottleneck will collectively
  overshoot.
- **`ssctl drain` cannot complete over SSH.** The operator's own SSH connection
  is a skyline_cc flow and will not end while drain waits, so drain always
  reaches its timeout. It is safe — the sysctl is written back to `fallback_cc`
  before the wait begins — but the struct_ops stays attached until that session
  closes.
- **The one-line installer needs `curl`**, which a minimal server image may not
  have. `bootstrap.sh` installs it when missing, but only once it is already
  running; the READMEs show the prerequisite and a `wget` equivalent.
- **CO-RE fixes offsets, not names.** A field renamed or removed in a future
  kernel makes relocation fail at load. Two kernels and one architecture is
  evidence, not proof, which is why the check is a script rather than a claim.
- **`early-loss` is an observation counter only.** It drives no decision.
- **`runtime.pin_dir` is unused.** The field is parsed and validated but nothing
  pins BPF objects yet.
- **The `ssctl` wire protocol has no authentication**, relying entirely on Unix
  socket file permissions. Multi-tenant hosts need additional access control.
- The experiment harness requires **Python 3.11 or newer** (`tomllib`).

[Unreleased]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/releases/tag/v0.1.0
