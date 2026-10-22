use crate::layout::{unword, word, Word};
use crate::ctx::{Answer, Datum, Lane, ScanPos};

pub type NextFn = Option<unsafe extern "C" fn(*mut Site) -> i64>;
pub type LaneFn = Option<unsafe extern "C" fn(*mut Site, u32) -> i64>;
/// `dict(site, part, lane, code, out)`: the datum behind a code (a varlena image resident with the part) into `out`; `< 0` a host error.
pub type DictFn = Option<unsafe extern "C" fn(*mut Site, u32, u32, u32, *mut u64) -> i64>;

crate::block! {
    /// One row-entry point of the program (abi-v2 §2.2, `docs/gravity/abi.md`): the window itself,
    /// written in place by the source behind `next`; `sel` through `answered` is zeroed per Call.
    Site size 192 offsets site {
        values => VALUES: Word<*mut Datum>,
        isnull => ISNULL: Word<*mut u8>,
        keys => KEYS: Word<*mut Datum>,
        keys_null => KEYS_NULL: Word<*mut u8>,
        lanes => LANES: Word<*mut Lane>,
        next => NEXT: Word<NextFn>,
        lane => LANE: Word<LaneFn>,
        fetch => FETCH: Word<LaneFn>,
        src => SRC: Word<*mut u8>,
        env => ENV: Word<*mut u8>,
        sel => SEL: Word<*const u32>,
        answer => ANSWER: Word<*const Answer>,
        rows_in => ROWS_IN: u64,
        rows_out => ROWS_OUT: u64,
        cont => CONT: u64,
        state => STATE: u32,
        rows => ROWS: u32,
        cursor => CURSOR: u32,
        sel_n => SEL_N: u32,
        part_id => PART_ID: u32,
        dict_epoch => DICT_EPOCH: u32,
        granule => GRANULE: u32,
        claim_id => CLAIM_ID: u32,
        pos => POS: ScanPos,
        nlanes => NLANES: u16,
        answered => ANSWERED: u8,
        part_facts => PART_FACTS: u8,
        nkeys => NKEYS: u16,
        natts => NATTS: u16,
        page_flags => PAGE_FLAGS: u16,
        lanes_cap => LANES_CAP: u16,
        dict => DICT: Word<DictFn>,
        page => PAGE: Word<*const u8>,
    }
    extra {
        pub const POS_FLAGS: usize = POS;
        pub const POS_OFF: usize = POS + 2;
        pub const POS_REL: usize = POS + 4;
        pub const POS_BLOCK: usize = POS + 8;
        pub const WINDOW_CELLS: (usize, usize) = (SEL, NKEYS - SEL);
        pub const STATE_CLOSED: u32 = 0;
        pub const STATE_OPEN: u32 = 1;
        pub const STATE_EOF: u32 = 2;
        /// `page_flags` of a raw-page window (the heap source's page protocol): no visible tuple has a null bitmap; every visible tuple carries the recipe's attributes.
        pub const PAGE_NONULLS: u16 = 1;
        pub const PAGE_FULL: u16 = 2;
        /// V4R-371 (D1001): an all-visible page answered from its line pointers alone: no header read, so neither flag above is known and the IR tests each row's header.
        pub const PAGE_UNREAD: u16 = 8;
        /// V4R-371: every line pointer of the page is a visible tuple (the offsets are exactly `1..=rows`): a row's line pointer is its index's.
        pub const PAGE_IOTA: u16 = 16;
        /// A bitmap morsel window whose page needs `bitmapqualorig` (a lossy page, or an exact page its AM marked
        /// for recheck); the morsel form's recheck filter runs on such windows only (lowerv4 step/filter.rs `guard`).
        pub const PAGE_RECHECK: u16 = 32;
        /// `part_facts` of a window whose part's dictionary entries each stand for a row the window's scan delivers (the
        /// host's `entries_safe`: no delete vector, no transaction snapshot; pgrc seals a dictionary from its part's own
        /// values): a code lane's `n` least codes are held by at least `n` of the part's rows. 0 on every other window.
        pub const PART_ENTRIES_SAFE: u8 = 1;
    }
}

