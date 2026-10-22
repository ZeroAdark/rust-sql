//! The build's Bloom words, tested by its probe ahead of the directory, road `join.rowfilter` (D3194). Budget 300 lines.
//! Owner M1-JOIN.
//!
//! A probe whose join drops its unmatched rows (INNER, SEMI, RIGHT, RIGHT SEMI, RIGHT ANTI: never a preserved probe
//! row) asks its build for a register-blocked Bloom filter of its keys (`A_RF`). The HASH lay-out writes it past the
//! KeyHash directory as it chains each record (bloomtag.rs `lay`; the partitioned merge's jobs OR their records' bits
//! in, `put`), so a parallel build's filter is whole when its merge ends and a rerun's build lays its own. A key sets
//! four bits of one 64-bit word: the word is `KeyHash::home` over the words' bits, the bits come from
//! `KeyHash::stored`, the hash the directory's chain is found by. The words stay within half one core's L2 share
//! (`rowfilter_max_bits`): a bigger build takes fewer bits a key, and one past `ROWFILTER_MIN_BITS_PER_KEY` lays none. The probe tests the word after its key's image and before the directory head: a row whose
//! bits are not all set has no record, exactly as a directory miss, so the join's rows never change; a row whose bits
//! are set walks its chain as before (about 1-3 % of the rows a build's keys miss, by the words' size). On the block
//! path, a probe right after the span tests the block's rows first (`block`): the rows it drops decode no late lane and
//! walk no step.
//!
//! The frame versions each window (frame.rs): the body that tests the filter runs only while it is armed (a HASH
//! build that laid words, past the machine's L2 by its directory, and not switched off); every other window runs the
//! plain body. The sample rule: each tested row counts in a frame word (1 a pass, 1 << 32 a reject), folded at each
//! window's start into the region's state line (its seen and passed rows, shared by every participant: plain adds once a
//! window, a lost one only delays the verdict) until `ROWFILTER_SAMPLE` rows are seen; if more than 1 / 2^
//! `ROWFILTER_PASS_SHIFT` of that first sample passed, the state's off word is set and every later window runs the plain
//! body. After the sample the line is only read, and a window that starts past it counts no row (D3700).
//!
//! D3323: road `join.rowfilter.cost` replaces the sample rule by one on measured costs, sampled again through the probe's
//! life (cost.rs); road `join.rowfilter.single` tests an armed filter ahead of a later probe's one-row body (frame.rs).
use gravity_abi::hash::{Depth, KeyHash};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_expr::emit::hash::{emit_home, emit_key_hash, emit_stored, Bits};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_program::Rep;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::cell::At;
use crate::frame::env::Env;
use crate::frame::row::{Mask, RowVals};
use crate::graph::Gx;
use crate::ops::hashjoin::build::{HASH, RF_BITS};
use crate::ops::hashjoin::keys::{image, key_cells, KeySrc, KeyVals};
use crate::ops::hashjoin::probe::Probe;

mod cost;
mod raw;
pub(crate) use raw::{need, open, shared, Words};

/// The road's value: the probe's test and the lay-out's words.
pub(crate) struct RowFilter;

crate::strategies! {
    /// A build's Bloom words, tested by its probe ahead of the directory.
    JOIN_ROWFILTER = 129, "join.rowfilter", Join;
    /// D3323: a window whose later probe could run the one-row body (V4R-27) runs the filtered body first while the
    /// filter is armed.
    JOIN_ROWFILTER_SINGLE = 149, "join.rowfilter.single", Join;
    /// D3323: the filter's verdict from measured costs, each participant sampling it again on one window in
    /// `ROWFILTER_RESAMPLE` (cost.rs).
    JOIN_ROWFILTER_COST = 150, "join.rowfilter.cost", Join;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JOIN_ROWFILTER, &RowFilter), (JOIN_ROWFILTER_SINGLE, &RowFilter), (JOIN_ROWFILTER_COST, &RowFilter)], ..crate::ops::NONE };

/// D3323: whether the filter's verdict comes from measured costs (cost.rs), road `join.rowfilter.cost`.
pub(crate) fn costed() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_ROWFILTER_COST))
}

