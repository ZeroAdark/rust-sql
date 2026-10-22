//! ROWMAP2 M1: the per-granule `RowIdentMap` (`crate::rowmap`). Every form round trips both ways against the ID list
//! it was built from (forward decode by range and by ordinal, ID → ordinal for every ID, absent IDs refused, the batch
//! call); the election property test checks the kind and reverse each workload elects against a reference rule and the
//! sizes; corrupt bodies are refused and never panic.

use crate::rowid::{IDS_PER_BLOCK, ROWID_LIMIT};
use crate::rowmap::{encode, encode_runs, radix_sort, summary, GranMap, Opts, RevForm, Reverse, GRANULE, G_DELTA, G_DENSE, G_RUNS, G_STRIDED, JUMP};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const EXT: u64 = 1024 * IDS_PER_BLOCK;

/// A sealed segment's IDs: `rows` rows, `lo..=hi` rows per 291-ID block (each block's first rows), a fresh extent
/// every 1024 blocks at `bases` (cycled), `dead` per mille of rows dead before the seal.
fn sealed(rng: &mut Rng, rows: usize, lo: u64, hi: u64, bases: &[u64], dead: u64) -> Vec<u64> {
    let mut out = Vec::with_capacity(rows);
    let mut block = 0u64;
    while out.len() < rows {
        let ext = bases[(block / 1024) as usize % bases.len()] + (block / 1024 / bases.len() as u64) * EXT * bases.len() as u64;
        let per = lo + rng.below(hi - lo + 1);
        for off in 0..per {
            if out.len() == rows {
                break;
            }
            if rng.below(1000) >= dead {
                out.push(ext + (block % 1024) * IDS_PER_BLOCK + off);
            }
        }
        block += 1;
    }
    out
}

/// A sorted merge of `e` envs inserted in time order (ROWMAP-DELTA's workload): the IDs of a sealed stream, ordered by
/// (env, ID); `late` per mille of rows displaced by up to `win` rows.
fn envs(rng: &mut Rng, rows: usize, e: u64, late: u64, win: u64) -> Vec<u64> {
    let ids = sealed(rng, rows, 80, 120, &[291 * 4096], 0);
    let mut keyed: Vec<(u64, u64, u64)> = ids
        .iter()
        .enumerate()
        .map(|(t, &id)| {
            let t = if rng.below(1000) < late { (t as u64).saturating_sub(rng.below(win + 1)) } else { t as u64 };
            (rng.below(e), t, id)
        })
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|k| k.2).collect()
}

fn shuffled(rng: &mut Rng, mut ids: Vec<u64>) -> Vec<u64> {
    for i in (1..ids.len()).rev() {
        ids.swap(i, rng.below(i as u64 + 1) as usize);
    }
    ids
}

