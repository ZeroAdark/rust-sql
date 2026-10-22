//! Kind `runs.topk_rows` (cards/M3.md L5, I26): PostgreSQL's `Limit -> Sort` over rows (a top-N heapsort) as runs.sort's
//! run with a cut. The producer (`TopEnd`) writes each row into the claim's run as runs/sort.rs does (key pairs, datums,
//! images, var copies: `RunEnd::append`); when the run holds its soft cap, the cut (kernels `sort_cut_res`) orders it and
//! keeps its first `n = count + offset` rows (WITH TIES: and every row tied with row `n - 1` on all keys), and the bound
//! (runs/bound_first.rs) drops later rows past row `n - 1` on key 0 before they are stored. The soft cap is
//! `max(SOFT_ROWS, 4 n)` rows, doubled past a cut that left more than half of it (ties); a run with no room for a row
//! (its region's rows or var bytes) cuts, then puts as a sort does when the cut kept more than half of it. The top's pipeline is described to the driver as a full sort's (a Sort breaker over the
//! share, runs spilled, section ends when claim-driven), so the resident merge, the put and `sort_begin` / `sort_next`
//! serve it unchanged; the emit (runs/sort.rs `RunEmit`) streams the merged rows in order, and the Limit step above cuts
//! them to `[offset, offset + count)` and its ties. A count that is NULL keeps every row; a negative one raises at the
//! open, before any row, as PostgreSQL's `recompute_limits` raises before its subplan runs; a count of 0 reads no row.
//! Budget 400 lines. Owner M3-TOPS.
use gravity_abi::breaker::{Budget, BreakerKind, Spill};
use gravity_decompose::graph::Bound as Lim;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::entries as h3;
use gravity_lower::Place;
use gravity_program::{DictRef, LaneId, Pipeline, Rep};
use types_nodes::list::NodeList;
use types_nodes::Node;
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::strategy::{CODE_CUT, CODE_HEADS, SORT_NATURAL, SORT_RESIDENT};
use crate::face::runs::{CodeTopkImpl, Form, TopDesc};
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker};
use crate::frame::iface::{breaker_of, breaker_of_mut};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::lifetime::bound::{self, Feed};
use crate::ops::Table;
use crate::rep::keylane::{self, Key};
use crate::rep::KeyWord;

use crate::ops::sort::inc_replay;
use crate::ops::sort::strategies::bound_first::Bound;
use crate::ops::sort::strategies::resident as res;
use crate::ops::sort::lower::{sort_of, Arm, RunEmit, RunEnd};
use crate::face::runs::Shape;
use crate::ops::shared::sort::claim_driven;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::RunsTopkRows, &StepMaker(make_step)), (KindId::RunsTopkRows, &EndMaker(make_end)), (KindId::RunsTopkRows, &SourceMaker(make_emit))],
    ..crate::ops::NONE
};

/// The least soft cap: a cut sorts at most this many rows per `SOFT_ROWS - n` rows stored. MEASURED-HELDOUT: a held-out
/// sweep (256..4096 over 96 top-N statements, LIMIT 1..10000, seven key forms, narrow and wide rows) is flat within 1.1%,
/// best at 1024, every answer right (v5 shapecheck sweep-RUNS-07.tsv).
pub(crate) const SOFT_ROWS: i64 = 1024;
/// The soft cap per row of the top: `SOFT_PER_N x n` rows (`n` = count + offset), at least `SOFT_ROWS`.
const SOFT_PER_N: i64 = 4;

/// The soft cap `open` lays for a top of `n` rows (`SOFT_PER_N x n`, at least `SOFT_ROWS`; `i64::MAX`, no cap, past the
/// product's range): one rule with the IR's, read where `n` is a constant (`TopEnd::top_rows`).
const fn soft_cap(n: i64) -> i64 {
    if n > i64::MAX / SOFT_PER_N {
        i64::MAX
    } else if n * SOFT_PER_N < SOFT_ROWS {
        SOFT_ROWS
    } else {
        n * SOFT_PER_N
    }
}
/// The cut's flags (kernels `sort_cut_res`): keep the rows tied with row `n - 1`; mark the kept rows ordered (sort.natural).
const CUT_TIES: i64 = 1;
const CUT_NATURAL: i64 = 2;

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A LIMIT / OFFSET bound as the program reads it (step/limit.rs's rule).
#[derive(Clone, Copy)]
enum B {
    All,
    Const(i64),
    Word(u16),
    /// D1016 row 5: computed from executor parameters, evaluated at each open of the top's pipeline (params_bound.rs).
    Eval(crate::rerun::params_bound::Computed),
}

/// Bound `l` (`which`: 0 count, 1 offset); a computed one keeps its value in the top's words (`top_words`), which a Limit
/// over the top reads too (D1634: one evaluation per open).
fn lim<'p>(px: &mut Px<'_, 'p>, l: &Lim<'p>, node: i32, k: usize, which: u8) -> B {
    match *l {
        Lim::All => B::All,
        Lim::Const(n) => B::Const(n),
        Lim::Expr(e) if bound::rebinds(px.gx, k, l) => B::Eval(eval(px, e, node, k, which)), // D1634
        Lim::Expr(e) => match bound::word(px, Feed::TupleBound, k, e, node) {
            Place::Word(w) => B::Word(w),
            _ if gravity_lower::rerun::words::has_exec(e) => B::Eval(eval(px, e, node, k, which)),
            _ => shape(node, "a LIMIT / OFFSET over a value the binder cannot place"),
        },
    }
}

fn eval<'p>(px: &mut Px<'_, 'p>, e: Node<'p>, node: i32, k: usize, which: u8) -> crate::rerun::params_bound::Computed {
    bound::declare(px, Feed::TupleBound, k, e, node, bound::open_words(px.gx, k, k));
    let words = crate::rerun::params_bound::top_words(px, k, which);
    crate::rerun::params_bound::admit(px, e, node, Some(words))
}

/// Pipeline `k`'s top: `(count, offset, ties)`; a zone walk is road topk.walk's (M3 L7).
fn form(px: &mut Px<'_, '_>, k: usize) -> (B, B, bool) {
    let node = px.gx.pipelines()[k].node_id;
    let (sd, _) = sort_of(px, k);
    match sd.form {
        SortForm::TopK { count, offset, ties } | SortForm::Walk { count, offset, ties, .. } => (lim(px, &count, node, k, 0), lim(px, &offset, node, k, 1), ties),
        SortForm::Runs => bug(node, "runs.topk_rows over a full sort"),
    }
}