/// D3323: whether an armed filter runs ahead of a later probe's one-row body (frame.rs), road `join.rowfilter.single`.
pub(crate) fn before_single() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_ROWFILTER_SINGLE))
}

/// The state line ahead of the words (a cache line of its own): the sampled rows seen and passed, and the off word.
pub(crate) const STATE: usize = 64;
const ST_SEEN: i32 = 0;
const ST_PASS: i32 = 8;
const ST_OFF: i32 = 16;
/// A rejected row's count in the frame word (a pass counts 1).
const REJECT: i64 = 1 << 32;

/// Whether probe op `j` of graph pipeline `k` tests its build's filter: the road is on, the probe drops its unmatched rows
/// and is no catalog probe, and it is its pipeline's first probe (one filtered body a window).
pub(crate) fn takes(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    if k >= gx.pipelines().len() || !crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_ROWFILTER)) {
        return false;
    }
    let ops = &gx.pipelines()[k].ops;
    let Some(Op::Join(JoinDesc::Probe { jointype, catalog: false, .. })) = ops.get(j) else { return false };
    let drops = matches!(*jointype, JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER) || crate::ops::shared::scan::drops_unmatched(*jointype);
    drops && ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { .. }))) == Some(j)
}

/// The words' log2 for a build of `n` keys on this machine: `within` half one core's L2 share (`rowfilter_max_bits`).
pub(crate) fn bits_for(n: u64) -> Option<u32> {
    within(n, gravity_program::cost::rowfilter_max_bits(gravity_driver::machine::profile().partition_bytes))
}

/// The words' log2 for `n` keys: at least `ROWFILTER_BITS_PER_KEY` bits a key in a power of two of words (one cache line
/// at least), at most 2^`most` words; `None` when those hold fewer than `ROWFILTER_MIN_BITS_PER_KEY` bits a key (such a
/// build lays no words).
fn within(n: u64, most: u32) -> Option<u32> {
    use gravity_program::cost::{ROWFILTER_BITS_PER_KEY, ROWFILTER_MIN_BITS_PER_KEY};
    let words = n.saturating_mul(ROWFILTER_BITS_PER_KEY).div_ceil(64).max(8);
    let fb = words.checked_next_power_of_two().map_or(most, u64::trailing_zeros).min(most);
    (64u64 << fb >= n.saturating_mul(ROWFILTER_MIN_BITS_PER_KEY)).then_some(fb)
}

/// The bytes a filter of 2^`fb` words takes past the directory: its state line, then the words.
pub(crate) const fn bytes(fb: u32) -> usize {
    STATE + (8 << fb)
}

/// Key hash `h`'s word among 2^`fb` and its four bits there.
pub(crate) fn place(h: KeyHash, fb: u32) -> (usize, u64) {
    let s = h.stored() as u64;
    (h.home(fb, Depth::TOP), (1 << (s & 63)) | (1 << ((s >> 6) & 63)) | (1 << ((s >> 12) & 63)) | (1 << ((s >> 18) & 63)))
}

/// A probe's filter in this window, read at its start (`window`): whether the filtered body runs (`armed`), the words
/// and their log2.
#[derive(Clone, Copy)]
pub(crate) struct Live {
    pub armed: Value,
    pub words: Value,
    pub fb: Value,
    /// D3700: the sample was not yet in at the window's start, so its rows count (`test`); past it nothing reads them.
    pub sampling: Value,
    /// D3323: the verdict is the cost rule's (cost.rs): the block test counts only while the window samples.
    pub cost: bool,
}

