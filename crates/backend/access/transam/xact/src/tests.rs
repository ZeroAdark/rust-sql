use crate::*;
use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    static LOG: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn log_push(v: usize) {
    LOG.with(|l| l.borrow_mut().push(v));
}

fn log_take() -> Vec<usize> {
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

fn install_test_seams() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        parallel_seams::is_parallel_worker::set(|| false);
        xlog_seams::xlog_logical_info_active::set(|| false);
        origin_seams::replorigin_session_origin::set(|| types_core::InvalidRepOriginId);
        origin_seams::replorigin_session_origin_lsn::set(|| 0);
        origin_seams::replorigin_session_origin_timestamp::set(|| 0);
        xloginsert_seams::xlog_insert_with_flags::set(|rmid, info, flags, fragments| {
            assert_eq!(rmid, RM_XACT_ID);
            assert_eq!(flags, XLOG_INCLUDE_ORIGIN);
            CAPTURED.with(|c| {
                let mut body = Vec::new();
                for f in fragments {
                    body.extend_from_slice(f);
                }
                *c.borrow_mut() = Some((info, body));
            });
            Ok(1234)
        });
    });
}

thread_local! {
    static CAPTURED: RefCell<Option<(u8, Vec<u8>)>> = const { RefCell::new(None) };
}

fn cb1(_e: XactEvent, arg: Datum) -> PgResult<()> {
    log_push(arg.as_usize());
    Ok(())
}

fn cb_self_unregister(e: XactEvent, arg: Datum) -> PgResult<()> {
    let _ = e;
    log_push(arg.as_usize());
    UnregisterXactCallback(cb_self_unregister, arg);
    Ok(())
}

fn cb_nested(_e: XactEvent, _arg: Datum) -> PgResult<()> {
    log_push(99);
    Ok(())
}

fn cb_registers_nested(_e: XactEvent, arg: Datum) -> PgResult<()> {
    log_push(arg.as_usize());
    RegisterXactCallback(cb_nested, Datum::from_usize(0));
    UnregisterXactCallback(cb_nested, Datum::from_usize(0));
    Ok(())
}

#[test]
fn callbacks_run_newest_first() {
    reset_xact_state_for_tests();
    for tag in [1usize, 2, 3] {
        RegisterXactCallback(cb1, Datum::from_usize(tag));
    }
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![3, 2, 1]);
    reset_xact_state_for_tests();
}

// Miri: fn-pointer identity is not stable under Miri (casts may mint distinct
// addresses), so UnregisterXactCallback's pointer-equality match can miss —
// same class as resowner's fn-pointer-identity test (no UB; pre-existing,
// verified against a tree with this batch's edits stashed).
#[cfg_attr(miri, ignore)]
#[test]
fn self_unregistration_is_safe() {
    reset_xact_state_for_tests();
    RegisterXactCallback(cb1, Datum::from_usize(1));
    RegisterXactCallback(cb_self_unregister, Datum::from_usize(2));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![2, 1, 1]);
    reset_xact_state_for_tests();
}

#[test]
fn mid_iteration_registration_not_invoked_this_round() {
    reset_xact_state_for_tests();
    RegisterXactCallback(cb_registers_nested, Datum::from_usize(1));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![1]);
    reset_xact_state_for_tests();
}

fn cb_unregisters_seven(_e: XactEvent, arg: Datum) -> PgResult<()> {
    log_push(arg.as_usize());
    UnregisterXactCallback(cb1, Datum::from_usize(7));
    Ok(())
}

fn sub_cb(
    _e: SubXactEvent,
    _my: SubTransactionId,
    _parent: SubTransactionId,
    _arg: Datum,
) -> PgResult<()> {
    Ok(())
}

// D4485: the walk trusts its copy while callbacks_gen is unchanged, so every Register / Unregister must move it, and a
// list that changed between commits is read fresh by the next walk.
#[cfg_attr(miri, ignore)]
#[test]
fn a_callback_registered_between_commits_fires_and_an_unregistered_one_does_not() {
    reset_xact_state_for_tests();
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(
        log_take(),
        Vec::<usize>::new(),
        "no callback registered: nothing runs"
    );
    RegisterXactCallback(cb1, Datum::from_usize(1));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![1]);
    RegisterXactCallback(cb1, Datum::from_usize(2));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_PRE_COMMIT).unwrap();
    assert_eq!(
        log_take(),
        vec![2, 1],
        "registered mid-session: fires at the next commit"
    );
    UnregisterXactCallback(cb1, Datum::from_usize(1));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![2], "unregistered: does not fire");
    UnregisterXactCallback(cb1, Datum::from_usize(2));
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), Vec::<usize>::new());
    reset_xact_state_for_tests();
}

