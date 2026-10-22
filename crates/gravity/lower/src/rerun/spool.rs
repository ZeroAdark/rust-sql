//! The spool (Materialize) face, lower.md §15.20: a producer that appends rows to a run buffer and puts full runs
//! to the slot's run stream, and a forward reader over that stream with a resumable, restorable cursor. A helper
//! module the rungs call; not a rung.
//!
//! A run is an abi `SortRun` of 8-byte "rows" (`row_bytes` 8: `cap` / `len` count words), so the sort's run hosts
//! carry it unchanged. A spooled row is `[words][natts values][null bytes, 8-padded][by-reference images, 8-aligned]`;
//! a by-reference datum is its image's byte offset from the row, so a run is position-independent.
#![allow(unsafe_code)]

use gravity_abi::breaker::{SortRun, SORTRUN_HDR};
use gravity_abi::ctx::RESUME_NONE;
use gravity_abi::program::{consumed, partial, SPILL_RUNS, SPILL_RUN_BASE, SPILL_RUN_CLOSE, SPILL_RUN_FILL, SPILL_RUN_OPEN, SPILL_RUN_PUT};
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_SCRATCH};
use gravity_abi::Ctx;
use gravity_expr::byref::{self, ByRef};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::util::{done, emit, spill, stopped};

/// The run's `cap` and `len` words (abi `SortRun`, repr(C)): their offsets from the layout, not literals.
const RUN_CAP: i32 = core::mem::offset_of!(SortRun, cap) as i32;
const RUN_LEN: i32 = core::mem::offset_of!(SortRun, len) as i32;
/// One `SPILL_RUN_FILL` carries its byte count in 16 bits.
const FILL_MAX: u64 = 0xfff8;
const MAGIC: u64 = 0x5350_4f4f_4c52_4431;

/// The words of a row's fixed part: the length word, the values, the null bytes.
pub const fn fixed_words(natts: usize) -> usize {
    1 + natts + natts.div_ceil(8)
}

/// A row as the producer stores it (the rig's fixture and the tests' oracle): `images[k]` a by-reference column's bytes.
pub fn encode_row(out: &mut Vec<u8>, values: &[u64], nulls: &[u8], images: &[Option<&[u8]>]) {
    let natts = values.len();
    let mut words = fixed_words(natts);
    let mut row = vec![0u64; words]; // alloc: Cold
    for k in 0..natts {
        if nulls[k] == 0 {
            row[1 + k] = match images[k] {
                Some(img) => {
                    let at = words * 8;
                    let w = img.len().div_ceil(8);
                    row.resize(words + w, 0);
                    // SAFETY: `w` words were just appended at `words`.
                    unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), row.as_mut_ptr().add(words) as *mut u8, img.len()) };
                    words += w;
                    at as u64
                }
                None => values[k],
            };
        }
    }
    row[0] = words as u64;
    let nb = (1 + natts) * 8;
    out.extend(row.iter().flat_map(|w| w.to_ne_bytes()));
    let start = out.len() - words * 8;
    out[start + nb..start + nb + natts].copy_from_slice(nulls);
}

// ---- the producer ----

unsafe fn put(ctx: *mut Ctx, run: *mut SortRun) -> u64 {
    if (*run).len == 0 {
        return 0;
    }
    if spill(ctx, SPILL_RUN_PUT, 0) < 0 {
        return ERROR as u64;
    }
    (*run).clear();
    0
}

gravity_abi::host! {
    /// The pipeline's own partial armed as a run of words; at the section end (`Pipeline.finish`) the last put, then 0.
    unsafe fn h_spool_open(ctx: u64) -> u64 {
        let ctx = ctx as *mut Ctx;
        let run = SortRun::at_share(partial(ctx), 8) as *mut SortRun;
        if (*ctx).claim.is_section_end() {
            return put(ctx, run);
        }
        run as u64
    }
}

gravity_abi::host! {
    /// The run put to the slot's run stream and emptied: one call per full run.
    unsafe fn h_spool_full(ctx: u64, run: u64) -> u64 {
        put(ctx as *mut Ctx, run as *mut SortRun)
    }
}

