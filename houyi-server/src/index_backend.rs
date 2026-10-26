//! IndexBackend：HNSW / ClusteredHnsw / FlatClustered 三种索引后端。
//! EN: IndexBackend: three index backends — HNSW / ClusteredHnsw / FlatClustered.

use houyi_vector::cluster::{ClusterConfig, ClusteredHnsw, FlatClustered, PqFlatClustered};
use houyi_vector::hii::{HiiL0, HiiL0Config};
use houyi_vector::{
    AsyncBuilderOnceReport, BuilderMetricsSnapshot, DistanceMetric, FlushPolicy, HnswBatchPlanMode,
    HotBufferIndex, VectorId, VectorIndex,
};
use std::sync::{Arc, Mutex};

/// 延迟构图后端共享的「已写入未构图」行缓冲（id, 向量）。
/// Pending (not-yet-built) rows shared by lazy-build backends: `(id, vector)`.
///
/// WHY：四个变体各自内联 `Arc<Mutex<Vec<(u64, Vec<f32>)>>>` 会触发
/// clippy::type_complexity 且容易漂移；提取别名让「缓冲行」这一概念只有一处定义。
/// EN: One alias for the buffered-rows shape shared by four variants.
type BufferedRows = Arc<Mutex<Vec<(u64, Vec<f32>)>>>;

#[derive(Clone)]
pub enum IndexBackend {
    Hnsw(Arc<HotBufferIndex>),
    Clustered {
        buf: BufferedRows,
        index: Arc<Mutex<Option<ClusteredHnsw>>>,
        dims: usize,
        built: Arc<Mutex<bool>>,
        /// 构图度量：决定 scored 距离是否为余弦（L2 构图返回 L2²，边缘不得 clamp）。
        /// Build metric: decides whether scored distances are cosine (L2 build → L2²,
        /// the edge must not clamp).
        metric: DistanceMetric,
    },
    FlatClustered {
        buf: BufferedRows,
        index: Arc<Mutex<Option<FlatClustered>>>,
        dims: usize,
        built: Arc<Mutex<bool>>,
        /// 构图度量：同 `Clustered`，纵深防御 L2 构图被误当余弦 clamp。
        /// Build metric — same fail-closed gate as `Clustered`.
        metric: DistanceMetric,
    },
    Flat {
        data: BufferedRows,
        dims: usize,
    },
    PqFlat {
        buf: BufferedRows,
        index: Arc<Mutex<Option<PqFlatClustered>>>,
        dims: usize,
        built: Arc<Mutex<bool>>,
    },
    /// HII L0: 内存增量层 — 实时写入可见，小图 ANN 检索。
    /// HII L0: memory incremental layer — real-time writes, small graph ANN search.
    /// L1/L2 TODO (待 GPU/NVMe 硬件就绪).
    HiiL0 {
        inner: Arc<Mutex<HiiL0>>,
        dims: usize,
    },
}

