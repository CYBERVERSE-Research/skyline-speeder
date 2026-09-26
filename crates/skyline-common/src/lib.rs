// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
use bitflags::bitflags;
use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;

/// ABI version for `KernelConfig`, matched against `bpf/include/skyline_abi.h`'s
/// `SKYLINE_ABI_VERSION`. `skyline_config_get_slot()` on the BPF side rejects a
/// version mismatch rather than misinterpreting the struct layout. See
/// `bpf/include/skyline_abi.h`'s top-of-file comment for what the current
/// layout encodes (target deployment envelope, M2/M3 semantics, the
/// queue-delay/ECN guardrail's `guardrail_gain_permille`).
pub const ABI_VERSION: u32 = 7;

/// The congestion control algorithm name skyline_cc registers under. Must stay
/// byte-identical to `.name` in bpf/skyline_cc.bpf.c -- the kernel matches the
/// sysctl write in `skyline-speederd` against the registered name, and a mismatch
/// fails with EINVAL rather than silently doing nothing. `ssctl` compares the
/// live `tcp_congestion_control` against it to tell "attached" from "actually
/// carrying the host's traffic". Deliberately NOT reused for struct_ops *map*
/// name lookups: those happen to be the same string today but are a different
/// namespace, and collapsing them would couple two things that are free to
/// diverge.
pub const SKYLINE_CC_NAME: &str = "skyline_cc";

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FeatureMask: u32 {
        const EARLY_LOSS = 1 << 0;
        const ADAPTIVE_CWND = 1 << 1;
        const LOSS_CLASSIFIER = 1 << 2;
        const PACING = 1 << 3;
        /* Not one of the M1-M4 ablation `Module`s -- driven by
         * SkylineConfig::prr_pacing_enabled instead of `enabled_modules`, so it
         * still applies to a modules=[] baseline profile by default. See
         * bpf/include/skyline_abi.h's SKYLINE_FEATURE_PRR doc comment. Only
         * exercised on the M2-off path (M2 on bypasses PRR entirely).
         */
        const PRR = 1 << 4;
        /// Same shape as PRR -- driven by SkylineConfig::auto_pacing_enabled,
        /// applies regardless of `enabled_modules`. See
        /// bpf/include/skyline_abi.h's SKYLINE_FEATURE_AUTO_PACING doc comment.
        const AUTO_PACING = 1 << 5;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Module {
    EarlyLoss,
    AdaptiveCwnd,
    LossClassifier,
    Pacing,
}

impl Module {
    pub fn bit(self) -> FeatureMask {
        match self {
            Self::EarlyLoss => FeatureMask::EARLY_LOSS,
            Self::AdaptiveCwnd => FeatureMask::ADAPTIVE_CWND,
            Self::LossClassifier => FeatureMask::LOSS_CLASSIFIER,
            Self::Pacing => FeatureMask::PACING,
        }
    }
}

impl fmt::Display for Module {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::EarlyLoss => "early-loss",
            Self::AdaptiveCwnd => "adaptive-cwnd",
            Self::LossClassifier => "loss-classifier",
            Self::Pacing => "pacing",
        };
        formatter.write_str(name)
    }
}

impl FromStr for Module {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "early-loss" => Ok(Self::EarlyLoss),
            "adaptive-cwnd" => Ok(Self::AdaptiveCwnd),
            "loss-classifier" => Ok(Self::LossClassifier),
            "pacing" => Ok(Self::Pacing),
            _ => Err(ConfigError::UnknownModule(value.to_owned())),
        }
    }
}

/// M2's live-tunable coefficients (a two-mode STARTUP/CRUISE state
/// machine) -- see `bpf/include/skyline_abi.h`'s
/// `enum skyline_mode` doc comment for why STARTUP/CRUISE is now the entire
/// mode set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveCwndConfig {
    pub min_rtt_window_s: u32,
    pub bw_window_rtts: u32,
    pub startup_plateau_rtts: u32,
    pub startup_growth_ratio: f64,
    /// SKYLINE_MODE_STARTUP's single gain (cwnd target and pacing rate both use
    /// it while M2 is on) -- see `KernelConfig::startup_gain_permille`.
    pub startup_gain: f64,
    /// SKYLINE_MODE_CRUISE's cwnd-target gain. Deliberately given more headroom
    /// than `cruise_pacing_gain` -- cwnd only needs to stay ahead of the
    /// pacing rate so pacing remains the binding constraint, mirroring how
    /// BBR treats cwnd as a cap rather than the primary rate control once
    /// pacing is active.
    pub cruise_inflight_gain: f64,
    /// SKYLINE_MODE_CRUISE's pacing-rate gain -- the actual rate control once
    /// M4 is on.
    pub cruise_pacing_gain: f64,
    /// Gain applied to both the cwnd target and the pacing rate for
    /// the rest of a round in which the queue-delay/ECN guardrail trips --
    /// the one signal Skyline Speeder still treats as genuine congestion. 0.0 = unset
    /// (identity: guardrail only cancels the boost, gain stays neutral at
    /// 1.0x). A value in `(0.0, 1.0)` makes this a real self-protective cut
    /// instead of just a ceiling.
    #[serde(default)]
    pub guardrail_gain: f64,
}

/// M3's live-tunable coefficient: a single loss-rate compensation factor
/// (not a multi-way loss classifier) -- see `bpf/skyline_cc.bpf.c`'s
/// `skyline_loss_inflation_permille()` doc comment for why "never reduce for
/// loss" is the correct default in the target deployment regime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LossClassifierConfig {
    /// Ceiling on the measured per-flow loss rate that
    /// `skyline_loss_inflation_permille()` will compensate for when inflating
    /// every BDP-derived cwnd target and the pacing rate (bw_bps is a
    /// *delivered*-rate filter and so under-provisions by (1-p) at loss
    /// rate p). 0.0 = disabled (inflation always exactly 1.0x).
    #[serde(default)]
    pub loss_inflation_max_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub bpf_dir: PathBuf,
    pub pin_dir: PathBuf,
    pub cgroup_path: PathBuf,
    pub socket_path: PathBuf,
    pub state_path: PathBuf,
    pub events_path: PathBuf,
    /// Size, in MiB, at which `events_path` is renamed to `<events_path>.1`
    /// (replacing the previous one) and started afresh, so the event log never
    /// holds much more than twice this. `events_path` sits on /run, a RAM-backed
    /// tmpfs shared with everything else on the host; an unbounded log once
    /// filled it and took Docker down with it. Defaulted so a config written
    /// before the cap existed is bounded too. 0 turns the event log off.
    #[serde(default = "default_events_max_mib")]
    pub events_max_mib: u32,
    #[serde(default)]
    pub tc_interface: Option<String>,
}

/// M1 tier-1 knobs: default values for global (net-namespace-wide) RACK
/// sysctls. `None` means "skyline-speederd does not own this sysctl" -- ownership is
/// handed entirely to the experiment harness (`infra/apply-guest-profile.sh`)
/// whenever a field is left unset, which is why `config/speeder-guest.toml` must
/// keep every field here at `None` (see `guest_config_never_owns_global_sysctls`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RackTuningConfig {
    #[serde(default)]
    pub tcp_recovery: Option<u32>,
    #[serde(default)]
    pub tcp_reordering: Option<u32>,
    #[serde(default)]
    pub tcp_early_retrans: Option<u32>,
}

/// M1 tier-2 knob: per-flow dynamic `TCP_BPF_RTO_MIN` tuning, applied by
/// `skyline_policy.bpf.c` on `BPF_SOCK_OPS_RTT_CB`. Independent of the
/// `[adaptive_cwnd]` / `[loss_classifier]` BPF ABI (`KernelConfig`) -- it
/// travels through its own `KernelRtoTuning` mirror and its own BPF map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RackRtoConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "RackRtoConfig::default_srtt_permille")]
    pub srtt_permille: u32,
    #[serde(default = "RackRtoConfig::default_floor_us")]
    pub floor_us: u32,
    #[serde(default = "RackRtoConfig::default_ceiling_us")]
    pub ceiling_us: u32,
    #[serde(default = "RackRtoConfig::default_warmup_samples")]
    pub warmup_samples: u32,
    /// TCP_RTO_MAX_MS ceiling = clamp(this * base_rtt / 1000, 1000ms,
    /// 120000ms) while no congestion evidence has been seen on the flow.
    /// 0 = the whole rto_max feature is disabled (TCP_RTO_MAX_MS is left at
    /// the kernel default), independent of the floor fields above.
    #[serde(default)]
    pub rto_max_normal_permille: u32,
    /// Same formula, once a fresh CE mark or growing queue delay (see
    /// `rto_max_congestion_ratio_permille`) is observed. Should be
    /// >= `rto_max_normal_permille` -- congestion should widen backoff's
    /// room, never shrink it.
    #[serde(default)]
    pub rto_max_congested_permille: u32,
    /// srtt exceeding min_rtt by more than this ratio (permille) counts as
    /// queueing evidence for the ceiling above. 0 = let skyline_policy.bpf.c use
    /// its built-in default (2000 = 2x).
    #[serde(default)]
    pub rto_max_congestion_ratio_permille: u32,
}

