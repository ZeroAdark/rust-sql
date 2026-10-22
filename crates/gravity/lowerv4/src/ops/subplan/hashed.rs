//! `nest.subplan`, hashed (B2a, D393; M8-plan §3.6): PG's `buildSubPlanHash` / `ExecHashSubPlan` over RT-2a's SubHash
//! object (exec v4/m8_sub.rs, V4R-365). A helper of nest/subplan.rs, which registers the rows. Owner M8-SUB.
//! - The set. decompose closes the SubPlan's plan into its own pipeline, ending in a Distinct breaker (its slot orders the
//!   build before the reader, which consumes it) after an `Op::SubPlan` Build, a pass-through step. The end sends each
//!   row's key column to `m8_subhash_insert` (a NULL key marks the set and is never stored). The finish seals the set,
//!   which publishes its KeyTable header at `SUBHASH_TABLE_AT`. The build runs at width 1, and the runtime's merge for
//!   it is a no-op: the set is the object, not the slot's region.
//! - The probe. The `Op::SubPlan` Hashed step, placed before its first reader, answers `lhs = ANY (set)` into the row's
//!   frame words as PG does. An empty set is false, and the left side is not evaluated. A NULL left side, or no match in
//!   a set holding a NULL, is NULL (false under unknownEqFalse). A match is true, and anything else is false. The walk is
//!   the set's own (gravity_lower::subhash: `slot_of`, linear probing, `width_mask`), with no host call per row. The step
//!   lends its row (B2c, site.rs `Lend::Row`) when it is conditional (D3451) or when its projection, filter or gate
//!   evaluates a VOLATILE or raising node before it (per_row.rs `order`, D1911): the reader probes where PG reaches it.
//! - PG builds the set at the first probe. A set proven raise-free with no VOLATILE call is built before the reader's
//!   pipeline (its slot orders it), unless its probe may never run (ahead.rs: LAZY and AHEAD, built at the first probe or
//!   ahead of its one reader on workers). Any other set is LAZY (RT-2c, V4R-369): its build and exclusive producers run
//!   at the first probe that finds no header (`m8_subhash_run`), so an outer that probes nothing sees no error and no call.
//! - Several key columns (RT-2d): the probe's row (its key words, then a null word per column: 1 NULL, 2 a cross-type
//!   value outside the key's type) goes to `m8_subhash_probe`, which answers as `ExecHashSubPlan` with its partial matches.
//!   An all-NULL probe first asks whether the set is empty, so an empty set's left side is never evaluated, as in PG.
//! - The key compares as typesem's `KeyUse::Hash` answer: a key whose datum is its value (`By::Word`) is one set word;
//!   a key compared by its canonical form (`By::Canonical`: text under a deterministic collation, numeric, uuid, ..) is
//!   two, the Fp128 of that form (`m6_hosted_fp`, exact, as the keyed tables' rep.hosted.hash), on both sides, and is
//!   probed through `m8_subhash_probe` (hashed_multi.rs). At most twenty set words (D3674: past eight the set's wide
//!   form, `m8::SUBHASH_WIDE`).
//! - D2598: a key typesem answers with `raise_hash` (text / bpchar under no collation: PG's hashtext raises 42P22 at
//!   the first non-NULL value it hashes) keeps its canonical form over the bytes, and its set is LAZY: the build raises
//!   PG's error at the first row PG hashes with that key not NULL (a row with no NULL key; under unknownEqFalse no
//!   other, since PG's set then has no partly-NULL table; otherwise any row, its non-NULL keys hashed into hashnulls).
//!   A probe never raises: PG hashes the left side only against a set holding a row with no NULL key, and its partial
//!   matches compare only keys neither side has NULL, each of which the build hashed first.
//! - D2849: an array key over such an element type (text[], varchar[], bpchar[] under no collation: PG's hash_array calls
//!   hashtext per element) keeps its canonical form (typesem `keysem::subplan::element_raise`), its one column only, and
//!   its set is LAZY. hash_array raises at the first non-NULL ELEMENT: the build raises at a row whose array holds one
//!   (`m6_raise_elems`), and a probe whose left array holds one raises when the set has a row with no NULL key (PG's
//!   `havehashrows`, `m8_subhash_exact`), where ExecHashSubPlan hashes it. A NULL array, an empty one or one of only NULL
//!   elements hashes quietly on either side.
//! - CNM-41: a string key under a nondeterministic collation (`By::Fmgr` only for that: its type's own equality and hash
//!   procs) is two set words, the Fp128 of the collation's sort key (`text_strnxfrm`, rep key.coll's exact image: PG's
//!   hashtext hashes that sort key, and two strings the collation calls equal have one), on both sides.
//! - D3676: a float4 or float8 key (`By::Canonical` of a by-value type, one type on both sides) is one set word on both
//!   sides, its word image (rep/word/hash.rs `float_image`: the value widened to float8, `-0` folded to `+0` and every NaN
//!   to one pattern, as float4eq / float8eq compare and hashfloat4 / hashfloat8 hash). D3682: so is a cross-type
//!   float48eq / float84eq key (typesem `keysem::subplan::float_cross`), each side widened from its own type.
//! - B2a (v0.4): any other one-type key PostgreSQL compares by its support functions (`By::Fmgr`: a user operator class
//!   over a by-value type or over procs other than the type's defaults, a built-in wrapper such as int4ne) keys by the
//!   operator's own procs through fmgr (`KeyCol::User`, keysem `subplan_user_key`; its own table when the equality is no
//!   equivalence). Refused by name: a cross-type or polymorphic key of that kind, any other canonical key of a
//!   by-value type (the host reads a by-reference value); under unknownEqFalse, a left side that may raise or call a
//!   VOLATILE function (`quiet_left`: PG's set drops its NULL rows, the set here keeps them).
use gravity_abi::status::raise;
use gravity_decompose::effects::{licence, Departure, Licence};
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_fn::typesem::{keysem, By, KeySem};
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::v4::m6_raise as h6;
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_lower::ops::subplan::keyset::{width_mask, HAS_NULL, MIX, NONEMPTY};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Parallel, Pipeline, Reason as Why};
use types_nodes::primnodes::BoolExprType;
use types_nodes::Node;
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::face::kinds::Face;
use crate::face::step::{ExprFace, StepMech};
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::{ExId, Px, WordKey};
use crate::graph::Gx;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::rerun::params::{NULL, SET};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// The KeyTable header's fields (gravity_lower::subhash::KeyTable, `repr(C)`).
const KEYS: i32 = 0;
const TAGS: i32 = 8;
const MASK: i32 = 16;
const FLAGS: i32 = 24;
/// `m8_raise`'s code for a probe that finds no sealed set (no PG error: a lowering bug, named by the runtime).
const NO_TABLE: u64 = 0xb2a;

/// The Build op of `plan_id` in pipeline `p`.
fn build_of<'a, 'p>(p: &'a PipelineNode<'p>, plan_id: Option<i32>) -> Option<&'a SubPlanDesc<'p>> {
    p.ops.iter().find_map(|op| match op {
        Op::SubPlan(d) if matches!(d.form, SubForm::Build) && plan_id.is_none_or(|id| id == d.plan_id) => Some(d),
        _ => None,
    })
}

/// The SubHash object's handle word for the build pipeline `kb`: declared by the first maker that asks (the build's end,
/// made before its reader's pipeline), the same word after; a key width the declaration does not hold refuses. `ahead`:
/// the one reader of a set that waits for its first probe though it may be built before it (ahead.rs), the proto's `peer`.
fn handle(px: &mut Px<'_, '_>, kb: usize, (widths, lazy, no_nulls): (&[u8], bool, bool), ahead: Option<usize>, node: i32) -> u16 {
    // unknownEqFalse: the set drops NULL-keyed rows (PG's no hashnulls), so a set of only those is empty
    let lz = if lazy { m8::SUBHASH_LAZY } else { 0 } | if no_nulls { m8::SUBHASH_NO_NULLS } else { 0 };
    let lz = lz | if ahead.is_some() { m8::SUBHASH_AHEAD } else { 0 };
    // D3674: past eight key words the wide form (exec subplan/multi.rs): the same two width bits per word from bit 24
    let lz = lz | if widths.len() > 8 { m8::SUBHASH_WIDE } else { 0 };
    let arg = match widths {
        [w] => 1 | u64::from(*w) << 8 | lz,
        _ => widths.iter().enumerate().fold(widths.len() as u64 | lz, |a, (i, w)| a | u64::from(w.trailing_zeros()) << (24 + 2 * i)),
    };
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::SubHash && p.pipeline as usize == kb) {
        if p.arg != arg {
            shape(node, "a hashed SubPlan whose probe and set disagree on the key's width");
        }
        return p.words.base as u16;
    }
    let h = px.proto_words(1);
    let proto = Proto::new(ProtoKind::SubHash, kb as u16).words(h, 1).arg(arg);
    px.declare_proto(ahead.map_or(proto, |r| proto.peer(r as u16)));
    h
}

