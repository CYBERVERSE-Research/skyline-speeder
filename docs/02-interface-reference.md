# Skyline Speeder Interface and Configuration Reference

This document describes every interface the Skyline Speeder control plane exposes: the command-line tool `ssctl`, the
Unix socket wire protocol behind it, the runtime status fields, the configuration file fields, and the observability
events/counters.

## 1. Control plane overview

At run time Skyline Speeder consists of a daemon, `skyline-speederd`, and a command-line client, `ssctl`:

- `skyline-speederd` runs on the server as a system service (a systemd unit; an OpenRC script on Alpine). It loads the
  BPF objects, maintains the configuration state, and accepts control requests over a Unix domain socket (default path
  `/run/skyline-speeder/speeder.sock`). Its own command line has only `--config <path>` (the default is the relative
  path `config/speeder.toml`; the service passes `/etc/skyline-speeder/speeder.toml`), `--validate-only` (read the
  configuration, probe capabilities, print the status JSON and exit; exits non-zero when a capability is missing),
  `--verify-bpf` (only together with `--validate-only`: additionally push the three objects through the kernel
  verifier) and `--version`.
- `ssctl` is a one-shot command client for `skyline-speederd`: each invocation opens one connection, sends one JSON
  request, reads one line of JSON response, and exits. All state is kept in the `skyline-speederd` process; `ssctl`
  itself holds no state.
- After a successful `ssctl enable` and until the next `ssctl drain`, `skyline-speederd` also periodically checks, in the
  background, and keeps in place the host-wide default congestion control and the `fq` qdisc (the guard); see
  section 9.

The request/response structures of the two are defined in the shared library `skyline-common`, so in this document
"`ssctl` command-line arguments" and "wire protocol fields" are two presentations of the same semantics.

## 2. The `ssctl` command line

Invocation: `ssctl [--socket <path>] <subcommand> [arguments...]`. `--socket` defaults to
`/run/skyline-speeder/speeder.sock` and usually does not need to be overridden.

`ssctl --version` prints `ssctl <version>`, and `skyline-speederd --version` does the same -- both report the version of
the binary on disk. For the version of the **running** daemon, look at `version` in `ssctl status` (section 4): if the
two differ after an upgrade, the old daemon is still the one running in memory.

### 2.1 Read-only commands

| Command | Purpose |
|---|---|
| `ssctl validate` | Validates the current configuration in the daemon's **memory** (the file read at startup plus any online changes since) and the kernel capabilities, without changing the running state. It does not re-read the file on disk: to validate a file, use `skyline-speederd --config <file> --validate-only` |
| `ssctl status` | **Takeover status**: whether skyline_cc is attached, whether it is the host-wide default congestion control, what the guard holds and how many times it has corrected it, the kernel capabilities, the M1 tier-1 global sysctls, the daemon's uptime, and an `ATTENTION` section listing every anomaly with the action that addresses it |
| `ssctl flows` (alias `ssctl flow`) | **The accelerated traffic**: lists, one per row, the TCP connections the kernel is currently running on `skyline_cc` (peer, RTT, cwnd, pacing rate, delivery rate, bytes sent, retransmitted share; sorted by bytes sent, at most 50 rows), plus the coefficients in effect on those connections and the algorithm-decision counters. See section 4.1 |
| `ssctl snapshot <path>` | Has the daemon write the complete `status` JSON to the given file, for external scripts to collect (not affected by the output format below; always JSON). The daemon writes the file: a relative path resolves against the daemon's working directory; under systemd the daemon runs with `ProtectSystem=strict` and a private `/tmp`: most paths are read-only to it, and a file it writes into `/tmp` is invisible to the caller, so writing under `/run/skyline-speeder/` is the safest. From 0.5.2 on it writes only regular files: when the target is a symbolic link, a directory, a device or a FIFO it refuses and writes nothing (the daemon runs as root; before, `ssctl snapshot /dev/sda` would write the JSON over the start of the disk, and a FIFO would block the whole request loop) |

#### Output format

From 0.3.0 on, every `ssctl` subcommand **prints a human-readable report by default** instead of JSON (except what
`snapshot` writes into the file). When the reply's `ok` is `false`, the exit code is 1.

| Option | Effect |
|---|---|
| `--json` | Prints the daemon's raw reply, byte for byte the same as 0.2.0. Scripts that parse the output should always add this option |
| `--color auto\|always\|never` | Default `auto`: colour only when stdout is a terminal, `NO_COLOR` is not set and `TERM` is not `dumb`. Under a non-UTF-8 locale the symbols fall back to ASCII automatically |

In `--json` mode the sponsor notice line goes to stderr, and stdout carries only JSON.

When a pipe closes the output early (`ssctl --json flows | head -1`, `ssctl status | grep -q ACCELERATING`), `ssctl`
quietly stops writing, and the exit code depends only on the daemon's reply (from 0.4.4 on; before that it panicked and
exited with 101, which under `set -o pipefail` turned a successful check into a failure).

> **Upgrade note**: checks from the 0.2.0 era such as `ssctl status | grep -q '"enabled": true'` must be changed to read
> `status.enabled` itself from the `--json` output -- the output has one key per line, and the first `"enabled":` is
> that one (see `sv` at the top of `DEPLOY.md` for how to write it). Do not use `grep -q '"enabled": true'`: from 0.4.0
> on the JSON also contains `redundancy.config.enabled`, which defaults to `true` and holds even when nothing is
> attached. `install.sh` uses exactly the first-key approach, and falls back automatically when it meets an older
> `ssctl` (one that does not know `--json`).

### 2.2 Feature switches (M1-M4 ablation)

