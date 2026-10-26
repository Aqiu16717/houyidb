#!/usr/bin/env bash
# HouyiDB 试用自检 / Trial smoke test
#
# 依次验证：健康检查 → Collection 自动创建 + upsert → 向量检索 →
# 结构化过滤混合检索 → 删除 → 状态。仅依赖 curl。
#
#   ./scripts/trial_smoke.sh
#   HOUYI_TRIAL_URL=http://127.0.0.1:17000 ./scripts/trial_smoke.sh
set -euo pipefail

BASE="${HOUYI_TRIAL_URL:-http://127.0.0.1:17000}"
COL="demo"
CURL=(curl -sS --noproxy '*')

expect_contains() { # $1=JSON $2=needle $3=label
  if [[ "$1" != *"$2"* ]]; then
    echo "!! $3 失败，响应：$1" >&2
    exit 1
  fi
  echo "ok  $3"
}

echo "== 等待服务就绪：$BASE"
for _ in $(seq 1 100); do
  if "${CURL[@]}" -f "$BASE/v1/health" >/dev/null 2>&1; then break; fi
  sleep 0.2
done
HEALTH="$("${CURL[@]}" -f "$BASE/v1/health")"
expect_contains "$HEALTH" '"ok"' "健康检查 /v1/health"

echo "== 写入 3 条 8 维向量（Collection 自动创建）"
UPSERT="$("${CURL[@]}" -f -X POST "$BASE/v1/collections/$COL/upsert" \
  -H 'Content-Type: application/json' -d '{
  "vectors": [
    {"id": 1, "values": [0.92,0.88,0.06,0.05,0.05,0.04,0.05,0.05], "payload": {"name": "apple", "kind": "fruit", "price": 3}},
    {"id": 2, "values": [0.05,0.04,0.92,0.90,0.06,0.05,0.05,0.04], "payload": {"name": "laptop", "kind": "electronics", "price": 999}},
    {"id": 3, "values": [0.04,0.05,0.05,0.06,0.90,0.92,0.05,0.04], "payload": {"name": "rust-book", "kind": "book", "price": 45}}
  ]
}')"
expect_contains "$UPSERT" '"inserted":3' "upsert 3 条"

echo "== 纯向量检索（应命中 1 apple）"
SEARCH="$("${CURL[@]}" -f -X POST "$BASE/v1/collections/$COL/search" \
  -H 'Content-Type: application/json' \
  -d '{"vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05], "k": 3, "include_payload": true}')"
expect_contains "$SEARCH" '"ids":[1' "top-1 命中 id=1"
expect_contains "$SEARCH" 'apple' "命中带回 payload"

echo "== 结构化过滤（price < 10，应只剩 id=1）"
HYBRID="$("${CURL[@]}" -f -X POST "$BASE/v1/collections/$COL/hybrid/search" \
  -H 'Content-Type: application/json' -d '{
  "vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05],
  "k": 3,
  "filters": [{"column": "price", "op": "lt", "value": 10}],
  "include_payload": true
}')"
expect_contains "$HYBRID" '"ids":[1]' "过滤后只返回 id=1"

echo "== 删除 id=1"
DEL="$("${CURL[@]}" -f -X POST "$BASE/v1/collections/$COL/delete" \
  -H 'Content-Type: application/json' -d '{"ids": [1]}')"
expect_contains "$DEL" '"deleted":1' "删除 id=1"

echo "== 删除后过滤检索（应为空）"
AFTER="$("${CURL[@]}" -f -X POST "$BASE/v1/collections/$COL/hybrid/search" \
  -H 'Content-Type: application/json' -d '{
  "vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05],
  "k": 3,
  "filters": [{"column": "price", "op": "lt", "value": 10}]
}')"
expect_contains "$AFTER" '"ids":[]' "已删除 id 不可见"

echo "== Collection 状态"
STATUS="$("${CURL[@]}" -f "$BASE/v1/collections/$COL/status")"
expect_contains "$STATUS" '"dims":8' "维度自动识别为 8"

echo
echo "全部通过。可继续："
echo "  curl -s $BASE/v1/openapi.json | head -c 800     # API 概览"
echo "  curl -s $BASE/metrics | head -n 20              # Prometheus 指标"
