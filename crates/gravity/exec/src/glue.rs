//! Plan keys, binding parts and error mapping: the small conversions between

use core::any::Any;
use std::rc::Rc;

use gravity_driver::{BindingParts, CommandId, Error, Identity, PerTuple, Pins, Snapshot};
use gravity_program::{Generation, Handle, PlanKey};
use types_error::{PgError, PgResult, ERRCODE_INTERNAL_ERROR, ERRCODE_OUT_OF_MEMORY, ERRCODE_QUERY_CANCELED};
use types_portal::CachedPlanHandle;
use types_snapshot::SnapshotData;

mod raw;
pub use raw::{abort_u8, poll_kernel};

const _: () = assert!(gravity_abi::ctx::ABORT_BIT == init_small::globals::ABORT_BIT && gravity_abi::ctx::SERVICE_BIT == init_small::globals::SERVICE_BIT);
// D3887: the host loops' poll reads the same abort bit; the probe's bit is neither
const _: () = assert!(cancelpoll::ABORT_BIT == gravity_abi::ctx::ABORT_BIT && cancelpoll::PROBE_BIT & (gravity_abi::ctx::ABORT_BIT | gravity_abi::ctx::SERVICE_BIT) == 0);
// C5: one revoke bit across its setter (mcx), the byte's owner (init_small) and the kernels' poll, apart from the others
const _: () = assert!(gravity_abi::ctx::REVOKE_BIT == init_small::globals::REVOKE_BIT && gravity_abi::ctx::REVOKE_BIT == mcx::revoke::REVOKE_BIT && gravity_abi::ctx::REVOKE_BIT & (gravity_abi::ctx::ABORT_BIT | gravity_abi::ctx::SERVICE_BIT | cancelpoll::PROBE_BIT) == 0);

/// D3406: the service bit of this thread's own byte, on a kernel's poll: cleared first (a delivery after it raises it
/// again), then C's CHECK_FOR_INTERRUPTS when an interrupt is pending (the bit may be stale: the drain that cleared
/// InterruptPending took its signal). true = stop: the check raised (its error stashed as the thread's host error,
/// `gravity_hosts::stash`, which `pg_error` reports for the kernel's `Error::Cancelled`), or a cancel came meanwhile.
fn service_interrupts(own: &core::sync::atomic::AtomicU8) -> bool {
    init_small::globals::ClearServicePending();
    if let Err(e) = cancelpoll::interrupts::check() {
        drop(gravity_hosts::stash(e));
        return true;
    }
    gravity_abi::ctx::abort_set(own)
}

pub fn plan_key(cplan: CachedPlanHandle, stmt_index: u16) -> Option<PlanKey> {
    if cplan.is_null() || !plancache_portal_seams::plan_identity::is_installed() {
        return None;
    }
    let (handle, generation, generic) = plancache_portal_seams::plan_identity::call(cplan);
    if !generic || stmt_index > 0xff {
        return None;
    }
    // the generation's low 23 bits; bit 23 is an identity key's (D3865 L5, `PlanKey::IDENTITY`), the high byte the index
    Some(PlanKey { handle: Handle(handle & !PlanKey::CONTENT), generation: Generation(((generation as u32) & (PlanKey::IDENTITY - 1)) | ((stmt_index as u32) << 24)) })
}

pub fn identity() -> Identity {
    Identity { proc_number: init_small::globals::MyProcNumber(), resource_owner: 0, latch: 0 }
}

/// The statement's binding parts; `resolved` is the runner's `Resolved` (the granule relations as this statement sees them) for a program over granule sources, `None` otherwise.
pub fn binding_parts<'a>(snapshot: &'a Rc<SnapshotData<'static>>, command_id: u32, params: &'a [u64], nulls: &'a [u8], resolved: Option<&'a (dyn Any + Send + Sync)>) -> BindingParts<'a> {
    poll_gap_probe();
    BindingParts {
        snapshot: Snapshot(snapshot as *const Rc<SnapshotData<'static>> as u64),
        command_id: CommandId(command_id),
        params,
        param_nulls: nulls,
        abort_flag: abort_u8(),
        pins: Pins(0),
        per_tuple: PerTuple(0),
        identity: identity(),
        resolved,
    }
}

