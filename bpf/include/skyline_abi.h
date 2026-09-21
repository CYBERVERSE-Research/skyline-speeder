/* SPDX-License-Identifier: GPL-2.0-only
 * Copyright (c) 2026 CYBERVERSE LLC
 */
#ifndef __SKYLINE_ABI_H
#define __SKYLINE_ABI_H

/* ABI version for struct skyline_config / struct skyline_flow_state below. The BPF
 * object and its userspace loader must agree on this value --
 * skyline_config_get_slot() in skyline_cc.bpf.c checks it and returns NULL on a
 * mismatch rather than misinterpreting the struct layout.
 *
 * The target deployment environment is fixed: 10-20% sustained random loss,
 * 100-300ms RTT, fairness explicitly not a goal. M2 owns cwnd directly in
 * every CA state once it is on (see skyline_set_cwnd_target() in skyline_cc.bpf.c),
 * bypassing PRR and the ssthresh-driven reduction machinery entirely. The
 * loss classifier (M3) does not distinguish congestion vs. non-congestion
 * loss in this regime -- it tracks the flow's own loss-rate EMA and turns
 * it into a 1/(1-p) compensation factor for bw_bps's systematic (1-p)
 * under-provisioning (bw_bps is a *delivered*-rate filter) -- see
 * skyline_loss_inflation_permille(). skyline_ssthresh() never reduces cwnd when M2
 * is on -- the queue-delay/ECN guardrail (flow->queue_clamped) is the only
 * thing still allowed to restrain the flow, acting by clamping the gain
 * skyline_set_cwnd_target()/skyline_apply_pacing() use (`guardrail_gain_permille`),
 * not by cutting cwnd.
 */
#define SKYLINE_ABI_VERSION 7
#define SKYLINE_EVENT_RING_SIZE (1U << 20)

enum skyline_feature {
    SKYLINE_FEATURE_EARLY_LOSS = 1U << 0,
    SKYLINE_FEATURE_ADAPTIVE_CWND = 1U << 1,
    SKYLINE_FEATURE_LOSS_CLASSIFIER = 1U << 2,
    SKYLINE_FEATURE_PACING = 1U << 3,
    /* Not one of the M1-M4 ablation modules (enabled_modules never sets
     * this) -- it is a baseline-framework correctness fix, on by default
     * via SkylineConfig::prr_pacing_enabled (independent boolean, defaults
     * true), with this bit existing only so the fix can be switched off
     * for rollback/comparison while every ablation profile (including
     * b2-skyline-base's enabled_modules=[]) still gets it by default. See
     * skyline_apply_prr()'s doc comment for why it exists: cong_control bypasses
     * the kernel's own PRR (tcp_cwnd_reduction()), and letting cwnd sit
     * untouched throughout Recovery/CWR is not enough on its own for the
     * zero-module Skyline Speeder baseline to track real CUBIC's goodput (the B1/B2
     * neutrality gate). With M2 on, skyline_cong_control() bypasses PRR
     * entirely -- this bit and skyline_apply_prr() only still matter on the
     * M2-off (B1/B2 neutrality) path.
     */
    SKYLINE_FEATURE_PRR = 1U << 4,
    /* Same shape as SKYLINE_FEATURE_PRR (baseline-framework fix, not an M1-M4
     * ablation module, driven by SkylineConfig::auto_pacing_enabled which
     * defaults true independent of enabled_modules). cong_control also
     * makes the kernel skip its own tcp_update_pacing_rate()
     * (net/ipv4/tcp_input.c) -- normally called after every RTT sample for
     * *every* TCP connection regardless of which congestion control is
     * active, so real CUBIC gets an automatic pacing-rate ceiling (200% of
     * the current cwnd/srtt rate during slow start, 120% in congestion
     * avoidance) even with M4 (SKYLINE_FEATURE_PACING) off. Without this,
     * SKYLINE_FEATURE_PACING=off leaves `fq` with no effective pacing rate at
     * all, letting slow start burst cwnd to several times the actual queue
     * capacity before the first loss is ever detected -- the dominant
     * remaining cause of the zero-module Skyline Speeder baseline (B2) measuring above
     * real CUBIC (B1)'s goodput if left unfixed. See
     * skyline_apply_auto_pacing_rate().
     */
    SKYLINE_FEATURE_AUTO_PACING = 1U << 5,
};

