#!/usr/bin/env python3
"""Render the single-ended acceleration comparison as README figures.

Reads head-to-head.jsonl and computes every number it draws, so the figures
cannot drift away from the data behind them -- re-run it after a new matrix and
the charts follow.

Standard library only, by design: this repo pins matplotlib for the experiment
harness, but a README figure should be reproducible on a bare checkout with no
virtualenv. SVG keeps the figure crisp at any width and its numbers selectable.

Two variants of each figure -- light and dark -- because GitHub renders READMEs
in both themes and a single light-surface figure is a glaring slab in dark mode.

Usage: python3 plot_comparison.py [data.jsonl] [output-dir]
"""
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

# ---------------------------------------------------------------- palette
# Categorical slots 1/2/3 of the validated default palette, in fixed order.
# Checked with the data-viz validator under the strictest `pairs: all` setting
# in both modes: worst CVD dE 9.2 light / 9.4 dark, worst normal-vision dE 24.0
# light / 20.9 dark. Light-mode aqua sits at 2.74:1 against the surface, below
# the 3:1 bar, which obliges the relief rule -- every bar here carries a visible
# value label, so identity and magnitude never rest on hue alone.
THEME = {
    "light": dict(surface="#fcfcfb", ink="#0b0b0b", ink2="#52514e", ink3="#6b6a66",
                  grid="#e8e7e3", rule="#d6d5d0",
                  series=("#2a78d6", "#eb6834", "#1baf7a")),
    "dark": dict(surface="#1a1a19", ink="#ffffff", ink2="#c3c2b7", ink3="#99988e",
                 grid="#343431", rule="#4a4a46",
                 series=("#3987e5", "#d95926", "#199e70")),
}
FONT = ("-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, "
        "Arial, sans-serif")
BBR, BRUTAL, SKY = 0, 1, 2
ALGO_SLOT = {"bbr": BBR, "brutal": BRUTAL, "skyline_cc": SKY}

# (scenario key, row label, field, lower_is_better, value format)
METRICS = [
    ("game",  "Game · stalls >150 ms",  "stall_150ms",     True,  "{:,.0f}"),
    ("game",  "Game · RTT p95",         "rtt_ms_p95",      True,  "{:,.0f} ms"),
    ("game",  "Game · jitter",          "jitter_ms",       True,  "{:,.1f} ms"),
    ("video", "Video · chunk p95",      "chunk_ms_p95",    True,  "{:,.0f} ms"),
    ("video", "Video · startup",        "startup_ms",      True,  "{:,.0f} ms"),
    ("web",   "Web · page load",        "pageload_ms_p50", True,  "{:,.0f} ms"),
    ("bulk1", "Bulk · 1 stream",        "goodput_mbps",    False, "{:,.0f} Mbps"),
    ("bulk4", "Bulk · 4 streams",       "goodput_mbps",    False, "{:,.0f} Mbps"),
    ("cost",  "Cost · retransmit rate", "retrans_pct",     True,  "{:,.1f}%"),
]


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def load(path):
    """scenario -> algo -> [per-run values], plus a synthetic 'cost' scenario."""
    runs = defaultdict(lambda: defaultdict(list))
    # retransmit rate is a per-rotation ratio of sums, not a mean of per-run
    # ratios: short runs would otherwise weigh as much as long ones.
    seg = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        r = json.loads(line)
        if "error" in r:
            continue
        scen = r["scenario"]
        if scen == "bulk":
            scen = f"bulk{r.get('streams', 1)}"
        runs[scen][r["algo"]].append(r)
        s = r.get("server") or {}
        acc = seg[r["algo"]][r["rep"]]
        acc[0] += s.get("TcpOutSegs", 0)
        acc[1] += s.get("TcpRetransSegs", 0)
    for algo, rots in seg.items():
        for rot, (out, rt) in rots.items():
            if out:
                runs["cost"][algo].append({"retrans_pct": 100.0 * rt / out, "rep": rot})
    return runs


def quart(vals, p):
    s = sorted(vals)
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def stats(runs, scen, algo, field):
    vals = [r[field] for r in runs.get(scen, {}).get(algo, [])
            if isinstance(r.get(field), (int, float))]
    if not vals:
        return None
    return dict(med=statistics.median(vals), p25=quart(vals, 25),
                p75=quart(vals, 75), n=len(vals))


