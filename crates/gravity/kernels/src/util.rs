//! The kernel helpers over the ABI accessors (docs/gravity/abi-v2.md §0): every function here
//! is loads and stores at fixed offsets plus the seven calls, one to one with a lowerer emit.
use gravity_abi::PtrWord;
use gravity_abi::ctx::{arena_alloc, cancel_pending, site_ptr, RESUME_NONE};
use gravity_abi::program;
use gravity_abi::site as abi_site;
use gravity_abi::status::{suspend, unsupported, CANCELLED, DONE, ERROR, OK, STOP, UNSUPPORTED_SCRATCH};
use gravity_abi::{Ctx, Datum, Lane, Site};

pub use gravity_abi::program::{consumed, partial};
pub use gravity_abi::site::{lane, next_row};

/// The window's iteration length: the selection's when one is set.
#[inline(always)]
pub const fn end(s: &Site) -> u32 {
    s.end()
}

/// The window row behind iteration index `j`.
#[inline(always)]
pub unsafe fn index(s: &Site, j: u32) -> usize {
    s.index(j)
}

/// What `emit` / `push` / `flush` return: the sink went on, or asked the producer to stop (the
/// host's `STOP` status, made a value here so `< 0` stays "failed" in every kernel).
pub const HOST_CONTINUE: i64 = 0;
pub const HOST_STOP: i64 = 1;
/// What `next` returns for a row (0 is the end of the probe).
pub const HOST_ROW: i64 = 1;
/// `write` / `lock` values.
pub const HOST_WRITTEN: i64 = gravity_abi::status::WRITE_DONE;
pub const HOST_SKIPPED: i64 = gravity_abi::status::WRITE_SKIPPED;
pub const HOST_RECHECK: i64 = gravity_abi::status::WRITE_RECHECK;

/// The resume word of a kernel that opened its source and is emitting: an `EMIT`-phase word
/// (§2 decision 9), so a suspend is legal for any pipeline over a finished breaker.
pub const RESUME_OPEN: i64 = gravity_abi::ctx::resume::word(gravity_abi::ctx::resume::EMIT, 1);

/// D1705: the host rewrites the block in place (`window`, `seek`), which disables this reference (Tree Borrows):
/// re-derive it after every host call, never hold it across one.
pub unsafe fn site<'a>(ctx: *mut Ctx, i: usize) -> &'a mut Site {
    &mut *site_ptr(ctx, i)
}

pub unsafe fn copy_row(dst_v: *mut Datum, dst_n: *mut u8, src_v: *const Datum, src_n: *const u8, n: usize) {
    for k in 0..n {
        *dst_v.add(k) = *src_v.add(k);
        *dst_n.add(k) = *src_n.add(k);
    }
}

pub unsafe fn key_from_param(ctx: *mut Ctx, s: &mut Site, k: usize, p: usize) {
    *s.keys.ptr().add(k) = *(*ctx).params.ptr().add(p);
    *s.keys_null.ptr().add(k) = *(*ctx).params_null.ptr().add(p);
}

pub unsafe fn keys_from_params(ctx: *mut Ctx, s: &mut Site, slots: &[usize]) {
    for (k, &p) in slots.iter().enumerate() {
        key_from_param(ctx, s, k, p);
    }
}

/// Ask site `i` to seek from its key cells at its next fill (the source seeks inside `next`).
pub unsafe fn seek(ctx: *mut Ctx, i: usize) -> i64 {
    abi_site::seek(site_ptr(ctx, i));
    OK
}

/// A range site's claim as its keys, then the seek request.
pub unsafe fn range_seek(ctx: *mut Ctx, i: usize) -> i64 {
    let s = site(ctx, i);
    let c = (*ctx).claim;
    *s.keys.ptr() = c.lo;
    *s.keys.ptr().add(1) = c.hi;
    *s.keys_null.ptr() = 0;
    *s.keys_null.ptr().add(1) = 0;
    seek(ctx, i)
}

/// The row protocol: one row of site `i` into its `values` / `isnull`; `HOST_ROW`, 0, or a status.
pub unsafe fn next(ctx: *mut Ctx, i: usize) -> i64 {
    let s = site_ptr(ctx, i);
    next_row(s, (*s).natts as usize)
}

/// The next window of site `i`: rows, 0 at the end, or a status.
pub unsafe fn window(ctx: *mut Ctx, i: usize) -> i64 {
    abi_site::next(site_ptr(ctx, i))
}

