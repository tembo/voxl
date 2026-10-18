//! BGRX -> I420 conversion via libyuv, split across threads by row bands.

extern "C" {
    fn ARGBToI420(
        src_argb: *const u8,
        src_stride_argb: i32,
        dst_y: *mut u8,
        dst_stride_y: i32,
        dst_u: *mut u8,
        dst_stride_u: i32,
        dst_v: *mut u8,
        dst_stride_v: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

#[derive(Clone)]
pub struct I420 {
    pub width: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        I420 {
            width,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }
    pub fn stride_uv(&self) -> usize {
        self.width.div_ceil(2)
    }

    pub fn height(&self) -> usize {
        self.y.len() / self.width.max(1)
    }

    /// RGB (8 bits per channel), from BT.601 limited range as converted for encoding.
    pub fn to_rgb(&self) -> Vec<u8> {
        let (w, h, suv) = (self.width, self.height(), self.stride_uv());
        let mut out = vec![0u8; w * h * 3];
        let clamp = |v: i32| (v >> 8).clamp(0, 255) as u8;
        for (row, line) in out.chunks_exact_mut(w * 3).enumerate() {
            let (yr, cr) = (&self.y[row * w..][..w], (row / 2) * suv);
            for (x, px) in line.chunks_exact_mut(3).enumerate() {
                let c = 298 * (yr[x] as i32 - 16);
                let d = self.u[cr + x / 2] as i32 - 128;
                let e = self.v[cr + x / 2] as i32 - 128;
                px[0] = clamp(c + 409 * e + 128);
                px[1] = clamp(c - 100 * d - 208 * e + 128);
                px[2] = clamp(c + 516 * d + 128);
            }
        }
        out
    }
}

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    fn get(&self) -> *mut u8 {
        self.0
    }
}

/// Convert rows [y0, y1) (even-aligned) of `src` into `dst`.
pub fn convert_band(
    src: &[u8],
    src_stride: usize,
    dst: &mut I420,
    y0: usize,
    y1: usize,
    threads: usize,
) {
    let rows = y1 - y0;
    if rows == 0 {
        return;
    }
    let w = dst.width;
    let suv = dst.stride_uv();
    let (yp, up, vp) = (
        SendPtr(dst.y.as_mut_ptr()),
        SendPtr(dst.u.as_mut_ptr()),
        SendPtr(dst.v.as_mut_ptr()),
    );
    let src_p = SendPtr(src.as_ptr() as *mut u8);
    let n = threads.clamp(1, (rows / 64).max(1));
    let chunk = (rows.div_ceil(n) + 1) & !1;
    let run = |a: usize, b: usize| unsafe {
        ARGBToI420(
            src_p.get().add(a * src_stride),
            src_stride as i32,
            yp.get().add(a * w),
            w as i32,
            up.get().add((a / 2) * suv),
            suv as i32,
            vp.get().add((a / 2) * suv),
            suv as i32,
            w as i32,
            (b - a) as i32,
        );
    };
    if n == 1 {
        run(y0, y1);
        return;
    }
    std::thread::scope(|s| {
        let mut a = y0;
        while a < y1 {
            let b = (a + chunk).min(y1);
            let run = &run;
            if b == y1 {
                run(a, b);
            } else {
                s.spawn(move || run(a, b));
            }
            a = b;
        }
    });
}
