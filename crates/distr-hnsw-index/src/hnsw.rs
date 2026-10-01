//! In-RAM HNSW graph over full-precision vectors (pass 3 of
//! `docs/m3-implementation-plan.md`; semantics in `docs/m3-engine-contract.md`
//! §1, §3, §8). Slots are append-only; deletes are tombstones that stay
//! linked for routing but are never returned.

use std::{cell::RefCell, cmp::Ordering, collections::BinaryHeap};

use crate::{
    distance::distance,
    oracle::TopK,
    quant::{quantize, QuantizedVectors},
    vector::FlatVectors,
    Hit, Metric,
};

/// Internal slot index (contract §1).
pub type Slot = u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HnswParams {
    /// Max neighbours per node on upper levels.
    pub m: usize,
    /// Max neighbours on level 0.
    pub m0: usize,
    pub ef_construction: usize,
    /// Seed mixed with the slot to draw levels deterministically.
    pub seed: u64,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            m0: 32,
            ef_construction: 200,
            seed: 0,
        }
    }
}

/// A candidate ordered as a max-heap on distance (worst on top), ties by
/// slot so heap order is deterministic.
#[derive(Clone, Copy, Debug)]
struct Far(f32, Slot);
impl PartialEq for Far {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Far {}
impl PartialOrd for Far {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Far {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(Ordering::Equal)
            .then(self.1.cmp(&other.1))
    }
}

/// Same, but a min-heap (best on top).
#[derive(Clone, Copy, Debug)]
struct Near(f32, Slot);
impl PartialEq for Near {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Near {}
impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Near {
    fn cmp(&self, other: &Self) -> Ordering {
        Far(other.0, other.1).cmp(&Far(self.0, self.1))
    }
}

/// Reusable visited set: a bitset plus the list of set bits to clear.
struct Visited {
    bits: Vec<u64>,
    touched: Vec<Slot>,
}

impl Visited {
    fn new() -> Self {
        Self {
            bits: Vec::new(),
            touched: Vec::new(),
        }
    }

    fn reset(&mut self, slots: usize) {
        for slot in self.touched.drain(..) {
            self.bits[slot as usize >> 6] = 0;
        }
        let words = slots.div_ceil(64);
        if self.bits.len() < words {
            self.bits.resize(words, 0);
        }
    }

    /// Returns true when the slot was not yet visited.
    #[inline]
    fn insert(&mut self, slot: Slot) -> bool {
        let word = &mut self.bits[slot as usize >> 6];
        let mask = 1_u64 << (slot & 63);
        if *word & mask != 0 {
            return false;
        }
        if *word == 0 {
            self.touched.push(slot);
        }
        *word |= mask;
        true
    }
}

thread_local! {
    static SCRATCH: RefCell<Visited> = RefCell::new(Visited::new());
}

/// How a search computes distances during traversal (contract §7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SearchMode {
    /// Exact `f32` distances throughout.
    F32,
    /// Int8 distances during traversal, then exact `f32` rescoring of the
    /// best `rescore` candidates. The returned distance is always exact.
    Int8 { rescore: usize },
}

impl SearchMode {
    /// Contract default: rescore `max(4k, 100)` candidates.
    pub fn int8_default(k: usize) -> Self {
        Self::Int8 {
            rescore: (4 * k).max(100),
        }
    }
}

pub struct Hnsw {
    params: HnswParams,
    metric: Metric,
    vectors: FlatVectors,
    quantized: QuantizedVectors,
    /// Level of each slot.
    levels: Vec<u8>,
    /// Level-0 neighbour lists: `m0` entries per slot, `level0_len` used.
    level0: Vec<Slot>,
    level0_len: Vec<u8>,
    /// Upper-level lists per slot: `levels[slot] * m` entries, counts per level.
    upper: Vec<Vec<Slot>>,
    upper_len: Vec<Vec<u8>>,
    tombstones: Vec<u64>,
    tombstone_count: usize,
    entry: Option<Slot>,
    max_level: u8,
}

impl Hnsw {
    pub fn new(dims: usize, metric: Metric, params: HnswParams) -> Self {
        Self {
            params,
            metric,
            vectors: FlatVectors::new(dims),
            quantized: QuantizedVectors::new(dims),
            levels: Vec::new(),
            level0: Vec::new(),
            level0_len: Vec::new(),
            upper: Vec::new(),
            upper_len: Vec::new(),
            tombstones: Vec::new(),
            tombstone_count: 0,
            entry: None,
            max_level: 0,
        }
    }

    pub fn quantized(&self) -> &QuantizedVectors {
        &self.quantized
    }

    pub fn params(&self) -> HnswParams {
        self.params
    }

    pub fn metric(&self) -> Metric {
        self.metric
    }

    pub fn dims(&self) -> usize {
        self.vectors.dims()
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub fn live_len(&self) -> usize {
        self.len() - self.tombstone_count
    }

    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count
    }

    pub fn vectors(&self) -> &FlatVectors {
        &self.vectors
    }

    pub fn level(&self, slot: Slot) -> u8 {
        self.levels[slot as usize]
    }

    pub fn max_level(&self) -> u8 {
        self.max_level
    }