/// D3506 SETLOCK (rerun/epq/set_lock.rs): the handle word of the twin of the set build pipeline `kb` fills: declared by
/// the first probe that asks (`ProtoKind::SubHashEpq`, `arg` the set's), the same word after. Exec lays a second set
/// beside the SubHash set of `kb`, whose LAZY build runs that set's members into it (subplan hash.rs `SubHashEpq`).
fn twin_handle(px: &mut Px<'_, '_>, kb: usize) -> u16 {
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::SubHashEpq && p.pipeline as usize == kb) {
        return p.words.base as u16;
    }
    let arg = px.protos.iter().find(|p| p.kind == ProtoKind::SubHash && p.pipeline as usize == kb).map_or(0, |p| p.arg);
    let t = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::SubHashEpq, kb as u16).words(t, 1).arg(arg));
    t
}

/// The SubHash object's address (the handle word, laid once per execution).
pub(crate) fn object(b: &mut FunctionBuilder<'_>, env: &Env, h: u16) -> Value {
    let w = b.load(Type::I64, env.params, h as i32 * 8);
    b.conv(ConvOp::IntToPtr, Type::Ptr, w)
}

pub(crate) fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// Leaves by the raise exit with the host's stashed error when `bad`.
pub(crate) fn raise_if(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

// ------------------------------------------------------------------------------------------------------------- the set

struct Build<'g> {
    node: i32,
    h: u16,
    /// Each key column's facts and how it compares: the row sent to `m8_subhash_insert` is their set words (one per
    /// `KeyCol::Word`, two per `KeyCol::Canon`), then a null word per set word.
    cols: PgVec<'g, (TypeFacts, KeyCol)>,
    /// A rerun set's member: every run of the build starts from an empty set.
    rebuild: bool,
    /// LAZY (built at the first probe): a rerun set's re-open re-arms it (exec subplan/raw.rs `rearm_sets`), not its build's open.
    lazy: bool,
    /// D2598: the key columns whose hash raises at a non-NULL value (`KeyCol::Canon { raise }`).
    raising: PgVec<'g, usize>,
    /// D2849: the one key column's raise is hash_array's, at a non-NULL element (`KeyCol::Canon { elems }`).
    elems: bool,
    /// unknownEqFalse: PG keeps no partly-NULL table, so it hashes only a row with no NULL key.
    no_nulls: bool,
    /// Construct C2: an AHEAD set's (ahead.rs `reader`) licence for its build at its one reader's start, ahead of that
    /// reader's first probe (`eager_build`'s); `None`: built at open or at the first probe. Held, not read.
    #[allow(dead_code)]
    ahead: Option<Licence>,
}

/// How one key column is held in the set (`test_of`): its datum word (`lw` the left side's width, `w` the key's), or
/// the two words of its canonical form's Fp128 (`form` canon.rs's content id).
#[derive(Clone, Copy)]
pub(crate) enum KeyCol {
    Word { lw: u8, w: u8 },
    /// `raise`: typesem's `raise_hash` (D2598): PG's hash of a non-NULL value raises 42P22. Typesem decides it; the set's
    /// build and probe read the field (no deciding function here: lint RV). `elems` (D2849, with `raise`): the raise is
    /// hash_array's, at a non-NULL element of the array (typesem `keysem::subplan::element_raise`).
    Canon { form: u64, raise: bool, elems: bool },
    /// CNM-41: a string under nondeterministic collation `coll`, its type's facts `tf` (the payload's header and bpchar trim).
    Coll { coll: u32, tf: TypeFacts },
    /// USEROP (kit K11.nin): a by-reference key whose `=` is user code, held as the Fp128 of its class under typesem user
    /// key `spec` (the host `m6_user_fp`: the type's hash proc and equality through fmgr, one representative per class).
    /// `unequal` (B2a): its equality is no equivalence or user code that may answer NULL, which a partial match across
    /// several columns calls pairwise (PG's execTuplesUnequal, raising XX000 "function %u returned NULL"): one column only.
    User { spec: u64, unequal: bool },
    /// regress subselect#162 / #163 / #166: a cross-type string equality (text = name, name = text) under a collation
    /// that compares by bytes, held as each side's canonical form's Fp128: `lform` the probe's left side, `rform` the set's
    /// key. Both forms' images are the string's bytes (text's payload, name's bytes up to its NUL), so equal strings
    /// share one image, as texteqname / nameeqtext compare them.
    Cross { lform: u64, rform: u64 },
    /// D3676: a float4 or float8 key, held as its exact word image (`float_key`): `l32` the probe's left side is a float4,
    /// `r32` the set's key (D3682: they differ under float48eq / float84eq).
    Float { l32: bool, r32: bool },
}

impl KeyCol {
    /// The set words' widths this column takes.
    pub(crate) fn widths(self) -> &'static [u8] {
        const W: [&[u8]; 9] = [&[], &[1], &[2], &[], &[4], &[], &[], &[], &[8]];
        match self {
            KeyCol::Word { w, .. } => W[w as usize],
            KeyCol::Float { .. } => &[8],
            KeyCol::Canon { .. } | KeyCol::Coll { .. } | KeyCol::User { .. } | KeyCol::Cross { .. } => &[8, 8],
        }
    }
}

/// D3676: a float4 (`f32`) or float8 datum word's set word, its exact image (rep/word/hash.rs `float_image`: equal words
/// are equal values under float4eq / float8eq, `-0 = 0` and every NaN equal).
pub(crate) fn float_key(b: &mut FunctionBuilder<'_>, v: Value, f32: bool) -> Value {
    let v = match b.value_type(v) {
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, v),
        Type::F32 => {
            let i = b.conv(ConvOp::Bitcast, Type::I32, v);
            b.conv(ConvOp::Zext, Type::I64, i)
        }
        _ => v,
    };
    crate::rep::word::hash::float_image(b, v, f32)
}

/// The Fp128 (lo, hi) of the by-reference value `v` of canon.rs form `form` (`m6_hosted_fp`, the image rep.hosted.hash
/// keys by), or (0, 0) when `valid` is false; a host error leaves by the raise exit.
pub(crate) fn fp(b: &mut FunctionBuilder<'_>, env: &Env, v: Value, valid: Value, form: u64) -> (Value, Value) {
    let (l_val, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(valid, (l_val, &[]), (l_out, &[env.zero, env.zero]));
    b.switch_to(l_val);
    let p = if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) };
    let s = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(s);
    let f = host(b, "m6_hosted_fp", &[Type::Ptr, Type::I64, Type::Ptr], crate::rep::hosted::hash::fp_entry());
    let form = b.iconst(Type::I64, form as i64);
    let st = b.call_host(f, &[p, form, out], Some(Type::I64)).unwrap_or(env.zero);
    let bad = b.icmp(IntCond::Ne, st, env.zero);
    raise_if(b, env, bad);
    let (lo, hi) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
    b.br(l_out, &[lo, hi]);
    b.switch_to(l_out);
    let o = b.block_params(l_out);
    (o[0], o[1])
}

/// USEROP: the Fp128 (lo, hi) of the class of the by-reference value `v` under typesem user key `spec` (`KeyCol::User`, the
/// image semantics' rep.hosted.hash `user_words` keys by, in the run's classes: D4197), or (0, 0) when `valid` is false; a
/// host error (the user's hash or equality raised) leaves by the raise exit.
pub(crate) fn user_fp(b: &mut FunctionBuilder<'_>, env: &Env, v: Value, valid: Value, spec: u64) -> (Value, Value) {
    let (l_val, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(valid, (l_val, &[]), (l_out, &[env.zero, env.zero]));
    b.switch_to(l_val);
    let p = if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) };
    let Some(keys) = crate::frame::env::user_keys(b, env.ctx) else { crate::refuse!(env.node, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
    let s = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(s);
    let entry = gravity_kernels::contract::entry4::<gravity_kernels::typesem::userfp::UserFp> as *const () as usize;
    let f = host(b, "m6_user_fp", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::Ptr], entry);
    let spec = b.iconst(Type::I64, spec as i64);
    let st = b.call_host(f, &[env.ctx, keys, p, spec, out], Some(Type::I64)).unwrap_or(env.zero);
    let bad = b.icmp(IntCond::Ne, st, env.zero);
    raise_if(b, env, bad);
    let (lo, hi) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
    b.br(l_out, &[lo, hi]);
    b.switch_to(l_out);
    let o = b.block_params(l_out);
    (o[0], o[1])
}

