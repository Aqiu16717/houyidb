#!/usr/bin/env python3
"""
HouyiDB vs Qdrant 综合对比测试脚本

测试矩阵:
  A. PerformanceCase (只读) — 先建索引，纯查询，固定 recall 下比 QPS/延迟
     - 2 数据集 × 4 ef_search × 4 并发 = 8 runs/DB
  B. StreamingPerformanceCase — 并发写入+查询 (Agent 真实负载)
     - 2 数据集 × 1 insert_rate = 2 runs/DB
  C. LabelFilterPerformanceCase — metadata filtering (hybrid search)
     - 2 数据集 × 2 label_pct × 3 ef_search = 12 runs/DB

用法 (在 vectorDBBench-houyidb 目录下):
  source .venv/bin/activate
  python ../houyidb-ccds/scripts/run_full_benchmark.py --db houyidb --suite all
  python ../houyidb-ccds/scripts/run_full_benchmark.py --db qdrant --suite readonly
  python ../houyidb-ccds/scripts/run_full_benchmark.py --db houyidb --suite streaming
  python ../houyidb-ccds/scripts/run_full_benchmark.py --db houyidb --suite filtering
"""

import argparse
import json
import logging
import os
import sys
import time
from datetime import datetime
from pathlib import Path

# 确保 vectordb_bench 在 sys.path 上
# EN: Ensure vectordb_bench is on sys.path.
_VDB_ROOT = os.environ.get(
    "VDB_ROOT",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "vectorDBBench-houyidb"),
)
_VDB_ROOT = os.path.abspath(_VDB_ROOT)
sys.path.insert(0, _VDB_ROOT)

from vectordb_bench import config as vdb_config
from vectordb_bench.interface import benchmark_runner
from vectordb_bench.models import (
    CaseConfig,
    CaseType,
    ConcurrencySearchConfig,
    TaskConfig,
    TaskStage,
)
from vectordb_bench.backend.clients import DB
from vectordb_bench.backend.clients.houyidb.config import HouyiDBConfig, HouyiDBIndexConfig
from vectordb_bench.backend.clients.qdrant_local.config import QdrantLocalConfig, QdrantLocalIndexConfig
from vectordb_bench.backend.clients.api import MetricType

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger("full_benchmark")

# ——— 默认参数 ———
# EN: --- Default parameters ---
DEFAULT_M = 24
DEFAULT_EFC = 400
DEFAULT_K = 10

# —— HouyiDB 连接 ——
# EN: -- HouyiDB connection --
HOUYI_HOST = os.environ.get("HOUYI_HOST", "127.0.0.1")
HOUYI_PG_PORT = int(os.environ.get("HOUYI_PG_PORT", "15432"))
HOUYI_HTTP_PORT = int(os.environ.get("HOUYI_HTTP_PORT", "17000"))

# —— Qdrant 连接 ——
# EN: -- Qdrant connection --
QDRANT_URL = os.environ.get("QDRANT_URL", "http://127.0.0.1:6333")

# —— 所有阶段 ——
# EN: -- All stages --
ALL_STAGES = [
    TaskStage.DROP_OLD,
    TaskStage.LOAD,
    TaskStage.SEARCH_SERIAL,
    TaskStage.SEARCH_CONCURRENT,
]

# —— 结果输出目录 ——
# EN: -- Result output directory --
RESULTS_DIR = Path(os.environ.get(
    "BENCH_RESULTS_DIR",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "bench-results"),
)).resolve()


def make_houyi_config(ef_search: int, m: int = DEFAULT_M, ef_construction: int = DEFAULT_EFC) -> tuple:
    db_config = HouyiDBConfig(
        host=HOUYI_HOST,
        port=HOUYI_PG_PORT,
        transport="pgwire",
        http_port=HOUYI_HTTP_PORT,
    )
    case_config = HouyiDBIndexConfig(
        metric_type=MetricType.COSINE,
        m=m,
        ef_construction=ef_construction,
        ef_search=ef_search,
    )
    return DB.HouyiDB, db_config, case_config


def make_qdrant_config(ef_search: int, m: int = DEFAULT_M, ef_construct: int = DEFAULT_EFC) -> tuple:
    from pydantic import SecretStr
    db_config = QdrantLocalConfig(url=SecretStr(QDRANT_URL))
    case_config = QdrantLocalIndexConfig(
        metric_type=MetricType.COSINE,
        m=m,
        ef_construct=ef_construct,
        hnsw_ef=ef_search,
        on_disk=False,
    )
    return DB.QdrantLocal, db_config, case_config


