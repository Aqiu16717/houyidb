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
echo "==> 下一步（完整性能验证，需人工对照 Qdrant）"
echo "    1) 终端 A: cargo run -p houyi-server --bin houyi-server -- --benchmark-http-addr 127.0.0.1:7000"
echo "    2) 终端 B: cd ../vectorDBBench-houyidb && source .venv/bin/activate"
echo "    3) 见 VDBBENCH_PROTOCOL.md §4 中 houyidb --transport http 与 qdrantlocal 同参命令"
echo "    4) 将摘要写入 bench-baseline.md §P0.4，并勾选 TODO.md P0.4 对应项"
