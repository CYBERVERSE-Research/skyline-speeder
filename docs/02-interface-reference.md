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
- `ssctl enable` 成功之后、下一次 `ssctl drain` 之前，`skyline-speederd` 还会在后台周期性
  检查并守住全机默认拥塞控制与 `fq` qdisc（guard），见第 9 节。

两者的请求/响应结构定义在共享库 `skyline-common` 里，因此本文档里"`ssctl` 命令
行参数"和"线协议字段"是同一套语义的两种呈现方式。

## 2. `ssctl` 命令行

调用形式：`ssctl [--socket <path>] <子命令> [参数...]`。`--socket` 默认为
`/run/skyline-speeder/speeder.sock`，通常不需要覆盖。

`ssctl --version` 打印 `ssctl <版本号>`，`skyline-speederd --version` 同理——两者说的都是
磁盘上那个二进制的版本。**正在运行**的 daemon 的版本看 `ssctl status` 里的 `version`
（第 4 节）：升级后两者不一致，说明内存里跑的还是旧 daemon。

### 2.1 只读命令

| 命令 | 作用 |
|---|---|
| `ssctl validate` | 校验当前配置文件语法与取值范围，不改变运行状态 |
| `ssctl status` | 打印完整运行期状态：daemon 版本、已启用模块、当前系数、M1 tier-1/tier-2 状态、DSCP 标记状态、能力探测结果、guard 状态 |
| `ssctl flows` | 打印当前活跃流数量（按设计只提供汇总计数，不枚举逐条连接的详细信息） |
| `ssctl snapshot <path>` | 把 `status` 的完整内容写入指定文件，供外部脚本采集 |

### 2.2 功能开关（M1-M4 消融）

