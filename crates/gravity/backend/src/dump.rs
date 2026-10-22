//! The program dump (`pgrust.gravity_dump_dir`, env seed `PGRUST_GRAVITY_DUMP_IR`): every program the code
//! cache compiles (a miss; a hit compiled nothing) is written to the directory as `<key>.ir` (`print_program`),
//! `<key>.<arch>.bin` (the mapped code, byte for byte) and `<key>.meta` (the statement, the build, each function's entry
//! offset into the `.bin`); `scripts/gravity-dump-asm.sh` disassembles them. `<key>` is the cache's content key
//! (`cache::key_of`, 32 hex digits). The executor installs the hooks (`install`): the directory (the GUC's value) and the
//! meta lines only it knows (the statement). Off (no hooks, or an empty directory) the compile path pays one load and a
//! branch, never per row; the code compiled is the same either way (the dump reads the mapping after the compile).

use gravity_ir::Program;
use std::fmt::Write as _;

mod raw;

/// The executor's side of the dump.
pub struct Hooks {
    /// The directory, `None` or empty = off: read once per compile.
    pub dir: fn() -> Option<String>,
    /// Extra `name value` lines for the `.meta` (the statement text), newline-terminated.
    pub meta: fn() -> String,
}

static HOOKS: pgsync::OnceLock<Hooks> = pgsync::OnceLock::new();

/// Installs the hooks once per process (a second call is ignored).
pub fn install(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

/// The dump directory in effect, `None` when off.
pub fn dir() -> Option<String> {
    let h = HOOKS.get()?;
    (h.dir)().filter(|d| !d.is_empty())
}

pub const ARCH: &str = if cfg!(target_arch = "aarch64") {
    "aarch64"
} else if cfg!(target_arch = "x86_64") {
    "x86_64"
} else {
    "unknown"
};

/// Called by the cache after a compile: writes the three files when the dump is on. A write failure is logged to stderr
/// and never fails the statement (the dump is a diagnostic).
pub(crate) fn on_compile(program: &Program, key: u128, compiled: &gravity_tpde::Compiled<'_>) {
    let Some(dir) = dir() else { return };
    if let Err(e) = write(&dir, program, key, compiled) {
        eprintln!("gravity dump: {dir}: {e}");
    }
}

fn write(dir: &str, program: &Program, key: u128, compiled: &gravity_tpde::Compiled<'_>) -> std::io::Result<()> {
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir)?;
    let stem = format!("{key:032x}");
    std::fs::write(dir.join(format!("{stem}.ir")), gravity_ir::text::print_program(program))?;
    let (base, code_bytes) = raw::write_mapped(&dir.join(format!("{stem}.{ARCH}.bin")), compiled)?;
    let mut m = String::new();
    let _ = writeln!(m, "key {stem}");
    let _ = writeln!(m, "arch {ARCH}");
    let _ = writeln!(m, "backend tpde");
    let _ = writeln!(m, "code_bytes {code_bytes}");
    for (i, f) in program.funcs.iter().enumerate() {
        let off = compiled.entry(gravity_ir::Func(i as u32)).wrapping_sub(base);
        let _ = writeln!(m, "func {i} {} {off:#x}", f.name);
    }
    let exe = std::env::current_exe().ok();
    let _ = writeln!(m, "exe {}", exe.as_ref().map_or("unknown".into(), |p| p.display().to_string()));
    // The gate's binaries carry a BUILD stamp beside them (box-prepare.sh: commit, tree, sha256, rustc).
    let stamp = exe.and_then(|p| std::fs::read_to_string(p.with_file_name("BUILD")).ok());
    for l in stamp.iter().flat_map(|s| s.lines()) {
        let _ = writeln!(m, "build {l}");
    }
    let _ = writeln!(m, "width any (a program is compiled once for every width: the cache key has no width)");
    if let Some(h) = HOOKS.get() {
        m.push_str(&(h.meta)());
    }
    // v5 identity: each host function's address as a binary-relative symbol address (`fn@<vaddr>`, the tool names it from
    // the binary's symbol table): the IR text names a host by its declared name only, and 42 of 187 bound functions share one
    for (i, h) in program.host_fns.iter().enumerate() {
        let _ = writeln!(m, "host {i} {} {}", h.name, code_addr(h.addr));
    }
    std::fs::write(dir.join(format!("{stem}.meta")), m)
}

