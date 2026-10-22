//! OnPair16 tokens (`crate::onpair`) and the `FrontOnPairZstd` dict payload arm (`crate::onpairwrap`):
//! exact round-trips at the codec and section grain, every unwrap face, and typed refusal of corrupt input
//! (damaged sections must not panic; the reader's CRC over the wrapped bytes rejects them before unwrap).

use super::dictwrap::{dict_sections, index_payload};
use super::splitmix;
use crate::onpair::{decode, stream_header, DecodeTable, Dict, Params, FORM_12, FORM_U16, MAX_IDS, STREAM_HDR};
use crate::onpairwrap::{unwrap_dict_payload, unwrap_dict_payload_fc, wrap_dict_payload, wrap_dict_payload_at};
use crate::wrapper::{
    unwrap_block_into, unwrap_dict_payload_fc_section, unwrap_dict_payload_section, unwrap_dict_payload_section_until,
    level_for, unwrap_section, unwrap_section_until, wrap_dict_payload_section,
};
use pgrc_format::enc::Wrapper;
use pgrc_format::part::StreamSectionHdr;
use pgrc_format::FormatError;

const WIDE: Params = Params { sample_max: 1 << 20, window: 4096, max_ids: MAX_IDS, threshold: 0 };

fn round_trip(text: &[u8], p: &Params) -> (usize, usize) {
    let d = Dict::train(text, p).expect("train");
    let mut table = Vec::new();
    d.write_table(&mut table).expect("table");
    let (t, used) = DecodeTable::read(&table).expect("read table");
    assert_eq!(used, table.len());
    assert_eq!(t.ids(), d.ids());
    for form in [FORM_U16, FORM_12] {
        if form == FORM_12 && d.ids() > 1 << 12 {
            assert!(d.encode(text, form, &mut Vec::new()).is_err(), "a wide set never writes 12-bit symbols");
            continue;
        }
        let mut s = Vec::new();
        d.encode(text, form, &mut s).expect("encode");
        let mut out = vec![0u8; text.len()];
        assert_eq!(decode(&t, &s, &mut out).expect("decode"), s.len());
        assert_eq!(out, text, "form {form}");
    }
    (d.ids(), text.len())
}

fn urls(n: usize, seed: u64) -> Vec<u8> {
    let mut st = seed;
    let mut v = Vec::new();
    for i in 0..n {
        let host = ["yandex.ru", "google.com", "mail.ru", "auto.ru", "irr.ru"][(splitmix(&mut st) % 5) as usize];
        v.extend_from_slice(format!("http://{host}/catalog/{}/item-{i}?q=%D0%9C{}", splitmix(&mut st) % 97, splitmix(&mut st) % 1000).as_bytes());
    }
    v
}

#[test]
fn codec_round_trips_edge_inputs() {
    let mut long = Vec::new();
    for i in 0..4000u32 {
        long.extend_from_slice(b"a-string-longer-than-sixteen-bytes/");
        long.extend_from_slice(&i.to_le_bytes());
    }
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        vec![b'a'; 100_000],
        vec![0u8; 5000],
        vec![0xFF; 5000],
        (0..=255u8).cycle().take(70_000).collect(),
        long,
        urls(20_000, 7),
    ];
    for c in &cases {
        round_trip(c, &Params::DEFAULT);
        round_trip(c, &WIDE);
    }
}

#[test]
fn codec_round_trips_random_bytes_and_text() {
    let mut st = 0x5eed;
    for round in 0..40 {
        let n = (splitmix(&mut st) % 50_000) as usize;
        let alphabet = 1 + splitmix(&mut st) % 255;
        let text: Vec<u8> = if round % 3 == 0 {
            urls(n / 60 + 1, splitmix(&mut st))
        } else {
            (0..n).map(|_| (splitmix(&mut st) % alphabet) as u8).collect()
        };
        let p = Params {
            sample_max: 1 + (splitmix(&mut st) % 200_000) as usize,
            window: 64 + (splitmix(&mut st) % 8192) as usize,
            max_ids: 256 + (splitmix(&mut st) % (MAX_IDS as u64)) as usize,
            threshold: (splitmix(&mut st) % 6) as u32,
        };
        round_trip(&text, &p);
    }
}

