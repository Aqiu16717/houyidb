//! CollectionManager: 多 Collection 路由，每 Collection 独立维度/索引。
//! Multi-collection router: each collection has its own dims, metric, and IndexBackend.

use crate::index_backend::IndexBackend;
use houyi_core::{HouyiPage, ScalarPredicate, ScalarValue};
use houyi_vector::{BuilderWorker, DistanceMetric, FlushPolicy};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const DEFAULT_COLLECTION: &str = "default";

/// 每 Collection 标量页的建议容量（字节）。
/// Advisory capacity (bytes) for each collection's scalar PAX page.
const COLLECTION_SCALAR_PAGE_CAPACITY: usize = 64 * 1024 * 1024;

/// 每 Collection 的内存标量存储：PAX 页 + 行号→向量 id 映射。
/// Per-collection in-memory scalar store: PAX page + row_idx→vector id map.
///
/// **持久化**：label/payload 以原始 JSON 随 `collection_upsert_batch` 落 collections
/// WAL（WAL-first），冷启动重放经 `json_value_to_scalar` 逐行重建本页——写入侧
/// 与重放侧走同一派生函数，语义逐字节等价。删除以 `collection_delete` 记录落盘，
/// 重放时重建墓碑。
/// EN: **Durability** — label/payload ride the `collection_upsert_batch` records as
/// original JSON (WAL-first); cold replay rebuilds this page row-by-row through the
/// same `json_value_to_scalar` derivation used on the write path. Deletes persist
/// as `collection_delete` records and replay as tombstones.
///
/// 追加写（append-only）：重复 upsert 同一 id 会追加新行；检索侧按 id 去重
/// （v1 已知限制，旧行不会被回收或覆盖；后续版本引入行版本/墓碑机制）。
/// EN: Append-only: re-upserting the same vector id appends a new row; the search
/// side dedupes by vector id. Known v1 limitation — stale rows are neither reclaimed
/// nor overwritten; a future version will add row versions/tombstones.
pub struct CollectionScalars {
    /// 字段私有：页行与 `row_to_id` 的 1:1 对齐是结构性不变量，
    /// 只能经由 [`append_row`](Self::append_row) 原子推进，禁止外部只改其一。
    /// EN: Private by design — page rows and `row_to_id` must advance together;
    /// callers go through `append_row` so the alignment cannot be broken by accident.
    page: HouyiPage,
    /// 第 i 项 = PAX 页第 i 行对应的向量 id；与页内行一一对应。
    /// Entry i = vector id backing page row i; aligned 1:1 with page rows.
    row_to_id: Vec<u64>,
    /// 已删除向量 id 的墓碑集：append-only 页无法物理删行，删除以墓碑表达——
    /// 过滤命中映射与 payload 回读都跳过墓碑 id；同 id 重新 upsert 时复活（清除）。
    /// EN: Tombstones of deleted vector ids. The append-only page cannot physically
    /// drop rows, so deletes are logical: id mapping and payload read-back both skip
    /// tombstoned ids; a re-upsert of the same id resurrects it (clears the mark).
    tombstones: HashSet<u64>,
}

impl CollectionScalars {
    fn new() -> Self {
        // 注意：capacity 仅为建议值——`HouyiPage::insert_row` 不做容量检查，
        // 标量列由普通 Vec 支撑，此处参数只影响对外暴露的 capacity()。
        // EN: capacity is advisory only — `insert_row` never checks it (scalar columns
        // are plain Vecs); the value only surfaces via `HouyiPage::capacity()`.
        Self {
            page: HouyiPage::new(COLLECTION_SCALAR_PAGE_CAPACITY),
            row_to_id: Vec::new(),
            tombstones: HashSet::new(),
        }
    }

    /// 原子追加一行标量并登记其向量 id（二者必须同时推进以保持 1:1 对齐）。
    /// Append one scalar row and register its backing vector id atomically.
    ///
    /// 空 map 不建行：裸向量（无 label/payload）不进标量页，否则会出现「有行无列」
    /// 的占位行并把后续 `row_to_id` 映射整体错位。调用方需先判断是否有标量可写。
    /// EN: An empty map appends nothing — bare vectors must not create placeholder
    /// rows that would shift every subsequent row→id mapping. Callers skip bare vectors.
    pub(crate) fn append_row(&mut self, id: u64, scalars: HashMap<String, ScalarValue>) {
        if scalars.is_empty() {
            return;
        }
        // 复活语义：携带新标量行的写入即撤销该 id 的墓碑（与向量侧 remove/insert
        // 的墓碑约定同口径；裸向量 upsert 不触碰标量墓碑，与「裸向量不建行」一致）。
        // EN: Resurrection — a write carrying fresh scalar rows clears the tombstone
        // (same contract as the vector-side tombstones; bare-vector upserts don't
        // touch scalar tombstones, consistent with "bare vectors add no row").
        self.tombstones.remove(&id);
        self.page.insert_row(scalars, None);
        self.row_to_id.push(id);
    }

