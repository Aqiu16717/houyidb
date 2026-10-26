#!/usr/bin/env bash
# HNSW / 向量路径：单步改进后的性能验证（Criterion + 单测）。
# 原则：每完成一项完整改进，应用本脚本或等价命令对比前后指标，确认无回退或记录收益。
# Principle: after each coherent HNSW change, run this (or equivalent) to compare before/after.
#
# 用法（仓库根目录）:
#   ./scripts/vector_hnsw_step_verify.sh
#   PERF_SMOKE_SAMPLE_SIZE=20 ./scripts/vector_hnsw_step_verify.sh
#
# 输出：终端打印 Criterion 摘要；建议在改进前后各保存一份日志以便 diff：
#   ./scripts/vector_hnsw_step_verify.sh 2>&1 | tee /tmp/houyi-vector-step-after.txt

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SAMPLE="${PERF_SMOKE_SAMPLE_SIZE:-10}"
WARM="${PERF_SMOKE_WARMUP_SECS:-1}"

echo "==> vector_hnsw_step_verify: cargo test -p houyi-vector"
cargo test -p houyi-vector -q

run_bench() {
  local label="$1"
  local filter="$2"
  echo ""
  echo "==> Criterion: ${label} (filter='${filter}', sample=${SAMPLE}, warm-up ${WARM}s)"
  cargo bench -p houyi-vector --bench vector_baseline -- \
    --sample-size "${SAMPLE}" \
    --warm-up-time "${WARM}" \
    --noplot \
    "${filter}"
}

run_bench "distance_pure (hot distance kernels)" "distance_pure"
run_bench "build_flush_once (insert_batch + one builder drain)" "dims_batch"

echo ""
echo "==> 完成。若需全量 recall 组，请运行: ./scripts/perf_smoke.sh"
echo "==> 更广冒烟（insert/search/build）：./scripts/perf_smoke_fast.sh"
echo "==> VectorDBBench 端到端对照：见与 houyidb 同级的 vectorDBBench-houyidb 与 gate 脚本"
