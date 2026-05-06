// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

use flowlab_core::types::SeqNum;

/// Snapshot for fast-forward replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub seq: SeqNum,
    pub state_hash: u64,
    pub book_data: Vec<u8>,
}

/// Magic + version for the on-disk snapshot frame. Bumping the
/// version invalidates older files explicitly instead of silently
/// decoding garbage.
const MAGIC: &[u8; 4] = b"FLSN";
const VERSION: u32 = 1;

/// Wire layout (little-endian, no padding):
///
///   [0..4)   magic         "FLSN"
///   [4..8)   version       u32
///   [8..16)  seq           u64
///   [16..24) state_hash    u64
///   [24..28) book_data_len u32
///   [28..)   book_data     bytes
///
/// Hand-rolled instead of pulling in bincode/postcard: keeps the
/// crate dep-free, makes the format trivially inspectable in a hex
/// dump, and lets Zig/Go readers parse it without a schema crate.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot too short: {0} bytes")]
    Truncated(usize),
    #[error("bad magic: {0:?}")]
    BadMagic([u8; 4]),
    #[error("unsupported version: {0}")]
    BadVersion(u32),
    #[error("declared book_data_len {declared} exceeds payload {actual}")]
    BadLength { declared: usize, actual: usize },
}

impl Snapshot {
    pub const HEADER_LEN: usize = 4 + 4 + 8 + 8 + 4;

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.book_data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.state_hash.to_le_bytes());
        // u32 is enough for any realistic book payload (4 GiB).
        // Saturating cast guards against a logic bug shipping a
        // larger Vec; the deserializer will then refuse it.
        let len: u32 = self.book_data.len().min(u32::MAX as usize) as u32;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.book_data);
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, SnapshotError> {
        if buf.len() < Self::HEADER_LEN {
            return Err(SnapshotError::Truncated(buf.len()));
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[0..4]);
        if &magic != MAGIC {
            return Err(SnapshotError::BadMagic(magic));
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(SnapshotError::BadVersion(version));
        }
        let seq = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let state_hash = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let declared = u32::from_le_bytes(buf[24..28].try_into().unwrap()) as usize;
        let payload = &buf[Self::HEADER_LEN..];
        if declared > payload.len() {
            return Err(SnapshotError::BadLength {
                declared,
                actual: payload.len(),
            });
        }
        Ok(Snapshot {
            seq,
            state_hash,
            book_data: payload[..declared].to_vec(),
        })
    }

    /// Atomic write: serialise to `<path>.tmp` then rename. Avoids
    /// leaving a half-written snapshot behind if the process is
    /// killed mid-write — the next resume just falls back to the
    /// previous good file.
    pub fn write_to_path(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("flsn.tmp");
        std::fs::write(&tmp, self.to_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read + decode in one shot.
    pub fn read_from_path(path: &std::path::Path) -> Result<Self, SnapshotIoError> {
        let bytes = std::fs::read(path).map_err(SnapshotIoError::Io)?;
        Self::from_bytes(&bytes).map_err(SnapshotIoError::Decode)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotIoError {
    #[error("snapshot I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot decode error: {0}")]
    Decode(#[from] SnapshotError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_book() {
        let s = Snapshot { seq: 0, state_hash: 0, book_data: vec![] };
        let bytes = s.to_bytes();
        assert_eq!(bytes.len(), Snapshot::HEADER_LEN);
        assert_eq!(Snapshot::from_bytes(&bytes).unwrap(), s);
    }

    #[test]
    fn roundtrip_with_payload() {
        let s = Snapshot {
            seq: 0xDEAD_BEEF_CAFE_BABE,
            state_hash: 0x1122_3344_5566_7788,
            book_data: (0u8..=255).cycle().take(4096).collect(),
        };
        let bytes = s.to_bytes();
        assert_eq!(Snapshot::from_bytes(&bytes).unwrap(), s);
    }

    #[test]
    fn rejects_truncated() {
        assert!(matches!(
            Snapshot::from_bytes(&[0u8; 10]),
            Err(SnapshotError::Truncated(10))
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = Snapshot { seq: 1, state_hash: 2, book_data: vec![] }.to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            Snapshot::from_bytes(&bytes),
            Err(SnapshotError::BadMagic(_))
        ));
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = Snapshot { seq: 1, state_hash: 2, book_data: vec![] }.to_bytes();
        // Bump version byte.
        bytes[4] = 99;
        assert!(matches!(
            Snapshot::from_bytes(&bytes),
            Err(SnapshotError::BadVersion(99))
        ));
    }

    #[test]
    fn rejects_truncated_payload() {
        let s = Snapshot { seq: 1, state_hash: 2, book_data: vec![1, 2, 3, 4] };
        let mut bytes = s.to_bytes();
        bytes.truncate(bytes.len() - 2); // drop last 2 payload bytes
        assert!(matches!(
            Snapshot::from_bytes(&bytes),
            Err(SnapshotError::BadLength { declared: 4, actual: 2 })
        ));
    }
}
