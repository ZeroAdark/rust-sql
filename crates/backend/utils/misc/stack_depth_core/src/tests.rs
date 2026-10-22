use super::*;

// The test's own read of the machine stack pointer, independent of the
// guard's probe: ASan's fake stack (detect_stack_use_after_return) relocates
// address-taken locals to heap pools but never the stack pointer itself.
#[inline(never)]
fn machine_sp_here() -> usize {
    let sp: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: register-only read of sp; touches no memory, keeps flags.
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: register-only read of rsp; touches no memory, keeps flags.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}

// upstream c0bf1d89df29 (18.6): both halves of the depth probe must measure
// the machine stack, or under ASan's fake stack |base - probe| is garbage
// (100% of guard evaluations raised 54001 on the CI cluster). Run with
// ASAN_OPTIONS=detect_stack_use_after_return=1 to see the pre-fix failure.
#[test]
fn base_probe_reads_the_machine_stack() {
    let sp = machine_sp_here();
    let probe = current_stack_addr();
    assert!(
        sp.abs_diff(probe) < 16 * 1024,
        "base probe {probe:#x} is not on the machine stack (sp {sp:#x})"
    );
}

#[test]
fn guard_probe_reads_the_machine_stack() {
    let budget: isize = 1 << 20;
    let margin: usize = 64 * 1024;
    let sp = machine_sp_here();
    set_enforced_stack_budget_for_tests(budget);
    let within = budget as usize - margin;
    restore_stack_base(sp + within);
    assert!(
        !stack_is_too_deep(),
        "probe left the machine stack (base above sp {sp:#x})"
    );
    restore_stack_base(sp - within);
    assert!(
        !stack_is_too_deep(),
        "probe left the machine stack (base below sp {sp:#x})"
    );
    restore_stack_base(sp + budget as usize + margin);
    assert!(stack_is_too_deep());
    restore_stack_base(0);
}

// ---------------------------------------------------------------------------
// E-DEBUGBUILD unit gate (bug catalog PR1613-1, fix PR #1613): the scale is a
// function of the build's opt-level ONLY. Opt-level 0 is the only profile
// with the pathological frames the 32x budget covers; every optimized build
// — including opt-level >= 1 with debug-assertions on, the debug-server
// profile shape — must enforce the optimized budget. Pre-fix,
// `cfg!(debug_assertions)` put that shape on the 32x arm and the json/jsonb
// regress deep-nesting inputs at max_stack_depth = 100kB stopped tripping
// 54001 there (22P02 "input string ended unexpectedly" instead).
//
// The two stock invocations cannot tell the keyings apart (dev: opt 0 +
// assertions, release: opt 3 + no assertions — both agree). The
// DISCRIMINATING build is the one where they disagree:
//
//   cargo test -p stack_depth_core --config profile.dev.opt-level=1
//
// (opt-level 1, debug-assertions still on). There a debug_assertions-keyed
// constant reads 32 and `scale_is_keyed_on_opt_level_not_debug_assertions`
// fails; the opt-level-keyed constant reads the optimized value and it passes. The two
// shape-specific tests below compile ONLY on such disagreeing builds and
// state the required value outright.
// ---------------------------------------------------------------------------

/// What `STACK_DEPTH_SCALE` must be for the opt-level build.rs reported.
fn expected_scale_for_opt_level() -> isize {
    if cfg!(pgrust_opt_level = "0") { 32 } else { 6 }
}

#[test]
fn build_script_cfg_and_env_agree_on_the_opt_level() {
    // build.rs emits both from the same OPT_LEVEL read; a mismatch means the
    // cfg the constant is keyed on is not the level cargo actually built at.
    assert!(
        matches!(BUILD_OPT_LEVEL, "0" | "1" | "2" | "3" | "s" | "z"),
        "build.rs saw an OPT_LEVEL cargo does not document: {BUILD_OPT_LEVEL:?}"
    );
    assert_eq!(
        cfg!(pgrust_opt_level = "0"),
        BUILD_OPT_LEVEL == "0",
        "cfg(pgrust_opt_level) disagrees with OPT_LEVEL={BUILD_OPT_LEVEL}"
    );
}

#[test]
fn scale_is_keyed_on_opt_level_not_debug_assertions() {
    let expected = expected_scale_for_opt_level();
    assert_eq!(
        STACK_DEPTH_SCALE, expected,
        "opt-level {BUILD_OPT_LEVEL} (debug_assertions={}) must enforce {expected}x — \
         the scale is keyed on something other than the build's opt-level",
        cfg!(debug_assertions)
    );
    // The enforced budget follows the same scale (no thread ceiling in a
    // test thread, so no clamp).
    assign_max_stack_depth(100);
    assert_eq!(max_stack_depth_bytes(), 100 * 1024 * expected);
    assert_eq!(scaled_max_stack_depth_bytes(), 100 * 1024 * expected);
}

