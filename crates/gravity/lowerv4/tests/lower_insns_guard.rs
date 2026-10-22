//! D3886's counter guard on the lowering (main 2026-10-11T03:28:51Z): the user-mode instructions one lowering of three
//! representative statements takes stay within `TOLERANCE` of their budgets, so the cold-compile cuts (one IR code key per
//! lowering, the IR builder's dense constant table, no on-demand walks without their hosts, the emit plan's dense need
//! index) cannot creep back unseen. A statement's count is the minimum over the warm lowerings (the first one fills the
//! process's caches: function resolution, the IR's one verify), on this thread only (`perf_event_open`, instructions, user
//! mode). A warm lowering's verify is the verify-once memo's hit (its code key included), so the IR verifiers' own walks
//! are not in the count.
//! - Budgets are the shipped `fast-profile`'s (USER RULING 2026-10-15T14:56Z) on aarch64 (the c8g boxes), the guard built
//!   the way the gate's fast leg builds it: `cargo test --profile fast-profile --config
//!   'profile.fast-profile.package.gravity_lowerv4.codegen-units=1' -p gravity_lowerv4_it --test lowerv4_suite`. The
//!   crate tests' `check-test` run prints its counts beside them and judges none; so does any other profile or architecture.
//! - No counter (perf_event_paranoid above 2, no PMU in the VM, not Linux): the test says SKIP and passes.
//! - A change that makes these lowerings dearer on purpose raises the budget in the same commit, saying why; a cut lowers it.
//! - lowerv4 at one codegen unit (testing PD-31): at the profile's 16 units, lowerv4's partition alone moves these counts by
//!   up to 1.5 %, even for a change in code these statements never run.
//!
//! The statements (decompose's plan fixtures): oltp-001, a point SELECT by its key (one pipeline, an ORM `find`); oltp-016,
//! a two-table join on a composite key with ORDER BY and parameters; oltp-018, a three-table join.
use gravity_lowerv4_it as common;