def make_db_configs(ef_search: int, db_name: str) -> tuple:
    if db_name == "houyidb":
        return make_houyi_config(ef_search)
    elif db_name == "qdrant":
        return make_qdrant_config(ef_search)
    else:
        raise ValueError(f"Unknown db: {db_name}")


def run_single_task(task: TaskConfig, task_label: str) -> bool:
    """运行单个 task 并等待完成。返回 True 表示成功。
    EN: Run a single task and wait for completion. Return True on success.
    """
    log.info(f"=== 开始: {task_label} ===")
    ok = benchmark_runner.run([task], task_label)
    if not ok:
        log.error(f"启动 task 失败: {task_label}")
        return False

    try:
        while benchmark_runner.has_running():
            time.sleep(2)
    except KeyboardInterrupt:
        log.warning("收到中断信号，停止当前 task...")
        benchmark_runner.stop_running()
        return False

    if benchmark_runner.latest_error:
        log.error(f"Task 运行出错: {benchmark_runner.latest_error}")
        return False

    log.info(f"=== 完成: {task_label} ===")
    return True


def extract_metrics_from_results(db_name: str) -> list[dict]:
    """从最新结果文件中提取关键指标。
    EN: Extract key metrics from the latest result file.
    """
    results_path = Path(vdb_config.RESULTS_LOCAL_DIR) / db_name
    if not results_path.exists():
        return []

    all_metrics = []
    for f in sorted(results_path.glob("result_*.json"), key=os.path.getmtime, reverse=True):
        try:
            with open(f) as fp:
                data = json.load(fp)
            for r in data.get("results", []):
                m = r.get("metrics", {})
                tc = r.get("task_config", {})
                cc = tc.get("case_config", {})
                db_cc = tc.get("db_case_config", {})
                all_metrics.append({
                    "file": str(f),
                    "task_label": data.get("task_label", ""),
                    "case_type": cc.get("case_id", ""),
                    "recall": m.get("recall"),
                    "ndcg": m.get("ndcg"),
                    "qps": m.get("qps"),
                    "serial_latency_p99": m.get("serial_latency_p99"),
                    "serial_latency_p95": m.get("serial_latency_p95"),
                    "load_duration": m.get("load_duration"),
                    "insert_duration": m.get("insert_duration"),
                    "optimize_duration": m.get("optimize_duration"),
                    "max_load_count": m.get("max_load_count"),
                    "ef_search": db_cc.get("ef_search", db_cc.get("hnsw_ef", "")),
                    "m": db_cc.get("m", ""),
                    "ef_construction": db_cc.get("ef_construction", db_cc.get("ef_construct", "")),
                    # streaming-specific
                    "st_qps": m.get("st_qps"),
                    "st_serial_latency_p99": m.get("st_serial_latency_p99"),
                })
        except Exception as e:
            log.warning(f"读取结果文件失败 {f}: {e}")
    return all_metrics


# ═══════════════════════════════════════════════════════════════
# 测试套件定义
# EN: Test suite definitions
# ═══════════════════════════════════════════════════════════════

def _dimension_matches(case_dim: int, requested_dim: int | None) -> bool:
    """检查数据集维度是否匹配请求。requested_dim=None 表示不限。
    EN: Check whether the dataset dimension matches the request.
    EN: requested_dim=None means unrestricted.
    """
    return requested_dim is None or case_dim == requested_dim

def suite_readonly(db_name: str, dimension: int | None = None):
    """A. PerformanceCase（只读）：先建索引，纯查询。
    EN: A. PerformanceCase (read-only): build the index first, then pure queries.
    """
    # (case_type, label_desc, dim, ef_list) — Cohere 1M 慢，只测 3 档
    # EN: (case_type, label_desc, dim, ef_list) — Cohere 1M is slow, so test only 3 tiers.
    cases = [
        (CaseType.Performance768D1M, "Cohere768d_1M", 768, [128, 256, 512]),
        (CaseType.Performance1536D50K, "OpenAI1536d_50K", 1536, [64, 128, 256, 512]),
    ]
    concurrencies = [4, 8, 16]

    for case_type, label_desc, case_dim, ef_list in cases:
        if not _dimension_matches(case_dim, dimension):
            continue
        for ef in ef_list:
            db, db_config, db_case_config = make_db_configs(ef, db_name)
            task_label = f"{db_name}-readonly-{label_desc}-ef{ef}"

            task = TaskConfig(
                db=db,
                db_config=db_config,
                db_case_config=db_case_config,
                case_config=CaseConfig(
                    case_id=case_type,
                    k=DEFAULT_K,
                    concurrency_search_config=ConcurrencySearchConfig(
                        num_concurrency=concurrencies,
                        concurrency_duration=30,
                    ),
                    custom_case=None,
                ),
                stages=ALL_STAGES,
                load_concurrency=0,
            )

            yield task, task_label


