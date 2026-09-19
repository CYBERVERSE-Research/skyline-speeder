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

## 内核版本要求

**精确 ABI 下限是 6.10；实际支持并验证的下限是 6.12 LTS。**

`skyline_cc.bpf.c` 挂在 `tcp_congestion_ops.cong_control` 上的回调用 **4 个参数**声明（`sk, ack, flag, rs`）。该签名自 v6.10 才启用——v6.9 及更早版本该函数指针只有 2 个参数（`sk, rs`），BPF 验证器会在加载时直接拒绝，报错明确指向参数个数不匹配。

> **`6.1.x` 和 `6.6.x` 两个 LTS 分支不支持**，两者都停留在旧的 2 参数签名。

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
sudo ./install.sh --check      # 只做前置检查
sudo ./install.sh --no-enable  # 安装但不挂载 skyline_cc
sudo ./install.sh --uninstall  # 卸载（保留 /etc/skyline-speeder 配置）
```

安装器会在改动任何东西之前记录当前的拥塞控制算法与默认 qdisc，`--uninstall`
会把它们还原。**装之前跑 BBR 的机器，卸载后回到 BBR**，而不是停在 `fallback_cc`。

> [!NOTE]
> BPF 对象必须在**目标机器本身**编译（或显式指定同版本内核的 BTF），不能搬运在别的内核版本上编译好的 `.bpf.o`。

日常操作：

```bash
ssctl status    # 运行时状态、能力集与决策计数器
ssctl flows     # 逐连接视图
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

**2. 十六个运行时参数**（在线生效，无需重启）

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
