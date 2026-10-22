//! Transaction-fact capabilities: the reader's [`CommitCheck`] (manifest
//! clog fence + snapshot visibility) and the writer's [`TxnProbe`]
//! (publish/recovery verdicts). Both are PASSED capabilities (spec §13.1);
//! this module is the ONE place the pgrc crates meet the transaction
//! layer.
//!
//! ## The visibility law (old-AM `rg_visible`, carried verbatim at
//! manifest-generation grain)
//!
//! A generation is visible iff its publisher xid `is_current`, OR
//! (`!XidInMVCCSnapshot` AND `did_commit`). Own-transaction publishes are
//! visible to later statements of the same transaction (COPY then SELECT);
//! concurrent uncommitted or snapshot-future publishers are not. The
//! epoch-qualified fxid in the manifest exists so a recycled xid can never
//! resurrect an aborted publish (#254); at this layer the xid-domain probes
//! honor that qualification by mapping the recorded fxid through the
//! recent-past window ([`recent_past_xid`]) before consulting the 32-bit
//! clog/snapshot layer — a fxid from an older epoch (its clog slot recycled
//! across wraparound) never adopts the recycled transaction's status
//! (`resolve_effective` also refuses structurally-broken chains regardless).
//!
//! ## Error discipline
//!
//! `CommitCheck::committed` returns `bool` (a frozen M3-F face), but the
//! clog/snapshot probes can error. Errors are RECORDED in the check and
//! must be re-raised by the caller after resolution ([`take_error`]); the
//! answer under a recorded error is the CONSERVATIVE arm (not visible /
//! in-progress) so a swallowed error can hide rows but never resurrect
//! aborted ones — and it is never actually swallowed because every caller
//! re-raises.

use std::cell::{Cell, RefCell};

use pgrc_read::CommitCheck;
use pgrc_write::publish::{TxnProbe, TxnVerdict};
use types_core::{FullTransactionId, TransactionId, TransactionIdIsNormal};
use types_error::PgError;
use types_snapshot::SnapshotData;

/// The pure visibility decision (unit-tested; the law above).
pub fn visible(is_current: bool, in_snapshot: bool, did_commit: bool) -> bool {
    is_current || (!in_snapshot && did_commit)
}

/// Map the recorded publisher [`FullTransactionId`] onto the bare 32-bit xid
/// the clog/snapshot layer probes — but ONLY when that xid still names *this*
/// transaction. A manifest carries an epoch-qualified 64-bit fxid precisely so
/// a recycled xid cannot resurrect an aborted publish (#254); the raw
/// `fxid as u32` truncation that this replaces threw the epoch away, so after
/// xid wraparound the 32-bit probes adjudicated whatever transaction now owns
/// the recycled clog slot.
///
/// C keeps clog authoritative only inside the recent-past window: a bare xid is
/// resolved to a full xid via `FullTransactionIdFromAllowableAt(nextFullXid,
/// xid)` (access/transam.h) — the unique full xid `<= nextFullXid` whose low 32
/// bits are `xid`, from the current epoch or the one before. If the recorded
/// fxid equals that reconstruction, the 32-bit clog/snapshot answer is about
/// the publisher and truncation is exact. If it does not, the recorded fxid
/// predates the window: its clog slot has been recycled to a newer transaction,
/// so the probe MUST NOT consult clog and must fall to the conservative arm.
///
/// (In a correctly-freezing cluster an effective generation's publisher is
/// always inside the window; a mismatch means aborted residue outlived its clog
/// — a corruption tripwire, surfaced as an error, never a recycled verdict.)
enum RecentPast {
    /// The recorded fxid is inside the recent-past window; probe this xid.
    Live(TransactionId),
    /// The recorded fxid predates the window (older epoch); its clog slot is
    /// recycled. Callers take the conservative arm and record this error.
    Aliased(Box<PgError>),
}

fn recent_past_xid(fxid: u64) -> RecentPast {
    recent_past_with(fxid, None).0
}

/// [`recent_past_xid`] given a next full xid read earlier, `held`. It is read again (and returned for the caller to
/// hold) when there is none or `fxid` is at or past it, so a publisher whose xid was assigned after `held` was read
/// is mapped against a next full xid past it, as a fresh read would map it (IXDMLPATH2 item 8 (A)). Below `held`,
/// the next full xid only moved forward since: the mapping differs from a fresh read's only for an fxid at the
/// window's far end (some 2^32 xids back), which a freezing cluster never has under a live generation (a held value
/// lives one [`SnapshotCommitCheck`], one catalog lookup).
fn recent_past_with(fxid: u64, held: Option<FullTransactionId>) -> (RecentPast, Option<FullTransactionId>) {
    let xid = fxid as TransactionId;
    // Special/bootstrap xids are epoch-independent (BootstrapXid, FrozenXid)
    // and are never recycled — probe them directly, matching
    // FullTransactionIdFromAllowableAt's non-normal fast path.
    if !TransactionIdIsNormal(xid) {
        return (RecentPast::Live(xid), held);
    }
    let next_full = match held {
        Some(n) if fxid < n.to_u64() => n,
        _ => match varsup_seams::read_next_full_transaction_id::call() {
            Ok(v) => v,
            Err(e) => return (RecentPast::Aliased(e), held),
        },
    };
    (recent_past_at(fxid, next_full), Some(next_full))
}