| 命令 | 作用 |
|---|---|
| `ssctl enable [--modules m1,m2,...] [--all-off]` | 启用 `skyline_cc` struct_ops，`--modules` 指定要打开的模块子集（`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`，逗号分隔），省略则使用当前生效的 `enabled_modules`（daemon 内存态，启动时来自配置文件）；`--all-off` 等价于传一个空集合——仍然注册 `skyline_cc` 作为拥塞控制算法，但四个模块全部关闭（见 `docs/03-design.md` 第 4 节"中性基线"的语义）。**附加成功后会把 `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`**，即全机新连接默认走本算法；写 sysctl 发生在附加之后，因为内核会拒绝一个尚未注册的算法名。随后**武装 guard 并立即做一次完整检查**（`[guard] interval_s = 0` 时也做）：`[guard] qdisc = true`（默认）时把 `net.core.default_qdisc` 写成 `fq`、把 `runtime.tc_interface` 的根 qdisc 换成 `fq`（它是 VLAN、bond、网桥时换的是它下面的物理网卡），此后直到下一次 `drain` 持续守住这几项，见第 9 节；周期检查正在退避的替换，`enable` 不等退避、立即重试。这次检查改了什么、发现了什么、哪一步没做成，都以 `; <内容>` 的形式追加在响应 `message` 末尾；qdisc 这一步失败**不会**让 `enable` 失败（此时 `skyline_cc` 已挂载且已是默认），只记进 message 和 `guard.last_error`。已发布的 v0.2.0 及更早 release 的 `enable` 不碰 qdisc；不想让它碰，设 `[guard] qdisc = false` |
| `ssctl disable --module <name>` | 关闭单个模块，不影响其余已启用的模块 |
| `ssctl drain [--timeout <秒>]` | 优雅摘除：**先解除 guard 的武装，并在仍持有 guard 锁时把 `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`**（正在进行的一次周期检查会先跑完，它写的值随即被覆盖；即使 rtnl 锁被卡住，这段等待也只有约一个 `tc` 超时，即 10 秒左右，见第 9 节"失败处理"；此后不会再有检查把 `skyline_cc` 写回去。无论 drain 最终成功还是超时，guard 都保持解除），再关闭 cgroup 派发——两条准入路径都要先关，全局 sysctl 先关，因为它放行的是全机进程而不只是 cgroup 内的；然后等待已有连接自然结束（默认超时 300 秒，超时仍有活跃连接则报错、不强制摘除），确认无活跃连接后注销 struct_ops（此时全局默认早已不指向它，不会出现"注销一个正被当作默认算法的 struct_ops"）。超时报错时 struct_ops 仍挂着，但 sysctl 已经落回，不会有新连接使用它（**通过 SSH 执行时必然走到这个分支**——执行者自己的 SSH 连接就是一条存量 skyline_cc 流）；已在用 TC 统计/M1 tier-2 RTO 调节的连接不受影响，只摘除 `skyline_cc` 一项。drain 不动任何 qdisc：`default_qdisc` 与网卡根 qdisc 保持 `fq` |

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
| `version` | 正在运行的 `skyline-speederd` 的版本号（编译时的 crate 版本）。来自不带此字段的旧 daemon 时解码为空字符串 |
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
| `guard` | cc/qdisc 守护的状态与计数器，见下方 `guard` 表；来自不带此字段的旧 daemon 时解码为全默认值（未武装、计数为 0） |

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
| `btf` / `bpffs` / `cgroup_v2` / `fq_available` / `struct_ops` | 硬性前提，任一为 `false` 则 `--validate-only`、`ssctl validate`、`ssctl enable` 直接拒绝（`skyline-speederd` 进程本身照常启动） |
| `fallback_cc_available` | `fallback_cc` 配置的算法名是否确实是内核已注册的拥塞控制；也是硬性前提，为 `false` 时上述三处一样拒绝 |
| `rack_reo_hook` | 探测一个内核补丁专用的钩子是否存在，标准上游内核上恒为 `false`；仅作记录，不参与上述门槛 |
| `notes` | 自由文本，记录软性降级信息（例如某功能因内核能力不足而退化为纯观测；`[guard] qdisc = true`、设置了 `tc_interface` 而主机上没有 `tc`（iproute2）时，也在这里提示 guard 无法维护该网卡（或它下面的物理网卡）的根 qdisc；`tc_interface` 不是以太网设备时提示 `TC interface <网卡> is not an Ethernet device (type N, e.g. a tunnel); skyline_tc is not attached`，见第 6.7 节）。`TC runtime unavailable: <原因>` 是 TC 程序没有加载的原因 |

`struct_ops` 与 `rack_reo_hook` 由 `skyline-speederd` 进程内直接解析
`/sys/kernel/btf/vmlinux` 得出，**不依赖 bpftool**（`--prebuilt` 主机上本来就没有它）：
`struct_ops` 看内核 BTF 中是否存在结构体 `bpf_struct_ops_tcp_congestion_ops`——libbpf
加载 `skyline_cc` 时解析的正是这个类型；`rack_reo_hook` 看是否有结构体带名为
`rack_reo_wnd` 的函数指针成员。BTF 文件存在但解析失败时两项都为 `false`，原因写进 `notes`。

`guard`（`GuardStatus`，行为见第 9 节）：

