//! D3190, merge.cosplit's runtime (lowerv4 face/cosplit.rs lowers the form): a merge join over two sorted runs,
//! wide. Once both producers ran, splitters are sampled from the outer's run (or its put runs' fences) and both inputs are
//! cut at their lower bounds on the merge keys' `[flags, word]` pairs (the cut module, ops/sort/cut.rs), so equal keys never
//! straddle two parts; the merge's claims are the parts, one each. The range table (its layout `COSPLIT_*` in gravity_abi
//! program.rs, its address in the proto's handle word, laid at prepare) holds each part's row ranges:
//! - both runs resident (`COSPLIT_MODE_RESIDENT`): part p's rows of each sealed run;
//! - the outer's runs put (`COSPLIT_MODE_SLICES`): part p's slice of each put run (cut through its fences), its claim's
//!   outer rows the merge of those slices (kernels sort `sort_cosplit_begin`), the inner's rows of the resident run.
//!
//! The pool's sizer may hand a claim several parts: consecutive parts are consecutive key ranges, so such a claim reads
//! one range of each run (the first part's start to the last's end; lowerv4 face/cosplit.rs `range`).
//!
//! A put inner, an outer run put without fences or past `MAXORDER` runs, an outer too small for two parts, or a width
//! under 2 leaves the one claim laid at prepare (`COSPLIT_MODE_WHOLE`): the whole of both runs, the serial form's rows.
//!
//! D4358: once cut, both slots are held as they lie (`Node::hold_as_laid`) until the merge reads them: the sort give-back a
//! revoke asks at the merge's open, and an own spill, put only runs no reader has committed to.
use core::cmp::Ordering;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gravity_abi::breaker::MAX_OWNERS;
use gravity_abi::limits::MAXORDER;
use gravity_abi::program::COSPLIT_MAX_PARTS as MAX_PARTS;
use gravity_driver::mem::fence::Records;
use gravity_fn::arena::{AVec, ChargedVec, Kind, OpArena, Owner};
use gravity_parallel::{MorselSource, PartitionRange};
use gravity_program::cost::{COSPLIT_PARTS_PER_WORKER as PARTS_PER_WORKER, COSPLIT_PART_ROWS_MIN as MIN_PART_ROWS};
use gravity_program::protos::{cosplit_of, cosplit_window_word, Proto, ProtoKind, COSPLIT_HDR_WORDS, COSPLIT_MODE, COSPLIT_MODE_RESIDENT, COSPLIT_MODE_SLICES, COSPLIT_PARTS, COSPLIT_PART_WORDS, COSPLIT_RUNS, COSPLIT_SLICES_AT, COSPLIT_WHOLE, COSPLIT_WINDOW};
use types_error::PgResult;

use gravity_kernels::ops::sort::entries::RunShape;

use crate::glue;
use super::cut;
use crate::ops::{Live, Prepare, Protocol};

pub struct CoSplit;

/// A part's ranges in the table: `[outer lo, outer hi, inner lo, inner hi]`.
type Part = [u64; COSPLIT_PART_WORDS];

/// One execution's co-split: the merge's pipeline, the producer after which the cut runs (the later of the two), the
/// runs' slots (outer, inner), the merge keys compared, the range table, the put outer's slices (`COSPLIT_SLICES_AT`
/// names them) and the claims over the table's parts.
struct CoSplitLive {
    pipeline: usize,
    last: usize,
    slots: [u8; 2],
    nkey: usize,
    table: Box<[AtomicU64]>,
    slices: pgsync::Mutex<Vec<u64>>,
    claims: PartitionRange,
    /// D3336 (A1a, `COSPLIT_WINDOW`): a parted window's fill alone, cut into its window's objects' count of parts; the
    /// window answers the fill's claims (one object each), so this execution answers none.
    window: bool,
    /// D3336 (A1a): the window's sort's fixed row (its SortMerge proto's `(peer, arg)`: keys, spec), `None` for a var row
    /// (its shape read off its run's table copy).
    fixed: Option<(usize, u64)>,
}

