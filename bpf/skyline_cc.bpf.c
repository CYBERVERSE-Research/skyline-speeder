// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
/*
 * Skyline Speeder congestion-control prototype.
 *
 * A rate-driven controller for a fixed target deployment (10-20% sustained
 * random loss, 100-300ms RTT, fairness explicitly not a goal). M2 owns
 * cwnd directly in every CA state
 * once it is on (see skyline_set_cwnd_target()), bypassing PRR and the
 * ssthresh-driven reduction machinery entirely -- those two mechanisms are
 * kept only on the M2-off path, where they are what makes a zero-module Skyline Speeder
 * profile track real CUBIC (the B1/B2 neutrality invariant this file's
 * neutral growth core exists to satisfy). That neutral core is independently
 * derived from RFC 9438 and must pass the B1/B2 parity gate before results
 * from optional modules are interpreted.
 */
#include "vmlinux.h"
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include "skyline_abi.h"

#define MSEC_PER_SEC 1000ULL
#define USEC_PER_SEC 1000000ULL
#define NSEC_PER_SEC 1000000000ULL
#define SKYLINE_CUBIC_C_PERMILLE 400ULL
#define SKYLINE_CUBIC_BETA_PERMILLE 700U
#define SKYLINE_MIN_CWND 4U
/* Real CUBIC's "already past the curve's current target" growth rate --
 * see skyline_grow_cwnd()'s doc comment for why this must be ~cwnd, not 1. */
#define SKYLINE_CUBIC_STALLED_WEIGHT_SCALE 100U
/* Absolute floor for every min-RTT-relative "queue is basically empty"
 * threshold -- see skyline_low_queue_us()'s doc comment. */
#define SKYLINE_QUEUE_LOW_FLOOR_US 1000ULL
/* skyline_loss_inflation_permille()'s ceiling: cap compensation at a measured
 * loss rate of 50% (1/(1-0.5) = 2.0x) regardless of how much higher the
 * flow's own EMA reads -- the target box's upper bound is 20% loss, and
 * retransmit overhead on top of that eats into the margin, so a lower
 * ceiling would leave too little headroom. An unbounded factor at
 * very high measured loss could still overshoot into self-inflicted
 * congestion, hence a ceiling at all. */
#define SKYLINE_LOSS_INFLATION_MAX_PERMILLE 500U
#define min_t(type, left, right) ((type)(left) < (type)(right) ? (type)(left) : (type)(right))
#define max_t(type, left, right) ((type)(left) > (type)(right) ? (type)(left) : (type)(right))

char LICENSE[] SEC("license") = "GPL";

extern __u32 tcp_slow_start(struct tcp_sock *tp, __u32 acked) __ksym;
extern void tcp_cong_avoid_ai(struct tcp_sock *tp, __u32 w, __u32 acked) __ksym;
extern __u32 tcp_reno_undo_cwnd(struct sock *sk) __ksym;

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, struct skyline_config);
} config_slots SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} active_config_slot SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_SK_STORAGE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int);
    __type(value, struct skyline_flow_state);
} flow_states SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_metrics);
} metrics SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u64);
} flow_count SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, SKYLINE_EVENT_RING_SIZE);
} events SEC(".maps");

static __always_inline struct tcp_sock *skyline_tcp_sk(struct sock *sk)
{
    return (struct tcp_sock *)sk;
}

static __always_inline struct inet_connection_sock *skyline_icsk(struct sock *sk)
{
    return (struct inet_connection_sock *)sk;
}

static __always_inline __u32 skyline_active_config_slot(void)
{
    __u32 key = 0;
    __u32 *slot = bpf_map_lookup_elem(&active_config_slot, &key);

    return slot ? *slot & 1U : 0;
}

static __always_inline const struct skyline_config *skyline_config_get_slot(__u32 slot)
{
    const struct skyline_config *config;

    slot &= 1U;
    config = bpf_map_lookup_elem(&config_slots, &slot);
    if (!config || config->abi_version != SKYLINE_ABI_VERSION)
        return 0;
    return config;
}

static __always_inline const struct skyline_config *skyline_config_get(void)
{
    return skyline_config_get_slot(skyline_active_config_slot());
}

static __always_inline struct skyline_metrics *skyline_metrics_get(void)
{
    __u32 key = 0;

    return bpf_map_lookup_elem(&metrics, &key);
}

static __always_inline struct skyline_flow_state *skyline_flow_get(struct sock *sk)
{
    return bpf_sk_storage_get(&flow_states, sk, 0,
                              BPF_SK_STORAGE_GET_F_CREATE);
}

static __always_inline struct skyline_flow_state *skyline_flow_lookup(struct sock *sk)
{
    return bpf_sk_storage_get(&flow_states, sk, 0, 0);
}

static __always_inline __u64 *skyline_flow_count_get(void)
{
    __u32 key = 0;

    return bpf_map_lookup_elem(&flow_count, &key);
}

static __always_inline void skyline_emit(struct sock *sk, __u32 type, __u32 state,
                                     __u64 value_a, __u64 value_b)
{
    struct skyline_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);

    if (!event)
        return;
    event->timestamp_ns = bpf_ktime_get_ns();
    /* bpf_get_socket_cookie() is not available to struct_ops programs. */
    event->socket_cookie =
        (__u64)BPF_CORE_READ(sk, __sk_common.skc_cookie.counter);
    event->type = type;
    event->state = state;
    event->value_a = value_a;
    event->value_b = value_b;
    bpf_ringbuf_submit(event, 0);
}

static __noinline __u64 skyline_cube_root(__u64 value)
{
    __u64 low = 0;
    /* The caller caps the input to 2.5e15; ten steps keep verifier
     * complexity bounded while resolving K to about 0.13 ms.
     */
    __u64 high = 135722ULL;

#pragma clang loop unroll(disable)
    for (int index = 0; index < 10; index++) {
        __u64 middle = low + ((high - low) >> 1);
        __u64 square = middle * middle;
        __u64 cube = square * middle;

        if (cube <= value)
            low = middle + 1;
        else
            high = middle;
    }
    return low ? low - 1 : 0;
}