impl RackRtoConfig {
    fn default_srtt_permille() -> u32 {
        1000
    }
    fn default_floor_us() -> u32 {
        5000
    }
    fn default_ceiling_us() -> u32 {
        200_000
    }
    fn default_warmup_samples() -> u32 {
        8
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            srtt_permille: Self::default_srtt_permille(),
            floor_us: Self::default_floor_us(),
            ceiling_us: Self::default_ceiling_us(),
            warmup_samples: Self::default_warmup_samples(),
            rto_max_normal_permille: 0,
            rto_max_congested_permille: 0,
            rto_max_congestion_ratio_permille: 0,
        }
    }

    pub fn kernel_config(&self, generation: u32) -> KernelRtoTuning {
        KernelRtoTuning {
            abi_version: SKYLINE_RTO_TUNING_ABI_VERSION,
            generation,
            enabled: u32::from(self.enabled),
            srtt_permille: self.srtt_permille,
            floor_us: self.floor_us,
            ceiling_us: self.ceiling_us.min(SKYLINE_RTO_MIN_KERNEL_DEFAULT_US),
            warmup_samples: self.warmup_samples,
            rto_max_normal_permille: self.rto_max_normal_permille,
            rto_max_congested_permille: self.rto_max_congested_permille,
            rto_max_congestion_ratio_permille: self.rto_max_congestion_ratio_permille,
            reserved: 0,
        }
    }
}

impl Default for RackRtoConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Marks retransmitted TCP segments with a DSCP codepoint in the IPv4
/// header's ToS byte, so upstream network equipment can apply policy
/// routing keyed on it. `dscp_value` is deliberately a placeholder -- the
/// actual codepoint is a network-team policy decision, not something Skyline Speeder
/// should hardcode; `enabled=true` with `dscp_value=0` is rejected by
/// `validate()` below because it isn't a no-op, it's a live directive to
/// strip whatever DSCP the socket may already carry.
///
/// Independent of `KernelConfig`/`ABI_VERSION`, same rationale as
/// `RackRtoConfig`: this changes at most once per deployment, not every
/// round, so a single fully-overwritten array-map slot is enough. Applied
/// by `bpf/skyline_tc.bpf.c` (interface-wide TC egress on `data0`, not
/// cgroup-scoped) -- detection compares each packet's own TCP sequence
/// number against `bpf_tcp_sock(sk)->snd_nxt`, so it works uniformly across
/// every congestion control on that interface (stock CUBIC, BBR, or
/// skyline_cc), not just Skyline Speeder-managed flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetransmitDscpConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 0-63 (6-bit DSCP field). Required to be non-zero while `enabled`.
    #[serde(default)]
    pub dscp_value: u8,
}

impl RetransmitDscpConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            dscp_value: 0,
        }
    }

    pub fn kernel_config(&self) -> KernelRetransmitDscpConfig {
        KernelRetransmitDscpConfig {
            abi_version: SKYLINE_RETRANSMIT_DSCP_ABI_VERSION,
            enabled: u32::from(self.enabled),
            dscp_value: u32::from(self.dscp_value),
            reserved: 0,
        }
    }
}

impl Default for RetransmitDscpConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// `[guard]`: what skyline-speederd keeps in place while skyline_cc is enabled
/// (between a successful `Request::Enable` and the next `Request::Drain`).
///
/// Ownership, which is an invariant -- each of these has exactly one writer so
/// two places cannot drift apart:
/// - `net.ipv4.tcp_congestion_control` = `skyline_cc` (always; skyline-speederd
///   already owned this sysctl before the guard existed);
/// - `net.core.default_qdisc` = `fq` and the root qdisc of
///   `runtime.tc_interface` = `fq` (or `mq` whose every child is `fq` on a
///   multi-queue device), only while `qdisc = true`. When `tc_interface` is a
///   VLAN, bond or bridge (root `noqueue`, the kernel default there) that
///   means the NICs under it instead; a shaped cake and classful/shaping
///   qdiscs are left alone. `infra/boot-enable.sh` used to write the qdisc
///   sysctl once at boot; it no longer does.
///
/// Why a periodic re-check and not a one-shot write: "one-click BBR" scripts
/// persist `tcp_congestion_control=bbr` and `default_qdisc=cake|fq_pie` in
/// /etc/sysctl.d, and anything that re-runs `sysctl --system` flips the host
/// back silently -- every new connection then bypasses skyline_cc and nothing
/// reports it. And `default_qdisc` only affects qdiscs created afterwards, so a
/// NIC brought up before that sysctl ran keeps whatever it got (fq_codel on a
/// stock Debian host), which is why the interface's root qdisc is replaced too.
///
/// Independent of `KernelConfig`/`ABI_VERSION`: nothing here reaches BPF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardConfig {
    /// Seconds between re-checks while skyline_cc is enabled. 0 turns the
    /// periodic re-check off; `ssctl enable` still applies everything once.
    #[serde(default = "default_guard_interval_s")]
    pub interval_s: u32,
    /// `false` leaves every qdisc alone: skyline-speederd then owns only the
    /// congestion control.
    #[serde(default = "default_true")]
    pub qdisc: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            interval_s: DEFAULT_GUARD_INTERVAL_S,
            qdisc: true,
        }
    }
}

/// See `GuardConfig::interval_s`.
pub const DEFAULT_GUARD_INTERVAL_S: u32 = 5;

/// Upper bound on `GuardConfig::interval_s`. A re-check an hour apart is
/// already too slow to be worth calling a guard; anything beyond that is far
/// more likely a typo (milliseconds for seconds) than intent.
pub const MAX_GUARD_INTERVAL_S: u32 = 3600;

fn default_guard_interval_s() -> u32 {
    DEFAULT_GUARD_INTERVAL_S
}

/// Live-tunable mirror of every M2 (`[adaptive_cwnd]`)/M3 (`[loss_classifier]`)
/// coefficient plus the top-level safety limits (`max_pacing_mbps`/
/// `max_cwnd_packets`/`max_queue_delay_ms`/`initial_cwnd_packets`). This
/// struct is the wire payload for `Request::SetModuleConfig`/
/// `ssctl set-module-config`: absolute-replace semantics (every field
/// always sent), flat (not nested under `adaptive_cwnd`/`loss_classifier`)
/// so it maps directly onto clap's `--long` flags.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModuleTuningConfig {
    pub max_pacing_mbps: u64,
    pub max_cwnd_packets: u32,
    pub max_queue_delay_ms: u32,
    /// See `SkylineConfig::max_queue_delay_ratio`.
    pub max_queue_delay_ratio: f64,
    /// See `SkylineConfig::initial_cwnd_packets`.
    pub initial_cwnd_packets: u32,
    /// See `SkylineConfig::min_cwnd_packets`. Defaulted on the wire so a
    /// request that predates the field still means "today's behavior"
    /// rather than failing to parse.
    #[serde(default = "default_min_cwnd_packets")]
    pub min_cwnd_packets: u32,
    pub min_rtt_window_s: u32,
    pub bw_window_rtts: u32,
    pub startup_plateau_rtts: u32,
    pub startup_growth_ratio: f64,
    /// See `AdaptiveCwndConfig::startup_gain`.
    pub startup_gain: f64,
    /// See `AdaptiveCwndConfig::cruise_inflight_gain`.
    pub cruise_inflight_gain: f64,
    /// See `AdaptiveCwndConfig::cruise_pacing_gain`.
    pub cruise_pacing_gain: f64,
    /// See `AdaptiveCwndConfig::guardrail_gain`.
    pub guardrail_gain: f64,
    /// See `LossClassifierConfig::loss_inflation_max_ratio`.
    pub loss_inflation_max_ratio: f64,
    pub prr_pacing_enabled: bool,
    pub auto_pacing_enabled: bool,
}

