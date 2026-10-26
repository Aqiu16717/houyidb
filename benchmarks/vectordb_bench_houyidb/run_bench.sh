#!/usr/bin/env bash
# 中文：使用 VDBBENCH_PROTOCOL.md 的默认参数运行 HouyiDB benchmark。
# EN: Run the HouyiDB benchmark with the defaults from VDBBENCH_PROTOCOL.md.
#
# 中文：前置条件：
#   1. install.py 已把适配器装入 VDB_ROOT；
#   2. houyidb-server 已在空 WAL 上启动；
#   3. VDB_ROOT 指向 VectorDBBench 源码目录。
# EN: Prerequisites:
#   1. install.py has installed the adapter into VDB_ROOT;
#   2. houyidb-server is running on an empty WAL;
#   3. VDB_ROOT points to a VectorDBBench source checkout.
#
# 用法 / Usage:
#   VDB_ROOT=/path/to/VectorDBBench \
#     ./benchmarks/vectordb_bench_houyidb/run_bench.sh [extra vectordb_bench args...]

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  VDB_ROOT=/path/to/VectorDBBench \
    ./benchmarks/vectordb_bench_houyidb/run_bench.sh [extra args...]

Required:
  VDB_ROOT   VectorDBBench source checkout

Common optional variables:
  HOUYI_HOST, HOUYI_PG_PORT, HOUYI_HTTP_PORT, VDB_HOUYI_TRANSPORT,
  VDB_CASE, VDB_K, VDB_M, VDB_EFC, VDB_EFS, VDB_NUM_CONCURRENCY,
  VDB_CONCURRENCY_DURATION, VDB_LOAD_CONCURRENCY, VDB_METRIC,
  HOUYI_TIMEOUT_SEC, HOUYI_USE_TLS, HOUYI_API_KEY, TASK_LABEL
EOF
}

if [[ -z "${VDB_ROOT:-}" ]]; then
  echo "ERROR: VDB_ROOT is required." >&2
  usage
  exit 2
fi
if [[ ! -d "${VDB_ROOT}" ]]; then
  echo "ERROR: VDB_ROOT is not a directory: ${VDB_ROOT}" >&2
  exit 2
fi
VDB_ROOT="$(cd "${VDB_ROOT}" && pwd)"
export VDB_ROOT

# 中文：以下默认值与 VDBBENCH_PROTOCOL.md 保持一致。
# EN: Defaults below match VDBBENCH_PROTOCOL.md.
HOUYI_HOST="${HOUYI_HOST:-127.0.0.1}"
HOUYI_PG_PORT="${HOUYI_PG_PORT:-15432}"
HOUYI_HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
VDB_HOUYI_TRANSPORT="${VDB_HOUYI_TRANSPORT:-pgwire}"
VDB_CASE="${VDB_CASE:-Performance1536D50K}"
VDB_K="${VDB_K:-10}"
VDB_M="${VDB_M:-24}"
VDB_EFC="${VDB_EFC:-400}"
VDB_EFS="${VDB_EFS:-256}"
VDB_NUM_CONCURRENCY="${VDB_NUM_CONCURRENCY:-1}"
VDB_CONCURRENCY_DURATION="${VDB_CONCURRENCY_DURATION:-30}"
VDB_LOAD_CONCURRENCY="${VDB_LOAD_CONCURRENCY:-0}"
VDB_METRIC="${VDB_METRIC:-COSINE}"
HOUYI_TIMEOUT_SEC="${HOUYI_TIMEOUT_SEC:-600}"
HOUYI_USE_TLS="${HOUYI_USE_TLS:-0}"
TASK_LABEL="${TASK_LABEL:-houyidb-$(date +%Y%m%d-%H%M%S)}"
export HOUYI_API_KEY="${HOUYI_API_KEY:-}"

case "${VDB_HOUYI_TRANSPORT}" in
  pgwire|http) ;;
  *)
    echo "ERROR: VDB_HOUYI_TRANSPORT must be 'pgwire' or 'http', got '${VDB_HOUYI_TRANSPORT}'." >&2
    exit 2
    ;;
esac
case "${VDB_METRIC}" in
  L2|COSINE|IP) ;;
  *)
    echo "ERROR: VDB_METRIC must be L2, COSINE or IP, got '${VDB_METRIC}'." >&2
    exit 2
    ;;
esac

ADAPTER_DIR="${VDB_ROOT}/vectordb_bench/backend/clients/houyidb"
if [[ ! -d "${ADAPTER_DIR}" ]]; then
  echo "ERROR: HouyiDB adapter not found at ${ADAPTER_DIR}" >&2
  echo "Run: python3 install.py --vdb-root ${VDB_ROOT}" >&2
  exit 1
fi

# 中文：优先复用已激活的环境；否则加载 VDB 自带的 venv。
# EN: Reuse an active environment; otherwise activate the VDB virtualenv.
if [[ -z "${VIRTUAL_ENV:-}" && -f "${VDB_ROOT}/.venv/bin/activate" ]]; then
  # shellcheck source=/dev/null
  source "${VDB_ROOT}/.venv/bin/activate"
  echo "==> activated ${VDB_ROOT}/.venv"
elif [[ -z "${VIRTUAL_ENV:-}" ]]; then
  echo "WARN: no active virtualenv and ${VDB_ROOT}/.venv not found; using system Python." >&2
