<div align="center">

# Skyline Speeder

**Sender-side TCP acceleration for high-loss, high-latency links, built on eBPF**

*Deploy on the sending server only. Clients stay on stock TCP and need no software, no patches, no configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#kernel-requirement)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-black.svg)](rust-toolchain.toml)

English · **[简体中文](README.zh.md)**

---

### Exclusively sponsored by **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## What this is

Skyline Speeder is single-sided TCP acceleration software. It runs as eBPF programs on the server that sends the data, and is meant for links with heavy packet loss and long round-trip times, where standard TCP collapses. It raises throughput on such links by keeping the sending rate at what the path actually delivers, instead of cutting it back every time a packet is lost. The receiving side is untouched.

## Kernel requirement

**Linux 6.12 LTS or newer**, with `/sys/kernel/btf/vmlinux` present. `skyline_cc` implements the four-argument `cong_control(sk, ack, flag, rs)` callback, which exists only from 6.10; older kernels reject the program at load time. That rules out the 6.1 and 6.6 LTS branches and the stock Debian 12 and Ubuntu 22.04/24.04 kernels.

Verified: `6.12.101`, `6.18.42` and `7.1.6` pass, including IPv4/IPv6 data-path smoke tests. `6.1.180` and `6.6.148` fail to load, by design.

**Distributions:** Debian and Ubuntu (apt, systemd), Fedora and the RHEL family — Rocky Linux, AlmaLinux, CentOS Stream (dnf, systemd) — and Alpine (apk, OpenRC), on a kernel that meets the line above. The one-line install has been run end to end on Debian 13, Fedora 43, Rocky Linux 10.2 and Alpine 3.23.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

On Alpine, as root — a fresh Alpine has `wget` but neither `curl` nor `bash`, and the bootstrap installs both:

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