    /// 登记删除墓碑：该 id 之后的过滤命中映射与 payload 回读一律跳过。
    /// EN: Record a delete tombstone — id mapping and payload read-back skip it.
    pub(crate) fn tombstone(&mut self, id: u64) {
        self.tombstones.insert(id);
    }

    /// 返回列上谓词命中的向量 id（行号→id 映射 + 按出现顺序去重）。
    /// Vector ids matched by `pred` on `column`: row→id mapping, order-preserving dedupe.
    ///
    /// append-only 重写会让多行指向同一 id（v1 无墓碑），故此处去重；顺序与页行顺序一致，
    /// 供多过滤条件交集使用。
    /// EN: Append-only re-upserts map several rows to one id (no tombstones in v1),
    /// hence the dedupe; order follows page rows and feeds the filter intersection.
    pub(crate) fn matching_ids(&self, column: &str, pred: &ScalarPredicate) -> Vec<u64> {
        let row_ids = self.page.filter_to_bitset(column, pred);
        let mut ids = Vec::with_capacity(row_ids.len());
        let mut seen = HashSet::with_capacity(row_ids.len());
        for row_idx in row_ids {
            if let Some(&vector_id) = self.row_to_id.get(row_idx as usize) {
                if self.tombstones.contains(&vector_id) {
                    continue;
                }
                if seen.insert(vector_id) {
                    ids.push(vector_id);
                }
            }
        }
        ids
    }

    /// 按向量 id 回读 payload（label + payload 键值），供检索响应携带文档内容。
    /// Read payloads back by vector id (label + payload keys) for search responses.
    ///
    /// append-only 语义：同一 id 多行时**最后一行生效**（与检索侧「重写以最新为准」
    /// 的直觉一致，也修正了 v1 文档里「过滤仍命中旧值」的已知限制中可回读的部分）；
    /// 墓碑 id 一律缺席。行扫描为 O(页行数)，仅在 `include_payload=true` 的检索尾部
    /// 调用（k 个 id 的查找合并成一次扫页），不在单点查询热路径。
    /// EN: Append-only semantics — the LAST row per id wins (matching the "latest
    /// re-upsert wins" intuition); tombstoned ids are always absent. The row scan is
    /// O(page rows), invoked only at the tail of `include_payload=true` searches
    /// (one merged scan for k ids), never on a point-lookup hot path.
    pub(crate) fn payloads_for(&self, ids: &[u64]) -> HashMap<u64, HashMap<String, ScalarValue>> {
        let want: HashSet<u64> = ids.iter().copied().collect();
        // 反向扫页：每个目标 id 只取最后一行。
        // EN: Reverse page scan — keep only the last row per wanted id.
        let mut out: HashMap<u64, HashMap<String, ScalarValue>> = HashMap::new();
        for row_idx in (0..self.row_to_id.len()).rev() {
            let id = self.row_to_id[row_idx];
            if !want.contains(&id) || self.tombstones.contains(&id) || out.contains_key(&id) {
                continue;
            }
            if let Some(view) = self.page.get_row(row_idx) {
                // 剔除 PAX 页缺失列的占位值（`HouyiPage::insert_row` 以
                // JsonText("null") 补齐列长）：占位不是用户数据，回读不得透出。
                // 已知取舍：用户真实写入的 JSON null 与该占位无法区分，一并隐藏
                // （页级表示限制，与 `json_value_to_scalar` 的 null→JsonText 约定相关）。
                // EN: Drop the page's missing-column padding (JsonText("null")) — it is
                // a layout artifact, not user data. Trade-off: a genuinely stored JSON
                // null is indistinguishable from padding and is hidden too.
                let scalars = view
                    .scalars
                    .into_iter()
                    .filter(|(_, v)| !matches!(v, ScalarValue::JsonText(s) if s == "null"))
                    .collect();
                out.insert(id, scalars);
            }
        }
        out
    }

