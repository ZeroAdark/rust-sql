//! D3105 (b): merge.join's inner past its grant. Its Sort grows the run only within the owner's limit and puts it past
//! that (runs/slot_run.rs, `m11_run_full`); at the merge's first window every run is merged into one (PG tuplesort's final
//! merge onto one tape for a sort read with random access, `merge_to_one`), which the merge reads through a window laid
//! over the consumed region (the run's region, empty once put): rerun/spool.rs's cursor over the one run (forward fills,
//! the mark kept, a restore before the window reopens the run and reads forward), its whole rows indexed so the merge's
//! fetch of row `j` stays an index: one load while `j` is in the window (the IR's fast path), `m11_window` else. Each row
//! in the window has its by-reference cells pointed at the images inside it (`point_images`), so a row's cells hold until
//! the next `m11_window`: a row's life, as runs.sort's emit's cells (`Until::Row`).
use gravity_abi::breaker::SortRun;
use gravity_abi::program::{SPILL_MERGE_BIG, SPILL_RUN_BASE, SPILL_RUN_CLOSE, SPILL_RUN_FILL, SPILL_RUN_OPEN};
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SCRATCH};
use gravity_abi::Ctx;

use crate::host_abi::{cmp_rows, merge_to_one, point_images, tbl_shape, var_shape, RowCmp, CMP_ERR};
use crate::table::fold_entries::bare;
use crate::util::spill;

/// The window's kind byte (a SortRun's `kind`, byte 15): neither a sort run's (`TABLE_SORT`) nor a merge's.
pub const MJ_WINDOW_KIND: u8 = 0x4d;
const MAGIC: u64 = 0x4d4a_5749_4e44_3331;
/// One `SPILL_RUN_FILL` carries its byte count in 16 bits.
const FILL_MAX: usize = 0xfff8;
/// The window's big buffer's key among the host's big buffers (`SPILL_MERGE_BIG`): beside a merge's (keyed the region `own`)
/// and D1330's big run's (`own + 1`).
fn big_key(own: u64) -> u64 {
    own + 2
}

/// The window at the consumed region's base. The IR reads `len` (the run's rows, where a SortRun keeps its `len`), the
/// kind byte, `j0` / `cnt` / `idx` (row `j0 + d` at `idx[d]` for `d < cnt`) and writes `mark_j`; the rest is the host's.
#[repr(C)]
pub struct Window {
    zero: u32,
    pub len: u32,
    _r: u32,
    _pad: [u8; 3],
    pub kind: u8,
    pub j0: u64,
    pub cnt: u64,
    pub idx: u64,
    /// The merge's mark (`SlotRun::mark`), -1 before its first.
    pub mark_j: i64,
    magic: u64,
    rows: u64,
    /// `buf[..blen]` holds the run's bytes from offset `boff`: whole rows `j0 ..` up to `parsed`, then part of the next.
    buf: u64,
    bcap: u64,
    blen: u64,
    parsed: u64,
    boff: u64,
    idx_cap: u64,
    /// A row whose run offset is known (`mark_off`): the mark, noted while its row was in the window; -1 none.
    mark_at: i64,
    mark_off: u64,
    /// The window is one record past the buffer, in the big buffer (`big`), at run offset `big_off`.
    big: u64,
    big_cap: u64,
    big_off: u64,
    big_on: u32,
    token: u32,
    stream: u64,
    /// The open reader (its next byte at `boff + blen`), -1 none; `eof` once it ended (the driver dropped it).
    handle: i32,
    eof: u32,
    rec: u32,
    /// Variable records: the record word's offset (`VarShape::rec_off`); `u32::MAX` for fixed ones of `rec` bytes.
    rec_off: u32,
    point: u32,
    _p: u32,
    own: u64,
}

