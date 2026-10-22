//! The condition cache (H02i, `verdict.cond`; priors pgrcolumnar condcache.rs and the labs' condcache.rs): a leading
//! filter's per-granule survivor words kept across statements by the granule host (hosts `cond_cache`), keyed by the
//! quals' fingerprint and the part's uuid. A hit is `NoMatch` (the host skips the granule), `AllMatch` or the words as the
//! selection (the window is covered, `WinConds::covered`); a miss evaluates and the frame calls `passed` for each row that
//! passed every covered step, whose bit this file sets in the window's verdict lane (`LaneId::Verdict(0)`), published by
//! the host when the window was consumed whole. Which quals the cache takes and their shapes (the CONDITION CACHE
//! CORRECTNESS RULE) are verdict_cond_key.rs; `finish` fingerprints the covered quals' shapes under the
//! process seed (dict fp, the registered Fp128 producer): no seed, no cache. The verdict lane follows the table lanes
//! (pgrc's `SourceMech::seal`). Registers road `verdict.cond`. Owner M1-SOURCE.
use gravity_abi::ctx::lane;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::SiteDesc;
use mcx::{GrowIn, Mcx, PgVec};

use crate::ops::scan::pgrc::verdict_cond_key::{put, Shape};
use crate::face::source::{VerdictImpl, VerdictMech};
use crate::frame::px::{ExId, Px};
use crate::ops::Table;

crate::strategies! {
    /// Condition cache: a qual's per-granule survivor words kept across statements, keyed (fingerprint, part uuid,
    /// granule); hit = NoMatch / AllMatch / the window mask, miss = evaluate and publish; only quals whose every
    /// function and operator is IMMUTABLE consult or fill it (fingerprint includes constants' types / typmods and
    /// collations; visibility and delete vectors applied after); pgrc / pgrcolumnar parts only, never the heap.
    VERDICT_COND = 16, "verdict.cond", PgrcFilter;
}

pub(crate) const ROWS: Table = Table { strategies: &[(VERDICT_COND, &VerdictImpl(&Cond))], ..crate::ops::NONE };

pub(crate) struct Cond;

impl VerdictMech for Cond {
    fn declare(&self, px: &mut Px<'_, '_>, _site: &mut SiteDesc, qual: ExId) -> bool {
        crate::ops::scan::pgrc::verdict_cond_key::shape_of(px, qual).is_some()
    }
}

/// One run of the covered quals' bind words, as `src.verdicts` carries it in a proto's `words` and `peer`: `n` words from
/// `base`, word `i`'s varlena bit `mask >> min(i, 15) & 1` (exec `Verdicts` reads it back by the same rule).
pub(crate) type Run = (u16, u16, u16);

/// The bit of `mask` that word `i` of a run reads: its own for the first 16 words, bit 15 for every later one.
const fn bit(i: usize) -> usize {
    if i < 15 { i } else { 15 }
}

/// The covered words in need order (any number, any order, repeats kept) as runs: a run grows while the next word is
/// its next index, fits a u16 count, and, from its 17th word on, has its 16th word's varlena flag. Every word is in exactly one
/// run at its position, so the runs read back in order are the words.
fn runs<'m>(words: impl IntoIterator<Item = (u16, bool)>, mcx: Mcx<'m>) -> PgVec<'m, Run> {
    let mut out: PgVec<'m, Run> = PgVec::new_in(mcx);
    for (w, varlena) in words {
        if let Some((base, n, mask)) = out.last_mut() {
            let i = *n as usize;
            let next = w as usize == *base as usize + i && *n < u16::MAX;
            if next && (i <= 15 || (*mask >> 15 & 1 != 0) == varlena) {
                *mask |= (varlena as u16) << bit(i);
                *n += 1;
                continue;
            }
        }
        out.push_in((w, 1, varlena as u16));
    }
    out
}

/// The covered quals' fingerprint inputs: their bind words as runs (`runs`) and the static shape fingerprint. The key is
/// the canonical predicate's fingerprint (the graph's filters are normalize's form; the shape walk is
/// verdict_cond_key.rs) plus the hashed vector of every covered constant: no cap on the words, no rule on their order.
/// D3938 (C3's ninth cache): and what made the verdicts, `made`: the stamp of the catalog reads the shapes rest on
/// (gravity_deps `committed_stamp`: a moved function or type row is another key) and the quals' producers
/// (verdict_cond_key.rs `producer`: their per-code tables, a fill expression's fingerprint among them), so an entry is
/// served only to a statement whose quals are made as its own were. `None` only before the process seed.
pub(crate) fn finish<'m>(shapes: &[Shape<'_>], relid: u32, site: &SiteDesc, wzone: &[(u64, u16)], made: (u128, &[u8]), mcx: Mcx<'m>) -> Option<(PgVec<'m, Run>, [u64; 2])> {
    let mut b = mcx::to_vec_in(&b"gravity.v4.cond.4"[..], mcx); // V4R-33: .3 = entries carry survivor counts; D3938: .4 = keyed by what made them
    put(&mut b, b'r', &[relid as u64, shapes.len() as u64]);
    put(&mut b, b'd', &[made.0 as u64, (made.0 >> 64) as u64, made.1.len() as u64]);
    b.extend_from_slice_in(made.1);
    for s in shapes {
        b.extend_from_slice_in(&(s.bytes.len() as u32).to_le_bytes());
        b.extend_from_slice_in(&s.bytes);
    }
    for z in site.zone_preds() {
        put(&mut b, b'z', &[z.lane as u64, z.op as u64 | (z.prefix as u64) << 8, z.value as u64]);
    }
    // D289: the word-valued zone predicates by lane, op, length and word (their values are the bound words hashed above)
    for &(arg, w) in wzone {
        put(&mut b, b'w', &[arg, w as u64]);
    }
    let fp = gravity_dict::fp::fp128(&b)?;
    Some((runs(shapes.iter().flat_map(|s| s.words.iter().copied()), mcx), if fp == [0, 0] { [1, 0] } else { fp }))
}

