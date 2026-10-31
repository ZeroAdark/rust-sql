//! GL-MEMWATCH-1: the process memory watchdog.
//!
//! pgrust-only (no C counterpart). The motivating incident class
//! (GL-HASHAGG-SPILL-1, GL-TSACCT-1): a query grows process memory by
//! gigabytes — accounted-but-huge or invisible to the per-context ledgers —
//! and the FIRST diagnostic is the cgroup OOM kill, which arrives with no
//! server-side output at all: under Linux overcommit, allocation never
//! fails, so C's dump-on-allocation-failure (and our port of it) never
//! fires. This watchdog makes the server narrate its own memory death
//! BEFORE the killer arrives.
//!
//! Mechanism: one postmaster-lifetime sampler thread ("pg:memwatchdog").
//! Every `pgrust.memory_watchdog_interval` (default 1s) it reads process
//! RSS (/proc/self/status; allocator stats off-Linux), the cgroup v2
//! `memory.current`/`memory.max` when present, the process-wide accounted
//! context block bytes (`mcx::global_footprint`), and the allocator's
//! committed bytes (installed hook, mimalloc `mi_process_info`). Against
//! the armed limit — `pgrust.memory_watchdog_limit`, else the cgroup v2
//! limit — it fires ONCE PER TIER PER EXCURSION at escalating thresholds
//! (base T from `pgrust.memory_watchdog_threshold`; tiers T, T+(100-T)/2,
//! T+3(100-T)/4 — 80/90/95 at the default), each time logging the full
//! ledger INCLUDING the delta between RSS and accounted context bytes (the
//! untracked-allocation / accounting-drift detector), and — under
//! `pgrust.memory_watchdog_dump` — signalling the watchdog's top holders
//! (D4092: `mempolicy::watchdog::DUMP_TOP` backends by its own rank, not
//! every live backend) to dump their memory-context trees through the
//! ported `pg_log_backend_memory_contexts` machinery.
//!
//! Why a timer thread and not query-boundary checkpoints: the incident
//! query never reaches a query boundary — it grows tens of GiB mid-flight
//! and dies there. Morsel-boundary checks only cover the runtime engine
//! (the incident ran on the LEGACY engine). The sampler observes mid-query
//! growth on every engine at ZERO cost on any query path: nothing is added
//! to any per-tuple, per-morsel, or per-query code. Its own cost is one
//! /proc read + a handful of atomic loads per second.
//!
//! The watchdog's own lines go through elog at LOG on its own thread (so
//! they carry log_line_prefix like every other server-log line; the
//! postmaster's prefix and log_timezone are carried over at spawn). The
//! requested per-backend dumps ride the normal ereport LOG path on each
//! backend thread.

use std::sync::atomic::{AtomicPtr, Ordering};

use mempolicy::relief::{Relief, Source};

pub mod ladderlog;
pub mod loops;
pub mod memlog;
pub mod purge;
pub mod release;
pub mod service;

// ---------------------------------------------------------------------------
// Allocator statistics hook (mi_process_info shape); installed by the binary
// that owns the global allocator (mcx::set_allocator_release precedent).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatorStats {
    /// Current working-set bytes as the allocator sees them (precise on
    /// macOS/Windows; estimated from committed memory elsewhere).
    pub current_rss: usize,
    /// Bytes of read/write memory the allocator currently has committed.
    pub current_commit: usize,
}

static ALLOC_STATS: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_allocator_stats(f: fn() -> AllocatorStats) {
    ALLOC_STATS.store(f as *mut (), Ordering::Release);
}

/// Wave-4 floor census: the `pgrust: memctx` dump reports the allocator's
/// process-wide rss/commit next to phys_footprint so allocator retention is
/// separable from stacks and other VM regions.
pub fn read_allocator_stats() -> Option<AllocatorStats> {
    allocator_stats()
}

fn allocator_stats() -> Option<AllocatorStats> {
    let p = ALLOC_STATS.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_allocator_stats stores here, always from the fn type.
    let f: fn() -> AllocatorStats = unsafe { std::mem::transmute(p) };
    Some(f())
}

// ---------------------------------------------------------------------------
// Kernel-side signals: /proc/self/status RSS breakdown (Linux) and the
// cgroup v2 memory files (the container case IS the incident case).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct ProcMem {
    rss_kb: u64,
    anon_kb: u64,
    shmem_kb: u64,
    hwm_kb: u64,
}

fn proc_status() -> Option<ProcMem> {
    let s = read_text("/proc/self/status")?;
    let mut m = ProcMem::default();
    let kb = |v: &str| v.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("VmRSS:") {
            m.rss_kb = kb(v);
        } else if let Some(v) = l.strip_prefix("VmHWM:") {
            m.hwm_kb = kb(v);
        } else if let Some(v) = l.strip_prefix("RssAnon:") {
            m.anon_kb = kb(v);
        } else if let Some(v) = l.strip_prefix("RssShmem:") {
            m.shmem_kb = kb(v);
        }
    }
    Some(m)
}

// The cgroup v2 memory-interface directory for this process, if any.
// /proc/self/cgroup's unified entry is "0::<path>"; with cgroup namespaces
// (containers) the path is usually "/" and the files sit directly under
// /sys/fs/cgroup. Resolved per call — it is two tiny virtual-file reads per
// tick and survives cgroup migration.
fn cgroup_dir() -> Option<std::path::PathBuf> {
    let s = read_text("/proc/self/cgroup")?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let joined = std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    if joined.join("memory.current").exists() {
        return Some(joined);
    }
    let root = std::path::PathBuf::from("/sys/fs/cgroup");
    if root.join("memory.current").exists() {
        return Some(root);
    }
    None
}

/// The watchdog's one file read: the host's virtual files (/proc/self, the cgroup interface), read-only (the allowlisted fs
/// site, lint-determinism.allow).
fn read_text(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_u64(path: &std::path::Path) -> Option<u64> {
    read_text(path)?.trim().parse().ok()
}

/// (memory.current, memory.max) — max None when unbounded ("max").
fn cgroup_mem() -> Option<(u64, Option<u64>)> {
    let dir = cgroup_dir()?;
    let current = read_u64(&dir.join("memory.current"))?;
    let max = match read_text(dir.join("memory.max")) {
        Some(s) if s.trim() == "max" => None,
        Some(s) => s.trim().parse().ok(),
        None => None,
    };
    Some((current, max))
}

// ---------------------------------------------------------------------------
// D3314: the one memory limit.
// ---------------------------------------------------------------------------

/// D3314: the memory limit the server runs under, the ONE reading the watchdog's tiers, the governor's bar, the cancel and
/// Gravity's auto caps (the grant pool, the part cache, the resident tables, the code cache: `gravity_driver::machine::
/// ram_reading` through `mcx::governor::limit`) all take: `pgrust.memory_watchdog_limit`; else the smallest bounded
/// cgroup v2 `memory.max` or `memory.high` over this process's cgroup and every ancestor (a systemd unit's MemoryMax and
/// MemoryHigh sit on the unit, a container's on its slice), never past physical memory; else physical memory. Before this
/// the governor read the leaf's memory.max only and Gravity read MemTotal: under a 16 GiB unit on a 61 GiB box the grant
/// pool armed at 45 GiB, admission never throttled and the kernel's OOM killer took the whole server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MemLimit {
    bytes: u64,
    source: LimitSource,
    /// The cgroup whose memory.max (or memory.high) is the limit (`source` Cgroup).
    cgroup: Option<std::path::PathBuf>,
    /// The cgroup file that set the limit (`source` Cgroup): "memory.max", "memory.high" or v1's
    /// "memory.limit_in_bytes".
    knob: &'static str,
    /// D4290: a memory.high below the memory.max, or with no bounded memory.max (whatever the limit's source).
    soft: Option<SoftLimit>,
}

/// D4290: a cgroup v2 memory.high (the smallest bounded one over the process's cgroup and its ancestors) below the
/// smallest bounded memory.max, or with no memory.max bound. Past memory.high the kernel throttles the cgroup's processes
/// and makes them reclaim, well before memory.max: sized to memory.max, the server ran into 0.5-2.6 s reclaim stalls
/// before any broker or revoke action of its own fired (D4276).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SoftLimit {
    high: u64,
    /// The cgroup whose memory.high it is: its memory.events counts the throttling ('high').
    dir: std::path::PathBuf,
    /// The smallest bounded memory.max; `None`: unlimited.
    max: Option<u64>,
}

fn memory_limit(limit_mb: i32) -> MemLimit {
    let self_cgroup = read_text("/proc/self/cgroup").unwrap_or_default();
    memory_limit_in(std::path::Path::new("/sys/fs/cgroup"), &self_cgroup, limit_mb, physical_memory())
}

/// `root` plays /sys/fs/cgroup, `self_cgroup` the /proc/self/cgroup contents.
fn memory_limit_in(root: &std::path::Path, self_cgroup: &str, limit_mb: i32, physical: Option<u64>) -> MemLimit {
    let soft = soft_limit_in(root, self_cgroup);
    if limit_mb > 0 {
        return MemLimit { bytes: (limit_mb as u64) << 20, source: LimitSource::Guc, soft, ..MemLimit::default() };
    }
    if let Some((dir, lim, knob)) = cgroup_limit(root, self_cgroup) {
        let bytes = physical.map_or(lim, |p| lim.min(p));
        return MemLimit { bytes, source: LimitSource::Cgroup, cgroup: Some(dir), knob, soft };
    }
    match physical {
        Some(p) => MemLimit { bytes: p, source: LimitSource::Physical, soft, ..MemLimit::default() },
        None => MemLimit { soft, ..MemLimit::default() },
    }
}

/// D4290: the cgroup v2 memory.high that throttles before memory.max (`SoftLimit`), if any. Equal limits, memory.high
/// unset ("max") or above memory.max, and v1 hierarchies (no memory.high) have none.
fn soft_limit_in(root: &std::path::Path, self_cgroup: &str) -> Option<SoftLimit> {
    let rel = v2_rel(self_cgroup)?;
    let (dir, high) = min_limit(root, rel, "memory.high")?;
    let max = min_limit(root, rel, "memory.max").map(|(_, m)| m);
    max.map_or(true, |m| high < m).then_some(SoftLimit { high, dir, max })
}

/// The unified hierarchy's path of this process's cgroup (`0::` line), when no v1 hierarchy carries the memory controller.
fn v2_rel(self_cgroup: &str) -> Option<&str> {
    if v1_rel(self_cgroup).is_some() {
        return None;
    }
    Some(self_cgroup.lines().find_map(|l| l.strip_prefix("0::"))?.trim().trim_start_matches('/'))
}

/// The v1 hierarchy that carries the memory controller (a `memory` line), its path.
fn v1_rel(self_cgroup: &str) -> Option<String> {
    self_cgroup.lines().find_map(|l| {
        let mut it = l.splitn(3, ':');
        let (_, ctls, path) = (it.next()?, it.next()?, it.next()?);
        ctls.split(',').any(|c| c == "memory").then(|| path.trim().trim_start_matches('/').to_string())
    })
}

/// D4290: the server log's WARNING at start for a soft limit (`SoftLimit`) and its HINT: both values, and the memory
/// budget sized to (`budget`, the limit read).
fn soft_limit_warning(s: &SoftLimit, budget: u64) -> (String, String) {
    let max = s.max.map_or_else(|| "unlimited".to_string(), |m| format!("{} MiB", m >> 20));
    (
        format!(
            "the cgroup's memory.high ({} MiB) is below its memory.max ({max}): the kernel throttles the server past \
             memory.high; the memory budget is sized to {} MiB",
            s.high >> 20,
            budget >> 20
        ),
        "Set MemoryHigh equal to MemoryMax (memory.high equal to memory.max), or leave it unset.".to_string(),
    )
}

/// D3316: the walk of COPY's container-aware headroom (copy/src/memheadroom.rs `headroom_with` / `walk`), for the limit
/// instead of the headroom: the hierarchy that carries the memory controller (a v1 `memory` line wins: hybrid hosts put
/// memory on v1; else the unified `0::` line), walked from this process's cgroup up to the mount's root inclusive (inside a
/// cgroup namespace the container's own limit sits on the root), the smallest bounded level and its directory. The pod
/// shape bounds an ancestor slice and leaves the leaf at `max`: reading the leaf alone, the watchdog armed at physical RAM
/// and never canceled (an 8 GiB ancestor limit was OOM-killed 3 s in).
/// D4290: on v2, the smaller of the smallest bounded memory.max and the smallest bounded memory.high (the kernel
/// throttles at memory.high), and which file it is.
fn cgroup_limit(root: &std::path::Path, self_cgroup: &str) -> Option<(std::path::PathBuf, u64, &'static str)> {
    if let Some(rel) = v1_rel(self_cgroup) {
        let base = root.join("memory");
        // v1 spells unlimited as a huge number
        let (dir, lim) = base.is_dir().then(|| min_limit(&base, &rel, "memory.limit_in_bytes")).flatten()?;
        return Some((dir, lim, "memory.limit_in_bytes"));
    }
    let rel = v2_rel(self_cgroup)?;
    let max = min_limit(root, rel, "memory.max").map(|(d, l)| (d, l, "memory.max"));
    let high = min_limit(root, rel, "memory.high").map(|(d, l)| (d, l, "memory.high"));
    match (max, high) {
        (Some(m), Some(h)) => Some(if h.1 < m.1 { h } else { m }),
        (m, h) => m.or(h),
    }
}

/// The smallest bounded `file` (numeric, under 2^60; "max" is unbounded) over `base/rel` and its ancestors up to `base`.
fn min_limit(base: &std::path::Path, rel: &str, file: &str) -> Option<(std::path::PathBuf, u64)> {
    let mut best: Option<(std::path::PathBuf, u64)> = None;
    let mut dir = if rel.is_empty() { base.to_path_buf() } else { base.join(rel) };
    loop {
        if let Some(lim) = read_u64(&dir.join(file)).filter(|&l| l < 1 << 60) {
            if best.as_ref().map_or(true, |(_, b)| lim < *b) {
                best = Some((dir.clone(), lim));
            }
        }
        if dir == base {
            break;
        }
        match dir.parent() {
            Some(up) if up.starts_with(base) => dir = up.to_path_buf(),
            _ => break,
        }
    }
    best
}

/// D3314: under a cgroup limit, what the kernel cannot reclaim before it OOM-kills: the limiting cgroup's memory.current
/// less its clean page cache. Kernel memory, page tables, dirty and writeback pages count; clean file pages do not (the
/// kernel drops them first). Under a 16 GiB unit the cgroup held 1.1-3.4 GiB of page cache beside 12.4-14.5 GiB anon.
fn cgroup_unreclaimable(dir: &std::path::Path) -> Option<u64> {
    let current = read_u64(&dir.join("memory.current"))?;
    let stat = read_text(dir.join("memory.stat"))?;
    Some(current.saturating_sub(reclaimable_file(&stat)))
}

/// The number after `key` at the start of a line of `text` (`key` ends with its separator: "anon " or "RssAnon:"), 0 when
/// absent; no allocation.
fn field(text: &[u8], key: &[u8]) -> u64 {
    text.split(|&c| c == b'\n')
        .find_map(|l| l.strip_prefix(key))
        .map_or(0, |v| {
            v.iter()
                .skip_while(|c| c.is_ascii_whitespace())
                .take_while(|c| c.is_ascii_digit())
                .fold(0u64, |a, &c| a.saturating_mul(10).saturating_add(u64::from(c - b'0')))
        })
}

/// D3808: memory.stat's dirty and writeback page cache: file_dirty + file_writeback (bytes).
fn dirty_file(stat: impl AsRef<[u8]>) -> u64 {
    let stat = stat.as_ref();
    field(stat, b"file_dirty ") + field(stat, b"file_writeback ")
}

/// memory.stat's clean page cache: active_file + inactive_file less file_dirty and file_writeback (bytes).
fn reclaimable_file(stat: impl AsRef<[u8]>) -> u64 {
    let stat = stat.as_ref();
    (field(stat, b"active_file ") + field(stat, b"inactive_file ")).saturating_sub(dirty_file(stat))
}

/// D4262: this server's own view of a limiting cgroup (`own` its anon + shmem, the cgroup's memory.current and memory.stat):
/// own, and the cgroup's unreclaimable bytes less the anon + shmem its other processes map. Kernel memory, page tables and
/// dirty or writeback page cache stay in it (no process maps them). Not the trigger (the kernel's OOM decision counts every
/// process in the cgroup, so the line stays `governed_resident`'s): who acts past it (`mempolicy::watchdog::acts`).
fn own_view_in(own: u64, current: u64, stat: &[u8]) -> u64 {
    let others = (field(stat, b"anon ") + field(stat, b"shmem ")).saturating_sub(own);
    own.max(current.saturating_sub(reclaimable_file(stat)).saturating_sub(others))
}

/// D3314: the limit as last read (`refresh_limit`: at `start`, then once a second), so the 100 ms governor path walks no
/// cgroup tree. Only `try_lock`: the watchdog thread is outside the modeled world, so it never waits on the lock (a busy
/// lock skips the store, retried at the next sample; a reader that finds it busy reads the files itself).
static LIMIT: pgsync::Mutex<Option<MemLimit>> = pgsync::Mutex::new(None);

/// -> the limit read now, and whether it was stored in `LIMIT`
fn store_limit() -> (MemLimit, bool) {
    let l = memory_limit(guc_tables::backing::pgrust_memory_watchdog_limit());
    if l.bytes > 0 {
        // Gravity's auto caps and its memory budget read it (a store, no other effect)
        mcx::governor::publish_limit(l.bytes as usize);
    }
    LIMIT_BYTES.store(l.bytes, Ordering::Relaxed);
    open_sample(&l);
    let stored = LIMIT.try_lock().map(|mut cached| *cached = Some(l.clone())).is_ok();
    (l, stored)
}

/// D4029 (d): `LIMIT`'s bytes, for the 100 ms path (no lock, no clone).
static LIMIT_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const SAMPLE_BUF: usize = 8 << 10;

/// The sample's files: with `read_text`, this, its open and its pread are the watchdog's fs sites (lint-determinism.allow).
type SampleFile = std::fs::File;

/// D4029 (d): the 100 ms sample's files (/proc/self/status; under a cgroup limit, its memory.current and memory.stat),
/// opened when the limit is stored and read by pread into one fixed buffer: the sample allocates nothing and walks no path,
/// so a process in direct reclaim cannot stall the thread that would free it.
/// D4872: plus the memory PSI (the limiting cgroup's memory.pressure, else the host's /proc/pressure/memory) and, with no
/// cgroup limit, the host's /proc/vmstat for its file refaults (under a limit they come from the cgroup's memory.stat,
/// read for `resident`), and the host-pressure tracker they feed (`mempolicy::hostpress`).
struct SampleFiles {
    status: Option<SampleFile>,
    cgroup: Option<std::path::PathBuf>,
    current: Option<SampleFile>,
    stat: Option<SampleFile>,
    events: Option<SampleFile>,
    psi: Option<SampleFile>,
    vmstat: Option<SampleFile>,
    host_open: bool,
    refaults: Option<u64>,
    host: mempolicy::hostpress::HostPress,
}

/// The sample's read buffer, apart from `SampleFiles` ({D}): its Options' None niches (-1 fds, the PathBuf's capacity) are
/// not zero, so the 8 KiB buffer inside them sat in .data; alone it is all zero and lands in .bss (no file bytes).
type SampleBuf = [u8; SAMPLE_BUF];

impl SampleFiles {
    const fn new() -> Self {
        SampleFiles {
            status: None,
            cgroup: None,
            current: None,
            stat: None,
            events: None,
            psi: None,
            vmstat: None,
            host_open: false,
            refaults: None,
            host: mempolicy::hostpress::HostPress::new(),
        }
    }

    /// (Re)open the files for the limit's cgroup `dir` (`None`: no cgroup limit).
    fn open(&mut self, dir: Option<&std::path::Path>) {
        let open = |p: &std::path::Path| SampleFile::open(p).ok();
        if self.status.is_none() {
            self.status = open(std::path::Path::new("/proc/self/status"));
        }
        let moved = self.cgroup.as_deref() != dir;
        // D4872: the PSI and refault files follow the limit (a moved limit restarts the tracker's window: the counters
        // are another file's)
        if moved || !self.host_open {
            self.psi = match dir {
                Some(d) => open(&d.join("memory.pressure")),
                None => open(std::path::Path::new("/proc/pressure/memory")),
            };
            self.vmstat = if dir.is_none() { open(std::path::Path::new("/proc/vmstat")) } else { None };
            self.refaults = None;
            self.host = mempolicy::hostpress::HostPress::new();
            self.host_open = true;
        }
        if moved {
            self.current = dir.and_then(|d| open(&d.join("memory.current")));
            self.stat = dir.and_then(|d| open(&d.join("memory.stat")));
            self.events = dir.and_then(|d| open(&d.join("memory.events")));
            self.cgroup = dir.map(std::path::Path::to_path_buf);
        }
    }

    /// `governed_resident` from the open files: anon + shmem, and at least the cgroup's unreclaimable bytes.
    fn resident(&mut self, buf: &mut SampleBuf) -> Option<u64> {
        let SampleFiles { status, current, stat, events, .. } = self;
        let text = pread(status.as_ref()?, buf)?;
        let own = (field(text, b"RssAnon:") + field(text, b"RssShmem:")) * 1024;
        let Some(current) = current.as_ref().and_then(|f| pread(f, buf)).map(|t| field(t, b"")) else {
            // D4388: no cgroup limit (or its files unreadable): the cgroup's last figures go, so the ladder's read and B0's
            // headroom are the governed resident's, never a stale cgroup-wide number from before the limit moved
            CGROUP_SEEN.clear();
            mcx::ladder::no_file_refaults();
            LAST_OWN.store(own, Ordering::Relaxed);
            return Some(own);
        };
        let (clean, view, file, inactive, mapped, refaults) = stat.as_ref().and_then(|f| pread(f, buf)).map_or((0, own.max(current), 0, 0, 0, None), |t| {
            let refaults = field(t, b"workingset_refault_file ").saturating_mul(page_bytes());
            // ladder_reconcile: the split its rule 3 checks against (two more lines read, with the knob on only)
            let (anon, shmem) = if mcx::reconcile::on() { (field(t, b"anon "), field(t, b"shmem ")) } else { (0, 0) };
            CGROUP_SEEN.anon.store(anon, Ordering::Relaxed);
            CGROUP_SEEN.shmem.store(shmem, Ordering::Relaxed);
            (reclaimable_file(t), own_view_in(own, current, t), field(t, b"file "), field(t, b"inactive_file ").saturating_sub(dirty_file(t)), field(t, b"file_mapped "), Some(refaults))
        });
        CGROUP_SEEN.clean.store(clean, Ordering::Relaxed);
        // D4872: the cgroup's refaults for the host-pressure tracker too (`host_pressed`)
        self.refaults = refaults;
        // ladder C1c, the refault gate: the cgroup's file refaults, the page cache paying for the anon a raise adds
        match refaults {
            Some(b) => mcx::ladder::note_file_refaults(b, mcx::governor::decide_ms()),
            None => mcx::ladder::no_file_refaults(),
        }
        let max_events = events.as_ref().and_then(|f| pread(f, buf)).map_or(0, |t| field(t, b"max "));
        CGROUP_SEEN.note(current, file, inactive, mapped, max_events, mcx::governor::decide_ms());
        LAST_OWN.store(view, Ordering::Relaxed);
        Some(own.max(current.saturating_sub(clean)))
    }
}

impl SampleFiles {
    /// D4872: the host's memory pressure at this sample (`mempolicy::hostpress`): the PSI `some` total and the file
    /// refaults (the cgroup's from the last `resident`, else the host's /proc/vmstat), through the tracker; whether
    /// pressed. No allocation, no path walk (the files were opened with the limit).
    fn host_pressed(&mut self, buf: &mut SampleBuf, now_ms: u64) -> bool {
        let psi_some_us = self.psi.as_ref().and_then(|f| pread(f, buf)).and_then(psi_some_total);
        let refault_bytes = match self.vmstat.as_ref() {
            Some(f) => pread(f, buf).map(|t| field(t, b"workingset_refault_file ").saturating_mul(page_bytes())),
            None => self.refaults,
        };
        let reading = mempolicy::hostpress::Reading { psi_some_us, refault_bytes };
        self.host.step(now_ms, reading, &mempolicy::hostpress::Bars::now())
    }
}

/// D4872: a PSI file's `some` line's total (µs): "some avg10=0.00 avg60=0.00 avg300=0.00 total=24".
fn psi_some_total(text: &[u8]) -> Option<u64> {
    let line = text.split(|&c| c == b'\n').find(|l| l.starts_with(b"some "))?;
    let at = line.windows(6).position(|w| w == b"total=")? + 6;
    let digits = &line[at..];
    let n = digits.iter().take_while(|c| c.is_ascii_digit()).count();
    (n > 0).then(|| digits[..n].iter().fold(0u64, |a, &c| a.saturating_mul(10).saturating_add(u64::from(c - b'0'))))
}

/// D4872: the host's memory pressure to the page pool, every sample on the watchdog's thread (`refresh_governor`):
/// pressed (the memory PSI's stall share or the file refault rate past its knob over a window, `pool_host_*`), the pool
/// gives back its free memory and prefaults nothing (`pagepool::set_host_pressed`), whatever the ladder's line. A busy
/// sample lock skips the sample.
fn host_press_pass() {
    if !pagepool::on() {
        return;
    }
    let Ok(mut s) = SAMPLE.try_lock() else { return };
    let Ok(mut b) = SAMPLE_BUF_MEM.try_lock() else { return };
    let pressed = s.host_pressed(&mut b, mcx::governor::decide_ms());
    drop((b, s));
    pagepool::set_host_pressed(pressed);
}

/// D4029 (d): the 100 ms path's memory touched before its first sample (the sample's buffer, the victim pick's pins
/// table, the log ring (D4812), a quarter of this thread's stack), so a sample under direct reclaim faults in none of its
/// own pages.
fn prefault() {
    if let Ok(mut b) = SAMPLE_BUF_MEM.try_lock() {
        b.fill(0);
    }
    let _ = sample_resident();
    let _ = mcx::governor::try_no_victim_view();
    memlog::prefault();
    let mut stack = [0u8; 64 << 10];
    std::hint::black_box(&mut stack);
}

/// One read of `f` from offset 0 into `buf` (procfs and cgroup files regenerate at offset 0): the bytes read.
fn pread<'a>(f: &SampleFile, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    #[cfg(unix)]
    {
        let n = std::os::unix::fs::FileExt::read_at(f, buf, 0).ok()?;
        buf.get(..n)
    }
    #[cfg(not(unix))]
    {
        let _ = (f, buf);
        None
    }
}

/// memory.stat's page counts in bytes: the page size, read once (`page_bytes`; 0 = not yet).
static PAGE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn page_bytes() -> u64 {
    match PAGE_BYTES.load(Ordering::Relaxed) {
        0 => {
            #[cfg(unix)]
            // SAFETY: sysconf reads a constant of the running system.
            let p = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096).max(1);
            #[cfg(not(unix))]
            let p = 4096;
            PAGE_BYTES.store(p, Ordering::Relaxed);
            p
        }
        p => p,
    }
}

/// Only the watchdog thread samples (`try_lock`); `store_limit` reopens on the same thread.
static SAMPLE: pgsync::Mutex<SampleFiles> = pgsync::Mutex::new(SampleFiles::new());
/// `SAMPLE`'s buffer, locked after `SAMPLE` (never before).
static SAMPLE_BUF_MEM: pgsync::Mutex<SampleBuf> = pgsync::Mutex::new([0; SAMPLE_BUF]);

fn open_sample(l: &MemLimit) {
    let dir = match (&l.source, &l.cgroup) {
        (LimitSource::Cgroup, Some(d)) => Some(d.as_path()),
        _ => None,
    };
    if let Ok(mut s) = SAMPLE.try_lock() {
        s.open(dir);
    }
}

