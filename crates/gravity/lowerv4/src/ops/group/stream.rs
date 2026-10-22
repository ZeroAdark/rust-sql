//! A streamed sorted group (D1071, V4R-379): kind `runs.group`'s step face (H22 `OrderUse::Stream`; runs/group.rs hands
//! `Op::GroupRun` here through keyed.hashagg's step maker), the form decompose gives a sorted GroupAggregate whose
//! aggregate arguments or FILTER call something VOLATILE (agg.rs `stream`). PostgreSQL's agg_retrieve_direct advances each
//! row into its group's transition states in the input's order and returns the group when the next group's first row
//! comes (that row is fetched, never advanced), so a LIMIT above stops the arguments at its last group. The step keeps the
//! one open group as the keyed form's record (keyed/hashagg_rec.rs `GroupRec`: the key image words, the representatives
//! and key copies, the aggregates' slots) in its frame words. A row whose key image differs from the record's hands the
//! group on first (keyed/emit_group.rs `record_row`: the keys by resno, the finals as the row's aggregate cells, which the
//! HAVING filter and the projection after the op read), then opens the record over the row and folds it; the input's end
//! hands the last group on (`StepMech::flush`). A stop inside the hand-on past the group marks it handed on, one at it
//! keeps it open: either way the resumed call re-enters the row. Serial (agg.rs makes the pipeline so); over an ordered
//! union's arms the words are the union's and only the last arm hands the open group on. A DISTINCT
//! aggregate's group (GroupForm::Distinct) keeps keyed.emit_group's pairs record and reads kind runs.group_distinct's set
//! (D1120, V4R-384: keyed/stream_distinct.rs; NotBuilt M6 by name until the kind registers). `finals_of` hands an ordered-set or hypothetical-set
//! aggregate its direct arguments over the group's row (D1520, V4R-537, aggtrans T7b: `direct`, `direct_exprs`). About 390 lines; no files.tsv row, so no budget gate.
//! Owner M3-GROUPED.
use gravity_abi::breaker::{KeyKind, SlotKind, LAYOUT_IMPLICIT_COUNT, MINCODE_NONE};
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::catalog::typefacts::row_of;
use gravity_decompose::Op;
use gravity_expr::emit::{AggCell, CellForm, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::agg::trans::hosted::{DIRECT_BOUND, DIRECT_NULL};
use mcx::{Mcx, PgBox, PgVec};
use mcx::GrowIn;
use types_nodes::primnodes::OUTER_VAR;

use crate::face::kinds::{Face, KindId};
use crate::face::runs::DistinctSetImpl;
use crate::face::step::StepMech;
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};
use mcx::CollectIn;

use crate::table::emit_key::KeyOut;
use crate::ops::agg::hashagg::{AccAt, Agg, GroupRec};

const RECORDOID: u32 = 2249;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::KeyedHashagg, &StepMaker(make))], ..crate::ops::NONE };

/// Frame words past the record: the open flag, then the key copies' arena cursor (`key_copy`: cursor, end, chunk).
const W_OPEN: u8 = 0;
const W_IMAGES: u8 = 1;
const W_RECORD: u8 = 4;

struct Stream<'g, 'p> {
    rec: GroupRec<'g, 'p>,
    keys: PgVec<'g, KeyOut<'g>>,
    node: i32,
    ncols: usize,
    /// The frame words' byte offsets: the open flag, the images cursor, the record.
    open: i32,
    images: i32,
    record: i32,
    /// Whether a key copy or a code extremum's text is kept (the images cursor is read).
    copies: bool,
    /// The step counts the group's rows into the record's sentinel (no aggregate does).
    count: bool,
    /// An ordered union's arm: its words are the union's (written by the first arm to open), and only the last arm hands
    /// the open group on at its input's end (`Scope::Union`'s flag word; `None` outside a union).
    union: Option<u16>,
    last: bool,
    /// D1120: a DISTINCT group's argument side (its set's words after the record's).
    distinct: Option<crate::ops::group::stream_distinct::Part<'g, 'p>>,
    /// D2719 / D2640: the keys under a user `=` over user code or another built-in, compared through it (`user_same`),
    /// never by their image word.
    user_eq: PgVec<'g, UserEq>,
    /// D2846: the group is one of decompose's `PipelineGraph::in_order` (its feed hands it PostgreSQL's row order): its
    /// folds that raise by their partial sums run in plan order (`facts::order::in_order_scope`).
    in_order: bool,
    /// D3539: the re-entry word (`StepMech::reentry`), over a join or set-returning function below the group (`again`).
    again: Option<i32>,
    /// PLANMEM: the lowering's planning context (`px.mcx()`): the scratch `lower`, `open_rec` and `hand_on` allocate.
    mcx: Mcx<'g>,
}