fn lane_ptr(b: &mut FunctionBuilder<'_>, lanes: Value, k: usize) -> (Value, Value) {
    let p = b.load(Type::Ptr, lanes, (k * lane::SIZE + lane::PTR) as i32);
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let z = b.iconst(Type::I64, 0);
    (p, b.icmp(IntCond::Ne, pi, z))
}

/// D3161: a window's fill of its verdict words, loaded once by `next_window`: the words (`p`) and whether a row that
/// passed every covered step records its bit (`go`: the lane is there and the window is a miss; a hit's cached words
/// are never written). Per row the record was a reload of the lane's pointer and flags and a test before the store.
#[derive(Clone, Copy)]
pub(crate) struct Fill {
    p: Value,
    go: Value,
}

/// The window's cache hit (the verdict lane carries the cached words, `ALL_MATCH`; the selection is the survivors) and
/// its `Fill`, from the verdict lane `k` of the lanes at `lanes`.
pub(crate) fn window(b: &mut FunctionBuilder<'_>, lanes: Value, k: usize) -> (Value, Fill) {
    let (p, has) = lane_ptr(b, lanes, k);
    let all = crate::ops::scan::pgrc::verdict_allmatch::covered(b, lanes, k);
    let t = b.bconst(true);
    let miss = b.int_bin(IntBinOp::Xor, all, t);
    let go = b.int_bin(IntBinOp::And, has, miss);
    (b.int_bin(IntBinOp::And, has, all), Fill { p, go })
}

/// `set(words)` when the window fills its words.
fn or_words(b: &mut FunctionBuilder<'_>, f: Fill, set: &dyn Fn(&mut FunctionBuilder<'_>, Value)) {
    let (l_set, l_done) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(f.go, (l_set, &[]), (l_done, &[]));
    b.switch_to(l_set);
    set(b, f.p);
    b.br(l_done, &[]);
    b.switch_to(l_done);
}

fn or_word(b: &mut FunctionBuilder<'_>, p: Value, w: Value, m: Value) {
    let at = b.ptr_add(p, w, 8, 0);
    let old = b.load(Type::I64, at, 0);
    let new = b.int_bin(IntBinOp::Or, old, m);
    b.store(at, 0, new);
}

/// Row `i` of the window passed every covered step.
pub(crate) fn record(b: &mut FunctionBuilder<'_>, f: Fill, i: Value) {
    or_words(b, f, &|b, p| {
        let six = b.iconst(Type::I64, 6);
        let w = b.int_bin(IntBinOp::Ushr, i, six);
        let m63 = b.iconst(Type::I64, 63);
        let sh = b.int_bin(IntBinOp::And, i, m63);
        let one = b.iconst(Type::I64, 1);
        let bit = b.int_bin(IntBinOp::Shl, one, sh);
        or_word(b, p, w, bit);
    });
}

