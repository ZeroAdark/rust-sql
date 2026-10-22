//! The build region's raw runtime (rule R6: the family's unsafe code lives in its raw.rs, behind safe wrappers), moved as
//! it was from rt.rs, build_part.rs and build.rs: `Rt` over a region and its byte accessors, the
//! serial merge and lay-out (`merge_into`, `finish`: `BuildMerge`'s bodies), the partitioned merge's byte work
//! (`prepare_parts`, `merge_part`: build_part.rs's), the partials' headers its plan reads (`heads`), and D276's reading of
//! a finished build (`build_rows`). The rest of ops/hashjoin calls the safe functions here; rt.rs re-exports the names
//! the strategies and the spill still read until their own raw.rs moves.
#![allow(unsafe_code)]
use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

use gravity_abi::hash::KeyHash;
use gravity_abi::limits::JOIN_KEY_IMAGE_WORDS;
use gravity_abi::status::UNSUPPORTED_SHAPE;
use gravity_driver::{Error, Partial, PartitionPlan, Region, State};
use gravity_kernels::table::image::AlignedImage;

use crate::ops::hashjoin::layout;
use crate::ops::hashjoin::build::{ALLOW, A_DENSE, A_KEEP, A_MARKS, BITS, DENSE, DIR, D_CSTR, D_VARLENA, EMPTY, END, FAILED, FLAGS, FORM, F_COMPACT, F_DUP, F_FP128, F_RANGE, F_RANGE0, F_SPLIT, GUARD, GUARD_BUCKETS, HASH, HDR, HI, KEPT, KIND, LIMIT, LO, LOW, MARKS, N, NULLKEY, PHASE, REC, RF_BITS, SLOTS, TOP, WORDS};
use crate::ops::hashjoin::strategies::rowfilter;
use crate::ops::hashjoin::rt::{dir_bits, past_share};
use crate::ops::hashjoin::strategies::dense_bits as bits;

/// The runtime view of a region (records laid by the build end).
pub(crate) struct Rt {
    pub d: *mut u8,
    pub len: usize,
    pub n: usize,
    pub rec: usize,
    pub words: usize,
    /// join.dense's bits form: 8 B records holding the key's low 32 bits (dense_bits.rs)
    pub compact: bool,
    /// The NULL-key records laid past the `n` keyed ones (0 before the lay-out's split); a mark byte per record.
    pub kept: usize,
    pub marks: bool,
    /// The first record's offset and the payload columns (layout.rs: past 32 columns the descriptor runs past the header).
    pub base: usize,
    pub pay: usize,
}

pub(crate) unsafe fn rd<T: Copy>(d: *const u8, off: usize) -> T {
    (d.add(off) as *const T).read_unaligned()
}

pub(crate) unsafe fn wr<T>(d: *mut u8, off: usize, v: T) {
    (d.add(off) as *mut T).write_unaligned(v)
}

impl Rt {
    pub unsafe fn of(d: *mut u8, len: usize) -> Rt {
        let (n, rec, words, compact) = (rd::<u64>(d, N) as usize, rd::<u32>(d, REC) as usize, rd::<u8>(d, WORDS) as usize, rd::<u8>(d, FLAGS) & F_COMPACT != 0);
        let pay = layout::pay_of(d);
        Rt { d, len, n, rec, words, compact, kept: kept(d), marks: rd::<u8>(d, ALLOW) & A_MARKS != 0, base: layout::base(pay), pay }
    }

    pub unsafe fn rec(&self, i: usize) -> *mut u8 {
        self.d.add(self.base + i * self.rec)
    }

    /// Payload column `c`'s NULL bit in a record: (byte offset, bit) (layout.rs `null_at`).
    pub fn null_at(&self, c: usize) -> (usize, u32) {
        layout::null_at(self.words, self.pay, c)
    }

    pub unsafe fn key(&self, i: usize) -> i64 {
        if self.compact { rd::<u32>(self.rec(i), 0) as i32 as i64 } else { rd::<i64>(self.rec(i), 0) }
    }

    pub unsafe fn hash(&self, i: usize) -> KeyHash {
        let mut w = AlignedImage([0u64; JOIN_KEY_IMAGE_WORDS]);
        for (j, x) in w.0.iter_mut().enumerate().take(self.words) {
            *x = rd::<u64>(self.rec(i), 8 * j);
        }
        KeyHash::of(&w.0[..self.words])
    }

    pub unsafe fn link(&self, i: usize) -> *mut u64 {
        self.rec(i).add(8 * self.words) as *mut u64
    }

    /// The directory's first byte: past the records (and their marks), a cache line up.
    pub fn dir(&self) -> usize {
        match self.marks {
            true => (self.marks_at() + self.n + self.kept).next_multiple_of(64),
            false => self.marks_at(),
        }
    }

    /// The marks array's first byte: one byte per record, keyed ones then kept, past the records.
    pub fn marks_at(&self) -> usize {
        (self.base + (self.n + self.kept) * self.rec).next_multiple_of(64)
    }

