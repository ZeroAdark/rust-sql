//! The window object's functions and its row driver (a `#[path]` child of hosts v4/m10.rs): nodeWindowAgg.c's
//! `ExecWindowAgg` per stored row (the partition's start, the peer group, the functions, the aggregates, the frame
//! pointers' updates, the run condition's pass-through modes) and windowfuncs.c's built-in window functions over
//! `WinGetFuncArgCurrent` / `InPartition` / `InFrame`. A window function PG runs through fmgr with a WindowObject of
//! its own (a C or PL window function outside windowfuncs.c) is refused before it reaches here. Created by M10-WINDOW.
use datum::{Datum, NullableDatum};
use gravity_kernels::ops::window::entries as abi;
use types_error::{PgError, PgResult};
use types_nodes::rawnodes::*;

use crate::ops::window::object::{bad, Rt, Status, WinSpec};

/// The built-in window functions (windowfuncs.c, pg_proc 3100 - 3114) and the plain aggregates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    LeadLag { forward: bool, offset: bool, default: bool },
    FirstValue,
    LastValue,
    NthValue,
    Agg,
}

/// A function's per-partition memory (`WinGetPartitionLocalMemory`: rank_context, ntile_context), zeroed per partition.
#[derive(Clone, Copy, Default)]
struct Local {
    rank: i64,
    ntile: i32,
    rows_per_bucket: i64,
    boundary: i64,
    remainder: i64,
}

pub(crate) struct Funcs {
    kinds: Vec<Kind>, // alloc: Exec
    local: Vec<Local>, // alloc: Exec
}

/// A window function's kind by its pg_proc OID (or an aggregate's), `None` for one outside windowfuncs.c (refused at
/// lowering).
fn kind_of(oid: u32, winagg: bool) -> Option<Kind> {
    let ll = |forward, offset, default| Some(Kind::LeadLag { forward, offset, default });
    match oid {
        _ if winagg => Some(Kind::Agg),
        3100 => Some(Kind::RowNumber),
        3101 => Some(Kind::Rank),
        3102 => Some(Kind::DenseRank),
        3103 => Some(Kind::PercentRank),
        3104 => Some(Kind::CumeDist),
        3105 => Some(Kind::Ntile),
        3106 => ll(false, false, false),
        3107 => ll(false, true, false),
        3108 => ll(false, true, true),
        3109 => ll(true, false, false),
        3110 => ll(true, true, false),
        3111 => ll(true, true, true),
        3112 => Some(Kind::FirstValue),
        3113 => Some(Kind::LastValue),
        3114 => Some(Kind::NthValue),
        _ => None,
    }
}

impl Funcs {
    pub(crate) fn new(s: &WinSpec) -> PgResult<Funcs> {
        let kinds = s.funcs.iter().zip(s.builtins.iter()).map(|(w, &b)| kind_of(b, w.winagg).ok_or_else(|| bad(&format!("window.agg: window function {} is not built in", w.winfnoid)))) // alloc: Cold
            .collect::<PgResult<Vec<_>>>()?; // alloc: Exec
        Ok(Funcs { local: vec![Local::default(); kinds.len()], kinds }) // alloc: Exec
    }

    pub(crate) fn is_agg(&self, i: usize) -> bool {
        self.kinds[i] == Kind::Agg
    }
}

impl Rt {
    /// PG `ExecReScanWindowAgg` at a pipeline's open (`m10_win_open`, D2346): the walk starts again at stored row 0 in run
    /// mode, the frame offsets taken anew by the next `m10_win_offsets`; `fill` (the fill's open: its input runs again)
    /// drops the stored rows too. A rerun set's re-open opens both, so no run reads the last run's rows or positions.
    pub(crate) fn open(&mut self, fill: bool) {
        (self.status, self.last) = (Status::Run, None);
        self.f.rewind();
        self.row_cx.reset();
        if fill {
            self.store.borrow_mut().clear();
            self.held = None;
        }
    }

