# crates/CLAUDE.md

Rust 控制面（`skyline-speederd`、`ssctl`、`skyline-common`）的细则。Claude Code 读写本目录下的文件时自动加载
本文件；全仓库的约定在仓库根目录的 `CLAUDE.md`。ABI 版本号的完整约束在 `bpf/CLAUDE.md`：`skyline-common` 与
`bpf/include/skyline_abi.h` 两侧一起改。

## 全局 sysctl：写入顺序与写入者（静默失效点 2 的细则）

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

guard 维护的是 `runtime.tc_interface` 那块网卡的根 qdisc，指错网卡的后果见 `config/CLAUDE.md`。

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

## guest 配置的守护测试

`crates/skyline-common/src/lib.rs` 的 `guest_config_never_owns_global_sysctls` 守护 `config/CLAUDE.md` 里
"`[rack_tuning]` 必须整段保持注释"那条约束——**不要为了让某个用例通过而删改它**。
