//! The hosted DISTINCT form's raw half (its unsafe code; the form itself is hosted_distinct.rs): the run, a list of
//! chunks in the aggregate context that takes the mapped road past work_mem (`room`, `chunks`, `keep_image`), and the
//! single-input form's row, combine and final (`collect`, `combine_sorted`, `fold_sorted`: nodeAgg.c
//! process_ordered_aggregate_single), each with its Safety text.
#![allow(unsafe_code)]
use core::alloc::Layout;
use core::ptr::{null_mut, NonNull};

use datum::{Datum, NullableDatum};
use gravity_abi::limits::{DISTINCT_RUN_CHUNK_MAX_WORDS, DISTINCT_RUN_CHUNK_MIN_WORDS};
use gravity_kernels::ops::agg::trans::hosted::{STATE_AGGNO, STATE_HOME, STATE_INIT, STATE_VALUE};
use mcx::Allocator;
use tuplesort::{Tuplesort, TUPLESORT_NONE};
use types_error::{PgError, PgResult};
use types_fmgr::{AggStateNode, LocalFcinfo};

use crate::ops::agg::trans::hosted_kept::image_len;
use crate::ops::agg::trans::hosted::AggArena;
use crate::ops::group::hosted_distinct::shape;
use crate::ops::shared::mapped::Road;

/// D3267: a group's kept rows, reached from its state slot's value word: chunks in arrival order, each holding whole rows
/// of (datum, NULL) words per input. A chunk holds at most `DISTINCT_RUN_CHUNK_MAX_WORDS` words (8 MiB), so a run of any
/// length never asks for an allocation near MaxAllocSize (the one 1 GiB run that aborted the server), and a growing run
/// never copies its rows. The head and the chunks a slot makes below its budget live in the slot's aggregate context; past it (`AggArena::budget`:
/// work_mem, as PG's per-aggregate sort, each participant its own), on the slot's mapped road (ops::shared::mapped), with
/// the later rows' by-reference images: any thread reads them, as the leader's combine and final read a worker's run.
#[repr(C)]
struct Run {
    /// The words kept, over every chunk.
    words: u64,
    first: *mut Chunk,
    last: *mut Chunk,
}

/// A chunk's header; its `cap` words follow it, the first `len` of them kept.
#[repr(C)]
struct Chunk {
    next: *mut Chunk,
    len: u64,
    cap: u64,
}

/// A row of up to this many inputs is built on the stack before its room is taken.
const KEPT_INLINE: usize = 4;

/// C9: a DISTINCT fold's stop for its statement's cancel, as an error. The hosted entry stashes it and fails; the call
/// settles as the statement's cancel (driver `stopped_error`), and the stash goes, so the owner raises PostgreSQL's
/// error for the stop and this text never reaches a client.
pub(crate) fn stopped() -> Box<PgError> {
    crate::pg_err("gravity: a DISTINCT aggregate's fold stopped for its statement's cancel")
}

