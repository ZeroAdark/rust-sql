//! The integer pre-pass wrapper (`intpre.rs`): per-scheme and per-view round-trips, wrapped sections through every
//! codec unwrap face, the plain-only settle to Zstd, determinism, the factor gate and typed refusals on corrupt input.

use super::{build_stream, splitmix, Built, GranuleData};
use crate::bytefor::{granule_min_width, ByteForEncoder};
use crate::deltafor::DeltaForEncoder;
use crate::dictcodes::DictCodesEncoder;
use crate::intpre::{prepass_decode, prepass_image, scheme_decode, scheme_image, settle, View};
use crate::wrapper::{
    unwrap_block_into, unwrap_section, unwrap_section_until, wrap_section, wrap_section_bound, wrap_section_with, ZstdAdapt, ADAPT_SAMPLE,
};
use pgrc_format::abi::EncodeInput;
use pgrc_format::class::{StorageClass, CLASS_VARLENA};
use pgrc_format::enc::Wrapper;
use pgrc_format::part::{StreamSectionHdr, STREAM_SECTION_HDR_LEN};
use pgrc_format::FormatError;

fn corpora() -> Vec<(&'static str, Vec<u64>)> {
    let mut s = 0x1A7E_u64;
    let mut ts = 1_372_708_800_000_000u64;
    let scale_1e6: Vec<u64> = (0..6000)
        .map(|i| {
            let r = splitmix(&mut s);
            ts = match i % 7 {
                0 => ts.wrapping_sub((r % 86_400) * 1_000_000),
                _ => ts + (r % 90) * 1_000_000,
            };
            ts
        })
        .collect();
    let exceptions: Vec<u64> =
        (0..6000).map(|_| if splitmix(&mut s) % 50 == 0 { splitmix(&mut s) >> 24 } else { splitmix(&mut s) % 256 }).collect();
    let nested: Vec<u64> = (0..8000)
        .map(|_| match splitmix(&mut s) % 400 {
            0 => splitmix(&mut s) >> 10,
            1..=20 => splitmix(&mut s) >> 44,
            _ => splitmix(&mut s) % 16,
        })
        .collect();
    let mut runs = Vec::new();
    while runs.len() < 6000 {
        let v = splitmix(&mut s) as u32 as u64;
        let n = 1 + splitmix(&mut s) % 50;
        runs.extend(std::iter::repeat_n(v, n as usize));
    }
    vec![
        ("empty", vec![]),
        ("one", vec![42]),
        ("one-max", vec![u64::MAX]),
        ("two", vec![u64::MAX, 0]),
        ("all-equal", vec![7; 5000]),
        ("arith", (0..5000u64).map(|i| 1000 + 3 * i).collect()),
        ("arith-down", (0..5000u64).map(|i| u64::MAX - 11 * i).collect()),
        ("arith-runs", (0..5000u64).map(|i| (i / 100) * 1_000_000 + (i % 100) * 7).collect()),
        ("i64-extremes", (0..4000).map(|i| [i64::MIN as u64, i64::MAX as u64, 0, u64::MAX][i % 4]).collect()),
        ("negative-deltas", (0..5000i64).map(|i| (1_000_000 - i * 37) as u64).collect()),
        ("scale-1e6", scale_1e6),
        ("scale-2^63", (0..100u64).map(|i| (i % 2) << 63).collect()),
        ("scale-prime", (0..3000u64).map(|i| (i * i % 977) * 1_000_003).collect()),
        ("exceptions", exceptions),
        ("nested-exceptions", nested),
        ("runs", runs),
        ("random", (0..4000).map(|_| splitmix(&mut s)).collect()),
        ("random-small", (0..4000).map(|_| splitmix(&mut s) % 1000).collect()),
    ]
}

#[test]
fn every_scheme_roundtrips_every_corpus() {
    for (name, vals) in corpora() {
        for scheme in 0..=4u8 {
            let Some(img) = scheme_image(scheme, &vals).expect("encode") else {
                assert!(scheme >= 2, "{name}: plain and patched base always apply");
                continue;
            };
            let got = scheme_decode(&img, vals.len(), 8).unwrap_or_else(|e| panic!("{name} scheme {scheme}: {e:?}"));
            assert_eq!(got, vals, "{name} scheme {scheme}");
        }
    }
}

