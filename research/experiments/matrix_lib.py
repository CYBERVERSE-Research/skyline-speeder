#!/usr/bin/env python3
from __future__ import annotations

import dataclasses
import hashlib
import itertools
import json
import math
import random
import tomllib
from pathlib import Path
from typing import Any, Iterable


KNOWN_MODULES = {
    "early-loss",
    "adaptive-cwnd",
    "loss-classifier",
    "pacing",
}
KNOWN_PROFILE_KINDS = {"stock-cubic", "controlled-cubic", "skyline", "bbr"}
KNOWN_LOSS_MODELS = {"random", "gemodel"}
KNOWN_LOSS_DIRECTIONS = {"data", "ack", "symmetric"}
KNOWN_EXECUTION_CLASSES = {"formal-kvm", "tcg-validation"}


@dataclasses.dataclass(frozen=True)
class RackRtoSpec:
    """M1 tier-2 per-flow dynamic TCP_BPF_RTO_MIN tuning, applied via
    `ssctl set-rack-rto`/`reset-rack-rto` (see run_matrix.py's apply_profile).
    Independent of the tier-1 sysctls above -- those are applied by
    infra/apply-guest-profile.sh, this is applied separately by ssctl.
    """

    enabled: bool
    srtt_permille: int
    floor_us: int
    ceiling_us: int
    warmup_samples: int
    # TCP_RTO_MAX_MS ceiling feature (since v2 of struct skyline_rto_tuning);
    # 0/0 leaves it disabled regardless of `enabled` above.
    rto_max_normal_permille: int = 0
    rto_max_congested_permille: int = 0
    rto_max_congestion_ratio_permille: int = 0


@dataclasses.dataclass(frozen=True)
class RetransmitDscpSpec:
    """Marks retransmitted TCP segments with a DSCP codepoint (IPv4 ToS
    upper 6 bits), applied via `ssctl set-retransmit-dscp`/
    `reset-retransmit-dscp` (see run_matrix.py's apply_retransmit_dscp).
    Mirrors `RetransmitDscpConfig` in crates/skyline-common/src/lib.rs
    field-for-field. Applied by bpf/skyline_tc.bpf.c (interface-wide TC egress,
    not cgroup-scoped) -- affects every TCP flow on the profile's data
    interface regardless of which congestion control it runs, so this is
    independent of `cc`/`modules` above.
    """

    enabled: bool
    dscp_value: int = 0


@dataclasses.dataclass(frozen=True)
class ModuleConfigSpec:
    """M2 ([adaptive_cwnd])/M3 ([loss_classifier]) coefficients plus the
    top-level safety limits, applied via `ssctl set-module-config`/
    `reset-module-config` (see run_matrix.py's apply_module_config). Mirrors
    `ModuleTuningConfig` in crates/skyline-common/src/lib.rs field-for-field --
    unlike RackRtoSpec there is no `enabled` gate, since these coefficients
    only matter once a profile's `modules` tuple turns M2/M3/M4 on; omitting
    `[profiles.module_config]` entirely (module_config=None) just means "use
    whatever skyline-speederd already has configured", the same as never calling
    set-module-config at all.

    `_parse_module_config()` stays permissive about unrecognized keys: a
    `[profiles.module_config]` table with a stale or misspelled field name
    still parses, the unrecognized field simply has no effect (dropped, not
    errored), and every ModuleConfigSpec field it doesn't set takes the
    current default. This means a config typo silently falls back to the
    default rather than crashing the run -- write manifests against the
    field names declared below to be sure they set what they mean to.
    """

    max_pacing_mbps: int
    max_cwnd_packets: int
    max_queue_delay_ms: int
    # Ratio of base RTT added to max_queue_delay_ms to form the actual
    # guardrail. 0.0 keeps the guardrail exactly at max_queue_delay_ms.
    max_queue_delay_ratio: float
    # Aggressive initial window (packets). 0 = kernel's own IW.
    initial_cwnd_packets: int
    min_rtt_window_s: int
    bw_window_rtts: int
    startup_plateau_rtts: int
    startup_growth_ratio: float
    # SKYLINE_MODE_STARTUP's single gain (cwnd target and pacing rate both
    # use it while M2 is on).
    startup_gain: float
    # SKYLINE_MODE_CRUISE's cwnd-target gain.
    cruise_inflight_gain: float
    # SKYLINE_MODE_CRUISE's pacing-rate gain.
    cruise_pacing_gain: float
    # Gain applied to both cwnd and pacing for the rest of a round in
    # which the queue-delay/ECN guardrail trips. 0.0 = unset (neutral, only
    # cancels the boost). See flow->queue_clamped's doc comment in
    # bpf/include/skyline_abi.h.
    guardrail_gain: float
    # Ceiling on the measured per-flow loss rate that
    # skyline_loss_inflation_permille() compensates for. 0.0 = disabled
    # (inflation always exactly 1.0x). See that function's doc comment in
    # bpf/skyline_cc.bpf.c.
    loss_inflation_max_ratio: float
    # Baseline-framework PRR-style continuous recovery-phase rate limiting
    # (see FeatureMask::PRR / bpf/include/skyline_abi.h's SKYLINE_FEATURE_PRR doc
    # comment) -- NOT one of the M1-M4 ablation modules, applies by default
    # even to a profile with modules=() (e.g. b2-skyline-base). Default True
    # matches SkylineConfig::prr_pacing_enabled's serde default. Only
    # exercised on the M2-off path.
    prr_pacing_enabled: bool = True
    # Kernel-equivalent pacing-rate ceiling used when M4 (pacing) is off --
    # same rationale/shape as prr_pacing_enabled, see
    # SkylineConfig::auto_pacing_enabled.
    auto_pacing_enabled: bool = True


