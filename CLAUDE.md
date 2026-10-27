# CLAUDE.md

Rules for AI coding agents. This file loads in every session and keeps each rule to a line; a directory's details live
in its own `CLAUDE.md`, which loads when you read or edit a file there. **Before a change that spans directories** (a
bulk rename, a new writer of a sysctl, an ABI change), read the `CLAUDE.md` of every directory it touches.
`CONTRIBUTING.md` has the full text and rationale of every rule here.

<!-- Maintainers: this file loads in every session, so keep it under 200 lines and each rule to a line. Details go in
     the directory's CLAUDE.md (loaded on demand) with a one-line summary here; if the two disagree, the directory's
     file wins. Block-level HTML comments like this one are stripped before the file reaches the agent. -->

| File | Details |
|---|---|
| `bpf/CLAUDE.md` | vmlinux.h and kfunc declarations, CO-RE portability, the algorithm name, ABI versions, the two struct_ops maps |
| `crates/CLAUDE.md` | order and writers of the global sysctls, the guard, configuration-plane semantics |
| `config/CLAUDE.md` | `[rack_tuning]` in the guest config, `tc_interface` |
| `packaging/CLAUDE.md` | install paths, `RuntimeDirectory=` / `checkpath` |
| `infra/CLAUDE.md` | boot scripts and sysctls, the scripts a release ships, `core-portability.sh` |
| `docs/CLAUDE.md` | documentation map |

## What breaks here

`skyline_cc` is a kernel congestion control algorithm registered through struct_ops, beside two more eBPF programs and
a Rust control plane. A program the verifier rejects does not load at all; a struct changed on one side of the shared
ABI (`bpf/include/skyline_abi.h`) is silently misread; a defect hits every TCP connection on the host.

## Build and check

```bash
make bpf                     # vmlinux.h (a build product, never committed) + the three CO-RE objects
cargo build --workspace --release
infra/build-musl.sh          # musl control plane in an Alpine 3.21 container, as releases and CI build it
make check                   # bash -n, harness tests, cargo fmt --check, cargo check -- not cargo test
make test                    # harness tests + cargo test --workspace
# the harness tests need Python >= 3.11 (tomllib): make check PYTHON=/path/to/python3.11
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf   # verifier, no attach: before every commit
```

## Hard invariants

- **Algorithm name `"skyline_cc"`:** ≤ 15 characters, byte-identical in the three `bpf/` programs, `SKYLINE_CC_NAME`,
  three `infra/` scripts and `install.sh`; a mismatch in the policy or TC program fails silently. → `bpf/CLAUDE.md`
- **ABI:** a layout change bumps the version that covers the struct (`SKYLINE_ABI_VERSION` or one of three others) in
  `skyline_abi.h` and `crates/skyline-common` together. → `bpf/CLAUDE.md`
- **`cong_control`** keeps its 4-argument `(sk, ack, flag, rs)` signature (kernel >= 6.10). → `bpf/CLAUDE.md`
- **struct_ops maps `skyline_cc` / `skyline_cc_txs`** set the same callbacks: a callback goes into both. → `bpf/CLAUDE.md`
- **Install paths** `/opt`, `/etc`, `/run/skyline-speeder` agree across config, units, OpenRC scripts and scripts;
  `RuntimeDirectory=` and OpenRC's `checkpath -d` are the parent of `socket_path`, `events_path` and `state_path`;
  `StateDirectory=` and the second `checkpath` are the parent of `DAEMON_REPORTS_DIR`. → `packaging/CLAUDE.md`
- **Error reports** are sent only on a `y` typed on a terminal (or `SKYLINE_FEEDBACK=yes`), scrubbed first; the daemon
  only records. The DSN is in the tree only sealed, by `infra/seal-feedback-dsn.py`, in two copies. → `crates/CLAUDE.md`
- **`PRR-SSRB`** in `bpf/skyline_cc.bpf.c` is RFC 6937 terminology, not the project's old name SSR: never rename it, and
  protect it with word boundaries in a bulk rename.