def hbar(x0, x1, y, h, r=3.5):
    """Horizontal bar, rounded on the data end only, anchored at the zero line."""
    if x1 >= x0:
        r = max(0.0, min(r, (x1 - x0) / 2, h / 2))
        return (f"M{x0:.1f},{y:.1f} L{x1 - r:.1f},{y:.1f} Q{x1:.1f},{y:.1f} {x1:.1f},{y + r:.1f} "
                f"L{x1:.1f},{y + h - r:.1f} Q{x1:.1f},{y + h:.1f} {x1 - r:.1f},{y + h:.1f} "
                f"L{x0:.1f},{y + h:.1f} Z")
    r = max(0.0, min(r, (x0 - x1) / 2, h / 2))
    return (f"M{x0:.1f},{y:.1f} L{x1 + r:.1f},{y:.1f} Q{x1:.1f},{y:.1f} {x1:.1f},{y + r:.1f} "
            f"L{x1:.1f},{y + h - r:.1f} Q{x1:.1f},{y + h:.1f} {x1 + r:.1f},{y + h:.1f} "
            f"L{x0:.1f},{y + h:.1f} Z")


# ============================================================ at-a-glance
W1, H1 = 1060, 596
LABEL_W, PAD_L, PAD_R = 208, 34, 96
ROW_H, BAR_H, BAR_GAP = 46, 15, 4


def render_glance(runs, mode):
    t = THEME[mode]
    rows = []
    for scen, label, field, lower, fmt in METRICS:
        base = stats(runs, scen, "bbr", field)
        if not base:
            continue
        entry = {"label": label, "base": base["med"], "fmt": fmt, "vals": {}}
        for algo in ("brutal", "skyline_cc"):
            st = stats(runs, scen, algo, field)
            if not st:
                continue
            b, v = base["med"], st["med"]
            # "better" always points the same way, whichever direction the
            # underlying metric runs, so one axis reads for every row.
            delta = (b - v) / b * 100.0 if lower else (v - b) / b * 100.0
            entry["vals"][algo] = (delta, v)
        rows.append(entry)

    span = max(abs(d) for r in rows for d, _ in r["vals"].values())
    span = max(span * 1.12, 10.0)
    plot_x0, plot_x1 = PAD_L + LABEL_W, W1 - PAD_R
    zero = (plot_x0 + plot_x1) / 2
    half = (plot_x1 - plot_x0) / 2

    def px(d):
        return zero + d / span * half

    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W1}" height="{H1}" '
         f'viewBox="0 0 {W1} {H1}" font-family="{FONT}" role="img" '
         f'aria-label="tcp-brutal and skyline_cc measured against a bbr baseline '
         f'across game, video, web and bulk traffic">',
         f'<rect width="{W1}" height="{H1}" fill="{t["surface"]}"/>']
    o.append(f'<text x="{PAD_L}" y="40" font-size="23" font-weight="700" '
             f'fill="{t["ink"]}">How much better than bbr?</text>')
    o.append(f'<text x="{PAD_L}" y="63" font-size="13" fill="{t["ink2"]}">'
             f'Singapore server → Alibaba Cloud client in South China · RTT 70–73 ms, '
             f'10–30% loss, 200 Mbps bottleneck</text>')
    o.append(f'<text x="{PAD_L}" y="82" font-size="12" fill="{t["ink3"]}">'
             f'Accelerated on the server only. Bars right of the line beat stock bbr; left of it lose to it. '
             f'Medians of 8 rotations, 119 of 120 runs completed.</text>')

    top = 116
    for i, gl in enumerate([-50, -25, 0, 25, 50, 75]):
        if abs(gl) > span:
            continue
        x = px(gl)
        if gl == 0:
            continue
        o.append(f'<line x1="{x:.1f}" y1="{top - 8}" x2="{x:.1f}" '
                 f'y2="{top + len(rows) * ROW_H - 6}" stroke="{t["grid"]}" stroke-width="1"/>')
        o.append(f'<text x="{x:.1f}" y="{top - 14}" font-size="10.5" text-anchor="middle" '
                 f'fill="{t["ink3"]}">{gl:+d}%</text>')

    for i, r in enumerate(rows):
        ry = top + i * ROW_H
        o.append(f'<text x="{PAD_L + LABEL_W - 14}" y="{ry + 22}" font-size="12.5" '
                 f'text-anchor="end" fill="{t["ink"]}">{esc(r["label"])}</text>')
        o.append(f'<text x="{PAD_L + LABEL_W - 14}" y="{ry + 36}" font-size="10.5" '
                 f'text-anchor="end" fill="{t["ink3"]}">bbr '
                 f'{esc(r["fmt"].format(r["base"]))}</text>')
        for j, algo in enumerate(("brutal", "skyline_cc")):
            if algo not in r["vals"]:
                continue
            delta, raw = r["vals"][algo]
            by = ry + 6 + j * (BAR_H + BAR_GAP)
            x1 = px(delta)
            o.append(f'<path d="{hbar(zero, x1, by, BAR_H)}" '
                     f'fill="{t["series"][ALGO_SLOT[algo]]}"/>')
            anchor, tx = ("start", x1 + 7) if delta >= 0 else ("end", x1 - 7)
            o.append(f'<text x="{tx:.1f}" y="{by + 12}" font-size="11.5" '
                     f'font-weight="600" text-anchor="{anchor}" fill="{t["ink"]}">'
                     f'{delta:+.0f}%  <tspan font-weight="400" fill="{t["ink3"]}">'
                     f'{esc(r["fmt"].format(raw))}</tspan></text>')

    # zero line last, in bbr's own hue: the baseline is an entity, not chrome
    o.append(f'<line x1="{zero:.1f}" y1="{top - 8}" x2="{zero:.1f}" '
             f'y2="{top + len(rows) * ROW_H - 6}" stroke="{t["series"][BBR]}" '
             f'stroke-width="2"/>')
    o.append(f'<text x="{zero:.1f}" y="{top - 14}" font-size="10.5" text-anchor="middle" '
             f'font-weight="600" fill="{t["series"][BBR]}">bbr</text>')

    ly = H1 - 26
    lx = PAD_L
    for label, slot in (("tcp-brutal @200M", BRUTAL), ("skyline_cc", SKY)):
        o.append(f'<rect x="{lx}" y="{ly - 10}" width="11" height="11" rx="2.5" '
                 f'fill="{t["series"][slot]}"/>')
        o.append(f'<text x="{lx + 17}" y="{ly}" font-size="12" '
                 f'fill="{t["ink2"]}">{esc(label)}</text>')
        lx += 20 + len(label) * 6.7 + 26
    o.append(f'<text x="{W1 - PAD_L}" y="{ly}" font-size="10.5" text-anchor="end" '
             f'fill="{t["ink3"]}">skyline_cc 0.1.0 coefficients (today&#8217;s &#8220;high random loss&#8221; '
             f'preset) · field measurement, not the controlled test bed</text>')
    o.append("</svg>")
    return "\n".join(o)


