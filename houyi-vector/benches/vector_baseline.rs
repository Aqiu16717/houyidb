use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use houyi_vector::distance;
use houyi_vector::{HotBufferIndex, VectorIndex};

// Recall 门禁的数据集/对照/阈值与 tests/recall_gate.rs 共享同一份实现（单源防漂移）。
// EN: Recall-gate helpers shared with tests/recall_gate.rs (single source of truth).
#[path = "../tests/common/recall.rs"]
mod recall;
use recall::{brute_force_topk, make_vector, recall_at_k, RECALL_MIN};

/// 纯距离函数吞吐（optimize / search 热点）；用于验证 `distance` 模块优化是否生效。
/// Raw distance throughput (hot path for build/search); validates `distance` module optimizations.
fn bench_distance_pure(c: &mut Criterion) {
    let dims = 1536usize;
    let a: Vec<f32> = (0..dims).map(|i| (i % 17) as f32 * 0.03).collect();
    let b: Vec<f32> = (0..dims).map(|i| (i % 13) as f32 * 0.07).collect();
    let mut group = c.benchmark_group("distance_pure");
    group.bench_function("cosine_1536", |ben| {
        ben.iter(|| {
            criterion::black_box(distance::cosine(
                criterion::black_box(a.as_slice()),
                criterion::black_box(b.as_slice()),
            ))
        })
    });
    group.bench_function("l2_squared_1536", |ben| {
        ben.iter(|| {
            criterion::black_box(distance::l2_squared(
                criterion::black_box(a.as_slice()),
                criterion::black_box(b.as_slice()),
            ))
        })
    });
    group.finish();
}

fn bench_insert(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let mut group = c.benchmark_group("insert_single");
    for dims in [128usize, 512, 1536] {
        group.bench_with_input(BenchmarkId::from_parameter(dims), &dims, |b, &dims| {
            b.to_async(&rt).iter(|| async move {
                let index = HotBufferIndex::new(dims);
                index
                    .insert(1, vec![0.1_f32; dims])
                    .await
                    .expect("insert should succeed");
            });
        });
    }
    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let mut group = c.benchmark_group("search_hot_buffer");
    for &(dims, count) in &[(128usize, 100usize), (512, 1000), (1536, 1000)] {
        group.bench_with_input(
            BenchmarkId::new("dims_count", format!("{dims}_{count}")),
            &(dims, count),
            |b, &(dims, count)| {
                b.to_async(&rt).iter(|| async move {
                    let index = HotBufferIndex::new(dims);
                    for id in 0..count as u64 {
                        index
                            .insert(id, vec![0.2_f32; dims])
                            .await
                            .expect("seed insert should succeed");
                    }
                    let _ = index.search(&vec![0.2_f32; dims], 10, None).await;
                });
            },
        );
    }
    group.finish();
}

fn bench_build_flush(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let mut group = c.benchmark_group("build_flush_once");
    for &(dims, batch) in &[(128usize, 1_000usize), (512, 1_000), (1536, 2_000)] {
        group.bench_with_input(
            BenchmarkId::new("dims_batch", format!("{dims}_{batch}")),
            &(dims, batch),
            |b, &(dims, batch)| {
                b.to_async(&rt).iter(|| async move {
                    let index = HotBufferIndex::new(dims);
                    let vectors: Vec<(u64, Vec<f32>)> = (0..batch as u64)
                        .map(|id| (id, vec![1.0_f32; dims]))
                        .collect();
                    index
                        .insert_batch(vectors)
                        .await
                        .expect("batch insert should succeed");
                    let _ = index.run_async_builder_once().await;
                });
            },
        );
    }
    group.finish();
}