impl Protocol for CoSplit {
    fn kind(&self) -> ProtoKind {
        ProtoKind::CoSplit
    }

    /// The table laid in the proto's word (`COSPLIT_MODE_WHOLE`: every part the whole run, one claim until the cut), and
    /// the claims.
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let (h, k) = (p.words.base as usize, p.pipeline as usize);
        let (inner, nkey) = cosplit_of(p.arg);
        let window = p.arg & COSPLIT_WINDOW != 0;
        if p.words.n != 1 || h >= cx.words.len().min(cx.nulls.len()) || k >= cx.program.pipelines.len() || nkey == 0 {
            return Err(glue::internal("gravity: merge.cosplit is not one table word over one of the program's pipelines"));
        }
        let parts = match window {
            true => window_objects(cx, p.arg)?,
            false if cx.width < 2 => 1,
            false => (cx.width as usize * PARTS_PER_WORKER).min(MAX_PARTS),
        };
        let words = COSPLIT_HDR_WORDS + parts * COSPLIT_PART_WORDS + MAX_OWNERS;
        // D3336 (A1a): a window's parts past the first start empty (`[WHOLE, WHOLE)`): uncut, claim 0 reads the whole run
        let part = |i: usize| (COSPLIT_HDR_WORDS..COSPLIT_HDR_WORDS + parts * COSPLIT_PART_WORDS).contains(&i).then(|| ((i - COSPLIT_HDR_WORDS) / COSPLIT_PART_WORDS, (i - COSPLIT_HDR_WORDS) % COSPLIT_PART_WORDS));
        let at = |i: usize| match (i, part(i)) {
            (COSPLIT_PARTS, _) => parts as u64,
            (_, Some((_, w))) if w % 2 == 1 => COSPLIT_WHOLE,
            (_, Some((q, 0))) if window && q > 0 => COSPLIT_WHOLE,
            _ => 0,
        };
        let table: Box<[AtomicU64]> = (0..words).map(|i| AtomicU64::new(at(i))).collect(); // alloc: Exec
        cx.words[h] = table.as_ptr() as u64;
        cx.nulls[h] = 0;
        *cx.nparams = (*cx.nparams).max(h + 1);
        let prod = |s: u8| cx.program.producer_of_slot(k, s);
        // D3336 (A1a): the sort's fixed row, from its SortMerge proto (the slot's sort), for the window cut's order
        let fixed = cx.program.v4.as_deref().and_then(|d| d.protos.iter().find(|q| q.kind == ProtoKind::SortMerge && q.slot == p.slot)).and_then(|q| (q.peer != gravity_program::protos::NONE).then_some((q.peer as usize, q.arg)));
        let last = match (prod(p.slot), prod(inner)) {
            (Some(a), _) if window => a,
            (Some(a), Some(b)) => a.max(b),
            _ => usize::MAX,
        };
        let live = CoSplitLive { pipeline: k, last, slots: [p.slot, inner], nkey: nkey as usize, table, slices: pgsync::Mutex::new(Vec::new()), claims: PartitionRange::new(1, 1), window, fixed }; // alloc: Exec
        Ok(Some(Box::new(live))) // alloc: Exec
    }
}

impl Live for CoSplitLive {
    /// D3336 (A1a): a window's cut (`window`) answers no claims (`claims_for` says `None`: the window answers the fill's),
    /// but the bit stays: `answers` names the methods this impl writes (rearm E2), and an ask it declines costs one call.
    fn answers(&self) -> u16 {
        crate::ops::ask::CLAIMS | crate::ops::ask::AFTER
    }

    fn claims_for(&self, pipeline: usize) -> Option<&dyn MorselSource> {
        (pipeline == self.pipeline && !self.window).then_some(&self.claims as &dyn MorselSource)
    }

