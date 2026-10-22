//! Executor parameters (PARAM_EXEC) and the kinds that set them (M8 L0-w, V4R-344; M8-plan §1.3a, main's ruling (3)):
//! crate-level, so lib.rs, the M1 / M2 readers (step/consts.rs, step/limit.rs, the index / bitmap / tid / sample sources)
//! and M8's nest files share it. `supply` gives every parameter a producer of the graph sets its word before any maker
//! compiles (`WordPlan::nest`: its needs place `Place::Exec(id)`, never a bind word, InitPlans' included: their value is
//! written at run time, never at bind), so a reader refuses by its producer's kind, never by a word budget. Owner M8-NEST.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_lower::{Place, WordPlan};
use mcx::GrowIn;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::kinds::{Face, KindId};
use crate::frame::env::Env;
use crate::frame::px::{Px, WordKey};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};

/// Every executor parameter a producer of the graph sets, with the producer's kind: a non-CTE InitPlan's results (every
/// setParam, FACTS-1a: a ROWCOMPARE's or a MULTIEXPR's columns), a nest's NestLoopParam, a SubPlan's parParam, its
/// paramIds (the columns its test reads) and a MULTIEXPR's setParam, embedded inners included. A CTE's and a worktable's
/// parameters (M9's) are read by no expression and stay unset.
fn each(gx: &Gx<'_, '_>, f: &mut dyn FnMut(i32, KindId)) {
    for i in gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte)) {
        i.params.iter().for_each(|id| f(id, KindId::NestInitplan));
    }
    gx.pipelines().iter().for_each(|p| ops(p, f));
}

fn ops(p: &PipelineNode<'_>, f: &mut dyn FnMut(i32, KindId)) {
    crate::stack_check(); // D3832
    for op in p.ops.iter() {
        match op {
            Op::Join(JoinDesc::Nest { params, inner, .. }) => {
                params.iter().filter_map(|n| n.as_nest_loop_param()).for_each(|n| f(n.paramno, KindId::NestParam));
                ops(inner, f);
            }
            Op::SubPlan(s) => {
                s.params.iter().chain(s.sets.iter()).chain(s.ids.iter()).for_each(|id| f(id, KindId::NestSubplan));
                if let SubForm::Nested { inner } = &s.form {
                    ops(inner, f);
                }
            }
            _ => {}
        }
    }
}

/// Before the diag / serve split (lib.rs `lower_in`): each producer's parameter placed `Place::Exec` from now on.
pub(crate) fn supply<'p>(gx: &Gx<'_, 'p>, plan: &mut dyn WordPlan<'p>) {
    each(gx, &mut |id, _| plan.nest(id));
}

/// The kind that sets parameter `id` (`None`: no producer of the graph sets it).
pub fn producer(gx: &Gx<'_, '_>, id: i32) -> Option<KindId> {
    let mut k = None;
    each(gx, &mut |i, kind| {
        if i == id {
            k = k.or(Some(kind));
        }
    });
    k
}

/// D3008: whether parameter `id` is one a producer writes BEFORE it (re-)opens the plan that reads it: a nest's
/// NestLoopParam (set from each outer row, then the inner rescanned) or a SubPlan's argument (`parParam`, set before
/// each run of the subplan), embedded inners included. A scan inside that plan finds the word set at its every open
/// (`load`), as an index key at its (re-)open does. A SubPlan's output columns (`paramIds`), a MULTIEXPR's `setParam`
/// and an InitPlan's results are not: their readers run before or without a write.
pub fn set_before_open(gx: &Gx<'_, '_>, id: i32) -> bool {
    fn walk(p: &PipelineNode<'_>, id: i32) -> bool {
        crate::stack_check(); // D3832
        p.ops.iter().any(|op| match op {
            Op::Join(JoinDesc::Nest { params, inner, .. }) => params.iter().filter_map(|n| n.as_nest_loop_param()).any(|n| n.paramno == id) || walk(inner, id),
            Op::SubPlan(s) => s.params.iter().any(|p| p == id) || matches!(&s.form, SubForm::Nested { inner } if walk(inner, id)),
            _ => false,
        })
    }
    gx.pipelines().iter().any(|p| walk(p, id))
}

