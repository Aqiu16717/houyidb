# 向量化接入指南（Embedding Guide）

> [English](EMBEDDING_GUIDE.en.md)

> 目标：文本进、向量出、自动入库、可检索——同时不把模型推理塞进数据库内核。
>
> 适用版本：v0.2.0-beta.4+。内核**不做** embedding 推理；HouyiDB 通过
> OpenAI 兼容的 `/v1/embeddings` 网关连接外部模型（OpenAI / Ollama / TEI /
> vLLM / DashScope 等），可选把返回的向量**自动写入 collection**。

---

## 1. 先选一条路

| 你的情况 | 推荐做法 | 需要改数据库吗 |
|---|---|---|
| 有 OpenAI / DashScope 等 API key | 网关直连云端 | 不需要 |
| 要求离线 / 数据不出内网 | 本机 Ollama 或 TEI + 网关 | 不需要 |
| 只验证链路，没有模型 | 内置 mock embedding | 不需要 |
| 文档带业务 id / 标题 / 权限等元数据 | collection upsert + 客户端算向量 | 不需要（用普通写入 API） |
| 要分块、增量更新、模型升级重建 | 客户端摄入管线（LangChain/LlamaIndex 等） | 不需要 |

**结论**：90% 的「向量化」需求在网关层就能解决，不需要改 HouyiDB。

---

## 2. 五分钟跑通

### 2.1 方式一：Ollama（推荐，离线、免费）

```bash
# 1) 安装 Ollama 后拉一个 embedding 模型（任选）
ollama pull nomic-embed-text      # 768 维，英文强
# 或：ollama pull bge-m3           # 1024 维，中英多语，推荐中文场景

# 2) 启动 HouyiDB，把上游指向 Ollama（注意：不要带 /v1）
HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:11434 ./scripts/trial_start.sh

# 3) 自检（另一个终端）
EMBED_MODEL=nomic-embed-text ./scripts/embed_smoke.sh

# 4) 完整示例：切块 → 入库 → 检索
EMBED_MODEL=nomic-embed-text python3 examples/text_ingest.py
```

> Docker 部署时，容器访问宿主机 Ollama 用 `host.docker.internal`：
> `HOUYI_OPENAI_UPSTREAM=http://host.docker.internal:11434`。
> Linux 需加 `--add-host=host.docker.internal:host-gateway` 或改用宿主机网桥地址。

### 2.2 方式二：OpenAI（最快，需 key）

```bash
HOUYI_OPENAI_UPSTREAM=https://api.openai.com \
HOUYI_OPENAI_API_KEY=sk-... \
./scripts/trial_start.sh

EMBED_MODEL=text-embedding-3-small ./scripts/embed_smoke.sh
```

- key 只配置在服务端；客户端调用 HouyiDB 时**不需要** OpenAI key，也看不到它。
- 数据会发往上游；合规敏感场景改用 Ollama/TEI。
- `dimensions` 等 OpenAI 参数会原样转发，可裁剪维度。

### 2.3 方式三：TEI（自托管、GPU/CPU 均可）

```bash
docker run --rm -p 8080:80 ghcr.io/huggingface/text-embeddings-inference:latest \
  --model-id BAAI/bge-m3

HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:8080 ./scripts/trial_start.sh
EMBED_MODEL=BAAI/bge-m3 ./scripts/embed_smoke.sh
```

TEI 1.x 提供 OpenAI 兼容的 `/v1/embeddings`，网关可直接对接。

### 2.4 方式四：mock（只验链路，无语义）

```bash
python3 scripts/mock_embedding_server.py --port 18100
# 另一个终端：
HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:18100 ./scripts/trial_start.sh
EMBED_MODEL=mock ./scripts/embed_smoke.sh
```

mock 用文本哈希生成确定性向量：能验证网关/入库/检索链路，但**没有语义**
（“苹果”和“水果”不会更近）。真实效果必须换真实模型。

### 2.5 其它 OpenAI 兼容上游

| 上游 | base URL（不带 `/v1`） | key |
|---|---|---|
| vLLM | `http://127.0.0.1:8000` | 通常不需要 |
| DashScope 兼容模式 | `https://dashscope.aliyuncs.com/compatible-mode` | `HOUYI_OPENAI_API_KEY` |
| 任意 OpenAI 兼容服务 | `https://your-host` | 按需 |

---

## 3. 两种调用模式

所有请求都是 OpenAI 形状：`POST /v1/embeddings`。