/// At a window's start over build region `r` (its `form`, the directory `dir` of 2^`bits` heads): the filter's words, its
/// sample folded from frame word `word` into the region's state line, the off verdict taken, and whether this window
/// tests it. A region without words (another form, no `A_RF`, no room) is never armed and its state never touched.
/// D3323: with the window count's frame word `win` (road `join.rowfilter.cost`) the verdict is cost.rs's, and a filter
/// whose directory fits L2 never touches its state.
pub(crate) fn window(b: &mut FunctionBuilder<'_>, env: &mut Env, (r, form, dir, bits): (Value, Value, Value, Value), (word, win): (i32, Option<i32>)) -> Live {
    let fb = b.load_zext(Type::I8, r, RF_BITS as i32);
    let hash = b.iconst(Type::I64, HASH as i64);
    let is_h = b.icmp(IntCond::Eq, form, hash);
    let some = b.icmp(IntCond::Ne, fb, env.zero);
    let has = b.int_bin(IntBinOp::And, is_h, some);
    let (l_has, l_join) = (b.create_block(&[]), b.create_block(&[Type::Bool, Type::Ptr, Type::Bool]));
    let (f, null) = (b.bconst(false), b.iconst(Type::I64, 0));
    let null = b.conv(ConvOp::IntToPtr, Type::Ptr, null);
    b.cond_br(has, (l_has, &[]), (l_join, &[f, null, f]));
    b.switch_to(l_has);
    let eight = b.iconst(Type::I64, 8);
    let dbytes = b.int_bin(IntBinOp::Shl, eight, bits);
    let state = b.ptr_add(dir, dbytes, 1, 0);
    if let Some(win) = win {
        let words = b.ptr_add(state, env.zero, 1, STATE as i32);
        let (big, l_big) = (crate::frame::prefetch::above_l2(b, dbytes), b.create_block(&[]));
        b.cond_br(big, (l_big, &[]), (l_join, &[f, words, f]));
        b.switch_to(l_big);
        let (armed, sampling) = cost::verdict(b, env, state, (word, win));
        b.br(l_join, &[armed, words, sampling]);
        b.switch_to(l_join);
        let p = b.block_params(l_join);
        return Live { armed: p[0], words: p[1], fb, sampling: p[2], cost: true };
    }
    let c = env.load_word(b, word);
    env.store_word(b, word, env.zero);
    // the first sample decides: once it is in, every participant only reads the state line (no window writes it again)
    let (sample, shift) = (b.iconst(Type::I64, gravity_program::cost::ROWFILTER_SAMPLE as i64), b.iconst(Type::I64, gravity_program::cost::ROWFILTER_PASS_SHIFT as i64));
    let (seen0, off0) = (b.load(Type::I64, state, ST_SEEN), b.load(Type::I64, state, ST_OFF));
    let sampling = b.icmp(IntCond::Ult, seen0, sample);
    let (l_fold, l_off) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(sampling, (l_fold, &[]), (l_off, &[off0]));
    b.switch_to(l_fold);
    let (s32, low) = (b.iconst(Type::I64, 32), b.iconst(Type::I64, u32::MAX as i64));
    let passed = b.int_bin(IntBinOp::And, c, low);
    let rejected = b.int_bin(IntBinOp::Ushr, c, s32);
    let tested = b.int_bin(IntBinOp::Add, passed, rejected);
    let seen = add_at(b, state, ST_SEEN, tested);
    let pass = add_at(b, state, ST_PASS, passed);
    let enough = b.icmp(IntCond::Uge, seen, sample);
    let scaled = b.int_bin(IntBinOp::Shl, pass, shift);
    let over = b.icmp(IntCond::Ugt, scaled, seen);
    let turn = b.int_bin(IntBinOp::And, enough, over);
    let turn = b.conv(ConvOp::Zext, Type::I64, turn);
    let off = b.int_bin(IntBinOp::Or, off0, turn);
    b.store(state, ST_OFF, off);
    b.br(l_off, &[off]);
    b.switch_to(l_off);
    let off = b.block_params(l_off)[0];
    let on = b.icmp(IntCond::Eq, off, env.zero);
    let big = crate::frame::prefetch::above_l2(b, dbytes);
    let armed = b.int_bin(IntBinOp::And, on, big);
    let words = b.ptr_add(state, env.zero, 1, STATE as i32);
    b.br(l_join, &[armed, words, sampling]);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    Live { armed: p[0], words: p[1], fb, sampling: p[2], cost: false }
}

/// `state`'s word at `off` plus `v`, stored back: the new value.
fn add_at(b: &mut FunctionBuilder<'_>, state: Value, off: i32, v: Value) -> Value {
    let x = b.load(Type::I64, state, off);
    let x = b.int_bin(IntBinOp::Add, x, v);
    b.store(state, off, x);
    x
}

