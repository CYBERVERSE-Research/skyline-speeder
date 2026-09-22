// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
mod guard;

use anyhow::{anyhow, bail, Context, Result};
use bytemuck::{bytes_of, try_from_bytes, try_pod_read_unaligned};
use clap::Parser;
use guard::Guard;
use libbpf_rs::btf::{types::Struct, Btf, BtfKind, BtfType, TypeId};
use libbpf_rs::{
    Link, MapCore, MapFlags, Object, ObjectBuilder, RingBufferBuilder, TcHook, TcHookBuilder,
    TC_EGRESS,
};
use skyline_common::{
    CapabilityReport, FeatureMask, Module, ModuleTuningConfig, RackRtoConfig, RackRtoStats,
    RackRtoStatus, RackTuningConfig, RackTuningStatus, Request, Response, RetransmitDscpConfig,
    RetransmitDscpStats, RetransmitDscpStatus, RuntimeStatus, SkylineConfig, SkylineEvent,
    SkylineMetrics, TcStats,
};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(version, about = "Skyline Speeder eBPF control daemon")]
struct Arguments {
    #[arg(long, default_value = "config/speeder.toml")]
    config: PathBuf,
    #[arg(long)]
    validate_only: bool,
    /// Load every BPF object through the kernel verifier, then detach it.
    #[arg(long, requires = "validate_only")]
    verify_bpf: bool,
}

struct BpfRuntime {
    objects: Vec<Object>,
    links: Vec<Link>,
    active_slot: u32,
    event_stop: Arc<AtomicBool>,
    event_threads: Vec<JoinHandle<()>>,
}

impl BpfRuntime {
    /// `event_log` is a shared handle opened once by `Daemon::new()` and
    /// passed to every runtime that might write events. Two separate
    /// `File`/`Mutex` instances opened on the same path do NOT serialize
    /// each other's writes, so any writer that appended through its own
    /// handle instead of this shared one could interleave mid-line and
    /// corrupt events.jsonl -- and would also keep its own idea of the file
    /// size, defeating `EventLog`'s cap. Kept as a shared
    /// `Arc<Mutex<EventLog>>` so a future second writer stays safe by
    /// construction -- see start_event_reader()'s single-buffer write for
    /// how the current sole writer uses it. `None` means the event log is
    /// off (`events_max_mib = 0`).
    fn load(config: &SkylineConfig, event_log: Option<Arc<Mutex<EventLog>>>) -> Result<Self> {
        let cc_path = config.runtime.bpf_dir.join("skyline_cc.bpf.o");

        // Baseline before anything is attached: if the attach below fails, the
        // machine is left on a known-good algorithm rather than on whatever it
        // happened to be. `Request::Enable` raises it to skyline_cc only after
        // the attach succeeds.
        set_default_congestion_control(&config.fallback_cc)?;
        let mut cc_object = load_object(&cc_path)?;
        let mut runtime = Self {
            objects: Vec::new(),
            links: Vec::new(),
            active_slot: 0,
            event_stop: Arc::new(AtomicBool::new(false)),
            event_threads: Vec::new(),
        };
        runtime.event_stop.store(true, Ordering::Release);
        update_config_maps(&mut cc_object, config, 0)?;
        let struct_ops_link = {
            let mut map = find_map_mut(&mut cc_object, "skyline_cc")?;
            map.attach_struct_ops()
                .context("attach skyline_cc struct_ops")?
        };
        runtime.links.push(struct_ops_link);
        // With the log off nothing drains the ring buffer: once it is full,
        // bpf_ringbuf_reserve() fails and skyline_emit() drops the event,
        // which costs less than reading and discarding every one.
        if let Some(event_log) = event_log {
            runtime.event_threads.push(start_event_reader(
                &cc_object,
                "events",
                event_log,
                runtime.event_stop.clone(),
            )?);
        }
        runtime.objects.push(cc_object);

        Ok(runtime)
    }

    fn update_config(&mut self, config: &SkylineConfig) -> Result<()> {
        let next_slot = (self.active_slot + 1) & 1;
        let cc_object = self
            .objects
            .first_mut()
            .ok_or_else(|| anyhow!("skyline_cc object is not loaded"))?;
        update_config_maps(cc_object, config, next_slot)?;
        self.active_slot = next_slot;
        Ok(())
    }

    fn unregister_struct_ops(&mut self) -> Result<()> {
        let object = self
            .objects
            .first()
            .ok_or_else(|| anyhow!("skyline_cc object is not loaded"))?;
        let map = object
            .maps()
            .find(|map| map.name() == OsStr::new("skyline_cc"))
            .ok_or_else(|| anyhow!("skyline_cc struct_ops map is missing"))?;
        map.delete(&0_u32.to_ne_bytes())
            .context("unregister skyline_cc struct_ops")
    }

    fn active_flows(&self) -> u64 {
        let Some(object) = self.objects.first() else {
            return 0;
        };
        let Some(map) = object
            .maps()
            .find(|map| map.name() == OsStr::new("flow_count"))
        else {
            return 0;
        };
        map.lookup(&0_u32.to_ne_bytes(), MapFlags::ANY)
            .ok()
            .flatten()
            .and_then(|value| try_pod_read_unaligned::<u64>(&value).ok())
            .unwrap_or(0)
    }

    fn metrics(&self) -> Option<SkylineMetrics> {
        let object = self.objects.first()?;
        let map = object
            .maps()
            .find(|map| map.name() == OsStr::new("metrics"))?;
        let values = map
            .lookup_percpu(&0_u32.to_ne_bytes(), MapFlags::ANY)
            .ok()
            .flatten()?;
        let mut total = SkylineMetrics::default();
        for value in values {
            let Ok(stats) = try_pod_read_unaligned::<SkylineMetrics>(&value) else {
                continue;
            };
            total.ack_events = total.ack_events.saturating_add(stats.ack_events);
            total.delivered_packets = total
                .delivered_packets
                .saturating_add(stats.delivered_packets);
            total.loss_events = total.loss_events.saturating_add(stats.loss_events);
            total.state_transitions = total
                .state_transitions
                .saturating_add(stats.state_transitions);
            total.pacing_updates = total.pacing_updates.saturating_add(stats.pacing_updates);
            total.guardrail_hits = total.guardrail_hits.saturating_add(stats.guardrail_hits);
            total.hypothetical_early_loss = total
                .hypothetical_early_loss
                .saturating_add(stats.hypothetical_early_loss);
            total.prr_adjustments = total.prr_adjustments.saturating_add(stats.prr_adjustments);
        }
        Some(total)
    }
}

