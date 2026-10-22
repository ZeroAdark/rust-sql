//! Native backend for Gravity IR through TPDE (AArch64 and x86-64).
//!
//! `compile` turns a `gravity_ir::Program` into executable code in one pass
//! (TPDE's single-pass register allocation) and maps it into memory. Host
//! functions are extern symbols resolved to `HostFnDecl::addr` at map time.
//!
//! Value convention at the boundary is the interpreter's: integers and
//! pointers are 64-bit (narrow ints sign-extended, `Bool` 0/1), `F32`
//! arguments and results travel in the low 32 bits of a float register (`s`
//! on AArch64, `xmm` on x86-64), `F64` in the low 64 bits. The target is
//! selected by `cfg(target_arch)`: `frontend` is the AArch64 lowering,
//! `frontend_x64` the x86-64 one (System V calling convention).

#![no_std]
#![deny(unsafe_code)]
extern crate alloc;

pub mod adaptor;
pub mod fold;
#[cfg(target_arch = "aarch64")]
pub mod frontend;
#[cfg(target_arch = "x86_64")]
pub mod frontend_x64;
pub mod view;
mod raw;

use alloc::vec::Vec;
use gravity_ir::{Func, Program};
use mcx::{GrowIn, Mcx};
use tpde_assembler::{Architecture, SymRef};
use tpde_compiler::{with_compiler_base, RegallocEvent};
use tpde_elf_mapper::ElfMapper;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[cfg(target_arch = "aarch64")]
use {tpde_compiler_a64::CompilerA64, tpde_function_writer_a64::A64Target};
#[cfg(target_arch = "x86_64")]
use {tpde_compiler_x64::CompilerX64, tpde_function_writer_x64::X64Target};

pub use raw::{interrupt_pending, now_ns, set_clock, stop_point, Interrupted};

/// Wall time of one `compile`, by phase, in nanoseconds of the clock
/// `set_clock` installed (all zero without one):
/// the frontend view (`FnView::build`, per function), TPDE's analyzer
/// (block layout + liveness), the encode (everything else inside the
/// compiler: instruction selection, register allocation, the object
/// writer), and the install (`ElfMapper::map`: layout, relocation,
/// `mmap`/`mprotect`, icache, unwind registration).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompileTiming {
    pub view_ns: u64,
    pub analysis_ns: u64,
    pub encode_ns: u64,
    pub install_ns: u64,
    pub total_ns: u64,
}

/// One register-allocation event the compiler emitted code for
/// (`RegallocEvent`), located in the program: the function, the original
/// IR block (`None` for a split edge block, then `layout` says which),
/// and the block's expected executions per call under the view's
/// frequency model (`FnView::block_freq`; a loop body counts 100 per
/// level, a cold block 0.001).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegEvent {
    pub func: u32,
    pub block: Option<u32>,
    pub layout: u32,
    pub freq: f32,
    pub event: RegallocEvent,
}

impl RegEvent {
    pub fn new(adaptor: &adaptor::GravityAdaptor<'_>, event: RegallocEvent, block: adaptor::GBlock) -> Self {
        let v = &adaptor.view;
        let orig = v.blocks.get(block.0 as usize).and_then(|b| b.orig);
        // A split block runs as often as the edge it carries: its source
        // block's frequency is the bound used here.
        let freq_of = |b: Option<u32>| b.and_then(|b| v.block_freq.get(b as usize).copied()).unwrap_or(1.0);
        let freq = match orig {
            Some(b) => freq_of(Some(b)),
            None => v.splits.get(v.blocks[block.0 as usize].inst_begin as usize).map_or(1.0, |_| 1.0),
        };
        RegEvent { func: adaptor.cur, block: orig, layout: block.0, freq, event }
    }
}

/// The events of one function summed per kind: `[Spill, Reload, PhiMove,
/// PhiReload, PhiStore, CallSave, CallRestore]`, static counts and the
/// frequency-weighted expectation per call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RegallocSummary {
    pub count: [u32; 12],
    pub weighted: [f32; 12],
}

