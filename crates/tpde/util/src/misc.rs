// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{ensure, invalid};
use types_error::PgResult;
mod sealed {
    pub trait Integer {}
    pub trait Unsigned {}
}
pub trait Integer: sealed::Integer + Copy {
    fn align_down(self, align: Self) -> PgResult<Self>;
    fn align_up(self, align: Self) -> PgResult<Self>;
}
macro_rules! integers {
    ($($t:ty),*) => {
        $(
            impl sealed::Integer for $t {}
            impl Integer for $t {
                fn align_down(self, align: Self) -> PgResult<Self> {
                    ensure(align > 0 && (align & (align - 1)) == 0, "invalid alignment")?;
                    Ok(self & !(align - 1))
                }
                fn align_up(self, align: Self) -> PgResult<Self> {
                    ensure(align > 0 && (align & (align - 1)) == 0, "invalid alignment")?;
                    let sum = self.checked_add(align - 1).ok_or_else(|| invalid("alignment overflow"))?;
                    Ok(sum & !(align - 1))
                }
            }
        )*
    };
}
integers!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
pub fn align_down<T: Integer>(value: T, align: T) -> PgResult<T> {
    value.align_down(align)
}
pub fn align_up<T: Integer>(value: T, align: T) -> PgResult<T> {
    value.align_up(align)
}
pub trait Unsigned: sealed::Unsigned + Copy {
    fn trailing(self) -> PgResult<Self>;
    fn leading(self) -> PgResult<Self>;
}
macro_rules! unsigned {
    ($($t:ty),*) => {
        $(
            impl sealed::Unsigned for $t {}
            impl Unsigned for $t {
                fn trailing(self) -> PgResult<Self> {
                    ensure(self != 0, "zero has no set bit")?;
                    Ok(self.trailing_zeros() as Self)
                }
                fn leading(self) -> PgResult<Self> {
                    ensure(self != 0, "zero has no set bit")?;
                    Ok(self.leading_zeros() as Self)
                }
            }
        )*
    };
}
unsigned!(u8, u16, u32, u64);
pub fn cnt_tz<T: Unsigned>(value: T) -> PgResult<T> {
    value.trailing()
}
pub fn cnt_lz<T: Unsigned>(value: T) -> PgResult<T> {
    value.leading()
}
pub fn zext(value: u64, bits: u32) -> PgResult<u64> {
    ensure(bits > 0 && bits < 64, "invalid extension width")?;
    Ok(value & ((1u64 << bits) - 1))
}
pub fn sext(value: u64, bits: u32) -> PgResult<i64> {
    ensure(bits > 0 && bits < 64, "invalid extension width")?;
    Ok(((value << (64 - bits)) as i64) >> (64 - bits))
}
pub const fn uleb_len(value: u64) -> usize {
    if value == 0 {
        1
    } else {
        (64 - value.leading_zeros() as usize + 6) / 7
    }
}
pub fn uleb_write(dst: &mut [u8], mut value: u64) -> PgResult<usize> {
    let len = uleb_len(value);
    ensure(dst.len() >= len, "short ULEB buffer")?;
    for byte in &mut dst[..len] {
        *byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            *byte |= 0x80;
        }
    }
    Ok(len)
}
pub const fn sleb_len(mut value: i64) -> usize {
    let mut len = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        len += 1;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            return len;
        }
    }
}
pub fn sleb_write(dst: &mut [u8], mut value: i64) -> PgResult<usize> {
    let len = sleb_len(value);
    ensure(dst.len() >= len, "short SLEB buffer")?;
    for (i, byte) in dst[..len].iter_mut().enumerate() {
        *byte = (value & 0x7f) as u8;
        value >>= 7;
        if i + 1 < len {
            *byte |= 0x80;
        }
    }
    Ok(len)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitSetIterator<const REVERSE: bool = false> {
    set: u64,
}
impl<const REVERSE: bool> BitSetIterator<REVERSE> {
    pub const fn new(set: u64) -> Self {
        Self { set }
    }
    pub const fn begin(self) -> Self {
        self
    }
    pub const fn end(self) -> Self {
        Self::new(0)
    }
    pub fn current(&self) -> PgResult<u64> {
        ensure(self.set != 0, "dereferencing exhausted bit iterator")?;
        Ok(if REVERSE {
            63 - self.set.leading_zeros() as u64
        } else {
            self.set.trailing_zeros() as u64
        })
    }
    pub fn advance(&mut self) -> PgResult<()> {
        let bit = self.current()?;
        if REVERSE {
            self.set &= !(1u64 << bit);
        } else {
            self.set &= self.set - 1;
        }
        Ok(())
    }
}
impl<const REVERSE: bool> Iterator for BitSetIterator<REVERSE> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        if self.set == 0 {
            return None;
        }
        let bit = if REVERSE {
            63 - self.set.leading_zeros() as u64
        } else {
            self.set.trailing_zeros() as u64
        };
        if REVERSE {
            self.set &= !(1u64 << bit);
        } else {
            self.set &= self.set - 1;
        }
        Some(bit)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.set.count_ones() as usize;
        (n, Some(n))
    }
}
impl<const REVERSE: bool> ExactSizeIterator for BitSetIterator<REVERSE> {}
impl<const REVERSE: bool> core::iter::FusedIterator for BitSetIterator<REVERSE> {}
