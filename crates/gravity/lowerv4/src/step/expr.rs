//! Expression compile, kind `expr.core` (KWC lowerv3 scan/expr.rs): a plan expression compiled through `gravity_expr` into
//! `Px` (tier-A ops inline; tier B / C sites as hosted calls under the fmgr result contract, `hir.rs`), its literals and
//! parameters placed as words (consts.rs), and emitted over a row: each column the expression reads through its cell's
//! `datum` accessor, an `Aggref` from the row's aggregate cells (`Env::aggs`), a raise through the row's raise block.
//! Laziness is gravity_expr's: an operand PG never evaluates is guarded (AND / OR / CASE / COALESCE and strict calls mask
//! its raise; a host call runs under a branch), so it cannot raise (D237). A filter's quals stay separate expressions
//! (`quals`), evaluated in plan order and stopping at the first false or NULL, as `ExecQual`. VOLATILE calls, whole-row
//! references and `= ANY` lists are M2's kinds, reached through face/step.rs's faces. Budget 340 lines. Owner M1-STEP.
use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use gravity_abi::status::raise;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::Op;
use gravity_expr::emit::{ColsRef, EmitError, ParamNeed, RowRefs};
use gravity_expr::{Compiled, ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use gravity_lower::Place;
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_core::Oid;
use types_nodes::list::NodeList;
use types_nodes::Node;

use crate::face::kinds::{Face, KindId};
use crate::face::step::{expr_exts, ExprFace, ExprMech, ExtPlan, EXT_AT};
use crate::step::expr_ext::Claims;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::frame::site::CTID_AT;
use crate::graph::Gx;
use crate::hir::{self, Ex};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep;
use crate::rep::expr_type::result_type;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ExprCore, &ExprFace(&ExprCore))], ..crate::ops::NONE };

/// `expr.core`'s registry value: the compile is this file's functions (siblings call them as `super::step_expr::…`).
pub(crate) struct ExprCore;

impl ExprMech for ExprCore {
    fn compile<'p>(&self, px: &mut Px<'_, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32) -> ExId {
        compile(px, node, outer, id)
    }
    fn compile_at<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, outer: Outer<'_, 'p>, id: i32) -> ExId {
        compile_at(px, at.0, at.1, node, outer, id)
    }
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals, ex: ExId) -> (Value, Value) {
        emit(b, env, id, row, ex)
    }
    fn pred(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals, ex: ExId) -> Value {
        pred(b, env, id, row, ex)
    }
    fn out_type<'p>(&self, gx: &Gx<'_, 'p>, node: Node<'p>, id: i32) -> Oid {
        result_type(gx, node, id)
    }
    fn tf_of(&self, gx: &Gx<'_, '_>, typid: u32) -> TypeFacts {
        tf_of(gx, typid)
    }
}

/// `(varno, list)`: every `Var(varno, n)` is entry `n` of `list` (a child's projection, a VALUES row).
pub(crate) type Outer<'a, 'p> = Option<(i32, &'a NodeList<'p>)>;

const SELF_ITEM_POINTER: i16 = -1;
const TABLEOID_ATTNO: i16 = -6;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

pub(crate) fn emit_what(e: EmitError) -> &'static str {
    match e {
        EmitError::Unsupported(k) => k.emit_refusal(),
        EmitError::NoColumn(0) => "a whole-row reference in an expression",
        EmitError::NoColumn(a) if a < 0 || a == crate::face::end::TABLEOID_AT => "a system column other than ctid / tableoid",
        EmitError::NoColumn(a) if (gravity_program::protos::XMIN_AT..=gravity_program::protos::CMAX_AT).contains(&a) => "a system column over a source without system lanes (V4R-88: heap only)",
        EmitError::NoColumn(_) => "a column the row does not carry",
        EmitError::CodesLane(_) => "a dictionary-coded column read as a value",
        EmitError::NoCells | EmitError::NoTable(_) | EmitError::PerCodeByRef => "a per-code table or call cell",
        EmitError::LongList => "a long list",
        EmitError::NotVector => "the vector form",
        EmitError::NoAggCell(_) => "an aggregate cell outside the record",
    }
}

/// memsig E-prog: a test program's lists context (a bump context of its own, never freed: a test builds a few programs).
#[cfg(test)]
pub(crate) fn test_lists() -> mcx::Mcx<'static> {
    Box::leak(Box::new(mcx::MemoryContext::new_bump("lowerv4-test lists"))).mcx() // alloc: Cold
}

/// `node` compiled, its program's lists in `m` (memsig E-prog: `args::compile_scan`).
pub(crate) fn compiled<'p>(m: mcx::Mcx<'p>, gx: &Gx<'_, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, tableoid: Option<(i32, gravity_expr::TableOid)>, returning: Option<gravity_expr::nodes::Returning>) -> Compiled<'p> {
    match crate::ops::agg::trans::args::compile_scan(m, node, gx.catalog, outer, tableoid, returning) { // V4R-178: an order-free ORDER BY's cell
        Ok(c) => c,
        Err(gravity_expr::CompileError::Unsupported(k)) => shape(id, k.compile_refusal()),
        Err(gravity_expr::CompileError::Pg(_)) => shape(id, "an expression the catalog refused"),
    }
}

/// `node` compiled into `px` for plan node `id`: its needs placed as words, a hosted call admitted unless `hir::refused`
/// names it. A target entry compiles as its expression.
pub(crate) fn compile<'p>(px: &mut Px<'_, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32) -> ExId {
    compile_in(px, node, outer, id, (usize::MAX, usize::MAX), None, |_, _, _, _| None)
}

/// `compile` of a claimed expression's operand (step/expr_ext.rs): its literals keep the words they have under `root`, the
/// claiming expression's root, so a literal is one word however many expressions compile it (D390).
pub(crate) fn compile_under<'p>(px: &mut Px<'_, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, root: Node<'p>) -> ExId {
    compile_in(px, node, outer, id, (usize::MAX, usize::MAX), Some(root), |_, _, _, _| None)
}

/// B2-31: `compile_under` of a claim's operand whose path reads read their lanes as the claiming expression's step `rw`
/// would (strategies/jsonb_nums.rs, jsonb_fwd.rs: the rewrite and LATE's check, as `compile_in_at` runs them).
pub(crate) fn compile_under_rw<'p>(px: &mut Px<'_, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, root: Node<'p>, rw: (usize, usize)) -> ExId {
    crate::stack_check();
    compile_in_at(px, node, outer, id, ((usize::MAX, usize::MAX), rw), Some(root), |_, _, _, _| None)
}

/// `compile` for the row step `j` of pipeline `k` reads: the per-code roots over the row's code columns read by code
/// (step/percode.rs), else a LIKE the window's frame pass decides (step/text_frame.rs), else an `expr_exts` claim; any is
/// the `Ex::ext`. An `OUTER` column that is one of the row's code columns (an aggregate's argument at its producer's end:
/// `code_cols` keys them by the last projection's resno, as `OUTER` is) goes to percode.rs as a scan column; its `probe`
/// keeps a column only when every read of it is a served root's (the probe's scan lanes are empty), and a column it
/// drops, or a `None`, is `OUTER` again, read per row.
pub(crate) fn compile_at<'p>(px: &mut Px<'_, 'p>, k: usize, j: usize, node: Node<'p>, outer: Outer<'_, 'p>, id: i32) -> ExId {
    compile_in(px, node, outer, id, (k, j), None, |px, c, ex, cl| {
        let codes = crate::step::project::code_cols(px, k, j);
        let at = (0..c.prog.nodes.len()).filter(|&i| outer.is_none() && matches!(&c.prog.nodes[i], ExprNode::Var(v) if v.src == VarSource::Outer && codes.iter().any(|x| x.0 == v.attno))).collect_in(px.mcx());
        var_src(c, &at[..], VarSource::Scan);
        // M8 B1: an expression reading an InitPlan's result takes no per-code form (they evaluate it off the row, before PG
        // runs the InitPlan)
        let init = c.prog.nodes.iter().any(|n| matches!(n, ExprNode::Param(q) if q.kind == types_nodes::primnodes::ParamKind::PARAM_EXEC && crate::rerun::params::producer(px.gx, q.id) == Some(KindId::NestInitplan)));
        // jsonops (3): key tests over presence lanes, before the per-code roads (not undone: plain words)
        crate::ops::scan::pgrc::strategies::jsonb_codes::presence(px, k, j, c);
        let pc = if init { None } else { crate::dict::face::rewritten(px, k, j, c, |px, c| crate::dict::percode::attach(px, k, j, c, ex, id)) };
        match pc {
            // D2997: num.cmp also claims the compares the per-code roots left (a dict-code atom OR a numeric one)
            Some(p) => {
                if let Some(x) = crate::step::expr_ext::residual(px, (k, j), node, c, ex, cl) {
                    let _ = p.ext.set(x);
                }
                Some(p as Rc<dyn Any>)
            }
            None => {
                var_src(c, &at[..], VarSource::Outer);
                // C06: a code set compile_parts left to the per-code road, which declined it: `pre_claim`'s claim now, as for any other list
                let pre = if cl.sets { crate::step::expr_ext::pre_claim(px, (k, j), node, c, ex, cl, id) } else { None };
                pre.or_else(|| crate::dict::strategies::text_frame::attach(px, k, j, c).map(|f| f as Rc<dyn Any>)).or_else(|| crate::step::expr_ext::attach(px, (k, j), node, c, ex, cl, expr_exts()))
            }
        }
    })
}

