//! M8's runtime protocols for v4 programs (M8-plan.md §1.4 / §3.7-§3.10, cards/M8.md L2): the protos' runtime (Init: run
//! at first read, once per execution, a once-lock at W8 / W16; Rerun: a pipeline range re-run per param change; Spool /
//! Cte: a shared spool, pulled lazily; Cache; ParallelOuter), the execution's M8 objects the kernels entries
//! (host_abi/v4/m8.rs) call through, and the copies of by-reference InitPlan / SubPlan results. Created by M8-RUNTIME.
//!
//! RT-1 builds `nest.initplan`'s Init: m8_init.rs; L0-k,
//! `src.prune`'s exec pruning over its params: m8_prune.rs. RT-1s builds `nest.spool`'s Spool (m8_spool.rs);
//! RT-2a `nest.subplan`'s SubHash and SubResult (m8_sub.rs).
use super::{Live, Prepare, Protocol};
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;
pub(crate) mod array;
pub mod hash;
pub mod initplan;
pub mod keyset;
pub(crate) mod lazy;
pub(crate) mod multi;
pub(crate) mod raw;

/// D1478: nest.subplan.rescan names the sites an EvalPlanQual recheck leaves unsubstituted (exec epq.rs); nothing to lay.
pub struct Rescan;

impl Protocol for Rescan {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Rescan
    }

    fn prepare(&self, _p: &Proto, _cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        Ok(None)
    }
}
