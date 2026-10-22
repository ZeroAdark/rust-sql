use crate::layout::{unword, word, Word};
use crate::breaker::RecOpenFn;
use crate::ctx::{Ctx, Partials};

pub type PushFn = Option<unsafe extern "C" fn(*mut Ctx) -> i64>;
pub type SpillFn = Option<unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64>;
pub type ArenaGrowFn = Option<unsafe extern "C" fn(*mut Ctx, u64) -> i64>;
pub type WriteFn = Option<unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64>;
pub type LockFn = Option<unsafe extern "C" fn(*mut Ctx, u64) -> i64>;
pub type PinFn = Option<unsafe extern "C" fn(*mut Ctx, u64) -> *mut u8>;
/// A program's entry, called from Rust (`Code::call`). It stays "C-unwind" while the host slots are "C" (landing 13):
/// a JIT'd or interpreted program never unwinds (its hosts answer through their pads), but hand kernels are Rust bodies
/// stored here, and the parallel unwind tests (d1702_*) panic in a member on purpose to prove the section's cleanup.
pub type Entry = unsafe extern "C-unwind" fn(*mut Ctx) -> i64;
/// The interpreter's and wasm's run (`Owner::Interp`); "C-unwind" as `Entry` (its body is under the one pad already).
pub type InterpRun = unsafe extern "C-unwind" fn(*const (), *mut Ctx) -> i64;

crate::block! {
    /// The per-Binding block (abi-v2 §2.3): the program-wide call targets and the breaker table
    /// pointer; `env` is the host side's private state behind the calls (a kernel never reads it),
    /// `sink` the concrete sink `push` was resolved for at bind (its private state, likewise opaque).
    ProgramBlock size 96 offsets prog {
        push => PUSH: Word<PushFn>,
        spill => SPILL: Word<SpillFn>,
        arena_grow => ARENA_GROW: Word<ArenaGrowFn>,
        write => WRITE: Word<WriteFn>,
        lock => LOCK: Word<LockFn>,
        partial => PARTIAL: Word<*mut Partials>,
        env => ENV: Word<*mut u8>,
        sink => SINK: Word<*mut u8>,
        /// The record-write host (`breaker::rec_open`, milestone-2.md §2.6 item 11).
        rec_open => REC_OPEN: Word<RecOpenFn>,
        owner => OWNER: Word<*mut u8>,
        pin => PIN: Word<PinFn>,
        instr => INSTR: Word<*mut u64>, // EXPLAIN ANALYZE's counter cells (ENTRY E1), zeroed at bind; null unless instrumented
    }
}

pub const PROGRAM_SIZE: usize = prog::SIZE;

pub const WRITE_INSERT: u32 = 1;
pub const WRITE_UPDATE: u32 = 2;
pub const WRITE_DELETE: u32 = 3;
/// W6-D1112 (V4R-528): a check option's SubPlan read. After `WRITE_SUBPLAN` the IR calls `write` with `WRITE_SUB_ARGS` (the
/// host's parked call: its address, laid out by the `SUB_*` offsets below), evaluates the SubPlan, writes its value and null
/// word there and calls `write` again with the row's own op | `WRITE_RESUME`: the host resumes the parked check.
pub const WRITE_SUB_ARGS: u32 = 5;
pub const WRITE_RESUME: u32 = 1 << 8;
/// The parked call's words: the SubPlan's plan id, its result (value, null), then each argument's (value, null) pair.
pub const SUB_PLAN_ID: i32 = 0;
pub const SUB_RESULT: i32 = 8;
pub const SUB_RESULT_NULL: i32 = 16;
pub const SUB_ARGS: i32 = 24;
/// D655 Row C (V4R-534): after `WRITE_RECHECK`, the newest version the host locked into the write site's cells and position
/// (driver `Writer::epq_row`), for the IR's own EvalPlanQual (lowerv4 write/rowop_epq.rs); `WRITE_DONE` or the host's error.
pub const WRITE_EPQ_ROW: u32 = 6;
/// ON CONFLICT DO UPDATE: the row-level security check over the existing row the conflict locked (WCO_RLS_CONFLICT_CHECK,
/// driver `Writer::conflict_check`), called by the IR's arm after its WHERE passed and before its SET, ExecOnConflictUpdate's
/// order; `WRITE_DONE` or the host's error.
pub const WRITE_CONFLICT_CHECK: u32 = 7;