### 3.1 纯代理（不写库）

不传 `collection` 字段：请求原样转发给上游，响应原样返回。适合自己管控写入。

```bash
curl -sS --noproxy '*' -X POST http://127.0.0.1:17000/v1/embeddings \
  -H 'Content-Type: application/json' \
  -d '{"model":"nomic-embed-text","input":"苹果是一种水果"}'
```

### 3.2 自动入库（推荐给快速试用）

传 `collection` 字段（HouyiDB 扩展；OpenAI SDK 用 `extra_body`）：

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:17000/v1", api_key="not-needed")
resp = client.embeddings.create(
    model="nomic-embed-text",
    input=["苹果是一种水果", "香蕉是黄色的", "HouyiDB 是向量数据库"],
    extra_body={"collection": "docs"},
)
print(resp.houyi)   # {'collection': 'docs', 'upserted': 3}
```

或 curl：

```bash
curl -sS --noproxy '*' -X POST http://127.0.0.1:17000/v1/embeddings \
  -H 'Content-Type: application/json' -d '{
  "model": "nomic-embed-text",
  "input": ["苹果是一种水果", "香蕉是黄色的"],
  "collection": "docs"
}'
# {"object":"list", ..., "houyi":{"collection":"docs","upserted":2}}
```

入库语义：

- id = `stable_id(text)`：blake2b-8 大端 u64 & (2^63−1)，与 Python/TS/Go 等
  SDK 一致；同一文本重复提交幂等，跨 SDK 可按同一 id 删除。
- 原文写入 payload `{"text": ...}`，检索时 `include_payload=true` 可取回。
- 自动创建 collection；维度由第一批向量决定，之后锁定。
- input 必须是字符串或字符串数组；token 数组返回 400。
- 单请求最多 2048 条（`MAX_INPUT_ITEMS`）；请求体上限 64MB。
- 上游错误的状态码与正文原样透传；上游 5xx 会计入熔断器。

---

## 4. 端到端示例：`examples/text_ingest.py`

```bash
# 使用内置示例文本，切块 → 入库 → 检索
EMBED_MODEL=nomic-embed-text python3 examples/text_ingest.py

# 摄入自己的文件
EMBED_MODEL=bge-m3 python3 examples/text_ingest.py \
  --file README.md --collection my_docs --chunk-size 300 --overlap 50

# 指定查询与 top-k
EMBED_MODEL=bge-m3 python3 examples/text_ingest.py \
  --file docs/USER_MANUAL.md --query "写入路径有什么特点？" --k 5
