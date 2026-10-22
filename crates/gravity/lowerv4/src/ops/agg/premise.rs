//! agg's premises (v5 landing 4, moved from props/group.rs and props/code.rs): a group's keys and aggregates, their class
//! kinds, and whether a Unique group is a DISTINCT ON. The props walk calls `group` per group.
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl, GroupForm, Register};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::Hint;
use gravity_decompose::catalog::numeric::{is_div, negation, numeric_type, F_NUMERIC_ADD, F_NUMERIC_MUL, F_NUMERIC_SUB};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_core::catalog::{INT2OID, INT4OID, INT8OID, NUMERICOID};
use types_nodes::{Node, NodeTag};

use crate::applies::Prop;
use crate::face::kinds::KindId;
use crate::graph::{emits_groups, group_kind, keyed};
use crate::rep::{key_kind, Accessor};

use crate::props::code::last_project;
use crate::props::{class_kind, hosted_group_key, Walk};

impl<'p> Walk<'_, '_, 'p> {
    /// Whether a Unique group is a DISTINCT ON: fewer keys than its input's (non-junk) columns. A SELECT DISTINCT keys
    /// every column.
    pub(crate) fn distinct_on(&self, g: &GroupDecl<'p>) -> bool {
        let Some(p) = self.gx.pipelines().get(g.producer.0 as usize) else { return false };
        let Some(l) = last_project(p) else { return false };
        let cols = l.iter().filter_map(|n| n.as_target_entry()).filter(|t| !t.resjunk).count();
        g.keys.len() < cols
    }

    pub(crate) fn group(&mut self, g: &GroupDecl<'p>) {
        self.node = g.node_id;
        self.add(group_kind(g));
        if g.form == GroupForm::Unique && self.distinct_on(g) {
            self.p.add(Prop::DistinctOn);
        }
        let keyed = keyed(g);
        // V4R-128: a sorted group / Unique's form K (runs/group.rs) hashes its keys as a keyed group does and sorts the
        // records by them: each key's hash arm and its order arm
        let sorted = matches!(group_kind(g), KindId::RunsGroup | KindId::RunsUnique) && crate::graph::grouped(g);
        if g.two_pass.is_some() {
            self.add(KindId::KeyedTwopass);
        }
        if g.set.is_some() {
            self.add(KindId::KeyedGsets);
        }
        if emits_groups(g) {
            self.add(KindId::KeyedEmitGroup);
        }
        if g.hints.iter().any(|h| matches!(h, Some(Hint::ZoneTopK { .. }))) {
            // V4R-8: an unordered grouped LIMIT the planner walks (ZoneTopK) is walkable, like a zonewalk sort.
            self.p.add(Prop::Walk);
        }
        let packed = self.p.has(Prop::Packed);
        let producer = self.gx.pipelines().get(g.producer.0 as usize);
        for key in g.keys.iter() {
            if keyed || sorted {
                let code = producer.is_some_and(|p| self.code_expr(p, key.expr.node(), last_project(p)));
                let acc: &[Accessor] = if sorted { &[Accessor::Hash, Accessor::Order] } else { &[Accessor::Hash] };
                for &a in acc {
                    self.add(if code {
                        if a == Accessor::Hash { KindId::RepCodeHash } else { KindId::RepCodeOrder }
                    } else if hosted_group_key(g, key) {
                        if a == Accessor::Hash { KindId::RepHostedHash } else { KindId::RepHostedOrderFixed } // a tid: fixed by-reference
                    } else if let Some(k) = fixed_hash(self.gx.facts, key.tf).filter(|_| a == Accessor::Hash) {
                        k
                    } else {
                        // M6-CLOSE (a', main 09-28): a group key over a packed lane keys by rep.packed.order's word (runs/copy_words.rs
                        // packed_cell: one scale, else refused by name); joins key through rep.packed.hash (the varlena Fp128),
                        // and so does the scale-free numeric key (D3848, keysem `scale_free`) and a set side's packed key the
                        // other side holds in another form (D3857, keyimg.rs `packed_set_key`)
                        match key_kind(key.kind, Some(key.image), packed, a) {
                            KindId::RepPackedHash if !gravity_decompose::catalog::keysem::image::scale_free(key) && !crate::table::keyimg::packed_set_key(self.gx, g.producer.0 as usize, key) => KindId::RepPackedOrder,
                            k => k,
                        }
                    });
                }
            }
            let e = key.expr.node();
            if !matches!(e.node_tag(), NodeTag::T_Var) {
                self.p.add(Prop::ExprKey);
                if self.named(e, &["mod", "div"]) {
                    self.p.add(Prop::ModKey);
                }
            }
            self.expr(e);
        }
        for a in g.aggs.iter() {
            if let Some(f) = a.filter {
                self.add(KindId::FoldFilter);
                self.expr(f);
            }
            if matches!(a.class, Class::CountStar | Class::CountAny | Class::CountPair) {
                self.p.add(Prop::Count);
            }
            if matches!(a.class, Class::Min(_) | Class::Max(_)) {
                self.minmax_expr(a.node);
            }
            // an ORDER BY inside the call (not WITHIN GROUP): fold/plain.rs and the keyed ends refuse runs.group (D346)
            if a.mods.order > 0 && !matches!(a.class, Class::OrderedSet(_) | Class::Hypothetical(_)) {
                self.add(KindId::RunsGroup);
            }
            let class = fold_kind(g, a, self.gx.mcx); // D396: class_of.rs's wide-packed case
            self.add(class);
            self.expr(a.node);
        }
    }

