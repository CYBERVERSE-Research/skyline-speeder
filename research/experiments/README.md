# Performance test reproduction guide

This directory holds the complete toolchain for Skyline Speeder's performance tests: the manifest-driven matrix framework
(`matrix_lib.py` expands, `run_matrix.py` executes, `analyze_results.py` analyzes),
plus the report renderer for the multi-kernel-version compatibility validation pipeline. `run_matrix.py --execute`
configures the two guests over SSH and drives iperf3/tc/ss, and also calls the host's
`infra/configure-path.sh` to adjust the qdisc of the test namespace.

Reproducing the results in `docs/04-performance-report.md` requires a two-VM KVM test bed
(a server VM + a client VM, with tap interfaces bridging the management network and the data network). Formal performance data
should only come from an environment that meets the "resource threshold" below.

## Toolchain

- `matrix_lib.py`: the core library for expanding and validating manifests (`load_manifest()` /
  `expand_runs()` / `validate_manifest()`), shared by the other scripts. A manifest is the
  Cartesian product profiles × scenarios × runs.
- `generate_matrix.py`: dry-run expansion of a manifest into a case list + a CSV skeleton; it pushes
  no configuration and is used to check the case count and naming before a formal run.
- `run_matrix.py`: dry-run by default (only expands and validates); with an explicit `--execute`
  it opens fresh connections case by case and collects the raw `iperf3`/`ss`/`tc` JSON/JSONL data.
  The output directory must be a new directory; the script refuses to overwrite existing results.
- `analyze_results.py`: reads the output of `run_matrix.py` and produces a per-run CSV,
  bootstrap 95% confidence intervals, the coefficient of variation (CV), and effect sizes relative to the reference profile;
  it also summarizes the applied/rejected counter deltas of `rack_rto` (M1 tier-2) and
  the median `rto:` of `ss -ti`.
- `render_kernel_compat_report.py`: reads the set of `state.json` files produced by the multi-kernel-version compatibility pipeline (see below)
  and renders `report.md`/`summary.json`. Pure functions; it does not
  build/deploy/SSH.
- `run_netns_lab.sh` / `plot_results.py`: a quick local single-machine network namespace
  smoke test (see "Local quick smoke test" below); it does not depend on the two-VM test bed and does not produce performance
  conclusions.

## Manifest list

Every manifest marks its nature with `execution_class = "formal-kvm"` (real performance data) or
`"tcg-validation"` (correctness/compatibility validation under TCG acceleration; the numbers do not represent KVM or bare-metal
performance), and `validate_manifest()` in `matrix_lib.py` enforces that
this constraint is consistent with the `performance_valid` field.

A scenario's `offload` decides what NetEm on the router receives: with `"off"` the Server's data NIC
turns off TSO/GSO/GRO, individual packets reach the router, and `loss_pct`/`reorder_pct` apply
per packet; with `"on"` (the default) the Server's large TSO frames reach the router as they are, and NetEm drops or
reorders a whole large frame each time. Write `"off"` for random per-packet loss; the cost is a throughput ceiling of about
100-150 Mbit/s (see section 2.3 of `docs/04-performance-report.md`).

| Manifest | Purpose |
|---|---|
| `smoke.toml` | Minimal correctness smoke test |
| `target-box.toml` | Target deployment environment grid (source of the core performance results) |
| `target-box-rtt300-samples.toml` | Extra samples at the high-RTT point, raising the statistical confidence at that point |
| `bandwidth-sweep.toml` | Bandwidth sensitivity |
| `neutrality.toml` | Neutrality check: all modules off (`b2-skyline-base`) against kernel CUBIC (`b1-controlled-cubic`), with all modules on in `skyline-best` as a reference; the gate looks only at `line-rate` (`parity_scenario` in `analyze_results.py`); `dc-lowrtt` is reported as well but is not a gate, and there are also two light-loss scenarios, `light-loss` (0.1%) and `primary` (0.5%) |
| `guardrail-gain-ab.toml` | Isolates the effect of the `guardrail_gain` coefficient |
| `rto-max-validation.toml` | Isolates the effect of the M1 tier-2 RTO cap mechanism |
| `rto-max-smoke.toml` | End-to-end smoke test of the RTO cap mechanism |
| `volatile-link.toml` | Link volatility scenarios (network conditions switch in segments mid-case) |
| `retransmit-dscp-mechanism.toml` / `-v6.toml` | Validation of the DSCP marking mechanism for retransmitted packets (IPv4/IPv6) |
| `retransmit-dscp-neutrality.toml` | Neutrality of the DSCP marking switch |
| `kernel-compat-smoke.toml` / `kernel-compat-smoke-tcg.toml` | Multi-kernel-version compatibility smoke test, in both KVM and TCG accelerator forms |

## Resource threshold and environment setup

Formal data requires usable nested KVM and a host with enough CPU/memory to run both VMs in parallel through the whole
matrix. A machine without enough resources is good only for code/verifier checks and short smoke runs, and must not be used to draw
performance conclusions.

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
# After both guests reboot and you confirm that uname -r meets the kernel version
# requirement in section 1.3 of DEPLOY.md, run this in the repository root on the host (not on a guest: the cloud-init image itself
# contains no repository checkout, and infra/install-guest.sh requires the machine it runs on to be
# the repository root locally, a precondition that does not hold on this test bed):
VMLINUX_BTF="$PWD/build/kernel/out-6.18.40/vmlinux" \
  infra/deploy-guest.sh --confirm-install