fi

if [[ -n "${VIRTUAL_ENV:-}" && -x "${VIRTUAL_ENV}/bin/python" ]]; then
  PYTHON_BIN="${VIRTUAL_ENV}/bin/python"
elif [[ -x "${VDB_ROOT}/.venv/bin/python" ]]; then
  PYTHON_BIN="${VDB_ROOT}/.venv/bin/python"
else
  PYTHON_BIN="$(command -v python3 || true)"
fi
if [[ -z "${PYTHON_BIN}" ]]; then
  echo "ERROR: python3 not found." >&2
  exit 1
fi

if command -v vectordb_bench >/dev/null 2>&1; then
  VDB_CMD=(vectordb_bench)
elif "${PYTHON_BIN}" -c "import vectordb_bench" >/dev/null 2>&1; then
  VDB_CMD=("${PYTHON_BIN}" -m vectordb_bench.cli.vectordbbench)
else
  echo "ERROR: vectordb_bench is not installed in the selected Python environment." >&2
  echo "Install VectorDBBench first, then activate its virtualenv." >&2
  exit 1
fi

# 中文：本地访问绕过系统代理，避免 health 检查被代理劫持。
# EN: Bypass system proxies for local endpoints so the health check is reliable.
export NO_PROXY="${NO_PROXY:-127.0.0.1,localhost}"
export no_proxy="${no_proxy:-127.0.0.1,localhost}"

SCHEME="http"
TLS_OPT=(--no-use-tls)
if [[ "${HOUYI_USE_TLS}" == "1" ]]; then
  SCHEME="https"
  TLS_OPT=(--use-tls)
fi

URL_HOST="${HOUYI_HOST}"
if [[ "${URL_HOST}" == *:* && "${URL_HOST}" != \[* ]]; then
  URL_HOST="[${URL_HOST}]"
fi
HEALTH_URL="${SCHEME}://${URL_HOST}:${HOUYI_HTTP_PORT}/v1/health"

echo "==> checking HouyiDB health: ${HEALTH_URL}"
if ! "${PYTHON_BIN}" - "${HEALTH_URL}" <<'PY'
import sys
import urllib.request

url = sys.argv[1]
try:
    with urllib.request.urlopen(url, timeout=5) as response:
        if not 200 <= response.status < 400:
            raise RuntimeError(f"HTTP {response.status}")
except Exception as exc:  # noqa: BLE001
    print(f"health check failed: {exc}", file=sys.stderr)
    raise SystemExit(1)
PY
then
  echo "ERROR: HouyiDB HTTP endpoint is not reachable: ${HEALTH_URL}" >&2
  echo "Check --benchmark-http-addr, --http-port and NO_PROXY." >&2
  exit 1
fi

cd "${VDB_ROOT}"

ARGS=(
  houyidb
  --host "${HOUYI_HOST}"
  --port "${HOUYI_PG_PORT}"
  --http-port "${HOUYI_HTTP_PORT}"
  --transport "${VDB_HOUYI_TRANSPORT}"
  --case-type "${VDB_CASE}"
  --k "${VDB_K}"
  --m "${VDB_M}"
  --ef-construction "${VDB_EFC}"
  --ef-search "${VDB_EFS}"
  --metric-type "${VDB_METRIC}"
  --load-concurrency "${VDB_LOAD_CONCURRENCY}"
  --num-concurrency "${VDB_NUM_CONCURRENCY}"
  --concurrency-duration "${VDB_CONCURRENCY_DURATION}"
  --timeout-sec "${HOUYI_TIMEOUT_SEC}"
  --task-label "${TASK_LABEL}"
  "${TLS_OPT[@]}"
)

echo "==> HouyiDB benchmark"
echo "    transport=${VDB_HOUYI_TRANSPORT} pg=${HOUYI_HOST}:${HOUYI_PG_PORT} http=${HOUYI_HOST}:${HOUYI_HTTP_PORT}"
echo "    case=${VDB_CASE} k=${VDB_K} m=${VDB_M} efc=${VDB_EFC} efs=${VDB_EFS}"
echo "    concurrency=${VDB_NUM_CONCURRENCY} duration=${VDB_CONCURRENCY_DURATION}s load_concurrency=${VDB_LOAD_CONCURRENCY}"
echo "    task_label=${TASK_LABEL}"
printf '    command:'
printf ' %q' "${VDB_CMD[@]}" "${ARGS[@]}" "$@"
printf '\n'

"${VDB_CMD[@]}" "${ARGS[@]}" "$@"

RESULT_DIR="${VDB_ROOT}/vectordb_bench/results/HouyiDB"
shopt -s nullglob
RESULT_FILES=("${RESULT_DIR}"/result_*.json)
shopt -u nullglob
if (( ${#RESULT_FILES[@]} > 0 )); then
  LATEST_RESULT="${RESULT_FILES[0]}"
  for result_file in "${RESULT_FILES[@]}"; do
    if [[ "${result_file}" -nt "${LATEST_RESULT}" ]]; then
      LATEST_RESULT="${result_file}"
    fi
  done
  echo "==> latest result: ${LATEST_RESULT}"
else
  echo "==> no result_*.json found under ${RESULT_DIR}" >&2
fi