/// V4R-179: a bounded incremental sort's stop. PostgreSQL's IncrementalSort under a bound `n` (nodeIncrementalSort.c: each
/// batch reads `min(32, n - done)` rows, then on to its prefix group's end, one row past it held as the next pivot) reads
/// rows up to the first prefix change at or after row `n`, and that one row more; its Limit never pulls past the count.
/// The top reads the same rows: once it has seen `n`, a row whose prefix pairs differ from the row before it is the row
/// PostgreSQL reads last; its projection has run (PostgreSQL's did), it is not kept, and the walk ends (`Env::full`).
struct Stop<'g> {
    pre: usize,
    /// Each prefix key's test words: its order pair, or (true, `tested_by_image`) its equality image.
    by_image: PgVec<'g, bool>,
    /// It stops at PostgreSQL's last row (every prefix key an exact word or image, no WITH TIES).
    exact: bool,
    w_seen: i32,
    /// The row before's prefix words, two per key (`[flags, word]`, or the image's two words).
    w_prev: PgVec<'g, i32>,
    /// EXPLAIN ANALYZE of an exact stop: the replay of PostgreSQL's batches over the rows read (inc_replay.rs).
    replay: Option<Replay>,
    /// Construct C2: the licence an inexact stop reads rows past PostgreSQL's stop under (`of`): the no-effects licence
    /// where no row before the sort raises (props `incremental_admit`, D449), else `Px::run_ahead`'s recheck licence;
    /// `None` for an exact stop, which reads PostgreSQL's rows. Held, not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

/// An exact stop's replay (inc_replay.rs): its state's first frame word (`inc_replay::WORDS` in a row), the Incremental
/// Sort node its cells count, a tuple's `t_len` (heap_form_minimal_tuple's: the 16-byte header, each column at its
/// alignment, no NULLs).
struct Replay {
    w0: i32,
    node: i32,
    t_len: u64,
}

/// A run row's minimal tuple length where every column is fixed-width; `None` otherwise (a varlena's length is its row's).
fn fixed_t_len(s: &Shape) -> Option<u64> {
    let mut off = 0u64;
    for c in s.cols.iter() {
        let len = u64::try_from(c.tf.typlen).ok().filter(|&l| l > 0)?;
        off = off.next_multiple_of(match c.tf.align { b'c' => 1, b's' => 2, b'i' => 4, _ => 8 }) + len;
    }
    Some(16 + off)
}

/// Whether prefix key `key` (over its column's facts `col`) is tested by its equality image (`rep::hash_image`: a string's
/// or a bytea's exact Fp128 of its payload, a bpchar's trimmed) rather than by its order pair: a string key ordered by its
/// type's default order (its image, or a default family's comparator that reaches no user code) under a collation whose
/// equal values are its equal bytes. Its order word is the payload's first 8 bytes, so keys that share them (`key-000001`,
/// `key-000002`, ...) give equal pairs on every row and the stop would read the whole input. Equal values give equal
/// images and equal images are equal keys (rep.varlena.hash's exact image, D247, as WITH TIES compares them), so such a
/// key's test is exact: the stop reads PostgreSQL's rows, a projection that can raise included. A name keeps its pair (its
/// image's cell is a fixed 64 bytes, hashed by rep.hosted's arm, not rep.varlena.hash's).
fn tested_by_image(key: &Key, col: &TypeFacts) -> bool {
    key.tail != 0 && (key.proc == 0 || key.dflt) && !key.user && key.coll.bytes_eq && col.base != keylane::NAME && keylane::key_tf(col, key).image.is_bytes()
}

