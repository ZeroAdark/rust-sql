use core::sync::atomic::{AtomicU8, Ordering};

use crate::layout::{unword, word, Word};
use crate::program::ProgramBlock;
use crate::site::Site;

pub type Datum = u64;

crate::block! {
    /// The Ctx header (abi-v2 §2.1, `docs/gravity/abi.md`): per-Call cells over per-Binding pointers.
    Ctx size 128 offsets hdr {
        program => PROGRAM: Word<*mut ProgramBlock>,
        sites => SITES: Word<*mut Site>,
        out => OUT: Word<*mut Out>,
        cancel => CANCEL: Word<*const u8>,
        arena_cur => ARENA_CUR: Word<*mut u8>,
        arena_end => ARENA_END: Word<*mut u8>,
        budget_bytes => BUDGET_BYTES: u64,
        spill => SPILL_STATE: Word<*mut u8>,
        claim => CLAIM: MorselRange,
        resume => RESUME: i64,
        params => PARAMS: Word<*const Datum>,
        params_null => PARAMS_NULL: Word<*const u8>,
        identity => IDENTITY: Word<*const Identity>,
        flags => FLAGS: u32,
        worker_ordinal => WORKER_ORDINAL: u16,
        width => WIDTH: u16,
        nsites => NSITES: u16,
        /// D3862: the hosted lane (lowerv4 hir.rs) worker ordinal 0 takes: 0 at the top level; inside a nested run (D1036)
        /// a pool lane past the section's workers, never lane 0, whose pending results the outer row still emits.
        host_lane0 => HOST_LANE0: u16,
        pad => PAD: [u16; 2],
    }
    extra {
        pub const CLAIM_LO: usize = CLAIM;
        pub const CLAIM_HI: usize = CLAIM + 8;
    }
}

crate::block! {
    /// The output block: `count` rows of `out_natts` columns, row-major, pushed at `cap`. The by-reference
    /// region (lower.md §15.13): `region_len` of `region_cap` bytes behind `region`, the images a by-reference
    /// output column's datums point into (copied at the emit, 8-aligned), shipped with the block, emptied by the push.
    Out size 56 offsets out {
        values => VALUES: Word<*mut Datum>,
        isnull => ISNULL: Word<*mut u8>,
        count => COUNT: i64,
        cap => CAP: i64,
        region => REGION: Word<*mut u8>,
        region_cap => REGION_CAP: i64,
        region_len => REGION_LEN: i64,
    }
}

crate::block! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    MorselRange size 16 offsets morsel {
        lo => LO: u64,
        hi => HI: u64,
    }
}

crate::block! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    Identity size 32 offsets identity {
        proc_number => PROC_NUMBER: i32,
        command_id => COMMAND_ID: u32,
        resource_owner => RESOURCE_OWNER: u64,
        latch => LATCH: u64,
        snapshot => SNAPSHOT: u64,
    }
}

/// Equality and hashing ignore `POS_ROW`: a row's own position and the same tid served from a
/// window base are the same position.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanPos {
    pub flags: u16,
    pub off: u16,
    pub rel: u32,
    pub block: u32,
}

pub mod scanpos {
    pub const FLAGS: usize = 0;
    pub const OFF: usize = 2;
    pub const REL: usize = 4;
    pub const BLOCK: usize = 8;
    pub const SIZE: usize = 12;
    pub const FIELDS: &[(&str, usize, usize)] = &[("flags", FLAGS, 2), ("off", OFF, 2), ("rel", REL, 4), ("block", BLOCK, 4)];
}

const _: () = assert!(core::mem::size_of::<ScanPos>() == scanpos::SIZE);

crate::block! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    Answer size 56 offsets answer {
        nonnull => NONNULL: u64,
        sum_lo => SUM_LO: u64,
        sum_hi => SUM_HI: u64,
        min => MIN: i64,
        max => MAX: i64,
        flags => FLAGS: u64,
        false_count => FALSE_COUNT: u64,
    }
}

