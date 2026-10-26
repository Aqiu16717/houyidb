//! HII (Hybrid Intelligent Index) — 三层异构索引。
//! HII: three-tier heterogeneous index (L0 memory + L1 GPU + L2 NVMe SSD).
//!
//! 当前实现：仅 L0 内存增量层（实时写入可见）。
//! L1/L2 为 TODO 占位，待 GPU/CUDA 硬件就绪后实现。
//!
//! Current: L0 only (memtable + small HNSW for real-time write visibility).
//! L1/L2 are TODO placeholders pending GPU hardware availability.

use crate::distance::cosine_distance_prenormalized;
use crate::hnsw_graph::{HnswGraph, ZERO_NORM_BAND_NORM2};
use crate::VectorId;
use std::collections::BTreeMap;

/// 原地 L2 单位化（写入热路径，零额外分配/零整向量拷贝）。落入零范数阈值带
/// （[`ZERO_NORM_BAND_NORM2`]，即 `norm² ≤ f32::EPSILON`、`norm ≲ 3.45e-4`）时保持
/// 入参原样不动；阈值带内的极小向量不单位化，其点积距离对任何方向都 ≈ 1
/// （「匹配不到任何向量」），调用方依赖「零向量保持全 0」的约定。
/// EN: In-place L2 normalization for the write hot path (no extra allocation or
/// full-vector copy). Inside the shared zero-norm band ([`ZERO_NORM_BAND_NORM2`],
/// `norm² ≤ f32::EPSILON`, i.e. norm ≲ 3.45e-4) the input is left untouched; such tiny
/// vectors match no direction (distance ≈ 1 everywhere).
///
/// WHY（范数平方为什么用 f64 累加）：HTTP 边界只校验维度，分量可以是任意有限 f32。
/// 若用 f32 累加平方和，单分量 |x| ≳ 1.8e19（平方即超 f32::MAX）或高维平方和溢出时
/// `norm2 = inf`，`inv = 0`，合法方向会被不可逆地腐蚀成全 0 向量（对一切向量距离恒为
/// 1，并会经 snapshot/WAL 把零向量持久化重放）。f64 可容纳 f32 分量的平方和而不溢出。
/// 注意这与图路径 `HnswGraph::normalize_for_cosine` 的 f32 累加存在刻意保留的精度差异：
/// 零范数阈值常量双方共用，但图路径改动会影响已建图数值、需走 recall 门禁，故暂不动；
/// 查询侧在进图之前先用本 f64 路径单位化（见 `search_scored`），绕开图路径对超大查询的
/// f32 溢出。调用方必须事先拒绝非有限分量；NaN 输入会继续传播 NaN。
/// EN: f32 accumulation overflows to inf for |x| ≳ 1.8e19 (or large high-dim sums),
/// which silently corrupts a valid direction into all zeros — persisted and replayed.
/// f64 accumulation cannot overflow for any f32 input. The band *constant* is shared
/// with the graph path but its f32 accumulation precision is intentionally left alone
/// (changing it perturbs built graphs and needs the recall gate); queries are normalized
/// via this f64 path before entering the graph, so huge-magnitude queries don't overflow
/// there either. Callers must reject non-finite components beforehand; NaN propagates.
fn normalize_cosine_in_place(v: &mut [f32]) {
    let norm2: f64 = v.iter().map(|&x| (x as f64) * (x as f64)).sum();
    if norm2 <= ZERO_NORM_BAND_NORM2 as f64 {
        return;
    }
    let inv = norm2.sqrt().recip();
    for x in v.iter_mut() {
        *x = (*x as f64 * inv) as f32;
    }
}

/// 查询专用的余弦归一化（分配新版本，绝不改写调用方的查询切片）：返回 L2 单位向量；
/// 零范数阈值带内（`norm ≲ 3.45e-4`）的向量原样保留（全 0 输入仍为全 0）。
/// EN: Query-only cosine normalization (allocates a new Vec, never mutates the
/// caller's query slice) — returns an L2-unit vector; vectors inside the zero-norm
/// band (norm ≲ 3.45e-4) are kept unchanged (an all-zero input stays all-zero).
///
/// WHY（为什么查询也要归一化，且为什么单独留一个分配版本）：L0 有两条检索路径——
/// ≥64 行走 [`HnswGraph`]（存储向量在 insert 时内部归一化），<64 行走 memtable 暴力
/// 扫描，存储侧写入即归一化（见 [`normalize_cosine_in_place`]），两条路径都按预归一化
/// 点积计分，查询因此必须在分叉前统一单位化。查询不被存储，不能原地改写入参，故保留
/// 这个分配版本；写入/重放走 [`normalize_cosine_in_place`] 原地完成。单位化幂等：进图
/// 后图内部的 f32 再归一化对单位查询是 no-op。零查询保持全 0，则 dot(0,v)=0 → 余弦
/// 距离 1，与 [`crate::distance::cosine`] 对零向量返回 1.0 的约定一致。
/// EN: Stored vectors are normalized at insert on both paths, so the query must be
/// normalized once before the brute/graph fork. The query slice is borrowed, not
/// stored, hence this allocating variant; the write/replay path normalizes in place.
/// Re-normalization inside the graph is idempotent on a unit query.
fn normalize_for_cosine_query(v: &[f32]) -> Vec<f32> {
    let mut out = v.to_vec();
    normalize_cosine_in_place(&mut out);
    out
}

// ── TODO: L1 / L2 占位 — 等有 GPU/NVMe 硬件再实现 ─────────────
//
// L1 (GPU 主力查询层):
//   依赖: CUDA toolkit, CAGRA graph (NVIDIA RAFT)
//   接口: build_from_l0(vectors) → GPU CAGRA graph
//         search(query, k) → GPU-accelerated top-k
//   预计代码量: ~800 行
//
// L2 (NVMe 磁盘持久层):
//   依赖: DiskANN / Vamana algorithm, io_uring
//   接口: build_from_l1(vectors) → DiskANN graph on NVMe
//         search(query, k) → beam-search with mmap
//   预计代码量: ~1200 行
// ────────────────────────────────────────────────────────────────

