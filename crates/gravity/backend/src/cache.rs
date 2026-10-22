//! The process-shared compiled-code cache (docs/gravity/code-cache.md): one mapping per IR content
//! per server process, every backend after the first a hit.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pgsync::{Condvar, Mutex, OnceLock, RwLock};
use std::time::Instant;

use gravity_abi::hash::Mix128;
use gravity_ir::{Func, Program};
use gravity_program::Code;
use gravity_tpde::CompileTiming;
use mcx::{MemoryContext, SendRoot};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

mod raw;
use raw::codes_of;

pub const CODE_CACHE_GUC: &str = "pgrust.gravity_code_cache_mb";
const KEY_VERSION: u64 = 2;
#[cfg(target_arch = "aarch64")]
const TARGET: u64 = 0xa64;
#[cfg(target_arch = "x86_64")]
const TARGET: u64 = 0x64;
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const TARGET: u64 = 0;

/// The content key of a compiled IR program (code-cache.md §1): the whole `gravity_ir::Program`
/// (functions, blocks, instructions, constants, types, hints, host names / signatures / ADDRESSES /
/// flags), the target, `KEY_VERSION`. Not the entry: one module compile serves every function (D230).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CodeKey(pub u128);

pub fn key_of(program: &Program) -> CodeKey {
    let mut h = Mix128::new();
    h.word(KEY_VERSION);
    h.word(TARGET);
    program.hash(&mut h);
    CodeKey(h.key128())
}

/// One cached mapping: immutable, executable, shared by `Arc`; the mapper's tables in the entry's own root context (`root`,
/// an mcx `SendRoot`: parentless, in no thread's root forest, dropped by the last `Arc` on whichever thread) (code-cache.md §2).
pub struct Entry {
    /// FIRST (D4762): a filed entry's own blocks (its `Arc` block, its root's header) on the program caches, opened at the
    /// filing; it drops before those blocks are freed, so they leave on the freeing thread wherever the entry was built.
    own: memledger::charge::Charge,
    key: CodeKey,
    bytes: usize,
    timing: CompileTiming,
    last_use: AtomicU64,
    /// D4579: the program tier let go the program this module was filed for (its database dropped, a relation it read
    /// superseded): evicted once nothing holds it (`evict_doomed`); a later hit takes the mark off (`touch`).
    doomed: AtomicBool,
    /// Declared before `root`: fields drop in declaration order, the mapping before the context its tables live in.
    compiled: gravity_tpde::Compiled<'static>,
    /// C2 (G1-A's program caches) + planprod P2: the context's owner by construction. At birth the compiling statement's
    /// (`birth_owner`); a filed entry is adopted by the program caches (`set_owner(PROGRAM_CACHES)` at the filing), so its
    /// live blocks are theirs (held_ctx_named) until the drop takes them off on the dropping thread (CtxOwner::post). Its
    /// code mapping is the program caches' already (door 2). A budget-0 or lost-race entry is never adopted.
    root: SendRoot,
    /// D4762: built in the program caches' home (`compile_and_file` with no backend on the thread): its drop enters it too.
    homed: bool,
    /// LAST (D4552 (a)): the home's exit, armed by the drop when `homed`.
    home_exit: memledger::HomeExit,
}

/// D4762: a filed entry's blocks outside its root context: its `Arc` block and the root's header.
const fn own_bytes() -> u64 {
    memledger::arc_bytes::<Entry>() + SendRoot::HEADER_BYTES
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.homed {
            self.home_exit.arm(&home());
        }
    }
}

/// C8: the owner of a filed entry's context bytes.
pub(crate) const PROGRAM_CACHES: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));

/// D4762: the program caches' home. A compile with no backend on its thread (the tier-0 compile pool, `promote::run`; a
/// server-start worker) runs in it: the module's entry, its context headers, the compile's transients and the codes are
/// allocated and freed in the program caches' account, not the thread's (UNBOUND), and the entry's drop on any thread
/// comes back to it (`Entry::homed`; its root is unpooled, `new_root`, so the drop frees there all it holds). The map's
/// inserts run in it too, so its table is theirs whichever thread grows it.
pub(crate) fn home() -> memledger::Home {
    memledger::Home::of(PROGRAM_CACHES.acct())
}

impl Entry {
    pub fn key(&self) -> CodeKey {
        self.key
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn timing(&self) -> CompileTiming {
        self.timing
    }
    pub fn compiled(&self) -> &gravity_tpde::Compiled<'static> {
        &self.compiled
    }
}

/// planprod P2: the owner a compile's root context takes at birth: the compiling statement's on a backend (its session's
/// between statements), the program caches' on the tier-0 compile pool and anywhere no backend runs (the pool files what
/// it compiles). A root that is never filed (a failed compile, a lost race, budget 0) drops as the statement's.
pub(crate) fn birth_owner() -> memledger::Owner {
    match memledger::charge::statement_owner() {
        o @ (memledger::Owner::Statement { .. } | memledger::Owner::Session(_)) => o,
        _ => PROGRAM_CACHES,
    }
}

/// A fresh root context for one mapping (code-cache.md §2): parentless and unobserved (an mcx `SendRoot`), so no live
/// thread's root forest (`mcxt_stats::ROOTS`) reaches it and any thread may drop it; owned at birth (`birth_owner`).
/// D4762: unpooled. Its keeper block and account node are allocated fresh and freed at its drop, never parked in the
/// dropping thread's pools: an entry the pool built in the program caches' home drops in that home on any thread, and a
/// keeper it parked there would park as the program caches' on a backend that then takes it for its own contexts and frees
/// it on its own account, so the program caches never got those bytes back.
fn new_root() -> SendRoot {
    SendRoot::new_owned_unpooled("GravityCodeCacheEntry", birth_owner())
}

/// D2909: the error of a compile stopped at a stop point (`gravity_tpde::stop_point`) by a pending interrupt; its context is
/// released like a failed compile's, nothing is filed, and the caller takes the interrupt (PostgreSQL's own error).
/// D3811: also a wait for another thread's compile of the module ended by one (`Flight::wait`): that compile goes on and
/// files its module, the waiter takes the interrupt.
pub const INTERRUPTED: &str = "gravity backend: the compile stopped for a pending interrupt";

