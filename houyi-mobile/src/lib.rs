//! HouyiDB Mobile: 轻量级向量索引，C ABI 接口，面向 iOS / Android。
//! C-compatible API for mobile vector search.
//!
//! 双模式 / Dual mode:
//!   - 单 Collection: houyi_mobile_create/insert/search  (向后兼容)
//!   - 多 Collection: houyi_mobile_db_open/collection/insert/search

// C ABI 边界：extern "C" 导出函数必须解引用原始指针，而 unsafe 无法跨 FFI 约束调用方。
// EN: FFI entry points deref raw pointers by design; `unsafe` cannot be enforced across the C ABI.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod db;
mod filter;
mod storage;

use houyi_vector::cluster::{ClusterConfig, ClusteredHnsw, FlatClustered};
use houyi_vector::DistanceMetric;
use std::sync::Mutex;
use storage::VectorStore;

// ── 索引类型枚举 ──────────────────────────────────────────
// EN: -- Index type enum --

enum IndexBackend {
    Flat(VectorStore),
    FlatClustered {
        store: VectorStore,
        index: Option<FlatClustered>,
    },
    ClusteredHnsw {
        store: VectorStore,
        index: Option<ClusteredHnsw>,
    },
}

/// 移动端索引句柄。
/// EN: Mobile-side index handle.
pub struct MobileIndex {
    inner: Mutex<IndexBackend>,
    dims: usize,
}

impl MobileIndex {
    pub fn new(dims: usize, index_type: &str) -> Self {
        let inner = match index_type {
            "flat" => IndexBackend::Flat(VectorStore::new(dims, 1024)),
            "flat-clustered" => IndexBackend::FlatClustered {
                store: VectorStore::new(dims, 1024),
                index: None,
            },
            _ => IndexBackend::ClusteredHnsw {
                store: VectorStore::new(dims, 1024),
                index: None,
            },
        };
        MobileIndex {
            inner: Mutex::new(inner),
            dims,
        }
    }

    /// 插入向量（所有模式均写入 store）。
    /// EN: Insert a vector (all modes write to the store).
    pub fn insert(&self, id: u64, vector: &[f32]) {
        let mut guard = self.inner.lock().unwrap();
        match &mut *guard {
            IndexBackend::Flat(store) => store.upsert(id, vector),
            IndexBackend::FlatClustered { store, .. } => store.upsert(id, vector),
            IndexBackend::ClusteredHnsw { store, .. } => store.upsert(id, vector),
        }
    }

    /// 构建索引（Flat: no-op; FlatClustered/HNSW: 聚类+建图）。
    /// EN: Build the index (Flat: no-op; FlatClustered/HNSW: cluster + build graph).
    pub fn build(&self) -> usize {
        let mut guard = self.inner.lock().unwrap();
        match &mut *guard {
            IndexBackend::Flat(store) => store.len(),
            IndexBackend::FlatClustered { store, index } => {
                let (ids, vecs) = store.raw_data();
                let n = ids.len();
                if n < 2 {
                    return n;
                }
                let c = (n as f64).sqrt().ceil() as usize;
                let fc =
                    FlatClustered::build(vecs, ids, self.dims, DistanceMetric::Cosine, c, 10, 0);
                let count = fc.total_vectors();
                *index = Some(fc);
                count
            }
            IndexBackend::ClusteredHnsw { store, index } => {
                let (ids, vecs) = store.raw_data();
                let n = ids.len();
                if n < 2 {
                    return n;
                }
                let cfg = ClusterConfig {
                    hnsw_ef_construction: 200,
                    hnsw_ef_search: 128,
                    hnsw_m: 12, // 移动端用小 M 节约内存
                    ..ClusterConfig::infer(n, self.dims)
                };
                let ch = ClusteredHnsw::build(vecs, ids, self.dims, DistanceMetric::Cosine, &cfg);
                let count = ch.total_vectors();
                *index = Some(ch);
                count
            }
        }
    }