/// HII L0 配置。
/// EN: HII L0 configuration.
#[derive(Debug, Clone)]
pub struct HiiL0Config {
    /// 维度。
    /// EN: Dimension.
    pub dims: usize,
    /// L0 小图的 M 参数（默认 4，保持高频写入下的构建速度）。
    /// EN: M for the L0 small graph (default 4, keeping build speed under heavy writes).
    pub m: usize,
    /// L0 构建时的 ef_construction。
    /// EN: ef_construction for L0 builds.
    pub ef_construction: usize,
    /// 合并阈值：memtable 达到此数量后触发合并到 L1。
    /// EN: Merge threshold: merge into L1 once the memtable reaches this count.
    pub merge_threshold: usize,
}

impl Default for HiiL0Config {
    fn default() -> Self {
        Self {
            dims: 128,
            m: 4,
            ef_construction: 32,
            merge_threshold: 100_000,
        }
    }
}

/// HII L0 内存增量层。
/// EN: HII L0 in-memory incremental layer.
///
/// 双结构：
/// EN: Dual structure:
/// - `memtable: BTreeMap` — 有序 ID 查找 + 范围扫描
///   EN: - `memtable: BTreeMap` — ordered ID lookup + range scans
/// - `graph: HnswGraph(M=4)` — 小图 ANN 检索
///   EN: - `graph: HnswGraph(M=4)` — small-graph ANN search
///
/// 写入直接落入 L0，立即可检索（毫秒级可见）。
/// EN: Writes land directly in L0 and are immediately searchable (millisecond visibility).
pub struct HiiL0 {
    dims: usize,
    memtable: BTreeMap<VectorId, Vec<f32>>,
    graph: Option<HnswGraph>,
    config: HiiL0Config,
    /// 已合并到 L1 的向量数（TODO: L1 实现后启用）。
    /// EN: Vectors merged into L1 (TODO: enabled after L1 lands).
    #[allow(dead_code)]
    merged_count: usize,
    /// 图是否已构建（首次 search 或手动触发）。
    /// EN: Whether the graph is built (on first search or manual trigger).
    built: bool,
}

impl HiiL0 {
    pub fn new(config: HiiL0Config) -> Self {
        Self {
            dims: config.dims,
            memtable: BTreeMap::new(),
            graph: None,
            config,
            merged_count: 0,
            built: false,
        }
    }

    /// 写入向量——直接落入 L0 memtable + 标记图脏。
    /// 毫秒级可见：写入后立即可被 search 查询到。
    /// EN: Writes a vector — lands directly in the L0 memtable + marks the graph dirty.
    /// EN: Millisecond visibility: searchable immediately after the write.
    pub fn insert(&mut self, id: VectorId, mut vector: Vec<f32>) -> Result<(), String> {
        if vector.len() != self.dims {
            return Err(format!(
                "HII L0: dim mismatch expect {} got {}",
                self.dims,
                vector.len()
            ));
        }
        // 边界拒绝非有限分量：NaN 会让 norm2/点积整条链路产出 NaN（钳制对 NaN 无效），
        // 毒化行在暴力排序里落点未定义且距离恒 NaN；Inf 会使归一化溢出成全 0。
        // 与维度校验对称地在此 fail-fast（HTTP/pgwire 边界另做预校验返回 4xx）。
        // EN: Reject NaN/±Inf at the boundary — NaN poisons norm/dot products (clamp
        // cannot help) and makes ranking undefined; Inf overflows normalization to
        // zeros. Fail-fast symmetric with the dim check.
        if let Some(bad) = vector.iter().find(|x| !x.is_finite()) {
            return Err(format!(
                "HII L0: non-finite component in vector id={id}: {bad}"
            ));
        }
        // 余弦度量约定写入即（原地）归一化：暴力路径与图路径口径一致，snapshot 重放幂等。
        // EN: Cosine convention — normalize the owned vector in place on write so the
        // brute-force and graph paths share one contract and snapshot replay stays idempotent.
        normalize_cosine_in_place(&mut vector);
        self.memtable.insert(id, vector);
        self.built = false; // 图需要重建
        Ok(())
    }

    /// 批量写入。
    /// EN: Batch write.
    pub fn insert_batch(&mut self, batch: Vec<(VectorId, Vec<f32>)>) -> Result<(), String> {
        for (id, vec) in batch {
            self.insert(id, vec)?;
        }
        Ok(())
    }

