//! D2715's split of a spilled partition past its region's length (a run-time cap grows or
//! follows PostgreSQL's spill, never raises where PostgreSQL answers; PG 18.6 keeps doubling its batches). The pair
//! pipeline (pair.rs) joins such a partition in sub-passes: its records and its kept probe rows group by the KeyHash's
//! next bits below the partition's (`fine`), and each sub-pass lays out one group's records in a region of its own and
//! hands on only that group's kept rows, so a key's matches and misses all meet in one sub-pass (INNER, LEFT, SEMI, ANTI).
//! A key no hash bit splits stays one group, one region past the cap (at 4 GiB load.rs `take_pair` refuses it by name).
//! A drained build (RIGHT / FULL / RIGHT SEMI / RIGHT ANTI) gathers each sub-pass's drain records, and the records of the
//! groups no kept row reached, into its partition's one drain part (`drain_end`); the drain reads a part past the cap in
//! chunks (`chunk_of`). The pair region's header carries the split (`Split`, pair.rs `M_SUB` .. `M_DLEN`); the groups
//! and the drain records live until the partition's end.
#![allow(unsafe_code)]
/// The pair pipeline's runtime: the kept rows' host, each partition's reload (`spill::load`, re-exported there).
pub(crate) mod load;
/// The spilling build region's runtime: the build end's host entries and the merge's spill half (spill/build.rs re-exports
/// what the rest of the family reads).
pub(crate) mod region;
use gravity_abi::{word, PtrWord, Word};
use gravity_abi::ctx::{Ctx, Partials};
use gravity_abi::hash::KeyHash;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::Error;
use gravity_kernels::table::image::AlignedImage;
use gravity_program::cost::JOIN_SPILL_EPOCH;
use gravity_fn::arena::{ChargedVec, Kind, Owner};

use crate::ops::hashjoin::build::{D_CSTR, D_VARLENA, REC, WORDS};
use gravity_abi::limits::JOIN_KEY_IMAGE_WORDS;
use crate::ops::hashjoin::layout;
use crate::ops::hashjoin::rt::{rd, varlena_len, wr, Rt};
use crate::ops::hashjoin::spill::build::{image, remix, serialized_len};
use crate::ops::hashjoin::spill::load::{arg_bits, close, part_len, part_region, records, Buf};
use crate::ops::hashjoin::drain::spill::{drain_bytes, drain_each, drain_head, u_part};
use crate::ops::hashjoin::spill::pair::{DRAIN, DRAIN_MARKED, FILL, FIRST, MAGIC, META, MORE, M_BND, M_DCAP, M_DLEN, M_DP, M_GRP, M_HI, M_KBND, M_KEPT, M_KOFF, M_MORE, M_NKEPT, M_OFFS, M_ORIG, M_PART, M_SBITS, M_SUB, M_TABLE};

/// A split partition's state in its pair region's header: the open sub-partition and the split bits, the grouped records
/// and their bounds, the grouped kept rows' offsets and their bounds, a drained build's drain records so far. `NONE`: an
/// unsplit partition (every word 0).
#[derive(Clone, Copy)]
pub(crate) struct Split {
    pub(crate) sub: u64,
    pub(crate) sbits: u8,
    pub(crate) grp: *mut u8,
    pub(crate) bnd: *mut u8,
    pub(crate) koff: *mut u8,
    pub(crate) kbnd: *mut u8,
    dp: *mut u8,
    dcap: usize,
    dlen: usize,
}

impl Split {
    pub(crate) const NONE: Split = Split::of(0, 0, [core::ptr::null_mut(); 4]);

    pub(crate) const fn of(sub: u64, sbits: u8, [grp, bnd, koff, kbnd]: [*mut u8; 4]) -> Split {
        Split { sub, sbits, grp, bnd, koff, kbnd, dp: core::ptr::null_mut(), dcap: 0, dlen: 0 }
    }

    unsafe fn load(m: *const u8) -> Split {
        let ptr = |off: usize| rd::<u64>(m, off) as usize as *mut u8;
        let (dcap, dlen) = (rd::<u64>(m, M_DCAP) as usize, rd::<u64>(m, M_DLEN) as usize);
        Split { sub: rd::<u64>(m, M_SUB), sbits: rd::<u64>(m, M_SBITS) as u8, grp: ptr(M_GRP), bnd: ptr(M_BND), koff: ptr(M_KOFF), kbnd: ptr(M_KBND), dp: ptr(M_DP), dcap, dlen }
    }