/// The error a compile meets when the host platform cannot map its code executable (`ElfMapper::map` refused): the
/// backend's caller may run the program interpreted instead (gravity_backend, macOS only).
pub const MAPPING_REFUSED: &str = "gravity_tpde: mapping refused on this platform";
pub const REGALLOC_KINDS: [&str; 12] = ["spill", "reload", "phi_mov", "phi_ld", "phi_st", "call_save", "call_restore", "demote", "region_begin", "region_end", "stale_entry", "lend"];

pub fn regalloc_kind(e: RegallocEvent) -> usize {
    match e {
        RegallocEvent::Spill => 0,
        RegallocEvent::Reload => 1,
        RegallocEvent::PhiMove => 2,
        RegallocEvent::PhiReload => 3,
        RegallocEvent::PhiStore => 4,
        RegallocEvent::CallSave => 5,
        RegallocEvent::CallRestore => 6,
        RegallocEvent::Demote => 7,
        RegallocEvent::RegionBegin => 8,
        RegallocEvent::RegionEnd => 9,
        RegallocEvent::StaleEntry => 10,
        RegallocEvent::Lend => 11,
    }
}

/// A mapped, executable program. Its entries live in the code's context (`mcx`, the mapper's); the register-allocation
/// events only under a recording compile (`compile_recording`, the tests'), none in production.
pub struct Compiled<'m> {
    mapper: ElfMapper<'m>,
    entries: mcx::PgVec<'m, usize>,
    /// Each function's code bytes (its symbol's final size: A64's unused prologue reservation removed), by `Func` index.
    sizes: mcx::PgVec<'m, u32>,
    /// A tagged compile's ranges (`compile_tagged`): `(function, offset from its entry, tag)`, by function then offset,
    /// each range running to the next of its function or to its size; empty for every other compile.
    ranges: mcx::PgVec<'m, (u32, u32, u32)>,
    code_bytes: usize,
    regalloc: Vec<RegEvent>,
    timing: CompileTiming,
    fusions: u8,
}

impl<'m> Compiled<'m> {
    /// Native address of function `f`. Its ABI is `extern "C"` with the
    /// crate-level value convention.
    pub fn entry(&self, f: Func) -> usize {
        self.entries[f.index()]
    }
    /// Function `f`'s code bytes from its entry (`entry`): the JIT perf map's range (gravity_backend `perfmap`).
    pub fn size(&self, f: Func) -> usize {
        self.sizes[f.index()] as usize
    }
    /// Function `f`'s tagged ranges `(offset from its entry, tag)` (`compile_tagged`; `adaptor::JIT_PROLOGUE` before
    /// the first tag, `adaptor::JIT_EPILOGUE` after the body), empty for an untagged compile.
    pub fn ranges(&self, f: Func) -> impl Iterator<Item = (u32, u32)> + '_ {
        let lo = self.ranges.partition_point(|r| r.0 < f.0);
        self.ranges[lo..].iter().take_while(move |r| r.0 == f.0).map(|r| (r.1, r.2))
    }
    pub fn code_bytes(&self) -> usize {
        self.code_bytes
    }
    pub fn mapper(&self) -> &ElfMapper<'m> {
        &self.mapper
    }
    /// Per-phase wall time of this compile (`CompileTiming`).
    pub fn timing(&self) -> CompileTiming {
        self.timing
    }
    /// shapecheck: the frontend fusions this compile took, bits of `view::FUSION_NAMES`.
    pub fn fusions(&self) -> u8 {
        self.fusions
    }
    /// Every register-allocation event the compiler reported, in emission
    /// order (`RegEvent`): a recording compile's (`compile_recording`), else none.
    pub fn regalloc(&self) -> &[RegEvent] {
        &self.regalloc
    }
    /// Per function: static counts per kind and the frequency-weighted
    /// expectation per call (`RegallocSummary`).
    pub fn regalloc_summary(&self, func: Func) -> RegallocSummary {
        let mut s = RegallocSummary::default();
        for e in self.regalloc.iter().filter(|e| e.func == func.0) {
            let k = regalloc_kind(e.event);
            s.count[k] += 1;
            s.weighted[k] += e.freq;
        }
        s
    }
    /// The events of `func` as text: one line per kind with the static
    /// count and the weighted expectation, then one line per block that
    /// has any, hottest first (`docs/design/gravity-simple/tpde-fixedregs.md`).
    pub fn regalloc_report(&self, func: Func) -> alloc::string::String {
        use core::fmt::Write;
        let mut out = alloc::string::String::new();
        let s = self.regalloc_summary(func);
        let _ = write!(out, "regalloc f{}:", func.0);
        for (k, name) in REGALLOC_KINDS.iter().enumerate() {
            if s.count[k] != 0 {
                let _ = write!(out, " {name}={} ({:.1}/call)", s.count[k], s.weighted[k]);
            }
        }
        if s.count.iter().all(|&c| c == 0) {
            let _ = write!(out, " none");
        }
        let _ = writeln!(out);
        // Per block, hottest first.
        let mut blocks: Vec<(u32, Option<u32>, f32, [u32; REGALLOC_KINDS.len()])> = Vec::new();
        for e in self.regalloc.iter().filter(|e| e.func == func.0) {
            let k = regalloc_kind(e.event);
            match blocks.iter_mut().find(|b| b.0 == e.layout) {
                Some(b) => b.3[k] += 1,
                None => {
                    let mut c = [0u32; REGALLOC_KINDS.len()];
                    c[k] = 1;
                    blocks.push((e.layout, e.block, e.freq, c));
                }
            }
        }
        blocks.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(core::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        for (layout, block, freq, c) in blocks {
            match block {
                Some(b) => {
                    let _ = write!(out, "  b{b} (layout {layout}, freq {freq}):");
                }
                None => {
                    let _ = write!(out, "  split (layout {layout}, freq {freq}):");
                }
            }
            for (k, name) in REGALLOC_KINDS.iter().enumerate() {
                if c[k] != 0 {
                    let _ = write!(out, " {name}={}", c[k]);
                }
            }
            let _ = writeln!(out);
        }
        out
    }
}