/// D3811: the pending interrupt taken through the interrupt facade's CHECK_FOR_INTERRUPTS (`cancelpoll::interrupts::check`,
/// C6 (d)): a caller with no PostgreSQL seam of its own takes an `INTERRUPTED` here (a tier-0 promotion, lib.rs
/// `promoted`), so the interrupt is PostgreSQL's own error (57014, 57P01, ..), never a failed compile. Ok(true) when it
/// raised nothing (a catch-up); Ok(false) when no backend is installed (a test without the host: the caller then treats
/// the `INTERRUPTED` as it did before). The facade is called directly; the check the host used to install here
/// (`set_interrupt_check`) is gone.
pub fn take_interrupt() -> PgResult<bool> {
    if !cancelpoll::interrupts::installed() {
        return Ok(false);
    }
    cancelpoll::interrupts::check().map(|()| true)
}

/// The crate's error for a compile that could not finish (tpde's own `invalid`'s class, 22023): a refusal at admission.
fn backend_error(message: String) -> Box<PgError> {
    PgError::error(message).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into()
}

/// F1 (D296's census): the compile under a catch. The code generator's asserts guard properties `gravity_ir::verify`
/// leaves out (SSA dominance, an edge into a function's entry block), so a lowerer bug can reach them; a panic there is
/// the compile's `Err` (the admission refuses "backend", the fallback takes the statement), never an unwind through the
/// executor. A Postgres error or a thread exit passes through unchanged; the root drops either way, after the compile.
fn compile_caught(mut root: SendRoot, compile: impl FnOnce(&'static MemoryContext) -> PgResult<gravity_tpde::Compiled<'static>>) -> PgResult<(gravity_tpde::Compiled<'static>, SendRoot)> {
    let ctx = raw::compile_context(&mut root);
    match gravity_abi::status::unwind::catch(|| gravity_deps::catalog_free(|| compile(ctx))) {
        Ok(Ok(c)) => Ok((c, root)),
        Ok(Err(e)) => {
            drop(root);
            Err(e)
        }
        Err(p) => {
            drop(root);
            // D2962: every designed exit (types_error::is_exit_payload), not only PanicExitThread
            if p.is::<PgError>() || p.is::<Box<PgError>>() || types_error::is_exit_payload(&*p) {
                std::panic::resume_unwind(p);
            }
            if p.is::<gravity_tpde::Interrupted>() {
                return Err(backend_error(String::from(INTERRUPTED))); // alloc: Cold
            }
            let msg = p.downcast_ref::<String>().map(String::as_str).or_else(|| p.downcast_ref::<&str>().copied()).unwrap_or("(non-string payload)");
            Err(backend_error(format!("gravity backend: the code generator panicked: {msg}"))) // alloc: Cold
        }
    }
}

/// What `get_or_compile_all` did: a hit (hash + lookup), a miss compiled here, or a miss whose compile
/// lost the insert race to another thread's (its mapping dropped, the winner's returned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    Hit,
    Miss,
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub lookup: Lookup,
    pub key: CodeKey,
    pub bytes: usize,
    /// The compile's phases (zero on a hit; `gravity_tpde::set_clock` unset reads zero).
    pub timing: CompileTiming,
    /// Wall nanoseconds of the whole call (`Instant`): the hit latency, or the miss's compile + insert.
    pub ns: u64,
}

impl Outcome {
    pub fn hit(&self) -> bool {
        self.lookup == Lookup::Hit
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub lost: u64,
    /// Misses that waited for another compile of their key (`claim`) instead of compiling it again.
    pub waited: u64,
    pub evictions: u64,
    pub entries: usize,
    pub bytes: usize,
    pub budget: usize,
}

struct State {
    map: HashMap<CodeKey, Arc<Entry>>,
    /// D3168: the staged functions' IR (`file_side`), in `bytes` beside the modules.
    sides: HashMap<u64, Arc<SideIr>>,
    bytes: usize,
    budget: usize,
    /// The keys a compile is running for now (`claim`): a miss of one waits for it instead of compiling the module again.
    inflight: HashMap<CodeKey, Arc<Flight>>,
}

impl State {
    fn new(budget: usize) -> State {
        State { map: HashMap::new(), sides: HashMap::new(), bytes: 0, budget, inflight: HashMap::new() }
    }

