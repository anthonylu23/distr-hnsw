//! Write-ahead log segments (contract §4). Every acknowledged operation is an
//! entry with a CRC32C, a strictly increasing sequence number, and an
//! operation id. Damage at the end of the newest segment, with no valid entry
//! after it, is a torn tail and is truncated; any other damage fails closed.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;

use crate::fsutil;

pub const WAL_MAGIC: &[u8; 4] = b"DHWL";
pub const WAL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
/// Rotate to a new segment once the current one exceeds this size.
pub const SEGMENT_ROTATE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_KEY_LEN: usize = 512;
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;
pub const MAX_DIMS: usize = 1 << 16;
/// Smallest legal `len` field: crc, seq, op_id, op, key_len, and a 1-byte key.
pub const MIN_FRAME_LEN: usize = 4 + 8 + 16 + 1 + 2 + 1;
/// Largest legal `len` field: an upsert with the longest key, payload, and
/// vector. Anything outside `MIN_FRAME_LEN..=MAX_FRAME_LEN` is not a frame.
pub const MAX_FRAME_LEN: usize =
    4 + 8 + 16 + 1 + 2 + MAX_KEY_LEN + 4 + MAX_PAYLOAD_LEN + 4 + 4 * MAX_DIMS;

#[derive(Clone, Debug, PartialEq)]
pub enum Operation {
    Upsert {
        key: Vec<u8>,
        payload: Vec<u8>,
        vector: Vec<f32>,
    },
    Delete {
        key: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub op_id: [u8; 16],
    pub operation: Operation,
}

impl Entry {
    fn encode(&self) -> Result<Vec<u8>, WalError> {
        let mut body = Vec::new();
        body.extend_from_slice(&self.seq.to_le_bytes());
        body.extend_from_slice(&self.op_id);
        match &self.operation {
            Operation::Upsert {
                key,
                payload,
                vector,
            } => {
                check_key(key)?;
                if payload.len() > MAX_PAYLOAD_LEN {
                    return Err(WalError::FieldTooLarge("payload"));
                }
                if vector.is_empty() || vector.len() > MAX_DIMS {
                    return Err(WalError::FieldTooLarge("vector"));
                }
                body.push(1);
                body.extend_from_slice(&(key.len() as u16).to_le_bytes());
                body.extend_from_slice(key);
                body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                body.extend_from_slice(payload);
                body.extend_from_slice(&(vector.len() as u32).to_le_bytes());
                for value in vector {
                    body.extend_from_slice(&value.to_le_bytes());
                }
            }
            Operation::Delete { key } => {
                check_key(key)?;
                body.push(2);
                body.extend_from_slice(&(key.len() as u16).to_le_bytes());
                body.extend_from_slice(key);
            }
        }
        let crc = crc32c::crc32c(&body);
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    fn decode(body: &[u8]) -> Result<Self, WalError> {
        let mut reader = Reader(body);
        let seq = reader.u64()?;
        let op_id: [u8; 16] = reader.take(16)?.try_into().expect("16 bytes");
        let op = reader.u8()?;
        let key_len = reader.u16()? as usize;
        let key = reader.take(key_len)?.to_vec();
        check_key(&key)?;
        let operation = match op {
            1 => {
                let payload_len = reader.u32()? as usize;
                if payload_len > MAX_PAYLOAD_LEN {
                    return Err(WalError::Malformed("payload length"));
                }
                let payload = reader.take(payload_len)?.to_vec();
                let dims = reader.u32()? as usize;
                if dims == 0 || dims > MAX_DIMS {
                    return Err(WalError::Malformed("vector length"));
                }
                let raw = reader.take(dims * 4)?;
                let vector = raw
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().expect("4 bytes")))
                    .collect();
                Operation::Upsert {
                    key,
                    payload,
                    vector,
                }
            }
            2 => Operation::Delete { key },
            _ => return Err(WalError::Malformed("operation code")),
        };
        if !reader.0.is_empty() {
            return Err(WalError::Malformed("trailing bytes in entry"));
        }
        Ok(Self {
            seq,
            op_id,
            operation,
        })
    }
}

fn check_key(key: &[u8]) -> Result<(), WalError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(WalError::FieldTooLarge("key"));
    }
    Ok(())
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WalError> {
        if self.0.len() < n {
            return Err(WalError::Malformed("truncated entry body"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, WalError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, WalError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2")))
    }
    fn u32(&mut self) -> Result<u32, WalError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4")))
    }
    fn u64(&mut self) -> Result<u64, WalError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8")))
    }
}

