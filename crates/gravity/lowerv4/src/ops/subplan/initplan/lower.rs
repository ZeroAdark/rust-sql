//! Kind `nest.initplan` (InitPlan, lazy at first read; M8-plan §3.7, R6-L12, D237; cards/M8.md L3, B1). Owner M8-SUB.
//! - The end. An InitPlan's pipeline (graph.rs `end_need`, L0-g) ends here instead of the sink. It folds the rows by the
//!   link into the Init cell, as PG's `ExecSetParamPlan` does (nodeSubplan.c:1084-1258):
//!   - EXISTS: true at the first row, and the pipeline stops there, Done;
//!   - EXPR / ROWCOMPARE / MULTIEXPR: the first row's columns, one pair per setParam; 21000 at a second row, which the
//!     pipeline reads as PG does;
//!   - ARRAY: every row's first column accumulated (RT-2b `m8_array_*`, nest/subplan_result.rs, PG's
//!     accumArrayResultAny), the array copied into the cell in `finish`; no row gives '{}' (makeArrayResultAny);
//!   - no row: NULL (EXISTS: false), in `finish`; an `INIT_EXISTS` EXISTS (raise-free, parallel-safe): false is the cell's
//!     value from the run's start (exec raw.rs `ensure`), so its pipeline may run as a section (D2998).
//! - The cell is the Init proto's runtime object (exec v4/m8.rs). The runtime lays it into the proto's handle word once
//!   per execution. Its pairs are at `init_pair(k)`: the value word, then the null word (kernels host_abi/v4/m8.rs).
//! - A by-reference column is copied at its row (`m8_init_copy`, into the cell's arena for the execution). A later window
//!   may release the page, the slot or the scratch the datum points into.
//! - The proto marks the pipeline and its producers, so the producer order skips them. The runtime runs them at a
//!   reader's first `m8_init_run`, once per execution. Every reader runs on the leader, whose frame holds the cell: emit
//!   pins each pipeline whose code can call `m8_init_run` (frame/program.rs, D1035), except the one reader of an
//!   `INIT_EAGER` Init (D1035 step 2), whose members the runtime runs at that reader's start when it runs on workers.
//!   Running a raising InitPlan ahead of its first read departs from PostgreSQL (v5 effects, `Departure::Ahead`): it is
//!   eager only under a licence, whose recheck re-runs the statement in plan order if the run ahead fails (errorhold;
//!   the run ahead's held error is gone); denied (a write, a row lock, the plan-order run) it is lazy.
//! - The reader's load is `InitLoad`, below: the Param node inside an expression (gravity_expr `LazySites`,
//!   step/expr_ext_lazy.rs), an index key (frame/site.rs), a LIMIT bound. Each thread's frame pair (params.rs `word`)
//!   caches the value; its flags' SET bit decides whether to call the host, and no IR load reads the cell to decide it.
//! - D1262: an InitPlan whose members a rerun set holds is re-decided per re-open, as PG's ExecReScanSetParamPlan: the set's
//!   run re-arms its cell and its next read runs the members again (exec rerun.rs, initplan init.rs `rearm`).
//! - Fails closed by name:
//!   - an InitPlan whose members a rerun set reaching outside it holds only in part (D1262; a set wholly inside it, a
//!     recursive union's generations or a nest's inner not holding the InitPlan's own pipeline, re-runs only within the
//!     cell's one run; a member in a CTE's plan the set holds nothing of is kept, as PG keeps it);
//!   - an ARRAY InitPlan of arrays, records or cstrings (the ARRAY SubPlan's names, nest/subplan_result.rs).
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::Op;
use gravity_expr::byref;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::frame::EndMaker;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::rerun::params;
use crate::rerun::params::InitLoad;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep;

use crate::ops::subplan::result as sr;
use crate::ops::subplan::result::AccOp;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestInitplan, &EndMaker(make)), (KindId::NestInitplan, &InitLoad(load))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// One result column: its entry in the row (`attno - 1` of the subplan's tlist, as `slot_getattr` reads it), its type
/// facts, its `typlen` when by reference.
struct Col {
    entry: usize,
    typ: u32,
    tf: TypeFacts,
    byref: Option<i16>,
}

