//! Session state + the transaction hook: the placement of the M3-D `WriterRegistry` (`pgrc_write::writer` doc:
//! "M3-H owns placement + the xact-callback registration"), purged at top-level commit and abort (the successful
//! statement already took its writer at publish; anything left is abandoned and logs its `RECLAIM`). Table
//! directories are created and dropped with the relation's smgr storage (`catalog_storage`). Exactly ONE
//! `thread_local!` block: the session-surface census counts declaration blocks.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use pgrc_write::writer::WriterRegistry;
use pgrc_write::wvfs::RealVfs;
use types_core::xact::XactEvent;
use types_error::PgResult;

thread_local! {
    // ONE block (TLS census +1): writer registry,
    // the once-per-backend hook-registration latch, the relcache
    // invalidation-callback latch (inval.rs's), and the per-statement
    // election-candidate cache (ingest.rs's — CodecCandidates is a pure
    // function of the relation schema; rebuilt per transaction, purged with
    // the writers). Co-located so the crate stays at exactly one census row.
    static WRITERS: RefCell<WriterRegistry> = RefCell::new(WriterRegistry::new());
    static HOOKS_REGISTERED: Cell<bool> = const { Cell::new(false) };
    static INVAL_REGISTERED: Cell<bool> = const { Cell::new(false) };
    static CANDIDATES: RefCell<BTreeMap<u64, pgrc_write::elect::CodecCandidates>> =
        const { RefCell::new(BTreeMap::new()) };
    // relfilenumber → folded footer facts (footer.rs — plan/ANALYZE-time
    // rows/bytes/NDV and zone columns, keyed inside the entry by the catalog
    // generation they were folded from, by identity (GENID / D1128: a
    // same-transaction TRUNCATE once reused the number); the resolve that
    // precedes every read makes a stale serve structurally impossible, so
    // entries live for the session like the old AM's part cache; they end
    // when its thread parks for the next session or exits, end_footer_facts).
    static FOOTER_FACTS: RefCell<BTreeMap<u64, crate::footer::SessionFacts>> =
        const { RefCell::new(BTreeMap::new()) };
    // front_claim.rs (D1159 c): this transaction's front writes (heap relid, subtransaction, registered epoch), and
    // committed registrations waiting for their invalidations to be sent (heap relid, epoch).
    static FRONT_SEEN: RefCell<Vec<(u32, u32, Option<u64>)>> = const { RefCell::new(Vec::new()) };
    static FRONT_PENDING: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    static FRONT_EXIT_REGISTERED: Cell<bool> = const { Cell::new(false) };
    // D339 (trunk 0a14b58f683, ported): (slot, gen) -> the command id that published it, for this transaction's
    // publishes only (cleared at transaction end with the writers): a cursor's snapshot taken before the COPY must not
    // see the generation.
    static PUBLISH_CIDS: RefCell<BTreeMap<(crate::catalog::RelSlot, u64), u32>> =
        const { RefCell::new(BTreeMap::new()) };
    // IXBULKSORT (ingest.rs): the cut parts of bulk writers that keep placements, until each of their rows was placed
    // or the transaction ends
    static PLACED: RefCell<Vec<crate::ingest::Placement>> = const { RefCell::new(Vec::new()) };
    // D4537 2: the bytes of PUBLISH_CIDS on this backend's session (`refit`).
    static CIDS_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    // D4537 T18b: the bytes of PLACED, FRONT_SEEN and FRONT_PENDING on this backend's session (`refit`).
    static PLACED_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    static SEEN_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    static PENDING_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    // D4537 T18c: the bytes of FOOTER_FACTS, CANDIDATES and WRITERS' registry on this backend's session (`refit`).
    static FACTS_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    static CANDS_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
    static WRITERS_CHARGE: RefCell<memledger::Charge> = const { RefCell::new(memledger::Charge::NONE) };
}

