//! AVX2 / SSSE3 v210 → 8-bit unpack, from lm-mxl-mixer's
//! `src/compositor/simd10.rs` (MIT). The plain twin is `convert::v210_to_uyvy`'s scalar loop; the
//! tests compare them.

#![cfg(target_arch = "x86_64")]
#![allow(clippy::missing_safety_doc)]

use std::arch::x86_64::*;

const M: i32 = 0x3FF;
const Z: i8 = -128;

#[inline(always)]
unsafe fn ld(m: &[i8; 16]) -> __m128i {
    unsafe { _mm_loadu_si128(m.as_ptr() as *const __m128i) }
}

#[inline(always)]
unsafe fn ld2(m: &[i8; 16]) -> __m256i {
    unsafe { _mm256_broadcastsi128_si256(ld(m)) }
}

/// v210 words straight to 8-bit samples (`min((v + 2) >> 2, 255)`): per
/// 4 words, the fields in 16-bit lanes (as the unpack), rounded and shifted
/// there, saturated to bytes by `packuswb`, ordered by one byte shuffle: 12
/// bytes, stored 16 at a time (each store stays inside `out`; the next one
/// overwrites its last 4 bytes). Returns the words done.
#[target_feature(enable = "ssse3,sse4.1")]
pub unsafe fn unpack_words_u8_ssse3(src: &[u8], out: &mut [u8]) -> usize {
    let words = (src.len() / 4).min(out.len() / 3);
    let steps = if out.len() >= 16 { ((out.len() - 16) / 12 + 1).min(words / 4) } else { 0 };
    unsafe {
        let m = _mm_set1_epi32(M);
        let two = _mm_set1_epi16(2);
        let order = ld(&U8_ORDER);
        for r in 0..steps {
            let v = _mm_loadu_si128(src.as_ptr().add(16 * r) as *const __m128i);
            let lo = _mm_or_si128(_mm_and_si128(v, m), _mm_slli_epi32(_mm_and_si128(_mm_srli_epi32(v, 10), m), 16));
            let hi = _mm_and_si128(_mm_srli_epi32(v, 20), m);
            let lo = _mm_srli_epi16(_mm_add_epi16(lo, two), 2);
            let hi = _mm_srli_epi16(_mm_add_epi16(hi, two), 2);
            let b = _mm_shuffle_epi8(_mm_packus_epi16(lo, hi), order);
            _mm_storeu_si128(out.as_mut_ptr().add(12 * r) as *mut __m128i, b);
        }
    }
    4 * steps
}

/// `packuswb` of `[a0 a1 b0 b1 c0 c1 d0 d1]` and `[a2 _ b2 _ c2 _ d2 _]`
/// to `a0 a1 a2 b0 b1 b2 …`.
#[rustfmt::skip]
const U8_ORDER: [i8; 16] = [0, 1, 8, 2, 3, 10, 4, 5, 12, 6, 7, 14, Z, Z, Z, Z];

/// [`unpack_words_u8_ssse3`] with 8 words per round (two 12-byte stores).
#[target_feature(enable = "avx2")]
pub unsafe fn unpack_words_u8_avx2(src: &[u8], out: &mut [u8]) -> usize {
    let words = (src.len() / 4).min(out.len() / 3);
    // Round r writes 16 bytes at 24 r and at 24 r + 12.
    let rounds = if out.len() >= 28 { ((out.len() - 28) / 24 + 1).min(words / 8) } else { 0 };
    unsafe {
        let m = _mm256_set1_epi32(M);
        let two = _mm256_set1_epi16(2);
        let order = ld2(&U8_ORDER);
        for r in 0..rounds {
            let v = _mm256_loadu_si256(src.as_ptr().add(32 * r) as *const __m256i);
            let lo = _mm256_or_si256(
                _mm256_and_si256(v, m),
                _mm256_slli_epi32(_mm256_and_si256(_mm256_srli_epi32(v, 10), m), 16),
            );
            let hi = _mm256_and_si256(_mm256_srli_epi32(v, 20), m);
            let lo = _mm256_srli_epi16(_mm256_add_epi16(lo, two), 2);
            let hi = _mm256_srli_epi16(_mm256_add_epi16(hi, two), 2);
            let b = _mm256_shuffle_epi8(_mm256_packus_epi16(lo, hi), order);
            let o = out.as_mut_ptr().add(24 * r);
            _mm_storeu_si128(o as *mut __m128i, _mm256_castsi256_si128(b));
            _mm_storeu_si128(o.add(12) as *mut __m128i, _mm256_extracti128_si256(b, 1));
        }
    }
    8 * rounds
}
