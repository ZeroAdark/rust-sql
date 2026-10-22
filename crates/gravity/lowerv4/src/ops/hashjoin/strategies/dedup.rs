//! Copies of a build record dropped for a consumer that never counts them, road `join.dedup` (D4821: a DISTINCT over a
//! dimension probing a fact x dimension build whose fact rows repeat the same (key, payload), e.g. one order's lines).
//! Budget 220 lines. Owner M1-JOIN.
//!
//! When the pipeline after an INNER probe hands each joined row to an end that keeps one copy of a row whatever its count
//! (`EndMech::takes_set`: a hash aggregate keyed by its whole output with no aggregate, a DISTINCT), two build records equal
//! in every byte the probe reads (image words, the payload NULL bits, the payload words) give one probe row the same joined
//! row twice, and the end keeps it once either way. The lay-out then chains one of them (`raw::seen`, `raw::same`): the
//! probe walks and hands on fewer records, the end folds fewer rows, the answer is the same set. Exact only when (all
//! checked at make time, `candidate`): the probe is INNER, not a catalog or nest probe, its build not swapped and probed by
//! this probe alone; every op after the probe is a filter, gate or projection that calls nothing VOLATILE (so every copy
//! passes, fails or raises alike); no op counts rows; the frame counts no rows (EXPLAIN ANALYZE would show the dropped
//! copies' absence). join.mult wins where it holds (one record per key, its count). A by-reference payload compares by
//! its offset, so equal values in two images stay two records (never a wrong drop). The walk is bounded (`raw::SEEN`): a
//! copy past it stays, which only costs the copy.
//!
//! D4904, the SEMI / ANTI widening (`keys`, switch `PGRUST_GRAVITY_SEMI_KEYS`, on unless 0): a SEMI or ANTI probe that reads
//! no build column past its key test asks only whether its row's key has a record, so its build keeps one record per image:
//! before the build grows or spills, and before its merge spills, it drops each record whose image words another record
//! holds (`raw::compact`). A build whose keys repeat (the fact side of an EXISTS / NOT EXISTS over a dimension key) then
//! fits its grant where it spilled.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use types_nodes::jointype::JoinType;

use crate::admit::refusal::Variant;
use crate::effects::visible::hashjoin::{copies_alike, past_volatile};
use crate::face::end::EndMech;
use crate::frame::px::Px;
use crate::ops::hashjoin::build_side::Shape;

mod raw;
pub(crate) use raw::{compact, same_slot, seen};

/// The road's value (`candidate` / `decide` choose it, the lay-outs drop the copies).
pub(crate) struct Dedup;

crate::strategies! {
    /// Copies of a build record dropped for a consumer that never counts them.
    JOIN_DEDUP = 49, "join.dedup", JoinFold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JOIN_DEDUP, &Dedup)], ..crate::ops::NONE };

/// `Shape::dedup`: no, a candidate (make), on (the end takes it: `decide`), one record per key (a SEMI / ANTI probe: `keys`).
pub(crate) const CAND: u8 = 1;
pub(crate) const ON: u8 = 2;
pub(crate) const KEYS: u8 = 3;

static SEMI_KEYS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// D4904's switch (exec knobs `PGRUST_GRAVITY_SEMI_KEYS`, read once where the lowerer is registered).
pub(crate) fn set_keys(on: bool) {
    SEMI_KEYS.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Build `sh` is probed by one pipeline of the graph alone and by no nest inner.
fn lone(px: &Px<'_, '_>, sh: &Shape) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    let probes_slot = |p: &gravity_decompose::PipelineNode<'_>| p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == sh.slot));
    let probes = ps.iter().enumerate().filter(|(q, p)| probes_slot(p) && gx.life_of(*q, sh.slot).is_some_and(|l| l.0 as usize == sh.build)).count();
    probes == 1 && !ps.inners().into_iter().any(|(_, p)| probes_slot(p))
}

