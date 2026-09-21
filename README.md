<div align="center">

# Skyline Speeder

**Sender-side TCP acceleration: eBPF struct_ops congestion control with a Rust control plane**

*Deploy on the sending server only. Clients stay on stock TCP and need no software, no patches, no configuration.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#kernel-requirement)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-black.svg)](rust-toolchain.toml)

English · **[简体中文](README.zh.md)**

---

### Exclusively sponsored by **Skyline Connect**

---

</div>

## What this is

Four parts, deployed only on the machine that *sends* the data:

| Component | Form | Job |
|---|---|---|
| `skyline_cc` | eBPF struct_ops congestion control | Adaptive cwnd, loss-rate compensation, pacing |
| `skyline_policy` | cgroup sockops | Early loss observation, per-flow dynamic RTO floor/ceiling |
| `skyline_tc` | TC egress program | Retransmit DSCP marking and accounting |
| `skyline-speederd` / `ssctl` | Rust userspace | Resident control plane and CLI |

Target environment: **long unidirectional flows over links with ample bandwidth and non-congestive random loss** — 10-20% packet loss, 100-300 ms RTT.

> [!IMPORTANT]
> **Loss is assumed to carry no congestion information.** Skyline Speeder recognises exactly two real congestion signals: growing queueing delay, and ECN marks. If the loss on your path actually comes from queue overflow in network equipment rather than from something link-layer and queue-independent, the loss-compensation mechanism and the queue guardrail push in *opposite* directions and you will make things worse.
>
> Rule of thumb before deploying: if a fixed-rate UDP flood shows zero loss while TCP retransmits heavily, your loss is self-inflicted queue overflow, not random loss. This is not the tool for that.

## Measured performance

Test bed: two KVM VMs, kernel 6.12.101, `htb` + `netem` injecting link conditions, fixed RNG seed, 100 Mbit/s link. Median throughput in Mbit/s.

| Scenario | Kernel CUBIC | Stock BBR | **Skyline Speeder** | vs BBR |
|---|---:|---:|---:|---:|
| `rtt100-loss10` | 0.31 | 84.28 | **95.36** | +13% |
| `rtt100-loss20` | 0.17 | 3.54 | **94.92** | **26.8x** |
| `rtt200-loss15` | 0.12 | 30.89 | **95.04** | **3.1x** |
| `rtt200-loss20` | 0.10 | 21.16 | **95.13** | **4.5x** |
| `rtt300-loss15` | 0.10 | 10.88 | **79.44** | **7.3x** |
| `rtt300-loss20` | 0.05 | 3.48 | **79.92** | **23.0x** |

All 9 grid points pass both the primary and the robustness criterion. At the high-RTT end, with samples raised to `n=5`, Skyline Speeder's own coefficient of variation stays under 4%.

Other results worth knowing:

- **Neutrality holds.** With all four modules off, the difference against stock kernel CUBIC on a zero-loss link is **<0.01%** — the disabled path introduces no systematic bias.
- **The guardrail costs nothing when it should not fire.** `guardrail_gain=0.8` versus a no-slowdown control group differs by <0.01% on the 0% loss scenario.
- **The RTO ceiling has evidence behind it.** A control group without the ceiling (including stock kernel BBR) was measured backing off to roughly **101 seconds**, closing on the kernel's 120 s default. 3/3 runs with the ceiling configured survived.
- **Known weakness:** `reorder-dsack` (pure reordering, outside the target environment) is the one scenario worse than baseline — about 9% below CUBIC and 13% below BBR.

Full methodology, validity boundaries and limitations: [docs/04-performance-report.md](docs/04-performance-report.md) (Chinese).

