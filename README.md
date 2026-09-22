<div align="center">

# Skyline Speeder

**Sender-side TCP acceleration for high-loss, high-latency links, built on eBPF**

*Deploy on the sending server only. Clients stay on stock TCP and need no software, no patches, no configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#kernel-requirement)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-black.svg)](rust-toolchain.toml)

English · **[简体中文](README.zh.md)**

---

### Exclusively sponsored by **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## What this is

Skyline Speeder is single-sided TCP acceleration software. It runs as eBPF programs on the server that sends the data, and is meant for links with heavy packet loss and long round-trip times, where standard TCP collapses. It raises throughput on such links by keeping the sending rate at what the path actually delivers, instead of cutting it back every time a packet is lost. The receiving side is untouched.

### How it works

BBR estimates bandwidth rather than reacting to each loss, yet it still gives ground to loss: the kernel's BBR falls back to packet conservation at the start of every loss recovery, and to a single packet after every retransmission timeout. On a link that drops 10-20% of packets at random, for reasons that have nothing to do with queues, recovery and timeouts become the normal state, and at 100-300 ms RTT its throughput swings from run to run and often collapses. Skyline Speeder replaces the congestion control on the sender, through a `tcp_congestion_ops` implemented in eBPF (`struct_ops`), and works from one assumption: **loss by itself carries no congestion information.**

- **Rate from measurement, not from loss.** Every round it measures the delivery rate (the peak of recent rounds) and the base RTT. On every ACK, in every TCP state including loss recovery, it sets the window directly to `bandwidth × base RTT × gain`. Loss by itself never shrinks it, and there is no packet-conservation phase to climb back out of.
- **Loss compensation.** On a path that loses a fraction `p` of packets, the delivery rate understates what has to be sent by `(1 - p)`. The window and the rate are scaled up by `1 / (1 - p)`, using the flow's own measured loss rate, up to a configured ceiling.
- **Pacing.** Data leaves at `bandwidth × gain × compensation` through the `fq` qdisc, evenly spaced rather than in bursts, under a hard rate cap. The window only has to stay out of the way.
- **Two phases.** STARTUP uses a high gain to find the path's bandwidth quickly. Once bandwidth stops growing for a few rounds, the flow settles into CRUISE.
- **A congestion guardrail.** Only two signals count as real congestion: queueing delay rising above a threshold (a fixed value or a fraction of the base RTT, whichever is larger), and ECN marks. When either fires, that round's gain drops to `guardrail_gain`, below the measured bandwidth, and the next clean round restores it.
- **A bounded RTO** (optional). A cgroup sockops program caps the kernel's retransmission timeout per connection, so a flap cannot leave a connection waiting up to two minutes.

A Rust daemon, `skyline-speederd`, loads the BPF objects (the kernel verifier checks each one at load) and pushes new coefficients online through a double-buffered slot that each flow switches to at an RTT boundary. `ssctl` is its command line.

> [!IMPORTANT]
> **The assumption only holds for loss that is independent of queues.** If the loss on your path is queue overflow at a congested bottleneck, loss compensation and the guardrail push in opposite directions and you will make things worse. Rule of thumb: if a fixed-rate UDP flow shows no loss while TCP retransmits heavily, your loss is self-inflicted queue overflow and this is not the tool. Fairness is not a goal either: it does not try to share a link evenly with competing flows.

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
| **Server retransmissions**, all scenarios | bbr lowest | bbr 10.0%, skyline_cc 11.6%, tcp-brutal 12.3% |

The single-stream result says the most about the three designs. **tcp-brutal sent the most and delivered the least: 13.9% of its segments were retransmissions, against 9.9% for bbr and 11.1% for skyline_cc, for a median of 46 Mbps.** Its fixed 200 Mbps is far above what this path delivers at 10-30% loss, and an open loop that sends harder when packets are lost turns that gap into pure retransmission. skyline_cc estimates the rate itself, so there is no target rate to set too high.

