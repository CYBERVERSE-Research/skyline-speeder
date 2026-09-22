// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC

//! `[guard]`: keeps the host on skyline_cc and fq while skyline_cc is enabled.
//!
//! Ownership is an invariant. Between a successful `Request::Enable` and the
//! next `Request::Drain`, skyline-speederd is the single writer of
//! - `net.ipv4.tcp_congestion_control` (skyline_cc; it already owned this
//!   sysctl before the guard existed),
//! - `net.core.default_qdisc` (fq) -- moved here from infra/boot-enable.sh,
//!   which no longer writes it, so the two cannot drift apart,
//! - the root qdisc of every device it manages (fq, or mq whose every child
//!   is fq on a multi-queue device): `runtime.tc_interface` itself, or, when
//!   that is a VLAN, bond or bridge whose root is the kernel's default
//!   noqueue, the NICs under it (see `managed_devices`),
//!
//! the last two only while `[guard] qdisc = true`. Nothing else in this
//! repository writes a different value to them while skyline_cc is enabled
//! (the experiment harness's skyline profile writes the same ones, and drains
//! before any other profile); infra/boot-disable.sh's cc write is the fallback
//! for when the daemon is already gone.
//!
//! Why a guard and not a one-shot write: "one-click BBR" scripts persist
//! `tcp_congestion_control=bbr` and `default_qdisc=cake|fq_pie` under
//! /etc/sysctl.d, and anything that re-runs `sysctl --system` puts them back.
//! Every new connection then bypasses skyline_cc and nothing reports it -- one
//! more silent failure mode. And `default_qdisc` only affects qdiscs created
//! afterwards: on a clean Debian 13 host with `default_qdisc = fq` in
//! /etc/sysctl.d, eth0 still had fq_codel because the NIC's qdisc was created
//! before that sysctl ran. So fixing the sysctl alone does not fix the
//! interface; its root qdisc has to be replaced.
//!
//! Arming is race-free by construction: the periodic pass, `Enable`'s arm and
//! `Drain`'s disarm all take the same mutex, and `Drain` writes `fallback_cc`
//! while still holding it, so a pass can never put skyline_cc back after that
//! write. The daemon's own stop writes no sysctl at all (by request: the
//! operator does not want `fallback_cc` written on stop).

use crate::{set_default_congestion_control, CONGESTION_CONTROL_SYSCTL, SKYLINE_CC_NAME};
use anyhow::{anyhow, bail, Context, Result};
use skyline_common::{GuardDevice, GuardLive, GuardStatus, SkylineConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_QDISC_SYSCTL: &str = "/proc/sys/net/core/default_qdisc";

const SYS_CLASS_NET: &str = "/sys/class/net";

const FQ: &str = "fq";

const NOQUEUE: &str = "noqueue";

/// Root qdisc kinds that carry no configuration an operator would miss:
/// the kernel's own defaults and the AQMs "one-click" scripts install. The
/// guard replaces these with fq -- cake only while it is unshaped
/// (`bandwidth unlimited`, no `autorate-ingress`; see `cake_shaping`): a
/// cake with a bandwidth is a rate limiter somebody set, as much as a tbf is.
const REPLACEABLE_KINDS: &[&str] = &[
    "pfifo_fast",
    "pfifo",
    "bfifo",
    "pfifo_head_drop",
    "fq_codel",
    "codel",
    "cake",
    "fq_pie",
    "pie",
    "sfq",
    "red",
    "sfb",
    "choke",
    "hhf",
];

/// Classful, shaping and hardware-offload kinds: somebody built that on
/// purpose, and replacing it would silently destroy their shaping. Left
/// alone, like any kind in neither list (and like a shaped cake).
///
/// noqueue is here only for the one place it can still reach a verdict: a
/// NIC (a device with a `device` link and nothing under it), where the kernel
/// never puts it by default, so somebody did. On a VLAN, bond, bridge,
/// macvlan, veth or tunnel noqueue is the kernel default and says nothing
/// about intent; `managed_devices` looks under such a device instead.
const DELIBERATE_KINDS: &[&str] = &[
    NOQUEUE, "htb", "hfsc", "cbq", "drr", "qfq", "ets", "prio", "multiq", "mqprio", "taprio",
    "tbf", "netem",
];

/// How deep `managed_devices` follows lower_* links under a noqueue device:
/// a VLAN on a bond in a bridge is three levels, and nothing real is deeper.
const MAX_LOWER_DEPTH: usize = 4;

/// Bound on one `tc` invocation. A tc that runs past it is killed and handed
/// to a reaper thread, and the caller gets an error at once instead of
/// waiting for it to exit. That is the point: a tc blocked on the rtnl lock
/// sleeps uninterruptibly, SIGKILL takes effect only once the lock is free,
/// and a blocking wait() would hold the guard's lock -- and `Drain`, which
/// needs that lock before it writes fallback_cc -- for as long as rtnl is
/// held. While a killed tc has not exited, every further `tc` fails at once
/// ("skipped", see `run_tc`) instead of starting another one that would
/// block the same way. What that delivers: one stuck rtnl costs a pass --
/// and a `Drain` or a status request waiting behind it -- about one
/// TC_TIMEOUT, not one per tc call, and never an unbounded wait.
const TC_TIMEOUT: Duration = Duration::from_secs(10);

/// `tc` processes `run_bounded` killed that have not exited yet (see
/// `TC_TIMEOUT`). Process-wide: the rtnl lock they wait on is host-wide too.
static TC_UNREAPED: AtomicUsize = AtomicUsize::new(0);

/// A replace that failed is retried after this long at the earliest (or
/// after one interval, when that is longer)...
const RETRY_BACKOFF_MIN: Duration = Duration::from_secs(30);
/// ...doubling on each consecutive failure of the same root, up to this.
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// What `[guard]` asks for, fixed at daemon start.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    interval: Duration,
    interval_s: u32,
    qdisc: bool,
    interface: Option<String>,
}

impl Settings {
    pub(crate) fn from_config(config: &SkylineConfig) -> Self {
        Self {
            interval: Duration::from_secs(u64::from(config.guard.interval_s)),
            interval_s: config.guard.interval_s,
            qdisc: config.guard.qdisc,
            interface: config.runtime.tc_interface.clone(),
        }
    }
}

/// Everything the guard reads or changes on the host, behind a trait so a
/// pass can be tested without root, a NIC, or iproute2.
pub(crate) trait Host: Send + Sync {
    fn congestion_control(&self) -> Result<String>;
    fn set_congestion_control(&self, name: &str) -> Result<()>;
    fn default_qdisc(&self) -> Result<String>;
    fn set_default_qdisc(&self, kind: &str) -> Result<()>;
    fn interface_exists(&self, interface: &str) -> bool;
    /// Number of transmit queues; 1 when it cannot be read.
    fn tx_queues(&self, interface: &str) -> usize;
    /// The devices behind /sys/class/net/<if>/lower_*: a VLAN's real device,
    /// a bond's slaves, a bridge's ports, a macvlan's parent. Sorted; empty
    /// when there are none or they cannot be read.
    fn lower_devices(&self, interface: &str) -> Vec<String>;
    /// Whether /sys/class/net/<if>/device exists: a NIC with a (physical or
    /// virtio) device behind it. Taps, veths, ifbs, tunnels and the stacked
    /// devices themselves have none.
    fn has_device_link(&self, interface: &str) -> bool;
    /// Runs `tc` and returns its stdout; a non-zero exit is an error.
    fn tc(&self, args: &[&str]) -> Result<String>;
}

pub(crate) struct SystemHost;

fn read_sysctl(path: &str) -> Result<String> {
    Ok(fs::read_to_string(path)
        .with_context(|| format!("read {path}"))?
        .trim()
        .to_owned())
}

impl Host for SystemHost {
    fn congestion_control(&self) -> Result<String> {
        read_sysctl(CONGESTION_CONTROL_SYSCTL)
    }

    fn set_congestion_control(&self, name: &str) -> Result<()> {
        set_default_congestion_control(name)
    }

    fn default_qdisc(&self) -> Result<String> {
        read_sysctl(DEFAULT_QDISC_SYSCTL)
    }

    fn set_default_qdisc(&self, kind: &str) -> Result<()> {
        // The kernel loads sch_<kind> itself on this write
        // (qdisc_set_default() -> request_module), which is why the guard
        // needs no `modprobe sch_fq` of its own.
        fs::write(DEFAULT_QDISC_SYSCTL, kind)
            .with_context(|| format!("set default_qdisc to {kind}"))
    }

    fn interface_exists(&self, interface: &str) -> bool {
        Path::new(SYS_CLASS_NET).join(interface).is_dir()
    }

    fn tx_queues(&self, interface: &str) -> usize {
        let queues = Path::new(SYS_CLASS_NET).join(interface).join("queues");
        fs::read_dir(queues)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("tx-"))
                    .count()
            })
            .unwrap_or(1)
            .max(1)
    }

    fn lower_devices(&self, interface: &str) -> Vec<String> {
        let mut lowers: Vec<String> = fs::read_dir(Path::new(SYS_CLASS_NET).join(interface))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter_map(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .and_then(|name| name.strip_prefix("lower_"))
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        lowers.sort();
        lowers
    }

    fn has_device_link(&self, interface: &str) -> bool {
        Path::new(SYS_CLASS_NET)
            .join(interface)
            .join("device")
            .exists()
    }

    fn tc(&self, args: &[&str]) -> Result<String> {
        run_tc(args, &TC_UNREAPED)
    }
}

/// `tc` with `TC_TIMEOUT`, or an immediate error while a `tc` killed earlier
/// has not exited: it is most likely still asleep on the rtnl lock, and a new
/// one would only join it and cost the caller another full timeout.
fn run_tc(args: &[&str], unreaped: &'static AtomicUsize) -> Result<String> {
    if unreaped.load(Ordering::SeqCst) > 0 {
        bail!(
            "tc {}: a previous tc has not exited yet (rtnl lock held?); skipped",
            args.join(" ")
        );
    }
    run_bounded("tc", args, TC_TIMEOUT, unreaped).map_err(|error| {
        if error.downcast_ref::<io::Error>().map(io::Error::kind) == Some(io::ErrorKind::NotFound) {
            error.context("tc is not installed (iproute2)")
        } else {
            error
        }
    })
}

/// Waits for a killed `child` on a thread of its own, counting it in
/// `unreaped` until it has exited.
fn reap_later(mut child: process::Child, unreaped: &'static AtomicUsize) {
    unreaped.fetch_add(1, Ordering::SeqCst);
    let spawned = thread::Builder::new()
        .name("skyline-guard-reap".to_owned())
        .spawn(move || {
            let _ = child.wait();
            unreaped.fetch_sub(1, Ordering::SeqCst);
        });
    if spawned.is_err() {
        // The closure, and the Child in it, is dropped unwaited: the process
        // stays a zombie until the daemon exits. Still better than blocking
        // here -- but nothing will ever count it down, so do not let it
        // block every later tc for good.
        unreaped.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs `program` and returns its stdout, killing it after `timeout`. A
/// non-zero exit is an error carrying its stderr. A program killed at the
/// timeout (or that could not be waited for) is reaped in the background and
/// counted in `unreaped` until it exits; this returns at once either way.
fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
    unreaped: &'static AtomicUsize,
) -> Result<String> {
    let rendered = format!("{program} {}", args.join(" "));
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {rendered}"))?;
    // stdout is drained on its own thread: an mq on a NIC with a few hundred
    // queues prints more than a pipe holds, and tc would then block on the
    // full pipe while this thread waits for it to exit.
    let reader = match child.stdout.take() {
        Some(mut stdout) => {
            let spawned = thread::Builder::new()
                .name("skyline-guard-out".to_owned())
                .spawn(move || {
                    let mut text = String::new();
                    let _ = stdout.read_to_string(&mut text);
                    text
                });
            match spawned {
                Ok(reader) => Some(reader),
                Err(error) => {
                    let _ = child.kill();
                    reap_later(child, unreaped);
                    return Err(error).with_context(|| format!("read the output of {rendered}"));
                }
            }
        }
        None => None,
    };
    let deadline = Instant::now() + timeout;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                break Err(anyhow!(
                    "{rendered} did not finish within {}ms and was killed",
                    timeout.as_millis()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                break Err(anyhow!(error).context(format!("wait for {rendered}")));
            }
        }
    };
    let status = match exited {
        Ok(status) => status,
        Err(error) => {
            // Not waited for here: a tc asleep on the rtnl lock dies only
            // once the lock is free, and wait() would block until then (see
            // TC_TIMEOUT). The reader is left detached rather than joined for
            // the same reason, and because a program that handed its stdout
            // to a child of its own would keep that pipe open after the kill.
            reap_later(child, unreaped);
            return Err(error);
        }
    };
    let stdout = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    // Read only after exit: what tc prints on stderr is one error line.
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    bail!("{rendered} failed ({status}): {}", stderr.trim())
}