> [!NOTE]
> These numbers come from one two-VM test bed capped at 100 Mbit/s. They are not a claim about your link. Reproduction steps are in [research/experiments/README.md](research/experiments/README.md).
>
> They were measured with the coefficient set that shipped as the default at the time, which is tuned for random loss and now lives on as the "high random loss" preset in [docs/usage.md](docs/usage.md). Today's defaults were tuned later on a production deployment where loss came from a full bottleneck, and have **not** been run through this test bed. If your link really does lose 10-20% at random, apply that preset to get the behaviour measured here.

## Kernel requirement

**Hard ABI floor is 6.10. The supported and verified floor is 6.12 LTS.**

`skyline_cc.bpf.c` hangs off `tcp_congestion_ops.cong_control` with a **four-argument** callback (`sk, ack, flag, rs`). That signature only exists from v6.10 — on v6.9 and earlier the function pointer takes two arguments (`sk, rs`) and the BPF verifier rejects the program at load time with an explicit arity mismatch.

> **The `6.1.x` and `6.6.x` LTS branches are not supported.** Both are on the old 2-argument signature. That rules out stock Debian 12 and Ubuntu 22.04/24.04 kernels; you need a 6.12+ kernel installed.

Verified: `6.12.101`, `6.18.42`, `7.1.6` pass (including IPv4/IPv6 data-path smoke tests). `6.1.180` and `6.6.148` fail to load, by design.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

> [!NOTE]
> A minimal server image often has neither `curl` nor `sudo`. Install a fetch
> tool first, and drop `sudo` if you are already root:
>
> ```bash
> apt-get update && apt-get install -y curl        # or wget
>
> curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
> wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
> ```

Or clone it and read the script before running it — **recommended**, since this changes the congestion control of every new connection on the machine:

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh
```

The installer: installs the toolchain (clang/llvm/libbpf/bpftool/Rust) → compiles the three CO-RE BPF objects against **this machine's kernel BTF** → builds the Rust control plane → detects and writes the egress interface → pushes every object through the kernel verifier → starts the daemon and enables it at boot. **It installs no proxy and opens no port.**

```bash
sudo ./install.sh --prebuilt   # install published artifacts, no build toolchain
sudo ./install.sh --check      # preflight only, changes nothing
sudo ./install.sh --no-enable  # install without attaching skyline_cc
sudo ./install.sh --uninstall  # remove (keeps /etc/skyline-speeder)
```

### Installing without a build toolchain

`--prebuilt` downloads the published release artifacts instead of compiling, so
the host needs **no clang, no LLVM, no bpftool and no Rust** — only `curl` and
`tar`. The shipped BPF objects were compiled against a pinned reference header
generated from a 6.12 LTS kernel, and CO-RE fixes the field offsets against
*this* kernel when they load.

```bash
sudo ./install.sh --prebuilt                  # latest release
sudo ./install.sh --release v0.1.0            # a specific tag

# From a mirror, an internal artifact store, or a host with no route to
# github.com -- copy the tarball over and point at it:
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz \
  sudo -E ./install.sh --prebuilt
```

The kernel requirement is unchanged: 6.12 LTS or newer, with
`/sys/kernel/btf/vmlinux` present, because that is what CO-RE relocates
against at load time. Each artifact carries a `MANIFEST` recording the commit,
the reference header digest and the digest of every shipped binary and object.

The installer records the congestion control and default qdisc in place before
it changes anything, and `--uninstall` puts them back. Uninstalling a host that
ran BBR returns it to BBR, not to `fallback_cc`.

> [!NOTE]
> The installer builds the BPF objects locally, which is why it needs clang, LLVM and bpftool. That is a property of the current installer, **not** of the objects: they are CO-RE relocatable and do load on kernels other than the one they were compiled against. Measured both ways on Debian 13 — objects built under 6.12.63 load and run on 6.19.14, and objects built against a 6.12 header load on 6.12.63 after being compiled on 6.19.14. `infra/kernel/core-portability.sh` makes that check repeatable.

Day-to-day:

```bash
ssctl status    # runtime state, kernel capabilities, decision counters
ssctl flows     # per-flow view
ssctl drain     # graceful detach, waits for existing flows to finish
```

> [!TIP]
> `ssctl drain` waits for every skyline_cc flow to end — **and your own SSH
> session is one of them**, so over SSH it will always reach its timeout. That
> is safe: drain writes the sysctl back to `fallback_cc` *before* it starts
> waiting, so no new connection uses skyline_cc either way. The struct_ops stays
> attached until the last flow closes, which the kernel reports as a live
> reference, not an error.

## The cgroup prerequisite

**This is the step people miss, and it fails silently.** The dynamic RTO half of `skyline_policy` attaches to `/sys/fs/cgroup/skyline-speeder`. Only connections created by processes inside that cgroup traverse it. A process outside it produces **no error at all** — `rack_rto.stats.applied` in `ssctl status` simply stays at 0 forever.

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <your service command...>
```

