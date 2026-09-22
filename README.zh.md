<div align="center">

# Skyline Speeder

**服务器发送端 TCP 加速：eBPF struct_ops 拥塞控制 + Rust 控制面**

*Sender-side TCP acceleration. eBPF struct_ops congestion control with a Rust control plane. Clients need no changes.*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#内核版本要求)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-black.svg)](rust-toolchain.toml)

**[English](README.md)** · 简体中文

---

### 由 **Skyline Connect** 独家冠名

*Exclusively sponsored by Skyline Connect*

---

</div>

## 这是什么

Skyline Speeder 是一套**只需部署在 TCP 连接服务器发送端**的加速方案——客户端保持标准 TCP，不需要任何改动，也不需要任何客户端软件。

四个组成部分：

| 组件 | 形态 | 作用 |
|---|---|---|
| `skyline_cc` | eBPF struct_ops 拥塞控制 | 自适应 cwnd + 丢包率补偿 + pacing |
| `skyline_policy` | cgroup sockops | 早期丢包感知，动态调节 RTO 下限/上限 |
| `skyline_tc` | TC egress 程序 | 重传包 DSCP 标记与统计 |
| `skyline-speederd` / `ssctl` | Rust 用户态 | 常驻控制面与命令行 |

适用场景：**带宽充足、存在非拥塞性随机丢包的单向长流**。

> [!IMPORTANT]
> 自保护机制只识别两个真实拥塞信号：队列时延增长与 ECN 标记。**如果链路的丢包本质来自网络设备排队**（而非无线链路层这类与排队无关的随机丢失），丢包补偿机制与排队护栏会有**相反的作用方向**。部署前请评估目标链路的丢包性质——固定速率 UDP 打流零丢包而 TCP 大量重传，通常意味着丢包是自身突发造成的排队溢出，而非随机丢包。

## 实测性能

测试床：双 VM KVM，内核 6.12.101，`htb` + `netem` 注入链路条件，固定随机数种子。链路 100 Mbit/s。

**目标环境 9 点网格（Mbit/s 中位数）**

| 场景 | 内核 CUBIC | 原生 BBR | **Skyline Speeder** | vs BBR |
|---|---:|---:|---:|---:|
| `rtt100-loss10` | 0.31 | 84.28 | **95.36** | +13% |
| `rtt100-loss20` | 0.17 | 3.54 | **94.92** | **26.8x** |
| `rtt200-loss15` | 0.12 | 30.89 | **95.04** | **3.1x** |
| `rtt200-loss20` | 0.10 | 21.16 | **95.13** | **4.5x** |
| `rtt300-loss15` | 0.10 | 10.88 | **79.44** | **7.3x** |
| `rtt300-loss20` | 0.05 | 3.48 | **79.92** | **23.0x** |

**9/9 点主判据与稳健性判据全部通过**，全部远超"至少领先 5%"的验收线。高 RTT 端加样本到 `n=5` 后，Skyline Speeder 自身变异系数均在 4% 以内。

其余关键结论：

- **中性性成立**：四个模块全部关闭时，与内核原生 CUBIC 在零丢包场景差异 **<0.01%**——关闭路径没有引入任何系统性偏差。
- **护栏无回归**：`guardrail_gain=0.8` 的自保护降速，在最容易触发护栏的 0% 丢包场景与"不降速"对照组差异 <0.01%。
- **RTO 上限有实测证据**：无上限对照组（含纯内核默认 BBR）实测到 RTO 被顶到约 **101 秒**，逼近内核 120 秒默认上限；有上限配置 3/3 全部存活。
- **已知劣势**：`reorder-dsack`（纯重排序，不在目标场景内）是唯一不如基线的场景，比 CUBIC 低约 9%、比 BBR 低约 13%。

完整方法学见 [性能验证报告](docs/04-performance-report.md)。

> [!NOTE]
> 这些数字只来自一套上限 100 Mbit/s 的双 VM 测试床，不代表你的链路。复现步骤见
> [research/experiments/README.md](research/experiments/README.md)。
>
> 上面的数字测的是**当时的**默认系数——那组系数针对随机丢包调校，现在以「高随机丢包档」的名字保留在
> [使用与调参指南](docs/usage.md) 里。当前的默认系数是后来在一台生产部署上调出来的（那里的丢包主要来自
> 瓶颈被挤满），**没有**在这套测试床上重跑过。链路确实是 10%-20% 随机丢包时，套用该档即可得到这里测的行为。

