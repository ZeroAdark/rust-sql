//! Constants and parameters as cells (KEEP lowerv3 scan/expr.rs's need placement; H07): an expression's literals and
//! parameters in `Compiled::param_needs` order, each placed by the word plan (a bind-image word, or an immediate the plan
//! folds). An `IN` list's array literal stands for one word per element. Emit reads them as `(value, null)` pairs through
//! frame/hoist.rs `words`, the one loader. A nested pipeline's per-row parameter (`Place::Exec`) is the nest's (M8).
//! Budget 160 lines. Owner M1-FRAME.
use gravity_expr::catalog::DomainConstraint;
use gravity_expr::emit::ParamNeed;
use gravity_expr::nodes::array::{elem_facts, Walk};
use gravity_expr::{Catalog, Compiled, Region};
use gravity_lower::Place;
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_core::Oid;
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeTag};

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};

use crate::step::expr::Outer;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

enum Need<'p> {
    /// `(type, by value, the literal, the expression root it sits under)`
    Lit(Oid, bool, Node<'p>, Node<'p>),
    Elems { typ: Oid, byval: bool, count: usize, at: Node<'p>, root: Node<'p> },
}

/// The needs found (`list`) and the plan nodes the program no longer evaluates (`skip`: join/hoist.rs's rewritten calls).
/// The walk descends where `emit::plan` does (emit/mod.rs `kids`), an interpreter island's root included.
struct Needs<'g, 's, 'p> {
    mcx: Mcx<'g>,
    list: PgVec<'g, Need<'p>>,
    skip: &'s [usize],
    /// The expression the walk is under: the compiled root, or an outer entry a `Var` inlines.
    root: Node<'p>,
    /// Each interpreter island's plan node (by address) and its parameters (executor?, id), in its arguments' order.
    isl: PgVec<'g, (usize, PgVec<'g, (bool, i32)>)>,
}

