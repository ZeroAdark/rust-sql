//! Gate 1 of P-1 (the plan properties, `stats.rs`): random publish sequences over synthetic parts — bulk loads of
//! 1, 15, 16, 300 and 1,000 parts, seals of a reserved `part_no` below the newest, rotations and compiles (no part
//! change), retires of loose parts, of whole runs and of parts inside a run — with, after every publish:
//! - `check`, fewer than `LOOSE_MAX` loose parts, fewer than `SMALL_RUNS_MAX` small runs, every run ≤ `RUN_PARTS_MAX`;
//! - `serve(state.image(m)) == serve(the fold of m.parts in order)`, zones included;
//! - every part's answers equal its own stats, and the generation's answers columns equal the per-part build;
//! - the bytes staged for parts other than the publish's own installs within the constant bound;
//! - the runs round-trip through their files: `StatsState::load` of the directory equals the planned state.

use super::*;
use crate::facts::{serve, FactsAcc, PartColFacts, PartFacts, ServedCol};
use crate::WriteResult;
use crate::facts::ColSize;
use crate::stats::{plan_stats, FullTest, PartStats, Run, StatsState, HELD_SMALL_MAX, LOOSE_MAX, RUN_PARTS_MAX, SMALL_RUNS_MAX};
use pgrc_format::answers::ColumnAnswers;
use pgrc_format::bankstats::PartIdent;
use pgrc_format::class::{CLASS_BYVAL, CLASS_VARLENA};
use pgrc_format::dirlayout::run_file_name;
use pgrc_format::facts::FactsImage;
use pgrc_format::manifest::{with_stats_ref, ManifestHeader, PartRecord, MANIFEST_MAGIC};
use pgrc_format::meta::{stats_section_len, KeyKind, StatsRecord, STATSF_COMPUTED};
use pgrc_meta::ndv::Hll;
use std::collections::BTreeSet;

const FP: u64 = 0xfeed;

/// One synthetic part: its identity and the owned bodies of its two columns (column 2 missing on some parts).
struct Syn {
    ident: PartIdent,
    cols: Vec<(u32, Vec<u8>, Option<Vec<u8>>, bool, bool, u64)>, // attno, stats body, ndv body, bloom, dict, ncodes
}

fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hll(lo: u64, n: u64) -> Vec<u8> {
    let mut h = Hll::default();
    (lo..lo + n).for_each(|v| h.observe_hash(mix(v)));
    let mut out = Vec::new();
    h.encode_section(&mut out);
    out
}

fn syn(p: u32) -> Syn {
    let g = 1 + (mix(u64::from(p)) % 3) as u32;
    let rows = 10 * u64::from(g);
    let base = i64::from(p) * 100;
    let mut body1 = Vec::new();
    for k in 0..g {
        let mut r = StatsRecord::absent();
        r.nonnull = 10;
        r.key_kind = KeyKind::Exact.as_u8();
        r.min_key = base + i64::from(k) * 10;
        r.max_key = base + i64::from(k) * 10 + 9;
        r.flags |= STATSF_COMPUTED;
        r.byte_len_sum = 80;
        r.encode_into(&mut body1);
    }
    let mut part = StatsRecord::absent();
    part.nonnull = rows as u32;
    part.key_kind = if p % 13 == 5 { KeyKind::Absent.as_u8() } else { KeyKind::Exact.as_u8() };
    part.min_key = base;
    part.max_key = base + i64::from(g) * 10 - 1;
    part.flags |= STATSF_COMPUTED;
    part.byte_len_sum = 8 * rows;
    part.encode_into(&mut body1); // the band record
    part.encode_into(&mut body1);
    assert_eq!(body1.len(), stats_section_len(g, 1));
    let mut cols = vec![(1, body1, Some(hll(u64::from(p) * 7, rows)), p % 2 == 0, false, 0)];
    if p % 7 != 3 {
        let mut body2 = Vec::new();
        let mut r = StatsRecord::absent();
        r.nonnull = 10;
        r.key_kind = KeyKind::Absent.as_u8();
        for _ in 0..g {
            r.encode_into(&mut body2);
        }
        let mut part2 = StatsRecord::absent();
        part2.nonnull = rows as u32;
        part2.byte_len_sum = 5 * rows + u64::from(p % 9);
        part2.char_len_max = p % 50;
        if p % 5 != 1 {
            part2.flags |= STATSF_COMPUTED;
        }
        part2.encode_into(&mut body2);
        part2.encode_into(&mut body2);
        let ndv = (p % 11 != 4).then(|| hll(u64::from(p % 40), 20));
        cols.push((2, body2, ndv, false, p % 3 == 0, if p % 3 == 0 { u64::from(p % 17) } else { 0 }));
    }
    Syn { ident: PartIdent { part_no: p, granule_count: g, band_count: 1, rows, footer_off: 4096 + u64::from(p) }, cols }
}

