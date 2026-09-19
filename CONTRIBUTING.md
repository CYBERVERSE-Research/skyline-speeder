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
make check                        # cargo fmt --check + cargo check + unit tests
make test                         # includes cargo test
```

`make bpf` reads type information from **this machine's** `/sys/kernel/btf/vmlinux`,
so it must run on the target kernel, or be pointed at BTF explicitly with
`make VMLINUX_BTF=<path> bpf`. `bpf/include/vmlinux.h` is a build artefact and is
never committed.

`make check` and `make test` drive a Python venv for the experiment-harness unit
tests, which need **Python 3.11 or newer** (`research/experiments/matrix_lib.py`
imports `tomllib`; on 3.10 and earlier the tests fail at import). Either run
`make venv` once, or point them at any suitable interpreter:

```bash
make check PYTHON=$(command -v python3)
```

### Required before every commit

Push all three objects through the kernel verifier without leaving runtime state
behind:

```bash
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

Paste the result into your pull request. A change that has not been through the
verifier is not reviewable, and CI cannot do this for you — GitHub runners do not
run a 6.12+ kernel, so CI compiles the objects but cannot load them.

## Hard invariants

Breaking any of these is a defect regardless of what else the change does.

| Identifier | Location | Constraint |
|---|---|---|
| `.name = "skyline_cc"` | `bpf/skyline_cc.bpf.c` | Registration name, **≤ 15 characters** (`TCP_CA_NAME_MAX` is 16 including NUL) |
| `SKYLINE_ABI_VERSION` | `bpf/include/skyline_abi.h` | Must be incremented on any layout change; userspace refuses to load a mismatched object |
| `cong_control` 4-argument signature | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)` — exists only on kernel >= 6.10 |
| Install paths `/opt`, `/etc`, `/run/skyline-speeder` | config, units, scripts | All three must agree |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **Must match the parent directory of `socket_path`** |

> The `RuntimeDirectory` / `socket_path` coupling is a hole this project has
> already fallen into once: a bulk rename changed `RuntimeDirectory` to
> `skyline` while the config still said `/run/skyline-speeder/`. The daemon
> started fine and the control socket simply never appeared. If you touch either
> side, check the other.

### Configuration-plane semantics

- `set-module-config` and `set-rack-rto` are **absolute overwrite**: every call
  carries the complete field set, never a delta. The matching `reset-*` commands
  restore config-file defaults.
- Configuration switching uses **double slots plus a generation counter**. New
  coefficients are written to the inactive slot and the generation is bumped;
  each flow switches only at an RTT boundary, so no flow ever reads a mix of old
  and new values within a single ACK. **Any change to the config delivery path
  must preserve this** or you get torn reads.

## Three silent failure modes

These produce **no error output whatsoever** — they just do not take effect. Be
careful around the code that touches them.

1. **cgroup not migrated.** `skyline_policy` attaches to
   `/sys/fs/cgroup/skyline-speeder`; a process outside it never traverses that
   BPF program. Symptom: `rack_rto.stats.applied` stays at 0. Decision tree in
   `DEPLOY.md` section 7.
2. **`enable` never run.** `skyline-speederd` does **not** attach `skyline_cc`
   on startup — `ssctl enable` is required. `skyline-speeder-enable.service`
   exists precisely to remove this silent failure. **Do not delete it on the
   grounds that `install-guest.sh` does not enable it** — that is deliberate:
   attaching changes congestion control for every new connection on the host,
   which is an operational decision, made explicitly by `install.sh`.
3. **`tc_interface` pointing at the wrong NIC.** The default in
   `config/speeder-guest.toml` is the test bed's interface name and will almost
   never match a real host. `install.sh` detects the default-route interface and
   rewrites it, but only on first install — it never overwrites an
   operator-edited config.

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
it.** Formal performance conclusions may only come from a two-VM test bed meeting
the resource thresholds in `research/experiments/README.md`. A machine below
those thresholds is good for code review, verifier checks and short smoke runs,
and **must not be used to draw performance conclusions**.

## Documentation you must update with your change

| Change | Also update |
|---|---|
| ABI struct | `skyline_abi.h` + `crates/skyline-common` + `SKYLINE_ABI_VERSION` |
| `ssctl` command or field | `docs/02-interface-reference.md` |
| Config field | `config/*.toml` + `docs/02-interface-reference.md` section 6 |
| Install flow | `docs/01-deployment-guide.md` + `DEPLOY.md` + `install.sh` |
| Algorithm behaviour | `docs/03-design.md`; performance claims need data in `docs/04-performance-report.md` |
| Anything user-facing in the README | Both `README.md` and `README.zh.md` |

## Style

- **Rust:** `cargo fmt` is mandatory; `make check` enforces it.
- **BPF C:** four-space indentation.
- **Shell:** `set -euo pipefail`.
- **Comments explain *why*.** A large share of the comments in this repository
  record verifier limitations, kernel behaviour and holes already fallen into.
  Those comments are worth more than the code around them. Do not delete them
  for brevity.

## Cutting a release

Release artifacts are built by `.github/workflows/release.yml` when a `v*` tag
is pushed. It compiles the BPF objects against a **pinned reference header**
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

After a release, verify the shipped objects load on a kernel other than the one
they were built against:

```bash
SKYLINE_BPF_DIR=<unpacked artifact>/bpf infra/kernel/core-portability.sh freeze
# get to the other kernel
infra/kernel/core-portability.sh verify
```

## Never commit

See `.gitignore`. In particular:

- `bpf/include/vmlinux.h` (machine-specific build artefact)
- `target/`, `build/`, `*.bpf.o`
- Rendered `user-data` (contains a real operator SSH public key) — only
  `user-data.template` is committed
- `known_hosts`, keys, filled-in lab inventories
- Any absolute path of the form `/home/<username>`

## Pull requests

- One logical change per PR.
- Include the `--validate-only --verify-bpf` output, and say which kernel version
  you ran it on.
- If the change touches the data path, say what you measured and on what. "It
  feels faster" is not a result; see the performance-claim discipline above.
- If it touches an invariant in the table above, say explicitly how you checked
  the other side.

## License

This repository is **GPL-2.0-only** throughout. See `LICENSE` and `NOTICE`.

Every file carries an `SPDX-License-Identifier`. Keep it when you edit a file,
and add one to any file you create.

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