/// D4537 2 (R2): `charge` fitted to `bytes` on this backend's session (a thread with no backend: its own holder), and
/// closed at 0: every transaction's end empties the map, so no charge outlives its session.
fn refit(charge: &'static std::thread::LocalKey<RefCell<memledger::Charge>>, bytes: usize) {
    charge.with(|c| {
        let mut c = c.borrow_mut();
        if bytes == 0 {
            if c.committed() != 0 {
                *c = memledger::Charge::NONE;
            }
            return;
        }
        if c.committed() == 0 {
            let (o, release) = match memledger::charge::context().0 {
                o @ memledger::Owner::Cache(_) => (o, memledger::Release::Evict),
                o => (memledger::Owner::Session(o.acct()), memledger::Release::Session),
            };
            *c = memledger::Charge::open(o, release, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        }
        c.fit_committed(bytes as u64);
    });
}

/// D339: record that this transaction published generation `gen` of `slot` under command id `cid`.
pub fn record_publish_cid(slot: crate::catalog::RelSlot, gen: u64, cid: u32) {
    let n = PUBLISH_CIDS.with(|m| {
        let mut m = m.borrow_mut();
        m.insert((slot, gen), cid); // alloc: Cold charged CIDS_CHARGE (the session: one entry each)
        m.len()
    });
    refit(&CIDS_CHARGE, n * size_of::<((crate::catalog::RelSlot, u64), u32)>());
}

/// D339: the command id this transaction published generation `gen` of `slot` under, if it did.
pub fn publish_cid(slot: crate::catalog::RelSlot, gen: u64) -> Option<u32> {
    PUBLISH_CIDS.with(|m| m.borrow().get(&(slot, gen)).copied())
}

/// IXBULKSORT: the placements of this transaction's cut bulk parts (ingest.rs).
pub(crate) fn with_placed<R>(f: impl FnOnce(&mut Vec<crate::ingest::Placement>) -> R) -> R {
    let r = PLACED.with(|p| f(&mut p.borrow_mut()));
    let bytes = PLACED.with(|p| {
        let p = p.borrow();
        p.capacity() * size_of::<crate::ingest::Placement>() + p.iter().map(crate::ingest::Placement::heap_bytes).sum::<usize>()
    });
    refit(&PLACED_CHARGE, bytes);
    r
}

/// The once-per-backend latch of front_claim.rs's exit hook.
pub(crate) fn with_front_exit_latch<R>(f: impl FnOnce(&Cell<bool>) -> R) -> R {
    FRONT_EXIT_REGISTERED.with(f)
}

/// This transaction's front writes (front_claim.rs).
pub(crate) fn with_front_seen<R>(f: impl FnOnce(&mut Vec<(u32, u32, Option<u64>)>) -> R) -> R {
    let r = FRONT_SEEN.with(|s| f(&mut s.borrow_mut()));
    refit(&SEEN_CHARGE, FRONT_SEEN.with(|s| s.borrow().capacity()) * size_of::<(u32, u32, Option<u64>)>());
    r
}

/// Committed front registrations not yet promoted (front_claim.rs).
pub(crate) fn with_front_pending<R>(f: impl FnOnce(&mut Vec<(u32, u64)>) -> R) -> R {
    let r = FRONT_PENDING.with(|s| f(&mut s.borrow_mut()));
    refit(&PENDING_CHARGE, FRONT_PENDING.with(|s| s.borrow().capacity()) * size_of::<(u32, u64)>());
    r
}

/// Per-relfilenumber footer-fact cache accessor (footer.rs owns the fold
/// and the generation-keyed staleness discipline).
pub fn with_footer_facts<R>(
    f: impl FnOnce(&mut BTreeMap<u64, crate::footer::SessionFacts>) -> R,
) -> R {
    let closed = FACTS_CHARGE.with(|c| c.borrow().committed() == 0);
    let r = FOOTER_FACTS.with(|c| f(&mut c.borrow_mut()));
    const NODE: usize = 2 * size_of::<usize>();
    refit(&FACTS_CHARGE, FOOTER_FACTS.with(|c| c.borrow().values().map(|s| size_of::<(u64, crate::footer::SessionFacts)>() + NODE + s.heap_bytes()).sum()));
    if FOOTER_FACTS.with(|c| !c.borrow().is_empty()) {
        if closed && memledger::bound().is_backend() {
            mcx::register_session_cleanup(Box::new(end_footer_facts)); // alloc: Cold
        }
        mcx::register_thread_reuse_hook(end_footer_facts);
    }
    r
}

/// D4739 + D4756: a session's footer facts end in its teardown's State phase (registered as the charge opens; a fn item
/// boxes to no block), before the ledger's session end check (`charge::session_drained`); the thread-reuse hook (cpool's
/// park) is the backstop for a teardown that did not drain. The map goes and its charge closes, so a pooled thread
/// carries neither into its next session nor leaves their bytes on UNBOUND while it waits.
fn end_footer_facts() {
    let facts = FOOTER_FACTS.try_with(|c| c.try_borrow_mut().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default());
    drop(facts);
    if FACTS_CHARGE.try_with(|_| ()).is_ok() {
        refit(&FACTS_CHARGE, 0);
    }
}

/// Per-relfilenumber election-candidate cache accessor (ingest.rs builds
/// entries lazily from the relation schema; cleared with the writer purge
/// so schema lifetime is bounded by the transaction).
pub fn with_candidates<R>(
    f: impl FnOnce(&mut BTreeMap<u64, pgrc_write::elect::CodecCandidates>) -> R,
) -> R {
    let r = CANDIDATES.with(|c| f(&mut c.borrow_mut()));
    refit_candidates();
    r
}

fn refit_candidates() {
    const NODE: usize = 2 * size_of::<usize>();
    refit(&CANDS_CHARGE, CANDIDATES.with(|c| c.borrow().values().map(|v| size_of::<(u64, pgrc_write::elect::CodecCandidates)>() + NODE + v.heap_bytes()).sum()));
}

/// The inval-callback latch accessor (the cell lives in this crate's single
/// census-pinned block; the logic lives in [`crate::inval`]).
pub fn with_inval_latch<R>(f: impl FnOnce(&Cell<bool>) -> R) -> R {
    INVAL_REGISTERED.with(f)
}

/// Run `f` over this backend's writer registry.
pub fn with_writers<R>(f: impl FnOnce(&mut WriterRegistry) -> R) -> R {
    let r = WRITERS.with(|w| f(&mut w.borrow_mut()));
    refit(&WRITERS_CHARGE, WRITERS.with(|w| w.borrow().shell_bytes()));
    r
}

/// Register the xact callback once per backend thread (the tableam `CB_EOXACT_REGISTERED` precedent), before any
/// writer lands in this module.
pub fn ensure_session_hooks() {
    HOOKS_REGISTERED.with(|c| {
        if !c.get() {
            xact::RegisterXactCallback(pgrc_xact_callback, datum::Datum::null());
            c.set(true);
        }
    });
}

fn purge_writers() {
    CANDIDATES.with(|c| c.borrow_mut().clear());
    refit_candidates();
    PUBLISH_CIDS.with(|m| m.borrow_mut().clear());
    refit(&CIDS_CHARGE, 0);
    PLACED.with(|p| p.borrow_mut().clear());
    refit(&PLACED_CHARGE, 0);
    let mut vfs = RealVfs;
    WRITERS.with(|w| {
        if let Err(e) = w.borrow_mut().at_eoxact(&mut vfs) {
            let _ = elog::elog(
                types_error::WARNING,
                format!("pgrcolumnar: writer purge at transaction end failed: {e}"), // alloc: Cold
            );
        }
    });
    refit(&WRITERS_CHARGE, WRITERS.with(|w| w.borrow().shell_bytes()));
}

/// IXPERF F5b: whether this transaction left the hook nothing: no writer or tmp-name seq, no election candidate, no
/// front write seen, no committed registration pending (front_claim.rs) and no publish command id recorded (D339:
/// purge_writers is the only place that map is cleared). Then at_xact_event, purge_writers and the
/// pre-commit / pre-prepare checks are no-ops, on commit and abort alike; only the bulk reservations' end stays (its
/// own process-wide early-out).
fn quiet() -> bool {
    WRITERS.with(|w| w.borrow().is_idle())
        && CANDIDATES.with(|c| c.borrow().is_empty())
        && FRONT_SEEN.with(|s| s.borrow().is_empty())
        && FRONT_PENDING.with(|s| s.borrow().is_empty())
        && PLACED.with(|p| p.borrow().is_empty())
        && PUBLISH_CIDS.with(|m| m.borrow().is_empty())
}

fn pgrc_xact_callback(event: XactEvent, _arg: datum::Datum) -> PgResult<()> {
    use XactEvent::*;
    // D4809 (F1): the scan counts a served statement left unlanded (an error, a cancel) land at its own transaction's
    // end, not at a later idle flush: after a DROP TABLE that flush re-made the dead relid's shared pgstat entry, which
    // only DROP DATABASE retired
    if matches!(event, XACT_EVENT_COMMIT | XACT_EVENT_ABORT | XACT_EVENT_PREPARE) {
        crate::scanstats::gravity_statement_end();
    }
    if quiet() {
        match event {
            XACT_EVENT_COMMIT | XACT_EVENT_ABORT => crate::rowid_alloc::end_of_xact(false),
            XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_PARALLEL_ABORT | XACT_EVENT_PREPARE => crate::rowid_alloc::end_of_xact(true),
            _ => {}
        }
        return Ok(());
    }
    crate::front_claim::at_xact_event(match event {
        XACT_EVENT_COMMIT => Some(true),
        XACT_EVENT_ABORT | XACT_EVENT_PREPARE | XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_PARALLEL_ABORT => Some(false),
        _ => None,
    });
    match event {
        XACT_EVENT_COMMIT | XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_ABORT | XACT_EVENT_PARALLEL_ABORT => {
            purge_writers();
            // IX1 L3: this transaction's bulk reservations leave the in-flight table (a parallel worker's stay, in
            // flight while the leader's transaction runs)
            crate::rowid_alloc::end_of_xact(matches!(event, XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_PARALLEL_ABORT));
        }
        XACT_EVENT_PRE_PREPARE => {
            // 2PC refusal fires while erroring is still allowed.
            if WRITERS.with(|w| !w.borrow().is_empty()) {
                return Err(crate::unsupported("PREPARE TRANSACTION with a pgrcolumnar ingest open"));
            }
        }
        XACT_EVENT_PREPARE => {
            purge_writers();
            // IX1 L3: the prepared transaction's reservations stay in flight while it does
            crate::rowid_alloc::end_of_xact(true);
        }
        XACT_EVENT_PRE_COMMIT | XACT_EVENT_PARALLEL_PRE_COMMIT => {
            // Unpublished writers from aborted savepoints may be discarded; live ones must refuse.
            let live = WRITERS.with(|w| {
                w.borrow().registered_stamps().find(|(_, stamp)| {
                    xact::TransactionIdIsCurrentTransactionId(
                        types_core::xact::FullTransactionId::from_u64(stamp.fxid).xid(),
                    )
                })
            });
            if let Some((relfilenumber, _)) = live {
                return Err(unpublished_ingest_error(relfilenumber));
            }
        }
    }
    Ok(())
}

#[cold]
fn unpublished_ingest_error(relfilenumber: u64) -> Box<types_error::PgError> {
    Box::<types_error::PgError>::new( // alloc: Cold
        types_error::PgError::error(format!( // alloc: Cold
            "pgrcolumnar: buffered ingest for relfilenumber {relfilenumber} reached \
             commit without table_finish_bulk_insert; unpublished rows would be lost"
        ))
        .with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use memledger::charge::owner_row;

    static BIND: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The crate's lib tests that bind a backend run one at a time (D4739's witness reads UNBOUND's committed bytes).
    pub(crate) fn bind_serial() -> std::sync::MutexGuard<'static, ()> {
        BIND.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// UNBOUND's committed bytes in the owner cells a session's unbind moves its residue to (exit, orphan).
    fn unbound_committed() -> i64 {
        let mut n = 0;
        memledger::charge::for_each_owner(|v| {
            if v.acct == memledger::Acct::UNBOUND && (v.name == "exit" || v.name == "orphan") {
                n += v.committed;
            }
        });
        n
    }

    /// D4537 T18b: PLACED, FRONT_SEEN and FRONT_PENDING are the session's bytes, refit as they grow and closed when emptied.
    #[test]
    fn the_transactions_placements_and_front_lists_are_on_the_session() {
        std::thread::spawn(|| {
            let _serial = bind_serial();
            const PROCNO: u32 = 3_951;
            memledger::bind_backend(PROCNO, 9_000 + PROCNO);
            let session = memledger::Owner::Session(memledger::Acct::backend(PROCNO));
            let base = owner_row(session).committed;
            with_front_seen(|s| s.extend((0..4096).map(|i| (i, 1, None))));
            let grown = owner_row(session).committed;
            assert!(grown >= base + 4096 * size_of::<(u32, u32, Option<u64>)>() as i64, "FRONT_SEEN's list is the session's");
            with_front_pending(|p| p.extend((0..1024).map(|i| (i, 1u64))));
            assert!(owner_row(session).committed > grown, "FRONT_PENDING's list too");
            with_front_seen(std::mem::take);
            with_front_pending(std::mem::take);
            assert_eq!(owner_row(session).committed, base, "emptied lists give their bytes back");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D4537 T18c: the footer facts and the election candidates are the session's bytes, refit as they grow and closed when
    /// emptied (the candidates at the transaction's purge).
    #[test]
    fn the_footer_facts_and_the_candidates_are_on_the_session() {
        std::thread::spawn(|| {
            let _serial = bind_serial();
            const PROCNO: u32 = 3_952;
            memledger::bind_backend(PROCNO, 9_000 + PROCNO);
            let session = memledger::Owner::Session(memledger::Acct::backend(PROCNO));
            let base = owner_row(session).committed;
            let facts = crate::footer::FooterFacts { gen: 1, rows: 1, bytes: 1, granules: 1, parts: 1, ndv: Some(vec![0; 4096]), cols: None, zones: Vec::new() };
            with_footer_facts(|m| {
                m.insert(7, crate::footer::SessionFacts::detached(facts));
            });
            let grown = owner_row(session).committed;
            assert!(grown >= base + 4096 * 8, "the NDV list is the session's");
            let cands = pgrc_write::elect::CodecCandidates::new(pgrc_write::elect::ColumnPosture::default())
                .with_column(1, 0, pgrc_write::elect::ColumnPosture::default())
                .with_column(2, 0, pgrc_write::elect::ColumnPosture::default());
            with_candidates(|m| {
                m.insert(7, cands);
            });
            assert!(owner_row(session).committed > grown, "the election candidates too");
            purge_writers();
            let after_purge = owner_row(session).committed;
            with_footer_facts(std::mem::take);
            assert_eq!(owner_row(session).committed, base, "emptied caches give their bytes back (purge took the candidates; {after_purge} before the facts went)");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D4739: on one pooled thread, each session's footer facts are its own and end with it: the session unbinds (its
    /// charge moves to UNBOUND's exit cell), the thread runs the reuse hooks as cpool does when it parks, and the facts
    /// are gone with UNBOUND's committed bytes back where they were before the session ended; the next session's facts
    /// are its own again.
    #[test]
    fn footer_facts_end_with_their_session_on_a_pooled_thread() {
        std::thread::spawn(|| {
            let _serial = bind_serial();
            for procno in [3_953u32, 3_954] {
                memledger::bind_backend(procno, 9_000 + procno);
                let session = memledger::Owner::Session(memledger::Acct::backend(procno));
                let base = owner_row(session).committed;
                let facts = crate::footer::FooterFacts { gen: 1, rows: 1, bytes: 1, granules: 1, parts: 1, ndv: Some(vec![0; 4096]), cols: None, zones: Vec::new() };
                super::with_footer_facts(|m| {
                    m.insert(7, crate::footer::SessionFacts::detached(facts));
                });
                assert!(owner_row(session).committed >= base + 4096 * 8, "the facts are session {procno}'s");
                let unbound = unbound_committed();
                memledger::unbind_backend();
                mcx::run_thread_reuse_hooks();
                assert!(super::FOOTER_FACTS.with(|c| c.borrow().is_empty()), "the park's hook empties session {procno}'s facts");
                assert_eq!(unbound_committed(), unbound, "nothing of session {procno}'s facts stays on UNBOUND");
            }
        })
        .join()
        .unwrap();
    }

    /// D4756: the facts end in the session's teardown (its State-phase cleanup), before the ledger's end check reads it.
    #[test]
    fn footer_facts_end_in_the_teardown_before_the_session_end_check() {
        static REGISTERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn sink(_: mcx::SessionCleanupPhase, _: Box<dyn FnOnce()>) {
            REGISTERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        mcx::set_session_cleanup_sink(sink);
        std::thread::spawn(|| {
            let _serial = bind_serial();
            let residues = || memledger::owner_end::totals(memledger::owner_end::Source::Owner, memledger::OwnerClass::Session);
            for procno in [3_955u32, 3_956] {
                memledger::bind_backend(procno, 9_000 + procno);
                let registered = REGISTERED.load(std::sync::atomic::Ordering::Relaxed);
                for rel in [7, 8] {
                    let facts = crate::footer::FooterFacts { gen: 1, rows: 1, bytes: 1, granules: 1, parts: 1, ndv: Some(vec![0; 4096]), cols: None, zones: Vec::new() };
                    super::with_footer_facts(|m| {
                        m.insert(rel, crate::footer::SessionFacts::detached(facts));
                    });
                }
                assert_eq!(REGISTERED.load(std::sync::atomic::Ordering::Relaxed), registered + 1, "one cleanup for session {procno}");
                let before = residues();
                memledger::unbind_backend();
                super::end_footer_facts(); // the cleanup, as run_session_teardown runs it: after the unbind, before the end check
                memledger::charge::session_drained();
                assert!(super::FOOTER_FACTS.with(|c| c.borrow().is_empty()), "the teardown empties session {procno}'s facts");
                assert_eq!(residues(), before, "no residue of session {procno}'s facts at its end check");
            }
        })
        .join()
        .unwrap();
    }

    use super::*;

    /// IXPERF F5b: a backend with nothing registered is quiet; a front write seen, or a committed registration
    /// pending, makes the hook do its work until it is cleared.
    #[test]
    fn the_hook_is_quiet_only_with_nothing_registered() {
        assert!(quiet());
        with_front_seen(|s| s.push((1, 1, None)));
        assert!(!quiet(), "a front write seen");
        with_front_seen(|s| s.clear());
        with_front_pending(|p| p.push((1, 7)));
        assert!(!quiet(), "a registration pending");
        with_front_pending(|p| p.clear());
        record_publish_cid((1, 1, 1), 1, 0);
        assert!(!quiet(), "a publish's command id recorded");
        PUBLISH_CIDS.with(|m| m.borrow_mut().clear());
        assert!(quiet());
    }

    /// D339 with F5b: 1,000 transactions that each only publish (pgrc_flush's shape: no writer, no front
    /// write), committed and aborted in turn through the hook, leave the publish command id map empty at every
    /// transaction end: it never holds more than one transaction's publishes.
    #[test]
    fn idle_publishes_never_accumulate_publish_cids() {
        for gen in 1..=1000u64 {
            record_publish_cid((1, 1, 1), gen, 0);
            record_publish_cid((2, 2, 2), gen, 1);
            assert!(!quiet(), "transaction {gen} published");
            let end = if gen % 2 == 0 { XactEvent::XACT_EVENT_COMMIT } else { XactEvent::XACT_EVENT_ABORT };
            assert!(pgrc_xact_callback(end, datum::Datum::null()).is_ok());
            assert_eq!(PUBLISH_CIDS.with(|m| m.borrow().len()), 0, "map after transaction {gen}");
        }
        assert!(quiet());
    }
}
