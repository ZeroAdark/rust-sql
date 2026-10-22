//! Road code.sparse's host kernel (cards/M3.md L8, M3-plan.md §6; the lab's `fold_nonbase_*`): the non-base
//! select over one pgrc DictCodes frame. `granule_source.rs` runs it in place of decode + the row selection for a leading
//! code qual whose per-code table rejects a granule's block base (VERDICT_CODE_SPARSE_TABLE, V4R-137).
//! Ported from M3-HOSTS' WIP 70c982fee63 (dropped there). Created by M3 (owner M3-LEVERS-A; sub-file of v4/m3.rs).
mod raw;

use pgrc_format::enc::EncodingId;
use pgrc_read::{ReadResult, StreamCursor};

/// DictCodes' block header right before a granule's first frame: base u32 LE, width u8, 3 pad (pgrc_codec dictcodes.rs).
const BLOCK_HDR_LEN: usize = 8;

/// Granule `lg`'s DictCodes block read in place (`StreamCursor::framed`): when `drops(base)` says every row holding the
/// block's base fails the filter (the zone form: the base is the part's '' code; the table form, H08: the verdict table
/// rejects it; a NULL slot holds the base too) its non-base rows whose code `keeps` passes land at their positions in
/// `codes` and their bits in `mask` (cleared first; rows past `rows` stay clear): the zone form keeps every non-base row,
/// the table form only those whose code the table does not reject for certain, so the selection is the qual's survivors
/// (and its faults), not every non-base row, and the lanes gathered beside it are read for those alone. `Some(the
/// non-base rows `keeps` dropped)` (0 for the zone form, whose keep test is constant: no count per row); `None` when the
/// stream is no framed DictCodes root stream, `drops` keeps the block's base or the block does not read cleanly: the
/// caller decodes whole (and reports any fault there).
pub fn sparse_block(cur: &mut StreamCursor, lg: u32, rows: usize, drops: impl Fn(u32) -> bool, keeps: impl Fn(u32) -> bool, codes: &mut [u32], mask: &mut [u64]) -> ReadResult<Option<usize>> {
    let Some(f) = cur.framed(lg, EncodingId::DictCodes as u16)? else {
        return Ok(None);
    };
    let first = f.frame_table.get(f.fbase as usize).map_or(0, |&o| o as usize);
    let Some(bh) = first.checked_sub(BLOCK_HDR_LEN).and_then(|h| f.payload.get(h..first)).filter(|_| f.values as usize == rows) else {
        return Ok(None);
    };
    let (base, width) = (u32::from_le_bytes([bh[0], bh[1], bh[2], bh[3]]), bh[4]);
    let need = (rows * width as usize).div_ceil(8);
    let Some(bits) = f.payload.get(first..first + need).filter(|_| width <= 32 && codes.len() >= rows && mask.len() * 64 >= rows && drops(base)) else {
        return Ok(None);
    };
    mask.fill(0);
    let mut dropped = 0;
    nonbase_select(base, width, bits, rows, &mut |r, c| {
        if keeps(c) {
            codes[r] = c;
            mask[r >> 6] |= 1 << (r & 63);
        } else {
            dropped += 1;
        }
    });
    Ok(Some(dropped))
}

