#![allow(unsafe_code)]
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use cancelpoll::Step;
use pgsync::{Mutex, MutexGuard, OnceLock};

use gravity_abi::breaker::{table_kind, HashAgg, HashBuild, SortRun, T1, BUILD_PARTITION, BUILD_TABLE, HASHAGG_HDR, HASHBUILD_ENTRY, SORTRUN_HDR, TABLE_AGG, TABLE_BUILD, TABLE_SORT, TABLE_T1};
use gravity_abi::buildrows::{RowStore, SLICE_HDR};

use crate::mem::fence::{Fences, Records};
use crate::mem::runcodec;
use crate::{Error, Result};

/// One spill file (`spillset::SpillFile`'s surface, `breakers.md`); read by several threads at once (the join's third phase), written by its owner alone.
pub trait SpillFile: Send + Sync {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()>;
    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>;
    /// A pull reader over partition `part` (`None` when it holds no bytes): the k-way merge's runs.
    fn open_part(&self, part: u32) -> Result<Option<Box<dyn RunReader>>>;
    /// D3190: `open_part` from byte `offset` of the partition on (a put sort run's fence, `mem::fence`); `None` when
    /// no byte lies past it. This default reads the bytes before it and drops them; a file that can seek does.
    fn open_part_at(&self, part: u32, offset: u64) -> Result<Option<Box<dyn RunReader>>> {
        let Some(mut r) = self.open_part(part)? else { return Ok(None) };
        let (mut left, mut buf) = (offset, [0u8; 4096]);
        let mut step = cancelpoll::step("spill run open: the bytes before a fence skipped");
        while left > 0 {
            step.tick()?;
            let n = r.read(&mut buf[..left.min(4096) as usize])?;
            if n == 0 {
                return Ok(None);
            }
            left -= n as u64;
        }
        Ok(Some(r))
    }
    fn spilled_bytes(&self) -> u64;
    /// CONVERSION WAVE 2 (alloclint round 3: spillset `EpochWriter::finish`'s extent lists): the owner the file's own heap
    /// (its directory, grown at each epoch's finish) is charged to, set once at its open, before its first epoch. A file
    /// without such heap ignores it.
    fn charge_to(&mut self, _owner: Option<memledger::Owner>) {}
    /// D1327: unlink the file (a sort merge pass's runs two passes on); a file without disk only drops.
    fn delete(self: Box<Self>) -> Result<()> {
        Ok(())
    }
    /// The bytes written for `part` across every epoch (0 when the file cannot say).
    fn part_bytes(&self, _part: u32) -> u64 {
        0
    }
    /// D2572: whether a sort run written here is LZ4-compressed (`runcodec`): a host temp file while the session has a
    /// `temp_file_limit` (the bytes then decide 53400); else, and for a file without disk, its blocks stay raw (the
    /// compress's CPU bought nothing).
    fn compress_runs(&self) -> bool {
        false
    }
    /// D2572-d: whether a run of `bytes` written now, after the statement's `spilled` sort-run bytes, is compressed
    /// (compacted rows, else LZ4 blocks): the file's `compress_runs`, unless it can tell the bytes cannot decide a 53400
    /// (`runcodec::raw_fits`: the host's temp file far under the session's `temp_file_limit`).
    fn compress_run(&self, spilled: u64, bytes: u64) -> bool {
        let _ = (spilled, bytes);
        self.compress_runs()
    }
    /// A file whose records its consumer's layout does not describe (road dict.entries' runs, hosts entry_runs.rs: 16 B
    /// `[lo, hi]` identities for the 24 B pair layout) names the fold its writer wrote them for and their width; the owner
    /// folds its runs through it and never at the layout's width (merge.rs `fold_sub`). `None`: the layout's records.
    /// `spilled_bytes` / `part_bytes` stay in the consumer layout's record units (records x `layout.size()`) even at a
    /// narrower width: the owner's estimate divides them by the layout's size.
    fn iov(&self) -> Option<(IovFn, usize)> {
        None
    }
    /// NEW-53: a file without disk (`MemFile`: a resident keyed end's continuing arm's records handed on in memory): the
    /// owner phase reads it as any file, but the spill census never counts it (`Stream::spilled`, `spilled_bytes`).
    fn in_memory(&self) -> bool {
        false
    }
    /// NEW-53: a memory file's chunk for the next hand-off's `bytes` (`MemFile::reserve`); a disk file reserves nothing.
    fn reserve_hand(&mut self, _bytes: usize) -> Result<()> {
        Ok(())
    }
}

/// NEW-53 (D1443): the records a resident keyed end's continuing arm hands to its run's last arm in memory (`SpillTable::
/// hand_own`): per partition, the extents of the chunks its epochs wrote. Each hand-off reserves its own chunk at the
/// table's exact record bytes (`reserve`), the bytes the Node holds on the statement's grant for it (`Node::hand_off`), so a
/// chunk never grows past its charge. The records point at key copies in statement-pinned chunks (`program::pin`,
/// D1392), which outlive the last arm's owner fold.
/// CONVERSION WAVE 2 (R4 b, c): each chunk is charged to the stream's slot owner through door 1 (`chunk::ChunkRef`), and
/// so is the chunk list; a reader shares the chunks it reads (`MemReader`) instead of copying its partition.
#[derive(Default)]
pub struct MemFile {
    chunks: memledger::ChargedVec<ChunkRef>,
    /// Per partition: (chunk, offset, length) of each run of its records.
    parts: Vec<Vec<(u32, usize, usize)>>,
    bytes: u64,
    /// The stream's slot owner when the file was made (`hand_own`): each chunk's charge is on it (`None`: uncharged).
    owner: Option<memledger::Owner>,
}

/// CONVERSION WAVE 2 (R4 b): one hand-off's chunk (`chunk.rs`), charged to the slot owner by construction (Kind Spill:
/// records handed on in place of a spill file).
mod chunk;
use chunk::ChunkRef;

impl MemFile {
    fn new(nparts: u32, owner: Option<memledger::Owner>) -> MemFile {
        let mut f = MemFile { owner, chunks: memledger::ChargedVec::new(owner, memledger::Kind::Spill), ..MemFile::default() }; // alloc: Exec charged owner
        f.parts.resize_with(nparts as usize, Vec::new); // alloc: Exec
        f
    }

    /// A chunk of `bytes` for the next hand-off's epochs, charged to the file's owner (and its place in the list).
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.chunks.push(ChunkRef::new(self.owner, bytes)).map_err(|_| Error::host("gravity: a hand-off's chunk list"))
    }

    fn slices(&self, part: u32) -> impl Iterator<Item = &[u8]> + '_ {
        self.parts.get(part as usize).into_iter().flatten().map(|&(c, off, len)| &self.chunks[c as usize].bytes()[off..off + len])
    }
}

impl SpillFile for MemFile {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        // a chunk a reader shares is never written again (`MemReader`): the epoch takes a chunk of its own
        if self.chunks.last_mut().and_then(ChunkRef::get_mut).is_none() {
            self.reserve(parts.iter().map(|p| p.1.len()).sum())?;
        }
        let c = self.chunks.len() - 1;
        let MemFile { chunks, parts: lists, bytes: total, .. } = self;
        let Some(mut chunk) = chunks[c].get_mut() else { return Err(Error::host("gravity: a hand-off's chunk shared by a reader")) };
        let mut step = cancelpoll::step("spill hand-off: an epoch's partitions copied");
        let mut r: Result<()> = Ok(());
        for &(part, bytes) in parts {
            if let Err(e) = step.tick() {
                r = Err(e.into());
                break;
            }
            let Some(list) = lists.get_mut(part as usize) else {
                r = Err(Error::host("gravity: a hand-off's partition past its file"));
                break;
            };
            let off = chunk.append(bytes);
            list.push((c as u32, off, bytes.len())); // alloc: Exec
            *total += bytes.len() as u64;
        }
        r
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut step = cancelpoll::step("spill hand-off: a partition's chunks read");
        for s in self.slices(part) {
            step.tick()?;
            sink(s)?;
        }
        Ok(())
    }

    /// CONVERSION WAVE 2 (R4 c): a reader over the partition's extents in place (it shares their chunks), no copy.
    fn open_part(&self, part: u32) -> Result<Option<Box<dyn RunReader>>> {
        let Some(list) = self.parts.get(part as usize).filter(|l| l.iter().any(|e| e.2 != 0)) else { return Ok(None) };
        // the list is the reader's, charged to the file's owner, filled inside its reservation
        let mut extents = memledger::ChargedVec::new(self.owner, memledger::Kind::Spill); // alloc: Exec charged self.owner
        extents.try_reserve(list.len()).map_err(|_| Error::host("gravity: a hand-off reader's extents"))?;
        extents.with_vec(|v| v.extend(list.iter().filter(|e| e.2 != 0).map(|&(c, off, len)| (self.chunks[c as usize].share(), off, len)))); // alloc: Exec charged self.owner
        Ok(Some(Box::new(MemReader { extents, at: 0, pos: 0 }) as Box<dyn RunReader>)) // alloc: Exec
    }

    fn spilled_bytes(&self) -> u64 {
        self.bytes
    }

    fn part_bytes(&self, part: u32) -> u64 {
        self.slices(part).map(|s| s.len() as u64).sum()
    }

    fn in_memory(&self) -> bool {
        true
    }

    fn reserve_hand(&mut self, bytes: usize) -> Result<()> {
        self.reserve(bytes)
    }
}

/// `MemFile::open_part`'s cursor over one partition's extents, in place: it holds the chunks it reads (`ChunkRef`), so
/// they outlive the file when the stream clears first, and no chunk it holds is written again (`write_epoch`).
struct MemReader {
    extents: memledger::ChargedVec<(ChunkRef, usize, usize)>,
    /// The extent read now and the bytes of it read.
    at: usize,
    pos: usize,
}

impl RunReader for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut n = 0;
        let mut step = cancelpoll::step("spill hand-off: a partition's extents read");
        while n < buf.len() {
            step.tick()?;
            let Some((chunk, off, len)) = self.extents.get(self.at) else { break };
            let k = (buf.len() - n).min(len - self.pos);
            buf[n..n + k].copy_from_slice(&chunk.bytes()[off + self.pos..off + self.pos + k]);
            n += k;
            self.pos += k;
            if self.pos == *len {
                self.at += 1;
                self.pos = 0;
            }
        }
        Ok(n)
    }
}

/// NEW-53: the records resident keyed ends handed on (`SpillTable::hand_own`): in memory, then to disk files (the
/// statement's grant refused the memory), over the process (tests and the census read them).
pub static HANDED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// NEW-53: the stream index of a hand-off's memory file and of its disk file (a resident end writes no other file).
pub const HAND_MEMORY: usize = 0;
pub const HAND_DISK: usize = 1;

/// A file's own fold (`SpillFile::iov`): slices of `stride`-byte records folded into the owner's table under the split, the
/// records folded or `-1 - folded` on a full table (`FoldFn`'s contract; kernels fold.rs `ident_iov`).
pub type IovFn = unsafe fn(table: *mut HashAgg, slices: &[(*const u8, usize)], stride: usize, split: gravity_abi::breaker::SubSplit) -> i64;

/// A pull cursor over one partition's bytes (`spillset::PartReader`'s surface).
pub trait RunReader: Send {
    /// Up to `buf.len()` bytes; 0 at the end.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
}

// The spill file format's limits (abi spill.rs): a file's parts (`MAX_RUNS`), the runs a token indexes, a split's
// partitions, a merge's passes.
pub use gravity_abi::spill::MAX_RUNS;
use gravity_abi::limits::MAXORDER;
use gravity_abi::spill::{MAX_CHAIN_RUNS, MAX_MERGE_PASS, MAX_PARTS};

pub trait SpillSet: Send {
    fn open(&mut self, purpose: &str, generation: u64, worker: usize, nparts: u32) -> Result<Box<dyn SpillFile>>;
    /// D3203: the set's files unlinked now, the unlinks run by `fan(n, job)` once they hold `min_bytes` (job `i` unlinks
    /// file `i` on whichever thread runs it; every other effect of a delete on the calling thread); the set's drop removes
    /// what is left. -> (files, their bytes, whether `fan` ran). A set without disk files unlinks nothing.
    fn delete_files_with(&mut self, _min_bytes: u64, _fan: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> (usize, u64, bool) {
        (0, 0, false)
    }
}

pub trait SpillFactory: Sync {
    fn create(&self) -> Result<Box<dyn SpillSet>>;

    /// D491: `bytes` of a temp file mapped shared (`TempMap::shared`), the file already unlinked, its bytes charged against
    /// `temp_file_limit` (53400 past it, as PostgreSQL's FileWrite) and allocated on disk (a write-back never meets a full
    /// disk as SIGBUS). `Ok(None)`: no file road here (the caller keeps its out of memory).
    fn map_temp(&self, _bytes: usize) -> Result<Option<TempMap>> {
        Ok(None)
    }

