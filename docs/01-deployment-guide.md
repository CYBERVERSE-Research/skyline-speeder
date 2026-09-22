# Skyline Speeder 部署与运维指南

## 1. 适用范围与前置条件

Skyline Speeder 只需要部署在 TCP 连接的**服务器发送端**——客户端保持标准 TCP，不需要
任何改动。当前设计面向单向长流场景，尚未针对短流/双向对等流量做验证。

部署前先明确目标机器满足：

- 内核版本满足第 2 节的要求；
- 有 root 权限（安装系统服务、加载 BPF 程序、创建 cgroup 都需要）；
- 目标发送路径的带宽、RTT、丢包特征跟第 11 节"已知风险与部署前评估"里描述
  的适用边界相符。

## 2. 内核版本要求

**精确 ABI 下限是内核 6.10（含）以上；实际支持并验证过的下限是 6.12 LTS。**

`skyline_cc.bpf.c` 里挂在 `struct tcp_congestion_ops.cong_control` 上的回调用 4
个参数声明（`sk, ack, flag, rs`）。这个签名是内核从 v6.10 才开始使用的——
v6.9 及更早版本的 `cong_control` 函数指针类型只有 2 个参数（`sk, rs`），BPF
验证器会在加载时直接拒绝 `skyline_cc`，且报错明确指向参数个数不匹配，不是配置
或编译问题。

**6.1.x 和 6.6.x 这两个 LTS 分支不支持**——两者都停留在旧的 2 参数签名。
`6.10`/`6.11` 满足 ABI 但不是 LTS、支持周期太短，不建议用来做长期部署的目标
内核，只作为版本边界参考。已验证并承诺支持的内核是 **6.12 LTS 及以后**
（含 6.12/6.18/7.1 等已实测通过版本）。

这条限制只影响 `skyline_cc`（M2/M3/M4 拥塞控制）——`skyline_tc`（重传 DSCP 标记）和
`skyline_policy`（M1 tier-2 动态 RTO）不碰 `cong_control`，不受影响，理论上可以
在更旧的内核上独立使用，但当前发布只按整体 6.12 LTS 门槛做验证和支持。

## 3. 构建

依赖：

```bash
sudo apt-get install -y \
  build-essential pkg-config clang llvm libbpf-dev libelf-dev zlib1g-dev \
  bpftool linux-tools-common linux-tools-generic
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup toolchain install 1.75.0 --profile minimal
rustup override set 1.75.0
```

（Debian 系发行版上 `linux-tools-common`/`linux-tools-generic` 可能不存在，
改用 `linux-perf`；`bpftool`/`pahole` 由 `bpftool`/`dwarves` 包提供，包名可能
因发行版而异。）

```bash
make bpf    # 生成 bpf/include/vmlinux.h 并编译三个 CO-RE BPF 对象
cargo build --workspace --release
```

`make bpf` 默认从本机 `/sys/kernel/btf/vmlinux` 读取内核类型信息——**必须在
目标机器本身上执行这一步**（或者在能访问目标内核同版本 `vmlinux`/BTF 数据
的机器上，通过 `make VMLINUX_BTF=<路径> bpf` 显式指定），不能直接搬运在别的
内核版本上编译好的 `.bpf.o`。`bpf/include/vmlinux.h` 是构建产物，不提交进
版本库。

## 4. 安装与部署

`infra/install-guest.sh` 会在**当前机器**上本地执行第 3 节的构建，并把产物
安装到标准系统路径：

```bash
sudo infra/install-guest.sh --confirm-install
```

它会安装/替换：

- `/usr/local/sbin/skyline-speederd`、`/usr/local/bin/ssctl`
- `/opt/skyline-speeder/bpf/*.bpf.o`
- `/opt/skyline-speeder/infra/*.sh`（`apply-guest-profile.sh`/`collect-guest-metrics.sh`/
  `snapshot-skyline-events.sh`/`run-in-skyline-cgroup.sh`）
