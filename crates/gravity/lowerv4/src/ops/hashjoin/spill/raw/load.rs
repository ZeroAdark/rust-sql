//! The runtime of the pair pipeline of a spilled join.inner build (rule R6: under spill/raw/; `spill::load` re-exports this
//! module, so its paths are unchanged). The hosts: `kept_put` (the probe's kept rows), `pair_open` / `pair_next` (each
//! partition's reload into a region of its own, laid out by ops/hashjoin/raw.rs `lay_out`) and their helpers.
//!
//! A kept row in a file: `[bytes u32 | mix u32]`, then the NULL bitmap words, then one word per cell. `mix` is the top half
//! of the row's remixed KeyHash (spill/build.rs `remix`): its partition's bits and the split bits below them (D2715: a
//! split partition hands each kept row on in its sub-partition, spill/raw.rs `fine`). A by-value cell's word is its datum.
//! A by-reference cell's word is the offset of its image from the row's start. The images follow, each padded to 8 bytes.
//!
//! SAFETY (parallel pair claims, W >= 2): each owner opens its own claims. It reads the stored files, which the build's
//! and the probe's owners wrote and flushed before their sections ended (`flush` at each claim's close; `v4::open`
//! refuses a stream its caller still stages). Each owner writes only its own `Partials` and its own regions; the shared
//! `more` table is never written, the owner's copy of it is (in its pair region's header).
#![allow(unsafe_code)]
use gravity_abi::PtrWord;
use gravity_abi::ctx::{Ctx, Partials};
use gravity_driver::mem::spill_host::v4;
use gravity_driver::Error;
use gravity_fn::arena::OpArena;

use crate::ops::hashjoin::build::{region, END, FLAGS, F_FP128, F_RANGE, F_RANGE0, HI, LIMIT, LO, LOW, N, NULLKEY, PHASE, REC, TOP, WORDS};
use crate::ops::hashjoin::layout;
use crate::ops::hashjoin::rt::{lay_out, rd, tail, wr, Rt};
use crate::ops::hashjoin::spill::build::{put_record, serialized_len, PBITS, ROW_STACK, SPILL, SPILLED};
use crate::ops::hashjoin::drain::spill::write_drain;
use crate::ops::hashjoin::spill::pair::{DRAIN, FILL, FIRST, MAGIC, META, M_BND, M_DP, M_GRP, M_HI, M_KBND, M_KEPT, M_KOFF, M_MORE, M_NKEPT, M_OFFS, M_ORIG, M_PART, M_SBITS, M_SUB};
use crate::ops::hashjoin::spill::raw as split;

/// The partition bits of the hosts' `bits` argument (its flags above them: `FILL`, `DRAIN`, `DRAIN_MARKED`).
pub(crate) const fn arg_bits(arg: u64) -> u8 {
    (arg & 0xff) as u8
}

// ---- the hosts ----

gravity_abi::host! {
    /// `exit`'s host: the `n` cells at `cells` (`[kind, datum, null]` each) and the row's KeyHash `h` written as one kept row
    /// to part `part` of the owner's file of `stream`, laid out on the stack unless it is past `ROW_STACK`. 0, or -1 with the
    /// error held.
    pub(crate) unsafe fn kept_put(ctx: *mut Ctx, stream: u64, bits: u64, part: u64, h: u64, cells: *const u64, n: u64) -> i64 {
        let cells = core::slice::from_raw_parts(cells, 3 * n as usize);
        let len = split::kept_len(cells);
        let (mut stack, mut heap) = ([0u8; ROW_STACK], Vec::new());
        let row = match len <= ROW_STACK {
            true => &mut stack[..len],
            false => {
                heap.resize(len, 0); // alloc: Exec
                &mut heap[..]
            }
        };
        split::kept_fill(cells, h, row);
        match v4::put(ctx, stream, bits as u8, part as u32, row) {
            v4::FAILED => -1,
            _ => 0,
        }
    }
}

/// The kept row of `cells` and KeyHash `h` in a buffer of its own (the tests').
#[cfg(test)]
pub(crate) unsafe fn kept_row(cells: &[u64], h: u64) -> Vec<u8> {
    let mut row = vec![0u8; split::kept_len(cells)];
    split::kept_fill(cells, h, &mut row);
    row
}

/// A reload buffer drawn by `v4::take_reload`, doubling (the old one given back).
pub(crate) struct Buf {
    ctx: *mut Ctx,
    pub(crate) p: *mut u8,
    cap: usize,
    pub(crate) len: usize,
}

impl Buf {
    pub(crate) unsafe fn new(ctx: *mut Ctx, cap: usize) -> gravity_driver::Result<Buf> {
        let p = v4::take_reload(ctx, cap);
        if p.is_null() {
            return Err(Error::host("gravity: a spill pair's reload buffer was refused"));
        }
        Ok(Buf { ctx, p, cap, len: 0 })
    }

    unsafe fn reserve(&mut self, more: usize) -> gravity_driver::Result<()> {
        if self.len + more <= self.cap {
            return Ok(());
        }
        let n = Buf::new(self.ctx, (2 * self.cap).max(self.len + more))?;
        core::ptr::copy_nonoverlapping(self.p, n.p, self.len);
        v4::give(self.ctx, self.p);
        (self.p, self.cap) = (n.p, n.cap);
        Ok(())
    }

