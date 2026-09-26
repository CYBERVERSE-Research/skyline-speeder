# Skyline Speeder —— Agent 部署手册

面向**自动化 agent / 无人值守脚本**的确定性部署手册。每一步都给出可机器判定的
成功条件与已知失败模式的精确处置。人类操作者请优先阅读
[docs/01-deployment-guide.md](docs/01-deployment-guide.md)。

约定：所有命令以 `root` 执行。`$?` 非零即失败，不要继续下一步。

---

## 0. 一句话路径

```bash
sudo ./install.sh              # 在本机从源码构建
sudo ./install.sh --prebuilt   # 或：安装已发布的产物，不需要工具链（两条路径的区别见 §2）
```

退出码非零即失败。输出约定（便于机器解析）：stdout 不是终端时，每一步开始、结束各打印
一行 `[ n/N] <步骤名>` / `[ n/N] done in <耗时>`，最后是 `all N steps done in <耗时>`，
随后是安装摘要与使用指南；终端上则是一行原地刷新的进度条。所有子命令（apt、rustup、
make、cargo、验证器）的输出写入 `/var/log/skyline-speeder-install.log`，每次运行开头清空、
结束后保留。失败时打印 `error <原因>`、失败那一步在日志里的最后至多 25 行，以及日志路径。
加 `--verbose`（或环境变量 `SKYLINE_VERBOSE=1`）则不画进度条，子命令输出同时打到屏幕。

成功判据：

```bash
v=$(skyline-speederd --version 2>/dev/null | awk '{print $2}')
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = "skyline_cc" ] \
  && ssctl status --json | grep -q '"enabled": true' \
  && { [ -z "$v" ] || ssctl status --json | grep -q "\"version\": \"$v\""; }
```

> `--json` 是 0.3.0 起的写法：`ssctl status` 默认打印的是人类可读报告，`--json` 打印
> 0.2.0 那份逐字节一致的 JSON。对着 0.2.0 及更早的 `ssctl` 要去掉 `--json`（它不认识这个
> 选项）。下面所有 grep/python 判据同理。

第三条确认内存里跑的就是刚装上的 daemon：`skyline-speederd --version` 读的是磁盘上的
二进制，`status` 里的 `version` 来自正在运行的进程。已发布的 v0.2.0 及更早的二进制没有
`--version`，它们的 `ssctl status` 也没有 `version` 字段，所以对它们（`--prebuilt` 在出现
更新的 release 之前装的就是 v0.2.0）第三条跳过；新的 `ssctl` 查询旧 daemon 时这个字段显示为
空字符串。注意两个 release 之间从 main 构建的版本号可能仍与上一个 release 相同，判断有没有
guard 看 `status` 里有没有 `"guard"` 键，不要只看版本号。

> **升级已有安装**：按原来的方式重新运行 `install.sh`（当初带了 `--prebuilt`/`--no-enable`
> 的照样带上）。检测到 `skyline-speederd.service` 处于 active 时，它在新对象通过内核验证器
> 之后自己重启 daemon：enable unit 处于 active 时随之重启——其 ExecStop
> （`boot-disable.sh`）执行 `ssctl drain`：drain 先解除 guard、写回 `fallback_cc`，再等存量
> 连接至多 60 秒（经 SSH 必然走到超时，无害），新 daemon 起来后重新挂载。用 `ssctl` 做的在线
> 修改只存在旧 daemon 的内存里，重启后不保留；安装器把旧的 `ssctl status --json` 写进了安装日志，
> 需要时照着重新下发。
>
> `skyline_cc` 是用裸 `ssctl enable` 挂载的主机（enable unit 不是 active，而旧 daemon 报告
> `"enabled": true` 或默认拥塞控制是 `skyline_cc`）也不需要手工处理：没有 ExecStop 替它
> drain，daemon 自己停止时又刻意不写 sysctl，所以安装器在重启前自己执行
> `timeout 75 ssctl drain --timeout 60`（失败只记日志）；重启后不带 `--no-enable` 时由 enable
> unit 挂载（此后开机自启），带 `--no-enable` 时再执行一次 `ssctl enable`（仍是手工挂载，
> 开机不自动挂）。
> 各版本的升级注意事项见 `CHANGELOG.md` 里对应的 *Upgrading from ...* 小节（从 0.2.0 升级见
> *Upgrading from 0.2.0*，其中包括 `ssctl enable` 现在也会设置 `fq` 这一行为变化）。