    /// A min / max whose argument is a call sets `minmax_expr`.
    pub(crate) fn minmax_expr(&mut self, agg: Node<'p>) {
        let arg = agg.as_aggref().and_then(|a| a.args.iter().next()).and_then(|t| t.as_target_entry().map(|t| t.expr));
        if arg.is_some_and(|e| matches!(e.node_tag(), NodeTag::T_FuncExpr | NodeTag::T_OpExpr)) {
            self.p.add(Prop::MinmaxExpr);
        }
    }
}

/// A group key over a by-reference fixed-width type (name, macaddr, point; uuid / interval / timetz natively): its cell is
/// `FixedPtr`, so the keyed image hashes it through `rep::arm_kind` (keyimg -> rep::hash_image): rep.fixed.hash for a native
/// fixed image, else rep.hosted.hash (a `name`: regress opr_sanity-040), where `key_kind` reads its key image (D387's class).
fn fixed_hash(facts: &dyn gravity_decompose::catalog::Facts, typid: u32) -> Option<KindId> {
    let tf = gravity_decompose::catalog::typefacts::facts_of(facts, typid)?;
    let rep = gravity_program::Rep::of_storage(tf.typlen, tf.byval);
    matches!(rep, gravity_program::Rep::FixedPtr { .. }).then(|| crate::rep::arm_kind(rep, Accessor::Hash, &tf))
}

/// D396: the class kind that folds `a` in group `g`'s end, as fold/class_of.rs `class_in` picks it (L0-e): D297's wide
/// case, a packed numeric sum / avg whose argument's tree passes one mantissa word, is class.num_varlena's, unless the
/// end folds num.window trees (a plain group's; V4R-38: a Hashed or Sorted one's, num_keyed.rs `forms`) and a tree takes
/// it (fold/plain.rs `of_agg`). The numeric tree's digit bound is fold/num_tree.rs's, read here from the plan (props
/// reaches no milestone file).
pub(crate) fn fold_kind(g: &GroupDecl<'_>, a: &AggDecl<'_>, mcx: Mcx<'_>) -> KindId {
    let k = class_kind(a);
    let e = a.node.as_aggref().and_then(|r| r.args.iter().next()).map(|t| t.as_target_entry().map_or(t, |t| t.expr));
    let wide = k == KindId::ClassNumPacked && e.and_then(|e| num_tree(e, false, true, mcx)).is_some_and(|t| t.digits > NUM_NARROW);
    let road = crate::face::source::on(crate::admit::refusal::Variant::Strategy(crate::face::strategy::NUM_WINDOW)); // class_in's
    let trees = road && ((g.form == GroupForm::Plain && !crate::graph::keyed(g)) || crate::ops::agg::numeric::strategies::num_keyed::forms(g, mcx));
    // D830: a spilled sum's datum folds in class.num_wide, with a dscale word in class.num_packed (V4R-549; num_carry.rs `datum`)
    let spilled = matches!(a.trans, gravity_decompose::ops::shared::agg_class::Trans::Abi(gravity_abi::breaker::SlotKind::SumNumWide | gravity_abi::breaker::SlotKind::AvgNumWide));
    let datum = if !spilled { KindId::ClassNumVarlena } else if a.dscale_slot == gravity_decompose::ops::shared::agg::NONE { KindId::ClassNumWide } else { KindId::ClassNumPacked };
    // D2579: a spilled packed sum's term may pass one word whatever its tree (a computed value cast to numeric(30, 18) has
    // none): no tree takes it, so its datum folds, never the one-word read
    if (wide || (spilled && k == KindId::ClassNumPacked)) && !(trees && num_of_agg(a, mcx)) { datum } else { k }
}

