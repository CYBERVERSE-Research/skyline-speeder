#!/usr/bin/env python3
"""Client-side scenario harness for comparing server-side (single-ended) TCP
acceleration. Every metric here is what the *client* experiences, because that
is what "acceleration" has to mean; server-side counters are collected
separately for diagnosis.

All four scenarios open fresh TCP connections on every run: nginx keep-alive
would otherwise let a run inherit a connection established under the previous
algorithm, which silently attributes one algorithm's numbers to another (a
connection keeps the congestion control it was created with).
"""
import argparse, http.client, json, os, socket, statistics, struct, sys, threading, time

# Address of the server being accelerated. Set SKYLINE_BENCH_HOST; the
# placeholder is deliberately not a real address so an unconfigured run
# fails loudly instead of measuring somebody else's box.
HOST = os.environ.get("SKYLINE_BENCH_HOST", "203.0.113.1")
HTTP_PORT = int(os.environ.get("SKYLINE_BENCH_HTTP_PORT", 8080))
GAME_PORT = int(os.environ.get("SKYLINE_BENCH_GAME_PORT", 9999))


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize(name, values):
    if not values:
        return {}
    return {
        f"{name}_n": len(values),
        f"{name}_min": round(min(values), 2),
        f"{name}_p50": round(pct(values, 50), 2),
        f"{name}_p95": round(pct(values, 95), 2),
        f"{name}_p99": round(pct(values, 99), 2),
        f"{name}_max": round(max(values), 2),
        f"{name}_mean": round(statistics.fmean(values), 2),
    }


def fetch(conn, path, rng=None):
    """One HTTP GET on an existing connection; returns (ttfb_ms, total_ms, nbytes)."""
    headers = {"Host": HOST}
    if rng:
        headers["Range"] = f"bytes={rng[0]}-{rng[1]}"
    t0 = time.perf_counter()
    conn.request("GET", path, headers=headers)
    resp = conn.getresponse()
    first = resp.read(1)
    t_first = time.perf_counter()
    n = len(first)
    while True:
        b = resp.read(1 << 16)
        if not b:
            break
        n += len(b)
    t_end = time.perf_counter()
    return (t_first - t0) * 1000, (t_end - t0) * 1000, n


# ---------------------------------------------------------------- game
def run_game(args):
    """Fixed-rate ping-pong. Latency tail is the metric: a game is unplayable
    because of its worst 1% of frames, not its average."""
    s = socket.create_connection((HOST, GAME_PORT), timeout=20)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    sent_at, rtts, lock = {}, [], threading.Lock()
    stop = threading.Event()

    def reader():
        buf = b""
        while not stop.is_set():
            try:
                b = s.recv(65536)
            except OSError:
                return
            if not b:
                return
            buf += b
            while len(buf) >= 512:
                seq = struct.unpack("!Q", buf[:8])[0]
                buf = buf[512:]
                now = time.perf_counter()
                with lock:
                    t0 = sent_at.pop(seq, None)
                if t0 is not None:
                    rtts.append((now - t0) * 1000)

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    interval = 1.0 / args.rate
    start = time.perf_counter()
    seq = 0
    while time.perf_counter() - start < args.duration:
        target = start + seq * interval
        d = target - time.perf_counter()
        if d > 0:
            time.sleep(d)
        with lock:
            sent_at[seq] = time.perf_counter()
        try:
            s.sendall(struct.pack("!Q", seq) + b"\x00" * 56)
        except OSError:
            break
        seq += 1
    time.sleep(2.0)
    stop.set()
    try:
        s.close()
    except OSError:
        pass
    lost = seq - len(rtts)
    out = {"sent": seq, "replies": len(rtts), "lost_or_late": lost,
           "lost_pct": round(100.0 * lost / max(seq, 1), 2)}
    out.update(summarize("rtt_ms", rtts))
    if rtts:
        base = min(rtts)
        # Frames arriving more than 50ms/150ms past the best-case RTT: the
        # visible stutters, which averages hide completely.
        out["stall_50ms"] = sum(1 for r in rtts if r > base + 50)
        out["stall_150ms"] = sum(1 for r in rtts if r > base + 150)
        out["jitter_ms"] = round(statistics.fmean(
            abs(b - a) for a, b in zip(rtts, rtts[1:])), 2) if len(rtts) > 1 else 0.0
    return out


# ---------------------------------------------------------------- web
PAGE = ["/10k.bin"] + ["/50k.bin"] * 12 + ["/100k.bin"] * 10 + ["/10k.bin"] * 8


def run_web(args):
    """Whole-page load over 6 parallel keep-alive connections, the way a browser
    does it. Short flows never leave slow start, so this measures how an
    algorithm opens up, not how it behaves in steady state."""
    loads, ttfbs = [], []
    for _ in range(args.repeats):
        conns = [http.client.HTTPConnection(HOST, HTTP_PORT, timeout=30) for _ in range(6)]
        for c in conns:
            c.connect()
            c.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        results, errs = [], []
        t0 = time.perf_counter()
        # index first (a browser cannot know the assets before it parses the HTML)
        tf, _, _ = fetch(conns[0], PAGE[0])
        ttfbs.append(tf)
        assets = PAGE[1:]

        def worker(conn, paths):
            try:
                for p in paths:
                    tf, tt, _ = fetch(conn, p)
                    results.append((tf, tt))
            except Exception as e:  # noqa: BLE001 - recorded, not swallowed
                errs.append(repr(e))

        threads = []
        for i, c in enumerate(conns):
            paths = assets[i::len(conns)]
            t = threading.Thread(target=worker, args=(c, paths))
            t.start()
            threads.append(t)
        for t in threads:
            t.join()
        loads.append((time.perf_counter() - t0) * 1000)
        ttfbs.extend(r[0] for r in results)
        for c in conns:
            c.close()
        if errs:
            return {"error": errs[:3]}
        time.sleep(1.0)
    out = {"page_bytes": 10240 + 12 * 51200 + 10 * 102400 + 8 * 10240, "objects": len(PAGE)}
    out.update(summarize("pageload_ms", loads))
    out.update(summarize("ttfb_ms", ttfbs))
    return out


