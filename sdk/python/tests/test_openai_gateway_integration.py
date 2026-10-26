"""OpenAI 兼容网关端到端联调：用真实 openai SDK 打 houyi-server 的 /v1/embeddings。

验证三件事：
1. 纯代理——把 base_url 从 OpenAI 换成 HouyiDB 就能工作（透明转发，无 houyi 字段）。
2. 自动入库——extra_body 带 collection 时，embedding 顺手 upsert 进库，
   响应注入 houyi 扩展字段，随后可用 houyidb SDK 检索回读（id == stable_id(text)）。
3. 上游错误透传——429 状态码与正文原样回到客户端（映射为 RateLimitError）。

依赖：houyi-server 以 --openai-upstream 指向假上游启动::

    .venv-eco/bin/python sdk/python/tests/fake_openai_upstream.py &   # 18090
    ./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 \
        --wal-path /tmp/houyidb-eco.wal --openai-upstream http://127.0.0.1:18090
"""

from __future__ import annotations

import pytest
from openai import OpenAI, RateLimitError

from houyidb import HouyiClient, stable_id

from conftest import fake_embed, wait_flushed


@pytest.fixture()
def oai(base_url: str) -> OpenAI:
    return OpenAI(base_url=f"{base_url}/v1", api_key="not-needed")


@pytest.fixture()
def sdk(base_url: str) -> HouyiClient:
    return HouyiClient(base_url)


def test_transparent_proxy_plain_embeddings(oai: OpenAI) -> None:
    """不带 collection：纯代理，响应里不应出现 houyi 扩展字段。

    EN: Without a collection: pure proxy; the response must not carry the
    houyi extension fields.
    """
    resp = oai.embeddings.create(model="fake-embed", input="hello proxy")
    assert len(resp.data) == 1
    assert resp.data[0].index == 0
    assert resp.data[0].embedding == pytest.approx(fake_embed("hello proxy"))
    assert resp.model == "fake-embed"
    assert not hasattr(resp, "houyi")


def test_auto_ingest_with_collection(oai: OpenAI, sdk: HouyiClient) -> None:
    """extra_body 带 collection：自动入库 + houyi 扩展字段 + SDK 检索回读。

    EN: extra_body with a collection: auto-ingest + houyi extension fields +
    SDK read-back.
    """
    texts = ["apple is a fruit", "banana is yellow"]
    resp = oai.embeddings.create(
        model="fake-embed",
        input=texts,
        extra_body={"collection": "oa_it"},
    )
    assert len(resp.data) == 2
    houyi = getattr(resp, "houyi", None)
    assert houyi is not None, "自动入库响应应注入 houyi 扩展字段"
    assert houyi["collection"] == "oa_it"
    assert houyi["upserted"] == 2

    col = sdk.collection("oa_it")
    wait_flushed(col)
    # 检索回读：用同一确定性向量查 top-1，命中的应是同一文本，id 是 stable_id(text)
    # EN: Read-back: query top-1 with the same deterministic vector; the hit
    # should be the same text, and its id is stable_id(text).
    result = col.search(fake_embed(texts[0]), k=1, include_payload=True)
    assert len(result.hits) == 1
    assert result.hits[0].id == stable_id(texts[0])
    assert result.hits[0].payload == {"text": texts[0]}


def test_upstream_error_passes_through(oai: OpenAI) -> None:
    """上游 429 原样透传：openai SDK 应将其映射为 RateLimitError，正文可读。

    EN: Upstream 429 passes through verbatim: the openai SDK should map it to
    RateLimitError and the body stays readable.
    """
    with pytest.raises(RateLimitError) as exc_info:
        oai.embeddings.create(model="rate-limited", input="whatever")
    err = exc_info.value
    assert err.status_code == 429
    assert "Rate limit reached" in str(err.body)
