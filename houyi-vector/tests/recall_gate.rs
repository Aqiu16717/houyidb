//! Recall 门禁测试：与 `benches/vector_baseline.rs` 的 `recall_check_hnsw_after_flush` 同配置，
//! 但把 recall 从「算了不判」变成真正的阈值断言。数据集/对照/阈值共享自 `tests/common/recall.rs`。
//!
//! Recall gate: same configs as the `recall_check_hnsw_after_flush` bench, but asserts a
//! minimum recall@k so regressions fail CI instead of being silently black_boxed away.
//!
//! 环境前提 / EN: This gate is only meaningful under default `HOUYI_*` env.
//! `HotBufferIndex::new` reads `VectorConfig/HnswConfig/BuildConfig::from_env()` — e.g.
//! `HOUYI_VECTOR_FORCE_FLAT=1` turns search into an exact brute-force scan (recall ≡ 1.0)
//! and the gate passes without exercising HNSW at all; `HOUYI_HNSW_*` knobs silently
//! switch the build path. CI must run with `HOUYI_*` unset; `BuildConfig` is pinned
//! explicitly below (the rest are not pinnable post-construction).

use houyi_core::config::BuildConfig;
use houyi_vector::{HotBufferIndex, VectorIndex};

#[path = "common/recall.rs"]
mod recall;
use recall::{brute_force_topk, make_vector, recall_at_k, RECALL_MIN};

/// 跑一组 (dims, total, k, ef)，返回实测 recall。 / Run one config, return measured recall.
/// EN: Run one config and return the measured recall.
async fn measure_recall(dims: usize, total: usize, k: usize, ef: usize) -> f32 {
    let dataset: Vec<(u64, Vec<f32>)> = (0..total as u64)
        .map(|id| (id, make_vector(id, dims)))
        .collect();
    let query = make_vector((total / 3) as u64, dims);
    let truth = brute_force_topk(&dataset, &query, k);

    let mut index = HotBufferIndex::new(dims);
    // 钉住可钉的构建配置，使被测构建路径不受环境变量影响（串行/512，与实测基线一致）。
    // EN: Pin the build path so ambient HOUYI_HNSW_* env cannot silently switch it.
    index.set_build_config(BuildConfig::default());
    index
        .set_hnsw_ef_search(ef)
        .expect("set ef_search should succeed");
    index
        .insert_batch(dataset)
        .await
        .expect("batch insert should succeed");
    while index.pending_build_count().await > 0 {
        let _ = index.run_async_builder_once().await;
    }
    let pred = index.search(&query, k, None).await;
    recall_at_k(&pred, &truth, k)
}

#[tokio::test]
async fn recall_gate_after_flush_meets_threshold() {
    for &(dims, total, k) in &[(128usize, 1000usize, 10usize), (512, 2000, 10)] {
        for ef in [8usize, 16, 32] {
            let recall = measure_recall(dims, total, k, ef).await;
            assert!(
                recall >= RECALL_MIN,
                "recall regression: dims={dims} total={total} k={k} ef={ef} \
                 recall={recall:.4} < {RECALL_MIN}"
            );
        }
    }
}
