// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
//! What `ssctl` prints.
//!
//! The two read-only commands used to print the same object -- every
//! request's reply carries the whole `RuntimeStatus`, and `ssctl` dumped it
//! as JSON, so `status` and `flows` differed in one line of text. They are
//! now two different questions with two different answers:
//!
//! * `status` -- is Skyline Speeder running, and has it actually taken the
//!   host over? Attachment, the host default, the guard, kernel support.
//! * `flows`  -- what is it doing to the traffic? The accelerated
//!   connections, the coefficients in force on them, and the counters
//!   proving the algorithm ran.
//!
//! `--json` still prints the raw reply, and every view is built from that
//! same reply: there is nothing here an operator cannot also get as JSON.
use crate::style::{bits_per_second, bytes, count, duration, ellipsize, pad, wrap, Mark, Theme};
use skyline_common::{
    FlowReport, FlowRow, GuardStatus, Module, ModuleTuningConfig, Response, RuntimeStatus,
    SKYLINE_CC_NAME,
};
use std::fmt::Write as _;

/// The project's funding, shown under every command. Skyline Speeder is
/// developed and released with no revenue of its own.
pub const SPONSOR: &str = "Exclusively sponsored by Skyline Connect";
pub const SPONSOR_URL: &str = "https://www.skylineconnect.io";

/// Label column of the `mark / label / value` rows.
const LABEL: usize = 22;

struct Page<'a> {
    theme: &'a Theme,
    out: String,
}

impl<'a> Page<'a> {
    fn new(theme: &'a Theme) -> Self {
        Self {
            theme,
            out: String::new(),
        }
    }

    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    fn section(&mut self, title: &str) {
        self.line(&self.theme.section(title));
    }

    /// `  <mark>  <label>   <value>`, the shape every fact in `status` takes.
    fn row(&mut self, mark: Mark, label: &str, value: &str) {
        let line = format!(
            "   {}  {} {}",
            self.theme.mark(mark),
            pad(label, LABEL),
            value
        );
        self.line(line.trim_end());
    }

    /// An indented continuation under the row above: a hint, a reason, a
    /// remedy. Dim, because it is never the headline -- and wrapped, never
    /// cut, because the end of a remedy is the part that says what to run.
    fn note(&mut self, text: &str) {
        let indent = LABEL + 7;
        for part in wrap(text, self.theme.width.saturating_sub(indent)) {
            let line = format!("{}{}", " ".repeat(indent), self.theme.dim(&part));
            self.line(&line);
        }
    }

    /// The title bar: name and version on the left, the one-word verdict on
    /// the right. Padded from the plain lengths -- colour codes are bytes
    /// that occupy no columns.
    fn banner(&mut self, title: &str, version: &str, lamp_on: bool, verdict: &str, mark: Mark) {
        let icon = " ";
        let left_plain = format!("{icon}{title} {version}");
        let right_plain = format!("  {verdict}");
        let gap = self
            .theme
            .width
            .saturating_sub(left_plain.chars().count() + right_plain.chars().count())
            .max(1);
        let verdict = match mark {
            Mark::Good => self.theme.bold_green(verdict),
            Mark::Bad => self.theme.bold_red(verdict),
            Mark::Warn => self.theme.bold_yellow(verdict),
            _ => self.theme.bold(verdict),
        };
        let line = format!(
            "{icon}{} {}{}{} {}",
            self.theme.bold_cyan(title),
            self.theme.dim(version),
            " ".repeat(gap),
            self.theme.lamp(lamp_on),
            verdict
        );
        self.line(&line);
        self.line(&self.theme.rule());
    }

    fn finish(mut self) -> String {
        self.blank();
        self.line(&sponsor(self.theme));
        self.out
    }
}

/// One line, under everything, naming who pays for this.
pub fn sponsor(theme: &Theme) -> String {
    format!(
        " {} {} {}",
        theme.dim("\u{2500}"),
        theme.dim(SPONSOR),
        theme.cyan(SPONSOR_URL)
    )
}

/// True when the host's new connections actually go through skyline_cc:
/// attached AND the namespace default. Attachment alone changes nothing --
/// registering an algorithm is not selecting it.
fn carrying_traffic(status: &RuntimeStatus) -> bool {
    status.enabled && status.guard.live.tcp_congestion_control == SKYLINE_CC_NAME
}

fn module_label(module: Module) -> &'static str {
    match module {
        Module::EarlyLoss => "M1 early-loss",
        Module::AdaptiveCwnd => "M2 adaptive-cwnd",
        Module::LossClassifier => "M3 loss-classifier",
        Module::Pacing => "M4 pacing",
    }
}

const ALL_MODULES: [Module; 4] = [
    Module::EarlyLoss,
    Module::AdaptiveCwnd,
    Module::LossClassifier,
    Module::Pacing,
];

