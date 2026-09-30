//! Exact nearest-neighbour search. This is the reference every approximate
//! path is measured against and the definition of result order: distance
//! ascending, then id ascending, tombstoned or filtered ids never returned.

use std::{cmp::Ordering, collections::BinaryHeap};

use rayon::prelude::*;

use crate::{distance::distance, vector::FlatVectors, Hit, Metric, RecordId};

/// Wrapper giving [`Hit`] the reverse ordering a max-heap needs to keep the
/// k best (smallest) hits.
#[derive(Clone, Copy, Debug)]
struct HeapHit(Hit);

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.0.cmp_rank(&other.0) == Ordering::Equal
    }
}
impl Eq for HeapHit {}
impl PartialOrd for HeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapHit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp_rank(&other.0)
    }
}

/// Bounded collector of the k best hits with the engine-wide tie rule.
#[derive(Clone, Debug)]
pub struct TopK {
    k: usize,
    heap: BinaryHeap<HeapHit>,
}

impl TopK {
    pub fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k + 1),
        }
    }

    /// The worst distance currently kept, if the collector is full.
    #[inline]
    pub fn threshold(&self) -> Option<Hit> {
        (self.heap.len() >= self.k)
            .then(|| self.heap.peek().map(|hit| hit.0))
            .flatten()
    }

    #[inline]
    pub fn push(&mut self, hit: Hit) {
        if self.k == 0 {
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(HeapHit(hit));
            return;
        }
        if let Some(worst) = self.heap.peek() {
            if hit.cmp_rank(&worst.0) == Ordering::Less {
                self.heap.pop();
                self.heap.push(HeapHit(hit));
            }
        }
    }

    /// Hits in rank order.
    pub fn into_sorted(self) -> Vec<Hit> {
        let mut hits: Vec<Hit> = self.heap.into_iter().map(|hit| hit.0).collect();
        hits.sort_by(Hit::cmp_rank);
        hits
    }
}

/// Exact top-k over every stored row that `allow` accepts. The query must
/// already be normalized for cosine collections.
pub fn search(
    vectors: &FlatVectors,
    metric: Metric,
    query: &[f32],
    k: usize,
    allow: impl Fn(RecordId) -> bool,
) -> Vec<Hit> {
    let mut top = TopK::new(k);
    for (id, row) in vectors.rows() {
        if !allow(id) {
            continue;
        }
        top.push(Hit {
            id,
            distance: distance(metric, query, row),
        });
    }
    top.into_sorted()
}

/// Exact top-k for many queries in parallel; row order matches `queries`.
pub fn search_batch(
    vectors: &FlatVectors,
    metric: Metric,
    queries: &FlatVectors,
    k: usize,
) -> Vec<Vec<Hit>> {
    (0..queries.len())
        .into_par_iter()
        .map(|index| {
            let query = queries.get(index as RecordId).expect("query row in range");
            search(vectors, metric, query, k, |_| true)
        })
        .collect()
}

/// Recall@k by distance: the fraction of returned hits whose distance is no
/// worse than the k-th true nearest distance (with a small relative
/// tolerance). This is the measure the benchmark and the acceptance
/// thresholds use, because public datasets contain exact-duplicate vectors
/// and zero vectors whose ties make id-based recall undercount even exact
/// search.
pub fn recall_by_distance(kth_true_distance: f32, actual: &[Hit], k: usize) -> f64 {
    if k == 0 {
        return 1.0;
    }
    let tolerance = 1e-5_f32 * kth_true_distance.abs().max(1e-3);
    let found = actual
        .iter()
        .take(k)
        .filter(|hit| hit.distance <= kth_true_distance + tolerance)
        .count();
    found as f64 / k as f64
}

/// Fraction of `expected` ids present in `actual`. Exact on sets without
/// duplicates; reported alongside distance recall for diagnosis.
pub fn recall(expected: &[RecordId], actual: &[Hit]) -> f64 {
    if expected.is_empty() {
        return 1.0;
    }
    let found = expected
        .iter()
        .filter(|id| actual.iter().any(|hit| hit.id == **id))
        .count();
    found as f64 / expected.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ties_break_by_id_and_filters_apply() {
        let vectors =
            FlatVectors::from_vec(2, vec![1.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0], Metric::L2)
                .unwrap();
        let hits = search(&vectors, Metric::L2, &[1.0, 0.0], 3, |_| true);
        assert_eq!(
            hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![0, 1, 3]
        );
        let filtered = search(&vectors, Metric::L2, &[1.0, 0.0], 3, |id| id != 0);
        assert_eq!(
            filtered.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
        assert_eq!(recall(&[0, 1, 3], &hits), 1.0);
        assert_eq!(recall(&[0, 1, 3], &filtered), 2.0 / 3.0);
        // Rows 0, 1, 3 are exact duplicates: any three of them are correct
        // by distance even when the ids differ from the reference.
        assert_eq!(recall_by_distance(0.0, &hits, 3), 1.0);
        assert_eq!(recall_by_distance(0.0, &filtered, 3), 2.0 / 3.0);
    }

    #[test]
    fn topk_keeps_the_k_smallest() {
        let mut top = TopK::new(2);
        for (id, distance) in [(5, 0.9), (1, 0.1), (2, 0.5), (3, 0.05)] {
            top.push(Hit { id, distance });
        }
        let hits = top.into_sorted();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![3, 1]);
        assert!(TopK::new(0).into_sorted().is_empty());
    }
}