/// The map of `ids` (through `opts`), decoded, checked both ways against `ids`; its elected form and granule forms.
fn round_trip(ids: &[u64], opts: Opts) -> (RevForm, [u32; 4], GranMap, Option<Reverse>) {
    let body = encode(ids, opts).expect("encode");
    let m = GranMap::decode(&body, ids.len() as u64).expect("decode");
    let rev = m.needs_rev().then(|| m.build_frameix().expect("frameix"));
    let n = ids.len();
    assert_eq!(m.rows() as usize, n);
    let mut all = vec![0u64; n];
    m.decode_ids(0, &mut all);
    assert_eq!(all, ids, "forward decode");
    let mut rng = Rng(0x9e37_79b9 ^ n as u64);
    for _ in 0..200.min(n) {
        let (a, len) = (rng.below(n as u64) as usize, 1 + rng.below(700) as usize);
        let len = len.min(n - a);
        let mut part = vec![0u64; len];
        m.decode_ids(a as u32, &mut part);
        assert_eq!(part, &ids[a..a + len], "range {a}+{len}");
        assert_eq!(m.ident(a as u32), ids[a], "ident {a}");
    }
    for (o, &id) in ids.iter().enumerate() {
        assert_eq!(m.ordinal_of(id, rev.as_ref()), Some(o as u32), "ordinal of {id} (row {o}), {:?}", m.rev_form());
    }
    let mut sorted: Vec<u64> = ids.to_vec();
    sorted.sort_unstable();
    let mut absent: Vec<u64> = vec![0, sorted.first().map_or(0, |x| x.saturating_sub(1)), sorted.last().map_or(0, |x| x + 1), ROWID_LIMIT - 1];
    absent.extend(sorted.windows(2).filter(|w| w[1] > w[0] + 1).take(300).map(|w| w[0] + 1));
    for &x in &absent {
        if sorted.binary_search(&x).is_err() {
            assert_eq!(m.ordinal_of(x, rev.as_ref()), None, "absent {x}");
        }
    }
    // the batch call: every ID sorted, a sparse sample, the absent ones
    let mut by_id: Vec<(u64, u32)> = ids.iter().enumerate().map(|(o, &x)| (x, o as u32)).collect();
    by_id.sort_unstable();
    let pos = |x: u64| by_id.binary_search_by_key(&x, |e| e.0).ok().map(|i| by_id[i].1);
    let mut out = vec![None; sorted.len()];
    m.ordinals_of(&sorted, rev.as_ref(), &mut out);
    assert!(sorted.iter().zip(&out).all(|(&x, &o)| o.is_some_and(|o| ids[o as usize] == x)), "batch, every ID");
    let sample: Vec<u64> = sorted.iter().step_by(97).copied().chain(absent.iter().copied()).collect();
    let mut out = vec![None; sample.len()];
    m.ordinals_of(&sample, rev.as_ref(), &mut out);
    for (i, &x) in sample.iter().enumerate() {
        assert_eq!(out[i], if sorted.binary_search(&x).is_ok() { pos(x) } else { None }, "batch sample {x}");
    }
    let (rf, forms) = summary(&body).expect("summary");
    assert_eq!((rf, forms), (m.rev_form(), m.granule_forms()), "summary agrees");
    (m.rev_form(), m.granule_forms(), m, rev)
}

#[test]
fn rowmap_dense_parts_are_header_only() {
    let ids: Vec<u64> = (5_000_000..5_000_000 + 20_000).collect();
    let (rf, forms, m, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono);
    assert_eq!(forms[G_DENSE as usize], 3, "{forms:?}");
    assert!(m.mem_bytes() < 300, "{} bytes", m.mem_bytes());
}

#[test]
fn rowmap_fixed_width_sealed_parts_are_strided() {
    let mut rng = Rng(7);
    let ids = sealed(&mut rng, 1 << 20, 113, 113, &[291 * 2048, 291 * 1024 * 40], 0);
    let (rf, forms, m, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono);
    assert!(forms[G_STRIDED as usize] >= 110, "{forms:?}");
    let bpr = m.mem_bytes() as f64 / ids.len() as f64;
    assert!(bpr <= 0.02, "{bpr} B/row");
}

#[test]
fn rowmap_variable_sealed_parts_are_runs_under_a_tenth_of_a_byte() {
    let mut rng = Rng(11);
    let ids = sealed(&mut rng, 1 << 20, 80, 120, &[291 * 3000], 0);
    let (rf, forms, m, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono);
    assert!(forms[G_RUNS as usize] >= 120, "{forms:?}");
    let bpr = m.mem_bytes() as f64 / ids.len() as f64;
    assert!(bpr <= 0.1, "{bpr} B/row");
}

#[test]
fn rowmap_holey_sealed_parts_take_delta_frames() {
    let mut rng = Rng(13);
    let ids = sealed(&mut rng, 200_000, 150, 250, &[291 * 9000], 400);
    let (rf, forms, _, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono);
    assert!(forms[G_DELTA as usize] >= 1, "{forms:?}");
}

