//! D3024 (b), sorted: a group emit that sorts its groups in place (form G: `runs.topk_groups`, or a full sort over the
//! group's records) runs wide as a plain one does (table/emit_group.rs `Emit::new` takes it). Each claim gathers its slices
//! of the owner runs into a pointer array the statement pins (kernels `agg_gather_claim_pinned`) and sorts it as the
//! serial emit sorts its one array; the top (ops/sort/topn/lower.rs `TopEnd`) files the run it kept (its first `n`, or
//! every record) in the run table (`ProtoKind::EmitRuns`, kernels table/emit.rs `RUNS_*`) at the slice its claim started
//! at. The top's drain (`TopDrain`) merges the runs by the same total order (`agg_merge_runs`; agg_sort's key words end
//! every comparison), so its first `n` are the serial emit's, ties included. One claim (the width-1 arm, a leader-kept
//! pipeline) is one run, read in place. Nothing the drain reads lives in a call's scratch: the table is the execution's,
//! each run a statement pin, the merge a statement pin (D4476's lifetime).
use gravity_abi::ctx::hdr;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntCond, Type, Value};
use gravity_kernels::host_abi::{agg_merge_runs, agg_merge_runs_part, agg_merge_runs_part_k, agg_runs_claim, RUNS_CLAIM_NONE, RUNS_ENTRY_BYTES, RUNS_MAX};
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, GrowIn};

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::ops::sort::topn::lower::TopN;
use crate::table::emit_group::ES_PTRS;

crate::strategies! {
    /// D3336 (pmerge P1): a sorted wide emit's runs merged by key range at the section end (`agg_merge_runs_part`); off: the leader.
    EMIT_PARMERGE = 138, "emit.parmerge", Keyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(EMIT_PARMERGE, &())], ..crate::ops::NONE };

/// A sorted wide emit's run table as its emit declared it (`declare`), for its top and the top's drain.
#[derive(Clone, Copy)]
pub(crate) struct SortedRuns<'g> {
    /// The emit's pipeline (its top's).
    pub(crate) top: usize,
    /// The table's handle word.
    pub(crate) word: u16,
    /// The emit's sort words and form word: the merge compares as each claim's `agg_sort` did.
    pub(crate) words: &'g [u64],
    pub(crate) form: u64,
    /// The top's bound (`TopEnd::n`, filed by `bound`): the merge's first `n`; `None` merges every record.
    pub(crate) n: Option<TopN>,
    /// D3336: the records' layout (the section end's parallel merge builds the drain's descriptor, `part`).
    pub(crate) layout: gravity_abi::breaker::RecordLayout,
    /// D3336 (A1a, face/partfill.rs): the leading sort words the section end cuts its parts on (a window over the drain is
    /// partitioned by them, `partfill::kcut`); 0: on the whole order.
    pub(crate) kcut: u8,
}

/// Pipeline `k`'s run table: its handle word declared, the emit's sort words and form kept for the drain. The proto names
/// the emit's producer (`peer`) and its record's bytes (`arg`): exec's runner keeps the emit on the leader when the
/// producer's groups fit the owner rule's leader bytes (runner/plan.rs `emit_on_leader`).
/// D3336 (A1a): `kcut`, the leading sort words its parts are cut on (`partfill::kcut`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn declare<'g>(px: &mut Px<'g, '_>, k: usize, producer: u16, layout: &gravity_abi::breaker::RecordLayout, words: &[u64], form: u64, kcut: u8) {
    let word = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::EmitRuns, k as u16).peer(producer).arg(layout.size() as u64).words(word, 1));
    let words: &'g [u64] = words.iter().copied().collect_in(px.mcx()).leak();
    px.runs.push_in(SortedRuns { top: k, word, words, form, n: None, layout: *layout, kcut });
}

/// D3336 (road emit.parmerge): whether the section end of sorted wide emit `top` merges its runs in parallel
/// (`part`): the road on and its top taking every record (no bound; `bound` filed before this is asked).
pub(crate) fn parallel(px: &Px<'_, '_>, top: usize) -> bool {
    of(px, top).is_some_and(|r| r.n.is_none()) && crate::ops::sort::strategies::resident::on(px, EMIT_PARMERGE)
}

