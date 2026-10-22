//! Road `topk.boundfirst` (cards/M3.md L5, H07a, I26): the run's bound as its first filter. Once a cut left the run holding
//! its first `n` rows (runs/topk_rows.rs), row `n - 1`'s key-0 `(flags, word)` pair (keylane.rs: flags then word,
//! unsigned, DESC and NULLS folded) is the bound (`RtConst::Bound`), and a row whose key-0 pair is past it cannot enter the
//! top: it is dropped before its row is stored. A row equal to the bound passes (a tie on key 0 is decided by the later
//! keys, WITH TIES keeps it). The bound is armed by the cut, never by an estimate, so arming changes cost, never rows.
//! An open re-arms it from a run a cut left ordered (`rearm`: a walk or a parallel top opens its pipeline at every claim
//! over the same run). It applies to every row top (applies `topk_rows`), and it sits after the row's projection:
//! PostgreSQL projects every row before its Sort, and so does this top (D449). Budget 160 lines. Owner M3-TOPS.
use gravity_abi::breaker::SORTRUN_HDR;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::entries::RUN_CUT;

use crate::face::consts::{RtConst, RtForm, RtSlot};
use crate::face::kinds::Face;
use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::ops::Table;

crate::strategies! {
    /// The run's bound as the first filter.
    TOPK_BOUNDFIRST = 65, "topk.boundfirst", TopkRows;
}

pub(crate) const ROWS: Table = Table { strategies: &[(TOPK_BOUNDFIRST, &())], ..crate::ops::NONE };

/// The bound's frame words for producer `k`: its armed flag and the pair (`RtSlot`'s value is the flags word; the word
/// follows at `word`).
#[derive(Clone, Copy)]
pub(crate) struct Bound {
    pub slot: RtSlot,
    pub word: i32,
}

/// The frame words' indexes: the Runs face's one table (frame/px.rs runs_word, D482).
use crate::frame::px::runs_word::{BOUND_ARMED as W_ARMED, BOUND_FLAGS as W_FLAGS, BOUND_WORD as W_WORD};

impl Bound {
    /// Producer `k`'s bound when road topk.boundfirst is on.
    pub(crate) fn of(px: &mut Px<'_, '_>, k: usize) -> Option<Bound> {
        if !crate::ops::sort::strategies::resident::on(px, TOPK_BOUNDFIRST) {
            return None;
        }
        let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
        let (armed, value, word) = (w(W_ARMED), w(W_FLAGS), w(W_WORD));
        Some(Bound { slot: RtSlot { what: RtConst::Bound, form: RtForm::Scalar, value, armed }, word })
    }