    unsafe fn store(&self, m: *mut u8) {
        (wr::<u64>(m, M_SUB, self.sub), wr::<u64>(m, M_SBITS, self.sbits as u64), wr::<u64>(m, M_GRP, self.grp as u64), wr::<u64>(m, M_BND, self.bnd as u64));
        (wr::<u64>(m, M_KOFF, self.koff as u64), wr::<u64>(m, M_KBND, self.kbnd as u64));
        (wr::<u64>(m, M_DP, self.dp as u64), wr::<u64>(m, M_DCAP, self.dcap as u64), wr::<u64>(m, M_DLEN, self.dlen as u64));
    }

    /// Room for `more` bytes past the drain records, their buffer doubling (`v4::take_reload`).
    unsafe fn reserve(&mut self, ctx: *mut Ctx, more: usize) -> gravity_driver::Result<()> {
        if self.dlen + more > self.dcap {
            let cap = (2 * self.dcap).max(self.dlen + more).max(FIRST);
            let n = v4::take_reload(ctx, cap);
            if n.is_null() {
                return Err(Error::host("gravity: a split partition's drain records were refused"));
            }
            if !self.dp.is_null() {
                core::ptr::copy_nonoverlapping(self.dp, n, self.dlen);
                v4::give(ctx, self.dp);
            }
            (self.dp, self.dcap) = (n, cap);
        }
        Ok(())
    }

    /// `bytes` (whole serialized records) appended to the drain records (`reserve`).
    unsafe fn gather(&mut self, ctx: *mut Ctx, bytes: &[u8]) -> gravity_driver::Result<()> {
        self.reserve(ctx, bytes.len())?;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.dp.add(self.dlen), bytes.len());
        self.dlen += bytes.len();
        Ok(())
    }

    /// Group `k`'s serialized records.
    unsafe fn group(&self, k: u64) -> &'static [u8] {
        let (lo, hi) = (rd::<u64>(self.bnd, 8 * k as usize) as usize, rd::<u64>(self.bnd, 8 * (k as usize + 1)) as usize);
        if hi == lo { &[] } else { core::slice::from_raw_parts(self.grp.add(lo), hi - lo) }
    }
}

/// Pair region `m` (its build region `d`) headed: the original build `orig`, the kept rows (`kept`, `rows` of them handed
/// on at `offs`), the partition `p` of the claim's `..end`, D2715's split (`Split`); then the probe's build `i` and the
/// owner's `more` table (a copy of the original's) point at `d`.
#[allow(clippy::type_complexity)]
pub(crate) unsafe fn install(pp: *mut Partials, i: usize, ncons: usize, (m, d): (*mut u8, *mut u8), orig: *const u8, (kept, offs, rows): (*mut u8, *mut u8, usize), (p, end): (u32, u64), split: Split) {
    (wr::<u64>(m, 0, MAGIC), wr::<u64>(m, M_ORIG, orig as u64), wr::<u64>(m, M_KEPT, kept as u64), wr::<u64>(m, M_OFFS, offs as u64));
    (wr::<u64>(m, M_NKEPT, rows as u64), wr::<u64>(m, M_MORE, (*pp).more.ptr() as usize as u64), wr::<u64>(m, M_PART, p as u64), wr::<u64>(m, M_HI, end));
    split.store(m);
    let table = m.add(M_TABLE) as *mut Word<*mut u8>;
    if !(*pp).more.ptr().is_null() {
        core::ptr::copy_nonoverlapping((*pp).more.ptr(), table, ncons.min(MORE));
    }
    *table.add(i) = word(d);
    (*pp).more = word(table as *const Word<*mut u8>);
    if i < 2 {
        (*pp).consumed[i] = word(d);
    }
}

/// The KeyHash bits below the partition's a split reads at its finest (`fine`); a group still past its cap there holds one
/// key's records (a region over 4 GiB: mem's question, BLOCKERS).
const FINE_BITS: u8 = 16;