/// D531: `compile` of a projection entry a group keys by its integer word (step/project.rs): the numeric `extract` at its
/// root emits that word (`RowRefs::word_root`, gravity_expr's datetime form), marked by its `Ex::ext`.
/// P1b: its number path casts read their int8 path lanes as step `j` of pipeline `k` would (strategies/jsonb_nums.rs).
pub(crate) fn compile_word<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, id: i32) -> ExId {
    // B2-31: an `extract(unit FROM timestamptz)` key is expr.fusion's word claim (a fixed session offset's open-coded field,
    // the hosted numeric's word under a zone with DST); every other word root is gravity_expr's datetime form
    compile_in_at(px, node, None, id, ((usize::MAX, usize::MAX), at), None, |px, c, ex, cl| {
        let fused = crate::step::expr_fuse::word_claimable(node).then(|| crate::step::expr_ext::attach(px, at, node, c, ex, cl, std::iter::once(crate::step::expr_fuse::FUSE_WORD))).flatten();
        fused.or_else(|| {
            fires::fire("expr.word.extract"); // B2-31 witness signal: an extract group key keyed by its integer word
            Some(Rc::new(WordRoot) as Rc<dyn Any>)
        })
    })
}

pub(crate) struct WordRoot;

/// B2-31: whether the program left after a claim still reads a scan column: a node reached from its root (gravity_expr's
/// `emit::kids`) that is a scan `Var` other than a claim's placeholder, or one whose children that walk does not know.
/// `false`: every read sits in the claim's operands, which compile and check their own (`compile_under_rw`).
fn reads_scan(c: &Compiled<'_>, mcx: Mcx<'_>) -> bool {
    let p = &c.prog;
    let mut stack = mcx::pg_vec![in mcx; c.root];
    while let Some(id) = stack.pop() {
        let kids = gravity_expr::emit::kids(p, id);
        match &p.nodes[id.0 as usize] {
            ExprNode::Var(v) if v.src == VarSource::Scan && v.attno < EXT_AT => return true,
            ExprNode::Var(_) | ExprNode::Const(_) | ExprNode::Param(_) => {}
            _ if kids.is_empty() => return true,
            _ => stack.extend_in(kids),
        }
    }
    false
}

fn var_src(c: &mut Compiled<'_>, at: &[usize], src: VarSource) {
    for &i in at {
        if let ExprNode::Var(v) = &mut c.prog.nodes[i] {
            v.src = src;
        }
    }
}

/// P1c: `compile` of a group's computed key over the row at op `j` of pipeline `k` (table/args.rs): its forwarded path
/// reads read their path lanes as step `j` would (strategies/jsonb_fwd.rs); its own compile keeps no step.
pub(crate) fn compile_key<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, id: i32) -> ExId {
    compile_in_at(px, node, None, id, ((usize::MAX, usize::MAX), at), None, |_, _, _, _| None)
}

/// `compile_ext` whose path reads read their lanes as step `rw` would (`compile_key`'s, percode_key.rs).
pub(crate) fn compile_ext_rw<'g, 'p>(px: &mut Px<'g, 'p>, node: Node<'p>, id: i32, rw: (usize, usize), attach: impl FnOnce(&mut Px<'g, 'p>, &mut Compiled<'p>, ExId) -> Option<Rc<dyn Any>>) -> ExId {
    compile_in_at(px, node, None, id, ((usize::MAX, usize::MAX), rw), None, |px, c, ex, _| attach(px, c, ex))
}

/// `compile` with the `Ex::ext` `attach` makes of the compiled expression (it may rewrite its `Var`s) before it is placed.
pub(crate) fn compile_ext<'g, 'p>(px: &mut Px<'g, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, attach: impl FnOnce(&mut Px<'g, 'p>, &mut Compiled<'p>, ExId) -> Option<Rc<dyn Any>>) -> ExId {
    compile_in(px, node, outer, id, (usize::MAX, usize::MAX), None, |px, c, ex, _| attach(px, c, ex))
}