    /// Unarmed at every open: `rearm` or the first cut of the open arms it again.
    pub(crate) fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("top bound armed", Base::Words, self.slot.armed, Type::I64, 0));
        w.declare(OpenWord::open("top bound flags", Base::Words, self.slot.value, Type::I64, 0));
        w.declare(OpenWord::open("top bound word", Base::Words, self.word, Type::I64, 0));
    }

    /// Whether a row whose key-0 pair is `pair` may enter the top: the bound is unarmed, or the pair is not past it.
    pub(crate) fn passes(&self, b: &mut FunctionBuilder<'_>, env: &Env, pair: [Value; 2]) -> Value {
        let (armed, bf) = self.slot.read(b, env);
        let bw = env.load_word(b, self.word);
        let gt_f = b.icmp(IntCond::Ugt, pair[0], bf);
        let eq_f = b.icmp(IntCond::Eq, pair[0], bf);
        let gt_w = b.icmp(IntCond::Ugt, pair[1], bw);
        let tie_past = b.int_bin(IntBinOp::And, eq_f, gt_w);
        let past = b.int_bin(IntBinOp::Or, gt_f, tie_past);
        let on = b.icmp(IntCond::Ne, armed, env.zero);
        let drop = b.int_bin(IntBinOp::And, on, past);
        let t = b.bconst(true);
        b.int_bin(IntBinOp::Xor, drop, t)
    }

    /// At the open, over the live `run` of `rb`-byte rows: a run a cut left holding its first `n` rows or more in key
    /// order (`RUN_CUT`) arms row `n - 1`'s key-0 pair, the pair that cut armed and exec merge.rs `run_bound` reads between
    /// claims; rows stored since follow it. Without it each claim stored rows up to the soft cap and cut again (a select
    /// over `max(1024, 4n)` rows and a sort of `n` per claim: a walk's cost per granule grew with its LIMIT).
    pub(crate) fn rearm(&self, b: &mut FunctionBuilder<'_>, env: &Env, run: Value, n: Value, rb: u32) {
        let (l_live, l_read, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
        let live = b.icmp(IntCond::Ne, ri, env.zero);
        b.cond_br(live, (l_live, &[]), (l_done, &[]));
        b.switch_to(l_live);
        let p2 = b.load_zext(Type::I8, run, crate::ops::sort::strategies::natural::PAD2);
        let bit = b.iconst(Type::I64, RUN_CUT as i64);
        let cut = b.int_bin(IntBinOp::And, p2, bit);
        let cut = b.icmp(IntCond::Ne, cut, env.zero);
        let len = b.load_zext(Type::I32, run, 4);
        let full = b.icmp(IntCond::Sge, len, n);
        let ok = b.int_bin(IntBinOp::And, cut, full);
        b.cond_br(ok, (l_read, &[]), (l_done, &[]));
        b.switch_to(l_read);
        let last = b.int_bin(IntBinOp::Sub, n, env.one);
        let rbv = b.iconst(Type::I64, rb as i64);
        let off = b.int_bin(IntBinOp::Mul, last, rbv);
        let at = b.ptr_add(run, off, 1, SORTRUN_HDR as i32);
        let (f, w) = (b.load(Type::I64, at, 0), b.load(Type::I64, at, 8));
        env.store_word(b, self.slot.value, f);
        env.store_word(b, self.word, w);
        env.store_word(b, self.slot.armed, env.one);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// Whether the live `run` a cut left ordered (`RUN_CUT`) holds more than `n` rows past its first `n`: a claim's finish cuts it
    /// then (topk_rows.rs `TopEnd::finish`). False for a null run (a section end's call).
    pub(crate) fn piled(&self, b: &mut FunctionBuilder<'_>, env: &Env, run: Value, n: Value) -> Value {
        let (l_live, l_join) = (b.create_block(&[]), b.create_block(&[Type::Bool]));
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
        let live = b.icmp(IntCond::Ne, ri, env.zero);
        let no = b.bconst(false);
        b.cond_br(live, (l_live, &[]), (l_join, &[no]));
        b.switch_to(l_live);
        let p2 = b.load_zext(Type::I8, run, crate::ops::sort::strategies::natural::PAD2);
        let bit = b.iconst(Type::I64, RUN_CUT as i64);
        let cut = b.int_bin(IntBinOp::And, p2, bit);
        let cut = b.icmp(IntCond::Ne, cut, env.zero);
        let len = b.load_zext(Type::I32, run, 4);
        let past = b.int_bin(IntBinOp::Sub, len, n);
        let over = b.icmp(IntCond::Sgt, past, n);
        let both = b.int_bin(IntBinOp::And, cut, over);
        b.br(l_join, &[both]);
        b.switch_to(l_join);
        b.block_params(l_join)[0]
    }

    /// The cut's pair at `out` (two words) armed as the bound when `full` (the run held at least `n` rows).
    pub(crate) fn arm(&self, b: &mut FunctionBuilder<'_>, env: &Env, out: Value, full: Value) {
        let (f, w) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
        let (of, ow, oa) = (env.load_word(b, self.slot.value), env.load_word(b, self.word), env.load_word(b, self.slot.armed));
        let (f, w) = (b.select(full, f, of), b.select(full, w, ow));
        let a = b.select(full, env.one, oa);
        env.store_word(b, self.slot.value, f);
        env.store_word(b, self.word, w);
        env.store_word(b, self.slot.armed, a);
    }
}

#[cfg(test)]
mod tests {
    /// D482: a pipeline that reads a sort run and ends in a top keeps the reader's words, the run end's own word, the top's
    /// count and soft cap and the bound's three, all under the same `Open { Runs, k, usize::MAX }` key: every index is its
    /// own word.
    #[test]
    fn a_top_over_a_run_reader_has_distinct_words() {
        use crate::frame::px::runs_word::*;
        let mut i = vec![READER_MODE, READER_CURSOR, READER_TOTAL, END_RUN, TOP_N, TOP_SOFT, BOUND_ARMED, BOUND_FLAGS, BOUND_WORD];
        i.sort_unstable();
        i.dedup();
        assert_eq!(i.len(), 9, "two of the pipeline's Runs words share an index: {i:?}");
    }
}
