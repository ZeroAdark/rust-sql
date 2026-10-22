
use ::datum::Datum;
use ::types_tuple::tupdesc::CompactAttribute;
use ::types_tuple::tupmacs::fetch_att;
use ::types_tuple::varatt;

use crate::rep::ColRep;
use crate::selection::Selection;
use crate::batch::Column;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingClass {
    ByvalWord { width: u8 },
    FixedRef { width: u16 },
    VarlenaPtr,
}

impl StagingClass {
    pub fn from_catalog(typlen: i16, typbyval: bool) -> Option<StagingClass> {
        if typbyval {
            match typlen {
                1 | 2 | 4 | 8 => Some(StagingClass::ByvalWord { width: typlen as u8 }),
                _ => None,
            }
        } else if typlen > 0 {
            Some(StagingClass::FixedRef { width: typlen as u16 })
        } else if typlen == -1 {
            Some(StagingClass::VarlenaPtr)
        } else {
            None
        }
    }

    pub fn from_attr(att: &CompactAttribute) -> Option<StagingClass> {
        StagingClass::from_catalog(att.attlen, att.attbyval)
    }

    #[inline]
    pub fn base_rep(self) -> ColRep {
        match self {
            StagingClass::ByvalWord { .. } => ColRep::ByvalWord,
            StagingClass::FixedRef { width } => ColRep::FixedRef { width },
            StagingClass::VarlenaPtr => ColRep::Varlena { inline_proven: false },
        }
    }

    /// # Safety
    ///
    /// `p` points to live tuple data of this class's shape, readable for
    /// the value's extent, outliving the batch per R1–R6.
    #[inline]
    pub unsafe fn stage(self, p: *const u8) -> Datum {
        match self {
            // SAFETY: caller contract.
            StagingClass::ByvalWord { width } => unsafe { fetch_att(p, true, width as i32) },
            StagingClass::FixedRef { .. } | StagingClass::VarlenaPtr => {
                Datum::from_usize(p as usize)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarlenaForm {
    Inline4B,
    Short,
    CompressedInline,
    ExternalOndisk,
    ExternalIndirect,
    ExternalExpanded,
}

impl VarlenaForm {
    #[inline]
    pub fn is_plain_inline(self) -> bool {
        matches!(self, VarlenaForm::Inline4B | VarlenaForm::Short)
    }

    #[inline]
    pub fn needs_detoast(self) -> bool {
        !self.is_plain_inline()
    }
}

///
/// # Safety
///
/// `p` points to a live varlena image's first byte (plus the tag byte for
/// external forms — every external image is ≥ 2 bytes).
#[inline]
pub unsafe fn classify_varlena(p: *const u8) -> VarlenaForm {
    // SAFETY: caller contract for every header peek below.
    unsafe {
        if varatt::varatt_is_1b_e(p) {
            match varatt::vartag_external(p) {
                varatt::VARTAG_ONDISK => VarlenaForm::ExternalOndisk,
                varatt::VARTAG_INDIRECT => VarlenaForm::ExternalIndirect,
                tag if varatt::vartag_is_expanded(tag) => VarlenaForm::ExternalExpanded,
                tag => unrecognized_vartag(tag),
            }
        } else if varatt::varatt_is_1b(p) {
            VarlenaForm::Short
        } else if varatt_is_4b_c(p) {
            VarlenaForm::CompressedInline
        } else {
            VarlenaForm::Inline4B
        }
    }
}

#[cold]
#[inline(never)]
fn unrecognized_vartag(tag: u8) -> ! {
    panic!("unrecognized TOAST vartag: {tag}")
}

/// # Safety
///
/// As [`varatt::varatt_is_1b`].
#[inline]
pub(crate) unsafe fn varatt_is_4b_c(p: *const u8) -> bool {
    // SAFETY: caller contract.
    #[cfg(target_endian = "little")]
    return unsafe { (*p & 0x03) == 0x02 };
    #[cfg(target_endian = "big")]
    return unsafe { (*p & 0xC0) == 0x40 };
}

impl Column {
    /// # Safety
    ///
    /// The column is staged per R1–R6: every selected valid datum points to
    /// a live varlena image (header readable).
    pub unsafe fn prove_inline(&mut self, sel: &Selection) -> bool {
        debug_assert!(matches!(self.base, ColRep::Varlena { .. }));
        for &pos in sel.as_slice() {
            let row = pos as usize;
            if !self.validity.is_valid(row) {
                continue;
            }
            let p = self.datums[row].as_usize() as *const u8;
            // SAFETY: caller contract — live image header.
            if unsafe { classify_varlena(p) }.needs_detoast() {
                return false;
            }
        }
        self.rep = ColRep::Varlena { inline_proven: true };
        true
    }
}