    /// 检索——优先用小图，若图未构建则 fallback 到 memtable 扫描。
    /// Search — small graph first; falls back to a memtable scan until the graph
    /// is lazily built.
    pub fn search(&mut self, query: &[f32], k: usize) -> Vec<VectorId> {
        // 排序/截断只在 scored 变体实现一份，id-only 仅剥离距离。
        // EN: The scored variant owns the only sort/truncate logic; ids-only strips it.
        self.search_scored(query, k)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// scored 检索：返回 `(向量 id, 余弦距离)`，按距离升序（越小越近），至多 `k` 条。
    /// EN: Scored search — `(vector id, cosine distance)` pairs sorted by distance
    /// ascending (smaller = closer), at most `k` pairs. Vectors are L2-normalized at
    /// insert (cosine convention), so both paths share identical semantics: identical
    /// directions yield distance 0 regardless of stored magnitude. The query is
    /// L2-normalized once here (f64 accumulation) before either path; on the graph path
    /// [`HnswGraph`] re-normalizes the already-unit query (idempotent). A non-finite
    /// query (NaN/±Inf) or a wrong-dimension query yields an empty result — distances
    /// would all be NaN/undefined (the brute dot product silently truncates to the
    /// shorter length while the graph path returns empty, a 64-row semantic jump);
    /// the HTTP/pgwire edge is expected to reject both with 4xx first, and this is the
    /// in-library last line of defense, symmetric with `insert`'s validation.
    pub fn search_scored(&mut self, query: &[f32], k: usize) -> Vec<(VectorId, f32)> {
        if self.memtable.is_empty() {
            return Vec::new();
        }
        if k == 0 {
            return Vec::new();
        }
        // 库层防御 1：维度不符直接返回空集。WHY：图路径对错误维度本就返回空，而暴力
        // 路径的点积按 min(长度) 截断前缀计算，会静默给出错误分数——同一条坏查询在
        // 64 行构图边界两侧语义跳变，与 insert 的维度校验对称地在此 fail-closed。
        // EN: Defense 1 — wrong-dim queries return empty. The graph path already
        // returns empty on dim mismatch, but the brute dot product silently computes
        // over a truncated prefix, which would be a 64-row boundary semantic jump;
        // fail closed, symmetric with `insert`'s dim validation.
        if query.len() != self.dims {
            tracing::warn!(
                expect_dims = self.dims,
                got_dims = query.len(),
                "[hii-l0] query dim mismatch; returning empty result set"
            );
            return Vec::new();
        }
        // 库层防御 2：非有限查询的距离恒 NaN、排序落点未定义（partial_cmp 退化为 Equal，
        // 结果等价随机），返回空比返回随机 k 个诚实。边界（HTTP/pgwire）应已 4xx 拦截。
        // EN: Defense 2 — NaN distances cannot be ranked (all comparisons collapse to
        // Equal), so return nothing rather than arbitrary k rows.
        if let Some(bad) = query.iter().find(|x| !x.is_finite()) {
            tracing::warn!(component = %bad, "[hii-l0] non-finite query component; returning empty result set");
            return Vec::new();
        }

        // 懒构建小图（首次搜索时触发）。
        // EN: Lazily build the small graph on first search.
        if !self.built && self.memtable.len() >= 64 {
            self.build_graph();
        }

        // 查询在分叉前只归一化一次（f64 累加）：两条路径同一口径；且进图的已是单位
        // 向量，图内部的 f32 归一化对它幂等（norm²≈dims 不溢出），从而绕开图路径对
        // 超大模长查询（|x| ≳ 1.8e19 时 f32 平方和溢出 → inf → 零查询）的腐蚀。
        // EN: Normalize once (f64) before the fork: one contract for both paths, and
        // the graph's internal f32 re-normalization is idempotent on a unit query
        // (norm²≈dims can't overflow), dodging the graph's huge-query overflow.
        let query = normalize_for_cosine_query(query);

        if let Some(ref graph) = self.graph {
            // ANN 过取 2k 作为召回缓冲，但对外 k 契约是「至多 k 条」：
            // graph.search 已按距离升序，直接截断前 k 即最近的 k 条。
            // EN: Over-fetch 2k for ANN recall, but the public contract is at most k;
            // graph.search is already distance-ascending so a plain truncate keeps the k closest.
            //
            // 必须用饱和乘法：裸 `k * 2` 在 debug build 下 k > usize::MAX/2 即整数溢出
            // panic（HTTP 边缘已把 k 封在 10_000，但库可被直接调用，不把安全性建立在
            // 调用方守规矩之上）；随后 `.min(memtable.len())` 立刻把过取量夹回数据规模。
            // EN: Must saturate — bare `k * 2` panics on overflow in debug builds. The
            // HTTP edge caps k, but the library is directly callable; `.min(len)` then
            // clamps the over-fetch to the actual data size.
            let k_search = k.saturating_mul(2).min(self.memtable.len());
            let mut hits = graph.search(&query, k_search);
            hits.truncate(k);
            hits
        } else {
            // 数据量太少，直接 memtable 扫描。
            // EN: Few rows: brute-force memtable scan.
            Self::brute_search_scored(&self.memtable, &query, k)
        }
    }

    /// 暴力扫描 memtable（数据量 < 64 时的 fallback），返回带余弦距离的有序结果。
    /// Brute-force memtable scan (<64 rows fallback); sorted scored pairs.
    ///
    /// 契约：存储向量与查询都必须已在写入/`search_scored` 入口完成 L2 单位化
    /// （零范数阈值带内保持全 0），这里直接走预归一化余弦点积，不再二次归一化。
    /// EN: Contract — both stored vectors and the query are already L2-normalized
    /// (zero-norm band stays all-zero); this uses the pre-normalized dot product
    /// directly and does not re-normalize.
    fn brute_search_scored(
        memtable: &BTreeMap<VectorId, Vec<f32>>,
        query: &[f32],
        k: usize,
    ) -> Vec<(VectorId, f32)> {
        let mut scored: Vec<(VectorId, f32)> = memtable
            .iter()
            .map(|(id, vec)| {
                let dist = cosine_distance_prenormalized(query, vec);
                (*id, dist)
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    /// 用 memtable 数据构建小 HNSW 图。
    /// EN: Builds the small HNSW graph from memtable data.
    fn build_graph(&mut self) {
        if self.memtable.is_empty() {
            return;
        }
        let mut graph = HnswGraph::new(
            self.config.dims,
            crate::DistanceMetric::Cosine,
            self.config.m,
            self.config.ef_construction,
            16, // ef_search (小图查询宽度)
            42, // seed
            1,  // max_layers_start (L0 小图保持单层)
        );
        for (&id, vec) in self.memtable.iter() {
            graph.insert(id, vec.clone());
        }
        self.graph = Some(graph);
        self.built = true;
    }

    /// 重建图（bulk insert 后调用来优化检索）。
    /// EN: Rebuilds the graph (call after bulk inserts to optimize search).
    pub fn rebuild_graph(&mut self) {
        self.graph = None;
        self.built = false;
        self.build_graph();
    }

    /// memtable 中的向量总数。
    /// EN: Total vectors in the memtable.
    pub fn size(&self) -> usize {
        self.memtable.len()
    }

    /// 是否为空。
    /// EN: Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.memtable.is_empty()
    }

    /// 是否需要合并到 L1。
    /// EN: Whether a merge into L1 is due.
    pub fn should_merge(&self) -> bool {
        self.memtable.len() >= self.config.merge_threshold
    }

    /// 导出当前 memtable 中的所有向量（供 L1 合并使用，TODO）。
    /// EN: Exports all vectors in the memtable (for L1 merge; TODO).
    pub fn export_vectors(&self) -> Vec<(VectorId, Vec<f32>)> {
        self.memtable
            .iter()
            .map(|(id, v)| (*id, v.clone()))
            .collect()
    }

    // ── TODO: merge_to_l1() — 等 L1 实现后启用 ─────────────────
    //
    // 将 L0 memtable 中的数据合并到 GPU L1 层：
    //   1. 调用 self.export_vectors() 导出数据
    //   2. 发送到 L1 (GPU CAGRA graph) 进行批量建图
    //   3. 清空 memtable 中已合并的条目
    //   4. 重置图标记，等待下次懒构建
    //
    // pub fn merge_to_l1(&mut self, l1: &mut HiiL1) -> usize {
    //     todo!("L1 GPU layer not implemented yet")
    // }
}

// ── TODO: HiiL1 / HiiL2 — 等有 GPU/CUDA 硬件再实现 ────────────
//
// pub struct HiiL1 { /* GPU CAGRA graph */ }
// pub struct HiiL2 { /* NVMe DiskANN graph */ }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hii_l0_insert_and_brute_search() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);

        // 写入 3 条（不足 64，走 brute force）
        // EN: Insert 3 rows (fewer than 64, so brute force).
        l0.insert(1, vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        l0.insert(2, vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        l0.insert(3, vec![0.0, 0.0, 1.0, 0.0]).unwrap();

        let results = l0.search(&[1.0, 0.0, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], 1); // 最近的是 id=1
    }

    /// TDD：scored 暴力检索（<64 条走 memtable 扫描）返回 (id, 余弦距离)：
    /// 距离升序、id 序列与仅返回 id 的 `search` 一致、精确匹配距离 ≈ 0。
    /// EN: Scored brute-force search (<64 rows) returns ascending cosine distances,
    /// ids identical to the id-only variant, and ~0 for an exact match.
    #[test]
    fn hii_l0_scored_brute_search_contract() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        l0.insert(1, vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        l0.insert(2, vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        l0.insert(3, vec![0.0, 0.0, 1.0, 0.0]).unwrap();

        let q = vec![1.0, 0.0, 0.0, 0.0];
        let scored = l0.search_scored(&q, 2);
        assert_eq!(scored.len(), 2);
        assert_eq!(scored[0].0, 1);
        assert!(scored[0].1.abs() < 1e-6, "exact match cosine distance ~0");
        assert!(scored[0].1 <= scored[1].1, "distances must be ascending");
        let ids_only: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids_only, l0.search(&q, 2), "scored/id-only 链路必须一致");
    }

    /// 图路径（≥64 条触发懒构图）下 scored 与 id-only 的 id 序列必须一致，距离升序，
    /// 且结果数恰好为 k（过取 2k 后已 truncate 到 k 契约）。
    /// EN: On the graph path, scored ids match the id-only variant, distances are
    /// ascending, and exactly k pairs are returned (2k over-fetch truncated to k).
    #[test]
    fn hii_l0_scored_graph_search_matches_id_only() {
        let config = HiiL0Config {
            dims: 8,
            m: 4,
            ef_construction: 32,
            merge_threshold: 1000,
        };
        let mut l0 = HiiL0::new(config);
        for i in 0..100 {
            let mut v = vec![0.0f32; 8];
            v[i % 8] = 1.0;
            l0.insert(i as u64, v).unwrap();
        }
        let query: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let scored = l0.search_scored(&query, 5);
        assert_eq!(scored.len(), 5, "graph path must honor the k contract");
        let ids_only: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids_only, l0.search(&query, 5));
        for pair in scored.windows(2) {
            assert!(pair[0].1 <= pair[1].1, "distances must be ascending");
        }
    }

    #[test]
    fn hii_l0_graph_search_basic() {
        let config = HiiL0Config {
            dims: 8,
            m: 4,
            ef_construction: 32,
            merge_threshold: 1000,
        };
        let mut l0 = HiiL0::new(config);

        // 写入 64+ 条触发图构建
        // EN: Insert 64+ rows to trigger graph construction.
        for i in 0..100 {
            let mut v = vec![0.0f32; 8];
            v[i % 8] = 1.0;
            l0.insert(i as u64, v).unwrap();
        }

        // 搜索应走图路径
        // EN: Search should take the graph path.
        let query: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let results = l0.search(&query, 5);
        assert_eq!(results.len(), 5);
    }

    /// 库层纵深防御：图路径的 2k 过取必须用饱和乘法——debug build 下裸 `k * 2` 在
    /// k > usize::MAX/2 时整数溢出直接 panic。HTTP 边缘已把 k 限制在 10_000，但库可被
    /// 直接调用，不能把「不 panic」建立在调用方守规矩之上。
    /// EN: Library-level defense in depth: the 2k over-fetch must saturate — a bare
    /// `k * 2` panics on overflow in debug builds. The HTTP edge caps k at 10,000, but
    /// direct library callers must never be able to trigger an arithmetic panic.
    #[test]
    fn hii_l0_huge_k_graph_path_does_not_overflow() {
        let config = HiiL0Config {
            dims: 8,
            m: 4,
            ef_construction: 32,
            merge_threshold: 1000,
        };
        let mut l0 = HiiL0::new(config);
        for i in 0..100 {
            let mut v = vec![0.0f32; 8];
            v[i % 8] = 1.0;
            l0.insert(i as u64, v).unwrap();
        }
        let query: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let scored = l0.search_scored(&query, usize::MAX);
        assert!(!scored.is_empty(), "huge k must still return hits");
        assert!(scored.len() <= 100, "result count bounded by memtable size");
    }

    #[test]
    fn hii_l0_rebuild_graph() {
        let config = HiiL0Config {
            dims: 4,
            m: 4,
            ef_construction: 32,
            merge_threshold: 1000,
        };
        let mut l0 = HiiL0::new(config);

        // 写入足够多向量，确保图能被构建
        // EN: Insert enough vectors to make sure the graph can be built.
        for i in 0..80 {
            let mut v = vec![0.0f32; 4];
            v[i % 4] = 1.0;
            l0.insert(i as u64, v).unwrap();
        }

        // 首次 search 触发懒构建
        // EN: The first search triggers lazy construction.
        let r1 = l0.search(&[1.0, 0.0, 0.0, 0.0], 3);
        assert!(!r1.is_empty(), "first search should return results");
        assert!(l0.graph.is_some(), "graph should be built");

        // 重建图
        // EN: Rebuild the graph.
        l0.rebuild_graph();
        assert!(l0.graph.is_some(), "graph should exist after rebuild");

        let r2 = l0.search(&[1.0, 0.0, 0.0, 0.0], 3);
        assert!(!r2.is_empty(), "search after rebuild should return results");
    }

    #[test]
    fn hii_l0_dimension_mismatch() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        assert!(l0.insert(1, vec![1.0, 2.0]).is_err());
        assert!(l0.insert(1, vec![1.0, 2.0, 3.0, 4.0]).is_ok());
    }

    #[test]
    fn hii_l0_should_merge_threshold() {
        let config = HiiL0Config {
            dims: 4,
            merge_threshold: 50,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        assert!(!l0.should_merge());

        for i in 0..50 {
            l0.insert(i, vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        }
        assert!(l0.should_merge());
    }

    /// FIX-2 TDD：非单位向量自查询（暴力路径，<64 行）+ clamp 饱和排名回归。
    ///
    /// 重要更正（OCR 第四轮）：`cosine_distance_prenormalized` 内部对裸点积做了
    /// `dot.clamp(-1,1)`（distance.rs，FIX-2 之前即如此），「非单位自查询 score 越界
    /// 变成 14」在本层根本不可达——旧实现在原夹具上同样通过，那段 [-1,1] 边界循环是
    /// 空断言。旧实现可观察的真实缺陷是**排名饱和**：大模长近正交向量的裸点积被 clamp
    /// 成距离 0，与精确同向行并列（稳定排序 + 更小 id 时直接夺冠）。本测试改用 dim-4
    /// 小夹具在暴力路径锁死该缺陷；双路径（40 行暴力 / 80 行图）同款回归见
    /// `hii_l0_same_direction_ranks_first_below_and_above_graph_boundary`。
    /// EN: Brute-path self-query plus a clamp-saturation ranking regression.
    /// Correction (OCR round 4): the prenormalized distance already clamps the dot
    /// (pre-FIX-2), so "score 14" was never reachable and the old bound loop was
    /// vacuous. The real old defect was ranking saturation — a large near-orthogonal
    /// row's raw dot clamped to distance 0, tying (and, via stable id order, winning).
    #[test]
    fn hii_l0_non_unit_self_query_scores_one_and_saturating_distractor_ranks_last() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        let target = vec![3.0_f32, 0.0, -1.0, 2.0];
        let target_hat = normalize_for_cosine_query(&target);
        // 近正交单位方向 d：cos(target, d) 恰为 0.1（e_y 与 target 严格正交）。
        // Near-orthogonal unit direction: cos(target, d) = 0.1 (e_y ⟂ target).
        let mut near_ortho = [0.0_f32; 4];
        for (d, t) in near_ortho.iter_mut().zip(target_hat.iter()) {
            *d = 0.1 * t;
        }
        near_ortho[1] += 0.99_f32.sqrt();
        // 放大 20 倍：裸点积 target·(20d) = 20·0.1·√14 ≈ 7.48 > 1，旧实现被 clamp
        // 饱和成距离 0；写入归一化后距离应保持 0.9。
        // Scaled ×20: raw dot ≈ 7.48 (>1, saturates under the old raw-dot path);
        // with write-time normalization the distance stays 0.9.
        let distractor: Vec<f32> = near_ortho.iter().map(|x| 20.0 * x).collect();

        // 干扰行给最小 id：旧实现下距离并列时稳定排序让它卡在第一名。
        // The distractor takes the smallest id on purpose: stable sort crowns it when
        // the old raw-dot path ties every direction at distance 0.
        l0.insert(1, distractor).unwrap();
        l0.insert(2, target.clone()).unwrap();
        // 同方向、模长 2×：余弦只认方向，距离同样为 0。
        // Same direction, 2× magnitude: cosine ignores magnitude, distance still 0.
        l0.insert(3, target.iter().map(|x| x * 2.0).collect())
            .unwrap();
        l0.insert(4, vec![0.0, 1.0, 0.0, 0.0]).unwrap();

        let scored = l0.search_scored(&target, 4);
        assert_eq!(scored.len(), 4);
        let ids: Vec<u64> = scored.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![2, 3, 1, 4],
            "exact self → same-direction 2× → near-orthogonal 0.9 → orthogonal 1"
        );
        assert!(
            scored[0].1.abs() < 1e-5,
            "exact self distance ~0 (score ~1)"
        );
        assert!(scored[1].1.abs() < 1e-5, "same-direction distance ~0");
        assert!(
            (scored[2].1 - 0.9).abs() < 1e-5,
            "large near-orthogonal row must not clamp-saturate, got {}",
            scored[2].1
        );
        assert!((scored[3].1 - 1.0).abs() < 1e-5, "orthogonal distance ~1");
    }

    /// FIX-2：零范数向量防护——存全 0、不 NaN；与 `distance::cosine` 对零向量
    /// 返回距离 1 的约定一致（零查询 / 零存储两侧都成立）。
    /// EN: Zero-norm vectors are stored as zeros (no NaN); distance is 1 by the
    /// cosine convention, on both the zero-stored and zero-query sides.
    #[test]
    fn hii_l0_zero_vector_guard_distance_one() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        l0.insert(1, vec![0.0; 4]).unwrap();
        l0.insert(2, vec![1.0, 0.0, 0.0, 0.0]).unwrap();

        let q = vec![1.0, 0.0, 0.0, 0.0];
        let scored = l0.search_scored(&q, 2);
        assert_eq!(scored[0].0, 2);
        assert!(scored[0].1.abs() < 1e-6);
        let zero_hit = scored.iter().find(|(id, _)| *id == 1).unwrap();
        assert_eq!(zero_hit.1, 1.0, "zero stored vector → cosine distance 1");

        // 零查询：所有距离退化为 1。必须覆盖 64 行构图边界两侧（<64 暴力路径、
        // ≥64 图路径），否则图内部 normalize_for_cosine 的零带回归（零向量被归成
        // NaN 距离）无法被捕获。
        // EN: A zero query yields distance 1 for every row; cover both sides of the
        // 64-row graph boundary so a zero-band regression in the graph's own
        // normalization (zero → NaN distance) stays caught.
        for rows in [2_usize, 80] {
            let mut l0z = HiiL0::new(HiiL0Config {
                dims: 4,
                ..Default::default()
            });
            for i in 0..rows {
                let mut v = vec![0.0_f32; 4];
                v[i % 4] = 1.0;
                l0z.insert(i as u64, v).unwrap();
            }
            let zero_q = l0z.search_scored(&[0.0; 4], 2);
            assert_eq!(zero_q.len(), 2, "rows={rows}");
            for (_, d) in zero_q {
                assert_eq!(d, 1.0, "rows={rows}: zero query → distance 1 on every row");
            }
        }
    }

