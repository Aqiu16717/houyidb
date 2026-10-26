# llama-index-vector-stores-houyidb

> [English](README.en.md)

HouyiDB 的 LlamaIndex `BasePydanticVectorStore` adapter。

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

## 语义映射

| LlamaIndex | HouyiDB |
|------------|---------|
| node 正文 | payload[`text_key`]（默认 `"text"`） |
| metadata + `_node_content` | payload 其余键（嵌套值走 JSON 文本列） |
| `node_id`（str） | u64（`houyidb.stable_id` 确定性哈希） |
| `ref_doc_id` | 独立标量列，支撑 `delete(ref_doc_id)` |
| `MetadataFilters` | filters（eq/gt/gte/lt/lte/in，AND 交集） |
| score | 余弦相似度（越大越相似） |

embedding 由 LlamaIndex 管线计算（node 自带），HouyiDB 只存取向量。

## 已知限制

- 仅支持 AND 交集过滤（`FilterCondition.OR` 抛 `ValueError`）；操作符仅 eq/gt/gte/lt/lte/in。
- `delete(ref_doc_id)` 走服务端按条件删除（标量页求值），`add` 后立即删除也可靠命中尚在异步构图队列中的 node。
- node metadata 不得含保留键 `label`。
- collection 的 payload 存储 v1 易失（进程重启后 node 内容丢失，向量索引不受影响）。
