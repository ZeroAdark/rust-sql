//! The front of a pgrcolumnar table: the ring of heap segments small writes land in.

pub mod builtins;
pub mod compact;
pub mod compile;
pub mod dml;
pub mod fetch;
pub mod ixfetch;
pub mod ixbitmap;
pub mod ixclean;
pub mod filter;
pub mod ledger;
pub mod logical;
pub mod maint;
mod pathcache;
pub mod position;
pub mod reclaim;
pub mod ring;
pub mod rotate;
pub mod route;
pub mod sample;
pub mod seal;
pub mod segscan;
pub mod shadow;
pub mod stageb;
pub mod stats;
pub mod tombs;
pub mod vacfull;
pub mod xact;

use mcx::Mcx;
pub use pgrc_format::class::ColSchema;
use types_error::PgResult;
use types_core::Oid;
use types_rel::{NoLock, Relation};

/// Activate slot 0 of a pgrcolumnar table without an active segment and create its tombstone companion; the caller's
/// lock on `relid` serializes it.
pub fn ensure_companions<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<()> {
    ensure_as(mcx, relid, false)
}

pub fn ensure_companions_stacked<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<()> {
    ensure_as(mcx, relid, true)
}

fn ensure_as<'mcx>(mcx: Mcx<'mcx>, relid: Oid, stacked: bool) -> PgResult<()> {
    let rel = table::table_open(mcx, relid, NoLock)?;
    let pgrc = is_pgrc_table(&rel);
    // the lowest slot no ring record names (slot 0 for a directory without records): a renumbering VACUUM FULL
    // leaves sealed segments without an active one, and an older snapshot may still read them
    let out = match pgrc && ring::find_active(&rel)?.is_none() {
        true => match ring::newest(&rel)?.map(|g| ring::activation_slot(ring::records(&g))) {
            Some(None) => Err(Box::new(types_error::PgError::error(format!("pgrcolumnar: all {} segment slots of \"{}\" are in use", pgrc_format::rowid::SEG_SLOTS, rel.name())).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED))),
            free => ring::activate(mcx, &rel, free.flatten().unwrap_or(0), 0, stacked),
        },
        false => Ok(()),
    };
    rel.close(NoLock)?;
    out?;
    if pgrc {
        pgrc_front_seams::create_tombstone::call(mcx, relid)?;
    }
    Ok(())
}

/// A pgrcolumnar table with storage: relam pgrcolumnar's and a relkind with a table AM. A partitioned table's relam
/// only names its partitions' default (no storage, no directory, no front), and the relam probe alone cannot tell:
/// `tableam_vocab::is_pgrcolumnar_am_oid` reads a per-backend registry the relcache fills when it builds a relation
/// with a table AM (`RelationInitTableAccessMethod` skips the other relkinds), so for a partitioned parent it answers
/// by what the backend opened before: no in a fresh session, yes after any pgrcolumnar table.
pub fn is_pgrc_table(rel: &types_rel::RelationData<'_>) -> bool {
    types_rel::RELKIND_HAS_TABLE_AM(rel.rd_rel.relkind) && tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam)
}

/// Rows only the row executor reads right (design §6.5): live segment rows or uncompiled tombstones, or a statement
/// view that is composed (RR-VIEW, SEAL-OFFHEAD, [`ring::front_bearing_parts`]: SQE and the planner's claims key by
/// generation number). TOMB: tombstones in a tombstone heap a reader can need ([`tombs::bearing`], D4597).
pub fn front_bearing<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    front_bearing_as(mcx, rel, false)
}

fn front_bearing_as<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, served: bool) -> PgResult<bool> {
    let segs = if served { ring::front_bearing_served(mcx, rel)? } else { ring::front_bearing_parts(mcx, rel)? };
    if segs {
        return Ok(true);
    }
    tombs::bearing(mcx, rel, !served)
}

pub fn front_bearing_reads<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    front_bearing_as(mcx, rel, true)
}

