//! Flat, row-major `f32` vector storage: the full-precision originals the
//! engine keeps beside its quantized graph, and the oracle's input.

use thiserror::Error;

use crate::{distance::normalize, Metric, RecordId};

/// Contiguous vectors of one dimensionality. Row `i` holds the vector for
/// [`RecordId`] `i` in dense storage; sparse id maps are the partition's job.
#[derive(Clone, Debug)]
pub struct FlatVectors {
    dims: usize,
    data: Vec<f32>,
}

impl FlatVectors {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            data: Vec::new(),
        }
    }

    pub fn with_capacity(dims: usize, rows: usize) -> Self {
        Self {
            dims,
            data: Vec::with_capacity(dims * rows),
        }
    }

    /// Wrap an existing row-major buffer without copying. `metric` decides
    /// whether rows are normalized in place first.
    pub fn from_vec(dims: usize, mut data: Vec<f32>, metric: Metric) -> Result<Self, VectorError> {
        if dims == 0 {
            return Err(VectorError::ZeroDims);
        }
        if !data.len().is_multiple_of(dims) {
            return Err(VectorError::Ragged {
                len: data.len(),
                dims,
            });
        }
        if metric.normalizes() {
            for row in data.chunks_exact_mut(dims) {
                normalize(row);
            }
        }
        Ok(Self { dims, data })
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        self.data.len() / self.dims
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Append a row and return its id. Normalizes for cosine collections.
    pub fn push(&mut self, vector: &[f32], metric: Metric) -> Result<RecordId, VectorError> {
        if vector.len() != self.dims {
            return Err(VectorError::WrongDims {
                expected: self.dims,
                actual: vector.len(),
            });
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(VectorError::NonFinite);
        }
        let id = self.len() as RecordId;
        self.data.extend_from_slice(vector);
        if metric.normalizes() {
            let start = id as usize * self.dims;
            normalize(&mut self.data[start..start + self.dims]);
        }
        Ok(id)
    }

    #[inline]
    pub fn get(&self, id: RecordId) -> Option<&[f32]> {
        let start = (id as usize).checked_mul(self.dims)?;
        self.data.get(start..start + self.dims)
    }

    pub fn rows(&self) -> impl Iterator<Item = (RecordId, &[f32])> + '_ {
        self.data
            .chunks_exact(self.dims)
            .enumerate()
            .map(|(index, row)| (index as RecordId, row))
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.data
    }
}

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("vector dimensionality must be greater than zero")]
    ZeroDims,
    #[error("buffer of {len} values is not a multiple of {dims} dimensions")]
    Ragged { len: usize, dims: usize },
    #[error("expected {expected} dimensions, got {actual}")]
    WrongDims { expected: usize, actual: usize },
    #[error("vector contains a non-finite value")]
    NonFinite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_get_and_normalize() {
        let mut store = FlatVectors::new(2);
        let id = store.push(&[3.0, 4.0], Metric::Cosine).unwrap();
        assert_eq!(id, 0);
        let row = store.get(0).unwrap();
        assert!((row[0] - 0.6).abs() < 1e-6 && (row[1] - 0.8).abs() < 1e-6);
        assert!(store.get(1).is_none());
        assert!(matches!(
            store.push(&[1.0], Metric::Cosine),
            Err(VectorError::WrongDims { .. })
        ));
        assert!(matches!(
            store.push(&[f32::NAN, 1.0], Metric::L2),
            Err(VectorError::NonFinite)
        ));
        let l2 = FlatVectors::from_vec(2, vec![3.0, 4.0], Metric::L2).unwrap();
        assert_eq!(l2.get(0).unwrap(), &[3.0, 4.0]);
        assert!(matches!(
            FlatVectors::from_vec(2, vec![1.0, 2.0, 3.0], Metric::L2),
            Err(VectorError::Ragged { .. })
        ));
    }
}