/// The frontend's recorded regalloc events, taken after a compile.
#[cfg(target_arch = "aarch64")]
fn take_events(t: &CompilerA64<'_, frontend::GravityFrontend>) -> Vec<RegEvent> {
    use tpde_compiler_a64::A64CompilerState;
    core::mem::take(&mut *t.a64_frontend().events.borrow_mut())
}
#[cfg(target_arch = "x86_64")]
fn take_events(t: &CompilerX64<'_, frontend_x64::GravityFrontendX64>) -> Vec<RegEvent> {
    use tpde_compiler_x64::X64CompilerState;
    core::mem::take(&mut *t.frontend().events.borrow_mut())
}

/// The frontend's accumulated analyzer nanoseconds (`CompileTiming::analysis_ns`).
#[cfg(target_arch = "aarch64")]
fn analysis_ns(t: &CompilerA64<'_, frontend::GravityFrontend>) -> u64 {
    use tpde_compiler_a64::A64CompilerState;
    t.a64_frontend().analysis_ns.get()
}
#[cfg(target_arch = "x86_64")]
fn analysis_ns(t: &CompilerX64<'_, frontend_x64::GravityFrontendX64>) -> u64 {
    use tpde_compiler_x64::X64CompilerState;
    t.frontend().analysis_ns.get()
}

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into()
}

/// CPU feature bits for the x86-64 compiler, detected on the host the code
/// will run on. AVX is deliberately left out so every emitted instruction is
/// a legacy SSE form; the frontend needs SSE4.1 (`roundsd`) and prefers
/// `popcnt`, both part of the x86-64-v2 baseline.
#[cfg(target_arch = "x86_64")]
pub fn host_cpu_features() -> u32 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    use tpde_compiler_x64::*;
    let leaf1 = __cpuid(1);
    let leaf7 = __cpuid_count(7, 0);
    let ext1 = __cpuid(0x8000_0001);
    let mut feats = CPU_BASELINE;
    let bit = |v: u32, b: u32| v & (1 << b) != 0;
    if bit(leaf1.ecx, 0) {
        feats |= CPU_SSE3;
    }
    if bit(leaf1.ecx, 9) {
        feats |= CPU_SSSE3;
    }
    if bit(leaf1.ecx, 19) {
        feats |= CPU_SSE4_1;
    }
    if bit(leaf1.ecx, 20) {
        feats |= CPU_SSE4_2;
    }
    if bit(leaf1.ecx, 23) {
        feats |= CPU_POPCNT;
    }
    if bit(leaf1.ecx, 13) {
        feats |= CPU_CMPXCHG16B;
    }
    if bit(leaf7.ebx, 3) {
        feats |= CPU_BMI1;
    }
    if bit(leaf7.ebx, 8) {
        feats |= CPU_BMI2;
    }
    if bit(ext1.ecx, 5) {
        feats |= CPU_LZCNT;
    }
    feats
}