`skyline_cc` is a global struct_ops and `skyline_tc` attaches per-interface; neither is subject to this.

## What you can tune

Defaults are the tuned values. **Most deployments should change nothing.**

**1. Four acceleration modules** (all on by default, individually switchable)

| Module | What it does |
|---|---|
| `adaptive-cwnd` | Decides how much may be in flight — this is where the speedup comes from |
| `loss-classifier` | Compensates the bandwidth estimate for loss, instead of backing off |
| `pacing` | Spreads sending evenly instead of dumping a window at once |
| `early-loss` | Earlier loss visibility (observation counter only on current kernels) |

```bash
ssctl enable                                  # everything on (default)
ssctl enable --modules adaptive-cwnd,pacing   # selected modules only
ssctl enable --all-off                        # everything off, as a control baseline
```

> [!IMPORTANT]
> **`ssctl enable` takes effect host-wide.** On a successful attach it writes
> `net.ipv4.tcp_congestion_control = skyline_cc`, so every new TCP connection on
> the machine uses it — not just connections from processes in the cgroup.
> `ssctl drain` is the symmetric reverse: it writes the sysctl back to
> `fallback_cc` first, then waits for existing flows to finish.

**2. Seventeen runtime parameters**, applied online without a restart — send gain, guardrail slowdown, loss-compensation ceiling, queueing-delay threshold, startup aggressiveness, hard rate and window caps. Each flow switches at an RTT boundary via a double-slot + generation counter, so no flow ever reads a torn mix of old and new coefficients.

> [!CAUTION]
> `set-module-config` is **absolute-overwrite**, not incremental. Setting one parameter resets every other parameter to the CLI's built-in default — *not* to the value in your config file. See [docs/usage.md](docs/usage.md) (Chinese).

**3. Dynamic RTO floor and ceiling** (**off** by default). The kernel's RTO backs off to 120 s; one flap can stall a connection for two minutes. Our control group was measured at 101 s. Requires the target process to be in the cgroup above.

**4. Retransmit DSCP marking** (**off** by default, `dscp_value = 0`).

> [!WARNING]
> **`dscp_value = 0` is a placeholder, not a usable value.** DSCP 0 means best-effort — marking with it does nothing. The actual value must be supplied by whoever runs the network (your ISP, data centre, or upstream admin). A wrong value is ignored at best; a value already in use for something else can get your traffic misclassified and **slowed down or policed**.

## Repository layout

