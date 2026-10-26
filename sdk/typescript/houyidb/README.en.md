# houyidb (TypeScript/JavaScript)

> [中文](README.md)

The official TS/JS client for the HouyiDB vector database. Node ≥ 18 (global fetch), with a single runtime dependency (`@noble/hashes`, used for the cross-language-consistent `stableId`).

```bash
npm install houyidb
```

## Quick Start

```ts
import { HouyiClient, stableId } from "houyidb";

const client = new HouyiClient("http://127.0.0.1:7000");
const col = client.collection("docs");

// Write (the collection is created implicitly on first write; dimensionality is fixed by the first vector)
await col.upsert([
  { id: stableId("doc-1"), values: embedding, payload: { text: "...", category: "fruit" } },
]);

// Search (score is similarity; higher means more similar)
const result = await col.search(queryEmbedding, 5, { includePayload: true });

// Hybrid search (filters are intersected; op: eq/gt/gte/lt/lte/in)
const filtered = await col.hybridSearch(queryEmbedding, 5, {
  filters: [{ column: "category", op: "eq", value: "fruit" }],
  includePayload: true,
});

// Delete (by id and/or filter conditions, idempotent)
await col.delete({ ids: [stableId("doc-1")] });
await col.delete({ filters: [{ column: "category", op: "eq", value: "fruit" }] });
```

## Key Design: Vector IDs Are bigint

HouyiDB's id keyspace is u64, beyond the JS Number safe range (2^53-1). This SDK:

- `SearchHit.id` is always `bigint`; `stableId()` returns `bigint`;
- the write side accepts `bigint | number | string` and normalizes it;
- request serialization and response parsing protect against 2^53 corruption internally—**do not** `JSON.parse` search responses yourself to compare ids; use the `SearchResult` returned by the SDK.

`stableId` uses the same algorithm as the Python SDK and the server OpenAI gateway (BLAKE2b-8 big-endian & 2^63-1): the same string id yields the same u64 in any language, so deletes can be replayed across SDKs.

## API Mapping with the Python SDK

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

## Tests

```bash
npm test          # 单元（stable_id 三方对拍）+ 集成（打 127.0.0.1:17000 真实服务，HOUYIDB_URL 可覆盖）
npm run build     # 产出 dist/（ESM + d.ts）
```

Integration prerequisite: `./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