#[test]
fn schemes_apply_where_they_should_and_shrink() {
    let c: std::collections::HashMap<_, _> = corpora().into_iter().collect();
    let size = |s: u8, n: &str| scheme_image(s, &c[n]).expect("encode").map(|i| i.len());
    let raw = |n: &str| c[n].len() * 8;
    assert!(size(3, "all-equal").expect("rle") < 40);
    assert!(size(4, "arith").expect("linear") < 60);
    assert!(size(2, "arith").expect("delta") < 60);
    assert!(size(3, "runs").expect("rle") * 3 < raw("runs"));
    assert!(size(2, "scale-1e6").expect("delta") * 3 < raw("scale-1e6"), "the 1e6 scale must be found");
    assert!(size(1, "exceptions").expect("pfor") * 4 < raw("exceptions"), "patched base must patch the spikes");
    assert!(size(1, "nested-exceptions").expect("pfor") * 4 < raw("nested-exceptions"));
    assert_eq!(size(3, "random"), None, "RLE refuses run-free input");
    assert_eq!(size(4, "random"), None, "linear runs refuse run-free input");
    assert_eq!(size(2, "one"), None, "delta needs two values");
}

#[test]
fn nesting_past_the_depth_refuses_typed() {
    // patched base of 1 value whose single exception nests one level per header: past MAX_DEPTH it refuses.
    let mut img = vec![1u8];
    for _ in 0..=crate::intpre::MAX_DEPTH + 1 {
        img.extend_from_slice(&0u64.to_le_bytes());
        img.push(0);
        img.extend_from_slice(&1u32.to_le_bytes());
    }
    assert!(matches!(scheme_decode(&img, 1, 8), Err(FormatError::Corrupt { .. }) | Err(FormatError::Truncated { .. })));
}

fn byte_for(data: &[Vec<u64>], signed: bool) -> Built {
    let class = StorageClass::ByvalWord { width: 8, signed };
    let gs: Vec<GranuleData> = data.iter().map(|d| GranuleData { rows: d.len() as u32, datums: d.clone(), validity: None }).collect();
    let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
    let width = inputs.iter().map(|g| granule_min_width(g, signed)).max().unwrap();
    let mut enc = ByteForEncoder::new_bytefor(8, width, signed);
    build_stream(&mut enc, &inputs, 0, signed)
}

fn granule_images(b: &Built) -> Vec<Vec<u8>> {
    let sec = b.section();
    let mut g0 = 0usize;
    b.build
        .granule_payload_ends
        .iter()
        .map(|&e| {
            let r = sec[STREAM_SECTION_HDR_LEN + g0..STREAM_SECTION_HDR_LEN + e as usize].to_vec();
            g0 = e as usize;
            r
        })
        .collect()
}

fn assert_view_roundtrips(view: View, raw: &[u8], what: &str) -> bool {
    match prepass_image(view, raw, 100).expect("encode") {
        Some(pre) => {
            let mut dst = vec![0u8; raw.len()];
            prepass_decode(&pre, &mut dst).unwrap_or_else(|e| panic!("{what}: {e:?}"));
            assert_eq!(dst, raw, "{what}: rebuilt image differs");
            true
        }
        None => false,
    }
}

#[test]
fn views_roundtrip_real_encoder_images() {
    let mut engaged = 0;
    for (name, vals) in corpora() {
        if vals.is_empty() {
            continue;
        }
        for signed in [false, true] {
            let full: Vec<u64> = vals.iter().cycle().take(8192).copied().collect();
            let tail = (vals.len() % 1500 + 1).min(vals.len());
            let b = byte_for(&[full, vals[..tail].to_vec()], signed);
            let hdr = StreamSectionHdr::decode(b.section()).expect("hdr");
            for (g, raw) in granule_images(&b).iter().enumerate() {
                engaged += assert_view_roundtrips(View::Frames(hdr.width), raw, &format!("{name} bytefor g{g}")) as usize;
            }
        }
        let gd = GranuleData { rows: vals.len() as u32, datums: vals.clone(), validity: None };
        let b = build_stream(&mut DeltaForEncoder::default(), &[gd.input(StorageClass::ByvalWord { width: 8, signed: true })], 0, true);
        for raw in granule_images(&b) {
            engaged += assert_view_roundtrips(View::DeltaFrames, &raw, &format!("{name} deltafor")) as usize;
        }
        let codes: Vec<u64> = vals.iter().map(|v| v % 100_003).collect();
        let gd = GranuleData { rows: codes.len() as u32, datums: codes, validity: None };
        for byte_align in [false, true] {
            let mut enc = DictCodesEncoder { class: CLASS_VARLENA, max_width: 32, byte_align };
            let b = build_stream(&mut enc, &[gd.input(StorageClass::VarlenaVerbatim)], 0, false);
            for raw in granule_images(&b) {
                engaged += assert_view_roundtrips(View::Codes, &raw, &format!("{name} codes")) as usize;
            }
        }
        for w in [1u8, 2, 4, 8] {
            let raw: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()[..w as usize].to_vec()).collect();
            engaged += assert_view_roundtrips(View::Words(w), &raw, &format!("{name} words w{w}")) as usize;
        }
    }
    assert!(engaged > 40, "the views must engage on most corpora at factor 1 ({engaged})");
}

