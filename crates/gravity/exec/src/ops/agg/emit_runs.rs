//! `runs.emit_runs` (D3024 (b), the sorted wide emit): the run table of a group emit that sorts its groups in place and
//! runs wide (kernels table/emit.rs `RUNS_*`), laid zeroed per execution behind the proto's handle word. Each claim of
//! `pipeline` files its sorted run at the entry of the slice it started at; the top's reader merges the runs after the
//! section (`agg_merge_runs`). The table only names the runs: each run's array is a statement pin of the thread that
//! sorted it, so the table holds no record and frees none. Memory: `RUNS_TABLE_BYTES` per execution in the execution's
//! context, freed with it; nothing to answer the runner, so no live state.
use gravity_kernels::host_abi::RUNS_TABLE_WORDS;
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use crate::glue;
use crate::ops::{Live, Prepare, Protocol};

pub const PROTOCOLS: &[&dyn Protocol] = &[&EmitRuns];

pub struct EmitRuns;

impl Protocol for EmitRuns {
    fn kind(&self) -> ProtoKind {
        ProtoKind::EmitRuns
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let base = p.words.base as usize;
        if p.words.n != 1 || base >= cx.words.len().min(cx.nulls.len()) {
            return Err(glue::internal("gravity: runs.emit_runs without its handle word"));
        }
        let table = mcx::leak_in(mcx::alloc_in(cx.mcx, [0u64; RUNS_TABLE_WORDS])?);
        (cx.words[base], cx.nulls[base]) = (table.as_mut_ptr() as u64, 0);
        *cx.nparams = (*cx.nparams).max(base + 1);
        Ok(None)
    }
}
