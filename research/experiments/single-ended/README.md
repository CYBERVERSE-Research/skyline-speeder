# 单边加速三方实测：bbr vs tcp-brutal vs skyline_cc

真实公网路径上的单边加速对比。完整数据表见 [`RESULTS.md`](RESULTS.md)，
图见 `docs/images/single-ended-ataglance-*.svg`（概览）和 `single-ended-detail-*.svg`（逐场景绝对值）。

> **这不是本仓库定义的正式性能数据。** 正式结论只来自 `research/experiments/README.md`
> 规定的受控双 VM 测试床（`execution_class = "formal-kvm"`），由 netem 精确控制每个 case 的
> RTT/丢包。这里是互补的另一类证据：单条生产链路、链路条件自行漂移、无法精确复现。
> **不要把这里的数字写进 `docs/04-performance-report.md`。**

## 环境

| | 服务端（发送，被加速） | 客户端（接收） |
|---|---|---|
| 位置 | **新加坡** | **中国华南（广东）** |
| 提供方 | KVM VPS（QEMU / SeaBIOS，cloud-init NoCloud） | **阿里云 ECS**（VPC 内网） |
| 系统 | Debian 13 / kernel 6.12.63-cloud | Debian 13 / kernel 6.12.85 |
| 带宽 | 10 Gbps | **200 Mbps（瓶颈）** |

地址均已隐去。定位依据，全部可复查，均不依赖第三方 GeoIP 库：

- **服务端在新加坡**：从服务端向各云区域端点建 TCP 连接，新加坡 4.9 ms，次近的雅加达已是
  18.9 ms、香港 39.1 ms；地址段在 RIPE 登记国家为 SG。
- **客户端是阿里云华南**：地址段 RDAP 登记为 Aliyun Computing，国家 CN；主机名为阿里云 ECS
  的 `iZ…Z` 命名格式；traceroute 第一个公网跳在 2.6 ms 处即为广东电信。深圳、广州、河源三个
  地域都在广东，这个证据分不出具体是哪一个，因此只写到"华南（广东）"。

链路：RTT 69.7–73.0 ms（很稳），**丢包 10–30%**（波动大），未注入任何人工损伤。
BDP ≈ 1.78 MB ≈ 1225 包。测试时段为晚高峰：首轮 19:59–21:29，三方对比 22:55–23:37（北京时间）。

**来回路由不对称：**

| 方向 | 承载内容 | 路径（按 AS） |
|---|---|---|
| 新加坡 → 广东 | **数据**（被加速的方向） | Cogent（AS174）/ Lumen（AS3356）→ 中国移动国际（AS58453）→ 广东移动（AS9808 / AS56040）→ 阿里云 |
| 广东 → 新加坡 | ACK | 阿里云 → 中国电信 163（AS4134）→ NTT（AS2914）→ 新加坡 |

两条路径的测量时间不同：广东 → 新加坡测于首轮测试前（晚高峰）；新加坡 → 广东测于次日上午，
当时客户端已下线（中间路由器仍会应答），晚高峰时的数据方向路由可能与此不同。

规模：**8 轮轮转 × 3 算法 × 5 场景 = 120 次运行，119 次完成；第 8 轮 bbr 的网页加载超时，该格按缺失处理（其胜率统计的有效轮次为 7）。**

## 被测配置

测的是会话开始时的 `main`（`e4c3113`，0.1.0）。**此后 #8 更换了 skyline_cc 的默认系数，
新默认值没有在这条链路上测过**，本目录的数字不代表当前默认行为。

**skyline_cc**：服务端安装后的 `/etc/skyline-speeder/speeder.toml` 原样使用，其系数即 0.1.0 的
默认值——现在以「高随机丢包档」保留在 `docs/usage.md`，也是 `docs/04-performance-report.md` 所用的那组。