/* Two modes. STARTUP is the only transient state (bw estimate not yet
 * warmed up, or still probing for the plateau); CRUISE is the terminal
 * state and stays there for the rest of the connection's life -- there is
 * no periodic PROBE cycle (BBR's ProbeBW-style resampling) because CRUISE's
 * own gain is unconditionally > 1.0 (over-sending by design), which keeps
 * refreshing the bw estimate on its own without a dedicated probing phase.
 * See skyline_update_model()'s STARTUP transition check and
 * skyline_set_cwnd_target()/skyline_apply_pacing()'s two-way gain lookup.
 */
enum skyline_mode {
    SKYLINE_MODE_STARTUP = 0,
    SKYLINE_MODE_CRUISE = 1,
};

enum skyline_event_type {
    SKYLINE_EVENT_STATE = 1,
    SKYLINE_EVENT_LOSS = 2,
    /* Values 3-8 are reserved and unused (M2 never reduces cwnd for loss;
     * see this file's top-of-file comment). Left as gaps rather than
     * renumbering GUARDRAIL/UNDO_CWND/GENERATION_SWITCH below, so consumers
     * can rely on those three numeric values staying fixed.
     */
    /* Emitted by skyline_update_model() every time the queue-delay/ECN guardrail
     * actually trips (see flow->queue_clamped's doc comment), whether or not
     * this changes flow->mode. value_a = queue_delay (us), value_b =
     * max_queue_delay_us, state = flow->mode at trip time.
     */
    SKYLINE_EVENT_GUARDRAIL = 9,
    /* Emitted on every skyline_undo_cwnd() struct_ops call (kernel decided a
     * cwnd reduction was spurious, e.g. proven wrong by a later DSACK, and
     * is restoring it). value_a = flow->prior_cwnd (the restore target),
     * value_b = the value actually returned to the kernel, state =
     * flow->mode.
     */
    SKYLINE_EVENT_UNDO_CWND = 10,
    /* Emitted in skyline_cong_control() when a flow adopts a new config slot
     * mid-connection (the double-buffer switch guarding against a torn
     * read within a round -- see skyline_reset_generation()). value_a = old
     * generation, value_b = new generation, state = flow->mode before the
     * switch.
     */
    SKYLINE_EVENT_GENERATION_SWITCH = 11,
};