/// The literals and parameters of `node` in `Compiled::param_needs` order; `false`: its needs are not its literals.
fn needs_of<'p>(node: Node<'p>, outer: Outer<'_, 'p>, catalog: &dyn Catalog<'p>, out: &mut Needs<'_, '_, 'p>) -> bool {
    crate::stack_check();
    match node.node_tag() {
        _ if out.skip.contains(&(node.as_raw().as_ptr() as usize)) => true,
        NodeTag::T_Const => node.as_const().map(|c| out.list.push_in(Need::Lit(c.consttype, c.constbyval, node, out.root))).is_some(),
        NodeTag::T_Param => {
            if let Some(p) = node.as_param().filter(|p| matches!(p.paramkind, ParamKind::PARAM_EXTERN | ParamKind::PARAM_EXEC)) {
                out.list.push_in(Need::Lit(p.paramtype, true, node, out.root));
            }
            true
        }
        NodeTag::T_Var => match (node.as_var(), outer) {
            (Some(v), Some((vn, list))) if v.varno == vn && v.varattno > 0 => {
                let e = list.nth(v.varattno as usize - 1);
                let e = e.as_target_entry().map_or(e, |t| t.expr);
                let root = core::mem::replace(&mut out.root, e);
                let ok = needs_of(e, None, catalog, out);
                out.root = root;
                ok
            }
            _ => true,
        },
        NodeTag::T_ScalarArrayOpExpr => {
            let Some(s) = node.as_scalar_array_op_expr() else { return false };
            if s.args.len() != 2 || !needs_of(s.args.nth(0), outer, catalog, out) {
                return false;
            }
            let arr = s.args.nth(1);
            // an ARRAY[...] operand is unrolled by the emit (scalar_array): its elements' needs, in order; any other array
            // that is no constant (a column, a call's result, a parameter, a long ARRAY[...]) is evaluated as itself and
            // walked (gravity_expr emit/scalar.rs `array_walk`): its own needs, in order
            let Some(c) = arr.as_const() else { return needs_of(arr, outer, catalog, out) };
            if c.constisnull {
                return true;
            }
            let Ok(ef) = elem_facts(catalog, c.consttype) else { return false };
            // the element count shapes the words, keyed by its class (V4R-110: gravity_expr konst.rs `list_len`, the keys'
            // reader), never baked; an image it cannot read is keyed by its bytes, and walked here as gravity_expr does
            let count = match crate::step::raw::const_list_len(c) {
                Some(n) => n,
                None => {
                    let mut scratch = [0u8; 256];
                    let Ok(mut w) = Walk::over(gravity_decompose::bake::read(c), ef, &mut Region::over(&mut scratch)) else { return false };
                    core::iter::from_fn(|| w.next()).count()
                }
            };
            out.list.push_in(Need::Elems { typ: ef.typ, byval: ef.byval, count, at: arr, root: out.root });
            // P5-02: a list past MAX_LIST is one more word, the array itself (gravity_expr emit `plan`)
            if count > gravity_program::cost::expr::MAX_LIST {
                out.list.push_in(Need::Lit(c.consttype, false, arr, out.root));
            }
            true
        }
        NodeTag::T_CoerceToDomain => {
            let Some(d) = node.as_coerce_to_domain() else { return false };
            if !needs_of(d.arg, outer, catalog, out) {
                return false;
            }
            let Ok(facts) = catalog.domain(d.resulttype) else { return false };
            facts.constraints.iter().all(|con| match con {
                DomainConstraint::Check { expr, .. } => needs_of(*expr, None, catalog, out),
                _ => true,
            })
        }
        NodeTag::T_Aggref => true,
        // RETURNING's PG 18 wrapper: gravity_expr compiles its expression (misc.rs `compile_returning`), even where it is NULL
        NodeTag::T_ReturningExpr => node.as_returning_expr().is_some_and(|r| needs_of(r.retexpr, outer, catalog, out)),
        // gravity_expr nodes/srf.rs `compile_srf` compiles no argument of a set-returning call (its value is the cell the
        // Srf step fills), so the program's needs omit them: the Srf step places its arguments' literals (M5 L0-c (2))
        NodeTag::T_FuncExpr if node.as_func_expr().is_some_and(|f| f.funcretset) => true,
        // a set-returning OpExpr is the same cell (D780: `srf_cell`, tsrf's |@| over unnest)
        NodeTag::T_OpExpr if node.as_op_expr().is_some_and(|o| o.opretset) => true,
        // D1224: gravity_expr nodes/agg.rs `compile_window_func` compiles no argument of a WindowFunc (its value is the cell
        // the window step fills, as an Aggref's is the Agg's), so the program's needs omit them: the window step places its
        // arguments' literals (M10 window.agg).
        NodeTag::T_WindowFunc => true,
        // M8 L0-d: a SubPlan is its step's value (nest.subplan evaluates its arguments and test): the program reads none of it
        NodeTag::T_SubPlan => true,
        // emit::kids stops here (an interpreter island's host call reads no literal of its subtree); its extern and executor
        // parameters are its arguments, words in its order (gravity_expr nodes/mod.rs `island`, V4R-352 / V4R-454)
        NodeTag::T_JsonExpr | NodeTag::T_JsonValueExpr | NodeTag::T_JsonIsPredicate | NodeTag::T_JsonConstructorExpr | NodeTag::T_XmlExpr | NodeTag::T_ArrayCoerceExpr => {
            // no island: a trivial ArrayCoerce over ARRAY[...] is that constructor (gravity_expr nodes/array.rs
            // `compile_array_coerce`), its needs its argument's; the others are the emit's refusal
            let Some(ids) = out.isl.iter().find(|(a, _)| *a == node.as_raw().as_ptr() as usize).map(|(_, v)| &v[..]) else {
                return node.as_array_coerce_expr().is_none_or(|a| needs_of(a.arg, outer, catalog, out));
            };
            let mut prms: PgVec<'_, Node<'p>> = PgVec::new_in(out.mcx);
            gravity_decompose::expr::walk(node, &mut |n| {
                prms.extend_in(n.as_param().filter(|q| matches!(q.paramkind, ParamKind::PARAM_EXTERN | ParamKind::PARAM_EXEC)).map(|_| n));
                true
            });
            for &(exec, id) in ids {
                let Some(q) = prms.iter().find(|n| n.as_param().is_some_and(|q| q.paramid == id && (q.paramkind == ParamKind::PARAM_EXEC) == exec)) else { return false };
                out.list.push_in(Need::Lit(q.as_param().map_or(0, |q| q.paramtype), true, *q, out.root));
            }
            true
        }
        _ => {
            let mut ok = true;
            gravity_decompose::expr::kids(node, &mut |k| ok &= needs_of(k, outer, catalog, out));
            ok
        }
    }
}