    /// The least recently used module or staged IR out of the cache, its bytes off `bytes`: never `keep` (the module just
    /// filed) or `keep_side`; with `idle`, only one nothing but the cache holds.
    fn evict(&mut self, keep: Option<CodeKey>, keep_side: Option<u64>, idle: bool) -> Option<Gone> {
        let module = self.map.iter().filter(|(k, e)| Some(**k) != keep && (!idle || Arc::strong_count(e) == 1)).min_by_key(|(_, e)| e.last_use.load(Ordering::Relaxed)).map(|(k, e)| (*k, e.last_use.load(Ordering::Relaxed)));
        let side = self.sides.iter().filter(|(id, x)| Some(**id) != keep_side && (!idle || Arc::strong_count(x) == 1)).min_by_key(|(_, x)| x.last_use.load(Ordering::Relaxed)).map(|(id, x)| (*id, x.last_use.load(Ordering::Relaxed)));
        let gone = match (module, side) {
            (Some((k, a)), Some((_, b))) if a <= b => Gone::Module(self.map.remove(&k)?),
            (Some((k, _)), None) => Gone::Module(self.map.remove(&k)?),
            (_, Some((id, _))) => Gone::Side(self.sides.remove(&id)?),
            (None, None) => return None,
        };
        self.bytes -= gone.bytes();
        EVICTIONS.fetch_add(1, Ordering::Relaxed);
        Some(gone)
    }
}

/// What an eviction took out, dropped once the cache's lock is released where it can be.
enum Gone {
    Module(Arc<Entry>),
    Side(Arc<SideIr>),
}

impl Gone {
    fn bytes(&self) -> usize {
        match self {
            Gone::Module(e) => e.bytes,
            Gone::Side(x) => x.bytes,
        }
    }
}

/// D3168: a staged function's IR kept for its lazy compile (lib.rs `staged`), filed beside the modules, so its bytes are
/// the cache's (`stats().bytes`: the program caches' census, by which `trim_to` frees). The cache holds its one `Arc` but
/// while a compile reads it (`SideRef::get`): the ladder's trim frees it as it frees an unused module, and its plain
/// function then runs with the stage off. Its code made, or its side dropped, takes it out (`SideRef::forget`).
pub struct SideIr {
    /// FIRST (as `Entry::own`): its `Arc` block and its root's header on the program caches.
    _own: memledger::charge::Charge,
    ir: Program,
    bytes: usize,
    last_use: AtomicU64,
}

impl SideIr {
    pub fn ir(&self) -> &Program {
        &self.ir
    }
}

const fn side_own_bytes() -> u64 {
    memledger::arc_bytes::<SideIr>() + SendRoot::HEADER_BYTES
}

/// A staged function's IR as its side holds it: the cache's (`Filed`, by its id), or its own with the cache off (`Kept`).
pub enum SideRef {
    Filed(u64, std::sync::Weak<SideIr>),
    Kept(Arc<SideIr>),
}

impl SideRef {
    /// The IR, touched; `None`: the cache let it go.
    pub fn get(&self) -> Option<Arc<SideIr>> {
        let x = match self {
            SideRef::Filed(_, w) => w.upgrade()?,
            SideRef::Kept(x) => x.clone(),
        };
        x.last_use.store(CLOCK.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        Some(x)
    }

    /// Out of the cache now (its code made, its side dropped).
    pub fn forget(&self) {
        let SideRef::Filed(id, _) = self else { return };
        let gone = {
            let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
            g.as_mut().and_then(|s| {
                let x = s.sides.remove(id)?;
                s.bytes -= x.bytes;
                Some(x)
            })
        };
        drop(gone);
    }
}

static SIDE_IDS: AtomicU64 = AtomicU64::new(0);

/// `ir` (owned by the program caches) filed as a staged function's IR, evicting to the budget as a module's filing does;
/// with the cache off, kept by its side alone.
pub fn file_side(ir: Program) -> SideRef {
    let own = memledger::charge::Charge::open(PROGRAM_CACHES, memledger::charge::release_of(PROGRAM_CACHES), memledger::Kind::Region, memledger::charge::Door::Heap, side_own_bytes());
    let bytes = ir.bytes() + side_own_bytes() as usize;
    let x = Arc::new(SideIr { _own: own, ir, bytes, last_use: AtomicU64::new(CLOCK.fetch_add(1, Ordering::Relaxed)) }); // alloc: Program charged PROGRAM_CACHES (SideIr::own)
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State::new(budget_bytes())); // alloc: Cold
    if s.budget == 0 {
        return SideRef::Kept(x);
    }
    let id = SIDE_IDS.fetch_add(1, Ordering::Relaxed);
    let weak = Arc::downgrade(&x);
    s.bytes += bytes;
    file_in_home(&mut s.sides, id, x);
    while s.bytes > s.budget {
        if s.evict(None, Some(id), false).is_none() {
            break;
        }
    }
    SideRef::Filed(id, weak)
}

/// A filing's insert, in the program caches' home: the map's table is theirs whichever thread grows it.
fn file_in_home<K: Eq + Hash, V>(m: &mut HashMap<K, V>, k: K, v: V) {
    let _home = home().enter();
    m.insert(k, v); // alloc: Cold
}

/// One running compile of a key: `done` once its module is filed, or its compile failed (`land`).
#[derive(Default)]
struct Flight {
    done: Mutex<bool>,
    cv: Condvar,
}

/// D3811: how long a waiter sleeps between reads of its pending interrupt (`Flight::wait`): a cancel, a statement_timeout,
/// a terminate or a fast shutdown ends the wait within one slice.
const WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(10);

impl Flight {
    /// Until the flight lands, or (D3811) until the waiter has an interrupt to take: then `INTERRUPTED`, and the caller
    /// takes it as a stopped compile's (exec admit `backend`, lib.rs `promoted`). The wait read nothing before (a backend
    /// waiting on the compile pool's 840 s compile answered neither pg_cancel_backend, pg_terminate_backend nor a fast
    /// shutdown). The flight stays its compiler's: it lands and files its module whatever the waiter does; the waiter's
    /// `Arc` goes, nothing is left in `inflight` or the map (`land` alone removes the key).
    fn wait(&self) -> PgResult<()> {
        let mut d = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*d {
            if gravity_tpde::interrupt_pending() {
                return Err(backend_error(String::from(INTERRUPTED))); // alloc: Cold
            }
            d = self.cv.wait_timeout(d, WAIT_SLICE).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
        }
        Ok(())
    }
}

enum Claim {
    /// This caller compiles; `Some` = it holds the key's flight and lands it (`None`: the cache is off, nothing is filed).
    Compile(Option<Arc<Flight>>),
    /// Another compile of the key runs: wait for it, then look again.
    Wait(Arc<Flight>),
    /// Filed since the caller's lookup: look again.
    Filed,
}

/// Under the write lock: the key's flight to wait on, or a new one this caller compiles under.
fn claim(key: CodeKey) -> Claim {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State::new(budget_bytes())); // alloc: Cold
    if s.budget == 0 {
        return Claim::Compile(None);
    }
    if s.map.contains_key(&key) {
        return Claim::Filed;
    }
    if let Some(f) = s.inflight.get(&key) {
        return Claim::Wait(f.clone());
    }
    let f = Arc::new(Flight::default()); // alloc: Cold
    let _home = home().enter();
    s.inflight.insert(key, f.clone()); // alloc: Cold
    Claim::Compile(Some(f))
}

/// The key's compile has ended, filed or failed: its waiters look again (a failure leaves the key unfiled, never marked:
/// the next miss compiles it).
fn land(key: CodeKey, f: &Flight) {
    if let Some(s) = CACHE.write().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.inflight.remove(&key);
    }
    *f.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
    f.cv.notify_all();
}