    /// `m10_win_reads(h, j, f)` (D2246): what PG's ExecWindowAgg reads at stored row `j`, the next row: `READS_RUN` when
    /// its functions are evaluated (run mode, or the first row of a partition, which leaves a pass-through mode), with
    /// `READS_FIRST` when function `f - 1` (`f` 0: none) reads its first-call argument there: the row begins a partition,
    /// or the function is an ntile whose every earlier call in the partition found its buckets NULL (window_ntile returns
    /// NULL without setting its context, so the next row reads them again); 0 for the row just answered and once done.
    /// The step evaluates a `step_arg` cell only where its function reads it, as PG evaluates the argument at
    /// WinGetFuncArgCurrent.
    pub(crate) fn reads(&mut self, j: u64, f: u64) -> PgResult<i64> {
        match self.last {
            Some((l, ..)) if l == j => return Ok(0),
            Some((l, ..)) if j != l + 1 => return Err(bad("m10_win_reads: rows out of order")),
            None if j != 0 => return Err(bad("m10_win_reads: rows out of order")),
            _ => {}
        }
        let begins = self.status != Status::Done && self.begins(j)?;
        let unset = f.checked_sub(1).map(|i| i as usize).is_some_and(|i| self.fns.kinds.get(i) == Some(&Kind::Ntile) && self.fns.local[i].ntile == 0);
        Ok(match (self.status, begins) {
            (Status::Done, _) => 0,
            (_, true) => abi::READS_RUN | abi::READS_FIRST,
            (Status::Run, false) if unset => abi::READS_RUN | abi::READS_FIRST,
            (Status::Run, false) => abi::READS_RUN,
            _ => 0,
        })
    }

    /// PG `ExecWindowAgg` for stored row `j` (the rows come in order; the row just answered is answered again).
    pub(crate) fn row(&mut self, j: u64) -> PgResult<i64> {
        match self.last {
            Some((l, st, _)) if l == j => return Ok(st),
            Some((l, ..)) if j != l + 1 => return Err(bad("m10_win_row: rows out of order")),
            None if j != 0 => return Err(bad("m10_win_row: rows out of order")),
            _ => {}
        }
        if j >= self.store.borrow().nrows {
            return Err(bad("m10_win_row: a row past the stored rows"));
        }
        self.store.borrow().held(j)?;
        let st = self.row_at(j)?;
        self.last = Some((j, st, None));
        Ok(st)
    }

    fn row_at(&mut self, j: u64) -> PgResult<i64> {
        if self.status == Status::Done {
            return Ok(abi::ROW_DONE);
        }
        self.row_cx.reset();
        if self.begins(j)? {
            // D3345: PG's release_partition: no position of the partition starting here reads a row before it; D3951: unless
            // a later open walks these rows again (the fill kept across the reader's re-opens)
            if !self.rewalks {
                self.store.borrow_mut().release_before(j);
            }
            self.begin_partition(j);
            self.status = Status::Run;
            self.begin_funcs();
        } else {
            self.f.cur += 1;
            (self.f.fh_valid, self.f.ft_valid) = (false, false);
        }
        if self.f.cur != (j - self.f.pstart) as i64 {
            return Err(bad("window.agg: the current row is not the stored row read"));
        }
        let fo = self.f.fo;
        if fo & (FRAMEOPTION_GROUPS | FRAMEOPTION_EXCLUDE_GROUP | FRAMEOPTION_EXCLUDE_TIES) != 0 && self.f.cur > 0 && !self.are_peers(self.f.cur - 1, self.f.cur)? {
            self.f.curgroup += 1;
            self.f.grouphead = self.f.cur;
            self.f.gt_valid = false;
        }
        if self.status == Status::Run {
            // bounded("other: the WindowAgg node's window functions, one evaluation each: the query text bounds them, as PG's ExecWindowAgg walks the same functions per row with no poll")
            for i in 0..self.spec.funcs.len() {
                if !self.fns.is_agg(i) {
                    let v = self.eval_windowfunction(i)?;
                    self.set_res(i, v);
                }
            }
            if self.aggs.any() {
                self.eval_windowaggregates()?;
            }
        } else {
            (0..self.spec.funcs.len()).for_each(|i| self.set_res(i, NullableDatum::null()));
        }
        if self.f.head_ptr {
            self.update_frameheadpos()?;
        }
        if self.f.tail_ptr {
            self.update_frametailpos()?;
        }
        if self.f.gt_ptr {
            self.update_grouptailpos()?;
        }
        Ok(match self.status {
            Status::Run => abi::ROW_EMIT,
            Status::PassThrough => abi::ROW_PASS,
            Status::PassThroughStrict => abi::ROW_SKIP,
            Status::Done => abi::ROW_DONE,
        })
    }

    /// The run condition failed at the row last answered (ExecWindowAgg's `!ExecQual(runcondition)` arm): a window
    /// with pass-through goes on with NULL values (a top window drops the rest of the partition, this row too), one
    /// without is done. Asked again for the same row, the same answer.
    pub(crate) fn runfail(&mut self) -> PgResult<i64> {
        let Some((j, st, done)) = self.last else { return Err(bad("m10_win_runfail: no row answered")) };
        if let Some(a) = done {
            return Ok(a);
        }
        if st != abi::ROW_EMIT {
            return Err(bad("m10_win_runfail: a row not evaluated in run mode"));
        }
        let w = self.spec.plan;
        let pass = !w.topWindow || w.partNumCols > 0;
        let a = match (pass, w.topWindow) {
            (true, true) => {
                self.status = Status::PassThroughStrict;
                abi::ROW_SKIP
            }
            (true, false) => {
                self.status = Status::PassThrough;
                abi::ROW_EMIT
            }
            (false, _) => {
                self.status = Status::Done;
                abi::ROW_DONE
            }
        };
        self.last = Some((j, st, Some(a)));
        Ok(a)
    }