    /// OCR 回归（查询侧维度契约）：维度不符的查询在暴力与图两条路径上都必须返回空集——
    /// 图路径本就对维度不符返回空，但暴力点积按 min(长度) 截断前缀会静默误算，造成
    /// 64 行边界两侧语义跳变。
    /// EN: A wrong-dim query returns empty on BOTH paths: the graph path already does,
    /// but the brute dot product silently computed over a truncated prefix, a 64-row
    /// boundary semantic jump.
    #[test]
    fn hii_l0_wrong_dim_query_returns_empty_on_both_paths() {
        for rows in [3_usize, 80] {
            let mut l0 = HiiL0::new(HiiL0Config {
                dims: 4,
                ..Default::default()
            });
            for i in 0..rows {
                let mut v = vec![0.0_f32; 4];
                v[i % 4] = 1.0;
                l0.insert(i as u64, v).unwrap();
            }
            for bad in [vec![1.0, 0.0, 0.0], vec![1.0, 0.0, 0.0, 0.0, 0.0]] {
                let hits = l0.search_scored(&bad, 2);
                assert!(
                    hits.is_empty(),
                    "rows={rows} bad_dims={}: must be empty",
                    bad.len()
                );
            }
            // 合法查询在维度拦截之后仍正常（early-out 不污染图状态）。
            // A valid query still works after dim rejections (state untouched).
            let ok = l0.search_scored(&[1.0, 0.0, 0.0, 0.0], 1);
            assert_eq!(ok.len(), 1, "rows={rows}: valid query still works");
            assert!(ok[0].1.abs() < 1e-4);
        }
    }