    /// 检索 top-k，返回 (ids, distances)。
    /// EN: Search top-k, returning (ids, distances).
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        let guard = self.inner.lock().unwrap();
        match &*guard {
            IndexBackend::Flat(store) => {
                let (ids, vecs) = store.raw_data();
                let n = ids.len();
                if n == 0 {
                    return Vec::new();
                }
                let mut scored: Vec<(u64, f32)> = (0..n)
                    .map(|i| {
                        let v = &vecs[i * self.dims..(i + 1) * self.dims];
                        let d: f32 = query
                            .iter()
                            .zip(v.iter())
                            .map(|(a, b)| {
                                let d = a - b;
                                d * d
                            })
                            .sum();
                        (ids[i], d)
                    })
                    .collect();
                scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
                scored.truncate(k);
                scored
            }
            IndexBackend::FlatClustered { index, .. } => match index.as_ref() {
                Some(fc) => fc.search(query, k),
                None => Vec::new(),
            },
            IndexBackend::ClusteredHnsw { index, .. } => match index.as_ref() {
                Some(ch) => ch.search(query, k),
                None => Vec::new(),
            },
        }
    }

    pub fn vector_count(&self) -> usize {
        let guard = self.inner.lock().unwrap();
        match &*guard {
            IndexBackend::Flat(store) => store.len(),
            IndexBackend::FlatClustered { store, .. } => store.len(),
            IndexBackend::ClusteredHnsw { store, .. } => store.len(),
        }
    }
}

// ── C ABI ─────────────────────────────────────────────────
// 移动端原生调用：iOS (Swift/ObjC), Android (JNI/Kotlin)
// EN: -- C ABI --
// EN: Mobile native calls: iOS (Swift/ObjC), Android (JNI/Kotlin)

use std::ffi::{c_char, CStr};

/// 创建索引句柄。
/// index_type: "flat" | "flat-clustered" | "clustered-hnsw"
/// 返回句柄指针，失败返回 null。
#[no_mangle]
pub extern "C" fn houyi_mobile_create(dims: u32, index_type: *const c_char) -> *mut MobileIndex {
    if index_type.is_null() || dims == 0 {
        return std::ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(index_type) };
    let t = c_str.to_str().unwrap_or("flat");
    let idx = MobileIndex::new(dims as usize, t);
    Box::into_raw(Box::new(idx))
}

/// 插入向量。vector 长度必须等于 dims。
/// EN: Insert a vector. The vector length must equal dims.
#[no_mangle]
pub extern "C" fn houyi_mobile_insert(
    handle: *mut MobileIndex,
    id: u64,
    vector: *const f32,
    len: u32,
) -> i32 {
    if handle.is_null() || vector.is_null() {
        return -1;
    }
    let idx = unsafe { &*handle };
    let v = unsafe { std::slice::from_raw_parts(vector, len as usize) };
    idx.insert(id, v);
    0
}

/// 构建/定稿索引。返回索引向量数。
/// EN: Build/finalize the index. Returns the number of indexed vectors.
#[no_mangle]
pub extern "C" fn houyi_mobile_build(handle: *mut MobileIndex) -> u32 {
    if handle.is_null() {
        return 0;
    }
    let idx = unsafe { &*handle };
    idx.build() as u32
}

/// 检索 top-k。结果写入 results 缓冲区: [id0, dist0, id1, dist1, ...]。
/// 返回实际写入的结果数。
/// EN: Search top-k. Results are written into the results buffer:
/// EN: [id0, dist0, id1, dist1, ...].
/// EN: Returns the number of results actually written.
#[no_mangle]
pub extern "C" fn houyi_mobile_search(
    handle: *mut MobileIndex,
    query: *const f32,
    dims: u32,
    k: u32,
    results: *mut f32, // [id as f32, dist] × k
    max_results: u32,
) -> u32 {
    if handle.is_null() || query.is_null() || results.is_null() {
        return 0;
    }
    let idx = unsafe { &*handle };
    let q = unsafe { std::slice::from_raw_parts(query, dims as usize) };
    let found = idx.search(q, k as usize);
    let n = found.len().min(max_results as usize);
    let out = unsafe { std::slice::from_raw_parts_mut(results, n * 2) };
    for (i, (id, dist)) in found.iter().take(n).enumerate() {
        out[i * 2] = *id as f32;
        out[i * 2 + 1] = *dist;
    }
    n as u32
}

