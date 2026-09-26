// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
//! Enumerating the TCP connections skyline_cc is accelerating.
//!
//! `skyline_cc.bpf.c` keeps its per-flow state in `flow_states`, a
//! `BPF_MAP_TYPE_SK_STORAGE` map. Socket storage is addressed by socket, not
//! by key: `bpf_map_get_next_key()` is not implemented for it, so user space
//! cannot walk it at all without either a file descriptor for every socket or
//! a `bpf_iter` program of its own. That is why the daemon has always answered
//! `flows` with nothing but a count.
//!
//! The kernel already publishes the part an operator wants -- cwnd, pacing
//! rate, RTT, retransmissions, and which congestion control each socket runs
//! -- through `sock_diag`, and those are the very fields skyline_cc writes. So
//! this reads them back out of the kernel with `ss`, the way the guard reads
//! qdiscs back with `tc`, and reports the connections whose congestion control
//! is `skyline_cc`.
//!
//! What this deliberately does NOT do is present kernel numbers as skyline_cc
//! internals: there is no mode (STARTUP/CRUISE), no bandwidth estimate and no
//! guardrail state here, because `ss` cannot see them. Those stay aggregate,
//! from the BPF counter maps.
use crate::guard::run_bounded;
use skyline_common::{FlowReport, FlowRow, FLOW_ROWS_MAX};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// `ss` walks every TCP socket on the host under `sock_diag`. It is not
/// holding the rtnl lock the way `tc` does, but a host with a very large
/// number of sockets still takes a moment, and `flows` is interactive.
const SS_TIMEOUT: Duration = Duration::from_secs(5);

/// Counts `ss` processes killed at the timeout that have not exited yet, the
/// same way the guard counts `tc`s. Shared with nothing else, so a stuck `ss`
/// cannot make the guard skip a qdisc check.
static SS_UNREAPED: AtomicUsize = AtomicUsize::new(0);

/// The connections `cc_name` is running right now.
///
/// Never fails: an `ss` that is missing, killed at its timeout or
/// unparseable comes back as `source_error` on an otherwise empty report, so
/// `ssctl flows` can still show everything that comes from the daemon itself.
pub fn collect(cc_name: &str) -> FlowReport {
    match run_bounded("ss", &["-tin"], SS_TIMEOUT, &SS_UNREAPED) {
        Ok(output) => parse(&output, cc_name),
        Err(error) => FlowReport {
            source_error: Some(describe(error)),
            ..FlowReport::default()
        },
    }
}

fn describe(error: anyhow::Error) -> String {
    let missing = error
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
        == Some(std::io::ErrorKind::NotFound);
    if missing {
        "ss is not installed (iproute2), so connections cannot be listed".to_owned()
    } else {
        format!("{error:#}")
    }
}

/// One socket as `ss -tin` prints it: the summary line, then its `-i` detail
/// on one or more continuation lines.
///
/// `-H` (suppress the header) is deliberately not passed: it is younger than
/// some of the iproute2 builds this can meet, and an unknown flag would make
/// `ss` exit non-zero and lose the whole listing. A header line is recognised
/// and skipped instead.
fn parse(output: &str, cc_name: &str) -> FlowReport {
    let mut report = FlowReport::default();
    let mut current: Option<(FlowRow, String)> = None;
    let mut malformed = 0_u64;

    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            // Continuation: the `-i` detail for the socket above.
            if let Some((_, info)) = current.as_mut() {
                info.push(' ');
                info.push_str(line.trim());
            }
            continue;
        }
        finish(current.take(), cc_name, &mut report);
        let mut fields = line.split_whitespace();
        let (Some(state), Some(_recv_q), Some(_send_q), Some(local), Some(peer)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            malformed += 1;
            continue;
        };
        // `ss` without -H starts with `State Recv-Q Send-Q Local Address:Port
        // Peer Address:Port`, which splits into more than five fields.
        if state == "State" || state == "Netid" {
            continue;
        }
        report.tcp_total += 1;
        current = Some((
            FlowRow {
                peer: peer.to_owned(),
                local: local.to_owned(),
                state: state.to_owned(),
                ..FlowRow::default()
            },
            String::new(),
        ));
    }
    finish(current.take(), cc_name, &mut report);

    // A count of 0 with output present means the format was not what this
    // expects -- say so rather than report "no connections", which is what an
    // idle host truthfully looks like.
    if report.tcp_total == 0 && malformed > 0 {
        report.source_error = Some(format!(
            "ss printed {malformed} line(s) in a format this build does not recognise"
        ));
    }

    report
        .accelerated
        .sort_by(|a, b| b.bytes_sent.cmp(&a.bytes_sent));
    if report.accelerated.len() > FLOW_ROWS_MAX {
        report.truncated = (report.accelerated.len() - FLOW_ROWS_MAX) as u64;
        report.accelerated.truncate(FLOW_ROWS_MAX);
    }
    report
}

