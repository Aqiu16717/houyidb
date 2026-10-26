//! L2 平方和与点积的 **x86_64 AVX2 + FMA** 实现，供 `feature = "simd-avx2"` 时运行期探测后调用。
//! AVX2+FMA for L2² and dot; invoked after runtime `is_x86_feature_detected!` when feature enabled.
//!
//! 禁止在无 `target_feature` 的上下文中内联本模块的 `__m256` 计算；由公开函数携带 `avx2,fma`。
//! Do not inline the intrinsics from generic code; keep them behind `target_feature` entry points.

use std::arch::x86_64::*;

/// 8 路 f32 水平求和。
/// EN: Horizontal sum of 8 lanes.
#[inline]
#[target_feature(enable = "avx2", enable = "fma")]
unsafe fn hsum256_ps(v: __m256) -> f32 {
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps(v, 1);
    let s = _mm_add_ps(lo, hi);
    let s = _mm_hadd_ps(s, s);
    let s = _mm_hadd_ps(s, s);
    _mm_cvtss_f32(s)
}

/// L2 距离平方，主循环 8×f32；`a.len()` 与 `b.len()` 已在外层用 `n = min` 约束。
/// Sum of squared differences; caller ensures equal effective length.
#[target_feature(enable = "avx2", enable = "fma")]
pub(crate) unsafe fn l2_squared(a: &[f32], b: &[f32], n: usize) -> f32 {
    let mut i = 0usize;
    let mut acc = _mm256_setzero_ps();
    while i + 8 <= n {
        let va = _mm256_loadu_ps(a.as_ptr().add(i));
        let vb = _mm256_loadu_ps(b.as_ptr().add(i));
        let d = _mm256_sub_ps(va, vb);
        acc = _mm256_fmadd_ps(d, d, acc);
        i += 8;
    }
    let mut sum = hsum256_ps(acc);
    while i < n {
        let d = *a.get_unchecked(i) - *b.get_unchecked(i);
        sum += d * d;
        i += 1;
    }
    sum
}

/// 等长点积，主循环 8×f32。
/// EN: Dot product, 8-wide.
#[target_feature(enable = "avx2", enable = "fma")]
pub(crate) unsafe fn dot_f32(a: &[f32], b: &[f32], n: usize) -> f32 {
    let mut i = 0usize;
    let mut acc = _mm256_setzero_ps();
    while i + 8 <= n {
        let va = _mm256_loadu_ps(a.as_ptr().add(i));
        let vb = _mm256_loadu_ps(b.as_ptr().add(i));
        acc = _mm256_fmadd_ps(va, vb, acc);
        i += 8;
    }
    let mut sum = hsum256_ps(acc);
    while i < n {
        sum += *a.get_unchecked(i) * *b.get_unchecked(i);
        i += 1;
    }
    sum
}