    /// begin_partition's per-function part: the partition-local memory and the aggregates' positions.
    fn begin_funcs(&mut self) {
        self.fns.local.iter_mut().for_each(|l| *l = Local::default());
        self.aggs.begin_partition();
    }

    /// Argument `a` of function `i` on partition position `pos`.
    fn arg_at(&mut self, i: usize, a: usize, pos: i64) -> PgResult<NullableDatum> {
        let (base, n, _) = self.spec.args[i];
        if a >= n {
            return Err(bad("window.agg: a window function argument past its arguments"));
        }
        self.cell(self.f.pstart + pos as u64, base + a)
    }

    /// PG `WinGetFuncArgCurrent`.
    fn arg_current(&mut self, i: usize, a: usize) -> PgResult<NullableDatum> {
        self.arg_at(i, a, self.f.cur)
    }

    /// PG `WinGetFuncArgInPartition` relative to the current row: `None` when out of the partition.
    fn arg_in_partition(&mut self, i: usize, a: usize, relpos: i64) -> PgResult<Option<NullableDatum>> {
        let pos = self.f.cur + relpos;
        if !self.has(pos)? {
            return Ok(None);
        }
        self.arg_at(i, a, pos).map(Some)
    }

    /// PG `WinGetFuncArgInFrame` (seek from the frame's head, `head`, or its tail): `None` when out of the frame.
    fn arg_in_frame(&mut self, i: usize, a: usize, relpos: i64, head: bool) -> PgResult<Option<NullableDatum>> {
        let fo = self.f.fo;
        let excl = fo & FRAMEOPTION_EXCLUSION;
        let cur = self.f.cur;
        let pos = if head {
            if relpos < 0 {
                return Ok(None);
            }
            self.update_frameheadpos()?;
            let fh = self.f.fh;
            let mut pos = fh + relpos;
            match excl {
                0 => {}
                FRAMEOPTION_EXCLUDE_CURRENT_ROW => {
                    if pos >= cur && cur >= fh {
                        pos += 1;
                    }
                }
                FRAMEOPTION_EXCLUDE_GROUP | FRAMEOPTION_EXCLUDE_TIES => {
                    self.update_grouptailpos()?;
                    let (gh, gt) = (self.f.grouphead, self.f.grouptail);
                    if pos >= gh && gt > fh {
                        let start = gh.max(fh);
                        // EXCLUDE TIES retains the current row only when it is
                        // inside the frame. Otherwise skip the overlapping peers
                        // as EXCLUDE GROUP does (PostgreSQL bug #19732).
                        pos = if excl == FRAMEOPTION_EXCLUDE_GROUP || cur < fh { pos + gt - start } else if pos == start { cur } else { pos + gt - start - 1 };
                    }
                }
                _ => return Err(bad("window.agg: an unrecognized frame exclusion")),
            }
            pos
        } else {
            if relpos > 0 {
                return Ok(None);
            }
            self.update_frametailpos()?;
            let ft = self.f.ft;
            let mut pos = ft - 1 + relpos;
            if excl != 0 {
                match excl {
                    FRAMEOPTION_EXCLUDE_CURRENT_ROW => {
                        if pos <= cur && cur < ft {
                            pos -= 1;
                        }
                    }
                    FRAMEOPTION_EXCLUDE_GROUP | FRAMEOPTION_EXCLUDE_TIES => {
                        self.update_grouptailpos()?;
                        let (gh, gt) = (self.f.grouphead, self.f.grouptail);
                        if pos < gt && gh < ft {
                            let end = gt.min(ft);
                            pos = if excl == FRAMEOPTION_EXCLUDE_GROUP || cur >= ft { pos - (end - gh) } else if pos == end - 1 { cur } else { pos - (end - 1 - gh) };
                        }
                    }
                    _ => return Err(bad("window.agg: an unrecognized frame exclusion")),
                }
                self.update_frameheadpos()?;
                if pos < self.f.fh {
                    return Ok(None);
                }
            }
            pos
        };
        if !self.has(pos)? || self.row_is_in_frame(pos)? <= 0 {
            return Ok(None);
        }
        self.arg_at(i, a, pos).map(Some)
    }

    /// windowfuncs.c's `rank_up`: whether the current row starts a new peer group.
    fn rank_up(&mut self, i: usize) -> PgResult<bool> {
        let cur = self.f.cur;
        if self.fns.local[i].rank == 0 {
            self.fns.local[i].rank = 1;
            return Ok(false);
        }
        Ok(!self.are_peers(cur - 1, cur)?)
    }