    #[inline]
    pub fn is_tombstoned(&self, slot: Slot) -> bool {
        self.tombstones
            .get(slot as usize >> 6)
            .is_some_and(|word| word & (1_u64 << (slot & 63)) != 0)
    }

    /// Mask a slot from results. Idempotent; its links remain for routing.
    pub fn tombstone(&mut self, slot: Slot) -> bool {
        assert!((slot as usize) < self.len(), "slot out of range");
        let word = &mut self.tombstones[slot as usize >> 6];
        let mask = 1_u64 << (slot & 63);
        if *word & mask != 0 {
            return false;
        }
        *word |= mask;
        self.tombstone_count += 1;
        true
    }

    /// Deterministic level for a slot (contract §8): splitmix64 over
    /// `(seed, slot)` mapped to a geometric draw with multiplier `1 / ln m`.
    pub fn level_for(&self, slot: Slot) -> u8 {
        let mut x = self.params.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (u64::from(slot).wrapping_add(0x6A09_E667_F3BC_C909));
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        // Uniform in (0, 1]; never exactly 0 so ln is finite.
        let unit = ((x >> 11) as f64 + 1.0) / ((1_u64 << 53) as f64 + 1.0);
        let multiplier = 1.0 / (self.params.m as f64).ln();
        let level = (-unit.ln() * multiplier).floor();
        level.min(31.0) as u8
    }

    #[inline]
    fn dist(&self, query: &[f32], slot: Slot) -> f32 {
        distance(
            self.metric,
            query,
            self.vectors.get(u64::from(slot)).expect("slot"),
        )
    }

    #[inline]
    fn neighbours(&self, slot: Slot, level: u8) -> &[Slot] {
        if level == 0 {
            let start = slot as usize * self.params.m0;
            &self.level0[start..start + self.level0_len[slot as usize] as usize]
        } else {
            let index = (level - 1) as usize;
            let start = index * self.params.m;
            let len = self.upper_len[slot as usize][index] as usize;
            &self.upper[slot as usize][start..start + len]
        }
    }

    fn set_neighbours(&mut self, slot: Slot, level: u8, list: &[Slot]) {
        if level == 0 {
            debug_assert!(list.len() <= self.params.m0);
            let start = slot as usize * self.params.m0;
            self.level0[start..start + list.len()].copy_from_slice(list);
            self.level0_len[slot as usize] = list.len() as u8;
        } else {
            debug_assert!(list.len() <= self.params.m);
            let index = (level - 1) as usize;
            let start = index * self.params.m;
            self.upper[slot as usize][start..start + list.len()].copy_from_slice(list);
            self.upper_len[slot as usize][index] = list.len() as u8;
        }
    }

    fn capacity(&self, level: u8) -> usize {
        if level == 0 {
            self.params.m0
        } else {
            self.params.m
        }
    }

    /// Insert a vector and return its slot. Vectors are normalized here for
    /// cosine collections. Single writer; readers may search concurrently
    /// once the caller publishes the new high-water mark.
    pub fn insert(&mut self, vector: &[f32]) -> Result<Slot, crate::vector::VectorError> {
        let slot_u64 = self.vectors.push(vector, self.metric)?;
        let slot = Slot::try_from(slot_u64).expect("partition slot space exhausted");
        self.quantized
            .push(self.vectors.get(slot_u64).expect("just pushed"));
        let level = self.level_for(slot);
        self.levels.push(level);
        self.level0.resize(self.level0.len() + self.params.m0, 0);
        self.level0_len.push(0);
        self.upper.push(vec![0; level as usize * self.params.m]);
        self.upper_len.push(vec![0; level as usize]);
        if self.tombstones.len() * 64 < self.len() {
            self.tombstones.push(0);
        }

        let Some(mut entry) = self.entry else {
            self.entry = Some(slot);
            self.max_level = level;
            return Ok(slot);
        };
        let query = self.vectors.get(slot_u64).expect("just pushed").to_vec();

        // Greedy descent through levels above the new node's level.
        let mut current_level = self.max_level;
        while current_level > level {
            let exact = |s: Slot| self.dist(&query, s);
            entry = self.greedy_closest(&exact, entry, current_level);
            current_level -= 1;
        }

        // From min(level, max_level) down to 0: search, select, link.
        let mut entries = vec![entry];
        let top = level.min(self.max_level);
        for lc in (0..=top).rev() {
            let candidates = {
                let exact = |s: Slot| self.dist(&query, s);
                self.search_layer(&exact, &entries, self.params.ef_construction, lc, None)
            };
            let selected = self.select_heuristic(&query, &candidates, self.capacity(lc));
            self.set_neighbours(slot, lc, &selected);
            for &neighbour in &selected {
                self.link_back(neighbour, slot, lc);
            }
            entries = candidates.iter().map(|c| c.1).collect();
        }
        if level > self.max_level {
            self.entry = Some(slot);
            self.max_level = level;
        }
        Ok(slot)
    }

