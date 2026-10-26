//! Recall 门禁共享模块：tests/recall_gate.rs 与 benches/vector_baseline.rs 经 `#[path]`
//! 各自引入同一份实现，避免 CI 强制指标（数据集生成 / brute-force 对照 / 阈值）双写漂移。
//!
//! Shared recall-gate helpers, included via `#[path]` from both the integration test and
//! the criterion bench so the CI-enforced metric and threshold have a single source.

/// CLAUDE.md 约定 Recall-Check 门槛 ≥95%。注意：k=10 时 recall 以 0.1 为步进量化，
/// `recall >= 0.95` 等效于满分门禁（漏 1 个邻居即 0.9 不合格）——按项目 95%-99% 标准，
/// 0.9 本就算回归，故保持 k=10 与 bench 配置对齐、不追求字面余量。六组实测均为 1.0。
/// EN: The project mandates recall >= 95%. Note: with k=10 recall is quantized to 0.1
/// steps, so this bound is effectively a perfect-recall gate (one miss = 0.9 = fail).
/// That is intentional: by the project's own 95-99% standard, 0.9 IS a regression.
pub const RECALL_MIN: f32 = 0.95;

pub fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

pub fn make_vector(id: u64, dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|i| ((id as usize * 31 + i * 17) % 997) as f32 / 997.0)
        .collect()
}

pub fn brute_force_topk(data: &[(u64, Vec<f32>)], query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<(u64, f32)> = data
        .iter()
        .map(|(id, v)| (*id, l2_distance(query, v)))
        .collect();
    scored.sort_by(|(id_a, d_a), (id_b, d_b)| d_a.total_cmp(d_b).then(id_a.cmp(id_b)));
    scored.into_iter().take(k).map(|(id, _)| id).collect()
}

pub fn recall_at_k(pred: &[u64], truth: &[u64], k: usize) -> f32 {
    if k == 0 {
        return 0.0;
    }
    let hit = pred
        .iter()
        .take(k)
        .filter(|id| truth.iter().take(k).any(|x| x == *id))
        .count();
    hit as f32 / k as f32
}
