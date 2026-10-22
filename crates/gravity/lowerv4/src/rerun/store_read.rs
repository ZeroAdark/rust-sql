//! The shared tuplestore reader (cards/M5.md `srf/store_read.rs`; M5-plan §3.2, M5-ENTRY-SPI §4.2 / §4.3; Decision 4): one
//! leader-only reader over a PG tuplestore and a read pointer, for form (c) in FROM (function_scan.rs), srf.pl results,
//! table functions and an AFTER trigger's named tuplestores (ENR). Each window is one host call (hosts v4/m5.rs
//! `store_batch`, H-c: select the read pointer, up to `max` gettupleslot, deform the columns the pipeline reads into the
//! reader's window, by-reference datums copied into the reader's batch region; no user code runs inside it). The window
//! is the host's, one per reader, valid until that reader's next batch or rewind, so a stopped call (a full out block, a
//! row-limited Execute) resumes inside it at store row `from` and never re-reads the store. The reader never ends a
//! store: its owner does (the scan for form (c), srf.pl and table functions; the AFTER-trigger query data for an ENR). A
//! store is PG's Tuplestore (own root context, `work_mem` spill), never a Gravity arena (P3).
//! `StoreScan` is a pipeline's source over one incumbent scan's store (hosts v4/m5.rs `ScanStore`: a named tuplestore, a
//! table function): the SetFn protocol's prepare builds it over the plan node with nothing evaluated, its first batch runs
//! the node's first fetch (a table function's fill, an ENR's lookup and read pointer), and every open rewinds it first
//! (`ExecReScan*` of a re-run scan; nothing before the fill). Registers the reader's `StoreMaker` and the named
//! tuplestore's `SourceMaker` (`Source::Named`, V4R-213). Owner M5-STORE.
use core::cell::Cell as Once;

use gravity_abi::status::raise;
use gravity_decompose::effects::Licence;
use gravity_decompose::ops::shared::srf::SrfDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::{expr, catalog::typefacts, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::srf::entries as host;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{ExprLife, ExprOut, NumTypmod, Parallel, Pipeline, Reason as Why, Rep, Until};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::{Node, NodeTag};

use crate::effects::visible::same_row::{at_pg_point, SameRow};
use crate::ops::srf::face::{RowType, StoreMaker, StoreRow, StoreSource};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::kinds::{Face, KindId};
use crate::face::source::SourceMech;
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::face::step::ExprMech;
use crate::frame::px::{ExId, Px, WordKey};
use crate::graph::Gx;
use crate::lx::CellRef;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrfFunction, &StoreMaker(make)), (KindId::SrfFunction, &SourceMaker(make_named))], ..crate::ops::NONE };

/// A window's rows: at most the cost home's `STORE_WINDOW_ROWS_MAX`, fewer for a wide row (a window stays near
/// `WINDOW_BYTES`), a multiple of 64 (the validity words are 64-row words).
const WINDOW_BYTES: u32 = 64 << 10;
/// The reader's frame words, `WordKey::Open { face: Srf, k, j, i: WORDS * member + w }`: the store's handle, its read
/// pointer, the current window's address, its rows, and the store rows before it.
const WORDS: u8 = 5;
const H: usize = 0;
const PTR: usize = 1;
const WIN: usize = 2;
const N: usize = 3;
const BASE: usize = 4;

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// D1874's name: a SubPlan the ScanStore's own executor state would compile (it holds no SubPlan state).
pub(crate) const CALL_SUBPLAN: &str = "a SubPlan in a store's call, the scan's qual or projection (the ScanStore's own executor state has none; D1874)";

/// Face `Srf`'s compile-time state: the seen bits given out so far (`reopen`).
#[derive(Default)]
pub(crate) struct SrfExt(pub(crate) u8);
crate::frame::px::ext_g!(SrfExt);

/// D1874: a FROM call's SubPlan the step evaluates on its row at the open: its plan id, its compile, cell and type facts.
pub(crate) struct Fed {
    plan_id: i32,
    ex: ExId,
    out: ExprOut,
    tf: TypeFacts,
}