/// `c`'s parameter needs placed through `px`'s word plan: `(places, needs)`, `places[i]` where need `i` is read.
/// `skip`: subtrees the program no longer evaluates (join/hoist.rs's rewritten calls), matched by node address. `root`: the
/// expression a literal's word is filed under (`node`, or the expression a claimed operand came from).
pub(crate) fn place<'p>(px: &mut Px<'_, 'p>, c: &Compiled<'p>, node: Node<'p>, root: Node<'p>, outer: Outer<'_, 'p>, id: i32, skip: &[usize]) -> (Vec<Place>, Vec<ParamNeed>) {
    let Ok(needs) = c.param_needs(px.mcx()) else { shape(id, "the parameter needs") };
    let prm = |k: &gravity_expr::nodes::NodeId| match &c.prog.nodes[k.0 as usize] {
        gravity_expr::nodes::ExprNode::Param(q) => Some((q.kind == ParamKind::PARAM_EXEC, q.id)),
        _ => None,
    };
    let mcx = px.mcx();
    let isl = c.prog.islands.iter().map(|i| {
        let a = c.prog.sites[i.site.0 as usize].args;
        (i.src, c.prog.kids[a.start as usize..(a.start + a.len) as usize].iter().filter_map(prm).collect_in(mcx))
    });
    let mut entries = Needs { mcx, list: PgVec::new_in(mcx), skip, root, isl: isl.collect_in(mcx) };
    if !needs_of(node, outer, px.gx.catalog, &mut entries) {
        crate::not_built!(id, Variant::Kind(KindId::NestSubplan));
    }
    let mismatch = || shape(id, "an expression whose parameter needs are not its literals");
    let mut places = Vec::with_capacity(needs.len()); // alloc: Program
    for e in entries.list.iter() {
        match *e {
            Need::Lit(typ, byval, at, root) => {
                let Some(n) = needs.get(places.len()).filter(|n| !matches!(n, ParamNeed::Elem(_))) else { mismatch() };
                // D317: a literal's word is per (root, literal) occurrence, however often its root compiles
                let p = match n {
                    ParamNeed::Const(_) => px.lit_word(root, at, u32::MAX, |px| px.take_claim(at).unwrap_or_else(|| px.plan.need_at(n, typ, byval, at))),
                    _ => px.plan.need_at(n, typ, byval, at),
                };
                // M8 L0-w (V4R-344): an executor parameter is its producer's (params.rs), never a budget's
                let p = match *n {
                    ParamNeed::Exec { id: param, .. } => crate::rerun::params::place(px, id, param, p),
                    _ => p,
                };
                if p == Place::Fold && !matches!(n, ParamNeed::Const(_)) {
                    shape(id, "a parameter past the word budget");
                }
                // D684: a folded by-reference literal is its image's address, which the plan frees before the program
                if p == Place::Fold && !byval && matches!(n, ParamNeed::Const(d) if !d.isnull) {
                    shape(id, "a by-reference literal past the word budget");
                }
                places.push(p); // alloc: Program
            }
            Need::Elems { typ, byval, count, at, root } => {
                for k in 0..count {
                    let Some(n @ ParamNeed::Elem(_)) = needs.get(places.len()) else { mismatch() };
                    let p = px.lit_word(root, at, k as u32, |px| px.plan.need_elem_at(n, typ, byval, at, k as u32));
                    if p == Place::Fold && !byval {
                        shape(id, "an IN list over by-reference elements past the word budget");
                    }
                    places.push(p); // alloc: Program
                }
            }
        }
    }
    if places.len() != needs.len() {
        mismatch();
    }
    (places, needs)
}

#[cfg(test)]
mod tests {
    use datum::Datum;
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_expr::Expr;
    use gravity_lower::{Place, Sequential};
    use gravity_program::{Arena, End, Pipeline};
    use mcx::MemoryContext;
    use types_nodes::primnodes::CoercionForm;
    use types_nodes::Node;
    use types_portal::params::ParamBind;

