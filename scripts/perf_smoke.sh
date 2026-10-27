#!/usr/bin/env bash
# 性能验证冒烟：Criterion 向量基准快速采样 + 提示 VectorDBBench 下一步。
# 用法：在项目根目录执行 ./scripts/perf_smoke.sh
# EN: Performance smoke: quick Criterion vector benchmark sampling + a pointer to
# EN: the next VectorDBBench step.
# EN: Usage: run ./scripts/perf_smoke.sh from the project root.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> houyi-vector: Criterion smoke (sample-size=10, warm-up 1s)"
cargo bench -p houyi-vector --bench vector_baseline -- \
  --sample-size 10 \
  --warm-up-time 1

echo ""
echo "==> 下一步（完整性能验证）"
echo "    1) 终端 A: cargo run -p houyi-server --bin houyi-server -- --benchmark-http-addr 127.0.0.1:7000"
echo "    2) 按 VDBBENCH_PROTOCOL.md 安装/运行 benchmarks/vectordb_bench_houyidb"
echo "    3) 结果按 docs/benchmarks/houyidb-performance-2026-10.md 的格式记录"