/// An extension's claim here (after placement) takes its plan nodes' literals out of the needs (a second `place`); its
/// operands compile after the expression, their literals under its root (`root`, else `node`: step/expr_ext.rs).
#[allow(clippy::too_many_arguments)]
fn compile_in<'g, 'p>(px: &mut Px<'g, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, at: (usize, usize), root: Option<Node<'p>>, attach: impl FnOnce(&mut Px<'g, 'p>, &mut Compiled<'p>, ExId, &mut Claims<'g, 'p>) -> Option<Rc<dyn Any>>) -> ExId {
    compile_in_at(px, node, outer, id, (at, at), root, attach)
}

/// `compile_in` whose number path casts read their int8 path lanes as step `rw` would (`rw`: compile_word's step, whose
/// own compile keeps no step: strategies/jsonb_nums.rs).
fn compile_in_at<'g, 'p>(px: &mut Px<'g, 'p>, node: Node<'p>, outer: Outer<'_, 'p>, id: i32, (at, rw): ((usize, usize), (usize, usize)), root: Option<Node<'p>>, attach: impl FnOnce(&mut Px<'g, 'p>, &mut Compiled<'p>, ExId, &mut Claims<'g, 'p>) -> Option<Rc<dyn Any>>) -> ExId {
    crate::admit::refusal::check_interrupts(id); // D231, D2909: each expression's compile is a cancel point
    let mut cl = Claims::new(px.mcx());
    let node = node.as_target_entry().map_or(node, |te| te.expr);
    // a claim operand (`root` given: step/expr_ext.rs `compile_subs`) is read VOLATILE by its plan node too, here where
    // the node is: its claim's emit asks (effects/visible/expr.rs `volatile`, `volatile_sub`) and holds no node
    let volatile = root.is_some() && crate::effects::visible::expr::operand(px.gx, at.0, node);
    let root = root.unwrap_or(node);
    let (mut c, mut places, mut needs, mut ext, hoisted, held) = compile_parts(px, node, root, outer, id, (at, rw), &mut cl);
    if ext.is_none() && !crate::ops::shared::dummy::dummy(px.gx, at.0, at.1) {
        ext = attach(px, &mut c, ExId(px.nexs() as u32), &mut cl);
        if ext.as_deref().is_some_and(|x| x.is::<ExtPlan>()) {
            let mcx = px.mcx();
            (places, needs) = crate::step::consts::place(px, &c, node, root, outer, id, &mcx::concat_in(&[&hoisted[..], &cl.skip[..]], mcx)[..]);
        }
    }
    // B2-31: a claim whose program reads no scan column itself leaves every read to its operands, which compile under `rw`
    // and check there (the claimed subtree's own nodes stay in `c`, reached from no root)
    if !(ext.as_deref().is_some_and(|x| x.is::<ExtPlan>()) && !reads_scan(&c, px.mcx())) {
        crate::ops::scan::pgrc::strategies::jsonb_nums::check(px, rw, &c, id); // P1b: a late guarded column's reads
        crate::ops::scan::pgrc::strategies::jsonb_fwd::check(px, rw, &c, id); // P1c: a late forwarded column's reads
    }
    let ex = px.add_placed(Ex { c, places, needs, ext, held, volatile, late: None });
    crate::step::expr_ext::compile_subs(px, ex, cl.subs, outer, id, root, rw);
    ex
}

/// A whole-row `Var` or a non-constant `= ANY` array is an extension's to claim before `emit_check`; a volatile call
/// takes `expr.volatile`'s held words.
#[allow(clippy::type_complexity)]
fn compile_parts<'g, 'p>(px: &mut Px<'g, 'p>, node: Node<'p>, root: Node<'p>, outer: Outer<'_, 'p>, id: i32, (at, rw): ((usize, usize), (usize, usize)), cl: &mut Claims<'_, 'p>) -> (Compiled<'p>, Vec<Place>, Vec<ParamNeed>, Option<Rc<dyn Any>>, PgVec<'g, usize>, Vec<i32>) {
    let gx = px.gx;
    let tableoid = gx.graph.scan_lanes.iter().find(|l| l.node_id == id && l.names().any(|n| n == "tableoid")).map(|l| (l.scanrelid as i32, l.relid));
    // D774: a RETURNING projection's tableoid is its target's (write.returning `RetTableoid`), a plan-time constant
    let tableoid = tableoid.or_else(|| crate::ops::kind_as::<crate::face::end::RetTableoid>(KindId::WriteReturning).and_then(|f| (f.0)(gx, at.0, at.1)));
    // D3681: over a relation-set source a row's tableoid is its own relation's, the source's `TABLEOID_AT` cell (scan/heap.rs
    // `push_tableoid`), never the first relation's constant
    let set = |varno: i32| gx.pipelines().get(at.0).is_some_and(|p| matches!(&p.source, gravity_decompose::Source::Range { site, scanrelid, .. } if site.set.len() > 1 && i32::from(*scanrelid) == varno));
    let tableoid = tableoid.map(|(varno, relid)| (varno, if set(varno) { gravity_expr::TableOid::Col(VarSource::Scan, crate::face::end::TABLEOID_AT) } else { gravity_expr::TableOid::Rel(relid) }));
    // merge#192: the MERGE target varno's tableoid while write.merge compiles its actions; D3471: ahead of the scan's own
    // lane when the expression reads that varno's tableoid, since with the target's Append on the probe side the actions
    // compile in a leaf scan's pipeline (`id` the leaf's), whose lane names the leaf's varno, not the result relation's
    let merge = px.ext::<StepExt>(Face::Step).and_then(|e| e.tableoid);
    let reads = |varno: i32| {
        let mut hit = false;
        gravity_decompose::expr::walk(node, &mut |n| {
            hit |= n.as_var().is_some_and(|v| v.varlevelsup == 0 && v.varno == varno && v.varattno == TABLEOID_ATTNO);
            !hit
        });
        hit
    };
    let tableoid = match merge {
        Some(m) if tableoid.is_some_and(|t| t.0 != m.0) && reads(m.0) => Some(m),
        _ => tableoid.or(merge),
    };
    // M4-RET R4a (V4R-286): a RETURNING projection's OLD / NEW Vars read the images write.returning lays out
    let returning = crate::ops::kind_as::<crate::face::end::RetImages>(KindId::WriteReturning).and_then(|f| (f.0)(gx, at.0, at.1));
    // memsig E-prog: built in place in the lowering's lists root, where a hosted program's lists stay (`hir::finish_keeping`)
    let mut c = compiled(px.lists_mcx(), gx, node, outer, id, tableoid, returning);
    // P1b: a `text::bigint` cast of a declared int8 path lane reads the lane where it is valid (strategies/jsonb_nums.rs)
    crate::ops::scan::pgrc::strategies::jsonb_nums::rewrite(px, rw, &mut c);
    // P1c: a path read at a group over the forwarded column reads the lane its projection carries (strategies/jsonb_fwd.rs)
    crate::ops::scan::pgrc::strategies::jsonb_fwd::rewrite(px, rw, &mut c);
    // D3038: `octet_length` of a column with an octet lane reads the lane (strategies/octet_lane.rs)
    let tested = crate::ops::scan::pgrc::strategies::octet_lane::rewrite(px, rw, outer.is_none(), node, &mut c);
    // V4R-255 (D717): only over the planner's dummy relation (dummy.rs: a Result under a constant false / NULL
    // one-time gate, its target list never evaluated) a system column reads ctid's slot, NULL there as `cols_of` reads
    // that Result's user columns; over any other source it keeps its own mapping or refuses by name
    let dummy = crate::ops::shared::dummy::dummy(gx, at.0, at.1);
    // D774: only a RETURNING projection (right after its write) reads tableoid's lane, which write.returning lays
    // D1112: or a SubPlan step of the RETURNING list, between the write and its projection (write.returning `ret_write`)
    let ret_list = gx.pipelines().get(at.0).is_some_and(|p| matches!(p.ops.get(at.1), Some(gravity_decompose::Op::Project(_) | gravity_decompose::Op::SubPlan(_))) && crate::ops::modifytable::returning::ret_write(p, at.1).is_some());
    for n in c.prog.nodes.iter_mut() {
        match n {
            ExprNode::Var(v) if dummy && v.src == VarSource::Scan && v.attno < 0 => v.attno = CTID_AT,
            ExprNode::Var(v) if (v.src, v.attno) == (VarSource::Scan, SELF_ITEM_POINTER) => v.attno = CTID_AT,
            // D774: a tableoid no constant answered reads its lane (write.returning lays a routed row's leaf there)
            ExprNode::Var(v) if ret_list && (v.src, v.attno) == (VarSource::Scan, TABLEOID_ATTNO) => v.attno = crate::face::end::TABLEOID_AT,
            // V4R-88: xmin / cmin / xmax / cmax read the heap source's system lanes (a source without them refuses by name)
            ExprNode::Var(v) if v.src == VarSource::Scan && gravity_program::protos::sys_at(v.attno).is_some() => v.attno = gravity_program::protos::sys_at(v.attno).unwrap_or(v.attno),
            _ => {}
        }
    }
    let ex = ExId(px.nexs() as u32);
    // D2367: over the dummy relation the expression is never evaluated (no row comes), so it takes no extension claim
    // (a claim reads its columns from the row, which carries none); the plain form compiles and `cols_of` reads nothing
    // C06: a code set PG hashed over a code column the row step reads by code is the per-code road's first (`attach`)
    cl.sets = !dummy && outer.is_none() && at.0 != usize::MAX && crate::dict::percode::sets_first(px, at.0, at.1, &c, node);
    let pre = if (dummy && c.emit_check().is_ok()) || cl.sets { None } else { crate::step::expr_ext::pre_claim(px, at, node, &mut c, ex, cl, id) };
    if let Err(e) = c.emit_check() {
        shape(id, emit_what(e));
    }
    let mut held = Vec::new(); // alloc: Program
    if c.host_calls() > 0 {
        if let Some((what, _)) = hir::refused(&c) {
            shape(id, what);
        }
        if let Some(why) = crate::step::expr_row::stored_image(px, at, outer, returning.is_some(), node) {
            shape(id, why);
        }
        if crate::effects::visible::expr::acts(px.gx.facts, &c) {
            held = crate::step::expr_ext::held(px, at, &c, ex, id);
        }
        // D2044: a hosted call to user code runs on no row past a pull's count (D439's Volatile protocol): its raise ahead
        // of a FETCH, for a row the cursor may never take, would leave state the statement cannot hold (PL/pgSQL returns its
        // error before its SPI_finish: exec runner `PlState`)
        if crate::effects::visible::expr::user_code(&c) {
            px.declare_proto(gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Volatile, at.0 as u16));
        }
    }
    // A' (join/hoist.rs): a call the build evaluated reads its verdict; its literals are not this program's needs
    let mut hoisted = crate::ops::hashjoin::strategies::hoist::rewrite(px, node, &mut c);
    hoisted.extend_in(tested);
    let mcx = px.mcx();
    let (places, needs) = crate::step::consts::place(px, &c, node, root, outer, id, &mcx::concat_in(&[&hoisted[..], &cl.skip[..]], mcx)[..]);
    (c, places, needs, pre, hoisted, held)
}

/// The step face's compile-time state (`Px::ext_mut(Face::Step)`): each filter op's quals, compiled once per pipeline
/// identity; each pipeline's leading filter ops' lone plain quals (step/filter.rs, built once per pipeline identity: a
/// filter of N ops reads them at every op); each pipeline's per-code tables (step/percode.rs).
#[derive(Default)]
pub(crate) struct StepExt {
    /// Keyed `(k, j, Px::ident(k))`, as `tables` is by identity (D3913).
    quals: BTreeMap<(usize, usize, usize), Rc<[ExId]>>,
    pub leads: BTreeMap<(usize, usize), Rc<[Option<ExId>]>>,
    /// Keyed by pipeline identity, an embedded inner's tagged `(k + 1) << 32` (u64: wasm32's usize is 32 bits).
    pub tables: Vec<(u64, Rc<RefCell<crate::dict::percode::Tables>>)>,
    /// mergecmd merge#192: the target varno's `tableoid` while write.merge compiles its MATCHED and NOT MATCHED BY SOURCE
    /// actions: one heap target's OID, a plan-time constant (D774's RETURNING form, ret_sys.rs), or D3467 the row's junk
    /// tableoid column over several heap result relations; `None` otherwise.
    pub tableoid: Option<(i32, gravity_expr::TableOid)>,
    /// append's arm markers (ops/append/lower.rs `make_arm`): each outermost union's run `(plan node, first, len)` and each
    /// run's one-slot check `(first, len, breaker, holds)`, walked once per union, not once per arm.
    pub arm_runs: Vec<(i32, usize, usize)>,
    pub arm_one_runs: Vec<(usize, usize, gravity_abi::breaker::Breaker, bool)>,
}
crate::frame::px::ext_g!(StepExt);

pub(crate) fn ext<'a>(px: &'a mut Px<'_, '_>) -> &'a mut StepExt {
    px.ext_mut::<StepExt>(Face::Step)
}

