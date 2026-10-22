//! Road `join.probe.staged` (D3168): a hash probe's rows in stages over a block, so the block's directory and record
//! misses are in flight together, as a vectorized probe keeps them. Budget 340 lines. Owner M1-JOIN.
//!
//! A probe past L2 takes up to two dependent misses a row (the directory head, then its first record), and the next row
//! starts only when this one is done. D3168 S0: the choice is the BUILT table's, never the plan's estimate: a pipeline with
//! a staged probe lowers twice (frame.rs `pipeline_fns_in`), its staged function and its plain one (the road-off code: a
//! staged body that never runs costs the per-row body its registers, TPDE's loop invariants going to the deeper stage
//! loops), and the plain one's entry hands the call to the staged one when the sealed build outgrows L2 (probe.rs
//! `stage_entry`): a program of its own, compiled the first time a build wants it, so small builds never pay its compile.
//! In the staged body (frame.rs `Env::batch`, under `batch_guard`: the HASH form, its
//! directory and records past L2, no partition spilled this window) the rows of a block that all reach the probe (right
//! after the block span, or a whole chunk of the straight rows; the steps between plain projections) run in stages, each
//! a tight loop over the block writing per-row scratch:
//! 1. each row's key image and KeyHash; while the build's filter is armed its Bloom test (D3194) drops a row here;
//! 2. each kept row's directory head and tags: its first record or none, independent loads, each record's lines hinted
//!    as it is found (or loaded or hinted by a stage 3 of their own, `Mode`);
//! 3. the rows walk in order (the frame), each reading its first record from stage 2 (walk.rs `hash_first`). A row with
//!    none walks no step: the staged joins drop a probe row without a match (or only mark the build's, `Mode::marks`).
//! The row order, the matches and every row handed on are the per-row body's. Keys are by-value lane cells of the window
//! only (row `j`'s element in the row, row `j + t`'s `t` elements on), as D3194's block test reads them.
use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_program::Rep;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::cell::At;
use crate::frame::env::{BatchRows, Batched, Env};
use crate::frame::row::RowVals;
use crate::ops::hashjoin::keys::{image, key_cells, KeySrc, KeyVals};
use crate::ops::hashjoin::probe::Probe;
use crate::ops::hashjoin::strategies::rowfilter::{self, Live};
use crate::ops::hashjoin::walk::{head_at, Hv};

/// The road's value: the probe's stages.
pub(crate) struct Staged;

crate::strategies! {
    /// A hash probe past L2 runs its rows in stages over a block: hash, heads, records, walk.
    JOIN_PROBE_STAGED = 151, "join.probe.staged", Join;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(JOIN_PROBE_STAGED, &Staged)], ..crate::ops::NONE };

/// Stage 3's touch of a record's lines: none, hinted, loaded, or hinted by stage 2 as it finds the record (no stage 3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recs {
    Skip,
    Hint,
    Load,
    Early,
}

/// The stages' form, read at lowering from the measurement knobs (process env): `PGRUST_GRAVITY_PROBE_STAGE_ROWS` (a
/// straight chunk's rows: 32, 64 (default), 128 or 256; the block path's blocks are the frame's 64),
/// `PGRUST_GRAVITY_PROBE_STAGE_LOADS` (two letters, default `pm`: stage 1 `p` hints each kept row's head line, `-` not; `m`
/// stage 2 hints each record's lines as it finds the record, else stage 3 `l` loads them, `p` hints them, `-` skips the
/// stage), `PGRUST_GRAVITY_PROBE_STAGE_MARKS=0` (the marking probes keep the per-row body),
/// `PGRUST_GRAVITY_PROBE_STAGE_GATE=dir` (the guard reads the directory alone past L2, not the directory and the records)
/// and `PGRUST_GRAVITY_PROBE_STAGE_SPLIT=0` (one function per pipeline, the staged body beside the per-row one: the A/B
/// arm the split is measured against).
#[derive(Clone, Copy)]
struct Mode {
    rows: usize,
    head_hint: bool,
    recs: Recs,
    marks: bool,
    foot: bool,
    split: bool,
    memo: bool,
}