fn segment_name(first_seq: u64) -> String {
    format!("{first_seq:020}.wal")
}

fn encode_header(partition_id: Uuid, first_seq: u64) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[0..4].copy_from_slice(WAL_MAGIC);
    header[4..6].copy_from_slice(&WAL_VERSION.to_le_bytes());
    header[8..24].copy_from_slice(partition_id.as_bytes());
    header[24..32].copy_from_slice(&first_seq.to_le_bytes());
    header
}

fn decode_header(bytes: &[u8], expected: Uuid, path: &Path) -> Result<u64, WalError> {
    if bytes.len() < HEADER_LEN {
        return Err(WalError::BadHeader(path.to_owned(), "short header"));
    }
    if &bytes[0..4] != WAL_MAGIC {
        return Err(WalError::BadHeader(path.to_owned(), "magic"));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != WAL_VERSION {
        return Err(WalError::UnsupportedVersion(version));
    }
    let id = Uuid::from_slice(&bytes[8..24])
        .map_err(|_| WalError::BadHeader(path.to_owned(), "partition id"))?;
    if id != expected {
        return Err(WalError::WrongPartition {
            path: path.to_owned(),
            found: id,
            expected,
        });
    }
    Ok(u64::from_le_bytes(bytes[24..32].try_into().expect("8")))
}

/// Outcome of reading one segment.
#[derive(Debug, Default)]
pub struct SegmentRead {
    pub entries: Vec<Entry>,
    /// Set when the newest segment ends in a torn tail: `(offset, bytes
    /// after it)`. Nothing is changed on disk; the caller repairs with
    /// [`truncate_torn_tail`] once the whole log has been validated. An
    /// offset below [`HEADER_LEN`] means the segment's header was never
    /// written, so the file holds no entries and is removed.
    pub torn_tail: Option<(u64, u64)>,
}

impl SegmentRead {
    /// Sequence of the last intact entry, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.entries.last().map(|entry| entry.seq)
    }
}

fn name_first_seq(path: &Path) -> Option<u64> {
    path.file_name()?
        .to_str()?
        .strip_suffix(".wal")?
        .parse()
        .ok()
}

/// Read a whole segment, validating the header, every CRC, and sequence
/// continuity. Only the `active` (newest) segment may end in a torn tail:
/// segments are synced before the next one is created, so damage anywhere
/// in a sealed segment is corruption. A tail counts as torn only when no
/// valid entry starts anywhere after the damage, so a damaged length field
/// in the middle of the log can never discard the acknowledged entries
/// behind it. The file is never modified here.
pub fn read_segment(
    path: &Path,
    partition_id: Uuid,
    active: bool,
) -> Result<SegmentRead, WalError> {
    let bytes = fs::read(path)?;
    // A segment's header is written and synced before any entry, so a short
    // or all-zero newest segment was created by an interrupted rotation and
    // holds nothing.
    if active && (bytes.len() < HEADER_LEN || bytes.iter().all(|b| *b == 0)) {
        return Ok(SegmentRead {
            entries: Vec::new(),
            torn_tail: Some((0, bytes.len() as u64)),
        });
    }
    let first_seq = decode_header(&bytes, partition_id, path)?;
    if name_first_seq(path).is_some_and(|named| named != first_seq) {
        return Err(WalError::BadHeader(
            path.to_owned(),
            "first sequence disagrees with the segment name",
        ));
    }
    let mut offset = HEADER_LEN;
    let mut entries = Vec::new();
    let mut expected = first_seq;
    let mut torn_tail = None;
    while offset < bytes.len() {
        match parse_entry(&bytes[offset..]) {
            Ok((entry, consumed)) => {
                if entry.seq != expected {
                    return Err(WalError::SequenceGap {
                        path: path.to_owned(),
                        expected,
                        found: entry.seq,
                    });
                }
                expected += 1;
                entries.push(entry);
                offset += consumed;
            }
            Err(FrameError::Corrupt(reason)) => {
                return Err(WalError::Corrupt {
                    path: path.to_owned(),
                    offset: offset as u64,
                    reason: reason.to_owned(),
                });
            }
            Err(FrameError::Unreadable(reason)) => {
                if !active {
                    return Err(WalError::Corrupt {
                        path: path.to_owned(),
                        offset: offset as u64,
                        reason: format!("{reason} in a sealed segment"),
                    });
                }
                if let Some(at) = next_valid_frame(&bytes, offset + 1) {
                    return Err(WalError::Corrupt {
                        path: path.to_owned(),
                        offset: offset as u64,
                        reason: format!("{reason}, with a valid entry after it at offset {at}"),
                    });
                }
                torn_tail = Some((offset as u64, (bytes.len() - offset) as u64));
                break;
            }
        }
    }
    Ok(SegmentRead { entries, torn_tail })
}

