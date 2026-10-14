// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include "skyline_abi.h"

#ifndef SOL_TCP
#define SOL_TCP 6
#endif
#ifndef AF_INET
#define AF_INET 2
#endif
#ifndef AF_INET6
#define AF_INET6 10
#endif
#ifndef TCP_CONGESTION
#define TCP_CONGESTION 13
#endif
#ifndef TCP_BPF_RTO_MIN
#define TCP_BPF_RTO_MIN 1004
#endif
#ifndef TCP_RTO_MAX_MS
#define TCP_RTO_MAX_MS 44
#endif

/* Kernel bounds on TCP_RTO_MAX_MS (net/ipv4/tcp.c): [MSEC_PER_SEC,
 * TCP_RTO_MAX_SEC * MSEC_PER_SEC]. Also what a flow's ceiling is reset to
 * when this feature is turned off out from under a live flow -- the kernel's
 * compiled-in default, not a value Skyline Speeder invented. Not necessarily
 * what the flow had before, though: a net.ipv4.tcp_rto_max_ms (6.15+), like
 * a tcp_rto_min_us (6.11+) for the floor, or a value the application set,
 * is not put back. (A route's locked rto_min is applied by the kernel at
 * use time and is never overridden in the first place.)
 */
#define SKYLINE_RTO_MAX_FLOOR_MS 1000U
#define SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS 120000U
/* Default queueing-evidence threshold when rto_max_congestion_ratio_permille
 * is left at 0: current srtt more than double the flow's min_rtt.
 */
#define SKYLINE_RTO_MAX_DEFAULT_CONGESTION_RATIO_PERMILLE 2000U

char LICENSE[] SEC("license") = "GPL";

/* Dual-stack: both AF_INET and native AF_INET6 flows are
 * accepted. Everything downstream of this gate (TCP_CONGESTION setsockopt,
 * BPF_SOCK_OPS_RTT_CB subscription, TCP_BPF_RTO_MIN/TCP_RTO_MAX_MS tuning)
 * is TCP-layer state that lives in the same struct tcp_sock for v4 and v6
 * sockets alike (a TCPv6 socket's struct tcp6_sock has struct tcp_sock as
 * its first member) -- so accepting AF_INET6 here needed no changes to any
 * of the code it gates.
 *
 * Kept as a named, documented function rather than inlining `return 1` at
 * the call site: a listening socket that doesn't bind an explicit address
 * family (iperf3's default, and many other servers) is dual-stack, and the
 * kernel reports incoming IPv4 connections on it as AF_INET6 with an
 * IPv4-mapped remote address (::ffff:a.b.c.d), not as AF_INET. A bare
 * `ops->family != AF_INET` check would silently discard every such
 * connection, making BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB appear to never
 * fire at all for a large share of real deployments -- so this gate
 * accepts both families explicitly rather than narrowing on just one.
 */
static __always_inline int skyline_is_supported_family(const struct bpf_sock_ops *ops)
{
    return ops->family == AF_INET || ops->family == AF_INET6;
}

/* CC selection switch: whether a new connection should be handed off to
 * skyline_cc. Independent of rack_rto below -- one profile can run stock/
 * controlled cubic while RTO tuning is still active, and vice versa.
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} policy_enabled SEC(".maps");

/* M1 tier-2 tuning. A single array-map slot, fully overwritten on each
 * update from user space -- see the comment on struct skyline_rto_tuning
 * (bpf/include/skyline_abi.h) for why this deliberately does not reuse
 * skyline_cc.bpf.c's double-buffered config_slots.
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_rto_tuning);
} rto_tuning SEC(".maps");

/* Per-flow debounce state: how many RTT samples this flow has seen, and
 * what rto_min/rto_max value was last applied to it (so an unchanged target
 * is never re-written). last_delivered_ce lets skyline_update_rto_max() detect a
 * *fresh* CE mark (delta since the last RTT_CB on this flow) the same way
 * skyline_cc.bpf.c does for its own guardrail, without sharing state with it --
 * the two modules stay independently toggleable.
 */
struct skyline_rto_flow {
    __u32 samples;
    __u32 applied_us;
    __u32 applied_rto_max_ms; /* 0 = kernel default, never touched */
    __u32 last_delivered_ce;
};

struct {
    __uint(type, BPF_MAP_TYPE_SK_STORAGE);
    __uint(map_flags, BPF_F_NO_PREALLOC);
    __type(key, int);
    __type(value, struct skyline_rto_flow);
} rto_flows SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_rto_stats);
} rto_stats SEC(".maps");

/* The slot, unless skyline-speederd wrote it in another layout: then none,
 * as skyline_cc.bpf.c and skyline_tc.bpf.c treat a mismatched slot of
 * theirs, rather than tuning RTOs from a misread struct -- a subscribed flow
 * is unsubscribed on its next RTT sample, as when the feature is off.
 */
static __always_inline struct skyline_rto_tuning *skyline_rto_config(void)
{
    __u32 key = 0;
    struct skyline_rto_tuning *tuning = bpf_map_lookup_elem(&rto_tuning, &key);

    if (tuning && tuning->abi_version != SKYLINE_RTO_TUNING_ABI_VERSION)
        return NULL;
    return tuning;
}

