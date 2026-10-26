# llama-index-vector-stores-houyidb

> [中文](README.md)

LlamaIndex `BasePydanticVectorStore` adapter for HouyiDB.

```python
from llama_index.core.schema import TextNode
from llama_index.core.vector_stores import VectorStoreQuery
from llama_index.vector_stores.houyidb import HouyiVectorStore

store = HouyiVectorStore(collection_name="docs", base_url="http://127.0.0.1:7000")

nodes = [TextNode(text="apple is a fruit", embedding=emb, metadata={"category": "fruit"})]
store.add(nodes)

result = store.query(VectorStoreQuery(query_embedding=q, similarity_top_k=3))
store.delete(ref_doc_id="<source document id>")
```

## Semantic Mapping

| LlamaIndex | HouyiDB |
|------------|---------|
| node text | payload[`text_key`] (default `"text"`) |
| metadata + `_node_content` | remaining payload keys (nested values go to a JSON text column) |
| `node_id` (str) | u64 (`houyidb.stable_id` deterministic hash) |
| `ref_doc_id` | separate scalar column, backing `delete(ref_doc_id)` |
| `MetadataFilters` | filters (eq/gt/gte/lt/lte/in, AND intersection) |
| score | cosine similarity (larger is more similar) |

Embeddings are computed by the LlamaIndex pipeline (carried on the node); HouyiDB only stores and retrieves vectors.

## Known Limitations

- Only AND-intersection filtering is supported (`FilterCondition.OR` raises `ValueError`); operators are limited to eq/gt/gte/lt/lte/in.
- `delete(ref_doc_id)` uses server-side delete-by-condition (evaluated against scalar pages); deleting immediately after `add` also reliably hits nodes that are still in the asynchronous build queue.
- Node metadata must not contain the reserved key `label`.
- Collection payload storage is volatile in v1 (node content is lost after a process restart; the vector index is unaffected).