/// A MONO part's DELTA granules next to other forms (a sealed segment with a holey stretch: seal-offhead's dm case and
/// front's maint/stack met it): a DELTA granule's last frame ends at its granule's end, not at the next DELTA
/// granule's first frame or the part's end. Granules holey, dense, holey, strided, then a holey partial one.
#[test]
fn rowmap_mono_delta_granules_next_to_other_forms() {
    let mut rng = Rng(19);
    let g = GRANULE as usize;
    let mut ids = Vec::new();
    for i in 0..5u64 {
        let (base, rows) = (EXT * (4 + 2 * i), if i == 4 { 3000 } else { g });
        match i {
            1 => ids.extend(base..base + rows as u64),
            3 => ids.extend(sealed(&mut rng, rows, 113, 113, &[base], 0)),
            _ => ids.extend(sealed(&mut rng, rows, 150, 250, &[base], 400)),
        }
    }
    let (rf, forms, m, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono);
    assert_eq!((forms[G_DELTA as usize], forms[G_DENSE as usize], forms[G_STRIDED as usize]), (3, 1, 1), "{forms:?}");
    let n = ids.len();
    for gi in 0..n.div_ceil(g) {
        let (a, b) = (gi * g, ((gi + 1) * g).min(n));
        let mut out = vec![0u64; b - a];
        m.decode_ids(a as u32, &mut out);
        assert_eq!(out, &ids[a..b], "granule {gi}");
        for a in [b.saturating_sub(1), b.saturating_sub(7), b.saturating_sub(300)] {
            let len = 20.min(n - a);
            let mut out = vec![0u64; len];
            m.decode_ids(a as u32, &mut out);
            assert_eq!(out, &ids[a..a + len], "rows {a}+{len} across granule {gi}'s end");
            assert_eq!(m.ident(a as u32), ids[a], "ident {a}");
        }
    }
}

/// A sorted merge's ranks: IDs spanning under 64 per row go through the span bitmap, wider ones (here 1000 apart)
/// through the radix sort; both round trip under every reverse.
#[test]
fn rowmap_sorted_merges_rank_by_bitmap_or_radix() {
    let mut rng = Rng(23);
    for spread in [3u64, 1000] {
        let ids = shuffled(&mut rng, (0..6000u64).map(|i| 291 * 64 + i * spread).collect());
        for force in [RevForm::Fences, RevForm::Labels, RevForm::FrameIx] {
            let (rf, ..) = round_trip(&ids, Opts { force: Some(force) });
            assert_eq!(rf, force, "spread {spread}");
        }
    }
}

#[test]
fn rowmap_concatenations_out_of_id_order_stay_mono() {
    let mut rng = Rng(17);
    // three inputs, the second below the first, a bulk part in between, granule boundaries inside inputs
    let a = sealed(&mut rng, 30_001, 60, 140, &[291 * 1024 * 50], 20);
    let b = sealed(&mut rng, 12_345, 60, 140, &[291 * 1024 * 10], 0);
    let c: Vec<u64> = (291 * 1024 * 30..291 * 1024 * 30 + 9_999).collect();
    let ids: Vec<u64> = a.iter().chain(&b).chain(&c).copied().collect();
    let (rf, forms, _, _) = round_trip(&ids, Opts::default());
    assert_eq!(rf, RevForm::Mono, "{forms:?}");
    // runs over several ranges (IDBURN's bulk parts, a seal's blocks): the same map as the ID list's
    let runs: Vec<(u64, u32)> = vec![(291 * 7, 291 * 10), (291 * 500, 13), (291 * 40, 0), (291 * 900, 9000)];
    let ids: Vec<u64> = runs.iter().flat_map(|&(lo, n)| lo..lo + u64::from(n)).collect();
    let body = encode_runs(&runs, Opts::default()).unwrap();
    assert_eq!(body, encode(&ids, Opts::default()).unwrap(), "runs and IDs encode alike");
    round_trip(&ids, Opts::default());
}