    /// Both runs sealed or put (the later producer ran): the cut, when the inner is resident and the outer is worth two
    /// parts.
    fn after_pipeline(&self, pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        if pipeline != self.last {
            return None;
        }
        if self.window {
            return self.after_window(exec);
        }
        let node = exec.node();
        let [o, i] = self.slots;
        let room = (self.table[COSPLIT_PARTS].load(Relaxed) as usize).min(MAX_PARTS);
        let inner = if node.sort_runs(i) == 0 { super::raw::sealed_run(node.slot_region(i), self.nkey) } else { None };
        let cut = match (inner, node.sort_runs(o)) {
            (None, _) => Ok(None),
            (Some(inner), 0) => match super::raw::sealed_run(node.slot_region(o), self.nkey) {
                Some(outer) => plan(outer, inner, parts_of(room, outer.1), self.nkey, node.slot_owner(o)).map(|r| r.map(|r| (r, Vec::new()))), // alloc: Exec
                None => Ok(None),
            },
            (Some(inner), n) => plan_put(node.spill_runs(), o, n, inner, room, self.nkey, node.slot_owner(o)),
        };
        let (ranges, slices) = match cut {
            Ok(Some(c)) => c,
            Ok(None) => {
                fires::fire("merge.cosplit.whole"); // shapecheck fire signal (D3190): a co-split merge run as one claim
                return None;
            }
            Err(e) => return Some(Err(e)),
        };
        for (p, r) in ranges.iter().enumerate() {
            r.iter().enumerate().for_each(|(w, &v)| self.table[COSPLIT_HDR_WORDS + p * COSPLIT_PART_WORDS + w].store(v, Relaxed));
        }
        if slices.is_empty() {
            self.table[COSPLIT_MODE].store(COSPLIT_MODE_RESIDENT, Relaxed);
        } else {
            let mut held = self.slices.lock().unwrap_or_else(|e| e.into_inner());
            *held = slices;
            self.table[COSPLIT_RUNS].store((held.len() / (2 * ranges.len())) as u64, Relaxed);
            self.table[COSPLIT_SLICES_AT].store(held.as_ptr() as u64, Relaxed);
            self.table[COSPLIT_MODE].store(COSPLIT_MODE_SLICES, Relaxed);
            fires::fire("merge.cosplit.put"); // shapecheck fire signal (D3190): a co-split over the outer's put runs
        }
        // D4358: the parts' ranges index both runs as they lie now; the merge's open (a revoke's sort give-back) or its own
        // spill must not put either under them (an inner put past its first row is a claim the kernel refuses as a Bug)
        let node = exec.node_mut();
        node.hold_as_laid(o);
        node.hold_as_laid(i);
        self.claims.set_total(ranges.len() as u64);
        fires::fire("merge.cosplit.wide"); // shapecheck fire signal (D3190): a merge join's claims cut over its inputs' keys
        Some(Ok(()))
    }
}

/// D3336 (A1a): the objects of the parted window whose object table is in `arg`'s window word (`cosplit_window_word`), laid
/// by the Window proto's prepare (declared before this one: the fill's source declares it after the window's handle).
fn window_objects(cx: &Prepare<'_>, arg: u64) -> PgResult<usize> {
    let w = cosplit_window_word(arg);
    let t = cx.words.get(w).copied().filter(|&t| t != 0 && cx.nulls.get(w) == Some(&0));
    let Some(t) = t else { return Err(glue::internal("gravity: win.partfill's cut names no laid window object table (D3336 A1a)")) };
    let n = super::raw::window_objects(t) as usize;
    if n == 0 || n > MAX_PARTS {
        return Err(glue::internal("gravity: win.partfill's window has no objects, or more than a cut's parts (D3336 A1a)"));
    }
    Ok(n)
}

