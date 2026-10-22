// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::ir_adaptor::IrAdaptor;

pub(super) struct MetadataRestore<'a, A: IrAdaptor>
where
    A::BlockRef: Copy,
{
    pub adaptor: &'a A,
    pub saved: &'a [(A::BlockRef, u32, u32)],
}
impl<A: IrAdaptor> Drop for MetadataRestore<'_, A>
where
    A::BlockRef: Copy,
{
    fn drop(&mut self) {
        for &(block, info, info2) in self.saved {
            self.adaptor.block_set_info(block, info);
            self.adaptor.block_set_info2(block, info2);
        }
    }
}
