//! A re-opened store's lifetime: what an open does to the rows a store kept.
//! - A FunctionScan store (rerun store_read.rs asks here): which exec parameters drop its rows at an open. PG's
//!   ExecReScanFunctionScan re-calls a function whose funcparams meet the node's chgParam and keeps the other functions'
//!   tuplestores; lowered, the store watches the parameters its calls read and reruns when its owner changed one since
//!   the store's last open (`StoreRead::pass`, the seen bit of each parameter's flags word).
//! - A WindowAgg's stored rows (window.agg asks here, `rewalks`): PostgreSQL's WindowAgg spools each partition and frees
//!   it when the next one begins (release_partition); its rescan (ExecReScanWindowAgg) frees the spool and reads its child
//!   again, which a Sort or Material below keeps when no parameter it reads changed. Gravity's object keeps the fill's rows
//!   and walks them; the fill's open drops them (D2346), and a walk frees each partition it leaves (D3345). A walk that a
//!   later open runs again over the same rows (the fill kept across the reader's re-opens, the Sort's kept rows in PG) must
//!   free none: they live until the fill's next open (D3951).
use types_nodes::Node;

use crate::admit::refusal::Variant;
use crate::face::kinds::{Face, KindId};
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::srf::face::RowType;
use crate::rerun::store_read::{calls, shape, SrfExt};

/// The ScanStore's executor state shares no PARAM_EXEC (hosts v4/m5.rs), so a call reading an InitPlan's or a re-run's
/// parameter stays the nest's (M8, store_reopen): the graph's own names, which it refuses first today (props.rs). Only
/// such a call: a store under an InitPlan that reads none (oltp-043's pg_sleep inside the InitPlan itself) runs as any
/// store. D1034: an InitPlan's is passed as a re-run's is, its value read through nest.initplan's InitLoad (the Init run
/// at the first open that needs it). `store_calls`: the calls whose parameters are the store's to pass.
pub(crate) fn reopened<'p>(px: &mut Px<'_, 'p>, rt: &RowType, store_calls: &[Node<'p>]) -> (Vec<(i32, Option<u16>)>, i64) {
    match crate::rerun::params::outside(px.gx, rt.k, store_calls) {
        Some(KindId::NestInitplan | KindId::NestRerun) => reopen(px, rt),
        Some(nest) => crate::not_built!(rt.node, Variant::Kind(nest)),
        None => (Vec::new(), 0),
    }
}

/// M8 store_reopen (V4R-439): a rerun-set store's calls' exec params (an InitPlan's: D1034 first), passed at each open
/// (`StoreRead::pass`): their ids, each producer built (params.rs `need`), and its seen bit, one of 61 above `SET`/`NULL`.
fn reopen(px: &mut Px<'_, '_>, rt: &RowType) -> (Vec<(i32, Option<u16>)>, i64) {
    let ids = gravity_decompose::params::read(calls(px.gx, rt));
    ids.iter().for_each(|&id| crate::rerun::params::need(px.gx, rt.node, id));
    // D1034: an InitPlan's parameter carries its Init's handle word (params.rs `init_handle`), which its load runs
    let pars = ids.iter().map(|&id| (id, (crate::rerun::params::producer(px.gx, id) == Some(KindId::NestInitplan)).then(|| crate::rerun::params::init_handle(px, id)))).collect();
    let n = &mut px.ext_mut::<SrfExt>(Face::Srf).0;
    if *n >= 61 {
        shape(rt.node, "a re-opened store past the 61 seen bits of a param's flags word");
    }
    *n += 1;
    (pars, 1 << (*n + 1))
}

/// D3951: window reader `k`'s walk may run again over the rows its fill stored once: the walk is re-opened (a nest's or
/// SubPlan's inner, a rerun set's member: bound.rs `reopened`) and its fill does not run again before each re-open
/// (`refilled`). Fails closed: a re-opened walk whose fill this cannot show refilled keeps its rows.
pub(crate) fn rewalks(px: &Px<'_, '_>, k: usize) -> bool {
    crate::lifetime::bound::reopened(px, k) && !refilled(px.gx, crate::ops::window::lower::fill_of(px, k), k)
}

/// Fill `f` runs again at every re-open of reader `k`: `k` is a member of a rerun set, every set holding `k` holds `f`,
/// and `f` is no kept member (graph.kept: run at its set's first run only).
fn refilled(gx: &Gx<'_, '_>, f: usize, k: usize) -> bool {
    let (f, k) = (f as u16, k as u16);
    let mut sets = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&k)).peekable();
    sets.peek().is_some() && sets.all(|r| r.pipelines.contains(&f)) && !gx.graph.kept.contains(&f)
}
