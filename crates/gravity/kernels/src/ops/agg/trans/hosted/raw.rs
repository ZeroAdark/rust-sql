//! hosted.rs's unsafe half: the arena head's function table, the seven host entries the lowered IR calls (each a
//! `contract::host` function; all but `m6_agg_final_direct` under the one landing pad, `#[guard(ERROR)]`: a panic is
//! parked for the program's caller and the entry answers `ERROR`), the keyed owner fold's combine and the handle check.
use gravity_abi::breaker::Rec;
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_fn::PgError;

use super::{handle_slot, stale, AGG_MAGIC, AGG_STATE_BYTES, STATE_AGGNO, STATE_HOME, STATE_INIT};

/// The arena's head (hosts ops/agg/trans/hosted.rs `AggArena`, `#[repr(C)]` with the head first): reached through its
/// functions, this crate cannot name the hosts crate.
#[repr(C)]
pub struct AggHead {
    /// `AGG_MAGIC` while the arena lives: a stale or foreign handle is an error, never a wild call.
    pub magic: u64,
    /// One transition of aggregate `aggno` over the state at `state` in worker `ordinal`'s slot: `args[i]` / `nulls[i]`
    /// (1 = NULL) its inputs.
    pub trans: unsafe fn(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8, args: *const u64, nulls: *const u8) -> Result<(), ()>,
    /// The final value of aggregate `aggno` at `out[0..3]`: the datum, 1 when NULL, and the bytes of a by-reference image
    /// (0 for a by-value one); the image lives until the slot's next final call.
    pub final_: unsafe fn(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8, out: *mut u64) -> Result<(), ()>,
    /// D1371: host-sorted aggregate `aggno`'s sort and transitions over its group's run at `state`, ahead of the group's
    /// finals (PG's finalize_aggregates runs every sorted transition before any final); the state is left folded, so the
    /// final reads it without folding again. Nothing for an aggregate that keeps no run.
    pub fold: unsafe fn(h: *mut AggHead, ordinal: u16, aggno: u32, state: *mut u8) -> Result<(), ()>,
    /// Aggregate `aggno`'s state at `from` combined into the one at `into` by its combinefn in worker `ordinal`'s slot
    /// (PG's `advance_combine_function`, the result in that slot's aggcontext); `from` is left zeroed (holds no state).
    /// Two partials of one group (a keyed record fold, D798).
    pub combine: unsafe fn(h: *mut AggHead, ordinal: u16, aggno: u32, into: *mut u8, from: *mut u8) -> Result<(), ()>,
    /// `combine` leaving `from` as it was (a keyed owner fold's source partial is read-only: a re-read folds it again);
    /// an empty `into` takes PG's copy of `from`'s state.
    pub combine_keep: unsafe fn(h: *mut AggHead, ordinal: u16, aggno: u32, into: *mut u8, from: *const u8) -> Result<(), ()>,
    /// The one home of the keyed group whose record identity words are `keys[..nkeys]`: `n * wide` zeroed state slots
    /// made at its first record (by any worker), the same address at every later one; null with the error stashed.
    /// `ctx`: the folding worker's (its pipeline's breaker slot owner holds the homes' bytes).
    pub home: unsafe fn(h: *mut AggHead, ctx: *mut Ctx, keys: *const u64, nkeys: usize, n: usize) -> *mut u8,
    /// Every home forgotten (a table laid out afresh: a new run of the keyed table); their states stay allocated to the
    /// arena's end, so an older record's tag never dangles.
    pub homes_clear: unsafe fn(h: *mut AggHead),
    /// D1520: the addresses of the direct arguments of the final `m6_agg_final_direct` is making (0 outside one): per
    /// direct argument its datum word (`*const u64`) and how it is given (`*const u8`: `DIRECT_*`); `final_` reads them.
    pub direct: u64,
    pub given: u64,
    /// D2332: nonzero when `final_`'s by-reference image lives until the same aggregate's next final or the arena's end
    /// (a plain Agg's arena: one group per open; D3578 a streamed group's: one group at a time; the image in a buffer of
    /// the aggregate's own); `m6_agg_final` then passes it as is, without its copy into the Call's scratch (which a rerun
    /// set's re-opens never rewind, and a long call keeps to its end).
    pub stable: u64,
    /// D3062-b: the states one home holds per aggregate: 1 (the group's one state), or a wide keyed arena's `width + 1`
    /// (one per worker slot, each advanced only by its worker, and last the gathered one the finals read).
    pub wide: u64,
    /// D3062-b: a wide home's gathered state of one aggregate (`subs`: its `wide` states): every worker's combined once
    /// into the last, in the merge slot; null with PG's error stashed. Only the emit calls it (its readers run width 1).
    pub gather: unsafe fn(h: *mut AggHead, subs: *mut u8) -> *mut u8,
}

