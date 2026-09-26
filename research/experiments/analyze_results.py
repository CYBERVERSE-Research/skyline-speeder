#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import json
import math
import random
import re
import statistics
from collections import defaultdict
from pathlib import Path


#: Shared with run_matrix.py's per-case kernel-health gate (see
#: run_case()'s diagnostic-collection step) -- kept as a single source of
#: truth so the runner's real-time gate and this script's post-hoc check
#: can never silently drift apart.
KERNEL_HEALTH_PATTERN = re.compile(
    r"kernel panic|\bOops:|general protection fault|watchdog: BUG|"
    r"BUG: unable to handle|TCP: out of memory",
    re.IGNORECASE,
)

RUN_FIELDS = [
    "case_id",
    "attempt",
    "valid",
    "failure_class",
    "execution_class",
    "performance_valid",
    "profile_id",
    "scenario_id",
    "modules",
    "rtt_ms",
    "rate_mbit",
    "queue_bdp",
    "loss_model",
    "loss_pct",
    "loss_direction",
    "parallel",
    "offload",
    "address_family",
    "run_index",
    "seed",
    "goodput_mbps",
    "sender_mbps",
    "retransmits",
    "mean_rtt_ms",
    "p95_sampled_rtt_ms",
    "max_cwnd_packets",
    "sender_cpu_pct",
    "receiver_cpu_pct",
    "kernel_health",
    "rack_rto_enabled",
    "rack_rto_applied",
    "rack_rto_rejected",
    "median_rto_ms",
    "max_rto_ms",
    "rack_rto_max_enabled",
    "rack_rto_max_applied",
    "rack_rto_max_rejected",
    "rack_rto_max_congested",
    "metrics_ack_events",
    "metrics_loss_events",
    "metrics_state_transitions",
    "metrics_pacing_updates",
    "metrics_guardrail_hits",
    "metrics_hypothetical_early_loss",
    "metrics_prr_adjustments",
    "n_segments",
    "segment_goodput_json",
    "retransmit_dscp_enabled",
    "retransmit_dscp_value",
    "retransmit_packets_seen",
    "retransmit_detected",
    "retransmit_marked",
    "retransmit_csum_fixups",
    "retransmit_abi_mismatch",
    "retransmit_ipv6_marked",
    "retransmit_ipv6_chain_bailout",
    "retransmit_capture_candidates",
    "retransmit_capture_marked",
    "retransmit_capture_false_positives",
    "retransmit_capture_dropped_server",
    "retransmit_capture_client_arrived",
    "retransmit_capture_dropped_client",
    "error",
]

# Fields of ssctl status --json's "metrics" object (mirrors struct skyline_metrics in
# bpf/include/skyline_abi.h) that are worth surfacing as before/after deltas.
# "ack_events" and "delivered_packets" are omitted from RUN_FIELDS (pure
# volume counters, not diagnostic of "did this feature ever fire") but are
# still read here for completeness in case a future column wants them.
# non_congestion_losses/congestion_losses/cwnd_reduction_events are not
# tracked (M3 is not a multi-way classifier and M2 never does a one-time
# classified cwnd cut -- see
# bpf/include/skyline_abi.h's top-of-file comment).
METRICS_DELTA_FIELDS = (
    "ack_events",
    "delivered_packets",
    "loss_events",
    "state_transitions",
    "pacing_updates",
    "guardrail_hits",
    "hypothetical_early_loss",
    "prr_adjustments",
)


def percentile(values, percentile_value):
    if not values:
        return float("nan")
    ordered = sorted(values)
    position = (len(ordered) - 1) * percentile_value / 100
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def bootstrap_mean_ci(values, seed=20260728, samples=2000):
    if not values:
        return float("nan"), float("nan")
    if len(values) == 1:
        return values[0], values[0]
    generator = random.Random(seed)
    means = []
    for _ in range(samples):
        sample = [generator.choice(values) for _ in values]
        means.append(statistics.mean(sample))
    return percentile(means, 2.5), percentile(means, 97.5)


def parse_iperf(payload):
    end = payload.get("end", {})
    sum_received = end.get("sum_received", {})
    sum_sent = end.get("sum_sent", {})
    streams = end.get("streams", [])
    rtts = [
        stream.get("sender", {}).get("mean_rtt")
        for stream in streams
        if stream.get("sender", {}).get("mean_rtt") is not None
    ]
    cpu = end.get("cpu_utilization_percent", {})
    return {
        "goodput_mbps": float(sum_received.get("bits_per_second", 0)) / 1_000_000,
        "sender_mbps": float(sum_sent.get("bits_per_second", 0)) / 1_000_000,
        "retransmits": int(sum_sent.get("retransmits", 0) or 0),
        "mean_rtt_ms": statistics.mean(rtts) / 1000 if rtts else 0.0,
        # In reverse mode the remote endpoint is the data sender.
        "sender_cpu_pct": float(cpu.get("remote_total", 0) or 0),
        "receiver_cpu_pct": float(cpu.get("host_total", 0) or 0),
    }


