"""HouyiDB 的 LlamaIndex ``BasePydanticVectorStore`` 实现。

语义映射（adapter 的全部职责）：

- node ↔ 向量 + payload：``node_to_metadata_dict`` 产出 ``_node_content``/
  ``_node_type``/metadata 一并入 payload（嵌套值走 JsonText 列），查询时
  ``metadata_dict_to_node`` 还原 node；正文另存 ``text_key`` 列（默认 ``"text"``）；
- node_id（str）↔ u64 向量 id：``houyidb.stable_id`` 确定性哈希；
- ``ref_doc_id`` 额外提升为独立标量列，支撑 ``delete(ref_doc_id)`` 的过滤删除；
- ``MetadataFilters`` ↔ HouyiDB filters（AND 交集；OR 与不支持的操作符显式拒绝）；
- score 语义：余弦相似度（越大越相似），直接透出。

EN: HouyiDB ``BasePydanticVectorStore`` implementation for LlamaIndex.
Semantic mapping (the adapter's entire responsibility):

- node <-> vector + payload: ``node_to_metadata_dict`` produces
  ``_node_content``/``_node_type``/metadata, all stored in the payload (nested
  values go to JsonText columns); queries restore the node with
  ``metadata_dict_to_node``; the content is also stored in the ``text_key``
  column (default ``"text"``);
- node_id (str) <-> u64 vector id: deterministic hash via
  ``houyidb.stable_id``;
- ``ref_doc_id`` is additionally promoted to its own scalar column, backing
  filter deletes via ``delete(ref_doc_id)``;
- ``MetadataFilters`` <-> HouyiDB filters (AND intersection; OR and unsupported
  operators are rejected explicitly);
- score semantics: cosine similarity (larger is more similar), passed through.
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional, Sequence

from houyidb import Filter, HouyiClient, HouyiError, Vector, stable_id
from llama_index.core.bridge.pydantic import PrivateAttr
from llama_index.core.schema import BaseNode, TextNode
from llama_index.core.vector_stores.types import (
    BasePydanticVectorStore,
    FilterCondition,
    FilterOperator,
    MetadataFilters,
    VectorStoreQuery,
    VectorStoreQueryMode,
    VectorStoreQueryResult,
)
from llama_index.core.vector_stores.utils import (
    metadata_dict_to_node,
    node_to_metadata_dict,
)

#: LlamaIndex 操作符 → HouyiDB 过滤算子。
#: EN: LlamaIndex operators mapped to HouyiDB filter ops.
_OP_MAP = {
    FilterOperator.EQ: "eq",
    FilterOperator.GT: "gt",
    FilterOperator.GTE: "gte",
    FilterOperator.LT: "lt",
    FilterOperator.LTE: "lte",
    FilterOperator.IN: "in",
}

#: payload 中存放 ref_doc_id 的列名（delete 按此列过滤）。
#: EN: Payload column name holding ref_doc_id (delete filters on this column).
_REF_DOC_ID_KEY = "ref_doc_id"


def _translate_filters(filters: Optional[MetadataFilters]) -> Optional[List[Filter]]:
    """``MetadataFilters`` → HouyiDB filters（仅 AND 交集可下推）。

    EN: ``MetadataFilters`` -> HouyiDB filters (only AND intersections can be
    pushed down).
    """
    if not filters or not filters.filters:
        return None
    if filters.condition == FilterCondition.OR:
        raise ValueError(
            "HouyiDB filters intersect (AND only); OR conditions are not supported"
        )
    out: List[Filter] = []
    for f in filters.filters:
        mapped = _OP_MAP.get(f.operator)
        if mapped is None:
            raise ValueError(
                f"unsupported filter operator '{f.operator}' for HouyiDB "
                f"(supported: eq/gt/gte/lt/lte/in)"
            )
        value = f.value
        if mapped == "in":
            # tuple/set 与 list 一样是集合语义：不展开会被包成单元素列表
            # [[1, 2]]，服务端 in 判断变成「一个列表元素」的成员检查，永不命中。
            # EN: tuple/set carry set semantics like list: without expansion
            # they get wrapped into a single-element list [[1, 2]], so the
            # server's `in` check tests membership of "one list element" and
            # never matches.
            if isinstance(value, (tuple, set, frozenset)):
                value = list(value)
            elif not isinstance(value, list):
                value = [value]
        out.append(Filter(column=f.key, op=mapped, value=value))
    return out


class HouyiVectorStore(BasePydanticVectorStore):
    """LlamaIndex ``BasePydanticVectorStore`` — HouyiDB collection 后端。

    :param collection_name: HouyiDB collection 名（首次写入隐式创建）。
    :param base_url: 服务地址；传了 ``client`` 时忽略。
    :param client: 可选的已构造 ``HouyiClient``。
    :param text_key: payload 中存放 node 正文的键名（不得为保留键 ``label``）。

    EN: LlamaIndex ``BasePydanticVectorStore`` backed by a HouyiDB collection.

    :param collection_name: HouyiDB collection name (created implicitly on the
        first write).
    :param base_url: Server address; ignored when ``client`` is passed.
    :param client: Optional pre-built ``HouyiClient``.
    :param text_key: Payload key holding the node content (must not be the
        reserved key ``label``).
    """

    stores_text: bool = True
    flat_metadata: bool = False

    collection_name: str
    base_url: str
    text_key: str = "text"

    _collection: Any = PrivateAttr()

    def __init__(
        self,
        collection_name: str,
        base_url: str = "http://127.0.0.1:7000",
        client: Optional[HouyiClient] = None,
        text_key: str = "text",
        **kwargs: Any,
    ) -> None:
        if text_key == "label":
            raise ValueError("text_key 'label' collides with the reserved label column")
        super().__init__(
            collection_name=collection_name,
            base_url=base_url,
            text_key=text_key,
            **kwargs,
        )
        if client is None:
            client = HouyiClient(base_url)
        self._collection = client.collection(collection_name)

    @classmethod
    def class_name(cls) -> str:
        return "HouyiVectorStore"

    @property
    def client(self) -> Any:
        """底层 collection 句柄（框架约定属性名）。

        EN: Underlying collection handle (framework-conventional property name).
        """
        return self._collection

    # ── 写入 ──────────────────────────────────────────────
    # EN: Write path.

    def add(self, nodes: Sequence[BaseNode], **add_kwargs: Any) -> List[str]:
        ids: List[str] = []
        batch: List[Vector] = []
        for node in nodes:
            metadata_dict = node_to_metadata_dict(
                node, remove_text=True, flat_metadata=self.flat_metadata
            )
            ref_doc_id = node.ref_doc_id
            if ref_doc_id is not None:
                metadata_dict[_REF_DOC_ID_KEY] = ref_doc_id
            if self.text_key in metadata_dict:
                # metadata 展开在 text_key 之后会静默覆盖正文（与 langchain
                # adapter 同款守卫）：读回时 metadata 值被当作正文弹出。
                # EN: Metadata expanded after text_key silently overwrites the
                # content (same guard as the LangChain adapter): on read the
                # metadata value is popped as the content.
                raise ValueError(
                    f"node metadata key '{self.text_key}' collides with "
                    f"text_key '{self.text_key}'; rename the metadata key"
                )
            payload: Dict[str, Any] = {
                self.text_key: node.get_content(metadata_mode="none"),
                **metadata_dict,
            }
            if "label" in payload:
                raise ValueError(
                    "node metadata key 'label' is reserved by HouyiDB; rename the key"
                )
            batch.append(
                Vector(
                    id=stable_id(node.node_id),
                    values=node.get_embedding(),
                    payload=payload,
                )
            )
            ids.append(node.node_id)
        if batch:
            self._collection.upsert(batch)
        return ids

    # ── 删除 ──────────────────────────────────────────────
    # EN: Deletion.

    def delete(self, ref_doc_id: str, **delete_kwargs: Any) -> None:
        try:
            self._collection.delete(
                filters=[Filter(_REF_DOC_ID_KEY, "eq", ref_doc_id)]
            )
        except HouyiError as e:
            if e.status_code == 404:
                return  # collection 尚未创建，无事可删 / EN: not created yet, nothing to delete
            raise

    # ── 检索 ──────────────────────────────────────────────
    # EN: Retrieval.

    def query(self, query: VectorStoreQuery, **kwargs: Any) -> VectorStoreQueryResult:
        if query.mode != VectorStoreQueryMode.DEFAULT:
            # 非 DEFAULT 模式（SPARSE/HYBRID 等）不带 query_embedding，会落入零向量
            # 兜底返回任意 top-k——静默错误答案比显式拒绝更糟。
            # EN: Non-DEFAULT modes (SPARSE/HYBRID etc.) carry no
            # query_embedding and would fall through to the zero-vector fallback
            # returning arbitrary top-k — a silent wrong answer is worse than an
            # explicit rejection.
            raise ValueError(
                f"HouyiVectorStore does not support query mode '{query.mode}'"
            )
        if query.doc_ids or query.node_ids:
            raise ValueError("HouyiVectorStore does not support doc_ids/node_ids queries")
        vector = query.query_embedding
        if vector is None:
            # 纯过滤查询：用零向量占位（余弦约定零向量距离为 1，仅作候选枚举）。
            # EN: Filter-only query: use a zero vector as a placeholder (under
            # the cosine convention it yields distance 1; only for candidate
            # enumeration).
            dims = self._dims_or_none()
            if dims is None:
                return VectorStoreQueryResult(nodes=[], similarities=[], ids=[])
            vector = [0.0] * dims
        k = query.similarity_top_k
        filters = _translate_filters(query.filters)
        result = self._collection.hybrid_search(
            vector, k, filters=filters, include_payload=True
        )

        nodes: List[BaseNode] = []
        similarities: List[float] = []
        ids: List[str] = []
        for hit in result.hits:
            payload = dict(hit.payload or {})
            text = str(payload.pop(self.text_key, ""))
            payload.pop(_REF_DOC_ID_KEY, None)
            if "_node_content" in payload:
                node = metadata_dict_to_node(payload)
                node.set_content(text)
            else:
                # 跨来源互操作：OpenAI 网关自动入库（payload={"text": ...}）与
                # langchain adapter 写入的行没有 _node_content——
                # metadata_dict_to_node 会整批抛 ValueError 让整条 query 崩掉；
                # 降级为纯 TextNode 保证混合来源 collection 可查
                # （代价：此类行重建的 node_id 是新随机 id，u64 不可逆）。
                # EN: Cross-source interoperability: rows auto-ingested by the
                # OpenAI gateway (payload={"text": ...}) and written by the
                # LangChain adapter have no _node_content —
                # metadata_dict_to_node raises ValueError for the whole batch
                # and crashes the entire query; degrade to a plain TextNode so
                # mixed collections stay queryable (cost: the rebuilt node_id is
                # a new random id; the u64 mapping is not reversible).
                node = TextNode(text=text, metadata=payload)
            nodes.append(node)
            similarities.append(hit.score)
            ids.append(node.node_id)
        return VectorStoreQueryResult(
            nodes=nodes, similarities=similarities, ids=ids
        )

    # ── 内部 ──────────────────────────────────────────────
    # EN: Internals.

    def _dims_or_none(self) -> Optional[int]:
        """collection 维度；collection 不存在（404）时返回 None。

        EN: Collection dimensions; returns None when the collection does not
        exist (404).
        """
        try:
            status = self._collection.status()
        except HouyiError as e:
            if e.status_code == 404:
                return None
            raise
        return int(status["dims"])
