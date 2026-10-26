#!/usr/bin/env python3
"""文本 → 向量 → 检索 的最小完整示例（走 HouyiDB 的 OpenAI 兼容网关）。

流程：
  1. 读取文本（--file，未指定则用内置示例），按字符切成小块；
  2. 调 /v1/embeddings 并带 `collection` 字段 → 上游模型算向量 + 自动入库；
  3. 查询文本走纯代理模式取查询向量；
  4. 在 collection 内检索并打印命中原文。

前置：
  HouyiDB 启动时配置了 OpenAI 兼容上游，例如 Ollama：
      ollama pull nomic-embed-text
      HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:11434 ./scripts/trial_start.sh
  没有真实模型时，可用 mock 验证链路（无语义）：
      python3 scripts/mock_embedding_server.py --port 18100
      HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:18100 ./scripts/trial_start.sh

用法：
  python3 examples/text_ingest.py
  python3 examples/text_ingest.py --file README.md --model bge-m3 --collection my_docs
  EMBED_MODEL=text-embedding-3-small python3 examples/text_ingest.py --query "..."

说明（重要）：
  - 网关自动入库的 id = stable_id(文本块)，完全相同的文本块会互相覆盖；
    不能携带业务 id/标题/权限等元数据。真实生产摄入请使用 collection upsert
    接口 + 客户端 embedding（见 docs/EMBEDDING_GUIDE.md）。
  - 换模型 = 换向量空间，collection 维度首次写入即锁定，需要新建 collection 重灌。
  - embedding 由上游模型计算；本脚本只负责分块、调用和检索。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path

# 不读 HTTP_PROXY/HTTPS_PROXY：默认目标是 127.0.0.1，系统代理会把本机请求也劫走。
# EN: Ignore ambient proxy settings — the default target is loopback (cf. curl --noproxy).
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

SAMPLE = """HouyiDB 是一个单机、内存优先的向量数据库。它使用 HNSW 图索引做近似最近邻检索，
底层向量与标量数据放在内存里，写入走 WAL 做持久化，重启后自动重放恢复。

写入路径分两段：请求先进入队列，后台构建器把一批向量插入 HNSW 图。合图时先读锁规划、
再短写锁提交，检索可以并发读图，因此边写边搜的长尾延迟较低。

标量过滤走 label 精确倒排与 payload 标量页；命中量大时会构建连续向量缓存，
把过滤检索退化为顺序 SIMD 扫描。HTTP 与 pgwire 两种协议共用同一个索引内核。
"""


def post_json(url: str, payload: dict, timeout: float = 180.0) -> dict:
    req = urllib.request.Request(
        url,
        data=json.dumps(payload, ensure_ascii=False).encode("utf-8"),
        headers={"content-type": "application/json"},
        method="POST",
    )
    try:
        with _OPENER.open(req, timeout=timeout) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        if exc.code == 501:
            raise SystemExit(
                "网关未配置上游（HTTP 501）。启动服务时设置 "
                "HOUYI_OPENAI_UPSTREAM，例如 Ollama: http://127.0.0.1:11434"
            ) from exc
        raise SystemExit(f"HTTP {exc.code}: {body}") from exc
    except urllib.error.URLError as exc:
        raise SystemExit(f"无法连接 {url}: {exc.reason}（服务启动了吗？）") from exc


def get_json(url: str, timeout: float = 10.0) -> dict:
    try:
        with _OPENER.open(url, timeout=timeout) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise SystemExit(f"HTTP {exc.code}: {body}") from exc
    except urllib.error.URLError as exc:
        raise SystemExit(f"无法连接 {url}: {exc.reason}（服务启动了吗？）") from exc


def chunk_text(text: str, size: int, overlap: int) -> list[str]:
    """按字符切块（仅演示！生产请用按 token/语义的切分器）。

    EN: Character-based chunking (demo only! Use a token/semantic splitter in
    production).
    """
    text = " ".join(text.split())
    if not text:
        return []
    step = max(1, size - overlap)
    chunks = [text[i : i + size] for i in range(0, len(text), step)]
    return [c for c in chunks if c.strip()]


def embed(base: str, model: str, texts: list[str], collection: str | None) -> dict:
    payload: dict = {"model": model, "input": texts}
    if collection:
        payload["collection"] = collection
    return post_json(f"{base}/v1/embeddings", payload)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--url",
        default=os.environ.get("HOUYI_TRIAL_URL", "http://127.0.0.1:17000"),
        help="HouyiDB HTTP 地址（默认环境变量 HOUYI_TRIAL_URL 或 127.0.0.1:17000）",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("EMBED_MODEL", "nomic-embed-text"),
        help="传给上游的 embedding 模型名（默认环境变量 EMBED_MODEL）",
    )
    parser.add_argument("--collection", default="docs_demo")
    parser.add_argument("--file", help="待摄入的文本文件；未指定则用内置示例")
    parser.add_argument("--chunk-size", type=int, default=120)
    parser.add_argument("--overlap", type=int, default=20)
    parser.add_argument("--batch", type=int, default=32, help="单次 embedding 条数")
    parser.add_argument("--query", default="HouyiDB 的写入路径有什么特点？")
    parser.add_argument("--k", type=int, default=3)
    args = parser.parse_args()
    if args.chunk_size <= 0:
        parser.error("--chunk-size must be >= 1")
    if args.overlap < 0 or args.overlap >= args.chunk_size:
        parser.error("--overlap must be >= 0 and < --chunk-size")
    if args.batch <= 0:
        parser.error("--batch must be >= 1")
    if args.k <= 0:
        parser.error("--k must be >= 1")

    base = args.url.rstrip("/")
    get_json(f"{base}/v1/health")
    print(f"== 服务在线：{base}（model={args.model}）")

    if args.file:
        try:
            raw = Path(args.file).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            raise SystemExit(
                f"读取 {args.file} 失败（需要 UTF-8 文本，可先转码）: {exc}"
            ) from exc
    else:
        raw = SAMPLE
    chunks = chunk_text(raw, args.chunk_size, args.overlap)
    if not chunks:
        raise SystemExit("文本为空，无内容可摄入")
    print(f"== 文本切块：{len(chunks)} 块（size={args.chunk_size}, overlap={args.overlap}）")

    upserted = 0
    for start in range(0, len(chunks), args.batch):
        batch = chunks[start : start + args.batch]
        resp = embed(base, args.model, batch, args.collection)
        upserted += int(resp.get("houyi", {}).get("upserted", len(batch)))
    print(f"== 自动入库：collection={args.collection}，upserted={upserted}")

    query_vec = embed(base, args.model, [args.query], None)["data"][0]["embedding"]
    print(f"== 查询向量维度：{len(query_vec)}")

    hits = post_json(
        f"{base}/v1/collections/{args.collection}/search",
        {"vector": query_vec, "k": args.k, "include_payload": True},
    )
    ids = hits.get("ids", [])
    scores = hits.get("scores", [])
    payloads = hits.get("payloads", {})
    if not ids:
        print("!! 检索结果为空（collection 是否已入库？）")
        sys.exit(1)

    print(f"\n== Top-{len(ids)}（query: {args.query}）")
    for rank, (vid, score) in enumerate(zip(ids, scores), 1):
        text = (payloads.get(str(vid)) or {}).get("text", "<no payload>")
        print(f"{rank}. id={vid} score={score:.4f}")
        print(f"   {text[:180]}{'…' if len(text) > 180 else ''}")

    print(
        "\n提示：若上游是 mock（hash 向量），排序没有语义；换成 bge/nomic/OpenAI "
        "等真实模型后再看相关性。"
    )


if __name__ == "__main__":
    main()
