//! Kind `nest.memo` (Memoize inside a nest inner; M8-plan §3.4, cards/M8.md L4; MC-1, M8-MEMO-CTE).
//! PostgreSQL's ExecMemoize (nodeMemoize.c:697-950) over the rows of the embedded inner below it:
//! - The re-open (`Memo::open`, called by nest/inner.rs after the inner's per-open words): the key (`param_exprs`)
//!   evaluated and looked up (MEMO_CACHE_LOOKUP, `m8_memo_miss`). A complete entry is a hit: the inner's source is never
//!   opened, and the walk replays the entry's rows (MEMO_CACHE_FETCH_NEXT_TUPLE) through the steps above the Memoize.
//!   Anything else is a miss: the inner runs.
//! - The fill (`Fill`, nest/memo_fill.rs, the step at the Memoize's place in the inner's chain): each row the inner
//!   hands on is stored as it goes (`m8_memo_store`, MEMO_FILLING_CACHE), never ahead of the consumer, so a row
//!   PostgreSQL never fetches (a LIMIT above the nest, a SEMI / ANTI leave) is never evaluated. A store the table cannot
//!   keep is MEMO_CACHE_BYPASS_MODE for the rest of the scan. The inner's end completes the entry (`m8_memo_done`); a
//!   scan cut short leaves it incomplete, and its next lookup purges it and runs the inner again. (cards/M8.md's "pull
//!   the inner to the end" miss is not PostgreSQL's and is not built.)
//! - Keys compare as the plan node says (`binary_mode`: by image; else the hash equality operator and its hash function
//!   with the key's collation), in the table (hosts v4/m8_memo.rs, exec v4/m8_memo.rs).
//! - A stop inside the continuation of a stored row keeps the `pend` word set: the row re-entered (D688) is not stored
//!   twice, and a resume past it clears the word (`enter`). A replayed row's position is its own per-open word (`pos`);
//!   the walk's `skip` re-enters or passes it as it does a site row. With one set function below the Memoize (D1018 tail
//!   row 1) the walk re-enters its source row at every resume inside the set, so `pend` holds the set's rows handed on
//!   (`row_tag`): the row re-entered has the same tag and is not stored twice, the set's next row a new one and is stored.
//! - A replayed cell is the stored datum (`At::Value`), living until the row's end: the entry's copies live until the
//!   next lookup, which only the next outer row makes.
//! - A pgrc inner's dictionary (`Rep::Code`) and window (`PackedNum`) cells are stored and handed on site-free, as
//!   nest.loop's `site_free` makes them for the owner's continuation (fill.rs `site_free`): a code its rendered varlena,
//!   a packed numeric its order word, made in the fill under the inner's site; the replay hands on the same forms.
//! - A volatile call below the Memoize runs at a miss only, as PostgreSQL's: its planner memoizes an inner whose volatile
//!   calls sit inside a subquery or a FROM function, and the cache lives one execution (exec ops/memoize/proto.rs).
//! - Inside a re-run plan (its nest's pipeline an embedded inner or a rerun set's member: opened again per row of another
//!   owner) PG's ExecReScanMemoize purges every entry at the first rescan after a param outside the keys changed;
//!   decompose's `JoinDesc::Memo.purge` / `.nonkey` say whether the child reads one and which (none: PG never purges, nor
//!   does the memo). Then the nest's step declares the purge word (nestloop/lower.rs `open_words` through the inner's
//!   `owner_words`), written 1 at every open of that pipeline, and the first lookup after it purges (`MEMO_PURGE`) and
//!   clears it. That is PG's purge exactly where every re-opener of the pipeline sets one of those params
//!   (lifetime/memoize.rs `purge_exact`: PG marks a param changed at every row of its owner); one that sets none re-opens
//!   where PG keeps the cache, and the extra purge only re-runs the child with the params PG's entry saw: the same rows
//!   and errors with no VOLATILE call below (more misses only), so it is served then.
//! - A child reading its own nest's param outside the keys (D3836: PG plans it for a LATERAL union whose arm returns an
//!   outer column, a PlaceHolderVar with its own param, beside an arm reading the column's Var): PG purges at every
//!   rescan, so it never hits; every lookup purges (`MEMO_PURGE`), the child runs at every outer row, the calls PG makes.
//! Fails closed by name: a key computed by a volatile call (PostgreSQL never plans one), an instrumented lowering (EXPLAIN
//! ANALYZE's hits / misses are M12's), a Memoize over VOLATILE code inside a re-run plan with a re-opener that sets none
//! of the child's params outside its keys (`purge_exact`: the calls would outnumber PG's),
//! a purge word no owner declared, over a one-time gate, over a cstring; over more than one set function, or one over
//! another source than a Result's one row.
//! Owner M8-MEMO-CTE.
use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::status::raise;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{ExprLife, ExprOut, NumForm, Rep};
use mcx::{GrowIn, Mcx, PgBox, PgVec};

