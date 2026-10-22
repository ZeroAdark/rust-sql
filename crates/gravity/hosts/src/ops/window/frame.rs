//! The window object's positions (a `#[path]` child of hosts v4/m10.rs): nodeWindowAgg.c's partition, peer and frame
//! positions over the stored rows. `begin_partition`, `are_peers`, `update_frameheadpos` / `update_frametailpos` /
//! `update_grouptailpos`, `row_is_in_frame` and `calculate_frame_offsets`, with every position relative to the
//! partition's first row as PG keeps them. The partition is spooled as PG's `spool_tuples` spools it (exact request,
//! PLAN-v5 §2.5; window W4): `begin_partition` takes its first row only, and a position is read only after `spool` has
//! compared the rows up to it with the partition's first (the row past the partition's end compared too, as PG reads it
//! into `first_part_slot`), so the rows a frame, an offset or a partition count reaches are the rows PG reads, no more.
//! Created by M10-WINDOW.
use datum::Datum;
use mcx::MemoryContext;
use types_core::Oid;
use types_error::PgResult;
use types_fmgr::FmgrInfo;
use types_nodes::rawnodes::*;

use crate::ops::window::object::{bad, Rt, WinSpec};

/// One equality column of the partition or the order keys: its cell, its `=` function and collation.
struct Key {
    col: usize,
    f: FmgrInfo,
    coll: Oid,
}

pub(crate) struct Frame {
    pub(crate) fo: i32,
    part: Vec<Key>, // alloc: Exec
    ord: Vec<Key>, // alloc: Exec
    /// The start / end in_range functions (RANGE with an offset), the order column and the comparison's options.
    in_range: [Option<FmgrInfo>; 2],
    sort_col: usize,
    in_coll: Oid,
    in_asc: bool,
    in_nulls_first: bool,
    /// The offsets `calculate_frame_offsets` kept (their by-reference copies in `keep`), once set; cleared at each open.
    pub(super) off: Option<[Datum; 2]>,
    pub(super) keep: MemoryContext,
    /// D1368: the partition / peer comparisons' context (PG's `tmpcontext`, ExecQualAndReset): a by-reference key's
    /// equality function (record_eq, array_eq) allocates as it compares; reset after every comparison.
    cmp: MemoryContext,
    /// The partition: its first stored row, its rows spooled so far (PG's `spooled_rows`), whether its end is found (PG's
    /// `partition_spooled`), and whether the next row begins a new one.
    pub(crate) pstart: u64,
    pub(crate) n: i64,
    pub(crate) spooled: bool,
    pub(crate) next_partition: bool,
    pub(crate) cur: i64,
    pub(crate) fh: i64,
    pub(crate) ft: i64,
    pub(crate) fh_valid: bool,
    pub(crate) ft_valid: bool,
    pub(crate) curgroup: i64,
    fhgroup: i64,
    ftgroup: i64,
    pub(crate) grouphead: i64,
    pub(crate) grouptail: i64,
    pub(crate) gt_valid: bool,
    /// prepare_tuplestore's read pointers: whether each row updates the frame head, tail and group tail.
    pub(crate) head_ptr: bool,
    pub(crate) tail_ptr: bool,
    pub(crate) gt_ptr: bool,
}

fn keys(n: i32, cols: &[i16], ops: &[Oid], colls: &[Oid]) -> PgResult<Vec<Key>> {
    let n = n.max(0) as usize;
    if cols.len() < n || ops.len() < n || colls.len() < n {
        return Err(bad("window.agg: key arrays shorter than their count"));
    }
    (0..n).map(|i| {
        let col = cols[i] as usize;
        if col == 0 {
            return Err(bad("window.agg: a key column 0"));
        }
        Ok(Key { col: col - 1, f: fmgr_core::fmgr_info(lsyscache::get_opcode(ops[i])?)?, coll: colls[i] })
    }).collect()
}

