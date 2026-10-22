//! Per-entry counters for folds over a join, road `join.mult` (H20, R5-L03: matches nobody tells apart). Budget
//! 160 lines. Owner M1-JOIN.
//!
//! When nothing after an inner or LEFT probe tells the matches of a row apart, the probe hands the row on ONCE with the
//! number of build records its key matches as `Env::mult`, and the end folds it as that many copies (`EndMech::takes_mult`:
//! count += m, sum += x * m); a LEFT probe's row without a match goes on once, its payload NULL (AGA-31: the fill's miss
//! path, walk.rs `lower_mult`). Exact only when (all checked at make time): the end takes it; the probe has no join qual; the
//! pipeline reads no build column after the probe (the shape's payload is empty); every op after the probe is a filter,
//! gate or projection that calls nothing VOLATILE (a STABLE or IMMUTABLE user call does not act, DECISIONS 3 C1; AGA-05),
//! so every copy passes or fails alike and no op counts rows.
//! The build collapses equal keys at lay-out (`collapse`, `bump`): one record per key, its count in the link word's high
//! half (the payload-NULL bits, unused without a payload), so a match costs one load in every form and a dense table stays
//! dense when keys repeat.
use gravity_decompose::Op;
use gravity_expr::emit::hash::emit_image_eq_at;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::GrowIn;

use crate::admit::refusal::Variant;
use crate::effects::visible::hashjoin::{copies_alike, past_volatile};
use crate::face::end::EndMech;
use crate::table::keyimg::ImageWords;
use crate::frame::env::Env;
use crate::frame::px::Px;

use crate::ops::hashjoin::build::LOW;
use crate::ops::hashjoin::rt::{rd, wr, Rt};
use crate::ops::hashjoin::build_side::Shape;
use crate::ops::hashjoin::probe::{rec_at, Hv};

mod raw;
pub(crate) use raw::{bump, collapse};

/// The road's value: `candidate` / `decide` choose the form, `collapse` / `bump` lay the counts, `weight` reads them.
pub(crate) struct Mult;

crate::strategies! {
    /// Per-entry counters for folds over a join.
    JOIN_MULT = 51, "join.mult", JoinFold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(crate::face::strategy::JOIN_MULT, &Mult)], ..crate::ops::NONE };

/// Whether probe op `j` of pipeline `k` (with an empty join qual: `qual_empty`) over build `sh` may take the form, as far
/// as the ops after it tell (the end answers later, `decide`).
pub(crate) fn candidate(px: &Px<'_, '_>, k: usize, j: usize, qual_empty: bool, sh: &Shape) -> bool {
    crate::face::source::on(Variant::Strategy(JOIN_MULT))
        && qual_empty
        && sh.payload.is_empty()
        && px.gx.pipelines()[k].ops[j + 1..].iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => copies_alike(px.gx, *n),
            Op::Project(l) => l.iter().all(|n| copies_alike(px.gx, n)),
            _ => false,
        })
        && !past_volatile(px.gx, k, j)
}

/// The form holds when the probe is a `candidate` and its pipeline's `end` takes a multiplicity; the build learns it.
pub(crate) fn decide(sh: &Shape, candidate: bool, end: &dyn EndMech) -> bool {
    let on = candidate && end.takes_mult();
    sh.mult.set(sh.mult.get() || on);
    on
}

/// A record's count of equal keys (`I64`, >= 1): the link word's high half plus one.
fn count(b: &mut FunctionBuilder<'_>, env: &Env, rec: Value, link_off: i32) -> Value {
    let link = b.load(Type::I64, rec, link_off);
    let s = b.iconst(Type::I64, 32);
    let hi = b.int_bin(IntBinOp::Ushr, link, s);
    b.int_bin(IntBinOp::Add, hi, env.one)
}

