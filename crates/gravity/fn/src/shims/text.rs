//! Hand adapters over the text bodies: where the typed body wants a raw varlena image, an
//! `FmgrInfo` scratch, or writes its result into a caller buffer — and `textcat`, which is a
//! copy and needs no scratch at all. Signatures follow the family convention (`e`, `r` for a
//! by-reference result, `coll` when an argument is collatable, then the SQL arguments).

use core::cell::RefCell;

use adt_like::IcScratch;

use crate::{fail, ok, region_cstring, region_copy, region_payload, region_payload2, with_scratch, Addr, ArgKind, ErrSlot, Family, RawFn, Region, Ret, Shim, View};

pub extern "C-unwind" fn textcat(_e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: View, a1: View) -> Ret {
    region_payload2(r, a0.bytes(), a1.bytes())
}

/// `text_substring` over the argument's raw image (`ArgKind::D`: the Datum as stored, any header form), as PostgreSQL's
/// text_substr passes its argument: a compressed or external value is fetched through `detoast_attr_slice` only as far as
/// the substring reaches (DatumGetTextPSlice), never detoasted whole; a short or plain one is read in place.
fn substr(e: *mut ErrSlot, r: *mut Region, d: i64, start: i32, len: i32, no_len: bool) -> Ret {
    let image = super::raw::varlena_image(d);
    with_scratch(|mcx| match varlena::text_substring(mcx, image, start, len, no_len) {
        Ok(v) => region_copy(r, v.as_bytes()),
        Err(err) => fail(e, err),
    })
}

/// D3151: PostgreSQL's text_substring over a plain text's payload `p` (lowerv4 road text.eq's cold view, rep/varlena
/// text_view.rs), the result's payload handed to `f`: its errors (a negative count) are text_substring's own. The whole
/// value's re-headered copy is charged to `owner` for its life (CLASS E: a memledger `ChargedVec`, filled as the plain
/// `Vec` was; `None`: under the caller's floor or no owner, uncharged).
pub fn substring_of<T>(p: &[u8], start: i32, len: i32, no_len: bool, owner: Option<(memledger::Owner, memledger::Kind)>, f: impl FnOnce(&[u8]) -> T) -> types_error::PgResult<T> {
    let mut img = memledger::ChargedVec::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1)); // alloc: Cold charged owner
    img.with_vec(|v| {
        v.reserve_exact(p.len() + 4); // alloc: Cold charged owner
        v.extend_from_slice(&((p.len() as u32 + 4) << 2).to_le_bytes()); // alloc: Cold charged owner
        v.extend_from_slice(p); // alloc: Cold charged owner
    });
    let mut cx = mcx::MemoryContext::new_bump("gravity_fn substring_of"); // alloc: Cold
    let out = varlena::text_substring(cx.mcx(), img.as_slice(), start, len, no_len).map(|v| f(v.data()));
    cx.reset();
    out
}

pub extern "C-unwind" fn text_substr(e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: i64, a1: i64, a2: i64) -> Ret {
    substr(e, r, a0, a1 as i32, a2 as i32, false)
}

pub extern "C-unwind" fn text_substr_no_len(e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: i64, a1: i64) -> Ret {
    substr(e, r, a0, a1 as i32, -1, true)
}

/// D4822 (lowerv4 road text.substr.ascii, default off): text_substring's result payload taken by bytes, `None` where
/// the walk must run. When every byte the result reaches through (the text's first `start + count - 1` bytes, all of
/// them without a count) is in 0x01..=0x7F, each is one character in every server encoding (PostgreSQL's server
/// encodings are ASCII-safe) and the walk, its validation included, cannot raise, so the characters are the bytes:
/// `start` below 1 counts from 1, a count past the text's end stops at it, a start past it (or `start + count <= 1`) is
/// the empty text. `None`: a compressed or external image (the slice fetch), a negative count (PostgreSQL's error), a
/// `start + count` past i32, any other byte before the result's end.
pub fn ascii_slice(image: &[u8], start: i32, len: i32, no_len: bool) -> Option<&[u8]> {
    if image[0] == 0x01 || (image[0] & 0x03) == 0x02 {
        return None;
    }
    let data = if image[0] & 0x01 == 0x01 {
        &image[1..((image[0] >> 1) & 0x7F) as usize]
    } else {
        let word = u32::from_ne_bytes([image[0], image[1], image[2], image[3]]);
        &image[4..types_tuple::varatt::varsize_4b_word(word) as usize]
    };
    let end = if no_len {
        data.len()
    } else {
        if len < 0 {
            return None;
        }
        let e = start.checked_add(len)?;
        if e <= 1 {
            return Some(&[]);
        }
        ((e - 1) as usize).min(data.len())
    };
    let head = &data[..end];
    if !head.iter().all(|&b| b.wrapping_sub(1) < 0x7F) {
        return None;
    }
    let s0 = (start.max(1) - 1) as usize;
    Some(if s0 >= end { &[] } else { &head[s0..] })
}

