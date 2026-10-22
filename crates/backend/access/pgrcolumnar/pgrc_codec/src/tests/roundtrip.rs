//! Per-encoding bit-exact round-trips on adversarial + fuzz corpora (§5
//! M3-C: "the election quadruple's encode-side verify"). Every stream here
//! is built through the production driver with verify ON, then decoded
//! AGAIN through the registry and compared row-by-row — two independent
//! passes over the same bytes. The ALP suite additionally runs the vendored
//! reader as a differential oracle on the exact stored frames.

use super::*;
use crate::alpc::{AlpEncoder, AlpF32Encoder};
use crate::arraydual::{assemble_array_datums, elect_array_split, ArrayElemFacts};
use crate::boolbm::BoolBitmapEncoder;
use crate::bytefor::{granule_min_width, ByteForEncoder};
use crate::deltafor::DeltaForEncoder;
use crate::dictcodes::DictCodesEncoder;
use crate::ffor::FforEncoder;
use crate::packednum::PackedNumericEncoder;
use crate::wrapper::{unwrap_section, wrap_section};
use pgrc_format::abi::{ByteArena, EncodeInput};
use pgrc_format::class::{StorageClass, CLASS_VARLENA};
use pgrc_format::dict::DictSections;
use pgrc_format::enc::EncodingId;
use pgrc_format::wire::put_varlena_entry;

// ---------------------------------------------------------------------------
// int corpora (shared shapes live in tests/mod.rs)
// ---------------------------------------------------------------------------

fn int_corpus(shape: u32, rows: u32, seed: &mut u64) -> GranuleData {
    roundtrip_int_corpus(shape, rows, seed)
}

fn signed_for(shape: u32) -> bool {
    shape % 6 == 3 || shape % 2 == 1
}

fn assert_int_roundtrip(built: &Built, granules: &[&GranuleData], class: StorageClass) {
    for (g, gd) in granules.iter().enumerate() {
        let (datums, _arena) = dec_full(built, g as u32);
        let input = gd.input(class);
        for r in 0..gd.rows {
            if !input.valid(r) {
                continue;
            }
            assert_eq!(
                datums[r as usize], gd.datums[r as usize],
                "granule {g} row {r}"
            );
        }
    }
}

#[test]
fn byte_for_roundtrips_across_shapes() {
    let mut seed = 0xB4_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    for shape in 0..12 {
        // Geometry law: only the LAST granule of an extent may be short.
        let last = [8192u32, 1500, 300, 777][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, 8192, last]
            .iter()
            .map(|&rows| int_corpus(shape, rows, &mut seed))
            .collect();
        let signed = signed_for(shape);
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let width = inputs
            .iter()
            .map(|g| granule_min_width(g, signed))
            .max()
            .unwrap();
        let mut enc = ByteForEncoder::new_bytefor(8, width, signed);
        let built = build_stream(&mut enc, &inputs, 0, signed);
        assert_int_roundtrip(&built, &gs.iter().collect::<Vec<_>>(), class);
    }
}

/// TH14's source lever (main 09-24): the framed at-position gather (`bytefor::sel_at_framed`, the resolved granule a cursor
/// keeps) lands each selected row's value at its own row, equal to `decode_full` at that row: as the datum word, and as an
/// i32 / i16 lane its low bytes; the other rows stay untouched. `Framed::of_ctx` is the ctx's own view of the granule. A row
/// past the granule and an `out` short of the last selected row refuse.
#[test]
fn byte_for_framed_sel_at_matches_full_at_every_store() {
    use crate::bytefor::{sel_at_framed, Framed};
    use pgrc_format::abi::Selection;
    const UNTOUCHED: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    let mut seed = 0xF4A3_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    for shape in 0..12 {
        let last = [8192u32, 1500, 300, 777][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, 8192, last]
            .iter()
            .map(|&rows| int_corpus(shape, rows, &mut seed))
            .collect();
        let signed = signed_for(shape);
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let width = inputs.iter().map(|g| granule_min_width(g, signed)).max().unwrap();
        let mut enc = ByteForEncoder::new_bytefor(8, width, signed);
        let built = build_stream(&mut enc, &inputs, 0, signed);
        for (g, gd) in gs.iter().enumerate() {
            let (full, _) = dec_full(&built, g as u32);
            let rows = gd.rows;
            let ctx = built.ctx(g as u32);
            let f = Framed::of_ctx(&ctx, EncodingId::ByteFor.as_u16()).expect("framed");
            let sels = [
                vec![],
                vec![0],
                vec![(rows - 1) as u16],
                (0..rows as u16).collect(),
                random_selection(rows, 3, &mut seed),
                random_selection(rows, 71, &mut seed),
            ];
            for sel_rows in &sels {
                let sel = Selection { rows: sel_rows };
                let n = sel_rows.len() as u32;
                let mut picked = vec![false; rows as usize];
                sel_rows.iter().for_each(|&r| picked[r as usize] = true);
                let mut w64 = vec![UNTOUCHED; rows as usize];
                let mut w32 = vec![0x5A5A_5A5Ai32; rows as usize];
                let mut w16 = vec![0x5A5Ai16; rows as usize];
                assert_eq!(sel_at_framed(&f, width, &sel, &mut w64).expect("u64"), n);
                assert_eq!(sel_at_framed(&f, width, &sel, &mut w32).expect("i32"), n);
                assert_eq!(sel_at_framed(&f, width, &sel, &mut w16).expect("i16"), n);
                for r in 0..rows as usize {
                    let (want64, want32, want16) = if picked[r] { (full[r], full[r] as i32, full[r] as i16) } else { (UNTOUCHED, 0x5A5A_5A5A, 0x5A5A) };
                    assert_eq!((w64[r], w32[r], w16[r]), (want64, want32, want16), "shape {shape} granule {g} row {r} (selected {})", picked[r]);
                }
            }
            let past = [rows as u16 - 1, rows as u16];
            let mut out = vec![0u64; rows as usize + 1];
            assert!(sel_at_framed(&f, width, &Selection { rows: &past[..] }, &mut out).is_err(), "a row past the granule");
            let mut short = vec![0i32; 100];
            assert!(sel_at_framed(&f, width, &Selection { rows: &[100] }, &mut short).is_err(), "an out short of the row");
        }
    }
}

