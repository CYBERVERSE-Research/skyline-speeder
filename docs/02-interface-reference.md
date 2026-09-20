# Skyline Speeder 接口与配置参考

本篇描述 Skyline Speeder 控制面对外暴露的全部接口：命令行工具 `ssctl`、它背后的 Unix
socket 线协议、运行期状态字段、配置文件字段，以及可观测性事件/计数器。

## 1. 控制面概览

Skyline Speeder 的运行期由一个守护进程 `skyline-speederd` 和一个命令行客户端 `ssctl` 构成：

- `skyline-speederd` 在服务器上以 systemd 服务方式运行，加载 BPF 对象、维护配置状态，并
  通过一个 Unix domain socket（默认路径 `/run/skyline-speeder/speeder.sock`）接受控制请求。
- `ssctl` 是发给 `skyline-speederd` 的一次性命令客户端：每次调用建立一条连接，发送一个
  JSON 请求，读取一行 JSON 响应，然后退出。所有状态都保存在 `skyline-speederd` 进程内，
  `ssctl` 本身不持有任何状态。

两者的请求/响应结构定义在共享库 `skyline-common` 里，因此本文档里"`ssctl` 命令
行参数"和"线协议字段"是同一套语义的两种呈现方式。

## 2. `ssctl` 命令行

调用形式：`ssctl [--socket <path>] <子命令> [参数...]`。`--socket` 默认为
`/run/skyline-speeder/speeder.sock`，通常不需要覆盖。

### 2.1 只读命令

| 命令 | 作用 |
|---|---|
| `ssctl validate` | 校验当前配置文件语法与取值范围，不改变运行状态 |
| `ssctl status` | 打印完整运行期状态：已启用模块、当前系数、M1 tier-1/tier-2 状态、DSCP 标记状态、能力探测结果 |
| `ssctl flows` | 打印当前活跃流数量（按设计只提供汇总计数，不枚举逐条连接的详细信息） |
| `ssctl snapshot <path>` | 把 `status` 的完整内容写入指定文件，供外部脚本采集 |

### 2.2 功能开关（M1-M4 消融）

| 命令 | 作用 |
|---|---|
| `ssctl enable [--modules m1,m2,...] [--all-off]` | 启用 `skyline_cc` struct_ops，`--modules` 指定要打开的模块子集（`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`，逗号分隔），省略则使用当前生效的 `enabled_modules`（daemon 内存态，启动时来自配置文件）；`--all-off` 等价于传一个空集合——仍然注册 `skyline_cc` 作为拥塞控制算法，但四个模块全部关闭（见 `docs/03-design.md` 第 4 节"中性基线"的语义）。**附加成功后会把 `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`**，即全机新连接默认走本算法；写 sysctl 发生在附加之后，因为内核会拒绝一个尚未注册的算法名 |
| `ssctl disable --module <name>` | 关闭单个模块，不影响其余已启用的模块 |
| `ssctl drain [--timeout <秒>]` | 优雅摘除：**先把 `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`**，再关闭 cgroup 派发——两条准入路径都要先关，全局 sysctl 先关，因为它放行的是全机进程而不只是 cgroup 内的；然后等待已有连接自然结束（默认超时 300 秒，超时仍有活跃连接则报错、不强制摘除），确认无活跃连接后注销 struct_ops（此时全局默认早已不指向它，不会出现"注销一个正被当作默认算法的 struct_ops"）。超时报错时 struct_ops 仍挂着，但 sysctl 已经落回，不会有新连接使用它（**通过 SSH 执行时必然走到这个分支**——执行者自己的 SSH 连接就是一条存量 skyline_cc 流）；已在用 TC 统计/M1 tier-2 RTO 调节的连接不受影响，只摘除 `skyline_cc` 一项 |

### 2.3 M2/M3/M4 系数在线调整

| 命令 | 作用 |
|---|---|
| `ssctl set-module-config [--<字段> <值> ...]` | 覆盖 M2/M3/M4 的全部系数与安全上限，见第 6 节字段表。绝对覆盖语义：每次调用都会发送完整的字段集合，不存在"只改一个字段、其余保持不变"的部分更新，避免新旧值混杂带来的排障困难 |
| `ssctl reset-module-config` | 恢复为 `skyline-speederd` 启动时配置文件里声明的值 |

### 2.4 M1 tier-2 动态 RTO 调节

