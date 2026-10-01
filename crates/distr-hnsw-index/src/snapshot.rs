//! Partition snapshots (contract §5): one memory-mappable file with a
//! CRC-protected header, a section table with a BLAKE3 digest per section,
//! 64-byte-aligned sections, and a whole-file footer digest. Loading verifies
//! everything before any byte is trusted and fails closed naming the part
//! that disagreed. The `f32` originals are served straight from the map.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    hnsw::{GraphParts, HnswParams, OwnedGraphParts, Slot},
    quant::QuantizedVectors,
    vector::{FlatVectors, MappedRows},
    Metric,
};

pub const SNAPSHOT_MAGIC: &[u8; 4] = b"DHSN";
pub const SNAPSHOT_VERSION: u16 = 1;
const HEADER_LEN: usize = 128;
const SECTION_ENTRY_LEN: usize = 52;
const ALIGN: usize = 64;
const FOOTER_LEN: usize = 32;

const SECTION_KEYS: u16 = 1;
const SECTION_SLOTS: u16 = 2;
const SECTION_PAYLOADS: u16 = 3;
const SECTION_VECTORS_F32: u16 = 4;
const SECTION_VECTORS_I8: u16 = 5;
const SECTION_GRAPH: u16 = 6;
const SECTION_IDEMPOTENCY: u16 = 7;

/// Per-slot metadata stored in the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotRecord {
    pub version: u64,
    pub tombstone: bool,
    /// Index into the keys section; `u32::MAX` for a slot whose key was
    /// re-pointed by a later upsert (still recorded for history).
    pub key_index: u32,
    pub payload: Vec<u8>,
}

/// Everything a snapshot captures, borrowed from the partition.
pub struct SnapshotInput<'a> {
    pub partition_id: Uuid,
    pub dims: usize,
    pub metric: Metric,
    pub params: HnswParams,
    pub high_water: u64,
    /// External keys and the live slot each points to.
    pub keys: &'a [(Vec<u8>, Slot)],
    pub slots: &'a [SlotRecord],
    pub vectors: &'a FlatVectors,
    pub quantized: &'a QuantizedVectors,
    pub graph: GraphParts<'a>,
    pub idempotency: &'a [([u8; 16], u64)],
}

/// A verified, loaded snapshot.
pub struct LoadedSnapshot {
    pub path: PathBuf,
    pub partition_id: Uuid,
    pub dims: usize,
    pub metric: Metric,
    pub params: HnswParams,
    pub high_water: u64,
    pub keys: Vec<(Vec<u8>, Slot)>,
    pub slots: Vec<SlotRecord>,
    pub vectors: FlatVectors,
    pub quantized: QuantizedVectors,
    pub graph: OwnedGraphParts,
    pub idempotency: Vec<([u8; 16], u64)>,
    pub file_bytes: u64,
}

pub fn snapshot_name(high_water: u64) -> String {
    format!("{high_water:020}.snap")
}

fn pad_to(buffer: &mut Vec<u8>, align: usize) {
    while !buffer.len().is_multiple_of(align) {
        buffer.push(0);
    }
}

fn metric_code(metric: Metric) -> u8 {
    match metric {
        Metric::Cosine => 1,
        Metric::Dot => 2,
        Metric::L2 => 3,
    }
}

fn metric_from(code: u8) -> Option<Metric> {
    match code {
        1 => Some(Metric::Cosine),
        2 => Some(Metric::Dot),
        3 => Some(Metric::L2),
        _ => None,
    }
}