> [!NOTE]
> This is a field measurement on one path, not the controlled two-VM test bed that formal results in this repository come from, and it is not a claim about your link. Its 70 ms RTT is also below this project's target of 100-300 ms, where bbr recovers from loss far more easily. Read it as evidence about how the three algorithms behave, not as a throughput benchmark. Per-rotation numbers and raw records: [research/experiments/single-ended/](research/experiments/single-ended/). Controlled test-bed results: [docs/04-performance-report.md](docs/04-performance-report.md) (Chinese).

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

## Kernel requirement

**Linux 6.12 LTS or newer**, with `/sys/kernel/btf/vmlinux` present. `skyline_cc` implements the four-argument `cong_control(sk, ack, flag, rs)` callback, which exists only from 6.10; older kernels reject the program at load time. That rules out the 6.1 and 6.6 LTS branches and the stock Debian 12 and Ubuntu 22.04/24.04 kernels.

Verified: `6.12.101`, `6.18.42` and `7.1.6` pass, including IPv4/IPv6 data-path smoke tests. `6.1.180` and `6.6.148` fail to load, by design.

## Quick start

```bash
# Build from source on this host
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash

# Or install the published release, no build toolchain
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --prebuilt
```

A minimal image may lack `curl` (`apt-get install -y curl`) or `sudo` (drop it if you are root). To read the scripts before running them, which is **recommended** since this changes the congestion control of every new connection on the machine:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh              # build from source
sudo ./install.sh --prebuilt   # published release artifacts, no toolchain
sudo ./install.sh --check      # preflight only, changes nothing
sudo ./install.sh --no-enable  # install without attaching skyline_cc
sudo ./install.sh --uninstall  # remove (keeps /etc/skyline-speeder)
```

The installer checks the kernel, builds or downloads the three BPF objects and the daemon, points the TC program at the default-route interface, runs every object through the kernel verifier, then starts the daemon, attaches `skyline_cc` and enables both at boot. It records the congestion control and qdisc in use beforehand, and `--uninstall` restores them. **It installs no proxy and opens no port.**

`--prebuilt` needs only `curl` and `tar`: the objects are CO-RE, compiled against a pinned 6.12 header and relocated against this kernel when they load. The prebuilt daemon is built on Ubuntu 24.04 and needs glibc 2.38 or newer with `libelf.so.1` and `libz.so.1` (Debian 13, Ubuntu 24.04 and later); on older userspace, build from source. `--release <tag>` pins a version, and `SKYLINE_ARTIFACT_URL=/path/to/tarball sudo -E ./install.sh --prebuilt` installs a copied artifact on a host with no route to GitHub.

> [!IMPORTANT]
> **Upgrading:** re-running the installer replaces the files but does not restart a daemon that is already running, so the old version keeps running. Stop the services first with `sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service`, or restart `skyline-speederd` afterwards. Details: [CHANGELOG.md](CHANGELOG.md), *Upgrading from 0.1.0*.

Day-to-day:

```bash
ssctl status    # runtime state, kernel capabilities, decision counters
ssctl flows     # number of active skyline_cc flows
ssctl drain     # graceful detach: fallback_cc for new connections, then wait for old ones
```

Over SSH, `ssctl drain` always runs into its timeout, because your own session is a skyline_cc flow. That is harmless: it switches new connections to `fallback_cc` before it starts waiting.

## Configuration

The shipped defaults are tuned values; **most deployments should change nothing.**

**Modules.** All four are on by default and can be switched individually:

| Module | What it does |
|---|---|
| `adaptive-cwnd` | Sets the window from measured bandwidth; this is where the speedup comes from |
| `loss-classifier` | Loss compensation: scales the estimate up by the measured loss rate instead of backing off |
| `pacing` | Spreads sending evenly at the target rate |
| `early-loss` | Earlier loss visibility (an observation counter on current kernels) |

```bash
ssctl enable                                  # everything on (default)
ssctl enable --modules adaptive-cwnd,pacing   # selected modules only
ssctl enable --all-off                        # everything off, as a control baseline
```

`ssctl enable` takes effect host-wide: on a successful attach it sets `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP connection on the machine uses it.