static CACHE: RwLock<Option<State>> = RwLock::new(None);
static CLOCK: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static LOST: AtomicU64 = AtomicU64::new(0);
static WAITED: AtomicU64 = AtomicU64::new(0);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
static BUDGET: OnceLock<usize> = OnceLock::new();

/// The budget in bytes: `pgrust.gravity_code_cache_mb` read once per process through the GUC seam
/// when it is installed (the fp cache's precedent), else the cost home's share of the machine's RAM, never under the
/// reference box's (`code_cache_bytes`); 0 = the cache is off (every call compiles, nothing is kept: the uncached arm of a measurement).
pub fn budget_bytes() -> usize {
    *BUDGET.get_or_init(|| match gravity_driver::env::guc_num::<u64>(CODE_CACHE_GUC) {
        Some(mb) => (mb as usize) << 20,
        None => {
            use gravity_driver::machine::{profile, REFERENCE_RAM};
            gravity_program::cost::runtime::code_cache_bytes(profile().ram_bytes, REFERENCE_RAM) as usize
        }
    })
}

/// Set the budget for this process (tests; before or after entries exist — the next insert evicts to it).
pub fn set_budget_bytes(bytes: usize) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    match g.as_mut() {
        Some(s) => s.budget = bytes,
        None => *g = Some(State::new(bytes)), // alloc: Cold
    }
}