struct skyline_config {
    __u32 abi_version;
    __u32 generation;
    __u32 feature_mask;
    /* Aggressive initial window, applied once by skyline_init(). Standard
     * IW10 slow-start takes ~7 RTTs (2.1s at 300ms RTT) to reach 1000
     * packets -- too slow for the target box's high-RTT corner. 0 = use
     * whatever the kernel already set.
     */
    __u32 initial_cwnd_packets;
    __u64 max_pacing_bps;
    __u32 max_cwnd_packets;
    __u32 max_queue_delay_us;
    /* Ratio of base RTT added to max_queue_delay_us to form the actual
     * guardrail (max of the two) -- see skyline_max_queue_delay_us(). 0 keeps
     * the guardrail exactly at max_queue_delay_us.
     */
    __u32 max_queue_delay_permille;
    __u32 min_rtt_window_us;
    __u32 bw_window_rtts;
    __u32 startup_plateau_rtts;
    __u32 startup_growth_permille;
    /* SKYLINE_MODE_STARTUP's single gain, applied to both the cwnd target
     * (skyline_set_cwnd_target()) and the pacing rate (skyline_apply_pacing())
     * while M2 is on -- see skyline_abi.h's enum skyline_mode doc comment.
     */
    __u32 startup_gain_permille;
    /* SKYLINE_MODE_CRUISE's cwnd-target gain. Deliberately given more headroom
     * than cruise_pacing_permille below --
     * cwnd only needs to stay ahead of the pacing rate so pacing (not cwnd)
     * is the binding constraint, mirroring how BBR itself treats cwnd as a
     * cap rather than the primary rate control once pacing is active.
     */
    __u32 cruise_inflight_permille;
    /* SKYLINE_MODE_CRUISE's pacing-rate gain -- this is the actual rate control
     * once M4 is on.
     */
    __u32 cruise_pacing_permille;
    /* Compensates every BDP-derived cwnd target and the pacing rate for
     * bw_bps's systematic under-provisioning at measured loss rate p
     * (bw_bps is a *delivered*-rate filter, so it reads link_rate*(1-p))
     * -- see skyline_loss_inflation_permille()'s doc comment. Ceiling on p is
     * SKYLINE_LOSS_INFLATION_MAX_PERMILLE, independent of this field. 0 =
     * disabled (inflation always exactly 1000 = 1.0x).
     */
    __u32 loss_inflation_max_permille;
    /* Gain applied to BOTH the cwnd target (skyline_set_cwnd_target()) and
     * the pacing rate (skyline_apply_pacing()) for the rest of a round in which
     * flow->queue_clamped trips (queue-delay guardrail exceeded, or a fresh
     * ECN CE mark) -- the one signal Skyline Speeder still treats as genuine congestion.
     * 0 = unset (identity: gain stays neutral at 1000 = 1.0x -- "stop
     * inflating" but never actually cut). A deliberately-set value below
     * 1000 makes this guardrail a real, self-protective cut instead of just
     * a ceiling on the boost, applied uniformly to both cwnd and pacing so
     * the protection has teeth regardless of which of M2/M4 happen to be
     * enabled.
     */
    __u32 guardrail_gain_permille;
    /* Floor under M2's BDP-derived cwnd target (skyline_set_cwnd_target()/
     * skyline_bdp_packets()). bw_bps * base_rtt comes out below a handful of
     * packets for any thin or app-limited flow, and a window that small has
     * no way to recover from a loss except a tail-loss probe or an RTO --
     * there are not enough packets behind the hole to produce the SACK
     * feedback RACK needs. Pacing (M4) still sets the send rate, so raising
     * this does not make a flow send faster; it only stops cwnd from being
     * what holds back a retransmission. Never goes below SKYLINE_MIN_CWND
     * (skyline_min_cwnd() takes the max of the two), and is deliberately not
     * consulted on the M2-off path, which keeps the fixed SKYLINE_MIN_CWND so
     * this knob cannot perturb B1/B2 neutrality.
     */
    __u32 min_cwnd_packets;
    /* Explicit tail padding: the __u64 above makes the struct 8-byte
     * aligned, and the Rust mirror (KernelConfig) is bytemuck::Pod, which
     * rejects implicit padding. Always 0.
     */
    __u32 reserved;
};