impl ModuleTuningConfig {
    /// Snapshot the current effective values out of a loaded `SkylineConfig`.
    /// Used both for `ssctl status`'s echo and to remember the
    /// configuration-file defaults `Request::ResetModuleConfig` restores.
    pub fn from_config(config: &SkylineConfig) -> Self {
        Self {
            max_pacing_mbps: config.max_pacing_mbps,
            max_cwnd_packets: config.max_cwnd_packets,
            max_queue_delay_ms: config.max_queue_delay_ms,
            max_queue_delay_ratio: config.max_queue_delay_ratio,
            initial_cwnd_packets: config.initial_cwnd_packets,
            min_cwnd_packets: config.min_cwnd_packets,
            min_rtt_window_s: config.adaptive_cwnd.min_rtt_window_s,
            bw_window_rtts: config.adaptive_cwnd.bw_window_rtts,
            startup_plateau_rtts: config.adaptive_cwnd.startup_plateau_rtts,
            startup_growth_ratio: config.adaptive_cwnd.startup_growth_ratio,
            startup_gain: config.adaptive_cwnd.startup_gain,
            cruise_inflight_gain: config.adaptive_cwnd.cruise_inflight_gain,
            cruise_pacing_gain: config.adaptive_cwnd.cruise_pacing_gain,
            guardrail_gain: config.adaptive_cwnd.guardrail_gain,
            loss_inflation_max_ratio: config.loss_classifier.loss_inflation_max_ratio,
            prr_pacing_enabled: config.prr_pacing_enabled,
            auto_pacing_enabled: config.auto_pacing_enabled,
        }
    }