/// A backend's compile scratch, reusable across compiles: the context the
/// frontend view's tables live in (`view::FnView`, one per compile, built
/// per function; an aset child of the compiler's, so a pass's temporaries
/// are freed as they drop) and the memory context every transient of the
/// compiler lives in (TPDE's analyzer, assembler and register-allocation
/// tables), reset before each compile. Never the code's context: the code
/// outlives them.
pub struct Scratch {
    view_ctx: mcx::MemoryContext,
    mctx: mcx::MemoryContext,
    /// Record the register-allocation events (`Compiled::regalloc`): the tests' opt-in (`recording`).
    record: bool,
}

impl Default for Scratch {
    fn default() -> Self {
        Self::new()
    }
}

impl Scratch {
    pub fn new() -> Self {
        let mctx = mcx::MemoryContext::new_bump("GravityCompileScratch");
        Scratch { view_ctx: mctx.new_child("GravityCompileView"), mctx, record: false }
    }

    /// A scratch whose compiles record the register-allocation events (`Compiled::regalloc`, `regalloc_report`).
    pub fn recording() -> Self {
        Scratch { record: true, ..Scratch::new() }
    }

    /// A scratch whose contexts `own` names the owner of, at birth: the caller's ledger owner (the compiling statement's,
    /// or the holder a pool compiles for), not the creating thread's guess. The ledger is the caller's (no_std here).
    pub fn owned(own: impl Fn(&mcx::MemoryContext)) -> Self {
        let s = Scratch::new();
        own(&s.view_ctx);
        own(&s.mctx);
        s
    }
}

/// Compile and map `program`. Every function in the program is compiled.
pub fn compile<'m>(mcx: Mcx<'m>, program: &Program) -> PgResult<Compiled<'m>> {
    compile_with(mcx, program, &mut Scratch::new())
}

/// `compile` with its scratch contexts owned at birth by `own`'s owner (`Scratch::owned`).
pub fn compile_owned<'m>(mcx: Mcx<'m>, program: &Program, own: impl Fn(&mcx::MemoryContext)) -> PgResult<Compiled<'m>> {
    compile_with(mcx, program, &mut Scratch::owned(own))
}

/// `compile_owned`, recording which code range each of the lowering's tags made (`Compiled::ranges`, the JIT map's):
/// the frontend reads each instruction's text offset, the code itself is the same.
pub fn compile_tagged<'m>(mcx: Mcx<'m>, program: &Program, own: impl Fn(&mcx::MemoryContext), tags: Option<&gravity_ir::JitTags>) -> PgResult<Compiled<'m>> {
    compile_in(mcx, program, &mut Scratch::owned(own), tags)
}

/// `compile`, recording the register-allocation events (`Compiled::regalloc`): the tests' and the debugging tools' road.
pub fn compile_recording<'m>(mcx: Mcx<'m>, program: &Program) -> PgResult<Compiled<'m>> {
    compile_with(mcx, program, &mut Scratch::recording())
}

/// `compile` with a reused `Scratch`: `mcx` holds the code (the mapping
/// and the mapper's tables), `scratch` everything transient. Never inlined, as `compile_in` (D4507): the callers share one
/// copy of the compiler.
#[inline(never)]
pub fn compile_with<'m>(mcx: Mcx<'m>, program: &Program, scratch: &mut Scratch) -> PgResult<Compiled<'m>> {
    compile_in(mcx, program, scratch, None)
}

