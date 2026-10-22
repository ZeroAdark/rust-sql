//! The wasm32 server's backend (lane wasmjit stage 2): a program's entries compiled to one wasm module
//! (`gravity_wasm::compile_entries`) and installed into the server's own function table by the embedder's host import
//! (`env.gravity_install`, raw.rs `wasm`): the module shares the server's linear memory, table and shadow stack, and each
//! entry is a table index, which on wasm32 IS a function pointer, so `Code::mapped` calls it directly (no host hop per
//! call). TPDE is native-only; this replaces `cache::get_or_compile_all` on `target_family = "wasm"`.
//!
//! A process-wide cache keyed by the program's content key (`cache::key_of`) holds each installed module, so a repeated
//! statement installs once; LRU by module bytes to the code-cache budget, and an evicted module is uninstalled when its
//! last `Code` drops (its table slots go back to the embedder's free list). A refusal of the wasm code generator or an
//! install error falls back to the interpreter (tier 0's codes) and is logged once per fingerprint; an embedder without
//! the JIT host (`NoHost`: wasmtime's stub, an old page) turns the path off for the process, silently.

use super::*;
use raw::wasm::{install, stack_guard, InstallError, Installed};
use std::collections::{HashMap, HashSet};
use pgsync::Mutex;
use std::sync::Arc;
use std::time::Instant;

struct Filed {
    installed: Arc<Installed>,
    bytes: usize,
    last_use: u64,
}

#[derive(Default)]
struct State {
    map: HashMap<cache::CodeKey, Filed>,
    bytes: usize,
    clock: u64,
    /// The fingerprints whose fallback was logged (once each).
    logged: HashSet<cache::CodeKey>,
    /// The embedder has no JIT host: every miss interprets, nothing is logged.
    no_host: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// What the path did since process start (the stage-2 witnesses read it through `stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WasmStats {
    pub installs: u64,
    pub hits: u64,
    pub evictions: u64,
    pub fallbacks: u64,
    pub filed: usize,
    pub filed_bytes: usize,
}

static STATS: Mutex<WasmStats> = Mutex::new(WasmStats { installs: 0, hits: 0, evictions: 0, fallbacks: 0, filed: 0, filed_bytes: 0 });

pub fn stats() -> WasmStats {
    let mut s = *STATS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        s.filed = st.map.len();
        s.filed_bytes = st.bytes;
    }
    s
}

fn bump(f: impl FnOnce(&mut WasmStats)) {
    f(&mut STATS.lock().unwrap_or_else(|e| e.into_inner()));
}

fn codes_of(installed: &Arc<Installed>, bytes: usize, n: usize) -> Vec<Code> {
    // D3615: every entry names the one module, so a program is charged its bytes once (`Program::code_bytes`)
    let mapping = Arc::as_ptr(installed) as usize;
    (0..n).map(|i| Code::mapped(installed.entry(i), bytes, Box::new(installed.clone())).of_mapping(mapping)).collect() // alloc: Program
}

/// `compiled_all` on wasm: the installed module's codes, or the interpreter's (`fallback`).
pub(super) fn compiled_all(program: &Program, entries: &[Func]) -> PgResult<(Vec<Code>, Outcome)> {
    let t0 = Instant::now();
    let key = cache::key_of(program);
    {
        let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let st = g.get_or_insert_with(State::default);
        st.clock += 1;
        let now = st.clock;
        if let Some(f) = st.map.get_mut(&key) {
            f.last_use = now;
            let codes = codes_of(&f.installed, f.bytes, entries.len());
            let bytes = f.bytes;
            drop(g);
            bump(|s| s.hits += 1);
            return Ok((codes, Outcome { lookup: Lookup::Hit, key, bytes, timing: Default::default(), ns: t0.elapsed().as_nanos() as u64 }));
        }
        if st.no_host {
            drop(g);
            return fallback(program, entries, key, None);
        }
    }
    let module = match gravity_deps::catalog_free(|| gravity_wasm::compile_entries(program, entries, gravity_wasm::Options { stack_guard: Some(stack_guard()), import_hosts: true })) {
        Ok(m) => m,
        Err(r) => return fallback(program, entries, key, Some(format!("the wasm code generator refused: {r}"))),
    };
    let installed = match install(&module, entries.len()) {
        Ok(i) => Arc::new(i), // alloc: Cold
        Err(InstallError::NoHost) => {
            STATE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(State::default).no_host = true;
            return fallback(program, entries, key, None);
        }
        Err(InstallError::Failed(m)) => return fallback(program, entries, key, Some(format!("the embedder could not install the module: {m}"))),
    };
    bump(|s| s.installs += 1);
    let bytes = module.len();
    let codes = codes_of(&installed, bytes, entries.len());
    let budget = cache::budget_bytes();
    if budget != 0 {
        let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let st = g.get_or_insert_with(State::default);
        st.clock += 1;
        let now = st.clock;
        st.bytes += bytes;
        st.map.insert(key, Filed { installed, bytes, last_use: now }); // alloc: Cold
        while st.bytes > budget && st.map.len() > 1 {
            let Some(k) = st.map.iter().filter(|(k, _)| **k != key).min_by_key(|(_, f)| f.last_use).map(|(k, _)| *k) else { break };
            let f = st.map.remove(&k).expect("victim present");
            st.bytes -= f.bytes;
            bump(|s| s.evictions += 1);
        }
    }
    Ok((codes, Outcome { lookup: Lookup::Miss, key, bytes, timing: Default::default(), ns: t0.elapsed().as_nanos() as u64 }))
}

/// The interpreter's codes for `program` (one shared copy of the IR), with `why` logged once per fingerprint; the
/// interpreter's own refusal (`tier0_ok`) is the statement's admission refusal, as a compile error is natively.
fn fallback(program: &Program, entries: &[Func], key: cache::CodeKey, why: Option<String>) -> PgResult<(Vec<Code>, Outcome)> {
    bump(|s| s.fallbacks += 1);
    if let Some(why) = why {
        let first = STATE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(State::default).logged.insert(key);
        if first {
            eprintln!("gravity backend: program {:032x} runs interpreted: {why}", key.0);
        }
    }
    tier0_ok(program).map_err(|m| Box::new(PgError::error(format!("gravity backend: no wasm code and the interpreter refuses: {m}")).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)))?;
    let shared = std::sync::Arc::new(program.clone_owned(Some(cache::PROGRAM_CACHES))); // alloc: Program
    let mut out = Vec::with_capacity(entries.len()); // alloc: Program
    for &f in entries {
        out.push(interp_shared(shared.clone(), f, key.0).map_err(|e| Box::new(PgError::error(e)))?);
    }
    Ok((out, Outcome { lookup: Lookup::Miss, key, bytes: 0, timing: Default::default(), ns: 0 }))
}
