# Skyline Speeder 部署与运维指南

## 1. 适用范围与前置条件

Skyline Speeder 只需要部署在 TCP 连接的**服务器发送端**——客户端保持标准 TCP，不需要
任何改动。当前设计面向单向长流场景，尚未针对短流/双向对等流量做验证。

部署前先明确目标机器满足：

- 内核版本满足第 2 节的要求；
- 有 root 权限（安装系统服务、加载 BPF 程序、创建 cgroup 都需要）；
- 目标发送路径的带宽、RTT、丢包特征跟第 11 节"已知风险与部署前评估"里描述
  的适用边界相符。

## 2. 内核版本要求

**精确 ABI 下限是内核 6.10（含）以上；实际支持并验证过的下限是 6.12 LTS。**

`skyline_cc.bpf.c` 里挂在 `struct tcp_congestion_ops.cong_control` 上的回调用 4
个参数声明（`sk, ack, flag, rs`）。这个签名是内核从 v6.10 才开始使用的——
v6.9 及更早版本的 `cong_control` 函数指针类型只有 2 个参数（`sk, rs`），BPF
验证器会在加载时直接拒绝 `skyline_cc`，且报错明确指向参数个数不匹配，不是配置
或编译问题。

**6.1.x 和 6.6.x 这两个 LTS 分支不支持**——两者都停留在旧的 2 参数签名。
`6.10`/`6.11` 满足 ABI 但不是 LTS、支持周期太短，不建议用来做长期部署的目标
内核，只作为版本边界参考。已验证并承诺支持的内核是 **6.12 LTS 及以后**
（含 6.12/6.18/7.1 等已实测通过版本）。

这条限制只影响 `skyline_cc`（M2/M3/M4 拥塞控制）——`skyline_tc`（重传 DSCP 标记）和
`skyline_policy`（M1 tier-2 动态 RTO）不碰 `cong_control`，不受影响，理论上可以
在更旧的内核上独立使用，但当前发布只按整体 6.12 LTS 门槛做验证和支持。

## 3. 构建

依赖：

```bash
sudo apt-get install -y \
  build-essential pkg-config clang llvm libbpf-dev libelf-dev zlib1g-dev \
  bpftool linux-tools-common linux-tools-generic iproute2
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup toolchain install 1.75.0 --profile minimal
rustup override set 1.75.0
```

（Debian 系发行版上 `linux-tools-common`/`linux-tools-generic` 可能不存在，
改用 `linux-perf`；`bpftool`/`pahole` 由 `bpftool`/`dwarves` 包提供，包名可能
因发行版而异。`iproute2` 是运行期依赖：`skyline-speederd` 用其中的 `tc` 维护出口
网卡的根 qdisc，见第 7 节。）

Debian 12 上用 `bookworm-backports` 的 6.12 内核时（在 bookworm 上够到 6.12 门槛的
常规做法），这条 `apt-get install` 会直接失败：`linux-headers-*` 把 `libelf1` 带到了
0.192，而 bookworm 的 `libelf-dev` 依赖 `libelf1 (= 0.188-2.1)`，apt 只报一句
`E: Unable to correct problems, you have held broken packages`。取与机上那份库同源的
版本即可，**只换这一个包**：

```bash
apt-cache madison libelf-dev                      # 看这台机器能拿到哪些版本
apt-get install -y libelf-dev=0.192-4~bpo12+1     # 与已装的 libelf1 同源的那个
```

不要用 `-t bookworm-backports` 整体换源：那会把 curl、iproute2、bpftool、libbpf1
一并换成 backports 版（实测 55 个包）。`install.sh` 自己会做这件事，见第 4.1 节。

```bash
make bpf    # 生成 bpf/include/vmlinux.h 并编译三个 CO-RE BPF 对象
cargo build --workspace --release
```

`make bpf` 默认从本机 `/sys/kernel/btf/vmlinux` 读取内核类型信息，所以最简单的做法
是**在目标机器本身上执行这一步**。为别的机器构建时，用 `make VMLINUX_BTF=<路径> bpf`
指定对方内核的 BTF，或用 `make PREBUILT_VMLINUX_H=<路径> bpf` 直接给一份现成的头。
对象是 CO-RE 的：头文件只需定义代码用到的类型，字段偏移在加载时按运行内核修正——
第 4.2 节的预编译产物就是在固定的 6.12 参考头上编译的。CO-RE 修不了字段改名或删除，
所以对象换到另一个内核上，一定先在目标机上跑第 7 节的 `--validate-only --verify-bpf`。
`bpf/include/vmlinux.h` 是构建产物，不提交进版本库。

## 4. 安装与部署

三条路径装出来的东西相同（同样的文件、systemd unit 与配置模板），都先过内核验证器：

| 路径 | 命令 | 适用 |
|---|---|---|
| 一键安装，源码构建（推荐） | `sudo ./install.sh` | 目标机可以安装编译工具链 |
| 一键安装，预编译产物 | `sudo ./install.sh --prebuilt` | 不想在目标机上装任何工具链 |
| 手动 | `sudo infra/install-guest.sh --confirm-install`，再按第 5–7 节 | 需要逐步控制 |

没有仓库 checkout 的机器用 `scripts/bootstrap.sh`：
`curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sudo bash`，
它把源码树放到 `/usr/local/src/skyline-speeder` 后交给 `install.sh`，其余参数原样转交
（例如 `... | sudo bash -s -- --prebuilt`）。它运行的 `install.sh` 来自 `--ref`（默认
`main`），不是最新 release。