    /// `[at, at + bytes)` below the images, else the named wall.
    pub unsafe fn room(&self, at: usize, bytes: usize) -> gravity_driver::Result<()> {
        if at.checked_add(bytes).is_some_and(|e| e <= (rd::<u64>(self.d, TOP) as usize).min(self.len)) { Ok(()) } else { Err(past_share()) }
    }
}

/// `BuildMerge::merge_into` (rt.rs): worker region `partial`'s records appended to the leader's, past its grant share
/// grown (D254); a build that may spill merges within its grant and spills past it.
pub(crate) fn merge_into(leader: &mut State<'_>, partial: Partial<'_>) -> gravity_driver::Result<()> {
    // SAFETY: both are this build's regions (kind 7) of their slices' lengths; the leader is ours alone here.
    unsafe {
        let s = partial.bytes.as_ptr() as *mut u8;
        // a build that may spill merges within its grant and spills past it
        if partial.bytes.len() >= HDR && crate::ops::hashjoin::spill::build::spilling(s) {
            return crate::ops::hashjoin::spill::build::merge_into(leader, partial.bytes);
        }
        if live(core::slice::from_ref(&partial)).next().is_none() {
            return Ok(());
        }
        headed(leader)?;
        let d = leader.bytes.as_mut_ptr();
        let (nd, td) = if rd::<u8>(d, PHASE) == 0 { (0, 0) } else { (rd::<u64>(d, N) as usize, tail(d)) };
        let rec = rd::<u32>(s, REC) as usize;
        grow_to(leader, layout::base(layout::pay_of(s)) + (nd + rd::<u64>(s, N) as usize) * rec + td + tail(s))?;
        append(leader.bytes, partial.bytes)
    }
}

/// exactsize B1b (`BuildMerge::prepare_serial`): before the serial merge's first partial, the leader grown once to every live
/// partial's records and images (their headers count them) and the KeyHash directory over their count, so `merge_into`
/// appends without a reopen and `finish` lays out in place, as `grow_to_exact` grows: under `pgrust.gravity_exact_size` =
/// build | all only. A build that may spill (`spilling`: its partition bits set) grows within G only (`grow_leader`): G
/// refusing, its merges make their own room as before (spill/raw/region.rs `merge_into`). A side that already spilled, or a
/// compact one (expanded at its finish), keeps the per-partial grows, as does every build at none.
pub(crate) fn presize(leader: &mut State<'_>, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
    if !leader.exact_build() {
        return Ok(());
    }
    // SAFETY: the partials are this build's regions (kind 7), read only; the leader is ours alone here.
    unsafe {
        use crate::ops::hashjoin::spill::build::{grow_leader, spilling, SPILLED};
        let Some(s0) = live(partials).next() else { return Ok(()) };
        // `spilling` is "may spill" (any spillable build); a side that spilled has partitions out (SPILLED)
        let own = |s: *const u8| (spilling(s) && rd::<u64>(s, SPILLED) != 0) || rd::<u8>(s, FLAGS) & F_COMPACT != 0;
        if live(partials).any(|s| own(s)) {
            return Ok(());
        }
        headed(leader)?;
        let d = leader.bytes.as_mut_ptr();
        let fresh = rd::<u8>(d, PHASE) == 0;
        if !fresh && own(d) {
            return Ok(());
        }
        let within = live(partials).any(|s| spilling(s)) || (!fresh && spilling(d));
        let (nd, td) = if fresh { (0, 0) } else { (rd::<u64>(d, N) as usize, tail(d)) };
        let n = nd + live(partials).map(|s| rd::<u64>(s, N) as usize).sum::<usize>();
        // as `Rt::dir` lays them: the records, their marks (a marking build), a cache line up, then the KeyHash heads
        let at = (layout::base(layout::pay_of(s0)) + n * rd::<u32>(s0, REC) as usize).next_multiple_of(64);
        let dir = if rd::<u8>(s0, ALLOW) & A_MARKS != 0 { (at + n).next_multiple_of(64) } else { at };
        let images = td + live(partials).map(|s| tail(s)).sum::<usize>();
        let bytes = dir + (8usize << dir_bits(n as u64)) + images;
        if within {
            // a build that may spill: within G, else its merges make their own room (refused: nothing drawn)
            if !grow_leader(leader, bytes, true, Some(n as u64))? {
                return Ok(());
            }
        } else {
            grow_to_exact(leader, bytes, n as u64)?;
        }
        fires::fire_shared("exact.serial_presize"); // shapecheck fire signal (exactsize B1b)
        Ok(())
    }
}