/// D4029 (d): the governed resident bytes now, from the sample's files; `None` when unreadable or the lock is busy.
fn sample_resident() -> Option<u64> {
    let mut s = SAMPLE.try_lock().ok()?;
    let mut b = SAMPLE_BUF_MEM.try_lock().ok()?;
    s.resident(&mut b)
}

/// D4029 (c), observe-only: the limiting cgroup at the last sample (memory.current, memory.stat's file and file_mapped) and
/// memory.events' 'max' count per second over the last whole second, for the backstop's lines (`pressure_note`); B0's
/// tick reads its clean inactive page cache (inactive_file less file_dirty and file_writeback).
#[derive(Default)]
struct CgroupSeen {
    current: std::sync::atomic::AtomicU64,
    file: std::sync::atomic::AtomicU64,
    inactive: std::sync::atomic::AtomicU64,
    mapped: std::sync::atomic::AtomicU64,
    max_rate: std::sync::atomic::AtomicU64,
    mark_ms: std::sync::atomic::AtomicU64,
    mark_max: std::sync::atomic::AtomicU64,
    /// `ladder_reconcile` only (0 with it off): memory.stat's anon and shmem and the clean page cache.
    anon: std::sync::atomic::AtomicU64,
    shmem: std::sync::atomic::AtomicU64,
    clean: std::sync::atomic::AtomicU64,
}

impl CgroupSeen {
    /// D4388: no cgroup is read: its byte figures are 0 (`ladder_read_of` and B0's headroom read the governed resident).
    fn clear(&self) {
        for a in [&self.current, &self.file, &self.inactive, &self.mapped, &self.anon, &self.shmem, &self.clean] {
            a.store(0, Ordering::Relaxed);
        }
    }

    fn note(&self, current: u64, file: u64, inactive: u64, mapped: u64, max_events: u64, now_ms: u64) {
        self.current.store(current, Ordering::Relaxed);
        self.file.store(file, Ordering::Relaxed);
        self.inactive.store(inactive, Ordering::Relaxed);
        self.mapped.store(mapped, Ordering::Relaxed);
        let (at, n) = (self.mark_ms.load(Ordering::Relaxed), self.mark_max.load(Ordering::Relaxed));
        let span = now_ms.saturating_sub(at);
        if at == 0 || span >= 1000 {
            if at != 0 {
                self.max_rate.store(max_events.saturating_sub(n).saturating_mul(1000) / span, Ordering::Relaxed);
            }
            self.mark_ms.store(now_ms.max(1), Ordering::Relaxed);
            self.mark_max.store(max_events, Ordering::Relaxed);
        }
    }
}

static CGROUP_SEEN: CgroupSeen = CgroupSeen {
    current: std::sync::atomic::AtomicU64::new(0),
    file: std::sync::atomic::AtomicU64::new(0),
    inactive: std::sync::atomic::AtomicU64::new(0),
    mapped: std::sync::atomic::AtomicU64::new(0),
    max_rate: std::sync::atomic::AtomicU64::new(0),
    mark_ms: std::sync::atomic::AtomicU64::new(0),
    mark_max: std::sync::atomic::AtomicU64::new(0),
    anon: std::sync::atomic::AtomicU64::new(0),
    shmem: std::sync::atomic::AtomicU64::new(0),
    clean: std::sync::atomic::AtomicU64::new(0),
};

/// D4029 (c): the backstop line's cgroup suffix (empty without a cgroup limit): its headroom under the limit, the 'max'
/// events per second, and its file and mapped file pages.
fn pressure_note(limit: u64) -> String {
    let current = CGROUP_SEEN.current.load(Ordering::Relaxed);
    if current == 0 {
        return String::new();
    }
    format!(
        " (cgroup: headroom {} MB, memory.events max {}/s, file {} MB, file_mapped {} MB)",
        limit.saturating_sub(current) >> 20,
        CGROUP_SEEN.max_rate.load(Ordering::Relaxed),
        CGROUP_SEEN.file.load(Ordering::Relaxed) >> 20,
        CGROUP_SEEN.mapped.load(Ordering::Relaxed) >> 20
    )
}

/// The last sample's governed resident bytes (0: none yet), for the samples on this thread after it.
static LAST_RESIDENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// D4262: the last sample's own view (`own_view_in`; with no cgroup limit, its anon + shmem; 0: none yet), for the
/// backstop's who-acts rule on this thread after it.
static LAST_OWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// D4262: a bystander's line was written this episode (past the line; cleared under it).
static BYSTANDER_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// D4886: the line of a cancel held for the pool's R1 ask was written this episode (past the line; cleared under it).
static POOL_HOLD_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn refresh_limit() -> MemLimit {
    store_limit().0
}

/// D3793: the memory limit read and published now, and where it comes from (the postmaster, after the configuration
/// files and before shared memory: Gravity checks the part cache's setting against it before the server starts). 0 when
/// nothing bounds the process.
pub fn limit_at_start() -> (u64, &'static str) {
    let l = refresh_limit();
    let source = match l.source {
        LimitSource::Guc => "pgrust.memory_watchdog_limit",
        LimitSource::Cgroup if l.knob == "memory.high" => "the cgroup memory.high",
        LimitSource::Cgroup => "the cgroup memory.max",
        LimitSource::Physical => "physical memory",
        LimitSource::None => "none",
    };
    if let Some(soft) = &l.soft {
        let (msg, hint) = soft_limit_warning(soft, l.bytes);
        warn_line(&msg, &hint, "limit_at_start");
    }
    (l.bytes, source)
}

/// D4290: a WARNING with a HINT in the server log (the postmaster at start, the watchdog's thread at run time); raw stderr
/// only if reporting itself unwinds, as `log_line`.
fn warn_line(msg: &str, hint: &str, func: &'static str) {
    let line = format!("memory watchdog: {msg}");
    // unwind-ok: log-then-die
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = elog::ereport(types_error::WARNING)
            .errmsg(line.clone())
            .errhint(hint.to_string())
            .finish(types_error::ErrorLocation::new("memruntime", 0, func));
    }))
    .is_err()
    {
        elog::write_stderr(&format!("WARNING:  {line}\nHINT:  {hint}\n"));
    }
}

/// D4290: the kernel's throttling of the server at a soft limit (memory.events' 'high' count of the `SoftLimit`'s cgroup),
/// read on the watchdog's tick. The first reading is the baseline; a rise is warned at most once per
/// `mempolicy::watchdog::HIGH_WARN_EVERY_MS`, the rises in between summed into the next warning.
#[derive(Default)]
struct HighWatch {
    /// The cgroup watched and its last 'high' count.
    seen: Option<(std::path::PathBuf, u64)>,
    /// Rises not warned yet.
    pending: u64,
    /// When the last warning went out (ms; `None`: never).
    warned_ms: Option<u64>,
}

impl HighWatch {
    /// `count` read from `dir`'s memory.events at `now_ms`: the throttling events to warn about now, if any.
    fn note(&mut self, dir: &std::path::Path, count: u64, now_ms: u64) -> Option<u64> {
        match &self.seen {
            Some((d, last)) if d == dir => self.pending += count.saturating_sub(*last),
            // a new cgroup (or the first reading): the baseline
            _ => self.pending = 0,
        }
        self.seen = Some((dir.to_path_buf(), count));
        let due = self.warned_ms.map_or(true, |w| now_ms.saturating_sub(w) >= mempolicy::watchdog::HIGH_WARN_EVERY_MS);
        if self.pending == 0 || !due {
            return None;
        }
        self.warned_ms = Some(now_ms);
        Some(std::mem::take(&mut self.pending))
    }
}

/// The 'high' line of a cgroup's memory.events.
fn high_events_in(dir: &std::path::Path) -> Option<u64> {
    let text = read_text(dir.join("memory.events"))?;
    text.lines().find_map(|l| l.strip_prefix("high ")).and_then(|v| v.trim().parse().ok())
}

/// D4290: the run-time WARNING and its HINT: the throttling events since the last warning.
fn high_warning(rise: u64, s: &SoftLimit) -> (String, String) {
    (
        format!(
            "the kernel throttled the server at the cgroup's memory.high ({} MiB) {rise} times since the last warning \
             (memory.events high)",
            s.high >> 20
        ),
        "Set MemoryHigh equal to MemoryMax (memory.high equal to memory.max), or leave it unset.".to_string(),
    )
}

/// D4290 (the watchdog's tick): under a soft limit, warn when its cgroup's memory.events 'high' count rose.
fn high_tick(watch: &mut HighWatch, l: &MemLimit, now_ms: u64) -> Option<(String, String)> {
    let soft = l.soft.as_ref()?;
    let count = high_events_in(&soft.dir)?;
    let rise = watch.note(&soft.dir, count, now_ms)?;
    Some(high_warning(rise, soft))
}

fn current_limit() -> MemLimit {
    let cached = LIMIT.try_lock().ok().and_then(|c| c.clone());
    cached.unwrap_or_else(refresh_limit)
}

/// D3314: the resident bytes the governor's bar, its resample and the cancel compare: anon + shmem, and under a cgroup
/// limit at least the cgroup's unreclaimable bytes.
fn governed_resident(m: &ProcMem, l: &MemLimit) -> u64 {
    let own = (m.anon_kb + m.shmem_kb) * 1024;
    match (&l.source, &l.cgroup) {
        (LimitSource::Cgroup, Some(dir)) => own.max(cgroup_unreclaimable(dir).unwrap_or(0)),
        _ => own,
    }
}

/// Under a cgroup limit, the limiting cgroup's resident memory as its memory.stat counts it: anon + shmem (bytes). The
/// memory ledger's view sets its 'unattributed' row against it; `None` without a cgroup limit or an unreadable file.
fn cgroup_resident() -> Option<usize> {
    cgroup_resident_in(&current_limit())
}

fn cgroup_resident_in(l: &MemLimit) -> Option<usize> {
    let (LimitSource::Cgroup, Some(dir)) = (&l.source, &l.cgroup) else { return None };
    let stat = read_text(dir.join("memory.stat"))?;
    let field = |k: &str| {
        stat.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix(' ')))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some((field("anon") + field("shmem")) as usize)
}

/// LEDGER2 (`mcx::governor::reconcile`): the limiting cgroup's memory.current and memory.stat anon, file, shmem, kernel
/// (the "kernel" key; older kernels: kernel_stack + pagetables + percpu + slab + vmalloc) and sock; glibc's in-use bytes
/// (mallinfo2: arenas' allocated + mmapped chunks); the process's threads. Read when a view asks, never by the sampler.
fn reconcile_terms() -> [Option<u64>; mcx::governor::reconcile::TERMS] {
    use mcx::governor::reconcile as r;
    let mut t = [None; r::TERMS];
    let l = current_limit();
    if let (LimitSource::Cgroup, Some(dir)) = (&l.source, &l.cgroup) {
        t[r::CURRENT] = read_u64(&dir.join("memory.current"));
        if let Some(stat) = read_text(dir.join("memory.stat")) {
            let s = stat.as_bytes();
            let has = |k: &[u8]| s.split(|&c| c == b'\n').any(|l| l.starts_with(k));
            t[r::ANON] = Some(field(s, b"anon "));
            t[r::FILE] = Some(field(s, b"file "));
            t[r::SHMEM] = Some(field(s, b"shmem "));
            t[r::KERNEL] = Some(if has(b"kernel ") {
                field(s, b"kernel ")
            } else {
                [&b"kernel_stack "[..], &b"pagetables "[..], &b"percpu "[..], &b"slab "[..], &b"vmalloc "[..]].iter().map(|k| field(s, k)).sum()
            });
            t[r::SOCK] = Some(field(s, b"sock "));
        }
    }
    t[r::C_LIBRARIES] = c_library_in_use();
    t[r::THREADS] = read_text("/proc/self/status").map(|s| field(s.as_bytes(), b"Threads:"));
    t
}

#[cfg(all(not(target_family = "wasm"), target_os = "linux", target_env = "gnu"))]
fn c_library_in_use() -> Option<u64> {
    // SAFETY: mallinfo2 takes no arguments and only reads glibc's arena statistics under its own locks.
    let m = unsafe { libc::mallinfo2() };
    Some((m.uordblks + m.hblkhd) as u64)
}

#[cfg(not(all(not(target_family = "wasm"), target_os = "linux", target_env = "gnu")))]
fn c_library_in_use() -> Option<u64> {
    None
}

/// The allocator's resident bytes: the pages present in every mapping mimalloc made, which it names `mimalloc` (and
/// pagegive keeps the name on a range it gives back). The ranges come from /proc/self/maps (no page-table walk), their
/// present pages from mincore(2) over those ranges alone. Read only when a view asks, never by the sampler. `None` when
/// mapping names are unavailable, the maps are unreadable or mincore refuses. pagepool P2: plus the page pool's present bytes (`pagepool::present`, its
/// own mincore over the superpages it holds and its lazy spans' committed bytes). Not its claimed bytes: a Cold take and a
/// lazy span's commit count there before any page is faulted (P3 stress: U0 -1.9 GiB). ACCOUNTING ONLY (the views, the
/// ladder's retained reading and the budget's retained floor): no kill-safety decision reads this; the watchdog's cancel
/// line and the governor's resample read the resident probe. pagepool P3 (c): the pool's part is swept now
/// (`pagepool::present_fresh`, milliseconds), at the same point as mimalloc's mincore here and the process's resident
/// bytes the view sets it against: the purger's published reading lags a fault by up to a sweep period, which the
/// unattributed row showed as a false spike. The sweep takes no pool lock (superpage and lazy-slot states are atomic
/// loads; mincore takes mmap_lock for read, one call per 4096 pages) and is single-flight (one sweeper at a time, a CAS
/// on present.rs's SWEEPING; a reader meanwhile takes the sweep's result), and present_fresh shares a sweep under 250 ms
/// old: at most ~4 sweeps a second whatever the number of readers (~40 ms CPU a second at a 10.9 GB-present pool).
fn allocator_resident() -> Option<usize> {
    // A failed read must not leave the previous reading's split visible. In particular, a kernel without
    // anonymous VMA names has no identifiable mimalloc ranges: their absence does not mean zero resident bytes.
    RESIDENT_SPLIT.with(|c| c.set(None));
    if !vma_names_supported()? {
        return None;
    }
    allocator_resident_lower_bound()
}

/// Preserve the reclaim ladder's existing conservative estimate when mapping names are unavailable: known named
/// mappings plus the pool's independent mincore reading. This can undercount mimalloc and is not a reportable total.
/// Kill safety continues to use the separate process/cgroup resident probes.
fn allocator_resident_lower_bound() -> Option<usize> {
    let maps = present_in(&read_text("/proc/self/maps")?, |n| n == "[anon:mimalloc]")?;
    let mut pool = 0;
    let r = with_pool(maps, pagepool::on(), || pagepool::present_fresh().inspect(|&p| pool = p));
    RESIDENT_SPLIT.with(|c| c.set(r.map(|_| (maps, pool))));
    r
}

/// O(1) after the first conclusive probe; no allocation/free or query path calls this. Cache success and EINVAL
/// (the kernel lacks anonymous VMA naming), but retry transient failures on the next diagnostic read.
#[cfg(target_os = "linux")]
fn vma_names_supported() -> Option<bool> {
    use std::sync::atomic::AtomicU8;
    static SUPPORT: AtomicU8 = AtomicU8::new(0);
    match SUPPORT.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => {
            let supported = probe_vma_names()?;
            SUPPORT.store(if supported { 2 } else { 1 }, Ordering::Relaxed);
            Some(supported)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn vma_names_supported() -> Option<bool> {
    Some(false)
}

#[cfg(target_os = "linux")]
fn probe_vma_names() -> Option<bool> {
    // SAFETY: sysconf reads a constant; the private mapping has no users and is never touched.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    let page = page as usize;
    let p = unsafe { libc::mmap(core::ptr::null_mut(), page, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
    if p == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: a page-aligned anonymous range we own and a valid, NUL-terminated name. Preserve errno before munmap.
    let rc = unsafe { libc::prctl(0x53564d41, 0 as libc::c_ulong, p as libc::c_ulong, page as libc::c_ulong, c"pgrust:residency-probe".as_ptr()) };
    let error = (rc != 0).then(std::io::Error::last_os_error);
    // SAFETY: release exactly the private mapping above, regardless of the prctl outcome.
    unsafe { libc::munmap(p, page) };
    match error {
        None => Some(true),
        Some(e) if e.raw_os_error() == Some(libc::EINVAL) => Some(false),
        Some(_) => None,
    }
}

std::thread_local! {
    /// pppress: this thread's last `allocator_resident` reading as its two terms (mimalloc's named mappings' present
    /// bytes, the pool's present bytes), `take_allocator_resident_split`.
    static RESIDENT_SPLIT: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
}

/// pppress (the memory view's 'mimalloc_resident'): this thread's last `allocator_resident` reading as (maps, pool),
/// taken (None after): a reader that called the governor's resident probe shows the maps term from the SAME read (no
/// second /proc/self/maps read). maps + pool = the reading.
pub fn take_allocator_resident_split() -> Option<(usize, usize)> {
    RESIDENT_SPLIT.with(|c| c.take())
}

/// The allocator's resident bytes `maps` plus the pool's present ones while it is `on` (read only then); `None` when the
/// pool is on and its reading is not.
fn with_pool(maps: usize, on: bool, present: impl FnOnce() -> Option<usize>) -> Option<usize> {
    if !on {
        return Some(maps);
    }
    present().map(|p| maps.saturating_add(p))
}

/// The Gravity region mappings' resident bytes: the pages present in the mappings named `pgrust:region` or
/// `pgrust:region:<breaker kind>` (`mempolicy::maps::region`; not the arena's nor `:mapped`), the way
/// `allocator_resident` reads the allocator's. The names exist only while the memory policy's trace is on.
fn region_resident() -> Option<usize> {
    if !vma_names_supported()? || !mempolicy::trace::on() {
        return None;
    }
    present_in(&read_text("/proc/self/maps")?, is_region_name)
}

fn is_region_name(n: &str) -> bool {
    (n == "[anon:pgrust:region]" || n.starts_with("[anon:pgrust:region:")) && n != "[anon:pgrust:region:arena]" && n != "[anon:pgrust:region:mapped]"
}

fn present_in(maps: &str, named: impl Fn(&str) -> bool) -> Option<usize> {
    const STEP_PAGES: usize = 1 << 16;
    let mut vec = Vec::new();
    vec.try_reserve_exact(STEP_PAGES).ok()?;
    vec.resize(STEP_PAGES, 0u8);
    let mut bytes = 0usize;
    for (lo, hi) in named_ranges(maps, &named) {
        // a mapping gone since /proc/self/maps listed it fails the read whole
        bytes += pagegive::resident_bytes(lo, hi, &mut vec)?;
    }
    Some(bytes)
}

/// The address ranges of the /proc/<pid>/maps lines whose name (the sixth field) `named` accepts.
fn named_ranges<'a>(maps: &'a str, named: &'a impl Fn(&str) -> bool) -> impl Iterator<Item = (usize, usize)> + 'a {
    maps.lines().filter(move |l| l.split_whitespace().nth(5).is_some_and(|n| named(n))).filter_map(|l| {
        let (lo, hi) = l.split_once(' ')?.0.split_once('-')?;
        Some((usize::from_str_radix(lo, 16).ok()?, usize::from_str_radix(hi, 16).ok()?))
    })
}

// ---------------------------------------------------------------------------
// The sampler.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum LimitSource {
    Guc,
    Cgroup,
    Physical,
    #[default]
    None,
}

#[derive(Default)]
struct WatchState {
    // Highest tier index (1..=3) fired in the current excursion; 0 = armed.
    latched: u32,
    // Last announced armed-state, to log arm/idle transitions exactly once.
    announced: Option<(LimitSource, u64)>,
    // The tick's limit store was skipped: the tick runs again at the next sample.
    store_due: bool,
}

struct Ledger {
    usage: u64,
    limit: u64,
    source: LimitSource,
    /// The cgroup file that set the limit (`MemLimit::knob`).
    knob: &'static str,
    pm: Option<ProcMem>,
    cg: Option<(u64, Option<u64>)>,
    accounted: usize,
    alloc: Option<AllocatorStats>,
}

fn collect(l: &MemLimit) -> Ledger {
    let pm = proc_status();
    let alloc = allocator_stats();
    let rss_bytes = match (pm, alloc) {
        (Some(m), _) => m.rss_kb * 1024,
        (None, Some(a)) => a.current_rss as u64,
        (None, None) => 0,
    };
    // D3314 / D3316: the one limit; under a cgroup's (the leaf's or an ancestor's), the usage and the ledger's cgroup
    // figures are that cgroup's
    let limiting = match (&l.source, &l.cgroup) {
        (LimitSource::Cgroup, Some(dir)) => read_u64(&dir.join("memory.current")).map(|c| (c, Some(l.bytes))),
        _ => None,
    };
    let cg = limiting.or_else(cgroup_mem);
    let usage = limiting.map_or(rss_bytes, |(c, _)| c);
    let (limit, source) = (l.bytes, l.source);
    Ledger {
        usage,
        limit,
        source,
        knob: l.knob,
        pm,
        cg,
        accounted: mcx::global_footprint::bytes(),
        alloc,
    }
}

// Total physical RAM: the limit of last resort. With neither a cgroup limit nor
// the GUC, the only thing that ends a runaway process is the kernel (Linux OOM
// killer, macOS jetsam) and a SIGKILL leaves no log line — the thread model
// has no surviving postmaster to report "terminated by signal 9".
fn physical_memory() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes an integer name and touches no memory.
        let (pages, page) = unsafe {
            (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE))
        };
        (pages > 0 && page > 0).then(|| pages as u64 * page as u64)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// pphatch: in a build with the page pool's `PoolAlloc`, one start line naming the global allocator and why.
fn allocator_line() {
    if !pagepool::GLOBAL_BUILD {
        return;
    }
    // the user 2026-10-18: the default is the architecture's (Hearth on Linux aarch64 and x86_64, mimalloc elsewhere)
    let why = match (pagepool::allocator_env(), pagepool::allocator_env_unknown()) {
        (_, Some(v)) => format!("PGRUST_ALLOCATOR=\"{v}\" names no allocator ('hearth' or 'mimalloc'): this architecture's default"),
        (Some(v), None) => format!("PGRUST_ALLOCATOR={v}"),
        (None, None) => "this architecture's default: Hearth on Linux aarch64 and x86_64, mimalloc elsewhere".to_owned(),
    };
    // by_arch_default!'s constructor came after the first allocation: the pool served by the old default
    let why = if pagepool::by_arch_late() { format!("{why}; WARNING: the per-architecture default ran after the first allocation and did not apply") } else { why };
    let line = if pagepool::hatch_on() {
        format!("global allocator: mimalloc ({why}; Hearth is off and its PGRUST_POOL_* switches are ignored; PGRUST_ALLOCATOR=hearth at start turns it on)")
    } else {
        format!("global allocator: hearth ({why}; PGRUST_ALLOCATOR=mimalloc at start turns it off)")
    };
    log_text(line);
}

fn log_line(msg: &str) {
    log_text(format!("memory watchdog: {msg}"));
}

fn log_text(line: String) {
    // Through elog so the line carries log_line_prefix (timestamp in
    // log_timezone, pid, ...) like every other server-log line; raw stderr
    // only if reporting itself unwinds — the watchdog's lines must not
    // vanish, they are the only witness of a memory incident. D4812: on the
    // watchdog's thread elog hands the formatted line to `memlog` (written
    // by pg:memlog, never by this thread), so the pass never waits on the log.
    // unwind-ok: log-then-die
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = elog::elog(types_error::LOG, line.clone());
    }))
    .is_err()
    {
        elog::write_stderr(&format!("LOG:  {line}\n"));
    }
}

fn fire(l: &Ledger, pct: u64, tier_pct: u64) {
    let pm = l.pm.unwrap_or_default();
    // The delta of record: anonymous RSS (the incident-class growth; shared
    // memory excluded) minus the accounted context block bytes. Positive =
    // heap outside memory contexts (allocator retention/overhead, plain Rust
    // estates, thread stacks) or per-context ledgers drifting under reality;
    // negative = contexts holding committed-but-untouched blocks. Off-Linux
    // (no /proc) the basis falls back to the allocator's process RSS.
    let rss_basis_kb = if pm.anon_kb > 0 {
        pm.anon_kb
    } else if pm.rss_kb > 0 {
        pm.rss_kb
    } else {
        l.alloc.map_or(0, |a| a.current_rss as u64 / 1024)
    };
    let accounted_kb = (l.accounted / 1024) as u64;
    let delta_kb = rss_basis_kb as i64 - accounted_kb as i64;
    let (cg_cur_kb, cg_max) = match l.cg {
        Some((c, m)) => (c / 1024, m),
        None => (0, None),
    };
    let alloc_commit_kb = l.alloc.map_or(0, |a| a.current_commit / 1024);
    // D3243 diagnostic: the allocator's live blocks against the same basis (only while counted)
    let live = mcx::governor::allocator_in_use().map_or_else(String::new, |b| {
        format!(" allocator_in_use={} kB resident_not_live={} kB", b / 1024, rss_basis_kb as i64 - (b / 1024) as i64)
    });
    log_line(&format!(
        "memory use {pct}% of limit crossed the {tier_pct}% tier: \
         usage={} kB limit={} kB rss={} kB anon={} kB shmem={} kB hwm={} kB \
         cgroup_current={} kB cgroup_limit={} accounted_contexts={} kB \
         allocator_committed={} kB unaccounted_delta={} kB{live}",
        l.usage / 1024,
        l.limit / 1024,
        pm.rss_kb,
        pm.anon_kb,
        pm.shmem_kb,
        pm.hwm_kb,
        cg_cur_kb,
        cg_max.map_or_else(|| "none".to_string(), |m| format!("{} kB", m / 1024)),
        accounted_kb,
        alloc_commit_kb,
        delta_kb,
    ));
    if guc_tables::backing::pgrust_memory_watchdog_dump() {
        log_line(&request_backend_dumps());
    }
}

// D4092: PROCSIG_LOG_MEMORY_CONTEXT to the watchdog's top holders only (the
// running statements its own rank puts first, `mempolicy::watchdog::DUMP_TOP`
// of them: `mcx::governor::try_top_holders`), not to every live backend: ~70
// backends dumping their whole context trees at once stalled each ~600 ms
// inside its kernel calls (nestfast's m-stress). Each one signalled dumps its
// own tree to the log at its next interrupt-processing point (the
// pg_log_backend_memory_contexts path). The registry busy: no dumps at this
// crossing (the watchdog's thread never waits). Failures are ignored (a proc
// exiting mid-scan). The line names the backends and their rank bytes.
fn request_backend_dumps() -> String {
    use types_storage::ProcSignalReason::PROCSIG_LOG_MEMORY_CONTEXT;
    let live = lmgr_proc::ProcGlobal().allProcs.iter().filter(|p| p.pid.load(Ordering::Relaxed) != 0).count();
    let mut top = [mcx::governor::TopHolder::default(); mempolicy::watchdog::DUMP_TOP];
    let Ok(k) = mcx::governor::try_top_holders(top.len(), &mut top) else {
        return format!("requested memory context dumps from 0 backends of {live} live (the statement registry was busy: no dumps at this crossing)");
    };
    let (mut n, mut who) = (0, String::new());
    for h in &top[..k] {
        if procsignal::SendProcSignal(h.pid, PROCSIG_LOG_MEMORY_CONTEXT, h.slot as _) >= 0 {
            n += 1;
            who.push_str(&format!("{}pid {} at {} MB", if who.is_empty() { "" } else { ", " }, h.pid, h.bytes >> 20));
        }
    }
    format!(
        "requested memory context dumps from {n} backends of {live} live (the watchdog's top holders by rank: {who}; \
         see subsequent \"logging memory contexts of PID\" log entries)"
    )
}

