# 使用与调参指南（新手向）

这份文档假设你**不懂 eBPF、不懂拥塞控制算法**，只想把服务器调快。全程复制粘贴即可。

术语只需要知道三个：

- **丢包**：数据包在路上弄丢了。线路噪声造成的叫**随机丢包**；发太快把设备缓冲撑爆
  造成的叫**拥塞丢包**。本项目只对**随机丢包**有效。
- **RTT**：数据包一来一回的时间，单位毫秒。国内到国外一般 100–300ms。
- **发送端加速**：只装在**发数据的那一头**（你的服务器）。客户端什么都不用装、不用改。

> 所以：用户**下载**会变快，用户**上传**不会。这是设计如此，不是没生效。

---

## 一、先确认你该不该用

用错场景会**更慢**。花两分钟测一下：

```bash
sudo apt-get install -y iperf3
# 对端跑 iperf3 -s，然后从你的服务器发固定速率 UDP
iperf3 -c <对端IP> -u -b 100M -t 10
iperf3 -c <对端IP> -u -b 400M -t 10
```

看 `Lost/Total Datagrams` 那一列：

| 测试结果 | 说明 | 建议 |
|---|---|---|
| 各速率**都稳定丢包**（5% 以上） | 线路噪声，随机丢包 | **适合**，效果明显 |
| 低速不丢，速率一高才丢 | 你把线路撑满了，拥塞丢包 | **不适合**，可能更糟 |
| 各速率**都不丢** | 线路干净 | 差别不大 |

> [!WARNING]
> **别用 `ping` 的丢包率判断。** 绝大多数网络对 ping 限速，它显示的丢包和真实数据传输
> 无关。我们实测过一条线路 ping 显示丢 6%，实际 UDP 打到 1.1Gbps 都零丢包。

---

## 二、装完就能用，默认是什么样

一键安装后默认状态：

| 项目 | 默认 | 说明 |
|---|---|---|
| 四个加速模块 | **全开** | 见下一节 |
| 动态 RTO 调节 | **关闭** | 需要手动开，见第五节 |
| 重传包 DSCP 标记 | **关闭，且值为 0** | ⚠️ 见第六节，**开之前必须先问网络管理员** |
| 摘除后回落算法 | `cubic` | 出问题时自动退回的系统默认算法 |
| 拥塞控制与 qdisc 守护（guard） | **开**，每 5 秒检查一次 | 被别的脚本改回 bbr / cake 会自动改回来，见第七节 |

安装脚本结束时会打印一份摘要：拥塞控制和 qdisc 改之前、之后各是什么，常用命令，以及
几个最常用参数的当前值。装的过程中所有命令的输出都在 `/var/log/skyline-speeder-install.log`，
出错时先看它。

检查当前状态：

```bash
sudo ssctl status
```

---

## 三、四个加速模块，分别是干嘛的

| 模块 | 大白话 | 建议 |
|---|---|---|
| `adaptive-cwnd` | **自动调节一次能发多少**。核心模块，加速主要靠它 | 保持开 |
| `loss-classifier` | **分辨"丢包是噪声还是真拥塞"**，噪声就不减速 | 保持开 |
| `pacing` | **把数据均匀发出去**，而不是一股脑挤出去 | 保持开 |
| `early-loss` | **更早发现丢包**。当前内核上只做观测，不影响决策 | 保持开（开了也不会有副作用）|

全开（默认）：

```bash
sudo ssctl enable
```

> [!IMPORTANT]
> `ssctl enable` 执行成功后，**这台机器上所有新建 TCP 连接**都会默认走 `skyline_cc`
> （它会把 `net.ipv4.tcp_congestion_control` 改成 `skyline_cc`）。不是只影响某个进程、
> 也不是只影响 cgroup 里的进程。想退回去用 `sudo ssctl drain`。

只开其中几个（用来定位是哪个模块起作用）：

```bash
sudo ssctl enable --modules adaptive-cwnd,pacing
```

