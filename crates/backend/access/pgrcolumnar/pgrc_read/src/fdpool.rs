//! The process-wide part-file descriptor pool (D2873). A [`crate::io::VfsPartIo`] holds a slot here, not an fd: the
//! fd is opened on demand and the least recently used idle one is closed when the pool is at its budget, fd.c's VFD
//! law made process-wide because a part is shared by every session and the gravity pool's threads. The budget is the
//! slice of RLIMIT_NOFILE the parts take before `fd` divides the rest among the sessions (`vfs::part_fd_slice`),
//! sized by demand: the part files this pool holds plus a floor, at most the cap `fd::set_max_safe_fds` set. The pool
//! reports what it holds at every open and release (`vfs::set_part_fd_demand`), and each session's file-cache limit
//! follows, so parts never take the descriptors a session keeps for WAL, pg_control, relation and temp files, and a
//! server with few or no part files keeps the sessions' share as it was before parts were counted.
//!
//! A slot's state is one word, `(fd + 1) << 32 | leases`. A read leases the open fd with one CAS and gives it back
//! after its syscall; the clock closes a slot only by moving it from `(fd, 0 leases)` to closed in one CAS, so no
//! lease ever sees its fd closed. A reopen must find the identity the first open recorded (`dev`, `ino`, `mtime`, and
//! the length `OpenPart::open` checked against the manifest): a renamed-over or replaced file is refused typed, never
//! read.

use core::ffi::CStr;
use core::sync::atomic::{AtomicU64 as Counter, AtomicUsize as Budget};
use std::ffi::CString;
use std::sync::Arc;

use pgsync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::{ReadError, ReadResult};

/// The budget when neither a test nor `fd` set one (no RLIMIT_NOFILE known: the sim world, a library harness).
pub const DEFAULT_BUDGET: usize = 1024;

const CLOSED: u64 = 0;
const LEASES: u64 = 0xffff_ffff;

static BUDGET: Budget = Budget::new(0);
static OPENS: Counter = Counter::new(0);
static REOPENS: Counter = Counter::new(0);
static CLOSES: Counter = Counter::new(0);
static OVER_BUDGET: Counter = Counter::new(0);

struct Pool {
    /// The slots whose fd is open, in no order; `Slot::pos` is each one's index.
    open: Vec<Arc<Slot>>,
    hand: usize,
    /// The slots alive (opened, not yet released): the pool's demand on the descriptor budget.
    slots: usize,
}

pgsync::process_global! {
    static POOL: pgsync::Mutex<Pool> = pgsync::Mutex::new(Pool { open: Vec::new(), hand: 0, slots: 0 });
}

/// One part file's descriptor slot.
pub struct Slot {
    path: CString,
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    state: AtomicU64,
    used: AtomicBool,
    pos: AtomicUsize,
}

/// An open fd leased for one syscall.
pub struct Lease<'a> {
    slot: &'a Slot,
    pub fd: libc::c_int,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.slot.state.fetch_sub(1, Ordering::Release);
    }
}

/// The pool's census: `(open now, budget, opens, reopens, closes, opens past the budget)`.
pub fn census() -> (usize, usize, u64, u64, u64, u64) {
    let open = pgsync::lock(&POOL).open.len();
    (open, budget(), OPENS.load(Ordering::Relaxed), REOPENS.load(Ordering::Relaxed), CLOSES.load(Ordering::Relaxed), OVER_BUDGET.load(Ordering::Relaxed))
}

/// Set the budget (0 = `fd`'s slice for this pool's demand, else [`DEFAULT_BUDGET`]). Lowering it closes idle fds at
/// the next open.
pub fn set_budget(n: usize) {
    BUDGET.store(n, Ordering::Relaxed);
}

pub fn budget() -> usize {
    match BUDGET.load(Ordering::Relaxed) {
        0 => match vfs::part_fd_slice() {
            0 => DEFAULT_BUDGET,
            n => n,
        },
        n => n.max(1),
    }
}

fn fd_of(state: u64) -> Option<libc::c_int> {
    (state >> 32 != 0).then(|| ((state >> 32) - 1) as libc::c_int)
}

/// Open `path` read-only into a new slot and gather its identity (one fstat, before any content read).
pub fn open(path: &CStr) -> ReadResult<Arc<Slot>> {
    let mut pool = pgsync::lock(&POOL);
    let fd = open_fd(&mut pool, path, "part open")?;
    let mut info = vfs::FileInfo::zeroed();
    if vfs::fstat(fd, &mut info) != 0 {
        let errno = vfs::get_errno();
        vfs::close(fd);
        return Err(ReadError::Io { at: "part fstat", errno });
    }
    let slot = Arc::new(Slot {
        path: path.to_owned(),
        dev: info.dev,
        ino: info.ino,
        len: info.size.max(0) as u64,
        mtime: (info.mtime_sec, info.mtime_nsec),
        state: AtomicU64::new(((fd as u64) + 1) << 32),
        used: AtomicBool::new(true),
        pos: AtomicUsize::new(pool.open.len()),
    });
    pool.open.push(Arc::clone(&slot));
    pool.slots += 1;
    vfs::set_part_fd_demand(pool.slots);
    OPENS.fetch_add(1, Ordering::Relaxed);
    Ok(slot)
}

