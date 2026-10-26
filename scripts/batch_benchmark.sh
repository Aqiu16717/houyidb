#!/usr/bin/env bash
# 批量基准测试: 覆盖多数据集 × 多 ef_search × 多并发
# 使用已有的 gate 脚本，保证可靠性
# EN: Batch benchmark: cover multiple datasets x multiple ef_search x multiple
# EN: concurrency. Reuse the existing gate script to keep results reliable.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="${ROOT}/scripts/vdbbench_qdrant_houyidb_gate.sh"

export VDB_M=24
export VDB_EFC=400
export VDB_K=10
export VDB_NUM_CONCURRENCY="4,8,16"
export VDB_CONCURRENCY_DURATION=30
export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"

TARGET="${1:-houyidb}"
MODE="${2:-readonly}"

run_readonly() {
  # 只读测试矩阵
  # Cohere 768d 1M: ef=128,256,512
  # OpenAI 1536d 50K: ef=64,128,256,512
  local cases=(
    "Performance768D1M:Cohere768d_1M:128,256,512"
    "Performance1536D50K:OpenAI1536d_50K:64,128,256,512"
  )

  for case_entry in "${cases[@]}"; do
    IFS=':' read -r case_type label efs <<< "$case_entry"
    IFS=',' read -ra ef_array <<< "$efs"
    for ef in "${ef_array[@]}"; do
      echo ""
      echo "================================================================"
      echo "  READONLY: ${label} ef=${ef}"
      echo "================================================================"
      export VDB_CASE="${case_type}"
      export VDB_EFS="${ef}"
      export TASK_LABEL_HOUYI="houyidb-${label}-ef${ef}-$(date +%Y%m%d)"
      export TASK_LABEL_QDRANT="qdrant-${label}-ef${ef}-$(date +%Y%m%d)"

      if [[ "${TARGET}" == "houyidb" ]]; then
        bash "${GATE}" run-houyidb
      else
        bash "${GATE}" run-qdrant
      fi

      echo "  ✓ ${label} ef=${ef} done"
    done
  done
}

run_streaming() {
  echo "Streaming test: 使用 Python 脚本..."
  cd "$(dirname "${ROOT}")/vectorDBBench-houyidb"
  source .venv/bin/activate
  python "${ROOT}/scripts/run_full_benchmark.py" --db "${TARGET}" --suite streaming 2>&1
}

case "${MODE}" in
  readonly) run_readonly ;;
  streaming) run_streaming ;;
  all)
    run_readonly
    run_streaming
    ;;
  *)
    echo "Usage: $0 <houyidb|qdrant> <readonly|streaming|all>" >&2
    exit 1
    ;;
esac
