//! dense.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs), moved unchanged from it.
//! The parent re-exports these names, so its callers and the build region's raw.rs read them as before.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// The dense form of the records when their range allows it (`None`: the KeyHash form), and the directory's end. A
/// marking build (`t.marks`) lays DCHAIN only: DENSE copies each record into its key's slot, and a mark is indexed by
/// the record's address in the record run (AGA-22).
///
/// # Safety
/// `t` is a build region with every record in, its header's `LO` / `HI` over them.
pub(crate) unsafe fn lay(t: &Rt) -> gravity_driver::Result<Option<(u8, usize)>> {
    let (flags, allow, lo) = (rd::<u8>(t.d, FLAGS), rd::<u8>(t.d, ALLOW), rd::<i64>(t.d, LO));
    let Some(span) = span_of(t.d, t.n) else { return Ok(None) };
    let dir = t.dir();
    let srec = t.rec - 8 * t.words;
    let bm = bitmap(span);
    let slots = dir + bm;
    let (mut dup, mult) = (false, allow & A_MULT != 0);
    // join.dedup: a copy of a record its key's slot or chain holds is dropped (dedup.rs)
    let (dedup, mut dropped) = (flags & crate::ops::hashjoin::build::F_DEDUP != 0 && !t.compact && !mult, 0usize);
    if !t.marks && t.room(slots, span * srec).is_ok() {
        let bits = t.d.add(dir) as *mut u64;
        core::ptr::write_bytes(bits, 0, bm / 8);
        for i in 0..t.n {
            let k = (t.key(i) - lo) as usize;
            let (w, m) = (bits.add(k >> 6), 1u64 << (k & 63));
            if *w & m != 0 {
                if mult {
                    crate::ops::hashjoin::strategies::multiplicity::bump(t, t.d.add(slots + k * srec), i);
                    continue;
                }
                if dedup && crate::ops::hashjoin::strategies::dedup::same_slot(t, t.d.add(slots + k * srec), i) {
                    dropped += 1;
                    continue;
                }
                dup = true;
                break;
            }
            *w |= m;
            let s = t.d.add(slots + k * srec);
            core::ptr::copy_nonoverlapping(t.link(i) as *const u8, s, srec);
            wr::<u64>(s, 0, rd::<u64>(s, 0) & !LOW);
        }
        if !dup {
            if dropped > 0 {
                fires::fire_shared("exec.join.dedup"); // shapecheck fire signal: a lay-out left copies unchained
            }
            wr::<u64>(t.d, DIR, dir as u64);
            wr::<u64>(t.d, SLOTS, slots as u64);
            fires::fire_shared("join.dense"); // shapecheck fire signal (REGISTRY JOIN-03)
            return Ok(Some((DENSE, slots + span * srec)));
        }
    }
    if mult || t.room(dir, 4 * span).is_err() {
        return Ok(None);
    }
    let heads = t.d.add(dir) as *mut u32;
    core::ptr::write_bytes(heads, 0, span);
    let fifo = allow & crate::ops::hashjoin::build::A_FIFO != 0; // pushed last to first: chains in record order
    dropped = 0;
    for n in 0..t.n {
        let i = if fifo { t.n - 1 - n } else { n };
        let h = heads.add((t.key(i) - lo) as usize);
        if dedup && crate::ops::hashjoin::strategies::dedup::seen(t, *h as u64, i) {
            dropped += 1;
            continue;
        }
        let l = t.link(i);
        *l = (*l & !LOW) | *h as u64;
        *h = i as u32 + 1;
    }
    if dup {
        wr::<u8>(t.d, FLAGS, flags | F_DUP);
    }
    if dropped > 0 {
        fires::fire_shared("exec.join.dedup"); // shapecheck fire signal: a lay-out left copies unchained
    }
    wr::<u64>(t.d, DIR, dir as u64);
    fires::fire_shared("join.dense"); // shapecheck fire signal (REGISTRY JOIN-03)
    Ok(Some((DCHAIN, dir + 4 * span)))
}