全部关掉（变成一个"什么都不做"的基准，用来做对比测试）：

```bash
sudo ssctl enable --all-off
```

> 全关时它不做任何加速，是一个中性基准，可以放心当作对照组（验证数据见
> [docs/03-design.md](03-design.md) 第 4 节）。

---

## 四、参数调整：先看这个致命陷阱

> [!CAUTION]
> **`ssctl set-module-config` 是"全量覆盖"，不是"只改你写的那个"。**
>
> 每个参数在命令行里都有内置默认值。你只写一个参数时，**其余 16 个会被静默重置成
> 内置默认值**，而不是保持你配置文件里的自定义值。
>
> ```bash
> # 危险：你以为只改了护栏，实际上其他 16 个参数全被重置了
> sudo ssctl set-module-config --guardrail-gain 0.5
> ```
>
> **正确做法**：先 `ssctl status` 看当前值，然后把**所有**你想保留的参数一起写全。
> 如果你从没改过配置文件，内置默认值恰好和出厂值一致，那么单独写一个是安全的。
>
> 例外：从 0.1.0 升级的主机。安装脚本不会覆盖已有的 `/etc/skyline-speeder/speeder.toml`，
> 里面仍是旧默认值（即下面的「④ 高随机丢包档」），和新版 `ssctl` 的内置默认值不同。
> 在这类主机上单独写一个参数，会把其余参数一起换成新默认值；而 `reset-module-config`
> 回到的是配置文件里的旧值。

想回到配置文件里的设置（全新安装的主机上就是出厂设置），随时可以：

```bash
sudo ssctl reset-module-config
```

### 常用参数，用大白话讲

**99% 的人不需要碰这些。** 装完默认就是调好的。

| 参数 | 它管什么 | 调大会怎样 | 调小会怎样 | 默认 |
|---|---|---|---|---:|
| `--cruise-inflight-gain` | **稳定期最多能比理论值多发多少** | 高丢包时更能顶住 | 高丢包下发不出去 | `3.0` |
| `--cruise-pacing-gain` | **稳定期发送速率是估算带宽的多少倍** | 更激进，可能堆队列 | 更保守，吞吐低 | `1.25` |
| `--guardrail-gain` | **发现真拥塞时降到原来的几成**（自我保护） | 降得少，更激进 | 降得狠，更安全 | `0.8` |
| `--loss-inflation-max-ratio` | **最多按多高的丢包率做补偿**（按 `1/(1-p)` 多发：0.10 ≈ 多发 11%，0.5 = 多发一倍） | 随机丢包链路上补偿更足；拥塞型瓶颈上会越丢越发 | 补偿更弱，更不容易自己把链路挤爆 | `0.10` |
| `--max-queue-delay-ms` | **排队延迟超过多少毫秒就认为堵了** | 更晚才减速，延迟更高 | 更早减速，延迟更低 | `70` |
| `--startup-gain` | **刚开始连接时冲多猛** | 起速快，可能过冲 | 起速慢更稳 | `3.0` |
| `--initial-cwnd-packets` | **一开始就发多少个包**（不用慢慢试探） | 小文件传输更快 | 更保守 | `100` |
| `--max-pacing-mbps` | **速率硬上限**，单位 Mbps | 允许跑更快 | 限速 | `1200` |
| `--max-cwnd-packets` | **未确认数据的硬上限**，单位包数 | 允许更多在途数据 | 省内存但限吞吐 | `50000` |
| `--min-cwnd-packets` | **窗口的下限**，单位包数（只在 `adaptive-cwnd` 开启时生效） | 小流量连接丢包后窗口里还有足够的包触发快速恢复，不必干等超时；发送速率仍由 pacing 决定，不会因此发得更快 | 回到历史行为 | `4` |

**改完立刻生效，不用重启**，而且是在每条连接的 RTT 边界上平滑切换，不会传到一半出乱子。

### 四档现成配方（复制即用）

Skyline Speeder 没有 BBR 那样的内置档位，只有裸参数。下面四档是我们整理的等价档位，
**直接复制粘贴**即可，不需要理解每个参数。