/// The bytes a partition's region may take before it splits (its length is a u32, load.rs `take_pair`).
/// `PGRUST_GRAVITY_PAIR_PART_BYTES` lowers it: a test lever, so a witness splits at test scale. Read at each spilled
/// partition's open (a cold path), never cached.
pub(crate) fn part_cap() -> usize {
    let max = u32::MAX as usize - META;
    gravity_driver::env::num::<usize>("PGRUST_GRAVITY_PAIR_PART_BYTES").map_or(max, |c| c.clamp(4096, max))
}

/// KeyHash `h`'s finest sub-partition under a partition of `bits`: the `FINE_BITS` bits of `route`'s remixed hash
/// right below the partition's, so a sub-partition at `s` bits is this `>> (FINE_BITS - s)` and keeps its keys together.
pub(crate) fn fine(h: u64, bits: u8) -> usize {
    fine_mix(remix(h), bits)
}

/// `fine` of an already remixed KeyHash `m`, of which only the top half need be known (a kept row's, `kept_mix`).
fn fine_mix(m: u64, bits: u8) -> usize {
    ((m << bits) >> (64 - FINE_BITS as u32)) as usize
}

// a kept row carries its remixed KeyHash's top half: every partition bit and split bit `fine` reads
const _: () = assert!(crate::ops::hashjoin::spill::build::BITS + FINE_BITS <= 32);

/// The KeyHash of the serialized record at `one` under `orig`: its image words, as `Rt::hash` reads a laid-out one.
unsafe fn raw_hash(orig: *const u8, one: *const u8) -> u64 {
    let words = (rd::<u8>(orig, WORDS) as usize).min(JOIN_KEY_IMAGE_WORDS);
    let mut w = AlignedImage([0u64; JOIN_KEY_IMAGE_WORDS]);
    for (j, x) in w.0.iter_mut().enumerate().take(words) {
        *x = rd::<u64>(one, 8 * j);
    }
    KeyHash::of(&w.0[..words]).to_abi()
}

// ---- the spill rows' byte layout (the hosts' unsafe halves: spill/build.rs `spill_route`, load.rs `kept_put`) ----

/// The bytes record `i` takes in a file (`serialize_over`).
pub(crate) unsafe fn file_len(t: &Rt, i: usize) -> usize {
    t.rec + (0..t.pay).filter_map(|c| image(t, i, c)).map(|(_, len)| len.next_multiple_of(8)).sum::<usize>()
}

/// Record `i` as a file holds it (spill/build.rs's module doc), written over `out` (`file_len` bytes).
pub(crate) unsafe fn serialize_over(t: &Rt, i: usize, out: &mut [u8]) {
    out[..t.rec].copy_from_slice(core::slice::from_raw_parts(t.rec(i), t.rec));
    let mut at = t.rec;
    for c in 0..t.pay {
        let Some((off, len)) = image(t, i, c) else { continue };
        wr::<u64>(out.as_mut_ptr(), 8 * (t.words + 1 + c), len as u64);
        out[at..at + len].copy_from_slice(core::slice::from_raw_parts(t.d.add(off), len));
        let end = (at + len).next_multiple_of(8);
        out[at + len..end].fill(0);
        at = end;
    }
}

/// Record `i` as a file holds it (`serialize_over`'s bytes), handed to `emit` in pieces read in place: the record's words
/// up to each image word, that image's length, the words after the last; then each image and its pad. No buffer.
pub(crate) unsafe fn serialize_each<E>(t: &Rt, i: usize, emit: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
    let rec = core::slice::from_raw_parts(t.rec(i), t.rec);
    let mut at = 0;
    for c in 0..t.pay {
        let Some((_, len)) = image(t, i, c) else { continue };
        let w = 8 * (t.words + 1 + c);
        emit(&rec[at..w])?;
        emit(&(len as u64).to_ne_bytes())?;
        at = w + 8;
    }
    emit(&rec[at..])?;
    let mut at = t.rec;
    for c in 0..t.pay {
        let Some((off, len)) = image(t, i, c) else { continue };
        emit(core::slice::from_raw_parts(t.d.add(off), len))?;
        let end = (at + len).next_multiple_of(8);
        emit(&[0u8; 8][..end - at - len])?;
        at = end;
    }
    Ok(())
}

