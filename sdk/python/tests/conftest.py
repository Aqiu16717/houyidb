"""三个包的联调测试共享设施。

针对一个真实运行的 houyi-server（新二进制，含 delete/include_payload）。
启动方式::

    cargo build --release -p houyi-server
    ./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 \
        --pgwire-addr 127.0.0.1:15432 --wal-path /tmp/houyidb-eco.wal

运行::

    .venv-eco/bin/python -m pytest sdk/python/tests -v

测试依赖（不在任一包的 install_requires 里，属联调环境自身开销）::

    pip install pytest requests openai   # openai 仅网关用例需要
"""

from __future__ import annotations

import os

import pytest

# fake_embed 单一事实来源在 fake_openai_upstream（网关假上游必须与之
# 产出一致，否则"网关写入 vs SDK 检索"的向量对不上）。
# EN: fake_embed's single source of truth lives in fake_openai_upstream (the
# fake gateway upstream must produce identical output, otherwise "gateway
# write vs SDK retrieval" vectors do not line up).
from fake_openai_upstream import DIMS, fake_embed  # noqa: F401

#: 尾斜杠会拼出 //v1/health 这类双斜杠路径（部分路由不容忍），取值时剥掉。
#: EN: A trailing slash would produce double-slash paths like //v1/health
#: (some routes reject them), so strip it when reading the value.
BASE_URL = os.environ.get("HOUYIDB_URL", "http://127.0.0.1:17000").rstrip("/")


def wait_flushed(collection, timeout: float = 10.0) -> None:
    """等待 collection 的异步构图队列排空（双速写入：批量 upsert 先滞留
    builder_queue，flush 后才对检索可见）。测试以此消除可见性竞态。

    注意：delete-by-filter 不依赖排空（标量页同步落页，服务端按页求值）。

    EN: Wait for the collection's async build queue to drain (dual-speed
    writes: batch upserts linger in builder_queue and only become visible to
    search after flush). Tests use this to remove visibility races.

    Note: delete-by-filter does not depend on draining (scalar rows land on the
    page synchronously and the server evaluates against the page).
    """
    import time

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if collection.status()["pending_count"] == 0:
            return
        time.sleep(0.02)
    raise TimeoutError(f"collection {collection.name} flush timeout")


@pytest.fixture(scope="session")
def base_url() -> str:
    import requests

    try:
        resp = requests.get(f"{BASE_URL}/v1/health", timeout=3.0)
        resp.raise_for_status()
    # 在 fixture 中直接给出可操作的报错。
    # EN: Surface an actionable error directly from the fixture.
    except Exception as e:  # noqa: BLE001
        pytest.fail(
            f"houyi-server 不可达（{BASE_URL}）：{e}\n"
            "先启动新二进制：./target/release/houyi-server "
            "--benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal"
        )
    return BASE_URL