    /// PG `eval_windowfunction` over windowfuncs.c's built-in functions.
    fn eval_windowfunction(&mut self, i: usize) -> PgResult<NullableDatum> {
        let cur = self.f.cur;
        let val = |d: Datum| Ok(NullableDatum { value: d, isnull: false });
        match self.fns.kinds[i] {
            Kind::RowNumber => val(Datum::from_i64(cur + 1)),
            Kind::Rank => {
                if self.rank_up(i)? {
                    self.fns.local[i].rank = cur + 1;
                }
                val(Datum::from_i64(self.fns.local[i].rank))
            }
            Kind::DenseRank => {
                if self.rank_up(i)? {
                    self.fns.local[i].rank += 1;
                }
                val(Datum::from_i64(self.fns.local[i].rank))
            }
            Kind::PercentRank => {
                let total = self.part_rows()?;
                if self.rank_up(i)? {
                    self.fns.local[i].rank = cur + 1;
                }
                if total <= 1 {
                    return val(Datum::from_f64(0.0));
                }
                val(Datum::from_f64((self.fns.local[i].rank - 1) as f64 / (total - 1) as f64))
            }
            Kind::CumeDist => {
                let total = self.part_rows()?;
                let up = self.rank_up(i)?;
                if up || self.fns.local[i].rank == 1 {
                    self.fns.local[i].rank = cur + 1;
                    let mut row = cur + 1;
                    while row < total {
                        cancelpoll::interrupts::check()?;
                        if !self.are_peers(row - 1, row)? {
                            break;
                        }
                        self.fns.local[i].rank += 1;
                        row += 1;
                    }
                }
                val(Datum::from_f64(self.fns.local[i].rank as f64 / total as f64))
            }
            Kind::Ntile => self.ntile(i),
            Kind::LeadLag { forward, offset, default } => {
                let off = if offset {
                    let o = self.arg_current(i, 1)?;
                    if o.isnull {
                        return Ok(NullableDatum::null());
                    }
                    o.value.as_i32() as i64
                } else {
                    1
                };
                match self.arg_in_partition(i, 0, if forward { off } else { -off })? {
                    Some(v) => Ok(v),
                    None if default => self.arg_current(i, 2),
                    None => Ok(NullableDatum::null()),
                }
            }
            Kind::FirstValue => Ok(self.arg_in_frame(i, 0, 0, true)?.unwrap_or(NullableDatum::null())),
            Kind::LastValue => Ok(self.arg_in_frame(i, 0, 0, false)?.unwrap_or(NullableDatum::null())),
            Kind::NthValue => {
                let nth = self.arg_current(i, 1)?;
                if nth.isnull {
                    return Ok(NullableDatum::null());
                }
                let nth = nth.value.as_i32();
                if nth <= 0 {
                    return Err(Box::new(PgError::error("argument of nth_value must be greater than zero".to_string()).with_sqlstate(types_error::ERRCODE_INVALID_ARGUMENT_FOR_NTH_VALUE))); // alloc: Cold
                }
                Ok(self.arg_in_frame(i, 0, nth as i64 - 1, true)?.unwrap_or(NullableDatum::null()))
            }
            Kind::Agg => Err(bad("window.agg: an aggregate evaluated as a window function")),
        }
    }

    /// windowfuncs.c's `window_ntile`.
    fn ntile(&mut self, i: usize) -> PgResult<NullableDatum> {
        if self.fns.local[i].ntile == 0 {
            let total = self.part_rows()?;
            let nb = self.arg_current(i, 0)?;
            if nb.isnull {
                return Ok(NullableDatum::null());
            }
            let nb = nb.value.as_i32();
            if nb <= 0 {
                return Err(Box::new(PgError::error("argument of ntile must be greater than zero".to_string()).with_sqlstate(types_error::ERRCODE_INVALID_ARGUMENT_FOR_NTILE))); // alloc: Cold
            }
            let l = &mut self.fns.local[i];
            (l.ntile, l.rows_per_bucket, l.boundary) = (1, 0, total / nb as i64);
            if l.boundary <= 0 {
                l.boundary = 1;
            } else {
                l.remainder = total % nb as i64;
                if l.remainder != 0 {
                    l.boundary += 1;
                }
            }
        }
        let l = &mut self.fns.local[i];
        l.rows_per_bucket += 1;
        if l.boundary < l.rows_per_bucket {
            if l.remainder != 0 && l.ntile as i64 == l.remainder {
                l.remainder = 0;
                l.boundary -= 1;
            }
            l.ntile += 1;
            l.rows_per_bucket = 1;
        }
        Ok(NullableDatum { value: Datum::from_i32(l.ntile), isnull: false })
    }
}