impl Drop for BpfRuntime {
    fn drop(&mut self) {
        // Detaching the struct_ops `Link` alone does NOT remove "skyline_cc"
        // from `tcp_available_congestion_control` -- confirmed empirically:
        // after a clean process exit that only ran `link.detach()`,
        // `skyline_cc` was still listed. Unregistering
        // the struct_ops MAP element (what `Request::Drain` already does
        // explicitly via `unregister_struct_ops()`) is what actually
        // triggers the kernel's tcp_congestion_ops unregister hook; `Link`
        // detachment only releases this process's own hold on the link
        // object. Doing both here mirrors Drain's own sequence (explicit
        // unregister, then let Drop's detach run against an
        // already-unregistered link, which is a harmless no-op).
        let _ = self.unregister_struct_ops();
        for link in &self.links {
            let _ = link.detach();
        }
        self.event_stop.store(false, Ordering::Release);
        for thread in self.event_threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// `skyline_select_congestion_control` (`bpf/skyline_policy.bpf.c`) runs independently
/// of `BpfRuntime`/the CC struct_ops: M1's RACK observation and per-flow RTO
/// tuning must work even when Skyline Speeder CC has never been enabled (the
/// `controlled-cubic` experiment profiles never call `Request::Enable`), so
/// this object is loaded once at daemon startup and lives for the daemon's
/// entire lifetime, independent of `Enable`/`Drain`.
///
/// Every failure mode here is recorded in `errors` rather than propagated,
/// mirroring `TcRuntime`: a missing cgroup path (a real possibility on
/// a host that hasn't run `install-guest.sh`) must not prevent the daemon
/// from starting.
struct PolicyRuntime {
    object: Option<Object>,
    link: Option<Link>,
    errors: Vec<String>,
}

impl PolicyRuntime {
    fn load(config: &SkylineConfig) -> Self {
        let mut errors = Vec::new();
        let policy_path = config.runtime.bpf_dir.join("skyline_policy.bpf.o");
        let mut object = match load_object(&policy_path) {
            Ok(object) => object,
            Err(error) => {
                errors.push(format!("policy object unavailable: {error:#}"));
                return Self {
                    object: None,
                    link: None,
                    errors,
                };
            }
        };
        // Default to disabled: CC selection must not flip on until an
        // explicit `Enable`, and RTO tuning must not subscribe to RTT_CB
        // until an explicit `SetRackRto`. This object loads at daemon
        // startup, before any `Enable`/`SetRackRto` call, which makes this
        // explicit zero-initialization load-bearing -- otherwise a baseline
        // `controlled-cubic`/`stock-cubic` profile's connections could be
        // silently switched onto `skyline_cc`.
        if let Err(error) = update_u32_map(&mut object, "policy_enabled", 0, 0) {
            errors.push(format!("policy_enabled initialization failed: {error:#}"));
        }
        if let Err(error) = write_rto_tuning(&mut object, &RackRtoConfig::disabled(), 0) {
            errors.push(format!("rto_tuning initialization failed: {error:#}"));
        }
        let link = match File::open(&config.runtime.cgroup_path) {
            Ok(cgroup) => {
                let attached = object
                    .progs_mut()
                    .find(|program| {
                        program.name() == OsStr::new("skyline_select_congestion_control")
                    })
                    .ok_or_else(|| anyhow!("policy program is missing"))
                    .and_then(|program| {
                        program
                            .attach_cgroup(cgroup.as_raw_fd())
                            .context("attach cgroup sockops policy")
                    });
                match attached {
                    Ok(link) => Some(link),
                    Err(error) => {
                        errors.push(format!("policy attach failed: {error:#}"));
                        None
                    }
                }
            }
            Err(error) => {
                errors.push(format!(
                    "open cgroup path {}: {error:#}",
                    config.runtime.cgroup_path.display()
                ));
                None
            }
        };
        Self {
            object: Some(object),
            link,
            errors,
        }
    }

    fn set_policy_enabled(&mut self, enabled: bool) -> Result<()> {
        let object = self
            .object
            .as_mut()
            .ok_or_else(|| anyhow!("policy object is not loaded"))?;
        update_u32_map(object, "policy_enabled", 0, u32::from(enabled))
    }

    fn set_rto_tuning(&mut self, config: &RackRtoConfig, generation: u32) -> Result<()> {
        let object = self
            .object
            .as_mut()
            .ok_or_else(|| anyhow!("policy object is not loaded"))?;
        write_rto_tuning(object, config, generation)
    }

    fn rto_stats(&self) -> Option<RackRtoStats> {
        let object = self.object.as_ref()?;
        let map = object
            .maps()
            .find(|map| map.name() == OsStr::new("rto_stats"))?;
        let values = map
            .lookup_percpu(&0_u32.to_ne_bytes(), MapFlags::ANY)
            .ok()
            .flatten()?;
        let mut total = RackRtoStats::default();
        for value in values {
            let Ok(stats) = try_pod_read_unaligned::<RackRtoStats>(&value) else {
                continue;
            };
            total.rtt_callbacks = total.rtt_callbacks.saturating_add(stats.rtt_callbacks);
            total.applied = total.applied.saturating_add(stats.applied);
            total.rejected = total.rejected.saturating_add(stats.rejected);
            total.skipped_warmup = total.skipped_warmup.saturating_add(stats.skipped_warmup);
            total.unchanged = total.unchanged.saturating_add(stats.unchanged);
            total.established_cb = total.established_cb.saturating_add(stats.established_cb);
            total.subscribe_ok = total.subscribe_ok.saturating_add(stats.subscribe_ok);
            total.subscribe_err = total.subscribe_err.saturating_add(stats.subscribe_err);
            total.rto_max_applied = total.rto_max_applied.saturating_add(stats.rto_max_applied);
            total.rto_max_rejected = total
                .rto_max_rejected
                .saturating_add(stats.rto_max_rejected);
            total.rto_max_unchanged = total
                .rto_max_unchanged
                .saturating_add(stats.rto_max_unchanged);
            total.rto_max_congested = total
                .rto_max_congested
                .saturating_add(stats.rto_max_congested);
        }
        Some(total)
    }
}

impl Drop for PolicyRuntime {
    fn drop(&mut self) {
        if let Some(link) = self.link.take() {
            let _ = link.detach();
        }
    }
}

fn write_rto_tuning(object: &mut Object, config: &RackRtoConfig, generation: u32) -> Result<()> {
    let kernel_tuning = config.kernel_config(generation);
    let map = find_map_mut(object, "rto_tuning")?;
    map.update(
        &0_u32.to_ne_bytes(),
        bytes_of(&kernel_tuning),
        MapFlags::ANY,
    )
    .context("update rto_tuning map")
}

fn write_retransmit_dscp_config(object: &mut Object, config: &RetransmitDscpConfig) -> Result<()> {
    let kernel_config = config.kernel_config();
    let map = find_map_mut(object, "retransmit_dscp_config")?;
    map.update(
        &0_u32.to_ne_bytes(),
        bytes_of(&kernel_config),
        MapFlags::ANY,
    )
    .context("update retransmit_dscp_config map")
}

/// Loads and tracks the TC (`skyline_tc.bpf.c`) object: interface-wide egress
/// packet/byte/retransmit-DSCP accounting, independent of M1-M4 and of
/// which congestion control a given flow runs.
struct TcRuntime {
    objects: Vec<Object>,
    tc_hook: Option<TcHook>,
    errors: Vec<String>,
}

impl TcRuntime {
    fn load(config: &SkylineConfig) -> Result<Self> {
        let mut runtime = Self {
            objects: Vec::new(),
            tc_hook: None,
            errors: Vec::new(),
        };

        if let Some(interface) = &config.runtime.tc_interface {
            // Refused, not attached with a warning: see require_ethernet().
            // The error becomes tc_error, so status says why, and
            // set-retransmit-dscp then fails instead of enabling a writer
            // that would write inside the IP header.
            require_ethernet(interface, interface_type(interface))?;
            let tc_path = config.runtime.bpf_dir.join("skyline_tc.bpf.o");
            match load_tc_observer(&tc_path, interface) {
                Ok((mut object, hook)) => {
                    // Default to disabled: same load-bearing zero-init
                    // rationale as PolicyRuntime::load()'s rto_tuning reset
                    // -- marking must not turn on until an explicit
                    // SetRetransmitDscp, and a stale enabled=1 left over
                    // from a previous incarnation's map contents must not
                    // survive into this one (replace(true) on the clsact
                    // hook preserves the underlying qdisc across restarts,
                    // but this is a freshly-loaded object/map either way).
                    if let Err(error) =
                        write_retransmit_dscp_config(&mut object, &RetransmitDscpConfig::disabled())
                    {
                        runtime.errors.push(format!(
                            "retransmit_dscp_config initialization failed: {error:#}"
                        ));
                    }
                    runtime.tc_hook = Some(hook);
                    runtime.objects.push(object);
                }
                Err(error) => runtime
                    .errors
                    .push(format!("TC observer unavailable: {error}")),
            }
        }
        Ok(runtime)
    }

    fn set_retransmit_dscp(&mut self, config: &RetransmitDscpConfig) -> Result<()> {
        let object = self
            .objects
            .iter_mut()
            .find(|object| {
                object
                    .maps()
                    .any(|map| map.name() == OsStr::new("retransmit_dscp_config"))
            })
            .ok_or_else(|| anyhow!("TC object is not loaded (runtime.tc_interface unset?)"))?;
        write_retransmit_dscp_config(object, config)
    }

    fn retransmit_dscp_stats(&self) -> Option<RetransmitDscpStats> {
        let map = self.objects.iter().find_map(|object| {
            object
                .maps()
                .find(|map| map.name() == OsStr::new("retransmit_dscp_stats"))
        })?;
        let values = map
            .lookup_percpu(&0_u32.to_ne_bytes(), MapFlags::ANY)
            .ok()
            .flatten()?;
        let mut total = RetransmitDscpStats::default();
        for value in values {
            let Ok(stats) = try_pod_read_unaligned::<RetransmitDscpStats>(&value) else {
                continue;
            };
            total.packets_seen = total.packets_seen.saturating_add(stats.packets_seen);
            total.retransmits_detected = total
                .retransmits_detected
                .saturating_add(stats.retransmits_detected);
            total.retransmits_marked = total
                .retransmits_marked
                .saturating_add(stats.retransmits_marked);
            total.csum_fixups = total.csum_fixups.saturating_add(stats.csum_fixups);
            total.abi_mismatch = total.abi_mismatch.saturating_add(stats.abi_mismatch);
            total.ipv6_marked = total.ipv6_marked.saturating_add(stats.ipv6_marked);
            total.ipv6_chain_bailout = total
                .ipv6_chain_bailout
                .saturating_add(stats.ipv6_chain_bailout);
        }
        Some(total)
    }

    fn tc_stats(&self) -> Option<TcStats> {
        let map = self.objects.iter().find_map(|object| {
            object
                .maps()
                .find(|map| map.name() == OsStr::new("tc_stats"))
        })?;
        let values = map
            .lookup_percpu(&0_u32.to_ne_bytes(), MapFlags::ANY)
            .ok()
            .flatten()?;
        let mut total = TcStats::default();
        for value in values {
            let Ok(stats) = try_pod_read_unaligned::<TcStats>(&value) else {
                continue;
            };
            total.packets = total.packets.saturating_add(stats.packets);
            total.bytes = total.bytes.saturating_add(stats.bytes);
            total.gso_packets = total.gso_packets.saturating_add(stats.gso_packets);
            total.drops = total.drops.saturating_add(stats.drops);
        }
        Some(total)
    }
}

impl Drop for TcRuntime {
    fn drop(&mut self) {
        if let Some(mut hook) = self.tc_hook.take() {
            let _ = hook.detach();
        }
    }
}

fn start_event_reader(
    object: &Object,
    map_name: &str,
    event_log: Arc<Mutex<EventLog>>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let map = object
        .maps()
        .find(|map| map.name() == OsStr::new(map_name))
        .ok_or_else(|| anyhow!("ring buffer map {map_name} is missing"))?;
    let mut builder = RingBufferBuilder::new();
    builder.add(&map, move |data| {
        let Ok(event) = try_from_bytes::<SkylineEvent>(data) else {
            return 0;
        };
        let payload = serde_json::json!({
            "timestamp_ns": event.timestamp_ns,
            "socket_cookie": event.socket_cookie,
            "value_a": event.value_a,
            "value_b": event.value_b,
            "type": event.event_type,
            "state": event.state,
        });
        // One write_all() call for the whole line (body + newline), not two
        // separate calls -- the shared Arc<Mutex<EventLog>> (see
        // BpfRuntime::load's doc comment) already serializes the two reader
        // threads, but a single buffer is cheap defense in depth against ever
        // going back to a split write/writeln pair. It is also what lets
        // EventLog rotate only between lines, never in the middle of one.
        if let Ok(mut line) = serde_json::to_vec(&payload) {
            line.push(b'\n');
            if let Ok(mut log) = event_log.lock() {
                let _ = log.append(&line);
            }
        }
        0
    })?;
    let ring_buffer = builder.build()?;
    Ok(thread::spawn(move || {
        while stop.load(Ordering::Acquire) {
            if ring_buffer.poll(Duration::from_millis(200)).is_err() {
                break;
            }
        }
    }))
}

/// The file behind `runtime.events_path`, capped at `max_bytes`. It lives on
/// /run, a RAM-backed tmpfs that the rest of the host needs too: appending
/// without a limit once filled all of it on a busy host, and Docker, which keeps
/// runc state under /run, stopped being able to start containers. Nothing
/// reported an error until then. When a line would take the file past
/// `max_bytes` it is renamed to `<events_path>.1`, replacing the previous one,
/// and a new file is started, so at most about `2 * max_bytes` is ever held.
/// `infra/snapshot-skyline-events.sh` reads across one such rotation.
struct EventLog {
    path: PathBuf,
    rotated_path: PathBuf,
    max_bytes: u64,
    file: File,
    /// Bytes in `file`, tracked here so the common case costs no syscall.
    len: u64,
}

impl EventLog {
    fn open(path: &Path, max_bytes: u64) -> Result<Self> {
        let file =
            open_for_append(path).with_context(|| format!("open event log {}", path.display()))?;
        // Not necessarily empty: under systemd RuntimeDirectory= removes the
        // previous run's log, but nothing does when the daemon runs by hand.
        let len = file
            .metadata()
            .with_context(|| format!("stat event log {}", path.display()))?
            .len();
        let mut rotated_path = path.as_os_str().to_owned();
        rotated_path.push(".1");
        Ok(Self {
            path: path.to_path_buf(),
            rotated_path: rotated_path.into(),
            max_bytes,
            file,
            len,
        })
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let line_len = line.len() as u64;
        if self.len + line_len > self.max_bytes {
            // `len` only counts this process's writes. Re-read the real size
            // before rotating: an operator may have truncated the file in place
            // to reclaim space, which O_APPEND handles without our help.
            self.len = self.file.metadata()?.len();
            // An empty file always takes the line, even one longer than the
            // cap, so a tiny cap cannot turn into rotating on every event.
            if self.len > 0 && self.len + line_len > self.max_bytes {
                self.rotate()?;
            }
        }
        self.file.write_all(line)?;
        self.len += line_len;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        match fs::rename(&self.path, &self.rotated_path) {
            Ok(()) => {}
            // Someone deleted the live file. Its space only comes back once
            // this handle is closed, which replacing it below does.
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            // Rather than write past the cap, drop this line; the next one
            // retries the rename.
            Err(error) => return Err(error),
        }
        self.file = open_for_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

fn open_for_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn interface_index(interface: &str) -> Result<i32> {
    let path = Path::new("/sys/class/net").join(interface).join("ifindex");
    let value = fs::read_to_string(&path)
        .with_context(|| format!("read interface index {}", path.display()))?;
    value
        .trim()
        .parse::<i32>()
        .with_context(|| format!("parse interface index for {interface}"))
}

/// `ARPHRD_ETHER` in /sys/class/net/<if>/type. VLANs, bonds, bridges, veths
/// and taps report it too; WireGuard, tun, GRE/IPIP and PPP do not.
const ARPHRD_ETHER: u32 = 1;

/// /sys/class/net/<if>/type; `None` when it cannot be read (no such
/// interface, say -- attaching then fails with its own error).
fn interface_type(interface: &str) -> Option<u32> {
    fs::read_to_string(Path::new("/sys/class/net").join(interface).join("type"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// skyline_tc.bpf.c parses an Ethernet header at offset 0 of every egress
/// packet (`struct ethhdr *eth = data`, then ETH_HLEN offsets). On an L3
/// device -- a WireGuard/WARP tunnel, tun, GRE, PPP -- there is no such
/// header: its stats would silently count nothing, and an enabled DSCP
/// writer would write at ETH_HLEN offsets inside the IP header. So it is
/// only attached to an Ethernet device. An unreadable type is let through:
/// the attach reports a missing interface better than this could.
fn require_ethernet(interface: &str, device_type: Option<u32>) -> Result<()> {
    match device_type {
        Some(device_type) if device_type != ARPHRD_ETHER => bail!(
            "{interface} is not an Ethernet device (type {device_type}, e.g. a tunnel); \
             skyline_tc is not attached"
        ),
        _ => Ok(()),
    }
}

fn load_tc_observer(path: &Path, interface: &str) -> Result<(Object, TcHook)> {
    let object = load_object(path)?;
    let ifindex = interface_index(interface)?;
    let hook = {
        let program = object
            .progs()
            .find(|program| program.name() == OsStr::new("skyline_tc_account"))
            .ok_or_else(|| anyhow!("TC accounting program is missing"))?;
        let mut builder = TcHookBuilder::new(program.as_fd());
        builder.ifindex(ifindex).replace(true).handle(1).priority(1);
        let mut hook = builder.hook(TC_EGRESS);
        // clsact is shared interface state and can survive an ungraceful
        // daemon exit. attach(replace=true) is the authoritative operation.
        let _ = hook.create();
        hook.attach()
            .with_context(|| format!("attach TC observer to {interface} egress"))?;
        hook
    };
    Ok((object, hook))
}

/// The congestion control algorithm name skyline_cc registers under. Must stay
/// byte-identical to `.name` in bpf/skyline_cc.bpf.c -- the kernel matches the
/// sysctl write below against the registered name, and a mismatch fails with
/// EINVAL rather than silently doing nothing. Deliberately NOT reused for the
/// struct_ops *map* name lookups elsewhere in this file: those happen to be the
/// same string today but are a different namespace, and collapsing them would
/// couple two things that are free to diverge.
const SKYLINE_CC_NAME: &str = "skyline_cc";

const CONGESTION_CONTROL_SYSCTL: &str = "/proc/sys/net/ipv4/tcp_congestion_control";

/// Writes the network namespace's default congestion control, which is what
/// decides the algorithm for every new connection on the machine that does not
/// ask for something specific. This is the global half of activation; the
/// cgroup sockops policy is the per-connection half, and the two are
/// independent (see `Request::Enable`).
///
/// skyline-speederd is this sysctl's single owner, and every write goes
/// through here: `BpfRuntime::load` (fallback_cc, before attach),
/// `Request::Enable` (skyline_cc, after attach), the guard (skyline_cc, while
/// armed -- see guard.rs) and `Request::Drain` (fallback_cc, before unregister,
/// under the guard's lock). infra/boot-disable.sh writes it only as the
/// fallback for when the daemon is already gone.
fn set_default_congestion_control(name: &str) -> Result<()> {
    fs::write(CONGESTION_CONTROL_SYSCTL, name)
        .with_context(|| format!("set default congestion control to {name}"))
}

/// Tier-1 RACK sysctls: applies only the fields the operator explicitly set
/// in `[rack_tuning]` (`None` = skyline-speederd does not own that sysctl at all, see
/// `RackTuningConfig`). A write failure is fatal -- an operator who asked
/// for a specific value deserves a loud failure, not a daemon that quietly
/// runs with different RACK behaviour than requested.
fn apply_rack_tuning(tuning: &RackTuningConfig) -> Result<()> {
    for (path, value) in [
        ("/proc/sys/net/ipv4/tcp_recovery", tuning.tcp_recovery),
        ("/proc/sys/net/ipv4/tcp_reordering", tuning.tcp_reordering),
        (
            "/proc/sys/net/ipv4/tcp_early_retrans",
            tuning.tcp_early_retrans,
        ),
    ] {
        let Some(value) = value else { continue };
        fs::write(path, value.to_string()).with_context(|| format!("set {path} to {value}"))?;
    }
    Ok(())
}

fn read_sysctl_u32(path: &str) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Live re-read of the three RACK sysctls, taken fresh on every `status()`
/// call so `ssctl status`/`validate` reflect whatever the experiment
/// harness (`infra/apply-guest-profile.sh`) most recently wrote, even when
/// skyline-speederd itself owns none of them.
fn read_rack_tuning() -> RackTuningConfig {
    RackTuningConfig {
        tcp_recovery: read_sysctl_u32("/proc/sys/net/ipv4/tcp_recovery"),
        tcp_reordering: read_sysctl_u32("/proc/sys/net/ipv4/tcp_reordering"),
        tcp_early_retrans: read_sysctl_u32("/proc/sys/net/ipv4/tcp_early_retrans"),
    }
}

fn load_object(path: &Path) -> Result<Object> {
    if !path.is_file() {
        bail!("BPF object {} does not exist; run make bpf", path.display());
    }
    let mut builder = ObjectBuilder::default();
    builder
        .open_file(path)
        .with_context(|| format!("open BPF object {}", path.display()))?
        .load()
        .with_context(|| format!("load BPF object {}", path.display()))
}

fn verify_bpf_objects(config: &SkylineConfig) -> Result<()> {
    for name in ["skyline_cc", "skyline_policy", "skyline_tc"] {
        let path = config.runtime.bpf_dir.join(format!("{name}.bpf.o"));
        let object = load_object(&path).with_context(|| format!("verify {name}"))?;
        drop(object);
    }
    Ok(())
}

fn find_map_mut<'object>(
    object: &'object mut Object,
    name: &str,
) -> Result<libbpf_rs::MapMut<'object>> {
    object
        .maps_mut()
        .find(|map| map.name() == OsStr::new(name))
        .ok_or_else(|| anyhow!("BPF map {name} is missing"))
}

fn update_u32_map(object: &mut Object, name: &str, key: u32, value: u32) -> Result<()> {
    let map = find_map_mut(object, name)?;
    map.update(&key.to_ne_bytes(), &value.to_ne_bytes(), MapFlags::ANY)
        .with_context(|| format!("update map {name}"))
}

fn update_config_maps(object: &mut Object, config: &SkylineConfig, slot: u32) -> Result<()> {
    let kernel_config = config.kernel_config();
    {
        let map = find_map_mut(object, "config_slots")?;
        map.update(&slot.to_ne_bytes(), bytes_of(&kernel_config), MapFlags::ANY)
            .context("update inactive config slot")?;
    }
    update_u32_map(object, "active_config_slot", 0, slot)
}

struct Daemon {
    config: SkylineConfig,
    capabilities: CapabilityReport,
    /// Keeps skyline_cc/fq in place between `Enable` and `Drain`. Its thread
    /// is stopped by `Daemon`'s own `Drop`, which runs before any field drops
    /// -- see there for why that order matters.
    guard: Guard,
    runtime: Option<BpfRuntime>,
    tc: Option<TcRuntime>,
    tc_error: Option<String>,
    policy: Option<PolicyRuntime>,
    /// The `[rack_rto]` values present in the configuration file at startup,
    /// restored by `Request::ResetRackRto`.
    rack_rto_defaults: RackRtoConfig,
    /// The M2/M3/M4 coefficients (`[adaptive_cwnd]`/`[loss_classifier]`/the
    /// three top-level limits) present in the configuration file at startup,
    /// restored by `Request::ResetModuleConfig`.
    module_tuning_defaults: ModuleTuningConfig,
    /// The `[retransmit_dscp]` value present in the configuration file at
    /// startup, restored by `Request::ResetRetransmitDscp`.
    retransmit_dscp_defaults: RetransmitDscpConfig,
    /// Opened once at startup by `Daemon::new()` -- see `BpfRuntime::load`'s
    /// doc comment. `None` when `events_max_mib = 0`.
    event_log: Option<Arc<Mutex<EventLog>>>,
}

impl Daemon {
    fn new(config: SkylineConfig) -> Result<Self> {
        let capabilities = probe_capabilities(&config);
        let rack_rto_defaults = config.rack_rto;
        let module_tuning_defaults = ModuleTuningConfig::from_config(&config);
        let retransmit_dscp_defaults = config.retransmit_dscp;
        let event_log = match config.runtime.events_max_mib {
            0 => None,
            max_mib => {
                let path = &config.runtime.events_path;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create event directory {}", parent.display()))?;
                }
                let max_bytes = u64::from(max_mib) * 1024 * 1024;
                Some(Arc::new(Mutex::new(EventLog::open(path, max_bytes)?)))
            }
        };
        let guard = Guard::new(&config);
        Ok(Self {
            config,
            capabilities,
            guard,
            runtime: None,
            tc: None,
            tc_error: None,
            policy: None,
            rack_rto_defaults,
            module_tuning_defaults,
            retransmit_dscp_defaults,
            event_log,
        })
    }

    fn active_flows(&self) -> u64 {
        self.runtime
            .as_ref()
            .map(BpfRuntime::active_flows)
            .unwrap_or(0)
    }

    fn start_tc(&mut self) {
        if self.tc.is_some() {
            return;
        }
        match TcRuntime::load(&self.config) {
            Ok(tc) => self.tc = Some(tc),
            Err(error) => self.tc_error = Some(format!("{error:#}")),
        }
    }

    fn start_policy(&mut self) {
        if self.policy.is_some() {
            return;
        }
        self.policy = Some(PolicyRuntime::load(&self.config));
    }

    fn status(&self) -> RuntimeStatus {
        let mut capabilities = self.capabilities.clone();
        if let Some(error) = &self.tc_error {
            capabilities
                .notes
                .push(format!("TC runtime unavailable: {error}"));
        }
        if let Some(tc) = &self.tc {
            capabilities.notes.extend(tc.errors.iter().cloned());
        }
        if let Some(policy) = &self.policy {
            capabilities.notes.extend(policy.errors.iter().cloned());
        }
        let active_flows = self.active_flows();
        let tc_stats = self.tc.as_ref().and_then(TcRuntime::tc_stats);
        let rack_rto_stats = self.policy.as_ref().and_then(PolicyRuntime::rto_stats);
        let retransmit_dscp_stats = self.tc.as_ref().and_then(TcRuntime::retransmit_dscp_stats);
        let metrics = self.runtime.as_ref().and_then(BpfRuntime::metrics);
        RuntimeStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            enabled: self.runtime.is_some(),
            generation: self.config.generation,
            modules: self.config.enabled_modules.clone(),
            fallback_cc: self.config.fallback_cc.clone(),
            active_flows,
            tc_stats,
            metrics,
            rack_tuning: RackTuningStatus {
                managed: self.config.rack_tuning,
                live: read_rack_tuning(),
            },
            rack_rto: RackRtoStatus {
                config: self.config.rack_rto,
                stats: rack_rto_stats,
            },
            retransmit_dscp: RetransmitDscpStatus {
                config: self.config.retransmit_dscp,
                stats: retransmit_dscp_stats,
            },
            module_tuning: ModuleTuningConfig::from_config(&self.config),
            capabilities,
            guard: self.guard.status(),
        }
    }

    fn handle(&mut self, request: Request) -> Response {
        match self.try_handle(request) {
            Ok(message) => {
                let _ = self.persist_state();
                Response {
                    ok: true,
                    message,
                    status: Some(self.status()),
                }
            }
            Err(error) => Response {
                ok: false,
                message: format!("{error:#}"),
                status: Some(self.status()),
            },
        }
    }

    fn try_handle(&mut self, request: Request) -> Result<String> {
        match request {
            Request::Validate => {
                self.config.validate()?;
                validate_capabilities(&self.capabilities)?;
                Ok("configuration and kernel capabilities are valid".to_owned())
            }
            Request::Enable { modules } => {
                if let Some(modules) = modules {
                    self.config.enabled_modules = modules;
                }
                self.config.generation = self.config.generation.saturating_add(1);
                self.config.validate()?;
                validate_capabilities(&self.capabilities)?;
                if let Some(runtime) = &mut self.runtime {
                    runtime.update_config(&self.config)?;
                } else {
                    self.runtime = Some(BpfRuntime::load(&self.config, self.event_log.clone())?);
                }
                if let Some(policy) = &mut self.policy {
                    policy.set_policy_enabled(true)?;
                }
                // Attaching the struct_ops only adds skyline_cc to
                // tcp_available_congestion_control -- registration is not
                // activation, the same way `modprobe tcp_bbr` does not make BBR
                // the default. Switching the namespace default is what actually
                // moves new connections onto it, machine-wide.
                //
                // This runs AFTER the attach, never before: the kernel rejects a
                // sysctl write naming an algorithm it has not seen registered,
                // so the reverse order would turn a failed attach into a failed
                // enable that also left the default pointing at nothing.
                set_default_congestion_control(SKYLINE_CC_NAME)?;
                // Only now, with skyline_cc attached and already the default,
                // is there anything for the guard to keep: arm it and run one
                // full pass right away (default_qdisc, then the root qdisc of
                // tc_interface or of the NICs under it), also when [guard]
                // interval_s = 0. It also retries at once a replace the
                // periodic pass is backing off from. Nothing it does can
                // fail the enable -- skyline_cc is live at this point, and a
                // qdisc it could not fix is reported in this message and in
                // `guard.last_error` instead.
                let report = self.guard.arm_and_enforce();
                let mut message = format!(
                    "Skyline Speeder enabled with modules: {} -- {SKYLINE_CC_NAME} is now the \
                     default congestion control for every new connection on this host",
                    display_modules(&self.config.enabled_modules)
                );
                for line in report.lines() {
                    message.push_str("; ");
                    message.push_str(line);
                }
                Ok(message)
            }
            Request::DisableModule { module } => {
                self.config.enabled_modules.retain(|item| *item != module);
                self.config.generation = self.config.generation.saturating_add(1);
                if let Some(runtime) = &mut self.runtime {
                    runtime.update_config(&self.config)?;
                }
                Ok(format!("module {module} disabled at the next RTT boundary"))
            }
            Request::Status => Ok("status returned".to_owned()),
            Request::Flows => Ok(
                "flow enumeration is intentionally local-only; aggregate count returned".to_owned(),
            ),
            Request::Snapshot { path } => {
                let payload = serde_json::to_vec_pretty(&self.status())?;
                fs::write(&path, payload)
                    .with_context(|| format!("write snapshot {}", path.display()))?;
                Ok(format!("snapshot written to {}", path.display()))
            }
            Request::Drain { timeout_s } => {
                // Order matters, and mirrors infra/boot-disable.sh: stop
                // handing NEW connections to skyline_cc before waiting for the
                // existing ones, or drain races against connections created
                // while it waits. Both dispatch paths have to close, and the
                // global sysctl closes first -- it is the one that admits
                // traffic from every process on the host, not just the cgroup.
                //
                // Doing this first also means the struct_ops unregister below
                // never runs while the namespace default still names it.
                //
                // The guard is disarmed before that write and the write happens
                // while its lock is still held: a periodic pass already running
                // finishes first (its skyline_cc write, if any, is overwritten
                // here), and none can start in between and put skyline_cc back
                // after drain wrote fallback_cc. Waiting for that pass is
                // bounded by about one guard.rs TC_TIMEOUT even while the rtnl
                // lock is stuck: a tc killed at the timeout is reaped in the
                // background, and every later tc of the pass fails at once.
                // It stays disarmed whatever the rest of the drain returns: the
                // experiment harness drains with --timeout 0, which bails
                // below, and then installs its own qdisc -- the guard must not
                // fight it. Drain touches no qdisc.
                self.guard
                    .disarm_and(|| set_default_congestion_control(&self.config.fallback_cc))?;
                if let Some(policy) = &mut self.policy {
                    policy.set_policy_enabled(false)?;
                }
                // active_flows() rather than status(): status() now also runs
                // `tc` for the guard's live view, five times a second here.
                let deadline = Instant::now() + Duration::from_secs(timeout_s);
                while self.active_flows() > 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(200));
                }
                if self.active_flows() > 0 {
                    bail!(
                        "drain timeout expired; struct_ops remains attached, but the default \
                         congestion control is already back on {} so no new connection is \
                         using it",
                        self.config.fallback_cc
                    );
                }
                if let Some(runtime) = &mut self.runtime {
                    runtime.unregister_struct_ops()?;
                }
                self.runtime = None;
                Ok(format!(
                    "new sockets use {}; Skyline Speeder CC links detached (TC stats and RTO tuning, \
                     if enabled, remain active)",
                    self.config.fallback_cc
                ))
            }
            Request::SetRackRto { config } => {
                let previous = self.config.rack_rto;
                self.config.rack_rto = config;
                if let Err(error) = self.config.validate() {
                    self.config.rack_rto = previous;
                    return Err(error.into());
                }
                self.config.generation = self.config.generation.saturating_add(1);
                if let Some(policy) = &mut self.policy {
                    policy.set_rto_tuning(&self.config.rack_rto, self.config.generation)?;
                } else {
                    bail!("policy runtime is not available; check capabilities notes");
                }
                Ok(format!(
                    "rack RTO tuning updated (generation {})",
                    self.config.generation
                ))
            }
            Request::ResetRackRto => {
                self.config.rack_rto = self.rack_rto_defaults;
                self.config.generation = self.config.generation.saturating_add(1);
                if let Some(policy) = &mut self.policy {
                    policy.set_rto_tuning(&self.config.rack_rto, self.config.generation)?;
                } else {
                    bail!("policy runtime is not available; check capabilities notes");
                }
                Ok("rack RTO tuning reset to configuration defaults".to_owned())
            }
            Request::SetModuleConfig { config } => {
                // Unlike SetRackRto, no `bail!` when `self.runtime` is None:
                // these coefficients only matter once Skyline Speeder CC is enabled, so
                // updating `self.config` now and letting the next `Enable`
                // pick it up naturally (via BpfRuntime::load's
                // update_config_maps call) is correct, not a missed error.
                let previous = ModuleTuningConfig::from_config(&self.config);
                config.apply_to(&mut self.config);
                if let Err(error) = self.config.validate() {
                    previous.apply_to(&mut self.config);
                    return Err(error.into());
                }
                self.config.generation = self.config.generation.saturating_add(1);
                if let Some(runtime) = &mut self.runtime {
                    runtime.update_config(&self.config)?;
                }
                Ok(format!(
                    "module tuning updated (generation {})",
                    self.config.generation
                ))
            }
            Request::ResetModuleConfig => {
                self.module_tuning_defaults.apply_to(&mut self.config);
                self.config.generation = self.config.generation.saturating_add(1);
                if let Some(runtime) = &mut self.runtime {
                    runtime.update_config(&self.config)?;
                }
                Ok("module tuning reset to configuration defaults".to_owned())
            }
            Request::SetRetransmitDscp { config } => {
                let previous = self.config.retransmit_dscp;
                self.config.retransmit_dscp = config;
                if let Err(error) = self.config.validate() {
                    self.config.retransmit_dscp = previous;
                    return Err(error.into());
                }
                // No generation bump: unlike rack_rto/module_config, this
                // struct carries no generation field and nothing reads one
                // for it -- see the doc comment on
                // struct skyline_retransmit_dscp_config in skyline_abi.h.
                if let Some(tc) = &mut self.tc {
                    tc.set_retransmit_dscp(&self.config.retransmit_dscp)?;
                } else {
                    bail!("TC runtime is not loaded; check runtime.tc_interface and tc_error in status");
                }
                Ok("retransmit DSCP marking updated".to_owned())
            }
            Request::ResetRetransmitDscp => {
                self.config.retransmit_dscp = self.retransmit_dscp_defaults;
                if let Some(tc) = &mut self.tc {
                    tc.set_retransmit_dscp(&self.config.retransmit_dscp)?;
                } else {
                    bail!("TC runtime is not loaded; check runtime.tc_interface and tc_error in status");
                }
                Ok("retransmit DSCP marking reset to configuration defaults".to_owned())
            }
            // Handled in `serve()`'s loop, which detects this variant before
            // dispatching here and breaks out instead -- see its doc comment.
            // Reaching this arm at all (e.g. a client sending it directly)
            // is harmless: it does not mutate any state.
            Request::Shutdown => Ok("shutting down".to_owned()),
        }
    }

    fn persist_state(&self) -> Result<()> {
        if let Some(parent) = self.config.runtime.state_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
        }
        fs::write(
            &self.config.runtime.state_path,
            serde_json::to_vec_pretty(&self.status())?,
        )
        .with_context(|| {
            format!(
                "write runtime state {}",
                self.config.runtime.state_path.display()
            )
        })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Runs before any field drops. The guard's thread must be stopped and
        // joined before `BpfRuntime` drops: that Drop unregisters the
        // struct_ops, and an armed pass after it would try to write a
        // skyline_cc the kernel no longer knows. Stopping it here, rather than
        // relying on field declaration order, keeps a reordered field list from
        // quietly breaking that. No sysctl is written on stop -- the operator
        // explicitly does not want fallback_cc written when the daemon stops;
        // `ssctl drain` (and infra/boot-disable.sh) is how that happens.
        self.guard.stop();
    }
}