use crate::cell::{Cell, Nullable};
use crate::face::kinds::{Face, KindId};
use crate::face::step::{ExprFace, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::StepMaker;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::{self, CellRef};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestMemo, &StepMaker(make_alone))], ..crate::ops::NONE };

/// D1018 tail row 1 (GAP-W3b-D1018-TAIL): one set function below the Memoize, over a Result's one row (`row_tag`).
pub(crate) const D_SRF_TWO: &str = "a Memoize over more than one set function below it (D1018 tail)";
pub(crate) const D_SRF_SRC: &str = "a Memoize over a set function whose inner source reads more than one row per open (D1018 tail)";

/// memoize entry D: a Memoize over VOLATILE code in a re-run plan whose pipeline a re-opener opens without setting any param
/// the child reads outside its keys: PG keeps the cache across that re-opener's rows, the purge word would drop it and run
/// the calls again (`purge_exact`).
pub(crate) const D_PURGE: &str = "a Memoize over VOLATILE code inside a re-run plan re-opened by an owner that sets none of the parameters its child reads outside its keys (PG keeps its cache there)";
const D_UNARMED: &str = "a Memoize purge word no owner declared (a re-run plan's memo outside nest.loop's inner)";

/// The state word after the re-open.
pub(crate) const FILLING: i64 = 1;
pub(crate) const BYPASS: i64 = 2;
pub(crate) const REPLAY: i64 = 3;

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// A Memoize is reached only through its nest's inner (nest/inner.rs makes it with `make`).
fn make_alone<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = crate::rerun::inner::op_node(&px.gx.pipelines()[k], j);
    shape(node, "a Memoize outside a nest's inner")
}

/// One key: its expression, its type, its datum's form, and its hash equality operator and collation.
struct Key {
    ex: ExId,
    tf: TypeFacts,
    out: ExprOut,
    op: u32,
    coll: u32,
}

/// What the re-open, the fill and the replay share.
pub(crate) struct Core<'g> {
    pub(crate) mcx: Mcx<'g>,
    pub(crate) node: i32,
    pub(crate) h: u16,
    keys: PgVec<'g, Key>,
    flags: u64,
    ex: &'static ExprFace,
    /// Per-open frame words (byte offsets): the state, the stored-row flag, the hit's rows, the replay position.
    pub(crate) state: i32,
    pub(crate) pend: i32,
    pub(crate) hit: i32,
    pub(crate) pos: i32,
    /// A re-run plan's purge word (`None`: no purge, PG never purges this memo there), and whether its owner declared it.
    purge: Option<i32>,
    /// D3836: every lookup purges (PG purges at every rescan: the child reads its own nest's param outside the keys).
    every: bool,
    armed: std::cell::Cell<bool>,
    /// The row at the Memoize as the fill first lowered it: each cell, its datum's IR type, and the column map.
    pub(crate) shape: RefCell<Option<(PgVec<'g, Cell>, PgVec<'g, Type>, PgVec<'g, Option<u16>>)>>,
}

/// The re-open and replay side, held by the inner.
pub(crate) struct Memo<'g> {
    pub(crate) core: Rc<Core<'g>>,
}

