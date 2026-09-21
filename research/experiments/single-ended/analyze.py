#!/usr/bin/env python3
"""Three-way comparison on a drifting path.

Reports two things side by side, because on this path neither alone is honest:
  * absolute medians + IQR, which answer "how fast was it", and
  * within-rotation win rate, which answers "which one was better" without
    being moved by the path getting worse between rotations. A rotation is the
    only window in which the three algorithms saw comparable conditions.
"""
import json, statistics, sys
from collections import defaultdict

ALGOS = ["bbr", "brutal", "skyline_cc"]
LABEL = {"bbr": "bbr", "brutal": "tcp-brutal@200M", "skyline_cc": "skyline_cc"}

# scenario key -> (title, metric, unit, lower_is_better, decimals)
METRICS = [
    ("game",  "RTT p50",        "rtt_ms_p50",       "ms",   True,  1),
    ("game",  "RTT p95",        "rtt_ms_p95",       "ms",   True,  1),
    ("game",  "RTT p99",        "rtt_ms_p99",       "ms",   True,  1),
    ("game",  "卡顿 >150ms",    "stall_150ms",      "次",   True,  0),
    ("game",  "抖动",           "jitter_ms",        "ms",   True,  1),
    ("web",   "页面加载",       "pageload_ms_p50",  "ms",   True,  0),
    ("web",   "TTFB p95",       "ttfb_ms_p95",      "ms",   True,  1),
    ("video", "分块 p50",       "chunk_ms_p50",     "ms",   True,  0),
    ("video", "分块 p95",       "chunk_ms_p95",     "ms",   True,  0),
    ("video", "启动延迟",       "startup_ms",       "ms",   True,  0),
    ("video", "卡顿次数",       "rebuffer_events",  "次",   True,  0),
    ("bulk1", "吞吐",           "goodput_mbps",     "Mbps", False, 1),
    ("bulk4", "吞吐",           "goodput_mbps",     "Mbps", False, 1),
]


def load(path):
    by = defaultdict(lambda: defaultdict(dict))   # scen -> algo -> rot -> record
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        r = json.loads(line)
        if "error" in r:
            print(f"!! {r.get('algo')}/{r.get('scenario')} rot{r.get('rep')}: {r['error']}",
                  file=sys.stderr)
            continue
        scen = r["scenario"]
        if scen == "bulk":
            scen = f"bulk{r.get('streams', 1)}"
        by[scen][r["algo"]][r["rep"]] = r
    return by


def q(vals, p):
    s = sorted(vals)
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def main(path, probe_path=None):
    by = load(path)
    rots = sorted({rot for s in by.values() for a in s.values() for rot in a})

    if probe_path:
        try:
            probes = [json.loads(l) for l in open(probe_path) if l.strip()]
            print("## 链路状态（每轮开始时实测）\n")
            print("| 轮次 | " + " | ".join(str(p["rot"]) for p in probes) + " |")
            print("|---|" + "---|" * len(probes))
            print("| RTT avg ms | " + " | ".join(
                f'{p["rtt_avg_ms"]:.1f}' if p.get("rtt_avg_ms") is not None else "-"
                for p in probes) + " |")
            print("| 丢包 % | " + " | ".join(
                f'{p["loss_pct"]:.0f}' if p.get("loss_pct") is not None else "-"
                for p in probes) + " |")
            print()
        except (OSError, ValueError, KeyError):
            pass

    print("## 逐轮原始数据\n")
    for scen, name, field, unit, lower, nd in METRICS:
        if scen not in by:
            continue
        print(f"**{scen} · {name} ({unit})** — {'越低越好' if lower else '越高越好'}\n")
        print("| 算法 | " + " | ".join(f"第{r}轮" for r in rots) + " | 中位数 | IQR |")
        print("|---|" + "---|" * (len(rots) + 2))
        for a in ALGOS:
            cells, vals = [], []
            for r in rots:
                rec = by[scen].get(a, {}).get(r)
                v = rec.get(field) if rec else None
                if isinstance(v, (int, float)):
                    vals.append(v)
                    cells.append(f"{v:,.{nd}f}")
                else:
                    cells.append("—")
            if vals:
                med = f"{statistics.median(vals):,.{nd}f}"
                iqr = f"{q(vals, 25):,.{nd}f}–{q(vals, 75):,.{nd}f}"
            else:
                med = iqr = "—"
            print(f"| {LABEL[a]} | " + " | ".join(cells) + f" | **{med}** | {iqr} |")
        print()

    print("## 轮内胜率（不受链路漂移影响）\n")
    print("在每一轮里，三者面对的是同一时段的链路。统计每轮谁排第一。\n")
    print("| 指标 | " + " | ".join(LABEL[a] for a in ALGOS) + " | 有效轮次 |")
    print("|---|---|---|---|---|")
    for scen, name, field, unit, lower, nd in METRICS:
        if scen not in by:
            continue
        wins = {a: 0 for a in ALGOS}
        valid = 0
        for r in rots:
            vals = {}
            for a in ALGOS:
                rec = by[scen].get(a, {}).get(r)
                v = rec.get(field) if rec else None
                if isinstance(v, (int, float)):
                    vals[a] = v
            if len(vals) < len(ALGOS):
                continue
            valid += 1
            best = min(vals.values()) if lower else max(vals.values())
            for a, v in vals.items():
                if v == best:
                    wins[a] += 1
        if valid:
            print(f"| {scen} · {name} | " +
                  " | ".join(f"{wins[a]}" for a in ALGOS) + f" | {valid} |")
    print()

    print("## 服务端代价（全部场景累计）\n")
    print("| 指标 | " + " | ".join(LABEL[a] for a in ALGOS) + " |")
    print("|---|---|---|---|")
    agg = {a: defaultdict(int) for a in ALGOS}
    for scen in by:
        for a in ALGOS:
            for rec in by[scen].get(a, {}).values():
                for k, v in (rec.get("server") or {}).items():
                    agg[a][k] += v
    rows = [("重传率 %", lambda d: 100.0 * d["TcpRetransSegs"] / d["TcpOutSegs"]
             if d.get("TcpOutSegs") else None, 2),
            ("发出报文段", lambda d: d.get("TcpOutSegs"), 0),
            ("重传报文段", lambda d: d.get("TcpRetransSegs"), 0),
            ("RTO 超时", lambda d: d.get("TcpExtTCPTimeouts"), 0),
            ("快速重传", lambda d: d.get("TcpExtTCPFastRetrans"), 0),
            ("白重传 DSACK", lambda d: d.get("TcpExtTCPDSACKRecv"), 0)]
    for label, fn, nd in rows:
        cells = []
        for a in ALGOS:
            v = fn(agg[a])
            cells.append(f"{v:,.{nd}f}" if isinstance(v, (int, float)) else "-")
        print(f"| {label} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
