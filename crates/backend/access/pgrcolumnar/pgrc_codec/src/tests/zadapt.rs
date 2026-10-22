#![cfg(not(target_family = "wasm"))]

use crate::wrapper::{
    is_string_data, level_for, parse_level, unwrap_section, wrap_section_with, zstd_compress_block_at, zstd_level, ZstdAdapt,
    ADAPT_MIN, ADAPT_SAMPLE,
};
use pgrc_format::enc::Wrapper;
use pgrc_format::part::{STREAM_SECTION_HDR_LEN, STREAM_SECTION_MAGIC};

fn mix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn words(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    let vocab: Vec<Vec<u8>> = (0..400).map(|_| (0..3 + mix(&mut s) % 12).map(|_| b'a' + (mix(&mut s) % 26) as u8).collect()).collect();
    let sentences: Vec<Vec<u8>> = (0..300).map(|_| (0..4 + mix(&mut s) % 10).flat_map(|_| [&vocab[(mix(&mut s) % 400) as usize][..], b" "].concat()).collect()).collect();
    let mut out = Vec::with_capacity(n + 256);
    while out.len() < n {
        out.extend_from_slice(&sentences[(mix(&mut s) % 300) as usize]);
        out.extend_from_slice(format!("/{};", mix(&mut s) % 100_000).as_bytes());
    }
    out.truncate(n);
    out
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n).map(|_| mix(&mut s) as u8).collect()
}

fn section(payload: &[u8]) -> Vec<u8> {
    section_w(payload, 1)
}

fn section_w(payload: &[u8], width: u8) -> Vec<u8> {
    let mut s = Vec::new();
    s.extend_from_slice(&STREAM_SECTION_MAGIC.to_le_bytes());
    s.extend_from_slice(&0u16.to_le_bytes());
    s.push(width);
    s.push(0);
    s.extend_from_slice(&[0u8; 24]);
    assert_eq!(s.len(), STREAM_SECTION_HDR_LEN);
    s.extend_from_slice(payload);
    s
}

fn ends(payload: usize, block: usize) -> Vec<u32> {
    let mut e: Vec<u32> = (1..).map(|i| (i * block).min(payload) as u32).take_while(|&x| (x as usize) < payload).collect();
    e.push(payload as u32);
    e
}

