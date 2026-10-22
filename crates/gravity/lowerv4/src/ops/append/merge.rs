//! Kind `runs.mergeappend` (cards/M3.md L6, I44): MergeAppend. Decompose feeds every child, already in the merge keys'
//! order, into one Sort slot (an arm: the child's pipeline ending in `Feed { slot, ordered }`), and the reader drains the
//! slot under `AppendDesc::Merge { keys }`. Each arm writes the slot's run through runs/sort.rs's producer over the merge
//! keys: the resident open (`sort_open_res`), `RUN_ORDERED` at the first row (road sort.natural), so an arm that arrives
//! in order is never sorted again. Arm 0 declares the `Arms` protocol over the arms' pipelines (V4R-129: a child Sort's pipeline between arms is no arm) and `SortMerge` on the slot (main
//! 09-25 "b3", exec v4/m3.rs `ArmSort`): each continuing arm's run is held statement-lived, the last arm's section end
//! merges them all once (one k-way), and an arm that spills puts every held run to the stream. The reader is
//! runs/sort.rs's emit over the same shape; the Feed and Merge ops pass the row on.
//! Admission (props/merge.rs `arms_admit`): every arm reads all of its rows, where PostgreSQL's merge may stop early,
//! so an arm whose row can raise refuses (D449's rule). Under a LIMIT the index arms take the pull form (D478) where
//! props `merge_pull` admits it, else this breaker form with each arm capped. Refused by name: arms over a child Sort's
//! run (D482). Ties across arms leave in arm order here (a tie choice: any member of a run of equal keys is PostgreSQL's
//! answer too), where the pull form replays PostgreSQL's heap (merge_pull_heap.rs). A pruning MergeAppend (D509):
//! each arm's Feed step reads the arm's src.prune skip bit (source/prune.rs declares it; a pruned arm's pipeline also
//! claims nothing) and passes no row when it is set; the pull form's reader reads the bits itself (runs/merge_pull.rs).
//! Budget 400 lines. Owner M3-MERGE.
use std::cell::Cell;

use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::{Op, Source};
use gravity_decompose::graph::Bound;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::strategy::SORT_NATURAL;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker, WordWin};
use crate::frame::iface;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::stop::Exits;
use crate::frame::row::{Carry, Mask, RowVals};
use crate::graph::Gx;
use crate::props::Merge;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;

use crate::ops::append::merge_pull as pull;

pub(crate) const ROWS: Table = Table {
    kinds: &[
        (KindId::RunsMergeappend, &StepMaker(make_step)),
        (KindId::RunsMergeappend, &EndMaker(make_end)),
        (KindId::RunsMergeappend, &SourceMaker(make_emit)),
    ],
    ..crate::ops::NONE
};

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// D3374 (e) (record keys E2): the refusal by name of a merge over an anonymous record key whose arms disagree on its
/// columns (it starts with typesem's name for such a key, which a Unique over the merge took before E2).
const PER_ROW_MERGE: &str = "an anonymous record key (its columns are known only per row) merged across arms of other column layouts (record_cmp names its operands' types in PostgreSQL's heap order, which the merge does not keep)";

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Arm pipeline `k`'s row: the projection before its Feed op (the MergeAppend's columns by resno).
fn cols_of<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> &'g NodeList<'p> {
    let p = &gx.pipelines()[k];
    let Some(f) = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))) else { bug(p.node_id, "a MergeAppend arm without its feed") };
    p.ops[..f].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).unwrap_or_else(|| shape(p.node_id, "a MergeAppend arm with no projection"))
}

/// D3931: every arm of the MergeAppend whose run `cols` is one arm's row of (`cols_of`, the list a run end and the reader
/// plan the run's shape over), each as (its pipeline, its Feed op's position); empty for any other run and for the pull
/// form (`merge_pull`: no arm writes a run; its reader holds the Sort runs' rows as they are). The run's key layout is one
/// decision over all of them (rep/keylane.rs `packed_words`).
pub(crate) fn writer_arms<'g>(gx: &Gx<'g, '_>, cols: &NodeList<'_>) -> PgVec<'g, (usize, usize)> {
    let mine = |q: usize| proj_of(gx, q).is_some_and(|(l, _)| core::ptr::eq(l as *const NodeList<'_> as *const (), cols as *const NodeList<'_> as *const ()));
    let found = (0..gx.pipelines().end()).find(|&q| mine(q)).and_then(|q| Merge::of_arm(gx, q));
    let Some(m) = found.filter(|m| !crate::props::merge_pull(gx, m)) else { return PgVec::new_in(gx.mcx) };
    m.arms.iter().filter_map(|&a| proj_of(gx, a).map(|(_, f)| (a, f))).collect_in(gx.mcx)
}