**① 保守档 —— 低延迟优先**（游戏、语音、SSH 交互）

```bash
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 50 --max-queue-delay-ratio 0.5 \
  --initial-cwnd-packets 50 --min-cwnd-packets 4 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 3 --startup-growth-ratio 0.25 --startup-gain 2.0 \
  --cruise-inflight-gain 1.5 --cruise-pacing-gain 1.05 \
  --guardrail-gain 0.7 --loss-inflation-max-ratio 0.10
```

**② 默认档 —— 出厂设置**（等价于 `ssctl reset-module-config`；从 0.1.0 升级且没改过配置文件的主机上，它回到的是 ④。要切到新默认档，执行不带任何参数的 `sudo ssctl set-module-config`，它发送的正是新版内置默认值；想重启后依然生效，再改配置文件，见「让档位重启后依然生效」）

```bash
sudo ssctl reset-module-config
```

**③ 激进档 —— 吞吐优先**（大文件下载、视频分发、确认线路有随机丢包）

```bash
sudo ssctl set-module-config \
  --max-pacing-mbps 2000 --max-cwnd-packets 100000 \
  --max-queue-delay-ms 200 --max-queue-delay-ratio 2.0 \
  --initial-cwnd-packets 200 --min-cwnd-packets 4 \
  --min-rtt-window-s 30 --bw-window-rtts 10 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.15 --startup-gain 4.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.3 \
  --guardrail-gain 1.0 --loss-inflation-max-ratio 0.5
```

激进档相对默认档改了什么：

| 参数 | 默认 | 激进 | 效果 |
|---|---:|---:|---|
| `cruise-pacing-gain` | 1.25 | **1.3** | 稳定期按带宽估计的 1.3 倍发 |
| `cruise-inflight-gain` | 3.0 | 3.0 | 与默认档相同 |
| `startup-gain` | 3.0 | **4.0** | 起步冲得更猛 |
| `startup-growth-ratio` | 0.20 | **0.15** | 带宽每轮增长不到 15% 才算到顶，更晚退出 STARTUP |
| `guardrail-gain` | 0.8 | **1.0** | 检测到拥塞时**不再降速**（1.0 = 中性） |
| `max-queue-delay-ms` / `max-queue-delay-ratio` | 70 / 0.6 | **200 / 2.0** | 排队延迟高得多才认为"堵了" |
| `loss-inflation-max-ratio` | 0.10 | **0.5** | 丢包补偿放开到最多多发一倍 |
| `bw-window-rtts` | 6 | **10** | 带宽估计记住更久以前的峰值 |
| `initial-cwnd-packets` | 100 | **200** | 连接一建立就发 200 个包 |
| `max-cwnd-packets` / `max-pacing-mbps` | 50000 / 1200 | **100000 / 2000** | 抬高两个硬上限 |

**实测效果**（单台服务器 → 客户端，RTT ≈ 18ms 的干净链路，下载 50MB，5 轮交错对比）。
这组数据测于默认系数调整**之前**：表里的「默认档」是当时的默认值，也就是下面的
「④ 高随机丢包档」；现在的默认档没有在这条链路上重测过。

| | 默认档 | 激进档 |
|---|---:|---:|
| 吞吐中位数 | 85.6 MB/s | **143.6 MB/s** |
| 逐轮配对提升 | 基准 | **+54%（中位）** |
| 胜出轮次 | — | **5 / 5** |
| 空载延迟 | 18.4 ms | 18.4 ms |
| 满载延迟 | 18.0 ms | 18.1 ms |

这条链路上激进档**没有测出延迟代价**——因为带宽充裕，没把队列堆起来。
但这不代表在你的链路上也如此：带宽紧张或存在瓶颈缓冲时，`guardrail-gain=1.0`
会让它一路挤到丢包为止。**换档后请自己测一遍满载延迟再决定是否长期使用。**

