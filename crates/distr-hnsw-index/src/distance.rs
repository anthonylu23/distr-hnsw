//! Distance kernels. Each kernel has a plain scalar reference (`*_scalar`)
//! and an unrolled implementation the compiler autovectorizes on stable Rust;
//! the two are property-tested against each other. Architecture intrinsics
//! can replace the unrolled bodies later without touching callers.

use crate::Metric;

const LANES: usize = 8;

/// Distance for `metric` between two vectors of equal length. Cosine assumes
/// both inputs are unit-normalized (see [`normalize`]).
#[inline]
pub fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - dot(a, b),
        Metric::Dot => -dot(a, b),
        Metric::L2 => l2_squared(a, b),
    }
}

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let chunks = a.len() / LANES * LANES;
    let mut acc = [0.0_f32; LANES];
    let (head_a, tail_a) = a.split_at(chunks);
    let (head_b, tail_b) = b.split_at(chunks);
    for (x, y) in head_a.chunks_exact(LANES).zip(head_b.chunks_exact(LANES)) {
        for lane in 0..LANES {
            acc[lane] += x[lane] * y[lane];
        }
    }
    let mut sum = acc.iter().sum::<f32>();
    for (x, y) in tail_a.iter().zip(tail_b) {
        sum += x * y;
    }
    sum
}

#[inline]
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let chunks = a.len() / LANES * LANES;
    let mut acc = [0.0_f32; LANES];
    let (head_a, tail_a) = a.split_at(chunks);
    let (head_b, tail_b) = b.split_at(chunks);
    for (x, y) in head_a.chunks_exact(LANES).zip(head_b.chunks_exact(LANES)) {
        for lane in 0..LANES {
            let d = x[lane] - y[lane];
            acc[lane] += d * d;
        }
    }
    let mut sum = acc.iter().sum::<f32>();
    for (x, y) in tail_a.iter().zip(tail_b) {
        let d = x - y;
        sum += d * d;
    }
    sum
}

/// Scalar references used only by tests and as the documented definition.
pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn l2_squared_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Unit-normalize in place. A zero vector is left unchanged and reported.
pub fn normalize(vector: &mut [f32]) -> bool {
    let norm = dot(vector, vector).sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return false;
    }
    let inverse = 1.0 / norm;
    for value in vector {
        *value *= inverse;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn close(a: f32, b: f32, scale: f32) -> bool {
        (a - b).abs() <= 1e-4 * scale.max(1.0)
    }

    proptest! {
        #[test]
        fn unrolled_kernels_match_scalar(
            a in prop::collection::vec(-100.0_f32..100.0, 0..300),
            b in prop::collection::vec(-100.0_f32..100.0, 0..300),
        ) {
            let n = a.len().min(b.len());
            let (a, b) = (&a[..n], &b[..n]);
            let scale = (a.iter().map(|x| x.abs()).sum::<f32>()
                + b.iter().map(|x| x.abs()).sum::<f32>()).max(1.0) * 100.0;
            prop_assert!(close(dot(a, b), dot_scalar(a, b), scale));
            prop_assert!(close(l2_squared(a, b), l2_squared_scalar(a, b), scale * 4.0));
        }

        #[test]
        fn normalized_vectors_have_unit_norm(
            v in prop::collection::vec(-100.0_f32..100.0, 1..300),
        ) {
            let mut v = v;
            if normalize(&mut v) {
                prop_assert!((dot(&v, &v) - 1.0).abs() < 1e-4);
            } else {
                prop_assert!(v.iter().all(|x| *x == 0.0));
            }
        }
    }

    #[test]
    fn metrics_agree_on_known_values() {
        let a = [1.0, 0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0, 0.0];
        assert_eq!(distance(Metric::Cosine, &a, &b), 1.0);
        assert_eq!(distance(Metric::Cosine, &a, &a), 0.0);
        assert_eq!(distance(Metric::L2, &a, &b), 2.0);
        assert_eq!(distance(Metric::Dot, &a, &a), -1.0);
    }
}
