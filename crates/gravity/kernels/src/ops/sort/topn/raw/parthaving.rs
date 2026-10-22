//! The part-collapsing owner face (ops/sort/topn/parthaving.rs, lower.md §15.26 need 6): its owner phase, the spec read
//! back from the owner words, the floor's pigeonhole prune and the whole-record keep, moved here unchanged (rule R6: the
//! family's unsafe code lives in raw). parthaving.rs keeps the spec, its word layout,
//! `owner_fns`, `runs_area` and the prune's `GroupSet`.
use gravity_abi::breaker::{FloatOverflow, HashAgg, OwnerTop, RecordLayout, DENSE, GROUPS_UNCOUNTED, HASHAGG_HDR, MINCODE_NONE, OWNERTOP_HDR, OWNER_MAX_RUNS, OWNER_PARAMS, OWNER_RUN_OFF};
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SCRATCH};
use gravity_abi::Site;

use crate::dict::reps::{render, unpack_slot, varlena_bytes, MAX_REPS, REP_NONE};
use crate::ops::sort::topn::parthaving::{runs_area, GroupSet, PartHavingSpec, LAYOUT_WORDS, P_FLOOR, P_HEAD, P_LAYOUT, P_MINS, P_MINS_MORE};

unsafe fn spec_of(params: *const u64) -> PartHavingSpec {
    let w = core::slice::from_raw_parts(params, OWNER_PARAMS);
    let mut words = [0u64; LAYOUT_WORDS];
    words.copy_from_slice(&w[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS]);
    // SAFETY: the words `owner_fns` packed.
    let layout: RecordLayout = RecordLayout::from_words(words);
    PartHavingSpec { layout, mins: unpack_slot(&core::array::from_fn(|i| if i == 0 { w[P_MINS] } else { w[P_MINS_MORE + i - 1] })), count_slot: w[P_HEAD] as u8, bits: (w[P_HEAD] >> 8) as u8, floor: w[P_FLOOR] as i64 }
}

pub(in crate::ops::sort::topn) fn owner_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `OWNER_PARAMS` words.
    let sp = unsafe { spec_of(params) };
    let table = (HASHAGG_HDR + 64 * sp.layout.size() as usize) as u64;
    OWNER_RUN_OFF as u64 + 2 * table
}

pub(in crate::ops::sort::topn) unsafe fn owner_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let sp = spec_of(params);
    let rec = sp.layout.size() as usize;
    let off = OWNER_RUN_OFF + runs_area(bytes);
    if off + HASHAGG_HDR + 2 * rec > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    // `n` carries the record's size: the runs form's reader without the words (`havingexport`)
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: rec as u32, table_off: off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    off as i64
}