/// Room for one more fd, then the open; an EMFILE / ENFILE closes one more idle fd and tries once again.
fn open_fd(pool: &mut Pool, path: &CStr, at: &'static str) -> ReadResult<libc::c_int> {
    let budget = budget();
    // bounded("other: one idle fd closed a pass, down to the fd budget (DEFAULT_BUDGET 1024 or the vfs part-fd slice); over it by one but after a budget cut")
    while pool.open.len() >= budget {
        if !evict_one(pool) {
            OVER_BUDGET.fetch_add(1, Ordering::Relaxed);
            break;
        }
    }
    for attempt in 0..2 {
        let fd = vfs::open(path, libc::O_RDONLY | libc::O_CLOEXEC, 0);
        if fd >= 0 {
            return Ok(fd);
        }
        let errno = vfs::get_errno();
        if attempt == 0 && (errno == libc::EMFILE || errno == libc::ENFILE) && evict_one(pool) {
            continue;
        }
        return Err(ReadError::Io { at, errno });
    }
    unreachable!()
}

/// The clock: clear the used bit of each open slot it passes, close the first idle one it finds unused.
fn evict_one(pool: &mut Pool) -> bool {
    let n = pool.open.len();
    // bounded("other: the clock's two passes over the open fds, at most the fd budget (DEFAULT_BUDGET 1024 or the vfs part-fd slice)")
    for _ in 0..2 * n {
        if pool.hand >= pool.open.len() {
            pool.hand = 0;
        }
        let slot = &pool.open[pool.hand];
        if slot.used.swap(false, Ordering::Relaxed) {
            pool.hand += 1;
            continue;
        }
        let s = slot.state.load(Ordering::Acquire);
        if s & LEASES == 0 && s != CLOSED && slot.state.compare_exchange(s, CLOSED, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            let i = pool.hand;
            close_at(pool, i, s);
            return true;
        }
        pool.hand += 1;
    }
    false
}

/// Close the fd of the open slot at `i` (its state already moved to closed from `s`) and take it off the list.
fn close_at(pool: &mut Pool, i: usize, s: u64) {
    if let Some(fd) = fd_of(s) {
        vfs::close(fd);
    }
    let last = pool.open.len() - 1;
    pool.open.swap(i, last);
    pool.open.pop();
    if i < pool.open.len() {
        pool.open[i].pos.store(i, Ordering::Relaxed);
    }
    CLOSES.fetch_add(1, Ordering::Relaxed);
}

/// Close a slot's fd for good (its `VfsPartIo` is dropping, so no lease is outstanding).
pub fn release(slot: &Arc<Slot>) {
    let mut pool = pgsync::lock(&POOL);
    pool.slots = pool.slots.saturating_sub(1);
    vfs::set_part_fd_demand(pool.slots);
    let s = slot.state.swap(CLOSED, Ordering::AcqRel);
    if s != CLOSED {
        let i = slot.pos.load(Ordering::Relaxed);
        if pool.open.get(i).is_some_and(|o| Arc::ptr_eq(o, slot)) {
            close_at(&mut pool, i, s);
        }
    }
}

impl Slot {
    pub fn dev_ino(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// The modification time the first open's fstat recorded (the reopen checks it; the registry compares it on a hit).
    pub fn mtime(&self) -> (i64, i64) {
        self.mtime
    }
}

/// The open fd for one syscall, reopened by path when the clock closed it.
#[inline]
pub fn lease(slot: &Arc<Slot>) -> ReadResult<Lease<'_>> {
    let mut s = slot.state.load(Ordering::Acquire);
    // bounded("other: a compare-and-swap retry, which repeats only when another thread's lease or close of this slot won the race")
    while let Some(fd) = fd_of(s) {
        match slot.state.compare_exchange_weak(s, s + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if !slot.used.load(Ordering::Relaxed) {
                    slot.used.store(true, Ordering::Relaxed);
                }
                return Ok(Lease { slot, fd });
            }
            Err(now) => s = now,
        }
    }
    reopen(slot)
}

#[cold]
fn reopen(slot: &Arc<Slot>) -> ReadResult<Lease<'_>> {
    let mut pool = pgsync::lock(&POOL);
    // another thread reopened it while this one waited for the lock
    let s = slot.state.load(Ordering::Acquire);
    if let Some(fd) = fd_of(s) {
        slot.state.fetch_add(1, Ordering::AcqRel);
        return Ok(Lease { slot, fd });
    }
    let fd = open_fd(&mut pool, &slot.path, "part reopen")?;
    let mut info = vfs::FileInfo::zeroed();
    if vfs::fstat(fd, &mut info) != 0 {
        let errno = vfs::get_errno();
        vfs::close(fd);
        return Err(ReadError::Io { at: "part reopen fstat", errno });
    }
    // mtime too: a part is never written after its publish, and an inode number reused by a new file at this path
    // (the part dropped, another written) carries the new file's mtime
    if (info.dev, info.ino, info.size.max(0) as u64, (info.mtime_sec, info.mtime_nsec)) != (slot.dev, slot.ino, slot.len, slot.mtime) {
        vfs::close(fd);
        return Err(ReadError::OpenMismatch { field: "part file identity at reopen (dev, ino, len, mtime)" });
    }
    if crate::io::fadv_random_armed() {
        let _ = vfs::fadvise_random(fd);
    }
    slot.pos.store(pool.open.len(), Ordering::Relaxed);
    slot.used.store(true, Ordering::Relaxed);
    slot.state.store((((fd as u64) + 1) << 32) | 1, Ordering::Release);
    pool.open.push(Arc::clone(slot));
    REOPENS.fetch_add(1, Ordering::Relaxed);
    Ok(Lease { slot, fd })
}
