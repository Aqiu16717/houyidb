# houyidb-client

> [English](README.en.md)

HouyiDB 向量数据库的官方 Rust 客户端（collections HTTP API）。同步阻塞式（`ureq` + rustls），仅 4 个直接依赖，与服务端共用同一套 `stable_id` 算法与依赖版本。

```toml
[dependencies]
houyidb-client = { path = "sdk/rust/houyidb-client" }
```

## 快速开始

```rust
use houyidb_client::{stable_id, Client, Filter, FilterOp, SearchOptions, Vector};
use std::collections::HashMap;

let client = Client::new("http://127.0.0.1:7000");
let col = client.collection("docs");

// 写入（collection 首次写入隐式创建，维度由首个向量定型）
col.upsert(&[Vector {
    id: stable_id("doc-1"),
    values: embedding, // Vec<f32>
    label: None,
    payload: Some(HashMap::from([
        ("text".into(), "...".into()),
        ("category".into(), "fruit".into()),
    ])),
}])?;

// 检索（score 为相似度 = 1 - cosine_distance，越大越相似）
let result = col.search(&query, 5, &SearchOptions {
    include_payload: true,
    ..Default::default()
})?;

// 混合检索（filters 取交集；op: eq/gt/gte/lt/lte/in）
let filtered = col.hybrid_search(&query, 5, &SearchOptions {
    filters: vec![Filter::new("category", FilterOp::Eq, "fruit")],
    include_payload: true,
    ..Default::default()
})?;

// 删除（按 id 和/或过滤条件，幂等；两者取并集）
col.delete(&[stable_id("doc-1")], &[])?;
```

## 与其他语言 SDK 的对应

- 向量 id 是 `u64`——原生覆盖键空间，无 JS 的 2^53 精度问题；
- `stable_id` 与 Python/TS/Go SDK、服务端 OpenAI 网关五方同算法
  （BLAKE2b-8 大端 & 2^63-1，digest 长度是算法参数而非截断）：同一字符串 id
  在任何语言得到同一 u64，删除可跨 SDK 重放；
- 错误统一为 `HouyiError`：`Http { status, message }`（服务端纯文本诊断原样透出）
  或 `Transport`（连接/读取失败）；
- collection 名经 RFC 3986 路径转义（与 Python `quote(safe="")` 同口径），
  含 `/ ? #` 或中文的名字不会改写路由。

## 测试

```bash
cargo test -p houyidb-client            # 单元（stable_id 五方对拍）+ 集成
```

集成测试打 127.0.0.1:17000 真实服务（`HOUYIDB_URL` 可覆盖；不可达自动跳过）。
前置：`./target/release/houyi-server --benchmark-http-addr 127.0.0.1:17000 --wal-path /tmp/houyidb-eco.wal`