#[test]
fn rowmap_sorted_merges_elect_labels_frameix_or_fences() {
    let mut rng = Rng(19);
    let e10 = envs(&mut rng, 1 << 18, 10, 0, 0);
    assert_eq!(round_trip(&e10, Opts::default()).0, RevForm::Labels);
    let e100 = envs(&mut rng, 1 << 18, 100, 10, 2000);
    assert_eq!(round_trip(&e100, Opts::default()).0, RevForm::Labels);
    let base = sealed(&mut rng, 1 << 17, 80, 120, &[291 * 77], 0);
    let rand = shuffled(&mut rng, base);
    assert_eq!(round_trip(&rand, Opts::default()).0, RevForm::FrameIx);
    // nearly in ID order: a few rows moved, so fragments overlap but frame fences barely do
    let mut near = sealed(&mut rng, 100_000, 80, 120, &[291 * 5], 0);
    for k in 0..20 {
        let (i, j) = (k * 4_999 + 7, k * 4_999 + 1_000);
        near.swap(i, j);
    }
    assert_eq!(round_trip(&near, Opts::default()).0, RevForm::Fences);
}

#[test]
fn rowmap_every_reverse_answers_every_permutation() {
    let mut rng = Rng(23);
    let shapes: Vec<Vec<u64>> = vec![
        // past one map granule each; a forced FENCES over a scattered part scans most frames per lookup, so the shapes
        // stay small enough for a debug build
        envs(&mut rng, 12_000, 7, 0, 0),
        envs(&mut rng, 12_000, 300, 100, 5_000),
        shuffled(&mut rng, (1..10_001).map(|k| k * 3 + 291).collect()),
        { let mut v = sealed(&mut rng, 10_000, 100, 100, &[291 * 9], 0); v.reverse(); v },
    ];
    for ids in &shapes {
        for f in [RevForm::Fences, RevForm::Labels, RevForm::FrameIx] {
            let (rf, _, _, _) = round_trip(ids, Opts { force: Some(f) });
            assert_eq!(rf, f, "forced {f:?}");
        }
    }
}

/// The reference rule: MONO iff the fragments (cut at granule starts, descents and jumps past `JUMP`) are disjoint and
/// at most one per 64 rows (or 16 past the granules).
fn reference_mono(ids: &[u64]) -> bool {
    let mut spans: Vec<(u64, u64)> = Vec::new();
    for (p, &id) in ids.iter().enumerate() {
        if p % GRANULE as usize == 0 || id <= ids[p - 1] || id - ids[p - 1] > JUMP {
            spans.push((id, id));
        } else {
            spans.last_mut().unwrap().1 = id;
        }
    }
    let n = ids.len() as u64;
    let few = spans.len() as u64 <= (n / 64).max(n.div_ceil(u64::from(GRANULE)) + 16);
    spans.sort_unstable();
    few && spans.windows(2).all(|w| w[0].1 < w[1].0)
}

#[test]
fn rowmap_election_property() {
    let mut rng = Rng(29);
    for case in 0..48u64 {
        let rows = 1 + rng.below(70_000) as usize;
        let ids = match case % 6 {
            0 => {
                let (lo, hi, b, d) = (1 + rng.below(200), 200 + rng.below(91), 291 * (1 + rng.below(1 << 20)), rng.below(500));
                sealed(&mut rng, rows, lo, hi, &[b], d)
            }
            1 => {
                let (e, late, win) = (1 + rng.below(2000), rng.below(200), rng.below(10_000));
                envs(&mut rng, rows, e, late, win)
            }
            2 => {
                let v = sealed(&mut rng, rows, 50, 150, &[291 * 31], 0);
                shuffled(&mut rng, v)
            }
            3 => {
                let mut v = sealed(&mut rng, rows, 90, 110, &[291 * 3], 0);
                for _ in 0..rng.below(30) {
                    let (i, j) = (rng.below(rows as u64) as usize, rng.below(rows as u64) as usize);
                    v.swap(i, j);
                }
                v
            }
            4 => {
                // dense ranges at distinct starts 20 blocks apart (a range is at most 5,000 IDs): a concatenation of
                // bulk parts in any order
                let mut used = std::collections::HashSet::new();
                let mut v: Vec<u64> = Vec::new();
                while v.len() < rows {
                    let b = rng.below(1 << 20);
                    if !used.insert(b) {
                        continue;
                    }
                    let lo = 291 * 20 * b;
                    let n = (1 + rng.below(5000)).min((rows - v.len()) as u64);
                    v.extend(lo..lo + n);
                }
                v
            }
            _ => (0..rows as u64).map(|k| 291 * 8 + k * (1 + case)).collect(),
        };
        let (rf, _, m, rev) = round_trip(&ids, Opts::default());
        assert_eq!(rf == RevForm::Mono, reference_mono(&ids), "case {case}: {rf:?}");
        if rf != RevForm::Mono && rf != RevForm::Fences {
            // the elected reverse is no larger (within 5 %) than the other in memory
            let other = if rf == RevForm::Labels { RevForm::FrameIx } else { RevForm::Labels };
            let (_, _, m2, rev2) = round_trip(&ids, Opts { force: Some(other) });
            let size = |m: &GranMap, r: &Option<Reverse>| m.mem_bytes() + r.as_ref().map_or(0, Reverse::mem_bytes);
            let (a, b) = (size(&m, &rev), size(&m2, &rev2));
            assert!(a as f64 <= b as f64 * 1.05, "case {case}: elected {rf:?} {a} B vs {other:?} {b} B");
        }
    }
}