/// A replayed row `(j, row)` through the steps past the Memoize and the owner's continuation: the carry after.
pub(crate) type Replay<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, Value, &RowVals) -> Carry<'e>;

/// The fill step at the Memoize's place in the inner's chain.
pub(crate) struct Fill<'g> {
    pub(crate) core: Rc<Core<'g>>,
}

/// The inner's Memoize op, if any (nest/inner.rs).
pub(crate) fn at(ip: &PipelineNode<'_>) -> Option<usize> {
    ip.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Memo { .. })))
}

/// Whether `op` is a row-at-a-time inner op (nest/inner.rs's D1018 test); past a Memoize only filters and projections
/// (a replayed row runs them, never the ops below the Memoize); below it also a set function (D1018 tail row 1: one, over
/// a Result's row, `make_parts`; the fill tags its rows, `row_tag`).
pub(crate) fn row_op(op: &Op<'_>, past_memo: bool) -> bool {
    match past_memo {
        true => matches!(op, Op::Filter(_) | Op::Project(_)),
        false => matches!(op, Op::Filter(_) | Op::Project(_) | Op::Gate(_) | Op::SubPlan(_) | Op::Srf(_)),
    }
}

/// D1018 tail row 1, the fill's stored-row tag: the rows the one expanding step below the Memoize has handed on (its offset
/// word; every set function's step writes k + 1 before it hands row k on), so the row a resume re-enters (the same count)
/// is not stored twice and the set's next row (a new count) is; `None` with no expanding step below (the flag 1, as
/// before: the IR is unchanged). More than one fails closed (`make_parts` refuses a second set function by name first).
pub(crate) fn row_tag(b: &mut FunctionBuilder<'_>, env: &Env, node: i32) -> Option<Value> {
    let mut below = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 < j)).map(|e| e.1);
    match (below.next(), below.next()) {
        (None, _) => None,
        (Some(o), None) => Some(env.load_word(b, o)),
        _ => crate::refuse!(node, Reason::Bug { payload: "a Memoize fill over more than one expanding step" }),
    }
}

/// The memo of op `m` (step `s`) of inner `ik`, embedded by op `j` of pipeline `k` (its nest): its re-open side into
/// `memo`, its fill step returned.
/// Never over a spool (nest.spool): PG plans Memoize only over a parameterized inner and Material only over an unparameterized one.
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), ik: usize, (m, si): (usize, usize), memo: &mut Option<(usize, Memo<'g>)>) -> PgBox<'g, dyn StepMech + 'g> {
    let n0 = crate::rerun::inner::op_node(&px.gx.pipelines()[ik], m);
    let _ = crate::frame::iface::kind_impl::<StepMaker>(&n0, KindId::NestMemo);
    let (mm, fill) = make_parts(px, (k, j), ik, m);
    *memo = Some((si, mm));
    fill
}

