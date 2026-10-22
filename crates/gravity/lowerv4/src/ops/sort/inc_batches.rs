//! group_core-0259: an unbounded incremental sort's stop under a Unique a counted LIMIT stops (decompose facts/stream.rs
//! hands it down; runs.incremental's end, incremental.rs, counts every row into it); the stop road: the same stop under a
//! sorted GroupAggregate a counted LIMIT stops, counting prefix groups (`groups`). Owner group (the streamed-group /
//! stop-early road).
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::frame::env::Env;
use crate::frame::row::Carry;

/// PostgreSQL's own row counts (nodeIncrementalSort.c, no bound: `ExecSetTupleBound` stops at a Unique), semantics to
/// match, not a tuning: the batch row that is its pivot (DEFAULT_MIN_GROUP_SIZE) and the batch rows past which the
/// presorted-prefix load starts (DEFAULT_MAX_FULL_SORT_GROUP_SIZE).
pub(super) const PIVOT_ROW: i64 = 32;
pub(super) const PREFIX_LOAD_PAST: i64 = 64;

/// group_core-0259 (decompose facts/stream.rs `Stop`): PostgreSQL's unbounded batches under a Unique a LIMIT stops. A
/// full-sort load takes 32 rows (the 32nd the pivot), then rows of the pivot's prefix; the first row of another prefix is
/// carried over and the batch is handed on. Past 64 rows the load hands on the batch's earlier prefix groups and then
/// reads the rest of the pivot's prefix group (a presorted-prefix load), which ends at the next prefix the same way. A
/// batch is whole prefix groups, so the Unique above hands on each group's distinct rows (its columns are the sort's
/// keys; or exactly its prefix keys, `per_group`: one row per group); once the rows handed on hold `need` distinct rows
/// PostgreSQL's Limit pulls no more and no load starts: the walk ends at that row (it was read: PostgreSQL read it too).
/// Exact where every prefix key is one exact order word and the producer is serial (its rows in prefix order); a row out
/// of prefix order leaves the statement UNSUPPORTED at run time.
/// D2432: a run put mid-group (the run's room ends inside a prefix group; PostgreSQL's batch holds that whole group
/// still) puts the group in parts (`split`); its distinct rows are then known only between the most of one part and
/// their sum (one under `per_group`), so the batch's closed groups and the rows handed on are ranges (`w_held` ..
/// `w_held_hi`, `w_out` .. `w_out_hi`): the walk ends where even the lower bound meets `need`, goes on while the upper
/// bound is under it, and between the two (PostgreSQL's Limit may stop there or not) leaves the statement UNSUPPORTED. A
/// run put where the row opens a prefix group closes the group before it (`pend`), counted at that row as if no put came
/// between.
pub(crate) struct Batches {
    pub(crate) need: i64,
    /// The group form (the stop road, decompose facts/stream.rs `group_stop`): nothing before the sort raises or acts, so
    /// PostgreSQL's batches are not modelled: each prefix group is handed on as it closes (`closed`), one group or more of
    /// the sorted GroupAggregate above, and the walk ends at the first row past `need` of them; a run put mid-stream (its
    /// split group counted late) only stops later, so it needs no UNSUPPORTED.
    pub(crate) groups: bool,
    /// topn_core-0240 (decompose facts/stream.rs `Stop::per_group`): the Unique's columns are the sort's prefix keys, so a
    /// closed prefix group holds one distinct row (`IncEnd::span` answers 1, not the group's full-compare count).
    pub(crate) per_group: bool,
    /// decompose facts/stream.rs `Stop::late`: nothing before the sort raises or acts, so where only the upper bound of a
    /// split group's distinct rows meets `need` the walk reads on (it stops later than PostgreSQL, never sooner).
    pub(crate) late: bool,
    /// D3376 (decompose `Stop::rows`) and D2316's held form (inc_held.rs: no Unique above, `need` never met): the rows
    /// handed on, not distinct rows (`IncEnd::span` answers the group's rows), so a split group's parts sum exactly.
    pub(crate) rows: bool,
    /// Frame words: the open batch's rows (a full-sort load's count), its mode (0 full-sort, 1 presorted-prefix), the
    /// distinct rows of its closed groups not handed on, the distinct rows handed on.
    pub(crate) w_rows: i32,
    pub(crate) w_mode: i32,
    pub(crate) w_held: i32,
    pub(crate) w_out: i32,
    /// D2432: the open group's parts put while open (the most distinct rows of one part, their sum; 0, 0 when none), the
    /// upper bounds of the distinct rows handed on and held (`w_out`, `w_held` are the lower), and the distinct rows plus
    /// one of a group a put closed (0: none).
    pub(crate) w_split_lo: i32,
    pub(crate) w_split_hi: i32,
    pub(crate) w_out_hi: i32,
    pub(crate) w_held_hi: i32,
    pub(crate) w_pend: i32,
}

