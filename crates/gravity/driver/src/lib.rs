//! Gravity driver: Node / Binding / Call / Entry over a Program (docs/gravity/driver.md).
#![deny(unsafe_code)]

pub mod ahead_trace;
pub mod bind;
pub mod binding;
pub mod claimcut;
pub mod entry;
pub mod env;
pub mod grant_ledger;
pub mod hostimpl;
pub mod lives;
pub mod machine;
pub mod node;
pub mod order;
pub mod park;
pub mod pull;
pub mod section;
pub mod sink;
pub mod slots;
pub mod source;

use std::borrow::Cow;

pub use crate::mem::arena::{box_locker, box_source, box_writer, Arena, ArenaBox, ArenaRef, ArenaVec, Mark, SourceBox};
pub use binding::{Attached, Binding, BindingParts, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, WorkerBinding};
pub use crate::mem::grant::{share, footprint, footprint_regions, forget_arena_draws, image_cut, AdmitRefused, Admitted, ARENA_ALLOWANCE, EXACT_BUILD, EXACT_FINAL, FLOOR_CHARGED_MIN, worker_arena, regions_for, regions_of, Accounting, Grant, GrantPool, Held, Overdraw, POOL_FRACTION_DEFAULT, POOL_FRACTION_WHOLE, PoolStats, Reservation, StmtCtx, Share, Split, StartSize, GRANT_FLOOR, IMAGES_MIN, POOL, SHARE_FLOOR, UNADMITTED_DREW};
pub use node::{in_list_set_region, program_images, try_region, try_region_in, RegionBuf, SLOT_PINS_GIVEN, SLOT_PINS_KEPT};
pub use crate::mem::region::pipeline_kind_scope;
pub use section::{no_owner_phase, one_owner_bytes, one_owner_whole_bytes, owner_place, OwnerPlace, MAX_WIDTH, OWN_BOUNDED, OWN_CUT, OWN_GATED, OWN_PAST_GRANT, OWN_PINNED, OWN_PIN_GIVEN, OWN_RESUMED, OWN_SPILLED, OWN_SPLIT, OWNER_CUT_HELD, SLOTS, TEST_OWNER_FIRST, TEST_OWNER_WALL};
pub use crate::mem::spill::{clear_region, install_spill_factory, FanOut, END_FAN_OUT, END_FILES, map_temp, refill_build, scatter_build, spill_factory, stream_records, stream_runs, HeldError, HeldHooks, HELD_INTERRUPT, IovFn, RunReader, SpillFactory, SpillFile, SpillSet, SpillTable, Stream, TempMap, Token, HELD_HOOKS, MAX_RUNS, OWN_STREAM, SPLIT_BITS, UNSUPPORTED_SPILL};
pub use gravity_abi::Partials;
/// M8 (D1036): the hosted lanes (lowerv4 hir.rs, one per worker ordinal) a run nested inside an outer row takes, the
/// innermost deepest: ordinal `NESTED_LANE_TOP + 1 - depth`, `NESTED_LANES` deep at most (`Node::nested_lane`).
pub const NESTED_LANE_TOP: u16 = 255;
pub const NESTED_LANES: u16 = 64;
pub const NESTED_LANE_FIRST: u16 = NESTED_LANE_TOP + 1 - NESTED_LANES;
pub use entry::{current_or, Current, CurrentCell, Entry, EntryStats, Exec, ExecState, Frame, OnDemand, Pending, Pulled, Recheck, Rerun, RowBuf, Step, Widths, END_US, TIMED};
pub use gravity_abi::{MorselRange, ScanPos};
pub use node::{stmt_context, Call, Derive, Node, NodeState, Ret};
pub use node::{StmtEnds, STMT_ENDS};
pub use park::Park;
pub use pull::{exists_cell, is_emit, relocate, PullAdapter, PullBuffer};
pub use order::{ClaimOrder, Wake};
pub use section::{Claimed, Open};
pub use crate::mem::spill_host::{RUN_CENSUS, SPILL_CENSUS};
pub use sink::{partition_plan, BatchFold, BorrowedBatch, Flow, Flush, LegRef, Merge, Partial, PartitionPlan, Region, Sink, SinkCalls, SinkLeg, SinkRef, State, WireFormat, MAX_WIRE_ATTS, P2_DIAG, POISON};
pub use slots::{DepotStats, SlotDepot, WorkerSlot, DEPOT, DEPOT_BYTES, DEPOT_SETS, DEPOT_SETS_PER_KEY};
pub use park::{RegionDepot, RegionDepotStats, PARK_BYTES, REGIONS, REGION_DEPOT_BYTES, REGION_MIN};
pub use gravity_abi::Lane;
pub use source::{blank_site, vec_bytes, Cursor, Locker, Outcome, Residency, SiteCalls, Source, Window, WindowBase, WriteOp, Writer};
pub mod mem;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Host(Cow<'static, str>),
    /// An expression form raised (`status::raised`): the mask's first code names the sqlstate and message.
    Raised(u64),
    BadReturn(i64),
    Unsupported(u8),
    Recheck(u8),
    /// D824 (V4R-298): a pipeline stopped for its lazy build (this pipeline) where the caller runs no producer arm (the pull
    /// adapter's fetch): the caller runs it (`Entry::build_wait`) and fetches on.
    BuildWait(usize),
    /// A kernel selected a faulting dictionary entry (`status::FAULT`): the runner re-evaluates
    /// entry `code` of table lane `lane` on `site` (of part `part`) through its function and raises that error.
    Fault { site: u8, lane: u8, code: u32, part: u32 },
    /// A breaker region or arena chunk the grant pool or the allocator could not cover (`budget::POOL`):
    /// the statement's error, never the process's abort. `why`: the SPILL-DEBT row of the region's breaker or of its road,
    /// when one names the refused grow (`mem::grow::debt_refused`); the client's 53200 is the same either way, exec glue
    /// writes the row in the server log only.
    OutOfMemory { bytes: u64, held: u64, capacity: u64, why: Option<gravity_program::spill::NoSpill> },
    /// A wall the program met at run time, by name (0A000 `gravity: refused (…)`, as an admission refusal reads).
    Refused(Cow<'static, str>),
}

pub type Result<T> = core::result::Result<T, Error>;

/// C9: a polled host step's stop (`cancelpoll::Step`) is the statement's cancel.
impl From<cancelpoll::Canceled> for Error {
    fn from(_: cancelpoll::Canceled) -> Error {
        Error::Cancelled
    }
}

/// C9: the host's answer for a call under abort byte `byte` that failed with `e` after one of its host loops stopped for
/// the abort bit (exec glue `stopped`). true: the stop is still the statement's to raise, so its owner raises
/// PostgreSQL's own error for it; the call's stashed error has been dropped (logged at DEBUG). false: `e` stands as raised.
pub static STOPPED: pgsync::OnceLock<fn(*const u8, &Error) -> bool> = pgsync::OnceLock::new();

/// C9: a call's error once a host loop of it stopped for its statement's abort bit (`cancelpoll::scoped_seen`): a raise
/// or a host's error is the statement's cancel when the host says the stop is still the statement's to raise (`STOPPED`;
/// with none installed, always), else it stands as raised. D4306: a kernel's scratch refusal (kind 2) likewise: a draw the
/// cancel ended returns null (`cancelpoll::seen`), which the kernel reads as its scratch running out. Every other error
/// stands.
pub fn stopped_error(byte: *const u8, e: Error) -> Error {
    if !matches!(e, Error::Raised(_) | Error::Host(_) | Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH)) {
        return e;
    }
    if STOPPED.get().is_none_or(|f| f(byte, &e)) {
        return Error::Cancelled;
    }
    e
}

