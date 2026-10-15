//! Tells the decoder that frames are never reordered, so it shows each one at once.
//!
//! Without `bitstream_restriction` in the SPS's VUI, a Main or High profile H.264
//! decoder must assume frames may be reordered, and Chromium's holds up to a DPB's worth
//! of frames before output. Screen capture sends frames only when something changes, so
//! each held frame waits for later changes: half a second and more on a quiet screen.
//! VideoToolbox (the Mac host's encoder) writes a VUI without it, and never reorders.
//!
//! [`no_reorder`] adds `bitstream_restriction` with `max_num_reorder_frames = 0` to an
//! access unit's SPS when it is missing. An SPS that already has it is left alone (a
//! stream that says it reorders, does), and so is anything this parser doesn't follow.

use crate::annexb::nal_ranges;

const NAL_SPS: u8 = 7;

/// The access unit with its SPS rewritten, or `None` if it needs no change.
pub fn no_reorder(au: &[u8]) -> Option<Vec<u8>> {
    let mut out = None::<Vec<u8>>;
    let mut copied = 0;
    for (_, payload, end) in nal_ranges(au) {
        if au[payload] & 0x1F != NAL_SPS {
            continue;
        }
        let Some(sps) = rewrite(&au[payload..end]) else { continue };
        let o = out.get_or_insert_with(|| Vec::with_capacity(au.len() + 8));
        o.extend_from_slice(&au[copied..payload]);
        o.extend_from_slice(&sps);
        copied = end;
    }
    let mut o = out?;
    o.extend_from_slice(&au[copied..]);
    Some(o)
}

/// The access unit's first SPS NAL unit (header byte included, no start code), if any.
/// A keyframe whose SPS differs from the last one starts a new stream, for example at
/// a new size.
pub fn find(au: &[u8]) -> Option<&[u8]> {
    nal_ranges(au).into_iter().find(|&(_, payload, end)| payload < end && au[payload] & 0x1F == NAL_SPS).map(|(_, payload, end)| &au[payload..end])
}

/// One SPS NAL unit (header byte included, no start code), rewritten if it lacks
/// `bitstream_restriction`.
fn rewrite(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(&nal[1..]);
    let mut r = Reader { data: &rbsp, pos: 0 };
    let Sps::Unrestricted { insert_at, vui_present, max_num_ref_frames } = parse(&mut r)? else { return None };
    let mut w = Writer::default();
    w.copy(&rbsp, insert_at);
    if !vui_present {
        // vui_parameters_present_flag, then aspect ratio, overscan, video signal,
        // chroma location, timing, NAL HRD, VCL HRD and pic_struct all absent.
        w.bit(true);
        for _ in 0..8 {
            w.bit(false);
        }
    }
    // bitstream_restriction_flag and its fields, with the values a decoder infers
    // when they are absent, except the reordering ones.
    w.bit(true);
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom
    w.ue(1); // max_bits_per_mb_denom
    w.ue(15); // log2_max_mv_length_horizontal
    w.ue(15); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
    w.bit(true); // rbsp_stop_one_bit, then alignment zeros
    let mut out = vec![nal[0]];
    out.extend(escape(&w.finish()));
    Some(out)
}

#[derive(Debug, PartialEq, Eq)]
enum Sps {
    Unrestricted {
        /// Bit position where the new syntax goes: the VUI flag, or the restriction flag.
        insert_at: usize,
        vui_present: bool,
        max_num_ref_frames: u32,
    },
    Restricted {
        max_num_reorder_frames: u32,
        max_dec_frame_buffering: u32,
    },
}

