//! rep/packednum's unsafe host entries, the module's one home of unsafe code: a packed numeric's image and mantissa,
//! moved unchanged from the files named on each module below, which import them back, so the addresses the IR takes,
//! host names and IR are unchanged.
#![allow(unsafe_code)]

/// `datum.rs`'s entries.
pub(crate) mod datum {
    use super::super::datum::NUM_IMAGE_BYTES;

    gravity_kernels::contract::host! {
        /// The image of mantissa `m` at `scale` written at `out` (`NUM_IMAGE_BYTES`), its address; 0 only if the image cannot be
        /// made, which a column's typmod scale never causes. # Safety — `out` is writable for `NUM_IMAGE_BYTES`.
        pub(crate) unsafe fn num_image(m: i64, scale: i64, out: u64) -> u64 {
            let buf = core::slice::from_raw_parts_mut(out as usize as *mut u8, NUM_IMAGE_BYTES as usize);
            let (mut r, mut e) = (gravity_fn::Region::over(buf), gravity_fn::ErrSlot::NONE);
            let ret = gravity_fn::agg_final::num_word(&mut e, &mut r, m, scale);
            drop(e.take());
            if ret.status == gravity_fn::OK { ret.val as u64 } else { 0 }
        }

        /// The order word of the stored image at `p` at `scale`. # Safety — `p` is a live numeric varlena.
        pub(crate) unsafe fn num_mantissa(p: u64, scale: i64) -> i64 {
            let mut e = gravity_fn::ErrSlot::NONE;
            gravity_fn::agg_final::num_mantissa(&mut e, p as usize as *const u8, scale).val
        }
    }
}
