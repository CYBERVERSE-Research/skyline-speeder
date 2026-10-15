# Single-ended acceleration, three-way field measurement: bbr vs tcp-brutal vs skyline_cc

A single-ended acceleration comparison on a real public-internet path. The full data tables are in [`RESULTS.md`](RESULTS.md);
the figures are `docs/images/single-ended-ataglance-*.svg` (overview) and `single-ended-detail-*.svg` (absolute values per scenario).

> **This is not formal performance data as this repository defines it.** Formal conclusions come only from the controlled two-VM test bed
> specified in `research/experiments/README.md` (`execution_class = "formal-kvm"`), where netem precisely controls each case's
> RTT/loss. This is a complementary, different kind of evidence: a single production link, link conditions that drift on their own, not precisely reproducible.
> **Do not put the numbers here into `docs/04-performance-report.md`.**

## Environment

| | Server (sending, accelerated) | Client (receiving) |
|---|---|---|
| Location | **Singapore** | **South China (Guangdong)** |
| Provider | KVM VPS (QEMU / SeaBIOS, cloud-init NoCloud) | **Alibaba Cloud ECS** (VPC private network) |
| System | Debian 13 / kernel 6.12.63-cloud | Debian 13 / kernel 6.12.85 |
| Bandwidth | 10 Gbps | **200 Mbps (bottleneck)** |

All addresses are withheld. The basis for the locations, all of it checkable, none of it relying on a third-party GeoIP database:

- **The server is in Singapore**: TCP connections from the server to endpoints in each cloud region: Singapore 4.9 ms, while the next closest, Jakarta, is already
  18.9 ms, and Hong Kong 39.1 ms; the address block's registered country in RIPE is SG.
- **The client is Alibaba Cloud South China**: the address block's RDAP registration is Aliyun Computing, country CN; the host name follows Alibaba Cloud ECS's
  `iZ…Z` naming format; the first public hop in traceroute, at 2.6 ms, is already Guangdong Telecom. The three regions Shenzhen, Guangzhou and Heyuan
  are all in Guangdong, and this evidence cannot tell which one it is, so this only says "South China (Guangdong)".

Link: RTT 69.7–73.0 ms (very stable), **loss 10–30%** (highly variable), no artificial impairment injected.
BDP ≈ 1.78 MB ≈ 1225 packets. The tests ran during the evening peak: first pass 19:59–21:29, three-way comparison 22:55–23:37 (Beijing time).

**Forward and return routes are asymmetric:**

| Direction | Carries | Path (by AS) |
|---|---|---|
| Singapore → Guangdong | **Data** (the accelerated direction) | Cogent (AS174) / Lumen (AS3356) → China Mobile International (AS58453) → Guangdong Mobile (AS9808 / AS56040) → Alibaba Cloud |
| Guangdong → Singapore | ACK | Alibaba Cloud → China Telecom 163 (AS4134) → NTT (AS2914) → Singapore |

The two paths were measured at different times: Guangdong → Singapore before the first-pass tests (evening peak); Singapore → Guangdong the next morning,
when the client was already offline (intermediate routers still answer); the route in the data direction at evening peak may differ from this.

Scale: **8 rotations × 3 algorithms × 5 scenarios = 120 runs, 119 completed; bbr's web page load in rotation 8 timed out, and that cell is treated as missing (the valid rotation count for its win-rate statistics is 7).**

## Configuration under test

What was tested is `main` at the start of the session (`e4c3113`, 0.1.0). **#8 later changed skyline_cc's default coefficients;
the new defaults have not been tested on this link**, and the numbers in this directory do not represent the current default behaviour.

**skyline_cc**: the server's `/etc/skyline-speeder/speeder.toml` after installation, used as is; its coefficients are the 0.1.0
defaults, now kept in `docs/usage.md` as the "high random loss preset", and also the set that `docs/04-performance-report.md` uses.

| Parameter | Value | Parameter | Value |
|---|---|---|---|
| `enabled_modules` | all four on | `initial_cwnd_packets` | 100 |
| `startup_gain` | 3.0 | `cruise_inflight_gain` | 2.0 |
| `cruise_pacing_gain` | 1.1 | `guardrail_gain` | 0.8 |
| `startup_plateau_rtts` | 3 | `startup_growth_ratio` | 0.25 |
| `min_rtt_window_s` | 10 | `bw_window_rtts` | 10 |
| `loss_inflation_max_ratio` | 0.5 | `max_queue_delay_ms` / `_ratio` | 100 / 1.0 |
| `max_pacing_mbps` | 1200 | `max_cwnd_packets` | 50000 |

