#!/usr/bin/env bash
# 崩溃一致性演练（P4.5.4）：种子化随机 kill -9 矩阵 + 重启不变量校验。
# Crash-consistency drill: seeded random kill -9 matrix plus post-restart
# invariant checking.
#
# 四个场景（可用 CHAOS_SCENARIOS 选择，逗号分隔）：
#   kill        —— 写压进行中随机时刻 kill -9；重启后 WAL/快照/计数/top-1/RPO 全查。
#   snapshot    —— 周期快照写到一半（failpoint pause 钉住窗口）kill -9；
#                  重启后验证 tmp 残留不影响恢复、旧数据完整。
#   failpoint   —— 注入 wal_append_pre=err（模拟 ENOSPC/EIO）：写必须失败且
#                  **不得被确认**；重启后不得出现幽灵数据。服务本身不得崩。
#   replication —— 主备 full_sync 写入后 kill follower → 重启追平；再 kill leader
#                  → promote → 用同一账本验证 RPO=0。
#
# 可复现性 / Reproducibility: `CHAOS_SEED` 唯一决定 kill 时刻与各轮延迟；
# 注入点按概率抽样（见 houyi-persistence/src/failpoint.rs）是刻意的不可复现项。
# 账本（intended/acked）记录真实发生的写，任何一次运行的断言都不依赖「应该写了多少」。
#
# 用法 / Usage:
#   ./scripts/chaos_drill.sh                       # 默认 3 轮 × 全部场景
#   CHAOS_SEED=12345 ./scripts/chaos_drill.sh      # 固定种子复现
#   CHAOS_ROUNDS=5 ./scripts/chaos_drill.sh        # 加深随机窗口
#   CHAOS_SCENARIOS=kill ./scripts/chaos_drill.sh  # 只跑一个场景
#   CHAOS_KEEP=1 ./scripts/chaos_drill.sh          # 保留现场
#
# 前提 / Prereq: 二进制必须以 `--features failpoints` 构建（HOUYI_BIN 显式给定时
# 自行保证）。未启用该 feature 时注入点为空操作，snapshot/failpoint 场景的
# 「钉住窗口」会退化为竞态——脚本会明确提示而不是静默降级。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

SEED="${CHAOS_SEED:-$(date +%s)}"
ROUNDS="${CHAOS_ROUNDS:-3}"
SCENARIOS="${CHAOS_SCENARIOS:-kill,snapshot,failpoint,replication}"
KEEP="${CHAOS_KEEP:-0}"
VECTORS="${CHAOS_VECTORS:-40}"
# 重启要重放全部历史写（debug 构建的 HNSW 插入不便宜，2 万条约 30s），
# 故启动超时按「重放需求」给足，而不是按首次启动的直觉值。
# EN: Restart replays all historical writes (HNSW insertion in a debug build is not
# EN: cheap; about 30s for 20k rows), so the startup timeout is sized by "replay
# EN: needs", not by first-start intuition.
START_TIMEOUT_SECS="${CHAOS_START_TIMEOUT_SECS:-120}"
# 每轮写入上限：控制重放成本随轮数线性增长（3 轮 × 5000 ≈ 20s 重放）。
# EN: Per-round write cap: keeps replay cost linear in the number of rounds
# EN: (3 rounds x 5000 is about 20s of replay).
WRITE_CAP="${CHAOS_WRITE_CAP:-5000}"

[[ "$SEED" =~ ^[0-9]+$ ]] || { echo "!! CHAOS_SEED 必须是非负整数（当前：${SEED}）" >&2; exit 2; }
[[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]] || { echo "!! CHAOS_ROUNDS 必须是正整数" >&2; exit 2; }

# 场景白名单：未知名字静默跳过会让「一个都没跑」也打印通过。
# EN: reject unknown scenario names — otherwise a typo yields a false green.
for s_ in ${SCENARIOS//,/ }; do
    case "$s_" in
        kill | snapshot | failpoint | replication) ;;
        *)
            echo "!! CHAOS_SCENARIOS 含未知场景 '$s_'（可选：kill,snapshot,failpoint,replication）" >&2
            exit 2
            ;;
    esac
done
ran_any=0
want() { [[ ",${SCENARIOS}," == *",$1,"* ]] && ran_any=1; }

