# Changelog

Notable changes to Skyline Speeder. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`SKYLINE_ABI_VERSION` is versioned separately from the release number: it tracks
the layout of `bpf/include/skyline_abi.h` and any bump to it is a breaking change
for anyone holding a prebuilt `.bpf.o`.

## [Unreleased]

## [0.2.0] - 2026-09-22

### Upgrading from 0.1.0

- **Save the runtime state first.** Overrides made with `ssctl` live only in
  the daemon's memory and are gone after the restart below, and stopping the
  daemon also removes `/run/skyline-speeder`. Keep a copy, for example
  `sudo ssctl status > skyline-status-before.json` (`modules`,
  `module_tuning`, `rack_rto`, `retransmit_dscp`), and afterwards re-run
  whatever `ssctl set-module-config`, `set-rack-rto`/`reset-rack-rto`,
  `set-retransmit-dscp`/`reset-retransmit-dscp`, `enable --modules`/`--all-off`
  or `disable --module` the host depends on. The new daemon starts with
  `[rack_rto]` and `[retransmit_dscp]` off, whatever the file says. Restore
  `module_tuning` with every value from the saved copy, since a
  `set-module-config` that names only some flags fills the rest with the 0.2.0
  defaults; and note that `rack_rto.config` and `retransmit_dscp.config` show
  the file's values even if they were never applied.
- **Stop the daemon before installing, or restart it after.** Re-running
  `install.sh` replaces the binaries and BPF objects but does not restart a
  `skyline-speederd` that is already running, and still reports success. The
  0.1.0 daemon then keeps running, unbounded event log included, and it cannot
  load the new `SKYLINE_ABI_VERSION` 7 objects: whenever it has to -- an
  `ssctl enable` after a drain that completed, or on a host where skyline_cc was
  never attached -- the enable fails and leaves new connections on
  `fallback_cc`. On a host installed with `--no-enable`, re-running
  `install.sh` without that flag therefore fails at the installer's own enable
  step.

  Stopping `skyline-speeder-enable.service` is what puts `fallback_cc` back as
  the system default; stopping the daemon while that unit is not active
  unregisters skyline_cc but leaves the sysctl naming it. So on a host where skyline_cc was attached with a
  bare `ssctl enable` rather than through that unit, run the drain line below
  first, and also before the restart described after it. Over SSH that drain
  always ends at its timeout and exits non-zero, which is harmless: it switches
  the default before it starts waiting.

  ```bash
  sudo ssctl drain --timeout 60       # only after a bare `ssctl enable`
  sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service
  sudo ./install.sh --prebuilt        # or however this host was installed
  ```

  If `install.sh` already ran over a live daemon, restart it:
  `sudo systemctl restart skyline-speederd.service`. systemd restarts
  `skyline-speeder-enable.service` with it only if that unit is active; if it
  is failed or inactive (for instance because the installer's enable step
  failed) and skyline_cc should be attached, start it once the new daemon is
  up: `sudo systemctl start skyline-speeder-enable.service`.

  New connections use `fallback_cc` until the new daemon attaches, and flows
  still open stay on the 0.1.0 instance until they close. After a re-install, a
  still-running old daemon shows up as `(deleted)` in
  `sudo readlink /proc/$(systemctl show -p MainPID --value skyline-speederd)/exe`.
- **Installed coefficients stay as they were.** `install.sh` never overwrites
  `/etc/skyline-speeder/speeder.toml`, so an upgraded host keeps the 0.1.0 set,
  and `ssctl reset-module-config` returns to it. `ssctl set-module-config`'s
  built-in defaults are the new set, so a command that names only some flags
  moves the rest to the new values. To adopt the new defaults, copy them from
  `config/speeder.toml` (listed under Changed) into the installed file and
  restart the daemon.
- `runtime.events_max_mib` needs no edit: a config without it gets the 8 MiB
  cap once the 0.2.0 daemon is running.
- `scripts/bootstrap.sh` fetches `main` unless told otherwise. For a source
  build pass `--ref v0.2.0`; with `--prebuilt` the artifacts come from the
  latest release whatever `--ref` says, and `--release v0.2.0` pins them.
- An experiment guest deployed before this release lacks the new
  `snapshot-skyline-events.sh` actions, and `run_matrix.py` then records an
  empty event log for every case without an error. Redeploy the guest
  (`infra/deploy-guest.sh --confirm-install`), then restart
  `skyline-speederd.service` on it, or reboot it, before running the matrix:
  like `install.sh`, the deploy replaces the files but leaves a running 0.1.0
  daemon in memory.

### Added

