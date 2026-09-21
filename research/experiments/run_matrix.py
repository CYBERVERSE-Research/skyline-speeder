#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from analyze_results import KERNEL_HEALTH_PATTERN
from matrix_lib import RunCase, expand_runs, load_manifest


class KernelHealthFatal(RuntimeError):
    """A case's kernel log shows a health-compromising event (panic, Oops,
    soft lockup, ...). Raised out of run_case() past its normal bool
    return -- unlike an ordinary per-case failure, this must stop the
    whole campaign even under --keep-going, since every case that runs
    afterward shares the same guest and its results are not trustworthy
    once the guest itself is in this state.
    """


FAILURE_CLASSES = {
    "environment": "INFRA",
    "path": "NETWORK",
    "profile": "CONTROL",
    "offload": "CONTROL",
    "iperf": "IPERF",
    "metrics": "METRICS",
    "snapshot": "METRICS",
    "cleanup": "SAFETY",
}

# Remote scratch paths for the retransmit-DSCP mechanism verification
# capture (case.scenario.capture_retransmits) -- fixed names are fine, only
# one case runs at a time per guest, and they're overwritten every case.
SKYLINE_RETRANSMIT_CAPTURE_SERVER_PATH = "/tmp/skyline-retransmit-capture-server.txt"
SKYLINE_RETRANSMIT_CAPTURE_CLIENT_PATH = "/tmp/skyline-retransmit-capture-client.txt"


@dataclass(frozen=True)
class Remote:
    destination: str
    port: int

    @classmethod
    def parse(cls, value: str):
        host, separator, port = value.rpartition(":")
        if not separator or not port.isdigit():
            return cls(value, 22)
        return cls(host, int(port))

    def command(self, arguments):
        remote_command = shlex.join(str(argument) for argument in arguments)
        known_hosts = os.environ.get(
            "SKYLINE_SSH_KNOWN_HOSTS", "build/run/known_hosts"
        )
        control_dir = Path(
            os.environ.get("SKYLINE_SSH_CONTROL_DIR", "build/run/ssh-control")
        )
        control_dir.mkdir(parents=True, exist_ok=True)
        return [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            f"UserKnownHostsFile={known_hosts}",
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPersist=60",
            "-o",
            f"ControlPath={control_dir}/%C",
            "-p",
            str(self.port),
            self.destination,
            remote_command,
        ]

    def run(self, arguments, *, check=True, timeout=None):
        return subprocess.run(
            self.command(arguments),
            check=check,
            text=True,
            capture_output=True,
            timeout=timeout,
        )

    def popen(self, arguments):
        return subprocess.Popen(
            self.command(arguments),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )


def run_local(arguments, *, check=True, timeout=None):
    return subprocess.run(
        arguments, check=check, text=True, capture_output=True, timeout=timeout
    )


def wait_for_iperf_server(server, timeout_s=30):
    """Wait for the guest listener without consuming iperf3's one-shot accept."""
    tenths = max(1, int(timeout_s * 10))
    script = (
        f"for _ in $(seq 1 {tenths}); do "
        "ss -ltnH 'sport = :5201' | grep -q . && exit 0; "
        "sleep 0.1; done; exit 1"
    )
    return server.run(["bash", "-lc", script], check=False)


def host_tc():
    requested = os.environ.get("SKYLINE_TC", "tc")
    resolved = shutil.which(requested)
    if resolved is None:
        raise RuntimeError(f"tc command is unavailable: {requested}")
    return str(Path(resolved).resolve())


def write_text(path, content):
    path.write_text(content, encoding="utf-8")


def sha256_bytes(payload):
    return hashlib.sha256(payload).hexdigest()


def load_vm_environment():
    path = Path(os.environ.get("SKYLINE_VM_ENVIRONMENT", "build/run/environment.json"))
    if not path.is_file():
        raise RuntimeError(
            f"VM environment is missing: {path}; start VMs with infra/run-vms.sh"
        )
    try:
        return path, json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise RuntimeError(f"invalid VM environment JSON: {error}") from error


def execution_identity(environment):
    return {
        "execution_class": environment.get("execution_class"),
        "performance_valid": environment.get("performance_valid"),
        "acceleration": environment.get("acceleration"),
        "qemu": environment.get("qemu"),
        "host_kernel": environment.get("host_kernel"),
        "resources": environment.get("resources"),
        # qcow2 overlays are mutable while guests run, so their whole-file
        # hashes cannot be resume identities. The initial hashes remain in the
        # environment snapshot; resume binds to the same absolute image paths.
        "server_image": environment.get("server", {}).get("image"),
        "client_image": environment.get("client", {}).get("image"),
    }


def guest_execution_identity(server, client):
    identity = {}
    for label, remote in (("server", server), ("client", client)):
        release = remote.run(["uname", "-r"], check=False)
        config = remote.run(
            ["bash", "-lc", "sha256sum /boot/config-$(uname -r)"],
            check=False,
        )
        if release.returncode != 0 or config.returncode != 0:
            raise RuntimeError(f"cannot identify {label} guest kernel")
        identity[label] = {
            "kernel_release": release.stdout.strip(),
            "kernel_config_sha256": config.stdout.split()[0],
        }
    return identity


def validate_execution_class(manifest, environment):
    actual_class = environment.get("execution_class")
    if actual_class != manifest.execution_class:
        raise RuntimeError(
            "execution class mismatch: "
            f"manifest={manifest.execution_class}, environment={actual_class}"
        )
    actual_performance = environment.get("performance_valid")
    if actual_performance is not manifest.performance_valid:
        raise RuntimeError(
            "performance validity mismatch: "
            f"manifest={manifest.performance_valid}, environment={actual_performance}"
        )


