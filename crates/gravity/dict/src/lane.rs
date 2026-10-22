//! The derived-lane hook (abi-v2 §2.2 `Site.lane`, milestone-1b addition b): a table's header
//! published as a `Lane` of kind `ID_TABLE`, read back by a kernel with `from_lane`.
use gravity_abi::ctx::lane as lf;
use gravity_abi::{word, Lane, PtrWord};

use crate::table::{Deferred, Header};
use crate::table::{IdxLen, LEN_MODE_BYTES};
use crate::{Bits, Derived, Fp128, Hash, I64};

pub const ELEM_BITS: u8 = 0;
pub const ELEM_I64: u8 = 1;
pub const ELEM_DERIVED: u8 = 2;
pub const ELEM_HASH: u8 = 3;
pub const ELEM_FP128: u8 = 4;
/// A length read from the dictionary index: the byte length, or (`_DELTA`) `byte_len − char_field`.
pub const ELEM_LEN_IDX: u8 = 5;
pub const ELEM_LEN_IDX_DELTA: u8 = 6;

/// Lane `k` of a site as the site's `ordinal`-th table over code lane `over`: `ptr` the header,
/// `ncodes` the dictionary's, `aux` = over | elem << 8.
/// # Safety — `hdr` is a table header.
pub unsafe fn publish(hdr: *const Header, ordinal: u32, over: u8, out: &mut Lane) {
    *out = Lane {
        ptr: word(hdr as *const u8),
        valid: word(core::ptr::null()),
        width: 8,
        kind: lf::KIND_BYVAL,
        region: lf::REGION_DICTIONARY,
        flags: lf::FLAG_ALL_VALID,
        ncodes_or_scale: (*hdr).ncodes,
        id: lf::id(lf::ID_TABLE, ordinal),
        aux: over as u32 | ((*hdr).elem as u32) << 8,
    };
}

pub fn is_table(l: &Lane) -> bool {
    lf::id_kind(l.id) == lf::ID_TABLE && !l.ptr.ptr().is_null()
}

pub fn elem(l: &Lane) -> u8 {
    (l.aux >> 8) as u8
}

/// The code lane the table is indexed by.
pub fn over(l: &Lane) -> u8 {
    l.aux as u8
}

/// The kernel's return for a gather that selected a faulting entry: the driver re-evaluates it.
pub fn deferred(site: u8, lane: u8, d: Deferred) -> i64 {
    gravity_abi::status::fault(site, lane, d.code)
}

unsafe fn header(l: &Lane, elem: u8) -> Option<*const Header> {
    if !is_table(l) || self::elem(l) != elem {
        return None;
    }
    Some(l.ptr.ptr() as *const Header)
}

impl Bits {
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<Bits> {
        header(l, ELEM_BITS).map(|h| Bits::at(h))
    }
}

impl I64 {
    /// A built table or an index view (`index_len_header`), read alike.
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<I64> {
        if !is_table(l) || !matches!(self::elem(l), ELEM_I64 | ELEM_LEN_IDX | ELEM_LEN_IDX_DELTA) {
            return None;
        }
        Some(I64::at(l.ptr.ptr() as *const Header))
    }
}

impl Hash {
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<Hash> {
        header(l, ELEM_HASH).map(|h| Hash::at(h))
    }
}

impl Fp128 {
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<Fp128> {
        header(l, ELEM_FP128).map(|h| Fp128::at(h))
    }
}

impl Derived {
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<Derived> {
        header(l, ELEM_DERIVED).map(|h| Derived::at(h))
    }
}

impl IdxLen<false> {
    /// The byte-length view alone (the monomorphic loop's source).
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<IdxLen<false>> {
        header(l, ELEM_LEN_IDX).filter(|h| (**h).pad[0] == LEN_MODE_BYTES).map(|h| IdxLen::at(h))
    }
}

impl IdxLen<true> {
    /// # Safety — a lane the source filled through `publish`.
    pub unsafe fn from_lane(l: &Lane) -> Option<IdxLen<true>> {
        header(l, ELEM_LEN_IDX_DELTA).map(|h| IdxLen::at(h))
    }
}
