# HouyiDB Mobile 使用手册

> [English](HOUYIDB_MOBILE_MANUAL.en.md)

> 轻量级向量索引，面向 iOS / Android 原生集成。  
> C ABI 接口，编译产物为静态库 (`.a`) 或动态库 (`.so` / `.dylib`)。

---

## 1. 构建

### 1.1 编译静态库

```bash
# macOS / iOS (aarch64)
cargo build --release -p houyi-mobile --target aarch64-apple-darwin
ls target/release/libhouyi_mobile.a   # ~19MB (release LTO)

# iOS (交叉编译，需 Xcode)
cargo build --release -p houyi-mobile --target aarch64-apple-ios

# Android (交叉编译，需 NDK)
cargo build --release -p houyi-mobile --target aarch64-linux-android
```

### 1.2 链接到项目

- **iOS**: 将 `libhouyi_mobile.a` 拖入 Xcode，添加 `libresolv.tbd` 依赖
- **Android**: 放入 `app/src/main/jniLibs/arm64-v8a/`，通过 JNI 调用

---

## 2. C API 参考

### 2.1 创建索引

```c
void* houyi_mobile_create(uint32_t dims, const char* index_type);
```

| 参数 | 说明 |
|------|------|
| `dims` | 向量维度 (如 128, 768, 1536) |
| `index_type` | `"flat"` / `"flat-clustered"` / `"clustered-hnsw"` |
| 返回 | 索引句柄 (NULL 表示失败) |

**三种索引类型**：

| 类型 | 内存 | 延迟 | Recall | 适用 |
|------|------|------|--------|------|
| `flat` | 无压缩 | O(N) | 1.0 (精确) | N < 5K, 离线评估 |
| `flat-clustered` | 无压缩 | < 1ms | > 0.95 | **推荐移动端默认** |
| `clustered-hnsw` | 图结构 | ~1ms | > 0.99 | N > 50K, 高 recall |

### 2.2 插入向量

```c
int32_t houyi_mobile_insert(void* handle, uint64_t id, const float* vector, uint32_t len);
```

| 参数 | 说明 |
|------|------|
| `handle` | 索引句柄 |
| `id` | 向量唯一标识 |
| `vector` | 浮点数组指针 |
| `len` | 数组长度 (必须 = dims) |
| 返回 | 0 成功, -1 失败 |

支持幂等覆盖：同一 `id` 重复插入时覆盖旧值。

### 2.3 构建索引

```c
uint32_t houyi_mobile_build(void* handle);
```

> 仅 `flat-clustered` 和 `clustered-hnsw` 需要调用。`flat` 模式下无操作。

返回已索引的向量总数。

### 2.4 检索

```c
uint32_t houyi_mobile_search(
    void* handle,
    const float* query,
    uint32_t dims,
    uint32_t k,
    float* results,      // 输出缓冲区 [id0, dist0, id1, dist1, ...]
    uint32_t max_results  // 缓冲区容量 (最多返回 max_results 个)
);
```

| 参数 | 说明 |
|------|------|
| `results` | 预分配缓冲区，每结果占 2 个 float (id + distance) |
| `max_results` | results 数组可容纳的最大结果数 (≥ k) |
| 返回 | 实际写入的结果数 |

### 2.5 查询数量

```c
uint32_t houyi_mobile_count(void* handle);
```

### 2.6 释放索引

```c
void houyi_mobile_destroy(void* handle);
```

---

## 3. 平台示例

### 3.1 Swift (iOS)

```swift
import Foundation

class HouyiDB {
    private var handle: UnsafeMutableRawPointer?
    let dims: UInt32 = 128

    init?(indexType: String = "flat-clustered") {
        guard let h = houyi_mobile_create(dims, indexType.cString(using: .utf8)) else {
            return nil
        }
        self.handle = h
    }

    func insert(id: UInt64, vector: [Float]) {
        guard let h = handle, vector.count == dims else { return }
        houyi_mobile_insert(h, id, vector, dims)
    }

    func build() -> UInt32 {
        guard let h = handle else { return 0 }
        return houyi_mobile_build(h)
    }

    func search(query: [Float], k: UInt32 = 10) -> [(UInt64, Float)] {
        guard let h = handle, query.count == dims else { return [] }
        var results = [Float](repeating: 0, count: Int(k * 2))
        let n = houyi_mobile_search(h, query, dims, k, &results, k)
        var out: [(UInt64, Float)] = []
        for i in 0..<Int(n) {
            out.append((UInt64(results[i*2]), results[i*2+1]))
        }
        return out
    }

    deinit {
        if let h = handle { houyi_mobile_destroy(h) }
    }
}

// 使用
let db = HouyiDB(indexType: "flat-clustered")!
db.insert(id: 1, vector: [/* 128 floats */])
db.insert(id: 2, vector: [/* 128 floats */])
let count = db.build()
print("Indexed \(count) vectors")

let results = db.search(query: [/* 128 floats */], k: 10)
for (id, dist) in results {
    print("id=\(id) dist=\(dist)")
}
```

