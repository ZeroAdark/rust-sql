use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pgsync::OnceLock;

use gravity_abi::Ctx;

/// A program's entry (`Entry`) and the interpreter's and wasm's run (`InterpRun`): contract::layout's fn-pointer types (abi).
pub use gravity_abi::program::{Entry, InterpRun};

pub enum Owner {
    Hand,
    Mapped(Box<dyn Any + Send + Sync>),
    Interp { state: Box<dyn Any + Send + Sync>, run: InterpRun },
    /// Tier 0 (H16 (d)): a code-cache miss's interpreted code, compiled (`Tiered::promote`) at the start of the program's
    /// second execution, or at its first when the driver's gate says the statement is not a tier-0 one.
    Tiered(Box<Tiered>),
}

/// Tier-0 code (H16 (d)): `interp` serves every call until `compiled` is set; the driver decides at each
/// execution's start (`Entry::start`, never mid-statement) whether this one interprets (`note_run`) or compiles first
/// (`promote`, synchronous). A compile that fails (the code generator's refusal or caught panic) is never the statement's
/// error: the code stays interpreted for good (`failure`), counted in `TIER_UP_FAILED`. A resource error PG would raise too
/// (out of memory, a cancel) unwinds out of `up` as any ereport does.
pub struct Tiered {
    interp: Code,
    compiled: OnceLock<Code>,
    failure: OnceLock<String>,
    runs: AtomicU64,
    cap: u64,
    /// D3867: a bound on the rows any plan node of the statement admitted emits (exec knobs/tier0.rs `rows_read`), which
    /// holds each pipeline's `predicted_rows` at the gate; `u64::MAX` = none.
    bound: u64,
    /// D4762: the content key of the module `up` compiles (the process code cache's `CodeKey`), 0 = none named.
    module: u128,
    up: Box<dyn Fn() -> Result<Code, String> + Send + Sync>,
}

/// Whether any tier-0 code was ever made in this process: the driver's per-statement check is one load while it is false.
pub static TIER0_LIVE: AtomicBool = AtomicBool::new(false);
/// Tier-0 executions started, tier-ups done, tier-ups that failed (the code stays interpreted) — the census counters.
pub static TIER0_RUNS: AtomicU64 = AtomicU64::new(0);
pub static TIER_UPS: AtomicU64 = AtomicU64::new(0);
pub static TIER_UP_FAILED: AtomicU64 = AtomicU64::new(0);
/// D3168 S0: staged functions made (`Staged::code`: one a code that made one), the ones of those whose code-cache lookup
/// compiled (a miss; a hit or a wait for another's compile did not), and the ns those compiles took (the served line's
/// `staged_total=`; process-wide).
pub static STAGED_MADE: AtomicU64 = AtomicU64::new(0);
pub static STAGED_COMPILED: AtomicU64 = AtomicU64::new(0);
pub static STAGED_NS: AtomicU64 = AtomicU64::new(0);
/// D3168: lowerings whose staged function the shape's memo skipped (`stage_memo`), and plain calls rerun with their stages
/// off because no staged function was lowered (`FLAG_STAGE_OFF`).
pub static STAGED_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static STAGED_WANTED: AtomicU64 = AtomicU64::new(0);

/// D3168: per program shape and pipeline, whether its staged function was ever chosen. A lowering lowers the staged function
/// the first time and afterwards only once a call chose it: a shape re-lowered per session pays the split once per process.
/// A lossy table: a collision or a lost entry lowers it again, never a wrong row.
pub mod stage_memo {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    const SLOTS: usize = 4096;
    const CHOSE: u64 = 1;
    const DEAD: u64 = 2;
    static MEMO: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

