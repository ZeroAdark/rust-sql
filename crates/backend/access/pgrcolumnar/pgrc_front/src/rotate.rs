//! The switch: the active segment becomes sealing and the lowest free slot becomes active.

use mcx::Mcx;
use pgrc_format::manifest::SegState;
use pgrc_format::rowid::SEG_SLOTS;
use types_core::ForkNumber;
use types_error::{PgError, PgResult};
use types_rel::{AccessShareLock, NoLock, Relation};

use crate::ring;

/// RBPROF: a rotation parks here after it counted the switch, before its transaction commits it.
pub const SWITCHED_POINT: &str = "pgrc-rotate-switched";

/// False without an active segment of at least `min_blocks` (≥ 1). A slot is free once reclaim drops its record.
pub fn rotate<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, min_blocks: u32) -> PgResult<bool> {
    let Some(g) = ring::newest(rel)? else { return Ok(false) };
    let recs = ring::records(&g);
    let Some(a) = recs.iter().find(|r| r.state == SegState::Active) else { return Ok(false) };
    let seg = table::table_open(mcx, a.relid, AccessShareLock)?;
    let n = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
    seg.close(NoLock)?;
    let n = n?;
    if n < min_blocks.max(1) {
        return Ok(false);
    }
    let Some(slot) = (0..SEG_SLOTS).find(|s| recs.iter().all(|r| r.slot != *s)) else {
        let msg = format!("pgrcolumnar: all {SEG_SLOTS} segment slots of \"{}\" are in use", rel.name());
        return Err(Box::new(PgError::error(msg).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)));
    };
    // A table that filled a segment will fill the next: its inserters get pages to find instead of an extension storm.
    let pre = if n >= crate::maint::ROTATE_BLOCKS { crate::maint::PRE_EXTEND_BLOCKS } else { 0 };
    ring::activate(mcx, rel, slot, pre, true)?;
    let sealing = ring::blocks(mcx, recs, &|s| s == SegState::Sealing)?;
    crate::maint::set_sealing(rel.rd_id, sealing + n as u64);
    let stall = crate::maint::rotated(rel.rd_id, a.relid);
    injection_point::injection_point(SWITCHED_POINT)?;
    let why = if stall { " (stall)" } else { "" };
    elog::elog(types_error::DEBUG1, format!("pgrcolumnar: rotated \"{}\" at {n} blocks (min {min_blocks}){why}, {sealing} blocks sealing, {pre} pre-extended", rel.name()))?;
    Ok(true)
}

pub fn retire_active<'mcx>(_mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    let Some(g) = ring::newest(rel)? else { return Ok(false) };
    if !ring::records(&g).iter().any(|r| r.state == SegState::Active) {
        return Ok(false);
    }
    pgrc_am::ingest::publish_ext_maint(rel, &mut |base, b| {
        let mut recs = base.map(pgrc_format::manifest::segments).transpose()?.unwrap_or_default();
        recs.iter_mut().filter(|r| r.state == SegState::Active).for_each(|r| r.state = SegState::Sealing);
        b.ext(ring::with_segments(base.map_or(&[][..], |m| &m.ext), &recs)?);
        Ok(())
    })?;
    Ok(true)
}
