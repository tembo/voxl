//! Thin safe wrapper around libx264 tuned for low-latency desktop streaming.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use std::ffi::CString;
use std::ptr;

#[derive(Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub crf: f32,
    /// VBV cap in kbit/s (0 = uncapped).
    pub max_kbps: u32,
    /// How far the motion search looks, in pixels (`ME_RANGE`).
    pub me_range: u32,
}

/// How far x264's motion search looks, in pixels. A trackpad flick through a terminal
/// moves the text up to ~140 px between frames at 60 fps, and its speed changes every
/// frame, so the vectors the search starts from are tens of pixels off. At 64 the frames
/// that moved more than 32 px came out 19-57% bigger than at 128, for the same picture.
pub const ME_RANGE: u32 = 128;

pub struct Encoder {
    h: *mut ffi::x264_t,
    param: ffi::x264_param_t,
    pic: ffi::x264_picture_t,
    pts: i64,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Encoder {}

pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub stride_y: usize,
    pub stride_uv: usize,
}

impl Encoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        unsafe {
            let mut param: ffi::x264_param_t = std::mem::zeroed();
            let preset = CString::new(cfg.preset.as_str()).unwrap();
            let tune = c"zerolatency";
            if ffi::x264_param_default_preset(&mut param, preset.as_ptr(), tune.as_ptr()) < 0 {
                return Err(format!("bad x264 preset {}", cfg.preset));
            }
            param.i_width = cfg.width as i32;
            param.i_height = cfg.height as i32;
            param.i_csp = ffi::X264_CSP_I420 as i32;
            param.i_fps_num = cfg.fps;
            param.i_fps_den = 1;
            param.i_timebase_num = 1;
            param.i_timebase_den = 1000;
            // VFR input costs a frame of encoder delay (duration estimation).
            param.b_vfr_input = 0;
            param.i_threads = cfg.threads as i32;
            param.b_sliced_threads = (cfg.threads > 1) as i32;
            param.i_slice_count = cfg.threads as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = if std::env::var("VOXL_X264_INFO").as_deref() == Ok("debug") {
                // Per-frame QP, macroblock types and size.
                ffi::X264_LOG_DEBUG
            } else if std::env::var("VOXL_X264_INFO").is_ok() {
                ffi::X264_LOG_INFO
            } else {
                ffi::X264_LOG_WARNING
            } as i32;
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.crf;
            param.rc.i_lookahead = 0;
            // ultrafast's diamond search only follows motion of a few pixels, so a dragged
            // window or a fast scroll would be coded as new pixels every frame. UMH finds
            // those shifts (a third of the bits for a window drag) for about the same CPU.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            param.analyse.i_me_range = cfg.me_range as i32;
            // What a scroll or a page of new text costs is the pixels that weren't on the
            // screen before, and ultrafast codes those the dearest way H.264 has: whole
            // 16x16 blocks, with the simple entropy coder. Text is edges a few pixels wide:
            // 4x4 intra blocks follow them, and CABAC packs what is left. Together a
            // scrolling page of text costs 30% fewer bits at the same quantizer for about
            // 2 ms more a frame at 3024x1898 (`scroll_cost` below); unchanged areas are
            // skipped as before, so a desktop at rest costs what it did (`encode_cost`).
            param.b_cabac = 1;
            param.analyse.inter = ffi::X264_ANALYSE_I4x4;
            // The same blocks in keyframes: `inter` only covers intra blocks inside P
            // frames, and ultrafast leaves `intra` empty, so a keyframe was still all
            // 16x16. And ultrafast picks each block's mode by SAD on whole pixels only;
            // one step finer (SATD, a half-pixel pass) picks better ones for no more
            // time. A keyframe of text is a third smaller, a page of new text 6%.
            param.analyse.intra = ffi::X264_ANALYSE_I4x4;
            param.analyse.i_subpel_refine = 1;
            // Per-macroblock "unchanged" hints from X damage let x264 skip analysis.
            param.analyse.b_mb_info = (std::env::var("VOXL_NO_MBINFO").is_err()) as i32;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.max_kbps as i32;
                // ~1.25 frames of buffer keeps single-frame spikes bounded.
                param.rc.i_vbv_buffer_size = vbv_buffer_kbit(cfg.max_kbps, cfg.fps) as i32;
            }
            // sRGB content (BT.709 primaries, sRGB transfer) in BT.601 limited range,
            // matching libyuv ARGBToI420 and the macOS NV12 capture.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 13; // IEC 61966-2-1: every capture path delivers sRGB
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            let profile = c"high";
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder {
                h,
                param,
                pic,
                pts: 0,
                width: cfg.width,
                height: cfg.height,
            })
        }
    }

    /// Reconfigure rate control on the fly (buffer of ~1.25 frames at `max_kbps`).
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        self.set_rate_buf(
            crf,
            max_kbps,
            vbv_buffer_kbit(max_kbps, self.param.i_fps_num),
        );
    }

    /// Reconfigure rate control with an explicit VBV buffer (kbit).
    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, buf_kbit: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            if max_kbps > 0 {
                self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
                self.param.rc.i_vbv_buffer_size = buf_kbit.max(1) as i32;
            }
            ffi::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    /// Encode one picture. `force_idr` requests a keyframe, `qp` (if set) forces the frame QP.
    /// Appends Annex-B output to `out`.
    /// `mb_info` (one byte per macroblock, X264_MBINFO_CONSTANT for unchanged ones) lets
    /// x264 skip analysis of static areas; it is consumed before this returns.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        qp: Option<i32>,
        mb_info: Option<&mut [u8]>,
        out: &mut Vec<u8>,
    ) -> Result<Encoded, String> {
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr {
                ffi::X264_TYPE_IDR as i32
            } else {
                ffi::X264_TYPE_AUTO as i32
            };
            self.pic.i_qpplus1 = qp.map(|q| q + 1).unwrap_or(0);
            self.pic.prop.mb_info = mb_info.map(|m| m.as_mut_ptr()).unwrap_or(ptr::null_mut());
            self.pic.prop.mb_info_free = None;
            // Strictly increasing pts even if the wall clock stalls.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size =
                ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if size > 0 && !nals.is_null() {
                // NAL payloads are contiguous in memory.
                let first = &*nals;
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
            }
            if std::env::var("VOXL_DEBUG_MARKER").is_ok() {
                log::info!(
                    "x264 out {} bytes, delayed {}, pts in {} out {}",
                    size,
                    ffi::x264_encoder_delayed_frames(self.h),
                    self.pts,
                    pic_out.i_pts
                );
            }
            Ok(Encoded {
                keyframe: pic_out.b_keyframe != 0,
                // Undocumented as an output, but x264 has filled it in for years and
                // FFmpeg's libx264 wrapper reads it the same way.
                qp: (pic_out.i_qpplus1 > 0).then(|| (pic_out.i_qpplus1 - 1) as f32),
                crf_avg: pic_out.prop.f_crf_avg as f32,
            })
        }
    }
}