/// The bytes of a by-reference cell's image at `d` of payload kind `kind`.
unsafe fn image_len(kind: u16, d: u64) -> usize {
    let at = d as usize as *const u8;
    match kind {
        D_VARLENA => varlena_len(at),
        D_CSTR => core::ffi::CStr::from_ptr(at as *const core::ffi::c_char).to_bytes_with_nul().len(),
        l => l as usize,
    }
}

/// The bytes of the kept row of `cells` (`kept_fill`).
pub(crate) unsafe fn kept_len(cells: &[u64]) -> usize {
    let n = cells.len() / 3;
    let images: usize = (0..n).filter(|&i| cells[3 * i + 2] == 0 && cells[3 * i] != 0).map(|i| image_len(cells[3 * i] as u16, cells[3 * i + 1]).next_multiple_of(8)).sum();
    8 + 8 * n.div_ceil(64) + 8 * n + images
}

/// The kept row (load.rs's module doc) of `cells` (`[kind, datum, null]` each; a by-reference datum's image is read at
/// it) and KeyHash `h`, written over `row` (`kept_len` bytes).
pub(crate) unsafe fn kept_fill(cells: &[u64], h: u64, row: &mut [u8]) {
    let n = cells.len() / 3;
    let nw = n.div_ceil(64);
    let mut off = 8 + 8 * nw + 8 * n;
    row[..off].fill(0);
    for i in 0..n {
        let (kind, d, null) = (cells[3 * i] as u16, cells[3 * i + 1], cells[3 * i + 2] != 0);
        let word = if null {
            row[8 + 8 * (i / 64) + (i % 64) / 8] |= 1 << (i % 8);
            0
        } else if kind == 0 {
            d
        } else {
            let len = image_len(kind, d);
            let at = off;
            row[at..at + len].copy_from_slice(core::slice::from_raw_parts(d as usize as *const u8, len));
            off = (at + len).next_multiple_of(8);
            row[at + len..off].fill(0);
            at as u64
        };
        wr::<u64>(row.as_mut_ptr(), 8 + 8 * nw + 8 * i, word);
    }
    wr::<u32>(row.as_mut_ptr(), 0, row.len() as u32);
    wr::<u32>(row.as_mut_ptr(), 4, (remix(h) >> 32) as u32);
}

/// Kept row `row`'s remixed KeyHash as far as it carries it (its top half, load.rs's module doc; the rest 0).
pub(crate) unsafe fn kept_mix(row: *const u8) -> u64 {
    (rd::<u32>(row, 4) as u64) << 32
}

/// The split bits partition `raw` (under `orig`, of a build of `bits`) joins at: 0 when its region fits `cap`
/// (`part_cap()`); else the fewest at which each sub-partition fits half of it (its directory may grow the region) or is
/// one finest bucket past half of it with the rest within (one key no hash bit splits: its sub-partition is one region
/// past the cap, and at 4 GiB `take_pair` refuses it by name); else `FINE_BITS`.
/// CONVERT wave 2: the finest buckets' counts are charged to `owner` (the pair claim's: the slot its pipeline produces into,
/// else the statement's, `OpArena::slot_owner_kind`); `None`: uncharged.
pub(crate) unsafe fn split_bits(orig: *const u8, raw: &[u8], bits: u8, cap: usize, owner: Option<(Owner, Kind)>) -> gravity_driver::Result<u8> {
    let (rec, base) = (rd::<u32>(orig, REC) as usize, layout::base(layout::pay_of(orig)));
    // `part_len` is under base + 3 x raw + 256 kB (the directory at load 1/2 over records of 16 bytes or more, 1/8 up to
    // 2048 records): far under the cap, no record is walked
    if base + 5 * raw.len() + (1 << 20) <= cap {
        return Ok(0);
    }
    let (n, imgs) = records(orig, raw)?;
    if part_len(orig, n, imgs, base) <= cap {
        return Ok(0);
    }
    let mut each = ChargedVec::new(owner.map(|o| o.0), owner.map_or(Kind::Region, |o| o.1));
    each.resize(1 << FINE_BITS, (0usize, 0usize)).map_err(|_| Error::out_of_memory(16 << FINE_BITS))?; // alloc: Exec charged owner
    let mut o = 0;
    while o < raw.len() {
        let one = raw.as_ptr().add(o);
        let l = serialized_len(orig, one);
        let e = &mut each[fine(raw_hash(orig, one), bits)];
        (e.0, e.1, o) = (e.0 + 1, e.1 + (l - rec), o + l);
    }
    let fits = |n: usize, i: usize| part_len(orig, n, i, base) <= cap / 2;
    let ok = |c: &[(usize, usize)]| {
        let (n, i) = c.iter().fold((0, 0), |a, e| (a.0 + e.0, a.1 + e.1));
        let big = c.iter().copied().max_by_key(|e| e.0 * rec + e.1).unwrap_or((0, 0));
        fits(n, i) || (!fits(big.0, big.1) && fits(n - big.0, i - big.1))
    };
    Ok((1..=FINE_BITS).find(|&s| each.chunks(1 << (FINE_BITS - s)).all(ok)).unwrap_or(FINE_BITS))
}