pub const SITE_SIZE: usize = site::SIZE;

impl Site {
    #[inline(always)]
    pub const fn end(&self) -> u32 {
        if unword(self.sel).is_null() {
            self.rows
        } else {
            self.sel_n
        }
    }

    /// # Safety — `j < end()`.
    #[inline(always)]
    pub unsafe fn index(&self, j: u32) -> usize {
        if unword(self.sel).is_null() {
            j as usize
        } else {
            *unword(self.sel).add(j as usize) as usize
        }
    }

    /// # Safety — `lanes` points at the site's lane array.
    #[inline(always)]
    pub unsafe fn lanes(&self) -> &[Lane] {
        core::slice::from_raw_parts(unword(self.lanes) as *const Lane, self.nlanes as usize)
    }
}

/// # Safety — `s` points at a bound site.
#[inline(always)]
pub unsafe fn lane<'a>(s: *const Site, k: usize) -> &'a Lane {
    debug_assert!((k as u16) < (*s).nlanes, "lane {k} of {}", (*s).nlanes);
    &*unword((*s).lanes).add(k)
}

/// D1340: table lane `k` (a dictionary / fingerprint / program table the source fills on demand through `lane_fill`). A table
/// lane lies past the window's data lanes (`nlanes`, which `Window::over` resets and `Window::set` grows) but inside the lane
/// array the driver's node bind sized by the descriptor (`lanes_cap`, driver node.rs), so it is bounded by that, not `nlanes`.
///
/// # Safety — `s` points at a bound site; lane `k` was filled by `lane_fill` for the current part.
#[inline(always)]
pub unsafe fn table_lane<'a>(s: *const Site, k: usize) -> &'a Lane {
    debug_assert!((k as u16) < (*s).lanes_cap, "table lane {k} of {}", (*s).lanes_cap);
    &*unword((*s).lanes).add(k)
}

/// # Safety — `s` points at a bound site.
#[inline(always)]
pub unsafe fn next(s: *mut Site) -> i64 {
    (unword((*s).next).unwrap_unchecked())(s)
}

/// # Safety — `s` points at a bound site with a current window.
#[inline(always)]
pub unsafe fn lane_fill(s: *mut Site, k: u32) -> i64 {
    (unword((*s).lane).unwrap_unchecked())(s, k)
}

/// `lane_fill`'s result for a `table_fn::PROGRAM` lane the source allocated but the program has not swept yet; `lane_fill(k | LANE_DONE)` publishes it
/// (the flag rides inside the driver's `u16` lane index: a site has fewer than 32,768 lanes).
pub const LANE_UNFILLED: i64 = 2;
pub const LANE_DONE: u32 = 1 << 15;
/// D3115: `lane_fill(site, k | LANE_CLAIM)` on an unfilled `PROGRAM` table lane k: the first code of the next `SWEEP_CHUNK`
/// codes this source sweeps (the statement's sources share one sweep of a part's table), or the table's code count when every
/// chunk is claimed. Rides beside `LANE_DONE` in the `u16` lane index (a site has fewer than 16,384 lanes).
pub const LANE_CLAIM: u32 = 1 << 14;
/// D3115: the codes one `LANE_CLAIM` hands out: a multiple of 64, so two sources never write the same bit-table or
/// fault-bitmap word.
pub const SWEEP_CHUNK: u64 = 4096;
/// D3115: `LANE_CLAIM`'s answer while every chunk is claimed and another source still sweeps one (the source slept a
/// moment first): the caller checks the cancel byte and claims again.
pub const CLAIM_WAIT: i64 = u32::MAX as i64;
/// `lane_fill(site, k | LANE_PACKED)` on site lane k (below the table lanes, so `LANE_DONE`'s bit is free there): 0 = the
/// window is not packed, else the byte offset of lane k's `PackedFrames` from the lane's `ptr`.
pub const LANE_PACKED: u32 = LANE_DONE;

/// One lane's frames in a packed window: `n` frames, frame f holding rows `[1024 f, min(1024 (f + 1), rows))`.
#[repr(C)]
pub struct PackedFrames {
    pub n: u64,
    pub frames: [PackedFrame; PACKED_FRAMES],
}

