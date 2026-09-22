//! Frame encoding/decoding for the WAL.

use bytes::BufMut;

use crate::{Error, Protocol, RawRecord, Result};

/// 4 (len) + 4 (crc) header before each payload.
pub const FRAME_HEADER_LEN: usize = 8;

/// Maximum payload size we'll accept (256 MiB). Guards against malicious length
/// fields causing OOM.
pub const MAX_PAYLOAD_LEN: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub payload_len: u32,
    pub crc: u32,
}

/// Encode a [`RawRecord`] into the writer as a complete frame (header + payload).
pub fn encode_record_into(buf: &mut Vec<u8>, rec: &RawRecord) {
    let payload_start = buf.len();
    buf.reserve(FRAME_HEADER_LEN + 64);

    // Placeholder header (will be filled once payload length and crc are known).
    buf.put_u32_le(0);
    buf.put_u32_le(0);

    // Payload body.
    let receive_ts_nanos = rec.receive_ts.timestamp_nanos_opt().unwrap_or(i64::MAX);
    buf.put_i64_le(receive_ts_nanos);
    buf.put_u8(protocol_tag(rec.protocol));
    let addr_bytes = rec.source_addr.as_bytes();
    assert!(
        addr_bytes.len() <= u16::MAX as usize,
        "source_addr too long"
    );
    buf.put_u16_le(addr_bytes.len() as u16);
    buf.extend_from_slice(addr_bytes);
    let raw_len = rec.raw.len();
    assert!(
        raw_len <= MAX_PAYLOAD_LEN,
        "raw payload exceeds MAX_PAYLOAD_LEN"
    );
    buf.put_u32_le(raw_len as u32);
    buf.extend_from_slice(&rec.raw);

    // Compute crc over payload body only, then patch the header.
    let payload_end = buf.len();
    let mut h = crc32fast::Hasher::new();
    h.update(&buf[payload_start + FRAME_HEADER_LEN..payload_end]);
    let crc = h.finalize();
    let payload_len = (payload_end - payload_start - FRAME_HEADER_LEN) as u32;

    buf[payload_start..payload_start + 4].copy_from_slice(&payload_len.to_le_bytes());
    buf[payload_start + 4..payload_start + 8].copy_from_slice(&crc.to_le_bytes());
}

fn protocol_tag(p: Protocol) -> u8 {
    match p {
        Protocol::HttpJson => 1,
        Protocol::SyslogUdp => 2,
        Protocol::SyslogTcp => 3,
        Protocol::Sentry => 4,
        Protocol::OtlpLog => 5,
        Protocol::OtlpSpan => 6,
        Protocol::OtlpMetric => 7,
    }
}

fn protocol_from_tag(tag: u8) -> Result<Protocol> {
    Ok(match tag {
        1 => Protocol::HttpJson,
        2 => Protocol::SyslogUdp,
        3 => Protocol::SyslogTcp,
        4 => Protocol::Sentry,
        5 => Protocol::OtlpLog,
        6 => Protocol::OtlpSpan,
        7 => Protocol::OtlpMetric,
        other => {
            return Err(Error::invalid_input(format!(
                "unknown protocol tag {other}"
            )))
        }
    })
}

/// Decode a complete payload (header already validated/consumed) into a record.
pub fn decode_payload(payload: &[u8]) -> Result<RawRecord> {
    if payload.len() < 8 + 1 + 2 + 4 {
        return Err(Error::invalid_input("payload too short"));
    }
    let mut cur = payload;
    let ts_nanos = read_i64_le(&mut cur);
    let proto_tag = read_u8(&mut cur);
    let protocol = protocol_from_tag(proto_tag)?;
    let addr_len = read_u16_le(&mut cur) as usize;
    if cur.len() < addr_len + 4 {
        return Err(Error::invalid_input("truncated addr/raw"));
    }
    let source_addr = std::str::from_utf8(&cur[..addr_len])
        .map_err(|e| Error::invalid_input(format!("source_addr not utf8: {e}")))?
        .to_string();
    cur = &cur[addr_len..];
    let raw_len = read_u32_le(&mut cur) as usize;
    if cur.len() != raw_len {
        return Err(Error::invalid_input(format!(
            "raw_len mismatch: declared {raw_len}, got {}",
            cur.len()
        )));
    }
    let raw = bytes::Bytes::copy_from_slice(cur);
    let receive_ts = chrono::DateTime::from_timestamp_nanos(ts_nanos);
    Ok(RawRecord {
        receive_ts,
        source_addr,
        protocol,
        raw,
    })
}