crate::block! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    Lane size 32 offsets lane {
        ptr => PTR: Word<*const u8>,
        valid => VALID: Word<*const u64>,
        width => WIDTH: u8,
        kind => KIND: u8,
        region => REGION: u8,
        flags => FLAGS: u8,
        ncodes_or_scale => NCODES_OR_SCALE: u32,
        id => ID: u32,
        aux => AUX: u32,
    }
    extra {
        pub const KIND_BYVAL: u8 = 0;
        pub const KIND_BYREF: u8 = 1;
        pub const KIND_CODES: u8 = 2;
        pub const KIND_FIXED: u8 = 3;
        pub const REGION_PAGE: u8 = 0;
        pub const REGION_ARENA: u8 = 1;
        pub const REGION_DICTIONARY: u8 = 2;
        pub const REGION_BREAKER: u8 = 3;
        pub const REGION_IMMEDIATE: u8 = 4;
        pub const FLAG_ALL_VALID: u8 = 1;
        pub const FLAG_SORTED: u8 = 2;
        pub const VERDICT_SHIFT: u8 = 2;
        pub const VERDICT_MASK: u8 = 3 << VERDICT_SHIFT;
        pub const VERDICT_MIXED: u8 = 0;
        pub const VERDICT_ALL_MATCH: u8 = 1 << VERDICT_SHIFT;
        pub const VERDICT_NONE_MATCH: u8 = 2 << VERDICT_SHIFT;
        pub const FLAG_ANSWERED: u8 = 16;
        /// A code lane's window is its part's dictionary entries (one row per code, in rank order), not rows.
        pub const FLAG_ENTRIES: u8 = 32;
        /// A code lane whose part carries a delete vector: a dictionary entry may be referenced by dead rows only.
        pub const FLAG_DELETES: u8 = 64;
        /// Every row of this window satisfies every registered zone predicate over THIS lane (the lane may be undecoded: `ptr` null).
        pub const FLAG_LANE_ALL_MATCH: u8 = 128;
        pub const ID_KIND_SHIFT: u32 = 24;
        pub const ID_ATTR: u32 = 0;
        pub const ID_ROWID: u32 = 1;
        pub const ID_OFFSET: u32 = 2;
        pub const ID_VERDICT: u32 = 3;
        pub const ID_PROJECTION: u32 = 4;
        pub const ID_JSONB: u32 = 5;
        /// A per-code table over a code lane (`gravity_dict::lane`): `ptr` is the table header,
        /// `aux` the code lane's index, the ordinal the table's fingerprint slot.
        pub const ID_TABLE: u32 = 6;
        /// `Lane.aux` of a code lane whose part holds no empty string.
        pub const EMPTY_CODE_NONE: u32 = u32::MAX;
        /// The part id of the code word `EMPTY_PART << 32` — the empty string of no part: a kernel
        /// keys it when a part holds no `''` entry, every source renders it as `''`.
        pub const EMPTY_PART: u32 = u32::MAX;

        pub const fn id(kind: u32, ordinal: u32) -> u32 {
            (kind << ID_KIND_SHIFT) | (ordinal & 0x00ff_ffff)
        }

        pub const fn id_kind(id: u32) -> u32 {
            id >> ID_KIND_SHIFT
        }

        pub const fn id_ordinal(id: u32) -> u32 {
            id & 0x00ff_ffff
        }
    }
}

crate::block! {
    /// What `ProgramBlock.partial` points at: the produced region and the consumed ones (`breakers.md`):
    /// the first two consumed regions inline (`consumed`), every one at `more[k]` (decision 82: a pipeline
    /// consumes any number of slots; `more` is the driver's table of `Pipeline.consumes.len()` regions).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    Partials size 32 offsets partials {
        own => OWN: Word<*mut u8>,
        consumed => CONSUMED: [Word<*mut u8>; 2],
        more => MORE: Word<*const Word<*mut u8>>,
    }
    extra {
        pub const CONSUMED0: usize = CONSUMED;
        pub const CONSUMED1: usize = CONSUMED + 8;
    }
}

pub use crate::lane::EMPTY_CODE_NONE;

pub const HEADER_SIZE: usize = hdr::SIZE;
pub const OUT_SIZE: usize = out::SIZE;
pub const LANE_SIZE: usize = lane::SIZE;
pub const IDENTITY_SIZE: usize = identity::SIZE;
pub const ANSWER_SIZE: usize = answer::SIZE;
pub const PARTIALS_SIZE: usize = partials::SIZE;
pub const CELL: usize = 8;

