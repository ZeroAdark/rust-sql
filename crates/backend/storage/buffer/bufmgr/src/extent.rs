//! The buffer pool's address-space reservation and its extent map.
//!
//! Blocks, descriptors and IO condition variables live in private anonymous
//! mappings sized for `pgrust.buffer_pool_max`, so the base addresses the hit
//! path reads never move. The reservation is cut into extents of 256 buffers
//! (2 MiB of blocks); each carries a state byte. Buffer ids stay dense: an
//! extent that is not ACTIVE keeps its descriptors (reads as an invalid,
//! unpinned buffer) and only its block memory is handed back to the kernel.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU8, Ordering};

use init_small::globals;
use types_core::BLCKSZ;
use types_error::{PgError, PgResult, ERRCODE_CONFIG_FILE_ERROR, ERRCODE_OUT_OF_MEMORY};

pub(crate) const EXTENT_SHIFT: u32 = 8;
pub(crate) const EXTENT_BUFFERS: i32 = 1 << EXTENT_SHIFT;
pub(crate) const EXTENT_BYTES: usize = EXTENT_BUFFERS as usize * BLCKSZ;

pub(crate) const ACTIVE: u8 = 0;
pub(crate) const DRAINING: u8 = 1;
pub(crate) const RETIRED: u8 = 2;
pub(crate) const UNBORN: u8 = 3;

const PAGE: usize = 4096;
const DESC_BYTES: usize = crate::buf_hdr::BUFFERDESC_PAD_TO_SIZE;
const CV_BYTES: usize = core::mem::size_of::<condition_variable::ConditionVariable>();

/// Sizing decided from the GUCs before anything is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub nbuffers_guc: i32,
    pub max_buffers: i32,
    pub boot_buffers: i32,
    pub n_extents: i32,
    pub thp: bool,
}

/// The mapped regions, published once at init.
pub(crate) struct Reservation {
    pub blocks: *mut u8,
    pub descs: *mut u8,
    pub cvs: *mut u8,
}

struct Map {
    blocks: AtomicPtr<u8>,
    states: AtomicPtr<AtomicU8>,
    max_buffers: AtomicI32,
    boot_buffers: AtomicI32,
    nbuffers_guc: AtomicI32,
    n_extents: AtomicI32,
}

static MAP: Map = Map {
    blocks: AtomicPtr::new(core::ptr::null_mut()),
    states: AtomicPtr::new(core::ptr::null_mut()),
    max_buffers: AtomicI32::new(0),
    boot_buffers: AtomicI32::new(0),
    nbuffers_guc: AtomicI32::new(0),
    n_extents: AtomicI32::new(0),
};

// The clock-sweep modulus: one past the last buffer of the highest extent
// that is not RETIRED. Stored Release after the extent's descriptors are
// initialized and its state is ACTIVE.
static SPAN: AtomicI32 = AtomicI32::new(0);
/// What the blocks reservation was given at `reserve` (huge-page advice, core exclusion), which a release's fresh
/// mapping must be given again.
static BLOCKS_THP: AtomicBool = AtomicBool::new(false);
static BLOCKS_NODUMP: AtomicBool = AtomicBool::new(false);
static ACTIVE_BUFFERS: AtomicI32 = AtomicI32::new(0);

/// Number of extents covering `n` buffers.
pub(crate) fn extents_for(n: i32) -> i32 {
    (n + EXTENT_BUFFERS - 1) >> EXTENT_SHIFT
}

pub(crate) fn round_up_extent(n: i32) -> i32 {
    (n + EXTENT_BUFFERS - 1) & !(EXTENT_BUFFERS - 1)
}

#[cold]
fn config_error(msg: String, detail: Option<String>) -> Box<PgError> {
    let mut e = PgError::error(msg).with_sqlstate(ERRCODE_CONFIG_FILE_ERROR);
    if let Some(d) = detail {
        e = e.with_detail(d);
    }
    Box::new(e)
}

/// buf_grow: whether pgrust.memory_ladder is on as the pool is reserved (it acts only then; a later change needs a
/// restart to widen the reservation).
fn ladder_on_at_start() -> bool {
    guc_tables::vars::pgrust_memory_ladder.installed()
        && guc_tables::vars::pgrust_memory_ladder.read() == guc_tables::consts::MEMORY_LADDER_ON
}