`[rack_rto]` is pushed by `setalgo.sh` with the values from `config/speeder.toml` (`srtt_permille` 1100,
`floor_us` 20000, `ceiling_us` 200000, `warmup_samples` 4), with `rto_max_*` set to 0 (the reason is at the end of this file).
The daemon was started before the first-pass tests and never restarted in between; the following are cumulative values up to the end of all the tests (and one smoke verification afterwards):
`rack_rto.stats` `applied` 6678, `rejected` 0, confirming that M1 really took effect; `guardrail_hits` 0,
i.e. the queueing-delay guardrail never fired once; `prr_adjustments` 0, consistent with PRR being bypassed by design while M2 is on.

**tcp-brutal**: v2.0.0, applied per destination with `brutalctl add <client>/32 200`, cwnd gain at the default 2.0.
200 Mbps is the nominal bandwidth of the client's link, which is exactly the value brutal's documentation asks for.

**bbr**: the version shipped with kernel 6.12.63, untuned.

## Four key methodological decisions

1. **Symmetric switching**. All three algorithms are applied the same way, by a route override on the destination prefix:
   `ip route replace <dst> via <gw> congctl lock <algorithm> proto 233`. This is exactly what `brutalctl`
   itself does, so brutal and the other algorithms go through exactly the same mechanism, with no application changes and no system defaults touched. See `setalgo.sh`.

2. **Rotate the algorithm order each rotation (Latin square)**. On a link that drifts, a fixed order systematically favours one position,
   and that bias would be read as a conclusion. See `(rot - 1 + k) % n` in `run-matrix.sh`.

3. **Probe the link before each rotation starts** (`path-probe.jsonl`), turning drift into data instead of an unmeasured confounder.

4. **Isolate the advantage the cgroup brings**. `skyline_policy.bpf.c` (M1 dynamic RTO) applies per **cgroup**, not per
   congestion algorithm, and every sending process is inside `/sys/fs/cgroup/skyline-speeder`. If it were on throughout, bbr/brutal
   would get the same RTO tuning for free, narrowing the gap. So `setalgo.sh` switches `rack_rto` along with the algorithm.

In addition, because the link drifts, the report gives both **absolute medians** and **within-rotation win rates**: the latter compares rankings only within the same rotation (all three facing the link in the same time window),
and is not affected by drift across rotations.

## Main conclusions

| Scenario | Result |
|---|---|
| Game | **skyline_cc** best on jitter in 5/8 rotations, best on stalls in 4/8; both accelerators have RTT tails far better than bbr |
| Video | **tcp-brutal** leads across the board: wins chunk p95 6/8, startup delay 7/8 |
| Web | Mixed among the three, no stable winner; page load brutal 4/7, TTFB bbr 4/7 |
| Bulk file, single stream | **bbr and skyline_cc 4/8 each**, tcp-brutal **0/8** (median 46 Mbps, lowest) |
| Bulk file, 4 concurrent | **skyline_cc 117 / brutal 115 / bbr 77 Mbps**, both accelerators clearly beat bbr |
| Server retransmission share (all scenarios cumulative) | bbr 9.1% < skyline_cc 10.4% < tcp-brutal 10.9% |

The most noteworthy one: **on the single-stream bulk file, tcp-brutal sent the most (421 thousand segments, of which 51 thousand were retransmissions, 12.2%; bbr 9.0%, skyline_cc 10.0%) yet delivered the least (median 46 Mbps)**.
At 10–30% loss, the 200 Mbps configured for it is already far above the rate the link can actually deliver, and brutal is designed to
"send more when packets are lost, to maintain the target rate", which under high loss becomes pure amplification of waste. tcp-brutal's own documentation says it plainly:
*"set it too high and you only produce loss."* This is not a bug; it is the inevitable cost of leaving the bandwidth judgement to the operator.

> **Correction of the metric.** The retransmission share here was first computed as `TcpRetransSegs ÷ TcpOutSegs`, giving 10.0% / 11.6% / 12.3%
> (all scenarios) and 9.9% / 11.1% / 13.9% (single stream), and the tcp-brutal single stream was written as "370 thousand segments, of which 51 thousand were retransmissions".
> But Linux counts in `TcpOutSegs` only segments that carry new data (or carry no data) (`__tcp_transmit_skb` in
> `net/ipv4/tcp_output.c`); retransmissions go only into `TcpRetransSegs`, so "all segments sent" is the sum of the two: 370 thousand were new data,
> plus 51 thousand retransmissions, 421 thousand sent in total. The numbers above have been recomputed as `TcpRetransSegs ÷ (TcpOutSegs + TcpRetransSegs)`,
> and `RESULTS.md` and the figures were regenerated accordingly; the raw data did not change, and the rankings did not change.