| 字段 | 含义 |
|---|---|
| `armed` | 是否正在守护：一次成功的 `enable` 之后为 `true`，`drain` 之后为 `false`。daemon 启动时恒为 `false`——即使上一个被杀掉的实例留下的 `skyline_cc` 仍然注册着，也只有本进程里的一次 `enable` 能武装它 |
| `interval_s` / `qdisc` | `[guard]` 配置的回显（第 6.8 节） |
| `checks` | 武装期间跑过的周期检查次数（`enable` 自己那一次不计入） |
| `cc_restored` | `tcp_congestion_control` 被改回 `skyline_cc` 的次数 |
| `default_qdisc_restored` | `net.core.default_qdisc` 被改回 `fq` 的次数（含 `enable` 那一次） |
| `interface_qdisc_replaced` | 受管网卡（`tc_interface`，或它下面的物理网卡，见 `live.devices`）的根 qdisc 被替换的次数，各网卡合计（含 `enable` 那一次） |
| `last_correction` / `last_correction_unix_s` | 最近一次改动的描述（如 `tcp_congestion_control bbr -> skyline_cc`）及其 Unix 时间（秒） |
| `last_error` | 最近一次失败（没有 `tc`、`tc` 超时或上一个被杀掉的 `tc` 还没退出、写入被拒、`default_qdisc` 不是 `fq` 因而不新建 `mq`、替换后仍不是 `fq` 等，见第 9 节）；保留到下一次失败为止，是历史，不代表现在仍失败 |
| `live` | 读 status 时现场读取的值，与是否武装无关：`tcp_congestion_control`、`default_qdisc`、`interface`（即 `tc_interface`）、`interface_qdisc`（该网卡自己的根 qdisc 摘要：`fq`、`cake`、`mq/fq`、`mq/cake,fq`——`mq/` 后是其子队列出现过的 qdisc 种类，去重后按字母序；VLAN、bond、网桥上是 `noqueue`；没有网卡或读不到时为 `null`）、`devices`（guard 实际维护的网卡，每项 `{"name": ..., "qdisc": ...}`，`qdisc` 与 `interface_qdisc` 同一格式，读不到或网卡 down 时为 `null`；就是 `tc_interface` 自己，或它是 VLAN、bond、网桥时它下面的物理网卡，见第 9 节"受管网卡"。`[guard] qdisc = false`、`tc_interface` 不存在或下面找不到网卡时为空列表；来自不带此字段的旧 daemon 时同样为空） |
| `notes` | 最近一次检查刻意没动的东西和没做成的事，例如 `eth0 root qdisc htb looks deliberate; left alone`、`eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone`、`eth0 (under bond0) root qdisc cake: the last replace failed; retrying in 30 s`、`wg0 is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked` |

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

配置文件为 TOML 格式，路径通过 `skyline-speederd --config <path>` 指定。各部分的生效方式不同：

- 顶层系数、`[adaptive_cwnd]`/`[loss_classifier]` 与 `enabled_modules`：daemon 启动时只读入内存，
  并不加载 `skyline_cc`；`ssctl enable` 挂载它时下发的是 daemon 内存里的当前值（启动时读自文件，
  之后改文件不生效；挂载前或 `drain` 之后用下面这些命令做的修改会保留）。挂载后系数可用
  `ssctl set-module-config`/`reset-module-config`、模块可用 `ssctl enable --modules`/`disable --module`
  在线覆盖；
- `[rack_rto]` 与 `[retransmit_dscp]`：daemon 启动时 BPF 侧初始化为关闭，文件里的值要执行一次
  `reset-rack-rto`/`reset-retransmit-dscp` 才下发（`set-*` 下发的是命令行给出的值）；在此之前
  `ssctl status` 里这两段的 `config` 显示的是文件值，并不代表已经生效；
- `[runtime]`、`[rack_tuning]`、`[guard]` 与 `fallback_cc`：没有在线命令，修改后需重启 `skyline-speederd`。

除上面注明的这一处例外，运行期实际生效值以 `ssctl status` 为准。

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
| `min_cwnd_packets` | u32 | M2 的 cwnd 目标下限（包），取值范围 `4`–`max_cwnd_packets`，省略 = `4`（历史行为）。只在 `adaptive-cwnd` 开启时生效；M2 关闭的中性基线路径始终使用固定的 4 包下限，不受此字段影响。发送速率仍由 pacing 决定，见 `docs/03-design.md` 第 6 节 | 是 |

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
| `events_max_mib` | u32 | 事件日志的大小上限（MiB），默认 `8`。超过后轮转，最多占用约两倍；`0` 关闭事件日志。缺省时同样取 `8`；修改需重启 daemon。详见第 8 节 |
| `tc_interface` | Option\<string\> | TC 程序（统计 + DSCP 标记）挂载的网络接口名；不设置则两者都不加载。必须是以太网设备（`/sys/class/net/<网卡>/type` 为 1；VLAN、bond、网桥也是）：`skyline_tc` 在每个报文的偏移 0 处按以太网帧头解析，在 WireGuard/WARP、tun、gre、ppp 这类三层隧道上统计会失真，启用的 DSCP 标记还会写进 IP 头内部，所以 daemon 拒绝挂载，`capabilities.notes` 里写明原因，`set-retransmit-dscp` 随之失败——默认路由走隧道的主机，这里填承载隧道流量的物理网卡。它同时决定 guard 维护哪块网卡的根 qdisc（第 6.8、9 节）：它自己，或它是 VLAN、bond、网桥时它下面的物理网卡；不设置则 guard 不碰任何网卡的根 qdisc |

