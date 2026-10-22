//! `nest.spool`'s Spool protocol (RT-1s; the face agreed with M8-NEST): PostgreSQL's Material
//! tuplestore in a v4 program. The inner's records are appended one at a time as the first scan reads them (PG's lazy
//! Material fill: a record the scan never reaches is never produced, and none is produced twice), into a region the
//! object grows (`m8_spool_grow`) and a by-reference datum's copy (`m8_spool_copy`) in an arena that never moves.
//!
//! The cell (kernels host_abi/v4/m8.rs `SpoolCell`): the head, the region's first record, its capacity in records and one
//! record's bytes (the proto's `arg`). It starts empty (no region, capacity 0) at every execution's prepare, so the first
//! append grows it, and the object with its regions drops at the execution's end. A grow doubles the region (64 records
//! first) and may move it: no pointer into the region outlives the next append, and every by-reference datum a record
//! holds points into the copies, never into the region.
//!
//! Memory: every region comes through the driver's `object_region`, the arena's road: W, then the statement's grant; past
//! them a session with a temp-file road writes to temp files (raw.rs `Road`, C10 D4168), a rig without one reserves past
//! G, counted as grown (D225). The only error is the pool's own out of memory with no file road. The producer is serial by M8-NEST's named reason (one
//! filler; a second worker filling is an error by name). Created by M8-RUNTIME.
//! The object behind the cell (its regions, the copies' arena, `Head::op`) and all of its unsafe code is ops/material/raw.rs.
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use super::super::{Live, Prepare, Protocol};
use super::raw::{new_obj, new_owned, seal, Draw, OwnedObj, SpoolObj};
use crate::glue;

pub struct Spool;

/// The protocol's `Live`: the object, alive until the execution's end.
struct SpoolLive {
    _obj: Box<SpoolObj>,
}

impl Live for SpoolLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::NONE
    }
}

/// D3040: an owned spool's `Live`: the object, and its fill pipeline, after which the leader seals it. D4628 (road
/// `spool.seal`, `SPOOL_SEAL_WIDE` on the proto): `wide` the threads that seal it off the leader's copy (raw.rs `seal`),
/// the statement's width; `None`: the leader copies every owner's records alone.
struct OwnedLive {
    obj: Box<OwnedObj>,
    fill: usize,
    wide: Option<u16>,
}

impl Live for OwnedLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    /// The fill pipeline completed (no worker runs): every owner's records sealed into the merged cell.
    fn after_pipeline(&self, pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.fill {
            return None;
        }
        seal(&self.obj, exec.node().ctx(), self.wide).err().map(Err)
    }
}

impl Protocol for Spool {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Spool
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        lay(p, cx.words, cx.nulls, cx.nparams, gravity_driver::hostimpl::arena::object_region, cx.width).map(Some)
    }
}

/// One Spool proto's cell laid in its handle word (the first of its words: the count and the readers' words follow,
/// zeroed at bind); `width` the execution's (an owned spool's wide seal runs on that many threads).
fn lay(p: &Proto, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, draw: Draw, width: u16) -> PgResult<Box<dyn Live>> {
    let base = p.words.base as usize;
    if p.words.n == 0 || base + p.words.n as usize > words.len().min(nulls.len()) {
        return Err(glue::internal("gravity: nest.spool's declaration is not its spool cell's (a handle word, then its count and readers' words)"));
    }
    use gravity_kernels::ops::shared::object::entries::{SPOOL_OWNED, SPOOL_SEAL_WIDE};
    let owned = p.arg & SPOOL_OWNED != 0;
    let rec = p.arg & !(SPOOL_OWNED | SPOOL_SEAL_WIDE);
    nulls[base] = 0;
    *nparams = (*nparams).max(base + 1);
    if owned {
        let obj = new_owned(rec, draw)?;
        words[base] = obj.handle();
        let wide = (p.arg & SPOOL_SEAL_WIDE != 0).then_some(width.max(1));
        return Ok(Box::new(OwnedLive { obj, fill: p.peer as usize, wide })); // alloc: Exec
    }
    let obj = new_obj(rec, draw)?;
    words[base] = obj.handle();
    Ok(Box::new(SpoolLive { _obj: obj })) // alloc: Exec
}

/// D3054 `nest.spool.chunks` (lowerv4 runs.gsets_spool): reader `pipeline`'s claims are record ranges of the owned spool
/// its fill `peer` sealed, `SPOOL_CLAIM_RECORDS` records a claim, sized once the fill ran (after the Spool proto's seal,
/// declared before this one). The handle word (`words.base`, laid by the Spool proto) names the `OwnedSpools`.
pub struct SpoolChunks;

struct ChunksLive {
    pipeline: usize,
    fill: usize,
    handle: u64,
    claims: gravity_parallel::PartitionRange,
}

impl Protocol for SpoolChunks {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SpoolChunks
    }
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let h = cx.words.get(p.words.base as usize).copied().unwrap_or(0);
        if h == 0 {
            return Err(glue::internal("gravity: nest.spool.chunks before its spool was laid"));
        }
        let claims = gravity_parallel::PartitionRange::new(0, gravity_program::cost::breaker::SPOOL_CLAIM_RECORDS);
        Ok(Some(Box::new(ChunksLive { pipeline: p.pipeline as usize, fill: p.peer as usize, handle: h, claims }))) // alloc: Exec
    }
}

impl Live for ChunksLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::CLAIMS | crate::ops::ask::AFTER
    }

    fn claims_for(&self, pipeline: usize) -> Option<&dyn gravity_parallel::MorselSource> {
        (pipeline == self.pipeline).then_some(&self.claims as &dyn gravity_parallel::MorselSource)
    }
    /// The fill ran and the Spool proto sealed it (declared first): the claims cover its records.
    fn after_pipeline(&self, pipeline: usize, _exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.fill {
            return None;
        }
        self.claims.set_total(super::raw::sealed_total(self.handle));
        Some(Ok(()))
    }
}

/// G2(c) (spool-scoped recheck): an empty spool of Spool proto `p` laid over `words`, a recheck's own copy of
/// the parameter words (exec rerun/epq.rs `fresh_spools`): a new cell in the handle word and the count and readers' words
/// zeroed, as at a prepare, so the recheck's rerun of the spool's fill appends from the first record and its readers
/// read only those (EvalPlanQualStart's fresh planstate tree has fresh tuplestores). The statement's own spool and words
/// are untouched; the object lives as long as the returned box (the recheck).
pub fn fresh(p: &Proto, words: &mut [u64], nulls: &mut [u8]) -> PgResult<Box<dyn Live>> {
    let mut n = 0;
    let live = lay(p, words, nulls, &mut n, gravity_driver::hostimpl::arena::object_region, 1)?;
    let base = p.words.base as usize;
    words[base + 1..base + p.words.n as usize].fill(0);
    Ok(live)
}

/// The crate rig's lay (lowerv4 tests/common on_demand.rs, m8_init's `prepare_bare`): each Spool proto's cell, its regions
/// from the grant pool outside any Node.
pub fn prepare_bare(program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Spool) {
        let l = lay(p, words, nulls, nparams, super::raw::pool, 1)?;
        out.with_vec(|v| v.push(l)); // alloc: Exec charged out
    }
    Ok(())
}