fn display_modules(modules: &[Module]) -> String {
    if modules.is_empty() {
        return "none".to_owned();
    }
    modules
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

const KERNEL_BTF_PATH: &str = "/sys/kernel/btf/vmlinux";

/// The kernel wraps every struct_ops type `T` in a `bpf_struct_ops_T` value
/// type, and libbpf resolves exactly this name (as a struct) when it loads the
/// `skyline_cc` map. Its presence is therefore the precondition the load itself
/// depends on, which the string "struct_ops" appearing somewhere is not:
/// `bpftool feature probe` names it on every line about it, "map_type
/// struct_ops is NOT available" included, so the old substring match was true
/// whenever bpftool ran at all and false whenever it was not installed.
const STRUCT_OPS_VALUE_TYPE: &str = "bpf_struct_ops_tcp_congestion_ops";

/// Function-pointer member added by the out-of-tree bounded RACK patch. No
/// upstream kernel has it, so `false` is the normal answer.
const RACK_REO_HOOK_MEMBER: &str = "rack_reo_wnd";

/// Capabilities that are facts about the kernel's type information.
#[derive(Debug, Default, PartialEq, Eq)]
struct BtfCapabilities {
    struct_ops: bool,
    rack_reo_hook: bool,
}

fn probe_btf_capabilities(btf: &Btf<'_>) -> BtfCapabilities {
    let mut capabilities = BtfCapabilities::default();
    // Filtered by kind rather than looked up by name: `type_by_name` returns the
    // first type of *any* kind carrying the name, while libbpf's own lookup is
    // `btf__find_by_name_kind(.., BTF_KIND_STRUCT)`. The RACK hook needs the
    // walk anyway -- the patch, not this repository, decides which struct
    // carries the member.
    for ty in btf.type_by_kind::<Struct<'_>>() {
        if ty.name() == Some(OsStr::new(STRUCT_OPS_VALUE_TYPE)) {
            capabilities.struct_ops = true;
        }
        if ty.iter().any(|member| {
            member.name == Some(OsStr::new(RACK_REO_HOOK_MEMBER))
                && is_function_pointer(btf, member.ty)
        }) {
            capabilities.rack_reo_hook = true;
        }
    }
    capabilities
}

