//! sort's raw half in exec (rule R6: the family's unsafe code lives here): the group walk's cut over the owners' runs
//! (walk.rs `WalkLive::after_pipeline`, `WALK_GROUP`): the `n`-th smallest distinct first key they hold, then each run
//! keeps, in place and in order, the records at or under it.
#![allow(unsafe_code)]
use gravity_abi::breaker::{table_kind, HashAgg, Owned, OwnerTop, RecordLayout, SortRun, OWNED_HDR, OWNERTOP_HDR, SORTRUN_HDR, TABLE_OWNED, TABLE_SORT};
use gravity_driver::{Error, Exec, RegionBuf, Result};
use gravity_fn::arena::Owner;
use gravity_kernels::ops::sort::entries::{table_bytes, take_table, RunShape, RUN_SEALED};
use memledger::{ChargedVec, Kind};

use super::proto::head;

/// The group walk's cut over slot region `region` (its owners' runs hold records of `layout`): under `n` distinct first
/// keys nothing is cut. False when the region holds no owner runs (shorter than an `Owned` header, or another table kind).
/// GP-1b: `flip` (`!0` for a descending walk) XORs each first key before its compares: the NOT of the key, ascending.
/// `owner`: the walk's slot owner, which the key list is charged to.
pub(super) fn group_cut(region: &mut [u8], layout: &RecordLayout, n: usize, flip: i64, owner: Option<Owner>) -> bool {
    // SAFETY: the kind byte is read only past a whole `Owned` header; an owned region's owners hold the pipeline's runs.
    if region.len() < OWNED_HDR || unsafe { table_kind(region.as_ptr()) } != TABLE_OWNED {
        return false;
    }
    if let Some(bound) = unsafe { nth_key(region.as_ptr(), layout, n, flip, owner) } {
        unsafe { cut_runs(region.as_mut_ptr(), layout, bound, flip) };
    }
    true
}

/// The `n`-th smallest distinct first key the owners' runs hold; `None` under `n` distinct keys (no worker proved a bound:
/// the walk ran whole). The `n + 1` keys' list is charged to `owner` (the walk's slot owner); it never grows past that.
/// # Safety
/// As `cut_runs`.
unsafe fn nth_key(own: *const u8, layout: &RecordLayout, n: usize, flip: i64, owner: Option<Owner>) -> Option<i64> {
    let o = Owned::view(own);
    let (rec, koff) = (layout.size() as usize, layout.key_off(0));
    let mut keys: ChargedVec<i64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
    keys.with_vec(|keys| {
        keys.reserve_exact(n + 1); // alloc: Exec charged owner
        for j in 0..o.nowners as usize {
            let base = o.owner(j);
            let dir = base.add(OWNERTOP_HDR) as *const u32;
            for g in 0..OwnerTop::view(base).worst as usize {
                let (run, m) = (base.add(*dir.add(2 * g) as usize), *dir.add(2 * g + 1) as usize);
                (0..m).for_each(|r| super::walk::smallest_insert(keys, n, (run.add(r * rec + koff) as *const i64).read_unaligned() ^ flip));
            }
        }
    });
    keys.get(n.checked_sub(1)?).copied()
}

/// The group walk's cut (`WALK_GROUP`): each owner's runs keep, in place and in order, the records whose first key (an
/// integer image, the zone minima's order) is at most `bound`; the gather reads each run's new length.
/// # Safety
/// `own` is a `TABLE_OWNED` region whose owners' runs hold records of `layout`.
unsafe fn cut_runs(own: *mut u8, layout: &RecordLayout, bound: i64, flip: i64) -> u64 {
    let o = Owned::view(own);
    let (rec, koff) = (layout.size() as usize, layout.key_off(0));
    let mut kept = 0u64;
    for j in 0..o.nowners as usize {
        let base = o.owner(j);
        let dir = base.add(OWNERTOP_HDR) as *mut u32;
        for g in 0..OwnerTop::view(base).worst as usize {
            let (run, m) = (base.add(*dir.add(2 * g) as usize), *dir.add(2 * g + 1) as usize);
            let mut w = 0usize;
            for r in 0..m {
                if (run.add(r * rec + koff) as *const i64).read_unaligned() ^ flip <= bound {
                    if w != r {
                        core::ptr::copy_nonoverlapping(run.add(r * rec), run.add(w * rec), rec);
                    }
                    w += 1;
                }
            }
            *dir.add(2 * g + 1) = w as u32;
            kept += w as u64;
        }
    }
    kept
}