/// Room at the end of the run at state slot `s` (none yet: made) for up to `want` words, whole rows of `w` words, at least
/// one row: its address and the words granted, counted in the run, for the caller to write. A full last chunk gets a
/// successor, in the aggregate context of `node` while it holds no more than `budget` bytes, else from `spill`.
/// # Safety — `s` is an opened DISTINCT state slot whose run (if any) this slot made, `w` its row's words.
unsafe fn room(node: &AggStateNode, spill: &mut Road, budget: u64, s: *mut u8, w: usize, want: usize) -> PgResult<(*mut u64, usize)> {
    let mcx = node.aggcontext();
    let at = s.add(STATE_VALUE) as *mut u64;
    let mut run = at.read_unaligned() as *mut Run;
    if run.is_null() {
        let layout = Layout::new::<Run>();
        run = mcx.allocate(layout).map_err(|_| mcx.oom(layout.size()))?.cast::<Run>().as_ptr();
        run.write(Run { words: 0, first: null_mut(), last: null_mut() });
        at.write_unaligned(run as u64);
    }
    let mut last = (*run).last;
    if last.is_null() || ((*last).cap - (*last).len) < w as u64 {
        let cap = if last.is_null() { DISTINCT_RUN_CHUNK_MIN_WORDS } else { ((*last).cap as usize * 2).min(DISTINCT_RUN_CHUNK_MAX_WORDS) }.max(w);
        let bytes = core::mem::size_of::<Chunk>() + 8 * cap;
        let past = spill.on() || node.aggcontext().context().subtree_allocated() as u64 > budget;
        let mapped = if past { spill.carve(bytes)? } else { None };
        let c = match mapped {
            Some(p) => p as *mut Chunk,
            None => {
                let layout = Layout::from_size_align(bytes, 8).expect("a DISTINCT run chunk's layout");
                let p: NonNull<u8> = mcx.allocate(layout).map_err(|_| mcx.oom(bytes))?.cast();
                p.as_ptr() as *mut Chunk
            }
        };
        c.write(Chunk { next: null_mut(), len: 0, cap: cap as u64 });
        if last.is_null() { (*run).first = c } else { (*last).next = c }
        (*run).last = c;
        last = c;
    }
    let free = ((*last).cap - (*last).len) as usize;
    let granted = want.min(free) / w * w;
    let p = (last as *mut u64).add(core::mem::size_of::<Chunk>() / 8 + (*last).len as usize);
    (*last).len += granted as u64;
    (*run).words += granted as u64;
    Ok((p, granted))
}

/// The run at state slot `s`: each chunk's kept words, in arrival order (none for a group that kept none).
pub(super) unsafe fn chunks<'a>(s: *const u8) -> impl Iterator<Item = &'a [u64]> {
    let run = (s.add(STATE_VALUE) as *const u64).read_unaligned() as *const Run;
    let mut c = if run.is_null() { null_mut() } else { (*run).first };
    core::iter::from_fn(move || {
        if c.is_null() {
            return None;
        }
        let words = core::slice::from_raw_parts((c as *const u64).add(core::mem::size_of::<Chunk>() / 8), (*c).len as usize);
        c = (*c).next;
        Some(words)
    })
}

/// Whether the run at state slot `s` keeps no row.
unsafe fn empty(s: *const u8) -> bool {
    let run = (s.add(STATE_VALUE) as *const u64).read_unaligned() as *const Run;
    run.is_null() || (*run).words == 0
}

/// A by-reference input `v` of `typlen` kept for the final: copied into the aggregate context of `node`
/// (ExecEvalAggOrderedTransDatum's datumCopy), or, once the slot takes the mapped road, into `spill` (an expanded object
/// is flattened by the aggregate context's copy, as before).
unsafe fn keep_image(node: &AggStateNode, spill: &mut Road, v: u64, typlen: i16) -> PgResult<u64> {
    let p = v as *const u8;
    if spill.on() && !(typlen == -1 && types_tuple::varatt::varatt_is_external_expanded(p)) {
        let len = image_len(p, typlen);
        if let Some(dst) = spill.carve(len)? {
            core::ptr::copy_nonoverlapping(p, dst, len);
            return Ok(dst as u64);
        }
    }
    Ok(execexpr::agg_datum_copy(node.aggcontext(), Datum::from_u64(v), typlen)?.as_u64())
}

