#!/usr/bin/env bash
# 备份恢复演练（P4.1.3）：热备份 WAL → 模拟灾难（清空数据目录）→ 恢复 → 验证数据完整。
# Backup/restore drill: hot-copy WAL artifacts, wipe the data dir, restore, verify.
#
# 演练的热备份语义 / Hot-backup semantics:
#   活动 WAL 是 append-only；运行中直接 cp 得到的副本等价于「某个时刻的崩溃现场」，
#   尾部可能是撕裂的半条记录——v2 格式（P4.1.1）的 CRC + 撕裂尾容忍保证重放安全。
#   因此最小备份单元 = *.wal + *.collections.wal + *.applied_lsn（+ 可选快照）。
#   EN: The active WAL is append-only, so a live `cp` is crash-consistent by
#   construction; the v2 format tolerates the possibly torn tail on replay.
#   Minimal backup unit = *.wal + *.collections.wal + *.applied_lsn.
#
# 用法 / Usage:
#   ./scripts/backup_restore_drill.sh                 # 全流程演练
#   HOUYI_BIN=target/release/houyi-server ./scripts/backup_restore_drill.sh
#
# 环境变量:
#   HOUYI_BIN        houyi-server 二进制；显式设置即契约——不存在/不可执行直接
#                    报错退出（静默回退 debug 会用错二进制演练，结果不可信）。
#                    缺省 target/debug/houyi-server，不存在则自动 cargo build
#   DRILL_VECTORS    载入向量数（默认 500，5 维；上限 1155 硬约束——验证判据
#                    依赖方向唯一：dim0 恒 1.0 使「正标量倍 ⇒ 恒等」（余弦下
#                    [1,1,1,1]∥[2,2,2,2] 的平局被构造性排除，OCR e38d097[7]），
#                    剩余分量周期 lcm(7,5,3,11)=1155，超界即同向量重复）
#   DRILL_KEEP=1     演练结束后保留临时目录（调试用）
#   DRILL_SELFTEST=1 只跑校验矩阵自测（负/正路径）后退出，不起服务

set -euo pipefail

# 锚定 repo root：脚本可从任意 cwd 调用；cargo 用 --manifest-path 显式定位，
# 相对路径（target/debug/...）一律以 repo root 为基准。
# EN: Anchor the repo root: the script can be invoked from any cwd; cargo is located
# EN: explicitly via --manifest-path, and relative paths (target/debug/...) always
# EN: resolve against the repo root.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

VECTORS="${DRILL_VECTORS:-500}"
HTTP_PORT=17881
PG_PORT=17882

# DRILL_VECTORS 校验（OCR c0efbd9[2] + 6968f48[1][3]）：
# - 非数字在 set -u/pipefail 下会炸成费解的 bash 算术错误，0/负数则带着
#   "空 upsert / 抽样断言误归因"走到后面才炸；
# - 前导零被 bash 算术当八进制：08/09 直接炸 "value too great for base"，
#   0010 静默当 8、走到 inserted 判等才以 "10 != 0010" 误归因——正则禁前导零；
# - 超 10 位触发 64 位算术溢出，且 if 条件把算术错误按 false 处理——两个
#   守卫（-lt 1 / -gt 1155）被静默绕过，超大值一路走到 python 内存爆炸。
#   位数上限 10（≤9999999999 < 2^63）保证后续算术比较永不溢出。
# 返回码即分类：0=合法；1=格式非法（非数字/前导零/超 10 位/0/负）；
# 2=超方向周期上限（OCR e38d097[2]：超 lcm(7,5,3,11)=1155 后向量整体重复，
# top-1 判据退化为引擎平局排序的掷硬币——数据完好也会误报失败）。
# 正则先行使算术永远只见 ≤10 位纯数字，溢出不可达。
# EN: DRILL_VECTORS validation (OCR c0efbd9[2] + 6968f48[1][3]):
# EN: - non-numeric values explode into confusing bash arithmetic errors under
# EN:   set -u/pipefail, while 0/negative values walk into "empty upsert /
# EN:   misattributed sampling assertion" failures later;
# EN: - leading zeros are octal to bash arithmetic: 08/09 fail with "value too great
# EN:   for base", and 0010 silently becomes 8, only to misattribute later as
# EN:   "10 != 0010" at the inserted equality check — the regex forbids leading zeros;
# EN: - more than 10 digits triggers 64-bit arithmetic overflow, and if conditions treat
# EN:   arithmetic errors as false — both guards (-lt 1 / -gt 1155) are silently
# EN:   bypassed, and huge values march into a Python memory blowup.
# EN: The 10-digit cap (<=9999999999 < 2^63) guarantees later comparisons never overflow.
# EN: Return codes are the classification: 0=valid; 1=invalid format (non-numeric /
# EN: leading zero / over 10 digits / 0 / negative); 2=above the direction-cycle limit
# EN: (OCR e38d097[2]: after lcm(7,5,3,11)=1155 the vectors repeat as a whole, and the
# EN: top-1 criterion degenerates into a coin flip on engine tie ordering — intact data
# EN: would fail). The regex runs first so arithmetic only ever sees <=10 pure digits;
# EN: overflow is unreachable.
validate_vectors() {
  local v="$1"
  [[ "$v" =~ ^(0|[1-9][0-9]{0,9})$ ]] || return 1
  (( v >= 1 )) || return 1
  (( v <= 1155 )) || return 2
  return 0
}