> [!WARNING]
> 激进档把 `guardrail-gain` 设成 1.0，等于**关掉了自我保护降速**。在真正拥塞的链路上
> 它会持续挤占缓冲、推高延迟，也会挤压同链路上的其他流量。
> **只在"线路带宽充足"时使用。**

**④ 高随机丢包档 —— 原默认值**（链路确实是 10%-20% 的**随机**丢包、不是被挤爆的丢包）

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

这是 [性能验证报告](04-performance-report.md) 里 `skyline-best` 测的那组系数，针对的是
"丢包不代表拥塞"的链路：丢包补偿放到最多多发一倍，护栏放得很宽。默认档后来改掉它，
是因为在并发连接很多、丢包主要来自瓶颈被挤满的生产环境里，这组系数会越丢越发——
重传明显增多，速度却没有换来。**判断方法**：换档后看重传占比和 RTO 超时数（做法见
文末「附：视频卡顿 / 大文件传输中断怎么查」），重传涨了而吞吐没涨，就说明你的丢包是
挤出来的，回默认档。

### 让档位重启后依然生效

`ssctl set-module-config` 改的是**内存态，重启会丢**。要长期生效，改配置文件：

```bash
sudo cp /etc/skyline-speeder/speeder.toml /etc/skyline-speeder/speeder.toml.bak
sudo nano /etc/skyline-speeder/speeder.toml
```

顶层字段对应 `--max-*` 系列，`[adaptive_cwnd]` 段对应各种 gain，改完：

```bash
# 先校验，不通过就别重启
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only

sudo systemctl restart skyline-speederd.service
sudo systemctl restart skyline-speeder-enable.service
```

### 有硬上限的参数（填超了整组命令会被拒绝）

| 参数 | 允许范围 | 备注 |
|---|---|---|
| `--bw-window-rtts` | **1 – 10** | 默认 6；10 是硬上限 |
| `--loss-inflation-max-ratio` | **0 – 0.5** | 默认 0.10；0.5 是 BPF 侧硬编码的上限（500‰） |
| `--guardrail-gain` | 0 – 1.0 | 1.0 = 不降速；0 = 关闭该护栏 |
| `--startup-growth-ratio` | 0 – 1.0 | |
| `--startup-gain` / `--cruise-*-gain` | ≥ 1.0 | 无上限，但越大越容易堆队列 |
| `--max-cwnd-packets` | ≥ 4 | |
| `--min-cwnd-packets` | **4 – `--max-cwnd-packets`** | 低于 4 会被拒绝（BPF 侧本来就不会让窗口低于 4） |

> 命令是**全量覆盖**的：只要有**任何一个**参数越界，**整条命令都会被拒绝**，
> 已生效的配置保持不变。返回里会写明是哪个参数、允许范围是多少。

### 改参数的正确姿势

```bash
# 1. 先看现在是什么
sudo ssctl status

# 2. 把所有要保留的值一起写全（这里示范：只想把护栏放松到 0.9）
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 70 --max-queue-delay-ratio 0.6 \
  --initial-cwnd-packets 100 --min-cwnd-packets 4 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.20 --startup-gain 3.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.25 \
  --guardrail-gain 0.9 \
  --loss-inflation-max-ratio 0.10

# 3. 不满意就一键还原
sudo ssctl reset-module-config
```

> 在线改的参数**重启后会丢失**。要长期生效，请改 `/etc/skyline-speeder/speeder.toml`
> 里对应的字段，然后 `sudo systemctl restart skyline-speederd`。

---

## 五、动态 RTO 调节（默认关闭）

**RTO** = 等多久没收到回应就重发。内核默认最长能退到 **120 秒**——线路抖一下，
连接就可能卡死两分钟。这个功能给它加个上限。

我们实测过：没有上限的对照组里，RTO 被顶到过 **101 秒**。

