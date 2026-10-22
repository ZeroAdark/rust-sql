//! Kind `srf.function` (L4; cards/M5.md `srf/function_scan.rs`, M5-plan §3.1, §3.5, §3.10): the FROM step over a
//! FunctionScan's one `src.result` row, an expanding step (H32). PG's FunctionScan materializes every function at the first
//! fetch, so the step opens every member at the row (its arguments evaluated once, in call order; every error raised
//! there) and then walks the rows: row `k` has each member's element `k` (a member past its end gives NULL: ROWS FROM's
//! padding; the row count is the longest member's) and, WITH ORDINALITY, the bigint `k + 1`. The offset word counts
//! the rows handed on and each member's opened set sits in frame words, so a stop inside the set resumes at row `k`
//! without re-opening (no argument is evaluated twice). The set loop polls cancel once per block of 64 rows (§3.10: one
//! row can open 10^10). Forms (face/srf.rs `Form`): the iterator (srf/iter.rs); a scalar call and an inlined body's
//! expression (`SrfKind::Scalar` / `Expr`): one row, the expression's value (a strict call on a NULL gives the one
//! all-NULL row PG gives). An entry with any other member (a library or C set function, a row type, a record) is form (c)
//! as a whole: PG's FunctionScan fills its store and the shared reader hands its rows on (srf/function_store.rs). An inline
//! entry runs at every open: in a pipeline re-run per re-open (srf/reopen_once.rs) it takes form (c) instead (D1657).
//! Owner M5-SERIES.
use gravity_decompose::ops::shared::srf::{SrfFn, SrfKind, SrfPlace};
use gravity_decompose::{catalog::typefacts, Op};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_program::{ExprLife, ExprOut, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::cell::{Cell, Nullable, Row};
use crate::face::kinds::{Face, KindId};
use crate::face::step::{Expand, ExpandKind, ExprMech, StepMech};
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, ResumeWord};
use crate::admit::refusal::Reason;
use crate::ops::Table;

use crate::ops::srf::face::Form;
use crate::ops::srf::iter as iter;
use crate::ops::srf::iter::{Iter, Opened};
use crate::ops::srf::function_store::store_step;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrfFunction, &StepMaker(make))], ..crate::ops::NONE };

/// The rows the set loop hands on between two cancel polls.
const BLOCK: i64 = 64;

/// One function of the FROM entry.
struct Member<'g> {
    form: Form,
    iter: Option<Iter>,
    /// Its arguments (the iterator's), or its expression (`Expr`, a scalar call).
    args: PgVec<'g, ExId>,
    /// Its output column (attno) and cell facts.
    col: i16,
    out: ExprOut,
    /// The frame words holding its opened set across a stop (`Opened::words`).
    words: PgVec<'g, i32>,
}

struct FromStep<'g> {
    node: i32,
    /// expr.core's face (V4R-240): the members' arguments and expressions.
    ex: &'static dyn ExprMech,
    members: PgVec<'g, Member<'g>>,
    /// WITH ORDINALITY's column.
    ordinality: Option<i16>,
    ncols: usize,
    offset: i32,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Srf(d) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a srf.function step over another op" }) };
    let SrfPlace::From { scanrelid, ordinality } = d.place else { crate::refuse!(node, Reason::Bug { payload: "a target-list set as srf.function" }) };
    let ex = crate::frame::iface::expr_face(&node).0;
    let ncols = d.functions.iter().map(|f| f.cols as usize).sum::<usize>() + ordinality as usize;
    // D1657: an inline entry `reopen_once` names would run at every re-open where PG's FunctionScan fills once and
    // rewinds; the store form fills once (function_store.rs: the once rule's Volatile protocol)
    if !d.functions.iter().all(|f| inline(px, ex, f, node)) || crate::ops::srf::reopen_once::reopen_once(gx, px.arena, k, d) {
        return store_step(px, ex, (k, j), d, (scanrelid as i32, ordinality, ncols), node);
    }
    let mut members: PgVec<'_, Member<'_>> = mcx::reserved_in(d.functions.len(), mcx);
    for (m, f) in d.functions.iter().enumerate() {
        members.push_in(member(px, ex, (k, j), m, f, node));
    }
    let ordinality = ordinality.then_some(ncols as i16);
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    mcx::pg_box_dyn!(px.mcx(), FromStep { node, ex, members, ordinality, ncols, offset } => dyn StepMech + 'g)
}

