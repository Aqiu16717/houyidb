#!/usr/bin/env python3
"""Read thresholds + compare VDB or incremental JSON. Exit 1 = regression / gate fail.

VDB 阈值环境覆盖：GATE_VDB_MIN_RECALL, GATE_VDB_MIN_NDCG, GATE_VDB_MAX_OPTIMIZE_SEC（0=不测上界）, GATE_VDB_MIN_QPS（0=不测）
INCREMENTAL：GATE_INC_MAX_UPSERT_P99_MS, GATE_INC_MAX_PENDING（0=不测）
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
THR_PATH = ROOT / "perf_regression_thresholds.json"


def _load_thresholds() -> dict:
    with open(THR_PATH, encoding="utf-8") as f:
        return json.load(f)


def _env_float(key: str, default: float) -> float:
    v = os.environ.get(key)
    if v is None or v == "":
        return default
    return float(v)


def check_vdb(result_json: Path) -> int:
    t = _load_thresholds()["vdb"]
    min_r = _env_float("GATE_VDB_MIN_RECALL", float(t["min_recall"]))
    min_n = _env_float("GATE_VDB_MIN_NDCG", float(t["min_ndcg"]))
    max_opt = _env_float("GATE_VDB_MAX_OPTIMIZE_SEC", float(t["max_optimize_sec"]))
    min_q = _env_float("GATE_VDB_MIN_QPS", float(t.get("min_qps", 0.0)))
    with open(result_json, encoding="utf-8") as f:
        d = json.load(f)
    m = (d.get("results") or [{}])[0].get("metrics") or {}
    recall = m.get("recall")
    ndcg = m.get("ndcg")
    opt = m.get("optimize_duration")
    qps = m.get("qps")
    failed: list[str] = []
    if recall is not None and float(recall) < min_r:
        failed.append(f"recall {recall} < {min_r}")
    if ndcg is not None and float(ndcg) < min_n:
        failed.append(f"ndcg {ndcg} < {min_n}")
    if opt is not None and max_opt > 0 and float(opt) > max_opt:
        failed.append(f"optimize_duration {opt}s > {max_opt}s (set GATE_VDB_MAX_OPTIMIZE_SEC=0 to skip)")
    if qps is not None and min_q > 0 and float(qps) < min_q:
        failed.append(f"qps {qps} < {min_q} (set GATE_VDB_MIN_QPS=0 to skip)")
    if failed:
        print("VDB gate FAIL:", "; ".join(failed), file=sys.stderr)
        return 1
    print(
        f"VDB gate OK: recall={recall} ndcg={ndcg} optimize={opt}s qps={qps} file={result_json}",
        file=sys.stderr,
    )
    return 0


def check_incremental(metrics: dict) -> int:
    t = _load_thresholds()["incremental"]
    max_p99 = _env_float("GATE_INC_MAX_UPSERT_P99_MS", float(t["max_upsert_p99_ms"]))
    max_pend = int(_env_float("GATE_INC_MAX_PENDING", float(t["max_pending_count"])))
    p99 = metrics.get("upsert_p99_ms")
    pending = metrics.get("pending_count")
    failed: list[str] = []
    if not metrics.get("ok"):
        failed.append("incremental bench ok=false or empty latencies")
    if p99 is not None and max_p99 > 0 and float(p99) > max_p99:
        failed.append(f"upsert_p99_ms {p99} > {max_p99}")
    if (
        pending is not None
        and max_pend > 0
        and int(pending) > max_pend
    ):
        failed.append(f"pending_count {pending} > {max_pend} (set GATE_INC_MAX_PENDING=0 to skip)")
    if failed:
        print("INCREMENTAL gate FAIL:", "; ".join(failed), file=sys.stderr)
        return 1
    print(f"INCREMENTAL gate OK: {json.dumps(metrics, ensure_ascii=False)}", file=sys.stderr)
    return 0


def main() -> int:
    if len(sys.argv) < 2:
        print("Usage: vdb <result.json> | inc -", file=sys.stderr)
        return 2
    sub = sys.argv[1]
    if sub == "vdb":
        if len(sys.argv) < 3:
            return 2
        return check_vdb(Path(sys.argv[2]))
    if sub == "inc":
        if len(sys.argv) >= 3 and sys.argv[2] != "-":
            m = json.load(open(sys.argv[2], encoding="utf-8"))
        else:
            m = json.load(sys.stdin)
        return check_incremental(m)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
