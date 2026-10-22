//! The JIT perf map (`pgrust.gravity_jit_perfmap`, superuser, env seed `PGRUST_GRAVITY_JIT_PERFMAP`; off by default, so a
//! release build carries it): every module the code cache compiles while it is on appends
//! one line per function to `/tmp/perf-<pid>.map`, Linux perf's JIT interface (tools/perf/Documentation/jit-interface.txt:
//! `START SIZE name`, hex), so `perf report` / `perf top` / `perf script` name the generated code. A function's name is
//! `gravity:<key8>:<func>[:n<node>]`: `<key8>` the first 8 hex digits of the module's code key (`cache::key_of`, the
//! dump's `<key>`), `<func>` the IR function (`p<i>` a pipeline), `<node>` the plan node of its pipeline when the lowering
//! left a note (`note`).
//!
//! A sidecar `/tmp/perf-<pid>.gravity` holds one `key <32 hex> ...` line per compile with the statement that lowered or
//! compiled it. A later code-cache hit compiles nothing and writes nothing: the map lives as long as the process, as the
//! code cache does, so turn it on before the statements run (a fresh server or the env seed). An evicted module's range
//! may be mapped again for a later one (`cache::evict_doomed`); a plain perf map cannot tell the two apart, so read the
//! map of a run with no evictions (`perf inject --jit`'s jitdump is the form that can, not built).
//!
//! Off (the default, or no hooks): one load and a branch per compile, never per row. The code compiled is the same
//! either way (the map reads the mapping after the compile). Both files are 0600, created by the server's user (perf
//! reads a map only when its owner is perf's user or root) and truncated by the process's first write. A write never
//! fails the statement (logged once); the writes stop at `CAP_BYTES` per process (logged once).

use gravity_ir::{Func, Program};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

/// `pgrust.gravity_jit_perfmap`'s values (guc_tables consts `GRAVITY_JIT_PERFMAP_*`).
pub const OFF: u8 = 0;
/// One line per function.
pub const FUNC: u8 = 1;
/// One line per tagged range of each function (the lowering's `gravity_ir::JitTags`; `FUNC` for a module it did not tag).
pub const TAGS: u8 = 2;

/// The bytes one process writes to the map and its sidecar, at most (`cost::runtime::PERFMAP_WRITE_BYTES`).
pub use gravity_program::cost::runtime::PERFMAP_WRITE_BYTES as CAP_BYTES;

/// The executor's side of the map.
pub struct Hooks {
    /// The mode in effect on the calling thread (`OFF`, `FUNC`, `TAGS`): read once per lowering and per compile.
    pub mode: fn() -> u8,
    /// The statement running on the calling thread, one line (the sidecar's).
    pub statement: fn() -> String,
}

static HOOKS: pgsync::OnceLock<Hooks> = pgsync::OnceLock::new();

