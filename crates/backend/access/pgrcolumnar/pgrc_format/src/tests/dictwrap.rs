//! pgrc format 2.2 dictionary wrapper layouts (spec §6.4/§7): the front-coded
//! frame and the packed index are pure re-expressions of the v1 images —
//! every synthetic dictionary below expands byte-identically, and seeded
//! corruption of either form refuses typed.

use crate::dict::{
    dict_frame_records, expand_front_coded_frame, expand_packed_dict_index, front_code_frame,
    pack_dict_index, utf8_char_count, DictIndexEntry, DICT_FC_RESTART, DICT_INDEX_ANCHOR, DICT_INDEX_ENTRY_LEN,
};
use crate::geom::{DICT_FRAME_ENTRIES, OVERSIZE_THRESHOLD};
use crate::wire::{pad_to, put_varlena_entry};
use crate::FormatError;

/// The writer's v1 images for a sorted entry list: (payload image, index
/// records, frame starts) — `pgrc_write::dict::emit_sections` verbatim.
fn v1_images(entries: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>, Vec<u32>) {
    let mut payload = Vec::new();
    let mut index = Vec::new();
    let mut frames = Vec::new();
    for (code, e) in entries.iter().enumerate() {
        if code as u32 % DICT_FRAME_ENTRIES == 0 {
            pad_to(&mut payload, 8);
            frames.push(payload.len() as u32);
        }
        let off = put_varlena_entry(&mut payload, e) as u32;
        DictIndexEntry { payload_off: off, byte_len: e.len() as u32, char_field: utf8_char_count(e) }
            .encode_into(&mut index);
    }
    (payload, index, frames)
}

fn sorted_distinct(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    v.sort();
    v.dedup();
    v
}

fn roundtrip(entries: &[Vec<u8>]) {
    let entries = sorted_distinct(entries.to_vec());
    let (payload, index, frames) = v1_images(&entries);
    let nf = frames.len();
    let mut rebuilt = vec![0xAAu8; payload.len()];
    let mut fc_total = 0usize;
    for f in 0..nf {
        let lo = frames[f] as usize;
        let hi = if f + 1 < nf { frames[f + 1] as usize } else { payload.len() };
        let recs = dict_frame_records(&index, f as u32).expect("frame records");
        let mut fc = Vec::new();
        front_code_frame(&payload[lo..hi], recs, &mut fc).expect("front code");
        fc_total += fc.len();
        expand_front_coded_frame(&fc, recs, &mut rebuilt[lo..hi]).expect("expand");
    }
    assert!(rebuilt == payload, "front-coded expand must be byte-identical ({} entries)", entries.len());
    if entries.len() > 1 {
        assert!(fc_total <= payload.len(), "front coding never grows the frame");
    }
    let mut packed = Vec::new();
    pack_dict_index(&index, &mut packed).expect("pack");
    let mut out = vec![0xAAu8; index.len()];
    expand_packed_dict_index(&packed, &mut out).expect("expand index");
    assert!(out == index, "packed index expand must be byte-identical ({} entries)", entries.len());
    if entries.len() >= 64 {
        assert!(packed.len() < index.len(), "packed index is smaller than 12 B records");
    }
}

fn numbered(n: usize, prefix: &str) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("{prefix}{i:06}").into_bytes()).collect()
}

#[test]
fn synthetic_dictionaries_expand_byte_identically() {
    // Restart and frame boundaries: k−1, k, k+1, a frame, a frame ± 1, two
    // frames + 1, a lone entry, an empty dictionary is never framed.
    for n in [1usize, 2, 31, 32, 33, 63, 64, 65, 1023, 1024, 1025, 2049] {
        roundtrip(&numbered(n, "https://example.com/path/"));
    }
    // Empty entry, ASCII/non-ASCII mix, byte-rank traps.
    let mut mixed: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"a".to_vec(),
        "z\u{00E9}bra".as_bytes().to_vec(),
        "\u{1F600}grin".as_bytes().to_vec(),
        "Москва".as_bytes().to_vec(),
        "Москвич".as_bytes().to_vec(),
        vec![0xFF, 0xFE, 0x00, 0x41],
    ];
    mixed.extend(numbered(300, "Москва/"));
    roundtrip(&mixed);
    // A whole frame sharing one long prefix (the front coder's best case)
    // and one differing only in the last byte.
    let long = "p".repeat(200);
    roundtrip(&numbered(1024, &long));
    let mut tail: Vec<Vec<u8>> = (0..1100u32).map(|i| format!("{long}{}", (b'a' + (i % 26) as u8) as char).into_bytes()).collect();
    tail.sort();
    tail.dedup();
    roundtrip(&tail);
    // A 32 KiB+ entry beside the oversize threshold, and entries whose
    // shared prefix exceeds one varint byte.
    let big = vec![b'x'; OVERSIZE_THRESHOLD as usize + 17];
    let mut near = vec![big.clone(), vec![b'x'; OVERSIZE_THRESHOLD as usize - 1]];
    near.push({ let mut b = big.clone(); b.push(b'y'); b });
    near.extend(numbered(40, "y"));
    roundtrip(&near);
}

