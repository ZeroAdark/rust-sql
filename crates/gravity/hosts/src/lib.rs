//! Gravity hosts: Source / Writer / Locker / Derive over the access methods (docs/gravity/hosts.md).
#![deny(unsafe_code)]

pub mod cost_host;
pub mod facts;
pub mod inputs;
pub mod memory_budget;
pub mod membroker;
pub mod perms;
pub mod recipe;
pub mod v4;
pub mod scratch;
pub mod tinylfu;
pub mod worker_arm;

use std::cell::Cell;

pub use crate::ops::scan::bitmap_source::BitmapSource;
pub use crate::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartList, PartSpec};
pub use crate::ops::scan::heap_source::{HeapRangeSource, RangeRecipe};
pub use crate::ops::modifytable::index_maint::{IndexMaint, IndexRecipe};
pub use crate::ops::scan::index_source::{IndexSource, SiteRecipe};
pub use crate::ops::scan::pgrc::lanebits::LaneBits;
pub use crate::ops::lockrows::lock::{LockHost, LockRecipe};
pub use crate::ops::{HostRegistry, Recipe};
pub use scratch::NodeScratch;
pub use crate::ops::modifytable::trigger::TriggerHost;
pub use worker_arm::{arm_database_encoding, arm_once, arm_worker, first_arm_micros, is_armed, no_arm, ArmFn, Carried, SessionCarry, StatementArm};
pub use crate::ops::modifytable::write::{WriteHost, WriteRecipe};

// The pgrc source's run-time calls the exec families make (its own exec half lives in its family): the top-k walk's
// section width from the resident zone store (`resolve`, `zone_walk_order`, `zone_walk_reach`; exec ops/sort walk.rs),
// the by-rowid fetch site's recipe (`FetchRecipe`; exec ops/sort fetch.rs), and the part list and a site's verdict cell
// with its runtime-filter claim that the two-pass marks and join.rtfilter protocols hold (`PartList` above,
// `VerdictCell`, `VerdictClaim`, `resolve`; exec ops/sort/topn proto.rs), with D3245's slots a site's filters claim
// (`RT_SLOTS`) and the key range a filter's keys are clipped to (`part_extrema`).
pub use crate::ops::scan::pgrc::fetch_site::FetchRecipe;
pub use crate::ops::scan::pgrc::granule_source::{part_extrema, resolve, zone_walk_order, zone_walk_reach, VerdictCell, VerdictClaim, RT_SLOTS};

use std::rc::Rc;

use gravity_abi::status::UNSUPPORTED_SHAPE;
use gravity_driver::Error;
use types_error::{PgError, PgResult, ERRCODE_T_R_SERIALIZATION_FAILURE};
use types_slot::SlotData;
pub mod dict;
pub mod instr;
pub mod ops;

/// A scan filter (the plan's `qual` on the heap tuple), compiled by the
/// runner, evaluated by the index source on every fetched or substituted row.
pub type FilterFn = Rc<dyn Fn(&mut SlotData<'static>) -> PgResult<bool>>;

/// IsolationUsesXactSnapshot (repeatable read / serializable); the unit
/// harness installs no xact seams, there every transaction is read committed.
pub fn xact_snapshot() -> bool {
    xact_seams::isolation_uses_xact_snapshot::is_installed() && xact_seams::isolation_uses_xact_snapshot::call()
}

/// PostgreSQL's text for a concurrent update under a transaction snapshot.
pub fn serialization_failure() -> Box<PgError> {
    Box::new(PgError::error("could not serialize access due to concurrent update".to_string()).with_sqlstate(ERRCODE_T_R_SERIALIZATION_FAILURE)) // alloc: Cold
}

thread_local! {
    static PENDING: Cell<Option<Box<PgError>>> = const { Cell::new(None) };
}

pub fn stash(e: Box<PgError>) -> Error {
    // D3887: a host loop of the running kernel call stopped for the statement's abort bit (`cancelpoll::canceled`): the
    // error its stop raised on the way out is the cancel's, which the statement's owner raises (never this one)
    if cancelpoll::seen() {
        note_dropped(&e);
        return Error::Cancelled;
    }
    PENDING.with(|p| p.set(Some(e)));
    Error::host("pg")
}

/// C9: an error dropped because its call stopped for the statement's cancel, whose owner raises the stop's own error
/// (`stash` above, exec glue `stopped`): logged at DEBUG1 with its SQLSTATE, so a real error a cancel ate stays traceable.
/// A cancel arrives at no fixed point, so reporting it in that error's place is one of PostgreSQL's own outcomes.
pub fn note_dropped(e: &PgError) {
    if !elog::message_level_is_interesting(types_error::DEBUG1) {
        return;
    }
    let code = types_error::unpack_sqlstate(e.sqlstate());
    let _ = elog::ereport(types_error::DEBUG1)
        .errmsg(format!("gravity: a call stopped for its statement's cancel dropped its error {}: {}", String::from_utf8_lossy(&code), e.message())) // alloc: Cold
        .finish(types_error::ErrorLocation::new("gravity", 0, "note_dropped"));
}

pub fn take_error() -> Option<Box<PgError>> {
    PENDING.with(|p| p.take())
}

/// A thread's stashed error from either road: a host's (`stash`) or a hosted site's (`gravity_fn::stash_error`, a shim
/// raise on a pool thread — M2 L5).
pub fn take_any_error() -> Option<Box<PgError>> {
    take_error().or_else(gravity_fn::take_error)
}

/// The extent of a program run and the mapping of its error (exec admit reentry `run_at`: a statement's run at its
/// level; gravity_pool: a claim, a background job): the thread's two stashes (`stash`, gravity_fn::stash_error) are
/// the extent's own. Entered, it takes an outer extent's stashes aside; dropped (an unwind too), what the extent left
/// goes and the outer's come back. An error a nested statement stashed and never mapped (it unwound: an ERROR a PL
/// exception block catches) is never the next caller's, and a nested statement never maps its caller's.
#[must_use = "the stashes are the extent's only while the guard lives"]
pub struct StashScope {
    host: Option<Box<PgError>>,
    shim: Option<Box<PgError>>,
}

impl StashScope {
    pub fn enter() -> StashScope {
        StashScope { host: take_error(), shim: gravity_fn::take_error() }
    }
}

impl Drop for StashScope {
    fn drop(&mut self) {
        PENDING.with(|p| p.set(self.host.take()));
        drop(gravity_fn::take_error());
        if let Some(e) = self.shim.take() {
            gravity_fn::stash_error(e);
        }
    }
}

pub fn unsupported() -> Error {
    Error::Unsupported(UNSUPPORTED_SHAPE)
}

pub(crate) fn pg_err(msg: &'static str) -> Box<PgError> {
    Box::new(PgError::error(msg.to_string())) // alloc: Cold
}
