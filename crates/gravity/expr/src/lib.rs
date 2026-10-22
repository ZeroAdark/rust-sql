//! Gravity's expression compiler (docs/gravity/expr-compiler.md): a planner expression tree
//! becomes a `Compiled` program with two faces, `eval` (tier 0, one row at a time) and `emit`
//! (the IR form a pipeline lowerer inlines); functions reach `gravity_fn`'s tiers.
#![allow(clippy::too_many_arguments, clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref)]

pub mod agg;
pub mod bake;
pub mod byref;
pub mod catalog;
pub mod compat;
pub mod emit;
pub mod eval;
pub mod nodes;
pub mod percode;
pub mod siphash;
pub mod views;

use core::fmt;

use types_error::{PgError, PgResult};
use types_nodes::Node;
use types_portal::params::ParamBind;

pub use catalog::{Catalog, PgCatalog};
pub use eval::{Cols, Row, Source};
pub use gravity_fn::{Datum, NullableDatum, Region, View};
pub use nodes::{ExprNode, Kids, Kind, NodeId, Program, SiteId, State, TableOid};
pub use views::Val;

/// PostgreSQL's check_stack_depth at a level of one of the expression compiler's recursions (the tree build `compile`,
/// the emitters' walks, tier 0's `eval`), each of which follows the depth of the user's expression: past max_stack_depth
/// it raises 54001 as a `PgError` panic payload, which the statement boundary passes on, never running into the thread's
/// stack end.
#[inline]
pub(crate) fn stack_check() {
    stack_depth_core::check_stack_depth_or_panic();
}

#[derive(Debug)]
pub enum CompileError {
    Unsupported(Kind),
    Pg(Box<PgError>),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Unsupported(k) => write!(f, "gravity_expr: unsupported expression node {k:?}"),
            CompileError::Pg(e) => write!(f, "{}", e.message()),
        }
    }
}

impl From<Box<PgError>> for CompileError {
    fn from(e: Box<PgError>) -> Self {
        CompileError::Pg(e)
    }
}

impl From<CompileError> for Box<PgError> {
    fn from(e: CompileError) -> Self {
        match e {
            CompileError::Pg(e) => e,
            CompileError::Unsupported(k) => Box::new( // alloc: Cold
                PgError::error(format!("gravity_expr: unsupported expression node {k:?}")) // alloc: Cold
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ),
        }
    }
}

pub type CompileResult<T> = Result<T, CompileError>;

pub struct Expr;

impl Expr {
    /// memsig E-prog: every `compile*` builds the program's lists in `m` (`Program::empty_in`), where they stay.
    pub fn compile<'mcx>(
        m: mcx::Mcx<'mcx>,
        tree: Node<'mcx>,
        catalog: &dyn Catalog<'mcx>,
        params: ParamBind<'mcx>,
    ) -> CompileResult<Compiled<'mcx>> {
        Self::compile_in(m, tree, catalog, params, None, None)
    }

    /// `compile` with every `outer.N` reference replaced by entry `N` of `outer` (a child's projection):
    /// the aggregate argument through the projection compiles over the scan's columns (lowerer-ladder §3
    /// rung 4; lower.md §6.9 need 6).
    pub fn compile_outer<'mcx>(
        m: mcx::Mcx<'mcx>,
        tree: Node<'mcx>,
        catalog: &dyn Catalog<'mcx>,
        params: ParamBind<'mcx>,
        outer: &types_nodes::list::NodeList<'mcx>,
    ) -> CompileResult<Compiled<'mcx>> {
        Self::compile_in(m, tree, catalog, params, Some((types_nodes::primnodes::OUTER_VAR, outer)), None)
    }

    /// `compile_outer` for a scan's own varno: every `Var` of `varno` is entry `N` of `rows` (a VALUES row's cells,
    /// lower/src/values.rs — an entry computed over the row is the expression over its cell nodes).
    pub fn compile_over<'mcx>(
        m: mcx::Mcx<'mcx>,
        tree: Node<'mcx>,
        catalog: &dyn Catalog<'mcx>,
        params: ParamBind<'mcx>,
        varno: i32,
        rows: &types_nodes::list::NodeList<'mcx>,
    ) -> CompileResult<Compiled<'mcx>> {
        Self::compile_in(m, tree, catalog, params, Some((varno, rows)), None)
    }

    /// `compile` over a scan: `outer` as `compile_over`, and `Var(varno, tableoid)` of `tableoid = (varno, t)` is `t`: the
    /// relation OID as an immediate (the row loop's system column that is a plan-time constant, lower/src/rows.rs), or a
    /// column of a row source holding it per row (`TableOid`).
    pub fn compile_scan<'mcx>(
        m: mcx::Mcx<'mcx>,
        tree: Node<'mcx>,
        catalog: &dyn Catalog<'mcx>,
        params: ParamBind<'mcx>,
        outer: Option<(i32, &types_nodes::list::NodeList<'mcx>)>,
        tableoid: Option<(i32, nodes::TableOid)>,
    ) -> CompileResult<Compiled<'mcx>> {
        Self::compile_in(m, tree, catalog, params, outer, tableoid)
    }

    fn compile_in<'mcx>(m: mcx::Mcx<'mcx>, tree: Node<'mcx>, catalog: &dyn Catalog<'mcx>, params: ParamBind<'mcx>, outer: Option<(i32, &types_nodes::list::NodeList<'mcx>)>, tableoid: Option<(i32, nodes::TableOid)>) -> CompileResult<Compiled<'mcx>> {
        let mut prog = Program::empty_in(m);
        let mut state = State::empty();
        let mut b = nodes::Builder { prog: &mut prog, state: &mut state, catalog, outer, tableoid, aggorder: false, returning: None, folded: false };
        let root = b.compile(tree)?;
        Ok(Compiled { prog, state, root, params, callback_arg: core::ptr::null_mut(), result_type: nodes_core::expr_type(tree) })
    }
}

