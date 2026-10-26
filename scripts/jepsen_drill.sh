#!/usr/bin/env bash
# Jepsen 式历史线性化演练（P4.5.7，fallback 检查器）。
# Jepsen-style linearizability drill with the fallback checker.
#
# 流程 / Flow:
#   1. leader(A) + follower(B)，建 full_sync 集合 c_full 与 async 集合 c_async；
#   2. N 个并发客户端向 leader 写互不相交的 id（每个 id 带 payload row=id，
#      供读阶段做**单点精确探测**），客户端各自记录 invoke/ok 历史（JSONL）；
#   3. 写压中 kill -9 leader → 手动 promote follower；
#   4. 客户端改连新 leader，对自己写过的每个 id 做精确检索并记录读历史；
#   5. `lincheck --strict` 检查 c_full（线性化：已确认的写必须可读）；
#      `--eventual` 检查 c_async（允许 stale，只查幻读——async 在 failover 时
#      允许丢失未复制的写，这是三级 HA 的语义边界，不是缺陷）。
#
# 为什么用客户端历史而非服务端 op-log（WHY）：线性化定义在**客户端观测**上
# （Jepsen 的 history 同样是客户端侧记录）；客户端侧还自然包含超时/失败这类
# 「未知」事件。服务端 op-log 需要给请求加字段，收益不抵改动面。
# EN: linearizability is defined over client-observed histories; client-side
# logging also captures timeouts as "info" events. A server-side op-log would
# require request-schema changes for no additional checking power.
#
# 用法 / Usage:
#   ./scripts/jepsen_drill.sh
#   JENKINS_CLIENTS=3 JENKINS_IDS=80 JENKINS_SEED=42 ./scripts/jepsen_drill.sh
#   JENKINS_KEEP=1 ./scripts/jepsen_drill.sh     # 保留现场（历史文件可复查）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
cd "$ROOT"

CLIENTS="${JENKINS_CLIENTS:-2}"
IDS="${JENKINS_IDS:-60}"
SEED="${JENKINS_SEED:-$(date +%s)}"
KEEP="${JENKINS_KEEP:-0}"
[[ "$CLIENTS" =~ ^[1-9][0-9]*$ ]] || { echo "!! JENKINS_CLIENTS 必须是正整数" >&2; exit 2; }
[[ "$IDS" =~ ^[1-9][0-9]*$ ]] || { echo "!! JENKINS_IDS 必须是正整数" >&2; exit 2; }
[[ "$SEED" =~ ^[0-9]+$ ]] || { echo "!! JENKINS_SEED 必须是非负整数（当前：$SEED）" >&2; exit 2; }

A_HTTP=17901; A_PG=17902; A_REPL=17903
B_HTTP=17904; B_PG=17905; B_REPL=17906

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-jepsen.XXXXXX")
L_DATA="$WORKDIR/leader"; F_DATA="$WORKDIR/follower"
mkdir -p "$L_DATA" "$F_DATA"
LEADER_PID=""; FOLLOWER_PID=""
cleanup() {
  for pid in "$LEADER_PID" "$FOLLOWER_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  if [[ "$KEEP" != "1" ]]; then rm -rf "$WORKDIR"; else echo "==> JENKINS_KEEP=1, 现场: $WORKDIR"; fi
}
trap cleanup EXIT

if [[ -n "${HOUYI_BIN:-}" ]]; then
  BIN="$HOUYI_BIN"
else
  BIN="target/debug/houyi-server"
  [[ -x "$BIN" ]] || { echo "==> 构建 houyi-server"; cargo build -p houyi-server >/dev/null; }
fi

# 端口预检。
# EN: Port preflight.
for hp in "$A_HTTP" "$B_HTTP"; do
  c=0; curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${hp}/v1/health" >/dev/null 2>&1 || c=$?
  [[ "$c" -eq 7 ]] || { echo "!! 127.0.0.1:${hp} 被占用（curl exit=${c}）" >&2; exit 1; }