/// CNM-41: the Fp128 (lo, hi) of the collation sort key of the string `v` (`KeyCol::Coll`), or (0, 0) when `valid` is false;
/// a host error leaves by the raise exit.
pub(crate) fn coll_fp(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, (v, valid): (Value, Value), coll: u32, tf: &TypeFacts) -> (Value, Value) {
    use crate::rep::varlena::hash::{out2, payload, ptr, raise_on, release};
    let (l_val, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(valid, (l_val, &[]), (l_out, &[env.zero, env.zero]));
    b.switch_to(l_val);
    let mut cx = env.lx(b, node);
    let p = ptr(cx.b, v);
    let (bytes, len, mark) = payload(&mut cx, p, tf);
    let (out, oid) = (out2(cx.b), cx.b.iconst(Type::I64, i64::from(coll)));
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = cx.b.declare_host_once("text_strnxfrm", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), gravity_kernels::host_abi::text_strnxfrm as *const () as usize, flags);
    let st = cx.b.call_host(f, &[env.ctx, bytes, len, oid, out], Some(Type::I64)).unwrap_or(env.zero);
    raise_on(&mut cx, st);
    let (lo, hi) = (cx.b.load(Type::I64, out, 0), cx.b.load(Type::I64, out, 8));
    release(&mut cx, mark); // D2239: a detoasted copy goes back once the sort key's image is taken (rep key.coll's order)
    cx.b.br(l_out, &[lo, hi]);
    cx.b.switch_to(l_out);
    let o = cx.b.block_params(l_out);
    let r = (o[0], o[1]);
    env.memo = cx.memo;
    r
}

/// The licence a set is built EAGER under, at open (construct C2): `licence(.., Ahead)` granted outright (its plan neither
/// acts nor may raise, so no recheck). `None` builds it LAZY at its first probe (PG's form): denied (the set plan acts),
/// or granted with recheck (it may raise), since no eager road rechecks a raise yet.
pub(crate) fn eager_build(d: &SubPlanDesc<'_>) -> Option<Licence> {
    licence(d.effects(), Departure::Ahead).ok().filter(|l| !l.recheck())
}

/// A set built LAZY at its first probe (PG's form): `eager_build` holds no licence.
pub(crate) fn lazy_build(d: &SubPlanDesc<'_>) -> bool {
    eager_build(d).is_none()
}

/// The end of pipeline `k`, a hashed SubPlan's set (graph.rs `set_kind`).
pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(d) = build_of(p, None) else { bug(node, "a hashed SubPlan's set without its Build op") };
    // PG rebuilds the set when a rescan changed a parameter its plan reads (buildSubPlanHash under chgParam): a rerun
    // set's member runs again per re-open, so its build empties the set first (`m8_subhash_reset`). A LAZY set builds at
    // the first probe after each re-open: the re-open re-arms it (exec subplan/raw.rs `rearm_sets`: the set emptied, the build
    // run again at the next probe, never at the re-open), as PG's next ExecHashSubPlan rebuilds it.
    let rebuild = crate::lifetime::subplan::rebuilt(gx, k);
    let n = d.ids.iter().count();
    // D3674: PostgreSQL's TupleHashTable has no column limit; the set lays at most `SUBHASH_WIDE_MAX` key words (the
    // wide form past eight, exec subplan/multi.rs)
    if !(1..=m8::SUBHASH_WIDE_MAX).contains(&n) {
        shape(node, "a hashed SubPlan over more than twenty key columns");
    }
    let Some(list) = p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).filter(|l| l.iter().count() >= n) else {
        shape(node, "a hashed SubPlan whose set's row has no projection of its keys")
    };
    let mut widths: PgVec<'_, u8> = PgVec::new_in(mcx);
    let mut cols: PgVec<'_, (TypeFacts, KeyCol)> = PgVec::new_in(mcx);
    for ((e, t), id) in list.iter().take(n).zip(tests_of(gx, d, node)).zip(d.ids.iter()) {
        let e = e.as_target_entry().map_or(e, |te| te.expr);
        let typ = gravity_decompose::catalog::expr_type::type_of(e).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(e));
        let (_, col, kt) = test_of(gx, Some(t), id, node);
        if typ != kt {
            shape(node, "a hashed SubPlan whose set's key is not its test's right side (nest.subplan B2a)");
        }
        let Some(tf) = typefacts::facts_of(gx.facts, typ) else { shape(node, "a hashed SubPlan's key of a type the catalog does not know") };
        widths.extend_from_slice_in(col.widths());
        cols.push_in((tf, col));
    }
    if widths.len() > m8::SUBHASH_WIDE_MAX {
        shape(node, "a hashed SubPlan whose keys take more than twenty set words");
    }
    // D2849: an element raise keys one column only (a partial match across several columns compares by array_eq)
    let elems = cols.iter().any(|c| matches!(c.1, KeyCol::Canon { elems: true, .. }));
    if elems && cols.len() > 1 {
        shape(node, gravity_fn::typesem::INDETERMINATE);
    }
    if cols.len() > 1 && cols.iter().any(|c| matches!(c.1, KeyCol::User { unequal: true, .. })) {
        shape(node, UNEQUAL);
    }
    // D2598: a set whose build may raise PG's hashing error builds at its first probe, as PG's buildSubPlanHash does
    let raising: PgVec<'_, usize> = cols.iter().enumerate().filter(|c| matches!(c.1 .1, KeyCol::Canon { raise: true, .. })).map(|c| c.0).collect_in(mcx);
    // USEROP: a user key's hash and equality are user code that may raise: the set builds at its first probe, as PG's
    // buildSubPlanHash does (an empty outer calls neither)
    // W8-EQUIVCLASS-SORTOP (v0.4): a set whose plan holds a no-order-proc Sort raises at its build's first merge inner
    // fetch: it builds at its first probe, as PG's buildSubPlanHash runs at the first ExecHashSubPlan (an empty outer
    // never builds it)
    let lazy = lazy_build(d) || !raising.is_empty() || cols.iter().any(|c| matches!(c.1, KeyCol::User { .. })) || crate::ops::sort::sort_noproc::no_proc_set(gx, k);
    let ahead = if lazy { None } else { crate::ops::subplan::ahead::reader(gx, d.plan_id, k) };
    // construct C2 (b): a set built before its probe (EAGER at its open, or AHEAD at its one reader's start) runs under
    // `eager_build`'s licence, asked with the whole SubPlan's effects (its test, args and plan): the run-ahead tracer reads
    // it on the build and on what feeds it (the set plan's pipelines: the effects home's `producers`, transitively)
    if !lazy {
        if let Some(l) = eager_build(d) {
            let mut feeds = mcx::pg_vec![in mcx; k];
            let mut at = 0;
            while let Some(&q) = feeds.get(at) {
                at += 1;
                for x in crate::effects::producers(gx, q) {
                    if !feeds.contains(&x) {
                        feeds.push_in(x);
                    }
                }
            }
            for &q in &feeds {
                px.hold(q, &l);
            }
        }
    }
    let lazy = lazy || ahead.is_some();
    let h = handle(px, k, (&widths[..], lazy, d.unknown_eq_false), ahead.map(|a| a.0), node);
    mcx::pg_box_dyn!(px.mcx(), Build { node, h, cols, rebuild, lazy, raising, elems, no_nulls: d.unknown_eq_false, ahead: ahead.and_then(|a| a.1) } => dyn EndMech + 'g)
}

impl Build<'_> {
    /// D2598: PG's buildSubPlanHash hashes the row (all its keys not NULL, or, with a partly-NULL table, any row: its
    /// non-NULL keys) and hashtext raises 42P22 at a raising key that is not NULL. `m6_raise` stashes the error and the
    /// row leaves through `l_exit` with its status.
    fn raise_at_hash(&self, b: &mut FunctionBuilder<'_>, env: &Env, vals: &[(Value, Value)], x: &Exits) {
        let (t, f) = (b.bconst(true), b.bconst(false));
        // any raising key not NULL; under unknownEqFalse, and no key NULL (selects: the IR has no boolean or / and)
        let mut hit = f;
        for &i in &self.raising {
            hit = b.select(vals[i].1, hit, t);
        }
        if self.no_nulls {
            for &(_, null) in vals {
                hit = b.select(null, f, hit);
            }
        }
        let (l_raise, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_raise);
        b.cond_br(hit, (l_raise, &[]), (l_on, &[]));
        b.switch_to(l_raise);
        let h = b.declare_host_once("m6_raise", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<h6::M6Raise> as *const () as usize, HostFlags { may_error: true, ..Default::default() });
        let code = b.iconst(Type::I64, h6::RAISE_INDETERMINATE_HASH as i64);
        let st = b.call_host(h, &[env.ctx, code], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        b.cond_br(bad, (x.l_exit, &[st]), (l_on, &[]));
        b.switch_to(l_on);
    }
}

