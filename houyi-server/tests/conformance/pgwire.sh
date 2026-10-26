#!/usr/bin/env bash
# pgwire 外部客户端一致性验证（P4.8 / M4）。
# 用**真实外部客户端**（psql / psycopg2 / pgbench）驱动生产 wire 实现，而不是
# 仓内自写编解码器——后者的通过不能证明 libpq 系客户端可用。
# EN: drive the server with real external clients (psql/psycopg2/pgbench).
#
# 前置：psql、pgbench（PostgreSQL 客户端）、python3 + psycopg2。
# 用法：./houyi-server/tests/conformance/pgwire.sh
#       HOUYI_BIN=target/release/houyi-server ...   # 可选
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
cd "$ROOT"

HTTP_PORT=17951
PG_PORT=17952
BIN="${HOUYI_BIN:-target/debug/houyi-server}"
[[ -x "$BIN" ]] || { echo "==> 构建 houyi-server (debug)"; cargo build -p houyi-server >/dev/null; }
[[ -x "$BIN" ]] || { echo "!! 缺少二进制: $BIN" >&2; exit 1; }

for tool in psql pgbench python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "!! 缺少外部客户端: $tool" >&2; exit 1; }
done
python3 -c 'import psycopg2' 2>/dev/null || {
  echo "!! 缺少 python3 psycopg2（pip install psycopg2-binary）" >&2; exit 1
}

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/houyidb-pgwire-conformance.XXXXXX")
PID=""
cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
    kill -TERM "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

"$BIN" \
  --benchmark-http-addr "127.0.0.1:${HTTP_PORT}" \
  --pgwire-addr "127.0.0.1:${PG_PORT}" \
  --wal-path "$WORKDIR/conf.wal" \
  --vector-dim 4 \
  >"$WORKDIR/server.log" 2>&1 &
PID=$!
deadline=$((SECONDS + 30))
while (( SECONDS < deadline )); do
  if curl -sf --max-time 1 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/readyz" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
curl -sf --max-time 1 --noproxy '*' "http://127.0.0.1:${HTTP_PORT}/readyz" >/dev/null || {
  echo "!! 服务未就绪"; tail -30 "$WORKDIR/server.log" >&2; exit 1;
}

PASS=0
fail() { echo "!! FAIL: $*" >&2; echo "--- server log ---" >&2; tail -40 "$WORKDIR/server.log" >&2; exit 1; }

echo "==> [1/5] psql Simple Query（SSLRequest 'N' 降级 + SELECT literal）"
out=$(psql "host=127.0.0.1 port=${PG_PORT} user=test dbname=test sslmode=prefer" -Atqc "SELECT 1")
[[ "$out" == "1" ]] || fail "psql SELECT 1 => '$out'"
out=$(psql "host=127.0.0.1 port=${PG_PORT} user=test dbname=test sslmode=disable" -Atqc "SELECT 1,2,3")
[[ "$out" == "1|2|3" ]] || fail "psql SELECT 1,2,3 => '$out'"
PASS=$((PASS+1))

echo "==> [2/5] psql 错误路径：SQLSTATE + 连接可继续使用"
err=$(psql "host=127.0.0.1 port=${PG_PORT} user=test dbname=test sslmode=disable" \
  -v ON_ERROR_STOP=1 -v VERBOSITY=verbose -Atqc "SELECT" 2>&1) && fail "非法 SQL 不应成功"
# `SELECT`（缺表达式）走 unsupported 分支返回 0A000；语法类错误是 42601。
# 一致性要求是「标准 ErrorResponse + SQLSTATE」，具体码由实现语义决定。
# EN: `SELECT` (missing expression) takes the unsupported branch and returns 0A000;
# syntax errors return 42601. The conformance requirement is a standard ErrorResponse
# plus SQLSTATE; the exact code follows the implementation's semantics.
grep -Eq "42601|0A000" <<<"$err" || fail "错误响应未带 SQLSTATE: $err"
# 同一连接继续查询（psql 单命令模式会新连接；用单个 -c 脚本验证服务端未崩溃）
# EN: Continue querying on the same connection (psql's single-command mode opens a
# new connection; this single -c script verifies the server did not crash).
out=$(psql "host=127.0.0.1 port=${PG_PORT} user=test dbname=test sslmode=disable" -Atqc "SELECT 9")
[[ "$out" == "9" ]] || fail "错误后新连接不可用 => '$out'"
PASS=$((PASS+1))

echo "==> [3/5] psycopg2：libpq 客户端 + 扩展协议（参数查询）"
python3 - "$PG_PORT" <<'PY' || fail "psycopg2 断言失败"
import sys, psycopg2
port = int(sys.argv[1])
with psycopg2.connect(host="127.0.0.1", port=port, user="test", dbname="test", sslmode="disable") as conn:
    with conn.cursor() as cur:
        cur.execute("SELECT %s", (42,))
        assert cur.fetchone() == (42,), "parameterized SELECT"
        cur.execute("SELECT 1,2,3")
        assert cur.fetchone() == (1, 2, 3)
print("psycopg2 ok")
PY
PASS=$((PASS+1))

echo "==> [4/5] pgbench Simple Query 模式"
printf 'SELECT 1;\n' > "$WORKDIR/bench.sql"
pgbench -h 127.0.0.1 -p "$PG_PORT" -U test -d test -n -M simple -c 2 -j 1 -t 10 \
  -f "$WORKDIR/bench.sql" >"$WORKDIR/pgbench-simple.log" 2>&1 \
  || fail "pgbench -M simple 失败: $(tail -5 "$WORKDIR/pgbench-simple.log")"
PASS=$((PASS+1))

echo "==> [5/5] pgbench Extended Query（prepared）"
pgbench -h 127.0.0.1 -p "$PG_PORT" -U test -d test -n -M extended -c 2 -j 1 -t 10 \
  -f "$WORKDIR/bench.sql" >"$WORKDIR/pgbench-extended.log" 2>&1 \
  || fail "pgbench -M extended 失败: $(tail -5 "$WORKDIR/pgbench-extended.log")"
PASS=$((PASS+1))

echo "==> pgwire 外部一致性验证通过（${PASS}/5）"