#[test]
fn the_seal_offers_every_view_but_dict_codes() {
    use crate::intpre::{eligible, view_for};
    use pgrc_format::enc::EncodingId;
    assert!(eligible(EncodingId::ByteFor.as_u16(), 5));
    assert!(eligible(EncodingId::Verbatim.as_u16(), 2));
    assert!(eligible(EncodingId::DeltaFor.as_u16(), 0));
    assert!(!eligible(EncodingId::Verbatim.as_u16(), 0));
    assert!(!eligible(EncodingId::DictCodes.as_u16(), 16));
    assert_eq!(view_for(EncodingId::DictCodes.as_u16(), 16), Some(View::Codes));
}

#[test]
fn codes_view_roundtrips_every_width() {
    let mut s = 0xC0DE_u64;
    let mut engaged = 0;
    for width in 1..=32u32 {
        let top = (1u64 << width) - 1;
        for n in [1usize, 7, 64, 8191, 8192] {
            let mut v = 0u64;
            let codes: Vec<u64> = (0..n)
                .map(|i| {
                    if i % 13 == 0 {
                        v = splitmix(&mut s) & top;
                    }
                    if i == n / 2 { top } else { v }
                })
                .collect();
            let gd = GranuleData { rows: n as u32, datums: codes, validity: None };
            for byte_align in [false, true] {
                let mut enc = DictCodesEncoder { class: CLASS_VARLENA, max_width: 32, byte_align };
                let b = build_stream(&mut enc, &[gd.input(StorageClass::VarlenaVerbatim)], 0, false);
                for raw in granule_images(&b) {
                    engaged += assert_view_roundtrips(View::Codes, &raw, &format!("codes w{width} n{n}")) as usize;
                }
            }
        }
    }
    assert!(engaged > 150, "run-heavy codes must engage at most widths ({engaged})");
}

#[test]
fn randomized_words_roundtrip() {
    let mut s = 0x5EED_u64;
    for i in 0..300 {
        let n = (splitmix(&mut s) % 9000) as usize + 1;
        let w = [1u8, 2, 4, 8][i % 4];
        let mask = if w == 8 { u64::MAX } else { (1u64 << (8 * w)) - 1 };
        let shape = splitmix(&mut s) % 5;
        let mut prev = splitmix(&mut s);
        let vals: Vec<u64> = (0..n)
            .map(|_| {
                let r = splitmix(&mut s);
                prev = match shape {
                    0 => r,
                    1 => prev.wrapping_add(r % 5),
                    2 => if r % 10 == 0 { r } else { prev },
                    3 => prev.wrapping_sub(r % 1000 * 1000),
                    _ => r % (1 + (r >> 60)),
                };
                prev & mask
            })
            .collect();
        let raw: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()[..w as usize].to_vec()).collect();
        for f in [100, 125, 200, 400] {
            if let Some(pre) = prepass_image(View::Words(w), &raw, f).expect("encode") {
                let mut dst = vec![0u8; raw.len()];
                prepass_decode(&pre, &mut dst).expect("decode");
                assert_eq!(dst, raw, "case {i} shape {shape} w{w} f{f}");
            }
        }
    }
}

#[test]
fn factor_gate_keeps_plain_below_the_factor() {
    let mut s = 0xFAC7_u64;
    let raw: Vec<u8> = (0..8192).flat_map(|_| (splitmix(&mut s) >> 24).to_le_bytes()).collect();
    assert!(prepass_image(View::Words(8), &raw, 125).expect("encode").is_some(), "8 -> 5 bytes clears 1.25");
    assert!(prepass_image(View::Words(8), &raw, 200).expect("encode").is_none(), "8 -> 5 bytes misses 2");
}