生产环境示例见 `config/speeder-guest.toml`（`bpf_dir`/`pin_dir` 指向部署后的
绝对路径 `/opt/skyline-speeder/bpf`，`[rack_tuning]`/`[rack_rto]` 整段留空）。

### 6.8 `[guard]`

`ssctl enable` 之后 `skyline-speederd` 守住什么、多久查一次（行为见第 9 节）。整段省略
时取下表默认值——从 0.2.0 升级、没有这一段的已装配置文件，重启新 daemon 后同样开启
qdisc 守护。修改后需重启 daemon。

| 字段 | 类型 | 说明 |
|---|---|---|
| `interval_s` | u32 | 周期检查间隔（秒），默认 `5`，取值 `0`–`3600`。`0` = 不做周期检查（没有后台线程），`ssctl enable` 仍然会做一次完整检查。替换失败后的重试退避从 max(`interval_s`, 30 秒) 起算（第 9 节） |
| `qdisc` | bool | 默认 `true`：除拥塞控制外，还守住 `net.core.default_qdisc = fq` 和受管网卡（`runtime.tc_interface`，或它下面的物理网卡）的根 qdisc。`false` = 不碰任何 qdisc，guard 只守拥塞控制 |

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

**大小上限与轮转。** `events_path` 默认在 `/run` 下，而 `/run` 是按内存计的 tmpfs，
与 systemd、Docker（runc 状态）等共用。连接多、丢包或排队频繁的主机上事件量很大：
早期版本不设上限，曾把 `/run` 整个写满，导致 Docker 无法写入 runc 状态文件，
而本服务自身不报任何错误。现在追加一行会使文件超过 `events_max_mib` 时，daemon 先把它重命名为
`<events_path>.1`（覆盖上一个），再新建文件继续写。轮转只发生在行与行之间，
最多占用约 `2 × events_max_mib`。外部截断（`truncate -c -s 0`）会立即释放空间，
daemon 从文件开头接着写；直接 `rm` 也不会出错，但被删的文件仍由 daemon 持有，
要等它写到上限、换成新文件时空间才释放。`events_max_mib = 0` 时不打开文件，也不读取 ring
buffer，内核侧的事件在 ring buffer 满后直接丢弃。

按区间截取事件请用 `infra/snapshot-skyline-events.sh cursor` 记下游标（`INODE:行数`），
事后用 `since <游标>` 取出其后的全部事件。该脚本能跨一次轮转拼接 `.1` 与当前文件；
跨两次及以上时会在 stderr 提示中间有事件丢失。

## 9. 拥塞控制与 qdisc 守护（guard）

**为什么需要。** VPS 上常见的"一键 BBR"脚本把 `net.ipv4.tcp_congestion_control=bbr` 和
`net.core.default_qdisc=cake`/`fq_pie` 写进 `/etc/sysctl.conf` 或 `/etc/sysctl.d/*.conf`。
开机时 systemd-sysctl 先应用它们，随后 `ssctl enable` 把拥塞控制改回 `skyline_cc`；但之后
任何重新执行 `sysctl --system`/`sysctl -p` 的操作都会悄悄把拥塞控制翻回 `bbr`——此后每条
新连接都绕开 `skyline_cc`，而且没有任何报错。另外 `default_qdisc` 只影响**之后新建**的
qdisc：网卡的根 qdisc 往往在这个 sysctl 生效前就建好了（一台全新的 Debian 13 主机上
实测：`/etc/sysctl.d` 里写着 `fq`，`eth0` 仍是 `fq_codel`），所以只改 sysctl 改不到网卡，
必须替换根 qdisc。