#[derive(Clone, Copy)]
pub struct Producer {
    open: HostFn,
    full: HostFn,
}

/// A spooled column: `typlen` `None` by value, else the by-reference type's (`-1` a varlena, `> 0` fixed).
#[derive(Clone, Copy)]
pub struct Col {
    pub value: Value,
    pub valid: Value,
    pub typlen: Option<i16>,
}

pub fn declare_producer(pb: &mut ProgramBuilder) -> Producer {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    Producer { open: pb.declare_host("spool_open", &[Type::Ptr], Some(Type::I64), h_spool_open as usize, mem), full: pb.declare_host("spool_full", &[Type::Ptr, Type::Ptr], Some(Type::I64), h_spool_full as usize, mem) }
}

/// `spool_open`'s word: `<= 0` the pipeline leaves with it (0: the section end's put is done), else the run.
pub fn open(b: &mut FunctionBuilder<'_>, p: &Producer, ctx: Value) -> Value {
    b.call_host(p.open, &[ctx], Some(Type::I64)).unwrap()
}

/// Append one row; a full run is put first (`spool_full`), a row wider than an empty run leaves through
/// `unsupported(UNSUPPORTED_SCRATCH)`. `l_exit` takes the status. The builder continues after the store.
pub fn push(b: &mut FunctionBuilder<'_>, p: &Producer, ctx: Value, run: Value, cols: &[Col], l_exit: Block) {
    let natts = cols.len();
    let fixed = fixed_words(natts) as i64 * 8;
    let zero = b.iconst(Type::I64, 0);
    let refs: Vec<ByRef> = cols.iter().filter_map(|c| c.typlen.map(|typlen| ByRef { value: c.value, valid: c.valid, typlen })).collect(); // alloc: Program
    let (per, total) = byref::sizes(b, &refs);
    let fixed_v = b.iconst(Type::I64, fixed);
    let bytes = b.int_bin(IntBinOp::Add, fixed_v, total);
    let three = b.iconst(Type::I64, 3);
    let words = b.int_bin(IntBinOp::Ushr, bytes, three);
    let l_full = b.create_block(&[]);
    let l_wide = b.create_block(&[]);
    let l_store = b.create_block(&[Type::I64]);
    b.mark_cold(l_full);
    b.mark_cold(l_wide);
    let len = b.load_zext(Type::I32, run, RUN_LEN);
    let cap = b.load_zext(Type::I32, run, RUN_CAP);
    let after = b.int_bin(IntBinOp::Add, len, words);
    let fits = b.icmp(IntCond::Ule, after, cap);
    b.cond_br(fits, (l_store, &[len]), (l_full, &[]));

    b.switch_to(l_full);
    let r = b.call_host(p.full, &[ctx, run], Some(Type::I64)).unwrap();
    let bad = b.icmp(IntCond::Slt, r, zero);
    let l_check = b.create_block(&[]);
    b.cond_br(bad, (l_exit, &[r]), (l_check, &[]));
    b.switch_to(l_check);
    let wide = b.icmp(IntCond::Ugt, words, cap);
    b.cond_br(wide, (l_wide, &[]), (l_store, &[zero]));
    b.switch_to(l_wide);
    let refuse = b.iconst(Type::I64, unsupported(UNSUPPORTED_SCRATCH));
    b.br(l_exit, &[refuse]);

    b.switch_to(l_store);
    let len = b.block_params(l_store)[0];
    let row = b.ptr_add(run, len, 8, SORTRUN_HDR as i32);
    b.store(row, 0, words);
    for w in 0..natts.div_ceil(8) {
        b.store(row, ((1 + natts + w) * 8) as i32, zero);
    }
    let t = b.bconst(true);
    let mut at = fixed_v;
    let mut r = 0;
    for (k, c) in cols.iter().enumerate() {
        let nv = b.int_bin(IntBinOp::Xor, c.valid, t);
        let byte = b.conv(ConvOp::Zext, Type::I8, nv);
        b.store(row, ((1 + natts) * 8 + k) as i32, byte);
        let datum = if let Some(typlen) = c.typlen {
            let sz = per[r];
            r += 1;
            copy_image(b, row, at, c.value, c.valid, typlen, sz.len);
            let d = at;
            at = b.int_bin(IntBinOp::Add, at, sz.size);
            d
        } else {
            c.value
        };
        let word = b.select(c.valid, datum, zero);
        b.store(row, ((1 + k) * 8) as i32, word);
    }
    let len1 = b.int_bin(IntBinOp::Add, len, words);
    let len32 = b.conv(ConvOp::Trunc, Type::I32, len1);
    b.store(run, RUN_LEN, len32);
}

