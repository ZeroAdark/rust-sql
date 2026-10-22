//! `Rc` in a context: non-atomic, `!Send` / `!Sync`, freed into its context at the last drop.

use core::alloc::Layout;
use core::cell::Cell;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

use allocator_api2::alloc::Allocator;
use ::types_error::PgResult;

use crate::{Mcx, PgBox};

#[repr(C)]
struct RcInner<T: ?Sized> {
    strong: Cell<usize>,
    value: T,
}

pub struct PgRc<'mcx, T: ?Sized> {
    ptr: NonNull<RcInner<T>>,
    mcx: Mcx<'mcx>,
    _owns: PhantomData<RcInner<T>>,
}

impl<'mcx, T> PgRc<'mcx, T> {
    /// A failed allocation raises the context's ERROR.
    pub fn new_in(value: T, mcx: Mcx<'mcx>) -> Self {
        Self::try_new_in(value, mcx).unwrap_or_else(|e| crate::raise(e))
    }

    pub fn try_new_in(value: T, mcx: Mcx<'mcx>) -> PgResult<Self> {
        let b = PgBox::try_new_in(RcInner { strong: Cell::new(1), value }, mcx)
            .map_err(|_| mcx.oom_boxed(core::mem::size_of::<RcInner<T>>()))?;
        let (raw, mcx) = allocator_api2::boxed::Box::into_raw_with_allocator(b);
        // SAFETY: Box::into_raw never returns null
        Ok(PgRc { ptr: unsafe { NonNull::new_unchecked(raw) }, mcx, _owns: PhantomData })
    }
}

impl<'mcx, T: Copy> PgRc<'mcx, [T]> {
    pub fn from_slice_in(s: &[T], mcx: Mcx<'mcx>) -> Self {
        let (layout, at) = slice_layout::<T>(s.len());
        let mem = match mcx.allocate(layout) {
            Ok(m) => m.cast::<u8>(),
            Err(_) => crate::raise(mcx.oom_boxed(layout.size())),
        };
        // SAFETY: `mem` is a fresh block of `layout`: the header at 0, `s.len()` elements of T at `at` (RcInner<[T]>'s
        // repr(C) layout); T: Copy, so a bitwise copy is the clone and nothing can panic midway
        unsafe {
            mem.as_ptr().cast::<Cell<usize>>().write(Cell::new(1));
            core::ptr::copy_nonoverlapping(s.as_ptr(), mem.as_ptr().add(at).cast::<T>(), s.len());
            let fat = core::ptr::slice_from_raw_parts_mut(mem.as_ptr().cast::<T>(), s.len()) as *mut RcInner<[T]>;
            PgRc { ptr: NonNull::new_unchecked(fat), mcx, _owns: PhantomData }
        }
    }
}

fn slice_layout<T>(n: usize) -> (Layout, usize) {
    let (l, at) = Layout::new::<Cell<usize>>()
        .extend(Layout::array::<T>(n).unwrap_or_else(|_| crate::raise(crate::invalid_alloc_size(n))))
        .unwrap_or_else(|_| crate::raise(crate::invalid_alloc_size(n)));
    (l.pad_to_align(), at)
}

impl<'mcx, T: ?Sized> PgRc<'mcx, T> {
    #[inline]
    fn inner(&self) -> &RcInner<T> {
        // SAFETY: the block lives while any PgRc to it does (strong >= 1)
        unsafe { self.ptr.as_ref() }
    }

    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        core::ptr::addr_eq(a.ptr.as_ptr(), b.ptr.as_ptr())
    }

    pub fn strong_count(this: &Self) -> usize {
        this.inner().strong.get()
    }

    pub fn get_mut(this: &mut Self) -> Option<&mut T> {
        if this.inner().strong.get() == 1 {
            // SAFETY: the only PgRc to the block, borrowed mutably
            Some(unsafe { &mut this.ptr.as_mut().value })
        } else {
            None
        }
    }

    pub fn allocator(this: &Self) -> Mcx<'mcx> {
        this.mcx
    }
}

impl<T: ?Sized> Clone for PgRc<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        let s = &self.inner().strong;
        s.set(s.get() + 1);
        PgRc { ptr: self.ptr, mcx: self.mcx, _owns: PhantomData }
    }
}

impl<T: ?Sized> Drop for PgRc<'_, T> {
    fn drop(&mut self) {
        let s = &self.inner().strong;
        let n = s.get() - 1;
        s.set(n);
        if n == 0 {
            // SAFETY: the last PgRc: drop the value in place, then free the block with the layout it was made with
            unsafe {
                let layout = Layout::for_value(self.ptr.as_ref());
                core::ptr::drop_in_place(&mut self.ptr.as_mut().value);
                self.mcx.deallocate(self.ptr.cast::<u8>(), layout);
            }
        }
    }
}

impl<T: ?Sized> Deref for PgRc<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner().value
    }
}

impl<T: ?Sized> AsRef<T> for PgRc<'_, T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for PgRc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for PgRc<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: ?Sized + PartialEq> PartialEq for PgRc<'_, T> {
    fn eq(&self, o: &Self) -> bool {
        **self == **o
    }
}

impl<T: ?Sized + Eq> Eq for PgRc<'_, T> {}

impl<T: ?Sized + PartialOrd> PartialOrd for PgRc<'_, T> {
    fn partial_cmp(&self, o: &Self) -> Option<core::cmp::Ordering> {
        (**self).partial_cmp(&**o)
    }
}

impl<T: ?Sized + Ord> Ord for PgRc<'_, T> {
    fn cmp(&self, o: &Self) -> core::cmp::Ordering {
        (**self).cmp(&**o)
    }
}

impl<T: ?Sized + Hash> Hash for PgRc<'_, T> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (**self).hash(h)
    }
}