impl CoSplitLive {
    /// D3336 (A1a, `COSPLIT_WINDOW`): the fill's sort ran: its one sealed run cut alone into at most the window's objects'
    /// count of parts on the first `nkey` keys, so no partition straddles two parts; the parts past the cut's are empty
    /// (the run's end), so object claim `i` reads part `i`. Uncut (`parts_of` under two, an order the cut cannot take, put
    /// runs: the sort spilled), part 0 stays the whole run, the others empty: claim 0 reads every row and the window deals
    /// them.
    fn after_window(&self, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        let node = exec.node();
        let o = self.slots[0];
        let room = (self.table[COSPLIT_PARTS].load(Relaxed) as usize).min(MAX_PARTS);
        // the run's own order on the window's keys: a fixed row's shape from the sort's SortMerge proto, a var row's from
        // its run's table copy; an order the cut cannot take (a user comparator, a lenient order: `cross_ok`) leaves it uncut
        let shape = |base: usize, rb: usize| match self.fixed {
            Some((nk, spec)) => Some(RunShape::fixed(rb as u32, nk, spec)),
            None => super::raw::var_run_shape(base),
        };
        let fit = |s: Option<RunShape>| s.filter(|s| super::raw::cross_ok(s, s, self.nkey));
        let cut = match node.sort_runs(o) {
            0 => match super::raw::sealed_run(node.slot_region(o), self.nkey) {
                Some(run) => match fit(shape(run.0, run.2)) {
                    Some(s) => plan_sides(run, None, parts_of(room, run.1), self.nkey, Some(&s), node.slot_owner(o)),
                    None => Ok(None),
                },
                None => Ok(None),
            },
            // put runs (the sort spilled): read merged by claim 0 and dealt, as the serial fill reads them
            _ => {
                fires::fire("exec.win.partfill.put"); // shapecheck fire signal (D3336 A1a): a K-cut fill over put runs, left uncut
                Ok(None)
            }
        };
        let ranges = match cut {
            Ok(Some(r)) => r,
            Ok(None) => {
                fires::fire("exec.win.partfill.whole"); // shapecheck fire signal (D3336 A1a): a K-cut fill run as one claim, dealt
                return None;
            }
            Err(e) => return Some(Err(e)),
        };
        let cut = ranges.len();
        let end = ranges.last().map_or(0, |r| r[1]);
        for p in 0..room {
            let r = if p < cut { ranges[p] } else { [end, end, 0, 0] };
            r.iter().enumerate().for_each(|(w, &v)| self.table[COSPLIT_HDR_WORDS + p * COSPLIT_PART_WORDS + w].store(v, Relaxed));
        }
        self.table[COSPLIT_MODE].store(COSPLIT_MODE_RESIDENT, Relaxed);
        fires::fire("exec.win.partfill.resident"); // shapecheck fire signal (D3336 A1a): a K-cut fill over one sealed run
        // D4358: the parts index the run as it lies now: no give-back or spill moves it under the fill's claims
        exec.node_mut().hold_as_laid(o);
        fires::fire("exec.win.partfill.cut"); // shapecheck fire signal (D3336 A1a): a K-cut fill's parts cut on the window's keys
        Some(Ok(()))
    }
}

/// Rows `a` and `b` on their first `nkey` keys: by `shape`'s own order (D3336 A1a's window cut), else their `[flags, word]`
/// pairs (the merge join's).
fn order(shape: Option<&RunShape>, a: usize, b: usize, nkey: usize) -> Ordering {
    match shape {
        Some(s) => super::raw::cmp_cross(s, a, s, b, nkey),
        None => super::raw::cmp_pairs(a, b, nkey),
    }
}

/// The parts an outer of `rows` rows is cut in: at most `room`, each at least `MIN_PART_ROWS` rows.
fn parts_of(room: usize, rows: u64) -> usize {
    room.min((rows / MIN_PART_ROWS) as usize)
}