@dataclasses.dataclass(frozen=True)
class Profile:
    id: str
    kind: str
    cc: str
    modules: tuple[str, ...]
    tcp_recovery: int
    tcp_reordering: int = 3
    tcp_early_retrans: int = 3
    rack_rto: RackRtoSpec | None = None
    module_config: ModuleConfigSpec | None = None
    retransmit_dscp: RetransmitDscpSpec | None = None


@dataclasses.dataclass(frozen=True)
class Segment:
    """A live mid-case network-condition transition (see
    infra/configure-path.sh's --live mode and run_matrix.py's segment
    scheduler thread). A Scenario's own flat fields ARE segment 0 (offset 0,
    applied the normal non-live way before the flow starts); `segments`
    holds transitions AFTER that -- each one hot-updates the router's
    HTB/netem chain via `tc ... change` (not `replace`) partway through the
    same case, without dropping the connection.

    offset_s is relative to the client iperf3 process launching (i.e. it
    INCLUDES the warmup window -- the flow is live throughout, only iperf3's
    own reported statistics omit the warmup_s portion).

    queue_packets, when set, bypasses the usual rate*rtt*queue_bdp BDP
    derivation entirely (see bdp_packets()) and pins the router's netem
    `limit` to this absolute packet count instead. This matters because
    that BDP-derived queue delay is `queue_bdp * rtt_ms`, independent of
    rate_mbit -- a rate cliff alone changes nothing about standing queue
    delay unless the packet count in the buffer is held constant while the
    egress rate drops (the same buffer draining much slower). Segments that
    don't set this get the normal per-segment BDP derivation at their own
    rtt_ms/rate_mbit.
    """

    offset_s: float
    rtt_ms: int
    rate_mbit: int
    loss_model: str
    loss_pct: float
    loss_direction: str
    burst_length: int
    queue_packets: int | None = None
    label: str = ""


