// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>
#include "skyline_abi.h"

#ifndef TC_ACT_OK
#define TC_ACT_OK 0
#endif
#ifndef ETH_P_IP
#define ETH_P_IP 0x0800
#endif
#ifndef ETH_P_IPV6
#define ETH_P_IPV6 0x86dd
#endif
#ifndef ETH_HLEN
#define ETH_HLEN 14
#endif
#ifndef offsetof
#define offsetof(type, member) __builtin_offsetof(type, member)
#endif

/* IPv6 extension-header nexthdr values not already in vmlinux.h's IPPROTO_*
 * enum (IPPROTO_TCP/ESP/AH are already there). HOPOPTS==0 means "Hop-by-Hop
 * Options header follows", not "no next header" -- easy to misread. */
#define SKYLINE_IPPROTO_HOPOPTS 0
#define SKYLINE_IPPROTO_ROUTING 43
#define SKYLINE_IPPROTO_DSTOPTS 60

/* Bound on the IPv6 extension-header walk in skyline_parse_ipv6() below.
 * Verifier-safety and the specific values here (N=4 headers, 96 bytes) were
 * confirmed with a standalone bpftool prog load/run spike before this was
 * wired in (0/1/2-header chains, chains deeper than N, Fragment/AH/ESP/
 * unknown nexthdr, truncated frames, and a hdrlen=255 attack all behaved as
 * designed; peak_states stayed at 23 out of a 1M instruction budget, so N=4
 * has real headroom, not just enough to barely pass). N=4 is RFC 8200's
 * worst legal chain (Hop-by-Hop, then Destination Options for a Routing
 * header, then Routing, then Destination Options for the upper layer); the
 * one chain that shows up in real traffic today is N=1 (IPv6 BIG TCP's
 * Hop-by-Hop Jumbogram option). Fragment/AH/ESP/anything unrecognized is
 * never walked (fail-open, see skyline_parse_ipv6()) -- this is not a general
 * IPv6 extension-header parser, only enough to reach a first-fragment TCP
 * header for the same reason skyline_parse_ipv4() below only handles IPv4's
 * first fragment.
 */
#define SKYLINE_IPV6_MAX_EXT_HDRS 4
#define SKYLINE_IPV6_MAX_EXT_BYTES 96
#define SKYLINE_IPV6_MAX_L4_OFF (ETH_HLEN + 40 + SKYLINE_IPV6_MAX_EXT_BYTES)

char LICENSE[] SEC("license") = "GPL";

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_tc_stats);
} tc_stats SEC(".maps");

/* Retransmit DSCP marking config/stats -- see the doc comment on
 * struct skyline_retransmit_dscp_config in skyline_abi.h for why this is its own
 * independent single-slot map rather than living in skyline_config, and why
 * detection lives here (the only program with skb access) instead of in
 * skyline_cc.bpf.c/skyline_policy.bpf.c (struct_ops has no skb; sockops here never
 * touches skb data).
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_retransmit_dscp_config);
} retransmit_dscp_config SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_retransmit_dscp_stats);
} retransmit_dscp_stats SEC(".maps");

/* First-flight redundancy config/stats -- see the doc comment on
 * struct skyline_redundancy_config in skyline_abi.h.
 */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_redundancy_config);
} redundancy_config SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct skyline_redundancy_stats);
} redundancy_stats SEC(".maps");

/* BPF_SKB_CLOCK_MONOTONIC since 6.11, BPF_SKB_TSTAMP_DELIVERY_MONO before --
 * the same value under two names, so the number is spelled out here rather
 * than tying the build to whichever name this vmlinux.h happens to carry.
 */
#define SKYLINE_SKB_CLOCK_MONOTONIC 1

/* Rewrites only the DSCP bits (iphdr.tos upper 6 bits), preserving the ECN
 * bits (lower 2) untouched, and fixes up the IPv4 header checksum
 * incrementally. iphdr.tos shares a 16-bit big-endian word with
 * iphdr.version/ihl, and bpf_l3_csum_replace has no 1-byte size class, so
 * the diff must be computed over that halfword, not the tos byte alone.
 * Never touches the TCP checksum -- ToS is not covered by the TCP
 * pseudo-header. Returns 1 if a csum fixup was issued (for the caller's
 * csum_fixups counter), 0 if the byte was already at the target value.
 *
 * old_tos/old_hw are supplied by the caller (skyline_parse_ipv4()) rather than
 * re-read here -- this is the original, already wire-verified calling
 * convention, kept unchanged by the v4/v6 split below rather than switched
 * to "each marker re-reads its own bytes" purely for cosmetic symmetry with
 * skyline_mark_dscp_v6(). IPv6 has no equivalent pre-existing convention to
 * preserve, so skyline_mark_dscp_v6() reads its own bytes instead.
 */
