//! Kind `expr.wholerow` (cards/M2.md; v1 execmain gravity/wholerow.rs `build` / `from_cols`): a whole-row `Var` (attno 0)
//! of a scan is PG's `ExecEvalWholeRowVar` over the scan's row. `attach` rewrites each such `Var` in the compiled program
//! into gravity_expr's `RowExpr` over the columns `Var(Scan, 1..n)` under the rowtype descriptor the `Var` already carries
//! (`State::descs`, read from the site's cell at run time: the owner's fact, never in the content key); the row site
//! forms the tuple into the call region (`heap_form_tuple`, then `toast_flatten_tuple_to_datum` when a column is
//! external: `toast_build_flattened_tuple`'s image). A dropped column keeps its stored datum over a heap scan, which
//! carries it (D2789: `image_site`, as C forms the image from the slot after `slot_getallattrs`), and is NULL elsewhere.
//! A row-valued IS [NOT] NULL of one is `ExecEvalRowNull`'s AND over the live columns. The decomposer gives a whole-row
//! scan every live column, and a heap scan every dropped one, as a lane (sources.rs `whole_row_atts`, `projected_of`).
//! The rewrite adds no literal (the expression's words are unchanged) and claims nothing: the program stays
//! gravity_expr's and the later extensions still see it. D437: a
//! subquery scan's own row as a `record` (C `ExecEvalWholeRowVar`'s RECORD leg: the subplan's non-junk columns, renamed
//! by the RTE's `eref` aliases, blessed) is gravity_expr's `ROW(...)` of those columns under those names
//! (decompose `subplan::RecordRow`); a VALUES scan's (D732) and a FunctionScan's (D693) the same way. mergecmd record-row
//! cache: its descriptor carries the statement's alias names and its columns' types, which the content key (lower
//! inputs/key.rs `record_rows`) and the front key (exec inputs/front.rs, the RTE's eref) hash, so the program is filed in
//! this backend's cache like any other: the blessed typmod is this backend's (typcache's record registry outlives the
//! program), and the program, owning the descriptor's context, is never shareable (exec admit `shareable`: an owner) and
//! never enters the shared tier. A named tuplestore's is its relation's descriptor (step/expr_row_named.rs, still unfiled).
//! Refused by name: a `record` row no source describes (a composite FROM member's),
//! an `OUTER_VAR` whole row or an `INNER_VAR` `record` one (V4R-245: an `INNER_VAR`
//! whole row of a relation rowtype is ON CONFLICT DO UPDATE's EXCLUDED, its Vars keep the source), and a stored-image size
//! reader over a row value (pg_column_size over any varlena, D2954) whose bytes may differ from PostgreSQL's (D2764 /
//! D2793, `stored_image`, asked by step/expr.rs
//! for every expression: a value read past a PostgreSQL tuple copy is packed there, a whole row that drops a dropped
//! attribute's stored value; typesem `image` names the readers). A row past one
//! row site's values (gravity_expr `row::MAX_EMITTED_ARGS`, 97) spans continuation sites (CR-WIDE-ROW, `row::row_site`):
//! any width a descriptor holds. Budget 250. Owner M2-WHOLEROW.
use gravity_expr::nodes::row::{image_site, row_site, RowExpr};
use gravity_expr::nodes::bool::BoolNode;
use gravity_expr::nodes::misc::Imm;
use gravity_expr::nodes::null::NullTestNode;
use gravity_expr::nodes::var::Var;
use gravity_expr::nodes::Builder;
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_nodes::list::NodeList;
use types_nodes::primnodes::{BoolExprType, CoercionForm, RowExpr as RowExprTree, VarReturningType, OUTER_VAR};
use types_nodes::Node;

use gravity_decompose::catalog::Am;
use gravity_decompose::ops::shared::record_row::RecordRow;
use gravity_decompose::Op;
use gravity_program::LaneId as SiteLane;