/// `status`: is it running, and has it taken the host over?
pub fn status(theme: &Theme, response: &Response) -> String {
    let Some(status) = &response.status else {
        return format!(
            " {} {}\n\n{}\n",
            theme.mark(Mark::Bad),
            response.message,
            sponsor(theme)
        );
    };
    let mut page = Page::new(theme);
    let live = &status.guard.live;
    let carrying = carrying_traffic(status);

    let (verdict, verdict_mark) = if carrying {
        ("ACCELERATING", Mark::Good)
    } else if status.enabled {
        ("ATTACHED, NOT DEFAULT", Mark::Warn)
    } else {
        ("STANDBY", Mark::Warn)
    };
    let version = if status.version.is_empty() {
        "(version unknown)".to_owned()
    } else {
        format!("v{}", status.version)
    };
    page.banner("SKYLINE SPEEDER", &version, carrying, verdict, verdict_mark);
    page.blank();

    // --- what the host is actually running ---------------------------------
    page.section("TAKEOVER");
    if status.enabled {
        let attached = match status.attached_s {
            Some(seconds) => format!("attached, {} ago", duration(seconds)),
            None => "attached".to_owned(),
        };
        page.row(Mark::Good, "skyline_cc struct_ops", &theme.green(&attached));
    } else {
        page.row(
            Mark::Bad,
            "skyline_cc struct_ops",
            &theme.red("not attached"),
        );
        page.note("nothing is accelerated until: sudo ssctl enable");
    }

    let default_cc = if live.tcp_congestion_control.is_empty() {
        "(unreadable)"
    } else {
        &live.tcp_congestion_control
    };
    if carrying {
        page.row(
            Mark::Good,
            "host default cc",
            &format!(
                "{}  {}",
                theme.green(default_cc),
                theme.dim("every new connection")
            ),
        );
    } else if status.enabled {
        page.row(Mark::Bad, "host default cc", &theme.red(default_cc));
        page.note("skyline_cc is loaded but new connections do not use it");
        page.note("something rewrote net.ipv4.tcp_congestion_control; re-run: sudo ssctl enable");
    } else {
        page.row(Mark::Off, "host default cc", &theme.dim(default_cc));
    }
    page.row(
        if status.enabled {
            Mark::Good
        } else {
            Mark::Off
        },
        "fallback on drain",
        &status.fallback_cc,
    );

    let mut modules = String::new();
    for module in ALL_MODULES {
        // Same rule as PARAMETERS in the flows report: a tick means "acting
        // on your traffic now", so a module that is switched on while
        // nothing is attached is not ticked.
        let on = status.enabled && status.modules.contains(&module);
        let text = format!(
            "{} {}",
            theme.mark(if on { Mark::Good } else { Mark::Off }),
            if on {
                module_label(module).to_owned()
            } else {
                theme.dim(module_label(module))
            }
        );
        if !modules.is_empty() {
            modules.push_str("  ");
        }
        modules.push_str(&text);
    }
    page.row(Mark::Info, "modules", &modules);
    page.row(
        Mark::Info,
        "live connections",
        &format!(
            "{} {}",
            theme.bold(&count(status.active_flows)),
            theme.dim("on skyline_cc  (sudo ssctl flows)")
        ),
    );

    page.blank();
    guard_section(&mut page, theme, &status.guard, status.enabled);

    // --- kernel support ----------------------------------------------------
    page.blank();
    page.section("KERNEL");
    let capabilities = &status.capabilities;
    page.row(
        Mark::Info,
        "release",
        &theme.bold(&capabilities.kernel_release),
    );
    let flags = [
        ("BTF", capabilities.btf),
        ("bpffs", capabilities.bpffs),
        ("cgroup v2", capabilities.cgroup_v2),
        ("fq", capabilities.fq_available),
        ("struct_ops", capabilities.struct_ops),
    ];
    let mut rendered = String::new();
    for (name, present) in flags {
        let mark = if present { Mark::Good } else { Mark::Bad };
        let _ = write!(rendered, "{} {name}   ", theme.mark(mark));
    }
    page.row(Mark::Info, "required", rendered.trim_end());
    let optional = format!(
        "{} RACK reorder hook   {} fallback {} available",
        theme.mark(if capabilities.rack_reo_hook {
            Mark::Good
        } else {
            Mark::Off
        }),
        theme.mark(if capabilities.fallback_cc_available {
            Mark::Good
        } else {
            Mark::Warn
        }),
        status.fallback_cc
    );
    page.row(Mark::Info, "optional", &optional);

    // --- tier-1 sysctls ----------------------------------------------------
    page.blank();
    page.section("GLOBAL SYSCTLS (M1 tier-1)");
    let managed = &status.rack_tuning.managed;
    let live_tuning = &status.rack_tuning.live;
    let owned = [
        (
            "tcp_recovery",
            managed.tcp_recovery,
            live_tuning.tcp_recovery,
        ),
        (
            "tcp_reordering",
            managed.tcp_reordering,
            live_tuning.tcp_reordering,
        ),
        (
            "tcp_early_retrans",
            managed.tcp_early_retrans,
            live_tuning.tcp_early_retrans,
        ),
    ];
    if owned.iter().all(|(_, managed, _)| managed.is_none()) {
        page.row(
            Mark::Off,
            "ownership",
            &theme.dim("not owned by skyline-speederd; the host's own values apply"),
        );
        for (name, _, live_value) in owned {
            if let Some(value) = live_value {
                page.row(Mark::Info, name, &format!("{value}  (host)"));
            }
        }
    } else {
        for (name, want, have) in owned {
            match (want, have) {
                (Some(want), Some(have)) if want == have => {
                    page.row(Mark::Good, name, &format!("{have}"))
                }
                (Some(want), Some(have)) => {
                    page.row(
                        Mark::Warn,
                        name,
                        &format!(
                            "{} {}",
                            theme.yellow(&have.to_string()),
                            theme.dim(&format!("(configured {want})"))
                        ),
                    );
                    page.note("changed since the daemon started; restart it to re-apply");
                }
                (Some(want), None) => {
                    page.row(Mark::Warn, name, &format!("unreadable (configured {want})"))
                }
                (None, Some(have)) => page.row(Mark::Off, name, &format!("{have}  (host)")),
                (None, None) => {}
            }
        }
    }

    // --- daemon ------------------------------------------------------------
    page.blank();
    page.section("DAEMON");
    page.row(
        Mark::Good,
        "uptime",
        &format!(
            "{}{}",
            duration(status.uptime_s),
            if status.uptime_s == 0 {
                theme.dim("  (older daemon: not reported)")
            } else {
                String::new()
            }
        ),
    );
    page.row(
        Mark::Info,
        "config generation",
        &format!(
            "{}  {}",
            status.generation,
            theme.dim("raised by every enable / set-* / reset-*")
        ),
    );

    // --- anything wrong ----------------------------------------------------
    let mut warnings: Vec<String> = Vec::new();
    if !status.enabled {
        warnings.push(
            "skyline_cc is not attached: this host is not accelerated (sudo ssctl enable)"
                .to_owned(),
        );
    } else if !carrying {
        warnings.push(format!(
            "new connections use {default_cc}, not skyline_cc -- the acceleration is bypassed"
        ));
    }
    if status.enabled && !status.guard.armed {
        warnings.push(
            "the guard is not armed: a sysctl file or a \"one-click BBR\" script can move this \
             host off skyline_cc without any error"
                .to_owned(),
        );
    }
    if let Some(stats) = &status.rack_rto.stats {
        if status.rack_rto.config.enabled && stats.established_cb == 0 {
            warnings.push(
                "dynamic RTO is on but skyline_policy has seen no connection: only processes \
                 inside /sys/fs/cgroup/skyline-speeder pass through it"
                    .to_owned(),
            );
        }
    }
    warnings.extend(status.capabilities.notes.iter().cloned());
    if !warnings.is_empty() {
        page.blank();
        page.section("ATTENTION");
        for warning in warnings {
            for (index, part) in wrap(&warning, theme.width.saturating_sub(7))
                .into_iter()
                .enumerate()
            {
                let lead = if index == 0 {
                    format!("   {}  ", theme.mark(Mark::Warn))
                } else {
                    " ".repeat(6)
                };
                page.line(&format!("{lead}{}", theme.yellow(&part)));
            }
        }
    }

    page.finish()
}

