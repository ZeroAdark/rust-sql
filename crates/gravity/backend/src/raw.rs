//! The backend's unsafe steps (rule R6, its raw home), moved whole from lib.rs: the mapped code's Send / Sync, the TPDE
//! entry address taken as an `Entry` (compile_in) and the code context held for the mapping (mapped), the admission's
//! row estimate erased into its thread-local (with_row_estimate / rows_read), and tier 0's interpreted entry (interp_run).
#![allow(unsafe_code)]

use super::*;

// SAFETY: the mapping is immutable once installed; `Mapped` is only ever held by the owning `Code` and
// dropped with it (the driver runs the code through its entry and never touches the owner); fields
// drop in order, the mapping before the context it borrows. The context (a SendRoot) is Send + Sync by itself; these
// answer for the mapper's tables in it.
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

/// `program`'s `entry` compiled through TPDE into `context`: the mapping and its entry address.
fn compile_in(context: &'static MemoryContext, program: &Program, entry: Func) -> PgResult<(gravity_tpde::Compiled<'static>, gravity_program::Entry)> {
    check_entries(program, &[entry])?;
    let compiled = gravity_deps::catalog_free(|| gravity_tpde::compile_owned(context.mcx(), program, own_scratch))?;
    // SAFETY: `check_entry` fixed the signature to `(ptr) -> (i64)`, which under the backend's value
    // convention (64-bit integers and pointers) is `extern "C" fn(*mut Ctx) -> i64`.
    let entry_fn: gravity_program::Entry = unsafe { core::mem::transmute::<usize, gravity_program::Entry>(compiled.entry(entry)) };
    Ok((compiled, entry_fn))
}

/// `Code::mapped` from `program`'s `entry` compiled through TPDE; the code and its mapper live in
/// their own root context (an mcx `SendRoot`, owned at birth like a code-cache entry's: planprod P2), owned by the `Code`
/// (uncached: the rig and the tests).
pub fn mapped(program: &Program, entry: Func) -> PgResult<Code> {
    let mut context = SendRoot::new_owned("GravityMappedCode", cache::birth_owner());
    let ctx = context.mcx().context();
    // SAFETY: `Mapped` keeps `context` alive past `compiled` (drop order); a SendRoot holds its context box raw, so the
    // address is stable when it moves into `Mapped`; only the compile writes through it, read-only after.
    let ctx: &'static MemoryContext = unsafe { &*(ctx as *const MemoryContext) };
    let (compiled, entry_fn) = compile_in(ctx, program, entry)?;
    let bytes = compiled.code_bytes();
    Ok(Code::mapped(entry_fn, bytes, Box::new(Mapped { compiled, context }))) // alloc: Program
}

