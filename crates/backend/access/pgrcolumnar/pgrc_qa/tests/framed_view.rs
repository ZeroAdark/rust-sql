//! TH14's source lever (main 09-24): `StreamCursor::framed` — the resolved granule of a framed root stream straight off the
//! cursor's loaded extent (no ctx build, no section header parse) — is the ctx's own view of that granule (`Framed::of_ctx`
//! through `granule_ctx`: the same payload bytes, frame table, frame base and value count), and the at-position gather over it
//! gives the values `decode_sel` gives, at each selected row, as the datum word and as a narrowed i32 / i16 lane. A stream of
//! another encoding (or a request for another encoding) has no framed view.
//!
//! Each granule's first BYTE_FOR ask on a fresh cursor is its first touch: `framed` loads the values extent exactly as
//! `decode_sel` does (`ensure_values`), never answering None for an extent not yet resident. Every seventh row is NULL and
//! the keep-all selection covers those rows: a NULL row's word under the slot is the frame's, the same as `decode_sel`'s.

use pgrc_codec::bytefor::{sel_at_framed, Framed};
use pgrc_format::abi::{ByteArena, DecodeOut, Selection};
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::enc::EncodingId;
use pgrc_qa::adapters::{full_binding, ForcedPlan};
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::cursor::StreamCursor;
use pgrc_write::writer::PartCutPolicy;
use std::sync::Arc;

/// Three full granules and a short fourth: the view's frame base moves past granule 0 and the last granule is short.
const ROWS: u64 = 3 * 8192 + 777;

fn mix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The fixture's one column under `plan`, values in a band of `span` around zero, every seventh row null.
fn check(name: &'static str, relf: u64, plan: ForcedPlan, span: u64, framed: bool) {
    let fx = int8_fixture(name, relf, ROWS, vec![plan], PartCutPolicy::default(), |i| {
        let mut s = i ^ relf;
        (i % 7 != 3).then(|| (mix(&mut s) % span) as i64 - (span / 2) as i64)
    });
    let built = build_fixture(fx);
    let bytes = built.files.get(&part_file_name(built.manifest.parts[0].part_no)).expect("part");
    let part = open_part_bytes(bytes, relf).expect("open");
    let mut cur = StreamCursor::open(Arc::clone(&part), full_binding(), 1, 0).expect("cursor");
    let width = cur.key().width;
    assert_eq!(cur.granule_count(), 4, "{name}: granules");
    let mut seed = relf;
    for g in 0..cur.granule_count() {
        let rows = cur.rows_in_granule(g) as usize;
        // another encoding than the stream's never has a view
        assert!(cur.framed(g, EncodingId::PackedNumeric as u16).expect("framed").is_none(), "{name} g{g}: packed numeric asked");
        let mine = cur
            .framed(g, EncodingId::ByteFor as u16)
            .expect("framed")
            .map(|f| (f.payload.as_ptr() as usize, f.payload.len(), f.frame_table.to_vec(), f.fbase, f.values));
        if !framed {
            assert!(mine.is_none(), "{name} g{g}: a {:?} stream has no BYTE_FOR view", cur.key());
            continue;
        }
        let mine = mine.expect("a BYTE_FOR root stream has a framed view, its first touch included");
        let theirs = cur
            .granule_ctx(g, |ctx| {
                Framed::of_ctx(ctx, EncodingId::ByteFor as u16)
                    .map(|v| (v.payload.as_ptr() as usize, v.payload.len(), v.frame_table.to_vec(), v.fbase, v.values))
            })
            .expect("ctx view");
        assert_eq!(mine, theirs, "{name} g{g}: the cursor's view is the ctx's view");
        assert_eq!(mine.4 as usize, rows, "{name} g{g}: a root stream's values are its rows");
        for keep in [1u64, 3, 97] {
            let sel_rows: Vec<u16> = (0..rows as u16).filter(|_| mix(&mut seed) % keep == 0).collect();
            let (sel, n) = (Selection { rows: &sel_rows }, sel_rows.len());
            let mut want = vec![0u64; n.max(1)];
            let mut arena = [0u8; 64];
            let out = &mut DecodeOut { datums: &mut want[..n], arena: ByteArena::new(&mut arena) };
            assert_eq!(cur.decode_sel(g, &sel, out).expect("decode_sel"), n as u32);
            let f = cur.framed(g, EncodingId::ByteFor as u16).expect("framed").expect("view");
            let f = Framed { payload: f.payload, frame_table: f.frame_table, fbase: f.fbase, values: f.values };
            let (mut at, mut at32, mut at16) = (vec![!0u64; rows], vec![0x5A5A_5A5Ai32; rows], vec![0x5A5Ai16; rows]);
            assert_eq!(sel_at_framed(&f, width, &sel, &mut at).expect("u64"), n as u32);
            assert_eq!(sel_at_framed(&f, width, &sel, &mut at32).expect("i32"), n as u32);
            assert_eq!(sel_at_framed(&f, width, &sel, &mut at16).expect("i16"), n as u32);
            let mut picked = vec![usize::MAX; rows];
            sel_rows.iter().enumerate().for_each(|(i, &r)| picked[r as usize] = i);
            for r in 0..rows {
                let got = (at[r], at32[r], at16[r]);
                match picked[r] {
                    usize::MAX => assert_eq!(got, (!0, 0x5A5A_5A5A, 0x5A5A), "{name} g{g} row {r}: untouched"),
                    i => assert_eq!(got, (want[i], want[i] as i32, want[i] as i16), "{name} g{g} row {r}: == decode_sel"),
                }
            }
        }
    }
}

#[test]
fn byte_for_view_is_the_ctx_view_at_each_delta_width() {
    check("framed_bf1", 9401, ForcedPlan::ByteFor { delta_width: 1, signed: true }, 200, true);
    check("framed_bf2", 9402, ForcedPlan::ByteFor { delta_width: 2, signed: true }, 50_000, true);
    check("framed_bf4", 9403, ForcedPlan::ByteFor { delta_width: 4, signed: true }, 3_000_000_000, true);
}

#[test]
fn delta_for_stream_has_no_byte_for_view() {
    check("framed_dfor", 9404, ForcedPlan::DeltaFor, 50_000, false);
}
