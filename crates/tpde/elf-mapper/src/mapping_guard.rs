// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{ensure, MapperImage};
use core::{marker::PhantomData, ptr::NonNull};
use mcx::{ExternalAllocationGuard, Mcx};
use tpde_assembler::{AssemblerElf, SymRef};
use types_error::PgResult;

pub struct ElfMapper<'m> {
    image: MapperImage<'m>,
    mapping: Option<MappingGuard<'m>>,
    mcx: Mcx<'m>,
}
impl<'m> ElfMapper<'m> {
    pub fn new(mcx: Mcx<'m>) -> PgResult<Self> {
        Ok(Self {
            image: MapperImage::new(mcx)?,
            mapping: None,
            mcx,
        })
    }
    pub fn reset(&mut self) {
        self.mapping = None;
        self.image.reset();
    }
    /// # Safety
    /// EH frame records and their mapped alignment must be valid for the platform
    /// unwinder. Records, resolved external addresses and everything they reference
    /// must remain valid until reset or destruction. No execution may overlap
    /// mapping or reset.
    pub unsafe fn map(
        &mut self,
        assembler: &mut AssemblerElf<'_>,
        resolver: impl FnMut(&[u8]) -> usize,
    ) -> PgResult<bool> {
        self.reset();
        if !platform::SUPPORTED || assembler.target().elf_machine != platform::MACHINE {
            return Ok(false);
        }
        let page = platform::page_size();
        if page == 0 {
            return Ok(false);
        }
        let size = self.image.layout(assembler, page)?;
        let Some(mut mapping) = MappingGuard::allocate(self.mcx, size, page)? else {
            return Ok(false);
        };
        let base = mapping.address();
        if !self
            .image
            .relocate_zeroed(assembler, mapping.writable_bytes(), base, resolver)?
        {
            self.image.reset();
            return Ok(false);
        }
        // Code is written only into this fresh RW mapping and published by the
        // RW -> RX `protect` below, never into executable pages: on Linux the
        // kernel syncs I/D caches when it installs a user-executable PTE
        // (`__sync_cache_and_tags` -> `__sync_icache_dcache`), as upstream
        // relies on; elsewhere `[0, code_end)` is maintained explicitly.
        platform::publish_code(mapping.pointer, self.image.code_end());
        let permissions = self.image.permissions();
        let protected = permissions.windows(2).all(|window| {
            let from = window[0];
            mapping.protect(from.offset, window[1].offset - from.offset, from.flags)
        });
        if !protected {
            self.image.reset();
            return Ok(false);
        }
        let section = assembler.get_section(self.image.eh_frame_section())?;
        let offset = usize::try_from(section.addr)
            .map_err(|_| crate::invalid("EH frame offset overflow"))?;
        // SAFETY: the caller supplies valid unwind records; layout adds their terminator.
        if !unsafe { mapping.register(offset) } {
            self.image.reset();
            return Ok(false);
        }
        self.mapping = Some(mapping);
        Ok(true)
    }
    pub fn get_sym_addr(&self, symbol: SymRef) -> Option<MappedSymbol<'_>> {
        let mapping = self.mapping.as_ref()?;
        Some(MappedSymbol {
            address: self.image.symbol_address(symbol)?,
            mapping,
        })
    }
    pub fn get_mapped_range(&self) -> Option<MappedRange<'_>> {
        let mapping = self.mapping.as_ref()?;
        Some(MappedRange { mapping })
    }
}

