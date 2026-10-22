//! reentry.rs: a statement started while a level runs takes the next level, past the static ones too (D3360); the
//! away-from-home rule is a refusal by name; a deep level goes with its caller's run (D3881).

use gravity_exec::admit::reentry;
use gravity_exec::admit::reentry::{AWAY_REFUSAL, MAX_DEPTH};
use gravity_exec::admit::kept;
use gravity_exec::Backend;

fn build(_depth: usize) -> Box<Backend> {
    Box::new(Backend::new_for_test()) // alloc: Cold
}

fn nest(left: usize, seen: &mut Vec<usize>, n: &mut usize) -> Result<(), &'static str> {
    reentry::enter(Some(build), None, false, |b, depth| {
        seen.push(depth); // alloc: Cold
        *n += 1;
        assert_eq!(reentry::depth(), depth + 1);
        b.admissions += 1;
        if left > 0 { nest(left - 1, seen, n) } else { Ok(()) }
    })
    .map(|r| r.expect("built"))?
}

#[test]
fn a_nested_statement_takes_the_next_level() {
    let (mut seen, mut n) = (Vec::new(), 0);
    assert_eq!(nest(MAX_DEPTH - 1, &mut seen, &mut n), Ok(()));
    for (i, d) in seen.iter().enumerate() {
        assert_eq!(*d, i);
    }
    assert_eq!(reentry::depth(), 0);
    assert_eq!(reentry::deepest(), MAX_DEPTH - 1);
    let mut levels = 0;
    assert_eq!(reentry::for_each_idle(|b| { levels += 1; assert!(b.admissions >= 1); }), 0);
    assert_eq!(levels, MAX_DEPTH);
    reentry::teardown();
}

#[test]
fn past_the_static_levels_a_statement_takes_a_deep_level_and_every_level_is_released() {
    let (mut seen, mut n) = (Vec::new(), 0);
    assert_eq!(nest(MAX_DEPTH + 3, &mut seen, &mut n), Ok(()));
    assert_eq!(seen, (0..MAX_DEPTH + 4).collect::<Vec<_>>());
    assert_eq!(reentry::depth(), 0);
    assert_eq!(reentry::deepest(), MAX_DEPTH + 3);
    // D3881: the deep levels went at the end of their caller's run; the static ones stay
    let mut levels = 0;
    assert_eq!(reentry::for_each_idle(|b| { levels += 1; assert!(b.admissions >= 1); }), 0);
    assert_eq!(levels, MAX_DEPTH);
    // the same nesting again takes the same depths, the deep ones built afresh
    let (mut again, mut m) = (Vec::new(), 0);
    assert_eq!(nest(MAX_DEPTH + 3, &mut again, &mut m), Ok(()));
    assert_eq!(again, seen);
    let mut levels = 0;
    reentry::for_each_idle(|b| { levels += 1; assert!(b.admissions >= 2); });
    assert_eq!(levels, MAX_DEPTH);
    // a deep home: held while idle (the nested statement goes past it), refused while it runs and the program is hosted
    let deep = MAX_DEPTH + 1;
    let pin = at(deep, |d| {
        assert_eq!(d, deep);
        reentry::pin(deep)
    });
    assert_eq!(reentry::enter(Some(build), Some(deep), true, |_, d| (d, reentry::running(deep))), Ok(Some((0, true))));
    assert!(!reentry::running(deep));
    drop(pin);
    reentry::teardown();
    assert_eq!(reentry::enter(None, None, false, |_, d| d), Ok(None));
    let mut levels = 0;
    reentry::for_each_idle(|_| levels += 1);
    assert_eq!(levels, 0);
}

/// `f` run at level `depth` (each level below it running a statement).
fn at<R>(depth: usize, f: impl FnOnce(usize) -> R) -> R {
    reentry::enter(Some(build), None, false, |_, d| if d == depth { f(d) } else { at(depth, f) }).expect("entered").expect("built")
}

/// The levels built now: the idle ones and the running ones.
fn built_levels() -> usize {
    let mut idle = 0;
    let busy = reentry::for_each_idle(|_| idle += 1);
    idle + busy
}