/// D3887: the poll-gap probe (`PGRUST_GRAVITY_POLL_GAP_MS`), the host loops' polls (`PGRUST_GRAVITY_HOST_POLL`), the
/// kernels' small sort road (`PGRUST_GRAVITY_SMALL_SORT`), the park's shed-once rule (`PGRUST_GRAVITY_SHED_ONCE`), the
/// context blocks' pooled memo (`PGRUST_GRAVITY_POOLED_MEMO`) and their and the program arenas' served bit
/// (`PGRUST_GRAVITY_POOLED_BIT`), the send
/// buffer's growth (`PGRUST_GRAVITY_SEND_GROW`),
/// printtup's binary text columns into the DataRow (`PGRUST_GRAVITY_SEND_INTO`) and the park's index (P2 W1,
/// `PGRUST_GRAVITY_PARK_INDEX` and its `_CHECK`), switched once per process at the first
/// statement; while the probe is on,
/// the statement's byte carries the probe's bit, so every kernel back-edge calls `poll_kernel`, which marks the probe.
fn poll_gap_probe() {
    if !cancelpoll::probe_set() {
        cancelpoll::set_host_polls(crate::inputs::knobs::diag::host_polls());
        gravity_kernels::host_abi::set_small_sort(crate::inputs::knobs::diag::small_sort());
        gravity_driver::park::set_shed_once(crate::inputs::knobs::diag::shed_once());
        gravity_driver::park::set_park_index(crate::inputs::knobs::diag::park_index(), crate::inputs::knobs::diag::park_index_check());
        mcx::heap::set_pooled_memo(crate::inputs::knobs::diag::pooled_memo());
        let pooled_bit = crate::inputs::knobs::diag::pooled_bit();
        mcx::heap::set_pooled_bit(pooled_bit);
        gravity_program::arena::set_pooled_bit(pooled_bit);
        gravity_wire::set_send_grow(crate::inputs::knobs::diag::send_grow());
        fmgr_core::set_send_into(crate::inputs::knobs::diag::send_into());
        cancelpoll::set_probe_ms(crate::inputs::knobs::diag::poll_gap_ms().unwrap_or(0));
        cancelpoll::set_stall_ms(crate::inputs::knobs::diag::poll_stall_ms().unwrap_or(0));
        if let Some(ms) = crate::inputs::knobs::diag::step_gap_ms() {
            cancelpoll::set_step_gap_ms(ms);
        }
        let _ = gravity_driver::STOPPED.set(stopped);
        if let Some(label) = crate::inputs::knobs::diag::cancel_at_step() {
            cancelpoll::set_cancel_at(Box::leak(label.into_boxed_str()), cancel_at_fire); // alloc: Cold
            CANCEL_AT_ARMED.store(true, core::sync::atomic::Ordering::Release);
        }
    }
    if cancelpoll::probe_on() {
        abort_u8().fetch_or(cancelpoll::PROBE_BIT, core::sync::atomic::Ordering::Relaxed);
    }
    if CANCEL_AT_ARMED.load(core::sync::atomic::Ordering::Relaxed) {
        cancel_at_target();
    }
}

/// C9: the driver's `STOPPED`, for a call under `byte` that failed with `e` after one of its host loops stopped for the
/// abort bit. On a pool worker (`byte` is its leader's) and on the owner while an abort-class interrupt is pending that it
/// can take (no CHECK_FOR_INTERRUPTS took the stop yet) the stop is still the statement's to raise: the call's stashed
/// errors are dropped, each logged at DEBUG1 with its SQLSTATE, and the owner raises PostgreSQL's own error for the stop
/// (`pg_error_of`'s Cancelled arm: 57014 with the cancel's or the statement_timeout's message, 57P01 for a terminate).
/// Otherwise `e` stands as raised.
fn stopped(byte: *const u8, e: &Error) -> bool {
    if core::ptr::eq(byte, abort_u8().as_ptr() as *const u8) && !cancelpoll::interrupts::pending_abort() {
        return false;
    }
    if let Error::Raised(mask) = *e {
        if gravity_abi::status::raise::describe(gravity_abi::status::raise::first(mask)).is_some() {
            gravity_hosts::note_dropped(&raised(mask));
        }
    }
    // the thread's two stashes: a host's and a hosted site's
    if let Some(d) = gravity_hosts::take_error() {
        gravity_hosts::note_dropped(&d);
    }
    if let Some(d) = gravity_fn::take_error() {
        gravity_hosts::note_dropped(&d);
    }
    true
}

/// `PGRUST_GRAVITY_CANCEL_AT_STEP` is set (`cancel_at_target` then runs at every statement's binding).
static CANCEL_AT_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The backend the lever cancels: the latest statement's owner (its abort byte, its pid).
static CANCEL_AT_BYTE: core::sync::atomic::AtomicPtr<u8> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static CANCEL_AT_PID: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

/// The lever's target: this statement's owner.
#[cold]
fn cancel_at_target() {
    CANCEL_AT_BYTE.store(abort_u8().as_ptr(), core::sync::atomic::Ordering::Release);
    CANCEL_AT_PID.store(init_small::globals::MyProcPid(), core::sync::atomic::Ordering::Release);
}

