# Usage and tuning guide (for newcomers)

This guide assumes you **know nothing about eBPF or congestion control algorithms** and just want a faster server. You can copy and paste your way through all of it.

You only need to know three terms:

- **Loss**: packets lost on the way. Loss caused by noise on the line is **random loss**; loss caused by sending so fast
  that a device's buffer overflows is **congestion loss**. This project only helps with **random loss**.
- **RTT**: the time a packet takes to get there and back, in milliseconds. From inside China to servers abroad it is typically 100–300ms.
- **Sender-side acceleration**: installed only on **the end that sends the data** (your server). Nothing needs to be installed or changed on the client.

> So: your users' **downloads** get faster, their **uploads** do not. That is by design, not a sign that it is not working.

---

## 1. First, check whether you should use it

In the wrong situation it makes things **slower**. Spend two minutes testing:

```bash
sudo apt-get install -y iperf3
# Run iperf3 -s on the other end, then send fixed-rate UDP from your server
iperf3 -c <peer-IP> -u -b 100M -t 10
iperf3 -c <peer-IP> -u -b 400M -t 10
```

Look at the `Lost/Total Datagrams` column:

| Result | What it means | Recommendation |
|---|---|---|
| **Steady loss at every rate** (5% or more) | Line noise: random loss | **A good fit**; the effect is clear |
| No loss at low rates, loss only once the rate goes up | You are filling the line: congestion loss | **Not a fit**; it may make things worse |
| **No loss at any rate** | A clean line | Little difference |

> [!WARNING]
> **Do not judge by `ping`'s loss rate.** Most networks rate-limit ping, so the loss it shows has nothing to do with
> real data transfer. We measured one line where ping showed 6% loss, while real UDP pushed all the way to 1.1Gbps lost nothing.

---

## 2. Ready to use once installed: what the defaults are

The default state after a one-line install:

| Item | Default | Notes |
|---|---|---|
| The four acceleration modules | **All on** | See the next section |
| Dynamic RTO adjustment | **Off** | Turn it on by hand; see section 5 |
| DSCP marking of retransmitted packets | **Off, and the value is 0** | ⚠️ See section 6; **you must ask your network administrator before turning it on** |
| First-flight redundancy | **On**: the handshake packets and the first 64 KiB of each connection are sent twice, the second copy 10 ms later | On a lossy line, short requests (web pages, APIs) do not sit waiting for loss recovery; the cost is about 64 KiB extra per connection, more when lost packets are retransmitted. To turn it off: `sudo ssctl set-redundancy --disable`; to go back to the config file's value: `sudo ssctl reset-redundancy`; to keep it off across restarts, write `[redundancy]` `enabled = false` in the config file |
| Fallback algorithm after detaching (`fallback_cc`) | The machine's default algorithm at boot (picked automatically at install, usually `bbr`; `cubic` when none can be picked) | New connections use it after `drain` detaches |
| Congestion control and qdisc guard | **On**, checking every 5 seconds | If another script switches things back to bbr / cake, they are switched back automatically; see section 7 |

When the installer finishes it prints a summary: the congestion control and qdisc before and after the change, the
common commands, and the current values of the most commonly used parameters. The output of every command run during the install is in `/var/log/skyline-speeder-install.log`;
when something goes wrong, look there first.

> Alpine uses OpenRC: there, the `sudo systemctl restart skyline-speederd` used below is
> `rc-service skyline-speederd restart`, and `journalctl -u skyline-speederd` becomes
> `grep skyline-speederd /var/log/messages` (Alpine has no sudo by default; run the commands as root).
> The commands the installer prints at the end are already written for the host it ran on.

Check the current state:

```bash
sudo ssctl status
```

---

## 3. The four acceleration modules and what each one does

| Module | In plain words | Recommendation |
|---|---|---|
| `adaptive-cwnd` | **Automatically adjusts how much can be sent at once.** The core module; most of the speedup comes from it | Keep on |
| `loss-classifier` | **Sends a little more, in line with the measured loss rate**, to make up for what is lost on the way (capped; see `--loss-inflation-max-ratio`) | Keep on |
| `pacing` | **Sends data out evenly** instead of pushing it all out at once | Keep on |
| `early-loss` | **Detects loss earlier.** On current kernels it only observes and does not affect decisions | Keep on (leaving it on has no side effects) |

All on (the default):

```bash
sudo ssctl enable --modules early-loss,adaptive-cwnd,loss-classifier,pacing
```

> `sudo ssctl enable` without `--modules` keeps the daemon's current set of modules: right after install that is all on, but after you have run
> `--modules` or `--all-off` below, you need to name all four as above to get back to all on (or restart the daemon).

> [!IMPORTANT]
> Once `ssctl enable` succeeds, **every new TCP connection on this machine** uses `skyline_cc` by default
> (it sets `net.ipv4.tcp_congestion_control` to `skyline_cc`). It does not affect only one process,
> nor only the processes in the cgroup. To go back, use `sudo ssctl drain`.

Turn on only some of them (to find out which module is making the difference):

```bash
sudo ssctl enable --modules adaptive-cwnd,pacing
```

Turn them all off (a "does nothing" baseline for comparison tests):