done

start_node() { # $1=log $2=data $3=role $4=http $5=pg $6=repl [$7=leader_addr]
  local log="$1" data="$2" role="$3" http="$4" pg="$5" repl="$6" leader="${7:-}"
  local extra=()
  [[ "$role" == "follower" ]] && extra+=(--replication-leader-addr "$leader")
  "$BIN" --benchmark-http-addr "127.0.0.1:${http}" --pgwire-addr "127.0.0.1:${pg}" \
    --wal-path "$data/jepsen.wal" --vector-dim 5 --wal-write-mode async \
    --role "$role" --replication-listen-addr "127.0.0.1:${repl}" \
    ${extra[@]+"${extra[@]}"} >"$WORKDIR/$log" 2>&1 &
  local pid=$!
  local deadline=$((SECONDS + 30))
  while (( SECONDS < deadline )); do
    curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${http}/v1/health" >/dev/null 2>&1 \
      && { echo "$pid"; return 0; }
    kill -0 "$pid" 2>/dev/null || { tail -20 "$WORKDIR/$log" >&2; return 1; }
    sleep 0.1
  done
  echo "!! 节点启动超时（$log）" >&2
  # 半起进程必须回收：固定端口上的孤儿会让下一次运行预检失败。
  # EN: Half-started processes must be reaped: an orphan on a fixed port makes the next
  # EN: run's preflight fail.
  kill -9 "$pid" 2>/dev/null || true
  return 1
}

metric_followers() { # $1=port → followers_active 或空
  curl -sf --max-time 5 --noproxy '*' "http://127.0.0.1:$1/metrics" 2>/dev/null \
    | awk '$1 == "houyidb_replication_followers_active" {print $2; exit}'
}

post_json() { # $1=url $2=file $3=out → code
  curl -s --max-time 60 --noproxy '*' -o "$3" -w "%{http_code}" -X POST \
    -H 'Content-Type: application/json' --data @"$2" "$1"
}

echo "==> [1/5] 启动 leader(:${A_HTTP}) 与 follower(:${B_HTTP})"
LEADER_PID=$(start_node leader.log "$L_DATA" leader "$A_HTTP" "$A_PG" "$A_REPL")
FOLLOWER_PID=$(start_node follower.log "$F_DATA" follower "$B_HTTP" "$B_PG" "$B_REPL" "127.0.0.1:${A_REPL}")
echo "    leader pid=$LEADER_PID follower pid=$FOLLOWER_PID"

echo "==> [2/5] 建集合并设级别（c_full=full_sync，c_async=async）"
setup_one() { # $1=name $2=level
  local body="$WORKDIR/$1-probe.json"
  echo "{\"vectors\":[{\"id\":1,\"values\":[1.0,1,1,1,1],\"payload\":{\"row\":1}}]}" >"$body"
  local code
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/$1/upsert" "$body" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $1 探针失败 $code: $(cat "$WORKDIR/r.json")" >&2; exit 1; }
  echo "{\"ha_level\":\"$2\"}" >"$WORKDIR/ha.json"
  code=$(post_json "http://127.0.0.1:${A_HTTP}/v1/collections/$1/ha_level" "$WORKDIR/ha.json" "$WORKDIR/r.json")
  [[ "$code" == "200" ]] || { echo "!! $1 设级别失败 $code" >&2; exit 1; }
}
setup_one c_full full_sync
setup_one c_async async

# 每个客户端写互不相交的 id 区间：client k → [base_k, base_k + IDS)。
# EN: Each client writes a disjoint ID range: client k -> [base_k, base_k + IDS).
client_base() { echo $(( 1000000 * ($1 + 1) )); }

cat >"$WORKDIR/client.py" <<'PY'
import json, sys, time, urllib.request, urllib.error