fn mode() -> Mode {
    let rows = gravity_driver::env::num::<usize>("PGRUST_GRAVITY_PROBE_STAGE_ROWS").filter(|r| matches!(r, 32 | 64 | 128 | 256)).unwrap_or(64);
    let loads = gravity_driver::env::text("PGRUST_GRAVITY_PROBE_STAGE_LOADS").unwrap_or_default();
    let at = |i: usize, d: u8| loads.as_bytes().get(i).copied().unwrap_or(d);
    let recs = match at(1, b'm') {
        b'p' => Recs::Hint,
        b'-' => Recs::Skip,
        b'l' => Recs::Load,
        _ => Recs::Early,
    };
    let marks = gravity_driver::env::num::<u32>("PGRUST_GRAVITY_PROBE_STAGE_MARKS") != Some(0);
    let foot = gravity_driver::env::text("PGRUST_GRAVITY_PROBE_STAGE_GATE").is_none_or(|g| g.trim() != "dir");
    let split = gravity_driver::env::num::<u32>("PGRUST_GRAVITY_PROBE_STAGE_SPLIT") != Some(0);
    let memo = gravity_driver::env::num::<u32>("PGRUST_GRAVITY_PROBE_STAGE_MEMO") != Some(0);
    Mode { rows, head_hint: at(0, b'p') == b'p', recs, marks, foot, split, memo }
}

/// A straight chunk's rows (frame.rs `chunks`).
pub(crate) fn rows() -> usize {
    mode().rows
}

/// Whether the staged body's guard reads the records with the directory (probe.rs `stage_window`).
pub(crate) fn foot() -> bool {
    mode().foot
}

/// Whether a pipeline with a staged probe lowers its staged and plain functions apart (frame.rs `pipeline_fns_in`).
pub(crate) fn split() -> bool {
    mode().split
}

/// Whether a split lowering asks the shape's memo first (gravity_program `stage_memo`; `PGRUST_GRAVITY_PROBE_STAGE_MEMO=0`:
/// every lowering lowers the staged function).
pub(crate) fn memo() -> bool {
    mode().memo
}

/// Whether a probe of join type `jt` stages its rows: the road is on and the join drops a probe row without a match
/// (INNER, SEMI; the marking probes under `Mode::marks`).
pub(crate) fn takes(jt: JoinType) -> bool {
    if !crate::face::source::on(crate::admit::refusal::Variant::Strategy(JOIN_PROBE_STAGED)) {
        return false;
    }
    match jt {
        JoinType::JOIN_INNER | JoinType::JOIN_SEMI => true,
        JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI => mode().marks,
        _ => false,
    }
}

