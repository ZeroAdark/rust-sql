//! The code cache's unsafe steps (rule R6, its raw home), moved from cache.rs: an entry's Send / Sync, the compile's
//! `'static` view of an entry's root context, and the mapped entries taken as `Code`s (codes_of). planprod P2: an entry's
//! context is its own mcx `SendRoot` (Send + Sync by itself), so the parent context's `unsafe impl Send` (SendCtx), the
//! `PARENT` lock and the hand-ordered drop are gone; the fields drop in declaration order.
#![allow(unsafe_code)]

use super::*;

// SAFETY: the mapping (`compiled`: its mapper's tables in `root`) is immutable once installed and run through its entry
// only; `root` (a SendRoot) is written by the compiling thread before the `Arc` is shared, set_owner'd at the filing before
// the `Arc` is made, and freed by the one thread that drops the last `Arc`, never by two at once
// (code-cache.md §2).
unsafe impl Send for Entry {}
unsafe impl Sync for Entry {}

/// `root`'s context seen as `'static` for the compile: `Entry` keeps the root alive past `compiled` (field order), and a
/// compile that fails drops it only after the compile returned (`compile_caught`); a SendRoot holds its context box raw, so
/// the address is stable when the root moves; only the compile writes through it until the entry is shared (the root's
/// `&mut` contract), read-only after.
pub(super) fn compile_context(root: &mut SendRoot) -> &'static MemoryContext {
    let ctx = root.mcx().context();
    // SAFETY: as above: the root outlives every use of the compile's view, at a stable address.
    unsafe { &*(ctx as *const MemoryContext) }
}

/// A `Code` per entry, each holding the one mapping (an evicted entry lives on in them), each naming it (D3615: a program
/// is charged the mapping's bytes once).
pub(super) fn codes_of(e: &Arc<Entry>, entries: &[Func]) -> Vec<Code> {
    // shapecheck: the frontend fusions of this mapping (REGISTRY CR2-19..25), on a hit as on a compile
    if fires::on() {
        let f = e.compiled.fusions();
        for (i, name) in gravity_tpde::view::FUSION_NAMES.iter().enumerate() {
            if f & (1 << i) != 0 {
                fires::fire_shared(*name);
            }
        }
    }
    entries
        .iter()
        .map(|&f| {
            // SAFETY: `crate::check_entry` fixed every entry's signature to `(ptr) -> (i64)` before the lookup, which under the
            // backend's value convention (64-bit integers and pointers) is `extern "C" fn(*mut Ctx) -> i64`.
            let entry = unsafe { core::mem::transmute::<usize, gravity_program::Entry>(e.compiled.entry(f)) };
            // the program key's view of this code is the IR's content key and the entry, as a tier-0 code's (D303): one
            // lowering keys one program whether its IR was compiled at the install or is promoted later
            Code::mapped(entry, e.bytes, Box::new(e.clone())).with_identity(e.key.0 ^ f.0 as u128).of_mapping(Arc::as_ptr(e) as usize) // alloc: Program
        })
        .collect() // alloc: Program
}