/// D2719 / D2640: a key under a user `=` whose procedure is not its type's own (decompose
/// `KeyDecl::user_eq`): its one image word (the key's datum, `user_eqs`), its NULL flag `(word, bit)` when the image
/// carries flags, and `proc | coll << 32` for the host (raw.rs `GroupEqHosted`: `proc` the equality's resolution id,
/// `coll` the group's collation for the key).
#[derive(Clone, Copy)]
struct UserEq {
    word: usize,
    null: Option<(usize, u32)>,
    pc: u64,
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

pub(crate) fn make<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::GroupRun(i) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a streamed group over another op" }) };
    let Some(g) = gx.stream(*i as usize) else { crate::refuse!(node, Reason::Bug { payload: "a streamed group without its declaration" }) };
    // D1120: a DISTINCT aggregate's per-group set (M6): refused by name until its file registers the set
    let set = (g.form == GroupForm::Distinct).then(|| crate::frame::iface::kind_impl::<DistinctSetImpl>(&node, KindId::RunsGroupDistinct).0);
    // an ordered union's arms (decompose append.rs copies the op into each): one open group across them, in arm order
    let union = px.union_of(k, j);
    let arm = union.map(|(_, marker)| match &p.ops[marker] {
        Op::Append(AppendDesc::Arm { i, n, ordered: true, prune: None }) => (*i, *n),
        Op::Append(AppendDesc::Arm { .. }) => shape(node, "run.stream: a streamed group over an unordered or pruned union's arms"),
        _ => crate::refuse!(node, Reason::Bug { payload: "a streamed union's marker that is no arm" }),
    });
    let own = g.producer.0 as usize == k;
    if (!own && arm.is_none()) || p.desc.is_eligible() {
        shape(node, "run.stream: a streamed group outside its own serial pipeline");
    }
    let node = g.node_id;
    crate::ops::group::stream_distinct::unpaired(g);
    let (mut rec, mut distinct) = match set {
        Some(set) => crate::ops::group::stream_distinct::records(px, g, (k, j), set),
        None => (GroupRec::new(px, g), None),
    };
    if !own && rec.reps.iter().flatten().any(|r| !r.datum) {
        // the declaration reads the last arm's relation: another arm's code lane is not its dictionary's
        shape(node, "run.stream: a code representative over an ordered union's arms");
    }
    if distinct.is_none() {
        rec.compile_args_at(px, false, (k, j));
    }
    rec.arena(px, k); // D1209: a hosted state's arena (V4R-375), the one keyed.hashagg's end declares; serial, no home
    // K4.riderg: a user-defined aggregate's transition and final are user code (they may act or raise): D439's pull cap, so
    // a cursor's FETCH n advances and finalizes no group past the n-th, as PG's GroupAggregate under its portal
    if crate::effects::visible::agg::folds_user(&rec) {
        px.declare_proto(gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Volatile, k as u16));
    }
    let keys = crate::table::emit_key::key_outs(px, node, g, &rec);
    let words = rec.layout.size().div_ceil(8) as usize + distinct.as_ref().map_or(0, |d| d.set.words() as usize);
    if W_RECORD as usize + words > u8::MAX as usize {
        shape(node, "run.stream: a group record wider than the step's frame words");
    }
    let key = |i: u8| match union {
        Some((union, marker)) => WordKey::Union { union, after: crate::frame::px::key_j(j - marker), i },
        None => WordKey::Open { face: Face::Step, k, j: crate::frame::px::key_j(j), i },
    };
    let base = px.frame_word(key(0));
    for w in 1..W_RECORD + words as u8 {
        if px.frame_word(key(w)) != base + w as u16 {
            crate::refuse!(node, Reason::Bug { payload: "a streamed group's frame words out of order" });
        }
    }
    let at = |w: u8| (base as i32 + w as i32) * 8;
    if let Some(d) = distinct.as_mut() {
        d.set_at = at(W_RECORD + rec.layout.size().div_ceil(8) as u8); // the set's words after the record's
    }
    let copies = rec.aggs.iter().any(|a| crate::props::class_kind(&a.d) == KindId::ClassMinmaxCode || crate::ops::shared::agg::order_keyed(&a.d)) || (0..rec.reps.len()).any(|i| crate::table::emit_key::copies(&rec, i)); // N13: a keyed numeric extreme's side copies
    let ncols = g.keys.iter().map(|kd| kd.resno.max(0) as usize).max().unwrap_or(0);
    let count = !rec.sentinel_owned() && distinct.is_none();
    let flag = union.map(|(union, _)| px.frame_word(WordKey::UnionFlag { union }));
    let last = arm.is_none_or(|(i, n)| i + 1 == n);
    let user_eq = user_eqs(px, node, &rec);
    let in_order = px.gx.graph.in_order.contains(&node);
    // D2922 (R1): a group folding its raising calls in plan order over an index merge (decompose facts/stream.rs
    // `merge_spine`) meets PostgreSQL's row order, ties across arms too, only in the merge's pull form, whose heap replays
    // PostgreSQL's (merge_pull_heap.rs); the breaker form's run merge takes ties in arm order
    if in_order && crate::props::Merge::of_reader(gx, k).is_some_and(|m| !crate::props::merge_pull(gx, &m)) {
        shape(node, "run.stream: a group folding its raising calls in plan order over a MergeAppend not in the pull form (D2922)");
    }
    let again = again(px, k, j);
    mcx::pg_box_dyn!(px.mcx(), Stream { rec, keys, node, ncols, open: at(W_OPEN), images: at(W_IMAGES), record: at(W_RECORD), copies, count, union: flag, last, distinct, user_eq, in_order, again, mcx: px.mcx() } => dyn StepMech + 'g)
}

