# HouyiDB Python 生态

> [English](README.en.md)

三个包，各司其职：

| 包 | 角色 | 依赖 |
|----|------|------|
| `houyidb` | collections HTTP API 客户端（传输层） | `requests` |
| `langchain-houyidb` | LangChain `VectorStore` adapter（语义方言层） | `langchain-core` + `houyidb` |
| `llama-index-vector-stores-houyidb` | LlamaIndex `BasePydanticVectorStore` adapter | `llama-index-core` + `houyidb` |

SDK 负责线路协议（端点、JSON、错误映射、str→u64 确定性 id 哈希）；
adapter 负责框架语义（Document/node 模型、embedding 委托、过滤 DSL 翻译、
score 口径、生命周期约定）。embedding 永远由调用方/框架计算，HouyiDB 只存取向量。

## 安装（开发态）

```bash
pip install -e sdk/python/houyidb \
            -e sdk/python/langchain-houyidb \
            -e sdk/python/llama-index-vector-stores-houyidb
```

## 最小示例

```python
from houyidb import HouyiClient, Vector, Filter

client = HouyiClient("http://127.0.0.1:7000")
col = client.collection("docs")  # 首次 upsert 隐式创建
col.upsert([Vector(id=1, values=emb, payload={"text": "apple", "price": 5})])

hits = col.search(query_emb, 3, include_payload=True)          # 纯向量
hits = col.hybrid_search(query_emb, 3,                         # 向量 × 过滤
                         filters=[Filter("price", "lt", 10)], include_payload=True)
col.delete([1])                                                # 按 id
col.delete(filters=[Filter("price", "lt", 10)])                # 按条件（见下）
```

LangChain / LlamaIndex 用法见各自包内 README。

## 写入可见性（双速写入）

批量 upsert 先滞留异步构图队列（builder_queue），flush（日常约几十毫秒）后才
对**检索**可见；但标量行随 upsert **同步**落页，因此：

- `delete(filters=...)` 按标量页求值，刚写入的向量立即可被按条件删除——
  不要用「先 search 拿 id 再 delete」的模式（检索看不到队列内向量，会漏删）；
- 检索断言前如需确定性，轮询 `col.status()["pending_count"] == 0`
  （测试辅助 `sdk/python/tests/conftest.py::wait_flushed`）。

## OpenAI 兼容网关（可选）

服务以 `--openai-upstream <上游 base_url>` 启动后暴露 OpenAI 形状的
`POST /v1/embeddings`：不带 `collection` 时是透明代理（"换 base_url 即可"）；
带 `collection`（SDK 经 `extra_body` 传入）时顺手把 embedding 入库——
"换 URL 即入库"：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:17000/v1", api_key="not-needed")
resp = client.embeddings.create(
    model="text-embedding-3-small",
    input=["apple is a fruit"],
    extra_body={"collection": "docs"},
)
resp.houyi  # {"collection": "docs", "upserted": 1} —— id = stable_id(text)，重跑幂等
```

## 测试

```bash
# 需要一个跑着新二进制的服务（含 delete/include_payload/openai 网关）
cargo build --release -p houyi-server
.venv-eco/bin/python sdk/python/tests/fake_openai_upstream.py &   # 假 OpenAI 上游 :18090
./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 \
    --pgwire-addr 127.0.0.1:15432 --wal-path /tmp/houyidb-eco.wal \
    --openai-upstream http://127.0.0.1:18090

.venv-eco/bin/python -m pytest sdk/python/tests -v   # HOUYIDB_URL 可覆盖默认地址
```

## 共同已知限制

- collection 的 payload 标量页 v1 易失：进程重启后 payload/过滤/delete 墓碑丢失，
  向量索引（WAL/快照）不受影响。
- payload/metadata 键 `label` 为保留列。
- 过滤操作符仅 `eq/gt/gte/lt/lte/in`，多条件 AND 交集；仅 hnsw/flat 后端支持
  过滤与删除（其余后端 400 fail-fast）。
- delete 为内存态操作（与标量页同口径，不进 WAL/快照）。