struct Init<'g> {
    node: i32,
    exists: bool,
    /// The handle word (a kernel word the runtime lays the Init cell's address into).
    h: u16,
    /// EXPR / ROWCOMPARE / MULTIEXPR: the column of each setParam, in order.
    cols: PgVec<'g, Col>,
    /// The per-open frame word counting the rows folded (0 or 1: a second raises 21000).
    seen: i32,
    /// ARRAY: the SubResult accumulator's handle word (RT-2b, one per lane; the Init runs on one).
    acc: Option<u16>,
    /// D2998: an `INIT_EXISTS` Init: the runtime sets its cell false at each run's start, so `finish` stores nothing.
    cell_reset: bool,
    /// Construct C2: the licence its members run ahead of PostgreSQL's first read of the param under (`INIT_EAGER`:
    /// licence()'s, with a recheck when the plan may raise), else the one a section's workers read past the row
    /// PostgreSQL stops at under (`INIT_EXISTS`: unobservable's). `None`: it runs at PostgreSQL's point. Held, not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

/// Whether pipeline `q` is a member of a (non-CTE) InitPlan: a rerun set holding it re-arms the InitPlan, never runs it.
pub(crate) fn member(gx: &Gx<'_, '_>, q: usize) -> bool {
    gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte)).any(|i| members(gx, i.pipeline as usize).contains(&q))
}

/// The InitPlan's members: pipeline `k` and, transitively, every producer of a slot one of them reads (the range the
/// runtime runs on demand), every arm folding into that production included (D393 append.arms, the runtime's `arms_of`).
fn members<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    let mut out = mcx::pg_vec![in gx.mcx; k];
    let mut i = 0;
    while let Some(&q) = out.get(i) {
        i += 1;
        for &s in gx.pipelines()[q].desc.consumes.iter() {
            for l in gx.life_of(q, s).map_or_else(|| mcx::PgVec::new_in(gx.mcx), |l| arms(gx, l.0 as usize, q, s)) {
                if !out.contains(&l) {
                    out.push_in(l);
                }
            }
        }
    }
    out
}

/// The pipelines whose rows fold into the production `l` of slot `s` that pipeline `q` reads: `l` and the producers of `s`
/// next to it with no reader of `s` between them (an Append's arms, the run the driver opens at its first arm).
fn arms<'g>(gx: &Gx<'g, '_>, l: usize, q: usize, s: u8) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let produces = |j: usize| ps[j].desc.produces() == Some(s);
    let reads = |j: usize| ps[j].desc.consumes.contains(&s);
    let before = (0..l).rev().take_while(|&j| !reads(j)).filter(|&j| produces(j));
    let after = (l + 1..q).take_while(|&j| !reads(j)).filter(|&j| produces(j));
    core::iter::once(l).chain(before).chain(after).collect_in(gx.mcx)
}