fn announce(state: &mut WatchState, l: &Ledger, base: i32) {
    let key = (l.source, l.limit);
    if state.announced == Some(key) {
        return;
    }
    state.announced = Some(key);
    state.latched = 0;
    match l.source {
        LimitSource::None => log_line(
            "idle: no memory limit signal (no bounded cgroup v2 limit found, \
             pgrust.memory_watchdog_limit is 0, physical memory size unreadable)",
        ),
        _ => {
            let t = mempolicy::watchdog::tiers(base);
            log_line(&format!(
                "armed: limit={} MB (source={}) tiers={}%/{}%/{}% interval={} ms",
                l.limit >> 20,
                match l.source {
                    LimitSource::Guc => "pgrust.memory_watchdog_limit",
                    LimitSource::Cgroup if l.knob == "memory.high" => "cgroup v2 memory.high",
                    LimitSource::Cgroup => "cgroup v2 memory.max",
                    LimitSource::Physical => "physical memory; no cgroup v2 limit, pgrust.memory_watchdog_limit is 0",
                    LimitSource::None => unreachable!(),
                },
                t[0],
                t[1],
                t[2],
                guc_tables::backing::pgrust_memory_watchdog_interval(),
            ));
        }
    }
}

fn tick(state: &mut WatchState) {
    if !guc_tables::backing::pgrust_memory_watchdog() {
        state.latched = 0;
        state.announced = None; // re-announce on re-enable
        state.store_due = false;
        return;
    }
    let base = guc_tables::backing::pgrust_memory_watchdog_threshold();
    let Some(l) = arm(state, base) else { return };
    if l.limit == 0 || l.usage == 0 {
        return;
    }
    let pct = l.usage.saturating_mul(100) / l.limit;
    let t = mempolicy::watchdog::tiers(base);
    let highest = mempolicy::watchdog::crossed(pct, base);
    if highest > state.latched {
        // One log volley per tier per excursion, tiers strictly escalating.
        fire(&l, pct, t[(highest - 1) as usize]);
        state.latched = highest;
    } else if state.latched > 0 && mempolicy::watchdog::rearms(pct, base) {
        log_line(&format!(
            "memory use receded to {pct}% of limit; watchdog re-armed"
        ));
        state.latched = 0;
    }
}

/// "armed: limit=" names the limit the cancel path reads (`LIMIT`), so it is announced only after its store; a skipped
/// store (the lock busy) announces nothing and leaves the tick due at the next sample.
fn arm(state: &mut WatchState, base: i32) -> Option<Ledger> {
    let (lim, stored) = store_limit();
    state.store_due = !stored;
    if !stored {
        return None;
    }
    let l = collect(&lim);
    announce(state, &l, base);
    Some(l)
}

// The governor's sample: the one limit (D3314, `current_limit`), resident bytes as anon + shmem and, under a cgroup
// limit, at least the cgroup's unreclaimable bytes (`governed_resident`: clean page cache is not ours to count).
/// ALLOC-LADDER (c): the allocator's line, re-set at every sample: the bytes mimalloc may take from its arenas from now
/// before resident memory reaches the governor's bar (else 90 % of the limit, the backstop's cancel tier). A take past
/// it wakes this thread at once (`memledger::arena::note_taken`); 0 (off) without a limit or before the arenas exist.
fn set_alloc_line(limit: u64, resident: u64) {
    let Some((general, cache)) = memledger::arena::taken() else { return };
    let line_at = match mcx::governor::bar() as u64 {
        0 => limit / 10 * 9,
        bar => bar,
    };
    if limit == 0 || line_at == 0 {
        memledger::arena::set_line(0);
        return;
    }
    let taken = general.saturating_add(cache);
    memledger::arena::set_line(taken.saturating_add(line_at.saturating_sub(resident)).max(taken.saturating_add(1)));
}

fn refresh_governor() {
    let pct = guc_tables::backing::pgrust_memory_governor_threshold();
    let projection = mempolicy::decide::projection();
    let footprint = mcx::global_footprint::bytes();
    let limit = LIMIT_BYTES.load(Ordering::Relaxed);
    let resident = sample_resident();
    LAST_RESIDENT.store(resident.unwrap_or(0), Ordering::Relaxed);
    // D4872: the host's memory pressure (PSI / file refaults) to the page pool, after the sample that read the refaults
    host_press_pass();
    // ladder pressure's reading when the ladder is not on (its line is published every sample, whatever the mode)
    mcx::ladder::publish_read(resident.unwrap_or(0));
    // D3934: decide reads R whatever the governor's threshold; with pgrust.memory_governor_threshold = 0 the governor's
    // own R is 0, and decide saw F = B: no projection ever crossed
    if let (true, Some(r)) = (projection, resident) {
        mcx::governor::publish_decide_resident(r as usize, footprint);
    }
    publish_samples(pct, limit, resident, footprint);
    log_trips(resident.filter(|_| pct > 0).map(|r| r as usize));
    if let Some(r) = resident {
        set_alloc_line(limit, r);
    }
    // D3934: the 90 % rule is the backstop rung under projection too: projection is never weaker than threshold
    // D4353: under memory_ladder = on the ladder acts, on its line under the cancel's (step 3b); the watchdog only measures
    if let Some(r) = resident {
        ladder_shadow(limit, r);
        refresh_cancel(limit, r);
    }
}

/// The governor's sample at the governed resident `resident` (its bar, `pgrust.memory_governor_threshold` percent of
/// `limit`) and the block gate's (D4812 (2)), whatever the bar: under `ladder_kill_read` the governed resident (the kill
/// reads'), else the ladder's read of the same sample (`ladder_read_of`, from the files this sample read), both at the context
/// footprint `footprint` taken before the read. No reading: the gate keeps its last sample, projected from its own
/// footprint.
fn publish_samples(pct: i32, limit: u64, resident: Option<u64>, footprint: usize) {
    match resident {
        Some(r) if pct > 0 && limit > 0 => {
            mcx::governor::publish((limit / 100).saturating_mul(pct.clamp(1, 100) as u64) as usize, r as usize, footprint)
        }
        _ => mcx::governor::publish(0, 0, footprint),
    }
    // `ladder_kill_read` (USER 10-18): the block gate holds and cancels at the cancel line, a kill read: the governed
    // resident with the switch on, the ladder's read without it
    if let Some(r) = resident {
        let gate = if mempolicy::ladder::kill_read_on() { r } else { ladder_read_of(r) };
        mcx::governor::publish_gate(gate as usize, footprint);
    }
}

/// D4324, LADDER-DESIGN section 10 step 2 (`pgrust.memory_ladder = shadow`): the ladder's decision at this sample, logged
/// per episode (`mempolicy::ladder::Shadow`) and acted on by nothing; the watchdog above still acts. The read is the
/// user's (memory.current less the clean inactive page cache; the governed resident bytes with no cgroup), the line the
/// limit less `MARGIN_MIN` (section 7's measured margin replaces it at step 4). The rungs' inputs are read only past the
/// line: the parked memory and caches the backstop's rungs could free (R2a; the protected floors of 0b are not in force
/// yet, so R2b has nothing apart), the spillable bytes revoke may ask (R4b), a cancel not yet acknowledged (R6).
///
/// D4324 step 3 (`on`): the sample also publishes the mode and the line the gates' climbs read (`mcx::ladder`), installs
/// their actuators once, and clears R3's flag while the read is under the line; its lines say `ladder on` and a closing
/// one carries the rungs the gates ran since the server started (runs / measured relief).
fn ladder_shadow(limit: u64, resident: u64) {
    use mempolicy::ladder::{self, ShadowLine, State, Who};
    // D4398: the ladder's pass never waits on a cache's lock: one another thread holds leaves that cache out of this pass
    let _pass = memledger::ladder_pass::LadderPass::enter();
    let mode = match guc_tables::backing::pgrust_memory_ladder() {
        guc_tables::consts::MEMORY_LADDER_SHADOW => mcx::ladder::SHADOW,
        guc_tables::consts::MEMORY_LADDER_ON => mcx::ladder::ON,
        _ => mcx::ladder::OFF,
    };
    // step 3b: under an armed watchdog cancel, below its line by the margin (mempolicy::ladder::line); D4389: the margin
    // is measured (the p99 rise over a cancel's landing and a tick, `ladder_margin`) while the ladder runs
    let margin = if mode == mcx::ladder::OFF { ladder::MARGIN_MIN } else { ladder_margin(limit) };
    let line = ladder::line_with(limit, guc_tables::backing::pgrust_memory_cancel_threshold(), margin);
    mcx::ladder::publish(if limit == 0 { mcx::ladder::OFF } else { mode }, line);
    // the watchdog's cancel line, for the ladder's readers (inline purges past it under PGRUST_PURGE_PRESSURE_BG)
    let pct = guc_tables::backing::pgrust_memory_cancel_threshold().clamp(1, 100) as u64;
    let cancel_line = if limit == 0 { 0 } else { (limit / 100).saturating_mul(pct) };
    mcx::ladder::publish_cancel_line(cancel_line);
    if mode == mcx::ladder::OFF || limit == 0 {
        mcx::ladder::clear_narrow();
        pagepool::set_claim_room(usize::MAX);
        pagepool::set_reserve_budget(u64::MAX);
        return;
    }
    if mode == mcx::ladder::ON && !LADDER_ACTS_IN.swap(true, Ordering::Relaxed) {
        mcx::ladder::install(&LADDER_ACTS);
    }
    SAMPLE_READ.store(ladder_read_of(resident), Ordering::Relaxed);
    // `ladder_kill_read` (USER 10-18): R6 reads the governed resident (the shrink rungs keep the ladder's read)
    mcx::ladder::publish_kill_read(resident);
    SAMPLE_LINE.store(line, Ordering::Relaxed);
    SAMPLE_LIMIT.store(limit, Ordering::Relaxed);
    // D4812: an episode's samples hand the statements R6's pick would find stale to the residency thread, before it picks
    sample_hint(limit, line, ladder_read_of(resident));
    let now = mcx::governor::decide_ms();
    // D4353: the ladder thread's pass: the hook's rungs, R2b and R6 past its one-tick wait (the episode lines below log it)
    if mode == mcx::ladder::ON {
        // D4364: past the watchdog's cancel line the one-tick wait is waived: R2b / R6 run now (the watchdog cancels
        // nothing while the ladder is on: refresh_cancel)
        if mempolicy::watchdog::cancel_at(resident, limit, guc_tables::backing::pgrust_memory_cancel_threshold()).is_some() {
            mcx::ladder::waive_wait();
        }
        // the flag R6's hold reads in this pass (`r6_held`; the pass clears it)
        let past_cancel = mcx::ladder::past_cancel();
        // D4391: the last R6 cancel's landing is tested at every sample, the read under the line included, before the pass
        let waited = mcx::ladder::hook_waited(now);
        let landing = r6_landing_at(now, ladder_read_of(resident));
        // S1 criterion 11 (`ladder_log_ms`): the sized caches' takes and raises before this sample's passes
        let moves = ladderlog::Moves::now();
        let climbed = mcx::ladder::hook_pass(now);
        // the purger's pressure mode: a pass that left a need asks for it
        if climbed.need != 0 {
            purge::ask(false);
        }
        // the landing's hold, counted: a pass past its wait that still needed bytes, with R6 held for the landing
        note_r6_hold(now, landing && waited && climbed.need != 0, past_cancel, ladder_read_of(resident) > cancel_line);
        // ladder C1 (`cache_floor`): the background trigger after the hook's pass (under LOW, R2a to HIGH, a batch of at
        // most 2 GiB a sample), the room under the line the draws read, and LOW the burst over a batch's give-back and a
        // tick (`ladder_low_burst`)
        let burst = if mcx::ladder::floors_on() { ladder_low_burst(limit) } else { None };
        let _ = mcx::ladder::background_pass(limit, line, ladder_read_of(resident), climbed.ran != 0, burst);
        ladderlog::sample(&moves, now, ladder_read_of(resident), line);
        if mcx::reconcile::on() {
            reconcile_sample(limit, line, resident);
        }
        // pagepool P6: the claimer's prefault is a raise: only into the room past TOP, nothing while pressed or landing
        // pppress card 15: the pool's landing once (the driver's hook, from the server's start, holds it already)
        let landing = mcx::ladder::claimer_landing(mcx::governor::decide_inputs().pending, pagepool::landing(u32::MAX) as u64);
        let top = mcx::ladder::marks3_now(limit).2;
        pagepool::set_claim_room(pagepool::claim_room(line, ladder_read_of(resident), mcx::ladder::room_low(), top, landing));
        // the warm reserve (PGRUST_POOL_RESERVE=1): its budget, the ladder's sized cache 3 (start: E1d's bound)
        if pagepool::reserve_on() {
            pagepool::set_reserve_budget(mcx::ladder::sized_budget(mcx::ladder::SIZED_RESERVE, pagepool::reserve_bound()));
        }
    } else {
        pagepool::set_claim_room(usize::MAX);
        pagepool::set_reserve_budget(u64::MAX);
    }
    let read = ladder_read_of(resident);
    // shadow: the whole path it would climb, as before; on: R2b / R6 only once its wait ended
    let waited = mode != mcx::ladder::ON || mcx::ladder::hook_waited(now);
    let kill_read = mcx::ladder::kill_read_now();
    let mut s = State { read, line, waited, kill_read, ..State::default() };
    if ladder::need(&s) == 0 {
        mcx::ladder::clear_narrow();
    }
    if ladder::need(&s) != 0 {
        s.elastic = ladder_elastic();
        s.protected = ladder_protected();
        s.revocable = mcx::revoke::spillable_total();
        s.cancel_in_flight = mcx::governor::try_no_victim_view().is_ok_and(|v| v.pending_ack);
    }
    let plan = ladder::plan(Who::Hook, &s);
    let Ok(mut sh) = LADDER_SHADOW.try_lock() else { return };
    let Some(l) = sh.observe(ladder::need(&s), plan, now) else { return };
    let episode = sh.episodes;
    drop(sh);
    // lane ladderview: the hook's episode in pgrust_memory_ladder_episodes() (observe only)
    if let ShadowLine::Open { need, .. } = l { mcx::ladder_stats::hook_open(need, read, line) } else if matches!(l, ShadowLine::Close { .. }) { mcx::ladder_stats::hook_close() }
    let mut rungs = String::new(); // alloc: Cold
    let mb = |b: u64| b >> 20;
    let closed = matches!(l, ShadowLine::Close { .. });
    let closes = closed && mode == mcx::ladder::ON;
    let text = match l {
        ShadowLine::Open { need, plan } | ShadowLine::Plan { need, plan } => {
            ladder::plan_names(Who::Hook, plan, &mut rungs);
            let what = if matches!(l, ShadowLine::Open { .. }) { "opens" } else { "plan changes" };
            format!(
                "ladder {} episode {episode} {what}: read {} MB (memory.current less inactive_file) past its line {} MB (the {} MB limit, its cancel line and {} MB of margin) by {} MB: would climb {rungs} (elastic {} MB, protected {} MB, revocable {} MB, cancel in flight {}){}",
                if mode == mcx::ladder::ON { "on" } else { "shadow" },
                mb(read),
                mb(line),
                mb(limit),
                mb(margin),
                mb(need),
                mb(s.elastic),
                mb(s.protected),
                mb(s.revocable),
                s.cancel_in_flight,
                pressure_note(limit)
            )
        }
        ShadowLine::Close { peak_need, ms } => {
            let mut ran = String::new(); // alloc: Cold
            if mode == mcx::ladder::ON {
                let (counts, climbs) = mcx::ladder::counts();
                ran = format!("; the gates' climbs since start {climbs}:"); // alloc: Cold
                for r in ladder::ASKER {
                    let (n, b) = counts[r as usize];
                    if n != 0 {
                        ran.push_str(&format!(" {} {n} runs {} MB", r.name(), b >> 20)); // alloc: Cold
                    }
                }
            }
            format!(
                "ladder {} episode {episode} closes: read {} MB under its line {} MB after {ms} ms (peak need {} MB){ran}",
                if mode == mcx::ladder::ON { "on" } else { "shadow" },
                mb(read),
                mb(line),
                mb(peak_need)
            )
        }
    };
    log_line(&text);
    if closes {
        log_line(&ladder_wiring_line());
    }
    if closed {
        // LEDGER2: the owner tree's shadow since start, skipped decisions counted (main 09:50:42Z: the R4a filter drops
        // nothing silently)
        let mut b = [0u8; 512];
        let n = mcx::ledger2::summary_line(&mut b);
        if let Ok(t) = core::str::from_utf8(&b[..n]) {
            log_line(t);
        }
    }
}

/// D4812: the last sample's read was past LOW, the line less C1's LOW headroom (`mcx::ladder::marks_now`).
static HINT_PAST_LOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// D4812: at each sample of a ladder episode (the read past LOW, from the ladder's first rung acting), the statements
/// that could outrank R6's pick once read again go to the residency thread (`mcx::governor::try_hint_stale`, R6's
/// minimum), which reads them before R6 picks; a busy lock skips the sample (counted).
fn sample_hint(limit: u64, line: u64, read: u64) {
    let past = read > line.saturating_sub(mcx::ladder::marks_now(limit).0);
    let was = HINT_PAST_LOW.swap(past, Ordering::Relaxed);
    if past {
        let _ = mcx::governor::try_hint_stale(mempolicy::consts::get(mempolicy::consts::K::CancelMin) as usize, !was);
    }
}

/// The ladder wiring witness's line (e2e memlead_ladder_wiring_witness), at each episode's close under ON: the climbs
/// that ran a rung per gate (LADDER-DESIGN section 3) and every rung's runs and measured relief in bytes, since start.
fn ladder_wiring_line() -> String {
    let (counts, _) = mcx::ladder::counts();
    let mut out = String::from("ladder on wiring: gate climbs"); // alloc: Cold
    for (g, n) in mcx::ladder::Gate::ALL.iter().zip(mcx::ladder::gate_climbs()) {
        out.push_str(&format!(" {} {n}", g.name())); // alloc: Cold
    }
    out.push_str("; rungs (runs, relief bytes)");
    for r in mempolicy::ladder::ASKER {
        let (n, b) = counts[r as usize];
        out.push_str(&format!(" {} {n} {b}", r.name())); // alloc: Cold
    }
    out
}

/// `ladder_reconcile`: the reads this thread computes (the ladder's, the kill reads' governed `resident`) and the check
/// of every term against the limiting cgroup's split at this sample (`mcx::reconcile::sample`); with no cgroup read
/// (no limit, or its files unreadable) the split is the process's: anon + shmem as current, no page cache.
fn reconcile_sample(limit: u64, line: u64, resident: u64) {
    use mempolicy::reconcile::{Split, Term};
    let seen = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    mcx::reconcile::put(Term::LadderRead, ladder_read_of(resident));
    mcx::reconcile::put(Term::Governed, resident);
    let current = seen(&CGROUP_SEEN.current);
    let split = if current == 0 {
        let own = LAST_OWN.load(Ordering::Relaxed);
        Split { current: own, anon: own, line, limit, ..Split::default() }
    } else {
        Split { current, anon: seen(&CGROUP_SEEN.anon), shmem: seen(&CGROUP_SEEN.shmem), file: seen(&CGROUP_SEEN.file), inactive: seen(&CGROUP_SEEN.inactive), clean: seen(&CGROUP_SEEN.clean), line, limit }
    };
    mcx::reconcile::sample(split);
}

/// The ladder's read (LADDER-DESIGN section 2, the user's Q1): memory.current less the clean inactive page cache at the
/// last sample; the governed `resident` with no cgroup.
/// D4364: never under the watchdog's own `resident` of the same sample. The allocator's lazily freed pages (MADV_FREE) sit
/// on the inactive file list, so memory.current less inactive_file leaves them out while the process's RssAnon (the
/// watchdog's read) counts them: the ladder read up to ~70 MB under the watchdog's in the wiring witness, under its line
/// while the watchdog's was past its cancel line, and the watchdog canceled first.
fn ladder_read_of(resident: u64) -> u64 {
    let current = CGROUP_SEEN.current.load(Ordering::Relaxed);
    if current != 0 {
        current.saturating_sub(CGROUP_SEEN.inactive.load(Ordering::Relaxed)).max(resident)
    } else {
        resident
    }
}

/// D4389: the ladder's last measured margin (bytes; `MARGIN_MIN` until the first), kept when the ring is busy.
static LADDER_MARGIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(mempolicy::ladder::MARGIN_MIN);

/// D4389 (section 7): the ladder's margin for `limit` now: the p99 rise of the governed resident over a cancel's landing
/// and one tick, from the 100 ms ring (`RISE`, pushed whatever the cancel rule), clamped by `mempolicy::ladder::margin`.
fn ladder_margin(limit: u64) -> u64 {
    let Ok(ring) = RISE.try_lock() else { return LADDER_MARGIN.load(Ordering::Relaxed) };
    let h = mempolicy::ladder::margin_window_ms(mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs));
    let m = mempolicy::ladder::margin(limit, ring.rise_p99_scaled(h));
    drop(ring);
    LADDER_MARGIN.store(m, Ordering::Relaxed);
    m
}

/// Ladder C1 (the broker's 10-16 note): LOW is the burst over one background batch's give-back at the measured rate and
/// a tick: the p99 rise of the ladder's read over windows that long (`mempolicy::ladder::low_window_ms`). `None` while
/// the ring is short or held (LOW keeps the last).
fn ladder_low_burst(limit: u64) -> Option<u64> {
    let h = mempolicy::ladder::low_window_ms(limit, mcx::ladder::give_back_ns_per_gib());
    RISE.try_lock().ok()?.rise_p99_scaled(h)
}

/// D4324 step 3: the read a gate's climb takes before and after each rung: a fresh sample of the sample files.
/// D4387: fresh always: when another thread holds the files (the watchdog or another gate, microseconds) the gate waits
/// for them rather than take the last tick's read (up to one period old, while concurrent loads grew ~1.6 GB/s), and
/// gates share one sample per millisecond (`FRESH_MS`), so a load's per-grain gate costs at most one sample per ms.
fn ladder_read_now() -> Option<u64> {
    let now = mcx::governor::decide_ms();
    let resident = match FRESH_RESIDENT.load(Ordering::Relaxed) {
        r if r != 0 && FRESH_MS.load(Ordering::Relaxed) == now => r,
        _ => {
            let r = sample_resident()
                .or_else(|| {
                    let mut s = pgsync::lock(&SAMPLE);
                    let mut b = pgsync::lock(&SAMPLE_BUF_MEM);
                    s.resident(&mut b)
                })
                .unwrap_or_else(|| LAST_RESIDENT.load(Ordering::Relaxed));
            FRESH_RESIDENT.store(r, Ordering::Relaxed);
            FRESH_MS.store(now, Ordering::Relaxed);
            r
        }
    };
    (resident != 0).then(|| ladder_read_of(resident))
}

/// D4387: the ladder's last fresh sample (`resident`) and the millisecond it was taken (`mcx::governor::decide_ms`).
static FRESH_RESIDENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FRESH_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static LADDER_ACTS: mcx::ladder::Acts = mcx::ladder::Acts {
    read: ladder_read_now,
    reclaimable: ladder_elastic,
    reclaim: ladder_reclaim,
    cancel: ladder_cancel,
    cancel_in_flight: ladder_cancel_in_flight,
    protected: ladder_protected,
    evict_protected: backstop_reclaim,
};

/// D4353, R6 (LADDER-DESIGN section 4) on this thread: the largest holder of at least cancel_min is canceled (the asker
/// too), unless this server is a bystander in its cgroup (D4262's test); with none, D4029's road (a canceled statement
/// that never answered is terminated). True when a cancel was raised.
fn ladder_cancel(_need: u64) -> bool {
    let (limit, resident) = (LIMIT_BYTES.load(Ordering::Relaxed), LAST_RESIDENT.load(Ordering::Relaxed));
    if limit == 0 || resident == 0 {
        return false;
    }
    let (read, line) = (ladder_read_of(resident), mcx::ladder::line());
    let own = match LAST_OWN.load(Ordering::Relaxed) {
        0 => read,
        o => o,
    };
    let Ok(largest) = mcx::governor::try_largest_live() else { return false };
    if !mempolicy::watchdog::acts(read, own, largest as u64, line) {
        return false;
    }
    let pct = (read.saturating_mul(100) / limit).clamp(1, 100) as usize;
    match mcx::governor::try_cancel_largest_victim(pct, mempolicy::consts::get(mempolicy::consts::K::CancelMin) as usize) {
        Ok(Some(v)) => {
            let at = mcx::governor::decide_ms();
            note_r6(at, read, v.bytes as u64, v.backend);
            note_r6_ack(at, v.backend, v.word);
            cancel_raised(v, read, limit, pct as i32, Some(line));
            true
        }
        Ok(None) => {
            let top = mempolicy::watchdog::tiers(guc_tables::backing::pgrust_memory_watchdog_threshold())[2];
            no_victim(pct, read, limit, pct as i32, top, Some(line));
            false
        }
        Err(_) => false,
    }
}

/// R6's one cancel in flight: one not yet answered (`r6_acks_hold`; past the watchdog's cancel line for `R6_ACK_BOUND_MS`
/// at most from its own raise; a busy registry: as R6's own raise, the next pass looks again), or (D4374) the last one's
/// bytes still landing (`r6_held`), or (D4886) the pool's R1 ask still landing past the cancel line (`pool_r1_holds`).
fn ladder_cancel_in_flight() -> bool {
    let (now, past) = (mcx::governor::decide_ms(), mcx::ladder::past_cancel());
    let own = r6_victim();
    let acks = mcx::governor::try_pending_acks(own.map(|(_, v)| v)).ok();
    r6_acks_hold(acks, own.map_or(0, |(raised, _)| raised), now, past)
        || ladder_read_now().is_some_and(|read| r6_held(now, read, past) || pool_r1_holds(read, LIMIT_BYTES.load(Ordering::Relaxed)))
}

/// Kill safety: whether the cancels waiting for their answer hold R6 (`acks`). Past the cancel line ANY unanswered
/// cancel holds it for `R6_ACK_BOUND_MS` at most from that cancel's own raise: another road's (the watchdog's, the
/// projection's) while one is that young (`other_young`), R6's own by its raise time (`r6_ack_held`, which counts the
/// timeout). Under the line each holds until answered, inside its ack window. R6's own victim is named by its cancel
/// word, so another road's pending cancel never times out R6's earlier, answered raise. Once R6's own cancel (the one
/// raised at `raised`, `r6_victim`'s) is answered, its raise time is cleared (`r6_answered`). A busy registry (None)
/// holds as R6's own raise would, uncounted.
fn r6_acks_hold(acks: Option<mcx::governor::PendingAcks>, raised: u64, now_ms: u64, past_cancel: bool) -> bool {
    let Some(a) = acks else { return mempolicy::ladder::r6_ack_holds(now_ms, raised, past_cancel) };
    if !a.own {
        r6_answered(raised);
    }
    let other = if past_cancel { a.other_young } else { a.other };
    other || (a.own && r6_ack_held(now_ms, past_cancel))
}

/// R6's last cancel while its raise is not answered (`R6_RAISED_AT` set): its raise time and its victim as
/// `mcx::governor::try_pending_acks` names it (its slot, its cancel word); None otherwise.
fn r6_victim() -> Option<(u64, (Option<usize>, usize))> {
    // Acquire: pairs with `note_r6_ack`'s Release, after the victim's slot and word
    let raised = R6_RAISED_AT.load(Ordering::Acquire);
    (raised != 0).then(|| (raised, (R6_VICTIM_SLOT.load(Ordering::Relaxed).checked_sub(1), R6_VICTIM_WORD.load(Ordering::Relaxed))))
}

