# docs/CLAUDE.md

文档地图。Claude Code 读写本目录下的文件时自动加载本文件；改了哪类东西要同步哪些文档，见仓库根目录 `CLAUDE.md`
的"文档同步要求"，性能数字的门槛见那里的"性能声明纪律"。

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
| `CONTRIBUTING.md` | 根目录与各目录 `CLAUDE.md` 约定的英文版，面向外部贡献者 |
| `CLAUDE.md` 与 `bpf/`、`crates/`、`config/`、`packaging/`、`infra/`、`docs/` 下的 `CLAUDE.md` | AI 编码助手：根目录的每次会话都加载，只放通用约定与索引；各目录的在读写该目录的文件时加载，放细则 |
