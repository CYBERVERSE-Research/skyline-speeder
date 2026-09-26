# Changelog

Notable changes to Skyline Speeder. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`SKYLINE_ABI_VERSION` is versioned separately from the release number: it tracks
the layout of `bpf/include/skyline_abi.h` and any bump to it is a breaking change
for anyone holding a prebuilt `.bpf.o`.

## [0.3.0] - 2026-09-26

### Upgrading from 0.2.0

- **`ssctl status` and `ssctl flows` print a report, not JSON.** Anything
  that parses their output needs `--json`, which prints exactly what 0.2.0
  printed. `install.sh` asks for `--json` and falls back to the bare command
  when it meets an older `ssctl`, and the health checks in `DEPLOY.md` §2 and
  §7 have been updated; a check of your own that greps for `"enabled": true`
  has to be changed the same way. `ssctl snapshot` is unaffected -- it always
  wrote JSON to a file and still does.

- **Re-run the new `install.sh` the way the host was installed** (with
  `--prebuilt` or `--no-enable` if it was installed with them). It now restarts
  `skyline-speederd` itself once the new objects have passed the kernel
  verifier, so the stop/restart steps in the 0.2.0 notes below are no longer
  needed. When `skyline-speeder-enable.service` is active the restart drains
  live flows for up to 60 seconds (over SSH it always runs into that timeout,
  which is harmless) and attaches skyline_cc again. This works on a host
  installed with 0.2.0 or earlier: what matters is the `install.sh` that runs,
  not the version it replaces. `scripts/bootstrap.sh` still fetches `main`
  unless given `--ref`.
- **A host where skyline_cc was attached with a bare `ssctl enable`**
  (`skyline-speeder-enable.service` not active, while the old daemon reports
  `enabled` or the default is `skyline_cc`) needs no manual drain any more.
  No `ExecStop` drains it and stopping the daemon still writes no sysctl, so
  the installer runs `ssctl drain --timeout 60` itself before the restart (a
  failure is only logged). Afterwards the enable unit attaches skyline_cc, and
  from then on at every boot; with `--no-enable` the installer runs
  `ssctl enable` once more instead, so the host ends as it was: attached by
  hand, not at boot.
- **Settings made with `ssctl` do not survive the restart**, as before. The
  installer now writes the old daemon's `ssctl status` into
  `/var/log/skyline-speeder-install.log`; re-apply from there what the host
  depends on (see *Upgrading from 0.1.0* for which commands).
- **`ssctl enable` now also changes qdiscs.** An installed
  `/etc/skyline-speeder/speeder.toml` from 0.2.0 has no `[guard]` table and is
  never overwritten, so it gets the defaults, `interval_s = 5` and
  `qdisc = true`: after the upgrade, attaching skyline_cc also sets
  `net.core.default_qdisc = fq` and replaces the root qdisc of
  `runtime.tc_interface` with `fq` (of the physical NICs under it, when it is a
  VLAN, bond or bridge), and the daemon keeps them there until the next drain.
  A root qdisc that looks built on purpose (`htb`, `tbf`, `netem`, `mqprio`, a
  `cake` with a bandwidth set, ...) is left alone. To keep 0.2.0's behaviour,
  add `[guard]` with `qdisc = false` to the installed file; a 0.2.0 daemon
  ignores the table, so it can go in before the upgrade.
- **`runtime.tc_interface` has to be an Ethernet device.** skyline_tc is no
  longer attached to a WireGuard/WARP, tun, GRE or PPP device (see Changed). An
  existing config is never rewritten, but the installer now warns when
  `tc_interface` names no device on the host (for instance the `link` that
  0.2.0's installer wrote on a host whose default route was
  `default dev wg0 scope link`) or a device that is not Ethernet: set it to the
  NIC that carries the traffic, then `sudo systemctl restart skyline-speederd`.
- `infra/boot-enable.sh` no longer writes `net.core.default_qdisc=fq` at boot.
  With `[guard] qdisc = false` nothing in Skyline Speeder sets a qdisc any
  more; set it in `/etc/sysctl.d` if the host relies on it.
- **`--uninstall` now leaves the host on bbr + fq, not on what it ran before**,
  and removes the build toolchain the install added (see Added).
  `--uninstall --restore-pre-install` keeps the old cc/qdisc restore; the
  toolchain removal has no opt-out flag -- delete lines from
  `/etc/skyline-speeder/added-packages` to keep a package. A host installed by
  0.2.0 or earlier has no such record, and running the new `install.sh` over it
  does not make one either: only a run that actually installs a package records
  it, and the toolchain is already there. Those hosts keep their toolchain
  unless it is removed by hand, or unless the host is installed again from
  clean.

### Added

