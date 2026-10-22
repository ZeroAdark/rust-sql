//! A table's rotation in a one-shot worker. Lock order everywhere: maintenance, front, rotate.

use init_small::globals as g;
use types_core::{InvalidOid, Oid};
use types_error::{PgResult, DEBUG1, LOG};
use types_rel::{AccessShareLock, NoLock};

const ROTATOR: &str = "pgrcolumnar front rotation";

pub(crate) fn rotate_detached(db: Oid, parent: Oid) -> bool {
    let w = crate::bgw(ROTATOR, rotator_main, db as u64 | (parent as u64) << 32, bgworker::BGW_NEVER_RESTART);
    matches!(bgworker::RegisterDynamicBackgroundWorker(w), Ok(Some(_)))
}

struct Ended(Oid);

impl Drop for Ended {
    fn drop(&mut self) {
        pgrc_front::maint::end_rotation(self.0);
    }
}

fn rotator_main(arg: u64) -> PgResult<()> {
    let (db, relid) = (arg as Oid, (arg >> 32) as Oid);
    let _ended = Ended(relid);
    crate::signals();
    bgworker::BackgroundWorkerInitializeConnectionByOid(db, InvalidOid, 0)?;
    // a maintenance worker: its publish defers rather than waits, and CREATE / DROP DATABASE may terminate it
    procarray::ProcSetStatusFlagDbMaintWorker()?;
    match rotate_in_own_transaction(relid) {
        Ok(true) => {
            pgrc_front::maint::queue(g::MyDatabaseId(), relid);
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(e) if pgrc_am::ingest::is_deferral(&e) => Ok(()),
        Err(e) => {
            let _ = elog::elog(LOG, format!("pgrcolumnar: the detached rotation of relation {relid} failed (the front worker rotates it): {}", e.message()));
            Ok(())
        }
    }
}

fn rotate_in_own_transaction(relid: Oid) -> PgResult<bool> {
    xact::StartTransactionCommand()?;
    let r = (|| {
        injection_point::injection_point("pgrc-rotate-detached")?;
        let ctx = mcx::MemoryContext::new("pgrc front rotation");
        let Some(rel) = table::try_table_open(ctx.mcx(), relid, AccessShareLock)? else { return Ok(false) };
        let out = if !tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam) || !pgrc_front::maint::try_rotate_lock(relid)? {
            Ok(false)
        } else {
            pgrc_front::rotate::rotate(ctx.mcx(), &rel, pgrc_front::maint::rotate_min(relid))
        };
        rel.close(NoLock)?;
        let rotated = out?;
        if rotated {
            elog::elog(DEBUG1, format!("pgrcolumnar: rotated relation {relid} in a detached worker"))?;
        }
        Ok(rotated)
    })();
    match r {
        Ok(v) => xact::CommitTransactionCommand().map(|()| v),
        Err(e) => {
            xact::AbortCurrentTransaction()?;
            Err(e)
        }
    }
}
