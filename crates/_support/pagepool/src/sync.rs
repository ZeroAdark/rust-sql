//! The atomics and thread-locals the pool's shared state uses: core's and std's. Under `cfg(shuttle)` (the models in
//! tests/shuttle_*.rs) the same atomics with a scheduling point before each operation, `repr(transparent)` and
//! `const fn new` (statics, and the meta zone's tables cast from raw memory), and shuttle's thread-locals. The loom
//! models drive the kernels through their traits instead (`loom_api`): loom's atomics cannot sit in statics.

#[cfg(not(shuttle))]
pub(crate) use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// A spin wait's pause: under the models the spinner yields (PCT lowers its priority), so the holder it waits on runs.
#[cfg(not(shuttle))]
pub(crate) use core::hint::spin_loop;
#[cfg(shuttle)]
pub(crate) use shuttle::hint::spin_loop;

#[cfg(not(shuttle))]
macro_rules! tls {
    ($($t:tt)*) => { std::thread_local! { $($t)* } };
}

#[cfg(shuttle)]
macro_rules! tls {
    ($($t:tt)*) => { shuttle::thread_local! { $($t)* } };
}

pub(crate) use tls;

#[cfg(shuttle)]
pub(crate) use model::*;

#[cfg(shuttle)]
mod model {
    use core::sync::atomic as a;
    pub(crate) use core::sync::atomic::Ordering;

    /// Where the model may switch threads: before every atomic operation (shuttle is sequentially consistent, so the
    /// operation itself then runs alone). A load of one of shuttle's own atomics: a plain scheduling point, not a yield
    /// (PCT lowers a yielding thread's priority, so a yield at every operation would make its schedules round robin).
    #[inline]
    fn point() {
        static POINT: shuttle::sync::atomic::AtomicBool = shuttle::sync::atomic::AtomicBool::new(false);
        // a failed model's teardown (a guard's unlock while unwinding) runs with no execution left to switch in
        if !std::thread::panicking() {
            POINT.load(Ordering::SeqCst);
        }
    }

    macro_rules! atomic {
        ($name:ident, $t:ty) => {
            #[repr(transparent)]
            pub struct $name(a::$name);

            impl core::fmt::Debug for $name {
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    self.0.fmt(f)
                }
            }

            #[allow(dead_code)]
            impl $name {
                pub const fn new(v: $t) -> Self {
                    Self(a::$name::new(v))
                }
                pub fn load(&self, o: Ordering) -> $t {
                    point();
                    self.0.load(o)
                }
                pub fn store(&self, v: $t, o: Ordering) {
                    point();
                    self.0.store(v, o)
                }
                pub fn swap(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.swap(v, o)
                }
                pub fn compare_exchange(&self, c: $t, n: $t, s: Ordering, f: Ordering) -> Result<$t, $t> {
                    point();
                    self.0.compare_exchange(c, n, s, f)
                }
                pub fn compare_exchange_weak(&self, c: $t, n: $t, s: Ordering, f: Ordering) -> Result<$t, $t> {
                    point();
                    self.0.compare_exchange(c, n, s, f)
                }
                pub fn fetch_update<F: FnMut($t) -> Option<$t>>(&self, s: Ordering, f: Ordering, g: F) -> Result<$t, $t> {
                    point();
                    self.0.fetch_update(s, f, g)
                }
                pub fn fetch_and(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_and(v, o)
                }
                pub fn fetch_or(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_or(v, o)
                }
            }
        };
    }

    macro_rules! int {
        ($name:ident, $t:ty) => {
            atomic!($name, $t);

            #[allow(dead_code)]
            impl $name {
                pub fn fetch_add(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_add(v, o)
                }
                pub fn fetch_sub(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_sub(v, o)
                }
                pub fn fetch_max(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_max(v, o)
                }
                pub fn fetch_min(&self, v: $t, o: Ordering) -> $t {
                    point();
                    self.0.fetch_min(v, o)
                }
            }
        };
    }

    atomic!(AtomicBool, bool);
    int!(AtomicU8, u8);
    int!(AtomicU16, u16);
    int!(AtomicU32, u32);
    int!(AtomicU64, u64);
    int!(AtomicUsize, usize);
    int!(AtomicIsize, isize);

    #[repr(transparent)]
    pub struct AtomicPtr<T>(a::AtomicPtr<T>);

    #[allow(dead_code)]
    impl<T> AtomicPtr<T> {
        pub const fn new(p: *mut T) -> Self {
            Self(a::AtomicPtr::new(p))
        }
        pub fn load(&self, o: Ordering) -> *mut T {
            point();
            self.0.load(o)
        }
        pub fn store(&self, p: *mut T, o: Ordering) {
            point();
            self.0.store(p, o)
        }
        pub fn swap(&self, p: *mut T, o: Ordering) -> *mut T {
            point();
            self.0.swap(p, o)
        }
        pub fn compare_exchange(&self, c: *mut T, n: *mut T, s: Ordering, f: Ordering) -> Result<*mut T, *mut T> {
            point();
            self.0.compare_exchange(c, n, s, f)
        }
    }
}
