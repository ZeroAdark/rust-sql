seam_core::seam!(
    pub fn postgres_main(dbname: &str, username: &str) -> !
);

seam_core::seam!(
    // argv is the FULL server argv (argv[1] == "--single").
    pub fn postgres_single_user_main(argv: &[String], username: &str) -> !
);

seam_core::seam!(
    // argv[1] == "--stdio-wire"; username is the fallback identity (the startup packet's user wins).
    pub fn postgres_stdio_wire_main(argv: &[String], username: &str) -> !
);

seam_core::seam!(
    // Err is the ereport(FATAL) "terminating connection" path.
    pub fn process_client_read_interrupt(blocked: bool) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn process_client_write_interrupt(blocked: bool) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn process_postgres_switches(argv: &[String], gucctx: u8) -> types_error::PgResult<()>
);

seam_core::seam_tracked!(
    // a raised cancel/die comes back as the Err. K11 (D4214 / D4229): tracked, so the poll-gap probe's poll site is the
    // caller's line (tcop postgres installs check_for_interrupts_at with set_tracked); the 244 call sites are unchanged.
    pub fn check_for_interrupts() -> types_error::PgResult<()>
);

seam_core::seam_tracked!(
    // K11: tracked, as check_for_interrupts (tcop postgres installs check_for_abort_interrupts_at).
    // The abort class only (ProcessAbortInterrupts; docs/gravity/driver.md
    // §6): the Gravity kernels'
    // in-loop check (non-terminal today: the host runs it and continues on a
    // stale byte). Raises, or returns with AbortPending cleared.
    pub fn check_for_abort_interrupts() -> types_error::PgResult<()>
);

seam_core::seam!(
    // INTERRUPT_PENDING && INTERRUPTS_CAN_BE_PROCESSED, read only: a check now
    // would process an interrupt. Gravity's cancelpoll::interrupts::pending
    // (C6 (d)): a lowering, a compile or a compile wait stops there, then
    // checks.
    pub fn interrupts_pending() -> bool
);

seam_core::seam!(
    // INTERRUPTS_CAN_BE_PROCESSED && AbortPending, read only: a check now
    // would process an abort-class interrupt (a cancel, a timeout, a
    // terminate). Gravity's cancelpoll::interrupts::pending_abort (C6 (d)):
    // the grant pool's wait wakes for one.
    pub fn abort_pending() -> bool
);

seam_core::seam!(
    // INTERRUPT_PENDING alone, read only, whether or not it can be processed
    // now. Gravity's cancelpoll::interrupts::flagged (C6 (d)): the grant
    // pool's wait reads it after a check, to wait on for the abort class only.
    pub fn interrupt_flagged() -> bool
);

seam_core::seam!(
    // die (postgres.c) — SIGTERM's thread-model rendering: flag setter run on
    // the target backend's own thread; Err is the single-user immediate
    // ProcessInterrupts leg.
    pub fn die() -> types_error::PgResult<()>
);

seam_core::seam!(
    // must run on the target backend's own thread (flags are thread-local).
    pub fn statement_cancel_handler()
);

seam_core::seam!(
    pub fn quickdie() -> !
);

seam_core::seam!(
    pub fn float_exception_handler() -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn handle_recovery_conflict_interrupt(reason: u32)
);

seam_core::seam!(
    pub fn reset_usage()
);

seam_core::seam!(
    pub fn show_usage(title: &str) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn set_debug_options(debug_flag: i32, gucctx: u8) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn set_plan_disabling_options(arg: &str, gucctx: u8) -> types_error::PgResult<bool>
);

seam_core::seam!(
    pub fn get_stats_option_name(arg: &str) -> Option<&'static str>
);

#[cfg(pgrust_sim)]
seam_core::seam!(
    // argv[1] == "--sim-net"; username is the fallback identity.
    pub fn postgres_sim_net_main(argv: &[String], username: &str) -> !
);

seam_core::tap!(
    pub fn tap_serial_lease_admission()
);

pub mod stmt_yield {
    thread_local! {
        static ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    static HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

    pub fn set_hook(f: fn()) {
        let _ = HOOK.set(f);
    }

    pub fn arm() {
        ARMED.with(|c| c.set(true));
    }
    pub fn disarm() {
        ARMED.with(|c| c.set(false));
    }

    #[inline(always)]
    pub fn tick() {
        if ARMED.with(|c| c.get()) {
            if let Some(f) = HOOK.get() {
                f();
            }
        }
    }
}

pub mod stmt_task_arm {
    fn posture(v: Option<&str>) -> bool {
        matches!(v.map(str::trim), Some("1") | Some("on"))
    }

    pub fn stmt_task_enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| posture(std::env::var("PGRUST_STMT_TASK").ok().as_deref()))
    }

    thread_local! {
        static ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub struct StmtTaskArm(());

    impl Drop for StmtTaskArm {
        fn drop(&mut self) {
            ARMED.with(|c| c.set(false));
        }
    }

    pub fn arm_statement(eligible: bool) -> StmtTaskArm {
        if eligible && stmt_task_enabled() {
            ARMED.with(|c| c.set(true));
        }
        StmtTaskArm(())
    }

    pub fn take_armed() -> bool {
        ARMED.with(|c| c.replace(false))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn knob_default_off_exact_spellings() {
            assert!(!posture(None), "unset = default OFF");
            assert!(posture(Some("1")), "=1 arms");
            assert!(posture(Some("on")), "=on arms");
            assert!(posture(Some(" 1 ")), "trimmed arming spelling");
            assert!(!posture(Some("0")));
            assert!(!posture(Some("off")));
            assert!(!posture(Some("true")), "non-registry spelling stays OFF");
            assert!(!posture(Some("ON")), "case-sensitive: only the exact spellings arm");
            assert!(!posture(Some("")));
        }

        #[test]
        fn knob_env_takes() {
            let Ok(v) = std::env::var("PGRUST_STMT_TASK") else {
                println!("SKIP: PGRUST_STMT_TASK unset (armed-witness leg runs it =0)");
                return;
            };
            let expect = posture(Some(v.as_str()));
            assert_eq!(
                stmt_task_enabled(),
                expect,
                "process switch disagrees with spelled posture"
            );
        }

        #[test]
        fn arm_scoped_and_consumed() {
            {
                let _g = arm_statement(false);
                assert!(!take_armed(), "ineligible never arms");
            }
            ARMED.with(|c| c.set(true));
            assert!(take_armed(), "armed flag consumed");
            assert!(!take_armed(), "consumed exactly once");
            {
                ARMED.with(|c| c.set(true));
                let _g = StmtTaskArm(());
            }
            assert!(!take_armed(), "guard drop disarms");
        }
    }
}
