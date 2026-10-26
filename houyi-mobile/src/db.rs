//! MobileDB: 多 Collection 管理（与服务器 CollectionManager 一脉相承）。
//! Multi-collection manager matching the server-side CollectionManager API.
//!
//! 用法 / Usage:
//!   let db = MobileDB::new();
//!   db.get_or_create("photos", 512, "flat-clustered");
//!   db.insert("photos", 1, &vec![0.1; 512])?;
//!   db.build("photos")?;
//!   let results = db.search("photos", &query, 10)?;

use crate::filter::{FilterOp, ScalarStore, ScalarVal};
use crate::storage::VectorStore;
use houyi_vector::cluster::{ClusterConfig, ClusteredHnsw, FlatClustered};
use houyi_vector::DistanceMetric;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 移动端索引后端（与服务器 IndexBackend 对应）。
/// Mobile index backend — mirrors server-side IndexBackend.
enum MobileBackend {
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

impl MobileBackend {
    fn new(dims: usize, index_type: &str) -> Self {
        match index_type {
            "flat" => MobileBackend::Flat(VectorStore::new(dims, 1024)),
            "flat-clustered" => MobileBackend::FlatClustered {
                store: VectorStore::new(dims, 1024),
                index: None,
            },
            _ => MobileBackend::ClusteredHnsw {
                store: VectorStore::new(dims, 1024),
                index: None,
            },
        }
    }
    fn insert(&mut self, id: u64, v: &[f32]) {
        match self {
            MobileBackend::Flat(s) => s.upsert(id, v),
            MobileBackend::FlatClustered { store, .. } => store.upsert(id, v),
            MobileBackend::ClusteredHnsw { store, .. } => store.upsert(id, v),
        }
    }
    fn build(&mut self, dims: usize) -> usize {
        match self {
            MobileBackend::Flat(s) => s.len(),
            MobileBackend::FlatClustered { store, index } => {
                let (ids, vecs) = store.raw_data();
                let n = ids.len();
                if n < 2 {
                    return n;
                }
                let c = (n as f64).sqrt().ceil() as usize;
                *index = Some(FlatClustered::build(
                    vecs,
                    ids,
                    dims,
                    DistanceMetric::Cosine,
                    c,
                    10,
                    0,
                ));
                index.as_ref().map(|i| i.total_vectors()).unwrap_or(0)
            }
            MobileBackend::ClusteredHnsw { store, index } => {
                let (ids, vecs) = store.raw_data();
                let n = ids.len();
                if n < 2 {
                    return n;
                }
                let cfg = ClusterConfig {
                    hnsw_ef_construction: 200,
                    hnsw_ef_search: 128,
                    hnsw_m: 12,
                    ..ClusterConfig::infer(n, dims)
                };
                *index = Some(ClusteredHnsw::build(
                    vecs,
                    ids,
                    dims,
                    DistanceMetric::Cosine,
                    &cfg,
                ));
                index.as_ref().map(|i| i.total_vectors()).unwrap_or(0)
            }
        }
    }
    fn search(&self, q: &[f32], k: usize, dims: usize) -> Vec<(u64, f32)> {
        match self {
            MobileBackend::Flat(s) => {
                let (ids, vecs) = s.raw_data();
                let n = ids.len();
                if n == 0 {
                    return Vec::new();
                }
                let mut scored: Vec<(u64, f32)> = (0..n)
                    .map(|i| {
                        let v = &vecs[i * dims..(i + 1) * dims];
                        let d: f32 = q
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
            MobileBackend::FlatClustered { index, .. } => match index.as_ref() {
                Some(fc) => fc.search(q, k),
                None => Vec::new(),
            },
            MobileBackend::ClusteredHnsw { index, .. } => match index.as_ref() {
                Some(ch) => ch.search(q, k),
                None => Vec::new(),
            },
        }
    }
    fn len(&self) -> usize {
        match self {
            MobileBackend::Flat(s) => s.len(),
            MobileBackend::FlatClustered { store, .. } => store.len(),
            MobileBackend::ClusteredHnsw { store, .. } => store.len(),
        }
    }
}

/// Collection：独立维度 + 独立索引后端。
/// A named collection with its own dims and index backend.
pub struct Collection {
    pub name: String,
    pub dims: usize,
    pub index_type: String,
    backend: Mutex<MobileBackend>,
    /// 标量列存储（用于混合检索）/ Scalar store for hybrid search
    /// EN: Scalar store for hybrid search.
    scalars: Mutex<ScalarStore>,
}

/// MobileDB：多 Collection 管理器（与服务器 CollectionManager 一脉相承）。
/// Multi-collection database — same semantics as server-side CollectionManager.
pub struct MobileDB {
    collections: Mutex<HashMap<String, Arc<Collection>>>,
}

impl MobileDB {
    /// 创建空数据库 / Create empty database.
    /// EN: Create an empty database.
    pub fn new() -> Self {
        MobileDB {
            collections: Mutex::new(HashMap::new()),
        }
    }

    /// 获取或创建 Collection / Get or create a named collection.
    /// 返回 Arc<Collection>（锁外安全持有）/ Returns Arc so caller can hold reference safely.
    /// EN: Get or create a named collection.
    /// EN: Returns Arc so the caller can hold the reference safely outside the lock.
    pub fn get_or_create(&self, name: &str, dims: usize, index_type: &str) -> Arc<Collection> {
        let mut guard = self.collections.lock().unwrap();
        guard
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(Collection {
                    name: name.to_string(),
                    dims,
                    index_type: index_type.to_string(),
                    backend: Mutex::new(MobileBackend::new(dims, index_type)),
                    scalars: Mutex::new(ScalarStore::new()),
                })
            })
            .clone()
    }

    /// 获取 Collection / Get a collection by name.
    /// EN: Get a collection by name.
    pub fn get(&self, name: &str) -> Option<Arc<Collection>> {
        self.collections.lock().unwrap().get(name).cloned()
    }

    /// 插入向量到指定 Collection / Insert vector into named collection.
    /// EN: Insert a vector into a named collection.
    pub fn insert(&self, coll_name: &str, id: u64, vector: &[f32]) -> Result<(), String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        if vector.len() != col.dims {
            return Err(format!("dim mismatch: {} vs {}", vector.len(), col.dims));
        }
        col.backend.lock().unwrap().insert(id, vector);
        Ok(())
    }