# 负路径矩阵自测（OCR 9e89839[2]）：校验编码了 bash 算术的微妙语义（八进制
# 解释 / 64 位溢出使比较静默为假 / if 条件吞算术错误），未来"简化"正则回
# ^[0-9]+$ 会静默重开溢出绕过且无任何东西能拦住——矩阵固化为可执行自测：
#   DRILL_SELFTEST=1 ./scripts/backup_restore_drill.sh
if [[ "${DRILL_SELFTEST:-0}" == "1" ]]; then
  st_fail=0; st_total=0
  # rc=1 组含空串/空白/符号/小数/十六进制形似（OCR 3f7871a[2]）：正则一旦
  # 被放宽，bash 算术会静默重新解释这几类（"+5"→5、" 5"→5），必须钉死。
  # EN: The rc=1 group includes empty/whitespace/sign/decimal/hex-like strings
  # EN: (OCR 3f7871a[2]): if the regex were loosened, bash arithmetic would silently
  # EN: reinterpret these ("+5"->5, " 5"->5), so it must stay nailed down.
  for v in 0 08 09 0010 99999999999999999999 12345678901 -5 abc "" " 5" "+5" "5.5" "0x10"; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    if [[ "$rc" -ne 1 ]]; then
      echo "!! 自测失败: '$v' 期望 rc=1 实得 $rc" >&2; st_fail=1
    fi
  done
  for v in 1156 9999999999; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    if [[ "$rc" -ne 2 ]]; then
      echo "!! 自测失败: '$v' 期望 rc=2 实得 $rc" >&2; st_fail=1
    fi
  done
  for v in 1 500 1155; do
    st_total=$((st_total + 1))
    rc=0; validate_vectors "$v" || rc=$?
    if [[ "$rc" -ne 0 ]]; then
      echo "!! 自测失败: '$v' 期望 rc=0 实得 $rc" >&2; st_fail=1
    fi
  done
  # 计数器而非硬编码总数（OCR 3f7871a[3]）：扩矩阵时总数自动同步。
  # EN: Counters rather than hard-coded totals (OCR 3f7871a[3]): totals stay in sync when
  # EN: the matrix grows.
  [[ "$st_fail" -eq 0 ]] && echo "==> 自测通过：${st_total} 例校验矩阵全部符合预期"
  exit "$st_fail"
fi

rc=0; validate_vectors "$VECTORS" || rc=$?
case "$rc" in
  0) ;;
  1)
    echo "!! DRILL_VECTORS='$VECTORS' 必须是正整数（无前导零，≤10 位）" >&2
    exit 1
    ;;
  2)
    echo "!! DRILL_VECTORS=$VECTORS 超过方向周期上限 1155，top-1 判据将失真" >&2
    exit 1
    ;;
  # 默认分支响亮失败（OCR 3f7871a[1]）：未来给 validate_vectors 加新返回
  # 分类时若漏改此处，静默 fallthrough 会让违例值溜进演练——正是本函数
  # 要消灭的静默绕过类，返回码契约必须自我强制。
  # EN: Default branch fails loudly (OCR 3f7871a[1]): if a new return class is added to
  # EN: validate_vectors and this spot is missed, a silent fallthrough would let violating
  # EN: values slip into the drill — exactly the silent-bypass class this function exists
  # EN: to eliminate; the return-code contract must enforce itself.
  *)
    echo "!! validate_vectors 返回未知分类 rc=${rc}（返回码契约被破坏）" >&2
    exit 1
    ;;
