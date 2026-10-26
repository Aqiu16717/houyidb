#!/usr/bin/env bash
# Qdrant vs HouyiDB 多场景 VDB 公平对照（2026-10 campaign）。
# 同一台机器、同一套 VDB 参数、轮流启动两个数据库（不同时占用内存）。
# EN: alternating Qdrant vs HouyiDB campaign; one DB at a time.
#
# 用法 / Usage:
#   ./scripts/vdbbench_campaign.sh                 # 全矩阵（约 3 小时）
#   SMOKE=1 ./scripts/vdbbench_campaign.sh         # OpenAI 50K 加载 + ef64 冒烟
#   PHASES=openai,cohere ./scripts/vdbbench_campaign.sh
#   PHASES=cohere-q,filter,streaming ...           # 断点续跑（跳过已完成侧）
#   PHASES=filter ./scripts/vdbbench_campaign.sh
#   PHASES=filter-h,stream-h ...                   # 只跑 HouyiDB 侧（另一侧已有结果）
#   RUN_TAG=l0x1 PHASES=cohere-h ./scripts/vdbbench_campaign.sh  # 结果标签带配置变体
#   KEEP=1 ...                                     # 保留临时数据与日志
#
# 脚本内部用 caffeinate 阻止休眠；压测期间请保持外接电源连接。
#
# 环境变量：
#   VDB_ROOT    默认 ../vectorDBBench-aetherdb
#   QDRANT_BIN  默认 ~/Downloads/qdrant
#   HOUYI_BIN   默认 target/release/houyi-server
#   VDB_CONCURRENCY 默认 1,8,16
#   RUN_TAG     追加到 task label（如 l0x1 / l0x2）
#   HOUYI_HNSW_LAYER0_MULT / HOUYI_HNSW_UPPER_MULT 透传给 houyi-server
#               （脚本默认 LAYER0_MULT=1，高 QPS；召回优先设 2。
#                 服务端代码默认为 2，以兼容旧图快照）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
VDB_ROOT="${VDB_ROOT:-$(cd "${ROOT}/.." && pwd)/vectorDBBench-aetherdb}"
QDRANT_BIN="${QDRANT_BIN:-$HOME/Downloads/qdrant}"
HOUYI_BIN="${HOUYI_BIN:-${ROOT}/target/release/houyi-server}"
PHASES="${PHASES:-openai,cohere,filter,streaming}"
SMOKE="${SMOKE:-0}"
CONC="${VDB_CONCURRENCY:-1,8,16}"
RUN_TAG="${RUN_TAG:-}"
# RUN_TAG 追加到 task label，用于区分同一矩阵的不同配置（如 l0x1/l0x2）。
# EN: appended to the run id so result labels carry the config variant.
RUN_ID="$(date +%Y%m%d-%H%M%S)${RUN_TAG:+-$RUN_TAG}"

[[ -x "$QDRANT_BIN" ]] || { echo "!! QDRANT_BIN 不可执行: $QDRANT_BIN" >&2; exit 1; }
[[ -x "$HOUYI_BIN" ]] || { echo "!! HOUYI_BIN 不可执行: $HOUYI_BIN" >&2; exit 1; }
[[ -d "$VDB_ROOT/.venv" ]] || { echo "!! VDB_ROOT/.venv 缺失: $VDB_ROOT" >&2; exit 1; }

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/houyidb-vdb-campaign.XXXXXX")"
RESULT_DIR="$WORKDIR/results"
mkdir -p "$RESULT_DIR"
QPID=""
HPID=""
CAFFEINATE_PID=""
# 压测期间禁止休眠/降频（曾因电源被切断+Idle Sleep 中断整轮测试）。
# EN: keep the machine awake for the entire campaign.
if command -v caffeinate >/dev/null 2>&1; then
  caffeinate -dimsu -w $$ &
  CAFFEINATE_PID=$!
fi

log() { echo "[$(date +%H:%M:%S)] $*"; }

