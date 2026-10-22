//! Just enough H.264 bitstream handling to stand in for a frame that won't be sent: a
//! copy of its slices with every macroblock skipped, which decodes to the picture
//! before it and keeps frame numbers continuous (see `video.rs`, overshoots).

/// Reads an RBSP bit by bit.
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn bit(&mut self) -> Option<u32> {
        let v = (self.b.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// Writes an RBSP bit by bit.
#[derive(Default)]
struct Writer {
    b: Vec<u8>,
    pos: usize,
}

impl Writer {
    fn bit(&mut self, v: u32) {
        let shift = self.pos % 8;
        if shift == 0 {
            self.b.push(0);
        }
        if v != 0 {
            *self.b.last_mut().unwrap() |= 0x80 >> shift;
        }
        self.pos += 1;
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) as u32 & 1);
        }
    }
    /// rbsp_slice_trailing_bits: a one, then zeros to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        self.b
    }
}

/// NAL units of an Annex B stream: (header byte, escaped payload).
fn nal_units(stream: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| s - 3)
        .chain([stream.len()])
        .collect();
    starts.into_iter().zip(ends).filter_map(move |(s, e)| {
        // Zero bytes before the next start code belong to it.
        let e = stream[..e]
            .iter()
            .rposition(|&b| b != 0)
            .map_or(s, |p| p + 1);
        (e > s).then(|| (stream[s], &stream[s + 1..e]))
    })
}

fn unescape(escaped: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for &b in escaped {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    rbsp
}

fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// The parts of the SPS and PPS that a slice header's layout depends on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Params {
    log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_pic_order_always_zero: bool,
    frame_mbs_only: bool,
    mbs: u32,
    cabac: bool,
    bottom_field_pic_order: bool,
    slice_groups: bool,
    weighted_pred: bool,
    deblocking_control: bool,
    redundant_pic_cnt: bool,
    has_sps: bool,
    has_pps: bool,
}

