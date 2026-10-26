#!/usr/bin/env python3
"""Qdrant gRPC StreamingPerformanceCase — 并发写入+查询测试
EN: Qdrant gRPC StreamingPerformanceCase — concurrent write + query test."""
import sys, time, logging, json, os
import multiprocessing as mp

def main():
    # VectorDBBench 姊妹检出目录：默认 <repo>/../vectorDBBench-houyidb，可用 VDB_ROOT 覆盖
    # EN: VectorDBBench sibling checkout: defaults to <repo>/../vectorDBBench-houyidb;
    # EN: override with VDB_ROOT.
    _VDB = os.environ.get("VDB_ROOT") or os.path.abspath(
        os.path.join(os.path.dirname(__file__), "..", "..", "vectorDBBench-houyidb"))
    sys.path.insert(0, _VDB)
    os.chdir(_VDB)

    from vectordb_bench.interface import benchmark_runner
    from vectordb_bench.models import (
        CaseConfig, CaseType, ConcurrencySearchConfig, TaskConfig, TaskStage,
    )
    from vectordb_bench.backend.clients import DB
    from vectordb_bench.backend.clients.qdrant_local.config import QdrantLocalConfig, QdrantLocalIndexConfig
    from pydantic import SecretStr

    logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
    log = logging.getLogger("qdrant-streaming")

    label = "qdrant-grpc-streaming-Cohere1M-v1"
    custom = {
        "dataset_with_size_type": "Medium Cohere (768dim, 1M)",
        "insert_rate": 500,
        "search_stages": [0.3, 0.6, 0.9],
        "concurrencies": [4, 8, 16],
        "optimize_after_write": True,
        "read_dur_after_write": 30,
    }
    task = TaskConfig(
        db=DB.QdrantLocal,
        db_config=QdrantLocalConfig(url=SecretStr("http://127.0.0.1:6333")),
        db_case_config=QdrantLocalIndexConfig(m=24, ef_construct=400, hnsw_ef=256),
        case_config=CaseConfig(
            case_id=CaseType.StreamingPerformanceCase, k=10,
            concurrency_search_config=ConcurrencySearchConfig(num_concurrency=[4, 8, 16], concurrency_duration=30),
            custom_case=custom,
        ),
        stages=[TaskStage.DROP_OLD, TaskStage.LOAD, TaskStage.SEARCH_SERIAL, TaskStage.SEARCH_CONCURRENT],
        load_concurrency=0,
    )

    log.info("▶ Qdrant gRPC Streaming test: %s", label)
    ok = benchmark_runner.run([task], label)
    if not ok:
        log.error("Failed to start"); sys.exit(1)

    log.info("Waiting (expect ~40-60 min for 1M @ 500/s + searches)...")
    while benchmark_runner.has_running():
        time.sleep(5)

    err = benchmark_runner.latest_error
    if err:
        log.error("Error: %s", err)
        sys.exit(1)

    log.info("✓ Complete")
    import glob as g
    files = sorted(g.glob("vectordb_bench/results/QdrantLocal/result_*streaming*v1*.json"), key=os.path.getmtime, reverse=True)
    for f in files[:1]:
        d = json.load(open(f))
        for r in d.get("results", []):
            m = r.get("metrics", {})
            log.info("st_stages=%s", m.get("st_search_stage_list"))
            log.info("st_recall=%s", [round(x,4) if x else x for x in m.get("st_recall_list", [])])
            log.info("st_max_qps=%s", [round(x,1) if x else x for x in m.get("st_max_qps_list_list", [])])
            log.info("st_conc_qps=%s", [[round(y,1) for y in x] if x else x for x in m.get("st_conc_qps_list_list", [])])
            log.info("st_p99=%s", [round(x*1000,1) if x else x for x in m.get("st_serial_latency_p99_list", [])])
    if not files:
        log.warning("No result files found")

if __name__ == "__main__":
    mp.freeze_support()
    main()
