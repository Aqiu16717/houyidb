#!/usr/bin/env bash
# 快速冒烟：只跑 insert / search / build_flush，跳过最慢的 recall_check_hnsw_after_flush 组。
# EN: Fast smoke: run only insert / search / build_flush, skipping the slowest
# EN: recall_check_hnsw_after_flush group.
# 用法：在项目根目录执行 ./scripts/perf_smoke_fast.sh
# EN: Usage: run ./scripts/perf_smoke_fast.sh from the project root.
# 完整含 recall 组请用 ./scripts/perf_smoke.sh
# EN: For the full run including the recall group, use ./scripts/perf_smoke.sh.
#
# 过滤串说明（Criterion 子串匹配；须避免与 recall 组 id 误匹配）：
# EN: Filter strings (Criterion substring matching; avoid accidental matches
# EN: with recall group ids):
#   - insert_single → 仅 insert 组
# EN:   - insert_single -> only the insert group
#   - dims_count    → 仅 search_hot_buffer/dims_count/…（recall 使用 dims_total_k_ef）
# EN:   - dims_count    -> only search_hot_buffer/dims_count/... (recall uses dims_total_k_ef)
#   - dims_batch    → 仅 build_flush_once/dims_batch/…
# EN:   - dims_batch    -> only build_flush_once/dims_batch/...

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Criterion 要求 sample-size >= 10
# EN: Criterion requires sample-size >= 10.
SAMPLE="${PERF_SMOKE_SAMPLE_SIZE:-10}"
WARM="${PERF_SMOKE_WARMUP_SECS:-1}"

run_filter() {
  local label="$1"
  local filter="$2"
  echo ""
  echo "==> houyi-vector: Criterion — ${label} (filter='${filter}', sample-size=${SAMPLE}, warm-up ${WARM}s)"
  cargo bench -p houyi-vector --bench vector_baseline -- \
    --sample-size "${SAMPLE}" \
    --warm-up-time "${WARM}" \
    "${filter}"
}

echo "==> perf_smoke_fast: insert + search(dims_count) + build_flush(dims_batch)，跳过 recall 组"
run_filter "insert_single" "insert_single"
run_filter "search_hot_buffer" "dims_count"
run_filter "build_flush_once" "dims_batch"

echo ""
echo "==> 完成。若需 recall_check_hnsw_after_flush，请运行: ./scripts/perf_smoke.sh"
echo "==> VectorDBBench / Qdrant 对标见 VDBBENCH_PROTOCOL.md"