    /// Add `slot` to `neighbour`'s list at `level`, pruning with the
    /// heuristic when the list is full.
    fn link_back(&mut self, neighbour: Slot, slot: Slot, level: u8) {
        let capacity = self.capacity(level);
        let current_len = self.neighbours(neighbour, level).len();
        if current_len < capacity {
            // Append in place: the list has spare capacity by construction.
            if level == 0 {
                let start = neighbour as usize * self.params.m0;
                self.level0[start + current_len] = slot;
                self.level0_len[neighbour as usize] += 1;
            } else {
                let index = (level - 1) as usize;
                let start = index * self.params.m;
                self.upper[neighbour as usize][start + current_len] = slot;
                self.upper_len[neighbour as usize][index] += 1;
            }
            return;
        }
        let selected = {
            let base = self.vectors.get(u64::from(neighbour)).expect("slot");
            let mut candidates: Vec<Far> = self
                .neighbours(neighbour, level)
                .iter()
                .map(|&s| Far(self.dist(base, s), s))
                .collect();
            candidates.push(Far(self.dist(base, slot), slot));
            self.select_heuristic(base, &candidates, capacity)
        };
        self.set_neighbours(neighbour, level, &selected);
    }

    /// HNSW Algorithm 4: keep a candidate only if it is closer to the query
    /// than to every already-selected neighbour; backfill from the pruned set.
    fn select_heuristic(&self, query: &[f32], candidates: &[Far], m: usize) -> Vec<Slot> {
        let mut sorted: Vec<Far> = candidates.to_vec();
        sorted.sort();
        let mut selected: Vec<Slot> = Vec::with_capacity(m);
        let mut pruned: Vec<Slot> = Vec::new();
        for Far(d_q, candidate) in sorted {
            if selected.len() >= m {
                break;
            }
            let candidate_vector = self.vectors.get(u64::from(candidate)).expect("slot");
            let closer_to_selected = selected.iter().any(|&s| {
                let d_s = distance(
                    self.metric,
                    candidate_vector,
                    self.vectors.get(u64::from(s)).expect("slot"),
                );
                d_s < d_q
            });
            if closer_to_selected {
                pruned.push(candidate);
            } else {
                selected.push(candidate);
            }
        }
        for slot in pruned {
            if selected.len() >= m {
                break;
            }
            selected.push(slot);
        }
        let _ = query;
        selected
    }

    fn greedy_closest(&self, dist: &dyn Fn(Slot) -> f32, mut current: Slot, level: u8) -> Slot {
        let mut best = dist(current);
        loop {
            let mut improved = false;
            for &neighbour in self.neighbours(current, level) {
                let d = dist(neighbour);
                if d < best || (d == best && neighbour < current) {
                    best = d;
                    current = neighbour;
                    improved = true;
                }
            }
            if !improved {
                return current;
            }
        }
    }

    /// Beam search on one level returning up to `ef` nearest slots that pass
    /// `allow` (tombstoned slots never pass). All visited slots route.
    fn search_layer(
        &self,
        dist: &dyn Fn(Slot) -> f32,
        entries: &[Slot],
        ef: usize,
        level: u8,
        allow: Option<&dyn Fn(Slot) -> bool>,
    ) -> Vec<Far> {
        SCRATCH.with(|scratch| {
            let mut visited = scratch.borrow_mut();
            visited.reset(self.len());
            let mut candidates: BinaryHeap<Near> = BinaryHeap::new();
            let mut results: BinaryHeap<Far> = BinaryHeap::new();
            let accepts = |slot: Slot| !self.is_tombstoned(slot) && allow.is_none_or(|f| f(slot));
            for &entry in entries {
                if !visited.insert(entry) {
                    continue;
                }
                let d = dist(entry);
                candidates.push(Near(d, entry));
                if accepts(entry) {
                    results.push(Far(d, entry));
                }
            }
            while let Some(Near(d, current)) = candidates.pop() {
                if results.len() >= ef {
                    if let Some(worst) = results.peek() {
                        if Far(d, current) > *worst {
                            break;
                        }
                    }
                }
                for &neighbour in self.neighbours(current, level) {
                    if !visited.insert(neighbour) {
                        continue;
                    }
                    let d = dist(neighbour);
                    let admit = results.len() < ef
                        || results
                            .peek()
                            .is_some_and(|worst| Far(d, neighbour) < *worst);
                    if admit {
                        candidates.push(Near(d, neighbour));
                        if accepts(neighbour) {
                            results.push(Far(d, neighbour));
                            if results.len() > ef {
                                results.pop();
                            }
                        }
                    }
                }
            }
            results.into_vec()
        })
    }

    /// Approximate top-k. `ef` is raised to at least `k`. `allow` restricts
    /// results (filters); tombstoned slots are always excluded. Results follow
    /// the contract order: distance ascending, then slot ascending.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        allow: Option<&dyn Fn(Slot) -> bool>,
    ) -> Vec<Hit> {
        self.search_with(query, k, ef, SearchMode::F32, allow)
    }

