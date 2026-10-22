use super::*;

#[test]
fn summary_allocations_use_wal_summarizer_context() {
    procsignal_seams::proc_signal_barrier_pending::set(|| true);
    procsignal_seams::process_proc_signal_barrier::set(|| {
        Err(Box::new(PgError::new(ERROR, "stop after summary allocation")))
    });
    let mut context = MemoryContext::new("Wal Summarizer");
    let err = SummarizeWAL(context.mcx(), 1, 0x1000000, true, 0, 0x1000000, &[]).unwrap_err();
    assert_eq!(err.message, "stop after summary allocation");
    assert!(context.stats().peak >= 8192);
    context.reset();
    assert_eq!(context.stats().used, 0);
}

#[test]
fn wal_summary_filename_roundtrip() {
    let ws = WalSummaryFile { tli: 1, start_lsn: 0x0000_0001_0428_0048, end_lsn: 0x0000_0001_0500_0000 };
    let name = format!(
        "{:08X}{:08X}{:08X}{:08X}{:08X}.summary",
        ws.tli,
        (ws.start_lsn >> 32) as u32,
        ws.start_lsn as u32,
        (ws.end_lsn >> 32) as u32,
        ws.end_lsn as u32
    );
    assert_eq!(name, "0000000100000001042800480000000105000000.summary");
    let (tli, start, end) = parse_wal_summary_filename(&name).unwrap();
    assert_eq!((tli, start, end), (ws.tli, ws.start_lsn, ws.end_lsn));
}

#[test]
fn wal_summary_filename_rejects_noise() {
    assert!(parse_wal_summary_filename("temp.summary").is_none());
    assert!(parse_wal_summary_filename("0000000100000001042800480000000105000000.partial").is_none());
    assert!(parse_wal_summary_filename("000000010000000104280048000000010500000g.summary").is_none());
    assert!(parse_wal_summary_filename("0000000100000001042800480000000105000000.summary.tmp").is_none());
}

#[test]
fn require_record_len_rejects_short_payloads() {
    // A short/empty untrusted WAL payload must produce a catchable
    // ERRCODE_DATA_CORRUPTED error rather than a slice-OOB panic.
    let err = require_record_len(&[], 4, "XLOG_CHECKPOINT_REDO", "test").err().unwrap();
    assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);

    let err = require_record_len(&[0u8; 3], 4, "XLOG_CHECKPOINT_REDO", "test").err().unwrap();
    assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);

    // ntablespaces-driven size: 8-byte header claiming one tablespace needs 12.
    let err = require_record_len(&[0u8; 8], 8 + 4, "XLOG_DBASE_DROP", "test").err().unwrap();
    assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);

    // Exactly enough, and more than enough, both pass.
    assert!(require_record_len(&[0u8; 4], 4, "XLOG_CHECKPOINT_REDO", "test").is_ok());
    assert!(require_record_len(&[0u8; 88], 4, "XLOG_CHECKPOINT_REDO", "test").is_ok());
}

#[test]
fn diff_ms_rounds_up_and_clamps() {
    assert_eq!(diff_ms(0, 0), 0);
    assert_eq!(diff_ms(10, 5), 0);
    assert_eq!(diff_ms(0, 1), 1);
    assert_eq!(diff_ms(0, 1000), 1);
    assert_eq!(diff_ms(0, 10_000_000), 10_000);
    // timestamp.c:1769-1772: overflow and >= INT_MAX ms clamp to INT_MAX.
    let int_max = i32::MAX as i64;
    assert_eq!(diff_ms(0, int_max * 1000 - 1000), int_max - 1);
    assert_eq!(diff_ms(0, int_max * 1000 - 999), int_max);
    assert_eq!(diff_ms(0, i64::MAX), int_max);
    assert_eq!(diff_ms(i64::MIN, i64::MAX), int_max);
}

// upstream 18f0de6b885a (18.6): Prevent walsummarizer from getting stuck at a timeline switch.
fn tle(tli: TimeLineID, begin: XLogRecPtr, end: XLogRecPtr) -> timeline_seams::TimeLineHistoryEntry {
    timeline_seams::TimeLineHistoryEntry { tli, begin, end }
}

