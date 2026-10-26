# houyidb-java

> [English](README.en.md)

HouyiDB 向量数据库的官方 Java 客户端（collections HTTP API）。同步阻塞式
（`java.net.http.HttpClient`），Java 17+，仅两个外部依赖：BouncyCastle
（跨语言一致的 `StableId`）与 Jackson（JSON）。

```groovy
dependencies {
    implementation 'top.houyidb:houyidb-client:0.2.0-beta.5'
}
```

## 快速开始

```java
import com.houyidb.client.*;
import com.houyidb.client.Types.*;

var client = new HouyiClient("http://127.0.0.1:7000");
var col = client.collection("docs");

// 写入（collection 首次写入隐式创建，维度由首个向量定型）
col.upsert(List.of(new Vector(
        StableId.of("doc-1"),
        embedding, // double[]
        Map.of("text", "...", "category", "fruit"))));

// 检索（score 为相似度 = 1 - cosine_distance，越大越相似）
SearchResult result = col.search(queryEmbedding, 5,
        new SearchOptions().includePayload(true));

// 混合检索（filters 取交集；FilterOp: EQ/GT/GTE/LT/LTE/IN）
SearchResult filtered = col.hybridSearch(queryEmbedding, 5,
        new SearchOptions().includePayload(true)
                .filter("category", FilterOp.EQ, "fruit"));

// 删除（按 id 和/或过滤条件，幂等；两者取并集）
col.delete(List.of(StableId.of("doc-1")), null);
```

## 与其他语言 SDK 的对应

- 向量 id 是 `long`——stable_id 落在 [0, 2^63-1]，有符号 long 无损承载，
  无 JS 的 2^53 精度问题；
- `StableId.of` 与 Python/TS/Go/Rust/.NET SDK、服务端 OpenAI 网关六方同算法
  （BLAKE2b-8 大端 & 2^63-1；`Blake2bDigest(64)` 的 digest 长度是算法参数
  而非截断）：同一字符串 id 在任何语言得到同一 u64，删除可跨 SDK 重放；
- 服务端非 2xx 统一抛 `HouyiException`（`statusCode()` + 服务端诊断文本），
  传输层失败仍为 `IOException`；
- collection 名经 RFC 3986 路径转义（与 Python `quote(safe="")` 同口径），
  含 `/ ? #` 或中文的名字不会改写路由。

## 测试

```bash
gradle test   # StableId 六方对拍 + 路径转义 + 集成（打 127.0.0.1:17000 真实服务，
              # HOUYIDB_URL 可覆盖；不可达经 JUnit Assumptions 自动跳过）
```

集成前置：`./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