/// Pipeline `q`'s projection before its Feed op and the Feed op's position (`cols_of` without its refusals); none for a
/// pipeline with no Feed.
fn proj_of<'g, 'p>(gx: &Gx<'g, 'p>, q: usize) -> Option<(&'g NodeList<'p>, usize)> {
    let p = &gx.pipelines()[q];
    let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. })))?;
    p.ops[..f].iter().rev().find_map(|op| if let Op::Project(l) = op { Some((*l, f)) } else { None })
}

/// The Feed op of an arm and the Merge op of the reader: the row goes on as it is.
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    match &p.ops[j] {
        Op::Append(AppendDesc::Feed { ordered: false, .. }) => bug(p.node_id, "a MergeAppend arm not in the merge keys' order"),
        // the pull form's reader reads the arms' skip bits (runs/merge_pull.rs); their own pipelines leave at the open
        Op::Append(AppendDesc::Feed { .. }) if Merge::of_arm(px.gx, k).is_some_and(|m| crate::props::merge_pull(px.gx, &m)) => mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g),
        Op::Append(AppendDesc::Feed { .. }) => {
            let inner: PgBox<'g, dyn StepMech + 'g> = match Merge::of_arm(px.gx, k).and_then(|m| crate::props::merge_cap(px.gx, &m)) {
                Some(cap) if serial(px, k) => mcx::pg_box_dyn!(px.mcx(), Cap::new(px, k, j, cap) => dyn StepMech + 'g),
                _ => mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g),
            };
            gate(px, k, inner)
        }
        Op::Append(AppendDesc::Merge { .. }) => mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g),
        _ => bug(p.node_id, "runs.mergeappend's step over an op that is not its feed or merge"),
    }
}

/// The reader's pruning (D509), when its Merge op has one.
pub(crate) fn prune_of<'g, 'p>(gx: &Gx<'g, 'p>, reader: usize) -> Option<&'g PruneDesc<'p>> {
    gx.pipelines()[reader].ops.iter().find_map(|op| if let Op::Append(AppendDesc::Merge { prune: Some(p), .. }) = op { Some(p) } else { None })
}

/// Arm `k`'s Feed step, behind its skip bit when the merge prunes (D509: arm `i` of the merge's `n`).
fn gate<'g>(px: &mut Px<'g, '_>, k: usize, inner: PgBox<'g, dyn StepMech + 'g>) -> PgBox<'g, dyn StepMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(m) = Merge::of_arm(px.gx, k) else { bug(node, "a MergeAppend arm without its merge") };
    let Some(p) = prune_of(px.gx, m.reader) else { return inner };
    let i = m.arms.iter().position(|&q| q == k).unwrap_or_else(|| bug(node, "a MergeAppend arm outside its merge's arms"));
    let skip = (crate::frame::iface::kind_impl::<crate::face::source::PruneImpl>(&node, crate::face::kinds::KindId::SrcPrune).0)(px, k, i as u16, m.arms.len() as u16, p, node);
    mcx::pg_box_dyn!(px.mcx(), Gate { skip, pass: Cell::new(None), inner } => dyn StepMech + 'g)
}

/// A pruned arm passes no row (source/append.rs's arm marker, over the arm's Feed step).
struct Gate<'g> {
    skip: (u16, u32),
    pass: Cell<Option<Value>>,
    inner: PgBox<'g, dyn StepMech + 'g>,
}

/// Skip bit `(word, bit)` clear: the arm is not pruned this execution.
pub(crate) fn unpruned(b: &mut FunctionBuilder<'_>, env: &Env, (word, bit): (u16, u32)) -> Value {
    let w = b.load(Type::I64, env.params, word as i32 * 8);
    let sh = b.iconst(Type::I64, bit as i64);
    let s = b.int_bin(IntBinOp::Ushr, w, sh);
    let s1 = b.int_bin(IntBinOp::And, s, env.one);
    b.icmp(IntCond::Eq, s1, env.zero)
}

