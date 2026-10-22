//! `contract::host`: the one way a crate writes an entry the lowered IR calls (a JIT-to-host
//! entry). `host!` writes the `extern "C"` function and its panic boundary, so no family file spells the ABI. It is
//! defined here because every crate that writes a host entry reaches abi (the driver and fn sit below kernels in the
//! layering, rule R5); kernels re-exports it as `contract::host`. Its caller half, `call`, is the interpreter's way into a
//! host by address.

/// Host entries the lowered IR calls, written as plain functions. An entry keeps its name, signature and body exactly as
/// written; the macro adds the ABI and, when asked, the boundary. Three forms, chosen by a marker right after the entry's
/// doc comment:
///
/// - `#[guard(FAIL)]`: `extern "C"`, the body under the one landing pad (`status::unwind::pad`). A panic in the body is
///   parked for the program's caller and the entry answers `FAIL`, the value its IR already reads as a failure (`ERROR`,
///   or a record entry's sunk buffer). This is kernels' `guard!(FAIL, body)` written as part of the signature.
/// - no marker: `extern "C"`, the body as written (an entry with its own boundary, like the driver's
///   `hostimpl::guarded`, or none yet).
/// - `#[unwind]`: `extern "C-unwind"`, the body as written (an entry whose caller lets an unwind through).
///
/// Any other attributes (`#[inline(never)]`, `#[cold]`, ...) go after the marker. The entry may be `unsafe` or not, may
/// take type parameters with one bound each (`<S: Source>`), and needs a return type only under `#[guard]`. One block may
/// hold several entries.
///
/// ```
/// use gravity_abi::status::ERROR;
///
/// gravity_abi::host! {
///     /// A host entry: `site`'s answer, or ERROR when its body panics.
///     #[guard(ERROR as u64)]
///     pub unsafe fn answer(_ctx: u64, site: u64) -> u64 {
///         assert!(site < 100);
///         site + 1
///     }
/// }
///
/// let entry: unsafe extern "C" fn(u64, u64) -> u64 = answer;
/// assert_eq!(unsafe { entry(0, 6) }, 7);
/// ```
#[macro_export]
macro_rules! host {
    () => {};
    (
        $(#[doc = $doc:literal])* #[guard($fail:expr)] $(#[$m:meta])*
        $v:vis unsafe fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) -> $r:ty $body:block $($more:tt)*
    ) => {
        $(#[doc = $doc])* $(#[$m])*
        $v unsafe extern "C" fn $name $(<$($g: $b),+>)? ($($a)*) -> $r {
            match $crate::status::unwind::pad(|| $body) {
                ::core::option::Option::Some(v) => v,
                ::core::option::Option::None => $fail,
            }
        }
        $crate::host! { $($more)* }
    };
    (
        $(#[doc = $doc:literal])* #[guard($fail:expr)] $(#[$m:meta])*
        $v:vis fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) -> $r:ty $body:block $($more:tt)*
    ) => {
        $(#[doc = $doc])* $(#[$m])*
        $v extern "C" fn $name $(<$($g: $b),+>)? ($($a)*) -> $r {
            match $crate::status::unwind::pad(|| $body) {
                ::core::option::Option::Some(v) => v,
                ::core::option::Option::None => $fail,
            }
        }
        $crate::host! { $($more)* }
    };
    (
        $(#[doc = $doc:literal])* #[unwind] $(#[$m:meta])*
        $v:vis unsafe fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) $(-> $r:ty)? $body:block $($more:tt)*
    ) => {
        $(#[doc = $doc])* $(#[$m])*
        $v unsafe extern "C-unwind" fn $name $(<$($g: $b),+>)? ($($a)*) $(-> $r)? $body
        $crate::host! { $($more)* }
    };
    (
        $(#[doc = $doc:literal])* #[unwind] $(#[$m:meta])*
        $v:vis fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) $(-> $r:ty)? $body:block $($more:tt)*
    ) => {
        $(#[doc = $doc])* $(#[$m])*
        $v extern "C-unwind" fn $name $(<$($g: $b),+>)? ($($a)*) $(-> $r)? $body
        $crate::host! { $($more)* }
    };
    (
        $(#[$m:meta])*
        $v:vis unsafe fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) $(-> $r:ty)? $body:block $($more:tt)*
    ) => {
        $(#[$m])*
        $v unsafe extern "C" fn $name $(<$($g: $b),+>)? ($($a)*) $(-> $r)? $body
        $crate::host! { $($more)* }
    };
    (
        $(#[$m:meta])*
        $v:vis fn $name:ident $(<$($g:ident: $b:path),+>)? ($($a:tt)*) $(-> $r:ty)? $body:block $($more:tt)*
    ) => {
        $(#[$m])*
        $v extern "C" fn $name $(<$($g: $b),+>)? ($($a)*) $(-> $r)? $body
        $crate::host! { $($more)* }
    };
}

/// The most words a host takes past nothing: `call`'s trampolines, one per count.
pub const MAX_ARGS: usize = 8;

/// A host's result as `call_typed` reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRet {
    /// No result (the answer is 0).
    Void,
    /// An integer of 32 bits or fewer: one wasm `i32` on a 32-bit target, zero-extended.
    Narrow,
    /// A 64-bit word: an integer, or a pointer carried as one (the host ABI's i64).
    Word,
}

#[cfg(not(target_pointer_width = "64"))]
mod wasm32;

/// `call` with the callee's parameter widths: bit `k` of `narrow` set = parameter `k` is an integer of 32 bits or fewer,
/// which a 32-bit target (wasm32) passes as an `i32` and an indirect call there must type exactly (a pointer is a 64-bit
/// word in every host's signature, the host ABI). On a 64-bit host every parameter travels in a full integer register,
/// so this IS `call` (the widths are not read).
/// # Safety
/// As `call`; on a 32-bit target `narrow` and `ret` describe the callee's declaration exactly.
#[inline]
pub unsafe fn call_typed(addr: usize, args: &[u64], narrow: u16, ret: HostRet) -> u64 {
    #[cfg(target_pointer_width = "64")]
    {
        let _ = (narrow, ret);
        call(addr, args)
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        wasm32::call(addr, args, narrow, ret)
    }
}

/// Call the `extern "C"` host at `addr` with `args` as integer-class words (at most `MAX_ARGS`), its answer read as one
/// word. Extra words past what the callee declares would be harmless under both supported ABIs (System V, AAPCS64), but
/// the call still dispatches on the exact count so the transmuted type matches the declaration.
/// # Safety
/// `addr` is an `extern "C"` function whose parameters are `args.len()` integer-class words and whose result is
/// integer-class or void (reading a garbage return register for a void function is fine).
pub unsafe fn call(addr: usize, args: &[u64]) -> u64 {
    use core::mem::transmute as t;
    let a = args;
    match a.len() {
        0 => t::<usize, extern "C" fn() -> u64>(addr)(),
        1 => t::<usize, extern "C" fn(u64) -> u64>(addr)(a[0]),
        2 => t::<usize, extern "C" fn(u64, u64) -> u64>(addr)(a[0], a[1]),
        3 => t::<usize, extern "C" fn(u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2]),
        4 => t::<usize, extern "C" fn(u64, u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2], a[3]),
        5 => t::<usize, extern "C" fn(u64, u64, u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2], a[3], a[4]),
        6 => t::<usize, extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2], a[3], a[4], a[5]),
        7 => t::<usize, extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2], a[3], a[4], a[5], a[6]),
        _ => t::<usize, extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64>(addr)(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]),
    }
}