    /// FIX-2 单元层：查询归一化辅助函数的不动点——对已单位向量再次归一化结果不变，
    /// 零向量保持全 0。本测试只覆盖 `normalize_for_cosine_query` 自身；snapshot/WAL
    /// 重放真正经过的入口是 `HiiL0::insert`（内部调用原地版 `normalize_cosine_in_place`），
    /// 入口级不动点由 `hii_l0_insert_replay_of_normalized_vector_is_fixed_point` 覆盖。
    /// EN: Unit-level fixed point of the query-normalization helper only. The actual
    /// snapshot/WAL replay entry point `HiiL0::insert` (which calls the in-place
    /// normalizer) is covered by
    /// `hii_l0_insert_replay_of_normalized_vector_is_fixed_point`.
    #[test]
    fn normalize_for_cosine_query_is_idempotent() {
        let v = vec![3.0_f32, 0.0, -1.0, 2.0];
        let once = normalize_for_cosine_query(&v);
        let twice = normalize_for_cosine_query(&once);
        for (a, b) in once.iter().zip(twice.iter()) {
            assert!((a - b).abs() < 1e-6, "re-normalize must be idempotent");
        }
        let norm: f32 = once.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
        let zero = normalize_for_cosine_query(&[0.0; 4]);
        assert!(zero.iter().all(|x| *x == 0.0), "zero vector kept as zero");
    }