def snapshot_environment(server, client, data_interface, vm_environment):
    local_commands = {
        "uname": ["uname", "-a"],
        "qemu": ["qemu-system-x86_64", "--version"],
        "tc": [host_tc(), "-Version"],
        "bpftool": ["bpftool", "version"],
        "runner_sha256": [
            "sha256sum",
            "research/experiments/run_matrix.py",
            "research/experiments/matrix_lib.py",
            "infra/configure-path.sh",
        ],
    }
    remote_commands = {
        "uname": ["uname", "-a"],
        "iperf3": ["iperf3", "--version"],
        "sysctl_cc": ["sysctl", "net.ipv4.tcp_congestion_control"],
        "available_cc": ["sysctl", "net.ipv4.tcp_available_congestion_control"],
        "tcp_recovery": ["sysctl", "net.ipv4.tcp_recovery"],
        "tcp_reordering": ["sysctl", "net.ipv4.tcp_reordering"],
        "tcp_early_retrans": ["sysctl", "net.ipv4.tcp_early_retrans"],
        # RTAX_RTO_MIN route locks silently override the BPF-set RTO floor
        # (see include/net/tcp.h:tcp_rto_min()); recorded here so a null
        # M1 tier-2 result can be diagnosed instead of misread as "no effect".
        "route_metrics": ["ip", "-d", "route", "show", "dev", data_interface],
        "qdisc": ["tc", "-s", "-j", "qdisc", "show", "dev", data_interface],
        "link": ["ip", "-d", "-j", "link", "show", "dev", data_interface],
        "offload": ["ethtool", "-k", data_interface],
        "driver": ["ethtool", "-i", data_interface],
        "bpftool": ["bpftool", "version"],
        "libbpf": ["pkg-config", "--modversion", "libbpf"],
        "bpf_sha256": ["bash", "-lc", "sha256sum /opt/skyline-speeder/bpf/*.bpf.o"],
        "kernel_config_sha256": [
            "bash",
            "-lc",
            "sha256sum /boot/config-$(uname -r)",
        ],
    }
    snapshot = {
        "execution_class": vm_environment.get("execution_class"),
        "performance_valid": vm_environment.get("performance_valid"),
        "vm_environment": vm_environment,
        "local": {},
        "server": {},
        "client": {},
    }
    for name, command in local_commands.items():
        result = run_local(command, check=False)
        snapshot["local"][name] = {
            "returncode": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
        }
    for label, remote in (("server", server), ("client", client)):
        for name, command in remote_commands.items():
            result = remote.run(command, check=False)
            snapshot[label][name] = {
                "returncode": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
    return snapshot


def configure_path(
    *,
    rtt_ms,
    rate_mbit,
    loss_model,
    loss_pct,
    loss_direction,
    burst_length,
    seed,
    router_sudo,
    queue_bdp=None,
    queue_packets=None,
    reorder_pct=0.0,
    reorder_correlation=0.0,
    live=False,
    timeout=None,
):
    """Configure (or, with live=True, hot-update) the router's egress path.

    Takes explicit scalar params rather than a RunCase so the same builder
    serves both the initial per-case setup (queue_bdp, from
    case.scenario.*) and a mid-case live segment transition (queue_packets,
    an absolute override -- see Segment's doc comment in matrix_lib.py for
    why a rate change alone does nothing to queueing delay unless the
    packet count in the buffer is held constant across it).
    """
    command = [
        "env",
        f"SKYLINE_TC={host_tc()}",
        "infra/configure-path.sh",
        "--rtt-ms",
        str(rtt_ms),
        "--rate-mbit",
        str(rate_mbit),
        "--loss-model",
        loss_model,
        "--loss-pct",
        str(loss_pct),
        "--loss-direction",
        loss_direction,
        "--burst-length",
        str(burst_length),
        "--seed",
        str(seed),
        "--reorder-pct",
        str(reorder_pct),
        "--reorder-correlation",
        str(reorder_correlation),
    ]
    if queue_packets is not None:
        command += ["--queue-packets", str(queue_packets)]
    elif queue_bdp is not None:
        command += ["--queue-bdp", str(queue_bdp)]
    if live:
        command.append("--live")
    if router_sudo:
        command.insert(0, "sudo")
    return run_local(command, check=False, timeout=timeout)


def clear_path(router_sudo):
    command = ["env", f"SKYLINE_TC={host_tc()}", "infra/configure-path.sh", "--clear"]
    if router_sudo:
        command.insert(0, "sudo")
    return run_local(command, check=False)


def _run_segment_schedule(case, router_sudo, stop_event, results):
    """Background scheduler for a segmented scenario's mid-case network
    transitions (see Segment's doc comment in matrix_lib.py). Runs in its
    own thread, started right before the blocking client iperf3 call and
    joined right after it returns (run_case()) -- offset_s is relative to
    that same t0. Appends one result dict per segment to `results` (a plain
    list; simple .append() from a single writer thread is safe under the
    GIL) so the caller can tell a completed schedule from one that was cut
    short by stop_event or an exception.
    """
    t0 = time.monotonic()
    for index, segment in enumerate(case.scenario.segments, start=1):
        delay = segment.offset_s - (time.monotonic() - t0)
        if delay > 0 and stop_event.wait(delay):
            return  # stopped while waiting for this segment's turn
        if stop_event.is_set():
            return
        record = {
            "index": index,
            "label": segment.label,
            "requested_offset_s": segment.offset_s,
            "applied_at_monotonic_s": time.monotonic() - t0,
            "applied_at_wall_clock": datetime.now(timezone.utc).isoformat(),
        }
        try:
            result = configure_path(
                rtt_ms=segment.rtt_ms,
                rate_mbit=segment.rate_mbit,
                loss_model=segment.loss_model,
                loss_pct=segment.loss_pct,
                loss_direction=case.scenario.loss_direction,
                burst_length=segment.burst_length,
                # netem's PRNG (and, for gemodel, its GOOD/BAD state) is
                # unconditionally reseeded/reset by the kernel on every
                # `tc qdisc change ... netem` call -- without a distinct
                # per-segment seed every transition would replay the exact
                # same loss sequence from the top.
                seed=case.seed + 1_000_000 * index,
                queue_packets=segment.queue_packets,
                reorder_pct=case.scenario.reorder_pct,
                reorder_correlation=case.scenario.reorder_correlation,
                router_sudo=router_sudo,
                live=True,
                timeout=30,
            )
            record["returncode"] = result.returncode
            record["stdout"] = result.stdout
            record["stderr"] = result.stderr
        except Exception as error:
            record["returncode"] = None
            record["error"] = str(error)
            results.append(record)
            return
        results.append(record)


def apply_profile(case, manifest, server):
    modules = ",".join(case.profile.modules)
    return server.run(
        [
            "sudo",
            "/opt/skyline-speeder/infra/apply-guest-profile.sh",
            case.profile.kind,
            manifest.server_data_interface,
            modules,
            manifest.stock_qdisc,
            str(case.profile.tcp_recovery),
            str(case.profile.tcp_reordering),
            str(case.profile.tcp_early_retrans),
        ],
        check=False,
    )


def rack_rto_command(spec):
    """Pure command construction, kept separate from apply_rack_rto so it is
    unit-testable without a live guest. M1 tier-2 (per-flow BPF RTO tuning)
    travels through `ssctl`, not apply-guest-profile.sh's positional sysctl
    arguments -- it is not a sysctl and does not belong to the shell script's
    responsibility.
    """
    if spec is None or not spec.enabled:
        return ["sudo", "ssctl", "reset-rack-rto"]
    return [
        "sudo",
        "ssctl",
        "set-rack-rto",
        "--srtt-permille",
        str(spec.srtt_permille),
        "--floor-us",
        str(spec.floor_us),
        "--ceiling-us",
        str(spec.ceiling_us),
        "--warmup-samples",
        str(spec.warmup_samples),
        "--rto-max-normal-permille",
        str(spec.rto_max_normal_permille),
        "--rto-max-congested-permille",
        str(spec.rto_max_congested_permille),
        "--rto-max-congestion-ratio-permille",
        str(spec.rto_max_congestion_ratio_permille),
    ]


def apply_rack_rto(case, server):
    return server.run(rack_rto_command(case.profile.rack_rto), check=False)


def module_config_command(spec):
    """Pure command construction, mirrors rack_rto_command(). M2/M3/M4
    coefficients travel through `ssctl` (skyline-speederd's own in-memory config,
    pushed to the BPF double-buffered config map), not apply-guest-profile.sh
    -- unlike the tier-1 sysctls, there is no external kernel state to write.
    `spec is None` (profile omits [profiles.module_config]) always issues a
    reset, so no case can silently inherit a previous case's overrides.
    """
    if spec is None:
        return ["sudo", "ssctl", "reset-module-config"]
    return [
        "sudo",
        "ssctl",
        "set-module-config",
        "--max-pacing-mbps",
        str(spec.max_pacing_mbps),
        "--max-cwnd-packets",
        str(spec.max_cwnd_packets),
        "--max-queue-delay-ms",
        str(spec.max_queue_delay_ms),
        "--max-queue-delay-ratio",
        str(spec.max_queue_delay_ratio),
        "--initial-cwnd-packets",
        str(spec.initial_cwnd_packets),
        "--min-rtt-window-s",
        str(spec.min_rtt_window_s),
        "--bw-window-rtts",
        str(spec.bw_window_rtts),
        "--startup-plateau-rtts",
        str(spec.startup_plateau_rtts),
        "--startup-growth-ratio",
        str(spec.startup_growth_ratio),
        "--startup-gain",
        str(spec.startup_gain),
        "--cruise-inflight-gain",
        str(spec.cruise_inflight_gain),
        "--cruise-pacing-gain",
        str(spec.cruise_pacing_gain),
        "--guardrail-gain",
        str(spec.guardrail_gain),
        "--loss-inflation-max-ratio",
        str(spec.loss_inflation_max_ratio),
    ] + (["--disable-prr-pacing"] if not spec.prr_pacing_enabled else []) + (
        ["--disable-auto-pacing"] if not spec.auto_pacing_enabled else []
    )


def apply_module_config(case, server):
    return server.run(module_config_command(case.profile.module_config), check=False)


def retransmit_dscp_command(spec):
    """Pure command construction, mirrors rack_rto_command()/
    module_config_command(). Unlike those two, this is NOT an skyline_cc-only
    knob -- bpf/skyline_tc.bpf.c is interface-wide, so it marks retransmits for
    whichever profile is running (stock CUBIC/BBR/skyline_cc alike). `spec is
    None` always issues a reset, same "no case can silently inherit a
    previous case's setting" rationale as the other two.
    """
    if spec is None or not spec.enabled:
        return ["sudo", "ssctl", "reset-retransmit-dscp"]
    return [
        "sudo",
        "ssctl",
        "set-retransmit-dscp",
        "--dscp-value",
        str(spec.dscp_value),
    ]


def apply_retransmit_dscp(case, server):
    return server.run(retransmit_dscp_command(case.profile.retransmit_dscp), check=False)


def restore_profile(manifest, server):
    return server.run(
        [
            "sudo",
            "/opt/skyline-speeder/infra/apply-guest-profile.sh",
            "controlled-cubic",
            manifest.server_data_interface,
            "",
            manifest.stock_qdisc,
            "1",
            "3",
            "3",
        ],
        check=False,
    )


def apply_offload(case, manifest, remote):
    state = case.scenario.offload
    return set_offload(manifest, remote, state)


def set_offload(manifest, remote, state):
    return remote.run(
        [
            "sudo",
            "ethtool",
            "-K",
            manifest.server_data_interface,
            "tso",
            state,
            "gso",
            state,
            "gro",
            state,
        ],
        check=False,
    )


def raise_client_socket_buffers(client):
    """Aggressive-optimization round, section 4.1 prerequisite -- see
    infra/apply-guest-profile.sh's matching comment for the full rationale
    (4MiB stock tcp_wmem/tcp_rmem ceilings cap throughput at rtt>=250ms
    regardless of congestion control). That script raises the server's
    tcp_wmem/tcp_rmem every case, but it is only ever installed on the
    server (deploy-guest.sh: "The client guest is not modified"). The iperf3
    runs use -R (reverse: server sends, client receives -- see run_case()),
    so the client's tcp_rmem/net.core.rmem_max need the same ceiling raised
    or the receive window caps throughput independent of the server-side fix.
    """
    return client.run(
        [
            "sudo",
            "sysctl",
            "-qw",
            "net.ipv4.tcp_rmem=4096 87380 67108864",
            "net.core.rmem_max=67108864",
        ],
        check=False,
    )


def qdisc_snapshot(router_sudo):
    command = [
        "ip",
        "netns",
        "exec",
        "skyline-router",
        host_tc(),
        "-s",
        "-j",
        "qdisc",
        "show",
    ]
    if router_sudo:
        command.insert(0, "sudo")
    return run_local(command, check=False)


def communicate(process, timeout):
    stdout, stderr = process.communicate(timeout=timeout)
    return process.returncode, stdout, stderr


def terminate_process(process):
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def communicate_or_terminate(process, timeout):
    # Bounded drain for use on a failure path where the process starting
    # normally (e.g. the iperf3 server, launched before the client) may
    # never exit on its own if whatever failed upstream means nothing
    # ever signals it to stop. A bare `communicate(timeout=...)` still
    # leaves the process running on TimeoutExpired; terminate it first so
    # the retry actually has something to read.
    try:
        return communicate(process, timeout)
    except subprocess.TimeoutExpired:
        terminate_process(process)
        try:
            return communicate(process, 5)
        except subprocess.TimeoutExpired:
            return process.returncode, "", ""


def _collect_failure_diagnostics(
    case_dir, server, server_process, metrics_process, perf_process,
    event_cursor, router_sudo,
):
    """Best-effort diagnostic capture for a case whose try block raised
    before reaching its own normal diagnostic-collection code (e.g. the
    iperf3 client exiting non-zero raises immediately, well before the
    server/metrics/perf output and kernel log ever get written) -- these
    are exactly the files needed to root-cause a failure, so losing them
    on the failure path defeats the point of collecting them at all.

    Only writes a file that doesn't already exist, so this is safe to call
    unconditionally on any non-valid case regardless of how far the normal
    flow got before raising. Every step is independently wrapped: one
    step failing (e.g. the SSH connection itself being down) must not
    prevent the others from being attempted. Returns a kernel-health
    fatal-note string if the collected kernel log shows a
    health-compromising event (see analyze_results.KERNEL_HEALTH_PATTERN),
    else None.
    """

    def write_pair_if_absent(stdout_name, stderr_name, process, timeout):
        stdout_path = case_dir / stdout_name
        stderr_path = case_dir / stderr_name
        if stdout_path.exists() and stderr_path.exists():
            return
        try:
            _, stdout, stderr = communicate_or_terminate(process, timeout)
        except Exception as error:
            stdout, stderr = f"diagnostic collection failed: {error}", ""
        if not stdout_path.exists():
            write_text(stdout_path, stdout)
        if not stderr_path.exists():
            write_text(stderr_path, stderr)

    def write_if_absent(name, producer):
        path = case_dir / name
        if path.exists():
            return
        try:
            write_text(path, producer())
        except Exception as error:
            write_text(path, f"diagnostic collection failed: {error}")

    if server_process is not None:
        write_pair_if_absent(
            "iperf-server.json", "iperf-server.stderr", server_process, 15
        )
    if metrics_process is not None:
        write_pair_if_absent(
            "ss-metrics.jsonl", "ss-metrics.stderr", metrics_process, 15
        )
    if perf_process is not None:
        write_pair_if_absent("perf.stdout", "perf.csv", perf_process, 15)
    write_if_absent(
        "nstat-after.txt",
        lambda: server.run(["nstat", "-az"], check=False, timeout=15).stdout,
    )
    write_if_absent(
        "skyline-events.jsonl",
        lambda: server.run(
            [
                "sudo",
                "/opt/skyline-speeder/infra/snapshot-skyline-events.sh",
                "since",
                event_cursor,
            ],
            check=False,
            timeout=15,
        ).stdout,
    )
    write_if_absent(
        "skyline-status-after.json",
        lambda: server.run(
            ["sudo", "ssctl", "status"], check=False, timeout=15
        ).stdout,
    )
    kernel_log_path = case_dir / "kernel-log.txt"
    kernel_log_text = None
    if kernel_log_path.exists():
        kernel_log_text = kernel_log_path.read_text(
            encoding="utf-8", errors="replace"
        )
    else:
        try:
            kernel_log = server.run(
                ["sudo", "journalctl", "-k", "-n", "300", "--no-pager"],
                check=False,
                timeout=15,
            )
            kernel_log_text = kernel_log.stdout + kernel_log.stderr
            write_text(kernel_log_path, kernel_log_text)
        except Exception as error:
            write_text(kernel_log_path, f"diagnostic collection failed: {error}")
    write_if_absent(
        "qdisc-after.json", lambda: qdisc_snapshot(router_sudo).stdout
    )
    issue = KERNEL_HEALTH_PATTERN.search(kernel_log_text) if kernel_log_text else None
    return f"kernel health marker detected: {issue.group(0)}" if issue else None


def failure_class(phase):
    return FAILURE_CLASSES.get(phase, "INFRA")


def cleanup_case(manifest, server, client, router_sudo):
    records = []
    profile = restore_profile(manifest, server)
    records.append(("server profile", profile))
    records.append(
        (
            "server rack rto",
            server.run(["sudo", "ssctl", "reset-rack-rto"], check=False),
        )
    )
    records.append(
        (
            "server module config",
            server.run(["sudo", "ssctl", "reset-module-config"], check=False),
        )
    )
    records.append(
        (
            "server retransmit dscp",
            server.run(["sudo", "ssctl", "reset-retransmit-dscp"], check=False),
        )
    )
    records.append(("server offload", set_offload(manifest, server, "on")))
    records.append(("client offload", set_offload(manifest, client, "on")))
    records.append(("host path", clear_path(router_sudo)))
    text = []
    ok = True
    for label, result in records:
        text.append(
            f"[{label}] returncode={result.returncode}\n"
            f"{result.stdout}{result.stderr}"
        )
        ok = ok and result.returncode == 0
    return ok, "\n".join(text)


def run_case(
    case: RunCase,
    manifest,
    case_dir: Path,
    attempt: int,
    server: Remote,
    client: Remote,
    router_sudo: bool,
):
    case_dir.mkdir()
    metadata = case.as_dict()
    metadata.update(
        {
            "attempt": attempt,
            "execution_class": manifest.execution_class,
            "performance_valid": manifest.performance_valid,
        }
    )
    (case_dir / "metadata.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True), encoding="utf-8"
    )
    status = {
        "valid": False,
        "phase": "setup",
        "failure_class": None,
        "error": None,
    }
    processes = []
    # "0:0" = "no log yet", i.e. everything in it is this case's.
    event_cursor = "0:0"
    # Only used when case.scenario.segments is non-empty (see
    # _run_segment_schedule below); always bound so the `finally` block can
    # unconditionally stop/join them regardless of where an exception was
    # raised in the try block below.
    segment_stop_event = None
    segment_thread = None
    segment_results = []
    # Bound so the `finally` block's best-effort diagnostic collection
    # (_collect_failure_diagnostics) can run regardless of how far the try
    # block got before raising -- these are only ever assigned once the
    # corresponding popen() call below actually runs.
    server_process = None
    metrics_process = None
    perf_process = None
    kernel_health_fatal = None
    try:
        status["phase"] = "path"
        path_result = configure_path(
            rtt_ms=case.scenario.rtt_ms,
            rate_mbit=case.scenario.rate_mbit,
            queue_bdp=case.scenario.queue_bdp,
            loss_model=case.scenario.loss_model,
            loss_pct=case.scenario.loss_pct,
            loss_direction=case.scenario.loss_direction,
            burst_length=case.scenario.burst_length,
            seed=case.seed,
            reorder_pct=case.scenario.reorder_pct,
            reorder_correlation=case.scenario.reorder_correlation,
            router_sudo=router_sudo,
        )
        write_text(case_dir / "path.txt", path_result.stdout + path_result.stderr)
        if path_result.returncode != 0:
            raise RuntimeError(
                f"path configuration exited with {path_result.returncode}"
            )

        status["phase"] = "profile"
        profile_result = apply_profile(case, manifest, server)
        rack_rto_result = (
            apply_rack_rto(case, server) if profile_result.returncode == 0 else None
        )
        module_config_result = (
            apply_module_config(case, server)
            if rack_rto_result is not None and rack_rto_result.returncode == 0
            else None
        )
        retransmit_dscp_result = (
            apply_retransmit_dscp(case, server)
            if module_config_result is not None and module_config_result.returncode == 0
            else None
        )
        profile_text = profile_result.stdout + profile_result.stderr
        if rack_rto_result is not None:
            profile_text += (
                "\n[rack-rto]\n" + rack_rto_result.stdout + rack_rto_result.stderr
            )
        if module_config_result is not None:
            profile_text += (
                "\n[module-config]\n"
                + module_config_result.stdout
                + module_config_result.stderr
            )
        if retransmit_dscp_result is not None:
            profile_text += (
                "\n[retransmit-dscp]\n"
                + retransmit_dscp_result.stdout
                + retransmit_dscp_result.stderr
            )
        write_text(case_dir / "profile.txt", profile_text)
        if profile_result.returncode != 0:
            raise RuntimeError(
                f"guest profile exited with {profile_result.returncode}"
            )
        if rack_rto_result is not None and rack_rto_result.returncode != 0:
            raise RuntimeError(
                f"rack rto tuning exited with {rack_rto_result.returncode}"
            )
        if module_config_result is not None and module_config_result.returncode != 0:
            raise RuntimeError(
                f"module config exited with {module_config_result.returncode}"
            )
        if retransmit_dscp_result is not None and retransmit_dscp_result.returncode != 0:
            raise RuntimeError(
                f"retransmit dscp config exited with {retransmit_dscp_result.returncode}"
            )
        client_buffers_result = raise_client_socket_buffers(client)
        write_text(
            case_dir / "client-buffers.txt",
            client_buffers_result.stdout + client_buffers_result.stderr,
        )
        if client_buffers_result.returncode != 0:
            raise RuntimeError(
                f"client socket buffer tuning exited with {client_buffers_result.returncode}"
            )

        status["phase"] = "offload"
        server_offload = apply_offload(case, manifest, server)
        client_offload = apply_offload(case, manifest, client)
        write_text(
            case_dir / "offload.txt",
            "server:\n"
            + server_offload.stdout
            + server_offload.stderr
            + "\nclient:\n"
            + client_offload.stdout
            + client_offload.stderr,
        )
        if server_offload.returncode or client_offload.returncode:
            raise RuntimeError("failed to apply requested offload state")
        write_text(
            case_dir / "offload-state.txt",
            "server:\n"
            + server.run(
                ["ethtool", "-k", manifest.server_data_interface], check=False
            ).stdout
            + "\nclient:\n"
            + client.run(
                ["ethtool", "-k", manifest.server_data_interface], check=False
            ).stdout,
        )

        status["phase"] = "snapshot"
        before = qdisc_snapshot(router_sudo)
        write_text(case_dir / "qdisc-before.json", before.stdout)
        write_text(
            case_dir / "nstat-before.txt",
            server.run(["nstat", "-az"], check=False).stdout,
        )
        # An INODE:LINES cursor rather than a line number: the daemon
        # rotates the log at runtime.events_max_mib, and the snapshot
        # script needs to know which file the count was taken in.
        event_cursor = (
            server.run(
                [
                    "sudo",
                    "/opt/skyline-speeder/infra/snapshot-skyline-events.sh",
                    "cursor",
                ],
                check=False,
            ).stdout.strip()
            or "0:0"
        )
        write_text(
            case_dir / "skyline-status-before.json",
            server.run(["sudo", "ssctl", "status"], check=False).stdout,
        )

        total_time = case.warmup_s + case.duration_s + 2
        server_process = server.popen(
            [
                "sudo",
                "/opt/skyline-speeder/infra/run-in-skyline-cgroup.sh",
                "timeout",
                str(total_time + 30),
                "iperf3",
                "-s",
                "-1",
                "-J",
            ]
        )
        metrics_process = server.popen(
            [
                "/opt/skyline-speeder/infra/collect-guest-metrics.sh",
                str(total_time),
                "1",
            ]
        )
        perf_process = server.popen(
            [
                "sudo",
                "perf",
                "stat",
                "-a",
                "-x,",
                "-e",
                "cycles,instructions,cache-misses,context-switches",
                "--",
                "sleep",
                str(total_time),
            ]
        )
        processes.extend([server_process, metrics_process, perf_process])
        # Retransmit-DSCP mechanism verification only (case.scenario.
        # capture_retransmits) -- deliberately NOT on by default, since
        # tcpdump on the sender costs real CPU and would pollute the
        # neutrality/overhead comparison cases that reuse this same
        # scenario shape with the flag left off. Captures to a FILE on the
        # guest (not through this popen's own stdout pipe) specifically to
        # avoid the deadlock a live, undrained pipe would hit at realistic
        # packet rates -- see analyze_results.py's retransmit-DSCP oracles
        # for why sender-side (not receiver-side) capture is required for
        # precision in a lossy scenario.
        retransmit_capture = {}
        if case.scenario.capture_retransmits:
            # IPv6's fixed header (40B) is twice IPv4's (20B) -- keep the
            # same TCP-header headroom past the L3 header on both families
            # rather than reusing IPv4's budget and silently truncating v6
            # captures before the TCP header (which would then look like
            # kernel-side drops to analyze_results.py's dropped-packet
            # check, not a snaplen problem).
            snaplen = 128 if case.scenario.address_family == "v6" else 96
            for role, remote, path in (
                ("server", server, SKYLINE_RETRANSMIT_CAPTURE_SERVER_PATH),
                ("client", client, SKYLINE_RETRANSMIT_CAPTURE_CLIENT_PATH),
            ):
                process = remote.popen(
                    [
                        "bash",
                        "-lc",
                        "exec sudo timeout {timeout} tcpdump -nn -v -S -s {snaplen} "
                        "-c {packet_cap} -i {iface} 'tcp port 5201' "
                        "> {path} 2>&1".format(
                            timeout=total_time,
                            snaplen=snaplen,
                            # Bounds worst-case capture size: a high-throughput,
                            # low-loss case (e.g. retransmit-zero-loss) can sustain
                            # tens of thousands of packets/sec for the whole
                            # window with nothing to cut it short, which measured
                            # 30-65MB / 250k-480k lines per file before this cap
                            # was added -- multi-minute analyze_results.py
                            # runtimes for no analytical benefit, since the
                            # precision oracle only needs a representative
                            # contiguous prefix, not every packet. -c stops
                            # tcpdump early (not a kernel drop -- still reports
                            # "0 packets dropped by kernel"), so a capped run may
                            # show retransmit_capture_marked below the BPF-side
                            # retransmits_marked delta for that case; that is
                            # expected and does not indicate a bug (unlike an
                            # actual kernel drop, which the existing dropped-
                            # packet check still catches and invalidates).
                            packet_cap=150_000,
                            iface=manifest.server_data_interface,
                            path=path,
                        ),
                    ]
                )
                retransmit_capture[role] = process
                processes.append(process)
        readiness = wait_for_iperf_server(
            server,
            30 if manifest.execution_class == "tcg-validation" else 10,
        )
        write_text(
            case_dir / "iperf-server-readiness.txt",
            readiness.stdout + readiness.stderr,
        )
        if readiness.returncode != 0:
            raise RuntimeError("iperf3 server did not become ready")

        if case.scenario.segments:
            segment_stop_event = threading.Event()
            segment_thread = threading.Thread(
                target=_run_segment_schedule,
                args=(case, router_sudo, segment_stop_event, segment_results),
                daemon=True,
            )
            segment_thread.start()

        status["phase"] = "iperf"
        if case.scenario.address_family == "v6":
            server_addr = manifest.server_data_ip6
            family_flag = "-6"
        else:
            server_addr = manifest.server_data_ip
            family_flag = "-4"
        iperf_client_command = [
            "iperf3",
            "-c",
            server_addr,
            family_flag,
            "-R",
            "-O",
            str(case.warmup_s),
            "-t",
            str(case.duration_s),
            "-P",
            str(case.scenario.parallel),
            "-J",
        ]
        if case.scenario.segments:
            # Finer per-interval resolution for the intervals[] array that
            # segment_goodput() (analyze_results.py) buckets by segment --
            # the default 1s reporting is coarse relative to segments as
            # short as ~8s. Left at iperf3's default everywhere else so
            # this doesn't perturb historical cases' output format/volume.
            iperf_client_command += ["-i", "0.5"]
        try:
            client_result = client.run(
                iperf_client_command,
                check=False,
                timeout=total_time + 30,
            )
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(
                f"iperf3 client did not exit within {total_time + 30}s "
                "(dead connection likely never closed on a lossy path)"
            ) from error
        write_text(case_dir / "iperf-client.json", client_result.stdout)
        write_text(case_dir / "iperf-client.stderr", client_result.stderr)
        if client_result.returncode != 0:
            raise RuntimeError(f"iperf3 client exited with {client_result.returncode}")

        if retransmit_capture:
            # Each capture's own `timeout {total_time}` wrapper already
            # bounds its lifetime to roughly the flow duration -- by now the
            # client has already run for that long, so this just waits out
            # whatever's left of tcpdump's own shutdown, then the capture
            # file is complete and safe to pull back.
            for role, process in retransmit_capture.items():
                communicate(process, 15)
            remotes = {"server": server, "client": client}
            paths = {
                "server": SKYLINE_RETRANSMIT_CAPTURE_SERVER_PATH,
                "client": SKYLINE_RETRANSMIT_CAPTURE_CLIENT_PATH,
            }
            for role in ("server", "client"):
                remote = remotes[role]
                pulled = remote.run(["cat", paths[role]], check=False)
                write_text(
                    case_dir / f"retransmit-capture-{role}.txt", pulled.stdout
                )
                remote.run(["sudo", "rm", "-f", paths[role]], check=False)

        if segment_thread is not None:
            # By now the client has already run for the full flow duration,
            # so every segment offset (validated to leave a >=5s tail before
            # the flow ends) is already in the past -- a short join is just
            # waiting out whatever's left of the last transition's own
            # subprocess call, not the schedule itself.
            segment_thread.join(timeout=5)
            write_text(
                case_dir / "segment-timeline.json",
                json.dumps(segment_results, indent=2, sort_keys=True),
            )
            expected = len(case.scenario.segments)
            failed = [r for r in segment_results if r.get("returncode") != 0]
            if segment_thread.is_alive() or len(segment_results) != expected or failed:
                raise RuntimeError(
                    "segment schedule incomplete or failed: "
                    f"{len(segment_results)}/{expected} transitions recorded, "
                    f"thread_alive={segment_thread.is_alive()}, failures={failed}"
                )

        code, stdout, stderr = communicate(server_process, 15)
        write_text(case_dir / "iperf-server.json", stdout)
        write_text(case_dir / "iperf-server.stderr", stderr)
        if code != 0:
            raise RuntimeError(f"iperf3 server exited with {code}")

        status["phase"] = "metrics"
        code, stdout, stderr = communicate(metrics_process, 15)
        write_text(case_dir / "ss-metrics.jsonl", stdout)
        write_text(case_dir / "ss-metrics.stderr", stderr)
        if code != 0:
            raise RuntimeError(f"metrics collector exited with {code}")

        _, stdout, stderr = communicate(perf_process, 15)
        write_text(case_dir / "perf.stdout", stdout)
        write_text(case_dir / "perf.csv", stderr)
        write_text(
            case_dir / "nstat-after.txt",
            server.run(["nstat", "-az"], check=False).stdout,
        )
        write_text(
            case_dir / "skyline-events.jsonl",
            server.run(
                [
                    "sudo",
                    "/opt/skyline-speeder/infra/snapshot-skyline-events.sh",
                    "since",
                    event_cursor,
                ],
                check=False,
            ).stdout,
        )
        write_text(
            case_dir / "skyline-status-after.json",
            server.run(["sudo", "ssctl", "status"], check=False).stdout,
        )
        kernel_log = server.run(
            ["sudo", "journalctl", "-k", "-n", "300", "--no-pager"],
            check=False,
        )
        kernel_log_text = kernel_log.stdout + kernel_log.stderr
        write_text(case_dir / "kernel-log.txt", kernel_log_text)
        kernel_issue = KERNEL_HEALTH_PATTERN.search(kernel_log_text)
        if kernel_issue:
            kernel_health_fatal = f"kernel health marker detected: {kernel_issue.group(0)}"
        after = qdisc_snapshot(router_sudo)
        write_text(case_dir / "qdisc-after.json", after.stdout)
        # Deliberately gated on the kernel staying healthy: a case whose
        # own iperf/metrics/perf steps all reported success can still have
        # run against a guest that panicked/soft-locked mid-flow (this is
        # exactly what happened live -- see KernelHealthFatal's own doc
        # comment), so "every step returned 0" is not sufficient for
        # calling the resulting data trustworthy.
        if kernel_issue is None:
            status["valid"] = True
            status["phase"] = "complete"
        else:
            status["phase"] = "kernel-health"
    except Exception as error:
        status["failure_class"] = failure_class(status["phase"])
        status["error"] = str(error)
    finally:
        # Must run BEFORE terminate_process/cleanup_case below: if the try
        # block raised before reaching the join a few lines up (e.g. the
        # client-timeout RuntimeError path), segment_thread could still be
        # sleeping toward its next transition. Stopping and joining it here
        # first prevents it from waking up mid-cleanup and firing a stale
        # `tc ... change` call against whatever case runs next.
        if segment_stop_event is not None:
            segment_stop_event.set()
        if segment_thread is not None:
            segment_thread.join(timeout=5)
        if not status["valid"] and kernel_health_fatal is None:
            # The try block raised before reaching its own diagnostic
            # collection above (e.g. the iperf3 client itself failed) --
            # recover what we can, best-effort, rather than let the
            # exception path be the one path that loses server/metrics/
            # perf output and the kernel log entirely.
            kernel_health_fatal = _collect_failure_diagnostics(
                case_dir,
                server,
                server_process,
                metrics_process,
                perf_process,
                event_cursor,
                router_sudo,
            )
        for process in processes:
            terminate_process(process)
        cleanup_ok, cleanup_text = cleanup_case(
            manifest, server, client, router_sudo
        )
        write_text(case_dir / "cleanup.txt", cleanup_text)
        if not cleanup_ok:
            status["valid"] = False
            status["failure_class"] = "SAFETY"
            cleanup_error = "failed to restore guest profile, offload, or host path"
            status["error"] = (
                f"{status['error']}; {cleanup_error}"
                if status["error"]
                else cleanup_error
            )
        if kernel_health_fatal is not None:
            status["valid"] = False
            status["failure_class"] = "INFRA"
            status["error"] = (
                f"{status['error']}; {kernel_health_fatal}"
                if status["error"]
                else kernel_health_fatal
            )
        (case_dir / "status.json").write_text(
            json.dumps(status, indent=2, sort_keys=True), encoding="utf-8"
        )
    if kernel_health_fatal is not None:
        # Raised AFTER the finally block (not from within it) so cleanup
        # and status.json still happen first -- this is what lets the
        # campaign loop distinguish "stop the whole campaign now" from an
        # ordinary per-case failure that --keep-going is meant to survive.
        raise KernelHealthFatal(
            f"{case.case_id}: {kernel_health_fatal} -- stopping campaign, "
            "guest needs a reboot/restore before any further cases can "
            "produce trustworthy data"
        )
    return status["valid"]


def case_attempts(output_dir, case):
    prefix = f"{case.case_id}--attempt"
    attempts = []
    for path in output_dir.glob(f"{prefix}*"):
        suffix = path.name.removeprefix(prefix)
        if path.is_dir() and suffix.isdigit():
            attempts.append((int(suffix), path))
    return sorted(attempts)


def successful_attempt(attempts):
    for _, path in reversed(attempts):
        status_path = path / "status.json"
        if not status_path.is_file():
            continue
        try:
            status = json.loads(status_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            continue
        if status.get("valid"):
            return path
    return None


def load_analysis_invalid_case_ids(csv_path):
    # analyze_results.py's runs.csv has exactly one row per case_id (the
    # latest attempt found on disk, re-evaluated against its own stricter
    # checks -- kernel health, capture false positives, etc. -- that the
    # runner's own per-case status.json does not apply). `valid` is
    # written by csv.DictWriter as the literal string "True"/"False"; any
    # other value (including missing/malformed rows) is treated as
    # not-confirmed-valid, since the cost of retrying an already-good case
    # is small compared to silently leaving a bad one in the campaign.
    invalid_case_ids = set()
    with csv_path.open(newline="", encoding="utf-8") as handle:
        for row in csv.DictReader(handle):
            if row.get("valid") != "True":
                invalid_case_ids.add(row.get("case_id"))
    return invalid_case_ids


def append_campaign_event(output_dir, event):
    event = {"timestamp": time.time(), **event}
    with (output_dir / "campaign-events.jsonl").open(
        "a", encoding="utf-8"
    ) as handle:
        handle.write(json.dumps(event, sort_keys=True) + "\n")


def prepare_campaign(
    args, manifest, vm_environment, guest_identity, manifest_bytes
):
    output_dir = Path(args.output_dir)
    manifest_sha = sha256_bytes(manifest_bytes)
    identity = execution_identity(vm_environment)
    identity["guests"] = guest_identity
    campaign = {
        "manifest_sha256": manifest_sha,
        "execution_class": manifest.execution_class,
        "performance_valid": manifest.performance_valid,
        "execution_identity": identity,
    }
    if args.resume:
        if not output_dir.is_dir():
            raise RuntimeError(f"resume directory does not exist: {output_dir}")
        campaign_path = output_dir / "campaign.json"
        if not campaign_path.is_file():
            raise RuntimeError("resume directory lacks campaign.json")
        existing = json.loads(campaign_path.read_text(encoding="utf-8"))
        if existing != campaign:
            raise RuntimeError("resume manifest or VM execution identity changed")
        append_campaign_event(output_dir, {"event": "resume"})
        return output_dir

    output_dir.mkdir(parents=True, exist_ok=False)
    (output_dir / "manifest.toml").write_bytes(manifest_bytes)
    (output_dir / "campaign.json").write_text(
        json.dumps(campaign, indent=2, sort_keys=True), encoding="utf-8"
    )
    append_campaign_event(output_dir, {"event": "start"})
    return output_dir


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest")
    parser.add_argument("output_dir")
    parser.add_argument("--execute", action="store_true")
    parser.add_argument("--no-router-sudo", action="store_true")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--keep-going", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument(
        "--retry-analysis-invalid",
        metavar="RUNS_CSV",
        help=(
            "Path to a runs.csv produced by analyze_results.py against this "
            "same output directory. A case whose runner-level status.json "
            "said valid=true but whose row in RUNS_CSV says valid=False "
            "(e.g. analyze_results.py's own kernel-health/capture-false-"
            "positive checks caught something the runner's own per-case "
            "checks did not) is treated as not-yet-successful, so a new "
            "attempt is created for it instead of being skipped. Requires "
            "--resume; never edits or deletes the original attempt."
        ),
    )
    args = parser.parse_args()
    if args.resume and not args.execute:
        parser.error("--resume requires --execute")
    if args.retry_analysis_invalid and not args.resume:
        parser.error("--retry-analysis-invalid requires --resume")

    manifest_path = Path(args.manifest)
    manifest_bytes = manifest_path.read_bytes()
    manifest = load_manifest(manifest_path)
    cases = expand_runs(manifest)
    if args.limit is not None:
        cases = cases[: args.limit]
    if not args.execute:
        print(
            json.dumps(
                {
                    "mode": "dry-run",
                    "experiment": manifest.name,
                    "execution_class": manifest.execution_class,
                    "performance_valid": manifest.performance_valid,
                    "runs": len(cases),
                    "first_case": cases[0].as_dict() if cases else None,
                },
                indent=2,
                sort_keys=True,
            )
        )
        return

    _, vm_environment = load_vm_environment()
    validate_execution_class(manifest, vm_environment)
    server = Remote.parse(manifest.server_ssh)
    client = Remote.parse(manifest.client_ssh)
    guest_identity = guest_execution_identity(server, client)
    output_dir = prepare_campaign(
        args, manifest, vm_environment, guest_identity, manifest_bytes
    )
    if not args.resume:
        (output_dir / "environment.json").write_text(
            json.dumps(
                snapshot_environment(
                    server,
                    client,
                    manifest.server_data_interface,
                    vm_environment,
                ),
                indent=2,
                sort_keys=True,
            ),
            encoding="utf-8",
        )

    retry_case_ids = (
        load_analysis_invalid_case_ids(Path(args.retry_analysis_invalid))
        if args.retry_analysis_invalid
        else set()
    )

    failures = 0
    executed = 0
    skipped = 0
    kernel_health_stop = False
    for index, case in enumerate(cases, 1):
        attempts = case_attempts(output_dir, case)
        successful = successful_attempt(attempts)
        if successful and case.case_id not in retry_case_ids:
            print(
                f"[{index}/{len(cases)}] skip {case.case_id}; "
                f"valid attempt={successful.name}",
                flush=True,
            )
            skipped += 1
            continue
        if successful and case.case_id in retry_case_ids:
            print(
                f"[{index}/{len(cases)}] {case.case_id} was runner-valid "
                f"but analysis-invalid; retrying",
                flush=True,
            )
        attempt = attempts[-1][0] + 1 if attempts else 1
        case_dir = output_dir / f"{case.case_id}--attempt{attempt:02d}"
        print(
            f"[{index}/{len(cases)}] {case.case_id} attempt={attempt}",
            flush=True,
        )
        append_campaign_event(
            output_dir,
            {"event": "case-start", "case_id": case.case_id, "attempt": attempt},
        )
        try:
            valid = run_case(
                case,
                manifest,
                case_dir,
                attempt,
                server,
                client,
                not args.no_router_sudo,
            )
        except KernelHealthFatal as error:
            # Deliberately ignores --keep-going: every case after this one
            # would share the same compromised guest, so continuing would
            # only spend time producing more data that has to be thrown
            # away once this is noticed.
            print(f"FATAL: {error}", file=sys.stderr, flush=True)
            executed += 1
            failures += 1
            append_campaign_event(
                output_dir,
                {
                    "event": "case-complete",
                    "case_id": case.case_id,
                    "attempt": attempt,
                    "valid": False,
                    "kernel_health_fatal": True,
                },
            )
            kernel_health_stop = True
            break
        executed += 1
        append_campaign_event(
            output_dir,
            {
                "event": "case-complete",
                "case_id": case.case_id,
                "attempt": attempt,
                "valid": valid,
            },
        )
        if not valid:
            failures += 1
            if not args.keep_going:
                break
    append_campaign_event(
        output_dir,
        {
            "event": "finish",
            "executed": executed,
            "skipped": skipped,
            "failures": failures,
        },
    )
    print(
        f"executed {executed}, skipped {skipped}, invalid {failures}; "
        f"output={output_dir}"
    )
    if kernel_health_stop:
        print(
            "stopped early: kernel health event detected -- restore/reboot "
            "the guest, then re-run with --resume "
            "--retry-analysis-invalid=<analysis/runs.csv> (from a run of "
            "analyze_results.py against this output directory) to recover "
            "every case the analyzer would otherwise reject.",
            file=sys.stderr,
        )
    if failures:
        sys.exit(1)


if __name__ == "__main__":
    main()
