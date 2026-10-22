//! `std` collection idioms written into a context: the plan-time conversion table's right-hand side. No `!needs_drop`
//! assert (an aset context frees each chunk as its value drops). Every lane here reserves fallibly: a context limit or a
//! failed block raises palloc's ERROR (`raise`), never `handle_alloc_error`'s abort. allocator_api2's own growth
//! (`PgVec::push` / `extend` / `with_capacity_in` / `resize`, `PgBox::new_in`) is that abort: use `GrowIn` and these.

use core::fmt;

use crate::{Mcx, PgBox, PgString, PgVec};
use ::types_error::PgError;

#[cold]
#[inline(never)]
fn short<T>(mcx: Mcx<'_>, n: usize) -> ! {
    raise(mcx.oom_boxed(n.saturating_mul(core::mem::size_of::<T>())))
}

/// Room for `n` more elements, or the context's ERROR.
#[inline]
fn room<T>(v: &mut PgVec<'_, T>, n: usize) {
    if v.capacity() - v.len() < n && v.try_reserve(n).is_err() {
        short::<T>(*v.allocator(), n)
    }
}

/// `PgVec` growth that raises its ERROR where `push` / `extend` / `resize` would abort.
pub trait GrowIn<T> {
    fn push_in(&mut self, x: T);
    fn extend_in<I: IntoIterator<Item = T>>(&mut self, it: I);
    fn extend_from_slice_in(&mut self, s: &[T])
    where
        T: Clone;
    fn resize_in(&mut self, n: usize, x: T)
    where
        T: Clone;
}

impl<T> GrowIn<T> for PgVec<'_, T> {
    #[inline]
    fn push_in(&mut self, x: T) {
        room(self, 1);
        self.push(x);
    }

    #[inline]
    fn extend_in<I: IntoIterator<Item = T>>(&mut self, it: I) {
        let it = it.into_iter();
        room(self, it.size_hint().0);
        for x in it {
            self.push_in(x);
        }
    }

    #[inline]
    fn extend_from_slice_in(&mut self, s: &[T])
    where
        T: Clone,
    {
        room(self, s.len());
        self.extend_from_slice(s);
    }

    #[inline]
    fn resize_in(&mut self, n: usize, x: T)
    where
        T: Clone,
    {
        room(self, n.saturating_sub(self.len()));
        self.resize(n, x);
    }
}

/// `Vec::with_capacity` in `mcx`.
#[inline]
pub fn reserved_in<'m, T>(n: usize, mcx: Mcx<'m>) -> PgVec<'m, T> {
    let mut v = PgVec::new_in(mcx);
    room(&mut v, n);
    v
}

pub trait CollectIn: Iterator + Sized {
    #[inline]
    fn collect_in<'m>(self, mcx: Mcx<'m>) -> PgVec<'m, Self::Item> {
        let mut v = PgVec::new_in(mcx);
        v.extend_in(self);
        v
    }
}

impl<I: Iterator> CollectIn for I {}

/// `collect::<Option<Vec<_>>>()` into a context: stops at the first `None`.
pub trait TryCollectOptionIn<T>: Iterator<Item = Option<T>> + Sized {
    fn try_collect_in<'m>(self, mcx: Mcx<'m>) -> Option<PgVec<'m, T>> {
        let mut v = PgVec::new_in(mcx);
        room(&mut v, self.size_hint().0);
        for x in self {
            v.push_in(x?);
        }
        Some(v)
    }
}

impl<T, I: Iterator<Item = Option<T>>> TryCollectOptionIn<T> for I {}

/// `collect::<Result<Vec<_>, _>>()` into a context: stops at the first `Err`.
pub trait TryCollectResultIn<T, E>: Iterator<Item = Result<T, E>> + Sized {
    fn try_collect_res_in<'m>(self, mcx: Mcx<'m>) -> Result<PgVec<'m, T>, E> {
        let mut v = PgVec::new_in(mcx);
        room(&mut v, self.size_hint().0);
        for x in self {
            v.push_in(x?);
        }
        Ok(v)
    }
}

impl<T, E, I: Iterator<Item = Result<T, E>>> TryCollectResultIn<T, E> for I {}

/// `Iterator::partition` into two `PgVec`s of one context: the first holds the items `f` accepts, in order.
pub trait PartitionIn: Iterator + Sized {
    fn partition_in<'m>(self, mcx: Mcx<'m>, mut f: impl FnMut(&Self::Item) -> bool) -> (PgVec<'m, Self::Item>, PgVec<'m, Self::Item>) {
        let (mut yes, mut no) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
        for x in self {
            if f(&x) {
                yes.push_in(x);
            } else {
                no.push_in(x);
            }
        }
        (yes, no)
    }
}