/// D3336: the section end's call (`SECTION_END`): this thread's share of the parallel merge (`agg_merge_runs_part`) under the
/// drain's descriptor; a failed compare (below zero, its error stashed) leaves through `l_exit`.
pub(crate) fn part(b: &mut FunctionBuilder<'_>, env: &Env, r: &SortedRuns<'_>) {
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let end = b.iconst(Type::I64, -1);
    let (l_part, l_go) = (b.create_block(&[]), b.create_block(&[]));
    let at_end = b.icmp(IntCond::Eq, lo, end);
    b.cond_br(at_end, (l_part, &[]), (l_go, &[]));
    b.switch_to(l_part);
    let desc = crate::table::emit_group::desc(b, &r.layout, r.words, r.form);
    let table = b.load(Type::Ptr, env.params, r.word as i32 * 8);
    let rc = match r.kcut {
        0 => {
            let h = b.declare_host_once("agg_merge_runs_part", &[Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), agg_merge_runs_part as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            b.call_host(h, &[env.ctx, desc, table], Some(Type::I64)).expect("agg_merge_runs_part returns its status")
        }
        // D3336 (A1a): the parts cut on the window's K (face/partfill.rs)
        n => {
            let h = b.declare_host_once("agg_merge_runs_part_k", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), agg_merge_runs_part_k as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let nk = b.iconst(Type::I64, n as i64);
            b.call_host(h, &[env.ctx, desc, table, nk], Some(Type::I64)).expect("agg_merge_runs_part_k returns its status")
        }
    };
    let failed = b.icmp(IntCond::Slt, rc, env.zero);
    b.cond_br(failed, (env.x.l_exit, &[rc]), (l_go, &[]));
    b.switch_to(l_go);
}

/// The run table of the sorted wide emit in pipeline `top`, if its emit runs so.
pub(crate) fn of<'g>(px: &Px<'g, '_>, top: usize) -> Option<SortedRuns<'g>> {
    px.runs.iter().find(|r| r.top == top).copied()
}

/// The top's bound for the merge (`SortedRuns::n`).
pub(crate) fn bound(px: &mut Px<'_, '_>, top: usize, n: Option<TopN>) {
    if let Some(r) = px.runs.iter_mut().find(|r| r.top == top) {
        r.n = n;
    }
}

/// The top's run (state `st`'s pointer array, its first `kept`) filed at the slice its claim started at, in table word
/// `word`. The section's end (a claim past every slice) files none; the claims are disjoint, so no entry is filed twice.
pub(crate) fn file(b: &mut FunctionBuilder<'_>, env: &Env, word: u16, st: Value, kept: Value) {
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let max = b.iconst(Type::I64, RUNS_MAX as i64);
    let (l_file, l_done) = (b.create_block(&[]), b.create_block(&[]));
    let inside = b.icmp(IntCond::Ult, lo, max);
    b.cond_br(inside, (l_file, &[]), (l_done, &[]));
    b.switch_to(l_file);
    let table = b.load(Type::Ptr, env.params, word as i32 * 8);
    let at = b.ptr_add(table, lo, RUNS_ENTRY_BYTES as u8, 0);
    let ptrs = b.load(Type::I64, st, ES_PTRS);
    b.store(at, 0, ptrs);
    b.store(at, 8, kept);
    b.br(l_done, &[]);
    b.switch_to(l_done);
}

/// The drain's records: the runs of `r` merged (`agg_merge_runs`, once per execution) under `desc` (the records' layout
/// and the emit's sort words and form, `SortedRuns`). The merge's state address, 0 when the pool refused its array, or a
/// failed compare's status (below zero, its error stashed).
pub(crate) fn merge(b: &mut FunctionBuilder<'_>, env: &Env, desc: Value, r: &SortedRuns<'_>) -> Value {
    let table = b.load(Type::Ptr, env.params, r.word as i32 * 8);
    let n = match r.n {
        Some(n) => n.value(b, env),
        None => b.iconst(Type::I64, -1),
    };
    let h = b.declare_host_once("agg_merge_runs", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), agg_merge_runs as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    b.call_host(h, &[env.ctx, desc, table, n], Some(Type::I64)).expect("agg_merge_runs returns its state")
}

/// D3336 (A1a, face/partfill.rs): a K-cut drain's claim (`agg_runs_claim` over run table `r` and the window's object table,
/// param word `tbl`): the object the claim fills filed in frame word `obj` (0: the window deals its partitions); a claim
/// that reads no record leaves for `l_out` with its kept count (frame word `w_kept`) 0 and no merge; else the claim's
/// `[lo, hi)` of the merged records (`u64::MAX`: their end), which the open clamps to their count (`clamp_u`).
pub(crate) fn claim(b: &mut FunctionBuilder<'_>, env: &Env, r: &SortedRuns<'_>, tbl: u16, obj: i32, w_kept: i32, l_out: Block) -> (Value, Value) {
    let table = b.load(Type::Ptr, env.params, r.word as i32 * 8);
    let wtbl = b.load(Type::Ptr, env.params, tbl as i32 * 8);
    let slot = b.declare_stack_slot(24, 8);
    let out = b.stack_addr(slot);
    let h = b.declare_host_once("agg_runs_claim", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), agg_runs_claim as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    let mode = b.call_host(h, &[env.ctx, table, wtbl, out], Some(Type::I64)).expect("agg_runs_claim returns its answer");
    let (lo, hi, o) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8), b.load(Type::I64, out, 16));
    env.store_word(b, obj, o);
    let none = b.iconst(Type::I64, RUNS_CLAIM_NONE);
    let (is_none, l_none, l_some) = (b.icmp(IntCond::Eq, mode, none), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(is_none, (l_none, &[]), (l_some, &[]));
    b.switch_to(l_none);
    env.store_word(b, w_kept, env.zero);
    b.br(l_out, &[env.zero]);
    b.switch_to(l_some);
    (lo, hi)
}

/// `v` at most `n`, unsigned (`claim`'s `u64::MAX` is past every count).
pub(crate) fn clamp_u(b: &mut FunctionBuilder<'_>, v: Value, n: Value) -> Value {
    let past = b.icmp(IntCond::Ugt, v, n);
    b.select(past, n, v)
}
