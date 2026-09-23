//! Minimal MessagePack decoder — just enough of the format for the Fluentd
//! forward protocol (`[tag, time, record, option]` msgpack arrays) without
//! pulling in a third-party crate.
//!
//! Supported: nil, bool, int/uint (all widths), float32/64, str/bin
//! (fixstr/8/16/32), array (fix/16/32), map (fix/16/32), and ext
//! (fixext1/2/4/8/16, ext8/16/32) — ext type 0 carries Fluentd's EventTime.

use std::io::Read;

#[derive(Debug, Clone, PartialEq)]
pub enum MsgValue {
    Nil,
    Bool(bool),
    /// Signed integer (any msgpack int decodes into the smallest lossless
    /// form: `Int` for negatives, `Uint` for positives ≥ 0).
    Int(i64),
    Uint(u64),
    F64(f64),
    Str(String),
    Bin(Vec<u8>),
    Array(Vec<MsgValue>),
    /// String keys, values as decoded (msgpack map keys in the forward
    /// protocol are always strings; non-string keys are lossy-converted).
    Map(Vec<(String, MsgValue)>),
    /// (ext type, body)
    Ext(i8, Vec<u8>),
}

impl MsgValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            MsgValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            MsgValue::Uint(u) => Some(*u),
            MsgValue::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            MsgValue::Int(i) => Some(*i),
            MsgValue::Uint(u) => i64::try_from(*u).ok(),
            MsgValue::F64(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            MsgValue::F64(f) => Some(*f),
            MsgValue::Int(i) => Some(*i as f64),
            MsgValue::Uint(u) => Some(*u as f64),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            MsgValue::Bin(b) => Some(b),
            MsgValue::Str(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    /// Stringify scalars (lossy for binary) — used for tags/ack ids that may
    /// arrive as bin instead of str depending on the client.
    pub fn to_string_lossy(&self) -> Option<String> {
        match self {
            MsgValue::Str(s) => Some(s.clone()),
            MsgValue::Bin(b) => Some(String::from_utf8_lossy(b).into_owned()),
            MsgValue::Int(i) => Some(i.to_string()),
            MsgValue::Uint(u) => Some(u.to_string()),
            MsgValue::F64(f) => Some(f.to_string()),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("msgpack decode failed at byte {at}: {kind}")]
pub struct DecodeError {
    pub at: usize,
    pub kind: &'static str,
}

fn err(at: usize, kind: &'static str) -> DecodeError {
    DecodeError { at, kind }
}

/// Decode exactly one value from the reader (blocking on `Read`). Returns
/// `Ok(None)` on a clean EOF before any byte of a new value.
pub fn read_value<R: Read>(r: &mut R) -> Result<Option<MsgValue>, DecodeError> {
    let mut pos = 0usize;
    match read_one(r, &mut pos) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind == "eof" && e.at == 0 => Ok(None),
        Err(e) => Err(e),
    }
}

/// Try to decode one complete value from an in-memory buffer without
/// consuming it. Returns:
/// - `Ok(Some((value, bytes_consumed)))` on success,
/// - `Ok(None)` when the buffer holds no complete value yet (need more
///   bytes — includes truncated values),
/// - `Err` on malformed data (the stream is unrecoverable).
pub fn try_decode(buf: &[u8]) -> Result<Option<(MsgValue, usize)>, DecodeError> {
    if buf.is_empty() {
        return Ok(None);
    }
    let mut cur = std::io::Cursor::new(buf);
    match read_value(&mut cur) {
        Ok(Some(v)) => Ok(Some((v, cur.position() as usize))),
        Ok(None) => Ok(None),
        Err(e) if e.kind == "eof" => Ok(None),
        Err(e) => Err(e),
    }
}

fn byte<R: Read>(r: &mut R, pos: &mut usize) -> Result<u8, DecodeError> {
    let mut b = [0u8; 1];
    match r.read_exact(&mut b) {
        Ok(()) => {
            *pos += 1;
            Ok(b[0])
        }
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && *pos == 0 => Err(err(0, "eof")),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(err(*pos, "eof")),
        Err(_) => Err(err(*pos, "io")),
    }
}

fn exact<R: Read>(r: &mut R, n: usize, pos: &mut usize) -> Result<Vec<u8>, DecodeError> {
    let mut v = vec![0u8; n];
    match r.read_exact(&mut v) {
        Ok(()) => {
            *pos += n;
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(err(*pos, "eof")),
        Err(_) => Err(err(*pos, "io")),
    }
}

fn read_one<R: Read>(r: &mut R, pos: &mut usize) -> Result<MsgValue, DecodeError> {
    let b = byte(r, pos)?;
    match b {
        0xc0 => Ok(MsgValue::Nil),
        0xc2 => Ok(MsgValue::Bool(false)),
        0xc3 => Ok(MsgValue::Bool(true)),

        // Positive fixint
        0x00..=0x7f => Ok(MsgValue::Uint(b as u64)),
        // Negative fixint
        0xe0..=0xff => Ok(MsgValue::Int(b as i8 as i64)),

        // Integers
        0xcc => Ok(MsgValue::Uint(exact(r, 1, pos)?[0] as u64)),
        0xcd => {
            let v = exact(r, 2, pos)?;
            Ok(MsgValue::Uint(u16::from_be_bytes([v[0], v[1]]) as u64))
        }
        0xce => {
            let v = exact(r, 4, pos)?;
            Ok(MsgValue::Uint(
                u32::from_be_bytes(v.try_into().unwrap()) as u64
            ))
        }
        0xcf => {
            let v = exact(r, 8, pos)?;
            Ok(MsgValue::Uint(u64::from_be_bytes(v.try_into().unwrap())))
        }
        0xd0 => Ok(MsgValue::Int(exact(r, 1, pos)?[0] as i8 as i64)),
        0xd1 => {
            let v = exact(r, 2, pos)?;
            Ok(MsgValue::Int(i16::from_be_bytes([v[0], v[1]]) as i64))
        }
        0xd2 => {
            let v = exact(r, 4, pos)?;
            Ok(MsgValue::Int(
                i32::from_be_bytes(v.try_into().unwrap()) as i64
            ))
        }
        0xd3 => {
            let v = exact(r, 8, pos)?;
            Ok(MsgValue::Int(i64::from_be_bytes(v.try_into().unwrap())))
        }

        // Floats
        0xca => {
            let v = exact(r, 4, pos)?;
            Ok(MsgValue::F64(
                f32::from_be_bytes(v.try_into().unwrap()) as f64
            ))
        }
        0xcb => {
            let v = exact(r, 8, pos)?;
            Ok(MsgValue::F64(f64::from_be_bytes(v.try_into().unwrap())))
        }

        // Strings
        0xa0..=0xbf => {
            let n = (b & 0x1f) as usize;
            Ok(MsgValue::Str(lossy(&exact(r, n, pos)?)))
        }
        0xd9 => {
            let n = byte(r, pos)? as usize;
            Ok(MsgValue::Str(lossy(&exact(r, n, pos)?)))
        }
        0xda => {
            let v = exact(r, 2, pos)?;
            let n = u16::from_be_bytes([v[0], v[1]]) as usize;
            Ok(MsgValue::Str(lossy(&exact(r, n, pos)?)))
        }
        0xdb => {
            let v = exact(r, 4, pos)?;
            let n = u32::from_be_bytes(v.try_into().unwrap()) as usize;
            Ok(MsgValue::Str(lossy(&exact(r, n, pos)?)))
        }

        // Binary
        0xc4 => {
            let n = byte(r, pos)? as usize;
            Ok(MsgValue::Bin(exact(r, n, pos)?))
        }
        0xc5 => {
            let v = exact(r, 2, pos)?;
            let n = u16::from_be_bytes([v[0], v[1]]) as usize;
            Ok(MsgValue::Bin(exact(r, n, pos)?))
        }
        0xc6 => {
            let v = exact(r, 4, pos)?;
            let n = u32::from_be_bytes(v.try_into().unwrap()) as usize;
            Ok(MsgValue::Bin(exact(r, n, pos)?))
        }

        // Arrays
        0x90..=0x9f => {
            let n = (b & 0x0f) as usize;
            read_array(r, n, pos)
        }
        0xdc => {
            let v = exact(r, 2, pos)?;
            read_array(r, u16::from_be_bytes([v[0], v[1]]) as usize, pos)
        }
        0xdd => {
            let v = exact(r, 4, pos)?;
            read_array(r, u32::from_be_bytes(v.try_into().unwrap()) as usize, pos)
        }

        // Maps
        0x80..=0x8f => {
            let n = (b & 0x0f) as usize;
            read_map(r, n, pos)
        }
        0xde => {
            let v = exact(r, 2, pos)?;
            read_map(r, u16::from_be_bytes([v[0], v[1]]) as usize, pos)
        }
        0xdf => {
            let v = exact(r, 4, pos)?;
            read_map(r, u32::from_be_bytes(v.try_into().unwrap()) as usize, pos)
        }

        // Extensions (incl. Fluentd EventTime = ext type 0)
        0xd4 => {
            let t = byte(r, pos)? as i8;
            let body = exact(r, 1, pos)?;
            Ok(MsgValue::Ext(t, body))
        }
        0xd5 => {
            let t = byte(r, pos)? as i8;
            let body = exact(r, 2, pos)?;
            Ok(MsgValue::Ext(t, body))
        }
        0xd6 => {
            let t = byte(r, pos)? as i8;
            let body = exact(r, 4, pos)?;
            Ok(MsgValue::Ext(t, body))
        }
        0xd7 => {
            let t = byte(r, pos)? as i8;
            let body = exact(r, 8, pos)?;
            Ok(MsgValue::Ext(t, body))
        }
        0xd8 => {
            let t = byte(r, pos)? as i8;
            let body = exact(r, 16, pos)?;
            Ok(MsgValue::Ext(t, body))
        }
        0xc7 => {
            let n = byte(r, pos)? as usize;
            let t = byte(r, pos)? as i8;
            Ok(MsgValue::Ext(t, exact(r, n, pos)?))
        }
        0xc8 => {
            let v = exact(r, 2, pos)?;
            let n = u16::from_be_bytes([v[0], v[1]]) as usize;
            let t = byte(r, pos)? as i8;
            Ok(MsgValue::Ext(t, exact(r, n, pos)?))
        }
        0xc9 => {
            let v = exact(r, 4, pos)?;
            let n = u32::from_be_bytes(v.try_into().unwrap()) as usize;
            let t = byte(r, pos)? as i8;
            Ok(MsgValue::Ext(t, exact(r, n, pos)?))
        }

        _ => Err(err(*pos, "unsupported marker")),
    }
}

fn read_array<R: Read>(r: &mut R, n: usize, pos: &mut usize) -> Result<MsgValue, DecodeError> {
    if n > 1_048_576 {
        return Err(err(*pos, "array too large"));
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        out.push(read_one(r, pos)?);
    }
    Ok(MsgValue::Array(out))
}

fn read_map<R: Read>(r: &mut R, n: usize, pos: &mut usize) -> Result<MsgValue, DecodeError> {
    if n > 1_048_576 {
        return Err(err(*pos, "map too large"));
    }
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let key = read_one(r, pos)?;
        let val = read_one(r, pos)?;
        out.push((key.to_string_lossy().unwrap_or_default(), val));
    }
    Ok(MsgValue::Map(out))
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8]) -> MsgValue {
        let mut cur = std::io::Cursor::new(bytes);
        read_value(&mut cur).expect("decodes").expect("one value")
    }

    #[test]
    fn scalars() {
        assert_eq!(dec(&[0xc0]), MsgValue::Nil);
        assert_eq!(dec(&[0xc3]), MsgValue::Bool(true));
        assert_eq!(dec(&[0x05]), MsgValue::Uint(5));
        assert_eq!(dec(&[0xff]), MsgValue::Int(-1));
        assert_eq!(dec(&[0xd0, 0x80]), MsgValue::Int(-128));
        assert_eq!(dec(&[0xcd, 0x01, 0x00]), MsgValue::Uint(256));
        assert_eq!(dec(&[0xd2, 0xff, 0xff, 0xf9, 0x7f]), MsgValue::Int(-1665));
        // f64 0x41D0000000000000 = 2^30
        assert_eq!(
            dec(&[0xcb, 0x41, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            MsgValue::F64(1_073_741_824.0)
        );
        assert_eq!(
            dec(&[0xa5, b'h', b'e', b'l', b'l', b'o']),
            MsgValue::Str("hello".into())
        );
        assert_eq!(dec(&[0xc4, 0x02, 0x00, 0xff]), MsgValue::Bin(vec![0, 255]));
    }

    #[test]
    fn containers() {
        // ["a", 1] fixarray
        let v = dec(&[0x92, 0xa1, b'a', 0x01]);
        assert_eq!(
            v,
            MsgValue::Array(vec![MsgValue::Str("a".into()), MsgValue::Uint(1)])
        );
        // {"log": "hi"} fixmap
        let v = dec(&[0x81, 0xa3, b'l', b'o', b'g', 0xa2, b'h', b'i']);
        assert_eq!(
            v,
            MsgValue::Map(vec![("log".into(), MsgValue::Str("hi".into()))])
        );
    }

    #[test]
    fn fluentd_event_time_ext() {
        // fixext8, type 0, 2026-09-23T00:00:00Z = 1758585600s + 500ms
        let secs: u32 = 1_758_585_600;
        let nans: u32 = 500_000_000;
        let mut b = vec![0xd7, 0x00];
        b.extend_from_slice(&secs.to_be_bytes());
        b.extend_from_slice(&nans.to_be_bytes());
        let v = dec(&b);
        match v {
            MsgValue::Ext(0, body) => {
                assert_eq!(body.len(), 8);
            }
            other => panic!("expected ext, got {other:?}"),
        }
    }

    #[test]
    fn stream_of_values_decodes_back_to_back() {
        // Two concatenated values: "abc" + 42
        let mut b = vec![0xa3, b'a', b'b', b'c', 0x2a];
        let mut cur = std::io::Cursor::new(&b);
        assert_eq!(
            read_value(&mut cur).unwrap().unwrap(),
            MsgValue::Str("abc".into())
        );
        assert_eq!(read_value(&mut cur).unwrap().unwrap(), MsgValue::Uint(42));
        assert!(read_value(&mut cur).unwrap().is_none(), "clean EOF");
        let _ = &mut b;
    }

    #[test]
    fn truncated_value_errors() {
        // str16 header but missing payload
        let b = [0xda, 0x00, 0x05, b'a'];
        let mut cur = std::io::Cursor::new(&b[..]);
        assert!(read_value(&mut cur).is_err());
    }
}
