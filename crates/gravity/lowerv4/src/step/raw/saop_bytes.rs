//! LIST-FORM (catalog CAP-10 / STEP-14): expr.saop's byte set, a constant text IN list under a collation whose equality is
//! byte equality (step/expr_saop.rs `bytes_list`, `Hashed::bytes`): built once per execution at bind into a grant-pool
//! region the execution holds, its address the site's word (step/raw/saop_set.rs `bound` hands it here), laid out as the
//! hashed set's (a head, `mask + 1` slots of `[tag, value]`), then the elements' shortest and longest payload and the
//! array's image; a slot's value is an element's payload address in that image. Its own module (and code) so the
//! hashed sets' probe and saop_host's eval keep the code they had. Owner M2-FRAME.
#![allow(unsafe_code)]
use gravity_abi::hash::{Depth, KeyHash};
use gravity_expr::nodes::array::Walk;
use gravity_expr::views::{image_view, payload_view};
use gravity_fn::{Datum, PgError, Region};

use crate::rep::varlena::hash_tagged::INLINE;
use crate::step::raw::saop_host::Desc;

/// A slot's occupied bit (the hashed set's: a tag is never 0).
const OCC: u64 = 1 << 32;

/// The set's head (the hashed set's layout); `mask + 1` slots follow it.
#[repr(C)]
struct Head {
    mask: u64,
    has_nulls: u64,
}

/// A tag's 32-bit hash: the key hash of an image (gravity_abi::hash, the one mix).
fn mix(image: &[u64]) -> u32 {
    gravity_abi::hash::KeyHash::of(image).stored()
}

fn err(m: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: expr.saop: {m}"))) // alloc: Cold
}

/// `Head::has_nulls`'s bit 3, a byte set keyed by every byte (`full_tag`): a list PG hashes (cost `TEXT_SET_FULL_MIN` or
/// more) where three distinct elements share a `bytes_tag` (alike in length, head and tail: URLs, padded codes), which
/// PG's hash tells apart; a shorter list keeps the head / tail tag, whose shared tags cost what the walk's compares cost.
const FULL: u64 = 8;

/// D4794 (road text.set.bpchar): `Head::has_nulls`'s bit 4, a bpchar byte set (`Hashed::trim`): its elements were keyed
/// without their trailing blanks, so `probe_bytes` drops an operand's too (bpchareq's bcTruelen on both sides).
const TRIM: u64 = 16;

/// `b` without its trailing blanks (PostgreSQL's bcTruelen) when `trim`, else `b`.
fn bc_true(b: &[u8], trim: bool) -> &[u8] {
    if trim { &b[..b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)] } else { b }
}

/// The 8 bytes at `p + i`.
/// # Safety
/// `p + i .. p + i + 8` is readable.
unsafe fn word8(p: *const u8, i: usize) -> u64 {
    u64::from_le(core::ptr::read_unaligned(p.add(i) as *const u64))
}

/// Payload `b`'s bytes as at most three words: its head 16 and tail 8 bytes past 16, all of them below (overlapping reads).
fn head_tail(b: &[u8]) -> [u64; 3] {
    let (n, p) = (b.len(), b.as_ptr());
    // SAFETY: every read lies inside `b`.
    unsafe {
        match n {
            16.. => [word8(p, 0), word8(p, 8), word8(p, n - 8)],
            8.. => [word8(p, 0), word8(p, n - 8), 0],
            4.. => [u32::from_le(core::ptr::read_unaligned(p as *const u32)) as u64 | (u32::from_le(core::ptr::read_unaligned(p.add(n - 4) as *const u32)) as u64) << 32, 0, 0],
            _ => [b.iter().enumerate().fold(0u64, |x, (i, &c)| x | (c as u64) << (8 * i)), 0, 0],
        }
    }
}

/// A byte set's tag: the payload's length (bits 33..) over a 32-bit hash (with `OCC`), so equal tags mean equal lengths.
fn tag_of(n: usize, h: u32) -> u64 {
    h as u64 | OCC | (n as u64) << 33
}

/// A byte set's tag for payload `b`: its length and the key hash of its first 16 and last 8 bytes, so an operand's probe
/// reads at most 24 of its bytes before the compare; elements alike in those bytes and their length share a tag and are
/// each compared, which costs what the walk's compares cost.
fn bytes_tag(b: &[u8]) -> u64 {
    let [x, y, z] = head_tail(b);
    tag_of(b.len(), mix(&[x, y, z, b.len() as u64]))
}

/// A `FULL` byte set's tag for payload `b`: its length and the key hash of every byte, 32 at a time (the running hash and
/// four words through KeyHash), then the words left, the last one ending at the payload's end (overlapping).
fn full_tag(b: &[u8]) -> u64 {
    let (n, p) = (b.len(), b.as_ptr());
    if n < 16 {
        return bytes_tag(b);
    }
    let (mut h, mut i) = (n as u64, 0);
    while i + 32 <= n {
        // SAFETY: `i + 32 <= n`: the four words lie inside `b`.
        h = mix(unsafe { &[h, word8(p, i), word8(p, i + 8), word8(p, i + 16), word8(p, i + 24)] }) as u64;
        i += 32;
    }
    let (mut w, mut k) = ([h, 0, 0, 0, 0], 1);
    while i + 8 <= n {
        // SAFETY: `i + 8 <= n`.
        w[k] = unsafe { word8(p, i) };
        (k, i) = (k + 1, i + 8);
    }
    // SAFETY: n >= 16, so `n - 8` lies inside `b`.
    w[k] = unsafe { word8(p, n - 8) };
    tag_of(n, mix(&w[..k + 1]))
}