/// `BuildMerge::finish` (rt.rs): the leader grown for its directory, then laid out.
pub(crate) fn finish(leader: &mut State<'_>) -> gravity_driver::Result<()> {
    // SAFETY: the leader region, every record in; grown first so that the directory fits below the images.
    unsafe {
        headed(leader)?;
        // USEROP's one-batch guard (ops/shared/one_batch.rs) over every record, before any form lays them out (the
        // dense bits form lays out here, in compact_out)
        let d = leader.bytes.as_mut_ptr();
        if rd::<u8>(d, PHASE) != 0 {
            one_batch(d, rd::<u64>(d, N) + kept(d) as u64, leader.hash_ceiling())?;
        }
        crate::ops::hashjoin::strategies::dense::compact_out(leader)?;
        let d = leader.bytes.as_mut_ptr();
        match (rd::<u8>(d, PHASE), rd::<u8>(d, FORM), rd::<u8>(d, FLAGS) & F_DUP) {
            (1, _, _) if crate::ops::hashjoin::spill::build::spilling(d) => {
                dense_within(leader)?;
                crate::ops::hashjoin::spill::build::finish_room(leader, &|d, len| {
                    let t = Rt::of(d, len);
                    t.dir() + dir_need(&t) + tail(d)
                })?
            }
            (1, _, _) => {
                // exactsize B1a: the records are counted, the directory laid next: grown to that need, not past it
                let t = Rt::of(d, leader.bytes.len());
                grow_to_exact(leader, t.dir() + dir_need(&t) + rf_need(&t) + tail(d), (t.n + t.kept) as u64)?;
            }
            // a partitioned merge's DENSE slots met a duplicate key: the forms again over the settled records
            (2, DENSE, F_DUP) => wr::<u8>(d, FORM, forms(&Rt::of(d, leader.bytes.len()))?),
            _ => {}
        }
        lay_out(leader.bytes.as_mut_ptr(), leader.bytes.len())
    }
}

/// The images' bytes, kept at the region's end: `[TOP, END)`.
pub(crate) unsafe fn tail(d: *const u8) -> usize {
    (rd::<u64>(d, END) - rd::<u64>(d, TOP)) as usize
}

/// D254: the leader grown to hold `bytes` (a no-op when it does), its images moved to the new end (`TOP`, `END` and the
/// limit follow). Only a real allocation failure errors; past the grant share the region grows.
pub(crate) unsafe fn grow_to(st: &mut State<'_>, bytes: usize) -> gravity_driver::Result<()> {
    grow_to_at(st, bytes, None)
}

/// exactsize B1a: `grow_to` where `bytes` is the build's counted need (`counted` records, their directory laid next): under
/// `pgrust.gravity_exact_size` = build | all the region grows to `bytes`, not to twice its length (`State::grow_exact`).
pub(crate) unsafe fn grow_to_exact(st: &mut State<'_>, bytes: usize, counted: u64) -> gravity_driver::Result<()> {
    grow_to_at(st, bytes, Some(counted))
}

unsafe fn grow_to_at(st: &mut State<'_>, bytes: usize, counted: Option<u64>) -> gravity_driver::Result<()> {
    let d = st.bytes.as_mut_ptr();
    let fresh = rd::<u8>(d, PHASE) == 0;
    let (end, t) = if fresh { (st.bytes.len(), 0) } else { (rd::<u64>(d, END) as usize, tail(d)) };
    if bytes <= end {
        return Ok(());
    }
    // C10: the leader's merge grows past the build's grant (a named SPILL-DEBT road)
    st.grow_debt(bytes, counted, gravity_program::spill::NoSpill::BuildLeaderMerge)?;
    let (d, len) = (st.bytes.as_mut_ptr(), st.bytes.len());
    if !fresh {
        core::ptr::copy(d.add(end - t), d.add(len - t), t);
        (wr::<u64>(d, TOP, (len - t) as u64), wr::<u64>(d, END, len as u64));
    }
    wr::<u32>(d, LIMIT, len.min(u32::MAX as usize) as u32);
    Ok(())
}

/// JOIN-03's run-time re-check: a region that may spill makes room for its directory within the statement's grant, else by
/// spilling more of it (spill/raw/region.rs `finish_room`). The dense form is a speed choice, never worth a spill the
/// KeyHash form would not make: when the dense directory does not fit the grant, the region gives up the dense form
/// (`A_DENSE` cleared) and lays KeyHash, spilling only if that does not fit either.
///
/// # Safety
/// `leader` is a build region in phase 1 with every record in.
unsafe fn dense_within(leader: &mut State<'_>) -> gravity_driver::Result<()> {
    let d = leader.bytes.as_mut_ptr();
    let t = Rt::of(d, leader.bytes.len());
    if crate::ops::hashjoin::strategies::dense::span_of(d, t.n).is_some() && !crate::ops::hashjoin::spill::build::grow_leader(leader, t.dir() + dir_need(&t) + tail(d), true, Some((t.n + t.kept) as u64))? {
        let d = leader.bytes.as_mut_ptr();
        wr::<u8>(d, ALLOW, rd::<u8>(d, ALLOW) & !A_DENSE);
    }
    Ok(())
}