/// D3539: a streamed group's re-entry word (`StepMech::reentry`) when an expanding op (a join, a set-returning function)
/// sits below it in pipeline `k`. A stop inside the hand-on leaves with the cursor at the row that closed the group, and
/// that row is not folded yet: the expanding step must hand the same row on again on the resumed call rather than
/// continue past it, as after a later expanding step that stopped inside (D688), or the row is lost to its group (a
/// cursor's FETCH, a PL/pgSQL FOR loop over a grouped merge join). Only in a pipeline made under its own identity: a nest
/// inner's group re-enters by its owner's walk (rerun/inner.rs `Held`, D1018) on the same step word.
fn again(px: &mut Px<'_, '_>, k: usize, j: usize) -> Option<i32> {
    let below = px.gx.pipelines()[k].ops[..j].iter().any(|o| matches!(o, Op::Join(_) | Op::Srf(_)));
    (below && px.ident(k) == k).then(|| crate::frame::stop::resume_word(px, crate::frame::stop::ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) }))
}

/// D2719 / D2640: the keys of `rec`'s group under a user `=` (user code, or a built-in other than the
/// type's own), each bound at lowering by KeySem (`keysem::eq::eq_call`: the pipeline runs on the leader, a user function
/// being no pool-safe builtin; a SQL / PL one's statements are served nested there, D3375 N7). The key's one image
/// word must be its datum (a by-value integer key: its word is the value, as a Datum carries it), since the open group
/// keeps no other copy of its first key; any other key refuses by name. D3624: a DISTINCT aggregate's group too: its record
/// is the pairs view (frame/pairs_view.rs), whose group keys keep the owner's image words and flags word, and its argument
/// set opens empty with each group the user `=` opens (`Part::open`), the argument read after the boundary, as
/// PostgreSQL's sorted Agg resets a DISTINCT call's input per group.
fn user_eqs<'g>(px: &Px<'g, '_>, node: i32, rec: &GroupRec<'_, '_>) -> PgVec<'g, UserEq> {
    let mut out = mcx::PgVec::new_in(px.mcx());
    for (i, kd) in rec.g.keys.iter().enumerate().filter(|(_, kd)| kd.user_eq != 0) {
        let tf = &rec.tfs[i];
        let (word, n) = rec.key_words(i);
        if n != 1 || !tf.byval || !matches!(tf.typlen, 2 | 4 | 8) || !kd.image.is_raw() || !matches!(kd.kind, KeyKind::I32 | KeyKind::I64) {
            shape(node, "run.stream: a group key under a user-code = whose image word is not its datum (D2719)");
        }
        let id = match gravity_fn::typesem::keysem::eq::eq_call(&crate::effects::EqReads(px.gx.catalog), kd.user_eq, tf.typid) {
            Ok(id) => id,
            Err(gravity_fn::typesem::keysem::eq::EqCallErr::NoProc) => shape(node, "run.stream: a group key's user = whose procedure the catalog does not give (D2719)"),
            Err(gravity_fn::typesem::keysem::eq::EqCallErr::Unbound(why)) => shape(node, why),
        };
        out.push_in(UserEq { word, null: rec.null_flag(i), pc: id as u64 | (kd.coll as u64) << 32 });
    }
    out
}