```
skyline-speeder/
├── install.sh                    one-click deployment (Debian/Ubuntu)
├── scripts/bootstrap.sh          remote installer entry point
├── DEPLOY.md                     deterministic deployment manual for automation agents
├── CONTRIBUTING.md               invariants a change must not break — read before patching
├── CLAUDE.md                     repository conventions for AI coding assistants
├── Makefile                      make bpf / rust / check / test
├── bpf/
│   ├── skyline_cc.bpf.c          struct_ops congestion control (runs on every ACK)
│   ├── skyline_policy.bpf.c      cgroup sockops: CC selection + dynamic RTO
│   ├── skyline_tc.bpf.c          TC egress: accounting + retransmit DSCP marking
│   └── include/skyline_abi.h     ABI shared between BPF and userspace
├── crates/
│   ├── skyline-speederd/         resident control plane
│   ├── ssctl/                    command line
│   └── skyline-common/           shared ABI types and config parsing
├── config/                       speeder.toml (dev) / speeder-guest.toml (production template)
├── packaging/                    systemd units
├── infra/                        install helpers, cgroup wrapper, two-VM test bed orchestration
├── research/experiments/         performance test manifests, runner and analysis tools
└── docs/                         design, interface reference, deployment guide, performance report
```

## Building from source

```bash
sudo apt-get install -y build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

make bpf                          # generate vmlinux.h, compile three CO-RE objects
cargo build --workspace --release
make check                        # formatting, static checks, unit tests
```

`make bpf` reads kernel type information from this machine's `/sys/kernel/btf/vmlinux`. For cross-building, point it at the target kernel's BTF explicitly: `make VMLINUX_BTF=<path> bpf`.

Run the kernel verifier over everything without leaving runtime state behind — **do this before every commit**:

```bash
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

## How this differs from the alternatives

|  | Stock BBR | [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) | Skyline Speeder |
|---|---|---|---|
| Control structure | Closed loop, probes bandwidth | **Open loop** — you supply the rate | Closed loop, estimates bandwidth |
| Reacts to loss | Yes, indirectly | No | No |
| Needs to know path bandwidth | No | **Yes** | No |
| Congestion self-protection | Yes | None | Queueing-delay + ECN guardrail |
| Form | In-kernel | Kernel module (DKMS) | eBPF CO-RE (verifier-checked) |
| Kernel floor | any | 5.10 | **6.12 LTS** |
| Cross-flow rate sharing | fair-ish | Yes, via `group_id` | **No** |

If you already know your egress bandwidth, it is fixed, and you are on an older kernel, tcp-brutal is the simpler answer. Skyline Speeder is for when the bandwidth is unknown or varies, you want observability and a congestion guardrail, and you can run a 6.12+ kernel. Neither is appropriate for a genuinely congested bottleneck.

### Measured against tcp-brutal on a production path

The table above is design properties. This is what they came to on one real link,
with both algorithms applied server-side only through the same per-destination route
override:

| | Where | Role |
|---|---|---|
| **Server** | KVM VPS in **Singapore** · 10 Gbps | sends; the only side that is accelerated |
| **Client** | **Alibaba Cloud** ECS, **South China (Guangdong)** · 200 Mbps | receives; stock TCP, untouched |

Addresses are omitted. RTT 70–73 ms, 10–30% loss, measured in the evening peak
(22:55–23:37 China Standard Time). The route is asymmetric, as mainland cross-border
routes often are: data from Singapore enters China over China Mobile International
(AS58453) and China Mobile Guangdong (AS9808), while ACKs return over China Telecom
(AS4134) and NTT (AS2914).

What ran: skyline_cc with the coefficients that shipped as the default in 0.1.0 — the
set now kept as the "high random loss" preset in [docs/usage.md](docs/usage.md), the
same one the grid above measured. **Today's defaults have not been run on this path.**
tcp-brutal v2.0.0 at 200 Mbps with its default cwnd gain; bbr as shipped by the kernel.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal and skyline_cc measured against a bbr baseline across game, video, web and bulk traffic" src="docs/images/single-ended-ataglance-light.svg">
</picture>

8 rotations × 3 algorithms × 5 scenarios = 120 runs; 119 completed and one bbr
page-load run timed out. Algorithm order was
rotated every rotation so that a path degrading mid-test cannot favour whoever went
first. Where each one wins, counted per rotation so that drift between rotations
cannot decide it:

| Scenario | Winner | Margin |
|---|---|---|
| **Video** (6 MB chunks @ 48 Mbps) | **tcp-brutal** | startup 7/8 rotations, chunk p95 6/8 |
| **Game** (ping-pong, 60 msg/s) | **skyline_cc** | jitter 5/8, stalls >150 ms 4/8 |
| **Bulk, 4 streams** | tie — both beat bbr | 117 / 115 vs 77 Mbps |
| **Bulk, 1 stream** | bbr and skyline_cc tie; brutal last | 4/8, 4/8, 0/8 rotations; median 59 / 53 / 46 Mbps |
| **Web page load** | no stable winner | rotation-to-rotation spread exceeds the difference |

The result worth internalising is the single-stream one. **On a single stream,
tcp-brutal sent the most and delivered the least: 13.9% of its segments were
retransmissions, against 9.9% for bbr and 11.1% for skyline_cc, for a median of
46 Mbps.** At 10–30% loss the 200 Mbps it was configured with is far above what the
path can actually deliver, and brutal's design — send harder when packets are lost, to
hold the target delivered rate — turns into pure amplification. That is not a bug; it
is the price of its open loop, and its own documentation says so: *"set it too high and
you only produce loss."* On an earlier, shorter run over the same path at 3–10% loss
(n=3), the same 200M configuration had the best single-stream median of the three —
117 Mbps against bbr's 89 and skyline_cc's 95. The configuration did not change; the
link did, and nothing told the operator to re-tune.

> [!NOTE]
> **On RTT this run sits below this project's target envelope, and bbr looks
> correspondingly strong in it.** The grid above is at 100–300 ms RTT, where stock BBR
> falls to 3–31 Mbit/s once loss reaches 15–20%; this path's 70 ms RTT lets BBR recover
> from loss far more easily, and its loss comes from a real cross-border carrier path at
> evening peak rather than the uniform random loss `netem` injects. Read this section as
> evidence about algorithm *character*, not as a second throughput benchmark.

Per-rotation numbers, the full metric set, and the raw records:
[research/experiments/single-ended/](research/experiments/single-ended/) —
and a per-scenario absolute view in
[docs/images/single-ended-detail-light.svg](docs/images/single-ended-detail-light.svg).

## Documentation

The five documents under `docs/` are in Chinese.

| Document | Contents |
|---|---|
| [docs/usage.md](docs/usage.md) | Beginner-facing: what each switch and parameter does, and recipes |
| [docs/01-deployment-guide.md](docs/01-deployment-guide.md) | Build, install, configure, verify, tune, roll back, troubleshoot |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` commands, wire protocol, config fields, event codes |
| [docs/03-design.md](docs/03-design.md) | Architecture, per-module mechanism, control-plane design, coefficient rationale |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Test environment, results, neutrality, mechanism validation, limitations |
| [research/experiments/README.md](research/experiments/README.md) | Reproducing the performance tests |
| [DEPLOY.md](DEPLOY.md) | Deterministic deployment manual for automation agents |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. This project has hard invariants — an ABI struct that must stay byte-identical on both sides, a registration name that must stay under 16 bytes, a systemd `RuntimeDirectory` coupled to a socket path, and three failure modes that report no error at all. The verifier is a hard gate: a change that does not load is not a change.

## License

Copyright (c) 2026 **CYBERVERSE LLC**. Distributed under **GPL-2.0**; see
[LICENSE](LICENSE).

GPL-2.0 is not a preference here. Parts of `skyline_cc.bpf.c` are close
reimplementations of GPL-2.0 Linux kernel algorithms — cited by file and line
in the source — and the BPF objects declare `GPL` to the kernel. The full
reasoning and the upstream attributions are in [NOTICE](NOTICE).

"Skyline Speeder" and "CYBERVERSE" are trademarks of CYBERVERSE LLC; the GPL
grant over the code does not grant any right to use them.

---

<div align="center">

Exclusively sponsored by **Skyline Connect**

</div>