def suite_streaming(db_name: str, dimension: int | None = None):
    """B. StreamingPerformanceCase：并发写入+查询。
    EN: B. StreamingPerformanceCase: concurrent writes + queries.
    """
    datasets = [
        ("Medium Cohere (768dim, 1M)", "Cohere768d_1M", 768),
        ("Small OpenAI (1536dim, 50K)", "OpenAI1536d_50K", 1536),
    ]

    for ds_type, label_desc, ds_dim in datasets:
        if not _dimension_matches(ds_dim, dimension):
            continue
        db, db_config, db_case_config = make_db_configs(256, db_name)
        task_label = f"{db_name}-streaming-{label_desc}"

        task = TaskConfig(
            db=db,
            db_config=db_config,
            db_case_config=db_case_config,
            case_config=CaseConfig(
                case_id=CaseType.StreamingPerformanceCase,
                k=DEFAULT_K,
                concurrency_search_config=ConcurrencySearchConfig(
                    num_concurrency=[4, 8, 16],
                    concurrency_duration=30,
                ),
                custom_case={
                    "dataset_with_size_type": ds_type,
                    "insert_rate": 500,
                    # 必须 < 1.0
                    # EN: must be < 1.0
                    "search_stages": [0.3, 0.6, 0.9],  # must be < 1.0
                    "concurrencies": [4, 8, 16],
                    "optimize_after_write": True,
                    "read_dur_after_write": 30,
                },
            ),
            stages=ALL_STAGES,
            load_concurrency=0,
        )

        yield task, task_label


def suite_filtering(db_name: str, dimension: int | None = None):
    """C. LabelFilterPerformanceCase：元数据过滤。
    EN: C. LabelFilterPerformanceCase: metadata filtering.
    """
    datasets = [
        ("Medium Cohere (768dim, 1M)", "Cohere768d_1M", 768),
        ("Small OpenAI (1536dim, 50K)", "OpenAI1536d_50K", 1536),
    ]
    # 1% 选择率（最常见场景）
    # EN: 1% selectivity (the most common scenario).
    label_pcts = [0.01]  # 1% 选择率 (最常见场景)
    ef_searches = [128, 256]
    # 过滤场景先测串行
    # EN: Test the filtering scenario serially first.
    concurrencies = [1]  # 过滤场景先测串行

    for ds_type, label_desc, ds_dim in datasets:
        if not _dimension_matches(ds_dim, dimension):
            continue
        for lp in label_pcts:
            for ef in ef_searches:
                db, db_config, db_case_config = make_db_configs(ef, db_name)
                task_label = f"{db_name}-filter-{label_desc}-lp{int(lp*100)}-ef{ef}"

                task = TaskConfig(
                    db=db,
                    db_config=db_config,
                    db_case_config=db_case_config,
                    case_config=CaseConfig(
                        case_id=CaseType.LabelFilterPerformanceCase,
                        k=DEFAULT_K,
                        concurrency_search_config=ConcurrencySearchConfig(
                            num_concurrency=concurrencies,
                            concurrency_duration=30,
                        ),
                        custom_case={
                            "dataset_with_size_type": ds_type,
                            "label_percentage": lp,
                        },
                    ),
                    stages=ALL_STAGES,
                    load_concurrency=0,
                )

                yield task, task_label


SUITES = {
    "readonly": suite_readonly,
    "streaming": suite_streaming,
    "filtering": suite_filtering,
}