stop_pid() { # $1=pid $2=name
  local pid="$1" name="$2"
  [[ -n "$pid" ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 300); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

stop_qdrant() { stop_pid "$QPID" qdrant; QPID=""; }
stop_houyi() { stop_pid "$HPID" houyi; HPID=""; }

cleanup() {
  stop_qdrant
  stop_houyi
  if [[ -n "$CAFFEINATE_PID" ]]; then
    kill "$CAFFEINATE_PID" 2>/dev/null || true
  fi
  if [[ "${KEEP:-0}" == "1" ]]; then
    echo "==> KEEP=1, 现场保留: $WORKDIR"
  else
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

has_phase() { [[ ",$PHASES," == *",$1,"* ]]; }

wait_url() { # $1=url $2=seconds
  local url="$1" deadline=$((SECONDS + ${2:-60}))
  while (( SECONDS < deadline )); do
    if curl -sf --max-time 2 --noproxy '*' "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

ensure_ports_free() {
  local busy=0
  for p in 6333 6334 17000 15432; do
    if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
      echo "!! 端口 $p 被占用：" >&2
      lsof -nP -iTCP:"$p" -sTCP:LISTEN >&2
      busy=1
    fi
  done
  (( busy == 0 )) || exit 1
}

start_qdrant() {
  stop_qdrant
  rm -rf "$WORKDIR/qdrant"
  mkdir -p "$WORKDIR/qdrant"
  QDRANT__STORAGE__STORAGE_PATH="$WORKDIR/qdrant/storage" \
  QDRANT__SERVICE__HTTP_PORT=6333 \
  QDRANT__SERVICE__GRPC_PORT=6334 \
  QDRANT__TELEMETRY_DISABLED=true \
  QDRANT__LOG_LEVEL=WARN \
    "$QDRANT_BIN" >"$WORKDIR/qdrant.log" 2>&1 &
  QPID=$!
  if ! wait_url "http://127.0.0.1:6333/collections" 90; then
    echo "!! Qdrant 启动失败" >&2
    tail -30 "$WORKDIR/qdrant.log" >&2
    exit 1
  fi
  log "Qdrant ready (storage=$WORKDIR/qdrant/storage)"
}

start_houyi() { # $1=dims
  local dims="$1"
  stop_houyi
  rm -rf "$WORKDIR/houyi"
  mkdir -p "$WORKDIR/houyi"
  HOUYI_HNSW_LAYER0_MULT="${HOUYI_HNSW_LAYER0_MULT:-1}" \
  HOUYI_HNSW_PARALLEL_BATCH_BUILD=1 \
  HOUYI_HNSW_MAX_BUILD_BATCH=8192 \
  HOUYI_HNSW_SEED_SERIAL_POINTS=256 \
  HOUYI_SERVER_METRICS_INTERVAL_SECS=120 \
  HOUYI_HNSW_OPTIMIZE_LOG_FILE="$WORKDIR/houyi-optimize.log" \
    "$HOUYI_BIN" \
      --benchmark-http-addr 127.0.0.1:17000 \
      --pgwire-addr 127.0.0.1:15432 \
      --wal-path "$WORKDIR/houyi/bench.wal" \
      --vector-dim "$dims" \
      --wal-write-mode async \
      --snapshot-interval-secs 0 \
      --snapshot-wal-threshold-bytes 0 \
      >"$WORKDIR/houyi.log" 2>&1 &
  HPID=$!
  if ! wait_url "http://127.0.0.1:17000/v1/health" 90; then
    echo "!! HouyiDB 启动失败" >&2
    tail -40 "$WORKDIR/houyi.log" >&2
    exit 1
  fi
  log "HouyiDB ready (dims=$dims, wal=$WORKDIR/houyi/bench.wal)"
}

run_qdrant() {
  ( cd "$VDB_ROOT"; source .venv/bin/activate
    export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
    python -m vectordb_bench.cli.vectordbbench qdrantlocal \
      --url http://127.0.0.1:6333 "$@" ) >>"$WORKDIR/vdb.log" 2>&1
}

run_houyi() {
  ( cd "$VDB_ROOT"; source .venv/bin/activate
    export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
    python -m vectordb_bench.cli.vectordbbench aetherdb \
      --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
      --timeout-sec 1200 "$@" ) >>"$WORKDIR/vdb.log" 2>&1
}

archive_result() { # $1=QdrantLocal|AetherDB $2=label
  local db="$1" label="$2" f
  f=$(ls -t "$VDB_ROOT/vectordb_bench/results/$db"/result_*_"$label"_*.json 2>/dev/null | head -1 || true)
  if [[ -z "$f" ]]; then
    echo "!! 找不到结果 JSON: db=$db label=$label" >&2
    tail -40 "$WORKDIR/vdb.log" >&2
    exit 1
  fi
  cp "$f" "$RESULT_DIR/${db}_${label}.json"
  log "result $db/$label -> $f"
}

# 通用参数（两侧一致）。 / shared knobs.
# EN: Common parameters (identical for both sides).
Q_COMMON=(--k 10 --m 24 --ef-construct 400 --load-concurrency 8
  --num-concurrency "$CONC" --concurrency-duration 30 --on-disk false)
H_COMMON=(--k 10 --m 24 --ef-construction 400 --load-concurrency 8
  --num-concurrency "$CONC" --concurrency-duration 30)
Q_LOAD=(--hnsw-ef 256 --skip-search-serial --skip-search-concurrent)
H_LOAD=(--ef-search 256 --skip-search-serial --skip-search-concurrent)
Q_SEARCH_ONLY=(--skip-drop-old --skip-load)
H_SEARCH_ONLY=(--skip-drop-old --skip-load)

q_load() { local label="$1"; shift; log "Qdrant load $label"; run_qdrant "$@" "${Q_COMMON[@]}" "${Q_LOAD[@]}" --task-label "$label"; archive_result QdrantLocal "$label"; }
h_load() { local label="$1"; shift; log "HouyiDB load $label"; run_houyi "$@" "${H_COMMON[@]}" "${H_LOAD[@]}" --task-label "$label"; archive_result AetherDB "$label"; }
q_search() { local label="$1" ef="$2"; shift 2; log "Qdrant search $label ef=$ef"; run_qdrant "$@" "${Q_COMMON[@]}" --hnsw-ef "$ef" "${Q_SEARCH_ONLY[@]}" --task-label "$label"; archive_result QdrantLocal "$label"; }
h_search() { local label="$1" ef="$2"; shift 2; log "HouyiDB search $label ef=$ef"; run_houyi "$@" "${H_COMMON[@]}" --ef-search "$ef" "${H_SEARCH_ONLY[@]}" --task-label "$label"; archive_result AetherDB "$label"; }

openai_h_phase() {
  local ef_list=(64 128 256)
  [[ "$SMOKE" == "1" ]] && ef_list=(64)
  log "===== OpenAI 1536D / 50K — HouyiDB ====="
  ensure_ports_free
  start_houyi 1536
  h_load "$RUN_ID-openai-h-load" --case-type Performance1536D50K
  for ef in "${ef_list[@]}"; do
    h_search "$RUN_ID-openai-h-ef${ef}" "$ef" --case-type Performance1536D50K
  done
  stop_houyi
}

openai_q_phase() {
  local ef_list=(64 128 256)
  [[ "$SMOKE" == "1" ]] && ef_list=(64)
  log "===== OpenAI 1536D / 50K — Qdrant ====="
  ensure_ports_free
  start_qdrant
  q_load "$RUN_ID-openai-q-load" --case-type Performance1536D50K
  for ef in "${ef_list[@]}"; do
    q_search "$RUN_ID-openai-q-ef${ef}" "$ef" --case-type Performance1536D50K
  done
  stop_qdrant
}

# 顺序 Q → H；cohere 相反，平衡热漂移。
# EN: Order Q -> H; reversed for cohere to balance thermal drift.
openai_phase() { openai_q_phase; openai_h_phase; }

cohere_h_phase() {
  local ef_list=(128 256)
  log "===== Cohere 768D / 1M — HouyiDB ====="
  ensure_ports_free
  start_houyi 768
  h_load "$RUN_ID-cohere-h-load" --case-type Performance768D1M
  for ef in "${ef_list[@]}"; do
    h_search "$RUN_ID-cohere-h-ef${ef}" "$ef" --case-type Performance768D1M
  done
  stop_houyi
}

cohere_q_phase() {
  local ef_list=(128 256)
  log "===== Cohere 768D / 1M — Qdrant ====="
  ensure_ports_free
  start_qdrant
  q_load "$RUN_ID-cohere-q-load" --case-type Performance768D1M
  for ef in "${ef_list[@]}"; do
    q_search "$RUN_ID-cohere-q-ef${ef}" "$ef" --case-type Performance768D1M
  done
  stop_qdrant
}

cohere_phase() { cohere_h_phase; cohere_q_phase; }

FILTER_ARGS=(--case-type LabelFilterPerformanceCase
  --dataset-with-size-type "Medium Cohere (768dim, 1M)" --label-percentage 0.01)

filter_q_phase() {
  local ef_list=(128 256)
  log "===== Label filter / Cohere 1M / 1% — Qdrant ====="
  ensure_ports_free
  start_qdrant
  q_load "$RUN_ID-filter-q-load" "${FILTER_ARGS[@]}"
  for ef in "${ef_list[@]}"; do
    q_search "$RUN_ID-filter-q-ef${ef}" "$ef" "${FILTER_ARGS[@]}"
  done
  stop_qdrant
}

filter_h_phase() {
  local ef_list=(128 256)
  log "===== Label filter / Cohere 1M / 1% — HouyiDB ====="
  ensure_ports_free
  start_houyi 768
  h_load "$RUN_ID-filter-h-load" "${FILTER_ARGS[@]}"
  for ef in "${ef_list[@]}"; do
    h_search "$RUN_ID-filter-h-ef${ef}" "$ef" "${FILTER_ARGS[@]}"
  done
  stop_houyi
}

# 顺序 Q → H，平衡热漂移。
# EN: Order Q -> H to balance thermal drift.
filter_phase() { filter_q_phase; filter_h_phase; }

streaming_h_phase() {
  log "===== Streaming / Cohere 100K — HouyiDB ====="
  ensure_ports_free
  start_houyi 768
  log "HouyiDB streaming"
  run_houyi --case-type StreamingPerformanceCase --m 24 --ef-construction 400 \
    --ef-search 128 --load-concurrency 8 --num-concurrency "$CONC" \
    --concurrency-duration 30 --task-label "$RUN_ID-stream-h"
  archive_result AetherDB "$RUN_ID-stream-h"
  stop_houyi
}

streaming_q_phase() {
  log "===== Streaming / Cohere 100K — Qdrant ====="
  ensure_ports_free
  start_qdrant
  log "Qdrant streaming"
  run_qdrant --case-type StreamingPerformanceCase --m 24 --ef-construct 400 \
    --hnsw-ef 128 --load-concurrency 8 --num-concurrency "$CONC" \
    --concurrency-duration 30 --on-disk false --task-label "$RUN_ID-stream-q"
  archive_result QdrantLocal "$RUN_ID-stream-q"
  stop_qdrant
}

# 顺序 H → Q。
# EN: Order H -> Q.
streaming_phase() { streaming_h_phase; streaming_q_phase; }

log "campaign=$RUN_ID phases=$PHASES smoke=$SMOKE workdir=$WORKDIR"
log "VDB_ROOT=$VDB_ROOT"
log "QDRANT_BIN=$QDRANT_BIN ($($QDRANT_BIN --version 2>/dev/null | tail -1))"
log "HOUYI_BIN=$HOUYI_BIN"

if has_phase openai; then openai_phase; fi
if has_phase openai-h; then openai_h_phase; fi
if has_phase openai-q; then openai_q_phase; fi
if [[ "$SMOKE" == "1" ]]; then
  log "SMOKE=1 完成；结果在 $RESULT_DIR"
  exit 0
fi
if has_phase cohere; then cohere_phase; fi
if has_phase cohere-h; then cohere_h_phase; fi
if has_phase cohere-q; then cohere_q_phase; fi
if has_phase filter; then filter_phase; fi
if has_phase filter-h; then filter_h_phase; fi
if has_phase filter-q; then filter_q_phase; fi
if has_phase streaming; then streaming_phase; fi
if has_phase stream-h; then streaming_h_phase; fi
if has_phase stream-q; then streaming_q_phase; fi

log "全部完成；结果目录 $RESULT_DIR"
