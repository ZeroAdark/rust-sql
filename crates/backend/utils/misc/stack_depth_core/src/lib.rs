//! stack_depth_core: the recursion guard itself (C
//! src/backend/utils/misc/stack_depth.c's stack_is_too_deep /
//! check_stack_depth / set_stack_base / the max_stack_depth state).
//!
//! SPLIT OUT OF `stack_depth` (lane p1-nodes, behavior-identical): the guard
//! is needed by low-level walkers (nodes/readfuncs, nodes/outfuncs,
//! nodes/copyfuncs — C calls check_stack_depth in all three), but
//! `stack_depth`'s GUC hook/seam half depends on `guc`, which depends
//! transitively on those very crates. The guard core has no GUC dependency;
//! the GUC check-hook/assign-hook/seam half stays in `stack_depth`, which
//! re-exports everything here so existing callers are unchanged.

#![allow(non_camel_case_types)]

#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64"), not(miri)))]
mod tests;

use std::cell::Cell;

use elog::ereport;
use types_error::{PgResult, ERRCODE_STATEMENT_TOO_COMPLEX, ERROR};

// A stack address, only ever subtracted, never dereferenced; 0 is C's NULL.
pub type pg_stack_base_t = usize;

pub const STACK_DEPTH_SLOP: isize = 512 * 1024;

/// Rust frames on the guarded recursion chains cost a multiple of their C
/// counterparts, so the same `max_stack_depth` GUC value buys far less
/// recursion than it does in C: at 2048kB, C 18.3 reaches 932 plpgsql
/// self-recursion levels vs 34 here (~27x, dev build, macOS aarch64), and
/// two driver suites (DBD::Pg `foreign_key_info`'s 9-join catalog query,
/// pgjdbc `testOidUpdatable`) hit "stack depth limit exceeded" at stock
/// config on queries C handles. The GUC keeps its C-parity value/limits
/// (SHOW, accepted range, rlimit cap are byte-identical to C); the byte
/// budget the guard *enforces* is scaled by this per-profile constant so
/// the same GUC setting buys comparable recursion DEPTH.
///
/// unoptimized (opt-level 0) = 32: covers the measured 27x worst chain
/// with slop.
/// optimized (opt-level >= 1, s, z) = 6: measured 2026-09-22 (opt-level 3,
/// macOS arm64, vs C 18.6 clang -O2) as the smallest integer at which the
/// statement chains the per-level frame-cap tests guard (analyzer, planner,
/// copyObject, expression init, jsonpath exec) reach C's depth at 100kB and
/// 2MB. C/pgrust bytes per level: nested sub-select 5.3x (the worst), jsonpath
/// exec 4.6x, FROM sub-select 4.4x, expression init 4.4x, cast chain 4.3x,
/// json/jsonb parse and jsonpath accessors 0.8-1.1x. plpgsql self-recursion
/// (6.6-6.8x) stays about 10% short of C at 6; 7 would cover it but let
/// json/jsonpath inputs C refuses at 2MB through. Re-measure with
/// `scripts/stack-calibrate.sh` when a chain's frames change.
///
/// Keyed on the build's opt-level (build.rs turns cargo's `OPT_LEVEL` into
/// the `pgrust_opt_level` cfg), NOT on `debug_assertions` — as PR #1613
/// re-keyed it (bug catalog PR1613-1). The pathological frames the 32x
/// budget covers are an opt-level-0 artifact (unmerged match-arm slots, no
/// mem2reg); an opt-level-1 build with assertions on — the debug-server
/// profile shape — has near-optimized frames, and under the 32x budget the
/// regress inputs `repeat('[', 10000)::json` and the `{"a":` / jsonb twins
/// at max_stack_depth = 100kB (3.2 MB enforced vs 0.64–1.4 MB used) parsed
/// to end of input (22P02) where C raises 54001. tests.rs pins the keying.
pub const STACK_DEPTH_SCALE: isize = if cfg!(pgrust_opt_level = "0") { 32 } else { 6 };

/// Cargo's `OPT_LEVEL` for this build ("0", "1", "2", "3", "s", "z") as
/// build.rs saw it — the input `STACK_DEPTH_SCALE` is keyed on. Test-facing.
pub const BUILD_OPT_LEVEL: &str = env!("PGRUST_OPT_LEVEL");