@dataclasses.dataclass(frozen=True)
class Scenario:
    id: str
    rtt_ms: int
    rate_mbit: int
    queue_bdp: float
    loss_model: str
    loss_pct: float
    loss_direction: str
    burst_length: int
    parallel: int
    offload: str
    # netem reorder on the data-direction egress only (see
    # infra/configure-path.sh's apply_egress()) -- 0 (default) disables it.
    # Exists to give RACK a path to a spurious-loss verdict later reversed
    # by a DSACK, which plain loss_pct/loss_model cannot produce.
    reorder_pct: float = 0.0
    reorder_correlation: float = 0.0
    # Mid-case live network-condition transitions after this scenario's own
    # fields (segment 0). Empty tuple (default) means a case's network
    # conditions never change after setup. See Segment's doc comment.
    # reorder_pct/reorder_correlation are NOT overridable per-segment --
    # every live update reuses the scenario's own values.
    segments: tuple[Segment, ...] = ()
    # Retransmit-DSCP mechanism verification only: spawn a sender+receiver
    # tcpdump capture for the case (see run_matrix.py's run_case() and
    # analyze_results.py's retransmit-DSCP oracles). False (default) for
    # every other scenario -- capture costs real sender-side CPU, so it
    # must stay off for neutrality/overhead comparisons that reuse the same
    # scenario shape with only a profile's retransmit_dscp toggled.
    capture_retransmits: bool = False
    # "v4" (default, every pre-existing manifest) or "v6" -- which of
    # Manifest.server_data_ip/server_data_ip6 the client connects to and
    # which iperf3 -4/-6 flag run_matrix.py passes. Per-scenario rather than
    # per-manifest/per-profile: the same profile matrix is meaningful run
    # over either family (see manifests/retransmit-dscp-mechanism-v6.toml),
    # so the address family is a property of the network path, same tier as
    # rtt_ms/loss_pct.
    address_family: str = "v4"


@dataclasses.dataclass(frozen=True)
class RunCase:
    experiment: str
    case_id: str
    profile: Profile
    scenario: Scenario
    run_index: int
    seed: int
    warmup_s: int
    duration_s: int

    def as_dict(self) -> dict[str, Any]:
        data = dataclasses.asdict(self)
        data["profile"]["modules"] = list(self.profile.modules)
        data["gilbert_elliott"] = (
            gilbert_elliott(self.scenario.loss_pct, self.scenario.burst_length)
            if self.scenario.loss_model == "gemodel"
            else None
        )
        data["queue_packets"] = bdp_packets(
            self.scenario.rate_mbit,
            self.scenario.rtt_ms,
            self.scenario.queue_bdp,
        )
        # The two derived fields above describe segment 0 only. For a
        # segmented scenario, list each segment's own derived queue depth
        # (respecting its queue_packets override, if set) and loss model
        # alongside -- otherwise a reader of metadata.json's top-level
        # gilbert_elliott/queue_packets could mistake segment 0's numbers
        # for the whole case's behavior.
        data["segment_derived"] = [
            {
                "offset_s": segment.offset_s,
                "label": segment.label,
                "gilbert_elliott": (
                    gilbert_elliott(segment.loss_pct, segment.burst_length)
                    if segment.loss_model == "gemodel"
                    else None
                ),
                "queue_packets": (
                    segment.queue_packets
                    if segment.queue_packets is not None
                    else bdp_packets(
                        segment.rate_mbit, segment.rtt_ms, self.scenario.queue_bdp
                    )
                ),
            }
            for segment in self.scenario.segments
        ]
        return data


@dataclasses.dataclass(frozen=True)
class Manifest:
    name: str
    execution_class: str
    performance_valid: bool
    seed: int
    warmup_s: int
    duration_s: int
    runs: int
    stock_qdisc: str
    server_ssh: str
    client_ssh: str
    server_data_ip: str
    server_data_interface: str
    profiles: tuple[Profile, ...]
    scenarios: tuple[Scenario, ...]
    # Optional -- only required when a scenario sets address_family="v6"
    # (validate_manifest enforces that). Kept separate from server_data_ip
    # rather than making that field a union: every existing manifest/
    # consumer already treats server_data_ip as "the" address, and v4-only
    # manifests (the overwhelming majority) should not have to think about
    # address families at all.
    server_data_ip6: str | None = None


def load_manifest(path: str | Path) -> Manifest:
    path = Path(path)
    with path.open("rb") as handle:
        raw = tomllib.load(handle)
    profiles = tuple(_parse_profile(item) for item in raw["profiles"])
    if "scenarios" in raw:
        scenarios = tuple(_parse_scenario(item) for item in raw["scenarios"])
    else:
        scenarios = tuple(_expand_dimensions(raw["dimensions"]))
    manifest = Manifest(
        name=str(raw["name"]),
        execution_class=str(raw["execution_class"]),
        performance_valid=bool(raw["performance_valid"]),
        seed=int(raw["seed"]),
        warmup_s=int(raw["warmup_s"]),
        duration_s=int(raw["duration_s"]),
        runs=int(raw["runs"]),
        stock_qdisc=str(raw["stock_qdisc"]),
        server_ssh=str(raw["server_ssh"]),
        client_ssh=str(raw["client_ssh"]),
        server_data_ip=str(raw["server_data_ip"]),
        server_data_interface=str(raw["server_data_interface"]),
        profiles=profiles,
        scenarios=scenarios,
        server_data_ip6=(
            str(raw["server_data_ip6"]) if "server_data_ip6" in raw else None
        ),
    )
    validate_manifest(manifest)
    return manifest


