//! The interpreter's raw memory and host calls (rule R6, its raw home). Every address here comes from the program being
//! run: the IR producer guarantees it is valid and (unless `unaligned`) aligned, the trusted-lowerer contract stated in
//! the gravity_ir crate docs; a host address is a `HostFnDecl`'s. These are the interpreter's only unsafe steps.
#![allow(unsafe_code)]

/// `LoadZext`: 1, 2 or (any other width) 4 bytes at `p`, zero-extended.
#[inline]
pub(crate) fn load_zext(p: *const u8, bytes: u32, unaligned: bool) -> u64 {
    // SAFETY: same contract as `Load`.
    unsafe {
        match bytes {
            1 => *p as u64,
            2 => read::<u16>(p, unaligned) as u64,
            _ => read::<u32>(p, unaligned) as u64,
        }
    }
}

/// `Load`: 1, 2, 4 or (any other width) 8 bytes at `p`, zero-extended (the caller canonicalises).
#[inline]
pub(crate) fn load(p: *const u8, bytes: u32, unaligned: bool) -> u64 {
    // SAFETY: the IR producer guarantees the address is valid and
    // (unless `unaligned`) aligned; this is the trusted-lowerer
    // contract stated in the gravity_ir crate docs.
    unsafe {
        match bytes {
            1 => *p as u64,
            2 => read::<u16>(p, unaligned) as u64,
            4 => read::<u32>(p, unaligned) as u64,
            _ => read::<u64>(p, unaligned),
        }
    }
}

/// `Store`: the low 1, 2, 4 or (any other width) 8 bytes of `v` at `p`.
#[inline]
pub(crate) fn store(p: *mut u8, bytes: u32, v: u64, unaligned: bool) {
    // SAFETY: as for `Load`.
    unsafe {
        match bytes {
            1 => *p = v as u8,
            2 => write::<u16>(p, v as u16, unaligned),
            4 => write::<u32>(p, v as u32, unaligned),
            _ => write::<u64>(p, v, unaligned),
        }
    }
}

/// `VecLoad`: 16 bytes at `p`.
#[inline]
pub(crate) fn load16(p: *const u8, unaligned: bool) -> [u64; 2] {
    // SAFETY: the trusted-lowerer contract of `Load`; 16 bytes.
    unsafe { read::<[u64; 2]>(p, unaligned) }
}

/// `VecStore`: 16 bytes at `p`.
#[inline]
pub(crate) fn store16(p: *mut u8, v: [u64; 2], unaligned: bool) {
    // SAFETY: as for `Store`.
    unsafe { write::<[u64; 2]>(p, v, unaligned) };
}

#[inline]
unsafe fn read<T: Copy>(p: *const u8, unaligned: bool) -> T {
    if unaligned {
        core::ptr::read_unaligned(p as *const T)
    } else {
        debug_assert_eq!(p as usize % core::mem::align_of::<T>(), 0, "misaligned load");
        core::ptr::read(p as *const T)
    }
}
#[inline]
unsafe fn write<T: Copy>(p: *mut u8, v: T, unaligned: bool) {
    if unaligned {
        core::ptr::write_unaligned(p as *mut T, v)
    } else {
        debug_assert_eq!(p as usize % core::mem::align_of::<T>(), 0, "misaligned store");
        core::ptr::write(p as *mut T, v)
    }
}

/// Call the host at `addr` with integer-class arguments: contract::host's caller half (`gravity_abi::host::call`), one
/// `extern "C"` trampoline per argument count.
pub(crate) fn call_host(addr: usize, args: &[u64], narrow: u16, ret: gravity_abi::host::HostRet) -> u64 {
    // SAFETY: `HostFnDecl` promises `addr` is an `extern "C"` function whose
    // parameters are all integer-class and whose result is integer-class or
    // void (reading a garbage return register for a void function is fine).
    // `narrow` / `ret` type the call on a 32-bit target (wasm32); a 64-bit host ignores them (`call_typed`).
    unsafe { gravity_abi::host::call_typed(addr, args, narrow, ret) }
}
