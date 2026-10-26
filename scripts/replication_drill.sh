#!/usr/bin/env bash
# 复制 failover 演练（P4.4.3）：主备实时同步 → 三级 HA 语义 → kill -9 主 →
# 手动 promote → RPO=0 验证 → 旧主清目录重并追平。
# Replication failover drill: live leader/follower sync, three HA levels, kill
# the leader, manual promote, RPO=0 verification, then wipe-and-rejoin the old
# leader as a follower.
#
# 防脑裂处置手册（v1 人工流程，本脚本第 [9] 步即其可执行形态）：
#   旧主恢复后**不得**直接以 leader 身份上线（双主脑裂）。两种情形：
#   - 旧主 resume_lsn > 新主 next_lsn（旧主有未复制的写）：协议层会识别
#     follower_ahead 并拒绝（ERROR kind + 无限退避 + 指标
#     follower_ahead_rejections_total）——人工裁决数据取舍后，**清空旧主
#     数据目录**，以 follower 身份重启全量重同步；
#   - 旧主 LSN 不超前：同样建议清目录重同步（v1 无增量冲突合并）。
#   EN: Split-brain runbook — a recovered old leader must NOT come back as
#   leader. Wipe its data dir and rejoin as a follower (full resync); the
#   protocol rejects a follower that is ahead (follower_ahead) pending manual
#   intervention. Step [9] below is this procedure in executable form.
#
# 用法 / Usage:
#   ./scripts/replication_drill.sh                 # 全流程演练
#   HOUYI_BIN=target/release/houyi-server ./scripts/replication_drill.sh
#
# 环境变量:
#   HOUYI_BIN        houyi-server 二进制；显式设置即契约（同 backup_restore_drill）
#   DRILL_VECTORS    每 collection 向量数（默认 100，5 维；上限 1155 硬约束——
#                    方向唯一性判据同 backup_restore_drill）
#   DRILL_KEEP=1     演练结束后保留临时目录（调试用）
#   DRILL_SELFTEST=1 只跑校验矩阵自测后退出，不起服务

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

VECTORS="${DRILL_VECTORS:-100}"

# 端口分配：leader 17891(HTTP)/17892(PG)/17893(REPL)，follower 17894/17895/17896。
# EN: Port allocation: leader 17891(HTTP)/17892(PG)/17893(REPL), follower
# EN: 17894/17895/17896.
L_HTTP=17891; L_PG=17892; L_REPL=17893
F_HTTP=17894; F_PG=17895; F_REPL=17896

# 与 backup_restore_drill 同一校验（返回码契约：0 合法 / 1 格式非法 / 2 超方向周期）。
# EN: Same validation as backup_restore_drill (return-code contract: 0 valid /
# EN: 1 invalid format / 2 above the direction cycle).
validate_vectors() {
  local v="$1"
  [[ "$v" =~ ^(0|[1-9][0-9]{0,9})$ ]] || return 1
  (( v >= 1 )) || return 1
  (( v <= 1155 )) || return 2
  return 0
}

if [[ "${DRILL_SELFTEST:-0}" == "1" ]]; then
  st_fail=0; st_total=0
  for v in 0 08 09 0010 99999999999999999999 12345678901 -5 abc "" " 5" "+5" "5.5" "0x10"; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    [[ "$rc" -ne 1 ]] && { echo "!! 自测失败: '$v' 期望 rc=1 实得 $rc" >&2; st_fail=1; }
  done
  for v in 1156 9999999999; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    [[ "$rc" -ne 2 ]] && { echo "!! 自测失败: '$v' 期望 rc=2 实得 $rc" >&2; st_fail=1; }
  done
  for v in 1 100 1155; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    [[ "$rc" -ne 0 ]] && { echo "!! 自测失败: '$v' 期望 rc=0 实得 $rc" >&2; st_fail=1; }
  done
  [[ "$st_fail" -eq 0 ]] && echo "==> 自测通过：${st_total} 例校验矩阵全部符合预期"
  exit "$st_fail"
fi