fn part_facts(s: &Syn) -> PartFacts<'_> {
    PartFacts {
        rows: s.ident.rows,
        file_len: s.ident.footer_off + 200,
        granules: s.ident.granule_count,
        bands: 1,
        cols: s
            .cols
            .iter()
            .map(|(attno, body, ndv, bloom, dict, ncodes)| PartColFacts {
                attno: *attno,
                class: if *attno == 1 { CLASS_BYVAL } else { CLASS_VARLENA },
                signed: *attno == 1,
                width: if *attno == 1 { 8 } else { 0 },
                stats_body: Some(body),
                ndv_body: ndv.as_deref(),
                bloom: *bloom,
                dict: *dict,
                ncodes: *ncodes,
                disk_bytes: 100 + u64::from(*attno),
            })
            .collect(),
    }
}

fn part_stats(s: &Syn) -> PartStats {
    let mut acc = FactsAcc::new();
    acc.fold_part(&part_facts(s)).unwrap();
    let p = s.ident.part_no;
    PartStats {
        hold: crate::stats::Hold::default(),
        ident: s.ident,
        facts: acc.into_image(0, 0, 0).unwrap(),
        answers: s.cols.iter().map(|c| (c.0, crate::bankplane::part_record(&c.1).to_vec(), (p % 3 != 1).then_some([(p as u8 ^ c.0 as u8) | 0x80; 24]))).collect(),
        ext: if p % 4 == 0 { vec![(7, vec![p as u8; 3])] } else { Vec::new() },
    }
}

fn manifest(gen: u64, parts: &BTreeMap<u32, Syn>, next: u32, root: Option<&pgrc_format::statsrun::RunRef>) -> Manifest {
    let recs: Vec<PartRecord> = parts
        .values()
        .map(|s| PartRecord { rows: s.ident.rows, file_len: s.ident.footer_off + 200, footer_off: s.ident.footer_off, dv_gen: 0, dv_len: 0, part_no: s.ident.part_no, flags: 0, granule_count: s.ident.granule_count, band_count: 1, dv_crc: 0, granule_rows: 0 })
        .collect();
    let header = ManifestHeader { gen, prev_gen: gen - 1, publisher_fxid: 1000 + gen, relfilenumber: RELFILENUMBER, schema_fingerprint: FP, magic: MANIFEST_MAGIC, format_version: pgrc_format::FORMAT_VERSION, spc: SPC, db: DB, part_count: recs.len() as u32, next_part_no: next, flags: 0, reserved: 0 };
    Manifest::new(header, recs).with_ext(with_stats_ref(&[], root).unwrap())
}

fn all_zones(_: &pgrc_format::facts::ColFacts) -> bool {
    true
}

fn served(img: &FactsImage) -> Vec<ServedCol> {
    serve(img, &all_zones).expect("serve")
}

/// Everything gate 1 checks after one publish.
fn verify(state: &StatsState, m: &Manifest, parts: &BTreeMap<u32, Syn>, what: &str) {
    state.check(m).unwrap_or_else(|e| panic!("{what}: check: {e}"));
    assert!(state.loose.len() < LOOSE_MAX, "{what}: {} loose", state.loose.len());
    let small = state.runs.iter().filter(|r| r.parts.len() <= LOOSE_MAX).count();
    assert!(small < SMALL_RUNS_MAX, "{what}: {small} small runs");
    assert!(state.runs.iter().all(|r| r.parts.len() <= RUN_PARTS_MAX), "{what}: a run over {RUN_PARTS_MAX}");
    let mut oracle = FactsAcc::new();
    for s in parts.values() {
        oracle.fold_part(&part_facts(s)).unwrap();
    }
    let h = m.header;
    let oracle = oracle.into_image(h.gen, h.publisher_fxid, h.schema_fingerprint).unwrap();
    let (img, cols) = state.generation(m, true).unwrap_or_else(|e| panic!("{what}: image: {e:?}"));
    assert_eq!(img.header, oracle.header, "{what}: header");
    assert_eq!(served(&img), served(&oracle), "{what}: served facts (zones included)");
    // answers: every run's per-part record is the part's own, and the generation's columns are the per-part build
    for r in &state.runs {
        for (attno, payload) in &r.answers {
            let col = ColumnAnswers::parse(payload, r.parts.len()).expect("run column");
            for (i, p) in r.parts.iter().enumerate() {
                let own = part_stats(&parts[&p.part_no]);
                let want = own.answers.iter().find(|a| a.0 == *attno);
                assert_eq!(col.stats_body(i), want.map(|a| &a.1[..]), "{what}: run record of part {} col {attno}", p.part_no);
                assert_eq!(col.digest(i), want.and_then(|a| a.2.as_ref()).map(|d| &d[..]), "{what}: digest");
            }
        }
        let want_ext: Vec<(u32, u16, Vec<u8>)> = r.parts.iter().flat_map(|p| part_stats(&parts[&p.part_no]).ext.into_iter().map(move |(t, b)| (p.part_no, t, b))).collect();
        assert_eq!(r.ext, want_ext, "{what}: part extensions");
    }
    for l in &state.loose {
        assert_eq!(**l, part_stats(&parts[&l.ident.part_no]), "{what}: loose part");
    }
    let first = parts.values().next().map(|s| s.cols.iter().map(|c| c.0).collect::<Vec<_>>()).unwrap_or_default();
    assert_eq!(cols.iter().map(|c| c.0).collect::<Vec<_>>(), first, "{what}: the generation's columns are the first part's");
    for (attno, payload) in &cols {
        let col = ColumnAnswers::parse(payload, m.parts.len()).expect("generation column");
        for (i, s) in parts.values().enumerate() {
            let own = s.cols.iter().find(|c| c.0 == *attno).map(|c| crate::bankplane::part_record(&c.1));
            assert_eq!(col.stats_body(i), own, "{what}: generation record {i}");
        }
    }
}

