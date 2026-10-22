//! D4113: an ERROR that unwinds out of a `with_scratch` body leaves the thread's retained scratch context idle (the next
//! call takes it, reset to its keeper), and an inner call's unwind caught inside an outer body leaves the outer's
//! context busy and its bytes intact (the inner gets a context of its own).

use gravity_fn::{ok, with_scratch, PgError};

const MIB: usize = 1 << 20;

fn fill(mcx: mcx::Mcx<'_>, n: usize, byte: u8) -> *mut u8 {
    let p = mcx.alloc_bytes(n).expect("scratch bytes").as_ptr();
    // SAFETY: `p` is `n` fresh bytes of the scratch context, live until its reset.
    unsafe { p.write_bytes(byte, n) };
    p
}

fn unwinds() -> bool {
    std::panic::catch_unwind(|| {
        with_scratch(|mcx| {
            fill(mcx, MIB, 7);
            std::panic::panic_any(Box::new(PgError::error("out of memory")))
        })
    })
    .is_err()
}

#[test]
fn scratch_is_idle_after_an_unwinding_body() {
    assert!(unwinds());
    with_scratch(|mcx| {
        let cx = mcx.context();
        assert_eq!(cx.name(), "gravity_fn scratch", "the slot stayed busy after the unwind");
        assert!(cx.mem_allocated() < MIB, "the unwound body's blocks are still held: {} bytes", cx.mem_allocated());
        ok(0)
    });
}

#[test]
fn nested_unwind_keeps_the_outer_busy() {
    with_scratch(|mcx| {
        let mine = fill(mcx, 64, 0xA5);
        assert!(unwinds());
        with_scratch(|inner| {
            assert_eq!(inner.context().name(), "gravity_fn scratch (nested)", "the inner unwind gave the outer's context away");
            fill(inner, 64, 0x5A);
            ok(0)
        });
        // SAFETY: `mine` is 64 bytes of the outer's context, not reset before this call returns.
        assert!(unsafe { core::slice::from_raw_parts(mine, 64) }.iter().all(|&b| b == 0xA5), "the outer's scratch bytes changed under it");
        ok(0)
    });
    with_scratch(|mcx| {
        assert_eq!(mcx.context().name(), "gravity_fn scratch", "the outer's return did not give the slot back");
        ok(0)
    });
}