rc=0; validate_vectors "$VECTORS" || rc=$?
case "$rc" in
  0) ;;
  1) echo "!! DRILL_VECTORS='$VECTORS' 必须是正整数（无前导零，≤10 位）" >&2; exit 1 ;;
  2) echo "!! DRILL_VECTORS=$VECTORS 超过方向周期上限 1155，top-1 判据将失真" >&2; exit 1 ;;
  *) echo "!! validate_vectors 返回未知分类 rc=${rc}（返回码契约被破坏）" >&2; exit 1 ;;
esac

# 方向唯一性守卫：[7/9] 追加的 extra id(3100000) 相对批首(3000000) 的方向槽
# 偏移 = 100000 mod 1155 = 670；批次占用槽位 0..VECTORS-1。VECTORS≥671 时
# extra 与批内向量撞方向，[8/9] top-1 判据出现平局失真——启动期拒绝。
# EN: Direction-uniqueness guard: the extra id (3100000) added by [7/9] is offset
# EN: from the batch start (3000000) by 100000 mod 1155 = 670 slots; the batch
# EN: occupies slots 0..VECTORS-1. With VECTORS >= 671 the extra collides with an
# EN: in-batch direction, and the top-1 criterion in [8/9] becomes a tie distortion —
# EN: reject at startup.
EXTRA_DIR_OFF=$(( (3100000 - 3000000) % 1155 ))
if (( EXTRA_DIR_OFF < VECTORS )); then
  echo "!! DRILL_VECTORS=$VECTORS 与 extra id 方向槽冲突（extra 偏移=$EXTRA_DIR_OFF < $VECTORS），top-1 判据将平局失真" >&2
  exit 1
fi

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-repl-drill.XXXXXX")
L_DATA="$WORKDIR/leader"
F_DATA="$WORKDIR/follower"
mkdir -p "$L_DATA" "$F_DATA"

LEADER_PID=""
FOLLOWER_PID=""
cleanup() {
  for pid in "${LEADER_PID}" "${FOLLOWER_PID}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      # pid 经命令替换 $(start_node ...) 捕获——节点是子壳的子进程而非本 shell
      # 的，wait 恒 "not a child"（127 被 || true 吞掉）：kill 有效、wait 仅兜底。
      # EN: pid is captured via command substitution $(start_node ...) — the node is a
      # EN: grandchild, not a child of this shell, so wait always says "not a child" (127
      # EN: swallowed by || true): kill works, wait is only a fallback.
      wait "$pid" 2>/dev/null || true
    fi
  done
  if [[ "${DRILL_KEEP:-0}" != "1" ]]; then
    rm -rf "$WORKDIR"
  else
    echo "==> DRILL_KEEP=1, 保留现场: $WORKDIR"
  fi
}
trap cleanup EXIT

if [[ -n "${HOUYI_BIN:-}" ]]; then
  BIN="$HOUYI_BIN"
  if [[ ! -f "$BIN" || ! -x "$BIN" ]]; then
    echo "!! HOUYI_BIN=$BIN 不存在/不是文件/不可执行（显式设置即契约，不回退）" >&2
    exit 1
  fi
else
  BIN="target/debug/houyi-server"
  if [[ ! -f "$BIN" || ! -x "$BIN" ]]; then
    echo "==> 构建 houyi-server (debug)"
    cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p houyi-server >/dev/null
  fi
fi

# 端口预检（六口）：HTTP 用 curl exit 7 语义（与 backup_restore_drill 同纪律，
# --noproxy '*' 强制直连）；PG/REPL 无 HTTP 探针，用 /dev/tcp 占位检测。
# EN: Port preflight (six ports): HTTP uses curl exit 7 semantics (same discipline
# EN: as backup_restore_drill, --noproxy '*' forces direct); PG/REPL have no HTTP
# EN: probe, so use /dev/tcp placeholder detection.
for hp in "$L_HTTP" "$F_HTTP"; do
  health_curl=0
  curl -sf --max-time 3 --noproxy '*' "http://127.0.0.1:${hp}/v1/health" >/dev/null 2>&1 || health_curl=$?
  if [[ "$health_curl" -ne 7 ]]; then
    echo "!! 127.0.0.1:${hp} 被占用或探测异常（curl exit=${health_curl}，残留 houyi-server?）" >&2
    exit 1
  fi
