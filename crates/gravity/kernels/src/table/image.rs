//! A key image on the stack, 16-byte aligned (D4684, D4691; lint RY): a plain `[u64; N]` takes whatever 8-byte slot its
//! frame gives it, and at an 8-mod-16 slot one of the 16-byte pair stores that write it straddles a 4 KiB page at one
//! stack position in 256; every reload of the image (the hash, a probe's compare) then waits on the split store (D4684:
//! +38 % cycles on a count top's walk after a 32-byte move of every thread's stack top). Aligned to 16, no store of it
//! crosses a page.

/// `N` key words, 16-byte aligned; read as a slice through `AsRef` (or `.0`).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct AlignedImage<const N: usize>(pub [u64; N]);

impl<const N: usize> AsRef<[u64]> for AlignedImage<N> {
    #[inline(always)]
    fn as_ref(&self) -> &[u64] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::AlignedImage;

    #[test]
    fn images_are_16_byte_aligned() {
        let a = (core::mem::align_of::<AlignedImage<2>>(), core::mem::align_of::<AlignedImage<3>>(), core::mem::align_of::<AlignedImage<9>>());
        assert_eq!(a, (16, 16, 16));
        let img = AlignedImage([7u64, 8, 0]);
        assert_eq!((&img as *const AlignedImage<3> as usize % 16, img.as_ref()), (0, &[7u64, 8, 0][..]));
    }
}