/* Real CUBIC's growth curve -- only exercised on the M2-off path (B1/B2
 * neutrality), see skyline_grow_cwnd().
 */
static __always_inline __u32 skyline_cubic_target(struct skyline_flow_state *flow,
                                               __u32 cwnd, __u64 now_ns)
{
    __u64 elapsed_ms;
    __u64 difference;
    __u64 offset;
    __u64 cubic_delta;

    if (!flow->epoch_start_ns) {
        flow->epoch_start_ns = now_ns;
        if (!flow->w_max)
            flow->w_max = cwnd;
        /* K (the time constant for reaching w_max on the cubic curve) is
         * computed ONCE here, from the cwnd at the START of this growth
         * epoch, and held fixed for the epoch's entire duration -- mirrors
         * real CUBIC's bictcp_update() (net/ipv4/tcp_cubic.c), which
         * computes ca->bic_K only when ca->epoch_start == 0 and never
         * touches it again until the next reduction event. Recomputing the
         * equivalent of K from the live (continuously growing) cwnd on
         * every call instead is a runaway, self-reinforcing overshoot (the
         * concave growing-from-below-toward-w_max phase flips to the convex
         * target-above-w_max phase far earlier than real elapsed time would
         * justify.
         */
        difference = flow->w_max > cwnd ? flow->w_max - cwnd : 0;
        difference = min_t(__u64, difference, 1000000ULL);
        flow->epoch_k_ms = (__u32)skyline_cube_root(difference * 2500000000ULL);
    }

    elapsed_ms = (now_ns - flow->epoch_start_ns) / 1000000ULL;
    offset = elapsed_ms > flow->epoch_k_ms
                 ? elapsed_ms - flow->epoch_k_ms
                 : flow->epoch_k_ms - elapsed_ms;
    if (offset > 100000ULL)
        offset = 100000ULL;
    cubic_delta = SKYLINE_CUBIC_C_PERMILLE * offset * offset * offset;
    cubic_delta /= 1000000000000ULL;

    if (elapsed_ms < flow->epoch_k_ms)
        return flow->w_max > cubic_delta ? flow->w_max - cubic_delta : SKYLINE_MIN_CWND;
    return flow->w_max + cubic_delta;
}

static __always_inline bool skyline_new_round(struct tcp_sock *tp,
                                          struct skyline_flow_state *flow,
                                          const struct rate_sample *sample)
{
    if ((__u64)sample->prior_delivered < flow->next_round_delivered)
        return false;
    flow->next_round_delivered = tp->delivered + tp->packets_out;
    flow->round_count++;
    return true;
}

static __always_inline __u64 skyline_rate_sample_bps(struct tcp_sock *tp,
                                                 const struct rate_sample *sample)
{
    __u64 bytes;
    __u64 interval;

    if (sample->delivered <= 0 || sample->interval_us <= 0)
        return 0;
    bytes = (__u64)sample->delivered * max_t(__u32, tp->mss_cache, 1U);
    interval = sample->interval_us;
    return bytes * 8ULL * USEC_PER_SEC / interval;
}

static __always_inline void skyline_reset_generation(struct skyline_flow_state *flow,
                                                  const struct skyline_config *config,
                                                  __u32 config_slot,
                                                  struct tcp_sock *tp)
{
    flow->generation = config->generation;
    flow->config_slot = config_slot & 1U;
    flow->mode = SKYLINE_MODE_STARTUP;
    flow->round_count = 0;
    flow->plateau_rounds = 0;
    flow->prior_cwnd = tp->snd_cwnd;
    flow->w_max = tp->snd_cwnd;
    flow->last_delivered_ce = tp->delivered_ce;
    flow->last_loss_delivered = tp->delivered;
    flow->min_rtt_us = 0;
    flow->min_rtt_stamp_us = 0;
    flow->min_srtt_us = 0;
    flow->min_srtt_stamp_us = 0;
    flow->bw_bps = 0;
    flow->prior_round_bw_bps = 0;
    flow->round_rate_bps = 0;
    flow->bw_sample_index = 0;
#pragma unroll
    for (int index = 0; index < 10; index++)
        flow->bw_samples[index] = 0;
    flow->next_round_delivered = tp->delivered;
    flow->epoch_start_ns = bpf_ktime_get_ns();
    /* w_max == cwnd at this point (both just set above), so the correct
     * frozen K for this instant is 0 (see skyline_cubic_target()'s doc
     * comment) -- keeps epoch_k_ms consistent with the reset w_max instead
     * of carrying over a stale value from before this reset.
     */
    flow->epoch_k_ms = 0;
    flow->prr_delivered = 0;
    flow->prr_out = 0;
    flow->tcp_cwnd = tp->snd_cwnd;
    flow->ack_cnt = 0;
    flow->recovery_active = 0;
    flow->loss_rate_permille = 0;
    flow->last_lost = tp->lost;
    flow->last_loss_rate_delivered = tp->delivered;
    flow->queue_clamped = 0;
}

static __always_inline void skyline_transition(struct sock *sk,
                                           struct skyline_flow_state *flow,
                                           __u32 next_mode)
{
    struct skyline_metrics *metric;

    if (flow->mode == next_mode)
        return;
    skyline_emit(sk, SKYLINE_EVENT_STATE, next_mode, flow->mode, next_mode);
    flow->mode = next_mode;
    metric = skyline_metrics_get();
    if (metric)
        metric->state_transitions++;
}