#[cfg_attr(miri, ignore)]
#[test]
fn a_callback_unregistered_by_another_mid_walk_does_not_fire() {
    reset_xact_state_for_tests();
    RegisterXactCallback(cb1, Datum::from_usize(7));
    RegisterXactCallback(cb_unregisters_seven, Datum::from_usize(8));
    // newest first: 8 runs, unregisters 7 before its turn
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![8]);
    CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
    assert_eq!(log_take(), vec![8]);
    reset_xact_state_for_tests();
}

#[cfg_attr(miri, ignore)]
#[test]
fn every_register_and_unregister_moves_the_callbacks_generation() {
    reset_xact_state_for_tests();
    let generation = || xs(|s| s.callbacks_gen);
    let g0 = generation();
    RegisterXactCallback(cb1, Datum::from_usize(1));
    let g1 = generation();
    assert_ne!(g1, g0, "RegisterXactCallback");
    UnregisterXactCallback(cb1, Datum::from_usize(1));
    let g2 = generation();
    assert_ne!(g2, g1, "UnregisterXactCallback");
    RegisterSubXactCallback(sub_cb, Datum::from_usize(1));
    let g3 = generation();
    assert_ne!(g3, g2, "RegisterSubXactCallback");
    UnregisterSubXactCallback(sub_cb, Datum::from_usize(1));
    assert_ne!(generation(), g3, "UnregisterSubXactCallback");
    reset_xact_state_for_tests();
}

#[cfg_attr(miri, ignore)]
#[test]
fn an_exiting_threads_callback_lists_are_freed() {
    reset_xact_state_for_tests();
    RegisterXactCallback(cb1, Datum::from_usize(1));
    RegisterSubXactCallback(sub_cb, Datum::from_usize(1));
    let held = || xs(|s| (s.xact_callbacks.capacity(), s.subxact_callbacks.capacity()));
    assert!(held().0 > 0 && held().1 > 0, "each registration holds a list block");
    let g0 = xs(|s| s.callbacks_gen);
    release_thread_callbacks();
    assert_eq!(held(), (0, 0), "both lists' blocks freed");
    assert_ne!(xs(|s| s.callbacks_gen), g0, "a walk in progress sees the lists changed");
    reset_xact_state_for_tests();
}

#[test]
fn block_status_code_idle_by_default() {
    reset_xact_state_for_tests();
    assert_eq!(TransactionBlockStatusCode(), b'I');
    assert!(!IsTransactionBlock());
    assert!(!IsTransactionOrTransactionBlock());
    assert!(!IsAbortedTransactionBlockState());
    assert!(!IsTransactionState());
    reset_xact_state_for_tests();
}

#[test]
fn command_counter_increment_noop_when_unused() {
    reset_xact_state_for_tests();
    CommandCounterIncrement().unwrap();
    reset_xact_state_for_tests();
}

#[test]
fn get_current_command_id_marks_used() {
    install_test_seams();
    reset_xact_state_for_tests();
    assert_eq!(GetCurrentCommandId(false).unwrap(), FirstCommandId);
    assert_eq!(GetCurrentCommandId(true).unwrap(), FirstCommandId);
    assert!(xs(|s| s.command_id_used()));
    reset_xact_state_for_tests();
}

#[test]
fn isolation_predicates_track_level() {
    reset_xact_state_for_tests();
    SetXactIsoLevel(XACT_READ_COMMITTED);
    assert!(!IsolationUsesXactSnapshot());
    assert!(!IsolationIsSerializable());
    SetXactIsoLevel(XACT_REPEATABLE_READ);
    assert!(IsolationUsesXactSnapshot());
    SetXactIsoLevel(XACT_SERIALIZABLE);
    assert!(IsolationIsSerializable());
    reset_xact_state_for_tests();
}

#[test]
fn parallel_mode_nesting() {
    reset_xact_state_for_tests();
    assert!(!IsInParallelMode());
    EnterParallelMode();
    assert!(IsInParallelMode());
    EnterParallelMode();
    ExitParallelMode();
    assert!(IsInParallelMode());
    ExitParallelMode();
    assert!(!IsInParallelMode());
    reset_xact_state_for_tests();
}

