//! mcxtfuncs.c over the mcxt_stats root-context forest. The ownership model
//! has no single TopMemoryContext (thread-native divergence); the view grafts
//! the forest under the real TopMemoryContext row for C's parentage shape.

use core::sync::atomic::Ordering::Relaxed;

use ::datum::Datum;
use ::mcx::{Mcx, TreeStats};
use ::types_error::{ErrorLocation, PgResult, WARNING};
use ::types_fmgr::{
    varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction,
};
use ::types_core::ProcNumber;
use ::types_storage::ProcSignalReason::{self, PROCSIG_LOG_MEMORY_CONTEXT};
use elog::{ereport, PCT_M};

#[cfg(test)]
mod tests;

const MEMORY_CONTEXT_IDENT_DISPLAY_SIZE: usize = 1024;
const COLS: usize = 10;
const INT4OID: types_core::Oid = 23;

#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    // pgrust is Rust: report where in OUR source this was raised.
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

fn text_datum(mcx: Mcx<'_>, s: &[u8]) -> PgResult<Datum> {
    Ok(varlena_result(varlena::cstring_to_text(mcx, s)?))
}

fn int4_array_datum(mcx: Mcx<'_>, vals: &[i32]) -> PgResult<Datum> {
    let mut v: mcx::PgVec<'_, Datum> = mcx::vec_with_capacity_in(mcx, vals.len())?;
    v.extend(vals.iter().map(|&i| Datum::from_i32(i)));
    let img = datum::array_build::construct_array_image(mcx, &v, INT4OID, 4, true, b'i')?;
    let img = img.leak();
    Ok(Datum::from_usize(img.as_ptr() as usize))
}

fn put_context_row(
    srf: &mut funcapi::MaterializedSRF<'_>,
    mcx: Mcx<'_>,
    t: &TreeStats,
    path: &[i32],
) -> PgResult<()> {
    let mut values = [Datum::from_usize(0); COLS];
    let mut nulls = [false; COLS];

    let mut name: &str = t.name;
    let mut ident: Option<&str> = t.ident.as_deref();
    if let Some(id) = ident {
        if name == "dynahash" {
            name = id;
            ident = None;
        }
    }
    values[0] = text_datum(mcx, name.as_bytes())?;
    match ident {
        Some(id) => {
            let bytes = id.as_bytes();
            let mut idlen = bytes.len();
            if idlen >= MEMORY_CONTEXT_IDENT_DISPLAY_SIZE {
                idlen = mbutils::pg_mbcliplen(
                    bytes,
                    idlen as i32,
                    (MEMORY_CONTEXT_IDENT_DISPLAY_SIZE - 1) as i32,
                ) as usize;
            }
            values[1] = text_datum(mcx, &bytes[..idlen])?;
        }
        None => nulls[1] = true,
    }
    values[2] = text_datum(mcx, t.kind.as_bytes())?;
    values[3] = Datum::from_i32(path.len() as i32);
    values[4] = int4_array_datum(mcx, path)?;
    // mcxtfuncs.c PutMemoryContextsStatRecord: the allocator's stats method
    // fills totalspace / nblocks / freespace / freechunks, and used_bytes is
    // totalspace - freespace. AllocSet (aset.c:1545 AllocSetStats): block
    // bytes, every block on set->blocks, block tails + freelist chunks,
    // freelist population. C divergence (allocator-native figures): no
    // per-chunk headers or context header in the totals, a keeper taken
    // lazily (a context that has not allocated yet is reported as C's one
    // keeper block), bump free space is the block-transition window-tail
    // snapshot rather than C's live freeptr walk.
    let total = t.arena_footprint.max(t.used);
    let free = t.free_bytes.min(total);
    values[5] = Datum::from_i64(total as i64);
    values[6] = Datum::from_i64(t.nblocks.max(1) as i64);
    values[7] = Datum::from_i64(free as i64);
    values[8] = Datum::from_i64(t.free_chunks as i64);
    values[9] = Datum::from_i64((total - free) as i64);

    srf.putvalues(&values, &nulls)
}

#[repr(C)]
struct MemoryContextId {
    context: *const TreeStats,
    id: i32,
}

fn with_context_id_table<T>(
    mcx: Mcx<'_>,
    visit: impl FnOnce(*mut types_hash::hsearch::HTAB) -> PgResult<T>,
) -> PgResult<T> {
    use types_hash::hsearch::{HASHCTL, HASH_BLOBS, HASH_CONTEXT, HASH_ELEM};
    let mut ctl = HASHCTL::new();
    ctl.keysize = core::mem::size_of::<*const TreeStats>();
    ctl.entrysize = core::mem::size_of::<MemoryContextId>();
    ctl.hcxt = (mcx.context() as *const mcx::MemoryContext).cast_mut().cast();
    let table = dynahash::hash_create(
        "pg_get_backend_memory_contexts",
        256,
        &ctl,
        HASH_ELEM | HASH_BLOBS | HASH_CONTEXT,
    )?;
    let result = visit(table);
    dynahash::hash_destroy(table);
    result
}