/// One mq child as `tc qdisc show` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MqChild {
    kind: String,
    /// The parent exactly as tc printed it: "8001:1", or ":1" under an mq with
    /// handle 0. Also what `tc qdisc replace ... parent` takes back.
    parent: String,
    /// See `cake_shaping`.
    shaping: Option<String>,
}

/// An interface's root qdisc, from `tc qdisc show dev <if>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootQdisc {
    kind: String,
    /// Major number of the root's handle ("8001:" -> 0x8001). 0 is the handle
    /// the kernel gives a qdisc it attaches by itself.
    major: u32,
    /// See `cake_shaping`.
    shaping: Option<String>,
    /// Only for mq: its per-queue children.
    children: Vec<MqChild>,
    /// Every major number the output uses anywhere -- handles and parents,
    /// clsact's ffff: included -- so a new handle can avoid all of them.
    majors: BTreeSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// fq, or mq whose every child is fq.
    Fq,
    /// A kind in `REPLACEABLE_KINDS` (at the root, or among mq's children
    /// next to fq): replace it.
    Replace,
    /// A classful/shaping/offload kind or a shaped cake, at the root or under
    /// mq; noqueue on a NIC.
    Deliberate,
    /// A kind in neither list, or an mq that shows no children.
    Unknown,
}

fn kind_verdict(kind: &str, shaping: bool) -> Verdict {
    if kind == FQ {
        Verdict::Fq
    } else if shaping || DELIBERATE_KINDS.contains(&kind) {
        Verdict::Deliberate
    } else if REPLACEABLE_KINDS.contains(&kind) {
        Verdict::Replace
    } else {
        Verdict::Unknown
    }
}

/// For a cake, what makes it a shaper somebody configured: "bandwidth 90Mbit"
/// (tc prints `bandwidth unlimited` for a cake nobody gave a rate, which is
/// what `default_qdisc=cake` creates) or "autorate-ingress". `None` for an
/// unshaped cake and for every other kind.
fn cake_shaping(kind: &str, options: &[&str]) -> Option<String> {
    if kind != "cake" {
        return None;
    }
    let bandwidth = options
        .windows(2)
        .find(|pair| pair[0] == "bandwidth")
        .map(|pair| pair[1])
        .filter(|rate| *rate != "unlimited");
    if let Some(rate) = bandwidth {
        return Some(format!("bandwidth {rate}"));
    }
    options
        .contains(&"autorate-ingress")
        .then(|| "autorate-ingress".to_owned())
}

/// "8001:" / "8001:1" / ":1" -> the major number (0 when empty).
fn major_of(id: &str) -> Option<u32> {
    let major = id.split(':').next()?;
    if major.is_empty() {
        return Some(0);
    }
    u32::from_str_radix(major, 16).ok()
}

struct QdiscLine<'a> {
    kind: &'a str,
    major: u32,
    /// `None` for the root.
    parent: Option<&'a str>,
    /// Everything after `root` / `parent <id>`.
    options: Vec<&'a str>,
}

/// `qdisc <kind> <handle>: [dev <if>] root|parent <id> <options...>`.
/// Continuation lines (mqprio and taprio print several) do not start with
/// "qdisc" and are skipped.
fn parse_line(line: &str) -> Option<QdiscLine<'_>> {
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "qdisc" {
        return None;
    }
    let kind = tokens.next()?;
    let major = major_of(tokens.next()?)?;
    // "root"/"parent" comes right after the handle, or after "dev <if>" when
    // tc was not given a device. Looking no further keeps a word inside some
    // qdisc's options from ever being mistaken for it.
    for _ in 0..3 {
        let parent = match tokens.next()? {
            "root" => None,
            "parent" => Some(tokens.next()?),
            _ => continue,
        };
        return Some(QdiscLine {
            kind,
            major,
            parent,
            options: tokens.collect(),
        });
    }
    None
}