/// A lane element read as int4 / int8: `i16` lanes (width 2), `i32` lanes (width 4) and datum-word lanes (width 8).
pub trait Cell: Copy {
    fn i32(self) -> i32;
    fn i64(self) -> i64;
}

impl Cell for i16 {
    #[inline(always)]
    fn i32(self) -> i32 {
        self as i32
    }

    #[inline(always)]
    fn i64(self) -> i64 {
        self as i64
    }
}

/// Monomorphise `$f::<T>($($a),*)` on a lane's width: 2 → `i16`, 4 → `i32`, else the datum word.
#[macro_export]
macro_rules! by_width {
    ($w:expr, $f:ident :: < $($pre:ty),* > ( $($a:expr),* )) => {
        match $w {
            2 => $f::<$($pre,)* i16>($($a),*),
            4 => $f::<$($pre,)* i32>($($a),*),
            _ => $f::<$($pre,)* u64>($($a),*),
        }
    };
    ($w:expr, $f:ident ( $($a:expr),* )) => {
        match $w {
            2 => $f::<i16>($($a),*),
            4 => $f::<i32>($($a),*),
            _ => $f::<u64>($($a),*),
        }
    };
}

impl Cell for i32 {
    #[inline(always)]
    fn i32(self) -> i32 {
        self
    }

    #[inline(always)]
    fn i64(self) -> i64 {
        self as i64
    }
}

impl Cell for u64 {
    #[inline(always)]
    fn i32(self) -> i32 {
        self as u32 as i32
    }

    #[inline(always)]
    fn i64(self) -> i64 {
        self as i64
    }
}

#[inline(always)]
pub unsafe fn row_valid(valid: *const u64, i: usize) -> bool {
    valid.is_null() || (*valid.add(i / 64) >> (i % 64)) & 1 != 0
}

/// Rows `cursor..end` of the site's window through `f(i)` until it says stop; true when every row ran.
#[inline(always)]
pub unsafe fn for_each_row(s: &Site, mut f: impl FnMut(usize) -> bool) -> bool {
    let (from, to) = (s.cursor, s.end());
    if s.sel.ptr().is_null() {
        for i in from..to {
            if !f(i as usize) {
                return false;
            }
        }
    } else {
        for j in from..to {
            if !f(*s.sel.ptr().add(j as usize) as usize) {
                return false;
            }
        }
    }
    true
}

/// The next window when the current one is spent: rows left to process
/// (> 0), 0 at the claim's end (state `EOF`), a status below zero.
/// The abort byte is read once per window.
pub unsafe fn advance(ctx: *mut Ctx, i: usize) -> i64 {
    loop {
        let s = site_ptr(ctx, i);
        let (cursor, end) = ((*s).cursor, (*s).end());
        if cursor < end {
            return (end - cursor) as i64;
        }
        let n = window(ctx, i);
        if n < 0 {
            if matches!(gravity_abi::status::Status::decode(n), gravity_abi::status::Status::Bad(_)) {
                gravity_fn::stash_error(Box::new(gravity_fn::PgError::error(&format!("gravity: advance: site {i}'s window returned {n:#x}")))); // alloc: Cold
                return ERROR;
            }
            return n;
        }
        if n == 0 {
            (*site_ptr(ctx, i)).state = abi_site::site::STATE_EOF;
            return 0;
        }
        if interrupted(ctx) {
            return CANCELLED;
        }
    }
}

pub unsafe fn write(ctx: *mut Ctx, i: usize, op: u32) -> i64 {
    program::write(ctx, op, i as u64)
}

pub unsafe fn lock(ctx: *mut Ctx, i: usize) -> i64 {
    program::lock(ctx, i as u64)
}

/// `Program.spill` with the kernels' signed argument words.
pub unsafe fn spill(ctx: *mut Ctx, op: u32, arg: i64) -> i64 {
    program::spill(ctx, op, arg as u64)
}

pub unsafe fn out_count(ctx: *mut Ctx) -> i64 {
    (*(*ctx).out.ptr()).count
}

/// Push the out block: `HOST_CONTINUE` / `HOST_STOP`, or a status below zero.
pub unsafe fn push(ctx: *mut Ctx) -> i64 {
    match program::push(ctx) {
        STOP => HOST_STOP,
        v => v,
    }
}

pub unsafe fn flush(ctx: *mut Ctx) -> i64 {
    if out_count(ctx) > 0 {
        push(ctx)
    } else {
        HOST_CONTINUE
    }
}