pub const W_LEN: i32 = core::mem::offset_of!(Window, len) as i32;
pub const W_KIND: i32 = core::mem::offset_of!(Window, kind) as i32;
pub const W_J0: i32 = core::mem::offset_of!(Window, j0) as i32;
pub const W_CNT: i32 = core::mem::offset_of!(Window, cnt) as i32;
pub const W_IDX: i32 = core::mem::offset_of!(Window, idx) as i32;
pub const W_MARK: i32 = core::mem::offset_of!(Window, mark_j) as i32;
const _: () = assert!(W_LEN == 4 && W_KIND == 15, "a window's len and kind sit where a SortRun keeps them");

/// Whether consumed region `own` holds a window laid by `lay` (a later window of the same fill).
pub(crate) unsafe fn live(own: *const u8) -> bool {
    let w = &*(own as *const Window);
    w.kind == MJ_WINDOW_KIND && w.magic == MAGIC
}

/// The window laid over `own` (the inner's region, its run put empty): the stream's `rows` rows merged into one run over
/// the region (`full` the run's table: its record form and order), then the window over the region with no row in it
/// (the first fetch fills it). `point`: the rows' images pointed (the merge reads a by-reference cell).
pub(crate) unsafe fn lay(ctx: *mut Ctx, own: *mut u8, full: *const u32, stream: u64, rows: u64, point: bool) -> *const u8 {
    let r = &*(own as *const SortRun);
    if r.len != 0 {
        return bare("Bug: a merge join's inner run both put and held") as *const u8;
    }
    let Ok(len) = u32::try_from(rows) else { return unsupported(UNSUPPORTED_SCRATCH) as *const u8 };
    let (rec, nkey, _, spec) = tbl_shape(full);
    let usable = SortRun::region_bytes(own, rec as u32);
    let var = var_shape(full);
    if spill(ctx, SPILL_RUN_BASE, own as i64) < 0 {
        return ERROR as *const u8;
    }
    let merged = match var {
        Some(v) => {
            let rc = RowCmp::of(full);
            CMP_ERR.with(|c| c.set(false));
            let m = merge_to_one(ctx, own, usable, rec, stream, Some(v.rec_off as u16), &|a, b| rc.cmp(a, b));
            if CMP_ERR.with(|c| c.get()) {
                spill(ctx, SPILL_RUN_BASE, 0);
                return crate::host_abi::cmp_failed("a sort merge compare failed") as *const u8;
            }
            m
        }
        None => {
            merge_to_one(ctx, own, usable, rec, stream, None, &|a, b| cmp_rows(nkey / 2, spec, a, b))
        }
    };
    spill(ctx, SPILL_RUN_BASE, 0);
    let token = match merged {
        Err(e) => return e as *const u8,
        Ok(None) => return bare("Bug: a merge join's inner rows with no run") as *const u8,
        Ok(Some(t)) => t,
    };
    let hdr = core::mem::size_of::<Window>();
    let n = usable.saturating_sub(hdr) / (rec + 8);
    if n < 2 {
        return unsupported(UNSUPPORTED_SCRATCH) as *const u8;
    }
    let buf = own.add(hdr + 8 * n);
    let bcap = (usable - hdr - 8 * n) & !7;
    let rec_off = var.map_or(u32::MAX, |v| v.rec_off as u32);
    *(own as *mut Window) = Window {
        zero: 0, len, _r: 0, _pad: [0; 3], kind: MJ_WINDOW_KIND, j0: 0, cnt: 0, idx: own.add(hdr) as u64, mark_j: -1, magic: MAGIC, rows,
        buf: buf as u64, bcap: bcap as u64, blen: 0, parsed: 0, boff: 0, idx_cap: n as u64, mark_at: -1, mark_off: 0, big: 0, big_cap: 0,
        big_off: 0, big_on: 0, token, stream, handle: -1, eof: 0, rec: rec as u32, rec_off, point: point as u32, _p: 0, own: own as u64,
    };
    own
}