/// The `spill(ctx, op, arg)` ops (`breakers.md`).
pub const SPILL_SCATTER: u32 = 0;
pub const SPILL_REFILL: u32 = 1;
pub const SPILL_PARTS: u32 = 2;
pub const SPILL_DROP: u32 = 3;
pub const SPILL_TARGET: u32 = 4;
/// The own region's run put (`arg` 0); D1330: a non-zero `arg` is the address of a var sort's big run (`SPILL_MERGE_BIG`'s).
pub const SPILL_RUN_PUT: u32 = 5;
pub const SPILL_RUNS: u32 = 6;
pub const SPILL_RUN_OPEN: u32 = 7;
pub const SPILL_RUN_FILL: u32 = 8;
/// Split the partition a refill left in the target table by the next hash window (its sub-tokens
/// queued as an overflow's would be): the join pairs a build split with the outer's.
pub const SPILL_SPLIT: u32 = 9;
pub const SPILL_RUN_BASE: u32 = 10; // lower.md §15.20: `addr` = where `SPILL_RUN_FILL` lands; 0 = `Partials.own`, the default at every binding
pub const SPILL_RUN_CLOSE: u32 = 11; // drop the run reader `handle` before its end
pub const SPILL_RUN_ADOPT: u32 = 12; // M7 L3b: the runs of stream `arg` (slot + 1) moved into the calling pipeline's own stream
/// NEW-43 (numbered after `SPILL_REGION_COMMIT`): stream `arg`'s put runs and merge passes dropped mid-life (0 the own
/// slot, else `slot + 1`, `STREAM_CONSUMED | k`): an INCSTREAM reader read their merge to its end and its producer streams
/// into the region again (kernels `SortStreamRearm`); `OK`.
pub const SPILL_RUN_DROP: u32 = 22;
/// D254: `arg` = the address of `[need, head, tail]` (`region_grow`); the new base, 0 = a real allocation failure (stashed).
pub const SPILL_REGION_GROW: u32 = 13;
/// D3105 (b), numbered after `SPILL_RUN_DROP`: `SPILL_REGION_GROW` only within the calling owner's limit (its share W, or
/// for the leader the statement's G, a reserved region's commit in place too: the driver's `region_grow_within`): the new
/// base, 1 when the limit refuses it (nothing drawn, no error: the caller puts its run instead; the driver fires
/// merge.inner_merged_run), 0 = a real allocation failure (stashed).
pub const SPILL_REGION_GROW_IN: u32 = 23;
/// JOIN's deferred build error (PG's outer-first hash join): the error a build pipeline raised while its merge
/// deferred it (`Merge::defer_error`), held by the driver on the node's spill table, raised now: the error status with the
/// held error re-stashed for the raise exit (every caller gets its own copy, so any participant's probe can raise it), or
/// 0 when nothing is held (`raise_held`).
pub const SPILL_RAISE_HELD: u32 = 14;
/// V4R-136: the rows of stream `arg`'s runs (0 the own slot, else `slot + 1`), summed over every owner as the runs were put
/// or adopted: a sort emit's exact row count (`sort_total`, D463).
pub const SPILL_RUN_ROWS: u32 = 15;
/// D1324: a stream argument naming the running pipeline's consumed region `k` (`STREAM_CONSUMED | k`) in place of a
/// slot number: the driver resolves it through the pipeline's `consumes[k]` as the program runs it. A slot a reader bakes
/// into its code goes stale when lowerv4 renumbers a deferred pipeline's slot after the code is built (frame.rs
/// `renumber_deferred`); the consumed index never moves, as the `Partials` regions the reader reads follow it.
pub const STREAM_CONSUMED: u64 = 1 << 30;
/// D645: the out block's by-reference region swapped for one of at least `arg` bytes (twice the old at least), drawn from
/// the statement's grant as D641's result spill and held until the statement's end; the rows before were pushed. The new
/// capacity, or 0 with OutOfMemory held by the driver (the IR's HOST raise).
pub const SPILL_OUT_GROW: u32 = 16;
/// D1327: a spilled sort whose runs outnumber its region's fan-in merges them in passes (PG tuplesort mergeruns): a new
/// merged run of pass `(arg >> 32) & 0x3fff_ffff` of stream `arg` (low 32 bits: 0 the own slot, else `slot + 1`,
/// `STREAM_CONSUMED | k`), in that pass's own file (`MERGE_FRESH`: the pass's first run, which opens it and unlinks the
/// file of the pass two before; pass 0's, every merged file of the stream), apart from the stream's put runs (`SPILL_RUNS` / `SPILL_RUN_ROWS` never count it); returns its
/// run token (`MERGED_RUN | pass << 20 | index`, `SPILL_RUN_OPEN`'s).
pub const SPILL_MERGE_BEGIN: u32 = 17;
/// D1327: `SPILL_MERGE_BEGIN`'s flag for a pass's first run.
pub const MERGE_FRESH: u64 = 1 << 62;
/// D1327: `arg` = the address of `[stream, token, ptr, len]`: `len` bytes at `ptr` appended to merged run `token`.
pub const SPILL_MERGE_WRITE: u32 = 18;
/// D1327: a record longer than a run's share of the merge region (PG lets one tuple pass work_mem): `arg` the address of
/// `[merge region, bytes]`, a buffer of `bytes` drawn from the grant (a free one of the owner's reused), held until the
/// statement's end; its address, or 0 with OutOfMemory held. `bytes` 0 frees the merge region's buffers for reuse; D1330:
/// `bytes` u64::MAX looks the key's buffer up (its address, else 0: a var sort's big run in flight, keyed its region + 1).
pub const SPILL_MERGE_BIG: u32 = 19;
/// D2572-c: the calling owner's var run rows laid out as `arg`'s words `[row bytes, record word offset, var slots, each
/// slot's offset]` (a var sort's, before its `SPILL_RUN_PUT`): a compressing spill file compacts the rows by it. Advisory:
/// a host without it answers an error the caller ignores.
pub const SPILL_RUN_SHAPE: u32 = 20;
/// The setop posture (option c'): the calling owner's produced region when it is an address-space reservation past its
/// share (a `Budget::Pool` side's): `arg` 0 asks the reservation's length (0 = none: the region is its share), `arg` > 0
/// commits `arg` more bytes of it in place, drawn as D254's past-share growth (W, then G, then pool bytes past both); the
/// bytes committed, 0 when the reservation has no room or the draw is refused (nothing raised: the fold spills as before).
pub const SPILL_REGION_COMMIT: u32 = 21;
/// D3190: a reader over rows `[lo, hi)` of one put run, its fills that slice's records whole and in order (a co-split
/// merge join's claim, its merge over each outer run's slice): `arg` the address of `[stream, token, lo, hi]` (the stream
/// as `SPILL_RUNS` names it, the token as `SPILL_RUN_OPEN`'s); a handle as `SPILL_RUN_OPEN`'s, its reader at once spent
/// when the slice is empty. The run is opened at its fence at or before `lo` (driver mem/fence.rs): a run put without
/// fences is an error.
pub const SPILL_RUN_SLICE: u32 = 24;
/// D3259: whether a sort's section-end runs of stream `arg` (as `SPILL_RUNS` names it), merged, would pass one region:
/// the merge's answer over every partial before the section end (driver `Merge::seal_over`, filed on the stream by the
/// section's `Open::file_seal_over`), the same for every worker's section end; 1 past it (each worker puts its own run,
/// kernels sort `resident`), 0 under it or with no answer filed (a width-1 arm's one run).
pub const SPILL_SEAL_OVER: u32 = 25;
/// A sort run's region that started small (the driver's `grant::starts_small`: it commits a start inside a reservation of
/// its work_mem) grown IN PLACE as its run fills: `arg` the address of `[need, head]`, the region at least twice its length
/// and `head + need` within the reservation (W, then G, then the pool's room now), the u32 at offset 8 re-armed to the new
/// length, the base unchanged. The new length; 0 for any other region, past the reservation, or when the pool has no room
/// now (nothing drawn, nothing raised: the run is put as before, as PostgreSQL's Sort spills past work_mem).
pub const SPILL_REGION_PLACE: u32 = 26;
/// D3644 (b): a sort's block of `arg` bytes drawn as `SPILL_MERGE_BIG`'s; 1 under mempolicy `work_floor`; 0 refused.
pub const SPILL_WORK_TAKE: u32 = 27;
pub const SPILL_WORK_GIVE: u32 = 28;
/// C10: `SPILL_REGION_GROW` from a spilling breaker's road that names its debt: `arg` = the address of `[need, head, tail,
/// why]` (`region_grow_debt`, `why` a gravity_program `spill::NoSpill` code); answered as `SPILL_REGION_GROW`, the grow
/// past the grant counted as that named debt, not as an unnamed one.
pub const SPILL_REGION_GROW_DEBT: u32 = 29;
/// C10 2b (D3582): the batched slot the running emit consumes first (`breaker::Owned::batched`), one batch of its
/// partitions folded into its one owner region at a time: `arg` the emit's record bytes (low 32 bits) and `OWNED_REWIND`
/// (a fresh gather: the first batch, folded again unless the region holds it untouched), else the next batch; the
/// batch's records gathered into the pointer array behind an emit state at the owner's table space
/// (`breaker::OWNED_EMIT_BYTES`). The state's address, 0 past the last batch, or an error status.
pub const SPILL_OWNED_NEXT: u32 = 30;
/// D4373: the permutation's copy of `arg` bytes within the sort's grant: W, else G, never the grant pool past G and never a
/// file; 1 under mempolicy `work_floor`; 0 when neither had room, nothing held and nothing raised (the rows are permuted in
/// place).
pub const SPILL_WORK_TRY: u32 = 31;
/// D4373: a block the sort cannot do without (its index, cut scratch, relay and put copies): W, else G, else (`WORK_SEAL`
/// in the arg: the pool's room now, then) the statement's mapped temp file (`map_owners`); 0 refused, the refusal held for
/// the emit's raise.
pub const SPILL_WORK_WITHIN: u32 = 32;
/// D4373: `SPILL_WORK_WITHIN`'s arg flag: a block of a run sealed in memory (not being put): past W and G the pool's room
/// now (counted as grown) before the mapped temp file, as PostgreSQL's in-memory sort holds its rows.
pub const WORK_SEAL: u64 = 1 << 62;
/// `SPILL_OWNED_NEXT`'s fresh-gather flag.
pub const OWNED_REWIND: u64 = 1 << 32;
/// LEDGER2 step 3: an operator arena's chunk (gravity_fn `arena::OpArena`): `arg` the address of `[bytes, owner word 0,
/// owner word 1]` (the arena's ledger owner, memledger `Owner::words`); a region of `bytes` drawn as `SPILL_WORK_TAKE`'s
/// (W, then G, then past G a mapped temp file), not zeroed, its charge moved to that owner and held until
/// `SPILL_ARENA_GIVE` or the statement's end. Its address, 0 refused (the refusal held for the raise).
pub const SPILL_ARENA_CHUNK: u32 = 33;
/// LEDGER2 step 3: an `SPILL_ARENA_CHUNK` chunk at `arg` given back (the arena's reset or drop).
pub const SPILL_ARENA_GIVE: u32 = 34;
/// LEDGER2 step 3: an operator arena's chunk asks (gravity_fn `arena`): its first chunk, each next one twice the last up
/// to the second (an ask past it is a chunk of its own size).
pub const ARENA_CHUNK_FIRST: usize = 64 << 10;
pub const ARENA_CHUNK_MAX: usize = 64 << 20;
/// LEDGER2 step 5: the smallest allocator-road buffer a sort kernel call carves from a scratch arena under its slot's owner
/// (`gravity_fn::arena::OpArena::scratch`); a smaller one stays the allocator's `Vec` (a hot small cut begins no owner).
pub const ARENA_SCRATCH_MIN: usize = 1 << 20;
/// D1327: a `SPILL_RUN_OPEN` run token naming a merged run (`SPILL_MERGE_BEGIN`'s token), not a put one.
pub const MERGED_RUN: u32 = 1 << 31;