/// Kept rows `kept` (`klen` bytes, `rows` of them) grouped by sub-partition at `s` split bits: their offsets in
/// sub-partition order and the groups' bounds (`2^s + 1` indexes into them, `kept_group_into`).
pub(crate) unsafe fn kept_groups(ctx: *mut Ctx, (kept, klen, rows): (*const u8, usize, usize), bits: u8, s: u8) -> gravity_driver::Result<(Buf, Buf)> {
    let g = 1usize << s;
    let (koff, kbnd) = (Buf::new(ctx, (8 * rows).max(8))?, Buf::new(ctx, 8 * (g + 1))?);
    kept_group_into(kept, klen, bits, s, core::slice::from_raw_parts_mut(kbnd.p as *mut u64, g + 1), core::slice::from_raw_parts_mut(koff.p as *mut u64, rows));
    Ok((koff, kbnd))
}

/// The offsets of kept rows `kept` (`klen` bytes) in the order of their sub-partitions at `s` split bits (their KeyHash,
/// `kept_mix`); `offs[bnd[k]..bnd[k + 1]]` are sub-partition `k`'s.
unsafe fn kept_group_into(kept: *const u8, klen: usize, bits: u8, s: u8, bnd: &mut [u64], offs: &mut [u64]) {
    let sub = |o: usize| fine_mix(kept_mix(kept.add(o)), bits) >> (FINE_BITS - s);
    let each = |f: &mut dyn FnMut(usize)| {
        let mut o = 0;
        while o < klen {
            f(o);
            o += rd::<u32>(kept, o) as usize;
        }
    };
    bnd.fill(0);
    each(&mut |o| bnd[sub(o) + 1] += 1);
    for k in 1..bnd.len() {
        bnd[k] += bnd[k - 1];
    }
    // each group's start is its cursor; past the walk it is the group's end, so the bounds move up one place
    each(&mut |o| {
        let k = sub(o);
        offs[bnd[k] as usize] = o as u64;
        bnd[k] += 1;
    });
    bnd.copy_within(..bnd.len() - 1, 1);
    bnd[0] = 0;
}

/// `raw`'s records (under `orig`) grouped by sub-partition at `s` split bits into a buffer of their own, and the
/// groups' bounds (`2^s + 1` offsets into it, `group_into`). The pair gives both back at its partition's end.
pub(crate) unsafe fn group(ctx: *mut Ctx, orig: *const u8, raw: &[u8], bits: u8, s: u8) -> gravity_driver::Result<(Buf, Buf)> {
    let g = 1usize << s;
    let bnd = Buf::new(ctx, 8 * (g + 1))?;
    let mut grp = Buf::new(ctx, raw.len().max(8))?;
    group_into(orig, raw, bits, s, core::slice::from_raw_parts_mut(bnd.p as *mut u64, g + 1), core::slice::from_raw_parts_mut(grp.p, raw.len()));
    grp.len = raw.len();
    Ok((grp, bnd))
}

