# 性能测试复现指南

本目录承载 Skyline Speeder 性能测试的完整工具链：manifest 驱动的 matrix 框架
（`matrix_lib.py` 展开、`run_matrix.py` 执行、`analyze_results.py` 分析），
以及多内核版本适配性验证流水线的报告渲染工具。`run_matrix.py --execute`
会通过 SSH 配置两台 guest 并驱动 iperf3/tc/ss，同时调用宿主机
`infra/configure-path.sh` 调整测试 namespace 的 qdisc。

复现 `docs/04-performance-report.md` 里的结果需要一套双 VM KVM 测试床
（服务器 VM + 客户端 VM，通过 tap 网卡桥接管理网与数据网）。正式性能数据
只应来自满足下文"资源门槛"的环境。

## 工具链

- `matrix_lib.py`：manifest 展开与校验的核心库（`load_manifest()` /
  `expand_runs()` / `validate_manifest()`），被其余脚本共享。manifest 是
  profiles × scenarios × runs 的笛卡尔积。
- `generate_matrix.py`：dry-run 展开 manifest 为 case 列表 + CSV 骨架，不
  下发任何配置，用于在正式跑之前核对 case 数量与命名。
- `run_matrix.py`：默认 dry-run（只展开并校验）；显式传入 `--execute` 后
  逐个 case 创建新连接并采集 `iperf3`/`ss`/`tc` 的 JSON/JSONL 原始数据。
  输出目录必须是新目录，脚本拒绝覆盖已有结果。
- `analyze_results.py`：读取 `run_matrix.py` 的输出，产出逐轮 CSV、
  bootstrap 95% 置信区间、变异系数（CV），以及相对参照 profile 的效应量；
  同时汇总 `rack_rto`（M1 tier-2）的 applied/rejected 计数增量与
  `ss -ti` 的 `rto:` 中位数。
- `render_kernel_compat_report.py`：读取多内核版本适配性流水线（见下文）
  产出的 `state.json` 集合，渲染 `report.md`/`summary.json`。纯函数，不
  build/deploy/SSH。
- `run_netns_lab.sh` / `plot_results.py`：单机 network namespace 本地
  快速冒烟（见下文"本地快速冒烟"），不依赖双 VM 测试床，也不产出性能
  结论。

## Manifest 清单

所有 manifest 都用 `execution_class = "formal-kvm"`（真实性能数据）或
`"tcg-validation"`（TCG 加速下的正确性/适配性验证，数字不代表 KVM 或裸机
性能）标记自己的性质，`matrix_lib.py` 的 `validate_manifest()` 会强制校验
这条约束与 `performance_valid` 字段一致。

场景的 `offload` 决定路由器上的 NetEm 拿到什么：`"off"` 时 Server 数据网卡
关掉 TSO/GSO/GRO，到达路由器的是一个个的包，`loss_pct`/`reorder_pct` 逐包
生效；`"on"`（默认）时 Server 的 TSO 大帧原样到达路由器，NetEm 每次丢掉或
重排一整个大帧。要逐包随机丢包就写 `"off"`，代价是吞吐上限约
100-150 Mbit/s（见 `docs/04-performance-report.md` 第 2.3 节）。