| 参数 | 值 | 参数 | 值 |
|---|---|---|---|
| `enabled_modules` | 四个全开 | `initial_cwnd_packets` | 100 |
| `startup_gain` | 3.0 | `cruise_inflight_gain` | 2.0 |
| `cruise_pacing_gain` | 1.1 | `guardrail_gain` | 0.8 |
| `startup_plateau_rtts` | 3 | `startup_growth_ratio` | 0.25 |
| `min_rtt_window_s` | 10 | `bw_window_rtts` | 10 |
| `loss_inflation_max_ratio` | 0.5 | `max_queue_delay_ms` / `_ratio` | 100 / 1.0 |
| `max_pacing_mbps` | 1200 | `max_cwnd_packets` | 50000 |

`[rack_rto]` 通过 `setalgo.sh` 下发 `config/speeder.toml` 的取值（`srtt_permille` 1100、
`floor_us` 20000、`ceiling_us` 200000、`warmup_samples` 4），`rto_max_*` 置 0（原因见文末）。
守护进程在首轮测试前启动、期间从未重启，以下是截至全部测试（及事后一次冒烟验证）结束的累计值：
`rack_rto.stats` 的 `applied` 6678、`rejected` 0，确认 M1 确实生效；`guardrail_hits` 0，
即排队时延护栏一次都没触发；`prr_adjustments` 0，与 M2 开启时按设计绕过 PRR 一致。

**tcp-brutal**：v2.0.0，用 `brutalctl add <客户端>/32 200` 按目的地生效，cwnd 增益取默认 2.0。
200 Mbps 即客户端链路的标称带宽——这正是 brutal 文档要求填写的值。

**bbr**：内核 6.12.63 自带版本，未调参。

## 方法上的四个关键决定

1. **对称切换**。三种算法一律用按目的地前缀的路由覆盖生效：
   `ip route replace <dst> via <gw> congctl lock <算法> proto 233`。这正是 `brutalctl`
   自己的做法，所以 brutal 和其余算法走完全相同的机制，不改应用、不动系统默认值。见 `setalgo.sh`。

2. **每轮轮换算法顺序（拉丁方）**。在会漂移的链路上，固定顺序会系统性地偏袒某个位置——
   那种偏差会被当成结论读。见 `run-matrix.sh` 的 `(rot - 1 + k) % n`。

3. **每轮开始前探测链路**（`path-probe.jsonl`），把漂移变成数据而不是未测量的混杂因素。

4. **隔离 cgroup 带来的优势**。`skyline_policy.bpf.c`（M1 动态 RTO）按 **cgroup** 生效而非按
   拥塞算法，而所有发包进程都在 `/sys/fs/cgroup/skyline-speeder` 内。若全程开启，bbr/brutal
   会一并蹭到同一套 RTO 调优，把差距做小。因此 `setalgo.sh` 让 `rack_rto` 随算法开关。

此外，因为链路漂移，报告同时给出**绝对中位数**和**轮内胜率**：后者只在同一轮（三者面对同一时段
链路）内比较排名，不受跨轮漂移影响。

## 主要结论

| 场景 | 结果 |
|---|---|
| 游戏 | **skyline_cc** 抖动 5/8 轮最优、卡顿 4/8 轮最优；两个加速器的 RTT 尾部都远好于 bbr |
| 视频 | **tcp-brutal** 全面领先：分块 p95 赢 6/8、启动延迟赢 7/8 |
| 网页 | 三者混杂，无稳定赢家；页面加载 brutal 4/7，TTFB bbr 4/7 |
| 大文件单流 | **bbr 与 skyline_cc 各 4/8**，tcp-brutal **0/8**（中位 46 Mbps，最低） |
| 大文件 4 并发 | **skyline_cc 117 / brutal 115 / bbr 77 Mbps**，两个加速器都明显胜过 bbr |
| 服务端重传率（全部场景累计） | bbr 10.0% < skyline_cc 11.6% < tcp-brutal 12.3% |

最值得注意的一条：**单流大文件上 tcp-brutal 发得最多（37.0 万报文段，其中 5.1 万重传，重传率 13.9%；bbr 为 9.9%，skyline_cc 为 11.1%）却传得最少（中位 46 Mbps）**。
在 10–30% 丢包下，为它配置的 200 Mbps 已远高于链路实际能交付的速率，而 brutal 的设计就是
"丢了就多发以维持目标速率"——这在高丢包下变成纯粹的损耗放大。tcp-brutal 自己的文档写得很清楚：
*"set it too high and you only produce loss."* 这不是 bug，是它把带宽判断交给运维的必然代价。