// The run merge's raw half (merge.rs `Resident`, proto.rs `ArmSort`): a sorted run's header at its region's base, a
// row's first words, its shape's compares / copy bytes / merges / compaction, the leader's laid header and its seal, a
// put to the slot's stream, each with the SAFETY reason it had at its call site.

/// The run header at the base of worker region `p`, which the caller found at least `SORTRUN_HDR` bytes long.
pub(super) fn run_at(p: &[u8]) -> &SortRun {
    debug_assert!(p.len() >= SORTRUN_HDR);
    // SAFETY: a worker region at least a run header long; its kind byte names the layout before the header's other
    // fields are read, and its rows are read only inside the region.
    unsafe { &*(p.as_ptr() as *const SortRun) }
}

/// Row `i`'s first two words (flags, image) of run `r`: a row the caller found inside its region, of at least 16 bytes.
pub(super) fn row_pair(r: &SortRun, i: usize) -> [u64; 2] {
    // SAFETY: row `i` lies inside the region the caller checked, and a row holds at least 16 bytes.
    unsafe { (r.row(i) as *const [u64; 2]).read_unaligned() }
}

/// D3027 (B): the part first granules the marks of the sealed run at the base of `region` name (heads.rs), sorted and
/// distinct: each row's word at `mark_off`, bit 63 a marked entry and bits 32..62 its part's first granule (lowerv4
/// dict/strategies/heads.rs `Mark`). `None` when the region holds no sealed run (its rows were put to the stream) or its
/// rows are too short for the word.
/// The list is charged to `owner` (the pass's slot owner).
pub(super) fn marked_starts(region: &[u8], mark_off: usize, owner: Option<Owner>) -> Option<ChargedVec<u64>> {
    if region.len() < SORTRUN_HDR {
        return None;
    }
    let r = run_at(region);
    let (n, rb) = (r.len as usize, r.row_bytes as usize);
    if r.kind != TABLE_SORT || r.pad[0] != RUN_SEALED || rb < mark_off + 8 || SORTRUN_HDR + n * rb > region.len() {
        return None;
    }
    // SAFETY: rows `0..n` of `rb` bytes lie inside the region (checked above), each with its mark word at `mark_off`.
    let mut starts: ChargedVec<u64> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
    starts.with_vec(|v| {
        v.extend((0..n).map(|i| unsafe { (r.row(i).add(mark_off) as *const u64).read_unaligned() }).filter(|m| m >> 63 != 0).map(|m| m >> 32 & 0x7fff_ffff)); // alloc: Exec charged owner
        v.sort_unstable();
        v.dedup();
    });
    Some(starts)
}

/// D4554: the (part first granule, code) pairs the marks of the sealed run at the base of `region` name (`marked_starts`'s
/// marks), sorted and deduplicated: the counts code.heads' emit asks.
pub(super) fn marked_codes(region: &[u8], mark_off: usize, owner: Option<Owner>) -> Option<ChargedVec<(u64, u32)>> {
    if region.len() < SORTRUN_HDR {
        return None;
    }
    let r = run_at(region);
    let (n, rb) = (r.len as usize, r.row_bytes as usize);
    if r.kind != TABLE_SORT || r.pad[0] != RUN_SEALED || rb < mark_off + 8 || SORTRUN_HDR + n * rb > region.len() {
        return None;
    }
    // SAFETY: as `marked_starts`: rows `0..n` of `rb` bytes lie inside the region, each with its mark word at `mark_off`.
    let mut pairs: ChargedVec<(u64, u32)> = ChargedVec::new(owner, Kind::Region); // alloc: Exec charged owner
    pairs.with_vec(|v| {
        v.extend((0..n).map(|i| unsafe { (r.row(i).add(mark_off) as *const u64).read_unaligned() }).filter(|m| m >> 63 != 0).map(|m| (m >> 32 & 0x7fff_ffff, m as u32))); // alloc: Exec charged owner
        v.sort_unstable();
        v.dedup();
    });
    Some(pairs)
}