```bash
sudo ssctl enable --all-off
```

> With everything off it does no acceleration at all: a neutral baseline you can safely use as the control group (for the data that shows this, see
> section 4 of [docs/03-design.md](03-design.md)).

---

## 4. Tuning parameters: read about this critical trap first

> [!CAUTION]
> **`ssctl set-module-config` is an "absolute overwrite", not "change only what you wrote".**
>
> Every parameter has a built-in default on the command line. If you pass only one parameter, **the other 16 are silently reset to
> their built-in defaults**, instead of keeping the custom values in your config file.
>
> ```bash
> # Dangerous: you think you changed only the guardrail, but all 16 other parameters were reset
> sudo ssctl set-module-config --guardrail-gain 0.5
> ```
>
> **The right way**: first check the current values with `ssctl flows` (the PARAMETERS IN FORCE section), then pass **every** parameter you want to keep, all together.
> If you have never changed the config file, the built-in defaults happen to match the shipped values, so passing just one is safe.
>
> The exception: a host upgraded from 0.3.x or earlier that kept its original config file. The installer does not overwrite an existing
> `/etc/skyline-speeder/speeder.toml`, so it still holds the defaults of that time (0.1.0 installed the ④ high random loss preset below,
> and 0.2.0 and 0.3.x installed a `min_cwnd_packets` of 4), which differ from the newer `ssctl`'s built-in defaults. On such a host, passing a single
> parameter replaces all the others with the new defaults as well, while `reset-module-config` goes back to the old values in the config file.

To go back to the settings in the config file (on a freshly installed host, the shipped settings), at any time:

```bash
sudo ssctl reset-module-config
```

### The common parameters, in plain words

**99% of people never need to touch these.** The defaults are already tuned when you install.

| Parameter | What it controls | Raising it | Lowering it | Default |
|---|---|---|---|---:|
| `--cruise-inflight-gain` | **The most it may send beyond the theoretical value in the steady phase** | Holds up better under heavy loss | Cannot get data out under heavy loss | `3.0` |
| `--cruise-pacing-gain` | **The steady-phase sending rate as a multiple of the estimated bandwidth** | More aggressive; may build a queue | More conservative; lower throughput | `1.25` |
| `--guardrail-gain` | **What fraction of the rate to drop to when real congestion is detected** (self-protection) | Cuts less; more aggressive | Cuts harder; safer | `0.8` |
| `--loss-inflation-max-ratio` | **The highest loss rate it compensates for** (it sends `1/(1-p)` as much: 0.10 ≈ 11% more, 0.5 = twice as much) | More compensation on random-loss links; on a congested bottleneck, the more it loses the more it sends | Less compensation; less likely to overload the link by itself | `0.10` |
| `--max-queue-delay-ms` | **How many milliseconds of queueing delay count as congestion** | Slows down later; higher latency | Slows down sooner; lower latency | `70` |
| `--startup-gain` | **How hard a new connection ramps up at the start** | Faster ramp-up; may overshoot | Slower ramp-up, steadier | `3.0` |
| `--initial-cwnd-packets` | **How many packets to send right from the start** (without probing slowly) | Faster small-file transfers | More conservative | `100` |
| `--max-pacing-mbps` | **Hard rate cap**, in Mbps | Allows higher speeds | Limits the speed | `1200` |
| `--max-cwnd-packets` | **Hard cap on unacknowledged data**, in packets | Allows more data in flight | Saves memory but limits throughput | `50000` |
| `--min-cwnd-packets` | **The window's floor**, in packets (takes effect only while `adaptive-cwnd` is on) | A low-traffic connection that loses a packet still has enough packets in its window to trigger fast recovery, instead of sitting and waiting for a timeout; a short response that times out also resends from this window instead of climbing up from 1 packet; the sending rate is still set by pacing, so this does not make it send faster | Back to the historical behaviour (4) | `32` |

**Changes take effect immediately, with no restart**, and each connection switches over smoothly at its next round boundary (about two RTTs), so nothing goes wrong halfway through a transfer.

### Four ready-made presets (copy and use)

Skyline Speeder has no built-in presets the way BBR does, only raw parameters. The four below are equivalent presets we put together;
just **copy and paste** them, with no need to understand each parameter.

**① The conservative preset — low latency first** (games, voice, interactive SSH)

```bash
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 50 --max-queue-delay-ratio 0.5 \
  --initial-cwnd-packets 50 --min-cwnd-packets 32 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 3 --startup-growth-ratio 0.25 --startup-gain 2.0 \
  --cruise-inflight-gain 1.5 --cruise-pacing-gain 1.05 \
  --guardrail-gain 0.7 --loss-inflation-max-ratio 0.10
```

**② The default preset — the shipped settings** (equivalent to `ssctl reset-module-config`; on a host upgraded from 0.3.x or earlier that kept its original config file, that goes back to the old configuration, which for 0.1.0 is ④. To switch to the new default preset, run `sudo ssctl set-module-config` with no parameters at all, which sends exactly the newer built-in defaults; to keep it across restarts, also edit the config file, see "Keeping a preset across restarts")

```bash
sudo ssctl reset-module-config
```