    /// Approximate top-k with the traversal mode chosen explicitly. In
    /// `Int8` mode the graph is traversed with int8 distances and the best
    /// `rescore` candidates are re-scored exactly from the `f32` originals;
    /// every returned distance is the exact distance of the returned slot.
    pub fn search_with(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mode: SearchMode,
        allow: Option<&dyn Fn(Slot) -> bool>,
    ) -> Vec<Hit> {
        let Some(mut entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let ef = ef.max(k);
        let exact = |s: Slot| self.dist(query, s);
        let quantized_query;
        let approximate;
        let dist: &dyn Fn(Slot) -> f32 = match mode {
            SearchMode::F32 => &exact,
            SearchMode::Int8 { .. } => {
                quantized_query = quantize(query);
                approximate = |s: Slot| {
                    self.quantized
                        .distance(self.metric, &quantized_query, s as usize)
                };
                &approximate
            }
        };
        let mut level = self.max_level;
        while level > 0 {
            entry = self.greedy_closest(dist, entry, level);
            level -= 1;
        }
        let candidates = self.search_layer(dist, &[entry], ef, 0, allow);
        let mut top = TopK::new(k);
        match mode {
            SearchMode::F32 => {
                for Far(d, slot) in candidates {
                    top.push(Hit {
                        id: u64::from(slot),
                        distance: d,
                    });
                }
            }
            SearchMode::Int8 { rescore } => {
                let mut ranked = candidates;
                ranked.sort();
                for Far(_, slot) in ranked.into_iter().take(rescore.max(k)) {
                    top.push(Hit {
                        id: u64::from(slot),
                        distance: exact(slot),
                    });
                }
            }
        }
        top.into_sorted()
    }

    /// Cutover between the exact route and masked traversal for filtered
    /// search (contract §9): allowed sets of at most this many slots are
    /// answered exactly. The constant is the measured crossover divided by
    /// `ef_search` on the public sets (294, 152, 215), rounded down to a
    /// value that favours the exact route near the boundary, since it is the
    /// one with perfect recall.
    pub const FILTER_CUTOVER_PER_EF: u64 = 200;

    pub fn filter_cutover(ef: usize) -> u64 {
        Self::FILTER_CUTOVER_PER_EF * ef.max(1) as u64
    }

    /// Filtered search (contract §9). `allowed` holds the slots a query may
    /// return; tombstoned slots are excluded regardless. When the allowed set
    /// is small (at most `cutover` slots) the query is answered exactly over
    /// that set, which is both cheaper and perfectly accurate in the regime
    /// where graph traversal degrades; otherwise the graph is traversed with
    /// the filter as a mask, routing through excluded slots without
    /// returning them.
    pub fn search_filtered(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mode: SearchMode,
        allowed: &roaring::RoaringBitmap,
        cutover: u64,
    ) -> Vec<Hit> {
        if k == 0 || allowed.is_empty() {
            return Vec::new();
        }
        if allowed.len() <= cutover {
            return self.search_exact_over(query, k, allowed);
        }
        let mask = |slot: Slot| allowed.contains(slot);
        self.search_with(query, k, ef, mode, Some(&mask))
    }

    /// Exact top-k over an explicit slot set (the selective-filter route).
    pub fn search_exact_over(
        &self,
        query: &[f32],
        k: usize,
        allowed: &roaring::RoaringBitmap,
    ) -> Vec<Hit> {
        let mut top = TopK::new(k);
        for slot in allowed.iter() {
            if slot as usize >= self.len() || self.is_tombstoned(slot) {
                continue;
            }
            top.push(Hit {
                id: u64::from(slot),
                distance: self.dist(query, slot),
            });
        }
        top.into_sorted()
    }

    /// Accounted bytes split into `(f32 originals, int8 copies, graph and
    /// bookkeeping)` for the memory model in contract §11.
    pub fn accounted_breakdown(&self) -> (usize, usize, usize) {
        let f32_bytes = self.vectors.as_slice().len() * 4;
        let int8_bytes = self.quantized.accounted_bytes();
        (
            f32_bytes,
            int8_bytes,
            self.accounted_bytes() - f32_bytes - int8_bytes,
        )
    }

    /// Accounted bytes of the structure, for the memory model in contract §11.
    pub fn accounted_bytes(&self) -> usize {
        let vectors = self.vectors.as_slice().len() * 4 + self.quantized.accounted_bytes();
        let level0 = self.level0.len() * 4 + self.level0_len.len();
        let upper: usize = self
            .upper
            .iter()
            .map(|list| list.len() * 4 + std::mem::size_of::<Vec<Slot>>())
            .sum::<usize>()
            + self
                .upper_len
                .iter()
                .map(|list| list.len() + std::mem::size_of::<Vec<u8>>())
                .sum::<usize>();
        vectors + level0 + upper + self.levels.len() + self.tombstones.len() * 8
    }
}

/// Neighbour lists guarded per slot for concurrent bulk construction.
struct LockedLinks {
    m: usize,
    m0: usize,
    /// One list per slot for level 0.
    level0: Vec<parking_lot::Mutex<Vec<Slot>>>,
    /// Upper lists per slot, one per level above 0.
    upper: Vec<Vec<parking_lot::Mutex<Vec<Slot>>>>,
}

impl LockedLinks {
    fn new(levels: &[u8], m: usize, m0: usize) -> Self {
        Self {
            m,
            m0,
            level0: levels
                .iter()
                .map(|_| parking_lot::Mutex::new(Vec::with_capacity(m0)))
                .collect(),
            upper: levels
                .iter()
                .map(|&level| {
                    (0..level)
                        .map(|_| parking_lot::Mutex::new(Vec::with_capacity(m)))
                        .collect()
                })
                .collect(),
        }
    }

