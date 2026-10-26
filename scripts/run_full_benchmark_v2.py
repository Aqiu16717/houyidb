#!/usr/bin/env python3
"""
HouyiDB vs Qdrant 综合对比测试 v2 — 严格顺序执行，杜绝并行。
覆盖: 多索引类型、Streaming、LabelFilter、gRPC Qdrant。

用法:
  source .venv/bin/activate
  python ../houyidb-ccds/scripts/run_full_benchmark_v2.py
"""

import json, logging, os, sys, time, traceback
from datetime import datetime
from pathlib import Path
from typing import Optional

_VDB_ROOT = os.path.abspath(os.path.join(
    os.path.dirname(__file__), "..", "..", "vectorDBBench-houyidb"))
sys.path.insert(0, _VDB_ROOT)

from vectordb_bench import config as vdb_config
from vectordb_bench.interface import benchmark_runner
from vectordb_bench.models import (
    CaseConfig, CaseType, ConcurrencySearchConfig, TaskConfig, TaskStage,
)
from vectordb_bench.backend.clients import DB
from vectordb_bench.backend.clients.houyidb.config import HouyiDBConfig, HouyiDBIndexConfig
from vectordb_bench.backend.clients.qdrant_local.config import QdrantLocalConfig, QdrantLocalIndexConfig
from vectordb_bench.backend.clients.api import MetricType
from pydantic import SecretStr

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
log = logging.getLogger("bench_v2")

# ── 连接配置 ──
# EN: -- Connection config --
HOUYI_HOST = os.environ.get("HOUYI_HOST", "127.0.0.1")
HOUYI_PG_PORT = int(os.environ.get("HOUYI_PG_PORT", "15432"))
HOUYI_HTTP_PORT = int(os.environ.get("HOUYI_HTTP_PORT", "17000"))
QDRANT_URL = os.environ.get("QDRANT_URL", "http://127.0.0.1:6333")

ALL_STAGES = [TaskStage.DROP_OLD, TaskStage.LOAD, TaskStage.SEARCH_SERIAL, TaskStage.SEARCH_CONCURRENT]
RESULTS_DIR = Path(os.path.join(os.path.dirname(__file__), "..", "bench-results"))
RESULTS_DIR.mkdir(parents=True, exist_ok=True)

# ── 全局结果收集 ──
# EN: -- Global result collection --
all_test_results: list[dict] = []


def make_task_houyi(
    case_type: CaseType,
    ef_search: int = 256,
    m: int = 24,
    ef_construction: int = 400,
    k: int = 10,
    concurrencies: list[int] | None = None,
    custom_case: dict | None = None,
) -> TaskConfig:
    if concurrencies is None:
        concurrencies = [4, 8, 16]
    return TaskConfig(
        db=DB.HouyiDB,
        db_config=HouyiDBConfig(host=HOUYI_HOST, port=HOUYI_PG_PORT, transport="pgwire", http_port=HOUYI_HTTP_PORT),
        db_case_config=HouyiDBIndexConfig(metric_type=MetricType.COSINE, m=m, ef_construction=ef_construction, ef_search=ef_search),
        case_config=CaseConfig(case_id=case_type, k=k,
            concurrency_search_config=ConcurrencySearchConfig(num_concurrency=concurrencies, concurrency_duration=30),
            custom_case=custom_case or {}),
        stages=ALL_STAGES, load_concurrency=0)


def make_task_qdrant(
    case_type: CaseType,
    ef_search: int = 256,
    m: int = 24,
    ef_construct: int = 400,
    k: int = 10,
    concurrencies: list[int] | None = None,
    custom_case: dict | None = None,
) -> TaskConfig:
    if concurrencies is None:
        concurrencies = [4, 8, 16]
    return TaskConfig(
        db=DB.QdrantLocal,
        db_config=QdrantLocalConfig(url=SecretStr(QDRANT_URL)),
        db_case_config=QdrantLocalIndexConfig(metric_type=MetricType.COSINE, m=m, ef_construct=ef_construct, hnsw_ef=ef_search, on_disk=False),
        case_config=CaseConfig(case_id=case_type, k=k,
            concurrency_search_config=ConcurrencySearchConfig(num_concurrency=concurrencies, concurrency_duration=30),
            custom_case=custom_case or {}),
        stages=ALL_STAGES, load_concurrency=0)