| 命令 | 作用 |
|---|---|
| `ssctl set-rack-rto [--disable] [--<字段> <值> ...]` | 配置每流动态 `TCP_BPF_RTO_MIN` 下限与 `TCP_RTO_MAX_MS` 上限，见第 6 节字段表。`--disable` 取消订阅 `BPF_SOCK_OPS_RTT_CB`，恢复内核默认 RTO 下限。同样是绝对覆盖语义 |
| `ssctl reset-rack-rto` | 恢复为配置文件里声明的 `[rack_rto]` |

### 2.5 重传包 DSCP 标记

| 命令 | 作用 |
|---|---|
| `ssctl set-retransmit-dscp --dscp-value <0-63>` | 启用标记并指定 DSCP 编码值。这个值是占位参数——具体编码由网络侧策略决定，Skyline Speeder 本身不预设生产环境的取值；启用时必须提供非零值（0 在语义上等同于"剥除现有 DSCP"，不是无操作，因此被拒绝） |
| `ssctl reset-retransmit-dscp` | 恢复为配置文件里声明的 `[retransmit_dscp]`（默认关闭） |

## 3. Unix socket 线协议

传输格式是单行 JSON 请求 + 单行 JSON 响应，通过 Unix domain socket 交换：
客户端连接后写入一行 `Request` 的 JSON 序列化（末尾 `\n`），读取一行
`Response` 的 JSON 序列化，然后可以关闭连接。

`Request` 使用 `command` 字段做类型标签（kebab-case），例如：

```json
{"command": "set-module-config", "config": {"max_pacing_mbps": 1200, ...}}
```

请求变体（与 `ssctl` 子命令一一对应，见第 2 节）：`validate` / `enable` /
`disable-module` / `status` / `flows` / `snapshot` / `drain` /
`set-rack-rto` / `reset-rack-rto` / `set-module-config` /
`reset-module-config` / `set-retransmit-dscp` / `reset-retransmit-dscp`。

响应统一为：

```rust
struct Response {
    ok: bool,           // 请求是否成功
    message: String,    // 人类可读的结果描述，失败时是错误信息
    status: Option<RuntimeStatus>,  // 大多数请求都附带最新的完整状态
}
```

## 4. 状态与计数器字段（`ssctl status` 的输出结构）

`RuntimeStatus` 是 `status`/大多数写操作响应里 `status` 字段的完整结构：

| 字段 | 含义 |
|---|---|
| `enabled` | `skyline_cc` struct_ops 当前是否已注册 |
| `generation` | 配置代际计数器，双缓冲切换机制见 `docs/03-design.md` 第 11 节 |
| `modules` | 当前启用的 M1-M4 模块子集 |
| `fallback_cc` | 未启用/摘除后新连接使用的拥塞控制算法名 |
| `active_flows` | 当前正在使用 `skyline_cc` 的连接数 |
| `tc_stats` | TC 程序的包/字节/GSO 段计数（`packets`/`bytes`/`gso_packets`）；`drops` 字段恒为 0——TC 程序始终 fail open，不主动丢包 |
| `metrics` | M2/M3/M4 的决策计数器，`skyline_cc` 从未启用过时为 `null`（见下表） |
| `rack_tuning` | M1 tier-1 全局 sysctl 的"配置声明值"与"当前实际值"对照 |
| `rack_rto` | M1 tier-2 配置 + 运行计数器 |
| `retransmit_dscp` | DSCP 标记配置 + 运行计数器 |
| `module_tuning` | M2/M3/M4 当前生效系数的完整回显 |
| `capabilities` | 内核能力探测结果，见下方"能力探测" |

`metrics`（`SkylineMetrics`，percpu 计数器求和）：

| 字段 | 含义 |
|---|---|
| `ack_events` | 处理过的 ACK 数 |
| `delivered_packets` | 累计确认的包数 |
| `loss_events` | 检测到的丢包事件数 |
| `state_transitions` | STARTUP→CRUISE 状态切换次数 |
| `pacing_updates` | pacing 速率被重新计算的次数 |
| `guardrail_hits` | 队列时延/ECN 护栏实际触发的次数 |
| `hypothetical_early_loss` | `early-loss` 模块开启时，每次 `loss_events` 计数同步递增的观测计数器——只统计，不驱动任何决策（见 `docs/03-design.md` 第 5 节） |
| `prr_adjustments` | 框架级 PRR 重实现介入的次数——M2 开启时该值应恒为 0（M2 会绕开这条路径），非零说明绕开逻辑未生效 |

能力探测（`CapabilityReport`，`skyline-speederd` 启动时探测一次）：