/// A VBV buffer of 1.25 frames at `max_kbps`, rounded up: x264 treats a buffer under
/// 1.1 frames as a single-frame VBV, which may lower rows of a frame below its starting
/// QP (video.rs's MotionQuality relies on it not doing that).
fn vbv_buffer_kbit(max_kbps: u32, fps: u32) -> u32 {
    (max_kbps * 5).div_ceil(4 * fps.max(1))
}

/// What x264 made of one picture.
pub struct Encoded {
    pub keyframe: bool,
    /// The QP x264 started the frame at, rounded (None if x264 didn't say). VBV may
    /// have raised rows of the frame above it; x264 doesn't report the average.
    pub qp: Option<f32>,
    /// x264's "average effective CRF": the CRF the frame was encoded with plus what VBV
    /// added to its average QP, before the frame and within it.
    pub crf_avg: f32,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of text `rows` tall, light on dark: words of 5x7 glyphs drawn 3x in 18x30
    /// cells, from a 16-letter alphabet, so letters repeat as they do in real text.
    fn text_page(w: usize, rows: usize) -> Vec<u8> {
        let mut page = vec![12u8; w * rows];
        let hash = |n: usize| (n as u64).wrapping_mul(0x9E3779B97F4A7C15) >> 29;
        let glyphs: Vec<u64> = (1..=16).map(|i| hash(i) & ((1 << 35) - 1)).collect();
        for line in 0..rows / 30 {
            let len = (hash(line + 99) % 60) as usize;
            for col in 0..len.min(w / 18) {
                if col % 6 == 5 || hash(line * 977 + col / 6) % 9 == 0 {
                    continue; // a space, or a blank word
                }
                let glyph = glyphs[(hash(line * 31 + col) % 16) as usize];
                for gy in 0..7 {
                    for gx in (0..5).filter(|gx| (glyph >> (gy * 5 + gx)) & 1 == 1) {
                        for py in 0..3 {
                            let at = (line * 30 + 4 + gy * 3 + py) * w + col * 18 + 2 + gx * 3;
                            page[at..at + 3].fill(220);
                        }
                    }
                }
            }
        }
        page
    }

