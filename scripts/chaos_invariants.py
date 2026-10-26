#!/usr/bin/env python3
"""崩溃一致性不变量校验器（P4.5.4）。
Crash-consistency invariant checker (P4.5.4).

重启后对**落盘工件**与**服务行为**做双向核对，不信任服务自报的「我恢复了」：

1. WAL 结构：v2 文件头 + 每条记录的 CRC32；撕裂尾容忍（与内核 end-of-WAL
   语义一致），但「尾部之后还有一条 CRC 完好的记录」判定为**中间腐坏**。
2. collections WAL 重放：把 create/upsert/delete 应用到内存模型，得到每个
   collection 的存活 id 集合——这是「WAL 完整可重放」的直接证据。
3. 图快照：魔数/版本/长度/CRC32；checkpoint_lsn 与 WAL 水位线交叉核对。
4. 计数守恒：`/v1/collections/<name>/status` 的 indexed+pending 必须等于重放
   得到的存活集合大小。
5. RPO=0：写侧账本里**已确认（HTTP 200）**的 id 必须全部可查；同时不得出现
   「从未发送过」的幽灵 id（`live ⊆ intended`）。
6. 抽样 top-1：对若干 id 用其确定性向量检索，top-1 必须是它自己（精确向量距离
   为 0，任何近似后端都不得把它挤掉）。

EN: Cross-checks on-disk artifacts and live behavior after restart: WAL structure
(v2 header + per-record CRC32, torn tail tolerated, mid-log corruption rejected),
collection replay (create/upsert/delete → live id set), graph snapshot CRC, count
conservation against the status endpoint, RPO=0 against the writer ledger, and
sampled top-1 exactness.

用法 / Usage:
  python3 scripts/chaos_invariants.py --data-dir DIR --http 127.0.0.1:17871 \
      --wal-name chaos.wal --acked-file acked.txt --intended-file intended.txt
  python3 scripts/chaos_invariants.py --selftest
"""

import argparse
import json
import os
import struct
import sys
import urllib.error
import urllib.request
import zlib

WAL_MAGIC = b"HOUYIWAL"
WAL_VERSION = 2
WAL_HEADER_LEN = 12
# 记录头：lsn u64 + len u32 + crc32 u32
# EN: Record header: lsn u64 + len u32 + crc32 u32
REC_HEADER_LEN = 16

GRAPH_MAGIC = b"HOUYIGRAPH"
GRAPH_VERSION = 1
GRAPH_HEADER_LEN = len(GRAPH_MAGIC) + 4 + 8 + 8 + 4


class CheckError(AssertionError):
    pass


def require(cond, msg):
    if not cond:
        raise CheckError(msg)


# ── WAL 解析 ────────────────────────────────────────────────────────────
# EN: -- WAL parsing --

def parse_wal(path):
    """解析一个 v2 WAL 文件（含撕裂尾容忍）。

    返回 (entries, info)：
      entries = [(lsn, payload)]
      info = {"valid_prefix": int, "torn_tail": bool, "mid_corruption": str|None}
    文件不存在 → ([], info with missing=True)。
    """
    info = {"valid_prefix": 0, "torn_tail": False, "mid_corruption": None, "missing": False}
    if not os.path.exists(path):
        info["missing"] = True
        return [], info
    data = open(path, "rb").read()
    require(
        len(data) >= WAL_HEADER_LEN,
        f"{path}: file shorter than the v2 header ({len(data)} bytes)",
    )
    require(data[:8] == WAL_MAGIC, f"{path}: bad WAL magic {data[:8]!r}")
    version = struct.unpack(">I", data[8:12])[0]
    require(version == WAL_VERSION, f"{path}: unsupported WAL version {version}")

    entries = []
    off = WAL_HEADER_LEN
    torn = False
    while off < len(data):
        if len(data) - off < REC_HEADER_LEN:
            torn = True
            break
        lsn, plen, crc = struct.unpack(">QII", data[off:off + REC_HEADER_LEN])
        end = off + REC_HEADER_LEN + plen
        if end > len(data):
            torn = True
            break
        payload = data[off + REC_HEADER_LEN:end]
        calc = zlib.crc32(struct.pack(">QI", lsn, plen) + payload) & 0xFFFFFFFF
        if calc != crc:
            # 停在 CRC 失败处（与内核口径一致）；但若其后还能解析出一条 CRC 完好
            # 的记录，说明腐坏发生在日志中间而不是尾部。
            # EN: Stop at the CRC failure (same as the kernel); but if a CRC-intact record can
            # EN: still be parsed after it, the corruption is mid-log, not at the tail.
            nxt = _find_valid_record(data, off + REC_HEADER_LEN)
            if nxt is not None:
                info["mid_corruption"] = (
                    f"{path}: CRC mismatch at offset {off} (lsn {lsn}), "
                    f"but a valid record follows at offset {nxt}"
                )
            torn = True
            break
        entries.append((lsn, payload))
        off = end

    info["valid_prefix"] = off
    info["torn_tail"] = torn
    return entries, info


