//! The wasm backend for Gravity IR (`docs/gravity/wasm.md` "as built"): one
//! core wasm module per program over `wasm-encoder`, importing the server's
//! linear memory, function table and shadow stack pointer. Values keep the
//! interpreter's convention at wasm widths (narrow ints sign-extended in
//! `i32`, `I64`/`Ptr` in `i64` under pointer policy P-A, vectors `v128`);
//! a host call is `call_indirect` at `HostFnDecl::addr` with the declared
//! signature under the same map.
#![deny(unsafe_code)]

pub mod cfg;
pub mod embed;
mod func;
mod locals;
pub mod irreduce;
mod raw;

use gravity_ir::*;
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, FuncType, FunctionSection, GlobalType, ImportSection, MemoryType, Module, RefType, TableType, TypeSection, ValType,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A back edge whose target does not dominate its source that the
    /// dispatch transform (`irreduce`) could not remove.
    Irreducible { func: Func, from: Block, to: Block },
    /// `HostFnDecl::addr` does not fit a table index.
    HostIndex { host: HostFn, addr: usize },
    /// An operation the backend has no form for.
    Unsupported { func: Func, block: Block, what: &'static str },
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::Irreducible { func, from, to } => write!(f, "irreducible control flow in f{} (b{} -> b{})", func.0, from.0, to.0),
            Refusal::HostIndex { host, addr } => write!(f, "host h{} addr {addr:#x} is not a table index", host.0),
            Refusal::Unsupported { func, block, what } => write!(f, "unsupported in f{} b{}: {what}", func.0, block.0),
        }
    }
}

/// Where the emitted code needs a routine rather than a sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Helper {
    SmulOvf64,
    Smulh64,
    Umulh64,
    Crc32c,
}

pub const MEMORY_IMPORT: &str = "memory";
pub const TABLE_IMPORT: &str = "__indirect_function_table";
pub const STACK_POINTER_IMPORT: &str = "__stack_pointer";
pub const IMPORT_MODULE: &str = "env";
pub const ENTRY_EXPORT: &str = "entry";

pub fn val_type(t: Type) -> ValType {
    match t {
        Type::Bool | Type::I8 | Type::I16 | Type::I32 => ValType::I32,
        Type::I64 | Type::Ptr => ValType::I64,
        Type::F32 => ValType::F32,
        Type::F64 => ValType::F64,
        _ => ValType::V128,
    }
}

/// `f<k>` for every function; the entry also as `entry` (a `(i32) -> i64`
/// wrapper when its signature is `fn(Ptr) -> I64`).
pub fn func_export_name(k: Func) -> String {
    format!("f{}", k.0)
}

pub(crate) struct Types {
    section: TypeSection,
    seen: Vec<(Vec<ValType>, Vec<ValType>)>,
}

impl Types {
    fn new() -> Self {
        Types { section: TypeSection::new(), seen: Vec::new() }
    }
    pub(crate) fn index(&mut self, params: &[ValType], results: &[ValType]) -> u32 {
        if let Some(i) = self.seen.iter().position(|(p, r)| p == params && r == results) {
            return i as u32;
        }
        self.section.ty().func_type(&FuncType::new(params.iter().copied(), results.iter().copied()));
        self.seen.push((params.to_vec(), results.to_vec()));
        (self.seen.len() - 1) as u32
    }
}

pub(crate) fn host_sig(d: &HostFnDecl) -> (Vec<ValType>, Vec<ValType>) {
    (d.params.iter().map(|&t| val_type(t)).collect(), d.ret.map(val_type).into_iter().collect())
}

fn func_sig(f: &Function) -> (Vec<ValType>, Vec<ValType>) {
    (f.params.iter().map(|&t| val_type(t)).collect(), f.returns.iter().map(|&t| val_type(t)).collect())
}

/// The shared shadow stack's floor for a module running inside the server (wasm32, the install path): a generated
/// frame whose new stack pointer falls below `floor` (or wraps) calls the `() -> ()` function at table index `overflow`
/// first, a server host that raises the statement's stack-depth error (an unwind, never a write below the stack).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackGuard {
    pub floor: u32,
    pub overflow: u32,
}

/// What a module is compiled for beyond its program: `stack_guard` None = no floor check (the standalone module the
/// tests and the native agreement runs instantiate).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub stack_guard: Option<StackGuard>,
    /// Reach every host (and the stack guard's overflow host) through the module's own function imports
    /// `env.host_<table index>` (`host_import_index`) instead of `call_indirect` on an imported table, and import no
    /// table: the embedder resolves each import from the server's table (wasm/pgrust-wasi.js). A table that many
    /// instances import makes every `table.grow` grow each of their dispatch tables (V8: live instances x table
    /// length, an out-of-memory at a few thousand installed programs); direct calls are cheaper too.
    pub import_hosts: bool,
}

/// The import name of the host at table index `addr` (`Options::import_hosts`).
pub fn host_import_name(addr: usize) -> String {
    format!("host_{addr}")
}

/// How a module reaches its hosts and numbers its functions (`Options::import_hosts`): `base` imported functions come
/// first; `hosts[k]` is host `k`'s import (empty: `call_indirect` through the imported table), `overflow` the stack
/// guard's.
#[derive(Default)]
pub(crate) struct Link {
    pub(crate) base: u32,
    pub(crate) hosts: Vec<u32>,
    pub(crate) overflow: Option<u32>,
}