---

## 1. 硬性前置条件

**任何一条不满足都不要继续**——`ssctl enable` 会直接拒绝，
而不会带着残缺的能力集运行。

| 条件 | 判定命令 | 要求 |
|---|---|---|
| 内核版本 | `uname -r` | **>= 6.12**（ABI 下限 6.10） |
| 内核 BTF | `test -r /sys/kernel/btf/vmlinux` | 存在（`CONFIG_DEBUG_INFO_BTF=y`） |
| bpffs | `mount \| grep -q ' /sys/fs/bpf '` | 已挂载 |
| cgroup v2 | `grep -qw cgroup2 /proc/filesystems` | 支持 |
| `fq` qdisc | `modprobe sch_fq` | 可加载 |
| struct_ops | 见 §4 的 `capabilities` | `true` |

> [!CAUTION]
> **`6.1.x` 与 `6.6.x` 两个 LTS 分支明确不支持。** 二者的
> `tcp_congestion_ops.cong_control` 仍是 2 参数签名（`sk, rs`），而
> `skyline_cc.bpf.c` 按 4 参数（`sk, ack, flag, rs`）声明——BPF 验证器会在加载时
> 直接拒绝，报错明确指向参数个数不匹配。**这不是配置问题，不要尝试绕过。**

---

## 2. 两条安装路径：源码构建与预编译产物

| | 源码构建（默认） | 预编译产物（`--prebuilt`） |
|---|---|---|
| 命令 | `./install.sh` | `./install.sh --prebuilt`；`./install.sh --release <tag>` 固定到某个版本（隐含 `--prebuilt`） |
| 目标机需要 | 编译工具链（clang/LLVM、bpftool、libbpf/libelf/zlib 开发包、Rust），安装器自动安装 | **不需要任何工具链**，只要 `curl`、`tar`、`iproute2`（安装器自动安装）；但预编译的 `skyline-speederd` 在 Ubuntu 24.04 上构建，需要 **glibc ≥ 2.38** 以及 `libelf.so.1`、`libz.so.1`（Debian 13、Ubuntu 24.04 及更新版本满足） |
| BPF 对象的类型来源 | 本机 `/sys/kernel/btf/vmlinux` | 发布流水线固定的 6.12 LTS 参考头，加载时由 CO-RE 按本机内核的 BTF 修正字段偏移 |
| 完整性校验 | 源码树本身（`scripts/bootstrap.sh` 可用 `SKYLINE_SHA256` 固定 tarball 摘要） | 产物旁的 `.sha256` 必须匹配，不匹配即中止；取不到 `.sha256` 时告警、只信任 TLS |

两条路径的其余步骤完全相同（同样过验证器、同样的 systemd unit 与配置模板）。

**预编译产物从哪来。** 默认取 GitHub 上的最新 release；`--release <tag>` 取指定版本；
环境变量 `SKYLINE_ARTIFACT_URL` 跳过 release 查找，直接使用一个 https URL（镜像、内网
制品库）或本机上的一个文件路径（无外网的主机：把 tarball 连同它的 `.sha256` 一起拷过去）。
校验文件按 `<URL 或路径>.sha256` 查找：

```bash
SKYLINE_ARTIFACT_URL=https://mirror.example/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
```

产物内的 `MANIFEST`（tag、commit、参考头摘要、每个二进制与对象的 SHA-256）写进安装日志，
安装摘要里只显示一行出处。

