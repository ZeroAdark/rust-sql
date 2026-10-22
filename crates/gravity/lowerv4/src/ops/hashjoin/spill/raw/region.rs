//! The spilling build region's runtime (rule R6: the family's unsafe code lives in its raw files, behind the safe IR
//! calls), moved as it was from spill/build.rs, whose lowering emits the calls: a record's partition and its file form
//! (`image`, `serialized_len`, `put_record`; spill/raw.rs `serialize_over`), the scatter of spilled partitions out of a
//! region (`scatter`), the host entries the build end calls (`spill_route`, `spill_grow`, `spill_flush`), and the merge's spill half
//! (`spilling`, `any_spilled`, `merge_into`, `finish_room`). The region layout and the file form are spill/build.rs's
//! module doc.
#![allow(unsafe_code)]
use gravity_abi::ctx::Ctx;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{Error, Partial, State};
use gravity_fn::arena::{AVec, ChargedVec, Kind, OpArena, Owner, Refused};

use crate::ops::hashjoin::build::{D_CSTR, D_VARLENA, END, FLAGS, F_COMPACT, HDR, HI, LIMIT, LO, N, PHASE, REC, TOP, WORDS};
use crate::ops::hashjoin::layout::{self, desc_at};
use crate::ops::hashjoin::rt::{append, head, headed, rd, tail, varlena_len, wr, Rt};
use crate::ops::hashjoin::spill::build::{route, wider, PBITS, ROW_STACK, SPILL, SPILLED};
use gravity_program::cost::JOIN_SPILL_EPOCH;
use crate::ops::hashjoin::spill::raw;
use crate::ops::hashjoin::strategies::dedup;

/// Record `i`'s partition: its image words hashed, then routed.
unsafe fn part_of(t: &Rt, i: usize, bits: u8) -> u32 {
    route(t.hash(i).to_abi(), bits)
}

/// Payload `c`'s image of record `i` (offset, bytes) when it has one in the region (`DESC` names a by-distance image, not NULL).
pub(crate) unsafe fn image(t: &Rt, i: usize, c: usize) -> Option<(usize, usize)> {
    let kind = rd::<u16>(t.d, desc_at(c));
    let (o, bit) = t.null_at(c);
    if kind == 0 || rd::<u64>(t.rec(i), o) >> bit & 1 != 0 {
        return None;
    }
    let off = rd::<u64>(t.d, END) as usize - rd::<u64>(t.rec(i), 8 * (t.words + 1 + c)) as usize;
    let p = t.d.add(off);
    let len = match kind {
        D_VARLENA => varlena_len(p),
        D_CSTR => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes_with_nul().len(),
        l => l as usize,
    };
    Some((off, len))
}

/// The bytes of the serialized record at `one` under header `d` (its record size, image words and payload kinds).
pub(crate) unsafe fn serialized_len(d: *const u8, one: *const u8) -> usize {
    let (rec, words, pay) = (rd::<u32>(d, REC) as usize, rd::<u8>(d, WORDS) as usize, layout::pay_of(d));
    let mut len = rec;
    for c in (0..pay).filter(|&c| rd::<u16>(d, desc_at(c)) != 0 && { let (o, bit) = layout::null_at(words, pay, c); rd::<u64>(one, o) >> bit & 1 == 0 }) {
        len += (rd::<u64>(one, 8 * (words + 1 + c)) as usize).next_multiple_of(8);
    }
    len
}

