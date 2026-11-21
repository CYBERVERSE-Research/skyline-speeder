<div align="center">

# Skyline Speeder

**基於 eBPF 的傳送端 TCP 加速，專為高丟包、高延遲鏈路設計**

*只部署在傳送資料的伺服器上。用戶端維持原生 TCP，不需安裝軟體、修補或任何設定。*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#系統需求)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · **繁體中文** · [日本語](README.ja.md) · [한국어](README.ko.md) · [Русский](README.ru.md) · [Français](README.fr.md)

---

### 由 **[Skyline Connect](https://www.skylineconnect.io)** 獨家贊助

---

</div>

## 這是什麼

Skyline Speeder 是一套單邊 TCP 加速軟體。它以 eBPF 程式的形式執行在傳送資料的伺服器上，針對丟包嚴重、往返時間長、標準 TCP 會崩潰的鏈路。接收端完全不需更動。

它把伺服器的壅塞控制換成 `skyline_cc`，核心只有一個假設：**丟包本身不帶有壅塞資訊。**

- **先量測，再傳送：** 視窗跟隨實測頻寬 × 基準 RTT，丟包從不讓它縮小。
- **補償並平滑傳送：** 依實測丟包率放大傳送速率（有上限），經由 `fq` 均勻送出。
- **只有一道煞車：** 只有排隊延遲上升與 ECN 標記才會讓它降速。
- **短回應優先：** 每條連線的交握與前 64 KiB 間隔 10 ms 送兩次，丟一個封包只多花幾毫秒，而不必等重傳計時器。

> [!IMPORTANT]
> 它只在丟包與佇列無關時有效。如果固定速率的 UDP 流量沒有丟包、TCP 卻大量重傳，表示丟包來自壅塞瓶頸的佇列溢位，這時它不是合適的工具。

## 系統需求

- **Linux 6.12 LTS 或更新版本**，且具備 `/sys/kernel/btf/vmlinux`。6.1、6.6 LTS 分支以及 Debian 12、Ubuntu 22.04/24.04 內建的核心都太舊。
- 實體主機或 KVM 虛擬機，不能是容器（LXC、Proxmox CT）。
- Debian、Ubuntu、Fedora、RHEL 系列（Rocky Linux、AlmaLinux、CentOS Stream）或 Alpine。

## 快速開始

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

在 Alpine 上以 root 執行：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

它會安裝最新發佈的版本，不在主機上安裝編譯器；把每個 BPF 物件交給核心驗證器檢查，然後讓每條新的 TCP 連線都使用 `skyline_cc`，重新開機後依然有效。它不安裝代理程式，也不開放任何連接埠。安裝後檢查：

```bash
ssctl status    # 本機是否已被接管？顯示 ACCELERATING 即是
ssctl flows     # 哪些連線正在加速，演算法做了哪些決定
```

解除安裝（主機切回 bbr + fq）：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 進階使用

**從複製的儲存庫安裝**，可以先閱讀腳本再執行：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 只做預檢，不更動任何東西
sudo ./install.sh              # 發佈版本（無法執行時才從原始碼建置）
sudo ./install.sh --source     # 從這份原始碼建置
sudo ./install.sh --release v0.6.0   # 指定的發佈版本，裝不上就停止
sudo ./install.sh --uninstall --restore-pre-install   # 解除安裝，並還原安裝前的壅塞控制與 qdisc
```

升級時再執行一次安裝腳本即可。它會排空現有連線（最多 60 秒），然後重新掛載 `skyline_cc`。

**日常操作：**

```bash
sudo ssctl drain                                # 平順卸載：新連線改用 fallback_cc
sudo ssctl enable                               # 重新掛載
sudo ssctl enable --all-off                     # 關閉所有模組的 skyline_cc：作為比較基準
sudo ssctl set-redundancy --disable             # 不再重複傳送首輪資料，適合依流量計費的出口
sudo ssctl set-module-config --max-pacing-mbps 500   # 絕對覆寫：沒寫的參數都回到預設值
```

出廠預設值都已調校過，大多數部署什麼都不用改。`ssctl set-*` 的修改在常駐程式重新啟動前有效；要長期保留，請寫進 `/etc/skyline-speeder/speeder.toml`。

**文件**（英文）：

| 文件 | 內容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | 安裝腳本做了什麼、運作原理、每項功能的預設值、代價與開關、`ssctl status` 與 `ssctl flows` 的逐行說明、從原始碼建置 |
| [docs/usage.md](docs/usage.md) | 每個開關與參數及其範圍、四組預設設定、疑難排解 |
| [DEPLOY.md](DEPLOY.md) | 部署與維運手冊：安裝、驗證、升級、解除安裝 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 指令、通訊協定、設定欄位、事件代碼 |
| [docs/03-design.md](docs/03-design.md) | 架構與各模組的運作方式 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 受控雙 VM 測試平台上與 bbr 的比較 |

## 實測效能

bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal) 與 skyline_cc 都只裝在伺服器端，跑在同一條真實的跨境鏈路上：新加坡的 KVM VPS 向華南（廣東）的阿里雲 ECS（200 Mbps，原生 TCP）傳送。RTT 70-73 ms，丟包 10-30%，晚間尖峰時段實測；8 輪 × 3 種演算法 × 5 個情境。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal 與 skyline_cc 在遊戲、影片、網頁與大檔案流量下相對於 bbr 基準的實測結果" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| 情境 | 勝出者 | 差距 |
|---|---|---|
| **影片**（6 MB 區塊 @ 48 Mbps） | **tcp-brutal** | 起播 7/8 輪，區塊 p95 6/8 輪 |
| **遊戲**（ping-pong，60 msg/s） | **skyline_cc** | 抖動 5/8 輪，>150 ms 卡頓 4/8 輪 |
| **大檔案，4 條串流** | skyline_cc 與 tcp-brutal，皆領先 bbr | 中位數 117 / 115 vs 77 Mbps |
| **大檔案，1 條串流** | bbr 與 skyline_cc 平手；tcp-brutal 最後 | 中位數 59 / 53 / 46 Mbps |
| **網頁載入** | 沒有穩定的勝出者 | 每輪之間的波動大於差距 |
| **伺服器重傳**，占所有送出區段的比例 | bbr 最低 | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

這是單一鏈路上的實地量測，不代表你的鏈路：skyline_cc 使用的是 0.1.0 的預設係數，也就是現在的 "high random loss" 預設設定。方法、條件與完整結果見 [docs/01-features.md](docs/01-features.md#measured-performance-in-detail)。

## 授權

GPL-2.0，見 [LICENSE](LICENSE) 與 [NOTICE](NOTICE)；版權所有 (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" 與 "CYBERVERSE" 是 CYBERVERSE LLC 的商標。參與貢獻前請先閱讀 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

<div align="center">

由 **[Skyline Connect](https://www.skylineconnect.io)** 獨家贊助

</div>