/// D3190 (merge.cosplit): the co-split's range table (exec ops/sort/cosplit.rs lays it, its proto's handle word holds its
/// address; the merge's IR and kernels sort `sort_cosplit_*` read it): `COSPLIT_HDR_WORDS` header words, then
/// `COSPLIT_PART_WORDS` words a part (`COSPLIT_PARTS` of them, at most `COSPLIT_MAX_PARTS`), then one word per owner
/// (`MAX_OWNERS`: the merge area each owner drew for its claims over a put outer, kernels' cache).
pub const COSPLIT_HDR_WORDS: usize = 4;

/// D3190: the most parts a co-split table holds (its words: `COSPLIT_HDR_WORDS + parts * COSPLIT_PART_WORDS + MAX_OWNERS`).
pub const COSPLIT_MAX_PARTS: usize = 256;

/// D3190: the most merge keys a co-split cuts on (each run row's first `2 * nkey` words, the keys' `[flags, word]` pairs;
/// the proto's argument holds the count).
pub const COSPLIT_MAX_KEYS: usize = 4;

/// D3190: the header word of the cut's form: `COSPLIT_MODE_WHOLE` (one claim, the whole of both runs: the serial form's
/// rows), `COSPLIT_MODE_RESIDENT` (both runs resident, a part's ranges row positions of each), `COSPLIT_MODE_SLICES` (the
/// outer's runs put: a part's outer range is its rows' place in the parts' order, `[the rows before it, and its own)`, its
/// rows the merge of each run's slice, `COSPLIT_SLICES_AT`).
pub const COSPLIT_MODE: usize = 0;
pub const COSPLIT_MODE_WHOLE: u64 = 0;
pub const COSPLIT_MODE_RESIDENT: u64 = 1;
pub const COSPLIT_MODE_SLICES: u64 = 2;