/// Moves the records of region `d` (`n` of them) that fall in partitions `mask` out: each is handed to `put` (partition,
/// bytes) and removed. The rest are packed in their order, and their images slide up to `END`, compacted in address order
/// so none is overwritten before it moves. `TOP` and `N` are set. Returns the records kept.
/// CONVERT1: its working memory (one serialized record, then the kept images' list) is carved from `a`, the hash build
/// operator's arena (`OpArena::scratch_of` its slot's owner: the allocator's chunks, charged to that operator's ledger
/// row and kind); a carve the allocator refuses is the statement's out of memory (53200), not the process's abort.
pub(crate) unsafe fn scatter(a: &OpArena, d: *mut u8, len: usize, n: usize, mask: u64, bits: u8, put: &mut dyn FnMut(u32, &[u8]) -> gravity_driver::Result<()>) -> gravity_driver::Result<usize> {
    wr::<u64>(d, N, n as u64);
    let t = Rt::of(d, len);
    let (mut keep, mut buf) = (0, AVec::<u8>::with_capacity(a, t.rec + 64).map_err(refused)?);
    for i in 0..n {
        let p = part_of(&t, i, bits);
        if mask >> p & 1 != 0 {
            // every byte of the record's file form is written over (`serialize_over`)
            buf.resize(a, raw::file_len(&t, i), 0).map_err(refused)?;
            raw::serialize_over(&t, i, buf.as_mut_slice(a));
            put(p, buf.as_slice(a))?;
            continue;
        }
        if keep != i {
            core::ptr::copy(t.rec(i), t.rec(keep), t.rec);
        }
        keep += 1;
    }
    let pay = layout::pay_of(d);
    // at most one image per kept record and by-distance payload (exactly that with no NULLs): the list's room taken
    // whole, one carve
    let cols = (0..pay).filter(|&c| rd::<u16>(t.d, desc_at(c)) != 0).count();
    let mut imgs = AVec::<(usize, usize, usize, usize)>::with_capacity(a, keep * cols).map_err(refused)?;
    for i in 0..keep {
        for c in 0..pay {
            if let Some((off, len)) = image(&t, i, c) {
                imgs.push(a, (off, len.next_multiple_of(8), i, c)).map_err(refused)?;
            }
        }
    }
    imgs.as_mut_slice(a).sort_unstable_by(|x, y| y.0.cmp(&x.0));
    let end = rd::<u64>(d, END) as usize;
    let mut top = end;
    for &(off, l8, i, c) in imgs.as_slice(a) {
        top -= l8;
        core::ptr::copy(d.add(off), d.add(top), l8);
        wr::<u64>(t.rec(i), 8 * (t.words + 1 + c), (end - top) as u64);
    }
    (wr::<u64>(d, TOP, top as u64), wr::<u64>(d, N, keep as u64));
    // the newest carve first: the arena's bump rewinds, so the caller's next scatter reuses the same chunk
    (imgs.release(a), buf.release(a));
    Ok(keep)
}

/// CONVERT1: a scratch carve the allocator refused: the statement's out of memory, as a refused region's.
fn refused(r: Refused) -> Error {
    Error::out_of_memory(r.bytes)
}

gravity_abi::host! {
    /// `route_row`'s host: when record `n` of `r` falls in a spilled partition, it goes to that partition's file (its images
    /// undone to `top0`; laid out on the stack unless past `ROW_STACK`) and the host returns 0. Otherwise it returns 1. On
    /// failure it returns -1 with the error held.
    pub(crate) unsafe fn spill_route(ctx: *mut Ctx, r: *mut u8, n: u64, top0: u64) -> i64 {
        let t = Rt::of(r, rd::<u32>(r, LIMIT) as usize);
        let bits = rd::<u8>(r, SPILL);
        let p = part_of(&t, n as usize, bits);
        if rd::<u64>(r, SPILLED) >> p & 1 == 0 {
            return 1;
        }
        let len = raw::file_len(&t, n as usize);
        let (mut stack, mut heap) = ([0u8; ROW_STACK], Vec::new());
        let one = match len <= ROW_STACK {
            true => &mut stack[..len],
            false => {
                heap.resize(len, 0); // alloc: Exec
                &mut heap[..]
            }
        };
        raw::serialize_over(&t, n as usize, one);
        wr::<u64>(r, TOP, top0);
        match v4::put(ctx, 0, bits, p, one) {
            v4::FAILED => -1,
            _ => 0,
        }
    }
}

