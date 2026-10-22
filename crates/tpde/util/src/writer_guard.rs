// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{ensure, invalid, misc, SmallVec};
use types_error::PgResult;

pub struct VectorWriter<'a, 'm, const N: usize = 256> {
    vector: Option<&'a mut SmallVec<'m, u8, N>>,
    cursor: usize,
    end: usize,
}
impl<const N: usize> Default for VectorWriter<'_, '_, N> {
    fn default() -> Self {
        Self {
            vector: None,
            cursor: 0,
            end: 0,
        }
    }
}
impl<'a, 'm, const N: usize> VectorWriter<'a, 'm, N> {
    pub fn new(vector: &'a mut SmallVec<'m, u8, N>) -> Self {
        let cursor = vector.len();
        Self {
            vector: Some(vector),
            cursor,
            end: cursor,
        }
    }
    pub fn at(vector: &'a mut SmallVec<'m, u8, N>, offset: usize) -> PgResult<Self> {
        ensure(
            offset <= vector.len(),
            "writer offset beyond initialized data",
        )?;
        let end = vector.len();
        Ok(Self {
            vector: Some(vector),
            cursor: offset,
            end,
        })
    }
    pub fn is_bound(&self) -> bool {
        self.vector.is_some()
    }
    pub fn size(&self) -> usize {
        self.cursor
    }
    pub fn capacity(&self) -> usize {
        self.end
    }
    pub fn data(&self) -> PgResult<&[u8]> {
        self.vector
            .as_deref()
            .map(|v| &v[..])
            .ok_or_else(|| invalid("unbound writer"))
    }
    pub fn data_mut(&mut self) -> PgResult<&mut [u8]> {
        self.vector
            .as_deref_mut()
            .map(|v| &mut v[..])
            .ok_or_else(|| invalid("unbound writer"))
    }
    pub fn reserve(&mut self, extra: usize) -> PgResult<()> {
        let vector = self
            .vector
            .as_deref_mut()
            .ok_or_else(|| invalid("unbound writer"))?;
        if self.end - self.cursor < extra {
            let end = self
                .cursor
                .checked_add(extra)
                .ok_or_else(|| invalid("writer size overflow"))?;
            vector.reserve(end)?;
            self.end = vector.capacity();
        }
        Ok(())
    }
    pub fn flush(&mut self) {
        if let Some(vector) = self.vector.as_deref_mut() {
            if self.cursor != self.end {
                vector.truncate(self.cursor);
                self.end = self.cursor;
            }
        }
    }
    fn admitted_end(&self, size: usize) -> PgResult<usize> {
        ensure(self.is_bound(), "unbound writer")?;
        let end = self
            .cursor
            .checked_add(size)
            .ok_or_else(|| invalid("writer size overflow"))?;
        ensure(end <= self.end, "writer reservation exhausted")?;
        Ok(end)
    }
    pub fn skip_unchecked(&mut self, size: usize) -> PgResult<()> {
        let end = self.admitted_end(size)?;
        let vector = self.vector.as_deref_mut().unwrap();
        if end > vector.len() {
            vector.resize_bytes(end)?;
        }
        self.cursor = end;
        Ok(())
    }
    pub fn skip(&mut self, size: usize) -> PgResult<()> {
        self.reserve(size)?;
        self.skip_unchecked(size)
    }
    pub fn unskip(&mut self, size: usize) -> PgResult<()> {
        ensure(
            self.is_bound() && size <= self.cursor,
            "writer rewind before start",
        )?;
        self.cursor -= size;
        Ok(())
    }
    pub fn zero_unchecked(&mut self, size: usize) -> PgResult<()> {
        let end = self.admitted_end(size)?;
        let vector = self.vector.as_deref_mut().unwrap();
        let initialized_end = end.min(vector.len());
        vector[self.cursor..initialized_end].fill(0);
        if end > vector.len() {
            vector.resize_bytes(end)?;
        }
        self.cursor = end;
        Ok(())
    }