thread_local! {
    static MAX_STACK_DEPTH: Cell<i32> = const { Cell::new(100) };
    static MAX_STACK_DEPTH_BYTES: Cell<isize> =
        const { Cell::new(100 * 1024 * STACK_DEPTH_SCALE) };
    // The scaled budget before the thread-stack ceiling clamp; kept so the
    // clamp can be (re)applied whichever of assign/ceiling happens first.
    static SCALED_STACK_BUDGET: Cell<isize> =
        const { Cell::new(100 * 1024 * STACK_DEPTH_SCALE) };
    // Enforceable ceiling derived from this thread's real stack reservation
    // (0 = unknown: no clamp). The guard must fire before the actual stack
    // ends, or recursion dies by SIGSEGV instead of SQLSTATE 54001.
    static THREAD_STACK_CEILING: Cell<isize> = const { Cell::new(0) };
    static STACK_BASE_PTR: Cell<usize> = const { Cell::new(0) };
    // 0 is C's "not yet computed" sentinel (a real rlimit is never 0).
    static STACK_DEPTH_RLIMIT_CACHE: Cell<isize> = const { Cell::new(0) };
    // D3360: how many 54001 errors this thread has built (`stack_depth_exceeded`), read by `exceeded_count`.
    static EXCEEDED: Cell<u64> = const { Cell::new(0) };
    // D3763: the shadow bytes the guard adds to its depth for the nested statement levels running on this thread (`EngineLevel`).
    #[cfg(target_family = "wasm")]
    static ENGINE_CHARGE: Cell<isize> = const { Cell::new(0) };
    // D3812: the two halves of wasm32's THREAD_STACK_CEILING (the smaller of them), 0 until `set_wasm_stack_ceiling`.
    #[cfg(target_family = "wasm")]
    static WASM_SHADOW_CEILING: Cell<isize> = const { Cell::new(0) };
    #[cfg(target_family = "wasm")]
    static WASM_ENGINE_CEILING: Cell<isize> = const { Cell::new(0) };
}

/// D3763: the shadow bytes one nested statement level charges wasm32's guard (`EngineLevel`); 0 elsewhere.
///
/// MEASURED (postgres.wasm in node v22.20.0 linux-arm64, `node --stack-size=984`, the endless `SELECT f(n + 1)` SQL
/// function with max_stack_depth high so the engine dies first): the engine stack ran out at 277 levels (jit) / 266
/// (interp) with 285 / 405 KB of shadow stack used. A level's ~18 frames (the served statement's runner, driver and
/// program, the fmgr call back into the SQL function) keep nearly all of their state on the engine's own stack: 3.5
/// engine bytes per level for each shadow byte, against the 0.6 the ceiling assumes (`WASM_ENGINE_PER_SHADOW`), so the
/// guard read a fifth of its limit when the engine died. A level costs ~3.6 KB of engine stack, which the ceiling's
/// ratio prices at ~6 KB of shadow; the level's own ~1 KB of shadow is already read, and the charge rounds the rest up
/// with headroom to 8 KB (the guard now trips at ~180 such levels, ~65% of the engine stack).
pub const WASM_ENGINE_LEVEL_CHARGE: isize = if cfg!(target_family = "wasm") { 8 * 1024 } else { 0 };

/// D3763: one nested statement level running on this thread (a SQL function's or a PL statement's served query), held
/// for the level's run: on wasm32 it adds `WASM_ENGINE_LEVEL_CHARGE` to the depth the guard reads, for the frames that
/// live on the engine's stack only (the guard reads the shadow stack, D3103), and takes it off when dropped (an error's
/// unwind too). Nothing elsewhere: the guard reads the one machine stack every frame is on.
pub struct EngineLevel(isize);

impl EngineLevel {
    #[inline]
    pub fn enter() -> EngineLevel {
        EngineLevel::charging(WASM_ENGINE_LEVEL_CHARGE)
    }

    /// D3833: a level of another recursion whose frames are engine-heavy, charging `bytes` (0 off wasm32).
    #[inline]
    pub fn charging(bytes: isize) -> EngineLevel {
        #[cfg(target_family = "wasm")]
        ENGINE_CHARGE.set(ENGINE_CHARGE.get() + bytes);
        EngineLevel(bytes)
    }
}