/// This machine's physical memory (bytes; 0 unknown).
fn physical_bytes() -> u64 {
    // SAFETY: sysconf reads a constant, no pointers.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    if pages <= 0 || size <= 0 { 0 } else { (pages as u64).saturating_mul(size as u64) }
}

/// buf_grow: the reservation's buffers for a pool that may grow into free memory: `buf_grow_max_pct` of `phys` bytes in
/// whole extents, with buf_grow and buf_giveback on and the ladder on at the start (`ladder_on`); 0 (shared_buffers
/// exactly, the inert shape) otherwise or when that is not past `nbuffers`.
pub(crate) fn grow_max_buffers(nbuffers: i32, ladder_on: bool, phys: u64) -> i32 {
    use mempolicy::consts::{get, K};
    if !ladder_on || get(K::BufGrow) == 0 || get(K::BufGiveback) == 0 {
        return 0;
    }
    let bytes = (phys as u128 * get(K::BufGrowMaxPct).min(100) as u128 / 100) as u64;
    let n = (bytes / BLCKSZ as u64).min(i32::MAX as u64 / 2) as i32 & !(EXTENT_BUFFERS - 1);
    if n > nbuffers { n } else { 0 }
}

/// `pgrust.buffer_pool_max` 0 is shared_buffers exactly (the inert default:
/// boot pool unchanged, last extent may be partial). Above it the boot pool
/// rounds up to whole extents.
pub(crate) fn plan_geometry(nbuffers: i32, max_guc: i32, thp: bool) -> PgResult<Geometry> {
    if max_guc != 0 && max_guc < nbuffers {
        return Err(config_error(
            format!(
                "\"pgrust.buffer_pool_max\" ({max_guc} buffers) must be 0 or at least \"shared_buffers\" ({nbuffers} buffers)"
            ),
            None,
        ));
    }
    if thp && !cfg!(target_os = "linux") {
        return Err(config_error(
            "\"pgrust.buffer_pool_thp\" is not supported on this platform".to_string(),
            Some("Transparent huge pages are only available on Linux.".to_string()),
        ));
    }
    let max_buffers = max_guc.max(nbuffers);
    let boot_buffers = if max_buffers == nbuffers {
        nbuffers
    } else {
        round_up_extent(nbuffers).min(max_buffers)
    };
    Ok(Geometry {
        nbuffers_guc: nbuffers,
        max_buffers,
        boot_buffers,
        n_extents: extents_for(max_buffers),
        thp,
    })
}

pub(crate) fn geometry_from_gucs(nbuffers: i32) -> PgResult<Geometry> {
    let max_guc = if guc_tables::vars::pgrust_buffer_pool_max.installed() {
        guc_tables::vars::pgrust_buffer_pool_max.read()
    } else {
        0
    };
    // buf_grow (user 10-18: the buffer pool grows into free memory): with no explicit max and the ladder on at the
    // start, the reservation is buf_grow_max_pct of physical memory, so the ladder can grow the pool into it
    let max_guc = if max_guc == 0 { grow_max_buffers(nbuffers, ladder_on_at_start(), physical_bytes()) } else { max_guc };
    let thp = guc_tables::vars::pgrust_buffer_pool_thp.installed()
        && guc_tables::vars::pgrust_buffer_pool_thp.read();
    plan_geometry(nbuffers, max_guc, thp)
}

#[cfg(target_os = "linux")]
const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE;
#[cfg(all(not(target_os = "linux"), not(target_family = "wasm")))]
const MAP_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANON;

#[cfg(not(target_family = "wasm"))]
fn map_anon(len: usize) -> Option<*mut u8> {
    // SAFETY: fresh anonymous mapping; the kernel picks the address.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            MAP_FLAGS,
            -1,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p as *mut u8)
}

/// wasm32 has no mmap: a reservation is one zeroed allocation of the linear
/// memory, which a wasm instance never hands back (it can only grow).
#[cfg(target_family = "wasm")]
fn map_anon(len: usize) -> Option<*mut u8> {
    let layout = std::alloc::Layout::from_size_align(len, PAGE).ok()?;
    // SAFETY: a reservation is at least one page long, so the layout is not zero-sized.
    let p = unsafe { std::alloc::alloc_zeroed(layout) };
    (!p.is_null()).then_some(p)
}