/// Filter op `j` of pipeline `k`: its quals in plan order, each its own expression (compiled once per identity; the
/// source's verdicts and the filter step share them). D3913: a pass made again under another identity (`Px::remake`, a
/// swapped side) compiles its own, so a per-code qual declares its tables on that pass's site, the one its source binds.
pub(crate) fn quals(px: &mut Px<'_, '_>, k: usize, j: usize) -> Rc<[ExId]> {
    let key = (k, j, px.ident(k));
    if let Some(q) = px.ext_mut::<StepExt>(Face::Step).quals.get(&key) {
        return Rc::clone(q);
    }
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Filter(n) = p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a filter's quals over another op" }) };
    let q: Rc<[ExId]> = match n.as_list() {
        Some(l) => l.iter().map(|q| compile_at(px, k, j, q, None, node)).collect(),
        None => std::iter::once(n).map(|q| compile_at(px, k, j, q, None, node)).collect(),
    }; // alloc: Program
    px.ext_mut::<StepExt>(Face::Step).quals.insert(key, Rc::clone(&q)); // alloc: Program
    q
}

/// A key's type facts for its hash / equality / order image: the catalog's (`typefacts::facts_of`: an enum is its oid
/// word, a domain its base's row under its own typid), else the opaque by-reference row (V4R-84).
pub(crate) fn tf_of(gx: &Gx<'_, '_>, typid: u32) -> TypeFacts {
    typefacts::facts_of(gx.facts, typid).unwrap_or(typefacts::opaque(typid, typid, -1, false, false))
}

/// A cell's type facts for its datum alone, where no catalog is at hand (emit time): the datum arm reads only the cell's
/// Rep, never the image, so this row never decides a key's image (that is `tf_of`'s).
pub(crate) fn tf_datum(typid: u32) -> TypeFacts {
    typefacts::row_of(typid).copied().unwrap_or(typefacts::opaque(typid, typid, -1, false, false))
}

/// `datum` of each column the expression reads (memoised per row), as values over column numbers: scan and `OUTER_VAR`
/// columns the row's (a scan's attno, after a projection its resno), `INNER_VAR` columns the joined row's inner map; the
/// scan columns in `codes` are read by code, not here. `[scan, inner]`, each `(values, valids)`.
fn cols_of<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, row: &RowVals, c: &Compiled<'_>, codes: &[(i16, u8, u8)], lazy: &[(bool, i16)]) -> [(PgVec<'e, Value>, PgVec<'e, Value>); 2] {
    let t = b.bconst(true);
    let mcx = env.mcx;
    let mut out: [(PgVec<'e, Value>, PgVec<'e, Value>); 2] = [(PgVec::new_in(mcx), PgVec::new_in(mcx)), (PgVec::new_in(mcx), PgVec::new_in(mcx))];
    let mut cells = PgVec::new_in(mcx);
    for n in c.prog.nodes.iter() {
        let ExprNode::Var(v) = n else { continue };
        let coded = v.src == VarSource::Scan && codes.iter().any(|x| x.0 == v.attno);
        // an extension site's placeholder is its own; a shredded path lane (SHRED_AT + n, M6 R3) is the row's column; a
        // claim's fallback-only column is rendered at its read (D2997, `fallback_cols`)
        let at = (v.src == VarSource::Inner, v.attno);
        if v.attno <= 0 || (EXT_AT..crate::frame::site::SHRED_AT).contains(&v.attno) || coded || lazy.contains(&at) || cells.iter().any(|&(s, a, _)| (s, a) == at) {
            continue;
        }
        let inner = v.src == VarSource::Inner;
        // a dummy relation's Result (a constant-false gate, no scan below) projects columns it never reads
        let cr = if row.row.cells.is_empty() { None } else if inner { row.inner_col(env, v.attno) } else { row.col(v.attno) };
        if cr.is_none() && !row.row.cells.is_empty() {
            shape(node, "a column the row does not carry");
        }
        cells.push_in((inner, v.attno, cr));
    }
    for (inner, a, _) in cells.iter() {
        let o = &mut out[*inner as usize];
        if o.0.len() < *a as usize {
            o.0.resize_in(*a as usize, env.zero);
            o.1.resize_in(*a as usize, t);
        }
    }
    // a lazy column's slot (never read: the hook answers it) keeps the vectors indexed by attno
    for &(inner, a) in lazy {
        let o = &mut out[inner as usize];
        if o.0.len() < a as usize {
            o.0.resize_in(a as usize, env.zero);
            o.1.resize_in(a as usize, t);
        }
    }
    let mut cx = env.lx(b, node);
    for (inner, a, cr) in cells {
        let (d, valid) = match cr {
            Some(cr) => {
                let (d, null) = rep::datum(&mut cx, cr, &tf_datum(cr.cell.typid));
                (d, cx.b.int_bin(IntBinOp::Xor, null, t))
            }
            None => (env.zero, cx.b.bconst(false)),
        };
        let o = &mut out[inner as usize];
        (o.0[a as usize - 1], o.1[a as usize - 1]) = (d, valid);
    }
    env.memo = cx.memo;
    out
}

/// D2997: column `attno` of the row (`inner`: the inner side's) rendered here, `[value, valid, 0]`: a claim's fallback-only
/// column at its read (step/expr_ext_lazy.rs). The column memo is set aside: what is built in the arm's block serves no read
/// outside it.
pub(crate) fn lazy_col(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, inner: bool, attno: i16) -> [Value; 3] {
    let cr = if inner { row.inner_col(env, attno) } else { row.col(attno) };
    let Some(cr) = cr else { shape(node, "a column the row does not carry") };
    let memo = env.memo.set_aside();
    let t = b.bconst(true);
    let mut cx = env.lx(b, node);
    let (d, null) = rep::datum(&mut cx, cr, &tf_datum(cr.cell.typid));
    let valid = cx.b.int_bin(IntBinOp::Xor, null, t);
    env.memo = cx.memo; // the arm's bounded loop headers stay through `restore` (D4445)
    env.memo.restore(memo);
    [d, valid, env.zero]
}

