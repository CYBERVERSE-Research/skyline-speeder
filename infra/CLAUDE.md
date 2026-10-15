# infra/CLAUDE.md

`infra/` 脚本的细则。Claude Code 读写本目录下的文件时自动加载本文件；全仓库的约定在仓库根目录的 `CLAUDE.md`。

- **全局 sysctl 与 qdisc**：谁、在什么前提下写 `net.ipv4.tcp_congestion_control`、`net.core.default_qdisc` 和出口
  网卡的根 qdisc 是不变量，完整规则在 `crates/CLAUDE.md`「全局 sysctl」，改下面几个脚本里的这类写入之前先读它。
  - `boot-enable.sh` 刻意不写任何 sysctl；它过去写的 `default_qdisc=fq` **不要加回去**。
  - `boot-disable.sh` 先 drain，之后只在默认仍是 `skyline_cc` 时兜底写回，是 daemon 已死时的兜底，不是重复。
  - `apply-guest-profile.sh` 在 `drain --timeout 0` 之后才写自己要的算法与 qdisc；它逐 case 精确控制
    `tcp_recovery` / `tcp_reordering` / `tcp_early_retrans`，所以 guest 配置不能接管这几个 sysctl（`config/CLAUDE.md`）。
- **随产物发布的脚本**：`release.yml` 把 `boot-enable.sh`、`boot-disable.sh`、`run-in-skyline-cgroup.sh`、
  `apply-guest-profile.sh`、`collect-guest-metrics.sh`、`snapshot-skyline-events.sh` 打进产物，改了它们就要发版
  （根目录 `CLAUDE.md`「预编译版本」）。
- **`kernel/core-portability.sh`**：改动数据面后若要依赖 CO-RE 可移植性，必须重跑（`bpf/CLAUDE.md`「CO-RE 可移植性」）；
  改过数据面的版本发版前也要跑。
- **Shell 风格**：`set -euo pipefail`；`boot-enable.sh` 是 POSIX sh 的 `set -eu`，`boot-disable.sh` 刻意只用
  `set -u`（根目录 `CLAUDE.md`「风格」）。