/// D3190: the header words of `COSPLIT_MODE_SLICES`: the outer's put runs, and the address of their slices (`[lo, hi)` row
/// positions of each run for each part, part-major: part p's run r at words `2 * (p * runs + r)`).
pub const COSPLIT_RUNS: usize = 1;
pub const COSPLIT_SLICES_AT: usize = 2;

/// D3190: the header word of the table's parts (the owners' words follow them).
pub const COSPLIT_PARTS: usize = 3;

/// D3190: the words of one part in the co-split's range table, `[outer lo, outer hi, inner lo, inner hi)` row positions of
/// the two sorted runs; a claim `[a, b)` covers parts a to b - 1 (consecutive key ranges). The whole run is
/// `[0, COSPLIT_WHOLE)`.
pub const COSPLIT_PART_WORDS: usize = 4;

/// D3190: a co-split table's end position that reads the run to its end (the reader clamps every position to its rows).
pub const COSPLIT_WHOLE: u64 = i64::MAX as u64;

impl ProgramBlock {
    pub const NONE: ProgramBlock = ProgramBlock { push: word(None), spill: word(None), arena_grow: word(None), write: word(None), lock: word(None), partial: word(core::ptr::null_mut()), env: word(core::ptr::null_mut()), sink: word(core::ptr::null_mut()), rec_open: word(None), owner: word(core::ptr::null_mut()), pin: word(None), instr: word(core::ptr::null_mut()) };
}

/// # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn push(ctx: *mut Ctx) -> i64 {
    (unword((*unword((*ctx).program)).push).unwrap_unchecked())(ctx)
}

/// # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn spill(ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    (unword((*unword((*ctx).program)).spill).unwrap_unchecked())(ctx, op, arg)
}

/// D254 (no runtime "past its share" error): the region the calling owner's pipeline produces (`Partials.own`: the leader's
/// slot at W1, a worker's partial at W >= 2) reopened at max(2 x len, head + tail + need): [0, head) copied to the new base, the
/// old [len - tail, len) to the new [new_len - tail, new_len), the u32 limit at offset 8 re-armed to new_len, Partials.own moved.
/// The new base (64-aligned), or 0 on a real allocation failure (the driver's OutOfMemory stashed for the raise).
/// # Safety — `ctx` is bound, its running pipeline produces a region, head + tail <= its length.
#[inline(never)]
pub unsafe fn region_grow(ctx: *mut Ctx, need: u64, head: u64, tail: u64) -> u64 {
    let args = [need, head, tail];
    spill(ctx, SPILL_REGION_GROW, args.as_ptr() as u64) as u64
}

/// C10: `region_grow` from a spilling breaker's road that still grows past its grant, the debt named by `why` (a
/// gravity_program `spill::NoSpill` code): `SPILL_REGION_GROW_DEBT`.
/// # Safety — as `region_grow`.
#[inline(never)]
pub unsafe fn region_grow_debt(ctx: *mut Ctx, need: u64, head: u64, tail: u64, why: u8) -> u64 {
    let args = [need, head, tail, why as u64];
    spill(ctx, SPILL_REGION_GROW_DEBT, args.as_ptr() as u64) as u64
}

/// C10 2b (D3582): `SPILL_OWNED_NEXT` over the running emit's first consumed slot, its records `rec` bytes: the emit
/// state of the batch now gathered (`rewind`: the first batch), 0 past the last batch (or a program block with no spill
/// host: a kernel test's rig), or an error status (the driver's error stashed).
/// # Safety — `ctx` is bound, its running pipeline consumes a batched slot first.
#[inline(never)]
pub unsafe fn owned_next(ctx: *mut Ctx, rec: u64, rewind: bool) -> i64 {
    match unword((*unword((*ctx).program)).spill) {
        Some(f) => f(ctx, SPILL_OWNED_NEXT, (rec & 0xffff_ffff) | if rewind { OWNED_REWIND } else { 0 }),
        None => 0,
    }
}

/// D3105 (b): `region_grow` within the calling owner's limit (`SPILL_REGION_GROW_IN`): the new base, 1 when the limit
/// refuses it (nothing drawn), 0 on a real allocation failure (its error stashed).
/// # Safety — as `region_grow`.
#[inline(never)]
pub unsafe fn region_grow_in(ctx: *mut Ctx, need: u64, head: u64, tail: u64) -> u64 {
    let args = [need, head, tail];
    spill(ctx, SPILL_REGION_GROW_IN, args.as_ptr() as u64) as u64
}

/// The setop posture (option c', `SPILL_REGION_COMMIT`): `need` 0 = the calling owner's region reservation (0: none), else
/// the bytes of it committed in place (`need`, or 0: no room, refused). Never an error status: a negative answer reads 0,
/// and a program block with no spill host (a kernel test's rig) answers 0: no reservation.
/// # Safety — `ctx` is bound, its running pipeline produces a region.
#[inline(never)]
pub unsafe fn region_commit(ctx: *mut Ctx, need: u64) -> u64 {
    match unword((*unword((*ctx).program)).spill) {
        Some(f) => f(ctx, SPILL_REGION_COMMIT, need).max(0) as u64,
        None => 0,
    }
}

/// D2894: the bit `SPILL_REGION_COMMIT`'s `need` 0 answer carries past the reservation's bytes when the reservation is its
/// breaker's PostgreSQL floor (the driver's `grow::floor_reserve`), not a set operation side's rows bound (option c').
pub const RESERVE_FLOOR: u64 = 1 << 62;