/// D4554: entry `p`'s `(slots, table)` words in code.heads' count-pass directory `dir` (ops/sort/heads.rs), read on the
/// leader while no worker writes it (before the pass, or after it ended).
pub(super) fn pass_entry(dir: &[core::cell::UnsafeCell<u64>], p: usize) -> (u64, u64) {
    let e = gravity_kernels::dict::strategies::heads::PASS_DIR_HDR + p * gravity_kernels::dict::strategies::heads::PASS_DIR_ENTRY;
    // SAFETY: the words are the protocol's own, and no worker writes them now (the caller's invariant above).
    unsafe { (*dir[e + 1].get(), *dir[e + 2].get()) }
}

/// D4554: sets entry `p`'s `(slots, table)` words in code.heads' count-pass directory `dir`, on the leader before the pass
/// runs, for a part no claim of the pass counts (its entry stays the protocol's).
pub(super) fn set_pass_entry(dir: &[core::cell::UnsafeCell<u64>], p: usize, slots: u64, table: u64) {
    let e = gravity_kernels::dict::strategies::heads::PASS_DIR_HDR + p * gravity_kernels::dict::strategies::heads::PASS_DIR_ENTRY;
    // SAFETY: as `pass_entry`: no worker reads or writes the entry until the pass, which never claims its part.
    unsafe { (*dir[e + 1].get(), *dir[e + 2].get()) = (slots, table) };
}

/// D4554: `code`'s rows in the dense count table `table` of `slots` words a count-pass claim filled (`pass_entry`'s, its
/// `slots` neither 0, `PASS_NO_TABLE` nor sparse): pinned for the statement.
pub(super) fn pass_rows(table: u64, slots: u64, code: u32) -> u32 {
    // SAFETY: a counted entry's table holds `slots` (at least 1) words, pinned for the statement (kernels heads_tally).
    unsafe { gravity_kernels::dict::strategies::heads::pass_slot(table as *const u32, slots as usize, code) as u32 }
}

/// The `n` bytes at `p`: a worker partial's region, idle after the section until its detach.
pub(super) fn partial_bytes<'a>(p: usize, n: usize) -> &'a [u8] {
    // SAFETY: a worker partial's region, idle after the section until its detach.
    unsafe { core::slice::from_raw_parts(p as *const u8, n) }
}

/// The shape of the sealed var run whose rows start at `base` (`None`: no table copy).
pub(super) fn var_run_shape(base: usize) -> Option<RunShape> {
    // SAFETY: a sealed var run carries the table copy its open reserved.
    unsafe { RunShape::of_run(head(base)) }
}

/// The bytes of the table copy of the live run whose rows start at `base`.
pub(super) fn copy_bytes(base: usize) -> usize {
    // SAFETY: `base` is a live run's rows base.
    unsafe { table_bytes(head(base)) }
}

