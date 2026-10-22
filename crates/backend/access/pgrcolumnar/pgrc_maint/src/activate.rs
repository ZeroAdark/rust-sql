//! A table's first front activation in its own transaction; the inserter waits for that transaction as a
//! `transactionid` lock wait. Nothing of the inserter's (no row, no snapshot, no role) goes into it.

use init_small::globals as g;
use pgsync::Mutex;
use types_core::{InvalidOid, InvalidTransactionId, Oid, TransactionId, TransactionIdIsValid};
use types_error::{PgResult, DEBUG1, LOG};
use types_rel::{AccessShareLock, NoLock, ShareUpdateExclusiveLock};
use types_storage::lock::XLTW_Oper;
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

const ACTIVATOR: &str = "pgrcolumnar front activation";
const POLL_MS: i64 = 1;
const WAIT_EVENT_BGWORKER_STARTUP: u32 = 0x0800_0000 + 6;

struct Req {
    id: u64,
    xid: TransactionId,
    done: Option<bool>,
}

pgsync::process_global! {
    static REQS: Mutex<(u64, Vec<Req>)> = Mutex::new((0, Vec::new()));
}

/// Removes the request when the inserter leaves (a cancel included); a worker that reports later finds nothing.
struct Forget(u64);

impl Drop for Forget {
    fn drop(&mut self) {
        pgsync::lock(&REQS).1.retain(|r| r.id != self.0);
    }
}

fn peek(id: u64) -> (TransactionId, Option<bool>) {
    pgsync::lock(&REQS).1.iter().find(|r| r.id == id).map_or((InvalidTransactionId, Some(false)), |r| (r.xid, r.done))
}

fn report(id: u64, f: impl FnOnce(&mut Req)) -> bool {
    match pgsync::lock(&REQS).1.iter_mut().find(|r| r.id == id) {
        Some(r) => {
            f(r);
            true
        }
        None => false,
    }
}

fn nap() -> PgResult<()> {
    let rc = latch::WaitLatch(g::MyLatch(), WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, POLL_MS, WAIT_EVENT_BGWORKER_STARTUP)?;
    if rc & WL_LATCH_SET != 0 {
        if let Some(l) = g::MyLatch() {
            latch::ResetLatch(l);
        }
    }
    postgres_seams::check_for_interrupts::call()
}

pub(crate) fn activate_detached(parent: Oid) -> PgResult<bool> {
    let id = {
        let mut reqs = pgsync::lock(&REQS);
        reqs.0 += 1;
        let id = reqs.0;
        reqs.1.push(Req { id, xid: InvalidTransactionId, done: None });
        id
    };
    let _forget = Forget(id);
    let mut w = crate::bgw(ACTIVATOR, activator_main, g::MyDatabaseId() as u64 | (parent as u64) << 32, bgworker::BGW_NEVER_RESTART);
    w.bgw_extra[..8].copy_from_slice(&id.to_le_bytes());
    let Some(handle) = bgworker::RegisterDynamicBackgroundWorker(w)? else {
        elog::elog(DEBUG1, format!("pgrcolumnar: no worker for the activation of relation {parent}: it activates in this transaction"))?;
        return Ok(false);
    };
    let stopped = || bgworker::GetBackgroundWorkerPid(&handle).0 == bgworker::BgwHandleStatus::BGWH_STOPPED;
    let xid = loop {
        let (xid, done) = peek(id);
        if let Some(ok) = done {
            return Ok(ok);
        }
        if TransactionIdIsValid(xid) {
            break xid;
        }
        if stopped() {
            return Ok(peek(id).1.unwrap_or(false));
        }
        nap()?;
    };
    lmgr::XactLockTableWait(xid, None, None, XLTW_Oper::None).map_err(|mut e| {
        e.add_context_line(format!("while waiting for the activation of relation {parent}"));
        e
    })?;
    loop {
        if let Some(ok) = peek(id).1 {
            return Ok(ok);
        }
        if stopped() {
            return Ok(peek(id).1.unwrap_or(false));
        }
        nap()?;
    }
}

/// One transaction, its xid reported before any lock is taken.
fn activator_main(arg: u64) -> PgResult<()> {
    let (db, relid) = (arg as Oid, (arg >> 32) as Oid);
    let id = bgworker::MyBgworkerEntry().map_or(0, |w| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&w.bgw_extra[..8]);
        u64::from_le_bytes(b)
    });
    crate::signals();
    let out = activate_in_own_transaction(db, relid, id);
    let ok = match out {
        Ok(ok) => ok,
        Err(e) => {
            let _ = elog::elog(LOG, format!("pgrcolumnar: the activation of relation {relid} failed (its inserter activates it): {}", e.message()));
            false
        }
    };
    report(id, |r| r.done = Some(ok));
    Ok(())
}

fn activate_in_own_transaction(db: Oid, relid: Oid, id: u64) -> PgResult<bool> {
    bgworker::BackgroundWorkerInitializeConnectionByOid(db, InvalidOid, 0)?;
    xact::StartTransactionCommand()?;
    let r = (|| {
        let xid = xact::GetTopTransactionId()?;
        report(id, |r| r.xid = xid);
        let ctx = mcx::MemoryContext::new("pgrc front activation");
        let Some(rel) = table::try_table_open(ctx.mcx(), relid, AccessShareLock)? else { return Ok(false) };
        let ours = tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam) && rel.rd_rel.relpersistence != types_core::RELPERSISTENCE_TEMP;
        rel.close(NoLock)?;
        if !ours {
            return Ok(false);
        }
        lmgr::LockRelationOid(relid, ShareUpdateExclusiveLock)?;
        pgrc_front::ensure_companions_stacked(ctx.mcx(), relid)?;
        elog::elog(DEBUG1, format!("pgrcolumnar: activated the front of relation {relid} in its own transaction {xid}"))?;
        Ok(true)
    })();
    match r {
        Ok(v) => xact::CommitTransactionCommand().map(|()| v),
        Err(e) => {
            xact::AbortCurrentTransaction()?;
            Err(e)
        }
    }
}