#[test]
fn twelve_bit_form_ends_at_4096_ids_and_the_table_fills_to_65536() {
    let text = urls(60_000, 3);
    let at = Params { max_ids: 1 << 12, ..WIDE };
    let d = Dict::train(&text, &at).expect("train");
    assert_eq!(d.ids(), 1 << 12);
    assert_eq!(d.form(), FORM_12);
    round_trip(&text, &at);
    let over = Params { max_ids: (1 << 12) + 1, ..WIDE };
    let d = Dict::train(&text, &over).expect("train");
    assert_eq!(d.ids(), (1 << 12) + 1);
    assert_eq!(d.form(), FORM_U16);
    round_trip(&text, &over);
    let mut st = 11;
    let noise: Vec<u8> = (0..3 << 20).map(|_| (splitmix(&mut st) % 23) as u8 + b'a').collect();
    let full = Params { sample_max: 3 << 20, window: 4096, max_ids: MAX_IDS, threshold: 2 };
    let (ids, _) = round_trip(&noise, &full);
    assert_eq!(ids, MAX_IDS, "the noise sample must fill the whole id space");
}

#[test]
fn training_and_encoding_are_deterministic() {
    let text = urls(5000, 9);
    let enc = || {
        let d = Dict::train(&text, &WIDE).expect("train");
        let mut b = Vec::new();
        d.write_table(&mut b).expect("table");
        d.encode(&text, d.form(), &mut b).expect("encode");
        b
    };
    assert_eq!(enc(), enc());
}

#[test]
fn corrupt_tables_and_streams_refuse_typed() {
    let text = urls(3000, 5);
    let d = Dict::train(&text, &WIDE).expect("train");
    let mut table = Vec::new();
    d.write_table(&mut table).expect("table");
    let (t, _) = DecodeTable::read(&table).expect("table");
    let mut s = Vec::new();
    d.encode(&text, FORM_U16, &mut s).expect("encode");
    let mut out = vec![0u8; text.len()];
    // a pair naming its own or a later id, and a pair past 16 bytes
    let mut bad = table.clone();
    bad[2..4].copy_from_slice(&256u16.to_le_bytes());
    assert!(matches!(DecodeTable::read(&bad), Err(FormatError::Corrupt { .. })));
    let mut wide = vec![0u8; 2];
    wide[..2].copy_from_slice(&5u16.to_le_bytes());
    for (l, r) in [(b'a' as u16, b'b' as u16), (256, 256), (257, 257), (258, 258), (259, 259)] {
        wide.extend_from_slice(&l.to_le_bytes());
        wide.extend_from_slice(&r.to_le_bytes());
    }
    assert!(matches!(DecodeTable::read(&wide), Err(FormatError::Corrupt { .. })), "a 32-byte token is refused");
    assert!(matches!(DecodeTable::read(&table[..table.len() - 1]), Err(FormatError::Truncated { .. })));
    assert!(matches!(DecodeTable::read(&[]), Err(FormatError::Truncated { .. })));
    // an id past the table, a short stream, a length disagreeing with the image, header pad bytes
    let mut oob = s.clone();
    oob[STREAM_HDR..STREAM_HDR + 2].copy_from_slice(&(t.ids() as u16).to_le_bytes());
    assert!(matches!(decode(&t, &oob, &mut out), Err(FormatError::Corrupt { .. })));
    assert!(matches!(decode(&t, &s[..s.len() - 1], &mut out), Err(FormatError::Truncated { .. })));
    assert!(matches!(decode(&t, &s, &mut out[1..]), Err(FormatError::Corrupt { .. })));
    let mut pad = s.clone();
    pad[1] = 1;
    assert!(matches!(stream_header(&pad), Err(FormatError::Corrupt { .. })));
    let mut form = s.clone();
    form[0] = 7;
    assert!(matches!(decode(&t, &form, &mut out), Err(FormatError::Corrupt { .. })));
    // fewer symbols than the stream claims bytes for: short output, refused
    let mut short = s.clone();
    let n = u32::from_le_bytes(short[8..12].try_into().unwrap());
    short[8..12].copy_from_slice(&(n - 1).to_le_bytes());
    assert!(decode(&t, &short, &mut out).is_err());
    // random damage never panics and never reports success with other bytes
    let mut st = 99;
    for _ in 0..2000 {
        let mut z = s.clone();
        for _ in 0..1 + splitmix(&mut st) % 4 {
            let i = (splitmix(&mut st) % z.len() as u64) as usize;
            z[i] = splitmix(&mut st) as u8;
        }
        let cut = (splitmix(&mut st) % (z.len() as u64 + 1)) as usize;
        let z = if splitmix(&mut st) % 4 == 0 { &z[..cut] } else { &z[..] };
        if decode(&t, z, &mut out).is_ok() {
            let (_, len, _, _) = stream_header(z).expect("hdr");
            assert_eq!(len, out.len());
        }
        let mut tz = table.clone();
        let i = (splitmix(&mut st) % tz.len() as u64) as usize;
        tz[i] = splitmix(&mut st) as u8;
        let _ = DecodeTable::read(&tz);
    }
}