impl Drop for EngineLevel {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_family = "wasm")]
        ENGINE_CHARGE.set(ENGINE_CHARGE.get() - self.0);
        #[cfg(not(target_family = "wasm"))]
        let _ = self.0;
    }
}

/// D3833: the shadow bytes one nest level of Gravity's lowering (lowerv4 `Inner::walk`, held across the level's
/// recursion) charges wasm32's guard (`EngineLevel::charging`); 0 elsewhere.
///
/// MEASURED (postgres.wasm simd in node v22.20.0 linux-arm64, a LATERAL chain `t<i> = LATERAL (SELECT t<i-1>.a + i
/// OFFSET 0)`, jit and interp alike): with lowerv4's walk frames shrunk, the engine's 984 KB ran out between 280 and 300
/// levels while the uncharged guard tripped only between 350 and 400 (a 4000 KB engine). A level costs ~1.3x more
/// engine stack than its shadow bytes say; 1.5 KB a level brings the guard to ~180-220 levels across the base the
/// statement runs on, ~70% of the engine's depth.
pub const WASM_NEST_LEVEL_CHARGE: isize = if cfg!(target_family = "wasm") { 1536 } else { 0 };

/// The depth the guard reads: the stack's own, plus on wasm32 the nested levels' engine charge (`EngineLevel`).
#[inline]
fn guarded_depth(stack_depth: isize) -> isize {
    #[cfg(target_family = "wasm")]
    return stack_depth.saturating_add(ENGINE_CHARGE.get());
    #[cfg(not(target_family = "wasm"))]
    stack_depth
}

/// D3360: how many times this thread has met the stack depth limit (each 54001 error built). A caller that maps the errors
/// under it to something else (Gravity's admission turns them into a refusal) compares two readings to learn that the
/// limit was met beneath it, and raises `stack_depth_error` instead.
pub fn exceeded_count() -> u64 {
    EXCEEDED.get()
}

/// D3360: `check_stack_depth`'s 54001 error, for a caller that learned the limit was met beneath it (`exceeded_count`).
pub fn stack_depth_error() -> Box<types_error::PgError> {
    stack_depth_exceeded()
}

pub fn max_stack_depth() -> i32 {
    MAX_STACK_DEPTH.get()
}

pub fn set_max_stack_depth(value: i32) {
    MAX_STACK_DEPTH.set(value);
}

pub fn max_stack_depth_bytes() -> isize {
    MAX_STACK_DEPTH_BYTES.get()
}

// upstream c0bf1d89df29 (18.6): Make stack depth check work with asan's use-after-return
// C's __builtin_frame_address(0): the machine stack pointer, which ASan's
// fake stack (detect_stack_use_after_return) never relocates the way it
// relocates an address-taken local; read in whichever frame this inlines
// into. C's no-__builtin_frame_address arm (a local's address) stays for
// targets without a readable sp.
#[inline(always)]
fn machine_stack_addr() -> usize {
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    {
        let sp: usize;
        // SAFETY: register-only read of sp; touches no memory, keeps flags.
        unsafe {
            core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        sp
    }
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        let sp: usize;
        // SAFETY: register-only read of rsp; touches no memory, keeps flags.
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        sp
    }
    #[cfg(not(all(any(target_arch = "aarch64", target_arch = "x86_64"), not(miri))))]
    {
        let stack_loc: u8 = 0;
        &raw const stack_loc as usize
    }
}

// inline(never) keeps the frame real; no black_box (it spills — docs/benchmarks/stack_depth.md).
#[inline(never)]
fn current_stack_addr() -> usize {
    machine_stack_addr()
}

// One backend = one thread: recorded at backend-thread spawn (C: in main()).
pub fn set_stack_base() -> pg_stack_base_t {
    let addr = current_stack_addr();
    STACK_BASE_PTR.with(|c| c.replace(addr))
}

pub fn restore_stack_base(base: pg_stack_base_t) {
    STACK_BASE_PTR.set(base);
}