/// `raw`'s records copied into `out` in the order of their sub-partitions at `s` split bits; `bnd[k]..bnd[k + 1]` is
/// sub-partition `k`'s bytes.
unsafe fn group_into(orig: *const u8, raw: &[u8], bits: u8, s: u8, bnd: &mut [u64], out: &mut [u8]) {
    let sub = |one: *const u8| fine(raw_hash(orig, one), bits) >> (FINE_BITS - s);
    let each = |f: &mut dyn FnMut(*const u8, usize)| {
        let mut o = 0;
        while o < raw.len() {
            let (one, l) = (raw.as_ptr().add(o), serialized_len(orig, raw.as_ptr().add(o)));
            f(one, l);
            o += l;
        }
    };
    bnd.fill(0);
    each(&mut |one, l| bnd[sub(one) + 1] += l as u64);
    for k in 1..bnd.len() {
        bnd[k] += bnd[k - 1];
    }
    // each group's start is its cursor; past the walk it is the group's end, so the bounds move up one place
    each(&mut |one, l| {
        let k = sub(one);
        out[bnd[k] as usize..bnd[k] as usize + l].copy_from_slice(core::slice::from_raw_parts(one, l));
        bnd[k] += l as u64;
    });
    bnd.copy_within(..bnd.len() - 1, 1);
    bnd[0] = 0;
}

/// The first sub-partition from `split.sub` (of `2^sbits`) of partition `p` (the claim's `..end`) with a kept row to hand
/// on (with a record, unless the probe fills) opened as `open_pair` opens a partition: its grouped records laid out in a
/// region of their own, its kept rows (`kept`, at the grouped offsets) handed on. With none left, the last sub-partition
/// opens empty and the partition ends at its window. A drained build's groups passed over (no kept row met their records)
/// send every record (`gather`), but RIGHT SEMI's (none matched). The kept rows and the groups stay for the partition's
/// later sub-partitions.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn open_sub(ctx: *mut Ctx, pp: *mut Partials, i: usize, arg: u64, ncons: usize, orig: *const u8, kept: *mut u8, (p, end): (u32, u64), mut split: Split) -> Result<(), Option<Error>> {
    let (fill, g) = (arg & FILL != 0, 1u64 << split.sbits);
    let at = |b: *mut u8, k: u64| rd::<u64>(b, 8 * k as usize) as usize;
    let (sub, rows) = match (split.sub..g).find(|&k| at(split.kbnd, k + 1) > at(split.kbnd, k) && (fill || at(split.bnd, k + 1) > at(split.bnd, k))) {
        Some(k) => (k, at(split.kbnd, k + 1) - at(split.kbnd, k)),
        None => (g, 0),
    };
    if arg & DRAIN != 0 && arg & DRAIN_MARKED == 0 {
        for k in split.sub..sub {
            split.gather(ctx, split.group(k))?;
        }
    }
    let raw: &[u8] = if rows == 0 { &[] } else { split.group(sub) };
    let (m, d, _, _) = part_region(ctx, META, orig, raw)?;
    split.sub = sub.min(g - 1);
    install(pp, i, ncons, (m, d), orig, (kept, split.koff.add(8 * at(split.kbnd, sub.min(g - 1))), rows), (p, end), split);
    Ok(())
}

/// The pair at `m`'s next sub-partition opened in its place (`open_sub`): a drained build's sub-pass first gathers its
/// drain records, then the current one is closed (load.rs `close`, its kept rows and groups kept for the next).
pub(crate) unsafe fn next_sub(ctx: *mut Ctx, pp: *mut Partials, i: usize, m: *mut u8, (stream, arg): (u64, u64), ncons: usize) -> Result<(), Option<Error>> {
    if arg & DRAIN != 0 {
        drain_sub(ctx, arg, m)?;
    }
    let (orig, kept) = (rd::<u64>(m, M_ORIG) as usize as *const u8, rd::<u64>(m, M_KEPT) as usize as *mut u8);
    let (p, end, mut split) = (rd::<u64>(m, M_PART) as u32, rd::<u64>(m, M_HI), Split::load(m));
    if close(ctx, pp, i, m, stream, arg, false) < 0 {
        return Err(None);
    }
    split.sub += 1;
    open_sub(ctx, pp, i, arg, ncons, orig, kept, (p, end), split)
}

/// A drained build's sub-pass at `m`: the records its drain sends (its region's, `drain_each`) serialized straight into
/// the gathered room (`serialize_over`), reserved whole first (`drain_bytes`: the one `reserve` a gather of them all makes).
unsafe fn drain_sub(ctx: *mut Ctx, arg: u64, m: *mut u8) -> gravity_driver::Result<()> {
    let (d, marked) = (m.add(META), arg & DRAIN_MARKED != 0);
    let mut split = Split::load(m);
    split.reserve(ctx, drain_bytes(d, marked))?;
    if !split.dp.is_null() {
        let room = core::slice::from_raw_parts_mut(split.dp, split.dcap);
        let mut at = split.dlen;
        let _ = drain_each(d, marked, &mut |t, i| -> Result<(), core::convert::Infallible> {
            let len = file_len(t, i);
            serialize_over(t, i, &mut room[at..at + len]);
            at += len;
            Ok(())
        });
        split.dlen = at;
    }
    split.store(m);
    Ok(())
}

