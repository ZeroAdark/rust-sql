//! The narrow face's differential (lane narrow, 2026-09-15): every int
//! encoding × delta / word width × lane width, `decode_narrow` against the
//! widened datums of `decode_full` narrowed (`as i16` / `as i32` — the path
//! the reader took before), bit-identical: negatives, FOR bases at the lane
//! type's bounds, values past the lane's range (the truncation the old path
//! made), all-null and mixed validity, short tails; the refusals typed.

use super::*;
use crate::bytefor::{granule_min_width, ByteForEncoder};
use crate::deltafor::DeltaForEncoder;
use crate::ffor::FforEncoder;
use pgrc_format::abi::{Face, NarrowOut};
use pgrc_format::class::StorageClass;
use pgrc_format::verbatim::VerbatimEncoder;
use pgrc_format::FormatError;

fn narrow_i16(built: &Built, g: u32) -> Vec<i16> {
    let ctx = built.ctx(g);
    let vt = crate::registry().resolve(ctx.key).expect("kernel resolves");
    let mut out = vec![0i16; ctx.rows as usize];
    assert_eq!((vt.decode_narrow)(&ctx, NarrowOut::I16(&mut out)).expect("decode_narrow i16"), ctx.rows);
    out
}

fn narrow_i32(built: &Built, g: u32) -> Vec<i32> {
    let ctx = built.ctx(g);
    let vt = crate::registry().resolve(ctx.key).expect("kernel resolves");
    let mut out = vec![0i32; ctx.rows as usize];
    assert_eq!((vt.decode_narrow)(&ctx, NarrowOut::I32(&mut out)).expect("decode_narrow i32"), ctx.rows);
    out
}

/// Both lane widths against the widened path, plus the short-output bound.
fn assert_narrow_eq(built: &Built, granules: usize, tag: &str) {
    for g in 0..granules as u32 {
        let (wide, _a) = dec_full(built, g);
        let w16: Vec<i16> = wide.iter().map(|&d| d as i16).collect();
        let w32: Vec<i32> = wide.iter().map(|&d| d as i32).collect();
        assert_eq!(narrow_i16(built, g), w16, "{tag}: i16 lane granule {g}");
        assert_eq!(narrow_i32(built, g), w32, "{tag}: i32 lane granule {g}");
        let ctx = built.ctx(g);
        let vt = crate::registry().resolve(ctx.key).expect("kernel resolves");
        if ctx.rows > 1 {
            let mut short = vec![0i16; ctx.rows as usize - 1];
            assert!(
                matches!((vt.decode_narrow)(&ctx, NarrowOut::I16(&mut short)), Err(FormatError::Bounds { .. })),
                "{tag}: a short lane is a bounds error"
            );
        }
    }
}

/// Validity shapes the corpora cycle through: none, sparse, dense, all-null.
fn validity_shape(kind: u32, rows: u32, seed: &mut u64) -> Option<Vec<u64>> {
    if kind % 4 == 3 {
        return Some(vec![0u64; rows.div_ceil(64) as usize]);
    }
    validity_pattern(kind, rows, seed)
}

#[test]
fn byte_for_narrow_equals_widened_every_delta_width_and_base() {
    let mut seed = 0x11A2_2026_u64;
    let class = StorageClass::ByvalWord { width: 8, signed: true };
    // (delta width, delta modulus); bases sit at and past the lane types' bounds.
    let widths: [(u8, u64); 8] = [(1, 200), (2, 60_000), (3, 1 << 23), (4, 3_000_000_000), (5, 1 << 39), (6, 1 << 47), (7, 1 << 55), (8, u64::MAX / 2)];
    let bases: [i64; 9] = [i16::MIN as i64, i16::MAX as i64 - 100, -7, i32::MIN as i64, i32::MAX as i64 - 100, 1_000_000, -(1i64 << 40), i64::MIN, i64::MAX - 100];
    for (wi, &(w, modu)) in widths.iter().enumerate() {
        for (bi, &base) in bases.iter().enumerate() {
            let shape = (wi + bi) as u32;
            // the base clamped so `base + modu` stays below `i64::MAX` (a wrap would widen the frame)
            let base = base.min(i64::MAX - (modu as i64).max(0) - 1);
            let gs: Vec<GranuleData> = [8192u32, 8192, [8192u32, 1500, 17, 1][bi % 4]]
                .iter()
                .map(|&rows| {
                    let validity = validity_shape(shape, rows, &mut seed);
                    let datums = (0..rows)
                        .map(|r| match shape % 3 {
                            0 => (base as u64).wrapping_add(if r % 2 == 0 { 0 } else { modu }),
                            1 => base as u64,
                            _ => (base as u64).wrapping_add(splitmix(&mut seed) % (modu + 1)),
                        })
                        .collect();
                    GranuleData { rows, datums, validity }
                })
                .collect();
            let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
            let need = inputs.iter().map(|g| granule_min_width(g, true)).max().unwrap();
            assert!(need <= w, "corpus must fit the forced width");
            let mut enc = ByteForEncoder::new_bytefor(8, w, true);
            let built = build_stream(&mut enc, &inputs, 0, true);
            assert_narrow_eq(&built, gs.len(), &format!("byte_for w{w} base {base}"));
        }
    }
}

