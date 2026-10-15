# packaging/CLAUDE.md

systemd unit 与 OpenRC 脚本的细则。Claude Code 读写本目录下的文件时自动加载本文件；全仓库的约定在仓库根目录的
`CLAUDE.md`。

## 路径的不变量（完整约束）

| 标识符 | 位置 | 约束 |
|---|---|---|
| 安装路径 `/opt|/etc|/run/skyline-speeder` | 配置、unit（含 `packaging/openrc/` 的两个 OpenRC 脚本）、脚本 | 三处必须一致 |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **必须是 `socket_path`（及 `events_path`、`state_path`）的父目录**：unit 带 `ProtectSystem=strict`，`/run` 下只有这个目录对 daemon 可写 |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC 下的 `RuntimeDirectory=`：同样**必须是 `socket_path`（及 `events_path`、`state_path`）的父目录**，daemon 启动先打开的就是那里的事件日志 |

> `RuntimeDirectory` 与 `socket_path` 的耦合是历史上真实踩过的坑：一次批量重命名
> 把 `RuntimeDirectory` 改成了 `skyline` 而配置里是 `/run/skyline-speeder/`，
> 当时 daemon 起来但控制 socket 永远不出现。现在的 daemon 自己创建事件日志、socket、状态文件
> 的父目录（`main.rs` 的 `Daemon::new`、`serve`、`persist_state`），而 `ProtectSystem=strict`
> 让 `/run` 除 `RuntimeDirectory=` 之外只读，所以按 unit 与代码推断，同样的错配今天表现为 daemon
> 打开事件日志时失败、被 systemd 反复重启——不变量不变，症状变了。改动任一侧务必对照另一侧。

## enable unit

`skyline-speeder-enable.service`（OpenRC 下是 `openrc/skyline-speeder-enable`）为消除"未执行 `enable`"这个静默
失效而存在，不要删除它（根目录 `CLAUDE.md`「静默失效点」第 2 条）。它的 `ExecStop`（`infra/boot-disable.sh`）是
`fallback_cc` 的写回者之一，顺序与前提见 `crates/CLAUDE.md`「全局 sysctl」。

本目录进产物，改了就要发版（根目录 `CLAUDE.md`「预编译版本」）。
