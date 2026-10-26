//! AArch64 NEON 距离计算：L2²、点积、Cosine（4×`f32` 宽，多累加器流水线）。
//! AArch64 NEON distances: L2², dot, cosine — 4-wide with multi-accumulator software pipelining.
//!
//! Apple M-series: FMLA latency ~4 cycles, throughput 2/cycle → 4-way unrolling hides latency.
//! 苹果 M 系列：FMLA 延迟约 4 周期、吞吐 2/周期 → 4 路展开隐藏延迟。

use std::arch::aarch64::*;

// ── L2 距离平方（4 累加器展开）───────────────────────────────
// EN: ── L2 squared distance (4 accumulators) ──────────────────

/// L2 距离平方，主循环一次处理 16 个 f32（4 组 × 4 宽），4 个独立累加器隐藏 FMA 延迟。
/// L2² with 4-way accumulator unrolling: 16 f32/iteration, 4 independent accumulators.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn l2_squared(a: &[f32], b: &[f32], n: usize) -> f32 {
    let mut i = 0usize;
    let mut acc0 = vdupq_n_f32(0.0);
    let mut acc1 = vdupq_n_f32(0.0);
    let mut acc2 = vdupq_n_f32(0.0);
    let mut acc3 = vdupq_n_f32(0.0);

    // 16 路主循环：4 条独立累加器链。
    // EN: 16-wide main loop: 4 independent accumulator chains.
    while i + 16 <= n {
        let pa = a.as_ptr().add(i);
        let pb = b.as_ptr().add(i);

        let va0 = vld1q_f32(pa);
        let vb0 = vld1q_f32(pb);
        let va1 = vld1q_f32(pa.add(4));
        let vb1 = vld1q_f32(pb.add(4));
        let va2 = vld1q_f32(pa.add(8));
        let vb2 = vld1q_f32(pb.add(8));
        let va3 = vld1q_f32(pa.add(12));
        let vb3 = vld1q_f32(pb.add(12));

        let d0 = vsubq_f32(va0, vb0);
        let d1 = vsubq_f32(va1, vb1);
        let d2 = vsubq_f32(va2, vb2);
        let d3 = vsubq_f32(va3, vb3);

        acc0 = vfmaq_f32(acc0, d0, d0);
        acc1 = vfmaq_f32(acc1, d1, d1);
        acc2 = vfmaq_f32(acc2, d2, d2);
        acc3 = vfmaq_f32(acc3, d3, d3);

        i += 16;
    }

    // 4 路尾部处理。
    // EN: 4-wide tail.
    while i + 4 <= n {
        let va = vld1q_f32(a.as_ptr().add(i));
        let vb = vld1q_f32(b.as_ptr().add(i));
        let d = vsubq_f32(va, vb);
        acc0 = vfmaq_f32(acc0, d, d);
        i += 4;
    }

    // 归约累加器：acc0 + acc1 + acc2 + acc3。
    // EN: Reduce accumulators: acc0 + acc1 + acc2 + acc3.
    let sum012 = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
    let mut sum = vaddvq_f32(sum012);

    // 标量尾部处理。
    // EN: Scalar tail.
    while i < n {
        let d = *a.get_unchecked(i) - *b.get_unchecked(i);
        sum += d * d;
        i += 1;
    }
    sum
}

// ── 点积（4 累加器展开）───────────────────────────────────────
// EN: ── Dot product (4 accumulators) ──────────────────────────

