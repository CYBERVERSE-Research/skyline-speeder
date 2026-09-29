import sys
import tempfile
import unittest
from pathlib import Path

EXPERIMENT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXPERIMENT_DIR))

from matrix_lib import (
    Manifest,
    ModuleConfigSpec,
    Profile,
    RackRtoSpec,
    RetransmitDscpSpec,
    Scenario,
    Segment,
    bdp_packets,
    expand_runs,
    gilbert_elliott,
    load_manifest,
    validate_manifest,
)


def _minimal_scenario():
    return Scenario(
        id="only",
        rtt_ms=20,
        rate_mbit=100,
        queue_bdp=1.0,
        loss_model="random",
        loss_pct=0.0,
        loss_direction="data",
        burst_length=1,
        parallel=1,
        offload="on",
    )


def _minimal_manifest(profiles, scenarios=None):
    return Manifest(
        name="test",
        execution_class="formal-kvm",
        performance_valid=True,
        seed=1,
        warmup_s=1,
        duration_s=1,
        runs=1,
        stock_qdisc="fq_codel",
        server_ssh="skyline@127.0.0.1:2222",
        client_ssh="skyline@127.0.0.1:2223",
        server_data_ip="10.20.1.2",
        server_data_interface="data0",
        profiles=tuple(profiles),
        scenarios=tuple(scenarios) if scenarios is not None else (_minimal_scenario(),),
    )


def _base_profile(**overrides):
    fields = dict(id="p", kind="controlled-cubic", cc="cubic", modules=(), tcp_recovery=1)
    fields.update(overrides)
    return Profile(**fields)


def _base_scenario(**overrides):
    fields = dict(
        id="only",
        rtt_ms=20,
        rate_mbit=100,
        queue_bdp=1.0,
        loss_model="random",
        loss_pct=0.0,
        loss_direction="data",
        burst_length=1,
        parallel=1,
        offload="on",
    )
    fields.update(overrides)
    return Scenario(**fields)


def _base_segment(**overrides):
    fields = dict(
        offset_s=20.0,
        rtt_ms=300,
        rate_mbit=15,
        loss_model="gemodel",
        loss_pct=8.0,
        loss_direction="data",
        burst_length=220,
    )
    fields.update(overrides)
    return Segment(**fields)


def _manifest_with_segments(scenario, warmup_s=5, duration_s=60):
    return Manifest(
        name="test",
        execution_class="formal-kvm",
        performance_valid=True,
        seed=1,
        warmup_s=warmup_s,
        duration_s=duration_s,
        runs=1,
        stock_qdisc="fq_codel",
        server_ssh="skyline@127.0.0.1:2222",
        client_ssh="skyline@127.0.0.1:2223",
        server_data_ip="10.20.1.2",
        server_data_interface="data0",
        profiles=(_base_profile(),),
        scenarios=(scenario,),
    )


def _base_module_config(**overrides):
    # Mirrors config/speeder.toml's current defaults.
    fields = dict(
        max_pacing_mbps=1000,
        max_cwnd_packets=20_000,
        max_queue_delay_ms=100,
        max_queue_delay_ratio=0.0,
        initial_cwnd_packets=100,
        min_rtt_window_s=10,
        bw_window_rtts=10,
        startup_plateau_rtts=3,
        startup_growth_ratio=0.25,
        startup_gain=3.0,
        cruise_inflight_gain=1.25,
        cruise_pacing_gain=1.05,
        guardrail_gain=0.0,
        loss_inflation_max_ratio=0.0,
    )
    fields.update(overrides)
    return ModuleConfigSpec(**fields)


