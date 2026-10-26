#!/usr/bin/env bash
# 逐个运行测试，每个 Cohere 1M 任务后重启 HouyiDB
# EN: Run tests one by one; restart HouyiDB after each Cohere 1M task.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VD="${ROOT}/.."
VDB_ROOT="${VD}/vectorDBBench-houyidb"
HOUYI_BIN="${ROOT}/target/release/houyi-server"
WAL_PATH="/var/folders/ym/lpc4tq_x41q4h3tdx7vmsy7m0000gn/T/houyidb-dev.wal"

export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

start_houyi() {
  local dim="${1:-768}"
  echo "  Starting HouyiDB (dim=${dim})..."
  pkill -f houyi-server 2>/dev/null || true
  sleep 1
  rm -f "${WAL_PATH}"
  RUST_LOG=info "${HOUYI_BIN}" \
    --benchmark-http-addr 127.0.0.1:17000 \
    --pgwire-addr 127.0.0.1:15432 \
    --vector-dim "${dim}" \
    --pgwire-hnsw-m 24 \
    --pgwire-hnsw-efc 400 \
    --wal-write-mode async \
    > /tmp/houyidb-bench.log 2>&1 &
  sleep 3
  for i in $(seq 1 10); do
    if curl -s http://127.0.0.1:17000/v1/health > /dev/null 2>&1; then
      echo "  HouyiDB ready"
      return 0
    fi
    sleep 2
  done
  echo "ERROR: HouyiDB failed to start" >&2
  return 1
}

stop_houyi() {
  pkill -f houyi-server 2>/dev/null || true
  sleep 2
}

run_one() {
  local case_type="$1" label="$2" efs_arg="$3" dim="$4"
  export VDB_CASE="${case_type}"
  export VDB_EFS="${efs_arg}"
  export VDB_M=24 VDB_EFC=400 VDB_K=10
  export VDB_NUM_CONCURRENCY="4,8,16"
  export VDB_CONCURRENCY_DURATION=30
  local ts=$(date +%Y%m%d)
  export TASK_LABEL_HOUYI="houyidb-${label}-ef${efs_arg}-${ts}"

  cd "${VDB_ROOT}" && source .venv/bin/activate
  echo "  Task: case=${case_type} ef=${efs_arg} label=${TASK_LABEL_HOUYI}"
  python -m vectordb_bench.cli.vectordbbench houyidb \
    --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
    --case-type "${case_type}" --k 10 \
    --m 24 --ef-construction 400 --ef-search "${efs_arg}" \
    --num-concurrency "4,8,16" --concurrency-duration 30 \
    --task-label "${TASK_LABEL_HOUYI}"
  echo "  ✓ Done"
}

# ═══════  Cohere 768d 1M (慢数据集, 每轮重启服务器)  ═══════
# EN: Cohere 768d 1M (slow dataset, server restarted each round)
for ef in 128 256 512; do
  echo ""
  echo "===== Cohere 768d 1M  ef=${ef} ====="
  start_houyi 768
  run_one "Performance768D1M" "Cohere768d_1M" "${ef}" 768
  stop_houyi
done

# ═══════  OpenAI 1536d 50K (快数据集, 无需重启)  ═══════
# EN: OpenAI 1536d 50K (fast dataset, no restart needed)
start_houyi 1536
for ef in 64 128 256 512; do
  echo ""
  echo "===== OpenAI 1536d 50K  ef=${ef} ====="
  run_one "Performance1536D50K" "OpenAI1536d_50K" "${ef}" 1536
done
stop_houyi

echo ""
echo "===== All read-only tests complete ====="