impl Batches {
    /// Between `lo` and `hi` distinct rows handed on (equal unless a split group's, D2432); where the lower bound of the rows
    /// handed on meets `need` the walk ends, where only the upper bound does the statement is UNSUPPORTED.
    fn hand_on(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, lo: Value, hi: Value, carry: &Carry, next: Value) {
        let (out, out_hi) = (env.load_word(b, self.w_out), env.load_word(b, self.w_out_hi));
        let (out, out_hi) = (b.int_bin(IntBinOp::Add, out, lo), b.int_bin(IntBinOp::Add, out_hi, hi));
        env.store_word(b, self.w_out, out);
        env.store_word(b, self.w_out_hi, out_hi);
        let need = b.iconst(Type::I64, self.need);
        let met = b.icmp(IntCond::Sge, out, need);
        let (l_stop, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(met, (l_stop, &[]), (l_go, &[]));
        b.switch_to(l_stop);
        env.full(b, carry, next);
        b.switch_to(l_go);
        if !self.late {
            let maybe = b.icmp(IntCond::Sge, out_hi, need);
            unsupported_exit(b, env, maybe);
        }
    }

    /// The group form: a prefix group closed, one group handed on.
    pub(crate) fn closed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, next: Value) {
        let one = env.one;
        self.hand_on(b, env, one, one, carry, next);
    }

    /// D2432: a part of the open group, `d` distinct rows, put while the group stays open (the run's room ended inside it):
    /// the group's distinct rows are at least the most of one part and at most their sum.
    pub(crate) fn split(&self, b: &mut FunctionBuilder<'_>, env: &Env, d: Value) {
        let (lo, hi) = (env.load_word(b, self.w_split_lo), env.load_word(b, self.w_split_hi));
        let more = b.icmp(IntCond::Sgt, d, lo);
        let lo = b.select(more, d, lo);
        // under `per_group` every part is the group's one row; under `rows` the parts' rows add up exactly
        let hi = if self.per_group { lo } else { b.int_bin(IntBinOp::Add, hi, d) };
        let lo = if self.rows { hi } else { lo };
        env.store_word(b, self.w_split_lo, lo);
        env.store_word(b, self.w_split_hi, hi);
    }