### 4.1 一键安装（`install.sh`）

依次执行：检查系统（Debian/Ubuntu、内核 ≥ 6.12、BTF、cgroup v2）→ 安装软件包 →
准备 Rust 工具链 → 编译 BPF 对象 → 编译控制面 → 安装文件（即第 4.3 节的
`install-guest.sh`）→ 把 `runtime.tc_interface` 指向默认路由所在的网卡（见下文"选择
出口网卡"）→ 过内核验证器 → 启动（升级时是重启）`skyline-speederd` → 挂载 `skyline_cc`
并设为开机自启 → 核对结果。

| 参数 | 作用 |
|---|---|
| `--prebuilt` | 安装已发布的产物，不编译，见第 4.2 节 |
| `--release <tag>` | 同 `--prebuilt`，固定到该版本 |
| `--check` | 只做前置检查，并打印当前的拥塞控制、qdisc（含 guard 将要维护的网卡）、开机时 systemd-sysctl 会写入的相关设置，以及 `/etc/sysctl.conf` 里只由 `sysctl -p`/`sysctl --system` 应用的设置；不改动任何东西 |
| `--no-enable` | 安装并启动 daemon，但不新挂载 `skyline_cc`：不启用 `skyline-speeder-enable.service`，此前已经启用的也不停用；升级前用裸 `ssctl enable` 挂着的，重启后再 `ssctl enable` 一次，回到升级前的状态（仍是手工挂载，开机不会自动挂）。结束时按新 daemon 的 `enabled` 与当前默认拥塞控制报告实际的挂载状态 |
| `--verbose` | 不画进度条，所有子命令的输出同时打到屏幕；环境变量 `SKYLINE_VERBOSE=1` 同效 |
| `--uninstall` | 卸载：切回 bbr + fq，并卸掉安装时装上的包，见第 9 节 |
| `--restore-pre-install` | 只配合 `--uninstall`：还原成安装前的拥塞控制与 qdisc，而不是 bbr + fq，见第 9 节 |

**输出。** 终端上只有一行原地刷新的进度条，例如
`[#######-------------]  36%  (5/11) Building the Rust control plane   1m12s ⠼`，警告单独
成行；stdout 不是终端（重定向到文件、CI 日志）时不输出任何转义码，每步开始、结束各打印
一行。apt、rustup、make、cargo 和验证器的全部输出写进
`/var/log/skyline-speeder-install.log`（每次运行开头清空，结束后保留）。某一步失败时打印
`error <原因>`、这一步在日志里的最后至多 25 行和日志路径，退出码非零；Ctrl-C 中断时退出码
为 130，后台的构建进程一并结束。

**软件包。** 先让 apt 出一份计划（`apt-get -s install`），确认解得开再真正安装；计划
解不开时不立即失败。上一次 `dpkg` 被打断（VPS 在升级中途被重置）就先
`dpkg --configure -a` / `apt-get -f install --no-remove` 补完它——`--no-remove` 是刻意的，
为了让依赖自洽而卸掉运维装的包不是安装器可以替他做的决定。某个包 apt 放不下去，就按
`apt-cache madison` 从新到旧试它的其他版本，直到整份计划成立：Debian 12 上装了
`bookworm-backports` 的 6.12 内核时，`linux-headers-*` 把 `libelf1` 带到 0.192，而
bookworm 的 `libelf-dev` 依赖 `libelf1 (= 0.188-2.1)`，安装器改取 `libelf-dev
0.192-4~bpo12+1`，**只换这一个包**（刻意不用 `-t bookworm-backports`，见第 3 节）。
换版本若牵连卸载，先把要卸载的包名告警出来。`apt-get update` 因为某个仓库不可达而失败
（一键 BBR 脚本留下的源、搬走的镜像）时只告警并继续——能不能装由计划决定。`--check`
同样跑这份计划，报告 apt 到底能不能装上这些包。

**选择出口网卡。** 只在配置里还是模板值 `data0` 时改写，运维改过的配置不动（它指向的网卡
不存在也只告警，不替你改）。取默认路由经过的第一块**以太网**设备
（`/sys/class/net/<网卡>/type` 为 1；VLAN、bond、网桥也算）：先看 IPv4 默认路由，再看
IPv6（纯 IPv6 主机没有 IPv4 默认路由），各按 `ip route` 列出的顺序，取 `dev` 后面那个词。
默认路由只从 WireGuard/WARP、tun、gre、ppp 这类三层隧道离开时，保留模板值并告警：
`skyline_tc` 按以太网帧头解析报文，daemon 拒绝把它挂到非以太网设备上（第 5 节），guard
要维护的也是真正承载流量的那块网卡，这种主机只能手工设置 `runtime.tc_interface`。配置的
网卡在本机不存在、或不是以太网设备时同样告警，并给出改法：改
`/etc/skyline-speeder/speeder.toml`，然后 `sudo systemctl restart skyline-speederd`。

**安装前记录。** 动手之前记下当前的拥塞控制、`net.core.default_qdisc`，以及 guard 将要
维护的每块网卡的根 qdisc（`fq`、`cake`、`mq/fq_codel` 这样的摘要；这些网卡通常就是
`runtime.tc_interface`，它是 VLAN、bond、网桥时则是它下面的物理网卡），用于结束时的前后
对比，并写入安装前快照，`--uninstall --restore-pre-install` 据此还原；默认的 `--uninstall`
切到 bbr + fq，不读这份快照的 cc/qdisc 值（两者都见第 7 节）。