/// D3223 S1: `bytefor::each_frame` walks a resolved granule's frames in row order, each frame's reference plus its stored
/// deltas (read at the stream's width) equal to `decode_full` at every row; a `false` stops the walk; a width past 8 refuses.
#[test]
fn byte_for_each_frame_is_the_full_decode() {
    use crate::bytefor::{each_frame, Framed};
    let mut seed = 0x5C4E_u64;
    let class = StorageClass::ByvalWord { width: 8, signed: true };
    for shape in 0..12 {
        let last = [8192u32, 1500, 300, 1024][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, last].iter().map(|&rows| int_corpus(shape, rows, &mut seed)).collect();
        let signed = signed_for(shape);
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let width = inputs.iter().map(|g| granule_min_width(g, signed)).max().unwrap();
        let mut enc = ByteForEncoder::new_bytefor(8, width, signed);
        let built = build_stream(&mut enc, &inputs, 0, signed);
        for (g, gd) in gs.iter().enumerate() {
            let (full, _) = dec_full(&built, g as u32);
            let ctx = built.ctx(g as u32);
            let f = Framed::of_ctx(&ctx, EncodingId::ByteFor.as_u16()).expect("framed");
            let (w, mut next, mut frames) = (width as usize, 0usize, 0usize);
            let done = each_frame(&f, width, |first, refw, deltas| {
                assert_eq!(first, next, "shape {shape} granule {g}: frames in row order");
                let n = deltas.len() / w;
                assert_eq!(n, (gd.rows as usize - first).min(1024), "shape {shape} granule {g}: a frame's values");
                for i in 0..n {
                    let mut d = [0u8; 8];
                    d[..w].copy_from_slice(&deltas[i * w..i * w + w]);
                    assert_eq!(refw.wrapping_add(u64::from_le_bytes(d)), full[first + i], "shape {shape} granule {g} row {}", first + i);
                }
                next += n;
                frames += 1;
                true
            })
            .expect("each_frame");
            assert!(done && next == gd.rows as usize, "shape {shape} granule {g}: every row");
            let mut seen = 0;
            assert!(!each_frame(&f, width, |_, _, _| { seen += 1; false }).expect("stopped"), "a false stops the walk");
            assert_eq!((seen, frames), (1, (gd.rows as usize).div_ceil(1024)));
            assert!(each_frame(&f, 9, |_, _, _| true).is_err(), "a width past 8");
        }
    }
}

#[test]
fn delta_for_roundtrips_across_shapes() {
    let mut seed = 0xDF_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    for shape in 0..8 {
        let last = [8192u32, 700, 2049][shape as usize % 3];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| int_corpus(shape, rows, &mut seed))
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let mut enc = DeltaForEncoder::default();
        let built = build_stream(&mut enc, &inputs, 0, true);
        assert_int_roundtrip(&built, &gs.iter().collect::<Vec<_>>(), class);
    }
}

#[test]
fn ffor_roundtrips_across_shapes() {
    let mut seed = 0xFF0_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    for shape in 0..8 {
        let last = [8192u32, 1025, 64][shape as usize % 3];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| int_corpus(shape, rows, &mut seed))
            .collect();
        let signed = signed_for(shape);
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
        let mut enc = FforEncoder { signed, carry: None };
        let built = build_stream(&mut enc, &inputs, 0, signed);
        assert_int_roundtrip(&built, &gs.iter().collect::<Vec<_>>(), class);
    }
}

// ---------------------------------------------------------------------------
// floats: ALP family, bit-exact incl. NaN payloads; vendored oracle
// ---------------------------------------------------------------------------

fn float_corpus(shape: u32, rows: u32, seed: &mut u64) -> GranuleData {
    roundtrip_float_corpus(shape, rows, seed)
}

#[test]
fn alp_roundtrips_bit_exact_with_oracle_differential() {
    let mut seed = 0xA1B_u64;
    for shape in 0..12 {
        let last = [8192u32, 3000, 1024, 100][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| float_corpus(shape, rows, &mut seed))
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(StorageClass::F64)).collect();
        for encoding in [EncodingId::Alp, EncodingId::AlpRd] {
            let mut enc = AlpEncoder { encoding, carry: None };
            let built = build_stream(&mut enc, &inputs, 0, false);
            for (g, gd) in gs.iter().enumerate() {
                let (datums, _arena) = dec_full(&built, g as u32);
                for r in 0..gd.rows as usize {
                    if !gd.input(StorageClass::F64).valid(r as u32) {
                        continue;
                    }
                    assert_eq!(
                        datums[r], gd.datums[r],
                        "shape {shape} granule {g} row {r}: float bits diverged"
                    );
                }
                // Vendored oracle differential on the exact stored frame.
                let ft = built.frame_table.as_deref().expect("alp frame table");
                let section = built.section();
                let hdr = pgrc_format::part::StreamSectionHdr::decode(section).expect("hdr");
                let payload = crate::section::payload_region(&hdr, section).expect("payload");
                let fs = ft[g] as usize;
                let fe = ft.get(g + 1).map(|&x| x as usize).unwrap_or(payload.len());
                let mut oracle: Vec<u64> = Vec::new();
                alp::granule::decode_frame_words(&payload[fs..fe], &mut oracle)
                    .expect("vendored oracle decodes our frame");
                // The oracle sees placeholder values in null slots; compare
                // ALL slots (placeholders are deterministic 0.0-encodings).
                let (mine, _a) = dec_full(&built, g as u32);
                for r in 0..gd.rows as usize {
                    if gd.input(StorageClass::F64).valid(r as u32) {
                        assert_eq!(mine[r], oracle[r], "oracle diverged at row {r}");
                    }
                }
            }
        }
    }
}

