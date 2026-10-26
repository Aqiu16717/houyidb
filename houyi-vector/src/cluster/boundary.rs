//! 边界点检测与跨聚类复制。
//! Boundary detection and cross-cluster replication.
//!
//! 当向量到次近中心的距离 ≤ 到最近中心 × α 时，判定为边界点，复制到次近聚类。
//! 这消除了聚类方法的主要 recall 损失来源（边界效应）。

use crate::distance;
use crate::DistanceMetric;
use rayon::prelude::*;

/// 边界分配结果：每个向量的主聚类 + 可选次要聚类列表。
/// EN: Boundary assignment: each vector's primary cluster + optional secondary cluster list.
#[derive(Debug, Clone)]
pub struct BoundaryAssignment {
    /// 主聚类
    /// EN: Primary cluster.
    pub primary: usize,
    /// 因边界效应而复制的次要聚类（可为空）
    /// EN: Secondary clusters copied due to the boundary effect (may be empty).
    pub secondaries: Vec<usize>,
}

/// 并行检测边界点并分配聚类（含复制）。
/// EN: Detects boundary points and assigns clusters in parallel (with replication).
///
/// - `vectors`: 平坦向量 [N × dims]
///   EN: - `vectors`: flat vectors [N × dims]
/// - `centroids`: 聚类中心 [C × dims]
///   EN: - `centroids`: cluster centroids [C × dims]
/// - `alpha`: 边界阈值（默认 1.2）。α 越小，复制越少，recall 越低。
///   EN: - `alpha`: boundary threshold (default 1.2). Smaller α means fewer copies, lower recall.
///
/// 返回每个向量的 `BoundaryAssignment`。
/// EN: Returns a `BoundaryAssignment` for each vector.
pub fn assign_with_boundary(
    vectors: &[f32],
    dims: usize,
    centroids: &[f32],
    alpha: f32,
    metric: DistanceMetric,
) -> Vec<BoundaryAssignment> {
    let n = vectors.len() / dims;
    let c = centroids.len() / dims;

    (0..n)
        .into_par_iter()
        .map(|i| {
            let v = &vectors[i * dims..(i + 1) * dims];

            // 找最近和次近中心
            // EN: Find the nearest and second-nearest centroids.
            let (primary, secondary) = find_two_nearest(v, centroids, dims, c, metric);

            let secondaries = if let Some(sec) = secondary {
                let d_primary =
                    dist_fn(metric, v, &centroids[primary * dims..(primary + 1) * dims]);
                let d_sec = dist_fn(metric, v, &centroids[sec * dims..(sec + 1) * dims]);

                // 边界条件：次近距离 ≤ 最近距离 × α
                // EN: Boundary condition: second-nearest distance ≤ nearest distance × α.
                if alpha > 0.0 && d_sec <= d_primary * alpha {
                    vec![sec]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            BoundaryAssignment {
                primary,
                secondaries,
            }
        })
        .collect()
}

/// 找到最近中心（返回 index）和次近中心（可选，单聚类时为 None）。
/// EN: Finds the nearest centroid (returns its index) and second-nearest (None with one cluster).
pub fn find_two_nearest(
    v: &[f32],
    centroids: &[f32],
    dims: usize,
    num_clusters: usize,
    metric: DistanceMetric,
) -> (usize, Option<usize>) {
    if num_clusters == 1 {
        return (0, None);
    }

    let mut best = 0;
    let mut best_d = dist_fn(metric, v, &centroids[0..dims]);
    let mut second = 1;
    let mut second_d = dist_fn(metric, v, &centroids[dims..2 * dims]);

    if second_d < best_d {
        std::mem::swap(&mut best, &mut second);
        std::mem::swap(&mut best_d, &mut second_d);
    }

    for k in 2..num_clusters {
        let d = dist_fn(metric, v, &centroids[k * dims..(k + 1) * dims]);
        if d < best_d {
            second = best;
            second_d = best_d;
            best = k;
            best_d = d;
        } else if d < second_d {
            second = k;
            second_d = d;
        }
    }

    (best, Some(second))
}

/// 按边界分配结果分组向量索引。返回 `clusters[c] = Vec<(向量索引, is_primary)>`。
/// `is_primary`: true = 主聚类, false = 因边界复制而来。
/// EN: Groups vector indices by boundary assignments. Returns `clusters[c] = Vec<(index, is_primary)>`.
/// EN: `is_primary`: true = primary cluster, false = added by boundary replication.
pub fn group_with_boundary(
    assignments: &[BoundaryAssignment],
    num_clusters: usize,
) -> Vec<Vec<(usize, bool)>> {
    let mut clusters: Vec<Vec<(usize, bool)>> = (0..num_clusters).map(|_| Vec::new()).collect();
    for (vec_idx, a) in assignments.iter().enumerate() {
        clusters[a.primary].push((vec_idx, true));
        for &sec in &a.secondaries {
            clusters[sec].push((vec_idx, false));
        }
    }
    clusters
}

/// 统计因边界复制导致的额外存储开销。
/// EN: Counts the extra storage cost caused by boundary replication.
pub fn replication_overhead(assignments: &[BoundaryAssignment]) -> f64 {
    let primary_count = assignments.len();
    let total_copies: usize = assignments.iter().map(|a| 1 + a.secondaries.len()).sum();
    (total_copies - primary_count) as f64 / primary_count as f64
}

#[inline]
fn dist_fn(metric: DistanceMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        DistanceMetric::L2 => distance::l2_squared(a, b),
        DistanceMetric::Cosine => distance::cosine(a, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DistanceMetric;

    #[test]
    fn test_boundary_detection_near_edge() {
        let dims = 2;
        // 两个聚类中心分别在 (0,0) 和 (10,10)
        // EN: Two centroids at (0,0) and (10,10).
        let centroids = vec![0.0_f32, 0.0, 10.0, 10.0];
        // 向量在正中间: (5,5) → 距离两个中心相等，应被判定为边界点
        // EN: Vector in the middle: (5,5) → equidistant to both centroids, so it is a boundary point.
        let vectors = vec![5.0_f32, 5.0];
        let assignments = assign_with_boundary(&vectors, dims, &centroids, 1.2, DistanceMetric::L2);

        assert_eq!(assignments.len(), 1);
        // 应该在两个聚类之间
        // EN: It should appear in both clusters.
        assert!(!assignments[0].secondaries.is_empty());
    }

    #[test]
    fn test_boundary_detection_far_from_edge() {
        let dims = 2;
        let centroids = vec![0.0_f32, 0.0, 10.0, 10.0];
        // 向量紧邻第一个中心: (1,1) → 距离 (0,0)=√2 ≈1.4, 距离 (10,10)=√162 ≈12.7
        // EN: Vector next to the first centroid: (1,1) → dist to (0,0)=√2 ≈1.4, to (10,10)=√162 ≈12.7.
        let vectors = vec![1.0_f32, 1.0];
        let assignments = assign_with_boundary(&vectors, dims, &centroids, 1.2, DistanceMetric::L2);

        assert_eq!(assignments.len(), 1);
        // 不应该有次要聚类
        // EN: It should have no secondary clusters.
        assert!(assignments[0].secondaries.is_empty());
    }

    #[test]
    fn test_replication_overhead_statistic() {
        let assignments = vec![
            BoundaryAssignment {
                primary: 0,
                secondaries: vec![],
            },
            BoundaryAssignment {
                primary: 0,
                secondaries: vec![1],
            },
            BoundaryAssignment {
                primary: 1,
                secondaries: vec![],
            },
            BoundaryAssignment {
                primary: 1,
                secondaries: vec![],
            },
        ];
        // 4 个主分配 + 1 个额外复制 = 总共 5 份 → 25% 开销
        // EN: 4 primary assignments + 1 extra copy = 5 total → 25% overhead.
        let overhead = replication_overhead(&assignments);
        assert!((overhead - 0.25).abs() < 0.01);
    }
}