/// The InitPlan's columns `0..n` of its last projection (junk entries follow every result column).
fn cols<'g>(px: &Px<'g, '_>, k: usize, n: usize, node: i32) -> PgVec<'g, Col> {
    let gx = px.gx;
    let Some(list) = gx.pipelines()[k].ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else {
        shape(node, "an InitPlan whose row has no projection")
    };
    let mut out = mcx::reserved_in(n, px.mcx());
    for (entry, e) in list.iter().enumerate().take(n) {
        let Some(te) = e.as_target_entry().filter(|te| !te.resjunk) else { shape(node, "an InitPlan result column that is not a result entry") };
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        let Some(tf) = typefacts::facts_of(gx.facts, typ) else { shape(node, "an InitPlan result of a type the catalog does not know") };
        if tf.typlen == -2 {
            shape(node, "an InitPlan cstring result");
        }
        out.push_in(Col { entry, typ, tf, byref: (!tf.byval).then_some(tf.typlen) });
    }
    if out.len() != n {
        shape(node, "an InitPlan with fewer result columns than parameters");
    }
    out
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(init) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && !matches!(i.link, SubLink::Cte)) else {
        crate::refuse!(node, Reason::Bug { payload: "nest.initplan's end on a pipeline no InitPlan ends in" })
    };
    // D1262: PG re-runs only the nodes a rescan changed a parameter of, and the cell's run runs every member. A set wholly
    // inside the InitPlan (a recursive union's generations, a nest's inner: its owner a member too) that does not hold the
    // InitPlan's own pipeline `k` re-runs only within the cell's one run, as PG's does within ExecSetParamPlan, and never
    // re-arms the cell (exec rerun `inits_of` re-arms an InitPlan whose pipeline `k` a set holds); any
    // other set holding a part of it refuses. A member the set does not hold is kept when it lies in a CTE's plan the
    // set holds nothing of (frame/iface `in_cte_plan_apart`, setop_recurse_two :101): PG's rescan reaches no changed
    // parameter there (ExecReScanCteScan rewinds the store), and the run keeps it too: its readers pull it, and its
    // store stays for the statement
    let m = members(gx, k);
    let inside = |r: &gravity_decompose::graph::Rerun| r.pipelines.iter().all(|&p| m.contains(&(p as usize))) && !r.pipelines.contains(&(k as u16));
    let kept = |r: &gravity_decompose::graph::Rerun, q: usize| crate::frame::iface::in_cte_plan_apart(gx, q, &r.pipelines[..]);
    if gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| m.contains(&(p as usize))) && !inside(r) && !m.iter().all(|&q| r.pipelines.contains(&(q as u16)) || kept(r, q))) {
        shape(node, "an InitPlan whose members a rerun set holds only in part (D1262: PG keeps a member no changed parameter reaches)");
    }
    let (exists, n) = match init.link {
        SubLink::Exists => (true, 0),
        // one column per setParam (ExecSetParamPlan's `slot_getattr(slot, ++i)`)
        SubLink::Expr | SubLink::RowCompare | SubLink::MultiExpr => (false, init.params.len()),
        // one parameter, the array of the first column's values; no statement-wide no-widen name (item 8: D2194's root fix
        // D2325 and the D2178 / D2180 / D2197 / D2213 root fixes are on the trunk)
        SubLink::Array if init.params.len() == 1 => (false, 1),
        SubLink::Array => crate::refuse!(node, Reason::Bug { payload: "an ARRAY InitPlan with other than one parameter" }),
        SubLink::Any | SubLink::All | SubLink::Cte => crate::refuse!(node, Reason::Bug { payload: "an ANY / ALL / CTE link on a non-CTE InitPlan" }),
    };
    // pair k is setParam k: m8_init_run finds a parameter's pair as `paramid - first` (PG numbers them in order)
    if init.params.first() != Some(init.param) || init.params.iter().enumerate().any(|(i, p)| p != init.param + i as i32) {
        shape(node, "an InitPlan whose parameters are not numbered in column order");
    }
    // D2799: the Init cell holds at most contract::layout's INIT_MAX_PAIRS pairs, a target list's bound (exec
    // ops/subplan/initplan/init.rs `lay` raises XX000 at prepare past it), so no ROWCOMPARE / MULTIEXPR InitPlan PostgreSQL
    // plans reaches this guard; it stays, by name, for a declaration the cell cannot hold
    if n > gravity_abi::limits::INIT_MAX_PAIRS {
        shape(node, "an InitPlan with more parameters than the Init cell's pair cap (a target list's)");
    }
    let cols = if exists { PgVec::new_in(px.mcx()) } else { cols(px, k, n, node) };
    let array = matches!(init.link, SubLink::Array);
    let acc = array.then(|| sr::array_acc(px, k, node, cols[0].typ));
    let h = params::init_handle(px, init.param);
    let pairs = cols.len().max(1) as u64;
    // D1035 step 2 (V4R-444): parallel-safe (PG's SubPlan.parallel_safe): its one reader may run on workers, its members ahead
    // of PG's first read of the param: `Departure::Ahead`, decided by the licence alone (a VOLATILE InitPlan acts: denied,
    // lazy, one call per execution; one that may raise: granted with recheck)
    // ARRAY: its accumulator is the leader lane's, so its reader stays there too
    use gravity_decompose::effects::{licence, unobservable, Departure, Effects};
    let eager = if init.parallel_safe && !array {
        licence(px.batch_effects(Effects::of_init_plan(init)), Departure::Ahead).ok().inspect(|l| px.licensed(k, l))
    } else {
        None
    };
    // D2998: an EXISTS whose plan cannot raise (F5): a section's workers read rows past the one PostgreSQL stops at.
    // Construct C2: only a plan that neither raises nor acts (`effects::unobservable`): a VOLATILE one (which acts) is
    // read on the leader, stopping where PostgreSQL stops
    let exists_ahead = if exists && init.parallel_safe { unobservable(Effects::of_init_plan(init)) } else { None };
    let bits = if eager.is_some() { m8::INIT_EAGER } else { 0 } | if exists_ahead.is_some() { m8::INIT_EXISTS } else { 0 };
    px.declare_proto(Proto::new(ProtoKind::Init, k as u16).words(h, 1).arg(pairs | bits | (init.param as u32 as u64) << 32));
    let seen = px.frame_word(WordKey::Open { face: Face::Nest, k, j: u64::MAX, i: 0 }) as i32 * 8;
    let cell_reset = exists_ahead.is_some();
    // construct C2 (b): an eager InitPlan's members run at its reader's start, ahead of PostgreSQL's first read of the
    // param, under that licence, which was asked with the whole InitPlan's effects (`Effects::of_init_plan`): the run-ahead
    // tracer reads it on each member (`members`: `k` and what feeds it)
    if let Some(l) = eager.as_ref().or(exists_ahead.as_ref()) {
        for &q in &m {
            px.hold(q, l);
        }
    }
    mcx::pg_box_dyn!(px.mcx(), Init { node, exists, h, cols, seen, acc, cell_reset, ahead: eager.or(exists_ahead) } => dyn EndMech + 'g)
}

fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// The Init cell's address (the handle word, laid once per execution).
fn cell(b: &mut FunctionBuilder<'_>, env: &Env, h: u16) -> Value {
    let w = b.load(Type::I64, env.params, h as i32 * 8);
    b.conv(ConvOp::IntToPtr, Type::Ptr, w)
}

/// Leaves by the raise exit with the host's stashed error when `bad`.
fn raise_if(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

/// Pair `k` of cell `c` set to `(v, null)`.
fn store_pair(b: &mut FunctionBuilder<'_>, c: Value, k: usize, v: Value, null: Value) {
    let at = m8::init_pair(k) as i32;
    b.store(c, at, v);
    b.store(c, at + 8, null);
}

impl<'g> Init<'g> {
    /// Column `col`'s datum as the cell keeps it: a by-reference value copied into the cell's arena, 0 for a NULL.
    fn keep(&self, b: &mut FunctionBuilder<'_>, env: &Env, c: Value, byref: Option<i16>, d: Value, null: Value) -> Value {
        let Some(typlen) = byref else { return d };
        let (l_copy, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(null, (l_join, &[env.zero]), (l_copy, &[]));
        b.switch_to(l_copy);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
        let len = byref::image_len(b, p, typlen);
        let f = host(b, "m8_init_copy", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], gravity_abi::host_words!(m8::m8_init_copy(ctx, h, ptr, len)));
        let r = b.call_host(f, &[env.ctx, c, p, len], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        raise_if(b, env, bad);
        b.br(l_join, &[r]);
        b.switch_to(l_join);
        b.block_params(l_join)[0]
    }
}

impl<'g> EndMech for Init<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("nest.initplan rows folded", Base::Words, self.seen, Type::I64, 0));
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.acc.is_some() {
            sr::acc_call(b, env, (self.acc, self.node), AccOp::Begin, &[]);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        if self.exists {
            // ExecSetParamPlan's EXISTS: true, and no further row is read
            let c = cell(b, env, self.h);
            store_pair(b, c, 0, env.one, env.zero);
            env.store_word(b, self.seen, env.one);
            // the count-reached exit, as a LIMIT met: write-back, finish, then the done host (the call ends Done)
            let c = Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
            env.full(b, &c, row.next);
            let l_after = b.create_block(&[]);
            b.switch_to(l_after);
            return c;
        }
        // every row's columns first, then 21000 at a second row: ExecSetParamPlan projects the row before it counts it
        let mut ds = mcx::reserved_in(self.cols.len(), env.mcx);
        let mut cx = env.lx(b, self.node);
        for col in self.cols.iter() {
            let Some(cr) = row.cell(col.entry) else { crate::refuse!(self.node, Reason::Bug { payload: "an InitPlan column past the row" }) };
            ds.push_in(rep::datum(&mut cx, cr, &col.tf));
        }
        env.memo = cx.memo;
        if self.acc.is_some() {
            // ARRAY: each row's element, copied by the accumulator as it comes; no cardinality check
            let (d, null) = ds[0];
            let n = b.conv(ConvOp::Zext, Type::I64, null);
            sr::acc_call(b, env, (self.acc, self.node), AccOp::Push, &[d, n]);
            return Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
        }
        let seen = env.load_word(b, self.seen);
        let again = b.icmp(IntCond::Ne, seen, env.zero);
        let (l_again, l_first) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_again);
        b.cond_br(again, (l_again, &[]), (l_first, &[]));
        b.switch_to(l_again);
        let f = host(b, "m8_raise", &[Type::Ptr, Type::I64], m8::m8_raise as *const () as usize);
        let code = b.iconst(Type::I64, m8::RAISE_CARDINALITY as i64);
        b.call_host(f, &[env.ctx, code], Some(Type::I64));
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_first);
        env.store_word(b, self.seen, env.one);
        let c = cell(b, env, self.h);
        for (k, (col, &(d, null))) in self.cols.iter().zip(ds.iter()).enumerate() {
            let v = self.keep(b, env, c, col.byref, d, null);
            let n = b.conv(ConvOp::Zext, Type::I64, null);
            store_pair(b, c, k, v, n);
        }
        Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }

    /// No row: the parameters are NULL, an EXISTS false (ExecSetParamPlan past its loop). ARRAY: the accumulated array
    /// ('{}' without a row), never NULL, copied into the cell's arena. An `INIT_EXISTS` EXISTS: nothing, its cell holds
    /// false from the run's start (D2998: a section's worker that saw no row must not overwrite another's true).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if self.exists && self.cell_reset {
            return;
        }
        if self.acc.is_some() {
            let p = sr::acc_call(b, env, (self.acc, self.node), AccOp::End, &[]);
            let c = cell(b, env, self.h);
            let f = b.bconst(false);
            let v = self.keep(b, env, c, Some(-1), p, f);
            store_pair(b, c, 0, v, env.zero);
            return;
        }
        let seen = env.load_word(b, self.seen);
        let none = b.icmp(IntCond::Eq, seen, env.zero);
        let (l_none, l_join): (Block, Block) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(none, (l_none, &[]), (l_join, &[]));
        b.switch_to(l_none);
        let c = cell(b, env, self.h);
        let null = if self.exists { env.zero } else { env.one };
        for k in 0..self.cols.len().max(1) {
            store_pair(b, c, k, env.zero, null);
        }
        b.br(l_join, &[]);
        b.switch_to(l_join);
    }
}

