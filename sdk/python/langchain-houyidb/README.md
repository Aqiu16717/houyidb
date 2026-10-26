# langchain-houyidb

> [English](README.en.md)

HouyiDB 的 LangChain `VectorStore` adapter。

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

## 语义映射

| LangChain | HouyiDB |
|-----------|---------|
| `Document.page_content` | payload[`text_key`]（默认 `"text"`） |
| `Document.metadata` | payload 其余键 |
| str id | u64（`houyidb.stable_id` 确定性哈希） |
| `{"col": v}` / `{"col": {"$gt": v}}` | filters（eq/gt/gte/lt/lte/in，多键取交集） |
| score | 余弦相似度（越大越相似） |

embedding 由用户侧 `Embeddings` 计算，HouyiDB 只存取向量。

## 已知限制

- filter 操作符仅支持 `$eq/$gt/$gte/$lt/$lte/$in`（其余 `ValueError`）。
- metadata 不得含保留键 `label`。
- collection 的 payload 存储 v1 易失（进程重启后 metadata 丢失，向量索引不受影响）。
