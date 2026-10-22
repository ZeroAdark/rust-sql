//! The DELTA_* family and BYTE_STREAM_SPLIT, normalized to the PLAIN
//! layout of the same physical type in a reusable page buffer, so the
//! batch fill runs the one PLAIN kernel set over every encoding.
//!
//! DELTA_BINARY_PACKED: header {block size, miniblocks per block, total
//! count, first value}, then blocks of {min delta, per-miniblock bit
//! widths, miniblock bodies}; arithmetic wraps in the physical width
//! (values are reconstructed in i64 and truncated for INT32, which equals
//! 32-bit wrapping arithmetic modulo 2^32).

use types_error::PgResult;

use crate::rle::corrupt_page;

/// Read a ULEB128 at `*pos` (capped at u64), bounds-checked against `end`.
#[inline]
fn uleb(buf: &[u8], end: usize, pos: &mut usize) -> PgResult<u64> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    loop {
        let Some(&b) = buf.get(*pos).filter(|_| *pos < end) else {
            return Err(corrupt_page("delta header varint overruns page"));
        };
        *pos += 1;
        if shift == 63 && b > 1 {
            return Err(corrupt_page("delta header varint overflows"));
        }
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(corrupt_page("delta header varint too long"));
        }
    }
}

#[inline]
fn zigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

/// Unpack `count` values of `width` bits (LSB-first) starting at `*pos`, each handed to `each` in order (memsig E-pub t5:
/// no mini-block scratch list; the caller keeps what it needs).
#[inline]
fn unpack_lsb(buf: &[u8], end: usize, pos: &mut usize, width: usize, count: usize, mut each: impl FnMut(i64)) -> PgResult<()> {
    let nbytes = (count * width).div_ceil(8);
    if *pos + nbytes > end {
        return Err(corrupt_page("delta miniblock overruns page"));
    }
    let src = &buf[*pos..*pos + nbytes];
    let mask: u64 = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    let mut acc: u128 = 0;
    let mut acc_bits = 0usize;
    let mut byte = 0usize;
    for _ in 0..count {
        while acc_bits < width {
            acc |= (src[byte] as u128) << acc_bits;
            byte += 1;
            acc_bits += 8;
        }
        each((acc as u64 & mask) as i64);
        acc >>= width;
        acc_bits -= width;
    }
    *pos += nbytes;
    Ok(())
}