/* The robust "no queueing" baseline for a flow -- deliberately max(), not
 * min(), of min_rtt_us and min_srtt_us. Under netem reordering a fraction of
 * packets legitimately bypass the emulated delay (they are not retransmits,
 * so rate_sample.is_retrans filtering would not help), so a raw per-packet
 * RTT sample genuinely collapses toward 0 -- and with it, flow->min_rtt_us.
 * tp->srtt_us is a 1/8-scaled EWMA and settles near
 * (1 - reorder_fraction) * base_delay instead, so its windowed minimum
 * (flow->min_srtt_us, maintained in skyline_update_model() the same way as
 * min_rtt_us) stays a meaningful baseline through the same reordering.
 * Taking the max of the two means this can only ever RAISE the baseline
 * relative to using min_rtt_us alone -- i.e. only ever shrink the computed
 * queue delay and grow the BDP target (skyline_bdp_packets()) -- both
 * anti-collapse directions, never a new way to overshoot.
 */
static __always_inline __u64 skyline_base_rtt_us(struct skyline_flow_state *flow)
{
    return max_t(__u64, flow->min_rtt_us, flow->min_srtt_us);
}

/* Floor + shared definition for every "queue_delay <= (basically empty)"
 * exit check. At rtt=2ms, base_rtt/8 alone would be 250us -- below
 * scheduler/NIC timing noise -- hence the absolute floor.
 */
static __always_inline __u64 skyline_low_queue_us(struct skyline_flow_state *flow)
{
    return max_t(__u64, skyline_base_rtt_us(flow) / 8U, SKYLINE_QUEUE_LOW_FLOOR_US);
}

/* The queue-delay guardrail threshold, as max(fixed floor, ratio-of-base-RTT)
 * -- config->max_queue_delay_permille == 0 (the default) returns exactly
 * max_queue_delay_us unchanged. A deliberately-set ratio lets the guardrail
 * widen automatically as base RTT grows, so cruise_inflight_gain/
 * cruise_pacing_gain can be tuned more aggressively on high-RTT paths
 * without the guardrail tripping purely from the RTT being larger.
 */
static __always_inline __u64 skyline_max_queue_delay_us(struct skyline_flow_state *flow,
                                                     const struct skyline_config *config)
{
    __u64 relative;

    if (!config->max_queue_delay_permille)
        return config->max_queue_delay_us;
    relative = skyline_base_rtt_us(flow) * config->max_queue_delay_permille / 1000U;
    return max_t(__u64, config->max_queue_delay_us, relative);
}

static __always_inline __u64 skyline_queue_delay_us(struct tcp_sock *tp,
                                                struct skyline_flow_state *flow)
{
    __u64 srtt = tp->srtt_us >> 3;
    __u64 base = skyline_base_rtt_us(flow);

    return srtt > base ? srtt - base : 0;
}

/* M3's role: compensate every BDP-derived cwnd target and the pacing rate
 * for bw_bps's systematic under-provisioning at this flow's own measured
 * loss rate p.
 * bw_bps is a *delivered*-rate max filter (see skyline_update_model()) -- on a
 * path with intrinsic loss p, sustaining delivered rate B requires holding
 * roughly BDP/(1-p) in flight, so every target computed straight from
 * bw_bps*base_rtt undershoots by exactly (1-p). Returns 1000 (neutral, 1.0x)
 * whenever M3 is off or loss_inflation_max_permille is unset (0, the
 * default) -- a strict identity everywhere this isn't explicitly opted into.
 */
static __always_inline __u32 skyline_loss_inflation_permille(struct skyline_flow_state *flow,
                                                          const struct skyline_config *config)
{
    __u32 p;

    if (!(config->feature_mask & SKYLINE_FEATURE_LOSS_CLASSIFIER))
        return 1000U;
    if (!config->loss_inflation_max_permille)
        return 1000U;
    p = min_t(__u32, flow->loss_rate_permille, config->loss_inflation_max_permille);
    p = min_t(__u32, p, SKYLINE_LOSS_INFLATION_MAX_PERMILLE);
    return 1000U * 1000U / (1000U - p);
}