/// Function `f`'s one column's type when its value is that column: not a composite, a record or a pseudo-type (a
/// one-attribute composite, or a record under a column list, is one column too, and PG spreads its attribute).
pub(crate) fn plain_type<'p>(px: &Px<'_, 'p>, ex: &dyn ExprMech, f: &SrfFn<'p>, node: i32) -> Option<u32> {
    let (t, facts) = (ex.out_type(px.gx, f.node, node), px.gx.facts);
    // through a domain to its base, from the plan's type facts (the catalog is never asked at lowering)
    let mut b = t;
    while let Some(r) = facts.type_row(b).filter(|_| typefacts::row_of(b).is_none()) {
        if matches!(r.typtype, b'c' | b'p') {
            return None;
        }
        if r.typtype != b'd' || r.base == 0 || r.base == b {
            break;
        }
        b = r.base;
    }
    (b != types_core::catalog::RECORDOID && typefacts::facts_of(facts, t).is_some()).then_some(t)
}

/// Whether `f` runs inline: the iterator, or one row of a scalar call's or an inlined body's value; any other member takes
/// the FROM entry to form (c).
fn inline<'p>(px: &mut Px<'_, 'p>, ex: &dyn ExprMech, f: &SrfFn<'p>, node: i32) -> bool {
    f.cols == 1 && (iter::plan(px, f, node).is_some() || matches!(f.kind, SrfKind::Scalar | SrfKind::Expr) && plain_type(px, ex, f, node).is_some())
}

fn member<'g, 'p>(px: &mut Px<'g, 'p>, ex: &dyn ExprMech, (k, j): (usize, usize), m: usize, f: &SrfFn<'p>, node: i32) -> Member<'g> {
    let mcx = px.mcx();
    let (form, iter, out, args) = match (iter::plan(px, f, node), plain_type(px, ex, f, node)) {
        (Some(it), _) => {
            let iter::IterKind::Series { width, .. } = it.kind;
            let t = if width == 8 { types_core::INT8OID } else { types_core::catalog::INT4OID };
            let args = f.args.iter().map(|a| ex.compile_at(px, (k, j), a, None, node)).collect_in(mcx);
            (Form::Iter, Some(it), ExprOut { rep: Rep::Word { bytes: width }, typid: t, life: ExprLife::Row }, args)
        }
        (None, Some(t)) => {
            let e = ex.compile_at(px, (k, j), f.node, None, node);
            let tf = ex.tf_of(px.gx, t);
            (Form::Expr, None, ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid: t, life: ExprLife::Row }, mcx::pg_vec![in mcx; e])
        }
        (None, None) => crate::refuse!(node, Reason::Bug { payload: "a srf.function member that does not run inline" }),
    };
    let n = iter.map_or(0, |it| it.nwords());
    let words = (0..n).map(|i| px.frame_word(WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j), i: (4 * m + i) as u8 }) as i32 * 8).collect_in(mcx);
    Member { form, iter, args, col: f.resno, out, words }
}