/// The directory's bytes for the form the lay-out will choose (dense slots or the KeyHash heads).
pub(crate) unsafe fn dir_need(t: &Rt) -> usize {
    let (flags, allow) = (rd::<u8>(t.d, FLAGS), rd::<u8>(t.d, ALLOW));
    if t.n == 0 {
        // a build whose every record spilled lays an empty directory (`forms`)
        if rd::<u64>(t.d, crate::ops::hashjoin::spill::build::SPILLED) != 0 { 8 << dir_bits(0) } else { 0 }
    } else if let Some(span) = crate::ops::hashjoin::strategies::dense::span_of(t.d, t.n) {
        match t.marks {
            true => 4 * span, // DCHAIN's heads: a marking build lays no DENSE slots (dense raw.rs `lay`)
            false => crate::ops::hashjoin::strategies::dense::bitmap(span) + span * (t.rec - 8 * t.words),
        }
    } else {
        // the lay-out's split leaves a keeping build's keyed records alone at N: few enough, they sit at the sparse
        // load and may want more heads than all N records (dir_bits is not monotonic there), so both counts are sized
        let keyed = match allow & A_KEEP != 0 && flags & F_SPLIT == 0 {
            true => (0..t.n).filter(|&i| *t.link(i) & LOW != NULLKEY).count(),
            false => t.n,
        };
        8 << dir_bits(t.n as u64).max(dir_bits(keyed as u64))
    }
}

/// D3194: the bytes the KeyHash form's Bloom words take past its heads (rowfilter.rs `need`), when the lay-out will choose
/// that form (`dir_need`'s last arm); 0 for the other forms. A spilling build's room is `dir_need`'s alone: its words
/// are laid only where the room is left (`rowfilter::open`).
pub(crate) unsafe fn rf_need(t: &Rt) -> usize {
    let keyhash = t.n > 0 && crate::ops::hashjoin::strategies::dense::span_of(t.d, t.n).is_none();
    if keyhash { rowfilter::need(t) } else { 0 }
}

/// A fresh leader of `len` bytes headed as worker region `s`, with no records and no images.
pub(crate) unsafe fn head(d: *mut u8, s: *const u8, len: usize) {
    core::ptr::copy_nonoverlapping(s, d, layout::base(layout::pay_of(s)));
    (wr::<u64>(d, N, 0), wr::<u64>(d, TOP, len as u64), wr::<u64>(d, END, len as u64), wr::<i64>(d, LO, i64::MAX), wr::<i64>(d, HI, i64::MIN));
    wr::<u32>(d, LIMIT, len.min(u32::MAX as usize) as u32);
}

/// The partials holding records (a worker whose pipeline never opened has no region at all).
pub(crate) fn live<'a>(partials: &'a [Partial<'a>]) -> impl Iterator<Item = *mut u8> + 'a {
    // SAFETY: a partial of at least the header's bytes is this build's region.
    partials.iter().filter(|p| p.bytes.len() >= HDR).map(|p| p.bytes.as_ptr() as *mut u8).filter(|&s| unsafe { rd::<u8>(s, PHASE) != 0 && rd::<u64>(s, N) != 0 })
}

/// A leader slot whose pipeline never opened (it scanned nothing) has no region: it grows to a zeroed header, so the merge
/// and the lay-out find it fresh (an EMPTY build the probe can read).
pub(crate) unsafe fn headed(st: &mut State<'_>) -> gravity_driver::Result<()> {
    if st.bytes.len() < HDR {
        st.grow(HDR)?;
        // a grow that left the view short (a released slot's stale size, D276) is an error, never a write through it
        if st.bytes.len() < HDR {
            return Err(Error::host("join build: the leader slot's grow left no header room"));
        }
        core::ptr::write_bytes(st.bytes.as_mut_ptr(), 0, HDR);
        wr::<u32>(st.bytes.as_mut_ptr(), LIMIT, st.bytes.len().min(u32::MAX as usize) as u32);
    }
    Ok(())
}

/// A worker's records appended to the leader's: its images copied below the leader's, their offsets moved.
pub(crate) unsafe fn append(dst: &mut [u8], src: &[u8]) -> gravity_driver::Result<()> {
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr() as *mut u8);
    if rd::<u8>(s, PHASE) == 0 || rd::<u64>(s, N) == 0 {
        return Ok(());
    }
    if rd::<u8>(d, PHASE) == 0 {
        head(d, s, dst.len());
    }
    let (t, ns) = (Rt::of(d, dst.len()), rd::<u64>(s, N) as usize);
    let end = t.base + (t.n + ns) * t.rec;
    t.room(t.base, end - t.base)?;
    core::ptr::copy_nonoverlapping(s.add(t.base), t.rec(t.n), ns * t.rec);
    for c in 0..t.pay {
        let kind = rd::<u16>(d, layout::desc_at(c));
        if kind == 0 {
            continue;
        }
        for i in t.n..t.n + ns {
            let (o, bit) = t.null_at(c);
            if rd::<u64>(t.rec(i), o) >> bit & 1 != 0 {
                continue;
            }
            let at = t.rec(i).add(8 * (t.words + 1 + c));
            let from = s.add(rd::<u64>(s, END) as usize - rd::<u64>(at, 0) as usize) as *const u8;
            let len = match kind {
                D_VARLENA => varlena_len(from),
                D_CSTR => core::ffi::CStr::from_ptr(from as *const core::ffi::c_char).to_bytes_with_nul().len(),
                l => l as usize,
            };
            let top = (rd::<u64>(d, TOP) as usize).checked_sub(len.next_multiple_of(8)).filter(|&v| v >= end).ok_or_else(past_share)?;
            core::ptr::copy_nonoverlapping(from, d.add(top), len);
            wr::<u64>(d, TOP, top as u64);
            wr::<u64>(at, 0, (rd::<u64>(d, END) as usize - top) as u64);
        }
    }
    wr::<u64>(d, N, (t.n + ns) as u64);
    wr::<i64>(d, LO, rd::<i64>(d, LO).min(rd::<i64>(s, LO)));
    wr::<i64>(d, HI, rd::<i64>(d, HI).max(rd::<i64>(s, HI)));
    Ok(())
}