## 内核版本要求

**精确 ABI 下限是 6.10；实际支持并验证的下限是 6.12 LTS。**

`skyline_cc.bpf.c` 挂在 `tcp_congestion_ops.cong_control` 上的回调用 **4 个参数**声明（`sk, ack, flag, rs`）。该签名自 v6.10 才启用——v6.9 及更早版本该函数指针只有 2 个参数（`sk, rs`），BPF 验证器会在加载时直接拒绝，报错明确指向参数个数不匹配。

> **`6.1.x` 和 `6.6.x` 两个 LTS 分支不支持**，两者都停留在旧的 2 参数签名。Debian 12 与 Ubuntu 22.04/24.04 的原装内核也都低于 6.10，在这些系统上需要另装 6.12 及以上的内核。

已验证：`6.12.101`、`6.18.42`、`7.1.6` 通过（含 IPv4/IPv6 数据路径冒烟测试）；`6.1.180` 和 `6.6.148` 按设计加载失败。

## 快速开始

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

> [!NOTE]
> 最小化服务器镜像上经常**既没有 `curl` 也没有 `sudo`**。先装一个下载工具；已经是
> root 就把 `sudo` 去掉：
>
> ```bash
> apt-get update && apt-get install -y curl        # 或 wget
>
> curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
> wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
> ```

或者自己克隆下来看过再装（**推荐**，这个脚本会改全机 TCP 行为）：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh
```

一键脚本会完成：安装工具链（clang/llvm/libbpf/bpftool/Rust）→ 用**本机内核 BTF** 编译三个 CO-RE BPF 对象 → 构建 Rust 控制面 → 自动探测并写入出口网卡 → 过一遍内核验证器 → 启动并配置开机自启。**不安装任何代理，不监听任何端口。**

```bash
sudo ./install.sh --prebuilt   # 装已发布的产物，不需要编译工具链
sudo ./install.sh --check      # 只做前置检查
sudo ./install.sh --no-enable  # 安装但不挂载 skyline_cc
sudo ./install.sh --uninstall  # 卸载（保留 /etc/skyline-speeder 配置）
```

> [!IMPORTANT]
> **升级已有安装：** 重新运行安装器只会替换文件，不会重启已经在运行的 daemon，旧版本会继续运行。请先用 `sudo systemctl stop skyline-speeder-enable.service skyline-speederd.service` 停掉服务再安装，或者装完后重启 `skyline-speederd`。详见 [CHANGELOG.md](CHANGELOG.md) 的 *Upgrading from 0.1.0*。

### 不装编译工具链的安装方式

`--prebuilt` 下载已发布的产物而不是现场编译，目标机**不需要 clang、LLVM、bpftool
和 Rust**，只要 `curl` 和 `tar`。随包的 BPF 对象是用 6.12 LTS 生成的固定参考头编译
的，加载时由 CO-RE 按**这台机器**的内核修正字段偏移。

```bash
sudo ./install.sh --prebuilt                  # 最新 release
sudo ./install.sh --release v0.2.0            # 指定 tag

# 镜像站、内网制品库，或者根本连不上 github.com 的机器：
# 自己把 tarball 拷过去，然后指过去
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz \
  sudo -E ./install.sh --prebuilt
