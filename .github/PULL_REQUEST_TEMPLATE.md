## What this changes

<!-- One paragraph. What behaviour is different after this merges? -->

## Verifier

CI compiles the BPF objects but **does not load them**. Paste your run on a
6.12 or later kernel:

```
$ skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
<output>
```

Kernel it ran on: `uname -r` = 

## Invariants

<!-- Delete any line that this PR cannot affect. Do not delete a line because
     you did not check it. -->

- [ ] `.name = "skyline_cc"` is still ≤ 15 characters, and the same name is still what `skyline_policy.bpf.c`, `skyline_tc.bpf.c`, `SKYLINE_CC_NAME` and the scripts use
- [ ] ABI layout unchanged, **or** the version that covers it (`SKYLINE_ABI_VERSION`, `SKYLINE_RTO_TUNING_ABI_VERSION`, `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION`, `SKYLINE_REDUNDANCY_ABI_VERSION`) bumped and `crates/skyline-common` updated to match
- [ ] `cong_control` still declared with 4 arguments `(sk, ack, flag, rs)`
- [ ] The `skyline_cc` and `skyline_cc_txs` struct_ops maps still set the same callbacks
- [ ] Install paths `/opt`, `/etc`, `/run/skyline-speeder` still agree across config, units and scripts
- [ ] `RuntimeDirectory=` still matches the parent directory of `socket_path`
- [ ] `checkpath -d /run/skyline-speeder` in `packaging/openrc/skyline-speederd` still matches it too
- [ ] `StateDirectory=` and `checkpath -d /var/lib/skyline-speeder` are still the parent of `DAEMON_REPORTS_DIR`
- [ ] Error reports are still sent only on a `y` typed on a terminal or `SKYLINE_FEEDBACK=yes`, scrubbed first, never by the daemon; the DSN is still only sealed, its two copies agree, and no plain DSN or Sentry auth token is in the diff
- [ ] Config delivery still writes the slot before `config_seq`, and BPF still reads only each flow's own copy (no torn reads)
- [ ] Sysctl order unchanged: `enable` writes `skyline_cc` after the attach and arms the guard after that; `drain` disarms the guard and writes `fallback_cc` under its lock before it unregisters; a stop of the daemon joins the guard before `BpfRuntime` drops and writes no sysctl
- [ ] `skyline-speeder-enable` (the systemd unit and the OpenRC script) still exists, and `infra/boot-enable.sh` still writes no sysctl
- [ ] The guard still leaves classful, shaping and offload root qdiscs, a shaped `cake`, `noqueue` and unknown kinds alone
- [ ] `[rack_tuning]` in `config/speeder-guest.toml` is still commented out in full, and `guest_config_never_owns_global_sysctls` is untouched
- [ ] `PRR-SSRB` was not renamed

## Documentation

- [ ] ABI change → `skyline_abi.h` + `crates/skyline-common` + the version that covers it + `docs/02-interface-reference.md` §5
- [ ] `ssctl` command or field → `docs/02-interface-reference.md`
- [ ] Config field → `config/*.toml` + `docs/02-interface-reference.md` §6
- [ ] Install flow → `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh` (and both READMEs and `docs/usage.md` where they describe it)
- [ ] Algorithm behaviour → `docs/03-design.md`
- [ ] User-facing README change → **both** `README.md` and `README.zh.md`
- [ ] New feature, changed default or `ssctl` report wording → both READMEs' feature table, the feature's subsection, and the key to `ssctl status` / `ssctl flows`
- [ ] Invariant or contribution process → the directory's `CLAUDE.md` (and its summary in the root `CLAUDE.md`) + `CONTRIBUTING.md` + this template
- [ ] Release → `Cargo.toml` version + `Cargo.lock` (no changelog: the repository keeps none)

## Prebuilt release

<!-- The one-line install installs the latest release's prebuilt artifact, not
     main. A change in bpf/, crates/, config/speeder-guest.toml, packaging/ or
     the infra/*.sh that release.yml ships reaches those users only with the
     next release (a host that cannot run the published binaries builds main
     from source instead); install.sh and scripts/bootstrap.sh are fetched from
     main and take effect on merge for later one-line installs, but not for a
     tree an earlier install kept. Answer "needed" (now, or with the next
     release) or "not needed", and why. -->

## Performance claims

<!-- If this PR claims any throughput or latency effect, say what was measured,
     on what test bed, with how many samples. A machine that does not pass
     `infra/preflight.sh formal` cannot support a performance conclusion.
     If the PR makes no performance claim, write "none". -->