struct skyline_flow_state {
    __u32 generation;
    __u32 config_slot;
    __u32 mode;
    __u32 counted;
    __u32 round_count;
    __u32 plateau_rounds;
    __u32 prior_cwnd;
    __u32 w_max;
    __u32 last_delivered_ce;
    __u32 last_loss_delivered;
    __u64 min_rtt_us;
    __u64 min_rtt_stamp_us;
    /* Windowed minimum of tp->srtt_us (the 1/8-scaled EWMA smoothed RTT),
     * aged the same way as min_rtt_us above. Unlike a raw per-packet RTT
     * sample, srtt does not collapse toward 0 when a fraction of packets
     * bypass netem's emulated delay under reordering -- see
     * skyline_base_rtt_us()'s doc comment for why this makes it the robust
     * choice for every "how empty is the queue" threshold.
     */
    __u64 min_srtt_us;
    __u64 min_srtt_stamp_us;
    __u64 bw_bps;
    __u64 prior_round_bw_bps;
    __u64 round_rate_bps;
    __u64 bw_samples[10];
    __u32 bw_sample_index;
    /* K (the cubic-curve time-to-reach-w_max constant, milliseconds),
     * frozen once per growth epoch by skyline_cubic_target() the first time it
     * runs after flow->epoch_start_ns is reset to 0. Must NOT be recomputed
     * from the live (growing) cwnd on every call within the same epoch --
     * see skyline_cubic_target()'s comment for the runaway-overshoot bug this
     * caused. Only exercised on the M2-off (B1/B2 neutrality) path.
     */
    __u32 epoch_k_ms;
    __u64 next_round_delivered;
    __u64 epoch_start_ns;
    __u64 neutral_pacing_rate;
    __u32 neutral_pacing_status;
    /* PRR (RFC 6937) accounting, mirroring the kernel's own
     * tp->prr_delivered/tp->prr_out (net/ipv4/tcp_input.c's
     * tcp_cwnd_reduction()) -- Skyline Speeder must keep its own copies because
     * cong_control makes the kernel skip that function entirely. Only
     * exercised on the M2-off path (see skyline_abi.h's top-of-file comment --
     * M2 on bypasses PRR entirely). Reset to 0 by skyline_set_state() on every
     * fresh TCP_CA_Recovery/TCP_CA_CWR entry (a new epoch) and by
     * skyline_reset_generation(). See skyline_apply_prr().
     */
    __u32 prr_delivered;
    __u32 prr_out;
    /* Real CUBIC's "TCP-friendly region" Reno estimate (net/ipv4/tcp_cubic.c's
     * bictcp_update():219,237-238,299-316) -- mirrors ca->tcp_cwnd/ca->ack_cnt.
     * Reset to the pre-cut cwnd / 0 whenever skyline_grow_cwnd() (M2 off) starts a
     * fresh growth epoch. Only exercised on the M2-off path.
     */
    __u32 tcp_cwnd;
    __u32 ack_cnt;
    /* 1 while inside a cwnd-reduction episode (from the first
     * TCP_CA_Recovery/TCP_CA_CWR/TCP_CA_Loss entry until the matching
     * TCP_CA_Open/TCP_CA_Disorder exit). Only meaningful on the M2-off path:
     * gates skyline_set_state()'s CUBIC-parity snap-to-ssthresh on episode exit
     * (mirrors real CUBIC's tcp_end_cwnd_reduction()). With M2 on, cwnd is
     * driven every ACK by skyline_set_cwnd_target() regardless of CA state, so
     * there is nothing to restore on exit.
     */
    __u32 recovery_active;
    /* EMA (permille, alpha=1/4 -> ~4-round time constant) of this flow's
     * own measured loss
     * rate -- fraction of tp->lost growth over tp->delivered growth, updated
     * once per round in skyline_update_model() independently of every feature
     * gate (so skyline_loss_inflation_permille(), an M3 consumer, gets a valid
     * signal even with M2 off). See skyline_loss_inflation_permille().
     */
    __u32 loss_rate_permille;
    /* tp->lost / tp->delivered snapshots from the previous round, consumed
     * by the loss_rate_permille update above to compute this round's delta.
     */
    __u32 last_lost;
    __u32 last_loss_rate_delivered;
    /* One-shot-per-round clamp set by skyline_update_model() when the queue-
     * delay guardrail trips or a fresh ECN CE mark lands -- the only thing
     * still allowed to restrain a loss-tolerant flow. Consumed (as "force
     * gain back to neutral 1.0x this round") by skyline_set_cwnd_target() and
     * skyline_apply_pacing(), then recomputed fresh next round -- deliberately
     * not sticky: a one-shot clamp that reasserts itself every round a real
     * signal is present is exactly as protective, without the risk of a
     * sticky mode never finding a clean exit condition under sustained loss.
     */
    __u32 queue_clamped;
};