static __always_inline bool skyline_update_model(struct sock *sk,
                                             struct tcp_sock *tp,
                                             struct skyline_flow_state *flow,
                                             const struct skyline_config *config,
                                             const struct rate_sample *sample)
{
    __u64 now_us = bpf_ktime_get_ns() / 1000ULL;
    __u64 rate_bps = skyline_rate_sample_bps(tp, sample);
    bool new_round = skyline_new_round(tp, flow, sample);

    if (sample->rtt_us > 0 &&
        (!flow->min_rtt_us || (__u64)sample->rtt_us < flow->min_rtt_us ||
         now_us - flow->min_rtt_stamp_us > config->min_rtt_window_us)) {
        flow->min_rtt_us = sample->rtt_us;
        flow->min_rtt_stamp_us = now_us;
    }
    /* Windowed minimum of srtt, aged the same way -- see skyline_base_rtt_us()'s
     * doc comment for why this (not min_rtt_us alone) is the robust choice
     * for the queue-delay/BDP baseline.
     */
    if (tp->srtt_us > 0) {
        __u64 srtt = tp->srtt_us >> 3;

        if (!flow->min_srtt_us || srtt < flow->min_srtt_us ||
            now_us - flow->min_srtt_stamp_us > config->min_rtt_window_us) {
            flow->min_srtt_us = srtt;
            flow->min_srtt_stamp_us = now_us;
        }
    }

    if (new_round && flow->round_rate_bps) {
        __u32 window = min_t(__u32, config->bw_window_rtts, 10U);
        __u64 maximum = 0;

#pragma unroll
        for (int offset = 9; offset > 0; offset--)
            flow->bw_samples[offset] = flow->bw_samples[offset - 1];
        flow->bw_samples[0] = flow->round_rate_bps;
        flow->bw_sample_index = 0;
        flow->round_rate_bps = 0;
#pragma unroll
        for (int offset = 0; offset < 10; offset++) {
            if ((__u32)offset < window &&
                flow->bw_samples[offset] > maximum)
                maximum = flow->bw_samples[offset];
        }
        flow->bw_bps = maximum;
    }
    if (rate_bps && !sample->is_app_limited) {
        if (rate_bps > flow->round_rate_bps)
            flow->round_rate_bps = rate_bps;
        if (rate_bps > flow->bw_bps)
            flow->bw_bps = rate_bps;
    }

    /* M3's role: per-flow loss-rate EMA (permille, alpha=1/4 -> ~4-round
     * time constant), the shared signal skyline_loss_inflation_permille() reads.
     * Deliberately computed unconditionally (not gated on any feature mask)
     * so a modules=[adaptive-cwnd] ablation profile (M3 off) still measures
     * the same underlying loss rate that a modules=[...,loss-classifier]
     * profile would report -- each mechanism gates its own *use* of the
     * signal instead (see skyline_loss_inflation_permille()'s own feature
     * check). tp->lost counts every segment ever marked lost, including a
     * retransmit that is itself later re-lost; tp->delivered is the
     * cumulative delivered-segment counter both already read elsewhere in
     * this function.
     */
    if (new_round) {
        __u32 lost_delta = tp->lost - flow->last_lost;
        __u32 delivered_delta = tp->delivered - flow->last_loss_rate_delivered;
        __u32 total = lost_delta + delivered_delta;
        __u32 sample_permille = total ? 1000U * lost_delta / total : 0U;

        flow->loss_rate_permille = (flow->loss_rate_permille * 3U + sample_permille) / 4U;
        flow->last_lost = tp->lost;
        flow->last_loss_rate_delivered = tp->delivered;
    }

    if (new_round && (config->feature_mask & SKYLINE_FEATURE_ADAPTIVE_CWND)) {
        struct skyline_metrics *metric = skyline_metrics_get();
        __u64 queue_delay = skyline_queue_delay_us(tp, flow);
        __u64 max_queue_delay = skyline_max_queue_delay_us(flow, config);
        bool fresh_ce = tp->delivered_ce != flow->last_delivered_ce;

        /* One-shot-per-round clamp (recomputed fresh every round), not a
         * sticky mode -- see flow->queue_clamped's doc comment in
         * skyline_abi.h.
         */
        flow->queue_clamped = (max_queue_delay && queue_delay > max_queue_delay) || fresh_ce;
        if (flow->queue_clamped) {
            if (metric)
                metric->guardrail_hits++;
            skyline_emit(sk, SKYLINE_EVENT_GUARDRAIL, flow->mode, queue_delay,
                     max_queue_delay);
        }

        /* Only two modes (see skyline_abi.h's enum skyline_mode doc comment) --
         * STARTUP is the only transient state; once it exits to CRUISE
         * there is nothing left to check here every round.
         */
        if (flow->mode == SKYLINE_MODE_STARTUP) {
            if (flow->prior_round_bw_bps &&
                flow->bw_bps * 1000ULL <
                    flow->prior_round_bw_bps *
                        (1000U + config->startup_growth_permille))
                flow->plateau_rounds++;
            else
                flow->plateau_rounds = 0;
            if (flow->plateau_rounds >= config->startup_plateau_rtts ||
                flow->queue_clamped)
                skyline_transition(sk, flow, SKYLINE_MODE_CRUISE);
        }
        flow->prior_round_bw_bps = flow->bw_bps;
        flow->last_delivered_ce = tp->delivered_ce;
    }
    return new_round;
}

/* M2's cwnd floor -- config->min_cwnd_packets, never below SKYLINE_MIN_CWND
 * (see skyline_abi.h's min_cwnd_packets doc comment for why a BDP-derived
 * target alone is too small for thin flows on a lossy path). The max() is
 * not just defensive: it is what keeps this floor from ever dropping under
 * the one the M2-off path uses, whatever userspace wrote. M2-on path only --
 * skyline_grow_cwnd()/skyline_apply_prr()/skyline_set_state() keep the fixed
 * SKYLINE_MIN_CWND so this knob cannot perturb B1/B2 neutrality.
 */
static __always_inline __u32 skyline_min_cwnd(const struct skyline_config *config)
{
    return max_t(__u32, config->min_cwnd_packets, SKYLINE_MIN_CWND);
}

static __always_inline __u32 skyline_bdp_packets(struct tcp_sock *tp,
                                             struct skyline_flow_state *flow,
                                             const struct skyline_config *config,
                                             __u32 gain_permille)
{
    __u64 bytes;
    __u64 packets;
    __u64 base_rtt;
    __u32 inflation;

    if (!flow->bw_bps || !flow->min_rtt_us)
        return tp->snd_cwnd;
    base_rtt = skyline_base_rtt_us(flow);
    bytes = flow->bw_bps * base_rtt;
    bytes /= 8ULL * USEC_PER_SEC;
    bytes = bytes * gain_permille / 1000U;
    /* M3's role: see skyline_loss_inflation_permille()'s doc comment --
     * identity (1000/1000) outside the feature/regime this compensates for.
     */
    inflation = skyline_loss_inflation_permille(flow, config);
    bytes = bytes * inflation / 1000U;
    packets = bytes / max_t(__u32, tp->mss_cache, 1U);
    return max_t(__u32, packets, skyline_min_cwnd(config));
}

/* SKYLINE_FEATURE_AUTO_PACING: kernel-equivalent pacing-rate ceiling for when
 * M4 (SKYLINE_FEATURE_PACING) is off, ported from tcp_update_pacing_rate()
 * (net/ipv4/tcp_input.c) -- see skyline_abi.h's SKYLINE_FEATURE_AUTO_PACING doc
 * comment for why this exists. Deliberately mirrors the kernel's exact
 * integer arithmetic (same constants, same order of operations) rather
 * than a simplified re-derivation, to avoid introducing a *different*
 * fidelity bug while fixing this one.
 */