pub const FLAG_RESUME: u32 = 1;
/// D393b (V4R-57): the claim's pipeline is an arm of a union whose arms continue one breaker slot (the driver's append.arms
/// run): a phase-1 table opens at `ARM_RUN_PBITS` whatever its radix bits and the claim's width, so every arm's table, the
/// workers' partials and the slot's spill files partition alike.
pub const FLAG_ARM_RUN: u32 = 2;
/// D2881: the call is a later claim of its pipeline's run on the leader (`Entry::claim_at`): the pipeline opened at the run's
/// first claim, which counts the run's one EXPLAIN ANALYZE loop. A pgrcolumnar table's parts are claims, where PostgreSQL's
/// node starts its scan once.
pub const FLAG_LATER_CLAIM: u32 = 4;
/// D824 (V4R-298): bit `FLAG_PENDING_SHIFT + i` set: the call's pipeline's `i`-th consumed slot is a lazy build that has not
/// run (PG's outer-first hash join: the build waits for the first outer row); its region is open, zeroed. i < 8.
pub const FLAG_PENDING_SHIFT: u32 = 8;
/// D3168: the call reruns a plain pipeline function with its probe stages off: no staged function was lowered for it
/// (gravity_program `Code::call`), so its guard takes the per-row body even past L2.
pub const FLAG_STAGE_OFF: u32 = 1 << 16;
/// The partition bits of an arm run's phase-1 tables (the W8 floor; a huge keyed union re-splits on refill: D404).
pub const ARM_RUN_PBITS: u8 = 6;
pub const RESUME_NONE: i64 = 0;
pub const POS_VALID: u16 = 1;
pub const POS_ROW: u16 = 2;
/// D732: an EvalPlanQual recheck's COPY mark site: `rel` + `block` hold the marked row's datum, which its scan emits alone.
pub const POS_COPY: u16 = 4;
pub const ANSWER_SUM: u64 = 1;
pub const ANSWER_MINMAX: u64 = 2;

unsafe impl Send for Lane {}
unsafe impl Sync for Lane {}

impl MorselRange {
    pub const ALL: MorselRange = MorselRange { lo: 0, hi: u64::MAX };
    /// The section-end call (milestone-2.md §2.6 item 20): once per worker after its last claim.
    pub const SECTION_END: MorselRange = MorselRange { lo: u64::MAX, hi: u64::MAX };

    pub const fn is_section_end(&self) -> bool {
        self.lo == u64::MAX
    }

    pub const fn new(lo: u64, hi: u64) -> MorselRange {
        MorselRange { lo, hi }
    }

    pub const fn len(&self) -> u64 {
        self.hi.saturating_sub(self.lo)
    }

    pub const fn is_empty(&self) -> bool {
        self.hi <= self.lo
    }

    pub const fn clamp(&self, total: u64) -> MorselRange {
        let hi = if self.hi < total { self.hi } else { total };
        let lo = if self.lo < hi { self.lo } else { hi };
        MorselRange { lo, hi }
    }
}

impl PartialEq for ScanPos {
    fn eq(&self, o: &ScanPos) -> bool {
        (self.flags & POS_VALID, self.off, self.rel, self.block) == (o.flags & POS_VALID, o.off, o.rel, o.block)
    }
}

impl Eq for ScanPos {}

impl core::hash::Hash for ScanPos {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        (self.flags & POS_VALID, self.off, self.rel, self.block).hash(h)
    }
}

impl ScanPos {
    pub const NONE: ScanPos = ScanPos { flags: 0, off: 0, rel: 0, block: 0 };

    pub const fn new(rel: u32, block: u32, off: u16) -> ScanPos {
        ScanPos { flags: POS_VALID, off, rel, block }
    }

    pub const fn row(rel: u32, block: u32, off: u16) -> ScanPos {
        ScanPos { flags: POS_VALID | POS_ROW, off, rel, block }
    }

    pub const fn is_valid(&self) -> bool {
        self.flags & POS_VALID != 0
    }

    pub const fn is_row(&self) -> bool {
        self.flags & POS_ROW != 0
    }
}

impl Answer {
    pub const NONE: Answer = Answer { nonnull: 0, sum_lo: 0, sum_hi: 0, min: 0, max: 0, flags: 0, false_count: 0 };

    pub const fn sum(&self) -> i128 {
        ((self.sum_hi as u128) << 64 | self.sum_lo as u128) as i128
    }

    pub fn set_sum(&mut self, v: i128) {
        self.sum_lo = v as u128 as u64;
        self.sum_hi = (v as u128 >> 64) as u64;
        self.flags |= ANSWER_SUM;
    }
}

impl Lane {
    pub const NONE: Lane = Lane { ptr: word(core::ptr::null()), valid: word(core::ptr::null()), width: 0, kind: 0, region: 0, flags: 0, ncodes_or_scale: 0, id: 0, aux: 0 };

    pub const fn byval(ptr: *const u8, valid: *const u64, width: u8, region: u8, id: u32) -> Lane {
        let flags = if valid.is_null() { lane::FLAG_ALL_VALID } else { 0 };
        Lane { ptr: word(ptr), valid: word(valid), width, kind: lane::KIND_BYVAL, region, flags, ncodes_or_scale: 0, id, aux: 0 }
    }

