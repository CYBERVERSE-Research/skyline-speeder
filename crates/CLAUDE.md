# crates/CLAUDE.md

Details for the Rust control plane (`skyline-speederd`, `ssctl`, `skyline-common`). Claude Code loads this file once it
reads or edits a file in this directory; the repository-wide conventions are in the root `CLAUDE.md`. The full
constraints on the ABI versions are in `bpf/CLAUDE.md`: `skyline-common` and `bpf/include/skyline_abi.h` change
together.

## Global sysctls: order and writers (details of silent failure point 2)

`ssctl enable` is host-wide by itself: after a successful struct_ops attach it writes
`net.ipv4.tcp_congestion_control = skyline_cc`. **The order of the sysctl writes is an invariant**: `enable` writes
after the attach (the kernel rejects a name that is not registered), and `drain` writes `fallback_cc` back before it
unregisters (otherwise a struct_ops would be unregistered while it is still the default algorithm). Inside
skyline-speederd every write of that sysctl goes through `set_default_congestion_control` in
`crates/skyline-speederd/src/main.rs` (`BpfRuntime::load` writes `fallback_cc` as the baseline before the attach;
then `enable`, the guard, `drain`), and from `enable` to `drain` the daemon is its only writer. Outside that window
three others write it, each for a reason: `infra/boot-disable.sh` drains first and writes the sysctl only if the
default is still `skyline_cc` afterwards -- the fallback for when the daemon is already gone, not a duplicate (written
before the drain, the still-armed guard would put `skyline_cc` back); the experiment harness's
`infra/apply-guest-profile.sh` writes the algorithm it wants after `drain --timeout 0`; and `install.sh --uninstall`
writes bbr (or, with `--restore-pre-install`, the pre-install value) after draining and removing the daemon.
`infra/boot-enable.sh` deliberately writes no sysctl.

**Being switched back after the attach is just as silent.** A "one-click BBR" script's `bbr` line in `/etc/sysctl.d`,
re-applied by `sysctl --system`, moves every new connection off `skyline_cc` without an error. So between `enable` and
`drain` the guard (`crates/skyline-speederd/src/guard.rs`, config `[guard]`) keeps three things in place: the default
congestion control and, with `[guard] qdisc = true` (the default), `net.core.default_qdisc = fq` and the root qdisc of
`runtime.tc_interface` (`fq`, or `mq` over `fq` on a multi-queue NIC; when that is a VLAN, bond or bridge whose root
is `noqueue`, the NICs with a `device` link under it instead, never a tap or veth). From `enable` to `drain` the daemon
is the only writer of all three (after a drain, `apply-guest-profile.sh` and `install.sh --uninstall` set qdiscs of
their own). `boot-enable.sh` used to write `default_qdisc=fq` once at boot and no longer does -- **do not add it back**:
it cannot change a NIC's existing root qdisc, and two writers of one setting drift apart. Related invariants:

- `enable` arms the guard only after it has written `skyline_cc`. `drain` disarms it first and writes `fallback_cc`
  **while still holding the guard's lock**; otherwise a periodic check could write `skyline_cc` back after the drain.
  A drain that times out leaves it disarmed too (the experiment matrix drains with `--timeout 0` and then sets its own
  qdisc).
- On stop, the guard thread is stopped and joined before `BpfRuntime` drops and unregisters the struct_ops
  (`impl Drop for Daemon`); otherwise an armed check would write a `skyline_cc` the kernel no longer knows.
- **Stopping the daemon writes no sysctl.** The maintainer explicitly does not want `fallback_cc` written on stop; do
  not add it. `drain` and the enable unit's `ExecStop` write it back.
- The guard replaces only qdisc kinds with no configuration worth keeping (`pfifo_fast`, `fq_codel`, an unshaped
  `cake`, `fq_pie`, ...). Classful, shaping and offload kinds (`htb`, `tbf`, `netem`, `mqprio`, ...), a `cake` with a
  bandwidth or `autorate-ingress`, `noqueue` and unknown kinds are left alone: replacing them would silently destroy
  an operator's shaping.

The guard looks after the root qdisc of the `runtime.tc_interface` NIC; what happens when that names the wrong NIC is
in `config/CLAUDE.md`.

## Configuration-plane semantics

- `set-module-config`, `set-rack-rto`, `set-retransmit-dscp` and `set-redundancy` are **absolute overwrite**: every
  call carries the complete field set, never a delta. The matching `reset-*` commands restore config-file defaults.
- Configuration switching uses **double slots, a sequence number and a per-flow copy**. The daemon writes
  configuration N into `config_slots[N & 1]` and **only then** sets `config_seq` to N. A flow copies the slot
  `config_seq` names when it starts (the ACK that completes the handshake reaches `cong_control` before `init`, and
  `init` copies again and starts over). A flow that has no copy yet tries again on every ACK; one that has a copy
  copies again only when a new round starts and `config_seq` has moved. A round is not one RTT:
  `skyline_new_round()` ends it when `prior_delivered` reaches the round's `delivered + packets_out`, about two RTTs
  for a flow that keeps its window full (about one right after a start or a switch). A copy taken while `config_seq`
  moved is discarded, and every ACK reads only the flow's own copy. So no flow reads a mix of old and new values
  within a round, however quickly updates follow each other. **Any change to the config delivery path must keep both
  halves** -- slot first, sequence number second; BPF reads only the copy -- or you get torn reads (0.4.x had neither,
  and two updates within one RTT rewrote a slot live flows were reading).

## The test that guards the guest config

`guest_config_never_owns_global_sysctls` in `crates/skyline-common/src/lib.rs` guards the rule in `config/CLAUDE.md`
that `[rack_tuning]` stays commented out -- **do not weaken or delete it to make a case pass**.