    /// 已落标的量行数（= `row_to_id` 长度；裸向量不计）。
    /// Number of scalar rows (bare vectors excluded).
    pub(crate) fn row_count(&self) -> usize {
        self.row_to_id.len()
    }
}

/// Collection 内 HNSW 后端的异步构图参数（与 benchmark 主索引 worker 同源）。
/// Async-builder parameters for per-collection HNSW backends (same knobs as the
/// benchmark main index worker).
///
/// 为什么需要它：`insert_batch` 只进待构图队列，向量要经 worker flush 才可被检索。
/// 不带该配置创建的 Manager（单元测试）不启动 worker，队列保持静态、行为确定；
/// 生产装配（main）必须注入，否则 Collection 的 HNSW 写入永远不可检索（冒烟发现的
/// 生产缺口）。
/// EN: Why this exists — `insert_batch` only enqueues; vectors become searchable
/// solely after a worker flush. Managers built without it (unit tests) start no
/// worker and stay deterministic; production assembly (main) must inject it, or
/// collection HNSW writes are never searchable (a gap found by smoke testing).
#[derive(Debug, Clone, Copy)]
pub struct CollectionBuilderConfig {
    pub threshold: usize,
    pub tick_ms: u64,
    pub policy: FlushPolicy,
}

pub struct CollectionManager {
    collections: Mutex<HashMap<String, Arc<Collection>>>,
    default_dims: usize,
    default_index_type: String,
    builder_cfg: Option<CollectionBuilderConfig>,
    /// 新建 collection 的默认 HA 级别（P4.4.2，TOML `default_ha_level`，
    /// 缺省 async）。优先级：collection 级设置（WAL 记录）> 本默认值。
    /// P4.6.3 起为枚举——非法级别在类型层不可表达。
    /// EN: Default HA level for newly created collections; a collection-level
    /// setting (WAL record) always wins over this default (enum since P4.6.3).
    default_ha_level: houyi_persistence::ha_level::HaLevel,
}

pub struct Collection {
    // 元数据字段：当前路由/状态端点尚未全部透出，先保留供后续 status/可观测性使用。
    // Metadata retained for future status/observability surfaces; not all read today.
    #[allow(dead_code)]
    pub(crate) name: String,
    pub(crate) dims: usize,
    #[allow(dead_code)]
    pub(crate) metric: DistanceMetric,
    pub(crate) index_type: String,
    #[allow(dead_code)]
    pub(crate) created_at: u64,
    /// 索引后端：crate 外的二进制/集成测试会直接驱动（排空异步 builder）。
    /// Index backend: also driven directly by in-package binary tests (builder drain).
    pub backend: IndexBackend,
    /// 该 Collection 的内存标量页（label/payload；随 collections WAL 持久化，详见类型文档）。
    /// Per-collection in-memory scalar page (label/payload; persisted via collections WAL).
    /// 每 Collection 一把锁而非全局锁：不同 Collection 的写入/过滤互不阻塞。
    /// EN: per-collection lock (not a global one) so collections never contend.
    pub(crate) scalars: Mutex<CollectionScalars>,
    /// 写互斥锁：串行化「WAL append + 内存变更」这组复合写（upsert/delete 共用），
    /// 保证 WAL 顺序与内存顺序一致——否则并发下同 id 的 upsert/delete 会让重放
    /// 结果与已确认状态分叉（重放复活已删 id、或过期匹配集销毁已确认写入）。
    /// 用 tokio Mutex 而非 std：guard 需跨 `.await`（WAL append / backend.delete /
    /// insert_batch）持有，std MutexGuard 非 Send 无法用于 axum handler。
    /// 锁序约定：write_lock 先于 scalars 获取，任何路径不得反向。
    /// EN: Write mutex serializing the compound "WAL append + in-memory mutation"
    /// (shared by upsert/delete) so WAL order always matches memory order —
    /// otherwise concurrent same-id upsert/delete makes replay diverge from the
    /// acknowledged state (resurrecting a deleted id, or letting a stale match
    /// set destroy an acknowledged write). tokio Mutex because the guard must be
    /// held across `.await` (WAL append / backend ops). Lock order: write_lock
    /// is always taken before scalars, never the reverse.
    pub(crate) write_lock: tokio::sync::Mutex<()>,
    /// HA 级别（P4.4.2）：collection 粒度的复制持久性语义，热路径一次
    /// load(Relaxed) 零锁读取。内部以 `HaLevel::code()` 存储（std 无
    /// AtomicHaLevel）；对外一律经 [`Self::ha_level`] / [`Self::set_ha_level`]
    /// 的枚举接口——裸码不得越过本类型边界（P4.6.3 类型硬化）。
    /// 运行期经 `POST /v1/collections/{name}/ha_level`（WAL-first）与复制流
    /// apply 改写，重启经 collections.wal 重放恢复。
    /// EN: Stored as the enum's code (std has no AtomicHaLevel); the enum
    /// accessors are the only crossing points. Mutated WAL-first via the
    /// endpoint or the replication stream, restored by replay.
    ha_level: std::sync::atomic::AtomicU8,
    /// sync 等待超时告警的限流计数（P4.4.2）：挂在 collection 上，生命周期
    /// 随 collection 回收——无进程级映射表的无界增长问题（OCR 6bfb210[5]）。
    /// EN: Rate-limit counter for sync-wait timeout warns; dies with the
    /// collection, so no process-lifetime map can grow unboundedly.
    pub sync_timeout_warns: std::sync::atomic::AtomicU64,
    /// HNSW 后端的异步构图 worker 句柄：仅持有保活，进程退出随 runtime 回收。
    /// Builder worker keep-alive handle for the HNSW backend; reclaimed with the
    /// runtime at process exit.
    _builder_worker: Option<BuilderWorker>,
}