use crate::face::kinds::KindId;
use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, SubEmit};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::step::expr::Outer;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::ExprWholerow, &ExprExtImpl(&WHOLE))], ..crate::ops::NONE };

struct Whole;
static WHOLE: Whole = Whole;

const RECORDOID: u32 = 2249;
const BOOLOID: u32 = 16;

/// The refusals by name (the census places them).
pub const RECORD_ROW: &str = "a whole-row reference of a record row (VALUES / subquery / function RTE)";
pub const JOIN_ROW: &str = "a whole-row reference through OUTER_VAR / INNER_VAR";
pub const RECORD_FORM: &str = "a subquery's record row the ROW() compiler refused";
pub const STORED_IMAGE: &str = "a stored-image size reader (pg_column_size) over a row value whose bytes may be a packed copy or keep a dropped value (D2764 / D2793)";
pub const STORED_SIZE: &str = "pg_column_size over a varlena read past a PostgreSQL tuple copy, which gives a short one a 1-byte header (D2954)";

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// V4R-245: a scan's whole row, or an INNER_VAR one of a relation rowtype (ON CONFLICT DO UPDATE's EXCLUDED; setrefs
/// makes no other); an OUTER_VAR whole row and an INNER_VAR `record` one refuse by name.
fn row_src(v: &Var) -> Result<(), &'static str> {
    if v.src == VarSource::Scan || (v.src == VarSource::Inner && v.typ != RECORDOID) { Ok(()) } else { Err(JOIN_ROW) }
}

/// The row's columns under whole-row `Var` `v`: `(dropped, type, attno)` per attribute, from its rowtype's descriptor,
/// or for a subquery scan's `record` row (`rec`) its non-junk columns at their resnos.
fn cols<'g>(c: &Compiled<'_>, v: &Var, rec: Option<&RecordRow<'_>>, mcx: Mcx<'g>) -> Result<PgVec<'g, (bool, u32, i16)>, &'static str> {
    row_src(v)?;
    if v.typ == RECORDOID {
        let Some(r) = rec else { return Err(RECORD_ROW) };
        // D3374: a function row's dropped attribute (type 0, no expression) keeps the refusal of before: its record would hold
        // the dropped attribute, which no ROW() here forms
        if r.named == 0 && r.cols.iter().any(|c| c.expr.is_none() && c.ty.0 == 0) {
            return Err(RECORD_ROW);
        }
        // a named tuplestore's dropped attribute (type 0, EXPR-ODDS A) is dropped
        return Ok(r.cols.iter().map(|c| (r.named != 0 && c.ty.0 == 0, c.expr.map_or(c.ty.0, gravity_expr::nodes::func::expr_type), c.resno)).collect_in(mcx));
    }
    let desc = c.state.descs.get(v.desc as usize).and_then(|d| d.desc.as_ref()).filter(|d| d.tdtypeid != RECORDOID);
    let Some(desc) = desc else { return Err(RECORD_ROW) };
    Ok(desc.attrs.iter().take(desc.natts.max(0) as usize).enumerate().map(|(a, t)| (t.attisdropped, t.atttypid, a as i16 + 1)).collect_in(mcx))
}

/// The subquery scan whose `record` row the expression reads (its level-0 whole-row `record` `Var`s name one varno).
fn record_of<'g, 'p>(px: &Px<'g, 'p>, node: Node<'p>) -> Option<&'g RecordRow<'p>> {
    let mut varno = None;
    let mut one = true;
    gravity_decompose::expr::walk(node, &mut |x| {
        if let Some(v) = x.as_var().filter(|v| v.varattno == 0 && v.varlevelsup == 0 && gravity_fn::typesem::expr::record_type(v.vartype)) {
            one &= varno.replace(v.varno).is_none_or(|w| w == v.varno);
        }
        true
    });
    let varno = varno.filter(|_| one)?;
    let graph: &'g gravity_decompose::graph::PipelineGraph<'p> = px.gx.graph;
    graph.record_rows.iter().find(|r| r.scanrelid as i32 == varno)
}