impl IndexBackend {
    pub fn new_hnsw(dims: usize) -> Self {
        IndexBackend::Hnsw(Arc::new(HotBufferIndex::new_with_metric(
            dims,
            DistanceMetric::Cosine,
        )))
    }
    pub fn new_clustered(dims: usize) -> Self {
        IndexBackend::Clustered {
            buf: Arc::new(Mutex::new(Vec::new())),
            index: Arc::new(Mutex::new(None)),
            dims,
            built: Arc::new(Mutex::new(false)),
            metric: DistanceMetric::Cosine,
        }
    }
    pub fn new_flat_clustered(dims: usize) -> Self {
        IndexBackend::FlatClustered {
            buf: Arc::new(Mutex::new(Vec::new())),
            index: Arc::new(Mutex::new(None)),
            dims,
            built: Arc::new(Mutex::new(false)),
            metric: DistanceMetric::Cosine,
        }
    }
    pub fn new_flat(dims: usize) -> Self {
        IndexBackend::Flat {
            data: Arc::new(Mutex::new(Vec::new())),
            dims,
        }
    }
    pub fn new_pq_flat(dims: usize) -> Self {
        IndexBackend::PqFlat {
            buf: Arc::new(Mutex::new(Vec::new())),
            index: Arc::new(Mutex::new(None)),
            dims,
            built: Arc::new(Mutex::new(false)),
        }
    }
    pub fn new_hii_l0(dims: usize) -> Self {
        let config = HiiL0Config {
            dims,
            ..Default::default()
        };
        IndexBackend::HiiL0 {
            inner: Arc::new(Mutex::new(HiiL0::new(config))),
            dims,
        }
    }
    /// PqFlat scored 透传：聚类索引自身已返回按距离升序的 (id, dist)；未 build → 空。
    /// PqFlat scored passthrough: the clustered index already yields ascending
    /// (id, dist); unbuilt index → empty.
    ///
    /// 已知限制（TODO/后续 issue）：这里的距离不是余弦距离——无 mmap 时是 PQ 子空间
    /// 平方 L2 查表距离之和，有 mmap 时是 Phase-2 对原始向量重排的平方 L2（L2²）。
    /// HTTP 边缘统一套用 `score = 1 - distance` 后，pq-flat 的分数不保证落在
    /// `[-1, 1]`，且对模长不同的向量排序可能与余弦不同；后续应让 pq-flat 重排
    /// 按 Cosine 度量计算，或在响应中显式透出每后端的分数类型。
    /// EN: Known limitation — pq-flat distances are squared-L2 (PQ table sums without
    /// mmap; exact L2² rerank with mmap), NOT cosine. `score = 1 - distance` is therefore
    /// unbounded and ranking can differ from cosine for vectors of differing magnitude.
    fn search_pq_scored(
        index: &Mutex<Option<PqFlatClustered>>,
        q: &[f32],
        k: usize,
    ) -> Vec<(VectorId, f32)> {
        index
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.search(q, k))
            .unwrap_or_default()
    }

    /// 服务端所有写入路径的分量有限性总闸门（HTTP 两条 upsert、pgwire `upsert_batch`、
    /// 冷启动重放都经 `insert`/`insert_batch` 进入后端）。
    /// EN: The single finiteness gate for every server ingestion path (both HTTP
    /// upserts, pgwire `upsert_batch`, and cold-start WAL/snapshot replay).
    ///
    /// WHY（为什么闸门必须在服务端适配层而不能只靠库层或 HTTP 校验）：库层校验是
    /// 不对称的——[`HiiL0`] 硬拒非有限分量，[`HotBufferIndex`] 只校验维度，而
    /// Clustered/FlatClustered/PqFlat/Flat 的 buf 分支连 `Err` 都不返回（直接 extend）。
    /// 同时 pgwire binary float4/float8 经 `f32::from_bits` 解码，NaN/±Inf 原样通过
    /// 维度校验（OCR 第五轮发现的唯一毒数据入口）。在适配层统一 fail-fast，整批原子
    /// 拒绝、任何行都不部分写入。维度仍由各库层与协议边界各自负责，这里不重复。
    /// 冷启动重放自身先经 `partition_finite_batch` 过滤历史毒行，闸门对其为 no-op。
    /// EN: Library validation is asymmetric (HiiL0 hard-rejects; HotBuffer checks dims
    /// only; the buf-based backends never return Err), and pgwire binary floats decoded
    /// via f32::from_bits sailed through the dim check. This adapter-level gate rejects
    /// atomically. Replay filters poisoned rows beforehand, so the gate is a no-op there.
    fn reject_non_finite<'a>(
        rows: impl Iterator<Item = (VectorId, &'a [f32])>,
    ) -> Result<(), String> {
        for (id, v) in rows {
            if let Some(bad) = v.iter().find(|x| !x.is_finite()) {
                return Err(format!(
                    "non-finite component (NaN/Infinity) in vector id={id}: {bad}"
                ));
            }
        }
        Ok(())
    }

    pub async fn insert(&self, id: VectorId, vector: Vec<f32>) -> Result<(), String> {
        Self::reject_non_finite(std::iter::once((id, vector.as_slice())))?;
        match self {
            Self::Hnsw(idx) => idx.insert(id, vector).await,
            _ => self.insert_batch(vec![(id, vector)]).await,
        }
    }
    pub async fn insert_batch(&self, batch: Vec<(u64, Vec<f32>)>) -> Result<(), String> {
        Self::reject_non_finite(batch.iter().map(|(id, v)| (*id, v.as_slice())))?;
        match self {
            Self::Hnsw(idx) => idx.insert_batch(batch).await,
            Self::HiiL0 { inner, .. } => inner.lock().unwrap().insert_batch(batch),
            Self::Clustered { buf, .. }
            | Self::FlatClustered { buf, .. }
            | Self::PqFlat { buf, .. } => {
                buf.lock().unwrap().extend(batch);
                Ok(())
            }
            Self::Flat { data, .. } => {
                data.lock().unwrap().extend(batch);
                Ok(())
            }
        }
    }

    /// Flat 暴力 scored 检索：先按 mask 过滤，再按**余弦距离**计分排序。
    /// EN: Flat brute-force scored search — apply the allow-list mask first, then
    /// score with cosine distance.
    ///
    /// 为什么从裸 L2² 改成余弦：`distance_metric()` 对 Flat 报告的是 Cosine，
    /// Collection/HTTP 层统一按余弦把距离换算成 `score = 1 - distance`；旧 flat 用
    /// `Σ(a-b)²` 排序，在「同向不同模长」向量上会给出与余弦相反的排序，是潜在不一致。
    /// 与 HotBufferIndex 的 Cosine 路径共用 [`houyi_vector::distance::cosine`]
    /// （公式内部按范数归一化），因此**存储向量保持原样、仅在计分时归一化**；
    /// 零向量按余弦约定距离为 1。
    /// EN: Why cosine instead of raw L2² — `distance_metric()` reports Cosine for
    /// Flat and the collection/HTTP layers uniformly convert `score = 1 - distance`;
    /// raw L2² ranks same-direction different-magnitude vectors in the opposite
    /// order. Shares the same full-cosine formula (norm-normalized internally) as the
    /// HotBufferIndex Cosine path, so stored vectors are never mutated; zero vectors
    /// map to distance 1 by the cosine convention.
    #[allow(clippy::type_complexity)]
    fn flat_search_scored(
        data: &Mutex<Vec<(u64, Vec<f32>)>>,
        q: &[f32],
        k: usize,
        filter: Option<&[VectorId]>,
    ) -> Vec<(u64, f32)> {
        let guard = data.lock().unwrap();
        if guard.is_empty() {
            return Vec::new();
        }
        // 过滤 mask 每次调用只构建一次 HashSet：flat 是暴力扫描，旧实现直接忽略
        // mask，会让混合检索的结构化过滤静默失效（返回未过滤结果）。
        // EN: Build the allow-list HashSet once per brute-force scan. The old flat
        // path ignored the mask, so structured filters silently returned unfiltered hits.
        let allow: Option<std::collections::HashSet<u64>> =
            filter.map(|ids| ids.iter().copied().collect());
        let mut scored: Vec<(u64, f32)> = guard
            .iter()
            .filter(|(id, _)| allow.as_ref().is_none_or(|set| set.contains(id)))
            .map(|(id, v)| {
                // 余弦距离（内部按范数归一化，存储向量不做任何改写）。
                // Cosine distance (normalized by norm inside; stored vectors untouched).
                let d: f32 = houyi_vector::distance::cosine(q, v);
                (*id, d)
            })
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        scored.truncate(k);
        scored
    }

    pub async fn search(&self, q: &[f32], k: usize, f: Option<&[VectorId]>) -> Vec<VectorId> {
        Self::strip(self.search_scored_with_ef_override(q, k, f, None).await)
    }
    pub async fn search_with_ef(
        &self,
        q: &[f32],
        k: usize,
        f: Option<&[VectorId]>,
        ef: Option<usize>,
    ) -> Vec<VectorId> {
        Self::strip(self.search_scored_with_ef_override(q, k, f, ef).await)
    }
    pub async fn search_with_ef_override(
        &self,
        q: &[f32],
        k: usize,
        f: Option<&[VectorId]>,
        ef: Option<usize>,
    ) -> Vec<VectorId> {
        Self::strip(self.search_scored_with_ef_override(q, k, f, ef).await)
    }

    /// id-only 入口统一剥离距离：所有后端只维护 scored 一条排序链路。
    /// EN: All id-only entry points strip distances here — each backend maintains a
    /// single scored sorting path.
    fn strip(pairs: Vec<(VectorId, f32)>) -> Vec<VectorId> {
        pairs.into_iter().map(|(id, _)| id).collect()
    }

    /// scored 检索：所有后端统一返回 `(id, 距离)`，按距离升序（越小越近）。
    /// 距离语义按后端区分：hnsw/clustered/flat-clustered/flat/hii-l0 均为余弦距离
    /// （HNSW/Clustered 系列构图时即按 Cosine，Flat 计分用全余弦公式，HiiL0 写入即
    /// 归一化）；**pq-flat 为例外**，返回平方 L2（无 mmap 为 PQ 查表距离和，有
    /// mmap 为精确 L2² 重排），不是余弦距离。距离→相似度的换算只在 HTTP 边缘做，
    /// 并由 [`Self::distance_is_cosine`] 告知是否可按 `[-1,1]` 钳制。
    /// EN: Scored search across backends: ascending (id, distance), smaller = closer.
    /// hnsw/clustered/flat-clustered/flat/hii-l0 return cosine distance; pq-flat is the
    /// explicit exception and returns squared-L2 (PQ table sum / exact L2² rerank).
    /// The distance→similarity conversion happens only at the HTTP edge.
    #[allow(clippy::type_complexity)]
    pub async fn search_scored_with_ef_override(
        &self,
        q: &[f32],
        k: usize,
        f: Option<&[VectorId]>,
        ef: Option<usize>,
    ) -> Vec<(VectorId, f32)> {
        match self {
            Self::HiiL0 { inner, .. } => {
                let mut l0 = inner.lock().unwrap();
                l0.search_scored(q, k)
            }
            Self::Hnsw(idx) => idx.search_scored_with_ef_override(q, k, f, ef).await,
            Self::Clustered { index, .. } => Self::search_c_scored(index, q, k),
            Self::FlatClustered { index, .. } => Self::search_fc_scored(index, q, k),
            Self::Flat { data, .. } => Self::flat_search_scored(data, q, k, f),
            Self::PqFlat { index, .. } => Self::search_pq_scored(index, q, k),
        }
    }

    /// Clustered scored 透传：c.search 自身已返回按距离升序的 (id, dist)；未 build → 空。
    /// Clustered scored passthrough: c.search already returns ascending pairs;
    /// an unbuilt index yields an empty vec.
    fn search_c_scored(
        index: &Mutex<Option<ClusteredHnsw>>,
        q: &[f32],
        k: usize,
    ) -> Vec<(VectorId, f32)> {
        index
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.search(q, k))
            .unwrap_or_default()
    }
    fn search_fc_scored(
        index: &Mutex<Option<FlatClustered>>,
        q: &[f32],
        k: usize,
    ) -> Vec<(VectorId, f32)> {
        index
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.search(q, k))
            .unwrap_or_default()
    }

    /// 删除单个向量 id：hnsw（墓碑 + 四路摘除）与 flat（直接剔除）支持；
    /// 其余后端 fail-fast 返回 Err——与混合检索 filter 的支持面（hnsw/flat）同口径，
    /// 宁可显式拒绝也不静默「删了还能搜到」。
    /// EN: Delete one vector id — supported on hnsw (tombstone + four-way removal)
    /// and flat (direct retain); all other backends fail fast with Err, mirroring
    /// the hybrid-filter support surface. Better an explicit refusal than a delete
    /// whose hits keep showing up.
    pub async fn delete(&self, id: VectorId) -> Result<bool, String> {
        match self {
            Self::Hnsw(idx) => Ok(idx.remove(id).await),
            Self::Flat { data, .. } => {
                let mut guard = data.lock().unwrap();
                let before = guard.len();
                guard.retain(|(vid, _)| *vid != id);
                Ok(before != guard.len())
            }
            other => Err(format!(
                "delete is unsupported for index backend '{}' (supported: hnsw, flat)",
                other.backend_name()
            )),
        }
    }

    /// 后端名（错误诊断用；不引入 Display 以免被当成稳定 API 依赖）。
    /// EN: Backend name for diagnostics (no Display impl — not a stable API surface).
    fn backend_name(&self) -> &'static str {
        match self {
            Self::Hnsw(_) => "hnsw",
            Self::Clustered { .. } => "clustered",
            Self::FlatClustered { .. } => "flat-clustered",
            Self::Flat { .. } => "flat",
            Self::PqFlat { .. } => "pq-flat",
            Self::HiiL0 { .. } => "hii-l0",
        }
    }

    pub async fn pending_build_count(&self) -> usize {
        match self {
            Self::HiiL0 { inner, .. } => inner.lock().unwrap().size(),
            Self::Hnsw(idx) => idx.pending_build_count().await,
            Self::Clustered { buf, .. }
            | Self::FlatClustered { buf, .. }
            | Self::PqFlat { buf, .. } => buf.lock().unwrap().len(),
            Self::Flat { .. } => 0,
        }
    }
    pub async fn indexed_count(&self) -> usize {
        match self {
            Self::HiiL0 { inner, .. } => inner.lock().unwrap().size(),
            Self::Hnsw(idx) => idx.indexed_count().await,
            Self::Clustered { index, .. } => index
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.total_vectors())
                .unwrap_or(0),
            Self::FlatClustered { index, .. } => index
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.total_vectors())
                .unwrap_or(0),
            Self::Flat { data, .. } => data.lock().unwrap().len(),
            Self::PqFlat { index, .. } => index
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.total_vectors())
                .unwrap_or(0),
        }
    }
    pub async fn hot_buffer_count(&self) -> usize {
        match self {
            Self::Hnsw(idx) => idx.hot_buffer_count().await,
            _ => 0,
        }
    }

    pub fn hnsw_m(&self) -> usize {
        match self {
            Self::Hnsw(idx) => idx.hnsw_m(),
            _ => 24,
        }
    }
    pub fn hnsw_ef_search(&self) -> usize {
        match self {
            Self::Hnsw(idx) => idx.hnsw_ef_search(),
            _ => 256,
        }
    }
    pub fn hnsw_ef_construction_value(&self) -> usize {
        match self {
            Self::Hnsw(idx) => idx.hnsw_ef_construction_value(),
            _ => 400,
        }
    }
    pub fn set_hnsw_m(&self, m: usize) -> Result<(), String> {
        if let Self::Hnsw(idx) = self {
            idx.set_hnsw_m(m)
        } else {
            Ok(())
        }
    }
    pub fn set_hnsw_ef_search(&self, ef: usize) -> Result<(), String> {
        if let Self::Hnsw(idx) = self {
            idx.set_hnsw_ef_search(ef)
        } else {
            Ok(())
        }
    }
    pub fn set_hnsw_ef_construction(&self, ef: usize) -> Result<(), String> {
        if let Self::Hnsw(idx) = self {
            idx.set_hnsw_ef_construction(ef)
        } else {
            Ok(())
        }
    }

    pub fn builder_metrics(&self) -> BuilderMetricsSnapshot {
        match self {
            Self::Hnsw(idx) => idx.builder_metrics(),
            _ => BuilderMetricsSnapshot {
                flush_count: 0,
                total_flushed_items: 0,
                max_pending_queue: 0,
                last_flush_items: 0,
                graph_edges_built: 0,
                graph_edges_level0: 0,
                graph_edges_upper: 0,
            },
        }
    }
    pub async fn run_async_builder_once(&self) -> AsyncBuilderOnceReport {
        match self {
            Self::Hnsw(idx) => idx.run_async_builder_once().await,
            _ => AsyncBuilderOnceReport {
                flushed: 0,
                plan_mode: Some(HnswBatchPlanMode::Serial),
            },
        }
    }
    pub fn start_async_builder_worker(
        &self,
        th: usize,
        t: u64,
        p: FlushPolicy,
    ) -> Option<houyi_vector::BuilderWorker> {
        match self {
            Self::Hnsw(idx) => Some(idx.start_async_builder_worker(th, t, p)),
            _ => None,
        }
    }

    pub async fn export_snapshot_vectors(&self) -> Result<Vec<(u64, Vec<f32>)>, String> {
        match self {
            Self::HiiL0 { inner, .. } => Ok(inner.lock().unwrap().export_vectors()),
            Self::Hnsw(idx) => idx.export_snapshot_vectors().await,
            Self::Clustered { buf, .. }
            | Self::FlatClustered { buf, .. }
            | Self::PqFlat { buf, .. } => Ok(buf.lock().unwrap().clone()),
            Self::Flat { data, .. } => Ok(data.lock().unwrap().clone()),
        }
    }

    /// 按 id 批量取向量（仅 Hnsw 支持；其余返回空，调用方回退到索引检索）。
    /// EN: Bulk fetch by id (Hnsw only; others return empty and callers fall back).
    pub async fn vectors_by_ids(&self, ids: &[u64]) -> Vec<(u64, Vec<f32>)> {
        match self {
            Self::Hnsw(idx) => idx.vectors_by_ids(ids).await,
            _ => Vec::new(),
        }
    }

    /// 图内节点数（诊断；仅 Hnsw）。
    /// EN: Graph node count (diagnostic; Hnsw only).
    pub async fn graph_node_count(&self) -> usize {
        match self {
            Self::Hnsw(idx) => idx.graph_node_count().await,
            _ => 0,
        }
    }

    /// 写入代数（过滤缓存新鲜度；仅 Hnsw）。
    /// EN: Write generation for filter-cache freshness (Hnsw only).
    pub fn write_generation(&self) -> u64 {
        match self {
            Self::Hnsw(idx) => idx.write_generation(),
            _ => 0,
        }
    }

    /// 编码 HNSW 图快照（P4.4.0）：仅 `Hnsw` 变体支持，其余返回 `None`。
    /// 调用前必须先 drain 构建队列（由调用方保证）。
    /// EN: Encode the HNSW graph snapshot (P4.4.0) — `Hnsw` variant only;
    /// other backends return `None`. Caller drains the builder queue first.
    pub async fn encode_graph_snapshot(&self) -> Option<Vec<u8>> {
        match self {
            Self::Hnsw(idx) => Some(idx.encode_graph_snapshot().await),
            _ => None,
        }
    }

    /// 从图快照恢复：仅 `Hnsw` 变体支持（外层 `Some`），失败（结构不符/
    /// 损坏/非空目标）在内层 `Err`，调用方弃用快照全量重放。
    /// EN: Restore from a graph snapshot — `Hnsw` only (outer `Some`); the
    /// inner `Err` covers mismatch/corruption/non-empty target, in which case
    /// the caller discards the snapshot and falls back to full replay.
    pub async fn restore_graph_snapshot(
        &self,
        payload: &[u8],
    ) -> Option<Result<(), houyi_vector::GraphSnapshotError>> {
        match self {
            Self::Hnsw(idx) => Some(idx.restore_graph_snapshot(payload).await),
            _ => None,
        }
    }
    /// 报告后端的**标称构图度量**（nominal build metric），用于快照头等「按什么度量
    /// 构建」的元数据场景。
    /// EN: Reports the backend's *nominal build metric* — metadata for "what metric was
    /// this index built with" (e.g. snapshot headers).
    ///
    /// 重要：本方法的返回值**不保证等于 scored 距离的实际口径**，不能据此决定分数能否
    /// 钳制到 `[-1, 1]`。唯一的分歧变体是 **pq-flat**：它建表时标称 Cosine（本方法
    /// 返回 Cosine），但 `search` 实际返回平方 L2 距离（PQ 查表距离和 / 精确 L2² 重排，
    /// 见 `search_pq_scored` 的已知限制），属于库层 by-design 的口径分离。需要 score
    /// 语义（余弦 vs L2²）的消费者必须改用 [`Self::distance_is_cosine`]，它对 pq-flat
    /// 恒为 false。
    /// EN: The reported metric is NOT guaranteed to match the scored distances' actual
    /// semantics, so it must not gate score clamping. The sole divergent variant is
    /// pq-flat: it nominally reports Cosine here, yet `search` returns squared-L2
    /// distances (PQ table sums / exact L2² rerank — see `search_pq_scored`). Consumers
    /// that need score semantics (cosine vs L2²) must use [`Self::distance_is_cosine`],
    /// which is always false for pq-flat.
    pub fn distance_metric(&self) -> DistanceMetric {
        match self {
            Self::Hnsw(idx) => idx.distance_metric(),
            // Clustered 系列按构图时记录的实际度量报告（库 build 公开接受 L2）。
            // Clustered variants report the actual recorded build metric (the library
            // build APIs publicly accept L2).
            Self::Clustered { metric, .. } | Self::FlatClustered { metric, .. } => *metric,
            // Flat/HiiL0 按余弦计分；PqFlat 建表标称 Cosine（实际 scored 距离为 L2²，
            // 由 distance_is_cosine 单独显式排除）。
            // Flat/HiiL0 score with cosine; PqFlat nominally reports Cosine (its scored
            // distance is L2², excluded explicitly by distance_is_cosine).
            Self::Flat { .. } | Self::PqFlat { .. } | Self::HiiL0 { .. } => DistanceMetric::Cosine,
        }
    }

    /// scored 距离是否为余弦语义：决定 HTTP 边缘 `score = 1 - distance` 能否钳制到
    /// `[-1, 1]`。pq-flat（平方 L2 重排距离，分数合法越界）恒为 false；HNSW 与
    /// Clustered/FlatClustered 按构图时的实际度量判定（库层 build 公开接受 L2，
    /// L2 构图的 scored 距离是 L2²）；Flat/HiiL0 恒为余弦。
    /// 注意不能只看 [`Self::distance_metric`]——pq-flat 建表时标称 Cosine，
    /// 但 `search` 实际返回的是 L2²（见 `search_pq_scored` 的已知限制说明）；
    /// HNSW 看 `HotBufferIndex::new_with_metric` 的实际度量，Clustered 系列看变体
    /// 记录的构图度量，均不能硬编码为余弦。当前服务器装配只构造 Cosine 索引，L2
    /// 判定属于库级契约的纵深防御。
    /// EN: pq-flat reports Cosine at build time yet returns L2² from search, so that
    /// variant is excluded explicitly; HNSW/Clustered/FlatClustered are decided by their
    /// actual build metric because the library build APIs publicly accept L2 (server
    /// assembly constructs Cosine only today).
    pub fn distance_is_cosine(&self) -> bool {
        match self {
            // pq-flat 建表标称 Cosine，但 scored search 返回 L2²：必须显式排除。
            // pq-flat nominally reports Cosine yet scored search returns L2².
            Self::PqFlat { .. } => false,
            Self::Hnsw(idx) => idx.distance_metric() == DistanceMetric::Cosine,
            // Clustered/FlatClustered 与 HNSW 同一口径：库层 build 公开接受 L2 并在
            // search 时按 L2² 计分（metric 只存在库类型私有字段里、无访问器），因此
            // 变体显式记录构图度量。服务器当前只走 Cosine 构图，L2 判定属纵深防御。
            // EN: Same gate as HNSW — the library build APIs publicly accept L2 and
            // honor it at search (the metric lives in a private field with no accessor),
            // so the variant records its build metric. Server assembly builds Cosine
            // only today; the L2 arm is defense in depth.
            Self::Clustered { metric, .. } | Self::FlatClustered { metric, .. } => {
                *metric == DistanceMetric::Cosine
            }
            // 已知余弦后端正向枚举（WHY: 不用 `_ => true`——未来新增非余弦后端时
            // 穷尽匹配会在编译期强制显式裁决，杜绝「默认 clamp 改写真实分数」的
            // fail-open，这正是 pq-flat 这次修掉的 bug 类别）。
            // EN: Positive enumeration — no catch-all: a future non-cosine backend
            // fails this exhaustiveness check at compile time instead of silently
            // getting its out-of-range scores clamped (the pq-flat bug class).
            Self::Flat { .. } | Self::HiiL0 { .. } => true,
        }
    }

    pub async fn optimize_build(&self) -> usize {
        match self {
            Self::HiiL0 { inner, .. } => {
                let mut l0 = inner.lock().unwrap();
                l0.rebuild_graph();
                l0.size()
            }
            Self::Hnsw(_) => 0,
            Self::Flat { data, .. } => {
                let n = data.lock().unwrap().len();
                n
            }
            Self::Clustered {
                buf,
                index,
                dims,
                built,
                metric,
            } => {
                let mut b = built.lock().unwrap();
                if *b {
                    return 0;
                }
                let data = {
                    let mut bg = buf.lock().unwrap();
                    std::mem::take(&mut *bg)
                };
                if data.is_empty() {
                    return 0;
                }
                let n = data.len();
                let ids: Vec<u64> = data.iter().map(|(id, _)| *id).collect();
                let vecs: Vec<f32> = data.iter().flat_map(|(_, v)| v.clone()).collect();
                drop(data);
                let cfg = ClusterConfig {
                    hnsw_ef_construction: 400,
                    hnsw_ef_search: 256,
                    hnsw_m: 24,
                    ..ClusterConfig::infer(n, *dims)
                };
                // 透传变体记录的实际构图度量：build 的度量决定 scored 距离是余弦还是
                // L2²，旧实现硬编码 Cosine 会让 metric=L2 的变体「自称 L2、按 Cosine
                // 构图」，与 distance_metric()/distance_is_cosine() 三处分歧。
                // EN: Thread the variant's recorded metric — the build metric decides
                // whether scored distances are cosine or L2²; hard-coding Cosine made an
                // L2 variant report L2 while actually building/searching by cosine.
                let ch = ClusteredHnsw::build(&vecs, &ids, *dims, *metric, &cfg);
                let count = ch.total_vectors();
                *index.lock().unwrap() = Some(ch);
                *b = true;
                count
            }
            Self::FlatClustered {
                buf,
                index,
                dims,
                built,
                metric,
            } => {
                let mut b = built.lock().unwrap();
                if *b {
                    return 0;
                }
                let data = {
                    let mut bg = buf.lock().unwrap();
                    std::mem::take(&mut *bg)
                };
                if data.is_empty() {
                    return 0;
                }
                let n = data.len();
                let ids: Vec<u64> = data.iter().map(|(id, _)| *id).collect();
                let vecs: Vec<f32> = data.iter().flat_map(|(_, v)| v.clone()).collect();
                drop(data);
                let c = (n as f64).sqrt().ceil() as usize;
                // 同 Clustered 分支：透传变体度量，保证构图口径与报告口径一致。
                // Same as the Clustered arm: thread the variant metric so the built and
                // reported metrics never diverge.
                let fc =
                    FlatClustered::build(&vecs, &ids, *dims, *metric, c, 15, 0xA7E5_72D8_B7DB_BEEF);
                let count = fc.total_vectors();
                *index.lock().unwrap() = Some(fc);
                *b = true;
                count
            }
            Self::PqFlat {
                buf,
                index,
                dims,
                built,
                ..
            } => {
                let mut b = built.lock().unwrap();
                if *b {
                    return 0;
                }
                let data = {
                    let mut bg = buf.lock().unwrap();
                    std::mem::take(&mut *bg)
                };
                if data.is_empty() {
                    return 0;
                }
                let n = data.len();
                let ids: Vec<u64> = data.iter().map(|(id, _)| *id).collect();
                let vecs: Vec<f32> = data.iter().flat_map(|(_, v)| v.clone()).collect();
                drop(data);
                let c = (n as f64).sqrt().ceil() as usize;
                // PqFlatClustered::build 的 metric 形参 by design 被忽略（库层签名为
                // `_metric`：PQ 距离恒为平方 L2，建表度量不参与距离计算），变体也不
                // 存储 metric；继续传标称 Cosine，真实 score 语义由 distance_is_cosine()
                // 对 PqFlat 恒 false 单独表达。
                // EN: PqFlatClustered::build ignores its metric parameter by design
                // (`_metric` — PQ distances are always squared-L2); the variant stores no
                // metric. Keep passing the nominal Cosine; real score semantics are
                // expressed by distance_is_cosine() returning false for PqFlat.
                let pf = PqFlatClustered::build(
                    &vecs,
                    &ids,
                    *dims,
                    DistanceMetric::Cosine,
                    c,
                    15,
                    0xA7E5_72D8_B7DB_BEEF,
                );
                let count = pf.total_vectors();
                *index.lock().unwrap() = Some(pf);
                *b = true;
                count
            }
        }
    }
}