/// D1874: the SubPlans of FROM entry `d`'s calls (op `j` of pipeline `k`), each compiled at the step for its value, which
/// the open passes before the rewind (`StoreSource::rewind_fed`; hosts srf/store.rs `bare` puts an exec param in its place),
/// or a refusal by D1874's name. PG evaluates a call's arguments at the FunctionScan's first fetch after a (re)scan that
/// changed a parameter they read, the step's open here, where every open recomputes the call. So a SubPlan is fed only
/// when that is unobservable: an EXISTS, scalar or ARRAY SubPlan of the nested form (an ANY / ALL / ROWCOMPARE test reads
/// the SubPlan's output params inside the call; a hashed one keeps its set), met once, nothing VOLATILE in it and no
/// VOLATILE or user call in the entry (recomputed on unchanged parameters, the call makes the same rows); a conditional
/// one only when it cannot raise (PG may skip it); one that can raise only when nothing PG evaluates ahead of it can
/// (the members before its own, the arguments left of it at every level: the same first error).
/// Construct C2: each with the licence its evaluation at the open holds: a raise-free SubPlan holds
/// `effects::quiet_ahead`'s over its effects; one that can raise runs at PostgreSQL's own point (`None`) by the same-row
/// rule (`effects::visible::same_row`): PostgreSQL's FunctionScan evaluates every call of the entry at that one first
/// fetch, so the SubPlan's rows are read whole there in either order and no row of the statement leaves between the
/// evaluations it reorders; nothing in the entry acts (`calm`); and what PostgreSQL evaluates before it raises nothing.
pub(crate) fn fed<'g, 'p>(px: &mut Px<'g, 'p>, ex: &dyn ExprMech, (k, j): (usize, usize), d: &SrfDesc<'p>, node: i32) -> PgVec<'g, (Fed, Option<Licence>)> {
    let (gx, mcx) = (px.gx, px.mcx());
    let (mut subs, mut alt): (PgVec<'_, (usize, Node<'p>)>, bool) = (PgVec::new_in(mcx), false);
    for (m, f) in d.functions.iter().enumerate() {
        expr::walk(f.node, &mut |x| {
            alt |= x.as_alternative_sub_plan().is_some();
            let s = x.as_sub_plan().is_some();
            if s {
                subs.push_in((m, x));
            }
            !s
        });
    }
    if subs.is_empty() {
        return PgVec::new_in(mcx);
    }
    let ops = &gx.pipelines()[k].ops[..j];
    let calm = d.functions.iter().all(|f| !crate::effects::volatile(gx, f.node) && !crate::effects::user_fn(gx, f.node));
    let mut out = mcx::reserved_in(subs.len(), mcx);
    for (i, &(m, x)) in subs.iter().enumerate() {
        let id = x.as_sub_plan().map_or(0, |s| s.plan_id);
        let once = !subs[..i].iter().any(|&(_, y)| y.as_sub_plan().is_some_and(|s| s.plan_id == id));
        let desc = ops.iter().rev().find_map(|o| match o { Op::SubPlan(s) if s.plan_id == id => Some(s), _ => None });
        let Some(s) = desc.filter(|_| calm && once && !alt) else { shape(node, CALL_SUBPLAN) };
        let form = matches!(s.link, SubLink::Exists | SubLink::Expr | SubLink::Array) && matches!(s.form, SubForm::Nested { .. }) && !s.alternative;
        let ahead = || d.functions[..m].iter().all(|f| crate::effects::raise_free(gx, f.node)) && gx.fx(d.functions[m].node).quiet_before(id) == Some(true);
        if !form || !crate::effects::nest_inner_calm(s.volatility) || (s.conditional && !s.raise_free) {
            shape(node, CALL_SUBPLAN);
        }
        let licence = if s.raise_free {
            let Some(l) = gravity_decompose::effects::quiet_ahead(s.effects()) else { shape(node, CALL_SUBPLAN) };
            Some(l)
        } else {
            let t = SameRow { whole: true, acts_before: !calm, raises_before: !ahead(), row_first: false };
            if !at_pg_point(&t) {
                shape(node, CALL_SUBPLAN);
            }
            None
        };
        let typ = ex.out_type(gx, x, node);
        let tf = ex.tf_of(gx, typ);
        let eo = gx.out_of(x).unwrap_or(ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid: typ, life: ExprLife::Row });
        out.push_in((Fed { plan_id: id, ex: ex.compile_at(px, (k, j), x, None, node), out: eo, tf }, licence));
    }
    out
}

/// D1874: each fed SubPlan's datum word and NULL flag on `row` (its error raises through the row's raise block), for
/// `StoreSource::rewind_fed`.
pub(crate) fn fed_values<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, ex: &dyn ExprMech, node: i32, row: &RowVals, fed: &[Fed]) -> PgVec<'e, (i32, Value, Value)> {
    let memo = env.memo.set_aside(); // evaluated here, at the open: its reads serve no read outside it
    let mut out = mcx::reserved_in(fed.len(), env.mcx);
    for (i, f) in fed.iter().enumerate() {
        let (v, valid) = ex.emit(b, env, node, row, f.ex);
        let t = b.bconst(true);
        let null = b.int_bin(IntBinOp::Xor, valid, t);
        let cell = Cell::of_expr(f.out, i as u16, i as u16, Nullable::Maybe);
        let mut cx = env.lx(b, node);
        let (d, n) = crate::rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(null) }, &f.tf);
        env.memo = cx.memo;
        let w = b.select(n, env.zero, d);
        out.push_in((f.plan_id, w, n));
    }
    env.memo.restore(memo);
    out
}

/// A column the pipeline reads: its place in the stored row (0-based), representation and type.
struct Col {
    a: u16,
    rep: Rep,
    typid: u32,
}

pub(crate) struct StoreRead<'g> {
    node: i32,
    cols: PgVec<'g, Col>,
    /// The stored row's width (`StoreRow::cols`), the deformed prefix (1 + the last column read), the columns read below 64.
    width: usize,
    natts: u32,
    need: u64,
    max: u32,
    w: [i32; WORDS as usize],
    /// `store_batch`, `store_rewind` and `store_param`'s addresses (a unit test's stand-ins).
    hosts: [usize; 3],
    /// M8 store_reopen (V4R-439): the exec params passed at every open and this store's seen bit in their flags (`reopen`);
    /// an InitPlan's with its Init's handle word (D1034).
    pars: Vec<(i32, Option<u16>)>,
    seen: i64,
    /// The current window's address (`I64`), set by `window` for the rows of that window.
    win: Once<Option<Value>>,
}