    /// Every owner's part `part` of `stream` appended.
    unsafe fn read(&mut self, stream: u64, part: u32) -> gravity_driver::Result<()> {
        for o in 0..owners(self.ctx, stream)? {
            self.read_owner(stream, o, part)?;
        }
        Ok(())
    }

    /// Owner `o`'s part `part` of `stream` appended.
    pub(crate) unsafe fn read_owner(&mut self, stream: u64, o: usize, part: u32) -> gravity_driver::Result<()> {
        let h = match v4::open(self.ctx, stream, o, part) {
            v4::FAILED => return Err(Error::host("gravity: a spill pair's part did not open")),
            0 => return Ok(()),
            h => h as usize - 1,
        };
        loop {
            self.reserve(FIRST)?;
            let got = v4::read(self.ctx, h, core::slice::from_raw_parts_mut(self.p.add(self.len), self.cap - self.len));
            match got {
                v4::FAILED => return Err(Error::host("gravity: a spill pair's part read failed")),
                0 => return Ok(()),
                g => self.len += g as usize,
            }
        }
    }
}

pub(crate) unsafe fn owners(ctx: *mut Ctx, stream: u64) -> gravity_driver::Result<usize> {
    match v4::owners(ctx, stream) {
        o if o < 0 => Err(Error::host("gravity: a spill pair's stream has no owners")),
        o => Ok(o as usize),
    }
}

unsafe fn consumed_at(pp: *mut Partials, i: usize) -> *mut u8 {
    if i < 2 { (*pp).consumed[i].ptr() } else { (*(*pp).more.ptr().add(i)).ptr() }
}

/// The pair region's header whose build region is `cur`, when `cur` is one (this owner took it and it carries `MAGIC`).
unsafe fn meta_of(ctx: *mut Ctx, cur: *mut u8) -> Option<*mut u8> {
    let m = cur.wrapping_sub(META);
    (v4::taken(ctx, m) && rd::<u64>(m, 0) == MAGIC).then_some(m)
}

/// The probe's build pointed back at the original region and the pair's regions given back: the region and its kept
/// rows' offsets (a D2715 split's are its partition's), and with `all` (the partition's end) the kept rows and the split's
/// groups and drain records. At its end a drained build's partition first writes its drain part (`write_drain`; a split
/// one's gathered, `split::drain_end`). 0, or `FAILED` with the error held.
pub(crate) unsafe fn close(ctx: *mut Ctx, pp: *mut Partials, i: usize, m: *mut u8, stream: u64, arg: u64, all: bool) -> i64 {
    let split = rd::<u64>(m, M_SBITS) != 0;
    let st = match all && arg & DRAIN != 0 {
        true if split => split::drain_end(ctx, stream, arg, m),
        true if rd::<u64>(m, M_NKEPT) > 0 => write_drain(ctx, stream, arg, m),
        _ => 0,
    };
    if i < 2 {
        (*pp).consumed[i] = gravity_abi::word(rd::<u64>(m, M_ORIG) as usize as *mut u8);
    }
    (*pp).more = gravity_abi::word(rd::<u64>(m, M_MORE) as usize as *const gravity_abi::Word<*mut u8>);
    for (off, give) in [(M_OFFS, !split), (M_KEPT, all), (M_GRP, all), (M_BND, all), (M_KOFF, all), (M_KBND, all), (M_DP, all)] {
        let p = rd::<u64>(m, off) as usize as *mut u8;
        if give && !p.is_null() {
            v4::give(ctx, p);
        }
    }
    wr::<u64>(m, 0, 0);
    v4::give(ctx, m);
    st
}

/// The serialized records of `raw` under header `orig`: (count, their image bytes).
pub(crate) unsafe fn records(orig: *const u8, raw: &[u8]) -> gravity_driver::Result<(usize, usize)> {
    let rec = rd::<u32>(orig, REC) as usize;
    let (mut n, mut imgs, mut o) = (0usize, 0usize, 0usize);
    while o < raw.len() {
        let l = if raw.len() - o < rec { usize::MAX } else { serialized_len(orig, raw.as_ptr().add(o)) };
        if l > raw.len() - o {
            return Err(Error::host("gravity: a spill pair's build part ends inside a record"));
        }
        (n, imgs, o) = (n + 1, imgs + (l - rec), o + l);
    }
    Ok((n, imgs))
}

/// The bytes a partition region of `n` records (from `base`) with `imgs` image bytes needs before its directory is sized.
pub(crate) fn part_len(orig: *const u8, n: usize, imgs: usize, base: usize) -> usize {
    // SAFETY: `orig` is a laid-out build region's header (the caller's).
    let rec = unsafe { rd::<u32>(orig, REC) } as usize;
    (base + n * rec + imgs + (8 << crate::ops::hashjoin::rt::dir_bits(n as u64)) + 64).next_multiple_of(64)
}

