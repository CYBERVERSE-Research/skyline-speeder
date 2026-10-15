# CLAUDE.md

Conventions for Claude Code and other AI coding agents. This file is loaded in every session and holds only what
applies to the whole repository, plus an index. Each directory's details live in that directory's own `CLAUDE.md`,
which Claude Code loads once it reads or edits a file there. Read this file before changing the repository.
**Before a change that spans directories** (a bulk rename, a new writer of a sysctl, an ABI change), read the
`CLAUDE.md` of every directory it touches: a directory's rules do not show up until you work in it.

| File | Details |
|---|---|
| `bpf/CLAUDE.md` | Where vmlinux.h comes from and why kfuncs are declared in the source; CO-RE portability; the full constraints on the algorithm name, the ABI versions, the `cong_control` signature and the two struct_ops maps |
| `crates/CLAUDE.md` | The order and the writers of the global sysctls, the guard's invariants, configuration-plane semantics (absolute overwrite; double slots, a sequence number and a per-flow copy) |
| `config/CLAUDE.md` | Why `[rack_tuning]` in the guest config stays commented out; the `tc_interface` default and when `install.sh` rewrites it |
| `packaging/CLAUDE.md` | Install paths; how `RuntimeDirectory=` and `checkpath` are tied to `socket_path` |
| `infra/CLAUDE.md` | The boot scripts and the global sysctls, the scripts a release ships, the CO-RE portability script |
| `docs/CLAUDE.md` | The documentation map: who each document is for and what it holds |

A detail lives only in its directory's `CLAUDE.md`; this file keeps a one-line summary and a pointer. If the two
disagree, the directory's file wins -- then fix the summary here. `CONTRIBUTING.md` carries the same conventions for
human contributors.

## What this project is

Skyline Speeder is three eBPF programs plus a Rust userspace control plane, and `skyline_cc` is a **kernel congestion
control algorithm registered through struct_ops**. That means:

- The BPF verifier is a hard gate: a program that does not verify does not load, and there is no state in between;
- BPF and userspace share one ABI (`bpf/include/skyline_abi.h`): **changing a struct's layout on one side only makes
  the data silently misread**;
- A defect costs anomalous TCP behaviour across the whole machine, not one failed request.

## Build and check