/// A window's rows for a deformed prefix of `natts` columns (9 bytes a cell: its word and a validity bit's share).
fn window_rows(natts: u32) -> u32 {
    (WINDOW_BYTES / (9 * natts.max(1))).clamp(64, gravity_program::cost::STORE_WINDOW_ROWS_MAX) & !63
}

fn make<'g>(px: &mut Px<'g, '_>, rt: RowType<'g>) -> PgBox<'g, dyn StoreSource + 'g> {
    let rd = StoreRead::new(px, &rt);
    mcx::pg_box_dyn!(px.mcx(), rd => dyn StoreSource + 'g)
}

/// A named tuplestore scan (a trigger body's `SELECT .. FROM new_table`): the relation's rows, the ENR found by name in
/// this execution's query environment (Decision 20, H-e).
fn make_named<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let Source::Named { scanrelid, relid, .. } = &p.source else { crate::refuse!(p.node_id, Reason::Bug { payload: "a named tuplestore entry over another source" }) };
    let natts = gx.facts.rel(*relid).map_or(0, |r| r.natts);
    let types = (1..=natts).map(|a| gx.facts.att(*relid, a).map_or(0, |f| f.typid)).collect_in(gx.mcx);
    scan(px, k, *scanrelid as i32, types)
}

/// M8 L0-l: what the ScanStore evaluates in its own executor state: a store's calls (op `j`'s FROM functions, or the table
/// function of the pipeline's source; a named tuplestore has none), then the scan node's own qual and projection (the
/// filters and the projection decompose places after its row), which its `ExecInit*` compiles (D1034).
pub(crate) fn calls<'g, 'p>(gx: &crate::graph::Gx<'g, 'p>, rt: &RowType) -> PgVec<'g, Node<'p>> {
    let p = &gx.pipelines()[rt.k];
    let (mut out, from): (PgVec<'g, Node<'p>>, usize) = match (p.ops.get(rt.j), &p.source) {
        (Some(Op::Srf(d)), _) => (d.functions.iter().map(|f| f.node).collect_in(gx.mcx), rt.j + 1),
        (None, Source::TableFunc { func, .. }) => {
            let cols = [&func.colexprs, &func.coldefexprs, &func.colvalexprs].into_iter().flat_map(|l| l.iter().flatten());
            (func.docexpr.into_iter().chain(func.rowexpr).chain(func.passingvalexprs.iter()).chain(func.ns_uris.iter()).chain(cols).collect_in(gx.mcx), 0)
        }
        _ => (PgVec::new_in(gx.mcx), 0),
    };
    for op in p.ops.get(from..).unwrap_or_default() {
        match op {
            Op::Filter(q) => out.push_in(*q),
            Op::Project(l) => return out.into_iter().chain(l.iter()).collect_in(gx.mcx),
            _ => {}
        }
    }
    out
}

