#!/usr/bin/env python3
"""HouyiDB benchmark HTTP: sustained small-batch upsert latency (incremental smoke).

用法 / Usage:
  INCREMENTAL_BENCH_BASE_URL=http://127.0.0.1:17000 \\
  python3 scripts/incremental_bench_http.py --dim 1536 --duration 20 --rps 15

输出 JSON 一行到 stdout，供 perf_regression_gate 解析。
Prints one JSON line to stdout for gate script.
"""
from __future__ import annotations

import argparse
import json
import random
import sys
import time
import urllib.error
import urllib.request


def _post_json(url: str, path: str, body: object, timeout: float) -> tuple[int, float, str]:
    t0 = time.perf_counter()
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        url.rstrip("/") + path,
        data=data,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            _ = r.read()
            ms = (time.perf_counter() - t0) * 1000.0
            return r.status, ms, "ok"
    except urllib.error.HTTPError as e:
        body_err = e.read().decode("utf-8", errors="replace")
        ms = (time.perf_counter() - t0) * 1000.0
        return e.code, ms, body_err


def _get_json(url: str, path: str, timeout: float) -> dict:
    req = urllib.request.Request(url.rstrip("/") + path, method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def _percentile(sorted_vals: list[float], p: float) -> float:
    if not sorted_vals:
        return 0.0
    n = len(sorted_vals)
    if n == 1:
        return sorted_vals[0]
    k = (n - 1) * p / 100.0
    f = int(k)
    c = min(f + 1, n - 1)
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:17000")
    ap.add_argument("--dim", type=int, default=128)
    ap.add_argument("--duration", type=float, default=15.0, help="Total seconds to sustain RPS / 总时长秒")
    ap.add_argument("--rps", type=float, default=20.0, help="Target requests per second (sequential) / 目标 RPS")
    ap.add_argument("--batch", type=int, default=1, help="Vectors per upsert / 每请求向量条数")
    ap.add_argument("--timeout", type=float, default=120.0, help="HTTP timeout per request / 单次请求秒")
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--id-base", type=int, default=9_000_000_000)
    ap.add_argument("--m", type=int, default=16)
    ap.add_argument("--ef-construction", type=int, default=64)
    args = ap.parse_args()
    rng = random.Random(args.seed)
    base = args.base_url.rstrip("/")
    n_req = max(1, int(args.duration * args.rps))
    interval = 1.0 / args.rps
    t_start = time.perf_counter()
    latencies: list[float] = []
    errors = 0
    for i in range(n_req):
        next_slot = t_start + (i + 1) * interval
        batch = []
        for b in range(args.batch):
            vid = args.id_base + i * args.batch + b
            vec = [rng.random() for _ in range(args.dim)]
            batch.append({"id": vid, "values": vec})
        body = {
            "vectors": batch,
            "index": {"m": args.m, "ef_construction": args.ef_construction},
        }
        code, ms, _ = _post_json(base, "/v1/vectors/upsert", body, args.timeout)
        if code == 200:
            latencies.append(ms)
        else:
            errors += 1
        now = time.perf_counter()
        if now < next_slot:
            time.sleep(next_slot - now)

    status: dict = {}
    try:
        status = _get_json(base, "/v1/index/status", args.timeout)
    except OSError as e:
        status = {"error": str(e)}

    latencies.sort()
    p50 = _percentile(latencies, 50)
    p95 = _percentile(latencies, 95)
    p99 = _percentile(latencies, 99)
    out = {
        "ok": errors == 0 and bool(latencies),
        "requests": n_req,
        "errors": errors,
        "upsert_p50_ms": round(p50, 3),
        "upsert_p95_ms": round(p95, 3),
        "upsert_p99_ms": round(p99, 3),
        "pending_count": status.get("pending_count"),
        "max_pending_queue": status.get("max_pending_queue"),
        "hot_count": status.get("hot_count"),
        "indexed_count": status.get("indexed_count"),
    }
    print(json.dumps(out, ensure_ascii=False))
    return 0 if (errors == 0 and latencies) else 1


if __name__ == "__main__":
    sys.exit(main())