/// Whether probe op `j` of pipeline `k` over build `sh` may drop its build's copies, as far as the probe and the ops after
/// it tell (the end answers later, `decide`); records the answer on the shape.
pub(crate) fn candidate(px: &Px<'_, '_>, k: usize, j: usize, jt: JoinType, catalog: bool, sh: &Shape) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    let ok = crate::face::source::on(Variant::Strategy(JOIN_DEDUP))
        && jt == JoinType::JOIN_INNER
        && !catalog
        && !sh.swapped
        && k < ps.len()
        && !crate::frame::instr::on()
        && ps[k].ops[j + 1..].iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => copies_alike(gx, *n),
            Op::Project(l) => l.iter().all(|n| copies_alike(gx, n)),
            _ => false,
        })
        && !past_volatile(gx, k, j)
        // the build's probes: this one alone among the graph's pipelines, none in a nest's inner
        && lone(px, sh);
    if ok {
        sh.dedup.set(CAND);
    }
    ok
}

/// D4904: whether SEMI / ANTI probe `jt` of pipeline `k` (`qual_empty`: no join qual and no hash clause it re-checks on a
/// record, `confirms`) over build `sh` lets the build keep one record per image; records the answer on the shape (`KEYS`).
/// Image-equal is the probe's own match, so two records of one image answer every probe row alike, and a key equal under
/// its operator but imaged apart (a cross-type or collated one) stays two records. Exact only when: the probe is SEMI or
/// ANTI (a RIGHT one marks or fills the build), reads no build column (an empty payload), is not a catalog or nest probe,
/// its build not swapped and probed by this probe alone; the frame counts no rows (EXPLAIN ANALYZE counts the Hash's
/// rows). A NULL key never reaches the build (only a probe that fills the build keeps one).
pub(crate) fn keys(px: &Px<'_, '_>, k: usize, jt: JoinType, qual_empty: bool, catalog: bool, sh: &Shape) -> bool {
    let ok = SEMI_KEYS.load(core::sync::atomic::Ordering::Relaxed)
        && matches!(jt, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
        && qual_empty
        && sh.payload.is_empty()
        && !catalog
        && !sh.swapped
        && k < px.gx.pipelines().len()
        && !crate::frame::instr::on()
        && lone(px, sh);
    if ok {
        sh.dedup.set(KEYS);
    }
    ok
}

/// The road holds when the probe is a `candidate`, join.mult does not (`mult`), and the pipeline's `end` keeps one copy of
/// a row whatever its count; the build learns it (build.rs `stamp`: `F_DEDUP`).
pub(crate) fn decide(sh: &Shape, mult: bool, end: &dyn EndMech) -> bool {
    let on = sh.dedup.get() == CAND && !mult && end.takes_set();
    if on {
        sh.dedup.set(ON);
        fires::fire("lower.join.dedup"); // shapecheck fire signal: a build lays its copies once
    }
    on
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::hash::{Depth, KeyHash};

    use crate::ops::hashjoin::build::{ALLOW, A_DENSE, A_TAGS, BITS, DCHAIN, DENSE, DIR, FLAGS, FORM, F_DEDUP, F_RANGE, HASH, HDR, HI, IDX, LO, LOW, N, PAY, PHASE, REC, SLOTS, TOP, WORDS};
    use crate::ops::hashjoin::rt::{lay_out, rd, wr};

    /// Records (one-word key, its payload NULL bit, one payload word) laid out under `allow` and `flags`: the form, and per
    /// probe key the payloads (`-1`: the NULL bit) of the records its walk reaches, sorted.
    fn reached(recs: &[(i64, bool, i64)], allow: u8, flags: u8, probes: &[i64]) -> (u8, Vec<Vec<i64>>) {
        let len = 1usize << 16;
        let mut buf = vec![0u64; len / 8];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region; records of 24 bytes (the key word, the link word, the payload word) from HDR.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 24), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, flags), wr::<u8>(d, PAY, 1));
            (wr::<u64>(d, N, recs.len() as u64), wr::<u64>(d, TOP, len as u64));
            (wr::<i64>(d, LO, recs.iter().map(|r| r.0).min().unwrap()), wr::<i64>(d, HI, recs.iter().map(|r| r.0).max().unwrap()));
            let rec = |i: usize| d.add(HDR + 24 * i);
            for (i, &(k, null, v)) in recs.iter().enumerate() {
                (wr::<i64>(rec(i), 0, k), wr::<u64>(rec(i), 8, if null { 1 << 32 } else { 0 }), wr::<i64>(rec(i), 16, v));
            }
            lay_out(d, len).unwrap();
            let (form, lo) = (rd::<u8>(d, FORM), rd::<i64>(d, LO));
            let pay = |link: u64, v: i64| if link >> 32 & 1 != 0 { -1 } else { v };
            let walk = |mut i1: u64, k: i64| {
                let mut out = Vec::new();
                while i1 != 0 {
                    let r = rec(i1 as usize - 1);
                    if rd::<i64>(r, 0) == k {
                        out.push(pay(rd::<u64>(r, 8), rd::<i64>(r, 16)));
                    }
                    i1 = rd::<u64>(r, 8) & LOW;
                }
                out
            };
            let of = |k: i64| -> Vec<i64> {
                let off = (k - lo) as usize;
                let mut v = match form {
                    DENSE if rd::<u64>(d, rd::<u64>(d, DIR) as usize + 8 * (off >> 6)) >> (off & 63) & 1 == 0 => vec![],
                    DENSE => {
                        let s = d.add(rd::<u64>(d, SLOTS) as usize + 16 * off);
                        vec![pay(rd::<u64>(s, 0), rd::<i64>(s, 8))]
                    }
                    DCHAIN => walk(rd::<u32>(d, rd::<u64>(d, DIR) as usize + 4 * off) as u64, k),
                    _ => walk(rd::<u64>(d, rd::<u64>(d, DIR) as usize + 8 * KeyHash::of(&[k as u64]).home(rd::<u32>(d, BITS), Depth::TOP)) & IDX, k),
                };
                v.sort_unstable();
                v
            };
            (form, probes.iter().map(|&k| of(k)).collect())
        }
    }

    /// Copies (key, NULL bit and payload equal) chain once in every form; a record that differs in its payload or its NULL
    /// bit stays; without `F_DEDUP` every record chains.
    #[test]
    fn copies_chain_once_in_every_form() {
        let (on, off) = (F_RANGE | F_DEDUP, F_RANGE);
        assert_eq!(reached(&[(5, false, 1), (5, false, 1), (7, false, 2), (5, false, 1)], A_DENSE, on, &[5, 6, 7]), (DENSE, vec![vec![1], vec![], vec![2]]));
        assert_eq!(reached(&[(5, false, 1), (5, false, 2), (5, false, 1), (7, false, 3), (5, true, 1)], A_DENSE, on, &[5, 7]), (DCHAIN, vec![vec![-1, 1, 2], vec![3]]));
        assert_eq!(reached(&[(5, false, 1), (5, false, 2), (5, false, 1), (7, false, 3)], A_DENSE, off, &[5, 7]), (DCHAIN, vec![vec![1, 1, 2], vec![3]]));
        let wide = [(1, false, 9), (1_000_000, false, 8), (1, false, 9), (1_000_000, false, 7), (42, false, 9), (1_000_000, false, 8)];
        assert_eq!(reached(&wide, A_DENSE | A_TAGS, on, &[1, 42, 1_000_000, 3]), (HASH, vec![vec![9], vec![9], vec![7, 8], vec![]]));
        assert_eq!(reached(&wide, A_DENSE | A_TAGS, off, &[1, 1_000_000]), (HASH, vec![vec![9, 9], vec![7, 8, 8]]));
    }

    /// A chain walk compares a new record with `raw::SEEN` kept records at most: a copy past them chains (only its cost).
    #[test]
    fn the_walk_is_bounded() {
        let mut recs: Vec<(i64, bool, i64)> = (0..super::raw::SEEN as i64 + 1).map(|v| (1_000_000, false, v)).collect();
        recs.push((1_000_000, false, 0));
        recs.push((1, false, 0));
        let (form, got) = reached(&recs, A_TAGS, F_RANGE | F_DEDUP, &[1_000_000]);
        assert_eq!((form, got[0].len()), (HASH, super::raw::SEEN + 2));
    }
    /// D4904: a keys-only build (`F_KEYS`, no payload) keeps the first record of each image, in order, one- or two-word
    /// images alike; a pass that keeps more than three quarters is marked done and none runs after it; a build without
    /// `F_KEYS` is left as it is.
    #[test]
    fn a_keys_only_build_keeps_one_record_per_image() {
        use crate::ops::hashjoin::build::F_KEYS;
        use crate::ops::hashjoin::spill::build::KEYS_DONE;
        let a = gravity_fn::arena::OpArena::scratch_of(None, gravity_fn::arena::Kind::HashBuild);
        let len = 1usize << 20;
        // the records' images after `super::compact` (None: no pass), whether it marked the pass done
        let run = |keys: &[Vec<i64>], flags: u8| -> (Option<usize>, Vec<Vec<i64>>, bool) {
            let words = keys[0].len();
            let rec = 8 * (words + 1);
            let mut buf = vec![0u64; len / 8];
            let d = buf.as_mut_ptr() as *mut u8;
            // SAFETY: `buf` is the region; records of `rec` bytes (the image words, the link word) from HDR, no payload.
            unsafe {
                (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, words as u8), wr::<u32>(d, REC, rec as u32), wr::<u8>(d, FLAGS, flags), wr::<u8>(d, PAY, 0));
                (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, TOP, len as u64));
                for (i, k) in keys.iter().enumerate() {
                    for (w, &v) in k.iter().enumerate() {
                        wr::<i64>(d.add(HDR + rec * i), 8 * w, v);
                    }
                }
                let got = super::compact(&a, d, len, keys.len());
                let n = rd::<u64>(d, N) as usize;
                let kept = (0..n).map(|i| (0..words).map(|w| rd::<i64>(d.add(HDR + rec * i), 8 * w)).collect()).collect();
                (got, kept, rd::<u8>(d, KEYS_DONE) != 0)
            }
        };
        let one = |v: &[i64]| v.iter().map(|&k| vec![k]).collect::<Vec<_>>();
        assert_eq!(run(&one(&[5, 7, 5, 5, 9, 7, 1, 5]), F_KEYS | F_RANGE), (Some(4), one(&[5, 7, 9, 1]), false));
        let two = [vec![1, 2], vec![1, 3], vec![1, 2], vec![2, 1], vec![1, 3], vec![2, 1]];
        assert_eq!(run(&two, F_KEYS), (Some(3), vec![vec![1, 2], vec![1, 3], vec![2, 1]], false));
        assert_eq!(run(&one(&[1, 2, 3, 4, 5, 6, 7, 1]), F_KEYS | F_RANGE), (Some(7), one(&[1, 2, 3, 4, 5, 6, 7]), true));
        assert_eq!(run(&one(&[5, 7, 5, 5]), F_RANGE), (None, one(&[5, 7, 5, 5]), false));
        let many: Vec<i64> = (0..10_000).map(|i| (i * 7919) % 300).collect();
        let (got, kept, _) = run(&one(&many), F_KEYS | F_RANGE);
        assert_eq!((got, kept.len()), (Some(300), 300));
        assert!(kept.iter().enumerate().all(|(i, k)| k[0] == many[i]), "the first of each image, in order");
    }
}