/// Files a finished socket into `report` if `cc_name` is running it.
fn finish(record: Option<(FlowRow, String)>, cc_name: &str, report: &mut FlowReport) {
    let Some((mut row, info)) = record else {
        return;
    };
    // The congestion control is printed as a bare word among the flags
    // (`ts sack skyline_cc wscale:7,7 ...`), so an exact token match is what
    // identifies it -- not a substring, which `cubic` would find inside a
    // hypothetical `cubic_x`.
    if !info.split_whitespace().any(|token| token == cc_name) {
        return;
    }
    report.on_skyline_cc += 1;
    apply_info(&mut row, &info);
    report.accelerated.push(row);
}

fn apply_info(row: &mut FlowRow, info: &str) {
    let tokens: Vec<&str> = info.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        // `send`, `pacing_rate` and `delivery_rate` print their value as the
        // NEXT token ("pacing_rate 611Mbps"); everything else is key:value.
        let next = || tokens.get(index + 1).copied();
        match *token {
            "send" => row.send_bps = next().and_then(parse_rate),
            "pacing_rate" => row.pacing_bps = next().and_then(parse_rate),
            "delivery_rate" => row.delivery_bps = next().and_then(parse_rate),
            _ => {}
        }
        let Some((key, value)) = token.split_once(':') else {
            continue;
        };
        match key {
            "rtt" => {
                // rtt:<srtt>/<rttvar>
                let (srtt, var) = value.split_once('/').unwrap_or((value, ""));
                row.rtt_ms = srtt.parse().ok();
                row.rtt_var_ms = var.parse().ok();
            }
            "minrtt" => row.min_rtt_ms = value.parse().ok(),
            "rto" => row.rto_ms = value.parse().ok(),
            "cwnd" => row.cwnd_packets = value.parse().ok(),
            "ssthresh" => row.ssthresh = value.parse().ok(),
            "mss" => row.mss = value.parse().ok(),
            "unacked" => row.unacked = value.parse().ok(),
            "bytes_sent" => row.bytes_sent = value.parse().ok(),
            "bytes_acked" => row.bytes_acked = value.parse().ok(),
            "bytes_retrans" => row.bytes_retrans = value.parse().ok(),
            // retrans:<in flight>/<total>
            "retrans" => {
                row.retrans_total = value
                    .split_once('/')
                    .map(|(_, total)| total)
                    .unwrap_or(value)
                    .parse()
                    .ok()
            }
            _ => {}
        }
    }
}

