//! Flat, row-major `f32` vector storage: the full-precision originals the
//! engine keeps beside its quantized graph, and the oracle's input.
//!
//! Storage is tiered (contract §7, §11): rows restored from a snapshot are
//! served from a read-only memory map of the snapshot file through the page
//! cache, and rows appended since live in an owned tail. Only the tail counts
//! toward the RAM hot set.

use std::sync::Arc;

use thiserror::Error;

use crate::{distance::normalize, Metric, RecordId};

/// A read-only, 4-byte-aligned region of `f32` rows inside a mapped file.
#[derive(Clone)]
pub struct MappedRows {
    map: Arc<memmap2::Mmap>,
    offset: usize,
    rows: usize,
}

impl MappedRows {
    /// `offset` is the byte offset of the first row inside `map`; the region
    /// must be 4-byte aligned and hold `rows × dims` values.
    pub fn new(
        map: Arc<memmap2::Mmap>,
        offset: usize,
        rows: usize,
        dims: usize,
    ) -> Result<Self, VectorError> {
        let bytes = rows
            .checked_mul(dims)
            .and_then(|v| v.checked_mul(4))
            .ok_or(VectorError::MappedRegionOutOfBounds)?;
        if offset + bytes > map.len() {
            return Err(VectorError::MappedRegionOutOfBounds);
        }
        if !(map.as_ptr() as usize + offset).is_multiple_of(4) {
            return Err(VectorError::MappedRegionMisaligned);
        }
        Ok(Self { map, offset, rows })
    }

    #[inline]
    fn slice(&self, dims: usize) -> &[f32] {
        let bytes = &self.map[self.offset..self.offset + self.rows * dims * 4];
        bytemuck::cast_slice(bytes)
    }
}

impl std::fmt::Debug for MappedRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedRows")
            .field("offset", &self.offset)
            .field("rows", &self.rows)
            .finish()
    }
}

/// Contiguous vectors of one dimensionality. Row `i` holds the vector for
/// [`RecordId`] `i` in dense storage; sparse id maps are the partition's job.
#[derive(Clone, Debug)]
pub struct FlatVectors {
    dims: usize,
    base: Option<MappedRows>,
    tail: Vec<f32>,
}

impl FlatVectors {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            base: None,
            tail: Vec::new(),
        }
    }

    pub fn with_capacity(dims: usize, rows: usize) -> Self {
        Self {
            dims,
            base: None,
            tail: Vec::with_capacity(dims * rows),
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
        Ok(Self {
            dims,
            base: None,
            tail: data,
        })
    }

    /// Serve the first `rows` rows from a memory map; appended rows go to the
    /// tail. Rows are stored exactly as the snapshot holds them (already
    /// normalized for cosine).
    pub fn from_mapped(dims: usize, base: MappedRows) -> Result<Self, VectorError> {
        if dims == 0 {
            return Err(VectorError::ZeroDims);
        }
        Ok(Self {
            dims,
            base: Some(base),
            tail: Vec::new(),
        })
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    fn base_rows(&self) -> usize {
        self.base.as_ref().map_or(0, |b| b.rows)
    }

    pub fn len(&self) -> usize {
        self.base_rows() + self.tail.len() / self.dims
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of vector data held in RAM (the tail); mapped rows are served
    /// from the page cache and are not counted.
    pub fn resident_bytes(&self) -> usize {
        self.tail.len() * 4
    }

    /// Bytes of vector data served from the memory map.
    pub fn mapped_bytes(&self) -> usize {
        self.base_rows() * self.dims * 4
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
        let start = self.tail.len();
        self.tail.extend_from_slice(vector);
        if metric.normalizes() {
            normalize(&mut self.tail[start..start + self.dims]);
        }
        Ok(id)
    }

    #[inline]
    pub fn get(&self, id: RecordId) -> Option<&[f32]> {
        let id = usize::try_from(id).ok()?;
        let base_rows = self.base_rows();
        if id < base_rows {
            let base = self.base.as_ref()?;
            let start = id * self.dims;
            return base.slice(self.dims).get(start..start + self.dims);
        }
        let start = (id - base_rows).checked_mul(self.dims)?;
        self.tail.get(start..start + self.dims)
    }

    pub fn rows(&self) -> impl Iterator<Item = (RecordId, &[f32])> + '_ {
        let base = self
            .base
            .as_ref()
            .map(|b| b.slice(self.dims))
            .unwrap_or(&[]);
        base.chunks_exact(self.dims)
            .chain(self.tail.chunks_exact(self.dims))
            .enumerate()
            .map(|(index, row)| (index as RecordId, row))
    }

    /// Copy every row into one owned buffer (used when writing a snapshot).
    pub fn to_contiguous(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.len() * self.dims);
        if let Some(base) = &self.base {
            out.extend_from_slice(base.slice(self.dims));
        }
        out.extend_from_slice(&self.tail);
        out
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
    #[error("mapped vector region lies outside the file")]
    MappedRegionOutOfBounds,
    #[error("mapped vector region is not 4-byte aligned")]
    MappedRegionMisaligned,
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

    #[test]
    fn mapped_base_and_owned_tail_read_as_one_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rows.bin");
        let rows: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut bytes = vec![0_u8; 64]; // leading padding to test offsets
        bytes.extend(rows.iter().flat_map(|v| v.to_le_bytes()));
        std::fs::write(&path, &bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let map = Arc::new(unsafe { memmap2::Mmap::map(&file).unwrap() });
        let base = MappedRows::new(map.clone(), 64, 3, 2).unwrap();
        let mut store = FlatVectors::from_mapped(2, base).unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.get(2).unwrap(), &[5.0, 6.0]);
        assert_eq!(store.resident_bytes(), 0);
        assert_eq!(store.mapped_bytes(), 24);
        store.push(&[7.0, 8.0], Metric::L2).unwrap();
        assert_eq!(store.len(), 4);
        assert_eq!(store.get(3).unwrap(), &[7.0, 8.0]);
        assert_eq!(store.resident_bytes(), 8);
        assert_eq!(store.rows().count(), 4);
        assert_eq!(
            store.to_contiguous(),
            vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]
        );
        assert!(
            MappedRows::new(map.clone(), 64, 4, 2).is_err(),
            "out of bounds"
        );
        assert!(MappedRows::new(map, 65, 1, 2).is_err(), "misaligned");
    }
}
