# Embedding Integration Guide

> [中文](EMBEDDING_GUIDE.md)

> Goal: text in, vectors out, automatic ingestion, searchable — without pushing
> model inference into the database core.
>
> Applies to: v0.2.0-beta.4+. The core **does not** perform embedding inference;
> HouyiDB connects to external models (OpenAI / Ollama / TEI / vLLM / DashScope,
> etc.) through an OpenAI-compatible `/v1/embeddings` gateway, and can
> optionally **write the returned vectors into a collection automatically**.

---

## 1. Choose a Path First

| Your situation | Recommended approach | Need to change the database? |
|---|---|---|
| Have an OpenAI / DashScope API key | Connect the gateway directly to the cloud | No |
| Require offline operation / data must not leave the intranet | Local Ollama or TEI + gateway | No |
| Only validating the pipeline, no model available | Built-in mock embedding | No |
| Documents carry metadata such as business id / title / permissions | collection upsert + client-side vector computation | No (use the normal write API) |
| Need chunking, incremental updates, model-upgrade rebuilds | Client-side ingestion pipeline (LangChain/LlamaIndex, etc.) | No |

**Conclusion**: 90% of "embedding" needs can be solved at the gateway layer
without modifying HouyiDB.

---

## 2. Get It Running in Five Minutes

### 2.1 Option One: Ollama (recommended; offline and free)

```bash
# 1) Install Ollama, then pull an embedding model (any one)
ollama pull nomic-embed-text      # 768 维，英文强
# Or: ollama pull bge-m3           # 1024 dims, Chinese-English multilingual, recommended for Chinese scenarios

# 2) Start HouyiDB and point the upstream at Ollama (note: do not include /v1)
HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:11434 ./scripts/trial_start.sh

# 3) Self-check (another terminal)
EMBED_MODEL=nomic-embed-text ./scripts/embed_smoke.sh

# 4) Full example: chunk → ingest → search
EMBED_MODEL=nomic-embed-text python3 examples/text_ingest.py
```

> When deployed with Docker, containers reach the host's Ollama via
> `host.docker.internal`:
> `HOUYI_OPENAI_UPSTREAM=http://host.docker.internal:11434`.
> On Linux, add `--add-host=host.docker.internal:host-gateway` or use the host
> bridge address instead.

### 2.2 Option Two: OpenAI (fastest; requires a key)

```bash
HOUYI_OPENAI_UPSTREAM=https://api.openai.com \
HOUYI_OPENAI_API_KEY=sk-... \
./scripts/trial_start.sh

EMBED_MODEL=text-embedding-3-small ./scripts/embed_smoke.sh
```

- The key is configured only on the server; clients calling HouyiDB **do not
  need** the OpenAI key and cannot see it.
- Data is sent to the upstream; for compliance-sensitive scenarios, use
  Ollama/TEI instead.
- OpenAI parameters such as `dimensions` are forwarded as-is, allowing
  dimensionality reduction.

### 2.3 Option Three: TEI (self-hosted; GPU or CPU)

```bash
docker run --rm -p 8080:80 ghcr.io/huggingface/text-embeddings-inference:latest \
  --model-id BAAI/bge-m3

HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:8080 ./scripts/trial_start.sh
EMBED_MODEL=BAAI/bge-m3 ./scripts/embed_smoke.sh
```

TEI 1.x provides an OpenAI-compatible `/v1/embeddings`, so the gateway can
connect to it directly.

### 2.4 Option Four: mock (pipeline validation only, no semantics)

```bash
python3 scripts/mock_embedding_server.py --port 18100
# Another terminal:
HOUYI_OPENAI_UPSTREAM=http://127.0.0.1:18100 ./scripts/trial_start.sh
EMBED_MODEL=mock ./scripts/embed_smoke.sh
```

The mock generates deterministic vectors by hashing the text: it validates the
gateway/ingestion/search pipeline, but is **not semantic** ("apple" and "fruit"
will not be closer). For real results you must switch to a real model.

### 2.5 Other OpenAI-Compatible Upstreams

| Upstream | base URL (without `/v1`) | key |
|---|---|---|
| vLLM | `http://127.0.0.1:8000` | Usually not required |
| DashScope compatibility mode | `https://dashscope.aliyuncs.com/compatible-mode` | `HOUYI_OPENAI_API_KEY` |
| Any OpenAI-compatible service | `https://your-host` | As needed |

---

## 3. Two Calling Modes

All requests follow the OpenAI shape: `POST /v1/embeddings`.

