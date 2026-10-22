//! p1-deadguard REGRESSION (release blocker): the regex compiler's recursion
//! guards were fixed FRAME-COUNT caps (`parse_depth >= MAX_PARSE_DEPTH` = 10_000,
//! `depth >= MAX_RECURSION_DEPTH` = 10_000, `v.depth >= v.max_depth`) where C
//! uses byte-based `stack_is_too_deep()` via `STACK_TOO_DEEP()` /
//! `rstacktoodeep()`. C's own comment on subre() says "Checking for stack
//! overflow here is sufficient to protect parse() and its recursive
//! subroutines", so that one site guards the whole parser.
//!
//! Bytes/frame arithmetic (measured, local --release, aarch64 macOS, by probing
//! the frame address inside the parse recursion): 266 bytes per parse frame.
//! 10_000 frames = 2.66 MB. child_thread_stack_size() has a hard 2 MiB floor,
//! and 2.66 MB does not fit in 2 MiB at all -- so on the smallest stack a real
//! backend thread can get, the cap was UNREACHABLE, the guard was DEAD CODE,
//! and the stack overflowed. pgrust is thread-per-backend, so the resulting
//! abort kills every session.
//!
//! Measured pre-fix, pattern `repeat('(',N) || 'a' || repeat(')',N)`:
//!   2 MiB stack: N=2000 -> REG_ETOOBIG   N=3000 PROCESS ABORT (died at parse
//!                depth ~7800 == ~2.07 MB, i.e. at the stack wall, well short
//!                of the 10_000-frame cap)
//!   8 MiB stack: N=3000..10000 -> REG_ETOOBIG (the cap is reachable there)
//! The guard being alive on one stack size and dead on another is exactly why
//! the bound has to be measured in bytes.
//!
//! Runs each probe in a subprocess because a stack overflow aborts the process.

use ::mcx::MemoryContext;
use regex_core::regex_compile::pg_regcomp;
use regex_core::regex_consts::REG_ADVANCED;

#[test]
fn deep_regex_nesting_reports_etoobig_and_does_not_abort() {
    const REG_ETOOBIG: i32 = 19;
    // Nesting depths that ABORTED the process before the fix on the 2 MiB floor.
    const DEPTHS: [usize; 4] = [3000, 5000, 10_000, 40_000];
    if let Ok(d) = std::env::var("RE_STACK_PROBE_DEPTH") {
        let depth: usize = d.parse().unwrap();
        let pat = format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
        let w: Vec<u32> = pat.chars().map(|c| c as u32).collect();
        let h = std::thread::Builder::new()
            // The child_thread_stack_size() 2 MiB floor is the worst case a
            // real backend thread can get. max_stack_depth is set to what
            // guc.c's rlimit branch would derive for it -- (2 MiB - 512 KiB
            // STACK_DEPTH_SLOP) / 1024 -- so the pairing keeps production's
            // HEADROOM instead of asserting the guard fires with none.
            .stack_size(2 << 20)
            .spawn(move || {
                // Without a stack base, stack_is_too_deep() short-circuits on
                // base == 0 and every guard below is INERT -- the test would be
                // vacuous.
                ::stack_depth::set_stack_base();
                // Production pairing: backend threads record their real
                // reservation; the ceiling keeps the scaled budget inside it.
                ::stack_depth::set_thread_stack_ceiling(2 << 20);
                ::stack_depth::assign_max_stack_depth(1536);
                let ctx = MemoryContext::new("t");
                pg_regcomp(ctx.mcx(), &w, REG_ADVANCED, 0).map(|_| ())
            })
            .unwrap();
        match h.join().expect("compile thread must not panic") {
            Ok(()) => eprintln!("PROBE OK"),
            Err(e) => eprintln!("PROBE ERR {}", e.0),
        }
        return;
    }
    let exe = std::env::current_exe().unwrap();
    for depth in DEPTHS {
        let out = std::process::Command::new(&exe)
            .args(["--exact", "--nocapture", "deep_regex_nesting_reports_etoobig_and_does_not_abort"])
            .env("RE_STACK_PROBE_DEPTH", depth.to_string())
            .output()
            .unwrap();
        let se = String::from_utf8_lossy(&out.stderr);
        let line = se.lines().find(|l| l.starts_with("PROBE")).unwrap_or_else(|| {
            panic!("depth {depth}: process died without a verdict (stack overflow): {se}")
        });
        // The trip depth is a function of frame size and is NOT a comparison
        // surface against C. That the process SURVIVES and reports REG_ETOOBIG
        // -- which regexp_* surfaces as "regular expression is too complex" --
        // rather than aborting is.
        assert_eq!(line, format!("PROBE ERR {REG_ETOOBIG}"), "depth {depth}: got {line:?}");
    }
}

/// regexport.c traverse_lacons() check_stack_depth(): the guard's failure is
/// the structured 54001 ereport, restored losslessly at the statement
/// boundary from a `Box<PgError>` panic payload (pg_trgm's NFA walk reaches
/// it through pg_reg_getnumoutarcs/pg_reg_getoutarcs, which return no error).
#[test]
fn traverse_lacons_stack_guard_raises_54001() {
    use regex_core::regex_export_free_error::{pg_reg_getnumoutarcs, pg_reg_getnumstates};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use types_error::{PgError, ERRCODE_STATEMENT_TOO_COMPLEX};

    let h = std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(|| {
            ::stack_depth::set_stack_base();
            ::stack_depth::set_thread_stack_ceiling(32 << 20);
            ::stack_depth::set_enforced_stack_budget_for_tests(8 << 20);
            // A chain of 2000 two-chr lookaheads: one LACON arc per state, so
            // the outarc walk from the chain's head recurses 2000 deep.
            let pat: Vec<u32> = format!("{}ab", "(?=ab)".repeat(2000))
                .chars()
                .map(|c| c as u32)
                .collect();
            let ctx = MemoryContext::new("t");
            let re = pg_regcomp(ctx.mcx(), &pat, REG_ADVANCED, 0).expect("compiles");
            let nstates = pg_reg_getnumstates(&re);
            for st in 0..nstates {
                pg_reg_getnumoutarcs(&re, st);
            }
            ::stack_depth::set_enforced_stack_budget_for_tests(64 << 10);
            let mut fired = 0;
            for st in 0..nstates {
                let Err(payload) = catch_unwind(AssertUnwindSafe(|| pg_reg_getnumoutarcs(&re, st)))
                else {
                    continue;
                };
                let e = payload
                    .downcast::<Box<PgError>>()
                    .expect("stack guard must panic with the Box<PgError> payload");
                assert_eq!(e.sqlstate(), ERRCODE_STATEMENT_TOO_COMPLEX);
                assert_eq!(e.message(), "stack depth limit exceeded");
                assert!(e.hint().is_some_and(|h| h.contains("max_stack_depth")), "{:?}", e.hint());
                fired += 1;
            }
            assert!(fired > 0, "the 64 kB budget must trip the walk");
        })
        .unwrap();
    h.join().expect("walk thread must not die");
}

// The re_fns hook must report the real stack check, as C's does.
#[test]
fn rstacktoodeep_reports_stack_is_too_deep() {
    std::thread::spawn(|| {
        // No stack base recorded: stack_is_too_deep() is false.
        assert_eq!(regex_core::regex_compile::rstacktoodeep(), 0);
        // A base far from the current sp puts the depth over any budget.
        ::stack_depth::restore_stack_base(1);
        assert!(::stack_depth::stack_is_too_deep());
        assert_eq!(regex_core::regex_compile::rstacktoodeep(), 1);
    })
    .join()
    .unwrap();
}