const NUM_NARROW: u32 = 18;

/// fold/num_tree.rs's node facts: scale, digits bound, and a root CASE's arm scales (`None`: a NULL arm).
struct NumTree<'m> {
    scale: i16,
    digits: u32,
    mul_one: bool,
    case: Option<(PgVec<'m, Option<i16>>, bool)>,
    /// D4567 (a): a root division (num_tree.rs `Tree::Div`), its display scale the dscale word's.
    div: bool,
}

fn num_leaf<'m>(scale: i16, digits: u32) -> NumTree<'m> {
    NumTree { scale, digits, mul_one: true, case: None, div: false }
}
/// num_tree.rs `bounded` (37 digits, a product with one side one word); num_const.rs `digits_i64`.
fn num_bounded(t: &NumTree) -> bool {
    t.mul_one && t.digits <= 37
}
fn num_digits_i64(m: i64) -> u32 {
    (m.unsigned_abs().checked_ilog10().unwrap_or(0) + 1).max(1)
}

/// fold/num_const.rs `const_of`: a numeric constant past one mantissa word (D2356) strict only within an i128 and the bound.
fn num_const<'m>(c: &types_nodes::primnodes::Const, strict: bool) -> Option<NumTree<'m>> {
    if c.constisnull {
        return None;
    }
    let m = match c.consttype {
        INT8OID => gravity_decompose::bake::read(c).as_usize() as i64,
        INT2OID | INT4OID => gravity_decompose::bake::read(c).as_i32() as i64,
        NUMERICOID => {
            let n = gravity_fn::typesem::numeric::mantissa(gravity_decompose::expr::const_bytes(c)?)?;
            let scale = i16::try_from(n.dscale).ok().filter(|&s| s <= 18)?;
            let leaf = num_leaf(scale, n.digits);
            return (n.word().is_some() || !strict || (n.value.is_some() && num_bounded(&leaf))).then_some(leaf);
        }
        _ => return None,
    };
    Some(num_leaf(0, num_digits_i64(m)))
}

fn num_null(r: Node<'_>) -> bool {
    crate::stack_check();
    r.as_relabel_type().map_or_else(|| r.as_const().is_some_and(|k| k.constisnull), |x| num_null(x.arg))
}

fn num_bare_var(e: Node<'_>) -> bool {
    crate::stack_check();
    match e.node_tag() {
        NodeTag::T_Var => e.as_var().is_some_and(|v| v.varattno > 0 && v.varno != types_nodes::primnodes::INNER_VAR),
        NodeTag::T_RelabelType => e.as_relabel_type().is_some_and(|r| num_bare_var(r.arg)),
        _ => false,
    }
}

