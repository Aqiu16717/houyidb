#!/usr/bin/env bash
# 企业就绪记分卡入口（P4.7 前置）。实现见 maturity_report.py。
# Enterprise readiness scorecard entry point; see maturity_report.py.
#
# 用法 / Usage:
#   ./scripts/maturity_report.sh                 # markdown 报告
#   ./scripts/maturity_report.sh --json          # JSON（CI）
#   ./scripts/maturity_report.sh --gate l1       # 门禁：未达 L1 退出 1
#   ./scripts/maturity_report.sh --selftest      # 记分逻辑自测
#
# 说明 / Notes:
# - 纯静态扫描（秒级）：只认已入库的测试名/脚本/实现点，不跑测试不联网。
# - L3 需要外部审计/认证证据，本地不可评估；--gate l3 恒失败。
# - 动态门禁仍由 cargo test / chaos_drill / replication_drill 等脚本承担。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${SCRIPT_DIR}/maturity_report.py" "$@"