static __always_inline int skyline_mark_dscp_v4(struct __sk_buff *skb, __u32 dscp_value,
                                              __u8 old_tos, __u16 old_hw)
{
    __u8 new_tos = (old_tos & 0x03) | ((dscp_value & 0x3f) << 2);

    if (new_tos == old_tos)
        return 0;

    __u16 new_hw = bpf_htons((bpf_ntohs(old_hw) & 0xff00) | new_tos);

    bpf_l3_csum_replace(skb, ETH_HLEN + offsetof(struct iphdr, check),
                         old_hw, new_hw, 2);
    bpf_skb_store_bytes(skb, ETH_HLEN + offsetof(struct iphdr, tos),
                         &new_tos, 1, 0);
    return 1;
}

/* IPv6 equivalent of skyline_mark_dscp_v4(): rewrites only the 6-bit DSCP field
 * inside the first 32-bit word of the IPv6 header (version:4 |
 * traffic_class:8 | flow_label:20, big-endian on the wire), preserving the
 * 2 ECN bits and the 20-bit flow label untouched. DSCP straddles the
 * byte-0/byte-1 boundary of that word, so this is done as one 16-bit
 * read-modify-write rather than byte-at-a-time. Unlike IPv4, there is no
 * IPv6 header checksum field to fix up -- bpf_l3_csum_replace() is neither
 * needed nor called here, so csum_fixups (see skyline_abi.h) stays 0 for IPv6
 * traffic by design, not by omission. TCP checksum is unaffected either
 * way: neither traffic class nor flow label are covered by the IPv6
 * pseudo-header. No return value -- there is no v6 equivalent of
 * csum_fixups to report, so unlike skyline_mark_dscp_v4() the caller does not
 * need to know whether a rewrite actually happened.
 *
 * Bit layout verified against a real ECN(0x03)+flow_label(0xabcde) test
 * packet with a standalone bpftool prog run spike before this was wired in
 * -- output showed DSCP set to the target value with ECN and flow label
 * byte-for-byte unchanged.
 */
static __always_inline void skyline_mark_dscp_v6(struct __sk_buff *skb, __u32 dscp_value)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;

    if (data + ETH_HLEN + 2 > data_end)
        return;

    __u16 vtc = bpf_ntohs(*(__u16 *)(data + ETH_HLEN));
    __u16 new_vtc = (vtc & 0xf03f) | ((__u16)(dscp_value & 0x3f) << 6);

    if (new_vtc == vtc)
        return;

    __be16 store = bpf_htons(new_vtc);

    bpf_skb_store_bytes(skb, ETH_HLEN, &store, 2, 0);
}

/* Parses an IPv4 + TCP header, first fragment only (frag_off's MF bit and
 * offset both zero), ihl >= 5. On success returns 1 and sets *l4_off to the
 * TCP header's byte offset from the start of the frame, plus *old_tos/
 * *old_hw for a later skyline_mark_dscp_v4() call (see that function's doc
 * comment for why they are threaded through rather than re-read there).
 * Anything else -- non-IPv4, non-TCP, non-first-fragment, ihl<5, or a
 * bounds-check failure -- returns 0 and leaves the output parameters
 * untouched (fail open, never drops a packet because of this feature).
 * Does NOT check that the TCP header itself fits past *l4_off -- that is
 * skyline_is_retransmit()'s job, shared with the IPv6 path below.
 */
static __always_inline int skyline_parse_ipv4(struct __sk_buff *skb, __u32 *l4_off,
                                            __u8 *old_tos, __u16 *old_hw)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct iphdr *ip;

    if ((void *)(eth + 1) > data_end)
        return 0;
    if (eth->h_proto != bpf_htons(ETH_P_IP))
        return 0;
    ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return 0;
    if (ip->protocol != IPPROTO_TCP || ip->ihl < 5)
        return 0;
    /* Non-first fragments have no TCP header at this offset -- frag_off's
     * low 13 bits are the fragment offset, bit 0x2000 is More-Fragments. */
    if (ip->frag_off & bpf_htons(0x3fff))
        return 0;

    *l4_off = ETH_HLEN + ip->ihl * 4;
    *old_tos = ip->tos;
    *old_hw = *(__u16 *)ip;
    return 1;
}