/// Region `d` (`len` bytes) headed as the original build `orig` with `raw`'s `n` records put in, before its lay-out. The
/// original's ALLOW and record shape stay (lay_out splits a keeping build's NULL-key records and lays the marks of a
/// marking one as the original's did); its lay-out words and flags but F_RANGE, F_RANGE0 and F_FP128 are cleared.
unsafe fn fill_part(orig: *const u8, raw: &[u8], n: usize, d: *mut u8, len: usize) {
    let (rec, base) = (rd::<u32>(orig, REC) as usize, layout::base(layout::pay_of(orig)));
    core::ptr::copy_nonoverlapping(orig, d, base);
    for off in [N, crate::ops::hashjoin::build::DIR, crate::ops::hashjoin::build::SLOTS, crate::ops::hashjoin::build::KEPT, crate::ops::hashjoin::build::MARKS] {
        wr::<u64>(d, off, 0);
    }
    (wr::<u8>(d, PHASE, 1), wr::<u8>(d, crate::ops::hashjoin::build::FORM, 0), wr::<u32>(d, crate::ops::hashjoin::build::BITS, 0));
    (wr::<u8>(d, FLAGS, rd::<u8>(orig, FLAGS) & (F_RANGE | F_RANGE0 | F_FP128)), wr::<u8>(d, SPILL, 0), wr::<u8>(d, PBITS, 0), wr::<u64>(d, SPILLED, (n == 0) as u64));
    (wr::<u64>(d, TOP, len as u64), wr::<u64>(d, END, len as u64), wr::<u32>(d, LIMIT, len as u32));
    let (range, mut lo, mut hi) = (rd::<u8>(orig, FLAGS) & (F_RANGE | F_RANGE0) != 0, i64::MAX, i64::MIN);
    let mut o = 0;
    for k in 0..n {
        o += put_record(d, &raw[o..]);
        // the build's range leaves a NULL-key record out (build.rs: a NULL row keeps lo / hi), and so does the partition's
        if range && rd::<u64>(d, base + k * rec + 8 * rd::<u8>(orig, WORDS) as usize) & LOW != NULLKEY {
            let w = rd::<i64>(d, base + k * rec);
            (lo, hi) = (lo.min(w), hi.max(w));
        }
    }
    (wr::<i64>(d, LO, lo), wr::<i64>(d, HI, hi));
}

/// A laid-out partition region of `raw`'s records under `orig`, `head` bytes into a region of its own (`take_pair`):
/// (the region, its build region, its length, its records). Its directory grows the region when the records leave it no
/// room.
pub(crate) unsafe fn part_region(ctx: *mut Ctx, head: usize, orig: *const u8, raw: &[u8]) -> gravity_driver::Result<(*mut u8, *mut u8, usize, usize)> {
    let (n, imgs) = records(orig, raw)?;
    let len = part_len(orig, n, imgs, layout::base(layout::pay_of(orig)));
    let (mut m, mut len) = (take_pair(ctx, head, len)?, len);
    let d = m.add(head);
    fill_part(orig, raw, n, d, len);
    let t = Rt::of(d, len);
    let need = t.dir() + crate::ops::hashjoin::rt::dir_need(&t) + tail(d);
    if need > len {
        let len2 = need.next_multiple_of(64);
        let m2 = take_pair(ctx, head, len2)?;
        let (d2, top, tl) = (m2.add(head), layout::base(layout::pay_of(orig)) + n * rd::<u32>(orig, REC) as usize, tail(d));
        core::ptr::copy_nonoverlapping(d, d2, top);
        core::ptr::copy_nonoverlapping(d.add(len - tl), d2.add(len2 - tl), tl);
        (wr::<u64>(d2, TOP, (len2 - tl) as u64), wr::<u64>(d2, END, len2 as u64), wr::<u32>(d2, LIMIT, len2 as u32));
        v4::give(ctx, m);
        (m, len) = (m2, len2);
    }
    let d = m.add(head);
    if let Err(e) = lay_out(d, len) {
        v4::give(ctx, m);
        return Err(e);
    }
    Ok((m, d, len, n))
}

gravity_abi::host! {
    /// `next_window`'s host past a partition's last kept row: the claim's next partition opened (1), or, past the claim's
    /// last, the pair closed (0); -1 with the error held.
    pub(crate) unsafe fn pair_next(ctx: *mut Ctx, stream: u64, consumed: u64, bits: u64, ncons: u64) -> i64 {
        let (pp, i) = ((*(*ctx).program.ptr()).partial.ptr(), consumed as usize);
        let Some(m) = meta_of(ctx, consumed_at(pp, i)) else { return 0 };
        if rd::<u64>(m, M_SUB) + 1 < 1u64 << rd::<u64>(m, M_SBITS) {
            return match crate::ops::hashjoin::spill::raw::next_sub(ctx, pp, i, m, (stream, bits), ncons as usize) {
                Ok(()) => 1,
                Err(Some(e)) => v4::fail(ctx, e),
                Err(None) => v4::FAILED,
            };
        }
        let (p, hi) = (rd::<u64>(m, M_PART) + 1, rd::<u64>(m, M_HI));
        if p >= hi {
            return close(ctx, pp, i, m, stream, bits, true);
        }
        match open_pair(ctx, stream, i, bits, ncons as usize, p as u32, hi) {
            Ok(()) => 1,
            Err(Some(e)) => v4::fail(ctx, e),
            Err(None) => v4::FAILED,
        }
    }
}