**所有权。** 从一次成功的 `enable` 到下一次 `drain`，下面几项只有 `skyline-speederd` 一个
写入者：

- `net.ipv4.tcp_congestion_control` = `skyline_cc`（它本来就是这个 sysctl 的唯一所有者）；
- `net.core.default_qdisc` = `fq`，以及受管网卡（通常就是 `runtime.tc_interface`，见下文
  "受管网卡"）的根 qdisc = `fq`（多队列网卡上是每个子队列都为 `fq` 的 `mq`）——仅当
  `[guard] qdisc = true`。`infra/boot-enable.sh` 过去在开机时写一次 `default_qdisc=fq`，
  现在不再写。

**武装与解除。** daemon 启动时不武装；`enable` 在挂载并切换默认算法之后武装并立即检查
一次；`drain` 先解除武装、并在同一把锁内写回 `fallback_cc`，所以周期检查不可能在 drain
写回之后再把 `skyline_cc` 写回去；drain 超时报错时 guard 同样保持解除（实验矩阵用
`drain --timeout 0` 后自己装 qdisc，guard 不能跟它抢）。停止 daemon 时先停掉并等待 guard
线程结束、再注销 struct_ops，且**不写任何 sysctl**（不会把 `fallback_cc` 写回去，这是
刻意的；要写回请先 `ssctl drain`）。

**受管网卡。** `runtime.tc_interface` 的根不是 `noqueue` 时，受管的就是它自己。是 `noqueue`
时——这是内核给 VLAN、bond、网桥、macvlan、veth 和隧道设备的默认值，这类设备自己不排队，
报文真正排队的是下面物理网卡的根 qdisc，一键脚本的 `cake` 也装在那里——guard 顺着
`/sys/class/net/<网卡>/lower_*` 往下找：

- 最多 4 层（网桥里的 bond 上的 VLAN 是 3 层），按名字排序，每块设备只看一次；
- 下层设备的根仍是 `noqueue`：继续往下找；
- 根是别的 qdisc、且有 `/sys/class/net/<网卡>/device` 链接（物理或 virtio 网卡：bond 的
  从属网卡、VLAN 的真实网卡、网桥的物理端口）：受管；根不是 `noqueue` 却没有 `device`
  链接的中间设备（例如网桥下一个自己装了 `htb` 的 bond）不受管；
- 没有 `device` 链接、下面也没有设备的端口（虚拟机的 tap、容器的 veth、ifb）从不受管，
  guard 连 `tc` 都不对它们运行：网桥上的虚拟机和容器端口不是本机的出口。

一块也没找到（例如内核 WireGuard 设备，或只挂着 tap 的网桥）时，只记一条 note：
`<网卡> is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked`。
tun、PPP 这类设备（OpenVPN、Cloudflare WARP 客户端、wireguard-go/Tailscale、PPPoE）不一样：
它们有自己的默认 qdisc（`fq_codel`、`pfifo_fast` 等），不是 `noqueue`，guard 像对网卡一样
管它自己，把未整形的根换成 `fq`（内层 TCP 的报文就在这里排队）；它下面的物理网卡没有
`lower_*` 链接，guard 够不着。`skyline_tc` 照样不挂在这些非以太网设备上。
例外：有 `device` 链接、下面没有别的设备的网卡根是 `noqueue`——内核从不给物理网卡这个
默认值，只能是有人设的——受管，并按刻意搭建处理（不动，记 note）。受管网卡在纠正记录
和 note 里带上路径，由近及远：`eth0 (under bond0)`、`eth0 (under bond0 under vmbr0)`。
`ssctl status` 的 `guard.live.devices` 列出当前受管的网卡与它们的根 qdisc。

**一次检查做什么**（`enable` 时一次；武装期间每 `interval_s` 秒一次）：