### 3.1 Pure Proxy (no database writes)

Omit the `collection` field: the request is forwarded to the upstream as-is, and
the response is returned as-is. Suitable when you want to control writes
yourself.

```bash
curl -sS --noproxy '*' -X POST http://127.0.0.1:17000/v1/embeddings \
  -H 'Content-Type: application/json' \
  -d '{"model":"nomic-embed-text","input":"苹果是一种水果"}'
```

### 3.2 Automatic Ingestion (recommended for quick trials)

Pass the `collection` field (a HouyiDB extension; with the OpenAI SDK, use
`extra_body`):

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

Or with curl:

```bash
curl -sS --noproxy '*' -X POST http://127.0.0.1:17000/v1/embeddings \
  -H 'Content-Type: application/json' -d '{
  "model": "nomic-embed-text",
  "input": ["苹果是一种水果", "香蕉是黄色的"],
  "collection": "docs"
}'
# {"object":"list", ..., "houyi":{"collection":"docs","upserted":2}}
```

Ingestion semantics:

- id = `stable_id(text)`: blake2b-8 big-endian u64 & (2^63−1), consistent with
  the Python/TS/Go SDKs; resubmitting the same text is idempotent, and the same
  id can be deleted across SDKs.
- The original text is written into payload `{"text": ...}` and can be retrieved
  with `include_payload=true` during search.
- The collection is created automatically; dimensionality is determined by the
  first batch of vectors and locked afterward.
- input must be a string or an array of strings; token arrays return 400.
- At most 2048 items per request (`MAX_INPUT_ITEMS`); request body limit is 64MB.
- Upstream error status codes and bodies are passed through as-is; upstream 5xx
  responses count toward the circuit breaker.

---

## 4. End-to-End Example: `examples/text_ingest.py`

```bash
# Use the built-in sample text: chunk → ingest → search
EMBED_MODEL=nomic-embed-text python3 examples/text_ingest.py

# Ingest your own file
EMBED_MODEL=bge-m3 python3 examples/text_ingest.py \
  --file README.md --collection my_docs --chunk-size 300 --overlap 50

# Specify the query and top-k
EMBED_MODEL=bge-m3 python3 examples/text_ingest.py \
  --file docs/USER_MANUAL.md --query "写入路径有什么特点？" --k 5
```

What the script does:

1. Reads the text (uses the built-in sample when `--file` is not specified);
2. Chunks by characters (`--chunk-size` / `--overlap`, demo only);
3. Calls `/v1/embeddings` in batches (with `collection`) for automatic
   ingestion;
4. Obtains query vectors through pure proxy mode;
5. Calls `/v1/collections/{name}/search` and prints the matched original text
   and scores.

**It is a demo script, not a production ingestion pipeline**: character
chunking is not token/semantic chunking; identical text chunks overwrite each
other because their `stable_id` values are the same; there are no metadata
fields.

---

## 5. Real Document Ingestion: Gateway vs Plain upsert

| Requirement | Gateway automatic ingestion | collection upsert + client-side embedding |
|---|---|---|
| Minimal code, plain text, quick demo | ✅ | Fair |
| Custom id (business primary key) | ❌ (id = text hash) | ✅ |
| Metadata such as title/source/permissions/time | ❌ (only `text`) | ✅ |
| Deduplication of identical text chunks / multiple versions | Handle it yourself | ✅ |
| Chunking strategy / token counting | ❌ | ✅ (handled by the framework) |
| Model chosen by the caller / multiple models | Server-wide global upstream | ✅ |
| Large-scale incremental updates / delete-and-replay | Limited | ✅ |

Recommended shape for production ingestion:

```python
# Pseudo-code: compute embeddings on the client/framework side and control id/payload yourself
vectors = embed_model.encode(chunks)
col.upsert([
    Vector(id=my_doc_id + chunk_no, values=v, payload={"text": c, "source": path})
    for c, v in zip(chunks, vectors)
])
```

The LangChain / LlamaIndex adapters already delegate embedding at the framework
layer (see `sdk/python/langchain-houyidb` and
`sdk/python/llama-index-vector-stores-houyidb`), so the database does not need
to do it again.

---

## 6. Model Selection Quick Reference