/// `compile_with`, recording the tags' ranges when `tags` (`compile_tagged`).
/// Never inlined: every compile (the promotion pool's thread, a miss compiled inline) calls this one copy. Fat LTO + PGO
/// inlined the whole TPDE compiler into the pool thread's entry (`__rust_begin_short_backtrace`, 1.7 MB of .text beside
/// the outlined copy the other callers keep; reports/binsize-20261015 §2).
#[inline(never)]
fn compile_in<'m>(mcx: Mcx<'m>, program: &Program, scratch: &mut Scratch, tags: Option<&gravity_ir::JitTags>) -> PgResult<Compiled<'m>> {
    #[cfg(target_arch = "aarch64")]
    let rules: &'static dyn view::ImmRules = &frontend::A64_IMM;
    #[cfg(target_arch = "x86_64")]
    let rules: &'static dyn view::ImmRules = &frontend_x64::X64_IMM;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let rules: &'static dyn view::ImmRules = &view::NoImm;
    if let Err(m) = view::check_edge_arity(program) {
        return Err(PgError::error(alloc::format!("gravity_tpde: {m}")).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into());
    }
    // the compiler's copy with its constant instructions folded (fold.rs); the caller's program is untouched
    let folded = fold::program(program, scratch.view_ctx.mcx());
    let program = folded.as_ref().unwrap_or(program);
    let t_start = now_ns();
    let record = scratch.record;
    scratch.mctx.reset();
    let smcx = scratch.mctx.mcx();
    let mut adaptor = adaptor::GravityAdaptor::with_view(program, rules, view::FnView::new_in(scratch.view_ctx.mcx()));
    adaptor.jit = tags.map(|t| adaptor::JitRec::new_in(t, scratch.view_ctx.mcx()));
    let mut mapper = ElfMapper::new(mcx)?;
    // The closure is target-independent; only the compiler, writer and
    // architecture tags differ, so it is spelled once as a macro.
    macro_rules! run {
        ($compiler:expr, $writer:expr, $arch:expr, $events:expr) => {
            with_compiler_base(smcx, adaptor, $compiler, $writer, $arch, |mut compiler| -> PgResult<(mcx::PgVec<'m, usize>, mcx::PgVec<'m, u32>, mcx::PgVec<'m, (u32, u32, u32)>, usize, Vec<RegEvent>, CompileTiming, u8)> {
                let t0 = now_ns();
                if !compiler.compile()? {
                    return Err(invalid("gravity_tpde: compilation failed"));
                }
                let t1 = now_ns();
                let fusions = compiler.adaptor.view.fusions;
                let done = compiler.adaptor.jit.as_mut().map(adaptor::JitRec::take_done);
                let mut timing = CompileTiming { view_ns: compiler.adaptor.view_ns, analysis_ns: analysis_ns(&*compiler.target()), ..CompileTiming::default() };
                timing.encode_ns = (t1 - t0).saturating_sub(timing.view_ns + timing.analysis_ns);
                let regalloc: Vec<RegEvent> = $events(&*compiler.target());
                let assembler = compiler.assembler_mut()?;
                // Symbols by link name (`f<i>` / `h<i>`: the index is in the name).
                let mut syms: mcx::PgVec<Option<SymRef>> = mcx::pg_vec![in smcx; None; program.funcs.len()];
                for index in 0..assembler.global_symbols().len() {
                    let symbol = SymRef::new(0x8000_0000 | index as u32);
                    let name = assembler.sym_name(symbol)?;
                    if let Some((false, fi)) = adaptor::LinkNames::parse(name) {
                        if fi < program.funcs.len() {
                            syms[fi] = Some(symbol);
                        }
                    }
                }
                // each function's final size (its symbol's), read before the map takes the assembler; a tagged compile's
                // ranges from the function's start (its symbol's text offset)
                let mut sizes = mcx::reserved_in(program.funcs.len(), mcx);
                let mut starts: mcx::PgVec<u64> = mcx::reserved_in(if done.is_some() { program.funcs.len() } else { 0 }, smcx);
                for s in &syms {
                    let (start, size) = match s {
                        Some(s) => assembler.symbol(*s).map(|e| (e.st_value, e.st_size))?,
                        None => (0, 0),
                    };
                    sizes.push_in(u32::try_from(size).unwrap_or(u32::MAX));
                    if done.is_some() {
                        starts.push_in(start);
                    }
                }
                let mut ranges = mcx::PgVec::new_in(mcx);
                if let Some(done) = &done {
                    ranges_from_entries(done, &starts, &sizes, &mut ranges);
                }
                let t2 = now_ns();
                let mapped = raw::map(&mut mapper, assembler, program)?;
                if !mapped {
                    return Err(invalid(MAPPING_REFUSED));
                }
                timing.install_ns = now_ns() - t2;
                let mut entries = mcx::reserved_in(program.funcs.len(), mcx);
                for s in syms {
                    let s = s.ok_or_else(|| invalid("gravity_tpde: function symbol missing"))?;
                    entries.push_in(mapper.get_sym_addr(s).ok_or_else(|| invalid("gravity_tpde: symbol not mapped"))?.address());
                }
                let code_bytes = mapper.get_mapped_range().map_or(0, |r| r.len());
                Ok((entries, sizes, ranges, code_bytes, regalloc, timing, fusions))
            })??
        };
    }
    #[cfg(target_arch = "aarch64")]
    let (entries, sizes, ranges, code_bytes, regalloc, mut timing, fusions) = run!(CompilerA64::new(smcx, frontend::GravityFrontend::with_record(record), 0), A64Target::new(smcx), Architecture::Aarch64, take_events);
    #[cfg(target_arch = "x86_64")]
    let (entries, sizes, ranges, code_bytes, regalloc, mut timing, fusions) = run!(CompilerX64::new(smcx, frontend_x64::GravityFrontendX64::with_record(record), host_cpu_features()), X64Target::new(), Architecture::X86_64, take_events);
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let (entries, sizes, ranges, code_bytes, regalloc, mut timing, fusions): (mcx::PgVec<'m, usize>, mcx::PgVec<'m, u32>, mcx::PgVec<'m, (u32, u32, u32)>, usize, Vec<RegEvent>, CompileTiming, u8) = {
        let _ = (adaptor, &mut mapper, smcx, record);
        return Err(invalid("gravity_tpde: unsupported target architecture"));
    };
    scratch.mctx.reset();
    timing.total_ns = now_ns() - t_start;
    Ok(Compiled { mapper, entries, sizes, ranges, code_bytes, regalloc, timing, fusions })
}

