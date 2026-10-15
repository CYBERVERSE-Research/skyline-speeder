# bpf/CLAUDE.md

Details for the three eBPF programs in `bpf/`. Claude Code loads this file once it reads or edits a file in this
directory; the repository-wide conventions (the invariant overview, silent failure points, documentation sync,
releases) are in the root `CLAUDE.md`. The full rules for configuration delivery (double slots, a sequence number and
a per-flow copy) are in `crates/CLAUDE.md`, "Configuration-plane semantics": on the BPF side, every ACK reads only the
flow's own copy.

## vmlinux.h and kfunc declarations

`make bpf` reads type information from **this machine's** `/sys/kernel/btf/vmlinux` by default. Point it at another
kernel's BTF with `make VMLINUX_BTF=<path> bpf`, or hand it a ready-made header with
`make PREBUILT_VMLINUX_H=<path> bpf` -- that path needs **neither bpftool nor this machine's
`/sys/kernel/btf/vmlinux`**, and it is what containers and the release workflow use. `bpf/include/vmlinux.h` is a
build product and is **never committed**.

A vmlinux.h generated on this machine is compiled with `-DBPF_NO_KFUNC_PROTOTYPES`: from 7.5 on, bpftool writes a
prototype for every kfunc the running kernel exports, and one of them can disagree with the distribution's
`bpf_helpers.h` (on Fedora 43, kernel 6.17 exports `bpf_stream_vprintk` with five arguments and libbpf 1.6.1 declares
four). So **every kfunc a BPF source calls is declared in that source** (see the top of `skyline_cc.bpf.c`), never
taken from vmlinux.h. The `PREBUILT_VMLINUX_H` path does not set the macro, and the released objects stay byte for
byte the same.

## CO-RE portability

The header only has to **define** the types the code uses. It need not come from the kernel the objects will run on,
but it **must not be newer** than that kernel: bpftool generates it with `preserve_access_index`, so every field access
carries a CO-RE relocation that libbpf fixes against the running kernel's BTF at load time, and CO-RE cannot supply a
field the running kernel lacks -- objects built against a newer kernel's header can fail to load on 6.12, which is why
releases build against the 6.12 reference header (`release.yml`). What has been verified is a 6.12 header: objects
built against it under 6.12.63 load on 6.19.14, and built against it under 6.19.14 load on 6.12.63;
`infra/kernel/core-portability.sh` reproduces this. **After a data-plane change, rerun that script
before relying on this property** -- CO-RE fixes offsets, not a field that was renamed or removed.

## Hard invariants (full constraints)

The four rows of the root `CLAUDE.md`'s invariant table that live in this directory, in full:

| Identifier | Location | Constraint |
|---|---|---|
| The algorithm name `"skyline_cc"` | `.name` of both struct_ops maps in `bpf/skyline_cc.bpf.c`; the `TCP_CONGESTION` setsockopt in `bpf/skyline_policy.bpf.c`; `skyline_runs_skyline_cc()` in `bpf/skyline_tc.bpf.c`; `SKYLINE_CC_NAME` in `crates/skyline-common`; the literals in `infra/boot-enable.sh`, `infra/boot-disable.sh`, `infra/apply-guest-profile.sh` and `install.sh` | Registration name, **≤ 15 characters** (`TCP_CA_NAME_MAX` is 16 including NUL), byte-identical everywhere. The daemon's sysctl write fails loudly on a mismatch; the policy and TC programs do not -- the setsockopt's error is ignored, and first-flight redundancy is left copying SYN-ACKs only. The struct_ops map names are a separate namespace |
| `SKYLINE_ABI_VERSION` and the three other ABI versions | `bpf/include/skyline_abi.h` and `crates/skyline-common/src/lib.rs` | A layout change bumps the version that covers the struct, on both sides: `SKYLINE_ABI_VERSION` for `struct skyline_config`, `skyline_flow_state` and `skyline_metrics`; `SKYLINE_RTO_TUNING_ABI_VERSION`, `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` and `SKYLINE_REDUNDANCY_ABI_VERSION` for their own structs (`docs/02-interface-reference.md` section 5). Since 0.5.2 `skyline-speederd` reads the version a `skyline_cc.bpf.o` was built for (`skyline_abi_version` in its `.rodata.abi` section) before loading it, and refuses an object of another version, or one without the section, with both versions in the error (`ssctl enable`, `--validate-only --verify-bpf` and the installer's verifier step). An older daemon that does not check can still load a newer object: then every flow's configuration copy (`skyline_adopt_config()`) fails, each flow grows its window as the kernel's Reno does (before 0.5.2 it stayed at the kernel's initial window, after an RTO at what `tcp_enter_loss()` left) and nothing sets its pacing. `cong_control` has replaced the kernel's own growth, so this is not a fallback to the kernel. There is no error and no counter: in `ssctl status --json`, `active_flows` grows while `metrics.ack_events` does not. A `struct skyline_config` of another size fails loudly instead, when the daemon writes the slot. The version also covers `struct skyline_metrics`: the Rust side reads the counters by layout, and a size mismatch leaves them all at 0. On a mismatch of their own versions, DSCP marking and first-flight redundancy skip their work and count `abi_mismatch`, and since 0.5.2 `skyline_policy` treats a dynamic-RTO slot as no configuration. The bump is what makes a half-updated pair stop instead of misreading the struct |
| `cong_control` 4-argument signature | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`, which exists only on kernel >= 6.10 |
| Two struct_ops maps, `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | Both set **the same callbacks** and differ only in where CA_EVENT_TX_START arrives (`cwnd_event`, or `cwnd_event_tx_start` from Linux 7.1); the daemon creates one of them, by the kernel's BTF. **A callback added to one goes into the other too**, or behaviour silently depends on the kernel version |

`PRR-SSRB` in `skyline_cc.bpf.c` is RFC 6937 terminology; do not rename it (root `CLAUDE.md`, "Naming landmine").

A change to a source or header in this directory (a comment included: `-g` line information changes and the object
bytes with it) needs a release; see "Prebuilt releases" in the root `CLAUDE.md`.