/// Into a byte set's `slots`: payload `(at, n)` under `tag`, unless an equal element is there (a repeated element keeps
/// one slot); the count of other elements already holding the tag.
/// # Safety
/// `at .. at + n` is the element's payload, in the set's copy of the array, as every slot's value is.
unsafe fn insert_bytes(slots: &mut [[u64; 2]], tag: u64, at: *const u8, n: usize) -> usize {
    let (mask, mut same) = (slots.len() - 1, 0);
    let mut i = tag as usize & mask;
    while slots[i][0] != 0 {
        if slots[i][0] == tag {
            if core::slice::from_raw_parts(slots[i][1] as usize as *const u8, n) == core::slice::from_raw_parts(at, n) {
                return same;
            }
            same += 1;
        }
        i = (i + 1) & mask;
    }
    slots[i] = [tag, at as u64];
    same
}

/// A varlena in line (varatt.h, little-endian as every lowering reads it: rep/varlena): `(payload, length)` for a plain
/// (4-byte) or a short (1-byte) header; `None` for a compressed or external one.
/// # Safety
/// `p` points at a live varlena datum.
unsafe fn in_line(p: *const u8) -> Option<(*const u8, usize)> {
    let b0 = *p;
    if b0 & 0x03 == 0 {
        Some((p.add(4), ((core::ptr::read_unaligned(p as *const u32) >> 2) & 0x3fff_ffff) as usize - 4))
    } else if b0 & 0x01 == 1 && b0 != 1 {
        Some((p.add(1), (b0 >> 1) as usize - 1))
    } else {
        None
    }
}

/// The payload length of the varlena at `p` from its header alone, never detoasting (PG's `toast_raw_datum_size` less
/// the header): an in-line datum's, a compressed one's raw size (`va_tcinfo`'s low 30 bits), an on-disk toast pointer's
/// (`va_rawsize`, the header included); `None` for any other form (read through its detoasted copy).
/// # Safety
/// `p` points at a live varlena datum.
unsafe fn raw_len(p: *const u8) -> Option<usize> {
    if let Some((_, n)) = in_line(p) {
        return Some(n);
    }
    match *p {
        1 => (*p.add(1) == 18).then(|| (core::ptr::read_unaligned(p.add(2) as *const i32) as usize).saturating_sub(4)),
        b0 if b0 & 0x03 == 0x02 => Some((core::ptr::read_unaligned(p.add(4) as *const u32) & 0x3fff_ffff) as usize),
        _ => None,
    }
}

/// One expanded value's buffer (CLASS E), charged to `ctx`'s owner (rep/varlena `value`; 0, the bind road's build: none).
struct Held {
    buf: gravity_fn::arena::ChargedVec<u8>,
    ctx: u64,
}

impl Held {
    fn new(ctx: u64) -> Held {
        Held { buf: gravity_fn::arena::ChargedVec::none(), ctx }
    }
}

/// LIST-FORM: varlena `d`'s image (`image`) or payload as `View` over `held`'s buffer, grown fourfold from 8 KiB (to 1 GiB)
/// while the view fails for want of space (a compressed or toasted datum's expanded copy): saop_host `with_region`'s heap
/// half, one copy of the code for the byte set's two readers. A grow takes a new buffer (charged at its size), the old
/// one and its charge gone first.
#[inline(never)]
fn expanded(d: u64, image: bool, held: &mut Held) -> types_error::PgResult<gravity_fn::View> {
    use gravity_fn::fmgr_call::FmgrCall;
    let mut cap = 8 << 10;
    loop {
        if held.buf.capacity() < cap {
            held.buf = crate::rep::varlena::raw::value::vec(held.ctx, cap);
        }
        held.buf.with_vec(|v| {
            v.clear();
            v.resize(cap, 0); // alloc: Cold charged value::owner(ctx, cap)
        });
        let mut r = Region::over(&mut held.buf[..]);
        let v = if image { image_view(Datum::from_u64(d), -1, &mut r) } else { payload_view(Datum::from_u64(d), -1, &mut r) };
        match v {
            Err(e) if FmgrCall::error_of(&e).0 == FmgrCall::error_of(&gravity_expr::views::no_space()).0 && cap < 1 << 30 => cap *= 4,
            v => return v,
        }
    }
}