/// D3881: a deep level lives while its caller's run does: one a statement still holds (its pin: a cursor's portal, a kept
/// serve) stays past that run and goes at the first top-level statement after its holder lets go; one an unwind left goes
/// there too.
#[test]
fn a_deep_level_goes_with_its_callers_run_unless_a_statement_holds_it() {
    let deep = MAX_DEPTH + 2;
    // inside the caller's run, the deep levels below the running one stay
    at(deep, |_| at(deep + 1, |_| assert_eq!(built_levels(), deep + 2)));
    assert_eq!(built_levels(), MAX_DEPTH);
    // their emptied state is shelved in the pool, charged, and the next deep levels built take it
    let shelved = kept::held();
    assert!(shelved > 0);
    at(deep, |_| assert!(kept::held() < shelved));
    assert_eq!(kept::held(), shelved);
    // a pin past the caller's run keeps its level (the idle ones below it go)
    let pin = at(deep, |_| reentry::pin(deep));
    assert_eq!(built_levels(), MAX_DEPTH + 1);
    assert_eq!(reentry::enter(Some(build), None, false, |_, d| d), Ok(Some(0)));
    assert_eq!(built_levels(), MAX_DEPTH + 1);
    // a statement nested at its depth while it is held runs there (a pin keeps the state, it does not hold the level)
    assert_eq!(at(deep - 1, |_| reentry::enter(Some(build), None, false, |_, d| d)), Ok(Some(deep)));
    drop(pin);
    assert_eq!(reentry::enter(Some(build), None, false, |_, d| d), Ok(Some(0)));
    assert_eq!(built_levels(), MAX_DEPTH);
    // an unwind through the levels leaves them to the next top-level statement
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| at(deep, |_| panic!("unwound")));
    std::panic::set_hook(hook);
    assert!(r.is_err());
    assert_eq!(reentry::depth(), 0);
    assert_eq!(built_levels(), MAX_DEPTH + 3);
    assert_eq!(reentry::enter(Some(build), None, false, |_, d| d), Ok(Some(0)));
    assert_eq!(built_levels(), MAX_DEPTH);
    reentry::teardown();
    assert_eq!(kept::held(), 0);
}

/// D3465: a statement nested N levels past the static ones finds its level in O(1): the deep levels it looks at grow
/// with N, not N^2 (a scan from the first deep level at every level made a 47,113-deep SQL recursion take 92 s), and
/// `nested` answers as `depth() > 0` does at every level. Each deep level's state is the small one (`DEEP_*`: a static
/// level's park, memo and plan slots are ~150 KB, and 47,000 levels held 11 GB).
#[test]
fn a_deep_recursion_looks_at_one_deep_level_per_statement() {
    const N: usize = 3000;
    fn level(depth: usize) -> Box<Backend> {
        Box::new(Backend::for_level(depth)) // alloc: Cold
    }
    fn deep(left: usize, seen: &mut usize) -> Result<(), &'static str> {
        reentry::enter(Some(level), None, false, |b, depth| {
            let plans = if depth < MAX_DEPTH { gravity_exec::MAX_PLANS } else { gravity_exec::DEEP_PLANS };
            assert_eq!(b.plans.len(), plans, "level {depth}");
            assert!(reentry::nested());
            assert_eq!(reentry::depth(), depth + 1);
            *seen = (*seen).max(depth);
            if left > 0 { deep(left - 1, seen) } else { Ok(()) }
        })
        .map(|r| r.expect("built"))?
    }
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(|| {
            assert!(!reentry::nested());
            for round in 0..2 {
                let (before, mut seen) = (reentry::deep_probes(), 0);
                assert_eq!(deep(MAX_DEPTH + N - 1, &mut seen), Ok(()));
                assert_eq!(seen, MAX_DEPTH + N - 1);
                let probes = reentry::deep_probes() - before;
                assert!(probes <= 2 * N as u64, "round {round}: {probes} deep levels looked at for {N} deep statements");
                assert!(!reentry::nested());
            }
            reentry::teardown();
        })
        .expect("spawn")
        .join()
        .expect("the deep recursion");
}

#[test]
fn a_running_deep_level_refuses_a_hosted_run_away_from_it() {
    let (mut seen, mut n) = (Vec::new(), 0);
    assert_eq!(nest(MAX_DEPTH, &mut seen, &mut n), Ok(()));
    let away = at(MAX_DEPTH, |d| {
        assert_eq!(d, MAX_DEPTH);
        assert!(reentry::running(MAX_DEPTH));
        (reentry::enter(Some(build), Some(MAX_DEPTH), true, |_, d| d), reentry::enter(Some(build), Some(MAX_DEPTH), false, |_, d| d))
    });
    assert_eq!(away, (Err(AWAY_REFUSAL), Ok(Some(MAX_DEPTH + 1))));
    reentry::teardown();
}