/// A host's address for the IR when its Rust signature takes or answers typed pointers (or `usize`) rather than the host
/// ABI's 64-bit words (`crates/gravity/lint/tests/host_decls.rs`): `host_words!(path::f(ctx, n, out))`, the host and its
/// parameters' names. On a 64-bit target it IS `path::f as *const () as usize` (one register per word whatever the Rust
/// type: nothing changes). On wasm32 a pointer is an `i32` and an indirect call must name the callee's exact type, so it
/// is the address of a word-typed `extern "C-unwind"` wrapper: every parameter an `i64` word converted to the host's own
/// parameter type (`FromWord`, inferred from the host's signature), the answer converted back (`IntoWord`), which is what
/// the IR's call types (the interpreter's `call_typed`, the wasm backend's) name for `Ptr` and `I64`.
#[macro_export]
macro_rules! host_words {
    ($($f:ident)::+ ($($a:ident),* $(,)?)) => {{
        #[cfg(target_pointer_width = "64")]
        {
            $($f)::+ as *const () as usize
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            #[allow(unsafe_code)]
            extern "C-unwind" fn words($($a: u64),*) -> u64 {
                // SAFETY: the IR passes each argument as the word of the value the host takes (its declaration's `Ptr` /
                // `I64` / narrow types, the audit's pairing), so each converted value is the host's own argument.
                $crate::host::words::IntoWord::into_word(unsafe { $($f)::+($($crate::host::words::FromWord::from_word($a)),*) })
            }
            words as *const () as usize
        }
    }};
}