/// The leader's run laid at the base of `bytes` (at least `need` bytes): `cap` rows of `rb` bytes, `total` of them
/// filled, the table copy of `src`'s run (when given) and `var` bytes of copies reserved below its top. The rows' base
/// and the copies' offset; `None` when the copy or the copies do not fit (the header then holds no rows).
pub(super) fn lay_run(bytes: &mut [u8], need: usize, cap: u32, rb: u32, total: u64, var: u64, src: Option<usize>) -> Option<(*mut u8, usize)> {
    debug_assert!(bytes.len() >= need);
    let r = bytes.as_mut_ptr() as *mut SortRun;
    // SAFETY: the leader region holds `need` bytes; the header, table copy and var words are written before any reader.
    unsafe {
        *r = SortRun { cap, len: 0, row_bytes: rb, pad: [0; 3], kind: TABLE_SORT };
        (*r).clear();
        let top = src.is_none_or(|b| take_table(r, head(b))).then(|| (*r).reserve_var(var as u32)).flatten()?;
        (*r).len = total as u32;
        Some(((*r).rows_ptr() as *mut u8, top as usize))
    }
}

/// The copy bytes of the sealed run of shape `s` whose rows start at `base`.
pub(super) fn var_bytes(s: &RunShape, base: usize) -> u64 {
    // SAFETY: a sealed run of the shape.
    unsafe { s.var_bytes(head(base)) }
}

/// Rows `a` and `b` of sealed runs of shape `s`, each compared inside its run.
pub(super) fn cmp_rows(s: &RunShape, a: usize, b: usize) -> core::cmp::Ordering {
    // SAFETY: rows of sealed runs of the shape, compared inside their runs.
    unsafe { s.cmp(a as *const u8, b as *const u8) }
}

/// D3190: whether rows of shapes `a` and `b` (two inputs' sealed runs, both live) order alike on their first `nkey`
/// keys, so `cmp_cross` cuts one input at the other's splitters (a co-split merge join's inner at its outer's).
pub(crate) fn cross_ok(a: &RunShape, b: &RunShape, nkey: usize) -> bool {
    // SAFETY: both shapes are read off live runs (their tables live while the runs do).
    unsafe { a.cross_ok(b, nkey) }
}

/// D3190: row `a` of shape `sa` against row `b` of shape `sb` on their first `nkey` keys, `cross_ok(sa, sb, nkey)`.
pub(crate) fn cmp_cross(sa: &RunShape, a: usize, sb: &RunShape, b: usize, nkey: usize) -> core::cmp::Ordering {
    // SAFETY: rows of live runs of the shapes (or a put run's fence copy, a record of the shape), the caller's cross_ok held.
    unsafe { sa.cmp_cross(a as *const u8, sb, b as *const u8, nkey) }
}

/// The copy bytes of the `n` sealed rows of shape `s` from `row`.
pub(super) fn var_len(s: &RunShape, row: usize, n: u32) -> u64 {
    // SAFETY: the run's rows `row..row + n`, sealed rows of the shape.
    unsafe { s.var_len(row as *const u8, n) }
}

/// Part runs `srcs` of shape `s` merged into the leader at `base`: their rows from offset `rows`, their copies from
/// offset `var`; false when a compare failed.
pub(super) fn merge_part(s: &RunShape, srcs: &mut [(usize, u32)], base: *mut u8, rows: usize, var: usize) -> bool {
    // SAFETY: `lay_run` sized the leader for every row and copy; this part's go to its own rows and copy bytes alone.
    unsafe { s.merge(srcs, base.add(rows), base.add(var)) }
}

/// Runs `srcs` of shape `s` merged into a run `lay_run` laid: the rows at `rows`, the copies from offset `var` of
/// `base`; false when a compare failed.
pub(super) fn merge_laid(s: &RunShape, srcs: &mut [(usize, u32)], rows: *mut u8, base: *mut u8, var: usize) -> bool {
    // SAFETY: `lay_run` sized the run for every row and copy of `srcs`.
    unsafe { s.merge(srcs, rows, base.add(var)) }
}

/// The run header at the base of `bytes` marked sealed (its rows are in place).
pub(super) fn seal(bytes: &mut [u8]) {
    // SAFETY: a run header was laid at the region's base (`lay_run`, or phase 2's laid run).
    unsafe { (*(bytes.as_mut_ptr() as *mut SortRun)).pad[0] = RUN_SEALED };
}