fn recent_past_at(fxid: u64, next_full: FullTransactionId) -> RecentPast {
    let xid = fxid as TransactionId;
    if full_xid_from_allowable_at(next_full, xid).to_u64() == fxid {
        RecentPast::Live(xid)
    } else {
        RecentPast::Aliased(Box::<PgError>::new( // alloc: Cold
            PgError::error(format!( // alloc: Cold
                "pgrcolumnar: publisher fxid {fxid} predates the clog-addressable \
                 recent-past window (next full xid {}); its clog slot has been \
                 recycled and cannot adjudicate this generation",
                next_full.to_u64()
            ))
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
        ))
    }
}

/// AUTOVAC FREEZE: the epoch-qualified form of a live 32-bit xid (a vacuum horizon): the unique full xid at or before
/// the next full xid with these low bits.
pub fn full_xid_at_now(xid: TransactionId) -> Result<u64, Box<PgError>> {
    let next_full = varsup_seams::read_next_full_transaction_id::call()?;
    Ok(full_xid_from_allowable_at(next_full, xid).to_u64())
}

/// `FullTransactionIdFromAllowableAt` (access/transam.h): the unique full xid
/// `<= next_full` whose low 32 bits are `xid`, from `next_full`'s epoch or the
/// one before. Non-normal xids are epoch-0. Caller guarantees normality here.
fn full_xid_from_allowable_at(next_full: FullTransactionId, xid: TransactionId) -> FullTransactionId {
    if !TransactionIdIsNormal(xid) {
        return FullTransactionId::from_epoch_and_xid(0, xid);
    }
    let mut epoch = next_full.epoch();
    if xid > next_full.xid() {
        // xid can't be from next_full's (not-yet-issued) epoch — it's the prior.
        epoch = epoch.saturating_sub(1);
    }
    FullTransactionId::from_epoch_and_xid(epoch, xid)
}

/// SEAL-OFFHEAD: the clog seams [`ClogTxnProbe`] calls are installed (a server; not a unit test's process).
pub fn clog_available() -> bool {
    varsup_seams::read_next_full_transaction_id::is_installed() && xact_seams::transaction_id_is_current_transaction_id::is_installed() && transam_seams::transaction_id_did_commit::is_installed() && procarray_seams::transaction_id_is_in_progress::is_installed()
}

/// Snapshot-scoped commit check for scans.
pub struct SnapshotCommitCheck<'a> {
    snapshot: Option<&'a SnapshotData<'a>>,
    error: RefCell<Option<Box<PgError>>>,
    /// IXDMLPATH2: the next full xid this check read last ([`recent_past_with`]): one read for every generation a
    /// lookup judges, own and committed alike, not one a probe.
    next_full: Cell<Option<FullTransactionId>>,
}

impl<'a> SnapshotCommitCheck<'a> {
    pub fn new(snapshot: Option<&'a SnapshotData<'a>>) -> SnapshotCommitCheck<'a> {
        SnapshotCommitCheck {
            snapshot,
            error: RefCell::new(None),
            next_full: Cell::new(None),
        }
    }

    /// [`recent_past_xid`] through this check's held next full xid.
    fn recent_past(&self, fxid: u64) -> RecentPast {
        let (out, held) = recent_past_with(fxid, self.next_full.get());
        self.next_full.set(held);
        out
    }

    /// Re-raise a probe error recorded during resolution (callers MUST call
    /// this after `resolve_effective`).
    pub fn take_error(&self) -> Result<(), Box<PgError>> {
        match self.error.borrow_mut().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn record(&self, e: Box<PgError>) {
        let mut slot = self.error.borrow_mut();
        if slot.is_none() {
            *slot = Some(e);
        }
    }

    /// Inherent face of [`CommitCheck::committed`] for callers outside
    /// the pgrc_read trait's dependency reach (the sqe engine-registry
    /// currency probe). Same visibility law, same recorded-error
    /// discipline — call [`Self::take_error`] after.
    pub fn committed_fxid(&self, fxid: u64) -> bool {
        CommitCheck::committed(self, fxid)
    }
}