gravity_abi::host! {
    /// `open`'s host: the claim's first partition opened (`open_pair`); the claim's partitions are `lo..hi` of the stream's
    /// `2^bits` (a serial run's claim is all of them). 0; 1 when the build never spilled (no pair); -1 with the error held.
    pub(crate) unsafe fn pair_open(ctx: *mut Ctx, stream: u64, consumed: u64, bits: u64, ncons: u64) -> i64 {
        let (pp, i) = ((*(*ctx).program.ptr()).partial.ptr(), consumed as usize);
        if let Some(m) = meta_of(ctx, consumed_at(pp, i)) {
            if close(ctx, pp, i, m, stream, bits, true) < 0 {
                return v4::FAILED;
            }
        }
        if rd::<u64>(consumed_at(pp, i), SPILLED) == 0 {
            return 1;
        }
        let (lo, hi) = ((*ctx).claim.lo, (*ctx).claim.hi.min(1 << arg_bits(bits)));
        match open_pair(ctx, stream, i, bits, ncons as usize, lo.min(hi) as u32, hi) {
            Ok(()) => 0,
            Err(Some(e)) => v4::fail(ctx, e),
            Err(None) => v4::FAILED,
        }
    }
}

/// Partition `p` (of the claim's `..end`) of the probe's build `i`: its records from every owner's file laid out in a region
/// of their own, headed as the original build (`M_ORIG`), and its kept rows with their offsets. The probe's build `i` and
/// the owner's `more` table then point at it; a pair still open (an unfinished claim, the previous partition) is closed
/// first. `p == end` (a claim past the partitions) opens an empty pair. `Err(None)`: an error already held (`close`).
unsafe fn open_pair(ctx: *mut Ctx, stream: u64, i: usize, arg: u64, ncons: usize, p: u32, end: u64) -> Result<(), Option<Error>> {
    let (bits, fill) = (arg_bits(arg), arg & FILL != 0);
    let pp = (*(*ctx).program.ptr()).partial.ptr();
    let mut cur = consumed_at(pp, i);
    if let Some(m) = meta_of(ctx, cur) {
        if close(ctx, pp, i, m, stream, arg, true) < 0 {
            return Err(None);
        }
        cur = consumed_at(pp, i);
    }
    let orig = cur as *const u8;
    let mut kept = Buf::new(ctx, FIRST)?;
    if (p as u64) < end {
        kept.read(stream, (1u32 << bits) + p)?;
    }
    let (mut rows, mut o) = (0usize, 0usize);
    while o < kept.len {
        (rows, o) = (rows + 1, o + rd::<u32>(kept.p, o) as usize);
    }
    if o != kept.len || rows > u32::MAX as usize {
        return Err(Some(Error::host("gravity: a spill pair's kept part ends inside a row")));
    }
    // a partition no probe row reached needs no build region: an empty one (EMPTY ends the probe)
    let mut raw = Buf::new(ctx, if rows == 0 { 8 } else { FIRST })?;
    if rows > 0 {
        raw.read(stream, p)?;
    }
    let bytes = core::slice::from_raw_parts(raw.p, raw.len);
    // D2715: a partition whose region would pass its length joins in sub-passes
    let sbits = if rows > 0 { split::split_bits(orig, bytes, bits, split::part_cap(), OpArena::slot_owner_kind(ctx)) } else { Ok(0) };
    if let Ok(s @ 1..) = sbits {
        let grouped = split::group(ctx, orig, bytes, bits, s);
        v4::give(ctx, raw.p);
        let (grp, bnd) = grouped?;
        let (koff, kbnd) = split::kept_groups(ctx, (kept.p, kept.len, rows), bits, s)?;
        return split::open_sub(ctx, pp, i, arg, ncons, orig, kept.p, (p, end), split::Split::of(0, s, [grp.p, bnd.p, koff.p, kbnd.p]));
    }
    let built = sbits.and_then(|_| part_region(ctx, META, orig, bytes));
    v4::give(ctx, raw.p);
    let (m, d, _, n) = built?;
    // no build record: the kept rows match nothing. INNER and SEMI hand none on; LEFT and ANTI hand them all on, each to
    // its miss. The region is an empty HASH (`SPILLED` set, build_rt.rs `forms`), never EMPTY, whose window would end the
    // pipeline (D237) before the claim's later partitions
    let rows = if n == 0 && !fill { 0 } else { rows };
    let offs = Buf::new(ctx, (8 * rows).max(8))?;
    let mut o = 0;
    for r in 0..rows {
        wr::<u64>(offs.p, 8 * r, o as u64);
        o += rd::<u32>(kept.p, o) as usize;
    }
    split::install(pp, i, ncons, (m, d), orig, (kept.p, offs.p, rows), (p, end), split::Split::NONE);
    Ok(())
}