static __always_inline struct skyline_rto_stats *skyline_rto_stats_get(void)
{
    __u32 key = 0;

    return bpf_map_lookup_elem(&rto_stats, &key);
}

static __always_inline void skyline_rto_unsubscribe(struct bpf_sock_ops *ops)
{
    int rto_min_value = (int)SKYLINE_RTO_MIN_KERNEL_DEFAULT_US;
    int rto_max_value = (int)SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS;

    bpf_sock_ops_cb_flags_set(ops,
        ops->bpf_sock_ops_cb_flags & ~BPF_SOCK_OPS_RTT_CB_FLAG);
    bpf_setsockopt(ops, SOL_TCP, TCP_BPF_RTO_MIN, &rto_min_value, sizeof(rto_min_value));
    bpf_setsockopt(ops, SOL_TCP, TCP_RTO_MAX_MS, &rto_max_value, sizeof(rto_max_value));
}

/* M1 tier-2, floor half: target = max(srtt_us, min_rtt_us) * srtt_permille
 * / 1000, clamped into [floor_us, min(ceiling_us, 200000)].
 */
static __always_inline void skyline_update_rto_min(struct bpf_sock_ops *ops,
                                               const struct skyline_rto_tuning *tuning,
                                               struct skyline_rto_stats *stats,
                                               struct skyline_rto_flow *flow,
                                               __u32 base)
{
    __u32 ceiling, target;
    int value;
    long ret;

    target = (__u32)(((__u64)base * tuning->srtt_permille) / 1000);
    ceiling = tuning->ceiling_us < SKYLINE_RTO_MIN_KERNEL_DEFAULT_US
                  ? tuning->ceiling_us
                  : SKYLINE_RTO_MIN_KERNEL_DEFAULT_US;
    if (tuning->floor_us > ceiling)
        return; /* self-contradictory config; leave the flow alone */
    if (target < tuning->floor_us)
        target = tuning->floor_us;
    if (target > ceiling)
        target = ceiling;

    if (flow->applied_us == target) {
        if (stats)
            stats->unchanged++;
        return;
    }

    value = (int)target;
    ret = bpf_setsockopt(ops, SOL_TCP, TCP_BPF_RTO_MIN, &value, sizeof(value));
    if (ret) {
        if (stats)
            stats->rejected++;
        return;
    }
    flow->applied_us = target;
    if (stats)
        stats->applied++;
}

/* M1 tier-2, ceiling half: TCP_RTO_MAX_MS = clamp(k * base_us / 1000, 1000,
 * 120000) ms. k is rto_max_congested_permille once "real congestion
 * evidence" is seen on this flow -- a fresh CE mark (delivered_ce advanced
 * since the last RTT_CB), or the current srtt exceeding min_rtt by more
 * than the configured ratio (queueing) -- else rto_max_normal_permille.
 * There is deliberately no branch that reverts to the kernel's 120s
 * default once evidence appears; congestion widens the ceiling via a
 * larger k on the same base_us, it never disconnects the ceiling from it.
 */
static __always_inline void skyline_update_rto_max(struct bpf_sock_ops *ops,
                                               const struct skyline_rto_tuning *tuning,
                                               struct skyline_rto_stats *stats,
                                               struct skyline_rto_flow *flow,
                                               struct bpf_tcp_sock *tcp_sock,
                                               __u32 srtt_us, __u32 min_rtt_us, __u32 base)
{
    __u32 ratio_permille, k, target_ms;
    __u32 delivered_ce = tcp_sock->delivered_ce;
    int fresh_ce = (delivered_ce != flow->last_delivered_ce);
    int queueing;
    int value;
    long ret;

    flow->last_delivered_ce = delivered_ce;

    if (!tuning->rto_max_normal_permille) {
        /* Feature off. If a prior config had already raised this flow's
         * ceiling, put it back -- there will be no further RTT_CB on this
         * flow to notice a live re-disable otherwise.
         */
        if (flow->applied_rto_max_ms) {
            value = (int)SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS;
            if (!bpf_setsockopt(ops, SOL_TCP, TCP_RTO_MAX_MS, &value, sizeof(value)))
                flow->applied_rto_max_ms = 0;
        }
        return;
    }

    ratio_permille = tuning->rto_max_congestion_ratio_permille
                          ? tuning->rto_max_congestion_ratio_permille
                          : SKYLINE_RTO_MAX_DEFAULT_CONGESTION_RATIO_PERMILLE;
    queueing = min_rtt_us &&
               (__u64)srtt_us * 1000 > (__u64)min_rtt_us * ratio_permille;

    k = (fresh_ce || queueing) ? tuning->rto_max_congested_permille
                                : tuning->rto_max_normal_permille;
    if (fresh_ce || queueing) {
        if (stats)
            stats->rto_max_congested++;
    }

    target_ms = (__u32)(((__u64)base * k) / 1000 / 1000);
    if (target_ms < SKYLINE_RTO_MAX_FLOOR_MS)
        target_ms = SKYLINE_RTO_MAX_FLOOR_MS;
    if (target_ms > SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS)
        target_ms = SKYLINE_RTO_MAX_KERNEL_DEFAULT_MS;

    if (flow->applied_rto_max_ms == target_ms) {
        if (stats)
            stats->rto_max_unchanged++;
        return;
    }

    value = (int)target_ms;
    ret = bpf_setsockopt(ops, SOL_TCP, TCP_RTO_MAX_MS, &value, sizeof(value));
    if (ret) {
        if (stats)
            stats->rto_max_rejected++;
        return;
    }
    flow->applied_rto_max_ms = target_ms;
    if (stats)
        stats->rto_max_applied++;
}