esac

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-drill.XXXXXX")
DATA="$WORKDIR/data"
BACKUP="$WORKDIR/backup"
mkdir -p "$DATA" "$BACKUP"
WAL="$DATA/drill.wal"

SERVER_PID=""
cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill -TERM "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  if [[ "${DRILL_KEEP:-0}" != "1" ]]; then
    rm -rf "$WORKDIR"
  else
    echo "==> DRILL_KEEP=1, 保留现场: $WORKDIR"
  fi
}
trap cleanup EXIT

# -f && -x（OCR e38d097[3]）：-x 对目录同样为真，目录会漏过契约检查，
# 推迟到 start_server 里炸成费解的 exec 错误。
# EN: -f && -x (OCR e38d097[3]): -x is also true for directories, so a directory would
# EN: slip past the contract check and only explode later as a confusing exec error in
# EN: start_server.
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

# 端口预检：health 有响应 = 残留服务占用——演练会打到错误实例，备份/验证
# 全部失真。指向既有清理脚本而非自动强杀（端口上的进程未必属于本演练）。
# HTTP/PG 双探（OCR e38d097[4]）：pgwire bind 失败在主进程是致命的，只探
# HTTP 会让「只占 PG 端口」的残留推迟到 start_server 的 15s 超时才爆炸。
# 两个探测都带时限（OCR e38d097[8]）：accept 了但不应答的 wedged 残留
# 不能反过来把演练挂死在预检。
# 只有 exit 7（connection refused）才算端口真空（OCR c0efbd9[1]）：curl 对
# 「拒绝」「accept 后死不应答（28 超时）」「非 health 端点（22）」同样非零
# 退出——后两种放过去，wedged 残留会漏进 start_server 才炸，正是 [4]/[8]
# 要消灭的延迟误归因。
# --noproxy '*'（OCR 6968f48[4]）：环境设了 http_proxy 且 no_proxy 未覆盖
# 127.0.0.1 时 curl 连的是代理——代理侧拒绝同样 exit 7，会把真实占用误判
# 真空；代理超时 28 又会误标 wedged。强制直连使退出码只反映目标端口。
# EN: Port preflight: a health response = a leftover service occupying the port — the
# EN: drill would hit the wrong instance and every backup/verification result would be
# EN: distorted. Point to the existing cleanup script instead of force-killing (the
# EN: process on the port may not belong to this drill).
# EN: Dual HTTP/PG probes (OCR e38d097[4]): a pgwire bind failure is fatal in the main
# EN: process; probing only HTTP lets a "PG-port-only" leftover survive until the 15s
# EN: timeout in start_server.
# EN: Both probes are time-limited (OCR e38d097[8]): a wedged leftover that accepts but
# EN: never answers must not hang the drill in preflight.
# EN: Only exit 7 (connection refused) counts as an empty port (OCR c0efbd9[1]): curl
# EN: also exits non-zero for "refused", "accepted then no answer (28 timeout)", and
# EN: "non-health endpoint (22)" — letting the latter two through means a wedged
# EN: leftover only explodes in start_server, exactly the delayed misattribution
# EN: [4]/[8] eliminate.
# EN: --noproxy '*' (OCR 6968f48[4]): when http_proxy is set and no_proxy does not
# EN: cover 127.0.0.1, curl goes through the proxy — a proxy-side refusal also exits 7
# EN: and would misread a real occupant as empty; a proxy timeout 28 would mislabel it
# EN: as wedged. Forcing a direct connection makes the exit code reflect only the
# EN: target port.
health_curl=0
curl -sf --max-time 3 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/v1/health" >/dev/null 2>&1 || health_curl=$?
if [[ "$health_curl" -eq 0 ]]; then
  echo "!! 127.0.0.1:${HTTP_PORT} 已有 HTTP 服务响应（残留 houyi-server?）" >&2
  echo "   先跑 HOUYI_HTTP_PORT=${HTTP_PORT} HOUYI_PG_PORT=${PG_PORT} ./scripts/vdb_houyidb_preflight.sh 清理再重试" >&2
  exit 1
elif [[ "$health_curl" -ne 7 ]]; then
  echo "!! 127.0.0.1:${HTTP_PORT} 有监听但 health 探测异常（curl exit=${health_curl}，wedged 残留?）" >&2
  echo "   先跑 HOUYI_HTTP_PORT=${HTTP_PORT} HOUYI_PG_PORT=${PG_PORT} ./scripts/vdb_houyidb_preflight.sh 清理再重试" >&2
  exit 1
