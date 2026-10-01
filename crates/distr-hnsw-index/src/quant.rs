//! Per-vector symmetric int8 scalar quantization (contract §7). Each stored
//! vector keeps its `f32` original elsewhere; this module holds the int8
//! copy the graph searches plus the two scalars needed to turn an int8 dot
//! product back into a distance: the quantization scale and the original
//! squared norm.

use crate::Metric;

const LANES: usize = 16;

/// Quantized vectors of one dimensionality, row-major, with per-row scale
/// and squared norm. Row `i` corresponds to slot `i`.
#[derive(Clone, Debug, Default)]
pub struct QuantizedVectors {
    dims: usize,
    data: Vec<i8>,
    scales: Vec<f32>,
    norms_sq: Vec<f32>,
}

/// A quantized query, computed once per search.
#[derive(Clone, Debug)]
pub struct QuantizedQuery {
    pub codes: Vec<i8>,
    pub scale: f32,
    pub norm_sq: f32,
}

/// Quantize one vector: `scale = max|x| / 127`, `q = round(x / scale)`.
pub fn quantize(vector: &[f32]) -> QuantizedQuery {
    let max = vector.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
    let norm_sq = vector.iter().map(|x| x * x).sum();
    if max == 0.0 {
        return QuantizedQuery {
            codes: vec![0; vector.len()],
            scale: 0.0,
            norm_sq,
        };
    }
    let scale = max / 127.0;
    let inverse = 1.0 / scale;
    let codes = vector
        .iter()
        .map(|x| (x * inverse).round().clamp(-127.0, 127.0) as i8)
        .collect();
    QuantizedQuery {
        codes,
        scale,
        norm_sq,
    }
}

/// Integer dot product with `i32` accumulation; autovectorizes on stable.
#[inline]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    let chunks = a.len() / LANES * LANES;
    let mut acc = [0_i32; LANES];
    let (head_a, tail_a) = a.split_at(chunks);
    let (head_b, tail_b) = b.split_at(chunks);
    for (x, y) in head_a.chunks_exact(LANES).zip(head_b.chunks_exact(LANES)) {
        for lane in 0..LANES {
            acc[lane] += i32::from(x[lane]) * i32::from(y[lane]);
        }
    }
    let mut sum: i32 = acc.iter().sum();
    for (x, y) in tail_a.iter().zip(tail_b) {
        sum += i32::from(*x) * i32::from(*y);
    }
    sum
}

/// Approximate distance between a quantized query and stored row `slot`.
#[inline]
pub fn distance_i8(
    metric: Metric,
    query: &QuantizedQuery,
    codes: &[i8],
    scale: f32,
    norm_sq: f32,
) -> f32 {
    let dot = query.scale * scale * dot_i8(&query.codes, codes) as f32;
    match metric {
        Metric::Cosine => 1.0 - dot,
        Metric::Dot => -dot,
        Metric::L2 => (query.norm_sq + norm_sq - 2.0 * dot).max(0.0),
    }
}

impl QuantizedVectors {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            data: Vec::new(),
            scales: Vec::new(),
            norms_sq: Vec::new(),
        }
    }

    pub fn with_capacity(dims: usize, rows: usize) -> Self {
        Self {
            dims,
            data: Vec::with_capacity(dims * rows),
            scales: Vec::with_capacity(rows),
            norms_sq: Vec::with_capacity(rows),
        }
    }

    pub fn len(&self) -> usize {
        self.scales.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scales.is_empty()
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Append the quantized form of `vector`; the caller guarantees the row
    /// index matches the slot it belongs to.
    pub fn push(&mut self, vector: &[f32]) {
        debug_assert_eq!(vector.len(), self.dims);
        self.push_quantized(quantize(vector));
    }

    pub fn push_quantized(&mut self, q: QuantizedQuery) {
        debug_assert_eq!(q.codes.len(), self.dims);
        self.data.extend_from_slice(&q.codes);
        self.scales.push(q.scale);
        self.norms_sq.push(q.norm_sq);
    }

    #[inline]
    pub fn distance(&self, metric: Metric, query: &QuantizedQuery, slot: usize) -> f32 {
        let start = slot * self.dims;
        distance_i8(
            metric,
            query,
            &self.data[start..start + self.dims],
            self.scales[slot],
            self.norms_sq[slot],
        )
    }

    /// Bytes held per vector: codes plus the two scalars.
    pub fn bytes_per_vector(&self) -> usize {
        self.dims + 8
    }

    pub fn accounted_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 4 + self.norms_sq.len() * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn quantization_error_is_bounded_by_half_a_step(
            v in prop::collection::vec(-10.0_f32..10.0, 1..300),
        ) {
            let q = quantize(&v);
            prop_assert_eq!(q.codes.len(), v.len());
            for (x, c) in v.iter().zip(&q.codes) {
                let back = f32::from(*c) * q.scale;
                prop_assert!((x - back).abs() <= q.scale / 2.0 + 1e-6, "{x} vs {back}");
            }
        }

        #[test]
        fn int8_distances_track_f32_within_quantization_error(
            a in prop::collection::vec(-1.0_f32..1.0, 8..256),
            b in prop::collection::vec(-1.0_f32..1.0, 8..256),
        ) {
            let n = a.len().min(b.len());
            let (a, b) = (&a[..n], &b[..n]);
            let qa = quantize(a);
            let qb = quantize(b);
            for metric in [Metric::Dot, Metric::L2] {
                let exact = distance::distance(metric, a, b);
                let approx = distance_i8(metric, &qa, &qb.codes, qb.scale, qb.norm_sq);
                // Each product carries at most (scale_a + scale_b)/2 error
                // against unit-bounded inputs; sum over dims.
                let bound = (qa.scale + qb.scale) * n as f32 * 1.5 + 1e-3;
                prop_assert!((exact - approx).abs() <= bound, "{metric:?}: {exact} vs {approx} (bound {bound})");
            }
        }
    }

    #[test]
    fn zero_vectors_quantize_to_zero() {
        let q = quantize(&[0.0, 0.0, 0.0]);
        assert_eq!(q.codes, vec![0, 0, 0]);
        assert_eq!(q.scale, 0.0);
        assert_eq!(distance_i8(Metric::Cosine, &q, &[5, 5, 5], 0.1, 1.0), 1.0);
    }

    #[test]
    fn stored_rows_round_trip_through_distance() {
        let mut store = QuantizedVectors::new(4);
        store.push(&[1.0, 0.0, 0.0, 0.0]);
        store.push(&[0.0, 1.0, 0.0, 0.0]);
        let q = quantize(&[1.0, 0.0, 0.0, 0.0]);
        assert!((store.distance(Metric::Cosine, &q, 0)).abs() < 1e-6);
        assert!((store.distance(Metric::Cosine, &q, 1) - 1.0).abs() < 1e-6);
        assert!((store.distance(Metric::L2, &q, 1) - 2.0).abs() < 1e-5);
        assert_eq!(store.bytes_per_vector(), 12);
        assert_eq!(store.accounted_bytes(), 8 + 16);
    }
}