impl<'g> FromStep<'g> {
    /// Every member opened on the row, in call order (an error leaves before any row is handed on, as PG's
    /// materialization raises before the first fetch returns), each opened set stored in its words.
    fn open<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> PgVec<'e, Value> {
        let mcx = env.mcx;
        let mut out: PgVec<'_, Value> = PgVec::new_in(mcx);
        for m in self.members.iter() {
            let Some(it) = m.iter else { continue };
            let args: PgVec<'_, (Value, Option<Value>)> = m.args.iter().map(|&a| arg(b, env, self.ex, self.node, row, a)).collect_in(mcx);
            let (o, err) = it.open(b, env, &args[..], true);
            raise(b, env, row, err);
            for (w, &at) in o.words(mcx).into_iter().zip(m.words.iter()) {
                env.store_word(b, at, w);
                out.push_in(w);
            }
        }
        out
    }

    /// Each member's opened set, from the words `open` stored (a resumed row) or a fresh open, and each expression member's
    /// value and NULL flag: evaluated at the open only (row 0 is handed on in the call that opens; a resumed row is past
    /// it, so a volatile or side-effecting expression runs once, as PG's materialization calls it once).
    #[allow(clippy::type_complexity)]
    fn opened<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> (PgVec<'e, Option<Opened>>, PgVec<'e, Option<(Value, Value)>>) {
        let mcx = env.mcx;
        let words: usize = self.members.iter().map(|m| m.words.len()).sum();
        let k0 = env.load_word(b, self.offset);
        let fresh = b.icmp(IntCond::Eq, k0, env.zero);
        let (l_open, l_held) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_open, &[]), (l_held, &[]));
        b.switch_to(l_open);
        let mut w = self.open(b, env, row);
        for m in self.members.iter().filter(|m| m.form == Form::Expr) {
            let (v, nl) = arg(b, env, self.ex, self.node, row, m.args[0]);
            w.extend_in([v, nl.unwrap_or(env.zero)]);
        }
        let go_t: PgVec<'_, Type> = w.iter().map(|&v| b.value_type(v)).collect_in(mcx);
        let l_go = b.create_block(&go_t[..]);
        b.br(l_go, &w[..]);
        b.switch_to(l_held);
        let mut w: PgVec<'_, Value> = self.members.iter().flat_map(|m| m.words.iter()).map(|&at| env.load_word(b, at)).collect_in(mcx);
        for &t in go_t[words..].iter().step_by(2) {
            let z = match t {
                Type::F64 => b.f64const(0.0),
                Type::F32 => b.f32const(0.0),
                Type::Bool => b.bconst(false),
                t => b.iconst(t, 0),
            };
            let nl = b.bconst(true);
            w.extend_in([z, nl]);
        }
        b.br(l_go, &w[..]);
        b.switch_to(l_go);
        let p = b.block_params(l_go);
        let (mut ws, mut es) = (p[..words].iter().copied(), p[words..].chunks(2));
        let sets = self.members.iter().map(|m| m.iter.map(|it| it.from_words(&ws.by_ref().take(m.words.len()).collect_in(mcx)[..]))).collect_in(mcx);
        let exprs = self.members.iter().map(|m| (m.form == Form::Expr).then(|| es.next().map(|e| (e[0], e[1]))).flatten()).collect_in(mcx);
        (sets, exprs)
    }

    /// The row count: the longest member's (an `Expr` member is one row).
    fn count(&self, b: &mut FunctionBuilder<'_>, env: &Env, sets: &[Option<Opened>]) -> Value {
        sets.iter().fold(env.zero, |n, s| {
            let m = s.map_or(env.one, |o| o.n);
            b.int_bin(IntBinOp::Umax, n, m)
        })
    }
}

/// Argument `a`'s word and NULL flag (an error raises through the row's raise block).
fn arg(b: &mut FunctionBuilder<'_>, env: &mut Env, ex: &dyn ExprMech, node: i32, row: &RowVals, a: ExId) -> (Value, Option<Value>) {
    let (v, valid) = ex.emit(b, env, node, row, a);
    let t = b.bconst(true);
    (v, Some(b.int_bin(IntBinOp::Xor, valid, t)))
}

/// A nonzero raise word leaves through the row's raise block with the rows handed on so far.
fn raise(b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals, err: Value) {
    if b.const_of(err).is_some_and(|c| c.1 == 0) {
        return;
    }
    let raised = b.icmp(IntCond::Ne, err, env.zero);
    let l_ok = b.create_block(&[]);
    match env.l_raise_row {
        Some(l) => b.cond_br(raised, (l, &[err, row.carry.out, row.next]), (l_ok, &[])),
        None => b.cond_br(raised, (env.l_raise, &[err]), (l_ok, &[])),
    }
    b.switch_to(l_ok);
}