/// `ROW(Var(scan, resno) ...)` named by the record row's column names, compiled by gravity_expr (`compile_row_expr`: the
/// descriptor built and blessed as C `ExecTypeFromTL` + `ExecTypeSetColNames` + `BlessTupleDesc` build it). Its nodes
/// live in a context the program owns; the names and types are in the program's keys (key.rs `record_rows`).
fn record_expr<'p>(px: &mut Px<'_, 'p>, c: &mut Compiled<'p>, r: &RecordRow<'p>) -> Result<NodeId, &'static str> {
    // D3374: a function row's dropped attribute (type 0, decompose `function_record_row`): its record would hold the
    // dropped attribute, which this ROW() does not form
    if r.cols.iter().any(|c| c.expr.is_none() && c.ty.0 == 0) {
        return Err(RECORD_ROW);
    }
    // the nodes are read by the compile here only (the descriptor copies the names, the program's `Var`s copy the attnos
    // and types)
    let mcx = px.mcx();
    let m = px.program_cx("expr.wholerow record");
    let tree = (|| -> types_error::PgResult<RowExprTree<'p>> {
        let mut args = mcx::reserved_in(r.cols.len(), mcx);
        let mut names = mcx::reserved_in(r.cols.len(), mcx);
        for col in r.cols.iter() {
            // D1887: C ExecTypeFromTL's column collation is exprCollation's (compile_row_expr copies the Var's): XMLSERIALIZE, JSON_*
            let (typ, typmod, coll) = col.expr.map_or(col.ty, |e| (gravity_expr::nodes::func::expr_type(e), gravity_expr::nodes::func::expr_typmod(e), gravity_expr::nodes::func::expr_collation(e)));
            args.push_in(Node::mk_var(m, r.scanrelid as i32, col.resno, typ, typmod, coll, 0)?);
            names.push_in(Node::mk_string(m, col.name)?);
        }
        Ok(RowExprTree { args: NodeList::from_slice(m, &args[..])?, row_typeid: RECORDOID, row_format: CoercionForm::COERCE_IMPLICIT_CAST, colnames: NodeList::from_slice(m, &names[..])?, location: -1 })
    })()
    .map_err(|_| RECORD_FORM)?;
    let mut b = Builder { prog: &mut c.prog, state: &mut c.state, catalog: px.gx.catalog, outer: None, tableoid: None, aggorder: false, returning: None, folded: true };
    let id = gravity_expr::nodes::row::compile_row_expr(&mut b, &tree).map_err(|_| RECORD_FORM)?;
    Ok(id)
}

fn whole_at(c: &Compiled<'_>, id: NodeId) -> Option<Var> {
    match c.prog.nodes[id.0 as usize] {
        ExprNode::Var(v) if v.attno == 0 => Some(v),
        _ => None,
    }
}