std::thread_local! {
    /// D1443: this thread's last `Error::Unsupported`: the pipeline whose run ended in it, and whether a kernel returned it.
    static UNSUPPORTED_AT: core::cell::Cell<(Option<usize>, bool)> = const { core::cell::Cell::new((None, false)) };
    /// `DECISIONS.md` 4 (D) b: the run-ahead tag of the pipeline whose run ended in an expression's error, with that error
    /// (`note_ahead`), for the statement's error (exec glue `pg_error`).
    static AHEAD_AT: core::cell::RefCell<Option<(gravity_program::AheadTag, Error)>> = const { core::cell::RefCell::new(None) };
    /// NR-3: the SPILL-DEBT row of the leader's last call that raised after a kernel pin the pool and the temp-file road
    /// refused on a debt home (`note_pin_debt`), for the statement's error (exec glue `pg_error`, `take_pin_debt`).
    static PIN_DEBT: core::cell::Cell<Option<gravity_program::spill::NoSpill>> = const { core::cell::Cell::new(None) };
}

/// NR-3: a call raised after a kernel pin refused on a road SPILL-DEBT row `row` names (`Inner::settle_call`).
#[cold]
pub fn note_pin_debt(row: Option<gravity_program::spill::NoSpill>) {
    PIN_DEBT.with(|c| c.set(row));
}

