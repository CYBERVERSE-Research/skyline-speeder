<div align="center">

# Skyline Speeder

**高ロス・高遅延の回線向け、eBPF ベースの送信側 TCP 高速化**

*導入するのは送信側のサーバーだけです。クライアント側は標準の TCP のままでよく、ソフトウェアのインストールもパッチも設定も不要です。*

[![License](https://img.shields.io/badge/license-GPL--2.0-blue.svg)](LICENSE)
[![Kernel](https://img.shields.io/badge/kernel-6.12%20LTS%2B-orange.svg)](#動作要件)
[![Platform](https://img.shields.io/badge/platform-Debian%20%7C%20Ubuntu%20%7C%20Fedora%20%7C%20RHEL%20%7C%20Alpine-red.svg)](install.sh)
[![eBPF](https://img.shields.io/badge/eBPF-CO--RE%20struct__ops-green.svg)](bpf/)
[![Rust](https://img.shields.io/badge/rust-1.82%2B-black.svg)](rust-toolchain.toml)

[English](README.md) · [简体中文](README.zh.md) · [繁體中文](README.zh-TW.md) · **日本語** · [한국어](README.ko.md) · [Русский](README.ru.md) · [Français](README.fr.md)

---

### 単独スポンサー：**[Skyline Connect](https://www.skylineconnect.io)**

---

</div>

## 概要

Skyline Speeder は、片側に導入するだけで使える TCP 高速化ソフトウェアです。データを送信するサーバー上で eBPF プログラムとして動作し、パケットロスが多く RTT の長い、通常の TCP ではまともに速度が出なくなるような回線を対象としています。受信側には一切手を加えません。

サーバーの輻輳制御アルゴリズムを `skyline_cc` に置き換えます。`skyline_cc` の設計の前提はただひとつ、**パケットロスそれ自体は輻輳について何の情報ももたらさない**、というものです。

- **測ってから送る：** ウィンドウは実測した帯域幅 × ベース RTT で決まり、パケットが失われても縮小しません。
- **ロスを補ってペーシング：** 実測したロス率に応じて送信レートを（上限まで）引き上げ、`fq` で等間隔に送出します。
- **ブレーキはひとつだけ：** 減速のきっかけになるのは、キューイング遅延の増加と ECN マークだけです。
- **短い応答を優先：** 各接続のハンドシェイクと最初の 64 KiB を、10 ms ずらして 2 回送信します。そのため 1 パケット失われても、再送タイマーを待たずに数ミリ秒の遅れで済みます。

> [!IMPORTANT]
> 効果があるのは、ロスがキューとは無関係に発生している場合に限られます。固定レートの UDP フローではロスが出ないのに TCP だけが大量に再送しているなら、そのロスは輻輳したボトルネックでのキューあふれが原因であり、本ツールの出番ではありません。

## 動作要件

- **Linux 6.12 LTS 以降**で、`/sys/kernel/btf/vmlinux` が存在すること。6.1 / 6.6 の LTS 系列や、Debian 12・Ubuntu 22.04/24.04 の標準カーネルは古すぎるため対象外です。
- 物理マシンまたは KVM 仮想マシンであること。コンテナ（LXC、Proxmox CT）は不可です。
- Debian、Ubuntu、Fedora、RHEL 系（Rocky Linux、AlmaLinux、CentOS Stream）、Alpine のいずれか。

## クイックスタート

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash
```

Alpine の場合は root で実行します：

```sh
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

これで最新の公開リリースがインストールされます。ホストにコンパイラは入れません。すべての BPF オブジェクトをカーネルのベリファイアにかけたうえで、以降の新しい TCP 接続はすべて `skyline_cc` を輻輳制御に使うよう設定し、再起動後もその設定を維持します。プロキシは導入せず、ポートも一切開きません。インストール後は次のコマンドで確認できます：

```bash
ssctl status    # skyline_cc に切り替わっているか（ACCELERATING なら切り替え済み）
ssctl flows     # どの接続が高速化対象か、アルゴリズムがどう判断したか
```

アンインストールすると、ホストは bbr + fq の構成になります：

```bash
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash -s -- --uninstall
```

## 高度な使い方

**リポジトリをクローンしてインストール**すると、実行前にスクリプトの中身を確認できます：

```bash
git clone https://github.com/CYBERVERSE-Research/skyline-speeder.git
cd skyline-speeder
sudo ./install.sh --check      # 事前チェックのみ（何も変更しない）
sudo ./install.sh              # 公開リリースを導入（実行できない環境でのみビルド）
sudo ./install.sh --source     # このツリーをソースからビルド
sudo ./install.sh --release v0.6.0   # 指定した公開リリースを導入（できなければ中止）
sudo ./install.sh --uninstall --restore-pre-install   # 削除し、輻輳制御と qdisc を導入前の設定に戻す
```

アップグレードするには、インストーラーをもう一度実行するだけです。稼働中のフローを最大 60 秒かけてドレインし、`skyline_cc` を再アタッチします。

**日常の運用：**

```bash
sudo ssctl drain                                # グレースフルに切り離す（新規接続は fallback_cc を使用）
sudo ssctl enable                               # 再アタッチ
sudo ssctl enable --all-off                     # 全モジュール無効の skyline_cc（比較用のベースライン）
sudo ssctl set-redundancy --disable             # 冒頭データの二重送信を止める（アウトバウンド転送量が従量課金の環境向け）
sudo ssctl set-module-config --max-pacing-mbps 500   # 丸ごと上書き：省略したフラグはすべて既定値に戻る
```

同梱の既定値はチューニング済みの値なので、ほとんどの環境では変更不要です。`ssctl set-*` による変更はデーモンを再起動するまで有効です。永続化するには `/etc/skyline-speeder/speeder.toml` に記述してください。

**ドキュメント**（英語）：

| ドキュメント | 内容 |
|---|---|
| [docs/01-features.md](docs/01-features.md) | インストーラーが行う処理、動作の仕組み、全機能の既定値・コスト・切り替え方法、`ssctl status` / `ssctl flows` の出力の各行の見方、ソースからのビルド方法 |
| [docs/usage.md](docs/usage.md) | 全スイッチとパラメーター（設定範囲付き）、4 つのプリセット、トラブルシューティング |
| [DEPLOY.md](DEPLOY.md) | デプロイ・運用マニュアル（インストール、動作確認、アップグレード、アンインストール） |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` のコマンド、ワイヤープロトコル、設定項目、イベントコード |
| [docs/03-design.md](docs/03-design.md) | アーキテクチャと各モジュールの仕組み |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 条件を統制した 2 台の VM によるテストベッドでの bbr との比較 |

## 実測性能

bbr、[tcp-brutal](https://github.com/HyNetworks/tcp-brutal)、skyline_cc を、いずれもサーバー側にだけ適用し、実在する国際回線 1 本で比較しました。送信側はシンガポールの KVM VPS、受信側は標準 TCP のままの華南の Alibaba Cloud ECS（200 Mbps）です。RTT 70〜73 ms、ロス率 10〜30%、夜のピーク時間帯に測定し、8 ローテーション × 3 アルゴリズム × 5 シナリオで実施しました。

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/single-ended-ataglance-dark.svg">
  <img alt="ゲーム・動画・Web・大容量転送における tcp-brutal と skyline_cc の実測結果（bbr をベースラインとした比較）" src="docs/images/single-ended-ataglance-light.svg">
</picture>

| シナリオ | 勝者 | 差 |
|---|---|---|
| **動画**（6 MB チャンク @ 48 Mbps） | **tcp-brutal** | 再生開始時間 7/8 ローテーション、チャンク p95 6/8 |
| **ゲーム**（ping-pong、60 msg/s） | **skyline_cc** | ジッター 5/8、150 ms 超の途切れ 4/8 |
| **大容量転送（4 ストリーム）** | skyline_cc と tcp-brutal（いずれも bbr を上回る） | 中央値 117 / 115 vs 77 Mbps |
| **大容量転送（1 ストリーム）** | bbr と skyline_cc が互角、tcp-brutal が最下位 | 中央値 59 / 53 / 46 Mbps |
| **Web ページの読み込み** | 勝者が一定しない | ローテーションごとのばらつきがアルゴリズム間の差を上回る |
| **サーバー側の再送率**（全送信セグメントに占める割合） | bbr が最も低い | bbr 9.1%、skyline_cc 10.4%、tcp-brutal 10.9% |

これは 1 本の回線で行った実環境での測定であり、お使いの回線で同じ結果が得られることを示すものではありません。skyline_cc は 0.1.0 の既定値（現在の "high random loss" プリセット）で動かしています。測定方法、条件、全結果は [docs/01-features.md](docs/01-features.md#measured-performance-in-detail) を参照してください。

## ライセンス

GPL-2.0 です。[LICENSE](LICENSE) と [NOTICE](NOTICE) を参照してください。Copyright (c) 2026 **CYBERVERSE LLC**。"Skyline Speeder" および "CYBERVERSE" は CYBERVERSE LLC の商標です。コントリビュートする前に [CONTRIBUTING.md](CONTRIBUTING.md) をお読みください。

---

<div align="center">

単独スポンサー：**[Skyline Connect](https://www.skylineconnect.io)**

</div>