    pub const fn all_valid(&self) -> bool {
        unword(self.valid).is_null()
    }

    /// # Safety — `i < rows` of the window that published this lane.
    #[inline(always)]
    pub unsafe fn is_valid(&self, i: usize) -> bool {
        let valid = unword(self.valid);
        valid.is_null() || (*valid.add(i / 64) >> (i % 64)) & 1 != 0
    }

    /// The element as a sign-extended datum word (int2/int4/int8 lanes).
    /// # Safety — `i < rows` of the window that published this lane.
    #[inline(always)]
    pub unsafe fn datum(&self, i: usize) -> Datum {
        let ptr = unword(self.ptr);
        match self.width {
            8 => *(ptr as *const u64).add(i),
            4 => *(ptr as *const i32).add(i) as i64 as u64,
            2 => *(ptr as *const i16).add(i) as i64 as u64,
            _ => *ptr.add(i) as u64,
        }
    }
}

impl Partials {
    pub const NONE: Partials = Partials { own: word(core::ptr::null_mut()), consumed: [word(core::ptr::null_mut()); 2], more: word(core::ptr::null()) };

    /// The two inline regions (the 8-key tier's face; `more` null: no third).
    pub const fn two(own: *mut u8, consumed: [*mut u8; 2]) -> Partials {
        Partials { own: word(own), consumed: [word(consumed[0]), word(consumed[1])], more: word(core::ptr::null()) }
    }
}

/// The resume word (milestone-1b §2 decision 9): `phase` in the top 16 bits, `position` in the
/// low 48. Build and probe phases run to completion or abort; a row-limited Execute suspends in `EMIT`.
pub mod resume {
    pub const NONE: u16 = 0;
    pub const BUILD: u16 = 1;
    pub const PROBE: u16 = 2;
    pub const SPILL_BATCH: u16 = 3;
    pub const EMIT: u16 = 4;
    /// The word's bits below the 16-bit phase.
    pub const POSITION_BITS: u32 = i64::BITS - u16::BITS;
    pub const POSITION_MASK: u64 = (1 << POSITION_BITS) - 1;

    pub const fn word(phase: u16, position: u64) -> i64 {
        (((phase as u64) << POSITION_BITS) | (position & POSITION_MASK)) as i64
    }

    pub const fn phase(word: i64) -> u16 {
        ((word as u64) >> POSITION_BITS) as u16
    }

    pub const fn position(word: i64) -> u64 {
        (word as u64) & POSITION_MASK
    }

    /// Whether a suspended kernel may be resumed: only an emit over a finished breaker.
    pub const fn resumable(word: i64) -> bool {
        phase(word) == EMIT
    }
}

pub const fn site_off(i: usize) -> usize {
    HEADER_SIZE + i * crate::site::SITE_SIZE
}

pub const fn ctx_size(nsites: usize) -> usize {
    site_off(nsites)
}

/// # Safety — `ctx` points at a ctx of at least `i + 1` sites.
#[inline(always)]
pub unsafe fn site_ptr(ctx: *mut Ctx, i: usize) -> *mut Site {
    unword((*ctx).sites).add(i)
}

/// # Safety — `ctx` points at a ctx and `off` is a cell offset inside it.
pub unsafe fn cell(ctx: *mut Ctx, off: usize) -> *mut u64 {
    (ctx as *mut u8).add(off) as *mut u64
}

/// The abort byte's bits (init_small::globals, the same values): `ABORT_BIT` a cancel, a termination or an abort-class
/// timeout is pending; `SERVICE_BIT` (D3406) a deferrable interrupt of the byte's owner is (a ProcSignalBarrier, a
/// memory-context dump), which a kernel of the owner services through `poll` and goes on; `REVOKE_BIT` (C5) the
/// statement was asked to spill (mcx::revoke), which any kernel of the statement services through `poll` and goes on
/// (bit 4 is the D3887 poll-gap probe's, cancelpoll `PROBE_BIT`).
pub const ABORT_BIT: u8 = 1;
pub const SERVICE_BIT: u8 = 2;
pub const REVOKE_BIT: u8 = 8;

/// The one abort poll: the statement's abort byte has its abort bit set (a cancel or a timeout is pending). Every loop
/// that stops for a cancel without polling — the owner phase, a pool worker, the section's end — reads it through here.
#[inline(always)]
pub fn abort_set(flag: &AtomicU8) -> bool {
    flag.load(Ordering::Acquire) & ABORT_BIT != 0
}

