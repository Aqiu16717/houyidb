"""llama-index-vector-stores-houyidb × 真实服务的联调：add/query/filter/delete。

EN: Integration test for llama-index-vector-stores-houyidb against a real
server: add/query/filter/delete.
"""

from __future__ import annotations

import pytest
from llama_index.core.schema import TextNode
from llama_index.core.vector_stores import (
    FilterOperator,
    MetadataFilter,
    MetadataFilters,
    VectorStoreQuery,
)
from llama_index.vector_stores.houyidb import HouyiVectorStore

from conftest import fake_embed, wait_flushed

COLLECTION = "li_it"


def _node(node_id: str, text: str, ref: str, **meta) -> TextNode:
    return TextNode(
        id_=node_id,
        text=text,
        embedding=fake_embed(text),
        metadata=meta,
        relationships={},
    )


def _make_nodes() -> list[TextNode]:
    nodes = [
        _node("n1", "apple is a fruit", "doc-1", category="fruit", price=5),
        _node("n2", "banana is yellow", "doc-1", category="fruit", price=3),
        _node("n3", "laptop is electronics", "doc-2", category="electronics", price=999),
    ]
    # ref_doc_id 通过 relationships 设置（REF_DOC 关系）。
    # EN: ref_doc_id is set via relationships (the REF_DOC relationship).
    from llama_index.core.schema import NodeRelationship, RelatedNodeInfo

    for n, ref in zip(nodes, ["doc-1", "doc-1", "doc-2"]):
        n.relationships[NodeRelationship.SOURCE] = RelatedNodeInfo(node_id=ref)
    return nodes


@pytest.fixture()
def store(base_url) -> HouyiVectorStore:
    s = HouyiVectorStore(collection_name=COLLECTION, base_url=base_url)
    s.add(_make_nodes())  # 固定 node_id，重跑幂等 / EN: fixed node_ids keep reruns idempotent
    wait_flushed(s.client)
    return s


def test_add_and_query(store):
    result = store.query(
        VectorStoreQuery(query_embedding=fake_embed("apple is a fruit"), similarity_top_k=2)
    )
    assert len(result.nodes) == 2
    assert result.nodes[0].get_content() == "apple is a fruit"
    assert result.nodes[0].metadata["category"] == "fruit"
    assert result.similarities[0] >= result.similarities[1]
    assert result.ids[0] == result.nodes[0].node_id


def test_query_with_metadata_filters(store):
    filters = MetadataFilters(
        filters=[
            MetadataFilter(key="category", operator=FilterOperator.EQ, value="fruit"),
            MetadataFilter(key="price", operator=FilterOperator.LT, value=4),
        ]
    )
    result = store.query(
        VectorStoreQuery(
            query_embedding=fake_embed("apple is a fruit"),
            similarity_top_k=3,
            filters=filters,
        )
    )
    assert [n.get_content() for n in result.nodes] == ["banana is yellow"]


def test_filter_only_query_zero_vector_fallback(store):
    """query_embedding=None 的纯过滤路径：零向量占位枚举候选 + 服务端过滤。

    覆盖 _dims_or_none 的正常分支（collection 存在时取维度构造零向量）。

    EN: Filter-only path with query_embedding=None: zero-vector placeholder
    enumerates candidates + server-side filtering.

    Covers the normal branch of _dims_or_none (dimensions read when the
    collection exists, used to build the zero vector).
    """
    filters = MetadataFilters(
        filters=[
            MetadataFilter(key="category", operator=FilterOperator.EQ, value="electronics")
        ]
    )
    result = store.query(VectorStoreQuery(similarity_top_k=3, filters=filters))
    assert [n.get_content() for n in result.nodes] == ["laptop is electronics"]


def test_or_condition_rejected(store):
    from llama_index.core.vector_stores import FilterCondition

    filters = MetadataFilters(
        filters=[MetadataFilter(key="category", value="fruit")],
        condition=FilterCondition.OR,
    )
    with pytest.raises(ValueError, match="OR"):
        store.query(
            VectorStoreQuery(
                query_embedding=fake_embed("x"), similarity_top_k=1, filters=filters
            )
        )


def test_delete_by_ref_doc_id(store):
    store.delete(ref_doc_id="doc-1")
    result = store.query(
        VectorStoreQuery(query_embedding=fake_embed("apple is a fruit"), similarity_top_k=5)
    )
    remaining = {n.get_content() for n in result.nodes}
    assert "apple is a fruit" not in remaining
    assert "banana is yellow" not in remaining
    assert "laptop is electronics" in remaining  # doc-2 不受影响 / EN: doc-2 unaffected
    store.delete(ref_doc_id="doc-1")  # 幂等，不抛 / EN: idempotent, does not raise