#[inline(never)]
pub fn stack_is_too_deep() -> bool {
    let stack_base_ptr = STACK_BASE_PTR.get();
    let stack_depth = guarded_depth(stack_base_ptr.abs_diff(machine_stack_addr()) as isize);
    // base != 0 (NULL) guard last: no wasted cycles in the normal case.
    stack_depth > MAX_STACK_DEPTH_BYTES.get() && stack_base_ptr != 0
}

#[inline]
pub fn check_stack_depth() -> PgResult<()> {
    if stack_is_too_deep() {
        return Err(stack_depth_exceeded());
    }
    Ok(())
}

/// Runs `f` in its own, never-inlined stack frame.
///
/// Recursive giant-`match` dispatchers (ExecInitNode's plan-node match,
/// ExecInitExprRec's expression match) are the C recursion sites this
/// crate's guard bounds. In Rust, every arm's by-value temporaries get a
/// distinct stack slot in the DISPATCHER's frame — at opt-level 0 LLVM
/// does not merge the mutually-exclusive arm slots, so the frame is the
/// SUM over all ~40 arms (measured 380kB for exec_init_node, 84kB for
/// init_expr_rec on arm64 dev builds). Five plan levels then trip the
/// C-parity max_stack_depth=2048kB guard on plans C inits in a few kB
/// (the LD7-F1 / TGT-F5/F7/F8 family). Wrapping each arm body in this
/// trampoline moves the arm's temporaries into a per-arm callee frame,
/// so the recursion's per-level cost is one arm, not the sum of all —
/// in every build profile (`inline(never)` is release-effective).
#[inline(never)]
pub fn with_own_frame<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Guard for recursion sites whose signatures cannot carry `PgResult`
/// (e.g. `equal()`): raises the C-identical 54001 as a `Box<PgError>` panic
/// payload, which `pg_error_from_panic` restores losslessly at the
/// statement boundary.
#[inline]
pub fn check_stack_depth_or_panic() {
    if stack_is_too_deep() {
        std::panic::panic_any(stack_depth_exceeded());
    }
}

#[cold]
#[inline(never)]
fn stack_depth_exceeded() -> Box<types_error::PgError> {
    EXCEEDED.set(EXCEEDED.get().wrapping_add(1));
    Box::new(
        ereport(ERROR)
            .errcode(ERRCODE_STATEMENT_TOO_COMPLEX)
            .errmsg("stack depth limit exceeded")
            .errhint(format!(
                "Increase the configuration parameter \"max_stack_depth\" (currently {}kB), \
                 after ensuring the platform's stack depth limit is adequate.",
                max_stack_depth()
            ))
            .into_error(),
    )
}

// C InitializeGUCOptionsFromEnvironment's stack-rlimit branch (guc.c): the
// boot default is 100kB; a usable platform limit raises it to
// min((rlimit - slop)/1024, 2048) kB, as PGC_S_ENV_VAR so conf/argv override.
//
// The enforced budget is the GUC's bytes x STACK_DEPTH_SCALE (see the
// constant's comment), clamped to the thread's real stack when known.
pub fn assign_max_stack_depth(newval: i32) {
    let scaled = (newval as isize)
        .saturating_mul(1024)
        .saturating_mul(STACK_DEPTH_SCALE);
    SCALED_STACK_BUDGET.set(scaled);
    MAX_STACK_DEPTH_BYTES.set(effective_budget(scaled));
}

fn effective_budget(scaled: isize) -> isize {
    let ceiling = THREAD_STACK_CEILING.get();
    if ceiling > 0 { scaled.min(ceiling) } else { scaled }
}

/// The scaled budget BEFORE the per-thread ceiling clamp — what thread
/// provisioning must accommodate (launch_backend sizes backend stacks from
/// this, plus slop).
pub fn scaled_max_stack_depth_bytes() -> isize {
    SCALED_STACK_BUDGET.get()
}

/// Test/calibration hook: set the ENFORCED byte budget directly, bypassing
/// STACK_DEPTH_SCALE and the thread-stack ceiling. Guard-witness tests pin
/// the 54001 mechanism at exact byte budgets (readfuncs/tsquery deep-nesting
/// witnesses); production code never calls this — the GUC assign hook is the
/// only production writer.
pub fn set_enforced_stack_budget_for_tests(bytes: isize) {
    SCALED_STACK_BUDGET.set(bytes);
    THREAD_STACK_CEILING.set(0);
    MAX_STACK_DEPTH_BYTES.set(bytes);
}

