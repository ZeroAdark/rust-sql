//! A keyed group's record as its ends and its emit read it (`GroupRec`: its layout, key images, representatives, key
//! sources and aggregates; `new`, `counts`, the key image, the argument cells and the row step), with the argument and
//! step-target types (`Arg`, `Agg`, `AccAt`, `KeyVal`) and `ex_ref` / `ex_val`. A pure move out of keyed/hashagg.rs
//! (main 09-26), re-exported there; shared with keyed_emit_group, keyed_twopass and runs_topk_groups. Owner M1-KEYED.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, Register, NONE};
use gravity_decompose::ops::shared::agg_class::{Class, Trans};
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_program::{LifeId, Rep};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::cell::Cell;
use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::table::face::TableDecl;
use crate::table::keyimg;
use crate::table::keyimg::{ImageWords, KeyCell};
use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::{CellRef, Scope};

use crate::table::emit_group::{key_col, key_nullable, rep_src, RepSrc};

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Expression `id` over `row` as a cell of `cell`'s kind (its NULL `!valid`).
pub(crate) fn ex_ref<'r>(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, cell: &'r Cell) -> CellRef<'r> {
    let (val, null) = ex_val(b, env, node, row, id);
    CellRef { cell, val, null: Some(null) }
}

/// Expression `id` over `row`: its value and its NULL (`!valid`).
pub(crate) fn ex_val(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> (Value, Value) {
    let (v, valid) = crate::step::expr::emit(b, env, node, row, id);
    let t = b.bconst(true);
    (v, b.int_bin(IntBinOp::Xor, valid, t))
}

/// D1520: direct argument cell `c` as a hosted finalfn is given it (kernels `DIRECT_*`): its datum word, and VALUE or NULL;
/// shared by the keyed end (a group's row) and the plain drain (a Param).
pub(crate) fn direct_word(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, c: CellRef<'_>, tf: &TypeFacts) -> (Value, Value) {
    use gravity_kernels::ops::agg::trans::hosted::{DIRECT_NULL, DIRECT_VALUE};
    let (v, null) = {
        let mut cx = env.lx(b, node);
        let d = crate::rep::datum(&mut cx, c, tf);
        env.memo = cx.memo;
        d
    };
    let (value, nul) = (b.iconst(Type::I64, DIRECT_VALUE as i64), b.iconst(Type::I64, DIRECT_NULL as i64));
    let v = if b.value_type(v) == Type::Ptr { b.conv(gravity_ir::ConvOp::PtrToInt, Type::I64, v) } else { v };
    (v, b.select(null, nul, value))
}

/// A key's cell in the producer's row, by value (a DISTINCT argument's key cell is made per lowering).
pub(crate) type KeyVal = (Cell, Value, Option<Value>);

/// An aggregate argument: a column of the row (its cell as it is: a code lane stays a code for min / max over codes) or an
/// expression over the row (expr.core), whose value is a datum of the expression's output rep.
pub(crate) enum Arg {
    Col(i16),
    Ex { id: ExId, cell: Cell },
    /// A DISTINCT argument read as its image alone: its per-entry `Fp128` form where it has one (keyed_distinct_plain `key_ref`).
    Key { id: ExId, cell: Cell },
}

/// V4R-367 (main 07:07Z design 2, 07:08Z allowlist): the one FILTER every pair aggregate of `g` carries, `None` with none.
/// The pair set keys the argument as NULL on a row the FILTER does not pass (`GroupRec::key_vals`), so the owner's
/// null_arg rule counts / folds no pair for it: PG's DISTINCT aggregate never sees that row, and a strict transition
/// function (or one of the NULL-ignoring three) ignores a NULL argument alike. Refused by name: a FILTER beside a second
/// pair aggregate (one pair set per group, so their FILTERs must be one; whether two are is their literals' values, which
/// the program keys hold as words: exec front.rs, lower key.rs; a program cached for equal FILTERs would serve unequal
/// ones), a transition function that is neither (pg_proc.proisstrict of the aggregate's aggtransfn: the effects home's
/// FuncProps of decompose aggtab's transfn_oid, PG 18.6's catalog; a user aggregate has no row). An argument that is
/// not a column of the producer's row is evaluated only on rows the FILTER passes (`key_vals`), as PG evaluates it.
pub(crate) fn pair_filter<'p>(g: &GroupDecl<'p>, node: i32) -> Option<types_nodes::Node<'p>> {
    // the tagged form's FILTERs are its keys' (`GroupRec::key_filters`)
    if crate::ops::shared::agg::tagged_multi(g) {
        return None;
    }
    let pairs = || g.aggs.iter().filter(|d| matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold));
    let f = pairs().find_map(|d| d.filter)?;
    if pairs().count() > 1 {
        shape(node, "a FILTER on a DISTINCT aggregate beside a second one over the same argument");
    }
    for d in pairs() {
        if !gravity_decompose::ops::agg::trans::table::row(d.fnoid).is_some_and(|r| transfn_strict(r) || NULL_IGNORING.contains(&r.transfn)) {
            shape(node, "a FILTER on a DISTINCT aggregate whose transition function reads a NULL argument");
        }
    }
    Some(f)
}

/// pg_proc.proisstrict of a built-in aggregate's transition function, as the effects home answers it.
fn transfn_strict(r: &gravity_decompose::ops::shared::agg_class::AggRow) -> bool {
    gravity_decompose::effects::builtin_func(r.transfn_oid).is_some_and(|f| f.strict)
}

/// V4R-367: the non-strict transition functions that return the state unchanged on a NULL argument (PG 18.6's C):
/// sum(int2) / sum(int4)'s int2_sum / int4_sum, sum / avg(int8)'s int8_avg_accum.
const NULL_IGNORING: &[&str] = &["int2_sum", "int4_sum", "int8_avg_accum"];

