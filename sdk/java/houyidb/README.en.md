# houyidb-java

> [中文](README.md)

The official Java client for the HouyiDB vector database (collections HTTP API). Synchronous blocking
(`java.net.http.HttpClient`), Java 17+, with only two external dependencies: BouncyCastle
(cross-language-consistent `StableId`) and Jackson (JSON).

```groovy
dependencies {
    implementation 'top.houyidb:houyidb-client:0.2.0-beta.5'
}
```

## Quick Start

```java
import com.houyidb.client.*;
import com.houyidb.client.Types.*;

var client = new HouyiClient("http://127.0.0.1:7000");
var col = client.collection("docs");

// Write (the collection is created implicitly on first write; dimensionality is fixed by the first vector)
col.upsert(List.of(new Vector(
        StableId.of("doc-1"),
        embedding, // double[]
        Map.of("text", "...", "category", "fruit"))));

// Search (score is similarity = 1 - cosine_distance; higher means more similar)
SearchResult result = col.search(queryEmbedding, 5,
        new SearchOptions().includePayload(true));

// Hybrid search (filters are intersected; FilterOp: EQ/GT/GTE/LT/LTE/IN)
SearchResult filtered = col.hybridSearch(queryEmbedding, 5,
        new SearchOptions().includePayload(true)
                .filter("category", FilterOp.EQ, "fruit"));

// Delete (by id and/or filter conditions, idempotent; the two are unioned)
col.delete(List.of(StableId.of("doc-1")), null);
```

## Mapping to Other Language SDKs

- Vector ids are `long`—stable_id falls within [0, 2^63-1], carried losslessly by signed long,
  avoiding JS's 2^53 precision issue;
- `StableId.of` uses the same algorithm as the Python/TS/Go/Rust/.NET SDKs and the server OpenAI gateway (six parties;
  BLAKE2b-8 big-endian & 2^63-1; the digest length of `Blake2bDigest(64)` is an algorithm parameter,
  not truncation): the same string id yields the same u64 in any language, so deletes can be replayed across SDKs;
- Non-2xx server responses uniformly raise `HouyiException` (`statusCode()` + server diagnostic text),
  while transport-layer failures remain `IOException`;
- Collection names are path-escaped per RFC 3986 (same semantics as Python `quote(safe="")`),
  so names containing `/ ? #` or Chinese characters do not alter routing.

## Tests

```bash
gradle test   # StableId 六方对拍 + 路径转义 + 集成（打 127.0.0.1:17000 真实服务，
              # HOUYIDB_URL can override; skipped automatically via JUnit Assumptions when unreachable)
```

Integration prerequisite: `./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