#[test]
fn my_xact_flags_or_path() {
    reset_xact_state_for_tests();
    assert_eq!(MyXactFlags(), 0);
    OrMyXactFlags(XACT_FLAGS_ACCESSEDTEMPNAMESPACE);
    OrMyXactFlags(XACT_FLAGS_ACQUIREDACCESSEXCLUSIVELOCK);
    assert_eq!(
        MyXactFlags(),
        XACT_FLAGS_ACCESSEDTEMPNAMESPACE | XACT_FLAGS_ACQUIREDACCESSEXCLUSIVELOCK
    );
    reset_xact_state_for_tests();
}

#[test]
fn commit_record_round_trips_through_parser() {
    install_test_seams();
    reset_xact_state_for_tests();

    XactLogCommitRecord(777, &[], &[], &[], &[], false, 0, InvalidTransactionId, None).unwrap();
    let (info, body) = CAPTURED.with(|c| c.borrow_mut().take()).unwrap();
    assert_eq!(info, XLOG_XACT_COMMIT);
    assert_eq!(body.len(), 8);
    let parsed = parse_commit_record(info, &body).unwrap();
    assert_eq!(parsed.xact_time, 777);
    assert_eq!(parsed.xinfo, 0);
    assert!(parsed.subxacts.is_empty());

    let subs = [10u32, 11, 12];
    XactLogCommitRecord(
        888,
        &subs,
        &[],
        &[],
        &[],
        false,
        XACT_FLAGS_ACQUIREDACCESSEXCLUSIVELOCK,
        InvalidTransactionId,
        None,
    )
    .unwrap();
    let (info, body) = CAPTURED.with(|c| c.borrow_mut().take()).unwrap();
    assert_eq!(info, XLOG_XACT_COMMIT | XLOG_XACT_HAS_INFO);
    let parsed = parse_commit_record(info, &body).unwrap();
    assert_eq!(parsed.xact_time, 888);
    assert_eq!(parsed.subxacts, vec![10, 11, 12]);
    assert_ne!(parsed.xinfo & XACT_XINFO_HAS_AE_LOCKS, 0);
    assert_ne!(parsed.xinfo & XACT_XINFO_HAS_SUBXACTS, 0);
    reset_xact_state_for_tests();
}

#[test]
#[ignore = "child of commit_record_works_without_origin_seam"]
fn commit_record_no_origin_seam_child() {
    // Fresh process: origin seams deliberately NOT installed; the C default
    // (InvalidRepOriginId) must apply and the record must carry no origin.
    assert!(!origin_seams::replorigin_session_origin::is_installed());
    parallel_seams::is_parallel_worker::set(|| false);
    xlog_seams::xlog_logical_info_active::set(|| false);
    xloginsert_seams::xlog_insert_with_flags::set(|_, info, flags, fragments| {
        assert_eq!(flags, XLOG_INCLUDE_ORIGIN);
        CAPTURED.with(|c| {
            let mut body = Vec::new();
            for f in fragments {
                body.extend_from_slice(f);
            }
            *c.borrow_mut() = Some((info, body));
        });
        Ok(1234)
    });
    reset_xact_state_for_tests();

    XactLogCommitRecord(777, &[], &[], &[], &[], false, 0, InvalidTransactionId, None).unwrap();
    let (info, body) = CAPTURED.with(|c| c.borrow_mut().take()).unwrap();
    assert_eq!(info, XLOG_XACT_COMMIT);
    let parsed = parse_commit_record(info, &body).unwrap();
    assert_eq!(parsed.xinfo & XACT_XINFO_HAS_ORIGIN, 0);
}

#[test]
#[cfg_attr(miri, ignore)] // spawns a child process, unsupported under Miri
fn commit_record_works_without_origin_seam() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "tests::commit_record_no_origin_seam_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "child failed: {out:?}");
}

#[test]
fn abort_record_round_trips_through_parser() {
    install_test_seams();
    reset_xact_state_for_tests();
    XactLogAbortRecord(555, &[7], &[], &[], 0, InvalidTransactionId, None).unwrap();
    let (info, body) = CAPTURED.with(|c| c.borrow_mut().take()).unwrap();
    assert_eq!(info, XLOG_XACT_ABORT | XLOG_XACT_HAS_INFO);
    let parsed = parse_abort_record(info, &body).unwrap();
    assert_eq!(parsed.xact_time, 555);
    assert_eq!(parsed.subxacts, vec![7]);
    reset_xact_state_for_tests();
}