1. 读 `tcp_congestion_control`，不是 `skyline_cc` 就改回；
2. `qdisc = true` 时：`default_qdisc` 不是 `fq` 就改成 `fq`（内核在这次写入时自行加载
   `sch_fq`）。先做这一步，因为下面新建的 `mq` 的子队列按它创建；
3. `qdisc = true`、设置了 `tc_interface` 且该网卡存在时：确定受管网卡（见上），对每一块
   读 `tc qdisc show dev <网卡>`，按根 qdisc 的种类处理：

| 根 qdisc（或 `mq` 的子队列）的种类 | 处理 |
|---|---|
| `fq`；或子队列全是 `fq` 的 `mq` | 已符合，不动 |
| `pfifo_fast` `pfifo` `bfifo` `pfifo_head_drop` `fq_codel` `codel` `cake`（未整形：`bandwidth unlimited`，没有 `autorate-ingress`——`default_qdisc=cake` 建出来的就是这样） `fq_pie` `pie` `sfq` `red` `sfb` `choke` `hhf` | 替换为 `fq`，做法见下 |
| 分类/整形/硬件卸载类：`htb` `hfsc` `cbq` `drr` `qfq` `ets` `prio` `multiq` `mqprio` `taprio` `tbf` `netem`；设了带宽（`bandwidth` 不是 `unlimited`）或带 `autorate-ingress` 的 `cake`；物理网卡上被人设成的 `noqueue`；子队列里有这些的 `mq` | **不动**：这是有人刻意搭的，替换会悄悄毁掉整形配置。只写进 `notes`，整形的 `cake` 连同原因，如 `eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone` |
| 其他不认识的种类；看不到子队列的 `mq` | 同样不动，写进 `notes` |

**怎样替换。** 一个 `mq` 的子队列只能在它新建时按 `default_qdisc` 一次建好，或逐个替换，
而逐个替换每换一个都让整块网卡停发一次（`mq_graft`），64 队列网卡上就是连续 64 次：

- 单队列网卡：`tc qdisc replace dev <网卡> root fq`。
- 多队列网卡，根不是 `tc` 建的 `mq`（内核自己挂的句柄为 0 的 `mq`，或一个可替换的单个
  根）：`tc qdisc replace dev <网卡> root mq`，新 `mq` 的子队列按 `default_qdisc` 创建。所以
  只在重新读到的 `default_qdisc` 确实是 `fq` 时才这么做；不是 `fq`（比如第 2 步写入失败）
  时不对根运行任何 `tc`，只报错
  `<网卡> root qdisc <摘要> left alone: default_qdisc is <x>, a fresh mq would get <x> children`
  ——否则会亲手装上 guard 要去掉的东西，而内核自己的 `mq` 下又不能逐个替换子队列。
- `tc` 建的 `mq`（句柄非 0）：对它 `replace root mq` 是内核接受、却什么都不改的空操作
  （`mq` 没有参数）。子队列全都不是 `fq`（至少两个）、且 `default_qdisc` 是 `fq` 时，执行
  一次 `tc qdisc replace dev <网卡> root handle <M>: mq`，`M` 取该网卡 `tc qdisc show` 输出里
  哪儿都没用到的、从十六进制 8000 起的第一个句柄号（内核自己分配句柄的区间；手写的 `tc`
  脚本习惯用 `1:` 这类小句柄，占用它会让之后的 `replace ... root handle 1: <别的种类>`
  被内核拒绝）：内核新建并嫁接一个子队列全为 `fq` 的 `mq`，网卡只停发一次。只有一个子队列
  不是 `fq`、已有子队列是 `fq`（可能调过 `maxrate` 等参数，新 `mq` 会把它们重建成默认参数）、
  或 `default_qdisc` 不是 `fq` 时，逐个 `tc qdisc replace dev <网卡> parent <H:N> fq`。
- 替换后重新读取核对。第一轮之后仍不全是 `fq`（例如期间有东西改了 `default_qdisc`）时，
  第二轮再替换一次剩下的（`mq` 下就是逐个替换仍不是 `fq` 的子队列），还不行就报错。

