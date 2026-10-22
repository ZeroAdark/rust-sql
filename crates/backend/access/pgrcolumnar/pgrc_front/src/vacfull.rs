//! VACUUM FULL of a pgrcolumnar table: a synchronous compaction in the calling backend, in place of heap's rewrite.
//! `vacuum_rel` calls [`step`] once per transaction under its session AccessExclusiveLock (every writer, reader and
//! maintenance worker queues behind it) until it returns false.
//! The renumber runs only once every tombstone and LOCK row is folded: an older snapshot reads its own generation and
//! the tombstone heaps by the IDs it knows, so a tombstone keyed by an old ID must not meet a new ID's row. Else it is
//! skipped with a WARNING (never a wait: an RR session holding the old snapshot that then asks for the table would
//! deadlock with the AccessExclusiveLock). Seal sets no new segment active when renumbering (an activation reserves an
//! extent, which a counter at its refusal mark refuses); the next inserter activates one.
//!
//! MVCC as PG's: a row a snapshot older than the command still sees is kept (its tombstone does not fold below the
//! horizon), and such a snapshot keeps reading its own generation, whose files stay until it ends.

use mcx::Mcx;
use pgrc_format::manifest::SegState;
use tableam_seams::PgrcVacFull;
use tableam_vocab::{VacuumParams, VACOPT_FULL, VACOPT_RENUMBER, VACOPT_VERBOSE};
use types_core::ForkNumber;
use types_error::PgResult;
use types_rel::{NoLock, Relation, RelationData};
use types_storage::buf::BufferAccessStrategy;

use crate::{compile, maint, ring, rotate, seal};

const SEAL: u32 = 0;
const COMPILE: u32 = 1;
const JOBS: u32 = 2;
const RENUMBER: u32 = tableam_seams::PGRC_VACFULL_RENUMBER;
const VACUUM: u32 = 4;
const RESET: u32 = 5;

const COMPILE_STEPS: u32 = 6;

pub fn step<'mcx>(mcx: Mcx<'mcx>, parent: &RelationData<'mcx>, params: &VacuumParams, bstrategy: BufferAccessStrategy, st: &mut PgrcVacFull) -> PgResult<bool> {
    let rel = table::table_open(mcx, parent.rd_id, NoLock)?;
    let out = step_on(mcx, &rel, parent, params, bstrategy, st);
    rel.close(NoLock)?;
    out
}

fn step_on<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, parent: &RelationData<'mcx>, params: &VacuumParams, bstrategy: BufferAccessStrategy, st: &mut PgrcVacFull) -> PgResult<bool> {
    // Every holder of the maintenance or front lock holds a lock on the table first, so under the AccessExclusiveLock
    // this never waits; it keeps a worker that reaches the table after the command from interleaving mid-step.
    maint::lock(rel.rd_id)?;
    match st.phase {
        SEAL => {
            st.parts_before = parts(rel)?.0;
            st.renumber = params.options & VACOPT_RENUMBER != 0 || pgrc_am::rowid_alloc::marks(rel)?.0 > pgrc_format::rowid::ROWID_LIMIT / 2;
            match st.renumber {
                true => rotate::retire_active(mcx, rel)?,
                false => rotate::rotate(mcx, rel, 1)?,
            };
            st.sealed += u64::from(seal::seal_all(mcx, rel)?.0);
            next(st, COMPILE);
        }
        COMPILE => {
            let c = compile::compile(mcx, rel, true)?;
            st.folded += c.tombstones;
            st.steps += 1;
            if !c.progressed() || st.steps >= COMPILE_STEPS {
                next(st, JOBS);
            }
        }
        JOBS => match crate::compact::run_full(mcx, rel)? {
            Some((_, out)) => {
                (st.jobs, st.removed, st.steps) = (st.jobs + 1, st.removed + out.dropped, st.steps + 1);
                if u64::from(st.steps) > 2 * (st.parts_before + st.sealed) + 2 {
                    elog::elog(types_error::LOG, format!("pgrcolumnar: VACUUM FULL of \"{}\" stopped after {} compaction jobs", rel.name(), st.steps))?;
                    next(st, VACUUM);
                }
            }
            None => match crate::compact::member_pass_forced(mcx, rel)? {
                Some(out) => {
                    (st.jobs, st.removed, st.steps) = (st.jobs + 1, st.removed + out.dropped, st.steps + 1);
                    if u64::from(st.steps) > 2 * (st.parts_before + st.sealed) + 2 {
                        elog::elog(types_error::LOG, format!("pgrcolumnar: VACUUM FULL of \"{}\" stopped after {} compaction jobs", rel.name(), st.steps))?;
                        next(st, VACUUM);
                    }
                }
                None => match st.renumber {
                    false => next(st, VACUUM),
                    true => match compile::unfolded(mcx, rel)? {
                        (0, 0) => {
                            st.cursor = pgrc_format::rowid::ROWID_FIRST;
                            next(st, RENUMBER);
                        }
                        (deletes, locks) => {
                            skipped(rel, st, deletes + locks)?;
                            next(st, VACUUM);
                        }
                    },
                },
            },
        },
        RENUMBER => {
            if st.steps > 0 {
                injection_point::injection_point("pgrc-vacfull-renumber-next")?;
            }
            match crate::compact::renumber_step(mcx, rel, &mut st.cursor)? {
                Some(r) => {
                    st.steps += 1;
                    if let crate::compact::Renumber::Rewrote(out) = r {
                        (st.rewritten, st.removed) = (st.rewritten + 1, st.removed + out.dropped);
                    }
                    if u64::from(st.steps) > 2 * parts(rel)?.0 + 2 {
                        elog::elog(types_error::LOG, format!("pgrcolumnar: VACUUM FULL of \"{}\" stopped its renumber after {} steps", rel.name(), st.steps))?;
                        next(st, VACUUM);
                    }
                }
                None => {
                    st.renumbered = true;
                    next(st, VACUUM);
                }
            }
        }
        RESET => {
            // after the renumber's publishes and the flush committed: nothing at or above the cursor is in use
            let Some(gen) = ring::newest(rel)? else { return Ok(false) };
            let keep: Vec<u32> = ring::records(&gen).iter().map(|r| r.part_no).collect();
            let top = (0..gen.manifest.parts.len()).flat_map(|i| gen.ids_of(i).last().map(|r| r.1)).max().unwrap_or(0);
            let top = gen.runs().iter().filter_map(|h| h.run.ranges.last().map(|r| r.1)).max().unwrap_or(0).max(top);
            st.next_id = pgrc_am::rowid_alloc::reset(rel, st.cursor.max(top), &|p| keep.contains(&p))?;
            let floor = st.next_id;
            pgrc_am::ingest::publish_ext_maint(rel, &mut |base, b| {
                b.ext(pgrc_format::manifest::with_next_row_id(base.map_or(&[][..], |m| &m.ext), floor)?);
                Ok(())
            })?;
            pgrc_am::rowid_alloc::note_renumber(rel, pgrc_am::rowid_alloc::Renumbered::Done { rewritten: st.rewritten });
            return Ok(false);
        }
        _ => {
            crate::builtins::flush_locked(mcx, rel)?;
            let lazy = VacuumParams { options: params.options & !(VACOPT_FULL | VACOPT_VERBOSE | VACOPT_RENUMBER), ..*params };
            tableam_seams::table_relation_vacuum::call(mcx, parent, &lazy, bstrategy)?;
            (st.parts_after, st.rows_after) = parts(rel)?;
            st.held_bytes = held_bytes(mcx, rel)?;
            if st.renumbered {
                next(st, RESET);
                return Ok(true);
            }
            return Ok(false);
        }
    }
    Ok(true)
}