/// D2764 / D2793: `STORED_IMAGE` where expression `node` calls a function that reads its argument's stored image (typesem
/// `image::reads_stored_image_size`) over a row value whose bytes this program cannot vouch for (`vouched`). Two images
/// of one row value differ in size where PostgreSQL's (1) keeps a dropped attribute's stored value (`ExecEvalWholeRowVar`
/// flattens the slot after `slot_getallattrs`), or (2) crossed a tuple copy, whose `heap_fill_tuple` gives a varlena of
/// at most 126 data bytes a 1-byte header (VARATT_CAN_MAKE_SHORT): an Agg evaluates the first row of each group over
/// `ExecCopySlotHeapTuple`'s copy (nodeAgg.c `agg_retrieve_direct`), and a Sort, Hash, Material, tuplestore or Gather
/// holds copies. Gravity's image keeps its 4-byte header wherever it is read, so only a value formed in this expression
/// or read as stored matches. D2954: (2) holds for any varlena, so `pg_column_size` (typesem `image::reads_header_width`:
/// the compression method and TOAST pointer survive a copy) over a varlena of another type that is not `vouched` is
/// `STORED_SIZE` (a computed short text or array read past an Agg's first-row copy or a Sort: 6 bytes there, 9 here; a
/// plain-storage varlena, which a copy never packs, refuses alike and the fallback answers it). Called for every compiled
/// expression with a hosted call (step/expr.rs `compile_parts`). D3374: a reader whose NULL-ness and `> 0` follow its
/// argument's (`image::sized_when_set`) is served where only they are read (`set_only`): any image answers alike.
pub(crate) fn stored_image<'p>(px: &Px<'_, 'p>, at: (usize, usize), outer: Outer<'_, 'p>, returning: bool, node: Node<'p>) -> Option<&'static str> {
    use gravity_fn::typesem::image::{reads_header_width, reads_stored_image_size, sized_when_set};
    let catalog = px.gx.catalog;
    let mut hit = None;
    let mut set_only: PgVec<'_, Node<'p>> = counted(px, at, node).then_some(node).into_iter().collect_in(px.mcx());
    gravity_decompose::expr::walk(node, &mut |x| {
        set_only.extend_in(set_test(x));
        let free = |funcid: u32| sized_when_set(funcid) && set_only.iter().any(|n| n.ptr_eq(x));
        let call = x.as_func_expr().filter(|f| hit.is_none() && reads_stored_image_size(f.funcid) && !free(f.funcid));
        if let Some((funcid, a)) = call.and_then(|f| f.args.iter().next().map(|a| (f.funcid, a))) {
            let why = gravity_decompose::catalog::expr_type::type_of(a).and_then(|t| {
                if composite(catalog, t) {
                    Some(STORED_IMAGE)
                } else {
                    (reads_header_width(funcid) && varlena(px, t)).then_some(STORED_SIZE)
                }
            });
            hit = why.filter(|_| !vouched(px, at, outer, returning, a));
        }
        hit.is_none()
    });
    hit
}

/// D3374: the operand `x` reads only for its NULL-ness or `> 0`: `IS [NOT] NULL`'s, or the size reader of an int4 compare
/// that is its `> 0` (gravity_expr percode `sized_class`, a class both keys hold: `y > 0`, `0 < y`).
fn set_test<'p>(x: Node<'p>) -> Option<Node<'p>> {
    if let Some(t) = x.as_null_test() {
        return t.arg;
    }
    let o = x.as_op_expr()?;
    let side = gravity_fn::typesem::image::positive_operand(o.opfuncid)?;
    (gravity_expr::percode::sized_class(o.opfuncid, &o.args) == Some(true)).then(|| o.args.iter().nth(side)).flatten()
}

/// D3374: `node` is the argument of a `count(x)` without DISTINCT of the group ending pipeline `at.0`, whose fold reads
/// only its NULL-ness (ops/agg/trans/args.rs `args_of` compiles it here).
fn counted(px: &Px<'_, '_>, at: (usize, usize), node: Node<'_>) -> bool {
    use gravity_decompose::ops::shared::agg_class::Class;
    let g = (at.0 < px.gx.pipelines().len()).then(|| px.gx.group_at(at.0)).flatten();
    let first = |t: Node<'_>| t.as_target_entry().map_or(t, |t| t.expr).ptr_eq(node);
    let reads = |a: &gravity_decompose::ops::shared::agg::AggDecl<'_>| {
        a.node.as_aggref().and_then(|r| r.args.iter().find(|&t| crate::ops::agg::trans::args::read_arg(t))).is_some_and(first)
    };
    g.is_some_and(|g| g.aggs.iter().any(|a| a.class == Class::CountAny && a.mods.distinct == 0 && reads(a)))
}

/// Type `t` is a varlena (`typlen` -1, through a domain: decompose typefacts reads a domain as its base).
fn varlena(px: &Px<'_, '_>, t: u32) -> bool {
    gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, t).is_some_and(|f| f.typlen == -1)
}

/// Type `t` is a row type: `record`, or a composite type (through a domain) with attributes.
fn composite(catalog: &dyn gravity_expr::Catalog<'_>, t: u32) -> bool {
    use gravity_expr::catalog::Support;
    t == RECORDOID || catalog.support(Support::Natts(t)).is_ok_and(|n| n > 0)
}

