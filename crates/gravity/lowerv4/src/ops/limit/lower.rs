//! LIMIT / OFFSET counters in per-open words, kind `step.limit` (KWC lowerv3 scan/limit.rs): OFFSET as a skip counter, the
//! count checked after each emit, both frame words (D229) the frame writes at open, once per union above an arm marker; a
//! window past the count is done at once. A bound is a constant, a bind-time word (`LIMIT $1`, `WordPlan::key`) or one over
//! executor parameters read once per open (D1016 row 5, params_bound.rs); as PG's `recompute_limits`: a NULL OFFSET is 0,
//! a NULL LIMIT none, a negative one raises (OFFSET checked first). WITH TIES (an ordered input): past the count, rows whose
//! tie keys equal the last emitted row's go on (two NULLs equal; under a nondeterministic collation
//! equal by key.coll's sort-key image), and the first row that differs ends the step. A row is
//! counted once it is through the steps below: after `next`, or at a stop inside `next` that leaves past the row (D357,
//! frame.carry's `through`). Budget 250 lines. Owner M1-FRAME.
use std::cell::Cell;

use gravity_decompose::graph::Bound;
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::Node;

use crate::face::kinds::{Face, KindId};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords, Scope};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::lifetime::bound::{self, Feed};
use crate::ops::shared::limit::{Above, Lim};
use crate::rep;
use crate::table::keyimg::{ImageForm, VarKeyImpl};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::StepLimit, &StepMaker(make))], ..crate::ops::NONE };

/// A WITH TIES key: the input column and the frame words of the last emitted row's image (two words) and null flag.
struct Tie {
    resno: i16,
    /// The key type's catalog facts (V4R-84): an enum ties by its oid word.
    tf: TypeFacts,
    /// The tie collation: a nondeterministic one ties by key.coll's sort-key image (DISTINCT ON's rule, per_group.rs).
    coll: KeyColl,
    img: [i32; 2],
    null: i32,
}

pub(crate) struct Limit<'g> {
    count: Lim,
    offset: Lim,
    /// D1634: `(count, offset)` computed bounds kept in the top's words below (`top_below`): the top's open resets them.
    shared: [bool; 2],
    skipped: i32,
    emitted: i32,
    /// WITH TIES: set once a row past the count differs from the last emitted one.
    tied: i32,
    ties: PgVec<'g, Tie>,
    scope: Scope,
    node: i32,
    /// D1759: a nest or SubPlan inner's bound PG checks at each re-open (a negative constant, a bind-time word), read past
    /// the open (`late`) as a computed one is: the prologue runs once per call of the owner, before any re-open.
    reopen: bool,
    /// `(count, offset)` as evaluated at the call's prologue; `count` `None` = no limit (`LIMIT ALL` / NULL).
    bounds: Cell<Option<(Option<Value>, Option<Value>)>>,
    /// D1755: the checks of the nodes above the Limit in its own pipeline (`Above`), made before a bound
    /// that may raise is read.
    above: Above<'g>,
    mcx: Mcx<'g>,
}

/// wpart-0002 (window): `op` is a Limit of a constant count of one or more under no offset or a constant one, no WITH TIES
/// (decompose window.rs `cap_arms` puts one in a settled union's arm under a window that reads only its first rows): a
/// bound with no expression to evaluate, which rerun/union_arms.rs `quiet_arm_in` reads as no call and no raise, and
/// whose count words `cap_union` makes the union's (wlim-w02).
pub(crate) fn const_bound(op: &Op<'_>) -> bool {
    matches!(op, Op::Limit { count: Bound::Const(c), offset: Bound::All | Bound::Const(0..), ties: None } if *c >= 1)
}

