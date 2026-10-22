//! D824's lazy build as a face another kind runs: the trait a lazy build's maker implements and its registry row. Hash
//! join registers it (ops/hashjoin/probe_wait.rs, kind join.inner); merge.join's inner runs it at INITIALIZE_INNER.
use gravity_ir::builder::FunctionBuilder;
use mcx::PgVec;

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;

/// D824's lazy build as another kind runs it (merge.join's inner at INITIALIZE_INNER, MERGE R3 (ii), V4R-494), registered as
/// `LazyImpl` beside join.inner's step maker (join/probe_wait.rs).
pub trait LazyMech: Sync {
    /// The pipelines to defer for build `b` read by pipeline `q` at its first row (producers ascending, `b` last), or `None`
    /// where running them later reorders anything PG would see (join/build_class.rs `lazy_group`). `first`: the slots of `q`
    /// whose producers PG runs before it (a merge's outer), exempt from the quiet rule.
    fn group<'g>(&self, px: &Px<'g, '_>, b: usize, q: usize, consumed: usize, slot: u8, first: &[u8]) -> Option<PgVec<'g, usize>>;
    /// The row stops the call while the group is pending (`BUILD_WAIT`) and resumes here once it ran (probe_wait.rs `wait`).
    fn wait(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, offset: i32, consumed: usize);
}

pub struct LazyImpl(pub &'static dyn LazyMech);