    /// What a lowering does with a pipeline whose probe can stage.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Split {
        /// lower the staged function beside the plain one
        Lower,
        /// the plain function alone, its guard kept: no call chose the staged one yet
        Want,
        /// the plain function alone, no guard: the staged function took no block
        Plain,
    }

    pub fn tag(shape: u128, root: i32, k: usize) -> u64 {
        let mut m = gravity_abi::hash::Mix128::new();
        for w in [shape as u64, (shape >> 64) as u64, root as u32 as u64, k as u64] {
            m.word(w);
        }
        (m.key128() as u64 & !3) | 4
    }

    fn slot(t: u64) -> &'static AtomicU64 {
        &MEMO[(t >> 40) as usize % SLOTS]
    }

    /// What the shape's entry says now; read only. D4875: `Lower` records nothing: the lowering records its staged
    /// function's outcome once it knows it (`lowered`, `dead`), so a lowering stopped in between (an interrupt's unwind:
    /// exec admit `install` lowers again) leaves the entry as it was and the next lowering lowers the staged function again.
    pub fn split(t: u64) -> Split {
        match slot(t).load(Relaxed) {
            v if v == t => Split::Want,
            v if v == t | DEAD => Split::Plain,
            _ => Split::Lower,
        }
    }

    /// D4875: the shape's lowering lowered its staged function and it took a block: the shape's next lowerings lower the
    /// plain function alone with its guard (`Want`) until a call chooses the staged one. A chosen shape stays chosen.
    pub fn lowered(t: u64) {
        let s = slot(t);
        if s.load(Relaxed) != t | CHOSE {
            s.store(t, Relaxed);
        }
    }

    pub fn dead(t: u64) {
        slot(t).store(t | DEAD, Relaxed);
    }

    pub fn chose(t: u64) {
        let s = slot(t);
        if s.load(Relaxed) != t | CHOSE {
            s.store(t | CHOSE, Relaxed);
        }
    }
}

/// D3168 S0: a pipeline's staged function (lowerv4 frame.rs `pipeline_fns_in`), made the first time the plain function
/// returns `status::STAGED` (its sealed build past the probe's guard), then run by every later call: a pipeline whose builds
/// stay under the guard never compiles it. Its maker (gravity_backend `staged`) holds the function's IR and never fails: a
/// compile the code generator refuses gives the interpreted function.
pub trait Staged: Send + Sync {
    /// The staged code, made at the first call; a call racing it may make its own (a code cache hit, or the wait for the
    /// first one's compile), of which one is kept. `None`: the plain code runs its stages off.
    fn code(&self) -> Option<&Code>;
}

/// D3168: what a code's `status::STAGED` hands its call to: its staged function, or (`Want`: the shape's memo skipped
/// it) none, the call marking its shape (`stage_memo::chose`) so the shape's next lowering lowers it.
enum StagedTo {
    Side(Box<dyn Staged>),
    Want(u64),
}

impl Tiered {
    /// The row cap the gate compares each pipeline's `predicted_rows` against (`pgrust.gravity_tier0_rows` at the compile).
    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// D3867: the bound the gate holds each pipeline's `predicted_rows` to (`u64::MAX`: none, the prediction alone).
    pub fn bound(&self) -> u64 {
        self.bound
    }

    /// The executions that interpreted until the tier settled (a failed promotion's own counted; the later ones not:
    /// the gate skips a settled code).
    pub fn runs(&self) -> u64 {
        self.runs.load(Ordering::Relaxed)
    }

    pub fn promoted(&self) -> bool {
        self.compiled.get().is_some()
    }

    /// Why the promotion failed, once it has: the code interprets every later execution and is never compiled again.
    pub fn failure(&self) -> Option<&str> {
        self.failure.get().map(String::as_str)
    }

    /// Compiled, or failed to: nothing is left for the gate to decide.
    pub fn settled(&self) -> bool {
        self.promoted() || self.failure.get().is_some()
    }

    /// This execution interprets.
    pub fn note_run(&self) {
        self.runs.fetch_add(1, Ordering::Relaxed);
        TIER0_RUNS.fetch_add(1, Ordering::Relaxed);
    }

