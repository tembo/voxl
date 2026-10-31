//! H.264 on the Mac's media engine with VideoToolbox (`--encoder=videotoolbox`), behind
//! the same interface as x264: I420 in, Annex B out, a keyframe when asked for.
//!
//! Each frame is copied into an NV12 pixel buffer from the session's pool and encoded in
//! low-latency mode (real time, no reordering, no frame delay), and `encode` waits for
//! it. VideoToolbox writes AVCC (length-prefixed NAL units, with the parameter sets in
//! the format description); the output is rewritten as Annex B with SPS and PPS in front
//! of every keyframe, as x264 writes it, so viewers and recordings take it unchanged.
//!
//! Quality follows x264's CRF where macOS allows it: the CRF becomes the lowest QP a
//! frame may use (macOS 14+), so still content isn't sent sharper than x264 would, and
//! paint-overs (a lower CRF) sharpen it. The bitrate cap becomes the average bitrate.
//! x264's macroblock hints have no VideoToolbox equivalent; the whole stripe is encoded.

use super::ffi::*;
use crate::encoder::{EncoderConfig, Planes};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use parking_lot::{Condvar, Mutex};
use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

/// A CoreFoundation constant (a framework's `CFStringRef` global) as a reference.
unsafe fn cf<'a, T>(p: *const c_void) -> &'a T {
    &*(p as *const T)
}

/// `kVTCompressionPropertyKey_MinAllowedFrameQP`, if this macOS has it (14 and later);
/// looked up at run time so voxl still starts on older versions.
fn min_qp_key() -> Option<CFStringRef> {
    let sym = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"kVTCompressionPropertyKey_MinAllowedFrameQP".as_ptr(),
        )
    };
    (!sym.is_null())
        .then(|| unsafe { *(sym as *const CFStringRef) })
        .filter(|k| !k.is_null())
}

/// What the output callback hands back.
#[derive(Default)]
struct Output {
    /// The latest frame the encoder finished with (emitted or dropped), by number.
    last: u64,
    /// That frame as Annex B, and whether it's a keyframe (None: dropped).
    frame: Option<(Vec<u8>, bool)>,
    error: Option<OSStatus>,
}

#[derive(Default)]
struct Shared {
    out: Mutex<Output>,
    cv: Condvar,
}

pub struct Encoder {
    session: VTCompressionSessionRef,
    /// The output callback's context; boxed so its address stays put.
    shared: Box<Shared>,
    submitted: u64,
    pts: i64,
    width: usize,
    height: usize,
    min_qp: Option<CFStringRef>,
    /// Rate last applied: CRF and kbit/s.
    applied: (f32, u32),
}

// A session may be used from any thread, one at a time.
unsafe impl Send for Encoder {}

extern "C" fn on_output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: u32,
    sample: CMSampleBufferRef,
) {
    let shared = unsafe { &*(refcon as *const Shared) };
    let frame = (status == 0 && !sample.is_null())
        .then(|| unsafe { annex_b(sample) })
        .flatten();
    let mut o = shared.out.lock();
    o.last = frame_refcon as u64;
    o.frame = frame;
    o.error = (status != 0).then_some(status);
    shared.cv.notify_all();
}

/// An encoded sample as Annex B (SPS and PPS first on a keyframe), and whether it's a
/// keyframe.
unsafe fn annex_b(sample: CMSampleBufferRef) -> Option<(Vec<u8>, bool)> {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
    let not_sync = if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
        ptr::null()
    } else {
        CFDictionaryGetValue(
            CFArrayGetValueAtIndex(attachments, 0),
            kCMSampleAttachmentKey_NotSync,
        )
    };
    let key = not_sync.is_null() || !CFBooleanGetValue(not_sync);
    let desc = CMSampleBufferGetFormatDescription(sample);
    let mut out = Vec::new();
    // The parameter sets, and how long the NAL unit length prefixes are.
    let (mut i, mut count, mut len_size) = (0, 1, 4);
    while i < count {
        let (mut set, mut size) = (ptr::null(), 0);
        if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            desc,
            i,
            &mut set,
            &mut size,
            &mut count,
            &mut len_size,
        ) != 0
        {
            return None;
        }
        if key {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(std::slice::from_raw_parts(set, size));
        }
        i += 1;
    }
    let block = CMSampleBufferGetDataBuffer(sample);
    if block.is_null() {
        return None;
    }
    let mut data = vec![0u8; CMBlockBufferGetDataLength(block)];
    if CMBlockBufferCopyDataBytes(block, 0, data.len(), data.as_mut_ptr().cast()) != 0 {
        return None;
    }
    avcc_to_annex_b(&data, len_size as usize, &mut out)?;
    Some((out, key))
}