# ---------------------------------------------------------------- video
def run_video(args):
    """ABR player model: ask for the next chunk on a fixed wall-clock cadence and
    track the playback buffer. A stream does not break because throughput is low
    on average, it breaks because one chunk arrived after the buffer ran dry."""
    chunk = args.chunk_mb * 1024 * 1024
    period = args.period          # seconds of video per chunk == request cadence
    conn = http.client.HTTPConnection(HOST, HTTP_PORT, timeout=60)
    conn.connect()
    times, buf_levels = [], []
    buffer_s = 0.0
    rebuffers, rebuffer_time = 0, 0.0
    start = time.perf_counter()
    startup_ms = None
    for i in range(args.chunks):
        want = start + i * period
        d = want - time.perf_counter()
        if d > 0:
            time.sleep(d)
            buffer_s = max(0.0, buffer_s - d)
        t0 = time.perf_counter()
        off = (i * chunk) % (200 * 1024 * 1024 - chunk)
        try:
            _, tt, n = fetch(conn, "/bulk200m.bin", (off, off + chunk - 1))
        except Exception as e:  # noqa: BLE001
            return {"error": repr(e), "completed": i}
        dl = time.perf_counter() - t0
        times.append(dl * 1000)
        if startup_ms is None:
            # The first chunk arrives into an empty buffer by definition. That is
            # startup delay, which is reported on its own -- counting it as a
            # rebuffer would charge every algorithm one phantom stall.
            startup_ms = dl * 1000
            buffer_s = period
            buf_levels.append(buffer_s)
            continue
        if dl > buffer_s:
            rebuffers += 1
            rebuffer_time += dl - buffer_s
            buffer_s = 0.0
        else:
            buffer_s -= dl
        buffer_s += period
        buf_levels.append(buffer_s)
    conn.close()
    out = {
        "bitrate_mbps": round(args.chunk_mb * 8 / period, 1),
        "chunks": args.chunks,
        "startup_ms": round(startup_ms, 1) if startup_ms else None,
        "rebuffer_events": rebuffers,
        "rebuffer_seconds": round(rebuffer_time, 2),
        "min_buffer_s": round(min(buf_levels), 2) if buf_levels else None,
    }
    out.update(summarize("chunk_ms", times))
    return out


# ---------------------------------------------------------------- bulk
def run_bulk(args):
    """Steady-state goodput, single stream and parallel."""
    total = args.mb * 1024 * 1024
    out = {"mb": args.mb, "streams": args.streams}
    per = total // args.streams
    got, errs = [], []
    # --deadline bounds the run. A congestion control that has collapsed on a
    # lossy path can take tens of minutes to move 100MB, and an unbounded read
    # turns that into a hung benchmark instead of a measurement. On a timeout
    # the bytes actually delivered are still reported, with completed=False, so
    # a collapse is recorded as the low number it is rather than as missing data.
    deadline = time.perf_counter() + args.deadline if args.deadline else None
    timed_out = []

    def worker(idx):
        try:
            c = http.client.HTTPConnection(HOST, HTTP_PORT, timeout=120)
            c.connect()
            off = idx * per
            t0 = time.perf_counter()
            c.request("GET", "/bulk200m.bin",
                      headers={"Host": HOST, "Range": f"bytes={off}-{off + per - 1}"})
            resp = c.getresponse()
            n = 0
            while True:
                if deadline and time.perf_counter() > deadline:
                    timed_out.append(idx)
                    break
                b = resp.read(1 << 16)
                if not b:
                    break
                n += len(b)
            got.append((n, time.perf_counter() - t0))
            try:
                c.close()
            except OSError:
                pass
        except Exception as e:  # noqa: BLE001
            errs.append(repr(e))

    t0 = time.perf_counter()
    threads = [threading.Thread(target=worker, args=(i,)) for i in range(args.streams)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t0
    if errs:
        return {"error": errs[:3]}
    nbytes = sum(n for n, _ in got)
    out["completed"] = not timed_out and nbytes >= total * 0.999
    out["wall_s"] = round(wall, 3)
    out["bytes"] = nbytes
    out["goodput_mbps"] = round(nbytes * 8 / wall / 1e6, 1)
    out.update(summarize("stream_s", [round(d, 3) for _, d in got]))
    return out


SCENARIOS = {"game": run_game, "web": run_web, "video": run_video, "bulk": run_bulk}

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario", choices=SCENARIOS)
    ap.add_argument("--label", required=True)
    ap.add_argument("--rep", type=int, default=0)
    ap.add_argument("--duration", type=float, default=40)
    ap.add_argument("--rate", type=float, default=60)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--chunks", type=int, default=40)
    ap.add_argument("--chunk-mb", type=int, default=4)
    ap.add_argument("--period", type=float, default=1.0)
    ap.add_argument("--mb", type=int, default=150)
    ap.add_argument("--streams", type=int, default=1)
    ap.add_argument("--deadline", type=float, default=0,
                    help="seconds; 0 = unbounded")
    a = ap.parse_args()
    rec = {"scenario": a.scenario, "algo": a.label, "rep": a.rep, "ts": time.time()}
    try:
        rec.update(SCENARIOS[a.scenario](a))
    except Exception as e:  # noqa: BLE001
        rec["error"] = repr(e)
    print(json.dumps(rec), flush=True)