impl Stream<'_, '_> {
    fn words(&self, env: &Env) -> Value {
        env.words.unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "a streamed group without the frame words" }))
    }

    /// The record's and the images cursor's addresses.
    fn at(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Option<Value>) {
        let st = self.words(env);
        let rec = b.ptr_add(st, env.zero, 1, self.record);
        (rec, self.copies.then(|| b.ptr_add(st, env.zero, 1, self.images)))
    }

    /// D2719: `same` (the group is open and every other key word matches) and, key by key, the user's `=` over the open
    /// group's first key and the row's, as PostgreSQL's ExecBuildGroupingEqual: called only while the row may still be
    /// the group's, never for a NULL key (the flags words already matched, so both are NULL: equal, as EEOP_NOT_DISTINCT
    /// takes them); a NULL result is not equal (EEOP_QUAL), a raise leaves through `l_raise`.
    fn user_same(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, words: &[Value], mut same: Value) -> Value {
        if self.user_eq.is_empty() {
            return same;
        }
        let h = eq_host(b);
        for u in &self.user_eq {
            let r = b.load(Type::I64, rec, self.rec.layout.key_off(u.word) as i32);
            let when = match u.null {
                Some((fw, bit)) => {
                    let sh = b.iconst(Type::I64, bit as i64);
                    let f = b.int_bin(IntBinOp::Ushr, words[fw], sh);
                    let f = b.int_bin(IntBinOp::And, f, env.one);
                    let some = b.icmp(IntCond::Eq, f, env.zero);
                    b.int_bin(IntBinOp::And, same, some)
                }
                None => same,
            };
            let pc = b.const_raw(Type::I64, u.pc);
            let eq = eq_call(b, env, h, when, &[env.ctx, r, words[u.word], pc], env.l_raise);
            let eq = b.icmp(IntCond::Ne, eq, env.zero);
            same = b.int_bin(IntBinOp::And, same, eq);
        }
        same
    }

    /// The record opened over `row`: zeroed, its key image words, its implicit count and unset codes (abi `Rec::open`),
    /// each aggregate's state initialised and each representative written and marked (keyed.hashagg's fresh record).
    fn open_rec(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, rec: Value, images: Option<Value>, img: &[Value]) {
        let l = &self.rec.layout;
        for w in 0..l.size().div_ceil(8) as i32 {
            b.store(rec, 8 * w, env.zero);
        }
        for (i, &w) in img.iter().enumerate() {
            b.store(rec, l.key_off(i) as i32, w);
        }
        if l.flags & LAYOUT_IMPLICIT_COUNT != 0 {
            b.store(rec, l.sentinel_off() as i32, env.one);
        }
        if !l.has_validity() {
            let none = b.const_raw(Type::I64, MINCODE_NONE);
            let codes: PgVec<'_, usize> = match l.is_wide() {
                false => (0..8).filter(|j| l.mincode_bits >> j & 1 == 1).collect_in(self.mcx),
                true => (0..l.nslots()).filter(|&j| l.slot_kind(j) == SlotKind::MinCode).collect_in(self.mcx),
            };
            codes.into_iter().for_each(|j| b.store(rec, l.slot_off(j) as i32, none));
        }
        for a in &self.rec.aggs {
            a.class.init(b, env, &self.rec.decl(a), self.rec.slot(rec, a.d.slot)); // a hosted state's slot is the record's
        }
        let t = b.bconst(true);
        let reps = crate::table::emit_group::rep_words(&self.rec, b, env, row);
        let reps = crate::table::emit_key::keep_datums_in(&self.rec, b, env, row, t, &mut |_, _| images, reps);
        for (k, w) in self.rec.g.keys.iter().zip(reps).filter_map(|(k, w)| w.map(|w| (crate::frame::pairs_view::rep_slot(self.rec.g, k.rep), w))) {
            b.store(rec, l.slot_off(k as usize) as i32, w);
            let set = { let none = b.const_raw(Type::I64, MINCODE_NONE); b.icmp(IntCond::Ne, w, none) };
            crate::ops::shared::slots::mark(b, env, rec, crate::ops::shared::slots::slot_valid(l, k), set);
        }
        if let Some(d) = &self.distinct {
            let set = b.ptr_add(self.words(env), env.zero, 1, d.set_at);
            d.open(&self.rec, b, env, rec, set);
        }
    }

    /// The open group handed on as a row (its keys by resno, its finals as the row's aggregate cells) through `next`, at
    /// `(at, past)`: a stop inside leaves by `stop_in` with the cursor `resume`, the group marked handed on first when the
    /// stop is past it. The row's aggregate cells are the group's only while the steps after this op run.
    #[allow(clippy::too_many_arguments)]
    fn hand_on<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rec: Value, carry: Carry<'e>, (at, past, resume): (Value, Value, Value), again: Option<i32>, next: Next<'_, 'e>) -> Carry<'e> {
        let st = self.words(env);
        let aggs = env.aggs.take();
        let grow = crate::table::emit_group::record_row(&self.rec, &self.keys, self.node, self.ncols, b, env, rec, (carry, past, at));
        let l_seen = b.create_block(&[Type::I64, Type::I64, Type::I64]);
        b.mark_cold(l_seen);
        let stop_in = core::mem::replace(&mut env.x.l_stop_in, l_seen);
        let c = next(b, env, &grow);
        (env.x.l_stop_in, env.aggs) = (stop_in, aggs);
        let here = b.current();
        b.switch_to(l_seen);
        let p = mcx::to_vec_in(b.block_params(l_seen), self.mcx);
        let is_past = b.icmp(IntCond::Eq, p[1], past);
        let is_at = b.icmp(IntCond::Eq, p[1], at);
        let known = b.int_bin(IntBinOp::Or, is_past, is_at);
        let (l_known, l_bad, l_mark, l_on) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(known, (l_known, &[]), (l_bad, &[]));
        b.switch_to(l_bad);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        b.br(env.x.l_exit, &[uns]);
        b.switch_to(l_known);
        b.cond_br(is_past, (l_mark, &[]), (l_on, &[]));
        b.switch_to(l_mark);
        b.store(st, self.open, env.zero);
        b.br(l_on, &[]);
        b.switch_to(l_on);
        // D3539: the row that closed the group is handed on again on the resumed call (`Stream::again`)
        if let Some(w) = again {
            let one = b.iconst(Type::I64, 1);
            env.store_word(b, w, one);
        }
        b.br(stop_in, &[p[0], resume, p[2]]);
        b.switch_to(here);
        b.store(st, self.open, env.zero);
        c
    }
}