fn unset(node: i32) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy("an executor parameter no nest, SubPlan or InitPlan of the graph sets") })
}

/// An expression's need of parameter `id` (step/consts.rs, a key arm): its producer's name while that kind is unbuilt (once
/// built, the reader loads its words: `load`; an InitPlan's through `InitLoad`, which runs it at first use), a Shape when
/// no producer sets it.
pub fn need(gx: &Gx<'_, '_>, node: i32, id: i32) {
    match producer(gx, id) {
        None => unset(node),
        Some(k) if crate::ops::kind(k).is_none() => crate::not_built!(node, Variant::Kind(k)),
        Some(_) => {}
    }
}

/// M8 L0-c: the byte offset of parameter `id`'s frame word `i` in the program's words: 0 its value, 1 its flags (`SET`,
/// `NULL`). Frame words are zeroed per execution on the leader and per section on a worker, so 0 flags mean not set here.
pub const fn word(id: i32, i: u8) -> i32 {
    (2 * id + i as i32) * 8
}

/// A parameter's flags word: its producer set it in this context (an InitPlan's run, a nest's or SubPlan's write).
pub const SET: i64 = 1;
/// A parameter's flags word: its value is NULL.
pub const NULL: i64 = 2;
/// D3374: a SubPlan's flags word: its EXPR sublink made a second row it holds (ops/subplan/per_row.rs `fold`'s `held`), for
/// an interpreter island to raise where it reads it.
pub const HELD: i64 = 4;