fn lim<'p>(px: &mut Px<'_, 'p>, k: usize, b: &Bound<'p>, node: i32, words: [i32; 2], rebinds: bool, top: Option<(usize, u8)>) -> Lim {
    // D1634: a bound the top below computes is read from the top's words, evaluated once per run
    let (words, at) = match top {
        Some((p, which)) => (crate::rerun::params_bound::top_words(px, p, which), bound::open_words(px.gx, p, k)),
        None => (words, bound::open_words(px.gx, k, k)),
    };
    let open = |px: &mut Px<'_, 'p>, e: Node<'p>| {
        bound::declare(px, Feed::Limit, k, e, node, at);
        Lim::Open(crate::rerun::params_bound::admit(px, e, node, Some(words)))
    };
    match *b {
        Bound::All => Lim::All,
        Bound::Const(n) => Lim::Const(n),
        Bound::Expr(e) if rebinds => open(px, e), // D1634
        Bound::Expr(e) => match bound::word(px, Feed::Limit, k, e, node) {
            Place::Word(w) => Lim::Word(w),
            _ if gravity_lower::rerun::words::has_exec(e) => open(px, e),
            _ => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a LIMIT / OFFSET over a value the binder cannot place") }),
        },
    }
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Limit { count, offset, ties } = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a limit entry over another op" }) };
    // wlim-w02 (window): a settled union feed's row cap counts across the union's feeds (rerun/union_arms.rs `cap_union`)
    let union = px.union_of(k, j).or_else(|| crate::rerun::union_arms::cap_union(px.gx, k, j));
    // D2241: a tie key the input's projection holds as a constant has one value on every row, so every row ties on it (two
    // NULLs equal): the step compares only the other keys and takes no words for it; the first key stays, so a list of
    // constants still ties every row (decompose's `leads_with` passes such a key the same way, from the same projection)
    let proj = p.ops[..j].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
    let constant = |resno: i16| proj.and_then(|l| l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno)).is_some_and(|te| te.expr.as_const().is_some());
    let keys: PgVec<'_, (i16, TypeFacts, KeyColl)> = ties.iter().flat_map(|t| t.keys.iter().enumerate().filter(|&(n, k)| n == 0 || !constant(k.resno)).map(|(n, k)| (k.resno, crate::step::expr::tf_of(px.gx, k.typid), px.gx.facts.coll(t.colls.get(n).copied().unwrap_or(0))))).collect_in(mcx);
    // D1634: read per open, as PG's recompute_limits at each rescan
    let (rc, ro) = (bound::rebinds(px.gx, k, count), bound::rebinds(px.gx, k, offset));
    let (oc, oo) = (rc || crate::rerun::params_bound::computed(count), ro || crate::rerun::params_bound::computed(offset));
    // word `i` of chunk `c`: chunk 0 is the step's own `u8` indices; CAP-30: a WITH TIES list past them goes on in further
    // chunks keyed past the op's position (bit 56 up, clear of a remade pipeline's bits and text_like's expression bits)
    let mut word = |c: usize, i: u8| {
        let key = match union {
            Some((union, marker)) => WordKey::Union { union, after: crate::frame::px::key_j(j - marker) ^ ((c as u64) << 56), i },
            None => WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j) ^ ((c as u64) << 56), i },
        };
        px.frame_word(key) as i32 * 8
    };
    let (skipped, emitted) = (word(0, 0), word(0, 1));
    let tied = if keys.is_empty() { -1 } else { word(0, 2) };
    let (w, t) = if oc || oo { ([word(0, 3), word(0, 4), word(0, 5), word(0, 6)], crate::rerun::params_bound::TIE_BASE) } else { ([0; 4], 3) };
    // D2241 / CAP-30: each key's three words, from `t` while chunk 0 has room, then 85 keys a chunk (a wrapped index would
    // hand one word two roles, D482)
    let room = (256 - usize::from(t)) / 3;
    let at = |n: usize| if n < room { (0, (usize::from(t) + 3 * n) as u8) } else { (1 + (n - room) / 85, (3 * ((n - room) % 85)) as u8) };
    let ties: PgVec<'_, Tie> = keys.iter().enumerate().map(|(n, &(resno, tf, coll))| {
        let (c, i) = at(n);
        Tie { resno, tf, coll, img: [word(c, i), word(c, i + 1)], null: word(c, i + 2) }
    }).collect_in(mcx);
    let checked = |b: &Bound<'_>| !matches!(*b, Bound::All | Bound::Const(0..)) && !crate::rerun::params_bound::computed(b);
    let reopen = k >= px.gx.pipelines().len() && (checked(count) || checked(offset));
    // D1755: only a bound read at the prologue that may raise waits on the checks above (a constant 0 or more reads nothing
    // that shows; a bound read past the open, `late`, is read after them)
    let above = if !reopen && (checked(count) || checked(offset)) { Above::of(px, k, j) } else { Above::none(mcx) };
    let tops = [0u8, 1].map(|i| crate::rerun::params_bound::top_below(px.gx, k, [count, offset][i as usize], i).map(|p| (p, i)));
    let (count, offset) = (lim(px, k, count, node, [w[0], w[1]], rc, tops[0]), lim(px, k, offset, node, [w[2], w[3]], ro, tops[1]));
    // CTELIMIT (ops/shared/limit.rs `cte_lock_cap`): a CTE's Limit over its row lock counts no further than the rows the
    // statement's INSERT reads of the CTE under its own LIMIT, as PostgreSQL's CteScan pulls the LockRows no further
    let count = match (count, crate::ops::shared::limit::cte_lock_cap(px.gx, k)) {
        (Lim::Const(c), Some((at, n))) if at == j => Lim::Const(c.min(n)),
        (c, _) => c,
    };
    let shared = tops.map(|t| t.is_some());
    // the counters' open scope, the lifetime home's: once per run of the union past whose arm marker they count (D2121)
    let scope = match union {
        Some((union, _)) => crate::lifetime::OpenAt::Union { flag: px.frame_word(WordKey::UnionFlag { union }) },
        None => crate::lifetime::OpenAt::Open,
    };
    mcx::pg_box_dyn!(mcx, Limit { count, offset, shared, skipped, emitted, tied, ties, scope, node, reopen, bounds: Cell::new(None), above, mcx } => dyn StepMech + 'g)
}