#[test]
fn switch_point_lists_descendant_timelines_oldest_first() {
    let tles = [
        tle(4, 0x3000, InvalidXLogRecPtr),
        tle(3, 0x2000, 0x3000),
        tle(2, 0x1000, 0x2000),
        tle(1, InvalidXLogRecPtr, 0x1000),
    ];
    let (switch_lsn, descendants) = WalSummarizerSwitchPoint(1, &tles).unwrap();
    assert_eq!(switch_lsn, 0x1000);
    assert_eq!(descendants, vec![2, 3, 4]);
    let (switch_lsn, descendants) = WalSummarizerSwitchPoint(3, &tles).unwrap();
    assert_eq!(switch_lsn, 0x3000);
    assert_eq!(descendants, vec![4]);
}

#[test]
fn switch_point_rejects_timelines_without_a_successor() {
    let tles = [tle(2, 0x1000, InvalidXLogRecPtr), tle(1, InvalidXLogRecPtr, 0x1000)];
    let err = WalSummarizerSwitchPoint(7, &tles).err().unwrap();
    assert_eq!(err.message(), "requested timeline 7 is not in this server's history");
    // The newest timeline has no end yet; C reports that the same way.
    let err = WalSummarizerSwitchPoint(2, &tles).err().unwrap();
    assert_eq!(err.message(), "requested timeline 2 is not in this server's history");
    // First entry is the current TLI with an end recorded: no descendants.
    let odd = [tle(2, 0x1000, 0x2000), tle(1, InvalidXLogRecPtr, 0x1000)];
    let err = WalSummarizerSwitchPoint(2, &odd).err().unwrap();
    assert_eq!(err.message(), "cannot compute switch point for current TLI 2");
}

fn ws(tli: TimeLineID, start_lsn: XLogRecPtr, end_lsn: XLogRecPtr) -> WalSummaryFile {
    WalSummaryFile { tli, start_lsn, end_lsn }
}

#[test]
fn summaries_complete_empty_list() {
    // C: empty list -> false with missing_lsn = InvalidXLogRecPtr.
    assert_eq!(WalSummariesAreComplete(&[], 0x1000, 0x2000), (false, InvalidXLogRecPtr));
}

