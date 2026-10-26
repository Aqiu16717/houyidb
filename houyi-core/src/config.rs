//! 统一配置系统：集中管理所有 `HOUYI_*` 环境变量，在进程启动时解析一次。
//! Centralized configuration: all `HOUYI_*` env vars parsed once at startup.
//!
//! 各 crate 通过 `&Config` 传参，不再各自读取 `std::env::var`。

/// 解析布尔型环境变量（`"1"`/`"true"`/`"yes"`/`"on"` → true；`"0"`/`"false"`/`"no"`/`"off"` → false）。
/// Parse boolean env var; case-insensitive.
pub fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(s) if s.is_empty() => default,
        Ok(s) => {
            let s = s.to_ascii_lowercase();
            match s.as_str() {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                _ => default,
            }
        }
        Err(_) => default,
    }
}

/// 解析 `usize` 环境变量，不存在时返回 `default`。
/// EN: Parses a `usize` env var; returns `default` when unset.
pub fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// 解析可选 `usize`（不存在 → None）。
/// EN: Parses an optional `usize` (absent → None).
pub fn env_usize_optional(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
}

// ── HNSW 图参数 ────────────────────────────────────────────
// EN: ── HNSW graph parameters ──────────────────────────────

#[derive(Debug, Clone)]
pub struct HnswConfig {
    /// 每层最大邻接数。
    /// EN: Max neighbors per layer (M).
    pub m: usize,
    /// 构图 greedy 搜索列表宽度。
    /// EN: Build-time beam width.
    pub ef_construction: usize,
    /// 查询 beam 宽度。
    /// EN: Query-time beam width.
    pub ef_search: usize,
    /// 最大层数上限。
    /// EN: Max possible layers.
    pub max_layers: usize,
    /// 随机种子。
    /// EN: RNG seed for level assignment.
    pub seed: u64,
    /// Level 0 连接数乘数。2.0 = 2M（默认，召回优先，兼容旧图快照）；
    /// 1.0 = M（高 QPS，需显式设置 `HOUYI_HNSW_LAYER0_MULT=1`）。
    /// Level-0 degree multiplier. 2.0 = 2M (default, recall-first,
    /// snapshot-compatible); 1.0 = M (higher QPS, opt-in via env).
    pub layer0_mult: f64,
    /// 上层连接数乘数。
    /// EN: Upper-layer degree multiplier.
    pub upper_mult: f64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self {
            m: 24,
            ef_construction: 400,
            ef_search: 512,
            max_layers: 10,
            seed: 0xA7E5_72D8_B7DB_BEEF,
            layer0_mult: 2.0,
            upper_mult: 0.5,
        }
    }
}

impl HnswConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        c.max_layers = env_usize("HOUYI_HNSW_MAX_LAYERS", 10).clamp(1, 16);
        c.seed = std::env::var("HOUYI_HNSW_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(c.seed);
        c.layer0_mult = std::env::var("HOUYI_HNSW_LAYER0_MULT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&f| f > 0.0)
            .unwrap_or(c.layer0_mult);
        c.upper_mult = std::env::var("HOUYI_HNSW_UPPER_MULT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&f| f > 0.0)
            .unwrap_or(c.upper_mult);
        c
    }
}

// ── 批量构图参数 ────────────────────────────────────────────
// EN: ── Batch build parameters ─────────────────────────────

#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// 单次 drain 最大向量数。
    /// EN: Max vectors per drain (clamped to [64, 256*1024]).
    pub max_build_batch: usize,
    /// 一次性取走队列全部待构图项。
    /// EN: Drain entire pending queue in one shot.
    pub optimize_drain_all: bool,
    /// 打印构图阶段耗时分解。
    /// EN: Print per-stage timing breakdown.
    pub breakdown_enabled: bool,
    /// 收集精确边数量指标（遍历全图）。
    /// EN: Collect exact edge counts (full graph scan).
    pub collect_edge_metrics: bool,
    /// 启用并行批量构图。
    /// EN: Enable parallel batch HNSW build.
    pub parallel_batch_build: bool,
    /// 并行构图最小批大小。
    /// EN: Min batch size for parallel path.
    pub parallel_min_batch: usize,
    /// 并行构图前图内最小点数。
    /// EN: Min graph size before parallel is eligible.
    pub parallel_min_graph: usize,
    /// 并行 chunk 大小。
    /// EN: Parallel planning chunk size.
    pub parallel_chunk_size: usize,
    /// 并行构图最小 chunk 大小。
    /// EN: Min chunk to use parallel (else serial).
    pub parallel_min_chunk: usize,
    /// 每组 chunk 数量（一次提交）。
    /// EN: Chunks per apply group.
    pub parallel_plan_group_chunks: usize,
    /// 图内点数 < K 时从 fresh 头单线程插入。
    /// EN: Seed ST insert threshold (0 = disable).
    pub seed_serial_points: usize,
    /// 串行合图时每 N 点释放一次写锁（0 = 关闭）。用于压低边写边搜的尾延迟。
    /// Release the graph write lock every N inserts in the serial merge path
    /// (0 = disabled). Bounds search wait during streaming builds.
    pub commit_chunk: usize,
    /// 每轮 builder drain 后 yield。
    /// EN: Yield after each drain round.
    pub builder_yield: bool,
    /// Bulk 构图 ef_construction 截断上限（None = 不截断）。
    /// EN: Cap for bulk ef.
    pub bulk_ef_construction_cap: Option<usize>,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            max_build_batch: 512,
            optimize_drain_all: false,
            breakdown_enabled: false,
            collect_edge_metrics: false,
            parallel_batch_build: false,
            parallel_min_batch: 512,
            parallel_min_graph: 4096,
            parallel_chunk_size: 256,
            parallel_min_chunk: 64,
            parallel_plan_group_chunks: 4,
            seed_serial_points: 256,
            commit_chunk: 0,
            builder_yield: false,
            bulk_ef_construction_cap: None,
        }
    }
}