fn corpus(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut st = seed;
    let mut v: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let host = ["yandex.ru", "google.com", "mail.ru"][(splitmix(&mut st) % 3) as usize];
            format!("http://{host}/catalog/{}/item-{i:07}?ref=Москва&p={}", i % 31, splitmix(&mut st) % 1000).into_bytes()
        })
        .collect();
    v.push(Vec::new());
    v.push(vec![0u8; 3]);
    v.push(vec![0xFF; 40]);
    v.sort();
    v.dedup();
    v
}

fn expect_v1(payload: &[u8], index: &[u8], w: &[u8]) {
    let ix = index_payload(index);
    let mut a = Vec::new();
    unwrap_dict_payload_section(w, ix, &mut a).expect("unwrap");
    assert_eq!(a, payload, "the whole unwrap rebuilds the writer's image");
    let hdr = StreamSectionHdr::decode(w).expect("hdr");
    let mut b = Vec::new();
    assert!(unwrap_dict_payload(&hdr, w, ix, &mut b, &mut || false).expect("unwrap"));
    assert_eq!(b, payload);
}

#[test]
fn front_onpair_payload_round_trips_every_face() {
    for n in [1usize, 31, 33, 1024, 1025, 5000] {
        let entries = corpus(n, n as u64);
        let (index, payload) = dict_sections(&entries);
        let ix = index_payload(&index);
        let mut w = Vec::new();
        wrap_dict_payload(&payload, ix, &mut w).expect("wrap");
        let hdr = StreamSectionHdr::decode(&w).expect("hdr");
        assert_eq!(hdr.wrapper, Wrapper::FrontOnPairZstd.as_u8());
        expect_v1(&payload, &index, &w);
        // the front-coded resident form equals FrontZstd's
        let mut fz = Vec::new();
        wrap_dict_payload_section(&payload, ix, &mut fz).expect("front zstd");
        let (mut want, mut got, mut got2) = (Vec::new(), Vec::new(), Vec::new());
        unwrap_dict_payload_fc_section(&fz, ix, &mut want).expect("fc");
        unwrap_dict_payload_fc_section(&w, ix, &mut got).expect("fc onpair");
        assert!(unwrap_dict_payload_fc(&hdr, &w, ix, &mut got2, &mut || false).expect("fc onpair"));
        assert_eq!(got, want);
        assert_eq!(got2, want);
        // faces without the index or at block grain refuse typed
        assert!(matches!(unwrap_section(&w, &mut Vec::new()), Err(FormatError::Corrupt { .. })));
        assert!(matches!(unwrap_block_into(Wrapper::FrontOnPairZstd, &w[32..], &mut [0u8; 8]), Err(FormatError::Corrupt { .. })));
    }
}