#[test]
fn rowmap_corrupt_bodies_are_refused_and_never_panic() {
    let mut rng = Rng(31);
    let shapes: Vec<Vec<u64>> = vec![
        sealed(&mut rng, 20_000, 80, 120, &[291 * 70], 30),
        sealed(&mut rng, 20_000, 100, 100, &[291 * 70], 0),
        envs(&mut rng, 20_000, 9, 20, 500),
        shuffled(&mut rng, (1..20_001).map(|k| k * 2 + 291).collect()),
        (291..291 + 20_000).collect(),
    ];
    for ids in &shapes {
        let body = encode(ids, Opts::default()).unwrap();
        let n = ids.len() as u64;
        assert!(GranMap::decode(&body, n + 1).is_err(), "rows mismatch");
        assert!(GranMap::decode(&body[..body.len() - 1], n).is_err(), "truncated");
        let mut long = body.clone();
        long.push(0);
        assert!(GranMap::decode(&long, n).is_err(), "trailing bytes");
        let mut v = body.clone();
        v[0] = 9;
        assert!(GranMap::decode(&v, n).is_err(), "version");
        for _ in 0..400 {
            let mut b = body.clone();
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(b.len() as u64) as usize;
                b[i] ^= 1 << rng.below(8);
            }
            if let Ok(m) = GranMap::decode(&b, n) {
                let rev = m.needs_rev().then(|| m.build_frameix().ok()).flatten();
                let mut all = vec![0u64; n as usize];
                m.decode_ids(0, &mut all);
                for &id in ids.iter().step_by(37) {
                    let _ = m.ordinal_of(id, rev.as_ref());
                }
                let mut out = vec![None; 64];
                m.ordinals_of(&ids[..64], rev.as_ref(), &mut out);
            }
        }
    }
}

#[test]
fn rowmap_radix_sort_sorts() {
    let mut rng = Rng(37);
    for n in [0usize, 1, 255, 256, 10_000] {
        let mut a: Vec<u64> = (0..n).map(|_| rng.next() >> 4).collect();
        let mut b = a.clone();
        radix_sort(&mut a, 60);
        b.sort_unstable();
        assert_eq!(a, b);
    }
    let mut c: Vec<u64> = (0..5000).map(|_| rng.below(1 << 20) << 20 | 7).collect();
    let mut d = c.clone();
    radix_sort(&mut c, 40);
    d.sort_unstable();
    assert_eq!(c, d, "shared low digits skip");
}

#[test]
fn rowmap_empty_and_tiny_parts() {
    round_trip(&[], Opts::default());
    round_trip(&[291], Opts::default());
    round_trip(&[ROWID_LIMIT - 1, 291], Opts::default());
    round_trip(&[900, 300, 600], Opts::default());
    let ids: Vec<u64> = (0..GRANULE as u64 + 1).map(|k| 291 + k * 2).collect();
    round_trip(&ids, Opts::default());
}

