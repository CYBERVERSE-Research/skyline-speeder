<div align="center">

# Skyline Speeder

**基于 eBPF 的发送端 TCP 加速，专为高丢包、高延迟链路设计**

*只需部署在发送数据的服务器上。客户端照常使用系统自带的 TCP，不用装软件、不用打补丁，也不用改配置。*

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

Skyline Speeder 是一款单边 TCP 加速软件，以 eBPF 程序的形式运行在发送端服务器上，专门针对丢包严重、往返时延长的链路——在这类链路上，标准 TCP 的速度往往一落千丈。接收端什么都不用动。

它把服务器的拥塞控制算法换成 `skyline_cc`，整套设计建立在一个假设上：**丢包本身说明不了拥塞。**

- **先测量，再发送：** 拥塞窗口按实测带宽 × 基准 RTT 设定，丢包不会让它缩小。
- **补偿丢包，匀速发送：** 按实测丢包率适当提高发送速率（有上限），再经 `fq` 均匀发出。
- **只踩一种刹车：** 只有排队时延上升或收到 ECN 标记时才降速。
- **先保短响应：** 每条连接的握手和前 64 KiB 数据间隔 10 ms 各发两份，丢一个包只多耗几毫秒，不必干等重传定时器。

> [!IMPORTANT]
> 只有丢包与排队无关时它才管用。如果固定速率的 UDP 流几乎不丢包，TCP 却在大量重传，说明丢包是拥塞瓶颈处队列溢出造成的，这种情况请不要用它。

## 运行要求

- **Linux 6.12 LTS 及以上**，且存在 `/sys/kernel/btf/vmlinux`。6.1、6.6 LTS 以及 Debian 12、Ubuntu 22.04/24.04 自带的内核都太旧，用不了。
- 物理机或 KVM 虚拟机均可，容器（LXC、Proxmox CT）不行。
- Debian、Ubuntu、Fedora、RHEL 系（Rocky Linux、AlmaLinux、CentOS Stream）或 Alpine。

## 快速开始

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine 请用 root 执行：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

脚本会安装最新的发布版（不会往主机上装编译器），先用内核验证器检查每个 BPF 对象，再把 `skyline_cc` 设为所有新建 TCP 连接的拥塞控制，重启后依然有效。它不装代理，也不开端口。装好后看一眼：

```bash
ssctl status    # 本机是否已接管？显示 ACCELERATING 就对了
ssctl flows     # 哪些连接在加速，算法是怎么调的
```

卸载（卸载后主机改用 bbr + fq）：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 进阶使用

**想先看过脚本再运行**，可以克隆仓库后安装：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 只做预检，不改动系统
sudo ./install.sh              # 安装发布版（本机跑不了时才从源码构建）
sudo ./install.sh --source     # 用当前源码构建
sudo ./install.sh --release v0.6.0   # 安装指定的发布版，装不了就中止
sudo ./install.sh --uninstall --restore-pre-install   # 卸载，并恢复安装前的拥塞控制和 qdisc
```

升级只需再跑一次安装脚本：它会先让现有连接排空（最多等 60 秒），再重新挂载 `skyline_cc`。

**日常操作：**

```bash
sudo ssctl drain                                # 平滑停用：新连接改走 fallback_cc
sudo ssctl enable                               # 重新挂载
sudo ssctl enable --all-off                     # 所有模块都关掉的 skyline_cc，做对比基线用
sudo ssctl set-redundancy --disable             # 关闭首段数据双发，出口按流量计费时建议关
sudo ssctl set-module-config --max-pacing-mbps 500   # 整体覆盖：没写到的参数一律恢复默认
```

默认参数已经调好，绝大多数场景无需改动。`ssctl set-*` 的改动在守护进程重启后失效；想长期保留，请写进 `/etc/skyline-speeder/speeder.toml`。

**文档**（英文）：

| 文档 | 内容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | 安装脚本具体做了什么、工作原理、各功能的默认值、代价与开关、`ssctl status` 和 `ssctl flows` 逐行说明、从源码构建 |
| [docs/usage.md](docs/usage.md) | 所有开关和参数的取值范围、四套预设、故障排查 |
| [DEPLOY.md](DEPLOY.md) | 部署与运维手册：安装、验证、升级、卸载 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 命令、通信协议、配置项、事件码 |
| [docs/03-design.md](docs/03-design.md) | 整体架构与各模块原理 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 在受控的双 VM 测试床上与 bbr 对比 |

## 实测性能

在同一条真实的跨境线路上对比 bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal) 和 skyline_cc，三者都只部署在服务器端：新加坡的 KVM VPS 向华南的阿里云 ECS（200 Mbps，系统自带 TCP）发数据。RTT 70-73 ms，丢包率 10-30%，晚高峰实测；共 8 轮 × 3 种算法 × 5 个场景。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal 与 skyline_cc 在游戏、视频、网页和大文件流量下相对 bbr 基线的实测结果" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| 场景 | 胜者 | 差距 |
|---|---|---|
| **视频**（6 MB 分片 @ 48 Mbps） | **tcp-brutal** | 起播 7/8 轮，分片 p95 6/8 轮 |
| **游戏**（ping-pong，60 msg/s） | **skyline_cc** | 抖动 5/8 轮，>150 ms 卡顿 4/8 轮 |
| **大文件，4 条流** | skyline_cc 与 tcp-brutal，均领先 bbr | 中位数 117 / 115 vs 77 Mbps |
| **大文件，1 条流** | bbr 与 skyline_cc 打平；tcp-brutal 垫底 | 中位数 59 / 53 / 46 Mbps |
| **网页加载** | 没有稳定的胜者 | 各轮之间的波动比差距还大 |
| **服务器重传**，占全部发送分段的比例 | bbr 最低 | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

这只是一条线路上的实测，不代表你的线路也会如此：skyline_cc 用的是 0.1.0 的默认系数，也就是现在的 "high random loss" 预设。测试方法、条件和完整结果见 [docs/01-features.md](docs/01-features.md#measured-performance-in-detail)。

## 许可

GPL-2.0，见 [LICENSE](LICENSE) 与 [NOTICE](NOTICE)；版权所有 (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" 和 "CYBERVERSE" 是 CYBERVERSE LLC 的商标。想参与贡献，请先阅读 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

<div align="center">

由 **[Skyline Connect](https://www.skylineconnect.io)** 独家赞助

</div>