| 字段 | 含义 |
|---|---|
| `kernel_release` | `uname -r` |
| `btf` / `bpffs` / `cgroup_v2` / `fq_available` / `struct_ops` | 五项硬性前提，任一为 `false` 则 `skyline-speederd` 拒绝启动 |
| `fallback_cc_available` | `fallback_cc` 配置的算法名是否确实是内核已注册的拥塞控制 |
| `rack_reo_hook` | 探测一个内核补丁专用的钩子是否存在，标准上游内核上恒为 `false`，仅记录不影响启动 |
| `notes` | 自由文本，记录软性降级信息（例如某功能因内核能力不足而退化为纯观测） |

`struct_ops` 与 `rack_reo_hook` 由 `skyline-speederd` 进程内直接解析
`/sys/kernel/btf/vmlinux` 得出，**不依赖 bpftool**（`--prebuilt` 主机上本来就没有它）：
`struct_ops` 看内核 BTF 中是否存在结构体 `bpf_struct_ops_tcp_congestion_ops`——libbpf
加载 `skyline_cc` 时解析的正是这个类型；`rack_reo_hook` 看是否有结构体带名为
`rack_reo_wnd` 的函数指针成员。BTF 文件存在但解析失败时两项都为 `false`，原因写进 `notes`。

## 5. ABI 版本

BPF 侧的三段配置各自独立维护自己的 ABI 版本号，互不联动——三者的生命周期
不同（M2/M3/M4 系数几乎每次调参都变，M1 tier-2/DSCP 标记只在功能本身改动时
才变），合并成一个版本号会让大多数改动被迫牵连另外两段：

| 常量 | 覆盖范围 |
|---|---|
| `SKYLINE_ABI_VERSION` | M2/M3/M4 系数结构（`KernelConfig`） |
| `SKYLINE_RTO_TUNING_ABI_VERSION` | M1 tier-2 动态 RTO 结构（`KernelRtoTuning`） |
| `SKYLINE_RETRANSMIT_DSCP_ABI_VERSION` | DSCP 标记结构（`KernelRetransmitDscpConfig`/`KernelRetransmitDscpStats`） |

三段配置的版本校验行为并不完全一致：

- M2/M3/M4（`SKYLINE_ABI_VERSION`）：BPF 侧读取配置槽位时校验版本号，不匹配则
  整体拒绝，退回到未启用 M2 时的固定 CUBIC-beta 降窗行为，没有对应的计数
  器记录这次拒绝。
- DSCP 标记（`SKYLINE_RETRANSMIT_DSCP_ABI_VERSION`）：版本不匹配时跳过标记逻辑
  （相当于功能关闭），并递增 `retransmit_dscp.stats` 里的 `abi_mismatch`
  计数器。
- M1 tier-2 动态 RTO（`SKYLINE_RTO_TUNING_ABI_VERSION`）：BPF 侧当前不校验这个
  版本号。

这只会发生在 `skyline-speederd` 与它加载的 `.bpf.o` 不是同一次构建产物时，正常部署
（`skyline-speederd`/`ssctl`/BPF 对象来自同一次 `make`）不会遇到。

## 6. 配置文件字段参考

配置文件为 TOML 格式，路径通过 `skyline-speederd --config <path>` 指定。除 `[rack_tuning]`
外的每一段都可以在不重启 `skyline-speederd` 的情况下用对应的 `ssctl set-*`/`reset-*`
命令在线覆盖；配置文件里的值只是启动时的初始默认值，运行期实际生效值以
`ssctl status` 为准。

### 6.1 顶层字段