/// Kill safety: R6's own cancel `word` raised at `at_ms` on its victim's `backend` (`Victim::word` / `::backend`), the
/// cancel `r6_ack_held` bounds past the cancel line. Beside `note_r6`, on every road that raises R6's cancel.
fn note_r6_ack(at_ms: u64, backend: Option<(usize, i32)>, word: usize) {
    R6_VICTIM_SLOT.store(backend.map_or(0, |(slot, _)| slot + 1), Ordering::Relaxed);
    R6_VICTIM_WORD.store(word, Ordering::Relaxed);
    // Release: a reader that sees this raise (Acquire) sees its victim above
    R6_RAISED_AT.store(at_ms.max(1), Ordering::Release);
}

/// Kill safety: R6's cancel raised at `raised` is answered (its victim's statement ended): it no longer holds R6 or
/// times out. A raise noted since (another time) is kept.
fn r6_answered(raised: u64) {
    if raised != 0 {
        let _ = R6_RAISED_AT.compare_exchange(raised, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// Kill safety: whether R6's last cancel, not yet answered, still holds the next one (`mempolicy::ladder::r6_ack_holds`):
/// past the cancel line for `R6_ACK_BOUND_MS` at most. The first time it stops holding a cancel counts an
/// `r6_ack_timeout`.
fn r6_ack_held(now_ms: u64, past_cancel: bool) -> bool {
    let raised = R6_RAISED_AT.load(Ordering::Relaxed);
    let holds = mempolicy::ladder::r6_ack_holds(now_ms, raised, past_cancel);
    if !holds && R6_ACK_TIMED_OUT.swap(raised, Ordering::Relaxed) != raised {
        mcx::ladder_stats::note_cancel(mcx::ladder_stats::Cancel::R6AckTimeout);
    }
    holds
}

/// R6's hold while the last cancel's bytes land (`r6_landing_at`), never past the watchdog's cancel line (`past_cancel`,
/// `mcx::ladder::past_cancel`): under reuse a victim's bytes go to the next hog and never fall from the read, so the
/// hold would last its whole landing time while the read climbs on to a kernel kill. A cancel not yet answered still
/// holds (`ladder_cancel_in_flight`): that is one in flight, not landing.
/// D4374 (`r6_land_past_cancel`): past the line, a victim that answered its cancel (its statement ended, `R6_RAISED_AT`
/// cleared) still holds R6 while its bytes land and the read is not rising since the last test (`R6_PREV_READ`; at the
/// cancel, the read then): a falling or flat read is the victim giving back, no climb to a kernel kill, and the next
/// pass 4-13 ms after the answer canceled an innocent aggregate while the hog's 848 MB drained (-50 MB at 13 ms, -738 MB
/// at 122 ms). Under reuse the next hog's growth raises the read, which ends the hold at that test.
fn r6_held(now_ms: u64, read_now: u64, past_cancel: bool) -> bool {
    if !past_cancel {
        return r6_landing_at(now_ms, read_now);
    }
    let prev = R6_PREV_READ.swap(read_now, Ordering::Relaxed);
    mempolicy::consts::get(mempolicy::consts::K::R6LandPastCancel) != 0
        && R6_RAISED_AT.load(Ordering::Acquire) == 0
        && mempolicy::ladder::r6_holds_past_cancel(read_now, prev)
        && r6_landing_at(now_ms, read_now)
}

/// D4374 (LADDER-DESIGN section 4): the ladder's last R6 cancel: when it was raised (decide ms; 0 = none), the ladder's
/// read then and the victim's bytes.
static R6_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Kill safety: when the ladder's last R6 cancel was raised (decide ms; 0 = none, or answered: `r6_answered`; the landing
/// test never clears it), its victim (`note_r6_ack`: its governor slot + 1, 0 = none; its cancel word), and the last
/// cancel counted as an `r6_ack_timeout` (its raise time).
static R6_RAISED_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_VICTIM_SLOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static R6_VICTIM_WORD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static R6_ACK_TIMED_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// D4374: the ladder's read at `r6_held`'s last test past the cancel line (the read at the cancel until one ran).
static R6_PREV_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The landing by attribution: the victim's backend account (its index + 1; 0 = none, the read decides), the id of the
/// statement running there at the cancel (never reused: the next statement or session on the slot is never it), the
/// bytes the account held before that statement (`memledger::statement_hold`'s, keeper blocks parked at the cancel
/// off), and the slack its threads' unfolded door-1 bytes leave.
static R6_ACCT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static R6_STMT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_BASE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static R6_SLACK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// With no account to judge by: the read less the reusable bytes at the cancel.
static R6_LIVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The wait on the purge after the victim let go: the count of the purger's passes that ends it (`purge::pass_mark` when
/// the let-go was first seen; 0 = not seen yet), the decide ms then (0 = not seen yet), and whether its end was counted.
static R6_LET_GO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_LET_GO_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_WAIT_COUNTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The cancel `note_r6` latches by attribution: `backend` (its governor slot = its process number, and its pid) when that
/// backend's account runs a statement holding more than its threads' unfolded bytes: (account, statement id, the
/// account's bytes at the statement's start, the slack). None: no backend, another session on the slot, no statement, or
/// a victim ranked by bytes its account does not hold (then the read judges the landing).
fn r6_victim_account(backend: Option<(usize, i32)>) -> Option<(memledger::Acct, u64, i64, i64)> {
    let (slot, pid) = backend?;
    let a = memledger::Acct::backend(u32::try_from(slot).ok()?);
    if !a.is_backend() {
        return None;
    }
    let row = memledger::account_row(a);
    let stmt = memledger::running_statement(a);
    let slack = (row.fold_slack as i64).max(memledger::DOOR_FOLD_BYTES);
    (u32::try_from(pid).ok() == Some(row.pid) && row.running && stmt != 0 && row.statement_net > slack)
        .then(|| (a, stmt, row.account_bytes.saturating_sub(row.statement_net).saturating_sub(memledger::keepers(a)), slack))
}

fn note_r6(at_ms: u64, read: u64, victim: u64, backend: Option<(usize, i32)>) {
    R6_READ.store(read, Ordering::Relaxed);
    R6_PREV_READ.store(read, Ordering::Relaxed);
    R6_BYTES.store(victim, Ordering::Relaxed);
    R6_LET_GO.store(0, Ordering::Relaxed);
    R6_LET_GO_MS.store(0, Ordering::Relaxed);
    R6_WAIT_COUNTED.store(false, Ordering::Relaxed);
    match r6_victim_account(backend) {
        Some((a, stmt, base, slack)) => {
            R6_STMT.store(stmt, Ordering::Relaxed);
            R6_BASE.store(base, Ordering::Relaxed);
            R6_SLACK.store(slack, Ordering::Relaxed);
            R6_ACCT.store(a.0 + 1, Ordering::Relaxed);
        }
        None => {
            R6_LIVE.store(Relief::of(Source::Reusable, mcx::ladder::reusable_bytes()).landing_read(read), Ordering::Relaxed);
            R6_ACCT.store(0, Ordering::Relaxed);
        }
    }
    // Release: a reader that sees this cancel (Acquire) sees its fields above
    R6_AT.store(at_ms.max(1), Ordering::Release);
}

/// D4374: whether the ladder's last R6 cancel is still landing at `now_ms` with the ladder's read `read_now`. Judged by
/// attribution: while the victim's statement still holds its bytes (`mempolicy::ladder::r6_victim_landing`; per call three
/// relaxed loads of its account, nothing written). A freed byte is no longer the victim's even while the allocator keeps
/// its page committed (the purger's later tick, reuse before release), so the read alone can stay up after the cancel
/// landed. With no account latched, the read: it has not fallen by half the victim's bytes, the whole read nor the read
/// less the reusable bytes (`mempolicy::ladder::r6_landing`). Once the victim let go, its pages are still on their way
/// out (the freeing threads' decommits, or a purger that owns the purge at its next tick): the hold waits for the read to
/// fall by half the victim's bytes or that tick, two purger periods at most (`r6_let_go_pending`), so no second
/// statement is canceled for bytes on their way back. Pressure, admission and the
/// cancel line read the whole memory as before; only this hold reads the victim.
/// D4391: a cancel found landed (its time past, its victim let go, or the read fallen by half its victim's bytes) is
/// done for good: the next grower that pushes the read back up within the landing time is not the old victim still
/// landing. The ladder's 100 ms sample tests it too (`ladder_shadow`), so a landing under the line, where no climb runs,
/// is seen.
/// D4426: so is a hold that a rising read ended (past the read at the cancel by more than the victim's bytes): the next
/// climb picks again.
fn r6_landing_at(now_ms: u64, read_now: u64) -> bool {
    let at = R6_AT.load(Ordering::Acquire);
    if at == 0 {
        return false;
    }
    let (landing_ms, read_at, victim) = (
        mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs),
        R6_READ.load(Ordering::Relaxed),
        R6_BYTES.load(Ordering::Relaxed),
    );
    let landing = match R6_ACCT.load(Ordering::Relaxed) {
        0 => {
            let live_now = Relief::of(Source::Reusable, mcx::ladder::reusable_bytes()).landing_read(read_now);
            let live_holds = mempolicy::ladder::r6_landing(now_ms, at, landing_ms, R6_LIVE.load(Ordering::Relaxed), live_now, victim);
            mempolicy::ladder::r6_landing(now_ms, at, landing_ms, read_at, read_now, victim)
                && (live_holds || r6_let_go_pending(now_ms, read_now))
        }
        i => {
            let (stmt, net) = memledger::statement_hold(memledger::Acct(i - 1));
            let holds = stmt == R6_STMT.load(Ordering::Relaxed)
                && net.saturating_sub(R6_BASE.load(Ordering::Relaxed)) > R6_SLACK.load(Ordering::Relaxed);
            mempolicy::ladder::r6_victim_landing(now_ms, at, landing_ms, read_at, read_now, victim, holds || r6_let_go_pending(now_ms, read_now))
        }
    };
    if !landing {
        // only the cancel this call read: a newer one (note_r6) is left alone
        let _ = R6_AT.compare_exchange(at, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
    landing
}

/// The victim let go of its bytes (its account, or with none the read less the reusable bytes), but their pages are still
/// on their way out of the memory: the hold waits (`mempolicy::ladder::r6_let_go_pending`) for the read to fall by half
/// the victim's bytes, or, when the purger owns the general arenas' purge (the pages then leave only at its pass), for one
/// pass begun after the let-go (one relaxed load of its pass count; the purger is unparked for it at once), at most
/// `R6_PURGE_WAIT_TICKS` of its period. The wait's end is counted once per cancel (`r6_hold_counts`).
fn r6_let_go_pending(now_ms: u64, read_now: u64) -> bool {
    let owns = purge::owns_general();
    let passes = purge::passes();
    // the first call that sees the let-go marks it: its time, then the purger's count to wait for (Release: a reader of
    // the count sees the time), and unparks the purger for that pass
    if R6_LET_GO_MS.compare_exchange(0, now_ms.max(1), Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        R6_LET_GO.store(purge::pass_mark(), Ordering::Release);
        if owns {
            purge::ask(true);
        }
    }
    let mark = R6_LET_GO.load(Ordering::Acquire);
    if mark == 0 {
        // another thread is marking it now
        return true;
    }
    let let_go_ms = R6_LET_GO_MS.load(Ordering::Relaxed);
    let (read_at, victim) = (R6_READ.load(Ordering::Relaxed), R6_BYTES.load(Ordering::Relaxed));
    let pending =
        mempolicy::ladder::r6_let_go_pending(owns, mark, passes, let_go_ms, now_ms, purge::TICK_MS, read_at, read_now, victim);
    if !pending && !R6_WAIT_COUNTED.swap(true, Ordering::Relaxed) {
        R6_PURGE_WAIT_MS.fetch_add(now_ms.saturating_sub(let_go_ms), Ordering::Relaxed);
        let ticked = owns && passes >= mark;
        let fell = read_now.saturating_add(victim / 2) <= read_at;
        if !ticked && !fell {
            R6_PURGE_WAIT_BOUNDED.fetch_add(1, Ordering::Relaxed);
        }
    }
    pending
}

/// The R6 landing's hold, counted (the accounts view's r6_landing_held_ms and r6_held_past_cancel): the milliseconds since
/// the previous hook pass of each pass past its wait whose climb still needed bytes while R6 was held for the last
/// cancel's landing, and those passes with the read past the cancel line. A landing while the pass's cancel-line flag is
/// up (`mcx::ladder::past_cancel`) holds nothing (`r6_held`), so it is not counted. Only the ladder's thread writes them.
static R6_HELD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_HELD_PAST_CANCEL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The wait after a victim let go (r6_purge_pending_ms, r6_purge_wait_bounded): its milliseconds from the let-go to its
/// end, summed, and the waits its bound ended before the read fell or the purger ticked.
static R6_PURGE_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_PURGE_WAIT_BOUNDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static R6_HOLD_PASS_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_r6_hold(now_ms: u64, landing: bool, past_cancel_flag: bool, past_cancel: bool) {
    let prev = R6_HOLD_PASS_MS.swap(now_ms, Ordering::Relaxed);
    if landing && !past_cancel_flag {
        if prev != 0 {
            R6_HELD_MS.fetch_add(now_ms.saturating_sub(prev), Ordering::Relaxed);
        }
        if past_cancel {
            R6_HELD_PAST_CANCEL.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The R6 landing's hold since start: (milliseconds held, passes held past the cancel line, milliseconds waited on the
/// purger after a let-go, waits the bound ended).
pub fn r6_hold_counts() -> (u64, u64, u64, u64) {
    (
        R6_HELD_MS.load(Ordering::Relaxed),
        R6_HELD_PAST_CANCEL.load(Ordering::Relaxed),
        R6_PURGE_WAIT_MS.load(Ordering::Relaxed),
        R6_PURGE_WAIT_BOUNDED.load(Ordering::Relaxed),
    )
}

/// D4353: the ladder's R2a count (Gravity's hosts; installed by the postmaster): what its reclaim can free now. Unset: the
/// backstop's count.
static LADDER_ELASTIC: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_ladder_elastic(f: fn() -> u64) {
    LADDER_ELASTIC.store(f as *mut (), Ordering::Release);
}

fn ladder_elastic() -> u64 {
    let p = LADDER_ELASTIC.load(Ordering::Acquire);
    if p.is_null() {
        return backstop_bytes();
    }
    // SAFETY: only set_ladder_elastic stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { std::mem::transmute(p) };
    f()
}

/// D4353 (C), LADDER-DESIGN section 0b: the ladder's R2a reclaim (down to each cache's protected floor) and R2b's bytes
/// (the protected cache not in use), Gravity's hosts'; installed by the postmaster. Unset: the backstop's reclaim, none.
static LADDER_RECLAIM: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static LADDER_PROTECTED: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_ladder_floors(reclaim: fn(u64) -> u64, protected: fn() -> u64) {
    LADDER_RECLAIM.store(reclaim as *mut (), Ordering::Release);
    LADDER_PROTECTED.store(protected as *mut (), Ordering::Release);
}

fn ladder_reclaim(want: u64) -> u64 {
    let p = LADDER_RECLAIM.load(Ordering::Acquire);
    if p.is_null() {
        return backstop_reclaim(want);
    }
    // SAFETY: only set_ladder_floors stores here, always from fn(u64) -> u64.
    let f: fn(u64) -> u64 = unsafe { std::mem::transmute(p) };
    f(want)
}

fn ladder_protected() -> u64 {
    let p = LADDER_PROTECTED.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_ladder_floors stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { std::mem::transmute(p) };
    f()
}
static LADDER_ACTS_IN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The shadow ladder's episode (only the watchdog thread samples).
static LADDER_SHADOW: pgsync::Mutex<mempolicy::ladder::Shadow> = pgsync::Mutex::new(mempolicy::ladder::Shadow::new());

// D3243 diagnostic: the governor's trips since the last line (at most one line every TRIP_LINE_SAMPLES governor samples, a
// second at the 100 ms cadence), with the allocator's figures, while the allocator's live bytes are counted
// (PGRUST_ALLOC_COUNT=1). The last trip's measures were taken at the trip. Only the watchdog thread writes these.
const TRIP_LINE_SAMPLES: u32 = 10;
static TRIPS_LOGGED: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
static SAMPLES_SINCE_LINE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(TRIP_LINE_SAMPLES);

fn log_trips(resident: Option<usize>) {
    if mcx::governor::allocator_in_use().is_none() {
        return;
    }
    let since = SAMPLES_SINCE_LINE.load(Ordering::Relaxed).saturating_add(1);
    SAMPLES_SINCE_LINE.store(since, Ordering::Relaxed);
    let counts = mcx::governor::trip_counts();
    let logged: [u64; 4] = core::array::from_fn(|k| TRIPS_LOGGED[k].load(Ordering::Relaxed));
    if counts == logged || since < TRIP_LINE_SAMPLES {
        return;
    }
    let moved: Vec<String> = (0..4)
        .filter(|&k| counts[k] > logged[k])
        .map(|k| format!("{} {}", counts[k] - logged[k], mcx::governor::TRIP_KINDS[k]))
        .collect();
    for (k, c) in counts.iter().enumerate() {
        TRIPS_LOGGED[k].store(*c, Ordering::Relaxed);
    }
    SAMPLES_SINCE_LINE.store(0, Ordering::Relaxed);
    let t = mcx::governor::last_trip();
    let resident = resident.unwrap_or_else(mcx::governor::projected);
    log_line(&format!(
        "memory governor refused {} since the last line; the last: {} \"{}\" asking {} kB at {} MB projected against the {} MB bar, allocator in use {} MB then; now resident {} MB: {}",
        moved.join(", "),
        t.kind,
        t.name,
        t.n >> 10,
        t.projected >> 20,
        t.bar >> 20,
        t.in_use.map_or(-1, |b| (b >> 20) as i64),
        resident >> 20,
        mcx::governor::allocator_census(resident).unwrap_or_default(),
    ));
}

// D3127: the governor's resident measure read now (a Gravity admission waiting out the sample's lag).
fn resident_now() -> Option<usize> {
    let l = current_limit();
    proc_status().map(|m| governed_resident(&m, &l) as usize)
}

// D3808: the dirty + writeback page-cache bytes inside `resident_now` (the limiting cgroup's memory.stat file_dirty +
// file_writeback; 0 when the limit is not a cgroup's, whose resident counts no page cache), for a bulk load's gate.
fn dirty_now() -> Option<usize> {
    match current_limit() {
        MemLimit { source: LimitSource::Cgroup, cgroup: Some(dir), .. } => {
            read_text(dir.join("memory.stat")).map(|s| dirty_file(&s) as usize)
        }
        _ => Some(0),
    }
}

// D3314: the limit is read again every `BROKER_LIMIT_CADENCE_MS` and published for Gravity's auto caps and its memory
// budget (memory step 5), whatever the governor's and the broker's settings; the governor samples every
// `GOVERNOR_CADENCE_MS` (both the memory policy's, `mempolicy::watchdog`).
use mempolicy::watchdog::{BROKER_LIMIT_CADENCE_MS, GOVERNOR_CADENCE_MS};

// wasm32: the wasi libc crate has no SIG* names; 15 is SIGTERM in the thread-signal emulation's numbering (procsignal).
#[cfg(not(target_family = "wasm"))]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(target_family = "wasm")]
const SIGTERM: i32 = 15;

fn refresh_broker_limit() -> bool {
    store_limit().1
}

// Memory step 6a: the memory broker's tick, installed by the postmaster before `start` (Gravity's hosts),
// run once a second after the broker's limit with the seconds since the watchdog started (never 0). The broker takes only
// short locks of its own consumers' (none this thread's path takes) and is contained per tick as the refreshes are.
static BROKER_TICK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_broker_tick(f: fn(u64)) {
    BROKER_TICK.store(f as *mut (), Ordering::Release);
}

fn broker_tick(sec: u64) {
    let p = BROKER_TICK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: only set_broker_tick stores here, always from the fn type.
    let f: fn(u64) = unsafe { std::mem::transmute(p) };
    f(sec);
}

/// B0: the work pool's tick (Gravity's hosts; installed by the postmaster before `start`), every `GOVERNOR_CADENCE_MS` with
/// the last sample's governed resident bytes and the room under the headroom margin (`mempolicy::sizes::pool_headroom_room`).
static POOL_TICK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_pool_tick(f: fn(u64, Option<u64>)) {
    POOL_TICK.store(f as *mut (), Ordering::Release);
}

fn pool_tick() {
    let (p, resident) = (POOL_TICK.load(Ordering::Acquire), LAST_RESIDENT.load(Ordering::Relaxed));
    if p.is_null() || resident == 0 {
        return;
    }
    let seen = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let room = mempolicy::sizes::pool_headroom_room(LIMIT_BYTES.load(Ordering::Relaxed), seen(&CGROUP_SEEN.current), seen(&CGROUP_SEEN.inactive));
    // SAFETY: only set_pool_tick stores here, always from the fn type.
    let f: fn(u64, Option<u64>) = unsafe { std::mem::transmute(p) };
    f(resident, room);
}

// D3872: the residency pass (Gravity's regions' resident pages read and their accounts' not-resident bytes set), installed
// by the postmaster before `start`; run at `mempolicy::watchdog::residency_period_ms` (every second at or under half of
// the budget B, every 200 ms past it) on pg:memresident (D4812 part 2), never on the watchdog's thread.
static RESIDENCY_PASS: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_residency_pass(f: fn() -> bool) {
    RESIDENCY_PASS.store(f as *mut (), Ordering::Release);
}

/// The residency pass's period now: the governed resident bytes against B at the current limit and bar.
fn residency_period() -> i64 {
    let (resident, limit) = (LAST_RESIDENT.load(Ordering::Relaxed), LIMIT_BYTES.load(Ordering::Relaxed));
    mempolicy::watchdog::residency_period_ms(resident, mempolicy::snapshot::b_now(limit, mcx::governor::bar() as u64))
}

fn residency_pass() {
    let p = RESIDENCY_PASS.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: only set_residency_pass stores here, always from the fn type.
    let f: fn() -> bool = unsafe { std::mem::transmute(p) };
    let _ = f();
}

/// D4812 part 2: pg:memresident, the thread of the walks over this process's pages: the residency pass (D3872's mincore
/// over Gravity's regions) and the retained reading (/proc/self/maps and mincore over the allocator's mappings). Each
/// mincore call allocates a kernel page for its vector, and under host memory pressure that allocation fell into global
/// direct reclaim (swap writeout, writeback throttling) for up to 3 s with the ladder's pass waiting on the watchdog's
/// thread. Here they wait alone. The thread keeps the watchdog's periods (`residency_period`, the retained reading's
/// `retained_every_ms` and RETAINED_DUE, which a rung's wake answers at once) and stamps each pass with the watchdog's own
/// clock; the watchdog counts the samples that found the stamp more than two periods old (`note_residency_age`).
static RESIDENT_THREAD: pgsync::OnceLock<pgsync::thread::Thread> = pgsync::OnceLock::new();
/// The watchdog's clock (WATCH_MS) at the last residency pass's end (0: none yet), the passes, and the watchdog's samples
/// that found the last pass older than two periods.
static RESIDENCY_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RESIDENCY_PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RESIDENCY_STALE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(the last residency pass's age on the watchdog's clock in ms, the samples that found it stale, the passes)`; the age
/// is the watchdog's whole run while no pass has ended.
pub fn residency_counts() -> (u64, u64, u64) {
    let (at, now) = (RESIDENCY_AT.load(Ordering::Relaxed), WATCH_MS.load(Ordering::Relaxed));
    (now.saturating_sub(at), RESIDENCY_STALE.load(Ordering::Relaxed), RESIDENCY_PASSES.load(Ordering::Relaxed))
}

fn start_resident() {
    let spawned = std::thread::Builder::new().name("pg:memresident".into()).stack_size(WATCHDOG_STACK).spawn(pagegive::pinned(|| {
        // a memory thread: its lines (the residency report) never wait on the log either
        memlog::defer_here();
        resident_loop();
    }));
    if let Ok(h) = spawned {
        let _ = RESIDENT_THREAD.set(h.thread().clone());
        // D4812: a cancel's pick hands the statements it found stale to this thread (it reads none itself)
        memledger::resident::set_hint_wake(wake_resident);
    }
}

fn wake_resident() {
    if let Some(t) = RESIDENT_THREAD.get() {
        t.unpark();
    }
}

fn resident_loop() {
    let step = mempolicy::watchdog::RESIDENCY_PRESSED_MS;
    let (mut since_residency, mut since_retained, mut at) = (0i64, 0i64, pg_clock::mono_ns());
    loop {
        pgsync::thread::park_timeout(std::time::Duration::from_millis(step as u64));
        let now = pg_clock::mono_ns();
        let ms = (now.saturating_sub(at) / 1_000_000) as i64;
        at = now;
        since_residency += ms;
        since_retained += ms;
        // unwind-ok: log-then-die
        let _ = std::panic::catch_unwind(memledger::resident::refresh_hinted);
        // unwind-ok: log-then-die
        if !RESIDENCY_PASS.load(Ordering::Acquire).is_null() && std::panic::catch_unwind(residency_period).is_ok_and(|p| since_residency >= p) {
            since_residency = 0;
            // unwind-ok: log-then-die
            let _ = std::panic::catch_unwind(residency_pass);
            RESIDENCY_AT.store(WATCH_MS.load(Ordering::Relaxed).max(1), Ordering::Relaxed);
            RESIDENCY_PASSES.fetch_add(1, Ordering::Relaxed);
            mcx::governor::note_residency_read();
        }
        // unwind-ok: log-then-die
        since_retained = std::panic::catch_unwind(|| retained_tick(since_retained)).unwrap_or(0);
    }
}

/// On the watchdog's thread, each sample: the residency pass's figures are stale when its last pass ended more than two
/// periods ago on the watchdog's clock (counted, never waited for).
fn note_residency_age() {
    if RESIDENCY_PASS.load(Ordering::Acquire).is_null() {
        return;
    }
    let (at, now) = (RESIDENCY_AT.load(Ordering::Relaxed), WATCH_MS.load(Ordering::Relaxed));
    if now.saturating_sub(at) > 2 * residency_period().max(0) as u64 {
        RESIDENCY_STALE.fetch_add(1, Ordering::Relaxed);
    }
}

// memfree Part 2: past pgrust.memory_cancel_threshold percent of the limit, the running statement that holds the most
// (its ledger, at least mempolicy's cancel_min) fails at its next block with 53200; one at a time. The thresholds are
// mempolicy::watchdog's. D3992: the backstop's rungs reclaim first (`mempolicy::watchdog::backstop`).

/// D3992: the backstop's rungs (Gravity's parked memory and every cache with a reclaim; installed by the postmaster from
/// the hosts): the bytes they could free now, and a reclaim of up to n bytes (the bytes freed).
static BACKSTOP_BYTES: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static BACKSTOP_RECLAIM: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
/// D3992: the backstop's last sample past the threshold reclaimed (its cancel waits one sample for those bytes).
static BACKSTOP_TRIMMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_backstop_rungs(bytes: fn() -> u64, reclaim: fn(u64) -> u64) {
    BACKSTOP_BYTES.store(bytes as *mut (), Ordering::Release);
    BACKSTOP_RECLAIM.store(reclaim as *mut (), Ordering::Release);
}

fn backstop_bytes() -> u64 {
    let p = BACKSTOP_BYTES.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_backstop_rungs stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { std::mem::transmute(p) };
    f()
}

fn backstop_reclaim(want: u64) -> u64 {
    let p = BACKSTOP_RECLAIM.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_backstop_rungs stores here, always from fn(u64) -> u64.
    let f: fn(u64) -> u64 = unsafe { std::mem::transmute(p) };
    f(want)
}

/// One backstop sample with `resident` past `pct` of `limit` (`top` the top tier): the rungs reclaim, the excess they leave
/// is asked of running statements as spills (C5's revoke, `pgrust.memory_revoke`), and `mempolicy::watchdog::cancels`
/// says whether this sample cancels. `None` under the threshold.
/// ALLOC-LADDER (a): the allocator's retained bytes now, the view's formula (`memledger::arena::retained_of`): the present
/// pages of its identifiable mappings plus the pool (a maps read + mincore: ~25 ms at 16 GiB) less what the accounts
/// hold of it. This remains a lower bound on kernels without VMA names; the SQL diagnostic omits the incomplete total.
/// `None` when the maps are unreadable.
pub fn retained_now() -> Option<u64> {
    let alloc_resident = allocator_resident_lower_bound()? as i64;
    let mut heap = 0i64;
    memledger::for_each_row(|r| heap = heap.wrapping_add(r.account_bytes.wrapping_sub(r.mapped_bytes)));
    let given = memledger::given_back_bytes();
    let (nr_heap, _) = memledger::not_resident_bytes();
    let live = mcx::governor::allocator_in_use().map_or(heap, |c| (c as i64).wrapping_sub(given));
    Some(memledger::arena::retained_of(alloc_resident, live, nr_heap) as u64)
}

/// The retained reading is due at the next sample (after a rung ran); else `mempolicy::alloc::retained_every_ms`, short
/// meaning the parked gate's predicate until the ladder's own exists.
static RETAINED_DUE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn memory_short_now() -> bool {
    mempolicy::cache::memory_short(&mempolicy::cache::ParkedSignals {
        bar: mcx::governor::bar() as u64,
        projected: mcx::governor::projected() as u64,
        free_b: memledger::free_after(0),
        b: memledger::b_at(),
        broker: false,
        bytes: 0,
        budget: u64::MAX,
    })
}

/// One retained reading when due; the elapsed ms since the last.
fn retained_tick(since: i64) -> i64 {
    let every = mempolicy::alloc::retained_every_ms(memory_short_now());
    if since < every && !RETAINED_DUE.swap(false, Ordering::Relaxed) {
        return since;
    }
    if let Some(r) = retained_now() {
        memledger::arena::note_retained(r);
    }
    0
}

/// ALLOC-LADDER (b), the ladder's first rung on this (the ladder's) thread: ask every pool worker to collect its own
/// pages at its next idle park, then purge the arenas once (`mcx::release_retained`, today's purge step). It never waits
/// for the answers; the yield is read at the next pass (the resident drop since the ask, logged; the retained reading
/// is due at once). At most once a second.
fn rung_return_allocator_memory(resident: u64) {
    // the watchdog's own clock (its samples' cadence): the rung runs on this thread only
    let now = WATCH_MS.load(Ordering::Relaxed);
    let last = RUNG1_AT_MS.load(Ordering::Relaxed);
    if last != u64::MAX && now.saturating_sub(last) < mempolicy::alloc::RUNG1_COOLDOWN_MS {
        return;
    }
    RUNG1_AT_MS.store(now, Ordering::Relaxed);
    let prev = RUNG1_AT_RESIDENT.swap(resident, Ordering::Relaxed);
    if prev != 0 {
        // the last rung's yield: the resident drop from its ask to now (measured, never assumed)
        RUNG1_LAST_YIELD.store(prev.saturating_sub(resident), Ordering::Relaxed);
    }
    // the give-back's whole allocator step: the own runs' hot pool, the owners' cache-heap and heap pages, the purges
    mcx::give_back_caches();
    // pagepool P3 (C.11): the pool's warm pages are allocator-retained free memory: R1 returns them too,
    // asked of the pool's purger (this thread never remaps); landing until its pass ran (`pagepool::landing`), never
    // freed. Only here, past the backstop's pressure line: never from the watchdog's periodic tick or an idle path
    if pagepool::on() {
        let _ = pagepool::return_warm();
    }
    RETAINED_DUE.store(true, Ordering::Relaxed);
    wake_resident();
}

/// pagepool P3: the pool's R1 faces for mcx's ladder (`mcx::ladder::PoolReturn`).
static POOL_RETURN: mcx::ladder::PoolReturn = mcx::ladder::PoolReturn {
    ask: pool_return_ask,
    releasable: || (pagepool::releasable() as u64).saturating_add(if on_watchdog() { pagepool::reserve_kept() } else { 0 }),
};

std::thread_local! {
    /// The memory watchdog's thread (the ladder's sample: its hook pass and background pass), marked at its start.
    static WATCHDOG_THREAD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

fn on_watchdog() -> bool {
    WATCHDOG_THREAD.try_with(|t| t.get()).unwrap_or(false)
}

/// The ladder's sample in progress (the watchdog thread's): its read, line and limit, for the reserve's shed size.
static SAMPLE_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SAMPLE_LINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SAMPLE_LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// R1's ask of the pool (its garbage, landing), then on the ladder's own passes only (the watchdog's thread; a gate's
/// climb on a query thread never) the warm reserve's shed while the ladder sizes it: R2a's first donor, ahead of every
/// cache, landing too. Sized to the pass's need less R1's landing: past the line the hook pass's (read - line), else the
/// background pass's batch toward HIGH; past the cancel line the whole reserve. Its budget comes down to what is left
/// (`note_taken`: a reversal if within the backoff of a raise).
fn pool_return_ask() -> u64 {
    let landing = pagepool::return_warm() as u64;
    if !on_watchdog() || !pagepool::reserve_on() {
        return landing;
    }
    let need = if mcx::ladder::past_cancel() {
        u64::MAX
    } else {
        let (read, line) = (SAMPLE_READ.load(Ordering::Relaxed), SAMPLE_LINE.load(Ordering::Relaxed));
        let high = mcx::ladder::marks_now(SAMPLE_LIMIT.load(Ordering::Relaxed)).1;
        if read > line { read - line } else { high.saturating_sub(line - read).min(mempolicy::ladder::C1_BATCH_MAX) }.saturating_sub(landing)
    };
    let shed = pagepool::reserve_ask(usize::try_from(need).unwrap_or(usize::MAX)) as u64;
    if shed != 0 {
        mcx::ladder::note_taken(mcx::ladder::SIZED_RESERVE, pagepool::reserve_probe().0.saturating_sub(shed));
    }
    landing.saturating_add(shed)
}

/// The watchdog thread's elapsed ms (advanced by its loop at each sample) and the first rung's last run on it.
static WATCH_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The watchdog thread's elapsed ms on its own clock (`GOVERNOR_CADENCE_MS` a sample): it moves with the wall clock while
/// the samples (the ladder's pass among them) keep their period, and stops while the thread waits.
pub fn watch_ms() -> u64 {
    WATCH_MS.load(Ordering::Relaxed)
}
static RUNG1_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
static RUNG1_AT_RESIDENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The last measured yield of the first rung (bytes), for the ladder's floor test and the log.
pub static RUNG1_LAST_YIELD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn backstop_sample(resident: u64, limit: u64, pct: i32, top: u64) -> Option<bool> {
    let trimmed = BACKSTOP_TRIMMED.load(Ordering::Relaxed);
    let step = mempolicy::watchdog::backstop(resident, limit, pct, top, backstop_bytes(), trimmed)?;
    // R1 before the cache rungs, in the same pass (memlead section 11: the asks go out, the ladder moves on)
    rung_return_allocator_memory(resident);
    if step.trim != 0 {
        let freed = backstop_reclaim(step.trim);
        if freed != 0 {
            log_line(&format!(
                "resident {} MB is past {pct}% of the {} MB limit: reclaimed {} MB of parked memory and caches ({} MB wanted) {}{}",
                resident >> 20,
                limit >> 20,
                freed >> 20,
                step.trim >> 20,
                if step.cancel { "and canceling" } else { "before any cancel" },
                pressure_note(limit)
            ));
        }
    }
    // C5: what the reclaim leaves, asked of running statements as spills (pgrust.memory_revoke; nothing while it is off)
    let asked = if step.revoke != 0 { mcx::revoke::request_revoke(step.revoke, 0, None) } else { 0 };
    if asked != 0 {
        log_line(&format!(
            "resident {} MB is past {pct}% of the {} MB limit: asked running statements to spill {} MB ({} asks, {} MB since start){}",
            resident >> 20,
            limit >> 20,
            asked >> 20,
            mcx::revoke::ASKS.load(Ordering::Relaxed),
            mcx::revoke::ASKED_BYTES.load(Ordering::Relaxed) >> 20,
            pressure_note(limit)
        ));
    }
    // D4314 (a): an ask is no relief, so it neither holds this sample's cancel nor arms the next one's
    BACKSTOP_TRIMMED.store(step.trim != 0, Ordering::Relaxed);
    Some(mempolicy::watchdog::cancels(step, asked))
}

fn refresh_cancel(limit: u64, resident: u64) {
    refresh_cancel_by(limit, resident, mcx::governor::try_largest_live)
}

/// `refresh_cancel` with `largest` the largest live statement's reader (`mcx::governor::try_largest_live`; D4417's witness
/// passes one that answers busy, as a held pins table or registry does).
fn refresh_cancel_by(limit: u64, resident: u64, largest: LargestLive) {
    let Some((past, least)) = backstop_cancels(limit, resident, largest) else { return };
    let pct = guc_tables::backing::pgrust_memory_cancel_threshold();
    // a busy registry or pins table: the next sample retries
    match mcx::governor::try_cancel_largest_victim(past, least) {
        Ok(Some(v)) => cancel_raised(v, resident, limit, pct, None),
        Ok(None) => no_victim(past, resident, limit, pct, mempolicy::watchdog::tiers(guc_tables::backing::pgrust_memory_watchdog_threshold())[2], None),
        Err(_) => {}
    }
}

/// D4886: the pool's R1 ask its purger has not served yet (`pagepool::landing`, for at most `cancel_landing_ms` after the
/// ask; a test's fake in its place).
fn pool_r1_landing() -> u64 {
    #[cfg(test)]
    if let Some(f) = *tests::POOL_LANDING.lock().unwrap_or_else(|e| e.into_inner()) {
        return f();
    }
    pagepool::landing(u32::try_from(mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs)).unwrap_or(u32::MAX)) as u64
}

/// D4886: past the watchdog's cancel line and under its top tier, the pool's R1 ask still landing (`pool_r1_landing`)
/// holds a cancel while it covers the excess over the line (`mempolicy::watchdog::landing_holds`): the backstop's
/// (`backstop_cancels`, with the cancels landing) and R6's (`ladder_cancel_in_flight`; past the cancel line no other
/// landing holds R6). The purger's pass that serves the ask ends the hold; the read after it decides. On a 32 GiB
/// Graviton's 100M-row bulk load (Hearth) the backstop canceled the COPY at 91 % with 12.1 GB of the pool's free runs
/// asked of the purger in the same sample and not yet released.
fn pool_r1_holds(read: u64, limit: u64) -> bool {
    let top = mempolicy::watchdog::tiers(guc_tables::backing::pgrust_memory_watchdog_threshold())[2];
    let (pct, pool) = (guc_tables::backing::pgrust_memory_cancel_threshold(), pool_r1_landing());
    let held = mempolicy::watchdog::landing_holds(read, limit, pct, top, pool);
    if held {
        note_pool_hold(read, limit, pct, pool);
    }
    held
}

/// D4886: one line an episode when the pool's R1 ask landing holds a cancel (the backstop's or R6's): the bytes the
/// purger is giving back, so a load that rode a crossing out reads as held, not as never past the line.
fn note_pool_hold(read: u64, limit: u64, pct: i32, pool: u64) {
    if !POOL_HOLD_LOGGED.swap(true, Ordering::Relaxed) {
        log_line(&format!(
            "resident {} MB is past {pct}% of the {} MB limit: cancel held while the pool gives back {} MB (its R1 ask, landing for at most {} ms){}",
            read >> 20,
            limit >> 20,
            pool >> 20,
            mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs),
            pressure_note(limit)
        ));
    }
}

/// The backstop's sample past the cancel line (the ladder off): its rungs, then whether it cancels: `Some((past, least))`
/// for the victim pick, None while under the line, a bystander's, held by the cancels or the pool's R1 ask landing, or
/// with the ladder on.
fn backstop_cancels(limit: u64, resident: u64, largest: LargestLive) -> Option<(usize, usize)> {
    let pct = guc_tables::backing::pgrust_memory_cancel_threshold();
    if !mempolicy::watchdog::cancel_armed(pct) {
        return None;
    }
    let Some((past, mut least)) = mempolicy::watchdog::cancel_at(resident, limit, pct) else {
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        BYSTANDER_LOGGED.store(false, Ordering::Relaxed);
        MEASURED_LOGGED.store(false, Ordering::Relaxed);
        POOL_HOLD_LOGGED.store(false, Ordering::Relaxed);
        return None;
    };
    // USER RULING 2026-10-13 17:3xZ (memory Group 1): with pgrust.memory_ladder = on the watchdog measures and logs only;
    // every cancel is the ladder's (R2b then R6, at once past this line: D4364's waiver in ladder_shadow). Its backstop
    // rungs (the reclaim, the revoke asks) are the ladder's R2a / R4b and do not run here either. Its tier lines stay.
    if mcx::ladder::on() {
        if !MEASURED_LOGGED.swap(true, Ordering::Relaxed) {
            log_line(&format!(
                "resident {} MB is past {past}% of the {} MB limit: the watchdog measures only (pgrust.memory_ladder = on: the ladder cancels){}",
                resident >> 20,
                limit >> 20,
                pressure_note(limit)
            ));
        }
        return None;
    }
    // a bystander's sample, or (D4417) one that could not tell: the next sample decides
    if bystander(resident, LAST_OWN.load(Ordering::Relaxed), limit, past, largest) != Some(false) {
        return None;
    }
    let top = mempolicy::watchdog::tiers(guc_tables::backing::pgrust_memory_watchdog_threshold())[2];
    if backstop_sample(resident, limit, pct, top) != Some(true) {
        return None;
    }
    // pagepool P3: the pool's R1 ask its purger has not served yet holds a cancel as an ask landing does. D4886: read
    // after this sample's ask (`backstop_sample`'s R1): read before it, the sample that first crossed the line (the first
    // to ask) saw none landing and canceled in the same sample
    let (pending, pool) = (mcx::governor::decide_inputs().pending, pool_r1_landing());
    if mempolicy::watchdog::landing_holds(resident, limit, pct, top, pending.saturating_add(pool)) {
        if pool != 0 && !mempolicy::watchdog::landing_holds(resident, limit, pct, top, pending) {
            note_pool_hold(resident, limit, pct, pool);
        }
        return None;
    }
    // D3941: under the projection rule the backstop takes no statement smaller than R's excess over B until R nears the kill
    let i = mcx::governor::decide_inputs();
    if mempolicy::decide::projection() && i.l != 0 && i.b != 0 {
        least = least.max(mempolicy::decide::backstop_least(&i, resident) as usize);
    }
    Some((past, least))
}

/// The watchdog's one line per episode past its cancel line while the ladder is on (it cancels nothing then).
static MEASURED_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// D4262: past `past` % of `limit`, whether this server is a bystander (`mempolicy::watchdog::acts` false): other processes
/// in its cgroup hold the overage and neither its own view (`own`, the last sample's `LAST_OWN`; 0 = none yet, read as the
/// resident) nor its largest statement can relieve it. Then nothing is asked or canceled, and one line per episode says
/// so. `Some(false)`: this server acts. D4417: `None` (unknown) when the answer needs the largest statement and the pins
/// table or the registry is held: that sample asks and cancels nothing and the next one decides (before, a busy registry
/// read "not a bystander", so a bystander's backstop asked its statements to spill and could cancel its largest). A
/// server holding `TENANT_PCT` of the cgroup acts without reading the registry.
///
/// BYSTRIM (main 2026-10-12T22:29:54Z): a bystander still runs the reclaim rung on its OWN idle memory (parked regions,
/// cache trims, `backstop_reclaim`) as the backstop's sample would: over the line that has no victim and lowers the risk
/// of a kernel kill. A trim after the episode's first line gets its own line.
fn bystander(resident: u64, own: u64, limit: u64, past: usize, largest: LargestLive) -> Option<bool> {
    let own = if own == 0 { resident } else { own };
    let line = (limit / 100).saturating_mul(past as u64);
    // the tenant's own share acts whatever its largest statement holds (`acts` with none): no registry read
    if mempolicy::watchdog::acts(resident, own, 0, line) {
        return Some(false);
    }
    let largest = largest().ok()?;
    if mempolicy::watchdog::acts(resident, own, largest as u64, line) {
        return Some(false);
    }
    BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
    let (freed, wanted) = bystander_trim(resident, limit, past);
    let trimmed = if freed != 0 {
        format!("reclaimed {} MB of its own parked memory and caches ({} MB wanted), ", freed >> 20, wanted >> 20)
    } else {
        String::new()
    };
    if !BYSTANDER_LOGGED.swap(true, Ordering::Relaxed) {
        log_line(&format!(
            "resident {} MB is past {past}% of the {} MB limit: this server holds {} MB ({}%) and its largest statement {} MB, under the {} MB over the line; the rest is other processes' in the cgroup: {trimmed}nothing asked or canceled{}",
            resident >> 20,
            limit >> 20,
            own >> 20,
            own.saturating_mul(100) / resident.max(1),
            largest >> 20,
            resident.saturating_sub(line) >> 20,
            pressure_note(limit)
        ));
    } else if freed != 0 {
        log_line(&format!(
            "resident {} MB is past {past}% of the {} MB limit, a bystander: {trimmed}nothing asked or canceled{}",
            resident >> 20,
            limit >> 20,
            pressure_note(limit)
        ));
    }
    Some(true)
}

/// D4417: the largest live statement's rank as the victim pick ranks it (`mcx::governor::try_largest_live`); `Err(Busy)`
/// while the pins table or the registry is held.
type LargestLive = fn() -> Result<usize, mcx::governor::Busy>;

/// BYSTRIM: a bystander's reclaim rung, the backstop sample's (`mempolicy::watchdog::backstop`'s trim: the excess over the
/// re-arm line, at most what the rungs could free now) on this server's own parked memory and caches; no ask, no cancel.
/// The bytes freed and wanted.
fn bystander_trim(resident: u64, limit: u64, past: usize) -> (u64, u64) {
    let Some(step) = mempolicy::watchdog::backstop(resident, limit, past as i32, 100, backstop_bytes(), false) else { return (0, 0) };
    rung_return_allocator_memory(resident);
    if step.trim == 0 {
        return (0, 0);
    }
    (backstop_reclaim(step.trim), step.trim)
}

/// The backstop's cancel landed on `victim` (`ladder`: the ladder's R6 at that line, D4353, with `resident` its read).
fn cancel_raised(victim: mcx::governor::Victim, resident: u64, limit: u64, pct: i32, ladder: Option<u64>) {
    mcx::ladder_stats::note_cancel(if ladder.is_some() { mcx::ladder_stats::Cancel::LadderR6 } else { mcx::ladder_stats::Cancel::Watchdog });
    let victim = Some(victim);
    // D3821: the victim is also condemned and its backend's interrupt and Gravity abort byte raised, so it stops at its
    // next interrupt poll or kernel back-edge, not only at its next block or draw (p99 2.7 s, max 15.9 s before)
    if let Some((slot, pid)) = victim.and_then(|v| v.backend) {
        procsignal::RaiseAbortByProcNumber(slot as _, pid);
    }
    let (held, charged) = victim.map_or((0, 0), |v| (v.bytes, v.charged));
    if held != 0 {
        // D3243 diagnostic: the allocator's split of the same resident bytes (while counted)
        let split = mcx::governor::allocator_census(resident as usize).map_or_else(String::new, |c| format!(" ({c}, MiB)"));
        // the victim's backend, so a cancel that is never answered can be traced to its session
        let who = victim.and_then(|v| v.backend).map_or_else(|| " in no backend".to_string(), |(slot, pid)| format!(" in pid {pid} (slot {slot})"));
        let head = match ladder {
            Some(line) => format!("ladder on R6: read {} MB is past its line {} MB of the {} MB limit: canceling the largest holder", resident >> 20, line >> 20, limit >> 20),
            None => format!("resident {} MB is past {pct}% of the {} MB limit: canceling the statement holding the most memory", resident >> 20, limit >> 20),
        };
        log_line(&format!(
            "{head} ({} MB) by resident bytes ({} MB charged){who}{split}{}{}",
            held >> 20,
            charged >> 20,
            pressure_note(limit),
            victim.map_or_else(String::new, |v| ranked_line(&v.top))
        ));
    }
}

/// memfuzz (P4): R6's ranked candidates on its cancel line, largest first: "; ranked: pid P (slot S) rank R MB
/// (resident X MB, parts Y MB)[, canceled]; ..." (the rank is `rank_bytes` over those terms, the charge where the row is
/// below its start); empty when the pick kept none (the projection's cancel).
fn ranked_line(top: &[mcx::governor::Cand]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for c in top.iter().filter(|c| c.rank != 0) {
        out.push_str(if out.is_empty() { "; ranked: " } else { "; " });
        let _ = match c.backend {
            Some((slot, pid)) => write!(out, "pid {pid} (slot {slot})"),
            None => write!(out, "no backend"),
        };
        let _ = write!(out, " rank {} MB (resident {} MB, parts {} MB)", c.rank >> 20, c.resident >> 20, c.parts >> 20);
        if c.canceled {
            out.push_str(", canceled");
        }
    }
    out
}

/// D4029: the session the backstop last terminated (pid, its statement's cancel word, when, refused) on this thread.
static TERMINATED: pgsync::Mutex<Option<(i32, usize, u64, bool)>> = pgsync::Mutex::new(None);
static NO_VICTIM_LINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// D4029: past the line and no statement holds cancel_min (`mempolicy::watchdog::no_victim`): before, the backstop wrote
/// nothing while a canceled statement that never answered grew to the kernel's kill.
fn no_victim(past: usize, resident: u64, limit: u64, pct: i32, top: u64, ladder: Option<u64>) {
    use mempolicy::watchdog::{NoVictim, CANCEL_ACK_MS, NO_VICTIM_LINE_MS};
    let tag = if ladder.is_some() { "ladder on R6: " } else { "" };
    let Ok(v) = mcx::governor::try_no_victim_view() else { return };
    let Ok(mut term) = TERMINATED.try_lock() else { return };
    let now = mcx::governor::decide_ms();
    let terminated = v.stalled.and_then(|s| {
        let t = (*term)?;
        (Some(t.0) == s.backend.map(|b| b.1) && t.1 == s.word).then(|| if t.3 { CANCEL_ACK_MS } else { now.saturating_sub(t.2) })
    });
    let at_top = resident >= (limit / 100).saturating_mul(top.max(pct as u64));
    let stalled = v.stalled.map(|s| (s.now > s.ranked && s.backend.is_some(), terminated));
    let mb = |b: usize| b >> 20;
    match mempolicy::watchdog::no_victim(v.pending_ack, stalled, at_top) {
        NoVictim::Wait => {}
        NoVictim::Terminate => {
            let Some(s) = v.stalled else { return };
            let Some((slot, pid)) = s.backend else { return };
            let refused = procsignal::SendThreadSignalByProcNumber(slot as _, pid, SIGTERM) != 0;
            mcx::ladder_stats::note_cancel(mcx::ladder_stats::Cancel::Terminate);
            *term = Some((pid, s.word, now, refused));
            log_line(&format!(
                "{tag}resident {} MB is past {pct}% of the {} MB limit: terminating the session of pid {pid} (slot {slot}): its statement was canceled {} ms ago, never answered, and grew {} -> {} MB{}{}",
                resident >> 20,
                limit >> 20,
                s.age_ms,
                mb(s.ranked),
                mb(s.now),
                if refused { " (refused: no such backend)" } else { "" },
                pressure_note(limit)
            ));
        }
        NoVictim::Smaller => {
            if let Ok(Some(v)) = mcx::governor::try_cancel_largest_victim(past, 1) {
                if ladder.is_some() {
                    note_r6(now, resident, v.bytes as u64, v.backend);
                    note_r6_ack(now, v.backend, v.word);
                }
                cancel_raised(v, resident, limit, pct, ladder);
            }
        }
        NoVictim::Line => {
            let last = NO_VICTIM_LINE.load(Ordering::Relaxed);
            if last != 0 && now.saturating_sub(last) < NO_VICTIM_LINE_MS {
                return;
            }
            NO_VICTIM_LINE.store(now, Ordering::Relaxed);
            let why = v.stalled.map_or_else(String::new, |s| {
                let who = s.backend.map_or_else(|| "no backend".to_string(), |(_, pid)| format!("pid {pid}"));
                format!("{who} canceled {} ms ago and unanswered ({} -> {} MB), ", s.age_ms, mb(s.ranked), mb(s.now))
            });
            log_line(&format!(
                "{tag}resident {} MB is past {pct}% of the {} MB limit: no victim: {why}the largest other statement holds {} MB < cancel_min ({} MB){}",
                resident >> 20,
                limit >> 20,
                mb(v.largest_other),
                mempolicy::consts::get(mempolicy::consts::K::CancelMin) >> 20,
                pressure_note(limit)
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// C3: the backstop of mempolicy::decide (pgrust.memory_cancel_rule = projection).
// ---------------------------------------------------------------------------

/// The resident ring for rise99 and U (`mempolicy::decide::Rise`): only this thread writes it; `try_lock` only.
static RISE: pgsync::Mutex<mempolicy::decide::Rise> = pgsync::Mutex::new(mempolicy::decide::Rise::new());

/// The fast rungs (Gravity's parked regions, slot sets and managed caches; installed by the postmaster from the hosts):
/// the bytes they could free now, and a reclaim of up to n bytes (the bytes freed).
static FAST_BYTES: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static FAST_RECLAIM: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_fast_rungs(bytes: fn() -> u64, reclaim: fn(u64) -> u64) {
    FAST_BYTES.store(bytes as *mut (), Ordering::Release);
    FAST_RECLAIM.store(reclaim as *mut (), Ordering::Release);
}

fn fast_bytes() -> u64 {
    let p = FAST_BYTES.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_fast_rungs stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { std::mem::transmute(p) };
    f()
}

fn fast_reclaim(want: u64) -> u64 {
    let p = FAST_RECLAIM.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_fast_rungs stores here, always from fn(u64) -> u64.
    let f: fn(u64) -> u64 = unsafe { std::mem::transmute(p) };
    f(want)
}

/// The rule last announced (u8::MAX = none yet).
static RULE_ANNOUNCED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);
/// The backstop's decide ticks (`log_decides` writes at every tenth).
static DECIDE_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The decide counts and the revoke asks (count, bytes) at the last line (`log_decides`).
static DECIDES_LOGGED: pgsync::Mutex<Option<(mcx::governor::DecideCounts, u64, u64)>> = pgsync::Mutex::new(None);

/// pgrust.memory_cancel_rule read into `mempolicy::decide` (every 100 ms; SIGHUP moves it); a change is logged.
fn sync_rule() {
    use mempolicy::decide::{self, Rule};
    let rule = Rule::from_index(guc_tables::backing::pgrust_memory_cancel_rule());
    decide::set_rule(rule);
    let prev = RULE_ANNOUNCED.swap(rule as u8, Ordering::Relaxed);
    if prev == rule as u8 {
        return;
    }
    if rule == Rule::Projection {
        log_line("pgrust.memory_cancel_rule = projection: statements grow in credits checked on their own threads, starts wait and the statements past their fair share spill while free memory is short, and the cancel waits for a projection past the limit; past pgrust.memory_cancel_threshold the statement holding the most is still canceled (the backstop)");
    } else if prev != u8::MAX {
        log_line("pgrust.memory_cancel_rule = threshold: the statement holding the most is canceled past pgrust.memory_cancel_threshold");
    }
}

/// The sampler's period now (`mempolicy::decide::sample_period_ms`).
fn sample_period() -> u64 {
    if !mempolicy::decide::projection() {
        return mempolicy::decide::SAMPLE_MS;
    }
    let i = mcx::governor::decide_inputs();
    mempolicy::decide::sample_period_ms(mempolicy::decide::projection(), i.r, i.b)
}

/// The backstop's decide, every `SAMPLE_MS` (the samples between, under the projection rule, only refresh the governor's
/// R): the rule synced, the ring's sample, the inputs published for the inline decides, decide(Tick) and what it plans (the
/// fast rungs; the cancel's interrupt is raised inside `decide_tick`), the inline decides' trace and the counts' line.
fn decide_sample() {
    use mempolicy::decide;
    sync_rule();
    let projection = decide::projection();
    let (r, limit) = (LAST_RESIDENT.load(Ordering::Relaxed), LIMIT_BYTES.load(Ordering::Relaxed));
    // D4389: the ring feeds the ladder's measured margin whatever the rule (its unattributed band only under projection)
    if r != 0 {
        if let Ok(mut ring) = RISE.try_lock() {
            // pagepool P2: the pool's resident bytes no owner holds are retained, not unattributed; pppress card 9: door 1's
            // pool bytes (PoolAlloc's) are in doors_total on their accounts, so pool_unowned leaves them out (counted once)
            ring.push(r, if projection { r as i64 - memledger::doors_total() - memledger::pool_unowned() } else { 0 });
        }
    }
    if !projection {
        let _ = mcx::governor::decide_tick();
        return;
    }
    if r == 0 {
        return;
    }
    let b = mempolicy::snapshot::b_now(limit, mcx::governor::bar() as u64);
    let Ok(ring) = RISE.try_lock() else { return };
    let cancel_landing = mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs);
    mcx::governor::publish_decide(&mcx::governor::DecidePublish {
        l: limit,
        b,
        u: ring.u_band(b),
        fast: decide::fast_landed(fast_bytes()),
        rise_fast: ring.rise99(decide::SAMPLE_MS, b),
        rise_slow: ring.rise99(cancel_landing, b),
        revoke_on: mcx::revoke::enabled(),
    });
    drop(ring);
    let Some(t) = mcx::governor::decide_tick() else { return };
    let freed = if t.reclaim_fast { fast_reclaim(decide::fast_want(&t.inputs)) } else { 0 };
    if let Some(v) = t.victim {
        let who = v.backend.map_or_else(|| " in no backend".to_string(), |(slot, pid)| format!(" in pid {pid} (slot {slot})"));
        log_line(&format!(
            "resident {} MB is projected past the {} MB limit less its margin before memory can be freed (F {} MB, F* {} MB, pending {} MB, rise99 {} MB over {} ms): canceling the statement furthest past its fair share of {} MB ({} MB){who}",
            t.inputs.r >> 20,
            t.inputs.l >> 20,
            t.inputs.free() >> 20,
            t.inputs.target() >> 20,
            t.inputs.pending >> 20,
            t.inputs.rise_slow.bytes >> 20,
            cancel_landing,
            t.inputs.share() >> 20,
            v.bytes >> 20,
        ));
    }
    if mempolicy::trace::on() {
        let mut rec = decide::record(decide::Event::Tick, &t.inputs, &t.plan);
        rec.push_str(&format!(" freed_fast={freed} revoked={} victim={}", t.revoked, t.victim.map_or(0, |v| v.bytes)));
        mempolicy::trace::note("decide.tick", &rec);
        let dropped = mcx::governor::drain_inline_trace(|d| {
            let mut rec = decide::record(d.event, &d.inputs, &d.plan);
            rec.push_str(&format!(" at_ms={} slot={} victim={}", d.at_ms, d.slot.map_or(-1, |s| s as i64), d.victim.map_or(0, |v| v.bytes)));
            mempolicy::trace::note("decide.grow", &rec);
        });
        if dropped != 0 {
            mempolicy::trace::note("decide.grow", &format!("dropped={dropped} (the inline ring was full)"));
        }
    }
    log_decides();
}

/// One line a second at most while decide's counts move: inline decides, cancels by who decided them, spill-selfs, the
/// starts' throttle waits (count, p50 / p99 / max ms from log2 buckets: the gate's throttle-wait report, main's condition 3)
/// and the revoke asks (mcx::revoke::ASKS / ASKED_BYTES: a revoke is visible without the trace).
fn log_decides() {
    let n = DECIDE_TICKS.fetch_add(1, Ordering::Relaxed);
    if n % 10 != 9 {
        return;
    }
    let c = mcx::governor::decide_counts();
    let (asks, asked) = (mcx::revoke::ASKS.load(Ordering::Relaxed), mcx::revoke::ASKED_BYTES.load(Ordering::Relaxed));
    let Ok(mut last) = DECIDES_LOGGED.try_lock() else { return };
    let (prev, prev_asks, prev_asked) = last.unwrap_or_default();
    if (c, asks, asked) == (prev, prev_asks, prev_asked) {
        return;
    }
    *last = Some((c, asks, asked));
    let waits = c.throttle_waits - prev.throttle_waits;
    let hist: [u64; 16] = core::array::from_fn(|k| c.throttle_hist[k] - prev.throttle_hist[k]);
    let q = |p: u64| {
        let want = (waits * p).div_ceil(100).max(1);
        let mut seen = 0u64;
        for (k, n) in hist.iter().enumerate() {
            seen += n;
            if seen >= want {
                return (1u64 << (k + 1)) - 1;
            }
        }
        0
    };
    log_line(&format!(
        "memory decide: {} inline decides, {} cancels inline ({} of them the grower itself), {} by the backstop, {} statements sent down their own road; {} starts throttled, {} since start (wait ms <= p50 {} p99 {}, max {} since start); {} revoke asks ({} MB), {} since start ({} MB)",
        c.inline_decides - prev.inline_decides,
        c.inline_cancels - prev.inline_cancels,
        c.self_cancels - prev.self_cancels,
        c.backstop_cancels - prev.backstop_cancels,
        c.spill_selfs - prev.spill_selfs,
        waits,
        c.throttle_waits,
        if waits == 0 { 0 } else { q(50) },
        if waits == 0 { 0 } else { q(99) },
        c.throttle_max_ms,
        asks - prev_asks,
        (asked - prev_asked) >> 20,
        asks,
        asked >> 20,
    ));
}

// D4444, measured 2026-10-15 on the aarch64 dist build (fat LTO; prologue sp decrements over the direct-call tree):
// 170,128 B of frames on the deepest path, and glibc takes the 102,753 B static TLS block from this stack too
const WATCHDOG_STACK: usize = 1 << 20;

/// Spawn the watchdog thread. Postmaster boot wiring (ServerLoop, next to
/// rtpool::start_if_enabled). The thread is unconditional and near-free; the
/// master switch is read every tick, so `pgrust.memory_watchdog` can arm and
/// disarm a running server via SIGHUP.
/// `thread_init` runs first on the watchdog thread: the postmaster hands
/// over its log identity (MyProcPid/MyStartTime, the log-context provider)
/// so the watchdog's lines read as the postmaster's — C has no such thread,
/// and the closest C analogue of "the process noticed its memory" is the
/// postmaster speaking.
pub fn start(thread_init: impl FnOnce() + Send + 'static) {
    mcx::governor::set_resident_probe(|| {
        proc_status().map(|m| ((m.anon_kb + m.shmem_kb) * 1024) as usize)
    });
    mcx::governor::set_cgroup_resident_probe(cgroup_resident);
    mcx::governor::set_reconcile_probe(reconcile_terms);
    mcx::governor::set_allocator_resident_probe(allocator_resident);
    mcx::governor::set_region_resident_probe(region_resident);
    // C3: a projection cancel raises its victim's interrupt and Gravity abort byte as the watchdog's does (D3821)
    mcx::governor::set_cancel_raise(|slot, pid| {
        procsignal::RaiseAbortByProcNumber(slot as _, pid);
    });
    // elog's per-thread config (C's globals, inherited at fork) is
    // thread-local here and the watchdog thread binds no GUC store: carry
    // the postmaster's log_line_prefix and log_timezone over at spawn so
    // its lines print with the same prefix as everyone else's.
    let log_line_prefix = elog::config::log_line_prefix_format();
    let log_tz = localtime::globals::log_timezone();
    // D3314: the limit published before any backend runs, so the grant pool and Gravity's other auto caps, which arm at a
    // session's first serve, size from it and not from MemTotal
    refresh_limit();
    // D4812: the drainer of the memory threads' log writes, before the first of them; mcx's own lines (the victim pick's
    // ledger2 shadow line, revoke's settled-ask line) through elog's stderr writer, so they are deferred there too
    let _ = memlog::start();
    mcx::set_line_sink(|line| elog::write_stderr(line));
    // D4812 (2): the block gate's inline rungs write their lines through the same ring, on the allocating thread
    mcx::set_gate_log_defer(memlog::gate_defer_begin, memlog::gate_defer_end);
    // C3: the cache purger beside the watchdog (PGRUST_CACHE_PURGE_NOW=1; off: not started)
    let _ = purge::start();
    allocator_line();
    // pagepool P2: the pool's claimer and purger, here in the server process (the supervisor forks single-threaded)
    if pagepool::on() {
        let _ = pagepool::start();
        // pagepool P3: the ladder's R1 and its background pass reach the pool through these
        mcx::ladder::set_pool_return(&POOL_RETURN);
        // the warm reserve (PGRUST_POOL_RESERVE=1): the ladder's fourth sized cache (held, foreground miss bytes)
        if pagepool::reserve_on() {
            mcx::ladder::set_sized_probe(mcx::ladder::SIZED_RESERVE, pagepool::reserve_probe);
        }
    }
    // D4692 (cache_release_bg; off: not started): the page releaser, so cache runs freed on query threads go back here;
    // H3 step (1) (give_back_service; off: as before): the same thread as the give-back service
    let _ = release::start();
    // D4812 part 2: the page walks (the residency pass, the retained reading) on a thread of their own
    start_resident();
    let _ = std::thread::Builder::new()
        .name("pg:memwatchdog".into())
        .stack_size(WATCHDOG_STACK)
        .spawn(pagegive::pinned(move || {
            elog::config::set_log_line_prefix(log_line_prefix);
            localtime::globals::set_log_timezone(log_tz);
            thread_init();
            // D4812: this thread's server-log writes go to pg:memlog; it never waits on the log's file or pipe
            memlog::defer_here();
            mcx::governor::set_resident_probe(resident_now);
            mcx::governor::set_dirty_probe(dirty_now);
            WATCHDOG_THREAD.with(|t| t.set(true));
            prefault();
            let mut state = WatchState::default();
            let mut high = HighWatch::default();
            // D4290: the high watch's rate limit counts the loop's cadence (the watchdog reads no clock)
            let mut clock_ms = 0u64;
            let mut since_tick = 0i64;
            let mut since_limit = BROKER_LIMIT_CADENCE_MS;
            let mut broker_sec = 0u64;
            let mut limit_due = false;
            let mut since_decide = 0i64;
            loop {
                // C3: GOVERNOR_CADENCE_MS, or 10 ms under the projection rule while resident memory is past B / 2
                // unwind-ok: log-then-die
                let period = std::panic::catch_unwind(sample_period).map_or(GOVERNOR_CADENCE_MS, |p| p as i64);
                // the allocator's entry point (ALLOC-LADDER (c)): a take of mimalloc past the line ends the park at once
                memledger::arena::park_ladder(period as u64);
                let due = memledger::arena::take_ladder_due();
                // `pgrust.memory_revoke` (PGC_SIGHUP) to the revoke registry: off asks nobody
                mcx::revoke::set_enabled(guc_tables::backing::pgrust_memory_revoke());
                // LEDGER2 step 4: `pgrust.memory_ledger_v2` (PGC_SIGHUP) to the cancel pick's rank
                mcx::ledger2::set_on(guc_tables::backing::pgrust_memory_ledger_v2());
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(refresh_governor);
                since_decide += period;
                if due {
                    since_decide = since_decide.max(GOVERNOR_CADENCE_MS);
                }
                if since_decide < GOVERNOR_CADENCE_MS {
                    continue;
                }
                since_decide = 0;
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(decide_sample);
                WATCH_MS.fetch_add(GOVERNOR_CADENCE_MS as u64, Ordering::Relaxed);
                // LADDER-DESIGN 6a: a cache give-back's second purge, after the owners answered its collect ask
                let _ = mcx::purge_after_ask();
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(pool_tick);
                // D4812 part 2: the residency pass and the retained reading run on pg:memresident; here only their age
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(note_residency_age);
                since_limit += GOVERNOR_CADENCE_MS;
                if since_limit >= BROKER_LIMIT_CADENCE_MS || limit_due {
                    // unwind-ok: log-then-die
                    limit_due = matches!(std::panic::catch_unwind(refresh_broker_limit), Ok(false));
                }
                if since_limit >= BROKER_LIMIT_CADENCE_MS {
                    since_limit = 0;
                    broker_sec += 1;
                    // unwind-ok: log-then-die
                    let _ = std::panic::catch_unwind(|| broker_tick(broker_sec));
                }
                // cache_track's drain every governor sample (D4892: a fresh door's samples reach its tracker within 100 ms;
                // the rate limit is still checked once a second, by the drain's own clock)
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(mempolicy::track::tick);
                since_tick += GOVERNOR_CADENCE_MS;
                clock_ms += GOVERNOR_CADENCE_MS as u64;
                let ms = guc_tables::backing::pgrust_memory_watchdog_interval().clamp(100, 60_000);
                if since_tick < ms as i64 && !state.store_due {
                    continue;
                }
                since_tick = 0;
                // The tick is written panic-free (no unwrap/index); the guard
                // is defense in depth for unwinding builds.
                // unwind-ok: log-then-die
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tick(&mut state);
                    // D4290: the kernel's throttling at a soft limit, whatever the watchdog's switch
                    if let Some((msg, hint)) = high_tick(&mut high, &current_limit(), clock_ms) {
                        warn_line(&msg, &hint, "high_tick");
                    }
                }));
            }
        }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4872: the PSI file's `some` total (µs), as /proc/pressure/memory and a cgroup's memory.pressure write it; a file
    /// without a `some` line or a total reads None (that signal does not press).
    #[test]
    fn d4872_psi_some_total_reads_the_some_line() {
        let host = b"some avg10=0.00 avg60=0.00 avg300=0.00 total=356581\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=352040\n";
        assert_eq!(psi_some_total(host), Some(356_581));
        let full_first = b"full avg10=1.00 avg60=0.00 avg300=0.00 total=7\nsome avg10=2.50 avg60=0.10 avg300=0.00 total=1906241\n";
        assert_eq!(psi_some_total(full_first), Some(1_906_241), "the some line, wherever it is");
        assert_eq!(psi_some_total(b"full avg10=0.00 avg60=0.00 avg300=0.00 total=24\n"), None, "no some line");
        assert_eq!(psi_some_total(b"some avg10=0.00 avg60=0.00 avg300=0.00\n"), None, "no total");
        assert_eq!(psi_some_total(b""), None);
    }

    /// The sample buffer's address (D4515's witness).
    fn sample_buf_addr() -> usize {
        pgsync::lock(&SAMPLE_BUF_MEM).as_ptr() as usize
    }

    /// D4515: the sample's 8 KiB read buffer lies in .bss (an anonymous writable mapping: no file bytes). Born red on trunk:
    /// it shared `SampleFiles` with Options whose None niches are not zero, which kept it in .data.
    #[cfg(target_os = "linux")]
    #[test]
    fn d4515_the_sample_buffer_is_in_bss() {
        let last = sample_buf_addr() + SAMPLE_BUF - 1;
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let line = maps
            .lines()
            .find(|l| {
                let (a, z) = l.split(' ').next().unwrap().split_once('-').unwrap();
                (usize::from_str_radix(a, 16).unwrap()..usize::from_str_radix(z, 16).unwrap()).contains(&last)
            })
            .unwrap_or_else(|| panic!("no mapping holds {last:#x}"));
        let f: Vec<&str> = line.split_whitespace().collect();
        assert!(f[1].starts_with("rw") && (f.len() == 5 || f[5] == "[heap]"), "the sample buffer is mapped from the binary (.data), not .bss: {line}");
    }

    /// D4444: glibc carves every thread's static TLS out of its own stack, so the watchdog's stack must hold twice
    /// the frames measured on the dist build plus this process's static TLS (no less than the server's measured block).
    #[cfg(not(target_family = "wasm"))]
    #[cfg(target_os = "linux")]
    #[test]
    fn d4444_the_watchdog_stack_holds_twice_its_dist_frames_and_the_static_tls() {
        const DIST_FRAMES: usize = 170_128;
        const SERVER_TLS: usize = 102_753;
        unsafe extern "C" fn add_tls(
            info: *mut libc::dl_phdr_info,
            _: libc::size_t,
            total: *mut libc::c_void,
        ) -> libc::c_int {
            // SAFETY: dl_iterate_phdr passes a valid dl_phdr_info whose dlpi_phdr holds dlpi_phnum headers, and `total`
            // is the caller's &mut usize, alive for the whole call.
            let (phdrs, total) = unsafe {
                let info = &*info;
                (std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize), &mut *total.cast::<usize>())
            };
            *total += phdrs.iter().filter(|p| p.p_type == libc::PT_TLS).map(|p| p.p_memsz as usize).sum::<usize>();
            0
        }
        let mut tls = 0usize;
        // SAFETY: the callback only reads the program headers and adds to `tls`, which outlives the call.
        unsafe { libc::dl_iterate_phdr(Some(add_tls), (&mut tls as *mut usize).cast()) };
        let need = 2 * (DIST_FRAMES + tls.max(SERVER_TLS));
        assert!(
            WATCHDOG_STACK >= need,
            "watchdog stack {WATCHDOG_STACK} B < 2 x ({DIST_FRAMES} B of frames + {tls} B of static TLS)"
        );
    }

    /// The backstop's revoke rung: a statement with spillable bytes, past the 90 % tier with nothing for the rungs to
    /// reclaim, is asked to spill on the first sample, and that sample cancels as it does with pgrust.memory_revoke off
    /// (D4314 (a): an ask sent is no relief; only a measured drop at the next sample spares a statement).
    #[test]
    fn past_the_threshold_a_revocable_statement_is_asked_to_spill_before_any_cancel() {
        use std::sync::atomic::AtomicU8;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        static BYTE: AtomicU8 = AtomicU8::new(0);
        const GIB: u64 = 1 << 30;
        let (limit, at) = (16 * GIB, |pct: u64| 16 * GIB / 100 * pct);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let stmt = std::thread::spawn(move || {
            mcx::governor::register(43, 7043);
            mcx::revoke::register_byte(&BYTE);
            mcx::governor::statement_start(0, true);
            mcx::revoke::note_spillable((4 * GIB) as i64);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let asked = mcx::revoke::generation();
            let canceled = mcx::governor::take_watchdog_cancel().is_some();
            mcx::governor::statement_end();
            (asked, canceled)
        });
        ready_rx.recv().unwrap();
        mcx::revoke::set_enabled(false);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        assert_eq!(backstop_sample(at(91), limit, 90, 95), Some(true), "revoke off: nothing asked, the first sample cancels");
        assert_eq!(BYTE.load(Ordering::Relaxed) & mcx::revoke::REVOKE_BIT, 0);
        mcx::revoke::set_enabled(true);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        assert_eq!(backstop_sample(at(89), limit, 90, 95), None, "under the threshold");
        let before = mcx::revoke::ASKED_BYTES.load(Ordering::Relaxed);
        assert_eq!(backstop_sample(at(91), limit, 90, 95), Some(true), "asked to spill, and the sample cancels as with revoke off (D4314 (a))");
        assert_ne!(BYTE.load(Ordering::Relaxed) & mcx::revoke::REVOKE_BIT, 0, "the statement's byte carries the revoke bit");
        assert!(mcx::revoke::ASKED_BYTES.load(Ordering::Relaxed) > before);
        assert_eq!(backstop_sample(at(91), limit, 90, 95), Some(true), "still past the next sample: cancel");
        mcx::revoke::set_enabled(false);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        go_tx.send(()).unwrap();
        let (asked, canceled) = stmt.join().unwrap();
        assert!(asked != 0 && !canceled, "the statement read an ask, never a cancel (the sample itself cancels nobody)");
    }

    /// The tests that drive the process-wide revoke switch, the ladder's mode and the cancel threshold, one at a time.
    pub(crate) static GLOBALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// D4886: the pool's R1 landing in `pool_r1_landing`'s place (None: the pool's own).
    pub(crate) static POOL_LANDING: std::sync::Mutex<Option<fn() -> u64>> = std::sync::Mutex::new(None);

    /// D4886 (born red): on a 32 GiB Graviton's 100M-row bulk load (Hearth, a 31483 MB limit) the backstop canceled the
    /// COPY at 28703 MB (91 %) while 12.1 GB of the pool's free runs sat in superpages still in use: the sample that
    /// first crossed the cancel line asked the purger for them (R1) but had read the pool's landing before its own ask,
    /// so nothing held the cancel. Read after the ask, the landing holds it under the top tier until the purger's pass
    /// served the ask; then the read decides. At the top tier nothing holds. R6 holds by the same rule.
    #[test]
    fn d4886_the_r1_ask_of_the_sample_that_crossed_the_line_holds_its_cancel_until_served() {
        use std::sync::atomic::AtomicBool;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const MB: u64 = 1 << 20;
        static SERVED: AtomicBool = AtomicBool::new(false);
        // the purger's ask landing from the sample's own R1 (`rung_return_allocator_memory` marks it) until served
        fn landing() -> u64 {
            if RUNG1_AT_MS.load(Ordering::Relaxed) != u64::MAX && !SERVED.load(Ordering::Relaxed) { 12_100 * MB } else { 0 }
        }
        let (limit, read) = (31_483 * MB, 28_703 * MB);
        let watch0 = guc_tables::backing::pgrust_memory_watchdog_threshold();
        let rung0 = RUNG1_AT_MS.swap(u64::MAX, Ordering::Relaxed);
        *POOL_LANDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(landing);
        guc_tables::backing::set_pgrust_memory_cancel_threshold(90);
        guc_tables::backing::set_pgrust_memory_watchdog_threshold(80);
        mcx::ladder::publish(mcx::ladder::OFF, 0);
        mcx::revoke::set_enabled(false);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        // this server holds the whole cgroup: it acts (no registry read)
        LAST_OWN.store(0, Ordering::Relaxed);
        SERVED.store(false, Ordering::Relaxed);
        POOL_HOLD_LOGGED.store(false, Ordering::Relaxed);
        assert_eq!(backstop_cancels(limit, read, busy), None, "the first sample past the line: its own R1 ask, landing, holds the cancel");
        assert!(POOL_HOLD_LOGGED.load(Ordering::Relaxed), "the held cancel has its line");
        assert_ne!(RUNG1_AT_MS.load(Ordering::Relaxed), u64::MAX, "the sample asked the pool (R1)");
        assert!(pool_r1_holds(read, limit), "R6 waits for the same ask");
        SERVED.store(true, Ordering::Relaxed);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        assert!(backstop_cancels(limit, read, busy).is_some(), "served and the read still past the line: the next sample cancels");
        assert!(!pool_r1_holds(read, limit), "served: R6 too");
        SERVED.store(false, Ordering::Relaxed);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        assert!(backstop_cancels(limit, limit / 100 * 96, busy).is_some(), "the top tier: no landing holds a cancel");
        assert!(!pool_r1_holds(limit / 100 * 96, limit), "the top tier: R6 cancels");
        assert!(!pool_r1_holds(read, 0), "no limit, no hold");
        assert_eq!(backstop_cancels(limit, limit / 100 * 80, busy), None, "under the line");
        assert!(!POOL_HOLD_LOGGED.load(Ordering::Relaxed), "under the line the episode ends: the next hold writes its line again");
        *POOL_LANDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        guc_tables::backing::set_pgrust_memory_cancel_threshold(0);
        guc_tables::backing::set_pgrust_memory_watchdog_threshold(watch0);
        RUNG1_AT_MS.store(rung0, Ordering::Relaxed);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
    }

    /// USER RULING 2026-10-13 17:3xZ (memory Group 1): with pgrust.memory_ladder = on the watchdog measures and logs
    /// only. A sample past its cancel line (91 % of the limit, pgrust.memory_cancel_threshold 90) with a revocable
    /// statement asks it nothing and cancels nothing: the ladder's R4b / R6 decide. With the ladder off the same sample
    /// still runs the backstop (it asks the statement to spill: today's road, unchanged until the flip).
    #[test]
    fn with_the_ladder_on_the_watchdog_past_its_cancel_line_asks_and_cancels_nothing() {
        use std::sync::atomic::AtomicU8;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        static BYTE: AtomicU8 = AtomicU8::new(0);
        const GIB: u64 = 1 << 30;
        let (limit, at) = (16 * GIB, |pct: u64| 16 * GIB / 100 * pct);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let stmt = std::thread::spawn(move || {
            mcx::governor::register(44, 7044);
            mcx::revoke::register_byte(&BYTE);
            mcx::governor::statement_start(0, true);
            mcx::revoke::note_spillable((4 * GIB) as i64);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let canceled = mcx::governor::take_watchdog_cancel().is_some();
            mcx::governor::statement_end();
            canceled
        });
        ready_rx.recv().unwrap();
        guc_tables::backing::set_pgrust_memory_cancel_threshold(90);
        mcx::revoke::set_enabled(true);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        // this server holds the whole cgroup (not a bystander, D4262); the sample test below writes LAST_OWN too
        LAST_OWN.store(0, Ordering::Relaxed);
        mcx::ladder::publish(mcx::ladder::ON, at(85));
        refresh_cancel(limit, at(91));
        refresh_cancel(limit, at(96));
        assert_eq!(BYTE.load(Ordering::Relaxed) & mcx::revoke::REVOKE_BIT, 0, "ladder on: the watchdog asked a statement to spill");
        // the ladder off: the same sample is the backstop's, as today
        mcx::ladder::publish(mcx::ladder::OFF, 0);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        LAST_OWN.store(0, Ordering::Relaxed);
        refresh_cancel(limit, at(91));
        assert_ne!(BYTE.load(Ordering::Relaxed) & mcx::revoke::REVOKE_BIT, 0, "ladder off: the backstop still asks (unchanged)");
        mcx::revoke::set_enabled(false);
        guc_tables::backing::set_pgrust_memory_cancel_threshold(0);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        go_tx.send(()).unwrap();
        assert!(!stmt.join().unwrap(), "no watchdog cancel reached the statement");
    }

    /// D4374 (memtest's D-tip scorecard, cell b, 1536M: R6 canceled a second statement 6 ms after the first, while the
    /// first victim's 960 MB were still giving back; 2 of 14 victims were innocent 40 / 56 MB hash aggregates): once the
    /// victim answered and its statement ended, R6 is still in flight until the ladder's read fell by half the victim's
    /// bytes or the cancel's landing time (`cancel_landing_ms`) passed (LADDER-DESIGN section 4: one cancel in flight).
    /// D4391 (born red): once the victim's bytes landed (the read fell by half of them at a sample), a new grower that
    /// pushes the read back up within the landing time is not the old victim still landing: R6 is free for it. Before,
    /// the hold only compared the read at a climb with the read at the cancel, and held R6 for the whole 2750 ms.
    #[test]
    fn d4391_a_landed_cancel_does_not_hold_r6_for_the_next_grower() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        note_r6(20_000, 1369 * MIB, 960 * MIB, None);
        assert!(!r6_landing_at(20_108, 526 * MIB), "108 ms later the read fell to 526 MB: landed");
        assert!(!r6_landing_at(22_532, 1306 * MIB), "2.5 s later a new grower at 1306 MB: not the old victim landing");
        R6_AT.store(0, Ordering::Relaxed);
    }

    /// Kill safety (a stress arm OOM-killed under reuse): the next hog reused a canceled hog's 468 MB, so the read
    /// neither fell by half the victim nor rose past the read at the cancel by the victim's bytes, and D4374's hold kept R6
    /// off for its whole landing time while the read climbed to a kernel kill. Past the cancel line no landing holds R6.
    #[test]
    fn past_the_cancel_line_a_cancel_still_landing_holds_no_r6() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        note_r6(30_000, 1500 * MIB, 468 * MIB, None);
        assert!(r6_held(30_400, 1700 * MIB, false), "under the cancel line: the bytes reused, the read up 200 MB: held");
        assert!(!r6_held(30_400, 1700 * MIB, true), "past the cancel line: the next pass may cancel");
        R6_AT.store(0, Ordering::Relaxed);
    }

    /// D4374 (`r6_land_past_cancel`; the Hearth ge2e W0 FAIL 10-18 05:53:59Z: R6 canceled the hog, 848 MB, at a read of
    /// 1458 MB, and 13 ms later, the read 1408 MB and still past the cancel line, an innocent 170 MB aggregate while the
    /// hog's bytes drained, -738 MB by 122 ms): past the line, once the victim answered, its landing holds R6 while the
    /// read falls or stays; the read rising (the bytes reused) frees R6 at that test. Not answered: the ack hold's (250 ms
    /// at most), not this one's. Off: past the line no landing holds R6, as before.
    #[test]
    fn d4374_past_the_cancel_line_an_answered_victims_landing_holds_r6_while_the_read_is_not_rising() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        use mempolicy::consts::{set, K};
        note_r6(60_000, 1458 * MIB, 848 * MIB, None);
        note_r6_ack(60_000, None, 0x305);
        assert!(!r6_held(60_005, 1458 * MIB, true), "not answered yet: this hold is not the one in flight");
        r6_answered(60_000);
        assert!(r6_held(60_013, 1408 * MIB, true), "answered, 13 ms later the read fell 50 MB: held");
        assert!(r6_held(60_022, 1255 * MIB, true), "still falling: held");
        assert!(r6_held(60_025, 1255 * MIB, true), "flat: held");
        assert!(!r6_held(60_030, 1300 * MIB, true), "the read rose (the next hog reusing the bytes): R6 free");
        note_r6(61_000, 1458 * MIB, 848 * MIB, None);
        set(K::R6LandPastCancel, 0);
        assert!(!r6_held(61_013, 1408 * MIB, true), "off: past the line no landing holds R6");
        set(K::R6LandPastCancel, 1);
        assert!(r6_held(61_014, 1408 * MIB, false), "under the line: the landing test as before");
        R6_AT.store(0, Ordering::Relaxed);
        R6_RAISED_AT.store(0, Ordering::Relaxed);
    }

    /// Kill safety: past the cancel line a cancel the victim has not answered holds R6 for 250 ms at most, counted once
    /// as an r6_ack_timeout; under the line it holds until answered.
    #[test]
    fn past_the_cancel_line_an_unanswered_cancel_holds_r6_for_250_ms_at_most() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let n = || mcx::ladder_stats::cancels()[mcx::ladder_stats::Cancel::R6AckTimeout as usize];
        note_r6(50_000, 1500 * MIB, 468 * MIB, None);
        note_r6_ack(50_000, None, 0x105);
        let before = n();
        assert!(r6_ack_held(50_100, true), "100 ms after the cancel: in flight");
        assert!(r6_ack_held(50_300, false), "under the cancel line: held until answered");
        assert_eq!(n(), before);
        assert!(!r6_ack_held(50_250, true), "250 ms unanswered past the line: R6 may act again");
        assert!(!r6_ack_held(50_400, true));
        assert_eq!(n(), before + 1, "counted once for that cancel");
        note_r6(51_000, 1500 * MIB, 468 * MIB, None);
        note_r6_ack(51_000, None, 0x205);
        assert!(!r6_ack_held(51_300, true));
        assert_eq!(n(), before + 2, "the next cancel counts again");
        R6_AT.store(0, Ordering::Relaxed);
        R6_RAISED_AT.store(0, Ordering::Relaxed);
    }

    /// Kill safety (9d's catch on R5b): past the cancel line, another road's cancel waiting for its answer (the
    /// watchdog's) beside R6's earlier raise, answered and more than 250 ms old, keeps R6 held and counts no
    /// r6_ack_timeout: the bound reads only R6's own victim's cancel word, and the answered raise is cleared.
    #[test]
    fn another_roads_pending_cancel_never_times_out_r6s_answered_raise() {
        use mcx::governor::PendingAcks;
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let n = || mcx::ladder_stats::cancels()[mcx::ladder_stats::Cancel::R6AckTimeout as usize];
        note_r6(50_000, 1500 * MIB, 468 * MIB, None);
        note_r6_ack(50_000, Some((7, 4007)), 0x105);
        assert_eq!(r6_victim(), Some((50_000, (Some(7), 0x105))), "R6's own cancel, named by its victim's slot and word");
        let before = n();
        let watchdog = PendingAcks { own: false, other: true, other_young: true };
        assert!(r6_acks_hold(Some(watchdog), 50_000, 50_400, true), "the watchdog's cancel in its ack window holds R6");
        assert_eq!(n(), before, "R6's answered raise never times out");
        assert_eq!(R6_RAISED_AT.load(Ordering::Relaxed), 0, "answered: its raise is cleared");
        assert_eq!(r6_victim(), None);
        assert!(r6_acks_hold(Some(watchdog), 0, 50_500, true), "and the watchdog's cancel goes on holding R6");
        assert_eq!(n(), before);
        // R6's own cancel unanswered past the line: bounded at 250 ms, counted once (R5b), whatever another road's
        note_r6_ack(60_000, Some((7, 4007)), 0x205);
        let own = PendingAcks { own: true, other: false, other_young: false };
        assert!(r6_acks_hold(Some(own), 60_000, 60_100, true), "its own cancel 100 ms unanswered: held");
        assert!(!r6_acks_hold(Some(own), 60_000, 60_250, true), "250 ms unanswered past the line: R6 may act again");
        assert_eq!(n(), before + 1, "one timeout for that cancel");
        assert!(r6_acks_hold(Some(PendingAcks { own: true, other: true, other_young: true }), 60_000, 60_300, true), "another road's young cancel still holds R6");
        assert!(!r6_acks_hold(Some(PendingAcks { own: true, other: true, other_young: false }), 60_000, 60_300, true), "no cancel under 250 ms old past the line: R6 may act");
        assert!(r6_acks_hold(Some(own), 60_000, 61_000, false), "under the cancel line: held until answered");
        assert!(!r6_acks_hold(None, 60_000, 61_000, true), "a busy registry past the line holds as R6's own raise: past its bound");
        assert!(r6_acks_hold(None, 60_000, 60_100, true), "and inside it: held");
        assert!(r6_acks_hold(None, 0, 61_000, true), "with no raise of its own: held");
        assert_eq!(n(), before + 1);
        // a raise noted after the scan is never cleared by that scan's answer
        note_r6_ack(70_000, Some((7, 4007)), 0x305);
        assert!(!r6_acks_hold(Some(PendingAcks::default()), 60_000, 70_010, true), "nothing pending");
        assert_eq!(R6_RAISED_AT.load(Ordering::Relaxed), 70_000, "the newer raise is kept");
        R6_AT.store(0, Ordering::Relaxed);
        R6_RAISED_AT.store(0, Ordering::Relaxed);
    }

    /// Kill safety (broker on the R5b delta): past the cancel line ANY unanswered cancel holds R6 for R6_ACK_BOUND_MS at
    /// most from its own raise. A watchdog cancel pending 300 ms, with no R6 raise of its own, no longer holds R6; under
    /// 250 ms, or under the line, it does.
    #[test]
    fn past_the_cancel_line_another_roads_cancel_holds_r6_for_250_ms_at_most() {
        use mcx::governor::PendingAcks;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        R6_RAISED_AT.store(0, Ordering::Relaxed);
        let n = || mcx::ladder_stats::cancels()[mcx::ladder_stats::Cancel::R6AckTimeout as usize];
        let before = n();
        let (aged, young) = (PendingAcks { own: false, other: true, other_young: false }, PendingAcks { own: false, other: true, other_young: true });
        assert!(!r6_acks_hold(Some(aged), 0, 80_300, true), "a watchdog cancel pending 300 ms past the line: R6 not held");
        assert!(r6_acks_hold(Some(young), 0, 80_100, true), "one pending under 250 ms: held");
        assert!(r6_acks_hold(Some(aged), 0, 80_300, false), "under the cancel line: held until answered");
        assert_eq!(n(), before, "another road's cancel never counts R6's timeout");
    }

    #[test]
    fn r6_stays_in_flight_while_the_victims_bytes_land() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let landing = mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs);
        // the victim answered (no statement is registered, so no cancel waits for its ack) but its bytes are still there
        note_r6(10_000, 1500 * MIB, 960 * MIB, None);
        assert!(r6_landing_at(10_006, 1490 * MIB), "6 ms later the victim's bytes have not left: no second cancel");
        assert!(!r6_landing_at(10_006, 1000 * MIB), "the read fell by half the victim's bytes: landed");
        assert!(!r6_landing_at(10_000 + landing, 1490 * MIB), "past its landing time the next cancel may go");
        R6_AT.store(0, Ordering::Relaxed);
        assert!(!r6_landing_at(10_006, 1490 * MIB), "no R6 yet: nothing in flight");
    }

    /// The landing judged by attribution (born red). 1536M ladder wiring witness with the purger owning the arenas and
    /// reuse before release on: R6 canceled a 600 MB holder at a read of 1351 MB. Its statement freed the bytes at once,
    /// but they stayed committed (free, reusable) until the purger's next ticks, and the next grower took part of them
    /// again, so the read fell only to 1292 MB, then 1136 MB: never by half the victim's bytes. The read-only test held R6
    /// for the whole landing time while that grower took the read from 1136 to 1534 MB, and the kernel killed the server.
    /// A freed byte is no longer the victim's: once its account let go the cancel has landed, whatever the read does.
    #[test]
    fn a_victim_whose_account_let_go_has_landed_though_the_read_did_not_fall() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let (procno, pid) = (4011u32, 4011i32);
        memledger::bind_backend(procno, pid as u32);
        memledger::statement_begin();
        memledger::door_heap((600 * MIB) as isize);
        memledger::fold();
        note_r6(20_811, 1351 * MIB, 600 * MIB, Some((procno as usize, pid)));
        assert!(r6_landing_at(20_812, 1351 * MIB), "the victim still holds its 600 MB: R6 stays in flight");
        // the canceled statement frees its bytes: they leave its account, not (yet) the read
        memledger::door_heap(-((600 * MIB) as isize));
        memledger::fold();
        assert!(r6_landing_at(20_815, 1292 * MIB), "its account let go; its pages are still on their way out");
        assert!(!r6_landing_at(20_915, 1266 * MIB), "two purger periods after the let-go: landed, though the read fell only 85 MB");
        assert!(!r6_landing_at(21_290, 1336 * MIB), "the next grower is not the victim landing: R6 may pick it");
        // a victim that holds its bytes keeps the hold until it lets go or its statement ends
        memledger::door_heap((500 * MIB) as isize);
        memledger::fold();
        note_r6(30_000, 1400 * MIB, 500 * MIB, Some((procno as usize, pid)));
        assert!(r6_landing_at(30_010, 1390 * MIB), "the victim still holds its 500 MB");
        let _ = memledger::statement_end();
        assert!(r6_landing_at(30_020, 1390 * MIB), "its statement ended (what it kept is its session's); its pages on their way");
        assert!(!r6_landing_at(30_120, 1390 * MIB), "two purger periods on: landed");
        memledger::door_heap(-((500 * MIB) as isize));
        memledger::fold();
        memledger::unbind_backend();
        R6_AT.store(0, Ordering::Relaxed);
    }

    static FAKE_REUSABLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn fake_reusable() -> u64 {
        FAKE_REUSABLE.load(Ordering::Relaxed)
    }

    /// With no account to judge by (no backend, or a victim ranked by bytes its account does not hold), the read judges
    /// the landing, and bytes freed to the allocator and kept committed (reusable) count as fallen.
    #[test]
    fn with_no_victim_account_the_read_less_the_reusable_bytes_judges_the_landing() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        mcx::ladder::set_reusable_probe(fake_reusable);
        FAKE_REUSABLE.store(4 * MIB, Ordering::Relaxed);
        note_r6(20_811, 1351 * MIB, 600 * MIB, None);
        assert!(r6_landing_at(20_812, 1351 * MIB), "nothing freed yet");
        FAKE_REUSABLE.store(100 * MIB, Ordering::Relaxed);
        assert!(r6_landing_at(20_813, 1351 * MIB), "96 MB freed and kept: not half the victim's bytes");
        FAKE_REUSABLE.store(619 * MIB, Ordering::Relaxed);
        assert!(r6_landing_at(20_815, 1292 * MIB), "615 MB freed and kept: let go, its pages on their way out");
        assert!(!r6_landing_at(20_915, 1292 * MIB), "two purger periods on: landed, though the whole read fell 59 MB");
        FAKE_REUSABLE.store(0, Ordering::Relaxed);
        note_r6(30_000, 1351 * MIB, 600 * MIB, Some((4012, 4012)));
        assert!(r6_landing_at(30_010, 1292 * MIB), "a backend with no account bound: the read judges, as before");
        assert!(!r6_landing_at(30_010, 1000 * MIB), "the whole read fell by half the victim's bytes");
        R6_AT.store(0, Ordering::Relaxed);
    }

    /// Under a purger that owns the purge, a victim that let go has landed once one whole purger tick begun after the
    /// let-go finished (its freed pages then left the read), or, with a purger that never ticks, after two of its periods.
    #[test]
    fn under_an_owning_purger_the_let_go_lands_after_its_tick_or_two_periods() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        purge::TEST_OWNS.store(true, Ordering::Relaxed);
        let (procno, pid) = (4013u32, 4013i32);
        memledger::bind_backend(procno, pid as u32);
        memledger::statement_begin();
        let (_, _, wait0, bounded0) = r6_hold_counts();
        // the purger ticks: the let-go waits for one whole tick begun after it
        memledger::door_heap((600 * MIB) as isize);
        memledger::fold();
        note_r6(20_811, 1351 * MIB, 600 * MIB, Some((procno as usize, pid)));
        memledger::door_heap(-((600 * MIB) as isize));
        memledger::fold();
        purge::test_purging(true);
        assert!(r6_landing_at(20_815, 1292 * MIB), "let go, but its pages wait for the purger's pass");
        purge::test_tick();
        purge::test_purging(false);
        assert!(r6_landing_at(20_840, 1292 * MIB), "the pass running at the let-go finished: it may have begun before the free");
        purge::test_tick();
        assert!(!r6_landing_at(20_870, 1292 * MIB), "a pass begun after the let-go finished: landed, the read as it was");
        assert_eq!(r6_hold_counts().2 - wait0, 55, "the wait, from the let-go to its end");
        assert_eq!(r6_hold_counts().3, bounded0, "the tick ended it, not the bound");
        // a purger that never ticks: the bound ends the wait two periods after the let-go
        memledger::door_heap((500 * MIB) as isize);
        memledger::fold();
        note_r6(30_000, 1400 * MIB, 500 * MIB, Some((procno as usize, pid)));
        memledger::door_heap(-((500 * MIB) as isize));
        memledger::fold();
        let period = purge::TICK_MS;
        assert!(r6_landing_at(30_010, 1390 * MIB), "let go: waiting for the purger");
        assert!(r6_landing_at(30_010 + 2 * period - 1, 1390 * MIB), "under two periods: still waiting");
        assert!(!r6_landing_at(30_010 + 2 * period, 1390 * MIB), "two periods with no tick: landed by the bound");
        assert_eq!(r6_hold_counts().3, bounded0 + 1, "the bound ended this wait");
        let _ = memledger::statement_end();
        memledger::unbind_backend();
        purge::TEST_OWNS.store(false, Ordering::Relaxed);
        R6_AT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn the_landing_hold_counts_held_passes_and_those_past_the_cancel_line() {
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        let (ms0, past0, _, _) = r6_hold_counts();
        R6_HOLD_PASS_MS.store(0, Ordering::Relaxed);
        note_r6_hold(1_000, true, false, false);
        assert_eq!(r6_hold_counts().0 - ms0, 0, "the first pass has no previous one to time from");
        assert_eq!(r6_hold_counts().1, past0);
        note_r6_hold(1_100, true, false, true);
        note_r6_hold(1_200, false, false, true);
        note_r6_hold(1_250, true, false, false);
        assert_eq!((r6_hold_counts().0, r6_hold_counts().1), (ms0 + 150, past0 + 1));
        // the pass's cancel-line flag up: R6 is not held (`r6_held`), so neither the time nor the pass counts
        note_r6_hold(1_300, true, true, true);
        assert_eq!((r6_hold_counts().0, r6_hold_counts().1), (ms0 + 150, past0 + 1));
    }

    /// R5: past the watchdog's cancel line no landing holds R6 off, the let-go's wait on the purger's pass included.
    #[test]
    fn past_the_cancel_line_the_let_gos_wait_on_the_purger_holds_nothing() {
        const MIB: u64 = 1 << 20;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        purge::TEST_OWNS.store(true, Ordering::Relaxed);
        let (procno, pid) = (4014u32, 4014i32);
        memledger::bind_backend(procno, pid as u32);
        memledger::statement_begin();
        memledger::door_heap((600 * MIB) as isize);
        memledger::fold();
        note_r6(40_000, 1400 * MIB, 600 * MIB, Some((procno as usize, pid)));
        memledger::door_heap(-((600 * MIB) as isize));
        memledger::fold();
        assert!(r6_held(40_005, 1400 * MIB, false), "let go, its pages wait for the purger's pass: R6 held");
        mempolicy::consts::set(mempolicy::consts::K::R6LandPastCancel, 0);
        assert!(!r6_held(40_005, 1400 * MIB, true), "r6_land_past_cancel 0, the cancel-line flag up: the wait holds nothing");
        mempolicy::consts::set(mempolicy::consts::K::R6LandPastCancel, 1);
        // D4374: on, the victim answered and the read flat past the line: its landing holds; a rise ends it
        assert!(r6_held(40_006, 1400 * MIB, true), "r6_land_past_cancel 1, the read not rising: held");
        assert!(!r6_held(40_007, 1401 * MIB, true), "the read rose: R6 free");
        let _ = memledger::statement_end();
        memledger::unbind_backend();
        purge::TEST_OWNS.store(false, Ordering::Relaxed);
        R6_AT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn proc_status_parses_on_linux_shape() {
        // On Linux this reads the live file; elsewhere it returns None.
        if cfg!(target_os = "linux") {
            let m = proc_status().expect("/proc/self/status readable on Linux");
            assert!(m.rss_kb > 0);
        } else {
            assert!(proc_status().is_none() || proc_status().is_some());
        }
    }

    #[test]
    fn physical_memory_is_the_fallback_limit() {
        let total = physical_memory().expect("physical RAM readable on this platform");
        assert!(total >= 256 << 20, "implausible RAM size {total}");
        let l = collect(&memory_limit(0));
        // D3314: a bounded memory.max on the leaf or any ancestor is the limit (never past RAM); else RAM
        if l.source == LimitSource::Physical {
            assert_eq!(l.limit, total);
        } else {
            assert_eq!(l.source, LimitSource::Cgroup);
            assert!(l.limit <= total);
        }
    }

    /// A scratch cgroup v2 tree: `files` are (relative path, contents).
    fn fake_tree(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let root = std::env::temp_dir().join(format!("memwatchdog-{tag}-{}-{nanos}", std::process::id()));
        for (rel, text) in files {
            let f = root.join(rel);
            std::fs::create_dir_all(f.parent().expect("a file under the root")).expect("scratch dir");
            std::fs::write(&f, text).expect("scratch file");
        }
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    /// D3314: the limit is the smallest bounded memory.max over the process's cgroup and its ancestors (a systemd unit's
    /// MemoryMax on the unit above a leaf without one), capped at physical memory; the GUC wins; no cgroup: RAM.
    #[test]
    fn d3314_memory_limit_is_the_smallest_memory_max_over_the_cgroup_and_its_ancestors() {
        const G: u64 = 1 << 30;
        let root = fake_tree(
            "limit",
            &[
                ("system.slice/memory.max", "max\n"),
                ("system.slice/pg.service/memory.max", "17179869184\n"),
                ("system.slice/pg.service/leaf/memory.max", "max\n"),
                ("system.slice/pg.service/leaf/inner/memory.max", "34359738368\n"),
            ],
        );
        let l = memory_limit_in(&root, "0::/system.slice/pg.service/leaf/inner\n", 0, Some(61 * G));
        assert_eq!((l.bytes, l.source), (16 * G, LimitSource::Cgroup));
        assert_eq!(l.cgroup.as_deref(), Some(root.join("system.slice/pg.service").as_path()));
        // the leaf alone unbounded, the unit bounded: the unit's
        let l = memory_limit_in(&root, "0::/system.slice/pg.service/leaf\n", 0, Some(61 * G));
        assert_eq!(l.bytes, 16 * G);
        // never past physical memory
        assert_eq!(memory_limit_in(&root, "0::/system.slice/pg.service\n", 0, Some(8 * G)).bytes, 8 * G);
        // the GUC wins
        let l = memory_limit_in(&root, "0::/system.slice/pg.service\n", 4096, Some(61 * G));
        assert_eq!((l.bytes, l.source, l.cgroup), (4 * G, LimitSource::Guc, None));
        // every level unbounded, or no cgroup: physical memory
        let l = memory_limit_in(&root, "0::/system.slice\n", 0, Some(61 * G));
        assert_eq!((l.bytes, l.source), (61 * G, LimitSource::Physical));
        assert_eq!(memory_limit_in(&root, "", 0, Some(61 * G)).source, LimitSource::Physical);
        assert_eq!(memory_limit_in(&root, "", 0, None), MemLimit::default());
        // inside a cgroup namespace the container's limit sits on the mount's root
        let ns = fake_tree("ns", &[("memory.max", "8589934592\n")]);
        assert_eq!(memory_limit_in(&ns, "0::/\n", 0, Some(61 * G)).bytes, 8 * G);
        // the pod shape (D3316): the leaf unbounded, the pod slice bounded
        let pod = fake_tree(
            "pod",
            &[
                ("kubepods.slice/kubepods-pod1.slice/memory.max", "8589934592\n"),
                ("kubepods.slice/kubepods-pod1.slice/cri-1.scope/memory.max", "max\n"),
            ],
        );
        let l = memory_limit_in(&pod, "0::/kubepods.slice/kubepods-pod1.slice/cri-1.scope\n", 0, Some(61 * G));
        assert_eq!((l.bytes, l.source), (8 * G, LimitSource::Cgroup));
        // v1 (the memory controller on a named hierarchy) wins over the unified line; unlimited is a huge number
        let v1 = fake_tree(
            "v1",
            &[("memory/pod/memory.limit_in_bytes", "4294967296\n"), ("memory/pod/c/memory.limit_in_bytes", "9223372036854771712\n")],
        );
        let l = memory_limit_in(&v1, "12:memory:/pod/c\n0::/x\n", 0, Some(61 * G));
        assert_eq!((l.bytes, l.cgroup.as_deref()), (4 * G, Some(v1.join("memory/pod").as_path())));
        for d in [&root, &ns, &pod, &v1] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// D4290: the kernel throttles a cgroup past its memory.high, so the limit is the smaller of the smallest bounded
    /// memory.max and the smallest bounded memory.high over the cgroup and its ancestors (the precheck slots' MemoryHigh
    /// 14717 MiB under MemoryMax 18396 MiB armed the server at 18396 MiB, and it stalled in reclaim at 14717).
    #[test]
    fn d4290_the_limit_is_memory_high_when_it_sits_below_memory_max() {
        const G: u64 = 1 << 30;
        const M: u64 = 1 << 20;
        let root = fake_tree(
            "high",
            &[
                ("system.slice/memory.max", "max\n"),
                ("system.slice/memory.high", "max\n"),
                ("system.slice/slot.scope/memory.max", &(18396 * M).to_string()),
                ("system.slice/slot.scope/memory.high", &(14717 * M).to_string()),
                ("system.slice/slot.scope/leaf/memory.max", "max\n"),
                ("system.slice/slot.scope/leaf/memory.high", "max\n"),
            ],
        );
        let l = memory_limit_in(&root, "0::/system.slice/slot.scope/leaf\n", 0, Some(61 * G));
        assert_eq!((l.bytes >> 20, l.source), (14717, LimitSource::Cgroup), "memory.high, not memory.max");
        assert_eq!(l.cgroup.as_deref(), Some(root.join("system.slice/slot.scope").as_path()));
        // never past physical memory
        assert_eq!(memory_limit_in(&root, "0::/system.slice/slot.scope\n", 0, Some(8 * G)).bytes, 8 * G);
        // the GUC still wins
        assert_eq!(memory_limit_in(&root, "0::/system.slice/slot.scope\n", 4096, Some(61 * G)).bytes, 4 * G);
        let other = fake_tree(
            "high2",
            &[
                // memory.max unlimited, memory.high set: memory.high
                ("un/memory.max", "max\n"),
                ("un/memory.high", "8589934592\n"),
                // equal: the one value
                ("eq/memory.max", "8589934592\n"),
                ("eq/memory.high", "8589934592\n"),
                // memory.high above memory.max: memory.max
                ("above/memory.max", "8589934592\n"),
                ("above/memory.high", "17179869184\n"),
                // memory.high on the unit, memory.max on the leaf below it
                ("unit/memory.high", "4294967296\n"),
                ("unit/memory.max", "max\n"),
                ("unit/leaf/memory.max", "8589934592\n"),
                ("unit/leaf/memory.high", "max\n"),
            ],
        );
        let at = |cg: &str| memory_limit_in(&other, &format!("0::/{cg}\n"), 0, Some(61 * G));
        assert_eq!((at("un").bytes, at("un").source), (8 * G, LimitSource::Cgroup));
        assert_eq!(at("eq").bytes, 8 * G);
        assert_eq!(at("above").bytes, 8 * G);
        let l = at("unit/leaf");
        assert_eq!((l.bytes, l.cgroup.as_deref()), (4 * G, Some(other.join("unit").as_path())));
        for d in [&root, &other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// D4290 (user): a soft limit (memory.high below memory.max, or memory.max unlimited) is a WARNING at start naming both
    /// values and the budget sized to, with a HINT; at run time a rise of its cgroup's memory.events 'high' count (the
    /// kernel throttling the server) is a WARNING at most once a minute. Equal limits warn neither.
    #[test]
    fn d4290_a_soft_limit_is_warned_at_start_and_its_throttling_at_run_time() {
        const G: u64 = 1 << 30;
        const M: u64 = 1 << 20;
        let events = |high: u64| format!("low 0\nhigh {high}\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n");
        let root = fake_tree(
            "soft",
            &[
                ("slot/memory.max", &(18396 * M).to_string()),
                ("slot/memory.high", &(14717 * M).to_string()),
                ("slot/memory.events", &events(6457)),
                ("un/memory.max", "max\n"),
                ("un/memory.high", "8589934592\n"),
                ("eq/memory.max", "8589934592\n"),
                ("eq/memory.high", "8589934592\n"),
                ("eq/memory.events", &events(5)),
                ("unset/memory.max", "8589934592\n"),
                ("unset/memory.high", "max\n"),
            ],
        );
        let at = |cg: &str| memory_limit_in(&root, &format!("0::/{cg}\n"), 0, Some(61 * G));
        // start: both values and the budget, and the hint
        let l = at("slot");
        assert_eq!(l.knob, "memory.high");
        let soft = l.soft.clone().expect("memory.high below memory.max is a soft limit");
        assert_eq!((soft.high >> 20, soft.max.map(|m| m >> 20), soft.dir.clone()), (14717, Some(18396), root.join("slot")));
        let (msg, hint) = soft_limit_warning(&soft, l.bytes);
        assert!(msg.contains("memory.high (14717 MiB)") && msg.contains("memory.max (18396 MiB)"), "{msg}");
        assert!(msg.contains("budget is sized to 14717 MiB"), "{msg}");
        assert!(hint.contains("Set MemoryHigh equal to MemoryMax") && hint.contains("leave it unset"), "{hint}");
        // memory.max unlimited
        let un = at("un");
        let (msg, _) = soft_limit_warning(un.soft.as_ref().expect("memory.max unlimited, memory.high set"), un.bytes);
        assert!(msg.contains("memory.high (8192 MiB)") && msg.contains("memory.max (unlimited)") && msg.contains("sized to 8192 MiB"), "{msg}");
        // the GUC's budget is named, the soft limit still warned
        let g = memory_limit_in(&root, "0::/slot\n", 4096, Some(61 * G));
        assert!(soft_limit_warning(g.soft.as_ref().expect("warned under the GUC too"), g.bytes).0.contains("sized to 4096 MiB"));
        // equal limits, or memory.high unset: none
        assert_eq!((at("eq").soft, at("unset").soft), (None, None));
        assert_eq!(at("eq").knob, "memory.max");
        // run time: the first reading is the baseline; a rise warns; another within the minute waits and is summed
        let mut w = HighWatch::default();
        assert_eq!(high_tick(&mut w, &l, 1_000), None, "the baseline");
        assert_eq!(high_tick(&mut w, &l, 2_000), None, "no rise");
        std::fs::write(root.join("slot/memory.events"), events(6460)).expect("events");
        let (msg, hint) = high_tick(&mut w, &l, 3_000).expect("a rise is warned");
        assert!(msg.contains("memory.high (14717 MiB) 3 times") && msg.contains("memory.events high"), "{msg}");
        assert!(hint.contains("Set MemoryHigh equal to MemoryMax"), "{hint}");
        std::fs::write(root.join("slot/memory.events"), events(6465)).expect("events");
        assert_eq!(high_tick(&mut w, &l, 4_000), None, "rate-limited");
        std::fs::write(root.join("slot/memory.events"), events(6470)).expect("events");
        assert_eq!(high_tick(&mut w, &l, 62_999), None, "still within the minute");
        let (msg, _) = high_tick(&mut w, &l, 63_000).expect("a minute later");
        assert!(msg.contains(" 10 times"), "the rises in between summed: {msg}");
        assert_eq!(high_tick(&mut w, &l, 200_000), None, "flat: nothing");
        // equal limits: no watch, whatever the events
        let eq = at("eq");
        let mut w = HighWatch::default();
        assert_eq!(high_tick(&mut w, &eq, 1_000), None);
        std::fs::write(root.join("eq/memory.events"), events(500)).expect("events");
        assert_eq!(high_tick(&mut w, &eq, 100_000), None, "equal limits warn nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D3314: under a cgroup limit the governor counts what the kernel cannot reclaim: memory.current less the clean page
    /// cache (dirty and writeback pages count), and never less than the process's own anon + shmem.
    #[test]
    fn d3314_governed_resident_counts_the_cgroup_less_its_clean_page_cache() {
        const M: u64 = 1 << 20;
        let stat = "anon 100\nfile 999\nactive_file 3145728000\ninactive_file 1048576000\nfile_dirty 104857600\nfile_writeback 0\nfile_mapped 7\n";
        assert_eq!(reclaimable_file(stat), 3145728000 + 1048576000 - 104857600);
        assert_eq!(reclaimable_file("file 5\nfile_dirty 9\n"), 0);
        let root = fake_tree(
            "resident",
            &[("u/memory.current", &format!("{}\n", 16_000 * M)), ("u/memory.stat", stat)],
        );
        let l = MemLimit { bytes: 16 << 30, source: LimitSource::Cgroup, cgroup: Some(root.join("u")), ..MemLimit::default() };
        let unreclaimable = 16_000 * M - reclaimable_file(stat);
        assert_eq!(cgroup_unreclaimable(&root.join("u")), Some(unreclaimable));
        let small = ProcMem { anon_kb: 1024, shmem_kb: 1024, ..ProcMem::default() };
        assert_eq!(governed_resident(&small, &l), unreclaimable);
        let big = ProcMem { anon_kb: 15 << 20, shmem_kb: 0, ..ProcMem::default() };
        assert_eq!(governed_resident(&big, &l), 15 << 30);
        // not a cgroup limit: anon + shmem only
        let physical = MemLimit { bytes: 61 << 30, source: LimitSource::Physical, cgroup: None, ..MemLimit::default() };
        assert_eq!(governed_resident(&small, &physical), 2 * M);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D4262: the trigger stays the cgroup's (the kernel's OOM decision counts every process in it); this server's own view
    /// (its anon + shmem and the cgroup's unmapped bytes, not another process's) decides who acts. A server holding 200 MiB
    /// of a slot cgroup 13,200 MiB full of other e2e servers' memory is past the 90 % line and a bystander: one line, no
    /// cancel (box 73: a 53 MB statement canceled with 53200). Alone in its cgroup it acts as before.
    #[test]
    fn d4262_the_trigger_stays_the_cgroup_s_and_a_bystander_does_not_act() {
        // D4417: the bystander statics (its line, LAST_OWN, the rungs) and the registry, one such test at a time
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const M: u64 = 1 << 20;
        let shared = format!(
            "anon {}\nshmem {}\nfile {}\nactive_file {}\ninactive_file {}\nfile_dirty {}\nfile_writeback 0\nfile_mapped 0\n",
            13_150 * M,
            250 * M,
            500 * M,
            300 * M,
            200 * M,
            50 * M
        );
        let root = fake_tree("d4262", &[("s/memory.current", &format!("{}\n", 14_000 * M)), ("s/memory.stat", &shared)]);
        let limit = 14_717 * M;
        let own = ProcMem { anon_kb: 150 << 10, shmem_kb: 50 << 10, ..ProcMem::default() };
        let l = MemLimit { bytes: limit, source: LimitSource::Cgroup, cgroup: Some(root.join("s")), ..MemLimit::default() };
        let resident = governed_resident(&own, &l);
        assert_eq!(resident, 14_000 * M - 450 * M, "the trigger: the cgroup less its clean page cache, as before");
        let view = own_view_in(200 * M, 14_000 * M, shared.as_bytes());
        assert_eq!(view, 200 * M + 150 * M, "own view: this server's 200 MiB and the cgroup's unmapped 150 MiB");
        assert_eq!(own_view_in(200 * M, 600 * M, b"anon 209715200\nactive_file 314572800\n"), 600 * M - 300 * M, "alone: the cgroup less its clean cache");
        let (past, _) = mempolicy::watchdog::cancel_at(resident, limit, 90).expect("past the 90 % line");
        assert!(decided(|| bystander(resident, view, limit, past, mcx::governor::try_largest_live)), "200 MiB of 13,550: a bystander, nothing canceled");
        assert!(BYSTANDER_LOGGED.load(Ordering::Relaxed), "its one line this episode");
        assert_eq!(bystander(resident, resident / 2, limit, past, busy), Some(false), "half the cgroup: the tenant acts, no registry read");
        assert_eq!(bystander(resident, 0, limit, past, busy), Some(false), "no own view yet: acts as before");
        BYSTANDER_LOGGED.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// BYSTRIM (main 2026-10-12T22:29:54Z): a bystander past the line still runs the reclaim rung on its OWN idle memory
    /// (parked regions, cache trims): over the line that has no victim and lowers the risk of a kernel kill (box 45 arm
    /// (i): trunk's backstop trimmed ~180 MB of parked regions there, the D4262 cut kept them). It asks and cancels nothing.
    #[test]
    fn bystrim_a_bystander_still_trims_its_own_parked_memory() {
        // D4417: the bystander statics (its line, LAST_OWN, the rungs) and the registry, one such test at a time
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const M: u64 = 1 << 20;
        std::thread_local! {
            static MINE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
            static WANTED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
        }
        // the rungs answer this test's thread only: another test's backstop sample reads none, as with no rungs installed
        fn bytes() -> u64 {
            if MINE.with(core::cell::Cell::get) { 300 * M } else { 0 }
        }
        fn reclaim(want: u64) -> u64 {
            if !MINE.with(core::cell::Cell::get) {
                return 0;
            }
            WANTED.with(|w| w.set(w.get() + want));
            want
        }
        set_backstop_rungs(bytes, reclaim);
        MINE.with(|m| m.set(true));
        let limit = 14_717 * M;
        let resident = 13_550 * M;
        let (past, _) = mempolicy::watchdog::cancel_at(resident, limit, 90).expect("past the 90 % line");
        assert!(decided(|| bystander(resident, 350 * M, limit, past, mcx::governor::try_largest_live)), "350 MiB of 13,550 MiB, no statement: a bystander");
        assert_eq!(WANTED.with(core::cell::Cell::get), 300 * M, "its own parked memory and caches trimmed: all 300 MiB, the excess is larger");
        MINE.with(|m| m.set(false));
    }

    /// D4417: `bystander`'s answer, its unknown (a held pins table or registry) retried as the next sample retries it.
    fn decided(f: impl Fn() -> Option<bool>) -> bool {
        loop {
            if let Some(b) = f() {
                return b;
            }
            std::thread::yield_now();
        }
    }

    /// D4417: the largest live statement's reader with the pins table or the registry held.
    fn busy() -> Result<usize, mcx::governor::Busy> {
        Err(mcx::governor::Busy)
    }

    /// D4417 (born red; trunk's bystrim flake, 1 in 20,000 runs): past the cancel line, a server holding 350 MiB of a
    /// 13,550 MiB cgroup is a bystander (D4262), but a sample whose `try_largest_live` found the pins table or the
    /// registry held read "not a bystander" and ran the backstop: it asked its statements to spill and could cancel its
    /// largest, which cannot relieve the overage. A held lock is no answer: that sample asks and cancels nothing, and the
    /// next sample decides. A server holding a quarter of the cgroup acts without reading the registry.
    #[test]
    fn d4417_a_held_registry_is_no_answer_the_next_sample_decides() {
        use std::sync::atomic::AtomicU8;
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        static BYTE: AtomicU8 = AtomicU8::new(0);
        const M: u64 = 1 << 20;
        let (limit, resident) = (14_717 * M, 13_550 * M);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let stmt = std::thread::spawn(move || {
            mcx::governor::register(45, 7045);
            mcx::revoke::register_byte(&BYTE);
            mcx::governor::statement_start(0, true);
            mcx::revoke::note_spillable((4096 * M) as i64);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            mcx::governor::statement_end();
        });
        ready_rx.recv().unwrap();
        guc_tables::backing::set_pgrust_memory_cancel_threshold(90);
        mcx::ladder::publish(mcx::ladder::OFF, 0);
        mcx::revoke::set_enabled(true);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        BYSTANDER_LOGGED.store(false, Ordering::Relaxed);
        let asked = || BYTE.load(Ordering::Relaxed) & mcx::revoke::REVOKE_BIT != 0;
        // a bystander's sample with the registry held: nothing asked, no line, nothing decided
        LAST_OWN.store(350 * M, Ordering::Relaxed);
        refresh_cancel_by(limit, resident, busy);
        assert!(!asked(), "a held registry: the bystander's sample asked its statement to spill");
        assert!(!BYSTANDER_LOGGED.load(Ordering::Relaxed), "a held registry decides nothing: no bystander line yet");
        assert_eq!(bystander(resident, 350 * M, limit, 90, busy), None, "a held registry: unknown, not 'acts'");
        assert_eq!(bystander(resident, resident / 2, limit, 90, busy), Some(false), "the tenant: acts, no registry read");
        // the next sample reads the registry (retried while another test holds it): a bystander, one line, nothing asked
        for _ in 0..10_000 {
            refresh_cancel(limit, resident);
            if BYSTANDER_LOGGED.load(Ordering::Relaxed) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(BYSTANDER_LOGGED.load(Ordering::Relaxed), "a later sample decides: a bystander, its one line");
        assert!(!asked(), "a bystander asks nothing");
        // the tenant (half the cgroup) acts with the registry held: its share decides without the registry
        BYSTANDER_LOGGED.store(false, Ordering::Relaxed);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        LAST_OWN.store(resident / 2, Ordering::Relaxed);
        refresh_cancel_by(limit, resident, busy);
        assert!(asked(), "the tenant's sample asks its statement to spill, the registry held or not");
        mcx::revoke::set_enabled(false);
        guc_tables::backing::set_pgrust_memory_cancel_threshold(0);
        BACKSTOP_TRIMMED.store(false, Ordering::Relaxed);
        BYSTANDER_LOGGED.store(false, Ordering::Relaxed);
        LAST_OWN.store(0, Ordering::Relaxed);
        go_tx.send(()).unwrap();
        stmt.join().unwrap();
    }

    /// D3808: the dirty + writeback bytes a bulk load's gate leaves to the write budget are exactly the page cache the
    /// governed resident counts beyond the clean part: file_dirty + file_writeback.
    #[test]
    fn d3808_dirty_file_is_the_dirty_and_writeback_page_cache() {
        let stat = "anon 100\nactive_file 3145728000\ninactive_file 1048576000\nfile_dirty 104857600\nfile_writeback 52428800\n";
        assert_eq!(dirty_file(stat), 104857600 + 52428800);
        assert_eq!(dirty_file("anon 5\n"), 0);
        assert_eq!(dirty_file("file_dirty_ratio 7\nfile_dirty 3\n"), 3);
    }

    std::thread_local! {
        static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Counts the allocations a thread makes while its COUNTING is set.
    struct Counting;

    // SAFETY: a pass-through to System plus a thread-local counter.
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            if COUNTING.with(std::cell::Cell::get) {
                ALLOCS.with(|a| a.set(a.get() + 1));
            }
            // SAFETY: forwarded verbatim.
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: forwarded verbatim.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static COUNTED: Counting = Counting;

    /// `f`'s result and the allocations this thread made while it ran.
    pub(crate) fn counted<R>(f: impl FnOnce() -> R) -> (R, usize) {
        let before = ALLOCS.with(std::cell::Cell::get);
        COUNTING.with(|c| c.set(true));
        let out = f();
        COUNTING.with(|c| c.set(false));
        (out, ALLOCS.with(std::cell::Cell::get) - before)
    }

    /// D4388: the limit moved off the cgroup (pgrust.memory_watchdog_limit set under a cgroup limit): the sample no longer
    /// reads memory.current, and the ladder's read is the governed resident, not the cgroup's last figure (a cgroup-wide
    /// 16,000 MiB beside a 300 MB limit read the server as a bystander: R6 canceled late or never).
    #[cfg(target_os = "linux")]
    #[test]
    fn d4388_off_the_cgroup_the_ladder_reads_the_governed_resident_not_the_cgroups_last_figure() {
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const M: u64 = 1 << 20;
        let stat = "anon 100\nfile 0\nactive_file 0\ninactive_file 0\nfile_mapped 0\nfile_dirty 0\nfile_writeback 0\n";
        let root = fake_tree("d4388", &[("u/memory.current", &format!("{}\n", 16_000 * M)), ("u/memory.stat", stat), ("u/memory.events", "max 0\n")]);
        let dir = root.join("u");
        let mut files = Box::new(SampleFiles::new());
        let mut buf: Box<SampleBuf> = Box::new([0; SAMPLE_BUF]);
        files.open(Some(&dir));
        let _ = files.resident(&mut buf);
        assert_eq!(ladder_read_of(0), 16_000 * M, "under the cgroup limit: the cgroup's read");
        files.open(None);
        let own = files.resident(&mut buf).expect("the process's own resident");
        assert_eq!(ladder_read_of(own), own, "off the cgroup: the governed resident, not the cgroup's last 16,000 MiB");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D4029 (d): the 100 ms sample reads its open files by pread into its fixed buffer: the governed resident of the
    /// reading path, with no allocation.
    #[cfg(target_os = "linux")]
    #[test]
    fn d4029_the_sample_reads_its_open_files_and_allocates_nothing() {
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const M: u64 = 1 << 20;
        let stat = "anon 100\nfile 4194304000\nactive_file 3145728000\ninactive_file 1048576000\nfile_mapped 7340032\nfile_dirty 104857600\nfile_writeback 0\n";
        let root = fake_tree(
            "sample",
            &[("u/memory.current", &format!("{}\n", 16_000 * M)), ("u/memory.stat", stat), ("u/memory.events", "low 0\nhigh 0\nmax 27485\noom 2\n")],
        );
        let dir = root.join("u");
        let mut files = Box::new(SampleFiles::new());
        let mut buf: Box<SampleBuf> = Box::new([0; SAMPLE_BUF]);
        files.open(Some(&dir));
        let l = MemLimit { bytes: 16 << 30, source: LimitSource::Cgroup, cgroup: Some(dir.clone()), ..MemLimit::default() };
        let want = governed_resident(&proc_status().expect("status"), &l);
        COUNTING.with(|c| c.set(true));
        let got = files.resident(&mut buf);
        COUNTING.with(|c| c.set(false));
        assert_eq!((got, ALLOCS.with(std::cell::Cell::get)), (Some(want), 0), "the reading path's resident, no allocation");
        // D4029 (c), observe-only: the cgroup's numbers for the backstop's lines
        let seen = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        assert_eq!((seen(&CGROUP_SEEN.current), seen(&CGROUP_SEEN.inactive), seen(&CGROUP_SEEN.mapped)), (16_000 * M, 1_048_576_000 - 104_857_600, 7 * M), "B0: the inactive page cache less its dirty pages");
        let s = CgroupSeen { mark_ms: 1000.into(), mark_max: 100.into(), ..CgroupSeen::default() };
        s.note(0, 0, 0, 0, 400, 1500);
        assert_eq!(s.max_rate.load(Ordering::Relaxed), 0, "inside the second: no rate yet");
        s.note(0, 0, 0, 0, 700, 3000);
        assert_eq!(s.max_rate.load(Ordering::Relaxed), 300, "600 'max' events over 2 s");
        assert_eq!(want, 16_000 * M - reclaimable_file(stat));
        // no cgroup limit: anon + shmem from the status file alone
        files.open(None);
        assert!(files.resident(&mut buf).is_some_and(|r| r > 0 && r < want));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn allocator_stats_hook_roundtrip() {
        assert!(allocator_stats().is_none());
        set_allocator_stats(|| AllocatorStats {
            current_rss: 42,
            current_commit: 43,
        });
        let s = allocator_stats().expect("installed");
        assert_eq!(s.current_rss, 42);
        assert_eq!(s.current_commit, 43);
    }

    /// D3629: with the lock held at the store sample nothing is announced and the tick is due again; the next sample
    /// stores, then announces, and the announced limit is the cached one the cancel path reads.
    #[test]
    fn d3629_armed_is_announced_only_after_the_limit_store() {
        let cached = || LIMIT.try_lock().ok().and_then(|c| c.clone()).map(|l| (l.source, l.bytes));
        let mut state = WatchState::default();
        for mb in [321, 654] {
            guc_tables::backing::set_pgrust_memory_watchdog_limit(mb);
            let before = (state.announced, cached());
            {
                let _held = pgsync::lock(&LIMIT);
                assert!(arm(&mut state, 80).is_none(), "a skipped store announces nothing");
            }
            assert!(state.store_due, "a skipped store is retried at the next sample");
            assert_eq!((state.announced, cached()), before, "neither the log nor the cache moved");
            let l = arm(&mut state, 80).expect("the next sample stores, then announces");
            let want = Some((LimitSource::Guc, (mb as u64) << 20));
            assert!(!state.store_due);
            assert_eq!(state.announced, want);
            assert_eq!(cached(), want, "the log and the cached limit agree");
            assert_eq!(Some((l.source, l.limit)), want);
        }
        guc_tables::backing::set_pgrust_memory_watchdog_limit(0);
    }

    /// The ledger view's cgroup resident: the limiting cgroup's memory.stat anon + shmem (not anon_thp / shmem_thp /
    /// active_anon), and nothing without a cgroup limit.
    #[test]
    fn the_cgroup_resident_is_its_anon_plus_shmem() {
        let stat = "anon_thp 4\nanon 100\nfile 999\nactive_anon 60\nshmem_thp 2\nshmem 7\n";
        let root = fake_tree("cgres", &[("u/memory.stat", stat)]);
        let l = MemLimit { bytes: 16 << 30, source: LimitSource::Cgroup, cgroup: Some(root.join("u")), ..MemLimit::default() };
        assert_eq!(cgroup_resident_in(&l), Some(107));
        let physical = MemLimit { bytes: 61 << 30, source: LimitSource::Physical, cgroup: None, ..MemLimit::default() };
        assert_eq!(cgroup_resident_in(&physical), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// pppress: `allocator_resident`'s split is the same read's two terms, taken once.
    #[test]
    fn the_allocator_resident_split_is_the_same_reads_terms() {
        let _ = super::take_allocator_resident_split();
        let r = super::allocator_resident();
        let s = super::take_allocator_resident_split();
        match r {
            Some(r) => assert_eq!(s.map(|(maps, pool)| maps + pool), Some(r), "maps + pool = the reading"),
            None => assert_eq!(s, None),
        }
        assert_eq!(super::take_allocator_resident_split(), None, "taken once");
    }

    /// Exercise the host syscall independently of the cached probe. A nameless host must not publish a zero total
    /// or an older split; a host with names must expose the real, faulted test mapping to the mincore reader.
    #[cfg(target_os = "linux")]
    #[test]
    fn residency_reporting_requires_available_mapping_names() {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let p = unsafe { libc::mmap(core::ptr::null_mut(), page, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        // SAFETY: the first byte of our private page; naming changes only its diagnostic attribute.
        unsafe { (p as *mut u8).write_volatile(1) };
        let rc = unsafe { libc::prctl(0x53564d41, 0 as libc::c_ulong, p as libc::c_ulong, page as libc::c_ulong, c"pgrust:residency-test".as_ptr()) };
        let error = (rc != 0).then(std::io::Error::last_os_error);
        let maps = super::read_text("/proc/self/maps").expect("Linux maps are readable");
        let named_resident = super::present_in(&maps, |n| n == "[anon:pgrust:residency-test]");
        // SAFETY: the mapping made above; all reads are finished.
        assert_eq!(unsafe { libc::munmap(p, page) }, 0);
        if let Some(e) = error {
            assert_eq!(e.raw_os_error(), Some(libc::EINVAL), "unexpected naming failure: {e}");
            assert_eq!(named_resident, Some(0), "no name is not a zero-memory observation");
            super::RESIDENT_SPLIT.with(|c| c.set(Some((7, 11))));
            assert_eq!(super::allocator_resident(), None, "unknown must not be reported as zero");
            assert_eq!(super::take_allocator_resident_split(), None, "unknown must clear the prior split");
            assert_eq!(super::region_resident(), None);
        } else {
            assert_eq!(named_resident, Some(page), "the named, faulted page is counted");
        }
        assert_eq!(super::vma_names_supported(), Some(rc == 0));
        assert_eq!(super::vma_names_supported(), Some(rc == 0), "cached capability agrees");
    }

    /// pagepool P2: the pool's present bytes join the allocator's resident ones only while the pool is on, and an
    /// unreadable pool reading leaves the whole reading unknown (never the maps alone, never its claimed bytes).
    #[test]
    fn the_allocator_resident_adds_the_pools_present_bytes_while_it_is_on() {
        assert_eq!(super::with_pool(7 << 20, false, || panic!("the pool is off: no reading")), Some(7 << 20));
        assert_eq!(super::with_pool(7 << 20, true, || Some(3 << 20)), Some(10 << 20));
        assert_eq!(super::with_pool(7 << 20, true, || None), None);
    }

    /// The allocator's mappings are the maps lines named `[anon:mimalloc]` (not another anonymous one, not a file's), the
    /// Gravity regions' those named for a region kind, and the resident bytes are the pages present in them.
    #[test]
    fn the_allocator_resident_is_its_named_mappings_present_pages() {
        let maps = "\
7f0000000000-7f0040000000 rw-p 00000000 00:00 0                          [anon:mimalloc]
7f0040000000-7f0040200000 rw-p 00000000 00:00 0
7f0050000000-7f0060000000 rw-p 00000000 00:00 0                          [anon:pgrust:region:hash_agg]
aaaa00000000-aaaa00100000 r-xp 00000000 103:01 4242                      /usr/lib/postgresql/postgres
7f0070000000-7f0070400000 ---p 00000000 00:00 0                          [anon:mimalloc]
";
        let r: Vec<_> = named_ranges(maps, &|n: &str| n == "[anon:mimalloc]").collect();
        assert_eq!(r, vec![(0x7f00_0000_0000, 0x7f00_4000_0000), (0x7f00_7000_0000, 0x7f00_7040_0000)]);
        // the Gravity regions' mappings: the default kind's and each breaker kind's, not the arena's nor `:mapped`
        let regions = "\
7f0080000000-7f0080100000 rw-p 00000000 00:00 0                          [anon:pgrust:region]
7f0090000000-7f0090200000 rw-p 00000000 00:00 0                          [anon:pgrust:region:hash_agg]
7f00a0000000-7f00a0100000 rw-p 00000000 00:00 0                          [anon:pgrust:region:arena]
7f00b0000000-7f00b0100000 rw-p 00000000 00:00 0                          [anon:pgrust:region:mapped]
7f00c0000000-7f00c0100000 rw-p 00000000 00:00 0                          [anon:pgrust:regions]
7f00d0000000-7f00d0100000 rw-p 00000000 00:00 0                          [anon:pgrust:arena]
";
        let r: Vec<_> = named_ranges(regions, &is_region_name).collect();
        assert_eq!(r, vec![(0x7f00_8000_0000, 0x7f00_8010_0000), (0x7f00_9000_0000, 0x7f00_9020_0000)]);
        #[cfg(target_os = "linux")]
        {
            // a real mapping of 64 pages with 10 of them written: 10 pages present
            // SAFETY: sysconf has no preconditions; a fresh anonymous private mapping, written inside its length.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let p = unsafe { libc::mmap(core::ptr::null_mut(), 64 * page, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
            assert_ne!(p, libc::MAP_FAILED);
            for i in 0..10 {
                // SAFETY: inside the mapping.
                unsafe { (p as *mut u8).add(i * page * 3).write_volatile(1) };
            }
            let line = format!("{:x}-{:x} rw-p 00000000 00:00 0 [anon:test]", p as usize, p as usize + 64 * page);
            assert_eq!(present_in(&line, |n| n == "[anon:test]"), Some(10 * page));
            // SAFETY: the mapping made above.
            unsafe { libc::munmap(p, 64 * page) };
        }
    }

    /// D4812 (2): the block gate projects from R6's read of the sample, with the governor's bar on or off: the
    /// governed resident under `ladder_kill_read` (USER 10-18), else the ladder's read (memory.current less the clean
    /// inactive page cache); its projection is that read plus the context bytes taken since.
    #[test]
    fn d4812_the_block_gates_projection_is_r6s_read_of_the_sample_plus_the_context_bytes_since() {
        let _one = GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        const M: u64 = 1 << 20;
        let (own, footprint, since) = (9_000 * M, 500 << 20, 7 << 20);
        // the cgroup at this sample: memory.current 16,000 MiB, 3,000 MiB of it clean inactive page cache
        CGROUP_SEEN.current.store(16_000 * M, Ordering::Relaxed);
        CGROUP_SEEN.inactive.store(3_000 * M, Ordering::Relaxed);
        let read = ladder_read_of(own);
        assert_eq!(read, 13_000 * M, "the ladder's read: memory.current less the clean inactive page cache, over the governed resident");
        for (kill, want) in [(0, read), (1, own)] {
            mempolicy::consts::set(mempolicy::consts::K::LadderKillRead, kill);
            for (pct, limit) in [(0, 16_000 * M), (90, 16_000 * M)] {
                publish_samples(pct, limit, Some(own), footprint);
                assert_eq!(mcx::governor::gate_projection_at(footprint + since), (want as usize) + since, "ladder_kill_read {kill}, governor threshold {pct}: R6's read and the bytes since");
            }
            // no reading: the gate keeps its sample
            publish_samples(90, 16_000 * M, None, footprint);
            assert_eq!(mcx::governor::gate_projection_at(footprint + since), (want as usize) + since, "ladder_kill_read {kill}: an unreadable sample keeps the gate's last");
        }
        mempolicy::consts::set(mempolicy::consts::K::LadderKillRead, 1);
        CGROUP_SEEN.clear();
        mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
        mcx::governor::publish_gate(0, mcx::global_footprint::bytes());
    }
}
