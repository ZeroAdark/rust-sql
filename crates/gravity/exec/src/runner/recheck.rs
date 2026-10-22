//! The batch re-run for errors at statement grain (v5 interfaces §2.3, `Batch::Statement`; the errorhold home). A
//! departure from PostgreSQL's order that may raise runs under a recheck licence (`effects::licence`), and the lowering
//! marks its pipeline `Pipeline::recheck` (lowerv4 `Px::licensed`). When such a SELECT fails before any row left it, it
//! runs once more in plan order: admitted again inside `effects::deny_all` (every licence denied, so each family emits
//! its PostgreSQL form; the program is never looked up or filed), and that run's rows or error are the statement's.
//! Gravity raises only where PostgreSQL's order raises. The two re-run sites are the simple protocol's runner
//! (`runner::simple`) and the portal path (`admit::entry` `executor_run`).
use super::*;

/// A plain SELECT (no row lock, no write: nothing a second run repeats) whose programs hold a recheck licence, outside
/// the plan-order run.
pub fn rechecks(a: &Admitted, pstmt: &PlannedStmt<'_>) -> bool {
    let licensed = |p: &Program| p.pipelines.iter().any(|x| x.recheck);
    !in_plan_order()
        && a.cmd == Cmd::Select
        && pstmt.rowMarks.iter().next().is_none()
        && (licensed(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| licensed(&x.program)) || g.inits.iter().any(|x| licensed(&x.program))))
}

/// An error the statement's own code raised, which the plan-order run may not raise. Never re-run: a cancel, a statement
/// timeout or a termination (class 57: the interrupt is consumed, a re-run would swallow it), a lost connection, a
/// transaction or lock failure (08, 25, 40, 55), a system error (58), or anything at FATAL or above.
pub fn rechecked(e: &types_error::PgError) -> bool {
    let class = e.sqlstate.0 & 0xfff;
    let of = |c: [u8; 2]| types_error::make_sqlstate([c[0], c[1], b'0', b'0', b'0']).0 & 0xfff;
    e.level < types_error::FATAL && ![*b"57", *b"08", *b"25", *b"40", *b"55", *b"58"].into_iter().any(|c| class == of(c))
}

/// The first run's error a refused plan-order run gives back as the statement's (no refusal in its place): a resource's (class 53: out of memory, a disk full; 54: a program limit), which names no
/// order of rows. A licensed departure's own error (a 22012 the departure met ahead) stays behind the refusal, which
/// names the gap: PostgreSQL's order may return rows where the departure raised. `DECISIONS.md` 4 (D) b: except an
/// error a part run ahead under a tagged licence raised (`ahead_tagged`), which is the statement's where PostgreSQL may
/// return rows.
pub fn surfaces_over_refusal(e: &types_error::PgError) -> bool {
    let class = e.sqlstate.0 & 0xfff;
    let of = |c: [u8; 2]| types_error::make_sqlstate([c[0], c[1], b'0', b'0', b'0']).0 & 0xfff;
    [*b"53", *b"54"].into_iter().any(|c| class == of(c)) || ahead_tagged(e)
}

/// `DECISIONS.md` 4 (D) b: `e` is the error of a part run ahead of PostgreSQL's plan under a tagged licence (exec glue
/// `pg_error` put the run-ahead tag first in its log-only detail).
pub fn ahead_tagged(e: &types_error::PgError) -> bool {
    e.detail_log().is_some_and(|d| d.starts_with(crate::glue::AHEAD_TAG))
}

/// D4206: whether error `e`, raised by a part run ahead of PostgreSQL's plan under a tagged licence, stands as raised. A
/// data error (classes 21, 22, 23) is the statement's (`DECISIONS.md` 4 (D) b, which covers these only); a resource
/// limit (53, 54) is quiet both ways (no run's row order decides it); an error that is not the plan's answer passes
/// through as raised and is never refused or re-run: an interrupt (57: a cancel, statement_timeout, a shutdown), a
/// transaction rollback (40) or state (25), a lock not available (55P03), a connection or system error (08, 58), anything
/// at FATAL or above. Any other class (a feature not supported, a rule of the catalog, an internal error) becomes a
/// refusal by name (exec glue `pg_error`): PostgreSQL may return rows where the part raised it.
pub fn tag_stands(e: &types_error::PgError) -> bool {
    let code = types_error::unpack_sqlstate(e.sqlstate());
    e.level >= types_error::FATAL || matches!(&[code[0], code[1]], b"21" | b"22" | b"23" | b"53" | b"54" | b"57" | b"40" | b"25" | b"08" | b"58") || &code == b"55P03"
}