/// 获取已索引向量数。
/// EN: Get the number of indexed vectors.
#[no_mangle]
pub extern "C" fn houyi_mobile_count(handle: *mut MobileIndex) -> u32 {
    if handle.is_null() {
        return 0;
    }
    let idx = unsafe { &*handle };
    idx.vector_count() as u32
}

/// 释放索引。
/// EN: Free the index.
#[no_mangle]
pub extern "C" fn houyi_mobile_destroy(handle: *mut MobileIndex) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

// ── MobileDB C API (多 Collection) ─────────────────────────
// EN: -- MobileDB C API (multiple collections) --

/// 打开/创建数据库 / Open or create a multi-collection database.
/// EN: Open or create a multi-collection database.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_open() -> *mut db::MobileDB {
    Box::into_raw(Box::new(db::MobileDB::new()))
}

/// 获取或创建 Collection / Get or create a named collection.
/// 首次调用时锁定维度，后续调用 dims 参数被忽略。
/// EN: Get or create a named collection.
/// EN: The first call locks the dimensions; later calls ignore the dims argument.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_collection(
    db_handle: *mut db::MobileDB,
    name: *const std::ffi::c_char,
    dims: u32,
    index_type: *const std::ffi::c_char,
) -> i32 {
    if db_handle.is_null() || name.is_null() || index_type.is_null() || dims == 0 {
        return -1;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(name) };
    let coll_name = c_name.to_str().unwrap_or("default");
    let c_type = unsafe { std::ffi::CStr::from_ptr(index_type) };
    let idx_type = c_type.to_str().unwrap_or("flat");
    db.get_or_create(coll_name, dims as usize, idx_type);
    0
}