/// Remove a torn tail found by [`read_segment`]: truncate the file at
/// `offset` and sync it, or remove a segment whose header was never written.
pub fn truncate_torn_tail(path: &Path, offset: u64) -> Result<(), WalError> {
    if offset < HEADER_LEN as u64 {
        fs::remove_file(path)?;
        fsutil::sync_dir(path.parent().unwrap_or(Path::new(".")))?;
        return Ok(());
    }
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(offset)?;
    file.sync_all()?;
    Ok(())
}

/// Why a frame could not be read.
enum FrameError {
    /// The frame boundary itself is unreadable (short, impossible length, or
    /// a CRC mismatch): a torn tail if nothing valid follows.
    Unreadable(&'static str),
    /// The CRC matched but the body is invalid: never a torn write.
    Corrupt(&'static str),
}

fn frame_len(bytes: &[u8]) -> Option<usize> {
    let len = u32::from_le_bytes(bytes.get(0..4)?.try_into().expect("4")) as usize;
    (MIN_FRAME_LEN..=MAX_FRAME_LEN)
        .contains(&len)
        .then_some(len)
}

fn parse_entry(bytes: &[u8]) -> Result<(Entry, usize), FrameError> {
    if bytes.len() < 8 {
        return Err(FrameError::Unreadable("short frame"));
    }
    let len = frame_len(bytes).ok_or(FrameError::Unreadable("frame length out of range"))?;
    if bytes.len() < 4 + len {
        return Err(FrameError::Unreadable(
            "frame runs past the end of the segment",
        ));
    }
    let crc = u32::from_le_bytes(bytes[4..8].try_into().expect("4"));
    let body = &bytes[8..4 + len];
    if crc32c::crc32c(body) != crc {
        return Err(FrameError::Unreadable("crc mismatch"));
    }
    let entry = Entry::decode(body).map_err(|error| match error {
        WalError::Malformed(reason) => FrameError::Corrupt(reason),
        _ => FrameError::Corrupt("entry body"),
    })?;
    Ok((entry, 4 + len))
}

/// Offset of the first complete, CRC-valid frame at or after `from`.
fn next_valid_frame(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(7)).find(|&at| {
        let rest = &bytes[at..];
        frame_len(rest).is_some_and(|len| {
            rest.len() >= 4 + len
                && crc32c::crc32c(&rest[8..4 + len])
                    == u32::from_le_bytes(rest[4..8].try_into().expect("4"))
        })
    })
}

/// Appends entries to the active segment, rotating by size, syncing before
/// every acknowledgement. After any failed write, sync, or rotation the
/// writer is poisoned and refuses further appends: the file may hold a
/// partial frame or an entry whose durability is unknown, and only a reopen,
/// which runs recovery, can establish the true end of the log.
pub struct WalWriter {
    directory: PathBuf,
    partition_id: Uuid,
    file: File,
    path: PathBuf,
    bytes: u64,
    next_seq: u64,
    poisoned: bool,
}

impl WalWriter {
    /// Open the writer positioned after `last_seq`. The newest segment is
    /// extended only when it ends exactly at `last_seq` and is below the
    /// rotation size; otherwise a new segment starts at `last_seq + 1`. A
    /// newest segment that ends anywhere else is refused, so the writer
    /// never appends behind a stale or damaged tail.
    pub fn open(directory: &Path, partition_id: Uuid, last_seq: u64) -> Result<Self, WalError> {
        fsutil::create_dir_durable(directory)?;
        let next_seq = last_seq + 1;
        let newest = list_segments(directory)?.pop();
        let extend = match newest {
            Some((first, path)) if first <= next_seq => {
                let read = read_segment(&path, partition_id, true)?;
                if read.torn_tail.is_some() {
                    return Err(WalError::Corrupt {
                        path,
                        offset: read.torn_tail.map_or(0, |t| t.0),
                        reason: "torn tail not yet repaired by recovery".to_owned(),
                    });
                }
                let end = read.last_seq().unwrap_or(first - 1);
                if end != last_seq {
                    return Err(WalError::SequenceGap {
                        path,
                        expected: next_seq,
                        found: end + 1,
                    });
                }
                let len = fs::metadata(&path)?.len();
                (len < SEGMENT_ROTATE_BYTES).then_some((path, len))
            }
            Some((first, path)) => {
                return Err(WalError::SequenceGap {
                    path,
                    expected: next_seq,
                    found: first,
                });
            }
            None => None,
        };
        let (path, bytes) = match extend {
            Some(existing) => existing,
            None => (
                create_segment(directory, partition_id, next_seq)?,
                HEADER_LEN as u64,
            ),
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            directory: directory.to_owned(),
            partition_id,
            file,
            path,
            bytes,
            next_seq,
            poisoned: false,
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// True after a failed write, sync, or rotation (or [`poison`](Self::poison)).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Refuse every later append. Used by the partition when the log and the
    /// in-memory state may have diverged.
    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Append one operation and durably sync it. Returns its sequence number.
    pub fn append(&mut self, op_id: [u8; 16], operation: Operation) -> Result<u64, WalError> {
        self.append_unsynced(op_id, operation).and_then(|seq| {
            self.sync()?;
            Ok(seq)
        })
    }

    /// Append without syncing; the caller must call [`sync`](Self::sync)
    /// before acknowledging (group commit). Used by failpoint tests to model
    /// a crash between append and sync.
    pub fn append_unsynced(
        &mut self,
        op_id: [u8; 16],
        operation: Operation,
    ) -> Result<u64, WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        let seq = self.next_seq;
        let entry = Entry {
            seq,
            op_id,
            operation,
        };
        // An invalid entry is refused before anything touches the file.
        let frame = entry.encode()?;
        if self.bytes >= SEGMENT_ROTATE_BYTES {
            if let Err(error) = self.rotate() {
                self.poisoned = true;
                return Err(error);
            }
        }
        if let Err(error) = self.file.write_all(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.bytes += frame.len() as u64;
        self.next_seq += 1;
        Ok(seq)
    }

    pub fn sync(&mut self) -> Result<(), WalError> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        // A failed fdatasync is not retried: Linux may report a writeback
        // error once and drop the dirty pages, so a retry could falsely
        // succeed.
        if let Err(error) = self.file.sync_data() {
            self.poisoned = true;
            return Err(error.into());
        }
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), WalError> {
        self.file.sync_all()?;
        let path = create_segment(&self.directory, self.partition_id, self.next_seq)?;
        self.file = OpenOptions::new().append(true).open(&path)?;
        self.path = path;
        self.bytes = HEADER_LEN as u64;
        Ok(())
    }

    pub fn current_segment(&self) -> &Path {
        &self.path
    }
}

/// Create a segment holding only its header, synced, with the directory
/// entry synced, before any entry can be appended to it.
fn create_segment(
    directory: &Path,
    partition_id: Uuid,
    first_seq: u64,
) -> Result<PathBuf, WalError> {
    let path = directory.join(segment_name(first_seq));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(&encode_header(partition_id, first_seq))?;
    file.sync_all()?;
    fsutil::sync_dir(directory)?;
    Ok(path)
}

/// Segments in ascending first-sequence order as `(first_seq, path)`.
pub fn list_segments(directory: &Path) -> Result<Vec<(u64, PathBuf)>, WalError> {
    let mut out = Vec::new();
    if !directory.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".wal") else {
            continue;
        };
        let first: u64 = stem
            .parse()
            .map_err(|_| WalError::BadHeader(path.clone(), "segment name"))?;
        out.push((first, path));
    }
    out.sort();
    Ok(out)
}