/// The plan's staged runs written under their names, then the generation loaded back from the directory.
fn round_trip(vfs: &mut MemVfs, plan: &crate::stats::StatsPlan, m: &Manifest, parts: &BTreeMap<u32, Syn>, what: &str) {
    for (r, bytes) in &plan.stage {
        let fd = vfs.create_rw(&format!("{DIR}/{}", run_file_name(r.gen, r.seq))).unwrap();
        vfs.pwrite_at(&fd, 0, bytes).unwrap();
        vfs.close_file(fd).unwrap();
    }
    let loaded = StatsState::load(vfs, DIR, m, None, &mut |_, rec| Ok(part_stats(&parts[&rec.part_no]))).unwrap_or_else(|e| panic!("{what}: load: {e}"));
    let key = |s: &StatsState| {
        let mut runs: Vec<crate::stats::Run> = s.runs.iter().map(|r| (**r).clone()).collect();
        runs.sort_by_key(|r| r.rref.key());
        let loose: Vec<PartIdent> = s.loose.iter().map(|p| p.ident).collect();
        (s.root, runs, loose)
    };
    assert_eq!(key(&loaded), key(&plan.state), "{what}: the directory's runs are the plan's");
}

struct Seq {
    rng: u64,
}

impl Seq {
    fn next(&mut self, n: u64) -> u64 {
        self.rng = mix(self.rng);
        self.rng % n
    }
}

fn run_sequence(seed: u64, steps: usize) {
    let mut vfs = mem_with_dir();
    let mut rng = Seq { rng: seed };
    let mut parts: BTreeMap<u32, Syn> = BTreeMap::new();
    let mut state = StatsState::default();
    let mut next = 0u32;
    let mut reserved: Vec<u32> = Vec::new();
    let mut max_rewrite = 0usize;
    for step in 0..steps {
        let gen = step as u64 + 1;
        let mut installs: Vec<u32> = Vec::new();
        let mut retired: Vec<u32> = Vec::new();
        let op = rng.next(10);
        let what = format!("seed {seed} step {step} op {op}");
        match op {
            0..=2 => {
                let n = [1u32, 15, 16, 300, 1000][rng.next(if step < 3 { 5 } else { 4 }) as usize];
                installs.extend(next..next + n);
                next += n;
            }
            3 => {
                reserved.push(next);
                next += 1;
                installs.push(next);
                next += 1;
            }
            4 => match reserved.pop() {
                Some(r) => installs.push(r),
                None => {}
            },
            5 => {} // a rotation or a compile: no part changes
            6 => retired.extend(state.loose.iter().take(1 + rng.next(3) as usize).map(|p| p.ident.part_no)),
            7 => {
                if !state.runs.is_empty() {
                    let r = &state.runs[rng.next(state.runs.len() as u64) as usize];
                    retired.extend(r.parts.iter().map(|p| p.part_no));
                }
            }
            _ => {
                if let Some(r) = state.runs.iter().find(|r| r.parts.len() > 3) {
                    let k = 1 + rng.next(3) as usize;
                    let start = rng.next((r.parts.len() - k) as u64) as usize;
                    retired.extend(r.parts[start..start + k].iter().map(|p| p.part_no));
                }
            }
        }
        for p in &retired {
            parts.remove(p);
        }
        for &p in &installs {
            parts.insert(p, syn(p));
        }
        let fresh: Vec<PartStats> = installs.iter().map(|&p| part_stats(&parts[&p])).collect();
        let mut refold = |ids: &[PartIdent]| -> WriteResult<Vec<PartStats>> { Ok(ids.iter().map(|id| { let mut s = part_stats(&parts[&id.part_no]); s.ext.clear(); s }).collect()) };
        let plan = plan_stats(&state, gen, 1000 + gen, FP, fresh, &retired, &mut refold, &|_| false).unwrap_or_else(|e| panic!("{what}: plan: {e}"));
        if installs.is_empty() && retired.is_empty() {
            assert!(plan.stage.is_empty() && !plan.root_changed, "{what}: a publish without part changes writes no run");
        }
        // the constant bound: parts written by this publish that it did not install
        let install_set: BTreeSet<u32> = installs.iter().copied().collect();
        let rewritten: usize = plan.stage.iter().filter_map(|(r, _)| plan.state.runs.iter().find(|x| x.rref == *r)).filter(|r| r.parts.iter().any(|p| !install_set.contains(&p.part_no))).map(|r| r.parts.len()).sum();
        assert!(rewritten <= RUN_PARTS_MAX + LOOSE_MAX + RUN_PARTS_MAX, "{what}: {rewritten} parts rewritten");
        let bytes: usize = plan.stage.iter().filter(|(r, _)| plan.state.runs.iter().find(|x| x.rref == *r).is_none_or(|x| x.parts.iter().any(|p| !install_set.contains(&p.part_no)))).map(|(_, b)| b.len()).sum();
        assert!(bytes <= rewritten * 512 + plan.stage.len() * (2 * 1100 + 1024), "{what}: {bytes} staged bytes for {rewritten} parts");
        max_rewrite = max_rewrite.max(rewritten);
        let m = manifest(gen, &parts, next, plan.state.root.as_ref());
        verify(&plan.state, &m, &parts, &what);
        round_trip(&mut vfs, &plan, &m, &parts, &what);
        state = plan.state;
    }
    assert!(max_rewrite > 0, "seed {seed}: the sequence never folded, merged or cut");
}