impl CommitCheck for SnapshotCommitCheck<'_> {
    fn committed(&self, fxid: u64) -> bool {
        let xid = match self.recent_past(fxid) {
            RecentPast::Live(xid) => xid,
            // Recycled clog slot: conservative arm is "not visible" (a
            // swallowed error can hide rows but never resurrect aborted ones).
            RecentPast::Aliased(e) => {
                self.record(e);
                return false;
            }
        };
        if xact_seams::transaction_id_is_current_transaction_id::call(xid) {
            return true;
        }
        // PSLR: only an MVCC snapshot's xid list hides a publisher. SnapshotDirty, SnapshotSelf and
        // SnapshotNonVacuumable read the commit facts, as heap's satisfies functions judge xmin. A sentinel's xmax of 0
        // hid every generation, so the apply worker's dirty seq scan returned no sealed row.
        if let Some(snap) = self.snapshot.filter(|s| types_snapshot::IsMVCCSnapshot(s)) {
            match snapmgr::XidInMVCCSnapshot(xid, snap) {
                Ok(true) => return false,
                Ok(false) => {}
                Err(e) => {
                    self.record(e);
                    return false;
                }
            }
        }
        match transam_seams::transaction_id_did_commit::call(xid) {
            Ok(v) => v,
            Err(e) => {
                self.record(e);
                false
            }
        }
    }

    /// D339 (trunk 0a14b58f683, ported): a generation this transaction published is seen only by an MVCC snapshot taken
    /// after the publishing command (its recorded command id below the snapshot's curcid), as heap's cmin < curcid.
    /// Without a snapshot, or for a generation of another transaction or one this session did not record, the commit
    /// check decides.
    fn sees_command(&self, fxid: u64, key: (u32, u32, u64, u64)) -> bool {
        let Some(snap) = self.snapshot else { return true };
        if snap.snapshot_type != types_snapshot::SnapshotType::SNAPSHOT_MVCC {
            return true;
        }
        let Some(cid) = crate::session::publish_cid((key.0, key.1, key.2), key.3) else { return true };
        let xid = match self.recent_past(fxid) {
            RecentPast::Live(xid) => xid,
            RecentPast::Aliased(_) => return true,
        };
        if !xact_seams::transaction_id_is_current_transaction_id::call(xid) {
            return true;
        }
        cid < snap.curcid.get()
    }

    /// RR-VIEW: a publish of this transaction under an MVCC snapshot (`catalog::visible` composes the own view).
    fn own(&self, fxid: u64) -> bool {
        if !self.snapshot.is_some_and(|s| s.snapshot_type == types_snapshot::SnapshotType::SNAPSHOT_MVCC) {
            return false;
        }
        match self.recent_past(fxid) {
            RecentPast::Live(xid) => xact_seams::transaction_id_is_current_transaction_id::call(xid),
            RecentPast::Aliased(_) => false,
        }
    }
}

/// IX1 L3: every publisher that has not aborted (committed, in progress, prepared, the caller's own): through it
/// `catalog::visible` answers the newest generation that may yet commit, so an index cleanup sees the segment an
/// activation in progress lists. A publisher past the clog window, or one a probe errors on, counts.
pub struct UnabortedCheck {
    next_full: Cell<Option<FullTransactionId>>,
}

impl UnabortedCheck {
    pub fn new() -> UnabortedCheck {
        UnabortedCheck { next_full: Cell::new(None) }
    }
}

impl Default for UnabortedCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitCheck for UnabortedCheck {
    fn committed(&self, fxid: u64) -> bool {
        let (past, held) = recent_past_with(fxid, self.next_full.get());
        self.next_full.set(held);
        let RecentPast::Live(xid) = past else { return true };
        if xact_seams::transaction_id_is_current_transaction_id::call(xid) {
            return true;
        }
        if procarray_seams::transaction_id_is_in_progress::call(xid).unwrap_or(true) {
            return true;
        }
        transam_seams::transaction_id_did_commit::call(xid).unwrap_or(true)
    }
}

/// VERDICTORDER: the injection point inside [`ClogTxnProbe::verdict`] between its ProcArray and clog checks.
pub const VERDICT_POINT: &str = "pgrc-verdict-mid";

/// Clog probe for publish + recovery (no snapshot: publish effectiveness
/// and crash reclaim are commit facts, not snapshot facts).
pub struct ClogTxnProbe {
    error: RefCell<Option<Box<PgError>>>,
    recovery: Option<Prepared>,
    /// SEAL-OFFHEAD: a background maintenance publish ([`pgrc_write::publish::TxnProbe::stacks`]).
    stack: bool,
}

struct Prepared {
    xids: Vec<TransactionId>,
}

impl ClogTxnProbe {
    pub fn new() -> ClogTxnProbe {
        ClogTxnProbe {
            error: RefCell::new(None),
            recovery: None,
            stack: false,
        }
    }

    /// SEAL-OFFHEAD: the probe of a maintenance publish that chains on a foreign in-progress head instead of waiting.
    pub fn stacking() -> ClogTxnProbe {
        ClogTxnProbe { stack: true, ..ClogTxnProbe::new() }
    }

    /// End of recovery: no transaction survives but the prepared ones. `prepared` holds their xids and their
    /// committed subtransactions' (a manifest or temp records the (sub)transaction that wrote it); any other
    /// undecided xid is dead, however it compares with a prepared one (P5: a crashed COPY's temps outlived every
    /// recovery while an older transaction stayed prepared).
    pub fn recovery(prepared: Vec<TransactionId>) -> ClogTxnProbe {
        ClogTxnProbe {
            error: RefCell::new(None),
            recovery: Some(Prepared { xids: prepared }),
            stack: false,
        }
    }

