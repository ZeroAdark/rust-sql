//! `ops::setop`'s raw half in exec, where the family's unsafe code lives: the image words a recursive UNION's IR offers its generation object,
//! the generation object's cell and `Head::op` (generation.rs keeps its state and operations), and the walks over a
//! finished set side's breaker region (proto.rs, skip.rs): a plain record's count, an owned table's records, each
//! record's key and count words.
#![allow(unsafe_code)]
use gravity_abi::breaker::{table_kind, HashAgg, Owned, OwnerTop, RecordLayout, HASHAGG_HDR, LAYOUT_IMPLICIT_COUNT, OWNERTOP_HDR, TABLE_AGG, TABLE_OWNED};
use gravity_abi::Ctx;
use gravity_kernels::ops::setop::entries as abi;
use gravity_kernels::ops::shared::object::entries as abi8;

use super::proto::MIN_REGION;

/// The `1 + n` words at `img` (the IR's stack slot: the image's KeyHash, then its `n` words), `None` for a null address or
/// no image word.
/// # Safety
/// `img`, when not null, addresses `1 + n` readable words for the call (kernels ops/setop/raw.rs `GenSeen`).
pub(crate) unsafe fn image<'a>(img: *const u64, n: u64) -> Option<&'a [u64]> {
    if img.is_null() || n == 0 {
        return None;
    }
    // SAFETY: the caller's: `1 + n` readable words at `img`.
    Some(unsafe { core::slice::from_raw_parts(img, 1 + n as usize) })
}

/// setop_rec-0043: a generation cell's held word (`GEN_HELD`), which the union's IR reads at its end (ops/setop/generation.rs
/// `GenLive::reopen` clears it when the union's CTE is re-armed).
pub(crate) struct HeldWord(*mut u64);

impl HeldWord {
    /// The held word of `cell`, a generation cell `lay` laid, whose heap block the caller keeps beside this word to the
    /// execution's end.
    pub(crate) fn of(cell: &mut [u64]) -> HeldWord {
        HeldWord(core::ptr::from_mut(&mut cell[gravity_kernels::ops::setop::entries::GEN_HELD / 8]))
    }

    /// The word zeroed, outside every call of the object (the caller holds the object's state borrowed).
    pub(crate) fn clear(&self) {
        // SAFETY: `of`'s: the word is in a cell block alive to the execution's end; no call of the object reads it now
        unsafe { self.0.write(0) };
    }
}

impl HeldWord {
    /// The word set to `v`, inside the object's own call (`gen_op`: the IR reads it after the call returns).
    pub(crate) fn set(&self, v: u64) {
        // SAFETY: `of`'s / `gen_op`'s: the word is in a cell block alive to the execution's end
        unsafe { self.0.write(v) };
    }
}

/// A generation object's cell (generation.rs `WORDS` words: the head, the held word zero, then `rt`, the state's address,
/// last), kept by the caller beside the state to the execution's end.
pub(crate) fn lay_cell(rt: u64) -> Box<[u64]> {
    use super::generation::WORDS;
    let mut cell = vec![0u64; WORDS].into_boxed_slice(); // alloc: Exec
    // SAFETY: `cell` is WORDS words: the cell (its head written here, the held word zero), then the state's address.
    unsafe {
        let c = cell.as_mut_ptr();
        c.cast::<abi::GenCell>().write(abi::GenCell { head: abi8::Head { magic: abi::GEN_MAGIC, op: gen_op }, held: 0 });
        c.add(WORDS - 1).write(rt);
    }
    cell
}

/// The object's `Head::op` (generation.rs `op`): its held word and its state read from the cell, the image words read for
/// `OP_GEN_SEEN`.
/// # Safety
/// `h` is a live object's cell (the host checked its magic), laid by `lay_cell` for this execution over `WORDS` words whose
/// last holds the address of its `GenRt`.
unsafe fn gen_op(h: *mut abi8::Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    use super::generation::{op, GenRt, WORDS};
    // SAFETY: the caller's: the cell's held word, then its state's address, laid by `lay_cell` for this execution.
    let (held_word, rt) = unsafe { (HeldWord(h.cast::<u8>().add(abi::GEN_HELD).cast::<u64>()), &*(*h.cast::<u64>().add(WORDS - 1) as *const GenRt)) };
    // SAFETY: the IR's `1 + b` words at `a` (kernels raw.rs `GenSeen`, typesem userfp.rs `UserCollideProbe`): the KeyHash,
    // then the image.
    let img = if code == abi::OP_GEN_SEEN || code == abi::OP_GEN_HAS { unsafe { image(a as *const u64, b) } } else { None };
    // The seen set's growth billed to the execution `ctx` runs (none without one: a unit test's call)
    // SAFETY: a non-null `ctx` is the bound context the IR's host call passes.
    let mut overdraw = |bytes: u64| (!ctx.is_null()).then(|| unsafe { gravity_driver::hostimpl::arena::object_overdraw(ctx, bytes) });
    // CONVERT wave 2 (site 2): the set's bytes on the ledger, to the slot this pipeline produces into, else the statement
    op(rt, &held_word, code, img, b, &mut overdraw, &|| gravity_fn::arena::OpArena::slot_owner_kind(ctx))
}