/// `611Mbps`, `46.3Mbps`, `8000bps` -> bits per second.
///
/// iproute2 formats these with `%.3g` against 1000-based steps, so a fast
/// flow prints as `1e+04Mbps` rather than `10Gbps`: the mantissa has to go
/// through a full float parse, and the only suffixes it ever emits are bps,
/// Kbps and Mbps. Gbps/Tbps are accepted anyway, in case that changes.
fn parse_rate(token: &str) -> Option<u64> {
    let lowered = token.to_ascii_lowercase();
    let (mantissa, scale) = if let Some(head) = lowered.strip_suffix("tbps") {
        (head, 1e12)
    } else if let Some(head) = lowered.strip_suffix("gbps") {
        (head, 1e9)
    } else if let Some(head) = lowered.strip_suffix("mbps") {
        (head, 1e6)
    } else if let Some(head) = lowered.strip_suffix("kbps") {
        (head, 1e3)
    } else {
        (lowered.strip_suffix("bps")?, 1.0)
    };
    let value: f64 = mantissa.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `ss -tin` output: a header, two sockets on skyline_cc and one on
    /// cubic, with the `-i` detail wrapped onto its own line.
    const SAMPLE: &str = "\
State  Recv-Q Send-Q Local Address:Port  Peer Address:Port
ESTAB  0      0      10.0.0.1:5201       10.0.0.2:41234
\t ts sack skyline_cc wscale:7,7 rto:204 rtt:13.5/6.2 mss:1448 pmtu:1500 cwnd:1204 ssthresh:900 bytes_sent:1288490188 bytes_acked:1288000000 bytes_retrans:5153960 segs_out:889 retrans:0/37 send 1.03e+03Mbps lastsnd:4 pacing_rate 1.29e+03Mbps delivery_rate 943Mbps delivered:889 busy:8ms unacked:12 minrtt:13.1
ESTAB  0      0      10.0.0.1:22         10.0.0.9:51484
\t ts sack cubic wscale:7,7 rto:204 rtt:0.379/0.098 mss:1448 cwnd:10 bytes_sent:5721 send 305Mbps pacing_rate 611Mbps minrtt:0.146
ESTAB  0      0      [2001:db8::1]:443   [2001:db8::2]:60000
\t cubic-less-line skyline_cc wscale:7,7 rtt:88.0/2.0 cwnd:340 bytes_sent:4404019 send 9.1Mbps pacing_rate 11.3Mbps delivery_rate 8.8Mbps minrtt:87.4
";

    #[test]
    fn counts_every_connection_but_lists_only_the_accelerated_ones() {
        let report = parse(SAMPLE, "skyline_cc");
        assert_eq!(report.tcp_total, 3);
        assert_eq!(report.on_skyline_cc, 2);
        assert_eq!(report.accelerated.len(), 2);
        assert_eq!(report.truncated, 0);
        assert!(report.source_error.is_none());
        // Sorted by bytes sent, so the 1.2 GiB flow leads.
        assert_eq!(report.accelerated[0].peer, "10.0.0.2:41234");
        assert_eq!(report.accelerated[1].peer, "[2001:db8::2]:60000");
    }

    #[test]
    fn reads_the_fields_skyline_cc_drives() {
        let report = parse(SAMPLE, "skyline_cc");
        let flow = &report.accelerated[0];
        assert_eq!(flow.cwnd_packets, Some(1204));
        assert_eq!(flow.ssthresh, Some(900));
        assert_eq!(flow.rtt_ms, Some(13.5));
        assert_eq!(flow.rtt_var_ms, Some(6.2));
        assert_eq!(flow.min_rtt_ms, Some(13.1));
        assert_eq!(flow.rto_ms, Some(204.0));
        assert_eq!(flow.unacked, Some(12));
        assert_eq!(flow.retrans_total, Some(37));
        assert_eq!(flow.mss, Some(1448));
        assert_eq!(flow.bytes_retrans, Some(5_153_960));
        // %.3g scientific notation, which a naive "strip Mbps and parse"
        // would drop on the floor.
        assert_eq!(flow.pacing_bps, Some(1_290_000_000));
        assert_eq!(flow.send_bps, Some(1_030_000_000));
        assert_eq!(flow.delivery_bps, Some(943_000_000));
        let ratio = flow.retransmit_ratio().expect("both byte counters present");
        assert!((ratio - 0.004).abs() < 0.0005, "{ratio}");
    }

    /// A socket whose detail line `ss` never printed (it can happen for a
    /// socket that closes during the walk) must not be counted as
    /// accelerated, and must not lose the sockets after it.
    #[test]
    fn a_socket_without_detail_is_counted_but_not_listed() {
        let text = "ESTAB 0 0 10.0.0.1:1 10.0.0.2:2\nESTAB 0 0 10.0.0.1:3 10.0.0.2:4\n\t skyline_cc cwnd:10\n";
        let report = parse(text, "skyline_cc");
        assert_eq!(report.tcp_total, 2);
        assert_eq!(report.on_skyline_cc, 1);
        assert_eq!(report.accelerated[0].peer, "10.0.0.2:4");
    }

    #[test]
    fn the_row_list_is_capped_but_the_count_is_not() {
        let mut text = String::new();
        for port in 0..(FLOW_ROWS_MAX + 7) {
            text.push_str(&format!("ESTAB 0 0 10.0.0.1:1 10.0.0.2:{port}\n"));
            text.push_str(&format!("\t skyline_cc cwnd:10 bytes_sent:{port}\n"));
        }
        let report = parse(&text, "skyline_cc");
        assert_eq!(report.on_skyline_cc as usize, FLOW_ROWS_MAX + 7);
        assert_eq!(report.accelerated.len(), FLOW_ROWS_MAX);
        assert_eq!(report.truncated, 7);
    }

    /// Another algorithm's name must never match: the whole point of the
    /// report is that it lists what skyline_cc is accelerating.
    #[test]
    fn a_host_with_no_accelerated_connection_reports_none() {
        let report = parse(SAMPLE, "bbr");
        assert_eq!(report.tcp_total, 3);
        assert_eq!(report.on_skyline_cc, 0);
        assert!(report.accelerated.is_empty());
        assert!(report.source_error.is_none());
    }

    #[test]
    fn output_in_an_unknown_format_is_reported_as_such() {
        let report = parse("ss: something went sideways\n", "skyline_cc");
        assert_eq!(report.tcp_total, 0);
        assert!(report
            .source_error
            .as_deref()
            .is_some_and(|note| note.contains("does not recognise")));
    }

    #[test]
    fn rates_parse_in_every_shape_iproute2_prints() {
        assert_eq!(parse_rate("611Mbps"), Some(611_000_000));
        assert_eq!(parse_rate("46.3Mbps"), Some(46_300_000));
        assert_eq!(parse_rate("8000bps"), Some(8000));
        assert_eq!(parse_rate("12.5Kbps"), Some(12_500));
        assert_eq!(parse_rate("1e+04Mbps"), Some(10_000_000_000));
        assert_eq!(parse_rate("2.5Gbps"), Some(2_500_000_000));
        assert_eq!(parse_rate("nonsense"), None);
        assert_eq!(parse_rate("-3Mbps"), None);
    }
}