pub fn front_blocks_relid<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<(u64, u64)> {
    let rel = table::table_open(mcx, relid, NoLock)?;
    let out = ring::unsealed_blocks(mcx, &rel).and_then(|seg| Ok((seg, compile::blocks(mcx, &rel)? as u64)));
    rel.close(NoLock)?;
    out
}

pub fn open_segment<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<Relation<'mcx>> {
    table::table_open(mcx, relid, NoLock)
}

pub fn front_bearing_relid<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<bool> {
    front_bearing_relid_as(mcx, relid, false)
}

/// [`front_bearing_relid`] for a reader of the parts that takes no tableam scan (Gravity, per execution): a table it
/// reads gets the scan's OC-4 relation-grain SIRead lock here. COMPOSED: a composed statement view does not bear here
/// (Gravity keys its part list by the view); live segment rows and tombstones do; a temp table always (D4697).
pub fn front_bearing_served<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<bool> {
    front_bearing_relid_as(mcx, relid, true)
}

fn front_bearing_relid_as<'mcx>(mcx: Mcx<'mcx>, relid: Oid, served: bool) -> PgResult<bool> {
    let rel = table::table_open(mcx, relid, NoLock)?;
    let pgrc = is_pgrc_table(&rel);
    let out = match pgrc {
        true if served && rel.rd_islocaltemp => Ok(true),
        true => front_bearing_as(mcx, &rel, served).and_then(|b| if b || !served { Ok(b) } else { predicate_lock_served(&rel).map(|()| false) }),
        false => Ok(false),
    };
    rel.close(NoLock)?;
    if served && pgrc && matches!(out, Ok(false)) {
        injection_point::injection_point("pgrc-front-served-clear")?;
    }
    out
}

/// For a served read of `rel`'s parts: [`filter::predicate_lock`] under the transaction snapshot (a serializable
/// transaction has one; taking it first registers the transaction with SSI, as the statement's own would).
fn predicate_lock_served(rel: &Relation<'_>) -> PgResult<()> {
    match ::xact::IsolationIsSerializable() {
        true => filter::predicate_lock(rel, &Some(snapmgr::GetTransactionSnapshot()?)),
        false => Ok(()),
    }
}

/// SQE's gate for a read of `rel`'s parts (its `find_columnar`; an error declines the statement to the row executor):
/// [`predicate_lock_served`]. Also an error while a generation a snapshot can still read holds parts written before an
/// ADD/DROP COLUMN or another schema ([`pgrc_am::fastadd::lagged`]): SQE reads stored streams only, the row scan and
/// Gravity fill the columns those parts lack.
pub fn served_lock(rel: &Relation<'_>) -> PgResult<()> {
    if pgrc_am::fastadd::lagged(rel)? {
        return Err(Box::new(types_error::PgError::error(format!("pgrcolumnar: \"{}\" holds parts of an older schema", rel.name()))));
    }
    predicate_lock_served(rel)
}

/// The caller holds the AccessExclusiveLock and makes the companions follow.
pub fn alter_columns(rel: &Relation<'_>, old: Vec<ColSchema>) -> PgResult<()> {
    pgrc_am::ingest::publish_schema(rel, old).map(drop)
}

/// ALTER's seal first, under its AccessExclusiveLock; a segment the drain cannot lock stays for the union scan.
pub fn seal_front<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<()> {
    if !is_pgrc_table(rel) || !ring::front_bearing(mcx, rel)? {
        return Ok(());
    }
    maint::try_lock(rel.rd_id)?;
    rotate::rotate(mcx, rel, 1)?;
    seal::seal_all(mcx, rel).map(|_| ())
}

pub fn segments(parent: Oid) -> PgResult<Vec<Oid>> {
    (0..pgrc_format::rowid::SEG_SLOTS).filter_map(|s| pgrc_front_seams::segment_relid::call(parent, s).transpose()).collect()
}

pub fn companions(parent: Oid) -> PgResult<Vec<Oid>> {
    let mut out = segments(parent)?;
    out.extend(tombs::all(parent)?.into_iter().map(|(_, r)| r));
    Ok(out)
}