/// Test/calibration hook: the stack bytes one nesting level of a recursive
/// path costs, measured through the guard itself. `probe(n)` runs the path
/// on an n-deep input; under an enforced budget of `budget` bytes the
/// deepest passing `n` gives `budget / n`. Runs on a fresh thread whose base
/// is its entry frame. Any error other than 54001 is a test failure. Depths
/// grow by doubling, so no probe runs past twice the guard's depth.
pub fn measured_bytes_per_level<F>(budget: usize, max_n: usize, probe: F) -> usize
where
    F: Fn(usize) -> PgResult<()> + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(budget + (32 << 20))
        .spawn(pagegive::pinned(move || {
            set_stack_base();
            set_enforced_stack_budget_for_tests(budget as isize);
            let fits = |n: usize| match probe(n) {
                Ok(()) => true,
                Err(e) if e.sqlstate() == ERRCODE_STATEMENT_TOO_COMPLEX => false,
                Err(e) => panic!("probe({n}) failed with something other than 54001: {e:?}"),
            };
            assert!(fits(1), "a 1-deep probe must fit in {budget} bytes");
            let (mut lo, mut hi) = (1, 2);
            while fits(hi) {
                assert!(hi < max_n, "a {hi}-deep probe fits in {budget} bytes; raise max_n");
                (lo, hi) = (hi, hi * 2);
            }
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if fits(mid) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            budget / lo
        }))
        .expect("spawn measuring thread")
        .join()
        .expect("measuring thread panicked")
}

/// Test-facing: the per-level stack cap that applies to this build, keyed on
/// the same opt-level classes as `STACK_DEPTH_SCALE`.
pub fn per_level_cap(optimized: usize, unoptimized: usize) -> usize {
    if cfg!(pgrust_opt_level = "0") || cfg!(pgrust_opt_level = "1") {
        unoptimized
    } else {
        optimized
    }
}

/// Record this thread's real stack reservation so the guard always fires
/// before the stack actually ends. Call once at thread start, next to
/// set_stack_base(). Keeps `min(8MiB, size/2)` in reserve below the
/// reservation for the frames past the last check (elog machinery etc.).
pub fn set_thread_stack_ceiling(stack_size: usize) {
    THREAD_STACK_CEILING.set(reserved_ceiling(stack_size));
    MAX_STACK_DEPTH_BYTES.set(effective_budget(SCALED_STACK_BUDGET.get()));
}

fn reserved_ceiling(stack_size: usize) -> isize {
    let size = stack_size.min(isize::MAX as usize) as isize;
    let reserve = (size / 2).min(8 << 20);
    (size - reserve).max(0)
}

/// D3103: wasm32's guard reads the SHADOW stack (wasm-ld's `__stack_pointer` region in linear memory, 64 MiB: only
/// address-taken locals and by-memory aggregates live there), but the rest of every frame lives on the engine's own
/// stack (V8: ~984 KB per thread unless the host raises it; wasmtime: `--max-wasm-stack`). With no ceiling, whichever
/// runs out first kills the server: the engine stack (RangeError / "call stack exhausted") at the browser's size, the
/// shadow stack (SP wraps below 0: "memory access out of bounds") at a 16 MB engine stack. So wasm32's ceiling is the
/// smaller of the shadow room and the shadow-byte equivalent of the engine stack the host declares
/// (`PGRUST_WASM_ENGINE_STACK`, bytes; V8's default when unset).
///
/// MEASURED (D3103 calibration, 2026-10-07): the release postgres.wasm of 7594b322ef6 in node v22.20.0 (linux-arm64),
/// `node --stack-size=984|4000|16000` (KB), max_stack_depth=60000 so the engine dies first, the shadow SP read at the
/// RangeError. Engine bytes per shadow byte at death, per chain (984 / 4000 / 16000 KB): `SELECT 1+1+...+1` (100k terms)
/// 0.26 / 0.26 / 0.26; 3000 nested scalar subqueries 0.35 / 0.35 / (shadow ran out first); SQL-function self-recursion
/// 0.42 / 0.41 / 0.39; plpgsql self-recursion 0.56 / 0.51 / 0.49; `repeat('[', n)::json` 5.07 / 5.74 / 5.93;
/// `repeat('{"a":', n)::jsonb` 5.05 / 5.74 / 5.93 (those charge `WASM_NATIVE_HEAVY_WEIGHT`). The ceiling assumes the
/// worst non-JSON chain rounded up: 0.6 = 3/5.
pub const WASM_ENGINE_PER_SHADOW: (usize, usize) = (3, 5);