/// One aggregate of a keyed group: its declaration, its class, its register shape, its arguments and its FILTER.
pub(crate) struct Agg<'p> {
    pub d: AggDecl<'p>,
    pub class: &'static dyn FoldClass,
    pub shape: AccShape,
    pub args: Vec<Arg>,
    pub filter: Option<ExId>,
}

/// A keyed group's record as its ends and its emit read it.
pub(crate) struct GroupRec<'g, 'p> {
    pub g: &'g GroupDecl<'p>,
    /// The lowering's facts (an ordered-set aggregate's direct argument asks its ORDER BY's family and effects).
    pub facts: &'g dyn gravity_decompose::catalog::Facts,
    pub node: i32,
    /// decompose's record (min / max(text) declared as images), rekeyed to the key image's words.
    pub layout: RecordLayout,
    /// The key image's record where `layout` is another's (a tagged pair, ops/group/pairs_multi.rs): `image_of`'s width.
    pub image: Option<RecordLayout>,
    /// Each key's image words (1, 2 for `Fp128`), and whether the image ends in the flags words (`null_flag`).
    pub wpk: PgVec<'g, u8>,
    /// Each key's collation (a nondeterministic one images by key.coll, D823).
    pub colls: PgVec<'g, KeyColl>,
    pub nullable: bool,
    pub tfs: PgVec<'g, TypeFacts>,
    pub reps: PgVec<'g, Option<RepSrc>>,
    /// Each key's source in the producer's row: its column, or its expression (a DISTINCT argument, resno 0; compiled by
    /// `compile_args`, `None` until then).
    pub ksrc: PgVec<'g, Option<Arg>>,
    /// The aggregates folding into record slots (a DISTINCT pair's `PairCount` / `PairFold` fold at the distinct emit).
    pub aggs: PgVec<'g, Agg<'p>>,
    /// The life holding the records: the group's producer, or a twopass.having split's pass 2 (keyed_twopass).
    pub home: LifeId,
    /// V4R-38: `Some` where the end folds num.window trees (fold/num_keyed.rs: `on`, `compile`, `step`; aggno, tree, wide).
    pub nums: Option<PgVec<'g, (i16, crate::ops::agg::numeric::strategies::num_tree::Tree<'g>, bool)>>,
    /// V4R-367: the pair aggregates' one FILTER (`pair_filter`), compiled by `compile_args`: a row it does not
    /// pass keys the DISTINCT argument as NULL (`key_vals`).
    pub pair_filter: Option<ExId>,
    /// The tagged form's FILTERs (ops/shared/agg.rs `tag_filter`), compiled by `compile_args` into the program arena:
    /// `(key, FILTER)`, a row it does not pass keys that argument as NULL (`key_vals`).
    pub key_filters: gravity_program::ArenaVec<(usize, ExId)>,
    /// V4R-375: the slot holding the aggregate arena's handle when the record holds hosted states (fold/hosted.rs
    /// `keyed_slots`), and that arena's parameter word once the end or the emit declared it (`arena`).
    pub handle: Option<u8>,
    pub arena: Option<u16>,
    /// D1520 (aggtrans T7b): the ordered-set direct arguments that are expressions over the keys, compiled for the group's
    /// row (keyed/stream.rs `direct_exprs`: aggno, argument index, expression, its cell).
    pub direct: PgVec<'g, (i16, usize, ExId, Cell)>,
    /// aggregates#251: an order-free call's ORDER BY entries that are not bare columns or constants (trans/args.rs
    /// `order_free`'s `evaluated`), compiled for the producer's row (aggno, expression): `discard` evaluates them per row.
    pub discard: PgVec<'g, (i32, ExId)>,
    /// D1371: the aggnos of the host-sorted aggregates folded ahead of each group's finals, in PG's transno order
    /// (effects `folds_ahead`; empty where no final could be seen before a later sorted aggregate's transitions).
    pub ahead: PgVec<'g, i16>,
    /// The licence the record's hosted aggregates fold ahead of PostgreSQL's group-by-group hand-on under
    /// (`keyed_slots`), `None` when none does.
    #[allow(dead_code)]
    pub fold_licence: Option<gravity_decompose::effects::Licence>,
    /// D3324 (b) (road key.dependent, ops/agg/strategies/dependent.rs): the text keys of a unique build key's row, whose
    /// record keeps the build row's code and whose image is 0 words while the build is unique; D3324 R3 (road key.carry):
    /// the text keys of one build row, whose record keeps its code and whose image is their own.
    pub dep: Option<crate::ops::agg::strategies::dependent::Dep>,
    /// textkeys step 3 (road key.canon): the keys (bit i: key i) whose record keeps no copy of an inline value, rendered
    /// from their canonical image at the emit (rep/varlena/key_canon.rs `bare_keys`); 0 off the road.
    pub bare: u64,
}