/// In the filtered body, ahead of the directory head of a row whose key hashes to `h`: the row goes on to `l_dir` when
/// every bit of its word is set, else to `l_miss` with no record (0); while the window samples it counts in frame word
/// `word` (D3700: a load / add / store per row, chained through the word, that nothing reads once the sample is in).
pub(crate) fn test(b: &mut FunctionBuilder<'_>, env: &Env, f: &Live, h: Value, word: i32, (l_dir, l_miss): (Block, Block)) {
    // the test in each arm: a pass computed before the branch would live across it (a stack home in the baseline backend)
    let (l_count, l_plain) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(f.sampling, (l_count, &[]), (l_plain, &[]));
    b.switch_to(l_count);
    let pass = hit(b, env, f, h);
    let (one, rej) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, REJECT));
    let add = b.select(pass, one, rej);
    let c = env.load_word(b, word);
    let c = b.int_bin(IntBinOp::Add, c, add);
    env.store_word(b, word, c);
    b.cond_br(pass, (l_dir, &[]), (l_miss, &[env.zero]));
    b.switch_to(l_plain);
    let pass = hit(b, env, f, h);
    b.cond_br(pass, (l_dir, &[]), (l_miss, &[env.zero]));
}

/// Whether every bit key hash `h` sets in its word of `f` is set (`place`'s word and bits).
pub(crate) fn hit(b: &mut FunctionBuilder<'_>, env: &Env, f: &Live, h: Value) -> Value {
    let at = emit_home(b, h, Bits::Dyn(f.fb), Bits::Const(0));
    let wp = b.ptr_add(f.words, at, 8, 0);
    let w = b.load(Type::I64, wp, 0);
    let s = emit_stored(b, h);
    let mut m = env.zero;
    for at in [0, 6, 12, 18] {
        let (sh, six3) = (b.iconst(Type::I64, at), b.iconst(Type::I64, 63));
        let x = b.int_bin(IntBinOp::Ushr, s, sh);
        let x = b.int_bin(IntBinOp::And, x, six3);
        let bit = b.int_bin(IntBinOp::Shl, env.one, x);
        m = b.int_bin(IntBinOp::Or, m, bit);
    }
    let both = b.int_bin(IntBinOp::And, w, m);
    b.icmp(IntCond::Eq, both, m)
}