fn is_function_pointer(btf: &Btf<'_>, type_id: TypeId) -> bool {
    btf.type_by_id::<BtfType<'_>>(type_id)
        .map(|ty| ty.skip_mods_and_typedefs())
        .filter(|ty| ty.kind() == BtfKind::Ptr)
        .and_then(|pointer| pointer.next_type())
        .is_some_and(|pointee| pointee.skip_mods_and_typedefs().kind() == BtfKind::FuncProto)
}

fn probe_capabilities(config: &SkylineConfig) -> CapabilityReport {
    let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| "unknown".to_owned())
        .trim()
        .to_owned();
    let btf = Path::new(KERNEL_BTF_PATH).is_file();
    let bpffs = Path::new("/sys/fs/bpf").is_dir();
    let cgroup_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").is_file();
    let fq_available = command_success("modinfo", &["sch_fq"])
        || fs::read_to_string("/proc/modules")
            .map(|modules| modules.lines().any(|line| line.starts_with("sch_fq ")))
            .unwrap_or(false);
    let mut notes = Vec::new();
    // Read from the kernel BTF in-process, never by shelling out to bpftool.
    // A `--prebuilt` host deliberately has no bpftool, and a missing binary
    // used to read as "no struct_ops": the install failed on a stock kernel
    // that supported it, and a patched kernel would have lost its RACK hook
    // without a word. A parse failure is reported for the same reason -- it
    // must not look like a kernel that simply lacks the feature.
    let BtfCapabilities {
        struct_ops,
        rack_reo_hook,
    } = if btf {
        match Btf::from_path(KERNEL_BTF_PATH) {
            Ok(kernel_btf) => probe_btf_capabilities(&kernel_btf),
            Err(error) => {
                notes.push(format!(
                    "kernel BTF at {KERNEL_BTF_PATH} could not be parsed: {error:#}"
                ));
                BtfCapabilities::default()
            }
        }
    } else {
        BtfCapabilities::default()
    };
    let fallback_cc_available =
        fs::read_to_string("/proc/sys/net/ipv4/tcp_available_congestion_control")
            .map(|available| {
                available
                    .split_whitespace()
                    .any(|name| name == config.fallback_cc.as_str())
            })
            .unwrap_or(false);

    if !config.runtime.cgroup_path.is_dir() {
        notes.push(format!(
            "cgroup {} does not exist",
            config.runtime.cgroup_path.display()
        ));
    }
    if !rack_reo_hook && config.feature_mask().contains(FeatureMask::EARLY_LOSS) {
        notes.push(
            "early-loss remains observation-only because the bounded RACK hook is absent"
                .to_owned(),
        );
    }
    if let Some(interface) = &config.runtime.tc_interface {
        if !Path::new("/sys/class/net").join(interface).is_dir() {
            notes.push(format!("TC interface {interface} does not exist"));
        }
        // Said here too, so `--validate-only` shows it before a start does.
        if let Err(error) = require_ethernet(interface, interface_type(interface)) {
            notes.push(format!("TC interface {error:#}"));
        }
        // Not a capability failure: skyline_cc works without tc, only the
        // guard's root-qdisc half does not -- say so before an enable finds out.
        if config.guard.qdisc && !command_success("tc", &["-V"]) {
            notes.push(format!(
                "tc (iproute2) is not installed; the guard cannot keep the root qdisc of \
                 {interface} (or of the NICs under it) on fq"
            ));
        }
    }
    CapabilityReport {
        kernel_release,
        btf,
        bpffs,
        cgroup_v2,
        fq_available,
        struct_ops,
        rack_reo_hook,
        fallback_cc_available,
        notes,
    }
}