/* IPv6 equivalent of skyline_parse_ipv4(): walks past the 40-byte fixed header
 * and up to SKYLINE_IPV6_MAX_EXT_HDRS extension headers looking for a TCP
 * header. Only Hop-by-Hop Options(0)/Routing(43)/Destination Options(60)
 * are actually walked -- their length field has the same "(hdrlen+1)*8
 * bytes" encoding, letting one loop body handle all three. Everything else
 * bails immediately without being parsed further:
 *   - Fragment(44): a first-fragment TCP header here would still be
 *     genuine, but its payload length can't be recovered from skb->len the
 *     way skyline_is_retransmit() needs (the frame only carries part of the
 *     original datagram), so end_seq would be wrong -- this mirrors
 *     skyline_parse_ipv4()'s equally deliberate "any fragment, including the
 *     first, is out of scope" choice above, not an oversight.
 *   - AH(51): its length field is encoded differently ("(hdrlen+2)*4
 *     bytes", not "(hdrlen+1)*8") -- folding it into the shared loop body
 *     would need a per-iteration branch, for a header type that is
 *     essentially never present on ordinary TCP traffic.
 *   - ESP(50): everything after it is ciphertext; there is no TCP header
 *     to find.
 *   - Anything unrecognized, a chain deeper than SKYLINE_IPV6_MAX_EXT_HDRS, a
 *     chain whose cumulative extension-header bytes exceed
 *     SKYLINE_IPV6_MAX_EXT_BYTES, or a bounds-check failure on any individual
 *     header.
 * Every bail-out above increments retransmit_dscp_stats.ipv6_chain_bailout
 * (see skyline_abi.h) rather than failing silently -- this walk fails open like
 * skyline_parse_ipv4() (never drops a packet), but unlike the IPv4 path, "no
 * IPv6 packets ever get marked" and "the walk keeps giving up before
 * reaching TCP" need to be distinguishable from the stats alone.
 */
static __always_inline int skyline_parse_ipv6(struct __sk_buff *skb, __u32 *l4_off,
                                            struct skyline_retransmit_dscp_stats *stats)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    __u8 nexthdr;
    __u32 off;
    int i;

    /* ipv6hdr.nexthdr sits at byte offset 6 within the 40-byte fixed header
     * (priority:4|version:4 = 1B, flow_lbl[3] = 3B, payload_len = 2B,
     * nexthdr = 1B here, hop_limit = 1B) -- checking data_end against this
     * offset also covers every earlier byte in the fixed header. */
    if (data + ETH_HLEN + 7 > data_end)
        return 0;
    nexthdr = *(__u8 *)(data + ETH_HLEN + 6);
    off = ETH_HLEN + 40;

#pragma unroll
    for (i = 0; i < SKYLINE_IPV6_MAX_EXT_HDRS; i++) {
        struct ipv6_opt_hdr *opt;

        if (nexthdr == IPPROTO_TCP)
            goto found;
        if (nexthdr != SKYLINE_IPPROTO_HOPOPTS &&
            nexthdr != SKYLINE_IPPROTO_ROUTING &&
            nexthdr != SKYLINE_IPPROTO_DSTOPTS) {
            if (stats)
                stats->ipv6_chain_bailout++;
            return 0;
        }

        opt = (struct ipv6_opt_hdr *)(data + off);
        if ((void *)(opt + 1) > data_end) {
            if (stats)
                stats->ipv6_chain_bailout++;
            return 0;
        }
        nexthdr = opt->nexthdr;
        off += ((__u32)opt->hdrlen + 1) * 8;

        /* Clamp immediately after the addition -- opt->hdrlen is an
         * attacker-controlled byte (up to 255, i.e. +2048 per iteration);
         * without this the verifier's tracked upper bound on `off` grows
         * unbounded across iterations and rejects the later pointer
         * arithmetic. Confirmed with a spike test that declares
         * hdrlen=255 in a short frame -- this clamp is what makes that
         * case bail instead of computing a wild offset. */
        if (off > SKYLINE_IPV6_MAX_L4_OFF) {
            if (stats)
                stats->ipv6_chain_bailout++;
            return 0;
        }
    }
    if (nexthdr != IPPROTO_TCP) {
        if (stats)
            stats->ipv6_chain_bailout++;
        return 0; /* chain deeper than SKYLINE_IPV6_MAX_EXT_HDRS */
    }