**③ The aggressive preset — throughput first** (large file downloads, video distribution, lines confirmed to have random loss)

```bash
sudo ssctl set-module-config \
  --max-pacing-mbps 2000 --max-cwnd-packets 100000 \
  --max-queue-delay-ms 200 --max-queue-delay-ratio 2.0 \
  --initial-cwnd-packets 200 --min-cwnd-packets 32 \
  --min-rtt-window-s 30 --bw-window-rtts 10 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.15 --startup-gain 4.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.3 \
  --guardrail-gain 1.0 --loss-inflation-max-ratio 0.5
```

What the aggressive preset changes relative to the default preset:

| Parameter | Default | Aggressive | Effect |
|---|---:|---:|---|
| `cruise-pacing-gain` | 1.25 | **1.3** | Sends at 1.3 × the bandwidth estimate in the steady phase |
| `cruise-inflight-gain` | 3.0 | 3.0 | Same as the default preset |
| `startup-gain` | 3.0 | **4.0** | Ramps up harder at the start |
| `startup-growth-ratio` | 0.20 | **0.15** | Bandwidth counts as having peaked only when it grows less than 15% per round, so STARTUP ends later |
| `guardrail-gain` | 0.8 | **1.0** | **No longer slows down** when congestion is detected (1.0 = neutral) |
| `max-queue-delay-ms` / `max-queue-delay-ratio` | 70 / 0.6 | **200 / 2.0** | Queueing delay must be much higher before it counts as "congested" |
| `loss-inflation-max-ratio` | 0.10 | **0.5** | Loss compensation opened up to sending at most twice as much |
| `bw-window-rtts` | 6 | **10** | The bandwidth estimate remembers peaks from further back |
| `initial-cwnd-packets` | 100 | **200** | Sends 200 packets as soon as a connection is established |
| `max-cwnd-packets` / `max-pacing-mbps` | 50000 / 1200 | **100000 / 2000** | Raises both hard caps |

**Measured effect** (one server → client, a clean link with RTT ≈ 18ms, a 50MB download, 5 interleaved rounds).
These numbers were measured **before** the default coefficients changed: the "default preset" in the table is the default of that time, that is, the
④ high random loss preset below; the current default preset has not been measured again on this link.

| | Default preset | Aggressive preset |
|---|---:|---:|
| Median throughput | 85.6 MB/s | **143.6 MB/s** |
| Per-round paired gain | Baseline | **+54% (median)** |
| Rounds won | — | **5 / 5** |
| Idle latency | 18.4 ms | 18.4 ms |
| Latency under load | 18.0 ms | 18.1 ms |

On this link the aggressive preset **showed no measurable latency cost**, because bandwidth was plentiful and no queue built up.
That does not mean the same holds on your link: where bandwidth is tight or there is a bottleneck buffer, `guardrail-gain=1.0`
makes it push all the way until packets are lost. **After switching presets, measure the latency under load yourself before deciding whether to keep it.**

> [!WARNING]
> The aggressive preset sets `guardrail-gain` to 1.0, which amounts to **turning off the self-protective slowdown**. On a truly congested link
> it keeps filling buffers and pushing latency up, and it also squeezes other traffic on the same link.
> **Use it only when "the line has plenty of bandwidth".**

**④ The high random loss preset — the former defaults** (for links that really do have 10%-20% **random** loss, not loss from being overloaded)

```bash
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 100 --max-queue-delay-ratio 1.0 \
  --initial-cwnd-packets 100 --min-cwnd-packets 4 \
  --min-rtt-window-s 10 --bw-window-rtts 10 \
  --startup-plateau-rtts 3 --startup-growth-ratio 0.25 --startup-gain 3.0 \
  --cruise-inflight-gain 2.0 --cruise-pacing-gain 1.1 \
  --guardrail-gain 0.8 --loss-inflation-max-ratio 0.5
```

These are the coefficients measured as `skyline-best` in the [performance validation report](04-performance-report.md), aimed at links where
"loss does not mean congestion": loss compensation goes up to sending at most twice as much, and the guardrail is set very loose. The default preset later moved away from them
because, in a production environment with many concurrent connections where loss came mostly from a full bottleneck, these coefficients sent more the more they lost —
retransmissions rose markedly and bought no speed. **How to tell**: after switching presets, look at the retransmission share and the number of RTO timeouts (for how, see
"Appendix: how to investigate video stalls / interrupted large-file transfers" at the end); if retransmissions went up while throughput did not, your loss is
self-inflicted by overloading the path: go back to the default preset.

### Keeping a preset across restarts

`ssctl set-module-config` changes the **in-memory state, which is lost on restart**. To make a change permanent, edit the config file:

```bash
sudo cp /etc/skyline-speeder/speeder.toml /etc/skyline-speeder/speeder.toml.bak
sudo nano /etc/skyline-speeder/speeder.toml
```

How command-line parameters map to the config file (replace `-` with `_` in the parameter name): `--max-pacing-mbps`, `--max-cwnd-packets`,
`--max-queue-delay-ms`, `--max-queue-delay-ratio`, `--initial-cwnd-packets` and `--min-cwnd-packets` are top-level fields;
`--loss-inflation-max-ratio` is in the `[loss_classifier]` section; the rest (the gains, `--min-rtt-window-s`,
`--bw-window-rtts`, `--startup-plateau-rtts`, `--startup-growth-ratio`) are in the `[adaptive_cwnd]` section.

