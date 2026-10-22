
use core::ptr::NonNull;

use ::datum::Datum;

use crate::strview::StrViews;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColRep {
    ByvalWord,
    FixedRef { width: u16 },
    Varlena { inline_proven: bool },
    DictCodes(DictCodes),
    Const,
    RowId,
    StrView(StrViews),
}

impl ColRep {
    #[inline]
    pub fn is_base(self) -> bool {
        matches!(
            self,
            ColRep::ByvalWord | ColRep::FixedRef { .. } | ColRep::Varlena { .. } | ColRep::RowId
        )
    }

    #[inline]
    pub fn is_overlay(self) -> bool {
        !self.is_base()
    }

    #[inline]
    pub fn datums_authoritative(self) -> bool {
        self.is_base() || matches!(self, ColRep::StrView(_))
    }

    #[inline]
    pub fn same_base_class(self, other: ColRep) -> bool {
        match (self, other) {
            (ColRep::ByvalWord, ColRep::ByvalWord) | (ColRep::RowId, ColRep::RowId) => true,
            (ColRep::FixedRef { width: a }, ColRep::FixedRef { width: b }) => a == b,
            (ColRep::Varlena { .. }, ColRep::Varlena { .. }) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct DictCodes {
    codes: NonNull<u32>,
    dict: DictHandle,
    epoch: DictEpoch,
    epoch_key: DictEpochKey,
}

impl DictCodes {
    /// # Safety
    ///
    /// The producer asserts the R1–R6 envelope: `codes` points to at least
    /// `nrows` live `u32` codes and the dictionary behind `dict` stays live
    /// and unmodified for the claim scope of the batch this lane is staged
    /// into (worker-private, never crossing threads); every code is
    /// `< dict.space().ncodes()`; the column is all-valid (zero-null-proof
    /// rule, M1) and code-eq == value-eq holds for the column's type class.
    #[inline]
    pub unsafe fn publish(
        codes: NonNull<u32>,
        dict: DictHandle,
        epoch: DictEpoch,
        epoch_key: DictEpochKey,
    ) -> DictCodes {
        DictCodes { codes, dict, epoch, epoch_key }
    }

    #[inline]
    pub fn codes_ptr(self) -> NonNull<u32> {
        self.codes
    }

    /// # Safety
    ///
    /// Caller asserts the publish contract still holds (claim-scoped, R1) and
    /// `nrows` does not exceed the published row count.
    #[inline]
    pub unsafe fn codes<'a>(self, nrows: usize) -> &'a [u32] {
        // SAFETY: publish contract — nrows live codes, unmodified for the
        // claim scope.
        unsafe { core::slice::from_raw_parts(self.codes.as_ptr(), nrows) }
    }

    #[inline]
    pub fn dict(self) -> DictHandle {
        self.dict
    }

    #[inline]
    pub fn epoch(self) -> DictEpoch {
        self.epoch
    }

    #[inline]
    pub fn epoch_key(self) -> DictEpochKey {
        self.epoch_key
    }
}

impl PartialEq for DictCodes {
    fn eq(&self, other: &Self) -> bool {
        self.codes == other.codes && self.dict == other.dict && self.epoch == other.epoch
    }
}

impl Eq for DictCodes {}

impl core::fmt::Debug for DictCodes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DictCodes")
            .field("codes", &self.codes)
            .field("dict", &self.dict)
            .field("epoch", &self.epoch)
            .finish()
    }
}

#[derive(Clone, Copy)]
pub struct DictHandle {
    space: NonNull<dyn DictSpace>,
}

impl DictHandle {
    #[inline]
    pub fn new<T: DictSpace>(space: &T) -> DictHandle {
        let r: &(dyn DictSpace + '_) = space;
        // SAFETY: lifetime erasure only (identical layout) — a handle is
        // not a borrow; liveness is the R1 claim-scope contract, asserted
        // at deref ([`DictHandle::space`]).
        let r: &'static (dyn DictSpace + 'static) = unsafe { core::mem::transmute(r) };
        DictHandle { space: NonNull::from(r) }
    }

    /// # Safety
    ///
    /// Caller asserts the R1–R6 envelope: the provider this handle was
    /// created from is still live and unmodified (claim-scoped,
    /// worker-private). The returned borrow must not outlive the claim.
    #[inline]
    pub unsafe fn space<'a>(&self) -> &'a dyn DictSpace {
        // SAFETY: caller contract — provider live for the claim scope.
        unsafe { self.space.as_ref() }
    }
}

impl PartialEq for DictHandle {
    fn eq(&self, other: &Self) -> bool {
        // Provider identity: data pointer only (vtable ptrs are unstable).
        core::ptr::addr_eq(self.space.as_ptr(), other.space.as_ptr())
    }
}

impl Eq for DictHandle {}

impl core::fmt::Debug for DictHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DictHandle").field("space", &self.space.as_ptr()).finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DictEpoch(pub u64);

pub trait DictSpace {
    fn ncodes(&self) -> u32;

    fn base_rep(&self) -> ColRep;

    fn entry_datum(&self, code: u32) -> Datum;

    fn byte_len(&self, code: u32) -> u32;

    fn char_len(&self, code: u32) -> u32;

    fn code_order_is_value_order(&self) -> bool;
}


#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DictEpochKey {
    pub part_uuid: [u8; 16],
    pub attno: u32,
    pub path_ord: u32,
}

pub const ESCAPE_CODE_U16: u16 = u16::MAX;

pub const ESCAPE_CODE_U32: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeWidth {
    U16,
    U32,
}

#[inline]
pub const fn code_width_for(entry_count: u32) -> CodeWidth {
    if entry_count < ESCAPE_CODE_U16 as u32 {
        CodeWidth::U16
    } else {
        CodeWidth::U32
    }
}
