#!/usr/bin/env bash
# Qdrant 只读测试 — 与 HouyiDB 对称的测试矩阵
# EN: Qdrant read-only test — the symmetric test matrix to HouyiDB.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VD="${ROOT}/.."
VDB_ROOT="${VD}/vectorDBBench-houyidb"

export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

start_qdrant() {
  echo "  Starting Qdrant..."
  pkill -f qdrant 2>/dev/null || true
  sleep 1
  ~/Downloads/qdrant --uri http://127.0.0.1:6333 > /tmp/qdrant-bench.log 2>&1 &
  sleep 3
  for i in $(seq 1 10); do
    if curl -s http://127.0.0.1:6333/collections > /dev/null 2>&1; then
      echo "  Qdrant ready"
      return 0
    fi
    sleep 2
  done
  echo "ERROR: Qdrant failed to start" >&2
  return 1
}

stop_qdrant() {
  pkill -f qdrant 2>/dev/null || true
  sleep 2
}

run_one() {
  local case_type="$1" label="$2" efs_arg="$3"
  export VDB_CASE="${case_type}"
  export VDB_EFS="${efs_arg}"
  export VDB_M=24 VDB_EFC=400 VDB_K=10
  export VDB_NUM_CONCURRENCY="4,8,16"
  export VDB_CONCURRENCY_DURATION=30
  local ts=$(date +%Y%m%d)
  export TASK_LABEL_QDRANT="qdrant-${label}-ef${efs_arg}-${ts}"

  cd "${VDB_ROOT}" && source .venv/bin/activate
  echo "  Task: case=${case_type} ef=${efs_arg} label=${TASK_LABEL_QDRANT}"
  python -m vectordb_bench.cli.vectordbbench qdrantlocal \
    --url http://127.0.0.1:6333 \
    --case-type "${case_type}" --k 10 \
    --m 24 --ef-construct 400 --hnsw-ef "${efs_arg}" \
    --num-concurrency "4,8,16" --concurrency-duration 30 \
    --task-label "${TASK_LABEL_QDRANT}"
  echo "  ✓ Done"
}

start_qdrant

# Cohere 768d 1M
for ef in 128 256 512; do
  echo ""
  echo "===== Qdrant  Cohere 768d 1M  ef=${ef} ====="
  run_one "Performance768D1M" "Cohere768d_1M" "${ef}"
  # Qdrant 不会崩溃，无需重启
  # EN: Qdrant does not crash, so no restart is needed.
done

# OpenAI 1536d 50K
for ef in 64 128 256 512; do
  echo ""
  echo "===== Qdrant  OpenAI 1536d 50K  ef=${ef} ====="
  run_one "Performance1536D50K" "OpenAI1536d_50K" "${ef}"
done

stop_qdrant
echo ""
echo "===== All Qdrant read-only tests complete ====="
