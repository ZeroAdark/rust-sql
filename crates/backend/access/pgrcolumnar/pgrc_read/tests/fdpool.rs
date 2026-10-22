//! The part-file descriptor pool (D2873): open part fds stay within the budget however many parts are open, every
//! read returns its own file's bytes across closes and reopens, and a reopen that finds another file at the path (a
//! dropped part replaced, renamed over, truncated, or gone) is refused, never read.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pgrc_read::fdpool;
use pgrc_read::io::{PartIo, VfsPartIo};
use pgrc_read::ReadError;

/// The pool is process-wide: the tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const LEN: usize = 4096;

fn byte(file: usize, i: usize) -> u8 {
    ((file * 7 + i) % 251) as u8
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pgrc-fdpool-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_part(dir: &Path, file: usize) -> CString {
    let p = dir.join(format!("part-{file}"));
    std::fs::write(&p, (0..LEN).map(|i| byte(file, i)).collect::<Vec<u8>>()).unwrap();
    CString::new(p.to_str().unwrap()).unwrap()
}

/// Descriptors this process holds on files under `dir` (Linux: /proc/self/fd).
fn fds_under(dir: &Path) -> Option<usize> {
    let rd = std::fs::read_dir("/proc/self/fd").ok()?;
    Some(rd.filter_map(|e| std::fs::read_link(e.ok()?.path()).ok()).filter(|t| t.starts_with(dir)).count())
}

#[test]
fn open_fds_stay_within_the_budget_and_every_read_is_its_own_files() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    fdpool::set_budget(4);
    let dir = scratch("budget");
    let parts: Vec<Arc<VfsPartIo>> = (0..64).map(|f| Arc::new(VfsPartIo::open(&write_part(&dir, f)).unwrap())).collect();
    assert!(fdpool::census().0 <= 4, "open after 64 opens: {:?}", fdpool::census());
    if let Some(n) = fds_under(&dir) {
        assert!(n <= 4, "{n} fds held under budget 4");
    }
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let parts = parts.clone();
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut x = t * 2654435761 + 1;
                let mut max_fds = 0;
                for _ in 0..3000 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let f = (x >> 33) as usize % parts.len();
                    let off = (x >> 13) as usize % (LEN - 64);
                    let mut buf = [0u8; 64];
                    parts[f].pread_exact(off as u64, &mut buf, "test read").unwrap();
                    assert!(buf.iter().enumerate().all(|(i, b)| *b == byte(f, off + i)), "part {f} at {off}: another file's bytes");
                    if let Some(n) = fds_under(&dir) {
                        max_fds = max_fds.max(n);
                    }
                }
                max_fds
            })
        })
        .collect();
    for t in threads {
        let max_fds = t.join().unwrap();
        assert!(max_fds <= 4 + 8, "{max_fds} fds held: budget 4 + 8 readers' leases at most");
    }
    let (open, budget, opens, reopens, closes, _) = fdpool::census();
    assert_eq!(budget, 4);
    assert!(open <= 4 && opens >= 64 && reopens > 0 && closes > 0, "census {:?}", fdpool::census());
    drop(parts);
    assert_eq!(fds_under(&dir).unwrap_or(0), 0, "a dropped part keeps no fd");
    fdpool::set_budget(0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Open `a`, then `b` with budget 1, which closes `a`'s fd: the next read of `a` reopens it by path.
fn closed_behind(dir: &Path) -> (VfsPartIo, VfsPartIo, CString) {
    fdpool::set_budget(1);
    let pa = write_part(dir, 1);
    let a = VfsPartIo::open(&pa).unwrap();
    let b = VfsPartIo::open(&write_part(dir, 2)).unwrap();
    let mut buf = [0u8; 8];
    b.pread_exact(0, &mut buf, "evict a").unwrap();
    (a, b, pa)
}

fn read_a(a: &VfsPartIo) -> Result<[u8; 16], ReadError> {
    let mut buf = [0u8; 16];
    a.pread_exact(100, &mut buf, "read a").map(|_| buf)
}

#[test]
fn a_reopen_reads_the_same_file() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch("same");
    let (a, _b, _) = closed_behind(&dir);
    let got = read_a(&a).unwrap();
    assert!(got.iter().enumerate().all(|(i, x)| *x == byte(1, 100 + i)));
    fdpool::set_budget(0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reopen_refuses_another_file_at_the_path() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch("refuse");
    let other = || vec![0xEEu8; LEN];

    // renamed over by a new file of the same length (a rolled-back generation's name taken again)
    let (a, _b, pa) = closed_behind(&dir);
    let tmp = dir.join("tmp");
    std::fs::write(&tmp, other()).unwrap();
    std::fs::rename(&tmp, pa.to_str().unwrap()).unwrap();
    assert!(matches!(read_a(&a), Err(ReadError::OpenMismatch { .. })), "renamed over: {:?}", read_a(&a));
    drop(a);

    // dropped, then a new file written at the same path (the inode number may be reused: mtime tells them apart)
    let (a, _b, pa) = closed_behind(&dir);
    std::fs::remove_file(pa.to_str().unwrap()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(pa.to_str().unwrap(), other()).unwrap();
    assert!(matches!(read_a(&a), Err(ReadError::OpenMismatch { .. })), "dropped and rewritten: {:?}", read_a(&a));
    drop(a);

    // truncated in place
    let (a, _b, pa) = closed_behind(&dir);
    std::fs::OpenOptions::new().write(true).open(pa.to_str().unwrap()).unwrap().set_len(LEN as u64 / 2).unwrap();
    assert!(matches!(read_a(&a), Err(ReadError::OpenMismatch { .. })), "truncated: {:?}", read_a(&a));
    drop(a);

    // dropped and gone
    let (a, _b, pa) = closed_behind(&dir);
    std::fs::remove_file(pa.to_str().unwrap()).unwrap();
    assert!(matches!(read_a(&a), Err(ReadError::Io { errno: libc::ENOENT, .. })), "gone: {:?}", read_a(&a));
    drop(a);

    fdpool::set_budget(0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The crash-matrix row for a reopen racing a DROP: readers keep reading part `a` while its fd is closed behind them
/// again and again (eight other parts cycling through a budget of 2) and another thread drops `a` and writes other
/// files at its path; every read is `a`'s bytes (its still-open fd, or a reopen of the same file) or a refusal, never
/// another file's. Once the race is over, a reopen of `a` is refused.
#[test]
fn a_reopen_racing_a_drop_never_reads_another_file() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch("race");
    fdpool::set_budget(2);
    let pa = write_part(&dir, 1);
    let a = Arc::new(VfsPartIo::open(&pa).unwrap());
    let others: Arc<Vec<VfsPartIo>> = Arc::new((2..10).map(|f| VfsPartIo::open(&write_part(&dir, f)).unwrap()).collect());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4usize)
        .map(|t| {
            let (a, others, stop) = (Arc::clone(&a), Arc::clone(&others), Arc::clone(&stop));
            std::thread::spawn(move || {
                let (mut ok, mut refused, mut i) = (0u64, 0u64, t);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut buf = [0u8; 8];
                    i = (i + 1) % others.len();
                    others[i].pread_exact(0, &mut buf, "cycle").unwrap();
                    assert_eq!(buf[0], byte(i + 2, 0), "part {} read another file's bytes", i + 2);
                    match read_a(&a) {
                        Ok(got) => {
                            assert!(got.iter().enumerate().all(|(i, x)| *x == byte(1, 100 + i)), "read another file's bytes");
                            ok += 1;
                        }
                        Err(_) => refused += 1,
                    }
                }
                (ok, refused)
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(50));
    for round in 0..50 {
        let _ = std::fs::remove_file(pa.to_str().unwrap());
        std::fs::write(pa.to_str().unwrap(), vec![round as u8 ^ 0xEE; LEN]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (mut ok, mut refused) = (0, 0);
    for r in readers {
        let (o, f) = r.join().unwrap();
        ok += o;
        refused += f;
    }
    let mut buf = [0u8; 8];
    for o in others.iter() {
        o.pread_exact(0, &mut buf, "close a behind").unwrap();
    }
    assert!(read_a(&a).is_err(), "after the drop a reopen of a must be refused ({ok} reads ok, {refused} refused in the race)");
    fdpool::set_budget(0);
    let _ = std::fs::remove_dir_all(&dir);
}