/// The sealed source run whose rows start at `base`, compacted to its put form in place (`charged_compact`); `owner` the
/// sort's slot owner, whose row its allocator copy is on.
pub(super) fn compact(s: &RunShape, base: usize, owner: Option<Owner>) -> Result<()> {
    // SAFETY: a sealed source run, idle after its section
    unsafe { charged_compact(s, head(base) as *mut SortRun, owner) }
}

/// D4373 (b): run `run` compacted to its put form in place, its copy's bytes held in the grant pool's room now
/// (`scratch_take`), else in a mapped temp file (`copy_map`, the statement's spill road past the pool), else (no temp-file
/// road) the allocator's as before; an error only when the map fails. D4413: the copy polls the backend's own abort byte
/// (`glue::abort_u8`: the leader's put of its resident runs runs between pipelines, outside any kernel call, so no byte
/// is lent to it) every `POLL_BATCH` rows; a cancel stops it with the run as it lay and raises. CONVERT wave 2: an allocator
/// copy is charged to `owner` (the sort's slot owner) from `ARENA_SCRATCH_MIN` bytes on (kernels `compact_for`).
/// # Safety
/// `run` is a live run of shape `s` that nothing else reads.
unsafe fn charged_compact(s: &RunShape, run: *mut SortRun, owner: Option<Owner>) -> Result<()> {
    let step = core::cell::RefCell::new(cancelpoll::step_static(crate::glue::abort_u8(), "sort put: a resident run's compact copy"));
    let pending = || step.borrow_mut().tick().is_err();
    let bytes = s.compact_bytes(run) as u64;
    let stopped = if bytes < mempolicy::consts::get(mempolicy::consts::K::WorkFloor) {
        s.compact_for(run, None, owner, &pending)
    } else if let Some(_held) = gravity_driver::mem::region::scratch_take(bytes) {
        s.compact_for(run, None, owner, &pending)
    } else {
        match gravity_driver::mem::region::copy_map(bytes)? {
            Some(m) => s.compact_at(run, None, m.as_mut_ptr(), &pending),
            None => s.compact_for(run, None, owner, &pending),
        }
    };
    if stopped {
        return Err(cancelpoll::Canceled.into());
    }
    Ok(())
}

/// Region `leader` (the slot's own, at least a run header long, idle between the arms) armed afresh as a share.
pub(super) fn arm_share(leader: &mut [u8]) {
    debug_assert!(leader.len() >= SORTRUN_HDR);
    // SAFETY: the slot's own region, at least a run header long, idle between the arms.
    unsafe {
        core::ptr::write_bytes(leader.as_mut_ptr(), 0, SORTRUN_HDR);
        HashAgg::arm(leader.as_mut_ptr(), leader.len().min(u32::MAX as usize) as u32);
    }
}

/// Run `run` put to slot `slot`'s stream (`Node::put_sort_run`): a sealed run in its put form, idle after its section.
pub(super) fn put_sort_run(exec: &mut Exec, slot: u8, run: *mut u8) -> Result<u32> {
    // SAFETY: as `Node::put_sort_run` asks: a sealed run in its put form that nothing else reads.
    unsafe { exec.node_mut().put_sort_run(slot, run) }
}