> [!CAUTION]
> **RTO「上限」需要内核 ≥ 6.15，在 6.12 上无法生效。**
>
> 这个功能靠 `TCP_RTO_MAX_MS`（socket 选项编号 **44**）下发。该编号在 Linux 6.12 里
> **尚未分配**（该内核最大只到 43 = `TCP_IS_MPTCP`），内核会返回 `ENOPROTOOPT`。
>
> 表现是：命令能成功下发、配置也显示已生效，但 `ssctl status` 里
> **`rack_rto.stats.rto_max_rejected` 持续增长而 `rto_max_applied` 恒为 0**。
> 我们在 Debian 6.12.101 上实测确认了这一点。
>
> **RTO「下限」（`floor_us` / `srtt_permille` / `ceiling_us`）不受影响，在 6.12 上正常工作**
> ——它走的是另一个选项 `TCP_BPF_RTO_MIN`（1004），该内核支持。
>
> 检查你的内核支不支持：
> ```bash
> grep -c "TCP_RTO_MAX_MS" /usr/src/linux-headers-$(uname -r)*/include/uapi/linux/tcp.h
> # 输出 0 = 不支持上限调节；非 0 = 支持
> ```
> 配置留着不会有副作用（只是被内核忽略），升级到 6.15+ 后会自动开始生效。

> [!CAUTION]
> **只在配置文件里写 `[rack_rto] enabled = true` 是不够的。** daemon 启动时不会把这段
> 推进 BPF map，必须**显式执行一次 `ssctl set-rack-rto`** 才会真正订阅。
> 实测：仅靠配置文件时 `established_cb` 有 2169，但 `subscribe_ok` / `rtt_callbacks`
> / `applied` 全是 0；执行命令后才开始增长。
>
> 因此**每次重启后都要重跑一次**，或把它加进开机脚本。

开启：

```bash
sudo ssctl set-rack-rto \
  --srtt-permille 1100 \
  --floor-us 20000 \
  --ceiling-us 200000 \
  --warmup-samples 4
```

| 参数 | 大白话 | 建议值 |
|---|---|---|
| `--srtt-permille` | 目标 RTO = 实测延迟 × 这个数 ÷ 1000。1100 = 1.1 倍 | `1100` |
| `--floor-us` | **最小**等待时间（微秒）。太小会误判成丢包乱重发 | `20000`（20ms） |
| `--ceiling-us` | RTO **下限**的上界（微秒）。**不是退避上限**，且内核硬卡在 200000 | `200000`（200ms） |
| `--warmup-samples` | 前几次只观察不干预，等延迟测准了再动手 | `4` |

**真正的「退避上限」是另外两个参数**（就是防卡死的关键，需内核 ≥ 6.15）：

```
退避上限 = clamp(permille × 基准RTT ÷ 1000,  1000ms,  120000ms)
                                             ↑ 夹住下限，所以 1 秒是能设到的最小值
```

| 参数 | 大白话 | 说明 |
|---|---|---|
| `--rto-max-normal-permille` | 平时的退避上限倍数 | `0` = 关闭本功能，保持内核 120 秒默认 |
| `--rto-max-congested-permille` | 检测到真拥塞后换用的倍数 | 必须 ≥ normal |
| `--rto-max-congestion-ratio-permille` | 延迟涨到几倍算「真拥塞」 | `0` = 用内置默认 2000（2 倍） |

以 RTT ≈ 120ms 的链路为例，想要**平时 1 秒、拥塞时 2 秒**：

```bash
sudo ssctl set-rack-rto \
  --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 --warmup-samples 4 \
  --rto-max-normal-permille 3000 \
  --rto-max-congested-permille 16700
```

算法：`3000` → 3.0 × 120ms = 360ms，低于 1000ms 下限 → 夹到 **1 秒**；
`16700` → 16.7 × 120ms ≈ **2 秒**。

> 注意公式挂在**每条连接各自的 RTT** 上，不是绝对值。RTT 更大的连接，同样的倍数会
> 算出更大的上限。想要"任意 RTT 都是 1 秒"，把 normal 设小即可（夹住下限就是 1 秒）。

关闭：

```bash
sudo ssctl reset-rack-rto
```