class MatrixTests(unittest.TestCase):
    def test_all_manifests_use_stable_data_interface_name(self):
        for path in sorted((EXPERIMENT_DIR / "manifests").glob("*.toml")):
            with self.subTest(manifest=path.name):
                manifest = load_manifest(path)
                self.assertEqual(manifest.server_data_interface, "data0")
                if manifest.execution_class == "tcg-validation":
                    self.assertFalse(manifest.performance_valid)
                else:
                    self.assertEqual(manifest.execution_class, "formal-kvm")
                    self.assertTrue(manifest.performance_valid)

    def test_target_box_family_manifests_expand_to_expected_case_counts(self):
        expected = {
            "target-box.toml": 90,
            "target-box-rtt300-samples.toml": 75,
            "bandwidth-sweep.toml": 40,
            "neutrality.toml": 24,
            "guardrail-gain-ab.toml": 105,
            "rto-max-validation.toml": 12,
        }
        for name, count in expected.items():
            manifest = load_manifest(EXPERIMENT_DIR / "manifests" / name)
            validate_manifest(manifest)
            self.assertEqual(len(expand_runs(manifest)), count, name)
        self.assertEqual(sum(expected.values()), 346)

    def test_volatile_link_manifest_expands_to_fifteen_cases(self):
        """Only manifest using Segment (mid-case live network-condition
        transitions) -- see the file's own header comment for the full
        design rationale (queueing built cleanly first, then a loss burst
        layered on top of already-established evidence, then recovery).
        """
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/volatile-link.toml"
        )
        validate_manifest(manifest)
        self.assertEqual(len(expand_runs(manifest)), 15)
        (scenario,) = manifest.scenarios
        self.assertEqual(len(scenario.segments), 3)
        self.assertEqual(
            {profile.id for profile in manifest.profiles},
            {
                "b0-stock-cubic",
                "b1-controlled-cubic",
                "bbr-fq",
                "skyline-best",
                "skyline-relaxed-congestion-ratio",
            },
        )

    def test_relaxed_ratio_ab_isolates_only_congestion_ratio(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/volatile-link.toml"
        )
        by_id = {profile.id: profile for profile in manifest.profiles}
        best = by_id["skyline-best"]
        relaxed = by_id["skyline-relaxed-congestion-ratio"]
        self.assertEqual(best.rack_rto.rto_max_congestion_ratio_permille, 0)
        self.assertEqual(
            relaxed.rack_rto.rto_max_congestion_ratio_permille, 1500
        )
        # Every other rack_rto/module_config field must be identical --
        # this A/B is meant to isolate exactly one coefficient.
        for field in ("srtt_permille", "floor_us", "ceiling_us", "warmup_samples",
                      "rto_max_normal_permille", "rto_max_congested_permille"):
            self.assertEqual(
                getattr(best.rack_rto, field), getattr(relaxed.rack_rto, field)
            )
        self.assertEqual(best.module_config, relaxed.module_config)

    def test_retransmit_dscp_manifests_expand_to_expected_case_counts(self):
        expected = {
            "retransmit-dscp-mechanism.toml": 12,
            "retransmit-dscp-neutrality.toml": 12,
            "retransmit-dscp-mechanism-v6.toml": 12,
        }
        for name, count in expected.items():
            manifest = load_manifest(EXPERIMENT_DIR / "manifests" / name)
            validate_manifest(manifest)
            self.assertEqual(len(expand_runs(manifest)), count, name)

    def test_retransmit_dscp_mechanism_v6_manifest_is_the_v4_twin(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/retransmit-dscp-mechanism-v6.toml"
        )
        v4_manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/retransmit-dscp-mechanism.toml"
        )
        # Profile set is a literal copy -- CC/modules are address-family
        # agnostic, only the scenarios differ (address_family="v6" + an
        # ipv6 server address at the manifest level).
        self.assertEqual(
            {profile.id for profile in manifest.profiles},
            {profile.id for profile in v4_manifest.profiles},
        )
        self.assertEqual(manifest.server_data_ip6, "fd20:1::2")
        for scenario in manifest.scenarios:
            self.assertEqual(scenario.address_family, "v6")
            self.assertTrue(scenario.capture_retransmits)

    def test_retransmit_dscp_mechanism_manifest_covers_four_cc_and_two_values(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/retransmit-dscp-mechanism.toml"
        )
        by_id = {profile.id for profile in manifest.profiles}
        self.assertEqual(
            by_id,
            {
                "b0-stock-cubic",
                "b1-controlled-cubic",
                "bbr-fq",
                "skyline-best",
                "bbr-fq-dscp-off",
                "bbr-fq-dscp46",
            },
        )
        scenario_ids = {scenario.id for scenario in manifest.scenarios}
        self.assertEqual(scenario_ids, {"retransmit-mechanism", "retransmit-zero-loss"})
        self.assertTrue(
            all(scenario.capture_retransmits for scenario in manifest.scenarios)
        )

    def test_retransmit_dscp_neutrality_ab_isolates_only_enabled_flag(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/retransmit-dscp-neutrality.toml"
        )
        by_id = {profile.id: profile for profile in manifest.profiles}
        on, off = by_id["b1-dscp-on"], by_id["b1-dscp-off"]
        self.assertTrue(on.retransmit_dscp.enabled)
        self.assertFalse(off.retransmit_dscp.enabled)
        self.assertEqual(on.kind, off.kind)
        self.assertEqual(on.cc, off.cc)
        self.assertEqual(on.modules, off.modules)
        self.assertFalse(
            any(scenario.capture_retransmits for scenario in manifest.scenarios)
        )

    def test_guardrail_ab_isolates_only_guardrail_gain(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/guardrail-gain-ab.toml"
        )
        by_id = {profile.id: profile for profile in manifest.profiles}
        best = by_id["skyline-best"]
        neutral = by_id["skyline-guardrail-neutral"]
        self.assertEqual(best.module_config.guardrail_gain, 0.8)
        self.assertEqual(neutral.module_config.guardrail_gain, 0.0)
        # every other module_config field must be identical -- only
        # guardrail_gain may differ between this A/B pair.
        best_fields = best.module_config.__dict__.copy()
        neutral_fields = neutral.module_config.__dict__.copy()
        best_fields.pop("guardrail_gain")
        neutral_fields.pop("guardrail_gain")
        self.assertEqual(best_fields, neutral_fields)
        self.assertEqual(best.rack_rto, neutral.rack_rto)

    def test_rto_max_ab_isolates_only_rack_rto(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/rto-max-validation.toml"
        )
        by_id = {profile.id: profile for profile in manifest.profiles}
        best = by_id["skyline-best"]
        baseline = by_id["skyline-rto-baseline"]
        self.assertIsNotNone(best.rack_rto)
        self.assertTrue(best.rack_rto.enabled)
        self.assertIsNone(baseline.rack_rto)
        self.assertEqual(best.module_config, baseline.module_config)

    def test_gilbert_elliott_stationary_loss(self):
        model = gilbert_elliott(0.5, 5)
        enter = model["enter_bad_pct"] / 100
        leave = model["leave_bad_pct"] / 100
        stationary_bad = enter / (enter + leave)
        self.assertAlmostEqual(stationary_bad, 0.005, places=9)
        self.assertAlmostEqual(1 / leave, 5)

    def test_bdp_packets(self):
        self.assertEqual(bdp_packets(100, 150, 1.0), 1250)

    def test_rto_max_smoke_matrix_expansion(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests/rto-max-smoke.toml"
        )
        self.assertEqual(len(manifest.profiles), 3)
        self.assertEqual(len(manifest.scenarios), 2)
        self.assertEqual(len(expand_runs(manifest)), 3 * 2 * 1)
        by_id = {profile.id: profile for profile in manifest.profiles}
        self.assertIsNone(by_id["b1-controlled-cubic"].rack_rto)
        self.assertEqual(by_id["rtomax-off"].rack_rto.rto_max_normal_permille, 0)
        self.assertEqual(by_id["rtomax-on"].rack_rto.rto_max_normal_permille, 3000)
        self.assertEqual(by_id["rtomax-on"].rack_rto.rto_max_congested_permille, 6000)
        self.assertTrue(by_id["rtomax-off"].rack_rto.enabled)
        self.assertTrue(by_id["rtomax-on"].rack_rto.enabled)

    def test_manifest_rejects_out_of_range_tcp_reordering(self):
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([_base_profile(tcp_reordering=301)]))

    def test_manifest_rejects_out_of_range_tcp_early_retrans(self):
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([_base_profile(tcp_early_retrans=9)]))

    def test_manifest_rejects_inconsistent_rack_rto_bounds(self):
        profile = _base_profile(
            rack_rto=RackRtoSpec(
                enabled=True,
                srtt_permille=1000,
                floor_us=5000,
                ceiling_us=1000,
                warmup_samples=8,
            )
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_rack_rto_ceiling_above_kernel_default(self):
        profile = _base_profile(
            rack_rto=RackRtoSpec(
                enabled=True,
                srtt_permille=1000,
                floor_us=5000,
                ceiling_us=300_000,
                warmup_samples=8,
            )
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_allows_disabled_rack_rto_with_any_bounds(self):
        # enabled=False must skip bound checks entirely -- it's the shape
        # produced when a profile omits [profiles.rack_rto] altogether.
        profile = _base_profile(
            rack_rto=RackRtoSpec(
                enabled=False,
                srtt_permille=0,
                floor_us=0,
                ceiling_us=0,
                warmup_samples=0,
            )
        )
        validate_manifest(_minimal_manifest([profile]))  # must not raise

    # --- retransmit_dscp (marks retransmitted packets' IPv4 ToS DSCP bits) ---

    def test_manifest_rejects_retransmit_dscp_enabled_with_zero_value(self):
        profile = _base_profile(
            retransmit_dscp=RetransmitDscpSpec(enabled=True, dscp_value=0)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_retransmit_dscp_value_above_63(self):
        profile = _base_profile(
            retransmit_dscp=RetransmitDscpSpec(enabled=True, dscp_value=64)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_retransmit_dscp_disabled_value_above_63(self):
        # Out-of-range is rejected regardless of enabled -- the field is
        # meaningless above 63 (6-bit DSCP) either way.
        profile = _base_profile(
            retransmit_dscp=RetransmitDscpSpec(enabled=False, dscp_value=100)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_allows_enabled_retransmit_dscp_with_valid_value(self):
        profile = _base_profile(
            retransmit_dscp=RetransmitDscpSpec(enabled=True, dscp_value=26)
        )
        validate_manifest(_minimal_manifest([profile]))  # must not raise

    def test_manifest_allows_retransmit_dscp_on_non_skyline_profile(self):
        # Deliberately different from module_config/rack_rto: detection
        # lives entirely in bpf/skyline_tc.bpf.c (interface-wide TC egress),
        # independent of cc/modules -- setting this on a stock CUBIC/BBR
        # profile is meaningful, not an error.
        profile = _base_profile(
            kind="stock-cubic",
            cc="cubic",
            retransmit_dscp=RetransmitDscpSpec(enabled=True, dscp_value=10),
        )
        validate_manifest(_minimal_manifest([profile]))  # must not raise

    def test_toml_round_trips_retransmit_dscp(self):
        toml_text = """
name = "retransmit-dscp-toml-smoke"
execution_class = "formal-kvm"
performance_valid = true
seed = 1
warmup_s = 1
duration_s = 1
runs = 1
stock_qdisc = "fq_codel"
server_ssh = "skyline@127.0.0.1:2222"
client_ssh = "skyline@127.0.0.1:2223"
server_data_ip = "10.20.1.2"
server_data_interface = "data0"

[[profiles]]
id = "bbr-fq"
kind = "bbr"
cc = "bbr"
modules = []

[profiles.retransmit_dscp]
enabled = true
dscp_value = 26

[[scenarios]]
id = "only"
rtt_ms = 20
rate_mbit = 100
queue_bdp = 1.0
loss_model = "random"
loss_pct = 0.0
loss_direction = "data"
burst_length = 1
parallel = 1
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "retransmit-dscp-toml-smoke.toml"
            path.write_text(toml_text, encoding="utf-8")
            manifest = load_manifest(path)
        (profile,) = manifest.profiles
        self.assertIsNotNone(profile.retransmit_dscp)
        self.assertTrue(profile.retransmit_dscp.enabled)
        self.assertEqual(profile.retransmit_dscp.dscp_value, 26)

    def test_manifest_rejects_module_config_on_non_skyline_profile(self):
        profile = _base_profile(
            kind="controlled-cubic", module_config=_base_module_config()
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_out_of_range_module_config_ratio(self):
        profile = _base_profile(
            kind="skyline",
            modules=("loss-classifier",),
            module_config=_base_module_config(loss_inflation_max_ratio=0.9),
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_out_of_range_module_config_gain(self):
        profile = _base_profile(
            kind="skyline",
            modules=("adaptive-cwnd",),
            module_config=_base_module_config(cruise_inflight_gain=0.5),
        )
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([profile]))

    def test_manifest_rejects_out_of_range_module_config_cwnd_floor(self):
        for floor in (3, 20_001):
            with self.subTest(min_cwnd_packets=floor):
                profile = _base_profile(
                    kind="skyline",
                    modules=("adaptive-cwnd",),
                    module_config=_base_module_config(min_cwnd_packets=floor),
                )
                with self.assertRaises(ValueError):
                    validate_manifest(_minimal_manifest([profile]))

    def test_manifest_allows_omitted_module_config(self):
        # module_config=None is the shape produced when a profile omits
        # [profiles.module_config] altogether -- must not raise, and must
        # not require kind == "skyline".
        profile = _base_profile(module_config=None)
        validate_manifest(_minimal_manifest([profile]))  # must not raise

    def test_manifest_rejects_out_of_range_reorder_pct(self):
        scenario = _base_scenario(reorder_pct=100.0)
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([_base_profile()], [scenario]))

    def test_manifest_rejects_out_of_range_reorder_correlation(self):
        scenario = _base_scenario(reorder_correlation=150.0)
        with self.assertRaises(ValueError):
            validate_manifest(_minimal_manifest([_base_profile()], [scenario]))

    def test_manifest_allows_zero_reorder_by_default(self):
        scenario = _base_scenario()
        validate_manifest(_minimal_manifest([_base_profile()], [scenario]))

    # --- Segment (mid-case live network-condition transitions) ---

    def test_manifest_allows_valid_segmented_scenario(self):
        scenario = _base_scenario(
            rtt_ms=300,
            loss_model="gemodel",
            loss_pct=0.5,
            burst_length=220,
            segments=(
                _base_segment(offset_s=20.0, label="cliff"),
                _base_segment(offset_s=35.0, rate_mbit=100, label="recovery"),
            ),
        )
        manifest = _manifest_with_segments(scenario, warmup_s=5, duration_s=54)
        validate_manifest(manifest)
        # segments don't change case identity/count -- only scenario.id does.
        self.assertEqual(len(expand_runs(manifest)), 1)

    def test_manifest_rejects_non_increasing_segment_offsets(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5,
            segments=(
                _base_segment(offset_s=30.0),
                _base_segment(offset_s=20.0),
            ),
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario))

    def test_manifest_rejects_segment_offset_inside_warmup(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5, segments=(_base_segment(offset_s=3.0),)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario, warmup_s=5))

    def test_manifest_rejects_segments_too_close_together(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5,
            segments=(
                _base_segment(offset_s=20.0),
                _base_segment(offset_s=21.0),
            ),
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario))

    def test_manifest_rejects_non_finite_segment_offset(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5,
            segments=(_base_segment(offset_s=float("nan")),),
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario))

    def test_manifest_rejects_too_many_segments(self):
        segments = tuple(
            _base_segment(offset_s=20.0 + 5.0 * index) for index in range(17)
        )
        scenario = _base_scenario(rtt_ms=300, loss_pct=0.5, segments=segments)
        with self.assertRaises(ValueError):
            validate_manifest(
                _manifest_with_segments(scenario, warmup_s=5, duration_s=200)
            )

    def test_manifest_rejects_segments_leaving_no_tail(self):
        # warmup_s=5 + duration_s=54 = 59s flow; offset_s=56 leaves only 3s,
        # under the 5s minimum tail.
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5, segments=(_base_segment(offset_s=56.0),)
        )
        with self.assertRaises(ValueError):
            validate_manifest(
                _manifest_with_segments(scenario, warmup_s=5, duration_s=54)
            )

    def test_manifest_rejects_zero_loss_pct_with_segments(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.0, segments=(_base_segment(),)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario))

    def test_manifest_rejects_multi_stream_with_segments(self):
        scenario = _base_scenario(
            rtt_ms=300, loss_pct=0.5, parallel=4, segments=(_base_segment(),)
        )
        with self.assertRaises(ValueError):
            validate_manifest(_manifest_with_segments(scenario))

    def test_toml_round_trips_segments_array(self):
        toml_text = """
name = "segment-toml-smoke"
execution_class = "formal-kvm"
performance_valid = true
seed = 1
warmup_s = 5
duration_s = 54
runs = 1
stock_qdisc = "fq_codel"
server_ssh = "skyline@127.0.0.1:2222"
client_ssh = "skyline@127.0.0.1:2223"
server_data_ip = "10.20.1.2"
server_data_interface = "data0"

[[profiles]]
id = "p"
kind = "controlled-cubic"
cc = "cubic"
modules = []
tcp_recovery = 1

[[scenarios]]
id = "volatile"
rtt_ms = 300
rate_mbit = 100
queue_bdp = 1.0
loss_model = "gemodel"
loss_pct = 0.5
loss_direction = "data"
burst_length = 220
parallel = 1

[[scenarios.segments]]
offset_s = 20.0
rtt_ms = 300
rate_mbit = 15
loss_model = "gemodel"
loss_pct = 8.0
loss_direction = "data"
burst_length = 220
queue_packets = 2500
label = "cliff"

[[scenarios.segments]]
offset_s = 35.0
rtt_ms = 300
rate_mbit = 100
loss_model = "gemodel"
loss_pct = 0.5
loss_direction = "data"
burst_length = 220
label = "recovery"
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "segment-toml-smoke.toml"
            path.write_text(toml_text, encoding="utf-8")
            manifest = load_manifest(path)
        (scenario,) = manifest.scenarios
        self.assertEqual(len(scenario.segments), 2)
        cliff, recovery = scenario.segments
        self.assertEqual(cliff.offset_s, 20.0)
        self.assertEqual(cliff.rate_mbit, 15)
        self.assertEqual(cliff.queue_packets, 2500)
        self.assertEqual(cliff.label, "cliff")
        self.assertIsNone(recovery.queue_packets)
        self.assertEqual(recovery.label, "recovery")

    def test_dimensions_reject_segments_key(self):
        from matrix_lib import _expand_dimensions

        with self.assertRaises(ValueError):
            list(
                _expand_dimensions(
                    {
                        "rtt_ms": [20],
                        "rate_mbit": [100],
                        "queue_bdp": [1.0],
                        "loss_pct": [0.0],
                        "loss_model": "random",
                        "loss_direction": "data",
                        "burst_length": 1,
                        "parallel": 1,
                        "segments": [],
                    }
                )
            )

if __name__ == "__main__":
    unittest.main()