> [!CAUTION]
> From 0.5.0, a key in the wrong section, or a misspelt key, makes the `--validate-only` below fail outright, naming the section it belongs in; 0.4.x and earlier
> **silently ignore** such keys, and `--validate-only` still passes. Whatever the version, after the restart check the values against the
> PARAMETERS IN FORCE section of `sudo ssctl flows`.

When you are done editing:

```bash
# Validate first; if it fails, do not restart
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only

sudo systemctl restart skyline-speederd.service    # the attach unit restarts along with it and attaches again
```

### Parameters with hard limits (go past one and the whole command is rejected)

| Parameter | Allowed range | Notes |
|---|---|---|
| `--bw-window-rtts` | **1 – 10** | Default 6; 10 is a hard cap |
| `--loss-inflation-max-ratio` | **0 – 0.5** | Default 0.10; 0.5 is the cap hard-coded on the BPF side (500‰) |
| `--guardrail-gain` | 0 – 1.0 | 1.0 = only cancels the boost, never slows down; 0 has the same effect as 1.0 (it does not turn the guardrail off) |
| `--startup-growth-ratio` | 0 – 1.0 | |
| `--startup-gain` / `--cruise-*-gain` | **1.0 – 1000** | The larger it is, the more easily a queue builds. The cap was added in 0.5.2: beyond it, the kernel side's 32-bit multiplication of the gain by the loss compensation wraps around to a very small value |
| `--max-cwnd-packets` | ≥ 4 | |
| `--min-cwnd-packets` | **4 – `--max-cwnd-packets`** | Below 4 is rejected (the BPF side never lets the window go below 4 anyway) |
| `--initial-cwnd-packets` | **0 – `--max-cwnd-packets`** | 0 = use the kernel's own initial window (from 0.5.2 it cannot exceed the cap) |
| `--min-rtt-window-s` | **1 – 4294** | 0 would make the base RTT always equal the current RTT, so the guardrail never sees any queueing (rejected from 0.5.2) |

Every fractional parameter must be an ordinary number: values such as `nan` and `inf` are rejected by both `ssctl` and the config file (from 0.5.2).

> The command is an **absolute overwrite**: if **any single** parameter is out of range, **the whole command is rejected**,
> and the configuration in force stays as it was. The reply states why it was rejected (usually naming the parameter and its allowed range).

### The right way to change parameters

```bash
# 1. First see what is in force now (the PARAMETERS IN FORCE section)
sudo ssctl flows

# 2. Pass every value you want to keep, all together (this example only wants to loosen the guardrail to 0.9)
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 70 --max-queue-delay-ratio 0.6 \
  --initial-cwnd-packets 100 --min-cwnd-packets 32 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.20 --startup-gain 3.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.25 \
  --guardrail-gain 0.9 \
  --loss-inflation-max-ratio 0.10

# 3. Not happy with it? Restore with one command
sudo ssctl reset-module-config
```

> Parameters changed online **are lost on restart**. To make them permanent, edit the matching fields in `/etc/skyline-speeder/speeder.toml`,
> then `sudo systemctl restart skyline-speederd`.

---

## 5. Dynamic RTO adjustment (off by default)

**RTO** = how long to wait without a response before resending. By default the kernel can back off as far as **120 seconds** — one hiccup on the line
and a connection can be stuck for two minutes. This feature puts a ceiling on it.

We measured it: in the control group without a ceiling, the RTO was pushed as high as **101 seconds**.

> [!CAUTION]
> **The RTO "ceiling" needs kernel ≥ 6.15; it cannot take effect on 6.12.**
>
> This feature sends its value with `TCP_RTO_MAX_MS` (socket option number **44**). That number is
> **not yet assigned** in Linux 6.12 (that kernel only goes up to 43 = `TCP_IS_MPTCP`), so the kernel returns `ENOPROTOOPT`.
>
> What you see: the command is sent successfully and the configuration shows as in force, but in `ssctl flows --json`
> **`rack_rto.stats.rto_max_rejected` keeps growing while `rto_max_applied` stays at 0**.
> We confirmed this by measurement on Debian 6.12.101.
>
> **The RTO "floor" (`floor_us` / `srtt_permille` / `ceiling_us`) is not affected and works normally on 6.12**
> — it uses a different option, `TCP_BPF_RTO_MIN` (1004), which that kernel supports.
>
> Check whether your kernel supports it:
> ```bash
> grep -c "TCP_RTO_MAX_MS" /usr/src/linux-headers-$(uname -r)*/include/uapi/linux/tcp.h
> # Output 0 = ceiling adjustment not supported; non-zero = supported
> ```
> Leaving the configuration in place has no side effects (the kernel just ignores it), and it starts taking effect by itself once you upgrade to 6.15+.