/// The statement being admitted's rows-read estimate and bound (`with_row_estimate`): computed at its first miss (a hit
/// reads nothing), then kept.
type Estimate = (*const (dyn Fn() -> (u64, u64) + 'static), Option<(u64, u64)>);

leader_tls::leader_tls! {
    thread_local! {
        /// The estimate of the statement whose admission runs now (`with_row_estimate`); none = unknown.
        static ROW_ESTIMATE: core::cell::Cell<Option<Estimate>> = const { core::cell::Cell::new(None) };
    }
}

/// `f` under `rows`, the rows the statement being admitted reads (the runner's, exec knobs/tier0.rs `rows_read`): a miss
/// inside it is tier 0 only when they are at most the cap. A serial pipeline's own `predicted_rows` is 0 (an index walk's),
/// so this is the gate's row count. `rows` runs at the first miss only (never on an admission hit); outside `f`, or when
/// it cannot tell, a miss compiles at once unless forced. D3867: `rows` answers (the rows read, a bound on the rows any
/// plan node emits, `u64::MAX` = none); a tier-0 code keeps the bound (`Code::tier0_bound`) for the driver's gate.
pub fn with_row_estimate<R>(rows: &dyn Fn() -> (u64, u64), f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Estimate>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ROW_ESTIMATE.with(|e| e.set(self.0));
        }
    }
    // SAFETY: the pointer is read only inside `f` (the guard puts the previous one back when `f` returns or unwinds), so
    // it never outlives `rows`; the 'static is the thread-local's spelling, not a lifetime anything relies on.
    let erased: *const (dyn Fn() -> (u64, u64) + 'static) = unsafe { core::mem::transmute::<*const (dyn Fn() -> (u64, u64) + '_), *const (dyn Fn() -> (u64, u64) + 'static)>(rows) };
    let _restore = Restore(ROW_ESTIMATE.with(|e| e.replace(Some((erased, None)))));
    f()
}

/// The admitted statement's rows read, computed once per admission; `u64::MAX` outside one.
pub(super) fn rows_read() -> (u64, u64) {
    let Some((f, known)) = ROW_ESTIMATE.with(core::cell::Cell::get) else { return (u64::MAX, u64::MAX) };
    if let Some(n) = known {
        return n;
    }
    // SAFETY: set by `with_row_estimate`, whose `rows` is live for as long as the thread-local holds it.
    let n = unsafe { (*f)() };
    ROW_ESTIMATE.with(|e| e.set(Some((f, Some(n)))));
    n
}

gravity_abi::host! {
    #[unwind]
    pub(super) unsafe fn interp_run(state: *const (), ctx: *mut Ctx) -> i64 {
        // SAFETY: `Code::call` passes the data pointer of the `InterpState` box `interp` stored.
        let st = unsafe { &*(state as *const InterpState) };
        // The one landing pad: a trap's panic (or a host's unwind) is parked and `Code::call` resumes it on return.
        gravity_abi::status::unwind::pad(|| match gravity_interp::Interp::new(&st.program).run(st.entry, &[ctx as u64]) {
            Ok(r) => r[0] as i64,
            Err(trap) => panic!("gravity_backend: the interpreter trapped in {}: {trap:?}", st.program.func(st.entry).name),
        })
        .unwrap_or(0)
    }
}

/// The wasm32 server's install path (wasm.rs): the embedder's two host imports, an installed module's entries as table
/// indexes (a wasm32 function pointer IS its index in the server's function table), and the shadow stack's floor.
#[cfg(target_family = "wasm")]
pub(super) mod wasm {
    use gravity_program::Entry;

    #[link(wasm_import_module = "env")]
    extern "C" {
        /// Compile and instantiate `bytes` (a gravity_wasm module importing env.memory, env.__indirect_function_table and
        /// env.__stack_pointer: the server's own), put its exports `entry0..entry<n-1>` in `n` consecutive table slots
        /// and answer the first slot; -1 with the error's text written (NUL-terminated, truncated) to `err`; -2 when the
        /// embedder has no JIT host (a stub).
        fn gravity_install(bytes: *const u8, len: usize, n: u32, err: *mut u8, err_cap: usize) -> i32;
        /// Null the `n` slots from `first` and give them back to the embedder's free list.
        fn gravity_uninstall(first: u32, n: u32);
    }

    extern "C" {
        /// wasm-ld's lowest address of the shadow stack.
        static __stack_low: u8;
    }

    pub(in super::super) enum InstallError {
        NoHost,
        Failed(String),
    }

    /// An installed module's table slots; dropped with the last `Code` that holds it (the cache's `Arc`).
    pub(in super::super) struct Installed {
        first: u32,
        n: u32,
    }

    pub(in super::super) fn install(bytes: &[u8], n: usize) -> Result<Installed, InstallError> {
        let mut err = [0u8; 512];
        // SAFETY: the import reads `len` bytes at `bytes` and writes at most `err_cap` bytes at `err`, both live here.
        let r = unsafe { gravity_install(bytes.as_ptr(), bytes.len(), n as u32, err.as_mut_ptr(), err.len()) };
        match r {
            -2 => Err(InstallError::NoHost),
            r if r < 0 => {
                let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
                Err(InstallError::Failed(String::from_utf8_lossy(&err[..end]).into_owned())) // alloc: Cold
            }
            r => Ok(Installed { first: r as u32, n: n as u32 }),
        }
    }

    impl Installed {
        /// Entry `i`'s function pointer: slot `first + i` holds the module's `entry<i>`, an `(i32) -> i64` wrapper.
        pub(in super::super) fn entry(&self, i: usize) -> Entry {
            assert!(i < self.n as usize, "gravity_backend: entry {i} of an installed module of {}", self.n);
            // SAFETY: the slot holds the module's `entry<i>`, `(i32) -> i64`, which is `Entry`'s wasm32 signature (one
            // pointer in, one i64 out); it stays installed while this `Installed` lives, and every `Code` holding the
            // pointer holds this `Installed` too.
            unsafe { core::mem::transmute::<usize, Entry>(self.first as usize + i) }
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            // SAFETY: the slots were installed for this value and nothing reaches them once it drops (see `entry`).
            unsafe { gravity_uninstall(self.first, self.n) }
        }
    }

    pub(in super::super) fn stack_guard() -> gravity_wasm::StackGuard {
        // SAFETY: only the symbol's address is taken.
        let low = unsafe { core::ptr::addr_of!(__stack_low) } as usize as u32;
        let margin = gravity_program::cost::runtime::WASM_STACK_FLOOR_MARGIN;
        gravity_wasm::StackGuard { floor: low + margin, overflow: stack_overflow as *const () as usize as u32 }
    }

    gravity_abi::host! {
        /// A generated frame would go below the floor: the statement's stack-depth error (PG's 54001), as an unwind
        /// through the generated frames (wasm exception handling crosses them; the catching Rust frame restores the
        /// stack pointer).
        #[unwind]
        fn stack_overflow() {
            std::panic::panic_any(Box::new(
                types_error::PgError::error("stack depth limit exceeded").with_sqlstate(types_error::ERRCODE_STATEMENT_TOO_COMPLEX),
            ));
        }
    }
}