def run_one(task: TaskConfig, label: str, db_label: str, index_type: str, notes: str = "") -> dict | None:
    """运行单个测试，阻塞等待完成。返回关键指标字典。
    EN: Run a single test, blocking until completion. Return a dict of key metrics.
    """
    log.info(f"▶ 开始 [{db_label}/{index_type}] {label}")
    start = time.time()
    ok = benchmark_runner.run([task], label)
    if not ok:
        log.error(f"  启动失败: {label}")
        return None

    try:
        while benchmark_runner.has_running():
            time.sleep(2)
    except KeyboardInterrupt:
        benchmark_runner.stop_running()
        return None

    elapsed = time.time() - start
    err = benchmark_runner.latest_error
    if err:
        log.error(f"  ✗ [{db_label}/{index_type}] {label}: {err}")
        return None

    # 从最新结果文件提取指标
    # EN: Extract metrics from the latest result file.
    result_dir = Path(vdb_config.RESULTS_LOCAL_DIR) / db_label
    files = sorted(result_dir.glob("result_*.json"), key=os.path.getmtime, reverse=True)
    metrics = None
    # 最多查最近 5 个文件
    # EN: Check at most the latest 5 files.
    for f in files[:5]:  # 最多查最近 5 个文件
        try:
            d = json.loads(f.read_text())
            if d.get("task_label") == label:
                for r in d.get("results", []):
                    m = r.get("metrics", {})
                    if m.get("recall") and m["recall"] > 0.01:
                        metrics = m
                        break
        except Exception:
            pass
        if metrics:
            break

    if not metrics:
        log.warning(f"  △ [{db_label}/{index_type}] {label}: 无有效指标 (可能 recall=0)")
        return None

    result = {
        "label": label, "db": db_label, "index_type": index_type, "notes": notes,
        "elapsed_s": round(elapsed, 1),
        "recall": metrics.get("recall"), "ndcg": metrics.get("ndcg"),
        "qps": metrics.get("qps"),
        "serial_latency_p99": metrics.get("serial_latency_p99"),
        "serial_latency_p95": metrics.get("serial_latency_p95"),
        "conc_qps_list": metrics.get("conc_qps_list", []),
        "conc_latency_p99_list": metrics.get("conc_latency_p99_list", []),
        "load_duration": metrics.get("load_duration"),
        "insert_duration": metrics.get("insert_duration"),
        "optimize_duration": metrics.get("optimize_duration"),
# streaming 专属字段
# EN: streaming-specific fields
        "st_qps": metrics.get("st_qps"),
        "st_recall_list": metrics.get("st_recall_list"),
        "st_serial_latency_p99_list": metrics.get("st_serial_latency_p99_list"),
    }
    log.info(f"  ✓ [{db_label}/{index_type}] {label}: recall={result['recall']:.4f} qps={result['qps']:.1f} p99={result['serial_latency_p99']*1000:.1f}ms ({elapsed:.0f}s)")
    all_test_results.append(result)
    return result


# ═══════════════════════════════════════════════════════════════════
# 测试计划
# EN: Test plan
# ═══════════════════════════════════════════════════════════════════

def test_plan():
    """生成测试计划列表。每条: (task, label, db_results_name, index_type, notes)
    EN: Generate the test plan list. Each entry:
    EN: (task, label, db_results_name, index_type, notes)
    """
    tests = []

    # ── Phase 1: HouyiDB 多索引类型 只读对比 ──
    # EN: -- Phase 1: HouyiDB multi-index read-only comparison --
    houyi_index_types = [
        ("hnsw", "HNSW (默认)"),
        ("clustered", "Clustered HNSW"),
        ("hii-l0", "HII L0 (内存增量层)"),
    ]
    for idx_type, idx_desc in houyi_index_types:
        tests.append((
            make_task_houyi(CaseType.Performance768D1M, ef_search=256),
            f"houyidb-readonly-Cohere1M-ef256-{idx_type}-v2",
            "HouyiDB", idx_desc,
            "Cohere 768d 1M，对比索引类型对大规模场景的影响"
        ))

