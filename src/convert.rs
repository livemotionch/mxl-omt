//! Pixel formats between MXL and OMT.
//!
//! MXL video is v210: 10-bit 4:2:2, three samples per little-endian 32-bit
//! word, samples in UYVY order (`Cb0 Y0 Cr0 Y1 Cb2 Y2 ...`), rows padded to
//! 128 bytes (48 pixels). OMT hands out and takes uncompressed frames as
//! UYVY (8-bit 4:2:2, same sample order) or P216 (16-bit 4:2:2: a Y plane,
//! then an interleaved CbCr plane of the same height, values in the top
//! bits). So v210 ⇄ UYVY is a repack with a shift, v210 ⇄ P216 a repack
//! with a de-interleave; neither changes colours. P216 keeps all 10 bits
//! (OMT encodes it as high-bit-depth VMX).
//!
//! A source of another size is scaled (bilinear, per plane, 10-bit) to the
//! house format; [`Planar`] is the intermediate for that path only.

/// Bytes of a v210 row of `width` pixels (128-byte aligned).
pub fn v210_line_bytes(width: usize) -> usize {
    width.div_ceil(48) * 128
}

pub fn v210_frame_bytes(width: usize, height: usize) -> usize {
    v210_line_bytes(width) * height
}

const M: u32 = 0x3FF;

#[inline(always)]
fn word(a: u32, b: u32, c: u32) -> u32 {
    a | (b << 10) | (c << 20)
}

