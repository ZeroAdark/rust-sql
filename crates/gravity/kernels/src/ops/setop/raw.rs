//! setop's raw half, where the family's unsafe and C-ABI code lives: the generation object's entries and the streamed
//! SetOp's text key compare and key copies (a varlena's, a fixed image's), which the lowered IR reaches through the
//! contract's landing pad (`contract::entry1::<H>`, `entry3`), and its two C-ABI entries written through the contract's
//! `host!` (entries.rs re-exports them under their old path). A built-in comparator's compare is lowerv4's
//! (ops/setop/raw.rs `StreamHostedCmp`, through KeySem's bound-call table).
use gravity_abi::Ctx;

use super::entries::{GenCell, GEN_MAGIC, OP_GEN_CHECK, OP_GEN_HOLD, OP_GEN_RAISE, OP_GEN_SEEN};
use crate::contract::Host;

crate::contract::host! {
    /// `setop.recursive`: the error the term's rerun just stashed (`m8_rerun` returned below zero) kept by generation `h`
    /// until the rows the term appended before it are returned: 0. An error that is never kept, or none stashed (a parked
    /// unwind), leaves it stashed: a status below zero, the IR raises at once. No `guard!` (crash policy): panic-free.
    pub unsafe fn m9_gen_hold(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, OP_GEN_HOLD, "m9_gen_hold")
    }

    /// `setop.recursive`: the kept error stashed again for the IR's raise; always a status below zero. No `guard!` (crash
    /// policy): panic-free.
    pub unsafe fn m9_gen_raise(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, OP_GEN_RAISE, "m9_gen_raise")
    }
}

/// Panic-free: a null or foreign handle is an error by name, never a call through it.
#[inline(always)]
unsafe fn call(ctx: *mut Ctx, h: u64, code: u64, entry: &str) -> i64 {
    let cell = h as *mut GenCell;
    if cell.is_null() || (*cell).head.magic != GEN_MAGIC {
        return stale(entry, h);
    }
    let head = core::ptr::addr_of_mut!((*cell).head);
    ((*cell).head.op)(head, ctx, code, 0, 0)
}

#[cold]
#[inline(never)]
fn stale(entry: &str, h: u64) -> i64 {
    crate::host_abi::bare(&format!("{entry}: a stale or foreign M9 handle {h:#x}")) // alloc: Cold
}

/// `setop.recursive` (D2231): PG's CHECK_FOR_INTERRUPTS once per generation, before the term re-runs (the IR passes the
/// generation object's handle): 0, or a status below zero with the interrupt's error stashed.
pub struct GenCheck;

impl Host for GenCheck {
    type Args = (u64,);
    /// # Safety
    /// `h` is the generation object's handle word (exec lays a `GenCell` per execution) or a stale one, which its magic
    /// refuses by name; nothing else is read.
    unsafe fn run(ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        let cell = h as *mut GenCell;
        if cell.is_null() || (*cell).head.magic != GEN_MAGIC {
            return crate::host_abi::bare(&format!("m9_gen_check: a stale or foreign M9 handle {h:#x}")) as u64; // alloc: Cold
        }
        let head = core::ptr::addr_of_mut!((*cell).head);
        ((*cell).head.op)(head, ctx, OP_GEN_CHECK, 0, 0) as u64
    }
}

/// `setop.recursive` (D1378): a recursive UNION's row offered to its generation object's set before it enters a table. The
/// IR passes the handle, the address of `1 + n` words (the image's KeyHash, then its `n` image words, NULL flags last) and
/// `n`: 1 when the image is new (now held), 0 when it was seen, or a status below zero with the error stashed.
pub struct GenSeen;

impl Host for GenSeen {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `h` as `GenCheck`'s; `img` addresses `1 + n` readable words for the call (the IR's stack slot); the object reads them
    /// and copies a new image, never keeping the address.
    unsafe fn run(ctx: *mut Ctx, (h, img, n): (u64, u64, u64)) -> u64 {
        let cell = h as *mut GenCell;
        if cell.is_null() || (*cell).head.magic != GEN_MAGIC {
            return crate::host_abi::bare(&format!("m9_gen_seen: a stale or foreign M9 handle {h:#x}")) as u64; // alloc: Cold
        }
        let head = core::ptr::addr_of_mut!((*cell).head);
        ((*cell).head.op)(head, ctx, OP_GEN_SEEN, img, n) as u64
    }
}

/// The streamed sorted SetOp's text key (lowerv4 ops/setop/stream.rs): setop_compare_slots's ApplySortComparator over two
/// non-NULL text datums under collation `coll`, as the key's sort support compares them (varlena.c `varstrfastcmp_c` under
/// "C" / "POSIX", else the locale's compare: `collation::sort_compare`), each datum in any header form (a short, plain,
/// compressed or external image, detoasted for the call): -1 / 0 / 1, or 2 when a detoast or the compare raised (its
/// error stashed).
pub struct StreamTextCmp;

