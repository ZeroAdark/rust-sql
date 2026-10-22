//! EXPLAIN ANALYZE of a bounded incremental sort (runs.topk_rows' exact stop): PostgreSQL's IncrementalSortInfo, its
//! Full-sort and Pre-sorted Groups, replayed over the rows the top reads. nodeIncrementalSort.c's batches follow the
//! input's prefix groups alone: a full-sort load takes `min(32, bound left)` rows, then rows of its pivot's prefix; past
//! 64 rows it sorts what it holds, hands each earlier prefix group on as its own presorted batch and reads the pivot's
//! group on in a presorted load; each sorted batch is instrumented (instrumentSortedGroup). Each batch's space is
//! tuplesort's accounting of its state (made once, reset per batch): the SortTuple array charged when the state is made
//! or regrown, each tuple's chunk, a bounded heap past `2 bound` tuples keeping `bound`; the space a batch reports is its
//! state's most so far (tuplesort_updatemax). The replay runs with no memory limit and keeps the most it would have
//! needed (`need`): exec reads the groups only where that fits work_mem (no spill, no capped array growth), else absent.
//! The state is the top's frame words (`WORDS`); the IR's host calls (raw.rs `incsort_replay_row` / `_end`) drive it and
//! the top's finish adds `agg`, `need` and `bad` into the node's counter cells.

use super::inc_batches::{PIVOT_ROW, PREFIX_LOAD_PAST};

/// PostgreSQL's DEFAULT_MIN_GROUP_SIZE and DEFAULT_MAX_FULL_SORT_GROUP_SIZE: the unbounded batches' own rows.
const PIVOT: u64 = PIVOT_ROW as u64;
const LOAD_PAST: u64 = PREFIX_LOAD_PAST as u64;
/// tuplesort.c's INITIAL_MEMTUPSIZE and sizeof(SortTuple).
const ARRAY: u64 = 1024;
const SORT_TUPLE: u64 = 24;
/// tuplesort_set_bound's limit: a larger bound is not set.
const BOUND_MAX: u64 = (i32::MAX / 2) as u64;
/// TuplesortMethod bits (top-N heapsort, quicksort).
const TOP_N: u64 = 1;
const QUICK: u64 = 2;

const START: u64 = 0;
const FULL: u64 = 1;
const PREFIX: u64 = 2;
const STOPPED: u64 = 3;
const ENDED: u64 = 4;

/// A row's flag (`row`'s `flags`): its prefix differs from the row before's.
pub(crate) const CHANGED: u64 = 1;
/// A row's flag: the top's walk ends at this row (the stop).
pub(crate) const LAST: u64 = 2;

/// One tuplesort state's accounting.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ts {
    made: u64,
    size: u64,
    used: i64,
    count: u64,
    bound: u64,
    heap: u64,
    max: i64,
}

/// An IncrementalSortGroupInfo's counts: groups, sort method bits, total and peak space (kB).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Agg {
    pub groups: u64,
    pub methods: u64,
    pub total: u64,
    pub peak: u64,
}

#[repr(C)]
pub(crate) struct Replay {
    /// Set at the open: the top's bound (count + offset; `i64::MAX`: none) and a tuple's `t_len`.
    pub bound: u64,
    pub t_len: u64,
    phase: u64,
    /// bound_Done, and the rows handed on.
    done: u64,
    returned: u64,
    /// A full-sort load's rows, min group size and the bound set on its state (0: none); a presorted load's tuples, its
    /// call's nTuples (bound_Done's step) and its state's bound.
    n: u64,
    m: u64,
    fbound: u64,
    pn: u64,
    local: u64,
    pbound: u64,
    /// The full-sort load's prefix groups (sizes, at most 65 rows): `glist[head..head + ngroups]`.
    head: u64,
    ngroups: u64,
    ts: [Ts; 2],
    /// Full-sort, then Pre-sorted.
    pub agg: [Agg; 2],
    /// The most bytes the replay's accounting needed of work_mem.
    pub need: u64,
    /// The rows the top read are not PostgreSQL's (a row past its stop, a walk ending where it would read on).
    pub bad: u64,
    glist: [u8; 72],
}

/// The state's words.
pub(crate) const WORDS: usize = core::mem::size_of::<Replay>() / 8;
const _: () = assert!(core::mem::size_of::<Replay>() % 8 == 0 && WORDS < 256);
/// The words the top's finish adds into its cells: `agg`, then `need` and `bad`.
pub(crate) const AGG_WORD: usize = core::mem::offset_of!(Replay, agg) / 8;
pub(crate) const STAT_WORDS: usize = 10;
const _: () = assert!(core::mem::offset_of!(Replay, bad) / 8 == AGG_WORD + STAT_WORDS - 1);

fn maxalign(n: u64) -> u64 {
    n.div_ceil(8) * 8
}

/// GetMemoryChunkSpace of a palloc of `size` bytes in an AllocSet of the default sizes.
fn allocset_space(size: u64) -> u64 {
    (if size <= 8192 { size.max(8).next_power_of_two() } else { maxalign(size) }) + 8
}