/// Decode one DELTA_BINARY_PACKED stream at `*pos`; appends the values to
/// `out` and leaves `*pos` just past the stream. `max_width` is 32 or 64;
/// the stream's own count must equal `expected` (the page's value count),
/// checked before anything is allocated.
pub(crate) fn delta_binary_packed(
    buf: &[u8],
    end: usize,
    pos: &mut usize,
    max_width: usize,
    expected: usize,
    out: &mut Vec<i64>,
) -> PgResult<usize> {
    let block_size = uleb(buf, end, pos)? as usize;
    let miniblocks = uleb(buf, end, pos)? as usize;
    let total = uleb(buf, end, pos)? as usize;
    if block_size == 0 || miniblocks == 0 || block_size % 128 != 0 || block_size % miniblocks != 0 {
        return Err(Box::new(types_error::PgError::error(format!( // alloc: Cold
            "corrupt parquet page: invalid delta block shape (block size {block_size}, {miniblocks} miniblocks, {total} values)"
        ))
        .with_sqlstate(types_error::ERRCODE_BAD_COPY_FILE_FORMAT)));
    }
    let per_mini = block_size / miniblocks;
    if per_mini % 32 != 0 {
        return Err(corrupt_page("delta miniblock size is not a multiple of 32"));
    }
    if total != expected {
        return Err(corrupt_page("delta value count does not match the page"));
    }
    // The first value is always present, even for an empty stream (every
    // writer emits it; every reader consumes it unconditionally).
    let first = zigzag(uleb(buf, end, pos)?);
    if total == 0 {
        return Ok(0);
    }
    out.try_reserve(total).map_err(|_| corrupt_page("out of memory decoding delta values"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    out.push(first); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    let mut prev = first;
    let mut left = total - 1;
    let mut widths = [0u8; 64];
    if miniblocks > widths.len() {
        return Err(corrupt_page("too many miniblocks per delta block"));
    }
    while left > 0 {
        let min_delta = zigzag(uleb(buf, end, pos)?);
        if *pos + miniblocks > end {
            return Err(corrupt_page("delta bit widths overrun page"));
        }
        widths[..miniblocks].copy_from_slice(&buf[*pos..*pos + miniblocks]);
        *pos += miniblocks;
        for &w in &widths[..miniblocks] {
            if left == 0 {
                break;
            }
            let w = w as usize;
            if w > max_width {
                return Err(corrupt_page("delta miniblock bit width exceeds the type"));
            }
            let take = left.min(per_mini);
            // A used miniblock is written in full even when only partly
            // filled; the trailing values are padding (unpacked, not kept).
            let mut k = 0usize;
            unpack_lsb(buf, end, pos, w, per_mini, |d| {
                if k < take {
                    prev = prev.wrapping_add(min_delta).wrapping_add(d);
                    out.push(prev); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
                }
                k += 1;
            })?;
            left -= take;
        }
    }
    Ok(total)
}

/// DELTA_BINARY_PACKED page -> PLAIN INT32/INT64 layout in `plain`.
pub(crate) fn delta_ints_to_plain(
    buf: &[u8],
    start: usize,
    end: usize,
    width: usize,
    expected: usize,
    plain: &mut Vec<u8>,
    scratch: &mut Vec<i64>,
) -> PgResult<()> {
    let mut pos = start;
    scratch.clear();
    let n = delta_binary_packed(buf, end, &mut pos, width * 8, expected, scratch)?;
    plain.clear();
    plain.try_reserve(n * width).map_err(|_| corrupt_page("out of memory decoding delta values"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    if width == 4 {
        for &v in scratch.iter() {
            plain.extend_from_slice(&(v as i32).to_le_bytes()); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        }
    } else {
        for &v in scratch.iter() {
            plain.extend_from_slice(&v.to_le_bytes()); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        }
    }
    Ok(())
}

/// DELTA_LENGTH_BYTE_ARRAY page -> PLAIN BYTE_ARRAY layout in `plain`.
pub(crate) fn delta_length_to_plain(
    buf: &[u8],
    start: usize,
    end: usize,
    expected: usize,
    plain: &mut Vec<u8>,
    scratch: &mut Vec<i64>,
) -> PgResult<()> {
    let mut pos = start;
    scratch.clear();
    let n = delta_binary_packed(buf, end, &mut pos, 32, expected, scratch)?;
    let mut total: usize = 0;
    for &l in scratch.iter() {
        if l < 0 || l > i32::MAX as i64 {
            return Err(corrupt_page("negative string length in delta stream"));
        }
        total = total.checked_add(l as usize).ok_or_else(|| corrupt_page("string lengths overflow"))?;
    }
    if pos + total > end {
        return Err(corrupt_page("delta string bytes overrun page"));
    }
    plain.clear();
    plain.try_reserve(total + 4 * n).map_err(|_| corrupt_page("out of memory decoding delta strings"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    for &l in scratch.iter() {
        let l = l as usize;
        plain.extend_from_slice(&(l as u32).to_le_bytes()); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        plain.extend_from_slice(&buf[pos..pos + l]); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        pos += l;
    }
    Ok(())
}

/// DELTA_BYTE_ARRAY page (prefix lengths + DELTA_LENGTH suffixes) -> PLAIN
/// BYTE_ARRAY layout, or PLAIN FLBA layout when `flba` is the fixed width.
pub(crate) fn delta_byte_array_to_plain(
    buf: &[u8],
    start: usize,
    end: usize,
    expected: usize,
    flba: Option<usize>,
    plain: &mut Vec<u8>,
    scratch: &mut Vec<i64>,
    scratch2: &mut Vec<i64>,
) -> PgResult<()> {
    let mut pos = start;
    scratch.clear();
    let n = delta_binary_packed(buf, end, &mut pos, 32, expected, scratch)?;
    scratch2.clear();
    let m = delta_binary_packed(buf, end, &mut pos, 32, n, scratch2)?;
    debug_assert_eq!(m, n);
    plain.clear();
    let mut prev_start = 0usize;
    let mut prev_len = 0usize;
    for i in 0..n {
        let (p, s) = (scratch[i], scratch2[i]);
        if p < 0 || s < 0 || p > i32::MAX as i64 || s > i32::MAX as i64 {
            return Err(corrupt_page("negative length in delta byte array stream"));
        }
        let (p, s) = (p as usize, s as usize);
        if p > prev_len {
            return Err(corrupt_page("delta prefix longer than the previous value"));
        }
        if pos + s > end {
            return Err(corrupt_page("delta suffix overruns page"));
        }
        let len = p + s;
        let this_start = match flba {
            None => {
                plain.try_reserve(4 + len).map_err(|_| corrupt_page("out of memory decoding delta strings"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
                plain.extend_from_slice(&(len as u32).to_le_bytes()); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
                plain.len()
            }
            Some(w) => {
                if len != w {
                    return Err(corrupt_page("delta value length differs from the fixed width"));
                }
                plain.try_reserve(len).map_err(|_| corrupt_page("out of memory decoding delta values"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
                plain.len()
            }
        };
        // Prefix copy from the previous value (already in `plain`), then
        // the suffix from the stream.
        plain.extend_from_within(prev_start..prev_start + p); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        plain.extend_from_slice(&buf[pos..pos + s]); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
        pos += s;
        prev_start = this_start;
        prev_len = len;
    }
    Ok(())
}

/// BYTE_STREAM_SPLIT page (`width` streams of `n` bytes) -> PLAIN layout.
pub(crate) fn byte_stream_split_to_plain(
    buf: &[u8],
    start: usize,
    end: usize,
    width: usize,
    n: usize,
    plain: &mut Vec<u8>,
) -> PgResult<()> {
    let need = n.checked_mul(width).ok_or_else(|| corrupt_page("value count overflow"))?;
    if start + need != end {
        return Err(corrupt_page("byte stream split page size does not match its values"));
    }
    plain.clear();
    plain.try_reserve(need).map_err(|_| corrupt_page("out of memory decoding values"))?; // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    plain.resize(need, 0); // alloc: Exec charged the cursor's charge (ColumnCursor::held_bytes: plain / tmp64 / tmp64b, synced after the page opens)
    let src = &buf[start..end];
    for k in 0..width {
        let stream = &src[k * n..(k + 1) * n];
        for (i, &b) in stream.iter().enumerate() {
            plain[i * width + k] = b;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_uleb(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    fn push_zig(out: &mut Vec<u8>, v: i64) {
        push_uleb(out, ((v << 1) ^ (v >> 63)) as u64);
    }

    /// Reference encoder (tests only): block 128, 4 miniblocks of 32;
    /// deltas wrap in `bits` (32 for INT32 columns, as writers do).
    fn encode(values: &[i64]) -> Vec<u8> {
        encode_w(values, 64)
    }

    fn wrap(v: i64, bits: u32) -> i64 {
        if bits == 64 { v } else { (v as i32) as i64 }
    }

    fn encode_w(values: &[i64], bits: u32) -> Vec<u8> {
        let mut out = Vec::new();
        push_uleb(&mut out, 128);
        push_uleb(&mut out, 4);
        push_uleb(&mut out, values.len() as u64);
        push_zig(&mut out, values.first().copied().unwrap_or(0));
        if values.is_empty() {
            return out;
        }
        let deltas: Vec<i64> =
            values.windows(2).map(|w| wrap(w[1].wrapping_sub(w[0]), bits)).collect();
        for block in deltas.chunks(128) {
            let min = *block.iter().min().unwrap();
            push_zig(&mut out, min);
            let mut widths = [0u8; 4];
            let mut bodies: Vec<Vec<u8>> = Vec::new();
            for (mi, mb) in block.chunks(32).enumerate() {
                let rel: Vec<u64> = mb
                    .iter()
                    .map(|&d| {
                        let r = d.wrapping_sub(min);
                        if bits == 64 { r as u64 } else { (r as u32) as u64 }
                    })
                    .collect();
                let maxv = rel.iter().copied().max().unwrap_or(0);
                let w = if maxv == 0 { 0 } else { 64 - maxv.leading_zeros() as usize };
                widths[mi] = w as u8;
                let mut body = Vec::new();
                let mut acc: u128 = 0;
                let mut bits = 0usize;
                for i in 0..32 {
                    let v = rel.get(i).copied().unwrap_or(0);
                    acc |= (v as u128) << bits;
                    bits += w;
                    while bits >= 8 {
                        body.push((acc & 0xff) as u8);
                        acc >>= 8;
                        bits -= 8;
                    }
                }
                if bits > 0 {
                    body.push((acc & 0xff) as u8);
                }
                bodies.push(body);
            }
            out.extend_from_slice(&widths);
            for b in bodies {
                out.extend_from_slice(&b);
            }
        }
        out
    }

    #[test]
    fn roundtrip_shapes() {
        for n in [0usize, 1, 2, 31, 32, 33, 127, 128, 129, 1000] {
            let values: Vec<i64> = (0..n as i64)
                .map(|i| (i * 7919) % 1000 - 500 + if i % 3 == 0 { 1 << 40 } else { 0 })
                .collect();
            let buf = encode(&values);
            let mut pos = 0;
            let mut out = Vec::new();
            let got = delta_binary_packed(&buf, buf.len(), &mut pos, 64, n, &mut out).unwrap();
            assert_eq!(got, n);
            assert_eq!(out, values, "n={n}");
            assert_eq!(pos, buf.len(), "n={n}");
        }
    }

    #[test]
    fn int32_wraps() {
        let values: Vec<i64> = vec![i32::MAX as i64, i32::MIN as i64, 0, -1];
        let buf = encode_w(&values, 32);
        let mut plain = Vec::new();
        let mut scratch = Vec::new();
        delta_ints_to_plain(&buf, 0, buf.len(), 4, 4, &mut plain, &mut scratch).unwrap();
        let got: Vec<i32> = plain.chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(got, [i32::MAX, i32::MIN, 0, -1]);
    }

    #[test]
    fn truncations_fail_cleanly() {
        let values: Vec<i64> = (0..300).collect();
        let buf = encode(&values);
        for cut in 0..buf.len() {
            let mut pos = 0;
            let mut out = Vec::new();
            assert!(delta_binary_packed(&buf, cut, &mut pos, 64, 300, &mut out).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn byte_stream_split_transposes() {
        // Two f32 values 1.0 and 2.0 as four byte streams.
        let a = 1.0f32.to_le_bytes();
        let b = 2.0f32.to_le_bytes();
        let buf = [a[0], b[0], a[1], b[1], a[2], b[2], a[3], b[3]];
        let mut plain = Vec::new();
        byte_stream_split_to_plain(&buf, 0, 8, 4, 2, &mut plain).unwrap();
        assert_eq!(&plain[..4], &a);
        assert_eq!(&plain[4..], &b);
    }
}