impl Collection {
    /// 读取 HA 级别（热路径一次 Relaxed load）。
    /// 存储码损坏在进程内不可达（所有写入点都经 [`Self::set_ha_level`]）；
    /// 真出现即属于内存腐坏，按不可恢复错误panic 并留痕，绝不静默落 async。
    /// EN: one Relaxed load on the hot path; an impossible bad code panics with
    /// a trace instead of silently degrading to async.
    pub fn ha_level(&self) -> houyi_persistence::ha_level::HaLevel {
        use houyi_persistence::ha_level::HaLevel;
        let code = self.ha_level.load(std::sync::atomic::Ordering::Relaxed);
        HaLevel::from_code(code).unwrap_or_else(|| {
            panic!("collection ha_level atomic holds unknown code {code} (memory corruption?)")
        })
    }

    /// 写 HA 级别（WAL-first 调用方在日志落盘成功后调用；复制 apply 同）。
    /// EN: set the level after the WAL record is durable (or via replication apply).
    pub fn set_ha_level(&self, level: houyi_persistence::ha_level::HaLevel) {
        self.ha_level
            .store(level.code(), std::sync::atomic::Ordering::Relaxed);
    }
}

impl CollectionManager {
    pub fn new(default_dims: usize, default_index_type: &str) -> Self {
        CollectionManager {
            collections: Mutex::new(HashMap::new()),
            default_dims,
            default_index_type: default_index_type.to_string(),
            builder_cfg: None,
            default_ha_level: houyi_persistence::ha_level::HaLevel::Async,
        }
    }

    /// 注入 Collection 异步构图配置（生产装配必须调用，详见 [`CollectionBuilderConfig`]）。
    /// Inject the per-collection builder config (mandatory in production assembly;
    /// see [`CollectionBuilderConfig`]).
    pub fn with_builder_config(mut self, cfg: CollectionBuilderConfig) -> Self {
        self.builder_cfg = Some(cfg);
        self
    }

    /// 注入默认 HA 级别（P4.4.2；TOML `default_ha_level` 装配入口）。
    /// 枚举参数自带合法性，无需运行时守卫（P4.6.3）。
    /// EN: Inject the default HA level; the enum carries validity (P4.6.3).
    pub fn with_default_ha_level(mut self, level: houyi_persistence::ha_level::HaLevel) -> Self {
        self.default_ha_level = level;
        self
    }

    /// 获取 Collection handle（无需持有锁）
    /// EN: Get a Collection handle (no lock held).
    pub fn get(&self, name: &str) -> Option<Arc<Collection>> {
        self.collections.lock().unwrap().get(name).cloned()
    }

    /// 获取或隐式创建 Collection。new_dims=None 时用默认维度。
    /// 仅 crate 内路由（HTTP/pgwire）调用。
    /// Crate-internal routing only (HTTP/pgwire handlers).
    pub(crate) fn get_or_create(
        &self,
        name: &str,
        new_dims: Option<usize>,
    ) -> Result<Arc<Collection>, String> {
        Ok(self.get_or_create_flagged(name, new_dims)?.0)
    }