/// 50K 向量切 16 批（每批 3125）并完整排空构图队列，模拟 `/v1/optimize` 的多轮 drain。
/// EN: Split 50K vectors into 16 batches (3125 each), then fully drain builder queue.
fn bench_build_flush_50k_16_batches(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let mut group = c.benchmark_group("build_flush_50k_16_batches");
    let dims = 1536usize;
    let total = 50_000usize;
    let batches = 16usize;
    let batch = total / batches;
    group.bench_function("dims_total_batches/1536_50000_16", |b| {
        b.to_async(&rt).iter(|| async move {
            let index = HotBufferIndex::new(dims);
            for bi in 0..batches {
                let start = (bi * batch) as u64;
                let end = if bi + 1 == batches {
                    total as u64
                } else {
                    ((bi + 1) * batch) as u64
                };
                let vectors: Vec<(u64, Vec<f32>)> =
                    (start..end).map(|id| (id, vec![1.0_f32; dims])).collect();
                index
                    .insert_batch(vectors)
                    .await
                    .expect("batch insert should succeed");
            }
            while index.pending_build_count().await > 0 {
                let _ = index.run_async_builder_once().await;
            }
        });
    });
    group.finish();
}

/// 先 `insert_batch` 再排空 `Async-Builder` 队列，使 `main` 上 HNSW 生效；否则仅在 HotBuffer 上测到「伪 recall」。
/// EN: Call `insert_batch` first, then drain the `Async-Builder` queue so HNSW takes
/// effect on `main`; otherwise the measured "recall" only reflects the HotBuffer.
fn bench_recall_check(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let mut group = c.benchmark_group("recall_check_hnsw_after_flush");
    for &(dims, total, k) in &[(128usize, 1000usize, 10usize), (512, 2000, 10)] {
        let dataset: Vec<(u64, Vec<f32>)> = (0..total as u64)
            .map(|id| (id, make_vector(id, dims)))
            .collect();
        let query = make_vector((total / 3) as u64, dims);
        let truth = brute_force_topk(&dataset, &query, k);
        let ef_values = [8usize, 16, 32];

        for ef in ef_values {
            // 不在注册阶段做 block_on/println，否则即使用 Criterion 过滤其它组，仍会执行本段并重算 recall。
            // EN: Avoid heavy work at registration time; it breaks `cargo bench -- <filter>`.
            group.bench_with_input(
                BenchmarkId::new("dims_total_k_ef", format!("{dims}_{total}_{k}_{ef}")),
                &(dims, total, k, ef),
                |b, &(dims, _total, k, ef)| {
                    let dataset = dataset.clone();
                    let query = query.clone();
                    let truth = truth.clone();
                    b.to_async(&rt).iter(|| async {
                        let index = HotBufferIndex::new(dims);
                        index
                            .set_hnsw_ef_search(ef)
                            .expect("set ef_search should succeed");
                        index
                            .insert_batch(dataset.clone())
                            .await
                            .expect("batch insert should succeed");
                        while index.pending_build_count().await > 0 {
                            let _ = index.run_async_builder_once().await;
                        }
                        let pred = index.search(&query, k, None).await;
                        let recall = recall_at_k(&pred, &truth, k);
                        // 门槛断言：recall 空转不判定等于没有 Recall-Check。
                        // 阈值单源在 tests/common/recall.rs（k=10 下 0.95 等效满分门禁，
                        // 六组实测均为 1.0）。环境前提同 recall_gate.rs：HOUYI_VECTOR_FORCE_FLAT=1
                        // 会把 search 切到暴力扫描使本断言空转，CI 须在干净 HOUYI_* 环境下运行。
                        // EN: Gate on recall instead of black_boxing it away; threshold lives in
                        // tests/common/recall.rs. Same clean-env requirement as the gate test.
                        assert!(
                            recall >= RECALL_MIN,
                            "recall regression: dims={dims} k={k} ef={ef} recall={recall:.4} < {RECALL_MIN}"
                        );
                        criterion::black_box(recall);
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(
    vector_baseline,
    bench_distance_pure,
    bench_insert,
    bench_search,
    bench_build_flush,
    bench_build_flush_50k_16_batches,
    bench_recall_check
);
criterion_main!(vector_baseline);