base_url, collection, start, count, hist_path = sys.argv[1:6]
start, count = int(start), int(count)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def log(rec):
    with open(hist_path, "a") as f:
        f.write(json.dumps(rec) + "\n")
        f.flush()

def now_ns():
    return time.time_ns()

def post(path, body):
    req = urllib.request.Request(base_url + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with opener.open(req, timeout=15) as resp:
        return json.loads(resp.read())

for i in range(count):
    rid = start + i
    t0 = now_ns()
    try:
        post(f"/v1/collections/{collection}/upsert",
             {"vectors": [{"id": rid, "values": [1.0, rid % 7 + 1, rid % 5 + 1, rid % 3 + 1, rid % 11 + 1],
                           "payload": {"row": rid}}]})
        status = "ok"
    except urllib.error.HTTPError as e:
        # 5xx：服务端可能**已应用**后才失败（如 apply 后锁毒化返 500）→ 结果
        # 不确定；4xx：请求被确定拒绝，未产生副作用。
        # EN: 5xx is indeterminate (the write may have been applied); 4xx is a
        # deterministic rejection with no side effects.
        status = "info" if e.code >= 500 else "fail"
    except Exception:
        status = "info"   # 连接中断/超时：结果不确定（可能已生效）
    log({"op": "upsert", "collection": collection, "ids": [rid],
         "invoke_ns": t0, "ok_ns": now_ns(), "status": status})
    if status != "ok":
        break
PY

cat >"$WORKDIR/reader.py" <<'PY'
import json, sys, time, urllib.request, urllib.error

base_url, collection, start, count, hist_path = sys.argv[1:6]
start, count = int(start), int(count)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def now_ns():
    return time.time_ns()

def log(rec):
    with open(hist_path, "a") as f:
        f.write(json.dumps(rec) + "\n")
        f.flush()

for i in range(count):
    rid = start + i
    t0 = now_ns()
    try:
        req = urllib.request.Request(
            base_url + f"/v1/collections/{collection}/hybrid/search",
            data=json.dumps({"vector": [1.0, rid % 7 + 1, rid % 5 + 1, rid % 3 + 1, rid % 11 + 1],
                             "k": 1,
                             "filters": [{"column": "row", "op": "eq", "value": rid}]}).encode(),
            headers={"Content-Type": "application/json"})
        with opener.open(req, timeout=15) as resp:
            found = json.loads(resp.read()).get("ids", [])
        status = "ok"
    except urllib.error.HTTPError as e:
        # 与写侧同口径：5xx 结果不确定（服务端可能已处理后才失败）。
        # EN: Same convention as the write side: 5xx results are indeterminate (the server
        # EN: may have applied the operation before failing).
        found, status = [], ("info" if e.code >= 500 else "fail")
    except Exception:
        found, status = [], "info"
    log({"op": "search", "collection": collection, "probe": [rid], "found": found,
         "invoke_ns": t0, "ok_ns": now_ns(), "status": status})
PY

# 等 follower 会话真正注册：health 就绪 ≠ 复制会话建立。full_sync 在
# NoFollower 时把等待降级为 async 并照常 ACK——若最早的写落在这个窗口，
# failover 后必然读不到，会被 lincheck 记成产品缺陷（实为编排假阳性）。
# EN: a ready health endpoint does not mean the replication session exists;
# full_sync degrades to async when no follower is registered.
deadline=$((SECONDS + 20))
until [[ "$(metric_followers "$A_HTTP")" =~ ^[1-9] ]]; do
  (( SECONDS < deadline )) || {
    echo "!! follower 会话 20s 内未注册（leader followers_active=$(metric_followers "$A_HTTP")）" >&2
    exit 1
  }
  sleep 0.1
done

echo "==> [3/5] 并发写压（${CLIENTS} 客户端 × ${IDS} id/集合）后 kill -9 leader"
WRITERS=()
for ((k = 0; k < CLIENTS; k++)); do
  base=$(client_base "$k")
  for col in c_full c_async; do
    python3 "$WORKDIR/client.py" "http://127.0.0.1:${A_HTTP}" "$col" "$base" "$IDS" \
      "$WORKDIR/hist-${col}-${k}.jsonl" &
    WRITERS+=($!)
  done
done
# 种子化 kill 窗口：让部分客户端停在半途（未完成的写即「未知」事件）。
# kill 窗口必须落在写压中途：默认 60 id × ~20ms/写 ≈ 1.2s；0.10~0.25s
# 保证客户端还在写。EN: keep the kill inside the write window.
sleep "$(python3 -c "print(0.10 + ($SEED % 15) / 100.0)")"
kill -9 "$LEADER_PID" 2>/dev/null || true
wait "$LEADER_PID" 2>/dev/null || true
LEADER_PID=""
for w in "${WRITERS[@]}"; do wait "$w" 2>/dev/null || true; done
echo "    leader 已 SIGKILL，写客户端退出"

echo "==> [4/5] promote follower 并等待就绪"
code=$(post_json "http://127.0.0.1:${B_HTTP}/v1/admin/promote" /dev/null "$WORKDIR/promote.json")
[[ "$code" == "200" ]] || { echo "!! promote 失败 $code: $(cat "$WORKDIR/promote.json")" >&2; exit 1; }

echo "==> [5/5] 从新 leader 精确复读并做线性化检查"
for ((k = 0; k < CLIENTS; k++)); do
  base=$(client_base "$k")
  for col in c_full c_async; do
    python3 "$WORKDIR/reader.py" "http://127.0.0.1:${B_HTTP}" "$col" "$base" "$IDS" \
      "$WORKDIR/hist-${col}-${k}.jsonl"
  done
done

# 每次都增量构建：`cargo test` 产出的是 deps/ 下的哈希二进制，standalone
# 路径不会随源码更新——只判存在会静默跑旧检查器。
# EN: always rebuild; `cargo test` writes a hashed binary, so an existence
# check would silently run a stale checker.
cargo build -p houyi-model --bin lincheck >/dev/null
# 历史有效性门：kill 必须落在写压中途（≥1 个 ok 与 ≥1 个 info 写），否则
# 这次「通过」是空转（写全部完成或一条都没发出）。
# EN: the kill must land mid-write; otherwise the pass is vacuous.
python3 - "$WORKDIR" <<'PYGATE'
import glob, json, sys
work = sys.argv[1]
ok = info = 0
for path in glob.glob(f"{work}/hist-c_full-*.jsonl"):
    for line in open(path):
        rec = json.loads(line)
        if rec["op"] == "upsert":
            ok += rec["status"] == "ok"
            info += rec["status"] == "info"
assert ok >= 1, "kill window too late/early: no acknowledged c_full write"
assert info >= 1, "kill window missed the write phase: no indeterminate c_full write"
reads = 0
for path in glob.glob(f"{work}/hist-c_full-*.jsonl"):
    for line in open(path):
        rec = json.loads(line)
        if rec["op"] == "search" and rec["status"] == "ok":
            reads += 1
assert reads >= 1, "read phase produced no observations (would pass vacuously)"
print(f"    历史构成门: ok={ok} info={info} reads={reads} ✓")
PYGATE

LINCHECK="target/debug/lincheck"

# full_sync：已确认的写必须在 failover 后可读（线性化）。
# EN: full_sync: acknowledged writes must be readable after failover
# EN: (linearizability).
"$LINCHECK" --strict --collection c_full "$WORKDIR"/hist-c_full-*.jsonl
# async：允许 stale（failover 可丢未复制的写），只查幻读。
# EN: async: staleness is allowed (failover may lose unreplicated writes); only check
# EN: for phantom reads.
"$LINCHECK" --eventual --collection c_async "$WORKDIR"/hist-c_async-*.jsonl

echo "==> 演练通过：full_sync 线性化 + async 无幻读（seed=${SEED}）"