fn timestamp_like(granules: usize, seed: u64) -> Vec<Vec<u64>> {
    let mut s = seed;
    let mut ts = 1_372_708_800_000_000u64;
    (0..granules)
        .map(|g| {
            let n = if g + 1 == granules { 5000 } else { 8192 };
            (0..n)
                .map(|i| {
                    let r = splitmix(&mut s);
                    ts = if i % 6 == 0 { 1_372_708_800_000_000 + (r % 2_678_400) * 1_000_000 } else { ts + (r % 120) * 1_000_000 };
                    ts
                })
                .collect()
        })
        .collect()
}

fn blocks(image: &[u8]) -> Vec<&[u8]> {
    let hdr = StreamSectionHdr::decode(image).expect("hdr");
    let end = [hdr.frame_table_off, hdr.gcount_table_off].into_iter().find(|&o| o != 0).map_or(image.len(), |o| o as usize);
    let p = &image[STREAM_SECTION_HDR_LEN..end];
    let first = u32::from_le_bytes(p[..4].try_into().unwrap()) as usize;
    let offs: Vec<usize> = (0..first / 4).map(|i| u32::from_le_bytes(p[i * 4..i * 4 + 4].try_into().unwrap()) as usize).collect();
    offs.windows(2).map(|w| &p[w[0]..w[1]]).collect()
}

#[test]
fn intpre_section_unwraps_identically_through_every_face() {
    let b = byte_for(&timestamp_like(4, 0x7135), false);
    let sec = b.section();
    let ends = &b.build.granule_payload_ends;
    let mut wrapped = Vec::new();
    wrap_section(sec, ends, Wrapper::IntPre, &mut wrapped).expect("wrap");
    assert_eq!(StreamSectionHdr::decode(&wrapped).expect("hdr").wrapper, Wrapper::IntPre.as_u8());
    assert!(blocks(&wrapped).iter().all(|b| b[0] == 1), "every timestamp block must pre-pass");
    let mut zstd = Vec::new();
    wrap_section(sec, ends, Wrapper::Zstd, &mut zstd).expect("wrap zstd");
    assert!(wrapped.len() * 2 < zstd.len(), "pre-pass {} vs zstd {}", wrapped.len(), zstd.len());
    assert!(wrapped.len() <= wrap_section_bound(sec, ends, Wrapper::IntPre));

    let mut out = Vec::new();
    unwrap_section(&wrapped, &mut out).expect("unwrap");
    assert_eq!(out, sec, "unwrap_section must rebuild the section byte-exactly");
    let mut out2 = Vec::new();
    assert!(unwrap_section_until(&wrapped, &mut out2, &mut || false).expect("unwrap until"));
    assert_eq!(out2, sec);
    let mut stopped = Vec::new();
    assert!(!unwrap_section_until(&wrapped, &mut stopped, &mut || true).expect("stop"));
    for (blk, raw) in blocks(&wrapped).iter().zip(granule_images(&b)) {
        let mut dst = vec![0u8; raw.len()];
        unwrap_block_into(Wrapper::IntPre, blk, &mut dst).expect("block");
        assert_eq!(dst, raw);
        let mut short = vec![0u8; raw.len() - 1];
        assert!(unwrap_block_into(Wrapper::IntPre, blk, &mut short).is_err(), "a short span refuses");
    }

    let mut again = Vec::new();
    wrap_section(sec, ends, Wrapper::IntPre, &mut again).expect("wrap");
    assert_eq!(again, wrapped, "deterministic bytes");
    let mut reserved = Vec::with_capacity(wrap_section_bound(sec, ends, Wrapper::IntPre));
    let cap = reserved.capacity();
    wrap_section(sec, ends, Wrapper::IntPre, &mut reserved).expect("wrap");
    assert_eq!((reserved.capacity(), &reserved), (cap, &wrapped), "the wrap stays inside its bound");
}