```

内核要求不变：6.12 LTS 以上，且 `/sys/kernel/btf/vmlinux` 必须存在——CO-RE 正是
在加载时对着它做重定位。预编译的 daemon 在 Ubuntu 24.04 上构建，还需要 glibc 2.38
以上以及 `libelf.so.1`、`libz.so.1`（Debian 13、Ubuntu 24.04 及更新版本满足）；
用户态更旧的系统，比如换了 backports 内核的 Debian 12，请从源码构建（这一组合尚未实测）。每个产物都带一份 `MANIFEST`，记录 commit、参考头指纹，以及
每个二进制和对象的指纹。

安装器会在改动任何东西之前记录当前的拥塞控制算法与默认 qdisc，`--uninstall`
会把它们还原。**装之前跑 BBR 的机器，卸载后回到 BBR**，而不是停在 `fallback_cc`。

> [!NOTE]
> 安装器会在本机编译 BPF 对象，这也是它需要 clang/LLVM/bpftool 的原因。但这是**当前安装器的做法，不是对象本身的限制**：它们是 CO-RE 可重定位的，能在非编译内核上加载。已在 Debian 13 上双向实测——6.12.63 下编译的对象在 6.19.14 上加载并正常跑流量；用 6.12 的头在 6.19.14 上编译出的对象，在 6.12.63 上加载通过。`infra/kernel/core-portability.sh` 把这个检查固化成可重复执行的脚本。

日常操作：

```bash
ssctl status    # 运行时状态、能力集与决策计数器
ssctl flows     # 当前 skyline_cc 活跃连接数
ssctl drain     # 优雅摘除，等待存量连接自然结束
```

> [!TIP]
> `ssctl drain` 会等所有 skyline_cc 连接结束——**你自己的 SSH 会话就是其中一条**，
> 所以通过 SSH 执行它必然走到超时。这是安全的：drain 在开始等待**之前**就已经把
> sysctl 写回 `fallback_cc`，无论如何都不会再有新连接用 skyline_cc。struct_ops 会
> 保持挂载直到最后一条流结束，内核把这报告为一个活跃引用，不是错误。

## cgroup 前提

**这是最容易被忽略、也最不容易被发现的一步。** `skyline_policy` 的动态 RTO 调节挂在 `/sys/fs/cgroup/skyline-speeder` 上，只有被迁移进该 cgroup 的进程建立的连接才会经过这段 BPF 代码。**进程不迁移进去不会报任何错误——只是静默不生效**，`ssctl status` 里 `rack_rto.stats.applied` 会一直停留在 0。

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <你的服务启动命令...>
```

`skyline_cc` 是全局 struct_ops、`skyline_tc` 挂在接口级 TC 上，两者都不受此限制。

## 可调项一览

装完即是调好的默认值，**绝大多数场景不需要改**。可调的东西分四类：

**1. 四个加速模块**（默认全开，可单独开关）

| 模块 | 大白话 |
|---|---|
| `adaptive-cwnd` | 自动调节一次能发多少 —— 加速主要靠它 |
| `loss-classifier` | 分辨"丢包是噪声还是真拥塞"，是噪声就不减速 |
| `pacing` | 把数据均匀发出去，而不是一股脑挤出去 |
| `early-loss` | 更早发现丢包（当前内核上只观测，不影响决策）|

```bash
ssctl enable                                  # 全开（默认）
ssctl enable --modules adaptive-cwnd,pacing   # 只开指定模块
ssctl enable --all-off                        # 全关，作为对照基准
```

> [!IMPORTANT]
> **`ssctl enable` 是全机生效的。** 附加成功后它会写
> `net.ipv4.tcp_congestion_control = skyline_cc`，这台机器上所有新建 TCP 连接都会
> 走本算法，不只是 cgroup 内进程建的连接。`ssctl drain` 是对称的反向操作：先把
> sysctl 写回 `fallback_cc`，再等存量连接结束。

**2. 十七个运行时参数**（在线生效，无需重启）

发送增益、护栏降速幅度、丢包补偿上限、排队延迟判定阈值、起步激进度、速率与窗口硬上限等，
全部可用 `ssctl set-module-config` 在线调整，在每条连接的 RTT 边界平滑切换。

