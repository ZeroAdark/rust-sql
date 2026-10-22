//! A pulled run's reader (jmerge-0012, D2801): the words and the fetch of a step that reads a run while its producer fills
//! it, one pull at a time, as PostgreSQL's node fetches its child one tuple per ExecProcNode. Two readers share it:
//! merge.join's pulled inner (ops/mergejoin/exact_inner.rs, which chooses its own form) and the streamed sorted set
//! operation's right input (ops/setop/stream.rs). The run's producer is a pulled set (`MERGE_PULLED`: deferred, run by
//! exec's merge.markable object, exec ops/mergejoin/proto.rs) whose end stops once the run holds the reader's want
//! (face/slot_run.rs `pulled_end`, its want word `want_word`), and whose reader re-reads the run as stored after each pull
//! (`pulled_reader`). Owner M11-MERGE.
//!
//! The form: one row per pull (`Form::Exact`: the input's code may act, so it runs on exactly the rows PG fetches), or a
//! budget doubling from one row (`Form::Grows`, MJ-AHEAD: `calm` code, which may raise; a pull that raises after storing
//! the wanted row holds its error for the next pull, `m11_hold`, as PG raises it only when it fetches the row that
//! raised). The budget doubles up to `AHEAD_STEP` rows, then grows by an eighth of the stored rows (`ahead_next`): a
//! reader that stops reads at most PG's rows plus the larger of the two past them, not up to twice as many, and a whole
//! read still pays a pull count logarithmic in its rows. The reader's words are its open words `W_LIVE` / `W_WANT` /
//! `W_AHEAD` under its own face and op (`reader_of`): its other words take other indexes.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::{SetDesc, SetForm};
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::mergejoin::raw::{MergeHold, MergePull};
use gravity_program::protos::{Proto, ProtoKind, MERGE_PULLED};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::face::kinds::Face;
use crate::face::runs::SlotRun;
use crate::frame::env::Env;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::graph::Gx;

/// The reader's open words over the run: its stored rows as its run's last open counted them, the rows its next pull stores
/// up to (read by the run's end), and the budgeted pull's next budget.
pub(crate) const W_LIVE: u8 = 5;
pub(crate) const W_WANT: u8 = 6;
pub(crate) const W_AHEAD: u8 = 7;

/// A budgeted pull's growth past its doubling: the budget doubles from one row while it is under this many rows, and past
/// it is the larger of this and an eighth of the rows stored (a LEFT merge over a streamed group that PG stops at 10,606
/// of 28,500 groups read 16,383 under a budget that only doubled; a SEMI merge whose filtered index scan inner PG stops
/// at 335 rows read 511 under a doubling to 1,024, 383 under a step of 64 and 354 under 32). MEASURED: per-pull cost
/// against overshoot on the merge rows of the perf census; 64 -> 32 took two SEMI merges over a filtered index scan
/// under a LIMIT from 4.5 and 4.9 ms to 3.5 and 4.3 (W0 and W8 alike). Past 512 stored rows both steps grow by an
/// eighth, so a long read lands at another point of that growth and its overshoot moves either way (the LEFT merge
/// above: 11,253 -> 11,771 rows read, 40.8 -> 42.8 ms).
const AHEAD_STEP: i64 = 32;

/// How a run is pulled: one row per pull, or a budget that grows (MJ-AHEAD: doubling, then `AHEAD_STEP`'s growth), which
/// reads rows past PostgreSQL's fetch and holds a raise for the pull that reaches its row: the held error's licence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Exact,
    Grows(gravity_decompose::effects::Licence),
}

impl Form {
    /// The budgeted form for an input its caller found `calm` (no act, no user code; it may raise): it holds the held
    /// error's licence (`effects::held_error`), `None` where that is denied.
    pub(crate) fn grows() -> Option<Form> {
        gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, false, None).ok().map(Form::Grows)
    }
}

/// A pulled run's words: its proto's handle word, then the reader's frame words (byte offsets) and its form.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PullWords {
    h: u16,
    live: i32,
    want: i32,
    budget: i32,
    form: Form,
}

