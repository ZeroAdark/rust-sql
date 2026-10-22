//! Definition / repetition level decoding at any depth: the RLE hybrid
//! (bit width from the column's maximum level) and the deprecated
//! BIT_PACKED encoding (MSB-first packing, no run structure). Levels are
//! decoded whole-page into reusable lanes; value kernels then consume
//! present values against them.

use types_error::PgResult;

use crate::page::Enc;
use crate::rle::{corrupt_page, HybridState};

/// Bit width that holds levels `0..=max`.
#[inline]
pub(crate) fn level_width(max: u16) -> usize {
    if max == 0 {
        0
    } else {
        (u16::BITS - max.leading_zeros()) as usize
    }
}

pub(crate) fn decode_levels(
    enc: Enc,
    buf: &[u8],
    start: usize,
    end: usize,
    max: u16,
    n: usize,
    out: &mut Vec<u16>,
) -> PgResult<usize> {
    out.clear();
    out.try_reserve(n).map_err(|_| corrupt_page("out of memory decoding levels"))?; // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
    let width = level_width(max);
    match enc {
        Enc::Rle => {
            let mut st = HybridState::new(start, end, width)?;
            let mut tmp = [0u32; 1024];
            let mut left = n;
            while left > 0 {
                let k = left.min(tmp.len());
                st.fill(buf, &mut tmp[..k])?;
                for &v in &tmp[..k] {
                    if v > u32::from(max) {
                        return Err(corrupt_page("level exceeds the column's maximum"));
                    }
                    out.push(v as u16); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
                }
                left -= k;
            }
            Ok(end)
        }
        Enc::BitPacked => {
            // Deprecated form: `width` bits per value, packed from the most
            // significant bit of each byte; the byte length is implied.
            let nbytes = (n * width).div_ceil(8);
            if start + nbytes > end {
                return Err(corrupt_page("bit-packed levels overrun page"));
            }
            let src = &buf[start..start + nbytes];
            let mut bit = 0usize;
            for _ in 0..n {
                let mut v: u32 = 0;
                for _ in 0..width {
                    let b = src[bit >> 3];
                    v = (v << 1) | u32::from((b >> (7 - (bit & 7))) & 1);
                    bit += 1;
                }
                if v > u32::from(max) {
                    return Err(corrupt_page("level exceeds the column's maximum"));
                }
                out.push(v as u16); // alloc: Exec charged the cursor's charge (ColumnCursor / FastCol held_bytes)
            }
            Ok(start + nbytes)
        }
        other => Err(crate::meta::unsupported(format!("level encoding {}", other.name()))), // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(level_width(0), 0);
        assert_eq!(level_width(1), 1);
        assert_eq!(level_width(2), 2);
        assert_eq!(level_width(3), 2);
        assert_eq!(level_width(4), 3);
        assert_eq!(level_width(255), 8);
    }

    #[test]
    fn bit_packed_msb_first() {
        // width 2, values 0,1,2,3,3,2,1,0 -> 00011011 11100100
        let buf = [0b0001_1011u8, 0b1110_0100];
        let mut out = Vec::new();
        let end = decode_levels(Enc::BitPacked, &buf, 0, 2, 3, 8, &mut out).unwrap();
        assert_eq!(out, [0, 1, 2, 3, 3, 2, 1, 0]);
        assert_eq!(end, 2);
        // width 3, five values 5,1,7,0,2 -> 101 001 111 000 010 (15 bits, 2 bytes)
        let buf = [0b1010_0111u8, 0b1000_0100];
        let end = decode_levels(Enc::BitPacked, &buf, 0, 2, 7, 5, &mut out).unwrap();
        assert_eq!(out, [5, 1, 7, 0, 2]);
        assert_eq!(end, 2);
    }

    #[test]
    fn rle_levels_reject_out_of_range() {
        // RLE run of 4 x value 3 with width 2 while max is 2.
        let buf = [0x08u8, 0x03];
        let mut out = Vec::new();
        assert!(decode_levels(Enc::Rle, &buf, 0, 2, 2, 4, &mut out).is_err());
        assert!(decode_levels(Enc::Rle, &buf, 0, 2, 3, 4, &mut out).is_ok());
        assert_eq!(out, [3, 3, 3, 3]);
    }
}