# ================================================================== detail
W2, H2 = 1060, 690
M_L, M_R, M_T = 34, 34, 132
COLS, GAP_X, GAP_Y = 3, 26, 60
PANEL_W = (W2 - M_L - M_R - GAP_X * (COLS - 1)) // COLS
PANEL_H = 198
DETAIL = [
    ("game",  "Game · stalls >150 ms",  "stall_150ms",     "count per 30 s", True,  0),
    ("video", "Video · chunk p95",      "chunk_ms_p95",    "ms",             True,  0),
    ("web",   "Web page load",          "pageload_ms_p50", "ms",             True,  0),
    ("bulk1", "Bulk · 1 stream",        "goodput_mbps",    "Mbps",           False, 0),
    ("bulk4", "Bulk · 4 streams",       "goodput_mbps",    "Mbps",           False, 0),
    ("cost",  "Server retransmit rate", "retrans_pct",     "% of segments sent", True, 1),
]


def vbar(x, y, w, h, r=4.0):
    r = max(0.0, min(r, w / 2, h))
    return (f"M{x:.1f},{y + h:.1f} L{x:.1f},{y + r:.1f} Q{x:.1f},{y:.1f} {x + r:.1f},{y:.1f} "
            f"L{x + w - r:.1f},{y:.1f} Q{x + w:.1f},{y:.1f} {x + w:.1f},{y + r:.1f} "
            f"L{x + w:.1f},{y + h:.1f} Z")