pub struct Compiled<'mcx> {
    pub prog: Program<'mcx>,
    pub state: State,
    pub root: NodeId,
    pub params: ParamBind<'mcx>,
    pub callback_arg: *mut (),
    pub result_type: types_core::Oid,
}

/// A compiled program moved into a context (`Compiled::into_in`): a hosted owner's, in its product root
/// (lowerv4 `hir`). Unbound (`params` is `ParamBind::NONE`, the callback argument null): an island binds its parameters
/// per call. Tier 0 reads it through the same views as a `Compiled` (`HostedProgram::view`, `HostedState::view`).
pub struct HostedCompiled {
    pub prog: nodes::HostedProgram,
    pub state: nodes::HostedState,
    pub root: NodeId,
    pub params: ParamBind<'static>,
    pub result_type: types_core::Oid,
}

impl<'mcx> Compiled<'mcx> {
    /// This program hosted (lowerv4 `hir`): its state's lists move into `m`, the hosted owner's product root, in place of
    /// the heap (none is cloned: a function-manager call, a JSON cache and a row descriptor are not); memsig E-prog: the
    /// program's own lists stay where its compiler built them (`Program::into_hosted`: the owner's lists root). Each owned
    /// image keeps its own heap block, which `names`, `json_vars` and an IR's baked `Imm` (jsonb_codes) point into. The
    /// cells built over the state's heap lists are re-pointed by `EmitState::rebind_all`. No list is cloned and none
    /// grows: the program is read (and its state written) in place.
    ///
    /// # Safety
    /// `m` and the program's lists' context outlive the result, and the result drops before either. The result is
    /// `'static` as the hosted owner's
    /// programs were before (lowerv4 hir `finish_keeping`): a node's plan lifetime is a `Const`'s marker alone, and the
    /// by-reference constants read at run time (an island's) are in `owned`; the binding is unbound (`ParamBind::NONE`,
    /// checked: an island binds its parameters per call) and the callback argument null.
    pub unsafe fn into_in(self, m: mcx::Mcx<'static>) -> HostedCompiled {
        let p = &self.params;
        let unbound = p.extern_params.is_none() && p.exec_vals.is_none() && p.n_exec == 0 && p.param_callback.is_none() && p.param_expanded.is_none() && p.param_rw.is_none() && p.param_ro_expanded.is_none() && p.extern_fetch.is_none();
        debug_assert!(unbound && self.callback_arg.is_null(), "gravity_expr: a hosted program is compiled unbound");
        let Compiled { prog, state, root, result_type, .. } = self;
        // SAFETY: the caller's contract above (memsig E-prog: the program's lists are copied into `m` at their lengths)
        let prog = unsafe { prog.into_hosted(m) };
        let state = nodes::HostedState { calls: moved(state.calls, m), call_facts: moved(state.call_facts, m), json: moved(state.json, m), descs: moved(state.descs, m) };
        HostedCompiled { prog, state, root, params: ParamBind::NONE, result_type }
    }
}