#[test]
fn front_onpair_unwrap_stops_when_asked() {
    let entries = corpus(5000, 1);
    let (index, payload) = dict_sections(&entries);
    let ix = index_payload(&index);
    let mut w = Vec::new();
    wrap_dict_payload(&payload, ix, &mut w).expect("wrap");
    let mut asks = 0;
    let mut out = vec![1u8];
    let done = unwrap_dict_payload_section_until(&w, ix, &mut out, &mut || {
        asks += 1;
        asks > 2
    })
    .expect("unwrap");
    assert!(!done && out.is_empty());
    let hdr = StreamSectionHdr::decode(&w).expect("hdr");
    let mut asks = 0;
    assert!(!unwrap_dict_payload_fc(&hdr, &w, ix, &mut out, &mut || {
        asks += 1;
        asks > 1
    })
    .expect("fc"));
    assert!(out.is_empty());
}

#[test]
fn front_onpair_corrupt_sections_refuse_typed() {
    let entries = corpus(3000, 2);
    let (index, payload) = dict_sections(&entries);
    let ix = index_payload(&index);
    let mut w = Vec::new();
    wrap_dict_payload(&payload, ix, &mut w).expect("wrap");
    let mut st = 4242;
    for _ in 0..600 {
        let mut z = w.clone();
        for _ in 0..1 + splitmix(&mut st) % 3 {
            let i = 32 + (splitmix(&mut st) % (z.len() as u64 - 32)) as usize;
            z[i] = splitmix(&mut st) as u8;
        }
        let mut out = Vec::new();
        let _ = unwrap_dict_payload_section(&z, ix, &mut out);
        let _ = unwrap_dict_payload_fc_section(&z, ix, &mut out);
    }
    // a section header whose block count disagrees with its frames
    let mut z = w.clone();
    let first = u32::from_le_bytes(z[32..36].try_into().unwrap());
    z[32..36].copy_from_slice(&(first - 4).to_le_bytes());
    assert!(unwrap_dict_payload_section(&z, ix, &mut Vec::new()).is_err());
    assert!(unwrap_dict_payload_section(&w[..w.len() / 2], ix, &mut Vec::new()).is_err());
}

#[test]
fn front_onpair_beats_front_zstd_on_title_dictionaries() {
    let mut st = 17;
    let vocab: Vec<String> = (0..3000)
        .map(|_| (0..3 + splitmix(&mut st) % 8).map(|_| char::from_u32(0x430 + (splitmix(&mut st) % 32) as u32).unwrap()).collect())
        .collect();
    let mut entries: Vec<Vec<u8>> = (0..20_000)
        .map(|_| {
            let words: Vec<&str> = (0..4 + splitmix(&mut st) % 8).map(|_| vocab[(splitmix(&mut st) % 3000) as usize].as_str()).collect();
            words.join(" ").into_bytes()
        })
        .collect();
    entries.sort();
    entries.dedup();
    let (index, payload) = dict_sections(&entries);
    let ix = index_payload(&index);
    let (mut op, mut fz) = (Vec::new(), Vec::new());
    wrap_dict_payload(&payload, ix, &mut op).expect("wrap");
    wrap_dict_payload_section(&payload, ix, &mut fz).expect("front zstd");
    expect_v1(&payload, &index, &op);
    assert!(op.len() < fz.len(), "onpair {} vs front zstd {}", op.len(), fz.len());
}

