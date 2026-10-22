//! M5's per-statement protocol (M5-plan.md §6, cards/M5.md L2), `ProtoKind::SetFn` (kernels host_abi v4/m5.rs
//! `setfn_arg` / `SETFN_*`). Created by M5-RIG (owner M5-HOSTS). Per execution of a srf step (H-f): its slots of
//! `gravity_fn::srf`'s bank at this re-entry level are reserved (an idle cursor and the next statement at the same level
//! never share a slot context) and reset and released at the statement's end or error (the runner drops this state on
//! every path); its cells live until then (a resume never re-opens); its store (H-c/H-d/H-e, hosts v4/m5.rs) ends then.
use std::thread::ThreadId;

use gravity_fn::srf_fmgr::Sets as FmgrSets;
use gravity_hosts::ops::srf::store::ScanStore;
use gravity_kernels::ops::srf::entries::{setfn_parts, SETFN_WORDS, SETFN_W_BASE, SETFN_W_CELLS, SETFN_W_NODE};
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;

use crate::ops::{Live, Prepare, Protocol};
use crate::admit::{Admit, Bind};
use crate::glue;

pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn Protocol] = &[&SetFn];

pub struct SetFn;

impl Protocol for SetFn {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SetFn
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        if p.words.n != SETFN_WORDS || base + SETFN_WORDS as usize > cx.words.len() {
            return Err(glue::internal("gravity: srf's words are not its declaration's"));
        }
        let (nslots, cell_bytes, node) = setfn_parts(p.arg);
        let mut host = match node {
            Some(id) => {
                let pstmt = cx.admitted.pstmt.ok_or_else(|| glue::internal("gravity: srf's node without its plan"))?;
                let plan = plan_node(pstmt, id as i32).ok_or_else(|| glue::internal("gravity: srf's node is not in the plan"))?;
                // H-b: a ProjectSet's fmgr sets; V4R-246: this execution's extern parameters (`eval_keys`) and query
                // environment (the entry's QueryDesc) for a store scan
                let h = FmgrSets::new(plan).map(Host::Fmgr).or_else(|| ScanStore::new(pstmt, plan, cx.admitted.params, cx.admitted.query_env).map(Host::Store));
                Some(h.ok_or_else(|| glue::internal("gravity: srf's node is no ProjectSet, FunctionScan, TableFuncScan or NamedTuplestoreScan"))?)
            }
            None => None,
        };
        let (level, first) = gravity_fn::srf::reserve_slots(nslots);
        // 64-aligned inside a zeroed word buffer: 7 spare words cover any base address
        let mut cells = vec![0u64; cell_bytes.div_ceil(8) + 7]; // alloc: Exec
        let skip = cells.as_ptr().align_offset(64);
        cx.words[base + SETFN_W_BASE] = first as u64;
        let fmgr = match &mut host {
            Some(Host::Fmgr(s)) => &mut **s as *mut FmgrSets,
            _ => core::ptr::null_mut(),
        };
        cx.words[base + SETFN_W_NODE] = match &mut host {
            Some(Host::Fmgr(_)) => fmgr as u64,
            Some(Host::Store(s)) => s.handle(),
            None => 0,
        };
        cx.words[base + SETFN_W_CELLS] = cells[skip..].as_mut_ptr() as u64;
        cx.nulls[base..base + SETFN_WORDS as usize].fill(0);
        *cx.nparams = (*cx.nparams).max(base + SETFN_WORDS as usize);
        Ok(Some(Box::new(Sets { level, first, nslots, thread: std::thread::current().id(), _cells: cells, fmgr, _host: host }))) // alloc: Exec
    }
}

/// The step's node: a ProjectSet's fmgr sets (H-b) or a store scan (H-c / H-d / H-e), ended with its `Sets`.
enum Host {
    Fmgr(Box<FmgrSets>),
    Store(Box<ScanStore>),
}

/// One execution's srf step: its reserved slots, its cells and its node's host.
struct Sets {
    level: usize,
    first: usize,
    nslots: usize,
    thread: ThreadId,
    _cells: Vec<u64>,
    /// `_host`'s fmgr sets, the handle the program reads (`SETFN_W_NODE`); null for a store scan.
    fmgr: *mut FmgrSets,
    _host: Option<Host>,
}

impl Live for Sets {
    fn answers(&self) -> u16 {
        crate::ops::ask::END
    }

    /// A set the reader stopped inside ends as `ShutdownSetExpr` ends it at ExecutorEnd (its srf_shutdown hook).
    fn end(&self) -> PgResult<()> {
        crate::ops::srf::raw::fmgr_sets(self.fmgr).map_or(Ok(()), |s| s.end())
    }
}

impl Drop for Sets {
    fn drop(&mut self) {
        // the bank and the reservations are the preparing thread's; a drop elsewhere must not touch that thread's peers
        if std::thread::current().id() == self.thread {
            gravity_fn::srf::release_slots(self.level, self.first, self.nslots);
        }
    }
}

/// The plan node numbered `id` in the statement (the main tree or a subplan).
fn plan_node(pstmt: &'static PlannedStmt<'static>, id: i32) -> Option<Node<'static>> {
    fn under(n: Option<Node<'static>>, id: i32) -> Option<Node<'static>> {
        stack_depth_core::check_stack_depth_or_panic();
        let n = n?;
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(n);
        }
        let kids = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, id).or_else(|| under(p.righttree, id)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), id)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, id)))
    }
    under(pstmt.planTree, id).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, id)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn live_executions_at_one_level_hold_disjoint_slots_and_release_them() {
        use gravity_fn::srf::{at_level, release_slots, reserve_slots};
        at_level(5, || {
            let (a, b) = (reserve_slots(2), reserve_slots(3));
            assert_eq!((a, b), ((5, 0), (5, 2)), "a second live execution takes the slots past the first's");
            assert_eq!(at_level(6, || reserve_slots(1)), (6, 0), "another level numbers its own bank");
            release_slots(5, a.1, 2);
            assert_eq!(reserve_slots(1), (5, 0), "a released range is reused first-fit");
            assert_eq!(reserve_slots(2), (5, 5), "a range that does not fit the hole goes past the held ones");
            assert_eq!(reserve_slots(0), (5, 0), "a step with no slots holds none");
        });
    }
}