impl Frame {
    pub(crate) fn new(s: &WinSpec, root: &MemoryContext) -> PgResult<Frame> {
        let w = s.plan;
        let fo = w.frameOptions;
        let part = keys(w.partNumCols, w.partColIdx, w.partOperators, w.partCollations)?;
        let ord = keys(w.ordNumCols, w.ordColIdx, w.ordOperators, w.ordCollations)?;
        if part.iter().chain(ord.iter()).any(|k| k.col >= s.natts) {
            return Err(bad("window.agg: a key past the child's columns"));
        }
        let range_off = fo & FRAMEOPTION_RANGE != 0;
        let bind = |on: bool, f: Oid| -> PgResult<Option<FmgrInfo>> { if on { Ok(Some(fmgr_core::fmgr_info(f)?)) } else { Ok(None) } };
        let in_range = [bind(range_off && fo & FRAMEOPTION_START_OFFSET != 0, w.startInRangeFunc)?, bind(range_off && fo & FRAMEOPTION_END_OFFSET != 0, w.endInRangeFunc)?];
        let sort_col = ord.first().map_or(0, |k| k.col);
        let groups_or_range = fo & (FRAMEOPTION_RANGE | FRAMEOPTION_GROUPS) != 0;
        let has_ord = w.ordNumCols != 0;
        Ok(Frame {
            fo,
            part,
            ord,
            in_range,
            sort_col,
            in_coll: w.inRangeColl,
            in_asc: w.inRangeAsc,
            in_nulls_first: w.inRangeNullsFirst,
            off: None,
            keep: root.new_child_wholesale("gravity window offsets"),
            cmp: root.new_child_wholesale("gravity window key compare"),
            pstart: 0,
            n: 0,
            spooled: false,
            next_partition: true,
            cur: 0,
            fh: 0,
            ft: 0,
            fh_valid: false,
            ft_valid: false,
            curgroup: 0,
            fhgroup: 0,
            ftgroup: 0,
            grouphead: 0,
            grouptail: -1,
            gt_valid: false,
            head_ptr: groups_or_range && (fo & FRAMEOPTION_START_CURRENT_ROW != 0 && has_ord || fo & FRAMEOPTION_START_OFFSET != 0),
            tail_ptr: groups_or_range && (fo & FRAMEOPTION_END_CURRENT_ROW != 0 && has_ord || fo & FRAMEOPTION_END_OFFSET != 0),
            gt_ptr: fo & (FRAMEOPTION_EXCLUDE_GROUP | FRAMEOPTION_EXCLUDE_TIES) != 0 && has_ord,
        })
    }

    /// `m10_win_open` (ExecReScanWindowAgg's `all_first`, D2346): the next row begins a partition, and the next
    /// `m10_win_offsets` evaluates the offsets again.
    pub(crate) fn rewind(&mut self) {
        self.next_partition = true;
        self.off = None;
        self.keep.reset();
    }

    fn offset(&self, i: usize) -> PgResult<Datum> {
        self.off.map(|o| o[i]).ok_or_else(|| bad("window.agg: a frame offset read before m10_win_offsets"))
    }
}