pub unsafe fn done(ctx: *mut Ctx) -> i64 {
    (*ctx).resume = RESUME_NONE;
    if flush(ctx) < 0 {
        ERROR
    } else {
        DONE
    }
}

pub unsafe fn stopped(ctx: *mut Ctx) -> i64 {
    suspend(out_count(ctx) == 0)
}

pub unsafe fn emit(ctx: *mut Ctx, natts: usize, parts: &[(*const Datum, *const u8, usize)]) -> i64 {
    let o = &mut *(*ctx).out.ptr();
    let row = o.count as usize;
    let mut col = 0;
    for &(v, n, cnt) in parts {
        copy_row(o.values.ptr().add(row * natts + col), o.isnull.ptr().add(row * natts + col), v, n, cnt);
        col += cnt;
    }
    o.count += 1;
    if o.count >= o.cap {
        push(ctx)
    } else {
        HOST_CONTINUE
    }
}

/// The cancellation test: one load of the abort byte.
#[inline(always)]
pub unsafe fn interrupted(ctx: *mut Ctx) -> bool {
    cancel_pending(ctx)
}

/// `bytes` of arena scratch (the zeroed scratch region first, then chunks); null when refused.
pub unsafe fn scratch(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    arena_alloc(ctx, bytes)
}

pub unsafe fn probe_emit(ctx: *mut Ctx, slots: &[usize], natts: usize) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        keys_from_params(ctx, site(ctx, 0), slots);
        seek(ctx, 0);
        (*ctx).resume = RESUME_OPEN;
    }
    loop {
        if interrupted(ctx) {
            return CANCELLED;
        }
        let r = next(ctx, 0);
        if r < 0 {
            return r;
        }
        if r != HOST_ROW {
            return done(ctx);
        }
        let s = site(ctx, 0);
        let f = emit(ctx, natts, &[(s.values.ptr(), s.isnull.ptr(), natts)]);
        if f < 0 {
            return f;
        }
        if f == HOST_STOP {
            return stopped(ctx);
        }
    }
}

/// `int4pl`: `None` is the incumbent's 22003 (`status::raised(raise::bit(raise::INT4_RANGE))`).
pub const fn int4_add(datum: Datum, k: i32) -> Option<Datum> {
    match (datum as u32 as i32).checked_add(k) {
        Some(v) => Some(v as i64 as u64),
        None => None,
    }
}

pub const fn int2_of(datum: Datum) -> i16 {
    datum as u16 as i16
}

pub const fn int4_of(datum: Datum) -> i32 {
    datum as u32 as i32
}

pub const fn int8_datum(v: i64) -> Datum {
    v as u64
}

pub struct ScratchSet {
    slots: *mut u32,
    cap: usize,
    pub len: usize,
}

impl ScratchSet {
    pub const SLOT_BYTES: usize = 4;

    /// # Safety
    /// `ctx` has at least `cap * 4` zeroed bytes of scratch.
    pub unsafe fn in_scratch(ctx: *mut Ctx, cap: usize) -> Option<ScratchSet> {
        let p = scratch(ctx, cap * ScratchSet::SLOT_BYTES);
        if p.is_null() {
            return None;
        }
        Some(ScratchSet { slots: p as *mut u32, cap, len: 0 })
    }

    pub fn insert(&mut self, key: u32) -> Result<bool, i64> {
        let tag = key.wrapping_add(1);
        let mut i = (key.wrapping_mul(0x9E37_79B9) as usize) & (self.cap - 1);
        for _ in 0..self.cap {
            let slot = unsafe { self.slots.add(i) };
            let v = unsafe { *slot };
            if v == tag {
                return Ok(false);
            }
            if v == 0 {
                unsafe { *slot = tag };
                self.len += 1;
                return Ok(true);
            }
            i = (i + 1) & (self.cap - 1);
        }
        Err(unsupported(UNSUPPORTED_SCRATCH))
    }
}

#[allow(dead_code)]
const _: fn(&Lane) = |_| {};

/// A varlena datum's payload bytes (4-byte header; moved from the deleted hand frame kernel's module, 09-29).
#[inline(always)]
pub unsafe fn text_bytes<'a>(d: u64) -> &'a [u8] {
    let p = d as *const u8;
    core::slice::from_raw_parts(p.add(4), (*(p as *const u32) >> 2) as usize - 4)
}
