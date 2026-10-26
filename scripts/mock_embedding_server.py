#!/usr/bin/env python3
"""OpenAI 兼容的 mock /v1/embeddings —— 仅用于链路自检，**不产生语义向量**。

用途：没有 OpenAI key、也不想装 Ollama/TEI 时，验证
「文本 → 网关 → 自动入库 → 检索」整条链路是否通。

它把文本的 blake2b 哈希展开成固定维度的单位向量：同样的文本得到同样向量，
不同文本得到不同向量，但**没有任何语义相似性**（"苹果"和"水果"不会更近）。
真实检索效果请用 OpenAI / Ollama / TEI 等真实模型。

用法：
    python3 scripts/mock_embedding_server.py --port 18100 [--dims 32]

然后在 HouyiDB 启动时设置：
    HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:18100
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def deterministic_vector(text: str, dims: int) -> list[float]:
    # 64 字节哈希循环展开到 dims 维，映射到 [-1, 1]，再单位化。
    # EN: Expand the 64-byte hash cyclically to dims dimensions, map to [-1, 1], then
    # EN: normalize.
    digest = hashlib.blake2b(text.encode("utf-8"), digest_size=64).digest()
    raw = [((digest[i % len(digest)] / 255.0) * 2.0 - 1.0) for i in range(dims)]
    norm = math.sqrt(sum(x * x for x in raw)) or 1.0
    return [x / norm for x in raw]


class Handler(BaseHTTPRequestHandler):
    dims = 32

    def _json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json; charset=utf-8")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # 标准库钩子名，保留 N802 命名
    # EN: stdlib hook name; the N802 naming is kept
    def do_POST(self) -> None:  # noqa: N802 (stdlib hook name)
        if self.path not in ("/v1/embeddings", "/embeddings"):
            self._json(404, {"error": {"message": f"unknown path {self.path}"}})
            return
        try:
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
        # 宽泛捕获异常（BLE001）：mock 只做链路验证
        # EN: Broad exception catch (BLE001): the mock only validates the request chain
        except Exception as exc:  # noqa: BLE001 — mock 只做链路验证
            self._json(400, {"error": {"message": f"bad JSON: {exc}"}})
            return

        raw_input = body.get("input")
        if isinstance(raw_input, str):
            texts = [raw_input]
        elif isinstance(raw_input, list) and all(isinstance(x, str) for x in raw_input):
            texts = raw_input
        else:
            self._json(
                400,
                {"error": {"message": "input must be a string or an array of strings"}},
            )
            return
        if not texts:
            self._json(400, {"error": {"message": "input must not be empty"}})
            return

        data = [
            {
                "object": "embedding",
                "index": idx,
                "embedding": deterministic_vector(text, self.dims),
            }
            for idx, text in enumerate(texts)
        ]
        self._json(
            200,
            {
                "object": "list",
                "data": data,
                "model": body.get("model") or f"mock-hash-{self.dims}d",
                "usage": {"prompt_tokens": 0, "total_tokens": 0},
            },
        )

    # 参数名沿用标准库签名
    # EN: Parameter name follows the stdlib signature
    def log_message(self, format: str, *args) -> None:  # noqa: A002 — stdlib signature
        print(f"[mock-embed] {format % args}", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description="mock OpenAI-compatible embeddings")
    parser.add_argument("--port", type=int, default=18100)
    parser.add_argument("--dims", type=int, default=32)
    args = parser.parse_args()
    Handler.dims = args.dims
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(
        f"[mock-embed] listening on http://127.0.0.1:{args.port}/v1/embeddings "
        f"(dims={args.dims}, hash-only, NO semantic meaning)",
        flush=True,
    )
    server.serve_forever()


if __name__ == "__main__":
    main()