    /// Write this snapshot back into an `SkylineConfig` in place. Callers are
    /// responsible for re-running `validate()` afterwards and rolling back
    /// (by calling `apply_to` again with a pre-change snapshot) on failure --
    /// see `Request::SetModuleConfig`/`ResetModuleConfig` in `skyline-speederd`.
    pub fn apply_to(&self, config: &mut SkylineConfig) {
        config.max_pacing_mbps = self.max_pacing_mbps;
        config.max_cwnd_packets = self.max_cwnd_packets;
        config.max_queue_delay_ms = self.max_queue_delay_ms;
        config.max_queue_delay_ratio = self.max_queue_delay_ratio;
        config.initial_cwnd_packets = self.initial_cwnd_packets;
        config.min_cwnd_packets = self.min_cwnd_packets;
        config.adaptive_cwnd.min_rtt_window_s = self.min_rtt_window_s;
        config.adaptive_cwnd.bw_window_rtts = self.bw_window_rtts;
        config.adaptive_cwnd.startup_plateau_rtts = self.startup_plateau_rtts;
        config.adaptive_cwnd.startup_growth_ratio = self.startup_growth_ratio;
        config.adaptive_cwnd.startup_gain = self.startup_gain;
        config.adaptive_cwnd.cruise_inflight_gain = self.cruise_inflight_gain;
        config.adaptive_cwnd.cruise_pacing_gain = self.cruise_pacing_gain;
        config.adaptive_cwnd.guardrail_gain = self.guardrail_gain;
        config.loss_classifier.loss_inflation_max_ratio = self.loss_inflation_max_ratio;
        config.prr_pacing_enabled = self.prr_pacing_enabled;
        config.auto_pacing_enabled = self.auto_pacing_enabled;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkylineConfig {
    pub generation: u32,
    pub enabled_modules: Vec<Module>,
    /// Baseline-framework PRR-style continuous recovery-phase rate limiting
    /// (see FeatureMask::PRR / bpf/include/skyline_abi.h's SKYLINE_FEATURE_PRR doc
    /// comment) -- deliberately NOT part of `enabled_modules`: it applies
    /// by default even to a profile with modules=[] (e.g. b2-skyline-base),
    /// since it fixes a baseline-parity bug rather than adding an optional
    /// feature. Exists as its own field purely so it can be switched off
    /// for rollback/A-B comparison against the pre-PRR behavior.
    #[serde(default = "default_true")]
    pub prr_pacing_enabled: bool,
    /// Baseline-framework kernel-equivalent pacing-rate ceiling used when
    /// M4 ([`Module::Pacing`]) is off (see FeatureMask::AUTO_PACING /
    /// bpf/include/skyline_abi.h's doc comment) -- same rationale and same
    /// independent-of-`enabled_modules` shape as `prr_pacing_enabled`.
    #[serde(default = "default_true")]
    pub auto_pacing_enabled: bool,
    pub fallback_cc: String,
    pub max_pacing_mbps: u64,
    pub max_cwnd_packets: u32,
    pub max_queue_delay_ms: u32,
    /// Ratio of base RTT added to `max_queue_delay_ms` to form the actual
    /// queue-delay guardrail (see `bpf/skyline_cc.bpf.c`'s
    /// `skyline_max_queue_delay_us()`: `max(max_queue_delay_ms, base_rtt * this)`).
    /// 0.0 (the default) keeps the guardrail exactly at the fixed
    /// `max_queue_delay_ms` value.
    #[serde(default)]
    pub max_queue_delay_ratio: f64,
    /// Aggressive initial window, applied once by `skyline_init()`. Standard
    /// IW10 slow-start takes ~7 RTTs (2.1s at 300ms RTT) to reach 1000
    /// packets -- too slow for the target box's high-RTT corner. 0 = use
    /// whatever the kernel already set (today's behavior).
    #[serde(default)]
    pub initial_cwnd_packets: u32,
    /// Floor under M2's BDP-derived cwnd target (see
    /// `bpf/include/skyline_abi.h`'s `min_cwnd_packets` doc comment). A thin
    /// or app-limited flow's `bw_bps * base_rtt` comes out below a handful
    /// of packets, and a window that small can only recover from a loss via
    /// a tail-loss probe or an RTO. Pacing still sets the send rate, so this
    /// does not make a flow send faster. Omitted = 4, the floor this project
    /// always had; only consulted while M2 (`adaptive-cwnd`) is on.
    #[serde(default = "default_min_cwnd_packets")]
    pub min_cwnd_packets: u32,
    pub adaptive_cwnd: AdaptiveCwndConfig,
    pub loss_classifier: LossClassifierConfig,
    #[serde(default)]
    pub rack_tuning: RackTuningConfig,
    #[serde(default)]
    pub rack_rto: RackRtoConfig,
    #[serde(default)]
    pub retransmit_dscp: RetransmitDscpConfig,
    /// Defaulted so an installed 0.2.0 configuration (never overwritten on
    /// upgrade, and without the table) still gets the guard.
    #[serde(default)]
    pub guard: GuardConfig,
    pub runtime: RuntimeConfig,
}

impl SkylineConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|source| ConfigError::Read(path.to_path_buf(), source))?;
        let config: Self = toml::from_str(&content).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.generation == 0 {
            return Err(ConfigError::Invalid("generation must be non-zero"));
        }
        if self.fallback_cc.is_empty() || self.fallback_cc.len() >= 16 {
            return Err(ConfigError::Invalid(
                "fallback_cc must contain between 1 and 15 bytes",
            ));
        }
        if self.max_cwnd_packets < 4 {
            return Err(ConfigError::Invalid("max_cwnd_packets must be at least 4"));
        }
        // The BPF side takes max(min_cwnd_packets, SKYLINE_MIN_CWND), so a
        // smaller value would be silently ignored -- reject it here instead.
        if self.min_cwnd_packets < MIN_CWND_FLOOR {
            return Err(ConfigError::Invalid("min_cwnd_packets must be at least 4"));
        }
        // skyline_set_cwnd_target() applies the cap first and the floor last;
        // a floor above the cap would undo the cap on every ACK.
        if self.min_cwnd_packets > self.max_cwnd_packets {
            return Err(ConfigError::Invalid(
                "min_cwnd_packets must not exceed max_cwnd_packets",
            ));
        }
        if !(1..=10).contains(&self.adaptive_cwnd.bw_window_rtts) {
            return Err(ConfigError::Invalid(
                "bw_window_rtts must be between 1 and 10",
            ));
        }
        if self.feature_mask().contains(FeatureMask::PACING) && self.max_pacing_mbps == 0 {
            return Err(ConfigError::Invalid(
                "max_pacing_mbps must be non-zero while pacing is enabled",
            ));
        }
        if !(0.0..=1.0).contains(&self.adaptive_cwnd.startup_growth_ratio) {
            return Err(ConfigError::Ratio(
                "startup_growth_ratio",
                self.adaptive_cwnd.startup_growth_ratio,
            ));
        }
        // Ceiling matches SKYLINE_LOSS_INFLATION_MAX_PERMILLE (500 permille,
        // 1/(1-0.5) = 2.0x) on the BPF side -- see
        // skyline_loss_inflation_permille()'s doc comment in bpf/skyline_cc.bpf.c.
        if !(0.0..=0.5).contains(&self.loss_classifier.loss_inflation_max_ratio) {
            return Err(ConfigError::Ratio(
                "loss_inflation_max_ratio",
                self.loss_classifier.loss_inflation_max_ratio,
            ));
        }
        // Every mode gain must be >= 1.0 -- fairness is not a design goal;
        // nothing here is meant to ever send slower than the neutral rate.
        if self.adaptive_cwnd.startup_gain < 1.0
            || self.adaptive_cwnd.cruise_inflight_gain < 1.0
            || self.adaptive_cwnd.cruise_pacing_gain < 1.0
        {
            return Err(ConfigError::Invalid(
                "adaptive_cwnd gains must all be >= 1.0",
            ));
        }
        // The guardrail gain is the one deliberate exception to "gains
        // are always >= 1.0" above -- it exists specifically to cut below
        // neutral when a real congestion signal fires. 0.0 = unset (neutral).
        if !(0.0..=1.0).contains(&self.adaptive_cwnd.guardrail_gain) {
            return Err(ConfigError::Ratio(
                "guardrail_gain",
                self.adaptive_cwnd.guardrail_gain,
            ));
        }
        // 0.0 disables the relative guardrail (today's behavior); negative
        // would silently underflow permille() on the BPF side.
        if self.max_queue_delay_ratio < 0.0 {
            return Err(ConfigError::Invalid(
                "max_queue_delay_ratio must be non-negative",
            ));
        }
        if let Some(value) = self.rack_tuning.tcp_recovery {
            if !matches!(value, 1 | 3 | 5 | 7) {
                return Err(ConfigError::Range(
                    "tcp_recovery",
                    "one of 1, 3, 5, 7",
                    value,
                ));
            }
        }
        if let Some(value) = self.rack_tuning.tcp_reordering {
            if !(1..=300).contains(&value) {
                return Err(ConfigError::Range("tcp_reordering", "1..=300", value));
            }
        }
        if let Some(value) = self.rack_tuning.tcp_early_retrans {
            if value > 4 {
                return Err(ConfigError::Range("tcp_early_retrans", "0..=4", value));
            }
        }
        if self.rack_rto.enabled {
            if self.rack_rto.srtt_permille == 0 {
                return Err(ConfigError::Invalid(
                    "rack_rto.srtt_permille must be non-zero while enabled",
                ));
            }
            if self.rack_rto.floor_us == 0 {
                return Err(ConfigError::Invalid("rack_rto.floor_us must be non-zero"));
            }
            if self.rack_rto.ceiling_us < self.rack_rto.floor_us {
                return Err(ConfigError::Invalid(
                    "rack_rto.ceiling_us must be >= rack_rto.floor_us",
                ));
            }
            if self.rack_rto.ceiling_us > SKYLINE_RTO_MIN_KERNEL_DEFAULT_US {
                return Err(ConfigError::Range(
                    "rack_rto.ceiling_us",
                    "<=200000 (kernel TCP_RTO_MIN)",
                    self.rack_rto.ceiling_us,
                ));
            }
            if self.rack_rto.rto_max_congested_permille < self.rack_rto.rto_max_normal_permille {
                return Err(ConfigError::Invalid(
                    "rack_rto.rto_max_congested_permille must be >= rto_max_normal_permille",
                ));
            }
        }
        if self.retransmit_dscp.enabled && self.retransmit_dscp.dscp_value == 0 {
            // Not a no-op: enabling this feature with dscp_value=0 actively
            // stamps DSCP 0 on every retransmit, stripping any DSCP the
            // socket may already carry. 0 as a placeholder value belongs to
            // the disabled state, not the enabled one.
            return Err(ConfigError::Invalid(
                "retransmit_dscp.enabled requires a non-zero dscp_value (0 = unset)",
            ));
        }
        if self.retransmit_dscp.dscp_value > 63 {
            return Err(ConfigError::Range(
                "retransmit_dscp.dscp_value",
                "0..=63",
                u32::from(self.retransmit_dscp.dscp_value),
            ));
        }
        if self.guard.interval_s > MAX_GUARD_INTERVAL_S {
            return Err(ConfigError::Range(
                "guard.interval_s",
                "0..=3600 (0 = no periodic re-check)",
                self.guard.interval_s,
            ));
        }
        Ok(())
    }

    pub fn feature_mask(&self) -> FeatureMask {
        let mut mask = self
            .enabled_modules
            .iter()
            .fold(FeatureMask::empty(), |mask, module| mask | module.bit());
        if self.prr_pacing_enabled {
            mask |= FeatureMask::PRR;
        }
        if self.auto_pacing_enabled {
            mask |= FeatureMask::AUTO_PACING;
        }
        mask
    }

    pub fn kernel_config(&self) -> KernelConfig {
        KernelConfig {
            abi_version: ABI_VERSION,
            generation: self.generation,
            feature_mask: self.feature_mask().bits(),
            initial_cwnd_packets: self.initial_cwnd_packets,
            max_pacing_bps: self.max_pacing_mbps.saturating_mul(1_000_000),
            max_cwnd_packets: self.max_cwnd_packets,
            max_queue_delay_us: self.max_queue_delay_ms.saturating_mul(1_000),
            max_queue_delay_permille: permille(self.max_queue_delay_ratio),
            min_rtt_window_us: self
                .adaptive_cwnd
                .min_rtt_window_s
                .saturating_mul(1_000_000),
            bw_window_rtts: self.adaptive_cwnd.bw_window_rtts,
            startup_plateau_rtts: self.adaptive_cwnd.startup_plateau_rtts,
            startup_growth_permille: permille(self.adaptive_cwnd.startup_growth_ratio),
            startup_gain_permille: permille(self.adaptive_cwnd.startup_gain),
            cruise_inflight_permille: permille(self.adaptive_cwnd.cruise_inflight_gain),
            cruise_pacing_permille: permille(self.adaptive_cwnd.cruise_pacing_gain),
            loss_inflation_max_permille: permille(self.loss_classifier.loss_inflation_max_ratio),
            guardrail_gain_permille: permille(self.adaptive_cwnd.guardrail_gain),
            min_cwnd_packets: self.min_cwnd_packets,
            reserved: 0,
        }
    }
}

