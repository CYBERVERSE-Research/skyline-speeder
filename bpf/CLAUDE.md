# bpf/CLAUDE.md

`bpf/` 三个 eBPF 程序的细则。Claude Code 读写本目录下的文件时自动加载本文件；全仓库的约定（不变量一览、静默
失效点、文档同步、发版）在仓库根目录的 `CLAUDE.md`。配置下发（双槽 + 配置序号 + 每连接副本）的完整规则在
`crates/CLAUDE.md`「控制面语义」：BPF 侧只读每条连接自己的副本。

## vmlinux.h 与 kfunc 声明

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

## CO-RE 可移植性

vmlinux.h 只需要**定义**代码用到的类型，不必来自最终运行的那个内核，但**不能比它新**：bpftool
生成的头带 `preserve_access_index`，每处字段访问都生成 CO-RE 重定位记录，偏移由 libbpf 在
加载时按运行内核的 BTF 修正；运行内核没有的字段，CO-RE 补不出来，用较新内核的头编出的对象可能
在 6.12 上加载失败——发布因此固定用 6.12 参考头编译（`release.yml`）。实测过的是 6.12 的头：
在 6.12.63 上编、在 6.19.14 上加载，以及在 6.19.14 上用 6.12 的头编、在 6.12.63 上加载
（`CHANGELOG.md` 0.1.0 的 *Verified*）。复现脚本见 `infra/kernel/core-portability.sh`。
**改动数据面后若要依赖这条性质，必须重跑该脚本**——CO-RE 能修偏移，修不了字段改名或删除。

## 硬性不变量（完整约束）

根目录 `CLAUDE.md` 的不变量表里，落在本目录的四项的完整约束：

| 标识符 | 位置 | 约束 |
|---|---|---|
| 算法名 `"skyline_cc"` | `bpf/skyline_cc.bpf.c`（两张 struct_ops map 的 `.name`）、`bpf/skyline_policy.bpf.c`（`TCP_CONGESTION` setsockopt）、`bpf/skyline_tc.bpf.c`（`skyline_runs_skyline_cc()`）、`crates/skyline-common` 的 `SKYLINE_CC_NAME`，以及 `infra/boot-enable.sh`、`infra/boot-disable.sh`、`infra/apply-guest-profile.sh`、`install.sh` 里的字面量 | 算法注册名，**≤ 15 字符**（`TCP_CA_NAME_MAX` 为 16 含 NUL），各处一字不差。daemon 写 sysctl 时对不上会报错；policy 与 tc 两处对不上则静默失效（setsockopt 的返回值被忽略；首轮冗余只剩 SYN-ACK）。struct_ops 的 map 名是另一个命名空间 |
| `SKYLINE_ABI_VERSION` 及另外三个 ABI 版本号 | `bpf/include/skyline_abi.h` 与 `crates/skyline-common/src/lib.rs` | 布局变更必须递增管这段结构的版本号，两侧一起改：`SKYLINE_ABI_VERSION` 管 `struct skyline_config`、`skyline_flow_state`、`skyline_metrics`，`SKYLINE_RTO_TUNING_ABI_VERSION`、`SKYLINE_RETRANSMIT_DSCP_ABI_VERSION`、`SKYLINE_REDUNDANCY_ABI_VERSION` 各管自己那段（`docs/02-interface-reference.md` 第 5 节）。加载时**没有**谁拒绝不匹配的一对。`SKYLINE_ABI_VERSION` 不一致时每条连接复制配置（`skyline_adopt_config()`）都失败：此后新建的连接，`skyline_cong_control` 在每个 ACK 上直接返回——cwnd 停在内核的初始窗口（RTO 之后停在 `tcp_enter_loss()` 留下的大小），也没有谁设 pacing；`cong_control` 已经取代了内核自己的增长逻辑，所以这不是交还给内核。不报错，也没有计数器：`ssctl status --json` 里 `active_flows` 在涨、`metrics.ack_events` 不动。`struct skyline_config` 大小变了的话，daemon 写配置槽时就会报错。它也管 `struct skyline_metrics`：Rust 侧按布局读计数器，大小不符时计数器全是 0。DSCP 标记与首轮冗余的版本不符时跳过并递增各自的 `abi_mismatch`。递增版本号，是让半新半旧的组合停摆，而不是误读结构体 |
| `cong_control` 4 参数签名 | `bpf/skyline_cc.bpf.c` | `(sk, ack, flag, rs)`，内核 >= 6.10 才有 |
| struct_ops 双变体 `skyline_cc` / `skyline_cc_txs` | `bpf/skyline_cc.bpf.c` | 两张 map 挂**同一组回调**，只有 TX_START 的入口不同（`cwnd_event` / 7.1 起的 `cwnd_event_tx_start`）；daemon 按内核 BTF 只创建其中一张。**给一张加回调，另一张也要加**，否则行为随内核版本悄悄不同 |

`skyline_cc.bpf.c` 里的 `PRR-SSRB` 是 RFC 6937 的术语，不要改名（根目录 `CLAUDE.md`「命名雷区」）。

改了本目录的源码或头文件（注释也算：`-g` 的行号信息会变，对象字节随之改变）就要发版，见根目录 `CLAUDE.md`「预编译版本」。