/// Hands the unaligned slack around the aligned blocks back to the kernel.
#[cfg(not(target_family = "wasm"))]
fn trim_slack(raw: *mut u8, head: usize, end: *mut u8, tail: usize) {
    // SAFETY: the two ends of the mapping `reserve` made, outside the blocks.
    unsafe {
        if head > 0 {
            libc::munmap(raw as *mut libc::c_void, head);
        }
        if tail > 0 {
            libc::munmap(end as *mut libc::c_void, tail);
        }
    }
}

/// wasm32: an allocation is freed whole or not at all, so the slack (less
/// than one extent) stays with the blocks.
#[cfg(target_family = "wasm")]
fn trim_slack(_raw: *mut u8, _head: usize, _end: *mut u8, _tail: usize) {}

#[cold]
fn reserve_error(what: &str, len: usize) -> Box<PgError> {
    let errno = std::io::Error::last_os_error();
    Box::new(
        PgError::error(format!(
            "could not reserve {len} bytes of address space for the buffer pool {what}: {errno}"
        ))
        .with_sqlstate(ERRCODE_OUT_OF_MEMORY)
        .with_hint(
            "Lower \"pgrust.buffer_pool_max\", or allow overcommit of reserved address space.",
        ),
    )
}

/// Maps the reservation for `geo`: blocks 2 MiB-aligned; states, descriptors
/// and CVs in one metadata mapping. Nothing is touched, so none of it is
/// resident until used.
pub(crate) fn reserve(geo: &Geometry) -> PgResult<Reservation> {
    let blocks_len = geo.n_extents as usize * EXTENT_BYTES;
    let raw_len = blocks_len + EXTENT_BYTES;
    let raw = map_anon(raw_len).ok_or_else(|| reserve_error("blocks", blocks_len))?;
    mempolicy::maps::name(raw, raw_len, c"pgrust:shared_buffers");
    let head = raw.align_offset(EXTENT_BYTES);
    let blocks = raw.wrapping_add(head);
    let tail = raw_len - head - blocks_len;
    trim_slack(raw, head, blocks.wrapping_add(blocks_len), tail);
    if geo.thp {
        advise_huge(blocks, blocks_len)?;
    }
    BLOCKS_THP.store(geo.thp, Ordering::Relaxed);
    exclude_from_core(blocks, blocks_len);

    let max = geo.max_buffers as usize;
    let states_len = (geo.n_extents as usize).next_multiple_of(PAGE);
    let descs_len = max * DESC_BYTES;
    let cvs_len = (max * CV_BYTES).next_multiple_of(PAGE);
    let meta_len = states_len + descs_len.next_multiple_of(PAGE) + cvs_len;
    let meta = map_anon(meta_len).ok_or_else(|| reserve_error("metadata", meta_len))?;
    mempolicy::maps::name(meta, meta_len, c"pgrust:shared_buffers_meta");
    let states = meta as *mut AtomicU8;
    let descs = meta.wrapping_add(states_len);
    let cvs = descs.wrapping_add(descs_len.next_multiple_of(PAGE));
    for e in 0..geo.n_extents as usize {
        let st = if (e as i32) < extents_for(geo.boot_buffers) {
            ACTIVE
        } else {
            UNBORN
        };
        // SAFETY: e < n_extents <= states_len bytes of the fresh mapping.
        unsafe { (*states.add(e)).store(st, Ordering::Relaxed) };
    }
    MAP.states.store(states, Ordering::Relaxed);
    MAP.blocks.store(blocks, Ordering::Relaxed);
    MAP.max_buffers.store(geo.max_buffers, Ordering::Relaxed);
    MAP.boot_buffers.store(geo.boot_buffers, Ordering::Relaxed);
    MAP.nbuffers_guc.store(geo.nbuffers_guc, Ordering::Relaxed);
    MAP.n_extents.store(geo.n_extents, Ordering::Relaxed);
    SPAN.store(geo.boot_buffers, Ordering::Release);
    set_active_buffers(geo.boot_buffers);
    Ok(Reservation { blocks, descs, cvs })
}