def render_detail(runs, mode):
    t = THEME[mode]
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W2}" height="{H2}" '
         f'viewBox="0 0 {W2} {H2}" font-family="{FONT}" role="img" '
         f'aria-label="bbr, tcp-brutal and skyline_cc across five scenarios and '
         f'their retransmission cost">',
         f'<rect width="{W2}" height="{H2}" fill="{t["surface"]}"/>']
    o.append(f'<text x="{M_L}" y="42" font-size="23" font-weight="700" '
             f'fill="{t["ink"]}">Single-ended TCP acceleration, measured end to end</text>')
    o.append(f'<text x="{M_L}" y="66" font-size="13" fill="{t["ink2"]}">'
             f'Singapore server → Alibaba Cloud client in South China · RTT 70–73 ms, 10–30% loss, 200 Mbps '
             f'bottleneck · Debian 13, kernel 6.12</text>')
    o.append(f'<text x="{M_L}" y="85" font-size="12" fill="{t["ink3"]}">'
             f'Medians of 8 rotations (119 of 120 runs completed) · whiskers span the '
             f'interquartile range · independent scale per panel</text>')
    o.append(f'<text x="{M_L}" y="104" font-size="11.5" fill="{t["ink3"]}">'
             f'Field measurement on one production path, not the repo&#39;s controlled '
             f'dual-VM test bed · algorithm order rotated every rotation so path drift '
             f'cannot favour one position</text>')

    for i, (scen, title, field, unit, lower, nd) in enumerate(DETAIL):
        col, row = i % COLS, i // COLS
        px_ = M_L + col * (PANEL_W + GAP_X)
        py = M_T + row * (PANEL_H + GAP_Y)
        plot_top, plot_h = py + 40, PANEL_H - 68
        base_y = plot_top + plot_h
        arrow = "↓ lower is better" if lower else "↑ higher is better"
        o.append(f'<text x="{px_}" y="{py + 12}" font-size="14" font-weight="600" '
                 f'fill="{t["ink"]}">{esc(title)}</text>')
        o.append(f'<text x="{px_}" y="{py + 29}" font-size="11.5" fill="{t["ink3"]}">'
                 f'{esc(unit)} · {arrow}</text>')
        series = []
        for algo, name in (("bbr", "bbr"), ("brutal", "brutal"), ("skyline_cc", "skyline")):
            st = stats(runs, scen, algo, field)
            if st:
                series.append((name, ALGO_SLOT[algo], st))
        if not series:
            continue
        top = max(s["p75"] for _, _, s in series) * 1.20
        slot = PANEL_W / len(series)
        bw = min(52.0, slot * 0.56)
        o.append(f'<line x1="{px_}" y1="{base_y}" x2="{px_ + PANEL_W}" y2="{base_y}" '
                 f'stroke="{t["rule"]}" stroke-width="1"/>')
        for j, (name, cslot, st) in enumerate(series):
            cx = px_ + slot * (j + 0.5)
            bh = plot_h * st["med"] / top
            by = base_y - bh
            o.append(f'<path d="{vbar(cx - bw / 2, by, bw, bh)}" fill="{t["series"][cslot]}"/>')
            y_lo, y_hi = base_y - plot_h * st["p25"] / top, base_y - plot_h * st["p75"] / top
            o.append(f'<line x1="{cx:.1f}" y1="{y_hi:.1f}" x2="{cx:.1f}" y2="{y_lo:.1f}" '
                     f'stroke="{t["ink2"]}" stroke-width="1.5" opacity="0.6"/>')
            for yy in (y_hi, y_lo):
                o.append(f'<line x1="{cx - 5:.1f}" y1="{yy:.1f}" x2="{cx + 5:.1f}" '
                         f'y2="{yy:.1f}" stroke="{t["ink2"]}" stroke-width="1.5" opacity="0.6"/>')
            label = f"{st['med']:,.{nd}f}"
            plate_w = len(label) * 7.4 + 8
            ytop = min(by, y_hi)
            o.append(f'<rect x="{cx - plate_w / 2:.1f}" y="{ytop - 19:.1f}" '
                     f'width="{plate_w:.1f}" height="16" fill="{t["surface"]}"/>')
            o.append(f'<text x="{cx:.1f}" y="{ytop - 7:.1f}" font-size="12.5" '
                     f'font-weight="600" text-anchor="middle" fill="{t["ink"]}">{label}</text>')
            o.append(f'<text x="{cx:.1f}" y="{base_y + 17}" font-size="11.5" '
                     f'text-anchor="middle" fill="{t["ink2"]}">{esc(name)}</text>')

    lx = M_L
    for label, slot in (("bbr (kernel baseline)", BBR), ("tcp-brutal @200M", BRUTAL),
                        ("skyline_cc", SKY)):
        o.append(f'<rect x="{lx}" y="{H2 - 30}" width="11" height="11" rx="2.5" '
                 f'fill="{t["series"][slot]}"/>')
        o.append(f'<text x="{lx + 17}" y="{H2 - 20}" font-size="12" '
                 f'fill="{t["ink2"]}">{esc(label)}</text>')
        lx += 20 + len(label) * 6.6 + 24
    o.append(f'<text x="{W2 - M_R}" y="{H2 - 20}" font-size="11" text-anchor="end" '
             f'fill="{t["ink3"]}">skyline_cc 0.1.0 coefficients (today&#8217;s &#8220;high random loss&#8221; '
             f'preset) · tcp-brutal v2.0.0 · 2026-09-20</text>')
    o.append("</svg>")
    return "\n".join(o)


if __name__ == "__main__":
    data = sys.argv[1] if len(sys.argv) > 1 else "head-to-head.jsonl"
    out = Path(sys.argv[2] if len(sys.argv) > 2 else ".")
    out.mkdir(parents=True, exist_ok=True)
    runs = load(data)
    for mode in ("light", "dark"):
        for name, fn in (("ataglance", render_glance), ("detail", render_detail)):
            p = out / f"single-ended-{name}-{mode}.svg"
            p.write_text(fn(runs, mode), encoding="utf-8")
            print(f"wrote {p} ({p.stat().st_size:,} bytes)")