- **`ssctl flows` lists the connections that are being accelerated.** It used
  to answer with the same object every other command returned, plus the line
  "flow enumeration is intentionally local-only; aggregate count returned" --
  a count, and nothing about the traffic. It now shows, per connection, the
  peer, RTT, congestion window, pacing rate, delivery rate, bytes sent and
  the retransmitted share of them, sorted by bytes sent (at most 50 rows; the
  counts stay exact above that). Under the table: what share of the host's
  TCP connections skyline_cc is carrying, the coefficients in force on them
  (pacing gains and cap, the window floor and ceiling, the queue-delay
  guardrail, loss compensation, the dynamic RTO bounds, DSCP), and the BPF
  counters showing what the algorithm decided, with the TC-layer and
  dynamic-RTO counters beside them.
  - The per-connection numbers are the kernel's own, read back through `ss`
    (iproute2, which the guard already needs for `tc`) and filtered to the
    sockets whose congestion control is `skyline_cc`. skyline_cc keeps its
    per-flow state in an `SK_STORAGE` map, which user space cannot enumerate
    without a file descriptor for every socket, so the daemon has never been
    able to walk it -- but cwnd, pacing rate and RTT are exactly what
    skyline_cc writes into the socket, so the kernel's view of them is a
    faithful record of what the acceleration did. Mode (STARTUP/CRUISE),
    bandwidth estimate and guardrail state stay aggregate, because `ss`
    cannot see them. An `ss` that is missing or too slow costs the table and
    nothing else: the report says so and everything from the daemon is still
    there.
- **`--json` on every `ssctl` command**, printing the reply exactly as 0.2.0
  did, and `--color auto|always|never` (`NO_COLOR` and `TERM=dumb` are
  honoured; symbols fall back to ASCII outside a UTF-8 locale).
- `ssctl flow` as an alias for `ssctl flows`.
- `uptime_s` and `attached_s` in `ssctl status`: how long the daemon has been
  running, and how long the current struct_ops attachment has been in place.
  `attached_s` is `null` while nothing is attached, and an `enable` that only
  pushes new coefficients into a live attachment does not reset it.
- Every `install.sh` run and every `ssctl` command names the project's
  sponsor, Skyline Connect (https://www.skylineconnect.io), which funds this
  work. In `--json` mode the line goes to stderr, so stdout stays parseable.

- **`install.sh --uninstall` leaves the host on bbr + fq and takes the build
  toolchain with it.** A host that installed Skyline Speeder almost always
  arrived from a "one-click BBR" setup, so being dropped to the daemon's
  `fallback_cc` (cubic) on the way out was a downgrade nobody asked for that
  nothing reported; and leaving clang, LLVM, bpftool and a Rust toolchain
  behind is not "removed". The uninstall now, after the drain and after the
  units, binaries and objects are gone:
  - sets `net.ipv4.tcp_congestion_control = bbr` and
    `net.core.default_qdisc = fq`, and puts `fq` back on the root qdisc of
    every NIC `runtime.tc_interface` resolves to -- the NICs the guard manages
    while it is armed, whether or not `[guard] qdisc` was ever turned off
    (`mq` over `fq` on a multi-queue one).
    One that is already `fq` is left as it is, and one that looks built on
    purpose (`htb`, `tbf`, `netem`, a `cake` with a bandwidth) is named and
    left exactly alone with the command to change it by hand -- the same rule
    the guard follows. A kernel without bbr gets `modprobe tcp_bbr` first
    (writing the sysctl does not load it: the kernel only accepts an algorithm
    that is already registered), and if it still has none, the pre-install
    value, then cubic, then reno, saying which. **For that boot only:** no file
    under `/etc/sysctl.d` is written or edited, so those files decide again
    after a reboot, and the uninstall says so.
  - removes the packages the install added, and only those. An install now
    records them in `/etc/skyline-speeder/added-packages` -- the difference
    between dpkg's installed set before and after apt ran, so it names the
    dependencies apt pulled in and can never name a package the host already
    had, which is why the removal needs no `autoremove`. It is also intersected
    with the plan apt accepted for that request, because a fresh cloud VM is
    often still running unattended-upgrades while the installer waits for the
    dpkg lock, and what that installed is not ours to remove. Four rails on
    top: `iproute2`, `curl`, `ca-certificates` and `tar` are never removed, nor
    is anything they still need transitively (keeping `curl` while removing the
    library under it is a contradiction apt resolves by taking `curl`, and
    without this rail the whole removal collapsed to nothing on the test host),
    nor anything dpkg calls *required* or *important* for any architecture; and
    apt plans the removal first, so a package something else on the host now
    needs is kept and named, the rest is removed, and nothing is removed at all
    only when no reduced plan stays inside the list (three rounds). (Measured
    while writing it: on one host, treating `libelf1` as removable would have
    taken 29 packages with it, `iproute2`, `ifupdown`, `isc-dhcp-client` and
    `cloud-init` among them.) Every failure in this step is a warning and a
    command to run by hand -- by then Skyline Speeder itself is already gone,
    so stopping would leave more behind, not less.
  - removes the rustup toolchain only when the installer is what installed it,
    which it records in `/etc/skyline-speeder/added-rustup`; a toolchain the
    operator had before is never touched. `rustup self uninstall` does the work
    where it can, and the fallback only deletes directories that still look
    like rustup's.
  - `/etc/skyline-speeder` is kept, as before, so a reinstall keeps the
    operator's settings. The two records above are consumed instead: each is
    deleted once what it describes is gone, so a reinstall starts a fresh one,
    and a removal that failed keeps its record for the next attempt.
- `install.sh --uninstall --restore-pre-install` keeps the old behaviour: put
  back the congestion control, `default_qdisc` and root qdiscs the host ran
  before the install, from `/etc/skyline-speeder/pre-install-state`. For a host
  that was on something else on purpose -- cubic for a comparison, a shaped
  qdisc -- where bbr and fq would be as wrong as cubic is on the usual one.
- The installer's closing guide has an `Uninstall` section, with the command
  spelled out against the path this run came from.
- **The guard: skyline-speederd keeps the host on skyline_cc and fq.**
  "One-click BBR" scripts write `tcp_congestion_control=bbr` and
  `default_qdisc=cake` or `fq_pie` into `/etc/sysctl.conf` or
  `/etc/sysctl.d`. Anything that re-runs `sysctl --system` then flips the
  default back to bbr, and every new connection bypasses skyline_cc with no
  error anywhere. `default_qdisc` also only shapes qdiscs created afterwards,
  so a NIC set up before it keeps `fq_codel` or `cake`. From a successful
  `ssctl enable` until the next `ssctl drain`, the daemon is now the single
  owner of `net.ipv4.tcp_congestion_control`, `net.core.default_qdisc` and the
  root qdisc of the egress NIC (`fq`, or `mq` over `fq` on a multi-queue NIC).
  It checks them when `enable` runs and then every `[guard] interval_s`
  seconds, and puts back whatever something else changed. Every correction is
  logged to the journal as a `guard: <what> <old> -> <new>` line, and the
  `enable` response lists what that enable changed. A qdisc it cannot fix never
  fails `enable`. Drain disarms it before writing `fallback_cc`, under the same
  lock, so a check cannot put skyline_cc back after a drain; the daemon's own
  stop still writes no sysctl.
  - *Which NIC.* `runtime.tc_interface` itself, unless its root is the kernel's
    default `noqueue` (a VLAN, bond, bridge or macvlan, which queue nothing):
    then the physical or virtio NICs found under it through `lower_*` links, up
    to four levels (bond slaves, a VLAN's real device, a bridge's physical
    ports). A VM's tap or a container's veth is never touched. A noqueue
    device with no NIC under it (a kernel WireGuard device, or a bridge of
    taps only) is noted in the status, and no qdisc is checked; a tun or PPP
    device has a qdisc of its own and is managed itself, like a NIC.
  - *What is replaced.* Only kinds with nothing worth keeping (`pfifo_fast`,
    `fq_codel`, `fq_pie`, an unshaped `cake` and the like). Classful, shaping
    and offload qdiscs, a `cake` with a `bandwidth` or `autorate-ingress`,
    `noqueue` set on a NIC by hand, and unknown kinds are left alone and named
    in the status (`eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate;
    left alone`).
  - *How.* A single-queue NIC gets `root fq`. A multi-queue NIC gets a fresh
    `root mq`, whose children the kernel builds from `default_qdisc`, and only
    while that reads back `fq`; otherwise the root is left alone with an error.
    An `mq` that `tc` created is replaced in one step by a new `mq` under a
    handle nothing on the device uses, rather than child by child, which would
    stop the whole NIC once per queue.
  - *When it fails.* A failed replace is retried after 30 seconds (or one
    interval, if longer), doubling on each consecutive failure up to five
    minutes; `ssctl enable` retries at once. Every `tc` call is bounded by 10
    seconds; one that is killed is reaped in the background, and until it has
    exited further `tc` calls fail at once, so a stuck rtnl lock delays a pass,
    and a drain or status request waiting behind it, by about ten seconds
    rather than indefinitely.
- `[guard]` in `speeder.toml`: `interval_s` (default 5, `0`..=`3600`; `0` means
  no periodic check, `enable` still checks once) and `qdisc` (default `true`;
  `false` leaves every qdisc alone). A config without the table gets the
  defaults.
- `guard` in `ssctl status`: whether it is armed, the counters
  (`checks`, `cc_restored`, `default_qdisc_restored`,
  `interface_qdisc_replaced`), the last correction and error, notes on what it
  left alone or is backing off from, and a live read of the congestion
  control, `default_qdisc`, the interface's own root qdisc (`fq`, `mq/fq`,
  `cake`, ...) and `devices`, the NICs it manages with their root qdiscs.
