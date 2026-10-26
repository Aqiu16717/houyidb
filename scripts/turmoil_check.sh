#!/usr/bin/env bash
# P4.5.5 turmoil 确定性网络仿真（本地门禁）。
#
# 为什么单独一个 feature / Why a dedicated feature：`turmoil-dst` 会把
# houyi-persistence 的网络类型换到 turmoil 的 drop-in 实现（仿真网络接管）。
# 它**只能**用于本测试目标——带进常规全量测试会让所有复制单测跑在仿真
# 网络上（没有 sim 上下文时直接失败）。
# EN: `turmoil-dst` swaps the replication layer's network types; it must only
# be enabled for this target, never for the regular suite.
#
# 覆盖 / Covered (tests/turmoil_replication.rs):
#   1. 基础链路：live 推送 + DURABLE ACK 水位线；
#   2. 分区 → 恢复：分区期间写只进 WAL，恢复后 catch-up，apply 序列无洞无重；
#   3. follower_ahead：LSN 超前的 follower 被拒绝并计数（脑裂信号）；
#   4. 确定性：同 seed 两次运行得到相同 apply 序列。
#
# 用法 / Usage:
#   ./scripts/turmoil_check.sh
#
# 已知边界 / Known limits: 只仿真**网络面**。WAL/文件 IO 与 wal worker 仍是
# 真实世界（存储面由 P4.5.4 的 kill -9 演练覆盖）；每个 turmoil host 内部跑
# 真实的 current-thread tokio runtime，定时器接到模拟时钟。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
cd "$ROOT"

echo "── turmoil DST（houyi-server --features turmoil-dst）──"
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
if ! cargo test -p houyi-server --features turmoil-dst --test turmoil_replication 2>&1 | tee "$LOG"; then
    echo "!! turmoil DST 失败（完整输出见上方）" >&2
    exit 1
fi
# `cargo test` 在过滤匹配 0 个测试时仍退出 0：断言确实跑了用例。
# EN: `cargo test` still exits 0 when the filter matches 0 tests: assert that tests
# EN: actually ran.
PASSED="$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$LOG" | tail -1)"
if [[ -z "${PASSED:-}" || "$PASSED" -eq 0 ]]; then
    echo "!! turmoil DST 匹配到 0 个测试（用例被重命名/门控掉了？）" >&2
    exit 1
fi
echo "    （${PASSED} 个用例）"


echo "turmoil DST 全绿。"
