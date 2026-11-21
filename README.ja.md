<div align="center">

# Skyline Speeder

**高パケットロス・高遅延リンク向けの、eBPF による送信側 TCP 高速化**

*送信側のサーバーにだけ導入します。クライアントは標準の TCP のままで、ソフトウェアもパッチも設定も要りません。*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#動作要件)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · **日本語** · [한국어](README.ko.md) · [Русский](README.ru.md) · [Français](README.fr.md)

---

### **[Skyline Connect](https://www.skylineconnect.io)** 独占スポンサー

---

</div>

## これは何か

Skyline Speeder は片側だけで動く TCP 高速化ソフトウェアです。データを送信するサーバー上で eBPF プログラムとして動作し、パケットロスが多く往復遅延の長い、標準の TCP では速度が崩れてしまうリンクを対象にしています。受信側には一切手を加えません。

サーバーの輻輳制御を `skyline_cc` に置き換えます。前提はひとつだけです：**パケットロスそのものは輻輳の情報を持たない。**

- **測ってから送る：** ウィンドウは実測帯域 × 基準 RTT に従い、ロスで縮むことはありません。
- **補償してペーシング：** 送信レートを実測ロス率に応じて（上限付きで）引き上げ、`fq` を通して均等な間隔で送り出します。
- **ブレーキはひとつ：** 減速するのはキューイング遅延の増加と ECN マークのときだけです。
- **短い応答を優先：** 各接続のハンドシェイクと最初の 64 KiB を 10 ms 間隔で 2 回送るため、1 パケットのロスは再送タイマーではなく数ミリ秒で済みます。

> [!IMPORTANT]
> 効果があるのは、ロスがキューと無関係な場合だけです。固定レートの UDP ではロスがないのに TCP が大量に再送しているなら、ロスの原因は混雑したボトルネックでのキューあふれであり、このツールの出番ではありません。

## 動作要件

- **Linux 6.12 LTS 以降**で、`/sys/kernel/btf/vmlinux` があること。6.1・6.6 LTS 系列や、Debian 12・Ubuntu 22.04/24.04 の標準カーネルは古すぎます。
- 物理ホストまたは KVM 仮想マシン。コンテナ（LXC、Proxmox CT）では動きません。
- Debian、Ubuntu、Fedora、RHEL 系（Rocky Linux、AlmaLinux、CentOS Stream）または Alpine。

## クイックスタート

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine では root で：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

最新の公開リリースをインストールし（ホストにコンパイラは入れません）、すべての BPF オブジェクトをカーネルの検証器に通したうえで、新しい TCP 接続すべてに `skyline_cc` を使わせ、再起動後もその状態を保ちます。プロキシは入れず、ポートも開けません。インストール後の確認：

```bash
ssctl status    # このホストは切り替わったか？ ACCELERATING なら OK
ssctl flows     # どの接続が高速化され、アルゴリズムが何を決めたか
```

アンインストール（ホストは bbr + fq に戻ります）：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 応用的な使い方

**クローンしたリポジトリからインストール**すれば、実行前にスクリプトを読めます：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 事前チェックのみ、何も変更しない
sudo ./install.sh              # 公開リリース（動かせない場合のみソースからビルド）
sudo ./install.sh --source     # このソースツリーからビルド
sudo ./install.sh --release v0.6.0   # 指定した公開リリース、無理なら中止
sudo ./install.sh --uninstall --restore-pre-install   # アンインストールし、導入前の輻輳制御と qdisc に戻す
```

アップグレードはインストーラーをもう一度実行するだけです。既存の接続を最大 60 秒かけて排出し、`skyline_cc` を再びアタッチします。

**日常の操作：**

```bash
sudo ssctl drain                                # 穏やかに切り離す：新しい接続は fallback_cc へ
sudo ssctl enable                               # 再びアタッチ
sudo ssctl enable --all-off                     # 全モジュールを止めた skyline_cc：比較用のベースライン
sudo ssctl set-redundancy --disable             # 最初のデータの二重送信を止める（従量課金の回線向け）
sudo ssctl set-module-config --max-pacing-mbps 500   # 全体上書き：省いたフラグは既定値に戻る
```

出荷時の既定値は調整済みで、ほとんどの環境では何も変える必要はありません。`ssctl set-*` の変更はデーモンの再起動まで有効です。残したい場合は `/etc/skyline-speeder/speeder.toml` に書きます。

**ドキュメント**（英語）：

| ドキュメント | 内容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | インストーラーの動作、仕組み、各機能の既定値・コスト・切り替え方、`ssctl status` と `ssctl flows` の各行の意味、ソースからのビルド |
| [docs/usage.md](docs/usage.md) | すべてのスイッチとパラメーターとその範囲、4 つのプリセット、トラブルシューティング |
| [DEPLOY.md](DEPLOY.md) | デプロイと運用のマニュアル：インストール、検証、アップグレード、削除 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` コマンド、通信プロトコル、設定項目、イベントコード |
| [docs/03-design.md](docs/03-design.md) | アーキテクチャと各モジュールの仕組み |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 管理された 2 台構成の VM テストベッドでの bbr との比較 |

## 実測性能

bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal)、skyline_cc をいずれもサーバー側だけに適用し、同じ実在の国際リンクで比較しました：シンガポールの KVM VPS から、華南（広東）の Alibaba Cloud ECS（200 Mbps、標準 TCP）へ送信。RTT 70-73 ms、ロス率 10-30%、夜間ピーク時に測定。8 ローテーション × 3 アルゴリズム × 5 シナリオ。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="ゲーム、動画、ウェブ、大容量転送での tcp-brutal と skyline_cc の bbr 比の実測結果" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| シナリオ | 勝者 | 差 |
|---|---|---|
| **動画**（6 MB チャンク @ 48 Mbps） | **tcp-brutal** | 再生開始 7/8 ローテーション、チャンク p95 6/8 |
| **ゲーム**（ping-pong、60 msg/s） | **skyline_cc** | ジッター 5/8、150 ms 超の停止 4/8 |
| **大容量、4 ストリーム** | skyline_cc と tcp-brutal、ともに bbr を上回る | 中央値 117 / 115 vs 77 Mbps |
| **大容量、1 ストリーム** | bbr と skyline_cc が互角、tcp-brutal が最下位 | 中央値 59 / 53 / 46 Mbps |
| **ウェブページ読み込み** | 安定した勝者なし | ローテーション間のばらつきが差より大きい |
| **サーバーの再送**、全送信セグメントに占める割合 | bbr が最小 | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

ひとつのリンクでの実地測定であり、あなたのリンクでの結果を保証するものではありません。skyline_cc は 0.1.0 の既定係数、現在の "high random loss" プリセットで動かしました。方法、条件、全結果は [docs/01-features.md](docs/01-features.md#measured-performance-in-detail) にあります。

## ライセンス

GPL-2.0。[LICENSE](LICENSE) と [NOTICE](NOTICE) を参照してください。Copyright (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" と "CYBERVERSE" は CYBERVERSE LLC の商標です。貢献する前に [CONTRIBUTING.md](CONTRIBUTING.md) をお読みください。

---

<div align="center">

**[Skyline Connect](https://www.skylineconnect.io)** 独占スポンサー

</div>