fn permille(value: f64) -> u32 {
    (value * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32
}

fn default_true() -> bool {
    true
}

/// `SKYLINE_MIN_CWND` in `bpf/skyline_cc.bpf.c` -- the fixed floor the M2-off
/// path keeps, and the lowest value `min_cwnd_packets` may take.
pub const MIN_CWND_FLOOR: u32 = 4;

fn default_min_cwnd_packets() -> u32 {
    MIN_CWND_FLOOR
}

/// See `RuntimeConfig::events_max_mib`.
pub const DEFAULT_EVENTS_MAX_MIB: u32 = 8;

fn default_events_max_mib() -> u32 {
    DEFAULT_EVENTS_MAX_MIB
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct KernelConfig {
    pub abi_version: u32,
    pub generation: u32,
    pub feature_mask: u32,
    /// See `SkylineConfig::initial_cwnd_packets`.
    pub initial_cwnd_packets: u32,
    pub max_pacing_bps: u64,
    pub max_cwnd_packets: u32,
    pub max_queue_delay_us: u32,
    /// See `SkylineConfig::max_queue_delay_ratio`.
    pub max_queue_delay_permille: u32,
    pub min_rtt_window_us: u32,
    pub bw_window_rtts: u32,
    pub startup_plateau_rtts: u32,
    pub startup_growth_permille: u32,
    /// See `AdaptiveCwndConfig::startup_gain`.
    pub startup_gain_permille: u32,
    /// See `AdaptiveCwndConfig::cruise_inflight_gain`.
    pub cruise_inflight_permille: u32,
    /// See `AdaptiveCwndConfig::cruise_pacing_gain`.
    pub cruise_pacing_permille: u32,
    /// See `LossClassifierConfig::loss_inflation_max_ratio`.
    pub loss_inflation_max_permille: u32,
    /// See `AdaptiveCwndConfig::guardrail_gain`. Reuses the slot that used
    /// to be unused padding (`reserved`, always 0) -- struct size and field
    /// count are unchanged, see `bpf/include/skyline_abi.h`'s top-of-file
    /// comment.
    pub guardrail_gain_permille: u32,
    /// See `SkylineConfig::min_cwnd_packets`.
    pub min_cwnd_packets: u32,
    /// Explicit tail padding -- the `u64` above makes the struct 8-byte
    /// aligned and `Pod` rejects implicit padding. Always 0; mirrors
    /// `struct skyline_config`'s `reserved`.
    pub reserved: u32,
}

/// Mirrors `struct skyline_rto_tuning` (`bpf/include/skyline_abi.h`). Independent of
/// `KernelConfig`/`ABI_VERSION` -- see `RackRtoConfig` doc comment. Includes
/// the three rto_max_* fields for the TCP_RTO_MAX_MS ceiling feature.
pub const SKYLINE_RTO_TUNING_ABI_VERSION: u32 = 2;
pub const SKYLINE_RTO_MIN_KERNEL_DEFAULT_US: u32 = 200_000;
pub const SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS: u32 = 120_000;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct KernelRtoTuning {
    pub abi_version: u32,
    pub generation: u32,
    pub enabled: u32,
    pub srtt_permille: u32,
    pub floor_us: u32,
    pub ceiling_us: u32,
    pub warmup_samples: u32,
    pub rto_max_normal_permille: u32,
    pub rto_max_congested_permille: u32,
    pub rto_max_congestion_ratio_permille: u32,
    pub reserved: u32,
}

/// Mirrors `struct skyline_retransmit_dscp_config` (`bpf/include/skyline_abi.h`).
/// Independent of `KernelConfig`/`ABI_VERSION` and of
/// `SKYLINE_RTO_TUNING_ABI_VERSION` -- see `RetransmitDscpConfig` doc comment.
pub const SKYLINE_RETRANSMIT_DSCP_ABI_VERSION: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct KernelRetransmitDscpConfig {
    pub abi_version: u32,
    pub enabled: u32,
    pub dscp_value: u32,
    pub reserved: u32,
}

/// Mirrors `struct skyline_retransmit_dscp_stats` (`bpf/include/skyline_abi.h`).
/// Field order must match the C struct exactly -- `#[repr(C)]` + `Pod`
/// reads this back from a raw kernel map value by byte layout, not by name.
/// `ipv6_marked`/`ipv6_chain_bailout` were added when dual-stack support
/// landed (ABI 1 -> 2); see the C struct's doc comment for what each field
/// means, especially why `csum_fixups` is structurally 0 for IPv6 traffic
/// and why `ipv6_marked` is a sub-count of `retransmits_marked` rather than
/// a parallel v4/v6 struct pair.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable, Serialize, Deserialize)]
pub struct RetransmitDscpStats {
    pub packets_seen: u64,
    pub retransmits_detected: u64,
    pub retransmits_marked: u64,
    pub csum_fixups: u64,
    pub abi_mismatch: u64,
    pub ipv6_marked: u64,
    pub ipv6_chain_bailout: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SkylineEvent {
    pub timestamp_ns: u64,
    pub socket_cookie: u64,
    pub value_a: u64,
    pub value_b: u64,
    pub event_type: u32,
    pub state: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable, Serialize, Deserialize)]
pub struct TcStats {
    pub packets: u64,
    pub bytes: u64,
    pub gso_packets: u64,
    pub drops: u64,
}

/// Mirrors `struct skyline_rto_stats` (`bpf/include/skyline_abi.h`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable, Serialize, Deserialize)]
pub struct RackRtoStats {
    pub rtt_callbacks: u64,
    pub applied: u64,
    pub rejected: u64,
    pub skipped_warmup: u64,
    pub unchanged: u64,
    /// Diagnostic counters (see bpf/skyline_policy.bpf.c): how many
    /// ESTABLISHED_CB events were observed, and whether the RTT_CB
    /// subscription call itself succeeded -- lets a caller distinguish "no
    /// eligible connections yet" from "subscription itself is failing"
    /// when `rtt_callbacks` stays at 0.
    pub established_cb: u64,
    pub subscribe_ok: u64,
    pub subscribe_err: u64,
    /// TCP_RTO_MAX_MS (ceiling) counterparts of applied/rejected/unchanged
    /// above. rto_max_congested counts how many of those applications used
    /// the congested-k branch (diagnostic only).
    pub rto_max_applied: u64,
    pub rto_max_rejected: u64,
    pub rto_max_unchanged: u64,
    pub rto_max_congested: u64,
}

/// Mirrors `struct skyline_metrics` (`bpf/include/skyline_abi.h`) -- percpu counters
/// updated by `skyline_cc.bpf.c` on every ack/loss/pacing/transition decision.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable, Serialize, Deserialize)]
pub struct SkylineMetrics {
    pub ack_events: u64,
    pub delivered_packets: u64,
    pub loss_events: u64,
    pub state_transitions: u64,
    pub pacing_updates: u64,
    pub guardrail_hits: u64,
    pub hypothetical_early_loss: u64,
    /// See `bpf/include/skyline_abi.h`'s doc comment on the field of the same
    /// name -- nonzero here for an M2-on profile means the PRR-bypass
    /// did not take effect.
    pub prr_adjustments: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityReport {
    pub kernel_release: String,
    pub btf: bool,
    pub bpffs: bool,
    pub cgroup_v2: bool,
    pub fq_available: bool,
    pub struct_ops: bool,
    pub rack_reo_hook: bool,
    pub fallback_cc_available: bool,
    pub notes: Vec<String>,
}

/// Live view of the tier-1 sysctl knobs: `managed` is what `[rack_tuning]`
/// declares (what skyline-speederd will/would apply), `live` is a fresh re-read of
/// `/proc/sys/net/ipv4/*` taken at status time (reflects whatever the
/// experiment harness most recently wrote, if skyline-speederd itself owns nothing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RackTuningStatus {
    pub managed: RackTuningConfig,
    pub live: RackTuningConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RackRtoStatus {
    pub config: RackRtoConfig,
    pub stats: Option<RackRtoStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetransmitDscpStatus {
    pub config: RetransmitDscpConfig,
    pub stats: Option<RetransmitDscpStats>,
}

/// `[guard]` at run time (see `GuardConfig`). JSON only, not ABI.
/// Defaulted field by field so a newer ssctl can decode an older daemon's
/// reply, and the other way round.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardStatus {
    /// True between a successful `Enable` and the next `Drain`. Starts false
    /// at daemon start even when a killed previous instance left skyline_cc
    /// registered: only an `Enable` in this process arms it.
    pub armed: bool,
    /// Echo of `[guard]`.
    pub interval_s: u32,
    pub qdisc: bool,
    /// Periodic re-checks run while armed (the pass `Enable` runs itself is
    /// not counted here).
    pub checks: u64,
    /// Times `tcp_congestion_control` was put back to skyline_cc.
    pub cc_restored: u64,
    /// Times `default_qdisc` was put back to fq.
    pub default_qdisc_restored: u64,
    /// Times the root qdisc of a managed device (`runtime.tc_interface`, or
    /// a NIC under it -- see `GuardLive::devices`) was replaced.
    pub interface_qdisc_replaced: u64,
    /// Human-readable, e.g. "tcp_congestion_control bbr -> skyline_cc".
    pub last_correction: Option<String>,
    pub last_correction_unix_s: Option<u64>,
    /// Most recent failure (tc missing or stuck, a write refused, a replace
    /// that failed or did not take). Kept until the next one: it is history,
    /// `notes` is now.
    pub last_error: Option<String>,
    /// Fresh read at status time.
    pub live: GuardLive,
    /// What the latest pass found and deliberately did not change, and what
    /// it could not do -- e.g. "eth0 root qdisc htb looks deliberate; left
    /// alone", "eth0 root qdisc cake: the last replace failed; retrying in
    /// 30 s".
    pub notes: Vec<String>,
}

/// The values `GuardStatus` is about, read fresh at status time whether or
/// not the guard is armed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardLive {
    pub tcp_congestion_control: String,
    pub default_qdisc: String,
    /// `runtime.tc_interface`.
    pub interface: Option<String>,
    /// Summary of that interface's own root qdisc: "fq", "mq/fq", "cake",
    /// "mq/cake,fq" (mq followed by its children's distinct kinds, sorted),
    /// "noqueue" on a VLAN/bond/bridge. `None` when there is no interface or
    /// `tc` could not be read.
    pub interface_qdisc: Option<String>,
    /// The devices whose root qdisc the guard manages, with their live
    /// summaries: `[tc_interface]` itself, or -- when its root is noqueue --
    /// the NICs found under it through lower_* links (bond slaves, a VLAN's
    /// real device, a bridge's physical ports; never a tap or veth). Empty
    /// with `[guard] qdisc = false`, for a tunnel with no NIC under it, and
    /// from a daemon older than the field.
    #[serde(default)]
    pub devices: Vec<GuardDevice>,
}

/// One entry of `GuardLive::devices`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardDevice {
    pub name: String,
    /// Same format as `GuardLive::interface_qdisc`; `None` when `tc` could
    /// not read it or it shows no root qdisc (a device that is down).
    pub qdisc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    /// The daemon's own version (`CARGO_PKG_VERSION`). Empty when decoded
    /// from a daemon older than the field.
    #[serde(default)]
    pub version: String,
    pub enabled: bool,
    pub generation: u32,
    pub modules: Vec<Module>,
    pub fallback_cc: String,
    pub active_flows: u64,
    pub tc_stats: Option<TcStats>,
    /// `None` when Skyline Speeder CC has never been enabled (mirrors `tc_stats`'s
    /// `Option` -- there is no `skyline_cc.bpf.o` loaded to read the map from).
    pub metrics: Option<SkylineMetrics>,
    pub rack_tuning: RackTuningStatus,
    pub rack_rto: RackRtoStatus,
    pub retransmit_dscp: RetransmitDscpStatus,
    /// Live echo of the current M2/M3/M4 coefficients (see
    /// `ModuleTuningConfig`). Unlike `rack_tuning`'s `managed`/`live` split,
    /// a single value suffices here: these fields live entirely in `skyline-speederd`'s
    /// own memory and are pushed atomically to the BPF config map, so there
    /// is no external sysctl-style tampering vector to reconcile against.
    pub module_tuning: ModuleTuningConfig,
    pub capabilities: CapabilityReport,
    /// `[guard]` state; defaulted (disarmed, zero counters) when decoded from
    /// a daemon older than the guard.
    #[serde(default)]
    pub guard: GuardStatus,
    /// Seconds since this `skyline-speederd` process started. 0 from a
    /// daemon older than the field.
    #[serde(default)]
    pub uptime_s: u64,
    /// Seconds since the `Enable` that attached the struct_ops currently
    /// loaded. `None` while nothing is attached, and from a daemon older
    /// than the field.
    #[serde(default)]
    pub attached_s: Option<u64>,
}

/// One TCP connection the kernel currently runs on `skyline_cc`, as
/// `ss -tin` reports it.
///
/// Every measurement is the kernel's own (`struct tcp_info` plus what
/// iproute2 derives from it), not skyline_cc's view of the flow: the BPF
/// side keeps its per-flow state in an `SK_STORAGE` map, which user space
/// cannot enumerate without a socket file descriptor. What the two have in
/// common -- cwnd, pacing rate, RTT -- is exactly the part skyline_cc
/// writes, so this is a faithful picture of what the acceleration did,
/// arrived at from the other end.
///
/// Every field but `peer`/`local`/`state` is `Option`: iproute2 prints a
/// key only when the kernel has a value for it, and which keys exist has
/// changed between versions. A missing one renders as `-` rather than as a
/// zero that would read like a measurement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowRow {
    /// `address:port`, numeric (`ss -n`); IPv6 in brackets.
    pub peer: String,
    pub local: String,
    /// `ESTAB`, `CLOSE-WAIT`, ... as iproute2 spells it.
    pub state: String,
    /// Smoothed RTT and its variation, milliseconds (`rtt:13.5/6.2`).
    pub rtt_ms: Option<f64>,
    pub rtt_var_ms: Option<f64>,
    /// `minrtt:` -- the floor skyline_cc's BDP target is built on.
    pub min_rtt_ms: Option<f64>,
    /// Congestion window, packets. What M2 drives.
    pub cwnd_packets: Option<u64>,
    pub ssthresh: Option<u64>,
    /// Pacing rate in bits per second. What M2/M4 drive.
    pub pacing_bps: Option<u64>,
    /// Kernel's delivery-rate estimate, bits per second.
    pub delivery_bps: Option<u64>,
    /// Kernel's send-rate estimate (cwnd/rtt), bits per second.
    pub send_bps: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub bytes_acked: Option<u64>,
    pub bytes_retrans: Option<u64>,
    /// Cumulative retransmitted segments (`retrans:0/12`'s second number).
    pub retrans_total: Option<u64>,
    pub unacked: Option<u64>,
    pub mss: Option<u32>,
    /// Current retransmission timeout, milliseconds. M1 tier-2 moves its
    /// floor and ceiling, so this is where that shows up.
    pub rto_ms: Option<f64>,
}