- `/etc/systemd/system/skyline-speederd.service`、`/etc/systemd/system/skyline-speeder-enable.service`

只在 `/etc/skyline-speeder/speeder.toml` 尚不存在时才会用 `config/speeder-guest.toml` 创建它——
已有配置文件不会被覆盖。

**升级已有安装**：安装脚本只替换文件，**不会重启**已经在运行的 `skyline-speederd`，旧版本会
继续留在内存里。装完先跑第 7 节的第一条命令（`--validate-only --verify-bpf`，不影响正在
运行的 daemon），通过后 `sudo ssctl drain --timeout 60`（把默认算法写回 `fallback_cc`；
单独停止 daemon 不会写回。经 SSH 执行时它总会等到超时并返回非零，这无害），最后
`sudo systemctl restart skyline-speederd.service`。`skyline-speeder-enable.service` 处于 active
时会随之重启并重新挂载；按本指南第 4、7 节安装的主机上它通常不是 active，重启后 `skyline_cc`
处于未挂载状态，原来挂着的话再执行一次 `sudo ssctl enable`。从 0.1.0 升级的完整注意事项见
`CHANGELOG.md` 的 *Upgrading from 0.1.0*。

## 5. 配置文件

生产环境以 `config/speeder-guest.toml` 为模板（对照 `docs/02-interface-reference.md`
第 6 节的完整字段说明）：

- 四个模块（`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`）默认全部
  启用；
- `[rack_tuning]` 整段留空——全局 sysctl 默认值不由 `skyline-speederd` 接管；
- `[rack_rto]` 默认关闭；`[retransmit_dscp]` 默认关闭，`dscp_value` 是占位
  参数，启用前必须由网络侧确定具体编码值。

安装后先检查一遍 `/etc/skyline-speeder/speeder.toml`，确认 `runtime.tc_interface` 指向正确
的发送网卡名，再继续下一步。

## 6. cgroup 前提

**这是最容易被忽略、也最不容易被发现的一步**：M1 tier-2（动态 RTO 调节）
挂在 `/sys/fs/cgroup/skyline-speeder` 这个 cgroup v2 路径上，只有被迁移进这个 cgroup 的
进程建立的连接才会经过这段 BPF 代码。**进程不迁移进去不会报任何错误——只是
静默不生效**，`ssctl status` 里 `rack_rto.stats.applied` 会一直停留在 0。