    /// `xid` may still commit: the ProcArray lists it, or at the end of recovery it is prepared (P5).
    fn running(&self, xid: TransactionId) -> Result<bool, Box<PgError>> {
        match &self.recovery {
            Some(p) => Ok(p.xids.contains(&xid)),
            None => procarray_seams::transaction_id_is_in_progress::call(xid),
        }
    }

    pub fn take_error(&self) -> Result<(), Box<PgError>> {
        match self.error.borrow_mut().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn record(&self, e: Box<PgError>) {
        let mut slot = self.error.borrow_mut();
        if slot.is_none() {
            *slot = Some(e);
        }
    }
}

/// The xid a publisher waits on for the holder of a foreign in-progress generation (PUBLISH-WAIT): its fxid through
/// the recent-past window; one before the window is the DATA_CORRUPTED error [`TxnProbe::verdict`] records.
pub fn holder_xid(fxid: u64) -> Result<TransactionId, Box<PgError>> {
    match recent_past_xid(fxid) {
        RecentPast::Live(xid) => Ok(xid),
        RecentPast::Aliased(e) => Err(e),
    }
}

impl Default for ClogTxnProbe {
    fn default() -> Self {
        ClogTxnProbe::new()
    }
}

/// WR-TMP: a hot standby's restartpoint verdict on writer temps. The primary's end-of-recovery cleanup is not
/// logged, so a COPY cut by the primary's crash leaves its temps on a standby, and its xid never gets an abort
/// record. The standby removes a temp only when its writer provably finished without a commit:
///
/// * its xid precedes the horizon, the standby's oldest xid considered running (`ComputeXidHorizons`: the
///   replayed known-assigned xids, among them every transaction the primary still runs, a prepared one's included;
///   replay expires the others at the primary's running-xacts records and shutdown checkpoints, the one after a
///   crash too). A subtransaction's xid follows its parent's, so a live or prepared parent keeps it too;
/// * and clog does not say committed (replay sets a commit before the horizon passes it).
///
/// The horizon is taken once, before the directory walk: a temp that appears later belongs to a writer at or past it.
pub struct StandbyTempProbe {
    horizon: TransactionId,
}

impl StandbyTempProbe {
    /// The horizon now (`GetOldestTransactionIdConsideredRunning`). The caller has checked that hot standby is active:
    /// before it the known-assigned xids are not complete.
    pub fn take() -> StandbyTempProbe {
        StandbyTempProbe { horizon: procarray_seams::get_oldest_transaction_id_considered_running::call() }
    }

    /// Whether the temp named by the writer `fxid` may go. An error (a clog probe's, or an fxid older than the
    /// clog-addressable window) keeps it.
    pub fn removable(&self, fxid: u64) -> Result<bool, Box<PgError>> {
        standby_temp_removable(fxid, self.horizon)
    }
}

fn standby_temp_removable(fxid: u64, horizon: TransactionId) -> Result<bool, Box<PgError>> {
    if !TransactionIdIsNormal(horizon) {
        return Ok(false);
    }
    let xid = match recent_past_xid(fxid) {
        RecentPast::Live(xid) => xid,
        RecentPast::Aliased(e) => return Err(e),
    };
    if !TransactionIdIsNormal(xid) {
        return Ok(false);
    }
    // In full xids: the horizon never passes the next xid, so it is inside the recent-past window.
    let next_full = varsup_seams::read_next_full_transaction_id::call()?;
    if fxid >= full_xid_from_allowable_at(next_full, horizon).to_u64() {
        return Ok(false);
    }
    Ok(!transam_seams::transaction_id_did_commit::call(xid)?)
}

impl TxnProbe for ClogTxnProbe {
    /// The current transaction tree (`TransactionIdIsCurrentTransactionId`, the reader's `is_current`): the publish
    /// stamps its subtransaction's fxid, and a savepoint's publish must chain on its parent's generation. A fxid
    /// before the recent-past window is never own (`verdict` recorded its error).
    fn is_own(&self, publisher: u64, _own: u64) -> bool {
        self.recovery.is_none()
            && matches!(recent_past_xid(publisher), RecentPast::Live(xid) if xact_seams::transaction_id_is_current_transaction_id::call(xid))
    }

    /// GENID: the process catalog's record of the numbers `dir` already used ([`crate::catalog::used_numbers`]).
    fn used_numbers(&self, dir: &str) -> (u64, u32) {
        crate::catalog::used_numbers(dir)
    }

    fn stacks(&self) -> bool {
        self.stack
    }