#[test]
fn an_unbuilt_level_is_not_built_without_a_builder() {
    assert_eq!(reentry::enter(None, None, false, |_, d| d), Ok(None));
    assert_eq!(reentry::enter(Some(build), None, false, |_, d| d), Ok(Some(0)));
    assert_eq!(reentry::enter(None, None, false, |_, d| d), Ok(Some(0)));
    reentry::teardown();
    assert_eq!(reentry::enter(None, None, false, |_, d| d), Ok(None));
}

#[test]
fn away_from_home_holds_an_idle_home_and_refuses_a_running_one() {
    // opened at level 1, fetched at the top level: level 1 is held, so a nested statement goes to level 2
    let inner = reentry::enter(Some(build), None, false, |_, _| reentry::enter(Some(build), None, false, |_, d| d));
    assert_eq!(inner, Ok(Some(Ok(Some(1)))));
    let nested = reentry::enter(Some(build), Some(1), true, |_, d| {
        assert_eq!(d, 0);
        reentry::enter(Some(build), None, false, |_, d| d)
    });
    assert_eq!(nested, Ok(Some(Ok(Some(2)))));
    // opened at the top level, fetched inside a running top-level statement: refused when its programs hold host state
    let away = reentry::enter(Some(build), None, false, |_, _| (reentry::enter(Some(build), Some(0), true, |_, d| d), reentry::enter(Some(build), Some(0), false, |_, d| d)));
    assert_eq!(away, Ok(Some((Err(AWAY_REFUSAL), Ok(Some(1))))));
    // D2355: the same condition, asked before a run starts its receiver
    assert!(!reentry::running(0));
    assert_eq!(reentry::enter(Some(build), None, false, |_, d| (reentry::running(d), reentry::running(d + 1))), Ok(Some((true, false))));
    assert!(!reentry::running(MAX_DEPTH));
    reentry::teardown();
}

#[test]
fn a_flush_while_a_level_runs_reaches_it_at_its_next_borrow() {
    reentry::enter(Some(build), None, false, |b, _| {
        assert_eq!(reentry::for_each_idle(|_| ()), 1);
        reentry::flush_later();
    })
    .unwrap();
    assert_eq!(reentry::enter(None, None, false, |_, d| d), Ok(Some(0)));
    reentry::teardown();
}

// MH L4 (MH-work §B.7–B.9): what a nested statement leaves behind on the thread — the outer's reserve, slot and
// placement survive it; a detoast re-entered under an outer view keeps the outer's bytes.
struct Idle;

impl gravity_parallel::Pool for Idle {
    fn run(&self, _width: u16, _claims: &dyn gravity_parallel::MorselSource, _job: &gravity_parallel::Job<'_>) -> Result<(), gravity_parallel::FirstError> {
        Ok(())
    }
}

static POOLS: [Idle; 2] = [Idle, Idle];

#[test]
fn the_outer_reserve_slot_and_placement_survive_a_nested_statement() {
    use gravity_driver::{Grant, POOL};
    let _ = gravity_parallel::install_pools(vec![&POOLS[0], &POOLS[1]]);
    let wait = core::time::Duration::from_millis(10);
    POOL.arm(256 << 20);
    POOL.gate(1);
    let held0 = POOL.held();
    let inflight0 = POOL.inflight();
    let outer = reentry::enter(Some(build), None, false, |_, depth| {
        assert_eq!(depth, 0);
        let placed = gravity_parallel::place();
        let a = POOL.admit(Grant::of(8 << 20), 1, 1, wait).unwrap();
        let (idx, reserve, slot) = (gravity_parallel::placed(), a.left(), a.holds_slot());
        assert!(slot && reserve == a.reserved);
        let inner = reentry::enter(Some(build), None, false, |_, depth| {
            assert_eq!(depth, 1);
            let placed = gravity_parallel::place();
            let b = POOL.admit(Grant::of(4 << 20), 1, 1, wait).unwrap();
            assert!(!b.holds_slot(), "the inner admission takes no second slot on the thread that holds one");
            assert_eq!(b.left(), b.reserved, "the inner draws from its own reservation");
            assert_eq!(POOL.inflight(), inflight0 + 1, "the outer's slot is shared, not a second one");
            let inner_idx = gravity_parallel::placed();
            let b_reserved = b.reserved;
            drop(b);
            drop(placed);
            (b_reserved, inner_idx)
        })
        .expect("entered")
        .expect("built");
        assert_eq!((gravity_parallel::placed(), a.left(), a.holds_slot()), (idx, reserve, slot), "the outer's placement, reservation and slot are untouched");
        assert_eq!(POOL.held(), held0 + a.reserved, "only the inner's bytes were released");
        assert_ne!(inner.1, idx, "the nested statement was placed on the other pool");
        let a_reserved = a.reserved;
        drop(a);
        drop(placed);
        (a_reserved, inner.0)
    })
    .expect("entered")
    .expect("built");
    assert!(outer.0 > 0 && outer.1 > 0);
    assert_eq!((POOL.inflight(), POOL.held()), (inflight0, held0));
    assert_eq!(gravity_parallel::placed(), 0);
    POOL.gate(0);
    POOL.arm(0);
    reentry::teardown();
}