    use crate::frame::px::Px;
    use crate::graph::Gx;

    /// D317: a root compiled twice (a top's drain re-projecting its entries) places its literal once, one word for the
    /// traversal's one occurrence; the same Const under two roots (one EC's Const shared by two clauses, exec admit's
    /// K4) with no location (no share identity, D3642) is a word per root, so a hit binding two values reads both.
    #[test]
    fn a_literal_compiled_twice_under_one_root_is_one_word() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cx = MemoryContext::new("consts");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let k = Node::mk_const(cx.mcx(), 20, -1, 0, 8, Datum::from_i64(7), false, true).expect("a const node");
        let root = |n| Node::mk_relabel_type(cx.mcx(), n, 20, -1, 0, CoercionForm::COERCE_IMPLICIT_CAST).expect("a relabel");
        let (r1, r2) = (root(k), root(k));
        let mut place = |r| {
            let c = Expr::compile_scan(crate::step::expr::test_lists(), r, &gravity_expr::PgCatalog, ParamBind::NONE, None, None).expect("compiles");
            super::place(&mut px, &c, r, r, None, 7, &[]).0
        };
        let first = place(r1);
        assert!(matches!(first[..], [Place::Word(_)]), "{first:?}");
        assert_eq!(place(r1), first, "the root's second compile reads its first word");
        let other = place(r2);
        assert!(matches!(other[..], [Place::Word(_)]) && other != first, "another root over the same Const: its own word ({other:?})");
        assert_eq!(place(r2), other);
        assert_eq!(px.plan.nwords(), 2);
    }

    /// D3642: the copies of one literal (one location, type and value: a set's arms, PG's `adjust_appendrel_attrs`) read
    /// one word under every root, each copy past the first a follower of the word plan; the same value as int4 and as
    /// int8, or at another location, keeps its own word.
    #[test]
    fn the_copies_of_one_literal_read_one_word() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use types_nodes::primnodes::Const;
        let cx = MemoryContext::new("consts");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = gravity_lower::Located::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let lit = |typ: u32, len: i32, v: Datum, location: i32| {
            Node::mk(cx.mcx(), Const { consttype: typ, consttypmod: -1, constcollid: 0, constlen: len, constvalue: v, constisnull: false, constbyval: true, location }).expect("a const node")
        };
        let root = |n| Node::mk_relabel_type(cx.mcx(), n, 20, -1, 0, CoercionForm::COERCE_IMPLICIT_CAST).expect("a relabel");
        let mut place = |r| {
            let c = Expr::compile_scan(crate::step::expr::test_lists(), r, &gravity_expr::PgCatalog, ParamBind::NONE, None, None).expect("compiles");
            super::place(&mut px, &c, r, r, None, 7, &[]).0
        };
        let copies: Vec<_> = (0..3).map(|_| place(root(lit(20, 8, Datum::from_i64(1), 40)))).collect();
        assert!(matches!(copies[0][..], [Place::Word(_)]), "{copies:?}");
        assert!(copies.iter().all(|c| *c == copies[0]), "every copy reads the first copy's word: {copies:?}");
        let int4 = place(Node::mk_relabel_type(cx.mcx(), lit(23, 4, Datum::from_i32(1), 40), 23, -1, 0, CoercionForm::COERCE_IMPLICIT_CAST).expect("a relabel"));
        assert!(matches!(int4[..], [Place::Word(_)]) && int4 != copies[0], "int4 1 beside int8 1: its own word ({int4:?})");
        let elsewhere = place(root(lit(20, 8, Datum::from_i64(1), 41)));
        assert!(matches!(elsewhere[..], [Place::Word(_)]) && elsewhere != copies[0], "another literal of one value: its own word");
        assert_eq!(px.plan.nwords(), 3);
        drop(px);
        assert_eq!(plan.followers.len(), 2, "the two copies past the first");
    }

    /// D1224: a WindowFunc is the window step's cell (gravity_expr compiles none of its arguments), so `wf(7) + 1` needs one
    /// word, the `1`, and the argument's `7` is not a need; the parent refused it "NotBuilt M8 nest.subplan".
    #[test]
    fn a_window_func_is_its_steps_cell_not_a_subplan() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use types_nodes::list::NodeList;
        use types_nodes::primnodes::{OpExpr, WindowFunc};
        let cx = MemoryContext::new("consts");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let m = cx.mcx();
        let k = |v| Node::mk_const(m, 20, -1, 0, 8, Datum::from_i64(v), false, true).expect("a const node");
        // sum(int8) OVER (), its argument the literal 7
        let wf = WindowFunc { winfnoid: 2107, wintype: 20, wincollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[k(7)]).unwrap(), aggfilter: None, runCondition: NodeList::from_slice(m, &[]).unwrap(), winref: 1, winstar: false, winagg: true, location: -1 };
        let wf = Node::mk(m, wf).expect("a window func");
        let root = Node::mk(m, OpExpr { opno: 684, opfuncid: 463, opresulttype: 20, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[wf, k(1)]).unwrap(), location: -1 }).expect("an int8pl");
        gravity_expr::nodes::func::acl_defer_begin(); // no catalog here: EXECUTE is not checked
        let c = Expr::compile_scan(crate::step::expr::test_lists(), root, &gravity_expr::PgCatalog, ParamBind::NONE, None, None).expect("compiles");
        let r = crate::catch(7, || super::place(&mut px, &c, root, root, None, 7, &[]).0);
        let places = r.unwrap_or_else(|e| panic!("a WindowFunc refused: {}", e.reason));
        assert!(matches!(places[..], [Place::Word(_)]), "one word, the 1: {places:?}");
    }

    /// D684: past the word budget (`reserve` fills it) a by-value literal still folds, its word the value, and a
    /// by-reference one refuses by shape: its fold would be the address of an image the plan frees before the program.
    #[test]
    fn a_by_reference_literal_past_the_word_budget_refuses() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cx = MemoryContext::new("consts");
        // room for the whole budget of reserved words (`limits::PARAM_WORDS`) and the vector's growth
        let arena = Arena::new(1 << 24);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        gravity_lower::WordPlan::reserve(&mut plan, gravity_program::limits::PARAM_WORDS);
        let mut px = Px::new(a, &gx, &mut plan);
        let int8 = Node::mk_const(cx.mcx(), 20, -1, 0, 8, Datum::from_i64(7), false, true).expect("a const node");
        let c = Expr::compile_scan(crate::step::expr::test_lists(), int8, &gravity_expr::PgCatalog, ParamBind::NONE, None, None).expect("compiles");
        let (places, _) = super::place(&mut px, &c, int8, int8, None, 7, &[]);
        assert!(matches!(places[..], [Place::Fold]), "a by-value literal past the budget folds: {places:?}");
        // a plain 4-byte-header varlena 'abc'
        let img: Vec<u8> = [(7u32 << 2).to_le_bytes().as_slice(), b"abc"].concat();
        let text = Node::mk_const(cx.mcx(), 25, -1, 100, -1, Datum::from_u64(img.as_ptr() as u64), false, false).expect("a const node");
        let c = Expr::compile_scan(crate::step::expr::test_lists(), text, &gravity_expr::PgCatalog, ParamBind::NONE, None, None).expect("compiles");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::place(&mut px, &c, text, text, None, 7, &[])));
        let refusal = r.expect_err("a by-reference literal past the budget refuses").downcast::<crate::admit::refusal::Refusal>().expect("a refusal");
        assert!(matches!(refusal.reason, crate::admit::refusal::Reason::Shape { .. }) && refusal.reason.to_string().contains("a by-reference literal past the word budget"), "{}", refusal.reason);
        // D1937: a protocol's words past the budget (a keyed table's, an aggregate's) refuse, the plan overflowed: the
        // reserve stops at the bound (it spun there, the backend uninterruptible)
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| px.proto_words(2)));
        assert!(r.expect_err("a protocol's words past the budget refuse").downcast::<crate::admit::refusal::Refusal>().is_ok());
        drop(px);
        assert!(plan.overflow && plan.words.len() == gravity_program::limits::PARAM_WORDS, "the plan overflowed at its bound");
    }
}