/// `host_words!`'s conversions between a host's Rust parameter / answer types and the IR's 64-bit words.
pub mod words {
    /// A host parameter's Rust type from its 64-bit word.
    pub trait FromWord {
        fn from_word(w: u64) -> Self;
    }
    /// A host answer's Rust type as its 64-bit word (a pointer zero-extended, as the IR's `Ptr` reads it, but for a status
    /// it carries: `ptr_word`).
    pub trait IntoWord {
        fn into_word(self) -> u64;
    }

    /// D3762: the top of a 32-bit address space that holds no allocation, only the statuses a pointer answer carries.
    /// A host that answers a pointer answers its failure as one too (`ERROR as *mut u8`, `guard!(ERROR as *mut u8, ..)`),
    /// and the IR reads the word below zero as that status (a sort's open: `> 0` the run, `0` done, else the status). On
    /// wasm32 the pointer holds the status's low 32 bits; zero-extended they read as a live pointer just below 4 GiB,
    /// and the program ran on with the failure dropped. 1 MiB holds every status a 32-bit pointer can carry whole (a
    /// raise mask of every code: `raised((1 << raise::CODES) - 1)`); the server binary's link caps a wasm32 memory below
    /// it (main_main build.rs `--max-memory`, 4 GiB less this window), so no live pointer is in it.
    pub const STATUS_WINDOW: u32 = 1 << 20;

    /// A 32-bit pointer answer's word: zero-extended, or sign-extended back to its status inside `STATUS_WINDOW`.
    #[inline(always)]
    pub const fn ptr32_word(p: u32) -> u64 {
        if p > u32::MAX - STATUS_WINDOW {
            p as i32 as i64 as u64
        } else {
            p as u64
        }
    }

    /// A pointer answer's word: on a 64-bit target the pointer's own (a status is its word already), on a 32-bit one
    /// `ptr32_word`.
    #[inline(always)]
    pub fn ptr_word(p: usize) -> u64 {
        if usize::BITS == 32 {
            ptr32_word(p as u32)
        } else {
            p as u64
        }
    }
    macro_rules! ints {
        ($($t:ty),*) => {$(
            impl FromWord for $t {
                #[inline(always)]
                fn from_word(w: u64) -> Self { w as $t }
            }
            impl IntoWord for $t {
                #[inline(always)]
                fn into_word(self) -> u64 { self as u64 }
            }
        )*};
    }
    ints!(u64, i64, usize, isize, u32, i32, u16, i16, u8, i8);
    impl<T> FromWord for *mut T {
        #[inline(always)]
        fn from_word(w: u64) -> Self { w as usize as *mut T }
    }
    impl<T> FromWord for *const T {
        #[inline(always)]
        fn from_word(w: u64) -> Self { w as usize as *const T }
    }
    impl<T> IntoWord for *mut T {
        #[inline(always)]
        fn into_word(self) -> u64 { ptr_word(self as usize) }
    }
    impl<T> IntoWord for *const T {
        #[inline(always)]
        fn into_word(self) -> u64 { ptr_word(self as usize) }
    }
    impl IntoWord for () {
        #[inline(always)]
        fn into_word(self) -> u64 { 0 }
    }
    impl FromWord for bool {
        #[inline(always)]
        fn from_word(w: u64) -> Self { w != 0 }
    }
    impl IntoWord for bool {
        #[inline(always)]
        fn into_word(self) -> u64 { self as u64 }
    }

}
