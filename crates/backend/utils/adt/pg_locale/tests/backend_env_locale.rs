//! pg_locale.c:283 pg_perm_setlocale setenv()s the permanent category, so in a
//! C backend the empty locale name "" resolves to the database's locale, not
//! the postmaster's environment. Threaded backends resolve it from their
//! per-thread records instead (own process: the freeze is global).

#[test]
fn empty_locale_name_resolves_to_backend_record() {
    std::env::remove_var("LC_ALL");
    std::env::set_var("LC_COLLATE", "POSIX");
    pg_locale::freeze_global_locale();
    let cx = mcx::MemoryContext::new("t");
    assert!(pg_locale::pg_perm_setlocale(cx.mcx(), libc::LC_COLLATE, "C").unwrap().is_some());
    let (ok, canon) = pg_locale::check_locale(libc::LC_COLLATE, "").unwrap();
    assert!(ok);
    assert_eq!(canon.as_deref(), Some("C"));
}

extern "C" {
    fn mbstowcs(dest: *mut libc::wchar_t, src: *const libc::c_char, n: usize) -> usize;
}

fn wide_len(s: &std::ffi::CStr) -> usize {
    let mut buf = [0 as libc::wchar_t; 8];
    // SAFETY: s is NUL-terminated; at most 8 wchars written.
    unsafe { mbstowcs(buf.as_mut_ptr(), s.as_ptr(), 8) }
}

#[test]
fn backend_ctype_scopes_null_locale_calls_and_restores() {
    pg_locale::freeze_global_locale();
    let cx = mcx::MemoryContext::new("t");
    assert!(pg_locale::pg_perm_setlocale(cx.mcx(), libc::LC_CTYPE, "en_US.UTF-8").unwrap().is_some());
    // SAFETY: querying the thread locale.
    let before = unsafe { libc::uselocale(core::ptr::null_mut()) };
    assert_eq!(pg_locale::with_backend_ctype(|| wide_len(c"caf\u{e9}")), 4);
    let r = std::panic::catch_unwind(|| pg_locale::with_backend_ctype(|| panic!("unwind")));
    assert!(r.is_err());
    // SAFETY: as above.
    assert_eq!(unsafe { libc::uselocale(core::ptr::null_mut()) }, before);
}