/// VARSIZE_ANY: a 1-byte header's length, else the 4-byte header's (an external TOAST pointer is its 18-byte form).
pub(crate) unsafe fn varlena_len(p: *const u8) -> usize {
    match *p {
        0x01 => 2 + if *p.add(1) == 18 { 16 } else { 8 },
        h if h & 1 == 1 => (h >> 1) as usize,
        _ => (rd::<u32>(p, 0) >> 2) as usize,
    }
}

/// The directory over every record (serial; the leader's region at the width-1 finish or after the merges).
pub(crate) unsafe fn lay_out(d: *mut u8, len: usize) -> gravity_driver::Result<()> {
    match rd::<u8>(d, PHASE) {
        2 => return Ok(()),
        0 => {
            wr::<u64>(d, N, 0);
            wr::<u8>(d, 15, KIND);
            // the driver zeroes only the first 64 header bytes: a region no row opened never spilled
            (wr::<u8>(d, crate::ops::hashjoin::spill::build::SPILL, 0), wr::<u8>(d, crate::ops::hashjoin::spill::build::PBITS, 0), wr::<u64>(d, crate::ops::hashjoin::spill::build::SPILLED, 0));
        }
        _ => {}
    }
    if rd::<u8>(d, ALLOW) & A_KEEP != 0 && rd::<u8>(d, FLAGS) & F_SPLIT == 0 {
        split(&Rt::of(d, len));
    }
    let t = Rt::of(d, len);
    if t.n + t.kept > (u32::MAX - 1) as usize {
        return Err(past_share());
    }
    settle(&t, 0..t.n + t.kept, rd::<u64>(d, END) as usize);
    if t.marks {
        place_marks(&t)?;
    }
    // D3194: no Bloom words until a HASH lay-out opens them (a worker's header carries garbage past the zeroed 64 bytes)
    wr::<u8>(d, RF_BITS, 0);
    wr::<u8>(d, FORM, forms(&t)?);
    wr::<u8>(d, PHASE, 2);
    Ok(())
}

/// USEROP's one-batch guard (build.rs `GUARD`) over the region's `n` records at the statement's hash_mem `limit`
/// (ops/shared/one_batch.rs `check`).
unsafe fn one_batch(d: *const u8, n: u64, limit: u64) -> gravity_driver::Result<()> {
    crate::ops::shared::one_batch::check((rd::<u16>(d, GUARD), rd::<u8>(d, GUARD_BUCKETS)), n, limit)
}

/// The form over settled records: dense, else the KeyHash directory (an empty one over kept NULL-key
/// records alone: PG counts them, so the build is not empty; or over none when every record spilled).
unsafe fn forms(t: &Rt) -> gravity_driver::Result<u8> {
    Ok(if t.n == 0 && t.kept == 0 && rd::<u64>(t.d, crate::ops::hashjoin::spill::build::SPILLED) == 0 {
        EMPTY
    } else if t.n == 0 {
        crate::ops::hashjoin::strategies::bloomtag::lay(t)?.0
    } else if let Some((f, _)) = crate::ops::hashjoin::strategies::dense::lay(t)? {
        f
    } else {
        crate::ops::hashjoin::strategies::bloomtag::lay(t)?.0
    })
}

/// Records `range`'s copied images' references from their distance to `end` (where their images end) to their offset
/// from the region's base (the region no longer moves; the probe adds its base).
pub(crate) unsafe fn settle(t: &Rt, range: core::ops::Range<usize>, end: usize) {
    for c in (0..t.pay).filter(|&c| rd::<u16>(t.d, layout::desc_at(c)) != 0) {
        for i in range.clone() {
            let at = t.rec(i).add(8 * (t.words + 1 + c));
            wr::<u64>(at, 0, (end as u64).wrapping_sub(rd::<u64>(at, 0)));
        }
    }
}