pub(in crate::ops::sort::topn) unsafe fn owner_end(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

pub(in crate::ops::sort::topn) unsafe fn owner_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let sp = spec_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    h.records += t.len as u64;
    let g = sp.layout.partition_keys();
    let words = |p: *const u8| core::slice::from_raw_parts(p as *const u64, g);
    if g == sp.layout.nkeys() && sp.floor > 1 {
        return keep_whole(&sp, h, t, owner);
    }
    if sp.floor > 1 {
        match prune_below_floor(&sp, t, g) {
            Prune::Kept => {}
            // the table no longer holds every group: no distinct count for the memo (its previous value stands)
            Prune::Pruned => h.groups = GROUPS_UNCOUNTED,
            // no group can reach the floor: nothing to keep, and the table is left as folded (the caller's to reuse)
            Prune::Nothing => {
                h.groups = GROUPS_UNCOUNTED;
                return 0;
            }
        }
    }
    t.sort_dense_by_words(g);
    let (n, cs, rec) = (t.len as usize, sp.count_slot as usize, sp.layout.size() as usize);
    let runs = h.worst as usize;
    let off = OWNER_RUN_OFF + h.total as usize;
    if runs >= OWNER_MAX_RUNS {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let mut m = 0usize;
    let mut i = 0usize;
    while i < n {
        let first = t.record(i);
        let mut count = first.count(cs);
        let mut j = i + 1;
        while j < n && words(t.record(j).as_ptr()) == words(first.as_ptr()) {
            count += t.record(j).count(cs);
            j += 1;
        }
        if h.groups != GROUPS_UNCOUNTED {
            h.groups += 1;
        }
        if count >= sp.floor {
            if j > i + 1 {
                let mut best = [MINCODE_NONE; MAX_REPS];
                for (mi, &(slot, lane)) in sp.mins.iter().enumerate().filter(|(_, e)| e.0 != REP_NONE) {
                    let so = sp.layout.slot_off(slot as usize) as usize;
                    let mut bytes: Option<&[u8]> = None;
                    for k in i..j {
                        let r = t.record(k);
                        if !r.valid(slot as usize) {
                            continue;
                        }
                        let w = *(r.as_ptr().add(so) as *const u64);
                        let Ok(Some(p)) = render(site, lane, w) else { return ERROR };
                        let v = varlena_bytes(p);
                        if bytes.is_none_or(|b| v < b) {
                            (bytes, best[mi]) = (Some(v), w);
                        }
                    }
                }
                for k in i + 1..j {
                    if first.fold(&t.record(k)).is_err() {
                        return FloatOverflow::STATUS;
                    }
                }
                for (mi, &(slot, _)) in sp.mins.iter().enumerate().filter(|(_, e)| e.0 != REP_NONE) {
                    if best[mi] != MINCODE_NONE {
                        *(first.as_ptr().add(sp.layout.slot_off(slot as usize) as usize) as *mut u64) = best[mi];
                    }
                }
            }
            if g < sp.layout.nkeys() {
                first.set_key(g, 0);
            }
            if off + (m + 1) * rec > h.table_off as usize {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
            core::ptr::copy_nonoverlapping(first.as_ptr(), owner.add(off + m * rec), rec);
            m += 1;
        }
        i = j;
    }
    if m > 0 {
        let dir = owner.add(OWNERTOP_HDR) as *mut u32;
        *dir.add(2 * runs) = off as u32;
        *dir.add(2 * runs + 1) = m as u32;
        h.worst = runs as u32 + 1;
        h.total += (m * rec) as u64;
        h.len += m as u32;
    }
    0
}

/// The floor's pigeonhole prune (V4R-173, L-FLOORPRUNE): the table is keyed by (group words, part word), so a group
/// holds at most one record per part — every fold into it (`fold_run`, the shape folds) merges a pair's records into
/// one — and at most `P = max part word + 1` records. A group whose records sum to `floor` or more therefore has one
/// record of `ceil(floor / P)` or more. The first sweep finds `P` and the largest record count: when no record reaches
/// the bound, no group can reach the floor (`Nothing`: the table is not touched again). Else the groups with a record at
/// the bound are collected in one sweep (any number and key width: `GroupSet`, grown as it fills, filtered by one word),
/// and the table is compacted in place to their records alone (`Pruned`: `DENSE`, the rest cleared): the groups dropped
/// are ones the floor rejects, and each kept group keeps every record. `Kept`: nothing pruned (the bound is 1, or no
/// memory for the set), the table as it was.
unsafe fn prune_below_floor(sp: &PartHavingSpec, t: &mut HashAgg, g: usize) -> Prune {
    if g == 0 {
        return Prune::Kept;
    }
    let (cap, cs) = (t.cap as usize, sp.count_slot as usize);
    let (mut pmax, mut cmax) = (0u64, i64::MIN);
    for i in 0..cap {
        let r = t.record(i);
        if r.used() {
            pmax = pmax.max(r.key(g));
            cmax = cmax.max(r.count(cs));
        }
    }
    let thr = (sp.floor as u64).div_ceil(pmax.saturating_add(1));
    if thr <= 1 {
        return Prune::Kept;
    }
    if cmax < thr as i64 {
        return Prune::Nothing;
    }
    let words = |p: *const u8| core::slice::from_raw_parts(p as *const u64, g);
    // one pass collects the groups with a record at the bound into a set that grows as it fills (no pass to size it first)
    let mut set = GroupSet::new(g);
    for i in 0..cap {
        let r = t.record(i);
        if r.used() && r.count(cs) >= thr as i64 && !set.insert(words(r.as_ptr())) {
            return Prune::Kept;
        }
    }
    let mut m = 0usize;
    for i in 0..cap {
        let r = t.record(i);
        if !r.used() {
            continue;
        }
        if set.contains(words(r.as_ptr())) {
            if i != m {
                t.record(m).copy_from(&r);
                r.clear();
            }
            m += 1;
        } else {
            r.clear();
        }
    }
    t.len = m as u32;
    t.mode = DENSE;
    Prune::Pruned
}

/// What the floor prune did to the table.
enum Prune {
    /// Nothing: every record stands.
    Kept,
    /// Compacted to the groups that can reach the floor.
    Pruned,
    /// No group can reach the floor; the table is untouched.
    Nothing,
}

/// A record without a part word under a floor (the two-pass form's pass 1): the fold left one record per group, so the groups at the floor are copied
/// out of the table as it stands and only THEY are sorted into the run — the table is neither compacted nor sorted (most groups are under the floor).
unsafe fn keep_whole(sp: &PartHavingSpec, h: &mut OwnerTop, t: &mut HashAgg, owner: *mut u8) -> i64 {
    let (g, cs, rec) = (sp.layout.partition_keys(), sp.count_slot as usize, sp.layout.size() as usize);
    let words = |p: *const u8| core::slice::from_raw_parts(p as *const u64, g);
    let runs = h.worst as usize;
    let off = OWNER_RUN_OFF + h.total as usize;
    if runs >= OWNER_MAX_RUNS || rec > 256 {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    h.groups += t.len as u64;
    let run = owner.add(off);
    let mut m = 0usize;
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if !r.used() || r.count(cs) < sp.floor {
            continue;
        }
        if off + (m + 1) * rec > h.table_off as usize {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        core::ptr::copy_nonoverlapping(r.as_ptr() as *const u8, run.add(m * rec), rec);
        m += 1;
    }
    // the kept records by their group words (a heap sort in place: the run's order is the sorted table's)
    let mut tmp = [0u8; 256];
    let at = |i: usize| run.add(i * rec);
    let mut swap = |a: usize, b: usize| {
        core::ptr::copy_nonoverlapping(at(a), tmp.as_mut_ptr(), rec);
        core::ptr::copy_nonoverlapping(at(b), at(a), rec);
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), at(b), rec);
    };
    let sift = |mut root: usize, end: usize, swap: &mut dyn FnMut(usize, usize)| loop {
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && words(at(child)) < words(at(child + 1)) {
            child += 1;
        }
        if words(at(root)) >= words(at(child)) {
            return;
        }
        swap(root, child);
        root = child;
    };
    for start in (0..m / 2).rev() {
        sift(start, m, &mut swap);
    }
    for end in (1..m).rev() {
        swap(0, end);
        sift(0, end, &mut swap);
    }
    if m > 0 {
        let dir = owner.add(OWNERTOP_HDR) as *mut u32;
        *dir.add(2 * runs) = off as u32;
        *dir.add(2 * runs + 1) = m as u32;
        h.worst = runs as u32 + 1;
        h.total += (m * rec) as u64;
        h.len += m as u32;
    }
    0
}