/// Under the adaptive zstd level a section whose whole first block is the sample reuses the sample's frame as block 0;
/// an IntPre block leads with its mode byte, so that reuse is the Zstd arm's only.
#[test]
fn adaptive_level_keeps_intpre_blocks_readable() {
    let b = byte_for(&timestamp_like(3, 0xADA), false);
    let (sec, ends) = (b.section(), &b.build.granule_payload_ends);
    assert!((ends[0] as usize) <= ADAPT_SAMPLE, "block 0 is the whole sample");
    let a = ZstdAdapt { low: 3, high: 9, permille: 0, gated: false };
    let mut ip = Vec::new();
    let level = wrap_section_with(sec, ends, Wrapper::IntPre, &mut ip, None, Some(&a), None).expect("wrap");
    assert!(level == 3 || level == 9);
    assert!(blocks(&ip).iter().all(|b| b[0] <= 1), "every block, the first included, leads with its mode byte");
    let mut out = Vec::new();
    unwrap_section(&ip, &mut out).expect("unwrap");
    assert_eq!(out, sec);
}

#[test]
fn plain_only_streams_settle_to_the_zstd_image() {
    let mut s = 0x0153_u64;
    let noise: Vec<Vec<u64>> = (0..3).map(|_| (0..8192).map(|_| splitmix(&mut s)).collect()).collect();
    let b = byte_for(&noise, false);
    let (sec, ends) = (b.section(), &b.build.granule_payload_ends);
    let mut ip = Vec::new();
    wrap_section(sec, ends, Wrapper::IntPre, &mut ip).expect("wrap");
    let mut zs = Vec::new();
    wrap_section(sec, ends, Wrapper::Zstd, &mut zs).expect("wrap");
    let mut images = vec![ip];
    assert_eq!(settle(Wrapper::IntPre, &mut images).expect("settle"), Wrapper::Zstd);
    assert_eq!(images[0], zs, "a plain-only IntPre image settles to the exact Zstd image");
    let mut out = Vec::new();
    unwrap_section(&images[0], &mut out).expect("unwrap");
    assert_eq!(out, sec);

    let t = byte_for(&timestamp_like(2, 9), false);
    let mut tp = Vec::new();
    wrap_section(t.section(), &t.build.granule_payload_ends, Wrapper::IntPre, &mut tp).expect("wrap");
    let mut ip2 = Vec::new();
    wrap_section(sec, ends, Wrapper::IntPre, &mut ip2).expect("wrap");
    let mut mixed = vec![ip2.clone(), tp.clone()];
    assert_eq!(settle(Wrapper::IntPre, &mut mixed).expect("settle"), Wrapper::IntPre, "one pre-passed band keeps the arm");
    assert_eq!((&mixed[0], &mixed[1]), (&ip2, &tp), "kept images are untouched");
    assert_eq!(settle(Wrapper::Zstd, &mut mixed).expect("settle"), Wrapper::Zstd);
}