/// V8's default stack (node main thread, browser workers) when the host declares none.
pub const DEFAULT_WASM_ENGINE_STACK: usize = 984 * 1024;

/// How many times its shadow bytes a native-heavy recursion charges on wasm32 (`check_stack_depth_weighted`): the JSON
/// parsers spend 5.1-5.9 engine bytes per shadow byte (the MEASURED table above), about 10x the 0.6 the ceiling
/// assumes. 1 on every other target, where `check_stack_depth_weighted` IS `check_stack_depth` (asserted below).
pub const WASM_NATIVE_HEAVY_WEIGHT: usize = if cfg!(target_family = "wasm") { 10 } else { 1 };
#[cfg(not(target_family = "wasm"))]
const _: () = assert!(WASM_NATIVE_HEAVY_WEIGHT == 1, "native check_stack_depth_weighted must be the plain check");

/// The shadow-byte size an engine stack of `engine_bytes` stands for.
pub fn wasm_shadow_equivalent(engine_bytes: usize) -> usize {
    let (engine, shadow) = WASM_ENGINE_PER_SHADOW;
    engine_bytes / engine * shadow
}

#[cfg(target_family = "wasm")]
extern "C" {
    /// wasm-ld's lowest address of the shadow stack.
    static __stack_low: u8;
}

/// wasm32 only: the guard's ceiling for an engine stack of `engine_bytes` (call after `set_stack_base`).
#[cfg(target_family = "wasm")]
pub fn set_wasm_stack_ceiling(engine_bytes: usize) {
    // SAFETY: only the symbol's address is taken; it is never read.
    let low = unsafe { core::ptr::addr_of!(__stack_low) } as usize;
    let shadow_room = STACK_BASE_PTR.get().saturating_sub(low);
    let engine_equivalent = wasm_shadow_equivalent(engine_bytes);
    let size = shadow_room.min(engine_equivalent);
    WASM_SHADOW_CEILING.set(reserved_ceiling(shadow_room));
    WASM_ENGINE_CEILING.set(reserved_ceiling(engine_equivalent));
    // wasm32 never fills the rlimit cache (WASI has no rlimits), so it holds the declared size instead.
    STACK_DEPTH_RLIMIT_CACHE.set(size.min(isize::MAX as usize) as isize);
    set_thread_stack_ceiling(size);
}

/// The stack the max_stack_depth boot default derives from (`adjust_max_stack_depth_from_rlimit`): on wasm32 the
/// shadow-byte size `set_wasm_stack_ceiling` computed from the host-declared engine stack (-1 before it runs), so the
/// JS hosts need no max_stack_depth override; elsewhere the platform rlimit. The GUC check hook keeps reading
/// `get_stack_depth_rlimit` (-1 on wasm32: any explicit value is accepted, the ceiling bounds what is enforced).
pub fn stack_depth_default_source() -> isize {
    #[cfg(target_family = "wasm")]
    return match STACK_DEPTH_RLIMIT_CACHE.get() {
        0 => -1,
        v => v,
    };
    #[cfg(not(target_family = "wasm"))]
    get_stack_depth_rlimit()
}

/// `check_stack_depth` for a recursion whose frames live almost wholly on the engine stack on wasm32 (the JSON
/// parsers): the shadow depth counts `WASM_NATIVE_HEAVY_WEIGHT` times. Identical to `check_stack_depth` elsewhere.
#[inline]
pub fn check_stack_depth_weighted() -> PgResult<()> {
    if WASM_NATIVE_HEAVY_WEIGHT == 1 {
        return check_stack_depth();
    }
    let stack_base_ptr = STACK_BASE_PTR.get();
    let depth = stack_base_ptr.abs_diff(current_stack_addr()) as isize;
    if guarded_depth(depth.saturating_mul(WASM_NATIVE_HEAVY_WEIGHT as isize)) > MAX_STACK_DEPTH_BYTES.get() && stack_base_ptr != 0 {
        return Err(stack_depth_exceeded());
    }
    Ok(())
}