    /// The compiled code, now (idempotent): every later call runs it. False when the compile failed, now or at an
    /// earlier promotion (never retried): the caller interprets, as the first execution did.
    pub fn promote(&self) -> bool {
        if self.compiled.get().is_some() {
            return true;
        }
        if self.failure.get().is_some() {
            return false;
        }
        match (self.up)() {
            Ok(c) => {
                if self.compiled.set(c).is_ok() {
                    TIER_UPS.fetch_add(1, Ordering::Relaxed);
                }
                true
            }
            Err(e) => {
                if self.failure.set(e).is_ok() {
                    TIER_UP_FAILED.fetch_add(1, Ordering::Relaxed);
                }
                false
            }
        }
    }
}

gravity_abi::host! {
    #[unwind]
    unsafe fn interp_identity(_: *mut Ctx) -> i64 {
        panic!("gravity_program: interpreter code runs through Code::call, never through its entry")
    }
}

/// A program's arena lays its codes out at 8-byte alignment (`Program::with_pipelines` sizes it with `round8`).
const _: () = assert!(core::mem::align_of::<Code>() <= 8);

pub struct Code {
    entry: Entry,
    bytes: usize,
    owner: Owner,
    /// D303: what a program's content key hashes for code whose entry is not its own (every interpreted entry is
    /// `interp_identity`): the IR's content key. `None`: the entry and the bytes, as for compiled and hand code. Two words,
    /// not a u128: a u128 makes `Code` 16-aligned, and the program's arena budgets its `Code` slice at 8 (`with_pipelines`).
    id: Option<[u64; 2]>,
    /// D3615: the mapping this code's bytes belong to (its address; 0: the code's own), which every code one compile
    /// mapped shares (`of_mapping`): `Program::code_bytes` counts a mapping's bytes once, not once per pipeline.
    mapping: usize,
    /// D3168 S0: the pipeline's staged function, run where this code returns `status::STAGED`; `None` for every other code.
    staged: Option<StagedTo>,
}

// the program's arena lays its `Code` slice out at 8-byte alignment (`Program::with_pipelines`, `round8`)
const _: () = assert!(core::mem::align_of::<Code>() <= 8);

impl Code {
    pub fn hand(entry: Entry) -> Code {
        Code { entry, bytes: 0, owner: Owner::Hand, id: None, mapping: 0, staged: None }
    }

    pub fn mapped(entry: Entry, bytes: usize, owner: Box<dyn Any + Send + Sync>) -> Code {
        Code { entry, bytes, owner: Owner::Mapped(owner), id: None, mapping: 0, staged: None }
    }

    /// Tier-0 code: `run(state, ctx)` interprets the program held in `state` (the data pointer of the box).
    pub fn interp(run: InterpRun, state: Box<dyn Any + Send + Sync>) -> Code {
        Code { entry: interp_identity, bytes: 0, owner: Owner::Interp { state, run }, id: None, mapping: 0, staged: None }
    }

    /// Tier-0 code over `interp`; `up` compiles (the process code cache) at the promotion; `cap` is the gate's row cap.
    pub fn tiered(interp: Code, cap: u64, up: Box<dyn Fn() -> Result<Code, String> + Send + Sync>) -> Code {
        TIER0_LIVE.store(true, Ordering::Relaxed);
        let t = Tiered { interp, compiled: OnceLock::new(), failure: OnceLock::new(), runs: AtomicU64::new(0), cap, bound: u64::MAX, module: 0, up };
        Code { entry: interp_identity, bytes: 0, owner: Owner::Tiered(Box::new(t)), id: None, mapping: 0, staged: None } // alloc: Program
    }

    /// D3867: a tier-0 code's bound on the rows the statement's plan nodes emit (`Tiered::bound`); other code unchanged.
    pub fn tier0_bound(mut self, bound: u64) -> Code {
        if let Owner::Tiered(t) = &mut self.owner {
            t.bound = bound;
        }
        self
    }

