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
make check                        # cargo fmt --check + cargo check + 单元测试
make test                         # 含 cargo test

# 不留运行状态地过一遍内核验证器 —— 提交前必做
skyline-speederd --config config/speeder.toml --validate-only --verify-bpf
```

`make bpf` 默认从**本机** `/sys/kernel/btf/vmlinux` 读取类型信息；也可以用
`make VMLINUX_BTF=<路径> bpf` 指定另一份 BTF，或用
`make PREBUILT_VMLINUX_H=<路径> bpf` 直接提供已生成好的头（这条路径**不需要
bpftool，也不需要 /sys/kernel/btf/vmlinux**，供容器/发布流水线使用）。
`bpf/include/vmlinux.h` 是构建产物，**不提交**。

vmlinux.h 只需要**定义**代码用到的类型，不必来自最终运行的那个内核：bpftool 生成的
头带 `preserve_access_index`，每处字段访问都生成 CO-RE 重定位记录，偏移由 libbpf 在
加载时按运行内核的 BTF 修正。已双向实测（6.12.63 ↔ 6.19.14），复现脚本见
`infra/kernel/core-portability.sh`。**改动数据面后若要依赖这条性质，必须重跑该脚本**
——CO-RE 能修偏移，修不了字段改名或删除。

## 硬性不变量

| 标识符 | 位置 | 约束 |
|---|---|---|
| `.name = "skyline_cc"` | `bpf/skyline_cc.bpf.c` | 算法注册名，**≤ 15 字符**（`TCP_CA_NAME_MAX` 为 16 含 NUL） |
| `SKYLINE_ABI_VERSION` | `bpf/include/skyline_abi.h` | 布局变更必须递增，用户态据此拒绝加载不匹配的对象 |
| `cong_control` 4 参数签名 | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`，内核 >= 6.10 才有 |
| 安装路径 `/opt|/etc|/run/skyline-speeder` | 配置、unit、脚本 | 三处必须一致 |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **必须与 `socket_path` 的父目录同名**，否则 socket 建不出来 |

> `RuntimeDirectory` 与 `socket_path` 的耦合是历史上真实踩过的坑：一次批量重命名
> 把 `RuntimeDirectory` 改成了 `skyline` 而配置里是 `/run/skyline-speeder/`，
> daemon 起来但控制 socket 永远不出现。改动任一侧务必对照另一侧。

## 命名雷区

`bpf/skyline_cc.bpf.c` 中的 **`PRR-SSRB`** 是 RFC 6937 的标准术语
（Slow Start Reduction Bound），**不是本项目旧名 SSR 的残留，不要改名**。
批量重命名时务必用词边界保护它。

## 静默失效点（本项目最大的风险来源）

以下三处出问题时**不报任何错误**，只是不生效。改动相关代码时格外小心：

1. **cgroup 未迁移**：`skyline_policy` 挂在 `/sys/fs/cgroup/skyline-speeder`，
   进程不在其中就不会经过该 BPF 程序。表现为 `rack_rto.stats.applied` 恒为 0。
   诊断决策树见 `DEPLOY.md` §7。

2. **未执行 `enable`**：`skyline-speederd` 启动后**不会自动挂载** `skyline_cc`，
   必须显式 `ssctl enable`。`skyline-speeder-enable.service` 正是为消除这个
   静默失效而存在——**不要以"install-guest.sh 里没启用它"为由删除它**，那是刻意的：
   挂载会改变全机新连接的拥塞控制，属于运维决策，由 `install.sh` 显式启用。

   `ssctl enable` 自己就是全机生效的：附加 struct_ops 成功后它会把
   `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`。**sysctl 的写入顺序是不变量**——
   `enable` 必须在附加之后写（内核拒绝未注册的算法名），`drain` 必须在注销之前写回
   `fallback_cc`（否则会出现"注销一个正被当作默认算法的 struct_ops"）。这个 sysctl
   只有 `crates/skyline-speederd/src/main.rs` 一处所有者，`infra/boot-enable.sh`
   刻意不再写它；`infra/boot-disable.sh` 里那次写入是 daemon 已死时的兜底，不是重复。

3. **`tc_interface` 指向错误网卡**：`config/speeder-guest.toml` 的默认值是测试床的
   接口名，真实主机上几乎必然不匹配。`install.sh` 会自动探测默认路由网卡改写，
   但仅在首次安装时（不覆盖运维已编辑的配置）。

## 控制面语义

- `set-module-config` / `set-rack-rto` 是**绝对覆盖**语义：每次调用发送完整字段
  集合，不是增量更新。对应的 `reset-*` 恢复配置文件默认值。
- 配置切换走**双槽 + 代际计数器**：新系数写入非活跃槽并递增代际，每条连接只在
  RTT 边界切换，避免同一轮 ACK 处理内读到新旧混杂的值。**修改配置下发路径时必须
  维持这个不变量**，否则会出现撕裂读。

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

## 文档同步要求

| 改动 | 需同步 |
|---|---|
| ABI 结构体 | `skyline_abi.h` + `crates/skyline-common` + `SKYLINE_ABI_VERSION` |
| `ssctl` 命令/字段 | `docs/02-interface-reference.md` |
| 配置字段 | `config/*.toml` + `docs/02-interface-reference.md` §6 |
| 安装流程 | `docs/01-deployment-guide.md` + `DEPLOY.md` + `install.sh` + `scripts/bootstrap.sh` |
| README 里任何面向用户的内容 | **`README.md`（英文）和 `README.zh.md`（中文）必须同时改** |
| 硬性不变量 / 贡献流程 | `CONTRIBUTING.md`（本文件的不变量表在那里有一份面向外部贡献者的英文版）|
| 算法行为 | `docs/03-design.md`，性能声明须有 `docs/04-performance-report.md` 数据支撑 |

## 性能声明纪律

**不要在文档里写没有测试数据支撑的性能数字。** 正式性能结论只能来自满足
`research/experiments/README.md` 资源门槛的双 VM 测试床；资源不足的机器只能用于
代码/verifier 检查和短时冒烟，**不得据此得出性能结论**。

## 风格

- Rust：`cargo fmt` 强制，`make check` 会校验。
- BPF C：4 空格缩进。
- Shell：`set -euo pipefail`。
- 注释解释**为什么**。本仓库大量注释记录的是验证器限制、内核行为与踩过的坑，
  这类注释比代码本身更有价值，不要为了简洁删除它们。