/// fold/num_tree.rs `tree` (with num_case.rs `compile` for a searched CASE or a COALESCE): `strict` every node bounded and a column one
/// word, else the shape only; `root` a CASE there may mix arm scales.
fn num_tree<'m>(e: Node<'_>, strict: bool, root: bool, mcx: Mcx<'m>) -> Option<NumTree<'m>> {
    crate::stack_check();
    if let Some(r) = e.as_relabel_type() {
        return num_tree(r.arg, strict, root, mcx);
    }
    let (case, coalesce) = (e.as_case_expr(), e.as_coalesce_expr());
    if case.is_some() || coalesce.is_some() {
        let mut arms: PgVec<'_, Option<NumTree<'m>>> = PgVec::new_in(mcx);
        let mut arm = |r: Option<Node<'_>>| -> Option<()> {
            match r.filter(|r| !num_null(*r)) {
                Some(r) => arms.push_in(Some(num_tree(r, strict, false, mcx)?)),
                None => arms.push_in(None),
            }
            Some(())
        };
        // PS4-17: a COALESCE's arms are its arguments (num_case.rs)
        match (case, coalesce) {
            (Some(c), _) => {
                if c.arg.is_some() {
                    return None;
                }
                for w in c.args.iter() {
                    let w = w.as_case_when()?;
                    w.expr?;
                    arm(w.result)?;
                }
                arm(c.defresult)?;
            }
            (None, Some(c)) => c.args.iter().try_for_each(|a| arm(Some(a)))?,
            (None, None) => return None,
        }
        let scale = arms.iter().flatten().map(|t| t.scale).max()?;
        let digits = arms.iter().flatten().map(|t| t.digits + (scale - t.scale) as u32).max()?;
        let mixed = arms.iter().flatten().any(|t| t.scale != scale);
        if mixed && !root {
            return None;
        }
        let t = NumTree { scale, digits, mul_one: true, case: Some((arms.iter().map(|a| a.as_ref().map(|t| t.scale)).collect_in(mcx), mixed)), div: false };
        return (num_bounded(&t) || !strict).then_some(t);
    }
    if let Some(v) = e.as_var() {
        if !num_bare_var(e) {
            return None;
        }
        let t = u32::try_from(v.vartypmod.checked_sub(4)?).ok()?;
        let (scale, digits) = ((t & 0xffff) as i16, t >> 16);
        return (numeric_type(v.vartype) && scale <= 18 && (digits <= NUM_NARROW || !strict)).then(|| num_leaf(scale, digits));
    }
    if let Some(c) = e.as_const() {
        return num_const(c, strict);
    }
    if let Some(t) = e.as_func_expr().and_then(|f| num_cast(f, strict, mcx)) {
        return Some(t);
    }
    if let Some(f) = e.as_func_expr() {
        let a = f.args.iter().next().filter(|_| types_core::cast::exact_to_numeric(f.funcid).is_some() && f.args.len() == 1)?;
        if let Some(c) = a.as_const() {
            return num_const(c, strict);
        }
        let digits = types_core::cast::int_digits(a.as_var()?.vartype)?;
        return num_bare_var(a).then(|| num_leaf(0, digits));
    }
    if let Some(op) = e.as_op_expr().filter(|o| negation(o.opfuncid) && o.args.len() == 1) {
        // D830: -x as fold/num_tree.rs's 0 - x
        let x = num_tree(op.args.iter().next()?, strict, false, mcx)?;
        let t = num_leaf(x.scale, x.digits + 1);
        return (num_bounded(&t) || !strict).then_some(t);
    }
    // D4567 (a): num_tree.rs `div`: x / k at the root alone
    if let Some(op) = e.as_op_expr().filter(|o| is_div(o.opfuncid) && o.args.len() == 2) {
        if !root {
            return None;
        }
        use gravity_decompose::catalog::numeric::{div_scales, exact_reciprocal};
        let x = num_tree(op.args.iter().next()?, strict, false, mcx).filter(|x| x.digits <= NUM_NARROW)?;
        let (km, ks) = gravity_decompose::ops::agg::div_const(op.args.iter().nth(1)?)?;
        let (c, e) = exact_reciprocal(km, ks)?;
        let s = u32::try_from(x.scale).ok()?;
        let d = div_scales(s, x.digits, km, ks)?;
        let at = s + e;
        let digits = x.digits + c.unsigned_abs().checked_ilog10().unwrap_or(0) + 1;
        return (d.min() >= at && at <= 18 && digits <= NUM_NARROW).then_some(NumTree { scale: at as i16, digits, mul_one: true, case: None, div: true });
    }
    let op = e.as_op_expr().filter(|o| o.args.len() == 2)?;
    // D4567: num_tree.rs `fixed_beside`: under + / - one side a CASE of mixed arm scales beside one of a scale at least
    // its every arm's
    let add = gravity_decompose::catalog::numeric::sum_or_difference(op.opfuncid);
    let (x, y) = (num_tree(op.args.iter().next()?, strict, add, mcx)?, num_tree(op.args.iter().nth(1)?, strict, add, mcx)?);
    let mixed = |t: &NumTree| t.case.as_ref().is_some_and(|c| c.1);
    let fixed = match (mixed(&x), mixed(&y)) {
        (true, true) => false,
        (true, false) => y.scale >= x.scale,
        (false, true) => x.scale >= y.scale,
        (false, false) => true,
    };
    if (add && !fixed) || x.div || y.div {
        return None;
    }
    let t = match op.opfuncid {
        F_NUMERIC_MUL => NumTree { scale: x.scale + y.scale, digits: x.digits + y.digits, mul_one: x.digits <= NUM_NARROW || y.digits <= NUM_NARROW, case: None, div: false },
        F_NUMERIC_ADD | F_NUMERIC_SUB => {
            let s = x.scale.max(y.scale);
            num_leaf(s, (x.digits + (s - x.scale) as u32).max(y.digits + (s - y.scale) as u32) + 1)
        }
        _ => return None,
    };
    (num_bounded(&t) || !strict).then_some(t)
}