static __always_inline void skyline_apply_auto_pacing_rate(struct sock *sk, struct tcp_sock *tp)
{
    __u64 rate;
    __u32 ratio;
    __u32 basis;

    if (!tp->srtt_us)
        return;
    /* Slow Start: cwnd < ssthresh/2 (approaching-end-of-slow-start still
     * uses the steeper 200% ratio, per the kernel's own comment) ->
     * 200%; Congestion Avoidance -> 120%. Mirrors
     * net_ipv4_sysctl_tcp_pacing_ss_ratio/_ca_ratio's compiled-in
     * defaults (200/120) -- not exposed as separate Skyline Speeder config knobs
     * since this path only exists to match kernel behavior, not to be an
     * independently-tunable Skyline Speeder feature.
     */
    ratio = (tp->snd_cwnd < tp->snd_ssthresh / 2) ? 200U : 120U;
    basis = max_t(__u32, tp->snd_cwnd, tp->packets_out);
    rate = (__u64)max_t(__u32, tp->mss_cache, 1U) * 80000ULL * (__u64)ratio * (__u64)basis;
    rate /= tp->srtt_us;
    if (rate > sk->sk_max_pacing_rate)
        rate = sk->sk_max_pacing_rate;
    sk->sk_pacing_status = SK_PACING_NEEDED;
    sk->sk_pacing_rate = rate;
}

static __always_inline void skyline_apply_pacing(struct sock *sk,
                                             struct tcp_sock *tp,
                                             struct skyline_flow_state *flow,
                                             const struct skyline_config *config)
{
    __u32 gain = config->cruise_pacing_permille;
    __u64 rate;
    struct skyline_metrics *metric;

    if (!(config->feature_mask & SKYLINE_FEATURE_PACING)) {
        if (config->feature_mask & SKYLINE_FEATURE_AUTO_PACING) {
            skyline_apply_auto_pacing_rate(sk, tp);
        } else {
            sk->sk_pacing_rate = flow->neutral_pacing_rate;
            sk->sk_pacing_status = flow->neutral_pacing_status;
        }
        return;
    }
    if (!flow->bw_bps)
        return;
    sk->sk_pacing_status = SK_PACING_NEEDED;
    if ((config->feature_mask & SKYLINE_FEATURE_ADAPTIVE_CWND) &&
        flow->mode == SKYLINE_MODE_STARTUP)
        gain = config->startup_gain_permille;

    /* M3's role: compensate the gain for bw_bps's systematic under-
     * provisioning at this flow's measured loss rate -- see
     * skyline_loss_inflation_permille()'s doc comment. Identity (1000/1000)
     * outside M3/unset.
     */
    gain = gain * skyline_loss_inflation_permille(flow, config) / 1000U;
    /* Guardrail wins: a tripped queue-delay/ECN clamp overrides both the
     * mode gain above and the loss-inflation compensation just applied --
     * see flow->queue_clamped's doc comment in skyline_abi.h. 0 (unset) keeps
     * this at neutral (1.0x, cancels the boost but does not actively cut);
     * a configured value below 1000 makes it a real reduction.
     */
    if (flow->queue_clamped)
        gain = config->guardrail_gain_permille ? config->guardrail_gain_permille : 1000U;

    rate = flow->bw_bps * gain / 1000U;
    if (config->max_pacing_bps && rate > config->max_pacing_bps)
        rate = config->max_pacing_bps;
    rate /= 8ULL;
    if (rate > sk->sk_max_pacing_rate)
        rate = sk->sk_max_pacing_rate;
    sk->sk_pacing_rate = rate;
    metric = skyline_metrics_get();
    if (metric)
        metric->pacing_updates++;
}

/* M2's cwnd owner in every CA state (see skyline_cong_control()) -- direct
 * BDP-target assignment, no AIMD stepping, mirroring how BBR itself assigns
 * cwnd from its own target every round (bbr_set_cwnd(), net/ipv4/tcp_bbr.c)
 * rather than converging toward it via additive increase. This is what lets
 * M2 keep driving cwnd straight through Recovery/CWR instead of falling back
 * to PRR's packet-conservation formula, which would otherwise throttle
 * throughput hard in the sustained-loss target regime (see this file's
 * top-of-file comment).
 */
static __always_inline void skyline_set_cwnd_target(struct tcp_sock *tp,
                                                struct skyline_flow_state *flow,
                                                const struct skyline_config *config,
                                                __u32 acked)
{
    __u32 gain;
    __u32 target;

    if (!flow->bw_bps || !flow->min_rtt_us) {
        /* No bandwidth estimate yet (start of connection) -- skyline_bdp_packets()
         * would just return tp->snd_cwnd unchanged in this state, so fall
         * back to standard slow start to make some progress instead of
         * freezing until the first bw sample lands.
         */
        if (acked)
            tcp_slow_start(tp, acked);
        return;
    }
    gain = flow->mode == SKYLINE_MODE_STARTUP ? config->startup_gain_permille
                                          : config->cruise_inflight_permille;
    /* Guardrail wins: see skyline_apply_pacing()'s matching comment. */
    if (flow->queue_clamped)
        gain = config->guardrail_gain_permille ? config->guardrail_gain_permille : 1000U;
    target = skyline_bdp_packets(tp, flow, config, gain);
    tp->snd_cwnd = target;
    if (config->max_cwnd_packets && tp->snd_cwnd > config->max_cwnd_packets)
        tp->snd_cwnd = config->max_cwnd_packets;
    /* Floor last, so it holds even against the guardrail's gain clamp above
     * -- same ordering the fixed SKYLINE_MIN_CWND floor always had here.
     * Userspace validation keeps min_cwnd_packets <= max_cwnd_packets, so
     * this can never undo the cap just applied.
     */
    if (tp->snd_cwnd < skyline_min_cwnd(config))
        tp->snd_cwnd = skyline_min_cwnd(config);
}