fn guard_section(page: &mut Page<'_>, theme: &Theme, guard: &GuardStatus, enabled: bool) {
    let armed = if guard.armed {
        format!(
            "{}  {}",
            theme.bold_green("ARMED"),
            theme.dim(&format!("re-checked every {} s", guard.interval_s))
        )
    } else if enabled {
        theme.bold_yellow("NOT ARMED").to_string()
    } else {
        theme.dim("not armed (nothing attached)").to_string()
    };
    page.section("DRIFT GUARD");
    page.row(
        if guard.armed { Mark::Good } else { Mark::Warn },
        "state",
        &armed,
    );

    let live = &guard.live;
    let cc_ok = live.tcp_congestion_control == SKYLINE_CC_NAME;
    page.row(
        if !guard.armed {
            Mark::Off
        } else if cc_ok {
            Mark::Good
        } else {
            Mark::Bad
        },
        "congestion control",
        &format!(
            "{}{}",
            live.tcp_congestion_control,
            restored(theme, guard.cc_restored)
        ),
    );
    if guard.qdisc {
        let dq_ok = live.default_qdisc == "fq";
        page.row(
            if !guard.armed {
                Mark::Off
            } else if dq_ok {
                Mark::Good
            } else {
                Mark::Bad
            },
            "net.core.default_qdisc",
            &format!(
                "{}{}",
                live.default_qdisc,
                restored(theme, guard.default_qdisc_restored)
            ),
        );
        if live.devices.is_empty() {
            let interface = live.interface.as_deref().unwrap_or("(none configured)");
            page.row(
                Mark::Off,
                "managed interface",
                &theme.dim(&format!("{interface}: no Ethernet device under it")),
            );
        } else {
            for device in &live.devices {
                let qdisc = device.qdisc.as_deref().unwrap_or("(unreadable)");
                let good = qdisc == "fq" || qdisc.starts_with("mq/fq");
                page.row(
                    if !guard.armed {
                        Mark::Off
                    } else if good {
                        Mark::Good
                    } else {
                        Mark::Warn
                    },
                    &format!("{} root qdisc", device.name),
                    &format!(
                        "{qdisc}{}",
                        times(theme, "replaced", guard.interface_qdisc_replaced)
                    ),
                );
            }
        }
    } else {
        page.row(
            Mark::Off,
            "qdisc",
            &theme.dim("not guarded ([guard] qdisc = false)"),
        );
    }

    if guard.checks > 0 {
        page.row(
            Mark::Info,
            "checks run",
            &format!("{}", count(guard.checks)),
        );
    }
    if let Some(correction) = &guard.last_correction {
        let when = guard
            .last_correction_unix_s
            .and_then(ago)
            .map(|seconds| theme.dim(&format!("{} ago \u{00b7} ", duration(seconds))))
            .unwrap_or_default();
        page.row(
            Mark::Warn,
            "last correction",
            &format!("{when}{correction}"),
        );
    }
    if let Some(error) = &guard.last_error {
        page.row(Mark::Bad, "last error", &theme.red(error));
    }
    for note in &guard.notes {
        let mut parts = wrap(note, theme.width.saturating_sub(LABEL + 7)).into_iter();
        page.row(
            Mark::Warn,
            "note",
            &theme.yellow(&parts.next().unwrap_or_default()),
        );
        for part in parts {
            page.note(&part);
        }
    }
}

/// `   put back 3x`, or nothing at all while the count is zero -- a counter
/// that never moved is noise on every other line.
fn restored(theme: &Theme, count: u64) -> String {
    times(theme, "put back", count)
}

fn times(theme: &Theme, verb: &str, count: u64) -> String {
    if count == 0 {
        String::new()
    } else {
        theme.dim(&format!("   {verb} {count}x"))
    }
}

fn ago(unix_s: u64) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    now.checked_sub(unix_s)
}