impl RootQdisc {
    /// `None` when the output shows no root qdisc at all.
    pub(crate) fn parse(output: &str) -> Option<Self> {
        let all: Vec<QdiscLine<'_>> = output.lines().filter_map(parse_line).collect();
        let majors: BTreeSet<u32> = all
            .iter()
            .flat_map(|line| [Some(line.major), line.parent.and_then(major_of)])
            .flatten()
            .collect();
        let lines: Vec<&QdiscLine<'_>> = all
            .iter()
            // clsact/ingress hang off ffff: (parent ffff:fff1). skyline_tc lives
            // on clsact, and a root replace never touches it.
            .filter(|line| {
                !matches!(line.kind, "clsact" | "ingress")
                    && line.major != 0xffff
                    && line.parent.and_then(major_of) != Some(0xffff)
            })
            .collect();
        let root = lines.iter().find(|line| line.parent.is_none())?;
        let children = if root.kind == "mq" {
            lines
                .iter()
                .filter_map(|line| {
                    let parent = line.parent?;
                    (major_of(parent) == Some(root.major)).then(|| MqChild {
                        kind: line.kind.to_owned(),
                        parent: parent.to_owned(),
                        shaping: cake_shaping(line.kind, &line.options),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        Some(Self {
            kind: root.kind.to_owned(),
            major: root.major,
            shaping: cake_shaping(root.kind, &root.options),
            children,
            majors,
        })
    }

    /// "fq", "cake", "mq/fq", "mq/cake,fq": mq followed by its children's
    /// distinct kinds, sorted (the format `GuardLive::interface_qdisc`
    /// documents).
    pub(crate) fn summary(&self) -> String {
        if self.kind != "mq" || self.children.is_empty() {
            return self.kind.clone();
        }
        let mut kinds: Vec<&str> = self
            .children
            .iter()
            .map(|child| child.kind.as_str())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        format!("mq/{}", kinds.join(","))
    }

    /// Why a cake here counts as deliberate, for the note: " (bandwidth
    /// 90Mbit)" -- the root's own, or the first shaped mq child's.
    fn shaping_detail(&self) -> String {
        self.shaping
            .as_deref()
            .or_else(|| {
                self.children
                    .iter()
                    .find_map(|child| child.shaping.as_deref())
            })
            .map(|shaping| format!(" ({shaping})"))
            .unwrap_or_default()
    }

    pub(crate) fn verdict(&self) -> Verdict {
        if self.kind != "mq" {
            return kind_verdict(&self.kind, self.shaping.is_some());
        }
        if self.children.is_empty() {
            return Verdict::Unknown;
        }
        let verdicts: Vec<Verdict> = self
            .children
            .iter()
            .map(|child| kind_verdict(&child.kind, child.shaping.is_some()))
            .collect();
        // Anything built on purpose under mq wins over a replaceable sibling:
        // the whole tree is left alone.
        for verdict in [Verdict::Deliberate, Verdict::Unknown, Verdict::Replace] {
            if verdicts.contains(&verdict) {
                return verdict;
            }
        }
        Verdict::Fq
    }

    /// A handle major nothing on the device uses, from 8000: up -- the range
    /// the kernel allocates from itself. Hand-written tc scripts pick small
    /// handles (`root handle 1: htb`), and a root that already carries the
    /// handle a later `tc qdisc replace ... root handle 1: <other kind>` names
    /// turns that command into a kind-mismatched "change", which the kernel
    /// rejects -- and a following `parent 1:3 ...` then lands under our mq, on
    /// one tx queue. The kernel's own allocator (qdisc_alloc_handle) skips
    /// handles in use, so it cannot collide with this one either. ffff: is
    /// clsact's/ingress's, and in `majors` whenever it is in use.
    fn unused_major(&self) -> Option<u32> {
        (0x8000..=0xfffe).find(|major| !self.majors.contains(major))
    }

    /// How to turn a `Verdict::Replace` root into fq. `default_is_fq`:
    /// net.core.default_qdisc reads back fq right now, so a freshly created
    /// mq gets fq children.
    pub(crate) fn replace_plan(
        &self,
        interface: &str,
        tx_queues: usize,
        default_is_fq: bool,
        round: Round,
    ) -> Plan {
        let command = |tail: &[&str]| -> Vec<String> {
            ["qdisc", "replace", "dev", interface]
                .iter()
                .chain(tail)
                .map(|arg| (*arg).to_owned())
                .collect()
        };
        if self.kind == "mq" && self.major != 0 {
            // An mq that tc created (handle != 0). `replace root mq` on it is a
            // same-kind "change", which the kernel accepts and ignores -- mq has
            // no parameters.
            let odd: Vec<&MqChild> = self
                .children
                .iter()
                .filter(|child| child.kind != FQ)
                .collect();
            if odd.len() > 1
                && odd.len() == self.children.len()
                && default_is_fq
                && round == Round::First
            {
                // Several children to fix, and none of them fq yet (a fresh mq
                // rebuilds every child from default_qdisc with default
                // parameters, so an fq sibling an operator tuned -- maxrate,
                // say -- is fixed child by child instead, and kept): one
                // fresh mq under a handle nothing
                // uses. A new handle makes the kernel create and graft a new
                // mq -- one device deactivation, every child built from
                // default_qdisc (fq) -- where replacing child by child goes
                // through mq_graft, which deactivates the whole device (and
                // drops what every queue holds) once per child: 64 resets in a
                // row on a 64-queue NIC.
                if let Some(major) = self.unused_major() {
                    let handle = format!("{major:x}:");
                    return Plan::Run(vec![command(&["root", "handle", handle.as_str(), "mq"])]);
                }
            }
            // One child, a default that is not fq (the per-child command names
            // fq itself), or the fallback when a fresh mq still did not come
            // out all fq.
            return Plan::Run(
                odd.iter()
                    .map(|child| command(&["parent", child.parent.as_str(), FQ]))
                    .collect(),
            );
        }
        if tx_queues > 1 {
            // A single fq root would serialize a multi-queue NIC onto one
            // lock, so a fresh mq -- which creates one child per queue from
            // default_qdisc. Only when that is fq: from any other default
            // (cake, say, because the write of fq failed) it would install
            // exactly what the guard exists to remove, and per-child fixes
            // are not possible under the kernel's own handle-0 mq.
            if !default_is_fq {
                return Plan::FreshMqNeedsFqDefault;
            }
            Plan::Run(vec![command(&["root", "mq"])])
        } else {
            Plan::Run(vec![command(&["root", FQ])])
        }
    }
}

/// Which attempt `replace_root` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Round {
    First,
    /// The first round's result is still not all fq.
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Run these `tc` argument lists, in order.
    Run(Vec<Vec<String>>),
    /// Only a fresh mq fits, and it would take its children from a
    /// default_qdisc that is not fq.
    FreshMqNeedsFqDefault,
}

/// `Ok(None)` when tc shows no root qdisc at all: a device that is down
/// still has the kernel's built-in noop qdisc, which tc does not list. When it
/// comes up the kernel attaches the default -- fq, once the pass has set it.
fn read_root(host: &dyn Host, interface: &str) -> Result<Option<RootQdisc>> {
    let output = host.tc(&["qdisc", "show", "dev", interface])?;
    Ok(RootQdisc::parse(&output))
}

/// Replaces a `Verdict::Replace` root and verifies the result; returns the new
/// summary. `label` names the device in messages ("eth0 (under bond0)").
fn replace_root(host: &dyn Host, name: &str, label: &str, before: &RootQdisc) -> Result<String> {
    let tx_queues = host.tx_queues(name);
    let mut current = before.clone();
    // Two rounds at most. The second only runs when the first did not end
    // all fq -- something rewrote default_qdisc between this pass's write and
    // the fresh mq, say -- and replaces what is left child by child under the
    // mq the first round created.
    for round in [Round::First, Round::Fallback] {
        if current.verdict() != Verdict::Replace {
            break;
        }
        // Read back rather than trusted from this pass's own write: that
        // write can fail, and something else can change it in between.
        let default_qdisc = host.default_qdisc();
        let default_is_fq = matches!(&default_qdisc, Ok(kind) if kind == FQ);
        let commands = match current.replace_plan(name, tx_queues, default_is_fq, round) {
            Plan::Run(commands) => commands,
            Plan::FreshMqNeedsFqDefault => {
                let summary = current.summary();
                match default_qdisc {
                    Ok(kind) => bail!(
                        "{label} root qdisc {summary} left alone: default_qdisc is {kind}, a \
                         fresh mq would get {kind} children"
                    ),
                    Err(error) => bail!(
                        "{label} root qdisc {summary} left alone: default_qdisc could not be \
                         read ({error:#}), so a fresh mq might not get fq children"
                    ),
                }
            }
        };
        for command in commands {
            let args: Vec<&str> = command.iter().map(String::as_str).collect();
            let result = host.tc(&args);
            if round == Round::Fallback {
                // Say what the first round already changed, rather than
                // hide it behind tc's own error text.
                result.with_context(|| {
                    format!(
                        "{label} root qdisc is {} after the first replace",
                        current.summary()
                    )
                })?;
            } else {
                result?;
            }
        }
        current = read_root(host, name)?.ok_or_else(|| {
            anyhow!("tc qdisc show dev {name} printed no root qdisc after the replace")
        })?;
    }
    let after = current.summary();
    if current.verdict() != Verdict::Fq {
        bail!(
            "{label} root qdisc is still {after} after replacing {} with fq",
            before.summary()
        );
    }
    Ok(after)
}

/// One device whose root qdisc the guard manages, as `managed_devices`
/// found it.
struct Target {
    name: String,
    /// `name`, plus the stacked devices it sits under: "eth0 (under bond0)",
    /// "eth0 (under bond0 under vmbr0)".
    label: String,
    /// Its root qdisc, already read (the resolution needs it anyway).
    root: Result<Option<RootQdisc>, String>,
}

fn label(device: &str, uppers: &[String]) -> String {
    if uppers.is_empty() {
        device.to_owned()
    } else {
        format!("{device} (under {})", uppers.join(" under "))
    }
}

fn is_noqueue(root: &Result<Option<RootQdisc>, String>) -> bool {
    matches!(root, Ok(Some(root)) if root.kind == NOQUEUE)
}

/// The devices whose root qdisc the guard manages for `runtime.tc_interface`.
struct Managed {
    targets: Vec<Target>,
    /// tc_interface's root is noqueue and no NIC was found under it (a kernel
    /// WireGuard device, or a bridge of VM taps only). A tun or PPP device
    /// has a qdisc of its own, is not noqueue, and so is managed itself.
    nothing_under_noqueue: bool,
}

/// `interface` itself, unless its root is noqueue. noqueue is the kernel's
/// default on a VLAN, bond, bridge or macvlan (and on veths and tunnels):
/// such a device queues nothing, and what TCP's packets meet is the root
/// qdisc of the NIC underneath -- which is where a one-click script's cake
/// sits. So under noqueue the lower_* links are followed and the NICs found
/// there are managed instead: bond slaves, a VLAN's real device, a bridge's
/// physical ports. A lower without a `device` link (a VM's tap, a
/// container's veth, an ifb) is never managed: a bridge's VM and container
/// ports are not this host's egress and not ours to change.
fn managed_devices(
    host: &dyn Host,
    interface: &str,
    root: Result<Option<RootQdisc>, String>,
) -> Managed {
    let mut targets = Vec::new();
    let noqueue = is_noqueue(&root);
    if noqueue {
        let mut visited = BTreeSet::from([interface.to_owned()]);
        follow_noqueue(host, interface, &[], root, 0, &mut visited, &mut targets);
    } else {
        targets.push(Target {
            name: interface.to_owned(),
            label: interface.to_owned(),
            root,
        });
    }
    Managed {
        nothing_under_noqueue: noqueue && targets.is_empty(),
        targets,
    }
}

/// `device`'s root is noqueue; `uppers` are the devices above it, nearest
/// first, and `depth` their count.
fn follow_noqueue(
    host: &dyn Host,
    device: &str,
    uppers: &[String],
    root: Result<Option<RootQdisc>, String>,
    depth: usize,
    visited: &mut BTreeSet<String>,
    targets: &mut Vec<Target>,
) {
    let lowers = host.lower_devices(device);
    if lowers.is_empty() {
        // Nothing under it. On a NIC the kernel never puts noqueue by itself,
        // so somebody did: managed, so the pass reports it as deliberate. On
        // anything else it is the default and there is nothing to manage.
        if host.has_device_link(device) {
            targets.push(Target {
                name: device.to_owned(),
                label: label(device, uppers),
                root,
            });
        }
        return;
    }
    if depth >= MAX_LOWER_DEPTH {
        return;
    }
    let below: Vec<String> = std::iter::once(device.to_owned())
        .chain(uppers.iter().cloned())
        .collect();
    for lower in lowers {
        if !visited.insert(lower.clone()) {
            continue;
        }
        let device_link = host.has_device_link(&lower);
        if !device_link && host.lower_devices(&lower).is_empty() {
            // A tap, veth or ifb with nothing under it can never be managed,
            // whatever its root: skip it without running tc (a bridge with
            // fifty VMs has fifty of them).
            continue;
        }
        let root = read_root(host, &lower).map_err(|error| format!("{error:#}"));
        if is_noqueue(&root) {
            follow_noqueue(host, &lower, &below, root, depth + 1, visited, targets);
        } else if device_link {
            targets.push(Target {
                label: label(&lower, &below),
                name: lower,
                root,
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Enable,
    Periodic,
}

/// What one pass did, found, and could not do.
#[derive(Debug, Default)]
pub(crate) struct PassReport {
    pub(crate) corrections: Vec<String>,
    pub(crate) notes: Vec<String>,
    pub(crate) errors: Vec<String>,
    /// The lines this pass wrote to the journal.
    logged: Vec<String>,
}

impl PassReport {
    /// Everything the operator who just ran `ssctl enable` should read.
    pub(crate) fn lines(&self) -> impl Iterator<Item = &String> {
        self.corrections
            .iter()
            .chain(&self.notes)
            .chain(&self.errors)
    }
}

/// A device whose root a replace failed to turn into fq.
#[derive(Debug)]
struct Stuck {
    /// The root summary the failed replace started from. A different one
    /// found later is tried at once: it is not what failed.
    summary: String,
    /// Consecutive failures on this summary.
    failures: u32,
    backoff: Duration,
    retry_at: Instant,
}

/// max(interval, 30 s), doubled per consecutive failure after the first,
/// capped at 5 min (or one interval, when that is longer), then rounded up to
/// whole intervals: a retry only ever happens on a periodic pass, so this is
/// when it really does, and what "retrying in N s" says. Not every interval: a replace that the kernel accepts
/// and then does not keep deactivates the device for a moment each time.
/// Not never: most failures are transient (a netlink ENOMEM, a tc killed at
/// TC_TIMEOUT before it sent anything), and a pass that stopped trying after
/// one of them would leave the NIC on cake until the next `ssctl enable`.
fn retry_backoff(interval: Duration, failures: u32) -> Duration {
    let base = interval.max(RETRY_BACKOFF_MIN);
    let doublings = failures.saturating_sub(1).min(16);
    let wanted = base
        .saturating_mul(1 << doublings)
        .min(RETRY_BACKOFF_MAX.max(interval));
    if interval.is_zero() {
        return wanted;
    }
    let steps = wanted.as_nanos().div_ceil(interval.as_nanos()).max(1);
    interval.saturating_mul(u32::try_from(steps).unwrap_or(u32::MAX))
}

fn stuck_note(label: &str, stuck: &Stuck, interval: Duration) -> String {
    if interval.is_zero() {
        format!(
            "{label} root qdisc {}: the last replace failed; retried on the next ssctl enable \
             ([guard] interval_s = 0)",
            stuck.summary
        )
    } else {
        // The step, not the time left: the note stays the same line from pass
        // to pass, so it reaches the journal once per step, not every pass.
        format!(
            "{label} root qdisc {}: the last replace failed; retrying in {} s",
            stuck.summary,
            stuck.backoff.as_secs()
        )
    }
}

#[derive(Debug, Default)]
struct GuardState {
    armed: bool,
    checks: u64,
    cc_restored: u64,
    default_qdisc_restored: u64,
    interface_qdisc_replaced: u64,
    last_correction: Option<String>,
    last_correction_unix_s: Option<u64>,
    last_error: Option<String>,
    notes: Vec<String>,
    /// The previous pass's notes and errors. A note or error is logged only
    /// when it was not already there: a deliberate htb root or a missing `tc`
    /// would otherwise write the same line to the journal every interval_s.
    reported: Vec<String>,
    /// Per managed device (by name): a replace that failed, and when the
    /// periodic pass tries it again (see `retry_backoff`). Cleared when the
    /// device reads fq, on a successful replace, and by `ssctl enable`.
    stuck: BTreeMap<String, Stuck>,
}

fn lock(state: &Mutex<GuardState>) -> MutexGuard<'_, GuardState> {
    // A panic in another holder leaves plain counters and strings behind,
    // nothing half-written that matters more than keeping the guard alive.
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn enforce_congestion_control(host: &dyn Host, state: &mut GuardState, report: &mut PassReport) {
    match host.congestion_control() {
        Ok(current) if current == SKYLINE_CC_NAME => {}
        Ok(current) => match host.set_congestion_control(SKYLINE_CC_NAME) {
            Ok(()) => {
                state.cc_restored = state.cc_restored.saturating_add(1);
                report.corrections.push(format!(
                    "tcp_congestion_control {current} -> {SKYLINE_CC_NAME}"
                ));
            }
            Err(error) => report.errors.push(format!(
                "tcp_congestion_control is {current} and could not be put back to \
                 {SKYLINE_CC_NAME}: {error:#}"
            )),
        },
        Err(error) => report.errors.push(format!("{error:#}")),
    }
}

fn enforce_default_qdisc(host: &dyn Host, state: &mut GuardState, report: &mut PassReport) {
    match host.default_qdisc() {
        Ok(current) if current == FQ => {}
        Ok(current) => match host.set_default_qdisc(FQ) {
            Ok(()) => {
                state.default_qdisc_restored = state.default_qdisc_restored.saturating_add(1);
                report
                    .corrections
                    .push(format!("default_qdisc {current} -> {FQ}"));
            }
            Err(error) => report.errors.push(format!(
                "default_qdisc is {current} and could not be set to {FQ}: {error:#}"
            )),
        },
        Err(error) => report.errors.push(format!("{error:#}")),
    }
}

/// Where a pass is: what it runs for, and when.
#[derive(Clone, Copy)]
struct PassContext {
    trigger: Trigger,
    now: Instant,
    interval: Duration,
}

fn enforce_interface_qdiscs(
    interface: &str,
    host: &dyn Host,
    state: &mut GuardState,
    pass: PassContext,
    report: &mut PassReport,
) {
    if !host.interface_exists(interface) {
        state.stuck.clear();
        report.notes.push(format!(
            "TC interface {interface} does not exist; its root qdisc is not checked"
        ));
        return;
    }
    let root = read_root(host, interface).map_err(|error| format!("{error:#}"));
    let managed = managed_devices(host, interface, root);
    if managed.nothing_under_noqueue {
        report.notes.push(format!(
            "{interface} is a virtual device (noqueue is its kernel default) and no NIC under \
             it is visible to the guard; no qdisc checked"
        ));
    }
    state.stuck.retain(|name, _| {
        managed
            .targets
            .iter()
            .any(|target| target.name == name.as_str())
    });
    for target in managed.targets {
        enforce_device(target, host, state, pass, report);
    }
}

fn enforce_device(
    target: Target,
    host: &dyn Host,
    state: &mut GuardState,
    pass: PassContext,
    report: &mut PassReport,
) {
    let Target { name, label, root } = target;
    let before = match root {
        Ok(Some(root)) => root,
        Ok(None) => {
            report.notes.push(format!(
                "{label} shows no root qdisc (is it down?); not checked"
            ));
            return;
        }
        Err(error) => {
            report.errors.push(error);
            return;
        }
    };
    let summary = before.summary();
    match before.verdict() {
        Verdict::Fq => {
            state.stuck.remove(&name);
            return;
        }
        Verdict::Deliberate => {
            state.stuck.remove(&name);
            report.notes.push(format!(
                "{label} root qdisc {summary}{} looks deliberate; left alone",
                before.shaping_detail()
            ));
            return;
        }
        Verdict::Unknown => {
            state.stuck.remove(&name);
            report.notes.push(format!(
                "{label} root qdisc {summary} is not a kind the guard knows is safe to \
                 replace; left alone"
            ));
            return;
        }
        Verdict::Replace => {}
    }
    if pass.trigger == Trigger::Periodic {
        if let Some(stuck) = state
            .stuck
            .get(&name)
            .filter(|stuck| stuck.summary == summary && pass.now < stuck.retry_at)
        {
            report.notes.push(stuck_note(&label, stuck, pass.interval));
            return;
        }
    }
    match replace_root(host, &name, &label, &before) {
        Ok(after) => {
            state.stuck.remove(&name);
            state.interface_qdisc_replaced = state.interface_qdisc_replaced.saturating_add(1);
            report
                .corrections
                .push(format!("{label} root qdisc {summary} -> {after}"));
        }
        Err(error) => {
            let failures = match state.stuck.get(&name) {
                Some(stuck) if stuck.summary == summary => stuck.failures.saturating_add(1),
                _ => 1,
            };
            let backoff = retry_backoff(pass.interval, failures);
            let stuck = Stuck {
                summary,
                failures,
                backoff,
                retry_at: pass.now + backoff,
            };
            report.errors.push(format!("{error:#}"));
            report.notes.push(stuck_note(&label, &stuck, pass.interval));
            state.stuck.insert(name, stuck);
        }
    }
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// One full enforcement pass. The caller holds the lock and has checked that
/// the guard is armed.
fn enforce(
    settings: &Settings,
    host: &dyn Host,
    state: &mut GuardState,
    trigger: Trigger,
    now: Instant,
) -> PassReport {
    let mut report = PassReport::default();
    enforce_congestion_control(host, state, &mut report);
    if settings.qdisc {
        // default_qdisc first: a fresh mq below takes its children from it.
        enforce_default_qdisc(host, state, &mut report);
        if let Some(interface) = &settings.interface {
            let pass = PassContext {
                trigger,
                now,
                interval: settings.interval,
            };
            enforce_interface_qdiscs(interface, host, state, pass, &mut report);
        }
    }

    // Every correction is logged, every time: they are real events, and the
    // journal is the operator's only clue that something on the host keeps
    // fighting them.
    let why = match trigger {
        Trigger::Enable => "applied by ssctl enable",
        Trigger::Periodic => "something else on this host changed it",
    };
    for correction in &report.corrections {
        report.logged.push(format!("guard: {correction} ({why})"));
    }
    if let Some(correction) = report.corrections.last() {
        state.last_correction = Some(correction.clone());
        state.last_correction_unix_s = unix_now();
    }
    let current: Vec<String> = report.notes.iter().chain(&report.errors).cloned().collect();
    for message in &current {
        if !state.reported.contains(message) {
            report.logged.push(format!("guard: {message}"));
        }
    }
    for line in &report.logged {
        eprintln!("{line}");
    }
    if let Some(error) = report.errors.last() {
        state.last_error = Some(error.clone());
    }
    state.notes = current.clone();
    state.reported = current;
    report
}

/// One tick of the periodic thread: nothing unless armed.
fn periodic_pass(
    settings: &Settings,
    host: &dyn Host,
    state: &Mutex<GuardState>,
    now: Instant,
) -> Option<PassReport> {
    let mut state = lock(state);
    if !state.armed {
        return None;
    }
    state.checks = state.checks.saturating_add(1);
    Some(enforce(settings, host, &mut state, Trigger::Periodic, now))
}

struct Worker {
    /// Dropping this wakes the thread's recv_timeout() at once, so stopping
    /// never waits out an interval.
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

pub(crate) struct Guard {
    settings: Settings,
    host: Arc<dyn Host>,
    state: Arc<Mutex<GuardState>>,
    worker: Option<Worker>,
}

impl Guard {
    pub(crate) fn new(config: &SkylineConfig) -> Self {
        Self::with_host(Settings::from_config(config), Arc::new(SystemHost))
    }

    fn with_host(settings: Settings, host: Arc<dyn Host>) -> Self {
        Self {
            settings,
            host,
            state: Arc::new(Mutex::new(GuardState::default())),
            worker: None,
        }
    }

    /// Starts the periodic thread (serve() only -- `--validate-only` must not
    /// leave one behind). A no-op when `interval_s = 0`.
    pub(crate) fn start(&mut self) -> Result<()> {
        if self.worker.is_some() || self.settings.interval.is_zero() {
            return Ok(());
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let settings = self.settings.clone();
        let host = Arc::clone(&self.host);
        let state = Arc::clone(&self.state);
        let thread = thread::Builder::new()
            .name("skyline-guard".to_owned())
            .spawn(move || loop {
                match stopped.recv_timeout(settings.interval) {
                    Err(RecvTimeoutError::Timeout) => {
                        // Already logged inside; nobody else reads a
                        // periodic report.
                        let _ = periodic_pass(&settings, &*host, &state, Instant::now());
                    }
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                }
            })
            .context("spawn the guard thread")?;
        self.worker = Some(Worker { stop, thread });
        Ok(())
    }

    /// Stops and joins the periodic thread. Idempotent. Must run before
    /// `BpfRuntime` drops -- see `Daemon`'s `Drop`.
    pub(crate) fn stop(&mut self) {
        if let Some(Worker { stop, thread }) = self.worker.take() {
            drop(stop);
            let _ = thread.join();
        }
    }

    /// `Request::Enable`, after skyline_cc is attached and already the
    /// default: arm, then run one full pass right away (also when
    /// `interval_s = 0`). Never fails -- the report carries what went wrong.
    pub(crate) fn arm_and_enforce(&self) -> PassReport {
        let mut state = lock(&self.state);
        state.armed = true;
        // The operator just asked: log what is still wrong again, and retry a
        // replace that failed last time without waiting out its backoff.
        state.reported.clear();
        state.stuck.clear();
        enforce(
            &self.settings,
            &*self.host,
            &mut state,
            Trigger::Enable,
            Instant::now(),
        )
    }

    /// `Request::Drain`: disarm, then run `then` (the fallback_cc write) while
    /// still holding the lock. A pass already running finishes first -- each
    /// of its tc calls bounded by TC_TIMEOUT -- and none can start between
    /// the two and put skyline_cc back. Stays disarmed whatever `then` or the
    /// rest of the drain returns.
    pub(crate) fn disarm_and<T>(&self, then: impl FnOnce() -> T) -> T {
        let mut state = lock(&self.state);
        state.armed = false;
        then()
    }

    pub(crate) fn status(&self) -> GuardStatus {
        let mut status = {
            let state = lock(&self.state);
            GuardStatus {
                armed: state.armed,
                interval_s: self.settings.interval_s,
                qdisc: self.settings.qdisc,
                checks: state.checks,
                cc_restored: state.cc_restored,
                default_qdisc_restored: state.default_qdisc_restored,
                interface_qdisc_replaced: state.interface_qdisc_replaced,
                last_correction: state.last_correction.clone(),
                last_correction_unix_s: state.last_correction_unix_s,
                last_error: state.last_error.clone(),
                live: GuardLive::default(),
                notes: state.notes.clone(),
            }
        };
        // After the lock is released: the live view needs none of the state,
        // and a periodic tick should not have to wait for this request's own
        // `tc qdisc show`.
        status.live = self.live();
        status
    }

    fn live(&self) -> GuardLive {
        let interface = self
            .settings
            .interface
            .as_deref()
            .filter(|interface| self.host.interface_exists(interface));
        let (interface_qdisc, devices) = match interface {
            None => (None, Vec::new()),
            Some(interface) => {
                let root = read_root(&*self.host, interface).map_err(|error| format!("{error:#}"));
                let interface_qdisc = match &root {
                    Ok(Some(root)) => Some(root.summary()),
                    _ => None,
                };
                // The managed set exists only while the guard owns qdiscs.
                let devices = if self.settings.qdisc {
                    managed_devices(&*self.host, interface, root)
                        .targets
                        .into_iter()
                        .map(|target| GuardDevice {
                            qdisc: target.root.ok().flatten().map(|root| root.summary()),
                            name: target.name,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                (interface_qdisc, devices)
            }
        };
        GuardLive {
            tcp_congestion_control: self.host.congestion_control().unwrap_or_default(),
            default_qdisc: self.host.default_qdisc().unwrap_or_default(),
            interface: self.settings.interface.clone(),
            interface_qdisc,
            devices,
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_runner_reads_output_larger_than_a_pipe() {
        static UNREAPED: AtomicUsize = AtomicUsize::new(0);
        // 256 KiB, four times a default pipe buffer: without the reader thread
        // the child would block writing and be killed at the timeout.
        let output = run_bounded(
            "sh",
            &["-c", "head -c 262144 /dev/zero | tr '\\0' x"],
            Duration::from_secs(20),
            &UNREAPED,
        )
        .expect("run sh");
        assert_eq!(output.len(), 262_144);
        assert!(output.bytes().all(|byte| byte == b'x'));
    }

    #[test]
    fn bounded_runner_reports_the_exit_status_and_stderr() {
        static UNREAPED: AtomicUsize = AtomicUsize::new(0);
        let error = run_bounded(
            "sh",
            &["-c", "echo 'RTNETLINK answers: busy' >&2; exit 2"],
            Duration::from_secs(20),
            &UNREAPED,
        )
        .expect_err("exit 2 is a failure");
        let message = format!("{error:#}");
        assert!(message.contains("RTNETLINK answers: busy"), "{message}");
        assert!(message.contains("exit status: 2"), "{message}");
    }

    #[test]
    fn bounded_runner_kills_what_does_not_finish_and_reaps_it_in_the_background() {
        static UNREAPED: AtomicUsize = AtomicUsize::new(0);
        let started = Instant::now();
        let error = run_bounded(
            "sh",
            &["-c", "exec sleep 30"],
            Duration::from_millis(200),
            &UNREAPED,
        )
        .expect_err("killed at the timeout");
        assert!(format!("{error:#}").contains("did not finish within 200ms"));
        assert!(started.elapsed() < Duration::from_secs(10));
        // The caller did not wait for the exit; the reaper thread does, and
        // counts the process down once it is gone.
        let deadline = Instant::now() + Duration::from_secs(10);
        while UNREAPED.load(Ordering::SeqCst) > 0 {
            assert!(
                Instant::now() < deadline,
                "the killed process was never reaped"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn bounded_runner_reports_a_missing_program() {
        static UNREAPED: AtomicUsize = AtomicUsize::new(0);
        let error = run_bounded(
            "skyline-no-such-program",
            &["qdisc", "show"],
            Duration::from_secs(1),
            &UNREAPED,
        )
        .expect_err("not installed");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn tc_is_skipped_while_a_killed_one_has_not_exited() {
        // One tc still asleep on the rtnl lock: the next one is not even
        // started (on a host without tc this would otherwise say "not
        // installed").
        static UNREAPED: AtomicUsize = AtomicUsize::new(1);
        let error = run_tc(&["qdisc", "show", "dev", "eth0"], &UNREAPED).expect_err("skipped");
        let message = format!("{error:#}");
        assert!(
            message.contains("a previous tc has not exited yet (rtnl lock held?); skipped"),
            "{message}"
        );
    }

    // Captured `tc qdisc show dev <if>` outputs (iproute2 6.x). Trailing
    // spaces are what tc prints.
    const FQ_ROOT: &str = "qdisc fq 8001: root refcnt 2 limit 10000p flow_limit 100p \
buckets 1024 orphan_mask 1023 quantum 3028b initial_quantum 15140b low_rate_threshold 550Kbit \
refill_delay 40ms timer_slack 10us horizon 10s horizon_drop \n";

    const MQ_DEFAULT_FQ: &str = "qdisc mq 0: root \n\
qdisc fq 0: parent :4 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 quantum 3028b \
initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms timer_slack 10us horizon 10s \
horizon_drop \n\
qdisc fq 0: parent :3 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 quantum 3028b \
initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms timer_slack 10us horizon 10s \
horizon_drop \n\
qdisc fq 0: parent :2 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 quantum 3028b \
initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms timer_slack 10us horizon 10s \
horizon_drop \n\
qdisc fq 0: parent :1 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 quantum 3028b \
initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms timer_slack 10us horizon 10s \
horizon_drop \n";

    const MQ_CAKE: &str = "qdisc mq 8002: root \n\
qdisc cake 0: parent 8002:2 bandwidth unlimited diffserv3 triple-isolate nonat nowash \
no-ack-filter split-gso rtt 100ms raw overhead 0 \n\
qdisc cake 0: parent 8002:1 bandwidth unlimited diffserv3 triple-isolate nonat nowash \
no-ack-filter split-gso rtt 100ms raw overhead 0 \n";

    const MQ_DEFAULT_FQ_CODEL: &str = "qdisc mq 0: root \n\
qdisc fq_codel 0: parent :2 limit 10240p flows 1024 quantum 1514 target 5ms interval 100ms \
memory_limit 32Mb ecn drop_batch 64 \n\
qdisc fq_codel 0: parent :1 limit 10240p flows 1024 quantum 1514 target 5ms interval 100ms \
memory_limit 32Mb ecn drop_batch 64 \n";

    const FQ_CODEL_ROOT: &str = "qdisc fq_codel 0: root refcnt 2 limit 10240p flows 1024 \
quantum 1514 target 5ms interval 100ms memory_limit 32Mb ecn drop_batch 64 \n";

    const CAKE_ROOT: &str = "qdisc cake 8003: root refcnt 2 bandwidth unlimited diffserv3 \
triple-isolate nonat nowash no-ack-filter split-gso rtt 100ms raw overhead 0 \n";

    /// `tc qdisc replace dev eth0 root cake bandwidth 90mbit`.
    const CAKE_SHAPED: &str = "qdisc cake 8004: root refcnt 2 bandwidth 90Mbit diffserv3 \
triple-isolate nonat nowash no-ack-filter split-gso rtt 100ms raw overhead 0 \n";

    const MQ_SHAPED_CAKE: &str = "qdisc mq 8005: root \n\
qdisc cake 0: parent 8005:2 bandwidth 45Mbit diffserv3 triple-isolate nonat nowash \
no-ack-filter split-gso rtt 100ms raw overhead 0 \n\
qdisc cake 0: parent 8005:1 bandwidth 45Mbit diffserv3 triple-isolate nonat nowash \
no-ack-filter split-gso rtt 100ms raw overhead 0 \n";

    const NOQUEUE_ROOT: &str = "qdisc noqueue 0: root refcnt 2 \n";

    const HTB_TREE: &str = "qdisc htb 1: root refcnt 2 r2q 10 default 0x10 \
direct_packets_stat 0 direct_qlen 1000\n\
qdisc fq_codel 10: parent 1:10 limit 10240p flows 1024 quantum 1514 target 5ms interval 100ms \
memory_limit 32Mb ecn drop_batch 64 \n\
qdisc fq 20: parent 1:20 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 \
quantum 3028b initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms \
timer_slack 10us horizon 10s horizon_drop \n";

    /// skyline_tc's clsact, listed before the root the way tc sorts it on
    /// some hosts.
    const CLSACT_AND_FQ: &str = "qdisc clsact ffff: parent ffff:fff1 \n\
qdisc fq 8001: root refcnt 2 limit 10000p flow_limit 100p buckets 1024 orphan_mask 1023 \
quantum 3028b initial_quantum 15140b low_rate_threshold 550Kbit refill_delay 40ms \
timer_slack 10us horizon 10s horizon_drop \n";

    const MQPRIO_ROOT: &str = "qdisc mqprio 8001: root tc 3 map 0 0 0 1 2 2 2 2 2 2 2 2 2 2 2 2\n\
             queues:(0:1) (2:2) (3:3)\n\
             mode:dcb\n\
             shaper:dcb\n";

    fn classify(output: &str) -> (String, Verdict) {
        let root = RootQdisc::parse(output).expect("a root qdisc");
        (root.summary(), root.verdict())
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn run(commands: &[&[&str]]) -> Plan {
        Plan::Run(commands.iter().map(|command| args(command)).collect())
    }

    #[test]
    fn fq_root_with_a_handle_is_left_as_is() {
        assert_eq!(classify(FQ_ROOT), ("fq".to_owned(), Verdict::Fq));
    }

    #[test]
    fn kernel_mq_with_fq_children_is_left_as_is() {
        assert_eq!(classify(MQ_DEFAULT_FQ), ("mq/fq".to_owned(), Verdict::Fq));
        let root = RootQdisc::parse(MQ_DEFAULT_FQ).expect("root");
        assert_eq!(root.major, 0);
        assert_eq!(root.children.len(), 4);
    }

    #[test]
    fn a_tc_made_mq_with_several_cake_children_gets_one_fresh_mq() {
        let root = RootQdisc::parse(MQ_CAKE).expect("root");
        assert_eq!(
            (root.summary(), root.verdict()),
            ("mq/cake".to_owned(), Verdict::Replace)
        );
        // mq 8002: made by tc, so `replace root mq` would be a no-op change;
        // a handle it does not have makes the kernel graft a new mq once.
        assert_eq!(
            root.replace_plan("eth0", 2, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "handle", "8000:", "mq"]])
        );
        // Child by child when a fresh mq would not come out fq, and as the
        // second round's fallback.
        let per_child = run(&[
            &["qdisc", "replace", "dev", "eth0", "parent", "8002:2", "fq"],
            &["qdisc", "replace", "dev", "eth0", "parent", "8002:1", "fq"],
        ]);
        assert_eq!(root.replace_plan("eth0", 2, false, Round::First), per_child);
        assert_eq!(
            root.replace_plan("eth0", 2, true, Round::Fallback),
            per_child
        );
    }

    #[test]
    fn tuned_fq_siblings_are_kept_by_fixing_the_others_child_by_child() {
        // An operator paced two queues with fq maxrate; something put cake on
        // the other two. A fresh mq would rebuild all four with default
        // parameters and lose the maxrate.
        let root = RootQdisc::parse(
            "qdisc mq 8001: root \n\
qdisc fq 8005: parent 8001:4 limit 10000p flow_limit 100p buckets 1024 maxrate 200Mbit \n\
qdisc cake 8004: parent 8001:3 bandwidth unlimited diffserv3 \n\
qdisc cake 8003: parent 8001:2 bandwidth unlimited diffserv3 \n\
qdisc fq 8002: parent 8001:1 limit 10000p flow_limit 100p buckets 1024 maxrate 200Mbit \n",
        )
        .expect("root");
        assert_eq!(root.verdict(), Verdict::Replace);
        assert_eq!(
            root.replace_plan("eth0", 4, true, Round::First),
            run(&[
                &["qdisc", "replace", "dev", "eth0", "parent", "8001:3", "fq"],
                &["qdisc", "replace", "dev", "eth0", "parent", "8001:2", "fq"],
            ])
        );
    }

    #[test]
    fn the_fresh_mq_handle_is_one_nothing_on_the_device_uses() {
        // 64 cake children under mq 1:, skyline_tc's clsact, and a stray
        // 8000: somewhere: 64 per-child grafts would reset the NIC 64 times.
        let mut output = String::from("qdisc clsact ffff: parent ffff:fff1 \nqdisc mq 1: root \n");
        for queue in (1..=64).rev() {
            output.push_str(&format!(
                "qdisc cake 0: parent 1:{queue:x} bandwidth unlimited diffserv3 \n"
            ));
        }
        output.push_str("qdisc pfifo 8000: parent 3:1 limit 1000p\n");
        let root = RootQdisc::parse(&output).expect("root");
        assert_eq!(root.children.len(), 64);
        assert!(root.majors.contains(&0xffff));
        assert_eq!(
            root.replace_plan("eth0", 64, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "handle", "8001:", "mq"]])
        );
    }

    #[test]
    fn kernel_mq_with_fq_codel_children_gets_a_fresh_mq_only_from_an_fq_default() {
        let root = RootQdisc::parse(MQ_DEFAULT_FQ_CODEL).expect("root");
        assert_eq!(
            (root.summary(), root.verdict()),
            ("mq/fq_codel".to_owned(), Verdict::Replace)
        );
        assert_eq!(
            root.replace_plan("eth0", 2, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "mq"]])
        );
        assert_eq!(
            root.replace_plan("eth0", 4, false, Round::First),
            Plan::FreshMqNeedsFqDefault
        );
    }

    #[test]
    fn mixed_mq_children_summarise_sorted_and_only_the_odd_ones_are_replaced() {
        let output = "qdisc mq 8001: root \n\
qdisc fq 0: parent 8001:3 limit 10000p \n\
qdisc cake 0: parent 8001:2 bandwidth unlimited \n\
qdisc fq 0: parent 8001:1 limit 10000p \n";
        let root = RootQdisc::parse(output).expect("root");
        assert_eq!(
            (root.summary(), root.verdict()),
            ("mq/cake,fq".to_owned(), Verdict::Replace)
        );
        // Exactly one odd child: one per-child replace, not a fresh mq.
        assert_eq!(
            root.replace_plan("eth0", 3, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "parent", "8001:2", "fq"]])
        );
    }

    #[test]
    fn fq_codel_root_is_replaced_with_fq_or_mq_by_queue_count() {
        let root = RootQdisc::parse(FQ_CODEL_ROOT).expect("root");
        assert_eq!(
            (root.summary(), root.verdict()),
            ("fq_codel".to_owned(), Verdict::Replace)
        );
        assert_eq!(
            root.replace_plan("eth0", 1, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "fq"]])
        );
        // One atomic replace names fq itself: the default does not matter.
        assert_eq!(
            root.replace_plan("eth0", 1, false, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "fq"]])
        );
        assert_eq!(
            root.replace_plan("eth0", 4, true, Round::First),
            run(&[&["qdisc", "replace", "dev", "eth0", "root", "mq"]])
        );
    }

    #[test]
    fn unshaped_cake_root_is_replaced() {
        assert_eq!(classify(CAKE_ROOT), ("cake".to_owned(), Verdict::Replace));
        assert_eq!(
            RootQdisc::parse(CAKE_ROOT).expect("root").shaping_detail(),
            ""
        );
    }

    #[test]
    fn a_shaping_cake_is_left_alone_at_the_root_and_under_mq() {
        assert_eq!(
            classify(CAKE_SHAPED),
            ("cake".to_owned(), Verdict::Deliberate)
        );
        assert_eq!(
            RootQdisc::parse(CAKE_SHAPED)
                .expect("root")
                .shaping_detail(),
            " (bandwidth 90Mbit)"
        );
        assert_eq!(
            classify(MQ_SHAPED_CAKE),
            ("mq/cake".to_owned(), Verdict::Deliberate)
        );
        assert_eq!(
            RootQdisc::parse(MQ_SHAPED_CAKE)
                .expect("root")
                .shaping_detail(),
            " (bandwidth 45Mbit)"
        );
        // autorate-ingress adjusts a rate by itself; also somebody's shaper.
        let autorate = "qdisc cake 8006: root refcnt 2 bandwidth unlimited autorate-ingress \
diffserv3 triple-isolate \n";
        assert_eq!(classify(autorate), ("cake".to_owned(), Verdict::Deliberate));
        assert_eq!(
            RootQdisc::parse(autorate).expect("root").shaping_detail(),
            " (autorate-ingress)"
        );
        // One shaped child is enough to leave the whole mq alone.
        let mixed = "qdisc mq 8001: root \n\
qdisc cake 0: parent 8001:2 bandwidth 45Mbit \n\
qdisc cake 0: parent 8001:1 bandwidth unlimited \n";
        assert_eq!(classify(mixed), ("mq/cake".to_owned(), Verdict::Deliberate));
    }

    #[test]
    fn noqueue_on_its_own_reads_as_deliberate() {
        // What the verdict says about a root that is noqueue. Whether the
        // pass ever asks is `managed_devices`' call: only for a NIC.
        assert_eq!(
            classify(NOQUEUE_ROOT),
            ("noqueue".to_owned(), Verdict::Deliberate)
        );
    }

    #[test]
    fn htb_tree_is_left_alone_even_with_fq_leaves() {
        assert_eq!(classify(HTB_TREE), ("htb".to_owned(), Verdict::Deliberate));
    }

    #[test]
    fn clsact_next_to_fq_is_ignored() {
        assert_eq!(classify(CLSACT_AND_FQ), ("fq".to_owned(), Verdict::Fq));
        let with_mq = format!("{MQ_DEFAULT_FQ}qdisc clsact ffff: parent ffff:fff1 \n");
        assert_eq!(classify(&with_mq), ("mq/fq".to_owned(), Verdict::Fq));
        let with_ingress =
            format!("qdisc ingress ffff: parent ffff:fff1 ----------------\n{CAKE_ROOT}");
        assert_eq!(
            classify(&with_ingress),
            ("cake".to_owned(), Verdict::Replace)
        );
    }

    #[test]
    fn mq_with_a_deliberate_child_is_left_alone() {
        let output = "qdisc mq 8001: root \n\
qdisc tbf 10: parent 8001:2 rate 100Mbit burst 32Kb lat 50ms \n\
qdisc cake 0: parent 8001:1 bandwidth unlimited \n";
        assert_eq!(
            classify(output),
            ("mq/cake,tbf".to_owned(), Verdict::Deliberate)
        );
    }

    #[test]
    fn multi_line_and_unknown_kinds_are_left_alone() {
        assert_eq!(
            classify(MQPRIO_ROOT),
            ("mqprio".to_owned(), Verdict::Deliberate)
        );
        assert_eq!(
            classify("qdisc dualpi2 8001: root refcnt 2 limit 10000p \n"),
            ("dualpi2".to_owned(), Verdict::Unknown)
        );
        // An mq that shows no children says nothing about what is under it.
        assert_eq!(
            classify("qdisc mq 0: root \n"),
            ("mq".to_owned(), Verdict::Unknown)
        );
    }

    #[test]
    fn output_without_a_device_filter_still_parses() {
        assert_eq!(
            classify("qdisc fq_pie 8001: dev eth0 root refcnt 2 limit 10240p flows 1024 \n"),
            ("fq_pie".to_owned(), Verdict::Replace)
        );
        assert_eq!(
            classify("qdisc cake 8001: dev eth0 root refcnt 2 bandwidth 1Gbit \n"),
            ("cake".to_owned(), Verdict::Deliberate)
        );
    }

    #[test]
    fn output_without_a_root_is_not_guessed_at() {
        assert!(RootQdisc::parse("").is_none());
        assert!(RootQdisc::parse("qdisc clsact ffff: parent ffff:fff1 \n").is_none());
        assert!(RootQdisc::parse("Cannot find device \"eth9\"\n").is_none());
    }

    #[test]
    fn retry_backoff_doubles_from_thirty_seconds_up_to_five_minutes() {
        let steps: Vec<u64> = (1..=7)
            .map(|failures| retry_backoff(Duration::from_secs(5), failures).as_secs())
            .collect();
        assert_eq!(steps, vec![30, 60, 120, 240, 300, 300, 300]);
        // Never sooner than one interval, never later than the cap.
        assert_eq!(retry_backoff(Duration::from_secs(60), 1).as_secs(), 60);
        // A retry only happens on a pass: never sooner than the next one, and
        // rounded up to whole intervals so the note tells the truth.
        assert_eq!(retry_backoff(Duration::from_secs(600), 1).as_secs(), 600);
        assert_eq!(retry_backoff(Duration::from_secs(200), 2).as_secs(), 400);
        assert_eq!(retry_backoff(Duration::from_secs(7), 1).as_secs(), 35);
        assert_eq!(
            retry_backoff(Duration::from_secs(5), u32::MAX).as_secs(),
            300
        );
    }

    /// One network device in the fake.
    struct FakeDevice {
        show: String,
        tx_queues: usize,
        lowers: Vec<String>,
        device_link: bool,
    }

    /// A tiny model of the kernel's side: sysctls, a few devices, and the
    /// `tc` behaviour the guard relies on.
    struct FakeHost {
        inner: Mutex<Fake>,
    }

    struct Fake {
        cc: String,
        default_qdisc: String,
        refuse_default_qdisc: bool,
        tc_installed: bool,
        /// Model a replace the kernel accepts but does not act on.
        ignore_replace: bool,
        /// Fail this many replaces the way a transient netlink error does.
        fail_replaces: usize,
        /// What a fresh mq's children become instead of default_qdisc:
        /// something rewrote the default between the pass's read and the
        /// replace.
        fresh_mq_children: Option<String>,
        devices: BTreeMap<String, FakeDevice>,
        next_handle: u32,
        tc_calls: Vec<String>,
    }

    impl FakeHost {
        /// One NIC, eth0.
        fn new(cc: &str, default_qdisc: &str, show: &str, tx_queues: usize) -> Arc<Self> {
            let host = Arc::new(Self {
                inner: Mutex::new(Fake {
                    cc: cc.to_owned(),
                    default_qdisc: default_qdisc.to_owned(),
                    refuse_default_qdisc: false,
                    tc_installed: true,
                    ignore_replace: false,
                    fail_replaces: 0,
                    fresh_mq_children: None,
                    devices: BTreeMap::new(),
                    next_handle: 0x8010,
                    tc_calls: Vec::new(),
                }),
            });
            host.add_device("eth0", show, tx_queues, &[], true);
            host
        }

        fn add_device(
            &self,
            name: &str,
            show: &str,
            tx_queues: usize,
            lowers: &[&str],
            device_link: bool,
        ) {
            self.fake().devices.insert(
                name.to_owned(),
                FakeDevice {
                    show: show.to_owned(),
                    tx_queues,
                    lowers: lowers.iter().map(|lower| (*lower).to_owned()).collect(),
                    device_link,
                },
            );
        }

        fn fake(&self) -> MutexGuard<'_, Fake> {
            self.inner.lock().unwrap()
        }

        fn show(&self, name: &str) -> String {
            self.fake().devices[name].show.clone()
        }

        fn set_show(&self, name: &str, show: &str) {
            self.fake()
                .devices
                .get_mut(name)
                .expect("known device")
                .show = show.to_owned();
        }

        fn replaces(&self) -> usize {
            self.fake()
                .tc_calls
                .iter()
                .filter(|call| call.starts_with("qdisc replace"))
                .count()
        }

        fn calls_naming(&self, device: &str) -> usize {
            let needle = format!(" dev {device}");
            self.fake()
                .tc_calls
                .iter()
                .filter(|call| call.contains(&needle) && call.split(' ').any(|word| word == device))
                .count()
        }
    }

    impl Fake {
        fn fresh_mq(&self, handle: u32, tx_queues: usize) -> String {
            let children = self
                .fresh_mq_children
                .clone()
                .unwrap_or_else(|| self.default_qdisc.clone());
            let mut show = format!("qdisc mq {handle:x}: root \n");
            for queue in (1..=tx_queues).rev() {
                show.push_str(&format!(
                    "qdisc {children} 0: parent {handle:x}:{queue:x} \n"
                ));
            }
            show
        }
    }

    impl Host for FakeHost {
        fn congestion_control(&self) -> Result<String> {
            Ok(self.fake().cc.clone())
        }

        fn set_congestion_control(&self, name: &str) -> Result<()> {
            self.fake().cc = name.to_owned();
            Ok(())
        }

        fn default_qdisc(&self) -> Result<String> {
            Ok(self.fake().default_qdisc.clone())
        }

        fn set_default_qdisc(&self, kind: &str) -> Result<()> {
            let mut fake = self.fake();
            if fake.refuse_default_qdisc {
                bail!("set default_qdisc to {kind}: No such file or directory");
            }
            fake.default_qdisc = kind.to_owned();
            Ok(())
        }

        fn interface_exists(&self, interface: &str) -> bool {
            self.fake().devices.contains_key(interface)
        }

        fn tx_queues(&self, interface: &str) -> usize {
            self.fake()
                .devices
                .get(interface)
                .map_or(1, |device| device.tx_queues)
        }

        fn lower_devices(&self, interface: &str) -> Vec<String> {
            let mut lowers = self
                .fake()
                .devices
                .get(interface)
                .map(|device| device.lowers.clone())
                .unwrap_or_default();
            lowers.sort();
            lowers
        }

        fn has_device_link(&self, interface: &str) -> bool {
            self.fake()
                .devices
                .get(interface)
                .is_some_and(|device| device.device_link)
        }

        fn tc(&self, args: &[&str]) -> Result<String> {
            let mut fake = self.fake();
            if !fake.tc_installed {
                bail!(
                    "tc is not installed (iproute2): run tc {}: No such file or directory \
                     (os error 2)",
                    args.join(" ")
                );
            }
            fake.tc_calls.push(args.join(" "));
            let (name, tail) = match args {
                ["qdisc", verb, "dev", name, tail @ ..]
                    if *verb == "show" || *verb == "replace" =>
                {
                    ((*name).to_owned(), tail)
                }
                _ => bail!("unexpected tc {}", args.join(" ")),
            };
            let Some((current_show, tx_queues)) = fake
                .devices
                .get(&name)
                .map(|device| (device.show.clone(), device.tx_queues))
            else {
                bail!("Cannot find device \"{name}\"");
            };
            if args[1] == "show" {
                return Ok(current_show);
            }
            if fake.ignore_replace {
                return Ok(String::new());
            }
            if fake.fail_replaces > 0 {
                fake.fail_replaces -= 1;
                bail!("RTNETLINK answers: Cannot allocate memory");
            }
            let current = RootQdisc::parse(&current_show);
            let show = match tail {
                ["root", "mq"] => {
                    // Fact from a 6.12 host: over an mq tc created this is a
                    // same-kind change that returns 0 and changes nothing.
                    if current.is_some_and(|root| root.kind == "mq" && root.major != 0) {
                        return Ok(String::new());
                    }
                    let handle = fake.next_handle;
                    fake.next_handle += 1;
                    fake.fresh_mq(handle, tx_queues)
                }
                ["root", "handle", handle, "mq"] => {
                    let major = major_of(handle).expect("a handle");
                    fake.fresh_mq(major, tx_queues)
                }
                ["root", kind] => {
                    let handle = fake.next_handle;
                    fake.next_handle += 1;
                    format!("qdisc {kind} {handle:x}: root refcnt 2 \n")
                }
                ["parent", parent, kind] => {
                    let marker = format!(" parent {parent} ");
                    current_show
                        .lines()
                        .map(|line| {
                            if line.contains(&marker) {
                                format!("qdisc {kind} 0: parent {parent} \n")
                            } else {
                                format!("{line}\n")
                            }
                        })
                        .collect()
                }
                _ => bail!("unexpected tc {}", args.join(" ")),
            };
            fake.devices.get_mut(&name).expect("known device").show = show;
            Ok(String::new())
        }
    }

    fn settings_for(interface: &str, qdisc: bool) -> Settings {
        Settings {
            interval: Duration::from_secs(3600),
            interval_s: 3600,
            qdisc,
            interface: Some(interface.to_owned()),
        }
    }

    fn settings(qdisc: bool) -> Settings {
        settings_for("eth0", qdisc)
    }

    fn guard(host: &Arc<FakeHost>, qdisc: bool) -> Guard {
        Guard::with_host(settings(qdisc), host.clone())
    }

    /// The shipped interval: backoff arithmetic in the tests below assumes it.
    fn guard_every_5s(host: &Arc<FakeHost>, interface: &str) -> Guard {
        Guard::with_host(
            Settings {
                interval: Duration::from_secs(5),
                interval_s: 5,
                ..settings_for(interface, true)
            },
            host.clone(),
        )
    }

    fn tick(guard: &Guard, now: Instant) -> PassReport {
        periodic_pass(&guard.settings, &*guard.host, &guard.state, now).expect("armed")
    }

    #[test]
    fn enable_puts_back_the_congestion_control_and_both_qdiscs() {
        let host = FakeHost::new("bbr", "cake", CAKE_ROOT, 1);
        let guard = guard(&host, true);
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec![
                "tcp_congestion_control bbr -> skyline_cc",
                "default_qdisc cake -> fq",
                "eth0 root qdisc cake -> fq",
            ]
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.logged.len(), 3);
        assert!(report.logged.iter().all(|line| line.starts_with("guard: ")));
        assert_eq!(host.fake().cc, "skyline_cc");
        assert_eq!(host.fake().default_qdisc, "fq");

        let status = guard.status();
        assert!(status.armed);
        assert_eq!(
            (
                status.cc_restored,
                status.default_qdisc_restored,
                status.interface_qdisc_replaced
            ),
            (1, 1, 1)
        );
        assert_eq!(
            status.last_correction.as_deref(),
            Some("eth0 root qdisc cake -> fq")
        );
        assert!(status.last_correction_unix_s.is_some());
        assert_eq!(status.checks, 0, "the enable pass is not a periodic check");
        assert_eq!(status.live.tcp_congestion_control, "skyline_cc");
        assert_eq!(status.live.default_qdisc, "fq");
        assert_eq!(status.live.interface.as_deref(), Some("eth0"));
        assert_eq!(status.live.interface_qdisc.as_deref(), Some("fq"));
        assert_eq!(
            status.live.devices,
            vec![GuardDevice {
                name: "eth0".to_owned(),
                qdisc: Some("fq".to_owned()),
            }]
        );
    }

    #[test]
    fn a_multi_queue_nic_gets_mq_with_fq_children() {
        let host = FakeHost::new("skyline_cc", "fq_codel", MQ_DEFAULT_FQ_CODEL, 2);
        let report = guard(&host, true).arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec![
                "default_qdisc fq_codel -> fq",
                "eth0 root qdisc mq/fq_codel -> mq/fq",
            ]
        );
        assert_eq!(host.replaces(), 1);
    }

    #[test]
    fn a_tc_made_mq_of_cakes_is_regrafted_once() {
        let host = FakeHost::new("skyline_cc", "fq", MQ_CAKE, 2);
        let report = guard(&host, true).arm_and_enforce();
        assert_eq!(report.corrections, vec!["eth0 root qdisc mq/cake -> mq/fq"]);
        assert_eq!(
            host.fake().tc_calls[1],
            "qdisc replace dev eth0 root handle 8000: mq"
        );
        assert_eq!(host.replaces(), 1);
    }

    #[test]
    fn a_fresh_mq_that_is_still_not_fq_is_fixed_child_by_child() {
        let host = FakeHost::new("skyline_cc", "fq", MQ_CAKE, 2);
        // Something rewrote default_qdisc between the pass's read and the
        // replace: the fresh mq came out cake again.
        host.fake().fresh_mq_children = Some("cake".to_owned());
        let report = guard(&host, true).arm_and_enforce();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.corrections, vec!["eth0 root qdisc mq/cake -> mq/fq"]);
        assert_eq!(host.replaces(), 3, "one fresh mq, then two children");
        assert!(host
            .fake()
            .tc_calls
            .contains(&"qdisc replace dev eth0 parent 8000:2 fq".to_owned()));
    }

    #[test]
    fn a_fresh_mq_is_not_built_from_a_default_that_is_not_fq() {
        for show in [MQ_DEFAULT_FQ_CODEL, FQ_CODEL_ROOT] {
            let host = FakeHost::new("skyline_cc", "cake", show, 4);
            host.fake().refuse_default_qdisc = true;
            let guard = guard(&host, true);
            let report = guard.arm_and_enforce();
            // `root mq` now would give every queue a cake: nothing is run.
            assert_eq!(host.replaces(), 0, "{show}");
            assert!(report.corrections.is_empty());
            assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
            assert!(report.errors[0].starts_with("default_qdisc is cake"));
            let summary = RootQdisc::parse(show).expect("root").summary();
            assert_eq!(
                report.errors[1],
                format!(
                    "eth0 root qdisc {summary} left alone: default_qdisc is cake, a fresh mq \
                     would get cake children"
                )
            );
            assert_eq!(guard.status().last_error, Some(report.errors[1].clone()));
            assert_eq!(host.show("eth0"), show);
        }
    }

    #[test]
    fn a_tc_made_mq_is_fixed_child_by_child_when_default_qdisc_cannot_be_written() {
        let host = FakeHost::new("skyline_cc", "cake", MQ_CAKE, 2);
        host.fake().refuse_default_qdisc = true;
        let report = guard(&host, true).arm_and_enforce();
        // Each per-child replace names fq itself, whatever the default says.
        assert_eq!(report.corrections, vec!["eth0 root qdisc mq/cake -> mq/fq"]);
        assert_eq!(host.replaces(), 2);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].starts_with("default_qdisc is cake"));
    }

    #[test]
    fn a_single_queue_nic_is_fixed_even_when_default_qdisc_cannot_be_written() {
        let host = FakeHost::new("skyline_cc", "cake", FQ_CODEL_ROOT, 1);
        host.fake().refuse_default_qdisc = true;
        let report = guard(&host, true).arm_and_enforce();
        assert_eq!(report.corrections, vec!["eth0 root qdisc fq_codel -> fq"]);
    }

    #[test]
    fn deliberate_root_is_left_alone_and_logged_once() {
        let host = FakeHost::new("skyline_cc", "fq", HTB_TREE, 1);
        let guard = guard(&host, true);
        let report = guard.arm_and_enforce();
        assert!(report.corrections.is_empty());
        assert_eq!(
            report.notes,
            vec!["eth0 root qdisc htb looks deliberate; left alone"]
        );
        assert_eq!(report.logged.len(), 1);
        assert_eq!(host.replaces(), 0);

        // The next periodic passes find the same thing and stay quiet.
        for _ in 0..2 {
            let again = tick(&guard, Instant::now());
            assert_eq!(again.notes, report.notes);
            assert!(again.logged.is_empty(), "{:?}", again.logged);
        }
        let status = guard.status();
        assert_eq!(status.checks, 2);
        assert_eq!(status.notes, report.notes);
        assert_eq!(host.replaces(), 0);
        let state = lock(&guard.state);
        assert_eq!(state.reported, report.notes);
    }

    #[test]
    fn a_cake_shaper_is_left_alone_and_the_note_says_why() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_SHAPED, 1);
        let report = guard(&host, true).arm_and_enforce();
        assert!(report.corrections.is_empty());
        assert_eq!(
            report.notes,
            vec!["eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone"]
        );
        assert_eq!(host.replaces(), 0);

        let host = FakeHost::new("skyline_cc", "fq", MQ_SHAPED_CAKE, 2);
        let report = guard(&host, true).arm_and_enforce();
        assert_eq!(
            report.notes,
            vec!["eth0 root qdisc mq/cake (bandwidth 45Mbit) looks deliberate; left alone"]
        );
        assert_eq!(host.replaces(), 0);
    }

    #[test]
    fn a_down_interface_is_noted_not_failed() {
        let host = FakeHost::new("skyline_cc", "fq", "", 1);
        let guard = guard(&host, true);
        let report = guard.arm_and_enforce();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.notes,
            vec!["eth0 shows no root qdisc (is it down?); not checked"]
        );
        assert_eq!(host.replaces(), 0);
        assert_eq!(guard.status().last_error, None);
    }