def segment_goodput(run_dir, scenario):
    """For a segmented scenario (see Segment in matrix_lib.py), bucket
    iperf-client.json's per-interval throughput by which segment was live at
    each interval's midpoint, using segment-timeline.json's recorded
    requested offsets as bucket boundaries (both are relative to the same
    t0 -- the client iperf3 process launching). A whole-case mean would
    blend the calm/cliff/recovery regimes together and hide exactly what a
    segmented scenario exists to observe (recovery slope, throughput during
    the cliff); returns [] for a non-segmented case, or if the timeline
    file is missing (e.g. an older attempt, or one where the segment
    schedule itself failed and the case is already invalid).
    """
    segments = scenario.get("segments") or []
    if not segments:
        return []
    timeline_path = run_dir / "segment-timeline.json"
    iperf_path = run_dir / "iperf-client.json"
    if not timeline_path.is_file() or not iperf_path.is_file():
        return []
    try:
        timeline = json.loads(timeline_path.read_text(encoding="utf-8"))
        payload = json.loads(iperf_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return []
    ordered = sorted(timeline, key=lambda entry: entry["requested_offset_s"])
    boundaries = [0.0] + [float(entry["requested_offset_s"]) for entry in ordered]
    labels = [""] + [entry.get("label", "") for entry in ordered]
    buckets = defaultdict(list)
    for interval in payload.get("intervals", []):
        summary = interval.get("sum", {})
        start, end, bps = summary.get("start"), summary.get("end"), summary.get(
            "bits_per_second"
        )
        if start is None or end is None or bps is None:
            continue
        midpoint = (float(start) + float(end)) / 2.0
        index = 0
        for candidate, boundary in enumerate(boundaries):
            if midpoint >= boundary:
                index = candidate
        buckets[index].append(float(bps) / 1_000_000)
    return [
        {
            "segment_index": index,
            "label": labels[index] if index < len(labels) else "",
            "offset_s": boundaries[index] if index < len(boundaries) else 0.0,
            "mean_goodput_mbps": statistics.mean(values),
            # A segment with active RTO recovery can have a long stall
            # followed by a single 0.5s interval where iperf3 reports the
            # delivery of everything that was buffered/retransmitted during
            # it, at a rate far above the shaped ceiling for that segment
            # (observed: a 15Mbit-capped segment with a single interval
            # >600Mbit/s) -- the mean gets dragged hard by these, median
            # doesn't. Report both rather than picking one silently.
            "median_goodput_mbps": statistics.median(values),
            "n_intervals": len(values),
        }
        for index, values in sorted(buckets.items())
    ]


def parse_ss_samples(path):
    rtts = []
    cwnds = []
    rtos = []
    if not path.is_file():
        return rtts, cwnds, rtos
    rtt_pattern = re.compile(r"\brtt:([0-9]+(?:\.[0-9]+)?)/")
    cwnd_pattern = re.compile(r"\bcwnd:([0-9]+)")
    rto_pattern = re.compile(r"\brto:([0-9]+(?:\.[0-9]+)?)")
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            try:
                sample = json.loads(line)
            except json.JSONDecodeError:
                continue
            text = sample.get("ss", "")
            rtts.extend(float(value) for value in rtt_pattern.findall(text))
            cwnds.extend(int(value) for value in cwnd_pattern.findall(text))
            rtos.extend(float(value) for value in rto_pattern.findall(text))
    return rtts, cwnds, rtos


def skyline_metrics_delta(run_dir):
    """Before/after delta of ssctl status --json's `metrics` field (struct
    skyline_metrics), the percpu counters that were populated from day one but
    never read by the Rust side until the feature-coverage debug pass. Used
    to directly confirm whether a given case actually exercised a specific
    decision path (e.g. guardrail_hits>0, hypothetical_early_loss>0)
    instead of inferring it indirectly from event logs alone.
    """

    def metrics_of(path):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        status = payload.get("status")
        if not isinstance(status, dict):
            return {}
        metrics = status.get("metrics")
        return metrics if isinstance(metrics, dict) else {}

    before = metrics_of(run_dir / "skyline-status-before.json")
    after = metrics_of(run_dir / "skyline-status-after.json")
    return {
        field: max(0, after.get(field, 0) - before.get(field, 0))
        for field in METRICS_DELTA_FIELDS
    }


#: Fields of ssctl status --json's `rack_rto.stats` object (mirrors struct
#: skyline_rto_stats in bpf/include/skyline_abi.h) surfaced as before/after deltas.
#: The rto_max_* fields (since v2 of struct skyline_rto_tuning, the TCP_RTO_MAX_MS
#: ceiling feature) are diagnostic-only, same role as applied/rejected below
#: for the pre-existing TCP_BPF_RTO_MIN floor: confirm the mechanism actually
#: fired for a case, don't silently trust "it was configured".
RACK_RTO_STATS_DELTA_FIELDS = (
    "applied",
    "rejected",
    "rto_max_applied",
    "rto_max_rejected",
    "rto_max_congested",
)


def rack_rto_stats_delta(run_dir):
    """M1 tier-2 diagnostic: before/after delta of ssctl status --json's
    `rack_rto.stats` field."""

    def stats_of(path):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        status = payload.get("status")
        if not isinstance(status, dict):
            return {}
        stats = status.get("rack_rto", {}).get("stats")
        return stats if isinstance(stats, dict) else {}

    before = stats_of(run_dir / "skyline-status-before.json")
    after = stats_of(run_dir / "skyline-status-after.json")
    return {
        field: max(0, after.get(field, 0) - before.get(field, 0))
        for field in RACK_RTO_STATS_DELTA_FIELDS
    }


#: Fields of ssctl status --json's `retransmit_dscp.stats` object (mirrors struct
#: skyline_retransmit_dscp_stats in bpf/include/skyline_abi.h). In-kernel truth, zero
#: parsing, always available even when case.scenario.capture_retransmits is
#: off -- the right oracle for gating checks (enabled/disabled,
#: abi_mismatch). NOT the right oracle for absolute counts against the
#: capture-derived columns below: retransmits_marked counts skbs, one per TC
#: pass, while the capture counts wire packets -- these diverge at high
#: throughput because skb->gso_segs>1 shows up even with offload=off fully
#: confirmed (tso/gso/gro all off, MTU-sized wire packets, reproduced on an
#: isolated connection -- see retransmit-dscp-mechanism.toml's header
#: comment). retransmit_capture_marked/false_positives, not this field, is
#: the authority on marking correctness.
RETRANSMIT_DSCP_STATS_DELTA_FIELDS = (
    "packets_seen",
    "retransmits_detected",
    "retransmits_marked",
    "csum_fixups",
    "abi_mismatch",
    "ipv6_marked",
    "ipv6_chain_bailout",
)


def retransmit_dscp_stats_delta(run_dir):
    """Retransmit-DSCP diagnostic: before/after delta of ssctl status --json's
    `retransmit_dscp.stats` field."""

    def stats_of(path):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        status = payload.get("status")
        if not isinstance(status, dict):
            return {}
        stats = status.get("retransmit_dscp", {}).get("stats")
        return stats if isinstance(stats, dict) else {}

    before = stats_of(run_dir / "skyline-status-before.json")
    after = stats_of(run_dir / "skyline-status-after.json")
    return {
        field: max(0, after.get(field, 0) - before.get(field, 0))
        for field in RETRANSMIT_DSCP_STATS_DELTA_FIELDS
    }


#: Two physical lines per TCP packet in `tcpdump -nn -v -S` output: the
#: IP-header summary (has tos) and an indented TCP-layer summary (has
#: Flags/seq). A pure ACK/zero-window-probe gets NO seq field printed at
#: all -- tcpdump itself omits it -- which conveniently means "no seq
#: match" already excludes exactly the packets the BPF program's own
#: end_seq==seq rule excludes, with no extra logic needed here.
_TCPDUMP_HEADER_RE = re.compile(r"^\d{2}:\d{2}:\d{2}\.\d+ IP \(tos (0x[0-9a-fA-F]+),")
_TCPDUMP_ENDPOINTS_RE = re.compile(r"^\s*([\d.]+)\.(\d+) > ([\d.]+)\.(\d+):")
#: IPv6, confirmed against a real capture (both an unmarked and a DSCP-marked
#: sample) -- structurally different from IPv4's two-line form above: the L3
#: summary and the endpoints/Flags/seq detail are ALL on one line, no
#: look-ahead to a second line needed. "class 0x.." is the IPv6 traffic-class
#: byte (DSCP high 6 bits + ECN low 2, bit-for-bit the same layout as IPv4's
#: "tos") -- and, confirmed on the real unmarked-packet sample, tcpdump OMITS
#: the "class 0x.., " clause entirely when traffic class is 0 rather than
#: printing "class 0x0, "; group(1) is None in that case and must be read as
#: 0, not treated as a parse failure.
#: The parenthetical's own content has a nested "next-header TCP (6)" --
#: [^)]* would stop dead at that inner ")", so this needs a non-greedy .*?
#: (happy to cross a ")" while backtracking) rather than an excluded-")"
#: character class.
_TCPDUMP_V6_LINE_RE = re.compile(
    r"^\d{2}:\d{2}:\d{2}\.\d+ IP6 \((?:class (0x[0-9a-fA-F]+), )?flowlabel .*?\) "
    r"([0-9a-fA-F:]+)\.(\d+) > ([0-9a-fA-F:]+)\.(\d+):"
)
_TCPDUMP_SEQ_RANGE_RE = re.compile(r"\bseq (\d+):(\d+)\b")
_TCPDUMP_SEQ_SINGLE_RE = re.compile(r"\bseq (\d+)\b")
_TCPDUMP_FLAGS_RE = re.compile(r"Flags \[([^\]]*)\]")
_TCPDUMP_DROPPED_RE = re.compile(r"(\d+) packets dropped by kernel")


def parse_tcpdump_capture(text):
    """Reconstructs one dict per TCP segment tcpdump printed a seq for:
    `{tos, seq, end_seq, src, sport, dst, dport, flags}`. `end_seq` already
    matches the BPF program's own definition (seq + payload + syn + fin):
    tcpdump's "seq A:B" form already reports B = A + payload for a data
    segment, and a bare "seq A" (no colon) only appears for a zero-payload
    SYN/FIN, where end_seq = A + 1.

    IPv4 and IPv6 lines have different shapes (see _TCPDUMP_V6_LINE_RE's doc
    comment) but converge on the same `tos` key -- IPv6's traffic-class byte
    is bit-for-bit the same DSCP+ECN layout as IPv4's ToS byte, so every
    downstream consumer of this function (retransmit_capture_precision/
    retransmit_capture_client_arrivals) is already address-family-agnostic
    without needing to know which family produced a given packet dict.
    """
    packets = []
    pending_tos = None
    for line in text.splitlines():
        header = _TCPDUMP_HEADER_RE.match(line)
        if header:
            pending_tos = int(header.group(1), 16)
            continue
        v6_line = _TCPDUMP_V6_LINE_RE.match(line)
        if v6_line:
            class_hex, src, sport, dst, dport = v6_line.groups()
            tos = int(class_hex, 16) if class_hex is not None else 0
            packet = _tcpdump_packet_from_detail_line(line, tos, src, sport, dst, dport)
            if packet:
                packets.append(packet)
            continue
        if pending_tos is None or not line[:1].isspace():
            continue
        tos = pending_tos
        pending_tos = None
        endpoints = _TCPDUMP_ENDPOINTS_RE.match(line)
        if not endpoints:
            continue
        src, sport, dst, dport = endpoints.groups()
        packet = _tcpdump_packet_from_detail_line(line, tos, src, sport, dst, dport)
        if packet:
            packets.append(packet)
    return packets


def _tcpdump_packet_from_detail_line(line, tos, src, sport, dst, dport):
    """Shared seq/flags extraction for both the IPv4 (indented detail line)
    and IPv6 (single combined line) shapes in parse_tcpdump_capture() --
    once `line` is known to hold the Flags/seq detail and `tos`/endpoints
    are already resolved by the caller, the rest is address-family-agnostic
    text tcpdump prints identically either way. Returns None for a pure
    ACK/zero-window probe (tcpdump omits `seq` entirely for those)."""
    range_match = _TCPDUMP_SEQ_RANGE_RE.search(line)
    if range_match:
        seq, end_seq = int(range_match.group(1)), int(range_match.group(2))
    else:
        single_match = _TCPDUMP_SEQ_SINGLE_RE.search(line)
        if not single_match:
            return None  # pure ACK/window probe -- tcpdump omitted seq entirely
        seq = int(single_match.group(1))
        end_seq = seq + 1  # bare SYN or FIN, no data
    flags_match = _TCPDUMP_FLAGS_RE.search(line)
    return {
        "tos": tos,
        "seq": seq,
        "end_seq": end_seq,
        "src": src,
        "sport": sport,
        "dst": dst,
        "dport": dport,
        "flags": flags_match.group(1) if flags_match else "",
    }


def tcpdump_packets_dropped(text):
    """`stderr` (redirected into the same file by run_matrix.py) prints
    tcpdump's own "N packets dropped by kernel" summary on exit -- nonzero
    means the capture itself is suspect, which the caller should treat as
    an infrastructure issue (mark the case invalid), not evidence of a
    marking bug."""
    match = _TCPDUMP_DROPPED_RE.search(text)
    return int(match.group(1)) if match else 0


def _signed32(value):
    value &= 0xFFFFFFFF
    return value - 0x1_0000_0000 if value >= 0x8000_0000 else value


def retransmit_capture_precision(text, dscp_value):
    """Server-side (sender) capture only -- the sender sees every original
    transmission AND every retransmission, so "was this byte range already
    sent on this flow" is exactly answerable here. (Reconstructing the same
    question from a RECEIVER-side capture in a lossy scenario is wrong: a
    dropped original transmission never reaches the receiver, so the
    retransmission that follows it would look like brand-new data and get
    flagged as a false positive at roughly the loss rate -- this function
    must only ever be called on the sender-side artifact.)

    Tracks each flow's (4-tuple) own maximum byte-sequence-number-sent
    watermark across the capture, independently re-deriving "is this a
    retransmit" the same way bpf/skyline_tc.bpf.c does (wraparound-safe signed
    comparison against the flow's own prior traffic) -- NOT by trusting the
    BPF program's own stats map, which is exactly what this oracle exists
    to cross-check.
    """
    candidates = 0
    marked = 0
    false_positives = 0
    max_seq_sent = {}
    for packet in parse_tcpdump_capture(text):
        key = (packet["src"], packet["sport"], packet["dst"], packet["dport"])
        watermark = max_seq_sent.get(key)
        is_retransmit = watermark is not None and _signed32(packet["end_seq"] - watermark) <= 0
        is_marked = bool(dscp_value) and (packet["tos"] >> 2) == dscp_value
        if is_retransmit:
            candidates += 1
        if is_marked:
            marked += 1
            if not is_retransmit:
                false_positives += 1
        if watermark is None or _signed32(packet["end_seq"] - watermark) > 0:
            max_seq_sent[key] = packet["end_seq"]
    return {
        "candidates": candidates,
        "marked": marked,
        "false_positives": false_positives,
    }


def retransmit_capture_client_arrivals(text, dscp_value):
    """Receiver-side capture: deliberately NOT a precision check (see
    retransmit_capture_precision's doc comment for why that would be wrong
    here) -- just confirms marked packets survive the path unchanged
    (count > 0 when the sender marked anything, i.e. DSCP isn't being
    stripped somewhere between server and client)."""
    marked = 0
    for packet in parse_tcpdump_capture(text):
        if bool(dscp_value) and (packet["tos"] >> 2) == dscp_value:
            marked += 1
    return marked


def load_run(run_dir):
    metadata = json.loads((run_dir / "metadata.json").read_text(encoding="utf-8"))
    status = json.loads((run_dir / "status.json").read_text(encoding="utf-8"))
    profile = metadata["profile"]
    scenario = metadata["scenario"]
    row = {
        "case_id": metadata["case_id"],
        "attempt": int(metadata.get("attempt", 1)),
        "valid": bool(status["valid"]),
        "failure_class": status.get("failure_class") or "",
        "execution_class": metadata.get("execution_class", "unknown"),
        "performance_valid": bool(metadata.get("performance_valid", True)),
        "profile_id": profile["id"],
        "scenario_id": scenario["id"],
        "modules": ",".join(profile["modules"]),
        "rtt_ms": scenario["rtt_ms"],
        "rate_mbit": scenario["rate_mbit"],
        "queue_bdp": scenario["queue_bdp"],
        "loss_model": scenario["loss_model"],
        "loss_pct": scenario["loss_pct"],
        "loss_direction": scenario["loss_direction"],
        "parallel": scenario["parallel"],
        "offload": scenario.get("offload", "on"),
        "address_family": scenario.get("address_family", "v4"),
        "run_index": metadata["run_index"],
        "seed": metadata["seed"],
        "goodput_mbps": "",
        "sender_mbps": "",
        "retransmits": "",
        "mean_rtt_ms": "",
        "p95_sampled_rtt_ms": "",
        "max_cwnd_packets": "",
        "sender_cpu_pct": "",
        "receiver_cpu_pct": "",
        "kernel_health": "unknown",
        "rack_rto_enabled": bool(
            (profile.get("rack_rto") or {}).get("enabled")
        ),
        "rack_rto_applied": "",
        "rack_rto_rejected": "",
        "median_rto_ms": "",
        "max_rto_ms": "",
        # TCP_RTO_MAX_MS ceiling half (since v2 of struct skyline_rto_tuning) is
        # independently toggled from the floor's `enabled` above -- 0 in
        # either permille field means this half never touches the socket
        # regardless of `enabled`.
        "rack_rto_max_enabled": bool(
            (profile.get("rack_rto") or {}).get("rto_max_normal_permille")
        ),
        "rack_rto_max_applied": "",
        "rack_rto_max_rejected": "",
        "rack_rto_max_congested": "",
        "metrics_ack_events": "",
        "metrics_loss_events": "",
        "metrics_state_transitions": "",
        "metrics_pacing_updates": "",
        "metrics_guardrail_hits": "",
        "metrics_hypothetical_early_loss": "",
        "metrics_prr_adjustments": "",
        "n_segments": len(scenario.get("segments") or []),
        "segment_goodput_json": "",
        "retransmit_dscp_enabled": bool(
            (profile.get("retransmit_dscp") or {}).get("enabled")
        ),
        "retransmit_dscp_value": (profile.get("retransmit_dscp") or {}).get(
            "dscp_value", 0
        ),
        "retransmit_packets_seen": "",
        "retransmit_detected": "",
        "retransmit_marked": "",
        "retransmit_csum_fixups": "",
        "retransmit_abi_mismatch": "",
        "retransmit_ipv6_marked": "",
        "retransmit_ipv6_chain_bailout": "",
        "retransmit_capture_candidates": "",
        "retransmit_capture_marked": "",
        "retransmit_capture_false_positives": "",
        "retransmit_capture_dropped_server": "",
        "retransmit_capture_client_arrived": "",
        "retransmit_capture_dropped_client": "",
        "error": status.get("error") or "",
    }
    if not status["valid"]:
        return row
    iperf_path = run_dir / "iperf-client.json"
    try:
        iperf = parse_iperf(json.loads(iperf_path.read_text(encoding="utf-8")))
    except (OSError, json.JSONDecodeError, ValueError) as error:
        row["valid"] = False
        row["error"] = f"invalid iperf JSON: {error}"
        return row
    row.update(iperf)
    if row["n_segments"]:
        row["segment_goodput_json"] = json.dumps(segment_goodput(run_dir, scenario))
    kernel_log_path = run_dir / "kernel-log.txt"
    kernel_log = (
        kernel_log_path.read_text(encoding="utf-8", errors="replace")
        if kernel_log_path.is_file()
        else ""
    )
    kernel_issue = KERNEL_HEALTH_PATTERN.search(kernel_log)
    row["kernel_health"] = "failed" if kernel_issue else "ok"
    if kernel_issue:
        row["valid"] = False
        row["error"] = f"kernel health marker detected: {kernel_issue.group(0)}"
        return row
    sampled_rtts, cwnds, rtos = parse_ss_samples(run_dir / "ss-metrics.jsonl")
    row["p95_sampled_rtt_ms"] = percentile(sampled_rtts, 95)
    row["max_cwnd_packets"] = max(cwnds, default=0)
    row["median_rto_ms"] = statistics.median(rtos) if rtos else ""
    row["max_rto_ms"] = max(rtos, default="")
    rack_rto_stats = rack_rto_stats_delta(run_dir)
    rack_rto_applied = rack_rto_stats["applied"]
    row["rack_rto_applied"] = rack_rto_applied
    row["rack_rto_rejected"] = rack_rto_stats["rejected"]
    rack_rto_max_applied = rack_rto_stats["rto_max_applied"]
    row["rack_rto_max_applied"] = rack_rto_max_applied
    row["rack_rto_max_rejected"] = rack_rto_stats["rto_max_rejected"]
    row["rack_rto_max_congested"] = rack_rto_stats["rto_max_congested"]
    metrics_delta = skyline_metrics_delta(run_dir)
    row["metrics_ack_events"] = metrics_delta["ack_events"]
    row["metrics_loss_events"] = metrics_delta["loss_events"]
    row["metrics_state_transitions"] = metrics_delta["state_transitions"]
    row["metrics_pacing_updates"] = metrics_delta["pacing_updates"]
    row["metrics_guardrail_hits"] = metrics_delta["guardrail_hits"]
    row["metrics_hypothetical_early_loss"] = metrics_delta["hypothetical_early_loss"]
    row["metrics_prr_adjustments"] = metrics_delta["prr_adjustments"]
    if row["rack_rto_enabled"] and rack_rto_applied == 0:
        # M1 tier-2 was declared on for this case but never actually fired --
        # cgroup membership, RTT_CB subscription, or the setsockopt path are
        # the candidate root causes (see infra/run-in-skyline-cgroup.sh and
        # bpf/skyline_policy.bpf.c). A silent zero-effect run must not be read as
        # "the tuning had no effect"; it must be excluded and investigated.
        row["valid"] = False
        row["error"] = "rack_rto enabled but rack_rto_applied == 0"
    elif row["rack_rto_max_enabled"] and rack_rto_max_applied == 0:
        # Same guard for the independent TCP_RTO_MAX_MS ceiling half -- it
        # can be configured on while the floor half above is off (or vice
        # versa), so this must not piggyback on rack_rto_applied.
        row["valid"] = False
        row["error"] = "rack_rto rto_max enabled but rack_rto_max_applied == 0"
    retransmit_stats = retransmit_dscp_stats_delta(run_dir)
    row["retransmit_packets_seen"] = retransmit_stats["packets_seen"]
    row["retransmit_detected"] = retransmit_stats["retransmits_detected"]
    row["retransmit_marked"] = retransmit_stats["retransmits_marked"]
    row["retransmit_csum_fixups"] = retransmit_stats["csum_fixups"]
    row["retransmit_abi_mismatch"] = retransmit_stats["abi_mismatch"]
    row["retransmit_ipv6_marked"] = retransmit_stats["ipv6_marked"]
    row["retransmit_ipv6_chain_bailout"] = retransmit_stats["ipv6_chain_bailout"]
    if scenario.get("capture_retransmits"):
        dscp_value = row["retransmit_dscp_value"]
        server_capture_path = run_dir / "retransmit-capture-server.txt"
        client_capture_path = run_dir / "retransmit-capture-client.txt"
        capture_errors = []
        if server_capture_path.is_file():
            server_text = server_capture_path.read_text(encoding="utf-8", errors="replace")
            precision = retransmit_capture_precision(server_text, dscp_value)
            row["retransmit_capture_candidates"] = precision["candidates"]
            row["retransmit_capture_marked"] = precision["marked"]
            row["retransmit_capture_false_positives"] = precision["false_positives"]
            dropped = tcpdump_packets_dropped(server_text)
            row["retransmit_capture_dropped_server"] = dropped
            if dropped:
                capture_errors.append(
                    f"sender capture dropped {dropped} packets (infra, not a marking bug)"
                )
            if precision["false_positives"]:
                # Hard gate, not advisory -- this is the actual safety
                # property the feature exists to guarantee (never mark a
                # packet that wasn't a genuine retransmission).
                capture_errors.append(
                    f"{precision['false_positives']} packets marked that were "
                    "not retransmissions (false positive)"
                )
        else:
            capture_errors.append("capture_retransmits set but no sender capture artifact")
        if client_capture_path.is_file():
            client_text = client_capture_path.read_text(encoding="utf-8", errors="replace")
            row["retransmit_capture_client_arrived"] = retransmit_capture_client_arrivals(
                client_text, dscp_value
            )
            dropped_client = tcpdump_packets_dropped(client_text)
            row["retransmit_capture_dropped_client"] = dropped_client
            if dropped_client:
                capture_errors.append(
                    f"receiver capture dropped {dropped_client} packets (infra, not a marking bug)"
                )
        if capture_errors:
            row["valid"] = False
            row["error"] = "; ".join(capture_errors)
    return row


def load_runs(root):
    latest = {}
    for metadata_path in sorted(root.glob("*/metadata.json")):
        run_dir = metadata_path.parent
        if not (run_dir / "status.json").is_file():
            continue
        row = load_run(run_dir)
        previous = latest.get(row["case_id"])
        if previous is None or row["attempt"] > previous["attempt"]:
            latest[row["case_id"]] = row
    return [latest[case_id] for case_id in sorted(latest)]


def coefficient_of_variation(values):
    if len(values) < 2:
        return 0.0
    average = statistics.mean(values)
    return statistics.stdev(values) / average if average else float("inf")


# Default profile-id references used to compute "effect_vs_<key>_pct"
# columns. Keyed (not positional) so a manifest with different reference
# profile ids can override via --reference-profiles without disturbing the
# b0/b1/b2 column names that write_report()'s parity gate depends on.
DEFAULT_REFERENCE_PROFILES = {
    "b0": "b0-stock-cubic",
    "b1": "b1-controlled-cubic",
    "b2": "b2-skyline-base",
}


def summarize(rows, performance_valid=True, reference_profiles=None):
    if reference_profiles is None:
        reference_profiles = DEFAULT_REFERENCE_PROFILES
    groups = defaultdict(list)
    for row in rows:
        if row["valid"]:
            groups[(row["profile_id"], row["scenario_id"])].append(row)
    summary = []
    means = {}
    for (profile, scenario), items in sorted(groups.items()):
        goodputs = [float(item["goodput_mbps"]) for item in items]
        rtts = [float(item["p95_sampled_rtt_ms"]) for item in items]
        rtts = [value for value in rtts if math.isfinite(value)]
        ci_low, ci_high = bootstrap_mean_ci(goodputs)
        loss_pct = float(items[0]["loss_pct"])
        cv = coefficient_of_variation(goodputs)
        cv_limit = 0.05 if loss_pct == 0 else 0.15
        record = {
            "profile_id": profile,
            "scenario_id": scenario,
            "valid_runs": len(items),
            "mean_goodput_mbps": statistics.mean(goodputs),
            "median_goodput_mbps": statistics.median(goodputs),
            "stdev_goodput_mbps": statistics.stdev(goodputs) if len(goodputs) > 1 else 0,
            "cv_goodput": cv,
            "cv_valid": cv <= cv_limit if performance_valid else None,
            "performance_gate_applied": performance_valid,
            "ci95_low_mbps": ci_low,
            "ci95_high_mbps": ci_high,
            "mean_retransmits": statistics.mean(
                float(item["retransmits"]) for item in items
            ),
            "p95_sampled_rtt_ms": statistics.mean(rtts) if rtts else float("nan"),
            "mean_sender_cpu_pct": statistics.mean(
                float(item["sender_cpu_pct"]) for item in items
            ),
        }
        for key in reference_profiles:
            record[f"effect_vs_{key}_pct"] = float("nan")
        means[(profile, scenario)] = record["mean_goodput_mbps"]
        summary.append(record)
    for record in summary:
        for key, reference_profile in reference_profiles.items():
            reference = means.get((reference_profile, record["scenario_id"]))
            if reference:
                record[f"effect_vs_{key}_pct"] = (
                    record["mean_goodput_mbps"] / reference - 1
                ) * 100
    return summary


def write_csv(path, fieldnames, rows):
    with path.open("x", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)


def format_number(value, digits=2):
    if isinstance(value, float) and math.isnan(value):
        return "n/a"
    return f"{value:.{digits}f}"


# Cosmetic-only: lets report prose read "B1"/"B2" (as it always has for the
# Core/Broad/Supplemental/RACK/BBR campaigns) instead of the full profile id,
# while still falling back to the id itself for a sweep campaign's
# non-standard reference profiles (e.g. "sw-base").
_KNOWN_PROFILE_SHORTHAND = {
    "b0-stock-cubic": "B0",
    "b1-controlled-cubic": "B1",
    "b2-skyline-base": "B2",
}


def _profile_shorthand(profile_id):
    return _KNOWN_PROFILE_SHORTHAND.get(profile_id, profile_id)


def write_report(
    path,
    rows,
    summary,
    execution_class="formal-kvm",
    performance_valid=True,
    parity_scenario="line-rate",
    parity_candidate_profile="b2-skyline-base",
    parity_reference_profile="b1-controlled-cubic",
    table_effect_key="b1",
):
    invalid = [row for row in rows if not row["valid"]]
    cv_failures = [item for item in summary if item["cv_valid"] is False]
    parity = next(
        (
            item
            for item in summary
            if item["profile_id"] == parity_candidate_profile
            and item["scenario_id"] == parity_scenario
        ),
        None,
    )
    parity_reference = next(
        (
            item
            for item in summary
            if item["profile_id"] == parity_reference_profile
            and item["scenario_id"] == parity_scenario
        ),
        None,
    )
    table_effect_field = f"effect_vs_{table_effect_key}_pct"
    lines = [
        "# Skyline Speeder 实验统计报告",
        "",
        f"- 执行类别：`{execution_class}`",
        f"- 性能结论有效：`{str(performance_valid).lower()}`",
        f"- 总运行数：{len(rows)}",
        f"- 有效运行数：{len(rows) - len(invalid)}",
        f"- 无效运行数：{len(invalid)}",
    ]
    if performance_valid:
        lines.extend(
            [
                f"- 变异系数超限组：{len(cv_failures)}",
                "",
                "## B1/B2 中性底座门槛",
                "",
            ]
        )
    else:
        lines.extend(
            [
                "",
                "> 本结果来自 QEMU TCG 集成验证，不代表 KVM、裸机或生产环境性能。",
                "> Goodput、RTT、CPU、置信区间和相对差异仅作诊断展示，未应用性能门槛。",
                "",
                "## 集成有效性",
                "",
                "集成验证通过。" if not invalid else "存在无效运行，集成验证未完全通过。",
                "",
            ]
        )
        if invalid:
            failure_counts = defaultdict(int)
            for row in invalid:
                failure_counts[row["failure_class"] or "UNKNOWN"] += 1
            lines.append(
                "失败分类："
                + "、".join(
                    f"{name}={count}"
                    for name, count in sorted(failure_counts.items())
                )
                + "。"
            )
            lines.append("")

    if performance_valid and parity and parity_reference:
        effect = parity.get(table_effect_field, float("nan"))
        reference_rtt = parity_reference["p95_sampled_rtt_ms"]
        parity_rtt = parity["p95_sampled_rtt_ms"]
        rtt_effect = (
            (parity_rtt / reference_rtt - 1) * 100
            if reference_rtt and math.isfinite(reference_rtt)
            else float("nan")
        )
        candidate_label = _profile_shorthand(parity_candidate_profile)
        reference_label = _profile_shorthand(parity_reference_profile)
        scenario_label = (
            "线速" if parity_scenario == "line-rate" else parity_scenario
        )
        lines.append(
            f"{candidate_label} 在{scenario_label}场景相对 {reference_label} 的 "
            f"goodput 差异为 {format_number(effect)}%，"
            f"P95 RTT 差异为 {format_number(rtt_effect)}%。"
        )
        parity_ok = (
            math.isfinite(effect)
            and abs(effect) <= 3
            and math.isfinite(rtt_effect)
            and abs(rtt_effect) <= 10
        )
        lines.append(
            "通过 goodput ±3% 与 P95 RTT ±10% 门槛。"
            if parity_ok
            else "未通过中性底座门槛，停止解释可选模块结果。"
        )
    elif performance_valid:
        candidate_label = _profile_shorthand(parity_candidate_profile)
        reference_label = _profile_shorthand(parity_reference_profile)
        lines.append(
            f"缺少 {parity_scenario} 场景的 {reference_label}/{candidate_label} 有效数据。"
        )
    lines.extend(
        [
            "",
            "## 分组结果",
            "",
            f"| Profile | Scenario | n | Goodput mean | 95% CI | CV | vs {table_effect_key.upper()} | P95 RTT |",
            "|---|---|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for item in summary:
        lines.append(
            "| {profile_id} | {scenario_id} | {valid_runs} | {mean} Mbit/s | "
            "[{low}, {high}] | {cv}% | {effect}% | {rtt} ms |".format(
                profile_id=item["profile_id"],
                scenario_id=item["scenario_id"],
                valid_runs=item["valid_runs"],
                mean=format_number(item["mean_goodput_mbps"]),
                low=format_number(item["ci95_low_mbps"]),
                high=format_number(item["ci95_high_mbps"]),
                cv=(
                    format_number(item["cv_goodput"] * 100)
                    if performance_valid
                    else "diagnostic"
                ),
                effect=format_number(item.get(table_effect_field, float("nan"))),
                rtt=format_number(item["p95_sampled_rtt_ms"]),
            )
        )
    path.write_text("\n".join(lines), encoding="utf-8")


def _parse_reference_profiles(pairs):
    """Parses ["b0=custom-id", "b1=other-id"] into a dict, for manifests
    whose reference profiles aren't the default B0/B1/B2. Returns None
    (i.e. "use the built-in b0/b1/b2 default") when no --reference-profiles
    were given.
    """
    if not pairs:
        return None
    references = {}
    for pair in pairs:
        key, separator, profile_id = pair.partition("=")
        if not separator:
            raise SystemExit(
                f"--reference-profiles entries must be key=profile_id, got {pair!r}"
            )
        references[key] = profile_id
    return references


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("results_dir")
    parser.add_argument("output_dir")
    parser.add_argument(
        "--parity-scenario",
        default="line-rate",
        help="scenario id used for the B1/B2 neutral-baseline gate (default: line-rate)",
    )
    parser.add_argument(
        "--parity-candidate",
        default="b2-skyline-base",
        help="profile id checked against --parity-reference for the neutral-baseline "
        "gate (default: b2-skyline-base)",
    )
    parser.add_argument(
        "--parity-reference",
        default="b1-controlled-cubic",
        help="profile id the --parity-candidate is compared against (default: "
        "b1-controlled-cubic)",
    )
    parser.add_argument(
        "--reference-profiles",
        nargs="*",
        metavar="KEY=PROFILE_ID",
        help="override the effect_vs_<key>_pct reference profiles (default: "
        "b0=b0-stock-cubic b1=b1-controlled-cubic b2=b2-skyline-base). The "
        "--parity-* flags' key must still be present here if you rely on "
        "the summary table's 'vs <key>' column matching the parity gate.",
    )
    args = parser.parse_args()
    results_dir = Path(args.results_dir)
    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=False)
    rows = load_runs(results_dir)
    if not rows:
        raise SystemExit("No completed run directories were found")
    campaign_path = results_dir / "campaign.json"
    if campaign_path.is_file():
        campaign = json.loads(campaign_path.read_text(encoding="utf-8"))
        execution_class = campaign.get("execution_class", "unknown")
        performance_valid = bool(campaign.get("performance_valid", False))
    else:
        execution_classes = {row["execution_class"] for row in rows}
        performance_flags = {row["performance_valid"] for row in rows}
        execution_class = (
            next(iter(execution_classes))
            if len(execution_classes) == 1
            else "mixed"
        )
        performance_valid = performance_flags == {True}
    reference_profiles = _parse_reference_profiles(args.reference_profiles)
    summary = summarize(
        rows, performance_valid=performance_valid, reference_profiles=reference_profiles
    )
    write_csv(output_dir / "runs.csv", RUN_FIELDS, rows)
    summary_fields = list(summary[0]) if summary else []
    write_csv(output_dir / "summary.csv", summary_fields, summary)
    write_report(
        output_dir / "report.md",
        rows,
        summary,
        execution_class=execution_class,
        performance_valid=performance_valid,
        parity_scenario=args.parity_scenario,
        parity_candidate_profile=args.parity_candidate,
        parity_reference_profile=args.parity_reference,
    )
    print(f"analyzed {len(rows)} runs into {output_dir}")


if __name__ == "__main__":
    main()