/// `flows`: what the acceleration is doing to the traffic.
pub fn flows(theme: &Theme, response: &Response) -> String {
    let Some(status) = &response.status else {
        return format!(
            " {} {}\n\n{}\n",
            theme.mark(Mark::Bad),
            response.message,
            sponsor(theme)
        );
    };
    let mut page = Page::new(theme);
    let carrying = carrying_traffic(status);
    let report = response.flows.clone().unwrap_or_default();

    let (verdict, mark) = if !status.enabled {
        ("NOT ACCELERATING", Mark::Bad)
    } else if !carrying {
        ("BYPASSED", Mark::Warn)
    } else if report.on_skyline_cc == 0 {
        ("IDLE", Mark::Warn)
    } else {
        ("ACCELERATING", Mark::Good)
    };
    page.banner(
        "ACCELERATED TRAFFIC",
        &format!("{} on skyline_cc", count(report.on_skyline_cc)),
        carrying && report.on_skyline_cc > 0,
        verdict,
        mark,
    );
    page.blank();

    if !status.enabled {
        page.row(
            Mark::Bad,
            "skyline_cc",
            &theme.red("not attached -- nothing on this host is accelerated"),
        );
        page.note("attach it with: sudo ssctl enable");
        page.blank();
    } else if !carrying {
        page.row(
            Mark::Bad,
            "host default cc",
            &theme.red(&status.guard.live.tcp_congestion_control),
        );
        page.note("skyline_cc is attached but new connections bypass it (sudo ssctl status)");
        page.blank();
    }

    connections(&mut page, theme, &report, status.enabled);
    page.blank();
    parameters(&mut page, theme, status);
    page.blank();
    decisions(&mut page, theme, status);

    page.finish()
}

fn connections(page: &mut Page<'_>, theme: &Theme, report: &FlowReport, attached: bool) {
    page.section("CONNECTIONS");
    if let Some(error) = &report.source_error {
        page.row(Mark::Warn, "listing unavailable", &theme.yellow(error));
        page.note("the counters and coefficients below come from the daemon and are unaffected");
        return;
    }

    let coverage = if report.tcp_total > 0 {
        report.on_skyline_cc as f64 / report.tcp_total as f64
    } else {
        0.0
    };
    let mark = if report.tcp_total == 0 {
        Mark::Off
    } else if coverage >= 0.999 {
        Mark::Good
    } else if coverage > 0.0 {
        Mark::Info
    } else {
        Mark::Warn
    };
    page.row(
        mark,
        "coverage",
        &format!(
            "{} {}  {}",
            theme.bar(coverage, 20, mark),
            theme.bold(&format!("{:.0}%", coverage * 100.0)),
            theme.dim(&format!(
                "{} of {} TCP connections",
                count(report.on_skyline_cc),
                count(report.tcp_total)
            ))
        ),
    );
    if attached && report.on_skyline_cc < report.tcp_total {
        page.note("connections opened before `enable` keep the algorithm they started on");
    }
    if report.accelerated.is_empty() {
        return;
    }

    // Volume and retransmissions from the kernel's own byte counters, which
    // is the one place in this report where "how much" can be stated as a
    // measured quantity -- see `decisions` for why the BPF delivered counter
    // cannot. Summed over the rows shown, so it matches the table even when
    // the list is capped.
    let sent: u64 = report
        .accelerated
        .iter()
        .filter_map(|row| row.bytes_sent)
        .sum();
    let retrans: u64 = report
        .accelerated
        .iter()
        .filter_map(|row| row.bytes_retrans)
        .sum();
    if sent > 0 {
        let ratio = retrans as f64 / sent as f64;
        let mark = if ratio < 0.01 {
            Mark::Good
        } else if ratio < 0.05 {
            Mark::Warn
        } else {
            Mark::Bad
        };
        page.row(
            mark,
            "volume",
            &format!(
                "{} sent \u{00b7} {} retransmitted  {} {}",
                theme.bold(&bytes(sent)),
                bytes(retrans),
                theme.bar(ratio / 0.10, 10, mark),
                theme.dim(&format!("{:.2}% (bar: 0-10%)", ratio * 100.0))
            ),
        );
    }

    // Peer takes whatever the terminal has left once the fixed columns are
    // accounted for: an IPv6 address plus a port is 47 characters, and
    // cutting the numbers to keep it whole would be the wrong trade.
    let peer_width = page.theme.width.saturating_sub(69).clamp(16, 44);
    page.blank();
    page.line(&theme.dim(&format!(
        "   {}  {:>9}  {:>7}  {:>11}  {:>11}  {:>10}  {:>6}",
        pad("PEER", peer_width),
        "RTT",
        "CWND",
        "PACING",
        "DELIVERED",
        "SENT",
        "RETR"
    )));
    for flow in &report.accelerated {
        page.line(&flow_row(theme, flow, peer_width));
    }
    if report.truncated > 0 {
        page.line(&theme.dim(&format!(
            "   {} more, not shown (sorted by bytes sent; --json has the same cap)",
            count(report.truncated)
        )));
    }
}

fn flow_row(theme: &Theme, flow: &FlowRow, peer_width: usize) -> String {
    let rtt = flow
        .rtt_ms
        .map(|value| format!("{value:.1} ms"))
        .unwrap_or_else(|| "-".to_owned());
    let cwnd = flow
        .cwnd_packets
        .map(count)
        .unwrap_or_else(|| "-".to_owned());
    let pacing = flow
        .pacing_bps
        .map(bits_per_second)
        .unwrap_or_else(|| "-".to_owned());
    let delivered = flow
        .delivery_bps
        .map(bits_per_second)
        .unwrap_or_else(|| "-".to_owned());
    let sent = flow.bytes_sent.map(bytes).unwrap_or_else(|| "-".to_owned());
    let (retrans, retrans_mark) = match flow.retransmit_ratio() {
        Some(ratio) if ratio <= 0.0 => ("0%".to_owned(), Mark::Good),
        Some(ratio) if ratio < 0.01 => (format!("{:.1}%", ratio * 100.0), Mark::Good),
        Some(ratio) if ratio < 0.05 => (format!("{:.1}%", ratio * 100.0), Mark::Warn),
        Some(ratio) => (format!("{:.1}%", ratio * 100.0), Mark::Bad),
        None => ("-".to_owned(), Mark::Off),
    };
    let retrans = match retrans_mark {
        Mark::Good => theme.green(&format!("{retrans:>6}")),
        Mark::Warn => theme.yellow(&format!("{retrans:>6}")),
        Mark::Bad => theme.red(&format!("{retrans:>6}")),
        _ => theme.dim(&format!("{retrans:>6}")),
    };
    format!(
        "   {}  {:>9}  {:>7}  {}  {}  {:>10}  {}",
        pad(&ellipsize(&flow.peer, peer_width), peer_width),
        rtt,
        cwnd,
        theme.cyan(&format!("{pacing:>11}")),
        theme.bold(&format!("{delivered:>11}")),
        sent,
        retrans
    )
}