/// D3666: the block array is a cache of relation pages, each one on disk or in
/// WAL, and at shared_buffers' size it was most of what a crashed server's
/// core held: it leaves the core (MADV_DONTDUMP, given again to a released
/// extent's fresh mapping). The crash handler puts back the blocks the
/// faulting thread was touching ([`blocks_span`]); PGRUST_CORE_DUMP_BUFFERS=1
/// keeps the whole array in the core. A refused advice only leaves the core
/// as large as before.
#[cfg(target_os = "linux")]
fn exclude_from_core(p: *mut u8, len: usize) {
    if std::env::var_os("PGRUST_CORE_DUMP_BUFFERS").is_some_and(|v| v == "1") {
        return;
    }
    BLOCKS_NODUMP.store(true, Ordering::Relaxed);
    // SAFETY: the blocks reservation just mapped; the advice only changes
    // what a core dump writes.
    unsafe { libc::madvise(p as *mut libc::c_void, len, libc::MADV_DONTDUMP) };
}

#[cfg(not(target_os = "linux"))]
fn exclude_from_core(_p: *mut u8, _len: usize) {}

/// D3666: the block array's `(base, bytes)`, `(0, 0)` before it is mapped.
/// Atomic loads only: the crash handler reads it.
pub fn blocks_span() -> (usize, usize) {
    let base = MAP.blocks.load(Ordering::Relaxed) as usize;
    if base == 0 {
        return (0, 0);
    }
    (base, MAP.n_extents.load(Ordering::Relaxed).max(0) as usize * EXTENT_BYTES)
}

/// MIMALLOC_ALLOW_THP=0 disables THP for the whole process (mimalloc sets
/// PR_SET_THP_DISABLE); under that prctl MADV_HUGEPAGE is accepted but
/// never honored, so asking for huge pages there is refused, not ignored.
#[cfg(target_os = "linux")]
fn advise_huge(p: *mut u8, len: usize) -> PgResult<()> {
    // SAFETY: integer-argument prctl query of this process's THP policy.
    if unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) } == 1 {
        return Err(config_error(
            "\"pgrust.buffer_pool_thp\" is on but transparent huge pages are disabled for this process"
                .to_string(),
            Some(
                "MIMALLOC_ALLOW_THP=0 in the server's environment disables them for the whole process."
                    .to_string(),
            ),
        ));
    }
    // SAFETY: range lies inside the reservation just mapped.
    if unsafe { libc::madvise(p as *mut libc::c_void, len, libc::MADV_HUGEPAGE) } != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(config_error(
            format!("could not enable transparent huge pages for the buffer pool: {errno}"),
            Some("\"pgrust.buffer_pool_thp\" is on but madvise(MADV_HUGEPAGE) failed.".to_string()),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn advise_huge(_p: *mut u8, _len: usize) -> PgResult<()> {
    unreachable!("plan_geometry refuses THP off Linux")
}

#[inline]
pub(crate) fn span() -> i32 {
    SPAN.load(Ordering::Acquire)
}

pub(crate) fn set_span(n: i32) {
    SPAN.store(n, Ordering::Release);
}

#[inline]
pub(crate) fn active_buffers() -> i32 {
    ACTIVE_BUFFERS.load(Ordering::Relaxed)
}

/// The block array's committed bytes on the buffer pool's owner; moved at `reserve` and under `RESIZING`.
static ACTIVE_HELD: pgsync::Mutex<Option<memledger::Charge>> = pgsync::Mutex::new(None);

pub(crate) fn set_active_buffers(n: i32) {
    ACTIVE_BUFFERS.store(n, Ordering::Relaxed);
    bufmgr_seams::publish_active_buffers(n);
    // G1-A door 2: the block array is a NORESERVE reservation; its active extents are what it commits
    let shared = memledger::Acct::of(memledger::Consumer::SharedBuffers);
    let bytes = n.max(0) as usize * BLCKSZ;
    {
        let owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::SharedBuffers));
        let mut held = ACTIVE_HELD.lock().unwrap_or_else(|e| e.into_inner());
        let held = held.get_or_insert_with(|| {
            memledger::Charge::open(owner, memledger::Release::Evict, memledger::Kind::Mapped, memledger::charge::Door::Mapped, 0)
        });
        let have = held.committed();
        if bytes as u64 >= have {
            held.commit(bytes as u64 - have);
        } else {
            // retired extents leave the claim with their pages: no room is kept for them
            held.uncommit(have - bytes as u64);
            held.shrink(have - bytes as u64);
        }
    }
    list_resident(shared, bytes);
}