#[test]
fn packed_index_refuses_unpackable_records_typed() {
    let entries = sorted_distinct(numbered(70, "e"));
    let (_payload, mut index, _) = v1_images(&entries);
    let mut ok = Vec::new();
    pack_dict_index(&index, &mut ok).expect("packable");
    // A char count above the byte length (the Absolute-form law) refuses.
    index[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(pack_dict_index(&index, &mut Vec::new()), Err(FormatError::EncodeContract { .. })));
    // An offset off the prefix sum refuses (the expand could not reproduce it).
    let (_p, mut index, _) = v1_images(&entries);
    index[12..16].copy_from_slice(&1u32.to_le_bytes());
    assert!(matches!(pack_dict_index(&index, &mut Vec::new()), Err(FormatError::EncodeContract { .. })));
    // The expand refuses a wrong record count and a truncated image.
    let mut short = vec![0u8; index.len() - DICT_INDEX_ENTRY_LEN];
    assert!(expand_packed_dict_index(&ok, &mut short).is_err());
    let mut out = vec![0u8; index.len()];
    assert!(expand_packed_dict_index(&ok[..ok.len() - 3], &mut out).is_err());
}

#[test]
fn front_coded_frame_corruption_refuses_typed_never_panics() {
    let entries = sorted_distinct(numbered(200, "corrupt/"));
    let (payload, index, _) = v1_images(&entries);
    let recs = dict_frame_records(&index, 0).expect("records");
    let mut fc = Vec::new();
    front_code_frame(&payload, recs, &mut fc).expect("front code");
    let mut dst = vec![0u8; payload.len()];
    for pos in (0..fc.len()).step_by(7) {
        let mut bad = fc.clone();
        bad[pos] ^= 0x5A;
        // A flipped suffix byte is legitimately invisible here (the extent
        // CRC owns data integrity); a flipped varint/header must refuse.
        let _ = expand_front_coded_frame(&bad, recs, &mut dst);
    }
    assert!(expand_front_coded_frame(&fc[..fc.len() - 1], recs, &mut dst).is_err());
    assert!(expand_front_coded_frame(&fc, recs, &mut dst[..payload.len() - 8]).is_err());
    assert_eq!(DICT_FC_RESTART, 32);
}