- **Configuration plane:** every `set-*` command is an absolute overwrite; a switch writes the slot before the sequence
  number, and BPF reads only each flow's own copy. → `crates/CLAUDE.md`
- **`[rack_tuning]`** in `config/speeder-guest.toml` stays commented out; never weaken
  `guest_config_never_owns_global_sysctls` to make a case pass. → `config/CLAUDE.md`

## Silent failures (no error, just no effect)

1. **cgroup not migrated:** only processes in `/sys/fs/cgroup/skyline-speeder` pass through `skyline_policy`;
   `rack_rto.stats.established_cb` stays 0 (`DEPLOY.md` section 7).
2. **`enable` never run:** the daemon does not attach `skyline_cc` by itself, and `skyline-speeder-enable.service` exists
   for exactly this -- never delete it. `enable` and `drain` write the host's sysctls in a fixed order and the guard
   holds them in between; **stopping the daemon writes no sysctl** and `infra/boot-enable.sh` writes none -- never add
   one. → `crates/CLAUDE.md`
3. **`tc_interface` on the wrong NIC:** the guest template's `data0` is the test bed's NIC; when `install.sh` rewrites
   it → `config/CLAUDE.md`.

## Never commit

`bpf/include/vmlinux.h`, `target/`, `build/`, `*.bpf.o`, a rendered `user-data` (commit `user-data.template`),
`known_hosts`, keys, filled-in lab inventories, any `/home/<user>` path, `.claude/` and `CLAUDE.local.md` at any level,
a plain Sentry DSN or a Sentry auth token (seal the DSN with `infra/seal-feedback-dsn.py`).
The `CLAUDE.md` files are public: no host addresses, account names, tokens or personal paths.

## Every change

- **Language (hard rule):** `README.zh.md` is the only Chinese file; everything else -- docs, comments, script and
  report output, test strings, commits, PRs -- is English. The two READMEs change together.
- **Docs to update** (full table in `CONTRIBUTING.md`): ABI struct → its version + `docs/02` section 5; `ssctl` command
  or field → `docs/02`; config field → `config/*.toml` + `docs/02` section 6; install flow → `DEPLOY.md`, `install.sh`,
  `scripts/bootstrap.sh` (and the READMEs and `docs/usage.md` where they describe it); a new feature, default or report
  wording in `crates/ssctl/src/view.rs` → both READMEs' feature table, its subsection and the report key
  (`cargo test -p ssctl -- --nocapture renders_for_review` prints both reports); an invariant → its directory's
  `CLAUDE.md`, `CONTRIBUTING.md` and the PR template's Invariants list; algorithm behaviour → `docs/03`.
- **Does it need a release?** Every PR answers in its "Prebuilt release" section. The one-line install takes the latest
  release, not `main` (hosts that cannot run it build `main`: `prebuilt_blocker` in `install.sh`), so a change in
  `bpf/`, `crates/`, `config/speeder-guest.toml`, `packaging/` or a shipped `infra/*.sh` reaches users only with a
  release; `install.sh` and `scripts/bootstrap.sh` usually take effect on merge; docs, CI and the harness never need
  one. Several PRs may share a release, but each PR says so.
- **Releasing:** bump `[workspace.package] version` and the three crates in `Cargo.lock` (builds run `--locked`), then
  tag `v<version>`; `release.yml` refuses a mismatch. **No changelog:** the repository keeps none -- never add
  `CHANGELOG.md` or a release-notes file. If the data plane changed, rerun `infra/kernel/core-portability.sh` first.
- **Performance numbers** in docs come only from a two-VM test bed that passes `infra/preflight.sh formal`; smaller
  machines are for code, verifier and smoke checks, never for performance conclusions.

## Style

Rust: `cargo fmt` (`make check` enforces it). BPF C: four-space indentation. Shell: `set -euo pipefail`; the POSIX sh
scripts (`scripts/bootstrap.sh`, `infra/boot-enable.sh`) use `set -eu`, and `infra/boot-disable.sh` deliberately only
`set -u`, so a stop never fails half way. Comments explain why: the ones recording verifier limits, kernel behaviour and
past mistakes are worth more than the code around them -- never delete them for brevity.