fi
# preflight 的端口默认值与本演练不同（17000/15432 vs 17881/17882），
# 修复指引必须显式传参（OCR e38d097[5]），否则运维照做后重试撞同一错误。
# pgwire 无 HTTP 探针，用 TCP 连接占位检测（loopback 上无监听即刻拒绝）。
# EN: preflight's port defaults differ from this drill (17000/15432 vs 17881/17882), so
# EN: the fix hint must pass them explicitly (OCR e38d097[5]); otherwise operators retry
# EN: into the same error.
# EN: pgwire has no HTTP probe; use a TCP connect placeholder check (immediate refusal
# EN: when nothing listens on loopback).
if (exec 3<>"/dev/tcp/127.0.0.1/${PG_PORT}") 2>/dev/null; then
  echo "!! 127.0.0.1:${PG_PORT} 已有 TCP 监听（残留 houyi-server pgwire?）" >&2
  echo "   先跑 HOUYI_HTTP_PORT=${HTTP_PORT} HOUYI_PG_PORT=${PG_PORT} ./scripts/vdb_houyidb_preflight.sh 清理再重试" >&2
  exit 1
fi

start_server() {
  # 分 boot 命名日志：两次启动不互相覆盖，灾难前/恢复后现场都可查。
  # EN: Per-boot log names: the two starts do not overwrite each other, and both the
  # EN: pre-disaster and post-recovery scenes remain inspectable.
  local log="$1"
  "$BIN" \
    --benchmark-http-addr "127.0.0.1:${HTTP_PORT}" \
    --pgwire-addr "127.0.0.1:${PG_PORT}" \
    --wal-path "$WAL" \
    --vector-dim 5 \
    >"$WORKDIR/$log" 2>&1 &
  SERVER_PID=$!
  # 总时限而非次数（OCR 6968f48[2]）：--max-time 2 × 100 次最坏 ≈210s，
  # 与启动超时的运维预期漂移——deadline 模式让总预算成为显式契约。
  # 预算 15s = 10s 启动基线 + 恢复路径 WAL 重放余量（OCR 9e89839[3]）。
  # SECONDS 为 1s 粒度：最后一次探测可在 deadline-1 出发，最长再占
  # --max-time 2 + sleep 0.1，实际最坏 ≈17s——±2s 粒度容差是有意保留
  # （OCR 9e89839[4]），精确硬顶需再预留一个探测窗口，收益不抵复杂度。
  # 单次探测仍带时限（OCR c0efbd9[1]）：启动中的服务若 accept 后挂死，
  # 无界 curl 会把启动超时变成永久阻塞。--noproxy '*' 同预检（6968f48[4]）。
  # EN: Total deadline rather than a count (OCR 6968f48[2]): --max-time 2 x 100 worst
  # EN: case is about 210s, drifting from the operator's startup-timeout expectation —
  # EN: deadline mode makes the total budget an explicit contract.
  # EN: A 15s budget = 10s startup baseline + WAL replay headroom on the recovery path
  # EN: (OCR 9e89839[3]).
  # EN: SECONDS has 1s granularity: the last probe can start at deadline-1 and take at
  # EN: most --max-time 2 + sleep 0.1, so the real worst case is about 17s — the +/-2s
  # EN: granularity tolerance is deliberate (OCR 9e89839[4]); an exact hard cap needs
  # EN: another probe window, and the benefit does not justify the complexity.
  # EN: Each probe is still time-limited (OCR c0efbd9[1]): if a starting service accepts
  # EN: then hangs, an unbounded curl turns the startup timeout into a permanent block.
  # EN: --noproxy '*' as in preflight (6968f48[4]).
  local deadline=$((SECONDS + 15))
  while (( SECONDS < deadline )); do
    if curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/v1/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "!! 服务启动超时, 日志:" >&2
  tail -20 "$WORKDIR/$log" >&2
  return 1
}

echo "==> [1/6] 启动服务并载入 ${VECTORS} 条向量"
start_server server1.log