impl Build<'_> {
    /// D2849: PG's buildSubPlanHash hashes a row whose (one) key is not NULL, and hash_array raises 42P22 at its first
    /// non-NULL element (`m6_raise_elems` reads the array); a NULL key is never hashed. The row leaves through `l_exit` with
    /// the status.
    fn raise_elems_at_hash(&self, b: &mut FunctionBuilder<'_>, env: &Env, vals: &[(Value, Value)], x: &Exits) {
        let (d, null) = vals[0];
        let (l_call, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(null, (l_on, &[]), (l_call, &[]));
        b.switch_to(l_call);
        let st = crate::ops::shared::hash_raise::elems(b, env, d);
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        b.cond_br(bad, (x.l_exit, &[st]), (l_on, &[]));
        b.switch_to(l_on);
    }
}

impl EndMech for Build<'_> {
    /// A rerun member's build empties the set its previous run sealed (PG `buildSubPlanHash` resets its table); a LAZY
    /// one's was emptied when the set re-opened (`make_end`).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if !self.rebuild || self.lazy {
            return;
        }
        let o = object(b, env, self.h);
        let bad = crate::lifetime::subplan::reset_at_rebuild(b, env, o);
        raise_if(b, env, bad);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let mcx = env.mcx;
        let n = self.cols.iter().map(|c| c.1.widths().len()).sum::<usize>() as i32;
        let slot = b.declare_stack_slot(16 * n as u32, 8);
        let at = b.stack_addr(slot);
        let mut vals: PgVec<'_, (Value, Value)> = mcx::reserved_in(self.cols.len(), mcx);
        for (i, (tf, _)) in self.cols.iter().enumerate() {
            let Some(cr) = row.cell(i) else { bug(self.node, "a hashed SubPlan's key past the row") };
            let mut cx = env.lx(b, self.node);
            vals.push_in(rep::datum(&mut cx, cr, tf));
            env.memo = cx.memo;
        }
        if self.elems {
            self.raise_elems_at_hash(b, env, &vals[..], x);
        } else if !self.raising.is_empty() {
            self.raise_at_hash(b, env, &vals[..], x);
        }
        let mut wi = 0;
        for ((_, col), &(d, null)) in self.cols.iter().zip(&vals) {
            let words = match *col {
                KeyCol::Word { .. } => mcx::pg_vec![in mcx; d],
                KeyCol::Float { r32, .. } => mcx::pg_vec![in mcx; float_key(b, d, r32)],
                KeyCol::Canon { form, .. } => {
                    let (t, f) = (b.bconst(true), b.bconst(false));
                    let valid = b.select(null, f, t);
                    let (lo, hi) = fp(b, env, d, valid, form);
                    mcx::pg_vec![in mcx; lo, hi]
                }
                KeyCol::Coll { coll, tf } => {
                    let (t, f) = (b.bconst(true), b.bconst(false));
                    let valid = b.select(null, f, t);
                    let (lo, hi) = coll_fp(b, env, self.node, (d, valid), coll, &tf);
                    mcx::pg_vec![in mcx; lo, hi]
                }
                KeyCol::User { spec, .. } => {
                    let (t, f) = (b.bconst(true), b.bconst(false));
                    let valid = b.select(null, f, t);
                    let (lo, hi) = user_fp(b, env, d, valid, spec);
                    mcx::pg_vec![in mcx; lo, hi]
                }
                // the set holds the test's right side: its form
                KeyCol::Cross { rform, .. } => {
                    let (t, f) = (b.bconst(true), b.bconst(false));
                    let valid = b.select(null, f, t);
                    let (lo, hi) = fp(b, env, d, valid, rform);
                    mcx::pg_vec![in mcx; lo, hi]
                }
            };
            let nw = b.conv(ConvOp::Zext, Type::I64, null);
            for w in words {
                b.store(at, 8 * wi, w);
                b.store(at, 8 * (n + wi), nw);
                wi += 1;
            }
        }
        let o = object(b, env, self.h);
        let f = host(b, "m8_subhash_insert", &[Type::Ptr, Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_insert(ctx, h, row)));
        let r = b.call_host(f, &[env.ctx, o, at], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        raise_if(b, env, bad);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }

    /// The set sealed at the build's end: its header published for the probes.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        let o = object(b, env, self.h);
        let f = host(b, "m8_subhash_seal", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_seal(ctx, h)));
        let r = b.call_host(f, &[env.ctx, o], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        raise_if(b, env, bad);
    }

    /// The set is filled by one worker (the runtime refuses a wider build); its breaker slot only orders it. The set stays in
    /// memory as PostgreSQL's hashed SubPlan table does (C10: `PgHolds::HashedSubPlan`).
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        use gravity_program::spill::{PgHolds, Shape};
        match d.produces() {
            Some(_) => crate::frame::iface::end_holds(Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Host }, ..d }, Shape::PgHolds(PgHolds::HashedSubPlan)),
            None => bug(self.node, "a hashed SubPlan's set not ending in its breaker"),
        }
    }
}

/// The Build op: the set's rows go on to its end.
pub(crate) struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

// ----------------------------------------------------------------------------------------------------------- the probe

struct Probe {
    node: i32,
    plan_id: i32,
    h: u16,
    lhs: Lhs,
    ex: &'static ExprFace,
    wmask: u64,
    /// The left side's width when it differs from the set's (a cross-type integer equality).
    lwidth: Option<u8>,
    /// D3676: a float key (`Some(f32)`): the left value probes as its word image (`float_key`).
    float: Option<bool>,
    unknown_eq_false: bool,
    /// Where it is probed (site.rs), and a `Source` probe's entry.
    lend: crate::ops::subplan::site::Lend,
    early: crate::ops::subplan::site::Early,
    /// V4R-369: the set builds at the first probe that finds no header (`m8_subhash_run`).
    lazy_set: bool,
    /// Frame words (byte offsets): the value and its flags (params.rs `SET` / `NULL`).
    val: i32,
    flags: i32,
    /// W6-D1112: a check option's, host-fed (`Lhs::Fed`): evaluated at every read of the host's.
    fed: bool,
    /// D3506 SETLOCK (rerun/epq/set_lock.rs): the handle word of the set a lock's EvalPlanQual redo probes instead
    /// (`Env::epq_twin`), when the set's plan locks: exec subplan hash.rs `SubHashEpq`.
    twin: Option<u16>,
}

/// One equality per key column, in the order of the SubPlan's parameters: the test itself over one column; PG's AND of
/// `lhs_i = $id_i` over several (RT-2d). PG keys its table by every clause of the AND (ExecInitSubPlan's oplist), so a
/// clause repeated verbatim (an inlined SQL operator function's `$1::text = $2 and $1::text = $2`, regress subselect)
/// is one more key column holding the same values under the same operator: the set and each probe answer as with the
/// clause once. A repeat that calls nothing VOLATILE is dropped (PG evaluates its left side again; any other keeps it).
fn tests_of<'g, 'p>(gx: &Gx<'g, 'p>, d: &SubPlanDesc<'p>, node: i32) -> PgVec<'g, Node<'p>> {
    test_clauses(gx, d, node).into_iter().map(|t| t.0).collect_in(gx.mcx)
}

/// `tests_of` with each test's place among its test's clauses (an AND's arguments in order, else the test alone).
fn test_clauses<'g, 'p>(gx: &Gx<'g, 'p>, d: &SubPlanDesc<'p>, node: i32) -> PgVec<'g, (Node<'p>, usize)> {
    let n = d.ids.iter().count();
    let Some(t) = d.test else { shape(node, "a hashed SubPlan whose test is not one operator (nest.subplan B2a)") };
    let and = t.as_bool_expr().filter(|be| be.boolop == BoolExprType::AND_EXPR);
    if n > 1 && and.is_none() {
        shape(node, "a hashed SubPlan over several columns whose test is not an AND of equalities");
    }
    let mut tests: PgVec<'g, (Node<'p>, usize)> = PgVec::new_in(gx.mcx);
    for (i, c) in and.map_or_else(|| mcx::pg_vec![in gx.mcx; t], |be| be.args.iter().collect_in(gx.mcx)).into_iter().enumerate() {
        if crate::effects::volatile(gx, c) || !tests.iter().any(|&(x, _)| same_node(x, c)) {
            tests.push_in((c, i));
        }
    }
    if n == 1 && tests.len() != 1 {
        shape(node, "a hashed SubPlan whose test is not one operator (nest.subplan B2a)");
    }
    if tests.len() != n {
        shape(node, "a hashed SubPlan over several columns whose test is not one equality per column");
    }
    tests
}