impl<'g, 'p> GroupRec<'g, 'p> {
    /// Group `g`'s record; `k` is a pipeline that reads it (its producer's shape is `g.producer`'s).
    pub fn new(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>) -> GroupRec<'g, 'p> {
        let (gx, node) = (px.gx, g.node_id);
        let mcx = px.mcx();
        let producer = g.producer.0 as usize;
        let mut rec = g.layout.record(px.arena);
        let (hosted, fold_licence) = crate::ops::agg::trans::hosted::keyed_slots(px, &mut rec, g, node); // V4R-375: after decompose's slots
        // V4R-367: its NULL; the tagged form's first argument under a FILTER flags it the same way
        let gk = g.keys.iter().position(|k| k.resno == 0);
        let tag0 = crate::ops::shared::agg::tagged_multi(g) && gk.is_some_and(|k| crate::ops::shared::agg::tag_filter(g, k).is_some());
        let pf = g.form == GroupForm::Distinct && (pair_filter(g, node).is_some() || tag0);
        let nullable = pf || g.keys.iter().any(|k| key_nullable(gx, producer, k));
        let kinds = crate::dict::coderep::key_kinds(gx, producer, &g.keys); // D823: a nondeterministic key is its sort-key Fp128
        let wpk: PgVec<'_, u8> = kinds.iter().map(|k| k.1).collect_in(mcx);
        let colls = g.keys.iter().map(|k| gx.facts.coll(k.coll)).collect_in(mcx);
        let mut layout = crate::props::pair_partition(gx, g, crate::ops::sort::topn::strategies::parted::parted_layout(px, g, crate::dict::coderep::image_layout(px.arena, &rec, &kinds, nullable, &crate::ops::sort::topn::order_slot::kinds(gx, g), rec.has_validity() || crate::ops::sort::topn::order_slot::wants_validity(gx, g, &rec, nullable), mcx))); // D563 L2
        // NEW-numeric-1: the rebuilds above (hosted, image, parted) start from fresh layouts; the keyed extremes' flag
        // goes back on, over a layout whose MinI64 / MaxI64 slots are theirs alone (decompose `settle_keyed`) but for
        // the top's order-expression slots after decompose's (order_slot.rs `kinds`): invalid until the emit, no merge
        // reads them
        if g.layout.ties {
            let own = rec.nslots() - usize::from(rec.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0);
            let order = own..own + crate::ops::sort::topn::order_slot::kinds(gx, g).len();
            let keyed = |j: usize| g.aggs.iter().any(|d| matches!(d.reg, Register::MinNumDyn | Register::MaxNumDyn) && d.slot as usize == j);
            if (0..layout.nslots()).any(|j| matches!(layout.slot_kind(j), SlotKind::MinI64 | SlotKind::MaxI64) && !keyed(j) && !order.contains(&j)) {
                bug(node, "an order-key layout (LAYOUT_KEY_TIES) with another MinI64 / MaxI64 slot");
            }
            layout.flags |= gravity_abi::breaker::LAYOUT_KEY_TIES;
        }
        let tfs = g.keys.iter().map(|k| crate::rep::hosted::user_op::tf(gx, node, g, k, crate::rep::hosted::hash::record_tf(gx.facts, node, crate::step::expr::tf_of(gx, k.tf), k))).collect_in(mcx); // hj-0310: a hashed key's user key
        let mut reps: PgVec<'_, _> = g.keys.iter().map(|k| rep_src(gx, producer, k, node)).collect_in(mcx);
        crate::dict::coderep::coarse_codes(gx, producer, node, &g.keys, &reps[..]); // D767
        // D3324 (b): a dependent key keeps its build row's code (not a scan lane's: no front, prefetch or coarse check reads it)
        let dep = crate::ops::agg::strategies::dependent::find(px, g);
        if let Some(d) = dep {
            crate::ops::agg::strategies::dependent::each(px, g, &d.u, |i, _, col, rep| {
                reps[i] = Some(RepSrc { col, attno: 0, typid: g.keys[i].tf, rep, derived: false, guard: false, datum: false, imaged: false, nocode: false });
            });
        }
        // D4854: a path key's representative reads its own path's drafted shred (`jsonb_keys::drafted`), else the group refuses
        for (k, r) in g.keys.iter().zip(reps.iter()) {
            let Some(RepSrc { rep: Rep::Code { dict }, nocode: true, .. }) = r else { continue };
            if crate::ops::scan::pgrc::strategies::jsonb_keys::drafted(px, producer, k, dict.lane) == Some(false) {
                shape(node, "a path key whose lane is not its path's drafted shred");
            }
        }
        // RD-key.codeif: a constant-arm key images from its hidden image column (keyed_emit_group `image_col`), its datum kept
        let ksrc = g.keys.iter().map(|k| crate::table::emit_group::image_col(gx, producer, k).or_else(|| key_col(gx, producer, k)).map(Arg::Col)).collect_in(mcx);
        // D495: only a Distinct group's argument key (resno 0) is never output; a returned float / numeric key keeps its
        // datum in its rep slot (M6 L0-m, keyed_emit_key `keep_datums`), a resno-0 one of another form has none
        for k in g.keys.iter().filter(|k| g.form != GroupForm::Distinct && k.resno == 0) {
            if k.image.needs_datum() {
                shape(node, "a float, numeric, uuid, interval or timetz group key (its image is not its output datum)"); // uuid / interval / timetz: M6-TYPES R1
            }
        }
        let nums = crate::ops::agg::numeric::strategies::num_keyed::on(px, g).then(|| PgVec::new_in(mcx));
        let aggs = g.aggs.iter().filter(|d| !matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold)).map(|d| {
            let mut d = *d;
            if let Some(first) = hosted.filter(|_| crate::ops::agg::trans::hosted::hosted_kind(&d)) {
                d.slot = crate::ops::agg::trans::hosted::keyed_slot(g, &d, first); // its AggState (the class reads `decl`'s NONE)
            }
            if d.slot == NONE && d.final_ == FinalForm::Distinct {
                // a DISTINCT aggregate outside the pair set (decompose keeps one; sum(DISTINCT n) over a numeric): M6's
                // keyed.distinct_multi, as keyed/distinct_plain.rs refuses it
                crate::not_built!(node, crate::admit::refusal::Variant::Kind(crate::face::kinds::KindId::KeyedDistinctMulti));
            }
            if d.slot == NONE {
                bug(node, "a keyed group's aggregate without a record slot");
            }
            let class = crate::ops::sort::topn::strategies::parted::class_in(&layout, &node, &d, nums.is_some(), mcx);
            crate::ops::agg::numeric::num_datum::guard(gx, node, class); // V4R-549: main 02:53Z's no-widen list over DATUM_DS
            Agg { d, class, shape: class.shape(&d), args: Vec::new(), filter: None }
        }).collect_in(mcx);
        let home = px.ext::<crate::ops::sort::topn::strategies::twopass::KeyedExt>(Face::Keyed).and_then(|e| e.home(g)).unwrap_or(g.producer);
        let handle = hosted.map(|_| (layout.slot_kinds().iter().rposition(|&k| k == SlotKind::AggState).expect("a hosted slot") + 1) as u8);
        let direct = crate::ops::group::stream::direct_exprs(px, g, node);
        let ahead = crate::effects::visible::group::folds_ahead(gx, g).into_iter().map(|i| g.aggs[i].aggno).collect_in(mcx);
        let bare = crate::rep::varlena::key_canon::bare_keys(px, g, &reps, &tfs, &colls, &wpk, home);
        GroupRec { g, facts: gx.facts, node, layout, image: None, wpk, colls, nullable, tfs, reps, ksrc, aggs, home, nums, pair_filter: None, key_filters: gravity_program::ArenaVec::new_in(px.arena), handle, arena: None, direct, discard: PgVec::new_in(mcx), ahead, fold_licence, dep, bare }
    }

    /// A pass 1's record (keyed_twopass): the key image and the row count alone, the named sentinel (slot 0, an
    /// unfiltered `count(*)`'s or the end's), no validity.
    /// `None` with an image wider than the admitted set's two words (a lone nullable Fp128's flags word aside: `fp_flags`).
    /// The group's own record may be wide (many aggregates): this one keeps none of them, so it is a few words always.
    pub fn counts(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>) -> Option<GroupRec<'g, 'p>> {
        let mut r = GroupRec::new(px, g);
        if r.layout.nkeys() > 2 + crate::ops::sort::topn::strategies::twopass::fp_flags(&r) as usize {
            return None;
        }
        let i = r.aggs.iter().position(|a| a.d.class == Class::CountStar && a.d.filter.is_none());
        let mcx = px.mcx();
        let mut aggs: PgVec<'g, Agg<'p>> = i.map(|i| r.aggs.swap_remove(i)).into_iter().collect_in(mcx);
        aggs.iter_mut().for_each(|a| a.d.slot = 0);
        let keys: PgVec<'_, KeyKind> = mcx::to_vec_in(&r.layout.key_kinds()[..], mcx);
        r.layout = RecordLayout::with_sentinel(&keys[..], &[SlotKind::Count], false, Some(0));
        r.reps.iter_mut().for_each(|x| *x = None);
        (r.aggs, r.home, r.dep) = (aggs, g.producer, None);
        Some(r)
    }

    /// A pass 1's record for a bound on another final than the row count (keyed_twopass under twopass.having): the key
    /// image, the group's unfiltered `count(*)` as the named sentinel (slot 0) when it has one, and the bounded aggregate
    /// (slot `slot` of the group's record) after it, or first before an implicit sentinel as decompose lays a group with
    /// no count; under the group's validity, so the final folds as in the group's own record. With the bounded final's
    /// slot in this record. `None` with an image wider than the admitted set's two words (as `counts`).
    pub fn bounded(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>, slot: u16) -> Option<(GroupRec<'g, 'p>, usize)> {
        let mut r = GroupRec::new(px, g);
        if r.layout.nkeys() > 2 + crate::ops::sort::topn::strategies::twopass::fp_flags(&r) as usize {
            return None;
        }
        let kind = r.layout.slot_kind(slot as usize);
        let mut b = r.aggs.remove(r.aggs.iter().position(|a| a.d.slot == slot)?);
        let c = r.aggs.iter().position(|a| a.d.class == Class::CountStar && a.d.filter.is_none()).map(|i| r.aggs.swap_remove(i));
        let at = c.is_some() as usize;
        b.d.slot = at as u16;
        let mcx = px.mcx();
        let mut aggs: PgVec<'g, Agg<'p>> = c.into_iter().map(|mut c| {
            c.d.slot = 0;
            c
        }).collect_in(mcx);
        aggs.push_in(b);
        let keys: PgVec<'_, KeyKind> = mcx::to_vec_in(&r.layout.key_kinds()[..], mcx);
        let slots: &[SlotKind] = if at == 1 { &[SlotKind::Count, kind] } else { &[kind] };
        r.layout = RecordLayout::with_sentinel(&keys[..], slots, r.layout.has_validity(), (at == 1).then_some(0));
        r.reps.iter_mut().for_each(|x| *x = None);
        (r.aggs, r.home, r.dep) = (aggs, g.producer, None);
        Some((r, at))
    }

    /// 5c-F6: a sum top's pass 1 record (topn twopass_count.rs): the key image, the leading sum (slot `slot` of the
    /// group's record) at slot 0 under the validity word (its NULL: bit 0 of the record's last word), with `abs` the
    /// scaled absolute sum of its argument at slot 1 (a float lead's: class_float.rs `AbsSum`), then the group's
    /// unfiltered `count(*)` as the named sentinel, or the implicit one in its place: either way the word after the
    /// lead's (and `abs`'s). `None` with an image wider than the admitted set's two words (as `counts`).
    pub fn led(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>, slot: u16, abs: bool) -> Option<GroupRec<'g, 'p>> {
        let abs = abs.then_some(&crate::ops::agg::trans::class_float::ABS_SUM as &'static dyn FoldClass);
        let mut r = GroupRec::new(px, g);
        if r.layout.nkeys() > 2 + crate::ops::sort::topn::strategies::twopass::fp_flags(&r) as usize {
            return None;
        }
        let kind = r.layout.slot_kind(slot as usize);
        let mut lead = r.aggs.remove(r.aggs.iter().position(|a| a.d.slot == slot)?);
        let c = r.aggs.iter().position(|a| a.d.class == Class::CountStar && a.d.filter.is_none()).map(|i| r.aggs.swap_remove(i));
        lead.d.slot = 0;
        let twin = abs.map(|class| {
            let d = AggDecl { slot: 1, trans: Trans::Abi(SlotKind::SumF64), ..lead.d };
            Agg { d, class, shape: class.shape(&d), args: Vec::new(), filter: None }
        });
        let mcx = px.mcx();
        let mut slots: PgVec<'_, SlotKind> = mcx::pg_vec![in mcx; kind];
        let mut aggs: PgVec<'g, Agg<'p>> = PgVec::new_in(mcx);
        aggs.push_in(lead);
        if let Some(t) = twin {
            slots.push_in(SlotKind::SumF64);
            aggs.push_in(t);
        }
        let sentinel = c.map(|mut c| {
            c.d.slot = slots.len() as u16;
            slots.push_in(SlotKind::Count);
            aggs.push_in(c);
            slots.len() - 1
        });
        let keys: PgVec<'_, KeyKind> = mcx::to_vec_in(&r.layout.key_kinds()[..], mcx);
        r.layout = RecordLayout::with_sentinel(&keys[..], &slots[..], true, sentinel);
        r.reps.iter_mut().for_each(|x| *x = None);
        (r.aggs, r.home, r.dep) = (aggs, g.producer, None);
        Some(r)
    }

    /// Table `t`'s image cursor (its AggState's arena words), where a min / max over codes copies its text (D277) and a
    /// varlena key keeps its output copy (M6 L0-l); `None` when none does.
    pub fn images(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &TableDecl) -> Option<Value> {
        // N13: a keyed numeric extreme's side copies take the cursor too
        let keyed = self.aggs.iter().any(|a| crate::ops::shared::agg::order_keyed(&a.d));
        let (copies, at) = (keyed || self.aggs.iter().any(|a| crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode) || (0..self.reps.len()).any(|i| crate::table::emit_key::copies(self, i)), core::mem::offset_of!(gravity_kernels::host_abi::AggState, arena_cur));
        copies.then(|| crate::table::bypass::state(b, env, t)).map(|st| b.ptr_add(st, env.zero, 1, at as i32))
    }

    /// V4R-375: the arena of pipeline `k` declared for a record holding hosted states (its word into `Env::agg_arena` at
    /// the row step and the finals; `None` without one).
    pub fn arena(&mut self, px: &mut Px<'_, '_>, k: usize) -> Option<u16> {
        self.arena = self.handle.map(|_| crate::ops::agg::trans::hosted::arena_word(px, k, self.g));
        self.arena
    }

    /// `a`'s declaration as its class reads it: a hosted state's slot is the record's (`keyed_slots`), not decompose's.
    pub fn decl(&self, a: &Agg<'p>) -> AggDecl<'p> {
        match self.layout.slot_kind(a.d.slot as usize) {
            SlotKind::AggState => AggDecl { slot: NONE, ..a.d },
            _ => a.d,
        }
    }

    /// Slot `slot` of the record at `rec` as an accumulator.
    pub fn slot(&self, rec: Value, slot: u16) -> Acc<'static> {
        Acc::Slot { base: rec, off: self.layout.slot_off(slot as usize) as i32, valid: crate::ops::shared::slots::slot_valid(&self.layout, slot) }
    }

    /// Key `k`'s image words `(first, n)`.
    pub fn key_words(&self, k: usize) -> (usize, usize) {
        (self.wpk[..k].iter().map(|&w| w as usize).sum(), self.wpk[k] as usize)
    }

    /// Sort word `w`, then (M6-TYPES R1b) the second image word when `w` sorts a uuid / interval / timetz key's first word:
    /// rep.fixed.hash's pair compares signed, word by word, in the type's order; the flags and NULL bit are `w`'s.
    pub fn sort_words(&self, w: u64) -> impl Iterator<Item = u64> {
        let fixed = |i: &usize| self.g.keys[*i].image.is_fixed() && self.wpk[*i] == 2;
        use gravity_kernels::host_abi::{sk_key, sk_key_word, SK_SLOT, SK_TEXT};
        let second = (0..self.wpk.len()).filter(fixed).find(|&i| w & (SK_SLOT | SK_TEXT) == 0 && sk_key_word(w) == self.key_words(i).0);
        // CAP-26: the second word's index in both of the word's index bytes
        core::iter::once(w).chain(second.map(|i| (w & !sk_key(usize::MAX & 0xffff)) | sk_key(self.key_words(i).0 + 1)))
    }

    /// The first flags word's index, when the image carries flags (one word per 64 keys after the image words; `null_flag`
    /// names key k's word and bit).
    pub fn flags_word(&self) -> Option<usize> {
        self.nullable.then(|| self.wpk.iter().map(|&w| w as usize).sum())
    }

    /// Key `k`'s NULL flag `(word, bit)` in the record, when the image carries flags: bit k % 64 of the flags word k / 64
    /// (`breaker::flag_at`, CAP-28).
    pub fn null_flag(&self, k: usize) -> Option<(usize, u32)> {
        let (w, bit) = gravity_abi::breaker::flag_at(k);
        self.flags_word().map(|f| (f + w, bit))
    }

    /// The record's flags words: one per 64 of the keys it images (a grouped DISTINCT view's argument, 0 words, is none of
    /// them), 0 without flags.
    pub fn flag_count(&self) -> usize {
        if self.nullable { gravity_abi::breaker::flag_words(self.wpk.iter().filter(|&&w| w != 0).count()) } else { 0 }
    }

    /// Whether some aggregate counts the rows into the record's sentinel (else the end does).
    pub fn sentinel_owned(&self) -> bool {
        self.aggs.iter().any(|a| self.layout.slot_off(a.d.slot as usize) == self.layout.sentinel_off())
    }

    /// Each key's cell in the producer's `row`, in key order, by value (an expression key evaluated: its NULL is `!valid`).
    pub fn key_vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> PgVec<'e, KeyVal> {
        // the filtered DISTINCT arguments: the pair aggregates' one FILTER on the argument key, or the tagged form's per key
        let filtered = || self.pair_filter.zip(self.g.keys.iter().position(|k| k.resno == 0)).into_iter().chain(self.key_filters.iter().map(|&(a, ex)| (ex, a)));
        let filter_of = |i: usize| filtered().find(|&(_, a)| a == i).map(|(ex, _)| ex);
        let mut out = mcx::reserved_in(self.g.keys.len(), env.mcx);
        for (i, s) in self.ksrc.iter().enumerate() {
            out.push_in(match (s, filter_of(i)) {
                (Some(Arg::Col(c)), _) => row.col(*c).map(|r| (*r.cell, r.val, r.null)).unwrap_or_else(|| bug(self.node, "a group key the row does not carry")),
                (Some(Arg::Key { id, cell } | Arg::Ex { id, cell }), Some(ex)) => self.filtered_key(b, env, row, ex, *id, *cell),
                (Some(Arg::Key { id, cell } | Arg::Ex { id, cell }), None) => crate::ops::group::distinct_plain::key_val(b, env, self.node, row, *id, *cell),
                (None, _) => bug(self.node, "a DISTINCT argument imaged before it was compiled"),
            });
        }
        // V4R-367: a DISTINCT argument reads as NULL where its FILTER is not true (NULL or false)
        for (ex, a) in filtered().filter(|&(_, a)| matches!(self.ksrc[a], Some(Arg::Col(_)))) {
            let (p, t, f) = (crate::step::expr::pred(b, env, self.node, row, ex), b.bconst(true), b.bconst(false));
            let rejected = b.int_bin(IntBinOp::Xor, p, t);
            let null = out[a].2.unwrap_or(f);
            out[a].2 = Some(b.int_bin(IntBinOp::Or, null, rejected));
        }
        out
    }

    /// G11: a computed DISTINCT argument under the pair FILTER `ex`, evaluated only where the FILTER is true (PG evaluates an
    /// aggregate's arguments only for the rows its FILTER passes) and NULL elsewhere.
    fn filtered_key(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, id: ExId, cell: Cell) -> KeyVal {
        let p = crate::step::expr::pred(b, env, self.node, row, ex);
        let (l_pass, l_skip) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(p, (l_pass, &[]), (l_skip, &[]));
        b.switch_to(l_pass);
        let (cell, val, null) = env.aside(|env| crate::ops::group::distinct_plain::key_val(b, env, self.node, row, id, cell));
        let (null, ty) = (null.unwrap_or_else(|| b.bconst(false)), b.value_type(val));
        let l_join = b.create_block(&[ty, Type::Bool]);
        b.br(l_join, &[val, null]);
        b.switch_to(l_skip);
        let (zero, t) = (b.const_raw(ty, 0), b.bconst(true));
        b.br(l_join, &[zero, t]);
        b.switch_to(l_join);
        let (val, null) = (b.block_params(l_join)[0], b.block_params(l_join)[1]);
        (cell, val, Some(null))
    }

    /// The key image of the producer's `row` in key order (a NOT NULL key contributes no flag; with the flags words every
    /// key contributes one, so the image's width is the record's).
    pub fn image(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> ImageWords {
        let keys = self.key_vals(b, env, row);
        self.image_of(b, env, &keys[..])
    }

    /// D3224 A4: the front index of the producer's `row` alone (keyimg.rs `codes`: each key's code as `image` reads it, no
    /// image word), where every key is a row column and the image takes no part word (`image_of`'s parted form); `None` otherwise
    /// and for any key not a code.
    pub fn front_codes(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<ImageWords> {
        // a numeric key's mantissa cell (`mantissa_cell`, `packed_cell`) images as a word, never as its code
        let cols = self.ksrc.iter().all(|s| matches!(s, Some(Arg::Col(_)))) && self.g.keys.iter().all(|k| !k.image.is_mantissa());
        if !cols || self.pair_filter.is_some() || !self.key_filters.is_empty() || self.layout.partition_keys() < self.layout.nkeys() {
            return None;
        }
        let keys = self.key_vals(b, env, row);
        let f = b.bconst(false);
        let mcx = env.mcx;
        let cells: PgVec<'_, KeyCell<'_>> = keys
            .iter()
            .zip(self.g.keys.iter())
            .zip(self.tfs.iter().zip(self.colls.iter()))
            .map(|(((cell, val, n), k), (tf, &coll))| {
                let c = CellRef { cell, val: *val, null: if self.nullable { Some(n.unwrap_or(f)) } else { None } };
                let scope = match (k.kind, c.cell.rep) {
                    (KeyKind::Code, Rep::Code { dict }) if coll.bytes_eq => Scope::SameDict(dict),
                    _ => Scope::Program,
                };
                KeyCell { cell: c, tf, scope, coll }
            })
            .collect_in(mcx);
        let mut cx = env.lx(b, self.node);
        let img = keyimg::codes(&mut cx, &cells[..]);
        env.memo = cx.memo;
        img
    }

    /// The image of the key cells `keys` (`key_vals`).
    pub fn image_of(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[KeyVal]) -> ImageWords {
        let f = b.bconst(false);
        let mcx = env.mcx;
        let refs: PgVec<'_, CellRef<'_>> = keys.iter().map(|(cell, val, n)| CellRef { cell, val: *val, null: if self.nullable { Some(n.unwrap_or(f)) } else { None } }).collect_in(mcx);
        let words: PgVec<'_, _> = refs.iter().enumerate().map(|(i, c)| self.mantissa_cell(b, env, i, *c)).collect_in(mcx);
        // D3324 (b): the dependent keys image as 0 words while the build is unique (dependent.rs `unique_now`)
        let skip = self.dep.as_ref().and_then(|d| crate::ops::agg::strategies::dependent::unique_now(d, b, env).map(|u| (u, d.mask)));
        // a parted record's image ends with its window's part ordinal, loaded after the image's flags words
        let part = (self.layout.partition_keys() < self.layout.nkeys() && !crate::props::pair_routed(self.g, &self.layout)).then(|| crate::ops::sort::topn::strategies::parted::part_at(env, self.node));
        let mut cx = env.lx(b, self.node);
        // M6-CLOSE (a'): a key over a packed lane images as its order word, an int8 cell (runs/copy_words.rs packed_cell)
        // D3857: not where the record keeps two words for the key (keyimg.rs `record_words`: a set side's `packed_set_key`,
        // D3848's scale-free key, or a cell key_cell_rep did not resolve as the packed lane it is): the cell images through
        // its hash arm, rep.packed.hash's Fp128, so the image's width follows the record's words (as `mantissa_cell`'s does)
        let packed: PgVec<'_, _> = refs.iter().zip(self.g.keys.iter()).zip(self.tfs.iter()).zip(words).enumerate().map(|(i, (((c, k), tf), w))| if self.wpk.get(i) == Some(&2) { w } else { crate::ops::sort::topn::copy_words::packed_cell(&mut cx, self.node, *c, k, tf).or(w) }).collect_in(mcx);
        let refs: PgVec<'_, CellRef<'_>> = refs.iter().zip(&packed).map(|(c, p)| p.as_ref().map_or(*c, |(cell, w)| CellRef { cell, val: *w, null: c.null })).collect_in(mcx);
        let int8 = gravity_decompose::catalog::typefacts::row_of(types_core::INT8OID).unwrap_or_else(|| bug(self.node, "no int8 type facts"));
        let cells: PgVec<'_, KeyCell<'_>> = refs
            .iter()
            .zip(self.g.keys.iter())
            .zip(self.tfs.iter().zip(self.colls.iter()).zip(&packed))
            .map(|((c, k), ((tf, &coll), p))| {
                let scope = match (k.kind, c.cell.rep) {
                    (KeyKind::Code, Rep::Code { dict }) if coll.bytes_eq => Scope::SameDict(dict),
                    _ => Scope::Program,
                };
                KeyCell { cell: *c, tf: if p.is_some() { int8 } else { tf }, scope, coll }
            })
            .collect_in(mcx);
        let img = keyimg::image_skip(&mut cx, &cells[..], skip, |b| part.map(|(ctx, off)| b.load_zext(Type::I32, ctx, off)));
        env.memo = cx.memo;
        if img.words.words.len() != self.image.unwrap_or(self.layout).nkeys() {
            bug(self.node, "a key image whose width is not its record's key words");
        }
        // D1391: per key, so two keys' wrong widths never cancel in the total (a pair view's argument key, 0 words, is not the image's)
        if img.widths.iter().zip(&self.wpk).any(|(&w, &e)| e != 0 && w != e) {
            bug(self.node, "a key image whose key's width is not its record's words for that key");
        }
        img
    }

    /// Key `i`'s cell as the record's one mantissa word, when the record keeps the key so (a narrow numeric decl, sized by
    /// cell.rs `key_cell_rep`'s packed lane) but the cell is another numeric: an int8 word cell holding `num_word::word` at the
    /// key's scale, the word a packed lane's order image is for the same value (decompose's form fixes every value's dscale
    /// at that scale, so the word is exact). The image's width so follows the record's words whichever cell reaches it, e.g.
    /// a hash probe's inner column, which the probe's projection evaluates as decompose's VarlenaPtr (step/project.rs
    /// forwards no INNER_VAR) while the record was sized by the build's packed lane (a GROUP BY key over a pgrc table's
    /// narrow numeric column, read through a hash join's probe). `None` for a packed cell (`packed_cell`'s) or any other key.
    fn mantissa_cell(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, i: usize, c: CellRef<'_>) -> Option<(Cell, Value)> {
        let (k, w) = (self.g.keys.get(i)?, *self.wpk.get(i)?);
        if !k.image.is_mantissa() || k.words != 1 || k.scale < 0 || w != 1 || matches!(c.cell.rep, Rep::PackedNum { .. }) {
            return None;
        }
        let (w, _, _) = crate::ops::agg::numeric::num_word::word(b, env, c, k.scale, false);
        let life = crate::cell::CellLife::of_lane(gravity_program::LaneLife::VALUE);
        Some((Cell { rep: Rep::Word { bytes: 8 }, typid: types_core::INT8OID, num: gravity_program::NumTypmod::NONE, life, at: crate::cell::At::Value(0), null: c.cell.null }, w))
    }

    /// The argument cells of `a` over `row`.
    pub fn args<'r, 'e>(&'r self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &'r RowVals, a: &'r Agg<'p>) -> PgVec<'e, CellRef<'r>> {
        let mcx = env.mcx;
        a.args
            .iter()
            .map(|arg| match arg {
                Arg::Col(c) => row.col(*c).unwrap_or_else(|| bug(self.node, "an aggregate argument the row does not carry")),
                Arg::Ex { id, cell } | Arg::Key { id, cell, .. } => ex_ref(b, env, self.node, row, *id, cell),
            })
            .collect_in(mcx)
    }

    /// aggregates#251: `a`'s ORDER BY entries in `discard` evaluated over `row` and dropped. PostgreSQL evaluates them for
    /// each row the call reads and sorts by them; an order-free class's answer never shows that order, so the entries'
    /// calls and raises are all that is left of them.
    fn discard(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, a: &Agg<'p>) {
        let no = a.d.node.as_aggref().map(|r| r.aggno);
        for &(_, id) in self.discard.iter().filter(|(n, _)| Some(*n) == no) {
            crate::step::expr::emit(b, env, self.node, row, id);
        }
    }

    /// D3224 A3: the sum whose slot `a`'s final reads (`FoldClass::shares`), as PostgreSQL shares the two's transition
    /// state: a sum of `a`'s `aggtransno` (the planner's find_compatible_trans: one transition function, equal arguments
    /// and FILTER) in this record, `a` over one bare column, both simple calls (no DISTINCT, ORDER BY or split) in record
    /// slots of their own (no hosted state), in a group whose emit finishes the record (`finals_of`: a hashed or sorted
    /// group, no pair set). Then `a` steps its count alone (`step`) and finishes over the sum's slot and its own. A pure
    /// function of the declarations: the producer's record and the emit's view decide alike.
    pub fn share(&self, a: &Agg<'p>) -> Option<&Agg<'p>> {
        if !matches!(self.g.form, GroupForm::Hashed | GroupForm::Sorted | GroupForm::Group) {
            return None;
        }
        if self.g.aggs.iter().any(|d| matches!(d.final_, FinalForm::Distinct | FinalForm::PairCount | FinalForm::PairFold)) {
            return None;
        }
        let tno = |d: &AggDecl<'p>| {
            let own = !matches!(self.layout.slot_kind(d.slot as usize), SlotKind::AggState);
            d.node.as_aggref().filter(|r| own && r.aggtransno >= 0 && r.aggsplit == 0 && r.aggdistinct.is_nil() && r.aggorder.is_nil()).map(|r| r.aggtransno)
        };
        let bare = a.d.node.as_aggref().is_some_and(|r| r.args.len() == 1 && r.args.nth(0).as_target_entry().is_some_and(|t| t.expr.as_var().is_some()));
        let t = tno(&a.d).filter(|_| bare)?;
        self.aggs.iter().find(|s| s.d.aggno != a.d.aggno && tno(&s.d) == Some(t) && s.class.shares(&s.d, &a.d) && a.class.shares(&s.d, &a.d))
    }

    /// One row's `args` into `a`'s `acc`: a sharing avg's count (`share`), else its class's step (a slot through the code
    /// extremes' `step_with`).
    #[allow(clippy::too_many_arguments)]
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &Agg<'p>, shared: bool, acc: Acc<'_>, args: &[CellRef<'_>], images: Option<Value>) {
        let d = self.decl(a);
        match (shared, acc) {
            (true, acc) => a.class.count(b, env, &d, acc, args),
            (false, acc @ Acc::Slot { .. }) => crate::dict::minmax_code::step_with(a.class, b, env, &d, acc, args, images),
            (false, acc) => a.class.step(b, env, &a.d, acc, args),
        }
    }

    /// `a`'s step over `row` into `acc`, under its FILTER (NULL or false skips the row, and its arguments are not read). An
    /// avg sharing its sum's slot (`share`, D3224 A3) steps its count alone.
    pub fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, a: &Agg<'p>, acc: AccAt<'_>) {
        let shared = self.share(a).is_some();
        let acc = if shared { acc } else {
            let Some(acc) = crate::ops::agg::numeric::strategies::num_keyed::step(self, b, env, row, a, acc) else { return };
            acc
        };
        let Some(ex) = a.filter else {
            self.discard(b, env, row, a);
            let args = self.args(b, env, row, a);
            match acc {
                AccAt::Slot(base, slot, images) => self.fold(b, env, a, shared, self.slot(base, slot), &args[..], images),
                AccAt::Regs(r) => self.fold(b, env, a, shared, Acc::Regs(r), &args[..], None),
            }
            return;
        };
        let p = crate::step::expr::pred(b, env, self.node, row, ex);
        let l_do = b.create_block(&[]);
        match acc {
            AccAt::Slot(base, slot, images) => {
                let l_join = b.create_block(&[]);
                b.cond_br(p, (l_do, &[]), (l_join, &[]));
                b.switch_to(l_do);
                env.aside(|env| {
                    self.discard(b, env, row, a);
                    let args = self.args(b, env, row, a);
                    self.fold(b, env, a, shared, self.slot(base, slot), &args[..], images)
                });
                b.br(l_join, &[]);
                b.switch_to(l_join);
            }
            AccAt::Regs(r) => {
                let types: PgVec<'_, Type> = r.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
                let l_join = b.create_block(&types[..]);
                b.cond_br(p, (l_do, &[]), (l_join, r));
                b.switch_to(l_do);
                let mut s: PgVec<'_, Value> = mcx::to_vec_in(&r[..], env.mcx);
                env.aside(|env| {
                    self.discard(b, env, row, a);
                    let args = self.args(b, env, row, a);
                    self.fold(b, env, a, shared, Acc::Regs(&mut s[..]), &args[..], None)
                });
                b.br(l_join, &s[..]);
                b.switch_to(l_join);
                r.copy_from_slice(b.block_params(l_join));
            }
        }
    }
}

/// Where a step writes: a group's record slot `(rec, slot)` and the table's image cursor (`GroupRec::images`), or the run's registers.
pub(crate) enum AccAt<'a> {
    Slot(Value, u16, Option<Value>),
    Regs(&'a mut [Value]),
}
