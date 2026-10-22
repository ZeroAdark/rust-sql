//! The one status vocabulary (abi-v2 §4): `>= 0` the call's value; negative codes with an optional payload in bits 8+ (`-(code | payload << 8)`), shared by host calls and kernel returns.

pub const OK: i64 = 0;
pub const DONE: i64 = 0;
pub const STOP: i64 = -1;
pub const CANCELLED: i64 = -2;
pub const ERROR: i64 = -3;
pub const SUSPEND: i64 = -4;
pub const RECHECK: i64 = -5;
pub const UNSUPPORTED: i64 = -6;
/// A deferred per-entry fault (expr.md §2.2): row `code` of table lane `lane` on `site` was selected; the driver re-evaluates that entry serially and raises its error.
pub const FAULT: i64 = -7;
/// A fault's entry code width: 2^24 codes, pgrc's widest packed dictionary code (`pgrc_am` ingest's `DICT_NDV_CAP`); a
/// part with more codes refuses before a kernel can name one (`gravity_dict` table, the text hosts), so the code always fits.
pub const FAULT_CODE_BITS: u32 = 24;
/// D824 (V4R-298): the probe of a lazy build (PG's outer-first hash join) reached its first row before the build ran: the
/// call stopped with the cursor at that row; the runner runs the build (consumed index in the payload) and resumes the call.
pub const BUILD_WAIT: i64 = -8;
/// D3168 S0: a pipeline's plain function found its sealed build past its probe's guard before it did anything: the call
/// runs the pipeline's staged function instead, compiled the first time one is wanted (gravity_program `Code::call`, which
/// never hands this status on).
pub const STAGED: i64 = -9;
/// `ERROR` with a raise mask in the payload: a bit per `raise::*` code, set by the expression
/// forms that raised on the row; the driver reports the lowest set bit's sqlstate and message.
pub const RAISE_BITS: u32 = 55;

pub const fn raised(mask: u64) -> i64 {
    -((-ERROR) | (((mask & ((1 << RAISE_BITS) - 1)) as i64) << 8))
}

/// The raise codes an expression form leaves in the status word (`raised`): PostgreSQL's sqlstate
/// and message per code, the incumbent's texts; `HOST` and `TABLE` carry no text of their own (the host's stashed error, or the faulting table entry's re-evaluation, names the error).
pub mod raise {
    pub const INT2_RANGE: u8 = 0;
    pub const INT4_RANGE: u8 = 1;
    pub const INT8_RANGE: u8 = 2;
    pub const DIV_ZERO: u8 = 3;
    pub const FLOAT_OVF: u8 = 4;
    pub const FLOAT_UNDER: u8 = 5;
    pub const TS_RANGE: u8 = 6;
    pub const HOST: u8 = 7;
    pub const TABLE: u8 = 8;
    pub const STEP_ZERO: u8 = 9;
    /// D2349: `date - date` over `infinity` / `-infinity` (PG's `date_mi`).
    pub const DATE_NOT_FINITE: u8 = 10;
    pub const CODES: u8 = 11;

    pub const fn bit(code: u8) -> u64 {
        1 << code
    }

    /// The lowest code of a mask (`raised`'s payload).
    pub const fn first(mask: u64) -> u8 {
        mask.trailing_zeros() as u8
    }

    /// (sqlstate, message) of a code; `None` for a code whose error the host or the table names.
    pub const fn describe(code: u8) -> Option<(&'static [u8; 5], &'static str)> {
        match code {
            INT2_RANGE => Some((b"22003", "smallint out of range")),
            INT4_RANGE => Some((b"22003", "integer out of range")),
            INT8_RANGE => Some((b"22003", "bigint out of range")),
            DIV_ZERO => Some((b"22012", "division by zero")),
            FLOAT_OVF => Some((b"22003", "value out of range: overflow")),
            FLOAT_UNDER => Some((b"22003", "value out of range: underflow")),
            TS_RANGE => Some((b"22008", "timestamp out of range")),
            STEP_ZERO => Some((b"22023", "step size cannot equal zero")),
            DATE_NOT_FINITE => Some((b"22008", "cannot subtract infinite dates")),
            _ => None,
        }
    }
}