/// Row `j`'s address in window `w`, the window moved to it: forward, the bytes from the mark (or the next row) kept and the
/// buffer filled behind them; back (a restore past the window, a rescan), the run reopened at the mark (or its start)
/// and read forward. A status below zero on an error.
pub(crate) unsafe fn row(ctx: *mut Ctx, w: *mut u8, j: u64, full: *const u32) -> i64 {
    let w = &mut *(w as *mut Window);
    if w.kind != MJ_WINDOW_KIND || w.magic != MAGIC {
        return bare("Bug: m11_window over a region that holds no window");
    }
    if j >= w.rows {
        return bare("Bug: a merge join's fetch past its inner run");
    }
    loop {
        if j >= w.j0 && j < w.j0 + w.cnt {
            return *(w.idx as *const u64).add((j - w.j0) as usize) as i64;
        }
        note_mark(w);
        let r = if j < w.j0 { rewind(ctx, w, j) } else { advance(ctx, w, full) };
        if let Err(e) = r {
            return e;
        }
    }
}

/// Row `j`'s run offset (`j` in the window).
unsafe fn offset_of_row(w: &Window, j: u64) -> u64 {
    if w.big_on != 0 {
        return w.big_off;
    }
    w.boff + (*(w.idx as *const u64).add((j - w.j0) as usize) - w.buf)
}

/// The mark's run offset kept while its row is in the window (a restore past the window reopens the run there).
unsafe fn note_mark(w: &mut Window) {
    if let Ok(m) = u64::try_from(w.mark_j) {
        if m >= w.j0 && m < w.j0 + w.cnt {
            (w.mark_at, w.mark_off) = (w.mark_j, offset_of_row(w, m));
        }
    }
}

/// Back before the window: the reader closed and the run reopened (when filled) at the noted mark when it is at or before
/// `j`, else at the run's start; the window empty at that row.
unsafe fn rewind(ctx: *mut Ctx, w: &mut Window, j: u64) -> Result<(), i64> {
    let (sj, so) = match u64::try_from(w.mark_at) {
        Ok(m) if m <= j => (m, w.mark_off),
        _ => (0, 0),
    };
    close(ctx, w)?;
    (w.j0, w.cnt, w.blen, w.parsed, w.boff, w.big_on, w.eof) = (sj, 0, 0, 0, so, 0, 0);
    Ok(())
}

unsafe fn close(ctx: *mut Ctx, w: &mut Window) -> Result<(), i64> {
    if w.handle >= 0 && spill(ctx, SPILL_RUN_CLOSE, w.handle as i64) < 0 {
        return Err(ERROR);
    }
    w.handle = -1;
    Ok(())
}

/// Forward past the window: the rows before the kept one dropped (the mark's while its bytes leave a quarter of the
/// buffer, else every whole row), the buffer filled from the run and its whole rows indexed and pointed. A record longer
/// than the buffer is read whole into the big buffer as a window of its own.
unsafe fn advance(ctx: *mut Ctx, w: &mut Window, full: *const u32) -> Result<(), i64> {
    let buf = w.buf as *mut u8;
    let tail = w.j0 + w.cnt;
    if w.big_on != 0 {
        (w.big_on, w.j0, w.cnt) = (0, tail, 0);
    } else {
        let mark = u64::try_from(w.mark_j).ok().filter(|&m| m >= w.j0 && m < tail);
        let (keep, at) = match mark.map(|m| (m, *(w.idx as *const u64).add((m - w.j0) as usize) - w.buf)) {
            Some((m, a)) if 4 * (w.blen - a) <= 3 * w.bcap => (m, a),
            _ => (tail, w.parsed),
        };
        core::ptr::copy(buf.add(at as usize), buf, (w.blen - at) as usize);
        (w.blen, w.boff, w.j0, w.cnt, w.parsed) = (w.blen - at, w.boff + at, keep, 0, 0);
    }
    let cap = w.bcap;
    fill(ctx, w, buf, cap)?;
    parse(w, full);
    if w.j0 + w.cnt > tail {
        return Ok(());
    }
    if w.cnt > 0 && w.eof == 0 {
        return Ok(()); // the mark's rows refilled, the next record not whole yet: the next call keeps no mark
    }
    if w.cnt > 0 {
        return Err(bare("Bug: a merge join's inner run ended before its rows"));
    }
    match (w.blen, w.eof != 0) {
        (0, true) => Err(bare("Bug: a merge join's inner run ended before its rows")),
        (n, _) if n < w.bcap => Err(unsupported(UNSUPPORTED_SCRATCH)), // a truncated record at the run's end
        _ => big(ctx, w, full),
    }
}

