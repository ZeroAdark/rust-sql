// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::mem::MaybeUninit;

pub(super) struct InlineBytes {
    bytes: [MaybeUninit<u8>; Self::CAPACITY],
    len: usize,
}

impl InlineBytes {
    pub(super) const CAPACITY: usize = 24;

    pub(super) fn new() -> Self {
        let mut bytes = [MaybeUninit::uninit(); Self::CAPACITY];
        bytes[0].write(0);
        Self { bytes, len: 1 }
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: new/append initialize exactly the live prefix; mutation requires &mut self.
        unsafe { core::slice::from_raw_parts(self.bytes.as_ptr().cast::<u8>(), self.len) }
    }

    pub(super) fn append(&mut self, prefix: &[u8], bytes: &[u8]) {
        let end = super::appended_size(self.len, prefix.len(), bytes.len())
            .filter(|&n| n <= Self::CAPACITY)
            .expect("inline string table capacity");
        // SAFETY: checked end fits the array; safe borrows keep both sources disjoint
        // SAFETY: from self. Copies and the terminator initialize the new live prefix.
        unsafe {
            let dst = self.bytes.as_mut_ptr().cast::<u8>().add(self.len);
            core::ptr::copy_nonoverlapping(prefix.as_ptr(), dst, prefix.len());
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(prefix.len()), bytes.len());
            dst.add(prefix.len() + bytes.len()).write(0);
        }
        self.len = end;
    }
}
