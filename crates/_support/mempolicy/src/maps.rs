//! Names the server's anonymous mappings in smaps (`[anon:pgrust:<site>]`) while the trace is on; off, one load.

use memledger::Kind;

/// A refusal (an older kernel, a file mapping) leaves the range unnamed.
#[inline]
pub fn name(p: *mut u8, len: usize, what: &'static core::ffi::CStr) {
    if crate::trace::on() {
        set(p, len, what);
    }
}

pub fn region(k: Kind) -> &'static core::ffi::CStr {
    match k {
        Kind::Region => c"pgrust:region",
        Kind::HashBuild => c"pgrust:region:hash_build",
        Kind::HashAgg => c"pgrust:region:hash_agg",
        Kind::PlainAgg => c"pgrust:region:plain_agg",
        Kind::Distinct => c"pgrust:region:distinct",
        Kind::Sort => c"pgrust:region:sort",
        Kind::TopK => c"pgrust:region:top_k",
        Kind::Spool => c"pgrust:region:spool",
        Kind::Spill => c"pgrust:region:spill",
        Kind::Arena => c"pgrust:region:arena",
        Kind::Mapped => c"pgrust:region:mapped",
    }
}

#[cfg(target_os = "linux")]
#[cold]
fn set(p: *mut u8, len: usize, what: &'static core::ffi::CStr) {
    use core::ffi::{c_int, c_ulong};
    extern "C" {
        fn prctl(option: c_int, ...) -> c_int;
    }
    const PR_SET_VMA: c_int = 0x5356_4d41;
    const PR_SET_VMA_ANON_NAME: c_ulong = 0;
    // SAFETY: prctl only reads the NUL-terminated name (the kernel copies it) and the range's address and length; a range
    // that is not wholly anonymous mappings is refused, nothing is changed then.
    let _ = unsafe { prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, p as c_ulong, len as c_ulong, what.as_ptr() as c_ulong) };
}

#[cfg(not(target_os = "linux"))]
fn set(_p: *mut u8, _len: usize, _what: &'static core::ffi::CStr) {}

#[cfg(test)]
mod tests {
    use memledger::Kind;

    #[test]
    fn region_names_follow_the_ledger_kinds() {
        for k in Kind::ALL {
            let want = if k == Kind::Region { "pgrust:region".to_owned() } else { format!("pgrust:region:{}", k.name()) }; // alloc: Cold
            assert_eq!(super::region(k).to_str(), Ok(want.as_str()));
        }
    }

    /// The trace switch itself is not toggled here: other tests read it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_named_mapping_shows_its_name() {
        extern "C" {
            fn mmap(addr: *mut core::ffi::c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut core::ffi::c_void;
            fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
        }
        let len = 1 << 20;
        // SAFETY: a fresh anonymous private mapping (PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS).
        let p = unsafe { mmap(core::ptr::null_mut(), len, 3, 0x02 | 0x20, -1, 0) } as *mut u8;
        assert!(!p.is_null() && p as usize != usize::MAX);
        super::set(p, len, c"pgrust:test");
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let at = format!("{:x}-", p as usize); // alloc: Cold
        let line = maps.lines().find(|l| l.starts_with(&at)).unwrap_or_default();
        let config = std::fs::read_to_string(format!("/boot/config-{}", std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim())); // alloc: Cold
        if config.is_ok_and(|c| c.contains("CONFIG_ANON_VMA_NAME=y")) {
            assert!(line.contains("[anon:pgrust:test]"), "{line}");
        }
        // SAFETY: the mapping made above, unmapped once.
        unsafe { munmap(p as *mut core::ffi::c_void, len) };
    }
}