/// Follows the SPS to its bitstream restriction; `None` if anything is out of the ordinary.
fn parse(r: &mut Reader) -> Option<Sps> {
    let profile = r.u(8)?;
    r.u(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.u(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.u(1)?;
        if r.u(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.u(1)? == 1 {
                    scaling_list(r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()?.min(255) {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.u(1)?;
    r.ue()?;
    r.ue()?;
    if r.u(1)? == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?;
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let at_vui = r.pos;
    if r.u(1)? == 0 {
        return Some(Sps::Unrestricted { insert_at: at_vui, vui_present: false, max_num_ref_frames });
    }
    if r.u(1)? == 1 && r.u(8)? == 255 {
        r.u(32)?;
    }
    if r.u(1)? == 1 {
        r.u(1)?;
    }
    if r.u(1)? == 1 {
        r.u(4)?;
        if r.u(1)? == 1 {
            r.u(24)?;
        }
    }
    if r.u(1)? == 1 {
        r.ue()?;
        r.ue()?;
    }
    if r.u(1)? == 1 {
        r.u(32)?;
        r.u(32)?;
        r.u(1)?;
    }
    let nal_hrd = r.u(1)? == 1;
    if nal_hrd {
        hrd(r)?;
    }
    let vcl_hrd = r.u(1)? == 1;
    if vcl_hrd {
        hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.u(1)?;
    }
    r.u(1)?; // pic_struct_present_flag
    let at_restriction = r.pos;
    let sps = if r.u(1)? == 1 {
        r.u(1)?;
        for _ in 0..4 {
            r.ue()?;
        }
        Sps::Restricted { max_num_reorder_frames: r.ue()?, max_dec_frame_buffering: r.ue()? }
    } else {
        Sps::Unrestricted { insert_at: at_restriction, vui_present: true, max_num_ref_frames }
    };
    // Only rbsp_trailing_bits may follow, or this isn't the SPS we think it is.
    (r.u(1)? == 1 && r.rest_is_zero() != Some(false)).then_some(sps)
}

fn scaling_list(r: &mut Reader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn hrd(r: &mut Reader) -> Option<()> {
    let count = r.ue()?.min(31) + 1;
    r.u(8)?;
    for _ in 0..count {
        r.ue()?;
        r.ue()?;
        r.u(1)?;
    }
    r.u(20).map(|_| ())
}

/// Emulation prevention bytes removed.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention bytes added.
fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        u32::try_from(v & 0xFFFF_FFFF).ok()
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64).min(u32::MAX as u64) as u32)
    }

    fn se(&mut self) -> Option<i64> {
        let v = self.ue()? as i64;
        Some(if v % 2 == 1 { (v + 1) / 2 } else { -(v / 2) })
    }

    /// Whether the bits left are all zero; `None` at the end.
    fn rest_is_zero(&mut self) -> Option<bool> {
        let mut any = None;
        while let Some(b) = self.bit() {
            any = Some(any.unwrap_or(true) && b == 0);
        }
        any
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn bit(&mut self, b: bool) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b {
            *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    fn ue(&mut self, v: u32) {
        let v = v as u64 + 1;
        let len = 64 - v.leading_zeros();
        for _ in 1..len {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit(v >> i & 1 == 1);
        }
    }

    /// The first `n` bits of `data`.
    fn copy(&mut self, data: &[u8], n: usize) {
        for i in 0..n {
            self.bit(data[i / 8] >> (7 - i % 8) & 1 == 1);
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Mac host's SPS (VideoToolbox, Main 4.0, 1920x1080, VUI for colour only),
    /// from build/moving-1080p.h264.
    const MAC_SPS: &[u8] = &[0x27, 0x4d, 0x00, 0x28, 0xab, 0x40, 0x3c, 0x01, 0x13, 0xf2, 0xcd, 0x40, 0x43, 0x40, 0x40, 0x80];
    const PPS_IDR: &[u8] = &[0, 0, 0, 1, 0x28, 0xee, 0x3c, 0x80, 0, 0, 1, 0x25, 0x88, 0x84];

    fn parsed(nal: &[u8]) -> Option<Sps> {
        let rbsp = unescape(&nal[1..]);
        parse(&mut Reader { data: &rbsp, pos: 0 })
    }

    const NO_REORDER: Sps = Sps::Restricted { max_num_reorder_frames: 0, max_dec_frame_buffering: 1 };

    #[test]
    fn the_mac_sps_gains_no_reordering() {
        assert!(matches!(parsed(MAC_SPS), Some(Sps::Unrestricted { vui_present: true, max_num_ref_frames: 1, .. })));
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(MAC_SPS);
        au.extend_from_slice(PPS_IDR);
        let out = no_reorder(&au).expect("rewritten");
        let nals = nal_ranges(&out);
        assert_eq!(nals.len(), 3);
        let (begin, p, e) = nals[0];
        assert_eq!(&out[begin..p + 4], &au[..8], "start code, header, profile and level are kept");
        assert_eq!(parsed(&out[p..e]), Some(NO_REORDER));
        assert_eq!(&out[nals[1].0..], PPS_IDR, "the rest is untouched");
        assert_eq!(no_reorder(&out), None, "already restricted");
    }

    #[test]
    fn an_sps_without_vui_gains_one() {
        let mut w = Writer::default();
        w.copy(&[66, 0xC0, 30], 24); // Constrained Baseline 3.0
        w.ue(0); // seq_parameter_set_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.bit(false);
        w.ue(19); // 320 wide
        w.ue(14); // 240 high
        w.bit(true); // frame_mbs_only_flag
        w.bit(true);
        w.bit(false); // no cropping
        w.bit(false); // no VUI
        w.bit(true); // stop bit
        let mut nal = vec![0x67];
        nal.extend(escape(&w.finish()));
        assert!(matches!(parsed(&nal), Some(Sps::Unrestricted { vui_present: false, .. })));
        assert_eq!(parsed(&rewrite(&nal).expect("rewritten")), Some(NO_REORDER));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// From the Mac host's M3 encoder, 1600x1040: normal rate control, then low-latency
    /// rate control in Main, Constrained Baseline and Constrained High.
    #[test]
    fn mac_encoder_variants() {
        let normal = hex("274d002aab403201074d4043404080");
        assert!(matches!(parsed(&normal), Some(Sps::Unrestricted { vui_present: true, max_num_ref_frames: 1, .. })));
        assert_eq!(parsed(&rewrite(&normal).unwrap()), Some(NO_REORDER));
        for low_latency in ["274d002a898a300c8041d35010d01078402134", "2742c02a898a300c8041d35010d01078402134", "27640c2aac131460190083a6a021a020f0804268"] {
            let nal = hex(low_latency);
            let restricted = Sps::Restricted { max_num_reorder_frames: 0, max_dec_frame_buffering: 5 };
            assert_eq!(parsed(&nal), Some(restricted), "{low_latency} already says no reordering");
            assert_eq!(rewrite(&nal), None, "{low_latency} is left alone");
        }
    }

    #[test]
    fn escaping_round_trips() {
        let data = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        let e = escape(&data);
        assert!(!e.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(unescape(&e), data);
    }

    #[test]
    fn finds_the_sps() {
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(MAC_SPS);
        au.extend_from_slice(PPS_IDR);
        assert_eq!(find(&au), Some(MAC_SPS));
        assert_eq!(find(PPS_IDR), None);
        assert_eq!(find(&[]), None);
    }

    #[test]
    fn other_input_passes_unchanged() {
        assert_eq!(no_reorder(&[0, 0, 1, 0x41, 0x9a]), None, "no SPS");
        assert_eq!(no_reorder(&[0, 0, 1, 0x67, 0x4d]), None, "truncated SPS");
        let mut garbage = MAC_SPS.to_vec();
        garbage.push(0xFF);
        assert_eq!(parsed(&garbage), None, "bits after the VUI");
    }
}