/// load.rs `close`'s drain at a split partition's end: the last sub-pass's drain records gathered, then the partition's
/// drain part written and flushed (`[MAGIC_U | records]`, as `write_drain` writes one) and the gathered records given
/// back. D4431: the header and the gathered records go to the stage as they lie, the records in `JOIN_SPILL_EPOCH`
/// slices (no copy of the part; the stage writes them as epochs of its own size, which a read joins back). 0, or `FAILED`
/// with the error held.
pub(crate) unsafe fn drain_end(ctx: *mut Ctx, stream: u64, arg: u64, m: *mut u8) -> i64 {
    if let Err(e) = drain_sub(ctx, arg, m) {
        return v4::fail(ctx, e);
    }
    let split = Split::load(m);
    let body: &[u8] = if split.dlen == 0 { &[] } else { core::slice::from_raw_parts(split.dp, split.dlen) };
    let n = match records(rd::<u64>(m, M_ORIG) as usize as *const u8, body) {
        Ok((n, _)) => n,
        Err(e) => return v4::fail(ctx, e),
    };
    let (bits, at) = (arg_bits(arg), u_part(arg_bits(arg), rd::<u64>(m, M_PART) as u32));
    let head = drain_head(n as u32);
    for piece in core::iter::once(&head[..]).chain(body.chunks(JOIN_SPILL_EPOCH)) {
        if v4::put(ctx, stream, bits, at, piece) == v4::FAILED {
            return v4::FAILED;
        }
    }
    v4::flush(ctx, stream, bits)
}