/// M8 L0-a / L0-b: an InitPlan parameter's read where its value is due (an index key at its scan's (re-)open, an
/// expression's operand): its frame pair when `SET`, else the InitPlan run on the cold path (M8-SUB's nest.initplan
/// registers it): `(env, node, the InitPlan's handle word (`init_handle`), paramid) -> [value, valid (`Bool`), err (0:
/// none)]`.
pub struct InitLoad(pub for<'e, 'p> fn(&mut FunctionBuilder<'_>, &mut Env<'e, 'p>, i32, u16, i32) -> [Value; 3]);

/// Face `Nest`'s compile-time state (`Px::ext_mut`), its one type: `init` is each InitPlan's handle word, by its index in
/// the graph's InitPlans; `sites` each site number held for a nest's owner (pipes.rs `reserve_site`) or, D3493, for an index
/// arm's feed (inner_arms_index.rs `hold`) or, D4062, for an arm's remade inner (inner_arms.rs `hold_inners`). An M8 nest or SubPlan
/// mechanism with make-time state adds a field here.
#[derive(Default)]
pub struct NestExt {
    pub init: Vec<(usize, u16)>,
    pub sites: Vec<(usize, usize)>,
    /// D4062 (rerun/inner_arms.rs `hold_inners`): the inners whose feed's site number a union arm's remake took.
    pub arm_sites: Vec<usize>,
    /// D3549: the CTEs filled ahead (ops/ctescan/lower.rs `fill_set`), made at the first CTE's end.
    pub cte_fills: Option<Vec<usize>>,
}
crate::frame::px::ext_g!(NestExt);

/// The graph InitPlan that sets parameter `id` (its index): any of its setParams, so they share its handle word.
fn init_ix(gx: &Gx<'_, '_>, id: i32) -> Option<usize> {
    gx.graph.init_plans.iter().position(|i| !matches!(i.link, SubLink::Cte) && i.params.iter().any(|p| p == id))
}

/// The handle word of the InitPlan that sets parameter `id` (`Proto` Init's one kernel word, M8-RUNTIME's
/// `m8_init_run(ctx, h, paramid)`): made on the first call in a lowering, the same word after, so nest.initplan's end and
/// every reader agree before any emit.
pub fn init_handle(px: &mut Px<'_, '_>, id: i32) -> u16 {
    let Some(ix) = init_ix(px.gx, id) else { px.bug("an InitPlan handle for a parameter no InitPlan sets") };
    if let Some(h) = init_handle_of(px, id) {
        return h;
    }
    let h = px.proto_words(1);
    px.ext_mut::<NestExt>(Face::Nest).init.push((ix, h)); // alloc: Program
    h
}

/// `init_handle`'s word when made (a reader's lookup after its make called it).
pub(crate) fn init_handle_of(px: &Px<'_, '_>, id: i32) -> Option<u16> {
    let ix = init_ix(px.gx, id)?;
    px.ext::<NestExt>(Face::Nest)?.init.iter().find(|&&(i, _)| i == ix).map(|&(_, h)| h)
}

/// An InitPlan parameter's expression place (`Place::Exec`): bit 30, its handle word in bits 16..29, `id` in 0..15, so
/// its read has `(id, h)` from the place alone (`init_of`); `None` past those ranges.
pub const fn init_place(id: i32, h: u16) -> Option<i32> {
    if id < 0 || id >= 1 << 16 || h >= 1 << 14 {
        return None;
    }
    Some(1 << 30 | (h as i32) << 16 | id)
}

/// The refusal of an InitPlan parameter whose handle word sits past `init_place`'s 14-bit field (a statement of more than
/// 16384 words whose InitPlan is placed late; `limits::PARAM_WORDS` is `1 << 15`).
pub(crate) const INIT_HANDLE_PAST: &str = "an InitPlan's handle word past word 16383 (its tagged place's 14-bit field)";

/// `init_place`'s `(id, h)`; `None` for a plain parameter id.
pub const fn init_of(p: i32) -> Option<(i32, u16)> {
    if p & 1 << 30 == 0 {
        return None;
    }
    Some((p & 0xffff, ((p >> 16) & 0x3fff) as u16))
}

/// step/consts.rs `place`: an expression's need of parameter `id` placed `p` (`Place::Exec(id)`): `need`, then an
/// InitPlan's parameter tagged with its handle word (`init_place`).
pub fn place(px: &mut Px<'_, '_>, node: i32, id: i32, p: Place) -> Place {
    need(px.gx, node, id);
    match producer(px.gx, id) {
        Some(KindId::NestInitplan) => {
            let h = init_handle(px, id);
            match init_place(id, h) {
                Some(t) => Place::Exec(t),
                // the word plan's bound is wider than the handle's field: a handle made past word 16383 refuses by name
                None if h >= 1 << 14 => crate::refuse!(node, Reason::Shape { why: ShapeWhy(INIT_HANDLE_PAST) }),
                None => crate::refuse!(node, Reason::Bug { payload: "an InitPlan parameter past its tagged place's range" }),
            }
        }
        _ => p,
    }
}

/// Before any maker (frame/program.rs): each id up to the largest a producer sets takes its pair first, frame words
/// `2 id` and `2 id + 1`, so a reader finds a parameter's words by its id alone (`word`).
pub(crate) fn reserve(px: &mut Px<'_, '_>) {
    let mut last = -1;
    each(px.gx, &mut |id, _| last = last.max(id));
    for (id, i) in (0..=last).flat_map(|id| [(id, 0u8), (id, 1)]) {
        if px.frame_word(WordKey::Param { id, i }) as i32 * 8 != word(id, i) {
            crate::refuse!(px.gx.root_node(), Reason::Bug { payload: "an executor parameter's word numbered after another frame word" });
        }
    }
}

/// M8 L0-a: a source's key cell over parameter `id` (a bare parameter, placed `Place::Exec`), which the source reads at
/// every (re-)open (frame/site.rs `store_keys`): from its words, or an InitPlan's through `InitLoad` (`INIT_KEY`, its handle
/// word made here); `need`'s refusal while its producer is unbuilt, nest.initplan's while no `InitLoad` is registered.
pub fn exec_key(px: &mut Px<'_, '_>, node: i32, id: i32) -> u16 {
    use crate::frame::site::{EXEC_KEY, INIT_KEY};
    let init = producer(px.gx, id) == Some(KindId::NestInitplan);
    match init {
        true if crate::ops::kind_as::<InitLoad>(KindId::NestInitplan).is_none() => crate::not_built!(node, Variant::Kind(KindId::NestInitplan)),
        true => _ = init_handle(px, id),
        false => need(px.gx, node, id),
    }
    match u16::try_from(id).ok().filter(|&i| i < INIT_KEY) {
        Some(i) => EXEC_KEY | if init { INIT_KEY } else { 0 } | i,
        None => crate::refuse!(node, Reason::Bug { payload: "an executor parameter id past a key cell's range" }),
    }
}

fn words(env: &Env, node: i32) -> Value {
    env.words.unwrap_or_else(|| crate::refuse!(node, Reason::Bug { payload: "an executor parameter without the words site" }))
}

/// Parameter `id`'s value (`I64`) and NULL flag (`Bool`), as its producer last wrote them (a nest's or a SubPlan's; an
/// InitPlan's is read through `InitLoad`, its tagged place (`init_place`) read here as its pair).
pub fn load(b: &mut FunctionBuilder<'_>, env: &Env, node: i32, id: i32) -> (Value, Value) {
    let id = init_of(id).map_or(id, |(id, _)| id);
    let w = words(env, node);
    let flags = b.load(Type::I64, w, word(id, 1));
    let n = b.iconst(Type::I64, NULL);
    let null = b.int_bin(IntBinOp::And, flags, n);
    (b.load(Type::I64, w, word(id, 0)), b.icmp(IntCond::Ne, null, env.zero))
}

/// D3008: whether parameter `id`'s producer set it in this context (its flags' `SET`), a `Bool`.
pub fn is_set(b: &mut FunctionBuilder<'_>, env: &Env, node: i32, id: i32) -> Value {
    let id = init_of(id).map_or(id, |(id, _)| id);
    let w = words(env, node);
    let flags = b.load(Type::I64, w, word(id, 1));
    let s = b.iconst(Type::I64, SET);
    let set = b.int_bin(IntBinOp::And, flags, s);
    b.icmp(IntCond::Ne, set, env.zero)
}

/// Parameter `id` set by its producer (a nest's NestLoopParam, a SubPlan's argument): `null` a `Bool`. Its flags' seen
/// bits clear: a store reading it is filled again at its next open (store_reopen: PG's chgParam).
pub fn store(b: &mut FunctionBuilder<'_>, env: &Env, node: i32, id: i32, v: Value, null: Value) {
    store_row(b, env, node, id, (v, null), None);
}

/// `store` for a nest's outer row: re-entered after a stop (`fresh` a `Bool`, false) it keeps the row's seen bits, as PG
/// adds the parameter to chgParam once per outer row (a store reading only a grand-owner's parameter is not filled again).
pub fn store_row(b: &mut FunctionBuilder<'_>, env: &Env, node: i32, id: i32, (v, null): (Value, Value), fresh: Option<Value>) {
    let w = words(env, node);
    b.store(w, word(id, 0), v);
    let (is_null, not_null) = (b.iconst(Type::I64, SET | NULL), b.iconst(Type::I64, SET));
    let mut flags = b.select(null, is_null, not_null);
    if let Some(fresh) = fresh {
        let (old, seen) = (b.load(Type::I64, w, word(id, 1)), b.iconst(Type::I64, !(SET | NULL)));
        let seen = b.int_bin(IntBinOp::And, old, seen);
        let kept = b.select(fresh, env.zero, seen);
        flags = b.int_bin(IntBinOp::Or, flags, kept);
    }
    b.store(w, word(id, 1), flags);
}

/// M8 L0-l: the nest kind that expressions evaluated outside the program's frame wait on (a store's calls run in the
/// ScanStore's executor state, which shares no PARAM_EXEC: srf/store_read.rs): nest.initplan when one reads an InitPlan's
/// parameter; nest.rerun (store_reopen: each open passes the parameters, those an owner wrote since its last open as PG's
/// chgParam) when one reads another executor parameter in a rerun set's pipeline `k` or in a nest's or nested SubPlan's
/// inner (`k` past the graph pipelines, D1086); `None` when they read neither (an InitPlan elsewhere in the graph, or
/// around the store, is not theirs). Elsewhere: UNPASSED.
pub fn outside<'p>(gx: &Gx<'_, 'p>, k: usize, calls: &[Node<'p>]) -> Option<KindId> {
    let pipes = gx.pipelines();
    let passed = k >= pipes.len() || gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == k));
    let mut kind = None;
    for &n in calls {
        gravity_decompose::expr::walk(n, &mut |x| {
            match x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC).map(|p| producer(gx, p.paramid)) {
                Some(Some(KindId::NestInitplan)) => kind = Some(KindId::NestInitplan),
                Some(_) if passed => kind = kind.or(Some(KindId::NestRerun)),
                Some(_) => crate::refuse!(pipes[k].node_id, Reason::Shape { why: ShapeWhy(UNPASSED) }),
                None => {}
            }
            true
        });
    }
    kind
}

