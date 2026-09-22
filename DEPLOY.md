# Skyline Speeder —— Agent 部署手册

面向**自动化 agent / 无人值守脚本**的确定性部署手册。每一步都给出可机器判定的
成功条件与已知失败模式的精确处置。人类操作者请优先阅读
[docs/01-deployment-guide.md](docs/01-deployment-guide.md)。

约定：所有命令以 `root` 执行。`$?` 非零即失败，不要继续下一步。

---

## 0. 一句话路径

```bash
sudo ./install.sh
```

成功判据：

```bash
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = "skyline_cc" ] \
  && ssctl status | grep -q '"enabled": true'
```

> **升级已有安装**：`install.sh` 不会重启已在运行的 `skyline-speederd`，上面的判据在旧 daemon
> 上也照样成立。先执行 `systemctl stop skyline-speeder-enable.service skyline-speederd.service`
> 再运行 `./install.sh`，并追加判据
> `! readlink /proc/$(systemctl show -p MainPID --value skyline-speederd)/exe | grep -q '(deleted)'`。
> 从 0.1.0 升级的完整注意事项见 `CHANGELOG.md` 的 *Upgrading from 0.1.0*。

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

## 2. 构建必须在目标机器上进行

`make bpf` 从本机 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`。**不能搬运在其他
内核版本上编译好的 `.bpf.o`。** 交叉构建须显式指定目标内核的 BTF：

```bash
make VMLINUX_BTF=/path/to/target/vmlinux bpf
```

`bpf/include/vmlinux.h` 是构建产物，已在 `.gitignore` 中排除，不要提交。

---

## 3. 分步部署

```bash
# 3.1 工具链
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool curl

# 3.2 Rust（rust-toolchain.toml 已固定 channel，rustup 会自动遵循）
command -v cargo >/dev/null || {
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
}

# 3.3 构建并安装
./infra/install-guest.sh --confirm-install

# 3.4 出口网卡 —— 模板默认值是测试床的接口名，几乎不会匹配真实主机
DEV=$(ip -o route show default | awk '{print $5; exit}')
sed -i "s|^tc_interface = \".*\"|tc_interface = \"$DEV\"|" /etc/skyline-speeder/speeder.toml
grep '^tc_interface' /etc/skyline-speeder/speeder.toml
```

> **升级已有安装**：`install-guest.sh` 只替换文件，不会重启已在运行的 `skyline-speederd`。
> 第 4 节验证通过后执行：
>
> ```bash
> # 按第 5 节挂载的主机上 enable unit 是 active：restart 会连带重启它，它的 ExecStop
> # （boot-disable.sh）先把默认算法写回 fallback_cc 再 drain，无需手动 drain。
> # 只有它不是 active（skyline_cc 由裸 `ssctl enable` 挂载）时才要手动 drain：单独停止
> # daemon 不会写回 fallback_cc。经 SSH 执行的 drain 必然超时并返回非零（见第 8 节），属预期。
> systemctl is-active -q skyline-speeder-enable.service || ssctl drain --timeout 60 || true
> systemctl is-active -q skyline-speeder-enable.service \
>   || [ "$(sysctl -n net.ipv4.tcp_congestion_control)" != skyline_cc ]
> systemctl restart skyline-speederd.service
> ```
>
> enable unit 此前不是 active、且该主机应挂载 skyline_cc 的话，restart 后按第 5 节启动它。从 0.1.0 升级的完整注意事项见
> `CHANGELOG.md` 的 *Upgrading from 0.1.0*。

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

# 5.3 挂载 struct_ops 并切换默认算法（开机自启同样走这条路径）
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

# G2 struct_ops 已注册
bpftool struct_ops show | grep -q skyline_cc

# G3 算法已进入内核可用列表
sysctl -n net.ipv4.tcp_available_congestion_control | grep -qw skyline_cc

# G4 已成为系统默认
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = "skyline_cc" ]

# G5 控制面自述已启用
ssctl status | grep -q '"enabled": true'

# G6 开机自启
systemctl is-enabled skyline-speederd.service skyline-speeder-enable.service
```

**G7 —— 重启存活性**（强烈建议）：

```bash
systemctl reboot
# 重启后重新执行 G1–G6
```

**G8 —— 实流验证**：确认真实连接确实走了该算法，而不只是 sysctl 值正确。

```bash
ss -tin | grep -c skyline_cc                       # 应 > 0
ssctl status | grep -E 'active_flows|tc_stats'
```

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
# 优雅摘除：阻止新连接进入，等存量连接自然结束
ssctl drain --timeout 60

# 停止（等价于 SIGTERM，会正确注销全部 struct_ops 与 BPF 链接）
systemctl stop skyline-speeder-enable.service skyline-speederd.service

# 彻底卸载（保留 /etc/skyline-speeder 配置，并还原安装前的 cc/qdisc）
sudo ./install.sh --uninstall
```

`drain` 经 SSH 执行必然超时（执行者的 SSH 连接本身就是一条 skyline_cc 存量流）；
sysctl 在等待前已写回 `fallback_cc`，故超时不影响"新连接不再使用 skyline_cc"这一
结果。`--uninstall` 从 `/etc/skyline-speeder/pre-install-state` 还原安装前的
拥塞控制算法与默认 qdisc。

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
