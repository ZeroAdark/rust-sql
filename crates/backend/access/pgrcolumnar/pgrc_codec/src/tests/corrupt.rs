//! Bounds-validated decode on corrupt inputs (§5 M3-C: "typed error, never
//! UB") — the seeded-corruption teeth. Two teeth per §10 law:
//!
//! 1. **fires on a seeded defect**: specific corruptions that MUST refuse
//!    (truncations, bad widths, out-of-bounds frame offsets, wrong section
//!    encoding, reserved/unknown IDs) are asserted to produce typed errors;
//! 2. **fails when it did not run**: every sweep counts its refusals and
//!    asserts the count is nonzero — a sweep that silently skipped would
//!    fail the count assertion, not pass vacuously.
//!
//! Random byte-flip sweeps additionally accept "decodes to SOME values":
//! per spec §1 the CRC layer above owns detection of value-level damage;
//! the kernels owe memory safety + typed structural refusals only.

use super::*;
use crate::alpc::{AlpEncoder, AlpF32Encoder};
use crate::bytefor::{granule_min_width, ByteForEncoder};
use crate::deltafor::DeltaForEncoder;
use crate::dictcodes::DictCodesEncoder;
use crate::ffor::FforEncoder;
use pgrc_format::abi::{ByteArena, DecodeOut, KernelKey};
use pgrc_format::class::{StorageClass, CLASS_BYVAL, CLASS_VARLENA};
use pgrc_format::enc::EncodingId;
use pgrc_format::part::StreamSectionHdr;
use pgrc_format::FormatError;

/// Decode granule 0 of a (possibly corrupted) section image; Ok(()) when it
/// decoded, Err on typed refusal. Must never panic or fault.
fn try_decode(b: &Built, section: &[u8]) -> Result<(), FormatError> {
    let hdr = StreamSectionHdr::decode(section)?;
    let ft = hdr.frame_table(section)?;
    let vt = crate::registry().resolve(b.key())?;
    let ctx = b.ctx_with(0, b.key(), section, ft.as_deref());
    let mut datums = vec![0u64; ctx.rows as usize];
    let mut arena_buf = vec![0u8; 1 << 21];
    let mut out = DecodeOut {
        datums: &mut datums,
        arena: ByteArena::new(&mut arena_buf),
    };
    (vt.decode_full)(&ctx, &mut out)?;
    Ok(())
}

fn corruption_sweep(b: &Built, label: &str, seed0: u64) {
    let section = b.section().to_vec();
    // Tooth 1a: every truncation refuses typed.
    let mut refusals = 0usize;
    for cut in 0..section.len().min(400) {
        if try_decode(b, &section[..cut]).is_err() {
            refusals += 1;
        }
    }
    assert!(refusals > 0, "{label}: truncation sweep never fired");
    // Tooth 1b + 2: seeded random byte flips — never a panic, count the
    // typed refusals, and require the sweep to have observed at least one
    // (a structurally-armored section that shrugs off every flip would be
    // suspicious; headers alone guarantee refusals).
    let mut seed = seed0;
    let mut flip_refusals = 0usize;
    for trial in 0..800 {
        let mut bad = section.clone();
        let n = 1 + (splitmix(&mut seed) % 4) as usize;
        for _ in 0..n {
            // Half the trials bias into the header/table-offset region —
            // the structurally sensitive bytes — so the tooth provably
            // fires; the other half sweep the whole section for safety.
            let span = if trial % 2 == 0 {
                bad.len().min(64)
            } else {
                bad.len()
            };
            let i = (splitmix(&mut seed) as usize) % span;
            bad[i] ^= (splitmix(&mut seed) as u8) | 1;
        }
        if try_decode(b, &bad).is_err() {
            flip_refusals += 1;
        }
    }
    assert!(flip_refusals > 0, "{label}: flip sweep never fired");
}

#[test]
fn byte_for_corruption_teeth() {
    let mut seed = 0xC0_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gd = roundtrip_int_corpus(1, 4096, &mut seed);
    let inputs = [gd.input(class)];
    let width = granule_min_width(&inputs[0], true);
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);
    corruption_sweep(&built, "byte_for", 0xC0FE);
}