/// The `len`-byte image at `src` copied to `row + at` when `valid`, as `byref::store` copies it (no byte past the image is
/// read, D3661).
fn copy_image(b: &mut FunctionBuilder<'_>, row: Value, at: Value, src: Value, valid: Value, typlen: i16, len: Value) {
    let (l_copy, l_done) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(valid, (l_copy, &[]), (l_done, &[]));
    b.switch_to(l_copy);
    let src = b.conv(ConvOp::IntToPtr, Type::Ptr, src);
    let dst = b.ptr_add(row, at, 1, 0);
    match typlen {
        n if n > 0 => byref::copy_fixed(b, dst, 0, src, n),
        _ => byref::copy_image(b, dst, src, len),
    }
    b.br(l_done, &[]);
    b.switch_to(l_done);
}

/// A producer without a section end (a serial pipeline, no `finishing`): the last run put before it returns.
pub fn finish(b: &mut FunctionBuilder<'_>, p: &Producer, ctx: Value, run: Value, l_exit: Block) {
    let zero = b.iconst(Type::I64, 0);
    let r = b.call_host(p.full, &[ctx, run], Some(Type::I64)).unwrap();
    let bad = b.icmp(IntCond::Slt, r, zero);
    let l_ok = b.create_block(&[]);
    b.cond_br(bad, (l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
}

// ---- the reader ----

/// The reader's state at the front of its buffer; the cursor is `(run, off + pos)`.
#[repr(C)]
pub struct Reader {
    pub buf: u64,
    pub pos: u64,
    pub end: u64,
    pub cap: u64,
    pub run: u32,
    pub nruns: u32,
    /// The run-relative byte offset of `buf[0]`.
    pub off: u64,
    pub handle: i32,
    pub stream: u32,
    pub magic: u64,
}

const RD_BUF: i32 = 0;
const RD_POS: i32 = 8;
const RD_END: i32 = 16;
const RD_RUN: i32 = 32;
const RD_OFF: i32 = 40;
pub const READER_HDR: usize = core::mem::size_of::<Reader>();

unsafe fn lay(ctx: *mut Ctx, base: *mut u8, bytes: usize, stream: u64) -> u64 {
    let rd = base as *mut Reader;
    if (*ctx).resume != RESUME_NONE && (*rd).magic == MAGIC {
        return base as u64;
    }
    if bytes < READER_HDR + 16 || (base as usize) % 8 != 0 {
        return unsupported(UNSUPPORTED_SCRATCH) as u64;
    }
    let nruns = spill(ctx, SPILL_RUNS, stream as i64);
    if nruns < 0 {
        return ERROR as u64;
    }
    *rd = Reader { buf: base.add(READER_HDR) as u64, pos: 0, end: 0, cap: ((bytes - READER_HDR) & !7) as u64, run: 0, nruns: nruns as u32, off: 0, handle: -1, stream: stream as u32, magic: MAGIC };
    base as u64
}

gravity_abi::host! {
    /// The reader laid over the `k`-th consumed slot's leader region (its run buffer, empty once put): a serial consumer's.
    unsafe fn h_reader_open(ctx: u64, k: u64, stream: u64) -> u64 {
        let ctx = ctx as *mut Ctx;
        let base = consumed(ctx, k as usize);
        if base.is_null() {
            return ERROR as u64;
        }
        let live = (*ctx).resume != RESUME_NONE && (*(base as *const Reader)).magic == MAGIC;
        let bytes = if live { 0 } else { SortRun::region_bytes(base, 8) };
        lay(ctx, base, bytes, stream)
    }
}

gravity_abi::host! {
    /// The reader laid over `bytes` at `base` the rung owns (its arena: a worker's private buffer).
    unsafe fn h_reader_open_at(ctx: u64, base: u64, bytes: u64, stream: u64) -> u64 {
        lay(ctx as *mut Ctx, base as *mut u8, bytes as usize, stream)
    }
}

unsafe fn whole_row(r: &Reader) -> Option<bool> {
    if r.pos + 8 > r.end {
        return Some(false);
    }
    let bytes = *((r.buf + r.pos) as *const u64) * 8;
    if bytes == 0 || bytes > r.cap {
        return None;
    }
    Some(r.pos + bytes <= r.end)
}

gravity_abi::host! {
    /// The unread tail moved to the front and the buffer filled behind it, the next run opened when one ends:
    /// 1 a whole row at `pos`, 0 the spool's end, `< 0` an error. One call per buffer, never per row.
    unsafe fn h_reader_fill(ctx: u64, rd: u64) -> u64 {
        let (ctx, r) = (ctx as *mut Ctx, &mut *(rd as *mut Reader));
        let buf = r.buf as *mut u8;
        loop {
            match whole_row(r) {
                None => return unsupported(UNSUPPORTED_SCRATCH) as u64,
                Some(true) => return 1,
                Some(false) => {}
            }
            let tail = r.end - r.pos;
            if r.pos > 0 {
                core::ptr::copy(buf.add(r.pos as usize), buf, tail as usize);
                r.off += r.pos;
                r.pos = 0;
                r.end = tail;
            }
            if r.handle < 0 {
                if r.run >= r.nruns {
                    return if tail == 0 { 0 } else { ERROR as u64 };
                }
                let h = spill(ctx, SPILL_RUN_OPEN, (r.run as u64 | (r.stream as u64) << 32) as i64);
                if h < 0 {
                    return ERROR as u64;
                }
                r.handle = h as i32;
                r.off = 0;
            }
            let want = (r.cap - r.end).min(FILL_MAX);
            if spill(ctx, SPILL_RUN_BASE, buf as i64) < 0 {
                return ERROR as u64;
            }
            let n = spill(ctx, SPILL_RUN_FILL, (r.handle as u64 | r.end << 16 | want << 48) as i64);
            spill(ctx, SPILL_RUN_BASE, 0);
            if n < 0 {
                return ERROR as u64;
            }
            if n == 0 {
                if tail != 0 {
                    return ERROR as u64;
                }
                r.handle = -1;
                r.run += 1;
            }
            r.end += n as u64;
        }
    }
}

gravity_abi::host! {
    /// The cursor put back at `(run, off)` (a `mark`): inside the buffer a move of `pos`, else the run reopened and
    /// read forward to it.
    unsafe fn h_reader_seek(ctx: u64, rd: u64, run: u64, off: u64) -> u64 {
        let (ctx, r) = (ctx as *mut Ctx, &mut *(rd as *mut Reader));
        if run as u32 == r.run && off >= r.off && off <= r.off + r.end {
            r.pos = off - r.off;
            return 0;
        }
        if r.handle >= 0 && spill(ctx, SPILL_RUN_CLOSE, r.handle as i64) < 0 {
            return ERROR as u64;
        }
        (r.handle, r.run, r.pos, r.end, r.off) = (-1, run as u32, 0, 0, 0);
        while r.off + r.end < off || r.handle < 0 {
            r.pos = r.end;
            let tail_run = r.run;
            let got = h_reader_fill(ctx as u64, rd) as i64;
            if got < 0 {
                return got as u64;
            }
            if got == 0 || r.run != tail_run {
                return ERROR as u64;
            }
            if r.off + r.end >= off && r.off <= off {
                break;
            }
        }
        r.pos = off - r.off;
        0
    }
}

gravity_abi::host! {
    unsafe fn h_emit(ctx: u64, v: u64, nl: u64, n: u64) -> u64 {
        emit(ctx as *mut Ctx, n as usize, &[(v as *const u64, nl as *const u8, n as usize)]) as u64
    }
}

gravity_abi::host! {
    unsafe fn h_stopped(ctx: u64) -> u64 {
        stopped(ctx as *mut Ctx) as u64
    }
}

gravity_abi::host! {
    unsafe fn h_done(ctx: u64) -> u64 {
        done(ctx as *mut Ctx) as u64
    }
}

#[derive(Clone, Copy)]
pub struct ReaderHosts {
    open: HostFn,
    open_at: HostFn,
    fill: HostFn,
    seek: HostFn,
}

/// The sink hosts a replaying consumer ends with (`emit(ctx, values, nulls, n)`: 0 go on, `> 0` the sink's stop).
#[derive(Clone, Copy)]
pub struct Sink {
    pub emit: HostFn,
    pub stopped: HostFn,
    pub done: HostFn,
}

pub fn declare_reader(pb: &mut ProgramBuilder) -> ReaderHosts {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    ReaderHosts {
        open: pb.declare_host("spool_reader", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), h_reader_open as usize, mem),
        open_at: pb.declare_host("spool_reader_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), h_reader_open_at as usize, mem),
        fill: pb.declare_host("spool_fill", &[Type::Ptr, Type::Ptr], Some(Type::I64), h_reader_fill as usize, mem),
        seek: pb.declare_host("spool_seek", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), h_reader_seek as usize, mem),
    }
}

pub fn declare_sink(pb: &mut ProgramBuilder) -> Sink {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    Sink { emit: pb.declare_host("spool_emit", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), h_emit as usize, mem), stopped: pb.declare_host("spool_stopped", &[Type::Ptr], Some(Type::I64), h_stopped as usize, mem), done: pb.declare_host("spool_done", &[Type::Ptr], Some(Type::I64), h_done as usize, mem) }
}