/* M2-off path only (B1/B2 neutrality) -- real CUBIC's growth curve plus its
 * TCP-friendliness Reno floor. Never called with M2 on (see
 * skyline_cong_control(): M2 on routes through skyline_set_cwnd_target() instead).
 */
static __always_inline void skyline_grow_cwnd(struct tcp_sock *tp,
                                          struct skyline_flow_state *flow,
                                          const struct skyline_config *config,
                                          __u32 acked)
{
    __u32 target;
    __u32 weight;
    bool fresh_epoch;

    if (!acked)
        return;
    if (tp->snd_cwnd < tp->snd_ssthresh) {
        acked = tcp_slow_start(tp, acked);
        if (!acked) {
            /* tcp_slow_start() can push snd_cwnd above max_cwnd_packets --
             * this early return must not skip the cap enforcement the
             * non-slow-start path below already applies.
             */
            if (config->max_cwnd_packets && tp->snd_cwnd > config->max_cwnd_packets)
                tp->snd_cwnd = config->max_cwnd_packets;
            return;
        }
    }

    fresh_epoch = !flow->epoch_start_ns;
    target = skyline_cubic_target(flow, tp->snd_cwnd, bpf_ktime_get_ns());
    /* TCP-friendliness Reno floor -- mirrors real CUBIC's bictcp_update()
     * tcp_friendliness branch (net/ipv4/tcp_cubic.c: 219,237-238,299-316),
     * applied unconditionally there (no config knob; tcp_friendliness
     * defaults on and nothing in this codebase ever turns it off). Required
     * when congestion epochs are much shorter than the cubic curve's own
     * time constant K (frequent losses at very low RTT), where cubic_delta
     * stays ~0 for the epoch's entire lifetime and the "stalled" branch's
     * near-freeze below would otherwise never let up.
     */
    if (fresh_epoch) {
        flow->tcp_cwnd = tp->snd_cwnd;
        flow->ack_cnt = acked;
    } else {
        flow->ack_cnt += acked;
    }
    {
        __u64 delta = (__u64)tp->snd_cwnd * 15ULL / 8ULL;

        if (delta) {
            __u32 incr = (__u32)(flow->ack_cnt / delta);

            if (incr) {
                flow->tcp_cwnd += incr;
                flow->ack_cnt -= incr * (__u32)delta;
            }
        }
    }

    if (target > tp->snd_cwnd) {
        weight = max_t(__u32, tp->snd_cwnd / (target - tp->snd_cwnd), 2U);
    } else {
        /* Target not yet ahead of cwnd -- real CUBIC's bictcp_update() sets
         * `ca->cnt = 100 * cwnd` here (net/ipv4/tcp_cubic.c:289), i.e. ~1
         * packet per 100 RTTs, an effective near-freeze until elapsed time
         * pushes the convex curve back above cwnd.
         */
        weight = max_t(__u32, tp->snd_cwnd * SKYLINE_CUBIC_STALLED_WEIGHT_SCALE, 2U);
    }
    if (flow->tcp_cwnd > tp->snd_cwnd) {
        __u32 friendly_gap = flow->tcp_cwnd - tp->snd_cwnd;
        __u32 friendly_weight = max_t(__u32, tp->snd_cwnd / friendly_gap, 1U);

        if (weight > friendly_weight)
            weight = friendly_weight;
    }
    tcp_cong_avoid_ai(tp, weight, acked);

    if (config->max_cwnd_packets && tp->snd_cwnd > config->max_cwnd_packets)
        tp->snd_cwnd = config->max_cwnd_packets;
    if (tp->snd_cwnd < SKYLINE_MIN_CWND)
        tp->snd_cwnd = SKYLINE_MIN_CWND;
}

/* RFC 6937 PRR, M2-off path only (B1/B2 neutrality) -- see
 * skyline_cong_control()'s doc comment for why M2 on bypasses this entirely.
 * cong_control makes the kernel skip its own PRR (tcp_cwnd_reduction()), so
 * this reimplements it: applies exactly the same continuous per-ACK rate
 * limiting throughout Recovery/CWR that a non-cong_control CC would get for
 * free, which is what makes a zero-module Skyline Speeder flow track real CUBIC's
 * lossy-scenario goodput instead of massively overshooting it.
 */
static __always_inline void skyline_apply_prr(struct tcp_sock *tp,
                                          struct skyline_flow_state *flow,
                                          const struct rate_sample *sample,
                                          struct skyline_metrics *metric)
{
    __s64 packets_in_flight;
    __s64 delta;
    __s64 sndcnt;
    __u32 newly_acked;
    __u32 new_cwnd;

    newly_acked = sample->acked_sacked > 0 ? (__u32)sample->acked_sacked : 0;
    if (!newly_acked || !flow->prior_cwnd)
        return;

    flow->prr_delivered += newly_acked;
    packets_in_flight = (__s64)tp->packets_out - (__s64)tp->sacked_out -
                        (__s64)tp->lost_out + (__s64)tp->retrans_out;
    if (packets_in_flight < 0)
        packets_in_flight = 0;

