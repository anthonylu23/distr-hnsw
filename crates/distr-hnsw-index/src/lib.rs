//! Single-partition vector engine for distr-hnsw (roadmap M3).
//!
//! Pass 1 provides the pieces everything else is measured against: flat
//! vector storage, distance kernels with scalar references, and the exact
//! oracle whose deterministic tie rules define the engine's semantics
//! (`docs/m3-implementation-plan.md`). No networking, no async runtime.

pub mod distance;
pub mod hnsw;
pub mod oracle;
pub mod partition;
pub mod quant;
pub mod snapshot;
pub mod vector;
pub mod wal;

/// Record identifier inside a partition. External keys map to these in the
/// partition's metadata; the engine only ever orders and compares by `u64`.
pub type RecordId = u64;

/// Distance metric of a collection. Cosine is computed as one minus the dot
/// product of unit-normalized vectors; callers normalize on insert and query
/// so the hot loop is a plain dot product.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Metric {
    Cosine,
    Dot,
    L2,
}

impl Metric {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cosine" | "angular" => Some(Self::Cosine),
            "dot" | "ip" => Some(Self::Dot),
            "l2" | "euclidean" => Some(Self::L2),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::Dot => "dot",
            Self::L2 => "l2",
        }
    }

    /// Whether vectors must be unit-normalized before storage and query.
    pub const fn normalizes(self) -> bool {
        matches!(self, Self::Cosine)
    }
}

/// One search hit. Ordering is the engine-wide tie rule: smaller distance
/// first, then smaller id, so results are deterministic and comparable to
/// the oracle bit for bit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub id: RecordId,
    pub distance: f32,
}

impl Hit {
    /// Total order used everywhere: distance ascending, then id ascending.
    /// NaN distances sort last so a corrupt vector can never win.
    pub fn cmp_rank(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            .partial_cmp(&other.distance)
            .unwrap_or_else(|| match (self.distance.is_nan(), other.distance.is_nan()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            })
            .then(self.id.cmp(&other.id))
    }
}
