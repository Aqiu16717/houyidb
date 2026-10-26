"""langchain-houyidb × 真实服务的联调：from_texts/search/filter/delete。

EN: Integration test for langchain-houyidb against a real server:
from_texts/search/filter/delete.
"""

from __future__ import annotations

from typing import List

import pytest
from langchain_core.embeddings import Embeddings
from langchain_houyidb import HouyiVectorStore

from conftest import fake_embed, wait_flushed

COLLECTION = "lc_it"

TEXTS = ["apple is a fruit", "banana is yellow", "laptop is electronics"]
METAS = [
    {"category": "fruit", "price": 5},
    {"category": "fruit", "price": 3},
    {"category": "electronics", "price": 999},
]


class FakeEmbeddings(Embeddings):
    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        return [fake_embed(t) for t in texts]

    def embed_query(self, text: str) -> List[float]:
        return fake_embed(text)


@pytest.fixture()
def store(base_url) -> HouyiVectorStore:
    s = HouyiVectorStore.from_texts(
        TEXTS,
        embedding=FakeEmbeddings(),
        metadatas=METAS,
        ids=["doc-a", "doc-b", "doc-c"],  # 固定 id，重跑幂等 / EN: fixed ids keep reruns idempotent
        collection_name=COLLECTION,
        base_url=base_url,
    )
    wait_flushed(s._collection)
    return s


def test_from_texts_returns_ids(store):
    docs = store.similarity_search("apple is a fruit", k=2)
    assert len(docs) == 2
    assert docs[0].page_content == "apple is a fruit"
    assert docs[0].metadata["category"] == "fruit"


def test_similarity_search_with_score(store):
    scored = store.similarity_search_with_score("apple is a fruit", k=3)
    assert len(scored) == 3
    scores = [s for _, s in scored]
    assert scores == sorted(scores, reverse=True)
    assert scored[0][0].page_content == "apple is a fruit"


def test_similarity_search_with_dict_filter(store):
    docs = store.similarity_search(
        "apple", k=3, filter={"category": "fruit", "price": {"$lt": 4}}
    )
    assert [d.page_content for d in docs] == ["banana is yellow"]
    assert docs[0].metadata["price"] == 3


def test_delete_excludes_ids(store):
    store.delete(ids=["doc-b"])
    docs = store.similarity_search("banana is yellow", k=3)
    assert "banana is yellow" not in [d.page_content for d in docs]
    store.delete(ids=["doc-b"])  # 幂等，不抛 / EN: idempotent, does not raise


def test_label_key_rejected(base_url):
    with pytest.raises(ValueError, match="label"):
        HouyiVectorStore.from_texts(
            ["x"],
            embedding=FakeEmbeddings(),
            metadatas=[{"label": "reserved"}],
            collection_name=COLLECTION,
            base_url=base_url,
        )