done
for tp in "$L_PG" "$L_REPL" "$F_PG" "$F_REPL"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${tp}") 2>/dev/null; then
    echo "!! 127.0.0.1:${tp} 已有 TCP 监听（残留进程?）" >&2
    exit 1
  fi
done

# $1=log $2=data_dir $3=role $4=http $5=pg $6=repl [$7=leader_addr]
# 启动后等 health（总时限 15s deadline 模式，单次探测带 2s 上限）。
start_node() {
  local log="$1" data="$2" role="$3" http="$4" pg="$5" repl="$6" leader_addr="${7:-}"
  local extra=()
  if [[ "$role" == "follower" ]]; then
    extra+=(--replication-leader-addr "$leader_addr")
  fi
  "$BIN" \
    --benchmark-http-addr "127.0.0.1:${http}" \
    --pgwire-addr "127.0.0.1:${pg}" \
    --wal-path "$data/drill.wal" \
    --vector-dim 5 \
    --wal-write-mode async \
    --role "$role" \
    --replication-listen-addr "127.0.0.1:${repl}" \
    ${extra[@]+"${extra[@]}"} \
    >"$WORKDIR/$log" 2>&1 &
  local pid=$!
  local deadline=$((SECONDS + 15))
  while (( SECONDS < deadline )); do
    if curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${http}/v1/health" >/dev/null 2>&1; then
      echo "$pid"
      return 0
    fi
    sleep 0.1
  done
  echo "!! 节点启动超时（role=${role}）, 日志:" >&2
  tail -20 "$WORKDIR/$log" >&2
  # 超时分支必须杀掉半起进程：端口是固定预检的，泄漏节点会污染后续重跑。
  # EN: The timeout branch must kill the half-started process: ports are fixed and
  # EN: preflighted, so a leaked node pollutes the next rerun.
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  return 1
}

# HTTP 辅助：body 写 $2 指向的文件，回显状态码（调用方判等，归因信息在自己手里）。
# EN: HTTP helper: body is written to the file in $2, the status code is echoed
# EN: (callers compare it; attribution stays with them).
http_code() {
  curl -s --max-time 30 --noproxy '*' -o "$2" -w "%{http_code}" "$1"
}
http_post() { # $1=url $2=json-file $3=out-file → 状态码
  curl -s --max-time 60 --noproxy '*' -o "$3" -w "%{http_code}" \
    -X POST "$1" -H 'Content-Type: application/json' --data @"$2"
}

# 生成确定性向量（方向唯一，dim0 恒 1.0；判据同 backup_restore_drill）。
# $1=id 起点 $2=数量 $3=输出文件
# EN: Generate deterministic vectors (unique direction, dim0 always 1.0; same
# EN: criterion as backup_restore_drill).
# EN: $1=id start $2=count $3=output file
gen_vectors() {
  python3 - "$1" "$2" >"$3" <<'PY'
import json, sys
base, n = int(sys.argv[1]), int(sys.argv[2])
vecs = [
    {"id": i, "values": [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1]}
    for i in range(base, base + n)
]
print(json.dumps({"vectors": vecs}))
PY
}

# collection 计数 = indexed + pending（builder 队列中的写同样持久、可查但尚未建图）。
# EN: collection count = indexed + pending (writes in the builder queue are also
# EN: durable and queryable but not yet graphed).
collection_count() { # $1=http_port $2=collection → 计数或空（404）
  local body="$WORKDIR/count.$$.json"
  local code
  code=$(http_code "http://127.0.0.1:$1/v1/collections/$2/status" "$body")
  if [[ "$code" != "200" ]]; then
    echo ""
    return 0
  fi
  python3 -c "
import json
d = json.load(open('$body'))
print(d['indexed_count'] + d['pending_count'])
"
}

