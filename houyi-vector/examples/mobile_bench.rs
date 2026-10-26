//! HouyiDB Mobile 对标 MicroNN 基准（支持真实数据集）。
//!
//! 用法:
//!   cargo run --example mobile_bench --release -- <train.bin> <test.bin> [K] [INDEX_TYPE]
//!   或: cargo run --example mobile_bench --release -- N DIMS K INDEX_TYPE  (随机数据)

use houyi_vector::cluster::{ClusterConfig, ClusteredHnsw, FlatClustered, PqFlatClustered};
use houyi_vector::DistanceMetric;
use std::fs;
use std::io::Read;
use std::time::Instant;

/// 统一检索闭包：查询向量 → 命中 ID 列表。
/// EN: Unified search closure: query vector → list of hit IDs.
type SearchFn = Box<dyn Fn(&[f32]) -> Vec<u64>>;

fn read_binary(path: &str) -> (Vec<f32>, Vec<u64>, usize) {
    let mut f = fs::File::open(path).expect("open");
    let mut h = [0u8; 16];
    f.read_exact(&mut h).unwrap();
    let n = u64::from_le_bytes(h[0..8].try_into().unwrap()) as usize;
    let dims = u64::from_le_bytes(h[8..16].try_into().unwrap()) as usize;
    let mut data = vec![0u8; n * dims * 4 + n * 8];
    f.read_exact(&mut data).unwrap();
    let mut vecs = vec![0f32; n * dims];
    for i in 0..n * dims {
        vecs[i] = f32::from_le_bytes(data[i * 4..(i + 1) * 4].try_into().unwrap());
    }
    let mut ids = vec![0u64; n];
    let off = n * dims * 4;
    for i in 0..n {
        ids[i] = u64::from_le_bytes(data[off + i * 8..off + (i + 1) * 8].try_into().unwrap());
    }
    (vecs, ids, dims)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let vectors: Vec<f32>;
    let ids: Vec<u64>;
    let dims: usize;
    let test_vecs: Vec<f32>;
    let _test_ids: Vec<u64>;
    let k: usize;
    let index_type: String;
    let n: usize;

    if args.len() >= 3 && args[1].ends_with(".bin") {
        // 真实数据模式
        // EN: Real data mode.
        let (v, i, d) = read_binary(&args[1]);
        let (tv, ti, td) = read_binary(&args[2]);
        assert_eq!(d, td, "dim mismatch");
        vectors = v;
        ids = i;
        dims = d;
        test_vecs = tv;
        _test_ids = ti;
        k = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100);
        index_type = args
            .get(4)
            .cloned()
            .unwrap_or_else(|| "flat-clustered".to_string());
        n = ids.len();
    } else {
        // 随机数据模式
        // EN: Random data mode.
        n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50000);
        dims = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
        k = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100);
        index_type = args
            .get(4)
            .cloned()
            .unwrap_or_else(|| "flat-clustered".to_string());
        use rand::Rng;
        use rand::SeedableRng;
        let seed: u64 = 42;
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut v = Vec::with_capacity(n * dims);
        let mut i: Vec<u64> = Vec::with_capacity(n);
        for idx in 0..n {
            i.push(idx as u64);
            for _ in 0..dims {
                v.push(rng.gen::<f32>());
            }
        }
        vectors = v;
        ids = i;
        let nq = 100;
        let mut tv = Vec::with_capacity(nq * dims);
        let mut ti: Vec<u64> = Vec::with_capacity(nq);
        for idx in 0..nq {
            ti.push(idx as u64);
            for _ in 0..dims {
                tv.push(rng.gen::<f32>());
            }
        }
        test_vecs = tv;
        _test_ids = ti;
    }

    let nq = test_vecs.len() / dims;
    let mem_per_vec = dims * 4;
    let total_mem_mb = n * mem_per_vec / 1024 / 1024;

    println!("=== HouyiDB Mobile vs MicroNN ===");
    println!("N={n} DIM={dims} K={k} INDEX={index_type}  Mem={total_mem_mb}MB");
    println!("Data: {} vectors", n);
    println!();

    // ── 构建 ──────────────────────────────────────
    // EN: Build.
    let build_start = Instant::now();
    let c = (n as f64).sqrt().ceil() as usize;

    let (results_fn, index_desc): (SearchFn, String) = match index_type.as_str() {
        "flat" => {
            let data: Vec<(u64, Vec<f32>)> = (0..n)
                .map(|i| (ids[i], vectors[i * dims..(i + 1) * dims].to_vec()))
                .collect();
            let data = std::sync::Arc::new(data);
            let ek = k;
            let f: SearchFn = Box::new(move |q: &[f32]| {
                let mut s: Vec<(u64, f32)> = data
                    .iter()
                    .map(|(id, v)| {
                        let d: f32 = q
                            .iter()
                            .zip(v.iter())
                            .map(|(a, b)| {
                                let d = a - b;
                                d * d
                            })
                            .sum();
                        (*id, d)
                    })
                    .collect();
                s.sort_by(|a, b| a.1.total_cmp(&b.1));
                s.truncate(ek);
                s.into_iter().map(|(id, _)| id).collect()
            });
            println!(
                "Build: {:.2}s (flat, no build)",
                build_start.elapsed().as_secs_f64()
            );
            (f, "Flat (brute-force)".to_string())
        }
        "flat-clustered" => {
            let fc = FlatClustered::build(&vectors, &ids, dims, DistanceMetric::Cosine, c, 10, 0);
            let fc = std::sync::Arc::new(fc);
            let ek = k;
            println!(
                "Build: {:.2}s  C={c}  vectors={}",
                build_start.elapsed().as_secs_f64(),
                fc.total_vectors()
            );
            (
                Box::new(move |q| fc.search(q, ek).into_iter().map(|(id, _)| id).collect()),
                "FlatClustered".to_string(),
            )
        }
        "pq-flat" => {
            let mmap_path = "/tmp/houyi_pq_vectors.bin";
            let pqf = PqFlatClustered::build_with_mmap(
                &vectors,
                &ids,
                dims,
                DistanceMetric::L2,
                c,
                10,
                0,
                Some(mmap_path),
            );
            let mem_mb = pqf.memory_mb();
            let disk_mb = pqf.disk_mb();
            let pqf = std::sync::Arc::new(pqf);
            let ek = k;
            println!(
                "Build: {:.2}s  C={c}  vectors={}  RAM={mem_mb:.1}MB  Disk={disk_mb:.0}MB",
                build_start.elapsed().as_secs_f64(),
                pqf.total_vectors()
            );
            let desc = format!("PQ+Rerank(RAM:{mem_mb:.0}MB Disk:{disk_mb:.0}MB)");
            (
                Box::new(move |q| pqf.search(q, ek).into_iter().map(|(id, _)| id).collect()),
                desc,
            )
        }
        _ => {
            let cfg = ClusterConfig {
                hnsw_ef_construction: 200,
                hnsw_ef_search: 128,
                hnsw_m: 12,
                ..ClusterConfig::infer(n, dims)
            };
            let ch = ClusteredHnsw::build(&vectors, &ids, dims, DistanceMetric::Cosine, &cfg);
            let ch = std::sync::Arc::new(ch);
            let ek = k;
            println!(
                "Build: {:.2}s  vectors={}",
                build_start.elapsed().as_secs_f64(),
                ch.total_vectors()
            );
            (
                Box::new(move |q| ch.search(q, ek).into_iter().map(|(id, _)| id).collect()),
                "ClusteredHnsw".to_string(),
            )
        }
    };

    // ── 延迟 ──────────────────────────────────────
    // EN: Latency.
    let search_start = Instant::now();
    for qi in 0..nq {
        let q = &test_vecs[qi * dims..(qi + 1) * dims];
        let _ = results_fn(q);
    }
    let search_elapsed = search_start.elapsed();
    let avg_ms = search_elapsed.as_secs_f64() / nq as f64 * 1000.0;
    let qps = nq as f64 / search_elapsed.as_secs_f64();

    // ── Recall ────────────────────────────────────
    // 注意：真值按 L2² 排序，而 flat-clustered/HNSW 模式按 Cosine 构图——
    // 两种度量的 recall 口径存在偏差（仅 pq-flat 分支同为 L2，口径一致）。
    // EN: ground truth ranks by L2² while flat-clustered/HNSW build with Cosine,
    // so the recall gate mixes metrics (only pq-flat is consistent).
    println!("Recall vs brute-force ({nq} queries)...");
    let mut hits = 0usize;
    let eval_n = nq.min(100);
    for qi in 0..eval_n {
        let q = &test_vecs[qi * dims..(qi + 1) * dims];
        let pred: ahash::AHashSet<u64> = results_fn(q).into_iter().collect();
        let mut brute: Vec<(u64, f32)> = (0..n)
            .map(|i| {
                let v = &vectors[i * dims..(i + 1) * dims];
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
        brute.sort_by(|a, b| a.1.total_cmp(&b.1));
        let truth: ahash::AHashSet<u64> = brute.iter().take(k).map(|(id, _)| *id).collect();
        hits += pred.intersection(&truth).count();
    }
    let recall_val = hits as f64 / (eval_n * k) as f64;

    // ── 报告 ──────────────────────────────────────
    // EN: Report.
    println!();
    println!("=== Results ({}) ===", index_desc);
    println!("  Latency avg:  {:.2} ms", avg_ms);
    println!("  QPS:          {:.0}", qps);
    println!("  Recall@{k}:    {:.4}", recall_val);
    println!("  Memory raw:   {total_mem_mb} MB");
    println!();
    println!("=== MicroNN Targets ===");
    println!(
        "  Latency:      < 7 ms  →  {}",
        if avg_ms < 7.0 { "✅" } else { "❌" }
    );
    println!(
        "  Recall@100:   ≥ 0.90  →  {}",
        if recall_val >= 0.90 { "✅" } else { "❌" }
    );
    println!("  Memory:       ~10 MB (with PQ)");
}