    /// FIX-2/OCR 第四轮：重放入口级不动点——snapshot/WAL 里的向量已是单位向量，冷启动
    /// 重放再走一次真实入口 `HiiL0::insert`（内部 `normalize_cosine_in_place`）必须保持
    /// 数值不变；零范数行仍保持全 0。若未来改动 insert 的归一化调用或其与校验的顺序，
    /// 本测试与辅助函数单测同时报警。
    /// EN: Replay entry-point fixed point: an already-normalized snapshot/WAL row
    /// re-inserted through the real `HiiL0::insert` stays bit-stable within tolerance,
    /// and a zero row stays zero. Catches future changes to insert's normalization call
    /// or its ordering relative to validation.
    #[test]
    fn hii_l0_insert_replay_of_normalized_vector_is_fixed_point() {
        let mut l0 = HiiL0::new(HiiL0Config {
            dims: 4,
            ..Default::default()
        });
        let unit = normalize_for_cosine_query(&[3.0_f32, 0.0, -1.0, 2.0]);
        // 模拟冷启动重放：磁盘载入的行已经归一化，再经 insert 写入必须幂等。
        // Simulate cold-start replay: the on-disk row is already normalized.
        l0.insert(1, unit.clone()).unwrap();
        let stored = l0
            .export_vectors()
            .into_iter()
            .find(|(id, _)| *id == 1)
            .unwrap()
            .1;
        for (a, b) in unit.iter().zip(stored.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "replay re-normalize must be a fixed point"
            );
        }
        let norm: f64 = stored.iter().map(|x| *x as f64 * *x as f64).sum::<f64>();
        assert!(
            (norm.sqrt() - 1.0).abs() < 1e-6,
            "stored replay row stays unit length"
        );
        // 零范数行：insert 不做单位化，导出仍为全 0（重放幂等 + 零向量约定）。
        // Zero-norm row is never unitized at insert; replay keeps it all zeros.
        l0.insert(2, vec![0.0; 4]).unwrap();
        let zero = l0
            .export_vectors()
            .into_iter()
            .find(|(id, _)| *id == 2)
            .unwrap()
            .1;
        assert!(
            zero.iter().all(|x| *x == 0.0),
            "zero row stays all-zero through replay"
        );
    }

    /// OCR 回归：f32 累加平方和在大分量下溢出成 inf → inv=0 → 合法方向被腐蚀成全 0。
    /// f64 累加后，任意有限 f32 输入都必须得到有限的单位向量；f32::MAX 同向边界值同理。
    /// EN: Regression for f32 norm² overflow (inf → zero vector): every finite f32
    /// input, including f32::MAX, must normalize to a finite unit vector under f64
    /// accumulation.
    #[test]
    fn hii_l0_huge_components_normalize_without_corruption() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        // 1e20 的平方 = 1e40，远超 f32::MAX（≈3.4e38）：f32 累加必溢出。
        // 1e20² = 1e40 ≫ f32::MAX (~3.4e38): f32 accumulation would overflow here.
        l0.insert(1, vec![1e20_f32, 0.0, 0.0, 0.0]).unwrap();
        l0.insert(2, vec![f32::MAX, 0.0, 0.0, 0.0]).unwrap();
        for (id, expected_first) in [(1_u64, 1.0_f32), (2, 1.0)] {
            let stored = l0
                .export_vectors()
                .into_iter()
                .find(|(i, _)| *i == id)
                .unwrap()
                .1;
            assert!(
                stored.iter().all(|x| x.is_finite()),
                "id={id} stored vector must stay finite, got {stored:?}"
            );
            let norm = stored.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "id={id} must be unit, norm={norm}"
            );
            assert!(
                (stored[0] - expected_first).abs() < 1e-5,
                "id={id} direction must survive normalization, got {stored:?}"
            );
        }
        // 同向的极大向量与单位向量方向一致：自查询距离必须 ≈ 0，不允许出现距离 1。
        // EN: The huge same-direction row is a self/cosine match (distance ≈ 0),
        // not a corrupted "orthogonal" zero row (distance 1).
        let hits = l0.search_scored(&[1.0, 0.0, 0.0, 0.0], 2);
        assert!(
            hits.iter().all(|(_, d)| d.abs() < 1e-4),
            "same-direction huge vectors must be distance 0, got {hits:?}"
        );
    }

    /// OCR 回归：NaN/±Inf 分量必须在 insert 边界被拒绝（与维度校验对称），
    /// 毒向量不得落盘进 memtable。
    /// EN: NaN/±Inf components are rejected at insert (symmetric with the dim
    /// check) and never enter the memtable.
    #[test]
    fn hii_l0_rejects_non_finite_components() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        for bad in [
            vec![1.0_f32, f32::NAN, 0.0, 0.0],
            vec![1.0, f32::INFINITY, 0.0, 0.0],
            vec![1.0, f32::NEG_INFINITY, 0.0, 0.0],
        ] {
            assert!(
                l0.insert(7, bad).is_err(),
                "non-finite vector must be rejected"
            );
        }
        assert_eq!(l0.size(), 0, "no poisoned row may be stored");
        // 同 id 的合法向量在拒绝后仍可正常写入（失败不留残痕）。
        // EN: A valid vector with the same id still inserts after rejection (no residue).
        l0.insert(7, vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        assert_eq!(l0.size(), 1);
    }

    /// OCR 回归（查询侧）：NaN/±Inf 查询在暴力与图两条路径上都必须返回空集——
    /// NaN 距离无法排序（partial_cmp 退化为 Equal，结果等价随机），返回空优于随机 k 条。
    /// 合法查询在毒查询之后仍正常，证明 early-out 不污染索引/图状态。
    /// EN: NaN/±Inf queries return empty on both paths; valid queries still work after.
    #[test]
    fn hii_l0_non_finite_query_returns_empty_on_both_paths() {
        for rows in [3_usize, 80] {
            let config = HiiL0Config {
                dims: 4,
                ..Default::default()
            };
            let mut l0 = HiiL0::new(config);
            for i in 0..rows {
                let mut v = vec![0.0_f32; 4];
                v[i % 4] = 1.0;
                l0.insert(i as u64, v).unwrap();
            }
            let bad_queries = [
                vec![f32::NAN, 0.0, 0.0, 0.0],
                vec![f32::INFINITY, 0.0, 0.0, 0.0],
                vec![f32::NEG_INFINITY, 0.0, 0.0, 0.0],
                vec![1.0, f32::NAN, 0.0, 0.0],
            ];
            for q in bad_queries {
                assert!(
                    l0.search_scored(&q, 3).is_empty(),
                    "rows={rows}: non-finite query {q:?} must return empty"
                );
            }
            // 触发/不触发图路径都验证合法查询不受影响。
            // EN: Verify valid queries work whether or not the graph path is triggered.
            let ok = l0.search_scored(&[1.0, 0.0, 0.0, 0.0], 1);
            assert_eq!(ok.len(), 1);
            assert_eq!(ok[0].1, 0.0, "valid query unaffected after poison queries");
        }
    }

    /// OCR 回归（超大查询）：|x| ≳ 1.8e19 的查询，图路径内部 f32 归一化会溢出成
    /// 零查询（所有距离退化为 1）。入口统一用 f64 预单位化后，暴力/图两条路径都必须
    /// 把同向超大查询识别为距离 0 的最近邻。
    /// EN: Huge-magnitude queries (|x| ≳ 1.8e19) must rank the same-direction row first
    /// on both paths; f64 pre-normalization at the entry dodges the graph path's f32
    /// norm² overflow (which would otherwise turn the query into a zero vector).
    #[test]
    fn hii_l0_huge_query_matches_direction_on_both_paths() {
        for (rows, above_boundary) in [(3_usize, false), (80_usize, true)] {
            let config = HiiL0Config {
                dims: 4,
                ..Default::default()
            };
            let mut l0 = HiiL0::new(config);
            for i in 0..rows {
                let mut v = vec![0.0_f32; 4];
                v[i % 4] = 1.0;
                l0.insert(i as u64, v).unwrap();
            }
            let q = vec![1e20_f32, 0.0, 0.0, 0.0];
            let hits = l0.search_scored(&q, 1);
            assert_eq!(hits.len(), 1, "rows={rows}");
            // e0 方向行有多个（id % 4 == 0，距离并列 0），断言方向而非具体 id。
            // Several e0 rows tie at distance 0 (id % 4 == 0); assert direction, not id.
            assert_eq!(
                hits[0].0 % 4,
                0,
                "graph={above_boundary}: huge query must hit an e0-direction row"
            );
            assert!(
                hits[0].1.abs() < 1e-4,
                "graph={above_boundary}: distance must be ~0, got {}",
                hits[0].1
            );
        }
    }

    /// 构造「小模长同向向量 vs 大模长近正交向量」的排名翻转场景：
    /// 裸点积（旧 bug）会按模长把大模长向量排第一；余弦只认方向，同向向量必须第一。
    /// 返回首命中 id。
    /// EN: Ranking-flip fixture: raw dot (old bug) favors the large-magnitude
    /// near-orthogonal vector; cosine puts the small same-direction vector first.
    fn boundary_ranking_first_id(filler_count: usize) -> (u64, Vec<(u64, f32)>) {
        let config = HiiL0Config {
            dims: 8,
            m: 4,
            ef_construction: 32,
            merge_threshold: 1000,
        };
        let mut l0 = HiiL0::new(config);
        // 干扰向量：在 dims 2..7 上有主分量，同时给一个朝 e0 递增的小梯度
        // （相似度 0.29→0.57，余弦距离 0.71→0.43）——既严格差于 id=901 的
        // 0.106（暴力路径第二名断言成立），又让图路径上的贪心光束有梯度可循，
        // 不至于在正交高原上提前终止而召回不到精确同向节点。
        // EN: Distractor vector: dominant components in dims 2..7 plus a small gradient toward e0
        // EN: (similarity 0.29→0.57, cosine distance 0.71→0.43) — strictly worse than id=901's
        // EN: 0.106 (so the brute-force runner-up assertion holds), yet giving the greedy beam a
        // EN: gradient so it does not stop early on an orthogonal plateau and miss the exact-direction node.
        for i in 0..filler_count {
            let mut f = vec![0.0f32; 8];
            let j = 2 + i % 6;
            f[0] = 0.3 + 0.005 * i as f32;
            f[j] = 1.0;
            l0.insert(i as u64, f).unwrap();
        }
        // id=900: 与 e0 严格同向但模长仅 0.2（余弦距离 0；裸点积仅 0.2）。
        // EN: id=900: exactly aligned with e0 but norm only 0.2 (cosine distance 0; raw dot 0.2).
        l0.insert(900, vec![0.2, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();
        // id=901: 大模长近正交（余弦距离 ≈ 0.106；裸点积 2.0，旧实现误排第一）。
        // EN: id=901: large norm, nearly orthogonal (cosine distance ≈ 0.106; raw dot 2.0 — old first).
        l0.insert(901, vec![2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();

        let q = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let scored = l0.search_scored(&q, 3);
        assert_eq!(scored.len(), 3, "k contract on both brute/graph paths");
        (scored[0].0, scored)
    }

    /// FIX-2：64 行构图边界两侧排序口径一致——同向向量在暴力路径（<64）与
    /// 图路径（≥64）都必须排第一；旧实现仅暴力路径错误（图路径内部本就归一化）。
    /// EN: Ranking must be identical on both sides of the 64-row graph boundary;
    /// the old brute path alone was affected (the graph always normalized).
    #[test]
    fn hii_l0_same_direction_ranks_first_below_and_above_graph_boundary() {
        // 42 行 < 64 → 暴力路径（旧实现首命中为 901，修复后为 900）。
        // EN: 42 rows < 64 → brute-force path (old first hit was 901; after the fix, 900).
        let (first_below, scored_below) = boundary_ranking_first_id(40);
        assert_eq!(
            first_below, 900,
            "brute path: same-direction must rank first"
        );
        assert_eq!(
            scored_below[1].0, 901,
            "brute path: large near-orthogonal is second"
        );

        // 82 行 ≥ 64 → 图路径（k 契约：恰好 3 条）。
        // EN: 82 rows ≥ 64 → graph path (k contract: exactly 3).
        let (first_above, _) = boundary_ranking_first_id(80);
        assert_eq!(
            first_above, 900,
            "graph path: same-direction must rank first"
        );
    }

    #[test]
    fn hii_l0_empty_search() {
        let config = HiiL0Config {
            dims: 4,
            ..Default::default()
        };
        let mut l0 = HiiL0::new(config);
        let results = l0.search(&[1.0, 0.0, 0.0, 0.0], 10);
        assert!(results.is_empty());
    }
}
