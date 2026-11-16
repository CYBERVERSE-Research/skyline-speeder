/* SPDX-License-Identifier: GPL-2.0-only */
/* Copyright (c) 2026 CYBERVERSE LLC */
/*
 * Helper declarations for a libbpf whose bpf_helper_defs.h predates them.
 *
 * NEVER #include this from a source. The Makefile passes it with -include,
 * and only when the installed bpf_helper_defs.h lacks the helper: libbpf
 * 0.5.0 (Ubuntu 22.04's libbpf-dev) has no bpf_skb_set_tstamp, which
 * skyline_tc.bpf.c calls. With a libbpf that declares it this file is not
 * part of the build at all -- a second declaration would be a redefinition --
 * so a release's objects do not depend on it.
 *
 * It is read before vmlinux.h, so it may use nothing vmlinux.h defines: the
 * struct is only named, and the integer types are spelled out (__u64, __u32).
 * The number is the helper's id in enum bpf_func_id, which is kernel ABI.
 */
#ifndef SKYLINE_OLD_LIBBPF_H
#define SKYLINE_OLD_LIBBPF_H

struct __sk_buff;

/* long bpf_skb_set_tstamp(struct __sk_buff *skb, u64 tstamp, u32 tstamp_type) */
static long (*bpf_skb_set_tstamp)(struct __sk_buff *skb, unsigned long long tstamp,
                                  unsigned int tstamp_type) = (void *) 192;

#endif
