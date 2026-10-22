//! D4113: expr's retained scratch contexts (views.rs detoast, nodes/array.rs arrayfuncs) on gravity_fn's ScratchSlot. An
//! ERROR that unwinds out of a body leaves the slot idle (the next call takes it, reset to its keeper), and an inner
//! call's unwind caught inside an outer body leaves the outer's context busy and its bytes intact.

use gravity_expr::nodes::array::with_scratch as array_scratch;
use gravity_expr::views::with_detoast_scratch;
use gravity_fn::PgError;

const MIB: usize = 1 << 20;

fn fill(mcx: mcx::Mcx<'_>, n: usize, byte: u8) -> *mut u8 {
    let p = mcx.alloc_bytes(n).expect("scratch bytes").as_ptr();
    // SAFETY: `p` is `n` fresh bytes of the scratch context, live until its reset.
    unsafe { p.write_bytes(byte, n) };
    p
}

fn raise() -> ! {
    std::panic::panic_any(Box::new(PgError::error("out of memory")))
}

trait Slot {
    const NAME: &'static str;
    fn run<T>(f: impl FnOnce(mcx::Mcx<'_>) -> T) -> T;
}

struct Detoast;
impl Slot for Detoast {
    const NAME: &'static str = "gravity_expr detoast scratch";
    fn run<T>(f: impl FnOnce(mcx::Mcx<'_>) -> T) -> T {
        with_detoast_scratch(f)
    }
}

struct Array;
impl Slot for Array {
    const NAME: &'static str = "gravity_expr array scratch";
    fn run<T>(f: impl FnOnce(mcx::Mcx<'_>) -> T) -> T {
        array_scratch(|m| Ok(f(m))).expect("the body raised no error")
    }
}

fn unwinds<S: Slot>() -> bool {
    std::panic::catch_unwind(|| {
        S::run(|m| -> () {
            fill(m, MIB, 7);
            raise()
        })
    })
    .is_err()
}

fn idle_after_an_unwinding_body<S: Slot>() {
    assert!(unwinds::<S>());
    S::run(|m| {
        let cx = m.context();
        assert_eq!(cx.name(), S::NAME, "the slot stayed busy after the unwind");
        assert!(cx.mem_allocated() < MIB, "the unwound body's blocks are still held: {} bytes", cx.mem_allocated());
    });
}

fn nested_unwind_keeps_the_outer_busy<S: Slot>() {
    S::run(|m| {
        let mine = fill(m, 64, 0xA5);
        assert!(unwinds::<S>());
        S::run(|inner| {
            assert_eq!(inner.context().name(), format!("{} (nested)", S::NAME), "the inner unwind gave the outer's context away");
            fill(inner, 64, 0x5A);
        });
        // SAFETY: `mine` is 64 bytes of the outer's context, not reset before this call returns.
        assert!(unsafe { core::slice::from_raw_parts(mine, 64) }.iter().all(|&b| b == 0xA5), "the outer's scratch bytes changed under it");
    });
    S::run(|m| assert_eq!(m.context().name(), S::NAME, "the outer's return did not give the slot back"));
}

#[test]
fn detoast_scratch_is_idle_after_an_unwinding_body() {
    idle_after_an_unwinding_body::<Detoast>();
}

#[test]
fn detoast_nested_unwind_keeps_the_outer_busy() {
    nested_unwind_keeps_the_outer_busy::<Detoast>();
}

#[test]
fn array_scratch_is_idle_after_an_unwinding_body() {
    idle_after_an_unwinding_body::<Array>();
}

#[test]
fn array_nested_unwind_keeps_the_outer_busy() {
    nested_unwind_keeps_the_outer_busy::<Array>();
}
