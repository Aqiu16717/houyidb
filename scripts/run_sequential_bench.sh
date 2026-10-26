#!/usr/bin/env bash
# 严格顺序执行所有测试：Qdrant (gRPC) → HouyiDB (hnsw) → HouyiDB (clustered) → HouyiDB (hii-l0)
# 绝不同时运行两个测试。
# EN: Run all tests strictly in order: Qdrant (gRPC) -> HouyiDB (hnsw) -> HouyiDB
# EN: (clustered) -> HouyiDB (hii-l0). Never run two tests at the same time.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export VDB_ROOT="${VDB_ROOT:-${ROOT}/../vectorDBBench-houyidb}"
HOUYI_BIN="${ROOT}/target/release/houyi-server"
WAL_PATH="/var/folders/ym/lpc4tq_x41q4h3tdx7vmsy7m0000gn/T/houyidb-dev.wal"
HTTP_ADDR="127.0.0.1:17000"
PG_ADDR="127.0.0.1:15432"
QDRANT_BIN="$HOME/Downloads/qdrant"

export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"
export VDB_M=24 VDB_EFC=400 VDB_K=10

PY_SCRIPT="${ROOT}/scripts/run_full_benchmark_v2.py"

start_qdrant() {
  echo "  → 启动 Qdrant (gRPC on :6334)..."
  pkill -f "$QDRANT_BIN" 2>/dev/null || true; sleep 1
  "$QDRANT_BIN" > /tmp/qdrant-bench.log 2>&1 &
  sleep 3
  for i in $(seq 1 15); do
    curl -s http://127.0.0.1:6333/collections > /dev/null 2>&1 && echo "  Qdrant ready (HTTP:6333 + gRPC:6334)" && return 0
    sleep 2
  done
  echo "ERROR: Qdrant start failed"; return 1
}

stop_qdrant() {
  pkill -f "$QDRANT_BIN" 2>/dev/null || true; sleep 2
}

start_houyi() {
  local idx_type="${1:-hnsw}"
  local dim="${2:-768}"
  echo "  → 启动 HouyiDB (index=$idx_type dim=$dim)..."
  pkill -f houyi-server 2>/dev/null || true; sleep 1
  rm -f "$WAL_PATH"
  RUST_LOG=info "$HOUYI_BIN" \
    --benchmark-http-addr "$HTTP_ADDR" --pgwire-addr "$PG_ADDR" \
    --vector-dim "$dim" --index-type "$idx_type" \
    --pgwire-hnsw-m 24 --pgwire-hnsw-efc 400 \
    --wal-write-mode async > /tmp/houyidb-bench.log 2>&1 &
  sleep 3
  for i in $(seq 1 10); do
    curl -s "http://${HTTP_ADDR}/v1/health" > /dev/null 2>&1 && echo "  HouyiDB ready ($idx_type dim=$dim)" && return 0
    sleep 2
  done
  echo "ERROR: HouyiDB start failed"; return 1
}

stop_houyi() {
  pkill -f houyi-server 2>/dev/null || true; sleep 2
}

run_bench() {
  cd "$VDB_ROOT" && source .venv/bin/activate
  python "$PY_SCRIPT" 2>&1 | tee /tmp/bench-v2-output.log
}

# ═══════════════════════════════════════════════════════
# 阶段 0: Qdrant 测试 (需要 Qdrant 服务)
# EN: Stage 0: Qdrant tests (requires a Qdrant service)
# ═══════════════════════════════════════════════════════
echo "=========================================="
echo "  Phase 0: Qdrant gRPC 测试"
echo "=========================================="
start_qdrant

# 只跑 Qdrant 相关的测试 (Phase 3 & 4)
# EN: Run only Qdrant-related tests (Phase 3 & 4).
cd "$VDB_ROOT" && source .venv/bin/activate
export NO_PROXY="127.0.0.1,localhost"; export no_proxy="127.0.0.1,localhost"

echo ""
echo "--- Qdrant Cohere 1M ef=256 (gRPC) ---"
python -m vectordb_bench.cli.vectordbbench qdrantlocal \
  --url http://127.0.0.1:6333 --case-type Performance768D1M --k 10 \
  --m 24 --ef-construct 400 --hnsw-ef 256 \
  --num-concurrency "4,8,16" --concurrency-duration 30 \
  --task-label "qdrant-grpc-Cohere1M-ef256-v2" 2>&1 | tail -5

echo ""
echo "--- Qdrant LabelFilter 1% ef=128 ---"
python -m vectordb_bench.cli.vectordbbench qdrantlocal \
  --url http://127.0.0.1:6333 --case-type LabelFilterPerformanceCase --k 10 \
  --m 24 --ef-construct 400 --hnsw-ef 128 \
  --dataset-with-size-type "Medium Cohere (768dim, 1M)" \
  --label-percentage 0.01 \
  --num-concurrency "1" --concurrency-duration 30 \
  --task-label "qdrant-grpc-filter-Cohere1M-lp1-ef128-v2" 2>&1 | tail -5

echo ""
echo "--- Qdrant LabelFilter 1% ef=256 ---"
python -m vectordb_bench.cli.vectordbbench qdrantlocal \
  --url http://127.0.0.1:6333 --case-type LabelFilterPerformanceCase --k 10 \
  --m 24 --ef-construct 400 --hnsw-ef 256 \
  --dataset-with-size-type "Medium Cohere (768dim, 1M)" \
  --label-percentage 0.01 \
  --num-concurrency "1" --concurrency-duration 30 \
  --task-label "qdrant-grpc-filter-Cohere1M-lp1-ef256-v2" 2>&1 | tail -5