/// The lever's delivery (`cancelpoll::set_cancel_at`): a step under the target statement's byte sends its backend a
/// SIGINT, pg_cancel_backend's thread signal, which raises the byte's abort bit at delivery; a server log line says so.
fn cancel_at_fire(byte: *const core::sync::atomic::AtomicU8) {
    // the thread-signal emulation's Linux-numbered SIGINT (lock waitqueue.rs's)
    const SIGINT: i32 = 2;
    let target = CANCEL_AT_BYTE.load(core::sync::atomic::Ordering::Acquire) as *const u8;
    if byte as *const u8 != target {
        return;
    }
    let pid = CANCEL_AT_PID.load(core::sync::atomic::Ordering::Acquire);
    let rc = procsignal_seams::send_thread_signal::call(pid, SIGINT);
    eprintln!("gravity: PGRUST_GRAVITY_CANCEL_AT_STEP sent backend {pid} a SIGINT (rc {rc})"); // alloc: Cold
}

/// The error of a raise mask (abi `status::raised`): the incumbent's sqlstate and message for the
/// lowest code; a code without a text of its own (a host's, a table entry's) is the internal report.
fn raised(mask: u64) -> Box<PgError> {
    use gravity_abi::status::raise;
    let code = raise::first(mask);
    match raise::describe(code) {
        Some((state, msg)) => Box::new(PgError::error(msg.to_string()).with_sqlstate(types_error::make_sqlstate(*state))), // alloc: Cold
        None => internal(&format!("gravity: program reported an error without a message (raise code {code})")), // alloc: Cold
    }
}

/// GV-D1: the mask's first code is the row's first error in PG's order. A stashed host / shim error is the error only
/// when that code names none of its own (`HOST`); beside an in-IR code a stash is a later or an unreported call's, dropped.
fn raised_or_stashed(mask: u64) -> Box<PgError> {
    use gravity_abi::status::raise;
    if raise::describe(raise::first(mask)).is_some() {
        drop((gravity_hosts::take_error(), gravity_fn::take_error()));
        return raised(mask);
    }
    gravity_hosts::take_error().or_else(gravity_fn::take_error).unwrap_or_else(|| raised(mask)) // alloc: Cold
}

pub fn pg_error(e: Error) -> Box<PgError> {
    let ahead = gravity_driver::take_ahead(Some(&e));
    // NR-3: taken at every error, used only for the raise of the call that kept it
    let pin = gravity_driver::take_pin_debt().filter(|_| matches!(e, Error::Raised(_)));
    let mut err = pg_error_of(e);
    if let Some(w) = pin {
        named_raise(&mut err, w);
    }
    if let Some(t) = ahead {
        if !crate::runner::tag_stands(&err) {
            return tag_refusal(&err, t);
        }
        tag_ahead(&mut err, t);
    }
    err
}

/// `DECISIONS.md` 4 (D) b: the run-ahead tag's first words (`tag_ahead`; runner `ahead_tagged` reads them).
pub(crate) const AHEAD_TAG: &str = "gravity ahead: ";

/// D4206: the reason's first words of `tag_refusal` (runner `tag_refused` reads them).
pub(crate) const TAG_REFUSAL: &str = "a part run ahead of the plan raised";

/// D4206 (`DECISIONS.md` 4 (D) b covers data errors only, classes 21 / 22 / 23): a part run ahead of PostgreSQL's plan
/// under a tagged licence raised an error `runner::tag_stands` does not keep (a feature not supported, a rule of the
/// catalog, an internal error). PostgreSQL may return rows where the part raised and no ruling makes that error the
/// statement's, so the statement refuses by name, the code and the node in the reason; the error's own text stays in the
/// log-only detail, without the run-ahead tag (the harness's ahead judge reads no refusal as a tagged error).
fn tag_refusal(err: &PgError, t: gravity_program::AheadTag) -> Box<PgError> {
    let code = types_error::unpack_sqlstate(err.sqlstate());
    let why = format!("{TAG_REFUSAL} {} at node {} {}, outside the data classes a run ahead may raise", String::from_utf8_lossy(&code), t.node, t.node_type); // alloc: Cold
    let mut r = crate::runner::refused(&why);
    r.detail_log = Some(format!("the run-ahead error: {}", err.message())); // alloc: Cold
    r
}