def generate_report(all_metrics: dict[str, list[dict]], output_path: Path):
    """生成 Markdown 综合测试报告。
    EN: Generate a Markdown comprehensive test report.
    """
    now = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    lines = [
        f"# HouyiDB vs Qdrant 综合性能测试报告",
        f"",
        f"**测试时间**: {now}",
        f"**测试机器**: {os.uname().sysname} {os.uname().machine}",
        f"**HouyiDB**: pgwire://{HOUYI_HOST}:{HOUYI_PG_PORT}",
        f"**Qdrant**: {QDRANT_URL}",
        f"",
        f"## 测试配置",
        f"",
        f"| 参数 | 值 |",
        f"|------|-----|",
        f"| M | {DEFAULT_M} |",
        f"| ef_construction | {DEFAULT_EFC} |",
        f"| k (top-K) | {DEFAULT_K} |",
        f"| 并发搜索时长 | 30s |",
        f"",
    ]

    # ——— 1. 只读性能 ———
    # EN: --- 1. Read-only performance ---
    lines.append("## 1. 只读性能 (PerformanceCase)")
    lines.append("")
    lines.append("先建索引，再纯查询。固定 recall 下对比 QPS/延迟。")
    lines.append("")

    readonly_houyi = [m for m in all_metrics.get("HouyiDB", []) if "readonly" in m.get("task_label", "")]
    readonly_qdrant = [m for m in all_metrics.get("QdrantLocal", []) if "readonly" in m.get("task_label", "")]

    if readonly_houyi or readonly_qdrant:
        lines.append("### 1.1 按数据集 × ef_search 汇总")
        lines.append("")
        lines.append("| 数据集 | ef_search | DB | Recall | NDCG | QPS | P95(ms) | P99(ms) | Load(s) |")
        lines.append("|--------|-----------|----|--------|------|-----|---------|---------|---------|")
        for metrics in readonly_houyi + readonly_qdrant:
            label = metrics["task_label"]
            db = "HouyiDB" if "houyidb" in label else "Qdrant"
            dataset = label.split("-")[2] if len(label.split("-")) > 2 else label
            ef = metrics.get("ef_search", "-")
            recall = f"{metrics['recall']:.4f}" if metrics['recall'] else "-"
            ndcg = f"{metrics['ndcg']:.4f}" if metrics['ndcg'] else "-"
            qps = f"{metrics['qps']:.1f}" if metrics['qps'] else "-"
            p95 = f"{metrics['serial_latency_p95']*1000:.1f}" if metrics.get('serial_latency_p95') else "-"
            p99 = f"{metrics['serial_latency_p99']*1000:.1f}" if metrics.get('serial_latency_p99') else "-"
            load = f"{metrics['load_duration']:.0f}" if metrics.get('load_duration') else "-"
            lines.append(f"| {dataset} | {ef} | {db} | {recall} | {ndcg} | {qps} | {p95} | {p99} | {load} |")
        lines.append("")

    # ——— 2. 流式性能 ———
    # EN: --- 2. Streaming performance ---
    lines.append("## 2. 流式写入+查询 (StreamingPerformanceCase)")
    lines.append("")
    lines.append("边插入边查询，模拟 Agent/RAG 持续摄入新文档的真实负载。")
    lines.append("")

    streaming_houyi = [m for m in all_metrics.get("HouyiDB", []) if "streaming" in m.get("task_label", "")]
    streaming_qdrant = [m for m in all_metrics.get("QdrantLocal", []) if "streaming" in m.get("task_label", "")]

    if streaming_houyi or streaming_qdrant:
        lines.append("| 数据集 | DB | st_qps | st_p99(ms) | Recall | Load(s) |")
        lines.append("|--------|-----|--------|------------|--------|---------|")
        for metrics in streaming_houyi + streaming_qdrant:
            label = metrics["task_label"]
            db = "HouyiDB" if "houyidb" in label else "Qdrant"
            dataset = label.split("-")[2] if len(label.split("-")) > 2 else label
            st_qps = f"{metrics['st_qps']}" if metrics.get('st_qps') else "-"
            st_p99 = f"{metrics['st_serial_latency_p99']*1000:.1f}" if metrics.get('st_serial_latency_p99') else "-"
            recall = f"{metrics['recall']:.4f}" if metrics['recall'] else "-"
            load = f"{metrics['load_duration']:.0f}" if metrics.get('load_duration') else "-"
            lines.append(f"| {dataset} | {db} | {st_qps} | {st_p99} | {recall} | {load} |")
        lines.append("")

# ——— 3. 元数据过滤 ———
# EN: --- 3. Metadata Filtering ---
    lines.append("## 3. Metadata Filtering (LabelFilterPerformanceCase)")
    lines.append("")
    lines.append("基于标量标签的过滤查询 (hybrid search)。")
    lines.append("")

    filter_houyi = [m for m in all_metrics.get("HouyiDB", []) if "filter" in m.get("task_label", "")]
    filter_qdrant = [m for m in all_metrics.get("QdrantLocal", []) if "filter" in m.get("task_label", "")]

    if filter_houyi or filter_qdrant:
        lines.append("| 数据集 | Label% | ef_search | DB | Recall | NDCG | QPS | P99(ms) | Load(s) |")
        lines.append("|--------|--------|-----------|----|--------|------|-----|---------|---------|")
        for metrics in filter_houyi + filter_qdrant:
            label = metrics["task_label"]
            db = "HouyiDB" if "houyidb" in label else "Qdrant"