/// Rewrites NAL units with `len_size`-byte big-endian length prefixes (AVCC) as Annex B
/// (4-byte start codes), appending to `out`. None if a length runs past the end.
fn avcc_to_annex_b(mut data: &[u8], len_size: usize, out: &mut Vec<u8>) -> Option<()> {
    if !(1..=4).contains(&len_size) {
        return None;
    }
    while !data.is_empty() {
        let (head, rest) = data.split_at_checked(len_size)?;
        let n = head.iter().fold(0usize, |n, &b| n << 8 | b as usize);
        let (nal, rest) = rest.split_at_checked(n)?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        data = rest;
    }
    Some(())
}

impl Encoder {
    /// A hardware encoder, or an error if this Mac has none for these settings.
    pub fn new(cfg: &EncoderConfig) -> Result<Self, String> {
        Self::open(cfg, true)
    }

    fn open(cfg: &EncoderConfig, hardware: bool) -> Result<Self, String> {
        let t = CFBoolean::new(true);
        let mut spec_keys: Vec<&CFString> = Vec::new();
        unsafe {
            spec_keys.push(cf(kVTVideoEncoderSpecification_EnableLowLatencyRateControl));
            if hardware {
                spec_keys.push(cf(
                    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                ));
            }
        }
        let spec_values: Vec<&CFType> = spec_keys.iter().map(|_| t as &CFType).collect();
        let spec = CFDictionary::<CFString, CFType>::from_slices(&spec_keys, &spec_values);
        let nv12 = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let (w, h) = (
            CFNumber::new_i32(cfg.width as i32),
            CFNumber::new_i32(cfg.height as i32),
        );
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        let attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    cf(kCVPixelBufferPixelFormatTypeKey),
                    cf(kCVPixelBufferWidthKey),
                    cf(kCVPixelBufferHeightKey),
                    cf(kCVPixelBufferIOSurfacePropertiesKey),
                ],
                &[&nv12, &w, &h, &iosurface],
            )
        };
        let shared = Box::<Shared>::default();
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                cfg.width as i32,
                cfg.height as i32,
                kCMVideoCodecType_H264,
                CFRetained::as_ptr(&spec).as_ptr().cast(),
                CFRetained::as_ptr(&attrs).as_ptr().cast(),
                ptr::null(),
                on_output,
                &*shared as *const Shared as *mut c_void,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            return Err(format!("VTCompressionSessionCreate failed ({status})"));
        }
        let mut enc = Encoder {
            session,
            shared,
            submitted: 0,
            pts: 0,
            width: cfg.width as usize,
            height: cfg.height as usize,
            min_qp: min_qp_key(),
            applied: (-1.0, 0),
        };
        let f = CFBoolean::new(false);
        let fps = CFNumber::new_i32(cfg.fps as i32);
        // Keyframes only when voxl asks for one (a new viewer, a lost frame).
        let keyint = CFNumber::new_i32(i32::MAX);
        unsafe {
            enc.set(kVTCompressionPropertyKey_RealTime, t);
            enc.set(kVTCompressionPropertyKey_AllowFrameReordering, f);
            enc.set(
                kVTCompressionPropertyKey_ProfileLevel,
                cf::<CFType>(kVTProfileLevel_H264_High_AutoLevel),
            );
            enc.set(
                kVTCompressionPropertyKey_H264EntropyMode,
                cf::<CFType>(kVTH264EntropyMode_CABAC),
            );
            enc.set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &keyint);
            enc.set(kVTCompressionPropertyKey_ExpectedFrameRate, &fps);
            // BT.601 limited range in BT.709 primaries, as x264 marks it (the capture's
            // and libyuv's conversion).
            enc.set(
                kVTCompressionPropertyKey_ColorPrimaries,
                cf::<CFType>(kCVImageBufferColorPrimaries_ITU_R_709_2),
            );
            enc.set(
                kVTCompressionPropertyKey_TransferFunction,
                cf::<CFType>(kCVImageBufferTransferFunction_ITU_R_709_2),
            );
            enc.set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                cf::<CFType>(kCVImageBufferYCbCrMatrix_ITU_R_601_4),
            );
        }
        enc.set_rate(cfg.crf, cfg.max_kbps);
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(enc.session) };
        if status != 0 {
            return Err(format!(
                "VideoToolbox couldn't prepare to encode ({status})"
            ));
        }
        Ok(enc)
    }

    /// Sets a session property; one this encoder doesn't support is skipped.
    fn set(&self, key: CFStringRef, value: &CFType) {
        let status =
            unsafe { VTSessionSetProperty(self.session, key, (value as *const CFType).cast()) };
        if status != 0 {
            log::debug!("VideoToolbox: {} not set ({status})", unsafe {
                cf_string(key)
            });
        }
    }

    /// Quality and bitrate cap, as for x264: `crf` becomes the lowest QP a frame may use,
    /// `max_kbps` the average bitrate.
    pub fn set_rate(&mut self, crf: f32, max_kbps: u32) {
        if (crf, max_kbps) == self.applied {
            return;
        }
        if max_kbps > 0 && max_kbps != self.applied.1 {
            let bps = CFNumber::new_i32((max_kbps as u64 * 1000).min(i32::MAX as u64) as i32);
            self.set(unsafe { kVTCompressionPropertyKey_AverageBitRate }, &bps);
        }
        if let Some(key) = self.min_qp.filter(|_| crf != self.applied.0) {
            self.set(key, &CFNumber::new_i32(crf.round().clamp(1.0, 51.0) as i32));
        }
        self.applied = (crf, max_kbps);
    }

    /// As `set_rate`; VideoToolbox has no separate buffer size.
    pub fn set_rate_buf(&mut self, crf: f32, max_kbps: u32, _buf_kbit: u32) {
        self.set_rate(crf, max_kbps);
    }

    /// Encodes one picture and waits for it. Appends Annex B to `out` and returns whether
    /// it's a keyframe; a frame the encoder drops appends nothing.
    pub fn encode(
        &mut self,
        p: &Planes,
        ts_ms: i64,
        force_idr: bool,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        let pb = self.nv12(p)?;
        // Strictly increasing pts even if the wall clock stalls.
        self.pts = ts_ms.max(self.pts + 1);
        let pts = CMTime {
            value: self.pts,
            timescale: 1000,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        };
        let props = force_idr.then(|| unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[cf(kVTEncodeFrameOptionKey_ForceKeyFrame)],
                &[CFBoolean::new(true)],
            )
        });
        // Numbered, so a late callback for an earlier (dropped) frame isn't taken for it.
        self.submitted += 1;
        let n = self.submitted;
        let mut flags = 0;
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pb,
                pts,
                CMTime::default(),
                props
                    .as_ref()
                    .map_or(ptr::null(), |d| CFRetained::as_ptr(d).as_ptr().cast()),
                n as *mut c_void,
                &mut flags,
            )
        };
        // The session holds its own reference while it needs the buffer.
        unsafe { CVPixelBufferRelease(pb) };
        if status != 0 {
            return Err(format!("VTCompressionSessionEncodeFrame failed ({status})"));
        }
        if flags & kVTEncodeInfo_FrameDropped != 0 {
            return Ok(false);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut o = self.shared.out.lock();
        while o.last < n {
            if self.shared.cv.wait_until(&mut o, deadline).timed_out() {
                // Low-latency mode emits every frame at once; if this one is held back
                // anyway, have the encoder finish it (without the lock its callback takes).
                drop(o);
                unsafe { VTCompressionSessionCompleteFrames(self.session, pts) };
                o = self.shared.out.lock();
                if o.last < n {
                    return Err("VideoToolbox didn't return the frame".into());
                }
            }
        }
        if let Some(e) = o.error.take() {
            return Err(format!("VideoToolbox encode failed ({e})"));
        }
        Ok(match o.frame.take() {
            Some((data, key)) => {
                out.extend_from_slice(&data);
                key
            }
            None => false,
        })
    }

    /// The picture as an NV12 pixel buffer from the session's pool (retained).
    fn nv12(&self, p: &Planes) -> Result<CVPixelBufferRef, String> {
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session) };
        if pool.is_null() {
            return Err("VideoToolbox has no pixel buffer pool".into());
        }
        let mut pb = ptr::null_mut();
        let status = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut pb) };
        if status != 0 || pb.is_null() {
            return Err(format!("no pixel buffer for VideoToolbox ({status})"));
        }
        let (w, h) = (self.width, self.height);
        unsafe {
            CVPixelBufferLockBaseAddress(pb, 0);
            let y = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
            for row in 0..h {
                let src = &p.y[row * p.stride_y..][..w];
                std::slice::from_raw_parts_mut(y.add(row * y_stride), w).copy_from_slice(src);
            }
            let uv = CVPixelBufferGetBaseAddressOfPlane(pb, 1) as *mut u8;
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1);
            let cw = w.div_ceil(2);
            for row in 0..h.div_ceil(2) {
                let dst = std::slice::from_raw_parts_mut(uv.add(row * uv_stride), cw * 2);
                let (u, v) = (
                    &p.u[row * p.stride_uv..][..cw],
                    &p.v[row * p.stride_uv..][..cw],
                );
                for (i, pair) in dst.chunks_exact_mut(2).enumerate() {
                    pair[0] = u[i];
                    pair[1] = v[i];
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, 0);
        }
        Ok(pb)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // No callbacks after this, so `shared` can go with the encoder.
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as CFTypeRef);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NAL unit types in an Annex B stream.
    fn nal_types(mut s: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        while let Some(i) = s.windows(4).position(|w| w == [0, 0, 0, 1]) {
            s = &s[i + 4..];
            if let Some(&b) = s.first() {
                types.push(b & 0x1f);
            }
        }
        types
    }

    #[test]
    fn rewrites_avcc_as_annex_b() {
        let mut out = Vec::new();
        let avcc = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        avcc_to_annex_b(&avcc, 4, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]);
        out.clear();
        avcc_to_annex_b(&[0, 2, 0x65, 0xaa], 2, &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0xaa]);
        // A length that runs past the end.
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 0x65], 4, &mut Vec::new()).is_none());
    }

    /// Encodes a moving picture and checks what viewers get: Annex B, SPS and PPS ahead
    /// of every keyframe, and keyframes where they were asked for.
    #[test]
    fn encodes_annex_b_with_keyframes_on_request() {
        let (w, h) = (640usize, 368usize);
        let cfg = EncoderConfig {
            width: w as u32,
            height: h as u32,
            fps: 60,
            preset: "ultrafast".into(),
            threads: 1,
            crf: 24.0,
            max_kbps: 8000,
            hardware: true,
        };
        // CI's Macs are VMs without a media engine: there, VideoToolbox's software
        // encoder stands in, which is enough to check the plumbing.
        let mut enc = Encoder::new(&cfg)
            .or_else(|e| {
                eprintln!("no hardware encoder ({e}); trying VideoToolbox's software one");
                Encoder::open(&cfg, false)
            })
            .expect("a VideoToolbox H.264 encoder");
        let mut y = vec![0u8; w * h];
        let (u, v) = (vec![90u8; w * h / 4], vec![160u8; w * h / 4]);
        let mut out = Vec::new();
        for f in 0..12usize {
            for (i, p) in y.iter_mut().enumerate() {
                *p = ((i % w + f * 5) ^ (i / w)) as u8;
            }
            let planes = Planes {
                y: &y,
                u: &u,
                v: &v,
                stride_y: w,
                stride_uv: w / 2,
            };
            let want_key = f == 0 || f == 7;
            out.clear();
            let key = enc
                .encode(&planes, f as i64 * 16, want_key, &mut out)
                .unwrap();
            let types = nal_types(&out);
            assert!(!types.is_empty(), "frame {f}: no output");
            assert_eq!(
                key, want_key,
                "frame {f}: keyframe {key}, NAL types {types:?}"
            );
            if want_key {
                assert_eq!(&types[..2], [7, 8], "frame {f}: SPS and PPS first");
                assert!(types.contains(&5), "frame {f}: no IDR slice in {types:?}");
            } else {
                assert!(types.contains(&1), "frame {f}: no slice in {types:?}");
                assert!(
                    !types.contains(&7),
                    "frame {f}: parameter sets on a delta frame"
                );
            }
        }
    }
}