**早于 guard 的 release。** 预编译产物是已发布的 release，可能比运行的 `install.sh` 旧。
已发布的 v0.2.0 及更早的 release 没有 guard（§10）：`ssctl status --json` 里没有 `"guard"` 键（判断
依据是这个键，不是版本号），`ssctl enable` 本身不碰 qdisc，之后也不守住默认拥塞控制；但同一
release 附带的 enable unit（它自己的 `boot-enable.sh`）在每次挂载和每次开机时写一次
`default_qdisc=fq`，不换网卡的根 qdisc，也没有谁守住这个值。安装器据此识别并告警
（`... is a release older than the guard ...`），摘要的 qdisc 行注明
`(not managed by this release)`；此时 §6 的 G7 与 §0 成功判据的第三条不适用。需要 guard 就用源码构建。用户态早于 glibc 2.38 的主机（例如 Debian 12 + backports
内核）上预编译的 daemon 起不来，`--prebuilt` 会在验证步骤失败——改用源码构建（这一组合
尚未测试）。

**源码构建。** `make bpf` 默认从本机 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`。对象是
CO-RE 的：头文件只需定义代码用到的类型，字段偏移在加载时按运行内核修正——发布产物正是
这样在 6.12 参考头上编译、在更新的内核上加载的。为别的机器构建时，显式指定对方内核的
BTF，或直接给一份现成的头（这条路径不需要 bpftool，也不需要本机 BTF）：

```bash
make VMLINUX_BTF=/path/to/target/vmlinux bpf
make PREBUILT_VMLINUX_H=/path/to/vmlinux.h bpf
```

CO-RE 修得了偏移，修不了字段改名或删除：对象换到另一个内核上，**必须**先在目标机上跑 §4
的验证器检查。`bpf/include/vmlinux.h` 是构建产物，已在 `.gitignore` 中排除，不要提交。

---

## 3. 分步部署

```bash
# 3.1 工具链
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2   # guard 用 tc 维护网卡 qdisc

# Debian 12 + bookworm-backports 的 6.12 内核：linux-headers-* 把 libelf1 带到 0.192，
# bookworm 的 libelf-dev 依赖 libelf1 (= 0.188-2.1)，上面这条会以
# "E: Unable to correct problems, you have held broken packages" 失败。
# 只换这一个包，取与机上库同源的版本（版本号以 apt-cache madison 为准）：
#   apt-get install -y libelf-dev=0.192-4~bpo12+1
# 不要用 -t bookworm-backports：那会连 curl/iproute2/bpftool/libbpf1 一起换源。
# install.sh 会自动做这件事（先让 apt 出计划，只替换放不下去的那个包）。

# 3.2 Rust（rust-toolchain.toml 已固定 channel，rustup 会自动遵循）
command -v cargo >/dev/null || {
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
}

# 3.3 构建并安装
./infra/install-guest.sh --confirm-install

# 3.4 出口网卡 —— 模板默认值是测试床的接口名，几乎不会匹配真实主机
# 取 "dev" 后面那个词：`default dev wg0 scope link` 这类没有 via 的路由里它不在第 5 列。
# 先 IPv4、再 IPv6（纯 IPv6 主机没有 IPv4 默认路由），只取以太网设备（type 1，
# VLAN/bond/bridge 也是）：skyline_tc 不挂在 WireGuard/tun 这类隧道上
DEV=$({ ip -o route show default; ip -6 -o route show default; } 2>/dev/null \
  | awk '$1 == "default" { for (i = 2; i < NF; i++) if ($i == "dev" && $(i + 1) != "lo") print $(i + 1) }' \
  | while read -r d; do
        if [ "$(cat "/sys/class/net/$d/type" 2>/dev/null)" = 1 ]; then echo "$d"; break; fi
    done)