- `skyline-speederd --version`, `ssctl --version`, and `version` in
  `ssctl status`, which is the running daemon's version. A mismatch with
  `skyline-speederd --version` means the daemon was not restarted after an
  upgrade. A newer `ssctl` still decodes a status without `version` or `guard`.
- **A quiet installer with a guide at the end.** `install.sh` shows one
  progress line redrawn in place, with the step, elapsed time and a spinner,
  plus warnings. Everything the steps print goes to
  `/var/log/skyline-speeder-install.log`, which is kept. Piped into a file or
  a CI log it prints one plain line as each step starts and ends; `--verbose`
  (or `SKYLINE_VERBOSE=1`) streams all output instead. A failure shows the
  error, the failing step's last lines of the log and the log's path. The end
  of an install shows the congestion control and qdiscs before and after (one
  row per managed NIC, with the reason for any it left alone), the sysctl
  files that still set something else (reported, never edited), the everyday
  `ssctl` commands, and how to tune the most common parameters live and
  persistently, with their current values. Which files count at boot follows
  systemd-sysctl: a same-named file in `/etc`, including a `/dev/null` link or
  an empty file, masks the others, and `/etc/sysctl.conf` counts only through
  a `sysctl.d` entry that links to it; otherwise it is reported as applied by
  `sysctl -p` / `sysctl --system` only. `--check` also prints the current
  congestion control, qdiscs and these settings.
