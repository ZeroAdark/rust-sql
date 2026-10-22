//! AUTOVAC FREEZE: VACUUM's freeze step for a pgrcolumnar table's manifest chain (spec §13.1 "Freeze"; the delta-v2
//! port of main's d8502dd97e4, #2472).
//!
//! Heap VACUUM freezes the xmins before its OldestXmin before it advances `relfrozenxid` past them, and the
//! anti-wraparound path then truncates pg_xact below the oldest `datfrozenxid`. A pgrcolumnar parent's heap fork is
//! empty, so its heap pass advanced `relfrozenxid` to OldestXmin while every reader still looked the effective
//! generation's publisher up in pg_xact: once the truncation passed that xid, every read of the table raised "could not
//! access status of transaction" (58P01) and VACUUM could not repair it (lane AUTOVAC, box 23 av2). This step runs
//! inside the parent's heap pass, right after it takes its cutoffs (`vacuumlazy::heap_vacuum_rel_capped`, the
//! pgrcolumnar arm), so the freeze cutoff is the same OldestXmin the new `relfrozenxid` comes from: under the table's publish lock
//! it republishes the newest generation every snapshot sees under the frozen publisher
//! ([`pgrc_write::publish::publish_freeze`]: no xid, one WAL-logged `PUBLISH` of kind `freeze`), and returns the
//! floor the heap pass keeps `relfrozenxid` at or below when it cannot.

use pgrc_write::publish::{publish_freeze, BaseStats, FreezeOutcome};
use pgrc_write::seal::PartSpec;
use pgrc_write::wvfs::RealVfs;
use types_core::TransactionId;
use types_error::PgResult;
use types_rel::Relation;

use crate::catalog::RelSlot;
use crate::probe::ClogTxnProbe;

/// The freeze step of `rel`'s VACUUM at its heap pass's `oldest_xmin` (every snapshot sees a publisher before it; one
/// that commits later is at or after it): the `relfrozenxid` cap, `None` for none (no reader of the table looks an xid
/// up in pg_xact any more, or the table has no directory); `Some(InvalidTransactionId)` when the chain could not be
/// read (`relfrozenxid` stays where it is). The caller holds the VACUUM's lock on `rel`.
pub fn freeze(rel: &Relation<'_>, oldest_xmin: TransactionId) -> PgResult<Option<TransactionId>> {
    let locator = rel.rd_locator.get();
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    if !crate::dirpath::dir_exists(&dir)? {
        return Ok(None);
    }
    let cutoff = crate::probe::full_xid_at_now(oldest_xmin)?;
    let relfilenumber = locator.relNumber as u64;
    let schema_fingerprint = pgrc_format::ident::schema_fingerprint(&crate::schema::col_schemas(rel)?);
    let spec = PartSpec { spc: locator.spcOid, db: locator.dbOid, relfilenumber, schema_fingerprint };
    let slot: RelSlot = (locator.spcOid, locator.dbOid, relfilenumber);
    let mut sink = pgrc_wal_seams::sink_for::call(rel);
    let t0 = pg_clock::MonoStamp::now();
    let lock = crate::inval::publish_lock(relfilenumber);
    let guard = pgsync::lock(&lock);
    let probe = ClogTxnProbe::new();
    let base = crate::catalog::generations(slot).and_then(|(gens, head)| gens.into_iter().find(|g| g.gen == head)).map(|g| g.base_hold());
    let out = publish_freeze(&mut RealVfs, &mut *sink, &dir, &spec, &probe, base.as_ref().map_or(BaseStats::None, |b| b.stats()), cutoff);
    let (outcome, floor) = out.map_err(crate::write_error)?;
    probe.take_error()?;
    match outcome {
        FreezeOutcome::Frozen(o) => {
            let base_gen = o.manifest.header.prev_gen;
            // CONCLOAD: recorded under the publish lock, as every publish (ingest::publish_serialized)
            let g = crate::ingest::catalog_publish(rel, slot, &dir, *o, t0)?;
            drop(guard);
            // CONCLOAD test hook (the pgrc-catalog-order isolation spec): published and recorded, the lock released
            injection_point::injection_point("pgrc-freeze-published")?;
            let _ = elog::elog(types_error::DEBUG1, format!("pgrcolumnar: VACUUM froze \"{}\": generation {} republishes generation {base_gen} under the frozen publisher", rel.name(), g.gen));
        }
        FreezeOutcome::Skipped(Some(why)) => {
            let _ = elog::elog(types_error::DEBUG1, format!("pgrcolumnar: VACUUM did not freeze \"{}\" ({why}): relfrozenxid stays at or before {}", rel.name(), floor.unwrap_or(0)));
        }
        FreezeOutcome::Skipped(None) => {}
    }
    // the floor's low 32 bits: a clog-bound fxid older than the recent-past window already fails every reader
    Ok(floor.map(|f| f as TransactionId))
}
