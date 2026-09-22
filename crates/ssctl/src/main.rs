// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use skyline_common::{
    Module, ModuleTuningConfig, RackRtoConfig, Request, Response, RetransmitDscpConfig,
};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(version, about = "Control the Skyline Speeder experimental daemon")]
struct Arguments {
    #[arg(long, default_value = "/run/skyline-speeder/speeder.sock")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Validate,
    /// Attach skyline_cc, make it the host default, and arm the guard.
    ///
    /// The guard puts the congestion control -- and, with [guard] qdisc =
    /// true, net.core.default_qdisc and runtime.tc_interface's root qdisc
    /// (fq) -- back whenever something else changes them, until `drain`. The
    /// response message lists everything this enable corrected.
    Enable {
        #[arg(long, value_delimiter = ',')]
        modules: Option<Vec<Module>>,
        #[arg(long, conflicts_with = "modules")]
        all_off: bool,
    },
    Disable {
        #[arg(long)]
        module: Module,
    },
    Status,
    Flows,
    Snapshot {
        path: PathBuf,
    },
    /// Move new connections back to fallback_cc, disarm the guard, detach.
    ///
    /// Waits up to --timeout seconds for skyline_cc flows to finish before
    /// unregistering it. Qdiscs are left as they are.
    Drain {
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// M1 tier-2: per-flow dynamic TCP_BPF_RTO_MIN tuning. Absolute-replace
    /// semantics -- every field is always sent, so a case can never end up
    /// with an ambiguous mix of old and new values.
    SetRackRto {
        /// Subscribe to BPF_SOCK_OPS_RTT_CB and apply the computed target
        /// (default). Pass --disable to unsubscribe and restore the kernel
        /// default RTO floor instead.
        #[arg(long)]
        disable: bool,
        #[arg(long, default_value_t = 1000)]
        srtt_permille: u32,
        #[arg(long, default_value_t = 5000)]
        floor_us: u32,
        #[arg(long, default_value_t = 200_000)]
        ceiling_us: u32,
        #[arg(long, default_value_t = 8)]
        warmup_samples: u32,
        /// TCP_RTO_MAX_MS ceiling = clamp(this * base_rtt / 1000, 1000ms,
        /// 120000ms) while no congestion evidence has been seen. 0 disables
        /// the whole rto_max feature, independent of the floor args above.
        #[arg(long, default_value_t = 0)]
        rto_max_normal_permille: u32,
        /// Same formula, once a fresh CE mark or growing queue delay is
        /// observed on the flow. Should be >= rto-max-normal-permille.
        #[arg(long, default_value_t = 0)]
        rto_max_congested_permille: u32,
        /// srtt exceeding min_rtt by more than this ratio (permille) counts
        /// as queueing evidence. 0 uses skyline_policy.bpf.c's built-in default
        /// (2000 = 2x).
        #[arg(long, default_value_t = 0)]
        rto_max_congestion_ratio_permille: u32,
    },
    /// Restore [rack_rto] to whatever the configuration file declared at
    /// daemon startup.
    ResetRackRto,
    /// M2 ([adaptive_cwnd]) / M3 ([loss_classifier]) coefficients, plus the
    /// top-level safety limits. Absolute-replace semantics, same as
    /// SetRackRto -- every field is always sent. Defaults below match
    /// config/speeder.toml's current values; the
    /// `set_module_config_defaults_match_the_shipped_config` test fails if
    /// the two drift apart. M2 is a two-mode (STARTUP/CRUISE)
    /// rate-driven controller, M3 a single loss-rate compensation factor
    /// -- see bpf/include/skyline_abi.h's top-of-file comment.
    SetModuleConfig {
        #[arg(long, default_value_t = 1200)]
        max_pacing_mbps: u64,
        #[arg(long, default_value_t = 50_000)]
        max_cwnd_packets: u32,
        #[arg(long, default_value_t = 70)]
        max_queue_delay_ms: u32,
        /// Ratio of base RTT added to max-queue-delay-ms to form the actual
        /// guardrail (max of the two). 0.0 keeps the guardrail exactly at
        /// max-queue-delay-ms.
        #[arg(long, default_value_t = 0.6)]
        max_queue_delay_ratio: f64,
        /// Aggressive initial window (packets). 0 leaves the kernel's
        /// own IW alone.
        #[arg(long, default_value_t = 100)]
        initial_cwnd_packets: u32,
        /// Floor under M2's BDP-derived cwnd target (packets), 4 or more.
        /// Pacing still sets the send rate; this only keeps a thin flow's
        /// window large enough to recover from a loss without an RTO.
        #[arg(long, default_value_t = 4)]
        min_cwnd_packets: u32,
        #[arg(long, default_value_t = 30)]
        min_rtt_window_s: u32,
        #[arg(long, default_value_t = 6)]
        bw_window_rtts: u32,
        #[arg(long, default_value_t = 5)]
        startup_plateau_rtts: u32,
        #[arg(long, default_value_t = 0.20)]
        startup_growth_ratio: f64,
        /// SKYLINE_MODE_STARTUP's single gain (cwnd target and pacing rate both
        /// use it while M2 is on).
        #[arg(long, default_value_t = 3.0)]
        startup_gain: f64,
        /// SKYLINE_MODE_CRUISE's cwnd-target gain.
        #[arg(long, default_value_t = 3.0)]
        cruise_inflight_gain: f64,
        /// SKYLINE_MODE_CRUISE's pacing-rate gain.
        #[arg(long, default_value_t = 1.25)]
        cruise_pacing_gain: f64,
        /// Gain applied to both cwnd and pacing for the rest of a round in
        /// which the queue-delay/ECN guardrail trips. 0.0 keeps the
        /// guardrail neutral (cancels the boost but never cuts below it);
        /// a value below 1.0 makes it a real self-protective cut.
        #[arg(long, default_value_t = 0.8)]
        guardrail_gain: f64,
        /// Ceiling on the measured per-flow loss rate to compensate for when
        /// inflating BDP-derived targets/pacing rate. 0.0 disables this
        /// (inflation always exactly 1.0x).
        #[arg(long, default_value_t = 0.10)]
        loss_inflation_max_ratio: f64,
        /// Turn off the PRR-style continuous recovery-phase rate limiting
        /// (on by default, independent of --modules -- see
        /// SkylineConfig::prr_pacing_enabled). Only exercised on the M2-off
        /// path. Exists for rollback/A-B comparison against the pre-PRR
        /// behavior.
        #[arg(long)]
        disable_prr_pacing: bool,
        /// Turn off the kernel-equivalent pacing-rate ceiling used when M4
        /// is off (on by default, independent of --modules -- see
        /// SkylineConfig::auto_pacing_enabled). Exists for rollback/A-B
        /// comparison.
        #[arg(long)]
        disable_auto_pacing: bool,
    },
    /// Restore [adaptive_cwnd]/[loss_classifier]/the top-level limits to
    /// whatever the configuration file declared at daemon startup.
    ResetModuleConfig,
    /// Mark retransmitted TCP segments with a DSCP codepoint (IPv4 ToS
    /// upper 6 bits) so upstream network equipment can route them
    /// differently -- applies to all TCP flows on runtime.tc_interface,
    /// regardless of which congestion control they use. dscp-value is a
    /// placeholder: the actual codepoint is a network-team policy decision,
    /// not something to hardcode. Absolute-replace semantics, same as
    /// SetRackRto/SetModuleConfig.
    SetRetransmitDscp {
        /// 0-63. Required (rejected by the daemon if left at 0 while the
        /// feature is enabled -- that would actively strip any DSCP the
        /// socket already carries, not a no-op).
        #[arg(long)]
        dscp_value: u8,
    },
    /// Restore [retransmit_dscp] to whatever the configuration file
    /// declared at daemon startup (disabled, unless explicitly configured).
    ResetRetransmitDscp,
}

fn request(command: Command) -> Request {
    match command {
        Command::Validate => Request::Validate,
        Command::Enable { modules, all_off } => Request::Enable {
            modules: if all_off { Some(Vec::new()) } else { modules },
        },
        Command::Disable { module } => Request::DisableModule { module },
        Command::Status => Request::Status,
        Command::Flows => Request::Flows,
        Command::Snapshot { path } => Request::Snapshot { path },
        Command::Drain { timeout } => Request::Drain { timeout_s: timeout },
        Command::SetRackRto {
            disable,
            srtt_permille,
            floor_us,
            ceiling_us,
            warmup_samples,
            rto_max_normal_permille,
            rto_max_congested_permille,
            rto_max_congestion_ratio_permille,
        } => Request::SetRackRto {
            config: RackRtoConfig {
                enabled: !disable,
                srtt_permille,
                floor_us,
                ceiling_us,
                warmup_samples,
                rto_max_normal_permille,
                rto_max_congested_permille,
                rto_max_congestion_ratio_permille,
            },
        },
        Command::ResetRackRto => Request::ResetRackRto,
        Command::SetModuleConfig {
            max_pacing_mbps,
            max_cwnd_packets,
            max_queue_delay_ms,
            max_queue_delay_ratio,
            initial_cwnd_packets,
            min_cwnd_packets,
            min_rtt_window_s,
            bw_window_rtts,
            startup_plateau_rtts,
            startup_growth_ratio,
            startup_gain,
            cruise_inflight_gain,
            cruise_pacing_gain,
            guardrail_gain,
            loss_inflation_max_ratio,
            disable_prr_pacing,
            disable_auto_pacing,
        } => Request::SetModuleConfig {
            config: ModuleTuningConfig {
                max_pacing_mbps,
                max_cwnd_packets,
                max_queue_delay_ms,
                max_queue_delay_ratio,
                initial_cwnd_packets,
                min_cwnd_packets,
                min_rtt_window_s,
                bw_window_rtts,
                startup_plateau_rtts,
                startup_growth_ratio,
                startup_gain,
                cruise_inflight_gain,
                cruise_pacing_gain,
                guardrail_gain,
                loss_inflation_max_ratio,
                prr_pacing_enabled: !disable_prr_pacing,
                auto_pacing_enabled: !disable_auto_pacing,
            },
        },
        Command::ResetModuleConfig => Request::ResetModuleConfig,
        Command::SetRetransmitDscp { dscp_value } => Request::SetRetransmitDscp {
            config: RetransmitDscpConfig {
                enabled: true,
                dscp_value,
            },
        },
        Command::ResetRetransmitDscp => Request::ResetRetransmitDscp,
    }
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let mut stream = UnixStream::connect(&arguments.socket)
        .with_context(|| format!("connect to {}", arguments.socket.display()))?;
    serde_json::to_writer(&mut stream, &request(arguments.command))?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Response = serde_json::from_str(line.trim()).context("decode daemon response")?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if response.ok {
        Ok(())
    } else {
        anyhow::bail!("{}", response.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyline_common::{
        CapabilityReport, GuardStatus, RackRtoStatus, RackTuningConfig, RackTuningStatus,
        RetransmitDscpStatus, RuntimeStatus, SkylineConfig,
    };

    #[test]
    fn version_flag_is_recognised() {
        let error = Arguments::try_parse_from(["ssctl", "--version"])
            .expect_err("--version exits through clap");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    /// A new ssctl talking to a daemon from before `version`/`guard` existed
    /// (an upgrade in progress, a daemon not restarted yet) must still decode
    /// the reply instead of failing on the missing fields.
    #[test]
    fn decodes_a_status_from_a_daemon_older_than_the_guard() {
        let shipped = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        let status = RuntimeStatus {
            version: "0.3.0".to_owned(),
            enabled: true,
            generation: 2,
            modules: shipped.enabled_modules.clone(),
            fallback_cc: shipped.fallback_cc.clone(),
            active_flows: 0,
            tc_stats: None,
            metrics: None,
            rack_tuning: RackTuningStatus {
                managed: RackTuningConfig::default(),
                live: RackTuningConfig::default(),
            },
            rack_rto: RackRtoStatus {
                config: shipped.rack_rto,
                stats: None,
            },
            retransmit_dscp: RetransmitDscpStatus {
                config: shipped.retransmit_dscp,
                stats: None,
            },
            module_tuning: ModuleTuningConfig::from_config(&shipped),
            capabilities: CapabilityReport {
                kernel_release: "6.12.0".to_owned(),
                btf: true,
                bpffs: true,
                cgroup_v2: true,
                fq_available: true,
                struct_ops: true,
                rack_reo_hook: false,
                fallback_cc_available: true,
                notes: Vec::new(),
            },
            guard: GuardStatus {
                armed: true,
                ..GuardStatus::default()
            },
        };
        let response = Response {
            ok: true,
            message: "status returned".to_owned(),
            status: Some(status),
        };
        let mut wire = serde_json::to_value(&response).expect("encode");
        let fields = wire["status"].as_object_mut().expect("status object");
        assert!(fields.remove("version").is_some());
        assert!(fields.remove("guard").is_some());

        let decoded: Response = serde_json::from_value(wire).expect("decode an older reply");
        let decoded = decoded.status.expect("status");
        assert_eq!(decoded.version, "");
        assert_eq!(decoded.guard, GuardStatus::default());
        assert!(decoded.enabled);
    }

    /// `set-module-config` is absolute-replace: a flag left off the command
    /// line is sent as its built-in default, not left alone. If those
    /// defaults drift from the shipped configuration file, "change one knob"
    /// silently rewrites every other knob to a stale value.
    #[test]
    fn set_module_config_defaults_match_the_shipped_config() {
        let arguments = Arguments::parse_from(["ssctl", "set-module-config"]);
        let Request::SetModuleConfig { config: from_cli } = request(arguments.command) else {
            panic!("set-module-config did not build a SetModuleConfig request");
        };
        let shipped = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        assert_eq!(from_cli, ModuleTuningConfig::from_config(&shipped));
    }
}