    /// D4762: names the module a tier-0 code's promotion compiles (`module`); other code unchanged.
    pub fn tier0_module(mut self, key: u128) -> Code {
        if let Owner::Tiered(t) = &mut self.owner {
            t.module = key;
        }
        self
    }

    /// D4762: the code cache key of the module a tier-0 code promotes from, 0 for every other code: the code cache files
    /// that module before any promotion (tier 0's compile pool), and a code that never promoted names no mapping.
    pub fn module(&self) -> u128 {
        match &self.owner {
            Owner::Tiered(t) => t.module,
            _ => 0,
        }
    }

    /// D3168 S0: the staged function this code's `status::STAGED` hands its call to (`Staged`).
    pub fn with_staged(mut self, staged: Box<dyn Staged>) -> Code {
        self.staged = Some(StagedTo::Side(staged));
        self
    }

    /// D3168: the stand-in for a staged function the shape's memo skipped (`StagedTo::Want`).
    pub fn with_want(mut self, tag: u64) -> Code {
        self.staged = Some(StagedTo::Want(tag));
        self
    }

    /// D303: the content key of the program this code runs (interpreted code: its entry names no program). The maker of
    /// interpreted code sets it (`gravity_backend::interp`: the IR's content key and the entry).
    pub fn with_identity(mut self, id: u128) -> Code {
        self.id = Some([id as u64, (id >> 64) as u64]);
        self
    }

    /// D3615: names the mapping this code is one entry of (`mapping`: its address, never 0), so a program whose codes one
    /// compile mapped is charged its bytes once (`Program::code_bytes`).
    pub fn of_mapping(mut self, mapping: usize) -> Code {
        self.mapping = mapping;
        self
    }

    /// The mapping this code's bytes belong to (a promoted tier-0 code: its compiled code's), 0 when they are its own.
    pub fn mapping(&self) -> usize {
        match &self.owner {
            Owner::Tiered(t) => t.compiled.get().map_or(0, Code::mapping),
            _ => self.mapping,
        }
    }

    /// What `Program::with_pipelines` hashes for this code: the entry and the bytes, or the IR content key (D303).
    pub fn identity(&self) -> (usize, usize) {
        match self.id {
            Some([lo, hi]) => (lo as usize, hi as usize),
            None => (self.entry as usize, self.bytes),
        }
    }

    /// The tier-0 state of this code, `None` for every other owner.
    pub fn tier0(&self) -> Option<&Tiered> {
        match &self.owner {
            Owner::Tiered(t) => Some(t),
            _ => None,
        }
    }

    /// What keeps `entry` mapped, the rest of the code dropped.
    pub fn into_owner(self) -> Owner {
        self.owner
    }

    pub fn entry(&self) -> Entry {
        self.entry
    }

    /// The machine code's bytes (a promoted tier-0 code: its compiled code's; 0 before).
    pub fn bytes(&self) -> usize {
        match &self.owner {
            Owner::Tiered(t) => t.compiled.get().map_or(0, Code::bytes),
            _ => self.bytes,
        }
    }

    pub fn is_hand(&self) -> bool {
        matches!(self.owner, Owner::Hand)
    }

    pub fn is_interp(&self) -> bool {
        matches!(self.owner, Owner::Interp { .. })
    }