**结束摘要与使用指南。** 核对步骤向新 daemon 确认 `skyline_cc` 真的挂上了：`ssctl status`
要报告 `"enabled": true`（带 guard 的版本还要 `"armed": true`），默认拥塞控制也要确实是
`skyline_cc`。只看 sysctl 不够：daemon 被外部杀掉之后，enable unit 仍显示
`active (exited)`，`enable --now` 什么也不做，新 daemon 什么都没挂，默认值却还指着已经
不在的那个 `skyline_cc`。不满足时安装器重启一次 `skyline-speeder-enable.service` 再查，
仍不满足就失败，并提示 `sudo ssctl enable` 与 `journalctl -u skyline-speeder-enable.service`。
`default_qdisc` 或网卡根 qdisc 不是 `fq` 只告警或注明原因（例如根 qdisc 是刻意搭建的
`htb`，或设了带宽的 `cake`，guard 按设计不动它）。随后打印：

- 拥塞控制与 qdisc 的前后对比（如 `skyline_cc (was bbr)`、`mq/fq on eth0 (was cake), default fq (was cake)`；
  bond 这类由多块网卡承载的，每块网卡一行，如 `fq on eth1 (under bond0) (was fq_codel)`；
  没动的注明原因，如 `cake on eth0 (bandwidth 90Mbit: it shapes, so it was left alone)`），
  以及由谁守住它们（第 7 节的 guard）；
- 仍会把它们设成别的值的 sysctl 文件（例如"一键 BBR"脚本留下的
  `/etc/sysctl.d/99-bbr.conf`）。按 systemd-sysctl 的规则判断开机时哪个文件生效：`/etc`
  下的同名文件（包括指向 `/dev/null` 的链接或空文件，即屏蔽）盖过 `/run`、`/usr/lib` 下
  的，各文件按文件名顺序应用；`/etc/sysctl.conf` 只有被 `sysctl.d` 里的链接（Debian 12
  及更早、Ubuntu 上的 `99-sysctl.conf`）引用时才在开机生效，否则注明它只由
  `sysctl -p`/`sysctl --system` 应用。安装器**不修改**这些文件，它们先生效，随后被
  `skyline-speederd` 覆盖；
- 日常命令（`ssctl status`/`flows`/`drain`/`enable`）与调参入口：几个最常用参数的当前值、
  在线修改（`set-module-config`，绝对覆盖）与持久修改（改配置文件、`--validate-only`
  校验、重启 daemon）两种做法，详见 `docs/usage.md`。

**升级。** 在已安装的主机上按原来的方式（`--prebuilt`/`--no-enable` 照样带上）再跑一次
`install.sh` 就是升级。`skyline-speederd.service` 处于 active 时，新对象通过验证器之后
安装器自己执行 `systemctl restart skyline-speederd.service`：`skyline-speeder-enable.service`
处于 active 时随之重启——它的 ExecStop（`boot-disable.sh`）执行 `ssctl drain`：先解除
guard 并把默认算法写回 `fallback_cc`，再等存量连接至多 60 秒（经 SSH 必然走到超时，
无害）——新 daemon 起来后重新挂载。摘要里显示版本变化
（`upgraded <旧> -> <新>`；已发布的 v0.2.0 及更早的二进制没有 `--version`，显示
`from a release without --version (v0.2.0 or older)`。两个 release 之间从 main 构建的版本号
可能仍是上一个 release 的，版本号本身不说明有没有 guard，看 `status` 里的 `guard`）。已有的 `/etc/skyline-speeder/speeder.toml` 不会被覆盖，缺少的新
配置段按默认值生效（例如 `[guard]`）。用 `ssctl` 做的在线修改只在旧 daemon 的内存里，
重启后不保留；安装器已把旧的 `ssctl status` 写进安装日志，需要时照着重新下发。

`skyline_cc` 是用裸 `ssctl enable` 挂载的主机（enable unit 不是 active，而旧 daemon 报告
`enabled: true` 或默认拥塞控制是 `skyline_cc`）：没有 ExecStop 替它 drain，daemon 自己
停止时又刻意不写 sysctl（第 9 节），默认值会一直指着旧 daemon 注销掉的 `skyline_cc`。所以
安装器在重启之前自己执行一次 `ssctl drain --timeout 60`（外面再套 75 秒超时，防旧 daemon
不应答；经 SSH 必然超时，无害）。重启之后，不带 `--no-enable` 时由 enable unit 挂载，此后
开机自启；带 `--no-enable` 时再执行一次 `ssctl enable`，回到升级前的状态——仍是手工挂载，
开机不会自动挂。从 0.2.0 升级的其余注意事项（包括 `ssctl enable` 现在也会设置 `fq`）见 `CHANGELOG.md` 的
*Upgrading from 0.2.0*。

### 4.2 预编译产物（`--prebuilt`）

目标机上不需要 clang、LLVM、bpftool 或 Rust，安装器只装 `curl`、`tar`、`iproute2`。
BPF 对象由发布流水线在固定的 6.12 LTS 参考头上编译，加载时由 CO-RE 按本机内核的 BTF
修正字段偏移，所以内核要求不变：6.12 LTS 或更新，且存在 `/sys/kernel/btf/vmlinux`。