/// The block array's entry on the residency registry (`memledger::resident::NONE` until listed).
static BLOCKS_RES: AtomicU32 = AtomicU32::new(memledger::resident::NONE);

/// The block array on the residency registry, read up to the highest extent in use: a page the buffer pool never read
/// into, or a retired extent's released block, is not resident. Runs at `reserve` and under `RESIZING`.
fn list_resident(shared: memledger::Acct, bytes: usize) {
    let lo = MAP.blocks.load(Ordering::Relaxed) as usize;
    let span = computed_span().max(0) as usize * BLCKSZ;
    let id = BLOCKS_RES.load(Ordering::Relaxed);
    if id != memledger::resident::NONE {
        memledger::resident::update(id, lo, span, bytes, shared);
    } else if lo != 0 && bytes >= mempolicy::watchdog::RESIDENCY_MIN {
        BLOCKS_RES.store(memledger::resident::register(lo, span, bytes, shared, true, true), Ordering::Relaxed);
    }
}

pub(crate) fn max_buffers() -> i32 {
    MAP.max_buffers.load(Ordering::Relaxed)
}

pub(crate) fn boot_buffers() -> i32 {
    MAP.boot_buffers.load(Ordering::Relaxed)
}

pub(crate) fn nbuffers_guc() -> i32 {
    MAP.nbuffers_guc.load(Ordering::Relaxed)
}

pub(crate) fn n_extents() -> i32 {
    MAP.n_extents.load(Ordering::Relaxed)
}

/// Buffer id range `[lo, hi)` of extent `e`; the last extent may be short.
#[inline]
pub(crate) fn extent_range(e: i32) -> (i32, i32) {
    let lo = e << EXTENT_SHIFT;
    (lo, (lo + EXTENT_BUFFERS).min(max_buffers()))
}

#[inline]
fn state_cell(e: i32) -> &'static AtomicU8 {
    let states = MAP.states.load(Ordering::Relaxed);
    debug_assert!(!states.is_null() && (0..n_extents()).contains(&e));
    // SAFETY: e < n_extents; the metadata mapping lives for the process.
    unsafe { &*states.add(e as usize) }
}

#[inline]
pub(crate) fn state(e: i32) -> u8 {
    state_cell(e).load(Ordering::Relaxed)
}

pub(crate) fn set_state(e: i32, st: u8) {
    state_cell(e).store(st, Ordering::Release);
}

/// Victim-path gate. Called with the buffer's header lock held: a drainer
/// flips DRAINING before it inspects any header of the extent, so a victim
/// path locking the header after the drainer's inspection sees the flip,
/// and one locking before it holds a pin the drainer sees.
#[inline]
pub(crate) fn buffer_active(buf_id: i32) -> bool {
    state(buf_id >> EXTENT_SHIFT) == ACTIVE
}

/// Hands extent `e`'s block memory back to the kernel: the range is replaced
/// by a fresh anonymous mapping (reads as zeroes, freed at once). Linux goes
/// through `pagegive` (D4685, CVE-2026-74674: never one MADV_DONTNEED over a
/// whole 2 MiB-aligned range, which an extent always is) and gives the fresh
/// mapping the reservation's name, core exclusion and huge-page advice again.
pub(crate) fn release_blocks(e: i32) -> PgResult<()> {
    let (lo, hi) = extent_range(e);
    let base = MAP.blocks.load(Ordering::Relaxed);
    let p = base.wrapping_add(lo as usize * BLCKSZ);
    let len = (hi - lo) as usize * BLCKSZ;
    release_range(p, len)
}

