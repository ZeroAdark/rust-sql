//! Bar B-rev at the catalog's level (ROWMAP2 M2, ROWMAP2.done): warm `RowMap`s of 1M-row parts, per form: `ident`
//! over every ordinal and the scan's `ident_from` (ns/row), `decode_ids` a granule at a time (ns/row), `ordinal_of`
//! on 1M random hits and the batch `ordinals_of` (ns/ID). Only the catalog's public API, so the same file times the
//! line and the tip. A timing run, not a check:
//! `cargo test -p pgrc_am --profile fast-profile -- --ignored --nocapture rowmap2_rowmap_bench` on box 22.

use std::sync::Arc;
use std::time::Instant;

use pgrc_format::rowid::IDS_PER_BLOCK;
use pgrc_format::rowmap::{encode, GranMap, Opts, RevForm, GRANULE};

use crate::catalog::{GranuleMap, RowMap};

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// A sealed segment's IDs: 80..=120 rows a 291-ID block, from `base`.
fn sealed(rng: &mut Rng, rows: usize, base: u64) -> Vec<u64> {
    let mut out = Vec::with_capacity(rows);
    let mut block = 0u64;
    while out.len() < rows {
        let per = 80 + rng.below(41);
        out.extend((0..per).map(|off| base + block * IDS_PER_BLOCK + off).take(rows - out.len()));
        block += 1;
    }
    out
}

#[test]
#[ignore]
fn rowmap2_rowmap_bench() {
    let n = 1usize << 20;
    let mut rng = Rng(77);
    let mono = sealed(&mut rng, n, 291 * 4096);
    let jitter = {
        let mut k: Vec<(u64, u64)> = mono.iter().enumerate().map(|(t, &id)| (t as u64 + rng.below(50), id)).collect();
        k.sort_unstable();
        k.into_iter().map(|x| x.1).collect::<Vec<_>>()
    };
    let scattered = {
        let mut k: Vec<(u64, u64)> = mono.iter().map(|&id| (rng.below(1000), id)).collect();
        k.sort_unstable();
        k.into_iter().map(|x| x.1).collect::<Vec<_>>()
    };
    let cases: [(&str, &[u64], Opts); 5] = [
        ("mono sealed", &mono, Opts::default()),
        ("fences (key within 50 rows)", &jitter, Opts { force: Some(RevForm::Fences) }),
        ("labels (1000 envs)", &scattered, Opts { force: Some(RevForm::Labels) }),
        ("frameix (1000 envs)", &scattered, Opts { force: Some(RevForm::FrameIx) }),
        ("elected (1000 envs)", &scattered, Opts::default()),
    ];
    let mut maps: Vec<(&str, &[u64], RowMap)> = vec![("implicit", &mono[..0], RowMap::Implicit(291 * 4096))];
    for (name, ids, opts) in cases {
        let body = encode(ids, opts).expect("encode");
        maps.push((name, ids, RowMap::Granules(Arc::new(GranuleMap::new(GranMap::decode(&body, ids.len() as u64).expect("decode"))))));
    }
    let implicit: Vec<u64> = (0..n as u64).map(|o| 291 * 4096 + o).collect();
    for (name, ids, m) in &maps {
        let ids: &[u64] = if ids.is_empty() { &implicit } else { ids };
        let rows = ids.len() as u64;
        assert_eq!(m.ordinal_of(rows, ids[7]), Some(7), "{name}: warm");
        let mut sum = 0u64;
        let t = Instant::now();
        for o in 0..rows as u32 {
            sum = sum.wrapping_add(m.ident(o));
        }
        let ident_ns = t.elapsed().as_nanos() as f64 / rows as f64;
        let mut hint = 0usize;
        let t = Instant::now();
        for o in 0..rows as u32 {
            sum = sum.wrapping_add(m.ident_from(o, &mut hint));
        }
        let from_ns = t.elapsed().as_nanos() as f64 / rows as f64;
        let mut out = vec![0u64; GRANULE as usize];
        let t = Instant::now();
        for g in 0..ids.len().div_ceil(GRANULE as usize) {
            let o = g * GRANULE as usize;
            let k = (ids.len() - o).min(GRANULE as usize);
            m.decode_ids(o as u32, &mut out[..k]);
            sum = sum.wrapping_add(out[k - 1]);
        }
        let dec_ns = t.elapsed().as_nanos() as f64 / rows as f64;
        let probes: Vec<u64> = (0..1_000_000).map(|_| ids[rng.below(rows) as usize]).collect();
        let mut hit = 0u64;
        let t = Instant::now();
        for &id in &probes {
            hit += u64::from(m.ordinal_of(rows, id).is_some());
        }
        let rev_ns = t.elapsed().as_nanos() as f64 / probes.len() as f64;
        let mut answers = vec![None; probes.len()];
        let t = Instant::now();
        m.ordinals_of(rows, &probes, &mut answers);
        let batch_ns = t.elapsed().as_nanos() as f64 / probes.len() as f64;
        assert_eq!(hit, probes.len() as u64, "{name}: every probe hits");
        assert!(answers.iter().all(Option::is_some), "{name}: every batch probe hits");
        println!(
            "B-revam {name}: ID->ordinal {rev_ns:.1} ns, batch {batch_ns:.1} ns/ID; ident {ident_ns:.2} ns/row, ident_from {from_ns:.2} ns/row, decode_ids {dec_ns:.2} ns/row; held {} B (sum {})",
            m.mem_bytes(),
            sum % 1000
        );
    }
}