## 文件

| 文件 | 内容 |
|---|---|
| `RESULTS.md` | **完整数据表**：逐轮原始值、中位数+IQR、轮内胜率、服务端代价 |
| `head-to-head.jsonl` | 主数据集，120 条，每条含客户端指标 + 服务端 `nstat` 增量 |
| `path-probe.jsonl` | 每轮开始时的链路 RTT/丢包 |
| `run-matrix.sh` | 编排：轮换顺序、链路探测、带时限的运行；拒绝覆盖已有结果目录 |
| `server-setup.sh` | 服务端准备与收尾（`start` / `stop`）：独立的 nginx 实例 + 游戏回显服务，都放进 skyline cgroup |
| `gameserver.py` | 游戏场景的 ping-pong 回显服务 |
| `bench.py` | 客户端四场景测量；地址由 `SKYLINE_BENCH_HOST` 指定 |
| `setalgo.sh` | 服务端算法切换；`DST` 必填，下一跳沿用内核原本的选择 |
| `analyze.py` | 生成 `RESULTS.md` |
| `plot_comparison.py` | 从 `head-to-head.jsonl` 直接计算并生成两张 README 图；不硬编码任何数字，换了数据图就跟着变（纯标准库，不需要 matplotlib） |
| `first-pass-*.jsonl` | 首轮探索性数据（含 cubic，n=3，丢包 3–10%），**方法较弱，仅作参考** |

```bash
# 重新分析
python3 analyze.py head-to-head.jsonl path-probe.jsonl > RESULTS.md
# 重新出图
python3 plot_comparison.py head-to-head.jsonl ../../../docs/images
```

### 重新跑一遍

前提：服务端已装好 skyline-speeder（并已 `ssctl enable`）与 tcp-brutal，有 nginx 和 python3；
客户端只需 python3；两端都能免密 SSH 登录（编排脚本开了 `BatchMode`，密码提示会让矩阵卡死在半途）。

```bash
# 1. 服务端：把本目录拷过去，启动测试服务
sudo ./server-setup.sh start

# 2. 任意一台能 SSH 到两端的机器上：
SERVER=root@<服务端> CLIENT=root@<客户端> \
SERVER_ADDR=<客户端访问服务端用的地址> \
CLIENT_PREFIX=<服务端看到的客户端地址>/32 \
    ./run-matrix.sh results-$(date +%Y%m%d-%H%M)

# 3. 服务端：测完立即收尾——HTTP（8080）与回显（9999）两个端口对外开放
sudo ./server-setup.sh stop
```

8 轮约 45 分钟，产生约 5 GB 流量。客户端地址若在 NAT 之后，`CLIENT_PREFIX` 要填它的**公网出口**地址——
服务端看到的是那个，路由覆盖也只对它生效。

## 两个执行中发现的仓库/环境问题

- **动态 RTO 默认关闭，且配置文件里的 `[rack_rto]` 在启动时不生效**——这两点都是设计如此，
  文档有写（`docs/usage.md` 第五节）：守护进程启动时把 BPF 侧的 RTO 调节清零，直到显式执行
  `ssctl set-rack-rto`。源码安装和 `--prebuilt` 装的都是同一份生产模板 `speeder-guest.toml`，
  它刻意不写 `[rack_rto]`，因为实验矩阵依赖 `reset-rack-rto` 等于"关闭"。所以本次测试用
  `setalgo.sh` 显式下发取值。
  另外，从代码看，`ssctl status` 的 `rack_rto.config` 报告的是配置文件里的值而不是内核侧的实际
  状态；配置文件写了 `enabled = true` 时，状态会显示已开启、实际却没有生效。本次测试的配置
  文件没有 `[rack_rto]`，没有触发这种情况，也未在测试中复现。

- **`rto_max` 在 6.12 内核上全部被拒**。`TCP_RTO_MAX_MS` 需要 Linux 6.15+，本次内核 6.12.63，
  `rto_max_rejected: 59 / rto_max_applied: 0`。计数器如实记录，不属于静默失效，但测试时已关闭该半个功能。