| Model | Dimensions | Language | Deployment | Notes |
|---|---|---|---|---|
| `nomic-embed-text` | 768 | Strong English | Ollama | Small and fast; the go-to starter |
| `bge-m3` | 1024 | Chinese/English multilingual | Ollama/TEI | Recommended for Chinese; supports long texts |
| `bge-small-zh-v1.5` | 512 | Chinese | TEI/sentence-transformers | Smaller and faster |
| `text-embedding-3-small` | 1536 (trimmable) | Multilingual | OpenAI | Stable; pay-as-you-go |
| `text-embedding-3-large` | 3072 (trimmable) | Multilingual | OpenAI | Higher quality, higher cost |

Selection tips:

- **Bigger dimensions are not always better**: VRAM/RAM usage, search latency,
  and index size all increase;
- **A collection can use only one model**: changing the model means changing the
  vector space, which means rebuilding the collection;
- **For Chinese scenarios, do not rely solely on a small English model**: the
  semantic quality gap shows up directly in recall;
- Start with `bge-m3` or `text-embedding-3-small` to establish a baseline, then
  adjust based on cost/latency.

---

## 7. Limitations and Pitfalls (must read)

1. **Model/dimension lock-in**: a collection's dimensionality is fixed at first
   write; changing models requires creating a new collection and reloading the
   data, since vectors from different models are not comparable.
2. **Same text = same id**: `stable_id(text)` means chunks with identical
   content overwrite each other. For repeated paragraphs in documents, add a
   prefix (such as the title) or switch to plain upsert with custom ids.
3. **Only `text`**: the gateway does not store metadata such as title, source,
   or permissions; when filtered search needs these fields, use collection
   upsert.
4. **No embedding cache**: repeated requests for the same text call the upstream
   again (cost/latency). upsert is idempotent, but upstream calls are not saved.
   Deduplicate before bulk imports.
5. **The upstream key is server-global**: all clients share the same key/quota;
   do not expose it to untrusted clients — enable `HOUYI_AUTH_TOKEN` on the
   gateway itself.
6. **Timeouts/circuit breaking**: a slow upstream slows requests;
   `HOUYI_OPENAI_TIMEOUT_SECS` (default 30s) is adjustable, and consecutive 5xx
   responses trigger fast-fail 503s.
7. **Chunking quality sets the recall ceiling**: embedding an entire long
   document directly usually performs poorly; in production, chunk by token and
   keep overlap.
8. **No sparse/BM25**: only dense vectors are available for now; exact keyword
   matching scenarios need an external solution or a future version.

---

## 8. Security and Privacy

- Cloud upstreams (OpenAI/DashScope) mean **your text leaves your machine**; for
  compliance-sensitive scenarios, use Ollama/TEI local inference.
- The upstream key is injected only through server-side ENV; clients calling
  HouyiDB never touch the key.
- When the gateway is exposed beyond the local machine, be sure to enable
  `HOUYI_AUTH_TOKEN` (or TLS); otherwise anyone can use your upstream quota and
  read or write ingested data.
- `/v1/embeddings` in pure proxy mode also requires authentication (the
  `/v1/health` and `/readyz` probes are exempt).

---

## 9. Troubleshooting

| Symptom | Cause / Resolution |
|---|---|
| HTTP 501 | `HOUYI_OPENAI_UPSTREAM` is not configured; add it at startup |
| HTTP 502 / upstream returns non-JSON | Wrong upstream address, not OpenAI-compatible, or the model name does not exist |
| HTTP 503 breaker open | Consecutive upstream 5xx; the circuit breaker is cooling down; fix the upstream first |
| 400 `token arrays are not supported` | Automatic ingestion accepts only string input |
| 400 dimension mismatch | The query/write vector length does not match the collection's first-write dimensionality |
| Docker cannot reach Ollama | Use `host.docker.internal:11434`; on Linux add host-gateway |
| Ingestion succeeds but search finds nothing | First check `pending_count`; then confirm the query model matches the ingestion model |
| Results are chaotic after changing models | Incompatible vector spaces; create a new collection and reload |

---

## 10. Why Not Build In Local Inference?

This is a deliberate boundary:

- A database's core value is indexing, writes, and search performance; embedding
  quality depends on the model, and the DB cannot improve it;
- Local inference requires a model runtime, weight distribution,
  cross-platform/GPU support, version upgrades, and long-term maintenance —
  burdens unrelated to the kernel;
- When offline operation is needed, Ollama/TEI are already mature standalone
  services; the database only needs to connect to them;
- Keeping models external lets them upgrade and scale independently while the
  database stays lightweight.

If you truly need a "single-machine, no sidecar, offline" delivery form, the
right approach is to build a standalone `houyi-embed` sidecar
(OpenAI-compatible) rather than stuffing weights into `houyi-server`.