> [!CAUTION]
> **Writing `[rack_rto] enabled = true` in the config file alone is not enough.** Every time the daemon starts it clears this feature,
> and it does not push this section of the config file into the BPF map. To actually turn it on, run `sudo ssctl reset-rack-rto` once (sends the values in
> the config file), or `sudo ssctl set-rack-rto ...` (sends the values given on the command line; see below).
> Measured: with only the config file, `established_cb` was 2169, but `subscribe_ok` / `rtt_callbacks`
> / `applied` were all 0; they started growing only after the command was run.
>
> So **run it again after every daemon restart**, or add it to a boot script. From 0.5.1, when the config file turns it on but it is not actually on,
> the ATTENTION section of `sudo ssctl status` tells you to run `reset-rack-rto`, and this line in `ssctl flows` honestly shows `off`;
> in 0.5.0 and earlier, both reports followed the file and showed it as on.

To turn it on:

```bash
sudo ssctl set-rack-rto \
  --srtt-permille 1100 \
  --floor-us 20000 \
  --ceiling-us 200000 \
  --warmup-samples 4
```

| Parameter | In plain words | Suggested value |
|---|---|---|
| `--srtt-permille` | Target RTO = measured latency × this number ÷ 1000. 1100 = 1.1 × | `1100` |
| `--floor-us` | The **minimum** wait (microseconds). Too small and it mistakes delay for loss and retransmits spuriously | `20000` (20ms) |
| `--ceiling-us` | The upper bound of the RTO **floor** (microseconds). **Not the backoff ceiling**, and the kernel hard-limits it to 200000 | `200000` (200ms) |
| `--warmup-samples` | For the first few samples it only observes and does not intervene, acting only once the latency is measured accurately | `4` |

**The real "backoff ceiling" is two other parameters** (the key to keeping connections from getting stuck; needs kernel ≥ 6.15):

```
backoff ceiling = clamp(permille × RTT ÷ 1000,  1000ms,  120000ms)
                                                 ↑ clamped from below, so 1 second is the smallest value you can set
```

RTT is the larger of the connection's smoothed RTT and its minimum RTT; it is worked out again at every RTT sample.

| Parameter | In plain words | Notes |
|---|---|---|
| `--rto-max-normal-permille` | The backoff ceiling multiplier in normal times | `0` = this feature off, keeping the kernel's 120-second default |
| `--rto-max-congested-permille` | The multiplier used instead for samples that show congestion (judged afresh at every sample; once the signs of congestion are gone it switches back to normal) | Must be ≥ normal |
| `--rto-max-congestion-ratio-permille` | How many times higher the latency must rise to count as "real congestion" | `0` = use the built-in default of 2000 (2 ×) |

Take a connection with an RTT of 120ms as an example:

```bash
sudo ssctl set-rack-rto \
  --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 --warmup-samples 4 \
  --rto-max-normal-permille 3000 \
  --rto-max-congested-permille 6000
```

The arithmetic: normally `3000` → 3.0 × 120ms = 360ms, below the 1000ms lower bound → clamped to **1 second**.
Congestion has two criteria, and they work out differently: when the smoothed RTT is over twice the minimum RTT, the multiplier applies to the RTT that has grown by then,
say 250ms → 6.0 × 250ms = **1.5 seconds**; when the RTT has not risen and only a new ECN mark has arrived, 6.0 × 120ms = 720ms → clamped to **1 second**.

> Note that the formula is tied to **each connection's own RTT**, not to an absolute value. On a connection with a larger RTT, the same multiplier
> works out to a higher ceiling; when congestion is judged by RTT, the RTT has already doubled, so the ceiling more than doubles with it, and the congested multiplier need not
> be set much larger than the normal one. If you want "1 second whatever the RTT", just set normal small (clamped at the lower bound, that is 1 second).

To turn it off:

```bash
sudo ssctl set-rack-rto --disable
```

(`sudo ssctl reset-rack-rto` goes back to the `[rack_rto]` settings in the config file: the shipped configuration has no such section, so that means off;
where the config file says `enabled = true`, it turns the feature on instead.)

> [!IMPORTANT]
> **This feature has a precondition: the process must be in the designated cgroup; otherwise it has no effect at all, and no error is reported.**
>
> To check: in the DYNAMIC RTO section of `sudo ssctl flows` (called CGROUP POLICY while dynamic RTO is off; this line is always there),
> if *connections seen* stays at 0, it is not taking effect (the text below it says outright that this is not an error, but that no process is in that cgroup).
>
> To put your service in the cgroup:
> ```bash
> sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <your service's start command>
> ```
> For a systemd service, see [DEPLOY.md section 7](../DEPLOY.md).
>
> The main acceleration (`skyline_cc`) is **not subject to this restriction**; it always applies globally.

---

## 6. ⚠️ DSCP marking of retransmitted packets: off by default, and the default value is 0

**This is the easiest feature to misconfigure. Read all of this before you do anything.**

What it does: it puts a DSCP mark (the priority field in the IP header) on **retransmitted packets**,
so that upstream routers / carrier equipment can recognise "this is a retransmission" and route it separately or give it priority.

### Why the default is 0

```toml
[retransmit_dscp]
enabled = false
dscp_value = 0      # <- a placeholder, not a usable value
```