/// The state a slot stands for: its group's home when tagged `STATE_HOME`, else the slot itself.
/// # Safety — `s` is an `AGG_STATE_BYTES` state slot.
pub unsafe fn at_home(s: *mut u8) -> *mut u8 {
    if s.add(STATE_HOME).read() == 1 { (s as *const u64).read_unaligned() as *mut u8 } else { s }
}

/// `at_home` for worker `ordinal`'s transition: a wide home's own state of that worker (a nested lane's ordinal, past
/// the workers, takes the first, as the host's `slot` maps it to the leader's).
/// # Safety — as `at_home`; `head` is live.
unsafe fn at_home_of(head: *const AggHead, s: *mut u8, ordinal: u16) -> *mut u8 {
    let home = at_home(s);
    let w = (*head).wide;
    if home == s || w <= 1 || ordinal as u64 + 1 >= w {
        return home;
    }
    home.add(ordinal as usize * AGG_STATE_BYTES as usize)
}

crate::contract::host! {
    /// `m6_agg_trans`: `state = transfn(state, args…)` for aggregate `aggno` of arena `h` (strictness, the first input's
    /// copy and the by-reference copy into the aggregate context as PG does), 0; `ERROR` with PG's error stashed.
    /// `ctx` is the calling worker's: its ordinal picks the arena's slot.
    /// # Safety — `h` is the execution's arena handle, `state` a slot of `AGG_STATE_BYTES`, `args` / `nulls` the
    /// aggregate's transition inputs; `ctx` is bound.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_trans(ctx: u64, h: u64, aggno: u64, state: u64, args: u64, nulls: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let state = state as *mut u8;
        let args = args as *const u64;
        let nulls = nulls as *const u8;
        let Some(head) = live(h) else { return stale() };
        let o = (*ctx).worker_ordinal;
        ((*head).trans)(head, o, aggno as u32, at_home_of(head, state, o), args, nulls).map_or(ERROR, |()| 0)
    }

    /// `m6_agg_final`: aggregate `aggno`'s final value (its finalfn, or the transition value) at `out[0..2]` (datum, 1 when
    /// NULL), 0; a by-reference result is copied into the ctx's scratch (the Call's window, as `m6_detoast`'s payload),
    /// unless the arena keeps it stable (`AggHead.stable`, D2332). `ERROR` with PG's error stashed; a refused scratch is
    /// PG's "out of memory".
    /// # Safety — as `m6_agg_trans`; `ctx` is bound, `out` is writable for two words.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_final(ctx: u64, h: u64, aggno: u64, state: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let state = state as *mut u8;
        let out = out as *mut u64;
        let Some(head) = live(h) else { return stale() };
        let mut r = [0u64; 3];
        let home = at_home(state);
        let state = if home != state && (*head).wide > 1 { ((*head).gather)(head, home) } else { home };
        if state.is_null() {
            return ERROR;
        }
        if ((*head).final_)(head, (*ctx).worker_ordinal, aggno as u32, state, r.as_mut_ptr()).is_err() {
            return ERROR;
        }
        if r[1] == 0 && r[2] != 0 && (*head).stable == 0 {
            let p = crate::util::scratch(ctx, r[2] as usize);
            if p.is_null() {
                return crate::ops::agg::trans::entries::failed(crate::ops::agg::trans::entries::out_of_memory(r[2] as usize));
            }
            core::ptr::copy_nonoverlapping(r[0] as *const u8, p, r[2] as usize);
            r[0] = p as u64;
        }
        out.write_unaligned(r[0]);
        out.add(1).write_unaligned(r[1]);
        0
    }

    /// `m6_agg_fold` (D1371): host-sorted aggregate `aggno`'s sort and transitions over the group's run at `state` (the
    /// arena's `fold`), run for every such aggregate of a group before its first final, as PG's finalize_aggregates runs
    /// process_ordered_aggregate_* for each sorted transition state before any finalfn: a final that raises or acts then
    /// meets the sorted transitions' NOTICEs, writes and errors in PG's order. 0; `ERROR` with PG's error stashed.
    /// # Safety — as `m6_agg_final`.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_fold(ctx: u64, h: u64, aggno: u64, state: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let state = state as *mut u8;
        let Some(head) = live(h) else { return stale() };
        let home = at_home(state);
        let state = if home != state && (*head).wide > 1 { ((*head).gather)(head, home) } else { home };
        if state.is_null() {
            return ERROR;
        }
        ((*head).fold)(head, (*ctx).worker_ordinal, aggno as u32, state).map_or(ERROR, |()| 0)
    }

    /// `m6_agg_final_direct` (D1520): `m6_agg_final` with an ordered-set aggregate's direct arguments given per group, as
    /// finalize_aggregate evaluates them: `direct[i]` / `given[i]` (`DIRECT_*`) for each of the Aggref's direct
    /// arguments. The head holds them for the final's one guarded call (no landing pad of its own: nothing here panics).
    /// # Safety — as `m6_agg_final`; `direct` / `given` hold one entry per direct argument.
    pub unsafe fn m6_agg_final_direct(ctx: u64, h: u64, aggno: u64, state: u64, out: u64, direct: u64, given: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let state = state as *mut u8;
        let out = out as *mut u64;
        let direct = direct as *const u64;
        let given = given as *const u8;
        let Some(head) = live(h) else { return m6_agg_final(ctx as u64, h, aggno, state as u64, out as u64) };
        ((*head).direct, (*head).given) = (direct as u64, given as u64);
        let r = m6_agg_final(ctx as u64, h, aggno, state as u64, out as u64);
        ((*head).direct, (*head).given) = (0, 0);
        r
    }

    /// `m6_agg_combine`: aggregate `aggno`'s state at `from` combined into the one at `into` (its combinefn, PG's
    /// `advance_combine_function`), `from` zeroed, 0; `ERROR` with PG's error stashed (an aggregate without a combinefn
    /// is one: lowering serves two partial states only with it). `ctx` is the calling worker's: its ordinal picks the
    /// slot.
    /// # Safety — as `m6_agg_trans`; `into` / `from` are two `AGG_STATE_BYTES` slots of aggregate `aggno` in arena `h`.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_combine(ctx: *mut Ctx, h: u64, aggno: u64, into: *mut u8, from: *mut u8) -> i64 {
        let Some(head) = live(h) else { return stale() };
        ((*head).combine)(head, (*ctx).worker_ordinal, aggno as u32, into, from).map_or(ERROR, |()| 0)
    }

    /// `m6_agg_home` (D798's bound): a keyed record `rec` the table just opened (a new group, or a group's record again
    /// after T1 evicted or scattered its last one) points each of its `n` `AggState` slots from byte `first` on at its
    /// group's one home state (the arena's `home` over the record's `nkeys` identity words, T1's own), tagged
    /// `STATE_HOME` and held: every record a group opens advances the same states, in input order, so a group holds one
    /// state, never one per partial (PG's HashAgg's one pergroup state). D3062-b: a wide arena's home is still the
    /// group's one, shared by every worker's records, and each worker advances its own state in it (`at_home_of`), so
    /// the owner fold still meets one home per group. 0; `ERROR` with the error stashed.
    /// # Safety — as `m6_agg_trans`; `rec` is a record of `nkeys` key words whose `n` state slots start at byte `first`.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_home(ctx: u64, h: u64, rec: u64, nkeys: u64, first: u64, n: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let rec = rec as *mut u8;
        let Some(head) = live(h) else { return stale() };
        let home = ((*head).home)(head, ctx as *mut Ctx, rec as *const u64, nkeys as usize, n as usize);
        if home.is_null() {
            return ERROR;
        }
        let per = AGG_STATE_BYTES as usize * (*head).wide.max(1) as usize; // D3062-b: a wide home's states per aggregate
        for j in 0..n as usize {
            let s = rec.add(first as usize + j * AGG_STATE_BYTES as usize);
            core::ptr::write_bytes(s, 0, AGG_STATE_BYTES as usize);
            (s as *mut u64).write_unaligned(home.add(j * per) as u64);
            s.add(STATE_INIT).write(1); // held (abi AGGSTATE_HELD): every fold of the record takes the tag
            s.add(STATE_HOME).write(1);
        }
        0
    }

    /// `m6_agg_home_open`: right before the keyed table's open (`agg_open`), which lays T1 out afresh in the call's
    /// partial region when its header is zeroed (a new run: the first claim, a rescan) and resumes it otherwise: a fresh
    /// layout forgets the arena's homes, so no run's records find another run's states (the forgotten homes live on to
    /// the arena's end: a tag never dangles). 0.
    /// # Safety — as `m6_agg_trans`; `ctx` is bound, its partial region the table's.
    #[guard(ERROR)]
    pub unsafe fn m6_agg_home_open(ctx: u64, h: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let Some(head) = live(h) else { return stale() };
        if (*(gravity_abi::program::partial(ctx) as *const gravity_abi::T1)).cap == 0 {
            ((*head).homes_clear)(head);
        }
        0
    }
}

