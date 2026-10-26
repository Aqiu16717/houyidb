#!/usr/bin/env python3
"""Compare two VectorDBBench result JSON files (e.g. QdrantLocal vs HouyiDB).

用法 / Usage:
  python3 scripts/vdbbench_compare_results.py Q.json A.json
  python3 scripts/vdbbench_compare_results.py a.json b.json \\
    --min-recall 0.998 --min-ndcg 0.998
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def _load(p: Path) -> dict[str, Any]:
    with p.open(encoding="utf-8") as f:
        return json.load(f)


def _first_case_metrics(data: dict[str, Any]) -> dict[str, Any]:
    results = data.get("results") or []
    if not results:
        raise ValueError("empty results")
    m = results[0].get("metrics")
    if not isinstance(m, dict):
        raise ValueError("missing metrics")
    return m


def _fmt(x: Any) -> str:
    if x is None:
        return "-"
    if isinstance(x, float):
        return f"{x:.6g}"
    return str(x)


def main() -> int:
    desc = "Compare two VectorDBBench result JSON files."
    ap = argparse.ArgumentParser(description=desc)
    ap.add_argument(
        "left_json",
        type=Path,
        help="First result JSON (e.g. QdrantLocal)",
    )
    ap.add_argument(
        "right_json",
        type=Path,
        help="Second result JSON (e.g. HouyiDB)",
    )
    ap.add_argument(
        "--min-recall",
        type=float,
        default=None,
        help="If set, exit 2 when either recall is below this.",
    )
    ap.add_argument(
        "--min-ndcg",
        type=float,
        default=None,
        help="If set, exit 2 when either ndcg is below this.",
    )
    ap.add_argument(
        "--require-right-ge-left",
        action="store_true",
        help=(
            "Exit 2 when right recall or ndcg is strictly less than left "
            "(e.g. HouyiDB vs Qdrant)."
        ),
    )
    args = ap.parse_args()

    left = _load(args.left_json)
    right = _load(args.right_json)
    ml = _first_case_metrics(left)
    mr = _first_case_metrics(right)

    keys = [
        ("recall", "recall"),
        ("ndcg", "ndcg"),
        ("load_duration", "load_duration"),
        ("qps", "qps"),
        ("serial_latency_p99", "serial_latency_p99"),
        ("serial_latency_p95", "serial_latency_p95"),
    ]

    print(f"left : {args.left_json}")
    print(f"right: {args.right_json}")
    print(f"{'metric':<22} {'left':>14} {'right':>14}")
    for k, _ in keys:
        print(f"{k:<22} {_fmt(ml.get(k)):>14} {_fmt(mr.get(k)):>14}")

    r_l, r_r = ml.get("recall"), mr.get("recall")
    n_l, n_r = ml.get("ndcg"), mr.get("ndcg")

    if args.min_recall is not None:
        for name, v in (("left", r_l), ("right", r_r)):
            if v is not None and float(v) < args.min_recall:
                msg = f"GATE FAIL: {name} recall {v} < {args.min_recall}"
                print(msg, file=sys.stderr)
                return 2
    if args.min_ndcg is not None:
        for name, v in (("left", n_l), ("right", n_r)):
            if v is not None and float(v) < args.min_ndcg:
                msg = f"GATE FAIL: {name} ndcg {v} < {args.min_ndcg}"
                print(msg, file=sys.stderr)
                return 2

    if args.require_right_ge_left:
        if r_l is not None and r_r is not None and float(r_r) < float(r_l):
            msg = f"GATE FAIL: right recall {r_r} < left {r_l}"
            print(msg, file=sys.stderr)
            return 2
        if n_l is not None and n_r is not None and float(n_r) < float(n_l):
            msg = f"GATE FAIL: right ndcg {n_r} < left {n_l}"
            print(msg, file=sys.stderr)
            return 2

    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