def _parse_profile(item: dict[str, Any]) -> Profile:
    rack_rto_raw = item.get("rack_rto")
    module_config_raw = item.get("module_config")
    retransmit_dscp_raw = item.get("retransmit_dscp")
    return Profile(
        id=str(item["id"]),
        kind=str(item["kind"]),
        cc=str(item["cc"]),
        modules=tuple(str(module) for module in item.get("modules", [])),
        tcp_recovery=int(item.get("tcp_recovery", 1)),
        tcp_reordering=int(item.get("tcp_reordering", 3)),
        tcp_early_retrans=int(item.get("tcp_early_retrans", 3)),
        rack_rto=_parse_rack_rto(rack_rto_raw) if rack_rto_raw is not None else None,
        module_config=(
            _parse_module_config(module_config_raw)
            if module_config_raw is not None
            else None
        ),
        retransmit_dscp=(
            _parse_retransmit_dscp(retransmit_dscp_raw)
            if retransmit_dscp_raw is not None
            else None
        ),
    )


def _parse_rack_rto(item: dict[str, Any]) -> RackRtoSpec:
    return RackRtoSpec(
        enabled=bool(item.get("enabled", False)),
        srtt_permille=int(item.get("srtt_permille", 1000)),
        floor_us=int(item.get("floor_us", 5000)),
        ceiling_us=int(item.get("ceiling_us", 200_000)),
        warmup_samples=int(item.get("warmup_samples", 8)),
        rto_max_normal_permille=int(item.get("rto_max_normal_permille", 0)),
        rto_max_congested_permille=int(item.get("rto_max_congested_permille", 0)),
        rto_max_congestion_ratio_permille=int(
            item.get("rto_max_congestion_ratio_permille", 0)
        ),
    )


def _parse_retransmit_dscp(item: dict[str, Any]) -> RetransmitDscpSpec:
    return RetransmitDscpSpec(
        enabled=bool(item.get("enabled", False)),
        dscp_value=int(item.get("dscp_value", 0)),
    )


def _parse_module_config(item: dict[str, Any]) -> ModuleConfigSpec:
    # Defaults mirror config/speeder.toml's current values so an override
    # table that only sets a handful of fields still produces a complete,
    # absolute-replace ModuleConfigSpec for ssctl set-module-config. A key
    # that doesn't map to a ModuleConfigSpec field is silently dropped
    # rather than rejected -- see ModuleConfigSpec's doc comment for why
    # staying permissive here is the deliberate choice, not an oversight.
    return ModuleConfigSpec(
        max_pacing_mbps=int(item.get("max_pacing_mbps", 1200)),
        max_cwnd_packets=int(item.get("max_cwnd_packets", 50_000)),
        max_queue_delay_ms=int(item.get("max_queue_delay_ms", 100)),
        max_queue_delay_ratio=float(item.get("max_queue_delay_ratio", 1.0)),
        initial_cwnd_packets=int(item.get("initial_cwnd_packets", 100)),
        min_rtt_window_s=int(item.get("min_rtt_window_s", 10)),
        bw_window_rtts=int(item.get("bw_window_rtts", 10)),
        startup_plateau_rtts=int(item.get("startup_plateau_rtts", 3)),
        startup_growth_ratio=float(item.get("startup_growth_ratio", 0.25)),
        startup_gain=float(item.get("startup_gain", 3.0)),
        # Deliberately NOT config/speeder.toml's shipped defaults: every
        # default in this block is the coefficient set
        # docs/04-performance-report.md measured as `skyline-best`, pinned
        # here so that report stays reproducible after the shipped defaults
        # moved to field-tuned values. A profile that wants the shipped
        # defaults has to spell them out.
        cruise_inflight_gain=float(item.get("cruise_inflight_gain", 2.0)),
        cruise_pacing_gain=float(item.get("cruise_pacing_gain", 1.1)),
        guardrail_gain=float(item.get("guardrail_gain", 0.8)),
        loss_inflation_max_ratio=float(item.get("loss_inflation_max_ratio", 0.5)),
        prr_pacing_enabled=bool(item.get("prr_pacing_enabled", True)),
        auto_pacing_enabled=bool(item.get("auto_pacing_enabled", True)),
    )