/// The manifest's ROWMAP2 bit (main 10-06): `with_ext` sets it exactly when a part names a granules map (a retired
/// runs map, form 1, or an implicit part leaves it clear, so a pre-M1 binary still reads that table), and decode
/// refuses a bit that disagrees with the parts either way.
#[test]
fn rowmap_manifest_bit_follows_the_granules_parts() {
    use crate::manifest::{Manifest, MANIFESTF_ROWMAP2, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};
    use crate::FormatError;
    let base = crate::tests::golden_sample_manifest();
    let with = |flags: u32| {
        let mut m = base.clone();
        m.parts[1].flags = flags;
        let ext = m.ext.clone();
        m.with_ext(ext)
    };
    for (flags, set) in [
        (0, false),
        (PART_FLAG_ROWMAP | 1 << PART_ROWMAP_FORM_SHIFT, false),
        // the form bits without the map flag name no map
        (PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT, false),
        (PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT, true),
    ] {
        let m = with(flags);
        assert_eq!(m.header.flags & MANIFESTF_ROWMAP2 != 0, set, "part flags {flags:#x}");
        assert_eq!(Manifest::decode(&m.encode()).as_ref(), Ok(&m), "part flags {flags:#x} round trip");
        let mut lie = m.clone();
        lie.header.flags ^= MANIFESTF_ROWMAP2;
        assert_eq!(Manifest::decode(&lie.encode()), Err(FormatError::Corrupt { at: "ManifestHeader flags vs ROWMAP2 parts" }), "part flags {flags:#x}: the bit flipped");
    }
}

/// The PUBLISH record of a manifest with the ROWMAP2 bit (as SEAL-OFFHEAD's UNDER bit): its kind byte carries
/// `PUBLISH_KIND_ROWMAP2`, which a pre-M1 binary's decode refuses as an unknown kind before its redo touches a file;
/// this binary round-trips it (with UNDER too) and refuses a kind bit that disagrees with the header's flag.
#[test]
fn rowmap_publish_sets_the_kind_bit() {
    use crate::manifest::{MANIFESTF_ROWMAP2, MANIFESTF_UNDER};
    use crate::wal::{DirLoc, PublishKind, PublishRec, PUBLISH_KIND_ROWMAP2, PUBLISH_KIND_UNDER};
    let at = 12 + 8; // the kind byte follows the dir and the base generation
    let header = crate::tests::golden_sample_manifest().header;
    let mut rec = PublishRec { dir: DirLoc { spc: 1663, db: 5, relnumber: 16384 }, base_gen: 6, kind: PublishKind::Seal, header, upsert: Vec::new(), retire: Vec::new(), ext: Vec::new(), installs: Vec::new(), renames: Vec::new(), manifest_crc: 1 };
    let mut plain = Vec::new();
    rec.encode(&mut plain).unwrap();
    assert_eq!(plain[at], PublishKind::Seal as u8, "no bit without the flag");
    rec.header.flags |= MANIFESTF_ROWMAP2;
    let mut out = Vec::new();
    rec.encode(&mut out).unwrap();
    assert_eq!(out[at], PublishKind::Seal as u8 | PUBLISH_KIND_ROWMAP2);
    assert_eq!(PublishRec::decode(&out).as_ref(), Ok(&rec));
    assert!(PublishKind::from_u8(out[at] & !PUBLISH_KIND_UNDER).is_none(), "a pre-M1 decode (UNDER masked only) refuses the kind");
    let mut lie = out.clone();
    lie[at] &= !PUBLISH_KIND_ROWMAP2;
    assert!(PublishRec::decode(&lie).is_err(), "the flag without the bit");
    plain[at] |= PUBLISH_KIND_ROWMAP2;
    assert!(PublishRec::decode(&plain).is_err(), "the bit without the flag");
    rec.header.flags |= MANIFESTF_UNDER;
    let mut both = Vec::new();
    rec.encode(&mut both).unwrap();
    assert_eq!(both[at], PublishKind::Seal as u8 | PUBLISH_KIND_UNDER | PUBLISH_KIND_ROWMAP2);
    assert_eq!(PublishRec::decode(&both).as_ref(), Ok(&rec));
}

