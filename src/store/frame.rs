//! Checksummed record framing shared by the WAL, snapshots, the audit trail
//! and the idempotency store.
//!
//! Layout of one frame (all integers little endian):
//!
//! ```text
//! +--------+--------+-----------+--------+-----------------+
//! | magic  | length | sequence  | crc32  | payload (len B) |
//! | u32    | u32    | u64       | u32    |                 |
//! +--------+--------+-----------+--------+-----------------+
//! ```
//!
//! The CRC32 covers the sequence number, the length and the payload, so a
//! frame whose header was partially overwritten is detected as well.
//!
//! Scanning distinguishes a *torn tail* (an incomplete frame at the very end
//! of a file, the signature of a crash mid-write) from *corruption* (an
//! invalid frame followed by more data), because only the former is safe to
//! repair automatically by truncation.

use std::ops::Range;

/// Size of the fixed frame header in bytes.
pub const FRAME_HEADER_LEN: usize = 20;

/// Magic number for WAL frames ("LWAL").
pub const WAL_MAGIC: u32 = u32::from_le_bytes(*b"LWAL");
/// Magic number for snapshot frames ("LSNP").
pub const SNAPSHOT_MAGIC: u32 = u32::from_le_bytes(*b"LSNP");
/// Magic number for audit-trail frames ("LAUD").
pub const AUDIT_MAGIC: u32 = u32::from_le_bytes(*b"LAUD");
/// Magic number for idempotency-store frames ("LIDM").
pub const IDEMPOTENCY_MAGIC: u32 = u32::from_le_bytes(*b"LIDM");

fn checksum(sequence: u64, payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&sequence.to_le_bytes());
    hasher.update(&(payload.len() as u32).to_le_bytes());
    hasher.update(payload);
    hasher.finalize()
}

/// Encodes a payload into a frame.
///
/// # Panics
/// Panics if the payload is larger than `u32::MAX` bytes; callers enforce far
/// smaller limits before encoding.
pub fn encode(magic: u32, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("frame payload exceeds u32::MAX");
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&checksum(sequence, payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A successfully decoded frame, referencing the payload inside the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub sequence: u64,
    pub offset: u64,
    pub payload: Range<usize>,
}

/// State of the bytes following the last valid frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tail {
    /// The buffer ended exactly at a frame boundary.
    Clean,
    /// An incomplete or checksum-failing frame at the very end of the buffer.
    Torn { offset: u64, reason: String },
    /// An invalid frame with further data after it.
    Corrupt { offset: u64, reason: String },
}

/// Result of scanning a buffer of frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub frames: Vec<Frame>,
    /// Number of bytes covered by valid frames from the start of the buffer.
    pub valid_len: u64,
    pub tail: Tail,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.tail == Tail::Clean
    }
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("slice of 4 bytes"))
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("slice of 8 bytes"))
}