// ------------------------------------------------------------------------------------------------ v5 identity (day 0)
// The identity tool (v5 PLAN §4.3, `scripts/gravity-identity/`) compares parent and tip per statement: the normalized IR, the
// run-time program (the descriptors and the v4 protocols: rerun sets and members are in neither the IR text nor `.meta`),
// and the symbol each bound address names. A code cache hit compiles nothing and the dump above stays silent, so every
// install (the executor's program-cache miss: each lowering) writes `prog/<seq>.prog` too, whatever the code cache did.

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The executable's mappings `(start, end, file offset)`, read once from `/proc/self/maps` (empty off Linux).
fn exe_maps() -> &'static [(usize, usize, usize)] {
    static MAPS: pgsync::OnceLock<Vec<(usize, usize, usize)>> = pgsync::OnceLock::new();
    MAPS.get_or_init(|| {
        let exe = std::fs::read_link("/proc/self/exe").ok().map(|p| p.display().to_string());
        let Some(exe) = exe else { return Vec::new() };
        let Ok(text) = std::fs::read_to_string("/proc/self/maps") else { return Vec::new() };
        let mut out = Vec::new(); // alloc: Cold
        for line in text.lines() {
            let mut f = line.split_whitespace();
            let (Some(range), Some(_perms), Some(off)) = (f.next(), f.next(), f.next()) else { continue };
            let path = line.find('/').map_or("", |i| line[i..].trim());
            if path != exe {
                continue;
            }
            let Some((a, b)) = range.split_once('-') else { continue };
            if let (Ok(a), Ok(b), Ok(o)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16), usize::from_str_radix(off, 16)) {
                out.push((a, b, o));
            }
        }
        out
    })
}

/// `addr` as the identity tool reads it: `fn@<vaddr>` inside the executable (vaddr = the address less the load bias, the
/// start of the offset-0 mapping: the address `nm` prints for a PIE), `0x0` for null, `ADDR` anywhere else (heap, stack,
/// another object).
pub fn code_addr(addr: usize) -> String {
    let maps = exe_maps();
    let bias = maps.iter().find(|m| m.2 == 0).map(|m| m.0);
    match bias {
        Some(b) if maps.iter().any(|&(s, e, _)| addr >= s && addr < e) => format!("fn@{:x}", addr - b),
        _ if addr == 0 => String::from("0x0"), // alloc: Cold
        _ => String::from("ADDR"), // alloc: Cold
    }
}

/// Every `0x<hex>` in a `Debug` text through `code_addr` (a function pointer becomes its symbol address, a heap pointer `ADDR`).
pub fn mask_addrs(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len()); // alloc: Cold
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'0' && i + 2 < b.len() && b[i + 1] == b'x' && b[i + 2].is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let j = (i + 2..b.len()).find(|&k| !b[k].is_ascii_hexdigit()).unwrap_or(b.len());
            match usize::from_str_radix(&text[i + 2..j], 16) {
                Ok(v) => out.push_str(&code_addr(v)),
                Err(_) => out.push_str(&text[i..j]),
            }
            i = j;
        } else {
            let c = text[i..].chars().next().unwrap_or('?');
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// One install's record, `<dir>/prog/<seq>.prog`: the statement (the executor's meta lines), then per program `name` (main,
/// init <k>, arm <k>) its host functions as symbol addresses, its run-time descriptors (`descs`, a `Debug` text, masked here)
/// and its IR text. A reference line (`ref <vaddr> gravity_backend::dump::code_addr`) lets the tool check the bias against the
/// symbol table. Never fails the statement.
pub fn write_prog(dir: &str, parts: &[(String, &Program, String)]) {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut m = String::new(); // alloc: Cold
    let _ = writeln!(m, "seq {seq}");
    let me: fn(usize) -> String = code_addr;
    let _ = writeln!(m, "ref {} gravity_backend::dump::code_addr", code_addr(me as usize));
    // the load bias and the executable's extent: the tool reads an IR constant inside it as a code address (never keyed)
    let maps = exe_maps();
    if let (Some(b), Some(e)) = (maps.iter().find(|x| x.2 == 0).map(|x| x.0), maps.iter().map(|x| x.1).max()) {
        let _ = writeln!(m, "bias {b} {e}");
    }
    if let Some(h) = HOOKS.get() {
        m.push_str(&(h.meta)());
    }
    for (name, ir, descs) in parts {
        let _ = writeln!(m, "=== program {name}");
        for (i, h) in ir.host_fns.iter().enumerate() {
            let _ = writeln!(m, "host {i} {} {}", h.name, code_addr(h.addr));
        }
        let _ = writeln!(m, "--- descs");
        m.push_str(&mask_addrs(descs));
        if !m.ends_with('\n') {
            m.push('\n');
        }
        let _ = writeln!(m, "--- ir");
        m.push_str(&gravity_ir::text::print_program(ir));
    }
    let d = std::path::Path::new(dir).join("prog");
    if let Err(e) = std::fs::create_dir_all(&d).and_then(|()| std::fs::write(d.join(format!("{seq:08}.prog")), m)) {
        eprintln!("gravity dump: {dir}/prog: {e}");
    }
}