/* Core of M1 tier-2: called on every RTT sample of a subscribed flow --
 * most of its ACKs, not once per round trip, which is also what
 * warmup_samples counts. Resolves the shared per-flow state once, then
 * drives the floor (TCP_BPF_RTO_MIN) and ceiling (TCP_RTO_MAX_MS) halves
 * independently.
 */
static __always_inline void skyline_update_rto_tuning(struct bpf_sock_ops *ops,
                                                   const struct skyline_rto_tuning *tuning)
{
    struct skyline_rto_stats *stats = skyline_rto_stats_get();
    struct skyline_rto_flow *flow;
    struct bpf_tcp_sock *tcp_sock;
    __u32 srtt_us, min_rtt_us, base;

    if (!tuning || !tuning->enabled) {
        skyline_rto_unsubscribe(ops);
        return;
    }
    if (stats)
        stats->rtt_callbacks++;

    /* ops->args[1] is the freshly computed srtt (<<3 fixed point), set by
     * tcp_bpf_rtt() before tp->srtt_us itself is written -- ops->srtt_us
     * would read the *previous* value (or 0 on the very first sample).
     */
    srtt_us = ops->args[1] >> 3;
    min_rtt_us = ops->rtt_min;
    if (min_rtt_us == (__u32)~0U)
        min_rtt_us = 0;
    base = srtt_us > min_rtt_us ? srtt_us : min_rtt_us;
    if (!base)
        return;

    {
        /* Read ops->sk exactly once: the verifier treats each read of a
         * context field as an independent value, so narrowing this one
         * null check only holds for the same register it was checked on --
         * a second, separate `ops->sk` read (e.g. inside bpf_tcp_sock()
         * below) would come back untyped/possibly-null again.
         */
        struct bpf_sock *sk = ops->sk;

        if (!ops->is_fullsock || !sk)
            return;
        /* bpf_sk_storage_get() requires a sock/tcp_sock/sock_common-typed
         * pointer; struct bpf_sock * is a narrower type the verifier
         * rejects directly, so it must go through bpf_tcp_sock() first.
         * This is family-agnostic: bpf_tcp_sock() only checks fullsock +
         * sk_protocol == IPPROTO_TCP, and a TCPv6 socket's tcp6_sock has
         * struct tcp_sock as its first member, so the same call works for
         * both AF_INET and AF_INET6 flows.
         */
        tcp_sock = bpf_tcp_sock(sk);
        if (!tcp_sock)
            return;
        flow = bpf_sk_storage_get(&rto_flows, tcp_sock, 0, BPF_SK_STORAGE_GET_F_CREATE);
    }
    if (!flow)
        return;

    if (flow->samples < tuning->warmup_samples) {
        flow->samples++;
        if (stats)
            stats->skipped_warmup++;
        return;
    }

    skyline_update_rto_min(ops, tuning, stats, flow, base);
    skyline_update_rto_max(ops, tuning, stats, flow, tcp_sock, srtt_us, min_rtt_us, base);
}

SEC("sockops")
int skyline_select_congestion_control(struct bpf_sock_ops *ops)
{
    const struct skyline_rto_tuning *tuning = skyline_rto_config();
    const char cc[] = "skyline_cc";

    if (!skyline_is_supported_family(ops))
        return 0;

    switch (ops->op) {
    case BPF_SOCK_OPS_ACTIVE_ESTABLISHED_CB:
    case BPF_SOCK_OPS_PASSIVE_ESTABLISHED_CB: {
        __u32 key = 0;
        __u32 *enabled = bpf_map_lookup_elem(&policy_enabled, &key);
        struct skyline_rto_stats *stats = skyline_rto_stats_get();

        if (stats)
            stats->established_cb++;
        if (enabled && *enabled)
            bpf_setsockopt(ops, SOL_TCP, TCP_CONGESTION, (void *)cc, sizeof(cc));
        if (tuning && tuning->enabled) {
            long ret = bpf_sock_ops_cb_flags_set(ops,
                ops->bpf_sock_ops_cb_flags | BPF_SOCK_OPS_RTT_CB_FLAG);
            if (stats) {
                if (ret)
                    stats->subscribe_err++;
                else
                    stats->subscribe_ok++;
            }
        }
        return 0;
    }
    case BPF_SOCK_OPS_RTT_CB:
        skyline_update_rto_tuning(ops, tuning);
        return 0;
    default:
        return 0;
    }
}
