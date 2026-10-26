#!/usr/bin/env bash
# 端到端 VectorDBBench（HouyiDB）监控运行器：
# - 实时打印 optimize 进度（indexed/pending、估算百分比、瞬时速度）
# - 速度持续过低时提前中止，避免“10 分钟还看不到头”
#
# 用法：
#   ./scripts/vdbbench_houyidb_monitored.sh
# 可选环境变量：
#   HOUYI_HTTP_PORT=17000
#   VDB_TOTAL=50000
#   MONITOR_INTERVAL_SECS=5
#   MONITOR_GRACE_SECS=60
#   MONITOR_MIN_IPS=120
#   MONITOR_SLOW_STREAK_LIMIT=6
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

HOUYI_HOST="${HOUYI_HOST:-127.0.0.1}"
HOUYI_HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
VDB_TOTAL="${VDB_TOTAL:-50000}"
MONITOR_INTERVAL_SECS="${MONITOR_INTERVAL_SECS:-5}"
MONITOR_GRACE_SECS="${MONITOR_GRACE_SECS:-60}"
MONITOR_MIN_IPS="${MONITOR_MIN_IPS:-120}"
MONITOR_SLOW_STREAK_LIMIT="${MONITOR_SLOW_STREAK_LIMIT:-6}"

STATUS_URL="http://${HOUYI_HOST}:${HOUYI_HTTP_PORT}/v1/index/status"

echo "==> start vectordbbench (pgwire) with monitor"
echo "==> status url: ${STATUS_URL}"
echo "==> total=${VDB_TOTAL}, interval=${MONITOR_INTERVAL_SECS}s, min_ips=${MONITOR_MIN_IPS}, grace=${MONITOR_GRACE_SECS}s"

./scripts/vdbbench_qdrant_houyidb_gate.sh run-houyidb > /tmp/houyidb_vdbbench_run.log 2>&1 &
RUN_PID=$!

cleanup() {
  if kill -0 "${RUN_PID}" >/dev/null 2>&1; then
    kill "${RUN_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

last_indexed=0
last_ts=$(date +%s)
start_ts=${last_ts}
slow_streak=0

while kill -0 "${RUN_PID}" >/dev/null 2>&1; do
  now_ts=$(date +%s)
  elapsed=$((now_ts - start_ts))

  payload="$(curl -sS --max-time 2 --noproxy '*' "${STATUS_URL}" || true)"
  if [[ -z "${payload}" ]]; then
    echo "[monitor] status unavailable; elapsed=${elapsed}s"
    sleep "${MONITOR_INTERVAL_SECS}"
    continue
  fi

  parsed="$(python3 - "$payload" <<'PY'
import json,sys
try:
    d=json.loads(sys.argv[1])
    print(d.get("indexed_count",0), d.get("pending_count",0), d.get("flush_count",0))
except Exception:
    print("0 0 0")
PY
)"
  indexed="$(echo "${parsed}" | awk '{print $1}')"
  pending="$(echo "${parsed}" | awk '{print $2}')"
  flushes="$(echo "${parsed}" | awk '{print $3}')"

  dt=$((now_ts - last_ts))
  if [[ "${dt}" -le 0 ]]; then
    dt=1
  fi
  di=$((indexed - last_indexed))
  ips=$((di / dt))
  pct=$(( indexed * 100 / VDB_TOTAL ))
  if [[ "${pct}" -gt 100 ]]; then
    pct=100
  fi

  echo "[monitor] elapsed=${elapsed}s indexed=${indexed} pending=${pending} flushes=${flushes} progress=${pct}% speed=${ips}/s"

  if [[ "${elapsed}" -ge "${MONITOR_GRACE_SECS}" && "${pending}" -gt 0 ]]; then
    if [[ "${ips}" -lt "${MONITOR_MIN_IPS}" ]]; then
      slow_streak=$((slow_streak + 1))
    else
      slow_streak=0
    fi
    if [[ "${slow_streak}" -ge "${MONITOR_SLOW_STREAK_LIMIT}" ]]; then
      echo "[monitor] too slow for ${slow_streak} checks (<${MONITOR_MIN_IPS}/s), aborting run."
      kill "${RUN_PID}" >/dev/null 2>&1 || true
      wait "${RUN_PID}" || true
      echo "==> benchmark aborted by monitor"
      echo "==> log: /tmp/houyidb_vdbbench_run.log"
      exit 2
    fi
  fi

  last_indexed="${indexed}"
  last_ts="${now_ts}"
  sleep "${MONITOR_INTERVAL_SECS}"
done

wait "${RUN_PID}"
rc=$?
echo "==> benchmark finished, exit=${rc}"
echo "==> log: /tmp/houyidb_vdbbench_run.log"
if [[ -f /tmp/houyidb_vdbbench_run.log ]]; then
  tail -n 80 /tmp/houyidb_vdbbench_run.log
fi
exit "${rc}"

