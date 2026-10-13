//! Just enough H.264 Annex B parsing to check access units from the capture app, derive
//! the WebCodecs codec string, and split a file into access units for replay.

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// A NAL unit: `start` is where its start code begins, `body` its bytes after the start
/// code (header included).
#[derive(Debug, Clone, Copy)]
pub struct Nal<'a> {
    pub start: usize,
    pub body: &'a [u8],
}

impl Nal<'_> {
    pub fn kind(&self) -> u8 {
        self.body.first().map_or(0, |h| h & 0x1F)
    }

    fn is_slice(&self) -> bool {
        matches!(self.kind(), NAL_SLICE | NAL_IDR)
    }

    /// first_mb_in_slice == 0: its ue(v) code is a single 1 bit.
    fn starts_picture(&self) -> bool {
        self.is_slice() && self.body.get(1).is_some_and(|b| b & 0x80 != 0)
    }
}

/// Splits Annex B data at 3- and 4-byte start codes. Bytes before the first start code
/// are ignored.
pub fn nal_units(data: &[u8]) -> Vec<Nal<'_>> {
    let mut codes = Vec::new(); // (start of start code, start of body)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            codes.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    codes
        .iter()
        .enumerate()
        .map(|(n, &(start, body))| {
            let end = codes.get(n + 1).map_or(data.len(), |&(next, _)| next);
            Nal { start, body: &data[body..end] }
        })
        .filter(|nal| !nal.body.is_empty())
        .collect()
}

/// "avc1.PPCCLL" from the SPS profile_idc, constraint flags and level_idc.
pub fn codec_string(sps: &[u8]) -> Option<String> {
    let [_, profile, constraints, level, ..] = sps else { return None };
    Some(format!("avc1.{profile:02X}{constraints:02X}{level:02X}"))
}

/// What the host needs to know about one access unit.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// An IDR carrying its SPS and PPS, with the codec string from that SPS.
    Keyframe { codec: String },
    Delta,
}

/// Checks an access unit against the app's keyframe flag. A keyframe must contain an
/// IDR slice, an SPS and a PPS; a delta must contain a slice and no IDR.
pub fn classify(data: &[u8], keyframe: bool) -> Result<Kind, String> {
    let nals = nal_units(data);
    let has = |kind| nals.iter().any(|n| n.kind() == kind);
    if !nals.iter().any(|n| n.is_slice()) {
        return Err("access unit has no slice".into());
    }
    match (keyframe, has(NAL_IDR)) {
        (true, true) => {
            let sps = nals.iter().find(|n| n.kind() == NAL_SPS).ok_or("keyframe without SPS")?;
            if !has(NAL_PPS) {
                return Err("keyframe without PPS".into());
            }
            let codec = codec_string(sps.body).ok_or("SPS too short")?;
            Ok(Kind::Keyframe { codec })
        }
        (false, false) => Ok(Kind::Delta),
        (true, false) => Err("keyframe flag on an access unit without an IDR".into()),
        (false, true) => Err("IDR in an access unit not flagged as a keyframe".into()),
    }
}

/// Splits an Annex B elementary stream into access units: a new one begins at an AUD,
/// or at an SPS/PPS/SEI or a picture's first slice once the current one has a slice.
pub fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut begin = None;
    let mut has_slice = false;
    for nal in nal_units(data) {
        let boundary = nal.kind() == NAL_AUD || (has_slice && (matches!(nal.kind(), NAL_SPS | NAL_PPS | NAL_SEI) || nal.starts_picture()));
        if boundary {
            if let Some(b) = begin {
                units.push(&data[b..nal.start]);
            }
            begin = None;
            has_slice = false;
        }
        begin.get_or_insert(nal.start);
        has_slice |= nal.is_slice();
    }
    if let Some(b) = begin {
        units.push(&data[b..]);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = &[0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x28, 0xAA, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65, 0x88, 0x84];
    const DELTA: &[u8] = &[0, 0, 0, 1, 0x41, 0x9A, 0x02];

    #[test]
    fn classifies_access_units() {
        assert_eq!(classify(KEY, true), Ok(Kind::Keyframe { codec: "avc1.4D0028".into() }));
        assert_eq!(classify(DELTA, false), Ok(Kind::Delta));
        assert!(classify(KEY, false).is_err());
        assert!(classify(DELTA, true).is_err());
        assert!(classify(&[0, 0, 1, 0x65, 0x88], true).is_err(), "IDR without SPS/PPS");
        assert!(classify(&[0, 0, 1, 0x67, 0x4D, 0, 0x28], false).is_err(), "no slice");
        assert!(classify(&[1, 2, 3], false).is_err(), "not Annex B");
    }

    #[test]
    fn splits_a_stream_into_access_units() {
        // Second slice of the same picture (first_mb != 0) stays in its unit.
        let second_slice: &[u8] = &[0, 0, 1, 0x41, 0x40, 0x01];
        let stream = [KEY, DELTA, second_slice, DELTA, KEY].concat();
        let units = access_units(&stream);
        assert_eq!(units, vec![KEY, &[DELTA, second_slice].concat()[..], DELTA, KEY]);
    }

    #[test]
    fn fixture_has_the_expected_structure() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/moving-1080p.h264");
        let data = std::fs::read(path).unwrap();
        let units = access_units(&data);
        assert_eq!(units.len(), 90);
        let keyframes: Vec<usize> = units.iter().enumerate().filter(|(_, u)| nal_units(u).iter().any(|n| n.kind() == NAL_IDR)).map(|(i, _)| i).collect();
        assert_eq!(keyframes, vec![0, 60]);
        assert_eq!(classify(units[0], true), Ok(Kind::Keyframe { codec: "avc1.4D0028".into() }));
        for (i, unit) in units.iter().enumerate() {
            assert!(classify(unit, keyframes.contains(&i)).is_ok(), "unit {i}");
        }
    }
}
