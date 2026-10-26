# houyidb-dotnet

> [English](README.en.md)

HouyiDB 向量数据库的官方 .NET 客户端（collections HTTP API）。async/await 全异步，
目标框架 net8.0 + net10.0，唯一外部依赖是 `Konscious.Security.Cryptography.Blake2`
（用于跨语言一致的 `StableId`）。

```bash
dotnet add package HouyiDb
```

## 快速开始

```csharp
using HouyiDb;

var client = new HouyiClient("http://127.0.0.1:7000");
var col = client.GetCollection("docs");

// 写入（collection 首次写入隐式创建，维度由首个向量定型）
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

// 检索（Score 为相似度 = 1 - cosine_distance，越大越相似）
var result = await col.SearchAsync(queryEmbedding, 5,
    new SearchOptions { IncludePayload = true });

// 混合检索（Filters 取交集；FilterOp: Eq/Gt/Gte/Lt/Lte/In）
var options = new SearchOptions { IncludePayload = true };
options.Filters.Add(new Filter("category", FilterOp.Eq, "fruit"));
var filtered = await col.HybridSearchAsync(queryEmbedding, 5, options);

// 删除（按 id 和/或过滤条件，幂等；两者取并集）
await col.DeleteAsync(ids: new[] { StableId.Of("doc-1") });
```

## 与其他语言 SDK 的对应

- 向量 id 是 `ulong`——原生覆盖键空间，无 JS 的 2^53 精度问题；
- `StableId.Of` 与 Python/TS/Go/Rust SDK、服务端 OpenAI 网关五方同算法
  （BLAKE2b-8 大端 & 2^63-1；`HMACBlake2B(64)` 无 key 构造即标准 BLAKE2b，
  digest 长度是算法参数而非截断）：同一字符串 id 在任何语言得到同一 u64，
  删除可跨 SDK 重放；
- 服务端非 2xx 统一抛 `HouyiException`（`StatusCode` + 服务端诊断文本）；
- collection 名经 RFC 3986 路径转义（`Uri.EscapeDataString`，与 Python
  `quote(safe="")` 同口径），含 `/ ? #` 或中文的名字不会改写路由。

## 测试

```bash
dotnet test   # StableId 五方对拍 + 集成（打 127.0.0.1:17000 真实服务，
              # HOUYIDB_URL 可覆盖；不可达自动跳过）
```

集成前置：`./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
