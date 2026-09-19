# Changelog

Notable changes to Skyline Speeder. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`SKYLINE_ABI_VERSION` is versioned separately from the release number: it tracks
the layout of `bpf/include/skyline_abi.h` and any bump to it is a breaking change
for anyone holding a prebuilt `.bpf.o`.

## [Unreleased]

### Added

- `infra/kernel/core-portability.sh` — a repeatable two-phase check that objects
  built against one kernel's types load on another: `freeze` records the objects
  and their digests on the build kernel, `verify` proves the bytes are unchanged
  and pushes them through the target kernel's verifier. It knows nothing about
  hosts or transports, so it works for whatever kernel pair needs checking.
- `make PREBUILT_VMLINUX_H=<path> bpf` builds against a header generated
  elsewhere, with no bpftool and no `/sys/kernel/btf/vmlinux` required. This is
  what lets the objects be built in a container or a release pipeline.
- CI now asserts every built object carries a `.BTF.ext` section — without it an
  object has no CO-RE relocation records and is bound to its build kernel — and
  uploads the objects as a build artifact.

### Changed

- Corrected the claim that a `.bpf.o` built on a different kernel version cannot
  be shipped and reused. It can: the objects are CO-RE relocatable. Verified in
  both directions on Debian 13 — built under 6.12.63, loaded and run under
  6.19.14; and built against a 6.12 header on 6.19.14, loaded under 6.12.63.
  The installer still builds locally; distributing prebuilt objects is separate
  work.

### Changed

- **`ssctl enable` now activates host-wide.** On a successful struct_ops attach
  it writes `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP
  connection on the machine uses it, not only connections from processes inside
  the cgroup. The sysctl write happens after the attach because the kernel
  rejects an algorithm name it has not seen registered.
- **`ssctl drain` is now symmetric.** It writes the sysctl back to `fallback_cc`
  first, then closes the cgroup dispatch path, then waits, then unregisters —
  the same order `infra/boot-disable.sh` uses. A drain that times out now leaves
  the struct_ops attached but the default already back on `fallback_cc`.
- `infra/boot-enable.sh` no longer writes the congestion-control sysctl itself;
  `ssctl enable` owns it. The script now reads it back and fails loudly if the
  default did not move. The write in `infra/boot-disable.sh` is kept
  deliberately as the path that still works when the daemon is already gone.

### Fixed

- CI: the BPF job failed on every run. Ubuntu's `bpftool` package is a wrapper
  script from `linux-tools-common` that dispatches to a binary matching
  `uname -r`; no such package exists for the runner kernel, so the wrapper
  passed a `command -v` check and then failed at run time. CI now installs the
  upstream static bpftool, pinned by sha256, and verifies it by running it.
- `install.sh --uninstall` left the host on `fallback_cc` instead of whatever it
  ran before. The installer now snapshots the congestion control and default
  qdisc to `/etc/skyline-speeder/pre-install-state` before starting anything,
  and the uninstaller restores both.
- Installer output was buried under dpkg unpack lines and locale warnings.
  `LC_ALL` is pinned and apt runs with `Dpkg::Use-Pty=0`, with the full log
  printed only when a step actually fails.

- `ssctl drain` followed by a manual `ssctl enable` no longer leaves the system
  default congestion control on `fallback_cc`.

### Licensing

- Copyright is recorded as **CYBERVERSE LLC**. The project is **GPL-2.0-only**
  throughout, and every source file now carries an `SPDX-License-Identifier`.
  `LICENSE` holds the complete GPL-2.0 text; `NOTICE` records why GPL-2.0 is
  required, the upstream kernel attributions, and the trademark terms.

## [0.1.0] - 2026-09-18

First public release.

### Added

- `skyline_cc` — eBPF struct_ops congestion control with four independently
  switchable modules: adaptive cwnd (M2), loss-rate compensation (M3), pacing
  (M4), and early-loss observation (M1).
- `skyline_policy` — cgroup sockops program: per-connection congestion-control
  selection, plus a per-flow dynamic RTO floor and ceiling.
- `skyline_tc` — TC egress program: packet/byte/GSO accounting and retransmit
  DSCP marking for IPv4 and IPv6.
- `skyline-speederd` / `ssctl` — Rust control plane: capability probing, online
  reconfiguration through a double-slot generation counter, graceful drain, and
  a `--validate-only --verify-bpf` health check that leaves no runtime state.
- `install.sh` — one-click Debian/Ubuntu installer with `--check`, `--no-enable`
  and `--uninstall`.
- `scripts/bootstrap.sh` — remote installer entry point for `curl | sudo bash`,
  with optional `SKYLINE_SHA256` tarball pinning.
- Two-VM experiment harness under `research/experiments/` and the performance
  report it produces.

### Verified

- Kernels `6.12.101`, `6.18.42` and `7.1.6`, including IPv4/IPv6 data-path smoke
  tests. `6.1.180` and `6.6.148` are rejected at load, as expected.
- Neutrality: with all modules off, within 0.01% of stock kernel CUBIC on a
  zero-loss link.
- Retransmit DSCP marking: about 97,000 marked segments across IPv4 and IPv6
  captures, zero false positives.

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
- **`early-loss` is an observation counter only.** It drives no decision.
- **`runtime.pin_dir` is unused.** The field is parsed and validated but nothing
  pins BPF objects yet.
- **The `ssctl` wire protocol has no authentication**, relying entirely on Unix
  socket file permissions. Multi-tenant hosts need additional access control.
- The experiment harness requires **Python 3.11 or newer** (`tomllib`).
- **`ssctl drain` cannot complete over SSH.** The operator's own SSH connection
  is a skyline_cc flow and will not end while drain waits, so drain always
  reaches its timeout. It is safe — the sysctl is written back to `fallback_cc`
  before the wait begins — but the struct_ops stays attached until that session
  closes.
- The documented one-line installer needs `curl`, which a minimal server image
  may not have. `bootstrap.sh` installs it if missing, but only once it is
  running; the README now shows the prerequisite and a `wget` equivalent.

[Unreleased]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/releases/tag/v0.1.0
