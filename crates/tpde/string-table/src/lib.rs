// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]

mod inline;

use inline::InlineBytes;
use mcx::{ArenaForget, Mcx, PgVec};
use types_error::PgResult;

pub struct StringTable<'mcx> {
    inline: InlineBytes,
    spill: ArenaForget<PgVec<'mcx, u8>>,
}

impl<'mcx> StringTable<'mcx> {
    /// ```compile_fail
    /// let mut context = mcx::MemoryContext::new_bump("compiler");
    /// let table = tpde_string_table::StringTable::new(context.mcx());
    /// context.reset();
    /// assert_eq!(table.size(), 1);
    /// ```
    pub fn new(mcx: Mcx<'mcx>) -> Self {
        Self {
            inline: InlineBytes::new(),
            spill: ArenaForget::new(mcx::vec_new_in(mcx)),
        }
    }

    pub fn size(&self) -> usize {
        if self.spill.capacity() == 0 {
            self.inline.as_slice().len()
        } else {
            self.spill.len()
        }
    }

    pub fn data(&self) -> &[u8] {
        if self.spill.capacity() == 0 {
            self.inline.as_slice()
        } else {
            &self.spill
        }
    }

    pub fn add(&mut self, bytes: &[u8]) -> PgResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.add_prefix(&[], bytes)
    }

    pub fn add_prefix(&mut self, prefix: &[u8], bytes: &[u8]) -> PgResult<usize> {
        let offset = self.size();
        let end = appended_size(offset, prefix.len(), bytes.len())
            .ok_or_else(|| self.spill.allocator().oom(usize::MAX))?;
        if end <= InlineBytes::CAPACITY {
            self.inline.append(prefix, bytes);
        } else {
            self.reserve(end)?;
            mcx::vec_append_bytes(&mut self.spill, prefix)?;
            mcx::vec_append_bytes(&mut self.spill, bytes)?;
            self.spill.push(0);
        }
        Ok(offset)
    }

    fn reserve(&mut self, end: usize) -> PgResult<()> {
        if end <= self.spill.capacity() {
            return Ok(());
        }
        let capacity = self.spill.capacity().max(InlineBytes::CAPACITY);
        let capacity = capacity
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .map(|n| n.max(end))
            .ok_or_else(|| self.spill.allocator().oom(end))?;
        let additional = capacity - self.spill.len();
        self.spill
            .try_reserve_exact(additional)
            .map_err(|_| self.spill.allocator().oom(capacity))?;
        if self.spill.is_empty() {
            mcx::vec_append_bytes(&mut self.spill, self.inline.as_slice())?;
        }
        Ok(())
    }
}

fn appended_size(offset: usize, prefix: usize, bytes: usize) -> Option<usize> {
    offset
        .checked_add(prefix)?
        .checked_add(bytes)?
        .checked_add(1)
}

const _: () = {
    assert!(core::mem::size_of::<StringTable<'static>>() <= 128);
    assert!(!core::mem::needs_drop::<StringTable<'static>>());
};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