/// The coefficients the daemon is running right now, grouped by what they
/// do rather than by which table of the configuration file they come from.
fn parameters(page: &mut Page<'_>, theme: &Theme, status: &RuntimeStatus) {
    let tuning: &ModuleTuningConfig = &status.module_tuning;
    // A module that is switched on but not attached applies to nothing, so
    // it is not ticked: the mark means "this is acting on your traffic".
    let on = |module: Module| status.enabled && status.modules.contains(&module);
    let m2 = on(Module::AdaptiveCwnd);
    let m3 = on(Module::LossClassifier);
    let m4 = on(Module::Pacing);

    if status.enabled {
        page.section("PARAMETERS IN FORCE");
    } else {
        page.section("PARAMETERS (CONFIGURED, NOT IN FORCE)");
    }
    page.row(
        if m2 { Mark::Good } else { Mark::Off },
        "pacing rate",
        &format!(
            "cruise {:.2}x bw \u{00b7} startup {:.2}x bw \u{00b7} cap {} per flow",
            tuning.cruise_pacing_gain,
            tuning.startup_gain,
            theme.bold(&format!("{} Mb/s", tuning.max_pacing_mbps))
        ),
    );
    page.row(
        if m2 { Mark::Good } else { Mark::Off },
        "congestion window",
        &format!(
            "{} \u{2026} {} packets \u{00b7} initial {} \u{00b7} cruise {:.2}x BDP",
            count(u64::from(tuning.min_cwnd_packets)),
            count(u64::from(tuning.max_cwnd_packets)),
            tuning.initial_cwnd_packets,
            tuning.cruise_inflight_gain
        ),
    );
    if status.enabled && !m2 {
        page.note("M2 is off: the kernel's own window control runs, these are not applied");
    }
    page.row(
        if m2 { Mark::Good } else { Mark::Off },
        "queue guardrail",
        &format!(
            "{} ms or {:.2}x base RTT, then gain {:.2}",
            tuning.max_queue_delay_ms, tuning.max_queue_delay_ratio, tuning.guardrail_gain
        ),
    );
    page.row(
        if m3 { Mark::Good } else { Mark::Off },
        "loss compensation",
        &format!(
            "up to {:.0}% of the measured loss rate",
            tuning.loss_inflation_max_ratio * 100.0
        ),
    );
    page.row(
        if m2 { Mark::Good } else { Mark::Off },
        "bandwidth estimate",
        &format!(
            "{} RTT window \u{00b7} min-RTT kept {} s \u{00b7} startup ends under {:.0}% growth in {} RTTs",
            tuning.bw_window_rtts,
            tuning.min_rtt_window_s,
            tuning.startup_growth_ratio * 100.0,
            tuning.startup_plateau_rtts
        ),
    );
    let framework = format!(
        "{} PRR pacing   {} kernel-equivalent pacing cap   {} M4 pacing",
        theme.mark(if status.enabled && tuning.prr_pacing_enabled {
            Mark::Good
        } else {
            Mark::Off
        }),
        theme.mark(if status.enabled && tuning.auto_pacing_enabled {
            Mark::Good
        } else {
            Mark::Off
        }),
        theme.mark(if m4 { Mark::Good } else { Mark::Off }),
    );
    page.row(Mark::Info, "framework switches", &framework);

    let rto = &status.rack_rto.config;
    if rto.enabled && status.enabled {
        page.row(
            Mark::Good,
            "dynamic RTO floor",
            &format!(
                "{:.3}x srtt, clamped {:.0}\u{2013}{:.0} ms, after {} samples",
                f64::from(rto.srtt_permille) / 1000.0,
                f64::from(rto.floor_us) / 1000.0,
                f64::from(rto.ceiling_us) / 1000.0,
                rto.warmup_samples
            ),
        );
        if rto.rto_max_normal_permille > 0 {
            page.row(
                Mark::Good,
                "dynamic RTO ceiling",
                &format!(
                    "{:.3}x base RTT, {:.3}x once congested",
                    f64::from(rto.rto_max_normal_permille) / 1000.0,
                    f64::from(rto.rto_max_congested_permille) / 1000.0
                ),
            );
        }
        page.note("only connections from /sys/fs/cgroup/skyline-speeder pass through it");
    } else {
        page.row(Mark::Off, "dynamic RTO", &theme.dim("off"));
    }

    let dscp = &status.retransmit_dscp.config;
    if dscp.enabled && status.enabled {
        page.row(
            Mark::Good,
            "retransmit DSCP",
            &format!("codepoint {} on every TCP retransmission", dscp.dscp_value),
        );
    } else {
        page.row(Mark::Off, "retransmit DSCP", &theme.dim("off"));
    }
}