#[test]
fn delta_for_corruption_teeth() {
    let mut seed = 0xC1_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gd = roundtrip_int_corpus(4, 4096, &mut seed);
    let inputs = [gd.input(class)];
    let mut enc = DeltaForEncoder::default();
    let built = build_stream(&mut enc, &inputs, 0, true);
    corruption_sweep(&built, "delta_for", 0xC1FE);
    // Seeded defect: an illegal width byte in a frame header refuses.
    let mut bad = built.section().to_vec();
    let hdr = StreamSectionHdr::decode(&bad).expect("hdr");
    let ft = hdr.frame_table(&bad).expect("ft").expect("present");
    let width_off = pgrc_format::part::STREAM_SECTION_HDR_LEN + ft[0] as usize + 8;
    bad[width_off] = 3; // not in {1,2,4,8}
    assert!(
        matches!(try_decode(&built, &bad), Err(FormatError::Corrupt { .. })),
        "illegal delta width must refuse Corrupt"
    );
}

#[test]
fn ffor_corruption_teeth() {
    let mut seed = 0xC2_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gd = roundtrip_int_corpus(0, 4096, &mut seed);
    let inputs = [gd.input(class)];
    let mut enc = FforEncoder { signed: true, carry: None };
    let built = build_stream(&mut enc, &inputs, 0, true);
    corruption_sweep(&built, "ffor", 0xC2FE);
    // Seeded defect: bit_width > 64 refuses.
    let mut bad = built.section().to_vec();
    let hdr = StreamSectionHdr::decode(&bad).expect("hdr");
    let ft = hdr.frame_table(&bad).expect("ft").expect("present");
    let bw_off = pgrc_format::part::STREAM_SECTION_HDR_LEN + ft[0] as usize + 8;
    bad[bw_off] = 65;
    assert!(
        try_decode(&built, &bad).is_err(),
        "bit width 65 must refuse"
    );
}

#[test]
fn alp_corruption_teeth() {
    let mut seed = 0xC3_u64;
    let gd = roundtrip_float_corpus(0, 4096, &mut seed);
    let inputs = [gd.input(StorageClass::F64)];
    let mut enc = AlpEncoder {
        encoding: EncodingId::Alp,
        carry: None,
    };
    let built = build_stream(&mut enc, &inputs, 0, false);
    corruption_sweep(&built, "alp", 0xC3FE);
    // Seeded defect: an unknown scheme tag refuses.
    let mut bad = built.section().to_vec();
    let hdr = StreamSectionHdr::decode(&bad).expect("hdr");
    let ft = hdr.frame_table(&bad).expect("ft").expect("present");
    let tag_off = pgrc_format::part::STREAM_SECTION_HDR_LEN + ft[0] as usize;
    bad[tag_off] = 9;
    assert!(
        matches!(try_decode(&built, &bad), Err(FormatError::Corrupt { .. })),
        "unknown alp scheme tag must refuse Corrupt"
    );
}

#[test]
fn dict_codes_corruption_teeth() {
    let mut seed = 0xC4_u64;
    let ndv = 100u32;
    let (index, payload) = super::roundtrip::dict_fixture(ndv);
    let rows = 4096u32;
    let datums = (0..rows)
        .map(|_| splitmix(&mut seed) % ndv as u64)
        .collect();
    let gd = GranuleData {
        rows,
        datums,
        validity: None,
    };
    let inputs = [gd.input(StorageClass::VarlenaVerbatim)];
    let mut enc = DictCodesEncoder {
        class: CLASS_VARLENA,
        max_width: 32,
        byte_align: false,
    };
    let built = build_stream(&mut enc, &inputs, 0, false);
    corruption_sweep(&built, "dict_codes", 0xC4FE);

    // Seeded defect: an out-of-range code (base bumped past NDV) must
    // refuse at materialization — the #340 incident-class law.
    let mut bad = built.section().to_vec();
    let hdr = StreamSectionHdr::decode(&bad).expect("hdr");
    let ft = hdr.frame_table(&bad).expect("ft").expect("present");
    let base_off = pgrc_format::part::STREAM_SECTION_HDR_LEN + ft[0] as usize - 8;
    bad[base_off..base_off + 4].copy_from_slice(&(ndv + 7).to_le_bytes());
    let ctx_hdr = StreamSectionHdr::decode(&bad).expect("hdr");
    let bft = ctx_hdr.frame_table(&bad).expect("ft");
    let vt = crate::registry().resolve(built.key()).expect("resolves");
    let mut ctx = built.ctx_with(0, built.key(), &bad, bft.as_deref());
    ctx.dict = Some(pgrc_format::dict::DictSections {
        index: &index[..],
        payload: &payload[..],
        entry_count: ndv,
        charlen_form: pgrc_format::dict::DictCharLenForm::Absolute,
        form: pgrc_format::dict::DictPayloadForm::Expanded,
    });
    let mut datums = vec![0u64; rows as usize];
    let mut arena_buf = vec![0u8; 1 << 20];
    let mut out = DecodeOut {
        datums: &mut datums,
        arena: ByteArena::new(&mut arena_buf),
    };
    assert!(
        matches!(
            (vt.decode_full)(&ctx, &mut out),
            Err(FormatError::Bounds { .. })
        ),
        "out-of-range dict code must refuse Bounds"
    );
}

