//! Annex B H.264 byte stream -> access units for WebCodecs.
//!
//! WebCodecs decodes one access unit per EncodedVideoChunk. Without a `description`
//! the decoder expects Annex B input with SPS/PPS in-band, which is what the
//! fixture (and VideoToolbox output after conversion) carries, so access units are
//! passed through byte-for-byte, start codes included.

use std::fmt;

pub const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ACCESS_UNIT_BYTES: usize = 1024 * 1024;
pub const MAX_ACCESS_UNITS: usize = 10_000;

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub key: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// RFC 6381 codec string from the first SPS, e.g. "avc1.4D001E".
    pub codec: String,
    pub units: Vec<AccessUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    TooLarge(usize),
    NoStartCode,
    NoSps,
    FirstUnitNotKey,
    UnitTooLarge(usize),
    TooManyUnits,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooLarge(n) => write!(f, "stream is {n} bytes; limit is {MAX_STREAM_BYTES}"),
            Error::NoStartCode => write!(f, "no Annex B start code found"),
            Error::NoSps => write!(f, "no SPS before the first slice"),
            Error::FirstUnitNotKey => write!(f, "first access unit is not an IDR keyframe"),
            Error::UnitTooLarge(n) => write!(f, "access unit is {n} bytes; limit is {MAX_ACCESS_UNIT_BYTES}"),
            Error::TooManyUnits => write!(f, "more than {MAX_ACCESS_UNITS} access units"),
        }
    }
}

impl std::error::Error for Error {}

/// Byte ranges of NAL units, each starting at its start code.
fn nal_ranges(data: &[u8]) -> Vec<(usize, usize, usize)> {
    // (start code begin, payload begin, end)
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((begin, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(begin, payload)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |&(b, _)| b);
        if payload < end {
            out.push((begin, payload, end));
        }
    }
    out
}

fn is_vcl(t: u8) -> bool {
    t == NAL_SLICE || t == NAL_IDR
}

/// first_mb_in_slice == 0, i.e. ue(v) whose first bit is 1. The first payload byte
/// follows a non-zero header byte, so it cannot be an emulation-prevention byte.
fn starts_new_picture(payload: &[u8]) -> bool {
    payload.get(1).is_some_and(|b| b & 0x80 != 0)
}

pub fn parse(data: &[u8]) -> Result<Stream, Error> {
    if data.len() > MAX_STREAM_BYTES {
        return Err(Error::TooLarge(data.len()));
    }
    let nals = nal_ranges(data);
    if nals.is_empty() {
        return Err(Error::NoStartCode);
    }

    let mut codec = None;
    let mut units: Vec<AccessUnit> = Vec::new();
    let mut cur: Option<(usize, usize, bool, bool)> = None; // (begin, end, has_vcl, key)

    let flush = |cur: &mut Option<(usize, usize, bool, bool)>, units: &mut Vec<AccessUnit>| -> Result<(), Error> {
        if let Some((begin, end, has_vcl, key)) = cur.take() {
            if !has_vcl {
                return Ok(()); // trailing parameter sets/SEI without a picture
            }
            if end - begin > MAX_ACCESS_UNIT_BYTES {
                return Err(Error::UnitTooLarge(end - begin));
            }
            if units.len() == MAX_ACCESS_UNITS {
                return Err(Error::TooManyUnits);
            }
            units.push(AccessUnit { key, data: data[begin..end].to_vec() });
        }
        Ok(())
    };

    for (begin, payload, end) in nals {
        let nal = &data[payload..end];
        let t = nal[0] & 0x1f;

        if t == NAL_SPS && codec.is_none() && nal.len() >= 4 {
            codec = Some(format!("avc1.{:02X}{:02X}{:02X}", nal[1], nal[2], nal[3]));
        }

        let has_vcl = cur.is_some_and(|c| c.2);
        let boundary = match t {
            NAL_AUD | NAL_SPS | NAL_PPS | NAL_SEI | 14..=18 => has_vcl,
            _ if is_vcl(t) => has_vcl && starts_new_picture(nal),
            _ => false,
        };
        if boundary {
            flush(&mut cur, &mut units)?;
        }

        if is_vcl(t) && codec.is_none() {
            return Err(Error::NoSps);
        }
        let c = cur.get_or_insert((begin, end, false, false));
        c.1 = end;
        c.2 |= is_vcl(t);
        c.3 |= t == NAL_IDR;
    }
    flush(&mut cur, &mut units)?;

    match units.first() {
        None => Err(Error::NoSps),
        Some(u) if !u.key => Err(Error::FirstUnitNotKey),
        Some(_) => Ok(Stream { codec: codec.ok_or(Error::NoSps)?, units }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x4d, 0x00, 0x1e, 0xab];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80];
    const SEI: &[u8] = &[0, 0, 1, 0x06, 0x05, 0x01, 0xff, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];
    const P_SECOND_SLICE: &[u8] = &[0, 0, 1, 0x41, 0x40, 0x11];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn groups_parameter_sets_with_idr_and_splits_pictures() {
        let s = parse(&cat(&[SPS, PPS, SEI, IDR, P, P])).unwrap();
        assert_eq!(s.codec, "avc1.4D001E");
        assert_eq!(s.units.len(), 3);
        assert!(s.units[0].key);
        assert_eq!(s.units[0].data, cat(&[SPS, PPS, SEI, IDR]));
        assert!(!s.units[1].key);
        assert_eq!(s.units[1].data, P);
    }

    #[test]
    fn keeps_multi_slice_pictures_together() {
        let s = parse(&cat(&[SPS, PPS, IDR, P, P_SECOND_SLICE, P])).unwrap();
        assert_eq!(s.units.len(), 3);
        assert_eq!(s.units[1].data, cat(&[P, P_SECOND_SLICE]));
    }

    #[test]
    fn new_parameter_sets_start_a_new_unit() {
        let s = parse(&cat(&[SPS, PPS, IDR, P, SPS, PPS, IDR])).unwrap();
        assert_eq!(s.units.len(), 3);
        assert!(s.units[2].key);
        assert_eq!(s.units[2].data, cat(&[SPS, PPS, IDR]));
    }

    #[test]
    fn rejects_bad_streams() {
        assert_eq!(parse(&[1, 2, 3, 4]), Err(Error::NoStartCode));
        assert_eq!(parse(&cat(&[PPS, IDR])), Err(Error::NoSps));
        assert_eq!(parse(&cat(&[SPS, PPS, P, IDR])), Err(Error::FirstUnitNotKey));
        assert_eq!(parse(&vec![0; MAX_STREAM_BYTES + 1]), Err(Error::TooLarge(MAX_STREAM_BYTES + 1)));
        let big = cat(&[SPS, PPS, IDR, &vec![0x55; MAX_ACCESS_UNIT_BYTES]]);
        assert!(matches!(parse(&big), Err(Error::UnitTooLarge(_))));
    }

    #[test]
    fn fixture_has_30_units_with_leading_keyframe() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../build/synthetic-srgb.h264");
        let Ok(data) = std::fs::read(path) else {
            eprintln!("fixture not present at {path}; skipping");
            return;
        };
        let s = parse(&data).unwrap();
        assert_eq!(s.codec, "avc1.4D001E");
        assert_eq!(s.units.len(), 30);
        assert!(s.units[0].key);
        assert_eq!(s.units.iter().map(|u| u.data.len()).sum::<usize>(), data.len());
    }
}