/// `abort_set` over the byte a binding lends (`Ctx::cancel`; null: none).
/// # Safety — `cancel` is null or a live abort byte.
#[inline(always)]
pub unsafe fn abort_byte_set(cancel: *const u8) -> bool {
    !cancel.is_null() && abort_set(&*(cancel as *const AtomicU8))
}

/// C9 (D4140): the units a kernel-side host loop of about 100 ns a unit or less runs between two `cancel_pending` checks
/// (kernels ops/sort/polled.rs `Poll::batch`): `cancelpoll::BATCH`'s value, where a check per unit costs 3 instructions and
/// stops the vectorizer while one per 4096 units costs under 1 % (c8g, lanes/keyrep/c9-design.md section 3).
pub const POLL_BATCH: usize = 1 << 12;

/// C9 (D4140): the run a kernel-side polled sort (kernels ops/sort/polled.rs `sort_by`) sorts between two checks before its
/// merge passes: `cancelpoll::SORT_RUN`'s value (64 Ki elements, a few ms of compares).
pub const POLL_SORT_RUN: usize = 1 << 16;

/// The most rows a kernel sort orders on its small road (kernels ops/sort/rowsort.rs `small_on`): on the stack, no grant
/// block, no poll, no copy of its rows. Its sorts make under `POLL_BATCH` compares (n log2 n = 2,048), so a polled sort of
/// them never ticks inside the sort either. Measured on c8g (48-byte rows, the allocator's road): key 0's radix 3.3 us
/// against 1.8 us on the small road at 100 rows, 0.60 of it at 128, 0.76 at 256, 0.86 at 512; a compared run 0.87-0.94.
pub const SMALL_SORT_ROWS: usize = 256;
/// The widest run row the small road permutes in place through one row of stack scratch; a wider row takes the copy.
pub const SMALL_SORT_ROW_BYTES: usize = 512;

/// A kernel's check (the lowered back-edge's, `cancel_pending`): the byte is non-zero, then `poll` says whether to stop.
/// # Safety — `ctx` is bound (`cancel` null or a live byte).
#[inline(always)]
pub unsafe fn cancel_pending(ctx: *mut Ctx) -> bool {
    let cancel = unword((*ctx).cancel);
    !cancel.is_null() && (*(cancel as *const AtomicU8)).load(Ordering::Acquire) != 0 && poll(ctx)
}

/// D3406: what a kernel does on a set byte (`set_poll`; unset: stop on the abort bit, as every kernel did before). The
/// server's (gravity_exec glue) runs the owner's CHECK_FOR_INTERRUPTS on its own byte (C's CFI: a busy statement absorbs a
/// ProcSignalBarrier and dumps its memory contexts) and stops only where it raised; on a byte another thread owns (a pool
/// worker reading its leader's) it stops on the abort bit alone.
pub type PollFn = unsafe fn(*mut Ctx) -> bool;

static POLL: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the kernels' poll (once, at server start; a later call replaces it).
pub fn set_poll(f: PollFn) {
    POLL.store(f as usize, Ordering::Release);
}

/// The kernels' poll on a set byte: true = stop (`CANCELLED`; an error the poll raised is the runner's to report).
/// # Safety — `ctx` is bound and its `cancel` a live byte.
#[cold]
#[inline(never)]
pub unsafe fn poll(ctx: *mut Ctx) -> bool {
    let f = POLL.load(Ordering::Acquire);
    if f == 0 {
        return abort_byte_set(unword((*ctx).cancel));
    }
    // SAFETY: only ever 0 or a `PollFn` (`set_poll`).
    let f: PollFn = core::mem::transmute::<usize, PollFn>(f);
    f(ctx)
}

/// `bytes` from the arena window, 8-aligned; null when exhausted (`arena_alloc` grows and retries).
/// # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn arena_bump(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    let n = (bytes + 7) & !7;
    let cur = unword((*ctx).arena_cur);
    if (unword((*ctx).arena_end) as usize).wrapping_sub(cur as usize) < n {
        return core::ptr::null_mut();
    }
    (*ctx).arena_cur = word(cur.add(n));
    cur
}

/// # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn arena_alloc(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    let p = arena_bump(ctx, bytes);
    if !p.is_null() {
        return p;
    }
    // D638: the grow asks for what `arena_bump` takes (the 8-rounded size), so a chunk sized to the request fits it
    if crate::program::arena_grow(ctx, ((bytes + 7) & !7) as u64) <= 0 {
        return core::ptr::null_mut();
    }
    arena_bump(ctx, bytes)
}