pub fn fc_pg_get_backend_memory_contexts(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_get_backend_memory_contexts: resolved FmgrInfo required");
    // SAFETY: executor arms es_query_cxt pre-call; it outlives this frame.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    with_context_id_table(mcx, |table| {
        let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
        debug_assert_eq!(srf.tupdesc.natts as usize, COLS);

        // breadth-first ids (C keeps them stable near the roots)
        let forest = mcxt_stats::backend_context_tree();
        let mut queue: std::collections::VecDeque<(&TreeStats, Vec<&TreeStats>)> = Default::default();
        let mut next_id = 1i32;
        for root in &forest {
            queue.push_back((root, Vec::new()));
        }
        while let Some((node, parent_path)) = queue.pop_front() {
            let mut path = parent_path;
            let key = node as *const TreeStats;
            let entry = dynahash::hash_search(
                table,
                (&key as *const *const TreeStats).cast(),
                types_hash::hsearch::HASH_ENTER,
                None,
            )? as *mut MemoryContextId;
            // SAFETY: the table reserves MemoryContextId-sized entries for pointer keys.
            unsafe { (*entry).id = next_id };
            next_id += 1;
            path.push(node);
            let mut ids = mcx::vec_with_capacity_in(mcx, path.len())?;
            for ancestor in &path {
                let key = *ancestor as *const TreeStats;
                let entry = dynahash::hash_search(
                    table,
                    (&key as *const *const TreeStats).cast(),
                    types_hash::hsearch::HASH_FIND,
                    None,
                )? as *const MemoryContextId;
                // SAFETY: every ancestor was inserted before its children were queued.
                ids.push(unsafe { (*entry).id });
            }
            put_context_row(&mut srf, mcx, node, &ids)?;
            for child in &node.children {
                queue.push_back((child, path.clone()));
            }
        }

        Ok(srf.finish(fcinfo))
    })
}

pub fn fc_pg_log_backend_memory_contexts(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let pid = fcinfo.arg_i32(0);
    let found = log_backend_memory_contexts(
        pid,
        |pid| {
            // mcxtfuncs.c:273-275: a backend (BackendPidGetProc) or an
            // auxiliary process (AuxiliaryPidGetProc, proc.c:1124 -- the
            // NUM_AUXILIARY_PROCS slots only, never an arbitrary PGPROC).
            procarray::BackendPidGetProc(pid)
                .map(|p| p.vxid.procNumber.load(Relaxed))
                .or_else(|| lmgr_proc::AuxiliaryPidGetProc(pid))
        },
        |pid, reason, proc_number| {
            if procsignal::SendProcSignal(pid, reason, proc_number) < 0 {
                // C's %m renders the errno SendProcSignal left (ESRCH).
                Err(elog::errno::current_errno())
            } else {
                Ok(())
            }
        },
    )?;
    Ok(Datum::from_bool(found))
}

// pg_log_backend_memory_contexts (mcxtfuncs.c:258-300) over an injectable
// process lookup (BackendPidGetProc / AuxiliaryPidGetProc -> ProcNumber) and
// signal send (SendProcSignal; Err carries the errno it set): the
// "lookup succeeded, signal failed" arm is a backend leaving its ProcSignal
// slot between the two calls, which no SQL-level harness can force.
fn log_backend_memory_contexts(
    pid: i32,
    lookup: impl FnOnce(i32) -> Option<ProcNumber>,
    send: impl FnOnce(i32, ProcSignalReason, ProcNumber) -> Result<(), i32>,
) -> PgResult<bool> {
    // BackendPidGetProc() and AuxiliaryPidGetProc() return NULL if the pid
    // isn't valid; by the time we signal, a process found here may have
    // terminated on its own -- both arms are WARNINGs so a
    // loop-through-resultset will not abort.
    let Some(proc_number) = lookup(pid) else {
        ereport(WARNING)
            .errmsg(format!("PID {pid} is not a PostgreSQL server process"))
            .finish(loc("pg_log_backend_memory_contexts"))?;
        return Ok(false);
    };

    if let Err(errno) = send(pid, PROCSIG_LOG_MEMORY_CONTEXT, proc_number) {
        // mcxtfuncs.c:297-298: "could not send signal to process %d: %m" --
        // the strerror text of the errno SendProcSignal set (ESRCH).
        ereport(WARNING)
            .with_saved_errno(errno)
            .errmsg(format!("could not send signal to process {pid}: {PCT_M}"))
            .finish(loc("pg_log_backend_memory_contexts"))?;
        return Ok(false);
    }

    Ok(true)
}

const fn b(foid: types_core::Oid, name: &'static str, retset: bool, func: PGFunction) -> FmgrBuiltin {
    FmgrBuiltin { foid, name, nargs: if retset { 0 } else { 1 }, strict: true, retset, func }
}

pub const MCXTFUNCS_BUILTINS: &[FmgrBuiltin] = &[
    b(2282, "pg_get_backend_memory_contexts", true, fc_pg_get_backend_memory_contexts),
    b(4543, "pg_log_backend_memory_contexts", false, fc_pg_log_backend_memory_contexts),
];
