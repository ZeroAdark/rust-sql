use std::sync::Mutex;

use ::types_error::PgError;

use super::*;

// audit-18.6 w2-009 a186-candidate-fp-adt-b3-6b431c73d504b91699c0-1:
// pg_log_backend_memory_contexts (mcxtfuncs.c:295-300) reports a failed
// SendProcSignal as WARNING "could not send signal to process %d: %m" -- the
// strerror text of the errno SendProcSignal set (ESRCH: the target left its
// ProcSignal slot between the lookup and the signal).  The race cannot be
// forced from SQL, so the lookup and the send are injected here.

fn warnings() -> &'static Mutex<Vec<String>> {
    static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    &WARNINGS
}

// The capture buffer is shared by every test thread (the emit hook is
// per-thread, the buffer is not): one test at a time.
fn serialized() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn capture(err: &PgError, output_to_server: &mut bool) {
    if err.level() == WARNING {
        warnings().lock().unwrap_or_else(|e| e.into_inner()).push(err.message().to_string());
    }
    *output_to_server = false;
}

fn run(
    pid: i32,
    lookup: impl FnOnce(i32) -> Option<ProcNumber>,
    send: impl FnOnce(i32, ProcSignalReason, ProcNumber) -> Result<(), i32>,
) -> (bool, Vec<String>) {
    let _one_at_a_time = serialized();
    let prev = elog::set_emit_log_hook(Some(capture));
    warnings().lock().unwrap_or_else(|e| e.into_inner()).clear();
    let r = log_backend_memory_contexts(pid, lookup, send).unwrap();
    elog::set_emit_log_hook(prev);
    let w = warnings().lock().unwrap_or_else(|e| e.into_inner()).clone();
    (r, w)
}

#[test]
fn signal_failure_warning_carries_strerror_text() {
    const ESRCH: i32 = 3;
    let (found, w) = run(
        4242,
        |pid| (pid == 4242).then_some(7),
        |pid, reason, proc_number| {
            assert_eq!((pid, proc_number), (4242, 7));
            assert!(matches!(reason, PROCSIG_LOG_MEMORY_CONTEXT));
            Err(ESRCH)
        },
    );
    assert!(!found);
    assert_eq!(w, vec!["could not send signal to process 4242: No such process".to_string()]);
}

#[test]
fn unknown_pid_is_not_a_server_process_warning() {
    let (found, w) = run(
        4243,
        |_| None,
        |_, _, _| panic!("no signal is sent for an unknown pid"),
    );
    assert!(!found);
    assert_eq!(w, vec!["PID 4243 is not a PostgreSQL server process".to_string()]);
}

#[test]
fn signalled_process_returns_true_without_warning() {
    let (found, w) = run(4244, |_| Some(3), |_, _, _| Ok(()));
    assert!(found);
    assert!(w.is_empty(), "unexpected WARNINGs: {w:?}");
}

#[test]
fn context_id_table_is_visible_during_walk_and_destroyed_afterward() {
    mcxt_stats::init_seams();
    let parent = mcx::MemoryContext::new("context-id-test");
    for fail in [false, true] {
        let result = with_context_id_table(parent.mcx(), |_| {
            let forest = mcxt_stats::backend_context_forest();
            let root = forest.iter().find(|t| t.name == "context-id-test").unwrap();
            assert_eq!(root.children.len(), 1);
            let table = &root.children[0];
            assert_eq!(table.name, "dynahash");
            assert_eq!(table.ident.as_deref(), Some("pg_get_backend_memory_contexts"));
            assert_eq!(table.kind, "AllocSet");
            assert!(table.used > 0);
            if fail {
                Err(Box::new(PgError::error("test failure")))
            } else {
                Ok(())
            }
        });
        assert_eq!(result.is_err(), fail);
        let forest = mcxt_stats::backend_context_forest();
        let root = forest.iter().find(|t| t.name == "context-id-test").unwrap();
        assert!(root.children.is_empty());
    }
}