/// Bar B-rev (ROWMAP2.done, pre-registered): warm ID -> ordinal ns per lookup per form over 1M-row parts (1M random
/// hits each, a decoded map, a built FRAMEIX reverse), the batch call per ID, the forward decode ns per row, the
/// FRAMEIX rebuild ns per row, and the bytes per row held (forward + reverse) and on disk. A timing run, not a check:
/// `cargo test -p pgrc_format --profile fast-profile -- --ignored --nocapture rowmap2_rev_bench` on box 22.
#[test]
#[ignore]
fn rowmap2_rev_bench() {
    use std::time::Instant;
    let n = 1usize << 20;
    let mut rng = Rng(77);
    let sealed_ids = sealed(&mut rng, n, 80, 120, &[291 * 4096], 20);
    let jitter = {
        let mut k: Vec<(u64, u64)> = sealed_ids.iter().enumerate().map(|(t, &id)| (t as u64 + rng.below(50), id)).collect();
        k.sort_unstable();
        k.into_iter().map(|x| x.1).collect::<Vec<_>>()
    };
    let scattered = envs(&mut rng, n, 1000, 0, 0);
    let cases: [(&str, &[u64], Opts); 5] = [
        ("mono sealed", &sealed_ids, Opts::default()),
        ("fences (key within 50 rows)", &jitter, Opts { force: Some(RevForm::Fences) }),
        ("labels (1000 envs)", &scattered, Opts { force: Some(RevForm::Labels) }),
        ("frameix (1000 envs)", &scattered, Opts { force: Some(RevForm::FrameIx) }),
        ("elected (1000 envs)", &scattered, Opts::default()),
    ];
    for (name, ids, opts) in cases {
        let t = Instant::now();
        let body = encode(ids, opts).expect("encode");
        let enc_ns = t.elapsed().as_nanos() as f64 / ids.len() as f64;
        let m = GranMap::decode(&body, ids.len() as u64).expect("decode");
        let t = Instant::now();
        let rev = m.needs_rev().then(|| m.build_frameix().expect("frameix"));
        let build_ns = t.elapsed().as_nanos() as f64 / ids.len() as f64;
        let mut out = vec![0u64; GRANULE as usize];
        let t = Instant::now();
        for g in 0..ids.len().div_ceil(GRANULE as usize) {
            let o = g * GRANULE as usize;
            let k = (ids.len() - o).min(GRANULE as usize);
            m.decode_ids(o as u32, &mut out[..k]);
        }
        let fwd_ns = t.elapsed().as_nanos() as f64 / ids.len() as f64;
        let probes: Vec<u64> = (0..1_000_000).map(|_| ids[rng.below(ids.len() as u64) as usize]).collect();
        let mut hit = 0u64;
        let t = Instant::now();
        for &id in &probes {
            hit += u64::from(m.ordinal_of(id, rev.as_ref()).is_some());
        }
        let rev_ns = t.elapsed().as_nanos() as f64 / probes.len() as f64;
        let mut answers = vec![None; probes.len()];
        let t = Instant::now();
        m.ordinals_of(&probes, rev.as_ref(), &mut answers);
        let batch_ns = t.elapsed().as_nanos() as f64 / probes.len() as f64;
        assert_eq!(hit, probes.len() as u64);
        let held = m.mem_bytes() + rev.as_ref().map_or(0, Reverse::mem_bytes);
        println!(
            "B-rev {name}: form {:?} grans {:?}; ID->ordinal {rev_ns:.1} ns, batch {batch_ns:.1} ns/ID; forward {fwd_ns:.2} ns/row; encode {enc_ns:.1} ns/row; frameix rebuild {build_ns:.1} ns/row; held {:.3} B/row, disk {:.3} B/row",
            m.rev_form(),
            m.granule_forms(),
            held as f64 / ids.len() as f64,
            body.len() as f64 / ids.len() as f64
        );
    }
}