预编译的 `skyline-speederd` 在 Ubuntu 24.04 上构建，需要 **glibc 2.38 或更新**，以及
`libelf.so.1` 和 `libz.so.1`：Debian 13、Ubuntu 24.04 及更新版本满足。用户态更旧的系统
（例如 Debian 12 + backports 内核）上预编译的 daemon 起不来，`--prebuilt` 会在验证步骤
失败——这种情况改用源码构建（该组合尚未测试）。

产物来源：

- 默认：GitHub 上的最新 release；
- `--release <tag>`：指定版本；
- 环境变量 `SKYLINE_ARTIFACT_URL`：跳过 release 查找，直接使用一个 https URL（镜像、
  内网制品库），或本机上的一个文件路径（连不上 GitHub 的主机：把 tarball 连同它的
  `.sha256` 一起拷过去）。

```bash
sudo ./install.sh --prebuilt
sudo ./install.sh --release <tag>
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz sudo -E ./install.sh --prebuilt
```

**早于 guard 的 release。** `--prebuilt` 装的是已发布的 release（`--release <tag>` 则是指定
的那一个），它可能比正在运行的 `install.sh` 旧。已发布的 v0.2.0 及更早的 release 没有
guard（第 7 节）：`ssctl status` 里没有 `guard`（判断依据是这个键，不是版本号），`ssctl
enable` 本身不碰 qdisc，之后也不守住默认拥塞控制；但同一 release 附带的 enable unit（它自己
的 `infra/boot-enable.sh`）会在每次挂载和每次开机时写一次 `net.core.default_qdisc=fq`，它不
换网卡的根 qdisc，也没有谁守住这个值。安装器据此识别，打印一条告警（`... is a release older than the guard ...`），摘要
里的 qdisc 行注明 `(not managed by this release)`，也不会声称有谁守住这些设置。需要 guard
就用源码构建，或用包含它的 release。

**校验。** 产物旁发布的 `<产物>.sha256` 必须与下载（或拷来）的 tarball 一致，不一致即中止；
取不到 `.sha256` 时告警并只信任 TLS。tarball 里的 `MANIFEST` 记录 tag、commit、参考头
摘要以及每个二进制和对象的 SHA-256，完整内容写进安装日志，摘要里显示一行出处（产物名、
commit、是否已校验 sha256）。

### 4.3 手动安装（`infra/install-guest.sh`）

`infra/install-guest.sh` 会在**当前机器**上本地执行第 3 节的构建，并把产物
安装到标准系统路径：

```bash
sudo infra/install-guest.sh --confirm-install
```

它会安装/替换：

- `/usr/local/sbin/skyline-speederd`、`/usr/local/bin/ssctl`
- `/opt/skyline-speeder/bpf/*.bpf.o`
- `/opt/skyline-speeder/infra/*.sh`（`apply-guest-profile.sh`/`collect-guest-metrics.sh`/
  `snapshot-skyline-events.sh`/`run-in-skyline-cgroup.sh`，以及 enable unit 调用的
  `boot-enable.sh`/`boot-disable.sh`）
- `/etc/systemd/system/skyline-speederd.service`、`/etc/systemd/system/skyline-speeder-enable.service`

只在 `/etc/skyline-speeder/speeder.toml` 尚不存在时才会用 `config/speeder-guest.toml` 创建它——
已有配置文件不会被覆盖。

**升级已有安装**：`install-guest.sh` 只替换文件，**不会重启**已经在运行的 `skyline-speederd`
（`install.sh` 会，见第 4.1 节；走一键路径的主机不需要这一段），旧版本会继续留在内存里。
装完先跑第 7 节的第一条命令（`--validate-only --verify-bpf`，不影响正在
运行的 daemon），通过后 `sudo ssctl drain --timeout 60`（把默认算法写回 `fallback_cc`；
单独停止 daemon 不会写回。经 SSH 执行时它总会等到超时并返回非零，这无害），最后
`sudo systemctl restart skyline-speederd.service`。`skyline-speeder-enable.service` 处于 active
时会随之重启并重新挂载；按本节和第 7 节手动安装的主机上它通常不是 active，重启后 `skyline_cc`
处于未挂载状态，原来挂着的话再执行一次 `sudo ssctl enable`。各版本的升级注意事项见
`CHANGELOG.md` 里对应的 *Upgrading from ...* 小节（从 0.2.0 升级见 *Upgrading from 0.2.0*）。

## 5. 配置文件

生产环境以 `config/speeder-guest.toml` 为模板（对照 `docs/02-interface-reference.md`
第 6 节的完整字段说明）：

- 四个模块（`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`）默认全部
  启用；
- `[rack_tuning]` 整段留空——全局 sysctl 默认值不由 `skyline-speederd` 接管；
- `[rack_rto]` 默认关闭；`[retransmit_dscp]` 默认关闭，`dscp_value` 是占位
  参数，启用前必须由网络侧确定具体编码值；
- `[guard]` 默认 `interval_s = 5`、`qdisc = true`：挂载后 `skyline-speederd` 每 5 秒确认一次
  拥塞控制仍是 `skyline_cc`、`default_qdisc` 与 `runtime.tc_interface`（或它下面的物理
  网卡）的根 qdisc 仍是 `fq`，被改就改回（第 7 节）。只想让它管拥塞控制设 `qdisc = false`。