/// The bit `SPILL_REGION_COMMIT`'s `need` 0 answer carries when the region started small: its committed `limit` is a start
/// that grows in place, and its table is laid out at the reservation's bytes (block sizes, chain words), never at the start.
pub const RESERVE_LAYOUT: u64 = 1 << 61;

/// D3809: the bits of `SPILL_REGION_COMMIT`'s `need` 0 answer (32..60) that carry a started-small region's share, in 8-byte
/// units, which its partitions are cut by (kernels `fold_bits`) as a share-committed region's are by its limit: start-small
/// changes what is committed at the open, never the fan-out. The reservation's bytes fit the low 32 bits (a region is at
/// most `u32::MAX`).
pub const RESERVE_SHARE_SHIFT: u32 = 32;

/// D3809: the largest share the answer carries, in 8-byte units (4 GiB less 8): a larger one saturates, which cuts no
/// partition a share-committed region would keep (any room past `T1_MAX_PARTS` 32 KiB blocks, 64 MiB, opens the most).
pub const RESERVE_SHARE_MAX: u64 = (1 << 29) - 1;

/// D3809: `share` placed in a `need` 0 answer's share bits (`RESERVE_SHARE_SHIFT`), saturating at `RESERVE_SHARE_MAX`.
pub const fn reserve_share_word(share: u64) -> u64 {
    let units = share >> 3;
    (if units > RESERVE_SHARE_MAX { RESERVE_SHARE_MAX } else { units }) << RESERVE_SHARE_SHIFT
}

/// A `need` 0 answer read back: the reservation's bytes, whether it is a floor (`RESERVE_FLOOR`), whether the region
/// started small (`RESERVE_LAYOUT`), and the share it is cut by (`RESERVE_SHARE_SHIFT`; 0 when the answer carries none).
pub const fn reserve_answer(a: u64) -> (u64, bool, bool, u64) {
    (a & (u32::MAX as u64), a & RESERVE_FLOOR != 0, a & RESERVE_LAYOUT != 0, ((a >> RESERVE_SHARE_SHIFT) & RESERVE_SHARE_MAX) << 3)
}

/// `SPILL_REGION_PLACE`: the calling owner's started-small region grown in place to hold at least `head + need` bytes; its
/// new length, 0 when refused. A program block with no spill host (a kernel test's rig) or a host without the op (an
/// error status) answers 0.
/// # Safety — `ctx` is bound, its running pipeline produces a region of at least `head` bytes.
#[inline(never)]
pub unsafe fn region_grow_place(ctx: *mut Ctx, need: u64, head: u64) -> u64 {
    let args = [need, head];
    match unword((*unword((*ctx).program)).spill) {
        Some(f) => f(ctx, SPILL_REGION_PLACE, args.as_ptr() as u64).max(0) as u64,
        None => 0,
    }
}

/// # Safety — `ctx` is null or a ctx whose program block, when set, is live. `None`: the allocator's (no spill host, or
/// the floor); null: refused.
#[inline(never)]
pub unsafe fn work_take(ctx: *mut Ctx, bytes: u64) -> Option<*mut u8> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    match f(ctx, SPILL_WORK_TAKE, bytes) {
        1 => None,
        p => Some(p.max(0) as usize as *mut u8),
    }
}

/// D4373: `work_take` on the road within the grant (`SPILL_WORK_TRY`): `None` the allocator's (no spill host, or the
/// floor); null: no room (nothing held, nothing raised); the block is given back with `work_give`.
/// # Safety — as `work_take`.
#[inline(never)]
pub unsafe fn work_try(ctx: *mut Ctx, bytes: u64) -> Option<*mut u8> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    match f(ctx, SPILL_WORK_TRY, bytes) {
        1 => None,
        p => Some(p.max(0) as usize as *mut u8),
    }
}

/// D4373: `work_take` on the road within the grant (`SPILL_WORK_WITHIN`; `seal`: `WORK_SEAL`): `None` the allocator's;
/// null: refused (the refusal held for the emit's raise); the block is given back with `work_give`.
/// # Safety — as `work_take`.
#[inline(never)]
pub unsafe fn work_within(ctx: *mut Ctx, bytes: u64, seal: bool) -> Option<*mut u8> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    match f(ctx, SPILL_WORK_WITHIN, bytes | if seal { WORK_SEAL } else { 0 }) {
        1 => None,
        p => Some(p.max(0) as usize as *mut u8),
    }
}

/// # Safety — `p` is a live `work_take` block of `ctx`'s owner, read by nothing after.
#[inline(never)]
pub unsafe fn work_give(ctx: *mut Ctx, p: *mut u8) {
    if let Some(f) = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill)) {
        f(ctx, SPILL_WORK_GIVE, p as usize as u64);
    }
}