/// LIST-FORM: the byte set of text array `a` (`Hashed::bytes`), laid out as any set (the head, `mask + 1` slots), then
/// the elements' shortest and longest payload (`min | max << 32`; an all-NULL list `u32::MAX`, 0) and the array's image,
/// each slot `[tag, the element's payload address in that image]`.
/// # Safety
/// `a` is a non-null datum of `d`'s array type.
#[inline(never)]
pub(crate) unsafe fn build_bytes(d: &Desc, a: u64) -> types_error::PgResult<gravity_driver::RegionBuf> {
    // the bind road: before any pipeline runs, on the Bind's context owner; exec's plan memo makes it the session's when it
    // files the admission (exec `GraphWords::adopt_sets`, D4844)
    let mut held = Held::new(0);
    let img = match in_line(a as usize as *const u8) {
        // a plain image in place (a short-header array is rebuilt with its 4-byte header, as image_view does)
        Some(_) if *(a as usize as *const u8) & 0x03 == 0 => gravity_fn::View { ptr: a as usize as *const u8, len: ((core::ptr::read_unaligned(a as usize as *const u32) >> 2) & 0x3fff_ffff) as u64 },
        _ => expanded(a, true, &mut held)?,
    };
    let n = Walk::over(Datum::from_usize(img.ptr as usize), d.elem, &mut Region::over(&mut []))?.len();
    let cap = (n * 2).next_power_of_two().max(8);
    let at = 16 + cap * 16 + 16;
    // road text.set: room for the images (`images`, as many slots as the byte set's) after the array's image, 8-aligned
    let (tset, sec) = (d.hashed.as_ref().is_some_and(|h| h.images), (at + img.len as usize).next_multiple_of(8));
    let room = if tset { sec + IMG_HEAD + (n + cap) * 16 } else { at + img.len as usize };
    let mut buf = gravity_driver::in_list_set_region(room).map_err(|e| super::saop_set::region_err("the IN list's set", e))?;
    let base = buf.as_mut_ptr();
    if base as usize % 8 != 0 {
        return Err(err("the IN list's set region is not 8-aligned"));
    }
    core::ptr::copy_nonoverlapping(img.ptr, base.add(at), img.len as usize);
    let head = base as *mut Head;
    let slots = core::slice::from_raw_parts_mut(head.add(1) as *mut [u64; 2], cap);
    slots.fill([0, 0]);
    (*head).mask = cap as u64 - 1;
    (*head).has_nulls = 0;
    let trim = d.hashed.as_ref().is_some_and(|h| h.trim);
    let (mut lo, mut hi) = (u32::MAX as usize, 0usize);
    let mut w = Walk::over(Datum::from_usize(base.add(at) as usize), d.elem, &mut Region::over(&mut []))?;
    while let Some(e) = w.next() {
        if e.isnull {
            (*head).has_nulls |= 1;
            continue;
        }
        // an element is never compressed nor external: PG detoasts them into the array
        let Some((p, len)) = in_line(e.value.as_usize() as *const u8) else { return Err(err("an IN list element that is not in line")) };
        let len = bc_true(core::slice::from_raw_parts(p, len), trim).len();
        (lo, hi) = (lo.min(len), hi.max(len));
        let full = (*head).has_nulls & FULL != 0;
        let b = core::slice::from_raw_parts(p, len);
        if insert_bytes(slots, if full { full_tag(b) } else { bytes_tag(b) }, p, len) >= 2 && !full && n >= gravity_program::cost::expr::TEXT_SET_FULL_MIN {
            // three distinct elements share a head / tail tag in a list PG hashes: key every byte, from the start
            slots.fill([0, 0]);
            (*head).has_nulls = FULL;
            (lo, hi) = (u32::MAX as usize, 0);
            w = Walk::over(Datum::from_usize(base.add(at) as usize), d.elem, &mut Region::over(&mut []))?;
        }
    }
    *(base.add(16 + cap * 16) as *mut u64) = lo as u64 | (hi as u64) << 32;
    if trim {
        (*head).has_nulls |= TRIM;
    }
    if tset && (hi <= INLINE as usize || lo > hi) {
        images(base.add(sec), base.add(at), d, n, cap)?;
        (*head).has_nulls |= (sec as u64 >> 3) << 32;
    }
    Ok(buf)
}

/// The images' head: the element count, the table's mask, its shift, a spare word.
pub(crate) const IMG_HEAD: usize = 32;
/// An image's second word no payload has (its top byte, the length, is above `INLINE`): a NULL element's slot in the
/// list, an empty slot in the table.
pub(crate) const IMG_NONE: u64 = u64::MAX;

/// Road text.set (textkeys step 2): payload `b`'s image (at most `INLINE` bytes, hash_tagged.rs: key.tagged's inline
/// form): its bytes 0..8 and 8.. as little-endian words, zero above them, the length in the second word's top byte, so
/// equal images are equal payloads. text_set.rs builds the same two words from an operand's bytes in line.
pub(crate) fn inline_image(b: &[u8]) -> [u64; 2] {
    let mut w = [0u8; 16];
    w[..b.len()].copy_from_slice(b);
    let (x, y) = w.split_at(8);
    let word = |v: &[u8]| v.iter().rev().fold(0u64, |a, &c| a << 8 | c as u64);
    [word(x), word(y) | (b.len() as u64) << 56]
}

