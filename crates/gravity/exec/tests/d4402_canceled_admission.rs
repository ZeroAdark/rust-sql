//! D4402 (born red): a statement the memory governor canceled is refused its grant at once, with the cancel's own 53200.
//! The pool holds another statement's bytes (240 of 256 MiB, on its own thread: less than the 64 MiB floor is left), and
//! this statement's cancel was consumed by its run's poll before it asked again (`take_watchdog_cancel`, as before an
//! errorhold re-run's admission or after a PL/pgSQL handler caught the 53200). Its ledger refuses every take
//! (`fits_raw_admit`), so before the fix it waited out the whole wait with nothing left to raise (memtest's e2 take: three
//! hash joins, 57014 at 600 s), then took the pool's refusal. A statement nobody canceled still waits for the holder.
//! Its own binary: it arms the process-wide grant pool, which the lib's tests read (`v4_build_refused`).

use std::time::{Duration, Instant};

use gravity_driver::{Grant, POOL};
use gravity_exec::inputs::knobs::grant::admit_waiting;
use mcx::governor;

const MIB: u64 = 1 << 20;

#[test]
fn d4402_a_memory_canceled_statement_is_refused_at_once_with_its_cancel() {
    POOL.gate(0);
    POOL.arm(256 * MIB);
    let (held_tx, held) = std::sync::mpsc::channel();
    let (end, end_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let r = POOL.admit_need_until(Grant::of(240 * MIB), &|g: Grant| g.bytes, Duration::from_millis(100), &|| false).expect("the holder is admitted");
        held_tx.send(()).unwrap();
        end_rx.recv().unwrap();
        drop(r);
    });
    held.recv().unwrap();
    let (outcome, ms, waited_ms) = std::thread::spawn(|| {
        let need = |g: Grant| g.bytes;
        // a statement nobody canceled waits for the holder's bytes, as before
        let t0 = Instant::now();
        let r = admit_waiting(&POOL, Grant::of(128 * MIB), &need, Duration::from_millis(300));
        let waited_ms = t0.elapsed().as_millis();
        assert!(matches!(r, Ok(Err(_))), "refused at its wait: {r:?}");
        // the ladder's R6 cancels this statement; its run's poll takes the cancel and raises it
        governor::register(4004, 44041);
        let ctx = mcx::MemoryContext::new("d4402 hog");
        governor::statement_start(0, true);
        governor::arm(true);
        ctx.mcx().alloc_uninit_bytes(core::alloc::Layout::from_size_align(256 << 20, 8).unwrap()).unwrap();
        let v = governor::cancel_largest_victim(90, 32 << 20).expect("the hog is chosen");
        assert_eq!(v.backend, Some((4004, 44041)), "{v:?}");
        assert!(governor::take_watchdog_cancel().is_some(), "the run's poll raised the cancel");
        let t0 = Instant::now();
        let r = admit_waiting(&POOL, Grant::of(128 * MIB), &need, Duration::from_secs(3));
        let ms = t0.elapsed().as_millis();
        governor::arm(false);
        let outcome = match r {
            Err(e) => Ok((e.sqlstate(), e.detail().map(str::to_string))),
            Ok(r) => Err(format!("{r:?}")), // alloc: Cold
        };
        governor::statement_end();
        drop(ctx);
        (outcome, ms, waited_ms)
    })
    .join()
    .unwrap();
    end.send(()).unwrap();
    holder.join().unwrap();
    POOL.arm(0);
    assert!(waited_ms >= 250, "a statement nobody canceled waited for the holder ({waited_ms} ms)");
    let (state, detail) = outcome.unwrap_or_else(|r| panic!("a canceled statement queued for the pool after {ms} ms: {r}"));
    assert_eq!(state, types_error::ERRCODE_OUT_OF_MEMORY);
    assert!(detail.as_deref().is_some_and(|d| d.contains("90% of its limit")), "the cancel's own text: {detail:?}");
    assert!(ms < 500, "refused at once, not after a wait ({ms} ms)");
}