    delta = (__s64)tp->snd_ssthresh - packets_in_flight;
    if (delta < 0) {
        /* Below ssthresh already (in-flight has fallen further than the
         * target) -- pace new sends so in-flight converges toward ssthresh
         * proportionally to how much has been delivered this epoch (PRR
         * proper), rather than the SSRB branch below.
         */
        __u64 dividend = (__u64)tp->snd_ssthresh * (__u64)flow->prr_delivered +
                          (__u64)flow->prior_cwnd - 1ULL;

        sndcnt = (__s64)(dividend / (__u64)flow->prior_cwnd) - (__s64)flow->prr_out;
    } else {
        /* PRR-SSRB: still above ssthresh -- send at most enough to track
         * delivered packets 1:1 (plus one extra per ACK when nothing new
         * was lost this round, mirroring the kernel's own
         * FLAG_SND_UNA_ADVANCED-and-no-newly_lost bonus), capped at delta
         * so in-flight never overshoots ssthresh.
         */
        sndcnt = (__s64)flow->prr_delivered - (__s64)flow->prr_out;
        if (sndcnt < (__s64)newly_acked)
            sndcnt = (__s64)newly_acked;
        if (sample->losses <= 0)
            sndcnt++;
        if (sndcnt > delta)
            sndcnt = delta;
    }
    /* Force at least one segment out the first time (kickstarts fast
     * retransmit), matching tcp_cwnd_reduction()'s
     * `max(sndcnt, prr_out ? 0 : 1)`.
     */
    if (sndcnt < (flow->prr_out ? 0 : 1))
        sndcnt = flow->prr_out ? 0 : 1;
    if (sndcnt < 0)
        sndcnt = 0;

    flow->prr_out += (__u32)sndcnt;
    new_cwnd = (__u32)(packets_in_flight + sndcnt);
    tp->snd_cwnd = max_t(__u32, new_cwnd, SKYLINE_MIN_CWND);
    if (metric)
        metric->prr_adjustments++;
}

SEC("struct_ops")
void BPF_PROG(skyline_init, struct sock *sk)
{
    struct tcp_sock *tp = skyline_tcp_sk(sk);
    struct skyline_flow_state *flow = skyline_flow_get(sk);
    __u32 config_slot = skyline_active_config_slot();
    const struct skyline_config *config = skyline_config_get();
    __u64 *count;

    if (!flow || !config)
        return;
    flow->neutral_pacing_rate = sk->sk_pacing_rate;
    flow->neutral_pacing_status = sk->sk_pacing_status;
    /* Aggressive initial window, applied before skyline_reset_generation()
     * snapshots tp->snd_cwnd into prior_cwnd/w_max/tcp_cwnd below -- see
     * skyline_abi.h's initial_cwnd_packets doc comment. 0 = leave the kernel's
     * own IW alone.
     */
    if (config->initial_cwnd_packets)
        tp->snd_cwnd = config->initial_cwnd_packets;
    skyline_reset_generation(flow, config, config_slot, tp);
    if (!flow->counted) {
        count = skyline_flow_count_get();
        if (count)
            __sync_fetch_and_add(count, 1);
        flow->counted = 1;
    }
    tp->snd_ssthresh = 0x7fffffffU;
}

SEC("struct_ops")
void BPF_PROG(skyline_cong_control, struct sock *sk, __u32 ack, int flag,
              const struct rate_sample *sample)
{
    struct tcp_sock *tp = skyline_tcp_sk(sk);
    struct inet_connection_sock *icsk = skyline_icsk(sk);
    struct skyline_flow_state *flow = skyline_flow_get(sk);
    const struct skyline_config *config;
    __u32 active_slot = skyline_active_config_slot();
    struct skyline_metrics *metric;

    if (!flow || !sample)
        return;
    config = skyline_config_get_slot(flow->config_slot);
    if (!config)
        config = skyline_config_get();
    if (!config)
        return;
    if (flow->config_slot != active_slot &&
        (__u64)sample->prior_delivered >= flow->next_round_delivered) {
        const struct skyline_config *next_config =
            skyline_config_get_slot(active_slot);

        if (next_config) {
            __u32 old_generation = flow->generation;
            __u32 prior_mode = flow->mode;

            config = next_config;
            skyline_reset_generation(flow, config, active_slot, tp);
            skyline_emit(sk, SKYLINE_EVENT_GENERATION_SWITCH, prior_mode,
                     old_generation, next_config->generation);
        }
    }

    skyline_update_model(sk, tp, flow, config, sample);
    metric = skyline_metrics_get();

    /* Basic loss bookkeeping -- deliberately no classification (see this
     * file's top-of-file comment: the target regime's correct answer is
     * "never reduce for loss"). Deduplicated at most once per
     * delivered-count advance, so a burst of RACK marks within the same
     * round only counts once.
     */
    if (sample->losses > 0 && tp->delivered != flow->last_loss_delivered) {
        flow->last_loss_delivered = tp->delivered;
        if (metric) {
            metric->loss_events++;
            if (config->feature_mask & SKYLINE_FEATURE_EARLY_LOSS)
                metric->hypothetical_early_loss++;
        }
        skyline_emit(sk, SKYLINE_EVENT_LOSS, flow->mode, sample->losses, 0);
    }

    skyline_apply_pacing(sk, tp, flow, config);

    /* M2 owns cwnd directly in every CA state, bypassing PRR entirely --
     * see this file's top-of-file comment for why an
     * `icsk_ca_state < TCP_CA_CWR` gate would make the entire M2 cwnd-target
     * machinery unreachable at >=10% loss (the flow lives almost entirely
     * inside Recovery/CWR there). M2 off keeps the original CUBIC-parity/PRR
     * path unchanged (B1/B2 neutrality).
     */
    if (config->feature_mask & SKYLINE_FEATURE_ADAPTIVE_CWND) {
        skyline_set_cwnd_target(tp, flow, config, sample->acked_sacked);
    } else if (icsk->icsk_ca_state < TCP_CA_CWR) {
        skyline_grow_cwnd(tp, flow, config, sample->acked_sacked);
    } else if (config->feature_mask & SKYLINE_FEATURE_PRR) {
        skyline_apply_prr(tp, flow, sample, metric);
    }

    if (metric) {
        metric->ack_events++;
        metric->delivered_packets += max_t(int, sample->delivered, 0);
        if (config->max_cwnd_packets && tp->snd_cwnd >= config->max_cwnd_packets)
            metric->guardrail_hits++;
    }
}