/// The allowed growth over a budget.
const TOLERANCE: f64 = 1.05;
/// Lowerings per statement: the first warms, the minimum of the rest is judged.
const RUNS: usize = 7;
/// (fixture, its budget: instructions:u per warm lowering in the fast profile since the last re-base below; before it, in the
/// check-test profile, built the way the gate builds it (check.sh
/// ct: workspace feature resolution, CARGO_PROFILE_CHECK_TEST_DEBUG=0, STRIP=debuginfo) and measured on box 71 (c8g class,
/// Neoverse-V2) at trunk 0f189af3fef, three processes: 533,352 / 1,528,692 / 3,093,555-3,102,534 (ece3316ca67, one
/// landing before it: 538,132 / 1,539,280 / 3,121,418-3,123,970; 0f189af3fef's lowerv4 C1 effects refactor cut ~0.7 %).
/// RE-BASED to trunk under D4119 (main 2026-10-12T00:40:31Z: the guard sat at its 5 % edge and blocked two unrelated entries).
/// This is not a deliberate growth: D4119 owns finding and cutting it. Its parts on oltp-016 (box 71, one-package builds):
/// +2.2 % was already in the guard as it landed (f6d9eba2704, a re-cut onto a trunk 238 commits past the budgets' base),
/// +0.94 % came in one landing (39341154552..00cb82df25d), and the rest is creep of 0.1-0.3 % per stretch of landings.
/// The first budgets, 522,000 / 1,470,000 / 3,020,000, came from D3886's tip fa019cc4a44 on box 24 (521,958 / 1,469,132 /
/// 3,012,402-3,019,162); the trunk before it, 6014362c0f8, read 784,105 / 1,772,866 / 5,244,591-5,247,722.)
/// RAISED under D4477 (main 2026-10-15T02:56:01Z / 03:07Z, approved in advance for a check-test-only rise) from 534,000 /
/// 1,529,000 / 3,103,000 to trunk 2fb7b14bf1e's gate-built counts (box 62: 593,591 / 1,904,320 / 3,792,267; box 48:
/// 593,637 / 1,906,572 / 3,787,550). The rise is the plan-time scratch moving from malloc into the GravityPlan context
/// (planmem tranche 2): in check-test every mcx allocation pays the aset path and the debug-only FreedSet at opt-level 1.
/// The same lowerings in fast-profile did not rise (box 45, one-package builds, 472ec0a2068 -> 7c47f674eb6: 388,300 ->
/// 383,035 / 1,072,638 -> 1,036,132 / 2,221,294 -> 2,170,197; check-test there 537,210 -> 587,367 / 1,571,248 -> 1,887,618
/// / 3,113,261 -> 3,759,068).
/// RE-BASED under D4477 route (a) (main 2026-10-15T03:10Z): check-test now builds mcx, hashbrown and allocator-api2 at
/// opt-level 3 (root Cargo.toml), which cut these lowerings at the same trunk from 594,757 / 1,911,306 / 3,781,869 (gate-built,
/// box 45) to the budgets below (one-package box 45 568,447 / 1,764,860 / 3,530,299, scaled by the gate build's measured
/// +1.3 / +1.3 / +0.6 %). fast-profile, unchanged by the profile block, read 383,035 / 1,036,132 / 2,178,718.
/// Set to (a)'s own gate-built counts once it was prechecked (box 48, 74140243ebc on trunk 05fef9a5aef, three processes):
/// 564,640 / 1,772,862 / 3,566,797-3,576,678; the scaled estimates above were 576,000 / 1,788,000 / 3,551,000.
/// RE-BASED with planmem's W1 LAST (main's rule 2026-10-15T03:12Z: a check-test-only rise with fast-profile inside +1 %
/// takes a re-base with the release table here). Gate-like builds (--workspace --test lowerv4_suite, DEBUG=0,
/// STRIP=debuginfo), dedicated box under sudo -n, three processes, tranche 4 36dcb504cb6 -> LAST 6fad93ba42e:
/// check-test 570,242 -> 575,567 / 1,831,091 -> 1,873,276 / 3,625,057 -> 3,662,096 (+0.9 / +2.3 / +1.0 %);
/// fast-profile, lowerv4 at one codegen unit (PD-31): 323,318 -> 323,046 / 921,048 -> 919,617 / 1,905,051 -> 1,901,894;
/// release (thin LTO, cgu 1): 299,801 -> 299,572 / 856,191 -> 855,053 / 1,784,614 -> 1,783,562. The check-test rise is
/// generic PgVec / GrowIn code instantiated in lowerv4 at check-test's opt-level 1; the shipped build reads lower.
/// RE-BASED with planmem's W2 wave 3 (R), the row values in the context (main 2026-10-15, the 03:12Z rule), same method,
/// trunk ef09c68a5f3 -> 895ab1a90ea: check-test 581,719 -> 594,307 / 1,878,551 -> 1,965,677 / 3,676,774 -> 3,773,937
/// (+2.2 / +4.6 / +2.6 %); fast-profile cgu 1: 318,250 -> 317,797 / 905,723 -> 904,804 / 1,863,586 -> 1,872,064;
/// release: 295,322 -> 294,940 / 842,813 -> 845,638 / 1,745,956 -> 1,743,420. Cold analytic-suite W0 plan time 1.0039 (A/A 1.0000).
/// RE-BASED with planmem's W2 wave 3 (C), the row carry in the context (main 2026-10-15T12:45Z, the 03:12Z rule; no extra
/// headroom), same method, the precheck's replayed pair on trunk 22f46953282 (+ A 59a5dc168ee -> + C 37ba1bfbb7f):
/// check-test 605,550 -> 613,639 / 2,030,217 -> 2,076,339 / 3,923,919 -> 4,034,108 (+1.3 / +2.3 / +2.8 %; precheck box
/// 48 read 613,978 / 2,077,835 / 4,041,509); fast-profile cgu 1: 313,991 -> 314,375 / 895,627 -> 897,028 / 1,830,377 ->
/// 1,840,334; release: 291,598 -> 292,044 / 834,491 -> 836,289 / 1,725,491 -> 1,724,613. Trunk itself had risen from
/// 595,000 / 1,966,000 / 3,774,000 with other landings. Cold analytic-suite W0 plan time 1.0017 (A/A 1.0000).
/// RE-BASED to the fast profile (USER RULING 2026-10-15T14:56Z: the guard gates on the shipped profile; main 14:56:41Z),
/// the gate's fast-leg build above on the dedicated box under sudo -n, three processes, at trunk + planmem W2 wave 3 (S)
/// a581cc930f2, no headroom (rounded up to the thousand): fast-profile cgu 1 314,024 / 894,182 / 1,832,630; check-test (no
/// longer judged) 633,718 / 2,136,016 / 4,115,765 against its last budgets 614,000 / 2,078,000 / 4,042,000; release 291,818 /
/// 834,627 / 1,726,517. The gate's own fast leg read 314,180 / 894,060 / 1,839,601 where this build read 313,648 / 894,235 /
/// 1,839,451 (one trunk, +0.17 / -0.02 / +0.01 %); oltp-018 moves up to 0.4 % between runs of one tree.
const GUARDS: [(&str, u64); 3] = [("oltp-001", 315_000), ("oltp-016", 895_000), ("oltp-018", 1_833_000)];
/// The profile the budgets are judged in (the shipped one).
const GATE: &str = "fast-profile";

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
mod counter {
    /// `struct perf_event_attr` up to PERF_ATTR_SIZE_VER5 (112 bytes).
    #[repr(C)]
    #[derive(Default)]
    struct Attr {
        type_: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
        config2: u64,
        branch_sample_type: u64,
        sample_regs_user: u64,
        sample_stack_user: u32,
        clockid: i32,
        sample_regs_intr: u64,
        aux_watermark: u32,
        sample_max_stack: u16,
        reserved_2: u16,
    }

