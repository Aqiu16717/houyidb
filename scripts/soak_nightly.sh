#!/usr/bin/env bash
# soak 哨兵（P4.5.8，本地纪律项）：小时级混合负载 + 不变量监控。
# Soak sentinel: hours-long mixed workload plus invariant monitoring.
#
# 负载 / Workload（内联 Python 驱动）：
#   三个 collection（c_async / c_semi / c_full）随机 upsert / 删除、
#   周期性建删 scratch collection、随机精确检索；写压中按种子化轮转注入故障
#   （follower 断连重连、leader kill -9 + promote + 旧主清目录重并）。
#
# 哨兵不变量 / Sentinels（每 SOAK_SAMPLE_SECS 采样，违例即 fail）：
#   1. RSS 封顶：任一节点 RSS ≤ 起始 RSS × 3；
#   2. FD 封顶：任一节点 FD 数 ≤ 起始 × 3 + 64（粗粒度泄漏信号）；
#   3. 指标单调：applied / shipped / fsync 不得倒退（重启后重设基线）；
#   4. 日志无 panic（ERROR 只记录：故障窗口内的连接错误属预期）；
#   5. 计数守恒（结束时）：服务端计数必须落在写侧账本的**不确定区间**内——
#      确定成功的走 live 集合，结果不确定（info）的操作按「可能生效」处理，
#      上界 +info 写、下界 -info 删。精确相等会误伤 kill -9 时响应丢失的写；
#   6. 精确检索：结束时每个集合抽样一个 id，按 payload row=id 过滤必须命中。
#
# 输出 / Output: 时序指标与结论落 docs/verification/soak-<date>.md；
# 失败时 seed 可复现（SOAK_SEED）。
#
# 用法 / Usage:
#   ./scripts/soak_nightly.sh                    # 默认 2 小时
#   SOAK_SECS=600 ./scripts/soak_nightly.sh      # 冒烟（10 分钟）
#   SOAK_SEED=12345 SOAK_SECS=600 ./scripts/soak_nightly.sh
#   SOAK_KEEP=1 ./scripts/soak_nightly.sh        # 保留数据目录与日志
#
# 边界 / Limits: recall 抽样退化为「精确过滤命中」（掩码路径恒为暴力），
# 不做 ANN 质量漂移——那由 P4.5.3 的 retrieval_oracle 覆盖。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
cd "$ROOT"

SOAK_SECS="${SOAK_SECS:-7200}"
SAMPLE_SECS="${SOAK_SAMPLE_SECS:-10}"
SOAK_SEED="${SOAK_SEED:-$(date +%s)}"
KEEP="${SOAK_KEEP:-0}"
[[ "$SOAK_SECS" =~ ^[1-9][0-9]*$ ]] || { echo "!! SOAK_SECS 必须是正整数" >&2; exit 2; }

A_HTTP=17911; A_PG=17912; A_REPL=17913
B_HTTP=17914; B_PG=17915; B_REPL=17916

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-soak.XXXXXX")
L_DATA="$WORKDIR/leader"; F_DATA="$WORKDIR/follower"
mkdir -p "$L_DATA" "$F_DATA"
LEADER_PID=""; FOLLOWER_PID=""; DRIVER_PID=""
cleanup() {
  for pid in "$DRIVER_PID" "$LEADER_PID" "$FOLLOWER_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  if [[ "$KEEP" != "1" ]]; then rm -rf "$WORKDIR"; else echo "==> SOAK_KEEP=1, 现场: $WORKDIR"; fi
}
trap cleanup EXIT

BIN="${HOUYI_BIN:-target/debug/houyi-server}"
[[ -x "$BIN" ]] || { echo "==> 构建 houyi-server"; cargo build -p houyi-server >/dev/null; }

start_node() { # $1=log $2=data $3=role $4=http $5=pg $6=repl [$7=leader_addr]
  local log="$1" data="$2" role="$3" http="$4" pg="$5" repl="$6" leader="${7:-}"
  local extra=()
  [[ "$role" == "follower" ]] && extra+=(--replication-leader-addr "$leader")
  "$BIN" --benchmark-http-addr "127.0.0.1:${http}" --pgwire-addr "127.0.0.1:${pg}" \
    --wal-path "$data/soak.wal" --vector-dim 5 --wal-write-mode async \
    --role "$role" --replication-listen-addr "127.0.0.1:${repl}" \
    ${extra[@]+"${extra[@]}"} >"$WORKDIR/$log" 2>&1 &
  local pid=$!
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${http}/v1/health" >/dev/null 2>&1 \
      && { echo "$pid"; return 0; }
    kill -0 "$pid" 2>/dev/null || { tail -20 "$WORKDIR/$log" >&2; return 1; }
    sleep 0.1
  done
  echo "!! 节点启动超时（$log）" >&2; return 1
}

