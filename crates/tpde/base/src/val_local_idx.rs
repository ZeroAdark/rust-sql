// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValLocalIdx(u32);

impl ValLocalIdx {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

const _: () = {
    assert!(core::mem::size_of::<ValLocalIdx>() == 4);
    assert!(core::mem::align_of::<ValLocalIdx>() == core::mem::align_of::<u32>());
    assert!(!core::mem::needs_drop::<ValLocalIdx>());
};

#[cfg(test)]
mod tests {
    use super::ValLocalIdx;

    #[test]
    fn every_bit_and_boundary_preserves_the_index() {
        for bit in 0..32 {
            for value in [1u32 << bit, !(1u32 << bit)] {
                assert_eq!(ValLocalIdx::new(value).get(), value);
            }
        }
        assert_eq!(ValLocalIdx::new(0).get(), 0);
        assert_eq!(ValLocalIdx::new(u32::MAX).get(), u32::MAX);
    }
}