- `min_cwnd_packets` (`--min-cwnd-packets`): the floor under M2's BDP-derived
  cwnd target is now configurable instead of the compile-time 4. It defaults to
  4, so an existing `speeder.toml` that does not declare it behaves exactly as
  before. Valid range is `4`..=`max_cwnd_packets`; the M2-off neutral path keeps
  the fixed floor and does not read it. Pacing still sets the send rate.
- `runtime.events_max_mib`: size cap, in MiB, for the BPF event log at
  `runtime.events_path`, default 8. At the cap the log rotates to
  `events.jsonl.1`, so it holds at most about twice that; `0` turns the log off.
  It is read at startup, so changing it takes a daemon restart. Why it exists is
  under Fixed.
- `infra/snapshot-skyline-events.sh cursor` and `since CURSOR`: a position in
  the event log, `INODE:LINES`, that survives one rotation. `run_matrix.py` uses
  them for its per-case event snapshot; `count` and `from` are unchanged.
- A single-ended field comparison of skyline_cc, bbr and tcp-brutal on one
  production path: a README section in both languages, the harness and the
  raw JSONL under `research/experiments/single-ended/`, and the figures in
  `docs/images/`. It is a
  field measurement, not the dual-VM test bed, and it ran the 0.1.0
  coefficients, now the "high random loss" preset, so it says nothing about
  this release's defaults. `README.zh.md` also gains the comparison with the
  alternatives that `README.md` already had.

### Changed

- **Default coefficients.** `config/speeder.toml`, the installed template
  `config/speeder-guest.toml` and `ssctl set-module-config`'s built-in defaults
  move together to a set tuned by interleaved A/B on a
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
  pinned to it. The new set has not been run through that matrix. The
  conservative preset and the tuning examples in `docs/usage.md` are rebased on
  the new set too.
- A test now fails if `ssctl set-module-config`'s defaults drift from
  `config/speeder.toml`; they used to be kept in sync by hand.
- **`SKYLINE_ABI_VERSION` 6 -> 7.** `struct skyline_config` grows by
  `min_cwnd_packets` plus an explicit `reserved` tail word (72 -> 80 bytes). A
  `.bpf.o` built against version 6 is rejected by a newer daemon and vice
  versa. Reinstalling replaces both files but not the daemon already in memory,
  so restart it too; see Upgrading from 0.1.0.

### Fixed

- **The event log could fill `/run`.** `skyline-speederd` appended every BPF
  event to `runtime.events_path` (`/run/skyline-speeder/events.jsonl`) with no
  limit. `/run` is a RAM-backed tmpfs shared with the rest of the host, and on a
  busy host the log filled all of it, at which point Docker could no longer
  write its runc state files; skyline-speeder itself reported nothing. The log
  is now capped by `runtime.events_max_mib` (see Added), and the cap also
  applies to an installed `speeder.toml` that predates the field. On a host
  still running 0.1.0,
  `sudo truncate -c -s 0 /run/skyline-speeder/events.jsonl` frees the space at
  once; `rm` does not, because the daemon keeps the file open.
- `install.sh --prebuilt` failed on a stock Debian 13 host with `BPF struct_ops
  support was not detected` (#5). The capability probe shelled out to `bpftool`,
  which a prebuilt install deliberately does not have, and read the missing
  binary as a missing kernel feature. `struct_ops` and `rack_reo_hook` are now
  read in-process from `/sys/kernel/btf/vmlinux`. A BTF file that cannot be
  parsed is now named in `capabilities.notes`, although validation still stops
  with the same `struct_ops` error.
- The same probe reported `struct_ops: true` on any host where bpftool ran at
  all: it matched the substring `struct_ops`, which `bpftool feature probe` also
  prints on its "is NOT available" line.
- `install.sh` no longer blames the verifier for every validation failure, and
  prints the full command to rerun rather than referring to a `>/dev/null` that
  a `curl | bash` user never typed.

### Known issues

- The prebuilt `skyline-speederd` is built on Ubuntu 24.04 and needs glibc 2.38
  or newer, plus `libelf.so.1` and `libz.so.1`: Debian 13 and Ubuntu 24.04 are
  fine. On older userspace running a 6.12+ kernel, such as Debian 12 with a
  backports kernel, the prebuilt daemon cannot start and `install.sh
  --prebuilt` fails at validation. A source build does not have the glibc
  requirement, but that combination has not been tested.
- Neither binary reports its version, and `install.sh` does not restart a
  running daemon (see Upgrading from 0.1.0).
- When skyline_cc was attached with a bare `ssctl enable`
  (`skyline-speeder-enable.service` not active), stopping `skyline-speederd`
  does not write `fallback_cc` back to `net.ipv4.tcp_congestion_control`;
  `ssctl drain` does. While that unit is active, stopping or restarting the
  daemon stops the unit first, and its `ExecStop` does the write.
- The known issues listed under 0.1.0 below still apply.

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

[Unreleased]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/releases/tag/v0.1.0
