//! bloomtag.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs), moved unchanged from it.
//! The parent re-exports these names, so its callers and the build region's raw.rs read them as before.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// The KeyHash directory over every record: 2^bits heads at <= 1/2 load (rt.rs `dir_bits`), each head its chain's first
/// record + 1 with the tags of its records above; each record inserted at its chain's head (LIFO). Under join.mult a
/// record whose key is already chained gives its count to that record instead (multiplicity.rs `collapse`).
///
/// # Safety
/// `t` is a build region with every record in.
pub(crate) unsafe fn lay(t: &Rt) -> gravity_driver::Result<(u8, usize)> {
    let bits = crate::ops::hashjoin::rt::dir_bits(t.n as u64);
    let dir = t.dir();
    t.room(dir, 8 << bits)?;
    let heads = t.d.add(dir) as *mut u64;
    core::ptr::write_bytes(heads, 0, 1 << bits);
    // D3194: the probe's Bloom words past the heads, each record's bits set below (rowfilter.rs)
    let rf = crate::ops::hashjoin::strategies::rowfilter::open(t, dir + (8 << bits));
    let (tags, mult) = (rd::<u8>(t.d, ALLOW) & A_TAGS != 0, rd::<u8>(t.d, ALLOW) & A_MULT != 0);
    let fifo = rd::<u8>(t.d, ALLOW) & crate::ops::hashjoin::build::A_FIFO != 0; // pushed last to first: chains in record order
    // join.dedup: a copy of a record its chain holds stays unchained (dedup.rs)
    let (dedup, mut dropped) = (rd::<u8>(t.d, crate::ops::hashjoin::build::FLAGS) & crate::ops::hashjoin::build::F_DEDUP != 0 && !t.compact && !mult, 0usize);
    for n in 0..t.n {
        let i = if fifo { t.n - 1 - n } else { n };
        let h = t.hash(i);
        if let Some(w) = &rf {
            w.set(h);
        }
        let s = heads.add(h.home(bits, Depth::TOP));
        if mult && crate::ops::hashjoin::strategies::multiplicity::collapse(t, *s & IDX, i).is_some() {
            continue;
        }
        if dedup && crate::ops::hashjoin::strategies::dedup::seen(t, *s & IDX, i) {
            dropped += 1;
            continue;
        }
        let l = t.link(i);
        *l = (*l & !LOW) | (*s & IDX);
        *s = (*s & !IDX) | (i as u64 + 1) | if tags { bit(h) } else { 0 };
    }
    if dropped > 0 {
        fires::fire_shared("exec.join.dedup"); // shapecheck fire signal: a lay-out left copies unchained
    }
    wr::<u64>(t.d, DIR, dir as u64);
    wr::<u32>(t.d, BITS, bits);
    Ok((HASH, dir + (8 << bits) + rf.map_or(0, |w| w.len())))
}

/// A partitioned merge's record `i` pushed on its chain in the heads its prepare zeroed (build_rt.rs): the jobs share the
/// heads, so the push is a compare-and-swap (a chain's order then varies by run; the probe's rows match as a multiset, in any order).
/// D3194: its bits ORed into the probe's Bloom words when the prepare opened them (`rf`, rowfilter.rs `shared`).
///
/// # Safety
/// `t` is the leader's region under a partitioned merge; record `i` is this job's.
#[inline]
pub(crate) unsafe fn put(t: &Rt, i: usize, rf: Option<&crate::ops::hashjoin::strategies::rowfilter::Words>) {
    let (h, bits, tags) = (t.hash(i), rd::<u32>(t.d, BITS), rd::<u8>(t.d, ALLOW) & A_TAGS != 0);
    if let Some(w) = rf {
        w.put(h);
    }
    let s = &*(t.d.add(rd::<u64>(t.d, DIR) as usize + 8 * h.home(bits, Depth::TOP)) as *const AtomicU64);
    let (l, tag) = (t.link(i), if tags { bit(h) } else { 0 });
    let mut old = s.load(Relaxed);
    loop {
        *l = (*l & !LOW) | (old & IDX);
        match s.compare_exchange_weak(old, (old & !IDX) | (i as u64 + 1) | tag, Relaxed, Relaxed) {
            Ok(_) => return,
            Err(cur) => old = cur,
        }
    }
}
