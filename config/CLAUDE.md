# config/CLAUDE.md

配置模板的细则。Claude Code 读写本目录下的文件时自动加载本文件；全仓库的约定在仓库根目录的 `CLAUDE.md`
（配置字段的同步要求在那里的"文档同步要求"表：`config/*.toml` + `docs/02-interface-reference.md` §6）。

## guest 配置的强约束

`config/speeder-guest.toml` 中 `[rack_tuning]` **必须整段保持注释**。该配置由实验
矩阵（`infra/apply-guest-profile.sh`）逐 case 精确控制 `tcp_recovery` /
`tcp_reordering` / `tcp_early_retrans`；若 `skyline-speederd` 在此接管这几个全局 sysctl，
daemon 重启会把 per-case 精确值悄悄覆盖回默认值。

`crates/skyline-common/src/lib.rs` 有 `guest_config_never_owns_global_sysctls`
测试守护这条约束——**不要为了让某个用例通过而删改它**。

## `tc_interface` 指向错误网卡（静默失效点 3 的细则）

`config/speeder-guest.toml` 的默认值 `data0` 是测试床的
接口名，真实主机上几乎必然不匹配。`install.sh` 每次运行（安装与升级都算），只要配置里还是
`tc_interface = "data0"` 这一行，就改写成默认路由（先 IPv4、再 IPv6）上的第一块以太网网卡；
已经是别的值（运维改过）就不动。默认路由只走 WireGuard/tun 这类隧道时不改写，保留模板值并
告警——`skyline_tc` 只挂在以太网设备上。
guard 维护的也是这块网卡的根 qdisc：指错了，真正的出口网卡保持原来的 qdisc，而被指到
的那块网卡被换成 `fq`。