    fn verdict(&self, fxid: u64) -> TxnVerdict {
        let past = if TransactionIdIsNormal(fxid as TransactionId) {
            match varsup_seams::read_next_full_transaction_id::call() {
                // D2876 (trunk 9afaaac28b5, ported): redo moves the next full xid past every commit or prepare record
                // it replays, and every xid assigned since is below it, so a publisher at or past it crashed before any
                // WAL named its xid: dead, though procarray (latestCompletedXid below it) calls it running, and though
                // its manifest is on disk (a PUBLISH with an xid is applied before its WAL is flushed: the commit
                // record flushes it). The end-of-recovery cleanup (pgrc_wal pgrc_cleanup, every StartupXLOG, before the
                // first xid is assigned) removes it and drops it from a live catalog, so the reissue of that xid finds
                // nothing of it. Before this, an fxid past the next full xid mapped outside the recent-past window:
                // InProgress with an error, the directory's cleanup failed and the dead generation was kept.
                Ok(next_full) if fxid >= next_full.to_u64() => return TxnVerdict::Aborted,
                Ok(next_full) => recent_past_at(fxid, next_full),
                Err(e) => RecentPast::Aliased(e),
            }
        } else {
            RecentPast::Live(fxid as TransactionId)
        };
        let xid = match past {
            RecentPast::Live(xid) => xid,
            // Recycled clog slot: conservative arm is InProgress (keeps temps
            // — dead-band, never data loss). Recording the error makes
            // the caller re-raise instead of either resurrecting the
            // aborted generation as Committed or reclaiming an old-committed one.
            RecentPast::Aliased(e) => {
                self.record(e);
                return TxnVerdict::InProgress;
            }
        };
        // Own transaction: in progress (recovery must never reclaim the
        // current transaction's temps; publish walks past its own gens via
        // the CommitCheck arm, not this one).
        if self.recovery.is_none() && xact_seams::transaction_id_is_current_transaction_id::call(xid) {
            return TxnVerdict::InProgress;
        }
        // A committer sets clog before it leaves the ProcArray: running first, then clog (heapam_visibility.c's order).
        // Out of the ProcArray and not committed = aborted or crashed. An error stays InProgress (keeps temps).
        match self.running(xid) {
            Ok(true) => return TxnVerdict::InProgress,
            Ok(false) => {}
            Err(e) => {
                self.record(e);
                return TxnVerdict::InProgress;
            }
        }
        // VERDICTORDER test hook (scripts/pgrc-verdictorder-e2e.sh, forced): between the two checks
        if let Err(e) = injection_point::injection_point(VERDICT_POINT) {
            self.record(e);
            return TxnVerdict::InProgress;
        }
        match transam_seams::transaction_id_did_commit::call(xid) {
            Ok(true) => TxnVerdict::Committed,
            Ok(false) => TxnVerdict::Aborted,
            Err(e) => {
                self.record(e);
                TxnVerdict::InProgress
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, Once};

    // Seams are process-global install-once fn pointers, so the transaction
    // layer is scripted once and each test drives it through these statics.
    // NEXT_FULL is the cluster's next full xid; the recycled slot COMMITTED_XID
    // reads committed. TEST_LOCK serialises the shared-state tests.
    static NEXT_FULL: AtomicU64 = AtomicU64::new(0);
    static COMMITTED_XID: AtomicU64 = AtomicU64::new(0);
    // D2876: an xid procarray lists as running (0 = none)
    static IN_PROGRESS_XID: AtomicU64 = AtomicU64::new(0);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    // One test's failed assertion fails that test alone: the lock it held is taken again, not poisoned for the rest.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    // IXDMLPATH2: reads of the next full xid (XidGenLock in a server)
    static NEXT_FULL_READS: AtomicU64 = AtomicU64::new(0);
    // VERDICTORDER: RACE_XID commits while it is probed. Each seam call on it is one tick: clog says committed from
    // tick RACE_CLOG on and the ProcArray lists it before tick RACE_LEFT >= RACE_CLOG (RecordTransactionCommit, then
    // ProcArrayEndTransaction). 0 = no race.
    static RACE_XID: AtomicU64 = AtomicU64::new(0);
    static RACE_TICK: AtomicU64 = AtomicU64::new(0);
    static RACE_CLOG: AtomicU64 = AtomicU64::new(0);
    static RACE_LEFT: AtomicU64 = AtomicU64::new(0);

    fn race_tick(xid: TransactionId) -> Option<u64> {
        (xid as u64 == RACE_XID.load(Ordering::Relaxed)).then(|| RACE_TICK.fetch_add(1, Ordering::Relaxed))
    }

    fn install_seams() {
        static SETUP: Once = Once::new();
        SETUP.call_once(|| {
            varsup_seams::read_next_full_transaction_id::set(|| {
                NEXT_FULL_READS.fetch_add(1, Ordering::Relaxed);
                Ok(FullTransactionId::from_u64(NEXT_FULL.load(Ordering::Relaxed)))
            });
            xact_seams::transaction_id_is_current_transaction_id::set(|_| false);
            transam_seams::transaction_id_did_commit::set(|xid| {
                Ok(match race_tick(xid) {
                    Some(t) => t >= RACE_CLOG.load(Ordering::Relaxed),
                    None => xid as u64 == COMMITTED_XID.load(Ordering::Relaxed),
                })
            });
            transam_seams::transaction_id_did_abort::set(|xid| {
                race_tick(xid);
                Ok(false)
            });
            procarray_seams::transaction_id_is_in_progress::set(|xid| {
                let racing = race_tick(xid).is_some_and(|t| t < RACE_LEFT.load(Ordering::Relaxed));
                Ok(racing || (xid != 0 && xid as u64 == IN_PROGRESS_XID.load(Ordering::Relaxed)))
            });
        });
    }

    // full_xid_from_allowable_at mirrors access/transam.h exactly (pure; no
    // seams, so it needs no lock).
    #[test]
    fn allowable_at_current_and_prior_epoch() {
        let next = FullTransactionId::from_epoch_and_xid(3, 100);
        // xid <= next.xid() ⇒ current epoch.
        assert_eq!(full_xid_from_allowable_at(next, 50).to_u64(), (3u64 << 32) | 50);
        // xid > next.xid() ⇒ it must be from the prior epoch.
        assert_eq!(full_xid_from_allowable_at(next, 200).to_u64(), (2u64 << 32) | 200);
        // Non-normal xids are epoch-0.
        assert_eq!(full_xid_from_allowable_at(next, 2).to_u64(), 2);
    }

    // The security property: an aborted publisher from an OLDER epoch whose
    // 32-bit xid has been recycled to a now-COMMITTED transaction must not be
    // adjudicated Committed. The old `fxid as u32` truncation returned
    // Committed here (resurrection); the recent-past guard returns InProgress
    // and records a corruption error instead.
    #[test]
    fn verdict_does_not_resurrect_recycled_aborted_publisher() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store((2u64 << 32) | 100, Ordering::Relaxed);
        COMMITTED_XID.store(50, Ordering::Relaxed); // recycled slot commits

        let probe = ClogTxnProbe::new();
        // Publisher recorded in epoch 1 (aborted), xid 50 — now recycled.
        let aborted_old = (1u64 << 32) | 50;
        assert_eq!(probe.verdict(aborted_old), TxnVerdict::InProgress);
        assert!(probe.take_error().is_err(), "aliased fxid must surface an error");
    }

    // D2876 (trunk 9afaaac28b5): after a crash the next full xid is the first xid no WAL named, and procarray
    // (latestCompletedXid below it) calls that xid running. A publisher at or past it is dead with no error, also once
    // the xid is reissued and commits, and at the end of recovery too; one below it is probed as before.
    #[test]
    fn verdict_publisher_at_or_past_the_next_full_xid_is_dead() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store((2u64 << 32) | 100, Ordering::Relaxed);
        IN_PROGRESS_XID.store(100, Ordering::Relaxed);
        COMMITTED_XID.store(0, Ordering::Relaxed);

        let probe = ClogTxnProbe::new();
        assert_eq!(probe.verdict((2u64 << 32) | 100), TxnVerdict::Aborted);
        assert_eq!(probe.verdict((2u64 << 32) | 150), TxnVerdict::Aborted);
        assert_eq!(probe.verdict((3u64 << 32) | 10), TxnVerdict::Aborted);
        COMMITTED_XID.store(100, Ordering::Relaxed);
        assert_eq!(probe.verdict((2u64 << 32) | 100), TxnVerdict::Aborted);
        assert!(probe.take_error().is_ok());
        let recovery = ClogTxnProbe::recovery(Vec::new());
        assert_eq!(recovery.verdict((2u64 << 32) | 150), TxnVerdict::Aborted, "the end-of-recovery cleanup's verdict");
        assert!(recovery.take_error().is_ok(), "no error: the directory's cleanup goes on");

        IN_PROGRESS_XID.store(99, Ordering::Relaxed);
        assert_eq!(probe.verdict((2u64 << 32) | 99), TxnVerdict::InProgress);
        // it commits: clog set, then out of the ProcArray (the probe asks the ProcArray first)
        COMMITTED_XID.store(99, Ordering::Relaxed);
        IN_PROGRESS_XID.store(0, Ordering::Relaxed);
        assert_eq!(probe.verdict((2u64 << 32) | 99), TxnVerdict::Committed);
        assert!(probe.take_error().is_ok());
        IN_PROGRESS_XID.store(0, Ordering::Relaxed);
        COMMITTED_XID.store(0, Ordering::Relaxed);
    }