impl<'g> StoreRead<'g> {
    fn new(px: &mut Px<'g, '_>, rt: &RowType) -> StoreRead<'g> {
        // D1874: the ScanStore's ExecInit* compiles its calls, qual and projection in its own executor state, which holds no
        // SubPlan state: a SubPlan there raises XX000 'SubPlan found with no parent plan' at the first open. A FROM function
        // store's qual and projection are the pipeline's filters and projection, which serve a SubPlan: its node is
        // initialized without them (hosts srf/store.rs `bare`), and its calls' SubPlans are the program's, their values
        // passed at every open (srf/function_store.rs `fed`, which refuses by this name the ones it cannot feed); a table
        // function's or a named tuplestore's still refuse here
        let sub = |n: Node<'_>| { let mut s = false; expr::walk(n, &mut |x| { s |= matches!(x.node_tag(), NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan); !s }); s };
        if !matches!(px.gx.pipelines()[rt.k].ops.get(rt.j), Some(Op::Srf(_))) && calls(px.gx, rt).into_iter().any(sub) {
            shape(rt.node, CALL_SUBPLAN);
        }
        // a FROM function store runs only its calls in that state: its qual and projection are the pipeline's filters and
        // projection (D1874), which read the program's words, so only the calls' parameters are the store's to pass, as
        // PG's ExecReScanFunctionScan re-calls on its funcparams alone (held-out scan-0201 / scan-0202: a hashed IN or
        // EXISTS SubPlan in the qual or target list, whose test reads the SubPlan's own output parameter)
        let store_calls = match px.gx.pipelines()[rt.k].ops.get(rt.j) {
            Some(Op::Srf(d)) => d.functions.iter().map(|f| f.node).collect_in(px.mcx()),
            _ => calls(px.gx, rt),
        };
        let (pars, seen) = crate::lifetime::store::reopened(px, rt, &store_calls);
        let mut cols = PgVec::new_in(px.mcx());
        // a named tuplestore's dropped attribute (`make_named` types it 0: no attribute facts) holds no value and no
        // reader takes it (a whole row reads every column: its record skips the dropped one, EXPR-ODDS A)
        for (a, &typid) in rt.types.iter().enumerate().filter(|&(a, &t)| t != 0 && (a >= 64 || rt.reads >> a & 1 == 1)) {
            let Some(tf) = typefacts::facts_of(px.gx.facts, typid) else { shape(rt.node, "a stored column of a type the catalog does not know") };
            cols.push_in(Col { a: a as u16, rep: Rep::of_storage(tf.typlen, tf.byval), typid });
        }
        let natts = cols.last().map_or(0, |c| c.a as u32 + 1);
        let need = cols.iter().filter(|c| c.a < 64).fold(0u64, |m, c| m | 1 << c.a);
        let max = window_rows(natts);
        let w = core::array::from_fn(|i| px.frame_word(WordKey::Open { face: Face::Srf, k: rt.k, j: crate::frame::px::key_j(rt.j), i: WORDS * rt.member + i as u8 }) as i32 * 8);
        let hosts = [host::store_batch as *const () as usize, host::store_rewind as *const () as usize, host::store_param as *const () as usize];
        // D439 (V4R-518, D1873): a re-opened store's calls run again in PG's executor state at an open after its owner
        // changed a parameter (a VOLATILE member or argument expr.volatile never sees): a pull (a cursor's FETCH n,
        // Execute's max_rows, a PL FOR loop) asks for the rows it still owes, so no owner row PG never pulls fills it
        if !pars.is_empty() {
            px.declare_proto(Proto::new(ProtoKind::Volatile, rt.k as u16));
        }
        StoreRead { node: rt.node, cols, width: rt.types.len(), natts, need, max, w, hosts, pars, seen, win: Once::new(None) }
    }

    /// M8 store_reopen: before every open's rewind, each param as its owner last wrote it, changed when the owner wrote it
    /// since this store's last open (its seen bit clear in the flags word the owner's params.rs `store` sets), then set.
    fn pass(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let Some(w) = env.words else { crate::refuse!(self.node, Reason::Bug { payload: "a re-opened store without the words site" }) };
        let f = Self::host(b, "store_param", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], self.hosts[2]);
        let (h, seen) = (env.load_word(b, self.w[H]), b.iconst(Type::I64, self.seen));
        for &(id, init) in &self.pars {
            if let Some(h) = init {
                // D1034: the Init's value into its frame pair (run here if this context has not), an error raised from the open
                let load = crate::frame::iface::init_load(&self.node).0;
                let err = load(b, env, self.node, h, id)[2];
                let raised = b.icmp(IntCond::Ne, err, env.zero);
                let l_ok = b.create_block(&[]);
                b.cond_br(raised, (env.l_raise, &[err]), (l_ok, &[]));
                b.switch_to(l_ok);
            }
            let ((v, _), flags) = (crate::rerun::params::load(b, env, self.node, id), b.load(Type::I64, w, crate::rerun::params::word(id, 1)));
            let (mine, marked) = (b.int_bin(IntBinOp::And, flags, seen), b.int_bin(IntBinOp::Or, flags, seen));
            let fresh = b.icmp(IntCond::Eq, mine, env.zero);
            let changed = b.select(fresh, env.one, env.zero);
            b.store(w, crate::rerun::params::word(id, 1), marked);
            let idv = b.iconst(Type::I64, id as i64);
            let r = b.call_host(f, &[env.ctx, h, idv, v, flags, changed], Some(Type::I64)).expect("store_param returns its status");
            let failed = b.icmp(IntCond::Ne, r, env.zero);
            self.raise_if(b, env, failed);
        }
    }

    fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
        b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
    }

    /// `failed` leaves with the host's stashed error.
    fn raise_if(&self, b: &mut FunctionBuilder<'_>, env: &Env, failed: Value) {
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(failed, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
    }

    /// D1874: each value of the FROM call's SubPlans (`fed`: plan id, datum word, NULL flag) as the exec param standing in for
    /// that SubPlan in the store's own state (hosts srf/store.rs `bare`), changed at every open: the call is recomputed at
    /// the rewind, as PG's is when a parameter its SubPlan reads changes; srf/function_store.rs `fed` admits only calls and
    /// SubPlans that make the same rows when recomputed on unchanged parameters.
    fn feed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, fed: &[(i32, Value, Value)]) {
        let f = Self::host(b, "store_param", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], self.hosts[2]);
        let h = env.load_word(b, self.w[H]);
        let [set, set_null] = [1, 3].map(|v| b.iconst(Type::I64, v));
        for &(plan_id, v, null) in fed {
            let id = b.iconst(Type::I64, host::STORE_FEED as i64 + plan_id as i64);
            let flags = b.select(null, set_null, set);
            let r = b.call_host(f, &[env.ctx, h, id, v, flags, env.one], Some(Type::I64)).expect("store_param returns its status");
            let failed = b.icmp(IntCond::Ne, r, env.zero);
            self.raise_if(b, env, failed);
        }
    }

    /// The window words cleared: the next `window` reads the next batch as store row 0 on.
    fn clear(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        for i in [WIN, N, BASE] {
            env.store_word(b, self.w[i], env.zero);
        }
    }
}