#[test]
fn corrupt_intpre_input_refuses_typed() {
    let b = byte_for(&timestamp_like(2, 0xBAD), true);
    let mut wrapped = Vec::new();
    wrap_section(b.section(), &b.build.granule_payload_ends, Wrapper::IntPre, &mut wrapped).expect("wrap");
    let mut s = 0xC0_u64;
    for _ in 0..3000 {
        let mut bad = wrapped.clone();
        let at = STREAM_SECTION_HDR_LEN + (splitmix(&mut s) as usize) % (bad.len() - STREAM_SECTION_HDR_LEN);
        bad[at] ^= 1 << (splitmix(&mut s) % 8);
        let mut out = Vec::new();
        let _ = unwrap_section(&bad, &mut out);
    }
    for cut in [STREAM_SECTION_HDR_LEN + 3, wrapped.len() / 2] {
        assert!(unwrap_section(&wrapped[..cut], &mut Vec::new()).is_err(), "truncated at {cut}");
    }
    // the pre-pass decoder alone, past zstd: every truncation refuses and random flips never panic
    let raw = &granule_images(&b)[0];
    let pre = prepass_image(View::Frames(StreamSectionHdr::decode(b.section()).unwrap().width), raw, 200).expect("enc").expect("engages");
    let mut dst = vec![0u8; raw.len()];
    for cut in 0..pre.len() {
        assert!(prepass_decode(&pre[..cut], &mut dst).is_err(), "pre image cut at {cut}");
    }
    for _ in 0..20_000 {
        let mut bad = pre.clone();
        let at = (splitmix(&mut s) as usize) % bad.len();
        bad[at] = splitmix(&mut s) as u8;
        let _ = prepass_decode(&bad, &mut dst);
    }
    let mut trailing = pre.clone();
    trailing.push(0);
    assert!(matches!(prepass_decode(&trailing, &mut dst), Err(FormatError::Corrupt { .. })));
    for (view, w) in [(9u8, 8u8), (0, 3), (0, 0), (1, 9)] {
        assert!(prepass_decode(&[view, w, 0], &mut dst).is_err(), "view {view} width {w}");
    }
    let blk = blocks(&wrapped)[0];
    for mode in [2u8, 255] {
        let mut m = blk.to_vec();
        m[0] = mode;
        assert!(matches!(unwrap_block_into(Wrapper::IntPre, &m, &mut dst), Err(FormatError::Corrupt { .. })));
    }
    let mut big = blk.to_vec();
    big[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(unwrap_block_into(Wrapper::IntPre, &big, &mut dst).is_err(), "an image length past the span refuses");
    let rle_bad = [3u8, 5, 0, 0, 0];
    assert!(scheme_decode(&rle_bad, 3, 8).is_err(), "more runs than values refuses");
    let mut pfor_bad = vec![1u8];
    pfor_bad.extend_from_slice(&0u64.to_le_bytes());
    pfor_bad.push(9);
    pfor_bad.extend_from_slice(&0u32.to_le_bytes());
    assert!(matches!(scheme_decode(&pfor_bad, 1, 8), Err(FormatError::Corrupt { .. })), "byte width 9 refuses");
    assert!(matches!(scheme_decode(&[7u8], 1, 8), Err(FormatError::Corrupt { .. })), "unknown scheme refuses");
}

/// Microbench (`--ignored`, release): `PGRC_INTPRE_BENCH=<dir>` holds `<column>.<byval width>.txt` files of one integer per
/// line in storage order (`<column>.c.txt`: dictionary codes, sealed as DICT_CODES). Each column is cut into 23-granule
/// streams, elected and encoded as the seal does, and per granule block timed: plain zstd and IntPre wrap / unwrap (MB/s
/// of encoded image, one thread), bytes, and every scheme forced on the block's main sequence (encode / decode MB/s,
/// bytes before zstd).
#[test]
#[ignore]
fn intpre_bench() {
    use crate::election::{elect_int, Election};
    use crate::intpre::view_for;
    use pgrc_format::enc::EncodingId;
    use std::time::Instant;
    let Ok(dir) = std::env::var("PGRC_INTPRE_BENCH") else { return };
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("dir").map(|e| e.expect("entry").path()).collect();
    files.sort();
    println!("column | enc/w | image MB | zstd MB | intpre MB (ratio) | pre-passed blocks | zstd enc/dec MB/s | intpre enc/dec MB/s | main-sequence scheme bytes before zstd (enc/dec MB/s)");
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let mut parts = name.split('.');
        let (col, kind) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
        let codes = kind == "c";
        let bw = if codes { 4 } else { kind.parse::<u8>().unwrap() };
        let vals: Vec<u64> = std::fs::read_to_string(&f).expect("read").lines().map(|l| l.trim().parse::<i64>().expect("int") as u64).collect();
        let class = if codes { StorageClass::VarlenaVerbatim } else { StorageClass::ByvalWord { width: bw, signed: true } };
        let (mut img, mut zb, mut ib, mut pre_blocks, mut blocks_n, mut regret) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        let (mut zt, mut zd, mut it, mut id) = (0f64, 0f64, 0f64, 0f64);
        let mut sch = [(0usize, 0f64, 0f64, 0usize); 5];
        let mut encs = std::collections::BTreeMap::new();
        for chunk in vals.chunks(23 * 8192) {
            let gs: Vec<GranuleData> = chunk.chunks(8192).map(|c| GranuleData { rows: c.len() as u32, datums: c.to_vec(), validity: None }).collect();
            let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
            let b = match codes {
                true => build_stream_noverify(&mut DictCodesEncoder { class: CLASS_VARLENA, max_width: 32, byte_align: false }, &inputs),
                false => match elect_int(&inputs, bw, true, false, false) {
                    Election::Elected { encoding: EncodingId::ByteFor, width, .. } => {
                        build_stream_noverify(&mut ByteForEncoder::new_bytefor(bw, width, true), &inputs)
                    }
                    Election::Elected { encoding: EncodingId::Const, .. } => continue,
                    _ => build_stream_noverify(&mut pgrc_format::verbatim::VerbatimEncoder { class }, &inputs),
                },
            };
            let hdr = StreamSectionHdr::decode(b.section()).expect("hdr");
            *encs.entry(format!("{}/w{}", hdr.encoding, hdr.width)).or_insert(0usize) += 1;
            let view = view_for(hdr.encoding, hdr.width);
            let mut trial = crate::intpre::Trial::default();
            for (bi, raw) in granule_images(&b).into_iter().enumerate() {
                if bi % 8 == 0 {
                    trial = crate::intpre::Trial::default();
                }
                img += raw.len();
                blocks_n += 1;
                let t = Instant::now();
                let mut z = Vec::new();
                crate::wrapper::zstd_compress_block(&raw, &mut z).expect("zstd");
                zt += t.elapsed().as_secs_f64();
                zb += z.len();
                let mut dst = vec![0u8; raw.len()];
                let t = Instant::now();
                crate::wrapper::zstd_decompress_block(&z, &mut dst).expect("unzstd");
                zd += t.elapsed().as_secs_f64();
                let t = Instant::now();
                let mut w = Vec::new();
                let used = crate::intpre::wrap_block(view, &raw, &mut w, &mut trial, crate::wrapper::zstd_level()).expect("wrap");
                it += t.elapsed().as_secs_f64();
                ib += w.len();
                pre_blocks += used as usize;
                regret += w.len().saturating_sub(1 + z.len());
                let t = Instant::now();
                crate::intpre::unwrap_block(&w, &mut dst).expect("unwrap");
                id += t.elapsed().as_secs_f64();
                assert_eq!(dst, raw);
                let main = main_sequence(view, &raw);
                for (s, slot) in sch.iter_mut().enumerate().filter(|_| !main.is_empty()) {
                    let t = Instant::now();
                    let Some(im) = scheme_image(s as u8, &main).expect("scheme") else { continue };
                    slot.1 += t.elapsed().as_secs_f64();
                    let t = Instant::now();
                    let back = scheme_decode(&im, main.len(), 8).expect("decode");
                    slot.2 += t.elapsed().as_secs_f64();
                    assert_eq!(back, main);
                    slot.0 += raw.len();
                    slot.3 += im.len();
                }
            }
        }
        let mb = |b: usize| b as f64 / 1e6;
        let per = sch
            .iter()
            .enumerate()
            .filter(|(_, s)| s.0 > 0)
            .map(|(i, s)| format!("{}: {:.1} MB ({:.0}/{:.0})", ["plain", "pfor", "delta", "rle", "linear"][i], mb(s.3), mb(s.0) / s.1, mb(s.0) / s.2))
            .collect::<Vec<_>>()
            .join("; ");
        println!(
            "{col} | {encs:?} | {:.1} | {:.1} | {:.1} ({:.3}) | {pre_blocks}/{blocks_n} (regret {regret} B) | {:.0}/{:.0} | {:.0}/{:.0} | {per}",
            mb(img), mb(zb), mb(ib), ib as f64 / zb as f64, mb(img) / zt, mb(img) / zd, mb(img) / it, mb(img) / id
        );
    }
}