impl StepMech for Stream<'_, '_> {
    fn open_words(&self, w: &mut OpenWords) {
        let word = |what, off| match self.union {
            Some(flag) => OpenWord::union(what, off, Type::I64, 0, flag),
            None => OpenWord::open(what, Base::Words, off, Type::I64, 0),
        };
        w.declare(word("stream open", self.open));
        for i in 0..3 {
            w.declare(word("stream images", self.images + 8 * i));
        }
        for i in self.distinct.as_ref().map_or(0..0, |d| 0..d.set.words() as i32) {
            w.declare(word("stream distinct set", self.distinct.as_ref().map_or(0, |d| d.set_at) + 8 * i));
        }
        // D3539: the re-entry word starts clear at every open, as an expanding step's offset does
        if let Some(o) = self.again {
            w.declare(OpenWord::open("stream re-entry", Base::Words, o, Type::I64, 0));
        }
    }

    fn reentry(&self) -> Option<i32> {
        self.again
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let st = self.words(env);
        // D3539: the row is here: a re-entry it waited for is over
        if let Some(w) = self.again {
            env.store_word(b, w, env.zero);
        }
        // the caller's arena: an end or stream after this one in the pipeline folds its hosted aggregates through its own,
        // never this Agg's under its own aggno (D1264's rule for the finals, here for the hand-on and the rows after)
        let outer = env.agg_arena;
        if self.rec.arena.is_some() {
            env.agg_arena = self.rec.arena; // D1209: the hosted classes' init, step and finals call through it
        }
        let (rec, images) = self.at(b, env);
        let img = match &self.distinct {
            Some(d) => d.image(&self.rec, b, env, row), // D1120: the group keys alone (the argument is read after the hand-on)
            None => self.rec.image(b, env, row),
        };
        let words = &img.words.words[..];
        // the row starts a group when no group is open or a key image word differs from the record's
        let open = b.load(Type::I64, st, self.open);
        let is_open = b.icmp(IntCond::Ne, open, env.zero);
        let mut same = is_open;
        for (i, &w) in words.iter().enumerate().filter(|(i, _)| !self.user_eq.iter().any(|u| u.word == *i)) {
            let r = b.load(Type::I64, rec, self.rec.layout.key_off(i) as i32);
            let e = b.icmp(IntCond::Eq, r, w);
            same = b.int_bin(IntBinOp::And, same, e);
        }
        let same = self.user_same(b, env, rec, words, same);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let (l_fold, l_new, l_emit, l_open) = (b.create_block(&types[..]), b.create_block(&[]), b.create_block(&[]), b.create_block(&types[..]));
        b.cond_br(same, (l_fold, &row.carry.params()), (l_new, &[]));
        b.switch_to(l_new);
        b.cond_br(is_open, (l_emit, &[]), (l_open, &row.carry.params()));
        b.switch_to(l_emit);
        let mine = env.agg_arena;
        env.agg_arena = outer; // the hand-on's finals take the record's arena themselves (`finals_of`)
        let c = self.hand_on(b, env, rec, row.carry.copy_in(env.mcx), (row.at, row.next, row.at), self.again, next);
        env.agg_arena = mine;
        b.br(l_open, &c.params());
        b.switch_to(l_open);
        let c = Carry::from_params(b.block_params(l_open), env.mcx);
        self.open_rec(b, env, row, rec, images, words);
        b.store(st, self.open, env.one);
        b.br(l_fold, &c.params());
        b.switch_to(l_fold);
        let c = Carry::from_params(b.block_params(l_fold), env.mcx);
        // D2846: a feed in PostgreSQL's order folds in plan order (decompose PipelineGraph::in_order)
        gravity_decompose::facts::order::in_order_scope(self.in_order, || match &self.distinct {
            Some(d) => {
                let set = b.ptr_add(st, env.zero, 1, d.set_at);
                d.fold(&self.rec, b, env, row, rec, images, set);
            }
            None => self.rec.aggs.iter().for_each(|a| self.rec.step(b, env, row, a, AccAt::Slot(rec, a.d.slot, images))),
        });
        if self.count {
            let off = self.rec.layout.sentinel_off() as i32;
            let n = b.load(Type::I64, rec, off);
            let n = b.int_bin(IntBinOp::Add, n, env.one);
            b.store(rec, off, n);
        }
        env.agg_arena = outer;
        c
    }