/// The stages over the rows `rows` of the block at row `j` (`StepMech::block_batch`), `row` the source's row `j` (`back`
/// maps a key column to its column there), `fill` the key columns' late lanes: the rows that walk and their first
/// records. `None`: a key that is no by-value lane cell of the source's row, or lanes the source cannot fill alone.
#[allow(clippy::type_complexity)]
pub(crate) fn block(p: &Probe<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, (row, back): (&RowVals, &dyn Fn(i16) -> Option<i16>), rows: BatchRows, fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<Batched> {
    let hv = p.hv.get()?;
    let mcx = env.mcx;
    let mut keys: PgVec<'_, KeySrc> = mcx::reserved_in(p.keys.len(), mcx);
    for k in p.keys.iter() {
        let KeySrc::Col(a, tf, c) = *k else { return None };
        keys.push_in(KeySrc::Col(back(a)?, tf, c));
    }
    let cols: PgVec<'_, i16> = keys.iter().map(|k| if let KeySrc::Col(a, ..) = k { *a } else { 0 }).collect_in(mcx);
    let cells = key_cells(b, env, p.node, row, &keys, None);
    let mut widths: PgVec<'_, u8> = mcx::reserved_in(cells.len(), mcx);
    for c in cells.iter() {
        let (At::Lane(_), Rep::Word { bytes }) = (c.0.at, c.0.rep) else { return None };
        widths.push_in(bytes);
    }
    if !fill(b, env, &cols[..]) {
        return None;
    }
    fires::fire("join.probe.staged"); // shapecheck fire signal (D3168): a probe's rows run its stages over a block
    env.staged = true;
    let m = mode();
    let (hs, firsts) = (array(b, 64 * rows.nw), array(b, 64 * rows.nw));
    let (kept, hits) = (array(b, rows.nw), array(b, rows.nw));
    let key = (&cells, &widths[..], &keys[..]);
    hashes(p, b, env, &hv, key, rows, (hs, firsts, kept), m.head_hint);
    each(b, env, (kept, rows.nw), Some(hits), true, &mut |b, env, t, bit, acc| {
        let at = b.ptr_add(hs, t, 8, 0);
        let h = b.load(Type::I64, at, 0);
        let hp = head_at(b, &hv, h);
        let head = b.load(Type::I64, hp, 0);
        let first = p.head_first(b, env, &hv, h, head);
        let at = b.ptr_add(firsts, t, 8, 0);
        b.store(at, 0, first);
        let none = b.icmp(IntCond::Eq, first, env.zero);
        if m.recs == Recs::Early {
            let r = b.select(none, hv.recs, first);
            let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
            lines(p, env).iter().for_each(|&o| b.prefetch(rec, o));
        }
        let with = b.int_bin(IntBinOp::Or, acc, bit);
        b.select(none, acc, with)
    });
    if !matches!(m.recs, Recs::Skip | Recs::Early) {
        records(p, b, env, (hits, rows.nw), (firsts, hs), m.recs);
    }
    Some(Batched { walk: if rows.all { rows.masks } else { hits }, offset: p.offset, firsts })
}

/// A block's array of `n` words (`Ptr`).
fn array(b: &mut FunctionBuilder<'_>, n: usize) -> Value {
    let s = b.declare_stack_slot(8 * n as u32, 8);
    b.stack_addr(s)
}

/// `f` over each set bit of the `nw` mask words at `masks`, in row order (`t` = 64 w + the bit, `bit` the bit's word),
/// threading a word through the word's rows (from its mask, or 0 with `from_zero`), stored at `out` after each mask word.
/// At most 64 turns a word, inside the checked block loop.
#[allow(clippy::type_complexity)]
fn each(b: &mut FunctionBuilder<'_>, env: &mut Env, (masks, nw): (Value, usize), out: Option<Value>, from_zero: bool, f: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, Value, Value, Value) -> Value) {
    let (l_word, l_bits, l_one, l_done) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64, Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[]));
    b.br(l_word, &[env.zero]);
    b.mark_loop_header(l_word);
    env.bounded.push_in(l_word);
    b.switch_to(l_word);
    let w = b.block_params(l_word)[0];
    let nwv = b.iconst(Type::I64, nw as i64);
    let (end, l_load) = (b.icmp(IntCond::Eq, w, nwv), b.create_block(&[]));
    b.cond_br(end, (l_done, &[]), (l_load, &[]));
    b.switch_to(l_load);
    let at = b.ptr_add(masks, w, 8, 0);
    let bits = b.load(Type::I64, at, 0);
    let acc = if from_zero { env.zero } else { bits };
    b.br(l_bits, &[bits, acc, w]);
    b.mark_loop_header(l_bits);
    env.bounded.push_in(l_bits);
    b.switch_to(l_bits);
    let (bits, acc, w) = (b.block_params(l_bits)[0], b.block_params(l_bits)[1], b.block_params(l_bits)[2]);
    let (none, l_next) = (b.icmp(IntCond::Eq, bits, env.zero), b.create_block(&[]));
    b.cond_br(none, (l_next, &[]), (l_one, &[]));
    b.switch_to(l_one);
    let t = b.int_un(IntUnOp::Ctz, bits);
    let bit = b.int_bin(IntBinOp::Shl, env.one, t);
    let six = b.iconst(Type::I64, 6);
    let base = b.int_bin(IntBinOp::Shl, w, six);
    let tt = b.int_bin(IntBinOp::Add, base, t);
    let acc2 = f(b, env, tt, bit, acc);
    let low = b.int_bin(IntBinOp::Sub, bits, env.one);
    let rest = b.int_bin(IntBinOp::And, bits, low);
    b.br(l_bits, &[rest, acc2, w]);
    b.switch_to(l_next);
    if let Some(o) = out {
        let at = b.ptr_add(o, w, 8, 0);
        b.store(at, 0, acc);
    }
    let w1 = b.int_bin(IntBinOp::Add, w, env.one);
    b.br(l_word, &[w1]);
    b.switch_to(l_done);
}

/// Row `t`'s KeyHash: the image of `cells` moved `t` elements on (each lane `widths` bytes a row), kept in its slot of
/// `hs`. The image's branch dominates nothing after it: its reads leave no memo entry.
fn hashed(p: &Probe<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, (cells, widths, keys): (&KeyVals, &[u8], &[KeySrc]), hs: Value, t: Value) -> Value {
    let moved: KeyVals<'_> = cells.iter().zip(widths).map(|(c, &w)| (c.0, b.ptr_add(c.1, t, w, 0), None, c.3)).collect_in(env.mcx);
    let outer = env.memo.set_aside();
    let mut lx = env.lx(b, p.node);
    let img = image(&mut lx, p.node, &moved, keys);
    env.memo = lx.memo;
    env.memo.restore(outer);
    let h = emit_key_hash(b, &img.words);
    let at = b.ptr_add(hs, t, 8, 0);
    b.store(at, 0, h);
    h
}

