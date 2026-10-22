// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

pub const ADDRESS_SANITIZER_ACTIVE: bool = cfg!(tpde_address_sanitizer);

#[cfg(tpde_address_sanitizer)]
extern "C" {
    fn __asan_poison_memory_region(pointer: *const u8, size: usize);
    fn __asan_unpoison_memory_region(pointer: *const u8, size: usize);
}

/// # Safety
/// When ASAN is active, the region must belong to a live allocation, with no
/// concurrent poisoning/unpoisoning of its shadow granules. Poisoned bytes must
/// remain inaccessible until unpoisoned; runtime alignment restrictions apply.
#[inline(always)]
pub unsafe fn poison_memory_region(pointer: *const u8, size: usize) {
    #[cfg(tpde_address_sanitizer)]
    // SAFETY: The caller owns the live region and its shadow-state transitions.
    unsafe {
        __asan_poison_memory_region(pointer, size);
    }
    #[cfg(not(tpde_address_sanitizer))]
    let _ = (pointer, size);
}

/// # Safety
/// When ASAN is active, the region must belong to a live allocation, with no
/// concurrent poisoning/unpoisoning of its shadow granules. Unpoisoning must not
/// expose bytes whose access would violate their allocation's validity.
#[inline(always)]
pub unsafe fn unpoison_memory_region(pointer: *const u8, size: usize) {
    #[cfg(tpde_address_sanitizer)]
    // SAFETY: The caller owns the live region and its shadow-state transitions.
    unsafe {
        __asan_unpoison_memory_region(pointer, size);
    }
    #[cfg(not(tpde_address_sanitizer))]
    let _ = (pointer, size);
}

#[cfg(all(test, not(tpde_address_sanitizer)))]
mod tests {
    use super::*;

    #[test]
    fn disabled_calls_do_not_touch_memory() {
        assert!(!ADDRESS_SANITIZER_ACTIVE);
        let mut bytes = [0xa5; 32];
        let pointer = bytes.as_mut_ptr();
        // SAFETY: Without ASAN these operations impose no pointer validity requirement.
        unsafe {
            poison_memory_region(pointer, bytes.len());
            unpoison_memory_region(pointer, bytes.len());
            poison_memory_region(core::ptr::null(), 0);
            unpoison_memory_region(core::ptr::null(), 0);
            poison_memory_region(core::ptr::null(), usize::MAX);
            unpoison_memory_region(core::ptr::null(), usize::MAX);
        }
        assert_eq!(bytes, [0xa5; 32]);
    }
}