**Coefficients.** Seventeen parameters (gains, guardrail, loss-compensation ceiling, queueing-delay threshold, startup behaviour, rate and window caps) change online with `ssctl set-module-config`, no restart. [docs/usage.md](docs/usage.md) (Chinese) explains each one and has four ready-made presets, including a "high random loss" preset for links that really do lose 10-20% at random.

> [!CAUTION]
> `set-module-config` is **absolute-overwrite**, not incremental: every parameter you leave out is reset to the CLI's built-in default, not to the value in your config file.

**Dynamic RTO floor and ceiling** (off by default). Only connections from processes inside `/sys/fs/cgroup/skyline-speeder` pass through `skyline_policy`, and a process outside it reports **no error**: `rack_rto.stats.applied` in `ssctl status` simply stays at 0. Start the service inside it:

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <your service command...>
```

`skyline_cc` itself is global and needs no cgroup.

**Retransmit DSCP marking** (off by default).

> [!WARNING]
> The shipped `dscp_value = 0` is a placeholder: DSCP 0 is best effort, so marking with it does nothing. The real value must come from whoever runs the network. A value already used for something else can get your traffic misclassified and policed.

## Repository layout

```
skyline-speeder/
├── install.sh                    one-command install (Debian/Ubuntu)
├── scripts/bootstrap.sh          remote installer entry point (curl | sudo bash)
├── bpf/
│   ├── skyline_cc.bpf.c          struct_ops congestion control, runs on every ACK
│   ├── skyline_policy.bpf.c      cgroup sockops: CC selection and dynamic RTO floor/ceiling
│   ├── skyline_tc.bpf.c          TC egress: accounting and retransmit DSCP marking
│   └── include/skyline_abi.h     ABI shared by BPF and userspace
├── crates/
│   ├── skyline-speederd/         resident control plane
│   ├── ssctl/                    command line
│   └── skyline-common/           shared ABI types and config parsing
├── config/                       speeder-guest.toml (installed template) / speeder.toml (development)
├── packaging/                    systemd units
├── infra/                        install helpers, cgroup wrapper, two-VM test bed
├── research/experiments/         test manifests, runner, analysis, field measurements
├── docs/                         usage guide, deployment, interface reference, design, performance report
├── CHANGELOG.md                  release notes and upgrade instructions
├── DEPLOY.md                     deterministic deployment manual for automation agents
└── CONTRIBUTING.md               invariants a change must not break
```

## Building from source

```bash
sudo apt-get install -y build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

make bpf                          # generate vmlinux.h, compile the three CO-RE objects
cargo build --workspace --release
make check                        # formatting, static checks, unit tests

# Run every object through the kernel verifier, leaving no runtime state
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

`make bpf` reads type information from this machine's `/sys/kernel/btf/vmlinux`. Use `make VMLINUX_BTF=<path> bpf` for another kernel's BTF, or `make PREBUILT_VMLINUX_H=<path> bpf` for a ready-made header.

## Documentation

The documents under `docs/` are in Chinese.

| Document | Contents |
|---|---|
| [docs/usage.md](docs/usage.md) | Beginner-facing: every switch and parameter, presets, troubleshooting |
| [docs/01-deployment-guide.md](docs/01-deployment-guide.md) | Build, install, configure, verify, tune, roll back |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` commands, wire protocol, config fields, event codes |
| [docs/03-design.md](docs/03-design.md) | Architecture and how each module works |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Controlled test bed: environment, results, neutrality, limitations |
| [research/experiments/README.md](research/experiments/README.md) | Reproducing the performance tests |
| [DEPLOY.md](DEPLOY.md) | Deterministic deployment manual for automation agents |

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