test -n "$DEV"    # 失败：没有默认路由，或默认路由只走隧道——见下方说明
sed -i "s|^tc_interface = \".*\"|tc_interface = \"$DEV\"|" /etc/skyline-speeder/speeder.toml
grep '^tc_interface' /etc/skyline-speeder/speeder.toml
```

`test -n "$DEV"` 失败而默认路由存在，说明它只经 WireGuard/WARP、tun、gre、ppp 这类三层
隧道离开：把 `tc_interface` 手工设成承载隧道流量的那块物理网卡（`install.sh` 此时同样保留
模板值并告警）。`tc_interface` 是 VLAN、bond 或网桥时照填即可，guard 维护的是它下面的
物理网卡（§10）。

> **升级已有安装**：`install-guest.sh` 只替换文件，不会重启已在运行的 `skyline-speederd`
> （`install.sh` 会，见 §0——走一键路径的主机不需要下面这段）。第 4 节验证通过后执行：
>
> ```bash
> # 按第 5 节挂载的主机上 enable unit 是 active：restart 会连带重启它，它的 ExecStop
> # （boot-disable.sh）会执行 drain（drain 自己先把默认算法写回 fallback_cc），无需手动 drain。
> # 只有它不是 active（skyline_cc 由裸 `ssctl enable` 挂载）时才要手动 drain：单独停止
> # daemon 不会写回 fallback_cc。经 SSH 执行的 drain 必然超时并返回非零（见第 8 节），属预期。
> systemctl is-active -q skyline-speeder-enable.service || ssctl drain --timeout 60 || true
> systemctl is-active -q skyline-speeder-enable.service \
>   || [ "$(sysctl -n net.ipv4.tcp_congestion_control)" != skyline_cc ]
> systemctl restart skyline-speederd.service
> ```
>
> enable unit 此前不是 active、且该主机应挂载 skyline_cc 的话，restart 后按第 5 节启动它。各版本的
> 升级注意事项见 `CHANGELOG.md` 里对应的 *Upgrading from ...* 小节。

---

## 4. 启动前验证（不留运行状态）

```bash
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf
```

该命令把三个 BPF 对象都过一遍内核验证器后退出，**不留下任何运行状态**。
用它在正式启动前排除内核版本 / BTF 不匹配的问题。

输出中 `capabilities` 的六项硬性前提必须全为 `true`：

```bash
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf \
  | python3 -c 'import json,sys; c=json.load(sys.stdin)["capabilities"]; \
      assert all(c[k] for k in ("btf","bpffs","cgroup_v2","fq_available","struct_ops","fallback_cc_available")), c; \
      print("capabilities OK")'
```

`rack_reo_hook: false` 是**正常的**——它只表示 early-loss 停留在观测模式，
不阻断部署。

---

## 5. 启动序列（顺序不可颠倒）

```bash
# 5.1 启动常驻进程
systemctl enable --now skyline-speederd.service

# 5.2 等待控制 socket —— 必须等，不能假设立即可用
timeout 60 sh -c 'until test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'

# 5.3 挂载 struct_ops 并切换默认算法（开机自启同样走这条路径）；[guard] qdisc = true
#     （默认）时同时把 default_qdisc 与 tc_interface（VLAN/bond/bridge 则是其下物理网卡）
#     的根 qdisc 设为 fq，并开始守护（§10）
systemctl enable --now skyline-speeder-enable.service
```

> [!IMPORTANT]
> **`skyline-speederd.service` 启动后不会自动挂载 `skyline_cc`。** 它只保持进程常驻；
> 不执行第 5.3 步，新连接会一直走配置文件里的 `fallback_cc`——**且不报任何错误**。
> `skyline-speeder-enable.service` 正是为消除这个静默失效而存在，它内部会先等待
> socket 再执行 `ssctl enable`，规避 §5.2 的启动竞态。

---

## 6. 验证门禁

```bash
# G1 daemon 与挂载单元均已启动
systemctl is-active skyline-speederd.service skyline-speeder-enable.service

# G2 struct_ops 已注册（需要 bpftool；--prebuilt 主机上没有它，以 G3 为准）
bpftool struct_ops show | grep -q skyline_cc

# G3 算法已进入内核可用列表
sysctl -n net.ipv4.tcp_available_congestion_control | grep -qw skyline_cc

# G4 已成为系统默认
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = "skyline_cc" ]