fn skip_scaling_list(r: &mut Reader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
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

impl Params {
    /// Updates from the SPS and PPS in `stream` (a keyframe carries both).
    pub fn update(&mut self, stream: &[u8]) {
        for (h, payload) in nal_units(stream) {
            let rbsp = unescape(payload);
            match h & 0x1f {
                7 => self.has_sps = self.sps(&rbsp).is_some(),
                8 => self.has_pps = self.pps(&rbsp).is_some(),
                _ => {}
            }
        }
    }

    fn sps(&mut self, rbsp: &[u8]) -> Option<()> {
        let mut r = Reader { b: rbsp, pos: 0 };
        let profile = r.bits(8)?;
        r.bits(16)?; // constraint flags, reserved, level
        r.ue()?; // seq_parameter_set_id
        if matches!(
            profile,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134
        ) {
            let chroma = r.ue()?;
            if chroma == 3 {
                r.bit()?; // separate_colour_plane_flag
            }
            r.ue()?; // bit_depth_luma_minus8
            r.ue()?; // bit_depth_chroma_minus8
            r.bit()?; // qpprime_y_zero_transform_bypass_flag
            if r.bit()? == 1 {
                for i in 0..if chroma == 3 { 12 } else { 8 } {
                    if r.bit()? == 1 {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        self.log2_max_frame_num = r.ue()? + 4;
        self.poc_type = r.ue()?;
        match self.poc_type {
            0 => self.log2_max_poc_lsb = r.ue()? + 4,
            1 => {
                self.delta_pic_order_always_zero = r.bit()? == 1;
                r.se()?; // offset_for_non_ref_pic
                r.se()?; // offset_for_top_to_bottom_field
                for _ in 0..r.ue()? {
                    r.se()?; // offset_for_ref_frame
                }
            }
            _ => {}
        }
        r.ue()?; // max_num_ref_frames
        r.bit()?; // gaps_in_frame_num_value_allowed_flag
        let w = r.ue()? + 1;
        let h = r.ue()? + 1;
        self.frame_mbs_only = r.bit()? == 1;
        self.mbs = w * h * if self.frame_mbs_only { 1 } else { 2 };
        Some(())
    }

    fn pps(&mut self, rbsp: &[u8]) -> Option<()> {
        let mut r = Reader { b: rbsp, pos: 0 };
        r.ue()?; // pic_parameter_set_id
        r.ue()?; // seq_parameter_set_id
        self.cabac = r.bit()? == 1;
        self.bottom_field_pic_order = r.bit()? == 1;
        self.slice_groups = r.ue()? > 0;
        if self.slice_groups {
            return Some(()); // unsupported; the rest depends on the map type
        }
        r.ue()?; // num_ref_idx_l0_default_active_minus1
        r.ue()?; // num_ref_idx_l1_default_active_minus1
        self.weighted_pred = r.bit()? == 1;
        r.bits(2)?; // weighted_bipred_idc
        r.se()?; // pic_init_qp_minus26
        r.se()?; // pic_init_qs_minus26
        r.se()?; // chroma_qp_index_offset
        self.deblocking_control = r.bit()? == 1;
        r.bit()?; // constrained_intra_pred_flag
        self.redundant_pic_cnt = r.bit()? == 1;
        Some(())
    }

    /// Where a P slice's header ends (bit offset in its RBSP, after the NAL header
    /// byte) and its first macroblock.
    fn p_slice_header(&self, rbsp: &[u8], nal_ref_idc: u8) -> Option<(usize, u32)> {
        let mut r = Reader { b: rbsp, pos: 0 };
        let first_mb = r.ue()?;
        if r.ue()? % 5 != 0 {
            return None; // not a P slice
        }
        r.ue()?; // pic_parameter_set_id
        r.bits(self.log2_max_frame_num)?; // frame_num
        match self.poc_type {
            0 => {
                r.bits(self.log2_max_poc_lsb)?;
                if self.bottom_field_pic_order {
                    r.se()?;
                }
            }
            1 if !self.delta_pic_order_always_zero => {
                r.se()?;
                if self.bottom_field_pic_order {
                    r.se()?;
                }
            }
            _ => {}
        }
        if self.redundant_pic_cnt {
            r.ue()?;
        }
        if r.bit()? == 1 {
            r.ue()?; // num_ref_idx_l0_active_minus1
        }
        if r.bit()? == 1 {
            // ref_pic_list_modification_flag_l0
            loop {
                match r.ue()? {
                    0..=2 => {
                        r.ue()?;
                    }
                    3 => break,
                    _ => return None,
                }
            }
        }
        if nal_ref_idc != 0 && r.bit()? == 1 {
            // adaptive_ref_pic_marking_mode_flag
            loop {
                match r.ue()? {
                    0 => break,
                    3 => {
                        r.ue()?;
                        r.ue()?;
                    }
                    1 | 2 | 4 | 6 => {
                        r.ue()?;
                    }
                    5 => {}
                    _ => return None,
                }
            }
        }
        r.se()?; // slice_qp_delta
        if self.deblocking_control && r.ue()? != 1 {
            r.se()?; // slice_alpha_c0_offset_div2
            r.se()?; // slice_beta_offset_div2
        }
        Some((r.pos, first_mb))
    }

    /// A stand-in for the P frame `frame` (Annex B): the same slice headers, every
    /// macroblock skipped. It decodes to its reference, the picture before, and takes
    /// the frame number (and reference picture marking) of the frame it replaces.
    /// None for streams this doesn't handle (CABAC, interlace, slice groups, weighted
    /// prediction) or anything that isn't a P frame.
    pub fn skip_frame(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if !self.has_sps
            || !self.has_pps
            || self.cabac
            || !self.frame_mbs_only
            || self.slice_groups
            || self.weighted_pred
        {
            return None;
        }
        let mut slices = Vec::new();
        for (h, payload) in nal_units(frame) {
            match h & 0x1f {
                1 => {
                    let rbsp = unescape(payload);
                    let (end, first_mb) = self.p_slice_header(&rbsp, h >> 5)?;
                    slices.push((h, rbsp, end, first_mb));
                }
                5 => return None,
                _ => {} // SEI and the like: not needed
            }
        }
        if slices.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for i in 0..slices.len() {
            let (h, rbsp, end, first_mb) = &slices[i];
            let next = slices.get(i + 1).map_or(self.mbs, |s| s.3);
            let mut w = Writer::default();
            let mut r = Reader { b: rbsp, pos: 0 };
            for _ in 0..*end {
                w.bit(r.bit()?);
            }
            w.ue(next.checked_sub(*first_mb).filter(|&n| n > 0)?); // mb_skip_run
            out.extend_from_slice(&[0, 0, 0, 1, *h]);
            escape(&w.finish(), &mut out);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_round_trip() {
        let mut w = Writer::default();
        for v in [0, 1, 2, 3, 7, 8, 255, 16000] {
            w.ue(v);
        }
        let b = w.finish();
        let mut r = Reader { b: &b, pos: 0 };
        for v in [0, 1, 2, 3, 7, 8, 255, 16000] {
            assert_eq!(r.ue(), Some(v));
        }
        assert_eq!(r.bit(), Some(1)); // stop bit
    }

    #[test]
    fn escape_round_trip() {
        let raw = [0u8, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 5, 0, 0];
        let mut esc = Vec::new();
        escape(&raw, &mut esc);
        assert!(!esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(unescape(&esc), raw);
    }
}