impl FlowRow {
    /// Retransmitted share of what was sent, 0.0-1.0, from the byte
    /// counters. `None` unless both are known and something was sent.
    pub fn retransmit_ratio(&self) -> Option<f64> {
        match (self.bytes_sent, self.bytes_retrans) {
            (Some(sent), Some(retrans)) if sent > 0 => Some(retrans as f64 / sent as f64),
            _ => None,
        }
    }
}

/// What `ssctl flows` reports: the accelerated connections and how much of
/// the host's TCP traffic they are.
///
/// `accelerated` is capped (see `FLOW_ROWS_MAX`); `on_skyline_cc` is the
/// full count either way, so a host with thousands of connections still
/// reports the truth about how many are accelerated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowReport {
    /// The accelerated connections, most bytes sent first, capped at
    /// `FLOW_ROWS_MAX`.
    pub accelerated: Vec<FlowRow>,
    /// Connected TCP sockets on the host, whatever congestion control they
    /// use. Listening sockets are not counted.
    pub tcp_total: u64,
    /// How many of those the kernel runs on `skyline_cc`. Can exceed
    /// `accelerated.len()`, which is capped.
    pub on_skyline_cc: u64,
    /// Rows left out of `accelerated` by the cap.
    pub truncated: u64,
    /// Why the enumeration is empty or partial: `ss` missing, killed at its
    /// timeout, or output that could not be parsed. `accelerated` is then
    /// empty and the counts are 0, but the rest of the report (the
    /// coefficients in force, the BPF counters) is still valid -- those come
    /// from the daemon, not from `ss`.
    pub source_error: Option<String>,
}