    /// 同 [`get_or_create`](Self::get_or_create)，额外返回「本次是否新建」——
    /// collections WAL 只在新建时落 `collection_create` 记录（幂等重放的依据）。
    /// EN: Same as `get_or_create`, plus a "created this call" flag — the
    /// collections WAL writes `collection_create` only on actual creation
    /// (the basis for idempotent replay).
    pub(crate) fn get_or_create_flagged(
        &self,
        name: &str,
        new_dims: Option<usize>,
    ) -> Result<(Arc<Collection>, bool), String> {
        let mut guard = self.collections.lock().unwrap();
        if let Some(col) = guard.get(name) {
            if let Some(d) = new_dims {
                if col.dims != d {
                    return Err(format!(
                        "collection '{name}' has dims={}, cannot insert with dims={d}",
                        col.dims
                    ));
                }
            }
            return Ok((col.clone(), false));
        }

        let dims = new_dims.unwrap_or(self.default_dims);
        let backend = match self.default_index_type.as_str() {
            "clustered" => IndexBackend::new_clustered(dims),
            "flat-clustered" => IndexBackend::new_flat_clustered(dims),
            "flat" => IndexBackend::new_flat(dims),
            _ => IndexBackend::new_hnsw(dims),
        };

        // HNSW 后端需要异步构图 worker 才能把待构图队列 flush 成可检索状态；
        // 「哪个后端需要 worker」的分发规则集中在 IndexBackend 适配层，此处不重复
        // 编码（否则未来新后端需要 worker 时，这里的第二处分发会静默返回 None，
        // 重新引入写入不可检索的缺口）。
        // EN: The which-backend-needs-a-worker dispatch lives solely in the
        // IndexBackend adapter; re-encoding it here would silently return None for
        // any future worker-needing backend, reintroducing the unsearchable gap.
        let builder_worker = self.builder_cfg.and_then(|cfg| {
            backend.start_async_builder_worker(cfg.threshold, cfg.tick_ms, cfg.policy)
        });

        // 生产必须注入 builder 配置的契约没有任何静态保障：HNSW 后端 + 无配置
        // 意味着写入永不检索（本修复的缺口）。单测故意不配置，故只告警一次不
        // panic；告警走 tracing（生产由 main 安装订阅器），不污染正常路径。
        // EN: Nothing statically enforces "production must inject the config":
        // an HNSW backend without it means writes stay unsearchable forever (the
        // gap this fixes). Tests intentionally run unconfigured, so warn once via
        // tracing instead of panicking (subscriber installed in main).
        if self.builder_cfg.is_none() && matches!(backend, IndexBackend::Hnsw(_)) {
            static WARN_ONCE: std::sync::Once = std::sync::Once::new();
            WARN_ONCE.call_once(|| {
                tracing::warn!(
                    "[houyi-collection] HNSW backend created without builder config; \
                     writes stay unsearchable until manually drained (tests only — \
                     production must inject CollectionBuilderConfig)"
                );
            });
        }

        let col = Arc::new(Collection {
            name: name.to_string(),
            dims,
            metric: DistanceMetric::Cosine,
            index_type: self.default_index_type.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            backend,
            scalars: Mutex::new(CollectionScalars::new()),
            write_lock: tokio::sync::Mutex::new(()),
            ha_level: std::sync::atomic::AtomicU8::new(self.default_ha_level.code()),
            sync_timeout_warns: std::sync::atomic::AtomicU64::new(0),
            _builder_worker: builder_worker,
        });

        guard.insert(name.to_string(), col.clone());
        Ok((col, true))
    }

    /// 列出所有 Collection 名称（crate 内列表端点使用）。
    /// EN: List all collection names (used by the crate-internal list endpoint).
    pub(crate) fn list(&self) -> Vec<String> {
        self.collections.lock().unwrap().keys().cloned().collect()
    }

    /// Collection 数量（当前仅 crate 内使用）。
    /// EN: Number of collections (currently crate-internal only).
    #[allow(dead_code)]
    pub(crate) fn count(&self) -> usize {
        self.collections.lock().unwrap().len()
    }

    /// 删除 Collection（当前仅 crate 内使用）。
    /// EN: Drop a collection (currently crate-internal only).
    #[allow(dead_code)]
    pub(crate) fn drop(&self, name: &str) -> bool {
        self.collections.lock().unwrap().remove(name).is_some()
    }
}