fn make_parts<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), ik: usize, m: usize) -> (Memo<'g>, PgBox<'g, dyn StepMech + 'g>) {
    let gx = px.gx;
    let ip: &PipelineNode<'_> = &gx.pipelines()[ik];
    let node = crate::rerun::inner::op_node(ip, m);
    let Some(Op::Join(JoinDesc::Memo { keys, singlerow, binary, ops, colls, purge, nonkey, .. })) = ip.ops.get(m) else { crate::refuse!(node, Reason::Bug { payload: "a memo over another op" }) };
    // a volatile call below the Memoize runs once per miss, as PG's does: its planner memoizes an inner whose volatile calls
    // sit inside a subquery or a FROM function (get_memoize_path tests only the inner rel's target list and quals), and the
    // fill and replay keep ExecMemoize's misses (the inner runs at a miss only, a row stored as handed on)
    let Some(Op::Join(JoinDesc::Nest { facts, .. })) = gx.pipelines()[k].ops.get(j) else { shape(node, "a Memoize under another op than a nest") };
    // D3836: a child reading its own nest's param outside the keys (a LATERAL union's arm returning an outer column reads a
    // PlaceHolderVar's param beside the Var's another arm reads): PG's nest marks each of its params changed at every outer
    // row, so ExecReScanMemoize purges at every rescan and no lookup hits; every lookup purges here (`Core::every`)
    let every = *purge && crate::lifetime::memoize::own_param(gx, nonkey, facts.node);
    // PG's ExecReScanMemoize purges the cache when a parameter outside its keys changed: a nest re-run by a rerun set or
    // inside another inner may see one, only where its child reads one (decompose's `purge`); with none PG never purges,
    // and the cache, one per execution, lives across the re-runs as PG's does; with one the purge word (`owner_words`)
    let rerun = crate::lifetime::memoize::rerun(gx, k);
    if *purge && rerun && !every && crate::effects::visible::memoize::purge_calls(facts.volatility) && !crate::lifetime::memoize::purge_exact(gx, k, nonkey) {
        shape(node, D_PURGE);
    }
    if crate::lifetime::memoize::gated(&ip.ops, m) {
        shape(node, "a Memoize inner with a one-time gate");
    }
    // D1018 tail row 1: one set function below, over a Result's one row (the fill's tag names a set row only while the
    // source row is fixed). The frozen no-widen list is gone (memoize item 8): D2178 / D2179 left it at ROOTFIX-CTE-PARK,
    // D2213 at GAP-D2213-GUARD (a SubPlan refuses a nest inner's site cells by its own name), as D2180 / D2197 / D2194 had
    match ip.ops[..m].iter().filter(|op| matches!(op, Op::Srf(_))).count() {
        0 => {}
        1 if !matches!(ip.source, Source::Const) => shape(node, D_SRF_SRC),
        1 => {}
        _ => shape(node, D_SRF_TWO),
    }
    // any key count, as PG's (NEST-02): the key block is a stack slot of `n` keys (`lookup`), the host's probe grows to it
    let n = keys.len();
    if n == 0 || ops.len() != n || colls.len() != n {
        shape(node, "a Memoize whose keys are not one or more with an operator and a collation each");
    }
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    let mut ks = mcx::reserved_in(n, px.mcx());
    for (i, key) in keys.iter().enumerate() {
        let e = ex.0.compile_at(px, (ik, m), key, None, node);
        if !px.placed(e).held.is_empty() {
            shape(node, "a Memoize key computed by a volatile call");
        }
        let typid = ex.0.out_type(gx, key, node);
        let tf = ex.0.tf_of(gx, typid);
        if tf.typlen == -2 {
            shape(node, "a Memoize key of a cstring type");
        }
        let out = gx.out_of(key).unwrap_or(ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid, life: ExprLife::Row });
        ks.push_in(Key { ex: e, tf, out, op: ops[i], coll: colls[i] });
    }
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::Cache, k as u16).words(h, 1).arg(wide_key(*binary, ops, colls)));
    // EXPLAIN ANALYZE: the cache's hits, misses, evictions, overflows and peak bytes, read from the memo object at the
    // run's end (frame/instr.rs `memo_cells`)
    crate::frame::instr::memo_cells(node, h);
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: crate::frame::px::key_j(m), i }) as i32 * 8;
    let (state, pend, hit, pos) = (word(px, 0), word(px, 1), word(px, 2), word(px, 3));
    let purge = (*purge && rerun && !every).then(|| word(px, 4));
    let flags = if *binary { abi::MEMO_BINARY } else { 0 } | if *singlerow { abi::MEMO_SINGLEROW } else { 0 };
    let core = Rc::new(Core { mcx: px.mcx(), node, h, keys: ks, flags, ex, state, pend, hit, pos, purge, every, armed: Default::default(), shape: RefCell::new(None) });
    (Memo { core: core.clone() }, mcx::pg_box_dyn!(px.mcx(), Fill { core } => dyn StepMech + 'g))
}

/// D3014 M2: the proto's `arg`, the one key a widened nest's workers compare and hash with functions the leader resolves
/// at prepare (exec ops/memoize/proto.rs `wide_key`; a pool worker reads no catalog): its hash equality operator, and its
/// collation in the high half. 0 for a binary-mode memo (no function) or more than one key (`Memo::widens`: that nest
/// stays serial). A serial run never reads it.
fn wide_key(binary: bool, ops: &[u32], colls: &[u32]) -> u64 {
    match (binary, ops, colls) {
        (false, [op], [coll]) => *op as u64 | (*coll as u64) << 32,
        _ => 0,
    }
}

pub(crate) fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// Leaves by the raise exit with the host's stashed error when `r` is below zero.
pub(crate) fn raise_if_bad(b: &mut FunctionBuilder<'_>, env: &Env, r: Value) {
    let bad = b.icmp(IntCond::Slt, r, env.zero);
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

/// `v` as the 64-bit word a block holds (bits kept).
pub(crate) fn to_word(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    match b.value_type(v) {
        Type::I64 => v,
        Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, v),
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, v),
        Type::F32 => {
            let i = b.conv(ConvOp::Bitcast, Type::I32, v);
            b.conv(ConvOp::Zext, Type::I64, i)
        }
        Type::Bool => b.conv(ConvOp::Zext, Type::I64, v),
        _ => b.conv(ConvOp::Sext, Type::I64, v),
    }
}