def _parse_scenario(item: dict[str, Any]) -> Scenario:
    return Scenario(
        id=str(item["id"]),
        rtt_ms=int(item["rtt_ms"]),
        rate_mbit=int(item["rate_mbit"]),
        queue_bdp=float(item["queue_bdp"]),
        loss_model=str(item["loss_model"]),
        loss_pct=float(item["loss_pct"]),
        loss_direction=str(item["loss_direction"]),
        burst_length=int(item["burst_length"]),
        parallel=int(item["parallel"]),
        offload=str(item.get("offload", "on")),
        reorder_pct=float(item.get("reorder_pct", 0.0)),
        reorder_correlation=float(item.get("reorder_correlation", 0.0)),
        segments=tuple(_parse_segment(seg) for seg in item.get("segments", [])),
        capture_retransmits=bool(item.get("capture_retransmits", False)),
        address_family=str(item.get("address_family", "v4")),
    )


def _parse_segment(item: dict[str, Any]) -> Segment:
    queue_packets = item.get("queue_packets")
    return Segment(
        offset_s=float(item["offset_s"]),
        rtt_ms=int(item["rtt_ms"]),
        rate_mbit=int(item["rate_mbit"]),
        loss_model=str(item["loss_model"]),
        loss_pct=float(item["loss_pct"]),
        loss_direction=str(item["loss_direction"]),
        burst_length=int(item["burst_length"]),
        queue_packets=int(queue_packets) if queue_packets is not None else None,
        label=str(item.get("label", "")),
    )


def _expand_dimensions(dimensions: dict[str, Any]) -> Iterable[Scenario]:
    if "segments" in dimensions:
        raise ValueError(
            "[dimensions] cannot express per-segment scenarios -- write "
            "affected scenarios explicitly under [[scenarios]] instead"
        )
    for rtt_ms, rate_mbit, queue_bdp, loss_pct in itertools.product(
        dimensions["rtt_ms"],
        dimensions["rate_mbit"],
        dimensions["queue_bdp"],
        dimensions["loss_pct"],
    ):
        yield Scenario(
            id=f"rtt{rtt_ms}-rate{rate_mbit}-q{queue_bdp:g}-loss{loss_pct:g}",
            rtt_ms=int(rtt_ms),
            rate_mbit=int(rate_mbit),
            queue_bdp=float(queue_bdp),
            loss_model=str(dimensions["loss_model"]),
            loss_pct=float(loss_pct),
            loss_direction=str(dimensions["loss_direction"]),
            burst_length=int(dimensions["burst_length"]),
            parallel=int(dimensions["parallel"]),
            offload=str(dimensions.get("offload", "on")),
        )