fn opened(b: &mut FunctionBuilder<'_>, r: Value, l_exit: Block) -> Value {
    let zero = b.iconst(Type::I64, 0);
    let bad = b.icmp(IntCond::Sle, r, zero);
    let err = b.iconst(Type::I64, ERROR);
    let none = b.icmp(IntCond::Eq, r, zero);
    let st = b.select(none, err, r);
    let l_ok = b.create_block(&[]);
    b.cond_br(bad, (l_exit, &[st]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.conv(ConvOp::IntToPtr, Type::Ptr, r)
}

/// The reader over the pipeline's `k`-th consumed slot (`stream` = that slot + 1): fresh on a fresh call, the live
/// cursor on a resumed one (the sink's stop mid-read).
pub fn reader_open(b: &mut FunctionBuilder<'_>, h: &ReaderHosts, ctx: Value, k: usize, stream: u32, l_exit: Block) -> Value {
    let kv = b.iconst(Type::I64, k as i64);
    let sv = b.iconst(Type::I64, stream as i64);
    let r = b.call_host(h.open, &[ctx, kv, sv], Some(Type::I64)).unwrap();
    opened(b, r, l_exit)
}

/// `reader_open` over a buffer the rung owns (`bytes` at `base`, 8-aligned, stable across a resume).
pub fn reader_open_at(b: &mut FunctionBuilder<'_>, h: &ReaderHosts, ctx: Value, base: Value, bytes: Value, stream: u32, l_exit: Block) -> Value {
    let sv = b.iconst(Type::I64, stream as i64);
    let r = b.call_host(h.open_at, &[ctx, base, bytes, sv], Some(Type::I64)).unwrap();
    opened(b, r, l_exit)
}

/// The next row: `(has, row)`; the row's bytes live until the next `next` / `restore`. The fast path is IR over the
/// buffer; `spool_fill` once per buffer.
pub fn next(b: &mut FunctionBuilder<'_>, h: &ReaderHosts, ctx: Value, rd: Value, l_exit: Block) -> (Value, Value) {
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let eight = b.iconst(Type::I64, 8);
    let three = b.iconst(Type::I64, 3);
    let l_len = b.create_block(&[]);
    let l_fill = b.create_block(&[]);
    let l_filled = b.create_block(&[]);
    let l_take = b.create_block(&[]);
    let l_out = b.create_block(&[Type::I64, Type::I64]);
    b.mark_cold(l_fill);
    let pos = b.load(Type::I64, rd, RD_POS);
    let end = b.load(Type::I64, rd, RD_END);
    let hdr_end = b.int_bin(IntBinOp::Add, pos, eight);
    let has_len = b.icmp(IntCond::Ule, hdr_end, end);
    b.cond_br(has_len, (l_len, &[]), (l_fill, &[]));

    b.switch_to(l_len);
    let buf = b.load(Type::Ptr, rd, RD_BUF);
    let p = b.ptr_add(buf, pos, 1, 0);
    let words = b.load(Type::I64, p, 0);
    let bytes = b.int_bin(IntBinOp::Shl, words, three);
    let row_end = b.int_bin(IntBinOp::Add, pos, bytes);
    let whole = b.icmp(IntCond::Ule, row_end, end);
    let some = b.icmp(IntCond::Ne, words, zero);
    let ok = b.select(some, whole, some);
    b.cond_br(ok, (l_take, &[]), (l_fill, &[]));

    b.switch_to(l_fill);
    let r = b.call_host(h.fill, &[ctx, rd], Some(Type::I64)).unwrap();
    let bad = b.icmp(IntCond::Slt, r, zero);
    b.cond_br(bad, (l_exit, &[r]), (l_filled, &[]));
    b.switch_to(l_filled);
    let at_end = b.icmp(IntCond::Eq, r, zero);
    b.cond_br(at_end, (l_out, &[zero, zero]), (l_take, &[]));

    b.switch_to(l_take);
    let pos = b.load(Type::I64, rd, RD_POS);
    let buf = b.load(Type::Ptr, rd, RD_BUF);
    let p = b.ptr_add(buf, pos, 1, 0);
    let words = b.load(Type::I64, p, 0);
    let bytes = b.int_bin(IntBinOp::Shl, words, three);
    let npos = b.int_bin(IntBinOp::Add, pos, bytes);
    b.store(rd, RD_POS, npos);
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    b.br(l_out, &[one, pi]);

    b.switch_to(l_out);
    let (has, row) = (b.block_params(l_out)[0], b.block_params(l_out)[1]);
    let has = b.icmp(IntCond::Ne, has, zero);
    let row = b.conv(ConvOp::IntToPtr, Type::Ptr, row);
    (has, row)
}

/// Column `k` of `natts` at `row`: `(datum, valid)`; a by-reference datum is the image's address inside the row.
pub fn col(b: &mut FunctionBuilder<'_>, row: Value, natts: usize, k: usize, by_ref: bool) -> (Value, Value) {
    let zero = b.iconst(Type::I64, 0);
    let word = b.load(Type::I64, row, ((1 + k) * 8) as i32);
    let nb = b.load_zext(Type::I8, row, ((1 + natts) * 8 + k) as i32);
    let valid = b.icmp(IntCond::Eq, nb, zero);
    if !by_ref {
        return (word, valid);
    }
    let p = b.ptr_add(row, word, 1, 0);
    let addr = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let datum = b.select(valid, addr, zero);
    (datum, valid)
}

/// The cursor of the row the next `next` returns: `(run, byte offset in the run)`.
pub fn mark(b: &mut FunctionBuilder<'_>, rd: Value) -> (Value, Value) {
    let run = b.load_zext(Type::I32, rd, RD_RUN);
    let off = b.load(Type::I64, rd, RD_OFF);
    let pos = b.load(Type::I64, rd, RD_POS);
    let at = b.int_bin(IntBinOp::Add, off, pos);
    (run, at)
}

/// Back to a `mark` (a merge join's equal-key group read again): no copy, a host call per restore, never per row.
pub fn restore(b: &mut FunctionBuilder<'_>, h: &ReaderHosts, ctx: Value, rd: Value, run: Value, at: Value, l_exit: Block) {
    let zero = b.iconst(Type::I64, 0);
    let r = b.call_host(h.seek, &[ctx, rd, run, at], Some(Type::I64)).unwrap();
    let bad = b.icmp(IntCond::Slt, r, zero);
    let l_ok = b.create_block(&[]);
    b.cond_br(bad, (l_exit, &[r]), (l_ok, &[]));
    b.switch_to(l_ok);
}