/// Stage 1: each row's KeyHash into `hs`, the rows that go on into `kept`. While the build's filter is armed its Bloom
/// test (the words stay in L2: D3194) drops a row (its first record 0 when every row walks, `rows.all`), and the block
/// counts in the sample once while the window samples; with `hint`, each row that goes on hints its head's line.
#[allow(clippy::type_complexity)]
fn hashes(p: &Probe<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, hv: &Hv, key: (&KeyVals, &[u8], &[KeySrc]), rows: BatchRows, (hs, firsts, kept): (Value, Value, Value), hint: bool) {
    let set = (rows.masks, rows.nw);
    let plain = |b: &mut FunctionBuilder<'_>, env: &mut Env| {
        each(b, env, set, Some(kept), false, &mut |b, env, t, _, acc| {
            let h = hashed(p, b, env, key, hs, t);
            if hint {
                let hp = head_at(b, hv, h);
                b.prefetch(hp, 0);
            }
            acc
        })
    };
    let Some(((word, _), f)) = p.rf.zip(p.rfl.get()) else { return plain(b, env) };
    let (l_armed, l_plain, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(f.armed, (l_armed, &[]), (l_plain, &[]));
    b.switch_to(l_plain);
    plain(b, env);
    b.br(l_out, &[]);
    b.switch_to(l_armed);
    each(b, env, set, Some(kept), false, &mut |b, env, t, bit, acc| {
        let h = hashed(p, b, env, key, hs, t);
        filtered(b, env, (hv, &f), (h, t, bit, acc), (firsts, rows.all, hint))
    });
    let l_count = b.create_block(&[]);
    b.cond_br(f.sampling, (l_count, &[]), (l_out, &[]));
    b.switch_to(l_count);
    let (mut all, mut passed) = (env.zero, env.zero);
    for w in 0..rows.nw as i32 {
        let (x, y) = (b.load(Type::I64, rows.masks, 8 * w), b.load(Type::I64, kept, 8 * w));
        let (x, y) = (b.int_un(IntUnOp::Popcnt, x), b.int_un(IntUnOp::Popcnt, y));
        (all, passed) = (b.int_bin(IntBinOp::Add, all, x), b.int_bin(IntBinOp::Add, passed, y));
    }
    rowfilter::count_rows(b, env, word, all, passed);
    b.br(l_out, &[]);
    b.switch_to(l_out);
}

/// Stage 1's filter test of row `t` (`bit`, its key hash `h`) in the rows kept so far `acc`: the rows kept after it.
fn filtered(b: &mut FunctionBuilder<'_>, env: &mut Env, (hv, f): (&Hv, &Live), (h, t, bit, acc): (Value, Value, Value, Value), (firsts, all, hint): (Value, bool, bool)) -> Value {
    let pass = rowfilter::hit(b, env, f, h);
    let dropped = b.int_bin(IntBinOp::Xor, acc, bit);
    if !hint && !all {
        return b.select(pass, acc, dropped);
    }
    let (l_pass, l_drop, l_next): (Block, Block, Block) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(pass, (l_pass, &[]), (l_drop, &[]));
    b.switch_to(l_pass);
    if hint {
        let hp = head_at(b, hv, h);
        b.prefetch(hp, 0);
    }
    b.br(l_next, &[acc]);
    b.switch_to(l_drop);
    if all {
        let at = b.ptr_add(firsts, t, 8, 0);
        b.store(at, 0, env.zero);
    }
    b.br(l_next, &[dropped]);
    b.switch_to(l_next);
    b.block_params(l_next)[0]
}

/// Stage 3: each row of `hits`' first record's lines (its first word, each 64 bytes on, its last word; at most four),
/// hinted or loaded (the loaded words folded into the row's slot of `sink`, so no load is dead).
fn records(p: &Probe<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, set: (Value, usize), (firsts, sink): (Value, Value), recs: Recs) {
    let offs = lines(p, env);
    each(b, env, set, None, true, &mut |b, env, t, _, acc| {
        let at = b.ptr_add(firsts, t, 8, 0);
        let r = b.load(Type::I64, at, 0);
        let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
        let mut v = env.zero;
        for &o in offs.iter() {
            match recs {
                Recs::Hint => b.prefetch(rec, o),
                _ => {
                    let x = b.load(Type::I64, rec, o);
                    v = b.int_bin(IntBinOp::Xor, v, x);
                }
            }
        }
        if recs == Recs::Load {
            let at = b.ptr_add(sink, t, 8, 0);
            b.store(at, 0, v);
        }
        acc
    });
}

/// A record's touch offsets: its first word, each 64 bytes on, its last word (at most four).
fn lines<'e>(p: &Probe<'_>, env: &Env<'e, '_>) -> PgVec<'e, i32> {
    let size = p.sh.rec.get().max(8) as i32;
    let mut offs: PgVec<'_, i32> = (0..size).step_by(64).take(3).collect_in(env.mcx);
    if size - 8 > *offs.last().unwrap_or(&0) {
        offs.push_in(size - 8);
    }
    offs
}