impl StepMech for Gate<'_> {
    fn end_is(&mut self, end: &dyn EndMech) {
        self.inner.end_is(end)
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.inner.open_words(w)
    }

    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.pass.set(Some(unpruned(b, env, self.skip)));
        self.inner.prologue(b, env, x)
    }

    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.inner.window(b, env, x)
    }

    fn block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, m: Mask) -> Mask {
        let m = self.inner.block(b, env, w, base, m);
        match self.pass.get() {
            Some(p) => Mask(b.select(p, m.0, env.zero)),
            None => m,
        }
    }

    fn block_reads(&self, env: &Env, w: &WordWin) -> u64 {
        self.inner.block_reads(env, w)
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let Some(pass) = self.pass.get() else { bug(-1, "a pruned merge arm's feed lowered before its prologue") };
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types);
        let l_yes = b.create_block(&[]);
        b.cond_br(pass, (l_yes, &[]), (l_join, &row.carry.params()));
        b.switch_to(l_yes);
        let c = self.inner.lower(b, env, row, next);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.inner.epilogue(b, env, x)
    }

    fn rescan(&self) -> crate::face::step::Rescan {
        self.inner.rescan()
    }

    fn hoist(&self, h: &mut crate::face::step::Hoist) {
        self.inner.hoist(h)
    }
}

/// Arm pipeline `k` runs as one claim stream in its order (a serial pipeline: no worker sees a part of the arm).
fn serial(px: &Px<'_, '_>, k: usize) -> bool {
    matches!(px.gx.pipelines()[k].desc.parallel, gravity_program::Parallel::Ineligible { .. })
}

/// A LIMIT's bound as the arm reads it.
#[derive(Clone, Copy)]
enum Lim {
    Const(i64),
    /// A bind-time parameter word (its null byte beside it).
    Word(u16),
}

/// The arm cap (lever M-B): under the reader's LIMIT, the merge emits at most `count + offset` rows, and an arm in the
/// merge keys' order gives at most its first that many, so the arm's pipeline ends once it has fed them. A NULL or
/// negative bound caps nothing (the reader's LIMIT raises or reads all). V4R-465 (props/merge.rs `first_row`) reads an
/// arm's raise at a cap of one as PostgreSQL's: the arm ends at the row that fills the cap, no row after it evaluated.
struct Cap {
    count: Lim,
    offset: Option<Lim>,
    fed: i32,
    node: i32,
    bound: core::cell::Cell<Option<Value>>,
}

impl Cap {
    fn new<'p>(px: &mut Px<'_, 'p>, k: usize, j: usize, (count, offset): (Bound<'p>, Bound<'p>)) -> Cap {
        let node = px.gx.pipelines()[k].node_id;
        let mut lim = |b: Bound<'p>| match b {
            Bound::All => None,
            Bound::Const(n) => Some(Lim::Const(n)),
            Bound::Expr(e) => match crate::lifetime::bound::word(px, crate::lifetime::bound::Feed::TupleBound, k, e, node) {
                Place::Word(w) => Some(Lim::Word(w)),
                _ => None,
            },
        };
        let (Some(count), offset) = (lim(count), lim(offset)) else { bug(node, "an arm cap without a count") };
        let fed = px.frame_word(WordKey::Open { face: Face::Runs, k, j: crate::frame::px::key_j(j), i: 0 }) as i32 * 8;
        Cap { count, offset, fed, node, bound: core::cell::Cell::new(None) }
    }

    /// The bound's value, i64::MAX when NULL or negative.
    fn value(b: &mut FunctionBuilder<'_>, env: &Env, l: Lim) -> Value {
        let max = b.iconst(Type::I64, i64::MAX);
        match l {
            Lim::Const(n) if n >= 0 => b.iconst(Type::I64, n),
            Lim::Const(_) => max,
            Lim::Word(w) => {
                let nb = b.load_zext(Type::I8, env.params_null, w as i32);
                let v = b.load(Type::I64, env.params, w as i32 * 8);
                let null = b.icmp(IntCond::Ne, nb, env.zero);
                let neg = b.icmp(IntCond::Slt, v, env.zero);
                let none = b.int_bin(IntBinOp::Or, null, neg);
                b.select(none, max, v)
            }
        }
    }
}

