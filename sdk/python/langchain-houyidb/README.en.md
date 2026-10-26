# langchain-houyidb

> [中文](README.md)

LangChain `VectorStore` adapter for HouyiDB.

```python
from langchain_houyidb import HouyiVectorStore

store = HouyiVectorStore.from_texts(
    ["apple is a fruit", "laptop is electronics"],
    embedding=MyEmbeddings(),
    collection_name="docs",
    base_url="http://127.0.0.1:7000",
)

docs = store.similarity_search("fruit", k=2)
docs = store.similarity_search(
    "device", k=2,
    filter={"category": "electronics", "price": {"$lt": 1000}},
)
store.delete(ids=["<add_texts 返回的 id>"])
```

## Semantic Mapping

| LangChain | HouyiDB |
|-----------|---------|
| `Document.page_content` | payload[`text_key`] (default `"text"`) |
| `Document.metadata` | remaining payload keys |
| str id | u64 (`houyidb.stable_id` deterministic hash) |
| `{"col": v}` / `{"col": {"$gt": v}}` | filters (eq/gt/gte/lt/lte/in; multiple keys are intersected) |
| score | cosine similarity (larger is more similar) |

Embeddings are computed by the user-side `Embeddings`; HouyiDB only stores and retrieves vectors.

## Known Limitations

- Filter operators support only `$eq/$gt/$gte/$lt/$lte/$in` (anything else raises `ValueError`).
- metadata must not contain the reserved key `label`.
- Collection payload storage is volatile in v1 (metadata is lost after a process restart; the vector index is unaffected).