impl StoreSource for StoreRead<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        for (i, what) in ["store handle", "store read pointer", "store window", "store window rows", "store window base"].into_iter().enumerate() {
            w.declare(OpenWord::open(what, Base::Words, self.w[i], Type::I64, 0));
        }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, h: Value, ptr: Value) {
        env.store_word(b, self.w[H], h);
        env.store_word(b, self.w[PTR], ptr);
        self.clear(b, env);
    }

    /// Store row `from` inside the current window (a resume): the rest of it; else the next batch, the rows after it.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block, from: Value) -> Win {
        let (base, n) = (env.load_word(b, self.w[BASE]), env.load_word(b, self.w[N]));
        let lim = b.int_bin(IntBinOp::Add, base, n);
        let inside = b.icmp(IntCond::Slt, from, lim);
        let (l_rest, l_batch, l_win) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64, Type::I64]));
        b.cond_br(inside, (l_rest, &[]), (l_batch, &[]));
        b.switch_to(l_rest);
        let (w, at) = (env.load_word(b, self.w[WIN]), b.int_bin(IntBinOp::Sub, from, base));
        b.br(l_win, &[w, at, n]);
        b.switch_to(l_batch);
        env.store_word(b, self.w[BASE], lim);
        env.store_word(b, self.w[N], env.zero);
        let f = Self::host(b, "store_batch", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], self.hosts[0]);
        let (h, ptr) = (env.load_word(b, self.w[H]), env.load_word(b, self.w[PTR]));
        let [max, natts, need] = [self.max as i64, self.natts as i64, self.need as i64].map(|v| b.iconst(Type::I64, v));
        let w = b.call_host(f, &[env.ctx, h, ptr, max, natts, need], Some(Type::I64)).expect("store_batch returns the window");
        let [end, failed] = [host::STORE_END, host::STORE_FAILED].map(|c| { let c = b.iconst(Type::I64, c as i64); b.icmp(IntCond::Eq, w, c) });
        self.raise_if(b, env, failed);
        let l_rows = b.create_block(&[]);
        b.cond_br(end, (l_eof, &[env.zero]), (l_rows, &[]));
        b.switch_to(l_rows);
        let rows = { let p = b.conv(ConvOp::IntToPtr, Type::Ptr, w); b.load(Type::I64, p, host::WIN_N as i32) };
        env.store_word(b, self.w[WIN], w);
        env.store_word(b, self.w[N], rows);
        b.br(l_win, &[w, env.zero, rows]);
        b.switch_to(l_win);
        let [w, cursor, end] = [0, 1, 2].map(|i| b.block_params(l_win)[i]);
        self.win.set(Some(w));
        Win { cursor, end, facts: Default::default() }
    }

    /// Row `j`: each read column's datum word and its NULL flag (its validity bit clear). A by-reference value lives in the
    /// reader's batch region until its next batch: a window's life.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value) -> StoreRow<'e> {
        let Some(w) = self.win.get() else { crate::refuse!(self.node, Reason::Bug { payload: "a stored row before its window" }) };
        let (wp, max) = (b.conv(ConvOp::IntToPtr, Type::Ptr, w), self.max as usize);
        let [six, low] = [6, 63].map(|v| b.iconst(Type::I64, v));
        let (word, bit) = (b.int_bin(IntBinOp::Ushr, j, six), b.int_bin(IntBinOp::And, j, low));
        let mut out = StoreRow { row: Row { cells: mcx::reserved_in(self.cols.len(), env.mcx) }, vals: mcx::reserved_in(self.cols.len(), env.mcx), cols: mcx::pg_vec![in env.mcx; None; self.width] };
        for (n, c) in self.cols.iter().enumerate() {
            let at = b.ptr_add(wp, j, 8, (host::WIN_VALUES + c.a as usize * max * 8) as i32);
            let v = b.load(Type::I64, at, 0);
            let vw = { let p = b.ptr_add(wp, word, 8, (host::win_valid(self.natts as usize, max) + c.a as usize * (max / 64) * 8) as i32); b.load(Type::I64, p, 0) };
            let set = { let s = b.int_bin(IntBinOp::Ushr, vw, bit); b.int_bin(IntBinOp::And, s, env.one) };
            let null = b.icmp(IntCond::Eq, set, env.zero);
            let life = CellLife { until: Until::Window, home: CellHome::FrameSlot };
            out.row.cells.push_in(Cell { rep: c.rep, typid: c.typid, num: NumTypmod::NONE, life, at: At::Slot(n as u16), null: Nullable::Maybe });
            out.vals.push_in((v, Some(null)));
            out.cols[c.a as usize] = Some(n as u16);
        }
        out
    }

    fn rewind(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.rewind_fed(b, env, &[]);
    }

    fn rewind_fed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, fed: &[(i32, Value, Value)]) {
        if !self.pars.is_empty() {
            self.pass(b, env);
        }
        if !fed.is_empty() {
            self.feed(b, env, fed);
        }
        let f = Self::host(b, "store_rewind", &[Type::Ptr, Type::I64, Type::I64], self.hosts[1]);
        let (h, ptr) = (env.load_word(b, self.w[H]), env.load_word(b, self.w[PTR]));
        let r = b.call_host(f, &[env.ctx, h, ptr], Some(Type::I64)).expect("store_rewind returns its status");
        let failed = b.icmp(IntCond::Ne, r, env.zero);
        self.raise_if(b, env, failed);
        self.clear(b, env);
    }
}

/// A pipeline's source over one incumbent scan's store: the reader, the SetFn protocol's words (its store at
/// `SETFN_W_NODE`), the store rows handed on (`from`, written back at every window's end and stop, so a resumed call
/// re-enters its window) and the current window's first store row (`WordKey::Open { face: Srf, k, j: u64::MAX, i:
/// WORDS + 0 / 1 }`, past the reader's).
pub(crate) struct StoreScan<'g> {
    rd: PgBox<'g, dyn StoreSource + 'g>,
    proto: u16,
    from: i32,
    base: i32,
}