impl<'g> Limit<'g> {
    fn words(&self, env: &Env) -> Value {
        match env.words {
            Some(w) => w,
            None => crate::refuse!(self.node, Reason::Bug { payload: "a limit without the frame words" }),
        }
    }

    /// A bound's value this call, `None` when it is ALL or NULL; a negative one raises `which` (0 OFFSET, 1 LIMIT).
    pub(crate) fn bound(b: &mut FunctionBuilder<'_>, env: &mut Env, l: Lim, which: u64) -> Option<Value> {
        let (v, null) = match l {
            Lim::All => return None,
            Lim::Open(c) => c.read(b, env),
            Lim::Const(n) => (b.iconst(Type::I64, n), None),
            Lim::Word(w) => {
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                (b.load(Type::I64, env.params, w as i32 * 8), Some(b.icmp(IntCond::Ne, nb, env.zero)))
            }
        };
        if matches!(l, Lim::Const(n) if n >= 0) {
            return Some(v);
        }
        let neg = b.icmp(IntCond::Slt, v, env.zero);
        let bad = match null {
            Some(n) => {
                let t = b.bconst(true);
                let nn = b.int_bin(IntBinOp::Xor, n, t);
                b.int_bin(IntBinOp::And, neg, nn)
            }
            None => neg,
        };
        let (l_raise, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(bad, (l_raise, &[]), (l_ok, &[]));
        b.switch_to(l_raise);
        let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("limit_negative", &[Type::I64], Some(Type::I64), gravity_kernels::ops::limit::limit_negative as *const () as usize, mem);
        let w = b.iconst(Type::I64, which as i64);
        let mask = b.call_host(h, &[w], Some(Type::I64)).unwrap_or(env.zero);
        b.br(env.l_raise, &[mask]);
        b.switch_to(l_ok);
        // a NULL OFFSET is 0; a NULL LIMIT is no limit (the count test below reads the null through `select`)
        match null {
            Some(n) if which == 0 => Some(b.select(n, env.zero, v)),
            Some(n) => {
                let none = b.iconst(Type::I64, i64::MAX);
                Some(b.select(n, none, v))
            }
            None => Some(v),
        }
    }

    /// Whether the bounds are read past the open (`late`), not at the prologue: a computed one (D1016 row 5) or `reopen`.
    fn is_late(&self) -> bool {
        self.reopen || matches!((self.count, self.offset), (Lim::Open(_), _) | (_, Lim::Open(_)))
    }

    /// D1016 row 5 / D1759: the bounds' `(count, offset)`, read (OFFSET first) past the open; `None` when the prologue read them.
    fn late(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Option<(Option<Value>, Option<Value>)> {
        let offset = if self.is_late() { Self::bound(b, env, self.offset, 0) } else { return None };
        Some((Self::bound(b, env, self.count, 1), offset))
    }

    /// The row's tie-key images and null flags: `(words, null)` per key; a key whose image is not exact refuses.
    fn keys(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> PgVec<'g, ([Option<Value>; 2], Value)> {
        let mut cx = env.lx(b, self.node);
        let mut out = mcx::reserved_in(self.ties.len(), self.mcx);
        for t in self.ties.iter() {
            let Some(c) = row.col(t.resno) else { crate::refuse!(self.node, Reason::Bug { payload: "a tie key the row does not carry" }) };
            let w = if t.coll.bytes_eq { rep::hash_image(&mut cx, c, &t.tf, rep::Scope::Program) } else { ImageForm::Coll.need::<VarKeyImpl>(&cx).0.hash(&mut cx, c, &t.tf, t.coll, rep::Scope::Program).unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "key.coll declined a WITH TIES key" })) };
            if !w.exact {
                crate::refuse!(self.node, Reason::Shape { why: ShapeWhy("LIMIT WITH TIES over a key whose image is not exact") });
            }
            let null = match c.null {
                Some(n) => n,
                None => cx.b.bconst(false),
            };
            out.push_in((w.w, null));
        }
        env.memo = cx.memo;
        out
    }
}