### 3.2 Kotlin (Android JNI)

```kotlin
class HouyiDB(private val dims: Int, indexType: String = "flat-clustered") {
    private var handle: Long = 0

    companion object {
        init { System.loadLibrary("houyi_mobile") }
    }

    // JNI declarations
    private external fun nativeCreate(dims: Int, type: String): Long
    private external fun nativeInsert(handle: Long, id: Long, vector: FloatArray): Int
    private external fun nativeBuild(handle: Long): Int
    private external fun nativeSearch(handle: Long, query: FloatArray, k: Int): FloatArray
    private external fun nativeDestroy(handle: Long)

    init {
        handle = nativeCreate(dims, indexType)
        if (handle == 0L) throw RuntimeException("Failed to create index")
    }

    fun insert(id: Long, vector: FloatArray) {
        require(vector.size == dims)
        nativeInsert(handle, id, vector)
    }

    fun build(): Int = nativeBuild(handle)

    fun search(query: FloatArray, k: Int = 10): List<Pair<Long, Float>> {
        val raw = nativeSearch(handle, query, k)
        return raw.toList().chunked(2).map { Pair(it[0].toLong(), it[1]) }
    }

    protected fun finalize() { nativeDestroy(handle) }
}
```

---

## 4. 完整流程

```
1. houyi_mobile_create(128, "flat-clustered")   // 创建索引
2. houyi_mobile_insert(...) × N                  // 插入 N 个向量
3. houyi_mobile_build(...)                       // 构建聚类索引
4. houyi_mobile_search(...) × M                  // M 次检索
5. houyi_mobile_destroy(...)                     // 释放内存
```

---

## 5. 性能调参

`flat-clustered` 模式的关键参数（编译前修改 `houyi-vector/src/cluster/mod.rs`）：

| 参数 | 默认 | 作用 |
|------|------|------|
| `ClusterConfig::kmeans_iters` | 15 | K-Means 迭代次数，越大聚类越精确 |
| `ClusterConfig::hnsw_ef_search` | 256 | 检索候选宽度 (仅 clustered-hnsw) |
| `PqConfig::m` | dims/16 | PQ 段数，越少精度越高但压缩率越低 |
| `PqConfig::k` | 256 | 每段码本大小 |

---

## 6. 与 MicroNN 对标

| 指标 | MicroNN | HouyiDB Mobile |
|------|:---:|:---:|
| 延迟 | < 7 ms | **3.93 ms** |
| Recall@100 | ≥ 90% | **0.937** |
| RAM | ~10 MB | **15.4 MB** |
| 向量存储 | 磁盘 | 磁盘 (mmap) |
| 接口 | Swift/ObjC | C ABI (Swift/Kotlin/Flutter) |

---

## 7. 混合检索 (向量 + 结构化过滤)

### 7.1 Swift 示例

```swift
let db = houyi_mobile_db_open()

// 创建 Collection
houyi_mobile_db_collection(db, "photos", 512, "flat-clustered")

// 插入向量 + 标量标签
houyi_mobile_db_insert(db, "photos", 1, vector, 512)
houyi_mobile_db_insert_scalars(db, "photos", 
    "[{\"col\":\"tag\",\"val\":\"text:beach\"},{\"col\":\"date\",\"val\":\"ts:1717200000\"}]")

// 混合检索: tag=beach AND date>2024-06-01
// filters_json: [{"col":"tag","op":"eq","val":"text:beach"},{"col":"date","op":"gt","val":"ts:1717200000"}]
houyi_mobile_db_search_hybrid(db, "photos", query, 512, 10, filters_json, results, 10)
```

### 7.2 标量值格式

| 类型 | 格式 | 示例 |
|:-----|:-----|:-----|
| 整数 | `int:42` | `"int:42"` |
| 浮点 | `float:0.8` | `"float:0.95"` |
| 文本 | `text:beach` | `"text:sunset"` |
| 时间戳 | `ts:1717200000` | `"ts:1717200000"` (unix 秒) |

### 7.3 过滤操作

| 操作 | 说明 |
|:-----|:-----|
| `eq` | 等于 |
| `gt` | 大于 |
| `lt` | 小于 |
| `gte` | 大于等于 |
| `lte` | 小于等于 |