# ── 阶段 2：HouyiDB 流式（Python API）──
# EN: -- Phase 2: HouyiDB Streaming (Python API) --
    streaming_custom = {
        "dataset_with_size_type": "Medium Cohere (768dim, 1M)",
        "insert_rate": 500,
        "search_stages": [0.3, 0.6, 0.9],
        "concurrencies": [4, 8, 16],
        "optimize_after_write": True,
        "read_dur_after_write": 30,
    }
    tests.append((
        make_task_houyi(CaseType.StreamingPerformanceCase, ef_search=256, concurrencies=[4, 8, 16], custom_case=streaming_custom),
        f"houyidb-streaming-Cohere1M-v2",
        "HouyiDB", "HNSW (默认)",
        "StreamingPerformanceCase: 边插入(500/s)边查询, stages=0.3/0.6/0.9"
    ))

# ── 阶段 3：Qdrant gRPC ──
# EN: -- Phase 3: Qdrant gRPC --
    tests.append((
        make_task_qdrant(CaseType.Performance768D1M, ef_search=256),
        f"qdrant-grpc-Cohere1M-ef256-v2",
        "QdrantLocal", "HNSW (gRPC)",
        "Qdrant Cohere 1M via gRPC (prefer_grpc=True)"
    ))

# ── 阶段 4：Qdrant 标签过滤 ──
# EN: -- Phase 4: Qdrant LabelFilter --
    for ef in [128, 256]:
        filter_custom = {
            "dataset_with_size_type": "Medium Cohere (768dim, 1M)",
            "label_percentage": 0.01,
        }
        tests.append((
            make_task_qdrant(CaseType.LabelFilterPerformanceCase, ef_search=ef, concurrencies=[1], custom_case=filter_custom),
            f"qdrant-grpc-filter-Cohere1M-lp1-ef{ef}-v2",
            "QdrantLocal", "HNSW (gRPC)",
            f"Qdrant LabelFilter 1%, ef={ef}"
        ))

    return tests


def format_table():
    """生成 Markdown 结果表。
    EN: Generate the Markdown results table.
    """
    if not all_test_results:
        return "无有效测试结果"

    lines = ["## 测试结果汇总", ""]
    lines.append("| # | DB | 索引类型 | 数据集/场景 | Recall | QPS | P99(ms) | Load(s) | 备注 |")
    lines.append("|---|-----|----------|------------|--------|-----|---------|---------|------|")
    for i, r in enumerate(all_test_results, 1):
        ds = r["label"].split("-")[2] if len(r["label"].split("-")) > 2 else r["label"]
        p99 = f"{r['serial_latency_p99']*1000:.1f}" if r.get("serial_latency_p99") else "-"
        load = f"{r['load_duration']:.0f}" if r.get("load_duration") else "-"
        lines.append(
            f"| {i} | {r['db']} | {r['index_type']} | {ds} | "
            f"{r['recall']:.4f} | {r['qps']:.1f} | {p99} | {load} | {r['notes']} |"
        )
    return "\n".join(lines)


def main():
    tests = test_plan()
    print(f"\n{'='*70}")
    print(f"  综合测试 v2: {len(tests)} 项，严格顺序执行")
    print(f"{'='*70}\n")
    for i, (task, label, db_dir, idx_type, notes) in enumerate(tests, 1):
        print(f"  [{i}/{len(tests)}] {db_dir}/{idx_type}: {label}")
    print()

    failed = 0
    for i, (task, label, db_dir, idx_type, notes) in enumerate(tests, 1):
        print(f"\n{'─'*60}")
        print(f"[{i}/{len(tests)}] {label}")
        result = run_one(task, label, db_dir, idx_type, notes)
        if result is None:
            failed += 1

    print(f"\n{'='*70}")
    print(f"  完成: {len(tests)-failed}/{len(tests)} 成功, {failed} 失败")
    print(f"{'='*70}")

    print(format_table())

    # 保存完整结果
    # EN: Save the complete results.
    ts = datetime.now().strftime("%Y%m%d_%H%M")
    out_path = RESULTS_DIR / f"all_results_v2_{ts}.json"
    out_path.write_text(json.dumps(all_test_results, indent=2, default=str))
    print(f"\n完整结果: {out_path}")


if __name__ == "__main__":
    main()