/// 向 Collection 插入向量 / Insert vector into named collection.
/// EN: Insert a vector into a named collection.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_insert(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
    id: u64,
    vector: *const f32,
    len: u32,
) -> i32 {
    if db_handle.is_null() || coll_name.is_null() || vector.is_null() {
        return -1;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    let v = unsafe { std::slice::from_raw_parts(vector, len as usize) };
    db.insert(name, id, v).map(|_| 0).unwrap_or(-1)
}

/// 构建 Collection 索引 / Build collection index.
/// EN: Build the collection index.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_build(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
) -> u32 {
    if db_handle.is_null() || coll_name.is_null() {
        return 0;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    db.build(name).unwrap_or(0) as u32
}

/// 检索 / Search within a collection.
/// EN: Search within a collection.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_search(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
    query: *const f32,
    dims: u32,
    k: u32,
    results: *mut f32,
    max_results: u32,
) -> u32 {
    if db_handle.is_null() || coll_name.is_null() || query.is_null() || results.is_null() {
        return 0;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    let q = unsafe { std::slice::from_raw_parts(query, dims as usize) };
    let found = db.search(name, q, k as usize).unwrap_or_default();
    let n = found.len().min(max_results as usize);
    let out = unsafe { std::slice::from_raw_parts_mut(results, n * 2) };
    for (i, (id, dist)) in found.iter().take(n).enumerate() {
        out[i * 2] = *id as f32;
        out[i * 2 + 1] = *dist;
    }
    n as u32
}

/// 获取 Collection 向量数 / Get vector count in a collection.
/// EN: Get the vector count of a collection.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_count(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
) -> u32 {
    if db_handle.is_null() || coll_name.is_null() {
        return 0;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    db.count(name).unwrap_or(0) as u32
}

/// 删除 Collection / Drop a collection.
/// EN: Drop a collection.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_drop(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
) -> i32 {
    if db_handle.is_null() || coll_name.is_null() {
        return -1;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    if db.drop(name) {
        0
    } else {
        -1
    }
}

/// 插入标量标签 / Insert scalar tags for a row.
/// tags_json: JSON array like [{"col":"tag","val":"text:beach"},{"col":"date","val":"ts:1717200000"}]
#[no_mangle]
pub extern "C" fn houyi_mobile_db_insert_scalars(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
    tags_json: *const std::ffi::c_char,
) -> i32 {
    if db_handle.is_null() || coll_name.is_null() || tags_json.is_null() {
        return -1;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    let c_json = unsafe { std::ffi::CStr::from_ptr(tags_json) };
    let json_str = c_json.to_str().unwrap_or("[]");

    // 简单 JSON 解析 [{"col":"tag","val":"text:beach"},...]
    // EN: Simple JSON parsing [{"col":"tag","val":"text:beach"},...]
    let mut tags: Vec<(&str, &str)> = Vec::new();
    let mut col = "";
    for part in json_str.split(['{', '}', ',', '"', ':']) {
        let t = part.trim();
        if t.is_empty() || t == "[" || t == "]" {
            continue;
        }
        if t == "col" || t == "val" {
            continue;
        }
        if col.is_empty() {
            col = t;
        } else {
            tags.push((col, t));
            col = "";
        }
    }
    db.insert_scalars(name, &tags).map(|_| 0).unwrap_or(-1)
}

/// 混合检索 / Hybrid search with structured filters.
/// filters_json: JSON array of filter ops
///   [{"col":"tag","op":"eq","val":"text:beach"},{"col":"date","op":"gt","val":"ts:1717200000"}]
#[no_mangle]
pub extern "C" fn houyi_mobile_db_search_hybrid(
    db_handle: *mut db::MobileDB,
    coll_name: *const std::ffi::c_char,
    query: *const f32,
    dims: u32,
    k: u32,
    filters_json: *const std::ffi::c_char,
    results: *mut f32,
    max_results: u32,
) -> u32 {
    if db_handle.is_null() || coll_name.is_null() || query.is_null() || results.is_null() {
        return 0;
    }
    let db = unsafe { &*db_handle };
    let c_name = unsafe { std::ffi::CStr::from_ptr(coll_name) };
    let name = c_name.to_str().unwrap_or("default");
    let q = unsafe { std::slice::from_raw_parts(query, dims as usize) };

    // 解析 filters JSON → FilterOp vec
    // EN: Parse filters JSON → FilterOp vec.
    let c_json = unsafe { std::ffi::CStr::from_ptr(filters_json) };
    let json_str = c_json.to_str().unwrap_or("[]");

    use crate::filter::{FilterOp, FilterOpType, ScalarVal};
    let mut filters = Vec::new();
    let mut current_col = String::new();
    let mut current_op = String::new();
    let mut current_val = String::new();
    let mut field = String::new();

    for part in json_str.split(['{', '}', ',', '"', ':']) {
        let t = part.trim();
        if t.is_empty() || t == "[" || t == "]" {
            continue;
        }
        if t == "col" || t == "op" || t == "val" {
            field = t.to_string();
            continue;
        }
        if !field.is_empty() {
            match field.as_str() {
                "col" => current_col = t.to_string(),
                "op" => current_op = t.to_string(),
                "val" => current_val = t.to_string(),
                _ => {}
            }
            if !current_col.is_empty() && !current_op.is_empty() && !current_val.is_empty() {
                let sv = if let Some(v) = current_val.strip_prefix("int:") {
                    v.parse().ok().map(ScalarVal::Int)
                } else if let Some(v) = current_val.strip_prefix("float:") {
                    v.parse().ok().map(ScalarVal::Float)
                } else if let Some(v) = current_val.strip_prefix("text:") {
                    Some(ScalarVal::Text(v.to_string()))
                } else if let Some(v) = current_val.strip_prefix("ts:") {
                    v.parse().ok().map(ScalarVal::Timestamp)
                } else {
                    Some(ScalarVal::Text(current_val.to_string()))
                };
                if let Some(val) = sv {
                    let op = match current_op.as_str() {
                        "eq" => FilterOpType::Eq,
                        "gt" => FilterOpType::Gt,
                        "lt" => FilterOpType::Lt,
                        "gte" => FilterOpType::GtEq,
                        "lte" => FilterOpType::LtEq,
                        _ => FilterOpType::Eq,
                    };
                    filters.push(FilterOp {
                        column: current_col.clone(),
                        op,
                        value: val,
                    });
                }
                current_col.clear();
                current_op.clear();
                current_val.clear();
            }
            field.clear();
        }
    }

    let found = db
        .search_hybrid(name, q, k as usize, &filters)
        .unwrap_or_default();
    let n = found.len().min(max_results as usize);
    let out = unsafe { std::slice::from_raw_parts_mut(results, n * 2) };
    for (i, (id, dist)) in found.iter().take(n).enumerate() {
        out[i * 2] = *id as f32;
        out[i * 2 + 1] = *dist;
    }
    n as u32
}

/// 关闭数据库 / Close database and free all resources.
/// EN: Close the database and free all resources.
#[no_mangle]
pub extern "C" fn houyi_mobile_db_close(db_handle: *mut db::MobileDB) {
    if !db_handle.is_null() {
        unsafe {
            drop(Box::from_raw(db_handle));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn test_flat_create_insert_search() {
        let dims = 4u32;
        let t = CString::new("flat").unwrap();
        let handle = houyi_mobile_create(dims, t.as_ptr());
        assert!(!handle.is_null());

        // Insert
        let v1: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
        let v2: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
        assert_eq!(houyi_mobile_insert(handle, 1, v1.as_ptr(), 4), 0);
        assert_eq!(houyi_mobile_insert(handle, 2, v2.as_ptr(), 4), 0);
        assert_eq!(houyi_mobile_count(handle), 2);

        // Search
        let q: [f32; 4] = [1.0, 0.1, 0.0, 0.0];
        let mut results = vec![0.0f32; 4]; // 2 results × 2 fields
        let n = houyi_mobile_search(handle, q.as_ptr(), 4, 1, results.as_mut_ptr(), 2);
        assert_eq!(n, 1);
        // 最近邻应该是 id=1
        // EN: The nearest neighbor should be id=1.
        assert_eq!(results[0] as u64, 1);

        houyi_mobile_destroy(handle);
    }

    #[test]
    fn test_flat_clustered_build_search() {
        let dims = 8u32;
        let n = 200;
        let t = CString::new("flat-clustered").unwrap();
        let handle = houyi_mobile_create(dims, t.as_ptr());

        // 插入随机向量
        // EN: Insert random vectors
        use rand::Rng;
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let mut vectors = Vec::new();
        for i in 0..n {
            let mut v = Vec::with_capacity(dims as usize);
            for _ in 0..dims {
                v.push(rng.gen::<f32>());
            }
            houyi_mobile_insert(handle, i as u64, v.as_ptr(), dims);
            vectors.push(v);
        }
        assert_eq!(houyi_mobile_count(handle), n as u32);

        // Build
        let count = houyi_mobile_build(handle);
        assert!(count > 0);

        // 检索：查询向量应命中自身
        // EN: Search: the query should find itself
        let q = &vectors[50];
        let mut results = vec![0.0f32; 20];
        let n_found = houyi_mobile_search(handle, q.as_ptr(), dims, 1, results.as_mut_ptr(), 10);
        assert_eq!(n_found, 1);
        assert_eq!(results[0] as u64, 50);

        houyi_mobile_destroy(handle);
    }

    #[test]
    fn test_db_multi_collection() {
        let db = db::MobileDB::new();
        // 创建两个维度不同的 Collection
        // EN: Create two collections with different dims
        let photos = db.get_or_create("photos", 128, "flat");
        let docs = db.get_or_create("docs", 256, "flat");
        assert_eq!(photos.dims, 128);
        assert_eq!(docs.dims, 256);

        // 向 Collection 插入向量
        // EN: Insert into collections
        let v128: Vec<f32> = (0..128).map(|i| i as f32 * 0.01).collect();
        let v256: Vec<f32> = (0..256).map(|i| i as f32 * 0.01).collect();
        db.insert("photos", 1, &v128).unwrap();
        db.insert("docs", 2, &v256).unwrap();
        assert_eq!(db.count("photos").unwrap(), 1);
        assert_eq!(db.count("docs").unwrap(), 1);

        // Search
        let r = db.search("photos", &v128, 1).unwrap();
        assert_eq!(r[0].0, 1);
        let r = db.search("docs", &v256, 1).unwrap();
        assert_eq!(r[0].0, 2);

        // 维度不匹配
        // EN: Dim mismatch
        assert!(db.insert("photos", 3, &v256).is_err());

        // List
        let names = db.list();
        assert!(names.contains(&"photos".to_string()));
        assert!(names.contains(&"docs".to_string()));
    }
}