/// fold/num_tree.rs `typmod_cast` (PS4-21): `x::numeric(p, s)` of a one-word tree, 1 <= p <= 18 and 0 <= s <= 18, is p
/// digits at scale s.
fn num_cast<'m>(f: &types_nodes::primnodes::FuncExpr<'_>, strict: bool, mcx: Mcx<'m>) -> Option<NumTree<'m>> {
    let k = f.args.iter().nth(1).and_then(|k| k.as_const()).filter(|k| gravity_decompose::catalog::casts::numeric_typmod_cast(f.funcid) && f.args.len() == 2 && !k.constisnull && k.constbyval)?;
    let (digits, scale) = gravity_fn::typesem::numeric::typmod_parts(k.constvalue.as_i32()).filter(|&(p, s)| (1..=NUM_NARROW as i32).contains(&p) && (0..=18).contains(&s))?;
    num_tree(f.args.iter().next()?, strict, false, mcx).filter(|x| x.digits <= NUM_NARROW)?;
    Some(num_leaf(scale as i16, digits as u32))
}

/// fold/plain.rs `of_agg`: a num.window tree takes a sum / avg's argument at its scale, a CASE's arm scales agreeing
/// with decompose's dscales.
fn num_of_agg(a: &AggDecl<'_>, mcx: Mcx<'_>) -> bool {
    use gravity_decompose::ops::shared::agg::{NONE, NULL_ARM};
    let Some(r) = a.node.as_aggref().filter(|r| r.args.len() == 1 && matches!(a.reg, Register::SumNum | Register::AvgNum)) else { return false };
    let Some(e) = r.args.iter().next().map(|t| t.as_target_entry().map_or(t, |t| t.expr)) else { return false };
    let Some(t) = num_tree(e, true, true, mcx).filter(|t| t.scale == a.scale && (0..=18).contains(&a.scale)) else { return false };
    let ds = a.dscale_slot != NONE;
    match (&t.case, ds) {
        (Some((arms, _)), true) => arms.len() <= a.dscales.len() && arms.iter().zip(a.dscales.iter()).all(|(x, d)| x.map_or(NULL_ARM, |s| s as u8) == *d),
        (Some((_, mixed)), false) => !mixed,
        (None, ds) => ds == t.div,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{FuncExpr, OpExpr};

    /// Does not apply: `fold_kind`'s copy of the numeric tree gates on the same catalog names (numeric_uminus,
    /// numeric_add / sub / mul, the numeric type's typmod, the exact casts to numeric, int2 / int4 / int8 / numeric
    /// constants), so every near form declines, strict or not, and its sum / avg keeps the class its kind gives (a packed
    /// argument past one word then folds as a datum): a one-argument numeric operator that is not the negation
    /// (numeric_uplus, numeric_abs), another type's negation (float8um, int4um), numeric_div and numeric_mod, a column of
    /// another type at a numeric typmod (a domain over numeric), a numeric column with no typmod or a scale past 18, a cast
    /// that is not exact (float8 -> numeric) or not to numeric, and a constant of another type, a NULL, a NaN or one past
    /// scale 18. The named forms beside them take their scale and digits.
    #[test]
    fn the_premise_tree_declines_every_near_form() {
        let cx = mcx::MemoryContext::new("numeric premise names");
        let mx = cx.mcx();
        let lits = ["12.5", "NaN", "0.0000000000000000001", "3"];
        let imgs: Vec<_> = lits.iter().map(|t| adt_numeric::numeric_in(t, -1, None).unwrap().unwrap()).collect();
        let num = |i: usize| Node::mk_const(mx, 1700, -1, 0, -1, datum::Datum::from_usize(imgs[i].as_bytes().as_ptr() as usize), false, false).expect("a const");
        let typmod = |p: i32, s: i32| ((p << 16) | s) + 4;
        let var = |t: u32, tm: i32| Node::mk_var(mx, 1, 1, t, tm, 0, 0).expect("a var");
        let op = |f: u32, args: &[_]| {
            let mut l = NodeList::nil();
            for a in args {
                l.lappend(mx, *a).expect("args");
            }
            Node::mk(mx, OpExpr { opfuncid: f, args: l, ..Default::default() }).expect("an op")
        };
        let cast = |f: u32, x| Node::mk(mx, FuncExpr { funcid: f, args: NodeList::make1(mx, x).expect("args"), ..Default::default() }).expect("a cast");
        let n10_2 = || var(1700, typmod(10, 2));
        let declines = |e, what: &str| {
            assert!(num_tree(e, false, true, mx).is_none(), "{what}: the shape walk takes it");
            assert!(num_tree(e, true, true, mx).is_none(), "{what}: the strict tree takes it");
        };
        let facts = |e| num_tree(e, true, true, mx).map(|t| (t.scale, t.digits));
        // the named forms
        assert_eq!(facts(n10_2()), Some((2, 10)), "numeric(10, 2)");
        assert_eq!(facts(op(1771, &[n10_2()])), Some((2, 11)), "numeric_uminus: 0 - x");
        assert_eq!(facts(cast(1740, var(23, -1))), Some((0, 10)), "int4 -> numeric");
        assert_eq!(facts(op(1726, &[n10_2(), num(0)])), Some((3, 13)), "numeric_mul");
        assert_eq!(facts(op(1724, &[n10_2(), num(0)])), Some((2, 11)), "numeric_add");
        let k4 = Node::mk_const(mx, 23, -1, 0, 4, datum::Datum::from_i32(7), false, true).expect("an int4 const");
        assert_eq!(facts(k4), Some((0, 1)), "an int4 constant");
        assert_eq!(facts(num(3)), Some((0, 1)), "a numeric constant");
        // their near forms
        declines(op(1915, &[n10_2()]), "numeric_uplus");
        declines(op(1704, &[n10_2()]), "numeric_abs");
        declines(op(220, &[var(701, -1)]), "float8um");
        declines(op(212, &[var(23, -1)]), "int4um");
        declines(op(1727, &[n10_2(), n10_2()]), "numeric_div");
        declines(op(1729, &[n10_2(), n10_2()]), "numeric_mod");
        declines(var(16_500, typmod(10, 2)), "a domain over numeric(10, 2)");
        declines(var(701, typmod(10, 2)), "a float8 column");
        declines(var(1700, -1), "numeric with no typmod");
        declines(var(1700, typmod(30, 20)), "numeric(30, 20)");
        declines(cast(1743, var(701, -1)), "float8 -> numeric");
        declines(cast(1744, n10_2()), "numeric -> int4");
        let k8 = Node::mk_const(mx, 701, -1, 0, 8, datum::Datum::from_usize(1.5f64.to_bits() as usize), false, true).expect("a float8 const");
        declines(k8, "a float8 constant");
        declines(Node::mk_const(mx, 1700, -1, 0, -1, datum::Datum::from_usize(0), true, false).expect("a NULL"), "a NULL numeric");
        declines(num(1), "NaN");
        declines(num(2), "a constant at scale 19");
    }
}