fn validate_capabilities(report: &CapabilityReport) -> Result<()> {
    if !report.btf {
        bail!("kernel BTF is unavailable");
    }
    if !report.bpffs {
        bail!("bpffs is unavailable");
    }
    if !report.cgroup_v2 {
        bail!("cgroup v2 is unavailable");
    }
    if !report.fq_available {
        bail!("sch_fq is unavailable");
    }
    if !report.struct_ops {
        bail!("BPF struct_ops support was not detected");
    }
    if !report.fallback_cc_available {
        bail!("configured fallback congestion control is unavailable");
    }
    Ok(())
}

fn command_success(program: &str, arguments: &[&str]) -> bool {
    Command::new(program)
        .args(arguments)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(socket_path)?.file_type().is_socket() {
        bail!("socket path {} is not a Unix socket", socket_path.display());
    }
    match UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "socket {} is owned by a running daemon",
            socket_path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            fs::remove_file(socket_path)
                .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspect existing socket {}", socket_path.display()))
        }
    }
}

fn serve(mut daemon: Daemon) -> Result<()> {
    let socket_path = daemon.config.runtime.socket_path.clone();
    prepare_socket_path(&socket_path)?;
    apply_rack_tuning(&daemon.config.rack_tuning)?;
    daemon.start_policy();
    daemon.start_tc();
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind Unix socket {}", socket_path.display()))?;
    daemon.persist_state()?;
    // Here and not in Daemon::new(): `--validate-only` must not start a
    // thread. It does nothing until an `Enable` arms it.
    daemon.guard.start()?;
    // `listener.incoming()` below blocks in accept(2) with no timeout.
    // Without a signal handler, systemctl stop's default SIGTERM would kill
    // the process without ever returning to safe Rust code, so `Daemon`'s
    // field `Drop` impls (which detach the skyline_cc struct_ops link and the
    // TC/policy links) would never run, leaking the struct_ops
    // registration. Spawning a real connection to our own socket is the
    // simplest async-signal-safe way to unblock that accept() call: the
    // signal-hook thread below only runs in ordinary thread context (not
    // inside the signal handler itself), so it's free to do blocking I/O.
    spawn_shutdown_signal_watcher(socket_path.clone())?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match handle_stream(&mut daemon, stream) {
                Ok(should_shutdown) => {
                    if should_shutdown {
                        break;
                    }
                }
                Err(error) => eprintln!("request failed: {error:#}"),
            },
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    // `daemon` drops here: Daemon's own Drop stops the guard thread first,
    // then BpfRuntime/PolicyRuntime/TcRuntime's Drop impls run, cleanly
    // detaching every link -- this is the ONLY path (besides a fatal accept
    // error) that reaches this point, so shutdown cleanup and the loop's
    // normal exit are the same code path.
    Ok(())
}