/// The counters that prove the algorithm ran, and the two rates worth a bar.
fn decisions(page: &mut Page<'_>, theme: &Theme, status: &RuntimeStatus) {
    let since = match status.attached_s {
        Some(seconds) => format!("since the attach, {} ago", duration(seconds)),
        None => "since the attach".to_owned(),
    };
    page.section("WHAT THE ALGORITHM DID");
    let Some(metrics) = &status.metrics else {
        page.row(
            Mark::Off,
            "counters",
            &theme.dim("no skyline_cc object is loaded, so there is nothing to count"),
        );
        return;
    };
    page.row(Mark::Info, "window", &theme.dim(&since));
    page.row(
        Mark::Info,
        "ack decisions",
        &theme.bold(&count(metrics.ack_events)),
    );
    // A bar needs a full scale the reader can see. Both rates below are
    // small numbers whose interesting range is 0-10%, so that is the scale,
    // and it is named on the line rather than implied.
    //
    // The denominator is `ack_events` for both, because both counters are
    // incremented on the same per-ack path: each is a share of the decisions
    // the algorithm made. `delivered_packets` would be the more natural
    // denominator for loss and is deliberately NOT used -- see the
    // "delivery samples" row below for what it actually accumulates.
    page.row(
        rate_mark(metrics.loss_events, metrics.ack_events, 0.02),
        "loss events",
        &rate_line(
            theme,
            metrics.loss_events,
            metrics.ack_events,
            "of ack decisions",
        ),
    );
    page.row(
        rate_mark(metrics.guardrail_hits, metrics.ack_events, 0.05),
        "guardrail hits",
        &rate_line(
            theme,
            metrics.guardrail_hits,
            metrics.ack_events,
            "of ack decisions",
        ),
    );
    if metrics.guardrail_hits > 0 {
        // Two increment sites in skyline_cc.bpf.c, not one: the
        // queue-delay/ECN clamp and the max_cwnd_packets ceiling. Naming
        // only the first would send an operator looking for queueing that
        // is not there.
        page.note(
            "a safety limit held the gains back: the queue-delay/ECN clamp, or the cwnd \
             ceiling, counted together",
        );
    }
    page.row(
        Mark::Info,
        "mode switches",
        &format!(
            "{}  {}",
            count(metrics.state_transitions),
            theme.dim("STARTUP <-> CRUISE")
        ),
    );
    page.row(Mark::Info, "pacing updates", &count(metrics.pacing_updates));
    // Named for what it is. It sums `rate_sample.delivered` per ack, and
    // consecutive acks report overlapping windows, so it runs about two
    // orders of magnitude above the packets actually delivered (measured on
    // a 6.12.63 host: 16.7 M against the kernel's own 164 k over the same
    // 20 s). Good for comparing two runs of the same shape, which is all the
    // experiment harness uses it for; wrong as a volume, which CONNECTIONS
    // above reports from the kernel's byte counters instead.
    page.row(
        Mark::Info,
        "delivery samples",
        &format!(
            "{}  {}",
            count(metrics.delivered_packets),
            theme.dim("overlapping rate samples, not a packet count")
        ),
    );
    if metrics.prr_adjustments > 0 {
        page.row(
            Mark::Warn,
            "PRR adjustments",
            &format!(
                "{}  {}",
                count(metrics.prr_adjustments),
                theme.dim("non-zero with M2 on means the PRR bypass did not take")
            ),
        );
    }

    if let Some(tc) = &status.tc_stats {
        page.blank();
        page.section("TC EGRESS (skyline_tc)");
        page.row(
            Mark::Info,
            "seen",
            &format!(
                "{} packets \u{00b7} {} \u{00b7} {} GSO",
                count(tc.packets),
                bytes(tc.bytes),
                count(tc.gso_packets)
            ),
        );
        page.row(
            if tc.drops == 0 {
                Mark::Good
            } else {
                Mark::Warn
            },
            "drops",
            &count(tc.drops),
        );
        if let Some(dscp) = &status.retransmit_dscp.stats {
            if status.retransmit_dscp.config.enabled || dscp.retransmits_marked > 0 {
                page.row(
                    Mark::Info,
                    "retransmits marked",
                    &format!(
                        "{} of {} detected",
                        count(dscp.retransmits_marked),
                        count(dscp.retransmits_detected)
                    ),
                );
            }
        }
    }

    if let Some(rto) = &status.rack_rto.stats {
        if status.rack_rto.config.enabled || rto.applied > 0 {
            page.blank();
            page.section("DYNAMIC RTO (skyline_policy)");
            page.row(
                if rto.applied > 0 {
                    Mark::Good
                } else {
                    Mark::Warn
                },
                "applied",
                &format!(
                    "{}  {}",
                    theme.bold(&count(rto.applied)),
                    theme.dim(&format!(
                        "rejected {} \u{00b7} warmup {} \u{00b7} unchanged {}",
                        count(rto.rejected),
                        count(rto.skipped_warmup),
                        count(rto.unchanged)
                    ))
                ),
            );
            page.row(
                if rto.established_cb > 0 {
                    Mark::Good
                } else {
                    Mark::Warn
                },
                "connections seen",
                &count(rto.established_cb),
            );
            if rto.established_cb == 0 {
                page.note("0 means no process is in /sys/fs/cgroup/skyline-speeder, not an error");
            }
        }
    }
}

/// 0-10% full scale, with the rate spelled out. A bar with no stated scale
/// is decoration; this one says what full means.
fn rate_line(theme: &Theme, part: u64, whole: u64, what: &str) -> String {
    if whole == 0 {
        return format!(
            "{}  {}",
            count(part),
            theme.dim("(nothing to compare against)")
        );
    }
    let rate = part as f64 / whole as f64;
    let mark = rate_mark(part, whole, 0.02);
    format!(
        "{} {}  {}",
        theme.bar(rate / 0.10, 10, mark),
        theme.bold(&format!("{:.2}%", rate * 100.0)),
        theme.dim(&format!("{} {what} (bar: 0-10%)", count(part)))
    )
}

fn rate_mark(part: u64, whole: u64, warn_above: f64) -> Mark {
    if whole == 0 {
        return Mark::Off;
    }
    let rate = part as f64 / whole as f64;
    if rate <= 0.0 {
        Mark::Good
    } else if rate < warn_above {
        Mark::Info
    } else {
        Mark::Warn
    }
}