found:
    *l4_off = off;
    return 1;
}

/* Family-agnostic core, shared by the IPv4 and IPv6 paths: given l4_off
 * (already resolved by skyline_parse_ipv4()/skyline_parse_ipv6(), but not yet
 * proven to have a full TCP header there), validates the TCP header fits,
 * then detects a retransmission by comparing this packet's own end_seq
 * against bpf_tcp_sock(sk)->snd_nxt, read BEFORE tcp_event_new_data_sent()
 * advances it for THIS packet (tcp_transmit_skb() hands the skb to the
 * device first) -- end_seq <= snd_nxt means this byte range was already
 * sent before, i.e. a retransmission (RTO, fast-retransmit, or RACK alike;
 * all of them genuinely resend previously-sent bytes, which is exactly what
 * should be steered). end_seq == seq (pure ACK / zero-window probe: no
 * payload, no SYN, no FIN) is excluded up front -- without this exclusion,
 * EVERY pure ACK would satisfy "end_seq <= snd_nxt" and get marked, since a
 * pure ACK's seq field sits at snd_nxt itself.
 *
 * GSO: at TC egress the skb is the pre-segmentation superframe (one seq,
 * one aggregated payload) -- the seq/end_seq logic is still correct, but
 * payload length is derived from skb->len rather than iphdr.tot_len/
 * ipv6hdr.payload_len, because BIG TCP sets that field to 0 (or an
 * unreliable value) and the real length only lives in skb->len; l4_off
 * already has every L2/L3 header byte -- including any IPv6 extension
 * headers -- folded in, so `skb->len - (l4_off + tcp->doff*4)` is the
 * aggregate TCP payload regardless of address family.
 *
 * Returns 1 (and increments retransmits_detected) for a genuine
 * retransmission; 0 otherwise (TCP header doesn't fit, pure ACK/probe, or
 * genuinely new data) -- packets_seen is incremented before any of those
 * 0-returning checks except the first, matching the original (pre-split)
 * accounting exactly.
 */
static __always_inline int skyline_is_retransmit(struct __sk_buff *skb, __u32 l4_off,
                                               struct skyline_retransmit_dscp_stats *stats)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct tcphdr *tcp = (void *)(data + l4_off);
    struct bpf_sock *sk;
    struct bpf_tcp_sock *tp;
    __u32 seq, end_seq, snd_nxt;

    if ((void *)(tcp + 1) > data_end)
        return 0;

    if (stats)
        stats->packets_seen++;

    seq = bpf_ntohl(tcp->seq);
    {
        __u32 header_len = l4_off + tcp->doff * 4;
        __u32 payload = skb->len > header_len ? skb->len - header_len : 0;

        end_seq = seq + payload + tcp->syn + tcp->fin;
    }
    if (end_seq == seq)
        return 0; /* pure ACK / zero-window probe -- never a retransmit */

    sk = skb->sk;
    if (!sk)
        return 0;
    sk = bpf_sk_fullsock(sk);
    if (!sk)
        return 0;
    tp = bpf_tcp_sock(sk);
    if (!tp)
        return 0;
    snd_nxt = tp->snd_nxt;

    if ((__s32)(end_seq - snd_nxt) > 0)
        return 0; /* new data, not yet sent before */

    if (stats)
        stats->retransmits_detected++;
    return 1;
}

/* Dispatches on address family, then runs the shared detection core and, if
 * the feature is enabled, marks the packet with the family-appropriate
 * marker. See skyline_parse_ipv4()/skyline_parse_ipv6() for what each family
 * accepts, and skyline_is_retransmit() for the shared detection logic.
 */