def _find_valid_record(data, start):
    """在 `start` 之后找一个 CRC 完好的记录起点（用于识别中间腐坏）。
    EN: Find a CRC-intact record start after `start` (used to detect mid-log
    EN: corruption).
    """
    for probe in range(start, len(data) - REC_HEADER_LEN + 1):
        lsn, plen, crc = struct.unpack(">QII", data[probe:probe + REC_HEADER_LEN])
        end = probe + REC_HEADER_LEN + plen
        if end > len(data):
            continue
        payload = data[probe + REC_HEADER_LEN:end]
        if (zlib.crc32(struct.pack(">QI", lsn, plen) + payload) & 0xFFFFFFFF) == crc:
            return probe
    return None


def wal_files(data_dir, wal_name):
    """归档段（按名排序，先于活动文件）+ 活动 WAL。
    EN: Archived segments (name-sorted, before the active file) + the active WAL.
    """
    stem = os.path.splitext(wal_name)[0]
    marker = f"{stem}.wal.archived."
    archived = sorted(
        os.path.join(data_dir, n)
        for n in os.listdir(data_dir)
        if n.startswith(marker)
    )
    return archived + [os.path.join(data_dir, wal_name)]


def load_all_wal(data_dir, wal_name):
    entries = []
    infos = []
    for p in wal_files(data_dir, wal_name):
        e, info = parse_wal(p)
        require(info["mid_corruption"] is None, info["mid_corruption"])
        entries.extend(e)
        infos.append((p, info))
    # LSN 严格递增（重复/回退是复制 resume 语义的真故障）。
    # EN: LSNs are strictly increasing (duplicate/rollback is a real fault in replication
    # EN: resume semantics).
    for (a, _), (b, _) in zip(entries, entries[1:]):
        require(b > a, f"WAL LSN not strictly increasing: {a} then {b}")
    return entries, infos


# ── collections WAL 重放 ────────────────────────────────────────────────
# EN: -- collections WAL replay --

