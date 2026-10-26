# houyidb-go

> [English](README.en.md)

HouyiDB 向量数据库的官方 Go 客户端。仅一个外部依赖（`golang.org/x/crypto`，用于跨语言一致的 `StableID`），Go ≥ 1.17。

```bash
go get github.com/houyidb-community/houyidb/sdk/go/houyidb
```

## 快速开始

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

    // 写入（collection 首次写入隐式创建，维度由首个向量定型）
    _, _ = col.Upsert(ctx, []houyidb.Vector{{
        ID:      houyidb.StableID("doc-1"),
        Values:  embedding, // []float64
        Payload: map[string]interface{}{"text": "...", "category": "fruit"},
    }})

    // 检索（Score 为相似度，越大越相似）
    result, _ := col.Search(ctx, queryEmbedding, 5,
        &houyidb.SearchOptions{IncludePayload: true})

    // 混合检索（Filters 取交集；Op: eq/gt/gte/lt/lte/in）
    filtered, _ := col.HybridSearch(ctx, queryEmbedding, 5, &houyidb.HybridSearchOptions{
        Filters:        []houyidb.Filter{{Column: "category", Op: houyidb.OpEq, Value: "fruit"}},
        IncludePayload: true,
    })
    _, _ = result, filtered

    // 删除（按 id 和/或过滤条件，幂等）
    _, _ = col.Delete(ctx, &houyidb.DeleteOptions{IDs: []uint64{houyidb.StableID("doc-1")}})
}
```

## 与其他语言 SDK 的对应

- 向量 id 是 `uint64`——Go 原生覆盖键空间，无 JS 的 2^53 精度问题；
- `StableID` 与 Python/TS SDK、服务端 OpenAI 网关四方同算法
  （BLAKE2b-8 大端 & 2^63-1）：同一字符串 id 在任何语言得到同一 u64，删除可跨 SDK 重放；
- 错误统一为 `*houyidb.HouyiError`（`StatusCode` + 服务端诊断文本），`errors.As` 可判。

## 测试

```bash
go test -race ./...   # StableID 四方对拍 + 集成（打 127.0.0.1:17000 真实服务，HOUYIDB_URL 可覆盖；不可达自动 Skip）
```

集成前置：`./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
