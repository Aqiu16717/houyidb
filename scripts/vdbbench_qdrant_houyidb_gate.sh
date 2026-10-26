#!/usr/bin/env bash
# VectorDBBench：QdrantLocal 与 HouyiDB（HTTP）公平对照 — 统一参数、结果落盘、可选 recall/ndcg 门禁。
# Fair compare: same VDB_* env params for both; writes JSON under $VDB_ROOT/vectordb_bench/results/ (default: sibling repo).
#
# 用法 / Usage（在项目根目录）:
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh check
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh preflight         # 释放 17000/15432、停 houyi-server、清 dev WAL（跑分前建议先执行）
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh dry-run
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh run-houyidb    # 需先启动 houyi-server，见下
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh run-qdrant       # 需先启动 Qdrant
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh compare <qdrant.json> <houyidb.json> [--min-recall 0.998] [--min-ndcg 0.998]
#   # run-houyidb / run-qdrant 成功结束后会多打一行 JSON 中的 recall/ndcg（终端任务表常不显示 ndcg）
#   ./scripts/vdbbench_qdrant_houyidb_gate.sh snapshot-template <qdrant.json> <houyidb.json>
#
# 本机 HouyiDB 建议: NO_PROXY=127.0.0.1,localhost（避免代理劫持 localhost）
# 数据集: DATASET_LOCAL_DIR（见 $VDB_ROOT/.env.example；默认 VDB_ROOT 与 houyidb 仓库同级）

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VDB_ROOT="${VDB_ROOT:-$(cd "${ROOT}/.." && pwd)/vectorDBBench-houyidb}"

# —— 公平对照参数（两侧必须一致）/ Shared params for both DBs ——
# EN: Fair comparison parameters (must be identical on both sides).
export VDB_CASE="${VDB_CASE:-Performance1536D50K}"
export VDB_K="${VDB_K:-10}"
export VDB_M="${VDB_M:-24}"
export VDB_EFC="${VDB_EFC:-400}"
export VDB_EFS="${VDB_EFS:-256}"
# 逗号分隔，与官方 CLI 一致 / comma-separated
# EN: comma-separated, consistent with the official CLI.
export VDB_NUM_CONCURRENCY="${VDB_NUM_CONCURRENCY:-1}"
export VDB_CONCURRENCY_DURATION="${VDB_CONCURRENCY_DURATION:-30}"

export HOUYI_HOST="${HOUYI_HOST:-127.0.0.1}"
# 按最快路径默认走 pgwire；HTTP 仅用于 health/optimize 控制面。
# EN: Default to pgwire data path; HTTP is control plane for health/optimize.
export VDB_HOUYI_TRANSPORT="${VDB_HOUYI_TRANSPORT:-pgwire}"
export HOUYI_PG_PORT="${HOUYI_PG_PORT:-15432}"
export HOUYI_HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
export QDRANT_URL="${QDRANT_URL:-http://127.0.0.1:6333}"
# 加载阶段并发：0 = cpu_count（VectorDBBench 默认）；设为 8 可与常见多线程加载对齐。
# Load-stage concurrency: 0 = cpu_count (VectorDBBench default); use 8 to align with typical multi-thread load.
export VDB_LOAD_CONCURRENCY="${VDB_LOAD_CONCURRENCY:-0}"

TASK_TS="$(date +%Y%m%d)"
TASK_QDRANT="${TASK_LABEL_QDRANT:-qdrant-fair-${TASK_TS}}"
TASK_HOUYI="${TASK_LABEL_HOUYI:-houyidb-fair-${TASK_TS}}"

_run_venv() {
  cd "${VDB_ROOT}"
  if [[ ! -d .venv ]]; then
    echo "ERROR: ${VDB_ROOT}/.venv missing. Run: python3 -m venv .venv && pip install -e '.[test,qdrant]'" >&2
    exit 1
  fi
  # 加载 venv；shellcheck 无需解析该文件
  # EN: Load the venv; shellcheck does not need to resolve this file.
  # shellcheck source=/dev/null
  source .venv/bin/activate
  export NO_PROXY="${NO_PROXY:-127.0.0.1,localhost}"
  export no_proxy="${no_proxy:-127.0.0.1,localhost}"
}