#[test]
fn a_nested_detoast_under_an_outer_view_keeps_the_outer_bytes() {
    use gravity_expr::views::with_detoast_scratch;
    fn bytes(cx: mcx::Mcx<'_>, byte: u8, n: usize) -> &'static [u8] {
        let mut v: mcx::PgVec<u8> = mcx::vec_with_capacity_in(cx, n).unwrap();
        mcx::vec_append_bytes(&mut v, &vec![byte; n]).unwrap(); // alloc: Cold
        // SAFETY: the test reads the slice only while its context is live (before that call's reset)
        unsafe { core::slice::from_raw_parts(v.leak().as_ptr(), n) }
    }
    let seen = with_detoast_scratch(|cx| {
        let outer = bytes(cx, 0x3C, 2048);
        let n = reentry::enter(Some(build), None, false, |_, _| with_detoast_scratch(|cx2| bytes(cx2, 0xC3, 4096).len())).expect("entered").expect("built");
        assert_eq!(n, 4096);
        outer.iter().filter(|b| **b == 0x3C).count()
    });
    assert_eq!(seen, 2048, "the outer view's bytes are intact after the nested detoast reset its own context");
    reentry::teardown();
}

/// D4294: a nested statement's stashed errors (a host's `stash`, a hosted site's `gravity_fn::stash_error`) end with its
/// run. One that unwound (an ERROR a PL exception block catches) leaves none for its caller's mapping of a HOST raise,
/// and a nested statement never maps its caller's pending stash, which is back once the nested statement ends.
#[test]
fn a_nested_statements_stash_ends_with_its_run_an_unwind_too() {
    use types_error::PgError;
    let msg = |e: Option<Box<PgError>>| e.map(|e| e.message().to_string());
    let left = reentry::enter(Some(build), None, false, |_, _| {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reentry::enter(Some(build), None, false, |_, _| {
                let _ = gravity_hosts::stash(Box::new(PgError::error("the nested statement's host error"))); // alloc: Cold
                gravity_fn::stash_error(Box::new(PgError::error("the nested statement's shim error"))); // alloc: Cold
                std::panic::panic_any(Box::new(PgError::error("the nested statement's ERROR"))) // alloc: Cold
            })
        }));
        assert!(r.is_err(), "the nested statement unwound");
        (msg(gravity_hosts::take_error()), msg(gravity_fn::take_error()))
    })
    .expect("a level")
    .expect("built");
    assert_eq!(left, (None, None), "an unwound nested statement's stashes are not its caller's to map");
    let (inner, outer) = reentry::enter(Some(build), None, false, |_, _| {
        let _ = gravity_hosts::stash(Box::new(PgError::error("the caller's host error"))); // alloc: Cold
        let inner = reentry::enter(Some(build), None, false, |_, _| msg(gravity_hosts::take_any_error())).expect("a level").expect("built");
        (inner, msg(gravity_hosts::take_error()))
    })
    .expect("a level")
    .expect("built");
    assert_eq!(inner, None, "a nested statement does not map its caller's stash");
    assert_eq!(outer.as_deref(), Some("the caller's host error"), "the caller's stash is back after the nested statement");
    reentry::teardown();
}