    // A recent-epoch publisher is probed exactly as before (no regression):
    // its fxid maps back to itself through the recent-past window.
    #[test]
    fn verdict_probes_current_epoch_publisher_normally() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store((2u64 << 32) | 100, Ordering::Relaxed);
        COMMITTED_XID.store(50, Ordering::Relaxed);

        let probe = ClogTxnProbe::new();
        let committed_recent = (2u64 << 32) | 50;
        assert_eq!(probe.verdict(committed_recent), TxnVerdict::Committed);
        assert!(probe.take_error().is_ok());
    }

    // End of recovery: an undecided xid is dead unless a prepared transaction
    // or one of its subtransactions carries it (the caller lists both).
    #[test]
    fn recovery_verdicts_keep_prepared_transactions_only() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store(1000, Ordering::Relaxed);
        COMMITTED_XID.store(450, Ordering::Relaxed);

        // 500 and 700 prepared, 510 a subtransaction of 500
        let probe = ClogTxnProbe::recovery(vec![500, 510, 700]);
        assert_eq!(probe.verdict(400), TxnVerdict::Aborted);
        assert_eq!(probe.verdict(450), TxnVerdict::Committed);
        assert_eq!(probe.verdict(500), TxnVerdict::InProgress);
        assert_eq!(probe.verdict(510), TxnVerdict::InProgress);
        assert_eq!(probe.verdict(600), TxnVerdict::Aborted, "P5: a crashed transaction after a prepared one is dead");
        assert_eq!(probe.verdict(800), TxnVerdict::Aborted);
        let none = ClogTxnProbe::recovery(Vec::new());
        assert_eq!(none.verdict(600), TxnVerdict::Aborted);
        assert!(probe.take_error().is_ok() && none.take_error().is_ok());
    }

    // WR-TMP: a hot standby removes a temp only below the horizon and only when clog does not say committed; at or
    // past it (a live writer, a prepared transaction and its subtransactions) it stays, in full xids across an
    // epoch boundary; an fxid older than the clog window is an error (kept).
    #[test]
    fn a_standby_removes_only_uncommitted_temps_below_the_horizon() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store(1000, Ordering::Relaxed);
        COMMITTED_XID.store(450, Ordering::Relaxed);
        // 500 prepared (or live), 510 its subtransaction: the horizon is 500
        assert!(standby_temp_removable(400, 500).unwrap(), "a crashed or aborted writer below the horizon");
        assert!(!standby_temp_removable(450, 500).unwrap(), "a committed writer keeps its temps");
        assert!(!standby_temp_removable(500, 500).unwrap());
        assert!(!standby_temp_removable(510, 500).unwrap());
        assert!(!standby_temp_removable(900, 500).unwrap());
        assert!(!standby_temp_removable(400, 0).unwrap(), "no horizon, nothing goes");
        assert!(!standby_temp_removable(2, 500).unwrap(), "a special xid is no writer's");
        // the horizon in the epoch before next's: 32-bit order is not full-xid order
        NEXT_FULL.store((2u64 << 32) | 100, Ordering::Relaxed);
        COMMITTED_XID.store(0, Ordering::Relaxed);
        let h = 4_000_000_000u32;
        assert!(standby_temp_removable((1u64 << 32) | 3_999_999_000, h).unwrap());
        assert!(!standby_temp_removable((1u64 << 32) | 4_000_000_500, h).unwrap());
        assert!(!standby_temp_removable((2u64 << 32) | 50, h).unwrap(), "past the horizon in the next epoch");
        assert!(standby_temp_removable(3_999_999_000, h).is_err(), "epoch 0: older than the clog window");
    }

    // CommitCheck (reader path): a recycled old-epoch aborted publisher must
    // not be reported visible, and the error is recorded for re-raise.
    #[test]
    fn committed_does_not_resurrect_recycled_aborted_publisher() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store((2u64 << 32) | 100, Ordering::Relaxed);
        COMMITTED_XID.store(50, Ordering::Relaxed);

        let check = SnapshotCommitCheck::new(None);
        let aborted_old = (1u64 << 32) | 50;
        assert!(!check.committed(aborted_old));
        assert!(check.take_error().is_err());
    }

    // PSLR: a non-MVCC snapshot (the apply worker's SnapshotDirty sentinel, xmin = xmax = 0) judges a publisher by the
    // commit facts, as no snapshot does; an MVCC snapshot still hides a publisher at or past its xmax.
    #[test]
    fn a_dirty_snapshot_sees_the_committed_publisher() {
        let _g = serial();
        install_seams();
        NEXT_FULL.store((2u64 << 32) | 1_000, Ordering::Relaxed);
        COMMITTED_XID.store(700, Ordering::Relaxed);
        let ctx = mcx::MemoryContext::new("probe dirty test");
        let committed = (2u64 << 32) | 700;
        let aborted = (2u64 << 32) | 701;
        for t in [types_snapshot::SnapshotType::SNAPSHOT_DIRTY, types_snapshot::SnapshotType::SNAPSHOT_SELF, types_snapshot::SnapshotType::SNAPSHOT_NON_VACUUMABLE] {
            let snap = SnapshotData::sentinel(ctx.mcx(), t);
            let check = SnapshotCommitCheck::new(Some(&snap));
            assert!(check.committed(committed), "{t:?}");
            assert!(!check.committed(aborted), "{t:?}");
            check.take_error().unwrap();
        }
        let mut mvcc = SnapshotData::sentinel(ctx.mcx(), types_snapshot::SnapshotType::SNAPSHOT_MVCC);
        mvcc.xmin = 600;
        mvcc.xmax = 650;
        let check = SnapshotCommitCheck::new(Some(&mvcc));
        assert!(!check.committed(committed));
        check.take_error().unwrap();
    }

    // IXDMLPATH2 item 8 (A): a check reads the next full xid once for all the generations it judges, own() and
    // committed() alike (catalog::visible asks both of each generation it walks), and reads it again for a publisher
    // at or past the value it holds. Two xids are assigned while the check walks the chain; the second one's publish
    // is judged inside the recent-past window, not as a recycled clog slot. With no re-read, the held value (epoch 2,
    // xid 1000) maps xid 1001 into epoch 1: an Aliased error and "not committed".
    #[test]
    fn a_check_reads_the_next_xid_once_and_again_past_what_it_read() {
        let _g = serial();
        install_seams();
        let ctx = mcx::MemoryContext::new("probe next_full test");
        let mut mvcc = SnapshotData::sentinel(ctx.mcx(), types_snapshot::SnapshotType::SNAPSHOT_MVCC);
        // every test xid precedes xmin: none is in the snapshot, so committed() reaches the clog
        mvcc.xmin = 5_000;
        mvcc.xmax = 5_000;
        let epoch = 2u64 << 32;
        for snapshot in [None, Some(&mvcc)] {
            NEXT_FULL.store(epoch | 1_000, Ordering::Relaxed);
            COMMITTED_XID.store(0, Ordering::Relaxed);
            let check = SnapshotCommitCheck::new(snapshot);
            let before = NEXT_FULL_READS.load(Ordering::Relaxed);
            for fxid in (epoch | 900)..(epoch | 910) {
                assert!(!check.own(fxid));
                assert!(!check.committed(fxid));
            }
            assert_eq!(NEXT_FULL_READS.load(Ordering::Relaxed) - before, 1, "one read for the ten generations (mvcc {})", snapshot.is_some());
            // xids 1000 and 1001 are assigned mid-walk; 1001's publisher committed
            NEXT_FULL.store(epoch | 1_002, Ordering::Relaxed);
            COMMITTED_XID.store(1_001, Ordering::Relaxed);
            assert!(check.committed(epoch | 1_001), "a publisher past the value read is judged in the window (mvcc {})", snapshot.is_some());
            assert!(!check.own(epoch | 1_001));
            assert!(check.take_error().is_ok(), "no recycled-slot error for a new publisher");
            assert_eq!(NEXT_FULL_READS.load(Ordering::Relaxed) - before, 2, "one more read, for the publisher past it");
            assert!(check.committed(epoch | 1_001) && NEXT_FULL_READS.load(Ordering::Relaxed) - before == 2);
        }
    }

    // VERDICTORDER (lcA-1): loader X (900) published over a committed base and commits while loader Y (1000) stacks
    // on X's head, at every point of Y's publish. X's part stays in Y's generation: never skipped by the walk as a
    // rolled-back head, never dropped by the settle as a rolled-back writer's.
    #[test]
    fn a_commit_across_a_stacking_publish_keeps_its_part() {
        use crate::tests::catalog_load::Dir;
        use pgrc_write::publish::BaseStats;
        // Y's process knows the numbers X used, as the process catalog would
        struct Known<'a>(&'a ClogTxnProbe, (u64, u32));
        impl TxnProbe for Known<'_> {
            fn verdict(&self, fxid: u64) -> TxnVerdict {
                self.0.verdict(fxid)
            }
            fn is_own(&self, publisher: u64, own: u64) -> bool {
                self.0.is_own(publisher, own)
            }
            fn stacks(&self) -> bool {
                self.0.stacks()
            }
            fn used_numbers(&self, _dir: &str) -> (u64, u32) {
                self.1
            }
        }
        let _g = serial();
        install_seams();
        NEXT_FULL.store(2_000, Ordering::Relaxed);
        COMMITTED_XID.store(42, Ordering::Relaxed);
        let mut lost = Vec::new();
        for clog in 0..=8 {
            for left in clog..=9 {
                let mut d = Dir::new(7_990);
                d.publish(42, &[1], 1, BaseStats::None);
                let x = d.publish(900, &[2], 1, BaseStats::None);
                RACE_TICK.store(0, Ordering::Relaxed);
                RACE_CLOG.store(clog, Ordering::Relaxed);
                RACE_LEFT.store(left, Ordering::Relaxed);
                RACE_XID.store(900, Ordering::Relaxed);
                let probe = ClogTxnProbe::stacking();
                let y = d.publish_by(1_000, &[3], 1, BaseStats::None, &Known(&probe, (x.gen, x.manifest.header.next_part_no)));
                RACE_XID.store(0, Ordering::Relaxed);
                let kept = y.manifest.parts.len() == 3 && y.manifest.parts.iter().any(|p| p.part_no == x.part_nos[0]);
                if !kept || !y.dropped.is_empty() || probe.take_error().is_err() {
                    lost.push((clog, left, y.dropped));
                }
            }
        }
        assert!(lost.is_empty(), "X's part lost at (clog tick, ProcArray tick, dropped): {lost:?}");
    }
}