/// Rows `base + i` for each bit `i` of `m` passed every covered step (a block of the plain window: no selection).
pub(crate) fn record_block(b: &mut FunctionBuilder<'_>, f: Fill, base: Value, m: Value) {
    or_words(b, f, &|b, p| {
        let six = b.iconst(Type::I64, 6);
        let w = b.int_bin(IntBinOp::Ushr, base, six);
        let m63 = b.iconst(Type::I64, 63);
        let sh = b.int_bin(IntBinOp::And, base, m63);
        let lo = b.int_bin(IntBinOp::Shl, m, sh);
        or_word(b, p, w, lo);
        // the high part of a block straddling two words (a resumed window's base need not be a multiple of 64)
        let z = b.iconst(Type::I64, 0);
        let straddles = b.icmp(IntCond::Ne, sh, z);
        let (l_hi, l_end) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(straddles, (l_hi, &[]), (l_end, &[]));
        b.switch_to(l_hi);
        let sixty4 = b.iconst(Type::I64, 64);
        let rsh = b.int_bin(IntBinOp::Sub, sixty4, sh);
        let hi = b.int_bin(IntBinOp::Ushr, m, rsh);
        let one = b.iconst(Type::I64, 1);
        let w1 = b.int_bin(IntBinOp::Add, w, one);
        let nz = b.icmp(IntCond::Ne, hi, z);
        let (l_st, l_skip) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(nz, (l_st, &[]), (l_skip, &[]));
        b.switch_to(l_st);
        or_word(b, p, w1, hi);
        b.br(l_skip, &[]);
        b.switch_to(l_skip);
        b.br(l_end, &[]);
        b.switch_to(l_end);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_program::{Arena, ArenaRef, ZoneOp, ZonePred};

    fn shape<'g>(mcx: mcx::Mcx<'g>, bytes: &[u8], words: &[(u16, bool)]) -> Shape<'g> {
        Shape { bytes: mcx::to_vec_in(bytes, mcx), words: mcx::to_vec_in(words, mcx) }
    }

    /// The words a run list reads back in order, each with its varlena bit (exec `Verdicts`' reading of the rule).
    fn read_back(runs: &[Run]) -> Vec<(u16, bool)> {
        runs.iter().flat_map(|&(base, n, mask)| (0..n).map(move |i| (base + i, mask >> bit(i as usize) & 1 != 0))).collect()
    }

    #[test]
    fn any_words_in_any_order_are_runs_that_read_back_as_the_words() {
        gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let ctx = mcx::MemoryContext::new("verdict-cond-runs");
        let m = ctx.mcx();
        let site = SiteDesc::none_in(ArenaRef::EMPTY);
        // one ascending run of at most 16 is the one run slot 0 carried before: its mask bit i is word i's
        let a = shape(m, b"a", &[(7, false), (8, true)]);
        let b2 = shape(m, b"b", &[(9, false)]);
        assert_eq!(finish(&[a, b2], 1, &site, &[], (0, &[]), m).expect("fp").0, [(7, 3, 0b010)]);
        let cases: [Vec<(u16, bool)>; 8] = [
            Default::default(),
            [(7, false), (9, false)].to_vec(),
            [(9, true), (8, false), (8, false)].to_vec(),
            (0..17).map(|i| (i, false)).collect(),
            (0..40).map(|i| (i, true)).collect(),
            // a by-value word after 15 varlena ones, and a varlena one after 20 by-value ones: a run cuts where bit 15 differs
            (0..15).map(|i| (i, true)).chain([(15, false), (16, true)]).collect(),
            (0..20).map(|i| (i + 3, false)).chain([(23, true), (24, true)]).collect(),
            (0..70).map(|i| (100 - i, i % 3 == 0)).collect(),
        ];
        for words in cases {
            let (runs, _) = finish(&[shape(m, b"a", &words)], 1, &site, &[], (0, &[]), m).expect("every word list keys");
            assert_eq!(read_back(&runs), words, "runs {runs:?}");
        }
        let long: Vec<(u16, bool)> = (0..1000).map(|i| (i, true)).collect();
        assert_eq!(finish(&[shape(m, b"a", &long)], 1, &site, &[], (0, &[]), m).unwrap().0, [(0, 1000, 0xffff)], "a long uniform list is one run");
        let cut: Vec<(u16, bool)> = (0..15).map(|i| (i, true)).chain([(15, false), (16, true)]).collect();
        assert_eq!(runs(cut, m), [(0, 16, 0x7fff), (16, 1, 1)]);
    }

    #[test]
    fn the_static_fingerprint_separates_shapes_relations_and_zone_predicates() {
        gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let ctx = mcx::MemoryContext::new("verdict-cond-fp");
        let m = ctx.mcx();
        let site = SiteDesc::none_in(ArenaRef::EMPTY);
        let fp = |bytes: &[u8], relid: u32, site: &SiteDesc| finish(&[shape(m, bytes, &[(3, false)])], relid, site, &[], (0, &[]), m).unwrap().1;
        // a constant of another type / typmod / collation serializes differently (walk's `C` record), so its shape differs
        let int4 = [b'C', 23, 0, 0, 0];
        let int8 = [b'C', 20, 0, 0, 0];
        assert_ne!(fp(&int4, 1, &site), fp(&int8, 1, &site));
        assert_ne!(fp(&int4, 1, &site), fp(&int4, 2, &site));
        // a site with lanes allocates: a real arena (the empty one fails every allocation)
        let arena = Arena::new(1 << 16);
        let mut zoned = SiteDesc::range_source(arena.handle(), 1, &[1]);
        zoned.add_zone(ZonePred::word(0, ZoneOp::Eq, 5));
        assert_ne!(fp(&int4, 1, &site), fp(&int4, 1, &zoned));
        assert_eq!(fp(&int4, 1, &site), fp(&int4, 1, &site));
        // D3938: one shape made under other catalog reads (a moved function row) or by another producer (other per-code
        // tables, another fill expression) is another key; the same making is the same key
        let made = |stamp: u128, producer: &[u8]| finish(&[shape(m, &int4, &[(3, false)])], 1, &site, &[], (stamp, producer), m).unwrap().1;
        assert_ne!(made(7, b""), made(8, b""));
        assert_ne!(made(1 << 64, b""), made(2 << 64, b""));
        assert_ne!(made(7, b"p\x01"), made(7, b"p\x02"));
        assert_ne!(made(7, b"p"), made(7, b""));
        assert_eq!(made(7, b"p\x01"), made(7, b"p\x01"));
    }
}
