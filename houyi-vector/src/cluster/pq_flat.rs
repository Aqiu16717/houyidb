//! PQ-FlatClustered: K-Means 分区 + 全局 PQ 码本 + mmap 重排序。
//! PQ in-memory (16MB), vectors on disk. Two-phase: PQ coarse → exact rerank.
//!
//! 内存: 128D × 1M → 16MB codes + 8MB IDs + 128KB codebook ≈ 24MB in RAM
//! 磁盘: 512MB vectors via mmap (按需读取, 每次查 ~200KB)

use super::kmeans;
use super::router;
use crate::pq::{PqConfig, PqIndex};
use crate::DistanceMetric;
use rayon::prelude::*;
use std::fs;
use std::io::Read;

pub struct PqFlatClustered {
    dims: usize,
    num_clusters: usize,
    nprobe: usize,
    centroids: Vec<f32>,
    cluster_ids: Vec<Vec<u64>>,
    cluster_codes: Vec<Vec<u8>>,
    codebook: Vec<f32>,
    pq_config: PqConfig,
    /// mmap 文件路径（存储全量原始向量，用于重排序）
    /// EN: mmap file path (stores all raw vectors for reranking).
    vecs_path: Option<String>,
}

impl PqFlatClustered {
    pub fn build(
        vectors: &[f32],
        ids: &[u64],
        dims: usize,
        metric: DistanceMetric,
        num_clusters: usize,
        kmeans_iters: usize,
        seed: u64,
    ) -> Self {
        Self::build_with_mmap(
            vectors,
            ids,
            dims,
            metric,
            num_clusters,
            kmeans_iters,
            seed,
            None,
        )
    }