/// Serialize a snapshot into memory. Separate from writing so tests can
/// corrupt bytes deliberately.
pub fn encode(input: &SnapshotInput<'_>) -> Result<Vec<u8>, SnapshotError> {
    let slot_count = input.slots.len();
    if input.vectors.len() != slot_count || input.quantized.len() != slot_count {
        return Err(SnapshotError::Inconsistent(
            "vector counts differ from slot count",
        ));
    }
    let live_count = input.slots.iter().filter(|s| !s.tombstone).count();

    // Build sections.
    let mut sections: Vec<(u16, Vec<u8>)> = Vec::new();

    let mut keys = Vec::new();
    for (key, slot) in input.keys {
        if key.is_empty() || key.len() > u16::MAX as usize {
            return Err(SnapshotError::Inconsistent("key length"));
        }
        keys.extend_from_slice(&(key.len() as u16).to_le_bytes());
        keys.extend_from_slice(key);
        keys.extend_from_slice(&slot.to_le_bytes());
    }
    sections.push((SECTION_KEYS, keys));

    let mut slots = Vec::with_capacity(slot_count * 25);
    let mut payloads = Vec::new();
    for record in input.slots {
        slots.extend_from_slice(&record.version.to_le_bytes());
        slots.push(u8::from(record.tombstone));
        slots.extend_from_slice(&record.key_index.to_le_bytes());
        slots.extend_from_slice(&(payloads.len() as u64).to_le_bytes());
        slots.extend_from_slice(&(record.payload.len() as u32).to_le_bytes());
        payloads.extend_from_slice(&record.payload);
    }
    sections.push((SECTION_SLOTS, slots));
    sections.push((SECTION_PAYLOADS, payloads));

    let f32_rows = input.vectors.to_contiguous();
    let mut f32_bytes = Vec::with_capacity(f32_rows.len() * 4);
    for value in &f32_rows {
        f32_bytes.extend_from_slice(&value.to_le_bytes());
    }
    sections.push((SECTION_VECTORS_F32, f32_bytes));

    let (codes, scales, norms) = input.quantized.parts();
    let mut i8_bytes = Vec::with_capacity(codes.len() + scales.len() * 8);
    for value in scales {
        i8_bytes.extend_from_slice(&value.to_le_bytes());
    }
    for value in norms {
        i8_bytes.extend_from_slice(&value.to_le_bytes());
    }
    i8_bytes.extend(codes.iter().map(|c| *c as u8));
    sections.push((SECTION_VECTORS_I8, i8_bytes));

    let g = &input.graph;
    let mut graph = Vec::new();
    graph.extend_from_slice(&(input.params.m as u32).to_le_bytes());
    graph.extend_from_slice(&(input.params.m0 as u32).to_le_bytes());
    graph.extend_from_slice(&(input.params.ef_construction as u32).to_le_bytes());
    graph.extend_from_slice(&input.params.seed.to_le_bytes());
    graph.extend_from_slice(&g.entry.unwrap_or(u32::MAX).to_le_bytes());
    graph.push(g.max_level);
    graph.extend_from_slice(&(g.tombstone_count as u64).to_le_bytes());
    graph.extend_from_slice(g.levels);
    graph.extend_from_slice(g.level0_len);
    for slot in g.level0 {
        graph.extend_from_slice(&slot.to_le_bytes());
    }
    for word in g.tombstones {
        graph.extend_from_slice(&word.to_le_bytes());
    }
    for slot in 0..slot_count {
        let level = g.levels[slot] as usize;
        if level == 0 {
            continue;
        }
        graph.extend_from_slice(&g.upper_len[slot]);
        for neighbour in &g.upper[slot] {
            graph.extend_from_slice(&neighbour.to_le_bytes());
        }
    }
    sections.push((SECTION_GRAPH, graph));

    let mut idem = Vec::with_capacity(input.idempotency.len() * 24);
    for (op_id, seq) in input.idempotency {
        idem.extend_from_slice(op_id);
        idem.extend_from_slice(&seq.to_le_bytes());
    }
    sections.push((SECTION_IDEMPOTENCY, idem));

    // Lay out: header, table, aligned sections, footer.
    let table_offset = HEADER_LEN;
    let mut cursor = table_offset + sections.len() * SECTION_ENTRY_LEN;
    cursor = cursor.div_ceil(ALIGN) * ALIGN;
    let mut table = Vec::with_capacity(sections.len() * SECTION_ENTRY_LEN);
    let mut body = Vec::new();
    for (kind, bytes) in &sections {
        let offset = cursor + body.len();
        table.extend_from_slice(&kind.to_le_bytes());
        table.extend_from_slice(&0_u16.to_le_bytes());
        table.extend_from_slice(&(offset as u64).to_le_bytes());
        table.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        table.extend_from_slice(blake3::hash(bytes).as_bytes());
        body.extend_from_slice(bytes);
        pad_to(&mut body, ALIGN);
    }

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(SNAPSHOT_MAGIC);
    header.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    header.extend_from_slice(&0_u16.to_le_bytes());
    header.extend_from_slice(input.partition_id.as_bytes());
    header.extend_from_slice(&(input.dims as u32).to_le_bytes());
    header.push(metric_code(input.metric));
    header.push(1); // quant: int8 per vector
    header.extend_from_slice(&[0, 0]);
    header.extend_from_slice(&input.high_water.to_le_bytes());
    header.extend_from_slice(&(slot_count as u32).to_le_bytes());
    header.extend_from_slice(&(live_count as u32).to_le_bytes());
    header.extend_from_slice(&(input.keys.len() as u32).to_le_bytes());
    header.extend_from_slice(&(input.idempotency.len() as u32).to_le_bytes());
    header.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    header.extend_from_slice(&(table_offset as u64).to_le_bytes());
    while header.len() < HEADER_LEN - 4 {
        header.push(0);
    }
    let crc = crc32c::crc32c(&header);
    header.extend_from_slice(&crc.to_le_bytes());

    let mut file = Vec::with_capacity(HEADER_LEN + table.len() + body.len() + FOOTER_LEN + ALIGN);
    file.extend_from_slice(&header);
    file.extend_from_slice(&table);
    pad_to(&mut file, ALIGN);
    debug_assert_eq!(file.len(), cursor);
    file.extend_from_slice(&body);
    let digest = blake3::hash(&file);
    file.extend_from_slice(digest.as_bytes());
    Ok(file)
}

