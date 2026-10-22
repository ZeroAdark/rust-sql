// PosixVfs battery (DST P1 Ruling 3 Class A): every test here builds real-fs
// fixtures (std::env::temp_dir + std::fs::create_dir_all + symlink) and runs
// them against ActiveVfs. Under `--cfg pgrust_sim` ActiveVfs is SimVfs, whose
// namespace is empty — the fixtures ENOENT at the first open. Posix-only by
// construction; SimVfs coverage lives in `sim::tests` (differential golden
// included).
#![cfg(not(pgrust_sim))]

use std::ffi::CString;

use crate::{FileInfo, PG_O_DIRECT};

fn tmpdir(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "vfs-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_owned()
}

fn c(path: &str) -> CString {
    CString::new(path).unwrap()
}

#[test]
fn open_write_read_size_close_unlink() {
    let dir = tmpdir("rw");
    let path = c(&format!("{dir}/f"));

    let fd = crate::open(&path, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600);
    assert!(fd >= 0);

    assert_eq!(crate::pwrite(fd, b"hello world", 0), 11);
    assert_eq!(crate::pwrite(fd, b"WALD", 6), 4);

    let mut buf = [0u8; 11];
    assert_eq!(crate::pread(fd, &mut buf, 0), 11);
    assert_eq!(&buf, b"hello WALDd");

    // Short read past EOF.
    let mut buf2 = [0u8; 32];
    assert_eq!(crate::pread(fd, &mut buf2, 6), 5);

    assert_eq!(crate::file_size(fd), 11);
    assert_eq!(crate::fsync(fd), 0);
    assert_eq!(crate::fdatasync(fd), 0);
    assert_eq!(crate::ftruncate(fd, 5), 0);
    assert_eq!(crate::file_size(fd), 5);
    assert_eq!(crate::close(fd), 0);

    assert_eq!(crate::truncate_path(&path, 2), 0);
    let mut info = FileInfo::zeroed();
    assert_eq!(crate::stat(&path, &mut info), 0);
    assert_eq!(info.size, 2);
    assert!(info.is_file());
    assert!(!info.is_dir());

    assert_eq!(crate::unlink(&path), 0);
    assert_eq!(crate::stat(&path, &mut info), -1);
    assert_eq!(crate::get_errno(), libc::ENOENT);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vectored_io_roundtrip() {
    let dir = tmpdir("vec");
    let path = c(&format!("{dir}/v"));
    let fd = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(fd >= 0);

    let a = *b"abcd";
    let b = *b"efgh";
    let iov = [
        libc::iovec { iov_base: a.as_ptr() as *mut _, iov_len: 4 },
        libc::iovec { iov_base: b.as_ptr() as *mut _, iov_len: 4 },
    ];
    assert_eq!(crate::pwritev(fd, &iov, 0), 8);

    let mut r1 = [0u8; 3];
    let mut r2 = [0u8; 5];
    let riov = [
        libc::iovec { iov_base: r1.as_mut_ptr() as *mut _, iov_len: 3 },
        libc::iovec { iov_base: r2.as_mut_ptr() as *mut _, iov_len: 5 },
    ];
    assert_eq!(crate::preadv(fd, &riov, 0), 8);
    assert_eq!(&r1, b"abc");
    assert_eq!(&r2, b"defgh");

    crate::close(fd);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn errno_contract_open_enoent() {
    let path = c("/nonexistent-vfs-test-dir/nope");
    crate::set_errno(0);
    assert_eq!(crate::open(&path, libc::O_RDONLY, 0), -1);
    assert_eq!(crate::get_errno(), libc::ENOENT);
}

#[test]
fn pg_o_direct_accepted() {
    // PG_O_DIRECT must be accepted by open on every platform where it's
    // nonzero (macOS: masked to F_NOCACHE; Linux: real O_DIRECT may be
    // refused by the fs, in which case -1/errno is the documented contract).
    let dir = tmpdir("dio");
    let path = c(&format!("{dir}/d"));
    let plain = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(plain >= 0);
    crate::close(plain);

    let fd = crate::open(&path, libc::O_RDWR | PG_O_DIRECT, 0o600);
    if fd >= 0 {
        crate::close(fd);
    } else {
        // Refusal must come with errno set (EINVAL on fs without O_DIRECT).
        assert_ne!(crate::get_errno(), 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn namespace_ops() {
    let dir = tmpdir("ns");
    let sub = c(&format!("{dir}/sub"));
    assert_eq!(crate::mkdir(&sub, 0o700), 0);

    let mut info = FileInfo::zeroed();
    assert_eq!(crate::stat(&sub, &mut info), 0);
    assert!(info.is_dir());

    let f1 = c(&format!("{dir}/sub/one"));
    let f2 = c(&format!("{dir}/sub/two"));
    let fd = crate::open(&f1, libc::O_WRONLY | libc::O_CREAT, 0o600);
    assert!(fd >= 0);
    crate::close(fd);

    assert_eq!(crate::rename(&f1, &f2), 0);
    assert_eq!(crate::stat(&f1, &mut info), -1);
    assert_eq!(crate::stat(&f2, &mut info), 0);

    let entries: Vec<_> = crate::read_dir(&sub).unwrap().map(Result::unwrap).collect();
    assert_eq!(entries, vec![crate::PgPath::from("two")]);

    // rmdir on non-empty fails with errno; then unlink + rmdir succeed.
    assert_eq!(crate::rmdir(&sub), -1);
    assert!(crate::get_errno() == libc::ENOTEMPTY || crate::get_errno() == libc::EEXIST);
    assert_eq!(crate::unlink(&f2), 0);
    assert_eq!(crate::rmdir(&sub), 0);

    // read_dir on a missing dir: Err(errno), errno also set in the TLS cell.
    crate::set_errno(0);
    let err = crate::read_dir(&sub).err().unwrap();
    assert_eq!(err, libc::ENOENT);
    assert_eq!(crate::get_errno(), libc::ENOENT);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Contract v2: link(2) through the shim.
#[test]
fn link_makes_a_second_name() {
    let dir = tmpdir("hl");
    let a = c(&format!("{dir}/a"));
    let b = c(&format!("{dir}/b"));
    let fd = crate::open(&a, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(fd >= 0);
    assert_eq!(crate::pwrite(fd, b"abc", 0), 3);
    crate::close(fd);
    assert_eq!(crate::link(&a, &b), 0);
    let mut info = FileInfo::zeroed();
    assert_eq!(crate::stat(&b, &mut info), 0);
    assert_eq!(info.nlink, 2);
    assert_eq!(crate::link(&a, &b), -1);
    assert_eq!(crate::get_errno(), libc::EEXIST);
    assert_eq!(crate::unlink(&a), 0);
    assert_eq!(crate::stat(&b, &mut info), 0);
    assert_eq!((info.nlink, info.size), (1, 3));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lstat_sees_symlinks() {
    let dir = tmpdir("ln");
    let target = format!("{dir}/target");
    let link = format!("{dir}/link");
    std::fs::write(&target, b"x").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let clink = c(&link);
    let mut info = FileInfo::zeroed();
    assert_eq!(crate::lstat(&clink, &mut info), 0);
    assert!(info.is_symlink());
    assert_eq!(crate::stat(&clink, &mut info), 0);
    assert!(info.is_file());

    let mut buf = [0u8; 512];
    let n = crate::read_link(&clink, &mut buf);
    assert!(n > 0);
    assert_eq!(&buf[..n as usize], target.as_bytes());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fallocate_convention() {
    let dir = tmpdir("fa");
    let path = c(&format!("{dir}/f"));
    let fd = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(fd >= 0);
    let rc = crate::fallocate(fd, 0, 4096);
    // 0 on success (Linux), positive errno otherwise (macOS: EOPNOTSUPP and
    // the caller zero-extends — fd's FileZero).
    if cfg!(target_os = "linux") {
        assert_eq!(rc, 0);
        assert_eq!(crate::file_size(fd), 4096);
    } else {
        assert!(rc > 0);
    }
    crate::close(fd);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn budget_probe_returns_and_releases() {
    let report = crate::fd_budget_probe_report(16);
    assert!(report.used >= 1);
    assert!(report.used <= 16);
    assert!(report.highest_fd >= report.used);
    // Probe must release everything: a repeat probe reuses the same fd range.
    // The harness is multi-threaded and sibling tests churn fds, so one
    // transient disagreement between neighbouring probes is legal; a probe
    // that LEAKS its dup'd fds shifts every subsequent pair and still fails.
    let mut prev = report;
    let mut stable = false;
    for _ in 0..8 {
        let again = crate::fd_budget_probe_report(16);
        if (prev.used, prev.highest_fd) == (again.used, again.highest_fd) {
            stable = true;
            break;
        }
        prev = again;
    }
    assert!(stable, "consecutive probes never agreed: the probe is leaking fds");
    assert_eq!(crate::fd_budget_probe(16), prev.used as usize);
}

#[test]
fn flush_range_and_fadvise_are_benign() {
    let dir = tmpdir("hint");
    let path = c(&format!("{dir}/h"));
    let fd = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(fd >= 0);
    assert_eq!(crate::pwrite(fd, &[7u8; 8192], 0), 8192);
    // Hints MAY no-op but must not corrupt anything or fail spuriously.
    let _ = crate::flush_range(fd, 0, 8192);
    let _ = crate::fadvise_willneed(fd, 0, 8192);
    let mut buf = [0u8; 8192];
    assert_eq!(crate::pread(fd, &mut buf, 0), 8192);
    assert!(buf.iter().all(|&b| b == 7));
    crate::close(fd);
    let _ = std::fs::remove_dir_all(&dir);
}

/// cachestat: a written file's pages are cached (Linux 6.5+; ENOSYS before, EOPNOTSUPP off Linux), the range's page
/// count follows the page size, an empty range is none, a bad fd fails.
#[test]
fn cachestat_counts_the_cached_pages_of_a_range() {
    let dir = tmpdir("cachestat");
    let path = c(&format!("{dir}/cs"));
    let fd = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(fd >= 0);
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let n = 4 * page as usize;
    assert_eq!(crate::pwrite(fd, &vec![3u8; n], 0), n as isize);
    let mut out = [0u64; 2];
    let rc = crate::cachestat(fd, 0, n as libc::off_t, &mut out);
    if rc == 0 {
        assert_eq!(out, [4, 4], "just written: every page in the cache");
        assert_eq!(crate::cachestat(fd, page as libc::off_t + 1, 1, &mut out), 0);
        assert_eq!(out[1], 1);
        assert_eq!(crate::cachestat(fd, 0, 0, &mut out), 0);
        assert_eq!(out, [0, 0]);
        assert_eq!(crate::cachestat(-1, 0, 4096, &mut out), -1);
        assert_eq!(crate::get_errno(), libc::EBADF);
    } else {
        assert!(matches!(crate::get_errno(), libc::ENOSYS | libc::EOPNOTSUPP), "errno {}", crate::get_errno());
    }
    crate::close(fd);
    let _ = std::fs::remove_dir_all(&dir);
}

// Finding F1b guard, posix arm: VfsFd's drop is exactly close(2) on the real
// descriptor (identical to the OwnedFd it replaced), and into_raw disarms.
// The harness is multi-threaded and fd numbers recycle, so "closed" is
// asserted by inode identity (fstat), never by probing a possibly-reused
// fd number.
#[test]
fn vfsfd_guard_posix_drop_closes_and_into_raw_disarms() {
    let dir = tmpdir("vfsfd");
    let path = c(&format!("{dir}/g"));

    // Drop arm: the kernel fd really closes.
    let raw = crate::open(&path, libc::O_RDWR | libc::O_CREAT, 0o600);
    assert!(raw >= 0);
    let mut before = FileInfo::zeroed();
    assert_eq!(crate::fstat(raw, &mut before), 0);
    // SAFETY: raw is live, minted by the active (posix) vfs, exclusively owned.
    let guard = unsafe { crate::VfsFd::from_raw(raw) };
    drop(guard);
    // Either the number is dead (EBADF) or another thread already recycled
    // it onto a different file — both prove OUR descriptor was closed.
    let mut after = FileInfo::zeroed();
    let rc = crate::fstat(raw, &mut after);
    assert!(
        rc == -1 || (after.dev, after.ino) != (before.dev, before.ino),
        "drop must close(2) the guarded descriptor"
    );

    // Disarm arm: into_raw leaves the fd open for the deliberate close.
    let raw2 = crate::open(&path, libc::O_RDWR, 0o600);
    assert!(raw2 >= 0);
    // SAFETY: as above.
    let guard2 = unsafe { crate::VfsFd::from_raw(raw2) };
    assert_eq!(guard2.into_raw(), raw2);
    let mut still = FileInfo::zeroed();
    assert_eq!(crate::fstat(raw2, &mut still), 0, "into_raw must not close");
    assert_eq!((still.dev, still.ino), (before.dev, before.ino));
    assert_eq!(crate::close(raw2), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// D2873: the part files' share of the descriptor budget follows what the part
// pool holds, so a server with few or no part files keeps the sessions' share.
#[test]
fn part_fd_slice_follows_demand_and_the_sessions_keep_the_rest() {
    use crate::{part_fd_slice_of_demand as slice, session_fd_limit_of as limit, FdBudget};
    // RLIMIT_NOFILE 7000, 134 children, 96 sessions (max_connections 20): cap 333
    let b = FdBudget { for_caches: 6234, sessions: 96, c_bound: 990, reserved: 10, part_floor: 16, part_cap: 333 };
    assert_eq!((slice(0, 16, 333), limit(&b, slice(0, 16, 333))), (16, 54));
    assert_eq!((slice(100, 16, 333), limit(&b, slice(100, 16, 333))), (116, 53));
    assert_eq!((slice(7500, 16, 333), limit(&b, slice(7500, 16, 333))), (333, 51));
    // 65536, 294 children, 176 sessions (max_connections 100): no parts = the share before parts were counted (353)
    let b = FdBudget { for_caches: 63970, sessions: 176, c_bound: 990, reserved: 10, part_floor: 16, part_cap: 26881 };
    assert_eq!(limit(&b, slice(0, 16, 26881)), 353);
    assert_eq!(limit(&b, slice(300, 16, 26881)), 351);
    assert_eq!(limit(&b, slice(100_000, 16, 26881)), 200);
    // 1048576: max_files_per_process binds whatever the parts hold
    let b = FdBudget { for_caches: 1_047_010, sessions: 176, c_bound: 990, reserved: 10, part_floor: 16, part_cap: 65536 };
    assert_eq!(limit(&b, slice(100_000, 16, 65536)), 990);

    // the recorded budget: the pool's demand moves the slice and every session's limit
    crate::set_fd_budget(FdBudget { for_caches: 6234, sessions: 96, c_bound: 990, reserved: 10, part_floor: 16, part_cap: 333 });
    crate::set_part_fd_demand(0);
    assert_eq!((crate::part_fd_slice(), crate::session_fd_limit()), (16, 54));
    crate::set_part_fd_demand(7500);
    assert_eq!((crate::part_fd_slice(), crate::session_fd_limit()), (333, 51));
    crate::set_part_fd_demand(0);
    assert_eq!((crate::part_fd_slice(), crate::session_fd_limit()), (16, 54));
}
