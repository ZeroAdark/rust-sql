//! An aggregate's class kind. Hooks / levers: -. Budget 70 lines (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from args.rs by responsibility (D267, move-only): the implementation of the class kind folding an aggregate
//! (`class_of`, `class_in`: D297's class.num_varlena gate); the kind itself is props.rs's `class_kind`, the admission
//! walk's rule, so a record's slot and its class cannot disagree. Registers nothing; every fold end reads it (keyed too,
//! through plain.rs).
use gravity_decompose::ops::shared::agg::{AggDecl, Register};

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::Mcx;

use crate::face::fold::{Acc, AccShape, AnswerForm, BlockFold, FinalOrder, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::props::class_kind;
use crate::rep::CellRef;
use crate::admit::refusal::Site;

/// `a`'s transition class (`class_kind`), `NotBuilt` by the class kind's name until its file registers it.
pub(crate) fn class_of(cx: &impl Site, a: &AggDecl<'_>, mcx: Mcx<'_>) -> &'static dyn FoldClass {
    class_in(cx, a, false, mcx)
}

/// `class_of` in an end that folds num.window trees (`trees`: the plain end, plain.rs's `of_agg`) or none (keyed ends,
/// answers). D297: a numeric argument past one i64 mantissa word that no tree takes is class.num_varlena's (D830: a
/// spilled one's datum class.num_wide's, num_carry.rs `datum`). A tree takes it only while road num.window is on: with
/// the road off (`gravity_road_off`) the end folds the argument's value, which one word cannot hold (D2356).
pub(crate) fn class_in(cx: &impl Site, a: &AggDecl<'_>, trees: bool, mcx: Mcx<'_>) -> &'static dyn FoldClass {
    let trees = trees && crate::face::source::on(crate::admit::refusal::Variant::Strategy(crate::face::strategy::NUM_WINDOW));
    if crate::ops::shared::agg::sorted(a) {
        return crate::face::fold::class(cx, KindId::ClassNumVarlena); // D1210: a host-sorted DISTINCT: the hosted fold (fmgr)
    }
    let e = a.node.as_aggref().and_then(|r| r.args.iter().next()).map(|t| t.as_target_entry().map_or(t, |t| t.expr));
    // D2579: decompose spilled the sum (its term may pass one word, D830) whatever the tree reads (a computed value cast to
    // numeric(30, 18) has none): no tree takes it, so its datum folds, never the one-word read
    let past_word = e.is_some_and(|e| crate::ops::agg::numeric::strategies::num_tree::wide(e, mcx)) || crate::ops::agg::numeric::num_carry::spilled(a);
    if class_kind(a) == KindId::ClassNumPacked && past_word && !(trees && crate::ops::agg::trans::plain::of_agg(a, mcx).is_some()) {
        return crate::ops::agg::numeric::num_carry::datum(cx, a, &VARLENA);
    }
    crate::face::fold::class(cx, class_kind(a))
}

/// class.num_varlena over a decl whose register decompose chose packed (D297's wide case): each call sees the `Var`
/// register (`SumNum` -> `SumNumVar`, `AvgNum` -> `AvgNumVar`), as a decl decompose gave no scale carries.
struct Varlena(fn() -> &'static dyn FoldClass);

static VARLENA: Varlena = Varlena(|| crate::face::fold::class(&0, KindId::ClassNumVarlena));

fn var<'p>(a: &AggDecl<'p>) -> AggDecl<'p> {
    let reg = match a.reg { Register::SumNum => Register::SumNumVar, Register::AvgNum => Register::AvgNumVar, r => r };
    AggDecl { reg, ..*a }
}

impl FoldClass for Varlena {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape { (self.0)().shape(&var(a)) }
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) { (self.0)().init(b, env, &var(a), acc) }
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) { (self.0)().step(b, env, &var(a), acc, args) }
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) { (self.0)().merge(b, env, &var(a), into, from) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) { (self.0)().finish(b, env, &var(a), acc) }
    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> { (self.0)().order(&var(a)) }
    fn block(&self, a: &AggDecl<'_>) -> Option<BlockFold> { (self.0)().block(&var(a)) }
    fn answer(&self, a: &AggDecl<'_>) -> Option<AnswerForm> { (self.0)().answer(&var(a)) }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gravity_decompose::ops::shared::agg::fold_of;
    use gravity_decompose::ops::shared::agg::NONE;
    use gravity_decompose::ops::shared::agg_class::{Class, Ext, Mods, Split};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use mcx::MemoryContext;
    use types_nodes::primnodes::Aggref;
    use types_nodes::Node;

    use super::*;

    /// Records the register of every decl it is handed.
    struct Sees;

    static SEEN: Mutex<Vec<Register>> = Mutex::new(Vec::new());

    impl FoldClass for Sees {
        fn shape(&self, a: &AggDecl<'_>) -> AccShape {
            SEEN.lock().unwrap().push(a.reg);
            AccShape { words: 1, bytes: 8, align: 8 }
        }
        fn init(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, a: &AggDecl<'_>, _acc: Acc<'_>) {
            SEEN.lock().unwrap().push(a.reg);
        }
        fn step(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>, _args: &[CellRef<'_>]) {}
        fn merge(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _a: &AggDecl<'_>, _into: Acc<'_>, _from: Acc<'_>) {}
        fn finish(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>) -> (Value, Value) {
            (env.zero, env.zero)
        }
        fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
            SEEN.lock().unwrap().push(a.reg);
            None
        }
    }

    static SEES: Sees = Sees;

    /// L0-e: D297's wide case hands class.num_varlena the decl with the register rewritten (a packed sum / avg register is
    /// its `Var` one); any other register is handed on as it is; M6-FOLD R1 registers class.num_varlena (a hosted state).
    #[test]
    fn a_wide_packed_decl_reaches_the_varlena_class_with_its_var_register() {
        let ctx = MemoryContext::new("l0e");
        let coll = KeyColl { oid: 0, bytes_eq: true, bytes_order: true };
        let mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };
        let decl = |class: Class, fnoid: u32| {
            let (reg, trans, final_) = fold_of(class, fnoid, false, true);
            let node = Node::mk(ctx.mcx(), Aggref { aggfnoid: fnoid, aggtype: 1700, ..Default::default() }).unwrap();
            AggDecl { aggno: 0, fnoid, reg, slot: 1, count_slot: NONE, final_, node, scale: 2, filter: None, class, trans, mods, coll, dscale_slot: NONE, dscales: &[] }
        };
        let v = Varlena(|| &SEES);
        for (class, fnoid, var) in [(Class::SumNum, 2114, Register::SumNumVar), (Class::AvgNum, 2103, Register::AvgNumVar), (Class::Min(Ext::Num), 2146, Register::MinNum)] {
            let d = decl(class, fnoid);
            SEEN.lock().unwrap().clear();
            (v.shape(&d), v.order(&d));
            assert_eq!(*SEEN.lock().unwrap(), [var, var], "{:?}", d.reg);
            assert!(d.reg != var || var == Register::MinNum, "the caller's decl keeps its packed register");
        }
        let s = crate::catch(3, || VARLENA.shape(&decl(Class::SumNum, 2114))).expect("M6 registers class.num_varlena");
        assert_eq!((s.words, s.bytes), (0, 16), "face/aggcall.rs's hosted state slot");
    }
}