/// D4822: `substr` with `ascii_slice` first (the result written as a fresh 4-byte-header text, as text_substring's).
fn substr_ascii(e: *mut ErrSlot, r: *mut Region, d: i64, start: i32, len: i32, no_len: bool) -> Ret {
    match ascii_slice(super::raw::varlena_image(d), start, len, no_len) {
        Some(p) => region_payload(r, p),
        None => substr(e, r, d, start, len, no_len),
    }
}

/// D4822: text_substr's twin under lowerv4 road text.substr.ascii (catalog.rs `TEXT_SUBSTR_ASCII`).
pub extern "C-unwind" fn text_substr_ascii(e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: i64, a1: i64, a2: i64) -> Ret {
    substr_ascii(e, r, a0, a1 as i32, a2 as i32, false)
}

/// D4822: text_substr_no_len's twin under lowerv4 road text.substr.ascii (catalog.rs `TEXT_SUBSTR_NO_LEN_ASCII`).
pub extern "C-unwind" fn text_substr_no_len_ascii(e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: i64, a1: i64) -> Ret {
    substr_ascii(e, r, a0, a1 as i32, -1, true)
}

/// D4822: the twins gravity_expr binds in place of text_substr (oids 877, 936) and text_substr_no_len (883, 937) while a
/// lowering runs with lowerv4 road text.substr.ascii on (`ascii_twin`); never in `SHIMS` (no catalog row of their own).
pub static TEXT_SUBSTR_ASCII: Shim = Shim { oid: 877, name: "text_substr_ascii", family: Family::ByRefRef, nargs: 3, coll: true, region: true, args: &[ArgKind::D, ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(unsafe { core::mem::transmute::<extern "C-unwind" fn(*mut ErrSlot, *mut Region, u32, i64, i64, i64) -> Ret, RawFn>(text_substr_ascii) }), call: |e, r, coll, _ty, a| text_substr_ascii(e, r, coll, a[0].v(), a[1].v(), a[2].v()) };

/// D4822: text_substr_no_len's twin (`TEXT_SUBSTR_ASCII`).
pub static TEXT_SUBSTR_NO_LEN_ASCII: Shim = Shim { oid: 883, name: "text_substr_no_len_ascii", family: Family::ByRefRef, nargs: 2, coll: true, region: true, args: &[ArgKind::D, ArgKind::V], tyargs: &[], addr: Addr(unsafe { core::mem::transmute::<extern "C-unwind" fn(*mut ErrSlot, *mut Region, u32, i64, i64) -> Ret, RawFn>(text_substr_no_len_ascii) }), call: |e, r, coll, _ty, a| text_substr_no_len_ascii(e, r, coll, a[0].v(), a[1].v()) };

/// D4822: a catalog shim's ASCII twin (`TEXT_SUBSTR_ASCII`, `TEXT_SUBSTR_NO_LEN_ASCII`), the shim itself for any other.
pub fn ascii_twin(s: &'static Shim) -> &'static Shim {
    match s.name {
        "text_substr" => &TEXT_SUBSTR_ASCII,
        "text_substr_no_len" => &TEXT_SUBSTR_NO_LEN_ASCII,
        _ => s,
    }
}

pub extern "C-unwind" fn textlike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    match adt_like::textlike(a0.bytes(), a1.bytes(), coll) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn textnlike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    match adt_like::textnlike(a0.bytes(), a1.bytes(), coll) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

thread_local! {
    // alloc: Node — the ILIKE case-folding scratch the wrapper keeps in its FmgrInfo.
    static IC: RefCell<IcScratch> = RefCell::new(IcScratch::default());
}

fn iclike(e: *mut ErrSlot, coll: u32, s: View, p: View, negate: bool) -> Ret {
    IC.with(|ic| {
        let mut ic = ic.borrow_mut();
        with_scratch(|mcx| match adt_like::generic_text_ic_like(mcx, s.bytes(), p.bytes(), coll, &mut ic) {
            Ok(v) => ok(((v == adt_like::LIKE_TRUE) != negate) as i64),
            Err(err) => fail(e, err),
        })
    })
}

pub extern "C-unwind" fn texticlike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    iclike(e, coll, a0, a1, false)
}

pub extern "C-unwind" fn texticnlike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    iclike(e, coll, a0, a1, true)
}

fn name_iclike(e: *mut ErrSlot, coll: u32, s: View, p: View, negate: bool) -> Ret {
    IC.with(|ic| {
        let mut ic = ic.borrow_mut();
        with_scratch(|mcx| match adt_like::nameiclike(mcx, &s.name64(), p.bytes(), coll, &mut ic) {
            Ok(v) => ok((v != negate) as i64),
            Err(err) => fail(e, err),
        })
    })
}

pub extern "C-unwind" fn nameiclike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    name_iclike(e, coll, a0, a1, false)
}

pub extern "C-unwind" fn nameicnlike(e: *mut ErrSlot, coll: u32, a0: View, a1: View) -> Ret {
    name_iclike(e, coll, a0, a1, true)
}

