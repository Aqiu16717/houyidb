//! 聚类分治 HNSW vs 标准 HNSW 对照基准（真实 OpenAI 数据集，参数对齐 VectorDBBench）。
//!
//! 用法:
//!   1. python3 scripts/export_dataset.py   # 导出 parquet → 二进制
//!   2. cargo run --example cluster_bench --release -- <train.bin> <test.bin> [K]
//!
//! 二进制格式: [n:u64][dims:u64][vectors: f32 × n × dims][ids: u64 × n]

use houyi_core::config::BuildConfig;
use houyi_vector::cluster::{ClusterConfig, ClusteredHnsw};
use houyi_vector::{DistanceMetric, HotBufferIndex, VectorIndex};
use std::fs;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

fn read_dataset(path: &str) -> (Vec<f32>, Vec<u64>, usize) {
    let mut f = fs::File::open(path).expect("open dataset file");
    let mut header = [0u8; 16];
    f.read_exact(&mut header).expect("read header");
    let n = u64::from_le_bytes(header[0..8].try_into().unwrap()) as usize;
    let dims = u64::from_le_bytes(header[8..16].try_into().unwrap()) as usize;

    let vec_bytes = n * dims * 4;
    let id_bytes = n * 8;
    let mut data = vec![0u8; vec_bytes + id_bytes];
    f.read_exact(&mut data).expect("read data");

    let mut vectors = vec![0f32; n * dims];
    for i in 0..n * dims {
        vectors[i] = f32::from_le_bytes(data[i * 4..(i + 1) * 4].try_into().unwrap());
    }

    let mut ids = vec![0u64; n];
    let id_offset = vec_bytes;
    for i in 0..n {
        ids[i] = u64::from_le_bytes(
            data[id_offset + i * 8..id_offset + (i + 1) * 8]
                .try_into()
                .unwrap(),
        );
    }

    (vectors, ids, dims)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: cluster_bench <train.bin> <test.bin> [K]");
        eprintln!("First run: python3 scripts/export_dataset.py");
        std::process::exit(1);
    }
    let train_path = &args[1];
    let test_path = &args[2];
    let k: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
    let tune_alpha: Option<f32> = args.get(4).and_then(|s| s.parse().ok());
    let tune_nprobe: Option<usize> = args.get(5).and_then(|s| s.parse().ok());

    println!("=== ClusteredHnsw vs HNSW (OpenAI dataset, aligned VectorDBBench) ===");
    println!();

    // ── 加载数据 ──────────────────────────────────────────
    // EN: Load data.
    println!("Loading train data from {train_path}...");
    let (vectors, ids, dims) = read_dataset(train_path);
    let n = ids.len();
    println!("Loaded {n} vectors, dims={dims}");

    println!("Loading test queries from {test_path}...");
    let (test_vecs, _test_ids, test_dims) = read_dataset(test_path);
    assert_eq!(dims, test_dims, "train/test dims mismatch");
    let n_queries = test_vecs.len() / dims;
    println!("Loaded {n_queries} queries");

    // ── Ground Truth (neighbors.parquet 预计算) ───────────
    // 真实 neighbor 在 neighbors.parquet 中。简化：用暴力检索前 100 条查 ground truth
    // EN: Ground truth (precomputed in neighbors.parquet). The real neighbors live in
    // neighbors.parquet; simplification: brute-force the first 100 queries for truth.
    let gt_queries = n_queries.min(100);
    println!("Computing ground truth (brute-force on {gt_queries} queries)...");
    let gt_start = Instant::now();
    let ground_truth: Vec<Vec<u64>> = (0..gt_queries)
        .map(|qi| {
            let q = &test_vecs[qi * dims..(qi + 1) * dims];
            let mut scored: Vec<(u64, f32)> = (0..n)
                .map(|i| {
                    let v = &vectors[i * dims..(i + 1) * dims];
                    let dot: f32 = q.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
                    let na: f32 = q.iter().map(|x| x * x).sum();
                    let nb: f32 = v.iter().map(|x| x * x).sum();
                    let d = if na == 0.0 || nb == 0.0 {
                        1.0
                    } else {
                        1.0 - dot / (na.sqrt() * nb.sqrt())
                    };
                    (ids[i], d)
                })
                .collect();
            scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            scored.truncate(k);
            scored.into_iter().map(|(id, _)| id).collect()
        })
        .collect();
    println!(
        "Ground truth done in {:.2}s",
        gt_start.elapsed().as_secs_f64()
    );
    println!();

    // ── Tokio 运行时 ─────────────────────────────────────
    // EN: ── Tokio Runtime ────────────────────────────────────
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_time()
        .build()
        .unwrap();

    // ── ClusteredHnsw ─────────────────────────────────────
    println!("=== ClusteredHnsw ===");
    let mut config = ClusterConfig {
        hnsw_ef_construction: 400,
        hnsw_ef_search: 256,
        hnsw_m: 24,
        ..ClusterConfig::infer(n, dims)
    };
    if let Some(a) = tune_alpha {
        config.boundary_alpha = a;
    }
    if let Some(np) = tune_nprobe {
        config.nprobe = np;
    }
    println!(
        "Config: C={}  nprobe={}  M={}  ef_c={}  ef_s={}  boundary={}",
        config.num_clusters,
        config.nprobe,
        config.hnsw_m,
        config.hnsw_ef_construction,
        config.hnsw_ef_search,
        config.boundary_alpha,
    );

    let build_start = Instant::now();
    let ch = ClusteredHnsw::build(&vectors, &ids, dims, DistanceMetric::Cosine, &config);
    let ch_build = build_start.elapsed();

    // recall 测量（100 条查询对 ground truth）
    // EN: Recall measurement (100 queries vs ground truth).
    let mut ch_hits = 0usize;
    for qi in 0..gt_queries {
        let q = &test_vecs[qi * dims..(qi + 1) * dims];
        let results = ch.search(q, k);
        let truth = &ground_truth[qi];
        ch_hits += results.iter().filter(|(id, _)| truth.contains(id)).count();
    }
    let ch_recall = ch_hits as f64 / (gt_queries * k) as f64;

    // QPS: 并行搜索全部 1000 查询 (Rayon 多线程，模拟并发请求)
    // EN: QPS: search all 1000 queries in parallel (Rayon multithreading, simulating
    // concurrent requests).
    let qps_start = Instant::now();
    let total_q = n_queries;
    use rayon::prelude::*;
    let _all_results: Vec<Vec<(u64, f32)>> = (0..total_q)
        .into_par_iter()
        .map(|qi| {
            let q = &test_vecs[qi * dims..(qi + 1) * dims];
            ch.search(q, k)
        })
        .collect();
    let qps_elapsed = qps_start.elapsed();
    let ch_qps = total_q as f64 / qps_elapsed.as_secs_f64();

    println!(
        "Build: {:.2}s | Recall@{k}: {:.4} | QPS: {:.0} | vectors: {}",
        ch_build.as_secs_f64(),
        ch_recall,
        ch_qps,
        ch.total_vectors(),
    );
    println!();

    // ── HotBufferIndex (生产参数) ──────────────────────────
    // EN: HotBufferIndex (production parameters).
    println!("=== HotBufferIndex (prod params) ===");

    let build_config = BuildConfig {
        parallel_batch_build: true,
        max_build_batch: 8192,
        ..BuildConfig::default()
    };

    let build_start = Instant::now();
    let mut index = HotBufferIndex::new_with_metric(dims, DistanceMetric::Cosine);
    index.set_build_config(build_config);
    index.set_hnsw_m(24).ok();
    index.set_hnsw_ef_construction(400).ok();
    index.set_hnsw_ef_search(256).ok();
    let index = Arc::new(index);

    rt.block_on(async {
        let chunk_size = 100;
        let all_chunks: Vec<Vec<(u64, Vec<f32>)>> = (0..n)
            .collect::<Vec<_>>()
            .chunks(chunk_size)
            .map(|c| {
                c.iter()
                    .map(|&i| (ids[i], vectors[i * dims..(i + 1) * dims].to_vec()))
                    .collect()
            })
            .collect();
        let workers = 8;
        let per_worker = all_chunks.len().div_ceil(workers);
        let mut handles = vec![];
        for wc in all_chunks.chunks(per_worker) {
            let idx = index.clone();
            let batch = wc.to_vec();
            handles.push(tokio::spawn(async move {
                for chunk in batch {
                    idx.insert_batch(chunk).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        while index.pending_build_count().await > 0 {
            index.run_async_builder_once().await;
        }
    });
    let hns_build = build_start.elapsed();
    let total_q = n_queries;

    // recall 测量
    // EN: Recall measurement.
    let mut hns_hits = 0usize;
    rt.block_on(async {
        for qi in 0..gt_queries {
            let q = &test_vecs[qi * dims..(qi + 1) * dims];
            let results = index.search(q, k, None).await;
            let truth = &ground_truth[qi];
            hns_hits += results.iter().filter(|id| truth.contains(id)).count();
        }
    });
    let hns_recall = hns_hits as f64 / (gt_queries * k) as f64;

    // QPS 测量：并行搜索（模拟 VectorDBBench 并发）
    // EN: QPS measurement: parallel search (simulating VectorDBBench concurrency).
    let qps_start = Instant::now();
    rt.block_on(async {
        let mut handles = vec![];
        for chunk in (0..total_q).collect::<Vec<_>>().chunks(total_q / 8 + 1) {
            let idx = index.clone();
            let queries_chunk: Vec<Vec<f32>> = chunk
                .iter()
                .map(|&qi| test_vecs[qi * dims..(qi + 1) * dims].to_vec())
                .collect();
            let k_val = k;
            handles.push(tokio::spawn(async move {
                let mut results = Vec::new();
                for q in &queries_chunk {
                    results.push(idx.search(q, k_val, None).await);
                }
                results
            }));
        }
        for h in handles {
            let _ = h.await.unwrap();
        }
    });
    let hns_search = qps_start.elapsed();
    let hns_qps = total_q as f64 / hns_search.as_secs_f64();

    println!(
        "Build: {:.2}s | Recall@{k}: {:.4} | QPS: {:.0} | indexed: {}",
        hns_build.as_secs_f64(),
        hns_recall,
        hns_qps,
        rt.block_on(index.indexed_count()),
    );
    println!();

    // ── 对比 ──────────────────────────────────────────────
    // EN: Comparison.
    println!("=== Comparison ===");
    let sep = "─";
    println!(
        "{:<16} {:>12} {:>12} {:>12}",
        "Metric", "Clustered", "HNSW", "Delta"
    );
    println!(
        "{:<16} {:>12} {:>12} {:>12}",
        sep.repeat(16),
        sep.repeat(12),
        sep.repeat(12),
        sep.repeat(12)
    );

    let bd = (ch_build.as_secs_f64() - hns_build.as_secs_f64()) / hns_build.as_secs_f64() * 100.0;
    let rd = (ch_recall - hns_recall) / hns_recall * 100.0;
    let qd = (ch_qps - hns_qps) / hns_qps * 100.0;

    println!(
        "{:<16} {:>10.2}s {:>10.2}s {:>+10.1}%",
        "Build",
        ch_build.as_secs_f64(),
        hns_build.as_secs_f64(),
        bd
    );
    println!(
        "{:<16} {:>10.4} {:>10.4} {:>+10.1}%",
        format!("Recall@{k}"),
        ch_recall,
        hns_recall,
        rd
    );
    println!(
        "{:<16} {:>10.0} {:>10.0} {:>+10.1}%",
        "QPS", ch_qps, hns_qps, qd
    );
    println!(
        "{:<16} {:>10} {:>10}",
        "Vectors",
        ch.total_vectors(),
        rt.block_on(index.indexed_count())
    );
}