/// Value `e` (a stored-image reader's argument: a row value, or a varlena for `pg_column_size`) has PostgreSQL's bytes
/// here: it is not a column read (a `ROW(...)`, a
/// function's result, a Param: formed by the same code on both sides); or this pipeline's scan's whole row, formed in
/// this expression in the scan's own lists outside a RETURNING list (whose row is the written version's), with its
/// dropped attributes' stored values when it has any (`keeps_dropped`, D2789; a named tuplestore's leaves them NULL); or
/// a stored column of this pipeline's heap scan (its bytes as on disk, which a copy keeps), directly or as an `OUTER_VAR`
/// through `outer`'s entry or the last projection before step `at.1`. Any other column read (a whole row through
/// `OUTER_VAR` or past the scan's lists, an `INNER_VAR`, a breaker's or another scan's column) may hold PostgreSQL's
/// packed copy.
fn vouched<'p>(px: &Px<'_, 'p>, at: (usize, usize), outer: Outer<'_, 'p>, returning: bool, e: Node<'p>) -> bool {
    crate::stack_check();
    let entry = |t: Node<'p>| t.as_target_entry().map_or(t, |t| t.expr);
    let Some(v) = e.as_var() else {
        // a relabel or domain coercion passes its argument's datum on
        let arg = e.as_relabel_type().map(|r| r.arg).or_else(|| e.as_coerce_to_domain().map(|c| c.arg));
        return arg.is_none_or(|a| vouched(px, at, outer, returning, a));
    };
    if v.varlevelsup != 0 || returning || v.varreturningtype != VarReturningType::VAR_RETURNING_DEFAULT {
        return false;
    }
    if let Some((_, list)) = outer.filter(|o| o.0 == v.varno && v.varattno > 0) {
        return list.iter().nth(v.varattno as usize - 1).is_some_and(|t| stored_column(px, at.0, entry(t)));
    }
    if v.varno == OUTER_VAR && v.varattno > 0 {
        let ops = &px.gx.pipelines()[at.0].ops;
        for j in (0..at.1.min(ops.len())).rev() {
            match &ops[j] {
                Op::Filter(_) | Op::Gate(_) | Op::SubPlan(_) => {}
                Op::Project(list) => return list.iter().nth(v.varattno as usize - 1).is_some_and(|t| stored_column(px, at.0, entry(t))),
                _ => return false,
            }
        }
        return false;
    }
    if v.varno <= 0 {
        return false;
    }
    if v.varattno > 0 {
        return stored_column(px, at.0, e);
    }
    // the whole row is formed here only in the scan's own lists (a filter, gate, SubPlan step or projection with only such
    // steps before it); past them (an aggregate's argument at the pipeline's end, which PostgreSQL evaluates over the
    // Agg's copy of the row) the decomposer reads it through the child's projection
    let p = &px.gx.pipelines()[at.0];
    let scan_op = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_));
    let own = p.ops.get(at.1).is_some_and(scan_op) && p.ops[..at.1].iter().all(scan_op);
    let dropped = dropped_in(px.gx.catalog, v.vartype) || matches!(p.source, gravity_decompose::Source::Named { relid, .. } if relid_dropped(px, relid));
    v.varattno == 0 && own && (!dropped || keeps_dropped(px, at.0))
}

/// Expression `e` is a stored column of pipeline `k`'s heap relation scan (`Var(scanrelid, attno > 0)`).
fn stored_column(px: &Px<'_, '_>, k: usize, e: Node<'_>) -> bool {
    let Some(v) = e.as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0) else { return false };
    let Some(p) = px.gx.pipelines().get(k) else { return false };
    p.source.relation().is_some_and(|(scanrelid, relid, _)| scanrelid as i32 == v.varno && px.gx.facts.rel(relid).is_some_and(|r| r.am == Am::Heap))
}

