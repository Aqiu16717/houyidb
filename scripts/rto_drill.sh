#!/usr/bin/env bash
# RTO / MTTR 实测演练（P4.8 / R4）。三阶段：
#   1) 有计划重启（关停时写图快照）→ 冷启动到 /readyz 的时间 = RTO(snapshot)
#   2) kill -9（无快照）→ WAL 全量重放到 /readyz 的时间 = RTO(wal-replay)
#   3) 主备 → 手动 promote 的端到端时间 = failover MTTR
# EN: measures restart RTO (snapshot and WAL-only) and manual-promote MTTR.
#
# 用法 / Usage:
#   ./scripts/rto_drill.sh
#   RTO_VECTORS=20000 ./scripts/rto_drill.sh      # 更大数据集
#   HOUYI_BIN=target/release/houyi-server ./scripts/rto_drill.sh
#   RTO_KEEP=1 ./scripts/rto_drill.sh             # 保留现场
#
# 结果写入 docs/verification/rto-mttr-<date>-<hhmmss>.md（同时打印摘要）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

VECTORS="${RTO_VECTORS:-10000}"
DIMS=8
H1=17901; P1=17902; R1=17903
H2=17904; P2=17905; R2=17906

if [[ -n "${HOUYI_BIN:-}" ]]; then
  BIN="$HOUYI_BIN"
  [[ -x "$BIN" ]] || { echo "!! HOUYI_BIN 不可执行: $BIN" >&2; exit 1; }
else
  BIN="target/debug/houyi-server"
  if [[ ! -x "$BIN" ]]; then
    echo "==> 构建 houyi-server (debug)"
    cargo build -p houyi-server >/dev/null
  fi
