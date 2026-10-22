//! D4305: a leader's draw past W and G the pool refused takes the session's temp file (a kernel arena grow's
//! `grow_mapped`, a kernel pin's `pin_mapped`, a spill take's `map_owners`); when the file is refused too (53400 past
//! temp_file_limit, ENOSPC), the kind 2 the kernel answers raises the file's own error, as `v4::take`'s already did,
//! never the pool's refusal named 53200 (`Entry::run_pipeline`'s pool_refused, D2845). One test: it installs the process's
//! spill factory and arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_driver::{install_spill_factory, Entry, Error, Grant, SpillFactory, SpillSet, TempMap, POOL, REGIONS};
use gravity_program::Code;

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
/// Past the 16 MiB pool: refused at once.
const ASK: u64 = 64 * MIB;
const FILE_ERROR: &str = "test: temporary file size exceeds temp_file_limit";

/// A session whose temp-file road refuses every file (temp_file_limit's 53400 in a server).
struct FullDisk;

impl SpillFactory for FullDisk {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn map_temp(&self, _bytes: usize) -> gravity_driver::Result<Option<TempMap>> {
        Err(Error::host(FILE_ERROR))
    }

    fn maps_temp(&self) -> bool {
        true
    }
}

static FULL_DISK: FullDisk = FullDisk;

/// Whether the kernel's draw was served.
static GOT: AtomicBool = AtomicBool::new(false);

type Kernel = unsafe extern "C-unwind" fn(*mut gravity_abi::Ctx) -> i64;

unsafe extern "C-unwind" fn k_grow(ctx: *mut gravity_abi::Ctx) -> i64 {
    // SAFETY: the driver binds `ctx` for the call
    GOT.store(unsafe { gravity_abi::program::arena_grow(ctx, ASK) } != 0, Ordering::Relaxed);
    unsupported(UNSUPPORTED_SCRATCH)
}

unsafe extern "C-unwind" fn k_pin(ctx: *mut gravity_abi::Ctx) -> i64 {
    // SAFETY: as `k_grow`
    GOT.store(!unsafe { gravity_abi::program::pin(ctx, ASK) }.is_null(), Ordering::Relaxed);
    unsupported(UNSUPPORTED_SCRATCH)
}

unsafe extern "C-unwind" fn k_take(ctx: *mut gravity_abi::Ctx) -> i64 {
    // SAFETY: as `k_grow`
    GOT.store(!unsafe { gravity_driver::mem::spill_host::v4::take(ctx, ASK as usize) }.is_null(), Ordering::Relaxed);
    unsupported(UNSUPPORTED_SCRATCH)
}

#[test]
fn a_refused_temp_file_raises_its_own_error() {
    assert!(install_spill_factory(&FULL_DISK), "this binary owns the process's spill factory");
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(16 * MIB);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let mut entry = Entry::new(2);
    let mut wrong = Vec::new();
    for (road, k) in [("arena grow", k_grow as Kernel), ("kernel pin", k_pin), ("spill take", k_take)] {
        let program = Arc::new(p_code(Code::hand(k), 1, 0));
        let a = std::rc::Rc::new(POOL.admit(Grant::of(MIB), 1, 1, Duration::from_millis(10)).unwrap());
        let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
        let r = entry.run(&mut exec, &mut b, gravity_driver::Call::batch(1));
        let got = GOT.load(Ordering::Relaxed);
        if got || r != Err(Error::host(FILE_ERROR)) {
            wrong.push(format!("{road}: served {got}, {r:?}"));
        }
        entry.end(exec, false);
        drop(a);
    }
    assert!(wrong.is_empty(), "the refused temp file's own error, never the pool's refusal:\n{}", wrong.join("\n"));
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statements' ends");
    POOL.arm(0);
}