#[derive(Debug, Error)]
pub enum WalError {
    #[error("WAL segment {0} has a bad header: {1}")]
    BadHeader(PathBuf, &'static str),
    #[error("unsupported WAL version {0}")]
    UnsupportedVersion(u16),
    #[error("WAL segment {path} belongs to partition {found}, expected {expected}")]
    WrongPartition {
        path: PathBuf,
        found: Uuid,
        expected: Uuid,
    },
    #[error("WAL segment {path} is corrupt at offset {offset}: {reason}")]
    Corrupt {
        path: PathBuf,
        offset: u64,
        reason: String,
    },
    #[error("WAL segment {path} has a sequence gap: expected {expected}, found {found}")]
    SequenceGap {
        path: PathBuf,
        expected: u64,
        found: u64,
    },
    #[error("malformed WAL entry: {0}")]
    Malformed(&'static str),
    #[error("WAL field too large: {0}")]
    FieldTooLarge(&'static str),
    #[error("WAL writer failed earlier; reopen the partition to recover the log")]
    Poisoned,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(key: &str, v: &[f32]) -> Operation {
        Operation::Upsert {
            key: key.as_bytes().to_vec(),
            payload: b"{}".to_vec(),
            vector: v.to_vec(),
        }
    }

    #[test]
    fn entries_round_trip_and_sequence_continues_across_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let dir = directory.path().join("wal");
        let mut writer = WalWriter::open(&dir, id, 0).unwrap();
        assert_eq!(writer.append([1; 16], upsert("a", &[1.0, 2.0])).unwrap(), 1);
        assert_eq!(
            writer
                .append([2; 16], Operation::Delete { key: b"a".to_vec() })
                .unwrap(),
            2
        );
        drop(writer);
        let segments = list_segments(&dir).unwrap();
        assert_eq!(segments.len(), 1);
        let read = read_segment(&segments[0].1, id, true).unwrap();
        assert_eq!(read.entries.len(), 2);
        assert!(read.torn_tail.is_none());
        assert_eq!(
            read.entries[1].operation,
            Operation::Delete { key: b"a".to_vec() }
        );

        let mut writer = WalWriter::open(&dir, id, 2).unwrap();
        assert_eq!(writer.append([3; 16], upsert("b", &[0.5])).unwrap(), 3);
        drop(writer);
        let read = read_segment(&segments[0].1, id, true).unwrap();
        assert_eq!(
            read.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    fn five_entry_segment() -> (tempfile::TempDir, Uuid, PathBuf, Vec<u8>, Vec<usize>) {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let dir = directory.path().join("wal");
        let mut writer = WalWriter::open(&dir, id, 0).unwrap();
        for i in 0..5 {
            writer
                .append([i; 16], upsert(&format!("k{i}"), &[i as f32; 8]))
                .unwrap();
        }
        let path = writer.current_segment().to_owned();
        drop(writer);
        let full = fs::read(&path).unwrap();
        // Offsets of every frame's len field.
        let mut starts = Vec::new();
        let mut at = HEADER_LEN;
        while at < full.len() {
            starts.push(at);
            at += 4 + u32::from_le_bytes(full[at..at + 4].try_into().unwrap()) as usize;
        }
        assert_eq!(starts.len(), 5);
        (directory, id, path, full, starts)
    }

    #[test]
    fn torn_tail_is_reported_then_truncated_but_mid_segment_damage_fails_closed() {
        let (_directory, id, path, full, _) = five_entry_segment();

        // Cut the last entry in half: torn tail, reported, file untouched
        // until the caller repairs it.
        let cut = full.len() - 20;
        fs::write(&path, &full[..cut]).unwrap();
        let read = read_segment(&path, id, true).unwrap();
        assert_eq!(read.entries.len(), 4);
        let (offset, dropped) = read.torn_tail.unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), cut, "read never repairs");
        assert_eq!(offset + dropped, cut as u64);
        truncate_torn_tail(&path, offset).unwrap();
        assert_eq!(fs::read(&path).unwrap().len() as u64, offset);
        let again = read_segment(&path, id, true).unwrap();
        assert_eq!(again.entries.len(), 4);
        assert!(again.torn_tail.is_none());

        // The same cut in a sealed segment is corruption, not a torn tail.
        fs::write(&path, &full[..cut]).unwrap();
        assert!(matches!(
            read_segment(&path, id, false),
            Err(WalError::Corrupt { .. })
        ));

        // Flip a byte inside the second entry: not a tail, must fail closed.
        let mut damaged = full.clone();
        damaged[HEADER_LEN + 60] ^= 0x01;
        fs::write(&path, &damaged).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::Corrupt { .. })
        ));