impl<I: Iterator> PartitionIn for I {}

#[inline]
pub fn to_vec_in<'m, T: Clone>(s: &[T], mcx: Mcx<'m>) -> PgVec<'m, T> {
    let mut v = reserved_in(s.len(), mcx);
    v.extend_from_slice(s);
    v
}

pub fn concat_in<'m, T: Clone>(parts: &[&[T]], mcx: Mcx<'m>) -> PgVec<'m, T> {
    let mut v = reserved_in(parts.iter().map(|p| p.len()).sum(), mcx);
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

#[doc(hidden)]
#[inline]
pub fn vec_elem_in<'m, T: Clone>(mcx: Mcx<'m>, x: T, n: usize) -> PgVec<'m, T> {
    let mut v = reserved_in(n, mcx);
    v.resize(n, x);
    v
}

#[doc(hidden)]
#[inline]
pub fn vec_array_in<'m, T, const N: usize>(mcx: Mcx<'m>, a: [T; N]) -> PgVec<'m, T> {
    let mut v = reserved_in(N, mcx);
    v.extend(a);
    v
}

#[inline]
pub fn box_in<'m, T>(x: T, mcx: Mcx<'m>) -> PgBox<'m, T> {
    match PgBox::try_new_in(x, mcx) {
        Ok(b) => b,
        Err(_) => short::<T>(mcx, 1),
    }
}

pub fn format_in<'m>(mcx: Mcx<'m>, args: fmt::Arguments<'_>) -> PgString<'m> {
    let mut s = PgString::new_in(mcx);
    if let Some(t) = args.as_str() {
        s.push_str(t);
        return s;
    }
    struct W<'a, 'm> {
        s: &'a mut PgString<'m>,
        err: Option<alloc::boxed::Box<PgError>>,
    }
    impl fmt::Write for W<'_, '_> {
        fn write_str(&mut self, t: &str) -> fmt::Result {
            self.s.try_push_str(t).map_err(|e| {
                self.err = Some(e);
                fmt::Error
            })
        }
    }
    let mut w = W { s: &mut s, err: None };
    let r = fmt::write(&mut w, args);
    if let Some(e) = w.err {
        raise(e);
    }
    r.expect("a formatting trait implementation returned an error when the underlying stream did not");
    s
}

/// The unwind carries the boxed `PgError`: the statement boundary recovers only that payload.
#[cold]
#[inline(never)]
pub fn raise(e: alloc::boxed::Box<PgError>) -> ! {
    #[cfg(any(feature = "std", test))]
    std::panic::panic_any(e);
    #[cfg(not(any(feature = "std", test)))]
    panic!("{e:?}")
}

#[macro_export]
macro_rules! pg_vec {
    (in $m:expr) => {
        $crate::PgVec::new_in($m)
    };
    (in $m:expr; $x:expr; $n:expr) => {
        $crate::vec_elem_in($m, $x, $n)
    };
    (in $m:expr; $($x:expr),+ $(,)?) => {
        $crate::vec_array_in($m, [$($x),+])
    };
}

#[macro_export]
macro_rules! pg_format {
    ($m:expr, $($arg:tt)*) => {
        $crate::format_in($m, ::core::format_args!($($arg)*))
    };
}