    pub fn zero(&mut self, size: usize) -> PgResult<()> {
        self.reserve(size)?;
        self.zero_unchecked(size)
    }
    pub fn write_unchecked(&mut self, bytes: &[u8]) -> PgResult<()> {
        let end = self.admitted_end(bytes.len())?;
        let vector = self.vector.as_deref_mut().unwrap();
        let overlap = bytes.len().min(vector.len() - self.cursor);
        vector[self.cursor..self.cursor + overlap].copy_from_slice(&bytes[..overlap]);
        vector.extend_from_slice(&bytes[overlap..])?;
        self.cursor = end;
        Ok(())
    }
    pub fn write(&mut self, bytes: &[u8]) -> PgResult<()> {
        self.reserve(bytes.len())?;
        self.write_unchecked(bytes)
    }
    pub fn write_int<T: Integral>(&mut self, value: T) -> PgResult<()> {
        self.reserve(T::SIZE)?;
        value.write_to(self)
    }
    pub fn write_int_unchecked<T: Integral>(&mut self, value: T) -> PgResult<()> {
        value.write_to(self)
    }
    pub fn write_uleb_unchecked(&mut self, value: u64) -> PgResult<()> {
        self.admitted_end(misc::uleb_len(value))?;
        let mut bytes = [0; 10];
        let len = misc::uleb_write(&mut bytes, value)?;
        self.write_unchecked(&bytes[..len])
    }
    pub fn write_uleb(&mut self, value: u64) -> PgResult<()> {
        self.reserve(10)?;
        self.write_uleb_unchecked(value)
    }
    pub fn write_sleb_unchecked(&mut self, value: i64) -> PgResult<()> {
        self.admitted_end(10)?;
        let mut bytes = [0; 10];
        let len = misc::sleb_write(&mut bytes, value)?;
        self.write_unchecked(&bytes[..len])
    }
    pub fn write_sleb(&mut self, value: i64) -> PgResult<()> {
        self.reserve(10)?;
        self.write_sleb_unchecked(value)
    }
}
impl<const N: usize> Drop for VectorWriter<'_, '_, N> {
    fn drop(&mut self) {
        self.flush();
    }
}
mod sealed {
    pub trait Integral {}
}
pub trait Integral: sealed::Integral {
    const SIZE: usize;
    fn write_to<const N: usize>(self, writer: &mut VectorWriter<'_, '_, N>) -> PgResult<()>;
    /// Native-endian bytes of the value, as `std::memcpy(dst, &t, sizeof(T))` stores them.
    fn with_ne_bytes<R>(self, consume: impl FnOnce(&[u8]) -> R) -> R;
}
macro_rules! integral {
    ($($t:ty),*) => {
        $(
            impl sealed::Integral for $t {}
            impl Integral for $t {
                const SIZE: usize = core::mem::size_of::<Self>();
                fn write_to<const N: usize>(self, writer: &mut VectorWriter<'_, '_, N>) -> PgResult<()> {
                    writer.write_unchecked(&self.to_ne_bytes())
                }
                #[inline(always)]
                fn with_ne_bytes<R>(self, consume: impl FnOnce(&[u8]) -> R) -> R {
                    consume(&self.to_ne_bytes())
                }
            }
        )*
    };
}
integral!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
impl sealed::Integral for bool {}
impl Integral for bool {
    const SIZE: usize = 1;
    fn write_to<const N: usize>(self, writer: &mut VectorWriter<'_, '_, N>) -> PgResult<()> {
        writer.write_unchecked(&[self as u8])
    }
    #[inline(always)]
    fn with_ne_bytes<R>(self, consume: impl FnOnce(&[u8]) -> R) -> R {
        consume(&[self as u8])
    }
}
const _: () = assert!(core::mem::size_of::<VectorWriter<'_, '_>>() <= 128);