fn array(size: u64) -> i64 {
    allocset_space(size * SORT_TUPLE) as i64
}

impl Replay {
    fn bounded(&self) -> bool {
        self.bound < i64::MAX as u64
    }

    /// A tuple's charge: its AllocSet chunk under a bound (TUPLESORT_ALLOWBOUNDED), else its MAXALIGNed length (bump).
    fn charge(&self) -> i64 {
        (if self.bounded() { allocset_space(self.t_len) } else { maxalign(self.t_len) }) as i64
    }

    fn left(&self) -> u64 {
        self.bound.saturating_sub(self.done)
    }

    /// One row the top read (`flags`: `CHANGED`, `LAST`).
    pub(crate) fn row(&mut self, flags: u64) {
        let changed = flags & CHANGED != 0;
        match self.phase {
            START if self.bound == 0 => self.phase = STOPPED,
            START => {
                self.begin_full();
                self.put_full(true);
            }
            FULL => self.full_row(changed),
            PREFIX => self.prefix_row(changed),
            _ => self.bad = 1,
        }
        if flags & LAST != 0 && self.phase != STOPPED {
            self.bad = 1;
        }
    }

    /// The top's input ended (or its walk stopped): a load still reading meets PostgreSQL's end of input.
    pub(crate) fn end(&mut self) {
        match self.phase {
            START if self.bound > 0 => {
                self.begin_full();
                self.sorted(0);
            }
            FULL => self.sorted(0),
            PREFIX => self.sorted(1),
            _ => {}
        }
        self.phase = ENDED;
    }

    fn begin_full(&mut self) {
        self.begin(0);
        let left = self.left();
        self.fbound = if self.bounded() && left < PIVOT { left } else { 0 };
        self.set_bound(0, self.fbound);
        self.m = if self.bounded() { PIVOT.min(left) } else { PIVOT };
        self.n = 0;
        self.head = 0;
        self.ngroups = 0;
        self.phase = FULL;
    }

    fn put_full(&mut self, new_group: bool) {
        self.put(0);
        self.n += 1;
        let last = (self.head + self.ngroups) as usize;
        if new_group || self.ngroups == 0 {
            match self.glist.get_mut(last) {
                Some(g) => *g = 1,
                None => self.bad = 1,
            }
            self.ngroups += 1;
        } else if let Some(g) = self.glist.get_mut(last - 1) {
            *g = g.saturating_add(1);
        }
    }

    fn full_row(&mut self, changed: bool) {
        if self.n < self.m || !changed {
            self.put_full(changed);
            if self.n > LOAD_PAST {
                self.switch();
            }
            return;
        }
        // a row past the pivot's prefix: the batch sorted, the row carried into the next
        self.sorted(0);
        self.done = self.bound.min(self.done + self.n);
        self.handed(0, self.fbound, self.n);
        if self.phase != STOPPED {
            self.begin_full();
            self.put_full(true);
        }
    }

    fn prefix_row(&mut self, changed: bool) {
        if !changed {
            self.put(1);
            self.pn += 1;
            self.local += 1;
            return;
        }
        self.sorted(1);
        self.done = self.bound.min(self.done + self.local);
        self.handed(1, self.pbound, self.pn);
        if self.phase != STOPPED {
            self.begin_full();
            self.put_full(true);
        }
    }

    /// Past 64 rows of one prefix: the full-sort batch sorted (a bounded heap keeps the bound left), then each prefix group
    /// it holds moved to the presorted state (switchToPresortedPrefixMode), all but the last sorted and handed on as they
    /// move, the last read on.
    fn switch(&mut self) {
        self.sorted(0);
        let mut keep = self.n;
        if self.ts[0].heap != 0 {
            keep = keep.min(self.left());
        }
        self.truncate(keep);
        self.local = keep;
        let mut first = true;
        while self.ngroups > 0 && self.phase != STOPPED {
            self.begin(1);
            self.pbound = if self.bounded() { self.left() } else { 0 };
            self.set_bound(1, self.pbound);
            let g = self.glist.get(self.head as usize).map_or(0, |&g| u64::from(g));
            self.head += 1;
            self.ngroups -= 1;
            for _ in 0..g {
                self.put(1);
            }
            self.pn = g;
            if self.ngroups == 0 {
                if !first {
                    self.local = 0;
                }
                self.phase = PREFIX;
                return;
            }
            self.sorted(1);
            self.done = self.bound.min(self.done + g);
            self.handed(1, self.pbound, g);
            first = false;
        }
    }

    /// The batch's first `keep` rows (the full-sort state's sorted order: its prefix groups in order).
    fn truncate(&mut self, keep: u64) {
        let mut left = keep;
        for i in 0..self.ngroups {
            let at = (self.head + i) as usize;
            let g = u64::from(self.glist[at]);
            if g >= left {
                self.glist[at] = left as u8;
                self.ngroups = i + u64::from(left > 0);
                return;
            }
            left -= g;
        }
    }

    /// The rows a sorted batch hands on (a bounded heap's `bound`), and PostgreSQL's Limit satisfied.
    fn handed(&mut self, i: usize, bound: u64, n: u64) {
        self.returned += if self.ts[i].heap != 0 { bound } else { n };
        if self.returned >= self.bound {
            self.phase = STOPPED;
        }
    }