#[test]
fn random_publish_sequences_keep_the_invariants_and_the_fold() {
    for seed in [1, 2, 3, 0xdead_beef] {
        run_sequence(seed, 70);
    }
}

#[test]
fn loose_parts_fold_and_small_runs_merge_at_their_bounds() {
    let mut parts: BTreeMap<u32, Syn> = BTreeMap::new();
    let mut state = StatsState::default();
    let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("no retires here") };
    let mut writes = Vec::new();
    for p in 0..(LOOSE_MAX * SMALL_RUNS_MAX) as u32 {
        parts.insert(p, syn(p));
        let plan = plan_stats(&state, u64::from(p) + 1, 7, FP, vec![part_stats(&parts[&p])], &[], &mut refold, &|_| false).unwrap();
        writes.push(plan.stage.iter().map(|(r, _)| r.nparts).collect::<Vec<_>>());
        state = plan.state;
    }
    // one in 16 writes a small run; the 256th also merges the 16 small runs into one big run (then carries the root)
    for (p, w) in writes.iter().enumerate() {
        let want: Vec<u32> = match p + 1 {
            n if n == LOOSE_MAX * SMALL_RUNS_MAX => vec![RUN_PARTS_MAX as u32],
            n if n % LOOSE_MAX == 0 => vec![LOOSE_MAX as u32],
            _ => vec![],
        };
        assert_eq!(w, &want, "publish {p}");
    }
    assert_eq!((state.runs.len(), state.loose.len()), (1, 0));
    // `hold` keeps a part's small run out of the merge
    let mut state = StatsState::default();
    for p in 0..(LOOSE_MAX * SMALL_RUNS_MAX) as u32 {
        let plan = plan_stats(&state, u64::from(p) + 1, 7, FP, vec![part_stats(&parts[&p])], &[], &mut refold, &|r: &Run| r.parts.iter().any(|p| p.part_no == 3)).unwrap();
        state = plan.state;
    }
    assert_eq!(state.runs.len(), SMALL_RUNS_MAX, "the held run and 15 others: not yet 16 mergeable");
    assert!(state.run_of(3).is_some_and(|r| r.parts.len() == LOOSE_MAX));
}

/// The born-red tooth: a state whose root dropped one run fails `check`, and a directory whose root lists a run
/// that is gone is refused by `load`.
#[test]
fn a_root_missing_a_run_is_refused() {
    let mut vfs = mem_with_dir();
    let parts: BTreeMap<u32, Syn> = (0..40u32).map(|p| (p, syn(p))).collect();
    let installs: Vec<PartStats> = parts.values().map(part_stats).collect();
    let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!() };
    let plan = plan_stats(&StatsState::default(), 1, 7, FP, installs, &[], &mut refold, &|_| false).unwrap();
    let m = manifest(1, &parts, 40, plan.state.root.as_ref());
    assert!(plan.state.check(&m).is_ok());
    let mut dropped = plan.state.clone();
    dropped.runs.remove(0);
    assert!(dropped.check(&m).is_err(), "a root that lost a run no longer covers the manifest");
    round_trip(&mut vfs, &plan, &m, &parts, "setup");
    let (lost, _) = &plan.stage[0];
    vfs.unlink_path(&format!("{DIR}/{}", run_file_name(lost.gen, lost.seq))).unwrap();
    assert!(StatsState::load(&mut vfs, DIR, &m, None, &mut |_, rec| Ok(part_stats(&parts[&rec.part_no]))).is_err());
    // a run of another schema is refused too
    let other = plan_stats(&StatsState::default(), 1, 7, FP + 1, parts.values().map(part_stats).collect(), &[], &mut refold, &|_| false).unwrap();
    for (r, bytes) in &other.stage {
        let fd = vfs.create_rw(&format!("{DIR}/{}", run_file_name(r.gen, r.seq))).unwrap();
        vfs.pwrite_at(&fd, 0, bytes).unwrap();
        vfs.close_file(fd).unwrap();
    }
    let m2 = manifest(1, &parts, 40, other.state.root.as_ref());
    assert!(StatsState::load(&mut vfs, DIR, &m2, None, &mut |_, rec| Ok(part_stats(&parts[&rec.part_no]))).is_err(), "binds_schema");
}

