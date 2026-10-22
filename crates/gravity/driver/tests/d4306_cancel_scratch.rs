//! D4306: a kernel's scratch refusal (kind 2) in a call that stopped for its statement's cancel is the cancel (57014, as
//! PostgreSQL raises it), never the pool's out of memory (53200, `Entry::run_pipeline`'s pool_refused, D2845). A kernel
//! pin past G whose pool wait the cancel ends (or whose stepped zeroing it cancels) returns null with no temp file
//! (`cancelpoll::seen`), and the kernel answers kind 2 on the null. Without a cancel the same refusal stays the named out of
//! memory. One test: it installs the process's spill factory (no temp file: the pin's last road refuses too) and arms the
//! process-wide pool.

use gravity_driver_rig as common;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_abi::PtrWord;
use gravity_driver::{install_spill_factory, Entry, Error, Grant, SpillFactory, SpillSet, TempMap, POOL, REGIONS};
use gravity_program::Code;

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
/// Past the 16 MiB pool: refused at once.
const ASK: u64 = 64 * MIB;

/// A temp-file road that maps nothing: the pin past the pool is refused on every road.
struct NoFile;

impl SpillFactory for NoFile {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn map_temp(&self, _bytes: usize) -> gravity_driver::Result<Option<TempMap>> {
        Ok(None)
    }

    fn maps_temp(&self) -> bool {
        true
    }
}

static NO_FILE: NoFile = NoFile;

thread_local! {
    /// The cancel arrives during the call: the kernel raises its statement's abort bit and polls before it pins.
    static CANCEL: Cell<bool> = const { Cell::new(false) };
}

/// Whether the kernel's pin was served.
static GOT: AtomicBool = AtomicBool::new(false);

/// The driver's `STOPPED` as exec installs it: the stop is the statement's to raise.
fn stopped(_byte: *const u8, _e: &Error) -> bool {
    true
}

unsafe extern "C-unwind" fn k_pin(ctx: *mut gravity_abi::Ctx) -> i64 {
    if CANCEL.get() {
        // SAFETY: the driver binds `ctx` for the call; its cancel byte is the statement's, lent for the call
        unsafe { (*((*ctx).cancel.ptr() as *const AtomicU8)).fetch_or(cancelpoll::ABORT_BIT, Ordering::SeqCst) };
        let _ = cancelpoll::canceled();
    }
    // SAFETY: as above
    GOT.store(!unsafe { gravity_abi::program::pin(ctx, ASK) }.is_null(), Ordering::Relaxed);
    unsupported(UNSUPPORTED_SCRATCH)
}

#[test]
fn a_scratch_refusal_in_a_canceled_call_is_the_cancel() {
    assert!(install_spill_factory(&NO_FILE), "this binary owns the process's spill factory");
    let _ = gravity_driver::STOPPED.set(stopped);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(16 * MIB);
    let mut entry = Entry::new(2);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let mut wrong = Vec::new();
    for cancel in [false, true] {
        CANCEL.set(cancel);
        let flag = AtomicU8::new(0);
        let sink = FakeSink::new();
        let mut sink_ref = &sink;
        let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
        let program = Arc::new(p_code(Code::hand(k_pin), 1, 0));
        let a = std::rc::Rc::new(POOL.admit(Grant::of(MIB), 1, 1, Duration::from_millis(10)).unwrap());
        let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
        let r = entry.run(&mut exec, &mut b, gravity_driver::Call::batch(1));
        let got = GOT.load(Ordering::Relaxed);
        // canceled: the statement's cancel; not: the pool's refusal, the named out of memory (D2845)
        let right = !got
            && if cancel {
                r == Err(Error::Cancelled)
            } else {
                a.refused() != 0 && matches!(r, Err(Error::OutOfMemory { capacity, .. }) if capacity == 16 * MIB)
            };
        if !right {
            wrong.push(format!("cancel {cancel}: pin served {got}, refused {} B kept, {r:?}", a.refused()));
        }
        entry.end(exec, false);
        drop(a);
    }
    CANCEL.set(false);
    assert!(wrong.is_empty(), "a kind 2 of a canceled call is the cancel; without one, the pool's refusal:\n{}", wrong.join("\n"));
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statements' ends");
    POOL.arm(0);
}