/// Every command that changes something: the daemon's own sentence, then
/// one line of where the host stands now.
pub fn action(theme: &Theme, response: &Response) -> String {
    let mut page = Page::new(theme);
    let mark = if response.ok { Mark::Good } else { Mark::Bad };
    // `enable` and `drain` answer with a headline and the corrections they
    // made, joined by "; ". Each correction is a fact of its own and gets
    // its own line.
    let mut parts = response.message.split("; ");
    let headline = parts.next().unwrap_or_default();
    // `enable`'s headline names every module and runs past any terminal, so
    // it wraps like the diagnostics do rather than folding at the edge.
    for (index, line) in wrap(headline, theme.width.saturating_sub(4))
        .into_iter()
        .enumerate()
    {
        let painted = if response.ok {
            theme.bold(&line)
        } else {
            theme.bold_red(&line)
        };
        if index == 0 {
            page.line(&format!(" {} {painted}", theme.mark(mark)));
        } else {
            page.line(&format!("   {painted}"));
        }
    }
    for part in parts {
        for (index, line) in wrap(part, theme.width.saturating_sub(6))
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                page.line(&format!("   {} {line}", theme.mark(Mark::Info)));
            } else {
                page.line(&format!("     {line}"));
            }
        }
    }

    if let Some(status) = &response.status {
        let carrying = carrying_traffic(status);
        let state = if carrying {
            theme.green("accelerating")
        } else if status.enabled {
            theme.yellow("attached, but not the host default")
        } else {
            theme.dim("not attached")
        };
        page.line(&format!(
            "   {} {}  {}",
            theme.mark(Mark::Info),
            state,
            theme.dim(&format!(
                "default cc {} \u{00b7} guard {} \u{00b7} {} live connection(s)",
                status.guard.live.tcp_congestion_control,
                if status.guard.armed {
                    "armed"
                } else {
                    "not armed"
                },
                status.active_flows
            ))
        ));
    }
    page.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::ColorChoice;
    use skyline_common::{
        CapabilityReport, GuardDevice, GuardLive, RackRtoConfig, RackRtoStats, RackRtoStatus,
        RackTuningConfig, RackTuningStatus, RetransmitDscpConfig, RetransmitDscpStatus,
        SkylineConfig, SkylineMetrics, TcStats,
    };

    fn theme() -> Theme {
        // Colour off: the assertions below look for text, and a test must
        // not depend on the terminal running it.
        Theme::detect(ColorChoice::Never)
    }

    fn healthy() -> RuntimeStatus {
        let shipped = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        RuntimeStatus {
            version: "0.3.0".to_owned(),
            enabled: true,
            generation: 4,
            modules: vec![Module::AdaptiveCwnd, Module::Pacing],
            fallback_cc: "bbr".to_owned(),
            active_flows: 14,
            tc_stats: Some(TcStats {
                packets: 8_402_113,
                bytes: 12_034_998_221,
                gso_packets: 1_002,
                drops: 0,
            }),
            metrics: Some(SkylineMetrics {
                ack_events: 1_204_551,
                delivered_packets: 8_402_113,
                loss_events: 12_004,
                state_transitions: 482,
                pacing_updates: 1_190_402,
                guardrail_hits: 73,
                hypothetical_early_loss: 0,
                prr_adjustments: 0,
            }),
            rack_tuning: RackTuningStatus {
                managed: RackTuningConfig::default(),
                live: RackTuningConfig {
                    tcp_recovery: Some(1),
                    tcp_reordering: Some(3),
                    tcp_early_retrans: Some(3),
                },
            },
            rack_rto: RackRtoStatus {
                config: RackRtoConfig::disabled(),
                stats: Some(RackRtoStats::default()),
            },
            retransmit_dscp: RetransmitDscpStatus {
                config: RetransmitDscpConfig::disabled(),
                stats: None,
            },
            module_tuning: ModuleTuningConfig::from_config(&shipped),
            capabilities: CapabilityReport {
                kernel_release: "6.12.63+deb13-cloud-amd64".to_owned(),
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
                interval_s: 5,
                qdisc: true,
                checks: 2_301,
                cc_restored: 2,
                default_qdisc_restored: 0,
                interface_qdisc_replaced: 1,
                last_correction: Some("tcp_congestion_control bbr -> skyline_cc".to_owned()),
                last_correction_unix_s: None,
                last_error: None,
                live: GuardLive {
                    tcp_congestion_control: SKYLINE_CC_NAME.to_owned(),
                    default_qdisc: "fq".to_owned(),
                    interface: Some("eth0".to_owned()),
                    interface_qdisc: Some("fq".to_owned()),
                    devices: vec![GuardDevice {
                        name: "eth0".to_owned(),
                        qdisc: Some("fq".to_owned()),
                    }],
                },
                notes: Vec::new(),
            },
            uptime_s: 439_200,
            attached_s: Some(11_520),
        }
    }

    fn sample_flows() -> FlowReport {
        FlowReport {
            accelerated: vec![
                FlowRow {
                    peer: "203.0.113.4:443".to_owned(),
                    local: "198.51.100.7:44122".to_owned(),
                    state: "ESTAB".to_owned(),
                    rtt_ms: Some(142.3),
                    rtt_var_ms: Some(6.2),
                    min_rtt_ms: Some(138.0),
                    cwnd_packets: Some(1204),
                    ssthresh: Some(900),
                    pacing_bps: Some(38_200_000),
                    delivery_bps: Some(31_400_000),
                    send_bps: Some(40_100_000),
                    bytes_sent: Some(1_288_490_188),
                    bytes_acked: Some(1_283_000_000),
                    bytes_retrans: Some(5_153_960),
                    retrans_total: Some(3_560),
                    unacked: Some(12),
                    mss: Some(1448),
                    rto_ms: Some(348.0),
                },
                FlowRow {
                    peer: "[2001:db8::2]:60000".to_owned(),
                    local: "[2001:db8::1]:443".to_owned(),
                    state: "ESTAB".to_owned(),
                    rtt_ms: Some(88.0),
                    cwnd_packets: Some(340),
                    pacing_bps: Some(11_300_000),
                    bytes_sent: Some(4_404_019),
                    bytes_retrans: Some(0),
                    ..FlowRow::default()
                },
            ],
            tcp_total: 18,
            on_skyline_cc: 14,
            truncated: 0,
            source_error: None,
        }
    }

    fn reply(status: RuntimeStatus, flows: Option<FlowReport>) -> Response {
        Response {
            ok: true,
            message: "status returned".to_owned(),
            status: Some(status),
            flows,
        }
    }

    /// `status` answers "has the host been taken over", and says nothing
    /// about per-connection traffic; `flows` answers "what is being
    /// accelerated, with which parameters". Before 0.3.0 both printed the
    /// same JSON object, which is the complaint this split exists to fix.
    #[test]
    fn the_two_reports_answer_different_questions() {
        let theme = theme();
        let status_text = status(&theme, &reply(healthy(), None));
        let flows_text = flows(&theme, &reply(healthy(), Some(sample_flows())));

        for expected in [
            "TAKEOVER",
            "DRIFT GUARD",
            "KERNEL",
            "GLOBAL SYSCTLS",
            "host default cc",
            "net.core.default_qdisc",
        ] {
            assert!(status_text.contains(expected), "status lacks {expected}");
            assert!(
                !flows_text.contains(expected),
                "flows should not repeat {expected}"
            );
        }
        for expected in [
            "CONNECTIONS",
            "PARAMETERS IN FORCE",
            "WHAT THE ALGORITHM DID",
            "coverage",
            "203.0.113.4:443",
            "pacing rate",
        ] {
            assert!(flows_text.contains(expected), "flows lacks {expected}");
            assert!(
                !status_text.contains(expected),
                "status should not repeat {expected}"
            );
        }
    }

    /// Attached but not the host default is the project's worst silent
    /// failure: everything looks loaded and no connection uses it. Both
    /// reports have to say so in as many words.
    #[test]
    fn a_bypassed_host_is_called_out_in_both_reports() {
        let theme = theme();
        let mut bypassed = healthy();
        bypassed.guard.live.tcp_congestion_control = "bbr".to_owned();
        bypassed.guard.armed = false;

        let status_text = status(&theme, &reply(bypassed.clone(), None));
        assert!(status_text.contains("ATTACHED, NOT DEFAULT"));
        assert!(status_text.contains("ATTENTION"));
        assert!(status_text.contains("the acceleration is bypassed"));
        assert!(status_text.contains("the guard is not armed"));

        let flows_text = flows(&theme, &reply(bypassed, Some(sample_flows())));
        assert!(flows_text.contains("BYPASSED"));
        assert!(flows_text.contains("new connections bypass it"));
    }

    #[test]
    fn a_standby_host_says_what_to_run() {
        let theme = theme();
        let mut standby = healthy();
        standby.enabled = false;
        standby.attached_s = None;
        standby.metrics = None;
        standby.guard.armed = false;
        standby.guard.live.tcp_congestion_control = "bbr".to_owned();

        let text = status(&theme, &reply(standby.clone(), None));
        assert!(text.contains("STANDBY"));
        assert!(text.contains("sudo ssctl enable"));

        let text = flows(&theme, &reply(standby, Some(FlowReport::default())));
        assert!(text.contains("NOT ACCELERATING"));
        assert!(text.contains("nothing on this host is accelerated"));
    }

    /// An `ss` that could not run must not take the rest of the report with
    /// it: the coefficients and counters come from the daemon.
    #[test]
    fn flows_survive_an_unusable_ss() {
        let theme = theme();
        let report = FlowReport {
            source_error: Some("ss is not installed (iproute2)".to_owned()),
            ..FlowReport::default()
        };
        let text = flows(&theme, &reply(healthy(), Some(report)));
        assert!(text.contains("listing unavailable"));
        assert!(text.contains("ss is not installed"));
        assert!(text.contains("PARAMETERS IN FORCE"));
        assert!(text.contains("WHAT THE ALGORITHM DID"));
    }

    /// A reply from a daemon too old to send `flows` still renders.
    #[test]
    fn flows_render_without_a_flow_report() {
        let text = flows(&theme(), &reply(healthy(), None));
        assert!(text.contains("CONNECTIONS"));
        assert!(text.contains("coverage"));
    }

    /// Every command names the sponsor, in both output modes (the JSON mode
    /// prints this same line on stderr).
    #[test]
    fn every_report_names_the_sponsor() {
        let theme = theme();
        for text in [
            status(&theme, &reply(healthy(), None)),
            flows(&theme, &reply(healthy(), Some(sample_flows()))),
            action(&theme, &reply(healthy(), None)),
        ] {
            assert!(text.contains(SPONSOR), "{text}");
            assert!(text.contains(SPONSOR_URL), "{text}");
        }
    }

    /// `enable` answers with a headline and a "; "-joined list of what it
    /// corrected; each correction has to become its own line.
    #[test]
    fn an_action_breaks_the_daemons_corrections_into_lines() {
        let response = Response {
            ok: true,
            message: "Skyline Speeder enabled with modules: adaptive-cwnd; put \
                      net.core.default_qdisc back to fq; replaced eth0 root qdisc fq_codel with fq"
                .to_owned(),
            status: Some(healthy()),
            flows: None,
        };
        let text = action(&theme(), &response);
        assert!(text.contains("put net.core.default_qdisc back to fq\n"));
        assert!(text.contains("replaced eth0 root qdisc fq_codel with fq\n"));
        assert!(text.contains("accelerating"));
    }

    /// Eyeball the two reports: `cargo test -p ssctl -- --nocapture
    /// renders_for_review`.
    #[test]
    fn renders_for_review() {
        let theme = Theme::detect(ColorChoice::Always);
        println!("{}", status(&theme, &reply(healthy(), None)));
        println!("{}", flows(&theme, &reply(healthy(), Some(sample_flows()))));
    }
}