impl AggArena {
    /// One input row of DISTINCT aggregate `i` in slot `k` kept in its group's run at `s` (ExecEvalAggOrderedTransDatum /
    /// Tuple): a strict transfn's row with a NULL input is dropped first, a by-reference value copied into the aggregate context
    /// (D3267: or onto the slot's mapped road past its budget, `keep_image`).
    /// # Safety — as `trans`.
    pub(crate) unsafe fn collect(&mut self, k: usize, i: usize, aggno: u32, s: *mut u8, args: *const u64, nulls: *const u8) -> PgResult<()> {
        if self.fed(i) {
            return self.fed_row(k, i, aggno, s, args, nulls); // D1596: advanced as it comes, PG's presorted DISTINCT
        }
        let (d, budget, slot) = (self.specs[i].distinct.as_ref().expect("a DISTINCT spec"), self.budget, &mut self.slots[k]);
        let n = d.cols.len();
        // only the transfn's inputs: a NULL ORDER BY key never drops its row (#509, EEOP_AGG_STRICT_INPUT_CHECK_NULLS)
        if slot.aggs[i].transfn.fn_strict && (0..d.ninputs).any(|c| nulls.add(c).read() != 0) {
            return Ok(());
        }
        if s.add(STATE_INIT).read() == crate::ops::agg::trans::hosted::FOLDED {
            return Err(shape("a row into a DISTINCT aggregate's group after its final folded it (D1323)"));
        }
        if s.add(STATE_INIT).read() == 0 {
            (s.add(STATE_VALUE) as *mut u64).write_unaligned(0);
            (s.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
            s.add(STATE_INIT).write(1);
        }
        let (node, spill) = (&*slot.node, &mut slot.spill);
        let mut kept = [0u64; 2 * KEPT_INLINE];
        let mut wide: Vec<u64> = Vec::new(); // alloc: Exec
        let row: &mut [u64] = if n <= KEPT_INLINE { &mut kept[..2 * n] } else { wide.resize(2 * n, 0); &mut wide }; // alloc: Exec
        // bounded("key-columns: one cell per sort column of a row; PG polls no deeper (a datum copy is one unit there too), at most MaxTupleAttributeNumber (1664) columns")
        for (c, &(typlen, byval)) in d.cols.iter().enumerate() {
            let (v, null) = (args.add(c).read(), nulls.add(c).read() != 0);
            row[2 * c] = if null || byval { v } else { keep_image(node, spill, v, typlen)? };
            row[2 * c + 1] = null as u64;
        }
        // the images first, then the row's room: an image may move the slot onto the mapped road, never a row half-written
        let (at, _) = room(node, spill, budget, s, 2 * n, 2 * n)?;
        core::ptr::copy_nonoverlapping(row.as_ptr(), at, 2 * n);
        slot.calls = slot.calls.wrapping_add(1);
        if slot.calls % 256 == 0 {
            slot.charge();
        }
        Ok(())
    }

    /// Two partial runs of one DISTINCT aggregate: `from`'s rows appended to `into`'s in slot `k` (their by-reference
    /// values stay where they are: every slot's context and mapped road live to the arena's end or its reset).
    /// # Safety — as `combine_in`.
    pub(crate) unsafe fn combine_sorted(&mut self, k: usize, aggno: u32, into: *mut u8, from: *const u8) -> PgResult<()> {
        if self.fed(self.agg(aggno)?) {
            return crate::ops::group::raw::presorted::rehome(into, from); // D1596: a state, never a run
        }
        if into.add(STATE_INIT).read() == crate::ops::agg::trans::hosted::FOLDED || from.add(STATE_INIT).read() == crate::ops::agg::trans::hosted::FOLDED {
            return Err(shape("a DISTINCT aggregate's partial combined after its final folded it (D1323)"));
        }
        if empty(from) {
            return Ok(());
        }
        let i = self.agg(aggno)?;
        let w = 2 * self.specs[i].distinct.as_ref().expect("a DISTINCT spec").cols.len();
        if into.add(STATE_INIT).read() == 0 {
            (into.add(STATE_VALUE) as *mut u64).write_unaligned(0);
            (into.add(STATE_AGGNO) as *mut u32).write_unaligned(aggno);
            into.add(STATE_INIT).write(1);
        }
        let (budget, slot) = (self.budget, &mut self.slots[k]);
        let mut step = cancelpoll::step("combine_sorted: a DISTINCT aggregate's partial run appended");
        // D3267: `from` is only read (a keyed owner fold's source stays as it was: `combine_keep`), whatever road its chunks took
        for mut words in chunks(from) {
            while !words.is_empty() {
                step.tick().map_err(|_| stopped())?;
                let (at, granted) = room(&slot.node, &mut slot.spill, budget, into, w, words.len())?;
                core::ptr::copy_nonoverlapping(words.as_ptr(), at, granted);
                words = &words[granted..];
            }
        }
        Ok(())
    }

    /// process_ordered_aggregate_single over the run at `s`: sorted, the duplicates dropped (the eqop under the
    /// aggregate's input collation; NULL equals NULL), each distinct value advanced into the fresh state at `out` (a zeroed
    /// `AGG_STATE_BYTES` slot, the initcond at its first call) in sort order.
    /// # Safety — `s` is DISTINCT aggregate `i`'s state slot, `out` a writable zeroed state slot; slot `k` is the caller's.
    pub(crate) unsafe fn fold_sorted(&mut self, k: usize, i: usize, aggno: u32, s: *const u8, out: *mut u8) -> PgResult<()> {
        if self.fed(i) {
            return Ok(crate::ops::group::raw::presorted::hand_on(s, out)); // D1596: every row already advanced, in PG's order
        }
        // D1320: the value word is a run only once `collect` / `combine_sorted` opened it (STATE_INIT); a zeroed slot is a
        // group no row reached. Any other word (a home tag, another form's value) raises by name, never read as a run.
        if s.add(STATE_HOME).read() != 0 || (s.add(STATE_INIT).read() == 0 && (s.add(STATE_VALUE) as *const u64).read_unaligned() != 0) {
            return Err(shape("a DISTINCT aggregate's final over a state its host sort never kept (D1320)"));
        }
        if empty(s) {
            return Ok(());
        }
        let d = self.specs[i].distinct.as_ref().expect("a DISTINCT spec");
        if d.cols.len() > 1 {
            return self.fold_multi(k, i, aggno, s, out);
        }
        let ((typlen, byval), key, coll, dedup) = (d.cols[0], d.keys[0], self.specs[i].a.inputcollid, d.dedup());
        let mut sort = Tuplesort::begin_datum(d.typ, key.1, key.2, key.3, crate::inputs::work_mem_kb(), TUPLESORT_NONE)?;
        let mut step = cancelpoll::step("fold_sorted: a DISTINCT aggregate's kept values sorted and folded");
        for c in chunks(s) {
            for e in c.chunks_exact(2) {
                step.tick().map_err(|_| stopped())?;
                sort.putdatum(Datum::from_u64(e[0]), e[1] != 0)?;
            }
        }
        sort.performsort()?;
        let mut old: Option<(u64, bool)> = None;
        let mut buf: Vec<u64> = Vec::new(); // alloc: Exec
        while let Some(nd) = sort.getdatum(true)? {
            step.tick().map_err(|_| stopped())?;
            if let Some((ov, on)) = old.filter(|_| dedup) {
                let same = match (on, nd.isnull) {
                    (true, true) => true,
                    (false, false) => {
                        let slot = &mut self.slots[k];
                        let Some(eq) = slot.aggs[i].eq.first_mut() else { return Err(shape("a DISTINCT aggregate without its equality function")) };
                        let mut fc = LocalFcinfo::<2>::fresh(coll);
                        fc.set_result_mcx(slot.tmp.mcx());
                        fc.args[0] = NullableDatum { value: Datum::from_u64(ov), isnull: false };
                        fc.args[1] = NullableDatum { value: nd.value, isnull: false };
                        let r = eq.invoke(&mut fc);
                        slot.tmp.reset();
                        r?.as_bool()
                    }
                    _ => false,
                };
                if same {
                    continue;
                }
            }
            let (v, n) = (nd.value.as_u64(), nd.isnull as u8);
            self.advance(k, i, aggno, out, &v, &n)?;
            // the sort's image may not outlive the next fetch once it spilled: the comparand keeps its own copy
            old = Some(match (nd.isnull, byval || typlen == 0) {
                (false, false) => {
                    let len = image_len(v as *const u8, typlen);
                    buf.clear();
                    buf.resize(len.div_ceil(8), 0);
                    core::ptr::copy_nonoverlapping(v as *const u8, buf.as_mut_ptr() as *mut u8, len);
                    (buf.as_ptr() as u64, false)
                }
                _ => (v, nd.isnull),
            });
        }
        sort.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3267 fault 2: an allocation past MaxAllocSize inside a host's landing pad (`m6_agg_trans` is `#[guard(ERROR)]`) is
    /// parked, the entry answers its failure, and the unwind the program's caller resumes carries mcxt.c's ERROR (XX000),
    /// which the statement boundary recovers: a statement error, never the crash a plain panic takes there (D2933).
    #[test]
    fn a_ceiling_hit_inside_a_hosts_pad_is_a_statement_error() {
        let ctx = mcx::MemoryContext::new("t");
        let size = mcx::MAX_ALLOC_SIZE + 1;
        let r = gravity_abi::status::unwind::pad(|| ctx.mcx().allocate(Layout::from_size_align(size, 8).unwrap()).is_ok());
        assert!(r.is_none(), "the ceiling unwound inside the pad");
        let payload = std::panic::catch_unwind(gravity_abi::status::unwind::resume).expect_err("the parked unwind resumes at the program's caller");
        let err = types_error::pg_error_from_panic(payload).unwrap_or_else(|_| panic!("the statement boundary recovers the ceiling's payload"));
        assert_eq!(err.sqlstate(), types_error::ERRCODE_INTERNAL_ERROR);
        assert_eq!(err.message(), format!("invalid memory alloc request size {size}")); // alloc: Cold
    }

    /// D3267 fault 1: a run past a chunk keeps every row in arrival order across chunks of at most `DISTINCT_RUN_CHUNK_MAX_WORDS` (no
    /// allocation near MaxAllocSize at any length), and appending it to another run (`combine_sorted`'s copy, granted in
    /// whole rows) leaves the source as it was. Budget 0 with no file road (the unit rig has no spill factory): the
    /// aggregate context keeps the chunks.
    #[test]
    fn a_run_keeps_its_rows_in_order_over_bounded_chunks_and_its_append_leaves_the_source() {
        let node = AggStateNode::new(mcx::MemoryContext::new("t"));
        let mut spill = Road::default();
        let (mut a, mut b) = ([0u64; 2], [0u64; 2]);
        let (sa, sb) = (a.as_mut_ptr() as *mut u8, b.as_mut_ptr() as *mut u8);
        let rows = 3 * DISTINCT_RUN_CHUNK_MAX_WORDS / 4 + 77; // 1.5 chunks of the largest size, past the doublings
        unsafe {
            for r in 0..rows as u64 {
                let (at, granted) = room(&node, &mut spill, 0, sa, 2, 2).unwrap();
                assert_eq!(granted, 2);
                (at.write(r), at.add(1).write(r & 1));
            }
            assert!(chunks(sa).all(|c| c.len() <= DISTINCT_RUN_CHUNK_MAX_WORDS && c.len() % 2 == 0));
            assert!(chunks(sa).count() > 2);
            let kept: Vec<u64> = chunks(sa).flat_map(|c| c.chunks_exact(2).map(|e| e[0])).collect();
            assert_eq!(kept, (0..rows as u64).collect::<Vec<_>>());
            let (at, _) = room(&node, &mut spill, 0, sb, 2, 2).unwrap();
            (at.write(u64::MAX), at.add(1).write(0));
            for mut words in chunks(sa) {
                while !words.is_empty() {
                    let (at, granted) = room(&node, &mut spill, 0, sb, 2, words.len()).unwrap();
                    assert!(granted >= 2 && granted % 2 == 0);
                    core::ptr::copy_nonoverlapping(words.as_ptr(), at, granted);
                    words = &words[granted..];
                }
            }
            let both: Vec<u64> = chunks(sb).flat_map(|c| c.chunks_exact(2).map(|e| e[0])).collect();
            assert_eq!(both.len(), rows + 1);
            assert_eq!(both[0], u64::MAX);
            assert!(both[1..].iter().copied().eq(0..rows as u64));
            assert_eq!(chunks(sa).map(|c| c.len()).sum::<usize>(), 2 * rows, "the source run is unchanged");
            assert!(!empty(sa) && empty([0u64; 2].as_ptr() as *const u8));
        }
    }
}