/// A held run (a run `merged` laid) in its put form: a var run's rows and copies compacted in place
/// (`RunShape::compact`).
pub(super) fn put_form(b: &mut RegionBuf, var: bool, owner: Option<Owner>) -> Result<*mut u8> {
    let r = b.as_mut_ptr() as *mut SortRun;
    // SAFETY: `b` is a run `merged` laid: its header at its base.
    unsafe {
        if var && (*r).len != 0 {
            let s = RunShape::of_run(r).ok_or_else(|| Error::host("gravity: runs.sort: a held var run without its table copy"))?;
            // D4373 (b): the copy's bytes held as `compact`'s
            charged_compact(&s, r, owner)?;
        }
    }
    Ok(r as *mut u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The group cut keeps, per run and in order, the records whose first key is at most the bound.
    #[test]
    fn cut_runs_keeps_the_records_at_or_under_the_bound() {
        use gravity_abi::breaker::{KeyKind, SlotKind};
        let layout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::Count], false, Some(0));
        let rec = layout.size() as usize;
        let (owner_bytes, run_off) = (4096usize, 1024usize);
        let mut buf = vec![0u64; (OWNED_HDR + 2 * owner_bytes) / 8]; // alloc: Cold
        let own = buf.as_mut_ptr() as *mut u8;
        let runs: [&[i64]; 3] = [&[5, 30, 7, 12], &[40, 41], &[3]];
        unsafe {
            *(own as *mut Owned) = Owned { nowners: 2, owner_bytes: owner_bytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) };
            let o = Owned::view(own);
            for (j, owner_runs) in [&runs[..2], &runs[2..]].into_iter().enumerate() {
                let base = o.owner(j);
                (*(base as *mut OwnerTop)).worst = owner_runs.len() as u32;
                let dir = base.add(OWNERTOP_HDR) as *mut u32;
                let mut off = run_off;
                for (g, keys) in owner_runs.iter().enumerate() {
                    (*dir.add(2 * g), *dir.add(2 * g + 1)) = (off as u32, keys.len() as u32);
                    for (r, &k) in keys.iter().enumerate() {
                        *(base.add(off + r * rec + layout.key_off(0)) as *mut i64) = k;
                    }
                    off += keys.len() * rec;
                }
            }
            assert_eq!((nth_key(own, &layout, 3, 0, None), nth_key(own, &layout, 7, 0, None), nth_key(own, &layout, 8, 0, None)), (Some(7), Some(41), None));
            // GP-1b: a descending walk compares the NOT of each key (41 40 30 12 7 5 3 ascending as NOTs)
            assert_eq!((nth_key(own, &layout, 3, !0, None), nth_key(own, &layout, 7, !0, None), nth_key(own, &layout, 8, !0, None)), (Some(!30), Some(!3), None));
            assert_eq!(cut_runs(own, &layout, 12, 0), 4);
            let read = |j: usize, g: usize| {
                let base = o.owner(j);
                let dir = base.add(OWNERTOP_HDR) as *const u32;
                let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
                (0..m).map(|r| *(base.add(off + r * rec + layout.key_off(0)) as *const i64)).collect::<Vec<i64>>() // alloc: Cold
            };
            assert_eq!((read(0, 0), read(0, 1), read(1, 0)), (vec![5, 7, 12], vec![], vec![3])); // alloc: Cold
            // GP-1b descending: over the kept 5 7 12 | - | 3, the 2 largest distinct keys are 12 and 7: keep k >= 7
            assert_eq!(nth_key(own, &layout, 2, !0, None), Some(!7));
            assert_eq!(cut_runs(own, &layout, !7, !0), 2);
            assert_eq!((read(0, 0), read(0, 1), read(1, 0)), (vec![7, 12], vec![], vec![])); // alloc: Cold
        }
    }
}

/// D3190 (merge.cosplit, cosplit.rs): the sealed sort run at the base of slot region `region` as `(rows base, rows, row
/// bytes)` when its rows hold `nkey` key pairs; `None` for a region too short, another table, or a run not sealed.
pub(super) fn sealed_run(region: &[u8], nkey: usize) -> Option<(usize, u64, usize)> {
    if region.len() < SORTRUN_HDR {
        return None;
    }
    // SAFETY: the slot's region holds a whole run header (a region's base is 8-aligned); its fields are read, never written.
    let r = unsafe { &*(region.as_ptr() as *const SortRun) };
    let (n, rb) = (r.len as usize, r.row_bytes as usize);
    let ok = r.kind == TABLE_SORT && r.pad[0] == RUN_SEALED && r.len <= r.cap && rb >= 16 * nkey;
    (ok && n.checked_mul(rb).is_some_and(|b| SORTRUN_HDR + b <= region.len())).then(|| (region.as_ptr() as usize + SORTRUN_HDR, n as u64, rb))
}