/// A stored word back as a value of type `ty`.
pub(crate) fn from_word(b: &mut FunctionBuilder<'_>, w: Value, ty: Type) -> Value {
    match ty {
        Type::I64 => w,
        Type::Ptr => b.conv(ConvOp::IntToPtr, Type::Ptr, w),
        Type::F64 => b.conv(ConvOp::Bitcast, Type::F64, w),
        Type::F32 => {
            let i = b.conv(ConvOp::Trunc, Type::I32, w);
            b.conv(ConvOp::Bitcast, Type::F32, i)
        }
        Type::Bool => {
            let z = b.iconst(Type::I64, 0);
            b.icmp(IntCond::Ne, w, z)
        }
        t => b.conv(ConvOp::Trunc, t, w),
    }
}

/// The image's bytes of by-reference datum `d` of `typlen` (0 for a NULL, never reading it).
pub(crate) fn image_len(b: &mut FunctionBuilder<'_>, d: Value, null: Value, typlen: i16) -> Value {
    let zero = b.iconst(Type::I64, 0);
    let (l_len, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(null, (l_join, &[zero]), (l_len, &[]));
    b.switch_to(l_len);
    let p = match b.value_type(d) {
        Type::Ptr => d,
        _ => b.conv(ConvOp::IntToPtr, Type::Ptr, d),
    };
    let n = gravity_expr::byref::image_len(b, p, typlen);
    b.br(l_join, &[n]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// A cell's by-reference typlen (`None`: by value); a form the memo cannot store refuses by name. A pgrc cell reaches
/// here site-free (fill.rs `site_free`): a kept order word is a word.
pub(crate) fn byref_len(node: i32, c: &Cell) -> Option<i16> {
    match c.rep {
        Rep::Word { .. } | Rep::PackedNum { form: NumForm::Word, .. } => None,
        Rep::VarlenaPtr => Some(-1),
        Rep::FixedPtr { len } => Some(len as i16),
        Rep::CstrPtr => shape(node, "a Memoize over a cstring column"),
        Rep::Code { .. } | Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn => crate::refuse!(node, Reason::Bug { payload: "a memo cell in its site's dictionary or window form past fill.rs site_free" }),
    }
}

impl Memo<'_> {
    /// D3014 M2: whether a widened nest's workers can make this memo's tables with no catalog read: a binary-mode memo
    /// calls no function, and one key's travel in the proto's `arg` (`wide_key`); more than one key keeps the nest serial.
    pub(crate) fn widens(&self) -> bool {
        self.core.flags & abi::MEMO_BINARY != 0 || self.core.keys.len() == 1
    }

    /// nest.loop's words (its `open_words`, through the inner's `owner_words`): a re-run plan's purge word, written 1 at
    /// every open of the nest's pipeline. Only the nest's step declares it: the inner's own per-open words would purge at
    /// every outer row of the memo's nest.
    pub(crate) fn owner_words(&self, w: &mut OpenWords) {
        if let Some(o) = self.core.purge {
            w.declare(OpenWord::open("nest.memo purge", Base::Words, o, Type::I64, 1));
            self.core.armed.set(true);
        }
    }

    /// The re-open after the inner's per-open words: the lookup (`lookup`), then on a miss only `source` (the inner's
    /// open: PG's Memoize never pulls its child on a hit). The open's status (0 on a hit).
    pub(crate) fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, source: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> Value) -> Value {
        let hit = self.lookup(b, env);
        let (l_src, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(hit, (l_join, &[env.zero]), (l_src, &[]));
        b.switch_to(l_src);
        let r = source(b, env);
        b.br(l_join, &[r]);
        b.switch_to(l_join);
        b.block_params(l_join)[0]
    }

    /// MEMO_CACHE_LOOKUP: the key block, the lookup, the state (a hit replays, a miss fills). The `Bool` hit.
    fn lookup(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let c = &*self.core;
        let n = c.keys.len();
        let none = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        let slot = b.declare_stack_slot(((abi::MEMO_KEY_HEAD + n * abi::MEMO_KEY_WORDS) * 8) as u32, 8);
        let kb = b.stack_addr(slot);
        let (nk, mut fl) = (b.iconst(Type::I64, n as i64), b.iconst(Type::I64, c.flags as i64));
        // a re-run plan's first lookup after its nest's pipeline opened purges (ExecReScanMemoize), and clears the word
        if c.every {
            fl = b.iconst(Type::I64, (c.flags | abi::MEMO_PURGE) as i64);
        } else if let Some(o) = c.purge {
            if !c.armed.get() {
                shape(c.node, D_UNARMED);
            }
            let set = env.load_word(b, o);
            let on = b.icmp(IntCond::Ne, set, env.zero);
            let pf = b.iconst(Type::I64, (c.flags | abi::MEMO_PURGE) as i64);
            fl = b.select(on, pf, fl);
            env.store_word(b, o, env.zero);
        }
        b.store(kb, 0, nk);
        b.store(kb, 8, fl);
        let t = b.bconst(true);
        for (i, k) in c.keys.iter().enumerate() {
            let (v, valid) = c.ex.0.emit(b, env, c.node, &none, k.ex);
            let isnull = b.int_bin(IntBinOp::Xor, valid, t);
            let cell = Cell::of_expr(k.out, 0, 0, Nullable::Maybe);
            let mut cx = env.lx(b, c.node);
            let (d, null) = rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(isnull) }, &k.tf);
            env.memo = cx.memo;
            env.memo.clear();
            let len = match (!k.tf.byval).then_some(k.tf.typlen) {
                Some(tl) => image_len(b, d, null, tl),
                None => env.zero,
            };
            let off = ((abi::MEMO_KEY_HEAD + i * abi::MEMO_KEY_WORDS) * 8) as i32;
            let w = to_word(b, d);
            let nw = b.conv(ConvOp::Zext, Type::I64, null);
            let (op, coll, tl) = (b.iconst(Type::I64, k.op as i64), b.iconst(Type::I64, k.coll as i64), b.iconst(Type::I64, k.tf.typlen as i64));
            for (x, v) in [w, nw, len, op, coll, tl].into_iter().enumerate() {
                b.store(kb, off + 8 * x as i32, v);
            }
        }
        let hv = b.load(Type::I64, env.params, c.h as i32 * 8);
        let f = host(b, "m8_memo_miss", &[Type::Ptr, Type::I64, Type::Ptr], gravity_abi::host_words!(abi::m8_memo_miss(ctx, h, key)));
        let r = b.call_host(f, &[env.ctx, hv, kb], Some(Type::I64)).unwrap_or(env.zero);
        raise_if_bad(b, env, r);
        env.store_word(b, c.hit, r);
        let hit = b.icmp(IntCond::Sgt, r, env.zero);
        let (rp, fi) = (b.iconst(Type::I64, REPLAY), b.iconst(Type::I64, FILLING));
        let s = b.select(hit, rp, fi);
        env.store_word(b, c.state, s);
        hit
    }
}
