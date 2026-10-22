// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![doc = include_str!("../tests/function_ref_ownership.md")]

use core::cell::{BorrowMutError, RefCell, RefMut};
use core::ops::Deref;

#[repr(transparent)]
pub struct FunctionRef<'a, F: ?Sized> {
    callable: &'a F,
}

impl<'a, F: ?Sized> FunctionRef<'a, F> {
    #[inline]
    pub const fn new(callable: &'a F) -> Self {
        Self { callable }
    }

    #[inline]
    pub const fn get(self) -> &'a F {
        self.callable
    }
}

impl<F: ?Sized> Copy for FunctionRef<'_, F> {}
impl<F: ?Sized> Clone for FunctionRef<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: ?Sized> Deref for FunctionRef<'_, F> {
    type Target = F;

    #[inline]
    fn deref(&self) -> &F {
        self.callable
    }
}

#[repr(transparent)]
pub struct FunctionRefMut<'a, F: ?Sized> {
    callable: &'a RefCell<F>,
}

impl<'a, F: ?Sized> FunctionRefMut<'a, F> {
    #[inline]
    pub const fn new(callable: &'a RefCell<F>) -> Self {
        Self { callable }
    }

    #[inline]
    pub fn borrow_mut(&self) -> RefMut<'a, F> {
        self.callable.borrow_mut()
    }

    #[inline]
    pub fn try_borrow_mut(&self) -> Result<RefMut<'a, F>, BorrowMutError> {
        self.callable.try_borrow_mut()
    }

    #[inline]
    pub fn with_mut<R>(&self, invoke: impl FnOnce(&mut F) -> R) -> R {
        invoke(&mut *self.borrow_mut())
    }

    #[inline]
    pub fn try_with_mut<R>(&self, invoke: impl FnOnce(&mut F) -> R) -> Result<R, BorrowMutError> {
        Ok(invoke(&mut *self.try_borrow_mut()?))
    }
}

impl<F: ?Sized> Copy for FunctionRefMut<'_, F> {}
impl<F: ?Sized> Clone for FunctionRefMut<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}

const _: () = {
    assert!(core::mem::size_of::<FunctionRef<'static, fn()>>() == core::mem::size_of::<usize>());
    assert!(core::mem::size_of::<FunctionRefMut<'static, fn()>>() == core::mem::size_of::<usize>());
    assert!(!core::mem::needs_drop::<FunctionRef<'static, fn()>>());
    assert!(!core::mem::needs_drop::<FunctionRefMut<'static, fn()>>());
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::string::String;

    fn identity(value: &str) -> &str {
        value
    }

    #[test]
    fn shared_copies_keep_capture_and_argument_identity() {
        let captured = String::from("symbol");
        let length = move |value: &str| value.len() + captured.len();
        let callback = FunctionRef::new(&length);
        let copy = callback;
        assert_eq!(callback("abc"), 9);
        assert_eq!(copy("x"), 7);
        assert!(core::ptr::eq(callback.get(), copy.get()));
        let callback = FunctionRef::new(&identity);
        let input = String::from("value");
        assert!(core::ptr::eq(callback(&input), input.as_str()));
    }

    #[test]
    fn signatures_forward_void_references_moves_and_many_arguments() {
        let increment = |value: &mut u64| *value += 1;
        let callback = FunctionRef::new(&increment);
        let mut value = 41;
        callback(&mut value);
        assert_eq!(value, 42);
        let consume = |value: Box<u64>| *value + 1;
        assert_eq!(FunctionRef::new(&consume)(Box::new(8)), 9);
        let many = |a, b, c, d, e, f, g, h, i| a + b + c + d + e + f + g + h + i;
        assert_eq!(FunctionRef::new(&many)(1, 2, 3, 4, 5, 6, 7, 8, 9), 45);
        let zero = || 7;
        assert_eq!(FunctionRef::new(&zero)(), 7);
    }

    #[test]
    fn typed_function_pointers_and_unsized_borrows() {
        let pointer: fn(&str) -> &str = identity;
        assert_eq!(FunctionRef::new(&pointer)("symbol"), "symbol");
        let callable = |value| value + 1;
        let erased: &dyn Fn(u64) -> u64 = &callable;
        assert_eq!(FunctionRef::new(erased)(41), 42);
        let mut state = 0;
        let callable = RefCell::new(|value| {
            state += value;
            state
        });
        let erased: &RefCell<dyn FnMut(u64) -> u64> = &callable;
        let callback = FunctionRefMut::new(erased);
        assert_eq!(callback.with_mut(|f| f(41)), 41);
        assert_eq!(callback.with_mut(|f| f(1)), 42);
    }

    #[test]
    fn wrapper_borrows_observe_rebinding_while_copies_keep_the_target() {
        let first: fn(u64) -> u64 = |value| value + 1;
        let second: fn(u64) -> u64 = |value| value + 2;
        let original = FunctionRef::new(&first);
        let nested = FunctionRef::new(&original);
        assert_eq!(nested(40), 41);
        assert_eq!(FunctionRef::new(&FunctionRef::new(&first))(40), 41);
        let original = RefCell::new(original);
        let snapshot = *original.borrow();
        let forwarded = FunctionRefMut::new(&original);
        assert_eq!(forwarded.with_mut(|f| f(40)), 41);
        *original.borrow_mut() = FunctionRef::new(&second);
        assert_eq!(forwarded.with_mut(|f| f(40)), 42);
        assert_eq!(snapshot(40), 41);
    }

    #[test]
    fn mutable_copies_share_capture_without_copying_it() {
        let mut total = 0;
        let callable = RefCell::new(|value| {
            total += value;
            total
        });
        let callback = FunctionRefMut::new(&callable);
        let copy = callback;
        assert_eq!(callback.with_mut(|f| f(3)), 3);
        assert_eq!(copy.try_with_mut(|f| f(5)).unwrap(), 8);
        assert_eq!((callback.borrow_mut())(7), 15);
        drop(callable);
        assert_eq!(total, 15);
    }

    #[test]
    fn mutable_reentrancy_fails_before_call_and_recovers() {
        let callable = RefCell::new(|| 7);
        let callback = FunctionRefMut::new(&callable);
        let copy = callback;
        callback.with_mut(|_| {
            assert!(copy.try_borrow_mut().is_err());
            assert!(copy.try_with_mut(|_| panic!("must not invoke")).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| copy.borrow_mut())).is_err());
        });
        assert_eq!(copy.with_mut(|f| f()), 7);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            callback.with_mut(|_| panic!("callback failed"))
        }))
        .is_err());
        assert_eq!(callback.try_with_mut(|f| f()).unwrap(), 7);
    }

    #[test]
    fn guarded_reference_return_keeps_mutable_capture_borrowed() {
        struct Accessor(u64);
        impl Accessor {
            fn get(&mut self) -> &mut u64 {
                &mut self.0
            }
        }
        let callable = RefCell::new(Accessor(41));
        let callback = FunctionRefMut::new(&callable);
        let mut result = RefMut::map(callback.borrow_mut(), Accessor::get);
        *result += 1;
        assert!(callback.try_borrow_mut().is_err());
        drop(result);
        assert_eq!(callback.with_mut(|f| *f.get()), 42);
    }
}