/// Relation `relid` has a dropped attribute (a named tuplestore's descriptor is its relation's).
fn relid_dropped(px: &Px<'_, '_>, relid: u32) -> bool {
    let facts = px.gx.facts;
    facts.rel(relid).is_some_and(|r| (1..=r.natts).any(|a| facts.att(relid, a).is_none()))
}

/// D2789: pipeline `k`'s source is a heap relation scan whose site carries every dropped attribute (decompose
/// `whole_row_atts`), so its whole row keeps their stored values (`image_site`), as PostgreSQL's does.
fn keeps_dropped(px: &Px<'_, '_>, k: usize) -> bool {
    let Some((_, relid, site)) = px.gx.pipelines().get(k).and_then(|p| p.source.relation()) else { return false };
    let facts = px.gx.facts;
    let carried = |a: i16| site.lanes().iter().any(|l| l.id == SiteLane::Attr(a));
    facts.rel(relid).is_some_and(|r| r.am == Am::Heap && (1..=r.natts).all(|a| facts.att(relid, a).is_some() || carried(a)))
}

/// Step `at` is a RETURNING list's projection (write.returning lays its OLD / NEW images), as step/expr.rs asks.
fn returning_list(px: &Px<'_, '_>, at: (usize, usize)) -> bool {
    crate::ops::kind_as::<crate::face::end::RetImages>(KindId::WriteReturning).and_then(|f| (f.0)(px.gx, at.0, at.1)).is_some()
}

/// Composite type `t` (through a domain) has a dropped attribute: an attno under its count with no field type.
fn dropped_in(catalog: &dyn gravity_expr::Catalog<'_>, t: u32) -> bool {
    use gravity_expr::catalog::Support;
    let n = catalog.support(Support::Natts(t)).unwrap_or(0).min(i16::MAX as u32) as i16;
    (1..=n).any(|attno| catalog.support(Support::Field { typid: t, attno }).is_ok_and(|f| f == 0))
}