`clsact`/`ingress` 不属于根 qdisc，忽略（`skyline_tc` 就挂在 `clsact` 上，替换根 qdisc 不会
碰到它）。网卡不存在、或 `tc` 显示不出根 qdisc（网卡处于 down 状态），只记一条 note。

**失败处理。** 每次调用 `tc` 最多 10 秒，超时即杀掉，并交给后台线程回收，调用方立即拿到
错误。卡在 rtnl 锁上的 `tc` 要等锁释放才会真正退出；在它退出之前，后续每次 `tc` 都立即
失败（`tc <参数>: a previous tc has not exited yet (rtnl lock held?); skipped`），不再启动
新的去一起排队。这样一次卡住的 rtnl 锁最多让一次检查——以及排在它后面的 `drain` 或
`status`——多等约 10 秒，而不是每个 `tc` 各等 10 秒，更不会无限期卡住（周期检查持有
drain 写回 `fallback_cc` 前要拿的那把锁）。没有 `tc`、写入被拒、替换后仍不是 `fq`，都
记进 `last_error` 和 `notes`，从不让 `enable` 失败。

一次替换失败后，周期检查对这块网卡退避，而不是每个周期都试（每次替换都会让网卡短暂停止
发送），也不是就此放弃（多数失败是暂时的，比如一次 netlink 内存不足、一个超时被杀的
`tc`）：第一次在 max(`interval_s`, 30 秒) 之后重试，同一个根 qdisc 连续失败则每次翻倍，最长
5 分钟（`interval_s` 更长时是一个周期）；重试只发生在周期检查上，所以这个时长再向上取整到
`interval_s` 的整数倍，写出来的就是真正重试的时间。等待期间 `notes` 里是
`<网卡> root qdisc <摘要>: the last replace failed; retrying in <N> s`（`N` 是这一档的退避，
不是倒计时，所以日志里每档只记一次；`interval_s = 0` 时没有周期检查，写作
`...; retried on the next ssctl enable ([guard] interval_s = 0)`）。根 qdisc 变成另一个样子时
立即重试；变成 `fq`、替换成功、不再受管，或 `ssctl enable`（不等退避、立即重试），都会
清掉退避。

**日志。** 每一次改动都以一行 `guard: ...` 写到 stderr（即 journald），注明原值、新值和
由谁触发：

```
guard: default_qdisc cake -> fq (applied by ssctl enable)
guard: eth0 root qdisc cake -> fq (applied by ssctl enable)
guard: eth1 (under bond0) root qdisc mq/fq_pie -> mq/fq (something else on this host changed it)
guard: tcp_congestion_control bbr -> skyline_cc (something else on this host changed it)
```

`notes` 与失败只在和上一次检查不同时记一行，不会每 `interval_s` 秒重复刷屏。以
`(something else on this host changed it)` 结尾的行反复出现，说明主机上有别的东西在跟你
抢这几项设置——这是唯一的线索：

```bash
journalctl -u skyline-speederd.service | grep 'guard:'
```

**关掉。** 只想让它管拥塞控制：`[guard] qdisc = false`；不要周期检查：`interval_s = 0`
（`enable` 仍会做一次）；两者都改配置后重启 daemon。`ssctl drain` 让 guard 立即停止守护。
`interval_s = 0` 也是挂载期间让手工改回的默认拥塞控制保持下去的唯一办法（例如只想让
`/sys/fs/cgroup/skyline-speeder` 里的进程走 `skyline_cc`）；但这只维持到下一次 `enable`，
包括开机时 `skyline-speeder-enable.service` 执行的那一次，见
`docs/01-deployment-guide.md` 第 7 节。
guard 不修改 `/etc/sysctl.conf`、`/etc/sysctl.d` 里的任何文件——那些文件每次开机仍会先
生效一次，随后被 `enable` 覆盖。`install.sh` 会在安装结束时列出这类文件。