impl<'g> StepMech for FromStep<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("srf.function offset", Base::Words, self.offset, Type::I64, 0));
        for &at in self.members.iter().flat_map(|m| m.words.iter()) {
            w.declare(OpenWord::open("srf.function opened set", Base::Words, at, Type::I64, 0));
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let mcx = env.mcx;
        let (sets, exprs) = self.opened(b, env, row);
        let n = self.count(b, env, &sets[..]);
        let k0 = env.load_word(b, self.offset);
        let k0 = resume_at(b, env, k0);
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(mcx);
        // l_elem(k, lim, cursors…, carry…): the rows of one block; l_chunk(k, cursors…, carry…): the cancel poll between
        // blocks; l_end(carry…): the set is through
        let ncur = sets.iter().flatten().count();
        let mut elem_t: PgVec<'_, Type> = mcx::pg_vec![in mcx; Type::I64; 2 + ncur];
        elem_t.extend_from_slice_in(&ctypes[..]);
        let l_elem = b.create_block(&elem_t[..]);
        let chunk_t: PgVec<'_, Type> = [Type::I64].into_iter().chain(elem_t[2..].iter().copied()).collect_in(mcx);
        let (l_chunk, l_body) = cancel::header(b, env.ctx, env.l_cancel, &chunk_t[..], env.mcx);
        let l_end = b.create_block(&ctypes[..]);
        let cur0: PgVec<'_, Value> = self.members.iter().zip(sets.iter()).filter_map(|(m, s)| Some(m.iter?.at(b, s.as_ref()?, k0, mcx)[0])).collect_in(mcx);
        let lim = block_end(b, env, k0, n);
        // D2967: the region's top once the set is open; each row's by-reference bytes go back to it once the row is
        // through (PG's per-tuple reset per FunctionScan row; what a later step or the end keeps it copies, as across
        // windows), so a set of any length runs in one row's bytes, not the whole set's
        let mark = env.hir.mark(b);
        let entry = mcx::concat_in(&[&[k0, lim][..], &cur0[..], &row.carry.params()[..]], mcx);
        b.br(l_elem, &entry[..]);
        // the poll, then the next block
        b.switch_to(l_body);
        let p = mcx::to_vec_in(&b.block_params(l_body)[..], mcx);
        let lim = block_end(b, env, p[0], n);
        let resume = mcx::concat_in(&[&[p[0], lim][..], &p[1..]], mcx);
        b.br(l_elem, &resume[..]);
        b.switch_to(l_elem);
        b.mark_loop_header(l_elem);
        env.bounded.push_in(l_elem);
        let p = mcx::to_vec_in(&b.block_params(l_elem)[..], mcx);
        let (k, lim, cur, carry) = (p[0], p[1], &p[2..2 + ncur], Carry::from_params(&p[2 + ncur..], env.mcx));
        let (l_row, l_past) = (b.create_block(&[]), b.create_block(&[]));
        let in_block = b.icmp(IntCond::Ult, k, lim);
        b.cond_br(in_block, (l_row, &[]), (l_past, &[]));
        b.switch_to(l_past);
        let done = b.icmp(IntCond::Uge, k, n);
        let back: PgVec<'_, Value> = [k].into_iter().chain(cur.iter().copied()).chain(carry.params()).collect_in(mcx);
        b.cond_br(done, (l_end, &carry.params()), (l_chunk, &back[..]));
        b.switch_to(l_row);
        let out = self.row_at(b, env, row, (&sets[..], &exprs[..]), k, cur, &carry);
        let k1 = b.int_bin(IntBinOp::Add, k, env.one);
        env.store_word(b, self.offset, k1);
        env.memo.clear();
        let seen = env.hir.seen();
        let c2 = next(b, env, &out);
        env.memo.clear();
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        // D3583: and the function-manager windows of the sites the row's later steps called
        env.hir.reset_seen(b, seen, env.mcx);
        let mut cur2: PgVec<'_, Value> = mcx::reserved_in(ncur, mcx);
        for (m, s) in self.members.iter().zip(sets.iter()) {
            if let (Some(it), Some(o)) = (m.iter, s) {
                let at = cur2.len();
                cur2.extend_in(it.advance(b, env, o, &cur[at..at + 1]));
            }
        }
        let tail = mcx::concat_in(&[&[k1, lim][..], &cur2[..], &c2.params()[..]], mcx);
        b.br(l_elem, &tail[..]);
        b.switch_to(l_end);
        env.store_word(b, self.offset, env.zero);
        Carry::from_params(b.block_params(l_end), env.mcx)
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Srf, offset: self.offset, held: self.members.iter().map(|m| m.words.len() as u8).sum() })
    }
}