impl<'g> Stop<'g> {
    /// The stop: a presorted prefix, a serial producer (a parallel claim's input is not in prefix order), and either no
    /// WITH TIES and every prefix key an exact order word (a key with no tail, rep/keylane.rs `pairs`: equal pairs are
    /// equal under the key type's comparator, float -0 / NaN folded, a packed numeric at its scale; never a name's first
    /// eight bytes), which stops at PostgreSQL's last row, or (PM2)
    /// any order word of a deterministic key, WITH TIES or not, where no row before the sort can raise (props
    /// `incremental_admit`, D449's rule: the top may read rows PostgreSQL does not). An inexact word (a string's or a
    /// uuid's first eight bytes, an interval's days over its time of day in 2^11 us steps, a comparator key's 0) is still
    /// one word per value (the rep order arms: equal values, equal images), so a changed pair is a changed prefix; equal
    /// pairs can hide a change, and the top then reads on to the next changed pair, rows whose prefix orders after every
    /// row it keeps (no top keeps them). WITH TIES keeps the `n`th row's ties, which share its prefix: all are read before
    /// the prefix changes (PostgreSQL's Limit passes no bound then, and its IncrementalSort reads at least 32 rows a
    /// batch, rows the top need not read when none can raise).
    fn of(px: &mut Px<'g, '_>, k: usize, s: &Shape, ties: bool) -> Option<Stop<'g>> {
        let mcx = px.mcx();
        let (sd, _) = sort_of(px, k);
        let pre = sd.presorted as usize;
        if pre == 0 || pre > s.keys.len() || claim_driven(&px.gx.pipelines()[k].desc) {
            return None;
        }
        // an exact word: a key with no tail (keylane `pairs` refuses a tail-less key whose order arm's word is inexact),
        // any type whose arm says so (an integer, a float, a packed numeric, timetz, ...), or a string tested by its exact
        // image (`tested_by_image`); a name keeps its image's tail
        let by_image: PgVec<'g, bool> = s.keys[..pre].iter().map(|key| tested_by_image(key, &s.cols[key.col as usize].tf)).collect_in(mcx);
        let exact = !ties && s.keys[..pre].iter().zip(&by_image).all(|(key, &image)| image || key.tail == 0 && s.cols[key.col as usize].tf.base != crate::rep::keylane::NAME);
        // errorhold (D449's rows): where a row before the sort may raise, the inexact stop without WITH TIES reads past
        // PostgreSQL's stop (never short of it: a changed pair is a changed prefix) under a recheck licence (`Px::run_ahead`:
        // the top's pipeline runs whole before any row leaves, no user or VOLATILE call in it or what feeds it, an acting
        // statement denies it): an error it raises re-runs the statement in plan order, where the licence is denied and
        // make_end refuses by D449's name (the row executor stops where PostgreSQL does). WITH TIES takes none: its
        // IncrementalSort reads 32-row batches, rows past the top's stop that PostgreSQL may raise on
        // Construct C2: the stop holds its licence (`Stop::ahead`): the recheck one, or where `incremental_admit` takes
        // it the no-effects one (the rows read past the stop raise nothing, and nothing raise-free calls a VOLATILE
        // function: decompose tests/effects_home.rs `no_volatile_builtin_is_raise_free`)
        let ahead = if exact {
            None
        } else {
            use gravity_decompose::effects::{unobservable, Effects};
            if !s.keys[..pre].iter().all(|key| key.coll.bytes_eq) {
                return None;
            }
            if crate::props::incremental_admit(px.gx, k) {
                // the run-ahead tracer reads it (C2 (b); `run_ahead` holds its own)
                unobservable(Effects::NONE).inspect(|l| px.hold(k, l))
            } else if !ties {
                Some(px.run_ahead(k, Effects::RAISES)?)
            } else {
                return None;
            }
        };
        // the row before's pairs: prefix key `q`'s two words keyed by the Sort op's position `at` with `q` above it (bits
        // 16..24: a prefix is a u8 of keys, an op position stays under 2^16, `Px::frame_word`'s remade-pipeline tag sits
        // at bit 32), so any prefix has its own words beside the end's index table (`runs_word`)
        let at = px.gx.pipelines()[k].ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or_else(|| bug(px.gx.pipelines()[k].node_id, "a top's pipeline without its sort"));
        let mut w = |j: usize, i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
        let w_seen = w(usize::MAX, runs_word::TOP_SEEN);
        let w_prev = (0..pre).flat_map(|q| [(q, 0), (q, 1)]).map(|(q, h)| w(at | q << 16, h)).collect_in(mcx);
        // shapecheck fire signal (REGISTRY RUNS-08): which stop the bounded incremental sort took
        fires::fire(if exact { "topk.stop.exact" } else if ties { "topk.stop.ties" } else { "topk.stop.prefix" });
        // EXPLAIN ANALYZE: an exact stop reads PostgreSQL's rows, so its batches replay (fixed-width rows only: the space
        // is their tuples'); its words keyed past any prefix key's (q = 0xfe), in a row or not armed
        let armed = crate::frame::instr::on() && exact && fixed_t_len(s).is_some();
        let words: PgVec<'g, i32> = if armed { (0..inc_replay::WORDS).map(|i| w(at | 0xfe << 16, i as u8)).collect_in(mcx) } else { PgVec::new_in(mcx) };
        let node = px.gx.pipelines()[k].node_ops.iter().rev().find(|o| o.first_op as usize <= at).map(|o| o.node_id);
        let replay = match (node, fixed_t_len(s)) {
            (Some(node), Some(t_len)) if armed && words.windows(2).all(|x| x[1] == x[0] + 8) => Some(Replay { w0: words[0], node, t_len }),
            _ => None,
        };
        Some(Stop { pre, by_image, exact, w_seen, w_prev, replay, ahead })
    }

    /// The replay's state pointer (its first frame word).
    fn replay_state(b: &mut FunctionBuilder<'_>, env: &Env, r: &Replay, node: i32) -> Value {
        let Some(words) = env.words else { bug(node, "a top's replay without the words site") };
        b.ptr_add(words, env.zero, 1, r.w0)
    }

    /// The replay's check: every row's prefix pairs against the row before's (stored at every row), the row handed to
    /// the replay (`CHANGED`, and `LAST` where the walk ends at it), then the stop as `check`'s.
    fn check_replayed(&self, r: &Replay, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, s: &Shape, row: &RowVals, w_n: i32) {
        let (seen, n) = (env.load_word(b, self.w_seen), env.load_word(b, w_n));
        let pairs = self.pairs(b, env, node, s, row);
        let mut same = b.bconst(true);
        for (i, v) in pairs.iter().flatten().enumerate() {
            let prev = env.load_word(b, self.w_prev[i]);
            let e = b.icmp(IntCond::Eq, *v, prev);
            same = b.int_bin(IntBinOp::And, same, e);
            env.store_word(b, self.w_prev[i], *v);
        }
        let first = b.icmp(IntCond::Eq, seen, env.zero);
        let either = b.int_bin(IntBinOp::Or, same, first);
        let t = b.bconst(true);
        let changed = b.int_bin(IntBinOp::Xor, either, t);
        let past = b.icmp(IntCond::Sge, seen, n);
        let stop = b.int_bin(IntBinOp::And, past, changed);
        let c = b.conv(ConvOp::Zext, Type::I64, changed);
        let l = b.conv(ConvOp::Zext, Type::I64, stop);
        let l = b.int_bin(IntBinOp::Add, l, l);
        let flags = b.int_bin(IntBinOp::Or, c, l);
        let st = Self::replay_state(b, env, r, node);
        let h = res::host(b, "incsort_replay_row", &[Type::Ptr, Type::I64], Type::I64, gravity_abi::host_words!(super::raw::incsort_replay_row(st, flags)));
        let _ = b.call_host(h, &[st, flags], Some(Type::I64));
        let (l_stop, l_count) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(stop, (l_stop, &[]), (l_count, &[]));
        b.switch_to(l_stop);
        env.full(b, &row.carry, row.next);
        b.switch_to(l_count);
        let seen1 = b.int_bin(IntBinOp::Add, seen, env.one);
        env.store_word(b, self.w_seen, seen1);
    }

    /// The replay's end: the input's end met (or the walk stopped), its counts added into the node's cells and cleared.
    fn replay_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32) {
        let Some(r) = &self.replay else { return };
        let st = Self::replay_state(b, env, r, node);
        let h = res::host(b, "incsort_replay_end", &[Type::Ptr], Type::I64, gravity_abi::host_words!(super::raw::incsort_replay_end(st)));
        let _ = b.call_host(h, &[st], Some(Type::I64));
        for (i, ctr) in crate::frame::instr::INC_STATS.iter().enumerate() {
            let off = r.w0 + 8 * (inc_replay::AGG_WORD + i) as i32;
            let v = env.load_word(b, off);
            crate::frame::instr::count(b, env.ctx, r.node, *ctr, v);
            env.store_word(b, off, env.zero);
        }
    }

    /// Before the row is kept: past `n` rows seen and a new prefix, the walk ends; else the row is counted. Only the `n`th
    /// row writes its prefix words and only a row past it reads them: before `n` no row stops, and past it an unchanged
    /// prefix leaves the words equal to the row before's (a changed one stops). So a top that never stops pays one compare
    /// a row before `n` and no store past it, not a store per prefix word a row (RUNS-08's 32-key worst case).
    fn check(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, s: &Shape, row: &RowVals, w_n: i32) {
        if let Some(r) = &self.replay {
            return self.check_replayed(r, b, env, node, s, row, w_n);
        }
        let (seen, n) = (env.load_word(b, self.w_seen), env.load_word(b, w_n));
        // `n` is at least 1 here (a top of 0 rows reads none), so the first row never stops
        let past = b.icmp(IntCond::Sge, seen, n);
        let last = { let n1 = b.int_bin(IntBinOp::Sub, n, env.one); b.icmp(IntCond::Eq, seen, n1) };
        let need = b.int_bin(IntBinOp::Or, past, last);
        let (l_need, l_count) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(need, (l_need, &[]), (l_count, &[]));
        b.switch_to(l_need);
        // the branch's words do not dominate the row's later reads
        let aside = env.memo.set_aside();
        let pairs = self.pairs(b, env, node, s, row);
        env.memo.restore(aside);
        let (l_cmp, l_keep, l_stop) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_cmp, &[]), (l_keep, &[]));
        b.switch_to(l_cmp);
        let mut same = b.bconst(true);
        for (i, v) in pairs.iter().flatten().enumerate() {
            let prev = env.load_word(b, self.w_prev[i]);
            let e = b.icmp(IntCond::Eq, *v, prev);
            same = b.int_bin(IntBinOp::And, same, e);
        }
        b.cond_br(same, (l_count, &[]), (l_stop, &[]));
        b.switch_to(l_stop);
        env.full(b, &row.carry, row.next);
        b.switch_to(l_keep);
        for (i, v) in pairs.iter().flatten().enumerate() {
            env.store_word(b, self.w_prev[i], *v);
        }
        b.br(l_count, &[]);
        b.switch_to(l_count);
        let seen1 = b.int_bin(IntBinOp::Add, seen, env.one);
        env.store_word(b, self.w_seen, seen1);
    }

    /// The row's prefix pairs (two words per key).
    fn pairs<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, s: &Shape, row: &RowVals) -> PgVec<'e, [Value; 2]> {
        let mcx = env.mcx;
        let tfs: PgVec<'e, TypeFacts> = s.cols.iter().map(|c| c.tf).collect_in(mcx);
        let mut cx = env.lx(b, node);
        // the prefix test compares the pairs for equality only: a collated string's word is its bytes prefix where equal
        // values are equal bytes (`KeyWord::Equal`, PostgreSQL's presorted-key test), not the provider's sort key per row
        cx.key_word = KeyWord::Equal;
        let mut pairs = mcx::reserved_in(self.pre, mcx);
        for (key, &image) in s.keys[..self.pre].iter().zip(&self.by_image) {
            if !image {
                pairs.extend_in(keylane::pairs(&mut cx, node, row, std::slice::from_ref(key), &tfs[..], mcx));
                continue;
            }
            // a NULL row's image is a constant (rep.varlena.hash: 0, 0), so NULL rows compare equal to each other
            let Some(cr) = row.col(key.col as i16 + 1) else { bug(node, "a sort key past the row") };
            let w = crate::rep::hash_image(&mut cx, cr, &keylane::key_tf(&tfs[key.col as usize], key), crate::rep::Scope::Program);
            if !w.exact {
                bug(node, "a string prefix key's image that is not exact");
            }
            let zero = cx.b.iconst(Type::I64, 0);
            pairs.push_in([w.w[0].unwrap_or(zero), w.w[1].unwrap_or(zero)]);
        }
        env.memo = cx.memo;
        pairs
    }
}

