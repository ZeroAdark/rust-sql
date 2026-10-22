//! `nest.subplan`'s LAZY build (RT-2c, V4R-369; a #[path] child of m8_sub.rs): a hashed SubPlan whose build may raise or
//! holds a VOLATILE call builds where PG builds it, at the first probe (`ExecHashSubPlan` -> `buildSubPlanHash`). Its
//! build pipeline and that pipeline's exclusive producers are its members, stepped past in the producer order
//! (`Live::deferred`) and run by the first `m8_subhash_run` through the runner's frame (Init's road, m8_init.rs), once per
//! execution. A set never probed never builds, and never raises. Every probe pipeline of a LAZY set runs on the leader,
//! which holds the frame (D1035: the lowering pins it; the rest of the program keeps its width). A rerun set holding the
//! build re-arms the set at each re-open (`rearm`, exec subplan/raw.rs `rearm_sets`: PG's ExecHashSubPlan rebuilds the table at
//! the first probe after a rescan that changed its parameters): the next probe runs the members again, each kept slot
//! reset in place first (`rerun_members`), as a re-armed InitPlan runs (initplan/init.rs).
//! Created by M8-RUNTIME.
use core::cell::Cell;

use gravity_program::Program;
use types_error::PgResult;

use crate::ops::Frame;
use crate::glue;

pub(super) const NOT_RUN: u8 = 0;
pub(super) const RUNNING: u8 = 1;
pub(super) const DONE: u8 = 2;
pub(super) const FAILED: u8 = 3;
/// A build that ran, re-armed by its rerun set's re-open: the next probe runs the members again.
pub(super) const STALE: u8 = 4;

/// A set's build road: its members (program order; none: built in program order), the runner's frame while it runs the
/// program (`Live::frame`) and this execution's run state.
pub(crate) struct Lazy {
    pub(crate) members: Box<[u16]>,
    pub(crate) frame: Cell<Option<Frame>>,
    pub(super) run: Cell<u8>,
}

impl Lazy {
    pub(crate) fn new(members: Box<[u16]>) -> Lazy {
        Lazy { members, frame: Cell::new(None), run: Cell::new(NOT_RUN) }
    }

    /// Build pipeline `pipeline`'s LAZY road in `program`: the members and the exclusive-slot wall (m8_init.rs). Every
    /// probe pipeline of a LAZY set runs on the leader (D1035: the lowering pins it, as D1005 pins a lazy build's probe).
    pub(crate) fn of(program: &Program, pipeline: usize) -> PgResult<Lazy> {
        if pipeline >= program.pipelines.len() {
            return Err(bad("a LAZY nest.subplan's build pipeline is not the program's"));
        }
        let members = program.members(pipeline).members.into_boxed_slice();
        crate::ops::subplan::initplan::init::exclusive_slots(program, &members, "nest.subplan (LAZY)")?;
        Ok(Lazy::new(members))
    }

    pub(crate) fn deferred(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|p| self.members.binary_search(&p).is_ok())
    }

    /// Whether this execution ran (or began) the build and no re-open re-armed it since: a reset then is a Bug.
    pub(crate) fn ran(&self) -> bool {
        !matches!(self.run.get(), NOT_RUN | STALE)
    }

    /// A rerun set's re-open (exec subplan/raw.rs `rearm_sets`): a build that ran goes STALE, so the next probe runs it again; one
    /// that never ran, is stale or failed stays so; one running is a Bug by name.
    pub(crate) fn rearm(&self) -> PgResult<()> {
        match self.run.get() {
            DONE => self.run.set(STALE),
            RUNNING => return Err(bad("m8_rerun: a LAZY hashed set re-armed inside its own build")),
            _ => {}
        }
        Ok(())
    }

    /// AHEAD (`abi::SUBHASH_AHEAD`, lowerv4 ops/subplan/ahead.rs): the set's one reader `pipeline` starts (its late step,
    /// on the leader, before any of its claims). When it runs on workers the frame runs the build's members now, at their
    /// widths (the build itself, the last, serial), as an eager InitPlan's members run ahead of its reader
    /// (initplan/init.rs), so no worker's probe finds the set unbuilt; a reader on the leader alone runs nothing here and
    /// its first probe builds the set (`ensure`), where PostgreSQL builds it. The lowering makes a set AHEAD only when its
    /// build is raise-free and calls nothing VOLATILE, so building it before a probe changes no outcome.
    pub(crate) fn ahead(&self, pipeline: usize, table: &Cell<u64>) -> PgResult<()> {
        if self.run.get() != NOT_RUN || table.get() != 0 || self.members.is_empty() {
            return Ok(());
        }
        let Some(mut f) = self.frame.get() else { return Ok(()) };
        self.run.set(RUNNING);
        match crate::ops::subplan::initplan::raw::on_demand(&mut f).prerun_members(pipeline, &self.members) {
            Ok(false) => self.run.set(NOT_RUN),
            Ok(true) if table.get() != 0 => self.run.set(DONE),
            Ok(true) => {
                self.run.set(FAILED);
                return Err(bad("m8_subhash_run: the AHEAD set's build ran without sealing it"));
            }
            Err(e) => {
                self.run.set(FAILED);
                return Err(glue::pg_error(e));
            }
        }
        Ok(())
    }
}

#[cold]
#[inline(never)]
pub(super) fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}