# 生成确定性向量（id → 5 维方向唯一）,一批 upsert。
# dim0 恒 1.0：余弦只认方向，[1,1,1,1] 与 [2,2,2,2] 是正标量倍、距离精确
# 为 0——全变化分量时 top-1 判据撞上平局（OCR e38d097[7]）。常数分量使
# k·v=w ⇒ k=1 ⇒ v=w，方向唯一性退化为向量唯一性，由周期上限 1155 保证。
# EN: Generate deterministic vectors (id -> 5-dim direction, unique), upserted as one
# EN: batch.
# EN: dim0 is always 1.0: cosine only cares about direction, and [1,1,1,1] and
# EN: [2,2,2,2] are positive scalar multiples with distance exactly 0 — with
# EN: all-varying components the top-1 criterion hits a tie (OCR e38d097[7]). A
# EN: constant component makes k*v=w => k=1 => v=w, so direction uniqueness reduces
# EN: to vector uniqueness, guaranteed by the 1155 cycle limit.
python3 - "$VECTORS" >"$WORKDIR/upsert.json" <<'PY'
import json, sys
n = int(sys.argv[1])
vecs = [
    {"id": i, "values": [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1], "label": f"batch-{i % 10}"}
    for i in range(1_000_000, 1_000_000 + n)
]
print(json.dumps({"vectors": vecs}))
PY
# inserted 必须等于请求数：静默少插时抽样验证照样可能过（只查 3 个 id），
# 确认数是唯一的全量信号。
# --max-time（OCR e38d097[6]）：演练唯一的大在途调用不能无界——服务 accept
# 后挂死时快速失败，与验证段 urlopen timeout=10 同一原则。
# --noproxy '*'（OCR 9e89839[1]）：预检/启动探测已强制直连，此处若走代理
# 会在 set -e 下以裸 curl 退出码死去、无归因——同一延迟误归因类。
# EN: inserted must equal the requested count: with silent under-insertion the sampling
# EN: check could still pass (it only queries 3 ids), so the confirmation count is the
# EN: only all-data signal.
# EN: --max-time (OCR e38d097[6]): the drill's only large in-flight call must not be
# EN: unbounded — fail fast when the service accepts then hangs, same principle as
# EN: timeout=10 in the verification section.
# EN: --noproxy '*' (OCR 9e89839[1]): preflight/startup probes already force direct
# EN: connections; going through a proxy here would die under set -e with a bare curl
# EN: exit code and no attribution — the same delayed-misattribution class.
RESP=$(curl -sf --max-time 60 --noproxy '*' -X POST "http://127.0.0.1:${HTTP_PORT}/v1/collections/drill/upsert" \
  -H 'Content-Type: application/json' \
  --data @"$WORKDIR/upsert.json")
# grep 无匹配时退出码 1，pipefail 会让 set -e 在给出下面的归因信息前
# 静默中止（OCR e38d097[1]）——`|| true` 把畸形响应留给显式判等分支报告。
# EN: grep exits 1 on no match, and pipefail makes set -e abort silently before
# EN: printing the attribution below (OCR e38d097[1]); `|| true` leaves the malformed
# EN: response to the explicit equality branch.
INSERTED=$(printf '%s' "$RESP" | grep -o '"inserted":[0-9]*' | cut -d: -f2 || true)
if [[ "$INSERTED" != "$VECTORS" ]]; then
  echo "!! upsert 确认数 inserted=$INSERTED != 请求数 $VECTORS" >&2
  exit 1
fi
echo "    upsert 确认 inserted=$INSERTED"

echo "==> [2/6] 热备份（服务运行中直接拷贝 WAL 家族文件）"
# 必需文件预检：缺了就不是「可选」而是演练前提不成立（服务未就绪/路径错），
# 静默 || true 会让空备份走到验证才爆炸，归因困难。
# EN: Required-file preflight: a missing file means the drill precondition is broken
# EN: (service not ready / wrong path), not that the file is optional; a silent
# EN: || true would only explode at verification with an empty backup and poor
# EN: attribution.
for f in drill.wal drill.collections.wal drill.applied_lsn; do
  [[ -f "$DATA/$f" ]] || { echo "!! 备份源缺失: ${DATA}/${f}（服务未就绪或路径错误）" >&2; exit 1; }
