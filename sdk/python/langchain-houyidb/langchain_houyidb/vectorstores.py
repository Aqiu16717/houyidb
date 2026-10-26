"""HouyiDB 的 LangChain ``VectorStore`` 实现。

语义映射（adapter 的全部职责）：

- ``Document.page_content`` ↔ payload[``text_key``]（默认 ``"text"``）；
- ``Document.metadata`` ↔ payload 其余键（扁平化，嵌套值按 JSON 文本列存储）；
- embedding 由用户侧的 ``Embeddings`` 计算，HouyiDB 只存取向量（框架约定）；
- str id ↔ u64 向量 id：``houyidb.stable_id`` 确定性哈希，delete 重放同一映射；
- filter dict ↔ HouyiDB filters：``{"col": v}`` 等值，或
  ``{"col": {"$gt": v}}`` 操作符形式；多键取交集；
- score 语义：余弦相似度（越大越相似），直接透出 HouyiDB 分数。

EN: LangChain ``VectorStore`` implementation for HouyiDB. Semantic mapping
(the adapter's entire responsibility):

- ``Document.page_content`` <-> payload[``text_key``] (default ``"text"``);
- ``Document.metadata`` <-> the remaining payload keys (flattened; nested
  values are stored as JSON text columns);
- embeddings are computed by the user-side ``Embeddings``; HouyiDB only stores
  and retrieves vectors (framework contract);
- str id <-> u64 vector id: deterministic hash via ``houyidb.stable_id``;
  delete replays the same mapping;
- filter dict <-> HouyiDB filters: ``{"col": v}`` for equality, or
  ``{"col": {"$gt": v}}`` operator form; multiple keys intersect;
- score semantics: cosine similarity (larger is more similar), exposing the
  HouyiDB score directly.
"""

from __future__ import annotations

import uuid
from typing import Any, Dict, Iterable, List, Optional, Tuple

from houyidb import Filter, HouyiClient, Vector, stable_id
from langchain_core.documents import Document
from langchain_core.embeddings import Embeddings
from langchain_core.vectorstores import VectorStore

#: LangChain 操作符 → HouyiDB 过滤算子（$ne/$nin 等无对应语义的显式拒绝）。
#: EN: LangChain operators mapped to HouyiDB filter ops ($ne/$nin etc. without
#: equivalent semantics are rejected explicitly).
_OP_MAP = {
    "$eq": "eq",
    "$gt": "gt",
    "$gte": "gte",
    "$lt": "lt",
    "$lte": "lte",
    "$in": "in",
}


def _translate_filter(filter: Optional[Dict[str, Any]]) -> Optional[List[Filter]]:
    """把 LangChain 风格的 filter dict 翻译为 HouyiDB filters（多键取交集）。

    EN: Translate a LangChain-style filter dict into HouyiDB filters (multiple
    keys intersect).
    """
    if not filter:
        return None
    out: List[Filter] = []
    for column, cond in filter.items():
        if isinstance(cond, dict):
            for op, value in cond.items():
                mapped = _OP_MAP.get(op)
                if mapped is None:
                    raise ValueError(
                        f"unsupported filter operator '{op}' for HouyiDB "
                        f"(supported: {sorted(_OP_MAP)})"
                    )
                out.append(Filter(column=column, op=mapped, value=value))
        else:
            out.append(Filter(column=column, op="eq", value=cond))
    return out