/// Pipeline `k`'s scan of range entry `varno` whose store holds rows of `types`; the reader deforms the columns the scan's
/// own ops read.
pub(crate) fn scan<'g>(px: &mut Px<'g, '_>, k: usize, varno: i32, types: PgVec<'g, u32>) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let rt = RowType { k, j: usize::MAX, member: 0, node, reads: reads_of(&p.ops, varno, types.len()), types };
    let rd = make(px, rt);
    let proto = px.proto_words(host::SETFN_WORDS);
    px.declare_proto(Proto::new(ProtoKind::SetFn, k as u16).words(proto, host::SETFN_WORDS).arg(host::setfn_arg(0, 0, Some(node as u32))));
    let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Srf, k, j: u64::MAX, i: WORDS + i }) as i32 * 8;
    mcx::pg_box_dyn!(px.mcx(), StoreScan { rd, proto, from: word(0), base: word(1) } => dyn SourceMech + 'g)
}

impl SourceMech for StoreScan<'_> {
    /// The leader alone (the store and the node's fill are the leader's), whatever the end.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match d.parallel {
            Parallel::Eligible => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Host }, ..d },
            _ => d,
        }
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.rd.open_words(w);
        w.declare(OpenWord::open("store rows handed on", Base::Words, self.from, Type::I64, 0));
        w.declare(OpenWord::open("store window first row", Base::Words, self.base, Type::I64, 0));
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let h = b.load(Type::I64, env.params, 8 * (self.proto as usize + host::SETFN_W_NODE) as i32);
        self.rd.open(b, env, h, env.zero);
        self.rd.rewind(b, env);
        env.zero
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let from = env.load_word(b, self.from);
        let win = self.rd.window(b, env, l_eof, from);
        let base = b.int_bin(IntBinOp::Sub, from, win.cursor);
        env.store_word(b, self.base, base);
        win
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let r = self.rd.row(b, env, win, j);
        RowVals { row: r.row, vals: r.vals, cols: r.cols, carry, next, at: j }
    }

    /// The window's end or a stop at its row `cur`: the rows handed on.
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        let base = env.load_word(b, self.base);
        let from = b.int_bin(IntBinOp::Add, base, cur);
        env.store_word(b, self.from, from);
    }

    fn fills(&self) -> bool {
        false
    }
}