    /// 构建 Collection 索引 / Build collection index.
    /// EN: Build the collection index.
    pub fn build(&self, coll_name: &str) -> Result<usize, String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        let mut guard = col.backend.lock().unwrap();
        Ok(guard.build(col.dims))
    }

    /// 在 Collection 中检索 / Search within a collection.
    /// EN: Search within a collection.
    pub fn search(
        &self,
        coll_name: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(u64, f32)>, String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        let guard = col.backend.lock().unwrap();
        Ok(guard.search(query, k, col.dims))
    }

    /// Collection 向量数 / Vector count in a collection.
    /// EN: Vector count in a collection.
    pub fn count(&self, coll_name: &str) -> Result<usize, String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        let guard = col.backend.lock().unwrap();
        Ok(guard.len())
    }

    /// 列出所有 Collection / List all collection names.
    /// EN: List all collection names.
    pub fn list(&self) -> Vec<String> {
        self.collections.lock().unwrap().keys().cloned().collect()
    }

    /// 插入标量标签 / Insert scalar tags for a row.
    /// `tags`: [("col_name", "value"), ...] — value 格式: "int:42" | "float:0.8" | "text:beach" | "ts:1717200000"
    /// EN: Insert scalar tags for a row.
    /// EN: `tags`: [("col_name", "value"), ...] — value format: "int:42" | "float:0.8"
    /// EN: | "text:beach" | "ts:1717200000"
    pub fn insert_scalars(&self, coll_name: &str, tags: &[(&str, &str)]) -> Result<(), String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        let row: Vec<(&str, ScalarVal)> = tags
            .iter()
            .filter_map(|(col_name, val)| parse_scalar(val).map(|sv| (*col_name, sv)))
            .collect();
        col.scalars.lock().unwrap().push_row(&row);
        Ok(())
    }

    /// 混合检索：先结构化过滤 → 再向量检索 / Hybrid search: filter → vector ANN.
    /// EN: Hybrid search: structured filter first → then vector search.
    pub fn search_hybrid(
        &self,
        coll_name: &str,
        query: &[f32],
        k: usize,
        filters: &[FilterOp],
    ) -> Result<Vec<(u64, f32)>, String> {
        let col = self
            .get(coll_name)
            .ok_or_else(|| format!("collection '{coll_name}' not found"))?;
        let filter_ids = {
            let guard = col.scalars.lock().unwrap();
            guard.filter(filters)
        };
        let guard = col.backend.lock().unwrap();
        let mut results = guard.search(query, k, col.dims);
        // `None` = 无过滤条件；`Some(empty)` = 有条件但无命中 → 结果必须为空。
        // 旧实现把两者都当「不过滤」，未知列/零命中的过滤反而返回全量结果。
        // EN: `None` = no filters; `Some(empty)` = matched nothing — the latter
        // must yield an empty result, not skip the mask.
        match filter_ids {
            None => {}
            Some(filter_ids) => {
                let id_set: std::collections::HashSet<u64> = filter_ids.into_iter().collect();
                results.retain(|(id, _)| id_set.contains(id));
            }
        }
        Ok(results)
    }

    /// 删除 Collection / Drop a collection.
    /// EN: Drop a collection.
    pub fn drop(&self, name: &str) -> bool {
        self.collections.lock().unwrap().remove(name).is_some()
    }
}

/// 解析标量值字符串 / Parse scalar value from string format.
/// "int:42" | "float:0.8" | "text:beach" | "ts:1717200000"
fn parse_scalar(raw: &str) -> Option<ScalarVal> {
    if let Some(v) = raw.strip_prefix("int:") {
        v.parse().ok().map(ScalarVal::Int)
    } else if let Some(v) = raw.strip_prefix("float:") {
        v.parse().ok().map(ScalarVal::Float)
    } else if let Some(v) = raw.strip_prefix("text:") {
        Some(ScalarVal::Text(v.to_string()))
    } else if let Some(v) = raw.strip_prefix("ts:") {
        v.parse().ok().map(ScalarVal::Timestamp)
    } else {
        Some(ScalarVal::Text(raw.to_string()))
    }
}