#[test]
fn parser_rejects_truncated_records() {
    assert!(parse_commit_record(XLOG_XACT_COMMIT, &[0u8; 4]).is_err());
    let mut body = Vec::new();
    body.extend_from_slice(&1i64.to_ne_bytes());
    body.extend_from_slice(&XACT_XINFO_HAS_SUBXACTS.to_ne_bytes());
    body.extend_from_slice(&100i32.to_ne_bytes()); // claims 100 subxacts, none present
    assert!(parse_commit_record(XLOG_XACT_COMMIT | XLOG_XACT_HAS_INFO, &body).is_err());
}

#[test]
fn savepoint_ops_rejected_outside_blocks() {
    install_test_seams();
    reset_xact_state_for_tests();
    assert!(DefineSavepoint(Some("sp")).is_err());
    assert!(ReleaseSavepoint("sp").is_err());
    assert!(RollbackToSavepoint("sp").is_err());
    reset_xact_state_for_tests();
}

#[test]
fn sub_transaction_id_accessors() {
    reset_xact_state_for_tests();
    assert_eq!(GetCurrentSubTransactionId(), InvalidSubTransactionId);
    assert!(!SubTransactionIsActive(5));
    assert_eq!(GetCurrentTransactionNestLevel(), 0);
    assert!(!IsSubTransaction());
    reset_xact_state_for_tests();
}

#[test]
fn transaction_id_is_current_rejects_special_xids() {
    reset_xact_state_for_tests();
    assert!(!TransactionIdIsCurrentTransactionId(InvalidTransactionId));
    assert!(!TransactionIdIsCurrentTransactionId(BootstrapTransactionId));
    assert!(!TransactionIdIsCurrentTransactionId(FrozenTransactionId));
    assert!(!TransactionIdIsCurrentTransactionId(12345));
    reset_xact_state_for_tests();
}

#[test]
fn save_restore_transaction_characteristics() {
    reset_xact_state_for_tests();
    SetXactIsoLevel(XACT_SERIALIZABLE);
    SetXactReadOnly(true);
    SetXactDeferrable(true);
    let saved = SaveTransactionCharacteristics();
    SetXactIsoLevel(XACT_READ_COMMITTED);
    SetXactReadOnly(false);
    SetXactDeferrable(false);
    RestoreTransactionCharacteristics(saved);
    assert_eq!(XactIsoLevel(), XACT_SERIALIZABLE);
    assert!(XactReadOnly());
    assert!(XactDeferrable());
    reset_xact_state_for_tests();
}

// audit fp-transam-xact-p1#1: session-end teardown frees the subtransaction stack.
#[test]
fn session_mem_teardown_frees_the_subtransaction_stack() {
    reset_xact_state_for_tests();
    xs(|s| {
        s.try_push_node(crate::state::TransactionNode::top()).unwrap();
        s.pop_node();
    });
    assert!(crate::state::subs_capacity_for_tests() > 0);
    session_mem_teardown();
    assert_eq!(crate::state::subs_capacity_for_tests(), 0);
    reset_xact_state_for_tests();
}

// wio P347: a deep savepoint stack's nodes are released at top-level end (C
// frees them with TopTransactionContext); a shallow one keeps its capacity.
#[test]
fn top_level_end_releases_deep_sub_stack() {
    reset_xact_state_for_tests();
    let fill = |n: usize| {
        xs(|s| {
            for _ in 0..n {
                s.try_push_node(crate::state::TransactionNode::top()).unwrap();
            }
            for _ in 0..n {
                s.pop_node();
            }
        })
    };
    fill(4);
    let small = crate::state::subs_capacity_for_tests();
    xs(|s| s.release_sub_stack());
    assert_eq!(crate::state::subs_capacity_for_tests(), small);
    fill(5000);
    assert!(crate::state::subs_capacity_for_tests() >= 5000);
    xs(|s| s.release_sub_stack());
    assert_eq!(crate::state::subs_capacity_for_tests(), 0);
    reset_xact_state_for_tests();
}

