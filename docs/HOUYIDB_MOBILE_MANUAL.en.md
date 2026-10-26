# HouyiDB Mobile User Manual

> [中文](HOUYIDB_MOBILE_MANUAL.md)

> Lightweight vector index for native iOS / Android integration.  
> C ABI interface; build artifacts are static (`.a`) or dynamic (`.so` / `.dylib`) libraries.

---

## 1. Building

### 1.1 Building the static library

```bash
# macOS / iOS (aarch64)
cargo build --release -p houyi-mobile --target aarch64-apple-darwin
ls target/release/libhouyi_mobile.a   # ~19MB (release LTO)

# iOS (cross-compile; requires Xcode)
cargo build --release -p houyi-mobile --target aarch64-apple-ios

# Android (cross-compile; requires NDK)
cargo build --release -p houyi-mobile --target aarch64-linux-android
```

### 1.2 Linking into your project

- **iOS**: Drag `libhouyi_mobile.a` into Xcode and add the `libresolv.tbd` dependency
- **Android**: Place it under `app/src/main/jniLibs/arm64-v8a/` and call it via JNI

---

## 2. C API Reference

### 2.1 Creating an index

```c
void* houyi_mobile_create(uint32_t dims, const char* index_type);
```

| Parameter | Description |
|------|------|
| `dims` | Vector dimensions (e.g., 128, 768, 1536) |
| `index_type` | `"flat"` / `"flat-clustered"` / `"clustered-hnsw"` |
| Return | Index handle (NULL indicates failure) |

**Three index types**:

| Type | Memory | Latency | Recall | Use case |
|------|------|------|--------|------|
| `flat` | No compression | O(N) | 1.0 (exact) | N < 5K, offline evaluation |
| `flat-clustered` | No compression | < 1ms | > 0.95 | **Recommended mobile default** |
| `clustered-hnsw` | Graph structure | ~1ms | > 0.99 | N > 50K, high recall |

### 2.2 Inserting vectors

```c
int32_t houyi_mobile_insert(void* handle, uint64_t id, const float* vector, uint32_t len);
```

| Parameter | Description |
|------|------|
| `handle` | Index handle |
| `id` | Unique vector identifier |
| `vector` | Pointer to the float array |
| `len` | Array length (must equal dims) |
| Return | 0 on success, -1 on failure |

Supports idempotent overwrite: inserting the same `id` again overwrites the old value.

### 2.3 Building the index

```c
uint32_t houyi_mobile_build(void* handle);
```

> Only `flat-clustered` and `clustered-hnsw` need this call. It is a no-op in `flat` mode.

Returns the total number of indexed vectors.

### 2.4 Searching

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

| Parameter | Description |
|------|------|
| `results` | Preallocated buffer; each result occupies 2 floats (id + distance) |
| `max_results` | Maximum number of results the results array can hold (≥ k) |
| Return | Number of results actually written |

### 2.5 Counting vectors

```c
uint32_t houyi_mobile_count(void* handle);
```

### 2.6 Destroying the index

```c
void houyi_mobile_destroy(void* handle);
```

---

## 3. Platform Examples

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

// Usage
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

## 4. Full Workflow

```
1. houyi_mobile_create(128, "flat-clustered")   // 创建索引
2. houyi_mobile_insert(...) × N                  // 插入 N 个向量
3. houyi_mobile_build(...)                       // 构建聚类索引
4. houyi_mobile_search(...) × M                  // M 次检索
5. houyi_mobile_destroy(...)                     // 释放内存
```

---

## 5. Performance Tuning

Key parameters for `flat-clustered` mode (modify `houyi-vector/src/cluster/mod.rs` before compiling):

| Parameter | Default | Purpose |
|------|------|------|
| `ClusterConfig::kmeans_iters` | 15 | Number of K-Means iterations; higher values yield more precise clustering |
| `ClusterConfig::hnsw_ef_search` | 256 | Search candidate width (clustered-hnsw only) |
| `PqConfig::m` | dims/16 | Number of PQ segments; fewer segments give higher accuracy but lower compression ratio |
| `PqConfig::k` | 256 | Codebook size per segment |

---

## 6. Comparison with MicroNN

| Metric | MicroNN | HouyiDB Mobile |
|------|:---:|:---:|
| Latency | < 7 ms | **3.93 ms** |
| Recall@100 | ≥ 90% | **0.937** |
| RAM | ~10 MB | **15.4 MB** |
| Vector storage | Disk | Disk (mmap) |
| Interface | Swift/ObjC | C ABI (Swift/Kotlin/Flutter) |

---

## 7. Hybrid Search (Vector + Structured Filtering)

### 7.1 Swift Example

```swift
let db = houyi_mobile_db_open()

// Create collection
houyi_mobile_db_collection(db, "photos", 512, "flat-clustered")

// Insert vector + scalar labels
houyi_mobile_db_insert(db, "photos", 1, vector, 512)
houyi_mobile_db_insert_scalars(db, "photos", 
    "[{\"col\":\"tag\",\"val\":\"text:beach\"},{\"col\":\"date\",\"val\":\"ts:1717200000\"}]")

// Hybrid search: tag=beach AND date>2024-06-01
// filters_json: [{"col":"tag","op":"eq","val":"text:beach"},{"col":"date","op":"gt","val":"ts:1717200000"}]
houyi_mobile_db_search_hybrid(db, "photos", query, 512, 10, filters_json, results, 10)
```

### 7.2 Scalar Value Formats

| Type | Format | Example |
|:-----|:-----|:-----|
| Integer | `int:42` | `"int:42"` |
| Float | `float:0.8` | `"float:0.95"` |
| Text | `text:beach` | `"text:sunset"` |
| Timestamp | `ts:1717200000` | `"ts:1717200000"` (unix seconds) |

### 7.3 Filter Operations

| Operation | Description |
|:-----|:-----|
| `eq` | Equal to |
| `gt` | Greater than |
| `lt` | Less than |
| `gte` | Greater than or equal to |
| `lte` | Less than or equal to |