/// P-1 L4a: `StatsState::order` walks the manifest merging the sources by their next part, and a state whose
/// sources it cannot walk (loose parts or runs out of `part_no` order) takes the sort. Either way every manifest
/// record is named by the source part with its ident, and a state missing a part or holding a changed one is refused.
#[test]
fn the_order_walk_and_the_sort_agree_whatever_the_source_order() {
    let parts: BTreeMap<u32, Syn> = (0..(3 * RUN_PARTS_MAX as u32 + 7)).map(|p| (p, syn(p))).collect();
    let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!() };
    let bulk: Vec<PartStats> = parts.values().filter(|s| s.ident.part_no < 3 * RUN_PARTS_MAX as u32).map(part_stats).collect();
    let mut state = plan_stats(&StatsState::default(), 1, 7, FP, bulk, &[], &mut refold, &|_| false).unwrap().state;
    for (k, p) in (3 * RUN_PARTS_MAX as u32..3 * RUN_PARTS_MAX as u32 + 7).enumerate() {
        state = plan_stats(&state, k as u64 + 2, 7, FP, vec![part_stats(&parts[&p])], &[], &mut refold, &|_| false).unwrap().state;
    }
    assert_eq!((state.runs.len(), state.loose.len()), (3, 7), "three bulk runs and seven loose parts");
    let m = manifest(9, &parts, 3 * RUN_PARTS_MAX as u32 + 7, state.root.as_ref());
    let named = |st: &StatsState| -> Vec<PartIdent> {
        st.try_index(&m).expect("covers").iter().map(|&(s, i)| match st.runs.get(s as usize) {
            Some(r) => r.parts[i as usize],
            None => st.loose[s as usize - st.runs.len()].ident,
        }).collect()
    };
    let want: Vec<PartIdent> = m.parts.iter().map(crate::stats::ident_of).collect();
    assert_eq!(named(&state), want, "the walk");
    let mut shuffled = state.clone();
    shuffled.runs.reverse();
    shuffled.loose.reverse();
    assert_eq!(named(&shuffled), want, "sources out of order: the sort");
    let mut short = state.clone();
    short.loose.pop();
    assert!(short.check(&m).is_err(), "a state missing a part does not cover the manifest");
    let mut changed = state.clone();
    let mut p = (*changed.loose[0]).clone();
    p.ident.rows += 1;
    changed.loose[0] = std::sync::Arc::new(p);
    assert!(changed.check(&m).is_err(), "nor one holding a part whose ident changed");
    let mut doubled = state.clone();
    doubled.loose[0] = std::sync::Arc::clone(&state.loose[1]);
    assert!(doubled.check(&m).is_err(), "nor one naming a part twice");
}