impl<'mcx> Program<'mcx> {
    /// This program's lists moved into `m` at their exact lengths (`Compiled::into_in`'s program half): a hosted owner's
    /// product root, or (lowerv4 `hir`, D3438) a template's root, which the process-wide tier shares read-only across
    /// backends. memsig E-prog: the compiler built the lists in place in its context (`empty_in`: a lowering's lists root,
    /// which the statement owns while it lowers); the filing compacts them here (each list one allocation of its length,
    /// none of the growth's slack or freed steps), and the lists root drops after. Each owned image moves whole, so its
    /// heap block, which `names`, `json_vars` and an IR's baked `Imm` point into, stays where it is.
    ///
    /// # Safety
    /// A node's plan lifetime is a `Const`'s marker alone (`into_in`'s contract), and `m` outlives the result.
    pub unsafe fn into_hosted(self, m: mcx::Mcx<'static>) -> nodes::HostedProgram {
        // SAFETY: the caller's contract above; a lifetime-only change, same type, layout and values otherwise. The old
        // lists free into their own context (still alive: the caller drops it after) as each is compacted.
        let p = unsafe { core::mem::transmute::<Program<'mcx>, Program<'static>>(self) };
        Program {
            nodes: compacted(p.nodes, m),
            kids: compacted(p.kids, m),
            sites: compacted(p.sites, m),
            convs: compacted(p.convs, m),
            names: compacted(p.names, m),
            oids: compacted(p.oids, m),
            json_vars: compacted(p.json_vars, m),
            owned: compacted(p.owned, m),
            naggs: p.naggs,
            islands: compacted(p.islands, m),
        }
    }
}

/// memsig E-prog: a program list moved into `m` at its length (`Program::into_hosted`): `moved`'s one allocation and
/// bitwise copy; the old buffer frees into the context it was built in.
fn compacted<T>(mut v: mcx::PgVec<'static, T>, m: mcx::Mcx<'static>) -> mcx::PgVec<'static, T> {
    const { assert!(core::mem::size_of::<T>() != 0) };
    let n = v.len();
    if n == 0 {
        return mcx::PgVec::new_in(m);
    }
    let layout = core::alloc::Layout::array::<T>(n).unwrap_or_else(|_| mcx::raise(m.oom_boxed(usize::MAX)));
    let p = m.alloc_uninit_bytes(layout).unwrap_or_else(|_| mcx::raise(m.oom_boxed(layout.size()))).cast::<T>();
    // SAFETY: as `moved`: `p` has room for `n` elements; they move bitwise and `v` forgets them first (length 0, so its
    // drop frees only its buffer), so each element drops once, by the result, which owns `p` with `m` as its allocator.
    unsafe {
        v.set_len(0);
        core::ptr::copy_nonoverlapping(v.as_ptr(), p.as_ptr(), n);
        mcx::PgVec::from_raw_parts_in(p.as_ptr(), n, n, m)
    }
}

/// A list moved into `m` (`Compiled::into_in`'s state half): one allocation and one copy, through mcx's
/// out-of-line allocator: no per-element growth path, so each element type adds a few instructions, not a growth loop.
fn moved<T>(mut v: Vec<T>, m: mcx::Mcx<'static>) -> mcx::PgVec<'static, T> {
    const { assert!(core::mem::size_of::<T>() != 0) };
    let n = v.len();
    if n == 0 {
        return mcx::PgVec::new_in(m);
    }
    let layout = core::alloc::Layout::array::<T>(n).unwrap_or_else(|_| mcx::raise(m.oom_boxed(usize::MAX)));
    let p = m.alloc_uninit_bytes(layout).unwrap_or_else(|_| mcx::raise(m.oom_boxed(layout.size()))).cast::<T>();
    // SAFETY: `p` has room for `n` elements of `T` (`layout`, from `m`); the elements move bitwise and `v` forgets
    // them first (length 0, so its drop frees only its buffer), so each element drops once, by the result, which
    // owns `p` with `m` as its allocator and `T`'s array layout for its capacity.
    unsafe {
        v.set_len(0);
        core::ptr::copy_nonoverlapping(v.as_ptr(), p.as_ptr(), n);
        mcx::PgVec::from_raw_parts_in(p.as_ptr(), n, n, m)
    }
}

impl<'mcx> Compiled<'mcx> {
    /// Tier 0 over one row; a by-reference result is an image in `region` until the caller resets it.
    pub fn eval(&mut self, row: &Row<'_>, region: &mut Region) -> PgResult<NullableDatum> {
        let mut cx = eval::Cx::new(row, region, &self.params, self.callback_arg);
        eval::eval(&self.prog.view(), &mut self.state.view(), self.root, &mut cx)
    }

    pub fn arm_param_callback_arg(&mut self, arg: *mut ()) {
        self.callback_arg = arg;
    }

    pub fn node(&self, id: NodeId) -> &ExprNode<'mcx> {
        &self.prog.nodes[id.0 as usize]
    }

    pub fn children(&self, k: Kids) -> &[NodeId] {
        &self.prog.kids[k.start as usize..k.start as usize + k.len as usize]
    }

    pub fn kinds(&self) -> impl Iterator<Item = Kind> + '_ {
        self.prog.nodes.iter().map(|n| n.kind())
    }

    /// The aggregate cells the program reads: `Row::aggs` / `RowRefs::aggs` hold at least this many.
    pub fn naggs(&self) -> usize {
        self.prog.naggs as usize
    }

    pub fn reset_window(&mut self) {
        self.state.reset_window();
    }
}