/// A tagged compile's ranges: `done`'s `(function, text offset, tag)` (each function's in the order emitted) made
/// relative to the function's start (`starts`, its symbol's text offset) into `out`, each function's led by `JIT_PROLOGUE`
/// at 0 when its first tag starts later, the ones past its size dropped, a later tag at the same offset kept (nothing was
/// emitted between the two), a range of the tag before it merged into that one, sorted by function then offset.
fn ranges_from_entries(done: &[(u32, u32, u32)], starts: &[u64], sizes: &[u32], out: &mut mcx::PgVec<'_, (u32, u32, u32)>) {
    let mut i = 0;
    while i < done.len() {
        let f = done[i].0;
        let n = done[i..].iter().take_while(|r| r.0 == f).count();
        let start = starts.get(f as usize).copied().unwrap_or(0) as u32;
        let size = sizes.get(f as usize).copied().unwrap_or(0);
        let first = out.len();
        for &(_, off, tag) in &done[i..i + n] {
            let off = off.saturating_sub(start);
            if off >= size {
                continue;
            }
            if out.len() == first && off != 0 {
                out.push_in((f, 0, adaptor::JIT_PROLOGUE));
            }
            let k = out.len();
            if k > first && out[k - 1].1 == off {
                out[k - 1].2 = tag;
            } else {
                out.push_in((f, off, tag));
            }
        }
        i += n;
    }
    out.dedup_by(|cur, prev| cur.0 == prev.0 && cur.2 == prev.2);
    out.sort_unstable_by_key(|r| (r.0, r.1));
}