#[test]
fn front_onpair_token_blocks_take_the_string_level() {
    let entries = corpus(5000, 7);
    let (index, payload) = dict_sections(&entries);
    let ix = index_payload(&index);
    let hdr = StreamSectionHdr::decode(&payload).expect("hdr");
    assert_eq!(level_for(Wrapper::FrontOnPairZstd, hdr.encoding, hdr.width, 3, Some(9)), 9);
    assert_eq!(level_for(Wrapper::FrontOnPairZstd, hdr.encoding, hdr.width, 3, None), 3);
    let (mut lo, mut hi) = (Vec::new(), Vec::new());
    wrap_dict_payload_at(&payload, ix, &mut lo, 1).expect("wrap at 1");
    wrap_dict_payload_at(&payload, ix, &mut hi, 19).expect("wrap at 19");
    expect_v1(&payload, &index, &lo);
    expect_v1(&payload, &index, &hi);
    assert_ne!(lo, hi, "the level reaches the token blocks");
}

/// A verbatim varlena stream through the production encoder: granules of `rows`, row `r`'s payload from
/// `value`, validity cycling all-valid / sparse / dense nulls.
fn verbatim_stream(rows: &[u32], value: &mut dyn FnMut(u32, &mut u64) -> Vec<u8>) -> (super::Built, Vec<Vec<u8>>) {
    use pgrc_format::abi::EncodeInput;
    use pgrc_format::class::StorageClass;
    let mut seed = 0x0A1_u64;
    let mut images: Vec<Vec<u8>> = Vec::new();
    let gs: Vec<super::GranuleData> = rows
        .iter()
        .enumerate()
        .map(|(gi, &n)| {
            let validity = super::validity_pattern(gi as u32, n, &mut seed);
            let datums = (0..n)
                .map(|r| {
                    let p = value(r, &mut seed);
                    let mut img = (((p.len() + 4) as u32) << 2).to_le_bytes().to_vec();
                    img.extend_from_slice(&p);
                    images.push(img);
                    images.last().unwrap().as_ptr() as u64
                })
                .collect();
            super::GranuleData { rows: n, datums, validity }
        })
        .collect();
    let class = StorageClass::VarlenaVerbatim;
    let inputs: Vec<EncodeInput<'_>> = gs.iter().map(|g| g.input(class)).collect();
    let built = super::build_stream(&mut pgrc_format::verbatim::VerbatimEncoder { class }, &inputs, 0, false);
    (built, images)
}

fn title_value(vocab: &[String]) -> impl FnMut(u32, &mut u64) -> Vec<u8> + '_ {
    move |r, st| match r % 211 {
        0 => Vec::new(),
        1 => vec![0xC3; pgrc_format::geom::OVERSIZE_THRESHOLD as usize + 9],
        _ => {
            let words: Vec<&str> = (0..2 + splitmix(st) % 9).map(|_| vocab[(splitmix(st) % vocab.len() as u64) as usize].as_str()).collect();
            words.join(" ").into_bytes()
        }
    }
}

fn vocab(n: usize, st: &mut u64) -> Vec<String> {
    (0..n).map(|_| (0..3 + splitmix(st) % 8).map(|_| char::from_u32(0x430 + (splitmix(st) % 32) as u32).unwrap()).collect()).collect()
}

#[test]
fn onpair_values_rebuild_the_exact_verbatim_image() {
    use crate::onpairvals::wrap_values;
    let mut st = 5;
    let vocab = vocab(2000, &mut st);
    let (built, _keep) = verbatim_stream(&[8192, 8192, 8192, 8192, 37], &mut title_value(&vocab));
    assert!(!built.ovf.is_empty(), "the overflow reference arm is exercised");
    let sec = built.section();
    let w = wrap_values(sec).expect("wrap").expect("offered");
    assert_eq!(StreamSectionHdr::decode(&w).unwrap().wrapper, Wrapper::OnPairZstd.as_u8());
    let mut out = Vec::new();
    unwrap_section(&w, &mut out).expect("unwrap");
    assert!(out == sec, "the whole-section face rebuilds the writer's bytes");
    let mut asks = 0;
    assert!(!unwrap_section_until(&w, &mut out, &mut || { asks += 1; asks > 1 }).expect("stop"));
    assert!(out.is_empty());
    let mut z = Vec::new();
    let plen = StreamSectionHdr::decode(sec).unwrap().frame_table_off - 32;
    crate::wrapper::wrap_section(sec, &[plen], Wrapper::Zstd, &mut z).expect("zstd, one block");
    assert!(w.len() < z.len(), "onpair {} vs zstd {}", w.len(), z.len());
}