/// D2997: the columns `c` reads only under a claim's fallback arm (step/text_like.rs `claim_calls`: `CASE WHEN
/// Var(EXT_AT + 2n) THEN .. ELSE <call> END`), as `(inner, attno)`: never a code column (`codes`), never one any other
/// node reads (the hot path, a WHEN, an interpreter island's range). Empty for an expression without a claim, so its
/// emission is unchanged.
pub(crate) fn fallback_cols<'g>(c: &Compiled<'_>, codes: &[(i16, u8, u8)], mcx: Mcx<'g>) -> PgVec<'g, (bool, i16)> {
    let p = &c.prog;
    let fallback = |i: usize| match &p.nodes[i] {
        ExprNode::Case(k) if k.arg.is_none() && k.whens.len == 2 => match &p.nodes[p.kids[k.whens.start as usize].0 as usize] {
            ExprNode::Var(v) if (EXT_AT..crate::frame::site::SHRED_AT).contains(&v.attno) && (v.attno - EXT_AT) % 2 == 0 => k.default,
            _ => None,
        },
        _ => None,
    };
    if !(0..p.nodes.len()).any(|i| fallback(i).is_some()) {
        return PgVec::new_in(mcx);
    }
    let col = |n: &ExprNode| match n {
        ExprNode::Var(v) if v.attno > 0 && v.attno < EXT_AT && !(v.src == VarSource::Scan && codes.iter().any(|x| x.0 == v.attno)) => Some((v.src == VarSource::Inner, v.attno)),
        _ => None,
    };
    // the hot reads: from the root, every arm but a claim's fallback; then every Var in an island's range
    let (mut hot, mut cold) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    let (mut seen, mut stack, mut arms) = (mcx::pg_vec![in mcx; false; p.nodes.len()], mcx::pg_vec![in mcx; c.root], PgVec::new_in(mcx));
    while let Some(id) = stack.pop() {
        let i = id.0 as usize;
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        hot.extend_in(col(&p.nodes[i]));
        let skip = fallback(i);
        arms.extend_in(skip);
        stack.extend_in(gravity_expr::emit::kids(p, id).into_iter().filter(|&k| Some(k) != skip));
    }
    for s in p.islands.iter() {
        hot.extend_in(p.nodes[s.first.0 as usize..=s.root.0 as usize].iter().filter_map(col));
    }
    let mut seen = mcx::pg_vec![in mcx; false; p.nodes.len()];
    let mut stack = arms;
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut seen[id.0 as usize], true) {
            continue;
        }
        cold.extend_in(col(&p.nodes[id.0 as usize]));
        stack.extend_in(gravity_expr::emit::kids(p, id));
    }
    cold.sort_unstable();
    cold.dedup();
    cold.retain(|x| !hot.contains(x));
    cold
}

/// Expression `id`'s per-code reads (step/percode.rs), when it has any.
pub(crate) fn plan_of<'a>(env: &Env<'a, '_>, id: ExId) -> Option<&'a crate::dict::percode::Plan> {
    env.placed(id).ext.as_deref().and_then(|x| x.downcast_ref())
}