/// The PR1613-1 build shape itself: optimized, assertions on. Compiles only
/// there (never under stock dev/release), and demands the optimized budget.
#[cfg(all(debug_assertions, not(pgrust_opt_level = "0")))]
#[test]
fn optimized_with_assertions_build_enforces_the_optimized_budget() {
    assert_eq!(
        STACK_DEPTH_SCALE, 6,
        "opt-level {BUILD_OPT_LEVEL} with debug-assertions on took the 32x opt-level-0 arm: \
         deep json/jsonb at max_stack_depth=100kB would parse to end of input (22P02) \
         where C raises 54001 (PR1613-1)"
    );
}

/// The mirror shape: opt-level 0 with assertions OFF must still get the
/// opt-level-0 budget — its frames are the pathological ones regardless of
/// assertions.
#[cfg(all(not(debug_assertions), pgrust_opt_level = "0"))]
#[test]
fn unoptimized_without_assertions_build_enforces_the_unoptimized_budget() {
    assert_eq!(
        STACK_DEPTH_SCALE, 32,
        "opt-level 0 with debug-assertions off took the optimized 6x arm: stock \
         max_stack_depth would trip 54001 on ordinary queries C answers (#1485)"
    );
}

// D3103: the weighted check is the plain one off wasm32 (weight 1), and the engine-to-shadow conversion is the
// calibrated 3/5 (0.6 engine bytes per shadow byte): V8's 984 KB stands for 1640 KB of shadow.
#[test]
fn weighted_check_is_plain_off_wasm_and_engine_ratio_is_calibrated() {
    assert_eq!(WASM_NATIVE_HEAVY_WEIGHT, 1);
    assert_eq!(wasm_shadow_equivalent(DEFAULT_WASM_ENGINE_STACK), 984 * 1024 / 3 * 5);
    let old = set_stack_base();
    set_enforced_stack_budget_for_tests(1);
    assert!(check_stack_depth_weighted().is_err() == check_stack_depth().is_err());
    set_enforced_stack_budget_for_tests(isize::MAX);
    assert!(check_stack_depth_weighted().is_ok() && check_stack_depth().is_ok());
    set_enforced_stack_budget_for_tests(100 * 1024 * STACK_DEPTH_SCALE);
    restore_stack_base(old);
}

// D3833: a lowering nest level charges its own amount on wasm32 only, beside a statement level's, and it comes off with it.
#[test]
fn nest_level_charge_is_wasm_only_and_scoped() {
    assert_eq!(WASM_NEST_LEVEL_CHARGE == 0, !cfg!(target_family = "wasm"));
    let before = guarded_depth(0);
    {
        let _a = EngineLevel::charging(WASM_NEST_LEVEL_CHARGE);
        let _b = EngineLevel::enter();
        assert_eq!(guarded_depth(0), before + WASM_NEST_LEVEL_CHARGE + WASM_ENGINE_LEVEL_CHARGE);
    }
    assert_eq!(guarded_depth(0), before);
}

// D3763: a nested statement level charges the guard for its engine frames on wasm32 only (8 KB a level; 0 elsewhere,
// where a held level changes no reading), and the charge comes off when the level ends, by an error's unwind too.
#[test]
fn engine_level_charge_is_wasm_only_and_scoped() {
    assert_eq!(WASM_ENGINE_LEVEL_CHARGE, if cfg!(target_family = "wasm") { 8 * 1024 } else { 0 });
    let old = set_stack_base();
    set_enforced_stack_budget_for_tests(1 << 20);
    let before = guarded_depth(0);
    {
        let _a = EngineLevel::enter();
        let _b = EngineLevel::enter();
        assert_eq!(guarded_depth(0), before + 2 * WASM_ENGINE_LEVEL_CHARGE);
        assert!(!stack_is_too_deep());
    }
    assert_eq!(guarded_depth(0), before);
    let unwound = std::panic::catch_unwind(|| {
        let _c = EngineLevel::enter();
        panic!("a level's error unwinds through it");
    });
    assert!(unwound.is_err());
    assert_eq!(guarded_depth(0), before);
    set_enforced_stack_budget_for_tests(100 * 1024 * STACK_DEPTH_SCALE);
    restore_stack_base(old);
}

// D3812: the shadow-heavy check (decompose's walk) is check_stack_depth_or_panic on every target but wasm32: nothing below
// the budget, the same 54001 payload past it.
#[test]
fn shadow_heavy_check_is_the_plain_check_off_wasm() {
    assert_eq!(WASM_SHADOW_HEAVY_PART.0, WASM_SHADOW_HEAVY_PART.1);
    let budget: isize = 1 << 20;
    let margin: usize = 64 * 1024;
    let sp = machine_sp_here();
    set_enforced_stack_budget_for_tests(budget);
    restore_stack_base(sp + budget as usize - margin);
    check_stack_depth_shadow_heavy_or_panic();
    restore_stack_base(sp + budget as usize + margin);
    let payload = std::panic::catch_unwind(check_stack_depth_shadow_heavy_or_panic).expect_err("past the budget it raises");
    let e = payload.downcast::<Box<types_error::PgError>>().expect("the payload is the guard's PgError");
    assert_eq!(e.sqlstate(), ERRCODE_STATEMENT_TOO_COMPLEX);
    set_enforced_stack_budget_for_tests(100 * 1024 * STACK_DEPTH_SCALE);
    restore_stack_base(0);
}