/// The sort op: the end takes the row as it is.
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, _k: usize, _j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g)
}

/// The producer: runs/sort.rs's run end with the cut, the soft cap and the bound.
struct TopEnd<'g> {
    run: RunEnd,
    node: i32,
    count: B,
    offset: B,
    ties: bool,
    bound: Option<Bound>,
    stop: Option<Stop<'g>>,
    /// Road topk.fetch (runs/fetch.rs): the full row's key columns the `(key, locator)` run keeps, and whether the
    /// locator is a heap row's TID word.
    fetch: Option<(PgVec<'g, u16>, bool)>,
    /// Frame words: `n` (count + offset, `i64::MAX` for no count) and the soft cap, both set at the open.
    w_n: i32,
    w_soft: i32,
    /// D643: the run was armed for section ends (the decomposer's descriptor claim-driven): its descriptor keeps them.
    ends: bool,
    /// A union's arm (decompose append.rs `copies`): every call's run is cut to the top at its finish, so the arm's sealed
    /// run holds its own top alone and exec `ArmSort` holds and merges n rows per arm, not every row under the soft cap.
    arm: bool,
    /// USEROP K7: a key's comparator may be the user's (`Key::user`, `KFLAG_USER`): the top is PostgreSQL's bounded
    /// heapsort, a step after each stored row (kernels sort raw.rs `SortHeapStep`), no bound; WITH TIES (PostgreSQL passes
    /// no bound: a full sort, `compute_tuples_needed`) the run is never cut at the soft cap.
    exact: bool,
    /// The top keeps PostgreSQL's bounded heap (`SortHeapStep` after each stored row, no bound): an exact top without WITH
    /// TIES, or (ROW-TOPN, `heap_key`) one whose first key is ordered by its comparator under the constant word.
    heap: bool,
    /// D3027 (b): the section end cuts the resident run to `n` rows and seals it by that bound (kernels
    /// `sort_open_res_top`), not by a share of the grant: a resident top with no WITH TIES, no heap and not a union's arm.
    sealed_by_n: bool,
}