`dscp_value = 0` is a **placeholder** meaning "not configured". In the standard, DSCP 0 is "best effort" (the default class),
so **marking with it is the same as not marking at all**.

> [!CAUTION]
> **The actual DSCP value must be specified by whoever runs your network (the carrier / data centre / upstream network administrator).**
>
> This is not a value you make up yourself — it must match the policy of the upstream equipment; otherwise:
> - Fill in the wrong value: upstream does not recognise it and the mark is ignored (wasted effort);
> - Fill in a value someone else is using: your traffic may be misclassified into another policy and **may get slower or even be rate-limited**.
>
> **Before turning it on, contact whoever runs your network and ask exactly "which DSCP value should retransmitted traffic be marked with".**

### How to turn it on once you have the value

```bash
# Replace <value> with the number your network operator gave you (0-63)
sudo ssctl set-retransmit-dscp --dscp-value <value>
```

> Note: this command **also turns the feature on** (`enabled` is set to true automatically);
> there is no separate command to run to turn it on.
>
> Writing it into the config file (`enabled = true` and `dscp_value = <value>` in the `[retransmit_dscp]` section) likewise **does not take effect when the daemon
> starts**: after every start, run `sudo ssctl reset-retransmit-dscp` once to send the values in the file. From 0.5.1,
> the ATTENTION section of `sudo ssctl status` reminds you of this.

To turn it off:

```bash
sudo ssctl set-retransmit-dscp --disable    # off, whatever the config file says (from 0.5.2)
```

(`sudo ssctl reset-retransmit-dscp` goes back to the settings in the config file: the shipped configuration is off; where the file turns it on, it turns it on instead.
0.5.1 and earlier have no `--disable`, so there you can only use this, or restart the daemon.)

### Further notes

- It is attached to the NIC's egress and applies equally to **all TCP traffic** on that NIC, whichever congestion control algorithm it uses.
- It is attached only to Ethernet NICs. When `runtime.tc_interface` in the configuration is a tunnel such as WireGuard / WARP or tun,
  it is not attached, and the command above to turn it on fails (the reason is given in the ATTENTION section of `ssctl status`):
  change `tc_interface` to the physical NIC that carries the tunnel's traffic, then restart `skyline-speederd`.
- It is **not affected** by `ssctl drain`. When the acceleration is detached, this marking keeps working; turn it off separately.
- Both IPv4 and IPv6 are supported; in our measurements nothing was marked by mistake.

---

## 7. Confirm that it really is working

```bash
sudo ssctl status
```

The top right of the title line gives the verdict directly; that one spot is all you need to look at:

```
● ACCELERATING            <- attached and the host-wide default; new connections all use it
○ ATTACHED, NOT DEFAULT   <- attached, but new connections bypass it, so it is as good as not working (see below)
○ DRAINING                <- drain was run: new connections are back on fallback_cc, skyline_cc only serves the remaining connections
                             (a drain run over SSH always stops here; to resume acceleration, run sudo ssctl enable)
○ STANDBY                 <- not attached; run sudo ssctl enable
```

For `ATTACHED, NOT DEFAULT` and `STANDBY`, the **ATTENTION** section at the bottom of the report lists each problem and the action
to take for it; `DRAINING` is the result of a drain you ran yourself and is not a problem. The four key lines each carry a mark too:

```
✔ skyline_cc struct_ops   attached, 3h 12m ago     <- acceleration attached
✔ host default cc         skyline_cc               <- new connections really do use it
✔ state ARMED (DRIFT GUARD section)                <- congestion control and qdisc are being guarded
✔ KERNEL section: all ✔ on the required line       <- the environment meets the requirements
```

Then confirm that real connections really are using it (run this while the server has traffic):

```bash
sudo ssctl flows
```

The coverage line in the CONNECTIONS section says "how many of the TCP connections run on skyline_cc",
and the connections are listed one by one below it. If it is 0, it may be that old connections already established do not switch partway through; just wait for new connections.

> A grey dot (not found) for `RACK reorder hook` on the optional line of the KERNEL section is **normal** and does not affect anything.
> For the raw JSON (for scripts to parse, or to paste into a report), add `--json`.

### Machines that previously had "one-click BBR" installed or their qdisc changed

You do not need to uninstall those first, or change anything back by hand. `ssctl enable` (which the installer runs automatically):

- switches the congestion control to `skyline_cc`;
- sets the default qdisc (`net.core.default_qdisc`) to `fq`;
- replaces the qdisc on the egress NIC (`cake`, `fq_pie`, `fq_codel` and so on) with `fq` — `skyline_cc` relies on `fq`
  to send data out evenly. When the egress is a VLAN, bond or bridge, it replaces the qdisc of the physical NICs under it (the ports of virtual machines
  and containers on the bridge are left alone).

After that, `skyline-speederd` checks every 5 seconds and changes back whatever anything else changes (for instance, a script running
`sysctl --system` again), and records it in the log:

```bash
journalctl -u skyline-speederd | grep 'guard:'
```

If lines with `something else on this host changed it` keep appearing, something on the machine keeps changing them.
That does not affect acceleration; to stop it for good, find the line that says `bbr` or `cake` in `/etc/sysctl.conf` or `/etc/sysctl.d/`
and delete it (the summary at the end of the install lists these files).