/// Road text.set: the images of the `n` elements of the array image at `arr` laid out at `sec` (8-aligned): the head
/// (`n`, the table's mask, its home shift, 0), the list (`n` images in element order, a NULL's `[0, IMG_NONE]`), then the
/// table (`cap2` slots, open addressing from the image's KeyHash home, an empty slot's second word `IMG_NONE`). The caller
/// checked that no element is longer than `INLINE`.
/// # Safety
/// `sec` has `IMG_HEAD + (n + cap2) * 16` writable bytes; `arr` is the set's copy of a text array of `n` elements.
unsafe fn images(sec: *mut u8, arr: *const u8, d: &Desc, n: usize, cap2: usize) -> types_error::PgResult<()> {
    let head = sec as *mut u64;
    let bits = cap2.trailing_zeros();
    for (k, v) in [n as u64, cap2 as u64 - 1, 64 - bits as u64, 0].into_iter().enumerate() {
        *head.add(k) = v;
    }
    let list = core::slice::from_raw_parts_mut(sec.add(IMG_HEAD) as *mut [u64; 2], n);
    let table = core::slice::from_raw_parts_mut(sec.add(IMG_HEAD + n * 16) as *mut [u64; 2], cap2);
    table.fill([0, IMG_NONE]);
    let mut w = Walk::over(Datum::from_usize(arr as usize), d.elem, &mut Region::over(&mut []))?;
    let mut i = 0;
    while let Some(e) = w.next() {
        let Some((p, len)) = (!e.isnull).then(|| in_line(e.value.as_usize() as *const u8)).flatten() else {
            list[i] = [0, IMG_NONE];
            i += 1;
            continue;
        };
        list[i] = inline_image(bc_true(core::slice::from_raw_parts(p, len), d.hashed.as_ref().is_some_and(|h| h.trim)));
        let mut k = KeyHash::of(&list[i]).home(bits, Depth::TOP);
        while table[k][1] != IMG_NONE && table[k] != list[i] {
            k = (k + 1) & (cap2 - 1);
        }
        table[k] = list[i];
        i += 1;
    }
    if i != n {
        return Err(err("the IN list's images miscounted"));
    }
    Ok(())
}

/// LIST-FORM: the byte set's answer for operand `v` (a non-null text datum): `(found, the array holds a NULL)`. An operand
/// whose length no element has misses on its header alone; else its payload (in place, or detoasted) keys one probe, and
/// a tag's elements compare byte for byte.
/// An expanded operand's copy is charged to `ctx`'s owner (`Held`; 0: none).
/// # Safety
/// `set` is the address of a region `build_bytes` built; `ctx` is 0 or the caller's bound ctx.
#[inline(never)]
pub(crate) unsafe fn probe_bytes(ctx: u64, set: u64, v: u64) -> Result<(bool, bool), Box<PgError>> {
    let (lo, hi) = lens_of(set);
    let nulls = (*(set as *const Head)).has_nulls & 1 != 0;
    // D4794: a bpchar set's operand without its trailing blanks (its raw length then bounds it from above only)
    let trim = (*(set as *const Head)).has_nulls & TRIM != 0;
    let p = v as usize as *const u8;
    if let Some((at, n)) = in_line(p) {
        return Ok(probe_payload(set, bc_true(core::slice::from_raw_parts(at, n), trim)));
    }
    if raw_len(p).is_some_and(|n| n < lo || (n > hi && !trim)) {
        return Ok((false, nulls));
    }
    let mut held = Held::new(ctx);
    let pv = expanded(v, false, &mut held)?;
    Ok(probe_payload(set, bc_true(core::slice::from_raw_parts(pv.ptr, pv.len as usize), trim)))
}

/// The set's shortest and longest element payload.
/// # Safety
/// `set` is the address of a region `build_bytes` built.
unsafe fn lens_of(set: u64) -> (usize, usize) {
    let head = set as *const Head;
    let lens = *((head as *const u8).add(16 + ((*head).mask as usize + 1) * 16) as *const u64);
    (lens as u32 as usize, (lens >> 32) as usize)
}

/// The byte set's answer for payload `b` (an operand's bytes, or road text.set's view of them): `(found, the array holds
/// a NULL)`.
/// # Safety
/// `set` is the address of a region `build_bytes` built.
pub(crate) unsafe fn probe_payload(set: u64, b: &[u8]) -> (bool, bool) {
    let head = set as *const Head;
    let mask = (*head).mask as usize;
    let slots = core::slice::from_raw_parts(head.add(1) as *const [u64; 2], mask + 1);
    let (lo, hi) = lens_of(set);
    let (nulls, full) = ((*head).has_nulls & 1 != 0, (*head).has_nulls & FULL != 0);
    if b.len() < lo || b.len() > hi {
        return (false, nulls);
    }
    let tag = if full { full_tag(b) } else { bytes_tag(b) };
    let mut i = tag as usize & mask;
    while slots[i][0] != 0 {
        // SAFETY: a slot's value is an element's payload of the tag's length, in the set's copy of the array
        if slots[i][0] == tag && core::slice::from_raw_parts(slots[i][1] as usize as *const u8, b.len()) == b {
            return (true, nulls);
        }
        i = (i + 1) & mask;
    }
    (false, nulls)
}

#[cfg(test)]
mod tests {
    use gravity_expr::nodes::array::ElemFacts;