- The installer's last step asks the new daemon, not just the sysctl, whether
  skyline_cc is attached: `enabled` must be true, the guard armed (when the
  daemon has one) and the default `skyline_cc`. If not, it restarts
  `skyline-speeder-enable.service` once and otherwise fails, pointing at
  `sudo ssctl enable` and the unit's journal. (After skyline-speederd was
  killed out of band, the enable unit stays `active (exited)`, so a plain
  `enable --now` attached nothing while the default still named the dead
  daemon's skyline_cc.)
- The pre-install snapshot also records the root qdisc of every NIC the guard
  will manage (`PRE_INSTALL_QDISC_DEV` and `PRE_INSTALL_ROOT_QDISC`, two
  space-separated lists in step; one NIC looks like a plain name and summary),
  and `--uninstall --restore-pre-install` puts each one's kind back, with
  default parameters, if it still has the `fq` or `mq/fq` Skyline Speeder left
  there (the default `--uninstall` leaves `fq` there on purpose). An `mq` is rebuilt
  with `tc qdisc del` of the root (a `replace root mq` over the guard's
  `tc`-made `mq` changes nothing), falling back to `replace root mq` over the
  kernel's own handle-0 `mq`. A new snapshot always carries both keys, empty
  when no NIC is known; only a 0.2.0 snapshot, which has neither, gets them
  added on upgrade, and its existing keys are not touched.
- `release.yml` refuses to build unless the tag is `v` plus the workspace
  version and `CHANGELOG.md` has a `## [<version>]` section for it, so a
  release can no longer ship binaries whose `--version` names another release.

### Changed

- **`ssctl status` and `ssctl flows` answer two different questions.** Both
  used to print the whole `RuntimeStatus` as JSON -- the same object, differing
  in one line of message -- so neither told an operator anything at a glance
  and the two overlapped almost completely. `status` is now about the
  takeover: whether skyline_cc is attached, whether it is the host's default
  (attached but not the default is this project's worst silent failure, and it
  is now stated in those words), what the guard holds and how often it has had
  to put it back, kernel support, the global sysctls, and an `ATTENTION` block
  listing everything wrong with a remedy. `flows` is about the traffic and is
  described under Added. Colour, symbols and proportion bars are used where
  they carry meaning -- a bar has its full scale written next to it -- and are
  dropped entirely when stdout is not a terminal.
- Commands that change something (`enable`, `drain`, `set-*`, `reset-*`)
  print the daemon's sentence and then one line of where the host stands now,
  instead of a screenful of JSON. `enable` and `drain` list each correction
  they made on its own line.

- **A bare `ssctl enable` now also sets `fq`.** With the default
  `[guard] qdisc = true` it writes `net.core.default_qdisc = fq` and replaces
  the root qdisc of `runtime.tc_interface` (or of the NICs under it); before,
  it touched no qdisc. Set `[guard] qdisc = false` to opt out.
- **skyline_tc is only attached to an Ethernet device**
  (`/sys/class/net/<if>/type` 1; VLANs, bonds and bridges qualify). It parses
  an Ethernet header at offset 0, so on an L3 tunnel its counters silently
  counted nothing and an enabled DSCP writer would have written inside the IP
  header. On any other device the daemon now refuses to attach it: the status
  says why under `capabilities.notes`, `--validate-only` prints it too, and
  `set-retransmit-dscp` fails.
- `infra/boot-enable.sh` writes no sysctl at all and no longer runs
  `modprobe sch_fq`: the daemon owns `default_qdisc` now, and the kernel loads
  `sch_fq` itself when it is written. The one-shot write at boot was not
  enough anyway, since it never reached the NIC's root qdisc and the next
  `sysctl --system` undid it.
