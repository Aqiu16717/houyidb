//! 依据维度与预估规模推断 HNSW `M` / `ef_construction` / `ef_search`，并支持 Recall-Optimized 模式。
//! Heuristic index defaults for VectorDBBench-credible runs; see `houyidb.md` §3.2.3.

use houyi_core::config::VectorConfig;

/// 推断得到的 HNSW 初始参数（可与 HTTP upsert 覆盖）。
/// EN: Initial HNSW knobs before client-provided index hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferredHnswParams {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

/// 按维度给出 VectorDBBench 常见数据集的**默认预估容量**（未设置 `HOUYI_VECTOR_ESTIMATED_VECTORS` 时使用）。
/// EN: Default cardinality hint per typical dataset family (Glove / Cohere / OpenAI / LAION).
pub fn default_estimated_cardinality(dims: usize) -> usize {
    match dims {
        1..=128 => 1_200_000,
        129..=1024 => 1_000_000,
        _ => 500_000,
    }
}

/// 核心启发式：由维度 `dims`、预估向量数 `estimated_n`、是否 Recall-Optimized 得到一组参数。
/// EN: Core heuristic; higher `estimated_n` and `recall_optimized` increase build/query cost for better recall.
pub fn infer_hnsw_params(
    dims: usize,
    estimated_n: usize,
    recall_optimized: bool,
) -> InferredHnswParams {
    let dims = dims.max(1);
    let mut m = if dims <= 100 {
        16
    } else if dims <= 512 {
        20
    } else {
        24
    };
    if recall_optimized {
        m = (m + 8).min(32);
    }

    let mut ef_c: usize = if estimated_n < 50_000 {
        128
    } else if estimated_n < 500_000 {
        200
    } else {
        400
    };
    if dims >= 1536 {
        ef_c = ef_c.max(200);
    }
    if recall_optimized {
        ef_c = (ef_c.saturating_mul(5) / 4).saturating_add(64).min(800);
    }

    let ef_s = if recall_optimized {
        if dims >= 1536 {
            1024
        } else {
            768
        }
    } else if dims >= 1536 {
        512
    } else if dims >= 768 {
        384
    } else {
        256
    };

    InferredHnswParams {
        m: m.max(2),
        ef_construction: ef_c.max(16),
        ef_search: ef_s.max(16),
    }
}

/// 根据 `VectorConfig` 推断初始 HNSW 参数；`auto_enabled` 表示是否走了自动推断路径。
/// EN: Derive initial HNSW params from config; `auto_enabled` is false when auto-tuning is off.
pub fn load_hnsw_initial_params(dims: usize, cfg: &VectorConfig) -> (InferredHnswParams, bool) {
    if dims == 0 {
        return (
            InferredHnswParams {
                m: 24,
                ef_construction: 400,
                ef_search: 512,
            },
            false,
        );
    }
    if !cfg.auto_hnsw {
        let p = InferredHnswParams {
            m: 24,
            ef_construction: 400,
            ef_search: 512,
        };
        return (p, false);
    }
    let estimated_n = cfg
        .estimated_vectors
        .unwrap_or_else(|| default_estimated_cardinality(dims));
    (
        infer_hnsw_params(dims, estimated_n, cfg.recall_optimized),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DistanceMetric, HotBufferIndex};
    use houyi_core::config::VectorConfig;
    use std::sync::{Mutex, OnceLock};

    /// 串行化会修改进程级 `env` 的测试，避免与其它用例并行时互相污染。
    /// EN: Serialize tests that mutate process environment.
    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("hnsw env test lock poisoned")
    }

    /// 测试结束（含 panic）后恢复 `HOUYI_VECTOR_*` 快照。
    /// EN: Restores env vars on drop so parallel test crates see a clean state.
    struct HnswEnvRestore {
        recall: Option<String>,
        auto: Option<String>,
        estimated: Option<String>,
    }

    impl HnswEnvRestore {
        fn new() -> Self {
            Self {
                recall: std::env::var("HOUYI_VECTOR_RECALL_OPTIMIZED").ok(),
                auto: std::env::var("HOUYI_VECTOR_AUTO_HNSW").ok(),
                estimated: std::env::var("HOUYI_VECTOR_ESTIMATED_VECTORS").ok(),
            }
        }
    }

    impl Drop for HnswEnvRestore {
        fn drop(&mut self) {
            restore_env("HOUYI_VECTOR_RECALL_OPTIMIZED", self.recall.take());
            restore_env("HOUYI_VECTOR_AUTO_HNSW", self.auto.take());
            restore_env("HOUYI_VECTOR_ESTIMATED_VECTORS", self.estimated.take());
        }
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    fn test_config(recall_optimized: bool, estimated: Option<usize>) -> VectorConfig {
        VectorConfig {
            auto_hnsw: true,
            recall_optimized,
            estimated_vectors: estimated,
            force_flat_search: false,
            sanity_check: false,
        }
    }

    #[test]
    fn recall_optimized_config_yields_stronger_params_than_baseline() {
        let base_cfg = test_config(false, None);
        let ro_cfg = test_config(true, None);
        let (base, auto_on) = load_hnsw_initial_params(1536, &base_cfg);
        assert!(auto_on, "auto tuning should be on");
        let (ro, auto_on2) = load_hnsw_initial_params(1536, &ro_cfg);
        assert!(auto_on2);
        assert!(ro.m >= base.m);
        assert!(ro.ef_construction >= base.ef_construction);
        assert!(ro.ef_search >= base.ef_search);
    }

    #[test]
    fn hot_buffer_index_applies_recall_optimized_env_at_construct() {
        let _guard = env_test_lock();
        let _restore = HnswEnvRestore::new();
        std::env::set_var("HOUYI_VECTOR_AUTO_HNSW", "1");
        std::env::remove_var("HOUYI_VECTOR_RECALL_OPTIMIZED");
        let base_ef =
            HotBufferIndex::new_with_metric(1536, DistanceMetric::Cosine).hnsw_ef_search();
        std::env::set_var("HOUYI_VECTOR_RECALL_OPTIMIZED", "1");
        let ro_ef = HotBufferIndex::new_with_metric(1536, DistanceMetric::Cosine).hnsw_ef_search();
        assert!(ro_ef >= base_ef);
    }

    #[test]
    fn recall_optimized_increases_cost() {
        let p = infer_hnsw_params(1536, 500_000, false);
        let q = infer_hnsw_params(1536, 500_000, true);
        assert!(q.m >= p.m);
        assert!(q.ef_construction >= p.ef_construction);
        assert!(q.ef_search >= p.ef_search);
    }

    #[test]
    fn openai_like_dims_get_stronger_defaults_than_glove() {
        let g = infer_hnsw_params(100, 1_200_000, false);
        let o = infer_hnsw_params(1536, 500_000, false);
        assert!(o.ef_search >= g.ef_search);
    }

    #[test]
    fn auto_disabled_uses_fixed_defaults() {
        let cfg = VectorConfig {
            auto_hnsw: false,
            ..VectorConfig::default()
        };
        let (p, auto) = load_hnsw_initial_params(1536, &cfg);
        assert!(!auto);
        assert_eq!(p.m, 24);
        assert_eq!(p.ef_construction, 400);
        assert_eq!(p.ef_search, 512);
    }
}