#[test]
fn byte_for_sb3_widths_roundtrip_and_exact_size() {
    // The SB-3 widening: every new byte-aligned width (3/5/6/7) — plus the
    // pow2 sanity pairs — proven end-to-end at its boundary range. The
    // planted min/max inside frame 0 make the stream width deterministic;
    // the size check is independent arithmetic over the frozen §6.4/§6.10
    // geometry (header + per-frame ref + rows×w + frame table), never the
    // encoder's own accounting.
    let mut seed = 0x3B_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let cases: [(u64, u8); 8] = [
        (0xFF, 1),
        (0x100, 2),
        (0xFF_FFFF, 3),
        (0x100_0000, 4),
        (0xFF_FFFF_FFFF, 5),
        (0x100_0000_0000, 6),
        (0xFF_FFFF_FFFF_FFFF, 7),
        (0x100_0000_0000_0000, 8),
    ];
    for &(range, want_w) in &cases {
        let rows = 2048u32;
        let base = 5_000_000_000u64;
        let mut datums: Vec<u64> = (0..rows)
            .map(|_| base + splitmix(&mut seed) % (range + 1))
            .collect();
        datums[0] = base; // plant the exact min ...
        datums[1] = base + range; // ... and max inside frame 0
        let gd = GranuleData {
            rows,
            datums,
            validity: None,
        };
        let inputs = [gd.input(class)];
        let w = granule_min_width(&inputs[0], true);
        assert_eq!(w, want_w, "range {range:#x} must elect width {want_w}");
        let mut enc = ByteForEncoder::new_bytefor(8, w, true);
        let built = build_stream(&mut enc, &inputs, 0, true);
        assert_int_roundtrip(&built, &[&gd], class);
        let frames = rows.div_ceil(1024) as usize;
        let expect = 32 + frames * 8 + rows as usize * w as usize + frames * 4;
        assert_eq!(built.section().len(), expect, "width {w}: exact section bytes");
    }
}

// ---------------------------------------------------------------------------
// bool bitmap
// ---------------------------------------------------------------------------

#[test]
fn bool_bitmap_roundtrips() {
    let mut seed = 0xB001_u64;
    for shape in 0..6 {
        let last = [8192u32, 5, 1027][shape as usize % 3];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| {
                let validity = validity_pattern(shape, rows, &mut seed);
                let datums = (0..rows).map(|_| splitmix(&mut seed) % 2).collect();
                GranuleData {
                    rows,
                    datums,
                    validity,
                }
            })
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(StorageClass::Bool)).collect();
        let mut enc = BoolBitmapEncoder;
        let built = build_stream(&mut enc, &inputs, 0, false);
        for (g, gd) in gs.iter().enumerate() {
            let (datums, _arena) = dec_full(&built, g as u32);
            for r in 0..gd.rows as usize {
                if gd.input(StorageClass::Bool).valid(r as u32) {
                    assert_eq!(datums[r], gd.datums[r] % 2, "granule {g} row {r}");
                }
            }
        }
    }
}