/// A probe column's left side: compiled over the step's row, or (W6-D1112) a check option's, laid by the host over the
/// written row (write_check.rs `lay_lefts`, one pair per clause of the test) and stored by rowop_sub.rs `serve` in these
/// words. Its null word: 0 a value, 1 NULL, `RAISED` the host's evaluation raised (it keeps the error: PG's projLeft runs
/// only once the set proved non-empty, so the probe answers NULL past the empty test and the host raises it at the resume;
/// an empty set's FALSE drops it).
#[derive(Clone, Copy)]
pub(crate) enum Lhs {
    Ex(ExId),
    Fed { val: i32, null: i32 },
}

/// W6-D1112: a fed left side's null word when the host's evaluation raised (write_check.rs `RAISED`).
pub(crate) const RAISED: i64 = 2;

impl Lhs {
    /// W6-D1112: whether a fed left side's evaluation raised (`RAISED`); `None` for a compiled one.
    pub(crate) fn raised(self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Option<Value> {
        let Lhs::Fed { null, .. } = self else { return None };
        let (n, r) = (env.load_word(b, null), b.iconst(Type::I64, RAISED));
        Some(b.icmp(IntCond::Eq, n, r))
    }

    pub(crate) fn emit(self, ex: &ExprFace, b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals) -> (Value, Value) {
        match self {
            Lhs::Ex(id) => ex.0.emit(b, env, node, row, id),
            Lhs::Fed { val, null } => {
                let (v, n) = (env.load_word(b, val), env.load_word(b, null));
                (v, b.icmp(IntCond::Eq, n, env.zero))
            }
        }
    }
}

/// W6-D1112: the words of the left sides a check option's hashed SubPlan at op `j` of write pipeline `k` reads, a (value,
/// null) pair per clause of its test, in the clauses' order (rowop_sub.rs `fed` names the same words, `serve` stores the
/// host's pairs there).
fn fed_words<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, clauses: usize) -> PgVec<'g, (i32, i32)> {
    let mcx = px.mcx();
    let mut word = |i: usize| px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: (2 + i) as u8 }) as i32 * 8;
    (0..clauses).map(|c| (word(2 * c), word(2 * c + 1))).collect_in(mcx)
}

/// The clauses of a hashed SubPlan's test, in order: an AND's arguments, else the test alone (write_check.rs lays a left
/// side per clause in this order).
fn clause_count(d: &SubPlanDesc<'_>) -> usize {
    d.test.map_or(0, |t| t.as_bool_expr().filter(|be| be.boolop == BoolExprType::AND_EXPR).map_or(1, |be| be.args.iter().count()))
}

/// Whether `a` and `b` are one expression, field by field over the kinds a hashed test's clause holds (an operator, a
/// function call, a column, a parameter, a constant, a relabel or an I/O cast): any other kind is never the same.
fn same_node(a: Node<'_>, b: Node<'_>) -> bool {
    crate::stack_check();
    let all = |x: &types_nodes::list::NodeList<'_>, y: &types_nodes::list::NodeList<'_>| x.iter().count() == y.iter().count() && x.iter().zip(y.iter()).all(|(p, q)| same_node(p, q));
    if let (Some(x), Some(y)) = (a.as_op_expr(), b.as_op_expr()) {
        return (x.opno, x.opresulttype, x.opretset, x.opcollid, x.inputcollid) == (y.opno, y.opresulttype, y.opretset, y.opcollid, y.inputcollid) && all(&x.args, &y.args);
    }
    if let (Some(x), Some(y)) = (a.as_func_expr(), b.as_func_expr()) {
        return (x.funcid, x.funcresulttype, x.funcretset, x.funcvariadic, x.funccollid, x.inputcollid) == (y.funcid, y.funcresulttype, y.funcretset, y.funcvariadic, y.funccollid, y.inputcollid) && all(&x.args, &y.args);
    }
    if let (Some(x), Some(y)) = (a.as_var(), b.as_var()) {
        return (x.varno, x.varattno, x.vartype, x.vartypmod, x.varcollid, x.varlevelsup) == (y.varno, y.varattno, y.vartype, y.vartypmod, y.varcollid, y.varlevelsup) && x.varreturningtype == y.varreturningtype;
    }
    if let (Some(x), Some(y)) = (a.as_param(), b.as_param()) {
        return x.paramkind == y.paramkind && (x.paramid, x.paramtype, x.paramtypmod, x.paramcollid) == (y.paramid, y.paramtype, y.paramtypmod, y.paramcollid);
    }
    if let (Some(x), Some(y)) = (a.as_relabel_type(), b.as_relabel_type()) {
        return (x.resulttype, x.resulttypmod, x.resultcollid) == (y.resulttype, y.resulttypmod, y.resultcollid) && same_node(x.arg, y.arg);
    }
    if let (Some(x), Some(y)) = (a.as_coerce_via_io(), b.as_coerce_via_io()) {
        return (x.resulttype, x.resultcollid) == (y.resulttype, y.resultcollid) && same_node(x.arg, y.arg);
    }
    if let (Some(x), Some(y)) = (a.as_const(), b.as_const()) {
        // the value by decompose's fingerprint (its type, length, null and image bytes through the bake accessor)
        return x.constcollid == y.constcollid && matches!((gravity_decompose::catalog::expr_fp(a, 0), gravity_decompose::catalog::expr_fp(b, 0)), (Some(p), Some(q)) if p == q);
    }
    false
}

/// The left side of `test` (`lhs = $id`), how its key column is held in the set, and the key's type. How the key
/// compares is typesem's answer for the operator and its collation (`Catalog::hash_key`, `KeyUse::Hash`): a key whose
/// datum words are its value is one word (`By::Word`; a cross-type integer equality sign-extends the narrower side); a
/// key compared by its canonical form is that form's Fp128 (`By::Canonical`, one type on both sides, by reference); a key
/// compared by PostgreSQL's support functions is refused by name. A polymorphic operator (array_eq, record_eq, range_eq,
/// multirange_eq: typesem answers its pseudo-type inputs `By::Fmgr`) whose procs are built in under a deterministic
/// collation (`native`) and agree with its equality (`linear`: a built-in operator; a user operator over built-in procs is
/// not, D2642) is asked again for the one concrete type both sides hold (`concrete_type`, `Catalog::hash_key_typed`):
/// KeySem reads that type in the pseudo-type's place, so its form keys it (`By::Canonical`; gen ma-0079: `tags NOT IN
/// (SELECT ARRAY[id, weight] ..)`), or its word when the form is one by-value word whose `=` is its bits (`By::Word`: an
/// enum's enum_eq, kit K11.cen `e NOT IN (SELECT m.e ..)`), and a collatable one under no input collation is
/// `Indeterminate` (D2849: hash_array -> hashtext raises 42P22).
/// B2a: a several-column set over a `KeyCol::User { unequal: true }` key (refused by name).
const UNEQUAL: &str = "a hashed SubPlan of several keys whose user equality may answer NULL or is no equivalence (a partial match calls it pairwise: nest.subplan B2a)";
/// PostgreSQL's FirstNormalObjectId: a proc at or past it is user code (a built-in equality never answers NULL).
const FIRST_NORMAL_OID: u32 = 16384;