def validate_manifest(manifest: Manifest) -> None:
    if manifest.execution_class not in KNOWN_EXECUTION_CLASSES:
        raise ValueError(
            f"unknown execution class {manifest.execution_class}"
        )
    if (
        manifest.execution_class == "tcg-validation"
        and manifest.performance_valid
    ):
        raise ValueError("TCG validation cannot produce valid performance data")
    if (
        manifest.execution_class == "formal-kvm"
        and not manifest.performance_valid
    ):
        raise ValueError("formal KVM manifests must enable performance validity")
    if manifest.runs < 1 or manifest.duration_s < 1 or manifest.warmup_s < 0:
        raise ValueError("invalid run count or duration")
    if len({profile.id for profile in manifest.profiles}) != len(manifest.profiles):
        raise ValueError("profile IDs must be unique")
    if len({scenario.id for scenario in manifest.scenarios}) != len(manifest.scenarios):
        raise ValueError("scenario IDs must be unique")
    for profile in manifest.profiles:
        if profile.kind not in KNOWN_PROFILE_KINDS:
            raise ValueError(f"unknown profile kind {profile.kind}")
        unknown = set(profile.modules) - KNOWN_MODULES
        if unknown:
            raise ValueError(f"profile {profile.id} has unknown modules {sorted(unknown)}")
        if profile.kind != "skyline" and profile.modules:
            raise ValueError(f"non-Skyline Speeder profile {profile.id} cannot enable Skyline Speeder modules")
        if profile.tcp_recovery not in {1, 3, 5, 7}:
            raise ValueError(
                f"profile {profile.id} has unsupported tcp_recovery "
                f"{profile.tcp_recovery}"
            )
        if not 1 <= profile.tcp_reordering <= 300:
            raise ValueError(
                f"profile {profile.id} has unsupported tcp_reordering "
                f"{profile.tcp_reordering}"
            )
        if profile.tcp_early_retrans not in {0, 1, 2, 3, 4}:
            raise ValueError(
                f"profile {profile.id} has unsupported tcp_early_retrans "
                f"{profile.tcp_early_retrans}"
            )
        if profile.rack_rto is not None and profile.rack_rto.enabled:
            rack_rto = profile.rack_rto
            if rack_rto.srtt_permille <= 0:
                raise ValueError(
                    f"profile {profile.id} rack_rto.srtt_permille must be positive"
                )
            if rack_rto.floor_us <= 0:
                raise ValueError(
                    f"profile {profile.id} rack_rto.floor_us must be positive"
                )
            if rack_rto.ceiling_us < rack_rto.floor_us:
                raise ValueError(
                    f"profile {profile.id} rack_rto.ceiling_us must be "
                    ">= rack_rto.floor_us"
                )
            if rack_rto.ceiling_us > 200_000:
                raise ValueError(
                    f"profile {profile.id} rack_rto.ceiling_us must be <= 200000 "
                    "(kernel TCP_RTO_MIN)"
                )
            if rack_rto.warmup_samples > 1000:
                raise ValueError(
                    f"profile {profile.id} rack_rto.warmup_samples is unreasonably large"
                )
            if (
                rack_rto.rto_max_congested_permille
                < rack_rto.rto_max_normal_permille
            ):
                raise ValueError(
                    f"profile {profile.id} rack_rto.rto_max_congested_permille "
                    "must be >= rto_max_normal_permille"
                )
        if profile.retransmit_dscp is not None:
            dscp = profile.retransmit_dscp
            # Mirrors RetransmitDscpConfig::validate() in
            # crates/skyline-common/src/lib.rs -- enabled with dscp_value=0
            # isn't a no-op, it actively strips any DSCP the socket already
            # carries, and 0 is reserved for "unset"/disabled.
            if dscp.enabled and dscp.dscp_value == 0:
                raise ValueError(
                    f"profile {profile.id} retransmit_dscp.enabled requires a "
                    "non-zero dscp_value (0 = unset)"
                )
            if not 0 <= dscp.dscp_value <= 63:
                raise ValueError(
                    f"profile {profile.id} retransmit_dscp.dscp_value must be "
                    f"between 0 and 63, got {dscp.dscp_value}"
                )
            # Detection lives entirely in bpf/skyline_tc.bpf.c, independent of
            # cc/modules -- setting this on a non-skyline profile is meaningful
            # (that's the whole point: it works for stock CUBIC/BBR too),
            # unlike module_config/rack_rto which are skyline_cc-only knobs.
        if profile.module_config is not None:
            if profile.kind != "skyline":
                raise ValueError(
                    f"non-Skyline Speeder profile {profile.id} cannot set module_config "
                    "(these coefficients only affect skyline_cc)"
                )
            module_config = profile.module_config
            if not 0.0 <= module_config.startup_growth_ratio <= 1.0:
                raise ValueError(
                    f"profile {profile.id} module_config.startup_growth_ratio "
                    f"must be between zero and one, got "
                    f"{module_config.startup_growth_ratio}"
                )
            # Ceiling matches SKYLINE_LOSS_INFLATION_MAX_PERMILLE (500
            # permille, 1/(1-0.5) = 2.0x) on the BPF side.
            if not 0.0 <= module_config.loss_inflation_max_ratio <= 0.5:
                raise ValueError(
                    f"profile {profile.id} module_config.loss_inflation_max_ratio "
                    f"must be between 0.0 and 0.5, got "
                    f"{module_config.loss_inflation_max_ratio}"
                )
            # Every mode gain must be >= 1.0 -- fairness is not a goal,
            # nothing here is meant to ever send slower than the neutral
            # rate.
            if (
                module_config.startup_gain < 1.0
                or module_config.cruise_inflight_gain < 1.0
                or module_config.cruise_pacing_gain < 1.0
            ):
                raise ValueError(
                    f"profile {profile.id} module_config gains must all be >= 1.0"
                )
            # guardrail_gain is the deliberate exception -- it exists
            # to cut below neutral when a real congestion signal fires.
            # 0.0 = unset (neutral).
            if not 0.0 <= module_config.guardrail_gain <= 1.0:
                raise ValueError(
                    f"profile {profile.id} module_config.guardrail_gain "
                    f"must be between 0.0 and 1.0, got "
                    f"{module_config.guardrail_gain}"
                )
            if module_config.max_queue_delay_ratio < 0.0:
                raise ValueError(
                    f"profile {profile.id} module_config.max_queue_delay_ratio "
                    "must be non-negative"
                )
            if module_config.max_cwnd_packets < 4:
                raise ValueError(
                    f"profile {profile.id} module_config.max_cwnd_packets "
                    "must be at least 4"
                )
            if not 1 <= module_config.bw_window_rtts <= 10:
                raise ValueError(
                    f"profile {profile.id} module_config.bw_window_rtts "
                    "must be between 1 and 10"
                )
    for scenario in manifest.scenarios:
        if scenario.loss_model not in KNOWN_LOSS_MODELS:
            raise ValueError(f"unknown loss model {scenario.loss_model}")
        if scenario.loss_direction not in KNOWN_LOSS_DIRECTIONS:
            raise ValueError(f"unknown loss direction {scenario.loss_direction}")
        if scenario.rtt_ms <= 0 or scenario.rate_mbit <= 0:
            raise ValueError("RTT and rate must be positive")
        if not 0 <= scenario.loss_pct < 100:
            raise ValueError("loss percentage must be in [0, 100)")
        if scenario.queue_bdp <= 0 or scenario.parallel <= 0:
            raise ValueError("queue BDP and parallelism must be positive")
        if scenario.offload not in {"on", "off"}:
            raise ValueError("offload must be on or off")
        if scenario.loss_model == "gemodel" and scenario.burst_length < 1:
            raise ValueError("burst length must be at least one")
        if not 0 <= scenario.reorder_pct < 100:
            raise ValueError("reorder_pct must be in [0, 100)")
        if not 0 <= scenario.reorder_correlation <= 100:
            raise ValueError("reorder_correlation must be in [0, 100]")
        if scenario.address_family not in {"v4", "v6"}:
            raise ValueError(
                f"scenario {scenario.id} has unknown address_family "
                f"{scenario.address_family!r} (must be v4 or v6)"
            )
        if scenario.address_family == "v6" and manifest.server_data_ip6 is None:
            raise ValueError(
                f"scenario {scenario.id} sets address_family=v6 but the "
                "manifest has no server_data_ip6"
            )
        if scenario.segments:
            _validate_segments(manifest, scenario)