fn skipped(rel: &Relation<'_>, st: &mut PgrcVacFull, visible: u64) -> PgResult<()> {
    st.visible = visible;
    pgrc_am::rowid_alloc::note_renumber(rel, pgrc_am::rowid_alloc::Renumbered::Skipped { visible });
    elog::ereport(types_error::WARNING)
        .errmsg(format!("pgrc table \"{}\": row IDs not renumbered: {visible} deleted or locked rows are still visible to older transactions", rel.name()))
        .errdetail("Renumbering needs every tombstone and row lock folded, and a transaction older than the deletes may still see them.")
        .errhint("Run VACUUM FULL again once they end. A replication slot's xmin or a standby's hot_standby_feedback can hold them back indefinitely: advance or drop the slot (pg_replication_slot_advance, pg_drop_replication_slot).")
        .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "vacfull::skipped"))
}

fn next(st: &mut PgrcVacFull, phase: u32) {
    (st.phase, st.steps) = (phase, 0);
}

fn parts(rel: &Relation<'_>) -> PgResult<(u64, u64)> {
    let Some(gen) = ring::newest(rel)? else { return Ok((0, 0)) };
    let deleted: u64 = gen.parts.iter().map(|p| p.deletes.as_ref().map_or(0, |d| d.deleted_rows)).sum();
    let rows: u64 = gen.manifest.parts.iter().map(|r| r.rows).sum();
    Ok((gen.manifest.parts.len() as u64, rows.saturating_sub(deleted)))
}

fn held_bytes<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<u64> {
    let Some(gen) = ring::newest(rel)? else { return Ok(0) };
    let l = rel.rd_locator.get();
    let mut counted: Vec<u32> = gen.manifest.parts.iter().map(|p| p.part_no).collect();
    let mut bytes = 0u64;
    if let Some((held, _)) = pgrc_am::catalog::generations((l.spcOid, l.dbOid, l.relNumber as u64)) {
        for p in held.iter().filter(|g| g.gen < gen.gen).flat_map(|g| g.manifest.parts.iter()) {
            if !counted.contains(&p.part_no) {
                counted.push(p.part_no);
                bytes += p.file_len;
            }
        }
    }
    for r in ring::records(&gen).iter().filter(|r| r.state == SegState::Sealed) {
        let seg = table::table_open(mcx, r.relid, NoLock)?;
        let n = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
        seg.close(NoLock)?;
        bytes += u64::from(n?) * types_core::BLCKSZ as u64;
    }
    Ok(bytes)
}