pub struct MappedSymbol<'a> {
    address: usize,
    mapping: &'a MappingGuard<'a>,
}
impl MappedSymbol<'_> {
    pub fn address(&self) -> usize {
        self.address
    }
    /// # Safety
    /// The symbol must be a native `extern "C" fn(u64, u64) -> u64`. Its code,
    /// imports and memory effects must be valid, obey Rust aliasing, and not unwind
    /// through this call. No borrowed image bytes may alias memory the code writes.
    pub unsafe fn call2_u64(&self, a: u64, b: u64) -> u64 {
        let _keep_mapping_alive = self.mapping;
        // SAFETY: the caller establishes the function's address, ABI and effects.
        let function: unsafe extern "C" fn(u64, u64) -> u64 =
            unsafe { core::mem::transmute(self.address) };
        // SAFETY: the mapping borrow covers execution; the caller establishes the ABI.
        unsafe { function(a, b) }
    }
}
pub struct MappedRange<'a> {
    mapping: &'a MappingGuard<'a>,
}
impl MappedRange<'_> {
    pub fn address(&self) -> usize {
        self.mapping.address()
    }
    pub fn len(&self) -> usize {
        self.mapping.size
    }
    pub fn is_empty(&self) -> bool {
        self.mapping.size == 0
    }
    /// # Safety
    /// No code, pointer or other thread may write mapped bytes while the returned
    /// slice is live. Its lifetime must not cross native execution that can write them.
    pub unsafe fn bytes(&self) -> &[u8] {
        // SAFETY: the guard owns this readable mapped extent; the caller excludes writes.
        unsafe { core::slice::from_raw_parts(self.mapping.pointer.as_ptr(), self.mapping.size) }
    }
}

struct MappingGuard<'m> {
    pointer: NonNull<u8>,
    size: usize,
    registered: Option<(usize, platform::Unwind)>,
    _budget: ExternalAllocationGuard<'m>,
    _exclusive: PhantomData<*mut ()>,
}
impl<'m> MappingGuard<'m> {
    fn allocate(mcx: Mcx<'m>, size: usize, page: usize) -> PgResult<Option<Self>> {
        ensure(
            size != 0 && page.is_power_of_two(),
            "invalid mapping extent",
        )?;
        let charged = size
            .checked_add(page - 1)
            .map(|v| v & !(page - 1))
            .ok_or_else(|| crate::invalid("mapping charge overflow"))?;
        let budget = mcx.reserve_external(charged)?;
        let Some(pointer) = arena::allocate(charged, page) else {
            return Ok(None);
        };
        // `size` stays the image's: the mapped range, `code_bytes`, the
        // permission windows and the digests are what they were; the
        // arena's range is the page-rounded `charged`.
        Ok(Some(Self {
            pointer,
            size,
            registered: None,
            _budget: budget,
            _exclusive: PhantomData,
        }))
    }
    fn address(&self) -> usize {
        self.pointer.as_ptr() as usize
    }
    fn writable_bytes(&mut self) -> &mut [u8] {
        // SAFETY: only the unpublished fresh RW mapping can enter this private method.
        unsafe { core::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.size) }
    }
    fn protect(&self, offset: usize, size: usize, flags: u32) -> bool {
        offset.checked_add(size).is_some_and(|end| end <= self.size)
            && platform::protect(self.pointer, offset, size, flags)
    }
    unsafe fn register(&mut self, offset: usize) -> bool {
        if offset.checked_add(4).is_none_or(|end| end > self.size) {
            return false;
        }
        let Some(api) = unwind_api() else {
            return false;
        };
        // SAFETY: bounds were checked and map's caller guarantees valid unwind records.
        unsafe { api.register(self.pointer.as_ptr().add(offset)) };
        self.registered = Some((offset, api));
        true
    }
}
/// The platform's unwind registration API, discovered once per process:
/// `Unwind::discover` is two `dlsym`s on Darwin, the largest single cost
/// of mapping a small program (docs/design/gravity-simple/compile-latency.md
/// §2, lever 2a). The discovered value is a pair of plain function
/// pointers (or "none"), so it is cached in atomics: 0 = not yet looked
/// up, 1 = no registration API, else the `register` pointer, with the
/// `deregister` pointer beside it.
fn unwind_api() -> Option<platform::Unwind> {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static STATE: AtomicUsize = AtomicUsize::new(0);
    static DEREG: AtomicUsize = AtomicUsize::new(0);
    match STATE.load(Ordering::Acquire) {
        0 => {
            let api = platform::Unwind::discover();
            let (state, dereg) = platform::unwind_to_raw(api);
            // Two threads discovering at once store the same values.
            DEREG.store(dereg, Ordering::Release);
            STATE.store(state, Ordering::Release);
            api
        }
        state => platform::unwind_from_raw(state, DEREG.load(Ordering::Acquire)),
    }
}