    /// The closed group's distinct rows as `(lo, hi)` from its last part's `d` and the parts put before it (D2432), the
    /// parts then cleared; `(d, d)` for a group never split.
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &Env, d: Value) -> (Value, Value) {
        let (lo, hi) = (env.load_word(b, self.w_split_lo), env.load_word(b, self.w_split_hi));
        let more = b.icmp(IntCond::Sgt, d, lo);
        let lo = b.select(more, d, lo);
        let hi = if self.per_group { lo } else { b.int_bin(IntBinOp::Add, hi, d) };
        let lo = if self.rows { hi } else { lo };
        env.store_word(b, self.w_split_lo, env.zero);
        env.store_word(b, self.w_split_hi, env.zero);
        (lo, hi)
    }

    /// D2432: a put closed the open group (`d` distinct rows in its last part) before the row that opens the next: that row
    /// counts it (`row`).
    pub(crate) fn pend(&self, b: &mut FunctionBuilder<'_>, env: &Env, d: Value) {
        let p = b.int_bin(IntBinOp::Add, d, env.one);
        env.store_word(b, self.w_pend, p);
    }

    /// A run's row that is not past its open group's first (`boundary`'s, the first row of a run or of the walk): the row
    /// opening a group a put closed (`pend`) or else a row of the open group's prefix.
    pub(crate) fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, next: Value) {
        let p = env.load_word(b, self.w_pend);
        let [l_open, l_same, l_done] = [(); 3].map(|_| b.create_block(&[]));
        let some = b.icmp(IntCond::Ne, p, env.zero);
        b.cond_br(some, (l_open, &[]), (l_same, &[]));
        b.switch_to(l_open);
        env.store_word(b, self.w_pend, env.zero);
        let d = b.int_bin(IntBinOp::Sub, p, env.one);
        self.opened(b, env, d, carry, next);
        b.br(l_done, &[]);
        b.switch_to(l_same);
        self.same(b, env, carry, next);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// A row of the open group's prefix (or the first): a full-sort load counts it, and its 65th row hands on the batch's
    /// closed groups and turns to the presorted-prefix load.
    pub(crate) fn same(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, next: Value) {
        let mode = env.load_word(b, self.w_mode);
        let [l_full, l_switch, l_done] = [(); 3].map(|_| b.create_block(&[]));
        let full = b.icmp(IntCond::Eq, mode, env.zero);
        b.cond_br(full, (l_full, &[]), (l_done, &[]));
        b.switch_to(l_full);
        let rows = env.load_word(b, self.w_rows);
        let rows = b.int_bin(IntBinOp::Add, rows, env.one);
        env.store_word(b, self.w_rows, rows);
        let past = b.iconst(Type::I64, PREFIX_LOAD_PAST);
        let past = b.icmp(IntCond::Sgt, rows, past);
        b.cond_br(past, (l_switch, &[]), (l_done, &[]));
        b.switch_to(l_switch);
        let (held, held_hi) = (env.load_word(b, self.w_held), env.load_word(b, self.w_held_hi));
        env.store_word(b, self.w_held, env.zero);
        env.store_word(b, self.w_held_hi, env.zero);
        env.store_word(b, self.w_mode, env.one);
        self.hand_on(b, env, held, held_hi, carry, next);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// A row opening a prefix group after one of `d` distinct rows closed (its last part's, D2432: a split group's are a
    /// range): in a presorted-prefix load or past a full-sort load's 32 rows the batch ends (the row carried over into the
    /// next) and is handed on; else the row joins it.
    pub(crate) fn opened(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, d: Value, carry: &Carry, next: Value) {
        let (d, d_hi) = self.whole(b, env, d);
        let (rows, mode, held) = (env.load_word(b, self.w_rows), env.load_word(b, self.w_mode), env.load_word(b, self.w_held));
        let held_hi = env.load_word(b, self.w_held_hi);
        let held_hi = b.int_bin(IntBinOp::Add, held_hi, d_hi);
        let held = b.int_bin(IntBinOp::Add, held, d);
        let pivot = b.iconst(Type::I64, PIVOT_ROW);
        let (prefix, filled) = (b.icmp(IntCond::Ne, mode, env.zero), b.icmp(IntCond::Sge, rows, pivot));
        let ends = b.int_bin(IntBinOp::Or, prefix, filled);
        let [l_end, l_join, l_done] = [(); 3].map(|_| b.create_block(&[]));
        b.cond_br(ends, (l_end, &[]), (l_join, &[]));
        b.switch_to(l_end);
        env.store_word(b, self.w_held, env.zero);
        env.store_word(b, self.w_held_hi, env.zero);
        env.store_word(b, self.w_mode, env.zero);
        env.store_word(b, self.w_rows, env.one);
        self.hand_on(b, env, held, held_hi, carry, next);
        b.br(l_done, &[]);
        b.switch_to(l_join);
        env.store_word(b, self.w_held, held);
        env.store_word(b, self.w_held_hi, held_hi);
        let rows = b.int_bin(IntBinOp::Add, rows, env.one);
        env.store_word(b, self.w_rows, rows);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }
}

/// Leaves the statement UNSUPPORTED at run time (`Batches`' run put mid-stream, a row out of prefix order); the builder
/// continues in a block of its own.
pub(crate) fn unsupported_exit(b: &mut FunctionBuilder<'_>, env: &Env, when: Value) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(when, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    b.br(env.x.l_exit, &[uns]);
    b.switch_to(l_ok);
}