/// The multiplicity of image `img` in the build (`I64`, 0: no match) from its first candidate record `first` (0: none).
/// `exact`: the form's candidate is the key's own record (dense); otherwise the chain is walked to the equal image,
/// which after `collapse` is the only one.
pub(crate) fn weight(b: &mut FunctionBuilder<'_>, env: &mut Env, hv: &Hv, img: &ImageWords, first: Value, exact: Value, sh: &Shape) -> Value {
    let l_walk = b.create_block(&[Type::I64]);
    let l_out = b.create_block(&[Type::I64]);
    b.br(l_walk, &[first]);
    b.switch_to(l_walk);
    b.mark_loop_header(l_walk);
    env.bounded.push_in(l_walk);
    let p = b.block_params(l_walk)[0];
    let none = b.icmp(IntCond::Eq, p, env.zero);
    let l_rec = b.create_block(&[]);
    b.cond_br(none, (l_out, &[env.zero]), (l_rec, &[]));
    b.switch_to(l_rec);
    let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let eq = emit_image_eq_at(b, &img.words, rec, 0);
    let hit = b.int_bin(IntBinOp::Or, eq, exact);
    let (l_hit, l_next) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(hit, (l_hit, &[]), (l_next, &[]));
    b.switch_to(l_hit);
    let w = count(b, env, rec, sh.link_off());
    b.br(l_out, &[w]);
    b.switch_to(l_next);
    let link = b.load(Type::I64, rec, sh.link_off());
    let low = b.iconst(Type::I64, LOW as i64);
    let n1 = b.int_bin(IntBinOp::And, link, low);
    let np = rec_at(b, env, hv, n1, sh.rec.get());
    let nz = b.icmp(IntCond::Eq, n1, env.zero);
    let next = b.select(nz, env.zero, np);
    b.br(l_walk, &[next]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::hash::{Depth, KeyHash};

    use crate::ops::hashjoin::build::{A_DENSE, A_MULT, A_TAGS, ALLOW, BITS, DENSE, DIR, FLAGS, FORM, F_RANGE, HASH, HI, IDX, LO, LOW, N, PHASE, REC, SLOTS, TOP, WORDS};
    use crate::ops::hashjoin::rt::{lay_out, rd, wr};

    /// One-word keys laid out under `allow`: the form, and each probe key's count (0: no match) as the probe reads it.
    fn counts(keys: &[i64], allow: u8, probes: &[i64]) -> (u8, Vec<u64>) {
        let len = 1usize << 16;
        let mut buf = vec![0u64; len / 8];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region; records of 16 bytes (the key word, the link word) from HDR.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 16), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, TOP, len as u64));
            (wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            keys.iter().enumerate().for_each(|(i, &k)| wr::<i64>(d, crate::ops::hashjoin::build::HDR + 16 * i, k));
            lay_out(d, len).unwrap();
            let (form, lo) = (rd::<u8>(d, FORM), rd::<i64>(d, LO));
            let rec = |i: u64| d.add(crate::ops::hashjoin::build::HDR + 16 * (i as usize - 1));
            let of = |k: i64| match form {
                DENSE if (k - lo) as u64 >= (rd::<i64>(d, HI) - lo + 1) as u64 => 0,
                DENSE if rd::<u64>(d, rd::<u64>(d, DIR) as usize + 8 * ((k - lo) as usize >> 6)) >> ((k - lo) & 63) & 1 == 0 => 0,
                DENSE => (rd::<u64>(d, rd::<u64>(d, SLOTS) as usize + 8 * (k - lo) as usize) >> 32) + 1,
                _ => {
                    let h = KeyHash::of(&[k as u64]);
                    let mut i1 = rd::<u64>(d, rd::<u64>(d, DIR) as usize + 8 * h.home(rd::<u32>(d, BITS), Depth::TOP)) & IDX;
                    while i1 != 0 && rd::<i64>(rec(i1), 0) != k {
                        i1 = rd::<u64>(rec(i1), 8) & LOW;
                    }
                    if i1 == 0 { 0 } else { (rd::<u64>(rec(i1), 8) >> 32) + 1 }
                }
            };
            (form, probes.iter().map(|&k| of(k)).collect())
        }
    }

    #[test]
    fn repeated_keys_collapse_into_counts_in_both_forms() {
        assert_eq!(counts(&[5, 7, 5, 5, 9], A_DENSE | A_MULT, &[5, 6, 7, 9, 10]), (DENSE, vec![3, 0, 1, 1, 0]));
        assert_eq!(counts(&[1, 1_000_000, 1, 42, 1_000_000], A_DENSE | A_MULT | A_TAGS, &[1, 42, 1_000_000, 7]), (HASH, vec![2, 1, 2, 0]));
    }
}