/// D4206: `e` is the refusal exec glue `pg_error` made of a run-ahead error `tag_stands` refuses: the statement's own
/// code raised it, so the plan-order run re-runs it where it re-runs that error (`reruns`).
pub fn tag_refused(e: &types_error::PgError) -> bool {
    tag_refusal_of(e).is_some()
}

/// D4206: the reason of `e` when it is the refusal of a run-ahead error (`tag_refused`), `None` for any other error.
pub fn tag_refusal_of(e: &types_error::PgError) -> Option<&str> {
    super::refusal_of(e).filter(|w| w.starts_with(crate::glue::TAG_REFUSAL))
}

/// D4206: the refusal of a run-ahead error that leaves a run as the statement's (no plan-order run took its place) logs
/// the named refusal line every named refusal logs (`Backend::note_removed`), so a test marks it by its name
/// (e2e-refused-by-name.py reads that line's refusal part): it is refused after its admission, where no admission line
/// names it. The portal's road (admit/entry `run_entry`); `simple_at` logs it on its own backend.
pub fn note_tag_refused(e: &types_error::PgError) {
    if let Some(why) = tag_refusal_of(e) {
        let _ = crate::with_backend(|b| b.note_removed(why));
    }
}

/// v5 errorhold: `e` may re-run in plan order (where the statement holds a recheck licence and sent no row): an error the
/// statement's code raised (`rechecked`) that is no refusal, or the refusal of a run-ahead error (`tag_refused`).
/// D4402: never once the memory governor canceled the statement (`memory_canceled`): its ledger refuses the plan-order
/// run every byte until the statement ends, and the first run's poll already consumed the cancel's interrupt, so the
/// re-run's grant admission waited for room it could never take and the cancel went unanswered to statement_timeout
/// (57014 at 600 s). The first run's 53200 is the statement's. A 53200 nobody canceled (a grant-pool or draw refusal, a
/// statement_mem_limit) re-runs as before.
pub fn reruns(e: &types_error::PgError) -> bool {
    (super::refusal_of(e).is_none() || tag_refused(e)) && rechecked(e) && !memory_canceled()
}

/// D4402: the memory governor canceled this thread's running statement (the ladder's R6, the watchdog): its ledger
/// holds the cancel until the statement ends.
fn memory_canceled() -> bool {
    mcx::governor::watchdog_cancel_pending()
}

/// D2573: the transaction state user code leaves behind when its error crosses back into a served run, read when the run
/// starts: the SPI stack's depth (a PL/pgSQL function returns its error before its SPI_finish), the GUC nest level (a SET
/// clause's level stays raised) and the transaction nest level (a subtransaction left open). PostgreSQL's abort unwinds
/// all three; a re-run in plan order (`recheck`) and a run-time refusal's fall-back to the row executor (D1509) go on in
/// the same transaction instead, so either runs only while all three are what the run started with (an SPI receiver
/// would take the plan-order run's rows into the leftover connection: improper call to spi_printtup). Otherwise the
/// run's error is the statement's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlState {
    spi: usize,
    guc: i32,
    xact: i32,
}

impl PlState {
    pub fn now() -> PlState {
        PlState { spi: spi::debug_stack_depth(), guc: guc::guc_nest_level(), xact: xact::GetCurrentTransactionNestLevel() }
    }

    /// Nothing the run called left SPI, GUC or subtransaction state behind: the statement may go on after its error.
    pub fn kept(self) -> bool {
        self == PlState::now()
    }

