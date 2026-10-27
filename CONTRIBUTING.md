# Contributing to Skyline Speeder

Thanks for looking. Before you write code, three things about this repository
that are different from an ordinary userspace project:

1. **The BPF verifier is a hard gate.** A program that does not verify does not
   load. There is no partially-working state, no warning, no degraded mode.
2. **BPF and userspace share one ABI** (`bpf/include/skyline_abi.h`). Changing
   the layout on one side only does not produce an error — it produces silently
   misinterpreted data.
3. **The blast radius is the whole machine.** A defect here is anomalous TCP
   behaviour for every new connection on the host, not one failed request.

If that is acceptable to you, read on.

## Build and check

```bash
make bpf                          # generate vmlinux.h, compile three CO-RE objects
cargo build --workspace --release
make check                        # bash -n on the scripts, the experiment-harness Python tests,
                                  # cargo fmt --check, cargo check -- no cargo test
make test                         # the experiment-harness Python tests, then cargo test --workspace
```

`make bpf` reads type information from **this machine's** `/sys/kernel/btf/vmlinux`
by default. Point it at another kernel's BTF with `make VMLINUX_BTF=<path> bpf`, or
hand it a ready-made header with `make PREBUILT_VMLINUX_H=<path> bpf` -- that path
needs neither bpftool nor this machine's BTF, and it is how releases are built.
The header only has to define the types the code uses. It need not come from the
kernel the objects will run on, but it must not be newer than that kernel: it is
generated with `preserve_access_index`, so every field access carries a CO-RE
relocation that libbpf fixes against the running kernel's BTF at load time, and
CO-RE cannot supply a field the running kernel lacks -- objects built against a
newer kernel's header can fail to load on 6.12, which is why releases build
against the 6.12 reference header. What has been verified is a 6.12 header:
objects built against it under 6.12.63 load on 6.19.14, and built against it
under 6.19.14 load on 6.12.63;
`infra/kernel/core-portability.sh` reproduces this. CO-RE cannot fix a field
that was renamed or removed either, so after a data-plane change, rerun that
script before relying on it. `bpf/include/vmlinux.h` is a build artefact and is
never committed.

A vmlinux.h generated this way is compiled with `-DBPF_NO_KFUNC_PROTOTYPES`: from
7.5 on, bpftool writes a prototype for every kfunc the running kernel exports, and
one of them can disagree with the distribution's `bpf_helpers.h` (on Fedora 43,
kernel 6.17 exports `bpf_stream_vprintk` with five arguments and libbpf 1.6.1
declares four). So **declare every kfunc a BPF source calls in that source**, as
`skyline_cc.bpf.c` does, rather than relying on vmlinux.h. The
`PREBUILT_VMLINUX_H` path a release builds with does not set the macro, and the
released objects stay byte for byte the same.

`make check` and `make test` run the experiment-harness unit tests with
`$(PYTHON)` (default `.venv/bin/python`). They need **Python 3.11 or newer**
(`research/experiments/matrix_lib.py` imports `tomllib`; on 3.10 and earlier the
tests fail at import) and nothing outside the standard library. `make venv`
builds `.venv` through pyenv from the *system* `python3`, so it only helps where
that is 3.11 or newer. Anywhere else, point the targets at a 3.11+ interpreter:

```bash
make check PYTHON=/path/to/python3.11
```

### Required before every commit

Push all three objects through the kernel verifier, attaching nothing and writing
no sysctl:

