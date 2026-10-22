//! D4414 (born red): D4402 (B)'s refusal on D4403's small road. A statement the memory governor canceled, whose cancel its
//! run's poll already consumed (an errorhold re-run's admission, a PL/pgSQL handler that caught the 53200 and queries
//! again), asks a small grant while the small buffer is full. Its ledger refuses every big-lane take (`fits_raw_admit`)
//! and nothing is left to raise, so before the fix it waited out the whole wait (pgrust.gravity_grant_wait_ms, 600 s) and
//! took the pool's refusal; now it is refused at once with the cancel's own 53200. With room in the buffer `take_small`
//! admits it as before (it never reads the ledger): nothing served is refused.
//! Its own binary: it arms the process-wide grant pool, which the lib's tests read.

use std::time::{Duration, Instant};

use gravity_driver::{Grant, POOL};
use gravity_exec::inputs::knobs::grant::admit_small_waiting;
use mcx::governor;

const MIB: u64 = 1 << 20;

#[test]
fn d4414_a_memory_canceled_small_ask_is_refused_at_the_full_buffer_and_admitted_with_room() {
    POOL.gate(0);
    POOL.arm(256 * MIB);
    POOL.set_buffer(16 * MIB);
    assert_eq!(POOL.reserve(), 16 * MIB, "a 16 MiB buffer under a 256 MiB pool");
    // another small statement fills the buffer
    let (seated_tx, seated) = std::sync::mpsc::channel();
    let (end, end_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let a = POOL.admit_small_until(Grant::of(MIB), &|_| 16 * MIB, Duration::ZERO, &|| false).expect("a buffer").expect("seated");
        assert!(a.small, "in the buffer");
        seated_tx.send(()).unwrap();
        let _ = end_rx.recv_timeout(Duration::from_secs(30));
        drop(a);
    });
    seated.recv().unwrap();
    let (full, ms, room) = std::thread::spawn(move || {
        let need = |_: Grant| 8 * MIB;
        // the ladder's R6 cancels this statement; its run's poll takes the cancel and raises it
        governor::register(4014, 44141);
        let ctx = mcx::MemoryContext::new("d4414 hog");
        governor::statement_start(0, true);
        governor::arm(true);
        ctx.mcx().alloc_uninit_bytes(core::alloc::Layout::from_size_align(256 << 20, 8).unwrap()).unwrap();
        let v = governor::cancel_largest_victim(90, 32 << 20).expect("the hog is chosen");
        assert_eq!(v.backend, Some((4014, 44141)), "{v:?}");
        assert!(governor::take_watchdog_cancel().is_some(), "the run's poll raised the cancel");
        // its small ask finds the buffer full
        let t0 = Instant::now();
        let r = admit_small_waiting(&POOL, Grant::of(MIB), &need, Duration::from_secs(3));
        let ms = t0.elapsed().as_millis();
        let full = match r {
            Err(e) => Ok((e.sqlstate(), e.detail().map(str::to_string))),
            Ok(r) => Err(format!("{r:?}")), // alloc: Cold
        };
        // the buffer has room again: take_small admits it, as before the fix
        end.send(()).unwrap();
        holder.join().unwrap();
        let room = match admit_small_waiting(&POOL, Grant::of(MIB), &need, Duration::from_secs(3)) {
            Ok(Some(Ok(a))) => Ok(a.small),
            other => Err(format!("{other:?}")), // alloc: Cold
        };
        governor::arm(false);
        governor::statement_end();
        drop(ctx);
        (full, ms, room)
    })
    .join()
    .unwrap();
    POOL.set_buffer(0);
    POOL.arm(0);
    let (state, detail) = full.unwrap_or_else(|r| panic!("a canceled small ask queued for the buffer after {ms} ms: {r}"));
    assert_eq!(state, types_error::ERRCODE_OUT_OF_MEMORY);
    assert!(detail.as_deref().is_some_and(|d| d.contains("90% of its limit")), "the cancel's own text: {detail:?}");
    assert!(ms < 500, "refused at once, not after a wait ({ms} ms)");
    assert_eq!(room, Ok(true), "with room the buffer admits the canceled statement's small ask, as before");
}