struct skyline_metrics {
    __u64 ack_events;
    __u64 delivered_packets;
    __u64 loss_events;
    __u64 state_transitions;
    __u64 pacing_updates;
    __u64 guardrail_hits;
    __u64 hypothetical_early_loss;
    /* How many times skyline_apply_prr() set tp->snd_cwnd (M2-off path only --
     * with M2 on, skyline_cong_control() bypasses PRR entirely, so this should
     * read 0 for any M2-on profile; a nonzero count there means the bypass
     * did not take effect). Deliberately a percpu counter, not a per-event
     * ring-buffer emission -- PRR fires far too often (every ACK during
     * recovery) to log individually without risking exhausting the events
     * ring buffer.
     */
    __u64 prr_adjustments;
};

struct skyline_event {
    __u64 timestamp_ns;
    __u64 socket_cookie;
    __u64 value_a;
    __u64 value_b;
    __u32 type;
    __u32 state;
};

struct skyline_tc_stats {
    __u64 packets;
    __u64 bytes;
    __u64 gso_packets;
    __u64 drops;
};

/* M1 per-flow dynamic RTO floor + ceiling tuning. Deliberately independent
 * of struct skyline_config / SKYLINE_ABI_VERSION: this value changes at most once
 * per experiment case (not every round), so it carries none of the "torn
 * read mid-round" risk that motivates skyline_cc.bpf.c's config_slots double
 * buffer. A single array-map slot, fully overwritten on each update, is
 * sufficient.
 *
 * The rto_max_* fields below drive a second, independent knob --
 * TCP_RTO_MAX_MS, the actual RTO backoff ceiling (~120s by kernel default;
 * an RTO that never fires denies Skyline Speeder the fast-retransmit-friendly
 * cwnd-recovery path). srtt_permille/floor_us/ceiling_us above govern the
 * separate TCP_BPF_RTO_MIN floor feature; the two knobs share one RTT_CB
 * subscription but are otherwise independent (either can be left at its
 * neutral/0 value while the other is active).
 */
#define SKYLINE_RTO_TUNING_ABI_VERSION 2
#define SKYLINE_RTO_MIN_KERNEL_DEFAULT_US 200000U

struct skyline_rto_tuning {
    __u32 abi_version;
    __u32 generation;
    __u32 enabled;          /* 0 = do not subscribe to RTT_CB at all */
    __u32 srtt_permille;    /* RTO_MIN target = max(srtt_us, min_rtt_us) * this / 1000 */
    __u32 floor_us;         /* RTO_MIN absolute safety floor, already HZ-quantized by skyline-speederd */
    __u32 ceiling_us;       /* RTO_MIN absolute safety ceiling, already <= 200000 */
    __u32 warmup_samples;   /* first N RTT samples on a flow are observed only */
    /* TCP_RTO_MAX_MS (the RTO backoff ceiling) = clamp(k * base_rtt, 1000ms,
     * 120000ms), base_rtt = max(srtt_us, min_rtt_us) -- same reordering-
     * robust baseline skyline_update_rto_min() already computes for the floor.
     * k = rto_max_congested_permille once congestion evidence is seen
     * (a fresh CE mark, or current srtt exceeding min_rtt by more than
     * rto_max_congestion_ratio_permille), else rto_max_normal_permille.
     * There is deliberately no "revert to the kernel's 120s default"
     * branch for the congested case -- congestion means backoff should be
     * given more room, not disconnected from base_rtt entirely.
     */
    __u32 rto_max_normal_permille;    /* 0 = whole rto_max feature disabled */
    __u32 rto_max_congested_permille; /* should be >= rto_max_normal_permille */
    __u32 rto_max_congestion_ratio_permille; /* srtt > min_rtt * this/1000 counts as queueing; 0 = built-in default (2000 = 2x) */
    __u32 reserved;
};

