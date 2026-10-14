# CLAUDE.md

面向 Claude Code / AI 编码助手的仓库约定。修改本仓库前请通读。

## 项目性质

Skyline Speeder 由三个 eBPF 程序 + 一个 Rust 用户态控制面组成，其中
`skyline_cc` 是通过 **struct_ops 注册的内核拥塞控制算法**。这意味着：

- BPF 验证器是硬门禁——通不过就是加载失败，没有中间状态；
- BPF 与用户态共享一份 ABI（`bpf/include/skyline_abi.h`），**任何一侧单独改动
  结构体布局都会导致数据被静默误解释**；
- 缺陷代价是全机 TCP 行为异常，不是一个 500 响应。

## 构建与验证

```bash
make bpf                          # 生成 vmlinux.h 并编译三个 CO-RE 对象
cargo build --workspace --release
infra/build-musl.sh               # musl（Alpine）版控制面，在 Alpine 3.21 容器里构建；发布流水线与 CI 用的就是它
make check                        # 脚本的 bash -n、实验框架的 Python 单测、cargo fmt --check、cargo check；不跑 cargo test
make test                         # 实验框架的 Python 单测 + cargo test --workspace
# 实验框架的测试要 Python ≥ 3.11（tomllib），只用标准库。make venv 经 pyenv 用系统的 python3 建 .venv，
# 系统 python3 低于 3.11 时这个 venv 同样跑不了；直接指定一个 ≥ 3.11 的解释器：
# make check PYTHON=/path/to/python3.11

# 不挂载、不写 sysctl 地过一遍内核验证器 —— 提交前必做
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

`make bpf` 默认从**本机** `/sys/kernel/btf/vmlinux` 读取类型信息；也可以用
`make VMLINUX_BTF=<路径> bpf` 指定另一份 BTF，或用
`make PREBUILT_VMLINUX_H=<路径> bpf` 直接提供已生成好的头（这条路径**不需要
bpftool，也不需要 /sys/kernel/btf/vmlinux**，供容器/发布流水线使用）。
`bpf/include/vmlinux.h` 是构建产物，**不提交**。

本机生成的 vmlinux.h 编译时带 `-DBPF_NO_KFUNC_PROTOTYPES`：bpftool 7.5 起把运行内核导出的每个
kfunc 原型都写进头里，其中一个与发行版 libbpf 的 `bpf_helpers.h` 对不上就编不过（Fedora 43：
6.17 内核的 `bpf_stream_vprintk` 五个参数，libbpf 1.6.1 声明四个）。所以 **BPF 源码调用的 kfunc
必须自己声明**（见 `skyline_cc.bpf.c` 顶部），不能靠 vmlinux.h。`PREBUILT_VMLINUX_H` 路径不带
这个宏，发布产物逐字节不变。

vmlinux.h 只需要**定义**代码用到的类型，不必来自最终运行的那个内核，但**不能比它新**：bpftool
生成的头带 `preserve_access_index`，每处字段访问都生成 CO-RE 重定位记录，偏移由 libbpf 在
加载时按运行内核的 BTF 修正；运行内核没有的字段，CO-RE 补不出来，用较新内核的头编出的对象可能
在 6.12 上加载失败——发布因此固定用 6.12 参考头编译（`release.yml`）。实测过的是 6.12 的头：
在 6.12.63 上编、在 6.19.14 上加载，以及在 6.19.14 上用 6.12 的头编、在 6.12.63 上加载
（`CHANGELOG.md` 0.1.0 的 *Verified*）。复现脚本见 `infra/kernel/core-portability.sh`。
**改动数据面后若要依赖这条性质，必须重跑该脚本**——CO-RE 能修偏移，修不了字段改名或删除。

## 硬性不变量

| 标识符 | 位置 | 约束 |
|---|---|---|
| 算法名 `"skyline_cc"` | `bpf/skyline_cc.bpf.c`（两张 struct_ops map 的 `.name`）、`bpf/skyline_policy.bpf.c`（`TCP_CONGESTION` setsockopt）、`bpf/skyline_tc.bpf.c`（`skyline_runs_skyline_cc()`）、`crates/skyline-common` 的 `SKYLINE_CC_NAME`，以及 `infra/boot-enable.sh`、`infra/boot-disable.sh`、`infra/apply-guest-profile.sh`、`install.sh` 里的字面量 | 算法注册名，**≤ 15 字符**（`TCP_CA_NAME_MAX` 为 16 含 NUL），各处一字不差。daemon 写 sysctl 时对不上会报错；policy 与 tc 两处对不上则静默失效（setsockopt 的返回值被忽略；首轮冗余只剩 SYN-ACK）。struct_ops 的 map 名是另一个命名空间 |
| `SKYLINE_ABI_VERSION` 及另外三个 ABI 版本号 | `bpf/include/skyline_abi.h` 与 `crates/skyline-common/src/lib.rs` | 布局变更必须递增管这段结构的版本号，两侧一起改：`SKYLINE_ABI_VERSION` 管 `struct skyline_config`、`skyline_flow_state`、`skyline_metrics`，`SKYLINE_RTO_TUNING_ABI_VERSION`、`SKYLINE_RETRANSMIT_DSCP_ABI_VERSION`、`SKYLINE_REDUNDANCY_ABI_VERSION` 各管自己那段（`docs/02-interface-reference.md` 第 5 节）。加载时**没有**谁拒绝不匹配的一对。`SKYLINE_ABI_VERSION` 不一致时每条连接复制配置（`skyline_adopt_config()`）都失败：此后新建的连接，`skyline_cong_control` 在每个 ACK 上直接返回——cwnd 停在内核的初始窗口（RTO 之后停在 `tcp_enter_loss()` 留下的大小），也没有谁设 pacing；`cong_control` 已经取代了内核自己的增长逻辑，所以这不是交还给内核。不报错，也没有计数器：`ssctl status --json` 里 `active_flows` 在涨、`metrics.ack_events` 不动。`struct skyline_config` 大小变了的话，daemon 写配置槽时就会报错。它也管 `struct skyline_metrics`：Rust 侧按布局读计数器，大小不符时计数器全是 0。DSCP 标记与首轮冗余的版本不符时跳过并递增各自的 `abi_mismatch`。递增版本号，是让半新半旧的组合停摆，而不是误读结构体 |
| `cong_control` 4 参数签名 | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`，内核 >= 6.10 才有 |
| struct_ops 双变体 `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | 两张 map 挂**同一组回调**，只有 TX_START 的入口不同（`cwnd_event` / 7.1 起的 `cwnd_event_tx_start`）；daemon 按内核 BTF 只创建其中一张。**给一张加回调，另一张也要加**，否则行为随内核版本悄悄不同 |
| 安装路径 `/opt|/etc|/run/skyline-speeder` | 配置、unit（含 `packaging/openrc/` 的两个 OpenRC 脚本）、脚本 | 三处必须一致 |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **必须是 `socket_path`（及 `events_path`、`state_path`）的父目录**：unit 带 `ProtectSystem=strict`，`/run` 下只有这个目录对 daemon 可写 |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC 下的 `RuntimeDirectory=`：同样**必须是 `socket_path`（及 `events_path`、`state_path`）的父目录**，daemon 启动先打开的就是那里的事件日志 |

> `RuntimeDirectory` 与 `socket_path` 的耦合是历史上真实踩过的坑：一次批量重命名
> 把 `RuntimeDirectory` 改成了 `skyline` 而配置里是 `/run/skyline-speeder/`，
> 当时 daemon 起来但控制 socket 永远不出现。现在的 daemon 自己创建事件日志、socket、状态文件
> 的父目录（`main.rs` 的 `Daemon::new`、`serve`、`persist_state`），而 `ProtectSystem=strict`
> 让 `/run` 除 `RuntimeDirectory=` 之外只读，所以按 unit 与代码推断，同样的错配今天表现为 daemon
> 打开事件日志时失败、被 systemd 反复重启——不变量不变，症状变了。改动任一侧务必对照另一侧。

## 命名雷区

`bpf/skyline_cc.bpf.c` 中的 **`PRR-SSRB`** 是 RFC 6937 的标准术语
（Slow Start Reduction Bound），**不是本项目旧名 SSR 的残留，不要改名**。
批量重命名时务必用词边界保护它。

## 静默失效点（本项目最大的风险来源）

以下三处出问题时**不报任何错误**，只是不生效。改动相关代码时格外小心：

1. **cgroup 未迁移**：`skyline_policy` 挂在 `/sys/fs/cgroup/skyline-speeder`，
   进程不在其中就不会经过该 BPF 程序。表现为 `rack_rto.stats.established_cb`（`ssctl flows`
   的 connections seen）恒为 0。诊断表见 `DEPLOY.md` §7。

2. **未执行 `enable`**：`skyline-speederd` 启动后**不会自动挂载** `skyline_cc`，
   必须显式 `ssctl enable`。`skyline-speeder-enable.service` 正是为消除这个
   静默失效而存在——**不要以"install-guest.sh 里没启用它"为由删除它**，那是刻意的：
   挂载会改变全机新连接的拥塞控制，属于运维决策，由 `install.sh` 显式启用。

   `ssctl enable` 自己就是全机生效的：附加 struct_ops 成功后它会把
   `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`。**sysctl 的写入顺序是不变量**——
   `enable` 必须在附加之后写（内核拒绝未注册的算法名），`drain` 必须在注销之前写回
   `fallback_cc`（否则会出现"注销一个正被当作默认算法的 struct_ops"）。daemon 里这个 sysctl
   只有一个写入点：`crates/skyline-speederd/src/main.rs` 的 `set_default_congestion_control`
   （`BpfRuntime::load` 附加前写 `fallback_cc` 作基线、`enable`、guard、`drain`），从 `enable`
   到 `drain` 只有 daemon 写它。这之外有三个写入者，各有前提：`infra/boot-disable.sh` 先 drain，
   之后只在默认仍是 `skyline_cc` 时才写，是 daemon 已死时的兜底，不是重复（它若抢在 drain 之前写，
   仍处于 armed 的 guard 会把 `skyline_cc` 写回去）；实验矩阵的 `infra/apply-guest-profile.sh` 在
   `drain --timeout 0` 之后写自己要的算法；`install.sh --uninstall` 在 drain 并移除 daemon 之后写回
   bbr（或 `--restore-pre-install` 的安装前值）。`infra/boot-enable.sh` 刻意不写任何 sysctl。

   **挂载之后被悄悄改回，同样是静默失效**：VPS 上"一键 BBR"脚本留在 `/etc/sysctl.d`
   的 `bbr` 一旦被 `sysctl --system` 重新应用，新连接就全部绕开 `skyline_cc`，不报任何错。
   所以从 `enable` 到 `drain`，guard（`crates/skyline-speederd/src/guard.rs`，配置
   `[guard]`）守住三项：默认拥塞控制，以及 `[guard] qdisc = true`（默认）时的
   `net.core.default_qdisc = fq` 与 `runtime.tc_interface` 的根 qdisc（`fq`，多队列网卡上
   是子队列全为 `fq` 的 `mq`；它是根为 `noqueue` 的 VLAN/bond/网桥时，改为它下面有
   `device` 链接的物理网卡，tap/veth 从不碰）。从 `enable` 到 `drain`，这三项同样只有 daemon
   一个写入者（drain 之后 `apply-guest-profile.sh` 与 `install.sh --uninstall` 会设自己的
   qdisc）——`boot-enable.sh` 过去开机时写一次 `default_qdisc=fq`，现在不写了，**不要加回去**
   （它改不到网卡已有的根 qdisc，还会和 daemon 各说各话）。相关不变量：
   - `enable` 写完 `skyline_cc` 之后才武装 guard；`drain` 先解除武装，并在**仍持有 guard
     的锁**时写回 `fallback_cc`，否则一次周期检查可能在 drain 写回之后又把 `skyline_cc`
     写回去。drain 超时报错也保持解除（实验矩阵 `drain --timeout 0` 后自己设 qdisc）。
   - daemon 停止时先停掉并 join guard 线程，再让 `BpfRuntime` drop 注销 struct_ops
     （`impl Drop for Daemon`），否则已武装的检查会去写一个内核已不认识的 `skyline_cc`。
   - **daemon 停止不写任何 sysctl。** 维护者明确不要"停止时写回 `fallback_cc`"，不要加；
     写回由 `drain` 和 enable unit 的 `ExecStop` 负责。
   - guard 只替换没有配置值得保留的 qdisc（`pfifo_fast`/`fq_codel`/未整形的 `cake`/`fq_pie`
     等），`htb`/`tbf`/`netem`/`mqprio` 等分类、整形、卸载类、设了带宽或 `autorate-ingress`
     的 `cake`、`noqueue` 和不认识的种类一律不动——替换它们会悄悄毁掉运维的整形配置。

3. **`tc_interface` 指向错误网卡**：`config/speeder-guest.toml` 的默认值 `data0` 是测试床的
   接口名，真实主机上几乎必然不匹配。`install.sh` 每次运行（安装与升级都算），只要配置里还是
   `tc_interface = "data0"` 这一行，就改写成默认路由（先 IPv4、再 IPv6）上的第一块以太网网卡；
   已经是别的值（运维改过）就不动。默认路由只走 WireGuard/tun 这类隧道时不改写，保留模板值并
   告警——`skyline_tc` 只挂在以太网设备上。
   guard 维护的也是这块网卡的根 qdisc：指错了，真正的出口网卡保持原来的 qdisc，而被指到
   的那块网卡被换成 `fq`。

## 控制面语义

- `set-module-config` / `set-rack-rto` / `set-retransmit-dscp` / `set-redundancy` 都是**绝对覆盖**
  语义：每次调用发送完整字段集合，不是增量更新。对应的 `reset-*` 恢复配置文件默认值。
- 配置切换走**双槽 + 配置序号 + 每连接副本**：daemon 把第 N 份配置写进
  `config_slots[N & 1]`，**写完才**把 `config_seq` 改成 N。每条连接在开始时复制一次（完成握手的
  那个 ACK 先经过 `cong_control`，`init` 再复制一次并从头开始）；还没有副本的连接每个 ACK 都再试；
  已有副本的连接只在新一轮开始、且序号变了时才复制。一轮不是一个 RTT：`skyline_new_round()` 在
  `prior_delivered` 追上本轮开始时的 `delivered + packets_out` 时结束一轮，窗口打满的流约两个 RTT
  （开始或配置切换后的第一轮约一个）。复制前后序号变了就丢掉这份副本；此后每个 ACK 只读自己的
  副本。所以一轮之内读到的不会新旧混杂，连续两次下发也一样。
  **修改配置下发路径时必须维持这两条**：先写槽、后写序号；BPF 只读自己的副本。
  否则会出现撕裂读（0.4.x 没有副本和序号，一个 RTT 内连续下发两次就会改写仍有连接在读的槽）。

## guest 配置的强约束

`config/speeder-guest.toml` 中 `[rack_tuning]` **必须整段保持注释**。该配置由实验
矩阵（`infra/apply-guest-profile.sh`）逐 case 精确控制 `tcp_recovery` /
`tcp_reordering` / `tcp_early_retrans`；若 `skyline-speederd` 在此接管这几个全局 sysctl，
daemon 重启会把 per-case 精确值悄悄覆盖回默认值。

`crates/skyline-common/src/lib.rs` 有 `guest_config_never_owns_global_sysctls`
测试守护这条约束——**不要为了让某个用例通过而删改它**。

## 不要提交的内容

见 `.gitignore`。特别注意：

- `bpf/include/vmlinux.h`（机器相关的构建产物）
- `target/`、`build/`、`*.bpf.o`
- 渲染后的 `user-data`（含运维真实 SSH 公钥），只提交 `user-data.template`
- `known_hosts`、密钥、填好的实验室清单
- 任何 `/home/<用户名>` 形式的绝对路径

## 文档地图

| 文档 | 读者与内容 |
|---|---|
| `README.md` / `README.zh.md` | 用户，按上手顺序：这是什么 → 内核版本要求 → 快速开始 → 原理与三者的设计差别 → 配置（默认运行的功能总表、每个功能一节、逐行解释 `ssctl status` / `ssctl flows`）→ 仓库结构 → 从源码构建 → 文档 → 实测性能 → 参与贡献 → 许可（中英内容必须一致） |
| `DEPLOY.md` | **唯一的部署与运维手册**，面向自动化 agent，人也照它操作：前置条件、安装器的每一步、验证门禁、cgroup、guard、升级、卸载、排障 |
| `docs/usage.md` | 新手：每个开关与参数、四档配方、动态 RTO、DSCP、连接卡顿排查 |
| `docs/02-interface-reference.md` | `ssctl` 命令、线协议、状态字段与计数器、配置字段、事件码、guard 规则 |
| `docs/03-design.md` | 各模块的机制、取舍与开放问题 |
| `docs/04-performance-report.md` | 双 VM 测试床的数据、有效性边界与局限 |
| `research/experiments/README.md` | 实验框架与复现方法 |
| `research/experiments/single-ended/README.md`、`RESULTS.md` | 单条公网链路上 bbr / tcp-brutal / skyline_cc 的现场对比：方法、逐轮数据、原始记录与复现脚本；README「实测性能」的图与数字出自这里。不是测试床数据，不进 `docs/04-performance-report.md` |
| `CHANGELOG.md` | 每个版本的变化与 *Upgrading from ...* 升级注意事项 |
| `CONTRIBUTING.md` | 本文件约定的英文版，面向外部贡献者 |

## 文档同步要求

| 改动 | 需同步 |
|---|---|
| ABI 结构体 | `skyline_abi.h` + `crates/skyline-common` + 管这段结构的版本号（`SKYLINE_ABI_VERSION` / `SKYLINE_RTO_TUNING_ABI_VERSION` / `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` / `SKYLINE_REDUNDANCY_ABI_VERSION`，两侧一起递增）+ `docs/02-interface-reference.md` 第 5 节 |
| `ssctl` 命令/字段 | `docs/02-interface-reference.md` |
| 配置字段 | `config/*.toml` + `docs/02-interface-reference.md` §6 |
| 安装流程 | `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh`（面向用户的部分还有两份 README 与 `docs/usage.md`） |
| README 里任何面向用户的内容 | **`README.md`（英文）和 `README.zh.md`（中文）必须同时改** |
| 新增功能、改默认值，或改 `ssctl status` / `ssctl flows` 的报告文字（`crates/ssctl/src/view.rs`） | 两份 README「配置」开头的功能总表、该功能的小节，以及末尾逐行解释两份报告的两张表（`cargo test -p ssctl -- --nocapture renders_for_review` 打印两份报告） |
| 硬性不变量 / 贡献流程 | `CONTRIBUTING.md`（本文件的不变量表在那里有一份面向外部贡献者的英文版）+ `.github/PULL_REQUEST_TEMPLATE.md` 的 Invariants 清单 |
| 算法行为 | `docs/03-design.md`，性能声明须有 `docs/04-performance-report.md` 数据支撑 |
| 发版（打 `v*` tag） | `Cargo.toml` 的 `[workspace.package] version` + `Cargo.lock` 里三个 workspace crate 的版本（CI、发版与 musl 构建都带 `--locked`，对不上就构建失败）+ `CHANGELOG.md` 的 `## [<版本>] - <日期>` 小节（没有 `[Unreleased]` 小节，直接写版本小节）与文末的对比链接——`release.yml` 校验 tag 恰为 `v<版本>` 且该小节存在，否则拒绝构建 |

## 预编译版本：每个 PR 都要回答"要不要发版"

一键安装（`scripts/bootstrap.sh` → `install.sh`，不带参数）默认装的是 **GitHub 上最新
release 的预编译产物**，不是 `main`。合并进 `main` 的改动，在下一个 release 之前**不会**
到达默认安装的用户——这同样是静默的：不报错，只是没生效。例外是跑不了产物的主机（不是 x86_64、
glibc 低于 2.38、musl 低于 1.2.5，或 musl 主机上最新 release 没有 musl 版）：安装器改为构建引导脚本
取来的源码树，默认就是 `main`（`install.sh` 的 `prebuilt_blocker`，`DEPLOY.md` 第 2.1 节）。

所以**每个 PR 都必须**在描述的 "Prebuilt release" 一节（见 PR 模板）写明是否需要随之发布
预编译版本，并给出理由：

| 改动落在 | 要发版吗 | 原因 |
|---|---|---|
| `bpf/`、`crates/`、`config/speeder-guest.toml`、`packaging/`（systemd unit 与 OpenRC 脚本）、`release.yml` 打进产物的 `infra/*.sh` | **要** | 它们就在产物里，不发版默认安装的用户拿不到（OpenRC 脚本自 v0.4.1 进产物，Alpine 默认装 musl 版产物，安装器优先用产物里的那两个） |
| `install.sh`、`scripts/bootstrap.sh` | 通常不要 | 一键安装每次都从 `main`（引导脚本的 `--ref`）取安装器，合并即对之后的一键安装生效；但已装主机上留着的那份不会自己更新：`bootstrap.sh --uninstall` 在 `/usr/local/src/skyline-speeder` 已有源码树时直接执行树里的 `install.sh`，照安装结束时的提示用树里的 `install.sh` 升级或卸载也一样。新安装器依赖产物里还没有的东西时除外，那就先发版 |
| 文档、实验框架、CI、只影响源码构建的部分 | 不要 | 不进产物 |

要发版时，合并后按上表"发版"一行走（版本号 + CHANGELOG 小节 + 打 `v*` tag），改过数据面的
先重跑 `infra/kernel/core-portability.sh`。可以攒几个 PR 一起发，但 PR 里要写明"随下一版发布"，
不能不提。

## 性能声明纪律

**不要在文档里写没有测试数据支撑的性能数字。** 正式性能结论只能来自通过
`infra/preflight.sh formal` 的双 VM 测试床（默认门槛：≥ 16 个逻辑 CPU、≥ 32 GiB 内存、工作目录
≥ 120 GiB 可用磁盘、可写的 `/dev/kvm` 与 vmx/svm；搭建见 `research/experiments/README.md`）；资源
不足的机器只能用于
代码/verifier 检查和短时冒烟，**不得据此得出性能结论**。

## 风格

- Rust：`cargo fmt` 强制，`make check` 会校验。
- BPF C：4 空格缩进。
- Shell：`set -euo pipefail`；POSIX sh 脚本（`scripts/bootstrap.sh`、`infra/boot-enable.sh`）是
  `set -eu`，`infra/boot-disable.sh` 刻意只用 `set -u`，停止时不能因为某一步失败而中断。
- 注释解释**为什么**。本仓库大量注释记录的是验证器限制、内核行为与踩过的坑，
  这类注释比代码本身更有价值，不要为了简洁删除它们。
