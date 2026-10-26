# HouyiDB Python Ecosystem

> [中文](README.md)

Three packages, each with a distinct role:

| Package | Role | Dependencies |
|----|------|------|
| `houyidb` | collections HTTP API client (transport layer) | `requests` |
| `langchain-houyidb` | LangChain `VectorStore` adapter (semantic dialect layer) | `langchain-core` + `houyidb` |
| `llama-index-vector-stores-houyidb` | LlamaIndex `BasePydanticVectorStore` adapter | `llama-index-core` + `houyidb` |

The SDK owns the wire protocol (endpoints, JSON, error mapping, deterministic str→u64 id hashing);
the adapters own framework semantics (Document/node models, embedding delegation, filter DSL translation,
score conventions, lifecycle contracts). Embeddings are always computed by the caller/framework; HouyiDB only stores and retrieves vectors.

## Installation (Development)

```bash
pip install -e sdk/python/houyidb \
            -e sdk/python/langchain-houyidb \
            -e sdk/python/llama-index-vector-stores-houyidb
```

## Minimal Example

```python
from houyidb import HouyiClient, Vector, Filter

client = HouyiClient("http://127.0.0.1:7000")
col = client.collection("docs")  # 首次 upsert 隐式创建
col.upsert([Vector(id=1, values=emb, payload={"text": "apple", "price": 5})])

hits = col.search(query_emb, 3, include_payload=True)          # 纯向量
hits = col.hybrid_search(query_emb, 3,                         # 向量 × 过滤
                         filters=[Filter("price", "lt", 10)], include_payload=True)
col.delete([1])                                                # 按 id
col.delete(filters=[Filter("price", "lt", 10)])                # 按条件（见下）
```

LangChain / LlamaIndex usage is documented in each package's README.

## Write Visibility (Dual-Speed Writes)

Batch upserts first sit in the asynchronous graph-build queue (builder_queue) and become
visible to **search** only after a flush (typically tens of milliseconds in normal operation);
scalar rows, however, are written to their pages **synchronously** with the upsert, therefore:

- `delete(filters=...)` is evaluated against scalar pages, so freshly written vectors can be deleted by condition immediately—
  do not use the "search first for ids, then delete" pattern (search cannot see vectors still in the queue, so deletes will miss them);
- when determinism is required before asserting on search results, poll `col.status()["pending_count"] == 0`
  (test helper `sdk/python/tests/conftest.py::wait_flushed`).

## OpenAI-Compatible Gateway (Optional)

Started with `--openai-upstream <upstream base_url>`, the server exposes an OpenAI-shaped
`POST /v1/embeddings`: without `collection` it is a transparent proxy ("just change the base_url");
with `collection` (passed by the SDK via `extra_body`) it also ingests the embeddings—
"change the URL and data is ingested":

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:17000/v1", api_key="not-needed")
resp = client.embeddings.create(
    model="text-embedding-3-small",
    input=["apple is a fruit"],
    extra_body={"collection": "docs"},
)
resp.houyi  # {"collection": "docs", "upserted": 1} —— id = stable_id(text)，重跑幂等
```

## Tests

```bash
# Requires a server running the new binary (with delete/include_payload/OpenAI gateway)
cargo build --release -p houyi-server
.venv-eco/bin/python sdk/python/tests/fake_openai_upstream.py &   # 假 OpenAI 上游 :18090
./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 \
    --pgwire-addr 127.0.0.1:15432 --wal-path /tmp/houyidb-eco.wal \
    --openai-upstream http://127.0.0.1:18090

.venv-eco/bin/python -m pytest sdk/python/tests -v   # HOUYIDB_URL 可覆盖默认地址
```

## Common Known Limitations

- Collection payload scalar pages are volatile in v1: payload/filter/delete tombstones are lost after a process restart,
  while the vector index (WAL/snapshot) is unaffected.
- The payload/metadata key `label` is a reserved column.
- Filter operators are limited to `eq/gt/gte/lt/lte/in`, and multiple conditions are ANDed; only the hnsw/flat backends support
  filtering and deletion (other backends fail fast with 400).
- delete is an in-memory operation (same semantics as scalar pages; not written to the WAL/snapshot).