/// The step `(face, k, j)` that pulls pipeline `inner`'s run: a merge join over it as its inner, or a streamed sorted set
/// operation over it as its right input.
fn reader_of(gx: &Gx<'_, '_>, inner: usize) -> Option<(Face, usize, usize)> {
    let of = |k: usize, s: u8| gx.life_of(k, s).is_some_and(|l| l.0 as usize == inner);
    let reads = |k: usize, op: &Op<'_>| match op {
        Op::Join(JoinDesc::Merge { slot, .. }) if of(k, *slot) => Some(Face::Merge),
        Op::SetOp(SetDesc::Set { right, form: SetForm::Stream { .. }, .. }) if of(k, *right) => Some(Face::Setop),
        _ => None,
    };
    gx.pipelines().iter().enumerate().find_map(|(k, p)| p.ops.iter().enumerate().find_map(|(j, op)| reads(k, op).map(|f| (f, k, j))))
}

/// The frame word (byte offset) pipeline `inner`'s pulled end stops at: its reader's want.
pub(crate) fn want_word(px: &mut Px<'_, '_>, inner: usize) -> Option<i32> {
    let (face, k, j) = reader_of(px.gx, inner)?;
    Some(px.frame_word(WordKey::Open { face, k, j: crate::frame::px::key_j(j), i: W_WANT }) as i32 * 8)
}

