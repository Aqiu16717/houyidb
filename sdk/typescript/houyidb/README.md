# houyidb (TypeScript/JavaScript)

> [English](README.en.md)

HouyiDB 向量数据库的官方 TS/JS 客户端。Node ≥ 18（全局 fetch），运行时仅一个依赖（`@noble/hashes`，用于跨语言一致的 `stableId`）。

```bash
npm install houyidb
```

## 快速开始

```ts
import { HouyiClient, stableId } from "houyidb";

const client = new HouyiClient("http://127.0.0.1:7000");
const col = client.collection("docs");

// 写入（collection 首次写入隐式创建，维度由首个向量定型）
await col.upsert([
  { id: stableId("doc-1"), values: embedding, payload: { text: "...", category: "fruit" } },
]);

// 检索（score 为相似度，越大越相似）
const result = await col.search(queryEmbedding, 5, { includePayload: true });

// 混合检索（filters 取交集；op: eq/gt/gte/lt/lte/in）
const filtered = await col.hybridSearch(queryEmbedding, 5, {
  filters: [{ column: "category", op: "eq", value: "fruit" }],
  includePayload: true,
});

// 删除（按 id 和/或过滤条件，幂等）
await col.delete({ ids: [stableId("doc-1")] });
await col.delete({ filters: [{ column: "category", op: "eq", value: "fruit" }] });
```

## 关键设计：向量 id 是 bigint

HouyiDB 的 id 键空间是 u64，超出 JS Number 安全域（2^53-1）。本 SDK：

- `SearchHit.id` 一律为 `bigint`；`stableId()` 返回 `bigint`；
- 写入侧接受 `bigint | number | string` 并归一；
- 请求序列化与响应解析内部做 2^53 防腐蚀处理——**不要**自己 `JSON.parse` 检索响应再比较 id，请使用 SDK 返回的 `SearchResult`。

`stableId` 与 Python SDK、服务端 OpenAI 网关三方同算法（BLAKE2b-8 大端 & 2^63-1）：同一字符串 id 在任何语言得到同一 u64，删除可跨 SDK 重放。

## 与 Python SDK 的 API 对应

| Python | TypeScript |
|---|---|
| `collection.upsert(vectors)` | `col.upsert(vectors)` |
| `collection.search(v, k, ef_search=, include_payload=)` | `col.search(v, k, { efSearch, includePayload })` |
| `collection.hybrid_search(v, k, filters=, ...)` | `col.hybridSearch(v, k, { filters, ... })` |
| `collection.delete(ids=, filters=)` | `col.delete({ ids, filters })` |
| `collection.status()` | `col.status()` |
| `client.health() / list_collections() / index_status()` | `client.health() / listCollections() / indexStatus()` |
| `stable_id(text) -> int` | `stableId(text) -> bigint` |
| `HouyiError(status_code)` | `HouyiError(statusCode)` |

## 测试

```bash
npm test          # 单元（stable_id 三方对拍）+ 集成（打 127.0.0.1:17000 真实服务，HOUYIDB_URL 可覆盖）
npm run build     # 产出 dist/（ESM + d.ts）
```

集成前置：`./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