/// The drain's chunk of a spilled part's records `body` (under `orig`) from byte `from`: its end, so a region of the
/// records `from..end` fits half of `cap` (one record at least); the whole when the whole fits `cap`.
pub(crate) unsafe fn chunk_of(orig: *const u8, body: &[u8], from: usize, cap: usize) -> gravity_driver::Result<usize> {
    let base = layout::base(layout::pay_of(orig));
    let rec = rd::<u32>(orig, REC) as usize;
    if from == 0 && base + 5 * body.len() + (1 << 20) <= cap {
        return Ok(body.len());
    }
    let (mut n, mut imgs, mut o) = (0usize, 0usize, from);
    while o < body.len() {
        let l = serialized_len(orig, body.as_ptr().add(o));
        if o + l > body.len() {
            return Err(Error::host("gravity: a spill drain part ends inside a record"));
        }
        if n > 0 && part_len(orig, n + 1, imgs + (l - rec), base) > cap / 2 {
            break;
        }
        (n, imgs, o) = (n + 1, imgs + (l - rec), o + l);
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::hashjoin::build::{ALLOW, A_TAGS, DIR, FLAGS, F_RANGE, HDR, KEPT, MARKS, N, PHASE};

    /// A build header: one image word, one payload word, no record yet (load.rs's test region).
    fn orig() -> Vec<u64> {
        let mut buf = vec![0u64; HDR / 8 + 1];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` covers the header.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, ALLOW, A_TAGS), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, 99), wr::<u64>(d, KEPT, 7), wr::<u64>(d, MARKS, 4096), wr::<u64>(d, DIR, 4096));
        }
        buf
    }

    /// `keys` serialized as a build part holds them, the row's number in its payload word.
    fn raw(keys: &[i64]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            out.extend(k.to_ne_bytes());
            out.extend(0u64.to_ne_bytes());
            out.extend((i as u64).to_ne_bytes());
        }
        out
    }

    /// D2715: a partition past its cap splits by the KeyHash's next bits: each key's records land in one group, every
    /// record in the group its sub-partition names, each group within half the cap, the bytes whole; a kept row of the
    /// same key (its KeyHash as `exit` writes it) names the same group. A partition within its cap does not split.
    #[test]
    fn a_partition_past_its_cap_splits_by_the_next_hash_bits() {
        let o = orig();
        let op = o.as_ptr() as *const u8;
        let keys: Vec<i64> = (0..4000).map(|i| i % 1000).collect();
        let r = raw(&keys);
        // SAFETY: `o` is a build header, `r` its serialized records.
        unsafe {
            let whole = {
                let (n, i) = records(op, &r).unwrap();
                part_len(op, n, i, HDR)
            };
            assert_eq!(split_bits(op, &r, 6, whole, None).unwrap(), 0);
            let cap = whole / 4;
            let s = split_bits(op, &r, 6, cap, None).unwrap();
            assert!((1..FINE_BITS).contains(&s), "{s}");
            let g = 1usize << s;
            let (mut bnd, mut out) = (vec![0u64; g + 1], vec![0u8; r.len()]);
            group_into(op, &r, 6, s, &mut bnd, &mut out);
            assert_eq!(bnd[g] as usize, r.len());
            let mut home = std::collections::HashMap::new();
            for k in 0..g {
                let part = &out[bnd[k] as usize..bnd[k + 1] as usize];
                assert!(part_len(op, part.len() / 24, 0, HDR) <= cap / 2);
                for rec in part.chunks(24) {
                    let key = i64::from_ne_bytes(rec[..8].try_into().unwrap());
                    assert_eq!(fine(raw_hash(op, rec.as_ptr()), 6) >> (FINE_BITS - s), k);
                    assert_eq!(fine(KeyHash::of(&[key as u64]).to_abi(), 6) >> (FINE_BITS - s), k);
                    assert_eq!(*home.entry(key).or_insert(k), k);
                }
            }
            assert_eq!(home.len(), 1000);
            // the probe's kept rows (one cell, the key) group by the KeyHash `exit` writes, into their keys' groups
            let mut kept = Vec::new();
            for key in (0..1200i64).rev() {
                kept.extend(crate::ops::hashjoin::spill::load::kept_row(&[0, key as u64, 0], KeyHash::of(&[key as u64]).to_abi()));
            }
            let (mut kb, mut ko) = (vec![0u64; g + 1], vec![0u64; 1200]);
            kept_group_into(kept.as_ptr(), kept.len(), 6, s, &mut kb, &mut ko);
            assert_eq!(kb[g], 1200);
            for k in 0..g {
                for &o in &ko[kb[k] as usize..kb[k + 1] as usize] {
                    let key = rd::<u64>(kept.as_ptr(), o as usize + 16) as i64;
                    assert!(!matches!(home.get(&key), Some(&h) if h != k), "key {key}");
                }
            }
        }
    }


    /// D2715: the drain loads a part past its cap in chunks of whole records, each within half the cap, covering the part;
    /// a part far under its cap loads whole.
    #[test]
    fn a_drain_part_past_its_cap_loads_in_chunks() {
        let o = orig();
        let op = o.as_ptr() as *const u8;
        let r = raw(&(0..3000).collect::<Vec<i64>>());
        // SAFETY: `o` is a build header, `r` its serialized records.
        unsafe {
            assert_eq!(chunk_of(op, &r, 0, usize::MAX >> 1).unwrap(), r.len());
            // 64 kB: a chunk of a few hundred records fits half of it, its directory included
            let cap = 64 << 10;
            let (mut from, mut chunks) = (0, 0);
            while from < r.len() {
                let to = chunk_of(op, &r, from, cap).unwrap();
                assert!(to > from && (to - from) % 24 == 0, "{from}..{to}");
                assert!((to - from) / 24 > 1 && part_len(op, (to - from) / 24, 0, HDR) <= cap / 2, "{from}..{to}");
                (from, chunks) = (to, chunks + 1);
            }
            assert!(chunks > 1, "{chunks}");
            // 16 kB: one record's minimum directory (8 << BUILD_MIN_CAP_BITS) is past half the cap, so every chunk
            // holds one record, the least a chunk takes
            let (mut from, mut chunks) = (0, 0);
            while from < r.len() {
                let to = chunk_of(op, &r, from, 16 << 10).unwrap();
                assert_eq!(to - from, 24, "{from}..{to}");
                (from, chunks) = (to, chunks + 1);
            }
            assert_eq!(chunks, 3000);
        }
    }
}