    /// Bytes of the frames that moved 32 px or more, scrolling text as a trackpad flick
    /// does: from 140 px a frame, slowing every frame.
    fn flick_bytes(me_range: u32) -> usize {
        let (w, h) = (1280, 768);
        let steps: Vec<usize> = (0..40)
            .map(|i| (140.0 * (-(i as f64) / 20.0).exp()) as usize)
            .collect();
        let page = text_page(w, h + steps.iter().sum::<usize>() + 30);
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut enc = Encoder::new(&EncoderConfig {
            width: w as u32,
            height: h as u32,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 1,
            crf: 24.0,
            max_kbps: 0,
            me_range,
        })
        .unwrap();
        let (mut top, mut bytes, mut out) = (0, 0, Vec::new());
        for (f, step) in std::iter::once(0).chain(steps).enumerate() {
            top += step;
            let planes = Planes {
                y: &page[top * w..(top + h) * w],
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            out.clear();
            enc.encode(&planes, f as i64 * 16, f == 0, None, None, &mut out)
                .unwrap();
            if step >= 32 {
                bytes += out.len();
            }
        }
        bytes
    }

    /// The frames of a flick move further than the search used to look, by different
    /// amounts each frame: found, they are coded as motion, not as new text.
    #[test]
    fn a_flick_is_found() {
        let (short, default) = (flick_bytes(64), flick_bytes(ME_RANGE));
        assert!(
            (default as f64) < short as f64 * 0.8,
            "{default} bytes at the default search range, {short} at 64"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// A full-screen page of text-like lines scrolled `STEP` px a frame: motion the search
    /// finds, plus `STEP` rows of new text every frame.
    /// cargo test --release scroll_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn scroll_cost() {
        /// Three of the page's 44 px lines a frame, near the fastest a trackpad flick moves
        /// a terminal (see `ME_RANGE`). The bench's default since it was written; it was
        /// also run at one line and at five.
        const STEP: usize = 132;
        let (w, h) = (3024usize, 1898usize);
        let step: usize = std::env::var("D")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(132);
        let range: u32 = std::env::var("R")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(ME_RANGE);
        let mut enc = Encoder::new(&EncoderConfig {
            width: w as u32,
            height: h as u32,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 4,
            crf: 24.0,
            max_kbps: 0,
            me_range: range,
        })
        .unwrap();
        let mut y = vec![0u8; w * h];
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut out = Vec::new();
        let (mut bytes, mut total) = (0usize, std::time::Duration::ZERO);
        let (frames, warm) = (40, 5);
        for f in 0..frames {
            for row in 0..h {
                let r = row + f * step;
                let line = (r / 44) as u64;
                for x in 0..w {
                    // 22x44 cells, each a "glyph" of light strokes on dark, its own per
                    // line and column.
                    let cell = (x / 22) as u64;
                    let g = (cell.wrapping_mul(2654435761) ^ line.wrapping_mul(0x9E3779B97F4A7C15))
                        >> 11;
                    let (gx, gy) = (x % 22, r % 44);
                    let ink = gy > 6
                        && gy < 36
                        && gx > 2
                        && gx < 19
                        && (g >> (((gx / 3) + (gy / 6) * 5) % 40)) & 1 == 1;
                    y[row * w + x] = if ink { 200 } else { 12 };
                }
            }
            let planes = Planes {
                y: &y,
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            out.clear();
            let t = std::time::Instant::now();
            enc.encode(&planes, f as i64 * 16, f == 0, None, None, &mut out)
                .unwrap();
            if f >= warm {
                total += t.elapsed();
                bytes += out.len();
            }
        }
        let n = (frames - warm) as f64;
        println!(
            "scroll {step} px/frame, search range {range}: {:.1} KB/frame, {:.1} ms/frame",
            bytes as f64 / n / 1024.0,
            total.as_secs_f64() * 1000.0 / n
        );
    }

    /// cargo test --release encode_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn encode_cost() {
        /// One thread: what `--adaptive-threads` encodes on until a frame would miss its
        /// budget. The bench's default since it was written; it was also run at four, the
        /// default without that flag.
        const THREADS: u32 = 1;
        let (w, h) = (2560usize, 1600usize);
        let mbs = (w / 16) * (h / 16);
        let threads = THREADS;
        #[cfg(unix)]
        let cpu = || unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut r);
            (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64 * 1e3
                + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e3
        };
        // Process CPU time isn't wired up on Windows; wall times still print.
        #[cfg(windows)]
        let cpu = || 0.0;
        for (label, hint) in [("no hints", false), ("hints", true)] {
            let mut enc = Encoder::new(&EncoderConfig {
                width: w as u32,
                height: h as u32,
                fps: 60,
                preset: "ultrafast".into(),
                threads,
                crf: 24.0,
                max_kbps: 30000,
                me_range: ME_RANGE,
            })
            .unwrap();
            let mut y = vec![0u8; w * h];
            let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
            for (i, p) in y.iter_mut().enumerate() {
                *p = ((i % w) ^ (i / w)) as u8;
            }
            let mut out = Vec::new();
            let mut total = std::time::Duration::ZERO;
            let mut cpu_ms = 0.0;
            for f in 0..120 {
                // A 640x360 region changes every frame; the rest stays still.
                for row in 0..360 {
                    for x in 0..640 {
                        y[row * w + x] = (x + row + f * 7) as u8;
                    }
                }
                let mut info: Vec<u8> = (0..mbs)
                    .map(|i| ((i % (w / 16)) >= 40 || (i / (w / 16)) >= 23) as u8)
                    .collect();
                let planes = Planes {
                    y: &y,
                    u: &u,
                    v: &v,
                    stride_y: w,
                    stride_uv: w / 2,
                };
                out.clear();
                let t = std::time::Instant::now();
                let c0 = cpu();
                enc.encode(
                    &planes,
                    f as i64 * 16,
                    f == 0,
                    None,
                    if hint { Some(&mut info) } else { None },
                    &mut out,
                )
                .unwrap();
                if f >= 20 {
                    total += t.elapsed();
                    cpu_ms += cpu() - c0;
                }
            }
            println!(
                "{threads}t {label}: {:.2} ms/frame wall, {:.2} ms CPU, {} bytes last",
                total.as_secs_f64() * 1000.0 / 100.0,
                cpu_ms / 100.0,
                out.len()
            );
        }
    }
}
