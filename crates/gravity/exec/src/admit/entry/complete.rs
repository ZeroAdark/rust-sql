//! Rung 14 contract C7, completion runs (docs/gravity/entry.md §11): COPY (query), CREATE TABLE AS, SELECT INTO and
//! REFRESH MATERIALIZED VIEW run their SELECT to completion (`ExecutorRun(Forward, 0)`) into a dest, so a parallel or
//! multi-pipeline program serves them through `runner::drive` (the tcop road's driver); every other caller keeps
//! the serial pull adapter, and a bounded run of such a program is an error by name (no such caller exists).
use crate::runner::{drive, eval_keys, key_words, via};
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::PgResult;
use types_slot::{EXEC_FLAG_BACKWARD, EXEC_FLAG_MARK, EXEC_FLAG_REWIND};

use crate::admit::entry::run::Serve;
use crate::admit::entry::QueryDesc;

/// An EXPLAIN ANALYZE of such a statement (EXPLAIN ANALYZE CREATE TABLE AS) takes the same road (docs/gravity/entry.md §4).
pub fn eligible(dest: CommandDest, eflags: i32) -> bool {
    matches!(dest, CommandDest::IntoRel | CommandDest::CopyOut | CommandDest::TransientRel) && eflags & (EXEC_FLAG_REWIND | EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK) == 0
}

// D3620: out of line, so run.rs `executor_run`'s frame (on every nested statement's stack path) does not hold its locals
#[inline(never)]
pub fn run(qd: &mut QueryDesc, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    let snap_ptr: *const std::rc::Rc<types_snapshot::SnapshotData<'static>> = qd.snapshot.as_ref().expect("a served statement has a snapshot");
    let params = qd.params;
    let serve: &mut Serve = qd.serve.as_mut().expect("serving");
    if count != 0 {
        return Err(crate::runner::refused("entry: a bounded run of a parallel program (completion runs only)"));
    }
    if serve.done {
        return Ok(());
    }
    let (home, hosted, cmd, cid) = (serve.home, serve.host_busy(), serve.cmd, serve.cid()); // D2355
    let mcx = serve.mcx();
    let r = crate::admit::reentry::enter(Some(crate::admit::reentry::built), Some(home), hosted, |b, depth| -> PgResult<()> {
        let amcx = b.admit_mcx();
        let nw = key_words(&serve.a);
        let (mut kparams, mut knulls) = (vec![0u64; nw], vec![0u8; nw]); // alloc: Exec
        if !eval_keys(&mut serve.a, params, &mut kparams, &mut knulls, amcx, false)? {
            return Err(crate::glue::internal("gravity_entry: key parameters"));
        }
        let nk = serve.a.nkparams;
        // the desc is boxed and keeps its snapshot until free; the run ends inside this call
        let snap = super::raw::snapshot(snap_ptr);
        // D622: a nested statement's sites (its InitPlans, its programs) take the grants it reserved at its start
        let nested = serve.nested.clone();
        // an EXPLAIN ANALYZE's run, and under PGRUST_GRAVITY_REPORT every run (serial census M0: the served line's `pipes=`):
        // its pipelines recorded (driver entry/record.rs, closed on every exit) and its wall time
        // an EXPLAIN ANALYZE's record keeps each pipeline's buffer counters too, and the run's whole delta (its root's)
        let probe = serve.report.is_some().then_some(crate::admit::entry::instrument::buffer_probe as fn() -> gravity_driver::entry::record::Bufs);
        let b0 = probe.map(|_| ::instrument::pg_buffer_usage());
        let token = (serve.report.is_some() || b.reporting()).then(|| b.entry.record_open_probed(&serve.a.program, probe));
        let ran = crate::runner::hand(depth, nested.as_ref(), || drive(mcx, &mut b.entry, &mut b.plan_scratch, &mut serve.a, None, snap, &kparams[..nk], &knulls[..nk], cid, cmd, dest, None, None));
        let runs = token.map(|t| b.entry.record_close(t));
        if let Some(b0) = b0 {
            ::instrument::buffer_usage_accum_diff(&mut serve.bufusage, &::instrument::pg_buffer_usage(), &b0);
        }
        let ran = ran?;
        let pipes = match &runs {
            Some((r, ns, _)) if b.reporting() => crate::admit::entry::instrument::served_pipes(r, *ns),
            _ => String::new(),
        };
        if let (Some((runs, run_ns, cells)), Some(report)) = (runs, serve.report.as_deref_mut()) {
            crate::admit::entry::instrument::report_driven(report, runs, run_ns, &ran);
            serve.instr = cells;
            serve.snapshot_fetches();
        }
        serve.rows = ran.rows;
        serve.es_processed = ran.rows;
        serve.workers = if ran.sections > 0 { ran.width } else { 0 };
        serve.done = true;
        let a = &serve.a;
        b.note_served_at(a.shape, ran.rows, ran.width, ran.sections, ran.claims, ran.opens, ran.granules, ran.first_touch, ran.cold, ran.p2, ran.io, ran.dict, ran.dictent, ran.elided, ran.node, via(a), a.program.code_of(0).kind(), &pipes);
        b.note_mechs(a.program.key.0, a.program.used); // MECHCORPUS: the executor-entry serve (writes with RETURNING) reports its kinds too
        Ok(())
    });
    match r {
        Ok(Some(Ok(()))) => Ok(()),
        Ok(Some(Err(e))) => {
            serve.done = true;
            Err(e)
        }
        Ok(None) => Err(crate::glue::internal("gravity_entry: no backend state")),
        Err(why) => Err(crate::runner::refused(why)),
    }
}