# G5 控制面自述已启用
ssctl status --json | grep -q '"enabled": true'

# G6 开机自启
systemctl is-enabled skyline-speederd.service skyline-speeder-enable.service

# G7 guard 已武装；qdisc 已就位（[guard] qdisc = false 时跳过后两条，
#    没有设置 runtime.tc_interface 时跳过最后一条）。最后一条看 guard 实际维护的网卡
#    （live.devices：tc_interface 自己，或它是 VLAN/bond/bridge 时下面的物理网卡）
ssctl status --json | grep -q '"armed": true'
[ "$(sysctl -n net.core.default_qdisc)" = fq ]
ssctl status --json | python3 -c 'import json,sys; d=json.load(sys.stdin)["status"]["guard"]["live"]["devices"]; \
    assert d and all(x["qdisc"] in ("fq", "mq/fq") for x in d), d; print("qdisc OK:", d)'
```

G7 后两条失败时看 `ssctl status` 的 DRIFT GUARD 段落（`--json` 里是 `guard` 的 `notes` 与
`last_error`，§10）：根 qdisc 是
`htb`/`tbf`/`netem` 等刻意搭建的整形结构、或设了带宽的 `cake` 时，guard 按设计不动它；
`tc_interface` 是隧道、下面没有物理网卡时 `devices` 为空——这两种都不是部署失败。

**G8 —— 重启存活性**（强烈建议）：

```bash
systemctl reboot
# 重启后重新执行 G1–G7
```

**G9 —— 实流验证**：确认真实连接确实走了该算法，而不只是 sysctl 值正确。

```bash
ss -tin | grep -c skyline_cc                       # 应 > 0
ssctl flows                                        # 逐条列出这些连接与它们的参数
ssctl status --json | grep -E 'active_flows|tc_stats'
```

`ssctl flows` 的 CONNECTIONS 段落直接给出同一份数据：coverage 那行是"主机上多少条 TCP
连接跑在 skyline_cc 上"，下面是逐条的 RTT、cwnd、pacing、交付速率与重传占比。它和上面
`ss -tin | grep -c` 数的是同一个东西——`ssctl flows` 就是 daemon 替你跑了那条 `ss`。

---

## 7. cgroup 前提（最容易被忽略）

`skyline_policy` 的动态 RTO 调节挂在 `/sys/fs/cgroup/skyline-speeder`，
**只有该 cgroup 内进程建立的连接**才会经过这段 BPF 代码。
**进程不迁移进去不报任何错误，只是静默不生效。**

```bash
sudo /opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <服务启动命令...>
```

systemd 服务可用 drop-in 迁移（`+` 前缀提权，写 `cgroup.procs` 需要 root）：

```ini
# /etc/systemd/system/<unit>.d/10-skyline-cgroup.conf
[Unit]
After=skyline-speederd.service skyline-speeder-enable.service

[Service]
ExecStartPost=+/bin/sh -c 'echo $MAINPID > /sys/fs/cgroup/skyline-speeder/cgroup.procs'
```

诊断决策树（`rack_rto.stats` 长期为 0 时）：

| 现象 | 结论 |
|---|---|
| `rtt_callbacks` 也是 0 | BPF 完全没被调用 → 目标进程不在 cgroup 内 |
| `rtt_callbacks` 非 0、`subscribe_ok`=0、`subscribe_err` 非 0 | 订阅失败 → 检查内核 sockops 能力 |
| `subscribe_ok` 非 0、`applied`=0、`rejected` 非 0 | 内核拒绝下发值 → 取值超出合法 RTO 区间 |

`skyline_cc`（全局 struct_ops）与 `skyline_tc`（接口级 TC）**不受**此限制。

---

## 8. 摘除与回滚

```bash
# 优雅摘除：解除 guard，阻止新连接进入，等存量连接自然结束
ssctl drain --timeout 60

# 停止（等价于 SIGTERM，会正确注销全部 struct_ops 与 BPF 链接）
systemctl stop skyline-speeder-enable.service skyline-speederd.service