#[inline]
fn read_i64_le(buf: &mut &[u8]) -> i64 {
    let v = i64::from_le_bytes(buf[..8].try_into().unwrap());
    *buf = &buf[8..];
    v
}
#[inline]
fn read_u8(buf: &mut &[u8]) -> u8 {
    let v = buf[0];
    *buf = &buf[1..];
    v
}
#[inline]
fn read_u16_le(buf: &mut &[u8]) -> u16 {
    let v = u16::from_le_bytes(buf[..2].try_into().unwrap());
    *buf = &buf[2..];
    v
}
#[inline]
fn read_u32_le(buf: &mut &[u8]) -> u32 {
    let v = u32::from_le_bytes(buf[..4].try_into().unwrap());
    *buf = &buf[4..];
    v
}

/// Parse a frame header from a fixed-size byte slice.
pub fn parse_header(buf: &[u8; FRAME_HEADER_LEN]) -> FrameHeader {
    FrameHeader {
        payload_len: u32::from_le_bytes(buf[..4].try_into().unwrap()),
        crc: u32::from_le_bytes(buf[4..].try_into().unwrap()),
    }
}

/// Decode a single frame given the raw header + payload bytes. Returns the
/// parsed record and an offset (relative to the start of `header_and_payload`)
/// suitable for checkpointing.
pub fn decode_frame(header_and_payload: &[u8]) -> Result<RawRecord> {
    if header_and_payload.len() < FRAME_HEADER_LEN {
        return Err(Error::WalTruncated(header_and_payload.len() as u64));
    }
    let header = parse_header(header_and_payload[..FRAME_HEADER_LEN].try_into().unwrap());
    let payload = &header_and_payload[FRAME_HEADER_LEN..];
    if payload.len() != header.payload_len as usize {
        return Err(Error::WalTruncated(header_and_payload.len() as u64));
    }
    let mut h = crc32fast::Hasher::new();
    h.update(payload);
    let actual = h.finalize();
    if actual != header.crc {
        return Err(Error::WalCrc {
            expected: header.crc,
            got: actual,
            offset: 0,
        });
    }
    decode_payload(payload)
}

/// Iterator that yields records from a contiguous byte buffer of frames.
pub struct FrameIter<'a> {
    remaining: &'a [u8],
}

impl<'a> FrameIter<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { remaining: buf }
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<RawRecord>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        if self.remaining.len() < FRAME_HEADER_LEN {
            return Some(Err(Error::WalTruncated(self.remaining.len() as u64)));
        }
        let header = parse_header(self.remaining[..FRAME_HEADER_LEN].try_into().unwrap());
        let end = FRAME_HEADER_LEN + header.payload_len as usize;
        if self.remaining.len() < end {
            return Some(Err(Error::WalTruncated(self.remaining.len() as u64)));
        }
        let frame = &self.remaining[..end];
        self.remaining = &self.remaining[end..];
        Some(decode_frame(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(proto: Protocol) -> RawRecord {
        RawRecord {
            receive_ts: chrono::Utc::now(),
            source_addr: "10.0.0.1:5555".into(),
            protocol: proto,
            raw: bytes::Bytes::from_static(b"{\"level\":\"info\",\"msg\":\"hi\"}"),
        }
    }

    #[test]
    fn roundtrip_all_protocols() {
        for p in [
            Protocol::HttpJson,
            Protocol::SyslogUdp,
            Protocol::SyslogTcp,
            Protocol::Sentry,
            Protocol::OtlpLog,
            Protocol::OtlpSpan,
            Protocol::OtlpMetric,
        ] {
            let rec = sample(p);
            let mut buf = Vec::new();
            encode_record_into(&mut buf, &rec);
            let decoded = decode_frame(&buf).expect("decode");
            assert_eq!(decoded.protocol, rec.protocol);
            assert_eq!(decoded.source_addr, rec.source_addr);
            assert_eq!(decoded.raw, rec.raw);
            assert_eq!(decoded.receive_ts, rec.receive_ts);
        }
    }

    #[test]
    fn frame_iter_decodes_multiple() {
        let mut buf = Vec::new();
        for _ in 0..5 {
            encode_record_into(&mut buf, &sample(Protocol::HttpJson));
        }
        let n = FrameIter::new(&buf).take_while(Result::is_ok).count();
        assert_eq!(n, 5);
    }

    #[test]
    fn crc_mismatch_errors() {
        let rec = sample(Protocol::HttpJson);
        let mut buf = Vec::new();
        encode_record_into(&mut buf, &rec);
        buf[12] ^= 0xFF; // flip a payload byte
        assert!(matches!(decode_frame(&buf), Err(Error::WalCrc { .. })));
    }
}