> [!IMPORTANT]
> **这个功能有个前提：进程必须在指定的 cgroup 里，否则完全不生效，而且不报错。**
>
> 检查：`sudo ssctl status` 里如果 `rack_rto.stats.applied` 一直是 0，就是没生效。
>
> 让你的服务进入 cgroup：
> ```bash
> sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <你的服务启动命令>
> ```
> systemd 服务的做法见 [DEPLOY.md 第 7 节](../DEPLOY.md)。
>
> 加速主体（`skyline_cc`）**不受这个限制**，永远全局生效。

---

## 六、⚠️ 重传包 DSCP 标记：默认关闭，且默认值是 0

**这是最容易配错的功能，请完整读完再动手。**

它的作用：给**重传的数据包**打一个 DSCP 标记（IP 头里的优先级字段），
让上游的路由器/运营商设备能识别出"这是重传包"，从而单独做分流或优先处理。

### 为什么默认是 0

```toml
[retransmit_dscp]
enabled = false
dscp_value = 0      # <- 占位符，不是可用值
```

`dscp_value = 0` 是**占位符**，代表"未配置"。DSCP 0 在标准里是"尽力而为"（默认等级），
**打上去等于没打**。

> [!CAUTION]
> **DSCP 的具体数值必须由你的网络方（运营商 / 机房 / 上游网络管理员）指定。**
>
> 这个值不是你自己拍脑袋定的——它必须和上游设备的策略对上，否则：
> - 填错了：上游不认识，标记被忽略（白做）；
> - 填成别人在用的值：你的流量可能被误分到别的策略里，**可能更慢甚至被限速**。
>
> **开启前请先联系网络方，问清楚"重传流量应该打哪个 DSCP 值"。**

### 确认拿到值之后怎么开

```bash
# 把 <值> 换成网络方给你的数字（0-63）
sudo ssctl set-retransmit-dscp --dscp-value <值>
```

> 注意：这条命令**同时就把功能打开了**（`enabled` 会被自动设为 true），
> 不需要另外再执行开启命令。

关闭：

```bash
sudo ssctl reset-retransmit-dscp
```

### 补充说明

- 它挂在网卡出方向上，对该网卡**所有 TCP 流量**一致生效，不区分用的是哪种拥塞控制算法。
- 它只挂在以太网网卡上。配置里的 `runtime.tc_interface` 是 WireGuard / WARP、tun 这类隧道时
  不会挂载，上面这条开启命令会报错（`ssctl status` 的 `capabilities` → `notes` 里写明原因）：
  把 `tc_interface` 改成承载隧道流量的那块物理网卡，再重启 `skyline-speederd`。
- 它**不受** `ssctl drain` 影响。摘除加速功能时，这个标记仍然在工作，要单独关。
- IPv4 和 IPv6 都支持，我们实测零误标记。

---

## 七、确认真的生效了

```bash
sudo ssctl status
```

四件事要对：

```
"enabled": true                      <- 加速已挂载
"capabilities": { ... 六项硬性前提全 true }   <- 环境满足要求
"active_flows": 大于 0                <- 有连接在用
"armed": true（在 "guard" 里）        <- 拥塞控制和 qdisc 正被守着
```

**光看这个还不够**，再确认真实连接确实在用它（服务器有流量时执行）：

```bash
ss -tin | grep -c skyline_cc
```

大于 0 就说明真的生效了。是 0 的话，可能是已建立的老连接不会中途切换，等新连接即可。

> `capabilities` 里 `rack_reo_hook: false` 是**正常的**，不影响使用。

### 之前装过「一键 BBR」、改过 qdisc 的机器

不用先卸载它们，也不用手动改回来。`ssctl enable`（安装脚本会自动执行）会：

- 把拥塞控制改成 `skyline_cc`；
- 把默认 qdisc（`net.core.default_qdisc`）改成 `fq`；
- 把出口网卡上的 qdisc（`cake`、`fq_pie`、`fq_codel` 等）换成 `fq`——`skyline_cc` 靠 `fq`
  把数据均匀发出去。出口是 VLAN、bond 或网桥时，换的是它下面的物理网卡（网桥上虚拟机、
  容器的端口不碰）。