It **leaves things alone** in these cases:

- The NIC has a rate-limiting / shaping qdisc you set up yourself (`htb`, `tbf`, `netem` and so on, and a `cake` with a bandwidth set,
  such as `cake bandwidth 90Mbit`) — replacing it would wipe out your rate-limiting setup. The DRIFT GUARD
  section of `ssctl status` says in a `note` that it left it alone;
- The egress is a device such as in-kernel WireGuard whose root is `noqueue` and under which no physical NIC can be found — `notes` will say
  `is a virtual device ... no qdisc checked`. If you want it managed, change `runtime.tc_interface` in the configuration
  to the real physical NIC (tun-type tunnels — the WARP client, wireguard-go, OpenVPN — have their own qdisc,
  and when set as `tc_interface` it is replaced with `fq` all the same);
- After `ssctl drain` has run — detaching stops the guard, and the qdisc stays as it is (still `fq`).

If you do not want it to manage qdiscs: set `qdisc` in the `[guard]` section of `/etc/skyline-speeder/speeder.toml` to
`false`, then:

```bash
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only
sudo systemctl restart skyline-speederd.service
```

---

## 8. Recipes for common scenarios

### Scenario 1: an ordinary cross-border server, line conditions unknown

**Change nothing.** It is already tuned once installed.

### Scenario 2: you want to verify whether it actually helps

```bash
# Baseline: all modules off (a neutral baseline with no acceleration at all)
sudo ssctl enable --all-off
# ... measure speed ...

# All on (name all four modules: enable without --modules keeps the all-off set from before)
sudo ssctl enable --modules early-loss,adaptive-cwnd,loss-classifier,pacing
# ... measure again under the same conditions ...
```

> The line itself fluctuates a lot, so **a single test cannot be trusted**. Alternate at least 3 rounds in the same time window and take the median.

### Scenario 3: latency-sensitive services (games, voice), you want it more conservative

```bash
# Slow down sooner, cut harder
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 50 --max-queue-delay-ratio 0.5 \
  --initial-cwnd-packets 100 --min-cwnd-packets 32 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.20 --startup-gain 3.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.05 \
  --guardrail-gain 0.7 \
  --loss-inflation-max-ratio 0.10
```

### Scenario 4: connections often freeze for tens of seconds for no apparent reason

Turn on the dynamic RTO adjustment from section 5, and make sure the process is in the cgroup.

---

## Appendix: how to investigate video stalls / interrupted large-file transfers

Typical symptoms: **4K freezes while 2K plays normally**, a large download stops moving halfway, SSH freezes for tens of seconds and then recovers.

### Step 1: check whether any connection is stuck in RTO backoff

```bash
# Any output means some connection is in exponential backoff; the bigger the number, the deeper the backoff
ss -tin | grep -oE 'backoff:[0-9]+' | sort | uniq -c

# RTO distribution; normally it should cluster at a few hundred milliseconds
ss -tin | grep -oE 'rto:[0-9]+' | sed 's/rto://' | sort -n | tail -5
```

As soon as you see `backoff:10` or more and an `rto` in the tens of thousands of milliseconds, that is the cause. We have actually seen
a connection with **an RTT of only 101ms but rto=120000ms (120 seconds)** — that connection was in effect already dead.

> **Why 4K breaks first while 2K is fine**: protocols such as AnyTLS / VLESS multiplex sessions, running many streams over the same
> TCP connection. Once that TCP connection stalls, every stream on it stalls together. 4K has a high bitrate and the player has little buffer headroom, so it
> shows the problem first; 2K has a low bitrate and can ride out a short pause on its buffer.

### Step 2: check whether the parameters are set too aggressively

```bash
# Retransmission share and number of RTO timeouts over 40 seconds
python3 - <<'EOF'
import time
def snmp():
    d={}
    for f in ("/proc/net/snmp","/proc/net/netstat"):
        L=open(f).read().splitlines()
        for i in range(0,len(L)-1,2):
            h,v=L[i].split(),L[i+1].split()
            if h[0]==v[0]: d.update({h[0]+k:int(x) for k,x in zip(h[1:],v[1:])})
    return d
a=snmp(); time.sleep(40); b=snmp()
o=b["Tcp:OutSegs"]-a["Tcp:OutSegs"]; r=b["Tcp:RetransSegs"]-a["Tcp:RetransSegs"]
# OutSegs does not include retransmitted segments, so the retransmission share is r/(o+r)
print(f"retransmission share {r*100/(o+r):.2f}%  RTO timeouts {b['TcpExt:TCPTimeouts']-a['TcpExt:TCPTimeouts']}")
EOF
```

We measured three presets on the same machine (40 seconds of real production traffic per preset, medians). These numbers were measured **before** the default coefficients
changed: the "default preset" is the default of that time, that is, the ④ high random loss preset above; the "conservative preset" is also the
version from before the change. The current default and conservative presets have not been measured again this way, and the "2.7 times" conclusion below applies only to the comparison of the aggressive preset with ④.

| Preset | Retransmission share | **RTO timeouts / 40 s** |
|---|---:|---:|
| Aggressive preset | 9.2% | **93** |
| Default preset | 8.1% | **35** |
| Conservative preset | 5.7% | **25** |