/// The block path's test (frame.rs `block_loop`, the probe right after the span): block `[j, j + 64)`'s `mask` narrowed
/// to the rows whose keys pass probe `p`'s filter `f`, before the late lanes decode. Every key is a by-value lane cell
/// of the window (row `j`'s element in `row`; row `j + d`'s `d` elements on: the block path's rows are its lanes'
/// rows), its late lane filled first (`fill`). A NULL key's lane holds no value: whatever its bits say, the row has no
/// match, so dropping it is the probe's own answer. The block's rows count in frame word `word` once (under the cost
/// rule, only while the window samples: a window that counted nothing leaves the state line alone). `None`: a key
/// that is no such cell, or lanes the source cannot fill alone (each row then tests on its own, `test`).
#[allow(clippy::type_complexity)]
pub(crate) fn block(p: &Probe<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, (row, j, mask): (&RowVals, Value, Mask), (word, f): (i32, &Live), fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<Value> {
    let mut cols: PgVec<'_, i16> = mcx::reserved_in(p.keys.len(), env.mcx);
    for k in p.keys.iter() {
        let KeySrc::Col(a, ..) = k else { return None };
        cols.push_in(*a);
    }
    let cells = key_cells(b, env, p.node, row, &p.keys, None);
    let mut widths: PgVec<'_, u8> = mcx::reserved_in(cells.len(), env.mcx);
    for c in cells.iter() {
        let (At::Lane(_), Rep::Word { bytes }) = (c.0.at, c.0.rep) else { return None };
        widths.push_in(bytes);
    }
    if !fill(b, env, &cols[..]) {
        return None;
    }
    let (l_loop, l_one, l_done) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.br(l_loop, &[mask.0, mask.0]);
    // at most 64 turns (one per row of the mask), inside the checked block loop
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    b.switch_to(l_loop);
    let (bits, out) = (b.block_params(l_loop)[0], b.block_params(l_loop)[1]);
    let none = b.icmp(IntCond::Eq, bits, env.zero);
    b.cond_br(none, (l_done, &[out]), (l_one, &[]));
    b.switch_to(l_one);
    let t = b.int_un(IntUnOp::Ctz, bits);
    let moved: KeyVals<'_> = cells.iter().zip(&widths).map(|(c, &w)| (c.0, b.ptr_add(c.1, t, w, 0), None, c.3)).collect_in(env.mcx);
    // the test's branch dominates nothing after it: its reads leave no memo entry
    let outer = env.memo.set_aside();
    let mut lx = env.lx(b, p.node);
    let img = image(&mut lx, p.node, &moved, &p.keys);
    env.memo = lx.memo;
    env.memo.restore(outer);
    let h = emit_key_hash(b, &img.words);
    let pass = hit(b, env, f, h);
    let bit = b.int_bin(IntBinOp::Shl, env.one, t);
    let dropped = b.int_bin(IntBinOp::Xor, out, bit);
    let out2 = b.select(pass, out, dropped);
    let low = b.int_bin(IntBinOp::Sub, bits, env.one);
    let rest = b.int_bin(IntBinOp::And, bits, low);
    b.br(l_loop, &[rest, out2]);
    b.switch_to(l_done);
    let out = b.block_params(l_done)[0];
    let (passed, all) = (b.int_un(IntUnOp::Popcnt, out), b.int_un(IntUnOp::Popcnt, mask.0));
    let rejected = b.int_bin(IntBinOp::Sub, all, passed);
    let s32 = b.iconst(Type::I64, 32);
    let rejected = b.int_bin(IntBinOp::Shl, rejected, s32);
    let add = b.int_bin(IntBinOp::Add, passed, rejected);
    let add = if f.cost { b.select(f.sampling, add, env.zero) } else { add };
    let c = env.load_word(b, word);
    let c = b.int_bin(IntBinOp::Add, c, add);
    env.store_word(b, word, c);
    Some(out)
}

/// D3168 (probe_stage.rs): a staged block's rows counted in frame word `word` once, `all` (an `I64` count) tested and
/// `passed` passed: as `block`'s count.
pub(crate) fn count_rows(b: &mut FunctionBuilder<'_>, env: &Env, word: i32, all: Value, passed: Value) {
    let rejected = b.int_bin(IntBinOp::Sub, all, passed);
    let s32 = b.iconst(Type::I64, 32);
    let rejected = b.int_bin(IntBinOp::Shl, rejected, s32);
    let add = b.int_bin(IntBinOp::Add, passed, rejected);
    let c = env.load_word(b, word);
    let c = b.int_bin(IntBinOp::Add, c, add);
    env.store_word(b, word, c);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words take 8 to 16 bits a key in a power of two, one cache line at least; a build past the cap lays none.
    #[test]
    fn the_words_take_eight_to_sixteen_bits_a_key() {
        assert_eq!(within(0, 18), Some(3), "one cache line");
        assert_eq!(within(1, 18), Some(3));
        assert_eq!(within(64, 18), Some(3), "64 keys x 8 bits = 8 words");
        assert_eq!(within(65, 18), Some(4));
        for n in [1_000u64, 310_000, 1_460_000, 4_000_000] {
            let bits = 64u64 << within(n, 24).expect("under the cap");
            assert!(bits >= 8 * n && bits < 16 * n, "{n} keys: {bits} bits");
        }
        let full = (64u64 << 17) / gravity_program::cost::ROWFILTER_BITS_PER_KEY;
        assert_eq!(within(full, 17), Some(17), "the most keys at 8 bits a key");
        assert_eq!(within(full + 1, 17), Some(17), "past them: fewer bits a key in the same words");
        let most = (64u64 << 17) / gravity_program::cost::ROWFILTER_MIN_BITS_PER_KEY;
        assert_eq!(within(most, 17), Some(17), "the most keys that lay words: 4 bits a key");
        assert_eq!(within(most + 1, 17), None, "one more lays none");
        assert_eq!(within(u64::MAX, 17), None);
    }

    /// The cap is half one core's L2 share: 2^17 words (1 MiB) on a 2 MiB L2, and this machine's words never pass it.
    #[test]
    fn the_words_fit_half_one_cores_l2_share() {
        assert_eq!(gravity_program::cost::rowfilter_max_bits(2 << 20), 17);
        assert_eq!(gravity_program::cost::rowfilter_max_bits(1 << 20), 16);
        let share = gravity_driver::machine::profile().partition_bytes;
        let fb = (0..40).filter_map(|s| bits_for(1u64 << s)).max().expect("a small build lays words");
        assert!(8u64 << fb <= (share / 2).max(64), "2^{fb} words past half an L2 share of {share} bytes");
    }

    /// Four bits of one word a key: every key's own bits are set, and a key outside the build passes about the blocked
    /// formula's share (8-16 bits a key, four bits: 1-4 %), for sequential, strided and scattered keys alike.
    #[test]
    fn a_key_outside_the_build_passes_about_the_formula_share() {
        let sets: [(&str, Box<dyn Fn(u64) -> u64>); 3] = [("sequential", Box::new(|i| i)), ("stride 32", Box::new(|i| i * 32)), ("scattered", Box::new(|i| i.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 7))];
        for (name, key) in sets {
            let n = 200_000u64;
            let fb = bits_for(n).expect("under the cap");
            let mut w = vec![0u64; 1 << fb];
            for i in 0..n {
                let (at, m) = place(KeyHash::word(key(i)), fb);
                w[at] |= m;
            }
            let hit = |k: u64| {
                let (at, m) = place(KeyHash::word(k), fb);
                w[at] & m == m
            };
            assert!((0..n).all(|i| hit(key(i))), "{name}: a member always passes");
            let probes = 400_000u64;
            let fp = (0..probes).filter(|&i| hit(key(n + 1 + i) ^ 1 << 62)).count() as f64 / probes as f64;
            assert!(fp < 0.04, "{name}: false positives {fp}");
        }
    }

    /// D3700: a tested row counts in the frame word only while its window samples (1 a pass, `REJECT` a reject); past the
    /// sample the word is never written, and the row goes the same way either way. Words all set pass any key, words all
    /// clear reject it.
    #[test]
    fn a_row_counts_only_while_its_window_samples() {
        for sampling in [true, false] {
            for (fill, pass) in [(u64::MAX, true), (0, false)] {
                let words = [fill; 8];
                let mut frame = [0u64; 4];
                let (wp, fp) = (words.as_ptr() as i64, frame.as_mut_ptr() as i64);
                let went = crate::frame::stop::tests::run1(&[], &mut |b, env| {
                    let at = b.iconst(Type::I64, fp);
                    env.words = Some(b.conv(ConvOp::IntToPtr, Type::Ptr, at));
                    let at = b.iconst(Type::I64, wp);
                    let words = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
                    let (armed, s, fb) = (b.bconst(true), b.bconst(sampling), b.iconst(Type::I64, 3));
                    let f = Live { armed, words, fb, sampling: s, cost: false };
                    let h = b.iconst(Type::I64, 0x2545_F491_4F6C_DD1D);
                    let (l_dir, l_miss, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
                    test(b, env, &f, h, 8, (l_dir, l_miss));
                    b.switch_to(l_dir);
                    let one = b.iconst(Type::I64, 1);
                    b.br(l_out, &[one]);
                    b.switch_to(l_miss);
                    let none = b.block_params(l_miss)[0];
                    b.br(l_out, &[none]);
                    b.switch_to(l_out);
                    b.block_params(l_out)[0]
                });
                assert_eq!(went, pass as i64, "sampling {sampling}: the row's way");
                let want = match (sampling, pass) { (false, _) => 0, (true, true) => 1, (true, false) => REJECT as u64 };
                assert_eq!(frame, [0, want, 0, 0], "sampling {sampling} pass {pass}: the frame word");
            }
        }
    }

    /// A multi-word image hashes through `KeyHash::of`, and its bits are found the same way.
    #[test]
    fn a_wide_image_is_placed_by_its_hash() {
        let fb = bits_for(1000).expect("under the cap");
        let mut w = vec![0u64; 1 << fb];
        for i in 0..1000u64 {
            let (at, m) = place(KeyHash::of(&[i, 7, 1]), fb);
            w[at] |= m;
        }
        assert!((0..1000u64).all(|i| {
            let (at, m) = place(KeyHash::of(&[i, 7, 1]), fb);
            w[at] & m == m
        }));
    }
}