/// LEDGER2 step 3 (`SPILL_ARENA_CHUNK`): `bytes` for an operator arena whose ledger owner is `owner` (memledger
/// `Owner::words`). `None`: no spill host (a kernel test's rig: the arena's own allocator road); null: refused.
/// # Safety — `ctx` is null or a ctx whose program block, when set, is live.
#[inline(never)]
pub unsafe fn arena_chunk(ctx: *mut Ctx, bytes: u64, owner: [u64; 2]) -> Option<*mut u8> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    let args = [bytes, owner[0], owner[1]];
    Some(f(ctx, SPILL_ARENA_CHUNK, args.as_ptr() as u64).max(0) as usize as *mut u8)
}

/// LEDGER2 step 5 (`SPILL_ARENA_CHUNK` asking 0 bytes): the ledger owner words (memledger `Owner::words`) of the breaker
/// slot the calling program block's pipeline produces into, which a kernel call's scratch arena begins under; CONVERT
/// wave 2 (R3): the statement's (or the portal's) owner for a pipeline that produces no slot; `None` with no spill host or
/// neither owner.
/// # Safety — as `arena_chunk`.
#[inline(never)]
pub unsafe fn arena_slot_owner(ctx: *mut Ctx) -> Option<[u64; 2]> {
    arena_slot_owner_kind(ctx).map(|(w, _)| w)
}

/// CONVERT wave 2: `arena_slot_owner` and the reply's word 0, the memledger `Kind` (as `u8`) the owner's bytes count
/// under: the produced slot's breaker kind, `Kind::Region` for the statement's (or the portal's) owner.
/// # Safety — as `arena_chunk`.
#[inline(never)]
pub unsafe fn arena_slot_owner_kind(ctx: *mut Ctx) -> Option<([u64; 2], u64)> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    let mut args = [0u64; 3];
    (f(ctx, SPILL_ARENA_CHUNK, args.as_mut_ptr() as u64) == 1).then(|| ([args[1], args[2]], args[0]))
}

/// CONVERT wave 2 (wave2-base commit 7): the `SPILL_ARENA_CHUNK` 0-byte ask's word 1 (the owner words' first, unread
/// when asking 0 bytes) that asks for the pipeline's OWN slot's owner (`arena_own_owner_kind`); 0 asks `arena_slot_owner`'s.
pub const ARENA_ASK_OWN: u64 = 1;

/// CONVERT wave 2 (wave2-base commit 7): the ledger owner words and kind (the reply's word 0, memledger `Kind` as `u8`) of
/// the breaker slot the calling program block's pipeline OWNS: the slot it produces into, else the first slot it consumes
/// (an emit's: the breaker it reads). That is the owner the pipeline's grant-road blocks (`work_take`) are charged to, so a
/// kernel's allocator-road scratch beside them (a sort's or an emit's `Work`) charges the same one. `None` with no spill
/// host, for a pipeline that neither produces nor consumes a slot, or when that slot has no owner; never the statement's.
/// # Safety — as `arena_chunk`.
#[inline(never)]
pub unsafe fn arena_own_owner_kind(ctx: *mut Ctx) -> Option<([u64; 2], u64)> {
    let f = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill))?;
    let mut args = [0u64, ARENA_ASK_OWN, 0];
    (f(ctx, SPILL_ARENA_CHUNK, args.as_mut_ptr() as u64) == 1).then(|| ([args[1], args[2]], args[0]))
}

/// LEDGER2 step 3 (`SPILL_ARENA_GIVE`): an `arena_chunk` chunk back to the host.
/// # Safety — `p` is a live `arena_chunk` chunk of `ctx`'s owner, read by nothing after.
#[inline(never)]
pub unsafe fn arena_give(ctx: *mut Ctx, p: *mut u8) {
    if let Some(f) = ctx.as_ref().and_then(|c| unword(c.program).as_ref()).and_then(|b| unword(b.spill)) {
        f(ctx, SPILL_ARENA_GIVE, p as usize as u64);
    }
}

/// The calling owner's region reservation (`region_commit`'s `need` 0 answer): its bytes (0: none) and whether it is its
/// breaker's PostgreSQL floor (D2894: its pool commits in place before a compact, kernels `folded_in_memory`).
/// # Safety — as `region_commit`.
pub unsafe fn region_reserve(ctx: *mut Ctx) -> (u64, bool) {
    let (bytes, floor, _, _) = region_reserve_laid(ctx);
    (bytes, floor)
}

/// `region_reserve`, whether the table is laid out at the reservation (`RESERVE_LAYOUT`: the region started small), and
/// the share a started-small region's partitions are cut by (D3809; 0: none carried).
/// # Safety — as `region_commit`.
pub unsafe fn region_reserve_laid(ctx: *mut Ctx) -> (u64, bool, bool, u64) {
    reserve_answer(region_commit(ctx, 0))
}