/// `DECISIONS.md` 4 (D) b: the statement's error, raised by a part run ahead of PostgreSQL's plan under a tagged licence
/// (driver `take_ahead`), carries the part's tag for the harness's judge in the server log only (errdetail_log): the
/// client reads the error as PostgreSQL raises it. The tag goes first; a detail the error carries follows it there.
fn tag_ahead(err: &mut PgError, t: gravity_program::AheadTag) {
    let code = types_error::unpack_sqlstate(err.sqlstate());
    let tag = format!("{AHEAD_TAG}{} node {} {} (DECISIONS 4 (D) b)", String::from_utf8_lossy(&code), t.node, t.node_type); // alloc: Cold
    err.detail_log = Some(match err.detail_log.take().or_else(|| err.detail.as_deref().map(String::from)) { // alloc: Cold
        Some(d) => format!("{tag}\n{d}"), // alloc: Cold
        None => tag,
    });
}

fn pg_error_of(e: Error) -> Box<PgError> {
    match e {
        Error::Host(msg) => gravity_hosts::take_error().or_else(gravity_fn::take_error).unwrap_or_else(|| internal(&format!("gravity: {msg}"))), // alloc: Cold
        Error::Raised(mask) => raised_or_stashed(mask),
        // the kernel stopped on its byte (one load, abi-v2 §3): the error its poll's CHECK_FOR_INTERRUPTS raised (D3406),
        // else the abort-class check raises the real error (cancel or terminate); a byte it does not own is still a cancel
        Error::Cancelled => match gravity_hosts::take_error() {
            Some(e) => e,
            None => match cancelpoll::interrupts::check_abort() {
                Err(e) => e,
                Ok(()) => Box::new(PgError::error("canceling statement due to user request".to_string()).with_sqlstate(ERRCODE_QUERY_CANCELED)), // alloc: Cold
            },
        },
        Error::BadReturn(x) => internal(&format!("gravity: bad kernel return {x}")), // alloc: Cold
        Error::Unsupported(k) => internal(&unsupported_message(k)), // alloc: Cold
        Error::Recheck(s) => internal(&format!("gravity: unhandled recheck at site {s}")), // alloc: Cold
        Error::BuildWait(b) => internal(&format!("gravity: unhandled lazy build wait for pipeline {b}")), // alloc: Cold
        // the runner re-evaluates the entry before this point (expr.md §2.2); reaching here is a gap
        Error::Fault { site, lane, code, part } => internal(&format!("gravity: deferred fault of entry {code} (part {part}) on table lane {lane} of site {site} was not re-evaluated")), // alloc: Cold
        Error::Refused(why) => crate::runner::refused(&why),
        Error::OutOfMemory { bytes, held, capacity, why } => {
            let detail = format!("Failed on request of size {bytes} in a Gravity breaker region (grant pool: {held} of {capacity} bytes held)."); // alloc: Cold
            let err = PgError::error("out of memory".to_string()).with_sqlstate(ERRCODE_OUT_OF_MEMORY).with_hint("Lower work_mem or hash_mem_multiplier, or the number of concurrent statements.".to_string()); // alloc: Cold
            // D4914: the server log's detail names the branch that refused the statement's last draw past its grant, when
            // one was kept (none: the detail it logs today)
            let logged = refusal_line().map(|r| format!("{detail}\n{r}")); // alloc: Cold
            // a refusal a SPILL-DEBT row names carries the row's server-only line last in its log-only detail, after the
            // detail it logs today; `note_debt_refused` takes it off and writes it where the error is the statement's
            Box::new(match why { // alloc: Cold
                Some(w) => err.with_detail_log(format!("{}\n{}", logged.as_deref().unwrap_or(&detail), debt_line(w))).with_detail(detail), // alloc: Cold
                None => logged_or_not(err, logged).with_detail(detail),
            })
        }
    }
}

/// NR-3: a raise after a kernel pin the pool and the temp-file road refused on a debt home (driver `take_pin_debt`): the
/// kernel's own out of memory (kernels `pin_refused`, 53200) carries the row's server-only line last in its log-only
/// detail, as `pg_error_of`'s OutOfMemory arm writes it; the client's error is the same. Any other error as it is.
fn named_raise(err: &mut PgError, w: gravity_program::spill::NoSpill) {
    if err.sqlstate() != ERRCODE_OUT_OF_MEMORY {
        return;
    }
    let Some(detail) = err.detail.as_deref() else { return };
    err.detail_log = Some(format!("{}\n{}", err.detail_log.as_deref().unwrap_or(detail), debt_line(w))); // alloc: Cold
}

/// D4914: `err` with the refusal's log-only detail `logged`, when the statement kept one; else as it is.
fn logged_or_not(err: PgError, logged: Option<String>) -> PgError {
    match logged {
        Some(l) => err.with_detail_log(l),
        None => err,
    }
}

