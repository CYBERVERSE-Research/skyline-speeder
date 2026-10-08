# Skyline Speeder —— Agent 部署手册

面向**自动化 agent 与无人值守脚本**的确定性部署与运维手册，也是本项目**唯一**的部署文档：
部署前判断、安装、验证、调参入口、升级、排障与卸载都在这里，人类运维同样照它操作。每一步
都给出可由机器判定的成功条件，以及已知失败模式的处置。

- 新手调参、现成配方、连接卡顿排查：[docs/usage.md](docs/usage.md)
- 每个命令、字段、配置项的精确定义：[docs/02-interface-reference.md](docs/02-interface-reference.md)
- 算法原理：[docs/03-design.md](docs/03-design.md)；性能数据与适用边界：[docs/04-performance-report.md](docs/04-performance-report.md)

**约定**

- 所有命令以 root 执行。普通用户在命令前加 `sudo`；引导脚本的管道写成 `| sudo bash`，要把环境变量
  带进去时用 `sudo -E`。
- 退出码非零即失败，不要继续下一步。例外都在原处写明：`ssctl drain` 超时（§11.1）、`--check` 的
  告警（§2.2）。
- 命令按 systemd 写，OpenRC（Alpine）的写法见 §12 的对照表。

从 `ssctl status --json` 取值用下面这个函数，每个新 shell 里先定义一次。它和 `install.sh` 用的是同一个办法：
`ssctl` 输出的 JSON 一行一个键，取某个键**第一次出现**的值，不依赖 python3 或 jq（精简镜像上没有）。
`version`、`enabled` 是 `status` 的头两个字段，`armed` 只出现在 `guard` 里，所以第一次出现的正是要的那个。
v0.2.0 及更早的 `ssctl` 不认识 `--json`（它不带参数就输出 JSON），函数这时退回不带参数的 `ssctl status`：

```sh
sv() { { ssctl status --json 2>/dev/null || ssctl status 2>/dev/null; } \
         | awk -v k="\"$1\":" '!f && $1 == k { v = $2; gsub(/[",]/, "", v); print v; f = 1 }'; }
sv enabled    # true / false：skyline_cc 是否已挂载
sv version    # 正在运行的 daemon 的版本；v0.2.0 及更早的 daemon 不报告版本，为空
sv armed      # guard 是否武装
```

> [!WARNING]
> 不要用 `ssctl status --json | grep -q '"enabled": true'` 判断是否挂载：0.4.0 起 JSON 里还有默认
> 为 `true` 的 `redundancy.config.enabled`，什么都没挂载时这条 grep 也成立。

---

## 0. 速查

### 0.1 安装

```bash
# 有源码树（仓库根目录）
./install.sh                  # 默认：装最新发布的预编译产物；本机跑不了时自动改为源码构建（§2.1）
./install.sh --source         # 在本机从这份源码树构建

# 没有源码树：Debian/Ubuntu、Fedora 与 RHEL 系（没有 curl 的先 apt-get/dnf install -y curl）
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
# Alpine：只有 busybox 的 wget，没有 curl 和 bash，引导脚本会补上
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

`scripts/bootstrap.sh` 把源码树放到 `/usr/local/src/skyline-speeder`（已有的旧树挪成
`<目录>.bak-<时间戳>`，不删除），按本机的包管理器补上缺的 `curl`、`bash`、`tar`，然后执行那里的
`install.sh`。其余参数原样转交：`... | bash -s -- --source`（Alpine 是 `... | sh -s -- --source`）。
它运行的 `install.sh` 来自 `--ref`（默认 `main`），装上的产物却来自最新 release（或 `--release` 指定
的那个）：`--ref` 选的是安装器，不是安装的版本。之后升级或卸载可以直接用那份树：
`/usr/local/src/skyline-speeder/install.sh --uninstall`；经引导脚本传 `--uninstall` 时它也直接用这份树，
不再下载。

### 0.2 成功判据

```sh
v=$(skyline-speederd --version 2>/dev/null | awk '{print $2}')
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = skyline_cc ] \
  && [ "$(sv enabled)" = true ] \
  && { [ -z "$v" ] || [ "$(sv version)" = "$v" ]; }
```

三条依次确认：全机默认算法是 `skyline_cc`；daemon 自己报告已挂载（只看 sysctl 不够：daemon 被外部
杀掉之后，默认值可能还指着已经注销的 `skyline_cc`）；内存里跑的就是刚装上的版本
（`--version` 读磁盘上的二进制，`status` 的 `version` 来自正在运行的进程，不一致说明还没有重启到新版本）。
v0.2.0 及更早的二进制没有 `--version`，第三条随之跳过。按设计不挂载的 `--no-enable` 安装（§2.2）
不适用这组判据。

### 0.3 输出与退出码

- stdout 不是终端时（agent、CI、重定向到文件），每一步开始、结束各打印一行
  `[ n/N] <步骤名>` / `[ n/N] done in <耗时>`，最后一行是 `all N steps done in <耗时>`，随后是安装摘要
  与使用指南；终端上则是一行原地刷新的进度条。N 是 9（预编译）或 11（源码构建），`--no-enable` 时各少 2。
- `--verbose`（或环境变量 `SKYLINE_VERBOSE=1`）不画进度条：步骤行变成 `==> (n/N) <步骤名>`，没有
  `done in` 与 `all N steps` 两种行，每条子命令的输出同时打到屏幕。
- `warn …` 与 `error …` 写到 stderr，进度与摘要写到 stdout。
- 包管理器、rustup、make、cargo 与验证器的全部输出写进 `/var/log/skyline-speeder-install.log`，每次安装
  开头清空、结束后保留（`--check` 与 `--uninstall` 不写它）。某一步失败时打印 `error <原因>`、这一步在
  日志里的最后至多 25 行和日志路径，退出码非零。Ctrl-C 中断时退出码为 130，后台的构建进程一并结束。
- 耗时：预编译安装通常在一分钟以内，升级时再加最多 60 秒的 drain（§10）；源码构建在 1 vCPU 的主机上要
  5–10 分钟，Alpine 更久。超时要留足。

---

## 1. 部署前判断

### 1.1 它装在哪、动了什么

- **只部署在发送数据的那一端**（通常是服务器）。客户端保持标准 TCP，不需要任何改动。所以加速的是这台
  机器**发出去**的数据：用户下载变快，上传不会。
- `ssctl enable`（安装器自动执行）**全机生效**：之后这台机器上所有新建的 TCP 连接默认都走 `skyline_cc`，
  不只是某个进程（§5.2）。
- 挂载期间 `skyline-speederd` 独占三项设置：默认拥塞控制、`net.core.default_qdisc` 与出口网卡的根
  qdisc（§9）。安装器不装代理、不开任何端口。

### 1.2 链路适用性

> [!WARNING]
> 自保护机制只认两个真实拥塞信号：排队时延增长与 ECN 标记。它的前提是**丢包本身不代表拥塞**。如果链路
> 的丢包其实来自设备排队溢出，丢包补偿与排队护栏会朝相反方向用力，只会更糟。它也不追求公平，不会刻意和
> 竞争的流平分链路。

判别方法：在对端跑 `iperf3 -s`，从本机发固定速率的 UDP，逐级加压：

```bash
for r in 100M 200M 400M 800M; do
    iperf3 -c <peer> -u -b $r -t 12 | tail -3