    /// The owner's name: `hand`, `mapped`, `interp`, or `tier0` until a tier-0 code's promotion (then its compiled code's;
/// `tier0failed` once its promotion failed, the code interpreted for good)
    /// (the `Debug` form's word; the served line's `code=`).
    pub fn kind(&self) -> &'static str {
        match &self.owner {
            Owner::Hand => "hand",
            Owner::Mapped(_) => "mapped",
            Owner::Interp { .. } => "interp",
            Owner::Tiered(t) => match t.compiled.get() {
                Some(c) => c.kind(),
                None if t.failure.get().is_some() => "tier0failed",
                None => "tier0",
            },
        }
    }

    /// # Safety
    /// `ctx` is laid out per this code's `Layout` and every pointer cell it reads is valid.
    pub unsafe fn call(&self, ctx: *mut Ctx) -> i64 {
        let r = self.enter(ctx);
        let made = match &self.staged {
            Some(StagedTo::Side(s)) if r == gravity_abi::status::STAGED => s.code(),
            Some(StagedTo::Want(t)) if r == gravity_abi::status::STAGED => {
                stage_memo::chose(*t);
                STAGED_WANTED.fetch_add(1, Ordering::Relaxed);
                None
            }
            _ => return r,
        };
        match made {
            Some(c) => c.call(ctx),
            None => {
                let flags = (*ctx).flags;
                (*ctx).flags = flags | gravity_abi::ctx::FLAG_STAGE_OFF;
                let r = self.enter(ctx);
                (*ctx).flags = flags;
                r
            }
        }
    }

    /// # Safety
    /// As `call`.
    unsafe fn enter(&self, ctx: *mut Ctx) -> i64 {
        let r = match &self.owner {
            Owner::Interp { state, run } => run(&**state as *const (dyn Any + Send + Sync) as *const (), ctx),
            Owner::Tiered(t) => match t.compiled.get() {
                Some(c) => c.call(ctx),
                None => t.interp.call(ctx),
            },
            _ => (self.entry)(ctx),
        };
        // D310: an unwind a host parked below the JIT'd frames (a FATAL's thread exit, a PANIC) goes on from here
        gravity_abi::status::unwind::resume();
        r
    }
}

impl fmt::Debug for crate::program::Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Program({:?}, {} sites, {:?})", self.key, self.sites.len(), self.code())
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Code({}, {:p}, {} bytes)", self.kind(), self.entry as *const (), self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::stage_memo::{self, Split};

    #[test]
    fn stage_memo_lowers_a_shape_once_until_a_call_chooses_it() {
        let t = stage_memo::tag(0x1234_5678_9abc_def0_1111_2222_3333_4444, 7, 3);
        assert_eq!(stage_memo::split(t), Split::Lower);
        stage_memo::lowered(t);
        assert_eq!(stage_memo::split(t), Split::Want);
        stage_memo::chose(t);
        assert_eq!([stage_memo::split(t), stage_memo::split(t)], [Split::Lower, Split::Lower]);
        stage_memo::lowered(t);
        assert_eq!(stage_memo::split(t), Split::Lower, "a chosen shape stays chosen");
        let d = stage_memo::tag(0x99, 1, 0);
        assert_eq!(stage_memo::split(d), Split::Lower);
        stage_memo::dead(d);
        assert_eq!(stage_memo::split(d), Split::Plain);
        assert_ne!(stage_memo::tag(0x99, 1, 0), stage_memo::tag(0x99, 1, 1), "a pipeline of its own");
    }

    /// D4875: a lowering that stopped after its `split` answered `Lower` and before the staged function's lowering knew
    /// whether it took a block (an interrupt's unwind; exec admit `install` lowers again) leaves the shape as it was: the
    /// next lowering lowers the staged function again. A `Want` there lowered the plain function with its stage guard
    /// whatever the staged function took, a program unlike the memo-free one when it took none (front_check's 'the
    /// variant's IR differs', XX000, at every front hit of the program: m6_d900_spill_reuse W8 on the mq).
    #[test]
    fn a_split_whose_lowering_stopped_leaves_no_want() {
        let t = stage_memo::tag(0xd487_5000_0000_0000_0000_0000_0000_0001, 2, 3);
        assert_eq!(stage_memo::split(t), Split::Lower, "the shape's first lowering");
        // that lowering stopped here: neither the staged function's outcome nor `dead` was recorded
        assert_eq!(stage_memo::split(t), Split::Lower, "the lowering again lowers the staged function, never Want");
    }
}
