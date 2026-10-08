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

- [ ] `.name = "skyline_cc"` is still ≤ 15 characters
- [ ] ABI layout unchanged, **or** `SKYLINE_ABI_VERSION` bumped and `crates/skyline-common` updated to match
- [ ] `cong_control` still declared with 4 arguments `(sk, ack, flag, rs)`
- [ ] The `skyline_cc` and `skyline_cc_txs` struct_ops maps still set the same callbacks
- [ ] Install paths `/opt`, `/etc`, `/run/skyline-speeder` still agree across config, units and scripts
- [ ] `RuntimeDirectory=` still matches the parent directory of `socket_path`
- [ ] `checkpath -d /run/skyline-speeder` in `packaging/openrc/skyline-speederd` still matches it too
- [ ] Config delivery still goes through the double-slot + generation counter (no torn reads)
- [ ] `[rack_tuning]` in `config/speeder-guest.toml` is still commented out in full, and `guest_config_never_owns_global_sysctls` is untouched
- [ ] `PRR-SSRB` was not renamed

## Documentation

- [ ] ABI change → `skyline_abi.h` + `crates/skyline-common` + `SKYLINE_ABI_VERSION`
- [ ] `ssctl` command or field → `docs/02-interface-reference.md`
- [ ] Config field → `config/*.toml` + `docs/02-interface-reference.md` §6
- [ ] Install flow → `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh`
- [ ] Algorithm behaviour → `docs/03-design.md`
- [ ] User-facing README change → **both** `README.md` and `README.zh.md`

## Prebuilt release

<!-- The one-line install installs the latest release's prebuilt artifact, not
     main. A change in bpf/, crates/, config/speeder-guest.toml, packaging/ or
     the infra/*.sh that release.yml ships reaches those users only with the
     next release; install.sh and scripts/bootstrap.sh are fetched from main
     and take effect on merge. Answer "needed" (now, or with the next release)
     or "not needed", and why. -->

## Performance claims

<!-- If this PR claims any throughput or latency effect, say what was measured,
     on what test bed, with how many samples. A machine below the thresholds in
     research/experiments/README.md cannot support a performance conclusion.
     If the PR makes no performance claim, write "none". -->