## Files

| File | Contents |
|---|---|
| `RESULTS.md` | **Full data tables**: per-rotation raw values, median + IQR, within-rotation win rates, server-side cost |
| `head-to-head.jsonl` | Main dataset, 120 records, each with client metrics + server `nstat` deltas |
| `path-probe.jsonl` | Link RTT/loss at the start of each rotation |
| `run-matrix.sh` | Orchestration: order rotation, link probing, time-limited runs; refuses to overwrite an existing results directory |
| `server-setup.sh` | Server preparation and teardown (`start` / `stop`): a dedicated nginx instance + the game echo service, both placed in the skyline cgroup |
| `gameserver.py` | Ping-pong echo service for the game scenario |
| `bench.py` | Client-side measurement of the four scenarios; the address is given by `SKYLINE_BENCH_HOST` |
| `setalgo.sh` | Server-side algorithm switching; `DST` is required, and the next hop keeps the kernel's original choice |
| `analyze.py` | Generates `RESULTS.md` |
| `plot_comparison.py` | Computes and generates the two README figures directly from `head-to-head.jsonl`; hard-codes no numbers, so the figures follow when the data changes (pure standard library, no matplotlib needed) |
| `first-pass-with-cubic.jsonl` | First-pass exploratory data: the four scenarios game, web, video and bulk file × cubic / bbr / tcp-brutal (100, 200, 400 Mbps) / skyline_cc, 71 records, n=3, loss 3–10%, no server counters; **weaker method, for reference only** |
| `first-pass-retransmission.jsonl` | Supplementary single-stream bulk-file runs after the first pass: the same six configurations 3 times each, 18 records, with the server's counter deltas (retransmissions); **weaker method, for reference only** |

```bash
# Re-analyze
python3 analyze.py head-to-head.jsonl path-probe.jsonl > RESULTS.md
# Regenerate the figures
python3 plot_comparison.py head-to-head.jsonl ../../../docs/images
```

### Running it again

Prerequisites: the server has skyline-speeder installed (and `ssctl enable` done) and tcp-brutal, plus nginx and python3;
the client needs only python3; both ends allow passwordless SSH login (the orchestration script enables `BatchMode`, and a password prompt would hang the matrix halfway).

```bash
# 1. Server: copy this directory over and start the test services
sudo ./server-setup.sh start

# 2. On any machine that can SSH to both ends:
SERVER=root@<server> CLIENT=root@<client> \
SERVER_ADDR=<address the client uses to reach the server> \
CLIENT_PREFIX=<client address as the server sees it>/32 \
    ./run-matrix.sh results-$(date +%Y%m%d-%H%M)

# 3. Server: tear down right after the tests; the two ports HTTP (8080) and echo (9999) are open to the outside
sudo ./server-setup.sh stop
```

8 rotations take about 45 minutes and produce about 5 GB of traffic. If the client's address is behind NAT, `CLIENT_PREFIX` must be its **public egress** address:
that is what the server sees, and the route override applies only to it.

## Two repository/environment issues found during the runs

- **Dynamic RTO is off by default, and `[rack_rto]` in the config file does not take effect at startup**. Both are by design and
  documented (section 5 of `docs/usage.md`): at startup the daemon zeroes the RTO adjustment on the BPF side, until
  `ssctl set-rack-rto` is run explicitly. Both a source install and a `--prebuilt` install install the same production template, `speeder-guest.toml`,
  which deliberately does not write `[rack_rto]`, because the experiment matrix relies on `reset-rack-rto` meaning "off". So this test used
  `setalgo.sh` to push the values explicitly.
  Also, in 0.5.0 and earlier, `rack_rto.config` in `ssctl status` reported the value in the config file rather than the kernel side's
  actual state: when the config file had `enabled = true`, it showed as on although it was not actually in effect. From 0.5.1 it is the value the kernel actually
  runs (`enabled: false` after the daemon starts, until a `set-`/`reset-` command), the file's value is in the new field
  `declared`, and ATTENTION gives the command to run. The config file in this test had no `[rack_rto]`, so this situation was not
  triggered.

- **`rto_max` is rejected every time on the 6.12 kernel**. `TCP_RTO_MAX_MS` needs Linux 6.15+; the kernel here was 6.12.63:
  `rto_max_rejected: 59 / rto_max_applied: 0`. The counters record this faithfully, so it is not a silent failure, but that half of the feature was turned off for the tests.
