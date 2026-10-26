#!/usr/bin/env python3
"""soak 账本 → 每集合的计数区间（P4.5.8）。

为什么是区间而不是精确值（WHY）：故障窗口内的操作结果**不确定**（客户端只
看到连接断，服务端可能已应用）。确定成功的写/删进 live 集合；不确定的写
（可能生效）抬上界、不确定的删（可能生效）压下界。精确相等会把「已应用但
响应丢失」的写误判成幽灵数据。
EN: indeterminate outcomes (connection lost mid-request) may or may not have
been applied; the live counter is therefore bounded, not exact.

用法 / Usage:
  python3 scripts/soak_ledger.py ledger.jsonl expected.json
"""

import json
import sys


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: soak_ledger.py <ledger.jsonl> <expected.json>")
    ledger, out = sys.argv[1], sys.argv[2]
    live: dict = {}
    maybe_up: dict = {}
    maybe_del: dict = {}
    for line in open(ledger):
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        key = (rec["coll"], rec["id"])
        if rec["op"] == "upsert":
            if rec["status"] == "ok":
                live[key] = True
            else:
                maybe_up[key] = True
        elif rec["op"] == "delete":
            if rec["status"] == "ok":
                live.pop(key, None)
            else:
                maybe_del[key] = True

    per: dict = {}
    for coll, _id in live:
        per.setdefault(coll, {"live": 0, "maybe_up": 0, "maybe_del": 0})["live"] += 1
    for coll, _id in maybe_up:
        per.setdefault(coll, {"live": 0, "maybe_up": 0, "maybe_del": 0})["maybe_up"] += 1
    for coll, _id in maybe_del:
        per.setdefault(coll, {"live": 0, "maybe_up": 0, "maybe_del": 0})["maybe_del"] += 1

    with open(out, "w") as f:
        json.dump(per, f)
    print(json.dumps(per))


if __name__ == "__main__":
    main()
