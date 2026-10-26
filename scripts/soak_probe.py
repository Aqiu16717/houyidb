#!/usr/bin/env python3
"""soak 终检：精确检索抽样（P4.5.8）。

对账本里每个集合一个**仍存活**的确定成功写，按 payload `row=id` 过滤做单点
精确检索（掩码路径恒为暴力扫描，不受 ANN 近似影响），必须命中该 id。
EN: exact point retrieval for a still-live acked id of each collection — the
mask path is brute force, so a miss is data loss, not an approximation artifact.

为什么不能直接取「最后一次 ok upsert」（WHY）：负载随后可能删除同一个 id
（删除是从已发 id 里均匀随机的），取到被删 id 会让探针在健康系统上误报
数据丢失。这里按与 `soak_ledger.py` 相同的语义重放账本，并且**排除任何
出现过删除记录（ok 或 info）的 id**——info 删除的结果不确定，采样它同样
可能误报。
EN: replay the ledger with the same live-set semantics as `soak_ledger.py`
and exclude ids with any delete record (ok or info), since an indeterminate
delete may legitimately have removed the id.

判定用显式 raise 而非 `assert`：`python -O` 会剥离 assert，哨兵会静默空转。
EN: explicit raises, not asserts — `python -O` strips asserts and would turn
this sentinel into a no-op.

用法 / Usage:
  python3 scripts/soak_probe.py ledger.jsonl http://127.0.0.1:17911
"""

import json
import sys
import urllib.error
import urllib.request
from typing import NoReturn


def fail(msg: str) -> NoReturn:
    print(f"!! soak_probe: {msg}", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: soak_probe.py <ledger.jsonl> <http-base>")
    ledger, base = sys.argv[1], sys.argv[2]
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    live: dict = {}
    touched_by_delete: set = set()
    for line in open(ledger):
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        key = (rec["coll"], rec["id"])
        if rec["op"] == "upsert":
            if rec["status"] == "ok":
                live.setdefault(rec["coll"], []).append(rec["id"])
        elif rec["op"] == "delete":
            # ok 删除确定移除；info 删除结果不确定——两者都不再适合采样。
            # EN: ok deletes are definitely removed; info deletes are indeterminate — neither is
            # EN: suitable for sampling anymore.
            touched_by_delete.add(key)
            if rec["status"] == "ok":
                live[rec["coll"]] = [
                    i for i in live.get(rec["coll"], []) if i != rec["id"]
                ]

    sample: dict = {}
    for coll, ids in live.items():
        candidates = [i for i in ids if (coll, i) not in touched_by_delete]
        if candidates:
            sample[coll] = candidates[-1]

    if not sample:
        # 没有任何可验证样本 = 探针没有执行任何检查，必须响亮失败而不是打印通过。
        # EN: No verifiable sample at all = the probe performed no checks; it must fail loudly
        # EN: instead of printing pass.
        fail("账本里没有仍存活的确定成功写，精确检索抽样无样本可验证")

    checked = 0
    for coll, rid in sample.items():
        body = {
            "vector": [1.0, rid % 7 + 1, rid % 5 + 1, rid % 3 + 1, rid % 11 + 1],
            "k": 1,
            "filters": [{"column": "row", "op": "eq", "value": rid}],
        }
        req = urllib.request.Request(
            f"{base}/v1/collections/{coll}/hybrid/search",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        try:
            with opener.open(req, timeout=15) as resp:
                ids = json.loads(resp.read()).get("ids", [])
        except (urllib.error.URLError, ValueError) as e:
            fail(f"{coll} id={rid} 检索失败: {e}")
        if ids != [rid]:
            fail(f"精确检索失败 {coll} id={rid}: {ids}")
        print(f"    {coll} id={rid} 精确命中 ✓")
        checked += 1
    if checked == 0:
        fail("抽样集为空，未执行任何检查")
    print(f"精确检索抽样通过（{checked} 个集合）")


if __name__ == "__main__":
    main()