# 端口：A 节点 17871/17872/17873，B 节点 17874/17875/17876。
# EN: Ports: node A 17871/17872/17873, node B 17874/17875/17876.
A_HTTP=17871; A_PG=17872; A_REPL=17873
B_HTTP=17874; B_PG=17875; B_REPL=17876

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-chaos.XXXXXX")
# pid 记录必须落文件而非数组：`pid=$(start_node ...)` 在命令替换的子壳里执行，
# 子壳对数组的追加对父壳不可见——泄漏的节点会占着固定端口污染下一次运行。
# EN: PIDs go to a file because `$(start_node ...)` runs in a subshell; an
# in-memory array would not see the append and leak the node.
PIDFILE="$WORKDIR/pids"
: >"$PIDFILE"
cleanup() {
  if [[ -f "$PIDFILE" ]]; then
    while read -r pid; do
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
      fi
    done <"$PIDFILE"
  fi
  if [[ "$KEEP" != "1" ]]; then
    rm -rf "$WORKDIR"
  else
    echo "==> CHAOS_KEEP=1, 保留现场: $WORKDIR"
  fi
}
trap cleanup EXIT

# ── 确定性伪随机（LCG）：同一 seed 产生同一 kill 序列 ──────────────────
# EN: -- Deterministic PRNG (LCG): the same seed yields the same kill sequence --
RAND_STATE=$(( (SEED % 2147483647) + 1 ))
rand_below() { # $1=上界（不含） → 0..$1-1
  RAND_STATE=$(( (RAND_STATE * 1103515245 + 12345) % 2147483648 ))
  echo $(( RAND_STATE % $1 ))
}

log() { echo "==> $*"; }
section() { echo; echo "── $* ────────────────────────────────────────"; }

# ── 二进制 ────────────────────────────────────────────────────────────
# EN: -- Binary --
if [[ -n "${HOUYI_BIN:-}" ]]; then
  BIN="$HOUYI_BIN"
  [[ -x "$BIN" ]] || { echo "!! HOUYI_BIN=$BIN 不可执行（显式设置即契约）" >&2; exit 1; }
else
  BIN="target/debug/houyi-server"
  log "构建 houyi-server（features=failpoints，崩溃注入所需）"
  cargo build -p houyi-server --features failpoints >/dev/null
fi

# ── 端口预检 ──────────────────────────────────────────────────────────
# EN: -- Port preflight --
for hp in "$A_HTTP" "$B_HTTP"; do
  c=0
  curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${hp}/v1/health" >/dev/null 2>&1 || c=$?
  [[ "$c" -eq 7 ]] || { echo "!! 127.0.0.1:${hp} 被占用（curl exit=${c}）" >&2; exit 1; }
done
for tp in "$A_PG" "$A_REPL" "$B_PG" "$B_REPL"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${tp}") 2>/dev/null; then
    echo "!! 127.0.0.1:${tp} 已有监听（残留进程?）" >&2
    exit 1
  fi
done

# ── 节点启动 ──────────────────────────────────────────────────────────
# $1=日志名 $2=数据目录 $3=http $4=pg $5=repl [其余透传给二进制]
# EN: -- Node startup --
# EN: $1=log name $2=data dir $3=http $4=pg $5=repl [remaining args passed to binary]
start_node() {
  local logname="$1" data="$2" http="$3" pg="$4" repl="$5"
  shift 5
  mkdir -p "$data"
  "$BIN" \
    --benchmark-http-addr "127.0.0.1:${http}" \
    --pgwire-addr "127.0.0.1:${pg}" \
    --wal-path "$data/chaos.wal" \
    --vector-dim 6 \
    --wal-write-mode async \
    --replication-listen-addr "127.0.0.1:${repl}" \
    "$@" >"$WORKDIR/$logname" 2>&1 &
  local pid=$!
  echo "$pid" >>"$PIDFILE"
  local deadline=$((SECONDS + START_TIMEOUT_SECS))
  while (( SECONDS < deadline )); do
    if curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${http}/v1/health" >/dev/null 2>&1; then
      echo "$pid"
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || { tail -20 "$WORKDIR/$logname" >&2; return 1; }
    sleep 0.1
  done
  echo "!! 节点启动超时（${logname}）:" >&2
  tail -20 "$WORKDIR/$logname" >&2
  kill -9 "$pid" 2>/dev/null || true
  return 1
}

# 等端口关闭（kill -9 后内核回收监听需要一点时间）。
# EN: Wait for the port to close (the kernel needs a moment to release the listener
# EN: after kill -9).
wait_port_free() { # $1=port
  local deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null || return 0
    sleep 0.05
  done
  echo "!! 端口 $1 在 10s 内未释放" >&2
  return 1
}