/// Each whole-row `Var` of `c` as a `RowExpr` over its descriptor's columns, and a row-valued IS [NOT] NULL of one as
/// `ExecEvalRowNull` reads it: every live column NULL (NOT NULL), an AND of the columns' tests; `Err` names the refusal.
pub(crate) fn rewrite<'p>(px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>) -> Result<(), &'static str> {
    let mcx = px.mcx();
    let rec = record_of(px, node);
    let tests: PgVec<'_, (usize, NullTestNode, Var)> = (0..c.prog.nodes.len()).filter_map(|i| match c.prog.nodes[i] {
        ExprNode::NullTest(t) if t.row => whole_at(c, t.arg).map(|v| (i, t, v)),
        _ => None,
    }).collect_in(mcx);
    for (i, t, v) in tests {
        let cols = cols(c, &v, rec, mcx)?;
        let mut b = Builder { prog: &mut c.prog, state: &mut c.state, catalog: px.gx.catalog, outer: None, tableoid: None, aggorder: false, returning: None, folded: true };
        let vars: PgVec<'_, NodeId> = cols.iter().filter(|c| !c.0).map(|&(_, typ, attno)| b.push(ExprNode::Var(Var { src: v.src, attno, typ, desc: 0 }))).collect_in(mcx);
        let test = |arg| NullTestNode { arg, is_null: t.is_null, row: false, site: None };
        let node = match vars[..] {
            [] => ExprNode::Imm(Imm { value: 1, typ: BOOLOID }),
            [one] => ExprNode::NullTest(test(one)),
            _ => {
                let each: PgVec<'_, NodeId> = vars.iter().map(|&v| b.push(ExprNode::NullTest(test(v)))).collect_in(mcx);
                ExprNode::Bool(BoolNode { op: BoolExprType::AND_EXPR, args: b.kids_of(&each[..]) })
            }
        };
        c.prog.nodes[i] = node;
        // the test was the whole-row Var's one reader (a plan node compiles to its own program node)
        c.prog.nodes[t.arg.0 as usize] = ExprNode::Imm(Imm { value: 0, typ: BOOLOID });
    }
    let rows: PgVec<'_, (usize, Var)> = (0..c.prog.nodes.len()).filter_map(|i| whole_at(c, NodeId(i as u32)).map(|v| (i, v))).collect_in(mcx);
    for (i, v) in rows {
        if v.typ == RECORDOID && v.src == VarSource::Scan {
            let r = rec.ok_or(RECORD_ROW)?;
            let id = if r.named != 0 { crate::step::expr_row_named::record(px, c, r)? } else { record_expr(px, c, r)? };
            c.prog.nodes[i] = core::mem::replace(&mut c.prog.nodes[id.0 as usize], ExprNode::Imm(Imm { value: 0, typ: BOOLOID }));
            continue;
        }
        let cols = cols(c, &v, None, mcx)?;
        // D2789: over a heap scan that carries its dropped attributes, every attribute is an argument and the image site keeps
        // a dropped one's stored datum (`keeps_dropped`); a RETURNING list's row is the written version's, whose dropped
        // attributes are NULL. Otherwise trailing dropped columns take no argument (the row site leaves them NULL), and a
        // dropped one before a live one reads the first live column, whose value the site skips (`attisdropped`), so the
        // row reads no column it does not carry.
        let image = v.src == VarSource::Scan && cols.iter().any(|c| c.0) && !returning_list(px, at) && keeps_dropped(px, at.0);
        let n = if image { cols.len() } else { cols.iter().rposition(|c| !c.0).map_or(0, |p| p + 1) };
        let first = cols.iter().position(|c| !c.0).unwrap_or(0);
        let mut b = Builder { prog: &mut c.prog, state: &mut c.state, catalog: px.gx.catalog, outer: None, tableoid: None, aggorder: false, returning: None, folded: true };
        let args: PgVec<'_, NodeId> = (0..n).map(|a| {
            let a = if cols[a].0 && !image { first } else { a };
            b.push(ExprNode::Var(Var { src: v.src, attno: cols[a].2, typ: cols[a].1, desc: 0 }))
        }).collect_in(mcx);
        let kids = b.kids_of(&args[..]);
        let site = if image { image_site(&mut b, n) } else { row_site(&mut b, n) };
        c.prog.nodes[i] = ExprNode::Row(RowExpr { args: kids, row_type: v.typ, desc: v.desc, natts: cols.len() as u32, site });
    }
    Ok(())
}

impl ExprExtMech for Whole {
    /// The rewrite in place; no claim (`None`): the program stays gravity_expr's, the later extensions try it.
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, _sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        if let Err(why) = rewrite(px, at, node, c) {
            shape(px.gx.pipelines().get(at.0).map_or(-1, |p| p.node_id), why);
        }
        None
    }

    fn emit(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _row: &RowVals, _ex: ExId, _site: u32, _sub: SubEmit<'_>) -> [Value; 3] {
        crate::refuse!(env.node, Reason::Bug { payload: "an expr.wholerow site: the kind claims none" })
    }
}

#[cfg(test)]
mod tests {
    use super::{row_src, JOIN_ROW, RECORDOID};
    use gravity_expr::nodes::var::Var;
    use gravity_expr::Source;

    /// V4R-245: EXCLUDED.* (an INNER_VAR whole row of a relation rowtype) is served; an OUTER_VAR whole row and an
    /// INNER_VAR `record` row still refuse by name.
    #[test]
    fn whole_row_sources() {
        let v = |src, typ| Var { src, attno: 0, typ, desc: 0 };
        assert_eq!(row_src(&v(Source::Scan, 16_384)), Ok(()));
        assert_eq!(row_src(&v(Source::Inner, 16_384)), Ok(()));
        assert_eq!(row_src(&v(Source::Outer, 16_384)), Err(JOIN_ROW));
        assert_eq!(row_src(&v(Source::Inner, RECORDOID)), Err(JOIN_ROW));
    }
}
