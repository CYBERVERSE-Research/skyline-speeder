<div align="center">

# Skyline Speeder

**Sender-side TCP acceleration for high-loss, high-latency links, built on eBPF**

*Deploy on the sending server only. Clients stay on stock TCP and need no software, no patches, no configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#requirements)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

**English** · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md) · [Русский](README.ru.md) · [Français](README.fr.md)

---

### Exclusively sponsored by **[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## What this is

Skyline Speeder is single-sided TCP acceleration software. It runs as eBPF programs on the server that sends the data, and is meant for links with heavy packet loss and long round-trip times, where standard TCP collapses. The receiving side is untouched.

It replaces the server's congestion control with `skyline_cc`, built on one assumption: **loss by itself carries no congestion information.**

- **Measure, then send:** the window follows measured bandwidth × base RTT, and a lost packet never shrinks it.
- **Compensate and pace:** the rate is scaled up for the measured loss rate, to a ceiling, and leaves evenly spaced through `fq`.
- **One brake:** only rising queueing delay and ECN marks slow it down.
- **Short responses first:** each connection's handshake and first 64 KiB go out twice, 10 ms apart, so one lost packet costs milliseconds instead of a retransmission timer.

> [!IMPORTANT]
> This helps only where loss is independent of queues. If a fixed-rate UDP flow shows no loss while TCP retransmits heavily, your loss is queue overflow at a congested bottleneck, and this is not the tool.

## Requirements

- **Linux 6.12 LTS or newer**, with `/sys/kernel/btf/vmlinux`. The 6.1 and 6.6 LTS branches and the stock Debian 12 and Ubuntu 22.04/24.04 kernels are too old.
- A host or a KVM virtual machine, not a container (LXC, Proxmox CT).
- Debian, Ubuntu, Fedora, the RHEL family (Rocky Linux, AlmaLinux, CentOS Stream) or Alpine.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

On Alpine, as root:

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

This installs the latest published release, with no compiler put on the host, runs every BPF object through the kernel verifier, makes `skyline_cc` the congestion control of every new TCP connection and keeps it so across reboots. It installs no proxy and opens no port. Then check:

```bash
ssctl status    # has this host been taken over? ACCELERATING means yes
ssctl flows     # which connections are accelerated, and what the algorithm decided
```

To remove it, which puts the host on bbr + fq:

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## Advanced usage

**Install from a clone**, to read the scripts before running them:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # preflight only, changes nothing
sudo ./install.sh              # the published release (builds only where it cannot run)
sudo ./install.sh --source     # build this tree from source
sudo ./install.sh --release v0.6.0   # that published release, or stop
sudo ./install.sh --uninstall --restore-pre-install   # remove, and put back the congestion control and qdisc from before
```

To upgrade, run the installer again. It drains live flows for up to 60 seconds and attaches `skyline_cc` again.

**Day to day:**

```bash
sudo ssctl drain                                # graceful detach: new connections go to fallback_cc
sudo ssctl enable                               # attach again
sudo ssctl enable --all-off                     # skyline_cc with every module off: a baseline for comparisons
sudo ssctl set-redundancy --disable             # stop sending first flights twice, where egress is billed by volume
sudo ssctl set-module-config --max-pacing-mbps 500   # absolute: every flag left out goes back to its default
```

The shipped defaults are tuned values, and most deployments should change nothing. An `ssctl set-*` change lasts until the daemon restarts; to keep it, put it in `/etc/skyline-speeder/speeder.toml`.

**Documentation:**

| Document | Contents |
|---|---|
| [docs/01-features.md](docs/01-features.md) | What the installer does, how it works, every feature with its default, cost and switch, every line of `ssctl status` and `ssctl flows`, building from source |
| [docs/usage.md](docs/usage.md) | Every switch and parameter with its range, four presets, troubleshooting |
| [DEPLOY.md](DEPLOY.md) | The deployment and operations manual: install, verification, upgrade, removal |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` commands, wire protocol, config fields, event codes |
| [docs/03-design.md](docs/03-design.md) | Architecture and how each module works |
| [docs/04-performance-report.md](docs/04-performance-report.md) | The controlled two-VM test bed against bbr |

## Measured performance

bbr, [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) and skyline_cc, each on the server only, on one real cross-border path: a KVM VPS in Singapore sending to an Alibaba Cloud ECS in South China (200 Mbps) that ran stock TCP. RTT 70-73 ms, 10-30% loss, at evening peak; 8 rotations × 3 algorithms × 5 scenarios.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal and skyline_cc measured against a bbr baseline across game, video, web and bulk traffic" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| Scenario | Winner | Margin |
|---|---|---|
| **Video** (6 MB chunks @ 48 Mbps) | **tcp-brutal** | startup 7/8 rotations, chunk p95 6/8 |
| **Game** (ping-pong, 60 msg/s) | **skyline_cc** | jitter 5/8, stalls >150 ms 4/8 |
| **Bulk, 4 streams** | skyline_cc and tcp-brutal, both ahead of bbr | median 117 / 115 vs 77 Mbps |
| **Bulk, 1 stream** | bbr and skyline_cc tie; tcp-brutal last | median 59 / 53 / 46 Mbps |
| **Web page load** | no stable winner | rotation-to-rotation spread exceeds the difference |
| **Server retransmissions**, share of all segments sent | bbr lowest | bbr 9.1%, skyline_cc 10.4%, tcp-brutal 10.9% |

A field measurement on one path, not a claim about your link: skyline_cc ran the 0.1.0 defaults, now the "high random loss" preset. Method, conditions and the full results: [docs/01-features.md](docs/01-features.md#measured-performance-in-detail).

## License

GPL-2.0, see [LICENSE](LICENSE) and [NOTICE](NOTICE); copyright (c) 2026 **CYBERVERSE LLC**. "Skyline Speeder" and "CYBERVERSE" are trademarks of CYBERVERSE LLC. Before contributing, read [CONTRIBUTING.md](CONTRIBUTING.md).

---

<div align="center">

Exclusively sponsored by **[Skyline Connect](https://www.skylineconnect.io)**

</div>