/// Durably write a snapshot to `directory/<high_water>.snap` via temp file,
/// fsync, rename, and directory fsync. Returns the final path.
pub fn write(directory: &Path, input: &SnapshotInput<'_>) -> Result<PathBuf, SnapshotError> {
    write_with_hook(directory, input, &|| true)
}

/// Like [`write`], calling `before_rename` after the temporary file is
/// synced; returning `false` abandons the write leaving the temporary file
/// in place, which models a crash at that boundary (contract §12).
pub fn write_with_hook(
    directory: &Path,
    input: &SnapshotInput<'_>,
    before_rename: &dyn Fn() -> bool,
) -> Result<PathBuf, SnapshotError> {
    fs::create_dir_all(directory)?;
    let bytes = encode(input)?;
    let final_path = directory.join(snapshot_name(input.high_water));
    let temporary = directory.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if !before_rename() {
            return Err(io::Error::other("snapshot aborted before rename"));
        }
        fs::rename(&temporary, &final_path)?;
        File::open(directory)?.sync_all()?;
        Ok::<(), io::Error>(())
    })();
    // A failure leaves the temporary file; open() ignores non-snapshot names.
    result?;
    Ok(final_path)
}

struct Cursor<'a>(&'a [u8], usize);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], SnapshotError> {
        if self.0.len() < n {
            return Err(SnapshotError::Malformed(what));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        self.1 += n;
        Ok(head)
    }
    fn u8(&mut self, w: &'static str) -> Result<u8, SnapshotError> {
        Ok(self.take(1, w)?[0])
    }
    fn u16(&mut self, w: &'static str) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(self.take(2, w)?.try_into().expect("2")))
    }
    fn u32(&mut self, w: &'static str) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(self.take(4, w)?.try_into().expect("4")))
    }
    fn u64(&mut self, w: &'static str) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(self.take(8, w)?.try_into().expect("8")))
    }
    fn f32(&mut self, w: &'static str) -> Result<f32, SnapshotError> {
        Ok(f32::from_le_bytes(self.take(4, w)?.try_into().expect("4")))
    }
}