/// The keys' span when `n` records may lay dense (the road on, one-word keys over at most the cost home's span for `n`
/// records, rt.rs `dense_span_max`), else `None`.
///
/// # Safety
/// `d` is a build region's header.
pub(crate) unsafe fn span_of(d: *const u8, n: usize) -> Option<usize> {
    let span = (rd::<i64>(d, HI) as i128 - rd::<i64>(d, LO) as i128 + 1).max(0) as u128;
    (rd::<u8>(d, ALLOW) & A_DENSE != 0 && rd::<u8>(d, FLAGS) & F_RANGE != 0 && n > 0 && span <= crate::ops::hashjoin::rt::dense_span_max(n as u64) as u128).then_some(span as usize)
}

/// A partitioned merge's record `i` into the `DENSE` slots its prepare zeroed (build_rt.rs): the jobs share the bitmap, so
/// a key's bit is taken atomically; a duplicate flags the region `F_DUP` and the finish lays it again (`DCHAIN`) from the
/// settled records, so once the flag is up no job puts again. Perf: a store per duplicate on the header's line (which
/// every put reads) made a W8 build of 200,000 rows over 1,000 keys 2x slower than the serial one; the flag is read
/// before anything is written (the key's bit is not: a load before the OR doubles the line's traffic for unique keys).
///
/// # Safety
/// `t` is the leader's region under a partitioned merge; record `i` is this job's, its images settled.
#[inline]
pub(crate) unsafe fn put(t: &Rt, i: usize) {
    let flags = &*(t.d.add(FLAGS) as *const AtomicU8);
    if flags.load(Relaxed) & F_DUP != 0 {
        return;
    }
    let (lo, dir, slots) = (rd::<i64>(t.d, LO), rd::<u64>(t.d, DIR) as usize, rd::<u64>(t.d, SLOTS) as usize);
    let (k, srec) = ((t.key(i) - lo) as usize, t.rec - 8 * t.words);
    let m = 1u64 << (k & 63);
    if (*(t.d.add(dir + 8 * (k >> 6)) as *const AtomicU64)).fetch_or(m, Relaxed) & m != 0 {
        flags.fetch_or(F_DUP, Relaxed);
        return;
    }
    let s = t.d.add(slots + k * srec);
    core::ptr::copy_nonoverlapping(t.link(i) as *const u8, s, srec);
    wr::<u64>(s, 0, rd::<u64>(s, 0) & !LOW);
}

/// A compact build (dense_bits.rs) laid out `DBITS` when its keys allow it, else its records expanded to the full layout
/// for the forms (phase 1 again). A partitioned `DBITS` is laid (a duplicate refused its phase 2: build_part.rs).
pub(crate) unsafe fn compact_out(st: &mut State<'_>) -> gravity_driver::Result<()> {
    let d = st.bytes.as_mut_ptr();
    if rd::<u8>(d, FLAGS) & F_COMPACT == 0 || rd::<u64>(d, N) == 0 || rd::<u8>(d, PHASE) == 2 {
        return Ok(());
    }
    if crate::ops::hashjoin::strategies::dense_bits::need(d) > 0 {
        grow_to(st, Rt::of(d, st.bytes.len()).dir() + crate::ops::hashjoin::strategies::dense_bits::need(d))?;
        if crate::ops::hashjoin::strategies::dense_bits::lay(&Rt::of(st.bytes.as_mut_ptr(), st.bytes.len())) {
            return Ok(());
        }
    }
    let d = st.bytes.as_mut_ptr();
    grow_to(st, HDR + rd::<u64>(d, N) as usize * rd::<u32>(d, FULL) as usize)?;
    crate::ops::hashjoin::strategies::dense_bits::expand(st.bytes.as_mut_ptr());
    Ok(wr::<u8>(st.bytes.as_mut_ptr(), PHASE, 1))
}