fn build_stream_noverify(enc: &mut dyn pgrc_format::abi::GranuleEncoder, inputs: &[EncodeInput<'_>]) -> Built {
    super::build_stream_opts(enc, inputs, 0, true, false)
}

/// The block's main sequence under `view` (the words, BYTE_FOR deltas, DELTA_FOR zigzag deltas or codes), as u64.
fn main_sequence(view: Option<View>, raw: &[u8]) -> Vec<u64> {
    let le = |b: &[u8]| {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    match view {
        Some(View::Words(w)) => raw.chunks_exact(w as usize).map(le).collect(),
        Some(View::Frames(w)) => {
            let w = w as usize;
            let mut out = Vec::new();
            let mut pos = 0;
            while pos < raw.len() {
                let cnt = ((raw.len() - pos - 8) / w).min(1024);
                out.extend(raw[pos + 8..pos + 8 + cnt * w].chunks_exact(w).map(le));
                pos += 8 + cnt * w;
            }
            out
        }
        Some(View::Codes) if raw[4] > 0 => {
            let (width, packed) = (raw[4] as usize, &raw[8..]);
            (0..packed.len() * 8 / width)
                .map(|i| {
                    let bit = i * width;
                    let end = (bit / 8 + 8).min(packed.len());
                    (le(&packed[bit / 8..end]) >> (bit % 8)) & ((1u64 << width) - 1)
                })
                .collect()
        }
        _ => Vec::new(),
    }
}