// xact.c:1651: a subxact context with a reset callback or child survives subcommit.
#[test]
fn subcommit_keeps_cur_transaction_context_with_callback_or_child() {
    use std::cell::Cell;
    use std::rc::Rc;
    let push_sub = || {
        xs(|s| s.try_push_node(crate::state::TransactionNode::top()).unwrap());
        AtSubStart_Memory();
    };
    let commit_sub = || {
        AtSubCommit_Memory().unwrap();
        xs(|s| s.pop_node());
    };

    reset_xact_state_for_tests();
    AtStart_Memory(crate::state::xs_ptr());
    push_sub();
    let fired = Rc::new(Cell::new(false));
    let f = fired.clone();
    xs(|s| {
        let ctx = s.node(1).cur_transaction_context.as_ref().expect("subxact context");
        assert_eq!(ctx.subtree_used(), 0);
        ctx.register_reset_callback(move || f.set(true));
    });
    commit_sub();
    assert!(!fired.get(), "callback fired at subcommit");
    assert_eq!(xs(|s| s.node(0).retained_child_contexts.len()), 1);
    AtCommit_Memory(crate::state::xs_ptr());
    assert!(fired.get(), "callback did not fire at the parent's commit");

    reset_xact_state_for_tests();
    AtStart_Memory(crate::state::xs_ptr());
    push_sub();
    let kid = xs(|s| s.node(1).cur_transaction_context.as_ref().expect("subxact context").new_child("kid"));
    commit_sub();
    assert_eq!(xs(|s| s.node(0).retained_child_contexts.len()), 1, "context with a live child dropped");
    drop(kid);
    AtCommit_Memory(crate::state::xs_ptr());
    assert_eq!(xs(|s| s.node(0).retained_child_contexts.len()), 0);

    reset_xact_state_for_tests();
    AtStart_Memory(crate::state::xs_ptr());
    push_sub();
    commit_sub();
    assert_eq!(xs(|s| s.node(0).retained_child_contexts.len()), 0);
    reset_xact_state_for_tests();
}

fn sub_cb_logs(
    _e: SubXactEvent,
    _my: SubTransactionId,
    _parent: SubTransactionId,
    arg: Datum,
) -> PgResult<()> {
    log_push(arg.as_usize());
    Ok(())
}

// D4780: a reused or retained thread runs its next session with the callbacks its registrants armed once per thread (the
// flag is thread-scoped, so nothing registers again): the session's end keeps them, and they fire in the next session.
#[cfg_attr(miri, ignore)]
#[test]
fn a_reused_threads_next_session_keeps_its_callbacks_and_they_fire() {
    std::thread::spawn(|| {
        install_test_seams();
        reset_xact_state_for_tests();
        thread_local! {
            static ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        let arm_once = || {
            if !ARMED.with(|a| a.replace(true)) {
                RegisterXactCallback(cb1, Datum::from_usize(7));
                RegisterSubXactCallback(sub_cb_logs, Datum::from_usize(8));
            }
        };
        // the first session on the thread arms the callbacks, they fire, and the session ends
        arm_once();
        CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
        CallSubXactCallbacks(SubXactEvent::SUBXACT_EVENT_COMMIT_SUB, 2, 1).unwrap();
        session_mem_teardown();
        assert_eq!(log_take(), vec![7, 8]);
        // the next session on the same thread: the flag holds, nothing registers again, and both still fire
        arm_once();
        CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
        CallSubXactCallbacks(SubXactEvent::SUBXACT_EVENT_COMMIT_SUB, 2, 1).unwrap();
        session_mem_teardown();
        assert_eq!(log_take(), vec![7, 8], "a session's end must keep the thread's callbacks");
        // the thread's real exit frees them
        thread_exit_teardown();
        CallXactCallbacks(xs_ptr(), XACT_EVENT_COMMIT).unwrap();
        assert_eq!(log_take(), Vec::<usize>::new());
    })
    .join()
    .unwrap();
}

// D4780: an exiting thread leaks none of its transaction state's heap: the callback lists (64 B blocks each once a
// callback registers), the unreported and parallel xid lists and a prepared gid are all freed by the exit teardown.
#[test]
fn thread_exit_teardown_frees_the_states_heap() {
    std::thread::spawn(|| {
        reset_xact_state_for_tests();
        RegisterXactCallback(cb1, Datum::from_usize(1));
        RegisterSubXactCallback(sub_cb, Datum::from_usize(2));
        xs(|s| {
            s.unreported_xids.push(5);
            s.parallel_current_xids.push(6);
            s.prepare_gid = Some(String::from("gid"));
        });
        assert!(crate::state::exit_heap_for_tests().iter().all(|&c| c > 0), "{:?}", crate::state::exit_heap_for_tests());
        session_mem_teardown();
        assert!(crate::state::exit_heap_for_tests()[..2].iter().all(|&c| c > 0), "a session's end keeps the callback lists");
        thread_exit_teardown();
        assert_eq!(crate::state::exit_heap_for_tests(), [0; 5], "the thread's exit frees every block");
    })
    .join()
    .unwrap();
}
