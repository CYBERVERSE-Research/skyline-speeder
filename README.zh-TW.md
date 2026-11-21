<div align="center">

# Skyline Speeder

**基於 eBPF 的傳送端 TCP 加速，專為高丟包、高延遲鏈路設計**

*只需部署在傳送資料的伺服器上。用戶端照常使用系統內建的 TCP，不必安裝軟體、不必上修補，也不必改設定。*

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

Skyline Speeder 是一套單邊 TCP 加速軟體，以 eBPF 程式的形式在傳送端伺服器上執行，專為丟包嚴重、往返時間長的鏈路而設計——在這類鏈路上，標準 TCP 的速度往往一落千丈。接收端完全不用動。

它把伺服器的壅塞控制演算法換成 `skyline_cc`，整套設計建立在一個假設上：**丟包本身說明不了壅塞。**

- **先量測，再傳送：** 壅塞視窗依實測頻寬 × 基準 RTT 設定，丟包不會讓它縮小。
- **補償丟包，等速傳送：** 依實測丟包率適度提高傳送速率（有上限），再經 `fq` 均勻送出。
- **只踩一種煞車：** 只有排隊延遲上升或收到 ECN 標記時才降速。
- **先顧短回應：** 每條連線的交握與前 64 KiB 資料間隔 10 ms 各送兩份，丟一個封包只多花幾毫秒，不必乾等重傳計時器。

> [!IMPORTANT]
> 只有丟包與排隊無關時它才有用。如果固定速率的 UDP 流量幾乎沒有丟包，TCP 卻大量重傳，代表丟包是壅塞瓶頸處佇列溢位造成的，這種情況請不要使用。

## 系統需求

- **Linux 6.12 LTS 以上**，且存在 `/sys/kernel/btf/vmlinux`。6.1、6.6 LTS 以及 Debian 12、Ubuntu 22.04/24.04 內建的核心都太舊，無法使用。
- 實體主機或 KVM 虛擬機皆可，容器（LXC、Proxmox CT）不行。
- Debian、Ubuntu、Fedora、RHEL 系列（Rocky Linux、AlmaLinux、CentOS Stream）或 Alpine。

## 快速開始

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine 請以 root 執行：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

腳本會安裝最新的正式版（不會在主機上安裝編譯器），先用核心驗證器檢查每個 BPF 物件，再把 `skyline_cc` 設為所有新建 TCP 連線的壅塞控制，重新開機後依然有效。它不裝代理程式，也不開任何連接埠。裝好後確認一下：

```bash
ssctl status    # 本機是否已接管？顯示 ACCELERATING 就對了
ssctl flows     # 哪些連線在加速，演算法怎麼調整
```

解除安裝（之後主機改用 bbr + fq）：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 進階使用

**想先看過腳本再執行**，可以 clone 儲存庫後安裝：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 只做預檢，不更動系統
sudo ./install.sh              # 安裝正式版（本機無法執行時才從原始碼建置）
sudo ./install.sh --source     # 用目前的原始碼建置
sudo ./install.sh --release v0.6.0   # 安裝指定的正式版，裝不了就中止
sudo ./install.sh --uninstall --restore-pre-install   # 解除安裝，並還原安裝前的壅塞控制與 qdisc
```

升級只要再跑一次安裝腳本：它會先讓現有連線排空（最多等 60 秒），再重新掛載 `skyline_cc`。

**日常操作：**

```bash
sudo ssctl drain                                # 平順停用：新連線改走 fallback_cc
sudo ssctl enable                               # 重新掛載
sudo ssctl enable --all-off                     # 所有模組都關閉的 skyline_cc，當作比較基準
sudo ssctl set-redundancy --disable             # 關閉首段資料雙送，出口依流量計費時建議關閉
sudo ssctl set-module-config --max-pacing-mbps 500   # 整體覆寫：沒寫到的參數一律恢復預設
```

預設參數已經調校好，絕大多數情況不需要改。`ssctl set-*` 的修改在常駐程式重新啟動後就會失效；想長期保留，請寫進 `/etc/skyline-speeder/speeder.toml`。

**文件**（英文）：

| 文件 | 內容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | 安裝腳本實際做了什麼、運作原理、各項功能的預設值、代價與開關、`ssctl status` 與 `ssctl flows` 逐行說明、從原始碼建置 |
| [docs/usage.md](docs/usage.md) | 所有開關與參數的範圍、四組預設組合、疑難排解 |
| [DEPLOY.md](DEPLOY.md) | 部署與維運手冊：安裝、驗證、升級、解除安裝 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 指令、通訊協定、設定欄位、事件代碼 |
| [docs/03-design.md](docs/03-design.md) | 整體架構與各模組原理 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 在受控的雙 VM 測試平台上與 bbr 比較 |

## 實測效能

在同一條真實的跨境線路上比較 bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal) 與 skyline_cc，三者都只部署在伺服器端：新加坡的 KVM VPS 向華南的阿里雲 ECS（200 Mbps，系統內建 TCP）傳資料。RTT 70-73 ms，丟包率 10-30%，晚間尖峰時段實測；共 8 輪 × 3 種演算法 × 5 個情境。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="tcp-brutal 與 skyline_cc 在遊戲、影片、網頁與大檔案流量下相對於 bbr 基準的實測結果" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| 情境 | 勝出者 | 差距 |
|---|---|---|
| **影片**（6 MB 區塊 @ 48 Mbps） | **tcp-brutal** | 起播 7/8 輪，區塊 p95 6/8 輪 |
| **遊戲**（ping-pong，60 msg/s） | **skyline_cc** | 抖動 5/8 輪，>150 ms 卡頓 4/8 輪 |
| **大檔案，4 條串流** | skyline_cc 與 tcp-brutal，都領先 bbr | 中位數 117 / 115 vs 77 Mbps |
| **大檔案，1 條串流** | bbr 與 skyline_cc 平手；tcp-brutal 最後 | 中位數 59 / 53 / 46 Mbps |
| **網頁載入** | 沒有穩定的勝出者 | 各輪之間的波動比差距還大 |
| **伺服器重傳**，占所有送出區段的比例 | bbr 最低 | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

這只是單一線路上的實測，不代表你的線路也會如此：skyline_cc 用的是 0.1.0 的預設係數，也就是現在的 "high random loss" 預設組合。測試方法、條件與完整結果見 [docs/01-features.md](docs/01-features.md#measured-performance-in-detail)。

## 授權

GPL-2.0，見 [LICENSE](LICENSE) 與 [NOTICE](NOTICE)；版權所有 (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" 與 "CYBERVERSE" 是 CYBERVERSE LLC 的商標。想參與貢獻，請先閱讀 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

<div align="center">

由 **[Skyline Connect](https://www.skylineconnect.io)** 獨家贊助

</div>