/// The parts' ranges of runs `outer` and `inner` (each `(rows base, rows, row bytes)`): `parts - 1` splitters sampled from
/// the outer, both runs cut at them on their first `nkey` key pairs; `None` under two parts.
fn plan(outer: (usize, u64, usize), inner: (usize, u64, usize), parts: usize, nkey: usize, owner: Option<Owner>) -> gravity_driver::Result<Option<ChargedVec<Part>>> {
    plan_sides(outer, Some(inner), parts, nkey, None, owner)
}

/// `plan` over the outer alone when `inner` is `None` (D3336 A1a: a window's fill), its parts' inner words 0.
/// D3336 (A1a): `shape` `Some`: the run's own order on its first `nkey` keys (`RunShape::cmp_cross`: images, collations,
/// bpchar trim; its caller held `cross_ok`) instead of their `[flags, word]` pairs.
fn plan_sides(outer: (usize, u64, usize), inner: Option<(usize, u64, usize)>, parts: usize, nkey: usize, shape: Option<&RunShape>, owner: Option<Owner>) -> gravity_driver::Result<Option<ChargedVec<Part>>> {
    if parts < 2 {
        return Ok(None);
    }
    let cmp = |a: usize, b: usize| -> Ordering { order(shape, a, b, nkey) };
    let o = cut::Run::Mem { base: outer.0, rows: outer.1, rb: outer.2 };
    let refused = |r: gravity_fn::arena::Refused| gravity_driver::Error::out_of_memory(r.bytes);
    let a = OpArena::scratch_of(owner, Kind::Sort);
    let splitters = cut::sample_in(&a, core::slice::from_ref(&o), parts, &cmp).map_err(refused)?;
    let splitters = splitters.as_slice(&a);
    let w = splitters.len() + 2;
    let (mut oc, mut ic) = (AVec::<u64>::default(), AVec::<u64>::default());
    oc.resize(&a, w, 0).map_err(refused)?;
    ic.resize(&a, w, 0).map_err(refused)?;
    cut::cut_into(&o, splitters, &cmp, oc.as_mut_slice(&a))?;
    if let Some(inner) = inner {
        cut::cut_into(&cut::Run::Mem { base: inner.0, rows: inner.1, rb: inner.2 }, splitters, &cmp, ic.as_mut_slice(&a))?;
    }
    let (oc, ic) = (oc.as_slice(&a), ic.as_slice(&a));
    let mut ranges = ChargedVec::new(owner, Kind::Sort); // alloc: Exec charged owner
    ranges.with_vec(|v| v.extend((0..splitters.len() + 1).map(|p| [oc[p], oc[p + 1], ic[p], ic[p + 1]]))); // alloc: Exec charged owner
    Ok(Some(ranges))
}