impl<'g> StepMech for Limit<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        let mut all = mcx::pg_vec![in self.mcx; ("limit skipped", self.skipped), ("limit emitted", self.emitted)];
        if !self.ties.is_empty() {
            all.push_in(("limit tied", self.tied));
        }
        for t in self.ties.iter() {
            all.extend_in([("limit tie image", t.img[0]), ("limit tie image", t.img[1]), ("limit tie null", t.null)]);
        }
        all.extend_in([self.count, self.offset].iter().zip(self.shared).filter_map(|(l, s)| if let (Lim::Open(c), false) = (l, s) { c.words() } else { None }).flat_map(|[v, f]| [("limit computed bound", v), ("limit computed flags", f)]));
        for (what, off) in all {
            w.declare(OpenWord { what, base: Base::Words, off, ty: Type::I64, init: 0, scope: self.scope });
        }
    }

    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if self.is_late() { return } // `late`: after the owner writes the parameters, at each re-open
        let raises = [self.offset, self.count].iter().any(|l| matches!(l, Lim::Word(_)) || matches!(l, Lim::Const(n) if *n < 0));
        let Some(stop) = raises.then(|| self.above.stop(b, env)).flatten() else {
            let offset = Self::bound(b, env, self.offset, 0);
            let count = Self::bound(b, env, self.count, 1);
            self.bounds.set(Some((count, offset)));
            return;
        };
        // D1755: PG makes the checks above first (a gate's quals, a Limit's bounds: the feeder's or this pipeline's) and
        // below a stopped node never reaches this Limit, so its bounds are read (a negative one raising) only past them;
        // stopped, they read 0 and the call closes
        let n = [self.offset, self.count].iter().filter(|l| !matches!(l, Lim::All)).count();
        let (l_read, l_join) = (b.create_block(&[]), b.create_block(&mcx::pg_vec![in self.mcx; Type::I64; n][..]));
        let zeros = mcx::pg_vec![in self.mcx; env.zero; n];
        b.cond_br(stop, (l_join, &zeros[..]), (l_read, &[]));
        b.switch_to(l_read);
        let offset = Self::bound(b, env, self.offset, 0);
        let count = Self::bound(b, env, self.count, 1);
        let read: PgVec<'_, Value> = [offset, count].into_iter().flatten().collect_in(self.mcx);
        b.br(l_join, &read[..]);
        b.switch_to(l_join);
        let mut joined = b.block_params(l_join).iter().copied();
        let offset = offset.and_then(|_| joined.next());
        let count = count.and_then(|_| joined.next());
        self.bounds.set(Some((count, offset)));
    }

    fn reopen(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Option<Value> {
        // D1903: a bound the top below computes (`shared`) sits in the top's words, which only the top's run resets; read
        // before the set re-runs the top, they hold the last outer row's value (a zero there shut every later re-open), so
        // the read here is fresh (the top's run evaluates it again for its own bound)
        for (l, s) in [self.count, self.offset].iter().zip(self.shared) {
            if let (Lim::Open(c), true) = (l, s) {
                c.words().iter().for_each(|&[_, f]| env.store_word(b, f, env.zero));
            }
        }
        // D3378: a constant count of 0 (D1903 over the inner's rerun set, D1917 over a one-time gate) shuts every re-open, as
        // PG's ExecLimit at 0 returns at LIMIT_EMPTY (with or without ties) before it pulls its child
        if matches!(self.count, Lim::Const(0)) {
            return Some(b.bconst(true));
        }
        let (Some(c), _) = self.late(b, env)? else { return None };
        Some(b.icmp(IntCond::Eq, c, env.zero))
    }

    /// A window past the count (or past the ties) is done at once.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let Some((Some(c), _)) = self.bounds.get().or_else(|| self.late(b, env)) else { return };
        let st = self.words(env);
        let em = b.load(Type::I64, st, self.emitted);
        let mut full = b.icmp(IntCond::Sge, em, c);
        if !self.ties.is_empty() {
            // with ties the step goes on past a nonzero count until a row differs
            let tied = b.load(Type::I64, st, self.tied);
            let done = b.icmp(IntCond::Ne, tied, env.zero);
            let zero = b.icmp(IntCond::Eq, c, env.zero);
            let f0 = b.int_bin(IntBinOp::And, full, zero);
            full = b.int_bin(IntBinOp::Or, f0, done);
        }
        let l_go = b.create_block(&[]);
        b.cond_br(full, (x.l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
    }

    /// D2152: a zero count read at the prologue closes the call (a positive constant or a bound read past the open never does).
    fn closes(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
        if matches!(self.count, Lim::Const(n) if n > 0) {
            return None;
        }
        let (Some(c), _) = self.bounds.get()? else { return None };
        Some(b.icmp(IntCond::Eq, c, env.zero))
    }

    /// The count its earlier calls emitted already reaches the bound read at the prologue (`window`'s test, without ties).
    fn met_at_open(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
        let (Some(c), _) = self.bounds.get()? else { return None };
        if !self.ties.is_empty() {
            return None;
        }
        let em = b.load(Type::I64, self.words(env), self.emitted);
        Some(b.icmp(IntCond::Sge, em, c))
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let Some((count, offset)) = self.bounds.get().or_else(|| self.late(b, env)) else { crate::refuse!(self.node, Reason::Bug { payload: "a limit lowered before its prologue" }) };
        let st = self.words(env);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_cont = b.create_block(&types[..]);
        if let Some(off) = offset {
            let sk = b.load(Type::I64, st, self.skipped);
            let skip = b.icmp(IntCond::Slt, sk, off);
            let (l_skip, l_keep) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(skip, (l_skip, &[]), (l_keep, &[]));
            b.switch_to(l_skip);
            let sk1 = b.int_bin(IntBinOp::Add, sk, env.one);
            b.store(st, self.skipped, sk1);
            // EXPLAIN ANALYZE: PostgreSQL's Limit pulls the rows its OFFSET skips from the node below, which returned them,
            // so that node's rows (the pipeline's, counted where its rows leave: stop.rs `define_exits`) count them too
            // (slow_d2887_sqe_scan_counts' LIMIT + OFFSET, served here since the instrumented zone, D1536); uninstrumented,
            // nothing is emitted
            crate::frame::instr::count_one(b, env.ctx, env.node, crate::frame::instr::Ctr::Rows);
            b.br(l_cont, &row.carry.params());
            b.switch_to(l_keep);
        }
        let Some(c) = count else {
            let c2 = next(b, env, row);
            b.br(l_cont, &c2.params());
            b.switch_to(l_cont);
            return Carry::from_params(b.block_params(l_cont), env.mcx);
        };
        let em = b.load(Type::I64, st, self.emitted);
        if self.ties.is_empty() {
            let em1 = b.int_bin(IntBinOp::Add, em, env.one);
            let count = |b: &mut FunctionBuilder<'_>| b.store(st, self.emitted, em1);
            let c2 = crate::frame::carry::through(b, env, row, next, &count);
            count(b);
            let full = b.icmp(IntCond::Sge, em1, c);
            let l_isfull = b.create_block(&[]);
            b.cond_br(full, (l_isfull, &[]), (l_cont, &c2.params()));
            b.switch_to(l_isfull);
            crate::frame::carry::full(b, env, &c2, row.next);
        } else {
            let keys = self.keys(b, env, row);
            let inside = b.icmp(IntCond::Slt, em, c);
            let (l_in, l_past) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(inside, (l_in, &[]), (l_past, &[]));
            // inside the count: emit, then keep the row's keys as the last emitted
            b.switch_to(l_in);
            let em1 = b.int_bin(IntBinOp::Add, em, env.one);
            let count = |b: &mut FunctionBuilder<'_>| {
                for (t, (w, null)) in self.ties.iter().zip(keys.iter()) {
                    for (i, v) in w.iter().enumerate() {
                        if let Some(v) = v {
                            b.store(st, t.img[i], *v);
                        }
                    }
                    let nw = b.conv(gravity_ir::ConvOp::Zext, Type::I64, *null);
                    b.store(st, t.null, nw);
                }
                b.store(st, self.emitted, em1);
            };
            let c2 = crate::frame::carry::through(b, env, row, next, &count);
            count(b);
            b.br(l_cont, &c2.params());
            // past it: a row equal to the last emitted goes on, the first that differs ends the step
            b.switch_to(l_past);
            let mut same = b.bconst(true);
            for (t, (w, null)) in self.ties.iter().zip(keys.iter()) {
                let sn = b.load(Type::I64, st, t.null);
                let snull = b.icmp(IntCond::Ne, sn, env.zero);
                let mut eq = b.bconst(true);
                for (i, v) in w.iter().enumerate() {
                    if let Some(v) = v {
                        let s = b.load(Type::I64, st, t.img[i]);
                        let e = b.icmp(IntCond::Eq, s, *v);
                        eq = b.int_bin(IntBinOp::And, eq, e);
                    }
                }
                let both_null = b.int_bin(IntBinOp::And, snull, *null);
                let neither = b.int_bin(IntBinOp::Or, snull, *null);
                let t1 = b.bconst(true);
                let neither = b.int_bin(IntBinOp::Xor, neither, t1);
                let valeq = b.int_bin(IntBinOp::And, neither, eq);
                let k_eq = b.int_bin(IntBinOp::Or, both_null, valeq);
                same = b.int_bin(IntBinOp::And, same, k_eq);
            }
            let (l_same, l_diff) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(same, (l_same, &[]), (l_diff, &[]));
            b.switch_to(l_same);
            let c3 = next(b, env, row);
            b.br(l_cont, &c3.params());
            b.switch_to(l_diff);
            b.store(st, self.tied, env.one);
            crate::frame::carry::full(b, env, &row.carry, row.next);
        }
        b.switch_to(l_cont);
        Carry::from_params(b.block_params(l_cont), env.mcx)
    }
}