ssh -p 2222 skyline@127.0.0.1 sudo systemctl enable --now skyline-speederd.service
ssh -p 2222 skyline@127.0.0.1 \
  timeout 30 sh -c 'until sudo test -S /run/skyline-speeder/speeder.sock; do sleep 1; done'
```

`infra/kernel/build.sh` uses `make bindeb-pkg`, which also produces a `linux-image-<version>-dbg` debug-symbol package that is
not deployed to the guests (it can reach 1GiB+, and its compression stage
may take more than a third of the whole kernel build time with no new log output for a long while; this is normal,
not a hung build); `infra/kernel/deploy.sh` sends only the regular image/headers packages to the
guests, and this debug package is a local audit artifact.

`infra/deploy-guest.sh` builds on the host and modifies the server guest only over SSH; both ends
keep the complete staging directory and SHA-256 checksum records, and the repository does not need to be copied into the guest.
After `systemctl enable --now`, the control socket of `skyline-speederd.service` takes a few seconds to
appear (there is no systemd readiness notification), so the `timeout 30 sh -c ...` step above is
required; skipping it and calling `ssctl` directly may randomly hit
`connect to /run/skyline-speeder/speeder.sock: No such file or directory`.

`infra/run-vms.sh` also supports `--accel tcg` (it does not depend on nested KVM, is used only for integration/
compatibility validation, and does not produce performance conclusions). Stop the VMs with
`infra/run-vms.sh stop --accel <kvm|tcg> --confirm-stop`; tearing down the network topology is confirmed separately with
`sudo infra/topology.sh down --confirm-down`.

The iproute2 6.1 shipped with Ubuntu 24.04 does not support a fixed NetEm random seed, and a formal matrix with loss
actively refuses to run. You can build a repository-local `tc` of the same generation as the target kernel:

```bash
infra/build-iproute2.sh
export SKYLINE_TC="$PWD/build/tools/tc-iproute2-6.18.0"
```

## Running the matrix and analyzing the results

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

`run_matrix.py` supports `--keep-going` (one failed case does not abort the whole set) and
`--resume` (requires the manifest, the execution class and the image paths to be exactly the same; cases that already succeeded are
skipped, and failed or incomplete cases are written to a new `attemptNN` directory).

At the end of each case, before marking it valid, the runner checks the kernel log for the duration of that case
(panic/Oops/soft lockup and the like); a hit immediately terminates the whole campaign, even with
`--keep-going`, because once the test bed shows a problem of this kind, every later case shares
the same guest, which can no longer be trusted, and carrying on only wastes time producing unusable data. After termination,
reboot/restore the guest snapshot, then continue with `--resume`.

`analyze_results.py` applies a stricter second judgement to the cases already written to disk (the same kernel
health check, plus retransmission-capture false positives and the like), and may judge cases that the runner itself considered valid
to be invalid. `--resume` itself looks at the `status.json` the runner wrote and does not know about this
second judgement, so it cannot fill in these cases; use
`--retry-analysis-invalid=<analysis/runs.csv>` (together with `--resume`) to rerun these
cases too, without manually rewriting or deleting the original `status.json`:

```bash
.venv/bin/python research/experiments/run_matrix.py \
  research/experiments/manifests/target-box.toml \
  research/experiments/runs/target-box --execute --resume \
  --retry-analysis-invalid=research/experiments/analysis/target-box/runs.csv
```

## Multi-kernel-version compatibility validation pipeline

```bash
infra/kernel/run-compat-pipeline.sh all research/experiments/runs/kernel-compat \
  --versions 6.12.101,6.18.42,7.1.6
```

The four subcommands `plan|build|test|report` can each be run separately; `build` only fetches/compiles
the `.deb` packages of each kernel version and touches no VM; `test` forks a child overlay per version,
installs the kernel, runs the smoke checks and tears down; `report` reads the existing `state.json` files to render the report
and can safely be rerun after partial/failed runs. For a full description of the parameters, see the script's own header comment.

## Local quick smoke test

This does not need the full two-VM test bed; it is used to verify that the toolchain itself runs end to end, and does not produce performance conclusions:

```bash
research/experiments/run_netns_lab.sh \
  --cc cubic --rtt-ms 150 --loss-pct 0.5 \
  --rate-mbit 100 --duration 30 --runs 5 \
  --output /tmp/skyline-local-smoke.csv

MPLCONFIGDIR=/tmp/matplotlib-serverspeeder \
  research/experiments/plot_results.py \
  /tmp/skyline-local-smoke.csv /tmp/skyline-local-smoke-plots
```

The script rejects unknown congestion control algorithm names. When the result file does not exist it writes the header, and when it exists it appends;
each new round of local tests should point at a new output file, to avoid mixing data from different kernel or tool versions.

The local kernel must allow unprivileged user namespaces, and `ip`, `tc`, `iperf3`, `jq`,
Python 3 and Matplotlib must be installed.

## Safety constraints

- Do not load kernel modules or acceleration binaries of unknown origin on the host.
- Black-box comparison experiments must be done in offline VMs whose snapshots can be destroyed, after first recording the sample's origin,
  license and SHA-256.
- Do not copy unauthorized binaries, licenses, or scripts that bypass authorization into this repository.
- Formal public-internet experiments must set a bandwidth cap and stop conditions, to keep aggressive retransmission from affecting third-party traffic.