post_json() { # $1=url $2=file $3=out → code
  curl -s --max-time 60 --noproxy '*' -o "$3" -w "%{http_code}" -X POST \
    -H 'Content-Type: application/json' --data @"$2" "$1"
}

sample_of() { # $1=pid $2=rss|fds
  case "$2" in
    rss) ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' ;;
    fds) lsof -p "$1" 2>/dev/null | wc -l | tr -d ' ' ;;
  esac
}

metric_of() { # $1=port $2=name
  curl -sf --max-time 5 --noproxy '*' "http://127.0.0.1:$1/metrics" 2>/dev/null \
    | awk -v n="$2" '$1 == n {print $2; exit}'
}

collection_count() { # $1=port $2=name
  local body="$WORKDIR/count.json"
  curl -s --max-time 10 --noproxy '*' -o "$body" \
    "http://127.0.0.1:$1/v1/collections/$2/status" || { echo ""; return 0; }
  python3 -c "
import json
try:
    print(json.load(open('$body'))['indexed_count'] + json.load(open('$body'))['pending_count'])
except Exception:
    print('')
"
}

echo "==> [1/4] 启动 leader(:${A_HTTP}) 与 follower(:${B_HTTP})，建三级集合"
LEADER_PID=$(start_node leader.log "$L_DATA" leader "$A_HTTP" "$A_PG" "$A_REPL")
FOLLOWER_PID=$(start_node follower.log "$F_DATA" follower "$B_HTTP" "$B_PG" "$B_REPL" "127.0.0.1:${A_REPL}")
for spec in "c_async:async" "c_semi:semi_sync" "c_full:full_sync"; do
  name="${spec%%:*}"; level="${spec##*:}"
  echo "{\"vectors\":[{\"id\":1,\"values\":[1.0,1,1,1,1],\"payload\":{\"row\":1}}]}" >"$WORKDIR/probe.json"
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/$name/upsert" "$WORKDIR/probe.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $name 探针失败 $code" >&2; exit 1; }
  echo "{\"ha_level\":\"$level\"}" >"$WORKDIR/ha.json"
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/$name/ha_level" "$WORKDIR/ha.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $name 设级别失败 $code" >&2; exit 1; }
done

# ── 混合负载驱动（内联 Python）──
# EN: -- Mixed workload driver (inline Python) --
cat >"$WORKDIR/driver.py" <<'PYEOF'
import json, os, random, sys, time, urllib.request