安装后先检查一遍 `/etc/skyline-speeder/speeder.toml`，确认 `runtime.tc_interface` 指向正确
的发送网卡名，再继续下一步——TC 程序挂在这块网卡上，guard 维护的也是这块网卡的根 qdisc
（它是 VLAN、bond 或网桥时，维护的是它下面的物理网卡，见第 7 节）。它必须是以太网设备：
`skyline_tc` 按以太网帧头解析每个报文，daemon 不会把它挂到 WireGuard/WARP、tun、gre、ppp
这类三层隧道上（`ssctl status` 的 `capabilities.notes` 里写明原因，`set-retransmit-dscp`
随之失败）。默认路由走隧道的主机，这里要填承载隧道流量的那块网卡。

## 6. cgroup 前提

**这是最容易被忽略、也最不容易被发现的一步**：M1 tier-2（动态 RTO 调节）
挂在 `/sys/fs/cgroup/skyline-speeder` 这个 cgroup v2 路径上，只有被迁移进这个 cgroup 的
进程建立的连接才会经过这段 BPF 代码。**进程不迁移进去不会报任何错误——只是
静默不生效**，`ssctl status` 里 `rack_rto.stats.applied` 会一直停留在 0。

用 `run-in-skyline-cgroup.sh` 包一层来启动需要被加速的服务进程：

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <你的服务启动命令...>
```

需要 root 只是为了完成 cgroup 迁移这一步；被包装的命令本身会以调用者的普通
用户身份继续执行。

（`skyline_cc`/`skyline_tc` 不受这条限制——`skyline_cc` 是全局 struct_ops，`skyline_tc` 挂在
接口级 TC 上，两者都不区分 cgroup。）

## 7. 启动验证清单

```bash
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf
sudo systemctl enable --now skyline-speederd.service
timeout 30 sh -c 'until sudo test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'
sudo ssctl enable
sudo ssctl validate
sudo ssctl status
sudo ssctl flows
```

`--validate-only --verify-bpf` 会让三个 BPF 对象都过一遍内核验证器再退出，
不留下任何运行状态——用它可以在正式启动前排除内核版本/BTF 不匹配的问题。

`systemctl enable --now` 只保证进程已经 fork/exec，不保证控制 socket已经
建好——`skyline-speederd.service` 没有 systemd readiness 通知，进程刚起来的头几秒
`/run/skyline-speeder/speeder.sock` 可能还不存在，此时立即执行 `ssctl` 会报
`connect to /run/skyline-speeder/speeder.sock: No such file or directory`，这不代表 daemon
异常，只是启动竞态；上面的 `timeout 30 sh -c '...'` 等 socket 文件出现后
再继续，自动化脚本应保留这一步而不是直接假设 socket 立即可用。

`install.sh` 在启动 daemon 之前会把当前的 `net.ipv4.tcp_congestion_control`、
`net.core.default_qdisc`，以及 guard 将要维护的网卡名和它们的根 qdisc 摘要
（`PRE_INSTALL_QDISC_DEV`/`PRE_INSTALL_ROOT_QDISC`，两个一一对应、以空格分隔的列表；只有
一块网卡时就是一个网卡名和一个摘要，与之前的格式相同）快照到
`/etc/skyline-speeder/pre-install-state`。快照只写一次，重装不会把 `skyline_cc` 误记成
"原值"；新写的快照总带着这两个键，不知道网卡时值为空。只有完全没有
`PRE_INSTALL_ROOT_QDISC` 这一行的快照（0.2.0 写的）才会在升级时补上这两个键——那些版本
从不碰根 qdisc，此时网卡上的还是原样——已有的键不动。

`--uninstall` 默认并不按这份快照还原，而是把主机切到 **bbr + fq**（第 9 节）：装过本项目的
机器几乎都是从「一键 BBR」那类脚本过来的，卸载后停在 `fallback_cc`（cubic）是没人要求过的
降级，而且没有任何东西会报告它。快照仍然有两个用处：这台内核没有 bbr 时用 `PRE_INSTALL_CC`
兜底，以及配置或网卡已经变了时用它记下的网卡清单。确实要回到安装前那套值（例如这台机器是
为了对照而特意跑 cubic 的），用 `--uninstall --restore-pre-install`。

安装器还会把「这次安装给主机装上了哪些包」记到 `/etc/skyline-speeder/added-packages`
（取 apt 执行前后两次 dpkg 已安装集合的差集，所以连 apt 顺带拉进来的依赖也在里面，而主机
本来就有的包一定不在），装了 rustup 的话把它的 `RUSTUP_HOME`/`CARGO_HOME` 记到
`/etc/skyline-speeder/added-rustup`。`--uninstall` 据此把工具链卸掉。

`skyline-speederd.service` 启动后只是常驻进程，不会自动挂载 `skyline_cc`——必须显式执行
`ssctl enable` 才会真正附加拥塞控制算法、让 `enabled_modules` 里配置的
模块生效；不执行这一步，新连接会一直走配置文件里的 `fallback_cc`。

> [!IMPORTANT]
> **`ssctl enable` 是全机生效的。** 它在附加 struct_ops 成功之后会把
> `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`，此后这台机器上**所有**新建
> TCP 连接（不只是 `/sys/fs/cgroup/skyline-speeder` 内进程建的）都默认使用本算法。
> 挂载期间 guard（见下文）也一直守着这一点，所以默认配置下**不支持"只让部分进程走
> skyline_cc"**：`enable` 之后手工把 sysctl 改回去，会在 `[guard] interval_s` 秒内（默认
> 5 秒）被改回来，日志里还会记成"主机上有别的东西改了它"。确实需要时：设
> `[guard] interval_s = 0`（不做周期检查，重启 daemon 生效），每次 `enable` 之后手工把
> `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`——`/sys/fs/cgroup/skyline-speeder`
> 里的进程建连时仍由 `skyline_policy` 切到 `skyline_cc`。这个状态只维持到下一次 `enable`，
> **包括开机时 `skyline-speeder-enable.service` 执行的那一次**，之后每次都要重新改回。
>
> 回退是对称的：`ssctl drain` 会先把 sysctl 写回 `fallback_cc`，再等存量连接结束。
>
> `[guard] qdisc = true`（默认）时 `ssctl enable` 还会把 `net.core.default_qdisc` 写成 `fq`，
> 并把 `runtime.tc_interface` 的根 qdisc 换成 `fq`（多队列网卡上是子队列全为 `fq` 的 `mq`；
> VLAN、bond、网桥则换它下面的物理网卡）。已发布的 v0.2.0 及更早 release 的 `enable` 不碰 qdisc。

**guard：挂载之后守住拥塞控制与 qdisc。** `default_qdisc` 只影响之后新建的 qdisc，网卡
的根 qdisc 往往在它生效前就建好了，所以光改 sysctl 改不到网卡；而 VPS 上常见的"一键
BBR"脚本把 `bbr` 与 `cake`/`fq_pie` 写进 `/etc/sysctl.d`，之后任何一次
`sysctl --system` 都会悄悄把拥塞控制翻回 `bbr`，新连接从此绕开 `skyline_cc` 且没有任何
报错。因此从 `enable` 到下一次 `drain`，`skyline-speederd` 独占这三项设置：`enable` 时立即
检查一遍，此后每 `[guard] interval_s` 秒（默认 5）再查一次，被改了就改回，并在 journald
里记一行 `guard: <项> <原值> -> <新值> (...)`。刻意搭建的整形/分类 qdisc（`htb`、`tbf`、
`netem`、`mqprio` 等）以及设了带宽（或 `autorate-ingress`）的 `cake` 不动，只在
`ssctl status` 的 `guard.notes` 里说明，例如
`eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone`。
`drain` 立即解除守护，且不动任何 qdisc。完整规则见 `docs/02-interface-reference.md`
第 9 节，退出方式见第 5 节的 `[guard]`。

**VLAN、bond、网桥与隧道。** 这类设备的根 qdisc 是内核默认的 `noqueue`：它们自己不排队，
报文真正排队的是下面物理网卡的根 qdisc——一键脚本的 `cake` 也装在那里。所以
`runtime.tc_interface` 的根是 `noqueue` 时，guard 顺着 `/sys/class/net/<网卡>/lower_*`
往下找（最多 4 层，每块只看一次），管理找到的物理/virtio 网卡（有 `device` 链接的那种：
bond 的从属网卡、VLAN 的真实网卡、网桥的物理端口），纠正记录里写明路径，如
`eth0 (under bond0) root qdisc cake -> fq`。没有 `device` 链接的端口（虚拟机的 tap、容器
的 veth、ifb）一律不碰：那是网桥上别人的端口，不是本机的出口。根是 `noqueue`、下面又找不到
网卡的（内核 WireGuard 设备、只挂着 tap 的网桥），只记一条
`<网卡> is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked`。
运维亲手把一块物理网卡设成 `noqueue` 的，按刻意搭建处理，不动。tun、PPP 设备（OpenVPN、WARP 客户端、
PPPoE）有自己的默认 qdisc，被当作 `tc_interface` 时 guard 管的就是它自己的根。

`ssctl status` 的 `capabilities` 字段列出六项硬性前提
（`btf`/`bpffs`/`cgroup_v2`/`fq_available`/`struct_ops`/`fallback_cc_available`）——任一为 `false`，
执行 `ssctl validate`/`ssctl enable` 时会直接拒绝，不会带着一个残缺的
能力集运行；`skyline-speederd.service` 进程本身的启动不受这条门槛影响。

## 8. 在线调参与配置切换

M2/M3/M4 的系数和 M1 tier-2 的 RTO 参数都可以在 `skyline-speederd` 运行期间在线调整，
不需要重启：

```bash
sudo ssctl set-module-config --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.25 --guardrail-gain 0.8
sudo ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000
```

两者都是绝对覆盖语义（每次调用都发送完整字段集合），且都提供对应的
`reset-*` 命令恢复为配置文件里的默认值。完整字段列表见
`docs/02-interface-reference.md` 第 2.3/2.4 节。

调整立即生效：`skyline-speederd` 把新系数写入一个非活跃配置槽并递增代际计数器，每条连接
只在 RTT 边界切换到新配置槽，避免同一轮 ACK 处理内读到新旧混杂的值。

## 9. 摘除与回滚

> [!IMPORTANT]
> **通过 SSH 执行 `drain` 必然走到超时**：你自己的 SSH 连接就是一条 skyline_cc 流，
> 它不会在 drain 等待期间结束。这不是故障——drain 在开始等待**之前**就已经把
> `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`，所以无论超时与否都不会再有
> 新连接使用 skyline_cc；struct_ops 保持挂载直到最后一条存量流结束，这是内核的引用
> 计数行为。要真正等到归零，从串口控制台或一条不走本机 skyline_cc 的通道执行。

优雅摘除用 `drain`：

```bash
sudo ssctl drain --timeout 60
```

它会先解除 guard（此后不再有检查把 `skyline_cc` 写回去），阻止新连接被分派到
`skyline_cc`，等待已有连接自然结束（默认超时 300 秒，可覆盖），确认无活跃连接后再注销
struct_ops——新建立的连接此后落回配置文件里的 `fallback_cc`。`drain` 不动 qdisc：
`default_qdisc` 与网卡根 qdisc 保持 `fq`。M1 tier-2 的 RTO 调节和重传 DSCP 标记不受这个
开关影响，需要单独用 `ssctl reset-rack-rto`/`reset-retransmit-dscp` 关闭。

正常停止服务（`systemctl stop skyline-speederd.service`，等价于收到 SIGTERM/SIGINT）会
正确注销全部 struct_ops 和 BPF 链接，重启服务前也不需要手动清理残留状态；但它**不会**把
`net.ipv4.tcp_congestion_control` 写回 `fallback_cc`。`skyline-speeder-enable.service` 处于
active 时，停止 daemon 会先停这个 unit，由它的 ExecStop（`boot-disable.sh`）写回：它先执行
`ssctl drain`（drain 自己解除 guard、写回 `fallback_cc`，再等存量连接），之后只在默认仍是
`skyline_cc` 时（daemon 已经不在、drain 没能执行）才自己写一次 `fallback_cc`——顺序反过来
的话，仍处于武装状态的 guard 会把刚写回的值又改成 `skyline_cc`。按第 7 节用 `ssctl enable`
挂载的主机上它不是 active，停止或重启 daemon 之前先执行 `sudo ssctl drain --timeout 60`
（经 SSH 必然走到超时，无害，见上方说明）；用 `install.sh` 升级时安装器会替你做这一步
（第 4.1 节）。

彻底卸载：`sudo ./install.sh --uninstall`。它先 drain（同时解除 guard），停用并删除两个
unit、二进制与 `/opt/skyline-speeder`，保留 `/etc/skyline-speeder`，然后：

1. **切到 bbr + fq。** `net.ipv4.tcp_congestion_control=bbr`、`net.core.default_qdisc=fq`，
   并把 `runtime.tc_interface` 解析出来的网卡（guard 武装时维护的正是这些；与 `[guard] qdisc`
   是否被关过无关）根 qdisc 确认成 `fq`（多队列网卡是子队列全为 `fq` 的 `mq`）。
   本来就是 `fq` 的（guard 刚才还在守着，所以通常如此）不动；是 `htb`/`tbf`/`netem`/设了带宽
   的 `cake` 这类**看起来是特意建的**就原样留下、只报告，并给出手工设置 `fq` 的命令——和
   guard 从不替换它们是同一个理由：覆盖掉会悄悄毁掉运维的整形配置。这台内核没有 bbr 时先
   `modprobe tcp_bbr`（写 sysctl 本身不会加载它：内核只接受已注册的算法名），仍然没有就退回
   `PRE_INSTALL_CC`、再退 cubic/reno，并明确告警用了哪一个。**只在本次运行时生效**：不写也
   不改 `/etc/sysctl.d` 下的任何文件（installed 期间 guard 遵守的也是这条规矩），所以重启后
   仍由那些文件决定，卸载结束时会专门提示这一点。
2. **卸掉安装时装上的工具链。** 按 `/etc/skyline-speeder/added-packages`（见第 7 节）
   `apt-get --purge remove`，四道护栏：
   - `iproute2`/`curl`/`ca-certificates`/`tar` 永不移除；
   - **这几个包递归依赖的东西也永不移除**——保留 `curl` 却删掉它底下的 `libcurl4t64`，apt
     只会用「那就把 curl 一起删」来解决这个矛盾。实测：少了这一条，测试机上是「整条工具链
     一个都卸不掉」，有了它是「93 个记录里卸掉 78 个」；
   - dpkg 优先级为 `required`/`important` 的永不移除（按每个架构分别判断：多架构主机上
     `dpkg-query -W -f '${Priority}'` 会把几个架构的优先级连成一个词，那样这道护栏会静默失效）；
   - 先 `apt-get -s --purge remove` 让 apt 出计划，计划里的 `Purg`/`Remv` 一旦出现清单之外的
     包，就把「被它依赖、因而造成这件事」的那个包单独留下并告警说明是谁需要它，其余照卸，
     再重新出计划（最多三轮）；三轮之后仍不干净，或者根本找不出该留谁，才**一个都不卸**、
     改为打印可自行执行的命令。写这段代码时实测：某台机器上如果把 `libelf1` 也当成可卸的，
     apt 的计划会连带删掉 29 个包，其中有 `iproute2`、`ifupdown`、`isc-dhcp-client`、
     `cloud-init`。

   记录本身也只取 apt 为这次请求实际接受的计划（`Inst` 行）与前后 dpkg 差集的交集：装包时
   往往有 unattended-upgrades 在并行跑（`install.sh` 会等 dpkg 锁最多五分钟正是因为这个），
   它装的东西不是我们该卸的。此时 Skyline Speeder 本身已经卸完了，所以这一步的任何失败都只是
   告警加一条手工命令，不会中断卸载。
   记录里的包一个都不在了，或没有这份记录（例如 0.2.0 装的机器），这一步什么也不做。
   rustup 只在 `/etc/skyline-speeder/added-rustup` 存在时移除（即确实是安装器装的），优先用
   `rustup self uninstall`，它不可用时才删目录，且只删仍然长得像 rustup 留下的目录。

加 `--restore-pre-install` 则把上面第 1 步换成按安装前快照还原：拥塞控制、`default_qdisc`；
快照里记有网卡的根 qdisc 时，逐块把它的**种类**也还原回去——
只在它仍是本项目留下的 `fq`/`mq/fq` 时才动（之后被别人改过的不碰），只还原种类、用默认
参数（例如手工设过的 `cake` 带宽不会回来），`mq` 下混有多种 qdisc 的只告警不还原。还原
`mq/<种类>` 时先把 `default_qdisc` 临时设成该种类，再 `tc qdisc del dev <网卡> root`，
让内核按它重建自己的 `mq`（guard 留下的是 `tc` 建的 `mq`，对它执行
`replace root mq` 是被内核接受、却什么都不改的空操作）；内核自己的 `mq`（句柄 0）删不掉，
这时才用 `replace root mq`。最后把 `default_qdisc` 设回、重新读取核对，不符就告警。

不用安装器时：先 `sudo ssctl drain --timeout 60`，再
`sudo systemctl disable --now skyline-speeder-enable.service skyline-speederd.service`，然后删除
第 4.3 节列出的全部安装路径；qdisc 与 sysctl 需要自己改回。

## 10. 可观测性与排障

`ssctl status` 的 `metrics` 字段（`skyline_cc` 从未启用过时为 `null`）暴露决策
计数器；事件环形缓冲区落盘到配置里的 `runtime.events_path`，记录状态切换/
护栏触发/配置代际切换等事件。完整字段与事件码含义见
`docs/02-interface-reference.md` 第 4/8 节。

事件日志位于 `/run`（内存 tmpfs），大小受 `runtime.events_max_mib` 限制（默认 8 MiB，
满了轮转为 `events.jsonl.1`，最多约占两倍）；不需要事件时设为 `0` 即可关闭。
早于此上限的版本会无限增长，可能写满 `/run`，使 Docker 等依赖 `/run` 的服务失败。
在这类版本上用 `truncate -c -s 0 /run/skyline-speeder/events.jsonl` 回收空间，不要用
`rm`：daemon 仍持有文件句柄，删除后空间不会释放。

**guard 的状态与日志。** `ssctl status` 的 `guard` 字段给出是否武装（`armed`）、改回过
几次（`cc_restored`/`default_qdisc_restored`/`interface_qdisc_replaced`）、最近一次改动与
失败、刻意没动的东西和暂时没做成的事（`notes`），以及现场读到的拥塞控制与 qdisc
（`live`；其中 `live.devices` 是 guard 实际维护的网卡及其根 qdisc，VLAN、bond、网桥上
就是它下面的物理网卡）。一次替换失败不会让 guard 就此放弃这块网卡：周期检查先退避
（首次 30 秒后重试，`interval_s` 更长时按它，之后每次失败翻倍，最长 5 分钟），期间
`notes` 里是 `<网卡> root qdisc <摘要>: the last replace failed; retrying in <N> s`，原因看
`last_error`；`ssctl enable` 不等退避、立即重试。每次改动都在 journald 里记一行：

```bash
journalctl -u skyline-speederd.service | grep 'guard:'
```

`(something else on this host changed it)` 结尾的行反复出现，说明主机上有别的东西在跟你
抢这些设置（常见是 `sysctl --system` 重新应用了 `/etc/sysctl.d` 里的 `bbr`）——guard 会
一直改回，要根除就找出那个文件。`ssctl status` 的 `version` 是正在运行的 daemon 的版本，
和 `skyline-speederd --version`（磁盘上的二进制）不一致说明还没重启到新版本。

**M1 tier-2 排障决策树**（`rack_rto.stats` 一直是 0 时）：

1. `rack_rto.stats.rtt_callbacks` 也是 0 → BPF 程序完全没被调用，先检查目标
   进程是不是真的在 `/sys/fs/cgroup/skyline-speeder` 里（见第 6 节）；
2. `rtt_callbacks` 非零但 `subscribe_ok` 是 0、`subscribe_err` 非零 → 订阅
   本身失败，检查内核版本的 sockops 能力；
3. `subscribe_ok` 非零但 `applied` 是 0、`rejected` 非零 → 订阅成功但内核拒
   绝了下发的值，多半是取值超出了内核自身对 RTO 的合法区间。

一个容易误判的现象：双栈监听 socket 上的 IPv4 连接会以 IPv4-mapped
`AF_INET6`（`::ffff:a.b.c.d`）形态出现，不是纯 `AF_INET`——这是大多数不显式
绑定地址族的服务的常态，不是异常。Skyline Speeder 的 family 判断同时接受两种形态，如果
自己新增代码需要判断地址族，要留意这一点。

## 11. 已知风险与部署前评估

Skyline Speeder 的自保护机制只识别两个真实拥塞信号：队列时延增长和 ECN 标记。**部署前
需要评估目标链路的丢包主要是随机丢包还是排队造成的**——如果链路的丢包本质
上来自网络设备排队（而不是无线链路层这类跟排队无关的随机丢失），Skyline Speeder 的丢包
补偿机制和排队护栏机制会有相反的作用方向，需要结合实际链路特征评估是否
适用。

其余已知的测试有效性边界（离线负载能力上限、BBR 版本不可调等）和局限性
清单见 `docs/04-performance-report.md` 第 8 节。

## 12. 复现性能测试指引

如果需要在自己的测试环境里重新验证第 4 章描述的性能结果，完整的复现步骤和
所需资源见 `research/experiments/README.md`。