/// How many `FlowRow`s a `FlowReport` carries at most. A terminal cannot
/// show more, and the counts above stay exact regardless.
pub const FLOW_ROWS_MAX: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Request {
    Validate,
    Enable {
        modules: Option<Vec<Module>>,
    },
    DisableModule {
        module: Module,
    },
    Status,
    Flows,
    Snapshot {
        path: PathBuf,
    },
    Drain {
        timeout_s: u64,
    },
    SetRackRto {
        config: RackRtoConfig,
    },
    ResetRackRto,
    SetModuleConfig {
        config: ModuleTuningConfig,
    },
    ResetModuleConfig,
    SetRetransmitDscp {
        config: RetransmitDscpConfig,
    },
    ResetRetransmitDscp,
    /// Sent by skyline-speederd's own SIGTERM/SIGINT handler (via a loopback connection
    /// to its own control socket) to unblock the blocking accept loop in
    /// `serve()` -- see that function's doc comment for why a real request
    /// is used instead of a shutdown flag. Handling it is a no-op beyond
    /// acknowledging the request; the actual struct_ops/link cleanup runs
    /// via `Daemon`'s field `Drop` impls once `serve()` returns and the
    /// daemon value goes out of scope.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub status: Option<RuntimeStatus>,
    /// Only `Request::Flows` fills this in: enumerating sockets costs an
    /// `ss` of its own, which every other request would pay for nothing.
    /// Defaulted on the wire, so a daemon older than the field still
    /// decodes here and a newer daemon's reply still decodes in an older
    /// `ssctl`.
    #[serde(default)]
    pub flows: Option<FlowReport>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("invalid TOML: {0}")]
    Parse(toml::de::Error),
    #[error("unknown module {0}")]
    UnknownModule(String),
    #[error("{0} must be between zero and one, got {1}")]
    Ratio(&'static str, f64),
    #[error("{0} must be {1}, got {2}")]
    Range(&'static str, &'static str, u32),
    #[error("{0}")]
    Invalid(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_has_expected_mask() {
        let config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        assert!(config.feature_mask().contains(FeatureMask::ADAPTIVE_CWND));
        // early-loss (M1) is on by default alongside the other three
        // modules -- see config/speeder.toml's enabled_modules comment.
        assert!(config.feature_mask().contains(FeatureMask::EARLY_LOSS));
        // See config/speeder.toml's comment for the default coefficients.
        assert_eq!(config.kernel_config().cruise_pacing_permille, 1250);
    }

    #[test]
    fn guest_configuration_uses_installed_bpf_path() {
        let config =
            SkylineConfig::load("../../config/speeder-guest.toml").expect("load guest config");
        assert_eq!(
            config.runtime.bpf_dir,
            PathBuf::from("/opt/skyline-speeder/bpf")
        );
        assert_eq!(config.runtime.tc_interface.as_deref(), Some("data0"));
    }

    #[test]
    fn event_log_stays_bounded_when_config_predates_the_cap() {
        // An installed /etc/skyline-speeder/speeder.toml is never overwritten,
        // so the hosts that hit the unbounded log will not gain the field on
        // upgrade -- they must get the cap anyway, not "unlimited".
        let content =
            fs::read_to_string("../../config/speeder-guest.toml").expect("read guest config");
        let legacy: String = content
            .lines()
            .filter(|line| !line.trim_start().starts_with("events_max_mib"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(
            legacy, content,
            "guest config should declare events_max_mib"
        );
        let config: SkylineConfig = toml::from_str(&legacy).expect("parse legacy config");
        assert_eq!(config.runtime.events_max_mib, DEFAULT_EVENTS_MAX_MIB);

        for path in [
            "../../config/speeder.toml",
            "../../config/speeder-guest.toml",
        ] {
            let shipped = SkylineConfig::load(path).expect("load shipped config");
            assert_eq!(
                shipped.runtime.events_max_mib, DEFAULT_EVENTS_MAX_MIB,
                "{path}"
            );
        }
    }

    const SHIPPED_CONFIGS: [&str; 2] = [
        "../../config/speeder.toml",
        "../../config/speeder-guest.toml",
    ];

    #[test]
    fn guard_defaults_apply_when_config_predates_the_table() {
        // Same situation as events_max_mib: an installed
        // /etc/skyline-speeder/speeder.toml from 0.2.0 has no [guard] and is
        // never overwritten, and it must get the guard anyway.
        for path in SHIPPED_CONFIGS {
            let content = fs::read_to_string(path).expect("read config");
            let mut value: toml::Value = toml::from_str(&content).expect("parse as TOML");
            value
                .as_table_mut()
                .expect("top-level table")
                .remove("guard")
                .unwrap_or_else(|| panic!("{path} should declare [guard]"));
            let legacy: SkylineConfig = value.try_into().expect("parse without [guard]");
            legacy.validate().expect("still valid");
            assert_eq!(legacy.guard, GuardConfig::default(), "{path}");
        }
    }

    #[test]
    fn shipped_configs_declare_the_guard_defaults() {
        assert_eq!(
            GuardConfig::default(),
            GuardConfig {
                interval_s: DEFAULT_GUARD_INTERVAL_S,
                qdisc: true,
            }
        );
        for path in SHIPPED_CONFIGS {
            // Declared explicitly, not merely defaulted: the file is where an
            // operator looks for the knob.
            let content = fs::read_to_string(path).expect("read config");
            let value: toml::Value = toml::from_str(&content).expect("parse as TOML");
            let table = value
                .get("guard")
                .and_then(toml::Value::as_table)
                .unwrap_or_else(|| panic!("{path} should declare [guard]"));
            assert!(table.contains_key("interval_s"), "{path}");
            assert!(table.contains_key("qdisc"), "{path}");

            let shipped = SkylineConfig::load(path).expect("load shipped config");
            assert_eq!(shipped.guard, GuardConfig::default(), "{path}");
        }
    }

    #[test]
    fn guard_interval_is_bounded() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.guard.interval_s = MAX_GUARD_INTERVAL_S + 1;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Range("guard.interval_s", _, 3601))
        ));

        config.guard.interval_s = MAX_GUARD_INTERVAL_S;
        config.validate().expect("an hour is the upper bound");
        config.guard.interval_s = 0;
        config
            .validate()
            .expect("0 turns the periodic re-check off");
    }

    #[test]
    fn guard_status_decodes_from_a_partial_table() {
        // A daemon one field behind (or ahead) must not make the whole status
        // undecodable.
        let status: GuardStatus =
            toml::from_str("armed = true\ncc_restored = 2\n").expect("parse partial status");
        assert!(status.armed);
        assert_eq!(status.cc_restored, 2);
        assert_eq!(status.live, GuardLive::default());
        assert!(status.last_error.is_none());

        // A daemon from before `live.devices`.
        let live: GuardLive = toml::from_str(
            "tcp_congestion_control = \"skyline_cc\"\ninterface = \"eth0\"\n\
             interface_qdisc = \"mq/fq\"\n",
        )
        .expect("parse live without devices");
        assert!(live.devices.is_empty());
        assert_eq!(live.interface_qdisc.as_deref(), Some("mq/fq"));
    }

    #[test]
    fn kernel_limits_saturate_instead_of_wrapping() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.max_pacing_mbps = u64::MAX;
        config.max_queue_delay_ms = u32::MAX;
        let kernel = config.kernel_config();
        assert_eq!(kernel.max_pacing_bps, u64::MAX);
        assert_eq!(kernel.max_queue_delay_us, u32::MAX);
    }

    #[test]
    fn guest_config_never_owns_global_sysctls() {
        let config =
            SkylineConfig::load("../../config/speeder-guest.toml").expect("load guest config");
        assert!(config.rack_tuning.tcp_recovery.is_none());
        assert!(config.rack_tuning.tcp_reordering.is_none());
        assert!(config.rack_tuning.tcp_early_retrans.is_none());
    }

    #[test]
    fn rack_tuning_rejects_out_of_range_values() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.rack_tuning.tcp_recovery = Some(2);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Range("tcp_recovery", _, 2))
        ));

        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.rack_tuning.tcp_reordering = Some(301);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Range("tcp_reordering", _, 301))
        ));

        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.rack_tuning.tcp_early_retrans = Some(5);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Range("tcp_early_retrans", _, 5))
        ));
    }

    #[test]
    fn rack_rto_validate_requires_consistent_bounds_when_enabled() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.rack_rto = RackRtoConfig {
            enabled: true,
            srtt_permille: 1000,
            floor_us: 5000,
            ceiling_us: 1000,
            warmup_samples: 8,
            rto_max_normal_permille: 0,
            rto_max_congested_permille: 0,
            rto_max_congestion_ratio_permille: 0,
        };
        assert!(config.validate().is_err());

        config.rack_rto.ceiling_us = 50_000;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rack_rto_validate_requires_congested_ge_normal_when_enabled() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.rack_rto = RackRtoConfig {
            enabled: true,
            rto_max_normal_permille: 3000,
            rto_max_congested_permille: 2000,
            ..RackRtoConfig::disabled()
        };
        assert!(config.validate().is_err());

        config.rack_rto.rto_max_congested_permille = 4000;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rack_rto_kernel_config_clamps_ceiling_to_kernel_default() {
        let config = RackRtoConfig {
            enabled: true,
            srtt_permille: 1000,
            floor_us: 5000,
            ceiling_us: u32::MAX,
            warmup_samples: 8,
            rto_max_normal_permille: 0,
            rto_max_congested_permille: 0,
            rto_max_congestion_ratio_permille: 0,
        };
        let kernel = config.kernel_config(3);
        assert_eq!(kernel.abi_version, SKYLINE_RTO_TUNING_ABI_VERSION);
        assert_eq!(kernel.generation, 3);
        assert_eq!(kernel.ceiling_us, SKYLINE_RTO_MIN_KERNEL_DEFAULT_US);
    }

    #[test]
    fn retransmit_dscp_validate_rejects_enabled_with_zero_value() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.retransmit_dscp = RetransmitDscpConfig {
            enabled: true,
            dscp_value: 0,
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Invalid(
                "retransmit_dscp.enabled requires a non-zero dscp_value (0 = unset)"
            ))
        ));

        config.retransmit_dscp.dscp_value = 10;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn retransmit_dscp_validate_rejects_out_of_range_value() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        // Disabled, out-of-range value is still rejected -- the field is
        // meaningless above 63 (6-bit DSCP) regardless of enabled.
        config.retransmit_dscp = RetransmitDscpConfig {
            enabled: false,
            dscp_value: 64,
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Range("retransmit_dscp.dscp_value", _, 64))
        ));
    }

    #[test]
    fn retransmit_dscp_kernel_config_round_trips() {
        let config = RetransmitDscpConfig {
            enabled: true,
            dscp_value: 26,
        };
        let kernel = config.kernel_config();
        assert_eq!(kernel.abi_version, SKYLINE_RETRANSMIT_DSCP_ABI_VERSION);
        assert_eq!(kernel.enabled, 1);
        assert_eq!(kernel.dscp_value, 26);
        assert_eq!(kernel.reserved, 0);
    }

    #[test]
    fn retransmit_dscp_disabled_by_default() {
        let config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        assert!(!config.retransmit_dscp.enabled);
        assert_eq!(config.retransmit_dscp.dscp_value, 0);
    }

    #[test]
    fn module_tuning_config_round_trips_through_skyline_config() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let mut tuning = ModuleTuningConfig::from_config(&config);
        // from_config() must recover exactly what's in config/speeder.toml today.
        assert_eq!(tuning.cruise_inflight_gain, 3.0);
        assert_eq!(tuning.startup_gain, 3.0);

        tuning.loss_inflation_max_ratio = 0.4;
        tuning.max_cwnd_packets = 40_000;
        tuning.apply_to(&mut config);
        assert!(config.validate().is_ok());
        assert_eq!(config.loss_classifier.loss_inflation_max_ratio, 0.4);
        assert_eq!(config.max_cwnd_packets, 40_000);

        // Round-trip: extracting again must reproduce the same struct.
        let round_tripped = ModuleTuningConfig::from_config(&config);
        assert_eq!(round_tripped, tuning);
    }

    #[test]
    fn prr_pacing_applies_to_a_zero_modules_baseline_by_default() {
        // b2-skyline-base's whole reason to exist is enabled_modules=[] --
        // PRR must still apply there by default (see FeatureMask::PRR's
        // doc comment: it is not one of the M1-M4 ablation Modules).
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.enabled_modules.clear();
        assert!(config.prr_pacing_enabled);
        assert!(config.feature_mask().contains(FeatureMask::PRR));
        assert!(!config.feature_mask().is_empty());

        let tuning = ModuleTuningConfig::from_config(&config);
        assert!(tuning.prr_pacing_enabled);
    }

    #[test]
    fn prr_pacing_can_be_switched_off_independent_of_modules() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.prr_pacing_enabled = false;
        tuning.apply_to(&mut config);
        assert!(config.validate().is_ok());
        assert!(!config.feature_mask().contains(FeatureMask::PRR));
        // Turning PRR off must not disturb any ablation module bit.
        assert!(config.feature_mask().contains(FeatureMask::ADAPTIVE_CWND));
    }

    #[test]
    fn auto_pacing_applies_to_a_zero_modules_baseline_by_default() {
        // Same rationale as PRR: b2-skyline-base (enabled_modules=[]) must
        // still get the kernel-equivalent pacing-rate ceiling by default.
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.enabled_modules.clear();
        assert!(config.auto_pacing_enabled);
        assert!(config.feature_mask().contains(FeatureMask::AUTO_PACING));

        let tuning = ModuleTuningConfig::from_config(&config);
        assert!(tuning.auto_pacing_enabled);
    }

    #[test]
    fn auto_pacing_can_be_switched_off_independent_of_modules_and_prr() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.auto_pacing_enabled = false;
        tuning.apply_to(&mut config);
        assert!(config.validate().is_ok());
        assert!(!config.feature_mask().contains(FeatureMask::AUTO_PACING));
        assert!(config.feature_mask().contains(FeatureMask::PRR));
        assert!(config.feature_mask().contains(FeatureMask::ADAPTIVE_CWND));
    }

    #[test]
    fn module_tuning_config_kernel_config_carries_initial_cwnd_packets() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.initial_cwnd_packets = 200;
        tuning.apply_to(&mut config);
        assert_eq!(config.kernel_config().initial_cwnd_packets, 200);
    }

    #[test]
    fn min_cwnd_packets_defaults_to_the_historical_floor_when_omitted() {
        // Every configuration file written before this field existed must
        // keep loading, and must keep meaning "4".
        let content = fs::read_to_string("../../config/speeder.toml").expect("read config");
        let without: String = content
            .lines()
            .filter(|line| !line.trim_start().starts_with("min_cwnd_packets"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(
            without.len(),
            content.len(),
            "fixture no longer declares the field"
        );
        let config: SkylineConfig = toml::from_str(&without).expect("parse without the field");
        config.validate().expect("still valid");
        assert_eq!(config.min_cwnd_packets, MIN_CWND_FLOOR);
        assert_eq!(config.kernel_config().min_cwnd_packets, MIN_CWND_FLOOR);
    }

    #[test]
    fn module_tuning_request_without_min_cwnd_packets_still_parses() {
        // `ModuleTuningConfig` is the `set-module-config` wire payload; a
        // request that predates the field must mean today's behavior, not
        // fail to deserialize.
        let config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let wire = toml::to_string(&ModuleTuningConfig::from_config(&config)).expect("serialize");
        let without: String = wire
            .lines()
            .filter(|line| !line.starts_with("min_cwnd_packets"))
            .map(|line| format!("{line}\n"))
            .collect();
        let tuning: ModuleTuningConfig = toml::from_str(&without).expect("parse without the field");
        assert_eq!(tuning.min_cwnd_packets, MIN_CWND_FLOOR);
    }

    #[test]
    fn module_tuning_config_kernel_config_carries_min_cwnd_packets() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.min_cwnd_packets = 16;
        tuning.apply_to(&mut config);
        config
            .validate()
            .expect("16 is within [4, max_cwnd_packets]");
        let kernel = config.kernel_config();
        assert_eq!(kernel.min_cwnd_packets, 16);
        assert_eq!(kernel.reserved, 0);
        assert_eq!(
            ModuleTuningConfig::from_config(&config).min_cwnd_packets,
            16
        );
    }

    #[test]
    fn min_cwnd_packets_must_stay_between_the_fixed_floor_and_the_cap() {
        let base = SkylineConfig::load("../../config/speeder.toml").expect("load config");

        // Below SKYLINE_MIN_CWND the BPF side would silently ignore it.
        let mut config = base.clone();
        config.min_cwnd_packets = MIN_CWND_FLOOR - 1;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        // Above the cap the floor would undo the cap on every ACK.
        let mut config = base.clone();
        config.min_cwnd_packets = config.max_cwnd_packets + 1;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        let mut config = base;
        config.min_cwnd_packets = config.max_cwnd_packets;
        config
            .validate()
            .expect("floor == cap is degenerate but consistent");
    }

    #[test]
    fn kernel_config_has_no_implicit_padding() {
        // 4 x u32, one u64, 14 x u32 -- `Pod` already refuses to compile with
        // implicit padding; this pins the size `struct skyline_config` in
        // bpf/include/skyline_abi.h has to match.
        assert_eq!(std::mem::size_of::<KernelConfig>(), 80);
        assert_eq!(std::mem::align_of::<KernelConfig>(), 8);
    }

    #[test]
    fn module_tuning_rejects_out_of_range_values() {
        let base = SkylineConfig::load("../../config/speeder.toml").expect("load config");

        let mut config = base.clone();
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.cruise_inflight_gain = 0.5;
        tuning.apply_to(&mut config);
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));

        let mut config = base.clone();
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.loss_inflation_max_ratio = 0.9;
        tuning.apply_to(&mut config);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Ratio("loss_inflation_max_ratio", _))
        ));

        let mut config = base;
        let mut tuning = ModuleTuningConfig::from_config(&config);
        tuning.max_cwnd_packets = 1;
        tuning.apply_to(&mut config);
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }
}