    /// D2126: undo what a callee's error left behind once the run holds that error instead of aborting (a cursor's
    /// fetch keeps the rows before it; PostgreSQL never reaches the error when the cursor stops first, and its abort would
    /// undo the same): the SPI connections above the run's depth (`spi::abort_above_depth`) and a raised GUC nest level
    /// (AtEOXact_GUC at the run's level + 1, as a failed function's SET clause is undone). A subtransaction left open, or
    /// a depth or level below the run's, is not undone. Call it only between calls (no user code running). Answers
    /// `kept` after.
    pub fn restore(self) -> bool {
        let now = PlState::now();
        if now == self {
            return true;
        }
        if now.xact != self.xact || now.spi < self.spi || now.guc < self.guc {
            return false;
        }
        if now.spi > self.spi {
            spi::abort_above_depth(self.spi);
        }
        if now.guc > self.guc {
            guc::AtEOXact_GUC(false, self.guc + 1);
        }
        self.kept()
    }
}

/// `f` in plan order: every licence denied, so the admission it runs lowers each family's PostgreSQL form.
pub fn plan_order<T>(f: impl FnOnce() -> T) -> T {
    gravity_decompose::effects::deny_all(f)
}

/// `plan_order` for a run of `count` rows (0: all): a read that stops at a count is bounded (`effects::deny_all_bounded`:
/// a family whose PostgreSQL form reads lazily and is not built refuses there, D2316).
pub fn plan_order_for<T>(count: u64, f: impl FnOnce() -> T) -> T {
    match count {
        0 => gravity_decompose::effects::deny_all(f),
        _ => gravity_decompose::effects::deny_all_bounded(f),
    }
}

/// Inside the plan-order run.
pub fn in_plan_order() -> bool {
    gravity_decompose::effects::denying()
}

/// The report line of a re-run (`PGRUST_GRAVITY_REPORT`). NEW-numeric-1: a run whose merges met an order-key tie (abi
/// `KEY_TIES`, taken here) names FOLD_TIE, the cause its error's text does not.
pub fn note_recheck(msg: &str) {
    let ties = take_key_ties();
    let _ = crate::with_backend(|b| {
        if b.reporting() {
            b.report_line(&recheck_line(ties, msg));
        }
    });
}

/// NEW-numeric-1: the merges that met an order-key tie since the last re-run (abi `KEY_TIES`, taken: zeroed). Both re-run
/// sites take it, reporting or not.
pub fn take_key_ties() -> u64 {
    gravity_abi::breaker::KEY_TIES.swap(0, core::sync::atomic::Ordering::Relaxed)
}