```bash
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

Paste the result into your pull request. A change that has not been through the
verifier is not reviewable, and CI cannot do this for you: it compiles the
objects but no job loads them.

## Hard invariants

Breaking any of these is a defect regardless of what else the change does.

| Identifier | Location | Constraint |
|---|---|---|
| The algorithm name `"skyline_cc"` | `.name` of both struct_ops maps in `bpf/skyline_cc.bpf.c`; the `TCP_CONGESTION` setsockopt in `bpf/skyline_policy.bpf.c`; `skyline_runs_skyline_cc()` in `bpf/skyline_tc.bpf.c`; `SKYLINE_CC_NAME` in `crates/skyline-common`; the literals in `infra/boot-enable.sh`, `infra/boot-disable.sh`, `infra/apply-guest-profile.sh` and `install.sh` | Registration name, **≤ 15 characters** (`TCP_CA_NAME_MAX` is 16 including NUL), byte-identical everywhere. The daemon's sysctl write fails loudly on a mismatch; the policy and TC programs do not -- the setsockopt's error is ignored, and first-flight redundancy is left copying SYN-ACKs only. The struct_ops map names are a separate namespace |
| `SKYLINE_ABI_VERSION` and the three other ABI versions | `bpf/include/skyline_abi.h` and `crates/skyline-common/src/lib.rs` | A layout change bumps the version that covers the struct, on both sides: `SKYLINE_ABI_VERSION` for `struct skyline_config`, `skyline_flow_state` and `skyline_metrics`; `SKYLINE_RTO_TUNING_ABI_VERSION`, `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` and `SKYLINE_REDUNDANCY_ABI_VERSION` for their own structs (`docs/02-interface-reference.md` section 5). Since 0.5.2 `skyline-speederd` reads the version a `skyline_cc.bpf.o` was built for (`skyline_abi_version` in its `.rodata.abi` section) before loading it, and refuses an object of another version, or one without the section, with both versions in the error (`ssctl enable`, `--validate-only --verify-bpf` and the installer's verifier step). An older daemon that does not check can still load a newer object: then no flow can adopt a configuration, each grows its window as `tcp_reno_cong_avoid()` does (before 0.5.2 it stayed at the kernel's initial window, after an RTO at what `tcp_enter_loss()` left) and nothing sets its pacing. `cong_control` has replaced the kernel's own growth, so this is not a fallback to the kernel. There is no error and no counter: `active_flows` grows while `metrics.ack_events` does not. A `struct skyline_config` of another size fails loudly instead, when the daemon writes the slot. A size mismatch also leaves the `struct skyline_metrics` counters at 0. On a mismatch of their own versions, DSCP marking and first-flight redundancy skip their work and count `abi_mismatch`, and since 0.5.2 `skyline_policy` treats a dynamic-RTO slot as no configuration. The bump is what makes a half-updated pair stop instead of misreading the struct |
| `cong_control` 4-argument signature | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)` — exists only on kernel >= 6.10 |
| Two struct_ops maps, `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | Both set **the same callbacks** and differ only in where CA_EVENT_TX_START arrives (`cwnd_event`, or `cwnd_event_tx_start` from Linux 7.1); the daemon creates one of them, by the kernel's BTF. **A callback added to one goes into the other too**, or the behaviour silently depends on the kernel version |
| Install paths `/opt`, `/etc`, `/run/skyline-speeder` | config, units (the two OpenRC scripts in `packaging/openrc/` included), scripts | All three must agree |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **Must be the parent directory of `socket_path`** (and of `events_path`, `state_path`): the unit sets `ProtectSystem=strict`, under which this is the one directory in `/run` the daemon can write |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC's `RuntimeDirectory=`: **must be the parent directory of `socket_path`** (and of `events_path`, `state_path`), where the daemon opens its event log first |
| `StateDirectory=skyline-speeder`, `checkpath -d /var/lib/skyline-speeder` | `packaging/skyline-speederd.service`, `packaging/openrc/skyline-speederd` | **Must be the parent directory of `DAEMON_REPORTS_DIR`** (`/var/lib/skyline-speeder/reports`, `crates/skyline-common`), which `ssctl` and `install.sh` (`FEEDBACK_DAEMON_REPORTS`, and `--uninstall`, which removes it) read too: under `ProtectSystem=strict` it is the one directory under `/var/lib` the daemon can write, and a mismatch only costs a line on stderr when a report cannot be written -- the error report is silently lost |
| Error reports | `install.sh` ("error reports"), `crates/ssctl/src/feedback.rs`, `crates/skyline-speederd/src/reports.rs` | **Sent only on a `y` typed on a terminal, or with `SKYLINE_FEEDBACK=yes`**: never with no terminal, never by skyline-speederd itself, which only records. Everything shown or sent is scrubbed first, by the same list in `feedback_scrub` and `scrub`. The DSN is in the tree only sealed, in two copies that `infra/seal-feedback-dsn.py` writes together (`FEEDBACK_SEALED` in `install.sh`, `crates/ssctl/src/feedback-dsn.sealed`) and `cargo test -p ssctl` compares; a plain DSN or a Sentry auth token never goes into the tree |

> The `RuntimeDirectory` / `socket_path` coupling is a hole this project has
> already fallen into once: a bulk rename changed `RuntimeDirectory` to
> `skyline` while the config still said `/run/skyline-speeder/`. The daemon
> started fine then and the control socket simply never appeared. Today's daemon
> creates those directories itself and runs under `ProtectSystem=strict`, so, by
> the unit and the code, the same mismatch now makes it fail at start and be
> restarted over and over: the invariant stands, the symptom changed. If you
> touch either side, check the other.

### Configuration-plane semantics

- `set-module-config`, `set-rack-rto`, `set-retransmit-dscp` and
  `set-redundancy` are **absolute overwrite**: every call carries the complete
  field set, never a delta. The matching `reset-*` commands restore config-file
  defaults.
- Configuration switching uses **double slots, a sequence number and a per-flow
  copy**. The daemon writes configuration N into `config_slots[N & 1]` and only
  then sets `config_seq` to N. A flow copies the slot `config_seq` names when it
  starts: the ACK that completes the handshake reaches `cong_control` before
  `init`, and `init` copies again and starts over. A flow that has no copy yet
  tries again on every ACK; one that has a copy copies again only when a new
  round starts and `config_seq` has moved. A round is not one RTT:
  `skyline_new_round()` ends it when `prior_delivered` reaches the round's
  `delivered + packets_out`, about two RTTs for a flow that keeps its window
  full (about one right after a start or a switch). A copy taken while
  `config_seq` moved is discarded, and every ACK reads only the flow's own
  copy. So no flow reads a mix of old and new values within a round, however
  quickly updates follow each other. **Any change to the config delivery path must keep
  both halves** -- slot first, sequence number second; BPF reads only the copy --
  or you get torn reads (0.4.x had neither, and two updates within one RTT
  rewrote a slot live flows were reading).

## Three silent failure modes

These produce **no error output whatsoever** — they just do not take effect. Be
careful around the code that touches them.

1. **cgroup not migrated.** `skyline_policy` attaches to
   `/sys/fs/cgroup/skyline-speeder`; a process outside it never traverses that
   BPF program. Symptom: `rack_rto.stats.established_cb` ("connections seen" in
   `ssctl flows`) stays at 0. Decision table in `DEPLOY.md` section 7.
2. **`enable` never run.** `skyline-speederd` does **not** attach `skyline_cc`
   on startup — `ssctl enable` is required. `skyline-speeder-enable.service`
   exists precisely to remove this silent failure. **Do not delete it on the
   grounds that `install-guest.sh` does not enable it** — that is deliberate:
   attaching changes congestion control for every new connection on the host,
   which is an operational decision, made explicitly by `install.sh`.

   `ssctl enable` is host-wide by itself: after a successful struct_ops attach
   it writes `net.ipv4.tcp_congestion_control = skyline_cc`. **The order of the
   sysctl writes is an invariant**: `enable` writes after the attach (the kernel
   rejects a name that is not registered), and `drain` writes `fallback_cc`
   back before it unregisters (otherwise a struct_ops would be unregistered
   while it is still the default algorithm). Inside skyline-speederd every write
   of that sysctl goes through `set_default_congestion_control` in
   `crates/skyline-speederd/src/main.rs` (`BpfRuntime::load`, `enable`, the
   guard, `drain`), and from `enable` to `drain` the daemon is its only writer.
   Outside that window three others write it, each for a reason:
   `infra/boot-disable.sh` drains first and writes the sysctl only if the
   default is still `skyline_cc` afterwards -- the fallback for when the daemon
   is already gone (written before the drain, the still-armed guard would put
   `skyline_cc` back); the experiment harness's `infra/apply-guest-profile.sh`
   writes the algorithm it wants after `drain --timeout 0`; and
   `install.sh --uninstall` writes bbr (or, with `--restore-pre-install`, the
   pre-install value) after draining and removing the daemon.
   `infra/boot-enable.sh` writes no sysctl.

   **Being switched back after the attach is just as silent.** A "one-click
   BBR" script's `bbr` line in `/etc/sysctl.d`, re-applied by
   `sysctl --system`, moves every new connection off `skyline_cc` without an
   error. So between `enable` and `drain` the guard
   (`crates/skyline-speederd/src/guard.rs`, config `[guard]`) keeps three
   things in place: the default congestion control and, with
   `[guard] qdisc = true` (the default), `net.core.default_qdisc = fq` and the
   root qdisc of `runtime.tc_interface` (`fq`, or `mq` over `fq` on a
   multi-queue NIC; when that is a VLAN, bond or bridge whose root is
   `noqueue`, the NICs with a `device` link under it instead, never a tap or
   veth). From `enable` to `drain` the daemon is the only writer of all three
   (after a drain, `apply-guest-profile.sh` and `install.sh --uninstall` set
   qdiscs of their own). `boot-enable.sh` used to write `default_qdisc=fq` once
   at boot and no longer does -- do not add it back: it cannot change a NIC's
   existing root qdisc, and two writers of one setting drift apart. Related
   invariants:
   - `enable` arms the guard only after it has written `skyline_cc`. `drain`
     disarms it first and writes `fallback_cc` **while still holding the
     guard's lock**; otherwise a periodic check could write `skyline_cc` back
     after the drain. A drain that times out leaves it disarmed too: the
     experiment matrix drains with `--timeout 0` and then installs its own
     qdisc, which the guard must not fight.
   - On stop, the guard thread is stopped and joined before `BpfRuntime` drops
     and unregisters the struct_ops (`impl Drop for Daemon`); otherwise an armed
     pass would write a `skyline_cc` the kernel no longer knows.
   - **Stopping the daemon writes no sysctl.** The maintainer explicitly does
     not want `fallback_cc` written on stop; `drain` and the enable unit's
     `ExecStop` do that.
   - The guard replaces only qdisc kinds with no configuration worth keeping
     (`pfifo_fast`, `fq_codel`, an unshaped `cake`, `fq_pie`, ...). Classful,
     shaping and offload kinds (`htb`, `tbf`, `netem`, `mqprio`, ...), a
     `cake` with a bandwidth or `autorate-ingress`, `noqueue` and unknown kinds
     are left alone: replacing them would silently destroy an operator's
     shaping.
3. **`tc_interface` pointing at the wrong NIC.** The default in
   `config/speeder-guest.toml`, `data0`, is the test bed's interface name and
   will almost never match a real host. `install.sh` rewrites it on every run,
   install or upgrade, while the file still holds the template line
   `tc_interface = "data0"`: the first Ethernet device a default route (IPv4,
   then IPv6) leaves through takes its place. A value an operator has changed
   is never touched. When the default route only goes through a tunnel
   (WireGuard, tun, ...) it keeps the placeholder and warns: skyline_tc is only
   attached to an Ethernet device. The guard keeps the root qdisc of that same NIC on
   `fq`: pointed at the wrong one, the real egress NIC keeps whatever qdisc it
   had, and the other NIC gets `fq` instead.

## Naming landmine

**`PRR-SSRB`** in `bpf/skyline_cc.bpf.c` is RFC 6937 standard terminology (Slow
Start Reduction Bound). It is **not** a leftover of this project's old name and
must not be renamed. Protect it with word boundaries during any bulk rename.

## The guest config constraint

`[rack_tuning]` in `config/speeder-guest.toml` **must stay commented out in its
entirety**. That config is driven per-case by the experiment matrix
(`infra/apply-guest-profile.sh`), which controls `tcp_recovery`,
`tcp_reordering` and `tcp_early_retrans` exactly. If `skyline-speederd` takes
ownership of those global sysctls here, a daemon restart silently overwrites the
per-case values with defaults.

`crates/skyline-common/src/lib.rs` has a `guest_config_never_owns_global_sysctls`
test guarding this. **Do not weaken or delete it to make a case pass.**

## Performance-claim discipline

**Do not put a performance number in the documentation without test data behind
it.** Formal performance conclusions may only come from a two-VM test bed that
passes `infra/preflight.sh formal` (by default ≥ 16 logical CPUs, ≥ 32 GiB RAM,
≥ 120 GiB free disk, a writable `/dev/kvm` and vmx/svm; setting one up is in
`research/experiments/README.md`). A machine below
those thresholds is good for code review, verifier checks and short smoke runs,
and **must not be used to draw performance conclusions**.

## Where things are documented

| Document | Reader and content |
|---|---|
| `README.md` / `README.zh.md` | Users, in the order a newcomer needs it: what it is, the kernel requirement, quick start, how it works and how the three algorithms differ, configuration (what a default install runs, a subsection per feature, and what every line of `ssctl status` and `ssctl flows` means), layout, building, documentation, measured performance, contributing, license. The two say the same thing |
| `DEPLOY.md` | The one deployment and operations manual, written for automation agents and followed by people: prerequisites, every installer step, verification gates, the cgroup, the guard, upgrades, removal, troubleshooting |
| `docs/usage.md` | Newcomers: every switch and parameter, four presets, dynamic RTO, DSCP, finding the cause of a stalled connection |
| `docs/02-interface-reference.md` | `ssctl` commands, the wire protocol, status fields and counters, ABI versions, configuration fields, event codes, guard rules |
| `docs/03-design.md` | How each module works, the trade-offs, open questions |
| `docs/04-performance-report.md` | The two-VM test bed's data, what it does and does not show |
| `research/experiments/README.md` | The experiment harness and how to reproduce a run |
| `research/experiments/single-ended/` | The one-path field comparison of bbr, tcp-brutal and skyline_cc the README's performance section cites; not test-bed data |
| `CLAUDE.md` and the `CLAUDE.md` in `bpf/`, `crates/`, `config/`, `packaging/`, `infra/` and `docs/` | The same conventions, for AI coding agents. The root file is loaded in every session and holds the repository-wide rules and an index; each directory's file is loaded when an agent reads or edits a file there and holds that directory's details |

## Documentation you must update with your change

| Change | Also update |
|---|---|
| ABI struct | `skyline_abi.h` + `crates/skyline-common` + the version that covers it (`SKYLINE_ABI_VERSION`, `SKYLINE_RTO_TUNING_ABI_VERSION`, `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` or `SKYLINE_REDUNDANCY_ABI_VERSION`) + `docs/02-interface-reference.md` section 5 |
| `ssctl` command or field | `docs/02-interface-reference.md` |
| Config field | `config/*.toml` + `docs/02-interface-reference.md` section 6 |
| Install flow | `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh` (and both READMEs and `docs/usage.md` where they describe it) |
| Algorithm behaviour | `docs/03-design.md`; performance claims need data in `docs/04-performance-report.md` |
| Anything user-facing in the README | Both `README.md` and `README.zh.md` |
| A new feature, a changed default, or report wording in `crates/ssctl/src/view.rs` | Both READMEs' table of what a default install runs, the feature's subsection, and the line-by-line key to `ssctl status` and `ssctl flows` (`cargo test -p ssctl -- --nocapture renders_for_review` prints both reports) |
| A hard invariant or the contribution process | This file, the `CLAUDE.md` of the directory the invariant lives in (the same conventions, for AI coding agents; the root `CLAUDE.md` keeps a one-line summary of each) and the Invariants list in `.github/PULL_REQUEST_TEMPLATE.md` |
| A release (`v*` tag) | `[workspace.package] version` in `Cargo.toml` and the three workspace crates in `Cargo.lock`; `release.yml` refuses a tag that is not exactly `v<version>`. There is no changelog to update: the repository keeps none |

## Style

- **Language:** everything in this repository is English except `README.zh.md`, the Chinese twin of
  `README.md`; the two READMEs change together.
- **Rust:** `cargo fmt` is mandatory; `make check` enforces it.
- **BPF C:** four-space indentation.
- **Shell:** `set -euo pipefail`; the POSIX sh scripts (`scripts/bootstrap.sh`,
  `infra/boot-enable.sh`) use `set -eu`, and `infra/boot-disable.sh`
  deliberately only `set -u`, so that a stop never fails half way.
- **Comments explain *why*.** A large share of the comments in this repository
  record verifier limitations, kernel behaviour and holes already fallen into.
  Those comments are worth more than the code around them. Do not delete them
  for brevity.

## Cutting a release

Release artifacts are built by `.github/workflows/release.yml` when a `v*` tag
is pushed: one per C library, the glibc build and a musl build for Alpine
(`skyline-speeder-<tag>-<arch>-musl.tar.gz`, built in an Alpine 3.21
container by `infra/build-musl.sh`, which needs docker and which CI runs on
every pull request too), carrying the same BPF objects. Before building anything it checks that the tag is exactly `v`
followed by the workspace version in `Cargo.toml`; otherwise it stops. So bump
`[workspace.package] version` in `Cargo.toml` and the three workspace crates'
entries in `Cargo.lock` (every build runs with `--locked`) before tagging: the
version is what `--version`, `ssctl status` and the installer's upgrade summary
report. The repository keeps no changelog: do not add `CHANGELOG.md` or a
release-notes file; a release's notes are its tag message and the release
page. The build uses `--locked`, as CI does. It
compiles the BPF objects against a **pinned reference header**
generated from the oldest supported kernel, not against whatever the runner
boots — CO-RE fixes field offsets, but it cannot conjure a field that does not
exist, so an object built against a 6.17 header can reference something absent
on 6.12 and fail to load there.

That header is not in the repository (`vmlinux.h` stays a build artefact). It is
published once and pinned by digest:

```bash
# on a host running the oldest supported kernel (6.12 LTS)
infra/kernel/make-reference-vmlinux.sh
# publish the .h.gz it produces, then:
gh variable set SKYLINE_REFERENCE_VMLINUX_URL    --body "<asset download URL>"
gh variable set SKYLINE_REFERENCE_VMLINUX_SHA256 --body "<digest it printed>"
```

The workflow **refuses to build** if those two variables are unset. That is
deliberate: falling back to the runner's own BTF would silently ship objects
built against a newer kernel, and the failure would land on a 6.12 user's
machine rather than in CI.

A release that changes the data plane is checked before it is tagged: build the
objects the way `release.yml` does, in a fresh checkout (the Makefile copies a
`PREBUILT_VMLINUX_H` by mtime and keeps a `vmlinux.h` that is already there),
freeze them on one kernel and verify that they load on another. The same script
checks an unpacked artifact after the release, with the artifact's own daemon:

```bash
SKYLINE_BPF_DIR=<unpacked artifact>/bpf infra/kernel/core-portability.sh freeze
# get to the other kernel
SKYLINE_SPEEDERD=<unpacked artifact>/bin/skyline-speederd infra/kernel/core-portability.sh verify
```

For an artifact, the artifact's `MANIFEST` (`reference_vmlinux_sha256`, the
digest of the `.h.gz` release.yml fetched) is what says which header the objects
were built against.

## Never commit

See `.gitignore`. In particular:

- `bpf/include/vmlinux.h` (machine-specific build artefact)
- `target/`, `build/`, `*.bpf.o`
- Rendered `user-data` (contains a real operator SSH public key) — only
  `user-data.template` is committed
- `known_hosts`, keys, filled-in lab inventories
- Any absolute path of the form `/home/<username>`

## Pull requests

- Say whether the change needs a new prebuilt release, and why (the "Prebuilt
  release" section of the PR template). The one-line install installs the
  latest release's prebuilt artifact, not `main` (a host that cannot run the
  published binaries -- not x86_64, glibc older than 2.38, musl older than
  1.2.5 -- builds the fetched tree, `main` by default, from source), so a change
  that lives in the artifact reaches those users only with the next release --
  and nothing fails in the meantime; it just is not there:

  | The change touches | Release needed? | Why |
  |---|---|---|
  | `bpf/`, `crates/`, `config/speeder-guest.toml`, `packaging/` (the systemd units and the OpenRC scripts), the `infra/*.sh` that `release.yml` ships | **yes** | it is in the artifact (the OpenRC scripts from v0.4.1 on: Alpine installs the musl artifact, and the installer prefers the two scripts in it) |
  | `install.sh`, `scripts/bootstrap.sh` | usually not | the one-line install always runs the installer from `main`; but `bootstrap.sh --uninstall`, and an upgrade run from the tree an install keeps, use the installer kept in `/usr/local/src/skyline-speeder`, not `main`'s. Release first when the new installer needs something the released artifact does not have yet |
  | docs, the experiment harness, CI, anything only a source build uses | no | not in the artifact |

  A release can wait and bundle several PRs, but the PR has to say that it goes
  out with the next one. See "Cutting a release" above.
- One logical change per PR.
- Include the `--validate-only --verify-bpf` output, and say which kernel version
  you ran it on.
- If the change touches the data path, say what you measured and on what. "It
  feels faster" is not a result; see the performance-claim discipline above.
- If it touches an invariant in the table above, say explicitly how you checked
  the other side.

## License

This repository is **GPL-2.0-only** throughout. See `LICENSE` and `NOTICE`.

Every BPF and Rust source carries an `SPDX-License-Identifier` (the scripts,
configuration and documents do not yet). Keep it when you edit a file, and add
one to any file you create.

By contributing you agree that your contribution is licensed under GPL-2.0-only,
and that you have the right to grant this — in particular, that you are not
pasting in code from a project whose license you have not checked.

If you are porting an algorithm from the Linux kernel or any other GPL codebase,
say so in the pull request and cite the source file. That determines which
license the result has to carry, and it is much cheaper to establish before the
code lands than after. See `NOTICE` for how the existing cases are recorded.

"Skyline Speeder" and "CYBERVERSE" are trademarks of CYBERVERSE LLC. The GPL
grant over the code does not grant any right to use them; a fork is free to use
the code and must not use these names to identify itself.