    /// 构建并同时写 mmap 文件。
    // 构造参数即构建配置全集；引入 builder 对单一调用点无收益。
    // EN: Builds and writes the mmap file at the same time.
    // EN: Constructor params are the full build config; a builder adds no value here.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_mmap(
        vectors: &[f32],
        ids: &[u64],
        dims: usize,
        _metric: DistanceMetric,
        num_clusters: usize,
        kmeans_iters: usize,
        seed: u64,
        mmap_path: Option<&str>,
    ) -> Self {
        let n = ids.len();
        let c = num_clusters.min(n).max(2);
        let pq_cfg = PqConfig::for_dims(dims);
        let sub_dims = pq_cfg.sub_dims(dims);
        let m = pq_cfg.m;
        let k = pq_cfg.k;

        // 写 mmap 文件（原始向量，扁平存储）
        // EN: Write the mmap file (raw vectors, flat layout).
        if let Some(path) = mmap_path {
            let mut data = Vec::with_capacity(16 + n * dims * 4);
            data.extend_from_slice(&(n as u64).to_le_bytes());
            data.extend_from_slice(&(dims as u64).to_le_bytes());
            for v in vectors.chunks_exact(dims) {
                for &x in v {
                    data.extend_from_slice(&x.to_le_bytes());
                }
            }
            fs::write(path, &data).expect("write mmap");
        }

        // 1) K-Means
        let km = kmeans::kmeans(vectors, dims, c, kmeans_iters, seed, DistanceMetric::L2);

        // 2) 全局 PQ 码本
        // EN: 2) Global PQ codebook.
        let global_pq = PqIndex::train(vectors, dims, &pq_cfg);
        let codebook = global_pq.codebooks;

        // 3) 分配 + 分组
        // EN: 3) Assign + group.
        let assignments = router::assign_vectors(vectors, dims, &km.centroids, DistanceMetric::L2);
        let groups = router::group_by_cluster(&assignments, c);

        // 4) 每聚类编码
        // EN: 4) Encode each cluster.
        let cluster_data: Vec<(Vec<u64>, Vec<u8>)> = groups
            .par_iter()
            .map(|vec_indices| {
                let mut cids = Vec::with_capacity(vec_indices.len());
                let mut codes = Vec::with_capacity(vec_indices.len() * m);
                for &vi in vec_indices {
                    let v = &vectors[vi * dims..(vi + 1) * dims];
                    cids.push(ids[vi]);
                    for seg in 0..m {
                        let sub = &v[seg * sub_dims..(seg + 1) * sub_dims];
                        let cb_start = seg * k * sub_dims;
                        let best = nearest_centroid_u8(
                            sub,
                            &codebook[cb_start..cb_start + k * sub_dims],
                            sub_dims,
                            k,
                        );
                        codes.push(best);
                    }
                }
                (cids, codes)
            })
            .collect();
        let (cluster_ids, cluster_codes): (Vec<_>, Vec<_>) = cluster_data.into_iter().unzip();

        let cluster_sz = n / c;
        let target = 25_000;
        // 下界不得超过上界：c ∈ {2,3}（小数据集）时 clamp(4, c) 会 panic。
        // EN: lower bound must not exceed upper bound — clamp(4, c) panics for c < 4.
        let nprobe = (target / cluster_sz.max(1)).clamp(4.min(c), c);

        PqFlatClustered {
            dims,
            num_clusters: c,
            nprobe,
            centroids: km.centroids,
            cluster_ids,
            cluster_codes,
            codebook,
            pq_config: pq_cfg,
            vecs_path: mmap_path.map(|s| s.to_string()),
        }
    }

    /// 两阶段检索: PQ 粗筛 → 从 mmap 读原始向量精确重排。
    /// EN: Two-stage search: PQ coarse filter → read raw vectors from mmap and rerank exactly.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        if query.len() != self.dims {
            return Vec::new();
        }
        let k = k.max(1);
        let m = self.pq_config.m;
        let cb_k = self.pq_config.k;
        let sub_dims = self.pq_config.sub_dims(self.dims);

        let total = self.total_vectors();
        let cluster_sz = (total / self.num_clusters).max(1);
        let target = k.saturating_mul(2500);
        let nprobe = self.nprobe.min((target / cluster_sz).max(4));

        let top_clusters = router::route_query(
            query,
            &self.centroids,
            self.dims,
            nprobe,
            DistanceMetric::L2,
        );

        // Phase 1: PQ 查表粗筛
        // EN: Phase 1: PQ table-lookup coarse filter.
        let mut dist_table = vec![0.0f32; m * cb_k];
        for seg in 0..m {
            let q_sub = &query[seg * sub_dims..(seg + 1) * sub_dims];
            for c in 0..cb_k {
                let start = seg * cb_k * sub_dims + c * sub_dims;
                dist_table[seg * cb_k + c] =
                    l2_dist(q_sub, &self.codebook[start..start + sub_dims], sub_dims);
            }
        }

        let coarse_k = k.saturating_mul(20).max(500); // PQ粗筛候选数, 调此参数平衡 recall/latency
        let mut candidates: Vec<(u64, f32)> = top_clusters
            .par_iter()
            .flat_map(|&cid| {
                let codes = &self.cluster_codes[cid];
                let ids = &self.cluster_ids[cid];
                let n = codes.len() / m;
                if n == 0 {
                    return Vec::new();
                }
                let mut local: Vec<(u64, f32)> = (0..n)
                    .map(|i| {
                        let mut d = 0.0;
                        for seg in 0..m {
                            d += dist_table[seg * cb_k + codes[i * m + seg] as usize];
                        }
                        (ids[i], d)
                    })
                    .collect();
                local.sort_by(|a, b| a.1.total_cmp(&b.1));
                local.truncate(coarse_k);
                local
            })
            .collect();
        candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
        candidates.dedup_by_key(|(id, _)| *id);
        candidates.truncate(coarse_k);

        // Phase 2: 从 mmap 读原始向量，精确重排
        // EN: Phase 2: read raw vectors from mmap and rerank exactly.
        if let Some(ref path) = self.vecs_path {
            let mut file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    candidates.truncate(k);
                    return candidates;
                }
            };
            // 读取 header: [n:u64][dims:u64]
            // EN: Read header: [n:u64][dims:u64].
            let mut h = [0u8; 16];
            file.read_exact(&mut h).unwrap();
            let file_n = u64::from_le_bytes(h[0..8].try_into().unwrap()) as usize;
            let file_dims = u64::from_le_bytes(h[8..16].try_into().unwrap()) as usize;
            assert_eq!(file_dims, self.dims);

            let mut reranked: Vec<(u64, f32)> = candidates
                .iter()
                .filter_map(|(id, _)| {
                    // 需要根据 id 找到在原始文件中的位置
                    // id → 原始索引: 简单情况 id = 原始索引
                    // EN: Need to locate the id in the raw file.
                    // EN: id → raw index: in the simple case id = raw index.
                    let idx = *id as usize;
                    if idx >= file_n {
                        return None;
                    }
                    let offset = 16 + idx * self.dims * 4;
                    let mut buf = vec![0u8; self.dims * 4];
                    file.read_exact_at(&mut buf, offset as u64).ok()?;
                    let mut v = vec![0.0f32; self.dims];
                    for i in 0..self.dims {
                        v[i] = f32::from_le_bytes(buf[i * 4..(i + 1) * 4].try_into().unwrap());
                    }
                    let d = l2_dist(query, &v, self.dims);
                    Some((*id, d))
                })
                .collect();

            reranked.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            reranked.truncate(k);
            return reranked;
        }

        // 无 mmap 回退: 纯 PQ 结果
        // EN: No-mmap fallback: pure PQ results.
        candidates.truncate(k);
        candidates
    }

    pub fn total_vectors(&self) -> usize {
        self.cluster_ids.iter().map(|v| v.len()).sum()
    }
    pub fn memory_mb(&self) -> f64 {
        let cb = self.codebook.len() * 4;
        let ib: usize = self.cluster_ids.iter().map(|v| v.len() * 8).sum();
        let cob: usize = self.cluster_codes.iter().map(|c| c.len()).sum();
        (cb + ib + cob) as f64 / 1024.0 / 1024.0
    }
    pub fn disk_mb(&self) -> f64 {
        self.vecs_path
            .as_ref()
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0)
    }
}