/// NR-3: the row `note_pin_debt` kept, taken (exec glue `pg_error`, at every statement error: a raise that is not an out of
/// memory leaves it unused).
pub fn take_pin_debt() -> Option<gravity_program::spill::NoSpill> {
    PIN_DEBT.with(|c| c.take())
}

/// D1443: pipeline `p`'s run ended in `e` (kept when `Unsupported`), `kernel`: a kernel's own status (`Slot::settle_call`).
pub fn note_unsupported(p: Option<usize>, e: &Error, kernel: bool) {
    if matches!(e, Error::Unsupported(_)) {
        UNSUPPORTED_AT.with(|c| c.set((p.or(c.get().0), kernel || c.get().1)));
    }
}

/// D1443: the failing pipeline and the kernel flag `note_unsupported` kept, taken (the statement's error names them).
pub fn take_unsupported() -> (Option<usize>, bool) {
    UNSUPPORTED_AT.with(|c| c.replace((None, false)))
}

/// `DECISIONS.md` 4 (D) b: pipeline `p` of `program` ended in `e` (`note_ahead_tag` with its `Pipeline::ahead`).
pub fn note_ahead(program: &gravity_program::Program, p: usize, e: &Error) {
    note_ahead_tag(program.pipelines.get(p).and_then(|x| x.ahead), e);
}

/// `DECISIONS.md` 4 (D) b: a pipeline tagged `t` ended in `e`. When it runs ahead of PostgreSQL's plan under a tagged
/// licence (`t` is `Some`) and `e` is an expression's error (a raise, or a host call's error), the tag is kept for the
/// statement's error; the first one kept stands until `take_ahead`.
pub fn note_ahead_tag(t: Option<gravity_program::AheadTag>, e: &Error) {
    let Some(t) = t else { return };
    if matches!(e, Error::Raised(_) | Error::Host(_)) {
        AHEAD_AT.with(|c| {
            let mut c = c.borrow_mut();
            if c.is_none() {
                *c = Some((t, e.clone()));
            }
        });
    }
}

/// `DECISIONS.md` 4 (D) b: the tag `note_ahead` kept, taken: `Some` only when the error it kept is `e`, the statement's
/// error as the driver returns it (an error of another pipeline, or of a plan-order run, is not tagged). `None` clears it.
pub fn take_ahead(e: Option<&Error>) -> Option<gravity_program::AheadTag> {
    AHEAD_AT.with(|c| c.borrow_mut().take()).filter(|(_, x)| e == Some(x)).map(|(t, _)| t)
}

impl Error {
    pub fn host(msg: impl Into<Cow<'static, str>>) -> Error {
        Error::Host(msg.into())
    }

    /// CONVERT1: `bytes` the allocator refused an operator's arena (gravity_fn `arena::Refused`): the statement's out of
    /// memory, with the grant pool's numbers now, as a refused region's.
    pub fn out_of_memory(bytes: u64) -> Error {
        let pool = &crate::mem::grant::POOL;
        Error::OutOfMemory { bytes, held: pool.held(), capacity: pool.capacity(), why: None }
    }
}