done
# 拷贝顺序即不变量：侧车/collections 先、主 WAL 最后。append-only 的 WAL 在
# 拷贝间隙只会增长，故 WAL 副本必然 ⊇ 侧车副本时刻的状态，保证
# applied_lsn ≤ WAL tail；顺序反过来则间隙内的新 append 会推进侧车越过
# 已拷 WAL 尾，恢复时越界。
# EN: Copy order is the invariant: sidecar/collections first, main WAL last. The
# EN: append-only WAL only grows during the copy gap, so the WAL copy necessarily
# EN: contains the sidecar state at copy time, guaranteeing applied_lsn <= WAL tail;
# EN: reversed order lets new appends in the gap push the sidecar past the copied WAL
# EN: tail, causing an out-of-bounds restore.
cp -p "$DATA"/drill.applied_lsn "$BACKUP/"
cp -p "$DATA"/drill.collections.wal "$BACKUP/"
cp -p "$DATA"/drill.wal.next_lsn "$BACKUP/" 2>/dev/null || true
# 快照为可选加速件（存在则省全量重放），不属最小备份单元。
# EN: Snapshots are an optional accelerator (skip full replay when present), not part
# EN: of the minimal backup unit.
cp -p "$DATA"/drill.vector_snapshot.json "$BACKUP/" 2>/dev/null || true
cp -p "$DATA"/drill.wal "$BACKUP/"
ls -la "$BACKUP"

echo "==> [3/6] 模拟灾难：SIGKILL + 清空数据目录"
kill -9 "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""
rm -rf "$DATA"
mkdir -p "$DATA"
[[ ! -e "$WAL" ]] && echo "    数据目录已清空"

echo "==> [4/6] 从备份恢复"
cp -p "$BACKUP"/drill.applied_lsn "$DATA/"
cp -p "$BACKUP"/drill.collections.wal "$DATA/"
cp -p "$BACKUP"/drill.wal.next_lsn "$DATA/" 2>/dev/null || true
cp -p "$BACKUP"/drill.vector_snapshot.json "$DATA/" 2>/dev/null || true
cp -p "$BACKUP"/drill.wal "$DATA/"

echo "==> [5/6] 重启服务（撕裂尾 + 重放路径）"
start_server server2.log

echo "==> [6/6] 验证：抽样检索已备份的向量"
python3 - "$VECTORS" "http://127.0.0.1:${HTTP_PORT}" <<'PY'
import json, sys, urllib.request
n, base = int(sys.argv[1]), sys.argv[2]
# 空 ProxyHandler（OCR 9e89839[1]）：urlopen 默认经 getproxies() 拾取
# http_proxy——与 curl 侧 --noproxy '*' 同一理由，验证流量必须直连，
# 否则代理故障会以超时/拒绝的形态误归因到恢复结果上。
# EN: Empty ProxyHandler (OCR 9e89839[1]): urlopen picks up http_proxy from
# EN: getproxies() by default — same reason as --noproxy '*' on the curl side:
# EN: verification traffic must be direct, otherwise a proxy failure would be
# EN: misattributed to the restore result as a timeout/refusal.
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
samples = [1_000_000, 1_000_000 + n // 2, 1_000_000 + n - 1]
for i in samples:
    v = [1.0, i % 7 + 1, i % 5 + 1, i % 3 + 1, i % 11 + 1]
    req = urllib.request.Request(
        f"{base}/v1/collections/drill/search",
        data=json.dumps({"vector": v, "k": 8}).encode(),
        headers={"Content-Type": "application/json"},
    )
    # timeout=10：服务挂死时快速失败，而非让演练永久阻塞。
    # EN: timeout=10: fail fast when the service hangs instead of blocking the drill
    # EN: forever.
    resp = json.loads(opener.open(req, timeout=10).read())
    ids, scores = resp["ids"], resp["scores"]
    # 严格判据：检索自身向量必须 top-1 且余弦分数≈1.0（「在 top-8 里」会
    # 放过邻居错位/分数异常这类恢复损伤）。dim0 恒 1.0 使方向唯一 ⟺ 向量
    # 唯一（排除正标量倍平局），周期 lcm(7,5,3,11)=1155 > 上限（bash 侧强
    # 制），集合内无同方向平局，top-1 判据稳定。
    # EN: Strict criterion: searching its own vector must be top-1 with cosine score
    # EN: close to 1.0 ("within top-8" would let neighbor misplacement / abnormal scores —
    # EN: restore damage — pass). dim0 always 1.0 makes direction unique <=> vector unique
    # EN: (excluding positive scalar-multiple ties); the cycle lcm(7,5,3,11)=1155 > cap
    # EN: (enforced on the bash side) means no same-direction ties within the set, so
    # EN: top-1 is stable.
    assert ids and ids[0] == i and abs(scores[0] - 1.0) < 1e-5, \
        f"id {i} 恢复后 top-1/score 不符: ids={ids} scores={scores}"
    print(f"    id {i} 恢复可检索 ✓ (top-1, score={scores[0]:.6f})")
print("==> 演练通过：热备份 → 灾难 → 恢复 → 数据完整")
PY