def _validate_segments(manifest: Manifest, scenario: Scenario) -> None:
    segments = scenario.segments
    if len(segments) > 16:
        raise ValueError(
            f"scenario {scenario.id} has {len(segments)} segments, max 16"
        )
    # A live update only ever edits an already-present loss clause (see
    # infra/configure-path.sh's --live mode); scenario.loss_pct == 0 would
    # mean segment 0 never establishes that clause in the first place, so
    # every later `tc qdisc change ... netem` would be adding the clause
    # structurally rather than just changing its values.
    if scenario.loss_pct <= 0:
        raise ValueError(
            f"scenario {scenario.id} has segments but loss_pct <= 0 -- a "
            "real (even tiny) loss clause must exist from t=0 so later "
            "live updates only change its values"
        )
    if scenario.parallel != 1:
        raise ValueError(
            f"scenario {scenario.id} has segments but parallel={scenario.parallel} "
            "-- multi-stream flows make queue dynamics and RTO attribution "
            "much harder to interpret; use parallel=1 for segmented scenarios"
        )
    min_gap = max(3.0, 2.0 * scenario.rtt_ms / 1000.0)
    warmup_margin = manifest.warmup_s + min_gap
    end_of_flow = manifest.warmup_s + manifest.duration_s
    previous_offset = None
    for segment in segments:
        if not math.isfinite(segment.offset_s):
            raise ValueError(
                f"scenario {scenario.id} has a non-finite segment offset_s"
            )
        if segment.offset_s <= 0:
            raise ValueError(
                f"scenario {scenario.id} segment offset_s must be positive"
            )
        if segment.offset_s < warmup_margin:
            raise ValueError(
                f"scenario {scenario.id} segment offset_s={segment.offset_s} "
                f"falls inside (or too close to) the warmup_s={manifest.warmup_s} "
                "window -- iperf3's -O omits that window from reported "
                "statistics, so a transition there is invisible in results"
            )
        if previous_offset is not None and segment.offset_s - previous_offset < min_gap:
            raise ValueError(
                f"scenario {scenario.id} segments at offset_s="
                f"{previous_offset}/{segment.offset_s} are closer than "
                f"{min_gap}s -- consecutive live updates need time to "
                "propagate before the next one lands"
            )
        previous_offset = segment.offset_s
        if segment.loss_model not in KNOWN_LOSS_MODELS:
            raise ValueError(f"unknown loss model {segment.loss_model}")
        if segment.loss_direction not in KNOWN_LOSS_DIRECTIONS:
            raise ValueError(f"unknown loss direction {segment.loss_direction}")
        if segment.rtt_ms <= 0 or segment.rate_mbit <= 0:
            raise ValueError(
                f"scenario {scenario.id} segment RTT and rate must be positive"
            )
        if not 0 <= segment.loss_pct < 100:
            raise ValueError(
                f"scenario {scenario.id} segment loss percentage must be in [0, 100)"
            )
        if segment.loss_model == "gemodel" and segment.burst_length < 1:
            raise ValueError(
                f"scenario {scenario.id} segment burst length must be at least one"
            )
        if segment.queue_packets is not None and segment.queue_packets < 32:
            raise ValueError(
                f"scenario {scenario.id} segment queue_packets must be >= 32"
            )
    if previous_offset is not None and previous_offset > end_of_flow - 5.0:
        raise ValueError(
            f"scenario {scenario.id}'s last segment at offset_s={previous_offset} "
            f"leaves less than 5s before the flow ends (warmup_s+duration_s="
            f"{end_of_flow}) -- it needs a tail long enough to actually be observed"
        )