/// D645: a by-reference output row wider than the out block's whole region: the region regrown (`SPILL_OUT_GROW`) to hold
/// `bytes`; its new capacity, or 0 with the driver's OutOfMemory held for the raise.
/// # Safety — `ctx` is bound, its out block is empty.
#[inline(never)]
pub unsafe extern "C" fn out_grow(ctx: *mut Ctx, bytes: u64) -> i64 {
    spill(ctx, SPILL_OUT_GROW, bytes)
}

/// The held build error raised (`SPILL_RAISE_HELD`): the error status for the IR's raise exit, or 0 when nothing is held.
/// # Safety — `ctx` is bound.
#[inline(never)]
pub unsafe fn raise_held(ctx: *mut Ctx) -> i64 {
    spill(ctx, SPILL_RAISE_HELD, 0)
}

/// A statement-lifetime region of `bytes` (zeroed; the driver's pinned pool chunk), null when the pool is capped or the binding has no pin. # Safety — `ctx` is bound.
#[inline(never)]
pub unsafe fn pin(ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    match unword((*unword((*ctx).program)).pin) {
        Some(f) => f(ctx, bytes),
        None => core::ptr::null_mut(),
    }
}

/// `pin`'s statement bit: the chunk stays pinned until the statement's end, never filed as a rerun set's pin (D1339 row
/// C: a keyed emit's kept array behind a frame word no re-open resets, its owner a member of a set whose next run would
/// unpin the set's pins: the driver's `Node::file_pins` / `rerun_keep_begin`).
pub const PIN_STATEMENT: u64 = 1 << 63;

/// `pin` with `PIN_STATEMENT`. # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn pin_statement(ctx: *mut Ctx, bytes: u64) -> *mut u8 {
    pin(ctx, bytes | PIN_STATEMENT)
}

/// # Safety — `ctx` is bound.
#[inline(never)]
pub unsafe fn arena_grow(ctx: *mut Ctx, bytes: u64) -> i64 {
    (unword((*unword((*ctx).program)).arena_grow).unwrap_unchecked())(ctx, bytes)
}

/// # Safety — `ctx` is bound; `site` names the write site.
#[inline(always)]
pub unsafe fn write(ctx: *mut Ctx, op: u32, site: u64) -> i64 {
    (unword((*unword((*ctx).program)).write).unwrap_unchecked())(ctx, op, site)
}

/// # Safety — `ctx` is bound; `site` names the site whose `pos` is locked.
#[inline(always)]
pub unsafe fn lock(ctx: *mut Ctx, site: u64) -> i64 {
    (unword((*unword((*ctx).program)).lock).unwrap_unchecked())(ctx, site)
}

/// # Safety — `ctx` is bound.
#[inline(always)]
pub unsafe fn partial(ctx: *mut Ctx) -> *mut u8 {
    unword((*unword((*unword((*ctx).program)).partial)).own)
}

/// # Safety — `ctx` is bound, `k < Pipeline.consumes.len()` (`k < 2` reads the inline pair; past it, the driver's `more` table).
#[inline(always)]
pub unsafe fn consumed(ctx: *mut Ctx, k: usize) -> *mut u8 {
    let p = &*unword((*unword((*ctx).program)).partial);
    if k < 2 { unword(p.consumed[k]) } else { unword(*unword(p.more).add(k)) }
}

#[cfg(test)]
mod reserve_answer_tests {
    use super::*;

    /// D3809: a started-small region's share rides the `need` 0 answer beside the reservation and its flags, saturating
    /// past 4 GiB (a W0 share of an 8 GiB grant reads 4 GiB, which opens the same partitions).
    #[test]
    fn a_laid_answer_carries_its_share() {
        let reserve = (u32::MAX as u64) & !0xffff;
        for share in [13_500_416u64, 64 << 20, 4 << 30] {
            let a = reserve | RESERVE_FLOOR | RESERVE_LAYOUT | reserve_share_word(share);
            let want = share.min(RESERVE_SHARE_MAX << 3) & !7;
            assert_eq!(reserve_answer(a), (reserve, true, true, want), "share {share}");
        }
        assert_eq!(reserve_answer(reserve_share_word(6 << 30)).3, RESERVE_SHARE_MAX << 3, "saturates");
        assert_eq!(reserve_answer(reserve_share_word(1 << 40)).3, RESERVE_SHARE_MAX << 3, "saturates");
        // an answer without the share (a set operation side's reservation, an older driver) reads 0
        assert_eq!(reserve_answer(1 << 20 | RESERVE_LAYOUT), (1 << 20, false, true, 0));
        assert_eq!(reserve_answer(0), (0, false, false, 0));
    }
}