/// Pack one row of `n` samples (UYVY order, 10-bit) from `get` into v210.
#[inline(always)]
fn pack_row(row: &mut [u8], n: usize, get: impl Fn(usize) -> u32) {
    let full = n / 12;
    for (b, chunk) in row.chunks_exact_mut(16).take(full).enumerate() {
        let k = b * 12;
        let w = [
            word(get(k), get(k + 1), get(k + 2)),
            word(get(k + 3), get(k + 4), get(k + 5)),
            word(get(k + 6), get(k + 7), get(k + 8)),
            word(get(k + 9), get(k + 10), get(k + 11)),
        ];
        for (i, w) in w.iter().enumerate() {
            chunk[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
    // A width that isn't a multiple of 6: the last block, zero padded.
    if n % 12 != 0 {
        let k0 = full * 12;
        let s = |k: usize| if k < n { get(k) } else { 0 };
        let chunk = &mut row[full * 16..full * 16 + 16];
        for i in 0..4 {
            let k = k0 + 3 * i;
            chunk[4 * i..4 * i + 4].copy_from_slice(&word(s(k), s(k + 1), s(k + 2)).to_le_bytes());
        }
    }
}

/// Unpack one v210 row into `n` samples, calling `put(k, sample)`.
#[inline(always)]
fn unpack_row(row: &[u8], n: usize, mut put: impl FnMut(usize, u32)) {
    let words = n.div_ceil(3);
    for w in 0..words {
        let v = u32::from_le_bytes([row[4 * w], row[4 * w + 1], row[4 * w + 2], row[4 * w + 3]]);
        let k = 3 * w;
        put(k, v & M);
        if k + 1 < n {
            put(k + 1, (v >> 10) & M);
        }
        if k + 2 < n {
            put(k + 2, (v >> 20) & M);
        }
    }
}

/// UYVY (8-bit) → v210.
pub fn uyvy_to_v210(src: &[u8], stride: usize, width: usize, height: usize, dst: &mut [u8]) {
    let n = 2 * width;
    let lb = v210_line_bytes(width);
    for y in 0..height {
        let s = &src[y * stride..y * stride + n];
        pack_row(&mut dst[y * lb..(y + 1) * lb], n, |k| (s[k] as u32) << 2);
    }
}

/// P216 (16-bit planar 4:2:2) → v210.
pub fn p216_to_v210(src: &[u8], stride: usize, width: usize, height: usize, dst: &mut [u8]) {
    let n = 2 * width;
    let lb = v210_line_bytes(width);
    let uv0 = stride * height;
    for y in 0..height {
        let yr = &src[y * stride..y * stride + 2 * width];
        let cr = &src[uv0 + y * stride..uv0 + y * stride + 2 * width];
        let y16 = |i: usize| u16::from_le_bytes([yr[2 * i], yr[2 * i + 1]]) as u32 >> 6;
        let c16 = |i: usize| u16::from_le_bytes([cr[2 * i], cr[2 * i + 1]]) as u32 >> 6;
        // Sample k of the UYVY sequence: odd k is luma k/2, even k chroma
        // (pair k/4: Cb at 4q, Cr at 4q+2 → CbCr plane index k/2).
        pack_row(&mut dst[y * lb..(y + 1) * lb], n, |k| if k & 1 == 1 { y16(k >> 1) } else { c16(k >> 1) });
    }
}

/// v210 → UYVY (8-bit, rounded).
pub fn v210_to_uyvy(src: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let n = 2 * width;
    let lb = v210_line_bytes(width);
    for y in 0..height {
        let d = &mut dst[y * n..(y + 1) * n];
        unpack_row(&src[y * lb..(y + 1) * lb], n, |k, v| d[k] = ((v + 2) >> 2).min(255) as u8);
    }
}

/// v210 → P216 (Y plane, then CbCr plane; stride `2 * width`).
pub fn v210_to_p216(src: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let n = 2 * width;
    let lb = v210_line_bytes(width);
    let stride = 2 * width;
    let (yp, cp) = dst.split_at_mut(stride * height);
    for y in 0..height {
        let yr = &mut yp[y * stride..(y + 1) * stride];
        let cr = &mut cp[y * stride..(y + 1) * stride];
        unpack_row(&src[y * lb..(y + 1) * lb], n, |k, v| {
            let b = ((v << 6) as u16).to_le_bytes();
            let i = 2 * (k >> 1);
            if k & 1 == 1 {
                yr[i..i + 2].copy_from_slice(&b);
            } else {
                cr[i..i + 2].copy_from_slice(&b);
            }
        });
    }
}

/// A black v210 frame (video range: Y 64, Cb/Cr 512).
pub fn v210_black(width: usize, height: usize, dst: &mut [u8]) {
    let n = 2 * width;
    let lb = v210_line_bytes(width);
    for y in 0..height {
        pack_row(&mut dst[y * lb..(y + 1) * lb], n, |k| if k & 1 == 1 { 64 } else { 512 });
    }
}

/// A frame as three 10-bit planes (4:2:2): the scaling path.
#[derive(Default)]
pub struct Planar {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
}

impl Planar {
    fn alloc(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        let cw = width.div_ceil(2);
        self.y.resize(width * height, 0);
        self.cb.resize(cw * height, 0);
        self.cr.resize(cw * height, 0);
    }

    /// From UYVY (8-bit) or P216.
    pub fn load(&mut self, src: &[u8], stride: usize, width: usize, height: usize, p216: bool) {
        self.alloc(width, height);
        let cw = width.div_ceil(2);
        for row in 0..height {
            for x in 0..width {
                let (yv, cv) = if p216 {
                    let yo = row * stride + 2 * x;
                    let co = stride * height + row * stride + 2 * x;
                    (
                        u16::from_le_bytes([src[yo], src[yo + 1]]) >> 6,
                        u16::from_le_bytes([src[co], src[co + 1]]) >> 6,
                    )
                } else {
                    let o = row * stride + 2 * x;
                    ((src[o + 1] as u16) << 2, (src[o] as u16) << 2)
                };
                self.y[row * width + x] = yv;
                // even x: Cb, odd x: Cr of pair x/2
                if x & 1 == 0 {
                    self.cb[row * cw + x / 2] = cv;
                } else {
                    self.cr[row * cw + x / 2] = cv;
                }
            }
        }
    }

    /// Bilinear resize into `out`.
    pub fn scale_into(&self, out: &mut Planar, width: usize, height: usize) {
        out.alloc(width, height);
        let (cw_in, cw_out) = (self.width.div_ceil(2), width.div_ceil(2));
        resize(&self.y, self.width, self.height, &mut out.y, width, height);
        resize(&self.cb, cw_in, self.height, &mut out.cb, cw_out, height);
        resize(&self.cr, cw_in, self.height, &mut out.cr, cw_out, height);
    }

    pub fn to_v210(&self, dst: &mut [u8]) {
        let (w, n) = (self.width, 2 * self.width);
        let cw = w.div_ceil(2);
        let lb = v210_line_bytes(w);
        for row in 0..self.height {
            let (y, cb, cr) = (&self.y[row * w..], &self.cb[row * cw..], &self.cr[row * cw..]);
            pack_row(&mut dst[row * lb..(row + 1) * lb], n, |k| {
                (match k & 3 {
                    0 => cb[k >> 2],
                    2 => cr[k >> 2],
                    _ => y[k >> 1],
                }) as u32
            });
        }
    }
}

/// Bilinear (16.16 fixed point, pixel centres aligned).
fn resize(src: &[u16], sw: usize, sh: usize, dst: &mut [u16], dw: usize, dh: usize) {
    if sw == 0 || sh == 0 {
        return;
    }
    let map = |d: usize, sn: usize, dn: usize| -> Vec<(usize, u32)> {
        (0..d)
            .map(|i| {
                let f = ((i as f64 + 0.5) * sn as f64 / dn as f64 - 0.5).max(0.0);
                let i0 = (f as usize).min(sn - 1);
                (i0, ((f - i0 as f64) * 65536.0) as u32)
            })
            .collect()
    };
    let xs = map(dw, sw, dw);
    let ys = map(dh, sh, dh);
    for (oy, &(y0, fy)) in ys.iter().enumerate() {
        let y1 = (y0 + 1).min(sh - 1);
        let (r0, r1) = (&src[y0 * sw..(y0 + 1) * sw], &src[y1 * sw..(y1 + 1) * sw]);
        let out = &mut dst[oy * dw..(oy + 1) * dw];
        for (o, &(x0, fx)) in out.iter_mut().zip(xs.iter()) {
            let x1 = (x0 + 1).min(sw - 1);
            let top = r0[x0] as u64 * (65536 - fx as u64) + r0[x1] as u64 * fx as u64;
            let bot = r1[x0] as u64 * (65536 - fx as u64) + r1[x1] as u64 * fx as u64;
            let v = (top * (65536 - fy as u64) + bot * fy as u64 + (1 << 31)) >> 32;
            *o = v as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern_uyvy(w: usize, h: usize) -> Vec<u8> {
        (0..w * h * 2).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn uyvy_roundtrip() {
        for (w, h) in [(1920, 4), (1280, 3), (8, 2)] {
            let src = pattern_uyvy(w, h);
            let mut v = vec![0u8; v210_frame_bytes(w, h)];
            uyvy_to_v210(&src, 2 * w, w, h, &mut v);
            let mut back = vec![0u8; 2 * w * h];
            v210_to_uyvy(&v, w, h, &mut back);
            assert_eq!(src, back, "{w}x{h}");
        }
    }

    #[test]
    fn p216_roundtrip_keeps_10_bits() {
        let (w, h) = (1926, 2);
        let mut v = vec![0u8; v210_frame_bytes(w, h)];
        // arbitrary 10-bit samples
        let n = 2 * w;
        let lb = v210_line_bytes(w);
        for y in 0..h {
            pack_row(&mut v[y * lb..(y + 1) * lb], n, |k| ((k * 37 + y * 11) % 1024) as u32);
        }
        let mut p = vec![0u8; 4 * w * h];
        v210_to_p216(&v, w, h, &mut p);
        // Y of pixel 1 is sample 3
        assert_eq!(u16::from_le_bytes([p[2], p[3]]) >> 6, (3 * 37 % 1024) as u16);
        let mut v2 = vec![0u8; v210_frame_bytes(w, h)];
        p216_to_v210(&p, 2 * w, w, h, &mut v2);
        assert_eq!(v, v2);
    }

    #[test]
    fn planar_identity_and_scale() {
        let (w, h) = (1280, 4);
        let src = pattern_uyvy(w, h);
        let mut p = Planar::default();
        p.load(&src, 2 * w, w, h, false);
        let mut a = vec![0u8; v210_frame_bytes(w, h)];
        p.to_v210(&mut a);
        let mut b = vec![0u8; v210_frame_bytes(w, h)];
        uyvy_to_v210(&src, 2 * w, w, h, &mut b);
        assert_eq!(a, b);
        // flat grey stays flat when scaled
        let grey: Vec<u8> = (0..1280 * 720 * 2).map(|i| if i % 2 == 0 { 128 } else { 100 }).collect();
        p.load(&grey, 2560, 1280, 720, false);
        let mut out = Planar::default();
        p.scale_into(&mut out, 1920, 1080);
        assert!(out.y.iter().all(|&v| v == 400));
        assert!(out.cb.iter().all(|&v| v == 512));
    }

    #[test]
    fn black() {
        let mut v = vec![0u8; v210_frame_bytes(1920, 1)];
        v210_black(1920, 1, &mut v);
        let mut u = vec![0u8; 3840];
        v210_to_uyvy(&v, 1920, 1, &mut u);
        assert_eq!(&u[..4], &[128, 16, 128, 16]);
    }
}
