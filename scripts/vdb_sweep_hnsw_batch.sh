#!/usr/bin/env bash
# 扫描 HOUYI_HNSW_MAX_BUILD_BATCH，在固定 Case 下比较 optimize_duration（本机调参用）。
# Sweeps `HOUYI_HNSW_MAX_BUILD_BATCH` to compare `optimize_duration` (local tuning; same VDB case).
#
# 用法（在 houyidb 根目录，需已 build release + vectorDBench venv + 数据集）:
#   ./scripts/vdb_sweep_hnsw_batch.sh
#   HNSW_SWEEP_BATCHES="256 512 1024" ./scripts/vdb_sweep_hnsw_batch.sh
#   HNSW_SWEEP_SKIP_SERIAL=1 ./scripts/vdb_sweep_hnsw_batch.sh
#
# 环境：每次使用独立 WAL、干净起停 houyi-server；`HOUYI_HNSW_PARALLEL_BATCH_BUILD=1`（除全串行基线）。
# EN: fresh WAL per run; unless serial baseline, parallel batch build is on.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VDB_ROOT="${VDB_ROOT:-$(cd "${ROOT}/.." && pwd)/vectorDBBench-houyidb}"
PY="${VDB_ROOT}/.venv/bin/python3"
SRV="${ROOT}/target/release/houyi-server"

if [[ ! -x "$SRV" ]]; then
  echo "ERROR: build houyi-server release first: cargo build -p houyi-server --release" >&2
  exit 1
fi
if [[ ! -x "$PY" ]]; then
  echo "ERROR: missing $PY" >&2
  exit 1
fi

# 默认：全串行基线 + 多档 batch（可覆盖）
# Default: serial baseline + several batch sizes (override with HNSW_SWEEP_BATCHES).
BATCHES_STR="${HNSW_SWEEP_BATCHES:-512 1024 2048 4096 8192 16384}"
read -r -a BATCH_ARR <<< "${BATCHES_STR}"

HOUYI_HOST="${HOUYI_HOST:-127.0.0.1}"
HOUYI_PG_PORT="${HOUYI_PG_PORT:-15432}"
HOUYI_HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
SKIP_SERIAL="${HNSW_SWEEP_SKIP_SERIAL:-0}"

_free_ports() {
  for p in "${HOUYI_HTTP_PORT}" "${HOUYI_PG_PORT}"; do
    pids=$(lsof -t -iTCP:"${p}" -sTCP:LISTEN 2>/dev/null || true)
    if [[ -n "${pids:-}" ]]; then
      # 有意省略引号：pids 为空格分隔的本地列表
      # EN: Quotes intentionally omitted; pids is a local space-separated list.
      # shellcheck disable=SC2086
      kill -9 ${pids} 2>/dev/null || true
    fi
  done
  sleep 0.35
}

_run_server() {
  local wal_path="$1"
  local max_batch="$2"
  local parallel_on="$3"  # 1 or 0
  rm -f "${wal_path}" "${wal_path}.applied" 2>/dev/null || true
  # parallel_min_batch 不可大于本档 max_batch，否则本批 count 永远达不到阈值。
  # EN: parallel_min_batch must not exceed this tier's max_batch, otherwise the batch
  # EN: count can never reach the threshold.
  local pmin=512
  if (( max_batch < 512 )); then
    pmin="${max_batch}"
  fi
  (
    export NO_PROXY=127.0.0.1,localhost
    export no_proxy=127.0.0.1,localhost
    export HOUYI_HNSW_MAX_BUILD_BATCH="${max_batch}"
    export HOUYI_HNSW_PARALLEL_MIN_BATCH="${pmin}"
    if [[ "${parallel_on}" == "1" ]]; then
      export HOUYI_HNSW_PARALLEL_BATCH_BUILD=1
    else
      unset HOUYI_HNSW_PARALLEL_BATCH_BUILD
    fi
    cd "${ROOT}" && exec "${SRV}" \
      --benchmark-http-addr "127.0.0.1:${HOUYI_HTTP_PORT}" \
      --pgwire-addr "127.0.0.1:${HOUYI_PG_PORT}" \
      --pgwire-hnsw-m 24 \
      --pgwire-hnsw-efc 400 \
      --wal-path "${wal_path}"
  ) > "/tmp/vdb_sweep_srv_${SLOT}.log" 2>&1 &
  echo $!
}