impl Host for StreamTextCmp {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `a` and `x` are live varlena datums (a row's value, or a held copy behind a 4-byte header).
    unsafe fn run(_ctx: *mut Ctx, (a, x, coll): (u64, u64, u64)) -> u64 {
        let r = gravity_fn::with_payload(a as usize as *const u8, |p| gravity_fn::with_payload(x as usize as *const u8, |q| gravity_fn::typesem::collation::sort_compare(p, q, coll as u32)));
        match r {
            Ok(Ok(Ok(c))) => c.signum() as i64 as u64,
            Ok(Ok(Err(e)) | Err(e)) | Err(e) => {
                gravity_fn::stash_error(e);
                2
            }
        }
    }

    /// A panic in the compare reads as raised.
    fn fail(_a: (u64, u64, u64)) -> u64 {
        2
    }
}

/// The streamed sorted SetOp's text key: the open left group's value, the varlena at `datum` (0: NULL, 0 back), copied as
/// its detoasted payload behind a 4-byte header into the buffer at `buf` (frame words `[ptr, cap]`), which a statement pin
/// grows to the larger of the copy and twice its old capacity (64 bytes at least), so memory is the largest value per key,
/// never per group: the copy's address, or `u64::MAX` when the detoast or the pin failed (its error stashed).
pub struct StreamKeyCopy;

impl Host for StreamKeyCopy {
    type Args = (u64, u64);
    /// # Safety
    /// `buf` addresses two writable frame words (0 / 0, or a pinned buffer and its byte capacity); `datum` is 0 or a live
    /// varlena.
    unsafe fn run(ctx: *mut Ctx, (buf, datum): (u64, u64)) -> u64 {
        if datum == 0 {
            return 0;
        }
        let copy = gravity_fn::with_payload(datum as usize as *const u8, |x| {
            let need = 4 + x.len();
            let (ptr, cap) = (buf as usize as *mut u64, (buf as usize as *mut u64).add(1));
            if (*cap as usize) < need {
                let size = need.max(2 * *cap as usize).max(64);
                let p = gravity_abi::program::pin(ctx, size as u64);
                if p.is_null() {
                    crate::host_abi::pin_refused(size); // D1392 (b)
                    return u64::MAX;
                }
                (*ptr, *cap) = (p as u64, size as u64);
            }
            let d = *ptr as usize as *mut u8;
            (d as *mut u32).write_unaligned((need as u32) << 2);
            core::ptr::copy_nonoverlapping(x.as_ptr(), d.add(4), x.len());
            d as u64
        });
        copy.unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            u64::MAX
        })
    }

    /// A panic reads as a failed copy (the IR raises).
    fn fail(_a: (u64, u64)) -> u64 {
        u64::MAX
    }
}

/// AGA-30: a fixed by-reference key's value (uuid, interval, ...): the `len`-byte image at `datum` (0: NULL, 0 back) copied
/// into the buffer at `buf` (frame words `[ptr, cap]`), which a statement pin grows to the larger of `len` and twice its
/// old capacity (64 bytes at least), as `StreamKeyCopy` grows a varlena's: the copy's address, or `u64::MAX` when the pin
/// is refused (its error stashed).
pub struct StreamFixedCopy;

impl Host for StreamFixedCopy {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `buf` addresses two writable frame words (0 / 0, or a pinned buffer and its byte capacity); `datum` is 0 or a live
    /// image of `len` bytes.
    unsafe fn run(ctx: *mut Ctx, (buf, datum, len): (u64, u64, u64)) -> u64 {
        if datum == 0 {
            return 0;
        }
        let (ptr, cap) = (buf as usize as *mut u64, (buf as usize as *mut u64).add(1));
        if *cap < len {
            let size = len.max(2 * *cap).max(64);
            let p = gravity_abi::program::pin(ctx, size);
            if p.is_null() {
                crate::host_abi::pin_refused(size as usize); // D1392 (b)
                return u64::MAX;
            }
            (*ptr, *cap) = (p as u64, size);
        }
        core::ptr::copy_nonoverlapping(datum as usize as *const u8, *ptr as usize as *mut u8, len as usize);
        *ptr
    }

