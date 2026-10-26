#!/usr/bin/env bash
# HouyiDB HTTP 自检：健康、WAL 状态、**POST** 触发向量快照（并打印 JSON 摘要）。
# HouyiDB HTTP smoke: health, WAL status, POST vector snapshot (JSON summary).
#
# 为何 `curl http://.../v1/wal/snapshot` 看不到东西？
# - 该路由仅允许 **POST**；默认 curl 使用 **GET** → 405，且响应体常为空，看起来像没输出。
# - 正确用法见下方 `snapshot_post` 或本脚本 `snapshot` 子命令。
#
# 用法 / Usage:
#   export HOUYIDB_HTTP=http://127.0.0.1:17000   # 与 --benchmark-http-addr 一致
#   ./scripts/houyidb_http_ops.sh all
#   ./scripts/houyidb_http_ops.sh health
#   ./scripts/houyidb_http_ops.sh wal-status
#   ./scripts/houyidb_http_ops.sh snapshot
#
# 快照文件位置（需与启动时 --wal-path 对应）：
# - `foo.wal` → 同目录 `foo.vector_snapshot.json`（POST snapshot 成功后写入）

set -euo pipefail

HOUYIDB_HTTP="${HOUYIDB_HTTP:-http://127.0.0.1:17000}"
BASE="${HOUYIDB_HTTP%/}"

json_pretty() {
  if command -v jq >/dev/null 2>&1; then
    jq .
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin), indent=2, ensure_ascii=False))'
  else
    cat
  fi
}

health() {
  echo "==> GET ${BASE}/v1/health"
  curl -sS --noproxy '*' "${BASE}/v1/health" | json_pretty
}

wal_status() {
  echo "==> GET ${BASE}/v1/wal/status"
  curl -sS --noproxy '*' "${BASE}/v1/wal/status" | json_pretty
}

# 必须使用 POST；无请求体即可。
# EN: Must use POST; an empty request body is enough.
snapshot() {
  echo "==> POST ${BASE}/v1/wal/snapshot"
  local tmp out code
  tmp="$(mktemp)"
  code="$(curl -sS --noproxy '*' -o "$tmp" -w '%{http_code}' -X POST "${BASE}/v1/wal/snapshot")"
  json_pretty <"$tmp"
  rm -f "$tmp"
  echo "(http status: ${code})" >&2
  if [[ "$code" != "200" ]]; then
    echo "若见 405：说明用了 GET，请改用 -X POST。" >&2
    return 1
  fi
}

optimize() {
  echo "==> POST ${BASE}/v1/optimize"
  curl -sS --noproxy '*' -X POST \
    -H 'Content-Type: application/json' \
    -d '{}' \
    "${BASE}/v1/optimize" | json_pretty
}

all() {
  health
  echo ""
  wal_status
  echo ""
  optimize
  echo ""
  snapshot
}

case "${1:-all}" in
  health) health ;;
  wal-status|wal_status) wal_status ;;
  snapshot) snapshot ;;
  optimize) optimize ;;
  all) all ;;
  -h|--help)
    sed -n '1,35p' "$0"
    ;;
  *)
    echo "unknown subcommand: $1 (use: health | wal-status | snapshot | optimize | all)" >&2
    exit 1
    ;;
esac