| Command | Purpose |
|---|---|
| `ssctl enable [--modules m1,m2,...] [--all-off]` | Enables the `skyline_cc` struct_ops. `--modules` names the subset of modules to turn on (`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`, comma-separated); when it is omitted, the `enabled_modules` currently in effect are used (the daemon's in-memory state, which comes from the configuration file at startup). `--all-off` is equivalent to passing an empty set -- `skyline_cc` is still registered as the congestion control algorithm, but all four modules are off (see the "neutral baseline" semantics in `docs/03-design.md` section 4). **After a successful attach it sets `net.ipv4.tcp_congestion_control` to `skyline_cc`**, i.e. every new connection on the host uses this algorithm by default; the sysctl write happens after the attach because the kernel rejects an algorithm name that is not yet registered. It then **arms the guard and immediately runs one full check** (also when `[guard] interval_s = 0`): with `[guard] qdisc = true` (the default) it sets `net.core.default_qdisc` to `fq` and replaces the root qdisc of `runtime.tc_interface` with `fq` (when that is a VLAN, bond or bridge, it replaces the root qdisc of the physical NIC under it), and from then until the next `drain` it keeps these in place; see section 9. For a replacement on which the periodic check is backing off, `enable` does not wait out the backoff but retries immediately. What this check changed, what it found and which step did not succeed are all appended to the end of the response `message` in the form `; <content>`; a failure in the qdisc step does **not** fail `enable` (by then `skyline_cc` is attached and is already the default), and is only recorded in the message and in `guard.last_error`. The `enable` of the published v0.2.0 and earlier releases does not touch qdiscs; if you do not want it to, set `[guard] qdisc = false`. Finally it turns on first-flight redundancy (section 2.6). A refused `enable` (an invalid module combination, insufficient kernel capabilities, a load or attach failure) changes nothing: the module set and the generation number stay as they were (from 0.5.2 on; before, a refused module set stayed in memory, `status` reported it as the one in use, and the next `disable` or `set-module-config` would also send it to the kernel). When the cgroup dispatch cannot be opened (for example `skyline_policy` is not loaded), `enable` still completes, with the reason appended to the end of the message: the host-wide default algorithm written afterwards covers the processes in the cgroup as well |
| `ssctl disable --module <name>` | Turns off a single module without affecting the other enabled modules |
| `ssctl drain [--timeout <seconds>]` | Graceful removal: **it first disarms the guard and, while still holding the guard lock, writes `net.ipv4.tcp_congestion_control` back to `fallback_cc`** (a periodic check in progress runs to completion first, and the value it wrote is overwritten right after; even if the rtnl lock is stuck, this wait is only about one `tc` timeout, i.e. around 10 seconds, see "Failure handling" in section 9; after that no check writes `skyline_cc` back again. Whether the drain finally succeeds or times out, the guard stays disarmed), then turns off the cgroup dispatch -- both admission paths are closed first, and the global sysctl first of all, because it admits every process on the host rather than only those in the cgroup; it then waits for existing connections to end on their own (default timeout 300 seconds; if active connections remain at the timeout it reports an error and does not force the removal), and once it has confirmed there are no active connections it unregisters the struct_ops (by then the global default has long stopped pointing at it, so a struct_ops is never unregistered while it is still the default algorithm). When it times out with an error, the struct_ops is still attached, but the sysctl has already been put back, so no new connection uses it (**a drain run over SSH always ends up in this branch** -- the operator's own SSH connection is an existing skyline_cc flow); connections already covered by TC statistics/M1 tier-2 RTO tuning are not affected; only `skyline_cc` is removed. drain does not touch any qdisc: `default_qdisc` and the NIC's root qdisc stay `fq`. From the timeout until the unregister or the next `enable`, `draining` in `status` is `true`, and both reports show `DRAINING` and the number of connections skyline_cc is still serving (0.5.1 and earlier reported this state as skyline_cc having been changed by someone else, and suggested running `enable` again). When the TC object is not loaded (`runtime.tc_interface` is unset, is a tunnel, or the NIC does not exist) or `skyline_policy` is not loaded, drain no longer fails: turning off something that is not running counts as done (0.5.1 and earlier reported an error and exited after writing back `fallback_cc` and before waiting for any connection, and the struct_ops stayed attached) |

### 2.3 Online tuning of the M2/M3/M4 coefficients

| Command | Purpose |
|---|---|
| `ssctl set-module-config [--<field> <value> ...]` | Overwrites all M2/M3/M4 coefficients and safety limits; see the field tables in section 6. Absolute overwrite semantics: every call sends the complete field set; there is no partial update that "changes one field and keeps the rest", which avoids the troubleshooting difficulty a mix of old and new values brings; fields not given take `ssctl`'s built-in defaults (the same as the shipped configuration template), not the daemon's current values. The two boolean fields are written as switches: `--disable-prr-pacing` and `--disable-auto-pacing`; leaving one out means on |
| `ssctl reset-module-config` | Restores the values declared in the configuration file `skyline-speederd` read at startup |

### 2.4 M1 tier-2 dynamic RTO tuning

| Command | Purpose |
|---|---|
| `ssctl set-rack-rto [--disable] [--<field> <value> ...]` | Configures the per-flow dynamic `TCP_BPF_RTO_MIN` floor and `TCP_RTO_MAX_MS` ceiling; see the field tables in section 6. `--disable` unsubscribes from `BPF_SOCK_OPS_RTT_CB` and restores the kernel's default RTO floor; it is an unconditional off. Absolute overwrite semantics as well: fields not given take `ssctl`'s built-in defaults (`--srtt-permille 1000`, `--floor-us 5000`, `--ceiling-us 200000`, `--warmup-samples 8`, and the three `--rto-max-*` at 0, i.e. the ceiling off), not the values in the configuration file |
| `ssctl reset-rack-rto` | Restores the `[rack_rto]` declared in the configuration file, and is also the only way to send that section to BPF: every time the daemon starts it initialises dynamic RTO to off (section 6). The shipped template has no such section, i.e. off; where the file says `enabled = true`, it turns the feature on. From 0.5.1 on, when the file turns it on and it is currently off, both the ATTENTION section of `ssctl status` and `ssctl flows` prompt you to run it |

### 2.5 DSCP marking of retransmitted packets

| Command | Purpose |
|---|---|
| `ssctl set-retransmit-dscp --dscp-value <1-63>` | Enables marking and sets the DSCP code point. The value is a placeholder parameter -- the actual code point is decided by network-side policy, and Skyline Speeder itself presets no production value; enabling requires a non-zero value (0 semantically means "strip the existing DSCP", not a no-op, so it is rejected). Fails when the TC program is not loaded, and marking stays off |
| `ssctl set-retransmit-dscp --disable` | Turns marking off regardless of what the configuration file says (from 0.5.2 on; before, the only ways were restarting the daemon or `reset-retransmit-dscp` -- and when the file turns marking on, reset turns it on). Also succeeds when the TC program is not loaded: it was not marking anyway |
| `ssctl reset-retransmit-dscp` | Restores the `[retransmit_dscp]` declared in the configuration file (off by default), and is also the only way to send that section to BPF: every time the daemon starts it initialises marking to off. From 0.5.1 on, when the file turns it on and it is currently off, both the ATTENTION section of `ssctl status` and `ssctl flows` prompt you to run it |

### 2.6 First-flight redundancy

While skyline_cc is enabled, the TC program sends two copies of every SYN-ACK this host sends, every SYN a skyline_cc
socket sends, and every data segment of a skyline_cc connection whose start falls within the first `first_kib` KiB,
with the copy sent `delay_ms` later (the rationale is in `docs/03-design.md` section 9, the configuration in
section 6.9). On by default.

| Command | Purpose |
|---|---|
| `ssctl set-redundancy [--first-kib <1-1024>] [--delay-ms <0-100>] [--disable]` | Overwrites `[redundancy]` as a whole: parameters not given take the built-in defaults (`64`/`10`), not "unchanged"; `--disable` turns it off. Takes effect immediately when skyline_cc is enabled, otherwise at the next `ssctl enable` |
| `ssctl reset-redundancy` | Restores the `[redundancy]` declared in the configuration file (on, with `64`/`10`, when the section is omitted entirely) |

## 3. Unix socket wire protocol

The transport format is a single-line JSON request plus a single-line JSON response, exchanged over a Unix domain
socket: after connecting, the client writes one line holding the JSON serialisation of a `Request` (terminated by
`\n`), reads one line holding the JSON serialisation of a `Response`, and may then close the connection.

`Request` uses the `command` field as its type tag (kebab-case), for example:

```json
{"command": "set-module-config", "config": {"max_pacing_mbps": 1200, ...}}
```

Request variants (one-to-one with the `ssctl` subcommands, see section 2): `validate` / `enable` /
`disable-module` / `status` / `flows` / `snapshot` / `drain` /
`set-rack-rto` / `reset-rack-rto` / `set-module-config` /
`reset-module-config` / `set-retransmit-dscp` / `reset-retransmit-dscp` /
`set-redundancy` / `reset-redundancy`. There is also a `shutdown` that `ssctl` does not send: when the daemon receives
SIGTERM/SIGINT it sends it to itself over its own socket to leave the accept loop; any client that can open this socket
can make the daemon exit by sending it (the same as SIGTERM: unregister the struct_ops, write no sysctl), so the
socket's file permissions are the entire access control.

The daemon **handles one request at a time**. `drain` waits within its own request until the existing connections end
or the timeout passes, and meanwhile other requests (including the `shutdown` that SIGTERM triggers) queue behind it;
`ssctl` has no timeout on its side. From 0.5.2 on each client has a 5-second deadline each for reading and for
writing, and a request line is at most 64 KiB: a client that connects and then does not finish sending a line (an
`nc -U` left open, a script that waits for the reply without writing a newline) is disconnected and no longer blocks
every request after it -- before, this blocked until systemd or OpenRC ended the daemon with SIGKILL, and SIGKILL runs no
cleanup, leaving the struct_ops in the kernel and the TC filters on the NIC. A request the daemon cannot decode (for
example a new `ssctl`'s command sent to an old daemon) gets an `ok: false` reply instead of being disconnected outright.
The socket file's permissions are fixed at `0600` and no longer depend on the daemon's umask at startup; the daemon
removes the file when it exits normally.

**A failed request does not change the configuration** (from 0.5.2 on): when `enable`, `disable`, `set-*` or `reset-*`
fails partway, the configuration and generation number in the daemon's memory return to what they were before the
request, and what `status` shows is what the kernel is using right now. The one exception is an `enable` that fails
only after it has already attached or sent the configuration (for example the sysctl write is refused): the kernel is
then already using the new configuration, and `status` reports it as it is.

The response is always:

```rust
struct Response {
    ok: bool,           // whether the request succeeded
    message: String,    // human-readable description of the result; the error message on failure
    status: Option<RuntimeStatus>,  // most requests carry the latest complete status
    flows: Option<FlowReport>,      // carried only by the flows request, see section 4.1
}
```

The `flows` field is optional on the wire protocol (serde `default`), so a new `ssctl` can decode an old daemon's reply
that lacks it, and an old `ssctl` can also decode a new daemon's reply. The same goes for `version`, `guard`,
`uptime_s`, `attached_s`, `redundancy`, `metrics_recent`, `draining` and `cgroup_path` in `RuntimeStatus`, and for
`guardrail_rounds` and `cwnd_cap_hits` in `metrics` (an old daemon's `redundancy` decodes as not in effect, `draining`
as `false`, and missing counters as 0).

## 4. Status and counter fields (the output structure of `ssctl status --json`)

`RuntimeStatus` is the complete structure of the `status` field in the replies to `status` and to most write
operations. The default readable report is one view of it, not all of it -- for every field, add `--json`
(section 2.1):

| Field | Meaning |
|---|---|
| `version` | Version of the running `skyline-speederd` (the crate version at compile time). Decodes as an empty string when it comes from an old daemon without this field |
| `enabled` | Whether the `skyline_cc` struct_ops is currently registered |
| `generation` | Configuration generation counter; the double-buffer switching mechanism is described in `docs/03-design.md` section 11 |
| `modules` | The subset of M1-M4 modules currently enabled |
| `fallback_cc` | Name of the congestion control algorithm new connections use when skyline_cc is not enabled or has been removed |
| `active_flows` | Number of connections currently using `skyline_cc` |
| `tc_stats` | Packets, bytes and GSO packets the TC program has seen (`packets`/`bytes`/`gso_packets`; `gso_packets` counts skbs with `gso_segs > 1`, not segments). The TC program always fails open and never drops a packet |
| `metrics` | The M2/M3/M4 decision counters (see the table below), **accumulated since this attach**. `null` when no struct_ops is loaded: never enabled, after a drain completes, after a daemon restart; every re-attach counts from zero |
| `metrics_recent` | The increase in `metrics` over roughly the last minute: `seconds` (the number of seconds actually covered) and `metrics` (the increase of the same set of counters over that time). While attached, the daemon reads the counters every 5 seconds and takes the difference between the current value and the reading closest to 60 seconds ago; when attached for less than a minute it covers back to the earliest reading. Use it to tell "still happening now" from "happened at some point since the attach". `null` when not attached, when just attached with no reading yet, or when it comes from a daemon older than 0.5.0 |
| `rack_tuning` | The "declared in configuration" and "currently actual" values of the M1 tier-1 global sysctls, side by side |
| `rack_rto` | M1 tier-2: `config` is the configuration `skyline_policy` is using right now, always off after the daemon starts until `set-rack-rto`/`reset-rack-rto`; `declared` is the `[rack_rto]` in the configuration file, i.e. the value `reset-rack-rto` would send; `stats` are the runtime counters (see the `rack_rto.stats` table below). 0.5.0 and earlier have no `declared` (it decodes as `null`), and `config` reports the file's value even when the BPF side is off |
| `retransmit_dscp` | DSCP marking: `config` is the configuration the TC program is using right now, always off after the daemon starts until `set-retransmit-dscp`/`reset-retransmit-dscp`; `declared` is the `[retransmit_dscp]` in the configuration file, i.e. the value `reset-retransmit-dscp` would send; `stats` are the runtime counters (see the `retransmit_dscp.stats` table below). 0.5.0 and earlier: as for `rack_rto` |
| `redundancy` | First-flight redundancy: `config` (the `[redundancy]` in the daemon's memory), `active` (whether the TC program is duplicating right now: skyline_cc is enabled, `enabled = true` and the TC program is loaded, all three at once), `stats` (see the table below; `null` when the TC program is not loaded) |
| `module_tuning` | A complete echo of the M2/M3/M4 coefficients currently in effect |
| `capabilities` | Kernel capability probe results; see "Capability probes" below |
| `guard` | State and counters of the cc/qdisc guard; see the `guard` table below. Decodes as all defaults (not armed, counts at 0) when it comes from an old daemon without this field |
| `uptime_s` | Seconds since the current `skyline-speederd` process started; 0 when it comes from an old daemon without this field |
| `attached_s` | Seconds since this struct_ops attach; `null` when not attached. An `enable` that only pushes new coefficients without reloading does not reset it |
| `draining` | A `drain` has handed new connections to `fallback_cc`, disarmed the guard and stopped first-flight redundancy, and is waiting for skyline_cc's existing connections to end: this is the state a timed-out drain leaves behind (a drain run over SSH always does). Returns to `false` once the struct_ops is unregistered or after another `enable`; `false` when it comes from a daemon of 0.5.1 or earlier (from 0.5.2 on) |
| `cgroup_path` | `runtime.cgroup_path`: the cgroup `skyline_policy` is attached to; the cgroup hints in the reports are written from it. `null` when it comes from a daemon of 0.5.1 or earlier, in which case `ssctl` shows the template default `/sys/fs/cgroup/skyline-speeder` (from 0.5.2 on) |

`metrics` (`SkylineMetrics`, per-CPU counters summed):

| Field | Meaning |
|---|---|
| `ack_events` | Number of ACKs processed |
| `delivered_packets` | **Not a packet count**: it adds `rate_sample.delivered` once per ACK, and the sampling intervals of neighbouring ACKs overlap, so it is about two orders of magnitude higher than the number of packets actually delivered (measured on 6.12.63: over the same 20 seconds it rose by 16,703,016 while the kernel's own `tp->delivered` rose by only 164,416, about 101 times). **Only usable to compare two runs of the same shape** (that is how the experiment matrix uses it; it is deliberately kept out of `RUN_FIELDS` in `analyze_results.py`), not as a traffic volume, and not as the denominator of any ratio. For the real volume look at the CONNECTIONS section of `ssctl flows`, which shows the kernel's byte counters |
| `loss_events` | Number of loss events detected. Several RACK marks within one round count once (de-duplicated by whether `tp->delivered` advanced), so each ACK adds at most 1 and it can be divided directly by `ack_events` |
| `state_transitions` | Number of STARTUP→CRUISE state switches |
| `pacing_updates` | Number of times the pacing rate was recalculated |
| `guardrail_hits` | Number of **rounds** in which the queue-delay/ECN guardrail fired (`flow->queue_clamped` set): at most once per flow per round, so divide it by `guardrail_rounds`, not by `ack_events`. Each firing also writes a type 9 event (section 8). In **0.4.x and earlier** this counter also included what is now `cwnd_cap_hits`: 0.4.x's `guardrail_hits` = 0.5.0's `guardrail_hits + cwnd_cap_hits`; a non-zero `guardrail_hits` with `guardrail_rounds` at 0 comes from such an old daemon |
| `hypothetical_early_loss` | An observation counter that increments in step with every `loss_events` count while the `early-loss` module is on -- it only counts and drives no decision (see `docs/03-design.md` section 5) |
| `prr_adjustments` | Number of times the framework-level PRR reimplementation intervened -- with M2 on this should always be 0 (M2 bypasses this path); non-zero means the bypass logic is not working |
| `guardrail_rounds` | Number of rounds in which the guardrail was checked: counted once each time a flow with M2 on enters a new round; the denominator of `guardrail_hits`. With M2 off the guardrail is not checked and neither counter moves |
| `cwnd_cap_hits` | Number of ACKs at the end of whose processing cwnd sits at `max_cwnd_packets` (or higher): counted per ACK, so the denominator is `ack_events`. This is the cap at work, not a congestion signal -- a flow whose BDP × gain exceeds the cap counts once on every ACK during that time |

`rack_rto.stats` (M1 tier-2, the counters of `skyline_policy`):

| Field | Meaning |
|---|---|
| `established_cb` | Number of new connections that passed through `skyline_policy` (*connections seen* in `ssctl flows`), accumulated since the daemon started; counted with dynamic RTO off as well. 0 means no process has yet established a connection inside `runtime.cgroup_path` (default `/sys/fs/cgroup/skyline-speeder`) -- this is the value that tells you whether the cgroup is taking effect. From 0.5.2 on `ssctl flows` always shows this line: in the `CGROUP POLICY (skyline_policy)` section with dynamic RTO off, in the `DYNAMIC RTO (skyline_policy)` section with it on; before, it was shown only after dynamic RTO had been turned on, and from 0.5.1 on dynamic RTO is off every time the daemon starts |
| `subscribe_ok` / `subscribe_err` | With dynamic RTO on, the number of times subscribing a new connection to the RTT callback succeeded / failed; with it off neither moves |
| `rtt_callbacks` | Number of RTT callbacks received with dynamic RTO on; does not move with it off (it is off after every daemon start, section 2.4) |
| `skipped_warmup` | Number of skips: the first `warmup_samples` RTT samples of each connection are only observed, nothing is sent |
| `applied` / `rejected` / `unchanged` | The `TCP_BPF_RTO_MIN` floor: sent successfully / rejected by the kernel / skipped because it equals the value already sent for that connection |
| `rto_max_applied` / `rto_max_rejected` / `rto_max_unchanged` | The same set of counts for the `TCP_RTO_MAX_MS` ceiling. Kernels < 6.15 do not have this option, and there you will only see `rto_max_rejected` grow |
| `rto_max_congested` | Number of times the ceiling calculation above switched to the "congested" multiplier because of ECN marks or queueing delay (diagnostics only) |

`retransmit_dscp.stats` (the counters of `skyline_tc`). With marking off the whole marking path does not run, and the
counters below (except `abi_mismatch`) stay where they were; turning marking off does not reset them:

| Field | Meaning |
|---|---|
| `packets_seen` | Number of IPv4/IPv6 TCP packets the program actually parsed (for IPv6, only those whose extension-header chain reached the TCP header) |
| `retransmits_detected` | The retransmitted packets among them (`end_seq <= snd_nxt`, excluding pure ACKs and probes) |
| `retransmits_marked` | Retransmitted packets sent out carrying `dscp_value`; should always equal `retransmits_detected`, and if it does not, the marking path has a problem |
| `csum_fixups` | The subset of `retransmits_marked` where the IPv4 header checksum was actually rewritten (a proxy metric for CPU cost; IPv6 has no header checksum and never counts here) |
| `ipv6_marked` | The subset of `retransmits_marked` sent over IPv6 |
| `ipv6_chain_bailout` | IPv6 packets given up on before reaching the TCP header: those that are not TCP (ICMPv6/ND, UDP/QUIC, etc., so it is never 0), and those whose extension-header chain exceeds the unroll limit, meets a fragment/AH/ESP/unknown extension header, or exceeds the length budget. All of them are passed through, never dropped; worth a look only when it is abnormally high relative to IPv6 TCP traffic |
| `abi_mismatch` | The configuration slot's ABI version does not match the BPF object; the whole feature silently does not work |

Capability probes (`CapabilityReport`, probed once when `skyline-speederd` starts):

| Field | Meaning |
|---|---|
| `kernel_release` | `uname -r` |
| `btf` / `bpffs` / `cgroup_v2` / `fq_available` / `struct_ops` | Hard prerequisites: if any is `false`, `--validate-only`, `ssctl validate` and `ssctl enable` refuse outright (the `skyline-speederd` process itself still starts normally). `fq_available` means the kernel has `sch_fq`: built in, loadable, loaded, or already the default qdisc; from 0.4.4 on this is decided by reading the kernel's module index instead of asking `modinfo` (busybox's `modinfo` returns success even for a module that does not exist) |
| `fallback_cc_available` | Whether the kernel can provide the algorithm `fallback_cc` names: it is registered (built in, or its module is loaded), or a module `tcp_<name>` exists -- the kernel loads it itself when root writes this sysctl (xanmod builds cubic as a module and registers only reno and bbr at boot). Also a hard prerequisite: when `false`, the same three places refuse. When the module was not yet loaded at daemon startup, `notes` says so; when the algorithm is unavailable, `notes` lists the algorithms registered at startup (except `skyline_cc`: drain writes the fallback precisely before unregistering it). Before 0.4.3 only registered algorithms were accepted |
| `rack_reo_hook` | Probes whether a hook specific to one kernel patch exists; always `false` on a standard upstream kernel; recorded only, not part of the thresholds above |
| `notes` | Free text recording soft degradations (for example a feature reduced to pure observation because a kernel capability is missing; with `[guard] qdisc = true`, `tc_interface` set and no `tc` (iproute2) on the host, it also notes here that the guard cannot maintain the root qdisc of that NIC (or of the physical NIC under it); when `tc_interface` is not an Ethernet device it says `TC interface <nic> is not an Ethernet device (type N, e.g. a tunnel); skyline_tc is not attached`, see section 6.7). `TC runtime unavailable: <reason>` gives the reason the TC program is not loaded |

`struct_ops` and `rack_reo_hook` are derived by parsing `/sys/kernel/btf/vmlinux` directly inside the
`skyline-speederd` process, **without depending on bpftool** (a `--prebuilt` host does not have it in the first place):
`struct_ops` checks whether the kernel BTF contains the struct `bpf_struct_ops_tcp_congestion_ops` -- exactly the type
libbpf resolves when it loads `skyline_cc`; `rack_reo_hook` checks whether some struct has a function-pointer member
named `rack_reo_wnd`. When the BTF file exists but fails to parse, both are `false` and the reason goes into `notes`.

`guard` (`GuardStatus`, behaviour in section 9):

| Field | Meaning |
|---|---|
| `armed` | Whether the guard is currently active: `true` after a successful `enable`, `false` after a `drain`. Always `false` when the daemon starts -- even if a `skyline_cc` left behind by a previous instance that was killed is still registered, only an `enable` in this process can arm it |
| `interval_s` / `qdisc` | Echo of the `[guard]` configuration (section 6.8) |
| `checks` | Number of periodic checks run while armed (the one `enable` runs itself is not counted) |
| `cc_restored` | Number of times `tcp_congestion_control` was changed back to `skyline_cc` |
| `default_qdisc_restored` | Number of times `net.core.default_qdisc` was changed back to `fq` (including the one at `enable`) |
| `interface_qdisc_replaced` | Number of times the root qdisc of a managed NIC (`tc_interface`, or the physical NIC under it, see `live.devices`) was replaced, summed over NICs (including the one at `enable`) |
| `last_correction` / `last_correction_unix_s` | Description of the most recent change (such as `tcp_congestion_control bbr -> skyline_cc`) and its Unix time (seconds) |
| `last_error` | The most recent failure (no `tc`, `tc` timed out or a previously killed `tc` has not exited yet, a write refused, `default_qdisc` is not `fq` so no new `mq` is created, still not `fq` after the replacement, etc.; see section 9); kept until the next failure, so it is history and does not mean it is still failing |
| `live` | Values read live when status is read, regardless of whether the guard is armed: `tcp_congestion_control`, `default_qdisc`, `interface` (i.e. `tc_interface`), `interface_qdisc` (a summary of that NIC's own root qdisc: `fq`, `cake`, `mq/fq`, `mq/cake,fq` -- after `mq/` come the qdisc kinds that appear among its child queues, de-duplicated and in alphabetical order; `noqueue` on a VLAN, bond or bridge; `null` when there is no NIC or it cannot be read), `devices` (the NICs the guard actually maintains, each item `{"name": ..., "qdisc": ...}`, with `qdisc` in the same format as `interface_qdisc`, and `null` when it cannot be read or the NIC is down; this is `tc_interface` itself, or, when that is a VLAN, bond or bridge, the physical NICs under it, see "Managed NICs" in section 9. An empty list when `[guard] qdisc = false`, or when `tc_interface` does not exist or no NIC is found under it; also empty when it comes from an old daemon without this field) |
| `notes` | What the most recent check deliberately left alone and what it did not manage to do, for example `eth0 root qdisc htb looks deliberate; left alone`, `eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone`, `eth0 (under bond0) root qdisc cake: the last replace failed; retrying in 30 s`, `wg0 is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked` |

`redundancy.stats` (summed over CPUs):

| Field | Meaning |
|---|---|
| `packets` | Number of copies sent (a GSO super-packet counts as one) |
| `bytes` | Sum of `skb->len` over those copies: the payload counts in full and a GSO super-packet's headers count only once, so it is slightly less than the extra bytes actually put on the wire |
| `handshakes` | The number of SYN-ACKs and SYNs among `packets` |
| `clone_failed` | Number of times `bpf_clone_redirect()` failed and no copy was sent; should always be 0 |
| `abi_mismatch` | Number of times the configuration slot's ABI version did not match the BPF object and the whole section was skipped |

### 4.1 Per-flow data from `ssctl flows`

The reply to `ssctl flows --json` carries one more field besides `status`, `flows` (`FlowReport`); replies to other
requests do not have this field -- enumerating sockets takes an extra run of `ss`, and only the command that asks for it
pays that cost.

| Field | Meaning |
|---|---|
| `accelerated` | Connections running on `skyline_cc`, in descending order of `bytes_sent`, at most 50 (`FLOW_ROWS_MAX`) |
| `tcp_total` | Total number of TCP sockets on the host in a connected state (excluding listening ones) |
| `on_skyline_cc` | How many of them use `skyline_cc` as their congestion control. **Not affected by the 50-row cap**; always the real number |
| `truncated` | Number not listed because of the cap |
| `source_error` | Why enumeration failed (`ss` not installed, killed on timeout, output format not recognised). The first four fields are then empty/0, but the parts of the reply that come from the daemon (coefficients, counters) are unaffected |

Fields of each `accelerated` entry: `peer`, `local`, `state`, plus `rtt_ms`, `rtt_var_ms`,
`min_rtt_ms`, `cwnd_packets`, `ssthresh`, `pacing_bps`, `delivery_bps`, `send_bps`,
`bytes_sent`, `bytes_acked`, `bytes_retrans`, `retrans_total`, `unacked`, `mss`,
`rto_ms`. All except the first three are nullable: `ss` prints only the keys the kernel has a value for, and a missing
one is rendered as `-`, not as a 0 that looks like a measurement.

**Where these numbers come from, and their limits.** They are **the kernel's own** `tcp_info`, read back by the daemon
by running `ss -tin` (`iproute2` is a hard dependency anyway, since the guard runs `tc`), then filtered by congestion
control name. `skyline_cc`'s per-flow state lives in `flow_states`, a `BPF_MAP_TYPE_SK_STORAGE`; socket storage is
addressed by socket and does not support `bpf_map_get_next_key`, so userspace cannot iterate it without an fd for each
socket -- which is also why 0.2.0 could only return a count. But cwnd, pacing rate and RTT are exactly the values
`skyline_cc` writes into the socket, so reading them back from the kernel side is a faithful record of "what the
acceleration did". What `ss` cannot see is not here: the mode (STARTUP/CRUISE), the bandwidth estimate and the
guardrail state exist only as aggregates, from the BPF counter map.

## 5. ABI versions

The four configuration sections on the BPF side each maintain their own ABI version number, independently of each
other -- their lifecycles differ (the M2/M3/M4 coefficients change with almost every round of tuning, while M1
tier-2/DSCP marking/first-flight redundancy change only when the feature itself changes), and merging them into one
version number would force most changes to drag the other sections along:

| Constant | Covers |
|---|---|
| `SKYLINE_ABI_VERSION` | The M2/M3/M4 coefficient structure (`KernelConfig`) and the decision counters (`SkylineMetrics`) |
| `SKYLINE_RTO_TUNING_ABI_VERSION` | The M1 tier-2 dynamic RTO structure (`KernelRtoTuning`) |
| `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` | The DSCP marking structures (`KernelRetransmitDscpConfig`/`KernelRetransmitDscpStats`) |
| `SKYLINE_REDUNDANCY_ABI_VERSION` | The first-flight redundancy structures (`KernelRedundancyConfig`/`RedundancyStats`) |

The sections do not all behave the same way on a version check:

- M2/M3/M4 (`SKYLINE_ABI_VERSION`): from 0.5.2 on, before `skyline-speederd` loads `skyline_cc.bpf.o` it first reads
  the version the object records itself (`skyline_abi_version` in the `.rodata.abi` section); if that differs from its
  own, or the object has no such section (objects built by 0.5.1 and earlier), it refuses to load and says why --
  `ssctl enable`, `--validate-only --verify-bpf` and the installer's verification step all report this error. On the
  object side, every connection still checks the version number when it copies the configuration
  (`skyline_adopt_config()`); on a mismatch it does not copy, and that connection has no configuration -- which only
  happens when an old daemon that does not run the check above has loaded a new object. This does not hand the
  connection back to the kernel: `cong_control` has already replaced the kernel's own growth logic. From 0.5.2 on such
  a connection grows its window the way the kernel's Reno does (before, it stayed at the kernel's initial window, and
  after an RTO at the size `tcp_enter_loss()` left), nobody sets pacing, and no counter records this (`active_flows`
  rises while `metrics.ack_events` does not move). If the size of `struct skyline_config` changed, the daemon reports
  an error when it writes the configuration slot. `metrics` is read by layout; per-CPU values whose size does not
  match are skipped, and the counters are all 0.
- DSCP marking (`SKYLINE_RETRANSMIT_DSCP_ABI_VERSION`): on a version mismatch the marking logic is skipped
  (equivalent to the feature being off), and the `abi_mismatch` counter in `retransmit_dscp.stats` is
  incremented.
- First-flight redundancy (`SKYLINE_REDUNDANCY_ABI_VERSION`): the same as DSCP marking -- on a mismatch it does not
  duplicate, and increments `abi_mismatch` in `redundancy.stats`.
- M1 tier-2 dynamic RTO (`SKYLINE_RTO_TUNING_ABI_VERSION`): from 0.5.2 on `skyline_policy` checks it, and on a
  mismatch treats the configuration as absent -- new connections do not subscribe to the RTT callback, and those
  already subscribed unsubscribe on their next RTT sample, the same as with the feature off; there is no
  corresponding counter. 0.5.1 and earlier do not check it, and on a mismatch read the structure misaligned.

This only happens when `skyline-speederd` and the `.bpf.o` it loads are not from the same build; a normal deployment
(`skyline-speederd`/`ssctl`/BPF objects from the same `make`) never meets it.

## 6. Configuration file field reference

The configuration file is TOML; its path is given with `skyline-speederd --config <path>`. The sections take effect in
different ways:

- Top-level coefficients, `[adaptive_cwnd]`/`[loss_classifier]` and `enabled_modules`: when the daemon starts they are
  only read into memory, and `skyline_cc` is not loaded; when `ssctl enable` attaches it, what is sent is the current
  value in the daemon's memory (read from the file at startup; later changes to the file do not take effect; changes
  made with the commands below before the attach or after a `drain` are kept). After the attach, the coefficients can
  be overwritten online with `ssctl set-module-config`/`reset-module-config`, and the modules with
  `ssctl enable --modules`/`disable --module`;
- `[rack_rto]` and `[retransmit_dscp]`: when the daemon starts, the BPF side is initialised to off, and the file's
  values are sent only once `reset-rack-rto`/`reset-retransmit-dscp` has been run (`set-*` sends the values given on
  the command line); until then the `config` of these two sections in `ssctl status` shows off, the file's values are
  in `declared`, and ATTENTION points this out (in 0.5.0 and earlier `config` showed the file's values, which did not
  mean they were in effect);
- `[redundancy]`: when the daemon starts it is likewise read into memory, and the BPF side is initialised to off; after
  a successful `ssctl enable` it is sent from the in-memory values (section omitted entirely = on), and `ssctl drain`
  turns it off right at the start; `set-redundancy`/`reset-redundancy` change it online;
- `[runtime]`, `[rack_tuning]`, `[guard]` and `fallback_cc`: there are no online commands; after changing them, restart
  `skyline-speederd`.

Apart from the one exception noted above, the values actually in effect at run time are the ones `ssctl status` /
`ssctl flows` report.

**Unknown keys are rejected (from 0.5.0 on).** When the file has keys the daemon does not know, `--validate-only` and
daemon startup both fail, and the error lists each of these keys
(`guardrail_gain at the top level (it is a key of [adaptive_cwnd])`, `table [gaurd]`); when a key belongs to another
section it says which section, and when it belongs to several sections it lists them all
(`enabled in [guard] (it is a key of [rack_rto], [redundancy] or [retransmit_dscp])`; 0.5.1 and earlier named only the
first, and gave no hint when an optional key such as `tc_interface` was in the wrong place). Keys in the wrong section or with misspelled names are therefore no longer
silently ignored. Daemons of 0.4.x and earlier silently ignore them; when downgrading to those versions, extra keys in
the file have no effect; when downgrading to 0.5.0 or newer, first remove the keys the error lists.

### 6.1 Top-level fields

| Field | Type | Description | Online override |
|---|---|---|---|
| `generation` | u32 | Configuration generation counter, must be non-zero; incremented automatically by every `enable`, `disable`, `set-`/`reset-module-config` and `set-`/`reset-rack-rto` (the DSCP marking and first-flight redundancy commands do not increment it: their configurations carry no generation number). The BPF side uses it to mark which generation of the configuration each connection is using (the old and new generations in type 11 events); preventing reads of half-old, half-new configuration relies on a separate sequence number, see `docs/03-design.md` section 11 | No (maintained internally) |
| `enabled_modules` | string[] | The subset of M1-M4 modules enabled at startup, with values `early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing` | Yes (`ssctl enable`) |
| `prr_pacing_enabled` | bool | Switch for the framework-level PRR reimplementation, independent of the four modules above -- in effect by default even when `enabled_modules` is empty (it corrects the problem that "bypassing the kernel's PRR leaves the baseline no longer like-for-like"; it is not an optional feature). With M2 on this path is never executed | Yes (`set-module-config`) |
| `auto_pacing_enabled` | bool | The pacing rate cap used when M4 is off, equivalent to the kernel's default behaviour; an independent switch, with the same semantics as above | Yes (`set-module-config`) |
| `fallback_cc` | string | Name of the congestion control algorithm new connections use when `skyline_cc` is not enabled or has been removed (written before every attach by enable, by `ssctl drain` and by the enable unit's ExecStop); must be an algorithm registered in the kernel, or a module `tcp_<name>` the kernel can load automatically, and must not be `skyline_cc` (drain writes it first and then unregisters skyline_cc; rejected from 0.5.2 on). The template value is `cubic`; `install.sh` substitutes the algorithm chosen by the boot-time sysctl configuration, in a configuration it creates and on every later run while the value is still its own choice -- a value set by hand is kept (see `DEPLOY.md` section 2.4) | No |
| `max_pacing_mbps` | u64 | Hard cap on the pacing rate (Mbps); must not be 0 when the `pacing` module is enabled | Yes |
| `max_cwnd_packets` | u32 | Hard cap on cwnd (packets), minimum 4 | Yes |
| `max_queue_delay_ms` | u32 | Fixed base value of the queue-delay guardrail (milliseconds) | Yes |
| `max_queue_delay_ratio` | f64 | Guardrail increment as a ratio of the base RTT; the actual guardrail = `max(max_queue_delay_ms, base RTT × this ratio)`, so that the guardrail widens automatically on high-RTT paths instead of sticking at a fixed number of milliseconds; `0.0` = no widening, always equal to `max_queue_delay_ms`. Must be a non-negative finite number (`nan`/`inf` rejected from 0.5.2 on) | Yes |
| `initial_cwnd_packets` | u32 | Initial cwnd at connection setup (packets); `0` = do not override, use the kernel's own initial window; must not exceed `max_cwnd_packets` (from 0.5.2 on: the initial window is written before any ACK passes through the cap) | Yes |
| `min_cwnd_packets` | u32 | Lower bound of M2's cwnd target (packets), range `4`–`max_cwnd_packets`; the shipped template has `32`, omitted = `4` (the historical behaviour). It is also the cwnd floor after an RTO for a flow that has no bandwidth estimate yet -- a short flow whose whole response goes out in the first round usually never gets an estimate, and after an RTO the kernel leaves it only 1 packet. Takes effect only when `adaptive-cwnd` is on; the neutral-baseline path with M2 off always uses a fixed floor of 4 packets and is not affected by this field. The sending rate is still set by pacing; see `docs/03-design.md` section 6 | Yes |

### 6.2 `[adaptive_cwnd]` (M2 coefficients)

| Field | Type | Description |
|---|---|---|
| `min_rtt_window_s` | u32 | Length of the minimum-RTT sliding window (seconds), range 1-4294 (from 0.5.2 on: `0` makes the base RTT always equal the current RTT, so the queue-delay guardrail never sees any queueing; the kernel side stores it in 32-bit microseconds) |
| `bw_window_rtts` | u32 | Length of the bandwidth estimate window: the number of rounds in which a sample entered the filter (a round is about two RTTs, `docs/03-design.md` section 3), range 1-10 |
| `startup_plateau_rtts` | u32 | Number of consecutive rounds needed to judge that bandwidth growth has hit a "plateau", which triggers the STARTUP→CRUISE switch; only rounds in which a sample entered the bandwidth filter count, and a round that is application-limited throughout does not (from 0.5.1 on) |
| `startup_growth_ratio` | f64 | Growth below this ratio counts as a plateau, range 0.0-1.0 |
| `startup_gain` | f64 | The single gain in STARTUP, applied to both the cwnd target and the pacing rate, range 1.0-1000 |
| `cruise_inflight_gain` | f64 | The cwnd-target gain in CRUISE, range 1.0-1000 -- deliberately looser than `cruise_pacing_gain`: cwnd only needs not to be the limiting factor, and the real rate limiter is pacing |
| `cruise_pacing_gain` | f64 | The pacing-rate gain in CRUISE (the rate limit actually in effect when M4 is on), range 1.0-1000 |
| `guardrail_gain` | f64 | The gain applied to both the cwnd target and the pacing rate when the queue-delay/ECN guardrail fires, range 0.0-1.0. `0.0` = guardrail slow-down not enabled (the guardrail only cancels acceleration, and the gain returns to the neutral 1.0); `(0.0, 1.0)` = actively slow down to that fraction of the measured bandwidth |

The floating-point fields in this section and the next must all be finite numbers: TOML can write `nan` and `inf`,
which on the BPF side become 0 and the u32 maximum respectively, and in the status JSON become a `null` that no reader
can decode; from 0.5.2 on they are all rejected. The three mode gains are capped at 1000 because the pacing path
multiplies the gain by M3's loss compensation (at most 2×) in 32 bits, and above about 2147 it wraps around to a very
small gain.

### 6.3 `[loss_classifier]` (M3 coefficients)

| Field | Type | Description |
|---|---|---|
| `loss_inflation_max_ratio` | f64 | Cap on the compensation for the measured loss rate, range 0.0-0.5. The cwnd/pacing targets are scaled up by `1/(1-measured loss rate)` to offset the underestimate of the "acknowledged rate" that loss causes, and this field caps that scale factor; `0.0` = no compensation, the factor is always 1.0 |

### 6.4 `[rack_tuning]` (M1 tier-1, global sysctl defaults)

All three fields are `Option<u32>`; leaving them empty or omitting the whole section means "`skyline-speederd` does not
take over these sysctls, and ownership is left entirely to others"; once a concrete value is declared,
`skyline-speederd` writes it to the corresponding `/proc/sys/net/ipv4/*` at startup, and if the write fails the daemon
fails to start. Production deployments usually omit the whole section and let the operating system defaults apply.

| Field | Type | Valid range |
|---|---|---|
| `tcp_recovery` | Option\<u32\> | Bit mask; only 1/3/5/7 are valid |
| `tcp_reordering` | Option\<u32\> | 1-300 |
| `tcp_early_retrans` | Option\<u32\> | 0-4 |

### 6.5 `[rack_rto]` (M1 tier-2, per-flow dynamic RTO)

| Field | Type | Description |
|---|---|---|
| `enabled` | bool | Whether to subscribe to `BPF_SOCK_OPS_RTT_CB` and apply the tuning below |
| `srtt_permille` | u32 | RTO floor target = `max(srtt_us, min_rtt_us) × this value / 1000` |
| `floor_us` | u32 | Absolute safety lower bound on the RTO floor (microseconds) |
| `ceiling_us` | u32 | Absolute safety upper bound on the RTO floor (microseconds); must not exceed the kernel constant `TCP_RTO_MIN` (200000) |
| `warmup_samples` | u32 | The first N RTT samples are only observed; no tuning is sent |
| `rto_max_normal_permille` | u32 | Without evidence of congestion, the `TCP_RTO_MAX_MS` ceiling = `clamp(this value × base RTT/1000, 1000ms, 120000ms)`; `0` = the whole ceiling-tuning feature off, and `TCP_RTO_MAX_MS` stays at the kernel default |
| `rto_max_congested_permille` | u32 | The multiplier used once there is evidence of congestion (a new CE mark, or srtt clearly above min_rtt); must be ≥ `rto_max_normal_permille` |
| `rto_max_congestion_ratio_permille` | u32 | When the ratio of srtt over min_rtt exceeds this value, that counts as evidence of congestion; `0` = use the built-in default (2000, i.e. 2×) |

With `enabled=true`, `floor_us`/`srtt_permille` must be non-zero, and `ceiling_us` must be
≥ `floor_us` and ≤ the kernel's `TCP_RTO_MIN`.

### 6.6 `[retransmit_dscp]`

| Field | Type | Description |
|---|---|---|
| `enabled` | bool | Whether to put a DSCP mark on retransmitted packets |
| `dscp_value` | u8 | DSCP code point, 0-63; must be non-zero when `enabled=true` |

### 6.7 `[runtime]`

| Field | Type | Description |
|---|---|---|
| `bpf_dir` | path | Directory containing the BPF object files |
| `pin_dir` | path | The current version does not read it and pins no objects (template value `/sys/fs/bpf/skyline-speeder`). Can be omitted from 0.5.2 on; required in 0.5.1 and earlier, so keep the line before downgrading |
| `cgroup_path` | path | Path of the cgroup the M1 tier-2 cgroup sockops program is attached to |
| `socket_path` | path | Path of the control-plane Unix socket |
| `state_path` | path | Path where the runtime state is persisted (not a configuration input). Rewritten only when the daemon starts and after each **successfully** handled request (a failed request, such as a timed-out drain, does not update it); it is not live state: for monitoring, call `ssctl status --json` |
| `events_path` | path | Path where the event stream is persisted |
| `events_max_mib` | u32 | Size cap of the event log (MiB), default `8`. Beyond it the log rotates, taking at most about twice as much; `0` turns the event log off. Also `8` when omitted; changing it requires a daemon restart. Details in section 8 |
| `tc_interface` | Option\<string\> | Name of the network interface the TC program (statistics + DSCP marking + first-flight redundancy) attaches to; when unset, none of the three is loaded. Must be an Ethernet device (`/sys/class/net/<nic>/type` is 1; VLANs, bonds and bridges are too): `skyline_tc` parses every packet as an Ethernet frame header at offset 0, so on layer-3 tunnels such as WireGuard/WARP, tun, gre and ppp, DSCP marking and first-flight redundancy would parse packets at the wrong offset (enabled DSCP marking would also write into the inside of the IP header; the packet and byte counts in `tc_stats` do not parse packets and are unaffected); the daemon therefore refuses to attach, `capabilities.notes` states the reason, and `set-retransmit-dscp` fails as a result -- on a host whose default route goes through a tunnel, put here the physical NIC that carries the tunnel traffic. It also decides which NIC's root qdisc the guard maintains (sections 6.8 and 9): this one, or, when it is a VLAN, bond or bridge, the physical NIC under it; when unset, the guard touches no NIC's root qdisc |

For a production example see `config/speeder-guest.toml` (`bpf_dir` points to the deployed absolute path
`/opt/skyline-speeder/bpf`; `[rack_tuning]` and `[rack_rto]` are left empty).

### 6.8 `[guard]`

What `skyline-speederd` keeps in place after `ssctl enable`, and how often it checks (behaviour in section 9). When the
whole section is omitted, the defaults in the table below apply -- an installed configuration file that was upgraded
from 0.2.0 and lacks this section likewise turns on the qdisc guard once the new daemon is restarted. Changing it
requires a daemon restart.

| Field | Type | Description |
|---|---|---|
| `interval_s` | u32 | Interval between periodic checks (seconds), default `5`, range `0`–`3600`. `0` = no periodic checks (no background thread); `ssctl enable` still runs one full check. The retry backoff after a failed replacement starts from max(`interval_s`, 30 seconds) (section 9) |
| `qdisc` | bool | Default `true`: besides the congestion control, also keeps `net.core.default_qdisc = fq` and the root qdisc of the managed NIC (`runtime.tc_interface`, or the physical NIC under it) in place. `false` = touch no qdisc; the guard keeps only the congestion control |

### 6.9 `[redundancy]`

First-flight redundancy (section 2.6; rationale in `docs/03-design.md` section 9). When the whole section is omitted,
the defaults in the table below apply, i.e. **on** -- an installed configuration file without this section is on after
an upgrade as well. The experiment matrix
(`research/experiments/run_matrix.py`) explicitly sends
`ssctl set-redundancy --disable` at the start of every case, unless the profile carries its own
`[profiles.redundancy]`: the data in `docs/04-performance-report.md` were all measured without copies.

| Field | Type | Description |
|---|---|---|
| `enabled` | bool | Default `true`. Takes effect only while skyline_cc is enabled; duplicates only the data segments and SYNs of skyline_cc connections, plus the SYN-ACKs this host sends |
| `first_kib` | u32 | Default `64`, range `1`–`1024` (not validated when `enabled = false`). Data segments whose start falls within this many KiB from the start of the connection's send stream are duplicated, and retransmissions within that range are duplicated too |
| `delay_ms` | u32 | Default `10`, range `0`–`100`. The copy is sent this much later than the original, held back by `fq` according to its EDT timestamp; `0` = right after the original. When the root qdisc is not `fq`, the copy goes out immediately |

## 7. Feature mask bit table

The `skyline_cc` module switches are a 32-bit mask on the BPF side:

| Bit | Name | Meaning |
|---|---|---|
| 0 | `EARLY_LOSS` | M1 early loss detection (an observation channel, see `docs/03-design.md`) |
| 1 | `ADAPTIVE_CWND` | M2 adaptive cwnd |
| 2 | `LOSS_CLASSIFIER` | M3 loss-rate compensation |
| 3 | `PACING` | M4 active pacing |
| 4 | `PRR` | Framework-level PRR reimplementation, an independent switch (see `prr_pacing_enabled`) |
| 5 | `AUTO_PACING` | The equivalent pacing cap when M4 is off, an independent switch (see `auto_pacing_enabled`) |

## 8. Event codes and counters

`skyline-speederd` collects decision events through a BPF ring buffer and persists them to the JSON Lines file
`events_path` points to, one event per line:

```json
{"socket_cookie":123,"state":1,"timestamp_ns":456,"type":2,"value_a":3,"value_b":0}
```

Each line is compact JSON with its keys in alphabetical order (match on `"type":2`, without spaces). `type` is the event
code (`type` in `struct skyline_event` on the BPF side; called `event_type` in the Rust mirror structure), and `state` is
the mode when the event happened: `SKYLINE_MODE_STARTUP` (0) or `SKYLINE_MODE_CRUISE` (1).

| `type` | Name | `value_a` | `value_b` |
|---|---|---|---|
| 1 | STATE | Mode before the switch | Mode after the switch (the same as the `state` field) |
| 2 | LOSS | Number of losses in this sample | Reserved, always 0 |
| 9 | GUARDRAIL | Queueing delay when it fired (microseconds). `value_a` ≤ `value_b` (or `value_b` is 0, i.e. the queue check is off) means this one was triggered by a new ECN CE mark | Guardrail threshold (microseconds) |
| 10 | UNDO_CWND | Restore-target cwnd | cwnd actually returned to the kernel |
| 11 | GENERATION_SWITCH | Configuration generation number before the switch | Configuration generation number after the switch (however many times configuration was sent within one round, it switches straight to the latest generation) |

(The numbering is not contiguous: 3-8 are reserved gaps that the current implementation never produces and that
consumers need not handle; consumers can rely on the numbers 1/2/9/10/11 staying fixed.)

Over the same period, the number of type 9 events equals the increase in `metrics.guardrail_hits`, and type 2 and
type 1 correspond to `loss_events` and `state_transitions` respectively -- provided no events were lost: when the ring
buffer is full, new events are dropped, and when the file rotates two or more times the events in between are lost as
well (see below).

**Size cap and rotation.** `events_path` is under `/run` by default, and `/run` is a tmpfs that counts against memory,
shared with systemd, Docker (runc state) and others. On hosts with many connections and frequent loss or queueing the
event volume is large: early versions set no cap and once filled `/run` completely, so Docker could not write its runc
state files, while this service itself reported no error. Now, when appending a line would make the file exceed
`events_max_mib`, the daemon first renames it to `<events_path>.1` (overwriting the previous one) and then creates a new
file to continue writing. Rotation happens only between lines, and the log takes at most about `2 × events_max_mib`.
An external truncate (`truncate -c -s 0`) frees the space immediately, and the daemon continues writing from the start
of the file; a plain `rm` does not cause an error either, but the deleted file is still held open by the daemon, and
the space is freed only when it reaches the cap and switches to a new file. With `events_max_mib = 0` the file is not
opened and the ring buffer is not read; kernel-side events are dropped once the ring buffer is full.

To extract the events of an interval, record a cursor (`INODE:LINES`) with `infra/snapshot-skyline-events.sh cursor`,
and afterwards use `since <cursor>` to get every event after it. The script can stitch `.1` and the current file
together across one rotation; across two or more it warns on stderr that events in between were lost.

## 9. The congestion control and qdisc guard

**Why it is needed.** The "one-click BBR" scripts common on VPSes write `net.ipv4.tcp_congestion_control=bbr` and
`net.core.default_qdisc=cake`/`fq_pie` into `/etc/sysctl.conf` or `/etc/sysctl.d/*.conf`. At boot, systemd-sysctl
applies them first, and then `ssctl enable` changes the congestion control back to `skyline_cc`; but after that,
anything that re-runs `sysctl --system`/`sysctl -p` silently flips the congestion control back to `bbr` -- from then on
every new connection bypasses `skyline_cc`, with no error at all. In addition, `default_qdisc` affects only qdiscs
created **afterwards**: a NIC's root qdisc is often created before this sysctl takes effect (measured on a fresh
Debian 13 host: `/etc/sysctl.d` says `fq`, yet `eth0` is still `fq_codel`), so changing only the sysctl does not reach
the NIC; the root qdisc has to be replaced.

**Ownership.** From a successful `enable` to the next `drain`, the following have only one writer, `skyline-speederd`:

- `net.ipv4.tcp_congestion_control` = `skyline_cc` (every write of this sysctl by the daemon goes through one function;
  before `enable` and after `drain`, the fallback in `infra/boot-disable.sh`, the experiment matrix's
  `apply-guest-profile.sh` and `install.sh --uninstall` also write it);
- `net.core.default_qdisc` = `fq`, and the root qdisc of the managed NIC (usually `runtime.tc_interface` itself, see
  "Managed NICs" below) = `fq` (on a multi-queue NIC, an `mq` whose child queues are all `fq`) -- only when
  `[guard] qdisc = true`. `infra/boot-enable.sh` used to write `default_qdisc=fq` once at boot;
  it no longer does.

**Arming and disarming.** The daemon does not arm the guard at startup; `enable` arms it after attaching and switching
the default algorithm, and checks once immediately; `drain` first disarms it and writes `fallback_cc` back under the
same lock, so a periodic check cannot write `skyline_cc` back after the drain's write; when a drain times out with an
error, the guard likewise stays disarmed (the experiment matrix installs its own qdisc after `drain --timeout 0`, and
the guard must not fight it). When the daemon stops, it first stops the guard thread and waits for it to finish, then
unregisters the struct_ops, and **writes no sysctl** (it does not write `fallback_cc` back; this is deliberate; to
write it back, run `ssctl drain` first).

**Managed NICs.** When the root of `runtime.tc_interface` is not `noqueue`, the managed NIC is that interface itself.
When it is `noqueue` -- the kernel's default for VLAN, bond, bridge, macvlan, veth and tunnel devices; such devices do
not queue themselves, packets really queue at the root qdisc of the physical NIC underneath, and that is also where a
one-click script's `cake` ends up -- the guard walks down `/sys/class/net/<nic>/lower_*`:

- at most 4 levels (a VLAN on a bond in a bridge is 3 levels), sorted by name, looking at each device only once;
- a lower device whose root is still `noqueue`: keep walking down;
- root is some other qdisc and there is a `/sys/class/net/<nic>/device` link (a physical or virtio NIC: a bond's member
  NIC, a VLAN's real NIC, a bridge's physical port): managed; an intermediate device whose root is not `noqueue` but
  that has no `device` link (for example a bond under a bridge with its own `htb` installed) is not managed;
- ports with no `device` link and no device under them (a VM's tap, a container's veth, ifb) are never managed, and
  the guard does not even run `tc` on them: the VM and container ports on a bridge are not this host's egress.

When none is found (for example a kernel WireGuard device, or a bridge with only taps on it), it records just one note:
`<nic> is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked`.
tun and PPP devices (OpenVPN, the Cloudflare WARP client, wireguard-go/Tailscale, PPPoE) are different:
they have their own default qdisc (`fq_codel`, `pfifo_fast`, etc.), not `noqueue`, so the guard manages the device
itself as it would a NIC and replaces an unshaped root with `fq` (that is where the inner TCP's packets queue); the
physical NIC under such a device has no `lower_*` link, and the guard cannot reach it. `skyline_tc` still does not
attach to these non-Ethernet devices.
Exception: a NIC that has a `device` link and no other device under it, and whose root is `noqueue` -- the kernel never
gives a physical NIC that default, so someone must have set it -- is managed, and treated as a deliberate setup (left
alone, with a note). In corrections and notes a managed NIC carries its path, nearest first: `eth0 (under bond0)`,
`eth0 (under bond0 under vmbr0)`. The DRIFT GUARD section of `ssctl status` (`guard.live.devices` in `--json`) lists the
currently managed NICs and their root qdiscs.

**What one check does** (once at `enable`; every `interval_s` seconds while armed):

1. read `tcp_congestion_control`, and if it is not `skyline_cc`, change it back;
2. with `qdisc = true`: if `default_qdisc` is not `fq`, change it to `fq` (the kernel loads `sch_fq` itself on this
   write). This step comes first because the child queues of an `mq` created below are created according to it;
3. with `qdisc = true`, `tc_interface` set and that NIC present: determine the managed NICs (see above), read
   `tc qdisc show dev <nic>` for each, and act on the kind of its root qdisc:

| Kind of the root qdisc (or of the `mq`'s child queues) | Action |
|---|---|
| `fq`; or an `mq` whose child queues are all `fq` | Already compliant; left alone |
| `pfifo_fast` `pfifo` `bfifo` `pfifo_head_drop` `fq_codel` `codel` `cake` (unshaped: `bandwidth unlimited`, no `autorate-ingress` -- which is what `default_qdisc=cake` creates) `fq_pie` `pie` `sfq` `red` `sfb` `choke` `hhf` | Replaced with `fq`, as described below |
| Classful/shaping/hardware-offload kinds: `htb` `hfsc` `cbq` `drr` `qfq` `ets` `prio` `multiq` `mqprio` `taprio` `tbf` `netem`; a `cake` with a bandwidth set (`bandwidth` is not `unlimited`) or with `autorate-ingress`; a `noqueue` someone set on a physical NIC; an `mq` with any of these among its child queues | **Left alone**: someone set this up deliberately, and replacing it would silently destroy the shaping configuration. Only written into `notes`; for a shaped `cake` together with the reason, such as `eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone` |
| Any other kind it does not recognise; an `mq` whose child queues cannot be seen | Likewise left alone, and written into `notes` |

**How it replaces.** The child queues of an `mq` can only be created all at once according to `default_qdisc` when the
`mq` is created, or be replaced one by one, and replacing them one by one stops the whole NIC from transmitting once per
replacement (`mq_graft`) -- on a 64-queue NIC that is 64 times in a row:

- Single-queue NIC: `tc qdisc replace dev <nic> root fq`.
- Multi-queue NIC whose root is not an `mq` created by `tc` (the kernel's own `mq` with handle 0, or a single
  replaceable root): `tc qdisc replace dev <nic> root mq`; the new `mq`'s child queues are created according to
  `default_qdisc`. So this is done only when `default_qdisc`, read again, really is `fq`; when it is not `fq` (for
  example because the write in step 2 failed), no `tc` is run on the root, and only an error is reported:
  `<nic> root qdisc <summary> left alone: default_qdisc is <x>, a fresh mq would get <x> children`
  -- otherwise the guard would itself install what it is meant to remove, and under the kernel's own `mq` the child
  queues cannot be replaced one by one.
- An `mq` created by `tc` (non-zero handle): `replace root mq` on it is a no-op that the kernel accepts but that changes
  nothing (`mq` has no parameters). When none of the child queues is `fq` (and there are at least two) and
  `default_qdisc` is `fq`, it runs one `tc qdisc replace dev <nic> root handle <M>: mq`, where `M` is the first handle
  number, counting from hex 8000, that is not used anywhere in that NIC's `tc qdisc show` output (the range in which
  the kernel allocates handles itself; hand-written `tc` scripts tend to use small handles such as `1:`, and taking
  one would make a later `replace ... root handle 1: <another kind>` be rejected by the kernel): the kernel creates and
  grafts an `mq` whose child queues are all `fq`, and the NIC stops transmitting only once. Only when just one child
  queue is not `fq`, when some child queues already are `fq` (they may have tuned parameters such as `maxrate`, which a
  new `mq` would rebuild with default parameters), or when `default_qdisc` is not `fq`, does it replace them one by
  one with `tc qdisc replace dev <nic> parent <H:N> fq`.
- After replacing, it reads back and checks. If after the first pass they are still not all `fq` (for example because
  something changed `default_qdisc` in the meantime), a second pass replaces what remains once more (under an `mq`,
  that means replacing one by one the child queues that are still not `fq`); if that still does not work, it reports
  an error.

`clsact`/`ingress` are not root qdiscs and are ignored (`skyline_tc` is attached to `clsact`, and replacing the root
qdisc does not touch it). When the NIC does not exist, or `tc` shows no root qdisc (the NIC is down), only a note is
recorded.

**Failure handling.** Each `tc` call is limited to 10 seconds; on timeout it is killed and handed to a background
thread to reap, and the caller gets the error immediately. A `tc` stuck on the rtnl lock does not actually exit until
the lock is released; until it exits, every subsequent `tc` fails immediately
(`tc <args>: a previous tc has not exited yet (rtnl lock held?); skipped`) instead of starting a new one to queue up
alongside it. This way a stuck rtnl lock makes at most one check -- and the `drain` or `status` queued behind it -- wait
about 10 seconds longer, rather than 10 seconds for each `tc`, and never blocks anything indefinitely (the periodic
check holds the lock drain must take before writing back `fallback_cc`). No `tc`, a refused write, still not `fq` after
a replacement: all of these are recorded in `last_error` and `notes`, and never make `enable` fail.

After a replacement fails, the periodic check backs off for that NIC, rather than trying again every period (each
replacement briefly stops the NIC from transmitting), and rather than giving up (most failures are transient, such as
a netlink out-of-memory or a `tc` killed on timeout): the first retry comes after max(`interval_s`, 30 seconds), and
each further consecutive failure on the same root qdisc doubles the wait, up to 5 minutes (one period, when
`interval_s` is longer); retries happen only on periodic checks, so this duration is further rounded up to a whole
multiple of `interval_s`, and what is written out is when the retry really happens. While waiting, `notes` contains
`<nic> root qdisc <summary>: the last replace failed; retrying in <N> s` (`N` is the backoff of the current step, not a
countdown, so the log records each step only once; with `interval_s = 0` there are no periodic checks and it reads
`...; retried on the next ssctl enable ([guard] interval_s = 0)`). When the root qdisc changes into something else, it
retries immediately; the root becoming `fq`, a successful replacement, the NIC no longer being managed, or
`ssctl enable` (which does not wait out the backoff but retries immediately) all clear the backoff.

**Logging.** Every change is written to stderr (i.e. journald) as one `guard: ...` line, giving the old value, the new
value and what triggered it:

```
guard: default_qdisc cake -> fq (applied by ssctl enable)
guard: eth0 root qdisc cake -> fq (applied by ssctl enable)
guard: eth1 (under bond0) root qdisc mq/fq_pie -> mq/fq (something else on this host changed it)
guard: tcp_congestion_control bbr -> skyline_cc (something else on this host changed it)
```

`notes` and failures are logged as a line only when they differ from the previous check, so they do not flood the log
every `interval_s` seconds. Lines ending in `(something else on this host changed it)` that keep appearing mean that
something else on the host is fighting you over these settings -- this is the only clue:

```bash
journalctl -u skyline-speederd.service | grep 'guard:'
```

**Turning it off.** To have it manage only the congestion control: `[guard] qdisc = false`; for no periodic checks:
`interval_s = 0` (`enable` still runs one); either way, change the configuration and restart the daemon. `ssctl drain`
makes the guard stop immediately. `interval_s = 0` is also the only way, while skyline_cc is attached, to keep a default
congestion control that was changed back by hand (for example when you want only the processes in
`/sys/fs/cgroup/skyline-speeder` to use `skyline_cc`); but this lasts only until the next `enable`,
including the one `skyline-speeder-enable.service` runs at boot; see
`DEPLOY.md` section 5.2.
The guard does not modify any file in `/etc/sysctl.conf` or `/etc/sysctl.d` -- those files still take effect once first
at every boot, and are then overridden by `enable`. `install.sh` lists such files at the end of the installation.