/// Road code.sparse's select (cards/M3.md L8, the lab's `fold_nonbase_wide`): the rows of a code frame (`values` codes of
/// `width` bits, LSB-first, each `base` + its field) whose code is not `base`, each with its code, to `f` in row order. At
/// widths 1-16 NEON unpacks 16 codes per step with one zero test; wider fields walk the 64-bit words, a zero word
/// retiring 64 / width codes at once. Width 0 is the single code `base`: no row.
pub fn nonbase_select(base: u32, width: u8, bits: &[u8], values: usize, f: &mut impl FnMut(usize, u32)) {
    let wu = width as usize;
    if wu == 0 {
        return;
    }
    let bits = &bits[..(values * wu).div_ceil(8).min(bits.len())];
    #[cfg(target_arch = "aarch64")]
    if wu <= 16 {
        let i = raw::neon(base, wu, bits, values, f);
        return tail(base, wu, bits, i, values, f);
    }
    let (mask, inv, n) = ((1u64 << wu) - 1, (65536 + wu - 1) / wu, bits.len());
    // the codes whose whole 8-byte window is readable; floor(x / wu) == (x * inv) >> 16 for every x < 64
    let fast = if n >= 8 { (((n - 8) * 8 + 7) / wu + 1).min(values) } else { 0 };
    let mut i = 0;
    while i < fast {
        let (bo, ws) = (i * wu, i * wu & !63);
        // code i < fast: its word's 8 bytes lie inside `bits`
        let word = raw::word(bits, ws >> 3);
        let b = bo - ws;
        let nf = ((64 - b) / wu).min(fast - i);
        let mut w = if nf * wu < 64 { word >> b & ((1u64 << (nf * wu)) - 1) } else { word >> b };
        while w != 0 {
            let j = (w.trailing_zeros() as usize * inv) >> 16;
            f(i + j, base + (w >> (j * wu) & mask) as u32);
            w &= !(mask << (j * wu));
        }
        i += nf;
        if i < fast && b + nf * wu < 64 {
            // a field straddling the word: its own window
            let c = field(bits, i * wu, mask);
            if c != 0 {
                f(i, base + c);
            }
            i += 1;
        }
    }
    tail(base, wu, bits, i, values, f)
}

/// The field at bit `bo` of `bits` (zeros past its end).
fn field(bits: &[u8], bo: usize, mask: u64) -> u32 {
    let (byte, mut w8) = (bo >> 3, [0u8; 8]);
    let end = (byte + 8).min(bits.len());
    if byte < end {
        w8[..end - byte].copy_from_slice(&bits[byte..end]);
    }
    (u64::from_le_bytes(w8) >> (bo & 7) & mask) as u32
}

/// Codes `i..values` one by one (the vector forms' tail).
fn tail(base: u32, wu: usize, bits: &[u8], mut i: usize, values: usize, f: &mut impl FnMut(usize, u32)) {
    while i < values {
        let c = field(bits, i * wu, (1u64 << wu) - 1);
        if c != 0 {
            f(i, base + c);
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Random frames of every width 1-24 (sparse, dense and all-base), against a scalar reference: every non-base row
    /// once, in row order, with its code.
    #[test]
    fn nonbase_select_matches_the_scalar_reference_at_every_width() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        for wu in 1..=24usize {
            for (values, density) in [(0usize, 2u64), (1, 2), (15, 2), (16, 2), (17, 3), (1000, 2), (1024, 50), (4099, 1000), (512, u64::MAX)] {
                let codes: Vec<u64> = (0..values).map(|_| if density != u64::MAX && rnd() % density == 0 { rnd() & ((1 << wu) - 1) } else { 0 }).collect(); // alloc: Cold
                let mut bits = vec![0u8; (values * wu).div_ceil(8) + 3]; // alloc: Cold
                for (i, &c) in codes.iter().enumerate() {
                    for b in 0..wu {
                        bits[(i * wu + b) / 8] |= ((c >> b & 1) as u8) << ((i * wu + b) % 8);
                    }
                }
                let want: Vec<(usize, u32)> = codes.iter().enumerate().filter(|c| *c.1 != 0).map(|(i, &c)| (i, 7 + c as u32)).collect(); // alloc: Cold
                let mut got = Vec::new(); // alloc: Cold
                nonbase_select(7, wu as u8, &bits, values, &mut |r, c| got.push((r, c)));
                assert_eq!(got, want, "width {wu}, {values} codes");
            }
        }
        let mut none = 0;
        nonbase_select(7, 0, &[], 100, &mut |_, _| none += 1);
        assert_eq!(none, 0, "width 0: every code the base");
    }
}