/// Returns `Ok(true)` iff the request was `Request::Shutdown`, signalling
/// the caller to stop the accept loop. A failure to write the response
/// back does NOT cancel that signal -- the shutdown-signal watcher below
/// deliberately doesn't wait for/read a response (it has nothing further
/// to do once the request is sent), so the write below routinely loses
/// its peer first; treating that as fatal would silently swallow the one
/// request whose delivery matters most.
fn handle_stream(daemon: &mut Daemon, mut stream: UnixStream) -> Result<bool> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: Request = serde_json::from_str(line.trim()).context("decode request")?;
    let should_shutdown = matches!(request, Request::Shutdown);
    let response = daemon.handle(request);
    match serde_json::to_writer(&mut stream, &response) {
        Ok(()) => {
            if let Err(error) = stream.write_all(b"\n") {
                eprintln!("failed to write response: {error:#}");
            }
        }
        Err(error) => eprintln!("failed to write response: {error:#}"),
    }
    Ok(should_shutdown)
}

/// Spawns a background thread that, on SIGTERM/SIGINT, sends a single
/// `Request::Shutdown` over `socket_path` to unblock `serve()`'s blocking
/// accept loop -- see `serve()`'s comment for why this indirection exists
/// instead of a plain shutdown flag.
fn spawn_shutdown_signal_watcher(socket_path: PathBuf) -> Result<JoinHandle<()>> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])
    .context("register SIGTERM/SIGINT handler")?;
    Ok(thread::spawn(move || {
        // Only the first signal matters -- forever() blocks until one
        // arrives, and there is nothing left to do once shutdown has been
        // requested (the process is on its way out either way).
        if signals.forever().next().is_some() {
            let sent: Result<()> = (|| {
                let mut stream = UnixStream::connect(&socket_path)
                    .with_context(|| format!("connect to {}", socket_path.display()))?;
                serde_json::to_writer(&mut stream, &Request::Shutdown)
                    .context("encode shutdown request")?;
                stream.write_all(b"\n").context("write shutdown request")?;
                // Read (and discard) the response before dropping the
                // connection -- handle_stream() tolerates a peer that
                // vanishes mid-write, but reading it here avoids racing
                // that fallback path at all and keeps the exchange clean.
                let mut discard = String::new();
                let _ = BufReader::new(stream).read_line(&mut discard);
                Ok(())
            })();
            if let Err(error) = sent {
                eprintln!("shutdown signal watcher: failed to notify daemon: {error:#}");
            }
        }
    }))
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = SkylineConfig::load(&arguments.config)?;
    let daemon = Daemon::new(config)?;
    if arguments.validate_only {
        let response = serde_json::to_string_pretty(&daemon.status())?;
        println!("{response}");
        validate_capabilities(&daemon.capabilities)?;
        if arguments.verify_bpf {
            verify_bpf_objects(&daemon.config)?;
            eprintln!("all BPF objects passed the kernel verifier");
        }
        return Ok(());
    }
    serve(daemon)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTF_KIND_INT: u32 = 1;
    const BTF_KIND_PTR: u32 = 2;
    const BTF_KIND_STRUCT: u32 = 4;
    const BTF_KIND_FUNC_PROTO: u32 = 13;

    /// Hand-assembled raw BTF, so the probe is tested against kernels this
    /// machine is not running: one without struct_ops, one with the RACK patch.
    /// Asserting on the host's own /sys/kernel/btf/vmlinux would only ever
    /// exercise whichever single answer the build host happens to give.
    struct RawBtf {
        types: Vec<u8>,
        strings: Vec<u8>,
        next_id: u32,
    }

    impl RawBtf {
        fn new() -> Self {
            Self {
                types: Vec::new(),
                // Offset 0 of the string section is the empty (anonymous) name.
                strings: vec![0],
                next_id: 1,
            }
        }

        fn string(&mut self, value: &str) -> u32 {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(value.as_bytes());
            self.strings.push(0);
            offset
        }

        /// `struct btf_type`: name_off, info (kind << 24 | vlen), size-or-type.
        fn push_type(&mut self, name_off: u32, kind: u32, vlen: u32, size_or_type: u32) -> u32 {
            for word in [name_off, kind << 24 | vlen, size_or_type] {
                self.types.extend_from_slice(&word.to_ne_bytes());
            }
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        fn int(&mut self) -> u32 {
            let name_off = self.string("int");
            let id = self.push_type(name_off, BTF_KIND_INT, 0, 4);
            // Trailing u32: encoding 0, bit offset 0, 32 bits wide.
            self.types.extend_from_slice(&32u32.to_ne_bytes());
            id
        }

        /// `void (*)(void)`.
        fn function_pointer(&mut self) -> u32 {
            let proto = self.push_type(0, BTF_KIND_FUNC_PROTO, 0, 0);
            self.push_type(0, BTF_KIND_PTR, 0, proto)
        }

        /// Members are laid out as if each were 8 bytes; nothing reads offsets.
        fn structure(&mut self, name: &str, members: &[(&str, u32)]) -> u32 {
            let name_off = self.string(name);
            let member_names: Vec<u32> =
                members.iter().map(|(name, _)| self.string(name)).collect();
            let id = self.push_type(
                name_off,
                BTF_KIND_STRUCT,
                members.len() as u32,
                members.len() as u32 * 8,
            );
            for (index, (member_name_off, (_, member_type))) in
                member_names.iter().zip(members).enumerate()
            {
                for word in [*member_name_off, *member_type, index as u32 * 64] {
                    self.types.extend_from_slice(&word.to_ne_bytes());
                }
            }
            id
        }

        fn parse(&self, test_name: &str) -> Btf<'static> {
            let mut blob = Vec::new();
            blob.extend_from_slice(&0xeb9f_u16.to_ne_bytes());
            blob.extend_from_slice(&[1, 0]); // version, flags
            for word in [
                24, // hdr_len
                0,  // type_off
                self.types.len() as u32,
                self.types.len() as u32, // str_off
                self.strings.len() as u32,
            ] {
                blob.extend_from_slice(&word.to_ne_bytes());
            }
            blob.extend_from_slice(&self.types);
            blob.extend_from_slice(&self.strings);

            // libbpf-rs 0.24 only parses BTF from a path. Tests share a process,
            // so the name carries the test as well as the pid.
            let path = std::env::temp_dir().join(format!(
                "skyline-speederd-{}-{test_name}.btf",
                std::process::id()
            ));
            fs::write(&path, blob).expect("write BTF fixture");
            let parsed = Btf::from_path(&path);
            let _ = fs::remove_file(&path);
            parsed.expect("parse BTF fixture")
        }
    }

    #[test]
    fn stock_kernel_has_struct_ops_but_no_rack_hook() {
        let mut btf = RawBtf::new();
        let callback = btf.function_pointer();
        let ops = btf.structure("tcp_congestion_ops", &[("cong_control", callback)]);
        btf.structure(STRUCT_OPS_VALUE_TYPE, &[("data", ops)]);

        assert_eq!(
            probe_btf_capabilities(&btf.parse("stock")),
            BtfCapabilities {
                struct_ops: true,
                rack_reo_hook: false,
            }
        );
    }

    #[test]
    fn rack_patched_kernel_reports_the_hook() {
        let mut btf = RawBtf::new();
        let callback = btf.function_pointer();
        let ops = btf.structure(
            "tcp_congestion_ops",
            &[("cong_control", callback), (RACK_REO_HOOK_MEMBER, callback)],
        );
        btf.structure(STRUCT_OPS_VALUE_TYPE, &[("data", ops)]);

        assert_eq!(
            probe_btf_capabilities(&btf.parse("rack-patched")),
            BtfCapabilities {
                struct_ops: true,
                rack_reo_hook: true,
            }
        );
    }

    /// The false positive the bpftool scrape had: a kernel without struct_ops
    /// for TCP still has plenty of types whose names merely contain the word.
    #[test]
    fn struct_ops_is_not_inferred_from_similar_names() {
        let mut btf = RawBtf::new();
        let callback = btf.function_pointer();
        btf.structure("tcp_congestion_ops", &[("cong_control", callback)]);
        btf.structure("bpf_struct_ops", &[("init", callback)]);
        btf.structure("bpf_struct_ops_tcp_congestion_ops_extra", &[]);

        assert_eq!(
            probe_btf_capabilities(&btf.parse("no-struct-ops")),
            BtfCapabilities::default()
        );
    }

    #[test]
    fn rack_hook_must_be_a_function_pointer() {
        let mut btf = RawBtf::new();
        let int = btf.int();
        btf.structure("tcp_sock", &[(RACK_REO_HOOK_MEMBER, int)]);

        assert!(!probe_btf_capabilities(&btf.parse("rack-scalar")).rack_reo_hook);
    }

    #[test]
    fn version_flag_is_recognised() {
        let error = Arguments::try_parse_from(["skyline-speederd", "--version"])
            .expect_err("--version exits through clap");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn skyline_tc_is_only_attached_to_an_ethernet_device() {
        require_ethernet("eth0", Some(ARPHRD_ETHER)).expect("Ethernet");
        // Unreadable: the attach says what is wrong.
        require_ethernet("eth9", None).expect("left to the attach");
        // 65534 = ARPHRD_NONE: WireGuard, tun.
        let error = require_ethernet("wg0", Some(65534)).expect_err("a tunnel");
        assert_eq!(
            format!("{error:#}"),
            "wg0 is not an Ethernet device (type 65534, e.g. a tunnel); skyline_tc is not \
             attached"
        );
        // The loopback device every network namespace has: 772 =
        // ARPHRD_LOOPBACK, read the same way TcRuntime::load reads it.
        if Path::new("/sys/class/net/lo").is_dir() {
            assert_eq!(interface_type("lo"), Some(772));
            assert!(require_ethernet("lo", interface_type("lo")).is_err());
        }
        assert_eq!(interface_type("skyline-no-such-if"), None);
    }

    #[test]
    fn guard_settings_follow_the_config() {
        let mut config = SkylineConfig::load("../../config/speeder.toml").expect("load config");
        config.guard.interval_s = 0;
        config.guard.qdisc = false;
        let mut guard = Guard::new(&config);
        // interval_s = 0: no thread, and nothing armed before an Enable.
        guard.start().expect("start");
        let status = guard.status();
        assert!(!status.armed);
        assert_eq!((status.interval_s, status.qdisc), (0, false));
        assert_eq!(status.live.interface, config.runtime.tc_interface);
    }

    /// A fresh directory per test; the crate has no tempfile dependency and
    /// these tests need nothing more.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("skyline-event-log-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// 16 bytes, so a 64-byte cap holds exactly four.
    const EVENT_LINE: &[u8] = b"0123456789abcde\n";

    #[test]
    fn event_log_rotates_at_the_cap_and_keeps_one_previous_file() {
        let dir = scratch_dir("rotate");
        let path = dir.join("events.jsonl");
        let mut log = EventLog::open(&path, 64).expect("open event log");
        // Five full files' worth plus one line.
        for _ in 0..21 {
            log.append(EVENT_LINE).expect("append");
        }
        assert_eq!(fs::read(&path).expect("read live"), EVENT_LINE);
        assert_eq!(
            fs::read(dir.join("events.jsonl.1")).expect("read rotated"),
            EVENT_LINE.repeat(4),
            "rotation must happen between lines and fill the file to the cap"
        );
        assert_eq!(fs::read_dir(&dir).expect("list").count(), 2);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn event_log_counts_what_an_existing_file_already_holds() {
        let dir = scratch_dir("existing");
        let path = dir.join("events.jsonl");
        fs::write(&path, EVENT_LINE.repeat(4)).expect("seed");
        let mut log = EventLog::open(&path, 64).expect("open event log");
        log.append(EVENT_LINE).expect("append");
        assert_eq!(fs::read(&path).expect("read live"), EVENT_LINE);
        assert_eq!(
            fs::read(dir.join("events.jsonl.1")).expect("read rotated"),
            EVENT_LINE.repeat(4)
        );
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn event_log_follows_an_external_truncate_or_delete() {
        let dir = scratch_dir("external");
        let path = dir.join("events.jsonl");
        let rotated = dir.join("events.jsonl.1");
        let mut log = EventLog::open(&path, 64).expect("open event log");
        for _ in 0..4 {
            log.append(EVENT_LINE).expect("append");
        }

        // Truncated in place: the file has room again, so no rotation.
        File::options()
            .write(true)
            .open(&path)
            .expect("reopen")
            .set_len(0)
            .expect("truncate");
        log.append(EVENT_LINE).expect("append after truncate");
        assert_eq!(fs::read(&path).expect("read live"), EVENT_LINE);
        assert!(!rotated.exists());

        // Deleted: writes go to the unlinked inode until the cap, then the
        // log starts a new file in its place instead of failing forever.
        fs::remove_file(&path).expect("delete");
        for _ in 0..4 {
            log.append(EVENT_LINE).expect("append after delete");
        }
        assert_eq!(fs::read(&path).expect("read live"), EVENT_LINE);
        assert!(!rotated.exists());
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn event_log_takes_a_line_longer_than_the_cap_without_looping() {
        let dir = scratch_dir("oversized");
        let path = dir.join("events.jsonl");
        let mut log = EventLog::open(&path, 8).expect("open event log");
        log.append(EVENT_LINE).expect("first append");
        log.append(EVENT_LINE).expect("second append");
        assert_eq!(fs::read(&path).expect("read live"), EVENT_LINE);
        assert_eq!(
            fs::read(dir.join("events.jsonl.1")).expect("read rotated"),
            EVENT_LINE
        );
        fs::remove_dir_all(&dir).expect("clean up");
    }
}