#[test]
fn summaries_complete_contiguous_chain() {
    let list = [ws(1, 0x1000, 0x1800), ws(1, 0x1800, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&list, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
    // Unsorted input: the function sorts a private copy.
    let rev = [ws(1, 0x1800, 0x2000), ws(1, 0x1000, 0x1800)];
    assert_eq!(WalSummariesAreComplete(&rev, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
}

#[test]
fn summaries_complete_reports_first_gap() {
    // Gap between 0x1800 and 0x1900: missing_lsn = end of covered prefix.
    let list = [ws(1, 0x1000, 0x1800), ws(1, 0x1900, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&list, 0x1000, 0x2000), (false, 0x1800));
    // Runs out before end_lsn without a gap.
    let short = [ws(1, 0x1000, 0x1800)];
    assert_eq!(WalSummariesAreComplete(&short, 0x1000, 0x2000), (false, 0x1800));
    // First summary starts after start_lsn: missing_lsn = start_lsn.
    let late = [ws(1, 0x1100, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&late, 0x1000, 0x2000), (false, 0x1000));
}

#[test]
fn summaries_complete_tolerates_overlap_and_containment() {
    // Overlapping ranges must still prove completeness (C comment: "intended
    // to be correct even in case of overlap").
    let list = [ws(1, 0x1000, 0x1a00), ws(1, 0x1400, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&list, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
    // A fully-contained range neither helps nor hurts.
    let contained = [ws(1, 0x1000, 0x1800), ws(1, 0x1200, 0x1600), ws(1, 0x1800, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&contained, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
    // Coverage extending past end_lsn counts.
    let over = [ws(1, 0x0800, 0x2800)];
    assert_eq!(WalSummariesAreComplete(&over, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
}

#[test]
fn summaries_complete_is_timeline_blind() {
    // Documented C behavior: TLIs are ignored; callers filter first.
    let list = [ws(1, 0x1000, 0x1800), ws(2, 0x1800, 0x2000)];
    assert_eq!(WalSummariesAreComplete(&list, 0x1000, 0x2000), (true, InvalidXLogRecPtr));
}

#[test]
fn filter_wal_summaries_bounds_are_inclusive() {
    let cx = MemoryContext::new("filter-wal-summaries-test");
    let mcx = cx.mcx();
    {
        let list = [
            ws(1, 0x1000, 0x1800),
            ws(1, 0x1800, 0x2000),
            ws(2, 0x2000, 0x2800),
            ws(1, 0x2800, 0x3000),
        ];
        // TLI filter.
        let r = FilterWalSummaries(mcx, &list, 2, InvalidXLogRecPtr, InvalidXLogRecPtr);
        assert_eq!(&r[..], &[ws(2, 0x2000, 0x2800)]);
        // tli == 0 means any timeline.
        let r = FilterWalSummaries(mcx, &list, 0, InvalidXLogRecPtr, InvalidXLogRecPtr);
        assert_eq!(r.len(), 4);
        // start_lsn bound is inclusive: a summary ENDING exactly at start_lsn
        // is kept (contrast GetWalSummaries' strict >=).
        let r = FilterWalSummaries(mcx, &list, 0, 0x1800, InvalidXLogRecPtr);
        assert_eq!(r.len(), 4);
        let r = FilterWalSummaries(mcx, &list, 0, 0x1801, InvalidXLogRecPtr);
        assert_eq!(&r[0], &ws(1, 0x1800, 0x2000));
        assert_eq!(r.len(), 3);
        // end_lsn bound is inclusive: a summary STARTING exactly at end_lsn is kept.
        let r = FilterWalSummaries(mcx, &list, 0, InvalidXLogRecPtr, 0x2800);
        assert_eq!(r.len(), 4);
        let r = FilterWalSummaries(mcx, &list, 0, InvalidXLogRecPtr, 0x27ff);
        assert_eq!(r.len(), 3);
    }
}

// GetLatestLSN's recovery arm: C takes max(GetWalRcvFlushRecPtr, replay) —
// flushed-but-unreplayed WAL on a streaming standby advances the summarizer.
#[test]
fn latest_lsn_prefers_further_ahead_flush() {
    // Flush ahead of replay: flush wins, with the flush TLI.
    assert_eq!(
        latest_lsn_from_flush_and_replay((0x2000, 2), (0x1000, 1)),
        (0x2000, 2)
    );
    // Replay ahead (or equal): replay wins, with the replay TLI.
    assert_eq!(
        latest_lsn_from_flush_and_replay((0x1000, 2), (0x3000, 1)),
        (0x3000, 1)
    );
    assert_eq!(
        latest_lsn_from_flush_and_replay((0x1000, 2), (0x1000, 1)),
        (0x1000, 1)
    );
    // No walreceiver: invalid flush LSN reduces to the replay position.
    assert_eq!(latest_lsn_from_flush_and_replay((0, 0), (0x1000, 1)), (0x1000, 1));
}

// ---------------------------------------------------------------------------
// wal_summary_contents_rows_in — the data half of the pg_wal_summary_contents
// SRF (walsummaryfuncs.c): per relation fork, limit_block row first (when
// valid), then modified blocks in blkreftable reader order (NOT sorted).
// ---------------------------------------------------------------------------

mod contents_rows {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Once;

    use super::super::*;
    use walsummarizer_seams::WalSummaryContentsRow;

    static SCRATCH_N: AtomicU32 = AtomicU32::new(0);

    fn fd_setup() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            guc_tables::init_seams();
            elog::init_seams();
            fd::init_seams();
            xact_seams::get_current_sub_transaction_id::set(|| 1);
            if !postgres_seams::check_for_interrupts::is_installed() {
                postgres_seams::check_for_interrupts::set(|| Ok(()));
            }
            aio_seams::pgaio_closing_fd::set(|_| {});
            if !waitevent_seams::pgstat_report_wait_start::is_installed() {
                waitevent_seams::pgstat_report_wait_start::set(|_| {});
            }
            if !waitevent_seams::pgstat_report_wait_end::is_installed() {
                waitevent_seams::pgstat_report_wait_end::set(|| {});
            }
        });
        fd::InitFileAccess();
    }

    /// A test's `<scratch>/summaries` directory: reads as its path (`Deref` to the `String`), and its scratch parent is
    /// removed with everything below it when dropped (the test's end, or a panic's unwind), so the suite leaves no
    /// pgrust_walsummary_srf_test_* in the temp dir.
    struct Summaries(String);

    impl std::ops::Deref for Summaries {
        type Target = String;
        fn deref(&self) -> &String {
            &self.0
        }
    }

    impl Drop for Summaries {
        fn drop(&mut self) {
            if let Some(parent) = std::path::Path::new(&self.0).parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }

    fn scratch_summaries_dir() -> Summaries {
        let n = SCRATCH_N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pgrust_walsummary_srf_test_{}_{n}/summaries",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Summaries(dir.to_str().unwrap().to_owned())
    }

    #[test]
    fn scratch_summaries_dirs_leave_nothing_behind() {
        let dir = scratch_summaries_dir();
        let parent = std::path::Path::new(&*dir).parent().unwrap().to_path_buf();
        std::fs::write(format!("{}/f", *dir), b"x").unwrap();
        drop(dir);
        assert!(!parent.exists(), "{} left behind", parent.display());
    }

    fn rl(spc: u32, db: u32, rel: u32) -> RelFileLocator {
        RelFileLocator { spcOid: spc, dbOid: db, relNumber: rel }
    }

    /// Serialize `tab` under the summary filename for (tli, start, end),
    /// byte-identical to what the WAL summarizer persists.
    fn write_summary_file(
        dir: &str,
        tli: TimeLineID,
        start_lsn: XLogRecPtr,
        end_lsn: XLogRecPtr,
        tab: &blkreftable::BlockRefTable<'_>,
    ) {
        let mut bytes: Vec<u8> = Vec::new();
        tab.write(|chunk: &[u8]| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        let name = format!(
            "{dir}/{:08X}{:08X}{:08X}{:08X}{:08X}.summary",
            tli,
            (start_lsn >> 32) as u32,
            start_lsn as u32,
            (end_lsn >> 32) as u32,
            end_lsn as u32
        );
        std::fs::write(name, bytes).unwrap();
    }

    #[test]
    fn contents_rows_match_c_emission_order() {
        fd_setup();
        let dir = scratch_summaries_dir();
        let cx = MemoryContext::new("contents-test");
        let mcx = cx.mcx();

        let mut tab = blkreftable::BlockRefTable::new(mcx);
        // Blocks marked out of order: reader order (array chunk insertion
        // order) is the SRF's row order, exactly like C — no sorting here.
        tab.mark_block_modified(rl(1663, 5, 16384), ForkNumber::MAIN_FORKNUM, 3).unwrap();
        tab.mark_block_modified(rl(1663, 5, 16384), ForkNumber::MAIN_FORKNUM, 1).unwrap();
        // A second fork with a valid limit block and no modified blocks.
        tab.set_limit_block(rl(1663, 5, 16385), ForkNumber::VISIBILITYMAP_FORKNUM, 12);
        write_summary_file(&dir, 1, 0x1000, 0x2000, &tab);

        let rows = wal_summary_contents_rows_in(&dir, 1, 0x1000, 0x2000).unwrap();
        let row = |rel: u32, fork: i16, blk: i64, limit: bool| WalSummaryContentsRow {
            relfilenode: rel,
            reltablespace: 1663,
            reldatabase: 5,
            relforknumber: fork,
            relblocknumber: blk,
            is_limit_block: limit,
        };
        assert_eq!(
            rows,
            vec![
                // rel 16384 MAIN: no limit row (InvalidBlockNumber), blocks
                // in insertion order 3 then 1.
                row(16384, 0, 3, false),
                row(16384, 0, 1, false),
                // rel 16385 VM: only the limit row.
                row(16385, 2, 12, true),
            ]
        );
    }

    #[test]
    fn contents_rows_missing_file_is_c_open_error() {
        fd_setup();
        let dir = scratch_summaries_dir();
        let err = wal_summary_contents_rows_in(&dir, 7, 0x7000, 0x8000).err().unwrap();
        assert!(
            err.message().starts_with("could not open file \""),
            "{}",
            err.message()
        );
    }
}

// ---------------------------------------------------------------------------
// audit-18.6 b203: C-exact error surfaces and lock discipline.
// ---------------------------------------------------------------------------

/// walsummarizer.c:1437 SummarizeSmgrRecord reads `xlrec->forkNum` as a plain
/// int; a WAL record carrying an unknown fork number must surface as a
/// catchable data-corruption error, never as an `expect` panic that takes
/// the summarizer (and, through the postmaster, the cluster) down.
#[test]
fn smgr_create_unknown_fork_is_an_error_not_a_panic() {
    let mut data = [0u8; 16];
    data[12..16].copy_from_slice(&4i32.to_ne_bytes());
    let err = smgr_create_forknum(&data)
        .err()
        .expect("fork number 4 is outside -1..=3 and must be refused");
    assert_eq!(err.sqlstate, types_error::ERRCODE_DATA_CORRUPTED);
    assert_eq!(
        err.message,
        "WAL record of type XLOG_SMGR_CREATE has invalid fork number 4"
    );

    // Every C fork number still decodes.
    for (raw, fork) in [
        (0, MAIN_FORKNUM),
        (1, FSM_FORKNUM),
        (2, VISIBILITYMAP_FORKNUM),
        (3, ForkNumber::INIT_FORKNUM),
    ] {
        data[12..16].copy_from_slice(&(raw as i32).to_ne_bytes());
        assert_eq!(smgr_create_forknum(&data).unwrap(), fork);
    }
}

/// walsummarizer.c:1036: when XLogReaderAllocate returns NULL, SummarizeWAL
/// raises ERRCODE_OUT_OF_MEMORY "out of memory" with the DETAIL "Failed while
/// allocating a WAL reading processor." — not mcx's generic request-size
/// detail.
#[test]
fn reader_allocation_failure_carries_c_detail() {
    let cx = MemoryContext::new("SummarizeWAL").with_limit(8);
    let err = allocate_summarizer_reader(cx.mcx(), 16 * 1024 * 1024)
        .err()
        .expect("an 8-byte context limit must fail the XLOG_BLCKSZ read buffer");
    assert_eq!(err.sqlstate, types_error::ERRCODE_OUT_OF_MEMORY);
    assert_eq!(err.message, "out of memory");
    assert_eq!(
        err.detail.as_deref(),
        Some("Failed while allocating a WAL reading processor.")
    );
}

/// walsummary.c:294 WriteWalSummary: a failed write reports `%m` (no byte
/// counts); a short write reports the counts, the offset, and HINT "Check
/// free disk space." (fd.c FileWriteV defaults errno to ENOSPC there).
#[test]
fn wal_summary_write_errors_match_walsummary_c() {
    const PATH: &str = "pg_wal/summaries/temp.summary";
    fn strerror(errnum: i32) -> String {
        // SAFETY: strerror returns a NUL-terminated string; copied out at once.
        unsafe { std::ffi::CStr::from_ptr(libc::strerror(errnum)) }
            .to_string_lossy()
            .into_owned()
    }
    let mut filepos: i64 = 7;

    let err = write_wal_summary(PATH, &mut filepos, &[1, 2, 3, 4], |_, _| {
        fd::set_errno(libc::EIO);
        Ok(-1)
    })
    .err()
    .expect("a negative write return is an error");
    assert_eq!(
        err.message,
        format!("could not write file \"{PATH}\": {}", strerror(libc::EIO))
    );
    assert_eq!(err.sqlstate, types_error::ERRCODE_IO_ERROR);
    assert_eq!(err.hint, None);
    assert_eq!(filepos, 7, "a failed write must not advance filepos");

    let err = write_wal_summary(PATH, &mut filepos, &[1, 2, 3, 4], |_, _| {
        fd::set_errno(libc::ENOSPC);
        Ok(3)
    })
    .err()
    .expect("a short write is an error");
    assert_eq!(
        err.message,
        format!("could not write file \"{PATH}\": wrote only 3 of 4 bytes at offset 7")
    );
    assert_eq!(err.sqlstate, types_error::ERRCODE_DISK_FULL);
    assert_eq!(err.hint.as_deref(), Some("Check free disk space."));
    assert_eq!(filepos, 7, "a short write must not advance filepos");

    write_wal_summary(PATH, &mut filepos, &[1, 2, 3, 4], |buf, pos| {
        assert_eq!((buf, pos), (&[1u8, 2, 3, 4][..], 7));
        Ok(4)
    })
    .unwrap();
    assert_eq!(filepos, 11);
}

/// walsummarizer.c:834 WalSummarizerShutdown takes WALSummarizerLock
/// exclusively around `summarizer_pgprocno = INVALID_PROC_NUMBER`, so a
/// concurrent GetWalSummarizerState (shared holder) never reads a procno
/// whose PGPROC slot the exiting summarizer is about to give up.
mod shutdown_lock {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    use std::sync::Once;
    use std::time::Duration;

    use super::super::*;
    use types_storage::storage::NUM_SPECIAL_WORKER_PROCS;

    fn setup() {
        static SETUP: Once = Once::new();
        SETUP.call_once(|| {
            s_lock_seams::perform_spin_delay::set(|_| std::thread::yield_now());
            s_lock_seams::finish_spin_delay::set(|_| {});
            shmem_seams::mul_size::set(|a, b| Ok(a * b));
            shmem_seams::add_size::set(|a, b| Ok(a + b));
            shmem_seams::shmem_alloc::set(|size| {
                Ok(Box::leak(vec![0u8; size].into_boxed_slice()).as_mut_ptr())
            });
            ipc_seams::on_shmem_exit::set(|_, _| {});
            // Real in-process semaphores: the contended LWLock wait parks on them.
            pg_sema::init_seams();
            if !waitevent_seams::pgstat_report_wait_start::is_installed() {
                if !waitevent_seams::pgstat_report_wait_start::is_installed() {
                    waitevent_seams::pgstat_report_wait_start::set(|_| {});
                }
                if !waitevent_seams::pgstat_report_wait_end::is_installed() {
                    waitevent_seams::pgstat_report_wait_end::set(|| {});
                }
            }
            if !postgres_seams::check_for_interrupts::is_installed() {
                postgres_seams::check_for_interrupts::set(|| Ok(()));
            }
            g::SetIsUnderPostmaster(false);
            g::SetMaxConnections(4);
            g::set_max_worker_processes(2);
            g::SetMaxBackends(4 + 3 + 2 + 2 + NUM_SPECIAL_WORKER_PROCS);
            lmgr_proc::InitProcGlobal(&lmgr_proc::ProcGlobalConfig {
                autovacuum_worker_slots: 3,
                max_wal_senders: 2,
                max_prepared_xacts: 2,
                fastpath_lock_groups_per_backend: 1,
            });
            lmgr_proc::init_seams();
            lwlock::CreateLWLocks(false).unwrap();
            WalSummarizerShmemInit();
        });
    }

    #[test]
    fn shutdown_resets_procno_under_walsummarizer_lock() {
        setup();
        // This thread plays pg_get_wal_summarizer_state(): GetWalSummarizerState
        // holds WALSummarizerLock shared while it reads summarizer_pgprocno.
        g::SetMyProcNumber(0);
        g::SetMyProcPid(7100);
        let d = ctl();
        d.summarizer_pgprocno.store(1, Relaxed);
        let lk = summarizer_lock();
        LWLockAcquire(lk, LW_SHARED, 0).unwrap();

        let summarizer = std::thread::spawn(|| {
            g::SetMyProcNumber(1);
            g::SetMyProcPid(7101);
            pg_sema_seams::pg_semaphore_reset::call(1);
            wal_summarizer_shutdown(0, 0);
        });

        // C-exact: the exiting summarizer queues on the lock (HAS_WAITERS) and
        // the procno stays intact until the reader releases. The unfixed port
        // clears the procno straight through the reader's shared hold.
        let mut queued = false;
        for _ in 0..20_000 {
            if lk.state.load(Acquire) & lwlock::LW_FLAG_HAS_WAITERS != 0 {
                queued = true;
                break;
            }
            if d.summarizer_pgprocno.load(Relaxed) == INVALID_PROC_NUMBER {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        let procno_while_reader_held = d.summarizer_pgprocno.load(Relaxed);
        LWLockRelease(lk).unwrap();
        summarizer.join().unwrap();

        assert!(
            queued && procno_while_reader_held == 1,
            "WalSummarizerShutdown must take WALSummarizerLock exclusively before \
             clearing summarizer_pgprocno (walsummarizer.c:834): queued={queued}, \
             procno seen under the reader's shared hold={procno_while_reader_held}"
        );
        assert_eq!(d.summarizer_pgprocno.load(Relaxed), INVALID_PROC_NUMBER);
        assert_eq!(lk.state.load(Relaxed) & lwlock::LW_LOCK_MASK, 0, "lock released");
    }
}

/// walsummary.c:249 compares the signed st_mtime directly: a pre-epoch
/// mtime is older than any cutoff and the summary is removed.
#[test]
fn pre_epoch_summary_mtime_is_expired() {
    assert!(summary_expired(-86_400, 1_700_000_000));
    assert!(summary_expired(1_699_999_999, 1_700_000_000));
    assert!(!summary_expired(1_700_000_000, 1_700_000_000));
    assert!(!summary_expired(i64::MAX, 1_700_000_000));
}

#[test]
fn shutdown_closes_open_wal_segment() {
    if std::env::var_os("WAL_SUMMARIZER_SHUTDOWN_TEST").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::shutdown_closes_open_wal_segment", "--nocapture"])
            .env("WAL_SUMMARIZER_SHUTDOWN_TEST", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    }
    file_seams::basic_open_file::set(|_, flags| fd::BasicOpenFile("/dev/null", flags).unwrap());
    procsignal_seams::proc_signal_barrier_pending::set(|| false);
    guc_tables::vars::summarize_wal.install(guc_tables::GucVarAccessors {
        get: || false,
        set: |_| {},
    });
    for mode in ["disabled", "shutdown", "normal"] {
        std::thread::spawn(move || {
            g::SetIsUnderPostmaster(true);
            interrupt::SetShutdownRequestPending(mode == "shutdown");
            let raw = Cell::new(-1);
            let exited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let cx = MemoryContext::new("summarizer shutdown test");
                let mut reader = allocate_summarizer_reader(cx.mcx(), 16 * 1024 * 1024).unwrap();
                let mut routine = SummarizerPageRead {
                    segment: guards::WalSegmentGuard(-1),
                    tli: 1,
                    historic: false,
                    read_upto: 0,
                    end_of_wal: false,
                    descendant_tlis: &[],
                };
                routine.segment_open(&mut reader.v, 1, &mut 1).unwrap();
                let first = reader.v.seg.ws_file;
                routine.segment_close(&mut reader.v);
                assert_eq!(reader.v.seg.ws_file, -1);
                assert_eq!(fd::pg_fstat(first, &mut fd::FileInfo::default()), -1);
                routine.segment_open(&mut reader.v, 2, &mut 1).unwrap();
                raw.set(reader.v.seg.ws_file);
                if mode == "normal" {
                    reader.XLogReaderFree(&mut routine);
                    assert_eq!(fd::pg_fstat(raw.get(), &mut fd::FileInfo::default()), -1);
                    raw.set(fd::BasicOpenFile("/dev/null", libc::O_RDONLY).unwrap());
                } else {
                    ProcessWalSummarizerInterrupts().unwrap();
                }
            }));
            if mode == "normal" {
                assert!(exited.is_ok());
            } else {
                assert_eq!(exited.unwrap_err().downcast_ref::<ipc::ProcExitThread>().unwrap().code, 0);
            }
            let result = fd::pg_fstat(raw.get(), &mut fd::FileInfo::default());
            if result == 0 {
                fd::pg_close(raw.get());
            }
            assert_eq!(result, if mode == "normal" { 0 } else { -1 }, "{mode}");
            if mode != "normal" {
                assert_eq!(fd::get_errno(), libc::EBADF);
            }
        }).join().unwrap();
    }
}
