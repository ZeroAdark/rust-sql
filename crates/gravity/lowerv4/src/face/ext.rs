//! The expression-extension face (plan §3.1; H14 H35), split out of step.rs (V4R-54): `ExprExtMech`, the claim an extension
//! takes over a compiled expression, and the registry walk over the extensions whose road is on. Closed at M2's exit.

use core::any::Any;
use std::rc::Rc;

use gravity_expr::Compiled;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use types_nodes::Node;

use crate::face::strategy::{self, Strategy};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::ops as registry;

/// An expression extension (M2's saop / wholerow / text.eq / text.memmem / jsonb.inplace / expr.fusion). `attach` may claim
/// sub-trees of `c` (compiled from plan node `node`), rewriting each claimed root to a placeholder `Var(Scan, EXT_AT + site)`;
/// its `Claim` names the plan nodes whose literals `c` no longer reads, and `sub(n)` compiles an operand as an expression of
/// its own (the index `emit`'s `sub` takes). `None` leaves `c` to gravity_expr. A site is emitted over the row before the
/// expression, so a claim is a sub-tree PG evaluates wherever the expression is. `at` is `(usize::MAX, _)` outside a step.
pub trait ExprExtMech: Sync {
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, c: &mut Compiled<'p>, ex: ExId, sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim>;
    /// Claimed site `site` of `ex` over `row`: `[value, valid, err]` (err: the raise word); `sub(b, env, i)` is operand `i`'s.
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, site: u32, sub: SubEmit<'_>) -> [Value; 3];
    /// The function oids this extension claims: two extensions registered on one road declare disjoint non-empty sets
    /// (ops/mod.rs `rows_are_unique`, V4R-54); an extension alone on its road may leave it empty.
    fn ops(&self) -> &'static [u32] {
        &[]
    }
    /// At each pipeline call's entry (the block dominating its window loops; V4R-73): `ex`'s per-call invariants (a
    /// constant operand decoded once) into the frame words `attach` took. A word no entry wrote reads as zero.
    fn entry(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _ex: ExId) {}
    /// A claimed filter qual's 64-row block form for this window: its guard (`None`: the window takes the row path).
    fn mask_open(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _ex: ExId) -> Option<Mask> {
        None
    }
    /// The rows of `[base, base + 64)` within `m` that pass, after a `mask_open` guard (`None` there is a Bug).
    fn mask(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value, _ex: ExId, _m: Mask) -> Option<Mask> {
        None
    }
    /// V4R-122: a `WordPlan::set` word's value, built once per execution at bind (exec admit.rs `WordTpl::Set`) for the
    /// owned object `desc` of the program's host state `owner` (its `Program::owner`, or this backend's rebuilt one, D3438)
    /// over the datum `a`: a region charged to the grant pool. `None`: this extension files none.
    fn bind_set(&self, _owner: Option<&dyn core::any::Any>, _desc: u32, _a: u64) -> Option<types_error::PgResult<gravity_driver::RegionBuf>> {
        None
    }
}

/// A claimed expression's operand `i` (`ExprExtMech::attach`'s `sub`) over the row: `[value, valid, err]`.
pub type SubEmit<'a> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, u32) -> [Value; 3];

pub struct ExprExtImpl(pub &'static dyn ExprExtMech);

/// An extension's claim: its plan, and the plan nodes it took (`Node::as_raw` addresses: their literals are not placed).
pub struct Claim {
    pub plan: Rc<dyn Any>,
    pub skip: Vec<usize>,
}

/// The `Ex::ext` of an expression an extension claimed: the extension, its plan and the `sub` operands' expressions.
pub struct ExtPlan {
    pub mech: &'static dyn ExprExtMech,
    pub plan: Rc<dyn Any>,
    pub subs: core::cell::RefCell<Vec<ExId>>,
}

impl ExtPlan {
    /// The expression's claim: its own `Ex::ext`, or (D2997) the residual claim of its per-code plan.
    pub fn of<'a>(e: &'a crate::hir::Ex<'_>) -> Option<&'a ExtPlan> {
        let x = e.ext.as_deref()?;
        x.downcast_ref().or_else(|| x.downcast_ref::<crate::dict::percode::Plan>()?.ext.get())
    }
}

/// The first placeholder attno (past `CTID_AT`): `Var(Scan, EXT_AT + site)` is an extension's claimed site `site`.
pub const EXT_AT: i16 = 1610;

/// D3875: whether `attno` is an extension's claimed site (`EXT_AT + site`): below `SHRED_AT`, whose columns are the site's
/// shred lanes (a jsonb path's, D3038's octet and char_length lanes), read as the row's other columns are.
pub fn ext_site(attno: i16) -> bool {
    (EXT_AT..crate::frame::site::SHRED_AT).contains(&attno)
}

/// Every registered extension in claim order (`ops::ext_rows`: its kind rows, then its road rows whose road is on); a
/// kind's extension claims only what needs that kind.
pub fn expr_exts() -> impl Iterator<Item = &'static dyn ExprExtMech> {
    exts_in(registry::ext_rows())
}

pub(crate) fn exts_in(rows: &'static [(Option<Strategy>, registry::Mech)]) -> impl Iterator<Item = &'static dyn ExprExtMech> {
    let on = |r: Strategy| !strategy::off().has(r) && !registry::dropped_strategies().has(r);
    rows.iter().filter(move |e| e.0.is_none_or(on)).filter_map(|e| e.1.downcast_ref::<ExprExtImpl>()).map(|e| e.0)
}

/// Every claimed expression extension's `entry` over the pipeline's expressions `exs`, in the call's entry block (V4R-73).
pub fn entries(b: &mut FunctionBuilder<'_>, env: &mut Env, exs: &[crate::hir::Ex<'_>], claimed: &[u32]) {
    for &i in claimed {
        if let Some(x) = ExtPlan::of(&exs[i as usize]) {
            x.mech.entry(b, env, ExId(i));
        }
    }
}

/// D3585: the expressions of `exs` an extension claimed (`ExtPlan::of`), in order: `entries`' list, found once per
/// program (frame/program.rs `functions`). Every pipeline's function walked every expression of the program (each arm of
/// an Append over N partitions has its own), so emitting the functions was quadratic in the arms.
pub fn claimed(exs: &[crate::hir::Ex<'_>]) -> Vec<u32> {
    exs.iter().enumerate().filter(|(_, e)| ExtPlan::of(e).is_some()).map(|(i, _)| i as u32).collect() // alloc: Program
}
