//! Per-block wire compression for StreamFS transfers.
//!
//! A block travels as one self-describing frame:
//! `"SFZ1" | algo u8 | raw_len u32 LE | body_len u32 LE | crc32(raw) u32 LE | body`.
//! Only the transfer is compressed: the client decodes into the exact raw bytes it asked for,
//! so caches, offsets and the files Steam sees never change. Decoding is bounded (raw_len is
//! checked against a caller limit before any allocation) and the CRC of the decoded bytes must
//! match, so a malformed or truncated frame is an error, never a wrong read.

use std::time::Instant;

pub const MAGIC: &[u8; 4] = b"SFZ1";
pub const HEADER: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Algo {
    Raw,
    Lz4,
    Zstd(i32),
}

impl Algo {
    pub fn id(self) -> u8 {
        match self {
            Algo::Raw => 0,
            Algo::Lz4 => 1,
            Algo::Zstd(l) => 0x10 + l.clamp(1, 19) as u8,
        }
    }
    pub fn from_id(id: u8) -> Option<Algo> {
        match id {
            0 => Some(Algo::Raw),
            1 => Some(Algo::Lz4),
            0x11..=0x23 => Some(Algo::Zstd((id - 0x10) as i32)),
            _ => None,
        }
    }
    pub fn name(self) -> String {
        match self {
            Algo::Raw => "raw".into(),
            Algo::Lz4 => "lz4".into(),
            Algo::Zstd(l) => format!("zstd{l}"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    #[error("frame too short or bad magic")]
    BadHeader,
    #[error("unknown algorithm id {0}")]
    UnknownAlgo(u8),
    #[error("raw_len {0} exceeds limit {1}")]
    TooLarge(u32, usize),
    #[error("body length mismatch: header {0}, actual {1}")]
    BodyLen(u32, usize),
    #[error("decompression failed: {0}")]
    Decode(String),
    #[error("decoded {0} bytes, expected {1}")]
    RawLen(usize, u32),
    #[error("crc mismatch")]
    Crc,
}

/// Raw compressed body (no frame).
pub fn compress_body(algo: Algo, raw: &[u8]) -> Vec<u8> {
    match algo {
        Algo::Raw => raw.to_vec(),
        Algo::Lz4 => lz4_flex::block::compress(raw),
        Algo::Zstd(l) => zstd::bulk::compress(raw, l).expect("zstd compress of in-memory buffer"),
    }
}

/// Decodes a bare body into exactly `raw_len` bytes; never allocates more than `raw_len`.
pub fn decompress_body(algo: Algo, body: &[u8], raw_len: usize) -> Result<Vec<u8>, CodecError> {
    let out = match algo {
        Algo::Raw => body.to_vec(),
        Algo::Lz4 => {
            let mut out = vec![0u8; raw_len];
            let n = lz4_flex::block::decompress_into(body, &mut out).map_err(|e| CodecError::Decode(e.to_string()))?;
            out.truncate(n);
            out
        }
        Algo::Zstd(_) => zstd::bulk::decompress(body, raw_len).map_err(|e| CodecError::Decode(e.to_string()))?,
    };
    if out.len() != raw_len {
        return Err(CodecError::RawLen(out.len(), raw_len as u32));
    }
    Ok(out)
}

/// Encodes with `algo`, falling back to RAW when the body would not be at least `min_gain`
/// (e.g. 0.05 = 5%) smaller than the input. Returns the frame and the algorithm actually used.
pub fn encode(algo: Algo, raw: &[u8], min_gain: f64) -> (Vec<u8>, Algo) {
    let mut used = algo;
    let mut body = if algo == Algo::Raw { Vec::new() } else { compress_body(algo, raw) };
    if algo == Algo::Raw || body.len() as f64 > raw.len() as f64 * (1.0 - min_gain) {
        used = Algo::Raw;
        body = raw.to_vec();
    }
    let mut f = Vec::with_capacity(HEADER + body.len());
    f.extend_from_slice(MAGIC);
    f.push(used.id());
    f.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32fast::hash(raw).to_le_bytes());
    f.extend_from_slice(&body);
    (f, used)
}

/// Decodes a frame, refusing any declared raw size above `max_raw`.
pub fn decode(frame: &[u8], max_raw: usize) -> Result<(Vec<u8>, Algo), CodecError> {
    if frame.len() < HEADER || &frame[..4] != MAGIC {
        return Err(CodecError::BadHeader);
    }
    let algo = Algo::from_id(frame[4]).ok_or(CodecError::UnknownAlgo(frame[4]))?;
    let u32at = |i: usize| u32::from_le_bytes(frame[i..i + 4].try_into().unwrap());
    let (raw_len, body_len, crc) = (u32at(5), u32at(9), u32at(13));
    if raw_len as usize > max_raw {
        return Err(CodecError::TooLarge(raw_len, max_raw));
    }
    let body = &frame[HEADER..];
    if body.len() != body_len as usize {
        return Err(CodecError::BodyLen(body_len, body.len()));
    }
    if algo == Algo::Raw && body_len != raw_len {
        return Err(CodecError::RawLen(body.len(), raw_len));
    }
    let raw = decompress_body(algo, body, raw_len as usize)?;
    if crc32fast::hash(&raw) != crc {
        return Err(CodecError::Crc);
    }
    Ok((raw, algo))
}

/// Decodes a stream of back-to-back frames (one per server chunk) and checks the total size.
/// Every frame is bounded by `max_raw`; the sum must be exactly `expect` bytes.
pub fn decode_stream(mut buf: &[u8], max_raw: usize, expect: usize) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::with_capacity(expect);
    while !buf.is_empty() {
        if buf.len() < HEADER || &buf[..4] != MAGIC {
            return Err(CodecError::BadHeader);
        }
        let body_len = u32::from_le_bytes(buf[9..13].try_into().unwrap()) as usize;
        let n = HEADER.checked_add(body_len).filter(|&n| n <= buf.len()).ok_or(CodecError::BodyLen(body_len as u32, buf.len() - HEADER))?;
        let (raw, _) = decode(&buf[..n], max_raw)?;
        if out.len() + raw.len() > expect {
            return Err(CodecError::RawLen(out.len() + raw.len(), expect as u32));
        }
        out.extend_from_slice(&raw);
        buf = &buf[n..];
    }
    if out.len() != expect {
        return Err(CodecError::RawLen(out.len(), expect as u32));
    }
    Ok(out)
}

/// Seconds spent in `f`, for benchmarks.
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        (0..300_000u32).map(|i| ((i / 7) % 251) as u8).collect()
    }

    #[test]
    fn roundtrip_every_algo() {
        let raw = sample();
        for a in [Algo::Raw, Algo::Lz4, Algo::Zstd(1), Algo::Zstd(3)] {
            let (f, used) = encode(a, &raw, 0.05);
            assert_eq!(used, a);
            let (back, got) = decode(&f, 1 << 20).unwrap();
            assert_eq!(back, raw);
            assert_eq!(got, a);
        }
    }

    #[test]
    fn incompressible_falls_back_to_raw() {
        let mut s = 1u64;
        let raw: Vec<u8> = (0..100_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect();
        let (f, used) = encode(Algo::Zstd(3), &raw, 0.05);
        assert_eq!(used, Algo::Raw);
        assert_eq!(f.len(), HEADER + raw.len());
        assert_eq!(decode(&f, 1 << 20).unwrap().0, raw);
    }

    #[test]
    fn stream_of_frames() {
        let raw = sample();
        let (a, b) = raw.split_at(100_000);
        let mut s = encode(Algo::Zstd(1), a, 0.05).0;
        s.extend(encode(Algo::Raw, b, 0.05).0);
        assert_eq!(decode_stream(&s, 1 << 20, raw.len()).unwrap(), raw);
        assert!(decode_stream(&s, 1 << 20, raw.len() - 1).is_err());
        assert!(decode_stream(&s, 1 << 20, raw.len() + 1).is_err());
        assert!(decode_stream(&s[..s.len() - 3], 1 << 20, raw.len()).is_err());
        assert_eq!(decode_stream(&[], 1 << 20, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let raw = sample();
        let (f, _) = encode(Algo::Lz4, &raw, 0.05);
        assert_eq!(decode(&f[..10], 1 << 20), Err(CodecError::BadHeader));
        assert_eq!(decode(&f, 1000), Err(CodecError::TooLarge(raw.len() as u32, 1000)));
        assert!(matches!(decode(&f[..f.len() - 1], 1 << 20), Err(CodecError::BodyLen(..))));
        let mut bad = f.clone();
        bad[4] = 0x7f;
        assert_eq!(decode(&bad, 1 << 20), Err(CodecError::UnknownAlgo(0x7f)));
        // flipped body byte: either the decoder or the CRC must catch it
        let mut bad = f.clone();
        let mid = HEADER + (f.len() - HEADER) / 2;
        bad[mid] ^= 0x55;
        assert!(decode(&bad, 1 << 20).is_err());
        // a "decompression bomb": header claims small raw_len, body expands larger -> error, bounded alloc
        let big = vec![0u8; 4 << 20];
        let (mut bomb, _) = encode(Algo::Zstd(3), &big, 0.05);
        bomb[5..9].copy_from_slice(&(1000u32).to_le_bytes());
        assert!(decode(&bomb, 1 << 20).is_err());
        // wrong crc
        let mut bad = f;
        bad[13] ^= 1;
        assert_eq!(decode(&bad, 1 << 20), Err(CodecError::Crc));
    }
}
