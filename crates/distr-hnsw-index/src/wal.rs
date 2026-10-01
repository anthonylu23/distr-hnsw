//! Write-ahead log segments (contract §4). Every acknowledged operation is an
//! entry with a CRC32C, a strictly increasing sequence number, and an
//! operation id. A damaged final entry is a torn tail and is truncated; any
//! other damage fails closed.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;

pub const WAL_MAGIC: &[u8; 4] = b"DHWL";
pub const WAL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
/// Rotate to a new segment once the current one exceeds this size.
pub const SEGMENT_ROTATE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_KEY_LEN: usize = 512;
const MAX_PAYLOAD_LEN: usize = 64 * 1024;
const MAX_DIMS: usize = 1 << 16;

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
    /// Set when a torn tail was found and removed: `(offset, bytes dropped)`.
    pub torn_tail: Option<(u64, u64)>,
}

/// Read a whole segment, validating the header, every CRC, and sequence
/// continuity starting at `expected_first_seq` (None = use the header).
/// A torn final entry is truncated from the file.
pub fn read_segment(
    path: &Path,
    partition_id: Uuid,
    repair: bool,
) -> Result<SegmentRead, WalError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let first_seq = decode_header(&bytes, partition_id, path)?;
    let mut offset = HEADER_LEN;
    let mut entries = Vec::new();
    let mut expected = first_seq;
    let mut torn_tail = None;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        // Decide whether this is a torn tail (only legal at the very end) or
        // mid-segment corruption.
        let parsed = parse_entry(remaining);
        match parsed {
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
            Err(reason) => {
                // Anything after this point would also be unreadable; it is a
                // torn tail only if we cannot even see a complete frame.
                let frame_complete = remaining.len() >= 8 && {
                    let len = u32::from_le_bytes(remaining[0..4].try_into().expect("4")) as usize;
                    remaining.len() >= 4 + len
                };
                if frame_complete && !matches!(reason, WalError::Malformed("crc")) {
                    return Err(WalError::Corrupt {
                        path: path.to_owned(),
                        offset: offset as u64,
                        reason: reason.to_string(),
                    });
                }
                if frame_complete {
                    // A complete frame with a bad CRC: torn only if it is the
                    // last frame in the file.
                    let len = u32::from_le_bytes(remaining[0..4].try_into().expect("4")) as usize;
                    if remaining.len() != 4 + len {
                        return Err(WalError::Corrupt {
                            path: path.to_owned(),
                            offset: offset as u64,
                            reason: "bad crc before end of segment".to_owned(),
                        });
                    }
                }
                torn_tail = Some((offset as u64, (bytes.len() - offset) as u64));
                break;
            }
        }
    }
    if let (Some((cut, _)), true) = (torn_tail, repair) {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(cut)?;
        file.sync_all()?;
    }
    Ok(SegmentRead { entries, torn_tail })
}

fn parse_entry(bytes: &[u8]) -> Result<(Entry, usize), WalError> {
    if bytes.len() < 8 {
        return Err(WalError::Malformed("short frame"));
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().expect("4")) as usize;
    if len < 4 + 8 + 16 + 1 {
        return Err(WalError::Malformed("frame length"));
    }
    if bytes.len() < 4 + len {
        return Err(WalError::Malformed("short frame"));
    }
    let crc = u32::from_le_bytes(bytes[4..8].try_into().expect("4"));
    let body = &bytes[8..4 + len];
    if crc32c::crc32c(body) != crc {
        return Err(WalError::Malformed("crc"));
    }
    Ok((Entry::decode(body)?, 4 + len))
}

/// Appends entries to the active segment, rotating by size, syncing before
/// every acknowledgement.
pub struct WalWriter {
    directory: PathBuf,
    partition_id: Uuid,
    file: File,
    path: PathBuf,
    bytes: u64,
    next_seq: u64,
}

impl WalWriter {
    /// Open the writer positioned after `last_seq`. If the newest segment is
    /// below the rotation size it is extended, otherwise a new one starts.
    pub fn open(directory: &Path, partition_id: Uuid, last_seq: u64) -> Result<Self, WalError> {
        fs::create_dir_all(directory)?;
        let next_seq = last_seq + 1;
        let newest = list_segments(directory)?.pop();
        let (path, bytes) = match newest {
            // The newest segment is extended when it starts at or before the
            // next sequence (recovery read it to its end) and has room.
            Some((first, path))
                if first <= next_seq && fs::metadata(&path)?.len() < SEGMENT_ROTATE_BYTES =>
            {
                let len = fs::metadata(&path)?.len();
                (path, len)
            }
            _ => {
                let path = directory.join(segment_name(next_seq));
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)?;
                file.write_all(&encode_header(partition_id, next_seq))?;
                file.sync_all()?;
                File::open(directory)?.sync_all()?;
                (path, HEADER_LEN as u64)
            }
        };
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            directory: directory.to_owned(),
            partition_id,
            file,
            path,
            bytes,
            next_seq,
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
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
        if self.bytes >= SEGMENT_ROTATE_BYTES {
            self.rotate()?;
        }
        let seq = self.next_seq;
        let entry = Entry {
            seq,
            op_id,
            operation,
        };
        let frame = entry.encode()?;
        self.file.write_all(&frame)?;
        self.bytes += frame.len() as u64;
        self.next_seq += 1;
        Ok(seq)
    }

    pub fn sync(&mut self) -> Result<(), WalError> {
        self.file.sync_data()?;
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), WalError> {
        self.file.sync_all()?;
        let path = self.directory.join(segment_name(self.next_seq));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(&encode_header(self.partition_id, self.next_seq))?;
        file.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        self.file = OpenOptions::new().append(true).open(&path)?;
        self.path = path;
        self.bytes = HEADER_LEN as u64;
        Ok(())
    }

    pub fn current_segment(&self) -> &Path {
        &self.path
    }
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

    #[test]
    fn torn_tail_is_truncated_but_mid_segment_damage_fails_closed() {
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

        // Cut the last entry in half: torn tail, truncated on read.
        let cut = full.len() - 20;
        fs::write(&path, &full[..cut]).unwrap();
        let read = read_segment(&path, id, true).unwrap();
        assert_eq!(read.entries.len(), 4);
        assert!(read.torn_tail.is_some());
        let repaired = fs::read(&path).unwrap();
        assert!(repaired.len() < cut);
        let again = read_segment(&path, id, true).unwrap();
        assert_eq!(again.entries.len(), 4);
        assert!(again.torn_tail.is_none());

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
        let mut version = full;
        version[4] = 9;
        fs::write(&path, &version).unwrap();
        assert!(matches!(
            read_segment(&path, id, true),
            Err(WalError::UnsupportedVersion(9))
        ));
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