/// P-1 L4a: a publish's part changes against its base by one merge join (`publish::part_changes`) equal the two maps
/// they replaced: base parts gone or with a changed ident retire, new parts or changed idents install, a DV-only change
/// is neither.
#[test]
fn part_changes_by_one_merge_join_equal_the_map_form() {
    let rec = |p: u32, rows: u64| PartRecord { rows, file_len: 300, footer_off: 100, dv_gen: 0, dv_len: 0, part_no: p, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
    let mut rng = Seq { rng: 7 };
    for _ in 0..200 {
        let (mut base, mut new) = (Vec::new(), Vec::new());
        for p in 0..40u32 {
            match rng.next(6) {
                0 => base.push(rec(p, 10)),
                1 => new.push(rec(p, 10)),
                2 => {
                    base.push(rec(p, 10));
                    new.push(rec(p, 10));
                }
                3 => {
                    base.push(rec(p, 10));
                    new.push(rec(p, 11));
                }
                4 => {
                    base.push(rec(p, 10));
                    new.push(PartRecord { dv_gen: 3, dv_len: 8, dv_crc: 1, ..rec(p, 10) });
                }
                _ => {}
            }
        }
        let (retired, added) = crate::publish::part_changes(&base, &new);
        let bmap: BTreeMap<u32, PartRecord> = base.iter().map(|p| (p.part_no, *p)).collect();
        let nmap: BTreeMap<u32, PartRecord> = new.iter().map(|p| (p.part_no, *p)).collect();
        let changed = |p: &PartRecord, other: &BTreeMap<u32, PartRecord>| other.get(&p.part_no).is_none_or(|o| crate::stats::ident_of(o) != crate::stats::ident_of(p));
        let want_retired: Vec<u32> = bmap.values().filter(|p| changed(p, &nmap)).map(|p| p.part_no).collect();
        let want_added: Vec<usize> = new.iter().enumerate().filter(|(_, p)| changed(p, &bmap)).map(|(k, _)| k).collect();
        assert_eq!((retired, added), (want_retired, want_added));
    }
}

/// P-1 L4a: `plan::merged_parts` by one merge equals the scan form it replaced (retire, then each upsert replacing
/// its number or appended, then a sort), duplicate upserts and a retired number upserted again included.
#[test]
fn merged_parts_by_one_merge_equal_the_scan_form() {
    let rec = |p: u32, rows: u64| PartRecord { rows, file_len: 300, footer_off: 100, dv_gen: 0, dv_len: 0, part_no: p, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 0 };
    let scan = |base: &Manifest, upsert: &[PartRecord], retire: &[u32]| {
        let mut parts = base.parts.clone();
        parts.retain(|p| !retire.contains(&p.part_no));
        for u in upsert {
            match parts.iter().position(|p| p.part_no == u.part_no) {
                Some(i) => parts[i] = *u,
                None => parts.push(*u),
            }
        }
        parts.sort_by_key(|p| p.part_no);
        parts
    };
    let mut rng = Seq { rng: 11 };
    for round in 0..300u64 {
        let parts: BTreeMap<u32, Syn> = (0..40u32).filter(|_| rng.next(3) != 0).map(|p| (p, syn(p))).collect();
        let base = manifest(1, &parts, 60, None);
        let upsert: Vec<PartRecord> = (0..rng.next(8)).map(|_| rec(rng.next(60) as u32, 1 + rng.next(5))).collect();
        let retire: Vec<u32> = (0..rng.next(6)).map(|_| rng.next(45) as u32).collect();
        assert_eq!(crate::plan::merged_parts(Some(&base), &upsert, &retire), scan(&base, &upsert, &retire), "round {round}");
    }
    assert_eq!(crate::plan::merged_parts(None, &[rec(5, 1), rec(2, 1), rec(5, 2)], &[5]), vec![rec(2, 1), rec(5, 2)]);
}

/// The synthetic table's columns as `FullTest` reads them: a by-value word and a varlena.
fn full_test() -> FullTest {
    FullTest::new(vec![
        ColSize { attno: 1, class: CLASS_BYVAL, fixed_len: 0, stats: None },
        ColSize { attno: 2, class: CLASS_VARLENA, fixed_len: 0, stats: None },
    ])
}

/// A part at the bulk cut by rows (a bulk load's full part).
fn full_part(p: u32) -> Syn {
    let mut s = syn(p);
    s.ident.rows = crate::writer::PartCutPolicy::default().max_rows;
    s
}

/// `hold` is K1 L7's `full` test (`pgrc_compact::PartView::full` over `facts::cut_bytes`): rows at the cut, or
/// buffer bytes at the cut from the part's own Stats records (a varlena column counts its payload only when computed).
#[test]
fn hold_is_k1s_full_test() {
    let t = full_test();
    // part 11 carries both columns (column 2's record is left uncomputed by syn; set explicitly below)
    let small = part_stats(&syn(11));
    assert!(!t.part_full(&small), "a small part is not full: held");
    assert!(t.part_full(&part_stats(&full_part(11))), "a part at the row cut is full");
    let cut = crate::writer::PartCutPolicy::default();
    let with_bytes = |computed: bool| {
        let mut s = syn(11);
        let body2 = &mut s.cols.iter_mut().find(|c| c.0 == 2).expect("column 2").1;
        let n = body2.len() / pgrc_format::meta::STATS_RECORD_LEN;
        let mut part2 = StatsRecord::decode(&mut pgrc_format::wire::Cur::new(&body2[(n - 1) * pgrc_format::meta::STATS_RECORD_LEN..])).unwrap();
        part2.byte_len_sum = cut.max_bytes;
        part2.flags = if computed { part2.flags | STATSF_COMPUTED } else { part2.flags & !STATSF_COMPUTED };
        body2.truncate((n - 1) * pgrc_format::meta::STATS_RECORD_LEN);
        part2.encode_into(body2);
        part_stats(&s)
    };
    assert!(t.part_full(&with_bytes(true)), "a varlena payload at the byte cut is full");
    assert!(!t.part_full(&with_bytes(false)), "an uncomputed record counts no heap bytes (K1's estimate)");
    let expect = crate::facts::cut_bytes(small.ident.rows, &[ColSize { attno: 1, class: CLASS_BYVAL, fixed_len: 0, stats: None }, ColSize { attno: 2, class: CLASS_VARLENA, fixed_len: 0, stats: None }]);
    assert!(expect < cut.max_bytes);
}

/// COMPACTP (the L0 ledger, `pgrc_am::l0`): `FullTest::full_in` over a generation's stats state, at a part's place
/// found by `StatsState::locate` (a run's or a loose part's, no manifest walk), is `part_full` over that part's own
/// stats.
#[test]
fn the_full_test_reads_a_generations_state_as_its_parts() {
    let t = full_test();
    let hold = |r: &Run| t.run_holds(r);
    let mut parts: BTreeMap<u32, Syn> = BTreeMap::new();
    let mut state = StatsState::default();
    for no in 0..40u32 {
        parts.insert(no, if no % 7 == 3 { full_part(no) } else { syn(no) });
        let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("no retires") };
        state = plan_stats(&state, u64::from(no) + 1, 7, FP, vec![part_stats(&parts[&no])], &[], &mut refold, &hold).unwrap().state;
    }
    assert!(!state.runs.is_empty() && !state.loose.is_empty(), "both kinds of source");
    for (&no, s) in &parts {
        let at = state.locate(no).expect("located");
        assert_eq!(t.full_in(&state, at, s.ident.rows), t.part_full(&part_stats(s)), "part {no}");
    }
    assert_eq!(state.locate(99), None);
}