SEC("struct_ops")
__u32 BPF_PROG(skyline_ssthresh, struct sock *sk)
{
    struct tcp_sock *tp = skyline_tcp_sk(sk);
    struct skyline_flow_state *flow = skyline_flow_get(sk);
    const struct skyline_config *config = flow
                                          ? skyline_config_get_slot(flow->config_slot)
                                          : skyline_config_get();

    /* No-flow/no-config fallback: the fixed CUBIC-beta cut (0.70*cwnd),
     * matching the M2-off path below -- neither `flow` nor `config` (both
     * required for the M2-on identity check) exist on this path.
     */
    if (!flow || !config)
        return max_t(__u32, tp->snd_cwnd * SKYLINE_CUBIC_BETA_PERMILLE / 1000U, 2U);
    flow->prior_cwnd = tp->snd_cwnd;
    flow->w_max = tp->snd_cwnd;
    flow->epoch_start_ns = 0;
    /* M2 on means "never reduce because of loss" -- the queue-delay/ECN
     * guardrail (flow->queue_clamped) is the only thing still allowed to
     * restrain the flow, and it acts through
     * skyline_set_cwnd_target()/skyline_apply_pacing()'s gain clamp, not through
     * ssthresh. M2 off keeps the exact CUBIC-beta cut (B1/B2 neutrality).
     */
    if (config->feature_mask & SKYLINE_FEATURE_ADAPTIVE_CWND)
        return max_t(__u32, tp->snd_cwnd, 2U);
    return max_t(__u32, tp->snd_cwnd * SKYLINE_CUBIC_BETA_PERMILLE / 1000U, 2U);
}

SEC("struct_ops")
__u32 BPF_PROG(skyline_undo_cwnd, struct sock *sk)
{
    struct skyline_flow_state *flow = skyline_flow_get(sk);
    __u32 reno = tcp_reno_undo_cwnd(sk);
    __u32 restored;

    if (!flow)
        return reno;
    restored = max_t(__u32, flow->prior_cwnd, reno);
    /* The kernel only calls .undo_cwnd() when it has decided a prior
     * reduction was spurious (typically proven by a later DSACK) and is
     * restoring cwnd -- this is the only place that happens, so every call
     * here is itself the signal, independent of whatever value ends up
     * being returned.
     */
    skyline_emit(sk, SKYLINE_EVENT_UNDO_CWND, flow->mode, flow->prior_cwnd, restored);
    return restored;
}

SEC("struct_ops")
void BPF_PROG(skyline_set_state, struct sock *sk, __u8 new_state)
{
    struct skyline_flow_state *flow = skyline_flow_get(sk);
    struct tcp_sock *tp;
    const struct skyline_config *config;

    if (!flow)
        return;
    if (new_state == TCP_CA_Loss) {
        /* RTO already happened: tcp_enter_loss() unconditionally forces
         * tp->snd_cwnd down to packets_in_flight+1 before this callback
         * even fires (independent of cong_control) -- nothing to do here
         * except track that a reduction episode is open, for the M2-off
         * restore-on-exit path below.
         */
        flow->recovery_active = 1;
        return;
    }
    if (new_state == TCP_CA_Recovery || new_state == TCP_CA_CWR) {
        /* (Re)start PRR accounting for this fresh epoch -- only exercised on
         * the M2-off path; M2 on bypasses PRR entirely (see
         * skyline_cong_control()'s doc comment).
         */
        flow->prr_delivered = 0;
        flow->prr_out = 0;
        flow->recovery_active = 1;
    } else if (new_state == TCP_CA_Open || new_state == TCP_CA_Disorder) {
        if (!flow->recovery_active)
            return;
        flow->recovery_active = 0;
        config = skyline_config_get_slot(flow->config_slot);
        if (!config)
            config = skyline_config_get();
        /* M2 on: cwnd is driven every ACK by skyline_set_cwnd_target()
         * regardless of CA state (see skyline_cong_control()) -- nothing to
         * restore here, the very next ACK already sets the right value.
         * M2 off: mirrors real CUBIC's tcp_end_cwnd_reduction(), which snaps
         * cwnd to ssthresh the moment PRR stops governing it -- required
         * for B1/B2 neutrality.
         */
        if (config && !(config->feature_mask & SKYLINE_FEATURE_ADAPTIVE_CWND)) {
            tp = skyline_tcp_sk(sk);
            tp->snd_cwnd = max_t(__u32, tp->snd_ssthresh, SKYLINE_MIN_CWND);
        }
    }
}

SEC("struct_ops")
void BPF_PROG(skyline_cwnd_event, struct sock *sk, enum tcp_ca_event event)
{
    struct skyline_flow_state *flow = skyline_flow_get(sk);

    if (!flow)
        return;
    if (event == CA_EVENT_TX_START)
        flow->epoch_start_ns = bpf_ktime_get_ns();
}

SEC("struct_ops")
void BPF_PROG(skyline_release, struct sock *sk)
{
    struct skyline_flow_state *flow = skyline_flow_lookup(sk);
    __u64 *count;

    if (!flow || !flow->counted)
        return;
    count = skyline_flow_count_get();
    if (count)
        __sync_fetch_and_sub(count, 1);
    flow->counted = 0;
}

SEC(".struct_ops")
struct tcp_congestion_ops skyline_cc = {
    .init = (void *)skyline_init,
    .release = (void *)skyline_release,
    .ssthresh = (void *)skyline_ssthresh,
    .undo_cwnd = (void *)skyline_undo_cwnd,
    .set_state = (void *)skyline_set_state,
    .cwnd_event = (void *)skyline_cwnd_event,
    .cong_control = (void *)skyline_cong_control,
    .name = "skyline_cc",
};
