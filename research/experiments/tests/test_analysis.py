import json
import math
import sys
import tempfile
import unittest
from pathlib import Path

EXPERIMENT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXPERIMENT_DIR))

from analyze_results import (
    bootstrap_mean_ci,
    load_run,
    load_runs,
    parse_iperf,
    parse_tcpdump_capture,
    percentile,
    retransmit_capture_client_arrivals,
    retransmit_capture_precision,
    retransmit_dscp_stats_delta,
    segment_goodput,
    summarize,
    tcpdump_packets_dropped,
    write_report,
)
from analyze_results import DEFAULT_REFERENCE_PROFILES


class AnalysisTests(unittest.TestCase):
    def test_parse_reverse_iperf(self):
        payload = {
            "end": {
                "sum_received": {"bits_per_second": 500_000_000},
                "sum_sent": {
                    "bits_per_second": 510_000_000,
                    "retransmits": 12,
                },
                "streams": [{"sender": {"mean_rtt": 150_000}}],
                "cpu_utilization_percent": {
                    "host_total": 3.0,
                    "remote_total": 8.0,
                },
            }
        }
        parsed = parse_iperf(payload)
        self.assertEqual(parsed["goodput_mbps"], 500)
        self.assertEqual(parsed["mean_rtt_ms"], 150)
        self.assertEqual(parsed["sender_cpu_pct"], 8)

    def test_bootstrap_is_deterministic(self):
        first = bootstrap_mean_ci([1, 2, 3, 4, 5], samples=100)
        second = bootstrap_mean_ci([1, 2, 3, 4, 5], samples=100)
        self.assertEqual(first, second)

    def test_percentile_empty(self):
        self.assertTrue(math.isnan(percentile([], 95)))

    def test_tcg_summary_disables_performance_gates(self):
        rows = [
            {
                "valid": True,
                "profile_id": "b1-controlled-cubic",
                "scenario_id": "clean",
                "goodput_mbps": 10.0,
                "p95_sampled_rtt_ms": 20.0,
                "loss_pct": 0.0,
                "retransmits": 0,
                "sender_cpu_pct": 100.0,
            }
        ]
        summary = summarize(rows, performance_valid=False)
        self.assertIsNone(summary[0]["cv_valid"])
        self.assertFalse(summary[0]["performance_gate_applied"])
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.md"
            write_report(
                report,
                rows,
                summary,
                execution_class="tcg-validation",
                performance_valid=False,
            )
            text = report.read_text(encoding="utf-8")
            self.assertIn("do not represent KVM, bare-metal or production performance", text)
            self.assertNotIn("B1/B2 neutral baseline gate", text)

    def test_load_runs_selects_latest_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for attempt, valid in ((1, False), (2, True)):
                run_dir = root / f"case--attempt{attempt:02d}"
                run_dir.mkdir()
                metadata = {
                    "case_id": "case",
                    "attempt": attempt,
                    "execution_class": "tcg-validation",
                    "performance_valid": False,
                    "profile": {"id": "b1", "modules": []},
                    "scenario": {
                        "id": "clean",
                        "rtt_ms": 20,
                        "rate_mbit": 20,
                        "queue_bdp": 1.0,
                        "loss_model": "random",
                        "loss_pct": 0.0,
                        "loss_direction": "data",
                        "parallel": 1,
                    },
                    "run_index": 1,
                    "seed": 1,
                }
                (run_dir / "metadata.json").write_text(json.dumps(metadata))
                (run_dir / "status.json").write_text(
                    json.dumps({"valid": valid, "error": None})
                )
                if valid:
                    (run_dir / "iperf-client.json").write_text(
                        json.dumps(
                            {
                                "end": {
                                    "sum_received": {"bits_per_second": 1},
                                    "sum_sent": {"bits_per_second": 1},
                                }
                            }
                        )
                    )
            rows = load_runs(root)
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0]["attempt"], 2)

    def test_segment_goodput_buckets_intervals_by_segment_boundary(self):
        scenario = {"segments": [{"offset_s": 10.0}, {"offset_s": 20.0}]}
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory)
            (run_dir / "segment-timeline.json").write_text(
                json.dumps(
                    [
                        {"requested_offset_s": 10.0, "label": "cliff"},
                        {"requested_offset_s": 20.0, "label": "recovery"},
                    ]
                )
            )
            (run_dir / "iperf-client.json").write_text(
                json.dumps(
                    {
                        "intervals": [
                            {"sum": {"start": 0.0, "end": 5.0, "bits_per_second": 100e6}},
                            {"sum": {"start": 5.0, "end": 10.0, "bits_per_second": 100e6}},
                            {"sum": {"start": 10.0, "end": 15.0, "bits_per_second": 10e6}},
                            {"sum": {"start": 15.0, "end": 20.0, "bits_per_second": 10e6}},
                            {"sum": {"start": 20.0, "end": 25.0, "bits_per_second": 90e6}},
                        ]
                    }
                )
            )
            result = segment_goodput(run_dir, scenario)
        by_index = {entry["segment_index"]: entry for entry in result}
        self.assertEqual(set(by_index), {0, 1, 2})
        self.assertAlmostEqual(by_index[0]["mean_goodput_mbps"], 100.0)
        self.assertEqual(by_index[0]["n_intervals"], 2)
        self.assertEqual(by_index[1]["label"], "cliff")
        self.assertAlmostEqual(by_index[1]["mean_goodput_mbps"], 10.0)
        self.assertEqual(by_index[2]["label"], "recovery")
        self.assertAlmostEqual(by_index[2]["mean_goodput_mbps"], 90.0)

    def test_segment_goodput_empty_without_segments_or_timeline(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory)
            self.assertEqual(segment_goodput(run_dir, {"segments": []}), [])
            self.assertEqual(segment_goodput(run_dir, {}), [])
            # Segments declared but no segment-timeline.json on disk (e.g.
            # the case failed before the schedule ever wrote one).
            self.assertEqual(
                segment_goodput(run_dir, {"segments": [{"offset_s": 10.0}]}), []
            )

    def test_load_run_reports_n_segments_and_segment_goodput(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory) / "case--attempt01"
            run_dir.mkdir()
            metadata = {
                "case_id": "case",
                "attempt": 1,
                "execution_class": "tcg-validation",
                "performance_valid": False,
                "profile": {"id": "skyline-best", "modules": []},
                "scenario": {
                    "id": "volatile",
                    "rtt_ms": 300,
                    "rate_mbit": 100,
                    "queue_bdp": 1.0,
                    "loss_model": "gemodel",
                    "loss_pct": 0.5,
                    "loss_direction": "data",
                    "parallel": 1,
                    "segments": [{"offset_s": 10.0, "label": "cliff"}],
                },
                "run_index": 1,
                "seed": 1,
            }
            (run_dir / "metadata.json").write_text(json.dumps(metadata))
            (run_dir / "status.json").write_text(json.dumps({"valid": True, "error": None}))
            (run_dir / "iperf-client.json").write_text(
                json.dumps(
                    {
                        "end": {
                            "sum_received": {"bits_per_second": 1_000_000},
                            "sum_sent": {"bits_per_second": 1_000_000},
                        },
                        "intervals": [
                            {"sum": {"start": 0.0, "end": 5.0, "bits_per_second": 50e6}},
                            {"sum": {"start": 10.0, "end": 15.0, "bits_per_second": 5e6}},
                        ],
                    }
                )
            )
            (run_dir / "segment-timeline.json").write_text(
                json.dumps([{"requested_offset_s": 10.0, "label": "cliff"}])
            )
            row = load_run(run_dir)
        self.assertEqual(row["n_segments"], 1)
        goodput = json.loads(row["segment_goodput_json"])
        self.assertEqual(len(goodput), 2)
        self.assertAlmostEqual(goodput[0]["mean_goodput_mbps"], 50.0)
        self.assertAlmostEqual(goodput[1]["mean_goodput_mbps"], 5.0)

    def test_load_run_non_segmented_case_leaves_segment_fields_empty(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory) / "case--attempt01"
            run_dir.mkdir()
            metadata = {
                "case_id": "case",
                "attempt": 1,
                "execution_class": "tcg-validation",
                "performance_valid": False,
                "profile": {"id": "b1", "modules": []},
                "scenario": {
                    "id": "clean",
                    "rtt_ms": 20,
                    "rate_mbit": 20,
                    "queue_bdp": 1.0,
                    "loss_model": "random",
                    "loss_pct": 0.0,
                    "loss_direction": "data",
                    "parallel": 1,
                },
                "run_index": 1,
                "seed": 1,
            }
            (run_dir / "metadata.json").write_text(json.dumps(metadata))
            (run_dir / "status.json").write_text(json.dumps({"valid": True, "error": None}))
            (run_dir / "iperf-client.json").write_text(
                json.dumps(
                    {
                        "end": {
                            "sum_received": {"bits_per_second": 1},
                            "sum_sent": {"bits_per_second": 1},
                        }
                    }
                )
            )
            row = load_run(run_dir)
        self.assertEqual(row["n_segments"], 0)
        self.assertEqual(row["segment_goodput_json"], "")

    # --- retransmit-DSCP oracles ---

    _CAPTURE_SAMPLE = (
        "08:17:31.423359 IP (tos 0x0, ttl 63, id 20540, offset 0, flags [DF], proto TCP (6), length 60)\n"
        "    10.20.2.2.44192 > 10.20.1.2.5201: Flags [S], seq 1000, win 42340, "
        "options [mss 1460], length 0\n"
        "08:17:31.423791 IP (tos 0x0, ttl 64, id 16511, offset 0, flags [DF], proto TCP (6), length 1500)\n"
        "    10.20.1.2.5201 > 10.20.2.2.44192: Flags [P.], seq 1001:2449, ack 1, win 502, "
        "options [nop,nop], length 1448\n"
        "08:17:31.500000 IP (tos 0x0, ttl 63, id 20541, offset 0, flags [DF], proto TCP (6), length 52)\n"
        "    10.20.2.2.44192 > 10.20.1.2.5201: Flags [.], ack 2449, win 21, "
        "options [nop,nop], length 0\n"
        "08:17:31.900000 IP (tos 0x28, ttl 64, id 16512, offset 0, flags [DF], proto TCP (6), length 1500)\n"
        "    10.20.1.2.5201 > 10.20.2.2.44192: Flags [P.], seq 1001:2449, ack 1, win 502, "
        "options [nop,nop], length 1448\n"
        "08:17:32.100000 IP (tos 0x28, ttl 64, id 16513, offset 0, flags [DF], proto TCP (6), length 1500)\n"
        "    10.20.1.2.5201 > 10.20.2.2.44192: Flags [P.], seq 5000:6448, ack 1, win 502, "
        "options [nop,nop], length 1448\n"
    )

    def test_parse_tcpdump_capture_extracts_seq_ranges_and_skips_pure_acks(self):
        packets = parse_tcpdump_capture(self._CAPTURE_SAMPLE)
        # SYN (bare seq -> end_seq=seq+1) + 3 data segments; the pure ACK
        # line (no seq at all) must be skipped entirely.
        self.assertEqual(len(packets), 4)
        syn = packets[0]
        self.assertEqual((syn["seq"], syn["end_seq"]), (1000, 1001))
        self.assertEqual(syn["tos"], 0x0)
        data = packets[1]
        self.assertEqual((data["seq"], data["end_seq"]), (1001, 2449))
        self.assertEqual((data["src"], data["sport"]), ("10.20.1.2", "5201"))
        marked_repeat = packets[2]
        self.assertEqual(marked_repeat["tos"], 0x28)
        self.assertEqual((marked_repeat["seq"], marked_repeat["end_seq"]), (1001, 2449))

    def test_retransmit_capture_precision_flags_true_retransmit_and_false_positive(self):
        # Segment 1001:2449 is sent twice (2nd time marked) -> a genuine
        # retransmit, correctly marked. Segment 5000:6448 is marked but was
        # NEVER sent before -> a false positive the oracle must catch.
        result = retransmit_capture_precision(self._CAPTURE_SAMPLE, dscp_value=10)
        self.assertEqual(result["candidates"], 1)
        self.assertEqual(result["marked"], 2)
        self.assertEqual(result["false_positives"], 1)

    def test_retransmit_capture_precision_no_false_positives_when_everything_matches(self):
        clean = self._CAPTURE_SAMPLE.replace(
            "seq 5000:6448", "seq 1001:2449"
        )  # the second "marked" packet is now also a genuine repeat
        result = retransmit_capture_precision(clean, dscp_value=10)
        self.assertEqual(result["marked"], 2)
        self.assertEqual(result["false_positives"], 0)

    def test_retransmit_capture_precision_zero_dscp_value_marks_nothing(self):
        # dscp_value=0 means the feature is unset/disabled -- must never be
        # treated as "everything at tos=0 counts as marked".
        result = retransmit_capture_precision(self._CAPTURE_SAMPLE, dscp_value=0)
        self.assertEqual(result["marked"], 0)
        self.assertEqual(result["false_positives"], 0)

    def test_retransmit_capture_client_arrivals_counts_matching_tos(self):
        count = retransmit_capture_client_arrivals(self._CAPTURE_SAMPLE, dscp_value=10)
        self.assertEqual(count, 2)

    def test_tcpdump_packets_dropped_parses_summary_line(self):
        text = self._CAPTURE_SAMPLE + "\n12 packets captured\n12 packets received by filter\n3 packets dropped by kernel\n"
        self.assertEqual(tcpdump_packets_dropped(text), 3)
        self.assertEqual(tcpdump_packets_dropped(self._CAPTURE_SAMPLE), 0)

    # --- IPv6 tcpdump format, built from a real capture (`iperf3 -6` over
    # fd20:1::2/fd20:2::2 with dscp_value=46 actually applied through skyline-speederd)
    # -- confirmed real quirks this fixture exercises: the whole packet is
    # ONE line (no separate indented detail
    # line like IPv4), the "next-header TCP (6)" clause has a nested paren
    # that the v4-derived regex initially choked on, "class 0x.." is
    # entirely absent (not "class 0x0,") when traffic class is 0, and a
    # pure ACK has no `seq` field at all -- same as IPv4.
    _CAPTURE_SAMPLE_V6 = (
        "11:32:30.090105 IP6 (flowlabel 0xa78bd, hlim 63, next-header TCP (6) payload length: 40) "
        "fd20:2::2.44192 > fd20:1::2.5201: Flags [S], seq 1000, win 43200, "
        "options [mss 1440,sackOK], length 0\n"
        "11:32:30.090189 IP6 (flowlabel 0xeb875, hlim 64, next-header TCP (6) payload length: 1460) "
        "fd20:1::2.5201 > fd20:2::2.44192: Flags [P.], seq 1001:2449, ack 1, win 21, "
        "options [nop,nop], length 1448\n"
        "11:32:30.090525 IP6 (flowlabel 0xa78bd, hlim 63, next-header TCP (6) payload length: 32) "
        "fd20:2::2.44192 > fd20:1::2.5201: Flags [.], ack 2449, win 22, "
        "options [nop,nop], length 0\n"
        "11:34:28.711158 IP6 (class 0xb8, flowlabel 0xba680, hlim 64, next-header TCP (6) "
        "payload length: 1460) fd20:1::2.5201 > fd20:2::2.44192: Flags [P.], seq 1001:2449, "
        "ack 1, win 21, options [nop,nop], length 1448\n"
        "11:34:28.717579 IP6 (class 0xb8, flowlabel 0xba680, hlim 64, next-header TCP (6) "
        "payload length: 1460) fd20:1::2.5201 > fd20:2::2.44192: Flags [P.], seq 5000:6448, "
        "ack 1, win 21, options [nop,nop], length 1448\n"
    )

    def test_parse_tcpdump_capture_handles_ipv6_single_line_format(self):
        packets = parse_tcpdump_capture(self._CAPTURE_SAMPLE_V6)
        # Same shape as the IPv4 fixture: SYN + 3 data segments, pure ACK
        # skipped -- but every line here is self-contained (no look-ahead
        # to an indented next line).
        self.assertEqual(len(packets), 4)
        syn = packets[0]
        self.assertEqual((syn["seq"], syn["end_seq"]), (1000, 1001))
        self.assertEqual(syn["tos"], 0x0)
        self.assertEqual((syn["src"], syn["sport"]), ("fd20:2::2", "44192"))
        data = packets[1]
        self.assertEqual((data["seq"], data["end_seq"]), (1001, 2449))
        self.assertEqual((data["dst"], data["dport"]), ("fd20:2::2", "44192"))
        self.assertEqual(data["tos"], 0x0)  # "class 0x.." absent -> 0, not a parse failure
        marked_repeat = packets[2]
        self.assertEqual(marked_repeat["tos"], 0xB8)  # dscp=46 -> 46<<2 == 0xb8, ECN=0

    def test_retransmit_capture_precision_flags_true_retransmit_and_false_positive_ipv6(self):
        # Mirrors the IPv4 false-positive test: 1001:2449 sent twice (2nd
        # marked) is a genuine retransmit; 5000:6448 marked but never sent
        # before is the false positive the oracle must still catch on IPv6.
        result = retransmit_capture_precision(self._CAPTURE_SAMPLE_V6, dscp_value=46)
        self.assertEqual(result["candidates"], 1)
        self.assertEqual(result["marked"], 2)
        self.assertEqual(result["false_positives"], 1)

    def test_retransmit_capture_precision_zero_dscp_value_marks_nothing_ipv6(self):
        result = retransmit_capture_precision(self._CAPTURE_SAMPLE_V6, dscp_value=0)
        self.assertEqual(result["marked"], 0)
        self.assertEqual(result["false_positives"], 0)

    def test_retransmit_capture_client_arrivals_counts_matching_tos_ipv6(self):
        count = retransmit_capture_client_arrivals(self._CAPTURE_SAMPLE_V6, dscp_value=46)
        self.assertEqual(count, 2)

    def test_retransmit_dscp_stats_delta_computes_before_after_difference(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory)
            before = {
                "status": {
                    "retransmit_dscp": {
                        "stats": {
                            "packets_seen": 100,
                            "retransmits_detected": 2,
                            "retransmits_marked": 2,
                            "csum_fixups": 2,
                            "abi_mismatch": 0,
                        }
                    }
                }
            }
            after = {
                "status": {
                    "retransmit_dscp": {
                        "stats": {
                            "packets_seen": 500,
                            "retransmits_detected": 9,
                            "retransmits_marked": 9,
                            "csum_fixups": 9,
                            "abi_mismatch": 0,
                        }
                    }
                }
            }
            (run_dir / "skyline-status-before.json").write_text(json.dumps(before))
            (run_dir / "skyline-status-after.json").write_text(json.dumps(after))
            delta = retransmit_dscp_stats_delta(run_dir)
        self.assertEqual(delta["packets_seen"], 400)
        self.assertEqual(delta["retransmits_detected"], 7)
        self.assertEqual(delta["retransmits_marked"], 7)

    def test_retransmit_dscp_stats_delta_missing_files_returns_zeros(self):
        with tempfile.TemporaryDirectory() as directory:
            delta = retransmit_dscp_stats_delta(Path(directory))
        self.assertEqual(delta["packets_seen"], 0)
        self.assertEqual(delta["retransmits_marked"], 0)

    def test_load_run_flags_capture_false_positive_as_invalid(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory) / "case--attempt01"
            run_dir.mkdir()
            metadata = {
                "case_id": "case",
                "attempt": 1,
                "execution_class": "tcg-validation",
                "performance_valid": False,
                "profile": {
                    "id": "bbr-fq",
                    "modules": [],
                    "retransmit_dscp": {"enabled": True, "dscp_value": 10},
                },
                "scenario": {
                    "id": "capture-smoke",
                    "rtt_ms": 20,
                    "rate_mbit": 20,
                    "queue_bdp": 1.0,
                    "loss_model": "random",
                    "loss_pct": 5.0,
                    "loss_direction": "data",
                    "parallel": 1,
                    "capture_retransmits": True,
                },
                "run_index": 1,
                "seed": 1,
            }
            (run_dir / "metadata.json").write_text(json.dumps(metadata))
            (run_dir / "status.json").write_text(json.dumps({"valid": True, "error": None}))
            (run_dir / "iperf-client.json").write_text(
                json.dumps(
                    {
                        "end": {
                            "sum_received": {"bits_per_second": 1_000_000},
                            "sum_sent": {"bits_per_second": 1_000_000},
                        }
                    }
                )
            )
            (run_dir / "retransmit-capture-server.txt").write_text(self._CAPTURE_SAMPLE)
            row = load_run(run_dir)
        self.assertFalse(row["valid"])
        self.assertIn("false positive", row["error"])
        self.assertEqual(row["retransmit_capture_false_positives"], 1)

    def test_load_run_capture_clean_case_stays_valid(self):
        with tempfile.TemporaryDirectory() as directory:
            run_dir = Path(directory) / "case--attempt01"
            run_dir.mkdir()
            clean_capture = self._CAPTURE_SAMPLE.replace("seq 5000:6448", "seq 1001:2449")
            metadata = {
                "case_id": "case",
                "attempt": 1,
                "execution_class": "tcg-validation",
                "performance_valid": False,
                "profile": {
                    "id": "bbr-fq",
                    "modules": [],
                    "retransmit_dscp": {"enabled": True, "dscp_value": 10},
                },
                "scenario": {
                    "id": "capture-smoke",
                    "rtt_ms": 20,
                    "rate_mbit": 20,
                    "queue_bdp": 1.0,
                    "loss_model": "random",
                    "loss_pct": 5.0,
                    "loss_direction": "data",
                    "parallel": 1,
                    "capture_retransmits": True,
                },
                "run_index": 1,
                "seed": 1,
            }
            (run_dir / "metadata.json").write_text(json.dumps(metadata))
            (run_dir / "status.json").write_text(json.dumps({"valid": True, "error": None}))
            (run_dir / "iperf-client.json").write_text(
                json.dumps(
                    {
                        "end": {
                            "sum_received": {"bits_per_second": 1_000_000},
                            "sum_sent": {"bits_per_second": 1_000_000},
                        }
                    }
                )
            )
            (run_dir / "retransmit-capture-server.txt").write_text(clean_capture)
            (run_dir / "retransmit-capture-client.txt").write_text(clean_capture)
            row = load_run(run_dir)
        self.assertTrue(row["valid"])
        self.assertEqual(row["retransmit_capture_false_positives"], 0)
        self.assertEqual(row["retransmit_capture_client_arrived"], 2)

    def _core_rows(self):
        return [
            {
                "valid": True,
                "profile_id": profile,
                "scenario_id": "line-rate",
                "goodput_mbps": goodput,
                "p95_sampled_rtt_ms": rtt,
                "loss_pct": 0.0,
                "retransmits": 0,
                "sender_cpu_pct": 5.0,
            }
            for profile, goodput, rtt in (
                ("b0-stock-cubic", 100.0, 20.0),
                ("b1-controlled-cubic", 100.0, 20.0),
                ("b2-skyline-base", 99.0, 21.0),
            )
        ]

    def test_default_reference_profiles_unchanged(self):
        # Locks in the exact default mapping analyze_results.py has always
        # used, so a future refactor of --reference-profiles can't silently
        # change what a plain `analyze_results.py results/ out/` run (no
        # extra flags) reports for Core/Broad/Supplemental/RACK/BBR
        # campaigns.
        self.assertEqual(
            DEFAULT_REFERENCE_PROFILES,
            {"b0": "b0-stock-cubic", "b1": "b1-controlled-cubic", "b2": "b2-skyline-base"},
        )

    def test_summarize_without_reference_profiles_matches_legacy_columns(self):
        summary = summarize(self._core_rows())
        b2 = next(item for item in summary if item["profile_id"] == "b2-skyline-base")
        self.assertIn("effect_vs_b0_pct", b2)
        self.assertIn("effect_vs_b1_pct", b2)
        self.assertIn("effect_vs_b2_pct", b2)
        self.assertAlmostEqual(b2["effect_vs_b1_pct"], -1.0, places=6)

    def test_summarize_with_custom_reference_profiles(self):
        rows = [
            {
                "valid": True,
                "profile_id": profile,
                "scenario_id": "primary",
                "goodput_mbps": goodput,
                "p95_sampled_rtt_ms": 20.0,
                "loss_pct": 0.5,
                "retransmits": 0,
                "sender_cpu_pct": 5.0,
            }
            for profile, goodput in (("sw-base", 50.0), ("sw-steady_inflight_gain-hi", 60.0))
        ]
        summary = summarize(rows, reference_profiles={"sw": "sw-base"})
        candidate = next(item for item in summary if item["profile_id"] == "sw-steady_inflight_gain-hi")
        self.assertIn("effect_vs_sw_pct", candidate)
        self.assertNotIn("effect_vs_b0_pct", candidate)
        self.assertAlmostEqual(candidate["effect_vs_sw_pct"], 20.0, places=6)

    def test_write_report_parity_gate_defaults_match_legacy_text(self):
        summary = summarize(self._core_rows())
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.md"
            write_report(report, self._core_rows(), summary)
            text = report.read_text(encoding="utf-8")
            self.assertIn("B2 relative to B1 in the line rate scenario:", text)
            self.assertIn("vs B1", text)

    def test_write_report_custom_parity_profiles(self):
        rows = [
            {
                "valid": True,
                "profile_id": profile,
                "scenario_id": "primary",
                "goodput_mbps": goodput,
                "p95_sampled_rtt_ms": 20.0,
                "loss_pct": 0.5,
                "retransmits": 0,
                "sender_cpu_pct": 5.0,
            }
            for profile, goodput in (("sw-base", 50.0), ("sw-base-rto", 51.0))
        ]
        summary = summarize(rows, reference_profiles={"sw": "sw-base"})
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.md"
            write_report(
                report,
                rows,
                summary,
                parity_scenario="primary",
                parity_candidate_profile="sw-base-rto",
                parity_reference_profile="sw-base",
                table_effect_key="sw",
            )
            text = report.read_text(encoding="utf-8")
            self.assertIn("sw-base-rto relative to sw-base in the primary scenario:", text)
            self.assertIn("vs SW", text)


if __name__ == "__main__":
    unittest.main()