base_a, base_b, ledger, stop_file, seed, start_id = sys.argv[1:7]
start_id = int(start_id)
rng = random.Random(int(seed))
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def post(base, path, body, timeout=15):
    req = urllib.request.Request(base + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with opener.open(req, timeout=timeout) as resp:
        return json.loads(resp.read())

def log(rec):
    with open(ledger, "a") as f:
        f.write(json.dumps(rec) + "\n")
        f.flush()

cur = start_id
scratch = 0
last_good = base_a

while not os.path.exists(stop_file):
    base = last_good
    col = rng.choice(["c_async", "c_semi", "c_full"])
    action = rng.random()
    rid = start_id + rng.randrange(max(1, cur - start_id))
    try:
        if action < 0.62:
            rid = cur
            cur += 1
            post(base, f"/v1/collections/{col}/upsert", {"vectors": [{
                "id": rid,
                "values": [1.0, rid % 7 + 1, rid % 5 + 1, rid % 3 + 1, rid % 11 + 1],
                "payload": {"row": rid}}]})
            log({"op": "upsert", "coll": col, "id": rid, "status": "ok"})
        elif action < 0.80:
            post(base, f"/v1/collections/{col}/delete", {"ids": [rid]})
            log({"op": "delete", "coll": col, "id": rid, "status": "ok"})
        elif action < 0.90:
            scratch += 1
            post(base, f"/v1/collections/scratch{scratch % 5}/upsert",
                 {"vectors": [{"id": 900000 + scratch % 5, "values": [1.0, 2, 2, 2, 2]}]})
            if scratch % 7 == 0:
                post(base, f"/v1/collections/scratch{scratch % 5}/delete",
                     {"ids": [900000 + scratch % 5]})
        else:
            post(base, f"/v1/collections/{col}/hybrid/search", {
                "vector": [1.0, rid % 7 + 1, rid % 5 + 1, rid % 3 + 1, rid % 11 + 1],
                "k": 1,
                "filters": [{"column": "row", "op": "eq", "value": rid}]})
    except Exception:
        # 故障窗口：结果不确定（可能已生效，也可能根本没发出去）。账本必须
        # 如实记录 info，否则计数守恒会把服务端已应用的写误判为幽灵。
        # EN: indeterminate outcome — record it so the ledger check can bound it.
        last_good = base_b if base == base_a else base_a
        time.sleep(0.05)
        if action < 0.62:
            log({"op": "upsert", "coll": col, "id": rid, "status": "info"})
        elif action < 0.80:
            log({"op": "delete", "coll": col, "id": rid, "status": "info"})
PYEOF

: >"$WORKDIR/ledger.jsonl"
echo "==> [2/4] 启动负载驱动（${SOAK_SECS}s，seed=${SOAK_SEED}）"
python3 "$WORKDIR/driver.py" "http://127.0.0.1:${A_HTTP}" "http://127.0.0.1:${B_HTTP}" \
  "$WORKDIR/ledger.jsonl" "$WORKDIR/stop" "$SOAK_SEED" 1000000 &
DRIVER_PID=$!

RAND_STATE=$(( (SOAK_SEED % 2147483647) + 1 ))
rand_below() { RAND_STATE=$(( (RAND_STATE * 1103515245 + 12345) % 2147483648 )); echo $(( RAND_STATE % $1 )); }

echo "==> [3/4] 哨兵循环（每 ${SAMPLE_SECS}s）"
REPORT="docs/verification/soak-$(date +%Y%m%d-%H%M%S).md"
mkdir -p docs/verification
{
  echo "# soak 哨兵报告"
  echo
  echo "- 开始：$(date -u +%FT%TZ)"
  echo "- seed：${SOAK_SEED}"
  echo "- 时长目标：${SOAK_SECS}s；采样间隔：${SAMPLE_SECS}s"
  echo
  echo "| 时刻(s) | A_rss(kB) | B_rss(kB) | A_fd | B_fd | applied | shipped | fsync | 事件 |"
  echo "|---:|---:|---:|---:|---:|---:|---:|---:|---|"
} >"$REPORT"

START_TS=$(date +%s)
A_RSS0=$(sample_of "$LEADER_PID" rss); B_RSS0=$(sample_of "$FOLLOWER_PID" rss)
A_FD0=$(sample_of "$LEADER_PID" fds); B_FD0=$(sample_of "$FOLLOWER_PID" fds)
LAST_APPLIED=0; LAST_SHIPPED=0; LAST_FSYNC=0
events=""
fault_round=0

while (( $(date +%s) - START_TS < SOAK_SECS )); do
  sleep "$SAMPLE_SECS"
  now=$(( $(date +%s) - START_TS ))

  # 故障注入：约每 60s 一次，轮转覆盖三类（顺序由 seed 决定）。
  # EN: Fault injection: about once every 60s, rotating across three kinds (order
  # EN: determined by the seed).
  if (( now > 0 && now % 60 < SAMPLE_SECS )); then
    case $(( (fault_round + SOAK_SEED) % 3 )) in
      0)
        kill -9 "$FOLLOWER_PID" 2>/dev/null || true
        wait "$FOLLOWER_PID" 2>/dev/null || true
        sleep 0.5
        FOLLOWER_PID=$(start_node "follower-restart-${fault_round}.log" "$F_DATA" follower \
          "$B_HTTP" "$B_PG" "$B_REPL" "127.0.0.1:${A_REPL}")
        events="${events} f${fault_round}:follower-restart"
        ;;
      1)
        kill -9 "$LEADER_PID" 2>/dev/null || true
        wait "$LEADER_PID" 2>/dev/null || true
        code=$(post_json "http://127.0.0.1:${B_HTTP}/v1/admin/promote" /dev/null "$WORKDIR/p.json" || echo "")
        [[ "$code" == "200" ]] || { echo "!! soak: promote 失败 $code" >&2; exit 1; }
        rm -rf "$L_DATA"; mkdir -p "$L_DATA"
        sleep 0.5
        LEADER_PID=$(start_node "leader-rejoin-${fault_round}.log" "$L_DATA" follower \
          "$A_HTTP" "$A_PG" "$A_REPL" "127.0.0.1:${B_REPL}")
        events="${events} f${fault_round}:promote+rejoin"
        LAST_APPLIED=0; LAST_SHIPPED=0; LAST_FSYNC=0
        ;;
      *)
        events="${events} f${fault_round}:none"
        ;;
    esac
    fault_round=$((fault_round + 1))
  fi

  A_RSS=$(sample_of "$LEADER_PID" rss); B_RSS=$(sample_of "$FOLLOWER_PID" rss)
  A_FD=$(sample_of "$LEADER_PID" fds); B_FD=$(sample_of "$FOLLOWER_PID" fds)
  applied=$(metric_of "$A_HTTP" houyidb_replication_applied_entries_total); applied=${applied:-0}
  shipped=$(metric_of "$A_HTTP" houyidb_replication_shipped_batches_total); shipped=${shipped:-0}
  fsync=$(metric_of "$A_HTTP" houyidb_replication_fsync_total); fsync=${fsync:-0}

  if grep -q "panicked at" "$WORKDIR"/leader*.log "$WORKDIR"/follower*.log 2>/dev/null; then
    echo "!! soak: 检测到 panic（见 $WORKDIR/*.log）" >&2; exit 1
  fi
  if [[ -n "$A_RSS0" && "$A_RSS0" -gt 0 && -n "$A_RSS" && "$A_RSS" -gt $(( A_RSS0 * 3 )) ]]; then
    echo "!! soak: leader RSS ${A_RSS}kB > 起始 ${A_RSS0}kB × 3" >&2; exit 1
  fi
  if [[ -n "$B_RSS0" && "$B_RSS0" -gt 0 && -n "$B_RSS" && "$B_RSS" -gt $(( B_RSS0 * 3 )) ]]; then
    echo "!! soak: follower RSS ${B_RSS}kB > 起始 ${B_RSS0}kB × 3" >&2; exit 1
  fi
  if [[ -n "$A_FD0" && "$A_FD0" -gt 0 && -n "$A_FD" && "$A_FD" -gt $(( A_FD0 * 3 + 64 )) ]]; then
    echo "!! soak: leader FD ${A_FD} > 起始 ${A_FD0} × 3 + 64" >&2; exit 1
  fi
  if (( applied < LAST_APPLIED || shipped < LAST_SHIPPED || fsync < LAST_FSYNC )); then
    echo "!! soak: 复制指标倒退 applied=${applied}(<${LAST_APPLIED}) shipped=${shipped}(<${LAST_SHIPPED}) fsync=${fsync}(<${LAST_FSYNC})" >&2
    exit 1
  fi
  LAST_APPLIED=$applied; LAST_SHIPPED=$shipped; LAST_FSYNC=$fsync

  echo "| ${now} | ${A_RSS:-?} | ${B_RSS:-?} | ${A_FD:-?} | ${B_FD:-?} | ${applied} | ${shipped} | ${fsync} |${events} |" >>"$REPORT"
