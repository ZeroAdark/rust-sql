//! The admission record (`Admitted`) and the helpers the graph admission shares (key expressions, compiled filters); the hand shape table that once lived here is deleted (09-29).

use std::sync::Arc;

use gravity_hosts::{FilterFn, HostRegistry};

use gravity_program::{Cmd, Program};

use mcx::{Mcx, PgBox};

use types_core::Oid;

use types_error::PgResult;


use types_nodes::parsenodes::RangeTblEntry;

use types_nodes::plannodes::PlannedStmt;

use types_nodes::NodeList;

use types_portal::params::{ParamBind, ParamExternData};

mod raw;
pub(crate) use raw::{compile_filter_fused, compile_prefilter, param_bind};

/// The key words an admission records (`Admitted::keys`), and the least key-expression image (`key_exprs_for`).
pub const SHAPE_PARAMS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySrc {
    Extern(usize),
    Expr(usize),
    /// A constant the shape fixed at admission (the zone walk's n and natts).
    Const(u64),
}

/// The object an EXECUTE gate's denial names (aclcheck_error's objtype), from the site that gates it, never from
/// pg_proc.prokind: nodeAgg.c ExecInitAgg checks an Aggref's aggfnoid as OBJECT_AGGREGATE; every other gate
/// (execExpr.c ExecInitFunc and its twins, nodeWindowAgg.c's winfnoid, an aggregate run as a window function) as
/// OBJECT_FUNCTION (privileges#562).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclObject {
    Function,
    Aggregate,
}

pub struct KeyExprs {
    pub stable: Box<[ParamExternData]>, // alloc: Program
    pub exprs: Vec<PgBox<'static, execexpr::ExprState<'static>>>, // alloc: Program
    pub acl_funcs: Vec<(Oid, AclObject)>, // alloc: Program
    pub filters: usize,
    /// The one-index bitmap recheck quals over these parameter slots (`sites_heap.rs` `recheck`): read through the slots
    /// as `filters` are, but not held by a parked Node (its source takes each execution's at the bind), so no park refusal.
    pub rechecks: usize,
}

pub struct Admitted {
    pub shape: &'static str,
    pub program: Arc<Program>,
    pub registry: HostRegistry,
    pub key_exprs: KeyExprs,
    pub nkparams: usize,
    pub keys: [KeySrc; SHAPE_PARAMS],
    pub cmd: Cmd,
    pub out_natts: usize,
    /// The params are the row itself (OL6: `nextval()`, `now()`), or the statement is a write (D3516: its snapshot is taken
    /// when it runs): evaluated at every Execute, under the portal's snapshot
    pub eval_at_execute: bool,
    /// A Sort root over the output (0-based output columns, ascending, NULLs
    pub sort_keys: [i16; 4],
    pub nsort: u8,
    /// The zone walk (§1.10 increment 3 step 5): the planner's `ZoneWalk` /
    pub walk: Option<Walk>,
    /// The plan tree the admission ran on (milestone-2.md §2.6 item 28): an installer decomposes and
    pub pstmt: Option<&'static PlannedStmt<'static>>,
    /// The graph admission's words (admit.rs); `None` for a shape-table admission.
    pub graph: Option<crate::admit::GraphWords>,
    /// V4R-246: this execution's extern parameters (`eval_keys`) and query environment (the entry's QueryDesc), NULL
    /// without; execution-lived, never in a program, the program cache or the content key.
    pub params: types_portal::ParamListHandle,
    pub query_env: types_portal::QueryEnvHandle,
    /// B2-31: the session TimeZone a constant of this admission was converted in (normalize `retype`'s timestamptz arms,
    /// `zone_held`), `None` when none was: the memo serves it again only under that zone (`Backend::catalog_stale`).
    pub timezone: Option<usize>,
    /// PGRCSERVE G5: the road answers it was lowered under (hosts am_rows.rs `Roads`): served again only while they stand.
    pub roads: gravity_hosts::ops::scan::am_rows::Roads,
    /// D3865: what the admission alone decides, worked out at its first execution and kept with it (runner `AdmitMemo`).
    pub(crate) memo: crate::runner::AdmitMemo,
    /// D3881: the admission context of the freed deep level it was admitted at (admit `kept`), held while it lives; last,
    /// so it drops after every field placed in it.
    pub keep: Option<std::rc::Rc<gravity_driver::mem::root::Root>>,
    /// D3865 L5: an identity admission's plan copy context (admit/entry `identity_copy`: `pstmt` is that copy), shared
    /// with the query desc that made it and held while the admission lives, so the memo and the shelf keep the copy with
    /// it under the plan's identity key; after `keep`, so it drops last.
    pub ident: Option<std::rc::Rc<gravity_driver::mem::root::Root>>,
}