| 字段 | 类型 | 说明 | 可在线覆盖 |
|---|---|---|---|
| `generation` | u32 | 配置代际计数器，必须非零；每次系数更新自动递增，`skyline_cc.bpf.c` 用它保证一次 ACK 处理内读到的是同一代配置，避免双缓冲区被半读 | 否（内部维护） |
| `enabled_modules` | string[] | 启动时启用的 M1-M4 模块子集，取值 `early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing` | 是（`ssctl enable`） |
| `prr_pacing_enabled` | bool | 框架级 PRR 重实现开关，独立于上面四个模块——即使 `enabled_modules` 为空也默认生效（修正的是"绕开内核 PRR 后基线不对等"这个问题，不是可选特性）。M2 开启时该路径不会被执行 | 是（`set-module-config`） |
| `auto_pacing_enabled` | bool | M4 关闭时使用的、等价于内核默认行为的 pacing 速率上限，独立开关，语义同上 | 是（`set-module-config`） |
| `fallback_cc` | string | 未启用/摘除 `skyline_cc` 时新连接使用的拥塞控制算法名，必须是内核已注册的算法 | 否 |
| `max_pacing_mbps` | u64 | pacing 速率硬上限（Mbps），`pacing` 模块启用时不可为 0 | 是 |
| `max_cwnd_packets` | u32 | cwnd 硬上限（包），最小值 4 | 是 |
| `max_queue_delay_ms` | u32 | 队列时延护栏的固定基准值（毫秒） | 是 |
| `max_queue_delay_ratio` | f64 | 按基准 RTT 折算的护栏增量比例，实际护栏 = `max(max_queue_delay_ms, 基准RTT × 该比例)`，让护栏在高 RTT 路径上自动放宽而不是卡在固定毫秒数；`0.0` = 不放宽，恒等于 `max_queue_delay_ms` | 是 |
| `initial_cwnd_packets` | u32 | 连接建立时的初始 cwnd（包），`0` = 不覆盖、使用内核自身的初始窗口 | 是 |

### 6.2 `[adaptive_cwnd]`（M2 系数）

| 字段 | 类型 | 说明 |
|---|---|---|
| `min_rtt_window_s` | u32 | 最小 RTT 滑动窗口长度（秒） |
| `bw_window_rtts` | u32 | 带宽估计窗口长度（RTT 个数），取值范围 1-10 |
| `startup_plateau_rtts` | u32 | 判定"带宽增长平台期"所需的连续 RTT 数，用于触发 STARTUP→CRUISE 切换 |
| `startup_growth_ratio` | f64 | 低于此增长比例视为平台期，取值范围 0.0-1.0 |
| `startup_gain` | f64 | STARTUP 态单一增益，同时作用于 cwnd 目标和 pacing 速率，必须 ≥1.0 |
| `cruise_inflight_gain` | f64 | CRUISE 态 cwnd 目标增益，必须 ≥1.0——刻意比 `cruise_pacing_gain` 更宽松，cwnd 只需不成为限制因素，真正的限速器是 pacing |
| `cruise_pacing_gain` | f64 | CRUISE 态 pacing 速率增益（M4 开启时真正生效的限速值），必须 ≥1.0 |
| `guardrail_gain` | f64 | 队列时延/ECN 护栏触发时对 cwnd 目标和 pacing 速率共同生效的增益，取值范围 0.0-1.0。`0.0` = 未启用护栏降速（护栏只取消加速，增益回到中性 1.0）；`(0.0, 1.0)` = 主动降速到测得带宽的该比例 |

### 6.3 `[loss_classifier]`（M3 系数）

| 字段 | 类型 | 说明 |
|---|---|---|
| `loss_inflation_max_ratio` | f64 | 对测得丢包率的补偿上限，取值范围 0.0-0.5。cwnd/pacing 目标按 `1/(1-测得丢包率)` 放大以抵消丢包造成的"已确认速率"低估，本字段是这个放大系数的封顶；`0.0` = 不补偿，放大系数恒为 1.0 |

### 6.4 `[rack_tuning]`（M1 tier-1，全局 sysctl 默认值）

三个字段全部是 `Option<u32>`，留空/整段省略表示"`skyline-speederd` 不接管这些 sysctl，
所有权完全交给外部"；一旦声明具体数值，`skyline-speederd` 启动时会写入对应的
`/proc/sys/net/ipv4/*`，写入失败则守护进程启动失败。生产部署通常整段省略，
让操作系统默认值生效。

| 字段 | 类型 | 合法范围 |
|---|---|---|
| `tcp_recovery` | Option\<u32\> | 位掩码，仅 1/3/5/7 合法 |
| `tcp_reordering` | Option\<u32\> | 1-300 |
| `tcp_early_retrans` | Option\<u32\> | 0-4 |

### 6.5 `[rack_rto]`（M1 tier-2，每流动态 RTO）