/// The put outer's cut: slot `o`'s `nruns` put runs (tokens `0 .. nruns`, as `SPILL_RUN_OPEN` names them) sampled through
/// their fences and each cut at the splitters, the resident inner cut at them too; `None` when a run has no fences, the
/// runs pass `MAXORDER` or the outer is under two parts. The sample and the runs' cuts (up to `MAXORDER` x `MAX_PARTS`
/// words each) are scratch charged to `owner`, the outer's Sort slot owner (CONVERT wave 2).
fn plan_put(t: &gravity_driver::mem::spill::SpillTable, o: u8, nruns: u32, inner: (usize, u64, usize), room: usize, nkey: usize, owner: Option<Owner>) -> gravity_driver::Result<Option<(ChargedVec<Part>, Vec<u64>)>> {
    if nruns as usize > MAXORDER {
        return Ok(None);
    }
    let mut fences = ChargedVec::new(owner, Kind::Sort); // alloc: Exec charged owner
    for r in 0..nruns {
        let Some(f) = t.run_fences(o, r) else { return Ok(None) };
        fences.with_vec(|v| v.push(f)); // alloc: Exec charged owner
    }
    let parts = parts_of(room, fences.iter().map(|f| f.rows()).sum());
    if parts < 2 {
        return Ok(None);
    }
    let mut opens = ChargedVec::new(owner, Kind::Sort); // alloc: Exec charged owner
    opens.with_vec(|v| v.extend((0..nruns).map(|r| move |f: Option<usize>| t.run_records(o, r, f)?.ok_or_else(|| gravity_driver::Error::host("gravity: merge.cosplit: a fenced run without its records"))))); // alloc: Exec charged owner
    let mut runs: ChargedVec<cut::Run<'_>> = ChargedVec::new(owner, Kind::Sort); // alloc: Exec charged owner
    runs.with_vec(|v| v.extend(fences.iter().zip(opens.iter()).map(|(&fences, open)| cut::Run::Put { fences, open }))); // alloc: Exec charged owner
    let cmp = |a: usize, b: usize| -> Ordering { super::raw::cmp_pairs(a, b, nkey) };
    let refused = |r: gravity_fn::arena::Refused| gravity_driver::Error::out_of_memory(r.bytes);
    let a = OpArena::scratch_of(owner, Kind::Sort);
    let splitters = cut::sample_in(&a, &runs, parts, &cmp).map_err(refused)?;
    let splitters = splitters.as_slice(&a);
    if splitters.is_empty() {
        return Ok(None);
    }
    let w = splitters.len() + 2;
    let mut cuts = AVec::<u64>::default();
    cuts.resize(&a, runs.len() * w, 0).map_err(refused)?;
    for (r, at) in runs.iter().zip(cuts.as_mut_slice(&a).chunks_exact_mut(w)) {
        cut::cut_into(r, splitters, &cmp, at)?;
    }
    let mut ic = AVec::<u64>::default();
    ic.resize(&a, w, 0).map_err(refused)?;
    cut::cut_into(&cut::Run::Mem { base: inner.0, rows: inner.1, rb: inner.2 }, splitters, &cmp, ic.as_mut_slice(&a))?;
    Ok(Some(slice_parts(cuts.as_slice(&a), ic.as_slice(&a), owner)))
}

