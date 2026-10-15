# infra/CLAUDE.md

Details for the scripts in `infra/`. Claude Code loads this file once it reads or edits a file in this directory; the
repository-wide conventions are in the root `CLAUDE.md`.

- **Global sysctls and qdiscs.** Who writes `net.ipv4.tcp_congestion_control`, `net.core.default_qdisc` and the egress
  NIC's root qdisc, and under what preconditions, is an invariant; the full rules are in `crates/CLAUDE.md`, "Global
  sysctls". Read them before changing such a write in any of these scripts.
  - `boot-enable.sh` deliberately writes no sysctl; **do not add back** the `default_qdisc=fq` it used to write.
  - `boot-disable.sh` drains first and writes the default back only if it is still `skyline_cc` afterwards -- the
    fallback for when the daemon is already gone, not a duplicate.
  - `apply-guest-profile.sh` writes the algorithm and qdisc it wants only after `drain --timeout 0`. It sets
    `tcp_recovery`, `tcp_reordering` and `tcp_early_retrans` per case, which is why the guest config must not take
    those sysctls over (`config/CLAUDE.md`).
- **Scripts a release ships.** `release.yml` packs `boot-enable.sh`, `boot-disable.sh`, `run-in-skyline-cgroup.sh`,
  `apply-guest-profile.sh`, `collect-guest-metrics.sh` and `snapshot-skyline-events.sh` into the artifact, so a change
  to them needs a release (root `CLAUDE.md`, "Prebuilt releases").
- **`kernel/core-portability.sh`.** After a data-plane change, rerun it before relying on CO-RE portability
  (`bpf/CLAUDE.md`, "CO-RE portability"), and before tagging a release that changed the data plane.
- **Shell style.** `set -euo pipefail`; `boot-enable.sh` is POSIX sh with `set -eu`, and `boot-disable.sh`
  deliberately uses only `set -u` (root `CLAUDE.md`, "Style").