/// A plain side's row count over its breaker region `r`: its one record's sentinel at byte `sent`, 0 when no record was
/// laid out; `None` when the bytes are not a record table.
pub(crate) fn region_count(r: &[u8], sent: usize) -> Option<i64> {
    if r.len() < HASHAGG_HDR.max(MIN_REGION) {
        return Some(0);
    }
    // SAFETY: a finished plain breaker region (a record table of one): its kind byte, then its header and record
    unsafe {
        let t = HashAgg::view(r.as_ptr());
        if table_kind(r.as_ptr()) != TABLE_AGG {
            // a region no row reached: zeroed, never laid out (the record host lays it out at the side's first row)
            return (t.cap == 0 && r.iter().all(|&b| b == 0)).then_some(0);
        }
        if t.len == 0 {
            return Some(0);
        }
        let n = *(t.record(0).as_ptr().add(sent) as *const i64);
        // an implicit sentinel is the rows + 1 (`Rec::open` marks the record used with 1; abi's merge takes the 1 off too)
        Some(if t.layout.flags & LAYOUT_IMPLICIT_COUNT != 0 { (n - 1).max(0) } else { n })
    }
}

/// A record of a finished owned table (`owned_each`): its address, live while its slot's region stays laid out (the
/// after-pipeline hooks read and write it before the emit that reads the slot runs).
#[derive(Clone, Copy)]
pub(crate) struct Record(*mut u8);

impl Record {
    /// The record's key (`key_into`) under layout `lay`, `n` keys over `w` image words, written over `out` (`key_len` words).
    pub(crate) fn key_into(self, lay: &RecordLayout, n: usize, w: usize, out: &mut [u64]) {
        // SAFETY: `owned_each`'s record of the side's layout, which holds at least `w` key words (proto.rs checks
        // `nkeys() >= w` before it reads one)
        unsafe { key_into(self.0, lay, n, w, out) }
    }

    /// The i64 count at byte `off` (a `Count` slot's offset in the record's layout).
    pub(crate) fn count(self, off: usize) -> i64 {
        // SAFETY: as `key`: a live record; `off` is a Count slot of its layout
        unsafe { *(self.0.add(off) as *const i64) }
    }

    /// The i64 count at byte `off` set to `v` (only the copy protocol writes its copy slot, before the emit reads it).
    pub(crate) fn set_count(self, off: usize, v: i64) {
        // SAFETY: as `count`
        unsafe { *(self.0.add(off) as *mut i64) = v };
    }
}

/// Every record of owned table region `r` (at least `MIN_REGION` bytes, finished: `agg_gather`'s runs-form walk, each
/// owner's directory of (offset, count) runs of `size` records) handed to `f` in that walk's order, no list made (CONVERT
/// wave 2: the list was an allocator Vec of a pointer per group); their number, or `None` when the region is not an owned
/// table (`f` never called). A count alone is the walk with a no-op `f`: the directories read, no record touched.
pub(crate) fn owned_each(r: &mut [u8], size: usize, mut f: impl FnMut(Record)) -> Option<usize> {
    let own = r.as_mut_ptr();
    // SAFETY: a finished breaker region of at least `MIN_REGION` bytes; its kind byte says whether it is an owned table.
    if unsafe { table_kind(own) } != TABLE_OWNED {
        return None;
    }
    // SAFETY: an owned table the owner phase finished: each owner's directory of (offset, count) runs of `size` records.
    unsafe {
        let o = Owned::view(own);
        let mut n = 0;
        for j in 0..o.nowners as usize {
            let h = OwnerTop::view(o.owner(j));
            let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
            for g in 0..h.worst as usize {
                let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
                (0..m).for_each(|k| f(Record(o.owner(j).add(off + k * size))));
                n += m;
            }
        }
        Some(n)
    }
}

/// Whether finished region `r` (at least `MIN_REGION` bytes) is an owned table none of whose owners' directories counts a
/// record (skip.rs: an empty left group).
pub(crate) fn owned_empty(r: &mut [u8]) -> bool {
    let own = r.as_mut_ptr();
    // SAFETY: as `owned_each`: a finished region's kind byte, then an owned table's directories
    unsafe {
        table_kind(own) == TABLE_OWNED && {
            let o = Owned::view(own);
            (0..o.nowners as usize).all(|j| {
                let h = OwnerTop::view(o.owner(j));
                let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
                (0..h.worst as usize).all(|g| *dir.add(2 * g + 1) == 0)
            })
        }
    }
}

/// A record's key: its image words (`w` of them; a NULL key's words are zero, as `emit_image` writes them) and the NULL
/// flags of its `n` keys, from the flags words at word index `w` on when the record has them (D1168: past the words, not
/// past `n`: a key may be two words; CAP-28: one flags word per 64 keys, key i's bit `breaker::flag_at(i)`), each word
/// masked to its keys' bits. A record without flags reads zero words, so its keys compare as a nullable side's non-NULL.
///
/// # Safety
/// `rec` is a record of layout `lay` with at least `w` key words; `out` holds `key_len(n, w)` words.
pub(crate) unsafe fn key_into(rec: *const u8, lay: &RecordLayout, n: usize, w: usize, out: &mut [u64]) {
    for (i, o) in out[..w].iter_mut().enumerate() {
        *o = *(rec.add(lay.key_off(i)) as *const u64);
    }
    for (f, o) in out[w..key_len(n, w)].iter_mut().enumerate() {
        let flags = if lay.nkeys() > w + f { *(rec.add(lay.key_off(w + f)) as *const u64) } else { 0 };
        let bits = n.saturating_sub(64 * f);
        *o = flags & if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    }
}

/// The words of a key `key_into` writes: `w` image words, then the flags words of `n` keys (at least one).
pub(crate) fn key_len(n: usize, w: usize) -> usize {
    w + gravity_abi::breaker::flag_words(n).max(1)
}

/// `key_into`'s key as a vector (the tests' reading).
///
/// # Safety
/// As `key_into`.
#[cfg(test)]
pub(crate) unsafe fn key(rec: *const u8, lay: &RecordLayout, n: usize, w: usize) -> Vec<u64> {
    let mut k = vec![0u64; key_len(n, w)]; // alloc: Cold
    // SAFETY: the caller's
    unsafe { key_into(rec, lay, n, w, &mut k) };
    k
}