pub const UNSUPPORTED_SHAPE: u8 = 1;
pub const UNSUPPORTED_SCRATCH: u8 = 2;
pub const UNSUPPORTED_NULL_KEY: u8 = 3;
pub const UNSUPPORTED_ON_WORKER: u8 = 4;
/// A by-reference output datum wider than the program's whole out region (lower.md §15.13).
pub const UNSUPPORTED_OUT_REGION: u8 = 5;
/// D597: a group key outside the dense domain the owner was opened with (a stale domain): the owner phase reruns round 1
/// through its plain form (`HashAggPartial::plain`, the hash reduce) from the sources it only read.
pub const UNSUPPORTED_DOMAIN: u8 = 6;
/// D1493: a spill past its files' limits (the driver's split generations, a spool's runs); its own kind: it was 5, the out
/// region's, and a sort that spilled too many runs printed the out region's text.
pub const UNSUPPORTED_SPILL: u8 = 7;
/// D1493: every kind by its name, one entry each: no two kinds share a number (`glue::unsupported_message` names each).
pub const UNSUPPORTED_KINDS: [(u8, &str); 7] = [(UNSUPPORTED_SHAPE, "shape"), (UNSUPPORTED_SCRATCH, "scratch"), (UNSUPPORTED_NULL_KEY, "null_key"), (UNSUPPORTED_ON_WORKER, "on_worker"), (UNSUPPORTED_OUT_REGION, "out_region"), (UNSUPPORTED_DOMAIN, "domain"), (UNSUPPORTED_SPILL, "spill")];

/// `write` / `lock` values; `WRITE_CONFLICT` = ON CONFLICT DO UPDATE's locked row, its cells in the conflict site (the write site's successor).
pub const WRITE_SKIPPED: i64 = 0;
pub const WRITE_DONE: i64 = 1;
pub const WRITE_RECHECK: i64 = 2;
pub const WRITE_CONFLICT: i64 = 3;
/// M8-WRITE W6: a MERGE's MATCHED action found its target row concurrently deleted (read committed): the row is NOT
/// MATCHED now, and its NOT MATCHED [BY TARGET] actions run (PostgreSQL 18's ExecMergeMatched, TM_Deleted).
pub const WRITE_NOT_MATCHED: i64 = 4;
/// W6-D1112 (V4R-528): the row's check option parked at a SubPlan read (`program::WRITE_SUB_ARGS`).
pub const WRITE_SUBPLAN: i64 = 5;

pub const fn suspend(out_full: bool) -> i64 {
    -((-SUSPEND) | ((out_full as i64) << 8))
}

pub const fn recheck(site: usize) -> i64 {
    -((-RECHECK) | ((site as i64) << 8))
}

pub const fn build_wait(consumed: u8) -> i64 {
    -((-BUILD_WAIT) | ((consumed as i64) << 8))
}

pub const fn unsupported(kind: u8) -> i64 {
    -((-UNSUPPORTED) | ((kind as i64) << 8))
}

pub const fn fault(site: u8, lane: u8, code: u32) -> i64 {
    fault_at(site, lane, code, 0)
}

/// `fault` with the window's part (the runner renders entry `code` of that part to re-evaluate it).
pub const FAULT_PART_BITS: u32 = 15;
/// The fault word: the 8-bit code, then site 8 | lane 8 | code `FAULT_CODE_BITS` | part `FAULT_PART_BITS`; all of it must fit
/// the 63 magnitude bits of a negative `i64`, so widening either field is a compile error here, not a silent overlap.
const FAULT_PART_SHIFT: u32 = 24 + FAULT_CODE_BITS;
const _: () = assert!(FAULT_PART_SHIFT + FAULT_PART_BITS <= 63);
pub const fn fault_at(site: u8, lane: u8, code: u32, part: u32) -> i64 {
    -((-FAULT) | ((site as i64) << 8) | ((lane as i64) << 16) | ((code as i64 & ((1 << FAULT_CODE_BITS) - 1)) << 24) | ((part as i64 & ((1 << FAULT_PART_BITS) - 1)) << FAULT_PART_SHIFT))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok(i64),
    Stop,
    Cancelled,
    Error,
    /// `ERROR` with a raise mask (`raised`): `raise::first(mask)` is the error reported.
    Raised { mask: u64 },
    Suspend { out_full: bool },
    Recheck { site: u8 },
    /// D824: the lazy build the pipeline's `consumed`-th slot names must run before the stopped call resumes.
    BuildWait { consumed: u8 },
    Unsupported { kind: u8 },
    Fault { site: u8, lane: u8, code: u32, part: u32 },
    Bad(i64),
}