> [!CAUTION]
> `set-module-config` 是**全量覆盖**语义：只写一个参数时，**其余参数会被重置为命令行
> 内置默认值**，而不是保持你配置文件里的自定义值。详见
> [使用与调参指南](docs/usage.md#四参数调整先看这个致命陷阱)。

**3. 动态 RTO 调节**（默认**关闭**）

内核默认 RTO 最长能退到 120 秒——线路抖一下连接就卡死两分钟。我们实测过对照组被顶到
**101 秒**。开启后可给它设上下限。需要目标进程位于指定 cgroup 内才生效。

**4. 重传包 DSCP 标记**（默认**关闭**，且 `dscp_value = 0`）

> [!WARNING]
> **`dscp_value` 默认值 0 是占位符，不是可用值。** DSCP 0 在标准里表示"尽力而为"
> （默认等级），打上去等于没打。
>
> **具体数值必须由网络方（运营商 / 机房 / 上游管理员）指定**——填错了上游不认识等于白做，
> 填成别人在用的值可能被误分策略、**反而更慢甚至被限速**。启用前请先确认该填多少。

> **新手请先读 [使用与调参指南](docs/usage.md)** —— 用大白话讲清每个开关和参数管什么、
> 调大调小分别会怎样，以及常见场景的现成配方。

## 仓库结构

```
skyline-speeder/
├── install.sh                    一键部署入口（Debian/Ubuntu）
├── scripts/bootstrap.sh          远程一键安装入口（curl | sudo bash）
├── DEPLOY.md                     面向自动化 agent 的部署手册
├── CONTRIBUTING.md               改动前必读：本项目的硬性不变量
├── CLAUDE.md                     AI 编码助手的仓库约定
├── Makefile                      make bpf / rust / check / test
├── bpf/
│   ├── skyline_cc.bpf.c          struct_ops 拥塞控制（每次 ACK 执行）
│   ├── skyline_policy.bpf.c      cgroup sockops：CC 选择 + 动态 RTO
│   ├── skyline_tc.bpf.c          TC egress：统计 + 重传 DSCP 标记
│   └── include/skyline_abi.h     BPF 与用户态共享的 ABI
├── crates/
│   ├── skyline-speederd/              常驻控制面
│   ├── ssctl/            命令行
│   └── skyline-common/           共享 ABI 类型与配置解析
├── config/
│   ├── speeder-guest.toml        生产环境模板
│   └── speeder.toml              本地开发模板
├── packaging/
│   ├── skyline-speederd.service
│   └── skyline-speeder-enable.service   开机挂载 skyline_cc
├── infra/                        安装、cgroup 工具、双 VM 测试床编排
├── research/experiments/         性能测试 manifest、执行与分析工具
└── docs/
    ├── usage.md                  新手向使用与调参指南
    ├── 01-deployment-guide.md    构建、安装、配置、验证、调参、回滚、排障
    ├── 02-interface-reference.md 命令、线协议、配置字段、事件码
    ├── 03-design.md              架构与各模块实现原理
    └── 04-performance-report.md  测试环境、结果、中性性、机制验证、局限性
```

## 从源码构建

```bash
sudo apt-get install -y build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

make bpf                          # 生成 vmlinux.h 并编译三个 CO-RE 对象
cargo build --workspace --release
make check                        # 格式、静态检查与单元测试
```

`make bpf` 默认从本机 `/sys/kernel/btf/vmlinux` 读取内核类型信息；交叉构建请用 `make VMLINUX_BTF=<路径> bpf` 显式指定目标内核的 BTF。

## 与替代方案的区别

|  | 原生 BBR | [tcp-brutal](https://github.com/HyNetworks/tcp-brutal) | Skyline Speeder |
|---|---|---|---|
| 控制结构 | 闭环，探测带宽 | **开环** —— 速率由你给定 | 闭环，自己估带宽 |
| 对丢包降窗 | 是，间接 | 否 | 否 |
| 需要预知链路带宽 | 否 | **是** | 否 |
| 拥塞自保护 | 有 | 无 | 排队时延 + ECN 护栏 |
| 形态 | 内核内置 | 内核模块（DKMS） | eBPF CO-RE（过验证器） |
| 内核门槛 | 任意 | 5.10 | **6.12 LTS** |
| 跨流速率共享 | 大致公平 | 有，通过 `group_id` | **无** |

如果你已经知道出口带宽、它基本固定、且内核较老，tcp-brutal 是更简单的答案。Skyline Speeder
适合带宽未知或会变、需要可观测性与拥塞护栏、且能跑 6.12+ 内核的场景。真正拥塞的瓶颈链路上，
两者都不合适。

### 与 tcp-brutal 的现场实测对比

上表是设计属性。下面是它们在一条真实链路上的实际表现，两者都只部署在服务端，通过同一套
按目的地前缀的路由覆盖生效：

| | 位置 | 角色 |
|---|---|---|
| **服务端** | **新加坡** KVM VPS · 10 Gbps | 发送方，唯一被加速的一端 |
| **客户端** | **阿里云** ECS，**华南（广东）** · 200 Mbps | 接收方，原生 TCP，不做任何改动 |

地址均已隐去。RTT 70–73 ms、丢包 10–30%，测于晚高峰（北京时间 22:55–23:37）。
来回路由不对称，这在大陆跨境线路上很常见：新加坡发出的数据经中国移动国际（AS58453）与
广东移动（AS9808）入境，ACK 则经中国电信（AS4134）与 NTT（AS2914）返回。

被测配置：skyline_cc 使用 0.1.0 时随附的默认系数——即现在保留在 [使用与调参指南](docs/usage.md)
里的「高随机丢包档」，也是上面网格测试所用的那一组。**当前的默认系数没有在这条链路上测过。**
tcp-brutal 为 v2.0.0，速率 200 Mbps，cwnd 增益取其默认值；bbr 为内核自带版本。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="以 bbr 为基线，tcp-brutal 与 skyline_cc 在游戏、视频、网页、大文件四类场景下的相对表现" src="docs/images/single-ended-ataglance-light.svg">
</picture>

8 轮 × 3 算法 × 5 场景 = 120 次运行，119 次完成，1 次 bbr 网页加载超时。每轮轮换算法顺序，避免链路在测试中劣化时
系统性偏袒先跑的那个。各场景赢家（按轮统计，跨轮漂移不影响排名）：

| 场景 | 赢家 | 幅度 |
|---|---|---|
| **视频**（6 MB 分块 @ 48 Mbps） | **tcp-brutal** | 启动延迟赢 7/8 轮，分块 p95 赢 6/8 轮 |
| **游戏**（ping-pong，60 msg/s） | **skyline_cc** | 抖动 5/8 轮，卡顿 >150 ms 4/8 轮 |
| **大文件 · 4 并发** | 打平，两者都胜过 bbr | 117 / 115 vs 77 Mbps |
| **大文件 · 单流** | bbr 与 skyline_cc 打平，brutal 垫底 | 4/8、4/8、0/8 轮；中位数 59 / 53 / 46 Mbps |
| **网页加载** | 无稳定赢家 | 逐轮波动大于三者差异 |

最值得记住的是单流那一条。**单流下 tcp-brutal 发得最多、传得最少：它发出的报文段有 13.9% 是重传，
bbr 为 9.9%，skyline_cc 为 11.1%，而它的中位吞吐只有 46 Mbps。** 在 10–30% 丢包下，为它配置的 200 Mbps 远高于链路实际能交付的速率，而 brutal
的设计——丢包了就多发以维持目标交付速率——在此变成纯粹的损耗放大。这不是 bug，是开环控制的
必然代价，它自己的文档也写明了：*"set it too high and you only produce loss."*
在同一条路径丢包只有 3–10% 的较早一轮（n=3，时间更短）里，同一个 200M 配置的单流中位数是三者
最高——117 Mbps，bbr 为 89，skyline_cc 为 95。**配置没变，链路变了，结果就反了**——而没有任何
机制提醒运维该重新调。

> [!NOTE]
> **就 RTT 而言，这轮测试低于本项目的目标区间，bbr 因此显得比网格测试中强得多。** 上面的网格在
> 100–300 ms RTT 下，丢包达到 15–20% 时原生 BBR 会跌到 3–31 Mbit/s；而这条路径的 70 ms RTT 让 BBR 从丢包中
> 恢复容易得多，且其丢包来自晚高峰的真实跨境运营商链路，而非 `netem` 注入的均匀随机丢包。
> 这一节应当作为算法**性格**的证据来读，而不是第二份吞吐量跑分。

逐轮数据、完整指标与原始记录见
[research/experiments/single-ended/](research/experiments/single-ended/)，
按场景的绝对值视图见
[docs/images/single-ended-detail-light.svg](docs/images/single-ended-detail-light.svg)。

## 文档

| 文档 | 内容 |
|---|---|
| **[docs/usage.md](docs/usage.md)** | **新手向：怎么用、模块开关、参数调整、DSCP 配置、常见配方** |
| [docs/01-deployment-guide.md](docs/01-deployment-guide.md) | 构建、安装、配置、启动验证、在线调参、摘除回滚、排障 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 命令、控制面线协议、配置字段、事件码 |
| [docs/03-design.md](docs/03-design.md) | 架构、各模块实现原理、控制面设计、系数选择依据 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 测试环境、目标环境结果、中性性、机制验证、内核适配性、局限性 |
| [research/experiments/README.md](research/experiments/README.md) | 复现性能测试的完整操作步骤 |
| [DEPLOY.md](DEPLOY.md) | 面向自动化 agent 的确定性部署手册 |

## 许可

版权所有 (c) 2026 **CYBERVERSE LLC**，以 **GPL-2.0** 分发，详见 [LICENSE](LICENSE)。

GPL-2.0 不是偏好选择：`skyline_cc.bpf.c` 中有若干段是对 GPL-2.0 Linux 内核算法的
高度贴近的重新实现（源码里逐处标了文件与行号），且三个 BPF 对象都向内核声明了
`GPL`。完整理由与上游归属见 [NOTICE](NOTICE)。

"Skyline Speeder"、"CYBERVERSE" 是 CYBERVERSE LLC 的商标；GPL 对代码的授权不包含
任何商标使用权。

---

<div align="center">

**Skyline Connect** 独家冠名

</div>