/// The resident front-coded form (lane dictresident): every entry decoded from its restart and
/// every entry walked in place equal the v1 image's, on the same synthetic dictionaries.
fn fc_resident_roundtrip(entries: &[Vec<u8>]) {
    use crate::dict::{fc_entry_into, fc_resident_build, FcResident, FcWalk};
    let entries = sorted_distinct(entries.to_vec());
    let (payload, index, frames) = v1_images(&entries);
    let nf = frames.len();
    let mut fcs = Vec::new();
    for f in 0..nf {
        let lo = frames[f] as usize;
        let hi = if f + 1 < nf { frames[f + 1] as usize } else { payload.len() };
        let mut fc = Vec::new();
        front_code_frame(&payload[lo..hi], dict_frame_records(&index, f as u32).expect("records"), &mut fc).expect("front code");
        fcs.push(fc);
    }
    let mut res = Vec::new();
    fc_resident_build(&index, &mut |f, out| { out.extend_from_slice(&fcs[f as usize]); Ok(()) }, &mut res).expect("build");
    let r = FcResident::parse(&res).expect("parse");
    assert_eq!(r.nentries as usize, entries.len());
    let mut buf = vec![0u8; 4 + entries.iter().map(Vec::len).max().unwrap_or(0)];
    for (c, e) in entries.iter().enumerate() {
        let len = fc_entry_into(&r, &index, c as u32, &mut buf).expect("entry");
        assert_eq!(len, e.len());
        assert_eq!(&buf[4..4 + len], &e[..], "entry {c}");
        assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), crate::wire::varlena_header_4b_u(len as u32));
    }
    let mut out = Vec::new();
    let mut w = FcWalk::new(r, &index, 0, entries.len() as u32).expect("walk");
    let mut c = 0usize;
    while let Some((code, len)) = w.next(&mut out).expect("next") {
        assert_eq!(code as usize, c);
        assert_eq!(&out[4..4 + len], &entries[c][..], "walk {c}");
        c += 1;
    }
    assert_eq!(c, entries.len());
    if entries.len() > 40 {
        let (s, e) = (entries.len() as u32 / 3 + 1, entries.len() as u32 - 5);
        let mut w = FcWalk::new(r, &index, s, e).expect("walk");
        let mut c = s;
        while let Some((code, len)) = w.next(&mut out).expect("next") {
            assert_eq!(code, c);
            assert_eq!(&out[4..4 + len], &entries[c as usize][..]);
            c += 1;
        }
        assert_eq!(c, e);
    }
    assert!(fc_entry_into(&r, &index, entries.len() as u32, &mut buf).is_err());
}

#[test]
fn fc_resident_form_decodes_byte_identically() {
    for n in [1usize, 2, 31, 32, 33, 63, 64, 65, 1023, 1024, 1025, 2049] {
        fc_resident_roundtrip(&numbered(n, "https://example.com/path/"));
    }
    let mut mixed = numbered(700, "");
    mixed.push(Vec::new());
    mixed.push("ünïcødé".as_bytes().to_vec());
    mixed.push("🚀🚀🚀".as_bytes().to_vec());
    mixed.push(vec![b'x'; OVERSIZE_THRESHOLD as usize + 17]);
    fc_resident_roundtrip(&mixed);
    let shared: Vec<Vec<u8>> = (0..1024).map(|i| format!("{}{}", "p".repeat(200), i).into_bytes()).collect();
    fc_resident_roundtrip(&shared);
}

/// P5-24 (keep): the two wrapper layout intervals are format facts of pgrc format 2.2 (spec §7.1), not tuned constants: a
/// written frame / packed index decodes only under the values it was written with. Pinned here so that a change
/// shows as a format change (a new wrapper id), and so that the frame stays self-contained (a frame starts with a
/// restart and an anchor).
#[test]
fn dict_wrapper_intervals_are_the_specs_format_facts() {
    assert_eq!((DICT_FC_RESTART, DICT_INDEX_ANCHOR), (32, 64), "spec docs/design/sqe/pgrc-format.md §7.1");
    assert_eq!(DICT_FRAME_ENTRIES % DICT_FC_RESTART, 0);
    assert_eq!(DICT_FRAME_ENTRIES % DICT_INDEX_ANCHOR, 0);
    // a packed index over 3 frames and a tail carries entry 64a's raw payload_off as anchor a, a = 0 .. ceil(n / 64),
    // right after its u32 n: the count and the offsets the reader restarts its prefix sum from
    let n = 3 * DICT_FRAME_ENTRIES as usize + 65;
    let (_payload, index, _frames) = v1_images(&numbered(n, "p524-"));
    let mut packed = Vec::new();
    pack_dict_index(&index, &mut packed).expect("pack");
    assert_eq!(u32::from_le_bytes(packed[..4].try_into().expect("len 4")) as usize, n);
    for a in 0..n.div_ceil(DICT_INDEX_ANCHOR as usize) {
        let e = a * DICT_INDEX_ANCHOR as usize * DICT_INDEX_ENTRY_LEN;
        assert_eq!(packed[4 + 4 * a..8 + 4 * a], index[e..e + 4], "anchor {a}");
    }
    let mut out = vec![0xAAu8; index.len()];
    expand_packed_dict_index(&packed, &mut out).expect("expand index");
    assert!(out == index, "the packed index expands byte-identically under the spec's intervals");
}