# 解析 label：houyidb-filter-Cohere768d_1M-lp1-ef256
# EN: parse label: houyidb-filter-Cohere768d_1M-lp1-ef256
            parts = label.split("-")
            dataset = parts[2] if len(parts) > 2 else label
            lp = parts[3] if len(parts) > 3 else "-"
            ef = metrics.get("ef_search", "-")
            recall = f"{metrics['recall']:.4f}" if metrics['recall'] else "-"
            ndcg = f"{metrics['ndcg']:.4f}" if metrics['ndcg'] else "-"
            qps = f"{metrics['qps']:.1f}" if metrics['qps'] else "-"
            p99 = f"{metrics['serial_latency_p99']*1000:.1f}" if metrics.get('serial_latency_p99') else "-"
            load = f"{metrics['load_duration']:.0f}" if metrics.get('load_duration') else "-"
            lines.append(f"| {dataset} | {lp} | {ef} | {db} | {recall} | {ndcg} | {qps} | {p99} | {load} |")
        lines.append("")

    # ——— 4. 总结 ———
    # EN: --- 4. Summary ---
    lines.append("## 4. 总结")
    lines.append("")
    lines.append("### 关键发现")
    lines.append("")
    lines.append("1. **只读性能**: 待填充")
    lines.append("2. **流式写入性能**: 待填充")
    lines.append("3. **Metadata Filtering**: 待填充")
    lines.append("")
    lines.append("### 风险与偏差记录")
    lines.append("")
    lines.append("- 是否同机同负载窗口: ")
    lines.append("- 是否发生异常 (OOM/timeout/重试): ")
    lines.append("- 是否存在不可比因素: ")

    report_content = "\n".join(lines)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(report_content, encoding="utf-8")
    log.info(f"报告已生成: {output_path}")
    return report_content


def main():
    parser = argparse.ArgumentParser(description="HouyiDB vs Qdrant 综合对比测试")
    parser.add_argument("--db", choices=["houyidb", "qdrant"], required=True, help="目标数据库")
    parser.add_argument("--suite", choices=["all", "readonly", "streaming", "filtering"],
                        default="all", help="测试套件")
    parser.add_argument("--dimension", type=int, default=None,
                        help="仅测试指定维度的数据集 (768 或 1536)")
    parser.add_argument("--dry-run", action="store_true", help="仅打印测试计划")
    parser.add_argument("--output", default=None, help="报告输出路径")
    args = parser.parse_args()

    dimension = args.dimension
    suites_to_run = list(SUITES.keys()) if args.suite == "all" else [args.suite]

    total_tasks = 0
    failed_tasks = 0

    print("=" * 70)
    print(f"  综合测试: {args.db.upper()} | 套件: {args.suite}")
    print(f"  结果目录: {RESULTS_DIR}")
    print("=" * 70)

    for suite_name in suites_to_run:
        suite_fn = SUITES[suite_name]
        print(f"\n--- {suite_name.upper()} ---")
        for task, task_label in suite_fn(args.db, dimension):
            total_tasks += 1
            print(f"  [{total_tasks}] {task_label}")
            print(f"      case={task.case_config.case_id.name}, k={task.case_config.k}")
            if task.case_config.custom_case:
                for k, v in task.case_config.custom_case.items():
                    print(f"      {k}={v}")

            if args.dry_run:
                continue

            success = run_single_task(task, task_label)
            if not success:
                failed_tasks += 1
                log.error(f"  FAILED: {task_label}")

    if args.dry_run:
        print(f"\n总计 {total_tasks} 个 task (dry-run)")
        return

    print(f"\n{'='*70}")
    print(f"  完成: {total_tasks - failed_tasks}/{total_tasks} 成功, {failed_tasks} 失败")
    print(f"{'='*70}")

    # 收集所有结果
    # EN: Collect all results.
    print("\n收集结果数据...")
    all_metrics = {}
    for db_dir_name in ["HouyiDB", "QdrantLocal"]:
        metrics = extract_metrics_from_results(db_dir_name)
        if metrics:
            all_metrics[db_dir_name] = metrics
            print(f"  {db_dir_name}: {len(metrics)} 条结果")

    # 生成报告
    # EN: Generate the report.
    output_path = Path(args.output) if args.output else RESULTS_DIR / f"benchmark_report_{datetime.now().strftime('%Y%m%d_%H%M')}.md"
    report = generate_report(all_metrics, output_path)
    print(f"\n{report}")


if __name__ == "__main__":
    main()