struct skyline_rto_stats {
    __u64 rtt_callbacks;
    __u64 applied;           /* TCP_BPF_RTO_MIN (floor) applied */
    __u64 rejected;          /* bpf_setsockopt returned non-zero (floor) */
    __u64 skipped_warmup;
    __u64 unchanged;         /* floor target equalled the previously applied value */
    __u64 established_cb;    /* diagnostic: ESTABLISHED_CB events observed */
    __u64 subscribe_ok;      /* diagnostic: bpf_sock_ops_cb_flags_set() returned 0 */
    __u64 subscribe_err;     /* diagnostic: bpf_sock_ops_cb_flags_set() returned non-zero */
    __u64 rto_max_applied;   /* TCP_RTO_MAX_MS (ceiling) applied, since v2 */
    __u64 rto_max_rejected;  /* bpf_setsockopt returned non-zero (ceiling) */
    __u64 rto_max_unchanged; /* ceiling target equalled the previously applied value */
    __u64 rto_max_congested; /* ceiling computed using the congested k (diagnostic) */
};

/* Retransmit DSCP marking (dual-stack): marks retransmitted TCP segments
 * with a DSCP codepoint in the IPv4 header's ToS byte or the IPv6 header's
 * traffic-class field
 * (same 6-bit DSCP codepoint either way, ECN bits and IPv6 flow label left
 * untouched), so upstream network equipment can apply policy routing keyed
 * on it. dscp_value is deliberately a placeholder -- the actual codepoint is
 * a network-team policy decision, not something Skyline Speeder should hardcode.
 *
 * Deliberately independent of struct skyline_config / SKYLINE_ABI_VERSION, same
 * rationale as struct skyline_rto_tuning above: this changes at most once per
 * deployment, not every round, so a single fully-overwritten array-map slot
 * is enough -- no double-buffering concern. Consumed only by skyline_tc.bpf.c
 * (bpf/skyline_tc.bpf.c), which is interface-wide (not cgroup-scoped) egress on
 * data0, so this marks retransmits for ALL TCP flows on that interface
 * regardless of which congestion control they run (stock CUBIC, BBR, or
 * skyline_cc alike) -- a deliberate scope choice, not an oversight: detection
 * happens by comparing the packet's own TCP sequence number against
 * bpf_tcp_sock(sk)->snd_nxt (read BEFORE tcp_event_new_data_sent() advances
 * it, at the same point tcp_transmit_skb() hands the skb to the device), so
 * it does not depend on skyline_cc/skyline_policy at all and needs no cross-program
 * shared state (struct_ops has no skb access, sockops callbacks in this
 * codebase never touch skb data -- skyline_tc.bpf.c is the only place that can
 * both observe a retransmit and rewrite the packet).
 *
 * Kept deliberately small: no min_payload_bytes/threshold knob -- the
 * pure-ACK/zero-window-probe exclusion (end_seq == seq) already answers
 * "should this packet even be considered", and nothing in the requirement
 * calls for a second knob layered on top of it.
 */
#define SKYLINE_RETRANSMIT_DSCP_ABI_VERSION 2

struct skyline_retransmit_dscp_config {
    __u32 abi_version;
    __u32 enabled;      /* 0 = disabled (safe default; also the zero-init value) */
    __u32 dscp_value;   /* 0-63, the codepoint to stamp. enabled=1 with dscp_value=0
                          * is rejected by Rust-side validate() -- see RetransmitDscpConfig */
    __u32 reserved;
};