> [!NOTE]
> **The one-line install now installs the latest published release by default**: prebuilt BPF objects and daemon, with no compiler, LLVM or Rust put on the host. Every release from v0.4.1 carries a glibc build and a musl build for Alpine. A host that can run neither (not x86_64, or a glibc older than 2.38 such as Debian 12's) is told why and builds from source instead. To build from source anyway, or to install a particular release:
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --source           # build from source on this host
> curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --release v0.4.0   # that release, not the latest
> ```
>
> Installers up to and including v0.4.0 built from source by default and needed `--prebuilt` for the release; the one-line install always runs the installer from `main`, so it has the new default already. `--prebuilt` still works and now means "the release or nothing": on a host that cannot run it, it stops instead of building.

A minimal Debian image may lack `curl` (`apt-get install -y curl`), and many images lack `sudo` (drop it if you are root). To read the scripts before running them, which is **recommended** since this changes the congestion control of every new connection on the machine:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh              # the published release (builds only where it cannot run)
sudo ./install.sh --source     # build this tree from source
sudo ./install.sh --prebuilt   # the published release, or stop
sudo ./install.sh --check      # preflight only, changes nothing
sudo ./install.sh --no-enable  # install, but attach nothing that was not attached
sudo ./install.sh --verbose    # every command's output instead of the progress line
sudo ./install.sh --uninstall  # remove, put this host on bbr + fq, remove the
                               #   packages the install added (keeps /etc/skyline-speeder)
sudo ./install.sh --uninstall --restore-pre-install   # ... but put back the
                               #   congestion control and qdisc from before the install
```

The installer uses the distribution's own package manager and service manager — apt, dnf or apk; systemd, or OpenRC on Alpine, where `rc-service skyline-speederd restart` stands in for `systemctl restart skyline-speederd` in these docs. It checks the kernel, builds or downloads the three BPF objects and the daemon, points the TC program at the default-route interface (IPv4 or IPv6; when that route leaves through a tunnel such as WireGuard, it asks you to name the NIC), runs every object through the kernel verifier, then starts the daemon, attaches `skyline_cc` and enables both at boot. It shows one progress line and keeps every command's output in `/var/log/skyline-speeder-install.log`; when it finishes it prints the congestion control and qdisc before and after, then a short guide to `ssctl`, tuning and uninstalling. **It installs no proxy and opens no port.**

`--uninstall` drains live flows, removes the two services (systemd units, or the OpenRC scripts on Alpine), binaries and BPF objects, puts the host on **bbr + fq**, and removes the packages the install added. It records which packages those were (`/etc/skyline-speeder/added-packages`) and removes only those: never `iproute2` (`iproute` and `iproute-tc` on Fedora/RHEL), `curl`, `ca-certificates` or `tar` (or `bash` on Alpine), never anything one of those still needs (keeping `curl` while removing the library under it is not something apt can do), and never anything dpkg calls *required* or *important* or dnf protects. The removal is planned first (by apt, or `rpm -e --test`; apk never removes what something still needs): a package something else on the host now needs is kept and named, and the rest is removed; only when no reduced plan stays inside the recorded list is nothing removed at all, and the command to do it by hand is printed instead. A rustup toolchain is removed only if the installer is the one that installed it. `bbr` and `fq` are set for that boot; no file under `/etc/sysctl.d` is written or edited, so those files decide again after a reboot. `--restore-pre-install` puts back the congestion control and qdiscs the host ran before instead — it also records those, including the egress interface's root qdisc. `/etc/skyline-speeder` is kept either way, so a reinstall keeps your settings.

The published release needs no toolchain, only `curl`, `tar` and `iproute2` (the installer installs them): the objects are CO-RE, compiled against a pinned 6.12 header and relocated against this kernel when they load. The prebuilt daemon comes in two builds: one built on Ubuntu 24.04, which needs glibc 2.38 or newer with `libelf.so.1` and `libz.so.1` (Debian 13, Ubuntu 24.04, Fedora 43, Rocky Linux 10 and later), and, from v0.4.1, one built in Alpine 3.21 against musl, which Alpine 3.21 and later install together with `libelf`, `zlib`, `zstd-libs` and `libgcc`. On older userspace the default install builds from source by itself, and `--source` does so anywhere. `--release <tag>` pins a version, and `SKYLINE_ARTIFACT_URL=/path/to/tarball sudo -E ./install.sh --prebuilt` installs a copied artifact on a host with no route to GitHub. What gets installed is a published release, which can be older than this README and than `main`: a change merged after the latest release reaches the one-line install only with the next one. A release that predates a feature described here (the v0.2.0 release has no qdisc guard, for one) is named as such by the installer.

> [!IMPORTANT]
> **Upgrading:** run the installer again. Once the new objects pass the kernel verifier it restarts `skyline-speederd` itself, draining live flows for up to 60 seconds, and attaches `skyline_cc` again, also on a host where it was attached by hand with `ssctl enable`. Settings changed with `ssctl` do not survive the restart. A host that was built from source moves to the published release unless you pass `--source` again — on Alpine too, from v0.4.1. Details: [DEPLOY.md](DEPLOY.md) §10.

Day-to-day:

```bash
ssctl status    # has this host been taken over? attachment, host default, guard, kernel
ssctl flows     # what is being accelerated? the connections, their parameters, the counters
ssctl drain     # graceful detach: fallback_cc for new connections, then wait for old ones
```

Both reports are written to be read, with colour and symbols on a terminal and neither when piped. `--json` prints the daemon's raw reply instead, which is what a script should parse. What every line means: [Reading `ssctl status` and `ssctl flows`](#reading-ssctl-status-and-ssctl-flows).

`ssctl flows` lists the TCP connections the kernel currently runs on `skyline_cc` (the 50 that have sent the most; the rest only counted, "N more, not shown") -- peer, RTT, congestion window, pacing rate, delivery rate, bytes sent and the retransmitted share of them -- followed by the coefficients in force on those connections and the counters showing what the algorithm decided. The per-connection numbers are the kernel's own, read back through `ss`: skyline_cc keeps its per-flow state in socket storage, which user space cannot enumerate, but cwnd, pacing rate and RTT are exactly what it writes into the socket.

Over SSH, `ssctl drain` always runs into its timeout, because your own session is a skyline_cc flow. That is harmless: it switches new connections to `fallback_cc` before it starts waiting.

## How it works

On a long-haul link that drops packets at random, for reasons that have nothing to do with queues, a congestion control that slows down on every loss collapses. Even BBR, which estimates bandwidth instead, still falls back to packet conservation in every loss recovery and to a single packet after every timeout. Skyline Speeder replaces the congestion control on the sending server with `skyline_cc`, written in eBPF (`struct_ops`), and works from one assumption: **loss by itself carries no congestion information.**

- **Measure, then send.** Every round it measures the delivery rate and the base RTT, and on every ACK, loss recovery included, it sets the window to `bandwidth × base RTT × gain`. A lost packet never shrinks it.
- **Compensate and pace.** A path that loses a fraction `p` of packets delivers only `1 - p` of what is sent, so the rate is scaled up by `1 / (1 - p)`, to a ceiling, and leaves evenly spaced through the `fq` qdisc rather than in bursts.
- **Two phases.** STARTUP climbs quickly to the path's bandwidth; once the measured bandwidth stops growing, CRUISE holds it.
- **One brake.** Only rising queueing delay and ECN marks count as congestion. Either one drops that round's rate below the measured bandwidth.
- **Short responses first.** Each connection's handshake and first 64 KiB go out twice, 10 ms apart, so one lost packet costs milliseconds instead of a retransmission timer, for about 64 KiB of extra traffic per connection.

A Rust daemon, `skyline-speederd`, loads the BPF programs, which the kernel verifier checks at load, and changes coefficients online; `ssctl` is its command line. Every feature, with its default, its cost and its switch: [Configuration](#configuration). Each mechanism in detail: [docs/03-design.md](docs/03-design.md) (Chinese).

> [!IMPORTANT]
> **The assumption only holds for loss that is independent of queues.** If the loss on your path is queue overflow at a congested bottleneck, loss compensation and the guardrail push in opposite directions and you will make things worse. Rule of thumb: if a fixed-rate UDP flow shows no loss while TCP retransmits heavily, your loss is self-inflicted queue overflow and this is not the tool. Fairness is not a goal either: it does not try to share a link evenly with competing flows.

### How the three differ by design

|  | bbr | tcp-brutal | Skyline Speeder |
|---|---|---|---|
| Control structure | Closed loop, probes bandwidth | **Open loop**: you supply the rate | Closed loop, estimates bandwidth |
| Backs off on loss | Indirectly | No | No |
| Needs the path bandwidth up front | No | **Yes** | No |
| Congestion self-protection | Yes | None | Queueing-delay + ECN guardrail |
| Form | In-kernel | Kernel module (DKMS) | eBPF CO-RE, verifier-checked |
| Kernel floor | any | 5.10 | **6.12 LTS** |

If you know your egress bandwidth, it is fixed and your kernel is old, tcp-brutal is the simpler answer. Skyline Speeder is for bandwidth that is unknown or changes, when you want a congestion guardrail and observability and can run a 6.12+ kernel. Neither belongs on a genuinely congested bottleneck.

## Configuration

The shipped defaults are tuned values; **most deployments should change nothing.** This is everything a default install runs, and where each part is switched:

| Feature | Default | What it does | Switch |
|---|---|---|---|
| [skyline_cc and its modules](#modules) | on, for every new TCP connection | sets window and pacing from measured bandwidth | `ssctl enable`, `ssctl drain` |
| [Coefficients](#coefficients) | tuned values | gains, guardrail, caps, startup behaviour | `ssctl set-module-config` |
| [First-flight redundancy](#first-flight-redundancy) | on | sends each connection's handshake and first 64 KiB twice | `ssctl set-redundancy` |
| [Drift guard](#drift-guard) | on, every 5 s | keeps skyline_cc and `fq` in place | `[guard]` |
| [Dynamic RTO floor and ceiling](#dynamic-rto-floor-and-ceiling) | off | bounds each connection's retransmission timeout | `ssctl set-rack-rto` |
| [Retransmit DSCP marking](#retransmit-dscp-marking) | off | marks retransmissions for the network upstream | `ssctl set-retransmit-dscp` |
| [Global loss-detection sysctls](#global-loss-detection-sysctls) | the host's own | three `net.ipv4` sysctls the daemon can set at start | `[rack_tuning]` |
| [Event log](#event-log) | on, rotated at 8 MiB | one line per mode switch, loss event and guardrail trip | `[runtime] events_max_mib` |

An `ssctl set-*` command takes effect at once and lasts until the daemon restarts; [Keeping a change](#keeping-a-change-across-restarts) makes it permanent. Every line of `ssctl status` and `ssctl flows` is explained [at the end of this section](#reading-ssctl-status-and-ssctl-flows).

### Modules

`skyline_cc` is four modules, all on by default and switched individually. `ssctl status` lists them as M1-M4:

| Module | What it does | Switched off |
|---|---|---|
| `adaptive-cwnd` (M2) | Sets the window from measured bandwidth; this is where the speedup comes from | the window grows and shrinks on loss as in the kernel's own TCP |
| `loss-classifier` (M3) | Loss compensation: scales the estimate up by the measured loss rate instead of backing off | no compensation |
| `pacing` (M4) | Spreads sending evenly at the target rate | the kernel's default pacing rate |
| `early-loss` (M1) | Earlier loss visibility (an observation counter on current kernels) | no difference in what is sent |

```bash
ssctl enable                                  # everything on (default)
ssctl enable --modules adaptive-cwnd,pacing   # selected modules only
ssctl enable --all-off                        # everything off, as a control baseline
ssctl disable --module loss-classifier        # one module off, the others as they are
```

`ssctl enable` without `--modules` keeps the current set, so after `--modules` or `--all-off` name all four to turn everything on again. With all four off, `skyline_cc` behaves like the kernel's own TCP: a neutral baseline for comparisons.

`ssctl enable` takes effect host-wide: on a successful attach it sets `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP connection on the machine uses it, while connections already open keep the algorithm they started with. It also puts `fq` in place, arms the [drift guard](#drift-guard) and starts [first-flight redundancy](#first-flight-redundancy). `ssctl drain` hands new connections to `fallback_cc` and stops both.

### Coefficients

Seventeen parameters change online with `ssctl set-module-config`, no restart. Each open connection picks the new values up at its next round (about two round trips) and starts its estimate over with them. `ssctl flows` shows them under *PARAMETERS IN FORCE*, one line per group:

| `ssctl flows` line | Flags, with defaults | What they set |
|---|---|---|
| pacing rate | `--cruise-pacing-gain 1.25` `--startup-gain 3.0` `--max-pacing-mbps 1200` | The sending rate as a multiple of measured bandwidth in CRUISE and in STARTUP (the STARTUP gain applies to the window too), and a cap per connection in Mbit/s |
| congestion window | `--initial-cwnd-packets 100` `--min-cwnd-packets 32` `--max-cwnd-packets 50000` `--cruise-inflight-gain 3.0` | The initial window (0 keeps the kernel's own), the window's floor and ceiling in packets, and how much may be in flight in CRUISE, as a multiple of bandwidth × base RTT |
| queue guardrail | `--max-queue-delay-ms 70` `--max-queue-delay-ratio 0.6` `--guardrail-gain 0.8` | Queueing delay (smoothed RTT above base RTT) past the larger of 70 ms and 0.6 × base RTT, or an ECN mark, sets that round's gain to 0.8. At 1.0 the guardrail only cancels the boost and never cuts below the measured bandwidth |
| loss compensation | `--loss-inflation-max-ratio 0.10` | The highest loss rate compensated for (0-0.5): rate × 1 / (1 − p), so at most × 1.11 by default |
| bandwidth estimate | `--bw-window-rtts 6` `--min-rtt-window-s 30` | Bandwidth is the highest delivery rate of the last 6 rounds, base RTT the lowest RTT of the last 30 s |
| startup exit | `--startup-growth-ratio 0.20` `--startup-plateau-rtts 5` | STARTUP ends after 5 measured rounds in a row of less than 20% growth, or when the guardrail trips. A measured round is one that put a delivery-rate sample into the estimate; a round in which the connection ran out of data before it filled its window, such as a keep-alive connection's small responses, neither counts nor resets the run |
| framework switches | `--disable-prr-pacing` `--disable-auto-pacing` | Used only with `adaptive-cwnd` or `pacing` off, to make that path behave like the kernel's own; leave them on |

> [!CAUTION]
> `set-module-config` is **absolute-overwrite**, not incremental: every parameter you leave out is reset to the CLI's built-in default, not to the value in your config file. Read the current values under *PARAMETERS IN FORCE* first and pass every one you want to keep; `ssctl reset-module-config` goes back to the config file.

[docs/usage.md](docs/usage.md) has each parameter's permitted range and four ready-made presets, including a "high random loss" preset for links that really do lose 10-20% at random.

### First-flight redundancy

On by default. On a lossy long-haul path, a short response spends most of its time waiting for loss recovery rather than on the wire. A response that fits in the first window arrives in one round trip; one lost segment adds at least another, a lost last segment two or three (the tail-loss probe, then the retransmission timer), and a lost SYN-ACK a fixed second. These timers belong to the kernel, and no window size shortens them.

So `skyline_tc`, on the egress NIC, sends the start of every connection twice:

- every SYN-ACK the host sends while skyline_cc is enabled (a SYN-ACK has no congestion control yet, so this one is host-wide);
- the SYN of every connection this host opens on skyline_cc;
- every segment of a skyline_cc connection that starts within the first 64 KiB it sends, retransmissions included.

The copy leaves 10 ms after the original, held back by `fq`: losses come in bursts, and two packets sent back to back are often lost together. Unless both are lost, a loss in that range costs 10 ms instead of a round trip or a timer. The receiving TCP discards whichever copy arrives second, so the client needs nothing.

**Cost:** about 64 KiB and one handshake packet per connection when nothing is lost. A response under 64 KiB goes out twice in full; a large download pays only for its first 64 KiB or so, since the segment that crosses the boundary is copied whole. Retransmissions in that range are copied too, so the more is lost the more is sent, with no fixed cap per connection. Turn it off where egress is billed by volume, or on a path that does not lose packets.

```bash
sudo ssctl set-redundancy --first-kib 32 --delay-ms 5   # absolute: a flag left out takes its default (64 KiB, 10 ms)
sudo ssctl set-redundancy --disable                     # off
sudo ssctl reset-redundancy                             # back to [redundancy] in the config file
```

In the config file these are `enabled`, `first_kib` (1-1024) and `delay_ms` (0-100; 0 sends the copy right behind the original) under `[redundancy]`. Copying runs only while skyline_cc is attached: `ssctl enable` starts it and `ssctl drain` stops it. It needs `skyline_tc` on an Ethernet `tc_interface`, and without `fq` as the root qdisc the copy leaves at once.

`ssctl flows` shows it twice. Under *PARAMETERS IN FORCE*, `first-flight copies` says whether it runs: `handshake + first 64 KiB of each connection sent twice, the copy +10 ms`; `configured, not running` when it should run and does not (the TC program is not loaded, or the last `ssctl enable` said why); `stopped by the drain` while a drain waits for skyline_cc's last connections; or `off`. Under *TC EGRESS*, `first-flight copies` says what it has cost since the daemon started: `52,113 sent, 58.2 MiB (9,870 handshakes)` is the number of copies (a GSO packet counts once), their bytes, and how many of them were SYNs or SYN-ACKs. A trailing `N could not be made` counts copies that failed and should never appear.

### Drift guard

On while skyline_cc is attached. `ssctl enable` sets three things and the daemon then keeps them: `net.ipv4.tcp_congestion_control = skyline_cc`; `net.core.default_qdisc = fq`, the qdisc pacing goes through; and `fq` as the egress NIC's root qdisc (for a VLAN, bond or bridge, that of the physical NICs under it). Until `ssctl drain`, it checks them every 5 seconds and puts back whatever something else changed, for instance a "one-click BBR" script's sysctl file being applied again, and logs each correction (`journalctl -u skyline-speederd | grep guard:`). A qdisc built on purpose (`htb`, `tbf`, `netem`, a `cake` with a bandwidth set, ...) is left alone.

```toml
[guard]
interval_s = 5    # 0: check only at ssctl enable
qdisc = true      # false: leave every qdisc to you and keep only the congestion control
```

`ssctl status` shows it under *DRIFT GUARD*: `ARMED`, each of the three with its current value and how many times it was put back, the last correction, the last error (kept until the next one, so it can be old), and a `note` for anything it deliberately left alone.

### Dynamic RTO floor and ceiling

Off by default. When no ACK comes back in time, TCP resends after its retransmission timeout (RTO) and doubles the timeout after every further miss, up to 120 seconds. Where losses are random, the next resend is no less likely to get through than the last, so a connection waiting that long is simply stuck. `skyline_policy` can bound the RTO of each connection:

- **Floor.** The kernel's minimum RTO is 200 ms by default. This sets it per connection to its RTT × `srtt_permille` / 1000, clamped between `floor_us` and `ceiling_us` (200 ms at most), once `warmup_samples` RTT samples are in. Works on 6.12.
- **Ceiling.** Caps the backed-off RTO at `rto_max_normal_permille` / 1000 × the connection's RTT, clamped to 1-120 s. It is worked out again at every RTT sample, and a sample that shows congestion (a new ECN mark, or a smoothed RTT above `rto_max_congestion_ratio_permille` / 1000 times the minimum, 2× by default) uses `rto_max_congested_permille` instead. 0 leaves the ceiling off. **It needs Linux 6.15 or newer**: 6.12 refuses the socket option, and only `rack_rto.stats.rto_max_rejected` in `ssctl flows --json` grows.

For example (`set-rack-rto` is absolute too: a flag left out takes its default):

```bash
sudo ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 --warmup-samples 4 \
    --rto-max-normal-permille 3000 --rto-max-congested-permille 6000
sudo ssctl set-rack-rto --disable
```

On a connection at 120 ms this sets a floor of 1.1 × 120 ms = 132 ms and a ceiling of 3 × 120 ms = 360 ms, raised to the 1 s minimum; once its smoothed RTT passes twice the minimum, at 250 ms say, the ceiling becomes 6 × 250 ms = 1.5 s.

Three things to know before relying on it:

- Only connections of processes inside `/sys/fs/cgroup/skyline-speeder` (`runtime.cgroup_path`) pass through `skyline_policy`, and a process outside it reports **no error**: `ssctl flows` simply shows 0 under *connections seen*, a row it shows whether dynamic RTO is on or off. Start the service inside it with `sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <your service command...>`; for a systemd service, see [DEPLOY.md](DEPLOY.md) §7. `skyline_cc` itself is global and needs no cgroup.
- **The daemon always starts with it off**, whatever `[rack_rto]` in the config says. After every start, run `ssctl reset-rack-rto`, which applies `[rack_rto]` from the file, or `ssctl set-rack-rto`.
- It works under any congestion control, and `ssctl drain` does not stop it.

`ssctl flows` shows `dynamic RTO floor` and `dynamic RTO ceiling` under *PARAMETERS IN FORCE*, and a *DYNAMIC RTO* section (*CGROUP POLICY* while it is off, with `connections seen` alone): `applied` counts floors set (then how many the kernel rejected, how many waited for warm-up and how many were already in place), and `connections seen` the connections that passed through `skyline_policy`. `applied` growing, or `warmup` on connections too short to get past the warm-up, is what shows it is running.

### Retransmit DSCP marking

Off by default. Marks every retransmitted TCP segment leaving `tc_interface` with a DSCP codepoint, so that routers upstream can treat retransmissions differently, for instance carry them on a cleaner path, where your network has such a policy. It applies to all TCP on that interface whatever its congestion control, IPv4 and IPv6 alike, and `ssctl drain` does not stop it.

> [!WARNING]
> The shipped `dscp_value = 0` is a placeholder: DSCP 0 is best effort, so marking with it does nothing. The real value must come from whoever runs the network. A value already used for something else can get your traffic misclassified and policed.

```bash
sudo ssctl set-retransmit-dscp --dscp-value <value>   # 1-63, from your network; this also turns marking on
sudo ssctl set-retransmit-dscp --disable              # off, whatever the config file says
sudo ssctl reset-retransmit-dscp                      # back to [retransmit_dscp] in the config file: off unless set there
```

Like the RTO bounds, `[retransmit_dscp]` in the config file is not applied when the daemon starts: run `ssctl reset-retransmit-dscp` after every start. Marking needs `skyline_tc` on an Ethernet `tc_interface`; on a tunnel device the command fails and `ssctl status` says why under *ATTENTION*. `ssctl flows` shows `retransmit DSCP  codepoint N on every TCP retransmission` under *PARAMETERS IN FORCE* and `retransmits marked  N of M detected` under *TC EGRESS*: retransmissions are counted only while marking runs, so `M` should grow with traffic, and `N` should equal it.

### Global loss-detection sysctls

The host's own values by default. A `[rack_tuning]` section in the config makes the daemon write `net.ipv4.tcp_recovery`, `tcp_reordering` and `tcp_early_retrans`, the kernel's loss-detection settings (RACK, the reordering threshold, tail-loss probes), when it starts; if a write is refused, it does not start. They are global: they apply to every TCP connection on the host, whatever its congestion control. The installed config has no such section. `ssctl status` lists the three under *GLOBAL SYSCTLS (M1 tier-1)*, marked `(host)` while the daemon does not own them; for values it owns, it warns when something has changed them since it started (restart it to write them again).

### Event log

On by default. The mode switches, loss events, guardrail trips, window undos and configuration switches of every skyline_cc connection go into `/run/skyline-speeder/events.jsonl`, one JSON line each, keyed by socket cookie. `/run` lives in memory, so past `events_max_mib` (8 MiB, under `[runtime]`) the file is rotated to `events.jsonl.1`, replacing the one before, so the log never takes much more than twice that; 0 turns the log off, and a change needs a daemon restart. The event codes are in [docs/02-interface-reference.md](docs/02-interface-reference.md) §8 (Chinese); guardrail trips are type 9, which `ssctl flows` points to under *guardrail trips*.

### Keeping a change across restarts

`ssctl` changes live in the daemon's memory and are gone after a restart or an upgrade. To keep one, put it in `/etc/skyline-speeder/speeder.toml` under the flag's name with `_` for `-`: `max_*`, `initial_cwnd_packets`, `min_cwnd_packets`, `prr_pacing_enabled` and `auto_pacing_enabled` at the top level, `loss_inflation_max_ratio` under `[loss_classifier]`, the gains and the bandwidth-estimate settings under `[adaptive_cwnd]`. Then check the file and restart:

```bash
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only
sudo systemctl restart skyline-speederd    # skyline_cc is attached again by the enable service
```

From 0.5.0, a misspelt key or one in the wrong section fails `--validate-only`, which names the section it belongs in. `[rack_rto]` and `[retransmit_dscp]` differ in two ways: in the file they also need `enabled = true`, which their `set-*` commands imply, and after the restart only `ssctl reset-rack-rto` and `ssctl reset-retransmit-dscp` apply them.

### Reading `ssctl status` and `ssctl flows`

`ssctl status` answers *has this host been taken over?*

| Line | Meaning |
|---|---|
| top line | The running daemon's version (after an upgrade, `ssctl --version` can differ until the daemon restarts) and the verdict: `ACCELERATING`, attached and the host default; `ATTACHED, NOT DEFAULT`, something changed the default since and new connections bypass skyline_cc, so run `ssctl enable`; `DRAINING`, a drain has moved new connections to `fallback_cc` and skyline_cc only serves the connections it still has (one run over SSH always ends here; `ssctl enable` takes the host back); `STANDBY`, not attached, so run `ssctl enable` |
| skyline_cc struct_ops | attached, and how long ago |
| host default cc | `net.ipv4.tcp_congestion_control`: what new connections get |
| fallback on drain | `fallback_cc`: what new connections get after `ssctl drain`; a fresh install sets it to the one the host's sysctl files select at boot, when this kernel has it |
| modules | M1-M4; a tick means acting on traffic now, so nothing is ticked while detached |
| live connections | connections on skyline_cc now |
| DRIFT GUARD | see [Drift guard](#drift-guard) |
| KERNEL | the kernel release; every item on `required` must be ticked. `RACK reorder hook` under `optional` needs a patched kernel, and grey is normal |
| GLOBAL SYSCTLS | see [Global loss-detection sysctls](#global-loss-detection-sysctls) |
| DAEMON | uptime, and the configuration generation, which goes up whenever the modules, the coefficients or the RTO bounds change |
| ATTENTION | what needs a look, with the command to run where there is one. On a stock kernel with `early-loss` on, it always says that early-loss remains observation-only: expected, nothing to do |

`ssctl flows` answers *what is being accelerated?*

| Line | Meaning |
|---|---|
| top line | `ACCELERATING`; `IDLE`, attached but no connection on skyline_cc yet; `DRAINING`, a drain is waiting for the connections listed; `BYPASSED`, attached but not the host default; `NOT ACCELERATING`, not attached |
| coverage | the share of the host's TCP connections that run on skyline_cc |
| volume | bytes sent and retransmitted by the connections listed, from the kernel's own counters |
| the table | per connection: RTT, window in packets, the pacing rate skyline_cc set, the kernel's delivery-rate estimate, bytes sent and the retransmitted share of them |
| PARAMETERS IN FORCE | the [coefficients](#coefficients), then the RTO bounds, DSCP marking and first-flight copies |
| window | the counters below run from the attach |
| ack decisions | ACKs skyline_cc has acted on |
| loss events | ACKs on which a loss was detected, as a share of ack decisions |
| guardrail trips | rounds in which queueing delay or an ECN mark set the gain to `guardrail_gain`, as a share of the rounds checked |
| cwnd at the cap | ACKs after which the window sat at `max_cwnd_packets`: a limit, not congestion |
| last 1m | the same three over the last minute: whether it is still going on |
| mode switches | changes between STARTUP and CRUISE |
| pacing updates | times the pacing rate was recomputed |
| delivery samples | a sum of overlapping rate samples, for comparing two runs; not a packet count |
| PRR adjustments | appears only when it is not 0; with `adaptive-cwnd` on, that is a bug worth reporting |
| TC EGRESS | since the daemon started, everything leaving `tc_interface` whatever its congestion control: `seen` counts packets, bytes and GSO packets; then [`retransmits marked`](#retransmit-dscp-marking) and [`first-flight copies`](#first-flight-redundancy) |
| DYNAMIC RTO, or CGROUP POLICY while it is off | see [Dynamic RTO floor and ceiling](#dynamic-rto-floor-and-ceiling); `connections seen` is how to tell whether any process is in the cgroup |

`--json` prints the daemon's raw reply instead, for scripts. `ssctl snapshot <file>` has the daemon write the `status` part of it to a regular file (under systemd, `/run/skyline-speeder/` is the safe place), and `ssctl validate` checks the daemon's configuration in memory against the kernel.

## Repository layout

```
skyline-speeder/
├── install.sh                    one-command install (Debian/Ubuntu, Fedora/RHEL, Alpine)
├── scripts/bootstrap.sh          remote installer entry point (curl | sudo bash, or wget | sh)
├── Makefile                      make bpf, make check, make test
├── bpf/
│   ├── skyline_cc.bpf.c          struct_ops congestion control, runs on every ACK
│   ├── skyline_policy.bpf.c      cgroup sockops: CC selection and dynamic RTO floor/ceiling
│   ├── skyline_tc.bpf.c          TC egress: accounting, retransmit DSCP marking, first-flight redundancy
│   └── include/skyline_abi.h     ABI shared by BPF and userspace
├── crates/
│   ├── skyline-speederd/         resident control plane
│   ├── ssctl/                    command line
│   └── skyline-common/           shared ABI types and config parsing
├── config/                       speeder-guest.toml (installed template) / speeder.toml (development)
├── packaging/                    systemd units; openrc/ holds the same two services for OpenRC
├── infra/                        install helpers, cgroup wrapper, two-VM test bed
├── research/experiments/         test manifests, runner, analysis, field measurements
├── docs/                         usage guide, interface reference, design, performance report
├── DEPLOY.md                     deployment and operations manual, written for automation agents
└── CONTRIBUTING.md               invariants a change must not break
```

## Building from source

`sudo ./install.sh --source` does all of this and installs the result. By hand:

```bash
sudo apt-get install -y build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool
# Fedora / RHEL (libbpf-devel is in CodeReady Builder there: add --enablerepo=crb):
#   sudo dnf install gcc make pkgconf-pkg-config clang llvm libbpf-devel elfutils-libelf-devel zlib-devel bpftool
# Alpine (bpftool is in the community repository):
#   apk add build-base pkgconf clang llvm libbpf-dev elfutils-dev zlib-dev linux-headers bpftool
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

make bpf                          # generate vmlinux.h, compile the three CO-RE objects
cargo build --workspace --release
make check PYTHON="$(command -v python3)"   # script syntax, the harness tests (Python 3.11+), formatting, cargo check; cargo test is `make test`

# Run every object through the kernel verifier, attaching nothing
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

On a Debian 12 host running a 6.12 kernel from `bookworm-backports`, `libelf-dev` has to come from backports as well — the backports `linux-headers` bring `libelf1 0.192` with them and bookworm's `libelf-dev` pins `libelf1` to 0.188, so apt can install neither (`apt-cache madison libelf-dev` lists the version to ask for). `install.sh` works that out on its own.

`make bpf` reads type information from this machine's `/sys/kernel/btf/vmlinux`. Use `make VMLINUX_BTF=<path> bpf` for another kernel's BTF, or `make PREBUILT_VMLINUX_H=<path> bpf` for a ready-made header.

## Documentation

The documents below are in English; only this README has a Chinese version, [README.zh.md](README.zh.md).

| Document | Contents |
|---|---|
| [docs/usage.md](docs/usage.md) | Beginner-facing: every switch and parameter, presets, troubleshooting |
| [DEPLOY.md](DEPLOY.md) | The deployment and operations manual, written for automation agents and followed by operators too: prerequisites, install, verification gates, upgrade, troubleshooting, removal |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` commands, wire protocol, config fields, event codes |
| [docs/03-design.md](docs/03-design.md) | Architecture and how each module works |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Controlled test bed: environment, results against bbr, limitations |
| [research/experiments/README.md](research/experiments/README.md) | Reproducing the performance tests |

## Measured performance

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) and skyline_cc on one real cross-border path, each applied on the server only, through the same per-destination route override. The client ran stock TCP throughout.

| | Where | Role |
|---|---|---|
| **Server** | KVM VPS in **Singapore** · 10 Gbps | sends; the only side that is accelerated |
| **Client** | **Alibaba Cloud** ECS, **South China (Guangdong)** · 200 Mbps | receives; stock TCP, untouched |

RTT 70-73 ms, 10-30% loss, measured at evening peak. skyline_cc ran the coefficients that shipped as the default in 0.1.0, now the "high random loss" preset in [docs/usage.md](docs/usage.md); **today's defaults have not been run on this path**. tcp-brutal was v2.0.0 at 200 Mbps; bbr was the kernel's own.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal and skyline_cc measured against a bbr baseline across game, video, web and bulk traffic" src="docs/images/single-ended-ataglance-light.svg">
</picture>

8 rotations × 3 algorithms × 5 scenarios = 120 runs, 119 completed. The algorithm order changed every rotation, so a path that degrades mid-test cannot favour whoever went first. Winners are counted per rotation:

| Scenario | Winner | Margin |
|---|---|---|
| **Video** (6 MB chunks @ 48 Mbps) | **tcp-brutal** | startup 7/8 rotations, chunk p95 6/8 |
| **Game** (ping-pong, 60 msg/s) | **skyline_cc** | jitter 5/8, stalls >150 ms 4/8 |
| **Bulk, 4 streams** | skyline_cc and tcp-brutal, both ahead of bbr | median 117 / 115 vs 77 Mbps |
| **Bulk, 1 stream** | bbr and skyline_cc tie; tcp-brutal last | 4/8, 4/8, 0/8 rotations; median 59 / 53 / 46 Mbps |
| **Web page load** | no stable winner | rotation-to-rotation spread exceeds the difference |
| **Server retransmissions**, share of all segments sent, all scenarios | bbr lowest | bbr 9.1%, skyline_cc 10.4%, tcp-brutal 10.9% |

The single-stream result says the most about the three designs. **tcp-brutal sent the most and delivered the least: 12.2% of its segments were retransmissions, against 9.0% for bbr and 10.0% for skyline_cc, for a median of 46 Mbps.** Its fixed 200 Mbps is far above what this path delivers at 10-30% loss, and an open loop that sends harder when packets are lost turns that gap into pure retransmission. skyline_cc estimates the rate itself, so there is no target rate to set too high.

> [!NOTE]
> This is a field measurement on one path, not the controlled two-VM test bed that formal results in this repository come from, and it is not a claim about your link. Its 70 ms RTT is also below this project's target of 100-300 ms, where bbr recovers from loss far more easily. Read it as evidence about how the three algorithms behave, not as a throughput benchmark. Per-rotation numbers and raw records: [research/experiments/single-ended/](research/experiments/single-ended/). Controlled test-bed results: [docs/04-performance-report.md](docs/04-performance-report.md) (Chinese).

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. This project has hard invariants: an ABI struct that must stay byte-identical on both sides, a registration name that must stay under 16 bytes, a systemd `RuntimeDirectory` coupled to a socket path, and three failure modes that report no error at all. The kernel verifier is a hard gate: a change that does not load is not a change.

## License

Copyright (c) 2026 **CYBERVERSE LLC**. Distributed under **GPL-2.0**; see
[LICENSE](LICENSE).

GPL-2.0 is not a preference here. Parts of `skyline_cc.bpf.c` are close
reimplementations of GPL-2.0 Linux kernel algorithms, cited by file and line
in the source, and the BPF objects declare `GPL` to the kernel. The full
reasoning and the upstream attributions are in [NOTICE](NOTICE).

"Skyline Speeder" and "CYBERVERSE" are trademarks of CYBERVERSE LLC; the GPL
grant over the code does not grant any right to use them.

---

<div align="center">

Exclusively sponsored by **[Skyline Connect](https://www.skylineconnect.io)**

</div>