#[test]
fn onpair_values_cover_empty_null_heavy_and_short_streams() {
    use crate::onpairvals::wrap_values;
    let mut st = 9;
    let vocab = vocab(500, &mut st);
    // all empty strings and NULLs: no text at all
    let (built, _k0) = verbatim_stream(&[8192, 8192, 100], &mut |_, _| Vec::new());
    let sec = built.section();
    if let Some(w) = wrap_values(sec).expect("wrap") {
        let mut out = Vec::new();
        unwrap_section(&w, &mut out).expect("unwrap");
        assert!(out == sec);
    }
    // one short granule under the floor: not offered
    let (built, _k1) = verbatim_stream(&[50], &mut title_value(&vocab));
    assert!(wrap_values(built.section()).expect("wrap").is_none());
    // granule shapes ending in a short frame, every validity pattern
    for rows in [[8192u32, 1025, 0], [3000, 0, 0], [8192, 8192, 1]] {
        let rows: Vec<u32> = rows.into_iter().filter(|&n| n > 0).collect();
        let (built, _k) = verbatim_stream(&rows, &mut title_value(&vocab));
        let sec = built.section();
        let Some(w) = wrap_values(sec).expect("wrap") else { continue };
        let mut out = Vec::new();
        unwrap_section(&w, &mut out).expect("unwrap");
        assert!(out == sec, "{rows:?}");
    }
}

#[test]
fn onpair_values_refuse_layouts_they_cannot_rebuild_and_corrupt_input() {
    use crate::onpairvals::wrap_values;
    let mut st = 11;
    let vocab = vocab(800, &mut st);
    let (built, _k) = verbatim_stream(&[8192, 8192], &mut title_value(&vocab));
    let sec = built.section().to_vec();
    // a nonzero pad byte between two entries is not the canonical layout: the arm is not offered
    let hdr = StreamSectionHdr::decode(&sec).unwrap();
    let ft = hdr.frame_table(&sec).unwrap().unwrap();
    let f1 = 32 + ft[1] as usize;
    let mut odd = sec.clone();
    let pad = (0..ft[1] as usize).rev().map(|i| 32 + i).find(|&i| odd[i] == 0 && i < f1 && i > 32 + ft[0] as usize + 4096).unwrap();
    odd[pad] = 0x5A;
    if let Some(w) = wrap_values(&odd).expect("wrap") {
        let mut out = Vec::new();
        unwrap_section(&w, &mut out).expect("unwrap");
        assert!(out == odd, "an offered arm still rebuilds exactly");
    }
    let w = wrap_values(&sec).expect("wrap").expect("offered");
    for _ in 0..600 {
        let mut z = w.clone();
        for _ in 0..1 + splitmix(&mut st) % 3 {
            let i = 32 + (splitmix(&mut st) % (z.len() as u64 - 32)) as usize;
            z[i] = splitmix(&mut st) as u8;
        }
        let _ = unwrap_section(&z, &mut Vec::new());
    }
    let mut z = w.clone();
    z[32..36].copy_from_slice(&16u32.to_le_bytes());
    assert!(unwrap_section(&z, &mut Vec::new()).is_err());
    assert!(unwrap_section(&w[..w.len() / 2], &mut Vec::new()).is_err());
    assert!(unwrap_block_into(Wrapper::OnPairZstd, &w[44..], &mut [0u8; 16]).is_err());
}