impl Drop for MappingGuard<'_> {
    fn drop(&mut self) {
        if let Some((offset, api)) = self.registered.take() {
            // SAFETY: exactly the registered live record range remains mapped until release.
            unsafe { api.deregister(self.pointer.as_ptr().add(offset)) };
        }
        arena::release(self.pointer, self.size);
    }
}

/// The mapping arena (docs/design/gravity-simple/compile-latency.md §2,
/// lever 2b): images are bump-allocated, page-aligned, out of 1 MB
/// chunks mapped once and touched once, instead of one `mmap` (and its
/// first-touch faults, and one `munmap`) per program. A chunk counts its
/// live images; an empty chunk is reused from its start (its pages may
/// be executable from the previous image: the reused range is flipped
/// back to read-write here) and an empty chunk that is not the only one
/// is unmapped. Each image keeps its own permission flips, its icache
/// maintenance and its unwind registration, exactly as before. One
/// process-wide arena behind a spin lock (a backend is single-threaded;
/// the harness's test threads contend only for the bookkeeping).
mod arena {
    use super::platform;
    use core::cell::UnsafeCell;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicBool, Ordering};
    use tpde_assembler::elf::SHF_WRITE;

    const CHUNK: usize = 1 << 20;

    struct Chunk {
        base: usize,
        size: usize,
        /// Bump offset of the next image.
        used: usize,
        /// Bytes ever handed out: pages below it may be executable.
        high: usize,
        live: usize,
        /// G1-A door 2 + C8 move 7: the chunk's pages on the program caches' charge while it is mapped.
        charge: memledger::Charge,
    }

    struct State {
        chunks: alloc::vec::Vec<Chunk>,
    }

    struct Arena {
        lock: AtomicBool,
        state: UnsafeCell<Option<State>>,
    }
    // SAFETY: `state` is only touched while `lock` is held (`with`).
    unsafe impl Sync for Arena {}

    static ARENA: Arena = Arena { lock: AtomicBool::new(false), state: UnsafeCell::new(None) };

    fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        while ARENA.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held; no other reference to the state exists.
        let state = unsafe { &mut *ARENA.state.get() };
        let r = f(state.get_or_insert_with(|| State { chunks: alloc::vec::Vec::new() }));
        ARENA.lock.store(false, Ordering::Release);
        r
    }

    /// A page-aligned range of `size` bytes (`size` a multiple of `page`),
    /// readable and writable.
    pub fn allocate(size: usize, page: usize) -> Option<NonNull<u8>> {
        with(|st| {
            let mut idx = st.chunks.iter().position(|c| c.size - c.used >= size);
            if idx.is_none() {
                let csize = if size > CHUNK { size } else { CHUNK };
                let base = platform::allocate(csize)?;
                mempolicy::maps::name(base.as_ptr(), csize, c"pgrust:jit_code");
                // Touch every page once, off the per-image path.
                let mut off = 0;
                while off < csize {
                    // SAFETY: a fresh private read-write mapping of `csize` bytes.
                    unsafe { base.as_ptr().add(off).write_volatile(0) };
                    off += page;
                }
                // G1-A door 2: the process-wide code arena is the program caches' memory (touched whole just above)
                let owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));
                let charge = memledger::Charge::open(owner, memledger::Release::Evict, memledger::Kind::Mapped, memledger::charge::Door::Mapped, csize as u64);
                st.chunks.push(Chunk { base: base.as_ptr() as usize, size: csize, used: 0, high: 0, live: 0, charge });
                idx = Some(st.chunks.len() - 1);
            }
            let c = &mut st.chunks[idx?];
            let off = c.used;
            let base = NonNull::new(c.base as *mut u8)?;
            if off < c.high {
                // Reused pages: back to read-write (the previous image left
                // them executable) and zeroed (the image writer assumes a
                // zero-filled range: the gaps between sections are never
                // written, and an image must not carry another's bytes).
                // Refuse the range rather than hand out pages that cannot
                // be written.
                let end = (off + size).min(c.high);
                if !platform::protect(base, off, end - off, SHF_WRITE) {
                    return None;
                }
                // SAFETY: `[off, end)` is inside the chunk's mapping, now
                // read-write, and no image is live in it.
                unsafe { core::ptr::write_bytes(base.as_ptr().add(off), 0, end - off) };
            }
            c.used = off + size;
            if c.used > c.high {
                c.high = c.used;
            }
            c.live += 1;
            NonNull::new((c.base + off) as *mut u8)
        })
    }

    pub fn release(pointer: NonNull<u8>, _size: usize) {
        let p = pointer.as_ptr() as usize;
        with(|st| {
            let Some(i) = st.chunks.iter().position(|c| p >= c.base && p < c.base + c.size) else { return };
            let c = &mut st.chunks[i];
            c.live -= 1;
            if c.live == 0 {
                c.used = 0;
                if st.chunks.len() > 1 {
                    let c = st.chunks.remove(i);
                    let (at, size) = (c.base, c.size);
                    drop(c.charge);
                    if let Some(base) = NonNull::new(at as *mut u8) {
                        platform::release(base, size);
                    }
                }
            }
        });
    }
}