fn touch(e: &Entry) {
    e.last_use.store(CLOCK.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    if e.doomed.load(Ordering::Relaxed) {
        e.doomed.store(false, Ordering::Relaxed);
    }
}

/// The hit half of `get_or_compile_all` (tier 0's lookup, H16 (d)): `program`'s codes when its module is cached (touched,
/// a hit counted), else `None` with nothing counted.
pub fn lookup_all(program: &Program, entries: &[Func]) -> Option<(Vec<Code>, Outcome)> {
    hit(key_of(program), entries)
}

/// `lookup_all` under `key`, the program's `key_of` taken once by the caller (D3886, lib.rs `mapped_cached_keyed`).
pub fn lookup_keyed(key: CodeKey, entries: &[Func]) -> Option<(Vec<Code>, Outcome)> {
    hit(key, entries)
}

/// The cached module's codes (`ns` 0: the caller times it).
fn hit(key: CodeKey, entries: &[Func]) -> Option<(Vec<Code>, Outcome)> {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let e = g.as_ref().and_then(|s| s.map.get(&key))?;
    touch(e);
    HITS.fetch_add(1, Ordering::Relaxed);
    Some((codes_of(e, entries), Outcome { lookup: Lookup::Hit, key, bytes: e.bytes, timing: CompileTiming::default(), ns: 0 }))
}

/// `Code::mapped` for each of `program`'s `entries` from the cache, all from one mapping: the hit path is the key (`key_of`)
/// and one read-locked lookup; a miss compiles the module once outside any lock (`compile`, into `new_root`) and inserts
/// under the write lock — the first insert of a key wins, a later one is `Lost`. LRU by bytes to the budget at insert; an
/// evicted entry lives on in every `Code` that holds it. The caller checked the entries' signatures (`crate::check_entry`).
///
/// A miss whose key another compile is running for (a backend's, or tier 0's compile pool's, `crate::promote`) waits for
/// it and takes its module, so one key compiles once at a time; when that compile failed, the waiter compiles itself. A
/// waiter with an interrupt pending leaves with `INTERRUPTED` (D3811, `Flight::wait`).
pub fn get_or_compile_all(program: &Program, entries: &[Func], compile: impl FnOnce(&'static MemoryContext) -> PgResult<gravity_tpde::Compiled<'static>>) -> PgResult<(Vec<Code>, Outcome)> {
    get_or_compile_keyed(key_of(program), program, entries, compile)
}

/// `get_or_compile_all` under `key`, the program's `key_of` taken once by the caller: a tier-0 program's promotions
/// (lib.rs `tiered_codes`) hash the module once for all its entries, not once per entry (D3696: n entries each hashing
/// the whole module made a deep set-operation chain's compile quadratic).
pub fn get_or_compile_keyed(key: CodeKey, program: &Program, entries: &[Func], compile: impl FnOnce(&'static MemoryContext) -> PgResult<gravity_tpde::Compiled<'static>>) -> PgResult<(Vec<Code>, Outcome)> {
    let t0 = Instant::now();
    let flight = loop {
        if let Some((codes, mut out)) = hit(key, entries) {
            out.ns = t0.elapsed().as_nanos() as u64;
            return Ok((codes, out));
        }
        match claim(key) {
            Claim::Compile(f) => break f,
            Claim::Wait(f) => {
                WAITED.fetch_add(1, Ordering::Relaxed);
                f.wait()?;
            }
            Claim::Filed => {}
        }
    };
    let made = match flight {
        None => compile_and_file(program, entries, key, compile),
        Some(f) => {
            // the flight lands on every exit, an unwind's included (a resource error the compile throws as an ereport)
            let made = gravity_abi::status::unwind::catch(|| compile_and_file(program, entries, key, compile));
            land(key, &f);
            made.unwrap_or_else(|p| std::panic::resume_unwind(p))
        }
    };
    made.map(|(codes, mut out)| {
        out.ns = t0.elapsed().as_nanos() as u64;
        (codes, out)
    })
}

/// The miss half of `get_or_compile_all`: compile `program` into a fresh context and file it under `key`.
/// `ns` 0: the caller times it.
fn compile_and_file(program: &Program, entries: &[Func], key: CodeKey, compile: impl FnOnce(&'static MemoryContext) -> PgResult<gravity_tpde::Compiled<'static>>) -> PgResult<(Vec<Code>, Outcome)> {
    // D4762: no backend here: the whole build is the program caches' (`home`); a backend's stays its statement's until the
    // filing adopts the root
    let entered = if birth_owner() == PROGRAM_CACHES { home().enter() } else { None };
    let homed = entered.is_some();
    let (compiled, root) = compile_caught(new_root(), compile)?;
    crate::dump::on_compile(program, key.0, &compiled);
    let timing = compiled.timing();
    let bytes = compiled.code_bytes();
    let mut entry = Entry { own: memledger::charge::Charge::NONE, key, bytes, timing, last_use: AtomicU64::new(0), doomed: AtomicBool::new(false), compiled, root, homed, home_exit: memledger::HomeExit::new() };
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| State::new(budget_bytes())); // alloc: Cold
    let file = s.budget != 0;
    if file {
        if let Some(e) = s.map.get(&key) {
            touch(e);
            LOST.fetch_add(1, Ordering::Relaxed);
            let out = Outcome { lookup: Lookup::Lost, key, bytes: e.bytes, timing, ns: 0 };
            let codes = codes_of(e, entries);
            drop(g);
            drop(entry);
            return Ok((codes, out));
        }
        // C2 (planprod P2): the filed module's root is the program caches' from here, by construction (its live blocks
        // move onto their row and leave it at the drop), adopted before the entry is shared
        entry.root.set_owner(PROGRAM_CACHES);
        entry.own = memledger::charge::Charge::open(PROGRAM_CACHES, memledger::charge::release_of(PROGRAM_CACHES), memledger::Kind::Region, memledger::charge::Door::Heap, own_bytes());
    }
    if homed {
        // its Arc block, made here in the home, leaves it with the entry's last field
        entry.home_exit = memledger::HomeExit::for_block(memledger::arc_bytes::<Entry>());
    }
    crate::perfmap::on_compile(program, key.0, &entry.compiled);
    let fresh = Arc::new(entry); // alloc: Cold
    if !file {
        drop(g);
        MISSES.fetch_add(1, Ordering::Relaxed);
        return Ok((codes_of(&fresh, entries), Outcome { lookup: Lookup::Miss, key, bytes, timing, ns: 0 }));
    }
    touch(&fresh);
    s.bytes += bytes;
    file_in_home(&mut s.map, key, fresh.clone());
    while s.bytes > s.budget {
        if s.evict(Some(key), None, false).is_none() {
            break;
        }
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let out = Outcome { lookup: Lookup::Miss, key, bytes, timing, ns: 0 };
    Ok((codes_of(&fresh, entries), out))
}

pub fn stats() -> CodeCacheStats {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let (entries, bytes, budget) = g.as_ref().map_or((0, 0, budget_bytes()), |s| (s.map.len(), s.bytes, s.budget));
    CodeCacheStats { hits: HITS.load(Ordering::Relaxed), misses: MISSES.load(Ordering::Relaxed), lost: LOST.load(Ordering::Relaxed), waited: WAITED.load(Ordering::Relaxed), evictions: EVICTIONS.load(Ordering::Relaxed), entries, bytes, budget }
}

/// D4298 (T4 E2, the memory ladder's R2a tail): evict least-recently-used modules no running code holds (the map's `Arc`
/// is the last one) and staged IR no compile reads, until the cache holds at most `target` bytes; a module in use stays
/// (`in_use_bytes`, the ladder's floor). The cache's lock is taken without waiting (held: nothing, the next pass retries),
/// and the evicted entries drop after it is released. The bytes evicted.
pub fn trim_to(target: usize) -> usize {
    let mut gone = Vec::new(); // alloc: Cold
    {
        let Ok(mut g) = CACHE.try_write() else { return 0 };
        let Some(s) = g.as_mut() else { return 0 };
        while s.bytes > target {
            let Some(v) = s.evict(None, None, true) else { break };
            gone.push(v); // alloc: Cold
        }
    }
    gone.iter().map(Gone::bytes).sum()
}

/// D4298: the bytes of the modules running code holds now and of the staged IR a compile reads (never evicted: the
/// ladder's floor for the cache); a held lock reads as all of the cache in use.
pub fn in_use_bytes() -> usize {
    let Ok(g) = CACHE.try_read() else { return usize::MAX };
    g.as_ref().map_or(0, |s| s.map.values().filter(|e| Arc::strong_count(e) > 1).map(|e| e.bytes).sum::<usize>() + s.sides.values().filter(|x| Arc::strong_count(x) > 1).map(|x| x.bytes).sum::<usize>())
}

/// D4579: marks the modules at `mappings` (`gravity_program::Code::mapping`: a module's address) doomed: the program tier
/// let go a program that runs from them. A module another holder or a later hit keeps stays; `evict_doomed` takes the
/// rest. The modules marked.
pub fn doom_mappings(mappings: impl Iterator<Item = usize>) -> usize {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_ref() else { return 0 };
    let mut n = 0;
    // a program's codes: one module address each
    for m in mappings {
        if let Some(e) = s.map.values().find(|e| Arc::as_ptr(e) as usize == m) {
            e.doomed.store(true, Ordering::Relaxed);
            n += 1;
        }
    }
    if n > 0 {
        ANY_DOOMED.store(true, Ordering::Relaxed);
    }
    n
}

/// D4762: `doom_mappings` for the modules named by their keys (`gravity_program::Code::module`: what a tier-0 code promotes
/// from, filed by the compile pool before any promotion, so a program that never promoted names no mapping of it). The
/// modules marked.
pub fn doom_modules(keys: impl Iterator<Item = u128>) -> usize {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_ref() else { return 0 };
    let mut n = 0;
    // a program's tier-0 codes: one key each
    for k in keys {
        if let Some(e) = s.map.get(&CodeKey(k)) {
            e.doomed.store(true, Ordering::Relaxed);
            n += 1;
        }
    }
    if n > 0 {
        ANY_DOOMED.store(true, Ordering::Relaxed);
    }
    n
}

/// D4579: a module may be doomed (`doom_mappings` marked one since `evict_doomed` last found none): with none, a pass
/// takes no lock.
static ANY_DOOMED: AtomicBool = AtomicBool::new(false);

/// D4579: evict every doomed module nothing holds (the map's `Arc` the last one), each dropped outside the cache's lock;
/// a doomed module still in use stays doomed for a later pass. The bytes evicted.
pub fn evict_doomed() -> usize {
    if !ANY_DOOMED.load(Ordering::Relaxed) {
        return 0;
    }
    let mut freed = 0;
    // the cache's doomed modules: each pass takes one out or ends
    loop {
        let gone = {
            let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
            let Some(s) = g.as_mut() else { break };
            let Some(k) = s.map.iter().find(|(_, e)| e.doomed.load(Ordering::Relaxed) && Arc::strong_count(e) == 1).map(|(k, _)| *k) else {
                // the mark is cleared under the write lock, which `doom_mappings`' read lock excludes
                if !s.map.values().any(|e| e.doomed.load(Ordering::Relaxed)) {
                    ANY_DOOMED.store(false, Ordering::Relaxed);
                }
                break;
            };
            let e = s.map.remove(&k).expect("doomed entry present");
            s.bytes -= e.bytes;
            EVICTIONS.fetch_add(1, Ordering::Relaxed);
            e
        };
        freed += gone.bytes;
    }
    freed
}

/// Drop every entry not in use (a `Code` holding one keeps it); the counters stay. Nothing is
/// persisted and nothing needs invalidating — the key is the whole content.
pub fn reset() {
    // a queued compile would file its module again after the reset
    crate::promote::quiesce();
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.as_mut() {
        s.map.clear();
        s.sides.clear();
        s.bytes = 0;
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{HostFlags, IntBinOp, Type};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// `f(ptr) -> i64 { load i64 [ptr] + c }`, `host` a declared (uncalled) host at `addr`.
    fn program(c: i64, host_addr: usize) -> (Program, Func) {
        let mut pb = ProgramBuilder::new();
        pb.declare_host("h", &[Type::I64], Some(Type::I64), host_addr, HostFlags::default());
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let v = b.load(Type::I64, p, 0);
        let k = b.iconst(Type::I64, c);
        let r = b.int_bin(IntBinOp::Add, v, k);
        b.ret(&[r]);
        drop(b);
        (pb.finish(), f)
    }

    fn run(code: &Code, x: i64) -> i64 {
        let mut cell = x;
        // SAFETY: the program reads one i64 at offset 0 of its ctx pointer.
        unsafe { code.call(&mut cell as *mut i64 as *mut gravity_abi::Ctx) }
    }

    fn get(c: i64) -> (Code, Outcome) {
        let (p, f) = program(c, 0x1000);
        crate::mapped_cached(&p, f).unwrap()
    }

    /// Two entries, `f(ptr) = load + c` and `g(ptr) = load * c`.
    fn program2(c: i64) -> (Program, [Func; 2]) {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        let g = pb.declare_func("g", &[Type::Ptr], &[Type::I64]);
        for (fun, op) in [(f, IntBinOp::Add), (g, IntBinOp::Mul)] {
            let mut b = pb.define(fun);
            let p = b.params()[0];
            let v = b.load(Type::I64, p, 0);
            let k = b.iconst(Type::I64, c);
            let r = b.int_bin(op, v, k);
            b.ret(&[r]);
        }
        (pb.finish(), [f, g])
    }

    #[test]
    fn one_module_serves_every_entry() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (p, fs) = program2(3);
        let (codes, o) = crate::mapped_cached_all(&p, &fs).unwrap();
        assert_eq!(o.lookup, Lookup::Miss);
        assert_eq!((run(&codes[0], 5), run(&codes[1], 5)), (8, 15));
        let bytes = stats().bytes;
        let (g, o2) = crate::mapped_cached(&p, fs[1]).unwrap();
        assert_eq!(o2.lookup, Lookup::Hit, "a second entry of the same program is a hit");
        assert_eq!(g.entry() as usize, codes[1].entry() as usize, "one mapping");
        assert_eq!((stats().entries, stats().bytes), (1, bytes), "resident bytes flat per program");
    }

    #[test]
    fn evicting_a_module_keeps_every_entry_alive() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (p, fs) = program2(4);
        let (held, o) = crate::mapped_cached_all(&p, &fs).unwrap();
        set_budget_bytes(o.bytes + o.bytes / 2);
        let (_c1, _) = get(201);
        let (_c2, _) = get(202);
        assert_eq!(crate::mapped_cached(&p, fs[0]).unwrap().1.lookup, Lookup::Miss, "evicted");
        assert_eq!((run(&held[0], 1), run(&held[1], 2)), (5, 8), "evicted while held: both entries still run");
        drop(held);
        set_budget_bytes(64 << 20);
    }

    /// The program caches' account against its owner rows: bytes no owner holds, within the in-server check's bound
    /// (max(16 MiB, 2 %)).
    fn program_caches_unattributed() -> (i64, i64) {
        let acct = PROGRAM_CACHES.acct();
        let total = memledger::account_row(acct).account_bytes;
        let mut held = 0i64;
        memledger::charge::for_each_owner(|v| {
            if v.acct == acct {
                held = held.wrapping_add(v.held());
            }
        });
        (total, total.wrapping_sub(held))
    }

    /// C2 (planprod P2): a filed module's root context is adopted by the program caches at the filing, by construction:
    /// its live blocks are their owner row's named context bytes (no parallel charge); the account's bytes and its owner
    /// rows agree. A cache that keeps nothing (budget 0) adopts nothing.
    #[test]
    fn a_filed_module_moves_its_context_to_the_program_caches_account() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (_c, o) = get(301);
        assert_eq!(o.lookup, Lookup::Miss);
        {
            let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
            let e = g.as_ref().and_then(|s| s.map.get(&o.key)).expect("filed");
            let allocated = e.root.allocated() as i64;
            let (named, _) = memledger::charge::owner_ctx(PROGRAM_CACHES);
            assert!(allocated > 0 && named >= allocated, "the entry's blocks on the program caches' row: named {named}, the root's {allocated}");
        }
        let (total, unattributed) = program_caches_unattributed();
        assert!(unattributed.abs() <= (16i64 << 20).max(total.abs() / 50), "program caches: account {total}, unattributed {unattributed}");
        let (_h, o2) = get(301);
        assert_eq!(o2.lookup, Lookup::Hit);
        set_budget_bytes(0);
        let (_u, o3) = get(302);
        assert!(CACHE.read().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|s| !s.map.contains_key(&o3.key)), "not filed");
        set_budget_bytes(64 << 20);
    }

    /// planprod P2 (a): a compile on a backend is its statement's from birth (a named owner, never the thread's guess); the
    /// filing moves a kept entry to the program caches, and an entry the cache does not keep (budget 0) stays the
    /// statement's and goes with its code.
    #[test]
    fn a_compile_is_its_statements_until_the_cache_files_it() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        std::thread::spawn(|| {
            // a procno inside the backend slots (BACKEND_SLOT_BASE + procno < ACCOUNT_SLOTS), or the thread is unbound
            let a = memledger::Acct::backend(4075);
            assert!(a.is_backend(), "a backend slot");
            memledger::bind_backend(4075, 9000 + 4075);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            set_budget_bytes(0);
            let (code, o) = get(311);
            assert_eq!(o.lookup, Lookup::Miss);
            let named = memledger::charge::owner_ctx(stmt).0;
            assert!(named > 0, "an entry the cache does not keep is its statement's: {named}");
            drop(code);
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "and goes with its code");
            set_budget_bytes(64 << 20);
            let (_c, o) = get(312);
            assert_eq!(o.lookup, Lookup::Miss);
            assert_eq!(memledger::charge::owner_ctx(stmt).0, 0, "a filed entry is the program caches'");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// planprod W0b (the tpde half): a compile's scratch contexts are named at birth with the code's owner (`own_scratch`),
    /// never left to the creating thread's guess: on a thread with no backend (the compile pool) the program caches', and
    /// on a backend the running statement's, even inside a portal, whose guess is the portal. Each row shows the scratch's
    /// bytes while it lives and drops them with it; a scratch made without an owner lands on neither row as named.
    #[test]
    fn a_compiles_scratch_is_named_with_its_codes_owner() {
        let _l = TEST_LOCK.lock().unwrap();
        std::thread::spawn(|| {
            let (p, _) = program(321, 0x1000);
            let code = MemoryContext::new_unobserved("ScratchWitnessCode");
            // the scratch's bytes on `o`'s row while it lives, after one compile: (named, inferred)
            let held = |own: bool, o: memledger::Owner| -> (i64, i64) {
                let mut s = if own { gravity_tpde::Scratch::owned(|c| c.set_owner(birth_owner())) } else { gravity_tpde::Scratch::new() };
                drop(gravity_tpde::compile_with(code.mcx(), &p, &mut s).unwrap());
                let (n, i) = memledger::charge::owner_ctx(o);
                drop(s);
                let (n2, i2) = memledger::charge::owner_ctx(o);
                (n - n2, i - i2)
            };
            assert_eq!(birth_owner(), PROGRAM_CACHES, "no backend on this thread");
            let pool = (held(true, PROGRAM_CACHES), held(false, PROGRAM_CACHES));
            let a = memledger::Acct::backend(4076);
            memledger::bind_backend(4076, 9000 + 4076);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            memledger::charge::portal_begin(77);
            let portal = memledger::charge::enter_portal(77);
            assert_eq!(birth_owner(), stmt, "a portal's compile is its statement's");
            let backend = (held(true, stmt), held(false, stmt));
            drop(portal);
            let _ = memledger::charge::portal_end(a, 77);
            memledger::statement_end();
            memledger::unbind_backend();
            eprintln!(
                "W0b witness: a compile's scratch bytes (named, inferred) while it lives: no backend, on the program caches' row: owned {:?}, unowned {:?}; a backend in a portal, on the statement's row: owned {:?}, unowned {:?}",
                pool.0, pool.1, backend.0, backend.1
            );
            assert!(pool.0 .0 >= 8192 && pool.0 .1 == 0, "the pool's scratch is the program caches': {:?}", pool.0);
            assert_eq!(pool.1, (0, 0), "an unowned scratch is not");
            assert!(backend.0 .0 >= 8192 && backend.0 .1 == 0, "a backend's scratch is its statement's: {:?}", backend.0);
            assert_eq!(backend.1, (0, 0), "an unowned scratch is the portal's guess");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_key_is_the_content() {
        let (a, fa) = program(1, 0x1000);
        let (b, fb) = program(1, 0x1000);
        let _ = (fa, fb);
        assert_eq!(key_of(&a), key_of(&b));
        assert_ne!(key_of(&a), key_of(&program(2, 0x1000).0), "a constant");
        assert_ne!(key_of(&a), key_of(&program(1, 0x2000).0), "a host address");
    }

    #[test]
    fn hit_miss_and_the_code_runs() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (c1, o1) = get(7);
        assert_eq!(o1.lookup, Lookup::Miss);
        assert_eq!(run(&c1, 35), 42);
        let (c2, o2) = get(7);
        assert_eq!(o2.lookup, Lookup::Hit);
        assert_eq!(o2.key, o1.key);
        assert_eq!(c2.entry() as usize, c1.entry() as usize, "one mapping");
        assert_eq!(run(&c2, 1), 8);
        let (c3, o3) = get(9);
        assert_eq!(o3.lookup, Lookup::Miss);
        assert_eq!(run(&c3, 1), 10);
        assert_eq!(stats().entries, 2);
    }

    /// D4298 (T4 E2): the memory ladder's trim evicts the least-recently-used modules no running code holds, down to the
    /// bytes asked, and never a module in use (its bytes are the ladder's floor for the cache).
    #[test]
    fn ladder_trim_evicts_modules_not_in_use_down_to_its_target() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let one = get(300).1.bytes;
        let (held, _) = get(301);
        let _ = get(302);
        assert_eq!((stats().entries, in_use_bytes()), (3, one), "one module held by running code");
        let before = stats().evictions;
        assert_eq!(trim_to(2 * one), one, "the least recently used module not in use first");
        assert_eq!((stats().entries, stats().evictions), (2, before + 1));
        assert_eq!(get(300).1.lookup, Lookup::Miss, "300 (the oldest, not in use) was evicted");
        let _ = trim_to(0);
        assert_eq!((stats().entries, stats().bytes), (1, one), "the module in use stays");
        assert_eq!(run(&held, 1), 302, "and still runs");
        drop(held);
        assert_eq!((in_use_bytes(), trim_to(0), stats().entries), (0, one, 0), "once released it goes too");
    }

    /// D4579: the modules a tier removal names are evicted once nothing holds them: one still held stays doomed until its
    /// holder lets go, an unnamed one stays, and a hit on a doomed module takes the mark off.
    #[test]
    fn doomed_modules_go_once_unheld_and_a_hit_keeps_one() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (a, oa) = get(400);
        let (b, _) = get(401);
        let (c, _) = get(402);
        let one = oa.bytes;
        let (ma, mb, mc) = (a.mapping(), b.mapping(), c.mapping());
        assert!(ma != 0 && mb != 0 && mc != 0 && ma != mb);
        assert_eq!(doom_mappings([ma, mb, mc].into_iter()), 3);
        drop((a, b));
        let (c2, again) = get(402);
        assert_eq!(again.lookup, Lookup::Hit, "402 is wanted again: the hit takes its mark off");
        drop((c, c2));
        assert_eq!(evict_doomed(), 2 * one, "400 and 401: doomed and no longer held");
        assert_eq!(stats().entries, 1, "402 stays: a hit after the mark");
        assert_eq!(get(400).1.lookup, Lookup::Miss);
        let (held, _) = get(403);
        assert_eq!(doom_mappings([held.mapping()].into_iter()), 1);
        assert_eq!(evict_doomed(), 0, "a doomed module still held stays");
        drop(held);
        assert_eq!(evict_doomed(), one, "its holder let go: the next pass takes it");
    }

    #[test]
    fn eviction_by_bytes_keeps_entries_in_use_alive() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        let (c0, o0) = get(100);
        let one = o0.bytes;
        set_budget_bytes(2 * one + one / 2);
        let before = stats().evictions;
        let (_c1, _) = get(101);
        let (_c2, _) = get(102);
        let s = stats();
        assert_eq!(s.entries, 2, "{s:?}");
        assert_eq!(s.evictions, before + 1);
        assert!(s.bytes <= s.budget);
        assert_eq!(run(&c0, 1), 101, "evicted while held: still runs");
        assert_eq!(get(100).1.lookup, Lookup::Miss, "evicted");
        assert_eq!(get(102).1.lookup, Lookup::Hit, "the most recently used survived");
        set_budget_bytes(64 << 20);
    }

    #[test]
    fn a_zero_budget_keeps_nothing() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(0);
        let (c1, o1) = get(21);
        let (c2, o2) = get(21);
        assert_eq!((o1.lookup, o2.lookup), (Lookup::Miss, Lookup::Miss));
        assert_ne!(c1.entry() as usize, c2.entry() as usize);
        assert_eq!(run(&c2, 21), 42);
        assert_eq!(stats().entries, 0);
        set_budget_bytes(64 << 20);
    }

    #[test]
    fn one_key_compiled_by_many_threads_maps_once() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let s0 = stats();
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(|| get(555))).collect();
        let got: Vec<(Code, Outcome)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let entry = got[0].0.entry() as usize;
        for (c, _) in &got {
            assert_eq!(c.entry() as usize, entry);
            assert_eq!(run(c, 1), 556);
        }
        let s = stats();
        assert_eq!(s.entries, 1);
        assert_eq!(s.misses - s0.misses, 1);
        assert_eq!((s.hits - s0.hits) + (s.lost - s0.lost), 7);
    }

    #[test]
    fn the_hit_path_is_under_five_microseconds() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        let (p, f) = program(3, 0x1000);
        let _ = crate::mapped_cached(&p, f).unwrap();
        let mut ns: Vec<u64> = (0..2000).map(|_| crate::mapped_cached(&p, f).unwrap().1.ns).collect();
        ns.sort_unstable();
        let (p50, p90, p99) = (ns[ns.len() / 2], ns[ns.len() * 9 / 10], ns[ns.len() * 99 / 100]);
        println!("code cache hit (1 function, 4 instructions): p50 {p50} ns, p90 {p90} ns, p99 {p99} ns");
        if !cfg!(debug_assertions) {
            assert!(p90 <= 5_000, "p90 {p90} ns");
        }
    }

    static STOP: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

    /// The stopped thread's pending interrupt (the seam is process-wide; other tests' threads read false).
    fn stop_probe() -> bool {
        *STOP.lock().unwrap() == Some(std::thread::current().id())
    }

    /// C6 (d): the stop points read the interrupt facade (`cancelpoll::interrupts::pending`), so the test installs
    /// PostgreSQL's `interrupts_pending` seam as the stopped thread's probe, once per process (a seam is set once).
    fn install_stop_probe() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| postgres_seams::interrupts_pending::set(stop_probe));
    }

    /// D2909: a compile whose stop point sees a pending interrupt ends in `INTERRUPTED` with nothing filed (the next compile
    /// of the same program is a miss that maps and runs), its context released.
    #[test]
    fn an_interrupted_compile_files_nothing() {
        let _l = TEST_LOCK.lock().unwrap();
        reset();
        set_budget_bytes(64 << 20);
        install_stop_probe();
        let (p, fs) = program2(9);
        *STOP.lock().unwrap() = Some(std::thread::current().id());
        let e = crate::mapped_cached_all(&p, &fs).err().expect("the stop point stops the compile");
        *STOP.lock().unwrap() = None;
        assert_eq!(e.message(), INTERRUPTED);
        assert_eq!(stats().entries, 0, "nothing filed");
        let (codes, o) = crate::mapped_cached_all(&p, &fs).unwrap();
        assert_eq!(o.lookup, Lookup::Miss);
        assert_eq!((run(&codes[0], 5), run(&codes[1], 5)), (14, 45));
    }
}