    /// C10 (D4168): whether `map_temp` maps temp files here, so a spool past W and G takes that road at once
    /// (`hostimpl::arena::object_region`). A factory whose `map_temp` maps nothing says false (the default).
    fn maps_temp(&self) -> bool {
        false
    }
}

/// D491: the session factory's `map_temp` (rule C14: spill files are made only in mem); `Ok(None)` without a factory.
pub fn map_temp(bytes: usize) -> Result<Option<TempMap>> {
    spill_factory().map_or(Ok(None), |f| f.map_temp(bytes))
}

/// C10 (D4168): whether this process has the temp-file road (`SpillFactory::maps_temp`).
pub fn file_road() -> bool {
    spill_factory().is_some_and(|f| f.maps_temp())
}

/// D491: a temp file's bytes mapped shared, read and written as memory, held by no grant pool: the page cache writes them back
/// to the file under memory pressure and reads them in again at a touch, so a region past the pool spills as PostgreSQL's
/// temp files do. Its pages start zeroed (a fresh file). The file is unlinked by its maker once mapped: the mapping alone
/// keeps it, and the drop gives back the mapping and the file's blocks.
pub struct TempMap {
    ptr: *mut u8,
    len: usize,
    /// G1-A door 2 / C8: its pages as their owner's charge while mapped (a spill's file pages are page cache the statement
    /// dirties: the context owner's, its statement's or portal's, while it holds them).
    charge: memledger::Charge,
    /// D4439: its range on the residency registry (`mem::resident`, as a mapped region's, D3872): the pages the page cache
    /// wrote back and dropped, or never read in, are off its account's resident bytes; `resident::NONE` when not listed.
    res: u32,
}

// SAFETY: the mapping is this value's alone; the owner phase that writes it and the reader after it run on one thread at a time.
unsafe impl Send for TempMap {}

impl TempMap {
    /// The first `len` bytes of open file `fd` (at least `len` long) mapped shared, readable and writable; `None` when the
    /// kernel refuses. The caller may close `fd` after: the mapping keeps the file.
    pub fn shared(fd: i32, len: usize) -> Option<TempMap> {
        if len == 0 {
            return None;
        }
        // wasm32-wasip1 has no mmap: a temp map is refused (the caller's `None` arm: no spill-to-file mapping).
        #[cfg(target_family = "wasm")]
        {
            let _ = fd;
            None
        }
        #[cfg(not(target_family = "wasm"))]
        {
            // SAFETY: a fresh shared mapping of `len` bytes of `fd` at an address the kernel picks; nothing else is touched.
            let p = unsafe { tempmap_vm::mmap(core::ptr::null_mut(), len, tempmap_vm::PROT_READ_WRITE, tempmap_vm::MAP_SHARED, fd, 0) };
            (p as usize != usize::MAX && !p.is_null()).then(|| {
                let (owner, release) = memledger::charge::context();
                let charge = memledger::Charge::open(owner, release, memledger::Kind::Spill, memledger::charge::Door::Mapped, len as u64);
                // D4439: a fresh map: none of its pages resident until a pass reads them
                let res = crate::mem::resident::register(p as usize, len, len, charge.acct(), true, true, charge.nr_home());
                TempMap { ptr: p as *mut u8, len, charge, res }
            })
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// LEDGER2 step 3: the mapping's charge moves to owner `o`, released as before (an operator arena's chunk past the pool:
    /// `spill_host::arena_chunk`; step 5: a breaker slot's owner at its draw). Nothing when it is `o`'s already.
    pub(crate) fn own_by(&mut self, o: memledger::Owner) {
        if self.charge.owner() != o {
            let release = self.charge.release();
            self.charge.transfer(o, release);
            // D4439: its not-resident bytes follow the charge to its new owner (as `RegionBuf::move_to`'s `listed`)
            crate::mem::resident::update_at(self.res, self.ptr as usize, self.len, self.len, self.charge.acct(), self.charge.nr_home());
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for TempMap {
    fn drop(&mut self) {
        // SAFETY: `ptr` / `len` are this value's own live mapping; nothing reads it past its owner's statement.
        #[cfg(not(target_family = "wasm"))]
        {
            crate::mem::resident::unregister(self.res);
            drop(core::mem::replace(&mut self.charge, memledger::Charge::NONE));
            unsafe { tempmap_vm::munmap(self.ptr as *mut core::ffi::c_void, self.len) };
        }
    }
}

/// D491: `TempMap`'s system calls (the same values on Linux and macOS; none on wasm32-wasip1).
#[cfg(not(target_family = "wasm"))]
mod tempmap_vm {
    use core::ffi::{c_int, c_void};
    extern "C" {
        pub(super) fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        pub(super) fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
    pub(super) const PROT_READ_WRITE: c_int = 3;
    pub(super) const MAP_SHARED: c_int = 0x01;
}

static FACTORY: OnceLock<&'static (dyn SpillFactory + Sync)> = OnceLock::new();

/// D3203: a parallel statement's spill files deleted across its idle workers at its end. The leader alone unlinked them,
/// the kernel freeing each file's page cache as it went: 64-174 ms of a W16 statement's end over a partitioned keyed
/// HashAgg's scatter, every worker idle. Installed by the pool beside its spill files (`install_spills`): `fan(width, n,
/// job)` runs `job(i)` for each `i < n` on at most `width` of the pool's workers and returns once every one it took ran;
/// false when no pool ran them.
pub type FanOut = fn(usize, usize, &(dyn Fn(usize) + Sync)) -> bool;
pub static END_FAN_OUT: OnceLock<FanOut> = OnceLock::new();

/// D3203: `delete_files_across`'s census, this process's totals (the runner's PHASES stmt line prints a statement's share
/// as `endfiles=`): 0 the spill files a parallel statement's end unlinked, 1 their bytes, 2 the ends that ran the unlinks
/// across the workers.
pub static END_FILES: [core::sync::atomic::AtomicU64; 3] = [const { core::sync::atomic::AtomicU64::new(0) }; 3];

pub fn install_spill_factory(f: &'static (dyn SpillFactory + Sync)) -> bool {
    FACTORY.set(f).is_ok()
}

/// D3035: the arena draws the grant pool refused that a mapped temp file took instead (`arena::raw::grow_mapped`), and their
/// bytes, this process's totals: the grant report line's `mapped` field, read by the census of statements served past the pool.
static MAPPED_PAST: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];

pub(crate) fn note_mapped_past(bytes: usize) {
    MAPPED_PAST[0].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    MAPPED_PAST[1].fetch_add(bytes as u64, core::sync::atomic::Ordering::Relaxed);
}

/// `(draws, bytes)` the mapped road took past the pool in this process (`note_mapped_past`).
pub fn mapped_past() -> (u64, u64) {
    (MAPPED_PAST[0].load(core::sync::atomic::Ordering::Relaxed), MAPPED_PAST[1].load(core::sync::atomic::Ordering::Relaxed))
}

pub fn spill_factory() -> Option<&'static (dyn SpillFactory + Sync)> {
    FACTORY.get().copied()
}

/// A new spill set over the session's factory, made here (rule C14: spill files are created only in mem); fail-closed
/// without a factory (`UNSUPPORTED_SPILL`), as the driver's own runs (exec ops/append/spool.rs `ArmSpool::new`).
pub fn new_set() -> Result<Box<dyn SpillSet>> {
    spill_factory().ok_or(Error::Unsupported(UNSUPPORTED_SPILL))?.create()
}

/// D1493: abi's own kind (it was 5, the out region's).
pub const UNSUPPORTED_SPILL: u8 = gravity_abi::status::UNSUPPORTED_SPILL;
/// A refilled partition that overflows splits by the next `SPLIT_BITS` hash bits, as deep as the hash has route bits.
pub const SPLIT_BITS: u8 = gravity_abi::hash::Depth::SPLIT_BITS as u8;
pub const OWN_STREAM: u8 = 0xFF;
/// D1327: the merged runs' file lineage (`file_key`), less the pass: no split lineage reaches it.
const MERGE_LINEAGE: u64 = 0xffff_ffff;

/// DR-17: a partition whose window ends at hash bit `next_shift` splits once more while the hash has `SPLIT_BITS` route
/// bits left below it and the owner's split count fits a token (`Token::extra`). Past that its records share every route
/// bit, so no further split separates them: `UNSUPPORTED_SPILL`.
fn may_split(next_shift: u8, extras: usize) -> bool {
    gravity_abi::hash::Depth::stored(next_shift as u32).deeper().is_some() && extras < u16::MAX as usize
}

/// A partition to refill: `extra` 0 is every owner's generation-0 file, else the leader's split file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub extra: u16,
    pub part: u16,
}

struct Extra {
    file: Box<dyn SpillFile>,
    shift: u8,
    bits: u8,
    generation: u8,
}

/// One ctx owner's splits of a stream: the split files it wrote on refill and its refill queue (owner-private: the third phase's workers refill at once and never touch each other's).
#[derive(Default)]
pub struct Splits {
    extras: Vec<Extra>,
    tokens: Vec<Token>,
}

/// One breaker life's spill stream (S5; `life` 0: none): files, splits and sorted runs per ctx owner (0 the leader, `1 + o` worker `o`).
#[derive(Default)]
pub struct Stream {
    pub life: u32,
    pub files: Vec<Option<Box<dyn SpillFile>>>,
    pub bits: Vec<u8>,
    splits: Vec<Splits>,
    runs: Vec<Chain>,
    nruns: Vec<u32>,
    /// V4R-136: the rows of each owner's runs (a put's table rows, kept at put / adopt time): `SPILL_RUN_ROWS`.
    rows: Vec<u64>,
    /// The owners' round-2 files (partitioned-record.md §3.5, §3.7), one per owner ordinal, written by its owner alone in round 1 and read by every owner in round 2.
    pub round2: Vec<Option<Box<dyn SpillFile>>>,
    /// M7-SPILL (V4R-359): each owner's staged bytes per partition of its stored file (`stage_put`), indexed by the part
    /// (a put finds its stage in one step), written as an epoch when a partition's reach `STAGE` bytes and at `stage_flush`:
    /// a kind-7 build's routed records and its probe's kept rows. CONVERSION WAVE 2: with their capacity's claim (`Stage`).
    stage: Vec<Stage>,
    /// D1327: the runs each pass of the sort's merge wrote (runmerge `merge_pass`), a file per pass (a pass reads the
    /// pass before's file, frozen, while it writes its own: a spill file is never read while written), apart from the put
    /// runs: `SPILL_RUNS`, `SPILL_RUN_ROWS` and `run_at` never count them; a reader names one as `MERGED_RUN | pass << 20 |
    /// index`. A pass's file is unlinked when the pass two on starts (the pass between has read it: PG logtape's space
    /// reuse), and a merge's first pass unlinks a merge before's (a rescan's): the peak is two passes' runs.
    merged: Vec<Option<(Chain, u32)>>,
    /// D2572: the merged run a pass is writing, its bytes gathered into encoded blocks (`runcodec`): written as the
    /// run's epochs when the pass's next run begins, when a run is opened and past `runcodec::EPOCH`.
    staged: runcodec::Staged,
    /// D2572-c: each owner's run rows' shape (`shape_of` at its put, a var sort's as its kernel declared it,
    /// `SPILL_RUN_SHAPE`): a compressing file's runs compact their rows by it, a merge's runs by the first owner's.
    shapes: Vec<Option<runcodec::RowShape>>,
    /// D2572-d: the raw bytes of the put runs (every owner's, each put adding its own: owners put at once, hence an
    /// atomic): what `SpillTable::put_raw_bytes` sums for a run's raw-or-compact choice; a merge pass rewrites at most
    /// these, so its runs are written raw only when they fit too (`compress_run`).
    put_raw: AtomicU64,
    /// D3190: each owner's put runs' fences (`mem::fence`), by run; `None` for a run put without them (no shape, or
    /// rows its shape does not frame). Dropped with the runs.
    fences: Vec<Vec<Option<Fences>>>,
    /// spillopt #5 (hybrid T1): each owner's phase-1 partitions a destaging scatter wrote (bit `part % 64` of word
    /// `part / 64`, `scatter_t1_destaging`): written first at its next full pool. Dropped with the files.
    destaged: Vec<Vec<u64>>,
    /// D3259: the section's sort runs, merged, would pass one region (`SPILL_SEAL_OVER`): filed by the leader before the
    /// section end (`Open::file_seal_over`, no worker running), read by every worker's section end, cleared after it.
    seal_over: core::sync::atomic::AtomicBool,
    /// D4373: the rows of those runs (the same read), plus one; 0 = not filed (`SPILL_SEAL_OVER`'s word, `seal_word`).
    seal_rows: core::sync::atomic::AtomicU64,
    /// C10 2b (D3582): a batched slot's one owner and its batches (mem/batch.rs `lay`), read by its emit's refills; dropped
    /// with the life.
    pub batched: Option<Box<crate::mem::batch::Batched>>,
    /// LEDGER2 step 5: the slot's ledger owner (`SlotRegion::owner`, `SpillTable::set_owner`) and the run bytes posted to
    /// its `spilled` (each put run's file bytes, `run_put`; owners put at once, hence an atomic): taken back when the runs
    /// are dropped (`clear`) or the owner leaves the slot.
    owner: Option<memledger::Owner>,
    posted: core::sync::atomic::AtomicI64,
}

/// D1493: the files of one owner's put runs, or of one merge pass's runs: run `i` is part `i % MAX_RUNS` of file
/// `i / MAX_RUNS` (each chained file its own name: `file_key`'s lineage names the chain, `chain_key`). Dropped with its
/// files (a stream's `clear`, a life's end, the statement's) or deleted (`delete`, a merge pass two before).
#[derive(Default)]
pub struct Chain {
    files: Vec<Box<dyn SpillFile>>,
}

impl Chain {
    /// The file run `i` is a part of, and its part; `None` past the files opened.
    fn at(&self, i: u32) -> Option<(&dyn SpillFile, u32)> {
        self.files.get((i / MAX_RUNS) as usize).map(|f| (f.as_ref(), i % MAX_RUNS))
    }

    fn at_mut(&mut self, i: u32) -> Option<(&mut Box<dyn SpillFile>, u32)> {
        self.files.get_mut((i / MAX_RUNS) as usize).map(|f| (f, i % MAX_RUNS))
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Files open (a test's count: `D1493`).
    pub fn files(&self) -> usize {
        self.files.len()
    }

    fn spilled_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.spilled_bytes()).sum()
    }

    fn delete(self) -> Result<()> {
        let mut step = cancelpoll::step("spill merge: a pass's run files unlinked");
        for f in self.files {
            step.tick()?;
            f.delete()?;
        }
        Ok(())
    }
}

/// M7-SPILL: a staged partition's bytes written as their own epoch past this many.
pub const STAGE: usize = 64 << 10;

/// D4215 / D4408: the spilled bytes read back, process-wide, by path: `[READ_PART]` a partition streamed whole
/// (`SpillFile::read_part`: the parallel merge's folds), `[READ_RUN]` a run's or a pair's reload (`RunReader::read`). The
/// Gravity report's per-statement line reads them (`read_counts`).
pub static READ_BYTES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
pub const READ_PART: usize = 0;
pub const READ_RUN: usize = 1;

/// `READ_BYTES` now.
pub fn read_counts() -> [u64; 2] {
    [READ_BYTES[READ_PART].load(Ordering::Relaxed), READ_BYTES[READ_RUN].load(Ordering::Relaxed)]
}

impl Stream {
    fn provision(&mut self, owners: usize) {
        if self.files.len() < owners {
            self.files.resize_with(owners, || None); // alloc: Node
            self.bits.resize(owners, 0); // alloc: Node
            self.splits.resize_with(owners, Splits::default); // alloc: Node
            self.runs.resize_with(owners, Chain::default); // alloc: Node
            self.nruns.resize(owners, 0); // alloc: Node
            self.rows.resize(owners, 0); // alloc: Node
            self.round2.resize_with(owners, || None); // alloc: Node
            self.stage.resize_with(owners, Stage::default); // alloc: Node
            self.shapes.resize_with(owners, || None); // alloc: Node
            self.fences.resize_with(owners, Vec::new); // alloc: Node
            self.destaged.resize_with(owners, Vec::new); // alloc: Node
        }
    }

    /// LEDGER2 step 5: the run bytes posted to the owner's `spilled` taken back (the runs dropped, or the owner leaving).
    fn unpost(&mut self) {
        let p = core::mem::take(self.posted.get_mut());
        if let (Some(o), true) = (self.owner, p != 0) {
            memledger::charge::spilled(o, -p);
        }
    }

    fn clear(&mut self) {
        self.unpost();
        // C9: a clear runs to its end (a life's start and end and the statement's end call it): each loop is one per owner
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for f in self.files.iter_mut() {
            *f = None;
        }
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for b in self.bits.iter_mut() {
            *b = 0;
        }
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for s in self.splits.iter_mut() {
            s.extras.clear();
            s.tokens.clear();
        }
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for r in self.runs.iter_mut() {
            *r = Chain::default();
        }
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for n in self.nruns.iter_mut() {
            *n = 0;
        }
        self.rows.iter_mut().for_each(|n| *n = 0);
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for f in self.round2.iter_mut() {
            *f = None;
        }
        self.stage.iter_mut().for_each(Stage::clear);
        self.merged.clear();
        self.staged = runcodec::Staged::default();
        self.shapes.iter_mut().for_each(|x| *x = None);
        self.fences.iter_mut().for_each(Vec::clear);
        self.destaged.iter_mut().for_each(Vec::clear);
        *self.put_raw.get_mut() = 0;
        *self.seal_over.get_mut() = false;
        *self.seal_rows.get_mut() = 0;
        self.batched = None;
    }

    /// D3259: `SPILL_SEAL_OVER`'s answer (`seal_over` filed for the section end running now, else false).
    pub fn seal_over(&self) -> bool {
        self.seal_over.load(core::sync::atomic::Ordering::Acquire)
    }

    /// D3259: the leader's answer for the section end it is about to run (no worker running), or false after it.
    pub fn set_seal_over(&self, over: bool) {
        self.seal_over.store(over, core::sync::atomic::Ordering::Release);
    }

    /// D4373: the rows the leader's answer read (`Merge::seal_rows`; `None`: not known), filed beside it.
    pub fn set_seal_rows(&self, rows: Option<u64>) {
        let w = rows.map_or(0, |r| r.min((1 << 61) - 2) + 1);
        self.seal_rows.store(w, core::sync::atomic::Ordering::Release);
    }

    /// `SPILL_SEAL_OVER`'s word: bit 0 the answer (`seal_over`), above it the rows it read plus one (0: not known), so a
    /// top's section end knows whether its cut keeps every row the answer read (kernels sort `resident_top`).
    pub fn seal_word(&self) -> i64 {
        (self.seal_over() as i64) | ((self.seal_rows.load(core::sync::atomic::Ordering::Acquire) as i64) << 1)
    }

    /// The sorted runs across every owner (the leader's k-way merge reads them in owner order).
    pub fn total_runs(&self) -> u32 {
        self.nruns.iter().sum()
    }

    /// V4R-136: the rows of every owner's runs (an emit's exact count).
    pub fn total_rows(&self) -> u64 {
        self.rows.iter().sum()
    }

    /// Run token `i` (owner order) → `(owner, run)`.
    fn run_at(&self, i: u32) -> Option<(usize, u32)> {
        let mut i = i;
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for (o, &n) in self.nruns.iter().enumerate() {
            if i < n {
                return Some((o, i));
            }
            i -= n;
        }
        None
    }

    /// Whether the stream wrote a disk file (NEW-53: a hand-off's memory file is no spill: `handed`).
    pub fn spilled(&self) -> bool {
        self.files.iter().flatten().any(|f| !f.in_memory()) || self.runs.iter().any(|c| !c.is_empty()) || !self.merged.is_empty()
    }

    /// NEW-53: whether a resident keyed end's continuing arm handed records on in memory (`SpillTable::hand_own`): files the
    /// run's last arm's owner phase reads beside its partials.
    pub fn handed(&self) -> bool {
        self.files.iter().flatten().any(|f| f.in_memory())
    }

    /// The bytes the stream wrote to disk (a hand-off's memory file is no spill).
    pub fn spilled_bytes(&self) -> u64 {
        self.files.iter().flatten().filter(|f| !f.in_memory()).map(|f| f.spilled_bytes()).sum::<u64>() + self.splits.iter().flat_map(|s| s.extras.iter()).map(|e| e.file.spilled_bytes()).sum::<u64>() + self.runs.iter().map(Chain::spilled_bytes).sum::<u64>() + self.round2.iter().flatten().map(|f| f.spilled_bytes()).sum::<u64>() + self.merged.iter().flatten().map(|m| m.0.spilled_bytes()).sum::<u64>()
    }

    pub fn bits0(&self) -> u8 {
        self.files.iter().position(Option::is_some).map_or(0, |i| self.bits[i])
    }

    /// The generation-0 partition count (0 = never spilled): the third phase's claims.
    pub fn gen0_parts(&self) -> u32 {
        if self.spilled() {
            1 << self.bits0()
        } else {
            0
        }
    }

    /// Owner `owner`'s refill queue length (0 = never spilled); the emit re-reads it per token.
    pub fn parts(&self, owner: usize) -> u32 {
        match self.splits.get(owner) {
            Some(s) if !s.tokens.is_empty() => s.tokens.len() as u32,
            _ => self.gen0_parts(),
        }
    }

    pub fn token(&self, owner: usize, i: u32) -> Option<Token> {
        match self.splits.get(owner) {
            Some(s) if !s.tokens.is_empty() => s.tokens.get(i as usize).copied(),
            _ => (i < self.gen0_parts()).then_some(Token { extra: 0, part: i as u16 }),
        }
    }

    pub fn generations(&self) -> u8 {
        self.splits.iter().flat_map(|s| s.extras.iter()).map(|e| e.generation + 1).max().unwrap_or(if self.spilled() { 1 } else { 0 })
    }
}

/// The Node's spill state (heap-stable, every ctx owner points into it): the set and the live stream of each slot's life.
#[derive(Default)]
pub struct SpillTable {
    /// Made under its lock by the first ctx owner that spills (W first claims overflow at once).
    pub set: Mutex<Option<Box<dyn SpillSet>>>,
    pub streams: Vec<Stream>,
    lives: u32,
    /// Per slot, what its ended lives' streams reached (read after the statement): the leader's queue, gen-0 parts, generations, bytes.
    ended: Vec<(u32, u32, u8, u64)>,
    /// Op 14: the first deferred pipeline error of the statement (`Entry::defer_failed`), raised by its consumer
    /// (`raise_held`), dropped unraised with the statement (`clear`, every exit path); a rerun call's own (`call_begin`).
    held: Mutex<Held>,
    /// D2086: the rerun calls begun while nothing was held, whose layer marks (each 0) are counted here instead of pushed under
    /// the lock (two locks per nested run); every locked use first moves them into `Held.marks` (`held_settled`).
    quiet: AtomicUsize,
    /// D2086: `Held.errs`' length, written under the lock: 0 lets `call_begin` / `call_end` count instead of lock.
    nerrs: AtomicUsize,
    /// D1719: the stored (kind-7) streams' file I/O, one at a time: a pair pass's claim writes its partition's drain part into
    /// its own file of the build slot's stream while another claim of the same pass opens that file's build and kept parts
    /// (`open_stored`); `SpillFile::read_part` iterates the epochs an epoch write appends to (a torn read: SIGSEGV, or an
    /// owner's extents missed and its records lost). Held around the epoch writes and the opens only, never the streaming.
    stored_io: Mutex<()>,
}

/// Op 14's held errors as layers: the statement's, then one per rerun call in progress (`marks`: where each call's layer
/// starts). A layer keeps its first error (a later one is dropped), unless that one is a finished call's (`done`); each entry
/// names the pipeline whose region is stamped FAILED (`usize::MAX`: not named).
#[derive(Default)]
struct Held {
    errs: Vec<(HeldError, usize, bool)>,
    marks: Vec<usize>,
}

/// Op 14: a pipeline's deferred error and the host's stashed error behind it (a `PgError`, opaque to the driver: `HELD_HOOKS`).
pub struct HeldError {
    pub err: Error,
    pub payload: Option<Box<dyn core::any::Any + Send + Sync>>,
}

/// The host side of a held error's payload, registered once by the runner (exec: gravity_hosts' stash): `take` the calling
/// thread's stashed error, `restash` a copy of one for a raise (every raising participant gets its own).
pub struct HeldHooks {
    pub take: fn() -> Option<Box<dyn core::any::Any + Send + Sync>>,
    pub restash: fn(&(dyn core::any::Any + Send + Sync)),
}

pub static HELD_HOOKS: OnceLock<HeldHooks> = OnceLock::new();

/// D2657: whether the calling thread's stashed host error is an interrupt (a cancel, a timeout or a shutdown that
/// check_for_interrupts raised inside a host or fn-shim call: SQLSTATE class 57, or FATAL and above), the stash left as
/// it was. Registered by the runner beside `HELD_HOOKS`.
pub static HELD_INTERRUPT: OnceLock<fn() -> bool> = OnceLock::new();

/// Op 14: whether pipeline error `e` is a statement's error a deferral may hold for the consumer's first row. Never a
/// cancel: the driver's own (`Error::Cancelled`) or one a host call raised (D2657: factorial's loop polls interrupts, so a
/// statement_timeout on a deferred build reached the driver as the call's raise, was held, and an empty outer dropped it:
/// the statement answered where PostgreSQL raises 57014; PG raises an interrupt where it lands, whatever the plan's
/// order). Never a recheck, a build wait or a bad return either.
pub fn holdable(e: &Error) -> bool {
    !matches!(e, Error::Cancelled | Error::Recheck(_) | Error::BuildWait(_) | Error::BadReturn(_)) && !HELD_INTERRUPT.get().is_some_and(|f| f())
}

impl SpillTable {
    /// One stream per breaker slot of the program (the Node's build).
    pub fn with_slots(nslots: usize) -> SpillTable {
        let mut t = SpillTable::default();
        t.streams.resize_with(nslots, Stream::default); // alloc: Node
        t.ended.resize(nslots, (0, 0, 0, 0)); // alloc: Node
        t
    }

    /// Slot `slot`'s new life (its breaker opens it): the stream emptied, the old life's files dropped (D165 by construction).
    pub fn begin_life(&mut self, slot: u8) -> u32 {
        self.lives = self.lives.wrapping_add(1).max(1);
        let s = &mut self.streams[slot as usize];
        s.clear();
        s.life = self.lives;
        self.lives
    }

    /// Slot `slot`'s life ended (its last consumer done): its stream and files dropped, what it reached kept for the census.
    pub fn end_life(&mut self, slot: u8) {
        let s = &mut self.streams[slot as usize];
        let e = &mut self.ended[slot as usize];
        *e = (e.0.max(s.parts(0)), e.1.max(s.gen0_parts()), e.2.max(s.generations()), e.3 + s.spilled_bytes());
        s.clear();
        s.life = 0;
    }

    /// LEDGER2 step 5: slot `slot`'s ledger owner (its breaker's, `Node::open_slot`; `None` as it ends): the bytes posted to
    /// the owner it had are taken back first.
    pub fn set_owner(&mut self, slot: u8, owner: Option<memledger::Owner>) {
        if let Some(s) = self.streams.get_mut(slot as usize) {
            if s.owner != owner {
                s.unpost();
                s.owner = owner;
            }
        }
    }

    /// NEW-43: slot `slot`'s put runs and merge passes dropped mid-life (`SPILL_RUN_DROP`): an INCSTREAM reader read their
    /// merge to its end and its producer streams into the region again; the life goes on, what the runs reached is kept
    /// for the census as `end_life` keeps it.
    pub fn drop_runs(&mut self, slot: u8) {
        let s = &mut self.streams[slot as usize];
        let e = &mut self.ended[slot as usize];
        *e = (e.0.max(s.parts(0)), e.1.max(s.gen0_parts()), e.2.max(s.generations()), e.3 + s.spilled_bytes());
        let life = s.life;
        s.clear();
        s.life = life;
    }

    /// Slot `slot`'s stream for a consumer's read: an error unless a live life owns it (S5: never a life that ended or never began).
    pub fn live_stream(&self, slot: u8) -> Result<&Stream> {
        match self.streams.get(slot as usize) {
            Some(s) if s.life != 0 => Ok(s),
            _ => Err(Error::host(format!("gravity: spill: slot {slot}'s stream read outside its life"))), // alloc: Cold
        }
    }

    /// D546: slot `slot`'s live stream's gen-0 parts (0 when none spilled in this life). An ended life's parts stay in the
    /// census (`census_of`) but never answer for the life that reuses the slot (a build after a spilled aggregate's slot).
    pub fn live_parts(&self, slot: u8) -> u32 {
        self.streams.get(slot as usize).map_or(0, Stream::gen0_parts)
    }

    /// Slot `slot`'s gen-0 parts and generations over its lives (the live stream's, or the most an ended one reached).
    pub fn census_of(&self, slot: u8) -> (u32, u8) {
        let (s, e) = (self.streams.get(slot as usize), self.ended.get(slot as usize).copied().unwrap_or_default());
        s.map_or((e.1, e.2), |s| (s.gen0_parts().max(e.1), s.generations().max(e.2)))
    }

    pub fn provision(&mut self, owners: usize) {
        // bounded("small-fixed: one stream per breaker slot of the program, a u8 slot: at most 256")
        for s in self.streams.iter_mut() {
            s.provision(owners);
        }
    }

    pub fn clear(&mut self) {
        self.clear_at(0);
    }

    /// `clear` at a statement's end that ran `width` workers: past one, its files are unlinked across them
    /// (`delete_files_across`) before the set drops.
    pub fn clear_at(&mut self, width: usize) {
        // bounded("small-fixed: one stream per breaker slot of the program, a u8 slot: at most 256")
        for s in self.streams.iter_mut() {
            s.clear();
            s.life = 0;
        }
        // bounded("small-fixed: one stream per breaker slot of the program, a u8 slot: at most 256")
        for e in self.ended.iter_mut() {
            *e = (0, 0, 0, 0);
        }
        self.delete_files_across(width);
        self.set = Mutex::new(None);
        self.held = Mutex::new(Held::default());
        self.quiet = AtomicUsize::new(0);
        self.nerrs = AtomicUsize::new(0);
    }

    /// D3203: the spill set's files, at the end of a statement that ran `width` workers (two or more), unlinked with the
    /// unlinks across those workers once they hold `END_FAN_OUT_BYTES` (program cost; `SpillSet::delete_files_with`: the
    /// sizes, the temp file statistics and any LOG stay on the leader, in the directory's order), before the set's drop
    /// removes its directories. A serial statement's set, or one with no pool, is removed by the drop's walk as before.
    fn delete_files_across(&mut self, width: usize) {
        let Some(fan) = END_FAN_OUT.get().filter(|_| width >= 2) else { return };
        let mut set = self.set.lock().unwrap_or_else(|p| p.into_inner());
        let Some(set) = set.as_mut() else { return };
        let (n, bytes, fanned) = set.delete_files_with(gravity_program::cost::runtime::END_FAN_OUT_BYTES, &|n: usize, job: &(dyn Fn(usize) + Sync)| {
            fan(width, n, job);
        });
        END_FILES[0].fetch_add(n as u64, Ordering::Relaxed);
        END_FILES[1].fetch_add(bytes, Ordering::Relaxed);
        END_FILES[2].fetch_add(fanned as u64, Ordering::Relaxed);
        if fanned {
            fires::fire("exec.end.unlink_across");
        }
    }

    /// The held layers, locked, with the counted quiet marks (`quiet`) moved into `marks` first (they are the newest).
    fn held_settled(&self) -> std::sync::MutexGuard<'_, Held> {
        let mut h = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let q = self.quiet.swap(0, Ordering::AcqRel);
        h.marks.extend(core::iter::repeat_n(0, q)); // alloc: Cold
        h
    }

    /// Op 14: hold `e` for its consumer's raise; the statement's first held error stays (a later one is dropped).
    pub fn hold(&self, e: HeldError) {
        self.hold_for(e, usize::MAX);
    }

    /// Op 14: `hold`, the error of pipeline `region`'s FAILED region, in the innermost layer (a rerun call's own while one
    /// runs: `call_begin`); a finished call's error kept there gives way to it.
    pub fn hold_for(&self, e: HeldError, region: usize) {
        let mut h = self.held_settled();
        let start = h.marks.last().copied().unwrap_or(0);
        match h.errs.get(start) {
            Some(&(_, _, false)) => {}
            _ => {
                h.errs.truncate(start);
                h.errs.push((e, region, false)); // alloc: Cold
            }
        }
        self.nerrs.store(h.errs.len(), Ordering::Release);
    }

    /// D1902 (errorhold): a rerun call of the set `member` names begins: a held error of a member's region from the set's
    /// earlier call is dropped (the region is reset in place for this call: nothing can raise it), and the call's own layer
    /// opens, so the call's member probes raise its own build's error, never an earlier call's (PG rebuilds per rescan).
    pub fn call_begin(&self, member: &dyn Fn(usize) -> bool) {
        // D2086: nothing held (every mark is then 0, errs never shorter than a mark): the call's mark 0 is counted, no lock
        if self.nerrs.load(Ordering::Acquire) == 0 {
            self.quiet.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let mut h = self.held_settled();
        let start = h.marks.last().copied().unwrap_or(0);
        if h.errs.get(start).is_some_and(|x| member(x.1)) {
            h.errs.truncate(start);
        }
        let n = h.errs.len();
        h.marks.push(n); // alloc: Cold
        self.nerrs.store(n, Ordering::Release);
    }

    /// The rerun call `call_begin` opened ends: an error it held unraised (its outer had no row) is dropped when the layer
    /// around it holds one already, else kept there, marked finished (a later hold there replaces it).
    pub fn call_end(&self) {
        // D2086: a counted quiet mark with nothing held since (a hold moves the counted marks under the lock): popped by count
        // bounded("other: a compare-exchange retry: it passes again only when another thread moved `quiet` between its load and the exchange, and does no work per pass")
        while self.nerrs.load(Ordering::Acquire) == 0 {
            let q = self.quiet.load(Ordering::Acquire);
            if q == 0 || self.quiet.compare_exchange(q, q - 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                if q > 0 {
                    return;
                }
                break;
            }
        }
        let mut h = self.held_settled();
        let Some(m) = h.marks.pop() else { return };
        let start = h.marks.last().copied().unwrap_or(0);
        if m > start {
            h.errs.truncate(m);
        } else if let Some(x) = h.errs.get_mut(m) {
            x.2 = true;
        }
        self.nerrs.store(h.errs.len(), Ordering::Release);
    }

    /// Op 14 (`program::raise_held`, any participant's thread): the held error, its host payload stashed again on this
    /// thread for the raise (`HELD_HOOKS.restash`); `None` when nothing is held. The held error stays for the next raiser.
    /// The innermost layer's: a rerun call's own while it runs.
    pub fn raise_held(&self) -> Option<Error> {
        let h = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let h = &h.errs.last()?.0;
        if let (Some(p), Some(hooks)) = (h.payload.as_deref(), HELD_HOOKS.get()) {
            (hooks.restash)(p);
        }
        Some(h.err.clone()) // alloc: Cold
    }

    /// The held error, without a raise (diagnostics and tests).
    pub fn held(&self) -> Option<Error> {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).errs.last().map(|h| h.0.err.clone())
    }

    pub fn spilled(&self) -> bool {
        self.streams.iter().any(Stream::spilled) || self.ended.iter().any(|e| e.1 != 0 || e.3 != 0)
    }

    pub fn spilled_bytes(&self) -> u64 {
        self.streams.iter().map(Stream::spilled_bytes).sum::<u64>() + self.ended.iter().map(|e| e.3).sum::<u64>()
    }

    /// D2572-d: the raw bytes of every stream's put runs so far, the statement's sort-run bytes a run's raw-or-compact
    /// choice weighs (`SpillFile::compress_run`). Read through the streams' atomic counts alone: during a section each
    /// owner writes its own runs and files, so a walk of their files (`spilled_bytes`) is the leader's after it.
    pub fn put_raw_bytes(&self) -> u64 {
        self.streams.iter().map(|s| s.put_raw.load(Ordering::Relaxed)).sum()
    }

    /// The leader's refill queue length of slot 0's stream (the one-breaker programs' census; an ended life's gen-0 count).
    pub fn parts(&self) -> u32 {
        self.streams.first().map_or(0, |s| s.parts(0)).max(self.ended.first().map_or(0, |e| e.0))
    }

    pub fn stream(&self, slot: u8) -> &Stream {
        &self.streams[slot as usize]
    }

    /// Slot `slot`'s bytes written to disk this statement: its live stream's and its ended lives'.
    pub fn slot_spilled_bytes(&self, slot: u8) -> u64 {
        self.streams.get(slot as usize).map_or(0, Stream::spilled_bytes) + self.ended.get(slot as usize).map_or(0, |e| e.3)
    }

    /// A file's name word: (the owning life, the slot, the split lineage): two lives of a slot never share a file (S5).
    fn file_key(&self, slot: u8, lineage: u64) -> u64 {
        (self.streams[slot as usize].life as u64 & 0xff) << 56 | (slot as u64) << 48 | lineage
    }

    /// A new file of `slot`'s stream. CONVERSION WAVE 2: the file's own heap (its directory) is charged to the slot's
    /// owner from its first epoch (`SpillFile::charge_to`).
    fn open_file(&self, slot: u8, purpose: &str, generation: u64, worker: usize, nparts: u32) -> Result<Box<dyn SpillFile>> {
        let mut set = self.set.lock().unwrap_or_else(|e| e.into_inner());
        if set.is_none() {
            let f = spill_factory().ok_or(Error::Unsupported(UNSUPPORTED_SPILL))?;
            *set = Some(f.create()?);
        }
        let mut f = set.as_mut().expect("a spill set").open(purpose, generation, worker, nparts)?;
        f.charge_to(self.streams[slot as usize].owner);
        Ok(f)
    }

    /// Owner `index`'s generation-0 file of `slot`, opened at its first scatter (fail-closed without a factory).
    pub fn file(&mut self, slot: u8, index: usize, bits: u8) -> Result<&mut dyn SpillFile> {
        self.file_stored(slot, index, bits, false)
    }

    /// `file` for a region that carries a row store: its slices ride partitions `1 << bits ..` of the same file, and a
    /// kind-7 build's kept probe rows and drain parts (M7-SPILL B4c) ride `2 << bits ..`. The count is only a bound: the
    /// spill set's directory lists the parts an epoch wrote, so the third band costs nothing where nothing drains.
    pub fn file_stored(&mut self, slot: u8, index: usize, bits: u8, stored: bool) -> Result<&mut dyn SpillFile> {
        self.file_locked(slot, index, bits, stored, None).map(|(_, f)| f)
    }

    /// D1719: `file_stored`, with the stored-stream lock (`lock`: a pass's stage write, which its other claims' opens may
    /// meet) taken first and handed back beside the file, so the caller's epoch write runs under it (disjoint fields).
    /// D4210: the lock is taken through the caller's step (`cancelpoll::lock_or_cancel`): another worker's epoch write
    /// can hold it through an I/O stall, and a plain `lock` waited that out whole (0.8-2.5 s off-CPU on the stress runs).
    fn file_locked(&mut self, slot: u8, index: usize, bits: u8, stored: bool, lock: Option<&mut cancelpoll::Step>) -> Result<(Option<MutexGuard<'_, ()>>, &mut dyn SpillFile)> {
        let io = match lock {
            Some(step) => Some(cancelpoll::lock_or_cancel(step, &self.stored_io)?),
            None => None,
        };
        self.streams[slot as usize].provision(index + 1);
        if self.streams[slot as usize].files[index].is_none() {
            let f = self.open_file(slot, "agg", self.file_key(slot, 0), index, (1 << bits) * if stored { 3 } else { 1 })?;
            let s = &mut self.streams[slot as usize];
            s.files[index] = Some(f);
            s.bits[index] = bits;
        }
        let s = &mut self.streams[slot as usize];
        if s.bits[index] != bits {
            return Err(Error::host("gravity: a spill's partition count changed mid-execution"));
        }
        Ok((io, s.files[index].as_mut().expect("opened").as_mut()))
    }

    /// M7-SPILL (V4R-359): `bytes` (whole records or rows) staged for partition `part` of owner `index`'s stored file of
    /// `slot` (`file_stored` at `bits`: parts `1 << bits ..` hold a kind-7 build's kept probe rows); a partition's stage past
    /// `STAGE` bytes is written as its own epoch, so the stages stay small whatever the owner writes. CONVERSION WAVE 2: a
    /// stage a put grew past `2 * STAGE` (a whole part put at once) is dropped after its write, not kept at that capacity
    /// to the life's end; the stages' capacity is charged to the slot's owner (`StageClaim`).
    pub fn stage_put(&mut self, slot: u8, index: usize, bits: u8, part: u32, bytes: &[u8]) -> Result<()> {
        let s = &mut self.streams[slot as usize];
        if s.stage.len() <= index {
            s.provision(index + 1);
        }
        let owner = s.owner;
        let Stage { claim, bufs: st } = &mut s.stage[index];
        let at = part as usize;
        if st.len() <= at {
            st.resize_with(at + 1, Vec::new); // alloc: Exec
        }
        let had = st[at].capacity();
        st[at].extend_from_slice(bytes); // alloc: Exec charged owner
        claim.moved(owner, had, st[at].capacity());
        if st[at].len() < STAGE {
            return Ok(());
        }
        // D3968: the step names a put's wait to the probe (the stored io lock, the file's first open, the epoch's write: one
        // call each) and ticks before it
        let mut step = cancelpoll::step("spill stage: the stored file's lock, open and epoch write");
        step.tick()?;
        let buf = core::mem::take(&mut st[at]);
        let r = self.file_locked(slot, index, bits, true, Some(&mut step)).and_then(|(_io, f)| f.write_epoch(&[(part, &buf)]));
        let s = &mut self.streams[slot as usize];
        let had = buf.capacity();
        let st = &mut s.stage[index];
        st.bufs[at] = StageClaim::kept(buf);
        st.claim.moved(s.owner, had, st.bufs[at].capacity());
        r
    }

    /// M7-SPILL: owner `index`'s staged bytes of `slot` written as one epoch (nothing staged: nothing written).
    pub fn stage_flush(&mut self, slot: u8, index: usize, bits: u8) -> Result<()> {
        let Some(st) = self.streams[slot as usize].stage.get_mut(index) else { return Ok(()) };
        if st.bufs.iter().all(|b| b.is_empty()) {
            return Ok(());
        }
        let mut step = cancelpoll::step("spill stage: the stored file's lock, open and epoch write");
        step.tick()?;
        let staged = core::mem::take(&mut st.bufs);
        // an epoch's parts go in ascending order (the spill set's write rule): the stages' own order
        let parts: Vec<(u32, &[u8])> = staged.iter().enumerate().filter(|e| !e.1.is_empty()).map(|(p, b)| (p as u32, b.as_slice())).collect(); // alloc: Exec
        let r = self.file_locked(slot, index, bits, true, Some(&mut step)).and_then(|(_io, f)| f.write_epoch(&parts));
        drop(parts);
        let s = &mut self.streams[slot as usize];
        let had: usize = staged.iter().map(|b| b.capacity()).sum();
        let st = &mut s.stage[index];
        st.bufs = staged.into_iter().map(StageClaim::kept).collect(); // alloc: Exec charged s.owner
        let now = st.bufs.iter().map(|b| b.capacity()).sum();
        st.claim.moved(s.owner, had, now);
        r
    }

    /// M7-SPILL: whether owner `index` holds staged bytes of `slot` (a read before its flush would miss them).
    pub fn staged(&self, slot: u8, index: usize) -> bool {
        self.streams[slot as usize].stage.get(index).is_some_and(|st| st.bufs.iter().any(|b| !b.is_empty()))
    }

    /// M7-SPILL: the owners of `slot`'s stream (its provisioned file entries) and a reader over owner `index`'s partition
    /// `part` of its stored file (`None`: that owner wrote no file, or the partition holds no bytes).
    pub fn owners(&self, slot: u8) -> usize {
        self.streams[slot as usize].files.len()
    }

    pub fn open_stored(&self, slot: u8, index: usize, part: u32) -> Result<Option<Box<dyn RunReader>>> {
        // D1719: the open reads the file's epoch list (and the file entry a first write fills): never beside a write.
        // D4210: that write can hold the lock through an I/O stall; the wait polls the statement's cancel
        let mut step = cancelpoll::step("spill open_stored: the stored file's lock");
        let _io = cancelpoll::lock_or_cancel(&mut step, &self.stored_io)?;
        match self.streams[slot as usize].files.get(index) {
            Some(Some(f)) => f.open_part(part),
            _ => Ok(None),
        }
    }

    /// The round-2 buffers at `region` (an owner's phase-1 table) flushed as one epoch of owner
    /// `ordinal`'s round-2 file of `slot`, opened at its first flush at the buffers' partition count;
    /// the pool reset. Returns the records written.
    /// # Safety
    /// `region` is a live phase-1 table, the calling owner's alone; entry `ordinal` is that owner's
    /// alone while the owners run (provisioned before the pool runs, read after it).
    pub unsafe fn flush_round2(&self, slot: u8, ordinal: usize, region: *mut u8) -> Result<u64> {
        if table_kind(region) != TABLE_T1 || ordinal >= self.streams[slot as usize].round2.len() {
            return Err(Error::host("gravity: a round-2 flush without buffers"));
        }
        let pbits = (*(region as *const T1)).pbits;
        let entry = self.streams[slot as usize].round2.as_ptr().add(ordinal) as *mut Option<Box<dyn SpillFile>>;
        if (*entry).is_none() {
            *entry = Some(self.open_file(slot, "round2", self.file_key(slot, 0), ordinal, 1 << pbits)?);
        }
        scatter_t1(region as *mut T1, 0, pbits, (*entry).as_mut().expect("opened").as_mut())
    }

    /// D533: owner `ordinal`'s round-2 file of `slot` dropped (a two-round owner folds round 1 again in a grown region; the
    /// next flush opens a fresh file).
    /// # Safety
    /// As `flush_round2`: entry `ordinal` is the calling owner's alone, and nothing reads it until round 2.
    pub unsafe fn reset_round2(&self, slot: u8, ordinal: usize) {
        if ordinal < self.streams[slot as usize].round2.len() {
            let entry = self.streams[slot as usize].round2.as_ptr().add(ordinal) as *mut Option<Box<dyn SpillFile>>;
            *entry = None;
        }
    }

    /// Write the dense rows of the table at `region` as owner `index`'s next sorted run of `slot` (the region cleared); returns the run's index in the owner's file.
    /// # Safety
    /// `region` is a live table region whose rows are dense at the front (a `DENSE` record table).
    pub unsafe fn run_put(&mut self, slot: u8, index: usize, region: *mut u8) -> Result<u32> {
        let (base, bytes) = dense_rows(region).ok_or_else(|| Error::host("gravity: a run from a table that is not dense"))?;
        let rows = table_rows(region);
        self.streams[slot as usize].provision(index + 1);
        // D2572-d: the statement's sort-run bytes before this run (a run far under the limit is written raw), this run's
        // counted first so owners putting at once see each other's; never `spilled_bytes`, which walks the other owners'
        // files while they write them (a W8 sort's SIGSEGV)
        self.streams[slot as usize].put_raw.fetch_add(bytes as u64, Ordering::Relaxed);
        let spilled = self.put_raw_bytes().saturating_sub(bytes as u64);
        let run = self.streams[slot as usize].nruns[index];
        if run >= MAX_CHAIN_RUNS {
            return Err(Error::Unsupported(UNSUPPORTED_SPILL));
        }
        // D1493: the owner's runs past a file's MAX_RUNS go on in a chained file (a sort of more than 4096 runs at W0)
        let chain = (run / MAX_RUNS) as usize;
        if self.streams[slot as usize].runs[index].files.len() == chain {
            let f = self.open_file(slot, "run", self.file_key(slot, chain as u64), index, MAX_RUNS)?;
            self.streams[slot as usize].runs[index].files.push(f); // alloc: Exec
        }
        let s = &mut self.streams[slot as usize];
        let mut fenced = None;
        if bytes > 0 {
            // D2572-c: the rows' shape (a var sort's as declared), kept for the stream's merge
            let shape = shape_of(region, s.shapes[index].as_ref());
            let known = shape.is_some();
            if known {
                s.shapes[index] = shape;
            }
            let shape = s.shapes[index].as_ref().filter(|_| known);
            let (f, part) = s.runs[index].at_mut(run).expect("a run's chained file");
            // D2572: as LZ4 blocks (`runcodec`), D2572-c its rows compacted; `run_open` reads the rows back; D3190:
            // its fences built as it is written (the bytes unchanged). D3974: the fences serve only the co-split cut, which
            // takes at most MAXORDER runs of the stream in all (exec ops/sort/cosplit.rs): an owner's runs past that are
            // put without them
            let raw = core::slice::from_raw_parts(base, bytes);
            if (run as usize) < MAXORDER {
                fenced = runcodec::put_run(f.as_mut(), part, raw, shape, spilled, s.owner)?;
            } else {
                runcodec::write_run_rows(f.as_mut(), part, raw, shape, spilled, s.owner)?;
            }
            // LEDGER2 step 5: the run's file bytes on the slot owner's `spilled` (its rows' bytes when the file cannot say)
            if let Some(o) = s.owner {
                let b = match f.part_bytes(part) {
                    0 => bytes as i64,
                    n => n as i64,
                };
                s.posted.fetch_add(b, Ordering::Relaxed);
                memledger::charge::spilled(o, b);
            }
        }
        let fs = &mut s.fences[index];
        fs.truncate(run as usize);
        fs.resize_with(run as usize, || None); // alloc: Exec
        // D3974: at its MAXORDER-th run the owner's earlier fences go too (no cut reads a stream past MAXORDER runs): the
        // fences a stream keeps are bounded by MAXORDER runs an owner, never one per run of a long run formation
        if run as usize == MAXORDER {
            fs.iter_mut().for_each(|f| *f = None);
        }
        fs.push(fenced); // alloc: Exec
        s.nruns[index] = run + 1;
        s.rows[index] += rows;
        clear_region(region);
        Ok(run)
    }

    /// M7 L3b, the sorted drain: every run of `from` moved to the same owner of `to` (the drain pipe's sort takes the probe pipe's runs); returns the runs moved.
    pub fn adopt_runs(&mut self, from: u8, to: u8) -> Result<u32> {
        if from == to {
            return Ok(0);
        }
        let n = self.streams[from as usize].runs.len();
        self.streams[to as usize].provision(n);
        let mut moved = 0;
        // bounded("small-fixed: one per owner, the leader and its workers: at most SLOTS (256) + 1")
        for i in 0..n {
            let (f, t) = if from < to {
                let (a, b) = self.streams.split_at_mut(to as usize);
                (&mut a[from as usize], &mut b[0])
            } else {
                let (a, b) = self.streams.split_at_mut(from as usize);
                (&mut b[0], &mut a[to as usize])
            };
            if f.nruns[i] == 0 {
                continue;
            }
            if t.nruns[i] != 0 {
                return Err(Error::host("gravity: a run adoption into a stream that holds runs"));
            }
            t.runs[i] = core::mem::take(&mut f.runs[i]);
            t.fences[i] = core::mem::take(&mut f.fences[i]);
            t.nruns[i] = core::mem::take(&mut f.nruns[i]);
            t.rows[i] = core::mem::take(&mut f.rows[i]);
            t.shapes[i] = f.shapes[i].take();
            moved += t.nruns[i];
        }
        let raw = self.streams[from as usize].put_raw.swap(0, Ordering::Relaxed);
        self.streams[to as usize].put_raw.fetch_add(raw, Ordering::Relaxed);
        Ok(moved)
    }

    /// D1327: a new merged run of `slot` in pass `pass`'s file, its token (`MERGED_RUN | pass << 20 | index`). `fresh`:
    /// the pass's first run, which opens the pass's file and unlinks the file of the pass two before (pass 0: every merged
    /// file of the slot, a merge before's).
    pub fn merge_begin(&mut self, slot: u8, pass: u32, fresh: bool) -> Result<u32> {
        self.merge_flush(slot)?;
        let p = pass as usize;
        if fresh {
            let s = &mut self.streams[slot as usize];
            if p == 0 {
                let mut step = cancelpoll::step("spill merge: a merge before's pass files unlinked");
                for (c, _) in s.merged.drain(..).flatten() {
                    step.tick()?;
                    c.delete()?;
                }
            } else if let Some((c, _)) = p.checked_sub(2).and_then(|q| s.merged.get_mut(q)).and_then(Option::take) {
                c.delete()?;
            }
            if p != s.merged.len() || p > MAX_MERGE_PASS {
                return Err(Error::host(format!("gravity: spill: merge pass {pass} of slot {slot} after {}", s.merged.len()))); // alloc: Cold
            }
            self.streams[slot as usize].merged.push(Some((Chain::default(), 0))); // alloc: Exec
        }
        let open = p + 1 == self.streams[slot as usize].merged.len();
        let n = match self.streams[slot as usize].merged.get(p) {
            Some(Some((_, n))) if open => *n,
            _ => return Err(Error::host(format!("gravity: spill: a run of merge pass {pass} of slot {slot}, not the open pass"))), // alloc: Cold
        };
        if n >= MAX_CHAIN_RUNS {
            return Err(Error::Unsupported(UNSUPPORTED_SPILL));
        }
        // D1493: a pass's runs past a file's MAX_RUNS go on in a chained file of the pass (the chain in the lineage's bits
        // below the pass's: MAX_MERGE_PASS < 64)
        let chain = (n / MAX_RUNS) as u64;
        if let Some(Some((c, _))) = self.streams[slot as usize].merged.get(p) {
            if c.files.len() as u64 == chain {
                let f = self.open_file(slot, "merge", self.file_key(slot, MERGE_LINEAGE - pass as u64 - (chain << 6)), 0, MAX_RUNS)?;
                if let Some(Some((c, _))) = self.streams[slot as usize].merged.get_mut(p) {
                    c.files.push(f); // alloc: Exec
                }
            }
        }
        if let Some(Some((_, n))) = self.streams[slot as usize].merged.get_mut(p) {
            *n += 1;
        }
        Ok(gravity_abi::program::MERGED_RUN | pass << 20 | n)
    }

    /// D1327: `bytes` appended to merged run `token` of `slot` (an epoch per write: a part's epochs read back in order).
    /// D2572: gathered into the stream's staged blocks (`runcodec::Staged`), written as epochs of encoded blocks.
    pub fn merge_write(&mut self, slot: u8, token: u32, bytes: &[u8]) -> Result<()> {
        let begins = self.streams[slot as usize].staged.token != Some(token);
        if begins {
            self.merge_flush(slot)?;
        }
        // D2572-d: a merged run begun raw only when the stream's put runs' raw bytes fit beside every stream's
        let spilled = if begins { self.put_raw_bytes() } else { 0 };
        let (p, i) = (((token & !gravity_abi::program::MERGED_RUN) >> 20) as usize, token & 0xfffff);
        let Stream { merged, staged, shapes, put_raw, owner, .. } = &mut self.streams[slot as usize];
        match merged.get_mut(p) {
            Some(Some((c, n))) if i < *n => match c.at_mut(i) {
                Some((f, part)) => {
                    if staged.token != Some(token) {
                        // D2572-c: a merge writes its runs' rows as they were put, compacted by their shape
                        staged.begin(token, f.compress_run(spilled, put_raw.load(Ordering::Relaxed)), shapes.iter().flatten().next(), *owner);
                    }
                    staged.push(bytes, &mut |b| f.write_epoch(&[(part, b)]))
                }
                None => Err(Error::host(format!("gravity: spill: merged run {token:#x} of slot {slot} past its files"))), // alloc: Cold
            },
            _ => Err(Error::host(format!("gravity: spill: a write to merged run {token:#x} of slot {slot}"))), // alloc: Cold
        }
    }

    /// D2572: the staged bytes of `slot`'s merged run in progress written as its last epoch (a run is never read while
    /// any of its bytes are staged).
    fn merge_flush(&mut self, slot: u8) -> Result<()> {
        let Stream { merged, staged, .. } = &mut self.streams[slot as usize];
        let Some(token) = staged.token else { return Ok(()) };
        let (p, i) = (((token & !gravity_abi::program::MERGED_RUN) >> 20) as usize, token & 0xfffff);
        match merged.get_mut(p).and_then(Option::as_mut).and_then(|(c, _)| c.at_mut(i)) {
            Some((f, part)) => staged.finish(&mut |b| f.write_epoch(&[(part, b)])),
            None => {
                *staged = runcodec::Staged::default();
                Err(Error::host(format!("gravity: spill: staged bytes of merged run {token:#x} of slot {slot} past its files"))) // alloc: Cold
            }
        }
    }

    /// A reader over run token `i` of `slot` (owner order, or `MERGED_RUN | index` a merge pass's run); `None` for an empty run.
    /// D2572: the reader decodes the run's blocks (`runcodec::Decoder`); a merged run's staged bytes are written first.
    pub fn run_open(&mut self, slot: u8, i: u32) -> Result<Option<Box<dyn RunReader>>> {
        self.merge_flush(slot)?;
        let s = &self.streams[slot as usize];
        let r = if i & gravity_abi::program::MERGED_RUN != 0 {
            let (p, m) = (((i & !gravity_abi::program::MERGED_RUN) >> 20) as usize, i & 0xfffff);
            match s.merged.get(p) {
                Some(Some((c, n))) if m < *n => match c.at(m) {
                    Some((f, part)) => f.open_part(part)?,
                    None => return Err(Error::BadReturn(i as i64)),
                },
                _ => return Err(Error::BadReturn(i as i64)),
            }
        } else {
            let Some((owner, run)) = s.run_at(i) else { return Err(Error::BadReturn(i as i64)) };
            match s.runs[owner].at(run) {
                Some((f, part)) => f.open_part(part)?,
                None => None,
            }
        };
        // D2572-c: a compacted run's rows expanded past its header (its first block says so)
        r.map(|r| runcodec::reader(r, s.owner)).transpose()
    }

    /// D3190: run token `i`'s fences (owner order, as `run_open` names runs); `None` for a run put without them or a
    /// merge pass's run.
    pub fn run_fences(&self, slot: u8, i: u32) -> Option<&Fences> {
        if i & gravity_abi::program::MERGED_RUN != 0 {
            return None;
        }
        let s = self.streams.get(slot as usize)?;
        let (owner, run) = s.run_at(i)?;
        s.fences.get(owner)?.get(run as usize)?.as_ref()
    }

    /// D3190: run token `i`'s records from fence `fence` on (its record first), or from its first record with `None`
    /// (`mem::fence::Records`); `None` for a run without fences. Any thread, while the run's file is frozen.
    pub fn run_records(&self, slot: u8, i: u32, fence: Option<usize>) -> Result<Option<Records>> {
        let Some(fs) = self.run_fences(slot, i) else { return Ok(None) };
        let s = &self.streams[slot as usize];
        let Some((file, part)) = s.run_at(i).and_then(|(owner, run)| s.runs[owner].at(run)) else { return Ok(None) };
        match fence {
            Some(f) if f < fs.len() => Records::at(file, part, fs, f).map(Some),
            Some(f) => Err(Error::host(format!("gravity: spill: fence {f} of a run with {}", fs.len()))), // alloc: Cold
            None => Records::first(file, part, fs.shape(), s.owner).map(Some),
        }
    }

    /// D2572-c: owner `index`'s var rows of `slot` laid out as `shape` (its kernel's `SPILL_RUN_SHAPE`, before a put).
    pub fn run_shape(&mut self, slot: u8, index: usize, shape: runcodec::RowShape) {
        let s = &mut self.streams[slot as usize];
        s.provision(index + 1);
        s.shapes[index] = Some(shape);
    }

    /// Scatter the table at `region` into owner `index`'s file of `slot` by the low `bits` of the hash.
    /// # Safety
    /// `region` is a live table region (a `HashAgg` or a `HashBuild` laid out by `at`).
    pub unsafe fn scatter_own(&mut self, slot: u8, index: usize, region: *mut u8, bits: u8) -> Result<u64> {
        // a phase-1 table's buffers are partitioned already: the stream takes their count
        let bits = if table_kind(region) == TABLE_T1 { (*(region as *const T1)).pbits } else { bits };
        let stored = table_kind(region) == TABLE_BUILD && RowStore::of(region).is_some();
        let owner = self.streams[slot as usize].owner;
        let file = {
            let mut step = cancelpoll::step("spill scatter: the stored file's lock and open");
            step.tick()?;
            self.file_stored(slot, index, bits, stored)?
        };
        scatter_region(region, 0, bits, file, owner)
    }

    /// spillopt #5 (hybrid T1): a kernel's full phase-1 pool at `region` (`SPILL_SCATTER` on a `TABLE_T1`) into owner
    /// `index`'s file of `slot` by `scatter_t1_destaging`: some partitions written, the rest kept in the pool. Only the
    /// kernel's flush takes this road; every other scatter of a table (a section's close, a seal, a hand-off) writes it whole.
    /// # Safety
    /// `region` is a live phase-1 table of this owner with no appender open.
    pub unsafe fn scatter_own_destaging(&mut self, slot: u8, index: usize, region: *mut u8) -> Result<u64> {
        let t = region as *mut T1;
        self.file_stored(slot, index, (*t).pbits, false)?;
        let Stream { files, destaged, .. } = &mut self.streams[slot as usize];
        let file = files[index].as_mut().expect("opened").as_mut();
        scatter_t1_destaging(t, file, &mut destaged[index])
    }

    /// NEW-53 (D1443): the phase-1 table at `region` of a resident keyed end (its records keep key copies: never a spill's)
    /// handed to its run's last arm through `slot`'s stream at the table's own partition count: into the stream's memory file
    /// (`HAND_MEMORY`) when `memory` (the Node holds its `bytes` on the statement's grant), else into its disk file
    /// (`HAND_DISK`: the grant refused them; a run-time shortfall spills, never raises). Returns the records moved.
    /// # Safety
    /// `region` is a live phase-1 table (`TABLE_T1`) of `bytes` record bytes; the leader thread.
    pub unsafe fn hand_own(&mut self, slot: u8, region: *mut u8, bytes: usize, memory: bool) -> Result<u64> {
        let bits = (*(region as *const T1)).pbits;
        let index = if memory { HAND_MEMORY } else { HAND_DISK };
        let file: &mut dyn SpillFile = if memory {
            let s = &mut self.streams[slot as usize];
            s.provision(index + 1);
            if s.files[index].is_none() {
                s.files[index] = Some(Box::new(MemFile::new(1 << bits, s.owner))); // alloc: Exec
                s.bits[index] = bits;
            }
            if s.bits[index] != bits {
                return Err(Error::host("gravity: a spill's partition count changed mid-execution"));
            }
            let f: &mut dyn SpillFile = s.files[index].as_deref_mut().expect("opened");
            if !f.in_memory() {
                return Err(Error::host("gravity: a resident end's hand-off found a disk file at its memory index"));
            }
            f.reserve_hand(bytes)?;
            f
        } else {
            self.file(slot, index, bits)?
        };
        let n = scatter_t1(region as *mut T1, 0, bits, file)?;
        HANDED[index].fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }

    /// Refill token `i` of `slot`'s stream (owner `owner`'s queue) into the cleared table at
    /// `region`; a partition that overflows splits into the next generation (its sub-partitions queued behind the owner's tokens, the table left empty). Returns the records held.
    /// # Safety `region` is a live table region; `owner` is the calling thread's ctx owner.
    pub unsafe fn refill(&mut self, slot: u8, owner: usize, i: u32, region: *mut u8) -> Result<u64> {
        let this = self as *mut SpillTable;
        let s = &mut self.streams[slot as usize];
        s.provision(owner + 1);
        let Some(token) = s.token(owner, i) else { return Err(Error::BadReturn(i as i64)) };
        clear_region(region);
        let (shift, bits, generation) = if token.extra == 0 { (0u8, s.bits0(), 0u8) } else { let e = &s.splits[owner].extras[token.extra as usize - 1]; (e.shift, e.bits, e.generation) };
        let next_shift = shift + bits;
        if table_kind(region) == TABLE_AGG {
            // the partition's records share its route bits: the table's homes read below them
            (*(region as *mut HashAgg)).set_route_bits(gravity_abi::hash::Depth::part(next_shift as u32).expect("a spill window inside the route bits"));
        }
        let split = core::cell::Cell::new(None::<usize>);
        let split_ref = &split;
        let mut overflow: Option<usize> = None;
        let mut n = 0u64;
        // SAFETY: `on_full` appends to the owner's own split files, never to the source it reads;
        // the set is made under its lock by the first ctx owner that spills (`open_file`)
        let mut on_full = move |region: *mut u8| -> Result<()> {
            let t = &mut *this;
            let ex = match split_ref.get() {
                Some(ex) => ex,
                None => {
                    if !may_split(next_shift, t.streams[slot as usize].splits[owner].extras.len()) {
                        return Err(Error::Unsupported(UNSUPPORTED_SPILL));
                    }
                    let lineage = ((generation as u64 + 1) << 32) | ((token.extra as u64) << 16) | token.part as u64;
                    let f = t.open_file(slot, "agg", t.file_key(slot, lineage), owner, split_parts(region))?;
                    let sp = &mut t.streams[slot as usize].splits[owner];
                    sp.extras.push(Extra { file: f, shift: next_shift, bits: SPLIT_BITS, generation: generation + 1 }); // alloc: Exec
                    split_ref.set(Some(sp.extras.len()));
                    sp.extras.len()
                }
            };
            let o = t.streams[slot as usize].owner;
            let e = &mut t.streams[slot as usize].splits[owner].extras[ex - 1];
            scatter_region(region, e.shift, e.bits, e.file.as_mut(), o).map(|_| ())
        };
        if token.extra == 0 {
            for o in 0..s.files.len() {
                let Some(f) = (*this).streams.as_ptr().add(slot as usize).as_ref().and_then(|s| s.files[o].as_ref()) else { continue };
                n += refill_region(region, token.part as u32, f.as_ref(), 1 << bits, &mut on_full, &mut overflow)?;
            }
        } else {
            let f = (*this).streams.as_ptr().add(slot as usize).as_ref().expect("stream").splits[owner].extras[token.extra as usize - 1].file.as_ref();
            n += refill_region(region, token.part as u32, f, 1 << bits, &mut on_full, &mut overflow)?;
        }
        if let Some(ex) = split.get() {
            // the tail joins the split file; the sub-partitions queue behind every token so far
            self.queue_split(slot, owner, ex, region)?;
            return Ok(0);
        }
        Ok(n)
    }

    /// Split the partition a refill left in the table at `region` (owner `owner`'s token `i`, it
    /// fit) by the next hash window, exactly as an overflow would have: the join's build split pairs each build sub-token with the outer sub-token of the same window and part, so the outer partition is read once per split, not once per build sub-token. Returns the owner's queue length (the sub-tokens are its last `1 << SPLIT_BITS`).
    /// # Safety
    /// As `refill`; the table at `region` holds token `i`'s partition.
    pub unsafe fn split_held(&mut self, slot: u8, owner: usize, i: u32, region: *mut u8) -> Result<u32> {
        let s = &mut self.streams[slot as usize];
        let Some(token) = s.token(owner, i) else { return Err(Error::BadReturn(i as i64)) };
        let (shift, bits, generation) = if token.extra == 0 { (0u8, s.bits0(), 0u8) } else { let e = &s.splits[owner].extras[token.extra as usize - 1]; (e.shift, e.bits, e.generation) };
        if !may_split(shift + bits, s.splits[owner].extras.len()) {
            return Err(Error::Unsupported(UNSUPPORTED_SPILL));
        }
        let lineage = ((generation as u64 + 1) << 32) | ((token.extra as u64) << 16) | token.part as u64;
        let f = self.open_file(slot, "agg", self.file_key(slot, lineage), owner, split_parts(region))?;
        let sp = &mut self.streams[slot as usize].splits[owner];
        sp.extras.push(Extra { file: f, shift: shift + bits, bits: SPLIT_BITS, generation: generation + 1 }); // alloc: Exec
        let ex = sp.extras.len();
        self.queue_split(slot, owner, ex, region)?;
        Ok(self.streams[slot as usize].splits[owner].tokens.len() as u32)
    }

    /// The table at `region` scattered into split `ex` of owner `owner`'s stream and cleared; the split's sub-partitions queued behind every token so far (the generation-0 tokens first).
    unsafe fn queue_split(&mut self, slot: u8, owner: usize, ex: usize, region: *mut u8) -> Result<()> {
        let st = &mut self.streams[slot as usize];
        let (gen0, o) = (st.gen0_parts(), st.owner);
        let sp = &mut st.splits[owner];
        let e = &mut sp.extras[ex - 1];
        scatter_region(region, e.shift, e.bits, e.file.as_mut(), o)?;
        clear_region(region);
        if sp.tokens.is_empty() {
            // bounded("small-fixed: one token per generation-0 partition, at most MAX_PARTS (2048)")
            for part in 0..gen0 {
                sp.tokens.push(Token { extra: 0, part: part as u16 }); // alloc: Exec
            }
        }
        for part in 0..1u32 << SPLIT_BITS {
            sp.tokens.push(Token { extra: ex as u16, part: part as u16 }); // alloc: Exec
        }
        Ok(())
    }
}

/// A split file's partitions: a stored build's rows ride behind its entries' (`file_stored`).
unsafe fn split_parts(region: *const u8) -> u32 {
    (1 << SPLIT_BITS) << (table_kind(region) == TABLE_BUILD && RowStore::of(region).is_some()) as u32
}

/// The dense rows at the front of a live table region: `(base, bytes)` for a `DENSE` record table
/// (records `0..len`), `None` for a hashed one.
/// # Safety
/// `region` is a live table region.
/// V4R-136: the rows of a table `dense_rows` accepts: a hash table's records, a sort run's rows.
/// # Safety
/// As `dense_rows`.
unsafe fn table_rows(region: *mut u8) -> u64 {
    match table_kind(region) {
        TABLE_AGG => (*(region as *const HashAgg)).len as u64,
        _ => (*(region as *const SortRun)).len as u64,
    }
}

/// The rows' shape of the table at `region` as its put lays them: a sort run's fixed head (`row_bytes`), or with its var
/// bytes the layout the kernel declared (`declared`, `SPILL_RUN_SHAPE`) when its head agrees; `None` for any other table.
/// # Safety
/// `region` is a live table region.
unsafe fn shape_of(region: *const u8, declared: Option<&runcodec::RowShape>) -> Option<runcodec::RowShape> {
    if table_kind(region) != TABLE_SORT {
        return None;
    }
    let r = &*(region as *const SortRun);
    if r.var_bytes() == 0 {
        return runcodec::RowShape::new(r.row_bytes, None);
    }
    declared.filter(|d| d.head == r.row_bytes && d.var.is_some()).cloned()
}

pub unsafe fn dense_rows(region: *mut u8) -> Option<(*const u8, usize)> {
    match table_kind(region) {
        TABLE_AGG => {
            let t = &*(region as *const HashAgg);
            (t.mode == gravity_abi::breaker::DENSE || t.len == 0).then(|| (region.add(HASHAGG_HDR) as *const u8, t.len as usize * t.layout.size() as usize))
        }
        TABLE_SORT => {
            let r = &*(region as *const SortRun);
            Some((region.add(SORTRUN_HDR) as *const u8, r.dense_bytes()))
        }
        _ => None,
    }
}

/// # Safety
/// `region` is a live table region.
pub unsafe fn clear_region(region: *mut u8) {
    match table_kind(region) {
        TABLE_AGG => (*(region as *mut HashAgg)).clear(),
        TABLE_BUILD => {
            (*(region as *mut HashBuild)).clear();
            if let Some(s) = RowStore::of_mut(region) {
                s.clear();
            }
        }
        TABLE_SORT => (*(region as *mut SortRun)).clear(),
        TABLE_T1 => (*(region as *mut T1)).clear(),
        _ => {}
    }
}

/// Scatter a live table region into one epoch of `file` by hash bits `shift..shift + bits`. `owner`: the slot's ledger owner
/// (its stream's), whose a scatter's working memory is (`None`: no one's).
/// # Safety
/// `region` is a live table region.
pub unsafe fn scatter_region(region: *mut u8, shift: u8, bits: u8, file: &mut dyn SpillFile, owner: Option<memledger::Owner>) -> Result<u64> {
    match table_kind(region) {
        TABLE_AGG => scatter(&mut *(region as *mut HashAgg), shift, bits, file),
        TABLE_BUILD => scatter_build(&mut *(region as *mut HashBuild), shift, bits, file, owner),
        TABLE_T1 => scatter_t1(region as *mut T1, shift, bits, file),
        k => Err(Error::host(if k == 0 { "gravity: spill of an empty region" } else { "gravity: spill of an unknown table" })),
    }
}

/// Refill partition `part` of `file` into the table at `region`; `on_full` scatters and clears a full table; `rows_base` the partition count the file was opened at (a build's row-store slices ride behind it).
/// # Safety
/// `region` is a live table region.
pub unsafe fn refill_region(region: *mut u8, part: u32, file: &dyn SpillFile, rows_base: u32, on_full: &mut dyn FnMut(*mut u8) -> Result<()>, overflow: &mut Option<usize>) -> Result<u64> {
    match table_kind(region) {
        TABLE_AGG => refill(&mut *(region as *mut HashAgg), part, file, on_full, overflow),
        TABLE_BUILD => refill_build(&mut *(region as *mut HashBuild), part, file, rows_base, on_full, overflow),
        _ => Err(Error::host("gravity: refill into an empty region")),
    }
}

/// Counting sort of `n` dense records at `base` by `part_of` (cycle-leader, no allocation), then one epoch.
unsafe fn scatter_dense(step: &mut Step, base: *mut u8, rec: usize, n: usize, bits: u8, part_of: &dyn Fn(*const u8) -> u32, file: &mut dyn SpillFile) -> Result<u64> {
    let starts = sort_dense(step, base, rec, n, bits, part_of)?;
    write_sorted(base, rec, bits, &starts, file)?;
    Ok(n as u64)
}

/// The in-place counting sort of `scatter_dense`: returns each partition's first record (and the end). D3966 (C9): each
/// pass ticks `step` per `cancelpoll::BATCH` records (a record is one hash and a move, a few ns: a `batch` loop; a
/// scatter's sort held a cancel 1.3 s); a canceled sort leaves the records permuted, which the canceled statement never
/// reads as a table again.
unsafe fn sort_dense(step: &mut Step, base: *mut u8, rec: usize, n: usize, bits: u8, part_of: &dyn Fn(*const u8) -> u32) -> Result<[u32; MAX_PARTS + 1]> {
    let parts = 1usize
        .checked_shl(u32::from(bits))
        .filter(|&p| p <= MAX_PARTS)
        .unwrap_or_else(|| gravity_abi::limits::exceeded(format_args!("at most {MAX_PARTS} spill partitions, 2^{bits} asked")));
    let at = |i: usize| base.add(i * rec);
    let mut counts = [0u32; MAX_PARTS];
    for i in 0..n {
        step.batch(i)?;
        counts[part_of(at(i)) as usize] += 1;
    }
    let mut starts = [0u32; MAX_PARTS + 1];
    // bounded("small-fixed: one add per partition, at most MAX_PARTS (2048)")
    for p in 0..parts {
        starts[p + 1] = starts[p] + counts[p];
    }
    let mut next = starts;
    let mut moved = 0usize;
    for p in 0..parts {
        while next[p] < starts[p + 1] {
            moved += 1;
            step.batch(moved)?;
            let i = next[p] as usize;
            let part = part_of(at(i)) as usize;
            if part == p {
                next[p] += 1;
                continue;
            }
            let j = next[part] as usize;
            next[part] += 1;
            // D327: in place, of any width (a wide record, `LAYOUT_WIDE`, is past the old fixed 256 B buffer); `i != j`
            core::ptr::swap_nonoverlapping(at(i), at(j), rec);
        }
    }
    Ok(starts)
}

/// D3966 (C9): a table's compaction pass before its scatter: `keep(i, n)` moves slot `i` down to `n` when it is used (true)
/// over every slot below `cap`, `step` ticked per `cancelpoll::BATCH` slots (a slot is one test and a move, a few ns: a
/// `batch` loop; a 3.7 s gap on memtest's ladder: a multi-GB table faulted through under reclaim). Returns the slots kept;
/// a canceled pass leaves the table half compacted, which the canceled statement never reads as a table again.
fn compact(step: &mut Step, cap: usize, mut keep: impl FnMut(usize, usize) -> bool) -> Result<usize> {
    let mut n = 0usize;
    for i in 0..cap {
        step.batch(i)?;
        if keep(i, n) {
            n += 1;
        }
    }
    Ok(n)
}

/// The sorted records as one epoch, a partition's run each.
unsafe fn write_sorted(base: *mut u8, rec: usize, bits: u8, starts: &[u32; MAX_PARTS + 1], file: &mut dyn SpillFile) -> Result<()> {
    let at = |i: usize| base.add(i * rec);
    let parts = 1usize << bits;
    let mut epoch: [(u32, &[u8]); MAX_PARTS] = [(0, &[]); MAX_PARTS];
    let mut ne = 0usize;
    // bounded("small-fixed: one slice of the sorted records per partition, at most MAX_PARTS (2048); the one epoch write is after it")
    for p in 0..parts {
        let (lo, hi) = (starts[p] as usize, starts[p + 1] as usize);
        if lo == hi {
            continue;
        }
        epoch[ne] = (p as u32, core::slice::from_raw_parts(at(lo), (hi - lo) * rec));
        ne += 1;
    }
    file.write_epoch(&epoch[..ne])
}

// The rows a stored split writes per call (the cost home's chosen capacity).
use gravity_program::cost::SPILL_SCATTER_CHUNK as SCATTER_CHUNK;
/// The row bytes one marker covers at most: a refill loads a partition an epoch at a time, so an epoch fits the part phase's table.
pub const EPOCH_BYTES: u32 = 4 << 10;

/// A stored build's entries and rows scattered by hash bits `shift..shift + bits`: the entries sorted by partition, then by row;
/// per partition its rows (the store's slice as it stands at the store's own window, else copied out in entry order) and its
/// entries in epochs of at most `EPOCH_BYTES` of rows, a marker before each (the epoch's row bytes; its entries' count in `key`),
/// each entry's payload its row's offset in its epoch — the stream `refill_build` loads an epoch at a time and splits between.
/// CONVERSION WAVE 2: the entry stream and the sort scratch (16 B an entry each: the scatter's working memory, sized by the
/// build's entries) are charged to `owner`, the build slot's ledger owner (`HeapClaim`: the step-5 pattern of an exact Vec
/// and a door-1 charge of its capacity, dropped with it), so they leave the account's unattributed row.
unsafe fn scatter_stored(step: &mut Step, build: &mut HashBuild, n: usize, store: &RowStore, shift: u8, bits: u8, part_of: &dyn Fn(*const u8) -> u32, file: &mut dyn SpillFile, owner: Option<memledger::Owner>) -> Result<u64> {
    use gravity_abi::breaker::HashBuildEntry;
    let own = shift == 0 && bits == store.bits();
    let base = build.entry_mut(0) as *mut HashBuildEntry as *mut u8;
    let starts = sort_dense(step, base, HASHBUILD_ENTRY, n, bits, part_of)?;
    let es = core::slice::from_raw_parts_mut(base as *mut HashBuildEntry, n);
    let row = |off: i64| {
        let p = store.at_ptr(off as u32);
        core::slice::from_raw_parts(p, (p as *const u64).read_unaligned() as usize * 8)
    };
    let parts = 1usize << bits;
    let mut stream: Vec<HashBuildEntry> = Vec::with_capacity(n + parts); // alloc: Exec charged owner
    let mut claim = HeapClaim::of(owner, memledger::Kind::HashBuild, &stream);
    let mut ranges = [(0usize, 0usize); MAX_PARTS];
    let mut buf: Vec<u8> = Vec::with_capacity(if own { 0 } else { SCATTER_CHUNK }); // alloc: Exec
    // C9: a partition's entries in row order through the polled sort (`cancelpoll::sort_by`), whose merge takes a scratch
    // of the largest partition's entries once that passes one sort run
    let widest = (0..parts).map(|q| (starts[q + 1] - starts[q]) as usize).max().unwrap_or(0);
    let mut scratch: Vec<HashBuildEntry> = if widest > cancelpoll::SORT_RUN { vec![HashBuildEntry::default(); widest] } else { Vec::new() }; // alloc: Exec charged owner
    let _scratch_claim = HeapClaim::of(owner, memledger::Kind::HashBuild, &scratch);
    for q in 0..parts {
        let seg = &mut es[starts[q] as usize..starts[q + 1] as usize];
        if seg.is_empty() {
            continue;
        }
        cancelpoll::sort_by(step, seg, &mut scratch, |a, b| (a.payload as u32).cmp(&(b.payload as u32)))?;
        let first = if own { store.part_range(q as u16).0 + SLICE_HDR as u32 } else { 0 };
        let s0 = stream.len();
        let (mut marker, mut from, mut at) = (s0, 0u32, 0u32);
        stream.push(HashBuildEntry::default());
        // a row's copy and its entry: tens of ns, a `batch` loop
        for (j, e) in seg.iter().enumerate() {
            step.batch(j)?;
            let r = row(e.payload);
            let rel = if own { (e.payload as u32).wrapping_sub(first) } else { at };
            if rel - from + r.len() as u32 > EPOCH_BYTES && stream.len() > marker + 1 {
                stream[marker].payload = (rel - from) as i64;
                stream[marker].key = (stream.len() - marker - 1) as i32;
                marker = stream.len();
                from = rel;
                stream.push(HashBuildEntry::default());
            }
            if !own {
                if !buf.is_empty() && buf.len() + r.len() > SCATTER_CHUNK {
                    file.write_epoch(&[((parts + q) as u32, &buf)])?;
                    buf.clear();
                }
                buf.extend_from_slice(r);
                at += r.len() as u32;
            }
            stream.push(HashBuildEntry { payload: (rel - from) as i64, ..*e });
        }
        let end = if own { (store.part_bytes(q as u16).len() - SLICE_HDR) as u32 } else { at };
        stream[marker].payload = (end - from) as i64;
        stream[marker].key = (stream.len() - marker - 1) as i32;
        ranges[q] = (s0, stream.len());
        // an epoch marker past the `n + parts` the stream was sized for grew it: the claim follows its capacity
        claim.fit(&stream);
        if own {
            file.write_epoch(&[((parts + q) as u32, &store.part_bytes(q as u16)[SLICE_HDR..])])?;
        } else if !buf.is_empty() {
            file.write_epoch(&[((parts + q) as u32, &buf)])?;
            buf.clear();
        }
    }
    let bytes = core::slice::from_raw_parts(stream.as_ptr() as *const u8, stream.len() * HASHBUILD_ENTRY);
    let mut epoch: [(u32, &[u8]); MAX_PARTS] = [(0, &[]); MAX_PARTS];
    let mut ne = 0usize;
    // bounded("small-fixed: one slice of the entry stream per partition, at most MAX_PARTS (2048); the one epoch write is after it")
    for (q, &(lo, hi)) in ranges.iter().enumerate().take(parts) {
        if lo < hi {
            epoch[ne] = (q as u32, &bytes[lo * HASHBUILD_ENTRY..hi * HASHBUILD_ENTRY]);
            ne += 1;
        }
    }
    if ne > 0 {
        file.write_epoch(&epoch[..ne])?;
    }
    Ok(n as u64)
}

/// CONVERSION WAVE 2 (M7-SPILL's stage): one owner's stage buffers, one per partition of its stored file, with their
/// capacity's claim (`StageClaim`), grown and shrunk on the owner's own thread with its buffers. The claim is declared
/// first: its drop moves the buffers' bytes to this thread's account, where their frees land next.
#[derive(Default)]
struct Stage {
    claim: StageClaim,
    bufs: Vec<Vec<u8>>,
}

impl Stage {
    /// Emptied at the stream's `clear`: the claim given back first, then the buffers freed.
    fn clear(&mut self) {
        self.claim = StageClaim::default();
        self.bufs.clear();
    }
}

/// CONVERSION WAVE 2: one owner's stage buffers' capacity, charged to the slot's owner through door 1 (a stage is the
/// stream's: the breaker's spill staging). Moved only on the thread that grows or frees the stage (the owner's own, and
/// the leader at the stream's `clear`, where the drop gives the bytes back with the buffers).
#[derive(Default)]
pub(crate) struct StageClaim(Option<memledger::Charge>);

impl StageClaim {
    /// The stage's capacity went `from` -> `to` bytes on this thread: a growth committed to `owner` (no owner: none), a
    /// shrink freed from the claim.
    fn moved(&mut self, owner: Option<memledger::Owner>, from: usize, to: usize) {
        if to > from {
            if self.0.is_none() {
                let Some(o) = owner else { return };
                self.0 = Some(memledger::Charge::open(o, memledger::Release::Statement, memledger::Kind::Spill, memledger::charge::Door::Heap, 0));
            }
            if let Some(c) = self.0.as_mut() {
                c.commit((to - from) as u64);
            }
        } else if let Some(c) = self.0.as_mut() {
            c.free((from - to) as u64);
        }
    }

    /// A written stage kept for the next puts, emptied; one a whole put grew past `2 * STAGE` dropped instead (its
    /// capacity would otherwise stay to the life's end: the partition's next puts stage at most `STAGE` again).
    fn kept(mut b: Vec<u8>) -> Vec<u8> {
        if b.capacity() > 2 * STAGE {
            return Vec::new(); // alloc: Exec
        }
        b.clear();
        b
    }
}

/// CONVERSION WAVE 2 (the step-5 pattern of `runcodec::coded_copy` / `fence::Fences`): a Vec's heap capacity charged to its
/// owner through door 1, by construction (the owner is the caller's slot owner, never the running thread's). Made right
/// after the Vec's allocation and `fit` right after a growth, on the thread that allocated; declared after its Vec, so it
/// drops first. `None`: no one's (the bytes stay where the allocator counted them). The second field: the bytes it covers
/// beside the Vec's (`beside`), which `fit` keeps.
pub(crate) struct HeapClaim(Option<memledger::Charge>, u64);

impl HeapClaim {
    pub(crate) fn of<T>(owner: Option<memledger::Owner>, kind: memledger::Kind, v: &Vec<T>) -> HeapClaim {
        HeapClaim(owner.map(|o| memledger::Charge::open(o, memledger::Release::Statement, kind, memledger::charge::Door::Heap, Self::bytes(v))), 0)
    }

    /// The claim also covers `n` bytes of a block this thread allocates beside the Vec's now (the shared block around it).
    pub(crate) fn beside(mut self, n: u64) -> HeapClaim {
        if let Some(c) = self.0.as_mut() {
            c.commit(n);
        }
        self.1 += n;
        self
    }

    fn bytes<T>(v: &Vec<T>) -> u64 {
        (v.capacity() * core::mem::size_of::<T>()) as u64
    }

    /// The claim raised to `v`'s capacity now, and what it covers beside (a growth reallocated it: the new block is on
    /// this thread's account).
    pub(crate) fn fit<T>(&mut self, v: &Vec<T>) {
        if let Some(c) = self.0.as_mut() {
            let (now, had) = (Self::bytes(v) + self.1, c.committed());
            if now > had {
                c.commit(now - had);
            }
        }
    }
}

/// Scatter a record table by its records' route bits `shift..shift + bits` (the KeyHash route, below the `shift` bits its rows consumed): compacted, sorted, one epoch, the table cleared. Returns the records written.
pub fn scatter(table: &mut HashAgg, shift: u8, bits: u8, file: &mut dyn SpillFile) -> Result<u64> {
    let rec = table.layout.size() as usize;
    let cap = table.cap as usize;
    let mut step = cancelpoll::step("spill scatter: a table compacted and sorted by partition");
    let n = compact(&mut step, cap, |i, n| {
        let r = table.record(i);
        if !r.used() {
            return false;
        }
        if i != n {
            table.record(n).copy_from(&r);
            r.clear();
        }
        true
    })?;
    let nkeys = table.layout.partition_keys();
    let at = gravity_abi::hash::Depth::part(shift as u32).expect("a spill window inside the route bits");
    let part_of = |p: *const u8| unsafe { gravity_abi::breaker::hash_at(p, nkeys) }.home(bits as u32, at) as u32;
    let base = table.record(0).as_ptr();
    let written = unsafe { scatter_dense(&mut step, base, rec, n, bits, &part_of, file) }?;
    table.clear();
    Ok(written)
}

/// The flush of a phase-1 table (`partitioned-record.md` §3.7): the table drained into its buffers,
/// every buffer's blocks written as one epoch per batch in partition order (a chain is consecutive
/// entries of its partition), the pool reset — the buffers' own partition count, never re-hashed.
/// # Safety
/// `this` is a live phase-1 table.
pub unsafe fn scatter_t1(this: *mut T1, shift: u8, bits: u8, file: &mut dyn SpillFile) -> Result<u64> {
    let t = &*this;
    if shift != 0 || bits != t.pbits {
        return Err(Error::host("gravity: a phase-1 table spills at its buffers' partition count"));
    }
    let mut written = 0u64;
    // C9: a pool's drain, then each of its blocks, a tick (a block is a pool's unit: an epoch slot, its write past MAX_PARTS)
    let mut step = cancelpoll::step("spill scatter: a phase-1 pool's blocks written");
    loop {
        step.tick()?;
        let drained = T1::drain(this);
        let t = &*this;
        let mut epoch: [(u32, &[u8]); MAX_PARTS] = [(0, &[]); MAX_PARTS];
        let mut ne = 0usize;
        let rec = t.layout.size() as usize;
        for p in 0..t.parts() as u32 {
            for b in t.chain(p) {
                step.tick()?;
                if b.recs == 0 {
                    continue;
                }
                if ne == MAX_PARTS {
                    file.write_epoch(&epoch[..ne])?;
                    ne = 0;
                }
                epoch[ne] = (p, core::slice::from_raw_parts(b.ptr, b.recs * rec));
                ne += 1;
            }
        }
        if ne > 0 {
            file.write_epoch(&epoch[..ne])?;
        }
        written += t.buffered;
        (*this).reset_pool();
        if drained {
            return Ok(written);
        }
    }
}

/// spillopt #5 (hybrid T1): the flushes after which a phase-1 table spills whole pools again (`scatter_t1`): past them the
/// table's input is past two pools, so the part a destaging flush keeps resident is a rounding of what it writes, while each
/// destaging flush moves kept blocks (`T1::destage`) and comes twice as often. MEASURED (box 67, 3 takes, the aggregate
/// spill cells): at 4, an input of ~20 pools (a count distinct at a 123 MiB grant) ran 5% slower; inputs of 1.1-1.3 pools
/// wrote half the bytes and ran 15-20% faster.
pub const DESTAGE_FLUSHES: u64 = 2;

/// spillopt #5 (hybrid T1, dynamic destaging): a kernel's full phase-1 pool (the table drained into it) writes only some
/// partitions' chains: the ones it wrote before (`spilled`, their records since), then the longest others, until at most
/// half the pool's blocks stay; those partitions stay in memory (`T1::destage` moves their blocks down) and the owner phase
/// folds them from memory, so a table whose input passes its pool by less than a pool writes about that excess, not whole
/// pools. The written partitions first, as a hybrid hash join keeps its spilled partitions spilled: the kept ones then stay
/// whole in memory and the owner phase folds them without a file read (MEASURED, box 67, 3 takes: the longest chains alone
/// ran 3-7% slower at 1.7 and 3+ pools than this order). A partition's chain is always written whole (an oldest-first
/// layout reads a partition's file before its chain).
/// Past `DESTAGE_FLUSHES` flushes, or when every partition is spilled, the whole pool (`scatter_t1`). `spilled` grows by
/// the partitions written. Returns the records written.
/// # Safety
/// `this` is a live phase-1 table with no appender open.
pub unsafe fn scatter_t1_destaging(this: *mut T1, file: &mut dyn SpillFile, spilled: &mut Vec<u64>) -> Result<u64> {
    if (*this).flushes >= DESTAGE_FLUSHES {
        return scatter_t1(this, 0, (*this).pbits, file);
    }
    let parts = (*this).parts();
    spilled.resize(parts.div_ceil(64), 0); // alloc: Exec
    let mut map: Vec<u32> = Vec::new(); // alloc: Exec
    let mut cand: Vec<(u32, u32)> = Vec::with_capacity(parts); // alloc: Exec
    let mut written = 0u64;
    // C9: as `scatter_t1`, a tick per drain, per partition's chain walked and per block written
    let mut step = cancelpoll::step("spill scatter: a phase-1 pool's blocks destaged");
    loop {
        step.tick()?;
        let drained = T1::drain(this);
        let t = &*this;
        let rec = t.layout.size() as usize;
        let mut out = spilled.clone(); // alloc: Exec
        let is_out = |o: &[u64], p: usize| o[p / 64] >> (p % 64) & 1 != 0;
        // the kept partitions' blocks, the longest chains written first until at most half the pool stays
        cand.clear();
        let mut kept = 0u64;
        for p in 0..parts {
            step.tick()?;
            let n = t.chain(p as u32).count() as u32;
            if n != 0 && !is_out(&out, p) {
                kept += n as u64;
                cand.push((n, p as u32));
            }
        }
        // bounded("small-fixed: one candidate per partition, at most MAX_PARTS (2048)")
        cand.sort_unstable_by(|a, b| b.cmp(a));
        // bounded("small-fixed: one candidate per partition, at most MAX_PARTS (2048), a test and a bit set each")
        for &(n, p) in cand.iter() {
            if 2 * kept <= t.nblocks as u64 {
                break;
            }
            out[p as usize / 64] |= 1 << (p % 64);
            kept -= n as u64;
        }
        let mut epoch: [(u32, &[u8]); MAX_PARTS] = [(0, &[]); MAX_PARTS];
        let mut ne = 0usize;
        for p in (0..parts).filter(|&p| is_out(&out, p)) {
            for b in t.chain(p as u32) {
                step.tick()?;
                if b.recs == 0 {
                    continue;
                }
                if ne == MAX_PARTS {
                    file.write_epoch(&epoch[..ne])?;
                    ne = 0;
                }
                epoch[ne] = (p as u32, core::slice::from_raw_parts(b.ptr, b.recs * rec));
                ne += 1;
                written += b.recs as u64;
            }
        }
        if ne > 0 {
            file.write_epoch(&epoch[..ne])?;
        }
        map.resize(t.nblocks as usize, 0); // alloc: Exec
        T1::destage(this, &out, &mut map);
        *spilled = out;
        if drained {
            return Ok(written);
        }
    }
}

/// Scatter a build's entries by the key's hash bits `shift..shift + bits`, then the build cleared. A build with a row store
/// writes, per partition, a marker entry (`used` 0, `payload` the epoch's row bytes), its entries with their payloads made
/// relative to the epoch's first row, then the rows (no origin word) behind `1 << bits`: a refill appends every epoch's
/// rows in order and rebases each epoch's entries by the bytes before it, however many epochs one owner's file holds.
/// `owner`: the build slot's ledger owner, whose a stored build's entry stream and sort scratch are (`scatter_stored`).
pub fn scatter_build(build: &mut HashBuild, shift: u8, bits: u8, file: &mut dyn SpillFile, owner: Option<memledger::Owner>) -> Result<u64> {
    let store = unsafe { RowStore::of_mut(build as *mut HashBuild as *mut u8) };
    let mut step = cancelpoll::step("spill scatter: a build's entries compacted and sorted by partition");
    let n = if build.table == BUILD_PARTITION {
        build.len as usize
    } else {
        compact(&mut step, build.cap as usize, |i, n| {
            if build.entry(i).used == 0 {
                return false;
            }
            if i != n {
                *build.entry_mut(n) = *build.entry(i);
                *build.entry_mut(i) = Default::default();
            }
            true
        })?
    };
    let mask = (1u32 << bits) - 1;
    let part_of = |p: *const u8| (HashBuild::key_hash(unsafe { *(p.add(4) as *const i32) }) >> shift) & mask;
    if let Some(store) = store {
        let written = unsafe { scatter_stored(&mut step, build, n, store, shift, bits, &part_of, file, owner) }?;
        store.clear();
        build.clear();
        return Ok(written);
    }
    let base = build.entry_mut(0) as *mut gravity_abi::breaker::HashBuildEntry as *mut u8;
    let written = unsafe { scatter_dense(&mut step, base, HASHBUILD_ENTRY, n, bits, &part_of, file) }?;
    build.clear();
    Ok(written)
}

/// `len` bytes of a spilled partition's rows from `rows` onto slice `slice` of `store` (one epoch's, behind its marker).
fn read_rows(rows: Option<&mut Box<dyn RunReader>>, store: &mut RowStore, slice: u16, mut len: usize) -> Result<()> {
    let Some(r) = rows else {
        return if len == 0 { Ok(()) } else { Err(Error::host("gravity: a spilled build's rows ran short of its markers")) };
    };
    let mut chunk = [0u8; 8192];
    let mut step = cancelpoll::step("spill refill: a build epoch's rows read");
    while len > 0 {
        step.tick()?;
        let want = len.min(chunk.len());
        let got = r.read(&mut chunk[..want])?;
        if got == 0 {
            return Err(Error::host("gravity: a spilled build's rows ran short of its markers"));
        }
        store.append_raw(slice, &chunk[..got]).ok_or(Error::Unsupported(UNSUPPORTED_SPILL))?;
        len -= got;
    }
    Ok(())
}

/// Room in the build at `build` for an epoch of `count` entries over `len` row bytes (a hashed table grown in its region).
unsafe fn epoch_fits(build: *mut HashBuild, slice: u16, len: usize, count: u32) -> bool {
    let b = &mut *build;
    let room = |s: &RowStore| { let (lo, hi) = s.part_range(slice); (s.cap as usize / s.parts as usize).saturating_sub((hi - lo) as usize) };
    if RowStore::of(build as *const u8).is_none_or(|s| room(s) < len) {
        return false;
    }
    if b.table != BUILD_TABLE {
        return b.len as u64 + count as u64 <= b.cap as u64;
    }
    // bounded("other: the table doubles until the epoch's entries fit, at most 32 doublings of a u32 capacity")
    while b.len as u64 + count as u64 > b.cap as u64 - b.cap as u64 / 8 {
        if !b.grow() {
            return false;
        }
    }
    true
}

/// Stream partition `part` of `file` in records of `rec` bytes through `each` (a straddling record is carried).
pub fn stream_records(file: &dyn SpillFile, part: u32, rec: usize, each: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<u64> {
    // C9: a record is a fold or a copy of a few ns to ~100 ns: a `batch` loop, its count running across the chunks
    let mut step = cancelpoll::step("spill refill: a partition's records streamed");
    let mut k = 0usize;
    stream_runs(file, part, rec, &mut |p: *const u8, n: usize| {
        for i in 0..n {
            step.batch(k)?;
            k += 1;
            // SAFETY: `n` whole records of `rec` bytes at `p`.
            each(unsafe { core::slice::from_raw_parts(p.add(i * rec), rec) })?;
        }
        Ok(())
    })
}

/// The bytes of a straddling record `stream_runs` carries on the stack: every 8-key-tier record (`u8` offsets) fits.
const CARRY_STACK: usize = 256;

/// The carry of a record straddling two chunks (D327: a fixed `[u8; 256]` panicked on a wide record, `LAYOUT_WIDE`, e.g. a
/// group's 120 sums at 1008 B): the stack's for a record of up to `CARRY_STACK` bytes, else `rec` bytes of the heap reserved
/// at the first straddle, fallibly — a refused reservation is the statement's `OutOfMemory`, never the process's abort.
struct Carry {
    stack: [u8; CARRY_STACK],
    heap: Vec<u8>,
}

impl Carry {
    fn new() -> Carry {
        Carry { stack: [0; CARRY_STACK], heap: Vec::new() } // alloc: Exec
    }

    fn buf(&mut self, rec: usize) -> Result<&mut [u8]> {
        if rec <= CARRY_STACK {
            return Ok(&mut self.stack[..rec]);
        }
        if self.heap.len() < rec {
            let pool = &crate::mem::grant::POOL;
            self.heap.try_reserve_exact(rec).map_err(|_| Error::OutOfMemory { bytes: rec as u64, held: pool.held(), capacity: pool.capacity(), why: None })?;
            self.heap.resize(rec, 0); // inside the reservation
        }
        Ok(&mut self.heap[..rec])
    }
}

/// The partition's records in runs of whole records (a chunk's, then a record straddling two chunks alone).
pub fn stream_runs(file: &dyn SpillFile, part: u32, rec: usize, each: &mut dyn FnMut(*const u8, usize) -> Result<()>) -> Result<u64> {
    let mut carry = Carry::new();
    let mut carried = 0usize;
    let mut n = 0u64;
    file.read_part(part, &mut |chunk: &[u8]| {
        let mut off = 0usize;
        if carried > 0 {
            let carry = carry.buf(rec)?;
            let take = (rec - carried).min(chunk.len());
            carry[carried..carried + take].copy_from_slice(&chunk[..take]);
            carried += take;
            off = take;
            if carried == rec {
                each(carry.as_ptr(), 1)?;
                n += 1;
                carried = 0;
            }
        }
        let whole = (chunk.len() - off) / rec;
        if whole > 0 {
            each(chunk[off..].as_ptr(), whole)?;
            n += whole as u64;
            off += whole * rec;
        }
        if off < chunk.len() {
            carry.buf(rec)?[..chunk.len() - off].copy_from_slice(&chunk[off..]);
            carried = chunk.len() - off;
        }
        Ok(())
    })?;
    assert_eq!(carried, 0, "a spill partition holds whole records");
    Ok(n)
}

/// Refill partition `part` from `file`: every record folded into `table` by key; `on_full` on a full table.
pub fn refill(table: &mut HashAgg, part: u32, file: &dyn SpillFile, on_full: &mut dyn FnMut(*mut u8) -> Result<()>, overflow: &mut Option<usize>) -> Result<u64> {
    let rec = table.layout.size() as usize;
    let layout = table.layout;
    let table_ptr = table as *mut HashAgg;
    stream_records(file, part, rec, &mut |bytes: &[u8]| {
        // SAFETY: the record view is read-only; the table is exclusively ours for the call.
        let src = unsafe { gravity_abi::breaker::Rec::at(bytes.as_ptr() as *mut u8, &layout) };
        let t = unsafe { &mut *table_ptr };
        let dst = match t.find_or_insert(src.keys()) {
            Some(d) => d,
            None => {
                *overflow = Some(overflow.unwrap_or(0) + 1);
                on_full(table_ptr as *mut u8)?;
                let t = unsafe { &mut *table_ptr };
                t.find_or_insert(src.keys()).ok_or(Error::Unsupported(UNSUPPORTED_SPILL))?
            }
        };
        dst.fold(&src).map_err(|_| Error::Raised(gravity_abi::breaker::FloatOverflow::MASK))
    })
}

/// Refill partition `part` of a build stream into `build` (appended or hashed by its mode); `on_full` on a full table. A build with a
/// row store streams its rows behind `rows_base` epoch by epoch at each marker entry (the epoch's row bytes, its entries' count in
/// `key`), the epoch's entries rebased onto where its rows landed; an epoch that does not fit splits the table first (D126).
pub fn refill_build(build: &mut HashBuild, part: u32, file: &dyn SpillFile, rows_base: u32, on_full: &mut dyn FnMut(*mut u8) -> Result<()>, overflow: &mut Option<usize>) -> Result<u64> {
    let build_ptr = build as *mut HashBuild;
    if let Some(store) = unsafe { RowStore::of(build_ptr as *const u8) } {
        let slice = if store.parts == 1 { 0 } else { part as u16 };
        let mut rows = file.open_part(rows_base + part)?;
        let (mut cur, mut markers) = (0u32, 0u64);
        let n = stream_records(file, part, HASHBUILD_ENTRY, &mut |bytes: &[u8]| {
            // SAFETY: an entry image is plain data; the build and its store are exclusively ours for the call.
            let mut e = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const gravity_abi::breaker::HashBuildEntry) };
            if e.used == 0 {
                let (len, count) = (e.payload as usize, e.key as u32);
                if !unsafe { epoch_fits(build_ptr, slice, len, count) } {
                    if unsafe { (*build_ptr).len } == 0 {
                        return Err(Error::Unsupported(UNSUPPORTED_SPILL));
                    }
                    *overflow = Some(overflow.unwrap_or(0) + 1);
                    on_full(build_ptr as *mut u8)?;
                    if !unsafe { epoch_fits(build_ptr, slice, len, count) } {
                        return Err(Error::Unsupported(UNSUPPORTED_SPILL));
                    }
                }
                let store = unsafe { RowStore::of_mut(build_ptr as *mut u8) }.expect("a stored build");
                cur = store.part_range(slice).1;
                read_rows(rows.as_mut(), store, slice, len)?;
                markers += 1;
                return Ok(());
            }
            e.payload = (e.payload as u32).wrapping_add(cur) as i64;
            if !unsafe { &mut *build_ptr }.insert_tagged(e.key, e.payload, e.used) {
                return Err(Error::Unsupported(UNSUPPORTED_SPILL));
            }
            Ok(())
        })?;
        return Ok(n - markers);
    }
    let n = stream_records(file, part, HASHBUILD_ENTRY, &mut |bytes: &[u8]| {
        // SAFETY: an entry image is plain data; the build is exclusively ours for the call.
        let e = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const gravity_abi::breaker::HashBuildEntry) };
        let b = unsafe { &mut *build_ptr };
        // full at 7/8 (the probe's contract): refilled to 100 %, every missing-key probe walked the table
        let full = |b: &HashBuild| b.table == BUILD_TABLE && b.len as u64 >= b.cap as u64 - b.cap as u64 / 8;
        if (full(b) || !b.insert_tagged(e.key, e.payload, e.used)) && !(b.grow() && b.insert_tagged(e.key, e.payload, e.used)) {
            *overflow = Some(overflow.unwrap_or(0) + 1);
            on_full(build_ptr as *mut u8)?;
            let b = unsafe { &mut *build_ptr };
            if !b.insert_tagged(e.key, e.payload, e.used) {
                return Err(Error::Unsupported(UNSUPPORTED_SPILL));
            }
        }
        Ok(())
    })?;
    Ok(n)
}