#[cfg(all(
    unix,
    not(target_vendor = "apple"),
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
mod platform {
    use core::{ffi::c_void, ptr::NonNull};
    use tpde_assembler::elf::{SHF_EXECINSTR, SHF_WRITE};
    pub const SUPPORTED: bool = true;
    #[cfg(target_arch = "aarch64")]
    pub const MACHINE: u16 = tpde_assembler::elf::EM_AARCH64;
    #[cfg(target_arch = "x86_64")]
    pub const MACHINE: u16 = tpde_assembler::elf::EM_X86_64;
    unsafe extern "C" {
        fn __register_frame(frame: *mut c_void);
        fn __deregister_frame(frame: *mut c_void);
    }
    #[derive(Clone, Copy)]
    pub enum Unwind {
        Gcc,
        Llvm {
            add: unsafe extern "C" fn(usize),
            remove: unsafe extern "C" fn(usize),
        },
    }
    /// `Unwind` as the cache's raw words (`super::unwind_api`): 1 = none,
    /// 2 = the GCC API, else the LLVM `add` pointer with `remove` beside it.
    pub fn unwind_to_raw(u: Option<Unwind>) -> (usize, usize) {
        match u {
            None => (1, 0),
            Some(Unwind::Gcc) => (2, 0),
            Some(Unwind::Llvm { add, remove }) => (add as usize, remove as usize),
        }
    }
    pub fn unwind_from_raw(state: usize, dereg: usize) -> Option<Unwind> {
        match state {
            1 => None,
            2 => Some(Unwind::Gcc),
            // SAFETY: the words were stored by `unwind_to_raw` from these types.
            _ => Some(unsafe { Unwind::Llvm { add: core::mem::transmute(state), remove: core::mem::transmute(dereg) } }),
        }
    }
    impl Unwind {
        /// Upstream binds `__unw_add/remove_dynamic_eh_frame_section` as weak
        /// externs, resolved once when the process is loaded. The runtime lookup
        /// is likewise performed once per process and its outcome (GCC, LLVM
        /// pair, or refusal because the remove hook is missing) is retained;
        /// every mapping still records the API it registered with.
        pub fn discover() -> Option<Self> {
            use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
            const UNKNOWN: u8 = 0;
            const GCC: u8 = 1;
            const REFUSED: u8 = 2;
            const LLVM: u8 = 3;
            static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
            static ADD: AtomicUsize = AtomicUsize::new(0);
            static REMOVE: AtomicUsize = AtomicUsize::new(0);
            match STATE.load(Ordering::Acquire) {
                GCC => return Some(Self::Gcc),
                REFUSED => return None,
                LLVM => {
                    // SAFETY: both slots were published with the fixed libunwind ABI below.
                    return Some(unsafe {
                        Self::Llvm {
                            add: core::mem::transmute::<usize, unsafe extern "C" fn(usize)>(
                                ADD.load(Ordering::Relaxed),
                            ),
                            remove: core::mem::transmute::<usize, unsafe extern "C" fn(usize)>(
                                REMOVE.load(Ordering::Relaxed),
                            ),
                        }
                    });
                }
                _ => {}
            }
            let found = Self::lookup();
            // Concurrent first discoveries publish the same load-time answer.
            match found {
                Some(Self::Gcc) => STATE.store(GCC, Ordering::Release),
                None => STATE.store(REFUSED, Ordering::Release),
                Some(Self::Llvm { add, remove }) => {
                    ADD.store(add as usize, Ordering::Relaxed);
                    REMOVE.store(remove as usize, Ordering::Relaxed);
                    STATE.store(LLVM, Ordering::Release);
                }
            }
            found
        }
        fn lookup() -> Option<Self> {
            // SAFETY: both names are terminated literals; RTLD_DEFAULT searches loaded objects.
            let add = unsafe {
                libc::dlsym(
                    libc::RTLD_DEFAULT,
                    c"__unw_add_dynamic_eh_frame_section".as_ptr(),
                )
            };
            if add.is_null() {
                return Some(Self::Gcc);
            }
            // SAFETY: same lookup contract; LLVM's pair must both exist before registration.
            let remove = unsafe {
                libc::dlsym(
                    libc::RTLD_DEFAULT,
                    c"__unw_remove_dynamic_eh_frame_section".as_ptr(),
                )
            };
            if remove.is_null() {
                return None;
            }
            // SAFETY: these names have the fixed libunwind ABI used by pinned upstream.
            Some(unsafe {
                Self::Llvm {
                    add: core::mem::transmute(add),
                    remove: core::mem::transmute(remove),
                }
            })
        }
        pub unsafe fn register(self, frame: *mut u8) {
            // SAFETY: the owner guarantees a live valid terminated EH frame section.
            unsafe {
                match self {
                    Self::Gcc => __register_frame(frame.cast()),
                    Self::Llvm { add, .. } => add(frame as usize),
                }
            }
        }
        pub unsafe fn deregister(self, frame: *mut u8) {
            // SAFETY: this exactly matches the selected registration API and record address.
            unsafe {
                match self {
                    Self::Gcc => __deregister_frame(frame.cast()),
                    Self::Llvm { remove, .. } => remove(frame as usize),
                }
            }
        }
    }
    pub fn page_size() -> usize {
        // SAFETY: querying the page size has no pointer or ownership preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 && (size as usize).is_power_of_two() {
            size as usize
        } else {
            0
        }
    }
    pub fn allocate(size: usize) -> Option<NonNull<u8>> {
        // SAFETY: a nonzero bounded anonymous private allocation needs no source pointer.
        let pointer = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return None;
        }
        if pointer.is_null() {
            // SAFETY: even a null successful mapping must be released before refusal.
            unsafe {
                libc::munmap(pointer, size);
            }
            return None;
        }
        NonNull::new(pointer.cast())
    }
    pub fn protect(pointer: NonNull<u8>, offset: usize, size: usize, flags: u32) -> bool {
        let prot = libc::PROT_READ
            | if flags & SHF_EXECINSTR != 0 {
                libc::PROT_EXEC
            } else {
                0
            }
            | if flags & SHF_WRITE != 0 {
                libc::PROT_WRITE
            } else {
                0
            };
        // SAFETY: private caller has checked the live mapped subrange and page boundaries.
        unsafe { libc::mprotect(pointer.as_ptr().add(offset).cast(), size, prot) == 0 }
    }
    pub fn release(pointer: NonNull<u8>, size: usize) {
        // SAFETY: guard owns this mapping and calls release once after unregistering frames.
        unsafe {
            libc::munmap(pointer.as_ptr().cast(), size);
        }
    }
    #[cfg(any(target_arch = "x86_64", target_os = "linux"))]
    pub fn publish_code(_: NonNull<u8>, _: usize) {}
    #[cfg(all(target_arch = "aarch64", not(target_os = "linux")))]
    pub fn publish_code(pointer: NonNull<u8>, size: usize) {
        let start = pointer.as_ptr() as usize;
        let end = start + size;
        let ctr: usize;
        // SAFETY: CTR_EL0 is userspace-readable and these maintenance operations target owned bytes.
        unsafe {
            core::arch::asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nostack, preserves_flags));
            let data_line = 4usize << ((ctr >> 16) & 15);
            let mut cursor = start & !(data_line - 1);
            while cursor < end {
                core::arch::asm!("dc cvau, {cursor}", cursor = in(reg) cursor, options(nostack, preserves_flags));
                cursor += data_line;
            }
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
            let instruction_line = 4usize << (ctr & 15);
            cursor = start & !(instruction_line - 1);
            while cursor < end {
                core::arch::asm!("ic ivau, {cursor}", cursor = in(reg) cursor, options(nostack, preserves_flags));
                cursor += instruction_line;
            }
            core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
        }
    }
}