#[test]
fn unknown_and_unassigned_ids_refuse_before_any_kernel() {
    // The retired FSST id 12 is RESERVED: it refuses typed before any table
    // walk; the unassigned 13..=127 band refuses UnknownEncoding.
    let reg = crate::registry();
    let reserved = reg.resolve(KernelKey {
        encoding: 12,
        class: CLASS_VARLENA,
        width: 0,
    });
    assert!(
        matches!(reserved, Err(FormatError::ReservedEncoding { id: 12 })),
        "retired id 12 must refuse ReservedEncoding, got {:?}",
        reserved.err()
    );
    let shifted = reg.resolve(KernelKey {
        encoding: 13,
        class: CLASS_VARLENA,
        width: 0,
    });
    assert!(
        matches!(shifted, Err(FormatError::UnknownEncoding { id: 13 })),
        "unassigned band id must refuse UnknownEncoding, got {:?}",
        shifted.err()
    );
    let unknown = reg.resolve(KernelKey {
        encoding: 0xBEEF,
        class: CLASS_BYVAL,
        width: 8,
    });
    assert!(
        matches!(unknown, Err(FormatError::UnknownEncoding { id: 0xBEEF })),
        "unknown id must refuse UnknownEncoding, got {:?}",
        unknown.err()
    );
    // Structural IDs never resolve to kernels either (they never appear in
    // stream entries; dispatch_shape.rs pins the key-normalization refusal).
    for id in [EncodingId::ArrayDual, EncodingId::JsonbShred] {
        let r = reg.resolve(KernelKey {
            encoding: id.as_u16(),
            class: CLASS_VARLENA,
            width: 0,
        });
        assert!(
            matches!(r, Err(FormatError::KernelMissing { .. })),
            "structural id {id:?} must have no kernel, got {:?}",
            r.err()
        );
    }
}

#[test]
fn alp_f32_corruption_teeth() {
    let mut seed = 0xC8_u64;
    let rows = 4096u32;
    let validity = validity_pattern(1, rows, &mut seed);
    let datums = (0..rows)
        .map(|_| ((splitmix(&mut seed) % 100_000) as f32 / 100.0).to_bits() as u64)
        .collect();
    let gd = GranuleData {
        rows,
        datums,
        validity,
    };
    let inputs = [gd.input(StorageClass::F32)];
    let mut enc = AlpF32Encoder::default();
    let built = build_stream(&mut enc, &inputs, 0, false);
    corruption_sweep(&built, "alp_f32", 0xC8FE);
    // Seeded defects: an unknown scheme tag refuses — and so does the RD
    // tag (1), which exists in the f64 vocabulary but has no f32 arm.
    for (tag, what) in [(9u8, "unknown tag"), (1u8, "RD tag at f32 width")] {
        let mut bad = built.section().to_vec();
        let hdr = StreamSectionHdr::decode(&bad).expect("hdr");
        let ft = hdr.frame_table(&bad).expect("ft").expect("present");
        let tag_off = pgrc_format::part::STREAM_SECTION_HDR_LEN + ft[0] as usize;
        bad[tag_off] = tag;
        assert!(
            matches!(try_decode(&built, &bad), Err(FormatError::Corrupt { .. })),
            "{what} must refuse Corrupt"
        );
    }
}