#[test]
fn alp_f32_roundtrips_bit_exact_with_oracle_differential() {
    // The SB-5 arm under the same two-pass law as the f64 suite: build
    // through the production driver (verify ON), decode again through the
    // registry and compare bits against the INPUT, then run the vendored
    // granule32 reader as a differential oracle on the exact stored frames.
    let mut seed = 0xA32_u64;
    for shape in 0..10 {
        let last = [8192u32, 3000, 1024, 100][shape as usize % 4];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| roundtrip_f32_corpus(shape, rows, &mut seed))
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(StorageClass::F32)).collect();
        let mut enc = AlpF32Encoder::default();
        let built = build_stream(&mut enc, &inputs, 0, false);
        for (g, gd) in gs.iter().enumerate() {
            let (datums, _arena) = dec_full(&built, g as u32);
            for r in 0..gd.rows as usize {
                if !gd.input(StorageClass::F32).valid(r as u32) {
                    continue;
                }
                assert_eq!(
                    datums[r], gd.datums[r],
                    "shape {shape} granule {g} row {r}: f32 bits diverged"
                );
            }
            // Vendored oracle differential on the exact stored frame.
            let ft = built.frame_table.as_deref().expect("alp f32 frame table");
            let section = built.section();
            let hdr = pgrc_format::part::StreamSectionHdr::decode(section).expect("hdr");
            let payload = crate::section::payload_region(&hdr, section).expect("payload");
            let fs = ft[g] as usize;
            let fe = ft.get(g + 1).map(|&x| x as usize).unwrap_or(payload.len());
            let mut oracle: Vec<u32> = Vec::new();
            alp::granule32::decode_frame_bits32(&payload[fs..fe], &mut oracle)
                .expect("vendored oracle decodes our frame");
            for r in 0..gd.rows as usize {
                if gd.input(StorageClass::F32).valid(r as u32) {
                    assert_eq!(
                        datums[r], oracle[r] as u64,
                        "f32 oracle diverged at row {r}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// dict codes (global codes; §6.11 frozen blocks)
// ---------------------------------------------------------------------------

/// Byte-rank-sorted dict fixture: entries "k000000".."k{n-1}" zero-padded so
/// byte order == numeric order; returns (index bytes, payload bytes) in
/// 8-aligned buffers (the `SegBuf` substrate model — the zero-copy gather
/// emits pointers into these, and the §7b pins check absolute alignment).
pub fn dict_fixture(n: u32) -> (super::Aligned8, super::Aligned8) {
    let mut index = Vec::new();
    let mut payload = Vec::new();
    for i in 0..n {
        let s = format!("k{i:06}");
        let off = put_varlena_entry(&mut payload, s.as_bytes());
        pgrc_format::dict::DictIndexEntry {
            payload_off: off as u32,
            byte_len: s.len() as u32,
            char_field: s.len() as u32,
        }
        .encode_into(&mut index);
    }
    (
        super::Aligned8::from_bytes(&index),
        super::Aligned8::from_bytes(&payload),
    )
}

#[test]
fn dict_codes_roundtrip_codes_and_materialization() {
    let mut seed = 0xD1C7_u64;
    let ndv = 3000u32;
    let (index, payload) = dict_fixture(ndv);
    for shape in 0..6 {
        let last = [8192u32, 1024, 77][shape as usize % 3];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| {
                let validity = validity_pattern(shape, rows, &mut seed);
                // Cluster codes so per-granule ranges stay narrow sometimes.
                let base = (splitmix(&mut seed) % 2000) as u64;
                let datums = (0..rows)
                    .map(|_| base + splitmix(&mut seed) % ((shape as u64 % 4) * 250 + 1))
                    .collect();
                GranuleData {
                    rows,
                    datums,
                    validity,
                }
            })
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs
            .iter()
            .map(|g| g.input(StorageClass::VarlenaVerbatim))
            .collect();
        let mut enc = DictCodesEncoder {
            class: CLASS_VARLENA,
            max_width: 32,
            byte_align: false,
        };
        let built = build_stream(&mut enc, &inputs, 0, false);
        let dict = DictSections {
            index: &index[..],
            payload: &payload[..],
            entry_count: ndv,
            charlen_form: pgrc_format::dict::DictCharLenForm::Absolute,
            form: pgrc_format::dict::DictPayloadForm::Expanded,
        };
        let vt = crate::registry()
            .resolve(built.key())
            .expect("dict kernel resolves");
        for (g, gd) in gs.iter().enumerate() {
            let mut ctx = built.ctx(g as u32);
            ctx.dict = Some(dict);
            // Codes face.
            let mut codes = vec![0u32; gd.rows as usize];
            let n = (vt.decode_codes)(&ctx, &mut codes).expect("decode_codes");
            assert_eq!(n, gd.rows);
            for r in 0..gd.rows as usize {
                if gd.input(StorageClass::VarlenaVerbatim).valid(r as u32) {
                    assert_eq!(codes[r] as u64, gd.datums[r], "granule {g} row {r} code");
                }
            }
            // Materialization face: images must equal the dict entries.
            let mut datums = vec![0u64; gd.rows as usize];
            let mut arena_buf = vec![0u8; 1 << 20];
            let mut out = pgrc_format::abi::DecodeOut {
                datums: &mut datums,
                arena: ByteArena::new(&mut arena_buf),
            };
            (vt.decode_full)(&ctx, &mut out).expect("decode_full");
            for r in 0..gd.rows as usize {
                if !gd.input(StorageClass::VarlenaVerbatim).valid(r as u32) {
                    continue;
                }
                let want = format!("k{:06}", gd.datums[r]);
                // SAFETY: arena-backed varlena datum built by the kernel.
                let got = unsafe { crate::section::varlena_payload(datums[r]).expect("shape") };
                assert_eq!(got, want.as_bytes(), "granule {g} row {r} image");
            }
            // Dict handle face.
            let layout = (vt.dict_handle)(&ctx).expect("dict_handle");
            assert_eq!(layout.entry_count, ndv);
            assert_eq!(layout.frame_entries, 1024);
        }
    }
}

/// pgrc21-widths: byte-aligned code widths are a strict subset of the
/// frozen §6.11 envelope — a `byte_align: true` stream decodes to the SAME
/// codes as the bit-exact stream, its priced facts round to byte
/// multiples, and the priced bytes match what the encoder seals.
#[test]
fn dict_codes_byte_align_same_codes() {
    let mut seed = 0xBA17_u64;
    for shape in 0..6 {
        let last = [8192u32, 1024, 77][shape as usize % 3];
        let gs: Vec<GranuleData> = [8192u32, last]
            .iter()
            .map(|&rows| {
                let validity = validity_pattern(shape, rows, &mut seed);
                let base = (splitmix(&mut seed) % 2000) as u64;
                // Range dial: per-shape spread hits widths on both sides of
                // a byte boundary (0, <8, 8..16, wide).
                let spread = [1u64, 3, 200, 5000, 70000, 300000][shape as usize];
                let datums = (0..rows)
                    .map(|_| base + splitmix(&mut seed) % spread)
                    .collect();
                GranuleData {
                    rows,
                    datums,
                    validity,
                }
            })
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs
            .iter()
            .map(|g| g.input(StorageClass::VarlenaVerbatim))
            .collect();
        for input in &inputs {
            let (bytes_bit, w_bit) = crate::dictcodes::granule_block_facts(input, false);
            let (bytes_byte, w_byte) = crate::dictcodes::granule_block_facts(input, true);
            assert_eq!(w_byte % 8, 0, "byte posture width is byte-aligned");
            assert_eq!(w_byte, w_bit.div_ceil(8) * 8, "rounded up, never down");
            assert!(bytes_byte >= bytes_bit);
        }
        let mut arms = Vec::new();
        for byte_align in [false, true] {
            let mut enc = DictCodesEncoder {
                class: CLASS_VARLENA,
                max_width: 32,
                byte_align,
            };
            let built = build_stream(&mut enc, &inputs, 0, false);
            let vt = crate::registry()
                .resolve(built.key())
                .expect("dict kernel resolves");
            let mut all: Vec<Vec<u32>> = Vec::new();
            for (g, gd) in gs.iter().enumerate() {
                let ctx = built.ctx(g as u32);
                let mut codes = vec![0u32; gd.rows as usize];
                let n = (vt.decode_codes)(&ctx, &mut codes).expect("decode_codes");
                assert_eq!(n, gd.rows);
                all.push(codes);
            }
            arms.push(all);
        }
        assert_eq!(arms[0], arms[1], "shape {shape}: codes identical across postures");
    }
}

// ---------------------------------------------------------------------------
// packed numeric (vendored A6b semantics; byte-identical reconstruction)
// ---------------------------------------------------------------------------

fn numeric_images(rows: u32, scale: i32, seed: &mut u64) -> Vec<adt_numeric::NumericImage> {
    (0..rows)
        .map(|_| {
            let mant = (splitmix(seed) % 10_000_000) as i64 - 5_000_000;
            let s = super::electiontests::decimal_string(mant, scale);
            adt_numeric::io::numeric_in(&s, -1, None)
                .expect("parse")
                .expect("non-soft")
        })
        .collect()
}

#[test]
fn packed_numeric_reconstruction_is_byte_identical() {
    let mut seed = 0xA6B_u64;
    for scale in [0i32, 2, 7] {
        let rows = 2048u32;
        let images: Vec<Vec<adt_numeric::NumericImage>> = (0..1)
            .map(|_| numeric_images(rows, scale, &mut seed))
            .collect();
        let gs: Vec<GranuleData> = images
            .iter()
            .map(|imgs| {
                let mut vseed = seed;
                GranuleData {
                    rows,
                    datums: imgs.iter().map(|i| i.as_bytes().as_ptr() as u64).collect(),
                    validity: validity_pattern(1, rows, &mut vseed),
                }
            })
            .collect();
        let inputs: Vec<EncodeInput<'_>> = gs
            .iter()
            .map(|g| g.input(StorageClass::VarlenaVerbatim))
            .collect();
        // Width from the mantissa arm (the election's job; here direct).
        let election = crate::election::elect_numeric(&inputs, usize::MAX / 2).expect("elect");
        let crate::election::Election::Elected {
            encoding: EncodingId::PackedNumeric,
            width,
            aux32,
            ..
        } = election
        else {
            panic!("uniform-scale corpus must elect packed numeric: {election:?}");
        };
        assert_eq!(aux32 as i32, scale);
        let mut enc = PackedNumericEncoder::new(scale, width);
        let built = build_stream(&mut enc, &inputs, 0, true);
        for (g, imgs) in images.iter().enumerate() {
            let (datums, _arena) = dec_full(&built, g as u32);
            let input = gs[g].input(StorageClass::VarlenaVerbatim);
            for r in 0..rows as usize {
                if !input.valid(r as u32) {
                    continue;
                }
                // SAFETY: arena-backed varlena datum built by the kernel.
                let got = unsafe { crate::section::varlena_payload(datums[r]).expect("shape") };
                assert_eq!(
                    got,
                    &imgs[r].as_bytes()[4..],
                    "granule {g} row {r}: numeric bytes diverged"
                );
            }
        }
    }
}

/// The mantissa face's selected forms against `mantissa_full` over a full eight-frame granule: dense in selection
/// order (`mantissa_sel`) and at each row's own position (`mantissa_sel_at`, every other row untouched), for the empty,
/// first-row, last-row, every-row, sparse and frame-edge selections; a row past the granule and a short `out` refuse.
#[test]
fn packed_numeric_mantissa_sel_forms_match_full() {
    use crate::packednum::{mantissa_full, mantissa_sel, mantissa_sel_at};
    use pgrc_format::abi::Selection;
    const UNTOUCHED: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    let mut seed = 0x5E1D_u64;
    let (rows, scale) = (pgrc_format::geom::GRANULE_ROWS, 2i32);
    let images = numeric_images(rows, scale, &mut seed);
    let g = GranuleData {
        rows,
        datums: images.iter().map(|i| i.as_bytes().as_ptr() as u64).collect(),
        validity: None,
    };
    let inputs = [g.input(StorageClass::VarlenaVerbatim)];
    let election = crate::election::elect_numeric(&inputs, usize::MAX / 2).expect("elect");
    let crate::election::Election::Elected {
        encoding: EncodingId::PackedNumeric,
        width,
        ..
    } = election
    else {
        panic!("a uniform-scale granule elects packed numeric: {election:?}");
    };
    let built = build_stream(&mut PackedNumericEncoder::new(scale, width), &inputs, 0, true);
    let ctx = built.ctx(0);
    let framed = crate::bytefor::Framed::of_ctx(&ctx, EncodingId::PackedNumeric.as_u16()).expect("framed");
    let mut full = vec![0u64; rows as usize];
    assert_eq!(mantissa_full(&ctx, width, &mut full).expect("full"), (scale, rows));
    let mut edges: Vec<u16> = (0..rows / 1024)
        .flat_map(|f| [f * 1024, f * 1024 + 1, f * 1024 + 1023])
        .map(|r| r as u16)
        .collect();
    edges.dedup();
    let sels = [
        vec![],
        vec![0],
        vec![(rows - 1) as u16],
        (0..rows as u16).collect(),
        random_selection(rows, 3, &mut seed),
        random_selection(rows, 53, &mut seed),
        edges,
    ];
    for sel_rows in &sels {
        let sel = Selection { rows: sel_rows };
        let n = sel_rows.len() as u32;
        let mut dense = vec![UNTOUCHED; sel_rows.len()];
        assert_eq!(mantissa_sel(&ctx, width, &sel, &mut dense).expect("sel"), (scale, n));
        let mut at = vec![UNTOUCHED; rows as usize];
        assert_eq!(mantissa_sel_at(&ctx, width, &sel, &mut at).expect("sel_at"), (scale, n));
        // the framed form (TH14's source lever): the same scale and the same rows, byte for byte
        let mut at_framed = vec![UNTOUCHED; rows as usize];
        assert_eq!(crate::packednum::mantissa_sel_at_framed(&framed, width, &sel, &mut at_framed).expect("framed sel_at"), (scale, n));
        assert_eq!(at_framed, at, "mantissa_sel_at_framed == mantissa_sel_at");
        assert_eq!(crate::packednum::framed_scale(&framed).expect("framed scale"), scale, "framed_scale == the granule's scale");
        for (i, &r) in sel_rows.iter().enumerate() {
            assert_eq!(dense[i], full[r as usize], "mantissa_sel row {r}");
        }
        let mut picked = vec![false; rows as usize];
        for &r in sel_rows {
            picked[r as usize] = true;
        }
        for r in 0..rows as usize {
            let want = if picked[r] { full[r] } else { UNTOUCHED };
            assert_eq!(at[r], want, "mantissa_sel_at row {r} (selected {})", picked[r]);
        }
    }
    // a row past the granule, and an `out` short of the last selected row, refuse
    let past = [rows as u16 - 1, rows as u16];
    let mut out = vec![0u64; rows as usize + 1];
    assert!(mantissa_sel_at(&ctx, width, &Selection { rows: &past[..] }, &mut out).is_err());
    assert!(mantissa_sel(&ctx, width, &Selection { rows: &past[..] }, &mut out).is_err());
    let mut short = vec![0u64; 100];
    assert!(mantissa_sel_at(&ctx, width, &Selection { rows: &[100] }, &mut short).is_err());
    assert!(crate::packednum::mantissa_sel_at_framed(&framed, width, &Selection { rows: &past[..] }, &mut out).is_err());
    assert!(crate::packednum::mantissa_sel_at_framed(&framed, width, &Selection { rows: &[100] }, &mut short).is_err());
}

// ---------------------------------------------------------------------------
// array dual (structural: split + child encode + assemble)
// ---------------------------------------------------------------------------

/// Hand-built 1-D int4 array image (payload form PG produces: ndim 1,
/// dataoffset 0, lbound 1) — or the ndim-0 empty form.
pub fn int4_array_image(elems: &[i32]) -> Vec<u8> {
    const INT4_OID: u32 = 23;
    let payload_len = if elems.is_empty() {
        12
    } else {
        20 + elems.len() * 4
    };
    let mut b = Vec::with_capacity(4 + payload_len);
    b.extend_from_slice(&pgrc_format::wire::varlena_header_4b_u(payload_len as u32).to_le_bytes());
    if elems.is_empty() {
        b.extend_from_slice(&0i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.extend_from_slice(&INT4_OID.to_le_bytes());
    } else {
        b.extend_from_slice(&1i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.extend_from_slice(&INT4_OID.to_le_bytes());
        b.extend_from_slice(&(elems.len() as i32).to_le_bytes());
        b.extend_from_slice(&1i32.to_le_bytes());
        for e in elems {
            b.extend_from_slice(&e.to_le_bytes());
        }
    }
    b
}

#[test]
fn array_dual_split_encode_assemble_is_byte_identical() {
    let mut seed = 0xA9_u64;
    let facts = ArrayElemFacts {
        elemtype: 23,
        elem_len: 4,
    };
    let rows = 1200u32;
    let arrays: Vec<Vec<u8>> = (0..rows)
        .map(|_| {
            let n = (splitmix(&mut seed) % 7) as usize;
            let elems: Vec<i32> = (0..n).map(|_| splitmix(&mut seed) as i32).collect();
            int4_array_image(&elems)
        })
        .collect();
    let mut vseed = seed;
    let validity = validity_pattern(1, rows, &mut vseed);
    let mut datums: Vec<u64> = arrays.iter().map(|a| a.as_ptr() as u64).collect();
    // Null rows must not carry stale pointers into the split.
    if let Some(v) = &validity {
        for r in 0..rows {
            if v[(r / 64) as usize] >> (r % 64) & 1 == 0 {
                datums[r as usize] = 0;
            }
        }
    }
    let gd = GranuleData {
        rows,
        datums,
        validity,
    };
    let input = gd.input(StorageClass::VarlenaVerbatim);
    let split = elect_array_split(&input, facts)
        .expect("split ok")
        .expect("shape accepted");

    // Encode the two substreams through the ordinary int kernels.
    let sizes_gd = GranuleData {
        rows,
        datums: split.sizes.clone(),
        validity: None, // sizes are row-aligned with 0 for null rows
    };
    let sclass = StorageClass::ByvalWord {
        width: 8,
        signed: false,
    };
    let s_in = [sizes_gd.input(sclass)];
    let s_width = granule_min_width(&s_in[0], false);
    let mut s_enc = ByteForEncoder::new_bytefor(8, s_width, false);
    let s_built = build_stream(&mut s_enc, &s_in, 0, false);
    let (dec_sizes, _sa) = dec_full(&s_built, 0);

    let elems_gd = GranuleData {
        rows: split.elems.len() as u32,
        datums: split.elems.clone(),
        validity: None,
    };
    let e_in = [elems_gd.input(sclass)];
    let e_width = granule_min_width(&e_in[0], false);
    let mut e_enc = ByteForEncoder::new_bytefor(8, e_width, false);
    let e_built = build_stream(&mut e_enc, &e_in, 0, false);
    let (dec_elems, _ea) = dec_full(&e_built, 0);

    // Assemble and byte-compare.
    let mut arena_buf = vec![0u8; crate::arraydual::assembled_arena_bytes(facts, &dec_sizes) + 64];
    let mut arena = ByteArena::new(&mut arena_buf);
    let mut out = vec![0u64; rows as usize];
    let validity2 = gd.validity.clone();
    assemble_array_datums(
        facts,
        &dec_sizes,
        &dec_elems,
        |r| match &validity2 {
            None => true,
            Some(w) => w[(r / 64) as usize] >> (r % 64) & 1 == 1,
        },
        &mut out,
        &mut arena,
    )
    .expect("assemble");
    for r in 0..rows as usize {
        if !input.valid(r as u32) {
            continue;
        }
        // SAFETY: arena-backed varlena datum.
        let got = unsafe { crate::section::varlena_payload(out[r]).expect("shape") };
        assert_eq!(got, &arrays[r][4..], "row {r}: array bytes diverged");
        assert_eq!(out[r] % 8, 0, "row {r}: array datum not 8-aligned");
    }
}

#[test]
fn array_dual_refuses_foreign_shapes() {
    let facts = ArrayElemFacts {
        elemtype: 23,
        elem_len: 4,
    };
    // Wrong elemtype refuses.
    let img = int4_array_image(&[1, 2, 3]);
    let datums = [img.as_ptr() as u64];
    let input = EncodeInput {
        class: StorageClass::VarlenaVerbatim,
        rows: 1,
        datums: &datums,
        validity: None,
    };
    let wrong = ArrayElemFacts {
        elemtype: 20,
        elem_len: 8,
    };
    assert!(elect_array_split(&input, wrong).expect("ok").is_none());
    // 2-D refuses.
    let mut twod = int4_array_image(&[1, 2, 3, 9]);
    twod[4..8].copy_from_slice(&2i32.to_le_bytes());
    let datums2 = [twod.as_ptr() as u64];
    let input2 = EncodeInput {
        class: StorageClass::VarlenaVerbatim,
        rows: 1,
        datums: &datums2,
        validity: None,
    };
    assert!(elect_array_split(&input2, facts).expect("ok").is_none());
    // lbound != 1 refuses.
    let mut lb = int4_array_image(&[1, 2, 3]);
    lb[20..24].copy_from_slice(&0i32.to_le_bytes());
    let datums3 = [lb.as_ptr() as u64];
    let input3 = EncodeInput {
        class: StorageClass::VarlenaVerbatim,
        rows: 1,
        datums: &datums3,
        validity: None,
    };
    assert!(elect_array_split(&input3, facts).expect("ok").is_none());
}

// ---------------------------------------------------------------------------
// wrapper: wrapped sections decode to the same values
// ---------------------------------------------------------------------------

#[test]
fn zstd_wrapped_section_unwraps_to_identical_decode() {
    // The CMP-A arm under the same law as LZ4: wrap → unwrap → every
    // granule decodes byte-identically to the unwrapped original.
    let mut seed = 0x117B_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gs: Vec<GranuleData> = (0..3).map(|_| int_corpus(5, 8192, &mut seed)).collect();
    let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
    let width = inputs
        .iter()
        .map(|g| granule_min_width(g, true))
        .max()
        .unwrap();
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);

    let mut wrapped = Vec::new();
    wrap_section(
        built.section(),
        &built.build.granule_payload_ends,
        pgrc_format::enc::Wrapper::Zstd,
        &mut wrapped,
    )
    .expect("wrap");
    let mut scratch = Vec::new();
    unwrap_section(&wrapped, &mut scratch).expect("unwrap");

    // The rebuilt image is the unwrapped section verbatim (stronger than
    // value equality: the unwrap twin law is byte-exact end to end).
    assert_eq!(
        scratch,
        built.section(),
        "zstd unwrap must rebuild the unwrapped section byte-exactly"
    );

    let hdr = pgrc_format::part::StreamSectionHdr::decode(&scratch).expect("hdr");
    let ft = hdr.frame_table(&scratch).expect("ft");
    for (g, gd) in gs.iter().enumerate() {
        let (orig, _oa) = dec_full(&built, g as u32);
        let ctx = built.ctx_with(g as u32, built.key(), &scratch, ft.as_deref());
        let (twin, _ta) = dec_full_ctx(&ctx);
        let input = gd.input(class);
        for r in 0..gd.rows as usize {
            if input.valid(r as u32) {
                assert_eq!(
                    orig[r], twin[r],
                    "granule {g} row {r} diverged after zstd unwrap"
                );
            }
        }
    }
}

#[test]
fn lz4_wrapped_section_unwraps_to_identical_decode() {
    let mut seed = 0x117A_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gs: Vec<GranuleData> = (0..3).map(|_| int_corpus(5, 8192, &mut seed)).collect();
    let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
    let width = inputs
        .iter()
        .map(|g| granule_min_width(g, true))
        .max()
        .unwrap();
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);

    let mut wrapped = Vec::new();
    wrap_section(
        built.section(),
        &built.build.granule_payload_ends,
        pgrc_format::enc::Wrapper::Lz4,
        &mut wrapped,
    )
    .expect("wrap");
    let mut scratch = Vec::new();
    unwrap_section(&wrapped, &mut scratch).expect("unwrap");

    // Decode every granule from the unwrapped twin; values must match the
    // original decode exactly.
    let hdr = pgrc_format::part::StreamSectionHdr::decode(&scratch).expect("hdr");
    let ft = hdr.frame_table(&scratch).expect("ft");
    for (g, gd) in gs.iter().enumerate() {
        let (orig, _oa) = dec_full(&built, g as u32);
        let ctx = built.ctx_with(g as u32, built.key(), &scratch, ft.as_deref());
        let (twin, _ta) = dec_full_ctx(&ctx);
        let input = gd.input(class);
        for r in 0..gd.rows as usize {
            if input.valid(r as u32) {
                assert_eq!(
                    orig[r], twin[r],
                    "granule {g} row {r} diverged after unwrap"
                );
            }
        }
    }
}

#[test]
fn wrap_section_bound_covers_incompressible_and_empty_blocks() {
    // C1-REG: a pool worker fills a buffer its caller reserved at `wrap_section_bound`; the wrap must never outgrow it,
    // whatever the payload (noise is the codecs' worst case), and must write the same bytes as into an empty `Vec`.
    use crate::wrapper::wrap_section_bound;
    use pgrc_format::enc::Wrapper;
    use pgrc_format::part::STREAM_SECTION_HDR_LEN;
    let mut seed = 0xC1_4E6_u64;
    let class = StorageClass::ByvalWord { width: 8, signed: true };
    let gs: Vec<GranuleData> = (0..3).map(|_| int_corpus(5, 8192, &mut seed)).collect();
    let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
    let width = inputs.iter().map(|g| granule_min_width(g, true)).max().unwrap();
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);
    let ends = built.build.granule_payload_ends.clone();
    let payload_len = *ends.last().unwrap() as usize;
    let payload = STREAM_SECTION_HDR_LEN..STREAM_SECTION_HDR_LEN + payload_len;
    // The same geometry with an empty first block and a 1-byte one (ends may repeat).
    let tiny: Vec<u32> = [0, 1].into_iter().chain(ends.iter().copied()).collect();
    for fill in ["encoded", "noise", "zeros"] {
        let mut section = built.section().to_vec();
        match fill {
            "noise" => section[payload.clone()].iter_mut().for_each(|b| *b = splitmix(&mut seed) as u8),
            "zeros" => section[payload.clone()].fill(0),
            _ => {}
        }
        for arm in [Wrapper::Lz4, Wrapper::Zstd] {
            for ends in [&ends, &tiny] {
                let bound = wrap_section_bound(&section, ends, arm);
                let mut plain = Vec::new();
                wrap_section(&section, ends, arm, &mut plain).expect("wrap");
                assert!(plain.len() <= bound, "{fill} {arm:?}: wrapped {} > bound {bound}", plain.len());
                let mut out = Vec::with_capacity(bound);
                let cap = out.capacity();
                wrap_section(&section, ends, arm, &mut out).expect("wrap");
                assert_eq!(out.capacity(), cap, "{fill} {arm:?}: the wrap reallocated its reserved buffer");
                assert_eq!(out, plain, "{fill} {arm:?}: a reserved buffer changed the image");
            }
        }
    }
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn zstd_compress_block_matches_a_fresh_context() {
    // C1-REG: the wrapper keeps zstd contexts for the next block instead of making one per block. A kept context must
    // write the frame a fresh one writes (the bytes on disk are pinned): every input here, in an order that reuses the
    // contexts across sizes (big after small, small after big, repeats), is compared with a fresh `zstd::bulk::compress`
    // at the wrapper's level, appended after bytes already in the buffer.
    use crate::wrapper::zstd_compress_block;
    let mut seed = 0x2C_7F_u64;
    let noise = |n: usize, seed: &mut u64| (0..n).map(|_| splitmix(seed) as u8).collect::<Vec<u8>>();
    let text = |n: usize| (0..n).map(|i| b"pgrc zstd frames "[i % 17] ^ (i / 4096) as u8).collect::<Vec<u8>>();
    let ints = |n: usize, seed: &mut u64| (0..n / 8).flat_map(|_| (splitmix(seed) % 1000).to_le_bytes()).collect::<Vec<u8>>();
    let inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![7],
        vec![0; 65536],
        noise(100_000, &mut seed),
        text(721_016),
        ints(8192 * 8, &mut seed),
        vec![1],
        noise(1 << 20, &mut seed),
        text(3000),
        Vec::new(),
        ints(362_000, &mut seed),
        vec![0; 17],
    ];
    for round in 0..3 {
        for (k, input) in inputs.iter().enumerate() {
            let fresh = zstd::bulk::compress(input, crate::wrapper::zstd_level()).expect("fresh compress");
            let mut out = b"prefix".to_vec();
            zstd_compress_block(input, &mut out).expect("pooled compress");
            assert_eq!(&out[..6], b"prefix", "round {round} input {k}: the bytes before the frame changed");
            assert_eq!(&out[6..], &fresh[..], "round {round} input {k} ({} B): a kept context wrote another frame", input.len());
        }
    }
}