/// The frames of a packed window: eight of 1024 rows each.
pub const PACKED_FRAMES: usize = 8;

/// Row r's value is `reference + delta(r)`, `delta(r)` the `width`-byte unsigned LE word at `deltas + (r - first_row) *
/// width` (width 0: no delta, `deltas` 0), exact as an i64 compare; `reference` and `fmax` are the lane's datum words as
/// its stream extends them (sign-extended; zero-extended for an unsigned stream: oid), `fmax` at least every valid row's
/// value. A NULL row's delta is 0.
#[repr(C)]
pub struct PackedFrame {
    pub deltas: u64,
    pub width: u64,
    pub reference: i64,
    pub fmax: i64,
    pub first_row: u64,
}

/// The most rows one `fetch(site, nrows)` may request: a fetch site publishes this many rows a request. The fetch emit
/// asks its winners this many at a time, so it bounds no top (CAP-25: lowerv4 ops/sort/strategies/fetch.rs `FetchEmit`;
/// fetch_shape.rs prices any bound).
pub const MAX_FETCH_ROWS: usize = 1024;

/// A selection mask word (64 rows) with at most this many kept rows gathers those rows alone, a walk of its set bits; a
/// fuller word takes the dense 64-row form (gravity_dict table.rs `gather_mask`). It moves no answer. CHOSEN, not swept.
pub const SPARSE_GATHER_MAX: u32 = 16;

/// # Safety — `s` points at a bound site with a current window.
#[inline(always)]
pub unsafe fn fetch(s: *mut Site, nrows: u32) -> i64 {
    (unword((*s).fetch).unwrap_unchecked())(s, nrows)
}

/// The value behind dictionary code `code` of code lane `lane` in part `part` (an emit's render of
/// its winners, never a per-row call).
/// # Safety — `s` points at a bound site of a source with dictionaries.
#[inline(always)]
pub unsafe fn dict_value(s: *mut Site, part: u32, lane: u32, code: u32, out: *mut u64) -> i64 {
    (unword((*s).dict).unwrap_unchecked())(s, part, lane, code, out)
}

/// # Safety — `s` points at a bound site.
#[inline(always)]
pub unsafe fn seek(s: *mut Site) {
    (*s).state = site::STATE_CLOSED;
    (*s).rows = 0;
    (*s).cursor = 0;
    (*s).sel = word(core::ptr::null());
    (*s).sel_n = 0;
}

/// The row protocol over the window: the next row into `values` / `isnull` (`natts` lanes), `pos`
/// moved to the row's; 1 for a row, 0 at the end (state `EOF`), a status below zero.
/// # Safety — `s` points at a bound site whose `values` / `isnull` hold `natts` cells.
pub unsafe fn next_row(s: *mut Site, natts: usize) -> i64 {
    loop {
        let st = &mut *s;
        if st.cursor < st.end() {
            let i = st.index(st.cursor);
            if st.answered != 0 {
                return crate::status::unsupported(crate::status::UNSUPPORTED_SHAPE);
            }
            let mut off_lane: *const Lane = core::ptr::null();
            for k in 0..st.nlanes as usize {
                let l = &*unword(st.lanes).add(k);
                if k < natts {
                    if unword(l.ptr).is_null() {
                        return crate::status::unsupported(crate::status::UNSUPPORTED_SHAPE);
                    }
                    *unword(st.values).add(k) = l.datum(i);
                    *unword(st.isnull).add(k) = !l.is_valid(i) as u8;
                } else if crate::lane::id_kind(l.id) == crate::lane::ID_OFFSET && !unword(l.ptr).is_null() {
                    off_lane = l;
                }
            }
            if !st.pos.is_row() {
                st.pos.off = if off_lane.is_null() { i as u16 + 1 } else { *(unword((*off_lane).ptr) as *const u16).add(i) };
            }
            st.cursor += 1;
            return 1;
        }
        let n = next(s);
        if n < 0 {
            return n;
        }
        if n == 0 {
            (*s).state = site::STATE_EOF;
            return 0;
        }
    }
}