/// Verify and load a snapshot. `expected_partition` guards against loading
/// another partition's file.
pub fn load(path: &Path, expected_partition: Uuid) -> Result<LoadedSnapshot, SnapshotError> {
    let file = File::open(path)?;
    let map = Arc::new(unsafe { memmap2::Mmap::map(&file)? });
    let bytes: &[u8] = &map;
    if bytes.len() < HEADER_LEN + FOOTER_LEN {
        return Err(SnapshotError::Malformed("file too short"));
    }
    // Header.
    let header = &bytes[..HEADER_LEN];
    let crc = u32::from_le_bytes(header[HEADER_LEN - 4..].try_into().expect("4"));
    if crc32c::crc32c(&header[..HEADER_LEN - 4]) != crc {
        return Err(SnapshotError::HeaderCrc);
    }
    let mut cursor = Cursor(header, 0);
    if cursor.take(4, "magic")? != SNAPSHOT_MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = cursor.u16("version")?;
    if version != SNAPSHOT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let _flags = cursor.u16("flags")?;
    let partition_id = Uuid::from_slice(cursor.take(16, "partition id")?)
        .map_err(|_| SnapshotError::Malformed("partition id"))?;
    if partition_id != expected_partition {
        return Err(SnapshotError::WrongPartition {
            found: partition_id,
            expected: expected_partition,
        });
    }
    let dims = cursor.u32("dims")? as usize;
    let metric = metric_from(cursor.u8("metric")?).ok_or(SnapshotError::Malformed("metric"))?;
    let quant = cursor.u8("quant")?;
    if quant != 1 {
        return Err(SnapshotError::Malformed("quantization scheme"));
    }
    cursor.take(2, "reserved")?;
    let high_water = cursor.u64("high water")?;
    let slot_count = cursor.u32("slot count")? as usize;
    let _live_count = cursor.u32("live count")? as usize;
    let key_count = cursor.u32("key count")? as usize;
    let idem_count = cursor.u32("idempotency count")? as usize;
    let section_count = cursor.u32("section count")? as usize;
    let table_offset = cursor.u64("table offset")? as usize;
    if dims == 0 {
        return Err(SnapshotError::Malformed("dims"));
    }

    // Footer over everything before it.
    let footer_at = bytes.len() - FOOTER_LEN;
    if blake3::hash(&bytes[..footer_at]).as_bytes() != &bytes[footer_at..] {
        return Err(SnapshotError::FooterHash);
    }

    // Section table and per-section digests.
    let table_end = table_offset + section_count * SECTION_ENTRY_LEN;
    if table_offset < HEADER_LEN || table_end > footer_at {
        return Err(SnapshotError::Malformed("section table"));
    }
    let mut sections: Vec<(u16, usize, usize)> = Vec::with_capacity(section_count);
    for i in 0..section_count {
        let entry = &bytes
            [table_offset + i * SECTION_ENTRY_LEN..table_offset + (i + 1) * SECTION_ENTRY_LEN];
        let kind = u16::from_le_bytes(entry[0..2].try_into().expect("2"));
        let offset = u64::from_le_bytes(entry[4..12].try_into().expect("8")) as usize;
        let len = u64::from_le_bytes(entry[12..20].try_into().expect("8")) as usize;
        let digest = &entry[20..52];
        if offset < table_end || offset.checked_add(len).is_none_or(|end| end > footer_at) {
            return Err(SnapshotError::Malformed("section bounds"));
        }
        if blake3::hash(&bytes[offset..offset + len]).as_bytes() != digest {
            return Err(SnapshotError::SectionHash(kind));
        }
        sections.push((kind, offset, len));
    }
    let section = |kind: u16| -> Result<&[u8], SnapshotError> {
        sections
            .iter()
            .find(|s| s.0 == kind)
            .map(|s| &bytes[s.1..s.1 + s.2])
            .ok_or(SnapshotError::MissingSection(kind))
    };

    // Keys.
    let mut c = Cursor(section(SECTION_KEYS)?, 0);
    let mut keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        let len = c.u16("key length")? as usize;
        let key = c.take(len, "key")?.to_vec();
        let slot = c.u32("key slot")?;
        keys.push((key, slot));
    }
    if !c.0.is_empty() {
        return Err(SnapshotError::Malformed("keys section trailing bytes"));
    }

    // Slots and payloads.
    let payloads = section(SECTION_PAYLOADS)?;
    let mut c = Cursor(section(SECTION_SLOTS)?, 0);
    let mut slots = Vec::with_capacity(slot_count);
    for _ in 0..slot_count {
        let version = c.u64("slot version")?;
        let tombstone = match c.u8("tombstone")? {
            0 => false,
            1 => true,
            _ => return Err(SnapshotError::Malformed("tombstone flag")),
        };
        let key_index = c.u32("key index")?;
        let offset = c.u64("payload offset")? as usize;
        let len = c.u32("payload length")? as usize;
        let payload = payloads
            .get(offset..offset + len)
            .ok_or(SnapshotError::Malformed("payload bounds"))?
            .to_vec();
        slots.push(SlotRecord {
            version,
            tombstone,
            key_index,
            payload,
        });
    }
    if !c.0.is_empty() {
        return Err(SnapshotError::Malformed("slots section trailing bytes"));
    }

    // f32 originals: served from the map.
    let (_, f32_offset, f32_len) = *sections
        .iter()
        .find(|s| s.0 == SECTION_VECTORS_F32)
        .ok_or(SnapshotError::MissingSection(SECTION_VECTORS_F32))?;
    if f32_len != slot_count * dims * 4 {
        return Err(SnapshotError::Malformed("f32 section length"));
    }
    let mapped = MappedRows::new(map.clone(), f32_offset, slot_count, dims)
        .map_err(|_| SnapshotError::Malformed("f32 section alignment"))?;
    let vectors =
        FlatVectors::from_mapped(dims, mapped).map_err(|_| SnapshotError::Malformed("dims"))?;

    // int8 copies.
    let mut c = Cursor(section(SECTION_VECTORS_I8)?, 0);
    let mut scales = Vec::with_capacity(slot_count);
    let mut norms = Vec::with_capacity(slot_count);
    for _ in 0..slot_count {
        scales.push(c.f32("scale")?);
    }
    for _ in 0..slot_count {
        norms.push(c.f32("norm")?);
    }
    let codes: Vec<i8> = c
        .take(slot_count * dims, "int8 codes")?
        .iter()
        .map(|b| *b as i8)
        .collect();
    if !c.0.is_empty() {
        return Err(SnapshotError::Malformed("int8 section trailing bytes"));
    }
    let quantized = QuantizedVectors::from_parts(dims, codes, scales, norms)
        .ok_or(SnapshotError::Malformed("int8 shape"))?;

    // Graph.
    let mut c = Cursor(section(SECTION_GRAPH)?, 0);
    let params = HnswParams {
        m: c.u32("m")? as usize,
        m0: c.u32("m0")? as usize,
        ef_construction: c.u32("ef_construction")? as usize,
        seed: c.u64("seed")?,
    };
    if params.m == 0 || params.m0 == 0 || params.m > 255 || params.m0 > 255 {
        return Err(SnapshotError::Malformed("graph parameters"));
    }
    let entry_raw = c.u32("entry")?;
    let entry = (entry_raw != u32::MAX).then_some(entry_raw);
    let max_level = c.u8("max level")?;
    let tombstone_count = c.u64("tombstone count")? as usize;
    let levels = c.take(slot_count, "levels")?.to_vec();
    let level0_len = c.take(slot_count, "level0 lengths")?.to_vec();
    let level0: Vec<Slot> = c
        .take(slot_count * params.m0 * 4, "level0 links")?
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4")))
        .collect();
    let tombstones: Vec<u64> = c
        .take(slot_count.div_ceil(64) * 8, "tombstone words")?
        .chunks_exact(8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8")))
        .collect();
    let mut upper = Vec::with_capacity(slot_count);
    let mut upper_len = Vec::with_capacity(slot_count);
    for &level in &levels {
        let level = level as usize;
        if level == 0 {
            upper.push(Vec::new());
            upper_len.push(Vec::new());
            continue;
        }
        let lens = c.take(level, "upper lengths")?.to_vec();
        let links: Vec<Slot> = c
            .take(level * params.m * 4, "upper links")?
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4")))
            .collect();
        upper.push(links);
        upper_len.push(lens);
    }
    if !c.0.is_empty() {
        return Err(SnapshotError::Malformed("graph section trailing bytes"));
    }
    for (slot, &len) in level0_len.iter().enumerate() {
        if len as usize > params.m0 {
            return Err(SnapshotError::Malformed("level0 length"));
        }
        for &n in &level0[slot * params.m0..slot * params.m0 + len as usize] {
            if n as usize >= slot_count {
                return Err(SnapshotError::Malformed("neighbour out of range"));
            }
        }
    }

    // Idempotency window.
    let mut c = Cursor(section(SECTION_IDEMPOTENCY)?, 0);
    let mut idempotency = Vec::with_capacity(idem_count);
    for _ in 0..idem_count {
        let op_id: [u8; 16] = c.take(16, "op id")?.try_into().expect("16");
        let seq = c.u64("op seq")?;
        idempotency.push((op_id, seq));
    }
    if !c.0.is_empty() {
        return Err(SnapshotError::Malformed(
            "idempotency section trailing bytes",
        ));
    }

    Ok(LoadedSnapshot {
        path: path.to_owned(),
        partition_id,
        dims,
        metric,
        params,
        high_water,
        keys,
        slots,
        vectors,
        quantized,
        graph: OwnedGraphParts {
            levels,
            level0,
            level0_len,
            upper,
            upper_len,
            tombstones,
            tombstone_count,
            entry,
            max_level,
        },
        idempotency,
        file_bytes: bytes.len() as u64,
    })
}

/// Snapshot files in a directory, newest high-water mark first.
pub fn list(directory: &Path) -> Result<Vec<(u64, PathBuf)>, SnapshotError> {
    let mut out = Vec::new();
    if !directory.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".snap") else {
            continue;
        };
        if let Ok(high_water) = stem.parse::<u64>() {
            out.push((high_water, path));
        }
    }
    out.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(out)
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot header CRC mismatch")]
    HeaderCrc,
    #[error("snapshot magic is invalid")]
    BadMagic,
    #[error("unsupported snapshot version {0}")]
    UnsupportedVersion(u16),
    #[error("snapshot belongs to partition {found}, expected {expected}")]
    WrongPartition { found: Uuid, expected: Uuid },
    #[error("snapshot footer hash mismatch")]
    FooterHash,
    #[error("snapshot section {0} hash mismatch")]
    SectionHash(u16),
    #[error("snapshot is missing section {0}")]
    MissingSection(u16),
    #[error("snapshot is malformed: {0}")]
    Malformed(&'static str),
    #[error("snapshot input is inconsistent: {0}")]
    Inconsistent(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
}