/// A re-run's report line; `ties` > 0 names FOLD_TIE and its merges, the cause the error's text does not name.
pub fn recheck_line(ties: u64, msg: &str) -> String {
    match ties {
        0 => format!("gravity: recheck in plan order after: {msg}"), // alloc: Cold
        n => format!("gravity: recheck in plan order after: FOLD_TIE ({n} merges): {msg}"), // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    use types_error::{make_sqlstate, ErrorLevel, PgError, FATAL};

    fn err(state: &[u8; 5]) -> PgError {
        PgError::error("x").with_sqlstate(make_sqlstate(*state))
    }

    /// The statement's own errors re-run; an interrupt, a connection, transaction, lock or system error never does, nor
    /// anything at FATAL or above (the prototype's re-run swallowed a 40 ms statement_timeout and returned rows).
    #[test]
    fn rechecked_takes_the_statements_own_errors_only() {
        for s in [b"22012", b"22003", b"22P02", b"2201F", b"23514", b"XX000", b"42883"] {
            assert!(super::rechecked(&err(s)), "{}", core::str::from_utf8(s).unwrap());
        }
        for s in [b"57014", b"57P01", b"57P05", b"08006", b"25P02", b"25006", b"40001", b"40P01", b"55P03", b"55006", b"58030"] {
            assert!(!super::rechecked(&err(s)), "{}", core::str::from_utf8(s).unwrap());
        }
        let mut fatal = err(b"22012");
        fatal.level = FATAL;
        assert!(!super::rechecked(&fatal));
        fatal.level = ErrorLevel(FATAL.0 + 1);
        assert!(!super::rechecked(&fatal));
    }

    /// D2573: a GUC nest level user code left raised (a SET clause's, on its error) is PL state the run did not keep; once
    /// unwound, the run's state is kept again.
    #[test]
    fn a_raised_guc_level_is_pl_state_left_behind() {
        let at = super::PlState::now();
        assert!(at.kept());
        let level = guc::NewGUCNestLevel();
        assert!(!at.kept(), "a SET clause's level left raised");
        guc::AtEOXact_GUC(false, level);
        assert!(at.kept(), "unwound");
    }

    /// A refused plan-order run gives back the first run's resource error (an out-of-memory, a disk full, a program
    /// limit), never a licensed departure's own error (PostgreSQL's order may return rows there: the refusal names it).
    #[test]
    fn only_a_resource_error_surfaces_over_a_refusal() {
        for s in [b"53200", b"53100", b"53000", b"54000", b"54001"] {
            assert!(super::surfaces_over_refusal(&err(s)), "{}", core::str::from_utf8(s).unwrap());
        }
        for s in [b"22012", b"22003", b"2201W", b"2201X", b"21000", b"XX000", b"23505"] {
            assert!(!super::surfaces_over_refusal(&err(s)), "{}", core::str::from_utf8(s).unwrap());
        }
        // `DECISIONS.md` 4 (D) b: a run-ahead part's error is the statement's
        let tagged = err(b"22012").with_detail_log("gravity ahead: 22012 node 2 Seq Scan (DECISIONS 4 (D) b)".to_string()); // alloc: Cold
        assert!(super::ahead_tagged(&tagged) && super::surfaces_over_refusal(&tagged));
        assert!(!super::ahead_tagged(&err(b"22012").with_detail_log("Key (a)=(1) already exists.".to_string()))); // alloc: Cold
    }

    /// NEW-numeric-1: a FOLD_TIE re-run's line names its merges.
    #[test]
    fn the_fold_tie_line_names_its_merges() {
        assert_eq!(super::recheck_line(0, "x"), "gravity: recheck in plan order after: x");
        assert_eq!(super::recheck_line(2, "x"), "gravity: recheck in plan order after: FOLD_TIE (2 merges): x");
    }

    /// D4402 (born red): a statement the memory governor canceled never re-runs in plan order. The ladder's R6 (as the
    /// watchdog's cancel) marks the statement's ledger and condemns its backend; the run's interrupt poll consumes the
    /// condemnation and raises its 53200 (`take_watchdog_cancel`, ProcessInterrupts' road). Before the fix that 53200
    /// re-ran a licensed SELECT in plan order under the same canceled ledger: its admission waited for grant bytes the
    /// ledger refuses (`fits_raw_admit`), with no condemnation left to raise, until statement_timeout (memtest's e2
    /// take: three hash joins, 57014 at 600 s). The same 53200 of a statement nobody canceled (a refused draw) still
    /// re-runs.
    #[test]
    fn a_memory_canceled_statement_never_reruns_in_plan_order() {
        std::thread::spawn(|| {
            use mcx::governor;
            let hog = core::alloc::Layout::from_size_align(256 << 20, 8).unwrap();
            governor::register(4002, 44021);
            let ctx = mcx::MemoryContext::new("d4402 hog");
            governor::statement_start(0, true);
            assert!(super::reruns(&err(b"53200")), "a 53200 nobody canceled re-runs as before");
            ctx.mcx().alloc_uninit_bytes(hog).unwrap();
            let v = governor::cancel_largest_victim(90, 32 << 20).expect("the hog is chosen");
            assert_eq!(v.backend, Some((4002, 44021)), "{v:?}");
            let e = governor::take_watchdog_cancel().expect("the run's poll raises the cancel's 53200");
            assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY);
            assert!(super::memory_canceled(), "the ledger holds the cancel until the statement ends");
            assert!(!super::reruns(&e), "the cancel's 53200 is the statement's: no plan-order run under a canceled ledger");
            governor::statement_end();
            drop(ctx);
            governor::statement_start(0, true);
            assert!(!super::memory_canceled() && super::reruns(&err(b"53200")), "the next statement re-runs as before");
            governor::statement_end();
        })
        .join()
        .unwrap();
    }

    /// The plan-order run is `deny_all`'s scope: inside it no licence is granted and nothing re-runs again.
    #[test]
    fn plan_order_denies_every_licence() {
        use gravity_decompose::effects::{licence, Departure, Effects};
        assert!(!super::in_plan_order());
        assert!(super::plan_order(|| super::in_plan_order() && licence(Effects::RAISES, Departure::Ahead).is_err()));
        assert!(!super::in_plan_order(), "restored");
    }
}