    /// The input's end hands the open group on; an ordered union's arm before the last keeps it open for the next arm.
    fn flushes(&self) -> bool {
        self.last
    }

    /// The open group at the input's end handed on (the frame's flush exits keep the source's position).
    fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
        let st = self.words(env);
        let (rec, _) = self.at(b, env);
        let open = b.load(Type::I64, st, self.open);
        let is_open = b.icmp(IntCond::Ne, open, env.zero);
        let types: PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let (l_emit, l_join) = (b.create_block(&[]), b.create_block(&types[..]));
        b.cond_br(is_open, (l_emit, &[]), (l_join, &carry.params()));
        b.switch_to(l_emit);
        let c = self.hand_on(b, env, rec, carry, (env.zero, env.one, env.zero), None, next);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

/// Record `rec`'s finals in a stack area, 16 bytes each (the datum word, then its valid word), as the row's aggregate
/// cells by aggno; the record's address after them (`record_of`). The emit's, and a streamed group's (keyed/stream.rs).
/// `row` is the group's row (its keys by resno): an ordered-set aggregate's direct arguments over it (D1520, `direct`).
pub(crate) fn finals_of<'e>(r: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rec: Value, row: &RowVals) -> AggRow<'e> {
    let n = r.aggs.len() as u32;
    // D3024: a packed numeric final is read lazily (`packed`): its raw words in its entry, its valid word past the record's
    let packed = |a: &crate::table::rec::Agg<'_>| a.class.packed_num(&r.decl(a)).filter(|_| r.layout.slot_kind(a.d.slot as usize) != SlotKind::AggState && !a.class.deferred_code());
    let m = r.aggs.iter().filter(|a| packed(a).is_some()).count() as u32;
    let slot = b.declare_stack_slot(16 * n + 8 + 8 * m, 8);
    let area = b.stack_addr(slot);
    b.store(area, 16 * n as i32, rec);
    let mut tail = 16 * n + 8;
    let naggno = r.aggs.iter().map(|a| a.d.aggno.max(0) as usize + 1).max().unwrap_or(0);
    let mut cells = mcx::pg_vec![in env.mcx; AggCell::word(0, CellValid::Always); naggno];
    // D1264: the record's own arena for its finals, the caller's back after them (an end above the emit folds its hosted
    // aggregates through its own arena, never this Agg's under its own aggno)
    let outer = env.agg_arena;
    if r.arena.is_some() {
        env.agg_arena = r.arena;
    }
    // D1371: every host-sorted aggregate's sort and transitions before the group's first final, as finalize_aggregates
    for &n in r.ahead.iter() {
        if let Some(a) = r.aggs.iter().find(|a| a.d.aggno == n && r.layout.slot_kind(a.d.slot as usize) == SlotKind::AggState) {
            a.class.fold_ahead(b, env, &r.decl(a), r.slot(rec, a.d.slot));
        }
    }
    for (i, a) in r.aggs.iter().enumerate() {
        let off = 16 * i as u32;
        if let Some(p) = packed(a) {
            let crate::face::fold::Acc::Slot { base, off: at, valid } = r.slot(rec, a.d.slot) else { unreachable!("a record slot") };
            let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
            for w in 0..1 + p.sum as i32 {
                let x = b.load(Type::I64, base, at + 8 * w);
                b.store(area, off as i32 + 8 * w, x);
            }
            let ok = b.conv(ConvOp::Zext, Type::I64, ok);
            b.store(area, tail as i32, ok);
            cells[a.d.aggno as usize] = AggCell { form: CellForm::PackedNum { sum: p.sum, scale: p.scale, render: p.render }, off, valid: CellValid::NonZeroAt(tail) };
            tail += 8;
            continue;
        }
        // D3224 A3: an avg sharing its sum's slot (rec.rs `share`) finishes over that slot and its own count
        let (v, null) = match (r.share(a), r.layout.slot_kind(a.d.slot as usize)) {
            (Some(s), _) => a.class.finish_shared(b, env, &r.decl(a), r.slot(rec, s.d.slot), r.slot(rec, a.d.slot)),
            (None, kind) => match (kind, direct(r, b, env, a, row)) {
                (SlotKind::AggState, None) => a.class.finish(b, env, &r.decl(a), r.slot(rec, a.d.slot)), // V4R-375
                (SlotKind::AggState, Some(d)) => a.class.finish_direct(b, env, &r.decl(a), r.slot(rec, a.d.slot), &d[..]),
                _ => crate::ops::sort::topn::strategies::parted::finish(r, b, env, a, rec),
            },
        };
        b.store(area, off as i32, v);
        let t = b.bconst(true);
        let valid = b.int_bin(IntBinOp::Xor, null, t);
        let valid = b.conv(ConvOp::Zext, Type::I64, valid);
        b.store(area, off as i32 + 8, valid);
        cells[a.d.aggno as usize] = AggCell::word(off, CellValid::NonZeroAt(off + 8));
    }
    env.agg_arena = outer;
    AggRow { rec: area, cells }
}

/// D1520 (V4R-537): aggregate `a`'s direct arguments over the group's row `row`, each (datum word, how it is given: kernels
/// `DIRECT_*`), as finalize_aggregate evaluates aggdirectargs over the group's representative, right before its finalfn
/// (a grouping set's nulled column is a NULL constant here, decompose agg.rs `arm_node`); `None` when each is a non-NULL
/// constant (the host's bound one, V4R-425). Served: constants, bare group keys whose equal members are one image (the
/// row's member is PG's first row's), and expressions over such keys (`direct_exprs`, aggtrans T7b). A final that may
/// raise (an ordered-set fraction out of range) raises for the same groups as PG's: its value is the group's rows and its
/// direct arguments, both PG's, and every group PG finalizes is finalized here (a numeric sum's or a variance's final
/// raises the same way); a read-write final runs once per group (hosts m6_agg `MEMO`: an order pass, a top's drain, a
/// resumed row). A NULL or no-constant direct argument is always given, so the host's "finalized without its direct
/// argument" never runs.
fn direct<'e>(r: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, a: &Agg<'_>, row: &RowVals) -> Option<PgVec<'e, (Value, Value)>> {
    let ag = a.d.node.as_aggref()?;
    let args = &ag.aggdirectargs;
    if args.iter().all(|n| n.as_const().is_some_and(|c| !c.constisnull)) {
        return None;
    }
    let node = r.node;
    let mut out = mcx::reserved_in(args.len(), env.mcx);
    for (i, n) in args.iter().enumerate() {
        if let Some(c) = n.as_const() {
            out.push_in((env.zero, b.iconst(Type::I64, (if c.constisnull { DIRECT_NULL } else { DIRECT_BOUND }) as i64)));
            continue;
        }
        // C2: the final compares with the ORDER BY's operators: each orders by its type's default family
        // (facts::order::default_order) and that type's compare neither acts nor raises (effects::of_compare; a record: unknown)
        let quiet = ag.aggorder.iter().all(|s| s.as_sort_group_clause().is_some_and(|c| gravity_decompose::facts::order::default_order(r.facts, c.sortop))) && ag.aggargtypes.iter().all(|t| t != RECORDOID && row_of(t).is_some() && { let e = gravity_decompose::effects::of_compare(r.facts, t); !e.acts && !e.may_raise });
        if !quiet {
            shape(node, "an ordered-set aggregate whose direct argument is no constant, ordered by a comparison that may raise (D1520: a user operator or type, a record)");
        }
        let var = n.as_relabel_type().map_or(n, |x| x.arg).as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0);
        let key = var.and_then(|v| r.g.keys.iter().position(|k| k.resno == v.varattno));
        let cell = key.filter(|&k| r.g.keys[k].image.members_alike(r.colls[k].bytes_eq)).and_then(|k| row.col(r.g.keys[k].resno));
        let ex = || r.direct.iter().find(|x| x.0 == a.d.aggno && x.1 == i).map(|x| crate::table::rec::ex_ref(b, env, node, row, x.2, &x.3));
        let Some((c, tf)) = cell.or_else(ex).and_then(|c| row_of(c.cell.typid).map(|tf| (c, *tf))) else {
            shape(node, "an ordered-set aggregate whose direct argument is no constant and no group key of one image (D1520: an expression, a column the group does not key, a key whose equal members differ)");
        };
        out.push_in(crate::table::rec::direct_word(b, env, node, c, &tf));
    }
    Some(out)
}

