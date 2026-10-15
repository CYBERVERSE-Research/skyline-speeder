# CLAUDE.md

面向 Claude Code / AI 编码助手的仓库约定。本文件每次会话都会加载，只放全仓库通用的约定和索引；各目录的细则写在
该目录自己的 `CLAUDE.md` 里，Claude Code 读写那个目录下的文件时才加载。修改本仓库前请通读本文件；**跨目录的改动**
（批量重命名、换 sysctl 的写入者、改 ABI）开工前，把涉及目录的 `CLAUDE.md` 都读一遍——没读过的细则不会自己出现。

| 文件 | 细则 |
|---|---|
| `bpf/CLAUDE.md` | vmlinux.h 的来源与 kfunc 声明、CO-RE 可移植性；算法名、ABI 版本号、`cong_control` 签名、struct_ops 双变体的完整约束 |
| `crates/CLAUDE.md` | 全局 sysctl 的写入顺序与写入者、guard 的不变量、控制面语义（绝对覆盖；双槽 + 配置序号 + 每连接副本） |
| `config/CLAUDE.md` | guest 配置的 `[rack_tuning]` 必须整段注释；`tc_interface` 的默认值与 `install.sh` 的改写规则 |
| `packaging/CLAUDE.md` | 安装路径、`RuntimeDirectory=` / `checkpath` 与 `socket_path` 的耦合 |
| `infra/CLAUDE.md` | 开机脚本与全局 sysctl、随产物发布的脚本、CO-RE 可移植性脚本 |
| `docs/CLAUDE.md` | 文档地图：每份文档的读者与内容 |

细则只写在它所属目录的 `CLAUDE.md` 里；本文件对每条只留一句摘要和指向，两处不一致时以目录里的为准，并把摘要改对。

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

`bpf/include/vmlinux.h` 是构建产物，**不提交**。`make bpf` 从哪里取 BTF、`PREBUILT_VMLINUX_H`、为什么 kfunc 必须
自己声明、vmlinux.h 为什么不能比运行内核新（改动数据面后要重跑 `infra/kernel/core-portability.sh`），见
`bpf/CLAUDE.md`。

## 硬性不变量

"约束"一列是摘要；完整约束、失效时的表现与理由在"细则"一列指向的文件里，改动任何一项之前先读它。

| 标识符 | 位置 | 约束 | 细则 |
|---|---|---|---|
| 算法名 `"skyline_cc"` | `bpf/` 的三个程序、`crates/skyline-common` 的 `SKYLINE_CC_NAME`，以及 `infra/` 三个脚本与 `install.sh` 里的字面量 | 算法注册名，**≤ 15 字符**（`TCP_CA_NAME_MAX` 为 16 含 NUL），各处一字不差；policy 与 tc 两处对不上时静默失效 | `bpf/CLAUDE.md` |
| `SKYLINE_ABI_VERSION` 及另外三个 ABI 版本号 | `bpf/include/skyline_abi.h` 与 `crates/skyline-common/src/lib.rs` | 布局变更必须递增管这段结构的版本号，两侧一起改。加载时**没有**谁拒绝不匹配的一对，不一致时静默停摆、不报错 | `bpf/CLAUDE.md` |
| `cong_control` 4 参数签名 | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`，内核 >= 6.10 才有 | `bpf/CLAUDE.md` |
| struct_ops 双变体 `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | 两张 map 挂**同一组回调**：**给一张加回调，另一张也要加**，否则行为随内核版本悄悄不同 | `bpf/CLAUDE.md` |
| 安装路径 `/opt|/etc|/run/skyline-speeder` | 配置、unit（含 `packaging/openrc/` 的两个 OpenRC 脚本）、脚本 | 三处必须一致 | `packaging/CLAUDE.md` |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **必须是 `socket_path`（及 `events_path`、`state_path`）的父目录** | `packaging/CLAUDE.md` |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC 下的 `RuntimeDirectory=`：同样必须是这几个路径的父目录 | `packaging/CLAUDE.md` |

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

   `ssctl enable` 自己就是全机生效的，**sysctl 的写入顺序与写入者是不变量**；挂载之后被"一键 BBR"悄悄改回同样是
   静默失效，所以从 `enable` 到 `drain` 由 guard 守住默认拥塞控制、`net.core.default_qdisc` 与出口网卡的根 qdisc。
   **daemon 停止不写任何 sysctl**，`infra/boot-enable.sh` 也不写——都不要加。完整的顺序、各写入者的前提与 guard 的
   不变量见 `crates/CLAUDE.md`。