    /// A panic reads as a failed copy (the IR raises).
    fn fail(_a: (u64, u64, u64)) -> u64 {
        u64::MAX
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;

    use super::{GenCheck, GenSeen, StreamKeyCopy, StreamTextCmp};
    use crate::contract::{entry1, entry2, entry3};

    /// The text key's compare under "C": byte order, a prefix first, equal bytes equal, in any header form (a short
    /// 1-byte header against a plain 4-byte one).
    #[test]
    fn the_text_key_compare_under_c_is_the_byte_order_in_any_header() {
        let plain = |s: &[u8]| [&((s.len() as u32 + 4) << 2).to_le_bytes()[..], s].concat(); // alloc: Cold
        let short = |s: &[u8]| [&[((s.len() as u8 + 1) << 1) | 1][..], s].concat(); // alloc: Cold
        // SAFETY: two live varlena images for the call; the context is not read
        let cmp = |a: &[u8], x: &[u8]| unsafe { entry3::<StreamTextCmp>(0, a.as_ptr() as u64, x.as_ptr() as u64, 950) } as i64;
        assert_eq!(cmp(&plain(b"k0001"), &plain(b"k0002")), -1);
        assert_eq!(cmp(&short(b"k0002"), &plain(b"k0001")), 1);
        assert_eq!(cmp(&short(b"k0001"), &plain(b"k0001")), 0);
        assert_eq!(cmp(&plain(b"k"), &short(b"k0")), -1);
        assert_eq!(cmp(&plain(b"Z"), &plain(b"a")), -1);
        assert_eq!(cmp(&plain(b""), &short(b"")), 0);
    }

    /// The test's `Program.pin`: a zeroed region of its own, never freed (the test's few pins).
    unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
        Box::leak(vec![0u64; (bytes as usize).div_ceil(8)].into_boxed_slice()).as_mut_ptr() as *mut u8 // alloc: Cold
    }

    /// The text key's copy: NULL copies nothing; a short image is copied as a plain one; a larger value regrows the buffer
    /// and a smaller one reuses it; the copy compares equal to its source.
    #[test]
    fn the_text_key_copy_is_a_plain_image_in_a_reused_buffer() {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE); // alloc: Cold
        program.pin = Some(test_pin);
        let mut cx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Cold
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(cx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        let ctx = cx.as_mut_ptr() as u64;
        let short = |s: &[u8]| [&[((s.len() as u8 + 1) << 1) | 1][..], s].concat(); // alloc: Cold
        let mut buf = [0u64; 2];
        let at = buf.as_mut_ptr() as u64;
        // SAFETY: a null datum: nothing is read or written
        assert_eq!(unsafe { entry2::<StreamKeyCopy>(ctx, at, 0) }, 0);
        assert_eq!(buf, [0, 0]);
        let a = short(b"k0001");
        // SAFETY: two live frame words, a live short varlena, a ctx that pins through `test_pin`
        let c = unsafe { entry2::<StreamKeyCopy>(ctx, at, a.as_ptr() as u64) };
        assert!(c != u64::MAX && c == buf[0] && buf[1] >= 64);
        // SAFETY: the copy just made: its 4-byte header and 5 payload bytes
        let img = unsafe { core::slice::from_raw_parts(c as usize as *const u8, 9) };
        assert_eq!(img, [&(9u32 << 2).to_le_bytes()[..], b"k0001"].concat());
        // SAFETY: the copy and its source are live varlenas
        assert_eq!(unsafe { entry3::<StreamTextCmp>(0, c, a.as_ptr() as u64, 950) }, 0);
        let long = [&(204u32 << 2).to_le_bytes()[..], &[b'x'; 200][..]].concat(); // alloc: Cold
        // SAFETY: as above
        let c2 = unsafe { entry2::<StreamKeyCopy>(ctx, at, long.as_ptr() as u64) };
        assert!(c2 != u64::MAX && c2 != c && buf[1] >= 204);
        // SAFETY: as above
        assert_eq!(unsafe { entry2::<StreamKeyCopy>(ctx, at, a.as_ptr() as u64) }, c2);
    }

    /// A null or foreign handle is an error by name, never a call through it (the op itself runs in the e2e witness).
    #[test]
    fn a_null_or_foreign_handle_is_an_error_by_name() {
        let _ = gravity_fn::take_error();
        // SAFETY: a null handle: nothing is read.
        assert_eq!(unsafe { entry1::<GenCheck>(0, 0) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m9_gen_check: a stale or foreign M9 handle 0x0")));
        let foreign = [0u64; 3];
        // SAFETY: a live three-word block whose magic is not the generation object's: only its magic is read.
        assert_eq!(unsafe { entry1::<GenCheck>(0, foreign.as_ptr() as u64) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("stale or foreign")));
    }

    /// D1378: the seen entry refuses a null or foreign handle by name before it reads the image.
    #[test]
    fn the_seen_entry_refuses_a_null_or_foreign_handle_by_name() {
        let _ = gravity_fn::take_error();
        let img = [0u64; 2];
        // SAFETY: a null handle: nothing is read.
        assert_eq!(unsafe { entry3::<GenSeen>(0, 0, img.as_ptr() as u64, 1) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m9_gen_seen: a stale or foreign M9 handle 0x0")));
        let foreign = [0u64; 3];
        // SAFETY: a live three-word block whose magic is not the generation object's: only its magic is read.
        assert_eq!(unsafe { entry3::<GenSeen>(0, foreign.as_ptr() as u64, img.as_ptr() as u64, 1) } as i64, ERROR);
        assert!(gravity_fn::take_error().is_some_and(|e| e.message().contains("m9_gen_seen: a stale or foreign")));
    }
}