fn test_of<'p>(gx: &Gx<'_, 'p>, test: Option<Node<'p>>, id: i32, node: i32) -> (Node<'p>, KeyCol, u32) {
    let Some(o) = test.and_then(|t| t.as_op_expr()) else { shape(node, "a hashed SubPlan whose test is not one operator (nest.subplan B2a)") };
    let args: PgVec<'_, Node<'p>> = o.args.iter().collect_in(gx.mcx);
    let mut typed = None;
    let key = match gx.catalog.hash_key(o.opno, o.inputcollid) {
        Ok(Ok(KeySem { lt, rt, by: By::Fmgr, native: true, linear: true, raise_hash: false, procs, .. })) if lt == rt && gravity_decompose::catalog::pseudo_type(gx.facts, lt) => match concrete_type(gx, &args[..]) {
            Some(t) => {
                typed = Some((t, procs));
                gx.catalog.hash_key_typed(o.opno, o.inputcollid, t, t)
            }
            None => shape(node, "a hashed SubPlan's key compared by its support functions (nest.subplan B2a)"),
        },
        k => k,
    };
    // D2849: a collatable array under no collation whose element hash raises at a non-NULL element
    let elems = match (&key, typed) {
        (Ok(Err(gravity_fn::typesem::TypeClass::Indeterminate(_))), Some((t, procs))) => keysem::subplan::element_raise(t, o.inputcollid, &procs)
            .unwrap_or_else(|_| shape(node, "a hashed SubPlan's key operator the catalog cannot read (nest.subplan B2a)"))
            .map(|form| (t, form)),
        _ => None,
    };
    let (lt, rt, col) = match key {
        _ if elems.is_some() => {
            let Some((t, form)) = elems else { unreachable!("elems answered") };
            (t, t, KeyCol::Canon { form, raise: true, elems: true })
        }
        Ok(Ok(KeySem { raise_hash: true, by: By::Word { .. } | By::Fmgr, .. })) => shape(node, gravity_fn::typesem::INDETERMINATE),
        Ok(Ok(KeySem { lt, rt, by: By::Word { lw, rw, .. }, .. })) => (lt, rt, KeyCol::Word { lw, w: rw }),
        // D3676: a float key's word image (`float_key`)
        Ok(Ok(KeySem { lt, rt, by: By::Canonical(_), raise_hash: false, .. })) if lt == rt && float_type(gx, rt).is_some() => {
            let Some(f32) = float_type(gx, rt) else { unreachable!("float_type answered") };
            (lt, rt, KeyCol::Float { l32: f32, r32: f32 })
        }
        Ok(Ok(KeySem { lt, rt, by: By::Canonical(form), raise_hash, .. })) => {
            if typefacts::facts_of(gx.facts, rt).is_none_or(|tf| tf.byval) {
                shape(node, "a hashed SubPlan's canonical key of a by-value type (m6_hosted_fp reads a by-reference value)");
            }
            (lt, rt, KeyCol::Canon { form, raise: raise_hash, elems: false })
        }
        Ok(Ok(KeySem { lt, rt, coll, by: By::Fmgr, procs, .. })) if lt == rt && coll_key(gx, lt, coll, &procs).is_some() => {
            let Some(tf) = coll_key(gx, lt, coll, &procs) else { unreachable!("coll_key answered") };
            (lt, rt, KeyCol::Coll { coll, tf })
        }
        // D3682: float48eq / float84eq, each side its own float's word image (hashfloat4 widens to float8)
        Ok(Ok(KeySem { lt, rt, by: By::Fmgr, native: true, linear: true, raise_hash: false, procs, .. })) if keysem::subplan::float_cross(lt, rt, &procs).is_some() => {
            let Some((l32, r32)) = keysem::subplan::float_cross(lt, rt, &procs) else { unreachable!("float_cross answered") };
            (lt, rt, KeyCol::Float { l32, r32 })
        }
        Ok(Ok(KeySem { lt, rt, by: By::Fmgr, native: true, linear: true, raise_hash: false, .. })) if keysem::image::string_pair(lt, rt) => match keysem::subplan::cross_forms(lt, rt) {
            Ok(Some((lform, rform))) => (lt, rt, KeyCol::Cross { lform, rform }),
            Err(_) => shape(node, "a hashed SubPlan's key operator the catalog cannot read (nest.subplan B2a)"),
            Ok(None) => shape(node, "a hashed SubPlan's key compared by its support functions (nest.subplan B2a)"),
        },
        Ok(Ok(KeySem { lt, rt, coll, by: By::Fmgr, native: false, procs, .. })) if lt == rt && user_key(gx, lt, coll, &procs, node).is_some() => {
            let Some(spec) = user_key(gx, lt, coll, &procs, node) else { unreachable!("user_key answered") };
            keysem::rep::user_key_site();
            (lt, rt, KeyCol::User { spec, unequal: false })
        }
        // B2a: any other one-type key PostgreSQL compares by the operator's own support functions (a user operator class
        // over a by-value type, or one whose procs are not the type's defaults, a built-in wrapper that is no equivalence):
        // the operator's procs through fmgr (`keysem::user::subplan_user_key`), the site's own table where its equality
        // is no equivalence (the paramid: one per set column in the statement, the build's and the probe's alike)
        Ok(Ok(KeySem { lt, rt, by: By::Fmgr, raise_hash: false, linear, procs, .. })) if lt == rt && !gravity_decompose::catalog::pseudo_type(gx.facts, lt) => {
            match keysem::user::subplan_user_key(&gravity_fn::typesem::KeyReq::hash(o.opno, o.inputcollid), lt, (id as u32 as u64) << 1 | 1) {
                Ok(Ok(spec)) => {
                    keysem::rep::user_key_site();
                    (lt, rt, KeyCol::User { spec, unequal: !linear || procs.eq >= FIRST_NORMAL_OID })
                }
                Ok(Err(why)) => shape(node, why),
                Err(_) => shape(node, "a hashed SubPlan's key operator the catalog cannot read (nest.subplan B2a)"),
            }
        }
        Ok(Ok(_)) => shape(node, "a hashed SubPlan's key compared by its support functions (nest.subplan B2a)"),
        Ok(Err(class)) => shape(node, class.why()),
        Err(_) => shape(node, "a hashed SubPlan's key operator the catalog cannot read (nest.subplan B2a)"),
    };
    // the key's Param, bare or under a RelabelType to the operator's right type (varchar into text's `=`: the same datum)
    let rhs = args.get(1).and_then(|&a| match a.as_relabel_type() {
        Some(r) if r.resulttype == rt => r.arg.as_param(),
        Some(_) => None,
        None => a.as_param().filter(|p| p.paramtype == rt),
    });
    match (args.first(), rhs.filter(|p| p.paramid == id)) {
        (Some(&l), Some(p)) if args.len() == 2 && gravity_decompose::catalog::expr_type::type_of(l) == Some(lt) => (l, col, p.paramtype),
        _ => shape(node, "a hashed SubPlan's test that is not its left side against its key (nest.subplan B2a)"),
    }
}

