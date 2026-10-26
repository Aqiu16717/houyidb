#!/usr/bin/env bash
# Rust SDK 集成测试（P4.6.4）：临时服务 + 临时 WAL，自建自清，hermetic。
# Rust SDK integration gate (P4.6.4): throwaway server + temp WAL, self-cleaning.
#
# 为什么不再用常驻 17000（WHY）：旧的集成测试默认连 127.0.0.1:17000 的
# 「联调实例」——该端口上有没有服务、服务是哪个版本、WAL 里是否残留六天前
# 的墓碑，全都取决于机器历史。测试要么静默跳过（假绿），要么被陈旧状态
# 误伤。现在 `cargo test --all` 在未设置 HOUYIDB_URL 时确定性跳过；本脚本
# 给出唯一受控入口：每次全新 debug 构建 + 全新端口 + 全新 WAL。
# EN: the old tests pointed at a long-lived dev server on :17000, so results
# depended on machine history (silent skips or stale-WAL failures). The SDK
# tests now skip deterministically unless HOUYIDB_URL is set; this script is
# the controlled runner: fresh build, fresh ports, fresh WAL.
#
# 用法 / Usage:
#   ./scripts/sdk_integration.sh
#   HOUYI_BIN=target/release/houyi-server ./scripts/sdk_integration.sh
#
# 环境变量 / Env:
#   HOUYI_BIN      houyi-server 二进制；显式设置即契约（不回退）
#   SDK_KEEP=1     保留临时目录与服务日志（调试用）
#   SDK_TEST_ARGS  追加给 libtest 的参数（默认 "--test-threads=1"）

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$REPO_ROOT"

# 端口与 replication_drill（1789x）错开，避免并行执行互踩。
# EN: Ports are offset from replication_drill (1789x) to avoid clashes when run in
# EN: parallel.
HTTP_PORT=18010
PG_PORT=18011

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-sdk-it.XXXXXX")
SERVER_PID=""

cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [[ "${SDK_KEEP:-0}" != "1" ]]; then
    rm -rf "$WORKDIR"
  else
    echo "==> SDK_KEEP=1, 保留现场: $WORKDIR"
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
  echo "==> 构建 houyi-server (debug)"
  cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p houyi-server >/dev/null
fi

# 端口预检：HTTP 用 curl exit 7 语义；PG 用 /dev/tcp 占位检测。
# EN: Port preflight: HTTP uses curl exit 7 semantics; PG uses /dev/tcp placeholder
# EN: detection.
health_curl=0
curl -sf --max-time 3 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/v1/health" >/dev/null 2>&1 || health_curl=$?
if [[ "$health_curl" -ne 7 ]]; then
  echo "!! 127.0.0.1:${HTTP_PORT} 被占用或探测异常（curl exit=${health_curl}）" >&2
  exit 1
fi
if (exec 3<>"/dev/tcp/127.0.0.1/${PG_PORT}") 2>/dev/null; then
  echo "!! 127.0.0.1:${PG_PORT} 已有 TCP 监听（残留进程?）" >&2
  exit 1
fi

echo "==> 启动临时 houyi-server (HTTP :${HTTP_PORT}, PG :${PG_PORT})"
"$BIN" \
  --benchmark-http-addr "127.0.0.1:${HTTP_PORT}" \
  --pgwire-addr "127.0.0.1:${PG_PORT}" \
  --wal-path "$WORKDIR/houyi.wal" \
  --vector-dim 16 \
  --wal-write-mode async \
  >"$WORKDIR/server.log" 2>&1 &
SERVER_PID=$!

deadline=$((SECONDS + 20))
until curl -sf --max-time 2 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/v1/health" >/dev/null 2>&1; do
  if (( SECONDS >= deadline )); then
    echo "!! houyi-server 20s 内未就绪；日志尾部：" >&2
    tail -20 "$WORKDIR/server.log" >&2 || true
    exit 1
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "!! houyi-server 提前退出；日志尾部：" >&2
    tail -20 "$WORKDIR/server.log" >&2 || true
    exit 1
  fi
  sleep 0.1
done
echo "==> 服务就绪，运行 SDK 集成测试"

# --test-threads=1：共享服务上串行执行，避免异步构图队列互相拖慢造成
# wait_flushed 抖动；用例各自独占 collection 名，串行也保证日志可读。
# EN: --test-threads=1: run serially on the shared server to avoid the async build
# EN: queue slowing each other down and causing wait_flushed flakiness; each test owns
# EN: its collection name, and serial execution keeps the log readable.
HOUYIDB_URL="http://127.0.0.1:${HTTP_PORT}" \
  cargo test -p houyidb-client --test integration -- ${SDK_TEST_ARGS:---test-threads=1}

echo "==> SDK 集成测试通过（临时服务与 WAL 将随 trap 清理）"