done
```

| 观察结果 | 结论 |
|---|---|
| UDP 各速率**都零丢包**，而 TCP 大量重传 | 丢包是自身突发造成的排队溢出，**不适用** |
| UDP 在低速率下就有稳定丢包 | 链路层随机丢包，适用 |
| 低速不丢，速率一高才丢 | 把链路撑满了，属于拥塞丢包，**不适用** |

ICMP 的丢包率**不能**作为判据：多数网络对 ICMP 限速，它和数据面的丢包无关。其余已知的有效性边界与
局限见 `docs/04-performance-report.md` 第 8 节。

### 1.3 硬性前置条件

| 条件 | 判定命令 | 要求 |
|---|---|---|
| 内核版本 | `uname -r` | **>= 6.12**（ABI 下限 6.10，见下） |
| 内核 BTF | `test -r /sys/kernel/btf/vmlinux` | 存在（`CONFIG_DEBUG_INFO_BTF=y`） |
| cgroup v2 | `test -e /sys/fs/cgroup/cgroup.controllers` | 以 unified 方式挂在 `/sys/fs/cgroup`（OpenRC 主机上由 `cgroups` 服务挂载，安装器会启动它） |
| `fq` qdisc | `modprobe sch_fq` | 内置或可加载 |
| struct_ops | §4 能力报告的 `struct_ops` | `true` |
| `fallback_cc` | §4 能力报告的 `fallback_cc_available` | `true`（§8.1） |
| bpffs | `mount \| grep -q ' /sys/fs/bpf '` | 已挂载（没挂载时安装器在建 `/sys/fs/bpf/skyline-speeder` 那一步失败；能力报告的 `bpffs` 只检查 `/sys/fs/bpf` 这个目录存在——没挂载 bpffs 时它也在——代替不了这条） |
| 发行版 | `cat /etc/os-release` | Debian/Ubuntu（apt + systemd）、Fedora 与 RHEL 系——Rocky Linux、AlmaLinux、CentOS Stream（dnf + systemd）、Alpine（apk + OpenRC）；其他发行版在第一步就以非零退出码被拒绝 |

daemon 自己强制的是能力报告里的六项：`btf`、`bpffs`、`cgroup_v2`、`fq_available`、`struct_ops`、
`fallback_cc_available`。任一为 `false`，`--validate-only`、`ssctl validate`、`ssctl enable` 都直接
拒绝，不会带着残缺的能力集运行；`skyline-speederd.service` 进程本身照样能启动。一键安装已在 Debian 13、
Fedora 43、Rocky Linux 10.2、Alpine 3.23 上完整跑通。

> [!CAUTION]
> **`6.1.x` 与 `6.6.x` 两个 LTS 分支明确不支持。** `skyline_cc.bpf.c` 的 `cong_control` 回调按 4 个参数
> （`sk, ack, flag, rs`）声明，这个签名从 v6.10 才有；更早的内核只有 2 参数版本（`sk, rs`），验证器会在
> 加载时直接拒绝，报错明确指向参数个数不匹配。**这不是配置问题，不要尝试绕过。**

`6.10`/`6.11` 满足 ABI 但不是 LTS，不建议作为长期部署的目标；验证并承诺支持的是 **6.12 LTS 及以后**
（6.12、6.18、7.1 等已实测通过）。这条限制只来自 `skyline_cc`：`skyline_tc` 与 `skyline_policy` 不碰
`cong_control`，但发布只按整体 6.12 的门槛验证。

---

## 2. 一键安装：`install.sh`

### 2.1 预编译产物还是源码构建

不带参数时安装器先判断本机能否运行发布的二进制：架构是发布流水线出产物的架构（目前只有 x86_64），且
C 库够新。两种 C 库各有一个产物：

- glibc 版 `skyline-speeder-<tag>-x86_64.tar.gz`：daemon 在 Ubuntu 24.04 上构建，需要 **glibc ≥ 2.38**
  （`getconf GNU_LIBC_VERSION`）以及 `libelf.so.1`、`libz.so.1`。Debian 13、Ubuntu 24.04、Fedora 43、
  Rocky Linux 10 及更新版本满足。
- musl 版 `skyline-speeder-<tag>-x86_64-musl.tar.gz`（v0.4.1 起）：在 Alpine 3.21 容器里构建，需要
  **musl ≥ 1.2.5**（`/lib/ld-musl-*.so.1` 存在即是 musl，版本由它自己报出），即 Alpine 3.21 及更新，以及
  `libelf`、`zlib`、`zstd-libs`、`libgcc`。

两个产物里的 BPF 对象是同一套文件，逐字节相同。能运行就装产物，否则在 stderr 打印一行
`warn building from source: <原因>` 后改为源码构建（例如 Debian 12 + backports 内核：内核够新，glibc
不够）。musl 主机上默认路径还会先问一次 release API：最新 release 没有 musl 版（v0.4.0 及更早）时同样
改为源码构建。

| | 预编译产物（默认） | 源码构建（`--source`，或默认路径在本机跑不了产物时） |
|---|---|---|
| 命令 | `./install.sh`；`--prebuilt` 只装产物；`--release <tag>` 固定版本 | `./install.sh --source` |
| 目标机需要 | 不需要任何工具链；安装器只装运行期依赖（§2.4 的包列表） | 编译工具链（clang/LLVM、bpftool、libbpf/libelf/zlib 开发包、Rust），安装器自动安装并记录，`--uninstall` 再卸掉 |
| 装的是什么 | 最新（或 `--release` 指定的）release：`main` 上在它之后合并的改动不在其中 | 运行的这份源码树（一键安装时即 `--ref`，默认 `main`） |
| BPF 对象的类型来源 | 发布流水线固定的 6.12 LTS 参考头；加载时由 CO-RE 按本机内核的 BTF 修正字段偏移 | 本机 `/sys/kernel/btf/vmlinux` |
| 完整性校验 | 产物旁的 `.sha256` 必须匹配；取不到 `.sha256` 时告警、只信任 TLS | 源码树本身（`scripts/bootstrap.sh` 可用 `SKYLINE_SHA256` 固定 tarball 的摘要） |

其余步骤完全相同：同样过验证器，同样的服务（systemd unit 或 OpenRC 脚本）与配置模板。

### 2.2 参数与环境变量

| 参数 | 作用 |
|---|---|
| （不带参数） | 本机能运行发布的二进制时装最新 release 的产物，否则打印原因并改为源码构建（§2.1） |
| `--source` | 无条件从这份源码树构建 |
| `--prebuilt` | 只装已发布的产物：本机跑不了时在第一步之前就以 `error --prebuilt/--release cannot install here: <原因>` 中止，不会改为构建 |
| `--release <tag>` | 同 `--prebuilt`，固定到该版本（也是装回旧版本的办法，§10.3） |
| `--check` | 只做前置检查，不改动任何东西：报告将走哪条路径（`would install the published release` / `would build from source: <原因>`）、包能否装上、当前拥塞控制与 qdisc（含 guard 将要维护的网卡）、开机时 systemd-sysctl 会写入的相关设置、`/etc/sysctl.conf` 里只由 `sysctl -p`/`sysctl --system` 应用的设置，以及新建配置会得到的 `fallback_cc` |
| `--no-enable` | 安装或升级并启动 daemon，但不新挂载 `skyline_cc`：不启用 `skyline-speeder-enable.service`，此前已经启用的也不停用；升级前是用裸 `ssctl enable` 挂着的，重启后再 `ssctl enable` 一次（仍是手工挂载，开机不会自动挂）。结束时按新 daemon 的 `enabled` 与当前默认拥塞控制报告实际的挂载状态，并给出挂载命令 |
| `--verbose` | 不画进度条，所有子命令的输出同时打到屏幕；环境变量 `SKYLINE_VERBOSE=1` 同效 |
| `--uninstall` | 卸载，切到 bbr + fq，并卸掉安装时装上的包（§11.3） |
| `--restore-pre-install` | 只能配合 `--uninstall`（单独使用直接报错退出）：还原安装前的拥塞控制与 qdisc，而不是 bbr + fq（§11.4） |
| `-h`/`--help` | 打印用法；其他参数一律以 `unknown argument` 退出 |

`--check` 的退出码：发行版、服务管理器、内核、BTF、cgroup 不满足，或 `--prebuilt`/`--release` 在本机装不
了时非零；包装不上、工具链装不上这类问题只在 stderr 打 `warn`，退出码仍是 0。agent 要同时看 stderr。

| 环境变量 | 作用 |
|---|---|
| `SKYLINE_ARTIFACT_URL` | 跳过 release 查找，直接用一个 https URL（镜像、内网制品库）或本机文件路径作为产物；优先于 `--release`；`<URL 或路径>.sha256` 存在就校验（§2.3） |
| `SKYLINE_VERBOSE=1` | 同 `--verbose` |
| `SKYLINE_REPO` | 查询 release 的仓库，默认 `CYBERVERSE-Research/skyline-speeder` |

`scripts/bootstrap.sh` 自己的参数：`--ref <git 引用>`（环境变量 `SKYLINE_REF`，默认 `main`）、
`--repo <owner/name>`（`SKYLINE_REPO`，取源码的仓库）、`--src-dir <路径>`（`SKYLINE_SRC_DIR`，默认
`/usr/local/src/skyline-speeder`）；`SKYLINE_SHA256=<摘要>` 固定下载的源码 tarball（用 `sudo -E` 传进去）。
`--repo` 只决定从哪里取源码；要让安装器也从那个仓库取 release，设环境变量 `SKYLINE_REPO`。

### 2.3 产物来源与校验

- 默认：GitHub 上的最新 release；`--release <tag>`：指定版本；
- `SKYLINE_ARTIFACT_URL`：https URL 或本机文件路径。连不上 GitHub 的主机把 tarball 连同它的 `.sha256`
  一起拷过去：

```bash
SKYLINE_ARTIFACT_URL=https://mirror.example/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
```

产物旁发布的 `<产物>.sha256` 必须与下载（或拷来）的 tarball 一致，不一致即中止。tarball 里的 `MANIFEST`
记录 tag、commit、参考头摘要、`libc=` 以及每个二进制与对象的 SHA-256：完整内容写进安装日志，摘要里显示
一行出处（产物名、commit、是否已校验 sha256）。`libc=` 与本机 C 库不符的产物（例如
`SKYLINE_ARTIFACT_URL` 指错了文件）在验证器之前就被拒绝，并说明该用哪一个。

装上的是已发布的 release，不是 `main`：`main` 上在最新 release 之后合并的改动，要等下一个 release 才会
进入默认安装。

### 2.4 安装器依次做了什么

| 步骤（输出里的名字） | 做什么 |
|---|---|
| Checking the system | 发行版及其包管理器、服务管理器；内核 ≥ 6.12、BTF、挂在 `/sys/fs/cgroup` 的 cgroup v2（OpenRC 主机上还没挂时启动 `cgroups` 服务）；本机能否运行发布的二进制 |
| Installing packages | 见下面的包列表与计划规则 |
| Downloading the release artifact | 预编译路径：取产物、校验 sha256 与 `MANIFEST` |
| Preparing the Rust toolchain / Building the BPF objects / Building the Rust control plane | 源码构建路径：rustup（`rust-toolchain.toml` 固定版本）、`make bpf`、`cargo build --release` |
| Installing files | 预编译路径直接复制产物里的文件；源码构建路径执行 `infra/install-guest.sh`。文件清单见 §2.5 |
| Configuring speeder.toml | 配置不存在时才用模板新建；改写出口网卡与 `fallback_cc`（见下） |
| Running the kernel verifier | `skyline-speederd --validate-only --verify-bpf`（§4）；失败即中止 |
| Starting skyline-speederd | 升级时是 `Draining live flows (up to 60 s), restarting skyline-speederd`（§10） |
| Attaching skyline_cc | `systemctl enable --now skyline-speeder-enable.service`（`--no-enable` 时跳过） |
| Verifying | 向新 daemon 确认 `enabled` 为 `true`（带 guard 的版本还要 `armed`），且默认拥塞控制确实是 `skyline_cc`；不满足时重启一次 enable unit 再查，仍不满足就失败，并提示 `ssctl enable` 与 enable unit 的日志 |

**包。** 预编译路径只装运行期依赖：apt `curl ca-certificates tar iproute2`；dnf `curl ca-certificates tar
iproute iproute-tc` 加按 soname 请求的 `libelf.so.1`、`libz.so.1`；apk `curl ca-certificates tar iproute2
bash libelf zlib zstd-libs libgcc`。源码构建装的是另一份列表：apt `build-essential pkg-config clang llvm
libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2`；dnf `gcc make pkgconf-pkg-config clang llvm libbpf-devel
elfutils-libelf-devel zlib-devel bpftool curl ca-certificates tar iproute iproute-tc`；apk `build-base pkgconf clang
llvm libbpf-dev elfutils-dev zlib-dev linux-headers bpftool curl ca-certificates tar iproute2 bash libgcc`。
`iproute2`（Fedora/RHEL 上的 `iproute-tc`）是运行期必需的：guard 用其中的 `tc` 维护网卡的根 qdisc。

- apt 先出一份计划（`apt-get -s install`），确认解得开再真正安装。上一次 `dpkg` 被打断（例如 VPS 在升级
  中途被重置）就先 `dpkg --configure -a` / `apt-get -f install --no-remove` 补完——`--no-remove` 是刻意的，
  为了依赖自洽而卸掉运维装的包不是安装器能替他做的决定。某个包放不下去，就按 `apt-cache madison` 从新到旧
  试它的其他版本，直到整份计划成立（Debian 12 上装了 `bookworm-backports` 的 6.12 内核时，正是只把
  `libelf-dev` 换成 `0.192-4~bpo12+1`，刻意不用 `-t bookworm-backports`：那会换掉 55 个包）。换版本后的
  计划要卸载任何包时**中止**并列出这些包；`apt-get update` 因为某个仓库不可达而失败时只告警并继续。
- dnf 与 apk 把整份请求一起求解，要么全装、要么一个不装，装不上时直接转述它们自己的报错。dnf 不装弱依赖。
  RHEL 系的 `libbpf-devel` 在默认关闭的 CodeReady Builder 仓库里，源码构建时只对这一次事务
  `--enablerepo=crb`（RHEL 上是 `codeready-builder-for-rhel-N-<arch>-rpms`，Oracle Linux 上是
  `olN_codeready_builder`），不改 `.repo` 文件。apk 的 `bpftool` 在 community 仓库：主机没启用时只对这一次
  命令加上同一镜像、同一版本的 `--repository .../community`，不改 `/etc/apk/repositories`。

**出口网卡（`runtime.tc_interface`）。** 只在配置里还是模板值 `data0` 时改写，运维改过的配置不动（它指向
的网卡不存在也只告警）。取默认路由经过的第一块**以太网**设备（`/sys/class/net/<网卡>/type` 为 1；VLAN、
bond、网桥也算）：先看 IPv4 默认路由，再看 IPv6（纯 IPv6 主机没有 IPv4 默认路由）。默认路由只从
WireGuard/WARP、tun、gre、ppp 这类三层隧道离开时保留模板值并告警：`skyline_tc` 按以太网帧头解析报文，
daemon 拒绝把它挂到非以太网设备上，这种主机要手工设成承载隧道流量的那块物理网卡（§8.1）。

**`fallback_cc`。** 同样只在这次运行新建配置文件时改写。它是 `skyline_cc` 不在时新连接用的算法（§8.1），
模板值是 `cubic`；安装器改用开机时 sysctl 配置选定的那个算法（按 systemd-sysctl 的规则找出生效的文件，例如
"一键 BBR"脚本留下的 `/etc/sysctl.d/99-bbr.conf` 里的 `bbr`）：它就是这台机器不装 Skyline Speeder 时运行的
算法，也是每次开机 `skyline-speederd` 启动前一定已经注册的算法——systemd-sysctl（OpenRC 上是 sysctl 服务）
在那之前写入它，内核顺带加载它的模块。开机配置里没有设定、设的是 `skyline_cc`、或设的算法本机此刻没有注册
时，保留 `cubic`。选择结果（`fallback_cc set to …` / `fallback_cc left at …`）写在安装日志里，
`--verbose` 时也打到屏幕；`--check` 会报告新建的配置将得到哪个值。

**安装前记录。** 动手之前记下当前的拥塞控制、`net.core.default_qdisc`，以及 guard 将要维护的每块网卡的
根 qdisc，写入 `/etc/skyline-speeder/pre-install-state`（§2.5），供结束时的前后对比和
`--uninstall --restore-pre-install` 使用。这份快照只写一次，重装不会把 `skyline_cc` 误记成"原值"。

**结束摘要。** 拥塞控制与 qdisc 的前后对比（如 `skyline_cc (was bbr)`、
`mq/fq on eth0 (was cake), default fq (was cake)`；bond 这类由多块网卡承载的每块一行；没动的注明原因，如
`cake on eth0 (bandwidth 90Mbit: it shapes, so it was left alone)`）以及由谁守住它们；仍会把它们设成别的值的
sysctl 文件（按 systemd-sysctl 的规则判断开机时哪个文件生效：`/etc` 下的同名文件——包括指向 `/dev/null` 的
链接或空文件——盖过 `/run`、`/usr/lib` 下的，各文件按文件名顺序应用；`/etc/sysctl.conf` 只有被 `sysctl.d`
里的链接引用时才在开机生效）。安装器**不修改**这些文件：它们开机时先生效，随后被 `skyline-speederd` 覆盖。
最后是日常命令、几个常用参数的当前值与调参入口，以及卸载会删掉多少个包。

### 2.5 装上的文件与状态

| 路径 | 内容 |
|---|---|
| `/usr/local/sbin/skyline-speederd`、`/usr/local/bin/ssctl` | daemon 与命令行 |
| `/opt/skyline-speeder/bpf/*.bpf.o` | 三个 BPF 对象 |
| `/opt/skyline-speeder/infra/*.sh` | enable unit 调用的 `boot-enable.sh`/`boot-disable.sh`、§7 的 `run-in-skyline-cgroup.sh`，以及 `apply-guest-profile.sh`、`collect-guest-metrics.sh`、`snapshot-skyline-events.sh` |
| `/etc/systemd/system/skyline-speederd.service`、`skyline-speeder-enable.service` | systemd 的两个 unit；OpenRC 主机上是 `/etc/init.d/skyline-speederd`、`/etc/init.d/skyline-speeder-enable`（§12） |
| `/etc/skyline-speeder/speeder.toml` | 配置；只在不存在时由模板创建，之后的安装与升级从不覆盖 |
| `/etc/skyline-speeder/pre-install-state` | 安装前快照：`PRE_INSTALL_CC`、`PRE_INSTALL_QDISC`，以及 `PRE_INSTALL_QDISC_DEV`/`PRE_INSTALL_ROOT_QDISC`（一一对应、以空格分隔的网卡与根 qdisc 摘要两个列表；不知道网卡时为空）。只写一次；0.2.0 写的快照没有后两个键，升级时补上 |
| `/etc/skyline-speeder/added-packages`、`added-rustup` | 这次安装给主机装上的包与 rustup（§11.3 据此卸载） |
| `/var/log/skyline-speeder-install.log` | 安装日志 |
| `/usr/local/src/skyline-speeder` | 经引导脚本安装时的源码树 |
| `/run/skyline-speeder/` | 运行时：控制 socket `speeder.sock`、事件日志 `events.jsonl`、`state.json`（§13） |
| `/sys/fs/cgroup/skyline-speeder`、`/sys/fs/bpf/skyline-speeder` | §7 的 cgroup；bpffs 下的目录 |

### 2.6 早于 guard 的 release

`--prebuilt`/`--release` 装的是已发布的 release，可能比正在运行的 `install.sh` 旧。v0.2.0 及更早的 release
没有 guard（§9）：`ssctl enable` 本身不碰 qdisc，之后也不守住默认拥塞控制；同一 release 附带的 enable unit
（它自己的 `boot-enable.sh`）在每次挂载和每次开机时写一次 `net.core.default_qdisc=fq`，不换网卡的根
qdisc，也没有谁守住这个值。安装器据此告警（`... is a release older than the guard ...`），摘要的 qdisc 行注明
`(not managed by this release)`；此时 §6 的 G7 不适用。判断依据是 daemon 报告的版本（这些版本的
`sv version` 为空），不要看 JSON 里有没有 `guard` 键：0.3.0 起的 `ssctl` 对任何 daemon 都会打印这个键。

---

## 3. 手动分步部署（不经 `install.sh`）

只在需要逐步控制时用；一键安装就是把下面这些做完。手动安装的主机没有安装前快照和包记录，`--uninstall`
因此不会卸任何工具链，`--restore-pre-install` 也无从还原（§11.3）。全程以 root 执行：
`infra/install-guest.sh` 以 root 运行 `make bpf` 和 `cargo build`，Rust 要装在 root 名下。

```bash
# 3.1 工具链（Debian/Ubuntu）
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2   # guard 用 tc 维护网卡 qdisc
# Fedora / RHEL 系（RHEL 系的 libbpf-devel 在 CodeReady Builder 里：加 --enablerepo=crb）：
#   dnf install -y gcc make pkgconf-pkg-config clang llvm libbpf-devel elfutils-libelf-devel zlib-devel bpftool curl iproute iproute-tc
# Alpine（bpftool 在 community 仓库）：
#   apk add build-base pkgconf clang llvm libbpf-dev elfutils-dev zlib-dev linux-headers bpftool curl iproute2 bash libgcc

# Debian 12 + bookworm-backports 的 6.12 内核：linux-headers-* 把 libelf1 带到 0.192，而 bookworm 的
# libelf-dev 依赖 libelf1 (= 0.188-2.1)，上面的 apt-get 会以 "held broken packages" 失败。只换这一个包，
# 取与机上那份库同源的版本：
#   apt-cache madison libelf-dev
#   apt-get install -y libelf-dev=0.192-4~bpo12+1
# 不要用 -t bookworm-backports：那会连 curl、iproute2、bpftool、libbpf1 一起换源（实测 55 个包）。

# 3.2 Rust（rust-toolchain.toml 固定了版本，rustup 会自动遵循）
command -v cargo >/dev/null || {
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
}

# 3.3 构建并安装（不带 --confirm-install 只打印将要做的事，退出码 2）
./infra/install-guest.sh --confirm-install
```

`make bpf` 默认从本机 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`。对象是 CO-RE 的：头文件只需定义代码用到的
类型，字段偏移在加载时按运行内核修正——发布产物正是在 6.12 参考头上编译、在更新的内核上加载的。为别的
机器构建时，指定对方内核的 BTF，或直接给一份现成的头（这条路径不需要 bpftool，也不需要本机 BTF）：

```bash
make VMLINUX_BTF=/path/to/target/vmlinux bpf
make PREBUILT_VMLINUX_H=/path/to/vmlinux.h bpf
```

CO-RE 修得了偏移，修不了字段改名或删除：对象换到另一个内核上，**必须**先在目标机上跑 §4 的验证器。
`bpf/include/vmlinux.h` 是构建产物，已在 `.gitignore` 中排除。

`install-guest.sh` 安装 §2.5 表里的二进制、对象、脚本与两个 unit（OpenRC 主机上是两个 init 脚本），只在
`/etc/skyline-speeder/speeder.toml` 不存在时用 `config/speeder-guest.toml` 创建它，并把
`skyline-speederd.service` 设为开机启动；**刻意不启用** `skyline-speeder-enable.service`——挂载会改变全机
新连接的拥塞控制，是运维决定，由 §5.1 的第 ③ 步显式执行。

```bash
# 3.4 出口网卡：模板值 data0 是测试床的接口名，几乎不会匹配真实主机。
# 取 "dev" 后面那个词（`default dev wg0 scope link` 这类没有 via 的路由里它不在第 5 列）；
# 先 IPv4、再 IPv6；只取以太网设备（type 1，VLAN/bond/bridge 也是），skyline_tc 不挂在隧道上
DEV=$({ ip -o route show default; ip -6 -o route show default; } 2>/dev/null \
  | awk '$1 == "default" { for (i = 2; i < NF; i++) if ($i == "dev" && $(i + 1) != "lo") print $(i + 1) }' \
  | while read -r d; do
        if [ "$(cat "/sys/class/net/$d/type" 2>/dev/null)" = 1 ]; then echo "$d"; break; fi
    done)
test -n "$DEV"    # 失败：没有默认路由，或默认路由只走隧道——见下方说明
sed -i "s|^tc_interface = \".*\"|tc_interface = \"$DEV\"|" /etc/skyline-speeder/speeder.toml
grep '^tc_interface' /etc/skyline-speeder/speeder.toml

# 3.5 fallback_cc：挂载 skyline_cc 之前的默认算法，就是主机自己的算法（§8.1）
CC=$(sysctl -n net.ipv4.tcp_congestion_control)
[ "$CC" = skyline_cc ] || sed -i "s|^fallback_cc = \".*\"|fallback_cc = \"$CC\"|" /etc/skyline-speeder/speeder.toml
grep '^fallback_cc' /etc/skyline-speeder/speeder.toml
```

`test -n "$DEV"` 失败而默认路由存在，说明它只经 WireGuard/WARP、tun、gre、ppp 这类三层隧道离开：把
`tc_interface` 手工设成承载隧道流量的那块物理网卡（`install.sh` 此时同样保留模板值并告警）。`tc_interface`
是 VLAN、bond 或网桥时照填即可，guard 维护的是它下面的物理网卡（§9）。

然后依次执行 §4（验证）、§5（启动与挂载）、§6（门禁）。手动安装的主机如何升级见 §10.2。

---

## 4. 启动前验证

```bash
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf
```

它读入磁盘上的配置、探测内核能力，把三个 BPF 对象都过一遍内核验证器后退出：**不挂载任何程序，不写任何
sysctl**，只会建出 `/run/skyline-speeder/` 与一个空的 `events.jsonl`。stdout 是 daemon 的状态 JSON
（含 `capabilities`），全部通过时 stderr 的最后一行是 `all BPF objects passed the kernel verifier`，退出码 0。
能力不满足时它先打印状态 JSON 再以非零退出，所以失败时也能读到原因。不带 `--verify-bpf` 时只校验配置与能力。

逐项检查六项硬性能力：

```sh
out=$(skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf); rc=$?
for k in btf bpffs cgroup_v2 fq_available struct_ops fallback_cc_available; do
    printf '%s\n' "$out" | awk -v k="\"$k\":" '!f && $1 == k { v = $2; gsub(/[",]/, "", v); print v; f = 1 }' \
        | grep -qx true || echo "capability $k is false"
done
[ "$rc" -eq 0 ] && echo "verifier OK"
```

- `rack_reo_hook: false` 是**正常的**：它只表示 early-loss 停留在观测模式，不阻断部署。`capabilities.notes`
  因此在原装内核上总有一条 `early-loss remains observation-only ...`，`ssctl status` 的 ATTENTION 段落有内容
  不等于部署失败。
- `fallback_cc_available: false`（`Error: configured fallback congestion control is unavailable`）：
  `fallback_cc` 填的算法这个内核既没有注册，也没有可加载的模块 `tcp_<名字>`；`notes` 列出本机已注册的算法，
  把 `fallback_cc` 改成其中之一（通常是主机原本在用的 `bbr`），见 §8.1 与 §13.4。

---

## 5. 启动与挂载

### 5.1 启动序列（顺序不可颠倒）

```bash
# ① 启动常驻进程
systemctl enable --now skyline-speederd.service

# ② 等控制 socket —— 必须等，不能假设立即可用
timeout 60 sh -c 'until test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'

# ③ 挂载 skyline_cc 并切换默认算法（开机自启走的也是这条路径）
systemctl enable --now skyline-speeder-enable.service
```

`skyline-speederd.service` 没有 readiness 通知：进程起来的头几秒 socket 可能还不存在，此时执行 `ssctl` 会报
`connect to /run/skyline-speeder/speeder.sock (is skyline-speederd running?)`，下一行 `Caused by` 是
`No such file or directory`。这只是启动竞态，所以第 ② 步要等。

> [!IMPORTANT]
> **`skyline-speederd.service` 启动后不会自动挂载 `skyline_cc`。** 它只保持进程常驻，除了 `[rack_tuning]`
> 里声明的 tier-1 sysctl（随附模板不声明）之外不写任何 sysctl；
> 不执行第 ③ 步，新连接就一直走主机当前的默认算法，**且不报任何错误**。`skyline-speeder-enable.service`
> 正是为消除这个静默失效而存在：它的 `boot-enable.sh` 先等 socket（最多 60 秒）再执行 `ssctl enable`，
> 每次开机都挂载一次；停止它时 `boot-disable.sh` 执行 drain（§11）。

### 5.2 `ssctl enable` 做了什么

1. 校验配置与六项能力（§1.3）；
2. 第一次挂载时先把默认拥塞控制写成 `fallback_cc`（基线：挂载失败时主机停在一个确定可用的算法上），再加载并注册
   `skyline_cc`（内核 ≥ 7.1 上注册它的 struct_ops map 叫 `skyline_cc_txs`，算法名仍是 `skyline_cc`），并让 §7 的
   cgroup 里的进程建连时切到它；
3. 挂载成功之后才把 `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`——内核不接受还没注册的算法名；
4. 武装 guard 并立即做一遍完整检查（§9）：`[guard] qdisc = true`（默认）时把 `net.core.default_qdisc` 写成
   `fq`，并把出口网卡的根 qdisc 换成 `fq`（多队列网卡上是子队列全为 `fq` 的 `mq`；VLAN、bond、网桥则换它下面的
   物理网卡）；
5. 打开首轮冗余：每条 `skyline_cc` 连接的握手包与前 64 KiB 各发两份，第二份晚 10 ms（§8.1）。

不带 `--modules` 的 `ssctl enable` 沿用 daemon 当前的模块集合：执行过 `ssctl enable --all-off` 之后，要用
`--modules early-loss,adaptive-cwnd,loss-classifier,pacing` 才能回到全开（或重启 daemon）。

> [!IMPORTANT]
> **`ssctl enable` 是全机生效的。** 挂载之后这台机器上**所有**新建 TCP 连接（不只是
> `/sys/fs/cgroup/skyline-speeder` 里的进程建的）都默认使用 `skyline_cc`，guard 也一直守着这一点：手工把
> sysctl 改回去，会在 `[guard] interval_s` 秒内（默认 5 秒）被改回来，日志里还会记成"主机上有别的东西改了它"。
> 确实只想让 cgroup 里的进程走 `skyline_cc` 时：设 `[guard] interval_s = 0`（不做周期检查，重启 daemon
> 生效），每次 `enable` 之后手工把 `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`——cgroup 里的进程
> 建连时仍由 `skyline_policy` 切到 `skyline_cc`。这只维持到下一次 `enable`，**包括开机时 enable unit 执行的
> 那一次**。回退是对称的：`ssctl drain` 先把 sysctl 写回 `fallback_cc`，再等存量连接（§11.1）。

---

## 6. 验证门禁

```sh
# G1 daemon 与挂载单元均已启动
systemctl is-active skyline-speederd.service skyline-speeder-enable.service

# G2（可选）struct_ops 已注册：需要 bpftool，预编译安装的主机上没有它，以 G3 为准。
#    内核 >= 7.1 上这张 map 叫 skyline_cc_txs，下面的 grep 同样匹配
bpftool struct_ops show | grep -q skyline_cc

# G3 算法已进入内核可用列表
sysctl -n net.ipv4.tcp_available_congestion_control | grep -qw skyline_cc

# G4 已成为系统默认
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = skyline_cc ]

# G5 daemon 自述已挂载（sv 的定义见文首）
[ "$(sv enabled)" = true ]

# G6 开机自启
systemctl is-enabled skyline-speederd.service skyline-speeder-enable.service

# G7 guard 已武装，qdisc 已就位（[guard] qdisc = false 时跳过后两条；没有可维护的网卡时跳过最后一条）。
#    最后一条看的是 guard 实际维护的网卡（guard.live.devices：tc_interface 自己，
#    或它是 VLAN/bond/bridge 时下面的物理网卡），每块的根必须是 fq 或 mq/fq
[ "$(sv armed)" = true ]
[ "$(sysctl -n net.core.default_qdisc)" = fq ]
#    （读不到根 qdisc 的网卡——例如 down 了——在 JSON 里是 null，同样算失败）
q=$(ssctl status --json | grep -E '^ +"qdisc": ("|null)' | sed -E 's/.*"qdisc": "?([^",]*)"?,?$/\1/')
[ -n "$q" ] && ! printf '%s\n' "$q" | grep -vqxE 'fq|mq/fq' && echo "qdisc OK:" $q
```

`ssctl status` 报告标题右侧就是结论：`● ACCELERATING`（已挂载且是全机默认）、`○ ATTACHED, NOT DEFAULT`
（挂上了但新连接绕开了它，等于没生效）、`○ STANDBY`（没挂载）。不是 `ACCELERATING` 时，最下面的
ATTENTION 段落逐条写明问题与处理动作。所有 `ssctl` 子命令默认打印给人看的报告，脚本解析一律加 `--json`；
应答 `ok` 为 `false` 时 `ssctl` 的退出码是 1。

G7 后两条失败时看 `ssctl status` 的 DRIFT GUARD 段落（`--json` 里是 `guard` 的 `notes` 与 `last_error`，
打印整段：`ssctl status --json | sed -n '/^    "guard": {/,/^    }/p'`）：根 qdisc 是 `htb`/`tbf`/`netem` 等
刻意搭建的整形结构、或设了带宽的 `cake` 时，guard 按设计不动它；`tc_interface` 是根为 `noqueue`、下面又找
不到物理网卡的隧道时没有可维护的网卡——这两种都不是部署失败（§9）。

**G8 重启存活**（强烈建议）：`systemctl reboot`，重启后重新执行 G1–G7。

**G9 实流验证**：确认真实连接确实走了这个算法，而不只是 sysctl 值正确。

```bash
ss -tin | grep -c skyline_cc          # 应 > 0（已建立的老连接不会中途切换，等新连接）
ssctl flows                           # CONNECTIONS 段落：coverage 一行与逐条连接
```

`ssctl flows` 就是 daemon 替你跑的那条 `ss`：coverage 一行写"主机上多少条 TCP 连接跑在 `skyline_cc` 上"，
下面逐条列出 RTT、cwnd、pacing、交付速率与重传占比（按已发字节排序，最多 50 条，其余只显示一行
条数 "N more, not shown"），再下面是生效中的参数与算法决策计数器。

---

## 7. cgroup 前提（M1 动态 RTO）

`skyline_policy` 挂在 cgroup `/sys/fs/cgroup/skyline-speeder` 上：**只有这个 cgroup 里的进程建立的连接**
才经过这段 BPF 代码，进程不迁移进去不报任何错误。受影响的只有 M1 tier-2 动态 RTO 调节；`skyline_cc`
（全局 struct_ops）与 `skyline_tc`（接口级 TC）**不受**此限制。

**动态 RTO 默认关闭。** daemon 每次启动都把它清零，配置文件里的 `[rack_rto]` 在启动时**不会**下发（随附的
模板没有这一段，即关闭）。打开它：

```bash
ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 --warmup-samples 4   # 下发命令行给的值
ssctl reset-rack-rto      # 或者：下发配置文件里 [rack_rto] 的值
ssctl set-rack-rto --disable   # 无条件关闭
```

两者都是绝对覆盖，每次 daemon 重启后要重新执行一次。参数含义、RTO 上限（`--rto-max-*`，需要内核 ≥ 6.15）
与建议值见 `docs/usage.md` 第五节。

迁移进程：

```bash
/opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <服务启动命令...>
```

需要 root 只是为了写 `cgroup.procs`；经 `sudo` 调用时，被包装的命令以 `$SUDO_USER` 的身份运行。systemd 服务
用 drop-in 迁移（`+` 前缀提权，写 `cgroup.procs` 需要 root）：

```ini
# /etc/systemd/system/<unit>.d/10-skyline-cgroup.conf
[Unit]
After=skyline-speederd.service skyline-speeder-enable.service

[Service]
ExecStartPost=+/bin/sh -c 'echo $MAINPID > /sys/fs/cgroup/skyline-speeder/cgroup.procs'
```

诊断（`ssctl flows` 的 DYNAMIC RTO 段落；`--json` 里是 `rack_rto.stats`）：

| 现象 | 结论 |
|---|---|
| `established_cb`（*connections seen*）为 0 | 没有进程的连接经过 `skyline_policy` → 目标进程不在 cgroup 内（RTO 调节打开时 ATTENTION 段落也会写明） |
| `established_cb` 增长，`subscribe_ok`、`rtt_callbacks` 为 0 | RTO 调节没有打开（daemon 启动后默认关闭）→ 执行上面的 `set-rack-rto` / `reset-rack-rto` |
| `subscribe_err` 非零 | 订阅 RTT 回调失败 → 检查内核的 sockops 能力 |
| `subscribe_ok` 非零、`applied` 为 0、`rejected` 非零 | 内核拒绝下发的值 → 取值超出内核对 RTO 的合法区间 |
| `rto_max_rejected` 增长、`rto_max_applied` 为 0 | 内核 < 6.15，没有 `TCP_RTO_MAX_MS`；RTO 上限不生效，下限照常工作 |

---

## 8. 配置与调参

### 8.1 配置文件要点

配置在 `/etc/skyline-speeder/speeder.toml`，模板是仓库里的 `config/speeder-guest.toml`（发布产物里叫
`config/speeder.toml`）。每个字段的类型、范围与何时生效见 `docs/02-interface-reference.md` 第 6 节。

- 四个模块（`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`）默认全部启用；
- `[redundancy]` 默认开启：`first_kib = 64`、`delay_ms = 10`（§5.2）；
- `[rack_tuning]` 整段留空：全局 sysctl 的默认值不由 `skyline-speederd` 接管；
- `[rack_rto]`、`[retransmit_dscp]` 默认关闭，而且写进文件也**不会在 daemon 启动时下发**，要
  `ssctl reset-rack-rto` / `reset-retransmit-dscp`（下发文件里的值）或 `set-*`（下发命令行的值）。
  `dscp_value` 是占位值，启用前必须由网络侧给出具体编码（`docs/usage.md` 第六节）；
- `[guard]` 默认 `interval_s = 5`、`qdisc = true`（§9）；
- `fallback_cc`：`skyline_cc` 不在时新连接用的算法。每次 enable 挂载前、`ssctl drain`、enable unit 的 ExecStop
  都写它；daemon 自己停止时不写。可以填内核已注册的算法（`/proc/sys/net/ipv4/tcp_available_congestion_control`），
  也可以填内核能自动加载的模块 `tcp_<名字>`（0.4.3 起；更早的版本只认已注册的）；
- `runtime.tc_interface` 必须是承载流量的以太网设备：`skyline_tc` 的重传 DSCP 标记与首轮冗余按以太网帧头
  解析报文，daemon 不会把它挂到 WireGuard/WARP、tun、gre、ppp 这类三层隧道上（原因写在 ATTENTION 段落里，
  `--json` 里是 `capabilities.notes`；`set-retransmit-dscp` 随之失败）。guard 维护的也是这块网卡的根 qdisc。

> [!CAUTION]
> 0.5.0 起 daemon 拒绝它不认识的键：写错段落的键（例如把 `loss_inflation_max_ratio` 写进 `[adaptive_cwnd]`）
> 或拼错的键，会让 `--validate-only` 和 daemon 启动都失败，报错逐个列出这些键，键名属于别的段落时指明是哪一段。
> **0.4.x 及更早的 daemon 静默忽略它们**，`--validate-only` 也照样通过。无论哪个版本，改完之后都用
> `ssctl flows` 的 PARAMETERS IN FORCE 段落核对生效的值。

### 8.2 在线调参

```bash
ssctl set-module-config --cruise-pacing-gain 1.3     # M2/M3/M4 系数与上限
ssctl reset-module-config                            # 回到配置文件里的值
```

- `set-module-config` 是**绝对覆盖**：每次发送全部 17 个系数，命令行没写的取 `ssctl` 内置默认值，不是配置文件里
  的值（全新安装时两者相同；从 0.3.x 及更早升级、保留了旧配置的主机上不同）。`--disable-prr-pacing`、
  `--disable-auto-pacing` 同理：不写就是打开。任何一个参数越界，整条命令被拒绝，已生效的配置不变。
- 新系数写进当前没被指向的那个配置槽，再把配置序号指过去；每条连接在 RTT 边界把它复制进自己的状态，之后只读
  自己的副本，所以同一轮里读到的不会新旧混杂，脚本里连着下发两次也一样（0.4.x 在一个 RTT 内连续下发两次时会
  改写仍有连接在读的槽）。`skyline_cc` 未挂载时改的系数在下一次 `enable` 时生效。
- `set-rack-rto` 同样是绝对覆盖，但直接改写那一份 RTO 参数，不经过配置槽（§7）。
- 四档现成配方（保守、默认、激进、高随机丢包）与每个参数调大调小的后果见 `docs/usage.md` 第四节。

### 8.3 持久修改

在线修改只在 daemon 的内存里，重启即丢。要长期生效：

```bash
cp /etc/skyline-speeder/speeder.toml /etc/skyline-speeder/speeder.toml.bak
vi /etc/skyline-speeder/speeder.toml
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only    # 不通过就别重启
systemctl restart skyline-speederd.service
```

重启 daemon 会连带重启处于 active 的 enable unit（`Requires=`）：它的 ExecStop 先 drain（最多 60 秒），新 daemon
起来后重新挂载。之后按 §8.1 的提示核对 PARAMETERS IN FORCE。

---

## 9. guard：守住拥塞控制与 qdisc

`default_qdisc` 只影响之后新建的 qdisc，网卡的根 qdisc 往往在它生效前就建好了，所以光改 sysctl 改不到网卡；而
VPS 上常见的"一键 BBR"脚本把 `bbr` 与 `cake`/`fq_pie` 写进 `/etc/sysctl.d`，之后任何一次 `sysctl --system`
都会悄悄把拥塞控制翻回 `bbr`，新连接从此绕开 `skyline_cc` 且没有任何报错。因此从 `enable` 到下一次 `drain`，
`skyline-speederd` 独占三项设置：`net.ipv4.tcp_congestion_control`，以及 `[guard] qdisc = true`（默认）时的
`net.core.default_qdisc` 与受管网卡的根 qdisc。`enable` 时立即检查一遍，此后每 `[guard] interval_s` 秒（默认 5）
再查一次，被改了就改回，并在日志里记一行 `guard: <项> <原值> -> <新值> (...)`。guard 与安装器都**不修改**
`/etc/sysctl.conf` 与 `/etc/sysctl.d` 下的任何文件。`drain` 立即解除守护，且不动任何 qdisc。完整规则见
`docs/02-interface-reference.md` 第 9 节。

**受管网卡。** 就是 `runtime.tc_interface` 自己；它的根是内核默认的 `noqueue`（VLAN、bond、网桥）时，报文真正
排队的是下面物理网卡的根 qdisc，guard 顺着 `/sys/class/net/<网卡>/lower_*` 往下找（最多 4 层，每块只看一次），
管理有 `device` 链接的物理/virtio 网卡（bond 的从属网卡、VLAN 的真实网卡、网桥的物理端口），纠正记录里写明路径，
如 `eth0 (under bond0) root qdisc cake -> fq`。没有 `device` 链接的端口（虚拟机的 tap、容器的 veth、ifb）一律不碰。
根是 `noqueue`、下面又找不到网卡的（内核 WireGuard 设备、只挂着 tap 的网桥），只记一条
`<网卡> is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked`。
tun、PPP 设备（OpenVPN、wireguard-go、WARP 客户端、PPPoE）有自己的默认 qdisc，被当作 `tc_interface` 时 guard
管的就是它自己的根。受管网卡列在 `ssctl status` 的 DRIFT GUARD 段落（`--json` 里是 `guard.live.devices`）。

**只替换没有配置值得保留的 qdisc**（`pfifo_fast`、`fq_codel`、未整形的 `cake`、`fq_pie` 等）。刻意搭建的整形、
分类、卸载类 qdisc（`htb`、`tbf`、`netem`、`mqprio` 等）、设了带宽或 `autorate-ingress` 的 `cake`、运维亲手在
物理网卡上设成的 `noqueue`、以及不认识的种类一律不动，只在 `notes` 里说明，例如
`eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone`。

判定命令：

```bash
ssctl status                                                          # DRIFT GUARD 段落
ssctl status --json | sed -n '/^    "guard": {/,/^    }/p'            # guard 整段 JSON
journalctl -u skyline-speederd.service | grep 'guard:'
```

| 现象 | 结论与处置 |
|---|---|
| `cc_restored` 持续增长，日志反复出现 `guard: tcp_congestion_control ... (something else on this host changed it)` | 主机上有东西在反复写这个 sysctl（常见：`sysctl --system`/`sysctl -p` 重新应用了 `/etc/sysctl.conf` 或 `/etc/sysctl.d/*.conf` 里的 `bbr`）。guard 会一直改回，生效不受影响；要根除，找出并删掉那一行（安装摘要列出了这些文件） |
| `notes` 含 `looks deliberate; left alone`，或 `is not a kind the guard knows is safe to replace` | 网卡根 qdisc 是整形、分类、卸载类，或设了带宽的 `cake`，或不认识的种类：按设计不动，以免毁掉整形配置 |
| `notes` 含 `is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard` | `tc_interface` 是根为 `noqueue`、下面没有物理网卡的设备，guard 不检查任何根 qdisc。把 `runtime.tc_interface` 改成承载流量的物理网卡并重启 `skyline-speederd` |
| `capabilities.notes` 含 `is not an Ethernet device` | `tc_interface` 不是以太网设备，`skyline_tc` 不挂载（`set-retransmit-dscp` 随之失败）。处置同上 |
| `last_error`/`notes` 含 `tc is not installed` | 安装 `iproute2`（Fedora/RHEL 上是 `iproute-tc`），然后 `ssctl enable` |
| `notes` 含 `the last replace failed; retrying in <N> s` | 一次替换没有生效，周期检查按退避重试（30 秒起，`interval_s` 更长时按它，每次失败翻倍，最长 300 秒）；原因看 `last_error`。排除后想立即重试执行 `ssctl enable`。`interval_s = 0` 时写作 `retried on the next ssctl enable` |
| `last_error` 含 `left alone: default_qdisc is` | `default_qdisc` 不是 `fq`（写入被拒，或有东西马上又改了它）；多队列网卡上新建的 `mq` 会按它建子队列，所以 guard 不动根 qdisc。先查谁在改 `default_qdisc` |
| `last_error` 含 `did not finish within 10000ms and was killed`，或 `a previous tc has not exited yet (rtnl lock held?); skipped` | `tc` 卡在内核的 rtnl 锁上：被杀掉的 `tc` 退出之前，guard 的每个 `tc` 都立即跳过，不会越积越多；锁释放后下一次检查自动恢复。持续出现时查是谁长时间持有 rtnl（例如卡住的网卡驱动） |
| `enabled` 为 `true` 而 `armed` 为 `false` | drain 超时之后的正常状态（§11.1）：struct_ops 仍挂着，guard 已解除。要恢复加速执行 `ssctl enable` |

**退出。** 只让它管拥塞控制：`[guard] qdisc = false`；不要周期检查：`interval_s = 0`（`enable` 仍会做一次）。
两者都改配置文件后重启 `skyline-speederd` 才生效。

---

## 10. 升级

### 10.1 一键安装的主机

再运行一次安装器即可，当初带了 `--source`/`--no-enable` 的照样带上（从源码构建的主机不带 `--source` 重跑，会
换成已发布的产物）。经引导脚本安装的主机重跑同一条一键命令。安装器的做法：

1. 新的二进制、对象与 unit 先就位，新对象通过内核验证器之后才动正在运行的 daemon；
2. `skyline-speederd.service` 处于 active 时，执行 `systemctl restart skyline-speederd.service`。enable unit
   处于 active 时随之重启：它的 ExecStop（`boot-disable.sh`）执行 `ssctl drain`——先解除 guard、写回
   `fallback_cc`，再等存量连接至多 60 秒（经 SSH 必然走到超时，无害）——新 daemon 起来后重新挂载；
3. `skyline_cc` 是用裸 `ssctl enable` 挂载的主机（enable unit 不是 active，而当前默认拥塞控制是 `skyline_cc`）
   没有 ExecStop 替它 drain，daemon 自己停止时又刻意不写 sysctl，所以安装器在重启前自己执行
   `timeout 75 ssctl drain --timeout 60`（失败只记日志）。重启后不带 `--no-enable` 时由 enable unit 挂载（此后
   开机自启），带 `--no-enable` 时再执行一次 `ssctl enable`（仍是手工挂载）。只看当前默认值、不看旧 daemon 的
   `enabled`，是因为一次超时的 drain 已经写回了 `fallback_cc` 却留着 `enabled: true`——那台主机是被有意摘除的。

升级之后：

- 已有的 `/etc/skyline-speeder/speeder.toml` 不会被覆盖，缺少的新配置段按默认值生效。升级到 0.5.0 及以后时，
  文件里有新 daemon 不认识的键（写错段落、拼错）的，安装器在验证器这一步停下，报错列出这些键，旧 daemon 继续
  运行（§10.3）：按报错改好文件再重跑；
- 用 `ssctl` 做的在线修改只在旧 daemon 的内存里，重启后不保留；安装器把旧的 `ssctl status --json` 写进了安装
  日志，需要时照着重新下发；
- 摘要里显示 `upgraded <旧> -> <新>`（v0.2.0 及更早的二进制没有 `--version`，显示
  `from a release without --version (v0.2.0 or older)`）；用 §0.2 的判据确认运行的是新版本；
- 各版本的升级注意事项见 `CHANGELOG.md` 里对应的 *Upgrading from ...* 小节。

### 10.2 手动安装的主机

`install-guest.sh` 只替换文件，**不会重启**正在运行的 daemon，旧版本继续留在内存里。装完先跑 §4 的验证器（不影响
正在运行的 daemon），通过后：

```bash
# enable unit 是 active（按 §5 挂载的主机）：restart 会连带重启它，它的 ExecStop 自己 drain，无需手动。
# 它不是 active（skyline_cc 由裸 ssctl enable 挂载）时才手动 drain：单独停止 daemon 不会写回 fallback_cc。
# 经 SSH 执行的 drain 必然超时并返回非零（§11.1），属预期。
systemctl is-active -q skyline-speeder-enable.service || ssctl drain --timeout 60 || true
systemctl restart skyline-speederd.service
```

enable unit 此前不是 active、而这台主机应当挂载 `skyline_cc` 的，restart 之后按 §5.1 的第 ③ 步启动它（或再执行一次
`ssctl enable`，但那样开机不会自动挂载）。

### 10.3 验证器失败、或要装回旧版本

安装器在验证器**之前**就替换了文件：验证器失败时它停下，旧 daemon 继续运行，但磁盘上已是新文件，下一次重启或
开机运行的就是新文件。先看 §4 的输出排除原因再重跑；要回到旧版本，用 `--release <旧 tag>` 安装那个版本——配置
文件原样保留。0.4.x 及更早的 daemon 会忽略它不认识的新键；0.5.0 及以后的会拒绝它们，要先删掉报错列出的键。

---

## 11. 摘除、停止与卸载

### 11.1 优雅摘除：`ssctl drain`

```bash
ssctl drain --timeout 60
```

依次：解除 guard，并在仍持有它的锁时把默认拥塞控制写回 `fallback_cc`（此后不再有检查把 `skyline_cc` 写回去）→
cgroup 里的进程不再被切到 `skyline_cc` → 停止首轮冗余 →
等存量 `skyline_cc` 连接自然结束（默认 300 秒，`--timeout` 覆盖）→ 全部结束后注销 struct_ops。新连接此后落回
`fallback_cc`。`drain` 不动 qdisc，`default_qdisc` 与网卡根 qdisc 保持 `fq`。

> [!IMPORTANT]
> **经 SSH 执行的 `drain` 必然走到超时**：你自己的 SSH 连接就是一条 `skyline_cc` 流，不会在等待期间结束。
> 超时时 `ssctl` 打印 `drain timeout expired; struct_ops remains attached, but the default congestion control is
> already back on <fallback_cc> ...` 并以 1 退出——按这行消息识别，属预期。此时 sysctl 早已写回、guard 已解除，
> "新连接不再使用 `skyline_cc`"已经成立；`skyline_cc` 本身仍保持注册（`sv enabled` 仍为 `true`、`sv armed`
> 为 `false`），直到之后某次 drain 等到连接归零、或 daemon 停止。要恢复加速执行 `ssctl enable`；要让 drain
> 真正等到归零并注销，从串口控制台或一条不走本机 `skyline_cc` 的通道执行它。

daemon 一次只处理一个请求：`drain` 等待期间，其他 `ssctl` 调用都排在它后面，不要在 drain 期间轮询状态。

M1 动态 RTO 与重传 DSCP 标记**不受 drain 影响**，要单独关：

```bash
ssctl set-rack-rto --disable      # 无条件关闭 RTO 调节（reset-rack-rto 是回到配置文件的设置）
ssctl reset-retransmit-dscp       # 回到配置文件的设置：除非 [retransmit_dscp] 显式开启，即关闭
```

### 11.2 停止

```bash
systemctl stop skyline-speeder-enable.service skyline-speederd.service
```

停止 daemon（等价于 SIGTERM）会先停 guard 线程，再注销全部 struct_ops 与 BPF 链接；它本身**不写任何 sysctl**
（刻意如此）。enable unit 处于 active 时，停止 daemon 会先停这个 unit，由它的 ExecStop（`boot-disable.sh`）写回
`fallback_cc`：它先 `ssctl drain`，之后只在默认仍是 `skyline_cc`（daemon 已经不在、drain 没能执行）时才自己写一次——
顺序反过来的话，仍处于武装状态的 guard 会把刚写回的值又改成 `skyline_cc`。用裸 `ssctl enable` 挂载的主机上，
停止之前先 `ssctl drain`。

`systemctl stop` 不改开机设置，下次开机仍会挂载。要持久摘除：`systemctl disable --now skyline-speeder-enable.service`
（daemon 照常常驻，开机不再挂载）。

### 11.3 卸载：`install.sh --uninstall`

```bash
./install.sh --uninstall                         # 在源码树里；经引导脚本安装的主机：
/usr/local/src/skyline-speeder/install.sh --uninstall
```

它先 drain（同时解除 guard），停用并删除两个 unit（或两个 OpenRC 脚本）、二进制与 `/opt/skyline-speeder`，
**保留** `/etc/skyline-speeder`（重装时配置还在）、安装日志、引导脚本的源码树与
`/sys/fs/{cgroup,bpf}/skyline-speeder`，然后：

1. **切到 bbr + fq**（默认）：`net.ipv4.tcp_congestion_control=bbr`、`net.core.default_qdisc=fq`，并把
   `runtime.tc_interface` 解析出来的网卡（guard 武装时维护的正是这些，与 `[guard] qdisc` 是否关过无关）的根 qdisc
   确认成 `fq`（多队列网卡是子队列全为 `fq` 的 `mq`）。已经是 `fq` 的不动；`htb`/`tbf`/`netem`/设了带宽的 `cake`
   这类看起来特意建的原样留下、只告警并给出手工命令。内核没有 bbr 时先 `modprobe tcp_bbr`，仍然没有则退回
   `PRE_INSTALL_CC`，再退 cubic、reno，并告警用了哪一个。**只在本次运行时生效**：不写也不改 `/etc/sysctl.d`
   下的任何文件，重启后仍由那些文件决定，结束时会提示。
2. **卸掉安装时装上的包**：只卸 `/etc/skyline-speeder/added-packages` 里记录的（安装时取包管理器执行前后已安装
   集合的差集，含顺带拉进来的依赖，不含主机本来就有的包；apt 还与这次请求被接受的计划取交集，排除并行跑着的
   unattended-upgrades 装的包）。没有这份记录的主机——手动安装的，或在安装器开始记录之前（v0.3.0 之前）装的——
   这一步什么也不卸。护栏：
   - `iproute2`（Fedora/RHEL 上 `iproute`、`iproute-tc`）、`curl`、`ca-certificates`、`tar`（Alpine 上还有
     `bash`）永不移除；
   - **这几个包递归依赖的东西也永不移除**——保留 `curl` 却删掉它底下的 `libcurl4t64`，apt 只会用"连 curl 一起删"
     解决，结果整条工具链一个都卸不掉；
   - dpkg 优先级 `required`/`important`（按架构分别判断）与 dnf 的 protected packages 永不移除；
   - 先出计划：apt 用 `apt-get -s --purge remove`，计划里出现清单之外的 `Purg`/`Remv` 时，把被依赖的那个包单独留下
     并告警说明是谁需要它，其余照卸、重新出计划（最多三轮）；dnf 用 `rpm -e --test` 同样处理（最多十轮），由
     `dnf remove` 执行并关掉 `clean_requirements_on_remove`；dnf 的记录只取这次请求所在的事务，并始终排除
     `gpg-pubkey`。apk 对记录里加进 `/etc/apk/world` 的名字执行 `apk del`，还被别的包需要的依赖由 apk 自己保留。
     仍凑不出不越界的方案时**一个都不卸**，改为打印手工命令。
   - rustup 只在 `/etc/skyline-speeder/added-rustup` 存在时移除。这一步的任何失败都只是告警：此时本体已经卸完。

安装结束时指南里"卸载还会删掉 N 个包"的 N 按同一套护栏计算，与实际卸载一致。主机上什么都没装时以 0 退出并说明
`nothing of Skyline Speeder is installed here`；只剩 `/etc/skyline-speeder` 时不改任何 sysctl 或 qdisc。

卸载后的判据：

```sh
! command -v skyline-speederd >/dev/null
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = bbr ] && [ "$(sysctl -n net.core.default_qdisc)" = fq ]
```

（内核没有 bbr 时第二条换成告警里写明的那个算法；`--restore-pre-install` 时换成快照里的值。）

### 11.4 `--restore-pre-install`

把 §11.3 的第 1 步换成按 `/etc/skyline-speeder/pre-install-state` 还原：拥塞控制与 `default_qdisc`；快照里记有
网卡及其根 qdisc 时，逐块还原根 qdisc 的**种类**——只在它仍是本项目留下的 `fq`/`mq/fq` 时才动，且只还原种类、用
默认参数（例如 `cake` 的带宽设置不会回来）；`mq` 下混有多种 qdisc 的只告警不还原；两个列表长度不同时一块都不还原。
还原 `mq/<种类>` 时临时把 `default_qdisc` 设成该种类，`tc qdisc del dev <网卡> root` 让内核按它重建自己的 `mq`
（对 guard 留下的、由 `tc` 建的 `mq` 执行 `replace root mq` 是空操作），删不掉（内核自己的句柄 0 的 `mq`）时才
`tc qdisc replace dev <网卡> root mq`，然后设回 `default_qdisc`、重新读取核对。没有快照（手动安装的主机）时只告警，
cc 与 qdisc 保持现状。

### 11.5 不用安装器卸载

```bash
ssctl drain --timeout 60 || true
systemctl disable --now skyline-speeder-enable.service skyline-speederd.service
rm -f /etc/systemd/system/skyline-speederd.service /etc/systemd/system/skyline-speeder-enable.service \
      /usr/local/sbin/skyline-speederd /usr/local/bin/ssctl
rm -rf /opt/skyline-speeder
systemctl daemon-reload
```

OpenRC：先后对 `skyline-speeder-enable`、`skyline-speederd` 执行 `rc-service <服务> stop` 与
`rc-update del <服务> default`，再删除 `/etc/init.d/` 下的两个脚本。拥塞控制与 qdisc 需要自己改回。

### 11.6 已知行为：struct_ops 在连接结束前不会消失

注销或卸载之后，`bpftool struct_ops show` 可能仍列出 `skyline_cc`（内核 ≥ 7.1 上是 `skyline_cc_txs`）。**这不是
失败**：struct_ops map 只要还有 socket 引用就不会被内核释放，与内核模块"in use"时 `rmmod` 失败是同一类引用计数
行为。判据（内核注册表本身是干净的）：

```bash
# 应恰好输出 1（启用中）或 0（已停用），绝不会因为残留 map 而出现重复注册
sysctl -n net.ipv4.tcp_available_congestion_control | tr ' ' '\n' | grep -c '^skyline_cc$'
ss -tin | grep -c skyline_cc       # 仍在引用它的连接数
```

残留 map 在这些连接自然结束后、或重启后由内核回收。**不要**强制 detach 仍被活跃 socket 使用的 struct_ops。

---

## 12. OpenRC 主机（Alpine）

一键安装照样适用，区别只有这些：

- **装 musl 版产物**（§2.1），外加它运行时要的 `libelf`、`zlib`、`zstd-libs`、`libgcc`；两个 OpenRC 脚本也在产物
  里。`--release v0.4.0` 这类没有 musl 版的版本会直接中止。源码构建在 1 vCPU 上约 10 分钟：Rust 的 musl 目标默认
  静态链接，而 Alpine 把 zlib 的静态库放在没人装的 `zlib-static` 里，链接会以 `cannot find -lz` 失败，仓库的
  `.cargo/config.toml` 因此让 musl 上的构建改为动态链接。
- **两个服务**是 `packaging/openrc/` 下的脚本，装到 `/etc/init.d/`，与两个 systemd unit 一一对应：
  - `skyline-speederd` 由 `supervise-daemon` 托管：异常退出 2 秒后重拉，60 秒内 5 次仍失败就放弃（对应
    `Restart=on-failure`）；停止时先发 SIGTERM，留 30 秒让它注销 struct_ops；输出经 `logger` 进 syslog（Alpine 上
    是 `/var/log/messages`）；启动前建好 `/run/skyline-speeder`（`RuntimeDirectory=` 的对应物，**必须是
    `socket_path`、`events_path`、`state_path` 的父目录**）、`/sys/fs/bpf/skyline-speeder` 与
    `/sys/fs/cgroup/skyline-speeder`。
  - `skyline-speeder-enable`：`need skyline-speederd`，start 执行 `boot-enable.sh`，stop 执行 `boot-disable.sh`。与
    systemd 的 `Requires=` + `After=` 效果相同，但 OpenRC 是在**后台**重启它：`rc-service skyline-speederd restart`
    返回时 `boot-enable.sh` 可能还在等 socket，检查前要等它结束（安装器升级时会等）。
- **cgroup v2**：Alpine 上只有 OpenRC 的 `cgroups` 服务会挂它，而它默认不在任何 runlevel 里。`skyline-speederd`
  声明了 `need cgroups`，每次开机由 OpenRC 先启动它；安装时安装器自己启动一次。`/etc/rc.conf` 的 `rc_cgroup_mode`
  设成 `hybrid`/`legacy` 时 v2 不在这个路径上，安装器中止并说明。
- Alpine 默认没有 `sudo`，以 root 执行；安装器最后打印的指南已经按本机写好命令。

| systemd | OpenRC |
|---|---|
| `systemctl restart skyline-speederd` | `rc-service skyline-speederd restart` |
| `systemctl enable --now skyline-speeder-enable.service` | `rc-update add skyline-speeder-enable default && rc-service skyline-speeder-enable start` |
| `systemctl disable --now skyline-speeder-enable.service` | `rc-service skyline-speeder-enable stop && rc-update del skyline-speeder-enable default` |
| `systemctl stop <服务>` | `rc-service <服务> stop` |
| `systemctl is-active <服务>`（G1） | `rc-service <服务> status`（输出 `status: started`） |
| `systemctl is-enabled <服务>`（G6） | `test -e /etc/runlevels/default/<服务>` |
| `journalctl -u skyline-speederd.service` | `grep skyline-speederd /var/log/messages` |

---

## 13. 可观测性与排障

### 13.1 故障速查

| 现象 | 先看 |
|---|---|
| 安装在某一步失败 | 屏幕上的 `error` 行与日志尾部；完整输出在 `/var/log/skyline-speeder-install.log`（§0.3） |
| `Error: configured fallback congestion control is unavailable` | §13.4 |
| 验证器拒绝 `skyline_cc`，报错指向参数个数 | 内核是 6.1/6.6 这类旧分支，不支持（§1.3） |
| `ssctl` 报 `connect to /run/skyline-speeder/speeder.sock` | daemon 没起来或还在启动（§5.1）；`journalctl -u skyline-speederd.service` |
| 状态是 `○ STANDBY` | 没挂载：`systemctl enable --now skyline-speeder-enable.service` 或 `ssctl enable`（§5） |
| 状态是 `○ ATTACHED, NOT DEFAULT` | 默认拥塞控制被改走了：看 DRIFT GUARD 段落与 `guard:` 日志（§9） |
| 重启后没有挂载 | enable unit 没有启用：G6（§6） |
| `rack_rto.stats` 一直是 0 | §7 的诊断表 |
| 网卡 qdisc 不是 `fq` | DRIFT GUARD 段落的 `notes`（§9） |
| `drain` 返回非零 | 经 SSH 时是预期的超时（§11.1） |
| 卸载后 `bpftool` 仍看到 `skyline_cc` | §11.6 |
| 连接卡几十秒、大文件中断、4K 卡 2K 不卡 | `docs/usage.md` 的"附：视频卡顿 / 大文件传输中断怎么查" |
| 版本不对 | `skyline-speederd --version`（磁盘）与 `sv version`（内存）不一致 = 还没重启到新版本（§0.2） |

### 13.2 报告、计数器与状态文件

- `ssctl status`：是否挂载、全机默认、guard、内核能力、全局 sysctl、daemon 运行时长；`ssctl flows`：被加速的
  连接、生效中的参数与首轮冗余、算法决策计数器（WHAT THE ALGORITHM DID 段落，`--json` 里是 `status.metrics`）。`metrics` 在没有已加载的
  struct_ops 时为 `null`——从未挂载、一次 drain 完成之后、daemon 重启之后都是——每次重新挂载从零计起。完整字段见
  `docs/02-interface-reference.md` 第 4、4.1 节。
- 那些计数器**自挂载起累计**。WHAT THE ALGORITHM DID 段落最后的 `last …` 两行是最近约一分钟的同一组比率
  （`--json` 里是 `status.metrics_recent`），用来区分"现在仍在发生"和"挂载以来某个时候发生过"。
  `guardrail trips` 是队列时延/ECN 护栏触发的轮数占检查轮数的比例，`cwnd at the cap` 是 cwnd 顶在
  `max_cwnd_packets` 上的 ACK 占全部 ACK 的比例——后者是上限在起作用，不是拥塞。0.4.x 把两者合在一个
  `guardrail hits` 里、都除以 ACK 数，读不出是哪一个；要在 0.4.x 上拆开，数事件日志里 `"type":9` 的行（护栏
  触发，§13.3），其余就是顶到上限的 ACK。
- `/run/skyline-speeder/state.json` 只在 daemon 启动时和每次成功处理请求后改写，**不是**实时状态；监控请调用
  `ssctl status --json`。
- `ssctl snapshot <路径>` 由 daemon 写文件，相对路径按 daemon 的工作目录解析。systemd 主机上 daemon 跑在
  `ProtectSystem=strict` 与私有 `/tmp` 里：大部分路径对它只读，它写进 `/tmp` 的文件你看不到，写到
  `/run/skyline-speeder/` 下最稳妥。

### 13.3 事件日志

事件环形缓冲区落盘到 `runtime.events_path`（默认 `/run/skyline-speeder/events.jsonl`），一行一个 JSON，记录状态
切换、护栏触发、配置代际切换等；字段与事件码见 `docs/02-interface-reference.md` 第 8 节。它在 `/run`（内存
tmpfs）里，大小受 `runtime.events_max_mib` 限制（默认 8 MiB，满了轮转为 `events.jsonl.1`，最多约占两倍）；设为
`0` 关闭。0.2.0 之前的版本没有这个上限，可能写满 `/run`，让 Docker 等依赖 `/run` 的服务失败：在这类版本上用
`truncate -c -s 0 /run/skyline-speeder/events.jsonl` 回收空间，不要 `rm`——daemon 仍持有文件句柄，删除后空间不会释放。

### 13.4 `configured fallback congestion control is unavailable`

校验（安装器的 *Running the kernel verifier* 一步、`--validate-only`、`ssctl enable`）在加载任何 BPF 对象之前就停下
了：`fallback_cc` 填的算法这个内核既没有注册，也没有可加载的模块 `tcp_<名字>`，能力报告的 `notes` 列出本机已注册的
算法。改 `/etc/skyline-speeder/speeder.toml` 里的 `fallback_cc` 为其中之一（通常是主机原本在用的 `bbr`），再重跑安装
命令或 `systemctl restart skyline-speederd`。0.4.3 之前的版本只认**已注册**的算法：xanmod 这类默认 bbr、把 cubic 编成
模块的内核上，模板的 `cubic` 开机后并未注册，安装因此失败，按同样的办法改成 `bbr` 即可。不要用 `modprobe tcp_cubic`
绕过：daemon 只在启动时探测一次，重启后 cubic 没有加载，开机自动挂载就会失败。

### 13.5 双栈监听与地址族

双栈监听 socket 上的 IPv4 连接以 IPv4-mapped `AF_INET6`（`::ffff:a.b.c.d`）的形态出现，而不是纯 `AF_INET`——
这是大多数不显式绑定地址族的服务的常态，不是异常。Skyline Speeder 的地址族判断同时接受两种形态。

---

## 14. 相关文档

| 文档 | 内容 |
|---|---|
| [README.md](README.md) / [README.zh.md](README.zh.md) | 项目简介、实测性能、快速开始 |
| [docs/usage.md](docs/usage.md) | 新手向：每个开关和参数、四档配方、动态 RTO、DSCP、卡顿排查 |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` 命令、线协议、状态字段、配置字段、事件码、guard 规则 |
| [docs/03-design.md](docs/03-design.md) | 各模块的设计与取舍 |
| [docs/04-performance-report.md](docs/04-performance-report.md) | 测试床数据、有效性边界与局限（第 8 节） |
| [research/experiments/README.md](research/experiments/README.md) | 在自己的环境里复现性能测试 |
| [CHANGELOG.md](CHANGELOG.md) | 每个版本的变化与 *Upgrading from ...* 升级注意事项 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 构建、验证器、硬性不变量、发版流程 |