/// D3336 (A1a, cosplit.rs `window_objects`): the object count of the parted window whose object table exec laid at `table`
/// (`WINDOW_PARTS_COUNT`, word 0).
pub(super) fn window_objects(table: u64) -> u64 {
    // SAFETY: `table` is the address the Window proto's prepare laid in its handle word (its table, live for the
    // execution, never moved once laid); word 0 is read, never written.
    unsafe { *(table as usize as *const u64).add(gravity_program::protos::WINDOW_PARTS_COUNT) }
}

/// D3190: rows `a` and `b` of two sorted runs on their first `nkey` keys' `[flags, word]` pairs, as a run orders a key with
/// no image tail (kernels rowsort.rs `RowCmp::step`): the flags, then the word, key by key.
pub(super) fn cmp_pairs(a: usize, b: usize, nkey: usize) -> core::cmp::Ordering {
    for w in 0..2 * nkey {
        // SAFETY: `a` and `b` are rows of sealed runs whose rows hold `nkey` pairs (`sealed_run`), read in place.
        let (x, y) = unsafe { ((a as *const u64).add(w).read_unaligned(), (b as *const u64).add(w).read_unaligned()) };
        if x != y {
            return x.cmp(&y);
        }
    }
    core::cmp::Ordering::Equal
}

#[cfg(test)]
mod cosplit_tests {
    use gravity_abi::breaker::{SortRun, SORTRUN_HDR, TABLE_SORT};
    use gravity_kernels::ops::sort::entries::RUN_SEALED;

    /// A region of one run header and `n` rows of `rb` bytes, sealed or not.
    fn region(n: u32, rb: u32, sealed: bool) -> Vec<u64> {
        let mut v = vec![0u64; (SORTRUN_HDR + (n * rb) as usize) / 8]; // alloc: Cold
        // SAFETY: the buffer holds a whole header at its 8-aligned base.
        unsafe { *(v.as_mut_ptr() as *mut SortRun) = SortRun { cap: n, len: n, row_bytes: rb, pad: [if sealed { RUN_SEALED } else { 0 }, 0, 0], kind: TABLE_SORT } };
        v
    }

    fn bytes(v: &[u64]) -> &[u8] {
        // SAFETY: the words' own bytes.
        unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
    }

    /// A sealed run reads as its rows; an unsealed one, a row narrower than the keys' pairs or a short region as none.
    #[test]
    fn only_a_sealed_run_with_its_pairs_reads() {
        let v = region(3, 32, true);
        assert_eq!(super::sealed_run(bytes(&v), 2), Some((v.as_ptr() as usize + SORTRUN_HDR, 3, 32)));
        assert_eq!(super::sealed_run(bytes(&v), 3), None, "a row of 32 bytes holds two pairs");
        assert_eq!(super::sealed_run(bytes(&region(3, 32, false)), 1), None);
        assert_eq!(super::sealed_run(&bytes(&v)[..SORTRUN_HDR + 40], 1), None, "the rows past the region");
        assert_eq!(super::sealed_run(&[], 1), None);
    }

    /// Pairs compare by flags first (NULLS FIRST 0 < a value 1 < NULLS LAST 2), then the word, key by key.
    #[test]
    fn pairs_compare_flags_then_words() {
        use core::cmp::Ordering::*;
        let rows: [[u64; 4]; 4] = [[0, 0, 1, 9], [1, 5, 1, 1], [1, 5, 1, 2], [2, 0, 1, 0]];
        let at = |i: usize| rows[i].as_ptr() as usize;
        assert_eq!(super::cmp_pairs(at(0), at(1), 2), Less);
        assert_eq!(super::cmp_pairs(at(1), at(2), 2), Less);
        assert_eq!(super::cmp_pairs(at(1), at(2), 1), Equal, "the first key ties");
        assert_eq!(super::cmp_pairs(at(3), at(2), 2), Greater);
    }
}