_wait_health() {
  local pid="$1"
  local i
  for i in $(seq 1 120); do
    c=$(curl -sS -o /dev/null -w "%{http_code}" --max-time 2 --noproxy '*' \
      "http://127.0.0.1:${HOUYI_HTTP_PORT}/v1/health" 2>/dev/null || echo 0)
    if [[ "$c" == "200" ]]; then
      return 0
    fi
    if ! kill -0 "${pid}" 2>/dev/null; then
      echo "ERROR: server died (slot ${SLOT})" >&2
      tail -40 "/tmp/vdb_sweep_srv_${SLOT}.log" >&2
      return 1
    fi
    sleep 0.25
  done
  echo "ERROR: health timeout (slot ${SLOT})" >&2
  return 1
}

SLOT="$(date +%s)"
SUMMARY_CSV="/tmp/vdb_sweep_hnsw_batch_${SLOT}.csv"
echo "mode,batch,max_build_batch,parallel_on,insert_s,optimize_s,load_s,recall,ndcg,result_json" > "${SUMMARY_CSV}"

export NO_PROXY=127.0.0.1,localhost
export no_proxy=127.0.0.1,localhost
export VDB_ROOT

_run_case() {
  local mode_name="$1"
  local max_batch="$2"        # 0 = use label only (serial) — we pass string "serial" for batch in CSV
  local parallel_on="$3"

  _free_ports
  local wal
  wal="/tmp/vdb_sweep_hnsw_${SLOT}_${mode_name}.wal"
  SRV_PID=$(_run_server "${wal}" "${max_batch}" "${parallel_on}")
  if ! _wait_health "${SRV_PID}"; then
    return 1
  fi
  local label="sweep-${SLOT}-${mode_name}"
  if ! VDB_ROOT="${VDB_ROOT}" \
    NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
    TASK_LABEL_HOUYI="${label}" \
    "${ROOT}/scripts/vdbbench_qdrant_houyidb_gate.sh" run-houyidb; then
    kill -9 "${SRV_PID}" 2>/dev/null || true
    wait "${SRV_PID}" 2>/dev/null || true
    return 1
  fi
  kill -9 "${SRV_PID}" 2>/dev/null || true
  wait "${SRV_PID}" 2>/dev/null || true
  _free_ports

  local jpath
  jpath=$(ls -t "${VDB_ROOT}/vectordb_bench/results/HouyiDB/result_"*"${label}"*_houyidb.json 2>/dev/null | head -1)
  if [[ -z "$jpath" || ! -f "$jpath" ]]; then
    echo "ERROR: result json not found for label ${label} (glob *${label}*)" >&2
    return 1
  fi

  "${PY}" - "$jpath" "${mode_name}" "${max_batch}" "${parallel_on}" "${SUMMARY_CSV}" <<'PY'
import json, sys, csv
jpath, mode, mb, p_on, out_csv = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
d = json.load(open(jpath, encoding="utf-8"))
m = (d.get("results") or [{}])[0].get("metrics") or {}
ins = m.get("insert_duration")
opt = m.get("optimize_duration")
lod = m.get("load_duration")
rec = m.get("recall")
ndc = m.get("ndcg")
with open(out_csv, "a", newline="") as f:
    w = csv.writer(f)
    w.writerow([mode, mb, mb, p_on, ins, opt, lod, rec, ndc, jpath])
print(f"  -> insert={ins}s optimize={opt}s load={lod}s recall={rec} ndcg={ndc}")
print(f"  -> json {jpath}")
PY
}

echo "==> HNSW batch sweep, VDB_ROOT=${VDB_ROOT}, batches=${BATCHES_STR}, SLOT=${SLOT}"
echo "==> summary: ${SUMMARY_CSV}"
echo ""

if [[ "${SKIP_SERIAL}" != "1" ]]; then
  echo "=== [baseline] full serial (unset HOUYI_HNSW_PARALLEL_BATCH_BUILD), HOUYI_HNSW_MAX_BUILD_BATCH=512 ==="
  _run_case "serial-baseline" 512 0 || true
  echo ""
fi

for b in "${BATCH_ARR[@]}"; do
  echo "=== parallel build, HOUYI_HNSW_MAX_BUILD_BATCH=${b} HOUYI_HNSW_PARALLEL_BATCH_BUILD=1 ==="
  # 关闭 SC2034 未使用变量告警
  # EN: Disable the SC2034 unused-variable warning.
  # shellcheck disable=SC2034
  _run_case "p-batch-${b}" "${b}" 1 || true
  echo ""
done

echo "==> done. Table:"
column -s, -t "${SUMMARY_CSV}" 2>/dev/null || cat "${SUMMARY_CSV}"
echo "CSV: ${SUMMARY_CSV}"