# 彻底卸载：切到 bbr + fq，卸掉安装时装上的工具链（保留 /etc/skyline-speeder 配置）
sudo ./install.sh --uninstall

# 同上，但还原安装前的 cc/qdisc 而不是 bbr + fq
sudo ./install.sh --uninstall --restore-pre-install
```

`drain` 经 SSH 执行必然超时（执行者的 SSH 连接本身就是一条 skyline_cc 存量流）；
sysctl 在等待前已写回 `fallback_cc`、guard 在那之前已解除，故超时不影响"新连接不再使用
skyline_cc"这一结果。`drain` 不动 qdisc，`default_qdisc` 与网卡根 qdisc 保持 `fq`。

单独停止 `skyline-speederd` 不写任何 sysctl（刻意如此）：enable unit 处于 active 时，停止
daemon 会先停这个 unit，由它的 ExecStop（`boot-disable.sh`）写回 `fallback_cc`——它先
`ssctl drain`，之后只在默认仍是 `skyline_cc`（daemon 已经不在）时才自己写一次；反过来先写
sysctl 的话，仍处于武装状态的 guard 会把它改回 `skyline_cc`。`skyline_cc` 是用裸
`ssctl enable` 挂载的主机上，停止之前先 `ssctl drain`（`install.sh` 升级时会自己做这一步，
见 §0）。

`--uninstall` 先 drain（同时解除 guard），停用删除两个 unit、二进制与 `/opt/skyline-speeder`，
然后做两件事：

1. **切到 bbr + fq**（默认）：`net.ipv4.tcp_congestion_control=bbr`、
   `net.core.default_qdisc=fq`，并把 `runtime.tc_interface` 解析出来的网卡（guard 武装时维护
   的就是这些；与 `[guard] qdisc` 是否关过无关）根 qdisc 确认成 `fq`（多队列网卡是子队列全为
   `fq` 的 `mq`）。已经是 `fq` 的不动；`htb`/`tbf`/`netem`/设了带宽的 `cake` 这类
   看起来特意建的原样留下、只告警并给出手工命令（与 guard 不替换它们同理）。内核没有 bbr 时
   先 `modprobe tcp_bbr`，仍然没有则退回 `PRE_INSTALL_CC` → cubic → reno，并告警用了哪个。
   **只在本次运行时生效**：不写 `/etc/sysctl.d`，重启后仍由那里的文件决定，结束时会提示。
2. **卸掉安装时装上的包**：按 `/etc/skyline-speeder/added-packages`（安装时取 apt 执行前后
   dpkg 已安装集合的差集、再与 apt 为这次请求接受的计划取交集后写入，因此含 apt 顺带拉进来的
   依赖，不含主机本来就有的包，也不含并行跑着的 unattended-upgrades 装的包）执行
   `apt-get --purge remove`。四道护栏：`iproute2`/`curl`/`ca-certificates`/`tar` 永不移除；
   **这几个包递归依赖的东西也永不移除**（否则「保留 curl、删掉它底下的 libcurl4t64」这个矛盾
   会被 apt 用「连 curl 一起删」来解决，实测会导致整条工具链一个都卸不掉）；dpkg 优先级
   `required`/`important` 的永不移除（按架构分别判断）；先用 `apt-get -s --purge remove`
   出计划，其中 `Purg`/`Remv` 一旦出现清单之外的包，就把被它依赖的那个包单独留下并告警说明是
   谁需要它，其余照卸、重新出计划（最多三轮），三轮后仍不干净才**一个都不卸**、改为打印手工
   命令。rustup 仅在 `/etc/skyline-speeder/added-rustup` 存在时移除。这一步的失败一律只是
   告警——此时本体已经卸完，中断卸载更糟。

加 `--restore-pre-install` 时第 1 步换成从 `/etc/skyline-speeder/pre-install-state`
还原安装前的拥塞控制算法与默认 qdisc；快照里记有网卡及其根 qdisc
（`PRE_INSTALL_QDISC_DEV`/`PRE_INSTALL_ROOT_QDISC`：一一对应、以空格分隔的两个列表，
guard 管几块网卡就有几项；两个列表长度不同时一块都不还原，只告警）时，逐块把根 qdisc 的
**种类**还原回去——只在它仍是本项目留下的 `fq`/`mq/fq` 时才动，且只还原种类、用默认参数
（例如 `cake` 的带宽设置不会回来）；`mq` 下混有多种 qdisc 的不还原，只告警。还原
`mq/<种类>`：临时把 `default_qdisc` 设成该种类，`tc qdisc del dev <网卡> root` 让内核按它重建
自己的 `mq`（对 guard 留下的、由 `tc` 建的 `mq` 执行 `replace root mq` 是空操作），删不掉
（内核自己的句柄 0 的 `mq`）时才 `tc qdisc replace dev <网卡> root mq`，然后设回
`default_qdisc`、重新读取核对。新写的快照总带着这两个键（不知道网卡时值为空）；0.2.0 写的
快照完全没有这两个键，只有它会在升级时被补上（已有的键不动）。两个键为空或不存在时跳过这
一步。

M1 tier-2 的 RTO 调节与重传 DSCP 标记**不受 drain 影响**，需单独关闭：

```bash
ssctl reset-rack-rto
ssctl reset-retransmit-dscp
```

### 8.1 已知行为：struct_ops 在连接结束前不会消失

停止服务或卸载后，`bpftool struct_ops show` 可能仍列出 `skyline_cc` 条目。
**这不是失败。** struct_ops map 只要还有 socket 引用就不会被内核释放——与内核模块
"in use" 时 `rmmod` 失败是同一类引用计数行为。

判据（确认内核注册表本身是干净的）：

```bash
# 应恰好输出 1（启用中）或 0（已停用），绝不会因为残留 map 而出现重复注册
sysctl -n net.ipv4.tcp_available_congestion_control | tr ' ' '\n' | grep -c '^skyline_cc$'

