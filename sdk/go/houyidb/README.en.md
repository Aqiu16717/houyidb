# houyidb-go

> [中文](README.md)

The official Go client for the HouyiDB vector database. Only one external dependency (`golang.org/x/crypto`, for the cross-language-consistent `StableID`); Go ≥ 1.17.

```bash
go get github.com/houyidb-community/houyidb/sdk/go/houyidb
```

## Quick Start

```go
package main

import (
    "context"

    houyidb "github.com/houyidb-community/houyidb/sdk/go/houyidb"
)

func main() {
    ctx := context.Background()
    client := houyidb.NewClient("http://127.0.0.1:7000")
    col := client.Collection("docs")

    // Write (the collection is created implicitly on first write; dimensionality is fixed by the first vector)
    _, _ = col.Upsert(ctx, []houyidb.Vector{{
        ID:      houyidb.StableID("doc-1"),
        Values:  embedding, // []float64
        Payload: map[string]interface{}{"text": "...", "category": "fruit"},
    }})

    // Search (Score is similarity; higher means more similar)
    result, _ := col.Search(ctx, queryEmbedding, 5,
        &houyidb.SearchOptions{IncludePayload: true})

    // Hybrid search (Filters are intersected; Op: eq/gt/gte/lt/lte/in)
    filtered, _ := col.HybridSearch(ctx, queryEmbedding, 5, &houyidb.HybridSearchOptions{
        Filters:        []houyidb.Filter{{Column: "category", Op: houyidb.OpEq, Value: "fruit"}},
        IncludePayload: true,
    })
    _, _ = result, filtered

    // Delete (by id and/or filter conditions, idempotent)
    _, _ = col.Delete(ctx, &houyidb.DeleteOptions{IDs: []uint64{houyidb.StableID("doc-1")}})
}
```

## Mapping to Other Language SDKs

- Vector ids are `uint64`—Go natively covers the keyspace, avoiding JS's 2^53 precision issue;
- `StableID` uses the same algorithm as the Python/TS SDKs and the server OpenAI gateway
  (four parties; BLAKE2b-8 big-endian & 2^63-1): the same string id yields the same u64 in any language, so deletes can be replayed across SDKs;
- Errors are uniformly `*houyidb.HouyiError` (`StatusCode` + server diagnostic text) and can be detected with `errors.As`.

## Tests

```bash
go test -race ./...   # StableID 四方对拍 + 集成（打 127.0.0.1:17000 真实服务，HOUYIDB_URL 可覆盖；不可达自动 Skip）
```

Integration prerequisite: `./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