    extern "C" {
        fn syscall(number: i64, ...) -> i64;
        fn ioctl(fd: i32, request: u64, ...) -> i32;
        fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
        fn close(fd: i32) -> i32;
    }

    #[cfg(target_arch = "aarch64")]
    const SYS_PERF_EVENT_OPEN: i64 = 241;
    #[cfg(target_arch = "x86_64")]
    const SYS_PERF_EVENT_OPEN: i64 = 298;
    const PERF_TYPE_HARDWARE: u32 = 0;
    const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
    /// disabled, exclude_kernel, exclude_hv
    const FLAGS: u64 = 1 | 1 << 5 | 1 << 6;
    const IOC_ENABLE: u64 = 0x2400;
    const IOC_DISABLE: u64 = 0x2401;
    const IOC_RESET: u64 = 0x2403;

    /// This thread's user-mode instructions counter.
    pub struct Insns(i32);

    impl Insns {
        pub fn open() -> Option<Insns> {
            let attr = Attr { type_: PERF_TYPE_HARDWARE, size: core::mem::size_of::<Attr>() as u32, config: PERF_COUNT_HW_INSTRUCTIONS, flags: FLAGS, ..Attr::default() };
            // SAFETY: perf_event_open(2) over a live, fully initialised attr of the size it states; this thread (pid 0),
            // any cpu (-1), no group (-1), no flags.
            let fd = unsafe { syscall(SYS_PERF_EVENT_OPEN, &attr as *const Attr, 0i64, -1i64, -1i64, 0u64) };
            (fd >= 0).then_some(Insns(fd as i32))
        }
        pub fn start(&self) {
            // SAFETY: ioctls on our own perf event fd
            unsafe {
                ioctl(self.0, IOC_RESET, 0u64);
                ioctl(self.0, IOC_ENABLE, 0u64);
            }
        }
        pub fn stop(&self) -> u64 {
            let mut v = 0u64;
            // SAFETY: our own fd; read_format 0 reads one u64 into a live u64
            let n = unsafe {
                ioctl(self.0, IOC_DISABLE, 0u64);
                read(self.0, &mut v as *mut u64 as *mut u8, 8)
            };
            assert_eq!(n, 8, "the instructions counter read");
            v
        }
    }

    impl Drop for Insns {
        fn drop(&mut self) {
            // SAFETY: our own fd, closed once
            unsafe { close(self.0) };
        }
    }
}

#[cfg(not(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"))))]
mod counter {
    pub struct Insns;
    impl Insns {
        pub fn open() -> Option<Insns> {
            None
        }
        pub fn start(&self) {}
        pub fn stop(&self) -> u64 {
            0
        }
    }
}

/// The cargo profile directory the test binary was built into (`<target>/<profile>/deps/<binary>`), read off argv[0] (cargo
/// and ct both start the binary by its full path). The file only reads the path; it never re-runs itself (registered.rs).
fn profile() -> String {
    let exe = std::path::PathBuf::from(std::env::args_os().next().unwrap_or_default());
    exe.parent().and_then(|d| d.parent()).and_then(|d| d.file_name()).and_then(|d| d.to_str()).unwrap_or_default().to_owned()
}

#[test]
fn lowering_instructions_stay_within_their_budgets() {
    let Some(c) = counter::Insns::open() else {
        eprintln!("lower_insns_guard: SKIP: no instructions:u counter here (perf_event_open refused: perf_event_paranoid, no PMU, or not Linux)");
        return;
    };
    let profile = profile();
    let mut over = Vec::new();
    for (name, budget) in GUARDS {
        let text = std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut counts = Vec::new();
        match common::lower_n(&text, RUNS, || c.start(), || counts.push(c.stop())) {
            Some(Ok(())) => {}
            Some(Err(r)) => panic!("{name}: the guard's statement refused: {r}"),
            None => panic!("{name}: the guard's statement does not decompose"),
        }
        let min = counts[1..].iter().copied().min().unwrap_or(0);
        let judged = profile == GATE && cfg!(target_arch = "aarch64") && budget != 0;
        eprintln!("lower_insns_guard: {name}: {min} instructions:u per warm lowering (all {counts:?}; budget {budget} in {GATE}, profile {profile}, judged {judged})");
        if judged && min as f64 > budget as f64 * TOLERANCE {
            over.push(format!("{name}: {min} > {budget} x {TOLERANCE}"));
        }
    }
    assert!(over.is_empty(), "lowering instructions over budget (D3886's guard; a deliberate growth raises the budget, saying why): {}", over.join("; "));
}
