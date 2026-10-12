//! Thin safe wrapper around libx264 tuned for low-latency desktop streaming.

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}

use std::ffi::CString;
use std::ptr;

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub preset: String,
    pub threads: u32,
    pub crf: f32,
    /// VBV cap in kbit/s (0 = uncapped).
    pub max_kbps: u32,
}

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
            let tune = CString::new("zerolatency").unwrap();
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
            param.b_sliced_threads = 1;
            param.i_slice_count = cfg.threads as i32;
            param.i_keyint_max = ffi::X264_KEYINT_MAX_INFINITE as i32;
            param.i_keyint_min = 1;
            param.i_scenecut_threshold = 0;
            param.b_intra_refresh = 0;
            param.i_bframe = 0;
            param.b_annexb = 1;
            param.b_repeat_headers = 1;
            param.b_aud = 0;
            param.i_log_level = ffi::X264_LOG_WARNING as i32;
            param.rc.i_rc_method = ffi::X264_RC_CRF as i32;
            param.rc.f_rf_constant = cfg.crf;
            param.rc.i_lookahead = 0;
            param.i_sync_lookahead = 0;
            param.rc.b_mb_tree = 0;
            if cfg.max_kbps > 0 {
                param.rc.i_vbv_max_bitrate = cfg.max_kbps as i32;
                // ~2 frames of buffer keeps single-frame spikes bounded.
                param.rc.i_vbv_buffer_size = (cfg.max_kbps / cfg.fps.max(1) * 3) as i32;
            }
            // BT.601 limited range, matching libyuv ARGBToI420.
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 1;
            param.vui.i_colmatrix = 6;
            param.vui.b_fullrange = 0;
            let profile = CString::new("high").unwrap();
            ffi::x264_param_apply_profile(&mut param, profile.as_ptr());

            let h = ffi::x264_encoder_open_164(&mut param);
            if h.is_null() {
                return Err("x264_encoder_open failed".into());
            }
            let mut pic: ffi::x264_picture_t = std::mem::zeroed();
            ffi::x264_picture_init(&mut pic);
            pic.img.i_csp = ffi::X264_CSP_I420 as i32;
            pic.img.i_plane = 3;
            Ok(Encoder { h, param, pic, pts: 0, width: cfg.width, height: cfg.height })
        }
    }

    /// Reconfigure rate control on the fly.
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        unsafe {
            self.param.rc.f_rf_constant = crf;
            if max_kbps > 0 {
                self.param.rc.i_vbv_max_bitrate = max_kbps as i32;
                self.param.rc.i_vbv_buffer_size = (max_kbps / self.param.i_fps_num.max(1) * 3) as i32;
            }
            ffi::x264_encoder_reconfig(self.h, &mut self.param);
        }
    }

    /// Encode one picture. `force_idr` requests a keyframe, `qp` (if set) forces the frame QP.
    /// Appends Annex-B output to `out` and returns whether it is a keyframe.
    pub fn encode(&mut self, p: &Planes, ts_ms: i64, force_idr: bool, qp: Option<i32>, out: &mut Vec<u8>) -> Result<bool, String> {
        unsafe {
            self.pic.img.plane[0] = p.y.as_ptr() as *mut u8;
            self.pic.img.plane[1] = p.u.as_ptr() as *mut u8;
            self.pic.img.plane[2] = p.v.as_ptr() as *mut u8;
            self.pic.img.i_stride[0] = p.stride_y as i32;
            self.pic.img.i_stride[1] = p.stride_uv as i32;
            self.pic.img.i_stride[2] = p.stride_uv as i32;
            self.pic.i_type = if force_idr { ffi::X264_TYPE_IDR as i32 } else { ffi::X264_TYPE_AUTO as i32 };
            self.pic.i_qpplus1 = qp.map(|q| q + 1).unwrap_or(0);
            // Strictly increasing pts even if the wall clock stalls.
            self.pts = ts_ms.max(self.pts + 1);
            self.pic.i_pts = self.pts;
            let mut nals: *mut ffi::x264_nal_t = ptr::null_mut();
            let mut n: i32 = 0;
            let mut pic_out: ffi::x264_picture_t = std::mem::zeroed();
            let size = ffi::x264_encoder_encode(self.h, &mut nals, &mut n, &mut self.pic, &mut pic_out);
            if size < 0 {
                return Err("x264_encoder_encode failed".into());
            }
            if size > 0 && !nals.is_null() {
                // NAL payloads are contiguous in memory.
                let first = &*nals;
                out.extend_from_slice(std::slice::from_raw_parts(first.p_payload, size as usize));
            }
            if std::env::var("SRS_DEBUG_MARKER").is_ok() {
                log::info!("x264 out {} bytes, delayed {}, pts in {} out {}", size, ffi::x264_encoder_delayed_frames(self.h), self.pts, pic_out.i_pts);
            }
            Ok(pic_out.b_keyframe != 0)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::x264_encoder_close(self.h) }
    }
}
