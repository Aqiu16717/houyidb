use houyi_vector::{HotBufferIndex, VectorIndex};
use std::time::Instant;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let dims: usize = std::env::var("HOUYI_50K_DIMS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1536);
    let total: usize = std::env::var("HOUYI_50K_TOTAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let batches: usize = std::env::var("HOUYI_50K_BATCHES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
        .max(1);
    let batch = total / batches;

    let index = HotBufferIndex::new(dims);
    let t0 = Instant::now();
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
            .expect("insert_batch should succeed");
    }
    let t_insert = t0.elapsed();

    let t1 = Instant::now();
    while index.pending_build_count().await > 0 {
        let _ = index.run_async_builder_once().await;
    }
    let t_build = t1.elapsed();
    let t_total = t0.elapsed();

    println!(
        "[optimize-50k-16-once] dims={} total={} batches={} insert_ms={} build_ms={} total_ms={}",
        dims,
        total,
        batches,
        t_insert.as_millis(),
        t_build.as_millis(),
        t_total.as_millis()
    );
}