| Manifest | 用途 |
|---|---|
| `smoke.toml` | 最小正确性冒烟 |
| `target-box.toml` | 目标部署环境网格（核心性能结果来源） |
| `target-box-rtt300-samples.toml` | 高 RTT 点加样本，提升该点的统计置信度 |
| `bandwidth-sweep.toml` | 带宽敏感性 |
| `neutrality.toml` | 中性性检查：模块全关（`b2-skyline-base`）对内核 CUBIC（`b1-controlled-cubic`），另有模块全开的 `skyline-best` 作参照；门槛只看 `line-rate`（`analyze_results.py` 的 `parity_scenario`）；`dc-lowrtt` 一并报告但不作门槛，另有 `light-loss`（0.1%）与 `primary`（0.5%）两个轻丢包场景 |
| `guardrail-gain-ab.toml` | 隔离验证 `guardrail_gain` 系数的作用 |
| `rto-max-validation.toml` | 隔离验证 M1 tier-2 RTO 上限机制的作用 |
| `rto-max-smoke.toml` | RTO 上限机制端到端冒烟 |
| `volatile-link.toml` | 链路波动场景（mid-case 网络条件分段切换） |
| `retransmit-dscp-mechanism.toml` / `-v6.toml` | 重传包 DSCP 标记机制验证（IPv4/IPv6） |
| `retransmit-dscp-neutrality.toml` | DSCP 标记开关的中性性 |
| `kernel-compat-smoke.toml` / `kernel-compat-smoke-tcg.toml` | 多内核版本适配性冒烟，KVM/TCG 两种加速器形态 |

## 资源门槛与环境搭建

正式数据要求嵌套 KVM 可用、宿主机有足够 CPU/内存支撑双 VM 并行跑满整个
矩阵。资源不足的机器只能用于代码/verifier 检查和短时冒烟，不得据此得出
性能结论。

```bash
infra/preflight.sh formal
infra/fetch-ubuntu-image.sh
infra/prepare-images.sh
infra/prepare-cloud-init.sh ~/.ssh/id_ed25519.pub
sudo infra/topology.sh up
infra/run-vms.sh start --accel kvm
infra/wait-for-guests.sh

infra/kernel/fetch-source.sh
infra/kernel/build.sh
infra/kernel/deploy.sh build/kernel --confirm-install-kernel
# 两台 guest 重启并确认 uname -r 满足 DEPLOY.md 第 1.3 节
# 的内核版本要求后，在宿主仓库根目录（不是 guest 上——cloud-init 镜像本身
# 不包含仓库 checkout，infra/install-guest.sh 要求在运行它的机器本地就是
# 仓库根目录，这个前提在这套测试床上不成立）：
VMLINUX_BTF="$PWD/build/kernel/out-6.18.40/vmlinux" \
  infra/deploy-guest.sh --confirm-install
ssh -p 2222 skyline@127.0.0.1 sudo systemctl enable --now skyline-speederd.service
ssh -p 2222 skyline@127.0.0.1 \
  timeout 30 sh -c 'until sudo test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'
```

`infra/kernel/build.sh` 会用 `make bindeb-pkg` 连带生成一个不会被部署到
guest 的 `linux-image-<version>-dbg` 调试符号包（体积可达 1GiB+，压缩阶段
可能占整个内核构建耗时的三分之一以上且长时间没有新日志输出，属正常现象，
不是构建卡死）；`infra/kernel/deploy.sh` 只会把常规 image/headers 包发给
guest，这部分调试包属于本机审计产物。

`infra/deploy-guest.sh` 在宿主上构建、只通过 SSH 修改 server guest，两端都
保留完整的 staging 目录和 SHA-256 校验记录，不需要把仓库复制进 guest。
`systemctl enable --now` 之后 `skyline-speederd.service` 的控制 socket 需要几秒钟才会
出现（没有 systemd readiness 通知），上面的 `timeout 30 sh -c ...` 步骤是
必需的，跳过它直接调用 `ssctl` 可能会随机遇到
`connect to /run/skyline-speeder/speeder.sock: No such file or directory`。

`infra/run-vms.sh` 也支持 `--accel tcg`（不依赖嵌套 KVM，仅用于集成/
适配性验证，不产出性能结论）。VM 停止用
`infra/run-vms.sh stop --accel <kvm|tcg> --confirm-stop`；网络拓扑清理由
`sudo infra/topology.sh down --confirm-down` 单独确认。

Ubuntu 24.04 自带的 iproute2 6.1 不支持 NetEm 固定随机种子，带丢包的正式
矩阵会主动拒绝运行。可构建与目标内核同代的仓库本地 `tc`：