# 仍在引用它的连接数
ss -tin | grep -c skyline_cc
```

残留 map 会在这些连接自然结束后、或重启后由内核回收。**不要**强制 detach 仍被
活跃 socket 使用的 struct_ops。

---

## 9. 部署前的链路适用性评估

> [!WARNING]
> 自保护机制只识别两个真实拥塞信号：队列时延增长与 ECN 标记。若链路丢包本质来自
> **设备排队**而非链路层随机丢失，丢包补偿与排队护栏会有**相反的作用方向**。

判别方法——固定速率 UDP 打流，逐级加压：

```bash
for r in 100M 200M 400M 800M; do
    iperf3 -c <peer> -u -b $r -t 12 | tail -3
done
```

| 观察结果 | 结论 |
|---|---|
| UDP 各速率均**零丢包**，而 TCP 大量重传 | 丢包是**自身突发造成的排队溢出**，非随机丢包 → **本方案不适用** |
| UDP 在低速率下即有稳定丢包 | 链路层随机丢包 → 适用 |

同时注意 ICMP 丢包率**不能**作为判据：多数网络对 ICMP 限速，其丢包率与数据面无关。

---

## 10. 外部改动 cc/qdisc：guard

`ssctl enable` 成功后直到下一次 `drain`，`skyline-speederd` 每 `[guard] interval_s` 秒
（默认 5）检查一次：`net.ipv4.tcp_congestion_control` 是否仍是 `skyline_cc`，以及
（`[guard] qdisc = true`，默认）`net.core.default_qdisc` 与受管网卡的根 qdisc 是否仍是
`fq`（多队列网卡上是子队列全为 `fq` 的 `mq`），被改了就改回去。受管网卡是
`runtime.tc_interface` 自己；它的根是内核默认的 `noqueue`（VLAN、bond、网桥）时，是顺着
`lower_*` 找到的、有 `device` 链接的物理网卡（tap、veth 从不受管），`ssctl status` 的
DRIFT GUARD 段落（`--json` 里是 `guard.live.devices`）列出它们。它针对的是
"一键 BBR"脚本留下的 `/etc/sysctl.d` 文件被 `sysctl --system` 重新应用、或有人用 `tc`
装了 `cake`/`fq_pie`——没有它，这类改动会让新连接悄悄绕开 `skyline_cc` 且不报任何错误。
完整规则见 `docs/02-interface-reference.md` 第 9 节。

判定命令：

```bash
ssctl status                 # DRIFT GUARD 段落
ssctl status --json | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["status"]["guard"], indent=2))'
journalctl -u skyline-speederd.service | grep 'guard:'
```

| 现象 | 结论与处置 |
|---|---|
| `cc_restored` 持续增长，日志反复出现 `guard: tcp_congestion_control ... (something else on this host changed it)` | 主机上有东西在反复写这个 sysctl（常见：`sysctl --system`/`sysctl -p` 重新应用了 `/etc/sysctl.conf` 或 `/etc/sysctl.d/*.conf` 里的 `bbr`）。guard 会一直改回，生效不受影响；要消除来源，找出并删掉那一行。guard 与安装器都**不改**这些文件 |
| `notes` 含 `looks deliberate; left alone`，或 `is not a kind the guard knows is safe to replace` | 网卡根 qdisc 是整形/分类/卸载类（`htb`/`tbf`/`netem`/`mqprio` 等）、设了带宽或 `autorate-ingress` 的 `cake`（note 里带着原因，如 `cake (bandwidth 90Mbit)`）、物理网卡上被人设成的 `noqueue`，或未知种类，guard 按设计不动它，以免毁掉整形配置 |
| `notes` 含 `is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard` | `tc_interface` 是根为 `noqueue` 的隧道（例如内核 WireGuard/wgcf WARP）或只挂着 tap 的网桥，下面找不到物理网卡，guard 不检查任何根 qdisc。把 `runtime.tc_interface` 改成承载流量的物理网卡并重启 `skyline-speederd` |
| `capabilities.notes` 含 `is not an Ethernet device` | `tc_interface` 不是以太网设备，`skyline_tc` 不挂载（`set-retransmit-dscp` 随之失败）。处置同上 |
| `last_error`/`notes` 含 `tc is not installed` | 安装 `iproute2`，然后 `ssctl enable` |
| `notes` 含 `the last replace failed; retrying in <N> s` | 一次替换没有生效，周期检查按退避重试（30 秒起、每次翻倍、最长 300 秒）；原因看 `last_error`。排除后想立即重试执行 `ssctl enable`。`interval_s = 0` 时写作 `retried on the next ssctl enable` |
| `last_error` 含 `left alone: default_qdisc is` | `default_qdisc` 不是 `fq`（写入被拒，或有东西马上又改了它），多队列网卡上新建的 `mq` 会按它建子队列，所以 guard 不动根 qdisc。先查谁在改 `default_qdisc` |
| `last_error` 含 `did not finish within 10000ms and was killed`，或 `a previous tc has not exited yet (rtnl lock held?); skipped` | `tc` 卡在内核的 rtnl 锁上：被杀掉的 `tc` 退出之前，guard 的每个 `tc` 都立即跳过，不会越积越多；锁释放后下一次检查自动恢复。持续出现时查是谁长时间持有 rtnl（例如卡住的网卡驱动） |
| `enabled` 为 `true` 而 `armed` 为 `false` | drain 超时后 struct_ops 仍挂着但 guard 已解除——这是 drain 的正常结果；要恢复加速执行 `ssctl enable` |

退出：只让它管拥塞控制，设 `[guard] qdisc = false`；不要周期检查，设 `interval_s = 0`
（`enable` 仍会做一次）。两者都要重启 `skyline-speederd` 才生效。挂载期间手工改回的默认
拥塞控制只有在 `interval_s = 0` 时才保持得住（只让 cgroup 内进程走 `skyline_cc` 的做法），
且只到下一次 `enable` 为止，包括开机时 enable unit 执行的那一次，见
`docs/01-deployment-guide.md` 第 7 节。