struct skyline_retransmit_dscp_stats {
    __u64 packets_seen;          /* IPv4/IPv6 + TCP packets this program actually parsed
                                   * (v6 counts include packets whose extension-header
                                   * chain was walked far enough to reach an IPv6 fixed
                                   * header; see ipv6_chain_bailout below for the ones
                                   * that gave up before that point). */
    __u64 retransmits_detected;  /* end_seq <= snd_nxt, excluding pure ACKs/probes */
    __u64 retransmits_marked;    /* enabled=1 only: every one of the above that carries
                                   * dscp_value out on the wire -- should equal
                                   * retransmits_detected when enabled, 0 when not; a
                                   * mismatch either way means the enable gate is broken.
                                   * NOT the same as csum_fixups below -- when the same
                                   * byte range is retransmitted more than once, the skb's
                                   * tos/traffic-class byte(s) are already dscp_value by
                                   * the 2nd+ attempt (Linux reuses the same header buffer
                                   * across retransmissions), so no rewrite is needed, but
                                   * the packet still counts here. */
    __u64 csum_fixups;           /* subset of retransmits_marked (IPv4 only) that actually
                                   * needed a bpf_l3_csum_replace (tos byte wasn't already
                                   * dscp_value) -- a CPU-cost proxy, not a correctness
                                   * metric; always <= retransmits_marked. Structurally 0
                                   * for every IPv6 packet: IPv6 has no header checksum to
                                   * fix up, so skyline_mark_dscp_v6() never calls
                                   * bpf_l3_csum_replace -- a low csum_fixups/retransmits_marked
                                   * ratio in a dual-stack deployment reflects IPv6 traffic
                                   * share, not a regression. */
    __u64 abi_mismatch;          /* config map's abi_version didn't match; feature no-opped */
    __u64 ipv6_marked;           /* subset of retransmits_marked that went out over IPv6
                                   * (v4 count = retransmits_marked - ipv6_marked). Kept as
                                   * a sub-count of the unified totals above rather than a
                                   * parallel v4/v6 struct pair -- splitting the whole
                                   * struct would cost a second map lookup on every packet
                                   * and force every consumer (skyline-speederd's percpu sum,
                                   * analyze_results.py, both manifests) to learn "sum two
                                   * sources" for no analytical benefit; the one invariant
                                   * that actually matters (retransmits_marked ==
                                   * retransmits_detected when enabled) is a whole-program
                                   * property that a split would only make harder to check. */
    __u64 ipv6_chain_bailout;    /* IPv6 packets where the extension-header walk gave up
                                   * without ever reaching a TCP header -- deeper than the
                                   * unrolled walk's bound, a Fragment/AH/ESP/unknown
                                   * nexthdr, or the walk's own byte-length budget. This is
                                   * the fail-open path's failure mode made observable: the
                                   * walk always fails open (never drops a packet), but a
                                   * silently-high bailout rate would mean IPv6 retransmits
                                   * are going unmarked for a real, fixable reason (e.g. a
                                   * bound picked too small for real-world traffic) rather
                                   * than "no IPv6 traffic at all" -- watch this counter,
                                   * not just retransmits_marked==0, when diagnosing "why
                                   * isn't IPv6 getting marked".
                                   *
                                   * On real traffic, this also counts
                                   * ordinary non-TCP IPv6 packets (ICMPv6/NDP and the like) --
                                   * their nexthdr is just as "unrecognized" to the walk as a
                                   * genuinely too-deep TCP extension-header chain would be,
                                   * and the two cases cannot be told apart without deeper
                                   * inspection than this feature needs. On a normal IPv6
                                   * network this counter's baseline will never be zero
                                   * purely from background ND/RA/etc. traffic -- that is
                                   * expected, not evidence of a chain-walk problem by itself;
                                   * a spike well above that baseline, or ipv6_marked staying
                                   * at 0 while retransmits_detected (IPv4) or other evidence
                                   * shows genuine IPv6 TCP traffic exists, is the actual
                                   * signal worth investigating.
                                   */
};

#endif