gravity_abi::host! {
    /// `grow`'s host. It first undoes `TOP` to `top0`. It grows the region within the owner's limit (the new base, `N`
    /// unchanged). If that is refused, a keys-only build (D4904) drops its records whose key image another holds, and with
    /// a quarter gone returns the same base; else it widens the spilled partitions and writes their records to the owner's
    /// file (the same base, fewer records). When every partition is already out, it grows by name (D254). It returns 0 with the error
    /// held on failure, and writes the records kept to `out`.
    pub(crate) unsafe fn spill_grow(ctx: *mut Ctx, r: *mut u8, n: u64, rec: u64, top0: u64, out: *mut u64) -> u64 {
        *out = n;
        wr::<u64>(r, TOP, top0);
        let (end, head) = (rd::<u64>(r, END), (layout::base(layout::pay_of(r))) as u64 + n * rec);
        let tail = end - top0;
        let settle = |nb: u64| {
            let d = nb as usize as *mut u8;
            let limit = rd::<u32>(d, LIMIT) as u64;
            (wr::<u64>(d, END, limit), wr::<u64>(d, TOP, limit - tail));
            nb
        };
        match v4::grow_in(ctx, rec, head, tail) {
            v4::FAILED => return 0,
            1 => {}
            nb => return settle(nb as u64),
        }
        // CONVERT1: the compaction's and the scatters' working memory, on the build slot's operator row (the slot owner this
        // ctx runs for)
        let a = OpArena::scratch_of(OpArena::slot_owner(ctx), Kind::HashBuild);
        // D4904: refused room within its limit, a keys-only build drops the records whose image another holds first; a
        // quarter of them gone, the row retries in place
        let (n, head) = match dedup::compact(&a, r, rd::<u32>(r, LIMIT) as usize, n as usize) {
            Some(k) if 4 * k as u64 <= 3 * n => {
                *out = k as u64;
                return r as u64;
            }
            Some(k) => {
                *out = k as u64;
                (k as u64, layout::base(layout::pay_of(r)) as u64 + k as u64 * rec)
            }
            None => (n, head),
        };
        let (bits, mask) = (rd::<u8>(r, PBITS), rd::<u64>(r, SPILLED));
        let m2 = wider(mask, bits);
        if bits == 0 || m2 == mask {
            // C10: one partition level, every partition already out: the resident part grows past the grant (a named road)
            let nb = gravity_abi::program::region_grow_debt(ctx, rec, head, tail, gravity_program::spill::NoSpill::BuildPartitionPastGrant.code());
            return if nb == 0 { 0 } else { settle(nb) };
        }
        let len = rd::<u32>(r, LIMIT) as usize;
        // D4265: a build whose home a revoke cut gives its growth back: it spills until what it keeps fits the length it
        // opened at (`keep`; 0 = not cut, or it cannot give back), then shrinks to it (`give_back`)
        let keep = v4::cut_keep(ctx, r);
        let mut put = |p: u32, bytes: &[u8]| match v4::put(ctx, 0, bits, p, bytes) {
            v4::FAILED => Err(Error::host("gravity: a kind-7 build's spill write failed")),
            _ => Ok(()),
        };
        let (mut m, mut kept) = (mask, n as usize);
        loop {
            let m2 = wider(m, bits);
            match scatter(&a, r, len, kept, m2 & !m, bits, &mut put) {
                Ok(k) => (kept, m) = (k, m2),
                // D4438: `fail` replaces the held error. A put's own failure (53400 past temp_file_limit) is held as a host
                // error over the PG error the host stashed (gravity_hosts `stash`), which the statement raises (exec glue
                // `pg_error_of`), so the 53400 still stands
                Err(e) => {
                    v4::fail(ctx, e);
                    return 0;
                }
            }
            if keep == 0 || kept_fits(r, kept, rec, keep) || wider(m, bits) == m {
                break;
            }
        }
        (wr::<u8>(r, SPILL, bits), wr::<u64>(r, SPILLED, m));
        *out = kept as u64;
        if keep != 0 {
            let fits = kept_fits(r, kept, rec, keep);
            if fits {
                shrink_to(r, keep);
            }
            v4::give_back(ctx, r, keep, fits);
        }
        r as u64
    }
}