impl Status {
    pub const fn decode(v: i64) -> Status {
        if v >= 0 {
            return Status::Ok(v);
        }
        let (code, payload) = (-((-v) & 0xff), (-v) >> 8);
        match code {
            STOP if payload == 0 => Status::Stop,
            CANCELLED if payload == 0 => Status::Cancelled,
            ERROR if payload == 0 => Status::Error,
            ERROR => Status::Raised { mask: payload as u64 },
            SUSPEND if payload <= 1 => Status::Suspend { out_full: payload == 1 },
            RECHECK if payload < 256 => Status::Recheck { site: payload as u8 },
            BUILD_WAIT if payload < 256 => Status::BuildWait { consumed: payload as u8 },
            UNSUPPORTED if payload < 256 => Status::Unsupported { kind: payload as u8 },
            FAULT => Status::Fault { site: payload as u8, lane: (payload >> 8) as u8, code: (payload >> 16) as u32 & ((1 << FAULT_CODE_BITS) - 1), part: (payload >> (FAULT_PART_SHIFT - 8)) as u32 & ((1 << FAULT_PART_BITS) - 1) },
            _ => Status::Bad(v),
        }
    }

    pub const fn encode(self) -> i64 {
        match self {
            Status::Ok(v) => v,
            Status::Stop => STOP,
            Status::Cancelled => CANCELLED,
            Status::Error => ERROR,
            Status::Raised { mask } => raised(mask),
            Status::Suspend { out_full } => suspend(out_full),
            Status::Recheck { site } => recheck(site as usize),
            Status::BuildWait { consumed } => build_wait(consumed),
            Status::Unsupported { kind } => unsupported(kind),
            Status::Fault { site, lane, code, part } => fault_at(site, lane, code, part),
            Status::Bad(v) => v,
        }
    }
}

/// D310: an unwind a host frame caught that is not a PostgreSQL ERROR (a FATAL's thread exit, a PANIC, a signal's kill, a
/// bug's panic) must not cross the `extern "C"` host frames the JIT'd code calls (Rust aborts there: the whole server, since
/// backends are threads). The host parks it here and returns its error status; the program's caller resumes it the moment
/// the JIT'd entry returns (`gravity_program::Code::call`), before anything reads the error the host left. One slot per
/// thread (a pipeline runs on one thread); a second payload parked before the resume is dropped, the first goes on.
pub mod unwind {
    extern crate std;
    use std::any::Any;
    use std::boxed::Box;
    use std::cell::Cell;

    std::thread_local! {
        static PARKED: Cell<Option<Box<dyn Any + Send>>> = const { Cell::new(None) };
    }

    /// Park `payload` for the program's caller (`resume`).
    #[cold]
    #[inline(never)]
    pub fn park(payload: Box<dyn Any + Send>) {
        PARKED.with(|p| {
            let first = p.take();
            p.set(Some(first.unwrap_or(payload)));
        });
    }

    /// The one landing pad (V4R-16, D314): `body` with any unwind out of it parked here instead of crossing the
    /// `extern "C"` frame above (Rust aborts there; a backend is a thread, so the whole server goes). `None` means the
    /// body unwound: the entry answers the failure its caller already reads, and the program's caller resumes the
    /// unwind once the program returns (`resume`). The non-unwinding path is `body` itself. Every host entry's pad is
    /// this one: the kernels' `contract::entryN::<H>` and `guard!`, the driver's `hostimpl::guarded`.
    #[inline(always)]
    pub fn pad<T>(body: impl FnOnce() -> T) -> Option<T> {
        match std::panic::catch_unwind(core::panic::AssertUnwindSafe(body)) { // unwind-ok: c-callback
            Ok(v) => Some(v),
            Err(payload) => {
                park(payload);
                None
            }
        }
    }

    /// The one catch of every unwind boundary that is not a host entry's pad (no JIT'd frame lies above it): `body`'s
    /// value, or the payload it unwound with, handed back to the caller, never parked; the caller tells a refusal or a
    /// bug's panic from a PostgreSQL error or a thread exit and resumes those. Its callers: the lowerer (lowerv4 `catch`,
    /// v3 design §3.5), the compile under a catch (backend `compile_caught`), a pool worker's containment of its job and
    /// claim (pool), and a claim's pin release during an unwind (driver section `ReleaseOnUnwind`).
    #[inline(always)]
    pub fn catch<T>(body: impl FnOnce() -> T) -> Result<T, Box<dyn Any + Send>> {
        std::panic::catch_unwind(core::panic::AssertUnwindSafe(body)) // unwind-ok: c-callback
    }

    /// Resume the parked unwind, if any: called where no JIT'd frame lies between it and the Rust caller.
    #[inline]
    pub fn resume() {
        if let Some(payload) = PARKED.with(|p| p.take()) {
            std::panic::resume_unwind(payload);
        }
    }
}