/// D4914: the server-only line naming the branch that refused this thread's statement's last draw past its grant
/// (`mcx::ladder_stats::Why`), with that draw's bytes and the statement's draw-gate waits so far; `None` when the statement
/// kept no refusal (no statement charged here, or no draw past its grant refused since its start).
fn refusal_line() -> Option<String> {
    let (why, bytes, waited_ns) = mcx::governor::stmt_refusal()?;
    let name = mcx::ladder_stats::Why::from_u8(why).map_or("unknown", mcx::ladder_stats::Why::name);
    Some(format!("gravity: memory: refused by {name} (the statement's last refused draw {bytes} bytes; its draw waits {} ms)", waited_ns / 1_000_000)) // alloc: Cold
}

/// The first words of the server-only line an out of memory names its SPILL-DEBT row by (`note_debt_refused`).
pub(crate) const DEBT_LINE: &str = "gravity: memory: SPILL-DEBT ";

/// The server-only line of an out of memory SPILL-DEBT row `w` names: the row and the sentence a user reads for it.
fn debt_line(w: gravity_program::spill::NoSpill) -> String {
    format!("{DEBT_LINE}{w:?}: {}", w.limitation()) // alloc: Cold
}

/// `err` is the statement's error, raised here: no re-run in plan order and no fall-back takes its place. When it is an
/// out of memory a SPILL-DEBT row names (`pg_error_of`), the row's line is taken off its log-only detail, which then
/// logs as it did before, and written once to the server log only (LOG_SERVER_ONLY: never to the client, whatever
/// client_min_messages says); the client's 53200 is the same either way. Never while the statement's cancel is pending
/// or was seen: the cancel is the statement's (57014). The line written, if any.
pub fn note_debt_refused(err: &mut PgError) -> Option<String> {
    let d = err.detail_log.as_deref()?;
    let at = d.rfind('\n').filter(|&i| d[i + 1..].starts_with(DEBT_LINE))?;
    let mut kept = err.detail_log.take()?;
    let line = kept.split_off(at + 1); // alloc: Cold
    kept.pop(); // the newline before the line
    err.detail_log = (Some(kept.as_str()) != err.detail.as_deref()).then_some(kept);
    if cancelpoll::seen() || cancelpoll::interrupts::pending_abort() {
        return None;
    }
    let _ = elog::ereport(types_error::LOG_SERVER_ONLY).errmsg(line.to_string()).finish(types_error::ErrorLocation::new("gravity", 0, "memory")); // alloc: Cold
    Some(line)
}

/// The text of an `Error::Unsupported` (the runner names the lowered join's spilled build by it).
pub fn unsupported_message(k: u8) -> String {
    if k == gravity_abi::status::UNSUPPORTED_OUT_REGION {
        return "gravity: unsupported execution (kind 5: a by-reference output row wider than the out region)".to_string(); // alloc: Cold
    }
    if k == gravity_abi::status::UNSUPPORTED_SPILL {
        return "gravity: unsupported execution (kind 7: a spill past its files' limits)".to_string(); // alloc: Cold
    }
    format!("gravity: unsupported execution (kind {k})") // alloc: Cold
}