/// The row references of expression `id` over `row`, handed to `f`: columns (the code columns by code unless `plain`; a
/// claimed site's placeholder its value), constants, the call cells and region, the aggregate row.
fn with_refs<T>(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, plain: bool, sites: &[(i16, [Value; 3])], f: impl FnOnce(&mut FunctionBuilder<'_>, &Compiled<'_>, &RowRefs<'_>) -> T) -> T {
    let e = env.placed(id);
    let pl = plan_of(env, id).filter(|_| !plain);
    let codes = pl.map_or(&[][..], |p| p.cols.as_slice());
    let lazy = fallback_cols(&e.c, codes, env.mcx);
    let [(mut sv, mut sn), (iv, inn)] = cols_of(b, env, node, row, &e.c, codes, &lazy[..]);
    crate::step::expr_ext::splice(&mut sv, &mut sn, sites, env.zero);
    let raises = crate::step::expr_ext::raises(b, sites, env.mcx);
    let (cv, cn) = crate::frame::hoist::words(b, env, node, id);
    let cols = ColsRef::Values { values: &sv[..], valids: &sn[..] };
    let mcx = env.mcx;
    let (codes, tables) = pl.map_or_else(|| (PgVec::new_in(mcx), PgVec::new_in(mcx)), |p| crate::dict::percode_len::refs(b, env, node, row, p));
    let scan = if pl.is_some() { ColsRef::Lanes { row: env.zero, lanes: &[] } } else { cols };
    let mut refs = RowRefs { outer: cols, inner: ColsRef::Values { values: &iv[..], valids: &inn[..] }, codes: &codes, tables: &tables, col_raises: &raises[..], ..RowRefs::scan(scan, &cv, &cn, env.mcx) };
    refs.word_root = e.ext.as_deref().is_some_and(|x| x.is::<WordRoot>()) || crate::step::expr_fuse::word_claimed(e);
    let aggs = env.aggs.take(); // V4R-342: lent back to a claimed site emitted at its read (step/expr_ext_lazy.rs)
    if let Some(a) = aggs.as_ref() {
        refs.rec = Some(a.rec);
        refs.aggs = &a.cells;
    }
    let late = match env.hoisted(id) {
        Some(h) => mcx::to_vec_in(&h.late[..], mcx),
        None => PgVec::new_in(mcx),
    };
    refs.late = &late[..]; // D3033: the late operands computed at the hoist scope
    let refs = env.hir.with_ex(b, e, refs);
    let (out, env) = crate::step::expr_ext_lazy::run(b, env, (node, row, id), &aggs, refs, value3, lazy, |b, refs| f(b, &e.c, refs));
    env.aggs = aggs;
    out
}

/// `[value, valid, err]` of `f` over the row; for a per-code expression whose read met a faulting or NULL entry (the
/// `TABLE` raise bit, only where PG evaluates that root) the whole expression again over the rendered columns, cold, so
/// its value, NULL and error are PG's. An error word that is a constant without the bit has no cold path and no join.
fn emit3(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, f: Emit3) -> [Value; 3] {
    crate::admit::refusal::check_interrupts(node); // D231, D2909: each expression's emission is a cancel point
    let held = &env.placed(id).held;
    if held.len() >= 2 {
        let v = crate::frame::stop::held(b, env, &held[..2], None, &mut |b, env| mcx::to_vec_in(&emit_row(b, env, node, row, id, f)[..], env.mcx));
        return [v[0], v[1], v[2]];
    }
    emit_row(b, env, node, row, id, f)
}

type Emit3 = fn(&Compiled<'_>, &mut FunctionBuilder<'_>, &RowRefs<'_>) -> Result<[Value; 3], EmitError>;

/// `emit3` without the held result: every evaluation of it runs `f`.
fn emit_row(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId, f: Emit3) -> [Value; 3] {
    let sites = crate::step::expr_ext::sites(b, env, node, row, id, value3);
    let fast = with_refs(b, env, node, row, id, false, &sites[..], |b, c, refs| f(c, b, refs)).unwrap_or_else(|e| shape(node, emit_what(e)));
    if plan_of(env, id).is_none() || b.const_of(fast[2]).is_some_and(|(_, e)| e & raise::bit(raise::TABLE) == 0) {
        return fast;
    }
    let bit = b.iconst(Type::I64, raise::bit(raise::TABLE) as i64);
    let hit = b.int_bin(IntBinOp::And, fast[2], bit);
    let hit = b.icmp(IntCond::Ne, hit, env.zero);
    let types = fast.map(|v| b.value_type(v));
    let (l_slow, l_join) = (b.create_block(&[]), b.create_block(&types));
    b.mark_cold(l_slow);
    b.cond_br(hit, (l_slow, &[]), (l_join, &fast));
    b.switch_to(l_slow);
    // D2074: the row's lazy per-code entries filled, then the fast form again; only a fault left (a raise, a NULL) re-evaluates
    if crate::dict::percode_fill::fill(b, env, node, row, id) {
        let memo = env.memo.set_aside();
        let again = with_refs(b, env, node, row, id, false, &sites[..], |b, c, refs| f(c, b, refs)).unwrap_or_else(|e| shape(node, emit_what(e)));
        env.memo.restore(memo);
        let hit = b.int_bin(IntBinOp::And, again[2], bit);
        let hit = b.icmp(IntCond::Ne, hit, env.zero);
        let l_whole = b.create_block(&[]);
        b.cond_br(hit, (l_whole, &[]), (l_join, &again));
        b.switch_to(l_whole);
    }
    let memo = env.memo.set_aside();
    // D2997: a residual claim's placeholders (dict/percode.rs Plan.ext) read their sites on the rendered form too
    let slow = with_refs(b, env, node, row, id, true, &sites[..], |b, c, refs| f(c, b, refs)).unwrap_or_else(|e| shape(node, emit_what(e)));
    env.memo.restore(memo);
    if slow.iter().zip(types.iter()).any(|(&v, &t)| b.value_type(v) != t) {
        shape(node, "a per-code read whose value form differs from the rendered one");
    }
    b.br(l_join, &slow);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    [p[0], p[1], p[2]]
}

/// The expression over the row: `(value, valid)` (valid = not NULL); a raise leaves through the row's raise block.
pub(crate) fn emit(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> (Value, Value) {
    let [value, valid, err] = value3(b, env, node, row, id);
    raise_if(b, env, row, err);
    (value, valid)
}

/// `[value, valid, err]` of the expression over the row, the raise word not yet taken.
fn value3(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> [Value; 3] {
    emit3(b, env, node, row, id, |c, b, refs| c.emit(b, refs).map(|l| [l.value, l.valid, l.err]))
}

/// The predicate over the row: true only when the value is true and not NULL.
pub(crate) fn pred(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> Value {
    let [cond, valid, err] = emit3(b, env, node, row, id, |c, b, refs| c.emit_pred(b, refs).map(|p| [p.cond, p.valid, p.err]));
    raise_if(b, env, row, err);
    b.int_bin(IntBinOp::And, cond, valid)
}

/// `pred` without the raise (road `mask.rows`, frame/mask_rows.rs): `(cond AND valid, err)`, the caller decides the row.
pub(crate) fn pred_err(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> (Value, Value) {
    let [cond, valid, err] = emit3(b, env, node, row, id, |c, b, refs| c.emit_pred(b, refs).map(|p| [p.cond, p.valid, p.err]));
    (b.int_bin(IntBinOp::And, cond, valid), err)
}

/// A nonzero raise mask leaves through the row's raise block (the out count and the row's cursor written back), or the
/// function's raise block outside a row loop.
pub(crate) fn raise_if(b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals, err: Value) {
    if b.const_of(err).is_some_and(|(_, c)| c == 0) {
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

#[cfg(test)]
pub(crate) mod tests {
    use std::any::Any;
    use std::rc::Rc;
    use std::sync::Mutex;

    use datum::Datum;
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_expr::nodes::var::Var;
    use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
    use gravity_expr::{Catalog, Compiled, ExprNode, PgCatalog, Source as VarSource};
    use types_core::Oid;
    use types_error::PgResult;
    use gravity_fn::{Shim, Tier};
    use gravity_ir::builder::FunctionBuilder;
    use gravity_ir::{IntBinOp, Type, Value};
    use gravity_lower::{Located, Place, WordSrc};
    use gravity_program::{Arena, End, Pipeline};
    use mcx::MemoryContext;
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{BoolExpr, BoolExprType, FuncExpr, OpExpr, ScalarArrayOpExpr};
    use types_nodes::Node;

    use crate::face::kinds::KindId;
    use crate::face::strategy::{Strategy, FLOOR, TEXT_EQ};
    use crate::face::step::{exts_in, Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
    use crate::frame::env::Env;
    use crate::frame::frame::{Mask, WordWin};
    use crate::frame::px::{ExId, Px};
    use crate::frame::row::{Carry, RowVals};
    use crate::graph::Gx;
    use crate::hir::Ex;
    use crate::admit::refusal::{Reason, Variant};
    use crate::ops::Mech;

    static LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    /// Held by each test that reads `calls` (the log is one per process, no thread-local: the session TLS census).
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    fn log(s: &'static str) {
        LOG.lock().unwrap().push(s);
    }

    /// The calls the test extensions saw since the last `calls`.
    pub(crate) fn calls() -> Vec<&'static str> {
        std::mem::take(&mut *LOG.lock().unwrap())
    }

    /// Claims nothing.
    pub(crate) struct Decline;

    impl ExprExtMech for Decline {
        fn attach<'p>(&self, _px: &mut Px<'_, 'p>, _at: (usize, usize), _node: Node<'p>, _c: &mut Compiled<'p>, _ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
            log("decline.attach");
            let _ = sub; // a decline's operands are dropped with it
            None
        }
        fn emit(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals, _ex: ExId, _site: u32, _sub: SubEmit<'_>) -> [Value; 3] {
            unreachable!("a declined expression is never emitted by its extension")
        }
    }

    /// Claims the root as site 3 (the value 42); with a block form (`.0`) the even rows of a block pass.
    pub(crate) struct Takes(pub bool);

    impl ExprExtMech for Takes {
        fn attach<'p>(&self, _px: &mut Px<'_, 'p>, _at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, _sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
            log("claim.attach");
            c.prog.nodes[c.root.0 as usize] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT + 3, typ: c.result_type, desc: 0 });
            Some(Claim { plan: Rc::new(7u8), skip: vec![node.as_raw().as_ptr() as usize] })
        }
        fn emit(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals, _ex: ExId, site: u32, _sub: SubEmit<'_>) -> [Value; 3] {
            log("claim.emit");
            assert_eq!(site, 3);
            [b.iconst(Type::I64, 42), b.bconst(true), b.iconst(Type::I64, 0)]
        }
        fn mask_open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin, _ex: ExId) -> Option<Mask> {
            log("claim.mask_open");
            self.0.then_some(Mask(env.one))
        }
        fn mask(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value, _ex: ExId, m: Mask) -> Option<Mask> {
            log("claim.mask");
            let even = b.iconst(Type::I64, 0x5555_5555_5555_5555);
            Some(Mask(b.int_bin(IntBinOp::And, m.0, even)))
        }
    }

    /// Claims `(a + b) + c`'s inner sum as site 0, its operands `a` and `b` compiled on their own: the site is `a * b`.
    struct Product;

    impl ExprExtMech for Product {
        fn attach<'p>(&self, _px: &mut Px<'_, 'p>, _at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
            let inner = node.as_op_expr()?.args.iter().next()?;
            let args: Vec<Node<'p>> = inner.as_op_expr()?.args.iter().collect();
            let ExprNode::Op(f) = &c.prog.nodes[c.root.0 as usize] else { return None };
            let kid = c.prog.kids[c.prog.sites[f.site.0 as usize].args.start as usize].0 as usize;
            log("product.attach");
            c.prog.nodes[kid] = ExprNode::Var(Var { src: VarSource::Scan, attno: EXT_AT, typ: 20, desc: 0 });
            assert_eq!((sub(args[0]), sub(args[1])), (0, 1));
            Some(Claim { plan: Rc::new(()), skip: vec![inner.as_raw().as_ptr() as usize] })
        }
        fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, _ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3] {
            log("product.emit");
            assert_eq!(site, 0);
            let (x, y) = (sub(b, env, 0), sub(b, env, 1));
            [b.int_bin(IntBinOp::Mul, x[0], y[0]), x[1], b.int_bin(IntBinOp::Or, x[2], y[2])]
        }
    }

    static PRODUCT: Product = Product;
    pub(crate) static DECLINE: Decline = Decline;
    pub(crate) static CLAIM: Takes = Takes(true);
    pub(crate) static CLAIM_ROWS: Takes = Takes(false);

    /// `7::int8` compiled as plan node 7 at step (0, 0) of a one-pipeline graph with `exts` tried in order: the program's
    /// expressions and the expression's id.
    pub(crate) fn claimed(exts: &[&'static dyn ExprExtMech]) -> (Vec<Ex<'static>>, ExId) {
        let (exs, id, _) = claimed_over(exts, |cx| Node::mk_const(cx.mcx(), 20, -1, 0, 8, Datum::from_i64(7), false, true).expect("a const node"));
        (exs, id)
    }

    /// `claimed` over the tree `mk` builds: also its bind words' values, in word order.
    fn claimed_over(exts: &[&'static dyn ExprExtMech], mk: impl FnOnce(&'static MemoryContext) -> Node<'static>) -> (Vec<Ex<'static>>, ExId, Vec<u64>) {
        claimed_in(&gravity_expr::PgCatalog, exts, mk)
    }

    /// `claimed_over` against catalog `catalog`.
    fn claimed_in(catalog: &'static dyn Catalog<'static>, exts: &[&'static dyn ExprExtMech], mk: impl FnOnce(&'static MemoryContext) -> Node<'static>) -> (Vec<Ex<'static>>, ExId, Vec<u64>) {
        let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("ext"))); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let a = arena.handle();
        let g: &'static mut PipelineGraph = Box::leak(Box::new(PipelineGraph::new(a))); // alloc: Cold
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx: &'static Gx = Box::leak(Box::new(Gx { graph: g, facts: &NoFacts, catalog, over: None, mcx: cx.mcx() })); // alloc: Cold
        gravity_expr::nodes::func::acl_defer_begin(); // no catalog here: EXECUTE is not checked
        let mut plan = Located::new(a);
        let mut px = Px::new(a, gx, &mut plan);
        let k = mk(cx);
        let id = super::compile_in(&mut px, k, None, 7, (0, 0), None, |px, c, ex, cl| crate::step::expr_ext::attach(px, (0, 0), k, c, ex, cl, exts.iter().copied()));
        let exs = px.take_exs();
        // memsig E-prog: the programs' lists live in the lowering's lists root; the test keeps them past the `Px`
        core::mem::forget(px.lists_root.take());
        drop(px);
        // exec admit.rs `template`: each literal word takes its own occurrence of the literal in the key's traversal
        let lits: Vec<usize> = plan.seq.words.iter().zip(plan.nodes.iter()).filter(|(w, _)| matches!(w, WordSrc::Const { .. })).filter_map(|(_, n)| n.map(|n| n.as_raw().as_ptr() as usize)).collect(); // alloc: Cold
        let mut once = lits.clone();
        (once.sort_unstable(), once.dedup());
        assert_eq!(once.len(), lits.len(), "D390: a literal with two words (a literal outside the key's traversal)");
        let words = plan.seq.words.iter().map(|w| if let WordSrc::Const { value, .. } = w { value.value.as_i64() as u64 } else { 0 }).collect(); // alloc: Cold
        (exs, id, words)
    }

    /// `PgCatalog` with int4's and int4[]'s type facts (a unit test has no syscache).
    struct Int4s;

    impl<'m> Catalog<'m> for Int4s {
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            let (len, elem) = if t == 1007 { (-1, 23) } else { (4, 0) };
            Ok(TypeFacts { len, byval: len > 0, align: b'i' as i8, typtype: b'b' as i8, collation: 0, elem })
        }
        fn func(&self, f: Oid) -> PgResult<FuncFacts> {
            Catalog::<'m>::func(&PgCatalog, f)
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            Catalog::<'m>::opcode(&PgCatalog, o)
        }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'m>::op_input_types(&PgCatalog, o)
        }
        fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
            Catalog::<'m>::btree_cmp_proc(&PgCatalog, f, l, r)
        }
        fn opclass(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'m>::opclass(&PgCatalog, o)
        }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
            Catalog::<'m>::type_cmp_proc(&PgCatalog, t)
        }
        fn base_type(&self, t: Oid, m: &mut i32) -> PgResult<Oid> {
            Catalog::<'m>::base_type(&PgCatalog, t, m)
        }
        fn domain(&self, t: Oid) -> PgResult<DomainFacts<'m>> {
            Catalog::<'m>::domain(&PgCatalog, t)
        }
        fn io(&self, t: Oid) -> PgResult<IoFacts> {
            Catalog::<'m>::io(&PgCatalog, t)
        }
        fn text(&self, c: Oid) -> PgResult<TextFacts> {
            Catalog::<'m>::text(&PgCatalog, c)
        }
        fn tier(&self, f: Oid) -> Tier {
            Catalog::<'m>::tier(&PgCatalog, f)
        }
        fn shim(&self, f: Oid) -> Option<&'static Shim> {
            Catalog::<'m>::shim(&PgCatalog, f)
        }
    }

    /// `PgCatalog` with text's and int4's type facts and byte-wise text (a unit test has no syscache), function `.0`
    /// VOLATILE and every other IMMUTABLE (a builtin's volatility is unknown there, and an unknown fmgr call's volatile).
    struct Vol(Oid);

    impl<'m> Catalog<'m> for Vol {
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            let (len, collation) = if t == 25 { (-1, 100) } else { (4, 0) };
            Ok(TypeFacts { len, byval: len > 0, align: b'i' as i8, typtype: b'b' as i8, collation, elem: 0 })
        }
        fn func(&self, f: Oid) -> PgResult<FuncFacts> {
            Catalog::<'m>::func(&PgCatalog, f).map(|x| FuncFacts { volatility: Some(if f == self.0 { b'v' } else { b'i' }), ..x })
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            Catalog::<'m>::opcode(&PgCatalog, o)
        }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'m>::op_input_types(&PgCatalog, o)
        }
        fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
            Catalog::<'m>::btree_cmp_proc(&PgCatalog, f, l, r)
        }
        fn opclass(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'m>::opclass(&PgCatalog, o)
        }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
            Catalog::<'m>::type_cmp_proc(&PgCatalog, t)
        }
        fn base_type(&self, t: Oid, _m: &mut i32) -> PgResult<Oid> {
            Ok(t)
        }
        fn domain(&self, t: Oid) -> PgResult<DomainFacts<'m>> {
            Catalog::<'m>::domain(&PgCatalog, t)
        }
        fn io(&self, t: Oid) -> PgResult<IoFacts> {
            Catalog::<'m>::io(&PgCatalog, t)
        }
        fn text(&self, _c: Oid) -> PgResult<TextFacts> {
            Ok(TextFacts { deterministic: true, equality_is_bytes: true, order_is_bytes: true, ctype_is_c: true })
        }
        fn tier(&self, f: Oid) -> Tier {
            Catalog::<'m>::tier(&PgCatalog, f)
        }
        fn shim(&self, f: Oid) -> Option<&'static Shim> {
            Catalog::<'m>::shim(&PgCatalog, f)
        }
    }

    /// V4R-351 (main 06:16Z: V4R-342's per-code guard narrowed). Over a dictionary-coded text column `t` and an int4
    /// column `v`, `int4inc(v) = v AND ascii(t) = ascii(t)` reads `t` by code (step/percode.rs: the second arm is a body
    /// root) unless `int4inc` is VOLATILE: a body's table can fault, and step/expr.rs's TABLE path would run the call,
    /// made before the root, a second time. With the arms swapped (the call after the root, behind its raise) and with
    /// `octet_length(t) > int4inc(v)` (a length never faults) the per-code form stays with the call VOLATILE. The near form
    /// that must decline the guard, `int4inc` IMMUTABLE (`vol` 0), keeps the per-code form. The functions and operators by
    /// their fmgroids.h / pg_operator.dat names.
    #[test]
    fn a_volatile_call_before_a_faulting_per_code_root_takes_no_per_code_form() {
        const F_INT4INC: u32 = 766;
        const F_ASCII: u32 = 1620;
        const F_OCTET_LENGTH_TEXT: u32 = 1374;
        const F_INT4EQ: u32 = 65;
        const F_INT4GT: u32 = 147;
        const INT4_EQUAL_OPERATOR: u32 = 96; // pg_operator_d.h Int4EqualOperator
        const INT4_GT_OPERATOR: u32 = 521; // pg_operator.dat: int4 > int4
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for (vol, shape, by_code) in [(0, 0, true), (F_INT4INC, 0, false), (F_INT4INC, 1, true), (F_INT4INC, 2, true)] {
            let catalog: &'static dyn Catalog<'static> = Box::leak(Box::new(Vol(vol))); // alloc: Cold
            let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("v4r342"))); // alloc: Cold
            let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
            let a = arena.handle();
            let g: &'static mut PipelineGraph = Box::leak(Box::new(PipelineGraph::new(a))); // alloc: Cold
            g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
            let gx: &'static Gx = Box::leak(Box::new(Gx { graph: g, facts: &NoFacts, catalog, over: None, mcx: cx.mcx() })); // alloc: Cold
            gravity_expr::nodes::func::acl_defer_begin(); // no catalog here: EXECUTE is not checked
            let mut plan = Located::new(a);
            let mut px = Px::new(a, gx, &mut plan);
            let code = gravity_program::LaneDesc { kind: gravity_abi::ctx::lane::KIND_CODES, width: 4, ..gravity_program::LaneDesc::attr(1) };
            px.set_site(0, gravity_program::SiteDesc::none_in(a).with_lane(code, true));
            let m = cx.mcx();
            let t = || Node::mk_var(m, 1, 1, 25, -1, 100, 0).expect("a text column");
            let v = || Node::mk_var(m, types_nodes::primnodes::OUTER_VAR, 2, 23, -1, 0, 0).expect("an int4 column of the row");
            let call = |funcid, x| Node::mk(m, FuncExpr { funcid, funcresulttype: 23, inputcollid: if funcid == F_INT4INC { 0 } else { 100 }, args: NodeList::make1(m, x).unwrap(), ..Default::default() }).unwrap();
            let op = |opno, opfuncid, l, r| Node::mk(m, OpExpr { opno, opfuncid, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[l, r]).unwrap(), location: -1 }).unwrap();
            let and = |l, r| Node::mk(m, BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::from_slice(m, &[l, r]).unwrap(), location: -1 }).unwrap();
            let (vol_arm, root) = (op(INT4_EQUAL_OPERATOR, F_INT4EQ, call(F_INT4INC, v()), v()), op(INT4_EQUAL_OPERATOR, F_INT4EQ, call(F_ASCII, t()), call(F_ASCII, t())));
            let e = match shape {
                0 => and(vol_arm, root),
                1 => and(root, vol_arm),
                _ => op(INT4_GT_OPERATOR, F_INT4GT, call(F_OCTET_LENGTH_TEXT, t()), call(F_INT4INC, v())),
            };
            let id = super::compile_at(&mut px, 0, 0, e, None, 7);
            let exs = px.take_exs();
            let pc = exs[id.0 as usize].ext.as_deref().is_some_and(|x| x.is::<crate::dict::percode::Plan>());
            assert_eq!(pc, by_code, "int4inc VOLATILE: {}, shape {shape}", vol != 0);
        }
    }

    /// `(a + b) + c` over int8 literals.
    fn sum3(cx: &'static MemoryContext, [a, b, c]: [i64; 3]) -> Node<'static> {
        let m = cx.mcx();
        let k = |v| Node::mk_const(m, 20, -1, 0, 8, Datum::from_i64(v), false, true).expect("a const node");
        let add = |x, y| Node::mk(m, OpExpr { opno: 684, opfuncid: 463, opresulttype: 20, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[x, y]).unwrap(), location: -1 }).unwrap();
        add(add(k(a), k(b)), k(c))
    }

    /// A row with no cells.
    pub(crate) fn row<'e>(env: &Env<'e, '_>) -> RowVals<'e> {
        RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero }
    }

    /// L0-a / L0-b: the extensions are tried in order at compile (the first claim wins, its plan kept in the `ExtPlan`), and
    /// the claimed site is the extension's at emit: the root reads its value.
    #[test]
    fn an_extension_claims_at_compile_and_lowers_its_site_at_emit() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        calls();
        let (exs, id) = claimed(&[&DECLINE, &CLAIM]);
        assert_eq!(calls(), ["decline.attach", "claim.attach"]);
        assert_eq!(ExtPlan::of(&exs[id.0 as usize]).and_then(|x| x.plan.downcast_ref::<u8>()), Some(&7));
        let v = crate::frame::stop::tests::run1(&exs, &mut |b, env| {
            let r = row(env);
            super::emit(b, env, 7, &r, id).0
        });
        assert_eq!((v, calls()), (42, vec!["claim.emit"])); // alloc: Cold
        let (exs, id) = claimed(&[&DECLINE]);
        assert!(exs[id.0 as usize].ext.is_none(), "no claim: gravity_expr's");
    }

    /// M2-FRAME-EXPR review 1 and 4: a claim over a sub-tree with literals takes them out of the expression's words (the
    /// literal after it reads its own word, not the claimed ones'), and its operands compile and emit on their own.
    #[test]
    fn a_claimed_subtree_leaves_the_later_literal_its_word() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        calls();
        let (exs, id, words) = claimed_over(&[&DECLINE, &PRODUCT], |cx| sum3(cx, [7, 5, 9]));
        assert_eq!(calls(), ["decline.attach", "product.attach"]);
        assert_eq!(ExtPlan::of(&exs[id.0 as usize]).map(|x| x.subs.borrow().len()), Some(2));
        let v = crate::frame::stop::tests::run_with(&exs, &words, &mut |b, env| {
            let r = row(env);
            super::emit(b, env, 7, &r, id).0
        });
        assert_eq!((v, calls()), (7 * 5 + 9, vec!["product.emit"])); // alloc: Cold
        let (exs, id, words) = claimed_over(&[&DECLINE], |cx| sum3(cx, [7, 5, 9]));
        let v = crate::frame::stop::tests::run_with(&exs, &words, &mut |b, env| {
            let r = row(env);
            super::emit(b, env, 7, &r, id).0
        });
        assert_eq!(v, 21, "no claim: gravity_expr's sum");
    }

    /// D390: a claim made after its expression's literals are placed (a filter qual's `date_part('hour', tz) = 3` claimed
    /// at the call) compiles its operands with their literals' words under the qual's root: one word per literal, as exec
    /// admit.rs's template requires (the claimed operands' literals had a second word under their own root).
    #[test]
    fn a_claimed_operands_literal_keeps_its_one_word() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        calls();
        let (exs, id, words) = claimed_over(&[&PRODUCT], |cx| sum3(cx, [3, 4, 5]));
        assert_eq!((words, calls()), (vec![3, 4, 5], vec!["product.attach"])); // alloc: Cold
        let subs = ExtPlan::of(&exs[id.0 as usize]).map(|x| x.subs.borrow().clone()).unwrap_or_default();
        let sub_words: Vec<_> = subs.iter().map(|s| exs[s.0 as usize].places.clone()).collect(); // alloc: Cold
        assert_eq!(sub_words, [vec![Place::Word(0)], vec![Place::Word(1)]], "the operands read the qual's words");
    }

    /// M2-FRAME-EXPR review 2: `= ANY` over an array that is not a constant asks `expr.saop` before `emit_check`: unbuilt it
    /// refuses by that kind's name, built (step/expr_saop.rs, V4R-SAOP) the kind claims it.
    #[test]
    fn a_non_constant_any_array_refuses_by_the_saop_kind() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let r = crate::catch(7, || {
            let (exs, id, _) = claimed_in(&Int4s, &[], |cx| {
                let m = cx.mcx();
                let arr = Node::mk_var(m, 1, 1, 1007, -1, 0, 0).expect("an int4[] column");
                let one = Node::mk_const(m, 23, -1, 0, 4, Datum::from_i32(1), false, true).expect("a const node");
                Node::mk(m, ScalarArrayOpExpr { opno: 96, opfuncid: 65, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: 0, args: NodeList::from_slice(m, &[one, arr]).unwrap(), location: -1 }).unwrap()
            });
            assert!(ExtPlan::of(&exs[id.0 as usize]).is_some(), "expr.saop claims it");
        });
        match r {
            Ok(()) => assert!(crate::ops::built_kinds().has(KindId::ExprSaop), "claimed while expr.saop is unbuilt"),
            r => {
                let reason = r.err().expect("refused while expr.saop is unbuilt").reason;
                assert!(matches!(reason, Reason::NotBuilt { variant: Variant::Kind(KindId::ExprSaop), .. }), "{reason}");
            }
        }
    }

    /// L0-a: `expr_exts` walks the rows in order, a kind's row and an on road's alike, and skips a row of another face's
    /// type.
    #[test]
    fn extensions_come_in_registry_order() {
        static ROWS: [(Option<Strategy>, Mech); 5] = [(None, &ExprExtImpl(&DECLINE)), (Some(TEXT_EQ), &ExprExtImpl(&CLAIM)), (Some(FLOOR), &()), (None, &ExprExtImpl(&CLAIM_ROWS)), (None, &())];
        let at = |m: &dyn ExprExtMech| m as *const dyn ExprExtMech as *const () as usize;
        let got: Vec<usize> = exts_in(&ROWS).map(at).collect(); // alloc: Cold
        assert_eq!(got, [at(&DECLINE), at(&CLAIM), at(&CLAIM_ROWS)]);
    }

    /// V4R-255 (D717): a scan system column over a Result (a Const source) compiles only under a one-time gate that
    /// dummy.rs `never` passes (the planner's dummy relation, its target list never evaluated); with no gate, or a gate
    /// that is not constant false / NULL, it refuses by name as before.
    #[test]
    fn a_system_column_over_a_result_compiles_only_under_a_never_gate() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for (gate, served) in [(None, false), (Some(1i64), false), (Some(0i64), true)] {
            let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("d717"))); // alloc: Cold
            let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
            let a = arena.handle();
            let g: &'static mut PipelineGraph = Box::leak(Box::new(PipelineGraph::new(a))); // alloc: Cold
            let mut p = PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite));
            if let Some(v) = gate {
                let c = Node::mk_const(cx.mcx(), 16, -1, 0, 1, Datum::from_i64(v), false, true).expect("a bool const");
                p.push(gravity_decompose::Op::Gate(Node::mk_list(cx.mcx(), NodeList::from_slice(cx.mcx(), &[c]).unwrap()).unwrap()));
            }
            let j = p.ops.len();
            g.pipelines.push(p);
            let gx: &'static Gx = Box::leak(Box::new(Gx { graph: g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: cx.mcx() })); // alloc: Cold
            let mut plan = Located::new(a);
            let r = crate::catch(7, || {
                let mut px = Px::new(a, gx, &mut plan);
                let tableoid = Node::mk_var(cx.mcx(), 1, -6, 26, -1, 0, 0).expect("a tableoid Var");
                super::compile_in(&mut px, tableoid, None, 7, (0, j), None, |_, _, _, _| None);
            });
            match r {
                Ok(()) => assert!(served, "gate {gate:?}: a system column over a Result compiled without a never gate"),
                Err(e) => {
                    assert!(!served, "gate {gate:?}: refused under a never gate: {}", e.reason);
                    assert!(e.reason.to_string().contains("a system column other than ctid / tableoid"), "{}", e.reason);
                }
            }
        }
    }
}