/// P-1 L2 (K1 ask 1, taken over from K1 L6): with `hold` = not full, runs of parts compaction may take stay small, so
/// a compaction whose inputs are held parts rewrites no run when they are loose or a whole small run, and at most one
/// small run when they are part of one; full parts still fold and merge into big runs.
#[test]
fn a_compaction_of_held_parts_rewrites_no_run_and_full_parts_still_merge() {
    let t = full_test();
    let hold = |r: &Run| t.run_holds(r);
    let mut parts: BTreeMap<u32, Syn> = BTreeMap::new();
    let mut state = StatsState::default();
    let mut gen = 0u64;
    let refolds = std::cell::Cell::new(0usize);
    let mut next = 0u32;
    // 260 small (held) one-part publishes: 16 small runs, none merged, 4 loose
    for _ in 0..(LOOSE_MAX * SMALL_RUNS_MAX + 4) {
        parts.insert(next, syn(next));
        gen += 1;
        let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("no retires") };
        state = plan_stats(&state, gen, 7, FP, vec![part_stats(&parts[&next])], &[], &mut refold, &hold).unwrap().state;
        next += 1;
    }
    assert_eq!((state.runs.len(), state.loose.len()), (SMALL_RUNS_MAX, 4), "held runs never merge");
    assert!(state.runs.iter().all(|r| r.parts.len() == LOOSE_MAX && hold(r)));
    assert!(state.check(&manifest(gen, &parts, next, state.root.as_ref())).is_ok());

    // a compaction: the 4 loose parts and the whole small run holding part 16 in, one output part out
    let whole: Vec<u32> = state.run_of(16).expect("run").parts.iter().map(|p| p.part_no).collect();
    let loose: Vec<u32> = state.loose.iter().map(|p| p.ident.part_no).collect();
    let retired: Vec<u32> = whole.iter().chain(&loose).copied().collect();
    retired.iter().for_each(|p| drop(parts.remove(p)));
    parts.insert(next, syn(next));
    gen += 1;
    let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("inputs are whole: no run is cut") };
    let plan = plan_stats(&state, gen, 7, FP, vec![part_stats(&parts[&next])], &retired, &mut refold, &hold).unwrap();
    next += 1;
    assert!(plan.stage.iter().all(|(r, _)| r.nparts == 0), "only the root carrier is written: {:?}", plan.stage.iter().map(|(r, _)| r.nparts).collect::<Vec<_>>());
    state = plan.state;
    assert!(state.check(&manifest(gen, &parts, next, state.root.as_ref())).is_ok());

    // a compaction of 4 parts inside a small run: that run alone is rewritten, from its 12 survivors
    let run = state.run_of(40).expect("run").clone();
    let inputs: Vec<u32> = run.parts.iter().take(4).map(|p| p.part_no).collect();
    inputs.iter().for_each(|p| drop(parts.remove(p)));
    parts.insert(next, syn(next));
    gen += 1;
    let mut refold = |ids: &[PartIdent]| -> WriteResult<Vec<PartStats>> {
        refolds.set(refolds.get() + ids.len());
        Ok(ids.iter().map(|id| part_stats(&syn(id.part_no))).collect())
    };
    let plan = plan_stats(&state, gen, 7, FP, vec![part_stats(&parts[&next])], &inputs, &mut refold, &hold).unwrap();
    next += 1;
    assert_eq!(refolds.get(), LOOSE_MAX - 4, "the cut small run's survivors only");
    assert_eq!(plan.stage.iter().map(|(r, _)| r.nparts).max(), Some((LOOSE_MAX - 4) as u32), "no run over a small run's size is written");
    state = plan.state;
    assert!(state.check(&manifest(gen, &parts, next, state.root.as_ref())).is_ok());
    let held_refs: BTreeSet<(u64, u32)> = state.runs.iter().map(|r| r.rref.key()).collect();

    // 272 full one-part publishes: the first fold takes the 2 loose held parts with 14 full ones (held), then 16 small
    // runs of full parts, which merge into one big run; the held runs stay
    let mut merged = false;
    for _ in 0..(LOOSE_MAX * SMALL_RUNS_MAX + LOOSE_MAX) {
        parts.insert(next, full_part(next));
        gen += 1;
        let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("no retires") };
        let plan = plan_stats(&state, gen, 7, FP, vec![part_stats(&parts[&next])], &[], &mut refold, &hold).unwrap();
        merged |= plan.stage.iter().any(|(r, _)| r.nparts as usize == RUN_PARTS_MAX);
        state = plan.state;
        next += 1;
    }
    assert!(merged, "full parts fold and merge normally");
    let big: Vec<&std::sync::Arc<Run>> = state.runs.iter().filter(|r| r.parts.len() > LOOSE_MAX).collect();
    assert_eq!(big.len(), 1);
    assert!(!hold(big[0]) && big[0].parts.iter().all(|p| p.rows >= crate::writer::PartCutPolicy::default().max_rows), "the big run holds full parts only");
    assert!(held_refs.iter().all(|k| state.runs.iter().any(|r| r.rref.key() == *k)), "the held small runs are untouched");
    assert_eq!(state.runs.iter().filter(|r| hold(r)).count(), held_refs.len() + 1, "and the mixed fold is held too");
    assert!(state.check(&manifest(gen, &parts, next, state.root.as_ref())).is_ok());
    let m = manifest(gen, &parts, next, state.root.as_ref());
    let all = |_: &pgrc_format::facts::ColFacts| true;
    let mut fold = FactsAcc::new();
    for s in parts.values() {
        fold.fold_part(&part_facts(s)).unwrap();
    }
    let walk = fold.into_image(gen, 7, FP).unwrap();
    assert_eq!(serve(&state.image(&m).unwrap(), &all).unwrap(), serve(&walk, &all).unwrap(), "stats == the fold");
}