# 等 follower 追平：三 collection 计数全部到位（deadline 20s，轮询消调度竞争）。
# 期望计数按 collection 分别给（failover 后 c_full 多一条，不能一刀切）。
# EN: Wait for the follower to catch up: all three collection counts in place
# EN: (deadline 20s, polling absorbs scheduling jitter).
# EN: Expected counts are per collection (c_full has one extra row after failover; a
# EN: single value will not do).
wait_counts() { # $1=http_port $2=e_async $3=e_semi $4=e_full $5=阶段名
  local deadline=$((SECONDS + 20))
  while (( SECONDS < deadline )); do
    local ok=1 c
    for spec in "c_async:$2" "c_semi:$3" "c_full:$4"; do
      local name="${spec%%:*}" want="${spec##*:}"
      c=$(collection_count "$1" "$name")
      [[ "$c" == "$want" ]] || { ok=0; break; }
    done
    [[ "$ok" == "1" ]] && return 0
    sleep 0.2
  done
  echo "!! $5：计数未在 20s 内到位（期望 $2/$3/$4）:" >&2
  for name in c_async c_semi c_full; do
    echo "    $name = $(collection_count "$1" "$name")" >&2
  done
  return 1
}

echo "==> [1/9] 启动 leader(:${L_HTTP}/repl:${L_REPL}) 与 follower(:${F_HTTP}/repl:${F_REPL})"
LEADER_PID=$(start_node leader1.log "$L_DATA" leader "$L_HTTP" "$L_PG" "$L_REPL")
FOLLOWER_PID=$(start_node follower1.log "$F_DATA" follower "$F_HTTP" "$F_PG" "$F_REPL" "127.0.0.1:${L_REPL}")
echo "    leader pid=$LEADER_PID follower pid=$FOLLOWER_PID"

echo "==> [2/9] follower 只读断言（写 503 read_only / 读 200）与幂等前置（leader 上 promote 409）"
gen_vectors 9000000 1 "$WORKDIR/probe.json"
code=$(http_post "http://127.0.0.1:${F_HTTP}/v1/collections/ro_probe/upsert" "$WORKDIR/probe.json" "$WORKDIR/ro_resp.json")
[[ "$code" == "503" ]] || { echo "!! follower upsert 期望 503 实得 $code: $(cat "$WORKDIR/ro_resp.json")" >&2; exit 1; }
grep -q "read_only" "$WORKDIR/ro_resp.json" || { echo "!! 503 报文未点名 read_only: $(cat "$WORKDIR/ro_resp.json")" >&2; exit 1; }
# search 需要一个合法请求体（collection 不存在也应回 404 而非 5xx/拒读）——
# 只读语义的关键是「读路径不被 503 拦截」，404 同样是放行后的业务响应。
# EN: search needs a valid request body (a missing collection should return 404, not
# EN: 5xx / rejected read) — the key to read-only semantics is "the read path is not
# EN: blocked by 503"; 404 is likewise a business response after being admitted.
echo '{"vector":[1.0,1,1,1,1],"k":1}' > "$WORKDIR/probe_search.json"
code=$(http_post "http://127.0.0.1:${F_HTTP}/v1/collections/ro_probe/search" "$WORKDIR/probe_search.json" "$WORKDIR/ro_search_resp.json")
[[ "$code" == "200" || "$code" == "404" ]] || { echo "!! follower search 期望 200/404 实得 $code" >&2; exit 1; }
code=$(http_post "http://127.0.0.1:${L_HTTP}/v1/admin/promote" /dev/null "$WORKDIR/promote_leader.json")
[[ "$code" == "409" ]] || { echo "!! leader 上 promote 期望 409 实得 $code: $(cat "$WORKDIR/promote_leader.json")" >&2; exit 1; }
echo "    只读/幂等断言通过"

