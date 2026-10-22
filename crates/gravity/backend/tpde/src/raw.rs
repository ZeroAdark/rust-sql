//! The TPDE backend's unsafe steps (rule R6, its raw home): the installed clock read back from its word, the entry block's
//! params viewed as the u32s TPDE reads, and the mapping of the assembled code with the program's host addresses.
#![allow(unsafe_code)]

use gravity_ir::{Program, Value};
use tpde_assembler::AssemblerElf;
use tpde_elf_mapper::ElfMapper;
use types_error::PgResult;

use crate::adaptor;

/// Monotonic nanosecond clock installed by the host (`set_clock`): the
/// crate is `no_std`, so the per-phase compile timing (`CompileTiming`)
/// reads its clock through this pointer. Unset, every phase reads zero.
static CLOCK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the nanosecond clock `CompileTiming` is measured with.
pub fn set_clock(f: fn() -> u64) {
    CLOCK.store(f as usize, core::sync::atomic::Ordering::Relaxed);
}

/// The installed clock, or 0.
#[inline]
pub fn now_ns() -> u64 {
    let p = CLOCK.load(core::sync::atomic::Ordering::Relaxed);
    if p == 0 {
        return 0;
    }
    // SAFETY: `p` was stored by `set_clock` from a `fn() -> u64`.
    let f: fn() -> u64 = unsafe { core::mem::transmute::<usize, fn() -> u64>(p) };
    f()
}

/// The unwind payload of a compile stopped at a `stop_point`: gravity_backend's catch (`compile_caught`) turns it into the
/// compile's error, and the caller takes the pending interrupt (a cancel, a statement_timeout, a terminate).
pub struct Interrupted;

/// A stop point of the compile (D2909: a long view build, analysis or encode took no cancel): with an interrupt pending
/// it unwinds with `Interrupted`, no panic hook run, every frame's tables dropped on the way (the view, the scratch, the
/// analyzer's), and the catch releases the code's context. One seam load when no backend is installed, one call when none
/// is pending.
#[inline]
pub fn stop_point() {
    if interrupt_pending() {
        stop();
    }
}

/// C6 (d): the interrupt facade's `cancelpoll::interrupts::pending` for the calling thread (false when no backend is
/// installed), read without unwinding: D3811's wait for another thread's compile of the same module (gravity_backend
/// cache `Flight::wait`) ends at it, as a stop point ends a compile. The facade is called directly; the probe word the
/// host used to install it into (`set_interrupt_probe`) is gone.
#[inline]
pub fn interrupt_pending() -> bool {
    cancelpoll::interrupts::pending()
}

#[cold]
#[inline(never)]
fn stop() -> ! {
    extern crate std;
    std::panic::resume_unwind(alloc::boxed::Box::new(Interrupted))
}

/// Block params as the u32 tuple fields they wrap.
#[inline]
pub(crate) fn values_u32(params: &[Value]) -> &[u32] {
    // SAFETY: `Value(pub u32)` has the layout of u32.
    unsafe { core::slice::from_raw_parts(params.as_ptr() as *const u32, params.len()) }
}

/// Maps the assembled code, each host symbol (`h<i>`) resolved to `program`'s host function `i`'s address.
pub(crate) fn map(mapper: &mut ElfMapper<'_>, assembler: &mut AssemblerElf<'_>, program: &Program) -> PgResult<bool> {
    let resolve = |name: &[u8]| -> usize {
        match adaptor::LinkNames::parse(name) {
            Some((true, hi)) => program.host_fns.get(hi).map_or(0, |h| h.addr),
            _ => 0,
        }
    };
    // SAFETY: host addresses are live `extern "C"` functions supplied by
    // the lowerer; nothing executes while mapping.
    unsafe { mapper.map(assembler, resolve) }
}

/// The view tables' growth at `std::vec::Vec`'s speed (`view::GrowHot`). allocator-api2's `Vec` (a stable crate: no
/// specialization) extends element by element, a capacity check and a length store per element, where std's copies a
/// `Copy` slice whole and fills from an iterator of known length with neither. Each step here writes only into room the
/// caller's `view::room` took (asserted below), so a refused growth is its context's ERROR before any write.
pub(crate) mod grow {
    use mcx::PgVec;

    /// `x` pushed into the room for it (`view::room(v, 1)` first): one length store, no second capacity check.
    #[inline(always)]
    pub(crate) fn push_in_room<T>(v: &mut PgVec<'_, T>, x: T) {
        let len = v.len();
        assert!(len < v.capacity());
        // SAFETY: `len < capacity` (asserted): the slot past the elements is allocated and unused.
        unsafe {
            v.as_mut_ptr().add(len).write(x);
            v.set_len(len + 1);
        }
    }

    /// `s` copied into the room for it (`view::room(v, s.len())` first): one copy.
    #[inline(always)]
    pub(crate) fn copy_in_room<T: Copy>(v: &mut PgVec<'_, T>, s: &[T]) {
        let len = v.len();
        assert!(v.capacity() - len >= s.len());
        // SAFETY: the room holds `s.len()` elements past `len` (asserted); `T: Copy`, so a byte copy is the elements; `s`
        // is borrowed shared while `v` is borrowed exclusively, so the two do not overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(s.as_ptr(), v.as_mut_ptr().add(len), s.len());
            v.set_len(len + s.len());
        }
    }

    /// Up to the room past `v`'s elements (at most `n`) taken from `it` and written with one length store at the end;
    /// the rest of `it` stays for the caller. A panic in `it` leaves `v` at its elements before the call (the written
    /// ones leak, as `Vec::extend`'s would not, which a `Copy` table never notices).
    #[inline(always)]
    pub(crate) fn fill_room<T, I: Iterator<Item = T>>(v: &mut PgVec<'_, T>, it: &mut I, n: usize) {
        let len = v.len();
        let n = n.min(v.capacity() - len);
        let p = v.as_mut_ptr();
        let mut k = 0;
        for x in it.take(n) {
            // SAFETY: `k < n <= capacity - len`: the slot is allocated and unused.
            unsafe { p.add(len + k).write(x) };
            k += 1;
        }
        // SAFETY: the `k` slots past `len` are written.
        unsafe { v.set_len(len + k) };
    }
}