/// A region of `len` bytes behind `head` bytes of its own header, or the pool's refusal (held by `take`).
unsafe fn take_pair(ctx: *mut Ctx, head: usize, len: usize) -> gravity_driver::Result<*mut u8> {
    if len > u32::MAX as usize {
        return Err(Error::host("gravity: a spilled build partition past 4 GiB"));
    }
    let m = v4::take_reload(ctx, head + len);
    if m.is_null() {
        return Err(Error::host("gravity: a spill pair's region was refused"));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::hashjoin::build::D_VARLENA;
    use crate::ops::hashjoin::rt::varlena_len;

    /// A kept row's cells in order: a word, a NULL, a varlena image at its offset from the row's start.
    #[test]
    fn a_kept_row_holds_words_nulls_and_images_by_offset() {
        let text = [(7u32 << 2).to_le_bytes().as_slice(), b"abc"].concat();
        let cells = [0u64, 42, 0, 0, 0, 1, D_VARLENA as u64, text.as_ptr() as u64, 0];
        // SAFETY: the varlena cell's datum is `text`, a 4-byte-header varlena of 7 bytes.
        let row = unsafe { kept_row(&cells, 0x1234) };
        let fixed = 8 + 8 + 8 * 3;
        // SAFETY: offsets inside `row`.
        unsafe {
            assert_eq!(rd::<u32>(row.as_ptr(), 0) as usize, row.len());
            assert_eq!(rd::<u64>(row.as_ptr(), 16), 42);
            assert_eq!(rd::<u64>(row.as_ptr(), 8), 0b010);
            let off = rd::<u64>(row.as_ptr(), 32) as usize;
            assert_eq!((off, &row[off..off + 7]), (fixed, &text[..]));
            assert_eq!(split::kept_mix(row.as_ptr()), crate::ops::hashjoin::spill::build::remix(0x1234) & !0xFFFF_FFFF);
        }
        assert_eq!(row.len() % 8, 0);
    }

    use crate::ops::hashjoin::build::{ALLOW, A_KEEP, A_MARKS, A_TAGS, DIR, FORM, F_SPLIT, HASH, HDR, KEPT, MARKS};
    use crate::ops::hashjoin::drain::spill::{drain_records, drain_set, u_part, MAGIC_U};
    use crate::ops::hashjoin::spill::pair::DRAIN_MARKED;

    const LEN: usize = 1 << 16;

    /// The original build's header (build_rt.rs's test region): one image word, one payload word (the row's number), under
    /// `allow`, no record yet (a partition region takes only its header).
    fn orig(allow: u8) -> Vec<u64> {
        let mut buf = vec![0u64; HDR / 8 + 1];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` covers the header.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, 99), wr::<u64>(d, KEPT, 7), wr::<u64>(d, MARKS, 4096), wr::<u64>(d, DIR, 4096));
        }
        buf
    }

    /// `keys` serialized as a build part holds them (None: a NULL-key record), the row's number in its payload word.
    fn raw(keys: &[Option<i64>]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            out.extend(k.unwrap_or(0).to_ne_bytes());
            out.extend((if k.is_none() { NULLKEY } else { 0 }).to_ne_bytes());
            out.extend((i as u64).to_ne_bytes());
        }
        out
    }

    /// The rows' numbers of serialized records `raw`, sorted.
    fn numbers(raw: &[u8]) -> Vec<u64> {
        let mut v: Vec<u64> = raw.chunks(24).map(|r| u64::from_ne_bytes(r[16..24].try_into().unwrap())).collect();
        v.sort_unstable();
        v
    }

    /// `keys` put in a partition region under `orig` and laid out, as `part_region` does (without its host).
    unsafe fn laid(o: *const u8, keys: &[Option<i64>]) -> Vec<u64> {
        let r = raw(keys);
        let (n, imgs) = records(o, &r).unwrap();
        assert_eq!((n, imgs), (keys.len(), 0));
        assert!(part_len(o, n, imgs, HDR) <= LEN);
        let mut buf = vec![0xABAB_ABAB_ABAB_ABABu64; LEN / 8];
        let d = buf.as_mut_ptr() as *mut u8;
        fill_part(o, &r, n, d, LEN);
        lay_out(d, LEN).unwrap();
        buf
    }

    /// A partition region inherits its build's ALLOW, so a RIGHT / FULL / RIGHT ANTI partition lays out as the build
    /// did: its NULL-key records split past the keyed ones, one zeroed mark per record before the directory. The original's
    /// lay-out words never leak in.
    #[test]
    fn a_partition_region_splits_and_marks_as_its_build() {
        let keys = [Some(5), None, Some(7), None, None, Some(5), Some(9), None];
        let o = orig(A_TAGS | A_KEEP | A_MARKS);
        // SAFETY: the header above and the region `laid` returns, ours alone.
        unsafe {
            let mut buf = laid(o.as_ptr() as *const u8, &keys);
            let d = buf.as_mut_ptr() as *mut u8;
            let t = Rt::of(d, LEN);
            assert_eq!((t.n, t.kept, rd::<u8>(d, FLAGS) & F_SPLIT, rd::<u8>(d, FORM), rd::<u8>(d, ALLOW)), (4, 4, F_SPLIT, HASH, A_TAGS | A_KEEP | A_MARKS));
            assert_eq!((rd::<i64>(d, LO), rd::<i64>(d, HI)), (5, 9), "the range of the keyed records, as the build's (a NULL-key record's word 0 left out)");
            let m = rd::<u64>(d, MARKS) as usize;
            assert!(m == t.marks_at() && m >= HDR + 8 * 24 && (m..m + 8).all(|o| *d.add(o) == 0), "one zeroed mark per record");
            assert!(rd::<u64>(d, DIR) as usize >= m + 8, "the directory follows the marks");
            let rec = |i: usize| rd::<u64>(t.rec(i), 16);
            let mut keyed: Vec<u64> = (0..4).map(rec).collect();
            keyed.sort_unstable();
            assert_eq!(keyed, vec![0, 2, 5, 6], "the keyed records first (the split moves, it keeps no order)");
            let mut nulls: Vec<u64> = (4..8).map(rec).collect();
            nulls.sort_unstable();
            assert_eq!(nulls, vec![1, 3, 4, 7]);
        }
        // an INNER build's partition keeps no NULL key apart and lays no marks
        let o = orig(A_TAGS);
        // SAFETY: as above.
        unsafe {
            let mut buf = laid(o.as_ptr() as *const u8, &[Some(1), Some(2)]);
            let t = Rt::of(buf.as_mut_ptr() as *mut u8, LEN);
            assert_eq!((t.n, t.kept, t.marks), (2, 0, false));
        }
    }

    /// A drained partition's part holds the records whose mark is the variant's (unmarked: RIGHT / FULL / RIGHT ANTI;
    /// marked: RIGHT SEMI), keyed and NULL-key, behind its header; `drain_records` reads back exactly those.
    #[test]
    fn a_drain_part_holds_the_records_of_its_mark_and_reads_back_whole() {
        let keys = [Some(5), None, Some(7), None, Some(5), Some(9)];
        let o = orig(A_TAGS | A_KEEP | A_MARKS);
        let op = o.as_ptr() as *const u8;
        // SAFETY: the header above and the region `laid` returns, ours alone.
        unsafe {
            let mut buf = laid(op, &keys);
            let d = buf.as_mut_ptr() as *mut u8;
            let t = Rt::of(d, LEN);
            // the probe marked keyed record 1 and NULL-key record 4 (by position after the split)
            for i in [1, 4] {
                *d.add(t.marks_at() + i) = 1;
            }
            let row = |i: usize| rd::<u64>(t.rec(i), 16);
            let (mut sent, mut kept): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
            (0..6).for_each(|i| if i == 1 || i == 4 { kept.push(row(i)) } else { sent.push(row(i)) });
            (sent.sort_unstable(), kept.sort_unstable());
            for (marked, want) in [(false, sent), (true, kept)] {
                let part = drain_set(d, marked);
                assert_eq!((&part[..4], u32::from_le_bytes(part[4..8].try_into().unwrap()) as usize), (&MAGIC_U[..], want.len()));
                let body = drain_records(op, &[&[], &part, &[]]).unwrap().expect("one owner wrote it");
                assert_eq!(numbers(body), want, "marked {marked}");
            }
            // a partition nothing reached still writes its header: zero records, not None
            let mut buf = laid(op, &[Some(1)]);
            *buf.as_mut_ptr().cast::<u8>().add(Rt::of(buf.as_mut_ptr() as *mut u8, LEN).marks_at()) = 1;
            let part = drain_set(buf.as_mut_ptr() as *mut u8, false);
            assert_eq!(drain_records(op, &[&part]).unwrap().map(<[u8]>::len), Some(0));
        }
    }

    /// A drained record's image (a by-distance payload: an 8-byte fixed image, then a varlena) goes out as a build
    /// part holds it, though a laid-out region holds its image words as offsets from its base (`settle`): the part loads
    /// back to the same bytes, and the region keeps its own words (the drain walks it again).
    #[test]
    fn a_drain_part_carries_its_records_images() {
        use crate::ops::hashjoin::build::DESC;
        // (payload kind, each record's image bytes as a file holds them: its length word, then the bytes padded to 8)
        let fixed = |i: u64| (0xB00 + i).to_ne_bytes().to_vec();
        let text = |i: u64| {
            let body = format!("boo{}", "o".repeat(i as usize * 5));
            [((4 + body.len()) as u32).wrapping_shl(2).to_le_bytes().as_slice(), body.as_bytes()].concat()
        };
        for (kind, img) in [(8u16, &fixed as &dyn Fn(u64) -> Vec<u8>), (D_VARLENA, &text)] {
            let mut o = orig(A_TAGS | A_KEEP | A_MARKS);
            let op = o.as_mut_ptr() as *mut u8;
            let (mut r, mut want) = (Vec::new(), Vec::new());
            for (i, k) in [Some(5i64), None, Some(7)].iter().enumerate() {
                let b = img(i as u64);
                r.extend(k.unwrap_or(0).to_ne_bytes());
                r.extend((if k.is_none() { NULLKEY } else { 0 }).to_ne_bytes());
                r.extend((i as u64).to_ne_bytes());
                r.extend((b.len() as u64).to_ne_bytes());
                r.extend(&b);
                r.resize(r.len().next_multiple_of(8), 0);
                want.push((i as u64, b));
            }
            // SAFETY: the header above (payload 1 an image of `kind`) and the regions below, ours alone.
            unsafe {
                (crate::ops::hashjoin::layout::set_pay(op, 2), wr::<u32>(op, REC, 32), wr::<u16>(op, DESC + 2, kind));
                let lay = |raw: &[u8]| {
                    let (n, _) = records(op, raw).unwrap();
                    assert_eq!(n, 3);
                    let mut buf = vec![0u64; LEN / 8];
                    let d = buf.as_mut_ptr() as *mut u8;
                    fill_part(op, raw, n, d, LEN);
                    lay_out(d, LEN).unwrap();
                    buf
                };
                let images = |d: *mut u8| {
                    let t = Rt::of(d, LEN);
                    let mut v: Vec<(u64, Vec<u8>)> = (0..3)
                        .map(|i| {
                            let p = d.add(rd::<u64>(t.rec(i), 24) as usize);
                            let len = if kind == D_VARLENA { varlena_len(p) } else { kind as usize };
                            (rd::<u64>(t.rec(i), 16), core::slice::from_raw_parts(p, len).to_vec())
                        })
                        .collect();
                    v.sort_unstable();
                    v
                };
                let mut buf = lay(&r);
                let d = buf.as_mut_ptr() as *mut u8;
                assert_eq!(images(d), want, "kind {kind}: a laid-out region reads its images at base + word");
                let part = drain_set(d, false);
                assert_eq!(images(d), want, "kind {kind}: the region keeps its settled words");
                let body = drain_records(op, &[&part]).unwrap().expect("one owner wrote it");
                let mut back = lay(body);
                assert_eq!(images(back.as_mut_ptr() as *mut u8), want, "kind {kind}: the drain part loads back to the same images");
            }
        }
    }

    /// D4431 / CONVERT-WAVE1: a drain hands on its part (`drain_pieces`, joined in `drain_set`) as its header, then the
    /// bytes `serialize_over` writes for each sent record, in record order (two image payloads: a fixed 8-byte one and a
    /// varlena, the varlena NULL in some records, one record past the batch: its `serialize_each` pieces), each put at most
    /// `JOIN_SPILL_EPOCH` bytes; `drain_bytes` counts the records' bytes, and the region is left as it was.
    #[test]
    fn a_drain_hands_on_each_record_as_serialize_over_writes_it() {
        use crate::ops::hashjoin::build::DESC;
        use crate::ops::hashjoin::drain::raw::drain_pieces;
        use crate::ops::hashjoin::drain::spill::{drain_bytes, drain_each, drain_head};
        use crate::ops::hashjoin::rt::settle;
        use gravity_program::cost::JOIN_SPILL_EPOCH;
        let mut o = orig(A_TAGS | A_KEEP | A_MARKS);
        let op = o.as_mut_ptr() as *mut u8;
        let keys = [Some(5i64), None, Some(7), Some(9), None, Some(11), Some(13)];
        let mut r = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            let fixed = (0xC00 + i as u64).to_ne_bytes();
            // record 6's text passes the drain's 8 KiB batch (`serialize_each`'s pieces)
            let body = format!("t{}", "x".repeat(if i == 6 { 9000 } else { i * 7 }));
            let text = [((4 + body.len()) as u32).wrapping_shl(2).to_le_bytes().as_slice(), body.as_bytes()].concat();
            // payload 2 (the varlena) NULL in every third record: link bit 32 + 2
            let null = i % 3 == 1;
            r.extend(k.unwrap_or(0).to_ne_bytes());
            r.extend(((if k.is_none() { NULLKEY } else { 0 }) | if null { 1 << 34 } else { 0 }).to_ne_bytes());
            r.extend((i as u64).to_ne_bytes());
            r.extend(8u64.to_ne_bytes());
            r.extend((if null { 0 } else { text.len() as u64 }).to_ne_bytes());
            r.extend(fixed);
            if !null {
                r.extend(&text);
                r.resize(r.len().next_multiple_of(8), 0);
            }
        }
        // SAFETY: the header above (payload 1 a fixed 8-byte image, payload 2 a varlena) and the region below, ours alone.
        unsafe {
            (crate::ops::hashjoin::layout::set_pay(op, 3), wr::<u32>(op, REC, 40), wr::<u16>(op, DESC + 2, 8), wr::<u16>(op, DESC + 4, D_VARLENA));
            let (n, _) = records(op, &r).unwrap();
            assert_eq!(n, keys.len());
            let mut buf = vec![0u64; LEN / 8];
            let d = buf.as_mut_ptr() as *mut u8;
            fill_part(op, &r, n, d, LEN);
            lay_out(d, LEN).unwrap();
            let t = Rt::of(d, LEN);
            for i in [1, 4, 5] {
                *d.add(t.marks_at() + i) = 1;
            }
            let end = rd::<u64>(d, crate::ops::hashjoin::build::END) as usize;
            for marked in [false, true] {
                let mut want = Vec::new();
                let mut sent = 0u32;
                for i in (0..t.n + t.kept).filter(|&i| (*d.add(t.marks_at() + i) != 0) == marked) {
                    settle(&t, i..i + 1, end);
                    let mut one = vec![0xEEu8; crate::ops::hashjoin::spill::raw::file_len(&t, i)];
                    crate::ops::hashjoin::spill::raw::serialize_over(&t, i, &mut one);
                    settle(&t, i..i + 1, end);
                    want.extend(one);
                    sent += 1;
                }
                let before = buf.clone();
                let d = buf.as_mut_ptr() as *mut u8;
                let (mut got, mut puts) = (Vec::new(), Vec::new());
                drain_pieces(d, marked, &mut |b| -> Result<(), ()> {
                    (got.extend_from_slice(b), puts.push(b.len()));
                    Ok(())
                })
                .unwrap();
                assert_eq!(got, [&drain_head(sent)[..], &want].concat(), "marked {marked}");
                assert!(puts.iter().all(|&p| p <= JOIN_SPILL_EPOCH), "marked {marked}: puts {puts:?}");
                // each record's `serialize_each` pieces are its `serialize_over` bytes
                let mut each = Vec::new();
                drain_each(d, marked, &mut |t, i| {
                    crate::ops::hashjoin::spill::raw::serialize_each(t, i, &mut |b| -> Result<(), ()> {
                        each.extend_from_slice(b);
                        Ok(())
                    })
                })
                .unwrap();
                assert_eq!(each, want, "marked {marked}");
                assert_eq!(drain_bytes(d, marked), want.len(), "marked {marked}");
                assert_eq!(drain_set(d, marked), got, "marked {marked}");
                assert!(buf == before, "marked {marked}: the region's words are settled back");
                let body = drain_records(op, &[&drain_set(buf.as_mut_ptr() as *mut u8, marked)]).unwrap().expect("one owner wrote it").to_vec();
                assert_eq!(records(op, &body).unwrap().0, sent as usize, "marked {marked}: the part reads back whole");
            }
        }
    }

    /// `drain_records`' named errors (short, bad magic, a truncated record, a count mismatch, two owners) and its
    /// None (no owner wrote the part).
    #[test]
    fn a_drain_part_is_read_whole_or_refused_by_name() {
        let o = orig(A_TAGS | A_KEEP | A_MARKS);
        let op = o.as_ptr() as *const u8;
        let part = |n: u32, body: &[u8]| [&MAGIC_U[..], &n.to_le_bytes(), body].concat();
        let one = raw(&[Some(3)]);
        let err = |owners: &[&[u8]], what: &str| match unsafe { drain_records(op, owners) } {
            Err(Error::Host(m)) => assert!(m.contains(what), "{m:?} names {what:?}"),
            r => panic!("{what}: {:?}", r.map(|b| b.map(<[u8]>::len))),
        };
        // SAFETY: `op` is the header above.
        unsafe {
            assert_eq!(drain_records(op, &[]).unwrap(), None, "no owner");
            assert_eq!(drain_records(op, &[&[], &[]]).unwrap(), None, "no owner wrote bytes");
            assert_eq!(drain_records(op, &[&part(1, &one)]).unwrap(), Some(&one[..]));
        }
        err(&[&part(1, &one)[..6]], "has no header");
        err(&[&[b'X', b'7', b'U', b'1', 1, 0, 0, 0][..]], "has no header");
        err(&[&part(1, &one[..10])], "ends inside a record");
        err(&[&part(1, &[one.clone(), one[..20].to_vec()].concat())], "ends inside a record");
        err(&[&part(2, &one)], "does not match its header");
        err(&[&part(0, &one)], "does not match its header");
        err(&[&part(1, &one), &[], &part(0, &[])], "written by two claims");
    }

    /// The drain parts sit past the kept probe rows, and the drain variant rides the arg bits above the partition bits.
    #[test]
    fn a_drain_part_follows_the_kept_rows_and_its_variant_rides_the_arg() {
        use types_nodes::jointype::JoinType;
        assert_eq!((u_part(5, 0), u_part(5, 31), u_part(6, 3)), (64, 95, 131));
        let bits = 5u64;
        for (jt, want) in [
            (JoinType::JOIN_RIGHT, DRAIN),
            (JoinType::JOIN_FULL, DRAIN),
            (JoinType::JOIN_RIGHT_ANTI, DRAIN),
            (JoinType::JOIN_RIGHT_SEMI, DRAIN | DRAIN_MARKED),
            (JoinType::JOIN_INNER, 0),
            (JoinType::JOIN_LEFT, 0),
            (JoinType::JOIN_SEMI, 0),
            (JoinType::JOIN_ANTI, 0),
        ] {
            let arg = bits | crate::ops::hashjoin::spill::pair::drain_of(jt);
            assert_eq!((arg & (DRAIN | DRAIN_MARKED), arg_bits(arg)), (want, 5), "{jt:?}");
        }
    }
}