| 字段 | 类型 | 说明 |
|---|---|---|
| `enabled` | bool | 是否订阅 `BPF_SOCK_OPS_RTT_CB` 并应用下面的调节 |
| `srtt_permille` | u32 | RTO 下限目标 = `max(srtt_us, min_rtt_us) × 该值 / 1000` |
| `floor_us` | u32 | RTO 下限的绝对安全下界（微秒） |
| `ceiling_us` | u32 | RTO 下限的绝对安全上界（微秒），不得超过内核 `TCP_RTO_MIN` 常量（200000） |
| `warmup_samples` | u32 | 前 N 个 RTT 样本只观测、不下发调节 |
| `rto_max_normal_permille` | u32 | 无拥塞证据时，`TCP_RTO_MAX_MS` 上限 = `clamp(该值 × 基准RTT/1000, 1000ms, 120000ms)`；`0` = 整个上限调节功能关闭，`TCP_RTO_MAX_MS` 保持内核默认 |
| `rto_max_congested_permille` | u32 | 出现拥塞证据（新 CE 标记，或 srtt 明显超过 min_rtt）后使用的倍数，必须 ≥ `rto_max_normal_permille` |
| `rto_max_congestion_ratio_permille` | u32 | srtt 超过 min_rtt 的比例超过此值即计为拥塞证据，`0` = 使用内置默认（2000，即 2 倍） |

`enabled=true` 时 `floor_us`/`srtt_permille` 必须非零，`ceiling_us` 必须
≥ `floor_us` 且 ≤ 内核 `TCP_RTO_MIN`。

### 6.6 `[retransmit_dscp]`

| 字段 | 类型 | 说明 |
|---|---|---|
| `enabled` | bool | 是否对重传包打 DSCP 标记 |
| `dscp_value` | u8 | DSCP 编码值，0-63；`enabled=true` 时必须非零 |

### 6.7 `[runtime]`

| 字段 | 类型 | 说明 |
|---|---|---|
| `bpf_dir` | path | BPF 对象文件所在目录 |
| `pin_dir` | path | struct_ops/map 在 bpffs 下的 pin 路径 |
| `cgroup_path` | path | M1 tier-2 cgroup sockops 挂载的 cgroup 路径 |
| `socket_path` | path | 控制面 Unix socket 路径 |
| `state_path` | path | 运行期状态落盘路径（供外部监控读取，不是配置输入） |
| `events_path` | path | 事件流落盘路径 |
| `tc_interface` | Option\<string\> | TC 程序（统计 + DSCP 标记）挂载的网络接口名；不设置则两者都不加载 |

生产环境示例见 `config/speeder-guest.toml`（`bpf_dir`/`pin_dir` 指向部署后的
绝对路径 `/opt/skyline-speeder/bpf`，`[rack_tuning]`/`[rack_rto]` 整段留空）。

## 7. Feature mask 位表

`skyline_cc` 的模块开关在 BPF 侧是一个 32 位掩码：

| 位 | 名称 | 含义 |
|---|---|---|
| 0 | `EARLY_LOSS` | M1 早期丢包感知（观测通道，见 `docs/03-design.md`） |
| 1 | `ADAPTIVE_CWND` | M2 自适应 cwnd |
| 2 | `LOSS_CLASSIFIER` | M3 丢包率补偿 |
| 3 | `PACING` | M4 主动 pacing |
| 4 | `PRR` | 框架级 PRR 重实现，独立开关（见 `prr_pacing_enabled`） |
| 5 | `AUTO_PACING` | M4 关闭时的等效 pacing 上限，独立开关（见 `auto_pacing_enabled`） |

## 8. 事件码与计数器

`skyline-speederd` 通过一个 BPF ring buffer 收集决策事件，落盘为 `events_path` 指向的
JSON Lines 文件，每行一个事件：

```rust
struct SkylineEvent {
    timestamp_ns: u64,
    socket_cookie: u64,
    value_a: u64,
    value_b: u64,
    event_type: u32,
    state: u32,   // 事件发生时的 SKYLINE_MODE_STARTUP(0) / SKYLINE_MODE_CRUISE(1)
}
```

| `event_type` | 名称 | `value_a` | `value_b` |
|---|---|---|---|
| 1 | STATE | 切换前的模式 | 切换后的模式（与 `state` 字段一致） |
| 2 | LOSS | 本次采样的丢包数 | 保留，恒为 0 |
| 9 | GUARDRAIL | 触发时的队列时延（微秒） | 护栏阈值（微秒） |
| 10 | UNDO_CWND | 恢复目标 cwnd | 实际返回给内核的 cwnd |
| 11 | GENERATION_SWITCH | 切换前的配置代际号 | 切换后的配置代际号 |

（编号不连续：3-8 是保留的编号空位，当前实现从未产出，也不需要消费者
处理，消费者可以依赖 1/2/9/10/11 这几个编号保持固定不变。）