```bash
make bpf                          # generate vmlinux.h and compile the three CO-RE objects
cargo build --workspace --release
infra/build-musl.sh               # the musl (Alpine) control plane, built in an Alpine 3.21 container; releases and CI use it
make check                        # bash -n on the scripts, the experiment-harness Python tests, cargo fmt --check, cargo check; no cargo test
make test                         # the experiment-harness Python tests + cargo test --workspace
# The harness tests need Python >= 3.11 (tomllib) and only the standard library. make venv builds .venv through
# pyenv from the system python3, so where that is older the venv cannot run them either; name a >= 3.11 interpreter:
# make check PYTHON=/path/to/python3.11

# Push the objects through the kernel verifier without attaching or writing a sysctl -- required before every commit
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

`bpf/include/vmlinux.h` is a build product and is **never committed**. Where `make bpf` takes its BTF from,
`PREBUILT_VMLINUX_H`, why a kfunc must be declared in the source that calls it, and why vmlinux.h must not be newer
than the running kernel (rerun `infra/kernel/core-portability.sh` after a data-plane change): `bpf/CLAUDE.md`.

## Hard invariants

The "Constraint" column is a summary. The full constraint, how a violation shows and why are in the file the
"Details" column names; read it before changing any of these.

| Identifier | Location | Constraint | Details |
|---|---|---|---|
| The algorithm name `"skyline_cc"` | the three programs in `bpf/`, `SKYLINE_CC_NAME` in `crates/skyline-common`, and the literals in three `infra/` scripts and `install.sh` | Registration name, **≤ 15 characters** (`TCP_CA_NAME_MAX` is 16 including NUL), byte-identical everywhere; a mismatch in the policy or TC program fails silently | `bpf/CLAUDE.md` |
| `SKYLINE_ABI_VERSION` and the three other ABI versions | `bpf/include/skyline_abi.h` and `crates/skyline-common/src/lib.rs` | A layout change bumps the version that covers the struct, on both sides. Since 0.5.2 the daemon refuses a `skyline_cc.bpf.o` built for another ABI; an older daemon that does not check loads a newer object without an error, and no flow gets a configuration | `bpf/CLAUDE.md` |
| `cong_control` 4-argument signature | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`, which exists only on kernel >= 6.10 | `bpf/CLAUDE.md` |
| Two struct_ops maps, `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | Both set **the same callbacks**: **a callback added to one goes into the other too**, or behaviour silently depends on the kernel version | `bpf/CLAUDE.md` |
| Install paths `/opt`, `/etc`, `/run/skyline-speeder` | config, units (the two OpenRC scripts in `packaging/openrc/` included), scripts | All three must agree | `packaging/CLAUDE.md` |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **Must be the parent directory of `socket_path`** (and of `events_path`, `state_path`) | `packaging/CLAUDE.md` |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC's `RuntimeDirectory=`: must be the parent directory of the same paths | `packaging/CLAUDE.md` |

## Naming landmine

**`PRR-SSRB`** in `bpf/skyline_cc.bpf.c` is RFC 6937 standard terminology (Slow Start Reduction Bound). It is **not** a
leftover of this project's old name SSR and must not be renamed. Protect it with word boundaries during any bulk
rename.

## Silent failure points (this project's biggest source of risk)

These three produce **no error at all** -- they just do not take effect. Be careful around the code that touches them:

1. **cgroup not migrated.** `skyline_policy` attaches to `/sys/fs/cgroup/skyline-speeder`; a process outside it never
   traverses that BPF program. Symptom: `rack_rto.stats.established_cb` ("connections seen" in `ssctl flows`) stays
   at 0. Decision table in `DEPLOY.md` section 7.

2. **`enable` never run.** `skyline-speederd` does **not** attach `skyline_cc` on startup -- `ssctl enable` is
   required. `skyline-speeder-enable.service` exists precisely to remove this silent failure. **Do not delete it on
   the grounds that `install-guest.sh` does not enable it** -- that is deliberate: attaching changes congestion
   control for every new connection on the host, which is an operational decision, made explicitly by `install.sh`.

   `ssctl enable` is host-wide by itself, and **the order and the writers of the sysctl writes are an invariant**.
   Being switched back after the attach (by a "one-click BBR" script) is just as silent, so from `enable` to `drain`
   the guard keeps the default congestion control, `net.core.default_qdisc` and the egress NIC's root qdisc in place.
   **Stopping the daemon writes no sysctl**, and `infra/boot-enable.sh` writes none either -- do not add one. The full
   order, each writer's preconditions and the guard's invariants: `crates/CLAUDE.md`.

3. **`tc_interface` pointing at the wrong NIC.** The default in `config/speeder-guest.toml`, `data0`, is the test bed's
   interface name and will almost never match a real host. Pointed at the wrong NIC, `skyline_tc` misses the real
   egress NIC and the guard puts `fq` on the other one. When `install.sh` rewrites it: `config/CLAUDE.md`.

## Configuration plane and the guest config

- Every `set-*` command is an **absolute overwrite**. A configuration switch writes **the slot first and the sequence
  number second**, and BPF reads only each flow's own copy. The full rules are in `crates/CLAUDE.md`,
  "Configuration-plane semantics"; any change to the config delivery path must keep them.
- `[rack_tuning]` in `config/speeder-guest.toml` **must stay commented out in its entirety**, and the
  `guest_config_never_owns_global_sysctls` test guards this -- **do not weaken or delete it to make a case pass**.
  Why: `config/CLAUDE.md`.

## Never commit

See `.gitignore`. In particular:

- `bpf/include/vmlinux.h` (a machine-specific build product)
- `target/`, `build/`, `*.bpf.o`
- a rendered `user-data` (it carries an operator's real SSH public key); commit only `user-data.template`
- `known_hosts`, keys, filled-in lab inventories
- any absolute path of the form `/home/<user>`
- agent files that belong to one machine or one person: `.claude/` and `CLAUDE.local.md` at any level. This file and
  the directory `CLAUDE.md` files are public: they are written in English, and host addresses, account names, tokens
  and personal paths stay out of them

## Documentation sync

Who each document is for and what it holds: the documentation map in `docs/CLAUDE.md`.

| Change | Also update |
|---|---|
| ABI struct | `skyline_abi.h` + `crates/skyline-common` + the version that covers it (`SKYLINE_ABI_VERSION`, `SKYLINE_RTO_TUNING_ABI_VERSION`, `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` or `SKYLINE_REDUNDANCY_ABI_VERSION`, bumped on both sides) + `docs/02-interface-reference.md` section 5 |
| `ssctl` command or field | `docs/02-interface-reference.md` |
| Config field | `config/*.toml` + `docs/02-interface-reference.md` section 6 |
| Install flow | `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh` (and both READMEs and `docs/usage.md` where they describe it) |
| Anything user-facing in the README | **Both `README.md` (English) and `README.zh.md` (Chinese), together** |
| A new feature, a changed default, or report wording in `ssctl status` / `ssctl flows` (`crates/ssctl/src/view.rs`) | Both READMEs' table of what a default install runs, the feature's subsection, and the two tables that explain both reports line by line (`cargo test -p ssctl -- --nocapture renders_for_review` prints both reports) |
| A hard invariant or the contribution process | The `CLAUDE.md` of the directory the invariant lives in (this file keeps only its summary) + `CONTRIBUTING.md` + the Invariants list in `.github/PULL_REQUEST_TEMPLATE.md` |
| Algorithm behaviour | `docs/03-design.md`; performance claims need data in `docs/04-performance-report.md` |
| A release (`v*` tag) | `[workspace.package] version` in `Cargo.toml` + the three workspace crates' versions in `Cargo.lock` (CI, releases and the musl build all use `--locked`, so a mismatch fails the build) + a `## [<version>] - <date>` section in `CHANGELOG.md` (there is no `[Unreleased]` section; write the version section directly) and its compare link at the bottom -- `release.yml` refuses a tag that is not exactly `v<version>` or has no such section |

## Prebuilt releases: every PR answers "does this need a release?"

The one-line install (`scripts/bootstrap.sh` → `install.sh`, no arguments) installs **the latest GitHub release's
prebuilt artifact**, not `main`. A change merged into `main` does **not** reach a default install until the next
release -- and that is silent too: no error, it just has not happened. The exception is a host that cannot run the
artifact (not x86_64, glibc older than 2.38, musl older than 1.2.5, or a musl host whose latest release has no musl
build): the installer builds the source tree the bootstrap script fetched instead, which is `main` by default
(`prebuilt_blocker` in `install.sh`, `DEPLOY.md` section 2.1).

So **every PR** states in its "Prebuilt release" section (see the PR template) whether a prebuilt release must follow,
and why:

| The change is in | Release? | Why |
|---|---|---|
| `bpf/`, `crates/`, `config/speeder-guest.toml`, `packaging/` (systemd units and OpenRC scripts), the `infra/*.sh` that `release.yml` ships | **Yes** | They are in the artifact; without a release a default install never gets them (the OpenRC scripts have shipped since v0.4.1; Alpine installs the musl artifact by default, and the installer prefers the two scripts in the artifact) |
| `install.sh`, `scripts/bootstrap.sh` | Usually not | The one-line install fetches the installer from `main` (the bootstrap script's `--ref`) every time, so a merge takes effect for every later one-line install. A copy already on a host does not update itself: `bootstrap.sh --uninstall` runs the tree's own `install.sh` when `/usr/local/src/skyline-speeder` already holds a source tree, and so does upgrading or uninstalling with that tree's `install.sh` as the install's closing hints say. The exception is a new installer that depends on something no artifact carries yet -- release that first |
| Documentation, the experiment harness, CI, anything that only affects source builds | No | Not in the artifact |

The `CLAUDE.md` files are not in the artifact (`release.yml` packs files by name), so a change to them alone needs no
release.

When a release is needed, follow the release row of the sync table after the merge (version + CHANGELOG section +
`v*` tag), and rerun `infra/kernel/core-portability.sh` first if the data plane changed. Several PRs may go out in one
release, but each PR says "goes out with the next release" -- it is never left unsaid.

## Performance-claim discipline

**Do not put a performance number in the documentation without test data behind it.** Formal performance conclusions
may only come from a two-VM test bed that passes `infra/preflight.sh formal` (by default ≥ 16 logical CPUs, ≥ 32 GiB
RAM, ≥ 120 GiB free disk in the working directory, a writable `/dev/kvm` and vmx/svm; setting one up is in
`research/experiments/README.md`). A machine below those thresholds is good for code and verifier checks and short
smoke runs only, and **must not be used to draw performance conclusions**.

## Style

- **Language (a hard rule): `README.zh.md` is the only Chinese file in the repository.** Everything else is
  English -- documents, the `CLAUDE.md` files, config and code comments, script and report output, test strings,
  commit messages and PRs. `README.zh.md` is the Chinese twin of `README.md`, and the two change together.
- Rust: `cargo fmt` is mandatory; `make check` enforces it.
- BPF C: four-space indentation.
- Shell: `set -euo pipefail`; the POSIX sh scripts (`scripts/bootstrap.sh`, `infra/boot-enable.sh`) use `set -eu`,
  and `infra/boot-disable.sh` deliberately only `set -u`, so that a stop never fails half way.
- Comments explain **why**. A large share of the comments in this repository record verifier limitations, kernel
  behaviour and holes already fallen into. Those comments are worth more than the code around them; do not delete
  them for brevity.