/// The export name of the `i`-th entry of `compile_entries` (an `(i32) -> i64` wrapper).
pub fn entry_export_name(i: usize) -> String {
    format!("entry{i}")
}

/// Compile `program` to one module; `entry` is exported as `entry`.
pub fn compile(program: &Program, entry: Func) -> Result<Vec<u8>, Refusal> {
    module(program, &[entry], Options::default(), false)
}

/// Compile `program` to one module whose `entries` are each exported as `entry<i>` (`entry_export_name`), an
/// `(i32) -> i64` wrapper: the whole program in one compile however many pipelines enter it (D230's rule for TPDE).
/// Every entry must be `fn(Ptr) -> I64` (`gravity_backend::check_entry`).
pub fn compile_entries(program: &Program, entries: &[Func], opts: Options) -> Result<Vec<u8>, Refusal> {
    for &e in entries {
        let f = program.func(e);
        if f.params != [Type::Ptr] || f.returns != [Type::I64] {
            return Err(Refusal::Unsupported { func: e, block: f.entry, what: "an entry that is not fn(ptr) -> i64" });
        }
    }
    module(program, entries, opts, true)
}

fn module(program: &Program, entries: &[Func], opts: Options, numbered: bool) -> Result<Vec<u8>, Refusal> {
    for (k, h) in program.host_fns.iter().enumerate() {
        if h.addr > u32::MAX as usize {
            return Err(Refusal::HostIndex { host: HostFn(k as u32), addr: h.addr });
        }
    }
    let mut types = Types::new();
    let mut imports = ImportSection::new();
    imports.import(IMPORT_MODULE, MEMORY_IMPORT, EntityType::Memory(MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None }));
    let mut link = Link::default();
    if opts.import_hosts {
        for h in program.host_fns.iter() {
            let (p, r) = host_sig(h);
            let ty = types.index(&p, &r);
            imports.import(IMPORT_MODULE, &host_import_name(h.addr), EntityType::Function(ty));
            link.hosts.push(link.base);
            link.base += 1;
        }
        if let Some(g) = opts.stack_guard {
            let ty = types.index(&[], &[]);
            imports.import(IMPORT_MODULE, &host_import_name(g.overflow as usize), EntityType::Function(ty));
            link.overflow = Some(link.base);
            link.base += 1;
        }
    } else {
        imports.import(IMPORT_MODULE, TABLE_IMPORT, EntityType::Table(TableType { element_type: RefType::FUNCREF, table64: false, minimum: 0, maximum: None, shared: false }));
    }
    imports.import(IMPORT_MODULE, STACK_POINTER_IMPORT, EntityType::Global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }));
    let sigs: Vec<(Vec<ValType>, Vec<ValType>)> = program.funcs.iter().map(func_sig).collect();
    let func_types: Vec<u32> = sigs.iter().map(|(p, r)| types.index(p, r)).collect();
    let mut helpers: Vec<Helper> = Vec::new();
    let nfuncs = program.funcs.len() as u32;
    let mut bodies = Vec::with_capacity(program.funcs.len());
    for (k, f) in program.funcs.iter().enumerate() {
        bodies.push(func::emit(program, Func(k as u32), f, &mut types, &mut helpers, nfuncs, opts.stack_guard, &link)?);
    }
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    for (k, body) in bodies.into_iter().enumerate() {
        functions.function(func_types[k]);
        code.function(&body);
    }
    for &h in &helpers {
        let (sig, body) = func::helper(h);
        functions.function(types.index(&sig.0, &sig.1));
        code.function(&body);
    }
    let mut exports = ExportSection::new();
    for k in 0..nfuncs {
        exports.export(&func_export_name(Func(k)), ExportKind::Func, link.base + k);
    }
    let mut next = link.base + nfuncs + helpers.len() as u32;
    for (i, &entry) in entries.iter().enumerate() {
        let ef = program.func(entry);
        let name = if numbered { entry_export_name(i) } else { ENTRY_EXPORT.to_string() };
        if ef.params == [Type::Ptr] && ef.returns == [Type::I64] {
            functions.function(types.index(&[ValType::I32], &[ValType::I64]));
            code.function(&func::entry_wrapper(entry, link.base));
            exports.export(&name, ExportKind::Func, next);
            next += 1;
        } else {
            exports.export(&name, ExportKind::Func, link.base + entry.0);
        }
    }
    let mut module = Module::new();
    module.section(&types.section);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);
    Ok(module.finish())
}

/// The frame layout of a function's stack slots: offsets from the frame
/// base and the 16-byte-rounded frame size (the interpreter's `Frame::new`).
pub fn frame_layout(f: &Function) -> (Vec<u32>, u32) {
    let mut offsets = Vec::with_capacity(f.stack_slots.len());
    let mut size = 0u32;
    for s in &f.stack_slots {
        let align = s.align.max(1);
        size = (size + align - 1) & !(align - 1);
        offsets.push(size);
        size += s.bytes;
    }
    let max_align = f.stack_slots.iter().map(|s| s.align).max().unwrap_or(16).max(16);
    (offsets, (size + max_align - 1) & !(max_align - 1))
}
