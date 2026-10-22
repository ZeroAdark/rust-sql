//! dense_bits.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs), moved unchanged from it.
//! The parent re-exports these names, so its callers and the build region's raw.rs read them as before.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// The directory bytes of region `d`'s `DBITS` form (0: the records expand). Safety: `d` is a compact build's header.
pub(crate) unsafe fn need(d: *const u8) -> usize {
    let ok = rd::<u8>(d, ALLOW) & A_DENSE != 0 && rd::<u8>(d, FLAGS) & F_RANGE != 0;
    let planes = |s: usize| match (rd::<u8>(d, PAY), vbits(rd::<u8>(d, VBITS))) {
        (0, _) => bitmap(s),
        (_, WIDE) => 2 * bitmap(s) + (2 * s).next_multiple_of(64),
        _ => 3 * bitmap(s),
    };
    span(rd::<i64>(d, LO), rd::<i64>(d, HI), rd::<u64>(d, N) as usize).filter(|_| ok).map_or(0, planes)
}

/// Region `t`'s bitmaps zeroed at `t.dir()` and the form set before any `put`. Safety: `need(t.d) > 0` bytes are free there.
pub(crate) unsafe fn prepare(t: &Rt) {
    let (at, s) = (t.dir(), bitmap(span(rd::<i64>(t.d, LO), rd::<i64>(t.d, HI), t.n).unwrap_or(0)));
    core::ptr::write_bytes(t.d.add(at), 0, need(t.d));
    (wr::<u64>(t.d, DIR, at as u64), wr::<u64>(t.d, SLOTS, (at + s) as u64), wr::<u8>(t.d, crate::ops::hashjoin::build::FORM, DBITS));
}

/// Records `r` of `src` (the region itself, or a worker's: build_part.rs) into `t`'s bitmaps, one atomic OR per 64-key word
/// and bitmap (partition jobs share the words at their edges), a wide value into its key's own plane entry; a duplicate key
/// flags `t` `F_DUP`. Safety: `t` is `prepare`d.
pub(crate) unsafe fn put(t: &Rt, src: &Rt, r: core::ops::Range<usize>) {
    let (dir, stride, lo) = (rd::<u64>(t.d, DIR) as usize, (rd::<u64>(t.d, SLOTS) - rd::<u64>(t.d, DIR)) as usize, rd::<i64>(t.d, LO));
    let wide = rd::<u8>(t.d, PAY) != 0 && vbits(rd::<u8>(t.d, VBITS)) == WIDE;
    let or = |at: usize, m: [u64; 3]| (0..3).filter(|&j| m[j] != 0).fold(false, |dup, j| {
        (*(t.d.add(at + j * stride) as *const AtomicU64)).fetch_or(m[j], Relaxed) & m[j] != 0 && j == 0 || dup
    });
    let (mut at, mut m, mut dup) = (0, [0u64; 3], false);
    for i in r {
        let (k, w) = ((src.key(i) - lo) as usize, rd::<u64>(src.rec(i), 0));
        let (a, b) = (dir + 8 * (k >> 6), 1u64 << (k & 63));
        if a != at && m[0] != 0 {
            (dup, m) = (or(at, m) || dup, [0; 3]);
        }
        let (v, nul) = match wide {
            true => {
                (*(t.d.add(dir + 2 * stride + 2 * k) as *const AtomicU16)).store((w >> 32) as u16, Relaxed);
                (w >> 48 & 1, 0)
            }
            false => (w >> 32 & 1, w >> 33 & 1),
        };
        (dup, at, m) = (dup || m[0] & b != 0, a, [m[0] | b, m[1] | b * v, m[2] | b * nul]);
    }
    if m[0] != 0 && or(at, m) || dup {
        (*(t.d.add(FLAGS) as *const AtomicU8)).fetch_or(F_DUP, Relaxed);
    }
}

/// The serial lay-out: `DBITS` when the keys are unique (true, phase 2), else false and the caller expands the records.
/// Safety: `need(t.d) > 0` bytes are free past the records.
pub(crate) unsafe fn lay(t: &Rt) -> bool {
    prepare(t);
    put(t, t, 0..t.n);
    let ok = rd::<u8>(t.d, FLAGS) & F_DUP == 0;
    wr::<u8>(t.d, crate::ops::hashjoin::build::PHASE, if ok { 2 } else { 1 });
    ok
}

/// The compact records rewritten in place as full ones, the last first (image, link holding the NULL bit, payload word).
/// Safety: `d` is a compact build's region of at least `HDR + n x FULL` bytes.
pub(crate) unsafe fn expand(d: *mut u8) {
    let (n, full, pay, wide) = (rd::<u64>(d, N) as usize, rd::<u32>(d, FULL), rd::<u8>(d, PAY), vbits(rd::<u8>(d, VBITS)) == WIDE);
    for i in (0..n).rev() {
        let (w, r) = (rd::<u64>(d.add(HDR + 8 * i), 0), d.add(HDR + full as usize * i));
        let (v, nul) = if wide { ((w >> 32) as u16 as i16 as i64 as u64, w >> 48 & 1) } else { (w >> 32 & 1, w >> 33 & 1) };
        (wr::<i64>(r, 0, w as u32 as i32 as i64), wr::<u64>(r, 8, nul << 32));
        if pay == 1 {
            wr::<u64>(r, 16, v);
        }
    }
    (wr::<u32>(d, REC, full), wr::<u8>(d, FLAGS, rd::<u8>(d, FLAGS) & !F_COMPACT));
}