/// The whole rows in `buf[parsed..blen]` indexed (and pointed), `parsed` past them.
unsafe fn parse(w: &mut Window, full: *const u32) {
    let buf = w.buf as *mut u8;
    let (mut at, rec, blen) = (w.parsed as usize, w.rec as usize, w.blen as usize);
    while (w.cnt as usize) < w.idx_cap as usize && at < blen {
        let n = match record_len(w, buf.add(at), blen - at) {
            Some(n) if at + n <= blen => n,
            _ => break,
        };
        let row = buf.add(at);
        if w.point != 0 {
            point_images(row, full);
        }
        *(w.idx as *mut u64).add(w.cnt as usize) = row as u64;
        (w.cnt, at) = (w.cnt + 1, at + n.max(rec));
    }
    w.parsed = at as u64;
}

/// The record at `p` (`have` bytes of it in hand): its length, `None` while its head is not in hand. A variable record
/// shorter than its fixed head or not 8-aligned reads as unknown (a corrupt run stops there: the caller raises).
unsafe fn record_len(w: &Window, p: *const u8, have: usize) -> Option<usize> {
    if w.rec_off == u32::MAX {
        return Some(w.rec as usize);
    }
    let o = w.rec_off as usize;
    if have < o + 4 {
        return None;
    }
    let n = (p.add(o) as *const u32).read_unaligned() as usize;
    (n >= w.rec as usize && n % 8 == 0).then_some(n)
}

/// Bytes from the run appended to `dst[blen..cap]` until it is full or the run ends (the reader opened at `boff` first
/// when none is open: the window's first fill, a rewind's).
unsafe fn fill(ctx: *mut Ctx, w: &mut Window, dst: *mut u8, cap: u64) -> Result<(), i64> {
    if w.handle < 0 && w.eof == 0 {
        if w.blen != 0 {
            return Err(bare("Bug: a merge join's inner window reopened over bytes it holds"));
        }
        let off = w.boff;
        open_at(ctx, w, dst, off)?;
    }
    while w.blen < cap && w.eof == 0 {
        let (at, want) = (w.blen as usize, (cap - w.blen) as usize);
        let got = read(ctx, w, dst, at, want)?;
        w.blen += got as u64;
    }
    Ok(())
}

/// The run reopened and read forward to run offset `off` (`dst` its scratch).
unsafe fn open_at(ctx: *mut Ctx, w: &mut Window, dst: *mut u8, off: u64) -> Result<(), i64> {
    let h = spill(ctx, SPILL_RUN_OPEN, (w.token as u64 | w.stream << 32) as i64);
    if h < 0 {
        return Err(ERROR);
    }
    w.handle = h as i32;
    let (mut left, room) = (off as usize, w.bcap as usize);
    while left > 0 {
        let got = read(ctx, w, dst, 0, left.min(room))?;
        if got == 0 {
            return Err(bare("Bug: a merge join's inner run shorter than its mark"));
        }
        left -= got;
    }
    Ok(())
}