    use super::*;
    use crate::step::raw::saop_host::Hashed;

    /// A 4-byte-header text datum of `b`, in 8-byte words.
    fn text(b: &[u8]) -> Vec<u64> {
        let mut img = (((4 + b.len()) as u32) << 2).to_le_bytes().to_vec(); // alloc: Cold
        img.extend_from_slice(b);
        let mut w = vec![0u64; img.len().div_ceil(8)]; // alloc: Cold
        unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), w.as_mut_ptr().cast::<u8>(), img.len()) };
        w
    }

    /// A one-dimensional text[] image of `elems` (`None`: NULL, with its bitmap), 4-byte element headers, in 8-byte words.
    fn array(elems: &[Option<&[u8]>]) -> Vec<u64> {
        let nulls = elems.iter().any(|e| e.is_none());
        let bitmap = if nulls { elems.len().div_ceil(8) } else { 0 };
        let mut data = Vec::new(); // alloc: Cold
        for e in elems.iter().flatten() {
            data.extend_from_slice(&(((4 + e.len()) as u32) << 2).to_le_bytes());
            data.extend_from_slice(e);
            data.resize(data.len().div_ceil(4) * 4, 0);
        }
        let head = (24 + bitmap).div_ceil(8) * 8;
        let mut img = vec![0u8; head]; // alloc: Cold
        img[4..8].copy_from_slice(&1i32.to_le_bytes());
        img[8..12].copy_from_slice(&(if nulls { head as i32 } else { 0 }).to_le_bytes());
        img[12..16].copy_from_slice(&25u32.to_le_bytes());
        img[16..20].copy_from_slice(&(elems.len() as i32).to_le_bytes());
        img[20..24].copy_from_slice(&1i32.to_le_bytes());
        for (i, e) in elems.iter().enumerate() {
            if e.is_some() && nulls {
                img[24 + i / 8] |= 1 << (i % 8);
            }
        }
        img.extend_from_slice(&data);
        let n = img.len() as u32;
        img[0..4].copy_from_slice(&(n << 2).to_le_bytes());
        let mut w = vec![0u64; img.len().div_ceil(8)]; // alloc: Cold
        unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), w.as_mut_ptr().cast::<u8>(), img.len()) };
        w
    }

    fn byte_desc() -> Desc {
        let elem = ElemFacts { typ: 25, len: -1, byval: false, align: b'i' as i8 };
        let hashed = Some(Hashed { eq: None, hash: None, bytes: true, images: true, extreme: None, trim: false });
        Desc { use_or: true, coll: 100, op: None, elem, lens: [-1, -1], hashed, user: 0, image_eq: 0, fp: 0 }.sealed()
    }

    /// D4367: a byte set whose region the grant pool refuses is PostgreSQL's out of memory (53200) naming the set, never
    /// an internal error (the pool refuses such a draw while the process is past the memory budget).
    #[test]
    fn a_refused_set_region_is_out_of_memory_naming_the_set() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let img = array(&[Some(&b"abc"[..]), Some(&b"def"[..])]);
        let d = byte_desc();
        gravity_driver::POOL.arm(1);
        let r = unsafe { build_bytes(&d, img.as_ptr() as u64) };
        gravity_driver::POOL.arm(0);
        let e = r.err().expect("a refused region refuses the set");
        assert_eq!(e.sqlstate(), types_error::ERRCODE_OUT_OF_MEMORY, "{}", e.message());
        assert!(e.detail().is_some_and(|d| d.contains("the IN list's set")), "{:?}", e.detail());
    }

    /// LIST-FORM: the byte set finds each element (the empty string, one alike in its length and its head and tail bytes
    /// so sharing a tag, a long one) and nothing else; a miss over a list holding a NULL reports it.
    #[test]
    fn a_byte_set_finds_its_elements_by_their_bytes() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let (m1, m2, m3) = (b"AAAAAAAAAAAAAAAAm1ZZZZZZZZ", b"AAAAAAAAAAAAAAAAm2ZZZZZZZZ", b"AAAAAAAAAAAAAAAAm3ZZZZZZZZ");
        assert_eq!(bytes_tag(m1), bytes_tag(m2), "alike in length, head and tail: one tag");
        let long = [b'q'; 300];
        for nulls in [false, true] {
            let mut elems: Vec<Option<&[u8]>> = vec![Some(b""), Some(b"abc"), Some(m1), Some(m2), Some(&long[..]), Some("日本".as_bytes())]; // alloc: Cold
            if nulls {
                elems.insert(2, None);
            }
            let img = array(&elems);
            let d = byte_desc();
            let set = unsafe { build_bytes(&d, img.as_ptr() as u64) }.unwrap();
            let at = set.as_ptr() as u64;
            let probe = |b: &[u8]| {
                let v = text(b);
                unsafe { probe_bytes(0, at, v.as_ptr() as u64) }.unwrap()
            };
            for e in elems.iter().flatten() {
                assert_eq!(probe(e), (true, nulls), "element {:?}", String::from_utf8_lossy(e));
            }
            for miss in [&b"ab"[..], b"abd", m3, &long[..299], b"abcd", "日".as_bytes()] {
                assert_eq!(probe(miss), (false, nulls), "operand {:?}", String::from_utf8_lossy(miss));
            }
        }
    }

    /// D4794 (road text.set.bpchar): a trimmed byte set answers as bpchareq: elements and operands compared without their
    /// trailing blanks (padded either side, all blanks = the empty string), leading and inner blanks kept; its images are
    /// the trimmed bytes' (text_set.rs probes those in line after text.lean's trim).
    #[test]
    fn a_bpchar_byte_set_compares_without_trailing_blanks() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let mut d = byte_desc();
        if let Some(h) = d.hashed.as_mut() {
            h.trim = true;
        }
        let d = d.sealed();
        assert_ne!(d.fp, byte_desc().fp, "a trimmed set is another site's constant");
        for nulls in [false, true] {
            let mut elems: Vec<Option<&[u8]>> = vec![Some(b"MAIL"), Some(b"SHIP   "), Some(b"  "), Some(b"a b")]; // alloc: Cold
            if nulls {
                elems.push(None);
            }
            let img = array(&elems);
            let set = unsafe { build_bytes(&d, img.as_ptr() as u64) }.unwrap();
            let at = set.as_ptr() as u64;
            assert_ne!(unsafe { (*(at as *const Head)).has_nulls } & TRIM, 0, "the head says trimmed");
            let probe = |b: &[u8]| {
                let v = text(b);
                unsafe { probe_bytes(0, at, v.as_ptr() as u64) }.unwrap()
            };
            for hit in [&b"MAIL"[..], b"MAIL      ", b"SHIP", b"SHIP ", b"", b"     ", b"a b  "] {
                assert_eq!(probe(hit), (true, nulls), "operand {:?}", String::from_utf8_lossy(hit));
            }
            for miss in [&b" MAIL"[..], b"MAILX", b"MAI", b"a  b", b"ab"] {
                assert_eq!(probe(miss), (false, nulls), "operand {:?}", String::from_utf8_lossy(miss));
            }
            let lens = unsafe { lens_of(at) };
            assert_eq!(lens, (0, 4), "the trimmed lengths bound the set");
        }
    }

    /// A list PG hashes (9 or more) whose elements share their head / tail tags keys every byte (`FULL`), so a probe
    /// compares one element, not the group; a repeated element keeps one slot; a short list keeps the head / tail tag.
    #[test]
    fn alike_elements_of_a_hashed_list_key_every_byte() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let names: Vec<Vec<u8>> = (0..12).map(|i| format!("AAAAAAAAAAAAAAAAm{i:02}ZZZZZZZZ").into_bytes()).collect(); // alloc: Cold
        for (k, want_full) in [(12usize, true), (7, false)] {
            let mut elems: Vec<Option<&[u8]>> = names[..k].iter().map(|b| Some(&b[..])).collect(); // alloc: Cold
            elems.push(Some(&names[0][..]));
            let img = array(&elems);
            let d = byte_desc();
            let set = unsafe { build_bytes(&d, img.as_ptr() as u64) }.unwrap();
            let at = set.as_ptr() as u64;
            let head = at as *const Head;
            assert_eq!(unsafe { (*head).has_nulls } & FULL != 0, want_full, "{k} alike elements");
            let slots = unsafe { core::slice::from_raw_parts(head.add(1) as *const [u64; 2], (*head).mask as usize + 1) };
            assert_eq!(slots.iter().filter(|s| s[0] != 0).count(), k, "the repeated element keeps one slot");
            for e in &names[..k] {
                let v = text(e);
                assert_eq!(unsafe { probe_bytes(0, at, v.as_ptr() as u64) }.unwrap(), (true, false));
            }
            for miss in [&b"AAAAAAAAAAAAAAAAm12ZZZZZZZZ"[..], b"AAAAAAAAAAAAAAAAm1ZZZZZZZZ", b"AAAAAAAAAAAAAAAAm01ZZZZZZZY"] {
                let v = text(miss);
                assert_eq!(unsafe { probe_bytes(0, at, v.as_ptr() as u64) }.unwrap(), (false, false));
            }
        }
    }

    /// The length a header gives without detoasting: plain, short, compressed (its raw size), on-disk external.
    #[test]
    fn raw_len_reads_the_header() {
        let plain = text(b"hello");
        let short: [u8; 8] = [(6 << 1) | 1, b'h', b'e', b'l', b'l', b'o', 0, 0];
        let mut comp = [0u8; 16];
        comp[0..4].copy_from_slice(&((16u32 << 2) | 2).to_le_bytes());
        comp[4..8].copy_from_slice(&(5000u32 | (1 << 30)).to_le_bytes());
        let mut ext = [0u8; 20];
        ext[0] = 1;
        ext[1] = 18;
        ext[2..6].copy_from_slice(&7004i32.to_le_bytes());
        unsafe {
            assert_eq!(raw_len(plain.as_ptr().cast()), Some(5));
            assert_eq!(raw_len(short.as_ptr()), Some(5));
            assert_eq!(raw_len(comp.as_ptr()), Some(5000));
            assert_eq!(raw_len(ext.as_ptr()), Some(7000));
        }
    }

    /// The tag keeps bit 32 set, so an element whose hash is 0 is never an empty slot.
    #[test]
    fn a_zero_hash_is_occupied() {
        assert_ne!(mix(&[0]) as u64 | OCC, 0);
        assert_ne!(0u64 | OCC, 0);
    }

    /// A 1-byte-header text datum of `b` (at most 126 bytes).
    fn text1(b: &[u8]) -> Vec<u8> {
        let mut v = vec![((b.len() as u8 + 1) << 1) | 1]; // alloc: Cold
        v.extend_from_slice(b);
        v
    }

    /// A 4-byte-header text datum of `b`, its bytes alone (no padding past them).
    fn text4(b: &[u8]) -> Vec<u8> {
        let mut v = (((b.len() as u32 + 4) << 2).to_le_bytes()).to_vec(); // alloc: Cold
        v.extend_from_slice(b);
        v
    }

    /// Road text.set: `image` is two little-endian words and the length, and no two payloads of at most 15 bytes share
    /// one; `IMG_NONE` is no payload's.
    #[test]
    fn an_image_is_its_bytes_and_length() {
        assert_eq!(inline_image(b""), [0, 0]);
        assert_eq!(inline_image(b"ab"), [0x6261, 2 << 56]);
        assert_eq!(inline_image(b"abcdefghi"), [u64::from_le_bytes(*b"abcdefgh"), b'i' as u64 | 9 << 56]);
        assert_eq!(inline_image(b"\0"), [0, 1 << 56], "a zero byte is not the empty string");
        for n in 0..=INLINE as usize {
            assert_ne!(inline_image(&[0xFF; 15][..n])[1], IMG_NONE);
        }
    }

    /// Road text.set: the in-line probe (rep/varlena/text_set.rs `probe`: the operand's image from its bytes in line, the
    /// list unrolled for a keyed count of at most `UNROLL`, the table past it or with no keyed count) answers as the byte
    /// set's probe and PostgreSQL's ScalarArrayOp: lists of 2 to 23 elements with and without a NULL and a repeated
    /// element, the empty string, 8 / 9 / 15-byte and multibyte elements; operands of 0 to 20 bytes over 1- and 4-byte
    /// headers placed at every offset of a word among garbage bytes; `= ANY` and `<> ALL`. A NULL operand, and a list with
    /// an element past 15 bytes (no images), take the cold path.
    #[test]
    fn text_set_probes_as_the_byte_set() {
        use gravity_ir::Type;
        use crate::frame::stop::tests::run_with;
        use crate::rep::varlena::text_set::{probe, UNROLL};
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let pool: Vec<&[u8]> = vec![b"85669", b"", b"CA", b"abcdefgh", b"abcdefghi", b"abcdefghijklmno", "日本".as_bytes(), b"a", b"WA", b"GA", b"Sports", b"Books", b"Home", b"86197", b"88274", b"83405", b"86475", b"85392", b"85460", b"80348", b"81792", b"abcdefg", b"Music"]; // alloc: Cold
        let mut ops: Vec<Vec<u8>> = pool.iter().map(|b| b.to_vec()).collect(); // alloc: Cold
        ops.extend([&b"abcdefghijklmnop"[..], b"abcdefghijklmn", b"CA ", b"C", "日".as_bytes(), b"8566", b"856690", b"x", b"abcdefghijklmnopqrst", b"\0"].iter().map(|b| b.to_vec()));
        const COLD: i64 = 99;
        for n in [2usize, 3, 7, 8, 9, 12, 16, 23] {
            for variant in 0..3 {
                let mut elems: Vec<Option<&[u8]>> = pool.iter().take(n).map(|b| Some(*b)).collect(); // alloc: Cold
                match variant {
                    1 => elems[n / 2] = None,
                    2 => elems[n - 1] = elems[0],
                    _ => {}
                }
                let img = array(&elems);
                let set = unsafe { build_bytes(&byte_desc(), img.as_ptr() as u64) }.unwrap();
                let at = set.as_ptr() as u64;
                assert_ne!(unsafe { (*(at as *const Head)).has_nulls } >> 32, 0, "a list of short elements has images");
                for keyed in [Some(n).filter(|&n| n <= 16), None] {
                    for use_or in [true, false] {
                        for o in &ops {
                            let (found, nulls) = unsafe { probe_payload(at, o) };
                            let want = if found { use_or as i64 } else if nulls { 2 } else { !use_or as i64 };
                            for (k, datum) in [text1(o), text4(o)].into_iter().enumerate() {
                                for shift in 0..8 {
                                    let mut buf = vec![0xAAu8; shift]; // alloc: Cold
                                    buf.extend_from_slice(&datum);
                                    buf.extend_from_slice(&[0xAA; 24]);
                                    let p = buf[shift..].as_ptr() as i64;
                                    let got = run_with(&[], &[], &mut |b, env| {
                                        let (sv, pv, t) = (b.iconst(Type::I64, at as i64), b.iconst(Type::I64, p), b.bconst(true));
                                        probe(b, env, sv, pv, t, None, false, keyed, use_or, &mut |b, _| b.iconst(Type::I64, COLD))
                                    });
                                    assert_eq!(got, want, "n {n} variant {variant} keyed {keyed:?} use_or {use_or} header {k} shift {shift} operand {:?}", String::from_utf8_lossy(o));
                                }
                            }
                        }
                    }
                }
                let null = run_with(&[], &[], &mut |b, env| {
                    let (sv, pv, f) = (b.iconst(Type::I64, at as i64), b.iconst(Type::I64, 0), b.bconst(false));
                    probe(b, env, sv, pv, f, None, false, Some(n).filter(|&n| n <= UNROLL), true, &mut |b, _| b.iconst(Type::I64, COLD))
                });
                assert_eq!(null, COLD, "a NULL operand goes cold");
            }
        }
        let img = array(&[Some(&b"short"[..]), Some(&b"sixteen bytes!!!"[..])]);
        let set = unsafe { build_bytes(&byte_desc(), img.as_ptr() as u64) }.unwrap();
        let at = set.as_ptr() as u64;
        assert_eq!(unsafe { (*(at as *const Head)).has_nulls } >> 32, 0, "an element past 15 bytes: no images");
        let d = text1(b"short");
        let got = run_with(&[], &[], &mut |b, env| {
            let (sv, pv, t) = (b.iconst(Type::I64, at as i64), b.iconst(Type::I64, d.as_ptr() as i64), b.bconst(true));
            probe(b, env, sv, pv, t, None, false, Some(2), true, &mut |b, _| b.iconst(Type::I64, COLD))
        });
        assert_eq!(got, COLD, "no images: the cold path");
    }

    /// Road text.set: a view operand (substring from `s` for `n` characters, rtrim1) is read over the datum in place
    /// (text_view.rs `substr` / `rtrim`) and probed as PostgreSQL's text_substring (characters by UTF-8 lead bytes) would
    /// cut it; a negative count goes cold (PostgreSQL's error); the view host (raw.rs `text_set_view`) answers an rtrim1
    /// view the same (its substring needs the server's encoding seam).
    #[test]
    fn text_set_probes_a_view() {
        use gravity_ir::Type;
        use crate::frame::stop::tests::run_with;
        use crate::rep::varlena::raw::text_set::text_set_view;
        use crate::rep::varlena::text_set::probe;
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let img = array(&[Some(&b"856"[..]), Some(&"日本".as_bytes()[..]), Some(&b"bcd"[..]), Some(&b""[..]), None]);
        let set = unsafe { build_bytes(&byte_desc(), img.as_ptr() as u64) }.unwrap();
        let at = set.as_ptr() as u64;
        // PostgreSQL's text_substring over characters: from max(s, 1), to s + n (exclusive) when a count is given
        let cut = |v: &str, st: i64, n: Option<i64>| -> String {
            let chars: Vec<char> = v.chars().collect(); // alloc: Cold
            let s1 = st.max(1) as usize - 1;
            let end = n.map_or(chars.len(), |n| (st + n - 1).clamp(0, chars.len() as i64) as usize);
            chars.get(s1..end.max(s1)).map_or(String::new(), |c| c.iter().collect())
        };
        let cases: [(&str, bool, Option<(i64, Option<i64>)>); 11] = [
            ("85669", false, Some((1, Some(3)))), ("abcdefgh", false, Some((2, Some(3)))), ("abcdefgh", false, Some((2, Some(4)))), ("日本語", false, Some((1, Some(2)))),
            ("abc", false, Some((5, Some(2)))), ("abcd   ", true, Some((2, None))), ("85669", false, Some((1, Some(-1)))), ("  ", true, Some((1, None))), ("8566", false, Some((0, Some(4)))),
            ("bcd  ", true, None), ("bcdx ", true, None),
        ];
        for (v, trim, sub) in cases {
            let d = text1(v.as_bytes());
            let got = run_with(&[], &[], &mut |b, env| {
                let (sv, pv, t) = (b.iconst(Type::I64, at as i64), b.iconst(Type::I64, d.as_ptr() as i64), b.bconst(true));
                let words = sub.map(|(st, n)| (b.iconst(Type::I64, st), n.map(|n| b.iconst(Type::I64, n))));
                probe(b, env, sv, pv, t, Some((trim, words)), false, Some(5), true, &mut |b, _| b.iconst(Type::I64, -7))
            });
            if sub.is_some_and(|(_, n)| n.is_some_and(|n| n < 0)) {
                assert_eq!(got, -7, "a negative count goes cold");
                continue;
            }
            let t = if trim { v.trim_end_matches(' ') } else { v };
            let x = sub.map_or(t.to_owned(), |(st, n)| cut(t, st, n));
            let (found, nulls) = unsafe { probe_payload(at, x.as_bytes()) };
            let want = if found { 1 } else if nulls { 2 } else { 0 };
            assert_eq!(got, want, "{v:?} trim {trim} {sub:?} -> {x:?}");
            if sub.is_none() {
                let host = unsafe { text_set_view(0, d.as_ptr() as u64, at, trim as u64 | 16, 0, 0) };
                assert_eq!(host, want, "the host, {v:?} trim {trim}");
            }
        }
    }
}