/// `InitLoad`: InitPlan parameter `id`'s `[value, valid, err]` where a reader reads it. The thread's frame pair holds it
/// once SET; before that `m8_init_run` runs the InitPlan (once per execution: at most here, on this thread's first read)
/// and hands back the pair's address, whose value and null are copied into the frame pair. An error of the InitPlan's run
/// is the host's raise, returned in `err` for the reader to raise where PG raises it.
fn load<'e, 'p>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, 'p>, _node: i32, h: u16, id: i32) -> [Value; 3] {
    let (vo, fo) = (params::word(id, 0), params::word(id, 1));
    let flags = env.load_word(b, fo);
    let bit = b.iconst(Type::I64, params::SET);
    let set = b.int_bin(IntBinOp::And, flags, bit);
    let ready = b.icmp(IntCond::Ne, set, env.zero);
    let (l_run, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.mark_cold(l_run);
    b.cond_br(ready, (l_join, &[env.zero]), (l_run, &[]));
    b.switch_to(l_run);
    let hv = b.load(Type::I64, env.params, h as i32 * 8);
    let pid = b.iconst(Type::I64, id as i64);
    let f = host(b, "m8_init_run", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(m8::m8_init_run(ctx, h, paramid)));
    let p = b.call_host(f, &[env.ctx, hv, pid], Some(Type::I64)).unwrap_or(env.zero);
    let bad = b.icmp(IntCond::Slt, p, env.zero);
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(l_join, &[e]);
    b.switch_to(l_ok);
    let pp = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
    let v = b.load(Type::I64, pp, 0);
    let n = b.load(Type::I64, pp, 8);
    env.store_word(b, vo, v);
    // the cell's null word is 0 or 1: flags SET | NULL * null
    let (is_null, not_null) = (b.iconst(Type::I64, params::SET | params::NULL), b.iconst(Type::I64, params::SET));
    let null = b.icmp(IntCond::Ne, n, env.zero);
    let f2 = b.select(null, is_null, not_null);
    env.store_word(b, fo, f2);
    b.br(l_join, &[env.zero]);
    b.switch_to(l_join);
    let err = b.block_params(l_join)[0];
    let value = env.load_word(b, vo);
    let flags = env.load_word(b, fo);
    let nbit = b.iconst(Type::I64, params::NULL);
    let nb = b.int_bin(IntBinOp::And, flags, nbit);
    let valid = b.icmp(IntCond::Eq, nb, env.zero);
    [value, valid, err]
}