/// `test_of`'s polymorphic operator: the one concrete type its test `lhs op $id` (`args`) compares, the left side's type
/// and the bare Param's (no pseudo-type), which KeySem reads in the operator's pseudo-type's place; else None (refused by
/// B2a's name).
fn concrete_type(gx: &Gx<'_, '_>, args: &[Node<'_>]) -> Option<u32> {
    let t = gravity_decompose::catalog::expr_type::type_of(*args.first()?)?;
    let p = args.get(1)?.as_param()?;
    (args.len() == 2 && p.paramtype == t && !gravity_decompose::catalog::pseudo_type(gx.facts, t)).then_some(t)
}

/// D3676: whether key type `t`, compared by its canonical form (`By::Canonical`), is a float (its facts' word image
/// `WordImage::Float`: float4, `Some(true)`, or float8): its set word is that image (`float_key`); else None.
fn float_type(gx: &Gx<'_, '_>, t: u32) -> Option<bool> {
    typefacts::facts_of(gx.facts, t).filter(|tf| tf.byval).and_then(|tf| tf.image.float32())
}

/// USEROP (kit K11.nin): the typesem user key of key type `t` when PostgreSQL's hashed SubPlan runs exactly its support
/// functions: a by-reference type (not a pseudo-type) whose user key names the procs the operator's own hash support
/// names under the type's collation (`keysem::subplan::user_set_key`); else None (refused by B2a's name).
fn user_key(gx: &Gx<'_, '_>, t: u32, coll: u32, procs: &gravity_fn::typesem::Procs, node: i32) -> Option<u64> {
    if gravity_decompose::catalog::pseudo_type(gx.facts, t) || typefacts::facts_of(gx.facts, t).is_none_or(|tf| tf.byval) {
        return None;
    }
    keysem::subplan::user_set_key(t, coll, procs).unwrap_or_else(|_| shape(node, "a hashed SubPlan's key operator the catalog cannot read (nest.subplan B2a)"))
}

/// CNM-41: a key typesem answers `By::Fmgr` only for its nondeterministic collation `coll`: a varlena string type (its
/// facts' text image) under its own equality and hash procs (the facts' `eq` / `hash`: texteq / hashtext, bpchareq /
/// hashbpchar), whose equal values are the equal sort keys PG's hash proc hashes. Its facts, else None.
fn coll_key(gx: &Gx<'_, '_>, t: u32, coll: u32, procs: &gravity_fn::typesem::Procs) -> Option<TypeFacts> {
    let tf = typefacts::facts_of(gx.facts, t)?;
    let text = tf.image.is_text() && tf.typlen == -1 && tf.collatable;
    let own = procs.eq == tf.eq && procs.rhs_eq == tf.eq && procs.lhs_hash == tf.hash && procs.rhs_hash == tf.hash;
    (text && own && coll != 0 && !gx.facts.coll(coll).bytes_eq).then_some(tf)
}

/// The probe step at op `j` of pipeline `k` (`Op::SubPlan` Hashed over `slot`).
pub(crate) fn make_probe<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, d: &SubPlanDesc<'p>, (slot, lend, fed, arm): (u8, crate::ops::subplan::site::Lend, bool, bool), node: i32) -> PgBox<'g, dyn StepMech + 'g> {
    match probe_of(px, k, j, d, (slot, lend, fed, arm), node) {
        Ok(p) => mcx::pg_box_dyn!(px.mcx(), p => dyn StepMech + 'g),
        Err(m) => m,
    }
}

/// D3506 SETLOCK (rerun/epq/set_lock.rs `redo_probes`): the one-word hashed probe at op `j` of pipeline `k` (a claim's
/// Sort producer) made again for its lock's EvalPlanQual redo, lent over the redo's row (`Lend::Row`): the step's words
/// and set handles, its twin's among them.
pub(crate) fn redo_probe<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32) -> (i32, PgBox<'g, dyn StepMech + 'g>) {
    let gx = px.gx;
    let Some(Op::SubPlan(d)) = gx.pipelines()[k].ops.get(j) else { bug(node, "a SETLOCK redo probe over another op") };
    let gravity_decompose::ops::shared::subplan::SubForm::Hashed { slot } = d.form else { bug(node, "a SETLOCK redo probe that is not hashed") };
    match probe_of(px, k, j, d, (slot, crate::ops::subplan::site::Lend::Row, false, false), node) {
        Ok(p) => (d.plan_id, mcx::pg_box_dyn!(px.mcx(), p => dyn StepMech + 'g)),
        Err(_) => shape(node, "a hashed SubPlan over several key words whose plan locks rows (D3506 SETLOCK: one key word)"),
    }
}

/// `make_probe`'s step: the one-word probe, else the several-word one (`Err`).
fn probe_of<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize, d: &SubPlanDesc<'p>, (slot, lend, fed, arm): (u8, crate::ops::subplan::site::Lend, bool, bool), node: i32) -> Result<Probe, PgBox<'g, dyn StepMech + 'g>> {
    let gx = px.gx;
    let mcx = px.mcx();
    if d.link != SubLink::Any {
        bug(node, "a hashed SubPlan whose link is not ANY");
    }
    let ids: PgVec<'_, i32> = d.ids.iter().collect_in(mcx);
    let fills = |q: &PipelineNode<'_>| q.desc.produces() == Some(slot) && build_of(q, Some(d.plan_id)).is_some();
    let Some(kb) = gx.pipelines().iter().position(fills) else { bug(node, "a hashed SubPlan's probe without its set's pipeline") };
    if !px.protos.iter().any(|p| p.kind == ProtoKind::SubHash && p.pipeline as usize == kb) {
        bug(node, "a hashed SubPlan's probe made before its set's end");
    }
    let lazy_set = build_of(&gx.pipelines()[kb], Some(d.plan_id)).is_some_and(lazy_build) || crate::ops::sort::sort_noproc::no_proc_set(gx, kb); // W8: as make_end
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    let (val, flags) = (px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: 0 }) as i32 * 8, px.frame_word(WordKey::Open { face: Face::Nest, k, j: crate::frame::px::key_j(j), i: 1 }) as i32 * 8);
    let clauses = test_clauses(gx, d, node);
    let one = (ids.len() == 1).then(|| test_of(gx, clauses.first().map(|c| c.0), ids[0], node));
    // W6-D1112: a check option's probe reads the left sides the host laid over the written row (PG's projLeft, evaluated
    // after the set proved non-empty: a VOLATILE or user call would show where PG never ran it; an error waits, `RAISED`)
    let fed = fed.then(|| fed_words(px, k, j, clause_count(d)));
    let lhs_of = |px: &mut Px<'g, 'p>, l: Node<'p>, c: usize| match &fed {
        Some(w) => {
            // one that can raise is laid too: the host keeps its error until the probe proves the set non-empty (`RAISED`);
            // a VOLATILE or user function's call (a notice, a write) would show where PG never ran it, and so would an
            // executor parameter's first read (an InitPlan the host's fetch would run ahead)
            let mut exec = false;
            gravity_decompose::expr::walk(l, &mut |n| {
                exec |= n.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC);
                !exec
            });
            if crate::effects::volatile(px.gx, l) || crate::effects::user_fn(px.gx, l) || exec {
                shape(node, "a check option's hashed SubPlan whose left side calls a VOLATILE or user function or reads an executor parameter (PG evaluates it only once the set is non-empty: D1112 row B)");
            }
            Lhs::Fed { val: w[c].0, null: w[c].1 }
        }
        // D3627: an ON CONFLICT DO UPDATE arm's probe (per_row.rs `conflict`, lent to the arm): its left side over the arm's
        // row (the existing row by attno, EXCLUDED as the inner), as the nested one's arguments
        None if arm => Lhs::Ex(ex.0.compile(px, l, None, node)),
        None => Lhs::Ex(ex.0.compile_at(px, (k, j), l, None, node)),
    };
    if !matches!(one, Some((_, KeyCol::Word { .. } | KeyCol::Float { .. }, _))) {
        // RT-2d: PG's AND of one equality per column, each `lhs_i = $id_i`; or one key of two set words
        let tests = match one {
            Some((l, col, _)) => mcx::pg_vec![in mcx; (l, col, clauses[0].1)],
            None => clauses.iter().zip(&ids).map(|(&(t, c), &id)| { let (l, col, _) = test_of(gx, Some(t), id, node); (l, col, c) }).collect_in(mcx),
        };
        let cols: PgVec<'g, (Lhs, KeyCol)> = tests.into_iter().map(|(l, col, c)| (lhs_of(px, l, c), col)).collect_in(mcx);
        if cols.len() > 1 && cols.iter().any(|c| matches!(c.1, KeyCol::Canon { elems: true, .. })) {
            shape(node, gravity_fn::typesem::INDETERMINATE); // D2849: as make_end
        }
        if cols.len() > 1 && cols.iter().any(|c| matches!(c.1, KeyCol::User { unequal: true, .. })) {
            shape(node, UNEQUAL); // B2a: as make_end
        }
        let widths: PgVec<'_, u8> = cols.iter().flat_map(|c| c.1.widths().iter().copied()).collect_in(mcx);
        let lazy_set = lazy_set || cols.iter().any(|c| matches!(c.1, KeyCol::Canon { raise: true, .. } | KeyCol::User { .. })); // D2598, USEROP: as make_end
        let ahead = if lazy_set { None } else { crate::ops::subplan::ahead::reader(gx, d.plan_id, kb) };
        let lazy_set = lazy_set || ahead.is_some();
        let h = handle(px, kb, (&widths[..], lazy_set, d.unknown_eq_false), ahead.map(|a| a.0), node);
        if crate::rerun::epq::set_lock::acting_set(gx, kb) {
            shape(node, "a hashed SubPlan over several key words whose plan locks rows (D3506 SETLOCK: one key word)");
        }
        if crate::rerun::epq::set_lock::write_twin(gx, k, kb) {
            shape(node, "a write rechecking over a hashed SubPlan of several key words whose plan acts (D3678: one key word)");
        }
        let (plan_id, unknown_eq_false) = (d.plan_id, d.unknown_eq_false);
        let fed = fed.is_some();
        return Err(mcx::pg_box_dyn!(mcx, crate::ops::subplan::hashed_multi::Multi { node, plan_id, h, cols, ex, unknown_eq_false, lend, early: Default::default(), lazy_set, val, flags, fed } => dyn StepMech + 'g));
    }
    let (lhs, lwidth, width, float) = match one {
        Some((l, KeyCol::Word { lw, w }, _)) => (l, lw, w, None),
        Some((l, KeyCol::Float { l32, .. }, _)) => (l, 8, 8, Some(l32)),
        _ => unreachable!("a one-word key"),
    };
    let ahead = if lazy_set { None } else { crate::ops::subplan::ahead::reader(gx, d.plan_id, kb) };
    let lazy_set = lazy_set || ahead.is_some();
    let h = handle(px, kb, (&[width], lazy_set, d.unknown_eq_false), ahead.map(|a| a.0), node);
    let lhs = lhs_of(px, lhs, clauses[0].1);
    let fed = fed.is_some();
    // D3506 SETLOCK: a set whose plan locks rows (rerun/epq/set_lock.rs) has a twin the claim's recheck probes; D3678:
    // so does a set whose plan acts that a write's recheck in the IR probes (set_lock.rs `write_twin`)
    let twin = ((crate::rerun::epq::set_lock::acting_set(gx, kb) || crate::rerun::epq::set_lock::write_twin(gx, k, kb)) && lazy_set).then(|| twin_handle(px, kb));
    Ok(Probe { node, plan_id: d.plan_id, h, lhs, ex, wmask: width_mask(width), lwidth: (lwidth != width).then_some(lwidth), float, unknown_eq_false: d.unknown_eq_false, lend, early: Default::default(), lazy_set, val, flags, fed, twin })
}

impl Probe {
    /// The verdict for the outer `row` into the frame words.
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) {
        crate::stack_check();
        let f = b.bconst(false);
        let unknown = b.bconst(!self.unknown_eq_false);
        let l_put = b.create_block(&[Type::I64, Type::Bool]);
        // D3506 SETLOCK: a lock's EvalPlanQual redo probes the set of its own (`twin`)
        let h = if env.epq_twin { self.twin.unwrap_or(self.h) } else { self.h };
        let o = object(b, env, h);
        let tab = b.load(Type::I64, o, m8::SUBHASH_TABLE_AT);
        let none = b.icmp(IntCond::Eq, tab, env.zero);
        let (l_none, l_tab) = (b.create_block(&[]), b.create_block(&[Type::I64]));
        b.mark_cold(l_none);
        b.cond_br(none, (l_none, &[]), (l_tab, &[tab]));
        b.switch_to(l_none);
        if self.lazy_set {
            // the first probe builds the set (PG's buildSubPlanHash), then reads the header it published
            let f = host(b, "m8_subhash_run", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_run(ctx, h)));
            let r = b.call_host(f, &[env.ctx, o], Some(Type::I64)).unwrap_or(env.zero);
            let bad = b.icmp(IntCond::Slt, r, env.zero);
            raise_if(b, env, bad);
            b.br(l_tab, &[r]);
        } else {
            let r = host(b, "m8_raise", &[Type::Ptr, Type::I64], m8::m8_raise as *const () as usize);
            let code = b.iconst(Type::I64, NO_TABLE as i64);
            b.call_host(r, &[env.ctx, code], Some(Type::I64));
            let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
            b.br(env.l_raise, &[e]);
        }
        b.switch_to(l_tab);
        let tab = b.block_params(l_tab)[0];
        let tab = b.conv(ConvOp::IntToPtr, Type::Ptr, tab);
        let fl = b.load(Type::I64, tab, FLAGS);
        // an empty set is false before the left side is evaluated
        let ne = b.iconst(Type::I64, NONEMPTY as i64);
        let ne = b.int_bin(IntBinOp::And, fl, ne);
        let empty = b.icmp(IntCond::Eq, ne, env.zero);
        let l_lhs = b.create_block(&[]);
        b.cond_br(empty, (l_put, &[env.zero, f]), (l_lhs, &[]));
        b.switch_to(l_lhs);
        if let Some(raised) = self.lhs.raised(b, env) {
            // the host raises its kept error at the resume (W6-D1112): NULL here, whatever unknownEqFalse says
            let (t, l_go) = (b.bconst(true), b.create_block(&[]));
            b.cond_br(raised, (l_put, &[env.zero, t]), (l_go, &[]));
            b.switch_to(l_go);
        }
        let (v, valid) = self.lhs.emit(self.ex, b, env, self.node, row);
        let l_walk = b.create_block(&[]);
        b.cond_br(valid, (l_walk, &[]), (l_put, &[env.zero, unknown]));
        b.switch_to(l_walk);
        let (keys, tags, mask) = (b.load(Type::Ptr, tab, KEYS), b.load(Type::Ptr, tab, TAGS), b.load(Type::I64, tab, MASK));
        let (l_miss, v) = (b.create_block(&[]), self.widen(b, v));
        let m = b.iconst(Type::I64, self.wmask as i64);
        let key = b.int_bin(IntBinOp::And, v, m);
        if self.lwidth.is_some_and(|lw| u32::from(lw) * 8 > self.wmask.count_ones()) {
            // a wider left side outside the key's range equals no key
            let bits = i64::from(self.wmask.count_ones());
            let (sh, l_fits) = (b.iconst(Type::I64, 64 - bits), b.create_block(&[]));
            let n = b.int_bin(IntBinOp::Shl, v, sh);
            let n = b.int_bin(IntBinOp::Sshr, n, sh);
            let fits = b.icmp(IntCond::Eq, n, v);
            b.cond_br(fits, (l_fits, &[]), (l_miss, &[]));
            b.switch_to(l_fits);
        }
        let mix = b.iconst(Type::I64, MIX as i64);
        let s = b.int_bin(IntBinOp::Mul, key, mix);
        let sh = b.iconst(Type::I64, 29);
        let s = b.int_bin(IntBinOp::Ushr, s, sh);
        let s = b.int_bin(IntBinOp::And, s, mask);
        // the walk ends at an empty slot (the set's load is at most one half); its header polls cancel, as every loop's
        let (l_loop, l_body) = cancel::header(b, env.ctx, env.l_cancel, &[Type::I64], env.mcx);
        let l_tag = b.create_block(&[]);
        b.br(l_loop, &[s]);
        b.switch_to(l_body);
        let at = b.block_params(l_body)[0];
        let tp = b.ptr_add(tags, at, 1, 0);
        let tag = b.load_zext(Type::I8, tp, 0);
        let free = b.icmp(IntCond::Eq, tag, env.zero);
        b.cond_br(free, (l_miss, &[]), (l_tag, &[]));
        b.switch_to(l_tag);
        let kp = b.ptr_add(keys, at, 8, 0);
        let got = b.load(Type::I64, kp, 0);
        let hit = b.icmp(IntCond::Eq, got, key);
        let next = b.int_bin(IntBinOp::Add, at, env.one);
        let next = b.int_bin(IntBinOp::And, next, mask);
        b.cond_br(hit, (l_put, &[env.one, f]), (l_loop, &[next]));
        // no match: NULL when the set holds a NULL (false under unknownEqFalse)
        b.switch_to(l_miss);
        let hn = b.iconst(Type::I64, HAS_NULL as i64);
        let hn = b.int_bin(IntBinOp::And, fl, hn);
        let has_null = b.icmp(IntCond::Ne, hn, env.zero);
        let null = b.int_bin(IntBinOp::And, has_null, unknown);
        b.br(l_put, &[env.zero, null]);
        b.switch_to(l_put);
        let (v, null) = (b.block_params(l_put)[0], b.block_params(l_put)[1]);
        env.store_word(b, self.val, v);
        let (is_null, not_null) = (b.iconst(Type::I64, SET | NULL), b.iconst(Type::I64, SET));
        let fw = b.select(null, is_null, not_null);
        env.store_word(b, self.flags, fw);
    }
}