/// The row an expanding step with `off` rows handed on re-enters: `off` itself (past the row a sink stopped after), or
/// `off - 1` when a later expanding step's offset word is set, i.e. that step stopped inside this row (a chain probe's
/// match, D824's lazy-build wait) and the same row must reach it again. project_set.rs and join probe_inner.rs follow the
/// same rule (D688). D824 found it missing here: a FROM function's row that a lazy build's wait stopped was skipped.
pub(crate) fn resume_at(b: &mut FunctionBuilder<'_>, env: &Env, off: Value) -> Value {
    let mut set = None;
    for &(_, o) in env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)) {
        let w = env.load_word(b, o);
        set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
    }
    let Some(set) = set else { return off };
    let set = b.icmp(IntCond::Ne, set, env.zero);
    let resumed = b.icmp(IntCond::Ne, off, env.zero);
    let inside = b.int_bin(IntBinOp::And, set, resumed);
    let back = b.int_bin(IntBinOp::Sub, off, env.one);
    b.select(inside, back, off)
}

impl<'g> FromStep<'g> {
    /// Row `k` of the set: each member's element `k` (NULL past its end), the ordinality `k + 1`; a stop after it resumes
    /// at this input row (`next` = `at`), where the offset word says how far the set got.
    #[allow(clippy::type_complexity)]
    fn row_at<'e>(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, row: &RowVals, (sets, exprs): (&[Option<Opened>], &[Option<(Value, Value)>]), k: Value, cur: &[Value], carry: &Carry) -> RowVals<'e> {
        let mut cells = mcx::reserved_in(self.ncols, env.mcx);
        let mut vals = mcx::reserved_in(self.ncols, env.mcx);
        let mut cols = mcx::pg_vec![in env.mcx; None; self.ncols];
        let mut c = 0;
        for ((m, s), e) in self.members.iter().zip(sets.iter()).zip(exprs.iter()) {
            let (v, null) = match (m.iter, s, e) {
                (Some(it), Some(o), _) => {
                    let (v, nl, _) = it.value(b, env, o, &cur[c..c + 1]);
                    c += 1;
                    let past = b.icmp(IntCond::Uge, k, o.n);
                    (v, b.int_bin(IntBinOp::Or, nl, past))
                }
                (_, _, Some((v, nl))) => {
                    let past = b.icmp(IntCond::Ne, k, env.zero);
                    (*v, b.int_bin(IntBinOp::Or, *nl, past))
                }
                _ => crate::refuse!(self.node, Reason::Bug { payload: "a srf.function member without its form's values" }),
            };
            cols[m.col as usize - 1] = Some(cells.len() as u16);
            cells.push_in(Cell::of_expr(m.out, cells.len() as u16, cells.len() as u16, Nullable::Maybe));
            vals.push_in((v, Some(null)));
        }
        if let Some(a) = self.ordinality {
            let ord = ExprOut { rep: Rep::Word { bytes: 8 }, typid: types_core::INT8OID, life: ExprLife::Row };
            cols[a as usize - 1] = Some(cells.len() as u16);
            cells.push_in(Cell::of_expr(ord, cells.len() as u16, cells.len() as u16, Nullable::Never));
            let k1 = b.int_bin(IntBinOp::Add, k, env.one);
            vals.push_in((k1, None));
        }
        RowVals { row: Row { cells }, vals, cols, carry: carry.copy_in(env.mcx), next: row.at, at: row.at }
    }
}

/// The end of the block that starts at row `k`: `k + min(n - k, 64)` (`n - k` is 0 past the end).
fn block_end(b: &mut FunctionBuilder<'_>, env: &Env, k: Value, n: Value) -> Value {
    let past = b.icmp(IntCond::Uge, k, n);
    let left = b.int_bin(IntBinOp::Sub, n, k);
    let left = b.select(past, env.zero, left);
    let blk = b.iconst(Type::I64, BLOCK);
    let step = b.int_bin(IntBinOp::Umin, left, blk);
    b.int_bin(IntBinOp::Add, k, step)
}