/// P-1 L2b, the hold bound (coordinator ruling 09-26): with the compaction worker off every part of a table built by
/// one-row publishes is held, and without a bound its small runs would number N / 16. With it, a publish that would
/// leave more than `HELD_SMALL_MAX` held small runs merges the oldest `SMALL_RUNS_MAX` of them as if nothing were held:
/// at N = 10,000 the runs stay within the big runs (N / 256) + `HELD_SMALL_MAX`, every publish writes at most one big
/// run, and the image serves the ordered fold.
#[test]
fn the_hold_bound_keeps_ten_thousand_held_parts_in_bounded_runs() {
    const N: u32 = 10_000;
    let t = full_test();
    let hold = |r: &Run| t.run_holds(r);
    let mut parts: BTreeMap<u32, Syn> = BTreeMap::new();
    let mut state = StatsState::default();
    let mut bound_merges = 0usize;
    for p in 0..N {
        parts.insert(p, syn(p));
        let mut refold = |_: &[PartIdent]| -> WriteResult<Vec<PartStats>> { unreachable!("no retires") };
        let plan = plan_stats(&state, u64::from(p) + 1, 7, FP, vec![part_stats(&parts[&p])], &[], &mut refold, &hold).unwrap();
        let written: Vec<u32> = plan.stage.iter().map(|(r, _)| r.nparts).filter(|&n| n > 0).collect();
        assert!(written.iter().filter(|&&n| n as usize > LOOSE_MAX).count() <= 1, "publish {p}: at most one big run written: {written:?}");
        bound_merges += written.iter().filter(|&&n| n as usize == RUN_PARTS_MAX).count();
        state = plan.state;
        let small = state.runs.iter().filter(|r| r.parts.len() <= LOOSE_MAX).count();
        let big = state.runs.len() - small;
        assert!(state.runs.iter().all(|r| hold(r) || r.parts.len() > LOOSE_MAX), "publish {p}: every small run is held");
        assert!(small <= HELD_SMALL_MAX && state.loose.len() < LOOSE_MAX, "publish {p}: {small} small runs, {} loose", state.loose.len());
        assert!(big <= (p as usize + 1) / RUN_PARTS_MAX, "publish {p}: {big} big runs");
    }
    let n = N as usize;
    assert_eq!(bound_merges, (n - LOOSE_MAX * (HELD_SMALL_MAX + 1)) / RUN_PARTS_MAX + 1, "the bound merges once per 256 publishes past 272");
    assert!(state.runs.len() <= n / RUN_PARTS_MAX + HELD_SMALL_MAX, "{} runs at N = {N} (N / 16 = {} without the bound)", state.runs.len(), n / LOOSE_MAX);
    let m = manifest(u64::from(N), &parts, N, state.root.as_ref());
    state.check(&m).unwrap();
    let all = |_: &pgrc_format::facts::ColFacts| true;
    let mut fold = FactsAcc::new();
    for s in parts.values() {
        fold.fold_part(&part_facts(s)).unwrap();
    }
    let h = m.header;
    let walk = fold.into_image(h.gen, h.publisher_fxid, h.schema_fingerprint).unwrap();
    let img = state.image(&m).unwrap();
    assert_eq!(img.header, walk.header);
    assert_eq!(serve(&img, &all).unwrap(), serve(&walk, &all).unwrap(), "stats == the ordered fold, zones included");
}

/// D4537 T18b: a shared stats object (a loose part, a run) is the part cache's bytes from the point it is shared, one
/// charge for every generation that holds the `Arc`; a clone is a new, unshared object (no charge).
#[test]
fn a_shared_stats_object_is_on_the_part_cache() {
    let p = part_stats(&syn(5));
    assert_eq!(p.hold.bytes(), 0, "unshared: no charge");
    let a = p.clone().into_arc();
    assert!(a.hold.bytes() >= (std::mem::size_of::<PartStats>() + 16) as u64, "the Arc block and the object are the cache's");
    assert_eq!(a.clone().hold.bytes(), a.hold.bytes(), "a second count is the same charge");
    let b = a.as_ref().clone();
    assert_eq!(b.hold.bytes(), 0, "a clone is a new, unshared object");
    assert_eq!(*a, b, "the charge is no part of the object's identity");
}