/// # Safety
/// `region` points at least `HASHAGG_HDR` readable bytes.
pub unsafe fn region_live(region: *const u8, bytes: usize) -> bool {
    bytes >= HASHAGG_HDR && table_kind(region) != 0 && *(region as *const u32) != 0
}

/// The records a live table region holds (a phase-1 table: its table's and its buffers').
/// # Safety
/// `region` is a live table region.
pub unsafe fn region_len(region: *const u8) -> u32 {
    if table_kind(region) == TABLE_T1 {
        return (*(region as *const T1)).held().min(u32::MAX as u64) as u32;
    }
    *(region as *const u32).add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3966: the scatter's in-place sort ticks its step every `cancelpoll::BATCH` records: canceled, it stops with the
    /// cancel; outside a canceled kernel call it sorts every record into its partition as before.
    #[test]
    fn d3966_a_canceled_scatter_sort_stops_at_its_next_poll() {
        let n = 3 * cancelpoll::BATCH;
        let mut recs: Vec<u64> = (0..n as u64).map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15)).collect(); // alloc: Cold
        let part_of = |p: *const u8| unsafe { (p as *const u64).read() as u32 & 7 };
        let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
        let base = recs.as_mut_ptr() as *mut u8;
        let got = cancelpoll::scoped(&byte, || unsafe { sort_dense(&mut cancelpoll::step("test sort"), base, 8, n, 3, &part_of) });
        assert!(matches!(got, Err(Error::Cancelled)), "a canceled sort stops");
        let starts = unsafe { sort_dense(&mut cancelpoll::step("test sort"), base, 8, n, 3, &part_of) }.expect("no cancel outside a kernel call");
        for p in 0..8 {
            assert!(recs[starts[p] as usize..starts[p + 1] as usize].iter().all(|&r| r as u32 & 7 == p as u32), "partition {p}");
        }
        assert_eq!(starts[8] as usize, n);
    }

    /// C6 (e): a partition count past `MAX_PARTS` raises through `gravity_abi::limits::exceeded` (PostgreSQL's 54000 as a
    /// `PgError` payload), never a plain panic, which aborts the server since D2933.
    #[test]
    fn c6e_spill_partitions_past_max_parts_raise_not_a_plain_panic() {
        let mut recs = [0u64; 4];
        let base = recs.as_mut_ptr() as *mut u8;
        let bits = MAX_PARTS.trailing_zeros() as u8 + 1;
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { sort_dense(&mut cancelpoll::step("test sort"), base, 8, 4, bits, &|_| 0) }))
            .expect_err("past MAX_PARTS raises");
        assert!(payload.downcast_ref::<std::string::String>().is_none() && payload.downcast_ref::<&str>().is_none(), "a plain panic");
        let at_max = MAX_PARTS.trailing_zeros() as u8;
        let starts = unsafe { sort_dense(&mut cancelpoll::step("test sort"), base, 8, 4, at_max, &|_| 0) }.expect("MAX_PARTS itself sorts");
        assert_eq!(starts[MAX_PARTS] as usize, 4);
    }

    /// D3966: a table's compaction pass (`scatter`, `scatter_build`) ticks its step every `cancelpoll::BATCH` slots:
    /// canceled, it stops at its first tick; outside a canceled kernel call it compacts every used slot down.
    #[test]
    fn d3966_a_canceled_compaction_pass_stops_at_its_next_poll() {
        let cap = 3 * cancelpoll::BATCH;
        let mut slots: Vec<u32> = (0..cap as u32).map(|i| if i % 3 == 0 { i + 1 } else { 0 }).collect(); // alloc: Cold
        let keep = |slots: &mut Vec<u32>, i: usize, n: usize| {
            if slots[i] == 0 {
                return false;
            }
            (slots[n], slots[i]) = (slots[i], if i != n { 0 } else { slots[i] });
            true
        };
        let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
        let mut seen = 0usize;
        let got = cancelpoll::scoped(&byte, || {
            compact(&mut cancelpoll::step("test compaction"), cap, |i, n| {
                seen = i + 1;
                keep(&mut slots, i, n)
            })
        });
        assert!(matches!(got, Err(Error::Cancelled)), "a canceled pass stops");
        assert_eq!(seen, cancelpoll::BATCH - 1, "at its first tick");
        let n = compact(&mut cancelpoll::step("test compaction"), cap, |i, n| keep(&mut slots, i, n)).expect("no cancel outside a kernel call");
        assert_eq!(n, cap.div_ceil(3));
        assert!(slots[..n].iter().all(|&v| v != 0) && slots[n..].iter().all(|&v| v == 0), "the used slots compacted down");
    }

    /// DR-17: the re-split depth is the hash's route bits, not a count: from the widest generation-0 fan-out (`MAX_PARTS`)
    /// a partition still splits past the old fourth generation, and stops only where a split would read past the route bits.
    #[test]
    fn a_partition_splits_while_the_hash_has_route_bits_left() {
        let bits0 = MAX_PARTS.trailing_zeros() as u8;
        let mut shift = bits0;
        let mut generations = 1u32;
        while may_split(shift, 0) {
            shift += SPLIT_BITS;
            generations += 1;
        }
        assert!(generations > 4, "{generations} generations from {bits0} partition bits");
        assert!(shift as u32 <= gravity_abi::hash::ROUTE_BITS && shift as u32 + SPLIT_BITS as u32 > gravity_abi::hash::ROUTE_BITS);
        assert!(!may_split(0, u16::MAX as usize), "a split past the token's extra index is refused");
    }

    /// D4439 on LEDGER2 (design A): a temp map's not-resident bytes book on its charge's owner as on its account, and a move
    /// to a breaker slot's operator (`own_by`) takes them along, as a mapped region's do (region.rs
    /// `ledger2_a_regions_not_resident_bytes_follow_its_owner`); they leave with the map.
    #[cfg(target_os = "linux")]
    #[test]
    fn d4439_a_temp_maps_not_resident_bytes_follow_its_owner() {
        use std::os::fd::AsRawFd;
        const MIB: usize = 1 << 20;
        const M: i64 = MIB as i64;
        std::thread::spawn(|| {
            memledger::bind_backend(3996, 9996); // a backend slot no other test binds (region.rs's ledger2 test: 3995)
            memledger::statement_begin();
            let st = memledger::charge::context().0;
            let op = memledger::charge::op_begin(st, memledger::Kind::HashAgg as u32, 0);
            let path = std::env::temp_dir().join(format!("d4439-own-by-{}", std::process::id())); // alloc: Cold
            let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("a temp file");
            let _ = std::fs::remove_file(&path);
            // a sparse file: none of its pages is in the page cache
            f.set_len(64 * MIB as u64).expect("the file's length");
            let mut m = TempMap::shared(f.as_raw_fd(), 64 * MIB).expect("a temp map");
            drop(f);
            assert_eq!(m.charge.owner(), st, "made on the running statement");
            // SAFETY: the map's 64 MiB are readable and writable (`TempMap::shared`)
            unsafe { core::ptr::write_bytes(m.as_mut_ptr(), 0x5a, 16 * MIB) };
            assert!(crate::mem::resident::pass_all());
            let u = memledger::charge::usage(st);
            assert!(u.held == 64 * M && (40 * M..=48 * M).contains(&u.not_resident), "16 of 64 MiB written: {u:?}");
            let nr = u.not_resident;
            m.own_by(op);
            let (us, uo) = (memledger::charge::usage(st), memledger::charge::usage(op));
            assert_eq!((uo.held, uo.not_resident), (64 * M, nr), "the operator's now, its not-resident bytes with it");
            assert_eq!((us.held, us.not_resident, us.subtree_not_resident), (0, 0, nr), "in the statement's subtree only");
            drop(m);
            let uo = memledger::charge::usage(op);
            assert_eq!((uo.held, uo.not_resident), (0, 0), "gone with the map");
            memledger::charge::op_end(op);
            let _ = memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