hard_kill() { # $1=pid
  kill -9 "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
}

# ── 写压（inline python）：先记 intended、200 后记 acked ───────────────
# 参数：base_url intended_file acked_file start_id [dim]
# EN: -- Write load (inline python): record intended before, acked after 200 --
# EN: args: base_url intended_file acked_file start_id [dim]
writer_py() {
  cat <<'PY'
import json, sys, urllib.request, urllib.error
base, intended_path, acked_path, start, cap = (
    sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]))
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
batch = 10
cur = start
end = start + cap
try:
    while cur < end:
        ids = list(range(cur, cur + batch))
        with open(intended_path, "a") as f:
            f.write("\n".join(map(str, ids)) + "\n")
            f.flush()
        body = json.dumps({"vectors": [
            {"id": i, "values": [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1, (i // 1155) % 2048 + 1], "payload": {"row": i}}
            for i in ids
        ]}).encode()
        req = urllib.request.Request(
            f"{base}/v1/collections/chaos/upsert", data=body,
            headers={"Content-Type": "application/json"})
        with opener.open(req, timeout=10) as resp:
            json.loads(resp.read())
        with open(acked_path, "a") as f:
            f.write("\n".join(map(str, ids)) + "\n")
            f.flush()
        cur += batch
except urllib.error.HTTPError as e:
    # 服务端还活着却拒绝写入 = 真故障（ENOSPC 注入之外不应出现）。
    # EN: The server is alive but rejects writes = real fault (should not happen outside
    # EN: ENOSPC injection).
    print(f"!! writer: server rejected a write: HTTP {e.code}", file=sys.stderr)
    sys.exit(1)
except Exception:
    # kill -9 后连接必然失败——这是本轮终止条件，不是错误。
    # EN: Connections must fail after kill -9 — that is this round's termination
    # EN: condition, not an error.
    pass
PY
}

collection_count() { # $1=http $2=name → 数字或空
  local body="$WORKDIR/count.$$.json"
  local code
  code=$(curl -s --max-time 10 --noproxy '*' -o "$body" -w "%{http_code}" \
    "http://127.0.0.1:$1/v1/collections/$2/status" || true)
  [[ "$code" == "200" ]] || { echo ""; return 0; }
  python3 -c "
import json
d = json.load(open('$body'))
print(d['indexed_count'] + d['pending_count'])
"
}

wait_count() { # $1=http $2=name $3=want $4=label
  local deadline=$((SECONDS + 25))
  while (( SECONDS < deadline )); do
    [[ "$(collection_count "$1" "$2")" == "$3" ]] && return 0
    sleep 0.2
  done
  echo "!! ${4}：$2 计数 $(collection_count "$1" "$2") != $3" >&2
  return 1
}

check_invariants() { # $1=data_dir $2=http $3=acked $4=intended $5=label $6=wal_name
  local dir="$1" http="$2" acked="$3" intended="$4" label="$5" wal="${6:-chaos.wal}"
  python3 "$SCRIPT_DIR/chaos_invariants.py" \
    --data-dir "$dir" --wal-name "$wal" --http "127.0.0.1:${http}" \
    --acked-file "$acked" --intended-file "$intended" \
    || { echo "!! ${label}：不变量违例（见上）" >&2; return 1; }
}

post_json() { # $1=url $2=file-or-/dev/null $3=out → 状态码
  local args=(-s --max-time 60 --noproxy '*' -o "$3" -w "%{http_code}" -X POST
              -H 'Content-Type: application/json')
  [[ "$2" != "/dev/null" ]] && args+=(--data @"$2")
  curl "${args[@]}" "$1"
}

# ══════════════════════════════════════════════════════════════════════
# 场景 1：随机时刻 kill -9（写压中）
# EN: Scenario 1: kill -9 at a random moment (during write load)
# ══════════════════════════════════════════════════════════════════════
scenario_kill() {
  section "场景 kill：写压中随机 kill -9 × ${ROUNDS} 轮"
  local dir="$WORKDIR/kill"
  local intended="$WORKDIR/kill.intended" acked="$WORKDIR/kill.acked"
  : >"$intended"; : >"$acked"
  local pid="" next_id=1000000

  for ((round = 1; round <= ROUNDS; round++)); do
    if [[ -z "$pid" ]]; then
      pid=$(start_node "kill-${round}.log" "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
      log "第 ${round} 轮：节点已启动 pid=$pid"
    fi
    writer_py | python3 - "http://127.0.0.1:${A_HTTP}" "$intended" "$acked" "$next_id" "$WRITE_CAP" &
    local writer=$!
    # 注意：writer 的 HTTPError 分支非零退出；wait 的返回值在下面统一检查。
    # EN: Note: the writer's HTTPError branch exits non-zero; wait's return value is
    # EN: checked uniformly below.
    local delay_ms=$(( 150 + $(rand_below 650) ))
    sleep "$(python3 -c "print(${delay_ms}/1000)")"
    hard_kill "$pid"
    pid=""
    if ! wait "$writer"; then
        echo "!! 写压进程异常退出（服务端拒绝写？见上方 stderr）" >&2
        exit 1
    fi
    wait_port_free "$A_HTTP"
    next_id=$((next_id + 20000))
    log "第 ${round} 轮：kill -9 @ ${delay_ms}ms；已确认 $(grep -c . "$acked" || true) 条"
    pid=$(start_node "kill-restart-${round}.log" "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
    check_invariants "$dir" "$A_HTTP" "$acked" "$intended" "kill 第 ${round} 轮"
  done
  hard_kill "$pid"
  log "场景 kill 通过"
}

# ══════════════════════════════════════════════════════════════════════
# 场景 2：图快照写到一半 kill -9
# EN: Scenario 2: kill -9 in the middle of writing a graph snapshot
# ══════════════════════════════════════════════════════════════════════
scenario_snapshot() {
  section "场景 snapshot：快照写入中 kill -9"
  local dir="$WORKDIR/snap"
  mkdir -p "$dir"
  local intended="$WORKDIR/snap.intended" acked="$WORKDIR/snap.acked"
  : >"$intended"; : >"$acked"

  # 先写一批数据，让快照有内容可写。
  # EN: Write a batch of data first so the snapshot has content.
  local pid
  pid=$(start_node snap.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
  local body="$WORKDIR/snap.json"
  python3 - "$body" 100 <<'PY'
import json, sys
path, n = sys.argv[1], int(sys.argv[2])
with open(path, "w") as f:
    json.dump({"vectors": [
        {"id": 2000000 + i, "values": [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1, (i // 1155) % 2048 + 1], "payload": {"row": 2000000 + i}}
        for i in range(n)
    ]}, f)
PY
  local code
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/snap/upsert" "$body" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! 预置写入失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  wait_count "$A_HTTP" snap 100 "snapshot 预置"
  hard_kill "$pid"
  wait_port_free "$A_HTTP"

  # 重启：周期快照 + 在「写了一半」处暂停 3s。
  # 注意必须 export 后调用：`VAR=x pid=$(...)` 里的赋值在命令替换展开之后才生效，
  # 子进程拿不到（bash 的求值顺序）。
  # EN: Restart: periodic snapshot + pause 3s at the "half-written" point.
  # EN: Note: must export then call: in `VAR=x pid=$(...)` the assignment takes effect
  # EN: after command substitution expansion, so the child does not see it (bash
  # EN: evaluation order).
  export HOUYI_FAILPOINTS="graph_snapshot_write_mid=1:pause:3000"
  pid=$(start_node snap2.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL" --snapshot-interval-secs 1)
  unset HOUYI_FAILPOINTS
  # 等 tmp 残留出现（= kill 窗口已打开）。
  # EN: Wait for the tmp residue to appear (= the kill window is open).
  local deadline=$((SECONDS + 15)) tmp=""
  while (( SECONDS < deadline )); do
    tmp=$(find "$dir" -name '*.tmp' -print -quit 2>/dev/null || true)
    [[ -n "$tmp" ]] && break
    sleep 0.1
  done
  [[ -n "$tmp" ]] || { echo "!! 15s 内未观察到快照 tmp 窗口（failpoints 未生效？）" >&2; exit 1; }
  log "已进入快照写入窗口：$(basename "$tmp")，kill -9"
  hard_kill "$pid"
  wait_port_free "$A_HTTP"

  # 重启（不带注入）：残留 tmp 不得影响恢复。
  # EN: Restart (without injection): the leftover tmp must not affect recovery.
  pid=$(start_node snap3.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL" --snapshot-interval-secs 1)
  wait_count "$A_HTTP" snap 100 "snapshot 首次恢复"

  # 等一轮**完整**快照落盘（注入已关闭），确认 tmp+rename 正常收尾。
  # EN: Wait for one **complete** snapshot to land (injection off), confirming
  # EN: tmp+rename finishes normally.
  local snapfile="$dir/chaos.graph_snapshot.bin"
  local d2=$((SECONDS + 20))
  while (( SECONDS < d2 )); do
    [[ -f "$snapfile" ]] && break
    sleep 0.2
  done
  [[ -f "$snapfile" ]] || { echo "!! 20s 内未写出完整快照: $snapfile" >&2; exit 1; }
  hard_kill "$pid"
  wait_port_free "$A_HTTP"

  # 最终重启：校验器同时检查快照结构（CRC）与 WAL 重放/CAS 计数。
  # EN: Final restart: the verifier checks both the snapshot structure (CRC) and WAL
  # EN: replay/CAS counts.
  pid=$(start_node snap4.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
  check_invariants "$dir" "$A_HTTP" "$acked" "$intended" "snapshot 恢复"
  wait_count "$A_HTTP" snap 100 "snapshot 二次恢复"
  hard_kill "$pid"
  log "场景 snapshot 通过"
}

# ══════════════════════════════════════════════════════════════════════
# 场景 3：注入系统错误（ENOSPC/EIO）
# EN: Scenario 3: inject system errors (ENOSPC/EIO)
# ══════════════════════════════════════════════════════════════════════
scenario_failpoint() {
  section "场景 failpoint：wal_append_pre=err（模拟 ENOSPC）"
  local dir="$WORKDIR/fp"
  mkdir -p "$dir"
  local pid
  export HOUYI_FAILPOINTS="wal_append_pre=1:err"
  pid=$(start_node fp.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
  unset HOUYI_FAILPOINTS

  local body="$WORKDIR/fp.json"
  echo '{"vectors":[{"id":1,"values":[1.0,1,1,1,1,1]}]}' >"$body"
  local code
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/fp/upsert" "$body" "$WORKDIR/r.json")
  [[ "$code" != "200" ]] || { echo "!! 注入 ENOSPC 后写仍返回 200（错误被吞）" >&2; exit 1; }
  log "写入按预期失败：HTTP $code"
  curl -sf --max-time 5 --noproxy '*' "http://127.0.0.1:${A_HTTP}/v1/health" >/dev/null \
    || { echo "!! 注入错误后服务不再健康（应只影响写入）" >&2; exit 1; }
  hard_kill "$pid"
  wait_port_free "$A_HTTP"

  # 重启（不带注入）：失败写不得留下任何幽灵记录。
  # EN: Restart (without injection): failed writes must not leave ghost records.
  : >"$WORKDIR/fp.acked"; : >"$WORKDIR/fp.intended"
  pid=$(start_node fp2.log "$dir" "$A_HTTP" "$A_PG" "$A_REPL")
  check_invariants "$dir" "$A_HTTP" "$WORKDIR/fp.acked" "$WORKDIR/fp.intended" "failpoint 重启"
  local count
  count=$(collection_count "$A_HTTP" fp)
  [[ -z "$count" || "$count" == "0" ]] || { echo "!! 幽灵数据：fp 计数=$count" >&2; exit 1; }
  log "重启后无幽灵数据（计数=${count:-不存在}）"
  hard_kill "$pid"
  log "场景 failpoint 通过"
}

# ══════════════════════════════════════════════════════════════════════
# 场景 4：复制链路的崩溃与 promote
# EN: Scenario 4: replication chain crash and promote
# ══════════════════════════════════════════════════════════════════════
scenario_replication() {
  section "场景 replication：full_sync 主备 → kill follower/leader → promote"
  local ldir="$WORKDIR/repl-leader" fdir="$WORKDIR/repl-follower"
  mkdir -p "$ldir" "$fdir"
  local acked="$WORKDIR/repl.acked" intended="$WORKDIR/repl.intended"
  : >"$acked"; : >"$intended"

  local lpid fpid
  lpid=$(start_node repl-leader.log "$ldir" "$A_HTTP" "$A_PG" "$A_REPL" --role leader)
  fpid=$(start_node repl-follower.log "$fdir" "$B_HTTP" "$B_PG" "$B_REPL" \
    --role follower --replication-leader-addr "127.0.0.1:${A_REPL}")
  log "leader pid=$lpid follower pid=$fpid"

  # full_sync：先探针建集合 → 设级别 → 整批写（200 = follower 已持久确认）。
  # EN: full_sync: probe to create the collection -> set level -> whole-batch write
  # EN: (200 = follower durably confirmed).
  local probe="$WORKDIR/repl-probe.json" batch="$WORKDIR/repl-batch.json"
  python3 - "$probe" "$batch" "$VECTORS" <<'PY'
import json, sys
probe, batch, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
with open(probe, "w") as f:
    json.dump({"vectors": [{"id": 3000000, "values": [1.0, 1, 1, 1, 1, 1], "payload": {"row": 3000000}}]}, f)
with open(batch, "w") as f:
    json.dump({"vectors": [
        {"id": 3000000 + i, "values": [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1, (i // 1155) % 2048 + 1], "payload": {"row": 3000000 + i}}
        for i in range(n)
    ]}, f)
PY
  local code
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/crepl/upsert" "$probe" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! 探针写入失败 $code" >&2; exit 1; }
  echo '{"ha_level":"full_sync"}' >"$WORKDIR/ha.json"
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/crepl/ha_level" "$WORKDIR/ha.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! 设 full_sync 失败 $code" >&2; exit 1; }
  python3 - "$acked" "$intended" "$VECTORS" <<'PY'
import sys
acked, intended, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
ids = [3000000 + i for i in range(n)]
open(intended, "a").write("\n".join(map(str, ids)) + "\n")
open(acked, "a").write("\n".join(map(str, ids)) + "\n")
PY
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/crepl/upsert" "$batch" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! full_sync 整批失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  wait_count "$B_HTTP" crepl "$VECTORS" "follower 首次追平"
  log "follower 追平（${VECTORS}）"

  # kill follower → 重启 → 追平；账本（leader 已确认）必须全部出现在 follower。
  # EN: kill follower -> restart -> catch up; the ledger (leader-confirmed) must all
  # EN: appear on the follower.
  hard_kill "$fpid"
  wait_port_free "$B_HTTP"
  fpid=$(start_node repl-follower2.log "$fdir" "$B_HTTP" "$B_PG" "$B_REPL" \
    --role follower --replication-leader-addr "127.0.0.1:${A_REPL}")
  wait_count "$B_HTTP" crepl "$VECTORS" "follower 重启追平"
  check_invariants "$fdir" "$B_HTTP" "$acked" "$intended" "follower 重启（RPO=0）"
  log "follower 重启后 RPO=0 ✓"

  # kill leader → promote follower → 新主必须可查全部已确认写。
  # EN: kill leader -> promote follower -> the new leader must serve all confirmed
  # EN: writes.
  hard_kill "$lpid"
  wait_port_free "$A_HTTP"
  code=$(post_json "http://127.0.0.1:${B_HTTP}/v1/admin/promote" "/dev/null" "$WORKDIR/promote.json")
  [[ "$code" == "200" ]] || { echo "!! promote 失败 $code: $(cat "$WORKDIR/promote.json")" >&2; exit 1; }
  check_invariants "$fdir" "$B_HTTP" "$acked" "$intended" "promote 后（RPO=0）"
  log "promote 后 RPO=0 ✓"
  hard_kill "$fpid"
  log "场景 replication 通过"
}

# ══════════════════════════════════════════════════════════════════════
# 入口
# EN: Entry
# ══════════════════════════════════════════════════════════════════════
log "chaos drill：seed=${SEED} rounds=${ROUNDS} scenarios=${SCENARIOS}"
echo "   工作目录: $WORKDIR"

# 校验器自身的负路径自测先跑——校验器坏了，后面所有「通过」都不可信。
# EN: Run the verifier's own negative-path self-test first — if the verifier is
# EN: broken, every later "pass" is untrustworthy.
python3 "$SCRIPT_DIR/chaos_invariants.py" --selftest

# 注入点解析的单元测试（feature 门控，不在默认 `cargo test --all` 里）。
# EN: Unit test for injection-point parsing (feature-gated; not in default
# EN: `cargo test --all`).
log "failpoint 单元测试"
cargo test -p houyi-persistence --features failpoints --lib failpoint >/dev/null \
  || { echo "!! failpoint 单元测试失败" >&2; exit 1; }

want kill && scenario_kill
want snapshot && scenario_snapshot
want failpoint && scenario_failpoint
want replication && scenario_replication

[ "$ran_any" -eq 1 ] || { echo "!! CHAOS_SCENARIOS='${SCENARIOS}' 没有选中任何场景" >&2; exit 2; }
log "演练通过：seed=${SEED}（复现：CHAOS_SEED=${SEED}）"