_check_http_ready() {
  local url="$1"
  local name="$2"
  local timeout_secs="${3:-2}"
  if ! python3 - "$url" "$timeout_secs" <<'PY'
import sys, urllib.request
url = sys.argv[1]
timeout = float(sys.argv[2])
try:
    with urllib.request.urlopen(url, timeout=timeout) as r:
        # 2xx/3xx 都视为可达
        # EN: Both 2xx/3xx are considered reachable.
        sys.exit(0 if 200 <= r.status < 400 else 1)
except Exception:
    sys.exit(1)
PY
  then
    echo "ERROR: ${name} not reachable at ${url}" >&2
    return 1
  fi
}

# 从最新 result_*.json 打印 recall/ndcg 等，避免仅依赖 VDB 终端表（其列常不展示 ndcg）。
# EN: Log line summary after each run; ground truth is still the JSON on disk.
_vdb_print_latest_result_metrics() {
  local which="${1:-HouyiDB}"
  local d="${VDB_ROOT}/vectordb_bench/results/${which}"
  if [[ ! -d "$d" ]]; then
    echo "WARN: VDB results directory missing: ${d}" >&2
    return 0
  fi
  local latest
  latest=$(ls -t "$d"/result_*.json 2>/dev/null | head -1) || true
  if [[ -z "${latest}" ]]; then
    echo "WARN: no result_*.json under ${d}" >&2
    return 0
  fi
  python3 - "$latest" "$which" <<'PY'
import json, sys

def fmt(v):
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:.6g}"
    return str(v)

path, label = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as f:
    d = json.load(f)
m = (d.get("results") or [{}])[0].get("metrics") or {}
# 质量指标优先，便于与 recall 门禁/基线表对齐
# EN: Quality metrics first, for alignment with the recall gate/baseline tables.
recall, ndcg = m.get("recall"), m.get("ndcg")
line1 = (
    f"    recall={fmt(recall)}  ndcg={fmt(ndcg)}  "
    f"load_duration={fmt(m.get('load_duration'))}  qps={fmt(m.get('qps'))}"
)
rest = [
    f"{k}={fmt(m.get(k))}"
    for k in ("insert_duration", "optimize_duration", "serial_latency_p99", "serial_latency_p95")
    if m.get(k) is not None
]
print(f"==> [{label}] VDB result metrics (JSON)")
print(line1)
if rest:
    print("    " + "  ".join(rest))
print(f"    file={path}")
PY
}

cmd_preflight() {
  # 与 HOUYI_HTTP_PORT / HOUYI_PG_PORT 对齐，保证 VDB 前无旧服务、无旧 WAL。
  # EN: Align ports and cold-start state; see scripts/vdb_houyidb_preflight.sh
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh"
}

cmd_check() {
  _run_venv
  python3 -c "import vectordb_bench; print('vectordb_bench OK')"
  echo "VDB_CASE=${VDB_CASE} VDB_K=${VDB_K} m=${VDB_M} ef_c=${VDB_EFC} ef_s=${VDB_EFS} conc=${VDB_NUM_CONCURRENCY} dur=${VDB_CONCURRENCY_DURATION} load_concurrency=${VDB_LOAD_CONCURRENCY}"
  echo "HOUYI transport=${VDB_HOUYI_TRANSPORT} pg=${HOUYI_HOST}:${HOUYI_PG_PORT} http=${HOUYI_HOST}:${HOUYI_HTTP_PORT} QDRANT_URL=${QDRANT_URL}"
}

cmd_dry_run() {
  _run_venv
  python3 -m vectordb_bench.cli.vectordbbench houyidb \
    --host "${HOUYI_HOST}" --port "${HOUYI_PG_PORT}" --http-port "${HOUYI_HTTP_PORT}" --transport "${VDB_HOUYI_TRANSPORT}" \
    --case-type "${VDB_CASE}" --k "${VDB_K}" \
    --m "${VDB_M}" --ef-construction "${VDB_EFC}" --ef-search "${VDB_EFS}" \
    --load-concurrency "${VDB_LOAD_CONCURRENCY}" \
    --num-concurrency "${VDB_NUM_CONCURRENCY}" \
    --concurrency-duration "${VDB_CONCURRENCY_DURATION}" \
    --task-label "${TASK_HOUYI}-dryrun" \
    --dry-run
}