/// D3812: the part of its shadow bytes a shadow-heavy recursion counts against wasm32's engine-derived ceiling, as
/// (numerator, denominator) (`check_stack_depth_shadow_heavy_or_panic`). (1, 1) on every other target.
///
/// MEASURED (2026-10-10, as for `WASM_ENGINE_PER_SHADOW`: node v22.20.0 linux-arm64, the guard lifted so the engine dies
/// first): a nested EXCEPT chain dies in decompose's plan mapping at 4.69 MB of shadow at 984 KB and 21.3 MB at 4000 KB,
/// 0.215 / 0.192 engine bytes per shadow byte; decompose's join chains spend under 0.15 (200 tables in 500 KB). The worst,
/// 0.215, is 0.36 of the ceiling's 0.6: rounded up to 3/8, which trips that chain at ~55% of a 984 KB engine stack.
pub const WASM_SHADOW_HEAVY_PART: (isize, isize) = if cfg!(target_family = "wasm") { (3, 8) } else { (1, 1) };
#[cfg(not(target_family = "wasm"))]
const _: () = assert!(WASM_SHADOW_HEAVY_PART.0 == WASM_SHADOW_HEAVY_PART.1, "native shadow-heavy check must be the plain check");

/// D3812: `check_stack_depth_or_panic` for a recursion whose frames keep most of their state on wasm32's shadow stack
/// (by-memory aggregates: decompose's walks), which spend far fewer engine bytes per shadow byte than wasm32's
/// engine-derived ceiling assumes. Identical to `check_stack_depth_or_panic` on every other target.
#[inline]
pub fn check_stack_depth_shadow_heavy_or_panic() {
    #[cfg(not(target_family = "wasm"))]
    check_stack_depth_or_panic();
    #[cfg(target_family = "wasm")]
    if shadow_heavy_too_deep() {
        std::panic::panic_any(stack_depth_exceeded());
    }
}

/// The whole depth against max_stack_depth's scaled budget and the shadow room, its `WASM_SHADOW_HEAVY_PART` against the
/// engine-derived ceiling; `EngineLevel`'s charge counts whole in each.
#[cfg(target_family = "wasm")]
#[inline(never)]
fn shadow_heavy_too_deep() -> bool {
    let stack_base_ptr = STACK_BASE_PTR.get();
    let engine_ceiling = WASM_ENGINE_CEILING.get();
    if stack_base_ptr == 0 || engine_ceiling == 0 {
        return stack_is_too_deep();
    }
    let shadow = stack_base_ptr.abs_diff(current_stack_addr()) as isize;
    let (num, den) = WASM_SHADOW_HEAVY_PART;
    let whole = guarded_depth(shadow);
    whole > SCALED_STACK_BUDGET.get() || whole > WASM_SHADOW_CEILING.get() || guarded_depth(shadow / den * num) > engine_ceiling
}

// Platform stack limit in bytes, -1 if unknown; cached after first call.
pub fn get_stack_depth_rlimit() -> isize {
    // Miri has no getrlimit; -1 is C's "limit unknown" (accept any value).
    // wasm32: WASI has no rlimits either — the same C no-getrlimit arm.
    #[cfg(any(miri, target_family = "wasm"))]
    return -1;
    #[cfg(not(any(miri, target_family = "wasm")))]
    {
        let cached = STACK_DEPTH_RLIMIT_CACHE.get();
        if cached != 0 {
            return cached;
        }

        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes into the provided rlimit struct.
        let val = if unsafe { libc::getrlimit(libc::RLIMIT_STACK, &mut rlim) } < 0 {
            -1
        } else if rlim.rlim_cur == libc::RLIM_INFINITY {
            isize::MAX
        } else if rlim.rlim_cur >= isize::MAX as libc::rlim_t {
            isize::MAX
        } else {
            rlim.rlim_cur as isize
        };

        STACK_DEPTH_RLIMIT_CACHE.set(val);
        val
    }
}