/// The zone walk's plan: the key attno, the LIMIT and whether groups (`ZoneTopK`) or rows (`ZoneWalk`) are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walk {
    pub attno: i16,
    pub n: u32,
    pub groups: bool,
    /// Output columns the top-k rows carry as `part << 32 | code` words (rendered to text by the leader).
    pub codes: u32,
    /// A secondary ORDER BY key among `codes`: the leader orders ties by its bytes (`TOPK_TIES` runs).
    pub secondary: Option<u16>,
    /// The walk goes in descending key order (`TopKDesc.walk_desc`).
    pub desc: bool,
    /// A granule holding a NULL walks first (`TopKDesc.walk_nulls_first`).
    pub nulls_first: bool,
}

impl Admitted {
    pub fn sort(&self) -> &[i16] {
        &self.sort_keys[..self.nsort as usize]
    }

    /// A program runs through the producers (runner `Producers`) when it has more than one pipeline, an eligible one, or
    /// a bitmap morsel site: that site's bitmap is built by the runner (`late_words` at its pipeline's start, or at the
    /// prepare for an undriven one), so an emit-only program whose one pipeline turned serial (a pool-unsafe call in its
    /// target list) still takes the producers' path (D3051: a CREATE TABLE AS over it claimed an unbuilt bitmap).
    pub fn is_parallel(&self) -> bool {
        self.program.pipelines.len() > 1 || self.program.pipelines[0].is_eligible() || self.bitmap_site()
    }

    /// A site of the program is a bitmap morsel's (D3051): the runner builds its bitmap.
    pub fn bitmap_site(&self) -> bool {
        (0..self.program.sites.len()).any(|s| self.registry.bitmap_morsel(s).is_some())
    }

    /// The completion tag's command (the CTE-write seam): a data-modifying CTE's write (`WritePlan::cte`) tags nothing, the
    /// main statement's SELECT does (its count the rows sent: the driver counts pushes); `cmd` stays the write's.
    pub fn tag(&self) -> Cmd {
        if self.program.write.cte { Cmd::Select } else { self.cmd }
    }
}

pub(crate) fn rte<'a>(pstmt: &'a PlannedStmt<'static>, rti: u32) -> Option<&'a RangeTblEntry<'static>> {
    pstmt.rtable.nth(rti.checked_sub(1)? as usize).as_range_tbl_entry()
}

/// A key-expression image over `nparams` extern parameters (`SHAPE_PARAMS` at least).
pub(crate) fn key_exprs_for(nparams: usize) -> KeyExprs {
    let n = nparams.max(SHAPE_PARAMS);
    KeyExprs { stable: vec![ParamExternData { value: datum::Datum::null(), isnull: true, pflags: 0, ptype: 0 }; n].into_boxed_slice(), exprs: Vec::new(), acl_funcs: Vec::new(), filters: 0, rechecks: 0 } // alloc: Program
}


/// The scan's filter compiled once per plan: a qual over the heap tuple with
pub(crate) fn compile_filter(mcx: Mcx<'static>, qual: &NodeList<'static>, exprs: &mut KeyExprs) -> PgResult<Option<Option<FilterFn>>> {
    let f = compile_filter_bound(mcx, qual, param_bind(exprs))?;
    exprs.filters += usize::from(matches!(f, Some(Some(_))));
    Ok(f)
}

/// `compile_filter` over a given binding (D1356: a bitmap recheck compiled again per derived source, exec v4/m8_recheck.rs).
pub(crate) fn compile_filter_bound(mcx: Mcx<'static>, qual: &NodeList<'static>, bind: ParamBind<'static>) -> PgResult<Option<Option<FilterFn>>> {
    Ok(compile_filter_fused(mcx, qual, bind)?.map(|f| f.map(|(f, _)| f)))
}

pub(crate) fn empty_registry() -> HostRegistry {
    HostRegistry::new()
}