/// A store's call reading an executor parameter in a pipeline no owner re-opens (no inner, no rerun set's member).
pub const UNPASSED: &str = "a set or table function reading an executor parameter outside a nest's inner or a rerun set (no open passes it: its store would be filled once, blind to it)";

/// A key or bound over executor parameters that is not read per re-open (an expression over them; a LIMIT bound): the
/// first unbuilt producer's name, else nest.param's while that kind is unbuilt, else a Shape; a Shape when no producer
/// sets one. A bare parameter key is read per re-open (frame/site.rs `EXEC_KEY`, L0-a).
pub fn key(gx: &Gx<'_, '_>, node: i32, value: Node<'_>) -> ! {
    let mut ids: mcx::PgVec<'_, i32> = mcx::PgVec::new_in(gx.mcx);
    gravity_decompose::expr::walk(value, &mut |x| {
        ids.extend_in(x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC).map(|p| p.paramid));
        true
    });
    ids.iter().for_each(|&id| need(gx, node, id));
    if crate::ops::kind(KindId::NestParam).is_none() {
        crate::not_built!(node, Variant::Kind(KindId::NestParam))
    }
    crate::refuse!(node, Reason::Shape { why: ShapeWhy("a key or bound computed from executor parameters (not read per re-open)") })
}

#[cfg(test)]
mod tests {
    use gravity_ir::{ConvOp, IntBinOp, Type};