pub fn internal(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(msg.to_string()).with_sqlstate(ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use gravity_abi::status::raise;
    use gravity_driver::Error;
    use types_error::PgError;

    use super::{pg_error, stopped, unsupported_message};

    fn stash(msg: &str) {
        gravity_fn::stash_error(Box::new(PgError::error(msg.to_string()))); // alloc: Cold
    }

    /// GV-D1: an in-IR code first in the row's raise word is the error; a stash beside it (a later host call's, an
    /// unreported element's) is not, and it does not survive into the next error.
    #[test]
    fn a_stash_never_beats_the_first_in_ir_code() {
        stash("a stale shim error");
        let _ = gravity_hosts::stash(Box::new(PgError::error("a stale host error".to_string()))); // alloc: Cold
        assert_eq!(pg_error(Error::Raised(raise::bit(raise::DIV_ZERO))).message(), "division by zero");
        assert!(gravity_fn::take_error().is_none() && gravity_hosts::take_error().is_none(), "both stashes dropped");
        stash("the shim's error");
        assert_eq!(pg_error(Error::Raised(raise::bit(raise::HOST))).message(), "the shim's error");
        let _ = gravity_hosts::stash(Box::new(PgError::error("the host's error".to_string()))); // alloc: Cold
        assert_eq!(pg_error(Error::Raised(raise::bit(raise::HOST))).message(), "the host's error");
        assert_eq!(pg_error(Error::Raised(raise::bit(raise::INT2_RANGE))).message(), "smallint out of range");
    }

    /// An out of memory a SPILL-DEBT row names reads to the client as any out of memory does, byte for byte (every
    /// field the frontend message sends); the row's line rides in its log-only detail. Where the error is the statement's,
    /// `note_debt_refused` writes the line once and leaves the error as an unnamed one is, its server log entry too.
    /// Never while the statement's cancel was seen: the cancel is the statement's.
    #[test]
    fn a_named_out_of_memory_reads_the_same_and_writes_its_row_once() {
        use gravity_program::spill::NoSpill;
        let oom = |why| Error::OutOfMemory { bytes: 64 << 20, held: 3 << 20, capacity: 4 << 20, why };
        let client = |e: &PgError| (e.level, e.sqlstate, e.message.clone(), e.message_raw.clone(), e.detail.clone(), e.detail_raw.clone(), e.hint.clone(), e.context.clone(), e.location.clone(), e.cursor_position, e.internal_position, e.internal_query.clone()); // alloc: Cold
        let plain = pg_error(oom(None));
        let mut named = pg_error(oom(Some(NoSpill::BuildNestInner)));
        assert_eq!(client(&*named), client(&*plain), "the client reads the same 53200");
        assert_eq!(plain.detail_log(), None);
        let line = format!("gravity: memory: SPILL-DEBT BuildNestInner: {}", NoSpill::BuildNestInner.limitation()); // alloc: Cold
        assert_eq!(named.detail_log(), Some(format!("{}\n{line}", plain.detail().unwrap()).as_str()), "the line rides last in the log-only detail"); // alloc: Cold
        assert_eq!(super::note_debt_refused(&mut named).as_deref(), Some(line.as_str()), "written where the error is the statement's");
        assert_eq!(*named, *plain, "then the error is an unnamed one's, its server log entry too");
        assert_eq!(super::note_debt_refused(&mut named), None, "once");
        assert_eq!(super::note_debt_refused(&mut pg_error(oom(None))), None, "an unnamed out of memory writes none");
        // the statement's cancel seen (a host loop of its call stopped for the abort bit): no line, the error unnamed
        let byte = core::sync::atomic::AtomicU8::new(gravity_abi::ctx::ABORT_BIT);
        let mut canceled = pg_error(oom(Some(NoSpill::BuildNestInner)));
        assert_eq!(cancelpoll::scoped(&byte, || (cancelpoll::canceled(), super::note_debt_refused(&mut canceled))), (true, None), "no line under a cancel");
        assert_eq!(*canceled, *plain);
    }

    /// NR-3: a kernel's own out of memory (kernels `pin_refused`: a stashed 53200 raised as HOST) after a pin the pool and
    /// the temp-file road refused on a debt home (driver `note_pin_debt`) reads to the client as without one; the row's
    /// line rides last in its log-only detail and is written once. Another raise takes the kept row and stays as it is.
    #[test]
    fn a_raised_pin_out_of_memory_names_its_debt_row_server_side() {
        use gravity_program::spill::NoSpill;
        let oom = || Box::new(PgError::error("out of memory".to_string()).with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY).with_detail("Failed on request of size 4096 in a Gravity pinned region (the statement's pinned pool is full).".to_string())); // alloc: Cold
        let host = || Error::Raised(raise::bit(raise::HOST));
        let client = |e: &PgError| (e.level, e.sqlstate, e.message.clone(), e.message_raw.clone(), e.detail.clone(), e.detail_raw.clone(), e.hint.clone(), e.context.clone(), e.location.clone(), e.cursor_position, e.internal_position, e.internal_query.clone()); // alloc: Cold
        assert_eq!(gravity_driver::take_pin_debt(), None);
        gravity_fn::stash_error(oom());
        let plain = pg_error(host());
        gravity_driver::note_pin_debt(Some(NoSpill::KeyCopies));
        gravity_fn::stash_error(oom());
        let mut named = pg_error(host());
        assert_eq!(gravity_driver::take_pin_debt(), None, "taken by the error");
        assert_eq!(client(&*named), client(&*plain), "the client reads the same 53200");
        assert_eq!(plain.detail_log(), None);
        let line = format!("gravity: memory: SPILL-DEBT KeyCopies: {}", NoSpill::KeyCopies.limitation()); // alloc: Cold
        assert_eq!(named.detail_log(), Some(format!("{}\n{line}", plain.detail().unwrap()).as_str()), "the line rides last in the log-only detail"); // alloc: Cold
        assert_eq!(super::note_debt_refused(&mut named).as_deref(), Some(line.as_str()), "written where the error is the statement's");
        assert_eq!(*named, *plain, "then the error is an unnamed one's");
        // another raise (an in-IR code): the row is taken and the error is as it is
        gravity_driver::note_pin_debt(Some(NoSpill::KeyCopies));
        let other = pg_error(Error::Raised(raise::bit(raise::DIV_ZERO)));
        assert_eq!((other.message(), other.detail_log()), ("division by zero", None));
        assert_eq!(gravity_driver::take_pin_debt(), None);
    }

    std::thread_local! {
        /// The backend's abort-class interrupt pending (tcop's AbortPending under INTERRUPTS_CAN_BE_PROCESSED), per test thread.
        static ABORT_PENDING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }

    /// C9: a call that failed after its host code stopped for the abort bit loses the thread's stashed errors while the stop
    /// is still the statement's to raise: on a pool worker (the byte is its leader's) and on the owner while an abort-class
    /// interrupt it can take is pending. Once the owner took the stop, the error and its stash stand.
    #[test]
    fn a_stopped_calls_stash_goes_only_while_the_stop_is_the_statements_to_raise() {
        use core::sync::atomic::AtomicU8;
        if !postgres_seams::abort_pending::is_installed() {
            postgres_seams::abort_pending::set(|| ABORT_PENDING.get());
        }
        let host = Error::Raised(raise::bit(raise::HOST));
        // a worker's call under its leader's byte
        let leader = AtomicU8::new(gravity_abi::ctx::ABORT_BIT);
        stash("a worker's stop placeholder");
        let _ = gravity_hosts::stash(Box::new(PgError::error("a worker host's placeholder".to_string()))); // alloc: Cold
        assert!(stopped(leader.as_ptr(), &host), "a worker's stop is its leader's to raise");
        assert!(gravity_fn::take_error().is_none() && gravity_hosts::take_error().is_none(), "both stashes dropped");
        // the owner's own call, its stop not taken yet
        let own = super::abort_u8();
        ABORT_PENDING.set(true);
        stash("the owner's stop placeholder");
        assert!(stopped(own.as_ptr(), &Error::host("pg")), "the owner raises its pending stop");
        assert!(gravity_fn::take_error().is_none(), "the placeholder dropped");
        // the owner took the stop: the error it raised for it stands
        ABORT_PENDING.set(false);
        stash("the stop's own error");
        assert!(!stopped(own.as_ptr(), &host), "a stop the owner already raised leaves the error as raised");
        assert_eq!(gravity_fn::take_error().as_deref().map(|e| e.message()), Some("the stop's own error"));
    }

    /// D1493: every unsupported kind has its own number and its own text (the driver's spill kind shared 5 with the out
    /// region's, so a sort past its runs printed "a by-reference output row wider than the out region").
    #[test]
    fn every_unsupported_kind_has_its_own_text() {
        let kinds = gravity_abi::status::UNSUPPORTED_KINDS;
        let texts: std::collections::BTreeSet<String> = kinds.iter().map(|&(k, _)| unsupported_message(k)).collect();
        assert_eq!(texts.len(), kinds.len(), "two kinds share a text: {texts:?}");
        let nums: std::collections::BTreeSet<u8> = kinds.iter().map(|&(k, _)| k).collect();
        assert_eq!(nums.len(), kinds.len(), "two kinds share a number: {kinds:?}");
        assert_eq!(gravity_driver::UNSUPPORTED_SPILL, gravity_abi::status::UNSUPPORTED_SPILL, "the driver's spill kind is abi's");
        assert!(unsupported_message(gravity_driver::UNSUPPORTED_SPILL).contains("a spill past"), "the spill kind names the spill");
    }

    /// `DECISIONS.md` 4 (D) b: a run-ahead pipeline's expression error carries its tag in the log-only detail, SQLSTATE
    /// first, the error's own detail after it; the client's message and detail stay PostgreSQL's. Another error (not the
    /// one noted), a cancel, and a run with no note carry none; each statement's error takes the note.
    #[test]
    fn a_run_ahead_error_carries_its_tag_in_the_log_detail_only() {
        let t = gravity_program::AheadTag { node: 3, node_type: "Materialize" };
        let div = Error::Raised(raise::bit(raise::DIV_ZERO));
        gravity_driver::take_ahead(None);
        gravity_driver::note_ahead_tag(Some(t), &div);
        let e = pg_error(div.clone());
        assert_eq!((e.message(), e.detail(), e.detail_log()), ("division by zero", None, Some("gravity ahead: 22012 node 3 Materialize (DECISIONS 4 (D) b)")));
        assert!(pg_error(div.clone()).detail_log().is_none(), "the note is taken by one error");
        gravity_driver::note_ahead_tag(Some(t), &div);
        assert!(pg_error(Error::Raised(raise::bit(raise::HOST))).detail_log().is_none(), "another error is not tagged");
        gravity_driver::note_ahead_tag(Some(t), &Error::Cancelled);
        gravity_driver::note_ahead_tag(None, &div);
        assert!(pg_error(div.clone()).detail_log().is_none(), "a cancel or an untagged pipeline notes nothing");
        let _ = gravity_hosts::stash(Box::new(PgError::error("value out of range".to_string()).with_sqlstate(types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE).with_detail("the host's detail".to_string()))); // alloc: Cold
        let host = Error::host("host");
        gravity_driver::note_ahead_tag(Some(t), &host);
        gravity_driver::note_ahead_tag(Some(gravity_program::AheadTag { node: 9, node_type: "Sort" }), &div);
        let e = pg_error(host);
        assert_eq!(e.detail(), Some("the host's detail"));
        assert_eq!(e.detail_log(), Some("gravity ahead: 22003 node 3 Materialize (DECISIONS 4 (D) b)\nthe host's detail"), "the first note stands");
    }

    /// D4206: a run-ahead error outside the data classes ruling (b) covers (here PostgreSQL's 0A000 of array_remove over
    /// a multidimensional array) is a refusal by name, its code and node in the reason and its text in the log-only
    /// detail without the run-ahead tag; the plan-order run may still re-run it. A resource limit (54000) keeps its
    /// error and tag, as a data error does; an untagged 0A000 is the statement's as it is.
    #[test]
    fn a_run_ahead_error_outside_the_data_classes_is_a_refusal() {
        let t = gravity_program::AheadTag { node: 4, node_type: "Index Scan" };
        let stash = |code, msg: &str| {
            let _ = gravity_hosts::stash(Box::new(PgError::error(msg.to_string()).with_sqlstate(code))); // alloc: Cold
            Error::host("host")
        };
        gravity_driver::take_ahead(None);
        let host = stash(types_error::ERRCODE_FEATURE_NOT_SUPPORTED, "removing elements from multidimensional arrays is not supported");
        gravity_driver::note_ahead_tag(Some(t), &host);
        let e = pg_error(host);
        assert_eq!(crate::runner::refusal_of(&e), Some("a part run ahead of the plan raised 0A000 at node 4 Index Scan, outside the data classes a run ahead may raise"));
        assert_eq!(e.detail_log(), Some("the run-ahead error: removing elements from multidimensional arrays is not supported"));
        assert!(crate::runner::tag_refused(&e) && crate::runner::reruns(&e) && !crate::runner::ahead_tagged(&e), "a refusal the plan-order run re-runs, not a tagged error");
        assert_eq!(crate::runner::tag_refusal_of(&e), crate::runner::refusal_of(&e), "the reason its named line logs");
        assert!(!crate::runner::reruns(&crate::runner::refused("another refusal")), "a lowering's refusal is no re-run");
        let host = stash(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED, "array size exceeds the maximum allowed");
        gravity_driver::note_ahead_tag(Some(t), &host);
        let e = pg_error(host);
        assert_eq!((e.message(), crate::runner::ahead_tagged(&e)), ("array size exceeds the maximum allowed", true), "a resource limit keeps its error");
        let host = stash(types_error::ERRCODE_FEATURE_NOT_SUPPORTED, "not supported");
        let e = pg_error(host);
        assert_eq!((e.message(), e.detail_log()), ("not supported", None), "an untagged error is the statement's");
        let host = stash(types_error::ERRCODE_QUERY_CANCELED, "canceling statement due to statement timeout");
        gravity_driver::note_ahead_tag(Some(t), &host);
        let e = pg_error(host);
        assert_eq!((e.sqlstate(), crate::runner::refusal_of(&e), crate::runner::reruns(&e)), (types_error::ERRCODE_QUERY_CANCELED, None, false), "a cancel during a run ahead is 57014, never refused or re-run");
        for (code, stands) in [(*b"22012", true), (*b"21000", true), (*b"23505", true), (*b"2202E", true), (*b"53200", true), (*b"54000", true), (*b"57014", true), (*b"40001", true), (*b"40P01", true), (*b"25P02", true), (*b"55P03", true), (*b"08006", true), (*b"58030", true), (*b"0A000", false), (*b"42804", false), (*b"XX000", false), (*b"55000", false)] {
            let e = PgError::error("e".to_string()).with_sqlstate(types_error::make_sqlstate(code)); // alloc: Cold
            assert_eq!(crate::runner::tag_stands(&e), stands, "{}", core::str::from_utf8(&code).unwrap());
        }
    }
}