/// One fill of at most `want` bytes into `dst[at..]` (several `SPILL_RUN_FILL`s of 16-bit counts): the bytes read, 0 at the
/// run's end (`eof`: the driver dropped the reader).
unsafe fn read(ctx: *mut Ctx, w: &mut Window, dst: *mut u8, at: usize, want: usize) -> Result<usize, i64> {
    if spill(ctx, SPILL_RUN_BASE, dst as i64) < 0 {
        return Err(ERROR);
    }
    let mut got = 0;
    while got < want {
        let ask = (want - got).min(FILL_MAX) as u64;
        let n = spill(ctx, SPILL_RUN_FILL, (w.handle as u64 & 0xffff | ((at + got) as u64) << 16 | ask << 48) as i64);
        if n < 0 {
            spill(ctx, SPILL_RUN_BASE, 0);
            return Err(ERROR);
        }
        if n == 0 {
            (w.eof, w.handle) = (1, -1);
            break;
        }
        got += n as usize;
    }
    spill(ctx, SPILL_RUN_BASE, 0);
    Ok(got)
}

/// A record longer than the buffer (PG lets one tuple pass work_mem): read whole into the big buffer (`SPILL_MERGE_BIG`,
/// the window's own key: one held at a time, the last freed for reuse first), the window that one row.
#[cold]
unsafe fn big(ctx: *mut Ctx, w: &mut Window, full: *const u32) -> Result<(), i64> {
    let buf = w.buf as *mut u8;
    let Some(n) = record_len(w, buf, w.blen as usize) else { return Err(unsupported(UNSUPPORTED_SCRATCH)) };
    if n as u64 > w.big_cap {
        let key = big_key(w.own);
        let free = [key, 0u64];
        if spill(ctx, SPILL_MERGE_BIG, free.as_ptr() as i64) < 0 {
            return Err(ERROR);
        }
        let ask = [key, n as u64];
        let p = spill(ctx, SPILL_MERGE_BIG, ask.as_ptr() as i64);
        if p <= 0 {
            return Err(ERROR);
        }
        (w.big, w.big_cap) = (p as u64, n as u64);
    }
    let big = w.big as *mut u8;
    core::ptr::copy_nonoverlapping(buf, big, w.blen as usize);
    fill(ctx, w, big, n as u64)?;
    if w.blen < n as u64 {
        return Err(unsupported(UNSUPPORTED_SCRATCH)); // a truncated record at the run's end
    }
    if w.point != 0 {
        point_images(big, full);
    }
    *(w.idx as *mut u64) = big as u64;
    (w.big_off, w.boff, w.blen, w.parsed, w.cnt, w.big_on) = (w.boff, w.boff + n as u64, 0, 0, 1, 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_abi::{SORT_TBL_HDR, VAR_TBL};
    use gravity_abi::program::{MERGED_RUN, SPILL_MERGE_BEGIN, SPILL_MERGE_WRITE, SPILL_RUNS};
    use gravity_abi::ProgramBlock;

    /// The fake driver's runs: the put runs, each merge pass's runs, the open readers `(run bytes, read position)`, the
    /// fill base and the big buffers (kept alive). A process static under a lock (the TLS census), one test at a time.
    #[derive(Default)]
    struct Fake {
        puts: Vec<Vec<u8>>,
        passes: Vec<Vec<Vec<u8>>>,
        readers: Vec<Option<(Vec<u8>, usize)>>,
        base: usize,
        bigs: Vec<Vec<u64>>,
        opens: usize,
    }
    static FAKE: std::sync::Mutex<Option<Fake>> = std::sync::Mutex::new(None); // alloc: Cold
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_of(f: &Fake, token: u32) -> Vec<u8> {
        if token & MERGED_RUN != 0 {
            let (p, i) = (((token & !MERGED_RUN) >> 20) as usize, (token & 0xfffff) as usize);
            f.passes[p][i].clone() // alloc: Cold
        } else {
            f.puts[token as usize].clone() // alloc: Cold
        }
    }

    unsafe extern "C" fn fake_spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        if op == gravity_abi::program::SPILL_ARENA_CHUNK {
            return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
        }
        let mut g = FAKE.lock().expect("fake");
        let f = g.as_mut().expect("a fake driver");
        match op {
            SPILL_RUN_BASE => {
                f.base = arg as usize;
                0
            }
            SPILL_RUNS => f.puts.len() as i64,
            SPILL_MERGE_BEGIN => {
                let pass = ((arg >> 32) & 0x3fff_ffff) as usize;
                if arg & (1 << 62) != 0 {
                    if pass == 0 {
                        f.passes.clear();
                    }
                    assert_eq!(f.passes.len(), pass, "a pass opened out of order");
                    f.passes.push(Vec::new()); // alloc: Cold
                }
                f.passes[pass].push(Vec::new()); // alloc: Cold
                (MERGED_RUN | (pass as u32) << 20 | (f.passes[pass].len() - 1) as u32) as i64
            }
            SPILL_MERGE_WRITE => {
                let w = arg as *const u64;
                let (token, p, n) = (*w.add(1) as u32, *w.add(2) as *const u8, *w.add(3) as usize);
                let (pass, i) = (((token & !MERGED_RUN) >> 20) as usize, (token & 0xfffff) as usize);
                f.passes[pass][i].extend_from_slice(core::slice::from_raw_parts(p, n));
                0
            }
            SPILL_MERGE_BIG => {
                let w = arg as *const u64;
                let bytes = *w.add(1) as usize;
                if bytes == 0 {
                    return 0;
                }
                let mut b = vec![0u64; bytes.div_ceil(8)]; // alloc: Cold
                let p = b.as_mut_ptr() as i64;
                f.bigs.push(b); // alloc: Cold
                p
            }
            SPILL_RUN_OPEN => {
                let bytes = run_of(f, arg as u32);
                f.readers.push(Some((bytes, 0))); // alloc: Cold
                f.opens += 1;
                (f.readers.len() - 1) as i64
            }
            SPILL_RUN_FILL => {
                let (h, off, cap) = ((arg & 0xffff) as usize, ((arg >> 16) & 0xffff_ffff) as usize, (arg >> 48) as usize);
                assert_ne!(f.base, 0, "a fill with no base");
                let Some((bytes, pos)) = f.readers[h].as_mut() else { return 0 };
                let n = (bytes.len() - *pos).min(cap).min(5000); // short reads, as a file's
                core::ptr::copy_nonoverlapping(bytes.as_ptr().add(*pos), (f.base + off) as *mut u8, n);
                *pos += n;
                if n == 0 {
                    f.readers[h] = None;
                }
                n as i64
            }
            SPILL_RUN_CLOSE => {
                f.readers[arg as usize] = None;
                0
            }
            _ => panic!("fake spill op {op}"),
        }
    }

    /// A ctx over the fake driver, the region `own` armed as an empty run's share of `bytes`.
    struct Rig {
        _program: Box<ProgramBlock>,
        ctx: Vec<u64>,
        own: Vec<u64>,
    }

    impl Rig {
        fn new(bytes: usize, puts: Vec<Vec<u8>>) -> Rig {
            *FAKE.lock().expect("fake") = Some(Fake { puts, ..Fake::default() });
            let mut program = Box::new(ProgramBlock::NONE); // alloc: Cold
            program.spill = Some(fake_spill);
            let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(0) / 8]; // alloc: Cold
            unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
            let mut own = vec![0u64; bytes / 8]; // alloc: Cold
            unsafe { (own.as_mut_ptr() as *mut u32).add(2).write(bytes as u32) };
            Rig { _program: program, ctx, own }
        }
        fn c(&mut self) -> *mut Ctx {
            self.ctx.as_mut_ptr() as *mut Ctx
        }
        fn own(&mut self) -> *mut u8 {
            self.own.as_mut_ptr() as *mut u8
        }
        fn at(&mut self, j: u64, tbl: &[u32]) -> *const u8 {
            let (c, own) = (self.c(), self.own());
            let p = unsafe { row(c, own, j, tbl.as_ptr()) };
            assert!(p > 0, "row {j}: {p}");
            p as *const u8
        }
        fn mark(&mut self, j: u64) {
            unsafe { (*(self.own() as *mut Window)).mark_j = j as i64 };
        }
        fn opens(&self) -> usize {
            FAKE.lock().expect("fake").as_ref().expect("a fake").opens
        }
    }

    /// A fixed row (32 bytes): the key pair `[1, key]`, the datum word `id`, a NULL byte 0.
    fn fixed(key: u64, id: u64) -> [u64; 4] {
        [1, key, id, 0]
    }

    fn bytes(rows: &[[u64; 4]]) -> Vec<u8> {
        rows.iter().flat_map(|r| r.iter().flat_map(|w| w.to_le_bytes())).collect() // alloc: Cold
    }

    fn key(p: *const u8) -> u64 {
        unsafe { (p as *const u64).add(1).read() }
    }

    /// PG's merge join over the window, the machine's fetches only (the mark, the current row, the next): every outer row
    /// meets its equal inner rows, a repeated outer key restores the mark, a new one re-reads the mark and the current row
    /// (TESTOUTER). The matches per outer row.
    fn merge(r: &mut Rig, tbl: &[u32], n: u64, outer: &[u64]) -> Vec<usize> {
        let (mut ip, mut mark, mut prev, mut out) = (0u64, None::<u64>, None::<u64>, Vec::new()); // alloc: Cold
        for &ok in outer {
            if let (Some(m), Some(p)) = (mark, prev) {
                let mk = key(r.at(m, tbl));
                if p == ok {
                    assert_eq!(mk, ok);
                    let mut j = m;
                    while j < n && key(r.at(j, tbl)) == ok {
                        j += 1;
                    }
                    out.push((j - m) as usize); // alloc: Cold
                    ip = j;
                    continue;
                }
                if ip < n {
                    let _ = key(r.at(ip, tbl));
                }
            }
            while ip < n && key(r.at(ip, tbl)) < ok {
                ip += 1;
            }
            let start = ip;
            if ip < n && key(r.at(ip, tbl)) == ok {
                (mark, ip) = (Some(ip), ip);
                r.mark(ip);
                while ip < n && key(r.at(ip, tbl)) == ok {
                    ip += 1;
                }
            }
            out.push((ip - start) as usize); // alloc: Cold
            prev = Some(ok);
        }
        out
    }

    /// D3105 (b): fixed rows put in five sorted runs, merged into one and read through a window of about 40 rows: the
    /// merge join's matches are the sorted keys' (a group of 300 equal keys past the window restores by reopening the run
    /// at the mark), and a rescan from row 0 reopens it at its start.
    #[test]
    fn a_window_over_put_runs_reads_the_merge_joins_rows() {
        let _one = ONE.lock().expect("one");
        let tbl = [32u32, 2, 1, 0, 0, 0];
        let mut all: Vec<u64> = (0..2000u64).map(|i| i / 7).chain((0..300).map(|_| 150)).collect(); // alloc: Cold
        let mut runs: Vec<Vec<[u64; 4]>> = vec![Vec::new(); 5]; // alloc: Cold
        for (i, &k) in all.iter().enumerate() {
            runs[i * 7 % 5].push(fixed(k, i as u64)); // alloc: Cold
        }
        let puts = runs.iter_mut().map(|r| {
            r.sort_by_key(|x| x[1]);
            bytes(r)
        }).collect(); // alloc: Cold
        all.sort_unstable();
        let n = all.len() as u64;
        let mut r = Rig::new(2048, puts);
        let (c, own) = (r.c(), r.own());
        assert_eq!(unsafe { lay(c, own, tbl.as_ptr(), 1, n, false) }, own as *const u8);
        let w = unsafe { &*(own as *const Window) };
        assert!(w.idx_cap < 60 && w.len as u64 == n, "a small window: {} rows", w.idx_cap);
        let outer: Vec<u64> = [1u64, 1, 2, 5, 5, 5, 149, 150, 150, 150, 151, 151, 300, 400].to_vec(); // alloc: Cold
        let want: Vec<usize> = outer.iter().map(|&k| all.iter().filter(|&&x| x == k).count()).collect(); // alloc: Cold
        let opens = r.opens();
        assert_eq!(merge(&mut r, &tbl, n, &outer), want);
        assert!(r.opens() > opens + 1, "the group past the window restored by reopening the run");
        for j in 0..n {
            assert_eq!(key(r.at(j, &tbl)), all[j as usize], "row {j} after the rescan");
        }
        *FAKE.lock().expect("fake") = None;
    }

    /// Keyless variable records (a Material's: a 32-byte head, the record word at 16, a varlena behind the slot at 24), some
    /// longer than the window's buffer (read whole into the big buffer): the merge keeps the runs' order, and every row reads
    /// back as the merged run holds it, forward and from a mark the window left.
    #[test]
    fn a_window_reads_variable_and_big_records() {
        let _one = ONE.lock().expect("one");
        let mut tbl = [0u32; SORT_TBL_HDR + 4];
        tbl[0] = 32;
        tbl[2] = VAR_TBL;
        tbl[SORT_TBL_HDR..].copy_from_slice(&[1, 16, 0, 24]);
        let rec = |id: u64| -> Vec<u8> {
            let len = match id % 11 { 0 => 5000, k => 8 * k as usize };
            let total = 32 + len;
            let mut v = vec![0u8; total]; // alloc: Cold
            v[0..8].copy_from_slice(&id.to_le_bytes());
            v[16..24].copy_from_slice(&(total as u64).to_le_bytes());
            v[24..28].copy_from_slice(&(len as u32).to_le_bytes());
            v[28..32].copy_from_slice(&8u32.to_le_bytes());
            v[32..].fill((id % 251) as u8);
            v
        };
        let puts: Vec<Vec<u8>> = (0..3u64).map(|r| (0..120u64).flat_map(|i| rec(r * 1000 + i)).collect()).collect(); // alloc: Cold
        let n = 360u64;
        let mut r = Rig::new(4096, puts);
        let (c, own) = (r.c(), r.own());
        assert_eq!(unsafe { lay(c, own, tbl.as_ptr(), 1, n, false) }, own as *const u8);
        let merged = {
            let g = FAKE.lock().expect("fake");
            let w = unsafe { &*(own as *const Window) };
            run_of(g.as_ref().expect("a fake"), w.token)
        };
        let mut want = Vec::new(); // alloc: Cold
        let mut at = 0;
        while at < merged.len() {
            let len = u64::from_le_bytes(merged[at + 16..at + 24].try_into().expect("8")) as usize;
            want.push(merged[at..at + len].to_vec()); // alloc: Cold
            at += len;
        }
        assert_eq!(want.len() as u64, n);
        let ids: Vec<u64> = want.iter().map(|w| u64::from_le_bytes(w[0..8].try_into().expect("8"))).collect(); // alloc: Cold
        let order: Vec<u64> = (0..3u64).flat_map(|r| (0..120u64).map(move |i| r * 1000 + i)).collect(); // alloc: Cold
        assert_eq!(ids, order, "a keyless merge keeps the runs' order");
        let check = |r: &mut Rig, j: u64| {
            let p = r.at(j, &tbl);
            let w = &want[j as usize];
            assert_eq!(unsafe { core::slice::from_raw_parts(p, w.len()) }, &w[..], "row {j}");
        };
        for j in 0..200 {
            check(&mut r, j);
        }
        r.mark(37);
        check(&mut r, 37);
        for j in 200..n {
            check(&mut r, j);
        }
        check(&mut r, 37);
        check(&mut r, 38);
        *FAKE.lock().expect("fake") = None;
    }
}