用 `run-in-skyline-cgroup.sh` 包一层来启动需要被加速的服务进程：

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <你的服务启动命令...>
```

需要 root 只是为了完成 cgroup 迁移这一步；被包装的命令本身会以调用者的普通
用户身份继续执行。

（`skyline_cc`/`skyline_tc` 不受这条限制——`skyline_cc` 是全局 struct_ops，`skyline_tc` 挂在
接口级 TC 上，两者都不区分 cgroup。）

## 7. 启动验证清单

```bash
sudo skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf
sudo systemctl enable --now skyline-speederd.service
timeout 30 sh -c 'until sudo test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'
sudo ssctl enable
sudo ssctl validate
sudo ssctl status
sudo ssctl flows
```

`--validate-only --verify-bpf` 会让三个 BPF 对象都过一遍内核验证器再退出，
不留下任何运行状态——用它可以在正式启动前排除内核版本/BTF 不匹配的问题。

`systemctl enable --now` 只保证进程已经 fork/exec，不保证控制 socket已经
建好——`skyline-speederd.service` 没有 systemd readiness 通知，进程刚起来的头几秒
`/run/skyline-speeder/speeder.sock` 可能还不存在，此时立即执行 `ssctl` 会报
`connect to /run/skyline-speeder/speeder.sock: No such file or directory`，这不代表 daemon
异常，只是启动竞态；上面的 `timeout 30 sh -c '...'` 等 socket 文件出现后
再继续，自动化脚本应保留这一步而不是直接假设 socket 立即可用。

`install.sh` 在启动 daemon 之前会把当前的 `net.ipv4.tcp_congestion_control` 与
`net.core.default_qdisc` 快照到 `/etc/skyline-speeder/pre-install-state`（只写一次，
重装不会把 `skyline_cc` 误记成"原值"）；`--uninstall` 读取它还原。装之前跑 BBR 的
机器，卸载后回到 BBR，而不是停在 `fallback_cc`。

`skyline-speederd.service` 启动后只是常驻进程，不会自动挂载 `skyline_cc`——必须显式执行
`ssctl enable` 才会真正附加拥塞控制算法、让 `enabled_modules` 里配置的
模块生效；不执行这一步，新连接会一直走配置文件里的 `fallback_cc`。

> [!IMPORTANT]
> **`ssctl enable` 是全机生效的。** 它在附加 struct_ops 成功之后会把
> `net.ipv4.tcp_congestion_control` 写成 `skyline_cc`，此后这台机器上**所有**新建
> TCP 连接（不只是 `/sys/fs/cgroup/skyline-speeder` 内进程建的）都默认使用本算法。
> 只想让部分进程走 skyline_cc 的话，不要用 `ssctl enable` 之后再手工改回 sysctl——
> 那会和下一次 `enable` 打架；正确做法是保持全局默认为 `fallback_cc`，靠 cgroup
> 把目标进程圈进来。
>
> 回退是对称的：`ssctl drain` 会先把 sysctl 写回 `fallback_cc`，再等存量连接结束。

`ssctl status` 的 `capabilities` 字段列出六项硬性前提
（`btf`/`bpffs`/`cgroup_v2`/`fq_available`/`struct_ops`/`fallback_cc_available`）——任一为 `false`，
执行 `ssctl validate`/`ssctl enable` 时会直接拒绝，不会带着一个残缺的
能力集运行；`skyline-speederd.service` 进程本身的启动不受这条门槛影响。

## 8. 在线调参与配置切换

M2/M3/M4 的系数和 M1 tier-2 的 RTO 参数都可以在 `skyline-speederd` 运行期间在线调整，
不需要重启：

```bash
sudo ssctl set-module-config --cruise-inflight-gain 3.0 --cruise-pacing-gain 1.25 --guardrail-gain 0.8
sudo ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000
```

两者都是绝对覆盖语义（每次调用都发送完整字段集合），且都提供对应的
`reset-*` 命令恢复为配置文件里的默认值。完整字段列表见
`docs/02-interface-reference.md` 第 2.3/2.4 节。

调整立即生效：`skyline-speederd` 把新系数写入一个非活跃配置槽并递增代际计数器，每条连接
只在 RTT 边界切换到新配置槽，避免同一轮 ACK 处理内读到新旧混杂的值。

## 9. 摘除与回滚

> [!IMPORTANT]
> **通过 SSH 执行 `drain` 必然走到超时**：你自己的 SSH 连接就是一条 skyline_cc 流，
> 它不会在 drain 等待期间结束。这不是故障——drain 在开始等待**之前**就已经把
> `net.ipv4.tcp_congestion_control` 写回 `fallback_cc`，所以无论超时与否都不会再有
> 新连接使用 skyline_cc；struct_ops 保持挂载直到最后一条存量流结束，这是内核的引用
> 计数行为。要真正等到归零，从串口控制台或一条不走本机 skyline_cc 的通道执行。

优雅摘除用 `drain`：

```bash
sudo ssctl drain --timeout 60
```

它会先阻止新连接被分派到 `skyline_cc`，等待已有连接自然结束（默认超时 300 秒，
可覆盖），确认无活跃连接后再注销 struct_ops——新建立的连接此后落回配置文件
里的 `fallback_cc`。M1 tier-2 的 RTO 调节和重传 DSCP 标记不受这个开关影响，
需要单独用 `ssctl reset-rack-rto`/`reset-retransmit-dscp` 关闭。

正常停止服务（`systemctl stop skyline-speederd.service`，等价于收到 SIGTERM/SIGINT）会
正确注销全部 struct_ops 和 BPF 链接，重启服务前也不需要手动清理残留状态；但它**不会**把
`net.ipv4.tcp_congestion_control` 写回 `fallback_cc`。`skyline-speeder-enable.service` 处于
active 时，停止 daemon 会先停这个 unit，由它的 ExecStop 写回；按第 7 节用 `ssctl enable`
挂载的主机上它不是 active，停止或重启 daemon 之前先执行 `sudo ssctl drain --timeout 60`
（经 SSH 必然走到超时，无害，见上方说明）。

彻底卸载：先 `sudo ssctl drain --timeout 60`，再
`sudo systemctl disable --now skyline-speeder-enable.service skyline-speederd.service`，然后删除
第 4 节列出的全部安装路径。

## 10. 可观测性与排障

`ssctl status` 的 `metrics` 字段（`skyline_cc` 从未启用过时为 `null`）暴露决策
计数器；事件环形缓冲区落盘到配置里的 `runtime.events_path`，记录状态切换/
护栏触发/配置代际切换等事件。完整字段与事件码含义见
`docs/02-interface-reference.md` 第 4/8 节。

事件日志位于 `/run`（内存 tmpfs），大小受 `runtime.events_max_mib` 限制（默认 8 MiB，
满了轮转为 `events.jsonl.1`，最多约占两倍）；不需要事件时设为 `0` 即可关闭。
早于此上限的版本会无限增长，可能写满 `/run`，使 Docker 等依赖 `/run` 的服务失败。
在这类版本上用 `truncate -c -s 0 /run/skyline-speeder/events.jsonl` 回收空间，不要用
`rm`：daemon 仍持有文件句柄，删除后空间不会释放。

**M1 tier-2 排障决策树**（`rack_rto.stats` 一直是 0 时）：

1. `rack_rto.stats.rtt_callbacks` 也是 0 → BPF 程序完全没被调用，先检查目标
   进程是不是真的在 `/sys/fs/cgroup/skyline-speeder` 里（见第 6 节）；
2. `rtt_callbacks` 非零但 `subscribe_ok` 是 0、`subscribe_err` 非零 → 订阅
   本身失败，检查内核版本的 sockops 能力；
3. `subscribe_ok` 非零但 `applied` 是 0、`rejected` 非零 → 订阅成功但内核拒
   绝了下发的值，多半是取值超出了内核自身对 RTO 的合法区间。

一个容易误判的现象：双栈监听 socket 上的 IPv4 连接会以 IPv4-mapped
`AF_INET6`（`::ffff:a.b.c.d`）形态出现，不是纯 `AF_INET`——这是大多数不显式
绑定地址族的服务的常态，不是异常。Skyline Speeder 的 family 判断同时接受两种形态，如果
自己新增代码需要判断地址族，要留意这一点。

## 11. 已知风险与部署前评估

Skyline Speeder 的自保护机制只识别两个真实拥塞信号：队列时延增长和 ECN 标记。**部署前
需要评估目标链路的丢包主要是随机丢包还是排队造成的**——如果链路的丢包本质
上来自网络设备排队（而不是无线链路层这类跟排队无关的随机丢失），Skyline Speeder 的丢包
补偿机制和排队护栏机制会有相反的作用方向，需要结合实际链路特征评估是否
适用。

其余已知的测试有效性边界（离线负载能力上限、BBR 版本不可调等）和局限性
清单见 `docs/04-performance-report.md` 第 9 节。

## 12. 复现性能测试指引

如果需要在自己的测试环境里重新验证第 4 章描述的性能结果，完整的复现步骤和
所需资源见 `research/experiments/README.md`。