#[inline]
fn l2_dist(a: &[f32], b: &[f32], len: usize) -> f32 {
    let mut s = 0.0f32;
    for i in 0..len {
        let d = a[i] - b[i];
        s += d * d;
    }
    s
}

fn nearest_centroid_u8(sub: &[f32], cb: &[f32], sub_dims: usize, k: usize) -> u8 {
    let mut best = 0u8;
    let mut best_d = f32::MAX;
    for c in 0..k {
        let d = l2_dist(sub, &cb[c * sub_dims..(c + 1) * sub_dims], sub_dims);
        if d < best_d {
            best_d = d;
            best = c as u8;
        }
    }
    best
}

// read_exact_at 兼容垫片。
// EN: read_exact_at polyfill.
trait ReadAt {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;
}
impl ReadAt for fs::File {
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::io::Seek;
        self.seek(std::io::SeekFrom::Start(offset))?;
        self.read_exact(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DistanceMetric;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    /// 回归：小数据集（c < 4）时 nprobe 的 clamp 下界不得超过上界，否则 build panic。
    /// EN: regression — for c < 4 the nprobe clamp lower bound must not exceed
    /// the upper bound (pre-existing panic on small collections).
    #[test]
    fn test_pq_flat_small_collection_no_clamp_panic() {
        let dims = 64;
        let n = 6;
        let mut rng = StdRng::seed_from_u64(7);
        let mut vecs = Vec::with_capacity(n * dims);
        let ids: Vec<u64> = (0..n as u64).collect();
        for _ in 0..n * dims {
            vecs.push(rng.gen::<f32>());
        }
        let pqf = PqFlatClustered::build(&vecs, &ids, dims, DistanceMetric::L2, 3, 5, 0);
        assert_eq!(pqf.total_vectors(), n);
    }

    #[test]
    fn test_pq_mmap_search() {
        let dims = 64;
        let n = 200;
        let mut rng = StdRng::seed_from_u64(42);
        let mut vecs = Vec::with_capacity(n * dims);
        let ids: Vec<u64> = (0..n as u64).collect();
        for _ in 0..n {
            for _ in 0..dims {
                vecs.push(rng.gen::<f32>());
            }
        }
        let c = (n as f64).sqrt().ceil() as usize;
        let tmp = std::env::temp_dir().join(format!("pq_test_{}.bin", std::process::id()));
        let pqf = PqFlatClustered::build_with_mmap(
            &vecs,
            &ids,
            dims,
            DistanceMetric::L2,
            c,
            10,
            0,
            Some(tmp.to_str().unwrap()),
        );
        assert_eq!(pqf.total_vectors(), n);
        assert!(pqf.memory_mb() < 2.0);
        assert!(pqf.disk_mb() > 0.0);

        let mut found = 0;
        for i in 0..20 {
            let q = &vecs[i * dims..(i + 1) * dims];
            let r = pqf.search(q, 1);
            if r.first().map(|x| x.0) == Some(ids[i]) {
                found += 1;
            }
        }
        assert!(found as f32 / 20.0 > 0.5);
        let _ = fs::remove_file(tmp);
    }
}