fn blocks(wrapped: &[u8], n: usize) -> Vec<&[u8]> {
    let r = &wrapped[STREAM_SECTION_HDR_LEN..];
    let off = |i: usize| u32::from_le_bytes(r[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
    (0..n).map(|i| &r[off(i)..off(i + 1)]).collect()
}

fn wrap(sec: &[u8], e: &[u32], a: Option<&ZstdAdapt>) -> (Vec<u8>, i32) {
    let mut out = Vec::new();
    let lv = wrap_section_with(sec, e, Wrapper::Zstd, &mut out, None, a, None).expect("wrap");
    (out, lv)
}

fn assert_blocks_at(sec: &[u8], e: &[u32], wrapped: &[u8], level: i32) {
    let payload = &sec[STREAM_SECTION_HDR_LEN..];
    let mut g0 = 0;
    for (b, (blk, &end)) in blocks(wrapped, e.len()).into_iter().zip(e).enumerate() {
        let fresh = zstd::bulk::compress(&payload[g0..end as usize], level).expect("fresh");
        assert_eq!(blk, &fresh[..], "block {b} is not a level-{level} frame");
        g0 = end as usize;
    }
    let mut scratch = Vec::new();
    unwrap_section(wrapped, &mut scratch).expect("unwrap");
    assert_eq!(scratch, sec, "the adaptive wrap does not unwrap to the section");
}

const A: ZstdAdapt = ZstdAdapt { low: 3, high: 9, permille: 0, gated: false };

#[test]
fn adaptive_choice_is_a_function_of_the_input() {
    let p = words(6 * 32 * 1024, 1);
    let (sec, e) = (section(&p), ends(p.len(), 32 * 1024));
    let (w1, l1) = wrap(&sec, &e, Some(&A));
    let (w2, l2) = wrap(&sec, &e, Some(&A));
    assert_eq!((l1, &w1), (l2, &w2), "same input, other bytes");
    assert_eq!(l1, 9, "text with permille 0 keeps the higher level");
    assert_blocks_at(&sec, &e, &w1, l1);
}

#[test]
fn adaptive_picks_high_only_past_the_threshold() {
    let p = words(4 * 40 * 1024, 2);
    let (sec, e) = (section(&p), ends(p.len(), 40 * 1024));
    let never = ZstdAdapt { permille: 1000, ..A };
    let (w, l) = wrap(&sec, &e, Some(&never));
    assert_eq!(l, 3);
    assert_blocks_at(&sec, &e, &w, 3);
    let (w, l) = wrap(&sec, &e, Some(&A));
    assert_eq!(l, 9);
    assert_blocks_at(&sec, &e, &w, 9);
}

#[test]
fn adaptive_tiny_section_takes_low_unsampled() {
    let p = words(ADAPT_MIN - 1, 3);
    let (sec, e) = (section(&p), ends(p.len(), 4096));
    let (w, l) = wrap(&sec, &e, Some(&A));
    assert_eq!(l, 3, "a section under ADAPT_MIN is never sampled");
    assert_blocks_at(&sec, &e, &w, 3);
}

#[test]
fn adaptive_single_block_past_the_sample_cut() {
    let p = words(3 * ADAPT_SAMPLE + 777, 4);
    let (sec, e) = (section(&p), vec![p.len() as u32]);
    let (w1, l1) = wrap(&sec, &e, Some(&A));
    let (w2, _) = wrap(&sec, &e, Some(&A));
    assert_eq!(w1, w2);
    assert_eq!(l1, 9);
    assert_blocks_at(&sec, &e, &w1, l1);
}

#[test]
fn adaptive_gated_keeps_low_when_the_sample_misses_the_wrapper_gate() {
    let p = noise(3 * 32 * 1024, 5);
    let (sec, e) = (section(&p), ends(p.len(), 32 * 1024));
    let (w, l) = wrap(&sec, &e, Some(&ZstdAdapt { gated: true, ..A }));
    assert_eq!(l, 3);
    assert_blocks_at(&sec, &e, &w, 3);
}

#[test]
fn high_wins_edge_cases() {
    assert!(!A.high_wins(1000, 500, 500), "a tie keeps low");
    assert!(!A.high_wins(0, 9, 9), "an empty sample keeps low");
    assert!(A.high_wins(1000, 500, 499));
    let t = ZstdAdapt { permille: 20, ..A };
    assert!(t.high_wins(1000, 500, 480) && !t.high_wins(1000, 500, 481), "20 per mille of 1000 = 20 bytes");
    assert!(!A.high_wins(1000, 900, 850), "unwrapped at both levels saves nothing on disk");
    let flip = ZstdAdapt { permille: 200, ..A };
    assert!(flip.high_wins(1000, 850, 790), "the gate flips at high: 1000 -> 790 on disk");
    let g = ZstdAdapt { gated: true, ..A };
    assert!(!g.high_wins(1000, 850, 790), "gated: low must clear the gate itself");
    assert!(g.high_wins(1000, 800, 700) && !g.high_wins(1000, 801, 700), "the gate is 5 x low <= 4 x base");
}

#[test]
fn adaptive_transformed_first_block_is_reused_verbatim() {
    let p = words(5 * 20 * 1024, 6);
    let (sec, e) = (section(&p), ends(p.len(), 20 * 1024));
    let mut out = Vec::new();
    let mut ident = |_b: usize, raw: &[u8], blk: &mut Vec<u8>| -> pgrc_format::FormatResult<()> {
        blk.extend_from_slice(raw);
        Ok(())
    };
    let l = wrap_section_with(&sec, &e, Wrapper::Zstd, &mut out, Some(&mut ident), Some(&A), None).expect("wrap");
    let (plain, pl) = wrap(&sec, &e, Some(&A));
    assert_eq!((l, out), (pl, plain), "an identity transform changes the adaptive wrap");
}

#[test]
fn kept_contexts_switch_levels_like_fresh_ones() {
    let inputs = [words(70_000, 7), noise(20_000, 8), words(3000, 9), Vec::new()];
    for (k, level) in [3, 9, 1, 9, 3, 12, 3].into_iter().enumerate() {
        for (i, input) in inputs.iter().enumerate() {
            let mut out = b"x".to_vec();
            zstd_compress_block_at(input, &mut out, level).expect("pooled");
            assert_eq!(&out[1..], &zstd::bulk::compress(input, level).expect("fresh")[..], "step {k} input {i} level {level}");
        }
    }
}

#[test]
fn text_level_switch_parses_one_to_twenty_two() {
    assert_eq!(parse_level(Some("9")), Some(9));
    assert_eq!(parse_level(Some(" 12 ")), Some(12));
    for bad in ["0", "23", "-1", "nine", ""] {
        assert_eq!(parse_level(Some(bad)), None, "{bad:?}");
    }
    assert_eq!(parse_level(None), None);
}

#[test]
fn text_level_applies_to_string_data_only() {
    assert!(is_string_data(Wrapper::FrontZstd, 8, 0), "a dictionary payload");
    assert!(is_string_data(Wrapper::Zstd, 0, 0), "varlena Verbatim values");
    assert!(!is_string_data(Wrapper::Zstd, 0, 8), "fixed-width Verbatim values");
    assert!(!is_string_data(Wrapper::Zstd, 8, 2), "dictionary codes");
    assert!(!is_string_data(Wrapper::PackedZstd, 8, 0), "the dictionary index");
    assert_eq!(level_for(Wrapper::FrontZstd, 8, 0, 3, Some(9)), 9);
    assert_eq!(level_for(Wrapper::Zstd, 0, 0, 3, Some(9)), 9);
    assert_eq!(level_for(Wrapper::Zstd, 4, 8, 3, Some(9)), 3);
    assert_eq!(level_for(Wrapper::FrontZstd, 8, 0, 3, None), 3, "unset keeps the base level");
}

#[test]
fn text_level_wraps_string_sections_and_skips_the_adaptive_choice() {
    let p = words(5 * 20 * 1024, 10);
    let e = ends(p.len(), 20 * 1024);
    let (s, n) = (section_w(&p, 0), section_w(&p, 8));
    let mut out = Vec::new();
    let l = wrap_section_with(&s, &e, Wrapper::Zstd, &mut out, None, Some(&A), Some(1)).expect("wrap");
    assert_eq!(l, 1, "string data takes the text level, not the adaptive one");
    assert_blocks_at(&s, &e, &out, 1);
    out.clear();
    let l = wrap_section_with(&n, &e, Wrapper::Zstd, &mut out, None, None, Some(1)).expect("wrap");
    assert_eq!(l, zstd_level(), "other data keeps the base level");
    assert_blocks_at(&n, &e, &out, l);
}