fi

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-rto-drill.XXXXXX")
PIDS=()
cleanup() {
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in ${PIDS[@]+"${PIDS[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
  if [[ "${RTO_KEEP:-0}" == "1" ]]; then
    echo "==> RTO_KEEP=1, 保留现场: $WORKDIR"
  else
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

wait_ready() { # $1=base_url $2=timeout_s
  local base="$1" deadline=$((SECONDS + ${2:-60}))
  while (( SECONDS < deadline )); do
    if curl -sf --max-time 1 --noproxy '*' "$base/readyz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.05
  done
  return 1
}

start_server() { # $1=log $2=data $3=role $4=http $5=pg $6=repl [$7=leader_addr]
  local log="$1" data="$2" role="$3" http="$4" pg="$5" repl="$6" leader="${7:-}"
  mkdir -p "$data"
  local extra=()
  [[ "$role" == "follower" ]] && extra+=(--replication-leader-addr "$leader")
  "$BIN" \
    --benchmark-http-addr "127.0.0.1:${http}" \
    --pgwire-addr "127.0.0.1:${pg}" \
    --wal-path "$data/bench.wal" \
    --vector-dim "$DIMS" \
    --wal-write-mode async \
    --role "$role" \
    --replication-listen-addr "127.0.0.1:${repl}" \
    --snapshot-interval-secs 0 \
    --snapshot-wal-threshold-bytes 0 \
    ${extra[@]+"${extra[@]}"} \
    >"$WORKDIR/$log" 2>&1 &
  local pid=$!
  PIDS+=("$pid")
  echo "$pid"
}

gen_batch() { # $1=start_id $2=count → JSON on stdout
  python3 - "$1" "$2" "$DIMS" <<'PY'
import json, math, sys
start, n, dims = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
vectors = []
for i in range(start, start + n):
    vals = [0.0] * dims
    vals[i % dims] = 1.0
    vals[(i * 7 + 3) % dims] += 0.01 * ((i % 17) - 8)
    norm = math.sqrt(sum(v * v for v in vals))
    # 带 payload 让标量行数可校验（scalar_row_count == N）。
    # EN: Include payload so scalar row count can be verified (scalar_row_count == N).
    vectors.append({"id": i, "values": [v / norm for v in vals], "payload": {"k": i}})
print(json.dumps({"vectors": vectors}))
PY
}

upsert_n() { # $1=base_url $2=total
  local base="$1" total="$2" batch=500 off=0
  while (( off < total )); do
    local n=$(( total - off > batch ? batch : total - off ))
    gen_batch "$off" "$n" > "$WORKDIR/batch.json"
    local code
    code=$(curl -s --max-time 60 --noproxy '*' -o "$WORKDIR/upsert.out" -w '%{http_code}' \
      -X POST "$base/v1/collections/docs/upsert" \
      -H 'content-type: application/json' --data @"$WORKDIR/batch.json")
    [[ "$code" == "200" ]] || {
      echo "!! upsert 失败 HTTP $code: $(head -c 300 "$WORKDIR/upsert.out")" >&2
      return 1
    }
    off=$(( off + n ))
  done
}

collection_rows() { # $1=base_url
  curl -sf --max-time 10 --noproxy '*' "$1/v1/collections/docs/status" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("scalar_row_count", -1))'
}

stop_graceful() { # $1=pid
  kill -TERM "$1" 2>/dev/null || true
  local deadline=$((SECONDS + 30))
  while kill -0 "$1" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.05; done
  kill -0 "$1" 2>/dev/null && { echo "!! 进程未在 30s 内退出" >&2; return 1; }
  wait "$1" 2>/dev/null || true
  return 0
}

echo "==> 1/3 快照重启 RTO（${VECTORS} 向量）"
A_DATA="$WORKDIR/a"
A_PID=$(start_server "a1.log" "$A_DATA" disabled "$H1" "$P1" "$R1")
wait_ready "http://127.0.0.1:${H1}" 60 || { echo "!! 首启失败"; tail -20 "$WORKDIR/a1.log"; exit 1; }
upsert_n "http://127.0.0.1:${H1}" "$VECTORS"
stop_graceful "$A_PID"
if [[ ! -d "$A_DATA/snapshots" ]] || [[ -z "$(ls -A "$A_DATA/snapshots" 2>/dev/null)" ]]; then
  echo "!! 关停未生成图快照，快照 RTO 场景无效" >&2
  exit 1
fi
T0=$(now_ms)
A2_PID=$(start_server "a2.log" "$A_DATA" disabled "$H1" "$P1" "$R1")
wait_ready "http://127.0.0.1:${H1}" 120 || { echo "!! 快照重启失败"; tail -30 "$WORKDIR/a2.log"; exit 1; }
T1=$(now_ms)
RTO_SNAPSHOT=$(( T1 - T0 ))
ROWS_A=$(collection_rows "http://127.0.0.1:${H1}")
[[ "$ROWS_A" == "$VECTORS" ]] || echo "!! 快照重启后行数 ${ROWS_A} != ${VECTORS}" >&2
stop_graceful "$A2_PID"

echo "==> 2/3 WAL 全量重放 RTO（kill -9，${VECTORS} 向量）"
B_DATA="$WORKDIR/b"
B_PID=$(start_server "b1.log" "$B_DATA" disabled "$H2" "$P2" "$R2")
wait_ready "http://127.0.0.1:${H2}" 60 || { echo "!! 首启失败"; tail -20 "$WORKDIR/b1.log"; exit 1; }
upsert_n "http://127.0.0.1:${H2}" "$VECTORS"
kill -9 "$B_PID" 2>/dev/null || true
wait "$B_PID" 2>/dev/null || true
T2=$(now_ms)
B2_PID=$(start_server "b2.log" "$B_DATA" disabled "$H2" "$P2" "$R2")
wait_ready "http://127.0.0.1:${H2}" 120 || { echo "!! WAL 重放重启失败"; tail -30 "$WORKDIR/b2.log"; exit 1; }
T3=$(now_ms)
RTO_WAL=$(( T3 - T2 ))
ROWS_B=$(collection_rows "http://127.0.0.1:${H2}")
[[ "$ROWS_B" == "$VECTORS" ]] || echo "!! WAL 重放后行数 ${ROWS_B} != ${VECTORS}" >&2
stop_graceful "$B2_PID"

echo "==> 3/3 手动 promote MTTR"
L_DATA="$WORKDIR/leader"; F_DATA="$WORKDIR/follower"
L_PID=$(start_server "l.log" "$L_DATA" leader "$H1" "$P1" "$R1")
wait_ready "http://127.0.0.1:${H1}" 60 || { echo "!! leader 启动失败"; tail -20 "$WORKDIR/l.log"; exit 1; }
F_PID=$(start_server "f.log" "$F_DATA" follower "$H2" "$P2" "$R2" "127.0.0.1:${R1}")
wait_ready "http://127.0.0.1:${H2}" 60 || { echo "!! follower 启动失败"; tail -20 "$WORKDIR/f.log"; exit 1; }
upsert_n "http://127.0.0.1:${H1}" 100
# 等 follower 真正应用（指标 > 0），否则 promote 测的是空追赶。
# EN: Wait until the follower actually applies (metric > 0); otherwise promote
# EN: measures an empty catch-up.
deadline=$((SECONDS + 30))
while (( SECONDS < deadline )); do
  applied=$(curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${H2}/metrics" \
    | awk '/^houyidb_replication_applied_entries_total /{print $2; exit}')
  [[ -n "$applied" && "$applied" != "0" ]] && break
  sleep 0.05
done
T4=$(now_ms)
PROMOTE_CODE=$(curl -s --max-time 60 --noproxy '*' -o "$WORKDIR/promote.json" -w '%{http_code}' \
  -X POST "http://127.0.0.1:${H2}/v1/admin/promote")
T5=$(now_ms)
PROMOTE_MTTR=$(( T5 - T4 ))
[[ "$PROMOTE_CODE" == "200" ]] || {
  echo "!! promote 失败 HTTP $PROMOTE_CODE: $(cat "$WORKDIR/promote.json")" >&2
  tail -30 "$WORKDIR/f.log" >&2
  exit 1
}
grep -q '"promoted":true' "$WORKDIR/promote.json" || {
  echo "!! promote 响应异常: $(cat "$WORKDIR/promote.json")" >&2; exit 1;
}
# promote 后该节点必须能写（角色翻转生效）。
# EN: After promote the node must be able to write (role flip took effect).
gen_batch 900000 1 > "$WORKDIR/write.json"
WCODE=$(curl -s --max-time 30 --noproxy '*' -o "$WORKDIR/write.out" -w '%{http_code}' \
  -X POST "http://127.0.0.1:${H2}/v1/collections/docs/upsert" \
  -H 'content-type: application/json' --data @"$WORKDIR/write.json")
[[ "$WCODE" == "200" ]] || { echo "!! promote 后写入失败 HTTP $WCODE" >&2; exit 1; }
stop_graceful "$L_PID"
stop_graceful "$F_PID"

STAMP=$(date +%Y-%m-%d-%H%M%S)
OUT="docs/verification/rto-mttr-${STAMP}.md"
mkdir -p docs/verification
cat > "$OUT" <<EOF
# RTO / MTTR 实测（${STAMP}）

- 环境：$(uname -srm)
- 二进制：\`${BIN}\`
- 数据集：${VECTORS} 条 × ${DIMS} 维（单 collection \`docs\`）

| 场景 | 实测 | 说明 |
|------|------|------|
| 快照重启 RTO | **${RTO_SNAPSHOT} ms** | 优雅关停生成图快照，冷启动到 \`/readyz\` |
| WAL 全量重放 RTO | **${RTO_WAL} ms** | \`kill -9\` 后无快照，重放到 \`/readyz\` |
| 手动 promote MTTR | **${PROMOTE_MTTR} ms** | follower 已应用 100 条写后 POST \`/v1/admin/promote\` |

验证：两种重启后 \`scalar_row_count\` 分别为 ${ROWS_A} / ${ROWS_B}（期望 ${VECTORS}）；
promote 后新 leader 写入成功。

> 本文件由 \`scripts/rto_drill.sh\` 生成；目标值见 \`docs/SLO.md\`。
EOF

echo
echo "==> 结果（${OUT}）"
echo "    快照重启 RTO      : ${RTO_SNAPSHOT} ms"
echo "    WAL 重放 RTO      : ${RTO_WAL} ms"
echo "    手动 promote MTTR : ${PROMOTE_MTTR} ms"
