//! Volatile memory wipe, matching C `explicit_bzero`.
//!
//! In upstream PostgreSQL every session is its own OS process, so freed heap
//! memory (cleartext passwords, SASLprep intermediates, SCRAM verifier
//! temporaries) can never be observed by another user; only the HMAC context
//! is explicitly wiped (`explicit_bzero`, hmac.c:124,295). pgrust runs every
//! session in one shared address space and the mcx allocator recycles freed
//! chunks / keeper blocks across sessions, so any secret left in freed memory
//! can leak into another session's allocation.
//!
//! These helpers overwrite secret buffers with zeroes such that the write
//! cannot be elided by the optimizer, and must be called before the owning
//! buffer's memory is released to any freelist, keeper pool, or global
//! allocator.
//!
//! The stores are `write_volatile`: a volatile store is a language-level side
//! effect the optimizer may not delete even when the buffer is provably dead
//! afterwards. (A plain `write_bytes` followed by `compiler_fence` is NOT
//! enough — the fence only constrains reordering, and LLVM dead-store
//! elimination removed the wipe from release builds.)

#![no_std]

use core::sync::atomic::{compiler_fence, Ordering};

#[inline]
fn wipe_bytes(p: *mut u8, n: usize) {
    for i in 0..n {
        // SAFETY: the caller guarantees `p..p+n` is a valid, exclusively
        // borrowed, initialized byte range.
        unsafe { core::ptr::write_volatile(p.add(i), 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Overwrite `buf` with zero bytes, guaranteeing the write is not optimized
/// away, mirroring the semantics of glibc `explicit_bzero`.
#[inline]
pub fn secure_zero(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    wipe_bytes(buf.as_mut_ptr(), buf.len());
}

/// Wipe a slice of any `Copy`/POD element type by clearing its raw bytes.
///
/// Intended for buffers about to be released that hold secret material in a
/// non-`u8` form, e.g. `pg_wchar` (u32) codepoint arrays carrying a password.
/// The elements are byte-zeroed in place; callers must only use this on
/// buffers that will not be read as `T` again (all-zero is a valid bit
/// pattern for the integer element types this is used with).
#[inline]
pub fn secure_zero_slice<T: Copy>(buf: &mut [T]) {
    let len_bytes = core::mem::size_of_val(buf);
    if len_bytes == 0 {
        return;
    }
    wipe_bytes(buf.as_mut_ptr() as *mut u8, len_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wipes_bytes() {
        let mut buf = *b"hunter2-cleartext-password";
        secure_zero(&mut buf);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn wipes_wchar_slice() {
        let mut buf: [u32; 5] = [0x0041, 0x00e9, 0x1f600, 0xffff, 0x10ffff];
        secure_zero_slice(&mut buf);
        assert!(buf.iter().all(|&c| c == 0));
    }

    #[test]
    fn empty_is_noop() {
        let mut empty: [u8; 0] = [];
        secure_zero(&mut empty);
    }

    // Functional check only: the buffer is read back through a volatile
    // pointer, so this cannot witness dead-store elimination of the wipe
    // itself (scripts/secure-zero-dse-probe.sh inspects release codegen).
    #[test]
    fn not_elided_when_result_unused() {
        let mut secret = [0xABu8; 64];
        // Touch it so the initial fill isn't itself dead.
        secret[0] = secret.len() as u8;
        let ptr = secret.as_ptr();
        secure_zero(&mut secret);
        for i in 0..secret.len() {
            // SAFETY: reading initialized in-bounds bytes.
            let b = unsafe { core::ptr::read_volatile(ptr.add(i)) };
            assert_eq!(b, 0, "byte {i} not wiped");
        }
    }

    #[test]
    fn wipes_odd_sizes_and_wide_elements() {
        for n in [1usize, 3, 7, 17, 100] {
            let mut v = [0x5Au8; 100];
            secure_zero(&mut v[..n]);
            assert!(v[..n].iter().all(|&b| b == 0));
            assert!(v[n..].iter().all(|&b| b == 0x5A));
        }
        let mut wide: [u64; 3] = [u64::MAX; 3];
        secure_zero_slice(&mut wide[..2]);
        assert_eq!(wide, [0, 0, u64::MAX]);
    }
}