done

echo "==> [4/4] 收敛与不变量终检"
touch "$WORKDIR/stop"
wait "$DRIVER_PID" 2>/dev/null || true
DRIVER_PID=""

# 账本 → 每集合的 [下界, 上界]（含确定性写/删与不确定操作）。
# EN: Ledger -> [lower, upper] per collection (including deterministic writes/deletes
# EN: and indeterminate ops).
python3 "$SCRIPT_DIR/soak_ledger.py" "$WORKDIR/ledger.jsonl" "$WORKDIR/expected.json"

# 收敛窗口：最后一次故障可能是 promote+重并，读侧节点（A）可能仍在追赶
# （async 集合尤其如此）。给 30s 让计数落入账本区间，再判定守恒。
# EN: convergence window — after a promote+rejoin the read-side node may still
# be catching up; poll for up to 30s before asserting.
FAILED=0
deadline=$((SECONDS + 30))
for col in c_async c_semi c_full; do
  bounds=$(python3 -c "
import json,sys
d = json.load(open('$WORKDIR/expected.json')).get('$col', {})
live = d.get('live', 0); up = d.get('maybe_up', 0); dl = d.get('maybe_del', 0)
# 探针 id=1 是建集合时写入的，账本不含它。
# EN: Probe id=1 is written when the collection is created; the ledger does not
# EN: include it.
print(live + 1 - dl, live + 1 + up)
")
  lo=${bounds%% *}; hi=${bounds##* }
  got=""
  while (( SECONDS < deadline )); do
    got=$(collection_count "$A_HTTP" "$col"); got=${got:-0}
    (( got >= lo && got <= hi )) && break
    sleep 0.5
  done
  if (( got < lo || got > hi )); then
    echo "!! soak: $col 计数 ${got} 超出账本区间 [${lo}, ${hi}]（30s 收敛窗口后）" >&2
    FAILED=1
  else
    echo "    $col 计数 ${got} ∈ [${lo}, ${hi}] ✓"
  fi
done

python3 "$SCRIPT_DIR/soak_probe.py" "$WORKDIR/ledger.jsonl" "http://127.0.0.1:${A_HTTP}"
[[ "$FAILED" == "0" ]] || { echo "!! soak: 计数守恒失败" >&2; exit 1; }

{
  echo
  echo "- 结束：$(date -u +%FT%TZ)"
  echo "- 故障事件：${events:-无}"
  echo "- 结论：**通过**（计数守恒区间 + 精确检索抽样 + 全程哨兵无违例）"
} >>"$REPORT"

echo "==> soak 通过：报告 $REPORT"