/// `pg_box_dyn!(m, v => dyn Trait + 'g)`: the unsizing cast is the one this macro writes, so `box_unsize_dyn`'s contract
/// holds by construction.
#[macro_export]
macro_rules! pg_box_dyn {
    ($m:expr, $v:expr => $t:ty) => {{
        let sized = $crate::box_in($v, $m);
        #[allow(unsafe_code)]
        // SAFETY: the closure is the unsizing coercion of the box's own pointer (same address, metadata only)
        let b: $crate::PgBox<'_, $t> = unsafe { $crate::box_unsize_dyn(sized, |p| p as *mut $t) };
        b
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryContext, PgRc};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use types_error::ERRCODE_OUT_OF_MEMORY;

    /// Each lane past a 64-byte limit raises the context's ERROR (53200, the boxed payload), never an abort.
    fn raises(what: &str, f: impl FnOnce(Mcx<'_>)) {
        let ctx = MemoryContext::new("PlanLimit").with_limit(64);
        let r = catch_unwind(AssertUnwindSafe(|| f(ctx.mcx())));
        let p = r.expect_err(what);
        let e = p.downcast_ref::<alloc::boxed::Box<PgError>>().unwrap_or_else(|| panic!("{what}: not a PgError"));
        assert_eq!(e.sqlstate(), ERRCODE_OUT_OF_MEMORY, "{what}");
    }

    #[test]
    fn each_helper_raises_at_the_limit() {
        let big = [7u64; 64];
        raises("collect_in", |m| drop((0..64u64).collect_in(m)));
        raises("collect_in, no hint", |m| drop((0..64u64).filter(|_| true).collect_in(m)));
        raises("to_vec_in", |m| drop(to_vec_in(&big[..], m)));
        raises("concat_in", |m| drop(concat_in(&[&big[..], &big[..]], m)));
        raises("vec_elem_in", |m| drop(crate::pg_vec![in m; 0u64; 64]));
        raises("vec_array_in", |m| drop(vec_array_in(m, big)));
        raises("reserved_in", |m| drop(reserved_in::<u64>(64, m)));
        raises("box_in", |m| drop(box_in(big, m)));
        raises("pg_box_dyn", |m| drop(crate::pg_box_dyn!(m, big => dyn core::fmt::Debug)));
        raises("PgRc::new_in", |m| drop(PgRc::new_in(big, m)));
        raises("PgRc::from_slice_in", |m| drop(PgRc::from_slice_in(&big[..], m)));
        raises("pg_format", |m| drop(crate::pg_format!(m, "{:?}", big)));
        raises("PgString::push_str", |m| PgString::new_in(m).push_str(&"x".repeat(200)));
        raises("push_in", |m| {
            let mut v = PgVec::new_in(m);
            for i in 0..64u64 {
                v.push_in(i);
            }
        });
        raises("extend_in", |m| PgVec::new_in(m).extend_in(0..64u64));
        raises("extend_from_slice_in", |m| PgVec::new_in(m).extend_from_slice_in(&big[..]));
        raises("resize_in", |m| PgVec::new_in(m).resize_in(64, 0u64));
    }

    #[test]
    fn grow_lanes_match_std_under_the_limit() {
        let ctx = MemoryContext::new("PlanFits");
        let m = ctx.mcx();
        let mut v = reserved_in::<u32>(2, m);
        v.push_in(1);
        v.extend_in([2, 3]);
        v.extend_from_slice_in(&[4]);
        v.resize_in(6, 9);
        assert_eq!(&v[..], &[1, 2, 3, 4, 9, 9]);
        assert_eq!(&(1..4).filter(|x| x % 2 == 1).collect_in(m)[..], &[1, 3]);
    }

    #[test]
    fn try_collect_and_partition_match_std() {
        let ctx = MemoryContext::new("PlanTry");
        let m = ctx.mcx();
        let ok = [Some(1u32), Some(2)].into_iter().try_collect_in(m);
        assert_eq!(ok.as_deref(), Some(&[1, 2][..]));
        assert!([Some(1u32), None, Some(3)].into_iter().try_collect_in(m).is_none());
        let r: Result<_, &str> = [Ok(1u32), Ok(2)].into_iter().try_collect_res_in(m);
        assert_eq!(&r.unwrap()[..], &[1, 2]);
        let r: Result<PgVec<'_, u32>, &str> = [Ok(1u32), Err("x"), Ok(3)].into_iter().try_collect_res_in(m);
        assert_eq!(r.err(), Some("x"));
        let (even, odd) = (1..8u32).partition_in(m, |x| x % 2 == 0);
        assert_eq!((&even[..], &odd[..]), (&[2, 4, 6][..], &[1, 3, 5, 7][..]));
        let (a, b): (alloc::vec::Vec<u32>, alloc::vec::Vec<u32>) = (1..8u32).partition(|x| x % 2 == 0);
        assert_eq!((&even[..], &odd[..]), (&a[..], &b[..]));
    }

    #[test]
    fn try_collect_and_partition_raise_at_the_limit() {
        raises("try_collect_in", |m| drop((0..64u64).map(Some).try_collect_in(m)));
        raises("try_collect_res_in", |m| drop((0..64u64).map(Ok::<_, ()>).try_collect_res_in(m)));
        raises("partition_in", |m| drop((0..64u64).partition_in(m, |x| x % 2 == 0)));
    }
}