static void skyline_check_retransmit(struct __sk_buff *skb,
                                  struct skyline_retransmit_dscp_config *cfg,
                                  struct skyline_retransmit_dscp_stats *stats)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    __u32 l4_off;
    int is_ipv6;
    __u8 old_tos = 0;
    __u16 old_hw = 0;

    if ((void *)(eth + 1) > data_end)
        return;

    if (eth->h_proto == bpf_htons(ETH_P_IP)) {
        if (!skyline_parse_ipv4(skb, &l4_off, &old_tos, &old_hw))
            return;
        is_ipv6 = 0;
    } else if (eth->h_proto == bpf_htons(ETH_P_IPV6)) {
        if (!skyline_parse_ipv6(skb, &l4_off, stats))
            return;
        is_ipv6 = 1;
    } else {
        return; /* ARP, VLAN, MPLS, etc. -- out of scope */
    }

    if (!skyline_is_retransmit(skb, l4_off, stats))
        return;
    if (!cfg->enabled)
        return;

    /* retransmits_marked counts every enabled retransmit that carries
     * dscp_value out on the wire -- i.e. it tracks a WIRE PROPERTY, not
     * "did this specific pass touch any bytes". These are not the same
     * thing: when the same byte range is retransmitted more than once,
     * Linux's retransmit path reuses the same underlying skb header
     * buffer across attempts, so the ToS/traffic-class byte(s) are already
     * dscp_value by the second retransmission -- the marker functions
     * correctly skip the (unneeded) rewrite, but the packet is still a
     * genuine, correctly marked retransmit and must still count as one.
     * Counting only the rewrite would silently undercount by roughly the
     * fraction of retransmits that are themselves re-retransmitted --
     * confirmed on real IPv4 capture data to undercount by ~2.4x under
     * heavy loss.
     * csum_fixups stays the narrower "actual bpf_l3_csum_replace performed"
     * count -- IPv4-only by construction (see skyline_mark_dscp_v6()), a
     * distinct, legitimate proxy for real per-packet CPU cost, not a
     * correctness metric.
     */
    if (stats)
        stats->retransmits_marked++;

    if (is_ipv6) {
        skyline_mark_dscp_v6(skb, cfg->dscp_value);
        if (stats)
            stats->ipv6_marked++;
    } else if (skyline_mark_dscp_v4(skb, cfg->dscp_value, old_tos, old_hw)) {
        if (stats)
            stats->csum_fixups++;
    }
}

/* Whether a full TCP socket runs skyline_cc, by the name its congestion
 * control registered under (bpf/skyline_cc.bpf.c's .name, and
 * SKYLINE_CC_NAME in skyline-speederd). The name, not the ops pointer: a
 * struct_ops registration has no address this program could know in
 * advance, and a restart of skyline-speederd registers a new one.
 */
static __always_inline bool skyline_runs_skyline_cc(struct tcp_sock *tp)
{
    const struct tcp_congestion_ops *ops = tp->inet_conn.icsk_ca_ops;
    const char expected[] = "skyline_cc";

    if (!ops)
        return false;
#pragma unroll
    for (int index = 0; index < (int)sizeof(expected); index++) {
        if (ops->name[index] != expected[index])
            return false;
    }
    return true;
}

/* The second pass of a copy made by skyline_redundancy_duplicate(): the
 * clone comes straight back through this hook, without a socket, still
 * carrying the tag and the delay written into cb[] before cloning. Stamping
 * its departure time here is what makes the copy late -- the stamp cannot be
 * set before the clone exists, and bpf_clone_redirect() clears skb->tstamp
 * on the way out. Returns true when the packet was such a copy.
 */
static __always_inline bool skyline_redundancy_copy(struct __sk_buff *skb)
{
    __u32 delay_us;

    if (skb->cb[0] != SKYLINE_REDUNDANCY_TAG || skb->sk)
        return false;
    delay_us = skb->cb[1];
    skb->cb[0] = 0;
    skb->cb[1] = 0;
    if (delay_us)
        bpf_skb_set_tstamp(skb, bpf_ktime_get_ns() + (__u64)delay_us * 1000ULL,
                           SKYLINE_SKB_CLOCK_MONOTONIC);
    return true;
}

/* First pass: decides whether this packet is one skyline_abi.h's
 * struct skyline_redundancy_config says to send twice, and if so clones it
 * back to the same device. Everything that is not clearly one of those --
 * a packet this parser cannot place, a socket that is not skyline_cc, a
 * segment past the first `bytes` -- goes out once, untouched.
 *
 * The data offset is where the segment starts within everything the
 * connection has sent: snd_una - bytes_acked is the first data sequence
 * number (ISN + 1) for the connection's whole life, so this needs no
 * per-connection state. Retransmissions of that range are copied too.
 */
