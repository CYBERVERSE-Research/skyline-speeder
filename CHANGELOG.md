# Changelog

Notable changes to Skyline Speeder. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`SKYLINE_ABI_VERSION` is versioned separately from the release number: it tracks
the layout of `bpf/include/skyline_abi.h` and any bump to it is a breaking change
for anyone holding a prebuilt `.bpf.o`.

## [Unreleased]

### Added

- `min_cwnd_packets` (`--min-cwnd-packets`): the floor under M2's BDP-derived
  cwnd target is now configurable instead of the compile-time 4. It defaults to
  4, so an existing `speeder.toml` that does not declare it behaves exactly as
  before. Valid range is `4`..=`max_cwnd_packets`; the M2-off neutral path keeps
  the fixed floor and does not read it. Pacing still sets the send rate.

### Changed

- **Default coefficients.** `config/speeder.toml` and `ssctl set-module-config`'s
  built-in defaults move together to a set tuned by interleaved A/B on a
  production deployment (many concurrent flows, roughly 100-150ms base RTT,
  loss dominated by a full bottleneck rather than random loss):
  `cruise_pacing_gain` 1.1 -> 1.25, `cruise_inflight_gain` 2.0 -> 3.0,
  `startup_plateau_rtts` 3 -> 5, `startup_growth_ratio` 0.25 -> 0.20,
  `loss_inflation_max_ratio` 0.5 -> 0.10, `max_queue_delay_ms` 100 -> 70,
  `max_queue_delay_ratio` 1.0 -> 0.6, `min_rtt_window_s` 10 -> 30,
  `bw_window_rtts` 10 -> 6. An installed `/etc/skyline-speeder/speeder.toml` is
  never overwritten, so an existing host keeps its values until its operator
  edits the file -- but `ssctl set-module-config` sends the *new* built-in
  default for every flag left off the command line. The previous set is kept as
  the "high random loss" preset in `docs/usage.md`; it is what
  `docs/04-performance-report.md` measured, and the experiment matrix stays
  pinned to it. The new set has not been run through that matrix.
- A test now fails if `ssctl set-module-config`'s defaults drift from
  `config/speeder.toml`; they used to be kept in sync by hand.
- **`SKYLINE_ABI_VERSION` 6 -> 7.** `struct skyline_config` grows by
  `min_cwnd_packets` plus an explicit `reserved` tail word (72 -> 80 bytes). A
  `.bpf.o` built against version 6 is rejected by a newer daemon and vice
  versa; rebuild or reinstall both together.

### Fixed

- **The event log could fill `/run`.** `skyline-speederd` appended every BPF
  event to `runtime.events_path` (`/run/skyline-speeder/events.jsonl`) with no
  limit. `/run` is a RAM-backed tmpfs shared with the rest of the host, and on a
  busy host the log filled all of it, at which point Docker could no longer write
  its runc state files; skyline-speeder itself reported nothing. The log is now capped by `runtime.events_max_mib` (default 8): at the
  cap it is renamed to `events.jsonl.1` and a new file started, so it holds at
  most about twice that. The default also applies to an installed
  `speeder.toml` that predates the field. `events_max_mib = 0` turns the log
  off. `infra/snapshot-skyline-events.sh` gained a rotation-aware
  `cursor`/`since` pair, which the experiment harness now uses.
- `install.sh --prebuilt` failed on a stock Debian 13 host with `BPF struct_ops
  support was not detected` (#5). The capability probe shelled out to `bpftool`,
  which a prebuilt install deliberately does not have, and read the missing
  binary as a missing kernel feature. `struct_ops` and `rack_reo_hook` are now
  read in-process from `/sys/kernel/btf/vmlinux`; a BTF file that cannot be
  parsed is reported in `capabilities.notes` instead of looking like an
  unsupported kernel.
- The same probe reported `struct_ops: true` on any host where bpftool ran at
  all: it matched the substring `struct_ops`, which `bpftool feature probe` also
  prints on its "is NOT available" line.
- `install.sh` no longer blames the verifier for every validation failure, and
  prints the full command to rerun rather than referring to a `>/dev/null` that
  a `curl | bash` user never typed.

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