```

脚本做的事：

1. 读取文本（未指定 `--file` 时用内置示例）；
2. 按字符切块（`--chunk-size` / `--overlap`，仅演示）；
3. 批量调 `/v1/embeddings`（带 `collection`）自动入库；
4. 查询走纯代理模式取向量；
5. 调 `/v1/collections/{name}/search`，打印命中原文与分数。

**它是演示脚本，不是生产摄入管线**：字符切块不等于按 token/语义切块；
相同的文本块会因 `stable_id` 相同而互相覆盖；没有元数据字段。

---

## 5. 真实文档摄入：网关 vs 普通 upsert

| 需求 | 网关自动入库 | collection upsert + 客户端 embedding |
|---|---|---|
| 最少代码、纯文本、快速 demo | ✅ | 一般 |
| 自定义 id（业务主键） | ❌（id=文本哈希） | ✅ |
| 标题/来源/权限/时间等元数据 | ❌（只有 `text`） | ✅ |
| 相同文本块去重/多版本 | 需自行处理 | ✅ |
| 分块策略/token 计数 | ❌ | ✅（框架负责） |
| 模型由调用方选择/多模型 | 服务端全局上游 | ✅ |
| 大规模增量更新/删除重放 | 有限 | ✅ |

生产摄入的推荐形态：

```python
# 伪代码：embedding 在客户端/框架侧算，id/payload 自己掌控
vectors = embed_model.encode(chunks)
col.upsert([
    Vector(id=my_doc_id + chunk_no, values=v, payload={"text": c, "source": path})
    for c, v in zip(chunks, vectors)
])
```

LangChain / LlamaIndex 适配器已经在框架层委托 embedding（见
`sdk/python/langchain-houyidb`、`sdk/python/llama-index-vector-stores-houyidb`），
不需要数据库再做一次。

---

## 6. 模型选择速查

| 模型 | 维度 | 语言 | 部署 | 备注 |
|---|---|---|---|---|
| `nomic-embed-text` | 768 | 英文强 | Ollama | 小、快，入门首选 |
| `bge-m3` | 1024 | 中英多语 | Ollama/TEI | 中文推荐；支持长文本 |
| `bge-small-zh-v1.5` | 512 | 中文 | TEI/sentence-transformers | 更小更快 |
| `text-embedding-3-small` | 1536（可裁剪） | 多语 | OpenAI | 稳定、按量计费 |
| `text-embedding-3-large` | 3072（可裁剪） | 多语 | OpenAI | 质量更高、成本更高 |

选择要点：

- **维度不是越大越好**：显存/内存、检索延迟、索引大小都会上升；
- **同一 collection 只能用同一模型**：换模型 = 换向量空间 = 重建 collection；
- **中文场景别只用英文小模型**：语义质量差距会直接体现在召回上；
- 先用 `bge-m3` 或 `text-embedding-3-small` 建立基线，再按成本/延迟调整。

---

## 7. 限制与坑（务必阅读）

1. **模型/维度锁定**：collection 维度在首次写入时确定；换模型必须新建
   collection 重灌，跨模型的向量不可比较。
2. **同文本 = 同 id**：`stable_id(text)` 意味着内容完全相同的块会覆盖。
   文档中重复段落要加前缀（如标题）或改用自定义 id 的普通 upsert。
3. **只有 `text`**：网关不存标题/来源/权限等元数据；过滤检索需要这些字段时，
   走 collection upsert。
4. **没有 embedding 缓存**：同一文本重复请求会重复调用上游（计费/延迟）。
   upsert 是幂等的，但上游调用不会省。批量导入时先去重。
5. **上游 key 是服务端全局的**：所有客户端共享同一 key/配额；不要把它暴露给
   不可信客户端，网关本身要开 `HOUYI_AUTH_TOKEN`。
6. **超时/熔断**：上游慢会拖慢请求；`HOUYI_OPENAI_TIMEOUT_SECS`（默认 30s）
   可调，连续 5xx 会触发 503 快速失败。
7. **切块质量决定召回上限**：整篇长文直接 embedding 通常效果很差；生产请按
   token 切块并保留重叠。
8. **没有稀疏/BM25**：当前只有稠密向量；关键词精确匹配场景需要外部方案或
   后续版本。

---

## 8. 安全与隐私

- 云端上游（OpenAI/DashScope）意味着**文本会离开你的机器**；合规敏感场景用
  Ollama/TEI 本地推理。
- 上游 key 仅经服务端 ENV 注入，客户端调用 HouyiDB 时不接触该 key。
- 网关暴露在非本机网络时，务必启用 `HOUYI_AUTH_TOKEN`（或 TLS），否则任何人
  都能用你的上游配额并读写入库数据。
- `/v1/embeddings` 纯代理模式也需要认证（探针 `/v1/health`、`/readyz` 豁免）。

---

## 9. 排障

| 现象 | 原因 / 处理 |
|---|---|
| HTTP 501 | 未配置 `HOUYI_OPENAI_UPSTREAM`；启动时加上 |
| HTTP 502 / 上游非 JSON | 上游地址不对、不是 OpenAI 兼容、或模型名不存在 |
| HTTP 503 breaker open | 上游连续 5xx，熔断冷却中；先修上游 |
| 400 `token arrays are not supported` | 自动入库只接受字符串 input |
| 400 维度不符 | 查询/写入向量长度与 collection 首次写入维度不一致 |
| Docker 连不上 Ollama | 用 `host.docker.internal:11434`；Linux 加 host-gateway |
| 入库成功但搜不到 | 先确认 `pending_count`；再确认查询模型与入库模型一致 |
| 换模型后结果全乱 | 向量空间不兼容；新建 collection 重灌 |

---

## 10. 为什么不内置本地推理？

这是刻意的边界：

- 数据库的核心价值是索引、写入、检索性能；embedding 质量取决于模型，不是
  DB 能提升的；
- 本地推理需要模型运行时、权重分发、跨平台/GPU、版本升级与长期维护，
  这些负担与内核无关；
- 需要离线时，Ollama/TEI 已经是成熟的独立服务，数据库只需连过去；
- 把模型留在外部，模型可以独立升级、独立扩缩，数据库保持轻量。

如果确实需要「单机、无 sidecar、离线」的交付形态，正确做法是做一个独立的
`houyi-embed` sidecar（OpenAI 兼容），而不是把权重塞进 `houyi-server`。