impl Rt {
    /// Rows `a` and `b` (stored rows) equal on `keys` (ExecBuildGroupingEqual: two NULLs match, one NULL does not).
    fn keys_match(&mut self, part: bool, a: u64, b: u64) -> PgResult<bool> {
        // D1368: the key functions' first call reads the catalog (lookup_type_cache); window pipelines are leader-only (the
        // fill Ineligible{Sink}, the reader site-less): an eligible one must bind the keys catalog-free first.
        // D3336: a parted object's keys are catalog-free builtins (`object::pool_safe`), compared on its claim's thread
        debug_assert!(self.spec.parted || !crate::worker_arm::is_armed(), "window.agg: a key comparison on a pool thread (D1368)");
        let n = if part { self.f.part.len() } else { self.f.ord.len() };
        // bounded("key-columns: one compare per partition or order key of two rows; PG polls no deeper (a datum compare is one unit there too), at most MaxTupleAttributeNumber (1664) keys")
        for i in 0..n {
            let col = if part { self.f.part[i].col } else { self.f.ord[i].col };
            let (x, y) = (self.cell(a, col)?, self.cell(b, col)?);
            match (x.isnull, y.isnull) {
                (true, true) => continue,
                (true, false) | (false, true) => return Ok(false),
                _ => {}
            }
            let Frame { part: ps, ord: os, cmp, .. } = &mut self.f;
            let k = if part { &mut ps[i] } else { &mut os[i] };
            // D1368: the result context armed (a record / array equality allocates), reset after the call
            let eq = types_fmgr::function_call2_coll_in(&mut k.f, k.coll, cmp.mcx(), x.value, y.value);
            cmp.reset();
            if !eq?.as_bool() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// D3336 (win.partruns, `OP_SAMEPART`): whether the record at `rec` (the fill's `m10_win_put` layout: the cells'
    /// datum words, then their NULL bytes) is in the partition of the last stored row (ExecBuildGroupingEqual over the
    /// partition keys: two NULLs match, one NULL does not); true when no row is stored. Asked on the fill's thread (the
    /// leader: a window read by claims keeps its fill serial) before the record is stored.
    pub(crate) fn same_part(&mut self, rec: *const u64) -> PgResult<bool> {
        let n = self.store.borrow().nrows;
        if n == 0 {
            return Ok(true);
        }
        if rec.is_null() {
            return Err(bad("window.agg: a parted put with no record (D3336)"));
        }
        let ncols = self.spec.cols.len();
        // bounded("key-columns: one compare per partition key of the record and the last row, at most MaxTupleAttributeNumber (1664) keys")
        for i in 0..self.f.part.len() {
            let col = self.f.part[i].col;
            let x = self.cell(n - 1, col)?;
            let (w, nb) = crate::ops::window::raw::cell_at(rec, ncols, col);
            match (x.isnull, nb != 0) {
                (true, true) => continue,
                (true, false) | (false, true) => return Ok(false),
                _ => {}
            }
            let Frame { part, cmp, .. } = &mut self.f;
            let k = &mut part[i];
            let eq = types_fmgr::function_call2_coll_in(&mut k.f, k.coll, cmp.mcx(), x.value, Datum::from_u64(w));
            cmp.reset();
            if !eq?.as_bool() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// PG `are_peers` of partition positions `a` and `b` (no ORDER BY: every row is a peer).
    pub(crate) fn are_peers(&mut self, a: i64, b: i64) -> PgResult<bool> {
        if self.f.ord.is_empty() {
            return Ok(true);
        }
        let p = self.f.pstart;
        self.keys_match(false, p + a as u64, p + b as u64)
    }

    /// PG `begin_partition` at stored row `j`: the positions reset, the partition holding its first row only (`row`
    /// checked that row `j` is stored).
    pub(crate) fn begin_partition(&mut self, j: u64) {
        let f = &mut self.f;
        (f.pstart, f.n, f.spooled, f.next_partition, f.fh_valid, f.ft_valid, f.gt_valid) = (j, 1, false, false, false, false, false);
        (f.cur, f.fh, f.ft, f.curgroup, f.fhgroup, f.ftgroup, f.grouphead, f.grouptail) = (0, 0, 0, 0, 0, 0, 0, -1);
    }

    /// PG `spool_tuples(winstate, pos)`: the partition's rows up to position `pos` (every row for a negative `pos`) spooled,
    /// each stored row after the last spooled one compared with the partition's first; the first that differs, or the end
    /// of the stored rows, ends the partition.
    pub(crate) fn spool(&mut self, pos: i64) -> PgResult<()> {
        while !self.f.spooled && (pos < 0 || self.f.n <= pos) {
            cancelpoll::interrupts::check()?;
            let r = self.f.pstart + self.f.n as u64;
            let stored = self.store.borrow().nrows;
            if r >= stored {
                // the exact request (W4): a pulled fill stores one more row, else the partition (and the input) ended
                if !self.more()? {
                    self.f.spooled = true;
                }
            } else if !self.keys_match(true, self.f.pstart, r)? {
                self.f.spooled = true;
            } else {
                self.f.n += 1;
            }
        }
        Ok(())
    }

    /// PG `window_gettupleslot`'s test: partition position `pos` holds a row (spooled to it first). D3575: its
    /// CHECK_FOR_INTERRUPTS too, so a frame that restarts per row (O(n^2) row reads) is cancelled as PostgreSQL's is.
    pub(crate) fn has(&mut self, pos: i64) -> PgResult<bool> {
        cancelpoll::interrupts::check()?;
        if pos < 0 {
            return Ok(false);
        }
        self.spool(pos)?;
        Ok(pos < self.f.n)
    }

    /// PG `WinGetPartitionRowCount`: the partition spooled to its end, its rows.
    pub(crate) fn part_rows(&mut self) -> PgResult<i64> {
        self.spool(-1)?;
        Ok(self.f.n)
    }

    /// PG `ExecWindowAgg`'s test at stored row `j` (spooled to it first): `j` begins a new partition.
    pub(crate) fn begins(&mut self, j: u64) -> PgResult<bool> {
        if self.f.next_partition || j < self.f.pstart {
            return Ok(true);
        }
        let pos = (j - self.f.pstart) as i64;
        self.spool(pos)?;
        Ok(self.f.spooled && pos >= self.f.n)
    }

    /// in_range(bound row `pos`'s order value, the current row's, the offset, sub, less) of `which` (0 start, 1 end),
    /// with PG's NULL ordering first: `Some(result)` when both are non-NULL, else `None` with the NULL flags.
    fn in_range(&mut self, which: usize, pos: i64, sub: bool, less: bool) -> PgResult<Result<bool, (bool, bool)>> {
        let p = self.f.pstart;
        let (v, c) = (self.cell(p + pos as u64, self.f.sort_col)?, self.cell(p + self.f.cur as u64, self.f.sort_col)?);
        if v.isnull || c.isnull {
            return Ok(Err((v.isnull, c.isnull)));
        }
        let off = self.f.offset(which)?;
        let (coll, mcx) = (self.f.in_coll, self.row_cx.mcx());
        let Some(fi) = self.f.in_range[which].as_mut() else { return Err(bad("window.agg: a RANGE offset frame without its in_range function")) };
        Ok(Ok(types_fmgr::function_call5_coll_in(fi, coll, mcx, v.value, c.value, off, Datum::from_bool(sub), Datum::from_bool(less))?.as_bool()))
    }

    /// PG `update_frameheadpos`.
    pub(crate) fn update_frameheadpos(&mut self) -> PgResult<()> {
        if self.f.fh_valid {
            return Ok(());
        }
        let (fo, cur) = (self.f.fo, self.f.cur);
        if fo & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 {
            self.f.fh = 0;
        } else if fo & FRAMEOPTION_START_CURRENT_ROW != 0 {
            if fo & FRAMEOPTION_ROWS != 0 {
                self.f.fh = cur;
            } else if self.f.ord.is_empty() {
                self.f.fh = 0;
            } else {
                while self.has(self.f.fh)? && !self.are_peers(self.f.fh, cur)? {
                    self.f.fh += 1;
                }
            }
        } else if fo & FRAMEOPTION_START_OFFSET != 0 {
            if fo & FRAMEOPTION_ROWS != 0 {
                let mut off = self.f.offset(0)?.as_i64();
                if fo & FRAMEOPTION_START_OFFSET_PRECEDING != 0 {
                    off = -off;
                }
                let fh = cur.checked_add(off).unwrap_or(i64::MAX);
                self.f.fh = if fh < 0 { 0 } else if fh > cur + 1 { self.spool(fh - 1).map(|()| fh.min(self.f.n))? } else { fh };
            } else if fo & FRAMEOPTION_RANGE != 0 {
                let (mut sub, mut less) = (fo & FRAMEOPTION_START_OFFSET_PRECEDING != 0, false);
                if !self.f.in_asc {
                    (sub, less) = (!sub, true);
                }
                while self.has(self.f.fh)? {
                    let stop = match self.in_range(0, self.f.fh, sub, less)? {
                        Ok(r) => r,
                        Err((headnull, currnull)) => if self.f.in_nulls_first { !headnull || currnull } else { headnull || !currnull },
                    };
                    if stop {
                        break;
                    }
                    self.f.fh += 1;
                }
            } else {
                let off = self.f.offset(0)?.as_i64();
                let min = if fo & FRAMEOPTION_START_OFFSET_PRECEDING != 0 { self.f.curgroup - off } else { self.f.curgroup.checked_add(off).unwrap_or(i64::MAX) };
                while self.f.fhgroup < min && self.has(self.f.fh)? {
                    self.f.fh += 1;
                    if self.has(self.f.fh)? && !self.are_peers(self.f.fh - 1, self.f.fh)? {
                        self.f.fhgroup += 1;
                    }
                }
            }
        } else {
            return Err(bad("window.agg: a frame start of no known form"));
        }
        self.f.fh_valid = true;
        Ok(())
    }

    /// PG `update_frametailpos` (the frame's end, exclusive).
    pub(crate) fn update_frametailpos(&mut self) -> PgResult<()> {
        if self.f.ft_valid {
            return Ok(());
        }
        let (fo, cur) = (self.f.fo, self.f.cur);
        if fo & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 {
            self.f.ft = self.part_rows()?;
        } else if fo & FRAMEOPTION_END_CURRENT_ROW != 0 {
            if fo & FRAMEOPTION_ROWS != 0 {
                self.f.ft = cur + 1;
            } else if self.f.ord.is_empty() {
                self.f.ft = self.part_rows()?;
            } else {
                while self.has(self.f.ft)? && (self.f.ft <= cur || self.are_peers(self.f.ft, cur)?) {
                    self.f.ft += 1;
                }
            }
        } else if fo & FRAMEOPTION_END_OFFSET != 0 {
            if fo & FRAMEOPTION_ROWS != 0 {
                let mut off = self.f.offset(1)?.as_i64();
                if fo & FRAMEOPTION_END_OFFSET_PRECEDING != 0 {
                    off = -off;
                }
                let ft = cur.checked_add(off).and_then(|v| v.checked_add(1)).unwrap_or(i64::MAX);
                self.f.ft = if ft < 0 { 0 } else if ft > cur + 1 { self.spool(ft - 1).map(|()| ft.min(self.f.n))? } else { ft };
            } else if fo & FRAMEOPTION_RANGE != 0 {
                let (mut sub, mut less) = (fo & FRAMEOPTION_END_OFFSET_PRECEDING != 0, true);
                if !self.f.in_asc {
                    (sub, less) = (!sub, false);
                }
                while self.has(self.f.ft)? {
                    let stop = match self.in_range(1, self.f.ft, sub, less)? {
                        Ok(r) => !r,
                        Err((tailnull, currnull)) => if self.f.in_nulls_first { !tailnull } else { !currnull },
                    };
                    if stop {
                        break;
                    }
                    self.f.ft += 1;
                }
            } else {
                let off = self.f.offset(1)?.as_i64();
                let max = if fo & FRAMEOPTION_END_OFFSET_PRECEDING != 0 { self.f.curgroup - off } else { self.f.curgroup.checked_add(off).unwrap_or(i64::MAX) };
                while self.f.ftgroup <= max && self.has(self.f.ft)? {
                    self.f.ft += 1;
                    if self.has(self.f.ft)? && !self.are_peers(self.f.ft - 1, self.f.ft)? {
                        self.f.ftgroup += 1;
                    }
                }
            }
        } else {
            return Err(bad("window.agg: a frame end of no known form"));
        }
        self.f.ft_valid = true;
        Ok(())
    }

    /// PG `update_grouptailpos`: the end (exclusive) of the current row's peer group.
    pub(crate) fn update_grouptailpos(&mut self) -> PgResult<()> {
        if self.f.gt_valid {
            return Ok(());
        }
        let cur = self.f.cur;
        if self.f.ord.is_empty() {
            self.f.grouptail = self.part_rows()?;
        } else {
            loop {
                self.f.grouptail += 1;
                let g = self.f.grouptail;
                if !self.has(g)? || g > cur && !self.are_peers(g, cur)? {
                    break;
                }
            }
        }
        self.f.gt_valid = true;
        Ok(())
    }

    /// PG `row_is_in_frame` for partition position `pos`: -1 out of frame and no later row is in it, 0 out of frame,
    /// 1 in frame.
    pub(crate) fn row_is_in_frame(&mut self, pos: i64) -> PgResult<i32> {
        let (fo, cur) = (self.f.fo, self.f.cur);
        self.update_frameheadpos()?;
        if pos < self.f.fh {
            return Ok(0);
        }
        if fo & FRAMEOPTION_END_CURRENT_ROW != 0 {
            if pos > cur && (fo & FRAMEOPTION_ROWS != 0 || !self.are_peers(pos, cur)?) {
                return Ok(-1);
            }
        } else if fo & FRAMEOPTION_END_OFFSET != 0 {
            if fo & FRAMEOPTION_ROWS != 0 {
                let mut off = self.f.offset(1)?.as_i64();
                if fo & FRAMEOPTION_END_OFFSET_PRECEDING != 0 {
                    off = -off;
                }
                if cur.checked_add(off).is_some_and(|end| pos > end) {
                    return Ok(-1);
                }
            } else {
                self.update_frametailpos()?;
                if pos >= self.f.ft {
                    return Ok(-1);
                }
            }
        }
        if fo & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
            if pos == cur {
                return Ok(0);
            }
        } else if fo & FRAMEOPTION_EXCLUDE_GROUP != 0 || fo & FRAMEOPTION_EXCLUDE_TIES != 0 && pos != cur {
            if self.f.ord.is_empty() {
                return Ok(0);
            }
            if pos >= self.f.grouphead {
                self.update_grouptailpos()?;
                if pos < self.f.grouptail {
                    return Ok(0);
                }
            }
        }
        Ok(1)
    }
}