    #[inline]
    fn list(&self, slot: Slot, level: u8) -> &parking_lot::Mutex<Vec<Slot>> {
        if level == 0 {
            &self.level0[slot as usize]
        } else {
            &self.upper[slot as usize][(level - 1) as usize]
        }
    }

    #[inline]
    fn snapshot(&self, slot: Slot, level: u8) -> Vec<Slot> {
        self.list(slot, level).lock().clone()
    }

    fn capacity(&self, level: u8) -> usize {
        if level == 0 {
            self.m0
        } else {
            self.m
        }
    }
}

/// Read-only view shared by all build threads.
struct BuildCore<'a> {
    metric: Metric,
    vectors: &'a FlatVectors,
    params: HnswParams,
    links: LockedLinks,
}

impl BuildCore<'_> {
    #[inline]
    fn dist(&self, query: &[f32], slot: Slot) -> f32 {
        distance(
            self.metric,
            query,
            self.vectors.get(u64::from(slot)).expect("slot"),
        )
    }

    fn greedy_closest(&self, query: &[f32], mut current: Slot, level: u8) -> Slot {
        let mut best = self.dist(query, current);
        loop {
            let mut improved = false;
            for neighbour in self.links.snapshot(current, level) {
                let d = self.dist(query, neighbour);
                if d < best || (d == best && neighbour < current) {
                    best = d;
                    current = neighbour;
                    improved = true;
                }
            }
            if !improved {
                return current;
            }
        }
    }

    fn search_layer(&self, query: &[f32], entries: &[Slot], ef: usize, level: u8) -> Vec<Far> {
        SCRATCH.with(|scratch| {
            let mut visited = scratch.borrow_mut();
            visited.reset(self.vectors.len());
            let mut candidates: BinaryHeap<Near> = BinaryHeap::new();
            let mut results: BinaryHeap<Far> = BinaryHeap::new();
            for &entry in entries {
                if !visited.insert(entry) {
                    continue;
                }
                let d = self.dist(query, entry);
                candidates.push(Near(d, entry));
                results.push(Far(d, entry));
            }
            while let Some(Near(d, current)) = candidates.pop() {
                if results.len() >= ef {
                    if let Some(worst) = results.peek() {
                        if Far(d, current) > *worst {
                            break;
                        }
                    }
                }
                for neighbour in self.links.snapshot(current, level) {
                    if !visited.insert(neighbour) {
                        continue;
                    }
                    let d = self.dist(query, neighbour);
                    let admit = results.len() < ef
                        || results
                            .peek()
                            .is_some_and(|worst| Far(d, neighbour) < *worst);
                    if admit {
                        candidates.push(Near(d, neighbour));
                        results.push(Far(d, neighbour));
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
            results.into_vec()
        })
    }

    /// HNSW Algorithm 4, identical to the sequential path.
    fn select_heuristic(&self, candidates: &[Far], m: usize) -> Vec<Slot> {
        let mut sorted: Vec<Far> = candidates.to_vec();
        sorted.sort();
        let mut selected: Vec<Slot> = Vec::with_capacity(m);
        let mut pruned: Vec<Slot> = Vec::new();
        for Far(d_q, candidate) in sorted {
            if selected.len() >= m {
                break;
            }
            let candidate_vector = self.vectors.get(u64::from(candidate)).expect("slot");
            let closer_to_selected = selected.iter().any(|&s| {
                distance(
                    self.metric,
                    candidate_vector,
                    self.vectors.get(u64::from(s)).expect("slot"),
                ) < d_q
            });
            if closer_to_selected {
                pruned.push(candidate);
            } else {
                selected.push(candidate);
            }
        }
        for slot in pruned {
            if selected.len() >= m {
                break;
            }
            selected.push(slot);
        }
        selected
    }

    fn insert(&self, slot: Slot, level: u8, entry: Slot, max_level: u8) {
        let query = self.vectors.get(u64::from(slot)).expect("slot");
        let mut current = entry;
        let mut current_level = max_level;
        while current_level > level {
            current = self.greedy_closest(query, current, current_level);
            current_level -= 1;
        }
        let mut entries = vec![current];
        for lc in (0..=level.min(max_level)).rev() {
            let candidates = self.search_layer(query, &entries, self.params.ef_construction, lc);
            let capacity = self.links.capacity(lc);
            let selected = self.select_heuristic(&candidates, capacity);
            *self.links.list(slot, lc).lock() = selected.clone();
            for neighbour in selected {
                let mut list = self.links.list(neighbour, lc).lock();
                if list.len() < capacity {
                    list.push(slot);
                    continue;
                }
                let base = self.vectors.get(u64::from(neighbour)).expect("slot");
                let mut with_new: Vec<Far> =
                    list.iter().map(|&s| Far(self.dist(base, s), s)).collect();
                with_new.push(Far(self.dist(base, slot), slot));
                *list = self.select_heuristic(&with_new, capacity);
            }
            entries = candidates.iter().map(|c| c.1).collect();
        }
    }
}

impl Hnsw {
    /// Build a graph over a fixed set of vectors using every available
    /// thread. Levels are the same deterministic function of `(seed, slot)`
    /// as sequential insertion, and the highest-level slot is the entry
    /// point, but neighbour selection depends on scheduling, so two parallel
    /// builds are not byte-identical. Intended for bulk loads, compaction
    /// rebuilds, and benchmarks; the WAL apply path inserts sequentially.
    pub fn build_parallel(vectors: FlatVectors, metric: Metric, params: HnswParams) -> Self {
        use rayon::prelude::*;

        let probe = Hnsw::new(vectors.dims(), metric, params);
        let slots = vectors.len();
        let levels: Vec<u8> = (0..slots)
            .map(|slot| probe.level_for(slot as Slot))
            .collect();
        let mut index = Hnsw {
            params,
            metric,
            vectors: FlatVectors::new(probe.dims()),
            quantized: QuantizedVectors::new(probe.dims()),
            levels: Vec::new(),
            level0: Vec::new(),
            level0_len: Vec::new(),
            upper: Vec::new(),
            upper_len: Vec::new(),
            tombstones: vec![0; slots.div_ceil(64)],
            tombstone_count: 0,
            entry: None,
            max_level: 0,
        };
        if slots == 0 {
            return index;
        }
        let (entry, max_level) = levels
            .iter()
            .enumerate()
            .map(|(slot, &level)| (slot as Slot, level))
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
            .expect("nonempty");

        let links = {
            let core = BuildCore {
                metric,
                vectors: &vectors,
                params,
                links: LockedLinks::new(&levels, params.m, params.m0),
            };
            // Insert in chunks that double in size, so the first few hundred
            // nodes link under little contention and later nodes find a
            // well-formed graph; within a chunk, insertion is parallel.
            let order: Vec<Slot> = (0..slots as Slot).filter(|&s| s != entry).collect();
            let mut start = 0_usize;
            let mut chunk = 1_usize;
            while start < order.len() {
                let end = (start + chunk).min(order.len());
                order[start..end]
                    .par_iter()
                    .for_each(|&slot| core.insert(slot, levels[slot as usize], entry, max_level));
                start = end;
                chunk = (chunk * 2).min(1 << 16);
            }
            core.links
        };

        // Compact; quantize every row in parallel.
        let mut quantized = QuantizedVectors::with_capacity(vectors.dims(), slots);
        let rows: Vec<crate::quant::QuantizedQuery> = (0..slots)
            .into_par_iter()
            .map(|slot| quantize(vectors.get(slot as u64).expect("slot")))
            .collect();
        for row in rows {
            quantized.push_quantized(row);
        }
        index.quantized = quantized;
        index.vectors = vectors;
        index.levels = levels;
        index.level0 = vec![0; slots * params.m0];
        index.level0_len = vec![0; slots];
        index.upper = Vec::with_capacity(slots);
        index.upper_len = Vec::with_capacity(slots);
        for slot in 0..slots {
            let list = links.level0[slot].lock();
            let start = slot * params.m0;
            index.level0[start..start + list.len()].copy_from_slice(&list);
            index.level0_len[slot] = list.len() as u8;
            let level = index.levels[slot] as usize;
            let mut flat = vec![0; level * params.m];
            let mut lens = vec![0_u8; level];
            for (i, guard) in links.upper[slot].iter().enumerate() {
                let list = guard.lock();
                flat[i * params.m..i * params.m + list.len()].copy_from_slice(&list);
                lens[i] = list.len() as u8;
            }
            index.upper.push(flat);
            index.upper_len.push(lens);
        }
        index.entry = Some(entry);
        index.max_level = max_level;
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle;
    use proptest::prelude::*;

    fn build(dims: usize, metric: Metric, rows: &[Vec<f32>]) -> Hnsw {
        let mut index = Hnsw::new(
            dims,
            metric,
            HnswParams {
                m: 8,
                m0: 16,
                ef_construction: 64,
                seed: 7,
            },
        );
        for row in rows {
            index.insert(row).unwrap();
        }
        index
    }

    fn oracle_store(dims: usize, metric: Metric, rows: &[Vec<f32>]) -> FlatVectors {
        let mut store = FlatVectors::new(dims);
        for row in rows {
            store.push(row, metric).unwrap();
        }
        store
    }

    #[test]
    fn levels_are_deterministic_and_geometric() {
        let index = Hnsw::new(4, Metric::L2, HnswParams::default());
        let levels: Vec<u8> = (0..20_000).map(|slot| index.level_for(slot)).collect();
        assert_eq!(
            levels,
            (0..20_000).map(|s| index.level_for(s)).collect::<Vec<_>>()
        );
        let zero = levels.iter().filter(|l| **l == 0).count() as f64 / levels.len() as f64;
        // P(level 0) = 1 - 1/m for multiplier 1/ln m.
        assert!((zero - (1.0 - 1.0 / 16.0)).abs() < 0.02, "{zero}");
        assert!(*levels.iter().max().unwrap() <= 8);
    }

    #[test]
    fn exhaustive_ef_matches_the_oracle_including_ties_and_tombstones() {
        let rows: Vec<Vec<f32>> = (0..300)
            .map(|i| vec![(i % 17) as f32, (i / 17) as f32, ((i * 7) % 5) as f32])
            .collect();
        let mut index = build(3, Metric::L2, &rows);
        let store = oracle_store(3, Metric::L2, &rows);
        let query = [3.2, 5.1, 2.0];
        let hits = index.search(&query, 10, 300, None);
        let exact = oracle::search(&store, Metric::L2, &query, 10, |_| true);
        assert_eq!(
            hits, exact,
            "ties resolve identically when the whole graph is explored"
        );

        for slot in [exact[0].id as Slot, exact[3].id as Slot] {
            assert!(index.tombstone(slot));
            assert!(!index.tombstone(slot));
        }
        let hits = index.search(&query, 10, 300, None);
        let exact = oracle::search(&store, Metric::L2, &query, 10, |id| {
            !index.is_tombstoned(id as Slot)
        });
        assert_eq!(hits, exact);
        assert!(hits.iter().all(|h| !index.is_tombstoned(h.id as Slot)));
        assert_eq!(index.live_len(), 298);
    }

    #[test]
    fn filters_restrict_results_but_still_route() {
        let rows: Vec<Vec<f32>> = (0..500)
            .map(|i| {
                vec![
                    (i as f32 * 0.37).sin(),
                    (i as f32 * 0.11).cos(),
                    (i % 13) as f32 / 13.0,
                ]
            })
            .collect();
        let index = build(3, Metric::Cosine, &rows);
        let store = oracle_store(3, Metric::Cosine, &rows);
        let mut query = vec![0.3, -0.2, 0.9];
        crate::distance::normalize(&mut query);
        let even = |slot: Slot| slot.is_multiple_of(2);
        let hits = index.search(&query, 5, 500, Some(&even));
        let exact = oracle::search(&store, Metric::Cosine, &query, 5, |id| id % 2 == 0);
        assert_eq!(hits, exact);
    }

    #[test]
    fn parallel_build_matches_sequential_quality() {
        let rows: Vec<Vec<f32>> = (0..3000)
            .map(|i| {
                (0..16)
                    .map(|d| ((i * (d + 3)) as f32 * 0.013).sin() + (d as f32 * 0.1))
                    .collect()
            })
            .collect();
        let params = HnswParams {
            m: 8,
            m0: 16,
            ef_construction: 64,
            seed: 11,
        };
        let sequential = {
            let mut index = Hnsw::new(16, Metric::L2, params);
            for row in &rows {
                index.insert(row).unwrap();
            }
            index
        };
        let mut store = FlatVectors::new(16);
        for row in &rows {
            store.push(row, Metric::L2).unwrap();
        }
        let parallel = Hnsw::build_parallel(store.clone(), Metric::L2, params);
        assert_eq!(parallel.len(), rows.len());
        assert_eq!(parallel.max_level(), sequential.max_level());
        for slot in 0..rows.len() as Slot {
            assert_eq!(parallel.level(slot), sequential.level(slot));
            assert!(
                !parallel.neighbours(slot, 0).is_empty(),
                "slot {slot} unlinked"
            );
        }
        let mut total = (0.0, 0.0);
        for q in 0..50 {
            let query: Vec<f32> = (0..16).map(|d| ((q * d + 1) as f32 * 0.07).cos()).collect();
            let exact = oracle::search(&store, Metric::L2, &query, 10, |_| true);
            let kth = exact.last().unwrap().distance;
            total.0 +=
                oracle::recall_by_distance(kth, &sequential.search(&query, 10, 64, None), 10);
            total.1 += oracle::recall_by_distance(kth, &parallel.search(&query, 10, 64, None), 10);
        }
        assert!(total.1 / 50.0 >= 0.9, "parallel recall {}", total.1 / 50.0);
        assert!((total.0 - total.1).abs() / 50.0 <= 0.05, "{total:?}");
    }

    #[test]
    fn filtered_search_matches_the_oracle_on_both_routes() {
        use roaring::RoaringBitmap;
        let rows: Vec<Vec<f32>> = (0..2000)
            .map(|i| {
                (0..12)
                    .map(|d| ((i * (d + 2)) as f32 * 0.017).sin())
                    .collect()
            })
            .collect();
        let mut store = FlatVectors::new(12);
        for row in &rows {
            store.push(row, Metric::L2).unwrap();
        }
        let mut index = Hnsw::build_parallel(
            store.clone(),
            Metric::L2,
            HnswParams {
                m: 8,
                m0: 16,
                ef_construction: 64,
                seed: 5,
            },
        );
        for slot in [3_u32, 400, 1999] {
            index.tombstone(slot);
        }
        let selective: RoaringBitmap = (0..2000_u32).filter(|s| s % 97 == 0).collect();
        let broad: RoaringBitmap = (0..2000_u32).filter(|s| s % 3 != 0).collect();
        let query: Vec<f32> = (0..12).map(|d| (d as f32 * 0.21).cos()).collect();
        for allowed in [&selective, &broad] {
            let exact = oracle::search(&store, Metric::L2, &query, 7, |id| {
                allowed.contains(id as Slot) && !index.is_tombstoned(id as Slot)
            });
            // Brute-force route (cutover above the set size) is exact.
            let brute = index.search_filtered(&query, 7, 64, SearchMode::F32, allowed, 10_000);
            assert_eq!(brute, exact);
            // Masked traversal with exhaustive ef also reproduces the oracle.
            let masked = index.search_filtered(&query, 7, 2000, SearchMode::F32, allowed, 0);
            assert_eq!(masked, exact);
            // Int8 masked traversal returns exact distances and only allowed slots.
            let int8 =
                index.search_filtered(&query, 7, 128, SearchMode::int8_default(7), allowed, 0);
            assert!(int8
                .iter()
                .all(|h| allowed.contains(h.id as Slot) && !index.is_tombstoned(h.id as Slot)));
            for hit in &int8 {
                assert_eq!(
                    hit.distance,
                    distance(Metric::L2, &query, store.get(hit.id).unwrap())
                );
            }
        }
        assert!(index
            .search_filtered(&query, 7, 64, SearchMode::F32, &RoaringBitmap::new(), 100)
            .is_empty());
        let only_tombstoned: RoaringBitmap = [3_u32, 400].into_iter().collect();
        assert!(index
            .search_filtered(&query, 7, 64, SearchMode::F32, &only_tombstoned, 100)
            .is_empty());
    }

    #[test]
    fn int8_search_returns_exact_distances_and_near_f32_recall() {
        let rows: Vec<Vec<f32>> = (0..4000)
            .map(|i| {
                (0..32)
                    .map(|d| ((i * (d + 5)) as f32 * 0.011).sin() * (1.0 + d as f32 * 0.05))
                    .collect()
            })
            .collect();
        let mut store = FlatVectors::new(32);
        for row in &rows {
            store.push(row, Metric::Cosine).unwrap();
        }
        let params = HnswParams {
            m: 16,
            m0: 32,
            ef_construction: 100,
            seed: 3,
        };
        let index = Hnsw::build_parallel(store.clone(), Metric::Cosine, params);
        let mut f32_total = 0.0;
        let mut int8_total = 0.0;
        for q in 0..100 {
            let mut query: Vec<f32> = (0..32).map(|d| ((q * d + 7) as f32 * 0.03).cos()).collect();
            crate::distance::normalize(&mut query);
            let exact = oracle::search(&store, Metric::Cosine, &query, 10, |_| true);
            let kth = exact.last().unwrap().distance;
            let f32_hits = index.search(&query, 10, 100, None);
            let int8_hits = index.search_with(&query, 10, 100, SearchMode::int8_default(10), None);
            for hit in &int8_hits {
                let recomputed = distance(Metric::Cosine, &query, store.get(hit.id).unwrap());
                assert_eq!(
                    hit.distance, recomputed,
                    "returned score must be the exact f32 distance"
                );
            }
            for pair in int8_hits.windows(2) {
                assert!(pair[0].cmp_rank(&pair[1]) != std::cmp::Ordering::Greater);
            }
            f32_total += oracle::recall_by_distance(kth, &f32_hits, 10);
            int8_total += oracle::recall_by_distance(kth, &int8_hits, 10);
        }
        let (f32_recall, int8_recall) = (f32_total / 100.0, int8_total / 100.0);
        assert!(f32_recall >= 0.95, "{f32_recall}");
        assert!(
            int8_recall >= f32_recall - 0.02,
            "f32 {f32_recall} vs int8 {int8_recall}"
        );
        assert!(index.accounted_bytes() > index.vectors().as_slice().len() * 4);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn recall_is_high_on_random_data(
            rows in prop::collection::vec(prop::collection::vec(-1.0_f32..1.0, 16), 50..400),
            query in prop::collection::vec(-1.0_f32..1.0, 16),
            deletes in prop::collection::vec(0_usize..400, 0..20),
        ) {
            let mut index = build(16, Metric::L2, &rows);
            let store = oracle_store(16, Metric::L2, &rows);
            for d in &deletes {
                if *d < rows.len() {
                    index.tombstone(*d as Slot);
                }
            }
            let k = 10.min(index.live_len());
            let hits = index.search(&query, k, 64, None);
            let exact = oracle::search(&store, Metric::L2, &query, k, |id| !index.is_tombstoned(id as Slot));
            prop_assert_eq!(hits.len(), k);
            prop_assert!(hits.iter().all(|h| !index.is_tombstoned(h.id as Slot)));
            let kth = exact.last().map(|h| h.distance).unwrap_or(0.0);
            let recall = oracle::recall_by_distance(kth, &hits, k);
            prop_assert!(recall >= 0.9, "recall {recall}");
            // Results are in contract order.
            for pair in hits.windows(2) {
                prop_assert!(pair[0].cmp_rank(&pair[1]) != std::cmp::Ordering::Greater);
            }
        }
    }
}