/// D4265: the `kept` records of `rec` bytes of build `d` and their images (`TOP` .. `END`) fit `keep` bytes.
unsafe fn kept_fits(d: *const u8, kept: usize, rec: u64, keep: u64) -> bool {
    let head = layout::base(layout::pay_of(d)) as u64 + kept as u64 * rec;
    let tail = rd::<u64>(d, END) - rd::<u64>(d, TOP);
    head + tail <= keep
}

/// D4265: build `d`'s images (`TOP` .. `END`) slid down to end at `keep` (`kept_fits`), `END` and `TOP` set: an image
/// word is its distance from `END`, so every kept record's stays valid. The driver arms `LIMIT` (`v4::give_back`). Run on
/// the owner's thread inside its grow: the build is read by no one else while its pipeline runs, and the IR's retry
/// reads `TOP` and `END` again from the header (build.rs `l_try`).
unsafe fn shrink_to(d: *mut u8, keep: u64) {
    let (end, top) = (rd::<u64>(d, END), rd::<u64>(d, TOP));
    let tail = end - top;
    if end != keep {
        core::ptr::copy(d.add(top as usize), d.add((keep - tail) as usize), tail as usize);
    }
    (wr::<u64>(d, END, keep), wr::<u64>(d, TOP, keep - tail));
}

gravity_abi::host! {
    /// `flush`'s host: 0, or `FAILED` with the error held.
    pub(crate) unsafe fn spill_flush(ctx: *mut Ctx, stream: u64, bits: u64) -> i64 {
        v4::flush(ctx, stream, bits as u8)
    }
}

// ---- the merge (join/build_rt.rs's BuildMerge) ----

/// Whether a region may spill: `PBITS` is set on a record layout the files take (not the compact bits form).
pub(crate) unsafe fn spilling(d: *const u8) -> bool {
    // a build that never spills (PBITS 0) reads its other spill words 0 too: the driver zeroed them at its open (`init`)
    debug_assert!(rd::<u8>(d, PBITS) != 0 || (rd::<u8>(d, SPILL) == 0 && rd::<u64>(d, SPILLED) == 0), "gravity: a build's spill words not zeroed at its open");
    rd::<u8>(d, PHASE) != 0 && rd::<u8>(d, PBITS) != 0 && rd::<u8>(d, FLAGS) & F_COMPACT == 0
}