    /// A parameter's producer writes its pair and a reader loads it back (L0-c / L0-b): the value, and NULL from the flags.
    #[test]
    fn a_parameter_written_by_its_producer_reads_back() {
        for (v, null, want) in [(42, false, 42), (7, true, 1007)] {
            let got = crate::frame::stop::tests::run1(&[], &mut |b, env| {
                env.words = Some(env.params);
                let (x, n) = (b.iconst(Type::I64, v), b.bconst(null));
                super::store(b, env, 0, 3, x, n);
                let (x, n) = super::load(b, env, 0, 3);
                let n = b.conv(ConvOp::Zext, Type::I64, n);
                let k = b.iconst(Type::I64, 1000);
                let n = b.int_bin(IntBinOp::Mul, n, k);
                let flags = b.load(Type::I64, env.params, super::word(3, 1));
                let set = b.iconst(Type::I64, super::SET);
                let set = b.int_bin(IntBinOp::And, flags, set);
                let n = b.int_bin(IntBinOp::Add, n, set);
                let r = b.int_bin(IntBinOp::Add, x, n);
                b.int_bin(IntBinOp::Sub, r, set)
            });
            assert_eq!(got, want, "value {v} null {null}");
        }
    }

    /// An InitPlan parameter's expression place carries its id and its InitPlan's handle word; a plain id is no such place.
    #[test]
    fn an_initplan_place_carries_its_id_and_handle() {
        let p = super::init_place(7, 300).expect("in range");
        assert_eq!(super::init_of(p), Some((7, 300)));
        assert_eq!(super::init_of(7), None);
        assert_eq!((super::init_place(1 << 16, 0), super::init_place(0, 1 << 14)), (None, None));
    }
}