/// Installs the hooks once per process (a second call is ignored).
pub fn install(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

/// The mode in effect on the calling thread, `OFF` with no hooks.
#[inline]
pub fn mode() -> u8 {
    match HOOKS.get() {
        Some(h) => (h.mode)(),
        None => OFF,
    }
}

// The notes kept (the oldest replaced first), and the pipeline functions a note names the plan node of
// (`cost::runtime::PERFMAP_NOTES` / `PERFMAP_NOTE_FUNCS`).
use gravity_program::cost::runtime::{PERFMAP_NOTES as NOTES_MAX, PERFMAP_NOTE_FUNCS as NODES_MAX};

/// What the lowering knows of a module that its IR does not say, under its code key: filed by the lowerer (`note`) while
/// the map is on, read by the module's compile, inline or on tier 0's compile pool (whose thread has no session: the
/// note's mode is the lowering session's). A slot of a fixed ring: nothing allocated but the tags' own root.
struct Slot {
    key: u128,
    /// The mode of the session that lowered it; `OFF`: an empty slot.
    mode: u8,
    n: u16,
    /// Each pipeline function's plan node, `(function index, plan node id)`, the first `n`.
    nodes: [(u32, i32); NODES_MAX],
    /// The IR's tags (mode `TAGS`), on the program caches' owner while filed; out of the slot during a tagged compile
    /// (`take_tags`).
    tags: Option<gravity_ir::JitTags>,
}

impl Slot {
    const EMPTY: Slot = Slot { key: 0, mode: OFF, n: 0, nodes: [(0, 0); NODES_MAX], tags: None };
}

struct Notes {
    slots: [Slot; NOTES_MAX],
    /// The slot a new key takes.
    next: usize,
}

/// `NOTED`: a note was ever filed (a compile with the map never on reads no lock).
static NOTES: pgsync::Mutex<Notes> = pgsync::Mutex::new(Notes { slots: [Slot::EMPTY; NOTES_MAX], next: 0 });
static NOTED: AtomicBool = AtomicBool::new(false);

/// Files the lowering's note of the module under `key` (the lowering's `Lowered::key`), replacing an older one of the
/// key: `mode` the lowering session's, `funcs[i]` the function that runs plan node `nodes[i]` (the first `NODES_MAX`),
/// `tags` the IR's (mode `TAGS`), adopted by the program caches' owner. Appends the sidecar's `lowered` line, the statement
/// that lowered it. Called only while the map is on.
pub fn note(key: u128, mode: u8, funcs: &[Func], nodes: &[i32], mut tags: Option<gravity_ir::JitTags>) {
    if let Some(t) = &mut tags {
        t.set_owner(crate::cache::PROGRAM_CACHES);
    }
    let old = {
        let mut q = NOTES.lock().unwrap_or_else(|e| e.into_inner());
        let i = match q.slots.iter().position(|s| s.mode != OFF && s.key == key) {
            Some(i) => i,
            None => {
                let i = q.next;
                q.next = (i + 1) % NOTES_MAX;
                i
            }
        };
        let s = &mut q.slots[i];
        (s.key, s.mode, s.n) = (key, mode, 0);
        for (f, nd) in funcs.iter().zip(nodes).take(NODES_MAX) {
            s.nodes[s.n as usize] = (f.0, *nd);
            s.n += 1;
        }
        core::mem::replace(&mut s.tags, tags)
    };
    drop(old); // a replaced note's tags, freed outside the lock
    NOTED.store(true, Ordering::Relaxed);
    let side = format!("key {key:032x} lowered mode {mode} stmt {}\n", statement()); // alloc: Cold
    write("", &side);
}

/// The tags filed under `key` (mode `TAGS`), taken out of its note for its compile (`gravity_tpde::compile_tagged`):
/// `put_tags` puts them back. A second compile of the key meanwhile (a race's) compiles untagged.
pub fn take_tags(key: u128) -> Option<gravity_ir::JitTags> {
    if !NOTED.load(Ordering::Relaxed) {
        return None;
    }
    let mut q = NOTES.lock().unwrap_or_else(|e| e.into_inner());
    q.slots.iter_mut().find(|s| s.mode == TAGS && s.key == key).and_then(|s| s.tags.take())
}

/// Puts `take_tags`' tags back in the note of `key` (dropped when the note was replaced meanwhile).
pub fn put_tags(key: u128, tags: gravity_ir::JitTags) {
    let back = {
        let mut q = NOTES.lock().unwrap_or_else(|e| e.into_inner());
        match q.slots.iter_mut().find(|s| s.mode == TAGS && s.key == key && s.tags.is_none()) {
            Some(s) => s.tags.replace(tags),
            None => Some(tags),
        }
    };
    drop(back);
}

/// `f` over the tags the lowering left for `key` (`take_tags`, `None`: untagged), put back after (`put_tags`): the code
/// cache's compile of the key (`gravity_tpde::compile_tagged`, which compiles untagged code the same).
pub fn with_tags<R>(key: u128, f: impl FnOnce(Option<&gravity_ir::JitTags>) -> R) -> R {
    let tags = take_tags(key);
    let r = f(tags.as_ref());
    if let Some(t) = tags {
        put_tags(key, t);
    }
    r
}

/// The statement on the calling thread through the hooks ("unknown" without them).
pub fn statement() -> String {
    HOOKS.get().map_or_else(|| String::from("unknown"), |h| (h.statement)()) // alloc: Cold
}

/// One line of the map: the range `[start, start + size)` named `name` (none for an empty range).
fn line(out: &mut String, start: usize, size: usize, name: core::fmt::Arguments<'_>) {
    if size != 0 {
        let _ = writeln!(out, "{start:x} {size:x} {name}");
    }
}

/// The map's lines for `compiled` (`program`'s module under `key`): one per function, or, for a tagged compile
/// (`gravity_tpde::Compiled::ranges`, mode `TAGS`), one per tagged range of it, `gravity:<key8>:<func>[:n<node>]:<tag>`:
/// none overlapping, together the function's whole range. `nodes`: the note's `(function index, plan node id)`.
pub fn lines(program: &Program, key: u128, compiled: &gravity_tpde::Compiled<'_>, nodes: &[(u32, i32)], tags: Option<&gravity_ir::JitTags>) -> String {
    let mut out = String::new(); // alloc: Cold
    let key8 = (key >> 96) as u32;
    let mut name = String::new(); // alloc: Cold
    for (i, f) in program.funcs.iter().enumerate() {
        let func = Func(i as u32);
        let (start, size) = (compiled.entry(func), compiled.size(func));
        name.clear();
        let _ = write!(name, "gravity:{key8:08x}:{}", f.name);
        if let Some((_, nd)) = nodes.iter().find(|(fi, _)| *fi == i as u32) {
            let _ = write!(name, ":n{nd}");
        }
        let mut rs = compiled.ranges(func).peekable();
        match tags {
            Some(t) if rs.peek().is_some() => {
                while let Some((off, tag)) = rs.next() {
                    let end = rs.peek().map_or(size, |n| n.0 as usize).min(size);
                    let tn = match tag {
                        gravity_tpde::adaptor::JIT_PROLOGUE => "prologue",
                        gravity_tpde::adaptor::JIT_EPILOGUE => "epilogue",
                        t2 => t.name(t2),
                    };
                    line(&mut out, start + off as usize, end.saturating_sub(off as usize), format_args!("{name}:{tn}"));
                }
            }
            _ => line(&mut out, start, size, format_args!("{name}")),
        }
    }
    out
}

/// Called by the cache after a compile it keeps: appends the module's lines when the map is on (the calling thread's
/// mode, or the lowering session's through its note) and the sidecar's line of the compile (with the statement when no
/// note names the key: a note's `lowered` line did).
pub(crate) fn on_compile(program: &Program, key: u128, compiled: &gravity_tpde::Compiled<'_>) {
    let m = mode();
    if m == OFF && !NOTED.load(Ordering::Relaxed) {
        return;
    }
    let base = compiled.mapper().get_mapped_range().map_or(0, |r| r.address());
    let (bytes, funcs) = (compiled.code_bytes(), program.funcs.len());
    let (map, side) = {
        let q = NOTES.lock().unwrap_or_else(|e| e.into_inner());
        let slot = q.slots.iter().find(|s| s.mode != OFF && s.key == key);
        let m = m.max(slot.map_or(OFF, |s| s.mode));
        if m == OFF {
            return;
        }
        let nodes = slot.map_or(&[][..], |s| &s.nodes[..s.n as usize]);
        let map = lines(program, key, compiled, nodes, slot.and_then(|s| s.tags.as_ref()));
        let side = match slot {
            Some(_) => format!("key {key:032x} base {base:x} bytes {bytes} funcs {funcs} mode {m}\n"), // alloc: Cold
            None => format!("key {key:032x} base {base:x} bytes {bytes} funcs {funcs} mode {m} stmt {}\n", statement()), // alloc: Cold
        };
        (map, side)
    };
    write(&map, &side);
}

/// The map's path for this process: `/tmp/perf-<pid>.map`; the sidecar's `/tmp/perf-<pid>.gravity`.
pub fn paths() -> (String, String) {
    let pid = std::process::id();
    (format!("/tmp/perf-{pid}.map"), format!("/tmp/perf-{pid}.gravity")) // alloc: Cold
}

struct State {
    /// The pid that last wrote (a fork's child truncates its own files at its first write).
    pid: u32,
    written: u64,
    warned: bool,
}

/// One lock for both files: a module's lines are written whole, and the process's first write truncates them.
static STATE: pgsync::Mutex<State> = pgsync::Mutex::new(State { pid: 0, written: 0, warned: false });

fn write(map: &str, side: &str) {
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let pid = std::process::id();
    let fresh = st.pid != pid;
    if fresh {
        (st.pid, st.written, st.warned) = (pid, 0, false);
    }
    let n = (map.len() + side.len()) as u64;
    if st.written + n > CAP_BYTES {
        if !core::mem::replace(&mut st.warned, true) {
            eprintln!("gravity perfmap: {} bytes written, the cap: no more lines this process", st.written);
        }
        return;
    }
    let (mp, sp) = paths();
    match raw::append(&mp, map, fresh).and_then(|()| raw::append(&sp, side, fresh)) {
        Ok(()) => st.written += n,
        Err(e) => {
            if !core::mem::replace(&mut st.warned, true) {
                eprintln!("gravity perfmap: {mp}: {e}");
            }
        }
    }
}

mod raw {
    /// `text` appended to `path` (created 0600; truncated first when `fresh`).
    #[cfg(unix)]
    pub(super) fn append(path: &str, text: &str, fresh: bool) -> std::io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut o = std::fs::OpenOptions::new();
        o.create(true).mode(0o600);
        if fresh {
            o.write(true).truncate(true);
        } else {
            o.append(true);
        }
        o.open(path)?.write_all(text.as_bytes())
    }

    #[cfg(not(unix))]
    pub(super) fn append(_path: &str, _text: &str, _fresh: bool) -> std::io::Result<()> {
        Ok(())
    }
}