/// D2719: the host that calls a key's `=` (raw.rs `GroupEqHosted`: `(ctx, a, b, proc | coll << 32)` -> 1 / 0 / 2 raised).
pub(crate) fn eq_host(b: &mut FunctionBuilder<'_>) -> HostFn {
    b.declare_host_once("group_eq_hosted", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<crate::ops::group::raw::GroupEqHosted> as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() })
}

/// D2719: the user `=`'s 1 / 0 over the two key Datums `args` carry when `when` holds, else 1 (the caller ands it into a
/// `same` already false, or both keys are NULL); its `2` (raised) leaves through `l_raise`.
pub(crate) fn eq_call(b: &mut FunctionBuilder<'_>, env: &mut Env, h: HostFn, when: Value, args: &[Value], l_raise: Block) -> Value {
    let (l_call, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(when, (l_call, &[]), (l_join, &[env.one]));
    b.switch_to(l_call);
    let r = b.call_host(h, args, Some(Type::I64)).expect("returns the equality");
    let two = b.iconst(Type::I64, 2);
    let (raised, l_ok) = (b.icmp(IntCond::Eq, r, two), b.create_block(&[]));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(raised, (l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.br(l_join, &[r]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// D1520 (aggtrans T7b): group `g`'s direct arguments that are expressions, compiled for `direct` (aggno, argument index,
/// expression, its cell): each whose every column is a group key of one image and that does not act (the effects home:
/// PG evaluates it once per group, a resumed final here may again). A constant or a bare key needs none; any other stays
/// `direct`'s refusal.
pub(crate) fn direct_exprs<'g, 'p>(px: &mut Px<'g, 'p>, g: &GroupDecl<'p>, node: i32) -> PgVec<'g, (i16, usize, crate::frame::px::ExId, crate::cell::Cell)> {
    use gravity_decompose::effects;
    let gx = px.gx;
    let one = |resno: i16| g.keys.iter().any(|k| k.resno == resno && k.image.members_alike(gx.facts.coll(k.coll).bytes_eq));
    let keyed = |n: types_nodes::Node<'p>| {
        gravity_decompose::expr::walk(n, &mut |x| x.as_var().is_none_or(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && one(v.varattno)))
    };
    let cx = effects::Cx::without_stmt(gx.facts, px.arena);
    let mut out = PgVec::new_in(px.mcx());
    for d in g.aggs.iter() {
        let Some(r) = d.node.as_aggref() else { continue };
        for (i, n) in r.aggdirectargs.iter().enumerate() {
            let bare = n.as_relabel_type().map_or(n, |x| x.arg).as_var().is_some(); // `direct` reads a key of one image itself
            if n.as_const().is_some() || bare || !keyed(n) || effects::of_expr(&cx, n, &effects::PLAIN).acts {
                continue;
            }
            let cell = crate::cell::Cell::of_expr(crate::step::project::out_of(gx, n, node), 0, 0, crate::cell::Nullable::Maybe);
            out.push_in((d.aggno, i, crate::step::expr::compile(px, n, None, node), cell));
        }
    }
    out
}