    #[test]
    fn missing_tc_is_reported_and_never_fatal() {
        let host = FakeHost::new("bbr", "fq", CAKE_ROOT, 1);
        host.fake().tc_installed = false;
        let guard = guard(&host, true);
        let report = guard.arm_and_enforce();
        // The congestion control is still enforced.
        assert_eq!(
            report.corrections,
            vec!["tcp_congestion_control bbr -> skyline_cc"]
        );
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].starts_with("tc is not installed"));
        let status = guard.status();
        assert!(status.armed);
        assert_eq!(status.last_error, Some(report.errors[0].clone()));
        assert_eq!(status.notes, report.errors);
        assert_eq!(status.live.interface_qdisc, None);
        assert_eq!(
            status.live.devices,
            vec![GuardDevice {
                name: "eth0".to_owned(),
                qdisc: None,
            }]
        );
    }

    #[test]
    fn a_transient_replace_error_is_retried_after_a_backoff() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        let guard = guard_every_5s(&host, "eth0");
        guard.arm_and_enforce();
        assert_eq!(host.show("eth0").split(' ').nth(1), Some("fq"));

        // Something puts cake back, and the next replace hits a one-off
        // netlink error.
        host.set_show("eth0", CAKE_ROOT);
        host.fake().fail_replaces = 1;
        let failed_at = Instant::now();
        let failed = tick(&guard, failed_at);
        assert_eq!(failed.errors.len(), 1);
        assert!(
            failed.errors[0].contains("Cannot allocate memory"),
            "{:?}",
            failed.errors
        );
        let note = "eth0 root qdisc cake: the last replace failed; retrying in 30 s";
        assert_eq!(failed.notes, vec![note]);
        let replaces = host.replaces();

        // Within the backoff: not retried, and nothing new in the journal.
        let quiet = tick(&guard, failed_at + Duration::from_secs(25));
        assert_eq!(host.replaces(), replaces);
        assert_eq!(quiet.notes, vec![note]);
        assert!(quiet.logged.is_empty(), "{:?}", quiet.logged);

        // After it: retried, and this time it works.
        let healed = tick(&guard, failed_at + Duration::from_secs(30));
        assert_eq!(healed.corrections, vec!["eth0 root qdisc cake -> fq"]);
        assert!(healed.notes.is_empty(), "{:?}", healed.notes);
        assert!(lock(&guard.state).stuck.is_empty());
        assert_eq!(guard.status().interface_qdisc_replaced, 2);
    }

    #[test]
    fn a_replace_that_keeps_failing_backs_off_to_five_minutes() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        host.fake().ignore_replace = true;
        let guard = guard_every_5s(&host, "eth0");
        let report = guard.arm_and_enforce();
        let start = Instant::now();
        assert!(report.corrections.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("still cake"));
        assert_eq!(
            report.notes,
            vec!["eth0 root qdisc cake: the last replace failed; retrying in 30 s"]
        );
        let mut replaces = host.replaces();
        assert!(replaces > 0);

        // Retry points and the step each failed retry then waits.
        let mut at = Duration::ZERO;
        for (wait, next) in [(30, 60), (60, 120), (120, 240), (240, 300), (300, 300)] {
            let before = tick(&guard, start + at + Duration::from_secs(wait - 1));
            assert_eq!(host.replaces(), replaces, "not retried before {wait} s");
            assert!(before.errors.is_empty());
            at += Duration::from_secs(wait);
            let retried = tick(&guard, start + at);
            assert!(host.replaces() > replaces, "retried after {wait} s");
            replaces = host.replaces();
            assert_eq!(retried.errors.len(), 1);
            assert_eq!(
                retried.notes,
                vec![format!(
                    "eth0 root qdisc cake: the last replace failed; retrying in {next} s"
                )]
            );
        }

        // `ssctl enable` retries at once, whatever the backoff.
        guard.arm_and_enforce();
        assert!(host.replaces() > replaces, "enable retries");

        // A different root is not what failed: tried at the next pass.
        host.fake().ignore_replace = false;
        host.set_show("eth0", FQ_CODEL_ROOT);
        tick(&guard, Instant::now());
        assert_eq!(
            guard.status().last_correction.as_deref(),
            Some("eth0 root qdisc fq_codel -> fq")
        );
    }

    #[test]
    fn with_no_periodic_pass_the_note_points_at_enable() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        host.fake().ignore_replace = true;
        let guard = Guard::with_host(
            Settings {
                interval: Duration::ZERO,
                interval_s: 0,
                ..settings(true)
            },
            host.clone(),
        );
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.notes,
            vec![
                "eth0 root qdisc cake: the last replace failed; retried on the next ssctl \
                 enable ([guard] interval_s = 0)"
            ]
        );
    }

    #[test]
    fn a_vlan_is_followed_to_its_nic() {
        let host = FakeHost::new("bbr", "cake", CAKE_ROOT, 1);
        host.add_device("eth0.77", NOQUEUE_ROOT, 1, &["eth0"], false);
        let guard = guard_every_5s(&host, "eth0.77");
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec![
                "tcp_congestion_control bbr -> skyline_cc",
                "default_qdisc cake -> fq",
                "eth0 (under eth0.77) root qdisc cake -> fq",
            ]
        );
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert!(!host
            .fake()
            .tc_calls
            .iter()
            .any(|call| call.starts_with("qdisc replace dev eth0.77")));
        let live = guard.status().live;
        assert_eq!(live.interface.as_deref(), Some("eth0.77"));
        assert_eq!(live.interface_qdisc.as_deref(), Some("noqueue"));
        assert_eq!(
            live.devices,
            vec![GuardDevice {
                name: "eth0".to_owned(),
                qdisc: Some("fq".to_owned()),
            }]
        );
    }

    #[test]
    fn a_bond_is_followed_to_every_slave() {
        let host = FakeHost::new("skyline_cc", "fq", MQ_CAKE, 2);
        host.add_device("eth1", FQ_CODEL_ROOT, 1, &[], true);
        host.add_device("bond0", NOQUEUE_ROOT, 1, &["eth1", "eth0"], false);
        let guard = guard_every_5s(&host, "bond0");
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec![
                "eth0 (under bond0) root qdisc mq/cake -> mq/fq",
                "eth1 (under bond0) root qdisc fq_codel -> fq",
            ]
        );
        let names: Vec<String> = guard
            .status()
            .live
            .devices
            .into_iter()
            .map(|device| format!("{}={}", device.name, device.qdisc.unwrap_or_default()))
            .collect();
        assert_eq!(names, vec!["eth0=mq/fq", "eth1=fq"]);
        assert_eq!(guard.status().interface_qdisc_replaced, 2);
    }

    #[test]
    fn a_bridge_manages_its_physical_port_and_never_touches_a_vm_tap() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        host.add_device("eno1", CAKE_ROOT, 1, &[], true);
        host.add_device("tap100i0", FQ_CODEL_ROOT, 1, &[], false);
        host.add_device("vmbr0", NOQUEUE_ROOT, 1, &["tap100i0", "eno1"], false);
        let guard = guard_every_5s(&host, "vmbr0");
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec!["eno1 (under vmbr0) root qdisc cake -> fq"]
        );
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        // The tap is not even read.
        assert_eq!(host.calls_naming("tap100i0"), 0);
        assert_eq!(host.show("tap100i0"), FQ_CODEL_ROOT);
        // eth0 exists on this fake host but is not under vmbr0.
        assert_eq!(host.show("eth0"), CAKE_ROOT);
        let devices = guard.status().live.devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "eno1");
    }

    #[test]
    fn stacked_devices_are_followed_down_and_named_by_their_path() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        host.add_device("bond0", NOQUEUE_ROOT, 1, &["eth0"], false);
        // bond0 appears twice (directly and under the VLAN): visited once.
        host.add_device("bond0.100", NOQUEUE_ROOT, 1, &["bond0"], false);
        host.add_device("vmbr0", NOQUEUE_ROOT, 1, &["bond0", "bond0.100"], false);
        let report = guard_every_5s(&host, "vmbr0").arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec!["eth0 (under bond0 under vmbr0) root qdisc cake -> fq"]
        );
        assert_eq!(host.replaces(), 1);
    }

    #[test]
    fn a_tunnel_with_nothing_under_it_is_noted_as_virtual_not_deliberate() {
        let host = FakeHost::new("skyline_cc", "fq", CAKE_ROOT, 1);
        host.add_device("wg0", NOQUEUE_ROOT, 1, &[], false);
        let guard = guard_every_5s(&host, "wg0");
        let report = guard.arm_and_enforce();
        assert!(report.corrections.is_empty());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            report.notes,
            vec![
                "wg0 is a virtual device (noqueue is its kernel default) and no NIC under it \
                 is visible to the guard; no qdisc checked"
            ]
        );
        assert_eq!(host.replaces(), 0);
        let live = guard.status().live;
        assert_eq!(live.interface_qdisc.as_deref(), Some("noqueue"));
        assert!(live.devices.is_empty());

        // A bridge of VM taps only says the same.
        host.add_device("tap100i0", FQ_CODEL_ROOT, 1, &[], false);
        host.add_device("vmbr1", NOQUEUE_ROOT, 1, &["tap100i0"], false);
        let report = guard_every_5s(&host, "vmbr1").arm_and_enforce();
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].starts_with("vmbr1 is a virtual device"));
        assert_eq!(host.replaces(), 0);
    }

    #[test]
    fn noqueue_set_on_a_nic_is_reported_as_deliberate() {
        // The kernel never gives a NIC noqueue by itself; somebody did.
        let host = FakeHost::new("skyline_cc", "fq", NOQUEUE_ROOT, 4);
        let guard = guard(&host, true);
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.notes,
            vec!["eth0 root qdisc noqueue looks deliberate; left alone"]
        );
        assert_eq!(host.replaces(), 0);
        assert_eq!(guard.status().live.devices.len(), 1);
    }

    #[test]
    fn qdisc_false_owns_only_the_congestion_control() {
        let host = FakeHost::new("bbr", "cake", CAKE_ROOT, 1);
        let guard = guard(&host, false);
        let report = guard.arm_and_enforce();
        assert_eq!(
            report.corrections,
            vec!["tcp_congestion_control bbr -> skyline_cc"]
        );
        assert_eq!(host.fake().default_qdisc, "cake");
        assert_eq!(host.replaces(), 0);
        // Nothing is managed, but the interface's own root is still shown.
        let live = guard.status().live;
        assert!(live.devices.is_empty());
        assert_eq!(live.interface_qdisc.as_deref(), Some("cake"));
    }

    #[test]
    fn a_disarmed_guard_leaves_the_host_alone() {
        let host = FakeHost::new("bbr", "cake", CAKE_ROOT, 1);
        let guard = guard(&host, true);
        assert!(
            periodic_pass(&guard.settings, &*guard.host, &guard.state, Instant::now()).is_none()
        );
        assert_eq!(host.fake().cc, "bbr");
        assert!(host.fake().tc_calls.is_empty());
        assert_eq!(guard.status().checks, 0);
        assert!(!guard.status().armed);
    }

    #[test]
    fn drain_writes_the_fallback_under_the_lock_after_disarming() {
        let host = FakeHost::new("skyline_cc", "fq", FQ_ROOT, 1);
        let guard = guard(&host, true);
        guard.arm_and_enforce();
        let wrote = guard.disarm_and(|| {
            // Held: no periodic pass can start until the fallback is written.
            assert!(guard.state.try_lock().is_err());
            host.set_congestion_control("cubic")
        });
        wrote.expect("fallback write");
        assert!(!guard.status().armed);
        // A tick after the drain does not put skyline_cc back.
        periodic_pass(&guard.settings, &*guard.host, &guard.state, Instant::now());
        assert_eq!(host.fake().cc, "cubic");
    }

    #[test]
    fn the_thread_restores_while_armed_and_stops_promptly() {
        let host = FakeHost::new("skyline_cc", "fq", FQ_ROOT, 1);
        let mut guard = Guard::with_host(
            Settings {
                interval: Duration::from_millis(10),
                ..settings(true)
            },
            host.clone(),
        );
        guard.start().expect("start");
        guard.arm_and_enforce();
        host.fake().cc = "bbr".to_owned();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.fake().cc != "skyline_cc" {
            assert!(Instant::now() < deadline, "the periodic pass never ran");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(guard.status().checks > 0);
        assert!(guard.status().cc_restored >= 1);

        guard.disarm_and(|| host.set_congestion_control("cubic").expect("write"));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(host.fake().cc, "cubic");
        guard.stop();
        assert!(guard.worker.is_none());
    }

    #[test]
    fn stopping_does_not_wait_out_the_interval() {
        let host = FakeHost::new("skyline_cc", "fq", FQ_ROOT, 1);
        let mut guard = guard(&host, true);
        guard.start().expect("start");
        assert!(guard.worker.is_some());
        let started = Instant::now();
        guard.stop();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn interval_zero_starts_no_thread_but_enable_still_enforces() {
        let host = FakeHost::new("bbr", "fq", FQ_ROOT, 1);
        let mut guard = Guard::with_host(
            Settings {
                interval: Duration::ZERO,
                interval_s: 0,
                ..settings(true)
            },
            host.clone(),
        );
        guard.start().expect("start");
        assert!(guard.worker.is_none());
        guard.arm_and_enforce();
        assert_eq!(host.fake().cc, "skyline_cc");
        assert_eq!(guard.status().interval_s, 0);
    }
}