(The retransmission shares were converted from the 10.1% / 8.8% / 6.0% recorded at the time as `retransmitted ÷ newly sent` into `retransmitted ÷ everything sent`; the conversion does not change the order.)

**The aggressive preset had 2.7 times the RTO timeouts of the default preset**, the highest in all three rounds of testing. RTO timeouts are the direct source of stalls.

**What to do: go back to the default preset first** (on a host upgraded from 0.3.x or earlier that kept its original config file, `reset-module-config` goes back to that old configuration;
to get the new default preset, use `sudo ssctl set-module-config` with no parameters at all instead; see ②)

```bash
sudo ssctl reset-module-config
```

If you still want more speed, **leave `guardrail-gain` alone** (that is the self-protection switch),
and only raise `cruise-pacing-gain` in small steps (1.25 → 1.3 → 1.35), running the script above at each step to watch the number of RTO timeouts;
if it rises noticeably, step back.

### Step 3: limit how long a dead connection hangs

The kernel default is `tcp_retries2 = 15`: once a connection is in backoff it can hang for up to **about 15 minutes** before it is given up.

In principle you would use `rto_max_*_permille` to push the backoff ceiling down to 1–2 seconds,
but **that needs kernel ≥ 6.15** (see section 5). The alternative on 6.12 is to shorten the give-up threshold:

```bash
sudo tee /etc/sysctl.d/99-zz-skyline-speeder-retries.conf >/dev/null <<'EOF'
net.ipv4.tcp_retries2 = 8
EOF
sudo sysctl -p /etc/sysctl.d/99-zz-skyline-speeder-retries.conf
```

This makes dead connections **fail fast so the client reconnects**, instead of hanging for a long time.
The smaller the value, the faster they fail (8 ≈ on the order of 100 seconds, 6 ≈ on the order of 25 seconds); too small, and it kills connections that could have recovered
from a brief network hiccup; with many mobile users, going below 6 is not recommended.

## 9. How to back out when something goes wrong

```bash
# 1. Parameters in a mess -> restore the parameters from the config file (on a host upgraded from 0.3.x or earlier that kept its original config, these are the defaults of that time)
sudo ssctl reset-module-config

# 2. Stop accelerating for now (existing connections finish on their own; nothing is cut off)
sudo ssctl drain --timeout 60

# 3. Stop it completely
sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service

# 4. Uninstall completely: switch back to bbr + fq and remove the build toolchain the install added (the config files are kept)
sudo ./install.sh --uninstall

# 4b. Go back to the congestion control and qdisc the machine used before the install, instead of bbr + fq
sudo ./install.sh --uninstall --restore-pre-install
```

What the uninstall does: before detaching, it drains the existing connections, removes the two systemd units, the binaries and the BPF objects, switches the machine to
**bbr + fq**, then removes the packages the install added (`clang`, `llvm`, `bpftool`, rustup and so on). It removes only the ones recorded at install time in
`/etc/skyline-speeder/added-packages`, **never** touches `iproute2`, `curl`,
`ca-certificates` or `tar`, never touches the libraries those packages still depend on, and never touches packages the system marks as essential; if apt's
plan shows that something outside the list would be removed along with them, it keeps the one package that causes this (and warns, saying what needs it),
and removes the rest; only when it really cannot put together a plan that stays inside the list does it remove nothing at all, printing the commands for you to decide on instead. `/etc/skyline-speeder` is kept, so the parameters you tuned
are still there when you reinstall.

> [!NOTE]
> `bbr` and `fq` are set **for the current boot**. This project never changes the files under `/etc/sysctl.d`, so after a reboot
> those files decide what is used. To keep bbr + fq in effect permanently, just write a file yourself:
> ```bash
> printf 'net.ipv4.tcp_congestion_control = bbr\nnet.core.default_qdisc = fq\n' \
>   | sudo tee /etc/sysctl.d/99-zz-skyline-bbr.conf
> sudo sysctl --system
> ```
>
> The name has to sort last: `/etc/sysctl.d` is applied in file-name order, and for the same key the one written later wins. Afterwards, use
> `sudo ./install.sh --check` to check which file actually sets these two keys at boot.

> [!NOTE]
> After uninstalling, `bpftool struct_ops show` may still show `skyline_cc` (called
> `skyline_cc_txs` on kernels 7.1 and later). **This is not a failure** —
> as long as any connection still uses it, the kernel does not release it. It disappears by itself once those connections close, or after a reboot.

**DSCP marking and RTO adjustment are not affected by `drain`**; turn them off separately:

```bash
sudo ssctl set-retransmit-dscp --disable
sudo ssctl set-rack-rto --disable
```

---

## 10. Going deeper

- [docs/02-interface-reference.md](02-interface-reference.md) — the exact definition of every command and every field
- [docs/03-design.md](03-design.md) — how the algorithm works and why it is designed this way
- [docs/04-performance-report.md](04-performance-report.md) — the full test data and its limitations
- [DEPLOY.md](../DEPLOY.md) — the deployment and operations manual: installing, verifying, upgrading, troubleshooting, uninstalling