/// The columns (bit `a - 1`) of range entry `varno` a scan's own ops read: its quals and its projection (the ops after the
/// projection read its output). Every column when an op before it is of another kind or a whole-row reference reads the row.
pub(crate) fn reads_of(ops: &[Op<'_>], varno: i32, width: usize) -> u64 {
    let all = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
    let mut m = 0u64;
    let mut var = |n: Node<'_>| {
        expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0) {
                m |= if (1..=64).contains(&v.varattno) { 1 << (v.varattno - 1) } else { all };
            }
            true
        })
    };
    for op in ops {
        match op {
            Op::Filter(n) | Op::Gate(n) => _ = var(*n),
            Op::Project(l) => {
                l.iter().for_each(|n| _ = var(n));
                return m;
            }
            _ => return all,
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::stop::tests::run1;
    use std::sync::{Mutex, MutexGuard};

    /// A stand-in store (`rows[r][c]`, `None` NULL): each read pointer's position, the reader's current window (kept until
    /// its next batch, as the host's), the batches served and the last `need` asked for.
    struct Fake {
        rows: Vec<Vec<Option<u64>>>,
        pos: [usize; 2],
        win: Vec<u64>,
        batches: usize,
        need: u64,
    }

    /// Statics, not TLS (push-check's TLS census counts every thread_local!); RUN serializes the tests that use FAKE.
    static FAKE: Mutex<Fake> = Mutex::new(Fake { rows: Vec::new(), pos: [0; 2], win: Vec::new(), batches: 0, need: 0 });
    static RUN: Mutex<()> = Mutex::new(());

    fn held<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `store_batch` over `FAKE` in the window layout `row` reads; only the needed columns are written.
    extern "C" fn batch(_ctx: u64, _h: u64, ptr: u64, max: u64, natts: u64, need: u64) -> u64 {
        {
            let f = &mut *held(&FAKE);
            let (max, natts, at) = (max as usize, natts as usize, f.pos[ptr as usize]);
            let n = (f.rows.len() - at).min(max);
            if n == 0 {
                return host::STORE_END;
            }
            let valid = 1 + natts * max;
            let mut win = vec![0u64; valid + natts * max / 64];
            win[0] = n as u64;
            for (j, row) in f.rows[at..at + n].iter().enumerate() {
                for (c, v) in row.iter().enumerate().take(natts).filter(|&(c, _)| need >> c & 1 == 1) {
                    if let Some(v) = v {
                        win[1 + c * max + j] = *v;
                        win[valid + c * max / 64 + j / 64] |= 1 << (j % 64);
                    }
                }
            }
            (f.pos[ptr as usize], f.batches, f.need, f.win) = (at + n, f.batches + 1, need, win);
            f.win.as_ptr() as u64
        }
    }

    extern "C" fn rewind(_ctx: u64, _h: u64, ptr: u64) -> u64 {
        held(&FAKE).pos[ptr as usize] = 0;
        0
    }

    /// A reader of `width` int8 columns reading `reads`, its words at `[0, 40)` of the caller's words.
    fn reader(width: usize, reads: &[u16]) -> StoreRead<'static> {
        let m = Box::leak(Box::new(mcx::MemoryContext::new("store reader"))).mcx(); // alloc: Cold
        let cols = reads.iter().map(|&a| Col { a, rep: Rep::Word { bytes: 8 }, typid: 20 }).collect_in(m);
        let natts = reads.iter().map(|&a| a as u32 + 1).max().unwrap_or(0);
        let need = reads.iter().fold(0u64, |m, &a| m | 1 << a);
        let hosts = [batch as *const () as usize, rewind as *const () as usize, 0];
        StoreRead { node: 0, cols, width, natts, need, max: window_rows(natts), w: [0, 8, 16, 24, 32], hosts, pars: Vec::new(), seen: 0, win: Once::new(None) }
    }

    /// A caller's loop over `r` (the reader's words in `words`): opened first when `open` (store 7, pointer `ptr`), from store
    /// row `from` until row `stop` (a stop leaves the words as a suspended call does), rewound first when `again`. Folds each
    /// row as `acc * 31 + 1`, then each read cell as `acc * 31 + value` (a NULL counts 1000).
    fn drive(r: &StoreRead, words: &mut [u64; 5], (open, again): (bool, bool), from: i64, stop: i64) -> i64 {
        let at = words.as_mut_ptr() as i64;
        run1(&[], &mut |b, env| {
            let wa = b.iconst(Type::I64, at);
            env.words = Some(b.conv(ConvOp::IntToPtr, Type::Ptr, wa));
            if open {
                let (h, p) = (b.iconst(Type::I64, 7), b.iconst(Type::I64, 0));
                r.open(b, env, h, p);
            }
            if again {
                r.rewind(b, env);
            }
            let [l_head, l_row] = [2, 3].map(|n| b.create_block(&vec![Type::I64; n]));
            let (l_go, l_body, l_eof, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
            let (from, stop) = (b.iconst(Type::I64, from), b.iconst(Type::I64, stop));
            b.br(l_head, &[from, env.zero]);
            b.switch_to(l_head);
            b.mark_loop_header(l_head);
            let [from, acc_h] = [0, 1].map(|i| b.block_params(l_head)[i]);
            let more = b.icmp(IntCond::Slt, from, stop);
            b.cond_br(more, (l_go, &[]), (l_out, &[acc_h]));
            b.switch_to(l_go);
            let win = r.window(b, env, l_eof, from);
            b.br(l_row, &[win.cursor, from, acc_h]);
            b.switch_to(l_row);
            b.mark_loop_header(l_row);
            let [j, from, acc] = [0, 1, 2].map(|i| b.block_params(l_row)[i]);
            let (inside, before) = (b.icmp(IntCond::Slt, j, win.end), b.icmp(IntCond::Slt, from, stop));
            let go = b.int_bin(IntBinOp::And, inside, before);
            b.cond_br(go, (l_body, &[]), (l_head, &[from, acc]));
            b.switch_to(l_body);
            let row = r.row(b, env, &win, j);
            let (k31, k1000) = (b.iconst(Type::I64, 31), b.iconst(Type::I64, 1000));
            let acc = { let m = b.int_bin(IntBinOp::Mul, acc, k31); b.int_bin(IntBinOp::Add, m, env.one) };
            let acc = row.vals.iter().fold(acc, |acc, &(v, null)| {
                let x = b.select(null.expect("a nullable cell"), k1000, v);
                let m = b.int_bin(IntBinOp::Mul, acc, k31);
                b.int_bin(IntBinOp::Add, m, x)
            });
            let (j1, from1) = (b.int_bin(IntBinOp::Add, j, env.one), b.int_bin(IntBinOp::Add, from, env.one));
            b.br(l_row, &[j1, from1, acc]);
            b.switch_to(l_eof);
            b.br(l_out, &[acc_h]);
            b.switch_to(l_out);
            b.block_params(l_out)[0]
        })
    }

    /// The same fold over rows `[lo, hi)` of the store.
    fn fold(rows: &[Vec<Option<u64>>], reads: &[u16], lo: usize, hi: usize) -> i64 {
        let cell = |acc: i64, x: i64| acc.wrapping_mul(31).wrapping_add(x);
        rows[lo..hi.min(rows.len())].iter().fold(0i64, |acc, r| reads.iter().fold(cell(acc, 1), |acc, &a| cell(acc, r[a as usize].map_or(1000, |v| v as i64))))
    }

    fn store(n: usize) -> Vec<Vec<Option<u64>>> {
        (0..n).map(|i| vec![Some(i as u64), (i % 7 != 3).then_some(i as u64 * 3), (i % 5 != 0).then_some(i as u64 ^ 0x55)]).collect()
    }

    fn fresh(rows: Vec<Vec<Option<u64>>>) {
        *held(&FAKE) = Fake { rows, pos: [0; 2], win: Vec::new(), batches: 0, need: 0 };
    }

    fn batches() -> usize {
        held(&FAKE).batches
    }

    /// Every row once, in store order, one host call per window (1,000 rows: 4 windows of 256), NULLs from the validity
    /// words; the unread column is never asked for (`need`) and has no cell.
    #[test]
    fn reads_every_row_one_batch_per_window() {
        let _run = held(&RUN);
        let (rows, reads) = (store(1000), [0u16, 2]);
        fresh(rows.clone());
        let r = reader(3, &reads);
        let mut words = [0u64; 5];
        assert_eq!(drive(&r, &mut words, (true, false), 0, i64::MAX), fold(&rows, &reads, 0, 1000));
        assert_eq!((batches(), held(&FAKE).need), (4, 0b101));
    }

    /// A stop after any row (inside a window, at its last row, at a window's edge) resumes at the next row of the same
    /// window without re-reading the store: the two calls see every row once, and the batches are still one per window.
    #[test]
    fn a_stop_resumes_inside_its_window() {
        let _run = held(&RUN);
        let (rows, reads) = (store(600), [0u16, 1, 2]);
        for k in [1, 63, 64, 255, 256, 257, 511, 599] {
            fresh(rows.clone());
            let r = reader(3, &reads);
            let mut words = [0u64; 5];
            let first = drive(&r, &mut words, (true, false), 0, k);
            let rest = drive(&r, &mut words, (false, false), k, i64::MAX);
            assert_eq!((first, rest), (fold(&rows, &reads, 0, k as usize), fold(&rows, &reads, k as usize, 600)), "stop at {k}");
            assert_eq!(batches(), 3, "stop at {k}: a resume re-read the store");
        }
    }

    /// `rewind` is PG's `tuplestore_rescan`: the next window starts at the store's first row again; an empty store ends at once.
    #[test]
    fn rewind_rereads_from_the_first_row_and_an_empty_store_ends() {
        let _run = held(&RUN);
        let (rows, reads) = (store(300), [1u16]);
        fresh(rows.clone());
        let r = reader(2, &reads);
        let mut words = [0u64; 5];
        let a = drive(&r, &mut words, (true, false), 0, 100);
        let b = drive(&r, &mut words, (false, true), 0, i64::MAX);
        assert_eq!((a, b, batches()), (fold(&rows, &reads, 0, 100), fold(&rows, &reads, 0, 300), 3));
        fresh(Vec::new());
        assert_eq!((drive(&r, &mut [0u64; 5], (true, false), 0, i64::MAX), batches()), (0, 0));
    }

    /// Reading no column (`count(*)`) deforms nothing; wide rows take smaller windows, a multiple of 64 rows.
    #[test]
    fn window_rows_follow_the_width() {
        let _run = held(&RUN);
        assert_eq!([0, 1, 20, 28, 29, 100, 1600].map(window_rows), [256, 256, 256, 256, 192, 64, 64]);
        let rows = store(70);
        fresh(rows.clone());
        let r = reader(3, &[]);
        assert_eq!((drive(&r, &mut [0u64; 5], (true, false), 0, i64::MAX), batches(), r.natts), (fold(&rows, &[], 0, 70), 1, 0));
    }
    /// `store_param`'s stand-in: each pass (id, value, flags, changed).
    static PASSED: Mutex<Vec<[u64; 4]>> = Mutex::new(Vec::new());

    extern "C" fn param(_ctx: u64, _h: u64, id: u64, value: u64, flags: u64, changed: u64) -> u64 {
        held(&PASSED).push([id, value, flags & 3, changed]);
        0
    }

    /// M8 store_reopen: every open passes each param before its rewind, `changed` only when its owner wrote it since this
    /// store's last open (PG's chgParam: a repeated value an owner wrote again is changed); another store's seen bit is
    /// its own. Params 0 and 1 in words 0..4, the reader's words from 32.
    #[test]
    fn a_reopened_store_passes_its_params_changed_only_after_their_owners_write() {
        let _run = held(&RUN);
        fresh(store(3));
        let mut r = reader(2, &[1]);
        (r.w, r.hosts[2], r.pars, r.seen) = ([32, 40, 48, 56, 64], param as *const () as usize, vec![(0, None), (1, None)], 1 << 2);
        let mut other = reader(2, &[1]);
        (other.w, other.hosts[2], other.pars, other.seen) = ([72, 80, 88, 96, 104], param as *const () as usize, vec![(0, None)], 1 << 3);
        let mut words = [0u64; 14];
        let open = |r: &StoreRead, words: &mut [u64; 14]| {
            held(&PASSED).clear();
            let at = words.as_mut_ptr() as i64;
            run1(&[], &mut |b, env| {
                let wa = b.iconst(Type::I64, at);
                env.words = Some(b.conv(ConvOp::IntToPtr, Type::Ptr, wa));
                let (h, p) = (b.iconst(Type::I64, 7), b.iconst(Type::I64, 0));
                r.open(b, env, h, p);
                r.rewind(b, env);
                env.zero
            });
            held(&PASSED).clone()
        };
        let (set, null) = (crate::rerun::params::SET as u64, crate::rerun::params::NULL as u64);
        (words[0], words[1], words[3]) = (5, set, set | null);
        assert_eq!(open(&r, &mut words), [[0, 5, set, 1], [1, 0, set | null, 1]], "the owner's first write");
        assert_eq!(open(&r, &mut words), [[0, 5, set, 0], [1, 0, set | null, 0]], "no write since: the store is reread");
        assert_eq!(open(&other, &mut words), [[0, 5, set, 1]], "another store's first open");
        (words[0], words[1]) = (5, set);
        assert_eq!(open(&r, &mut words), [[0, 5, set, 1], [1, 0, set | null, 0]], "the same value written again");
        (words[0], words[1]) = (6, set);
        assert_eq!(open(&r, &mut words), [[0, 6, set, 1], [1, 0, set | null, 0]]);
        assert_eq!(open(&other, &mut words), [[0, 6, set, 1]]);
    }
}
