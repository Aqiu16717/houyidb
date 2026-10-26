#!/usr/bin/env bash
# HouyiDB 试用启动脚本 / Trial launcher
# EN: HouyiDB trial launcher.
#
# 默认端口刻意避开 macOS AirPlay（5000/7000）与常见本地 PostgreSQL（5432）：
# EN: Default ports deliberately avoid macOS AirPlay (5000/7000) and common local
# EN: PostgreSQL (5432):
#   HTTP    127.0.0.1:17000
#   pgwire  127.0.0.1:15432
# 数据落在 <bundle>/data（WAL-first，重启自动恢复）。
# EN: Data lives in <bundle>/data (WAL-first, automatic recovery on restart).
#
# 可覆盖的环境变量：
# EN: Overridable environment variables:
#   HOUYI_TRIAL_HTTP_ADDR    默认 127.0.0.1:17000
# EN: HOUYI_TRIAL_HTTP_ADDR    default 127.0.0.1:17000
#   HOUYI_TRIAL_PGWIRE_ADDR  默认 127.0.0.1:15432
# EN: HOUYI_TRIAL_PGWIRE_ADDR  default 127.0.0.1:15432
#   HOUYI_TRIAL_DATA_DIR     默认 <bundle>/data
# EN: HOUYI_TRIAL_DATA_DIR     default <bundle>/data
#   HOUYI_TRIAL_DIM          默认 768（仅影响 /v1/vectors 与 pgwire 主索引；
#                            Collections API 按首次写入自动确定维度）
# EN: HOUYI_TRIAL_DIM          default 768 (affects only /v1/vectors and the
# EN:                          pgwire main index; the Collections API infers dims
# EN:                          from the first write)
#   HOUYI_SERVER_BIN         显式指定二进制路径（默认自动探测）
# EN: HOUYI_SERVER_BIN         explicit binary path (auto-detected by default)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BIN="${HOUYI_SERVER_BIN:-}"
if [[ -z "$BIN" ]]; then
  if [[ -x "$ROOT/bin/houyi-server" ]]; then
    BIN="$ROOT/bin/houyi-server"
  elif [[ -x "$ROOT/target/release/houyi-server" ]]; then
    BIN="$ROOT/target/release/houyi-server"
  else
    echo "!! 找不到 houyi-server：请先 cargo build --release -p houyi-server，" >&2
    echo "   或设置 HOUYI_SERVER_BIN=/path/to/houyi-server" >&2
    exit 1
  fi
fi

HTTP_ADDR="${HOUYI_TRIAL_HTTP_ADDR:-127.0.0.1:17000}"
PG_ADDR="${HOUYI_TRIAL_PGWIRE_ADDR:-127.0.0.1:15432}"
DATA_DIR="${HOUYI_TRIAL_DATA_DIR:-$ROOT/data}"
DIM="${HOUYI_TRIAL_DIM:-768}"

mkdir -p "$DATA_DIR"
PG_HOST="${PG_ADDR%:*}"
PG_HOST="${PG_HOST#\[}"
PG_HOST="${PG_HOST%\]}"
PG_PORT="${PG_ADDR##*:}"
[[ "$PG_PORT" =~ ^[0-9]+$ ]] || PG_PORT=""

echo "HouyiDB trial"
echo "  binary : $BIN"
echo "  HTTP   : http://${HTTP_ADDR}    (健康检查 /v1/health, OpenAPI /v1/openapi.json)"
if [[ -n "$PG_PORT" ]]; then
  echo "  pgwire : ${PG_ADDR}            (psql -h ${PG_HOST} -p ${PG_PORT} -U any -d any)"
else
  echo "  pgwire : ${PG_ADDR}            (地址缺少端口，请检查 HOUYI_TRIAL_PGWIRE_ADDR)"
fi
echo "  WAL    : ${DATA_DIR}/houyidb.wal"
echo "  Ctrl-C 停止（SIGTERM 会优雅关停；数据已 WAL-first 落盘）"
echo

exec "$BIN" \
  --benchmark-http-addr "$HTTP_ADDR" \
  --pgwire-addr "$PG_ADDR" \
  --wal-path "${DATA_DIR}/houyidb.wal" \
  --vector-dim "$DIM" \
  "$@"