/// Whether any partial spilled. If one did, `plan` in build_part.rs keeps the serial merge, whose leader spills too.
pub(crate) fn any_spilled(partials: &[Partial<'_>]) -> bool {
    // SAFETY: a partial of at least the header's bytes is this build's region, read only.
    partials.iter().filter(|p| p.bytes.len() >= HDR).any(|p| unsafe { rd::<u8>(p.bytes.as_ptr(), PHASE) != 0 && rd::<u64>(p.bytes.as_ptr(), SPILLED) != 0 })
}

/// The leader's spill writes, per partition. Each is written as an epoch of the leader's file past `JOIN_SPILL_EPOCH` (the cost home) and at the end.
/// CONVERT wave 2 (R1 (ii)): every block (a part's stage, the part list, the flush's sorted list) is a `ChargedVec` on the
/// leader slot's owner (`State::op_owner`), hash build kind: its capacity charged as it grows, given back as it drops.
struct Out {
    bits: u8,
    owner: Option<Owner>,
    parts: ChargedVec<(u32, ChargedVec<u8>)>,
}

/// CONVERT wave 2: a heap block the allocator refused: the statement's out of memory, as a refused region's.
fn refused_heap(bytes: usize) -> Error {
    Error::out_of_memory(bytes as u64)
}

impl Out {
    /// No part staged yet; its blocks charged to the leader's slot owner.
    fn of(leader: &State<'_>, bits: u8) -> Out {
        let owner = leader.op_owner();
        Out { bits, owner, parts: ChargedVec::new(owner, Kind::HashBuild) }
    }

    fn put(&mut self, st: &mut State<'_>, p: u32, bytes: &[u8]) -> gravity_driver::Result<()> {
        let at = match self.parts.iter().position(|e| e.0 == p) {
            Some(i) => i,
            None => {
                self.parts.push((p, ChargedVec::new(self.owner, Kind::HashBuild))).map_err(|_| refused_heap(core::mem::size_of::<(u32, ChargedVec<u8>)>()))?; // alloc: Exec charged self.owner
                self.parts.len() - 1
            }
        };
        self.parts[at].1.extend_from_slice(bytes).map_err(|_| refused_heap(bytes.len()))?; // alloc: Exec charged self.owner
        if self.parts[at].1.len() >= JOIN_SPILL_EPOCH {
            st.spill_write(self.bits, &[(p, self.parts[at].1.as_slice())])?;
            self.parts[at].1.clear();
        }
        Ok(())
    }

    fn flush(&mut self, st: &mut State<'_>) -> gravity_driver::Result<()> {
        // an epoch's parts go in ascending order (the spill set's write rule)
        let mut parts = ChargedVec::<(u32, &[u8])>::new(self.owner, Kind::HashBuild);
        parts.try_reserve(self.parts.len()).map_err(|_| refused_heap(core::mem::size_of::<(u32, &[u8])>() * self.parts.len()))?; // alloc: Exec charged self.owner
        for e in self.parts.iter().filter(|e| !e.1.is_empty()) {
            parts.push((e.0, e.1.as_slice())).map_err(|_| refused_heap(core::mem::size_of::<(u32, &[u8])>()))?; // alloc: Exec charged self.owner
        }
        parts.sort_unstable_by_key(|e| e.0);
        if !parts.is_empty() {
            st.spill_write(self.bits, parts.as_slice())?;
        }
        drop(parts);
        // each part's block (and its charge) dropped; the part list's block stays for the next fill
        self.parts.clear();
        Ok(())
    }
}

/// Widens the leader's partitions (`wider`) and writes out its records of the new ones. Returns false when every
/// partition is already out.
unsafe fn widen(st: &mut State<'_>, out: &mut Out) -> gravity_driver::Result<bool> {
    let d = st.bytes.as_mut_ptr();
    let (bits, mask) = (rd::<u8>(d, PBITS), rd::<u64>(d, SPILLED));
    let m2 = wider(mask, bits);
    if m2 == mask {
        return Ok(false);
    }
    spill_to(st, out, m2).map(|()| true)
}

/// The leader's spilled set made `m2` (a suffix of the partitions, as `wider` keeps it): its records of the partitions newly
/// in it written out, the rest compacted.
unsafe fn spill_to(st: &mut State<'_>, out: &mut Out, m2: u64) -> gravity_driver::Result<()> {
    let d = st.bytes.as_mut_ptr();
    let (bits, mask) = (rd::<u8>(d, PBITS), rd::<u64>(d, SPILLED));
    let (len, n) = (st.bytes.len(), rd::<u64>(d, N) as usize);
    // CONVERT1: the scatter's working memory, on the leader slot's operator row
    let a = OpArena::scratch_of(st.op_owner(), Kind::HashBuild);
    scatter(&a, d, len, n, m2 & !mask, bits, &mut |p, b| out.put(st, p, b))?;
    (wr::<u8>(d, SPILL, bits), wr::<u64>(d, SPILLED, m2));
    Ok(())
}

/// exactsize B1d (`pgrust.gravity_exact_size` = build | all): the spilled set a spilling build's end takes in ONE scatter,
/// from one count of its resident records and their images per partition (`part_of`, `image`): the longest resident
/// prefix [0, k) whose records, marks, KeyHash directory and images fit the region's own length or `State::room_in` (what
/// `grow_leader` takes without a grow, or grows to within G), where `widen` halves the prefix
/// each pass whatever the bytes (a build 10% past its room spilled half). The resident set stays a prefix (`wider`'s
/// invariant). `None` (the caller widens as before): not under build, nothing resident, or no shorter prefix (the room
/// fits the whole one by this count, which the lay-out's form may still pass).
unsafe fn fit_prefix(st: &State<'_>) -> Option<u64> {
    if !st.exact_build() {
        return None;
    }
    let (d, len) = (st.bytes.as_ptr() as *mut u8, st.bytes.len());
    let (bits, mask) = (rd::<u8>(d, PBITS), rd::<u64>(d, SPILLED));
    let parts = 1usize << bits.min(6);
    let all = if parts >= 64 { u64::MAX } else { (1u64 << parts) - 1 };
    let r = ((mask & all).trailing_zeros() as usize).min(parts);
    if r == 0 {
        return None;
    }
    let t = Rt::of(d, len);
    let (mut recs, mut imgs) = ([0usize; 64], [0usize; 64]);
    for i in 0..t.n {
        let p = part_of(&t, i, bits) as usize;
        recs[p] += 1;
        for c in 0..t.pay {
            if let Some((_, l)) = image(&t, i, c) {
                imgs[p] += l.next_multiple_of(8);
            }
        }
    }
    // the region's own end needs no grow (`grow_leader`'s `bytes <= end`): a G with less left than the region holds still
    // keeps the prefix that fits the region
    let room = (st.room_in().min(usize::MAX as u64) as usize).max(rd::<u64>(d, END) as usize);
    let fits = |k: usize| {
        let (m, b) = (recs[..k].iter().sum::<usize>(), imgs[..k].iter().sum::<usize>());
        // as `Rt::dir` lays them: the records, their marks (a marking build), a cache line up, then the KeyHash heads
        let at = (t.base + m * t.rec).next_multiple_of(64);
        let dir = if t.marks { (at + m).next_multiple_of(64) } else { at };
        dir + (8usize << crate::ops::hashjoin::rt::dir_bits(m as u64)) + b <= room
    };
    let k = (0..r).rev().find(|&k| fits(k))?;
    Some(mask | (all & !((1u64 << k) - 1)))
}

/// Grows the leader to `bytes` (within G when `within`), moving its images to the new end as build_rt's `grow_to` does.
/// Returns false when G refused. `counted`: `bytes` is the build's counted need at its end (its records, the directory laid
/// next), grown to as `grow_to_exact` grows (exactsize B1a); `None`: a merge's room, doubled as before.
pub(crate) unsafe fn grow_leader(st: &mut State<'_>, bytes: usize, within: bool, counted: Option<u64>) -> gravity_driver::Result<bool> {
    let d = st.bytes.as_mut_ptr();
    let fresh = rd::<u8>(d, PHASE) == 0;
    let (end, t) = if fresh { (st.bytes.len(), 0) } else { (rd::<u64>(d, END) as usize, tail(d)) };
    if bytes <= end {
        return Ok(true);
    }
    match (within, counted) {
        (true, None) if !st.grow_in(bytes)? => return Ok(false),
        (true, Some(n)) if !st.grow_in_exact(bytes, n)? => return Ok(false),
        (true, _) => {}
        // C10: a spilled build's leader merge past G grows past its grant (a named SPILL-DEBT road)
        (false, c) => st.grow_debt(bytes, c, gravity_program::spill::NoSpill::BuildSpillMerge)?,
    }
    let (d, len) = (st.bytes.as_mut_ptr(), st.bytes.len());
    if !fresh {
        core::ptr::copy(d.add(end - t), d.add(len - t), t);
        (wr::<u64>(d, TOP, (len - t) as u64), wr::<u64>(d, END, len as u64));
    }
    wr::<u32>(d, LIMIT, len.min(u32::MAX as usize) as u32);
    Ok(true)
}

/// Makes room for `need` more bytes below the leader's images: grown within G, else a keys-only build compacted (D4904),
/// else partitions widened, else a named grow.
unsafe fn room(st: &mut State<'_>, out: &mut Out, need: usize) -> gravity_driver::Result<()> {
    loop {
        let d = st.bytes.as_mut_ptr();
        let used = layout::base(layout::pay_of(d)) + rd::<u64>(d, N) as usize * rd::<u32>(d, REC) as usize;
        if used + need <= rd::<u64>(d, TOP) as usize {
            return Ok(());
        }
        let want = used + need + tail(d);
        if grow_leader(st, want, true, None)? || compacted(st) || widen(st, out)? {
            continue;
        }
        grow_leader(st, want, false, None)?;
    }
}

/// D4904: the leader's records compacted (strategies/dedup.rs `compact`), its working memory on the leader slot's operator
/// row; false when no pass ran (not a keys-only build, or a pass this open already kept most of them).
unsafe fn compacted(st: &mut State<'_>) -> bool {
    let a = OpArena::scratch_of(st.op_owner(), Kind::HashBuild);
    let (d, len) = (st.bytes.as_mut_ptr(), st.bytes.len());
    dedup::compact(&a, d, len, rd::<u64>(d, N) as usize).is_some()
}

/// BuildMerge's `merge_into` for a region that may spill (`spilling`). When neither side spilled and G holds both, it
/// appends as the plain merge does. Otherwise the partial's spilled partitions become the leader's (the leader's records
/// of them written out). Each of the partial's records then goes out when its partition is spilled, else in, with room
/// made as `room` makes it.
pub(crate) unsafe fn merge_into(leader: &mut State<'_>, partial: &[u8]) -> gravity_driver::Result<()> {
    let s = partial.as_ptr();
    headed(leader)?;
    let d = leader.bytes.as_mut_ptr();
    let fresh = rd::<u8>(d, PHASE) == 0;
    if rd::<u64>(s, SPILLED) == 0 && (fresh || rd::<u64>(d, SPILLED) == 0) {
        let (nd, td) = if fresh { (0, 0) } else { (rd::<u64>(d, N) as usize, tail(d)) };
        let want = layout::base(layout::pay_of(s)) + (nd + rd::<u64>(s, N) as usize) * rd::<u32>(s, REC) as usize + td + tail(s);
        if grow_leader(leader, want, true, None)? {
            append(leader.bytes, partial)?;
            let d = leader.bytes.as_mut_ptr();
            (wr::<u8>(d, SPILL, 0), wr::<u64>(d, SPILLED, 0));
            return Ok(());
        }
    }
    let d = leader.bytes.as_mut_ptr();
    if rd::<u8>(d, PHASE) == 0 {
        head(d, s, leader.bytes.len());
        (wr::<u8>(d, SPILL, 0), wr::<u64>(d, SPILLED, 0));
    }
    let bits = rd::<u8>(s, PBITS);
    let mut out = Out::of(leader, bits);
    let (mask, want) = (rd::<u64>(d, SPILLED), rd::<u64>(s, SPILLED));
    // the merge's working memory (the scatter's, then one serialized record at a time), on the leader slot's operator row
    let a = OpArena::scratch_of(leader.op_owner(), Kind::HashBuild);
    if want & !mask != 0 {
        let (len, n) = (leader.bytes.len(), rd::<u64>(d, N) as usize);
        scatter(&a, d, len, n, want & !mask, bits, &mut |p, b| out.put(leader, p, b))?;
        let d = leader.bytes.as_mut_ptr();
        (wr::<u8>(d, SPILL, bits), wr::<u64>(d, SPILLED, mask | want));
    }
    let ts = Rt::of(s as *mut u8, usize::MAX);
    let mut one = AVec::<u8>::with_capacity(&a, ts.rec + 64).map_err(refused)?;
    for i in 0..ts.n {
        let p = part_of(&ts, i, bits);
        // every byte of the record's file form is written over (`serialize_over`)
        one.resize(&a, raw::file_len(&ts, i), 0).map_err(refused)?;
        raw::serialize_over(&ts, i, one.as_mut_slice(&a));
        if rd::<u64>(leader.bytes.as_ptr(), SPILLED) >> p & 1 == 0 {
            room(leader, &mut out, one.len())?;
        }
        if rd::<u64>(leader.bytes.as_ptr(), SPILLED) >> p & 1 != 0 {
            out.put(leader, p, one.as_slice(&a))?;
            continue;
        }
        put_record(leader.bytes.as_mut_ptr(), one.as_slice(&a));
    }
    one.release(&a);
    let d = leader.bytes.as_mut_ptr();
    wr::<i64>(d, LO, rd::<i64>(d, LO).min(rd::<i64>(s, LO)));
    wr::<i64>(d, HI, rd::<i64>(d, HI).max(rd::<i64>(s, HI)));
    out.flush(leader)
}

/// Appends a serialized record to region `d` (room already made): its bytes become record `N`, and its images go below
/// `TOP`, kept by distance. Returns the serialized bytes it took.
pub(crate) unsafe fn put_record(d: *mut u8, one: &[u8]) -> usize {
    let t = Rt::of(d, usize::MAX);
    let at = t.rec(t.n);
    core::ptr::copy_nonoverlapping(one.as_ptr(), at, t.rec);
    let (end, mut src) = (rd::<u64>(d, END) as usize, t.rec);
    for c in 0..t.pay {
        let (o, bit) = t.null_at(c);
        if rd::<u16>(d, desc_at(c)) == 0 || rd::<u64>(t.rec(t.n), o) >> bit & 1 != 0 {
            continue;
        }
        let len = rd::<u64>(at, 8 * (t.words + 1 + c)) as usize;
        let top = rd::<u64>(d, TOP) as usize - len.next_multiple_of(8);
        core::ptr::copy_nonoverlapping(one.as_ptr().add(src), d.add(top), len);
        wr::<u64>(d, TOP, top as u64);
        wr::<u64>(at, 8 * (t.words + 1 + c), (end - top) as u64);
        src += len.next_multiple_of(8);
    }
    wr::<u64>(d, N, t.n as u64 + 1);
    src
}

/// BuildMerge's `finish` for a region that may spill. Room for the directory is made within G, else by spilling more of
/// the leader, else by a named grow, and the spilled records are written. `need` gives the region bytes the lay-out
/// needs over the records now in.
pub(crate) unsafe fn finish_room(leader: &mut State<'_>, need: &dyn Fn(*mut u8, usize) -> usize) -> gravity_driver::Result<()> {
    let bits = rd::<u8>(leader.bytes.as_ptr(), PBITS);
    let mut out = Out::of(leader, bits);
    let mut fitted = false;
    loop {
        let (d, len) = (leader.bytes.as_mut_ptr(), leader.bytes.len());
        let want = need(d, len);
        // exactsize B1a: the resident records are counted here (N, KEPT): their directory's need is exact
        let counted = Some(rd::<u64>(d, N) + crate::ops::hashjoin::raw::kept(d) as u64);
        if grow_leader(leader, want, true, counted)? {
            break;
        }
        if compacted(leader) {
            continue;
        }
        // exactsize B1d: one counted spill to the prefix that fits (once: a room the draw still refuses was misjudged, and
        // the halving after it bounds the passes), else the halving
        let fit = if fitted { None } else { fit_prefix(leader) };
        if let Some(m2) = fit {
            fitted = true;
            spill_to(leader, &mut out, m2)?;
            fires::fire_shared("exact.spill_fit"); // shapecheck fire signal (exactsize B1d)
            continue;
        }
        if !widen(leader, &mut out)? {
            grow_leader(leader, want, false, counted)?;
            break;
        }
    }
    out.flush(leader)
}