impl From<Arc<HotBufferIndex>> for IndexBackend {
    fn from(idx: Arc<HotBufferIndex>) -> Self {
        IndexBackend::Hnsw(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn drain_hnsw(backend: &IndexBackend) {
        for _ in 0..1000 {
            if backend.run_async_builder_once().await.flushed == 0 {
                break;
            }
        }
    }

    fn batch_200_d16() -> Vec<(u64, Vec<f32>)> {
        let dims = 16usize;
        (0..200_u64)
            .map(|id| {
                (
                    id,
                    (0..dims)
                        .map(|j| ((id as usize * 13 + j * 7) % 200) as f32 / 200.0)
                        .collect(),
                )
            })
            .collect()
    }

    /// scored 契约：结果数 == k（数据足够时）、距离升序、id 序列与 id-only 入口一致。
    /// EN: Scored contract across backends: k results, ascending distances,
    /// ids identical to the id-only entry point.
    #[tokio::test]
    async fn hnsw_scored_search_contract() {
        let backend = IndexBackend::new_hnsw(16);
        backend.insert_batch(batch_200_d16()).await.unwrap();
        drain_hnsw(&backend).await;
        assert_eq!(backend.indexed_count().await, 200);

        let q = vec![0.42_f32; 16];
        let scored = backend
            .search_scored_with_ef_override(&q, 10, None, Some(256))
            .await;
        assert_eq!(scored.len(), 10);
        for pair in scored.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distances must be ascending");
        }
        let scored_ids: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
        let ids_only = backend
            .search_with_ef_override(&q, 10, None, Some(256))
            .await;
        assert_eq!(scored_ids, ids_only);
    }

    /// HNSW 精确匹配距离 ≈ 0（全余弦公式，非单位向量也成立）。
    /// EN: Exact-match query yields distance ~0 even for non-unit vectors.
    #[tokio::test]
    async fn hnsw_scored_exact_match_near_zero() {
        let backend = IndexBackend::new_hnsw(4);
        let target = vec![3.0_f32, 0.0, -1.0, 2.0];
        backend
            .insert_batch(vec![
                (1, vec![0.0, 1.0, 0.0, 0.0]),
                (2, target.clone()),
                (3, vec![1.0, 1.0, 1.0, 1.0]),
            ])
            .await
            .unwrap();
        // 批量写入先进 builder_queue，排空后才进入主图可被检索。
        // Batched rows land in builder_queue first; drain before searching.
        drain_hnsw(&backend).await;
        let scored = backend
            .search_scored_with_ef_override(&target, 3, None, Some(256))
            .await;
        assert_eq!(scored.first().map(|(id, _)| *id), Some(2));
        assert!(scored.first().unwrap().1.abs() < 1e-5);
    }

    /// Flat 余弦修正：同向但模长大（L2²=81）的向量必须排在正交但 L2 更近
    /// （L2²=2）的向量之前——旧 flat 用裸 L2² 排序，会把正交向量误排第一。
    /// EN: Flat cosine correction — a same-direction far-magnitude vector
    /// (L2²=81) must outrank an orthogonal vector (L2²=2); the old raw-L2²
    /// scoring ranked the orthogonal one first despite distance_metric()=Cosine.
    #[tokio::test]
    async fn flat_scored_uses_cosine_not_raw_l2() {
        let backend = IndexBackend::new_flat(2);
        backend
            .insert_batch(vec![
                // q=[1,0]：id=1 同向模长 10（余弦距离 0，L2²=81）；
                // id=2 正交单位向量（余弦距离 1，L2²=2）。
                // EN: q=[1,0]: id=1 is same-direction with magnitude 10 (cosine
                // distance 0, L2²=81); id=2 is an orthogonal unit vector (cosine
                // distance 1, L2²=2).
                (1, vec![10.0, 0.0]),
                (2, vec![0.0, 1.0]),
            ])
            .await
            .unwrap();
        let q = vec![1.0, 0.0];
        let scored = backend
            .search_scored_with_ef_override(&q, 2, None, None)
            .await;
        assert_eq!(scored.len(), 2);
        assert_eq!(
            scored[0].0, 1,
            "cosine: same-direction vector must rank first"
        );
        assert!(
            scored[0].1.abs() < 1e-6,
            "same-direction cosine distance ~0"
        );
        assert!(
            (scored[1].1 - 1.0).abs() < 1e-6,
            "orthogonal cosine distance ~1"
        );
        for pair in scored.windows(2) {
            assert!(pair[0].1 <= pair[1].1);
        }
        // id-only 链路同步改为余弦排序。
        // EN: The id-only entry point inherits the corrected cosine ordering.
        let ids = backend.search_with_ef_override(&q, 2, None, None).await;
        assert_eq!(ids, vec![1, 2]);

        // mask 仍然生效：先按 mask 过滤再计分。
        // EN: Mask support retained — filter before scoring.
        let masked = backend
            .search_scored_with_ef_override(&q, 2, Some(&[2]), None)
            .await;
        assert_eq!(
            masked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![2]
        );

        // 空结果（无命中 mask）→ 空 pairs。
        // EN: Empty result (mask matches nothing) → empty pairs.
        let empty = backend
            .search_scored_with_ef_override(&q, 2, Some(&[999]), None)
            .await;
        assert!(empty.is_empty());
    }

    /// HII L0 scored：暴力路径距离升序、精确匹配 ≈ 0、与 id-only 一致。
    /// EN: HiiL0 scored brute-force path: ascending, exact match ~0, ids match.
    #[tokio::test]
    async fn hii_l0_scored_search_contract() {
        let backend = IndexBackend::new_hii_l0(4);
        backend
            .insert_batch(vec![
                (1, vec![1.0, 0.0, 0.0, 0.0]),
                (2, vec![0.0, 1.0, 0.0, 0.0]),
                (3, vec![0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .unwrap();
        let q = vec![1.0, 0.0, 0.0, 0.0];
        let scored = backend
            .search_scored_with_ef_override(&q, 2, None, None)
            .await;
        assert_eq!(scored.len(), 2);
        assert_eq!(scored[0].0, 1);
        assert!(scored[0].1.abs() < 1e-6);
        assert!(scored[0].1 <= scored[1].1);
        let scored_ids: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            scored_ids,
            backend.search_with_ef_override(&q, 2, None, None).await
        );
    }

    /// Clustered/FlatClustered/PqFlat：c.search(q,k) 的 (id, dist) 对直接透出，
    /// None 索引（未 build）→ 空。
    /// EN: Clustered backends pass (id, dist) pairs through; unbuilt index → empty.
    #[tokio::test]
    async fn clustered_backends_scored_passthrough() {
        for (backend, name) in [
            (IndexBackend::new_clustered(16), "clustered"),
            (IndexBackend::new_flat_clustered(16), "flat-clustered"),
            (IndexBackend::new_pq_flat(16), "pq-flat"),
        ] {
            // 未构建索引时必须返回空而非 panic。
            // EN: An unbuilt index must return empty rather than panic.
            let q = vec![0.42_f32; 16];
            assert!(
                backend
                    .search_scored_with_ef_override(&q, 10, None, None)
                    .await
                    .is_empty(),
                "{name}: unbuilt index must return empty"
            );

            backend.insert_batch(batch_200_d16()).await.unwrap();
            let built = backend.optimize_build().await;
            assert_eq!(built, 200, "{name}: build must index all rows");
            let scored = backend
                .search_scored_with_ef_override(&q, 10, None, None)
                .await;
            assert!(!scored.is_empty(), "{name}: scored search must return hits");
            for pair in scored.windows(2) {
                assert!(pair[0].1 <= pair[1].1, "{name}: distances ascending");
            }
            let scored_ids: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
            assert_eq!(
                scored_ids,
                backend.search_with_ef_override(&q, 10, None, None).await,
                "{name}: scored/id-only ids must match"
            );
        }
    }

    /// OCR 第五轮回归：服务端写入总闸门必须对**所有后端**拒绝 NaN/±Inf。库层校验
    /// 不对称——HiiL0 硬拒非有限分量，HotBufferIndex 只查维度，Clustered/FlatClustered/
    /// PqFlat/Flat 连 Err 都不返回；pgwire binary float4/float8 经 `f32::from_bits`
    /// 解码，毒分量原样通过维度校验，是唯一仍开放的毒数据入口。整批原子拒绝：毒批次
    /// 内的合法行也不得部分写入；单行入口 `insert` 共用同一闸门。
    /// EN: The server-side write choke point must reject NaN/±Inf on EVERY backend:
    /// library validation is asymmetric (HiiL0 hard-rejects; HotBuffer checks dims only;
    /// Clustered/Flat/PqFlat/Flat never return Err) and pgwire binary floats decoded via
    /// f32::from_bits sailed through the dim check. A poisoned batch fails atomically and
    /// the single-row `insert` entry shares the gate.
    #[tokio::test]
    async fn insert_gate_rejects_non_finite_on_every_backend() {
        #[rustfmt::skip]
        let makers: [fn(usize) -> IndexBackend; 6] = [
            IndexBackend::new_hnsw,
            IndexBackend::new_clustered,
            IndexBackend::new_flat_clustered,
            IndexBackend::new_flat,
            IndexBackend::new_pq_flat,
            IndexBackend::new_hii_l0,
        ];
        for make in makers {
            let backend = make(4);
            // 一条合法行先落（用于确认毒批次前后没有部分写入）。
            // One good row lands first; it marks the pre-poison state.
            backend
                .insert_batch(vec![(1, vec![1.0, 0.0, 0.0, 0.0])])
                .await
                .unwrap();

            for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let err = backend
                    .insert_batch(vec![
                        (2, vec![1.0, 0.0, 0.0, 0.0]),
                        (3, vec![1.0, bad, 0.0, 0.0]),
                    ])
                    .await
                    .expect_err("poisoned batch must be rejected on every backend");
                assert!(err.contains("non-finite"), "unexpected message: {err}");

                let err = backend
                    .insert(9, vec![0.0, bad, 0.0, 0.0])
                    .await
                    .expect_err("single-row insert shares the finiteness gate");
                assert!(err.contains("non-finite"), "unexpected message: {err}");
            }

            // 原子性：导出的行里只有最初的合法 id=1。
            // Atomicity: only the original good row is observable afterwards.
            let mut ids: Vec<u64> = backend
                .export_snapshot_vectors()
                .await
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            ids.sort_unstable();
            assert_eq!(ids, vec![1], "no poisoned or partial rows may land");
        }
    }

    /// OCR 回归：clamp 判定必须按后端真实语义——pq-flat 永远非余弦（标称 Cosine 但
    /// scored search 返回 L2²）；HNSW 按建索引实际度量（库层公开支持 L2）；其余后端
    /// 当前均为余弦。
    /// EN: The clamp gate follows real semantics — pq-flat is never cosine (reports
    /// Cosine but returns L2²), HNSW follows its actual build metric (L2 is publicly
    /// constructible at library level), every other current backend is cosine.
    #[tokio::test]
    async fn distance_is_cosine_follows_metric_and_pq_flat_exception() {
        assert!(IndexBackend::new_hnsw(4).distance_is_cosine());
        assert!(IndexBackend::new_flat(4).distance_is_cosine());
        assert!(IndexBackend::new_clustered(4).distance_is_cosine());
        assert!(IndexBackend::new_flat_clustered(4).distance_is_cosine());
        assert!(IndexBackend::new_hii_l0(4).distance_is_cosine());
        assert!(!IndexBackend::new_pq_flat(4).distance_is_cosine());
        let l2_hnsw = IndexBackend::Hnsw(Arc::new(HotBufferIndex::new_with_metric(
            4,
            DistanceMetric::L2,
        )));
        assert!(!l2_hnsw.distance_is_cosine());
    }

    /// OCR 回归（fail-closed 纵深防御）：ClusteredHnsw/FlatClustered 的库层 build 公开
    /// 接受 L2 并在 search 时按 L2² 计分（metric 仅存于库类型私有字段）。若变体不记录
    /// 实际构图度量，`distance_is_cosine()` 会硬编码 true，把 L2² 分数误钳进 [-1,1]——
    /// 与 pq-flat 同类的 fail-open。L2 构图的变体必须报告非余弦。
    /// EN: The library build APIs publicly accept L2 for clustered indexes and score by
    /// L2²; a variant that failed to record the build metric would hard-code cosine and
    /// wrongly clamp L2² scores into [-1,1] (the pq-flat fail-open class). L2-built
    /// clustered variants must report non-cosine.
    #[tokio::test]
    async fn clustered_l2_build_reports_non_cosine_and_still_searches() {
        let dims = 16usize;
        let batch = batch_200_d16();
        let ids: Vec<u64> = batch.iter().map(|(id, _)| *id).collect();
        let vecs: Vec<f32> = batch.iter().flat_map(|(_, v)| v.clone()).collect();
        let cfg = ClusterConfig {
            hnsw_ef_construction: 400,
            hnsw_ef_search: 256,
            hnsw_m: 24,
            ..ClusterConfig::infer(200, dims)
        };

        let ch = ClusteredHnsw::build(&vecs, &ids, dims, DistanceMetric::L2, &cfg);
        let l2_clustered = IndexBackend::Clustered {
            buf: Arc::new(Mutex::new(Vec::new())),
            index: Arc::new(Mutex::new(Some(ch))),
            dims,
            built: Arc::new(Mutex::new(true)),
            metric: DistanceMetric::L2,
        };
        assert!(!l2_clustered.distance_is_cosine());
        assert_eq!(l2_clustered.distance_metric(), DistanceMetric::L2);
        let q = vec![0.42_f32; dims];
        let hits = l2_clustered
            .search_scored_with_ef_override(&q, 5, None, None)
            .await;
        assert!(!hits.is_empty(), "L2 clustered index still returns hits");

        let clusters = (200.0_f64).sqrt().ceil() as usize;
        let fc = FlatClustered::build(
            &vecs,
            &ids,
            dims,
            DistanceMetric::L2,
            clusters,
            15,
            0xA7E5_72D8_B7DB_BEEF,
        );
        let l2_flat_clustered = IndexBackend::FlatClustered {
            buf: Arc::new(Mutex::new(Vec::new())),
            index: Arc::new(Mutex::new(Some(fc))),
            dims,
            built: Arc::new(Mutex::new(true)),
            metric: DistanceMetric::L2,
        };
        assert!(!l2_flat_clustered.distance_is_cosine());
        assert_eq!(l2_flat_clustered.distance_metric(), DistanceMetric::L2);
        let hits = l2_flat_clustered
            .search_scored_with_ef_override(&q, 5, None, None)
            .await;
        assert!(
            !hits.is_empty(),
            "L2 flat-clustered index still returns hits"
        );
    }

    /// OCR 回归：`optimize_build` 必须把变体记录的构图度量透传给库层 build，而不是
    /// 硬编码 Cosine——否则变体字段自称 L2、实际图按 Cosine 构建，`distance_metric()`/
    /// `distance_is_cosine()` 与 scored 距离口径三者分裂（边缘会把 L2² 分数误当余弦
    /// clamp）。行为证据：远离全部数据（分量 0..1）的查询 q=[10;16] 在 L2² 口径下
    /// 最近距离远超余弦距离上界 2；若 build 被硬编码成 Cosine，本断言失败。
    /// EN: optimize_build must thread the variant's stored metric into the library build
    /// instead of hard-coding Cosine; otherwise metric reporting and actual scored
    /// distances diverge and the edge mis-clamps L2² scores. A far query (q=10 vs data
    /// in 0..1) yields squared-L2 distances far above the cosine upper bound 2.
    #[tokio::test]
    async fn optimize_build_threads_stored_l2_metric() {
        let dims = 16usize;
        let batch = batch_200_d16();
        let q = vec![10.0_f32; dims];

        // ClusteredHnsw 变体：未构图、metric=L2。
        // ClusteredHnsw variant: unbuilt buf, recorded metric L2.
        let clustered = IndexBackend::Clustered {
            buf: Arc::new(Mutex::new(batch.clone())),
            index: Arc::new(Mutex::new(None)),
            dims,
            built: Arc::new(Mutex::new(false)),
            metric: DistanceMetric::L2,
        };
        assert_eq!(clustered.optimize_build().await, 200);
        assert_eq!(clustered.distance_metric(), DistanceMetric::L2);
        assert!(!clustered.distance_is_cosine());
        let hits = clustered
            .search_scored_with_ef_override(&q, 5, None, None)
            .await;
        assert!(!hits.is_empty(), "built L2 clustered index returns hits");
        assert!(
            hits[0].1 > 2.0,
            "built metric must be L2 (squared-L2 distance > 2), got {}",
            hits[0].1
        );

        // FlatClustered 变体同口径。
        // Same contract for the FlatClustered variant.
        let flat_clustered = IndexBackend::FlatClustered {
            buf: Arc::new(Mutex::new(batch)),
            index: Arc::new(Mutex::new(None)),
            dims,
            built: Arc::new(Mutex::new(false)),
            metric: DistanceMetric::L2,
        };
        assert_eq!(flat_clustered.optimize_build().await, 200);
        assert_eq!(flat_clustered.distance_metric(), DistanceMetric::L2);
        assert!(!flat_clustered.distance_is_cosine());
        let hits = flat_clustered
            .search_scored_with_ef_override(&q, 5, None, None)
            .await;
        assert!(
            !hits.is_empty(),
            "built L2 flat-clustered index returns hits"
        );
        assert!(
            hits[0].1 > 2.0,
            "built metric must be L2 (squared-L2 distance > 2), got {}",
            hits[0].1
        );
    }
}