/// ROW-TOPN (a top-N over a ROW() key: 1.23 ms vs rowexec's 0.39 ms over a scan): whether the run's first key is
/// ordered by a comparator through fmgr whose prefix word is the constant 0 (rep.hosted.order: a record's, an array's, a
/// range's, an enum's, a `name`'s under a collation; never numeric / inet / bit, whose payload word orders them and whose
/// comparator a pool worker may call, `gravity_fn::resolved::pool_safe`). Its bound (topk.boundfirst) never drops a row,
/// so the soft cap's cut called the comparator two to four times per row stored (a select, or PostgreSQL's qsort for a
/// comparator that may meet an order that is not total); PostgreSQL's bounded heap compares most rows once, with its top.
/// The heap never panics on such an order (raw.rs `PgHeap`: in-bounds moves only). A by-value key (an enum, a by-value
/// user type) keeps the cut: its comparator is cheap beside the heap's host step per stored row (g-topn topn_core-0041,
/// `ORDER BY e, id LIMIT 100` over an enum: 12.3 ms with the cut, 14.5 with the heap; rowexec 21.5).
fn heap_key(s: &Shape) -> bool {
    s.keys.first().is_some_and(|k| {
        let tf = &s.cols[k.col as usize].tf;
        k.proc != 0 && !tf.byval && !gravity_fn::resolved::info(k.proc).is_some_and(|f| gravity_fn::resolved::pool_safe(tf.typid, f.fn_oid))
    })
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let (count, offset, ties) = form(px, k);
    let (sd, cols) = sort_of(px, k);
    if crate::effects::visible::rep::user_batches(px.gx, k) && !crate::ops::shared::one_batch::served(px, k) {
        shape(node, crate::rep::keylane::USER_BATCHES);
    }
    // D3024 (b): a wide group emit's top takes section ends as a claim-driven source's (ops/sort/lower.rs `wide_row_end`)
    let eligible = claim_driven(&px.gx.pipelines()[k].desc) || px.wide.contains(&k) || crate::face::partruns::reads_wide(px, k);
    // a union's arms continue one run (decompose append.rs `copies`): each arm's run is resident and exec `ArmSort`, declared
    // at the run's first arm only, merges them (runs/sort.rs make_end's rule)
    let union_arm = crate::ops::sort::lower::union_arm(&px.gx.pipelines()[k]);
    // sort.keylane applies to full sorts only (props lane_key); sort.natural and sort.resident to every sort kind
    let arm = Arm { keylane: false, natural: res::on(px, SORT_NATURAL), resident: union_arm.is_some() || eligible && res::on(px, SORT_RESIDENT), finish: eligible };
    let mut run = RunEnd::over(px, k, &sd.keys, Some(sd.tlist), cols, arm);
    // D3024 (b): a wide emit the leader keeps (table/emit_group.rs `kept_on_leader`, read now that the end's expressions are
    // made) runs site-less in one call: this top seals its run itself, as a serial producer's (ops/sort/lower.rs)
    let eligible = if eligible && !claim_driven(&px.gx.pipelines()[k].desc) && crate::table::emit_group::kept_on_leader(px, k) {
        run.kept_serial(px, k, union_arm.is_some());
        false
    } else {
        eligible
    };
    let fetch = crate::ops::sort::strategies::fetch::of(px, k).map(|(_, _, heap)| {
        let (sh, keycols) = crate::ops::sort::strategies::fetch::narrow(&run.shape, node, px.mcx());
        run.shape = sh;
        (keycols, heap)
    });
    let top = TopDesc { k, node, sd };
    if fetch.is_none() {
        run.shape = coded(px, k, sd, cols, &run.shape, heads(px, &top).is_some());
    }
    if let (true, Some(br), true) = (run.resident, breaker_of(&px.gx.pipelines()[k].desc), union_arm.is_none_or(|i| i == 0)) {
        res::declare(px, k, br.slot, (!run.shape.var()).then_some((run.shape.spec, 2 * run.shape.keys.len() as u16)));
    }
    let exact = run.shape.var() && run.shape.keys.iter().any(|key| key.user);
    // ROW-TOPN: a top (not a union's arm, whose arms continue one run) whose first key's word is the constant keeps
    // PostgreSQL's heap too (`heap_key`). A claim-driven one is served serial (its comparator binds the pipeline to the
    // leader, frame/mechs.rs M6-HOSTED), so its steps share the leader's heaps, as an exact top's do
    let heap = !ties && (exact || run.shape.var() && union_arm.is_none() && heap_key(&run.shape));
    let bound = if exact || heap { None } else { Bound::of(px, k) };
    // a bounded incremental sort: the stop (V4R-179; PM2: over any deterministic prefix, WITH TIES too, where no row before
    // the sort can raise; errorhold: without WITH TIES where one may, under a recheck licence); else the top reads every
    // row, admitted when no row before the sort can raise (D449)
    let stop = Stop::of(px, k, &run.shape, ties);
    // a reader that counts on the exact stop (ops/shared/sort.rs `exact_stop`: MergeAppend's pull form under the top,
    // whose arm rows may raise) reads PostgreSQL's rows only through it: fail closed where the run shape disagrees
    let pulled = || crate::props::Merge::of_reader(px.gx, k).is_some_and(|m| crate::props::merge_pull(px.gx, &m) && !crate::props::arms_admit(px.gx, &m));
    if !stop.as_ref().is_some_and(|st| st.exact) && crate::ops::shared::sort::exact_stop(px.gx, k) && pulled() {
        bug(node, "a top whose graph test promises the exact stop and whose run shape takes another");
    }
    // a union's arms continue one run: every arm keeps the provider's words there (an arm without the stop writes them)
    if union_arm.is_none() && stop.as_ref().is_some_and(|st| plain_ok(&run.shape, st.pre)) {
        run.key_word = KeyWord::Plain;
    }
    if sd.presorted > 0 && stop.is_none() && !crate::props::incremental_admit(px.gx, k) {
        shape(node, "a bounded incremental sort whose rows can raise past PostgreSQL's early stop (D449's rule)");
    }
    let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
    // D482: the Runs face's one index table, past the run reader's and the run end's words
    let (w_n, w_soft) = (w(runs_word::TOP_N), w(runs_word::TOP_SOFT));
    let sealed_by_n = run.resident && !ties && !heap && !exact && union_arm.is_none();
    if sealed_by_n {
        fires::fire("topk.seal_by_n"); // D3027 (b)'s fire count (R-KEEP)
    }
    let mut end: PgBox<'g, dyn EndMech + 'g> = mcx::pg_box_dyn!(px.mcx(), TopEnd { run, node, count, offset, ties, bound, stop, fetch, w_n, w_soft, ends: eligible, arm: union_arm.is_some(), exact, heap, sealed_by_n } => dyn EndMech + 'g);
    // L0-f (M6-CODES R3, V4R-294; V4R-310): an engaged code top-k road wraps the end (code.cut, code.heads)
    for r in [CODE_CUT, CODE_HEADS] {
        let m = crate::frame::iface::need_strategy::<CodeTopkImpl>(&node, r).0;
        end = if res::on(px, r) && m.engages(px.gx, &top) { m.end(px, &top, end) } else { end };
    }
    crate::ops::sort::sort_raise::wrap(px.gx, k, end) // D1292: PG's 42P22 at the Sort's start
}

/// Whether a top that stops at its presorted prefix (`pre` keys) keeps 0 for its run's collated words (`KeyWord::Plain`):
/// PostgreSQL's bounded sort takes no abbreviated key (tuplesort.c `tuplesort_set_bound`), and a prefix key holds one
/// value per group the top reads, so its comparator's memcmp test decides it; the provider's sort key per row (ICU:
/// scan-0211 / 0254, 6,000 rows of one prefix group) was the top's cost. Only where no key past the prefix is a collated
/// string (its words cut the run without the comparator) and no column is a code (the code roads keep their own words).
fn plain_ok(s: &Shape, pre: usize) -> bool {
    s.keys.get(pre..).is_some_and(|ks| ks.iter().all(|k| k.tail == 0 || k.coll.bytes_order)) && s.cols.iter().all(|c| !matches!(c.form, Form::Code { .. }))
}

impl<'g> TopEnd<'g> {
    /// A bound's value this call (`None`: ALL or NULL, no limit); a negative one raises `which` (0 OFFSET, 1 LIMIT).
    fn value(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l: B, which: i64) -> Option<Value> {
        let (v, null) = match l {
            B::All => return None,
            B::Eval(c) => c.read(b, env),
            B::Const(n) if n >= 0 => return Some(b.iconst(Type::I64, n)),
            B::Const(n) => (b.iconst(Type::I64, n), None),
            B::Word(w) => {
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                (b.load(Type::I64, env.params, w as i32 * 8), Some(b.icmp(IntCond::Ne, nb, env.zero)))
            }
        };
        let neg = b.icmp(IntCond::Slt, v, env.zero);
        let t = b.bconst(true);
        let bad = match null {
            Some(n) => {
                let nn = b.int_bin(IntBinOp::Xor, n, t);
                b.int_bin(IntBinOp::And, neg, nn)
            }
            None => neg,
        };
        // D1755: under a node above that stopped (the feeder's check: a zero count, a false gate) PG never reaches this
        // Limit, so its bound neither raises nor counts: 0, the top reads no row
        let (bad, v) = match env.stopped {
            Some(s) => {
                let go = b.int_bin(IntBinOp::Xor, s, t);
                (b.int_bin(IntBinOp::And, bad, go), b.select(s, env.zero, v))
            }
            None => (bad, v),
        };
        let (l_raise, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(bad, (l_raise, &[]), (l_ok, &[]));
        b.switch_to(l_raise);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("limit_negative", &[Type::I64], Some(Type::I64), gravity_kernels::host_abi::limit_negative as *const () as usize, mem);
        let wv = b.iconst(Type::I64, which);
        let mask = b.call_host(h, &[wv], Some(Type::I64)).unwrap_or(env.zero);
        b.br(env.l_raise, &[mask]);
        b.switch_to(l_ok);
        // a NULL OFFSET is 0; a NULL LIMIT is no limit
        match null {
            Some(n) if which == 0 => Some(b.select(n, env.zero, v)),
            Some(n) => {
                let all = b.iconst(Type::I64, i64::MAX);
                Some(b.select(n, all, v))
            }
            None => Some(v),
        }
    }

    /// USEROP K7 / ROW-TOPN: a heap top's step after the row just stored (kernels sort raw.rs `SortHeapStep`: PostgreSQL's bounded heap, made
    /// at `2n + 1` rows, then a compare with its top per row). A status below zero leaves through `l_exit`.
    fn heap_step(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let run = self.run.run(b, env);
        let n = env.load_word(b, self.w_n);
        let h = res::host(b, "sort_heap_step", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Type::I64, gravity_kernels::contract::entry3::<h3::SortHeapStep> as *const () as usize);
        let tbl = self.run.shape.table(b);
        let r = b.call_host(h, &[env.ctx, run, tbl, n], Some(Type::I64)).expect("the heap step returns a status");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
    }

    /// The cut: the run ordered and kept to its first `n` rows (and their ties); the bound armed when the run held `n`;
    /// the soft cap doubled while a cut leaves more than half of it. A status below zero leaves through `l_exit`. Returns
    /// the rows kept.
    fn cut(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let s = &self.run.shape;
        let run = self.run.run(b, env);
        let n = env.load_word(b, self.w_n);
        let flags = b.iconst(Type::I64, if self.ties { CUT_TIES } else { 0 } | if self.run.natural { CUT_NATURAL } else { 0 });
        let slot = b.declare_stack_slot(16, 8);
        let out = b.stack_addr(slot);
        let r = if s.var() {
            let h = res::host(b, "sort_cut_res_var", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::Ptr], Type::I64, gravity_abi::host_words!(h3::sort_cut_res_var(ctx, run, tbl, n, flags, out)));
            let tbl = s.table(b);
            b.call_host(h, &[env.ctx, run, tbl, n, flags, out], Some(Type::I64))
        } else {
            let h = res::host(b, "sort_cut_res", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr], Type::I64, gravity_abi::host_words!(h3::sort_cut_res(ctx, run, nkey, spec, n, flags, out)));
            let [_, nkey, spec] = s.args(b);
            b.call_host(h, &[env.ctx, run, nkey, spec, n, flags, out], Some(Type::I64))
        }
        .expect("the cut returns the rows kept");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        if let Some(bd) = &self.bound {
            let full = b.icmp(IntCond::Sge, r, n);
            bd.arm(b, env, out, full);
        }
        let soft = env.load_word(b, self.w_soft);
        let two = b.int_bin(IntBinOp::Add, r, r);
        let grow = b.icmp(IntCond::Sgt, two, soft);
        let soft = b.select(grow, two, soft);
        env.store_word(b, self.w_soft, soft);
        r
    }
}