#[cfg(all(unix, target_vendor = "apple", any(target_arch = "aarch64", target_arch = "x86_64")))]
mod platform {
    //! Darwin (arm64, and x86_64 natively or under Rosetta 2): anonymous RW
    //! mapping flipped to RX with `mprotect` (allowed outside the hardened
    //! runtime, which the release binaries do not use: they are ad-hoc
    //! signed; a hardened build would need `MAP_JIT`, the allow-jit
    //! entitlement and `pthread_jit_write_protect_np`), instruction cache
    //! maintained by hand (a no-op on x86_64), unwind records registered
    //! through libunwind's `__register_frame` when it is exported and
    //! skipped otherwise.
    use core::{ffi::c_void, ptr::NonNull};
    use tpde_assembler::elf::{SHF_EXECINSTR, SHF_WRITE};
    pub const SUPPORTED: bool = true;
    #[cfg(target_arch = "aarch64")]
    pub const MACHINE: u16 = tpde_assembler::elf::EM_AARCH64;
    #[cfg(target_arch = "x86_64")]
    pub const MACHINE: u16 = tpde_assembler::elf::EM_X86_64;
    #[derive(Clone, Copy)]
    pub enum Unwind {
        None,
        Frame {
            register: unsafe extern "C" fn(*mut c_void),
            deregister: unsafe extern "C" fn(*mut c_void),
        },
    }
    impl Unwind {
        pub fn discover() -> Option<Self> {
            // SAFETY: terminated literals; RTLD_DEFAULT searches loaded images.
            let (reg, dereg) = unsafe {
                (
                    libc::dlsym(libc::RTLD_DEFAULT, c"__register_frame".as_ptr()),
                    libc::dlsym(libc::RTLD_DEFAULT, c"__deregister_frame".as_ptr()),
                )
            };
            if reg.is_null() || dereg.is_null() {
                return Some(Self::None);
            }
            // SAFETY: fixed libunwind ABI.
            Some(unsafe {
                Self::Frame {
                    register: core::mem::transmute(reg),
                    deregister: core::mem::transmute(dereg),
                }
            })
        }
        pub unsafe fn register(self, frame: *mut u8) {
            if let Self::Frame { register, .. } = self {
                // SAFETY: the owner guarantees a live terminated EH frame section.
                unsafe { register(frame.cast()) }
            }
        }
        pub unsafe fn deregister(self, frame: *mut u8) {
            if let Self::Frame { deregister, .. } = self {
                // SAFETY: matches the registration above.
                unsafe { deregister(frame.cast()) }
            }
        }
    }
    /// `Unwind` as the cache's raw words (`super::unwind_api`).
    pub fn unwind_to_raw(u: Option<Unwind>) -> (usize, usize) {
        match u {
            None | Some(Unwind::None) => (1, 0),
            Some(Unwind::Frame { register, deregister }) => (register as usize, deregister as usize),
        }
    }
    pub fn unwind_from_raw(state: usize, dereg: usize) -> Option<Unwind> {
        if state == 1 {
            return Some(Unwind::None);
        }
        // SAFETY: the words were stored by `unwind_to_raw` from these types.
        Some(unsafe { Unwind::Frame { register: core::mem::transmute(state), deregister: core::mem::transmute(dereg) } })
    }
    pub fn page_size() -> usize {
        // SAFETY: no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size > 0 && (size as usize).is_power_of_two() {
            size as usize
        } else {
            0
        }
    }
    pub fn allocate(size: usize) -> Option<NonNull<u8>> {
        // SAFETY: anonymous private mapping, no source pointer.
        let pointer = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED || pointer.is_null() {
            return None;
        }
        NonNull::new(pointer.cast())
    }
    pub fn protect(pointer: NonNull<u8>, offset: usize, size: usize, flags: u32) -> bool {
        let prot = libc::PROT_READ
            | if flags & SHF_EXECINSTR != 0 { libc::PROT_EXEC } else { 0 }
            | if flags & SHF_WRITE != 0 { libc::PROT_WRITE } else { 0 };
        // SAFETY: the caller checked the live mapped subrange and page boundaries.
        unsafe { libc::mprotect(pointer.as_ptr().add(offset).cast(), size, prot) == 0 }
    }
    pub fn release(pointer: NonNull<u8>, size: usize) {
        // SAFETY: the guard owns this mapping and releases once.
        unsafe {
            libc::munmap(pointer.as_ptr().cast(), size);
        }
    }
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }
    pub fn publish_code(pointer: NonNull<u8>, size: usize) {
        // Darwin gives no kernel-side I/D sync on the RW -> RX flip, so the
        // executable extent `[0, code_end)` is maintained explicitly.
        // SAFETY: libSystem's cache maintenance over owned bytes; CTR_EL0 is
        // not readable from user space on Darwin.
        unsafe { sys_icache_invalidate(pointer.as_ptr().cast(), size) }
    }
}