cmd_run_houyidb() {
  _run_venv
  _check_http_ready "http://${HOUYI_HOST}:${HOUYI_HTTP_PORT}/v1/health" "HouyiDB benchmark API"
  echo "==> HouyiDB benchmark (transport=${VDB_HOUYI_TRANSPORT}, pg=${HOUYI_HOST}:${HOUYI_PG_PORT}, http=${HOUYI_HOST}:${HOUYI_HTTP_PORT})"
  # 可选 HNSW 调优（与公平对照无关，需在启动 houyi-server 前 export；勿默认开 OPTIMIZE_DRAIN_ALL=1 以免极端大批次 OOM/变慢）
  # Optional HNSW tuning: export before starting houyi-server, e.g. HOUYI_HNSW_MAX_BUILD_BATCH, HOUYI_HNSW_PARALLEL_BATCH_BUILD=1
  echo "    (optional) HOUYI_HNSW_MAX_BUILD_BATCH, HOUYI_HNSW_PARALLEL_BATCH_BUILD, HOUYI_HNSW_OPTIMIZE_DRAIN_ALL — see houyi-vector run_async_builder_once / worker"
  echo "    optimize 对账：看 **运行 houyi-server 的终端** 每行 [houyi-optimize]；或 export HOUYI_HNSW_OPTIMIZE_LOG_FILE=/tmp/houyi-optimize.log"
  echo "    构图种子：HOUYI_HNSW_SEED_SERIAL_POINTS（默认 256，与 Qdrant 单线程种子段同旨；0=关闭仅并行 plan 余量）"
  python3 -m vectordb_bench.cli.vectordbbench houyidb \
    --host "${HOUYI_HOST}" --port "${HOUYI_PG_PORT}" --http-port "${HOUYI_HTTP_PORT}" --transport "${VDB_HOUYI_TRANSPORT}" \
    --case-type "${VDB_CASE}" --k "${VDB_K}" \
    --m "${VDB_M}" --ef-construction "${VDB_EFC}" --ef-search "${VDB_EFS}" \
    --load-concurrency "${VDB_LOAD_CONCURRENCY}" \
    --num-concurrency "${VDB_NUM_CONCURRENCY}" \
    --concurrency-duration "${VDB_CONCURRENCY_DURATION}" \
    --task-label "${TASK_HOUYI}"
  _vdb_print_latest_result_metrics HouyiDB
}

cmd_run_qdrant() {
  _run_venv
  _check_http_ready "${QDRANT_URL}/collections" "Qdrant"
  echo "==> QdrantLocal benchmark (ensure Qdrant is up at ${QDRANT_URL})"
  python3 -m vectordb_bench.cli.vectordbbench qdrantlocal \
    --url "${QDRANT_URL}" \
    --case-type "${VDB_CASE}" --k "${VDB_K}" \
    --m "${VDB_M}" --ef-construct "${VDB_EFC}" --hnsw-ef "${VDB_EFS}" \
    --load-concurrency "${VDB_LOAD_CONCURRENCY}" \
    --num-concurrency "${VDB_NUM_CONCURRENCY}" \
    --concurrency-duration "${VDB_CONCURRENCY_DURATION}" \
    --task-label "${TASK_QDRANT}"
  _vdb_print_latest_result_metrics QdrantLocal
}

cmd_compare() {
  shift
  exec python3 "${ROOT}/scripts/vdbbench_compare_results.py" "$@"
}