/// An int4 result, so sign-extended (D2311: the emitted code reads an int4 word sign-extended; the fmgr wrapper's
/// `UInt32GetDatum` word is zero-extended, which C reads through DatumGetInt32).
pub extern "C-unwind" fn hashtext(e: *mut ErrSlot, coll: u32, a0: View) -> Ret {
    match varlena::hashtext_bytes(coll, a0.bytes()) {
        Ok(h) => ok(h as i32 as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn hashtextextended(e: *mut ErrSlot, coll: u32, a0: View, a1: i64) -> Ret {
    match varlena::builtins::hashtext_nondeterministic(coll, a0.bytes(), Some(a1 as u64)) {
        Ok(Some(h)) => ok(h as i64),
        Ok(None) => ok(hashfn::hash_bytes_extended(a0.bytes(), a1 as u64) as i64),
        Err(err) => fail(e, err),
    }
}

macro_rules! out_fns {
    ($($name:ident: $body:path, $decode:expr, $n:literal;)*) => {$(
        pub extern "C-unwind" fn $name(_e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
            let mut buf = [0u8; $n];
            let len = $body($decode(a0), &mut buf);
            region_cstring(r, &buf[..len])
        }
    )*};
}

out_fns! {
    int2out: adt_int::int2out, |a| a as i16, 8;
    int4out: adt_int::int4out, |a| a as i32, 12;
    int8out: adt_int8::int8out, |a| a, 24;
    float4out: adt_float::float4out, |a| f32::from_bits(a as u32), 32;
    float8out: adt_float::float8out, |a| f64::from_bits(a as u64), 32;
}

pub extern "C-unwind" fn boolout(_e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
    region_cstring(r, &[adt_bool::boolout(a0 != 0)])
}

#[cfg(test)]
mod ascii_tests {
    use super::ascii_slice;

    /// PostgreSQL's substring over an ASCII text, by characters: positions `[max(start, 1), start + len)` (no count: to
    /// the end), clipped to the text; `start + len <= 1` is empty.
    fn reference(s: &str, start: i32, len: i32, no_len: bool) -> String {
        let end = if no_len { i64::MAX } else { start as i64 + len as i64 };
        let from = start.max(1) as i64;
        s.chars().enumerate().filter(|(i, _)| (*i as i64 + 1) >= from && ((*i as i64) + 1) < end).map(|(_, c)| c).collect()
    }

    fn short(p: &[u8]) -> Vec<u8> {
        let mut v = vec![(((p.len() + 1) as u8) << 1) | 1]; // alloc: Cold
        v.extend_from_slice(p);
        v
    }

    fn long(p: &[u8]) -> Vec<u8> {
        let mut v = (((p.len() as u32) + 4) << 2).to_le_bytes().to_vec();
        v.extend_from_slice(p);
        v
    }

    #[test]
    fn ascii_bytes_are_the_characters() {
        for s in ["", "a", "abc", "Conventional childr", "warehouse number one, long", "0123456789012345678901234"] {
            for img in [short(s.as_bytes()), long(s.as_bytes())] {
                for start in -3..=30 {
                    for len in 0..=30 {
                        let want = reference(s, start, len, false);
                        assert_eq!(ascii_slice(&img, start, len, false), Some(want.as_bytes()), "{s:?} from {start} for {len}");
                    }
                    let want = reference(s, start, 0, true);
                    assert_eq!(ascii_slice(&img, start, 0, true), Some(want.as_bytes()), "{s:?} from {start}");
                }
            }
        }
    }

    #[test]
    fn the_walk_runs_where_it_must() {
        let s = "abc\u{e9}def".as_bytes();
        for img in [short(s), long(s)] {
            assert_eq!(ascii_slice(&img, 1, 3, false), Some(&b"abc"[..]), "a multibyte character past the result's end is not read");
            assert_eq!(ascii_slice(&img, 2, 3, false), None, "one before it is");
            assert_eq!(ascii_slice(&img, 5, 2, false), None, "characters before the start are counted, so they must be ASCII");
            assert_eq!(ascii_slice(&img, 1, 0, true), None, "no count reads the whole text");
            assert_eq!(ascii_slice(&img, -5, 6, false), Some(&b""[..]), "start + count <= 1 is the empty text, unread");
        }
        assert_eq!(ascii_slice(&short(b"a\0b"), 1, 3, false), None, "a NUL byte is left to the walk's validation");
        assert_eq!(ascii_slice(&short(b"abc"), 1, -1, false), None, "a negative count is PostgreSQL's error");
        assert_eq!(ascii_slice(&short(b"abc"), i32::MAX, 5, false), None, "start + count past i32");
        assert_eq!(ascii_slice(&[0x01, 18, 0, 0, 0, 0, 0, 0, 0, 0], 1, 2, false), None, "an external pointer");
        assert_eq!(ascii_slice(&[0x12, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0], 1, 2, false), None, "a compressed inline image");
    }
}