echo "==> [3/9] 三 collection 设三级，各写入 ${VECTORS} 条"
# 每 collection：先 upsert 1 条探针（隐式建 collection）→ 设级别 → 整批
# upsert（含探针同 id，幂等覆盖）——整批在目标级别保护下完成，sync 级别的
# 成功响应 = follower 已确认。
# EN: Per collection: first upsert 1 probe row (implicit collection creation) -> set
# EN: level -> whole-batch upsert (same probe id included, idempotent overwrite) — the
# EN: batch completes under the target level, and a successful sync-level response =
# EN: follower confirmed.
gen_vectors 1000000 "$VECTORS" "$WORKDIR/upsert_async.json"
gen_vectors 2000000 "$VECTORS" "$WORKDIR/upsert_semi.json"
gen_vectors 3000000 "$VECTORS" "$WORKDIR/upsert_full.json"
gen_vectors 1000000 1 "$WORKDIR/probe_async.json"
gen_vectors 2000000 1 "$WORKDIR/probe_semi.json"
gen_vectors 3000000 1 "$WORKDIR/probe_full.json"

for spec in "c_async:async:probe_async:upsert_async:1000000" \
            "c_semi:semi_sync:probe_semi:upsert_semi:2000000" \
            "c_full:full_sync:probe_full:upsert_full:3000000"; do
  IFS=':' read -r name level probe upsert _ <<<"$spec"
  code=$(http_post "http://127.0.0.1:${L_HTTP}/v1/collections/${name}/upsert" "$WORKDIR/${probe}.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $name 探针 upsert 失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  echo "{\"ha_level\":\"${level}\"}" > "$WORKDIR/ha.json"
  code=$(http_post "http://127.0.0.1:${L_HTTP}/v1/collections/${name}/ha_level" "$WORKDIR/ha.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $name 设级别 $level 失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  code=$(http_post "http://127.0.0.1:${L_HTTP}/v1/collections/${name}/upsert" "$WORKDIR/${upsert}.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $name 整批 upsert 失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  INSERTED=$(grep -o '"inserted":[0-9]*' "$WORKDIR/r.json" | cut -d: -f2 || true)
  [[ "$INSERTED" == "$VECTORS" ]] || { echo "!! $name inserted=$INSERTED != $VECTORS" >&2; exit 1; }
done
echo "    三 collection 写入确认（各 inserted=${VECTORS}）"

echo "==> [4/9] follower 追平断言（计数 + sync 指标）"
wait_counts "$F_HTTP" "$VECTORS" "$VECTORS" "$VECTORS" "follower 追平"
echo "    follower 三 collection 计数 = $VECTORS ✓"
curl -sf --max-time 5 --noproxy '*' "http://127.0.0.1:${L_HTTP}/metrics" > "$WORKDIR/leader_metrics.txt"
# sync_waits：c_semi + c_full 整批各 1 次等待（探针写时是 async 默认级别）。
# 行尾锚定防前缀误匹配（"total 2" 也会匹配 "total 20"）。
# EN: sync_waits: one wait each for the c_semi + c_full batches (probe writes use the
# EN: async default level).
# EN: End-of-line anchoring avoids prefix false matches ("total 2" would also match
# EN: "total 20").
grep -qE "houyidb_replication_sync_waits_total 2$" "$WORKDIR/leader_metrics.txt" \
  || { echo "!! leader sync_waits_total 期望 2: $(grep sync_waits "$WORKDIR/leader_metrics.txt")" >&2; exit 1; }
grep -qE "houyidb_replication_sync_timeouts_total 0$" "$WORKDIR/leader_metrics.txt" \
  || { echo "!! 出现 sync 超时降级（loopback 不应超时）: $(grep sync_timeouts "$WORKDIR/leader_metrics.txt")" >&2; exit 1; }
echo "    leader sync_waits=2 timeouts=0 ✓（semi/full 各 1 次 ACK 等待）"

echo "==> [5/9] 模拟主宕机：kill -9 leader"
kill -9 "$LEADER_PID" 2>/dev/null || true
wait "$LEADER_PID" 2>/dev/null || true
LEADER_PID=""
echo "    leader 已 SIGKILL（full_sync 语义：杀前成功的写 follower 已 fsync+可查）"

echo "==> [6/9] follower 手动 promote（幂等 409 一并断言）"
code=$(http_post "http://127.0.0.1:${F_HTTP}/v1/admin/promote" /dev/null "$WORKDIR/promote1.json")
[[ "$code" == "200" ]] || { echo "!! promote 期望 200 实得 $code: $(cat "$WORKDIR/promote1.json")" >&2; exit 1; }
grep -q '"promoted":true' "$WORKDIR/promote1.json" || { echo "!! promote 响应缺 promoted:true: $(cat "$WORKDIR/promote1.json")" >&2; exit 1; }
echo "    promoted ✓ next_lsn=$(grep -o '"next_lsn":[0-9]*' "$WORKDIR/promote1.json" | cut -d: -f2)"
code=$(http_post "http://127.0.0.1:${F_HTTP}/v1/admin/promote" /dev/null "$WORKDIR/promote2.json")
[[ "$code" == "409" ]] || { echo "!! 二次 promote 期望 409 实得 $code" >&2; exit 1; }
echo "    二次 promote 409 ✓"

echo "==> [7/9] 新主可写（c_full 追加 1 条）"
gen_vectors 3100000 1 "$WORKDIR/extra.json"
code=$(http_post "http://127.0.0.1:${F_HTTP}/v1/collections/c_full/upsert" "$WORKDIR/extra.json" "$WORKDIR/r.json")
[[ "$code" == "200" ]] || { echo "!! 新主写入失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
echo "    新主 upsert 200 ✓"

echo "==> [8/9] RPO=0 验证：杀前 full_sync 集合全部可查（计数 + 抽样 top-1）"
COUNT=$(collection_count "$F_HTTP" c_full)
EXPECTED=$((VECTORS + 1))
[[ "$COUNT" == "$EXPECTED" ]] || { echo "!! c_full 计数=${COUNT} 期望 ${EXPECTED}（RPO>0!）" >&2; exit 1; }
echo "    c_full 计数=$COUNT ✓（杀前 $VECTORS + promote 后 1）"
python3 - "$VECTORS" "http://127.0.0.1:${F_HTTP}" <<'PY'
import json, sys, urllib.request
n, base_url = int(sys.argv[1]), sys.argv[2]
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
samples = [3_000_000, 3_000_000 + n // 2, 3_000_000 + n - 1]
for i in samples:
    v = [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1]
    req = urllib.request.Request(
        f"{base_url}/v1/collections/c_full/search",
        data=json.dumps({"vector": v, "k": 8}).encode(),
        headers={"Content-Type": "application/json"},
    )
    resp = json.loads(opener.open(req, timeout=10).read())
    ids, scores = resp["ids"], resp["scores"]
    assert ids and ids[0] == i and abs(scores[0] - 1.0) < 1e-5, \
        f"id {i} failover 后 top-1/score 不符（RPO>0）: ids={ids} scores={scores}"
    print(f"    id {i} 新主可查 ✓ (top-1, score={scores[0]:.6f})")
print("==> RPO=0 验证通过：full_sync 集合杀前写零丢失")
PY

echo "==> [9/9] 旧主清数据目录、以 follower 身份重并追平（防脑裂人工处置的可执行形态）"
rm -rf "$L_DATA"
mkdir -p "$L_DATA"
LEADER_PID=$(start_node leader2.log "$L_DATA" follower "$L_HTTP" "$L_PG" "$L_REPL" "127.0.0.1:${F_REPL}")
wait_counts "$L_HTTP" "$VECTORS" "$VECTORS" "$EXPECTED" "旧主重并追平"
echo "    旧主重并后计数 c_async/c_semi=${VECTORS} c_full=${EXPECTED} ✓"
# 重并后是 follower：写必须再次被 503 拦截。
# EN: After re-merge it is a follower: writes must be blocked by 503 again.
code=$(http_post "http://127.0.0.1:${L_HTTP}/v1/collections/c_full/upsert" "$WORKDIR/extra.json" "$WORKDIR/r.json")
[[ "$code" == "503" ]] || { echo "!! 重并节点 upsert 期望 503 实得 $code" >&2; exit 1; }
echo "    重并节点只读 ✓"

echo "==> 演练通过：主备同步 → 三级 HA → kill -9 → promote → RPO=0 → 旧主重并"