#[cfg(target_os = "linux")]
fn release_range(p: *mut u8, len: usize) -> PgResult<()> {
    let huge = BLOCKS_THP.load(Ordering::Relaxed);
    // SAFETY: page-aligned range inside the blocks reservation (a MAP_PRIVATE |
    // MAP_ANONYMOUS | MAP_NORESERVE mapping of ours); no pinned or valid buffer
    // maps into it (the extent is RETIRED) and nothing touches it meanwhile.
    if unsafe { pagegive::give_back(p as usize, len, pagegive::Mapping::Reserved { huge }) } != 0 {
        return Err(release_error(len));
    }
    mempolicy::maps::name(p, len, c"pgrust:shared_buffers");
    if BLOCKS_NODUMP.load(Ordering::Relaxed) {
        // SAFETY: the range just released; the advice only changes what a core dump writes.
        unsafe { libc::madvise(p as *mut libc::c_void, len, libc::MADV_DONTDUMP) };
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(target_family = "wasm")))]
fn release_range(p: *mut u8, len: usize) -> PgResult<()> {
    // SAFETY: MAP_FIXED over a range this module owns (inside the blocks
    // reservation, extent RETIRED): atomically replaces it with zero pages.
    let q = unsafe {
        libc::mmap(
            p as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            MAP_FLAGS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if q == libc::MAP_FAILED || q as *mut u8 != p {
        return Err(release_error(len));
    }
    Ok(())
}

/// wasm32: the range is zeroed in place, as a fresh mapping would read; the
/// memory stays with the instance.
#[cfg(target_family = "wasm")]
fn release_range(p: *mut u8, len: usize) -> PgResult<()> {
    // SAFETY: a range inside the blocks reservation; no pinned or valid buffer
    // maps into it (the extent is RETIRED).
    unsafe { core::ptr::write_bytes(p, 0, len) };
    Ok(())
}

#[cold]
fn release_error(len: usize) -> Box<PgError> {
    let errno = std::io::Error::last_os_error();
    Box::new(PgError::error(format!(
        "could not release {len} bytes of buffer pool memory: {errno}"
    )))
}

/// Resident bytes of the blocks reservation below `limit` buffers, from
/// mincore(2) (one entry per OS page: 16 KiB on Apple silicon), in
/// fixed-size chunks (no allocation).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn resident_block_bytes(limit: i32) -> u64 {
    const CHUNK_PAGES: usize = 4096;
    let base = MAP.blocks.load(Ordering::Relaxed);
    if base.is_null() {
        return 0;
    }
    // SAFETY: sysconf has no preconditions.
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        p if p > 0 => p as usize,
        _ => PAGE,
    };
    let total = limit.max(0) as usize * BLCKSZ;
    let mut vec = [0u8; CHUNK_PAGES];
    let mut resident = 0u64;
    let mut off = 0usize;
    while off < total {
        let len = (total - off).min(CHUNK_PAGES * page);
        let pages = len.div_ceil(page);
        // SAFETY: range inside the reservation; vec holds `pages` entries.
        let rc = unsafe { mincore_raw(base.add(off), len, vec.as_mut_ptr()) };
        if rc == 0 {
            resident += vec[..pages].iter().filter(|&&v| v & 1 != 0).count() as u64 * page as u64;
        }
        off += len;
    }
    resident
}

#[cfg(target_os = "linux")]
unsafe fn mincore_raw(p: *mut u8, len: usize, vec: *mut u8) -> libc::c_int {
    // SAFETY: caller contract.
    unsafe { libc::mincore(p as *mut libc::c_void, len, vec) }
}

#[cfg(all(not(target_os = "linux"), not(target_family = "wasm")))]
unsafe fn mincore_raw(p: *mut u8, len: usize, vec: *mut u8) -> libc::c_int {
    // SAFETY: caller contract.
    unsafe { libc::mincore(p as *const libc::c_void, len, vec as *mut libc::c_char) }
}

/// wasm32: all of the linear memory is resident, so the blocks below `limit`
/// buffers are.
#[cfg(target_family = "wasm")]
pub(crate) fn resident_block_bytes(limit: i32) -> u64 {
    if MAP.blocks.load(Ordering::Relaxed).is_null() {
        return 0;
    }
    limit.max(0) as u64 * BLCKSZ as u64
}

/// The span implied by the current states: one past the highest extent that
/// is not RETIRED or UNBORN. Resizer-only (serialized).
pub(crate) fn computed_span() -> i32 {
    let mut e = n_extents() - 1;
    while e >= 0 {
        let st = state(e);
        if st == ACTIVE || st == DRAINING {
            return extent_range(e).1;
        }
        e -= 1;
    }
    0
}

/// Pin floor: every PGPROC that can pin (backends, the runtime gang,
/// auxiliaries) keeps one default io_combine_limit read's worth of buffers,
/// so "no unpinned buffers available" cannot fire at the floor.
pub(crate) const PIN_FLOOR_PER_PROC: i32 = 16;

pub(crate) fn pin_floor_buffers() -> i32 {
    (globals::MaxBackends() + types_storage::NUM_AUXILIARY_PROCS).max(1) * PIN_FLOOR_PER_PROC
}