/// Each part's ranges and the slices (`[lo, hi)` of each run for each part, part-major) of the runs' cuts `cuts` (back to
/// back, each `parts + 1` positions, as many as `ic`'s) and the inner's `ic`: a part's outer range is its rows' place in the parts' order (the rows of
/// the parts before it, then its own: a claim of several parts reads the first's start to the last's end, kernels
/// `sort_cosplit_total` / `_next`), its inner range its own cut's.
fn slice_parts(cuts: &[u64], ic: &[u64], owner: Option<Owner>) -> (ChargedVec<Part>, Vec<u64>) {
    let n = ic.len() - 1;
    let mut slices = Vec::with_capacity(2 * n * (cuts.len() / ic.len())); // alloc: Exec
    let mut at = 0;
    let mut ranges = ChargedVec::new(owner, Kind::Sort); // alloc: Exec charged owner
    ranges.with_vec(|v| v.extend((0..n).map(|p| { // alloc: Exec charged owner
        let rows: u64 = cuts.chunks_exact(ic.len()).map(|c| c[p + 1] - c[p]).sum();
        cuts.chunks_exact(ic.len()).for_each(|c| slices.extend([c[p], c[p + 1]]));
        at += rows;
        [at - rows, at, ic[p], ic[p + 1]]
    })));
    (ranges, slices)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of `rb` bytes (`rb / 8` words) whose first two words are each key's `[flags, word]`: a run as the cut reads it.
    fn run(keys: &[(u64, u64)], rb: usize) -> Vec<u64> {
        let mut v = vec![0u64; keys.len() * rb / 8]; // alloc: Cold
        for (r, &(f, w)) in keys.iter().enumerate() {
            v[r * rb / 8] = f;
            v[r * rb / 8 + 1] = w;
        }
        v
    }

    /// Two sorted runs with many-to-many duplicates, NULLS FIRST (flags 0) and NULLS LAST (flags 2) rows, cut into parts:
    /// the parts cover each run in order, and every key of part p sorts before every key of part p + 1 in both runs, so
    /// equal keys of the two runs land in one part.
    #[test]
    fn both_runs_cut_where_no_equal_key_straddles() {
        let key = |x: u64| (1u64, x ^ 1 << 63);
        let mut ok: Vec<(u64, u64)> = (0..5000u64).map(|x| key(x / 7)).collect(); // alloc: Cold
        ok.insert(0, (0, 0));
        ok.extend([(2, 0), (2, 0)]);
        let mut ik: Vec<(u64, u64)> = (0..3000u64).map(|x| key(x / 3 * 2)).collect(); // alloc: Cold
        ik.insert(0, (0, 0));
        ik.push((2, 0));
        let (orb, irb) = (24, 16);
        let (ov, iv) = (run(&ok, orb), run(&ik, irb));
        for parts in [2usize, 3, 8, 64] {
            let ranges = plan((ov.as_ptr() as usize, ok.len() as u64, orb), (iv.as_ptr() as usize, ik.len() as u64, irb), parts, 1, None).unwrap().unwrap();
            assert_eq!(ranges.len(), parts);
            assert_eq!((ranges[0][0], ranges[0][2]), (0, 0));
            assert_eq!((ranges[parts - 1][1], ranges[parts - 1][3]), (ok.len() as u64, ik.len() as u64));
            for p in 0..parts {
                let r = ranges[p];
                assert!(r[0] <= r[1] && r[2] <= r[3], "part {p}: {r:?}");
                if p + 1 < parts {
                    assert_eq!((r[1], r[3]), (ranges[p + 1][0], ranges[p + 1][2]), "parts {p} and {} meet", p + 1);
                }
                // every key of this part (either run) under every key of the next parts
                let keys = |v: &[(u64, u64)], lo: u64, hi: u64| v[lo as usize..hi as usize].to_vec();
                let here: Vec<(u64, u64)> = keys(&ok, r[0], r[1]).into_iter().chain(keys(&ik, r[2], r[3])).collect(); // alloc: Cold
                let later: Vec<(u64, u64)> = ranges[p + 1..].iter().flat_map(|n| keys(&ok, n[0], n[1]).into_iter().chain(keys(&ik, n[2], n[3]))).collect(); // alloc: Cold
                if let (Some(a), Some(b)) = (here.iter().max(), later.iter().min()) {
                    assert!(a < b, "parts={parts}: part {p}'s key {a:?} not under a later part's {b:?}");
                }
            }
        }
    }

    /// Under 2 parts nothing is cut: one claim, the whole of both runs.
    #[test]
    fn under_two_parts_nothing_is_cut() {
        let (ov, iv) = (run(&[(1, 5), (1, 6)], 16), run(&[(1, 6)], 16));
        assert!(plan((ov.as_ptr() as usize, 2, 16), (iv.as_ptr() as usize, 1, 16), 1, 1, None).unwrap().is_none());
        assert_eq!((parts_of(32, 4095), parts_of(32, 3 * MIN_PART_ROWS), parts_of(4, 1 << 30)), (0, 3, 4));
    }

    /// The put outer's parts: each part's outer range its slices' rows after the parts before it, the slices part-major
    /// with each run's cuts in order, the inner's range its own cut's.
    #[test]
    fn put_runs_slice_part_major() {
        let (ranges, slices) = slice_parts(&[0, 3, 3, 9, 0, 0, 5, 6], &[0, 1, 4, 4], None);
        assert_eq!(ranges.to_vec(), vec![[0, 3, 0, 1], [3, 8, 1, 4], [8, 15, 4, 4]]); // alloc: Cold
        assert_eq!(slices, vec![0, 3, 0, 0, 3, 3, 0, 5, 3, 9, 5, 6]); // alloc: Cold
    }
}