/// The kept NULL-key records past the keyed ones (`Rt::kept`); 0 until the lay-out's split.
pub(crate) unsafe fn kept(d: *const u8) -> usize {
    if rd::<u8>(d, FLAGS) & F_SPLIT != 0 { rd::<u64>(d, KEPT) as usize } else { 0 }
}

/// The kept NULL-key records (link low half `NULLKEY`) moved past the keyed ones in any order (a record carries no
/// position: its images are kept by distance, its link is the lay-out's), their links' low halves cleared; `N` the keyed
/// count, `KEPT` theirs, `F_SPLIT` set.
unsafe fn split(t: &Rt) {
    let (mut i, mut j) = (0, t.n);
    while i < j {
        if *t.link(i) & LOW != NULLKEY {
            i += 1;
            continue;
        }
        j -= 1;
        if i != j {
            core::ptr::swap_nonoverlapping(t.rec(i), t.rec(j), t.rec);
        }
    }
    (i..t.n).for_each(|k| *t.link(k) &= !LOW);
    (wr::<u64>(t.d, N, i as u64), wr::<u64>(t.d, KEPT, (t.n - i) as u64), wr::<u8>(t.d, FLAGS, rd::<u8>(t.d, FLAGS) | F_SPLIT));
}

/// The marks array placed past the records, its offset in `MARKS` (the directory follows it, `Rt::dir`); its bytes
/// start clear at every lay-out (the lifetime home's `built`).
pub(crate) unsafe fn place_marks(t: &Rt) -> gravity_driver::Result<()> {
    t.room(t.marks_at(), t.n + t.kept)?;
    crate::lifetime::hashjoin::built(core::slice::from_raw_parts_mut(t.d.add(t.marks_at()), t.n + t.kept));
    Ok(wr::<u64>(t.d, MARKS, t.marks_at() as u64))
}

/// A live partial's header words, the partitioned merge's plan reads them (build_part.rs `plan`).
#[derive(Clone, Copy)]
pub(crate) struct Head {
    pub n: u64,
    pub allow: u8,
    pub flags: u8,
    pub lo: i64,
    pub hi: i64,
    pub kept: u64,
    pub rec: u32,
}