stop_qdrant

# ═══════════════════════════════════════════════════════
# 阶段 1: HouyiDB HNSW (默认)
# EN: Stage 1: HouyiDB HNSW (default)
# ═══════════════════════════════════════════════════════
echo ""
echo "=========================================="
echo "  Phase 1: HouyiDB HNSW (默认)"
echo "=========================================="
start_houyi hnsw 768

echo ""
echo "--- HouyiDB HNSW Cohere 1M ef=256 ---"
python -m vectordb_bench.cli.vectordbbench houyidb \
  --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
  --case-type Performance768D1M --k 10 \
  --m 24 --ef-construction 400 --ef-search 256 \
  --num-concurrency "4,8,16" --concurrency-duration 30 \
  --task-label "houyidb-hnsw-Cohere1M-ef256-v2" 2>&1 | tail -3

stop_houyi

# ═══════════════════════════════════════════════════════
# 阶段 2: HouyiDB Clustered HNSW
# EN: Stage 2: HouyiDB Clustered HNSW
# ═══════════════════════════════════════════════════════
echo ""
echo "=========================================="
echo "  Phase 2: HouyiDB Clustered HNSW"
echo "=========================================="
start_houyi clustered 768

echo ""
echo "--- HouyiDB Clustered HNSW Cohere 1M ef=256 ---"
python -m vectordb_bench.cli.vectordbbench houyidb \
  --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
  --case-type Performance768D1M --k 10 \
  --m 24 --ef-construction 400 --ef-search 256 \
  --num-concurrency "4,8,16" --concurrency-duration 30 \
  --task-label "houyidb-clustered-Cohere1M-ef256-v2" 2>&1 | tail -3

stop_houyi

# ═══════════════════════════════════════════════════════
# 阶段 3: HouyiDB HII L0
# EN: Stage 3: HouyiDB HII L0
# ═══════════════════════════════════════════════════════
echo ""
echo "=========================================="
echo "  Phase 3: HouyiDB HII L0"
echo "=========================================="
start_houyi hii-l0 768

echo ""
echo "--- HouyiDB HII L0 Cohere 1M ef=256 ---"
python -m vectordb_bench.cli.vectordbbench houyidb \
  --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
  --case-type Performance768D1M --k 10 \
  --m 24 --ef-construction 400 --ef-search 256 \
  --num-concurrency "4,8,16" --concurrency-duration 30 \
  --task-label "houyidb-hii-l0-Cohere1M-ef256-v2" 2>&1 | tail -3

stop_houyi

# ═══════════════════════════════════════════════════════
# 阶段 4: HouyiDB Streaming
# EN: Stage 4: HouyiDB Streaming
# ═══════════════════════════════════════════════════════
echo ""
echo "=========================================="
echo "  Phase 4: HouyiDB HNSW Streaming"
echo "=========================================="
start_houyi hnsw 768

echo ""
echo "--- HouyiDB Streaming Cohere 1M ---"
python << 'PYEOF'
import sys, os, time, json, logging
sys.path.insert(0, os.environ["VDB_ROOT"])
from vectordb_bench.interface import benchmark_runner
from vectordb_bench.models import CaseConfig, CaseType, ConcurrencySearchConfig, TaskConfig, TaskStage
from vectordb_bench.backend.clients import DB
from vectordb_bench.backend.clients.houyidb.config import HouyiDBConfig, HouyiDBIndexConfig
from vectordb_bench.backend.clients.api import MetricType

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
log = logging.getLogger("streaming")

label = "houyidb-streaming-Cohere1M-v2"
custom = {
    "dataset_with_size_type": "Medium Cohere (768dim, 1M)",
    "insert_rate": 500,
    "search_stages": [0.3, 0.6, 0.9],
    "concurrencies": [4, 8, 16],
    "optimize_after_write": True,
    "read_dur_after_write": 30,
}
task = TaskConfig(
    db=DB.HouyiDB,
    db_config=HouyiDBConfig(host="127.0.0.1", port=15432, transport="pgwire", http_port=17000),
    db_case_config=HouyiDBIndexConfig(metric_type=MetricType.COSINE, m=24, ef_construction=400, ef_search=256),
    case_config=CaseConfig(case_id=CaseType.StreamingPerformanceCase, k=10,
        concurrency_search_config=ConcurrencySearchConfig(num_concurrency=[4, 8, 16], concurrency_duration=30),
        custom_case=custom),
    stages=[TaskStage.DROP_OLD, TaskStage.LOAD, TaskStage.SEARCH_SERIAL, TaskStage.SEARCH_CONCURRENT],
    load_concurrency=0)

log.info(f"Starting streaming test: {label}")
ok = benchmark_runner.run([task], label)
if not ok:
    log.error("Failed to start")
    sys.exit(1)
while benchmark_runner.has_running():
    time.sleep(2)
if benchmark_runner.latest_error:
    log.error(f"Error: {benchmark_runner.latest_error}")
else:
    log.info("Streaming test complete!")
PYEOF

stop_houyi

echo ""
echo "=========================================="
echo "  全部测试完成！"
echo "=========================================="