impl StepMech for Cap {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("merge arm fed", Base::Words, self.fed, Type::I64, 0));
    }

    /// `count + offset`, saturating (either one uncapped caps nothing).
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        let c = Cap::value(b, env, self.count);
        let n = match self.offset {
            Some(o) => {
                let o = Cap::value(b, env, o);
                let s = b.int_bin(IntBinOp::Add, c, o);
                let max = b.iconst(Type::I64, i64::MAX);
                let wrapped = b.icmp(IntCond::Slt, s, c);
                b.select(wrapped, max, s)
            }
            None => c,
        };
        self.bound.set(Some(n));
    }

    /// A window once the arm fed its cap is done at once.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let n = self.bound.get().unwrap_or_else(|| bug(self.node, "an arm cap before its prologue"));
        let Some(st) = env.words else { bug(self.node, "an arm cap without the frame words") };
        let fed = b.load(Type::I64, st, self.fed);
        let (full, l_go) = (b.icmp(IntCond::Sge, fed, n), b.create_block(&[]));
        b.cond_br(full, (x.l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
    }

    /// The row fed, then counted; the arm ends at the cap (`Env::full`: the carried states stored first).
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let n = self.bound.get().unwrap_or_else(|| bug(self.node, "an arm cap before its prologue"));
        let Some(st) = env.words else { bug(self.node, "an arm cap without the frame words") };
        let c = next(b, env, row);
        let fed = b.load(Type::I64, st, self.fed);
        let fed1 = b.int_bin(IntBinOp::Add, fed, env.one);
        b.store(st, self.fed, fed1);
        let types: PgVec<'_, Type> = c.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let (full, l_full, l_cont) = (b.icmp(IntCond::Sge, fed1, n), b.create_block(&[]), b.create_block(&types));
        b.cond_br(full, (l_full, &[]), (l_cont, &c.params()));
        b.switch_to(l_full);
        env.full(b, &c, row.next);
        b.switch_to(l_cont);
        Carry::from_params(b.block_params(l_cont), env.mcx)
    }
}

/// The arm's producer: runs/sort.rs's run end over the merge keys, the arm's run marked ordered from its first row and
/// opened resident; arm 0 declares the arms' and the slot's protocols.
fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(m) = Merge::of_arm(gx, k) else { bug(node, "a MergeAppend arm whose reader the graph does not hold") };
    // D449: the pull form reads PostgreSQL's rows exactly (one per arm at the open, then the emitting arm's next), so an
    // arm row that can raise raises where PostgreSQL's does; only the breaker form reads past the early stop. v5
    // errorhold (worklist item 9): there each arm runs ahead under a recheck licence (`Px::run_ahead`): an error before
    // any row left re-runs the statement in plan order, where the licence is denied and D449's name answers.
    // `DECISIONS.md` 4 (D) b: where no plan-order run re-checks the arm (rows may leave before it runs), it runs ahead
    // unchecked (`Px::ahead_unchecked`): a genuine error from its rows past the early stop is the statement's, tagged
    let _raise = raise_ahead(px, k, &m, node);
    // D2733: the root's full read admits the breaker form's whole arms only while the statement runs to its end (a cursor's
    // FETCH may stop PostgreSQL's merge after a few rows): then it is served only to a reader that takes every row
    let _whole = whole_ahead(px, k, &m);
    // USEROP (K10.y1 .h0): a merge key of another family's order (decompose append.rs `merge_keys`) whose comparator is
    // SQL / PL: the breaker merge (the arms' runs, then one k-way) calls it on pairs of its own, not those of PostgreSQL's
    // binary heap over the arms' heads
    if m.keys.iter().any(|key| key.compare_fn() && !crate::rep::keylane::c_level_cmp(gx, key.opno)) {
        shape(node, crate::rep::keylane::USER_ARRIVAL);
    }
    // D3374 (e) (record keys E2): an anonymous record key whose arms disagree on its columns (decompose append.rs
    // `merge_keys`): record_cmp raises 42804 naming its operands' types in its call's order, and neither form calls it on
    // PostgreSQL's pairs in that order (the breaker's k-way merges pairs of its own)
    if m.keys.iter().any(|key| key.per_row) {
        shape(node, PER_ROW_MERGE);
    }
    // D478: under the reader's LIMIT a breaker arm reads up to count + offset rows where PostgreSQL's merge pulls about
    // (count + offset) / arms from each (LIMIT 5000 over 4 index arms: 2.6x rowexec hot, 5.7x cold): the pull form
    // (runs/merge_pull.rs) reads them in the reader, and the arm's own pipeline reads nothing. Where it does not apply (an
    // arm that is a join, a key it cannot hold) the breaker form serves, each arm capped at count + offset rows (`Cap`):
    // D449's rule above holds, so the rows it reads past PostgreSQL's share raise nothing (a slower form is served, never
    // refused for its speed)
    if crate::props::merge_pull(gx, &m) {
        // the arms still produce the slot (empty): one append.arms run opens its region once and frees it after the reader
        if k == m.arms[0] {
            fires::fire("merge.pull"); // shapecheck fire signal (REGISTRY NEW-lowerv4-a2-165, STEP-25, CAP-02)
            let mut arms = Proto::new(ProtoKind::Arms, k as u16);
            (arms.arg, arms.slot) = ((m.arms[m.arms.len() - 1] - k + 1) as u64, m.slot);
            px.declare_proto(arms);
        }
        return mcx::pg_box_dyn!(px.mcx(), pull::Fused => dyn EndMech + 'g);
    }
    let cols = cols_of(gx, k);
    if k == m.arms[0] {
        fires::fire("merge.breaker"); // shapecheck fire signal (REGISTRY NEW-lowerv4-38)
        let first = crate::face::runs::Shape::plan(px, node, m.keys, None, cols);
        let mut arms = Proto::new(ProtoKind::Arms, k as u16);
        // V4R-129: `arg` counts the run's pipelines, a child Sort's between two arms included (it produces another slot)
        (arms.arg, arms.slot) = ((m.arms[m.arms.len() - 1] - k + 1) as u64, m.slot);
        px.declare_proto(arms);
        iface::sort_resident(px, k, m.slot, (!first.var()).then_some((first.spec, 2 * first.keys.len() as u16)));
    }
    // the resident open is b3's protocol, not road sort.resident's; the ordered mark holds unless sort.natural is forced off
    let natural = crate::face::source::on(Variant::Strategy(SORT_NATURAL));
    iface::sort_end_over(px, k, m.keys, cols, iface::SortArm { keylane: false, natural, resident: true, finish: false })
}