class HouyiVectorStore(VectorStore):
    """LangChain ``VectorStore`` — HouyiDB collection 后端。

    :param collection_name: HouyiDB collection 名（首次写入隐式创建）。
    :param embedding: 用户侧 embedding 模型（LangChain ``Embeddings``）。
    :param client: 可选的已构造 ``HouyiClient``；与 ``base_url`` 二选一。
    :param base_url: 服务地址（未传 client 时使用）。
    :param text_key: payload 中存放 ``page_content`` 的键名（不得为保留键 ``label``）。

    EN: LangChain ``VectorStore`` backed by a HouyiDB collection.

    :param collection_name: HouyiDB collection name (created implicitly on the
        first write).
    :param embedding: User-side embedding model (LangChain ``Embeddings``).
    :param client: Optional pre-built ``HouyiClient``; mutually exclusive with
        ``base_url``.
    :param base_url: Server address (used when client is not passed).
    :param text_key: Payload key holding ``page_content`` (must not be the
        reserved key ``label``).
    """

    def __init__(
        self,
        collection_name: str,
        embedding: Embeddings,
        *,
        client: Optional[HouyiClient] = None,
        base_url: Optional[str] = None,
        text_key: str = "text",
    ):
        if text_key == "label":
            raise ValueError("text_key 'label' collides with the reserved label column")
        if client is None:
            client = HouyiClient(base_url or "http://127.0.0.1:7000")
        self._collection = client.collection(collection_name)
        self._embedding = embedding
        self._text_key = text_key

    @property
    def embeddings(self) -> Embeddings:
        return self._embedding

    # ── 写入 ──────────────────────────────────────────────
    # EN: Write path.

    def add_texts(
        self,
        texts: Iterable[str],
        metadatas: Optional[List[dict]] = None,
        *,
        ids: Optional[List[str]] = None,
        **kwargs: Any,
    ) -> List[str]:
        texts = list(texts)
        if ids is None:
            ids = [uuid.uuid4().hex for _ in texts]
        if metadatas is None:
            metadatas = [{} for _ in texts]
        if not (len(ids) == len(texts) == len(metadatas)):
            raise ValueError("ids/texts/metadatas length mismatch")

        vectors = self._embedding.embed_documents(texts)
        batch = []
        for str_id, text, metadata, values in zip(ids, texts, metadatas, vectors):
            if "label" in metadata:
                raise ValueError(
                    "metadata key 'label' is reserved by HouyiDB; rename the key"
                )
            if self._text_key in metadata:
                # 与 'label' 守卫对称：metadata 展开在 text_key 之后，同名键会
                # 静默覆盖正文——存进去的是 metadata 值，读回时被当作 page_content
                # 弹出，正文丢失且该 metadata 键也随之消失（静默数据腐坏）。
                # EN: Symmetric to the 'label' guard: metadata is expanded after
                # text_key, so a same-named key silently overwrites the content
                # — the metadata value is stored, then popped as page_content on
                # read; the text is lost and that metadata key disappears too
                # (silent data corruption).
                raise ValueError(
                    f"metadata key '{self._text_key}' collides with text_key; "
                    "rename the key"
                )
            payload = {self._text_key: text, **metadata}
            batch.append(Vector(id=stable_id(str_id), values=values, payload=payload))
        self._collection.upsert(batch)
        return list(ids)

    # ── 检索 ──────────────────────────────────────────────
    # EN: Retrieval.

    def similarity_search(
        self,
        query: str,
        k: int = 4,
        filter: Optional[Dict[str, Any]] = None,
        **kwargs: Any,
    ) -> List[Document]:
        return [doc for doc, _ in self.similarity_search_with_score(query, k, filter=filter, **kwargs)]

    def similarity_search_with_score(
        self,
        query: str,
        k: int = 4,
        filter: Optional[Dict[str, Any]] = None,
        **kwargs: Any,
    ) -> List[Tuple[Document, float]]:
        vector = self._embedding.embed_query(query)
        return self.similarity_search_by_vector_with_score(vector, k, filter=filter, **kwargs)

    def similarity_search_by_vector(
        self,
        embedding: List[float],
        k: int = 4,
        filter: Optional[Dict[str, Any]] = None,
        **kwargs: Any,
    ) -> List[Document]:
        return [
            doc
            for doc, _ in self.similarity_search_by_vector_with_score(
                embedding, k, filter=filter, **kwargs
            )
        ]

    def similarity_search_by_vector_with_score(
        self,
        embedding: List[float],
        k: int = 4,
        filter: Optional[Dict[str, Any]] = None,
        **kwargs: Any,
    ) -> List[Tuple[Document, float]]:
        filters = _translate_filter(filter)
        ef_search = kwargs.get("ef_search")
        if filters:
            result = self._collection.hybrid_search(
                embedding, k, filters=filters, ef_search=ef_search, include_payload=True
            )
        else:
            result = self._collection.search(
                embedding, k, ef_search=ef_search, include_payload=True
            )
        docs: List[Tuple[Document, float]] = []
        for hit in result.hits:
            payload = dict(hit.payload or {})
            text = str(payload.pop(self._text_key, ""))
            docs.append((Document(page_content=text, metadata=payload), hit.score))
        return docs

    # ── 删除 ──────────────────────────────────────────────
    # EN: Deletion.

    def delete(self, ids: Optional[List[str]] = None, **kwargs: Any) -> bool:
        if not ids:
            raise ValueError("delete requires explicit ids (drop-all is not supported)")
        self._collection.delete([stable_id(str_id) for str_id in ids])
        return True

    # ── 工厂 ──────────────────────────────────────────────
    # EN: Factory.

    @classmethod
    def from_texts(
        cls,
        texts: List[str],
        embedding: Embeddings,
        metadatas: Optional[List[dict]] = None,
        *,
        ids: Optional[List[str]] = None,
        collection_name: str = "langchain",
        client: Optional[HouyiClient] = None,
        base_url: Optional[str] = None,
        text_key: str = "text",
        **kwargs: Any,
    ) -> "HouyiVectorStore":
        store = cls(
            collection_name,
            embedding,
            client=client,
            base_url=base_url,
            text_key=text_key,
        )
        store.add_texts(texts, metadatas, ids=ids, **kwargs)
        return store