之后 `skyline-speederd` 每 5 秒检查一次，谁改回去就再改过来（比如有脚本又执行了
`sysctl --system`），并在日志里记一笔：

```bash
journalctl -u skyline-speederd | grep 'guard:'
```

带 `something else on this host changed it` 的行反复出现，说明机器上有东西一直在改。
不影响加速；想根除，就去 `/etc/sysctl.conf`、`/etc/sysctl.d/` 里找写着 `bbr`、`cake` 的
那一行删掉（安装结束时的摘要会列出这些文件）。

以下情况它**不会动**：

- 网卡上是你自己搭的限速 / 整形 qdisc（`htb`、`tbf`、`netem` 等，以及设了带宽的 `cake`，
  比如 `cake bandwidth 90Mbit`）——替换会把你的限速配置弄没。`ssctl status` 的 `guard` →
  `notes` 里会写明它没动；
- 出口是 WireGuard / WARP 这类隧道、下面找不到物理网卡时——`notes` 里会写
  `is a virtual device ... no qdisc checked`。想让它管，就把配置里的 `runtime.tc_interface`
  改成真正的物理网卡；
- 执行过 `ssctl drain` 之后——摘除即停止守护，qdisc 保持现状（仍是 `fq`）。

不想让它管 qdisc：把 `/etc/skyline-speeder/speeder.toml` 里 `[guard]` 段的 `qdisc` 改成
`false`，然后：

```bash
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only
sudo systemctl restart skyline-speederd.service
```

---

## 八、常见场景配方

### 场景 1：普通跨境服务器，不确定线路情况

**什么都别改。** 安装完就是调好的。

### 场景 2：想验证到底有没有效果

```bash
# 基准：把模块全关（不做任何加速的中性基准）
sudo ssctl enable --all-off
# ... 测速 ...

# 全开
sudo ssctl enable
# ... 同样条件再测一次 ...
```

> 线路本身波动很大，**单次测试不可信**。同一时段交替测 3 轮以上取中位数。

### 场景 3：延迟敏感业务（游戏、语音），想更保守

```bash
# 更早减速、降得更狠
sudo ssctl set-module-config \
  --max-pacing-mbps 1200 --max-cwnd-packets 50000 \
  --max-queue-delay-ms 50 --max-queue-delay-ratio 0.5 \
  --initial-cwnd-packets 100 --min-cwnd-packets 4 \
  --min-rtt-window-s 30 --bw-window-rtts 6 \
  --startup-plateau-rtts 5 --startup-growth-ratio 0.20 --startup-gain 3.0 \
  --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.05 \
  --guardrail-gain 0.7 \
  --loss-inflation-max-ratio 0.10
```

### 场景 4：连接经常莫名卡死几十秒

开启第五节的动态 RTO 调节，并确认进程在 cgroup 里。

---

## 附：视频卡顿 / 大文件传输中断怎么查

典型症状：**4K 卡死但 2K 正常**、大文件下到一半不动、SSH 卡几十秒又恢复。

### 第一步：看有没有连接陷入 RTO 退避

```bash
# 有输出就说明有连接在指数退避，数字越大退避越深
ss -tin | grep -oE 'backoff:[0-9]+' | sort | uniq -c

# RTO 分布，正常应集中在几百毫秒
ss -tin | grep -oE 'rto:[0-9]+' | sed 's/rto://' | sort -n | tail -5
```

一旦看到 `backoff:10` 以上、`rto` 上万毫秒，就是它。我们实测遇到过
**RTT 仅 101ms 而 rto=120000ms（120 秒）** 的连接——这条连接实际已经死了。

> **为什么 4K 先崩、2K 没事**：AnyTLS / VLESS 这类协议做会话复用，多条流跑在同一条
> TCP 上。这条 TCP 一旦卡住，其上所有流一起卡。4K 码率高、播放器缓冲余量小，最先
> 表现出来；2K 码率低，能靠缓冲扛过短暂停顿。

