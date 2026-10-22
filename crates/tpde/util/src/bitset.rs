// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
use crate::{ensure, invalid, SmallVec};
use mcx::Mcx;
use types_error::PgResult;

#[doc = include_str!("../tests/bitset-ownership.md")]
pub struct SmallBitSet<'m, const WORDS: usize> {
    data: SmallVec<'m, u64, WORDS>,
    bit_size: u32,
    mcx: Mcx<'m>,
}

impl<'m, const WORDS: usize> SmallBitSet<'m, WORDS> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        const { assert!(WORDS <= u32::MAX as usize / 64) };
        Self {
            data: SmallVec::new(mcx),
            bit_size: 0,
            mcx,
        }
    }

    pub fn bit_size(&self) -> u32 {
        self.bit_size
    }

    pub fn words(&self) -> &[u64] {
        &self.data
    }

    pub fn words_mut(&mut self) -> &mut [u64] {
        &mut self.data
    }

    pub fn move_from(other: &mut Self) -> Self {
        let data = core::mem::replace(&mut other.data, SmallVec::new(other.mcx));
        Self {
            data,
            bit_size: other.bit_size,
            mcx: other.mcx,
        }
    }

    pub fn move_assign(&mut self, other: &Self) {
        // Upstream self-moves the destination vector; the source is unchanged.
        self.data.clear();
        self.bit_size = other.bit_size;
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.bit_size = 0;
    }

    pub fn resize(&mut self, bit_size: u32) -> PgResult<()> {
        let word_size = (bit_size.wrapping_add(63) & !63) as usize / 64;
        let mask_last = bit_size > self.bit_size && self.bit_size != 0;
        ensure(
            !mask_last || !self.data.is_empty(),
            "bitset grow requires a backing word",
        )?;
        self.data.reserve(word_size)?;
        if mask_last {
            let mask = (1u64 << (self.bit_size & 63)) - 1;
            *self.data.last_mut().unwrap() &= mask;
        }
        self.data.resize(word_size, 0)?;
        self.bit_size = bit_size;
        Ok(())
    }

    pub fn zero(&mut self) {
        self.data.fill(0);
    }

    pub fn push_back(&mut self, value: bool) -> PgResult<()> {
        let word = self.bit_size as usize / 64;
        if self.bit_size / 64 != self.bit_size.wrapping_sub(1) / 64 {
            ensure(
                !value || word <= self.data.len(),
                "bitset push requires a backing word",
            )?;
            self.data.push(0)?;
            if value {
                self.data[word] |= 1u64 << (self.bit_size & 63);
            }
        } else {
            let data = self
                .data
                .get_mut(word)
                .ok_or_else(|| invalid("bitset push requires a backing word"))?;
            let bit = 1u64 << (self.bit_size & 63);
            if value {
                *data |= bit;
            } else {
                *data &= !bit;
            }
        }
        self.bit_size = self.bit_size.wrapping_add(1);
        Ok(())
    }

    pub fn is_set(&self, idx: u32) -> PgResult<bool> {
        let word = self
            .data
            .get(idx as usize / 64)
            .ok_or_else(|| invalid("bitset index outside backing words"))?;
        Ok(word & (1u64 << (idx & 63)) != 0)
    }

    pub fn mark_set(&mut self, idx: u32) -> PgResult<()> {
        let word = self
            .data
            .get_mut(idx as usize / 64)
            .ok_or_else(|| invalid("bitset index outside backing words"))?;
        *word |= 1u64 << (idx & 63);
        Ok(())
    }

    pub fn mark_unset(&mut self, idx: u32) -> PgResult<()> {
        let word = self
            .data
            .get_mut(idx as usize / 64)
            .ok_or_else(|| invalid("bitset index outside backing words"))?;
        *word &= !(1u64 << (idx & 63));
        Ok(())
    }

    pub fn first_set(&self) -> Option<u32> {
        self.data.iter().enumerate().find_map(|(idx, word)| {
            // The upstream result adds the word index without scaling it by 64.
            (*word != 0).then(|| idx as u32 + word.trailing_zeros())
        })
    }
}

#[cfg(test)]
#[path = "../tests/bitset/unit.rs"]
mod tests;