3. **`tc_interface` 指向错误网卡**：`config/speeder-guest.toml` 的默认值 `data0` 是测试床的接口名，真实主机上
   几乎必然不匹配；指错了，`skyline_tc` 挂不到真正的出口网卡，guard 还会把被指到的那块网卡的根 qdisc 换成 `fq`。
   `install.sh` 何时改写它，见 `config/CLAUDE.md`。

## 控制面语义与 guest 配置

- `set-*` 都是**绝对覆盖**语义；配置切换**先写槽、后写序号**，BPF 只读每条连接自己的副本。完整规则见
  `crates/CLAUDE.md`「控制面语义」——修改配置下发路径时必须维持。
- `config/speeder-guest.toml` 中 `[rack_tuning]` **必须整段保持注释**，`guest_config_never_owns_global_sysctls`
  测试守护这条约束——**不要为了让某个用例通过而删改它**。原因见 `config/CLAUDE.md`。

## 不要提交的内容

见 `.gitignore`。特别注意：

- `bpf/include/vmlinux.h`（机器相关的构建产物）
- `target/`、`build/`、`*.bpf.o`
- 渲染后的 `user-data`（含运维真实 SSH 公钥），只提交 `user-data.template`
- `known_hosts`、密钥、填好的实验室清单
- 任何 `/home/<用户名>` 形式的绝对路径
- 只属于一台机器或一个人的 agent 文件：`.claude/` 与任何一级的 `CLAUDE.local.md`（本文件和各目录的 `CLAUDE.md`
  是公开的，里面同样不写主机地址、账号、令牌或个人路径）

## 文档同步要求

每份文档的读者与内容见 `docs/CLAUDE.md` 的文档地图。

| 改动 | 需同步 |
|---|---|
| ABI 结构体 | `skyline_abi.h` + `crates/skyline-common` + 管这段结构的版本号（`SKYLINE_ABI_VERSION` / `SKYLINE_RTO_TUNING_ABI_VERSION` / `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` / `SKYLINE_REDUNDANCY_ABI_VERSION`，两侧一起递增）+ `docs/02-interface-reference.md` 第 5 节 |
| `ssctl` 命令/字段 | `docs/02-interface-reference.md` |
| 配置字段 | `config/*.toml` + `docs/02-interface-reference.md` §6 |
| 安装流程 | `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh`（面向用户的部分还有两份 README 与 `docs/usage.md`） |
| README 里任何面向用户的内容 | **`README.md`（英文）和 `README.zh.md`（中文）必须同时改** |
| 新增功能、改默认值，或改 `ssctl status` / `ssctl flows` 的报告文字（`crates/ssctl/src/view.rs`） | 两份 README「配置」开头的功能总表、该功能的小节，以及末尾逐行解释两份报告的两张表（`cargo test -p ssctl -- --nocapture renders_for_review` 打印两份报告） |
| 硬性不变量 / 贡献流程 | 不变量所属目录的 `CLAUDE.md`（本文件只留摘要）+ `CONTRIBUTING.md`（这些约定面向外部贡献者的英文版）+ `.github/PULL_REQUEST_TEMPLATE.md` 的 Invariants 清单 |
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

各目录的 `CLAUDE.md` 不进产物（`release.yml` 按文件名逐个打包），只改它们不需要发版。

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