### 第二步：看是不是参数开太猛

```bash
# 40 秒内的重传率与 RTO 超时次数
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
print(f"重传率 {r*100/o:.2f}%  RTO超时 {b['TcpExt:TCPTimeouts']-a['TcpExt:TCPTimeouts']} 次")
EOF
```

我们在同一台机器上实测三档（每档 40 秒真实业务流量，中位数）。这组数据测于默认系数调整
**之前**：「默认档」是当时的默认值，也就是上面的「④ 高随机丢包档」；「保守档」也是调整前的
版本。现在的默认档和保守档没有用这个方法重测过，下面「2.7 倍」的结论只适用于激进档与 ④ 的对比。

| 档位 | 重传率 | **RTO 超时 / 40 秒** |
|---|---:|---:|
| 激进档 | 10.1% | **93** |
| 默认档 | 8.8% | **35** |
| 保守档 | 6.0% | **25** |

**激进档的 RTO 超时是默认档的 2.7 倍**，三轮测试全部最高。RTO 超时就是卡顿的直接来源。

**处置：先回默认档**（从 0.1.0 升级且没改过配置文件的主机上，`reset-module-config` 回到的是 ④；
要回新默认档，改用不带任何参数的 `sudo ssctl set-module-config`，见 ②）

```bash
sudo ssctl reset-module-config
```

如果还想要速度，**不要动 `guardrail-gain`**（那是自我保护开关），
只小幅提 `cruise-pacing-gain`（1.25 → 1.3 → 1.35），每档跑上面的脚本看 RTO 超时数，
涨得明显就退回去。

### 第三步：限制死连接的挂起时长

内核默认 `tcp_retries2 = 15`，连接进入退避后最长会挂**约 15 分钟**才被放弃。

理论上应该用 `rto_max_*_permille` 把退避上限压到 1–2 秒，
但**那需要内核 ≥ 6.15**（见第五节）。6.12 上的替代方案是缩短放弃阈值：

```bash
cat > /etc/sysctl.d/99-zz-skyline-speeder-retries.conf <<'EOF'
net.ipv4.tcp_retries2 = 8
EOF
sudo sysctl -p /etc/sysctl.d/99-zz-skyline-speeder-retries.conf
```

让死连接**快速失败、客户端重连**，而不是长时间悬挂。
值越小失败越快（8 ≈ 100 秒量级，6 ≈ 25 秒量级）；太小会误杀网络短暂抖动中
本可恢复的连接，移动端用户多时不建议低于 6。

## 九、出问题了怎么退回

```bash
# 1. 参数改乱了 -> 还原成配置文件里的参数（从 0.1.0 升级的主机上是旧默认值）
sudo ssctl reset-module-config

# 2. 想暂时不用加速（等现有连接自然结束，不断线）
sudo ssctl drain --timeout 60

# 3. 完全停掉
sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service

# 4. 彻底卸载（配置文件会保留；安装前的拥塞控制和 qdisc 会还原回去）
sudo ./install.sh --uninstall
```

> [!NOTE]
> 卸载后 `bpftool struct_ops show` 可能还能看到 `skyline_cc`。**这不是失败**——
> 只要还有连接在用它，内核就不释放。这些连接断开后或重启后会自动消失。

**DSCP 标记和 RTO 调节不受 `drain` 影响**，要单独关：

```bash
sudo ssctl reset-retransmit-dscp
sudo ssctl reset-rack-rto
```

---

## 十、还想深入

- [docs/02-interface-reference.md](02-interface-reference.md) —— 每个命令、每个字段的精确定义
- [docs/03-design.md](03-design.md) —— 算法原理、为什么这么设计
- [docs/04-performance-report.md](04-performance-report.md) —— 完整测试数据与局限性
- [DEPLOY.md](../DEPLOY.md) —— 自动化部署与排障