/// D449 / v5 errorhold: the licence the breaker form's arms run ahead of PostgreSQL's early stop under where an arm row
/// can raise past it (`arms_admit` fails): `Px::run_ahead`'s recheck licence, else `Px::ahead_unchecked`'s; both
/// denied, D449's name refuses. `None`: the pull form, or arms `arms_admit` admits (no licence asked).
fn raise_ahead(px: &mut Px<'_, '_>, k: usize, m: &Merge<'_>, node: i32) -> Option<gravity_decompose::effects::Licence> {
    let gx = px.gx;
    if crate::props::arms_admit(gx, m) || crate::props::merge_pull(gx, m) {
        return None;
    }
    let raises = gravity_decompose::effects::Effects::RAISES;
    let l = px.run_ahead(k, raises).or_else(|| px.ahead_unchecked(k, raises));
    if l.is_none() {
        shape(node, "a MergeAppend arm whose rows can raise past PostgreSQL's early stop (D449's rule)");
    }
    l
}

/// D2733: the whole-read licence of the breaker form's arms where the root's full read admits them only while the
/// statement runs to its end (`merge_admit::whole_only`), served to a reader that takes every row alone
/// (`shared::whole::read`, over the arms' effects and their producers'). `None`: the pull form, a merge `whole_only`
/// does not take, or arms that act past call-local builtins.
fn whole_ahead(px: &mut Px<'_, '_>, k: usize, m: &Merge<'_>) -> Option<gravity_decompose::effects::Licence> {
    let gx = px.gx;
    if crate::props::merge_pull(gx, m) || !crate::ops::append::merge_admit::whole_only(gx, m) {
        return None;
    }
    let own = m.arms.iter().fold(gravity_decompose::effects::Effects::NONE, |e, &a| e.and(crate::effects::pipeline_effects(gx, px.arena, a, true)));
    crate::ops::shared::whole::read(px, k, own)
}

/// The reader: runs/sort.rs's emit of the slot's merged run, in the last arm's shape (every arm's is the same).
fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    if !matches!(gx.pipelines()[k].source, Source::Breaker { .. }) {
        return pull::empty(px, k);
    }
    let reader = Merge::of_reader(gx, k);
    // a pull-form merge's Sort arm (graph.rs maps its Breaker source here) reads nothing; its reader pulls the run
    if reader.is_none() && Merge::of_arm(gx, k).is_some_and(|m| crate::props::merge_pull(gx, &m)) {
        return pull::empty(px, k);
    }
    let Some(m) = reader else { bug(node, "a MergeAppend's emit without its merge op or arms") };
    let cols = cols_of(gx, *m.arms.last().expect("a merge has an arm"));
    if crate::props::merge_pull(gx, &m) {
        return pull::make(px, k, &m, cols);
    }
    iface::sort_emit_over(px, k, m.keys, cols)
}