        // Wrong partition id and bad magic fail closed too.
        let mut other = full.clone();
        other[8] ^= 0xFF;
        fs::write(&path, &other).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::WrongPartition { .. })
        ));
        let mut magic = full.clone();
        magic[0] = b'X';
        fs::write(&path, &magic).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::BadHeader(..))
        ));
        let mut version = full.clone();
        version[4] = 9;
        fs::write(&path, &version).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::UnsupportedVersion(9))
        ));
        // A header whose first sequence disagrees with the file name.
        let mut renamed = full;
        renamed[24] = 7;
        fs::write(&path, &renamed).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::BadHeader(..))
        ));
    }

    #[test]
    fn a_damaged_length_field_never_discards_the_entries_behind_it() {
        let (_directory, id, path, full, starts) = five_entry_segment();
        // Every bit of every non-final entry's len field: whatever value it
        // takes, later intact entries exist, so the read must fail closed.
        for &start in &starts[..4] {
            for byte in 0..4 {
                for bit in 0..8 {
                    let mut damaged = full.clone();
                    damaged[start + byte] ^= 1 << bit;
                    fs::write(&path, &damaged).unwrap();
                    let result = read_segment(&path, id, true);
                    assert!(
                        matches!(result, Err(WalError::Corrupt { .. })),
                        "len flip at {start}+{byte} bit {bit}: {:?}",
                        result.map(|r| (r.entries.len(), r.torn_tail))
                    );
                }
            }
        }
        // The final entry's len is the only one whose damage reads as a torn
        // tail: nothing valid follows it.
        let last = starts[4];
        let mut damaged = full.clone();
        damaged[last + 3] ^= 0x01;
        fs::write(&path, &damaged).unwrap();
        let read = read_segment(&path, id, true).unwrap();
        assert_eq!(read.entries.len(), 4);
        assert_eq!(read.torn_tail.unwrap().0, last as u64);
    }

    #[test]
    fn zero_filled_tails_and_unwritten_headers_are_torn_not_corrupt() {
        let (directory, id, path, full, _) = five_entry_segment();
        // Size extended but data never written: a zero-filled region.
        let mut zeroed = full.clone();
        zeroed.extend(std::iter::repeat_n(0_u8, 4096));
        fs::write(&path, &zeroed).unwrap();
        let read = read_segment(&path, id, true).unwrap();
        assert_eq!(read.entries.len(), 5);
        assert_eq!(read.torn_tail, Some((full.len() as u64, 4096)));

        // A newest segment whose header never reached the disk holds nothing
        // and is removed by the repair.
        let dir = directory.path().join("wal");
        let empty = dir.join(segment_name(6));
        fs::write(&empty, [0_u8; 32]).unwrap();
        let read = read_segment(&empty, id, true).unwrap();
        assert!(read.entries.is_empty());
        assert_eq!(read.torn_tail, Some((0, 32)));
        truncate_torn_tail(&empty, 0).unwrap();
        assert!(!empty.exists());
        fs::write(&empty, [0x44_u8; 10]).unwrap();
        assert_eq!(
            read_segment(&empty, id, true).unwrap().torn_tail,
            Some((0, 10))
        );
        // As a sealed segment the same file is a bad header.
        assert!(matches!(
            read_segment(&empty, id, false),
            Err(WalError::BadHeader(..))
        ));
    }

    #[test]
    fn writer_refuses_a_stale_tail_and_stays_poisoned_after_a_failure() {
        let (directory, id, path, full, starts) = five_entry_segment();
        let dir = directory.path().join("wal");
        // The newest segment ends at 5; a writer told the log ends at 7
        // (a snapshot ahead of a stale segment) must not append seq 8 after 5.
        assert!(matches!(
            WalWriter::open(&dir, id, 7),
            Err(WalError::SequenceGap { .. })
        ));
        // An unrepaired torn tail is refused as well.
        fs::write(&path, &full[..starts[4] + 3]).unwrap();
        assert!(matches!(
            WalWriter::open(&dir, id, 4),
            Err(WalError::Corrupt { .. })
        ));
        fs::write(&path, &full).unwrap();

        let mut writer = WalWriter::open(&dir, id, 5).unwrap();
        writer.poison();
        assert!(matches!(
            writer.append([9; 16], upsert("x", &[1.0])),
            Err(WalError::Poisoned)
        ));
        assert!(matches!(writer.sync(), Err(WalError::Poisoned)));
        assert_eq!(writer.next_seq(), 6);
        drop(writer);
        assert_eq!(
            fs::read(&path).unwrap(),
            full,
            "nothing written after poison"
        );
    }

    #[test]
    fn field_limits_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let mut writer = WalWriter::open(&directory.path().join("wal"), id, 0).unwrap();
        assert!(matches!(
            writer.append([0; 16], upsert("", &[1.0])),
            Err(WalError::FieldTooLarge("key"))
        ));
        assert!(matches!(
            writer.append([0; 16], upsert("k", &[])),
            Err(WalError::FieldTooLarge("vector"))
        ));
        assert_eq!(writer.next_seq(), 1, "rejected entries consume no sequence");
    }
}
