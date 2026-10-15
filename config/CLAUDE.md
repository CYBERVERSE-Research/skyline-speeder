# config/CLAUDE.md

Details for the configuration templates. Claude Code loads this file once it reads or edits a file in this directory;
the repository-wide conventions are in the root `CLAUDE.md` (a config field change also updates `config/*.toml` and
`docs/02-interface-reference.md` section 6, per its "Docs to update" rule).

## The guest config constraint

`[rack_tuning]` in `config/speeder-guest.toml` **must stay commented out in its entirety**. That config is driven
per-case by the experiment matrix (`infra/apply-guest-profile.sh`), which controls `tcp_recovery`, `tcp_reordering`
and `tcp_early_retrans` exactly. If `skyline-speederd` takes ownership of those global sysctls here, a daemon restart
silently overwrites the per-case values with defaults.

`crates/skyline-common/src/lib.rs` has a `guest_config_never_owns_global_sysctls` test guarding this. **Do not weaken
or delete it to make a case pass.**

## `tc_interface` pointing at the wrong NIC (details of silent failure point 3)

The default in `config/speeder-guest.toml`, `data0`, is the test bed's interface name and will almost never match a
real host. `install.sh` rewrites it on every run, install or upgrade, while the file still holds the template line
`tc_interface = "data0"`: the first Ethernet device a default route (IPv4, then IPv6) leaves through takes its place.
A value an operator has changed is never touched. When the default route only goes through a tunnel (WireGuard, tun,
...), it keeps the placeholder and warns: `skyline_tc` is only attached to an Ethernet device. The guard keeps the root
qdisc of that same NIC on `fq`: pointed at the wrong one, the real egress NIC keeps whatever qdisc it had, and the
other NIC gets `fq` instead.