static __always_inline void skyline_redundancy_duplicate(struct __sk_buff *skb,
                                                     struct skyline_redundancy_config *cfg,
                                                     struct skyline_redundancy_stats *stats)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct bpf_sock *sk;
    struct tcp_sock *tp;
    struct tcphdr *tcp;
    __u32 l4_off, len;
    __u8 old_tos;
    __u16 old_hw;
    bool handshake;

    if ((void *)(eth + 1) > data_end)
        return;
    if (eth->h_proto == bpf_htons(ETH_P_IP)) {
        if (!skyline_parse_ipv4(skb, &l4_off, &old_tos, &old_hw))
            return;
    } else if (eth->h_proto == bpf_htons(ETH_P_IPV6)) {
        if (!skyline_parse_ipv6(skb, &l4_off, 0))
            return;
    } else {
        return;
    }
    tcp = (void *)(data + l4_off);
    if ((void *)(tcp + 1) > data_end)
        return;

    handshake = tcp->syn;
    sk = skb->sk;
    if (!sk)
        return;
    if (!(tcp->syn && tcp->ack)) {
        /* A SYN-ACK belongs to a request socket, which has no congestion
         * control yet; everything else must come from a skyline_cc socket.
         */
        tp = bpf_skc_to_tcp_sock(sk);
        if (!tp || !skyline_runs_skyline_cc(tp))
            return;
        if (!handshake) {
            __u32 header_len = l4_off + tcp->doff * 4;

            if (skb->len <= header_len)
                return; /* pure ACK, bare FIN */
            if (bpf_ntohl(tcp->seq) - (tp->snd_una - (__u32)tp->bytes_acked) >= cfg->bytes)
                return;
        }
    }

    len = skb->len;
    skb->cb[0] = SKYLINE_REDUNDANCY_TAG;
    skb->cb[1] = cfg->delay_us;
    if (bpf_clone_redirect(skb, skb->ifindex, 0)) {
        if (stats)
            stats->clone_failed++;
    } else if (stats) {
        stats->packets++;
        stats->bytes += len;
        if (handshake)
            stats->handshakes++;
    }
    skb->cb[0] = 0;
    skb->cb[1] = 0;
}

SEC("tc")
int skyline_tc_account(struct __sk_buff *skb)
{
    __u32 key = 0;
    struct skyline_tc_stats *stats = bpf_map_lookup_elem(&tc_stats, &key);
    struct skyline_retransmit_dscp_config *dscp_cfg;
    struct skyline_retransmit_dscp_stats *dscp_stats;
    struct skyline_redundancy_config *red_cfg;
    struct skyline_redundancy_stats *red_stats;

    if (stats) {
        stats->packets++;
        stats->bytes += skb->len;
        if (skb->gso_segs > 1)
            stats->gso_packets++;
    }

    /* A redundancy copy on its way out: nothing below applies to it. Its
     * original was already DSCP-marked, if it was a retransmission, before
     * it was cloned.
     */
    if (skyline_redundancy_copy(skb))
        return TC_ACT_OK;

    /* Config lookups first: enabled=0 (the safe zero-init default) costs
     * exactly one array lookup per feature on top of the counting above --
     * no header parsing, no skb mutation -- so each feature is free when off.
     */
    dscp_cfg = bpf_map_lookup_elem(&retransmit_dscp_config, &key);
    if (dscp_cfg && dscp_cfg->enabled) {
        dscp_stats = bpf_map_lookup_elem(&retransmit_dscp_stats, &key);
        if (dscp_cfg->abi_version != SKYLINE_RETRANSMIT_DSCP_ABI_VERSION) {
            if (dscp_stats)
                dscp_stats->abi_mismatch++;
        } else {
            skyline_check_retransmit(skb, dscp_cfg, dscp_stats);
        }
    }

    red_cfg = bpf_map_lookup_elem(&redundancy_config, &key);
    if (red_cfg && red_cfg->enabled) {
        red_stats = bpf_map_lookup_elem(&redundancy_stats, &key);
        if (red_cfg->abi_version != SKYLINE_REDUNDANCY_ABI_VERSION) {
            if (red_stats)
                red_stats->abi_mismatch++;
        } else {
            skyline_redundancy_duplicate(skb, red_cfg, red_stats);
        }
    }
    return TC_ACT_OK;
}
