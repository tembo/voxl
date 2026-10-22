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
}

/// Lowest QP a frame can ask for (the highest is H.264's 51).
pub const MIN_QP: i32 = 11;

pub struct Encoder {
    h: *mut ffi::x264_t,
    pic: ffi::x264_picture_t,
    pts: i64,
    /// VOXL_PSNR: (frames, sum of luma PSNR) since the last report.
    psnr: Option<(u32, f64)>,
    /// SPS and PPS fields from the last keyframe, to read slice headers.
    params: crate::h264::Params,
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
            // One older frame kept to fall back on when the newest is invalidated
            // (`invalidate_last`); it isn't searched, so it costs no encoding time.
            param.i_dpb_size = 2;
            param.b_aud = 0;
            let psnr = std::env::var("VOXL_PSNR").is_ok();
            // Luma PSNR of what viewers decode (x264 only measures it at INFO or above).
            param.analyse.b_psnr = psnr as i32;
            param.i_log_level = if std::env::var("VOXL_X264_INFO").as_deref() == Ok("debug") {
                // Per-frame QP, macroblock types and size.
                ffi::X264_LOG_DEBUG
            } else if std::env::var("VOXL_X264_INFO").is_ok() || psnr {
                ffi::X264_LOG_INFO
            } else {
                ffi::X264_LOG_WARNING
            } as i32;
            // Constant QP, chosen per frame by the caller. CRF and VBV need x264's lowres
            // lookahead, a full-frame motion search that costs as much as encoding the
            // frame and doesn't parallelize across sliced threads.
            param.rc.i_rc_method = ffi::X264_RC_CQP as i32;
            // x264 clamps forced QPs to the span of its I, P and B frame QPs, and those
            // ratios to 10 (about 20 QP either way): MIN_QP..51 around 31.
            param.rc.i_qp_constant = 31;
            param.rc.f_ip_factor = 10.0;
            param.rc.f_pb_factor = 10.0;
            param.rc.i_lookahead = 0;
            // Without the lowres pass's motion vectors as candidates, ultrafast's diamond
            // search loses scrolls. The uneven multi-hexagon search finds them on its own:
            // on recorded Chromium scrolling it encodes a third smaller than CRF with the
            // lowres pass, and no slower than diamond.
            param.analyse.i_me_method = ffi::X264_ME_UMH as i32;
            // Per-macroblock "unchanged" hints from X damage let x264 skip analysis.
            param.analyse.b_mb_info = (std::env::var("VOXL_NO_MBINFO").is_err()) as i32;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            // BT.601 limited range, matching libyuv ARGBToI420.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 1;
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
                pic,
                pts: 0,
                psnr: psnr.then_some((0, 0.0)),
                params: Default::default(),
                width: cfg.width,
                height: cfg.height,
            })
        }
    }

    /// Encode one picture at `qp` (MIN_QP..=51). `force_idr` requests a keyframe.
    /// Appends Annex-B output to `out` and returns whether it is a keyframe.
    /// `mb_info` (one byte per macroblock, X264_MBINFO_CONSTANT for unchanged ones) lets
    /// x264 skip analysis of static areas; it is consumed before this returns.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        qp: i32,
        mb_info: Option<&mut [u8]>,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
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
            self.pic.i_qpplus1 = qp + 1;
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
                let start = out.len();
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
                if pic_out.b_keyframe != 0 {
                    self.params.update(&out[start..]);
                }
            }
            if let Some((n, sum)) = self.psnr.as_mut() {
                *n += 1;
                *sum += pic_out.prop.f_psnr[0];
                if *n == 300 {
                    log::info!("psnr Y {:.2} dB over {n} frames", *sum / *n as f64);
                    (*n, *sum) = (0, 0.0);
                }
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
            Ok(pic_out.b_keyframe != 0)
        }
    }
}

impl Encoder {
    /// Make x264 forget the frame it just encoded: the next one refers to the frame
    /// before. Send `skip_frame` of it instead, so the decoder's references match.
    pub fn invalidate_last(&mut self) -> bool {
        unsafe { ffi::x264_encoder_invalidate_reference(self.h, self.pts) == 0 }
    }

    /// A frame repeating the picture before `frame` (one of this encoder's P frames),
    /// to send in its place after `invalidate_last` (see `h264::Params::skip_frame`).
    pub fn skip_frame(&self, frame: &[u8]) -> Option<Vec<u8>> {
        self.params.skip_frame(frame)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overshoot_stand_in() {
        let (w, h) = (640usize, 368usize);
        let mut enc = Encoder::new(&EncoderConfig {
            width: w as u32,
            height: h as u32,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 4,
        })
        .unwrap();
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let pic = |k: usize| -> Vec<u8> {
            (0..w * h)
                .map(|i| (((i % w) * (7 + k) + (i / w) * (3 + 2 * k)) % 251) as u8)
                .collect()
        };
        let y0 = pic(0);
        let p0 = Planes {
            y: &y0,
            u: &u,
            v: &v,
            stride_y: w,
            stride_uv: w / 2,
        };
        let mut key = Vec::new();
        assert!(enc.encode(&p0, 0, true, 22, None, &mut key).unwrap());
        assert!(
            enc.skip_frame(&key).is_none(),
            "only P frames have stand-ins"
        );
        // A scene change: a different picture altogether.
        let y1 = pic(5);
        let p1 = Planes {
            y: &y1,
            u: &u,
            v: &v,
            stride_y: w,
            stride_uv: w / 2,
        };
        let mut big = Vec::new();
        assert!(!enc.encode(&p1, 16, false, 22, None, &mut big).unwrap());
        let fill = enc.skip_frame(&big).expect("stand-in");
        // Four slices (threads), each a header and one skip run.
        assert_eq!(fill.windows(4).filter(|s| *s == [0, 0, 0, 1]).count(), 4);
        assert!(fill.len() < 100, "{} bytes", fill.len());
        assert!(enc.invalidate_last());
        let mut again = Vec::new();
        assert!(!enc.encode(&p1, 17, false, 45, None, &mut again).unwrap());
        assert!(again.len() < big.len() / 2);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test --release encode_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn encode_cost() {
        let (w, h) = (2560usize, 1600usize);
        let mbs = (w / 16) * (h / 16);
        let threads: u32 = std::env::var("T")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(1);
        let cpu = || unsafe {
            let mut r: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut r);
            (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64 * 1e3
                + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e3
        };
        for (label, hint) in [("no hints", false), ("hints", true)] {
            let mut enc = Encoder::new(&EncoderConfig {
                width: w as u32,
                height: h as u32,
                fps: 60,
                preset: "ultrafast".into(),
                threads,
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
                    22,
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