def expand_runs(manifest: Manifest) -> list[RunCase]:
    cases: list[RunCase] = []
    sequence = 0
    for profile, scenario, run_index in itertools.product(
        manifest.profiles,
        manifest.scenarios,
        range(1, manifest.runs + 1),
    ):
        sequence += 1
        identity = {
            "experiment": manifest.name,
            "profile": profile.id,
            "scenario": scenario.id,
            "run": run_index,
            "seed": manifest.seed,
        }
        digest = hashlib.sha256(
            json.dumps(identity, sort_keys=True).encode("utf-8")
        ).hexdigest()[:12]
        cases.append(
            RunCase(
                experiment=manifest.name,
                case_id=f"{profile.id}--{scenario.id}--r{run_index:02d}--{digest}",
                profile=profile,
                scenario=scenario,
                run_index=run_index,
                seed=manifest.seed + sequence,
                warmup_s=manifest.warmup_s,
                duration_s=manifest.duration_s,
            )
        )
    random.Random(manifest.seed).shuffle(cases)
    return cases


def gilbert_elliott(mean_loss_pct: float, burst_length: int) -> dict[str, float]:
    if not 0 < mean_loss_pct < 100:
        raise ValueError("Gilbert-Elliott mean loss must be between zero and 100")
    if burst_length < 1:
        raise ValueError("burst length must be positive")
    stationary_bad = mean_loss_pct / 100.0
    leave_bad = 1.0 / burst_length
    enter_bad = stationary_bad * leave_bad / (1.0 - stationary_bad)
    return {
        "enter_bad_pct": enter_bad * 100.0,
        "leave_bad_pct": leave_bad * 100.0,
        "loss_bad_pct": 100.0,
        "loss_good_pct": 0.0,
    }


def bdp_packets(
    rate_mbit: int,
    rtt_ms: int,
    queue_bdp: float,
    packet_bytes: int = 1500,
) -> int:
    packets = rate_mbit * 1_000_000 * rtt_ms / 1000 / (8 * packet_bytes)
    return max(32, round(packets * queue_bdp))