/// The keyed owner fold's hosted combine (`HashAgg::fold_run_with`'s `StateCombine`): the arena whose handle `src`
/// carries (`handle_slot`), aggregate `from`'s number, the arena's slot 0 (width 1), `from` never written; false with
/// PG's error stashed. A `STATE_HOME` source (every record of a keyed hosted table's group points at its one home) is
/// taken, not combined: an empty slot takes the tag, one on the same home keeps it. No other source reaches it (debug
/// asserted; release keeps the combinefn call, which raises by name without one), so a hashed group needs no combinefn.
pub fn owner_combine(src: &Rec<'_>, _slot: usize, into: *mut u8, from: *const u8) -> bool {
    let Some(head) = handle_slot(src.layout).and_then(|h| unsafe { live(src.i64(h) as u64) }) else {
        stale();
        return false;
    };
    // SAFETY: `into` / `from` are two `AGG_STATE_BYTES` slots of one aggregate (`StateCombine`'s contract), the head live.
    unsafe {
        if from.add(STATE_HOME).read() == 1 {
            let (to, at) = ((into as *const u64).read_unaligned(), (from as *const u64).read_unaligned());
            let empty = to == 0 && (into.add(8) as *const u64).read_unaligned() == 0;
            if empty {
                core::ptr::copy_nonoverlapping(from, into, AGG_STATE_BYTES as usize);
                return true;
            }
            if into.add(STATE_HOME).read() == 1 && to == at {
                return true;
            }
            crate::ops::agg::trans::entries::failed(Box::new(PgError::error("gravity: a keyed group's partial on a home beside another state"))); // alloc: Cold
            return false;
        }
        debug_assert!(false, "V4R-427: a keyed hosted record's source partial off its home (hashagg open_rec homes every record)");
        ((*head).combine_keep)(head, 0, (from.add(STATE_AGGNO) as *const u32).read_unaligned(), into, from).is_ok()
    }
}

unsafe fn live(h: u64) -> Option<*mut AggHead> {
    let head = h as *mut AggHead;
    (!head.is_null() && (*head).magic == AGG_MAGIC).then_some(head)
}