impl<'g> TopEnd<'g> {
    /// The row's key-0 `(flags, word)` pair as the run stores it (rep/keylane.rs `pairs`), the pair the bound compares.
    fn key0_pair(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> [Value; 2] {
        let s = &self.run.shape;
        let mcx = env.mcx;
        let tfs: PgVec<'_, TypeFacts> = s.cols.iter().map(|c| c.tf).collect_in(mcx);
        let mut cx = env.lx(b, self.node);
        cx.key_word = self.run.key_word; // the bound compares against the run's words
        let pairs = crate::rep::keylane::pairs(&mut cx, self.node, row, &s.keys[..1], &tfs[..], mcx);
        env.memo = cx.memo;
        pairs[0]
    }
}

impl<'g> EndMech for TopEnd<'g> {
    /// D3990: the run's append copies the row; the bound is armed from the run's rows. A top that stops at a prefix change
    /// (`stop`) keeps the previous row's prefix pairs in its words, so it is left out.
    fn copies_row(&self) -> bool {
        self.stop.is_none()
    }

    /// D3027: past this worker's armed bound (topk.boundfirst), the test `lower` drops the row by. Not for a fetch top,
    /// whose key comes from its locator's fetch.
    fn past_bound(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Value> {
        let bd = self.bound.as_ref().filter(|_| self.fetch.is_none())?;
        let pair = self.key0_pair(b, env, row);
        let keep = bd.passes(b, env, pair);
        let t = b.bconst(true);
        Some(b.int_bin(IntBinOp::Xor, keep, t))
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.run.open_words(w);
        w.declare(OpenWord::open("top n", Base::Words, self.w_n, Type::I64, 0));
        w.declare(OpenWord::open("top soft cap", Base::Words, self.w_soft, Type::I64, 0));
        if let Some(bd) = &self.bound {
            bd.open_words(w);
        }
        for c in [self.count, self.offset].iter().filter_map(|l| if let B::Eval(c) = l { c.words() } else { None }) {
            w.declare(OpenWord::open("top computed bound", Base::Words, c[0], Type::I64, 0));
            w.declare(OpenWord::open("top computed flags", Base::Words, c[1], Type::I64, 0));
        }
        if let Some(st) = &self.stop {
            w.declare(OpenWord::open("top rows seen", Base::Words, st.w_seen, Type::I64, 0));
            for &p in st.w_prev.iter() {
                w.declare(OpenWord::open("top prefix pair", Base::Words, p, Type::I64, 0));
            }
            for i in 0..st.replay.as_ref().map_or(0, |_| inc_replay::WORDS) {
                let r = st.replay.as_ref().map_or(0, |r| r.w0);
                w.declare(OpenWord::open("top incremental sort replay", Base::Words, r + 8 * i as i32, Type::I64, 0));
            }
        }
    }

    /// `n` and the soft cap (a negative bound raises before the run opens), then runs.sort's open (at the section end the
    /// seal or put, leaving through `l_done`; D3027 (b): a run sealed by `n` cut to it first); a top of 0 rows reads none.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let offset = self.value(b, env, self.offset, 0);
        let count = self.value(b, env, self.count, 1);
        let all = b.iconst(Type::I64, i64::MAX);
        let n = match (count, offset) {
            (None, _) => all,
            (Some(c), None) => c,
            (Some(c), Some(o)) => {
                // count + offset saturating at i64::MAX (both are non-negative here)
                let room = b.int_bin(IntBinOp::Sub, all, o);
                let over = b.icmp(IntCond::Sgt, c, room);
                let sum = b.int_bin(IntBinOp::Add, c, o);
                b.select(over, all, sum)
            }
        };
        env.store_word(b, self.w_n, n);
        // the replay's bound (the top's n, `i64::MAX` for none) and tuple length
        if let Some(r) = self.stop.as_ref().and_then(|st| st.replay.as_ref()) {
            env.store_word(b, r.w0, n);
            let t = b.iconst(Type::I64, r.t_len as i64);
            env.store_word(b, r.w0 + 8, t);
        }
        let four = b.iconst(Type::I64, SOFT_PER_N);
        let quarter = b.iconst(Type::I64, i64::MAX / SOFT_PER_N);
        let big = b.icmp(IntCond::Sgt, n, quarter);
        let n4 = b.int_bin(IntBinOp::Mul, n, four);
        let n4 = b.select(big, all, n4);
        let least = b.iconst(Type::I64, SOFT_ROWS);
        let small = b.icmp(IntCond::Slt, n4, least);
        let soft = b.select(small, least, n4);
        // USEROP K7: an exact top WITH TIES is PostgreSQL's full sort (no bound): never cut at the soft cap
        let soft = if self.exact && self.ties { all } else { soft };
        env.store_word(b, self.w_soft, soft);
        // D3027 (b): the section end's cut to `n` and its seal by `n` (the cut's flags as the soft cap's, no WITH TIES)
        let flags = if self.run.natural { CUT_NATURAL } else { 0 };
        self.run.open_bounded(b, env, self.sealed_by_n.then_some((n, flags)));
        // LIMIT 0: PostgreSQL's Limit never runs its subplan
        let (none, l_go) = (b.icmp(IntCond::Eq, n, env.zero), b.create_block(&[]));
        b.cond_br(none, (env.x.l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
        if let Some(bd) = &self.bound {
            let run = self.run.run(b, env);
            bd.rearm(b, env, run, n, self.run.shape.row_bytes);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let fetched = self.fetch.as_ref().map(|(kc, heap)| {
            let Some(l) = env.loc else { bug(self.node, "a fetch top over a source without a locator") };
            let loc = crate::ops::sort::strategies::fetch::locator_word(b, l, *heap);
            crate::ops::sort::strategies::fetch::run_row(row, kc, loc, self.node, env.mcx)
        });
        let row = fetched.as_ref().unwrap_or(row);
        if let Some(st) = &self.stop {
            st.check(b, env, self.node, &self.run.shape, row, self.w_n);
        }
        let l_join = b.create_block(&[Type::I64]);
        if let Some(bd) = &self.bound {
            let pair = self.key0_pair(b, env, row);
            let keep = bd.passes(b, env, pair);
            let l_keep = b.create_block(&[]);
            b.cond_br(keep, (l_keep, &[]), (l_join, &[row.carry.out]));
            b.switch_to(l_keep);
        }
        // the soft cap: a cut before the row is stored
        let run = self.run.run(b, env);
        let len = b.load_zext(Type::I32, run, 4);
        let soft = env.load_word(b, self.w_soft);
        let (at_cap, l_cut, l_store) = (b.icmp(IntCond::Sge, len, soft), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(at_cap, (l_cut, &[]), (l_store, &[]));
        b.switch_to(l_cut);
        self.cut(b, env);
        b.br(l_store, &[]);
        b.switch_to(l_store);
        // a run with no room for the row (its rows at cap, or its var copies at the rows' end): the cut, and a put when the
        // cut kept more than half of its rows (D1028: a cut that drops nothing leaves no room, one that drops a few is due
        // again a few rows later, a sort of the run per row; so at least as many rows are stored as the next cut sorts).
        // D3544: an empty run with no room for the row's copies (a key larger than the run's region) puts too: no cut frees
        // room there, and the put hands that row a big run of its own (D1330, `sort_full_var`), as a full sort's put does
        let c = self.run.append(b, env, row, &mut |b, env, _| {
            let run = self.run.run(b, env);
            let before = b.load_zext(Type::I32, run, 4);
            let kept = self.cut(b, env);
            let two = b.int_bin(IntBinOp::Add, kept, kept);
            let (more, empty) = (b.icmp(IntCond::Sgt, two, before), b.icmp(IntCond::Eq, before, env.zero));
            let (put, l_put, l_back) = (b.int_bin(IntBinOp::Or, more, empty), b.create_block(&[]), b.create_block(&[]));
            b.cond_br(put, (l_put, &[]), (l_back, &[]));
            b.switch_to(l_put);
            self.run.put(b, env, false);
            b.br(l_back, &[]);
            b.switch_to(l_back);
        });
        if self.heap {
            self.heap_step(b, env);
        }
        let _ = x;
        b.br(l_join, &[c.out]);
        b.switch_to(l_join);
        Carry::new(b.block_params(l_join)[0], env.mcx)
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        if let Some(st) = &self.stop {
            st.replay_end(b, env, self.node);
        }
        if self.arm {
            // the run word is written at every open (0): a section-end call's open sealed the run and stored none
            let run = self.run.run(b, env);
            let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
            let (live, l_cut, l_on) = (b.icmp(IntCond::Ne, ri, env.zero), b.create_block(&[]), b.create_block(&[]));
            b.cond_br(live, (l_cut, &[]), (l_on, &[]));
            b.switch_to(l_cut);
            self.cut(b, env);
            b.br(l_on, &[]);
            b.switch_to(l_on);
        } else if let (Some(bd), false) = (&self.bound, self.exact && self.ties) {
            // a claim leaving more than `n` rows past the run's first `n` (rows stored under the bound the open re-armed)
            // cuts here: the walk cuts at the soft cap alone otherwise, so those rows piled up claim after claim into the
            // section end's seal (a sort of the whole run) and the W8 merge (a copy of every row): zwd5-064 at W8 sealed
            // 788-796 rows a worker, against 92-96 before the re-arm. The seal now sees at most 2n rows. Not for an exact
            // top WITH TIES (never cut, as the soft cap).
            let run = self.run.run(b, env);
            let (l_cut, l_on) = (b.create_block(&[]), b.create_block(&[]));
            let n = env.load_word(b, self.w_n);
            let piled = bd.piled(b, env, run, n);
            b.cond_br(piled, (l_cut, &[]), (l_on, &[]));
            b.switch_to(l_cut);
            self.cut(b, env);
            b.br(l_on, &[]);
            b.switch_to(l_on);
        }
        self.run.finish(b, env, x);
    }

    fn wants_locator(&self) -> bool {
        self.fetch.is_some()
    }

    /// Described as a full sort's pipeline: a Sort breaker over the share, its runs spilled past it, section ends when
    /// claim-driven (the driver and exec then treat the top's run as runs.sort's); the rows its run holds before it cuts
    /// published when known (`top_rows`), which bound the share.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, mut d: Pipeline) -> Pipeline {
        let Some(br) = breaker_of_mut(&mut d) else { bug(self.node, "a row top that does not end in its breaker") };
        *br = gravity_abi::breaker::Breaker { kind: BreakerKind::Sort, budget: Budget::Share, spill: Spill::Runs, ..*br };
        if claim_driven(&d) && !d.finish {
            d = d.finishing();
        }
        // D643: a run emitted for section ends keeps them though the source took the pipeline serial (a hosted call a pool
        // thread cannot run, source/heap.rs / pgrc.rs): the end's arm read the decomposer's descriptor, and without the
        // section end the resident run is never sealed and the emit reads no row. The serial arm then runs its ALL claim
        // and the section end, as a full sort's decomposer-finishing descriptor over the same source does. RUNS-26: its
        // run's row (a fetch's key and locator) is the record the breaker cost reads, as a full sort's
        // D3024 (b): a wide emit kept on the leader after this top chose would run no section end (lower.rs `WIDE_KEPT_LATE`)
        if self.ends && !d.finish && d.driving_site < 0 && px.wide.contains(&k) {
            crate::refuse!(self.node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::ops::sort::lower::WIDE_KEPT_LATE) });
        }
        Pipeline { finish: d.finish || self.ends, ..d }.storing_rows(self.run.shape.row_bytes).keeping_rows(self.top_rows())
    }
}

impl<'g> TopEnd<'g> {
    /// The rows the run holds before it cuts (`open`'s soft cap, `soft_cap`) when a constant count and offset fix them (no
    /// offset is 0); 0 (none published) for a computed or parameter bound, a negative one (it raises), no count, a union's
    /// arm (the arms' tops meet in one merged run) and an exact top WITH TIES (never cut).
    fn top_rows(&self) -> u64 {
        let n = match (self.count, self.offset) {
            (B::Const(c), B::Const(o)) if c >= 0 && o >= 0 => c.saturating_add(o),
            (B::Const(c), B::All) if c >= 0 => c,
            _ => return 0,
        };
        match soft_cap(n) {
            _ if self.arm || (self.exact && self.ties) => 0,
            i64::MAX => 0,
            soft => soft as u64,
        }
    }
}

/// C-R6f: a top-N emit over a rerun set member through the late fetch or code.heads (only RunEmit re-opens per run).
pub(crate) const D_TOP_RERUN: &str = "a top-N late-fetch or code.heads emit over a rerun set member (only the run reader re-opens per run: D1337 C-R6f)";

/// The emit: runs/sort.rs's reader over the top's run (the Limit step above windows it).
fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    let Source::Breaker { slot } = p.source else { bug(node, "a top's emit over a source that is not its breaker") };
    let Some(life) = px.gx.life_of(k, slot) else { bug(node, "a top's emit with no producer") };
    let producer = life.0 as usize;
    let _ = form(px, producer);
    let (sd, cols) = sort_of(px, producer);
    if !px.gx.pipelines()[producer].ops.iter().any(|op| matches!(op, Op::Sort(_))) {
        bug(node, "a top's emit whose producer has no sort");
    }
    let shape = Shape::plan(px, px.gx.pipelines()[producer].node_id, &sd.keys, Some(sd.tlist), cols);
    // C-R6f: a rerun set re-reads only RunEmit's rows (rerun/sets.rs `members_close`); the late fetch's and code.heads's
    // emits are not proven to re-open
    let member = crate::rerun::sets::in_owner_set(px.gx, producer);
    if let Some((relid, attnos, _)) = crate::ops::sort::strategies::fetch::of(px, producer) {
        if member {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(D_TOP_RERUN) });
        }
        let (narrow, _) = crate::ops::sort::strategies::fetch::narrow(&shape, node, px.mcx());
        return crate::ops::sort::sort_raise::wrap_emit(px.gx, k, mcx::pg_box_dyn!(px.mcx(), crate::ops::sort::strategies::fetch::emit(px, k, narrow, &shape, relid, &attnos) => dyn SourceMech + 'g));
    }
    let top = TopDesc { k: producer, node: px.gx.pipelines()[producer].node_id, sd };
    let hm = heads(px, &top);
    if member && hm.is_some() {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(D_TOP_RERUN) });
    }
    let coded = coded(px, producer, sd, cols, &shape, hm.is_some());
    let (mcx, mut reader) = (px.mcx(), Some(RunEmit::over(px, k, coded)));
    // V4R-310 (L0-f): an engaged code.heads repeats each entries row by its rows (its emit's top is this reader's); PLANMEM:
    // the reader is boxed as the road's run reader when it wraps it, else as the emit (a PgBox has no trait upcast)
    let mut wrapped = || mcx::pg_box_dyn!(mcx, reader.take().unwrap_or_else(|| bug(node, "a top's run reader wrapped twice")) => dyn crate::face::runs::RunReader + 'g);
    let src = match hm.and_then(|m| m.emit(px, &TopDesc { k, ..top }, &mut wrapped)) {
        Some(src) => src,
        None => mcx::pg_box_dyn!(mcx, reader.take().unwrap_or_else(|| bug(node, "a top's run reader wrapped and kept")) => dyn SourceMech + 'g),
    };
    crate::ops::sort::sort_raise::wrap_emit(px.gx, k, src) // D1292: an empty producer's raise
}

/// V4R-310 (L0-f): code.heads's mechanism when its road is on and it engages `top` (the run then carries its mark column).
fn heads(px: &Px<'_, '_>, top: &TopDesc<'_, '_>) -> Option<&'static dyn crate::face::runs::CodeTopkMech> {
    let m = crate::frame::iface::need_strategy::<CodeTopkImpl>(&top.node, CODE_HEADS).0;
    (res::on(px, CODE_HEADS) && m.engages(px.gx, top) && m.marks()).then_some(m)
}