cmd_snapshot_template() {
  shift
  local qdrant_json="${1:-}"
  local houyi_json="${2:-}"
  if [[ -z "${qdrant_json}" || -z "${houyi_json}" ]]; then
    echo "Usage: $0 snapshot-template <qdrant.json> <houyidb.json>" >&2
    exit 1
  fi
  if [[ ! -f "${qdrant_json}" ]]; then
    echo "ERROR: qdrant json not found: ${qdrant_json}" >&2
    exit 1
  fi
  if [[ ! -f "${houyi_json}" ]]; then
    echo "ERROR: houyidb json not found: ${houyi_json}" >&2
    exit 1
  fi
  local now_date
  now_date="$(date +%F)"
  local git_sha="unknown"
  if command -v git >/dev/null 2>&1; then
    git_sha="$(git -C "${ROOT}" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  fi
  local q_recall q_ndcg q_p95 q_p99 q_qps q_load
  local h_recall h_ndcg h_p95 h_p99 h_qps h_load
  q_recall="$(python3 - "$qdrant_json" "recall" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  q_ndcg="$(python3 - "$qdrant_json" "ndcg" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  q_p95="$(python3 - "$qdrant_json" "serial_latency_p95" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  q_p99="$(python3 - "$qdrant_json" "serial_latency_p99" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  q_qps="$(python3 - "$qdrant_json" "qps" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  q_load="$(python3 - "$qdrant_json" "load_duration" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_recall="$(python3 - "$houyi_json" "recall" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_ndcg="$(python3 - "$houyi_json" "ndcg" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_p95="$(python3 - "$houyi_json" "serial_latency_p95" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_p99="$(python3 - "$houyi_json" "serial_latency_p99" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_qps="$(python3 - "$houyi_json" "qps" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"
  h_load="$(python3 - "$houyi_json" "load_duration" <<'PY'
import json,sys
p,k=sys.argv[1],sys.argv[2]
try:
  d=json.load(open(p,encoding="utf-8"))
  m=(d.get("results") or [{}])[0].get("metrics") or {}
  v=m.get(k)
  print("-" if v is None else v)
except Exception:
  print("-")
PY
)"

  cat <<EOF
### Gate 运行记录（${now_date}）

1) 参数快照
\`\`\`text
日期：${now_date}
机器/系统：
git commit：${git_sha}
VDB_CASE=${VDB_CASE}
VDB_K=${VDB_K}
VDB_M=${VDB_M}
VDB_EFC=${VDB_EFC}
VDB_EFS=${VDB_EFS}
VDB_NUM_CONCURRENCY=${VDB_NUM_CONCURRENCY}
VDB_CONCURRENCY_DURATION=${VDB_CONCURRENCY_DURATION}
HOUYI_HTTP_PORT=${HOUYI_HTTP_PORT}
QDRANT_URL=${QDRANT_URL}
\`\`\`

2) 执行命令
\`\`\`bash
./scripts/vdbbench_qdrant_houyidb_gate.sh check
./scripts/vdbbench_qdrant_houyidb_gate.sh run-qdrant
./scripts/vdbbench_qdrant_houyidb_gate.sh run-houyidb
./scripts/vdbbench_qdrant_houyidb_gate.sh compare "${qdrant_json}" "${houyi_json}" --min-recall 0.998
\`\`\`

3) 结果文件
\`\`\`text
Qdrant JSON: ${qdrant_json}
HouyiDB JSON: ${houyi_json}
compare 输出（摘要）:
\`\`\`

4) 结论摘要
| 项 | HouyiDB | Qdrant | 结论 |
| --- | --- | --- | --- |
| recall@k | ${h_recall} | ${q_recall} | 待填 |
| ndcg@k | ${h_ndcg} | ${q_ndcg} | 待填 |
| p95 latency | ${h_p95} | ${q_p95} | 待填 |
| p99 latency | ${h_p99} | ${q_p99} | 待填 |
| qps | ${h_qps} | ${q_qps} | 待填 |
| load_duration | ${h_load} | ${q_load} | 待填 |

5) 风险与偏差记录
- 是否同机同负载窗口（是/否）：
- 是否发生异常（OOM/timeout/重试/连接失败）：
- 是否存在不可比因素（参数不一致、服务重启、数据缓存污染）：
- 若有偏差，本次结果是否作废（是/否）：
EOF
}

usage() {
  sed -n '1,21p' "$0" | tail -n +2
}

main() {
  local sub="${1:-}"
  case "${sub}" in
    check) cmd_check ;;
    preflight|pf) cmd_preflight ;;
    dry-run) cmd_dry_run ;;
    run-houyidb) cmd_run_houyidb ;;
    run-qdrant) cmd_run_qdrant ;;
    compare) cmd_compare "$@" ;;
    snapshot-template) cmd_snapshot_template "$@" ;;
    ""|-h|--help|help) usage ;;
    *)
      echo "Unknown: ${sub}" >&2
      usage
      exit 1
      ;;
  esac
}

main "$@"