/// 等长点积，主循环 16 f32/迭代，4 累加器。
/// Dot product, 16 f32/iter, 4-way unrolled accumulators.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn dot_f32(a: &[f32], b: &[f32], n: usize) -> f32 {
    let mut i = 0usize;
    let mut acc0 = vdupq_n_f32(0.0);
    let mut acc1 = vdupq_n_f32(0.0);
    let mut acc2 = vdupq_n_f32(0.0);
    let mut acc3 = vdupq_n_f32(0.0);

    while i + 16 <= n {
        let pa = a.as_ptr().add(i);
        let pb = b.as_ptr().add(i);

        let va0 = vld1q_f32(pa);
        let vb0 = vld1q_f32(pb);
        let va1 = vld1q_f32(pa.add(4));
        let vb1 = vld1q_f32(pb.add(4));
        let va2 = vld1q_f32(pa.add(8));
        let vb2 = vld1q_f32(pb.add(8));
        let va3 = vld1q_f32(pa.add(12));
        let vb3 = vld1q_f32(pb.add(12));

        acc0 = vfmaq_f32(acc0, va0, vb0);
        acc1 = vfmaq_f32(acc1, va1, vb1);
        acc2 = vfmaq_f32(acc2, va2, vb2);
        acc3 = vfmaq_f32(acc3, va3, vb3);

        i += 16;
    }

    while i + 4 <= n {
        let va = vld1q_f32(a.as_ptr().add(i));
        let vb = vld1q_f32(b.as_ptr().add(i));
        acc0 = vfmaq_f32(acc0, va, vb);
        i += 4;
    }

    let sum012 = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
    let mut sum = vaddvq_f32(sum012);

    while i < n {
        sum += *a.get_unchecked(i) * *b.get_unchecked(i);
        i += 1;
    }
    sum
}

// ── Cosine 距离（单趟计算 dot + norm_a + norm_b）────────────
// EN: ── Cosine distance (single pass: dot + norm_a + norm_b) ──