    /// tuplesort_begin_heap's first batch, else tuplesort_reset: the array charged only when made or regrown.
    fn begin(&mut self, i: usize) {
        let t = &mut self.ts[i];
        if t.made == 0 {
            *t = Ts { made: 1, size: ARRAY, used: array(ARRAY), count: 0, bound: 0, heap: 0, max: 0 };
            return;
        }
        t.used = 0;
        if t.size != ARRAY {
            t.size = ARRAY;
            t.used = array(ARRAY);
        }
        t.count = 0;
        t.bound = 0;
        t.heap = 0;
    }

    fn set_bound(&mut self, i: usize, bound: u64) {
        if self.bounded() && bound == 0 && self.phase != STOPPED && i == 1 {
            self.bad = 1;
        }
        self.ts[i].bound = if bound > BOUND_MAX { 0 } else { bound };
    }

    /// tuplesort_puttuple_common: the tuple charged; the array grown at one slot short (doubled: work_mem permitting,
    /// which `need` records); the bounded heap made past `2 bound` tuples, after which each tuple costs nothing kept.
    fn put(&mut self, i: usize) {
        let c = self.charge();
        let t = &mut self.ts[i];
        if t.heap != 0 {
            return;
        }
        t.used += c;
        if t.count + 1 >= t.size {
            let used = t.used.max(0) as u64;
            self.need = self.need.max(2 * used).max(used + t.size * SORT_TUPLE);
            t.used += array(t.size * 2) - array(t.size);
            t.size *= 2;
        }
        t.count += 1;
        self.need = self.need.max(t.used.max(0) as u64);
        if t.bound > 0 && t.count > 2 * t.bound {
            t.used -= (t.count - t.bound) as i64 * c;
            t.count = t.bound;
            t.heap = 1;
        }
    }

    /// tuplesort_performsort and the batch's instrumentation (tuplesort_get_stats: the state's most space so far).
    fn sorted(&mut self, i: usize) {
        let t = &mut self.ts[i];
        t.max = t.max.max(t.used);
        let kb = (t.max.max(0) as u64).div_ceil(1024);
        let method = if t.heap != 0 { TOP_N } else { QUICK };
        let a = &mut self.agg[i];
        a.groups += 1;
        a.methods |= method;
        a.total += kb;
        a.peak = a.peak.max(kb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay(bound: u64, groups: &[u64], stop: bool) -> Replay {
        // the open's state: every word 0 but the bound and the tuple length ((int4, int4): 24)
        let mut r = Replay { bound, t_len: 24, phase: 0, done: 0, returned: 0, n: 0, m: 0, fbound: 0, pn: 0, local: 0, pbound: 0, head: 0, ngroups: 0, ts: [Ts::default(); 2], agg: [Agg::default(); 2], need: 0, bad: 0, glist: [0; 72] };
        let mut first = true;
        for &g in groups {
            for j in 0..g {
                r.row(if j == 0 && !first { CHANGED } else { 0 });
            }
            first = false;
        }
        if stop {
            r.row(CHANGED | LAST);
        }
        r.end();
        r
    }

    /// incremental_sort#28: 1000 rows in prefix groups of 50, LIMIT 55: Full-sort Groups: 2, top-N heapsort and
    /// quicksort, 26 kB each (PostgreSQL 18.6).
    #[test]
    fn two_full_sort_groups() {
        let r = replay(55, &[50, 50], true);
        assert_eq!(r.bad, 0);
        assert_eq!((r.agg[0].groups, r.agg[0].methods, r.agg[0].total / r.agg[0].groups, r.agg[0].peak), (2, TOP_N | QUICK, 26, 26));
        assert_eq!(r.agg[1].groups, 0);
    }

    /// incremental_sort#46: groups 1, 1, 1, 1 then 996, LIMIT 70: Full-sort Groups: 1 quicksort 27 kB; Pre-sorted
    /// Groups: 5, top-N heapsort and quicksort, 25 kB average and peak (PostgreSQL 18.6).
    #[test]
    fn presorted_groups() {
        let r = replay(70, &[1, 1, 1, 1, 996], false);
        assert_eq!(r.bad, 0);
        assert_eq!((r.agg[0].groups, r.agg[0].methods, r.agg[0].total, r.agg[0].peak), (1, QUICK, 27, 27));
        assert_eq!((r.agg[1].groups, r.agg[1].methods, r.agg[1].total / r.agg[1].groups, r.agg[1].peak), (5, TOP_N | QUICK, 25, 25));
    }

    /// An input that ends before the bound: the last load meets the end of input.
    #[test]
    fn end_of_input() {
        let r = replay(55, &[10], false);
        assert_eq!((r.agg[0].groups, r.agg[1].groups, r.bad), (1, 0, 0));
    }

    /// A walk stopping where PostgreSQL would read on is not PostgreSQL's.
    #[test]
    fn early_stop_is_bad() {
        let r = replay(55, &[10], true);
        assert_eq!(r.bad, 1);
    }
}
