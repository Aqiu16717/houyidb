"""假 OpenAI 上游：供 houyi-server --openai-upstream 指过来的联调用。

POST /v1/embeddings 按 input 文本用 fake_embed 同款确定性哈希生成向量，
model == "rate-limited" 时回 OpenAI 形状的 429。仅用标准库，独立可跑::

    .venv-eco/bin/python sdk/python/tests/fake_openai_upstream.py   # 127.0.0.1:18090
"""

from __future__ import annotations

import hashlib
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import List

HOST = "127.0.0.1"
PORT = 18090

#: 假上游产出的向量维度（与联调测试一致，任意小维度）。
#: EN: Vector dimensions produced by the fake upstream (matching the
#: integration tests; arbitrarily small).
DIMS = 16


def fake_embed(text: str, dims: int = DIMS) -> List[float]:
    """与 conftest.fake_embed 同款的确定性伪 embedding。

    EN: Deterministic pseudo-embedding identical to conftest.fake_embed.
    """
    out: List[float] = []
    counter = 0
    while len(out) < dims:
        block = hashlib.blake2b(
            f"{text}:{counter}".encode("utf-8"), digest_size=8
        ).digest()
        out.append(int.from_bytes(block, "big") / float(1 << 64) * 2.0 - 1.0)
        counter += 1
    return out[:dims]


class Handler(BaseHTTPRequestHandler):
    # BaseHTTPRequestHandler 约定要求这个 CamelCase 方法名。
    # EN: BaseHTTPRequestHandler requires this CamelCase method name.
    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v1/embeddings":
            self._reply(404, {"error": {"message": "not found", "type": "not_found"}})
            return
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        model = body.get("model", "fake-embed")
        if model == "rate-limited":
            self._reply(
                429,
                {
                    "error": {
                        "message": "Rate limit reached for fake upstream",
                        "type": "rate_limit_error",
                        "code": "rate_limit_exceeded",
                    }
                },
            )
            return
        raw_input = body.get("input", "")
        texts = raw_input if isinstance(raw_input, list) else [raw_input]
        if not all(isinstance(t, str) for t in texts):
            # 真实 OpenAI 对非字符串 input 回 400；假上游回空向量 200 会把
            # 调用方的坏请求伪装成成功，问题被推迟到检索阶段才暴露。
            # EN: Real OpenAI returns 400 for non-string input; a fake upstream
            # returning 200 with empty vectors would disguise the caller's bad
            # request as success and defer the problem until retrieval.
            self._reply(
                400,
                {
                    "error": {
                        "message": "input must be a string or a list of strings",
                        "type": "invalid_request_error",
                        "param": "input",
                        "code": "invalid_value",
                    }
                },
            )
            return
        data = [
            {"object": "embedding", "index": i, "embedding": fake_embed(t)}
            for i, t in enumerate(texts)
        ]
        self._reply(
            200,
            {
                "object": "list",
                "data": data,
                "model": model,
                "usage": {
                    "prompt_tokens": sum(len(str(t)) for t in texts),
                    "total_tokens": sum(len(str(t)) for t in texts),
                },
            },
        )

    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass  # 测试输出保持安静 / EN: keep test output quiet


if __name__ == "__main__":
    HTTPServer((HOST, PORT), Handler).serve_forever()