def replay_collections(entries):
    """把 collections WAL 记录重放成 {name: set(ids)}。未知 op 跳过（与内核一致）。
    EN: Replay collections WAL records into {name: set(ids)}. Unknown ops are
    EN: skipped (same as the kernel).
    """
    live = {}
    for lsn, payload in entries:
        try:
            rec = json.loads(payload.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            raise CheckError(f"collection WAL record lsn={lsn} is not JSON: {e}")
        if rec.get("v") != 1:
            continue
        op = rec.get("op")
        name = rec.get("name")
        if op == "collection_create":
            live.setdefault(name, set())
        elif op == "collection_upsert_batch":
            live.setdefault(name, set()).update(item["id"] for item in rec.get("items", []))
        elif op == "collection_delete":
            live.setdefault(name, set()).difference_update(rec.get("ids", []))
        # set_ha_level / 未知 op：不影响存活集合。
        # EN: set_ha_level / unknown op: does not affect live collections.
    return live


# ── 图快照 ──────────────────────────────────────────────────────────────
# EN: -- graph snapshot --

def check_graph_snapshot(path):
    """校验快照文件结构；返回 checkpoint_lsn。文件不存在 → None。
    EN: Validate the snapshot file structure; return checkpoint_lsn. Missing
    EN: file -> None.
    """
    if not os.path.exists(path):
        return None
    data = open(path, "rb").read()
    require(len(data) >= GRAPH_HEADER_LEN, f"{path}: shorter than the snapshot header")
    require(data[: len(GRAPH_MAGIC)] == GRAPH_MAGIC, f"{path}: bad snapshot magic")
    off = len(GRAPH_MAGIC)
    version = struct.unpack(">I", data[off:off + 4])[0]
    offset_after_version = off + 4
    require(version == GRAPH_VERSION, f"{path}: unsupported snapshot version {version}")
    checkpoint, length = struct.unpack(">QQ", data[offset_after_version:offset_after_version + 16])
    crc = struct.unpack(">I", data[offset_after_version + 16:offset_after_version + 20])[0]
    payload = data[GRAPH_HEADER_LEN:]
    require(
        len(payload) == length,
        f"{path}: payload length {len(payload)} != declared {length}",
    )
    require(
        (zlib.crc32(payload) & 0xFFFFFFFF) == crc,
        f"{path}: payload CRC mismatch",
    )
    return checkpoint


def snapshot_files(data_dir, wal_name):
    stem = os.path.splitext(wal_name)[0]
    out = [os.path.join(data_dir, f"{stem}.graph_snapshot.bin")]
    snap_dir = os.path.join(data_dir, "snapshots")
    if os.path.isdir(snap_dir):
        out.extend(
            os.path.join(snap_dir, n) for n in sorted(os.listdir(snap_dir)) if n.endswith(".bin")
        )
    return out


# ── HTTP ────────────────────────────────────────────────────────────────

def _opener():
    # 与演练脚本的 curl `--noproxy '*'` 同口径：显式绕过环境代理，
    # 否则 http_proxy 存在时请求会被转发到代理（本机端口走不通）。
    # EN: bypass ambient proxy env, matching curl's `--noproxy '*'`.
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def http_json(base, path, payload=None, timeout=10):
    url = f"http://{base}{path}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if data else "GET")
    if data:
        req.add_header("Content-Type", "application/json")
    try:
        with _opener().open(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def vector_for(i, dim=6):
    """与演练脚本保持同一确定性公式。

    六个分量中有五个是周期 1155 的模组合，第六个是 `i // 1155` 的模值——两者
    合起来在 ~2.36M 个 id 内**方向唯一**。少于 6 维时模组合每 1155 个 id 就重复，
    top-1 会出现合法的平局（按 id 升序打破），采样断言就会误报。
    EN: Five components are periodic with period 1155; the sixth encodes
    `i // 1155`, making directions unique over ~2.36M ids. With fewer dims the
    vector repeats every 1155 ids and top-1 has legitimate ties.
    """
    full = [
        1.0,
        i % 7 + 1.0,
        i % 5 + 1.0,
        i % 3 + 1.0,
        i % 11 + 1.0,
        (i // 1155) % 2048 + 1.0,
    ]
    require(dim == 6, f"chaos vectors are 6-dimensional (got --dim {dim})")
    return full[:dim]


# ── 主检查 ──────────────────────────────────────────────────────────────
# EN: -- main checks --

def check(data_dir, wal_name, http_addr, acked_file, intended_file, sample_top1, dim):
    collections_wal = os.path.join(
        data_dir, os.path.splitext(wal_name)[0] + ".collections.wal"
    )

    # 1) 两个 WAL 的结构校验（主 WAL 可能为空/不存在——collection 路径不用它）。
    # EN: 1) Structural validation of both WALs (the main WAL may be empty/missing — the
    # EN: collection path does not use it).
    main_entries, main_infos = load_all_wal(data_dir, wal_name)
    coll_entries, coll_infos = load_all_wal(
        os.path.dirname(collections_wal), os.path.basename(collections_wal)
    )
    torn = [p for p, i in main_infos + coll_infos if i["torn_tail"]]
    if torn:
        print(f"  · 撕裂尾（重启后应已截断）: {', '.join(torn)}", file=sys.stderr)

    # 2) 重放集合
    # EN: 2) Replay collections
    live = replay_collections(coll_entries)

    # 3) 快照结构 + checkpoint 与 WAL 交叉核对
    # EN: 3) Snapshot structure + cross-check checkpoint against WAL
    max_lsn = max((lsn for lsn, _ in main_entries + coll_entries), default=None)
    for path in snapshot_files(data_dir, wal_name):
        cp = check_graph_snapshot(path)
        if cp is None:
            continue
        if max_lsn is not None and cp > max_lsn:
            print(
                f"  · 注意: {os.path.basename(path)} checkpoint_lsn={cp} > WAL max_lsn={max_lsn}"
                "（截断后属正常）",
                file=sys.stderr,
            )

    # 4) 账本：RPO=0（acked ⊆ live）与无幽灵写（live ⊆ intended）
    # EN: 4) Ledger: RPO=0 (acked subset of live) and no ghost writes (live subset of
    # EN: intended)
    acked = _read_ids(acked_file)
    intended = _read_ids(intended_file)
    acked_set = set(acked)
    intended_set = set(intended)
    live_all = set().union(*live.values()) if live else set()

    _check_health(http_addr)

    missing = sorted(acked_set - live_all)
    require(
        not missing,
        f"RPO violated: {len(missing)} acknowledged id(s) missing after restart "
        f"(first: {missing[:10]})",
    )
    if intended_set:
        phantom = sorted(live_all - intended_set)
        require(
            not phantom,
            f"phantom writes: {len(phantom)} id(s) present that were never sent "
            f"(first: {phantom[:10]})",
        )

    # 5) 计数守恒（服务自报 vs WAL 重放）
    # EN: 5) Count conservation (server self-report vs WAL replay)
    for name, ids in sorted(live.items()):
        code, body = http_json(http_addr, f"/v1/collections/{name}/status")
        require(code == 200, f"status for collection {name}: HTTP {code} {body}")
        try:
            total = body["indexed_count"] + body["pending_count"]
        except (KeyError, TypeError) as e:
            raise CheckError(f"status for {name}: unexpected payload shape ({e})")
        require(
            total == len(ids),
            f"count mismatch for {name}: service reports {total}, WAL replay says {len(ids)}",
        )

    # 6) 抽样 top-1（精确向量必须排第一）。
    # 先等异步 builder 排空：`pending_count > 0` 的向量已持久、计入总数，但尚未
    # 进图/热缓冲，检索看不到它们——此时抽样是**集合未就绪**，不是丢失。
    # EN: Drain the async builder first — vectors still pending are durable and
    # counted but not searchable yet, so sampling then would misreport data loss.
    checked = 0
    for name, ids in sorted(live.items()):
        ordered = sorted(ids)
        if not ordered or sample_top1 <= 0:
            continue
        _wait_indexed(http_addr, name)
        step = max(1, len(ordered) // sample_top1)
        for i in ordered[::step][:sample_top1]:
            # 用标量过滤把候选集钉成 [i]（掩码路径恒为精确暴力，不受 HNSW 近似
            # 影响），同时验证 payload 标量页也随 WAL/快照正确恢复。
            # EN: Pin the candidate set to [i] with a scalar filter — the mask path
            # is exact brute force regardless of ANN quality, and it also proves the
            # payload scalar page was restored.
            code, body = http_json(
                http_addr,
                f"/v1/collections/{name}/hybrid/search",
                {
                    "vector": vector_for(i, dim),
                    "k": 1,
                    "filters": [{"column": "row", "op": "eq", "value": i}],
                },
            )
            require(code == 200, f"filtered search {name} id={i}: HTTP {code} {body}")
            hits = body.get("ids") or []
            require(
                hits == [i],
                f"exact filtered retrieval mismatch for {name} id={i}: got {hits[:3]}",
            )
            checked += 1

    print(
        f"  ✓ 不变量全过: {len(live)} collection(s), "
        f"{len(live_all)} live id(s), {len(acked_set)} acked, 精确检索抽样 {checked}"
    )


def _wait_indexed(http_addr, name, timeout=60.0):
    """等该 collection 的异步 builder 排空（pending_count → 0）。
    EN: Wait for the collection's async builder to drain (pending_count -> 0).
    """
    import time

    deadline = time.time() + timeout
    pending = None
    while time.time() < deadline:
        code, body = http_json(http_addr, f"/v1/collections/{name}/status")
        require(code == 200, f"status for collection {name}: HTTP {code} {body}")
        pending = body.get("pending_count", 0)
        if pending == 0:
            return
        time.sleep(0.2)
    raise CheckError(
        f"builder did not drain for {name} within {timeout:.0f}s (pending_count={pending})"
    )


def _read_ids(path):
    if not path or not os.path.exists(path):
        return []
    out = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                out.append(int(line))
    return out


def _check_health(http_addr):
    code, body = http_json(http_addr, "/v1/health")
    require(code == 200 and body.get("status") == "ok", f"health: HTTP {code} {body}")


# ── 负路径自测 ──────────────────────────────────────────────────────────
# EN: -- negative-path self-test --

def selftest():
    """人为构造合法/腐坏样本，断言校验器分别放行/报错。
    EN: Construct valid/corrupt samples and assert the verifier accepts/rejects
    EN: them respectively.
    """
    import tempfile

    def rec(lsn, payload):
        plen = len(payload)
        crc = zlib.crc32(struct.pack(">QI", lsn, plen) + payload) & 0xFFFFFFFF
        return struct.pack(">QII", lsn, plen, crc) + payload

    def header():
        return WAL_MAGIC + struct.pack(">I", WAL_VERSION)

    body = b'{"v":1,"op":"collection_create","name":"c"}'
    good = header() + rec(1, body) + rec(2, body)

    cases = []

    # 合法：完整文件通过。
    # EN: Valid: a complete file passes.
    cases.append(("valid", good, True))
    # 撕裂尾：最后一条记录只写了一半 → 放行（有效前缀）。
    # EN: Torn tail: only half of the last record is written -> accepted (valid prefix).
    cases.append(("torn tail", good[:-3], True))
    # 文件头长度不足 → 报错。
    # EN: File header shorter than required -> error.
    cases.append(("truncated header", good[:8], False))
    # 魔数错 → 报错。
    # EN: Bad magic -> error.
    cases.append(("bad magic", b"HOUYIWAX" + good[8:], False))
    # 版本错 → 报错。
    # EN: Bad version -> error.
    cases.append(("bad version", good[:8] + struct.pack(">I", 9) + good[12:], False))
    # 中间腐坏：第 1 条 CRC 错、其后还有完整记录 → 报错。
    # EN: Mid-log corruption: record 1 has a bad CRC and intact records follow -> error.
    corrupt_mid = bytearray(good)
    # 破坏第 1 条记录的 **payload**（记录头 16 字节之后的第一个负载字节）：
    # 长度字段保持不变，于是解析器在 CRC 处停住，而其后还有一条完整记录。
    # EN: Corrupt the **payload** of record 1 (the first payload byte after the 16-byte
    # EN: header): the length field stays unchanged, so the parser stops at the CRC while
    # EN: a complete record follows.
    corrupt_mid[WAL_HEADER_LEN + REC_HEADER_LEN] ^= 0xFF
    cases.append(("mid-log corruption", bytes(corrupt_mid), False))

    failures = 0
    for name, data, should_pass in cases:
        with tempfile.NamedTemporaryFile(suffix=".wal", delete=False) as f:
            f.write(data)
            path = f.name
        try:
            _, info = parse_wal(path)
            ok = info["mid_corruption"] is None
            detail = info["mid_corruption"] or "ok"
        except CheckError as e:
            ok = False
            detail = str(e)
        finally:
            os.unlink(path)
        if ok != should_pass:
            failures += 1
            print(f"!! selftest '{name}': expected pass={should_pass}, got pass={ok} ({detail})")
        else:
            print(f"   selftest '{name}': {'放行' if ok else '拒绝'} ✓")

    # 快照负路径：CRC 错必须报错；合法必须放行。
    # EN: Snapshot negative path: a bad CRC must error; a valid one must pass.
    with tempfile.TemporaryDirectory() as d:
        payload = b"graphpayload"
        hdr = (
            GRAPH_MAGIC
            + struct.pack(">I", GRAPH_VERSION)
            + struct.pack(">Q", 7)
            + struct.pack(">Q", len(payload))
            + struct.pack(">I", zlib.crc32(payload) & 0xFFFFFFFF)
        )
        good_path = os.path.join(d, "good.bin")
        open(good_path, "wb").write(hdr + payload)
        bad_path = os.path.join(d, "bad.bin")
        bad = bytearray(hdr + payload)
        bad[-1] ^= 0xFF
        open(bad_path, "wb").write(bytes(bad))

        try:
            cp = check_graph_snapshot(good_path)
            if cp != 7:
                raise CheckError(f"checkpoint_lsn {cp} != 7")
            print("   selftest 'snapshot valid': 放行 ✓")
        except CheckError as e:
            failures += 1
            print(f"!! selftest 'snapshot valid': {e}")
        try:
            check_graph_snapshot(bad_path)
            failures += 1
            print("!! selftest 'snapshot CRC': 期望报错，实际放行")
        except CheckError:
            print("   selftest 'snapshot CRC': 拒绝 ✓")

    if failures:
        print(f"!! 自测失败 {failures} 项", file=sys.stderr)
        return 1
    print(f"==> 自测通过：{len(cases) + 2} 例全部符合预期")
    return 0


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--selftest", action="store_true", help="跑负路径自测后退出")
    ap.add_argument("--data-dir")
    ap.add_argument("--wal-name", default="chaos.wal")
    ap.add_argument("--http", help="host:port of the benchmark HTTP listener")
    ap.add_argument("--acked-file", help="ids that received HTTP 200 (one per line)")
    ap.add_argument("--intended-file", help="ids ever sent (one per line)")
    ap.add_argument("--sample-top1", type=int, default=8)
    ap.add_argument("--dim", type=int, default=6)
    args = ap.parse_args(argv)

    if args.selftest:
        return selftest()
    require(args.data_dir, "--data-dir is required")
    require(args.http, "--http is required")
    check(
        args.data_dir,
        args.wal_name,
        args.http,
        args.acked_file,
        args.intended_file,
        args.sample_top1,
        args.dim,
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except CheckError as e:
        print(f"!! 不变量违例: {e}", file=sys.stderr)
        sys.exit(1)