impl BuildConfig {
    pub fn from_env() -> Self {
        Self {
            max_build_batch: env_usize("HOUYI_HNSW_MAX_BUILD_BATCH", 512).clamp(64, 256 * 1024),
            optimize_drain_all: env_bool("HOUYI_HNSW_OPTIMIZE_DRAIN_ALL", false),
            breakdown_enabled: env_bool("HOUYI_HNSW_OPTIMIZE_BREAKDOWN", false),
            collect_edge_metrics: env_bool("HOUYI_HNSW_COLLECT_EDGE_METRICS", false),
            parallel_batch_build: env_bool("HOUYI_HNSW_PARALLEL_BATCH_BUILD", false),
            parallel_min_batch: env_usize("HOUYI_HNSW_PARALLEL_MIN_BATCH", 512).max(1),
            parallel_min_graph: env_usize("HOUYI_HNSW_PARALLEL_MIN_GRAPH", 4096),
            parallel_chunk_size: env_usize("HOUYI_HNSW_PARALLEL_CHUNK_SIZE", 256).max(32),
            parallel_min_chunk: env_usize("HOUYI_HNSW_PARALLEL_MIN_CHUNK", 64).max(8),
            parallel_plan_group_chunks: env_usize("HOUYI_HNSW_PARALLEL_PLAN_GROUP_CHUNKS", 4)
                .max(1),
            seed_serial_points: env_usize("HOUYI_HNSW_SEED_SERIAL_POINTS", 256),
            commit_chunk: env_usize("HOUYI_HNSW_COMMIT_CHUNK", 0),
            builder_yield: env_bool("HOUYI_HNSW_BUILDER_YIELD", false),
            bulk_ef_construction_cap: match std::env::var("HOUYI_HNSW_BULK_EF_CONSTRUCTION")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
            {
                None => None,
                Some(0) => None,
                Some(cap) => Some(cap),
            },
        }
    }
}

// ── 向量索引参数 ────────────────────────────────────────────
// EN: ── Vector index parameters ─────────────────────────────

#[derive(Debug, Clone)]
pub struct VectorConfig {
    pub auto_hnsw: bool,
    pub recall_optimized: bool,
    pub estimated_vectors: Option<usize>,
    pub force_flat_search: bool,
    pub sanity_check: bool,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            auto_hnsw: true,
            recall_optimized: false,
            estimated_vectors: None,
            force_flat_search: false,
            sanity_check: false,
        }
    }
}

impl VectorConfig {
    pub fn from_env() -> Self {
        Self {
            auto_hnsw: env_bool("HOUYI_VECTOR_AUTO_HNSW", true),
            recall_optimized: env_bool("HOUYI_VECTOR_RECALL_OPTIMIZED", false),
            estimated_vectors: env_usize_optional("HOUYI_VECTOR_ESTIMATED_VECTORS"),
            force_flat_search: env_bool("HOUYI_VECTOR_FORCE_FLAT", false),
            sanity_check: env_bool("HOUYI_VECTOR_SANITY_CHECK", false),
        }
    }
}

// ── 优化日志参数 ────────────────────────────────────────────
// EN: ── Optimize log parameters ─────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct OptimizeLogConfig {
    pub quiet: bool,
    pub log_file: Option<String>,
}

impl OptimizeLogConfig {
    pub fn from_env() -> Self {
        Self {
            quiet: env_bool("HOUYI_HNSW_OPTIMIZE_LOG_QUIET", false),
            log_file: std::env::var("HOUYI_HNSW_OPTIMIZE_LOG_FILE")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

// ── 聚合配置 ────────────────────────────────────────────────
// EN: ── Aggregate config ───────────────────────────────────

/// 进程级统一配置，启动时调用 `HouyiConfig::load()` 一次。
/// Process-level config; call `HouyiConfig::load()` once at startup.
#[derive(Debug, Clone)]
pub struct HouyiConfig {
    pub hnsw: HnswConfig,
    pub build: BuildConfig,
    pub vector: VectorConfig,
    pub optimize_log: OptimizeLogConfig,
}

impl HouyiConfig {
    pub fn load() -> Self {
        Self {
            hnsw: HnswConfig::from_env(),
            build: BuildConfig::from_env(),
            vector: VectorConfig::from_env(),
            optimize_log: OptimizeLogConfig::from_env(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_bool_parses_variants() {
        // 这些测试并不实际设置环境变量——它们测试解析逻辑。
        // EN: These tests don't actually set env vars — they test the parsing logic.
        // 未设置环境变量时，env_bool 返回默认值。
        // EN: When no env var is set, env_bool returns the default.
        assert!(env_bool("HOUYI_NONEXISTENT_TEST_VAR_12345", true));
        assert!(!env_bool("HOUYI_NONEXISTENT_TEST_VAR_12345", false));
    }

    #[test]
    fn default_configs_are_valid() {
        let hnsw = HnswConfig::default();
        assert!(hnsw.m >= 2);
        assert!(hnsw.ef_construction >= 1);
        assert!(hnsw.ef_search >= 1);
        assert!(hnsw.max_layers >= 1);

        let build = BuildConfig::default();
        assert!(build.max_build_batch >= 64);
        assert!(build.parallel_chunk_size >= 32);

        let vec = VectorConfig::default();
        assert!(vec.auto_hnsw);
        assert!(!vec.force_flat_search);
    }

    #[test]
    fn houyi_config_load_returns_valid_defaults() {
        let cfg = HouyiConfig::load();
        assert!(cfg.hnsw.m >= 2);
        assert!(cfg.build.max_build_batch >= 64);
    }
}