```bash
infra/build-iproute2.sh
export SKYLINE_TC="$PWD/build/tools/tc-iproute2-6.18.0"
```

## 运行矩阵与分析结果

```bash
.venv/bin/python research/experiments/generate_matrix.py \
  research/experiments/manifests/target-box.toml /tmp/skyline-target-box

.venv/bin/python research/experiments/run_matrix.py \
  research/experiments/manifests/target-box.toml \
  research/experiments/runs/target-box --execute

.venv/bin/python research/experiments/analyze_results.py \
  research/experiments/runs/target-box \
  research/experiments/analysis/target-box
```

`run_matrix.py` 支持 `--keep-going`（单个 case 失败不中止整组）和
`--resume`（要求 manifest、执行类别与镜像路径完全一致；已成功的 case 会
被跳过，失败或不完整的 case 写入新的 `attemptNN` 目录）。

每个 case 结束时会在标记为有效之前检查该 case 期间的内核日志
（panic/Oops/soft lockup 等），一旦命中会立即终止整个 campaign——即使加了
`--keep-going` 也不例外，因为测试床一旦出现这类问题，之后所有 case 共享
同一个已经不可信的 guest，继续跑只会浪费时间产出不能用的数据。终止后需要
重启/恢复 guest 快照，再用 `--resume` 续跑。

`analyze_results.py` 会对已经落盘的 case 做更严格的二次判定（同样的内核
健康检查，加上重传抓包假阳性等），可能把 runner 自己认为有效的 case 又
判成无效。`--resume` 本身看的是 runner 自己写的 `status.json`，不知道这
层二次判定，因此补不全这些 case——用
`--retry-analysis-invalid=<analysis/runs.csv>`（配合 `--resume`）让这些
case 也重新跑一次，不需要手工改写或删除原始 `status.json`：

```bash
.venv/bin/python research/experiments/run_matrix.py \
  research/experiments/manifests/target-box.toml \
  research/experiments/runs/target-box --execute --resume \
  --retry-analysis-invalid=research/experiments/analysis/target-box/runs.csv
```

## 多内核版本适配性验证流水线

```bash
infra/kernel/run-compat-pipeline.sh all research/experiments/runs/kernel-compat \
  --versions 6.12.101,6.18.42,7.1.6
```

`plan|build|test|report` 四个子命令可以分开单独运行；`build` 只拉取/编译
各版本内核的 `.deb` 包，不接触任何 VM；`test` 逐版本 fork 子 overlay、
安装内核、跑冒烟检查、拆除；`report` 读取已有的 `state.json` 渲染报告，
可在部分/失败的运行之后安全重跑。完整参数说明见脚本自身的头部注释。

## 本地快速冒烟

不需要起完整双 VM 测试床，用于验证工具链本身能跑通，不产出性能结论：

```bash
research/experiments/run_netns_lab.sh \
  --cc cubic --rtt-ms 150 --loss-pct 0.5 \
  --rate-mbit 100 --duration 30 --runs 5 \
  --output /tmp/skyline-local-smoke.csv

MPLCONFIGDIR=/tmp/matplotlib-serverspeeder \
  research/experiments/plot_results.py \
  /tmp/skyline-local-smoke.csv /tmp/skyline-local-smoke-plots
```

脚本拒绝未知的拥塞控制算法名称。结果文件不存在时写入表头，存在时追加；
每轮新的本地测试应指向新的输出文件，避免混合不同内核或工具版本的数据。

要求本机内核允许无特权 user namespace，并安装 `ip`、`tc`、`iperf3`、`jq`、
Python 3 与 Matplotlib。

## 安全约束

- 不在宿主机加载来源不明的内核模块或加速二进制。
- 黑盒对比实验必须在离线、可销毁快照的 VM 中进行，并先记录样本来源、
  许可证和 SHA-256。
- 不把未授权的二进制、许可证或绕过授权的脚本复制进本仓库。
- 正式公网实验必须设置带宽上限和停止条件，避免激进重传影响第三方流量。
