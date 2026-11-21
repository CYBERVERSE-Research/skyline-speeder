<div align="center">

# Skyline Speeder

**基于 eBPF 的发送端 TCP 加速，专为高丢包、高延迟链路设计**

*只部署在发送数据的服务器上。客户端保持原生 TCP，无需安装软件、打补丁或做任何配置。*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#运行要求)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · **简体中文** · [繁體中文](README.zh-TW.md) · [日本語](README.ja.md) · [한국어](README.ko.md) · [Русский](README.ru.md) · [Français](README.fr.md)

---

### 由 **[Skyline Connect](https://www.skylineconnect.io)** 独家赞助

---

</div>

## 这是什么

Skyline Speeder 是一款单边 TCP 加速软件。它以 eBPF 程序的形式运行在发送数据的服务器上，面向丢包严重、往返时延长、标准 TCP 会崩溃的链路。接收端无需任何改动。

它把服务器的拥塞控制换成 `skyline_cc`，核心只有一个假设：**丢包本身不携带拥塞信息。**

- **先测量，再发送：** 窗口跟随实测带宽 × 基准 RTT，丢包从不让它缩小。
- **补偿并平滑发送：** 按实测丢包率放大发送速率（有上限），经 `fq` 均匀发出。
- **只有一道刹车：** 只有排队时延上升和 ECN 标记才会让它降速。
- **短响应优先：** 每条连接的握手和前 64 KiB 间隔 10 ms 发两遍，丢一个包只多花几毫秒，而不是等重传定时器。

> [!IMPORTANT]
> 它只在丢包与队列无关时有用。如果固定速率的 UDP 流不丢包、TCP 却大量重传，说明丢包来自拥塞瓶颈处的队列溢出，这时它不是合适的工具。

## 运行要求

- **Linux 6.12 LTS 或更新**，并且有 `/sys/kernel/btf/vmlinux`。6.1、6.6 LTS 分支以及 Debian 12、Ubuntu 22.04/24.04 的自带内核都太旧。
- 物理机或 KVM 虚拟机，不能是容器（LXC、Proxmox CT）。
- Debian、Ubuntu、Fedora、RHEL 系（Rocky Linux、AlmaLinux、CentOS Stream）或 Alpine。

## 快速开始

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine 上以 root 运行：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

它安装最新发布的版本，不在主机上装编译器；把每个 BPF 对象交给内核验证器检查，然后让每条新 TCP 连接都使用 `skyline_cc`，重启后依然生效。它不安装代理，也不开放端口。装完检查：

```bash
ssctl status    # 本机是否已被接管？显示 ACCELERATING 即是
ssctl flows     # 哪些连接正在加速，算法做了哪些决定
```

卸载（主机切回 bbr + fq）：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 进阶使用

**从克隆的仓库安装**，可以先读脚本再运行：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 只做预检，不改任何东西
sudo ./install.sh              # 发布版本（无法运行时才从源码构建）
sudo ./install.sh --source     # 从这份源码构建
sudo ./install.sh --release v0.6.0   # 指定的发布版本，装不上就停止
sudo ./install.sh --uninstall --restore-pre-install   # 卸载，并恢复安装前的拥塞控制和 qdisc
```

升级时再运行一次安装脚本即可。它会排空现有连接（最多 60 秒），然后重新挂载 `skyline_cc`。

**日常操作：**

```bash
sudo ssctl drain                                # 平滑卸载：新连接改用 fallback_cc
sudo ssctl enable                               # 重新挂载
sudo ssctl enable --all-off                     # 关闭全部模块的 skyline_cc：用作对比基线
sudo ssctl set-redundancy --disable             # 不再重复发送首轮数据，适合按流量计费的出口
sudo ssctl set-module-config --max-pacing-mbps 500   # 绝对覆盖：没写的参数都回到默认值
```

出厂默认值是调好的，大多数部署什么都不用改。`ssctl set-*` 的修改在守护进程重启前有效；要长期保留，写进 `/etc/skyline-speeder/speeder.toml`。

**文档**（英文）：

| 文档 | 内容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | 安装脚本做了什么、工作原理、每个功能的默认值、代价与开关、`ssctl status` 和 `ssctl flows` 的逐行解释、从源码构建 |
| [docs/usage.md](docs/usage.md) | 每个开关与参数及其取值范围、四套预设、排障 |
| [DEPLOY.md](DEPLOY.md) | 部署与运维手册：安装、验证、升级、卸载 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 命令、通信协议、配置字段、事件码 |
| [docs/03-design.md](docs/03-design.md) | 架构与各模块的工作方式 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 受控双 VM 测试床上与 bbr 的对比 |

## 实测性能

bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal) 和 skyline_cc 都只装在服务器端，跑在同一条真实的跨境链路上：新加坡的 KVM VPS 向华南（广东）的阿里云 ECS（200 Mbps，原生 TCP）发送。RTT 70-73 ms，丢包 10-30%，晚高峰实测；8 轮 × 3 种算法 × 5 个场景。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal 与 skyline_cc 在游戏、视频、网页和大文件流量下相对 bbr 基线的实测结果" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| 场景 | 胜者 | 差距 |
|---|---|---|
| **视频**（6 MB 分片 @ 48 Mbps） | **tcp-brutal** | 起播 7/8 轮，分片 p95 6/8 轮 |
| **游戏**（ping-pong，60 msg/s） | **skyline_cc** | 抖动 5/8 轮，>150 ms 卡顿 4/8 轮 |
| **大文件，4 条流** | skyline_cc 与 tcp-brutal，都领先 bbr | 中位数 117 / 115 vs 77 Mbps |
| **大文件，1 条流** | bbr 与 skyline_cc 打平；tcp-brutal 垫底 | 中位数 59 / 53 / 46 Mbps |
| **网页加载** | 没有稳定的胜者 | 轮与轮之间的波动大于差距 |
| **服务器重传**，占全部发送分段的比例 | bbr 最低 | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

这是单条链路上的实地测量，不代表你的链路：skyline_cc 用的是 0.1.0 的默认系数，即现在的 "high random loss" 预设。方法、条件与完整结果见 [docs/01-features.md](docs/01-features.md#measured-performance-in-detail)。

## 许可

GPL-2.0，见 [LICENSE](LICENSE) 与 [NOTICE](NOTICE)；版权所有 (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" 和 "CYBERVERSE" 是 CYBERVERSE LLC 的商标。参与贡献前请先读 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

<div align="center">

由 **[Skyline Connect](https://www.skylineconnect.io)** 独家赞助

</div>