/// The reader `(face, k, j)`'s pull of pipeline `inner`'s run in form `form`: its proto declared (one handle word over the
/// run's pipeline) and the reader's words of the run's stored rows, the pull's want and its budget.
pub(crate) fn declare(px: &mut Px<'_, '_>, (face, k, j): (Face, usize, usize), inner: usize, form: Form) -> PullWords {
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::Markable, inner as u16).words(h, 1).arg(MERGE_PULLED));
    let mut word = |i: u8| px.frame_word(WordKey::Open { face, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    PullWords { h, live: word(W_LIVE), want: word(W_WANT), budget: word(W_AHEAD), form }
}

/// MJ-AHEAD: reader pipeline `k` feeds no InitPlan's cell or CTE's set (sets run on demand), so the budgeted pull never
/// nests in another deferred set (D1114's chain-default classes name no such member).
pub(crate) fn unnested(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let mut todo: PgVec<'_, usize> = gx.graph.init_plans.iter().map(|i| i.pipeline as usize).collect_in(gx.mcx);
    let mut seen: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    while let Some(q) = todo.pop() {
        if q == k {
            return false;
        }
        if q >= ps.len() || seen.contains(&q) {
            continue;
        }
        seen.push_in(q);
        let src = if let Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
        todo.extend_in(ps[q].desc.consumes.iter().copied().chain(src).filter_map(|s| gx.life_of(q, s)).map(|l| l.0 as usize));
        todo.extend_in(gx.union_feeds(q).into_iter().chain(gx.run_arms(q)));
    }
    true
}

/// The budgeted pull's next budget after a pull of `budget` rows past `stored`: twice the budget, at most the larger of
/// `AHEAD_STEP` and an eighth of the stored rows.
fn ahead_next(b: &mut FunctionBuilder<'_>, stored: Value, budget: Value, _held: gravity_decompose::effects::Licence) -> Value {
    let (twice, three, step) = (b.int_bin(IntBinOp::Add, budget, budget), b.iconst(Type::I64, 3), b.iconst(Type::I64, AHEAD_STEP));
    let eighth = b.int_bin(IntBinOp::Sshr, stored, three);
    let room = b.int_bin(IntBinOp::Smax, eighth, step);
    b.int_bin(IntBinOp::Smin, twice, room)
}

impl PullWords {
    /// The stored rows and want words, 0 at the reader's open, and the budget, 1 row; `what` names the three.
    pub(crate) fn open_words(&self, w: &mut OpenWords, what: [&'static str; 3]) {
        w.declare(OpenWord::open(what[0], Base::Words, self.live, Type::I64, 0));
        w.declare(OpenWord::open(what[1], Base::Words, self.want, Type::I64, 0));
        w.declare(OpenWord::open(what[2], Base::Words, self.budget, Type::I64, 1));
    }

    /// At each window: the run's rows as its open counted them.
    pub(crate) fn keep(&self, b: &mut FunctionBuilder<'_>, env: &Env, n: Value) {
        env.store_word(b, self.live, n);
    }

    /// Whether row `ip` is past `run`. The run is first pulled past its stored rows (`m11_pull`, PG's ExecProcNode at the
    /// fetch) up to its want (the stored rows plus the budget, grown after each pull of a `Form::Grows` run), the run
    /// opened again after each (a growth moves it, and a budgeted pull may store rows before it reaches the input's end):
    /// past them only once it ended; a pull's or an open's error leaves the row through the exit, its error stashed, except
    /// a budgeted pull's raised after it stored row `ip`: held for the next pull (`m11_hold`), the row read.
    pub(crate) fn ended(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, run: &dyn SlotRun, ip: Value) -> Value {
        let (l_chk, l_pull, l_bad, l_ok, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::Bool]));
        b.br(l_chk, &[]);
        b.switch_to(l_chk);
        b.mark_loop_header(l_chk);
        env.bounded.push_in(l_chk);
        let stored = env.load_word(b, self.live);
        let (have, f) = (b.icmp(IntCond::Slt, ip, stored), b.bconst(false));
        b.cond_br(have, (l_out, &[f]), (l_pull, &[]));
        b.switch_to(l_pull);
        let budget = env.load_word(b, self.budget);
        let want = b.int_bin(IntBinOp::Add, stored, budget);
        env.store_word(b, self.want, want);
        if let Form::Grows(held) = self.form {
            let next = ahead_next(b, stored, budget, held);
            env.store_word(b, self.budget, next);
        }
        let host = b.declare_host_once("m11_pull", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<MergePull> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let hw = b.load(Type::I64, env.params, self.h as i32 * 8);
        let st = b.call_host(host, &[env.ctx, hw], Some(Type::I64)).expect("m11_pull returns its status");
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        b.mark_cold(l_bad);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        match self.form {
            Form::Exact => b.br(env.x.l_exit, &[st]),
            Form::Grows(_) => {
                // the error held for the next pull, then the run as the pull left it: row `ip` stored is read, else the
                // next pull (back at the check) raises the held error
                let host = b.declare_host_once("m11_hold", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<MergeHold> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
                let rc = b.call_host(host, &[env.ctx, hw], Some(Type::I64)).expect("m11_hold returns its status");
                let (lost, l_held) = (b.icmp(IntCond::Slt, rc, env.zero), b.create_block(&[]));
                b.cond_br(lost, (env.x.l_exit, &[rc]), (l_held, &[]));
                b.switch_to(l_held);
                let x = env.x;
                let n2 = run.open(b, env, &x);
                env.store_word(b, self.live, n2);
                b.br(l_chk, &[]);
            }
        }
        b.switch_to(l_ok);
        let (l_more, t) = (b.create_block(&[]), b.bconst(true));
        let more = b.icmp(IntCond::Ne, st, env.zero);
        match self.form {
            Form::Exact => b.cond_br(more, (l_more, &[]), (l_out, &[t])),
            Form::Grows(_) => {
                // a budgeted pull may store rows and then reach the input's end: the run as it left it decides
                let l_fin = b.create_block(&[]);
                b.cond_br(more, (l_more, &[]), (l_fin, &[]));
                b.switch_to(l_fin);
                let x = env.x;
                let n2 = run.open(b, env, &x);
                env.store_word(b, self.live, n2);
                let past = b.icmp(IntCond::Sge, ip, n2);
                b.br(l_out, &[past]);
            }
        }
        b.switch_to(l_more);
        let x = env.x;
        let n2 = run.open(b, env, &x);
        env.store_word(b, self.live, n2);
        b.br(l_chk, &[]);
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }
}
