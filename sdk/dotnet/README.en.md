# houyidb-dotnet

> [中文](README.md)

The official .NET client for the HouyiDB vector database (collections HTTP API). Fully asynchronous with
async/await, targeting net8.0 + net10.0; the only external dependency is `Konscious.Security.Cryptography.Blake2`
(for the cross-language-consistent `StableId`).

```bash
dotnet add package HouyiDb
```

## Quick Start

```csharp
using HouyiDb;

var client = new HouyiClient("http://127.0.0.1:7000");
var col = client.GetCollection("docs");

// Write (the collection is created implicitly on first write; dimensionality is fixed by the first vector)
await col.UpsertAsync(new[]
{
    new Vector
    {
        Id = StableId.Of("doc-1"),
        Values = embedding, // float[]
        Payload = new Dictionary<string, object?>
        {
            ["text"] = "...",
            ["category"] = "fruit",
        },
    },
});

// Search (Score is similarity = 1 - cosine_distance; higher means more similar)
var result = await col.SearchAsync(queryEmbedding, 5,
    new SearchOptions { IncludePayload = true });

// Hybrid search (Filters are intersected; FilterOp: Eq/Gt/Gte/Lt/Lte/In)
var options = new SearchOptions { IncludePayload = true };
options.Filters.Add(new Filter("category", FilterOp.Eq, "fruit"));
var filtered = await col.HybridSearchAsync(queryEmbedding, 5, options);

// Delete (by id and/or filter conditions, idempotent; the two are unioned)
await col.DeleteAsync(ids: new[] { StableId.Of("doc-1") });
```

## Mapping to Other Language SDKs

- Vector ids are `ulong`—native keyspace coverage, avoiding JS's 2^53 precision issue;
- `StableId.Of` uses the same algorithm as the Python/TS/Go/Rust SDKs and the server OpenAI gateway (five parties;
  BLAKE2b-8 big-endian & 2^63-1; `HMACBlake2B(64)` without a key is standard BLAKE2b,
  and the digest length is an algorithm parameter, not truncation): the same string id yields the same u64 in any language,
  so deletes can be replayed across SDKs;
- Non-2xx server responses uniformly raise `HouyiException` (`StatusCode` + server diagnostic text);
- Collection names are path-escaped per RFC 3986 (`Uri.EscapeDataString`, same semantics as Python
  `quote(safe="")`), so names containing `/ ? #` or Chinese characters do not alter routing.

## Tests

```bash
dotnet test   # StableId 五方对拍 + 集成（打 127.0.0.1:17000 真实服务，
              # HOUYIDB_URL can override; skipped automatically when unreachable)
```

Integration prerequisite: `./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