#[cfg(not(any(
    all(unix, not(target_vendor = "apple"), any(target_arch = "aarch64", target_arch = "x86_64")),
    all(unix, target_vendor = "apple", any(target_arch = "aarch64", target_arch = "x86_64"))
)))]
mod platform {
    use core::ptr::NonNull;
    pub const SUPPORTED: bool = false;
    pub const MACHINE: u16 = 0;
    #[derive(Clone, Copy)]
    pub struct Unwind;
    impl Unwind {
        pub fn discover() -> Option<Self> {
            None
        }
        pub unsafe fn register(self, _: *mut u8) {}
        pub unsafe fn deregister(self, _: *mut u8) {}
    }
    pub fn unwind_to_raw(_: Option<Unwind>) -> (usize, usize) {
        (1, 0)
    }
    pub fn unwind_from_raw(_: usize, _: usize) -> Option<Unwind> {
        None
    }
    pub fn page_size() -> usize {
        0
    }
    pub fn allocate(_: usize) -> Option<NonNull<u8>> {
        None
    }
    pub fn protect(_: NonNull<u8>, _: usize, _: usize, _: u32) -> bool {
        false
    }
    pub fn release(_: NonNull<u8>, _: usize) {}
    pub fn publish_code(_: NonNull<u8>, _: usize) {}
}
const _: () = {
    assert!(core::mem::size_of::<ElfMapper<'static>>() <= 128);
    assert!(core::mem::needs_drop::<ElfMapper<'static>>());
    assert!(!core::mem::needs_drop::<MappedSymbol<'static>>());
    assert!(!core::mem::needs_drop::<MappedRange<'static>>());
};