#[test]
fn verbatim_narrow_equals_widened_every_word_width_and_sign() {
    let mut seed = 0x11A2_0DDB_u64;
    for &w in &[1u8, 2, 4, 8] {
        for &signed in &[false, true] {
            let class = StorageClass::ByvalWord { width: w, signed };
            let lim: u64 = if w == 8 { u64::MAX } else { (1u64 << (w * 8)) - 1 };
            // the closed-form value base: only the last granule may be short
            let gs: Vec<GranuleData> = [8192u32, 8192, [300u32, 1, 8192, 17][(w as usize + signed as usize) % 4]]
                .iter()
                .enumerate()
                .map(|(i, &rows)| {
                    let validity = validity_shape(w as u32 + signed as u32 + i as u32, rows, &mut seed);
                    let datums = (0..rows)
                        .map(|r| {
                            let raw = match r % 5 {
                                0 => lim,
                                1 => lim >> 1,
                                2 => 1u64 << (w * 8 - 1),
                                3 => 0,
                                _ => splitmix(&mut seed) & lim,
                            };
                            crate::section::extend_word(raw, w, signed)
                        })
                        .collect();
                    GranuleData { rows, datums, validity }
                })
                .collect();
            let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
            let built = build_stream(&mut VerbatimEncoder { class }, &inputs, 0, signed);
            assert_narrow_eq(&built, gs.len(), &format!("verbatim w{w} signed={signed}"));
        }
    }
}

#[test]
fn ffor_and_delta_for_narrow_equal_widened() {
    let mut seed = 0x11A2_FF_u64;
    let class = StorageClass::ByvalWord { width: 8, signed: true };
    for shape in 0..8u32 {
        let last = [8192u32, 1025, 64, 1][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| {
                let mut gd = roundtrip_int_corpus(shape, rows, &mut seed);
                if shape % 4 == 3 {
                    gd.validity = Some(vec![0u64; rows.div_ceil(64) as usize]);
                }
                gd
            })
            .collect();
        let signed = shape % 2 == 1;
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let built = build_stream(&mut FforEncoder { signed, carry: None }, &inputs, 0, signed);
        assert_narrow_eq(&built, gs.len(), &format!("ffor shape {shape}"));
        let built = build_stream(&mut DeltaForEncoder::default(), &inputs, 0, true);
        assert_narrow_eq(&built, gs.len(), &format!("delta_for shape {shape}"));
    }
}

#[test]
fn narrow_refuses_typed_off_the_int_encodings() {
    let mut seed = 0x11A2_B00_u64;
    let gd = roundtrip_f32_corpus(0, 512, &mut seed);
    let class = StorageClass::F32;
    let inputs = [gd.input(class)];
    let built = build_stream(&mut VerbatimEncoder { class }, &inputs, 0, false);
    let ctx = built.ctx(0);
    let vt = crate::registry().resolve(ctx.key).expect("kernel resolves");
    let mut out = vec![0i32; 512];
    assert!(matches!(
        (vt.decode_narrow)(&ctx, NarrowOut::I32(&mut out)),
        Err(FormatError::FaceUnsupported { face: Face::DecodeNarrow, .. })
    ));
}