/// Scans `buf` for consecutive frames with the given magic.
///
/// Scanning stops at the first invalid frame. Frames larger than
/// `max_payload` are treated as invalid.
pub fn scan(buf: &[u8], magic: u32, max_payload: usize) -> Scan {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    let total = buf.len();
    let tail = loop {
        if pos == total {
            break Tail::Clean;
        }
        let remaining = &buf[pos..];
        if remaining.iter().all(|b| *b == 0) {
            break Tail::Torn {
                offset: pos as u64,
                reason: format!("{} zero bytes after last frame", remaining.len()),
            };
        }
        if remaining.len() < FRAME_HEADER_LEN {
            break Tail::Torn {
                offset: pos as u64,
                reason: format!("incomplete header ({} bytes)", remaining.len()),
            };
        }
        let found_magic = read_u32(buf, pos);
        if found_magic != magic {
            break Tail::Corrupt {
                offset: pos as u64,
                reason: format!("bad magic {found_magic:#010x}"),
            };
        }
        let len = read_u32(buf, pos + 4) as usize;
        let sequence = read_u64(buf, pos + 8);
        let crc = read_u32(buf, pos + 16);
        if len > max_payload {
            break Tail::Corrupt {
                offset: pos as u64,
                reason: format!("payload length {len} exceeds limit {max_payload}"),
            };
        }
        let start = pos + FRAME_HEADER_LEN;
        let end = start + len;
        if end > total {
            break Tail::Torn {
                offset: pos as u64,
                reason: format!(
                    "incomplete payload: need {len} bytes, have {}",
                    total - start
                ),
            };
        }
        if checksum(sequence, &buf[start..end]) != crc {
            let reason = format!("checksum mismatch for sequence {sequence}");
            break if end == total {
                Tail::Torn {
                    offset: pos as u64,
                    reason,
                }
            } else {
                Tail::Corrupt {
                    offset: pos as u64,
                    reason,
                }
            };
        }
        frames.push(Frame {
            sequence,
            offset: pos as u64,
            payload: start..end,
        });
        pos = end;
    };
    Scan {
        frames,
        valid_len: pos as u64,
        tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn build(payloads: &[&[u8]]) -> Vec<u8> {
        payloads
            .iter()
            .enumerate()
            .flat_map(|(i, p)| encode(WAL_MAGIC, i as u64 + 1, p))
            .collect()
    }

    #[test]
    fn roundtrip_multiple_frames() {
        let buf = build(&[b"one", b"", b"three"]);
        let scan = scan(&buf, WAL_MAGIC, 1024);
        assert!(scan.is_clean());
        assert_eq!(scan.frames.len(), 3);
        assert_eq!(&buf[scan.frames[2].payload.clone()], b"three");
        assert_eq!(scan.frames[1].sequence, 2);
        assert_eq!(scan.valid_len, buf.len() as u64);
    }

    #[test]
    fn truncated_payload_is_torn() {
        let buf = build(&[b"hello", b"world"]);
        let cut = &buf[..buf.len() - 2];
        let s = scan(cut, WAL_MAGIC, 1024);
        assert_eq!(s.frames.len(), 1);
        assert!(matches!(s.tail, Tail::Torn { .. }));
        assert_eq!(s.valid_len, (FRAME_HEADER_LEN + 5) as u64);
    }

    #[test]
    fn truncated_header_is_torn() {
        let buf = build(&[b"hello"]);
        let mut cut = buf.clone();
        cut.extend_from_slice(&encode(WAL_MAGIC, 2, b"x")[..7]);
        let s = scan(&cut, WAL_MAGIC, 1024);
        assert_eq!(s.frames.len(), 1);
        assert!(matches!(s.tail, Tail::Torn { .. }));
    }

    #[test]
    fn checksum_failure_at_end_is_torn_but_in_middle_is_corrupt() {
        let mut buf = build(&[b"aaaa", b"bbbb"]);
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;
        assert!(matches!(scan(&buf, WAL_MAGIC, 64).tail, Tail::Torn { .. }));

        let mut buf = build(&[b"aaaa", b"bbbb"]);
        buf[FRAME_HEADER_LEN] ^= 0xFF;
        let s = scan(&buf, WAL_MAGIC, 64);
        assert!(matches!(s.tail, Tail::Corrupt { .. }));
        assert!(s.frames.is_empty());
    }

    #[test]
    fn zero_filled_tail_is_torn() {
        let mut buf = build(&[b"abc"]);
        buf.extend_from_slice(&[0u8; 64]);
        let s = scan(&buf, WAL_MAGIC, 64);
        assert_eq!(s.frames.len(), 1);
        assert!(matches!(s.tail, Tail::Torn { .. }));
    }

    #[test]
    fn wrong_magic_and_oversized_frames_are_corrupt() {
        let buf = encode(AUDIT_MAGIC, 1, b"abc");
        assert!(matches!(
            scan(&buf, WAL_MAGIC, 64).tail,
            Tail::Corrupt { .. }
        ));
        let buf = encode(WAL_MAGIC, 1, &[1u8; 100]);
        assert!(matches!(
            scan(&buf, WAL_MAGIC, 10).tail,
            Tail::Corrupt { .. }
        ));
    }

    #[test]
    fn header_tampering_is_detected() {
        let mut buf = build(&[b"payload", b"next"]);
        // Flip a bit in the sequence number of the first frame.
        buf[8] ^= 0x01;
        assert!(matches!(
            scan(&buf, WAL_MAGIC, 64).tail,
            Tail::Corrupt { .. }
        ));
    }

    proptest! {
        #[test]
        fn any_prefix_scans_without_panicking(payloads in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 1..8), cut in 0usize..600) {
            let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
            let buf = build(&refs);
            let cut = cut.min(buf.len());
            let s = scan(&buf[..cut], WAL_MAGIC, 1024);
            // Every frame found must be one of the originals, in order.
            for (i, f) in s.frames.iter().enumerate() {
                prop_assert_eq!(&buf[f.payload.clone()], payloads[i].as_slice());
            }
            prop_assert!(s.valid_len as usize <= cut);
            if cut == buf.len() {
                prop_assert!(s.is_clean());
            } else {
                let corrupt = matches!(s.tail, Tail::Corrupt { .. });
                prop_assert!(!corrupt);
            }
        }
    }
}
