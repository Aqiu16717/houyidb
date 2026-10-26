//! 插入 → Async-Builder 构图 → ANN 检索，与 brute-force L2 真值对照，验证 recall@k。
//! 用法见 `--help`；通过 `--min-recall`（默认 0.99）控制门禁（默认用**平均** recall）。
//! EN: End-to-end recall@k vs brute-force; use for Recall-Check / CI gate.
//!
//! ```text
//! HOUYI_VECTOR_AUTO_HNSW=0 cargo run -p houyi-vector --example recall_gate -- --help
//! ```

use houyi_vector::{DistanceMetric, HotBufferIndex, VectorIndex};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::env;

fn next_parse<T: std::str::FromStr>(it: &mut impl Iterator<Item = String>) -> T
where
    T::Err: std::fmt::Debug,
{
    it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        eprintln!("missing or invalid value after flag");
        std::process::exit(2);
    })
}

fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

struct Args {
    n: usize,
    dims: usize,
    k: usize,
    num_queries: usize,
    min_recall: f32,
    use_min_not_mean: bool,
    seed: u64,
    m: usize,
    ef_construction: usize,
    ef_search: usize,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            n: 3_000,
            dims: 64,
            k: 10,
            num_queries: 80,
            min_recall: 0.99,
            use_min_not_mean: false,
            seed: 0x00C0_FFEE_BEEF_u64,
            m: 24,
            ef_construction: 400,
            ef_search: 512,
        };
        let mut it = env::args().skip(1);
        while let Some(arg) = it.next() {
            if arg == "--help" || arg == "-h" {
                eprintln!(
                    "recall_gate — 验证 insert / 构图 / search 的 recall@k（L2 brute-force 真值）\n\
                    \n\
                    选项（均有合理默认，可调高 ef_* 以冲 >99%%）：\n\
                      --n N              向量条数（默认 {}）\n\
                      --dims D           维度（默认 {}）\n\
                      --k K              top-k（默认 {}）\n\
                      --queries Q        随机查询条数（默认 {}）\n\
                      --min-recall R     门禁阈值（默认 {}）\n\
                      --strict-min       用「各次 recall 最小值」而非平均（更严）\n\
                      --seed U64         随机种子\n\
                      --m / --efc / --efs  HNSW 参数\n",
                    a.n, a.dims, a.k, a.num_queries, a.min_recall
                );
                std::process::exit(0);
            }
            match arg.as_str() {
                "--n" => a.n = next_parse(&mut it),
                "--dims" => a.dims = next_parse(&mut it),
                "--k" => a.k = next_parse(&mut it),
                "--queries" => a.num_queries = next_parse(&mut it),
                "--min-recall" => a.min_recall = next_parse(&mut it),
                "--strict-min" => a.use_min_not_mean = true,
                "--seed" => a.seed = next_parse(&mut it),
                "--m" => a.m = next_parse(&mut it),
                "--efc" => a.ef_construction = next_parse(&mut it),
                "--efs" => a.ef_search = next_parse(&mut it),
                _ => eprintln!("unknown arg {arg}, try --help"),
            }
        }
        a
    }
}

async fn drain(index: &HotBufferIndex) {
    while index.pending_build_count().await > 0 {
        let _ = index.run_async_builder_once().await;
    }
}

#[tokio::main]
async fn main() {
    // 关闭自动推断，使参数完全由本程序与 CLI 决定。
    // EN: Disable auto-inference so this program and the CLI fully determine the
    // parameters.
    env::set_var("HOUYI_VECTOR_AUTO_HNSW", "0");

    let args = Args::parse();
    if args.k == 0 || args.n < args.k + 10 {
        eprintln!("need n >> k (e.g. n>=k+10)");
        std::process::exit(2);
    }

    let mut rng = StdRng::seed_from_u64(args.seed);
    let data: Vec<(u64, Vec<f32>)> = (0..args.n as u64)
        .map(|id| {
            let v: Vec<f32> = (0..args.dims)
                .map(|_| rng.gen_range(-1.0_f32..1.0_f32))
                .collect();
            (id, v)
        })
        .collect();

    let index = HotBufferIndex::new_with_metric(args.dims, DistanceMetric::L2);
    index.set_hnsw_m(args.m).expect("m");
    index
        .set_hnsw_ef_construction(args.ef_construction)
        .expect("efc");
    index.set_hnsw_ef_search(args.ef_search).expect("efs");

    index
        .insert_batch(data.clone())
        .await
        .expect("insert_batch");
    drain(&index).await;

    let mut recalls = Vec::with_capacity(args.num_queries);
    for qi in 0..args.num_queries {
        let qvec = &data[qi % data.len()].1;
        let mut scored: Vec<(u64, f32)> =
            data.iter().map(|(id, v)| (*id, l2_sq(qvec, v))).collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let truth: std::collections::HashSet<u64> =
            scored.into_iter().take(args.k).map(|(id, _)| id).collect();

        let pred = index.search(qvec, args.k, None).await;
        let hit = pred.iter().filter(|id| truth.contains(id)).count();
        recalls.push(hit as f32 / args.k as f32);
    }

    let mean = recalls.iter().sum::<f32>() / recalls.len() as f32;
    let min_r = recalls
        .iter()
        .copied()
        .min_by(f32::total_cmp)
        .unwrap_or(0.0);

    println!(
        "recall_gate: n={} dims={} k={} queries={} m={} efc={} efs={}",
        args.n, args.dims, args.k, args.num_queries, args.m, args.ef_construction, args.ef_search
    );
    println!(
        "  mean@{} = {:.4}  min@{} = {:.4}",
        args.k, mean, args.k, min_r
    );

    let ok = if args.use_min_not_mean {
        min_r + 1e-6 >= args.min_recall
    } else {
        mean + 1e-6 >= args.min_recall
    };
    if !ok {
        eprintln!(
            "FAIL: need {} >= {:.4} (use higher --efs/--efc or more benign --dims/--n)",
            if args.use_min_not_mean { "min" } else { "mean" },
            args.min_recall
        );
        std::process::exit(1);
    }
    println!(
        "PASS (threshold {:.4}, {})",
        args.min_recall,
        if args.use_min_not_mean { "min" } else { "mean" }
    );
}