/// The live partials' headers (`live`), read only.
pub(crate) fn heads<'a>(partials: &'a [Partial<'a>]) -> impl Iterator<Item = Head> + 'a {
    // SAFETY: a live partial is this build's region, at least a header long, read only.
    live(partials).map(|s| unsafe { Head { n: rd(s, N), allow: rd(s, ALLOW), flags: rd(s, FLAGS), lo: rd(s, LO), hi: rd(s, HI), kept: rd(s, KEPT), rec: rd(s, REC) } })
}

/// build_part.rs `plan_within`: whether every live partial may spill (`spilling`: the record form, partition bits set) and
/// the leader table over all of them would pass `ceil` bytes: their records, a KeyHash directory over them and every image
/// (`prepare_parts`' need at its largest form).
pub(crate) fn past_ceiling(partials: &[Partial<'_>], ceil: u64) -> bool {
    // SAFETY: a live partial is this build's region, at least a header long, read only.
    unsafe {
        use crate::ops::hashjoin::spill::build::spilling;
        let Some(s0) = live(partials).next() else { return false };
        if !live(partials).all(|s| spilling(s)) {
            return false;
        }
        let n = live(partials).map(|s| rd::<u64>(s, N) as usize).sum::<usize>();
        let at = (layout::base(layout::pay_of(s0)) + n * rd::<u32>(s0, REC) as usize).next_multiple_of(64);
        let images = live(partials).map(|s| tail(s)).sum::<usize>();
        (at + (8usize << dir_bits(n as u64)) + images) as u64 > ceil
    }
}

/// build_part.rs `prepare`'s byte work: the leader headed, grown, its directory zeroed and laid out (`PHASE` 2).
pub(crate) fn prepare_parts(leader: &mut State<'_>, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
    let Some(s0) = live(partials).next() else { return Ok(()) };
    // SAFETY: the leader is ours alone here; the partials are this build's regions, read only.
    unsafe {
        headed(leader)?;
        let d = leader.bytes.as_mut_ptr();
        if rd::<u8>(d, PHASE) == 0 {
            head(d, s0, leader.bytes.len());
        }
        let (nd, td, ts) = (rd::<u64>(d, N), tail(d), live(partials).map(|s| tail(s)).sum::<usize>());
        let ranged = |f: u8| (nd == 0 || rd::<u8>(d, FLAGS) & f != 0) && live(partials).all(|s| rd::<u8>(s, FLAGS) & f != 0);
        // PS1-22: a text fingerprint build only when every partial holds the bit (else join.rtfilter exports nothing)
        let (range, range0, fp) = (ranged(F_RANGE), ranged(F_RANGE0), ranged(F_FP128));
        wr::<u64>(d, N, nd + live(partials).map(|s| rd::<u64>(s, N)).sum::<u64>());
        // USEROP's one-batch guard (ops/shared/one_batch.rs) over every record, before the jobs lay them out
        one_batch(d, rd::<u64>(d, N), leader.hash_ceiling())?;
        wr::<i64>(d, LO, live(partials).fold(rd::<i64>(d, LO), |m, s| m.min(rd::<i64>(s, LO))));
        wr::<i64>(d, HI, live(partials).fold(rd::<i64>(d, HI), |m, s| m.max(rd::<i64>(s, HI))));
        wr::<u8>(d, FLAGS, (rd::<u8>(d, FLAGS) & !(F_RANGE | F_RANGE0 | F_FP128)) | if range { F_RANGE } else { 0 } | if range0 { F_RANGE0 } else { 0 } | if fp { F_FP128 } else { 0 });
        let t = Rt::of(d, leader.bytes.len());
        if t.compact {
            // join.dense's bits form (partition_plan admitted its span): the bitmaps, the jobs `put` into them
            grow_to_exact(leader, t.dir() + bits::need(d), t.n as u64)?;
            bits::prepare(&Rt::of(leader.bytes.as_mut_ptr(), leader.bytes.len()));
            wr::<u8>(leader.bytes.as_mut_ptr(), RF_BITS, 0);
            return Ok(wr::<u8>(leader.bytes.as_mut_ptr(), PHASE, 2));
        }
        // the jobs chain into DENSE slots or KeyHash heads; a marking build takes no DENSE slots (dense raw.rs `lay`)
        let (at, span) = (t.dir(), crate::ops::hashjoin::strategies::dense::span_of(d, t.n).filter(|_| !t.marks));
        let bits = dir_bits(t.n as u64);
        let need = span.map_or(8 << bits, |sp| crate::ops::hashjoin::strategies::dense::bitmap(sp) + sp * (t.rec - 8 * t.words));
        // exactsize B1a: every partial's records counted (N), the shared directory sized from them: grown to that need
        // D3194: the KeyHash heads' Bloom words past them (rowfilter.rs), opened below where the room is
        let rf = if span.is_none() { rowfilter::need(&t) } else { 0 };
        // D3918: a build that may spill (its partials' partition bits set) sizes the leader's table within the statement's
        // grant, as its serial merge does (`grow_leader`, the refusable road: `State::grow_in_exact`). Refused, phase 2 is
        // off (`Unsupported`: the runner re-arms the leader) and the serial fold merges each partial, widening the
        // leader's spilled partitions where it finds no room (spill/raw/region.rs `merge_into`). It grew by name here,
        // past any grant, whatever the pool held. A build that never spills keeps D254's named growth.
        if live(partials).any(|s| crate::ops::hashjoin::spill::build::spilling(s)) {
            if !crate::ops::hashjoin::spill::build::grow_leader(leader, at + need + rf + td + ts, true, Some(t.n as u64))? {
                return Err(Error::Unsupported(UNSUPPORTED_SHAPE));
            }
        } else {
            grow_to_exact(leader, at + need + rf + td + ts, t.n as u64)?;
        }
        let (d, len) = (leader.bytes.as_mut_ptr(), leader.bytes.len());
        wr::<u64>(d, TOP, rd::<u64>(d, END) - (td + ts) as u64);
        Rt::of(d, len).room(at, need)?;
        core::ptr::write_bytes(d.add(at), 0, span.map_or(need, crate::ops::hashjoin::strategies::dense::bitmap));
        wr::<u64>(d, DIR, at as u64);
        let t = Rt::of(d, len);
        if t.marks {
            place_marks(&t)?; // a marking probe's build (no kept records: `plan`)
        }
        wr::<u8>(d, RF_BITS, 0);
        match span {
            Some(sp) => {
                fires::fire_shared("join.dense"); // shapecheck fire signal (REGISTRY JOIN-03)
                (wr::<u64>(d, SLOTS, (at + crate::ops::hashjoin::strategies::dense::bitmap(sp)) as u64), wr::<u8>(d, FORM, DENSE))
            }
            None => {
                rowfilter::open(&t, at + need);
                (wr::<u32>(d, BITS, bits), wr::<u8>(d, FORM, HASH))
            }
        };
        wr::<u8>(d, PHASE, 2);
        Ok(())
    }
}

/// build_part.rs `merge`'s byte work: partition `part`'s records copied, settled and put into the shared directory.
pub(crate) fn merge_part(leader: Region, part: u32, plan: PartitionPlan, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
    // SAFETY: the prepare sized and headed the leader; this job alone writes its partition's records and images, and the
    // directory only atomically.
    unsafe {
        let (d, t) = (leader.ptr, Rt::of(leader.ptr, leader.len));
        let (p, end) = (1usize << plan.pbits, rd::<u64>(d, END) as usize);
        let (lo, hi) = (t.n * part as usize / p, t.n * (part as usize + 1) / p);
        let mut base = t.n - live(partials).map(|s| rd::<u64>(s, N) as usize).sum::<usize>();
        let mut off = end - rd::<u64>(d, TOP) as usize - live(partials).map(|s| tail(s)).sum::<usize>();
        if t.compact {
            // the bits straight from each region's records, none copied into the leader (a compact build has no
            // images); a duplicate key refuses phase 2, and the serial fold (the driver's `reset_leader`) copies and expands
            bits::put(&t, &t, lo..hi.min(base));
            for s in live(partials) {
                let ns = rd::<u64>(s, N) as usize;
                if lo.max(base) < hi.min(base + ns) {
                    // a worker's view for its records only (never `room`)
                    bits::put(&t, &Rt::of(s, 0), lo.max(base) - base..hi.min(base + ns) - base);
                }
                base += ns;
            }
            let dup = (*(d.add(FLAGS) as *const AtomicU8)).load(Relaxed) & F_DUP != 0;
            return if dup { Err(Error::Unsupported(UNSUPPORTED_SHAPE)) } else { Ok(()) };
        }
        settle(&t, lo..hi.min(base), end);
        for s in live(partials) {
            let (ns, ts) = (rd::<u64>(s, N) as usize, tail(s));
            let (a, b) = (lo.max(base), hi.min(base + ns));
            if a < b {
                core::ptr::copy_nonoverlapping(s.add(t.base + (a - base) * t.rec), t.rec(a), (b - a) * t.rec);
                settle(&t, a..b, end - off);
            }
            if (lo..hi).contains(&base) {
                core::ptr::copy_nonoverlapping(s.add(rd::<u64>(s, TOP) as usize), d.add(end - off - ts), ts);
            }
            (base, off) = (base + ns, off + ts);
        }
        match rd::<u8>(d, FORM) {
            DENSE => (lo..hi).for_each(|i| crate::ops::hashjoin::strategies::dense::put(&t, i)),
            _ => {
                let rf = rowfilter::shared(&t);
                (lo..hi).for_each(|i| crate::ops::hashjoin::strategies::bloomtag::put(&t, i, rf.as_ref()))
            }
        }
    }
    Ok(())
}

/// D276's reading of a finished build (a `RegionFn`, HOSTS' BuildSideLive): one item `(0, N, FORM)`, or `None` for a region
/// that is not a laid-out build. A zeroed header (phase 0, kind 0, N 0) is a build no row opened: the runner's unclaimed
/// open maps it zeroed after the merge's finish, so it reads as the empty build the probe finds there (FORM 0, EMPTY).
///
/// # Safety
/// `region` holds at least 64 readable bytes (the reader checks).
pub(crate) unsafe fn build_rows(region: *const u8, _words: &[u64], _owner: Option<gravity_fn::arena::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    match (rd::<u8>(region, 15), rd::<u8>(region, PHASE), rd::<u64>(region, N)) {
        // kept NULL-key records count (PG's totalTuples): a build of only those is not empty. The value is
        // read for emptiness only (BuildSideLive: n == 0), so non-zero = not empty; a spilled build adds 1, never a count
        (KIND, 2, n) if rd::<u8>(region, FORM) != FAILED => {
            let spilled = rd::<u64>(region, crate::ops::hashjoin::spill::build::SPILLED) != 0;
            emit(0, n + kept(region) as u64 + spilled as u64, rd::<u8>(region, FORM) as u64)
        }
        (0, 0, 0) => emit(0, 0, EMPTY as u64),
        _ => return None,
    }
    Some(1)
}

/// The payload columns of the build whose header is at `d`.
pub(crate) unsafe fn pay_of(d: *const u8) -> usize {
    rd::<u8>(d, crate::ops::hashjoin::build::PAY) as usize | (rd::<u8>(d, layout::PAY_HI) as usize) << 8
}

/// Stamp `pay` payload columns into the header at `d` (both bytes, so a header not zeroed past 64 bytes reads right).
pub(crate) unsafe fn set_pay(d: *mut u8, pay: usize) {
    debug_assert!(pay <= layout::MAX_PAY);
    wr::<u8>(d, crate::ops::hashjoin::build::PAY, pay as u8);
    wr::<u8>(d, layout::PAY_HI, (pay >> 8) as u8);
}

#[cfg(test)]
mod pay_tests {
    use super::*;

    #[test]
    fn the_width_round_trips_past_a_byte() {
        let mut h = [0xA5u8; 192];
        for pay in [0, 1, 32, 255, 256, 1000, layout::MAX_PAY] {
            unsafe {
                set_pay(h.as_mut_ptr(), pay);
                assert_eq!(pay_of(h.as_ptr()), pay);
            }
        }
        assert!(layout::PAY_HI < 64 && layout::PAY_HI != crate::ops::hashjoin::build::PAY && layout::PAY_HI != crate::ops::hashjoin::build::GUARD_BUCKETS);
    }
}