/// Cosine 距离 = 1 - dot/(norm_a * norm_b)，单趟 NEON 扫描同时累加 dot、na²、nb²。
/// Cosine distance in one NEON pass: dot, na², nb² accumulated together.
///
/// 使用 3 组 × 4 累加器（12 个）完全隐藏 FMA 延迟。
/// 3×4=12 accumulators to fully hide FMA latency on Apple Silicon.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn cosine(a: &[f32], b: &[f32], n: usize) -> f32 {
    let mut i = 0usize;

    // 点积累加器。
    // EN: Dot accumulators.
    let mut dot0 = vdupq_n_f32(0.0);
    let mut dot1 = vdupq_n_f32(0.0);
    let mut dot2 = vdupq_n_f32(0.0);
    let mut dot3 = vdupq_n_f32(0.0);
    // norm_a 累加器。
    // EN: Norm-a accumulators.
    let mut na0 = vdupq_n_f32(0.0);
    let mut na1 = vdupq_n_f32(0.0);
    let mut na2 = vdupq_n_f32(0.0);
    let mut na3 = vdupq_n_f32(0.0);
    // norm_b 累加器。
    // EN: Norm-b accumulators.
    let mut nb0 = vdupq_n_f32(0.0);
    let mut nb1 = vdupq_n_f32(0.0);
    let mut nb2 = vdupq_n_f32(0.0);
    let mut nb3 = vdupq_n_f32(0.0);

    // 16 路主循环。
    // EN: 16-wide main loop.
    while i + 16 <= n {
        let pa = a.as_ptr().add(i);
        let pb = b.as_ptr().add(i);

        let a0 = vld1q_f32(pa);
        let b0 = vld1q_f32(pb);
        let a1 = vld1q_f32(pa.add(4));
        let b1 = vld1q_f32(pb.add(4));
        let a2 = vld1q_f32(pa.add(8));
        let b2 = vld1q_f32(pb.add(8));
        let a3 = vld1q_f32(pa.add(12));
        let b3 = vld1q_f32(pb.add(12));

        dot0 = vfmaq_f32(dot0, a0, b0);
        na0 = vfmaq_f32(na0, a0, a0);
        nb0 = vfmaq_f32(nb0, b0, b0);

        dot1 = vfmaq_f32(dot1, a1, b1);
        na1 = vfmaq_f32(na1, a1, a1);
        nb1 = vfmaq_f32(nb1, b1, b1);

        dot2 = vfmaq_f32(dot2, a2, b2);
        na2 = vfmaq_f32(na2, a2, a2);
        nb2 = vfmaq_f32(nb2, b2, b2);

        dot3 = vfmaq_f32(dot3, a3, b3);
        na3 = vfmaq_f32(na3, a3, a3);
        nb3 = vfmaq_f32(nb3, b3, b3);

        i += 16;
    }

    // 4 路尾部处理。
    // EN: 4-wide tail.
    while i + 4 <= n {
        let va = vld1q_f32(a.as_ptr().add(i));
        let vb = vld1q_f32(b.as_ptr().add(i));
        dot0 = vfmaq_f32(dot0, va, vb);
        na0 = vfmaq_f32(na0, va, va);
        nb0 = vfmaq_f32(nb0, vb, vb);
        i += 4;
    }

    // Reduce
    let d01 = vaddq_f32(vaddq_f32(dot0, dot1), vaddq_f32(dot2, dot3));
    let n01 = vaddq_f32(vaddq_f32(na0, na1), vaddq_f32(na2, na3));
    let n02 = vaddq_f32(vaddq_f32(nb0, nb1), vaddq_f32(nb2, nb3));

    let mut dot = vaddvq_f32(d01);
    let mut na = vaddvq_f32(n01);
    let mut nb = vaddvq_f32(n02);

    // 标量尾部处理。
    // EN: Scalar tail.
    while i < n {
        let x = *a.get_unchecked(i);
        let y = *b.get_unchecked(i);
        dot += x * y;
        na += x * x;
        nb += y * y;
        i += 1;
    }

    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - (dot / (na.sqrt() * nb.sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与标量参考实现对比，验证 NEON 正确性。
    /// EN: Compares against the scalar reference to verify NEON correctness.
    fn l2_ref(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum()
    }

    fn cosine_ref(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = 0.0;
        let mut na = 0.0;
        let mut nb = 0.0;
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 {
            return 1.0;
        }
        1.0 - (dot / (na.sqrt() * nb.sqrt()))
    }

    #[test]
    fn neon_l2_matches_reference() {
        let dims = [1usize, 4, 7, 15, 16, 32, 63, 128, 1536];
        for &d in &dims {
            let a: Vec<f32> = (0..d).map(|i| (i % 17) as f32 * 0.03).collect();
            let b: Vec<f32> = (0..d).map(|i| (i % 13) as f32 * 0.07).collect();
            let neon = unsafe { l2_squared(&a, &b, d) };
            let ref_ = l2_ref(&a, &b);
            let err = (neon - ref_).abs();
            assert!(
                err < 1e-4 || err / ref_.max(1e-12) < 1e-5,
                "l2 dim={d} neon={neon} ref={ref_} err={err}"
            );
        }
    }

    #[test]
    fn neon_dot_matches_reference() {
        let dims = [1usize, 4, 8, 15, 16, 64, 1536];
        for &d in &dims {
            let a: Vec<f32> = (0..d).map(|i| (i % 7) as f32 * 0.1 - 0.3).collect();
            let b: Vec<f32> = (0..d).map(|i| (i % 5) as f32 * 0.11 - 0.2).collect();
            let neon = unsafe { dot_f32(&a, &b, d) };
            let ref_: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
            let err = (neon - ref_).abs();
            assert!(
                err < 1e-5 || err / ref_.abs().max(1e-6) < 1e-5,
                "dot dim={d} neon={neon} ref={ref_} err={err}"
            );
        }
    }

    #[test]
    fn neon_cosine_matches_reference() {
        let dims = [1usize, 4, 7, 15, 16, 32, 63, 128, 1536];
        for &d in &dims {
            let a: Vec<f32> = (0..d).map(|i| (i % 17) as f32 * 0.03).collect();
            let b: Vec<f32> = (0..d).map(|i| (i % 13) as f32 * 0.07).collect();
            let neon = unsafe { cosine(&a, &b, d) };
            let ref_ = cosine_ref(&a, &b);
            let err = (neon - ref_).abs();
            assert!(
                err < 1e-4 || err / ref_.abs().max(1e-6) < 1e-4,
                "cosine dim={d} neon={neon} ref={ref_} err={err}"
            );
        }
    }
}