#[test]
fn wrapped_section_corruption_teeth() {
    let mut seed = 0xC5_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gd = roundtrip_int_corpus(5, 8192, &mut seed);
    let inputs = [gd.input(class)];
    let width = granule_min_width(&inputs[0], true);
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);
    let mut wrapped = Vec::new();
    crate::wrapper::wrap_section(
        built.section(),
        &built.build.granule_payload_ends,
        pgrc_format::enc::Wrapper::Lz4,
        &mut wrapped,
    )
    .expect("wrap");
    let mut scratch = Vec::new();
    let mut refusals = 0usize;
    for cut in 0..wrapped.len().min(300) {
        if crate::wrapper::unwrap_section(&wrapped[..cut], &mut scratch).is_err() {
            refusals += 1;
        }
    }
    assert!(refusals > 0, "wrapped truncation sweep never fired");
    for _ in 0..600 {
        let mut bad = wrapped.clone();
        let i = (splitmix(&mut seed) as usize) % bad.len();
        bad[i] ^= (splitmix(&mut seed) as u8) | 1;
        let _ = crate::wrapper::unwrap_section(&bad, &mut scratch); // no panic, no UB
    }
    // LZ4 blocks mislabeled as Zstd (the old typed-refusal pin's byte flip,
    // now that the arm is implemented): the frame machinery must refuse
    // TYPED — never a silent wrong answer, never a panic.
    let mut zstd = wrapped.clone();
    zstd[7] = 2;
    assert!(
        matches!(
            crate::wrapper::unwrap_section(&zstd, &mut scratch),
            Err(FormatError::Corrupt { .. })
        ),
        "LZ4 payload under a Zstd wrapper byte must refuse Corrupt"
    );
    // Unknown wrapper id: refusal-before-fault stays typed — the id rides
    // the error (the old-binary posture against a future layer).
    let mut unk = wrapped.clone();
    unk[7] = 200;
    assert!(
        matches!(
            crate::wrapper::unwrap_section(&unk, &mut scratch),
            Err(FormatError::WrapperUnsupported { wrapper: 200 })
        ),
        "unknown wrapper id must refuse WrapperUnsupported"
    );
    // The dictionary layers on a non-dict section: the index-needing
    // payload arm refuses the role-agnostic face typed; the packed-index
    // arm refuses the LZ4 bytes as corruption.
    let mut fc = wrapped.clone();
    fc[7] = 3;
    assert!(matches!(crate::wrapper::unwrap_section(&fc, &mut scratch), Err(FormatError::Corrupt { .. })));
    let mut pk = wrapped.clone();
    pk[7] = 4;
    assert!(matches!(crate::wrapper::unwrap_section(&pk, &mut scratch), Err(FormatError::Corrupt { .. })));
}

#[test]
fn zstd_wrapped_section_corruption_teeth() {
    // The CMP-A arm under the same two-tooth law as LZ4: truncations refuse
    // typed and counted; random flips never panic; the payload-swap and
    // unknown-id refusals stay typed.
    let mut seed = 0xC6_u64;
    let class = StorageClass::ByvalWord {
        width: 8,
        signed: true,
    };
    let gd = roundtrip_int_corpus(5, 8192, &mut seed);
    let inputs = [gd.input(class)];
    let width = granule_min_width(&inputs[0], true);
    let mut enc = ByteForEncoder::new_bytefor(8, width, true);
    let built = build_stream(&mut enc, &inputs, 0, true);
    let mut wrapped = Vec::new();
    crate::wrapper::wrap_section(
        built.section(),
        &built.build.granule_payload_ends,
        pgrc_format::enc::Wrapper::Zstd,
        &mut wrapped,
    )
    .expect("wrap");
    let mut scratch = Vec::new();
    // Round-trip sanity before the teeth: the wrapped image rebuilds.
    crate::wrapper::unwrap_section(&wrapped, &mut scratch).expect("unwrap");
    let mut refusals = 0usize;
    for cut in 0..wrapped.len().min(300) {
        if crate::wrapper::unwrap_section(&wrapped[..cut], &mut scratch).is_err() {
            refusals += 1;
        }
    }
    assert!(refusals > 0, "zstd wrapped truncation sweep never fired");
    let mut flip_refusals = 0usize;
    for _ in 0..600 {
        let mut bad = wrapped.clone();
        let i = (splitmix(&mut seed) as usize) % bad.len();
        bad[i] ^= (splitmix(&mut seed) as u8) | 1;
        // No panic, no UB; count typed refusals so the sweep provably ran.
        if crate::wrapper::unwrap_section(&bad, &mut scratch).is_err() {
            flip_refusals += 1;
        }
    }
    assert!(flip_refusals > 0, "zstd flip sweep never fired");
    // Zstd frames mislabeled as LZ4: typed refusal, never silent.
    let mut lz4 = wrapped.clone();
    lz4[7] = 1;
    assert!(
        crate::wrapper::unwrap_section(&lz4, &mut scratch).is_err(),
        "Zstd payload under an LZ4 wrapper byte must refuse typed"
    );
}