impl Probe {
    /// The left value as a sign-extended integer when its type is not the key's (a cross-type integer equality), a float's word
    /// image (D3676, `float_key`), else as emitted.
    fn widen(&self, b: &mut FunctionBuilder<'_>, v: Value) -> Value {
        if let Some(f32) = self.float {
            return float_key(b, v, f32);
        }
        let Some(lw) = self.lwidth else { return v };
        let sh = b.iconst(Type::I64, 64 - i64::from(lw) * 8);
        let n = b.int_bin(IntBinOp::Shl, v, sh);
        b.int_bin(IntBinOp::Sshr, n, sh)
    }
}

impl crate::ops::subplan::per_row::Eval for Probe {
    fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) {
        crate::stack_check();
        Probe::eval(self, b, env, row)
    }

    fn fed(&self) -> bool {
        self.fed
    }
}

impl StepMech for Probe {
    fn prologue(&self, _: &mut FunctionBuilder<'_>, env: &mut Env, _: &crate::frame::stop::Exits) {
        crate::ops::subplan::site::prologue(self, (self.lend, &self.early), (self.plan_id, self.val, self.flags), env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        crate::ops::subplan::site::lend(self, self.lend, (self.plan_id, self.val, self.flags), b, env, row, next)
    }
}

#[cfg(test)]
mod tests {
    use super::float_key;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::Type;

    /// The set word `float_key` gives the datum word `d` of a float4 (`f32`) or float8.
    fn key(d: u64, f32: bool) -> u64 {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("k", &[Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let v = float_key(&mut b, p, f32);
        b.ret(&[v]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[d]).expect("runs")[0]
    }

    /// D3676: float4eq / float8eq's classes: `-0 = 0`, every NaN equal, a float4 keys as its float8 (hashfloat4)
    #[test]
    fn float_set_words_fold_negative_zero_and_nan() {
        let f8 = |x: f64| key(x.to_bits(), false);
        let f4 = |x: f32| key(u64::from(x.to_bits()), true);
        assert_eq!(f8(-0.0), f8(0.0));
        assert_eq!(f4(-0.0), f4(0.0));
        assert_eq!(f8(f64::from_bits(0xFFF8_0000_0000_0000)), f8(f64::NAN));
        assert_eq!(f8(f64::from_bits(0x7FF0_0000_0000_0001)), f8(f64::NAN));
        assert_eq!(f4(f32::from_bits(0xFFC0_0001)), f4(f32::NAN));
        assert_eq!(f4(1.5), f8(1.5));
        assert_ne!(f8(1.0), f8(-1.0));
        assert_ne!(f8(f64::INFINITY), f8(f64::NAN));
        assert_ne!(f4(f32::MIN_POSITIVE), f4(0.0));
    }
}