/// D538 (late-rendered code lanes, M3-WALK): the top's run with each non-key column that is the scan's own code lane (a bare Var of pipeline
/// `k`'s site whose lane is `Rep::Code`) held as its code word, rendered only at the rows the emit's consumer reads
/// (keyed/late_render.rs's rule), never at a row the cut or the bound drops. Only a run `k` alone feeds: the emit renders
/// through one producer's site (a union's arms each have their own).
fn coded(px: &mut Px<'_, '_>, k: usize, sd: &SortDesc<'_>, cols: &NodeList<'_>, full: &Shape, mark: bool) -> Shape {
    let mcx = px.mcx();
    let gx = px.gx;
    let fed = |s: u8| gx.pipelines().iter().filter(|q| breaker_of(&q.desc).is_some_and(|c| c.slot == s)).count();
    let alone = breaker_of(&gx.pipelines()[k].desc).is_some_and(|br| fed(br.slot) == 1);
    let scan = gx.pipelines()[k].source.relation().map(|r| r.0 as i32).filter(|_| alone);
    // the producer's site: its source's draft while `k` is made; numbered by the time its breaker's reader is made
    let codes: PgVec<'_, Option<(DictRef, u16)>> = {
        let site = match px.pipeline_site(k) { Some(i) => px.sites.get(i), None => px.site_mut(k).map(|d| &*d) };
        let code = |c: usize, n: Node<'_>| {
            let v = n.as_target_entry()?.expr.as_var().filter(|v| Some(v.varno) == scan && !full.keys.iter().any(|x| x.col as usize == c))?;
            match site?.lanes.iter().find(|l| l.id == LaneId::Attr(v.varattno))?.rep {
                Rep::Code { dict } => Some((dict, k as u16)),
                _ => None,
            }
        };
        cols.iter().enumerate().map(|(c, n)| code(c, n)).collect_in(mcx)
    };
    Shape::plan_coded(px, gx.pipelines()[k].node_id, &sd.keys, Some(sd.tlist), cols, &|c| codes.get(c).copied().flatten(), mark)
}