- `infra/boot-disable.sh` (the enable unit's `ExecStop`) runs `ssctl drain`
  first and writes `fallback_cc` itself only if the default is still
  `skyline_cc` afterwards, that is when the daemon was already gone. Writing it
  before the drain, as it did, let the still-armed guard put skyline_cc back
  and log a misleading "something else on this host changed it".
- **`install.sh` restarts a running `skyline-speederd` on upgrade**, after the
  verifier has passed, instead of leaving the old daemon in memory, and drains
  a host attached with a bare `ssctl enable` first (see Upgrading). The
  summary shows the version it upgraded from.
- `install.sh --no-enable` attaches nothing that was not attached: it enables
  no unit and disables none, re-attaches after the upgrade restart only what a
  bare `ssctl enable` had attached, and reports the attach state the new
  daemon and the host default actually show.
- `install.sh` points `runtime.tc_interface` at the first Ethernet device a
  default route leaves through, IPv4 routes first, then IPv6. When the only
  default route goes through a tunnel it keeps the placeholder and warns that
  the NIC has to be set by hand; a configured `tc_interface` that does not
  exist or is not Ethernet is warned about, never rewritten.
- Both install paths, source and `--prebuilt`, install `iproute2`, which the
  guard needs for `tc`. apt waits up to five minutes for the dpkg lock (a
  fresh cloud VM is often still running unattended-upgrades) and keeps an
  operator's changed configuration files instead of prompting.
- `--prebuilt` installs a published release, which can be older than the
  `install.sh` that runs it: 0.2.0, for one, has no guard. The installer now
  recognises such a daemon (no `guard` in its status), says so in one warning,
  marks the qdisc row "(not managed by this release)" and claims nothing about
  keeping settings in place.
- `scripts/bootstrap.sh` prints apt's output only when apt fails, and uses
  colours only on a terminal.
- `release.yml` builds with `cargo build --locked`, and CI runs `cargo check`
  and `cargo test` with `--locked`, so a stale `Cargo.lock` fails on the pull
  request rather than at tag time.
- `docs/04-performance-report.md` compares only with BBR. The acceptance
  criteria are now stated against `bbr-fq` alone; BBR was the strongest
  baseline wherever a criterion was evaluated, so no outcome changes. The
  check that all modules off matches the kernel's built-in congestion control,
  which is a correctness test rather than a performance comparison, moved to
  `docs/03-design.md` section 4, and the report's later sections are
  renumbered.
- `DEPLOY.md` and `docs/01-deployment-guide.md` cover the `--prebuilt` path
  (glibc and library requirements, `--release`, `SKYLINE_ARTIFACT_URL`,
  checksums) next to the source build.

### Fixed

- **`ssctl status`'s `delivered_packets` was documented as "cumulative
  acknowledged packets", which it is not.** It sums `rate_sample.delivered`
  once per ack, and consecutive acks report overlapping windows, so it runs
  about two orders of magnitude above the packets actually delivered
  (measured on a 6.12.63 host: it advanced by 16,703,016 over the same 20
  seconds in which the kernel's own `tp->delivered` advanced by 164,416 --
  101x). The counter itself is unchanged: the experiment harness compares it
  between runs of the same shape, which is sound, and deliberately keeps it
  out of `RUN_FIELDS`, so no published number depends on it. What changed is
  that it is now described accurately in
  `docs/02-interface-reference.md` section 4, `ssctl flows` labels it
  *delivery samples* rather than a packet count, and nothing derives a rate
  from it -- the loss-event share is against `ack_events`, and the real
  volume comes from the kernel's byte counters in the connection table.
- `guardrail_hits` counts two things, not one: the queue-delay/ECN clamp and
  the `max_cwnd_packets` ceiling. It was documented as the first alone, which
  would send an operator looking for queueing that is not there.

- **`install.sh` could not install the build toolchain on a Debian 12 host
  running a 6.12 kernel from `bookworm-backports`**, which is the usual way to
  reach this project's 6.12 kernel floor on bookworm. Installing
  `linux-headers-cloud-amd64` from backports (for a DKMS module, say) brings
  `libelf1 0.192` with it, bookworm's `libelf-dev` depends on
  `libelf1 (= 0.188-2.1)`, and apt could satisfy nothing:
  `E: Unable to correct problems, you have held broken packages`. The
  installer now has apt plan the install first, and when a package cannot be
  placed it offers that package's other versions until the request resolves --
  here `libelf-dev 0.192-4~bpo12+1`, the version matching the library the host
  already has. One package, not the whole toolchain out of another suite. The
  same plan finishes an interrupted `dpkg` run first (`dpkg --configure -a`,
  `apt-get -f install --no-remove`) when that is what is blocking apt, and
  `--check` now reports whether apt can install the packages at all.
- `install.sh` stopped at `apt-get update failed` when a single repository on
  the host was unreachable -- a "one-click BBR" script's leftover source, a
  moved mirror -- even though every suite it needed had refreshed. It now
  names the repositories that failed and carries on; the package plan decides
  whether the host can install what the build needs.
- `skyline-speeder-enable.service` pointed `Documentation=` at
  `file:/usr/share/doc/skyline-speeder/01-deployment-guide.md`, which nothing
  installs. Both units now link to the deployment guide on GitHub.
- `install.sh` took the default-route interface from the fifth field of
  `ip -o route show default`, which is not the device on a route without a
  gateway (`default dev wg0 scope link`). It now takes the word after `dev`;
  the same command in `DEPLOY.md` is fixed too.
- `install.sh` found no egress interface on an IPv6-only host, since it only
  read the IPv4 default route; it now falls back to `ip -6 route`, and so does
  the command in `DEPLOY.md`.
- `install.sh --prebuilt` stopped with no error message when the release had
  no artifact for the host's architecture: `set -e` ended it on the failed
  lookup before it could print "no prebuilt artifact for <arch>".

### Known issues

- Stopping or restarting `skyline-speederd` by hand while skyline_cc was
  attached with a bare `ssctl enable` (`skyline-speeder-enable.service` not
  active) still does not write `fallback_cc` back to
  `net.ipv4.tcp_congestion_control`; this is deliberate. `ssctl drain` does,
  and so does that unit's `ExecStop` when it is active. `install.sh` drains
  such a host itself before its upgrade restart.
- The prebuilt `skyline-speederd` still needs glibc 2.38 or newer with
  `libelf.so.1` and `libz.so.1` (see 0.2.0).

## [0.2.0] - 2026-09-22

### Upgrading from 0.1.0

- **Save the runtime state first.** Overrides made with `ssctl` live only in
  the daemon's memory and are gone after the restart below, and stopping the
  daemon also removes `/run/skyline-speeder`. Keep a copy, for example
  `sudo ssctl status > skyline-status-before.json` (`modules`,
  `module_tuning`, `rack_rto`, `retransmit_dscp`), and afterwards re-run
  whatever `ssctl set-module-config`, `set-rack-rto`/`reset-rack-rto`,
  `set-retransmit-dscp`/`reset-retransmit-dscp`, `enable --modules`/`--all-off`
  or `disable --module` the host depends on. The new daemon starts with
  `[rack_rto]` and `[retransmit_dscp]` off, whatever the file says. Restore
  `module_tuning` with every value from the saved copy, since a
  `set-module-config` that names only some flags fills the rest with the 0.2.0
  defaults; and note that `rack_rto.config` and `retransmit_dscp.config` show
  the file's values even if they were never applied.
- **Stop the daemon before installing, or restart it after.** Re-running
  `install.sh` replaces the binaries and BPF objects but does not restart a
  `skyline-speederd` that is already running, and still reports success. The
  0.1.0 daemon then keeps running, unbounded event log included, and it cannot
  load the new `SKYLINE_ABI_VERSION` 7 objects: whenever it has to -- an
  `ssctl enable` after a drain that completed, or on a host where skyline_cc was
  never attached -- the enable fails and leaves new connections on
  `fallback_cc`. On a host installed with `--no-enable`, re-running
  `install.sh` without that flag therefore fails at the installer's own enable
  step.

  Stopping `skyline-speeder-enable.service` is what puts `fallback_cc` back as
  the system default; stopping the daemon while that unit is not active
  unregisters skyline_cc but leaves the sysctl naming it. So on a host where skyline_cc was attached with a
  bare `ssctl enable` rather than through that unit, run the drain line below
  first, and also before the restart described after it. Over SSH that drain
  always ends at its timeout and exits non-zero, which is harmless: it switches
  the default before it starts waiting.

  ```bash
  sudo ssctl drain --timeout 60       # only after a bare `ssctl enable`
  sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service
  sudo ./install.sh --prebuilt        # or however this host was installed
  ```

  If `install.sh` already ran over a live daemon, restart it:
  `sudo systemctl restart skyline-speederd.service`. systemd restarts
  `skyline-speeder-enable.service` with it only if that unit is active; if it
  is failed or inactive (for instance because the installer's enable step
  failed) and skyline_cc should be attached, start it once the new daemon is
  up: `sudo systemctl start skyline-speeder-enable.service`.

  New connections use `fallback_cc` until the new daemon attaches, and flows
  still open stay on the 0.1.0 instance until they close. After a re-install, a
  still-running old daemon shows up as `(deleted)` in
  `sudo readlink /proc/$(systemctl show -p MainPID --value skyline-speederd)/exe`.
- **Installed coefficients stay as they were.** `install.sh` never overwrites
  `/etc/skyline-speeder/speeder.toml`, so an upgraded host keeps the 0.1.0 set,
  and `ssctl reset-module-config` returns to it. `ssctl set-module-config`'s
  built-in defaults are the new set, so a command that names only some flags
  moves the rest to the new values. To adopt the new defaults, copy them from
  `config/speeder.toml` (listed under Changed) into the installed file and
  restart the daemon.
- `runtime.events_max_mib` needs no edit: a config without it gets the 8 MiB
  cap once the 0.2.0 daemon is running.
- `scripts/bootstrap.sh` fetches `main` unless told otherwise. For a source
  build pass `--ref v0.2.0`; with `--prebuilt` the artifacts come from the
  latest release whatever `--ref` says, and `--release v0.2.0` pins them.
- An experiment guest deployed before this release lacks the new
  `snapshot-skyline-events.sh` actions, and `run_matrix.py` then records an
  empty event log for every case without an error. Redeploy the guest
  (`infra/deploy-guest.sh --confirm-install`), then restart
  `skyline-speederd.service` on it, or reboot it, before running the matrix:
  like `install.sh`, the deploy replaces the files but leaves a running 0.1.0
  daemon in memory.

### Added

- `min_cwnd_packets` (`--min-cwnd-packets`): the floor under M2's BDP-derived
  cwnd target is now configurable instead of the compile-time 4. It defaults to
  4, so an existing `speeder.toml` that does not declare it behaves exactly as
  before. Valid range is `4`..=`max_cwnd_packets`; the M2-off neutral path keeps
  the fixed floor and does not read it. Pacing still sets the send rate.
- `runtime.events_max_mib`: size cap, in MiB, for the BPF event log at
  `runtime.events_path`, default 8. At the cap the log rotates to
  `events.jsonl.1`, so it holds at most about twice that; `0` turns the log off.
  It is read at startup, so changing it takes a daemon restart. Why it exists is
  under Fixed.
- `infra/snapshot-skyline-events.sh cursor` and `since CURSOR`: a position in
  the event log, `INODE:LINES`, that survives one rotation. `run_matrix.py` uses
  them for its per-case event snapshot; `count` and `from` are unchanged.
- A single-ended field comparison of skyline_cc, bbr and tcp-brutal on one
  production path: a README section in both languages, the harness and the
  raw JSONL under `research/experiments/single-ended/`, and the figures in
  `docs/images/`. It is a
  field measurement, not the dual-VM test bed, and it ran the 0.1.0
  coefficients, now the "high random loss" preset, so it says nothing about
  this release's defaults. `README.zh.md` also gains the comparison with the
  alternatives that `README.md` already had.

### Changed

- **Default coefficients.** `config/speeder.toml`, the installed template
  `config/speeder-guest.toml` and `ssctl set-module-config`'s built-in defaults
  move together to a set tuned by interleaved A/B on a
  production deployment (many concurrent flows, roughly 100-150ms base RTT,
  loss dominated by a full bottleneck rather than random loss):
  `cruise_pacing_gain` 1.1 -> 1.25, `cruise_inflight_gain` 2.0 -> 3.0,
  `startup_plateau_rtts` 3 -> 5, `startup_growth_ratio` 0.25 -> 0.20,
  `loss_inflation_max_ratio` 0.5 -> 0.10, `max_queue_delay_ms` 100 -> 70,
  `max_queue_delay_ratio` 1.0 -> 0.6, `min_rtt_window_s` 10 -> 30,
  `bw_window_rtts` 10 -> 6. An installed `/etc/skyline-speeder/speeder.toml` is
  never overwritten, so an existing host keeps its values until its operator
  edits the file -- but `ssctl set-module-config` sends the *new* built-in
  default for every flag left off the command line. The previous set is kept as
  the "high random loss" preset in `docs/usage.md`; it is what
  `docs/04-performance-report.md` measured, and the experiment matrix stays
  pinned to it. The new set has not been run through that matrix. The
  conservative preset and the tuning examples in `docs/usage.md` are rebased on
  the new set too.
- A test now fails if `ssctl set-module-config`'s defaults drift from
  `config/speeder.toml`; they used to be kept in sync by hand.
- **`SKYLINE_ABI_VERSION` 6 -> 7.** `struct skyline_config` grows by
  `min_cwnd_packets` plus an explicit `reserved` tail word (72 -> 80 bytes). A
  `.bpf.o` built against version 6 is rejected by a newer daemon and vice
  versa. Reinstalling replaces both files but not the daemon already in memory,
  so restart it too; see Upgrading from 0.1.0.

### Fixed

- **The event log could fill `/run`.** `skyline-speederd` appended every BPF
  event to `runtime.events_path` (`/run/skyline-speeder/events.jsonl`) with no
  limit. `/run` is a RAM-backed tmpfs shared with the rest of the host, and on a
  busy host the log filled all of it, at which point Docker could no longer
  write its runc state files; skyline-speeder itself reported nothing. The log
  is now capped by `runtime.events_max_mib` (see Added), and the cap also
  applies to an installed `speeder.toml` that predates the field. On a host
  still running 0.1.0,
  `sudo truncate -c -s 0 /run/skyline-speeder/events.jsonl` frees the space at
  once; `rm` does not, because the daemon keeps the file open.
- `install.sh --prebuilt` failed on a stock Debian 13 host with `BPF struct_ops
  support was not detected` (#5). The capability probe shelled out to `bpftool`,
  which a prebuilt install deliberately does not have, and read the missing
  binary as a missing kernel feature. `struct_ops` and `rack_reo_hook` are now
  read in-process from `/sys/kernel/btf/vmlinux`. A BTF file that cannot be
  parsed is now named in `capabilities.notes`, although validation still stops
  with the same `struct_ops` error.
- The same probe reported `struct_ops: true` on any host where bpftool ran at
  all: it matched the substring `struct_ops`, which `bpftool feature probe` also
  prints on its "is NOT available" line.
- `install.sh` no longer blames the verifier for every validation failure, and
  prints the full command to rerun rather than referring to a `>/dev/null` that
  a `curl | bash` user never typed.

### Known issues

- The prebuilt `skyline-speederd` is built on Ubuntu 24.04 and needs glibc 2.38
  or newer, plus `libelf.so.1` and `libz.so.1`: Debian 13 and Ubuntu 24.04 are
  fine. On older userspace running a 6.12+ kernel, such as Debian 12 with a
  backports kernel, the prebuilt daemon cannot start and `install.sh
  --prebuilt` fails at validation. A source build does not have the glibc
  requirement, but that combination has not been tested.
- Neither binary reports its version, and `install.sh` does not restart a
  running daemon (see Upgrading from 0.1.0).
- When skyline_cc was attached with a bare `ssctl enable`
  (`skyline-speeder-enable.service` not active), stopping `skyline-speederd`
  does not write `fallback_cc` back to `net.ipv4.tcp_congestion_control`;
  `ssctl drain` does. While that unit is active, stopping or restarting the
  daemon stops the unit first, and its `ExecStop` does the write.
- The known issues listed under 0.1.0 below still apply.

## [0.1.0] - 2026-09-19

First public release.

Everything below is in this release. Nothing was published before it, so there
is no "changed since" to report: work that happened between the first commit and
the tag is release content, not a changelog of revisions, and is recorded as such
rather than as fixes to a version nobody could have installed.

### Added

**Data path**

- `skyline_cc` — eBPF struct_ops congestion control with four independently
  switchable modules: adaptive cwnd (M2), loss-rate compensation (M3), pacing
  (M4), and early-loss observation (M1).
- `skyline_policy` — cgroup sockops program: per-connection congestion-control
  selection, plus a per-flow dynamic RTO floor and ceiling.
- `skyline_tc` — TC egress program: packet/byte/GSO accounting and retransmit
  DSCP marking for IPv4 and IPv6.

**Control plane**

- `skyline-speederd` / `ssctl` — capability probing, online reconfiguration
  through a double-slot generation counter, graceful drain, and a
  `--validate-only --verify-bpf` health check that leaves no runtime state.
- `ssctl enable` activates host-wide: on a successful struct_ops attach it
  writes `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP
  connection on the machine uses it, not only connections from processes inside
  the cgroup. The sysctl write happens after the attach, because the kernel
  rejects an algorithm name it has not seen registered.
- `ssctl drain` is the symmetric reverse: the sysctl goes back to `fallback_cc`
  first, then the cgroup dispatch path closes, then it waits, then it
  unregisters. A drain that times out leaves the struct_ops attached but the
  default already back on `fallback_cc`.

**Installation**

- `install.sh` — one-click Debian/Ubuntu installer with `--prebuilt`, `--check`,
  `--no-enable` and `--uninstall`.
- `--prebuilt` installs published artifacts instead of compiling: no clang, no
  LLVM, no bpftool and no Rust on the target host, only `curl` and `tar`.
  `--release <tag>` pins a version; `SKYLINE_ARTIFACT_URL` takes an https URL
  (mirror, internal artifact store) or a local path, for hosts with no route to
  github.com.
- The installer snapshots the congestion control and default qdisc to
  `/etc/skyline-speeder/pre-install-state` before it starts anything, and
  `--uninstall` restores both. A host that ran BBR comes back on BBR.
- `scripts/bootstrap.sh` — remote installer entry point for `curl | sudo bash`,
  with optional `SKYLINE_SHA256` tarball pinning.

**Build and release**

- `.github/workflows/release.yml` builds and publishes artifacts on a `v*` tag,
  compiling against a pinned reference header rather than the runner's own
  kernel, and refusing to build when that header is not configured.
- `infra/kernel/make-reference-vmlinux.sh` generates that header on a host
  running the oldest supported kernel and prints the digest it is pinned by.
- `infra/kernel/core-portability.sh` — a repeatable two-phase check that objects
  built against one kernel's types load on another: `freeze` records the objects
  and their digests on the build kernel, `verify` proves the bytes are unchanged
  and pushes them through the target kernel's verifier. It knows nothing about
  hosts or transports, so it works for whatever kernel pair needs checking.
- `make PREBUILT_VMLINUX_H=<path> bpf` builds against a header generated
  elsewhere, with no bpftool and no `/sys/kernel/btf/vmlinux` required.
- CI asserts every built object carries a `.BTF.ext` section — without it an
  object has no CO-RE relocation records and is bound to its build kernel — and
  uploads the objects as a build artifact.

**Research**

- Two-VM experiment harness under `research/experiments/` and the performance
  report it produces.

### Verified

- Kernels `6.12.101`, `6.18.42` and `7.1.6`, including IPv4/IPv6 data-path smoke
  tests. `6.1.180` and `6.6.148` are rejected at load, as expected.
- **CO-RE portability, both directions**, on Debian 13 with the objects checked
  byte-identical at each step: built under 6.12.63, the verifier passes on
  6.19.14 and the objects attach and carry real traffic there; built on 6.19.14
  against a 6.12 header, the verifier passes on 6.12.63.
- Neutrality: with all modules off, within 0.01% of stock kernel CUBIC on a
  zero-loss link.
- Retransmit DSCP marking: about 97,000 marked segments across IPv4 and IPv6
  captures, zero false positives.
- The one-click install, the prebuilt install and the uninstall path, end to end
  on a clean Debian 13 host running 6.12.63.

### Licensing

- Copyright **CYBERVERSE LLC**, **GPL-2.0-only** throughout, with an
  `SPDX-License-Identifier` on every source file. `LICENSE` holds the complete
  GPL-2.0 text; `NOTICE` records why GPL-2.0 is required, the upstream kernel
  attributions, and the trademark terms.

### Known issues and limitations

- **Congestive bottlenecks are out of scope.** Loss is assumed to carry no
  congestion information; on a genuinely congested path this keeps pushing and
  harms both itself and everything sharing the path. The queueing-delay/ECN
  guardrail is the only self-protection and is not a general safety net.
- **Reordering is a weak spot.** The `reorder-dsack` scenario measures roughly
  9% below CUBIC and 13% below BBR.
- **Bandwidth validated only to 100 Mbit/s** — a structural ceiling of the test
  bed, not a property of the algorithm.
- **No cross-flow coordination.** Each flow estimates bandwidth independently and
  applies its own gain, so several flows sharing one bottleneck will collectively
  overshoot.
- **`ssctl drain` cannot complete over SSH.** The operator's own SSH connection
  is a skyline_cc flow and will not end while drain waits, so drain always
  reaches its timeout. It is safe — the sysctl is written back to `fallback_cc`
  before the wait begins — but the struct_ops stays attached until that session
  closes.
- **The one-line installer needs `curl`**, which a minimal server image may not
  have. `bootstrap.sh` installs it when missing, but only once it is already
  running; the READMEs show the prerequisite and a `wget` equivalent.
- **CO-RE fixes offsets, not names.** A field renamed or removed in a future
  kernel makes relocation fail at load. Two kernels and one architecture is
  evidence, not proof, which is why the check is a script rather than a claim.
- **`early-loss` is an observation counter only.** It drives no decision.
- **`runtime.pin_dir` is unused.** The field is parsed and validated but nothing
  pins BPF objects yet.
- **The `ssctl` wire protocol has no authentication**, relying entirely on Unix
  socket file permissions. Multi-tenant hosts need additional access control.
- The experiment harness requires **Python 3.11 or newer** (`tomllib`).

[Unreleased]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/CYBERVERSE-Research/skyline-speeder/releases/tag/v0.1.0
