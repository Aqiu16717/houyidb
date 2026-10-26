#!/usr/bin/env bash
# 向量化网关自检 / Embedding gateway smoke test
#
# 前置：HouyiDB 启动时配置了 OpenAI 兼容上游，例如：
#   HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:11434 ./scripts/trial_start.sh   # Ollama
#   HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:18100 ./scripts/trial_start.sh   # mock（仅链路）
#
# 用法：
#   ./scripts/embed_smoke.sh
#   HOUYI_TRIAL_URL=http://127.0.0.1:17000 EMBED_MODEL=nomic-embed-text ./scripts/embed_smoke.sh
#
# 验证内容：健康检查 → 自动入库（collection 扩展字段）→ 代理模式取查询向量
# → 带 payload 的检索。语义效果取决于上游模型；mock 仅保证链路通。
set -euo pipefail

BASE="${HOUYI_TRIAL_URL:-http://127.0.0.1:17000}"
MODEL="${EMBED_MODEL:-nomic-embed-text}"
COL="${EMBED_SMOKE_COLLECTION:-embed_smoke}"
CURL=(curl -sS --noproxy '*')

fail() { echo "!! $*" >&2; exit 1; }
ok() { echo "ok  $*"; }

echo "== 健康检查 $BASE"
for _ in $(seq 1 100); do
  "${CURL[@]}" -f "$BASE/v1/health" >/dev/null 2>&1 && break
  sleep 0.2
done
"${CURL[@]}" -f "$BASE/v1/health" >/dev/null || fail "服务不可达：$BASE"
ok "服务在线"

echo "== 自动入库（model=$MODEL, collection=${COL}）"
UPSERT="$("${CURL[@]}" -X POST "$BASE/v1/embeddings" \
  -H 'Content-Type: application/json' -d "{
  \"model\": \"$MODEL\",
  \"input\": [\"苹果是一种水果\", \"香蕉是黄色的\", \"HouyiDB 是向量数据库\"],
  \"collection\": \"$COL\"
}")"
echo "$UPSERT" | grep -q '"upserted"' || {
  echo "$UPSERT" | head -c 800 >&2
  fail "自动入库失败（上游未配置时返回 501；检查 HOUYI_OPENAI_UPSTREAM）"
}
UPSERTED="$(python3 -c '
import json, sys
try:
    print(json.loads(sys.argv[1])["houyi"]["upserted"])
except Exception as exc:
    sys.exit(f"bad upsert response: {exc}")
' "$UPSERT")" || { echo "$UPSERT" | head -c 800 >&2; fail "自动入库响应解析失败"; }
ok "自动入库 $UPSERTED 条"

echo "== 代理模式取查询向量"
EMBED="$("${CURL[@]}" -X POST "$BASE/v1/embeddings" \
  -H 'Content-Type: application/json' \
  -d "{\"model\": \"$MODEL\", \"input\": \"苹果是一种水果\"}")"
VECTOR="$(python3 -c '
import json, sys
try:
    print(json.dumps(json.loads(sys.argv[1])["data"][0]["embedding"]))
except Exception as exc:
    sys.exit(f"bad embedding response: {exc}")
' "$EMBED")" || { echo "$EMBED" | head -c 800 >&2; fail "代理模式 embedding 解析失败"; }
ok "查询向量维度 $(python3 -c 'import json,sys; print(len(json.loads(sys.argv[1])))' "$VECTOR")"

echo "== 向量检索（collection=${COL}）"
SEARCH="$("${CURL[@]}" -X POST "$BASE/v1/collections/$COL/search" \
  -H 'Content-Type: application/json' \
  -d "{\"vector\": $VECTOR, \"k\": 3, \"include_payload\": true}")"
echo "$SEARCH" | grep -q '"ids"' || { echo "$SEARCH" >&2; fail "检索失败"; }
COUNT="$(python3 -c '
import json, sys
try:
    print(len(json.loads(sys.argv[1])["ids"]))
except Exception as exc:
    sys.exit(f"bad search response: {exc}")
' "$SEARCH")" || { echo "$SEARCH" | head -c 800 >&2; fail "检索响应解析失败"; }
[[ "$COUNT" -ge 1 ]] || fail "检索结果为空"
ok "检索返回 $COUNT 条（含 payload）"

echo
echo "链路已通。注意：检索语义取决于上游模型；mock 只做链路自检。"
echo "真实模型示例：Ollama \`ollama pull nomic-embed-text\` / bge-m3；OpenAI text-embedding-3-small。"
