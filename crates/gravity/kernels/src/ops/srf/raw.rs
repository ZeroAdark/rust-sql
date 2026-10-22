//! `ops::srf`'s raw half in kernels (PLAN-v5 §3.2 R6 / R8): M5's host entries (srf/entries.rs's H-a library face, H-b
//! fmgr face and the store reader's head and entries), moved unchanged from srf/entries.rs (construct bundle srf-04) and
//! written through the contract's `host!` under `#[guard]` (srf-05: the landing pad `guard!` gave them, now part of the
//! signature). entries.rs keeps the cell and window layouts and re-exports these under its paths.
use gravity_abi::Ctx;
use gravity_fn::ErrSlot;

use super::entries::{bug, cell_nulls, done, entry_at, no_entry, settle, stale, CELL_ARGS, CELL_OUT, CELL_STATE, FACT_ALIGN, FACT_BYVAL, SRF_FAILED, STORE_FAILED, STORE_MAGIC};

crate::contract::host! {
    /// `open` the library entry `entry` (its index in `gravity_fn::srf::ENTRIES`, the face's `Form::Library`) over the cell's
    /// `nargs` arguments; `coll` is the call's input collation, the entry's `aux` when it takes one (`Entry::coll`).
    /// 1 = opened (step it), 0 = a strict entry with a NULL argument (no rows, PG's strict SRF; nothing opened), or
    /// `SRF_FAILED`.
    #[guard(SRF_FAILED)]
    pub unsafe fn srf_open(ctx: u64, cell: u64, entry: u64, nargs: u64, coll: u64) -> u64 {
        open(ctx as *mut Ctx, cell as *mut u8, entry, nargs as usize, coll)
    }
}

unsafe fn open(ctx: *mut Ctx, cell: *mut u8, entry: u64, nargs: usize, coll: u64) -> u64 {
    let Some(en) = entry_at(entry) else { return no_entry(entry) };
    let nulls = cell.add(cell_nulls(nargs));
    if en.strict && (0..nargs).any(|i| nulls.add(i).read() != 0) {
        return 0;
    }
    let aux = if en.coll { coll as i64 } else { en.aux };
    let mut e = ErrSlot::NONE;
    done((en.open)(&mut e, ctx, cell.add(CELL_STATE), cell.add(CELL_ARGS) as *const i64, nulls, nargs as i64, aux), &mut e)
}

crate::contract::host! {
    /// The opened entry's next row into `CELL_OUT`: 1 = a row, 0 = the set's end, or `SRF_FAILED`.
    #[guard(SRF_FAILED)]
    pub unsafe fn srf_step(ctx: u64, cell: u64, entry: u64) -> u64 {
        step(ctx as *mut Ctx, cell as *mut u8, entry)
    }
}

unsafe fn step(ctx: *mut Ctx, cell: *mut u8, entry: u64) -> u64 {
    let Some(en) = entry_at(entry) else { return no_entry(entry) };
    let mut e = ErrSlot::NONE;
    match (en.step)(&mut e, ctx, cell.add(CELL_STATE), cell.add(CELL_OUT) as *mut i64) {
        r @ (0 | 1) => r as u64,
        _ => settle(&mut e),
    }
}

crate::contract::host! {
    /// The array face's open (`unnest`, `generate_subscripts`): `datum`'s plain image (detoasted and normalised once, in the
    /// slot's context) and its element facts into the cell's state (`gravity_fn::srf::ar`), which the IR walks for by-value
    /// elements. The functions are strict: the IR tests the NULL array first. 1, or `SRF_FAILED`.
    #[guard(SRF_FAILED)]
    pub unsafe fn srf_array_open(ctx: u64, cell: u64, datum: u64, facts: u64) -> u64 {
        let (elmlen, byval, align) = (facts as u16 as i16 as i64, (facts >> FACT_BYVAL) & 1, (facts >> FACT_ALIGN) & 0xff);
        let mut e = ErrSlot::NONE;
        done(gravity_fn::srf::array_open(&mut e, ctx as *mut Ctx, (cell as *mut u8).add(CELL_STATE), datum as i64, elmlen, byval as i64, align as i64), &mut e)
    }

    /// The opened array's next element into `CELL_OUT`'s first column (a by-reference element: a pointer into the image,
    /// valid until the slot's next open or the statement's end): 1 = an element, 0 = the end.
    #[guard(SRF_FAILED)]
    pub unsafe fn srf_unnest_step(ctx: u64, cell: u64) -> u64 {
        let _ = ctx;
        gravity_fn::srf::unnest_step((cell as *mut u8).add(CELL_STATE), (cell as *mut u8).add(CELL_OUT) as *mut i64) as u64
    }

    /// The fmgr set face in the target list (H-b; any builtin / C SRF without a library body, a composite result as one
    /// record datum): `h` is this execution's `gravity_fn::srf_fmgr::Sets` (`SETFN_W_NODE` of a ProjectSet step), `resno`
    /// the target-list entry whose set opens over the cell's arguments. 1 = opened (a strict NULL call opens an empty set),
    /// or `SRF_FAILED`.
    #[guard(SRF_FAILED)]
    pub unsafe fn fmgr_open(ctx: u64, cell: u64, h: u64, resno: u64, nargs: u64) -> u64 {
        let (c, n) = (cell as *mut u8, nargs as usize);
        let Some(sets) = (h as *mut gravity_fn::srf_fmgr::Sets).as_mut() else { return bug("gravity: an fmgr set with no handle") };
        // its row is one column (a scalar, or a composite as one datum): `CELL_OUT`'s first
        let set = match sets.get(resno as usize) {
            Ok(set) => set as *mut gravity_fn::srf_fmgr::FmgrSet,
            Err(err) => {
                gravity_fn::stash_error(err);
                return SRF_FAILED;
            }
        };
        let mut e = ErrSlot::NONE;
        done(gravity_fn::srf_fmgr::open(&mut e, ctx as *mut Ctx, c.add(CELL_STATE), c.add(CELL_ARGS) as *const i64, c.add(cell_nulls(n)), n as i64, set as i64), &mut e)
    }

    /// The fmgr set's next row into `CELL_OUT`: 1, 0 = the end, or `SRF_FAILED`.
    #[guard(SRF_FAILED)]
    pub unsafe fn fmgr_step(ctx: u64, cell: u64, h: u64, resno: u64) -> u64 {
        let _ = (h, resno);
        let c = cell as *mut u8;
        let mut e = ErrSlot::NONE;
        match gravity_fn::srf_fmgr::step(&mut e, ctx as *mut Ctx, c.add(CELL_STATE), c.add(CELL_OUT) as *mut i64) {
            r @ (0 | 1) => r as u64,
            _ => settle(&mut e),
        }
    }
}

/// A store reader's head (H-c's incumbent FunctionScan; H-d's table function and H-e's ENR take the same face): the
/// host object behind the handle in `SETFN_W_NODE`, reached through its functions (this crate cannot name the hosts
/// crate). Leader-only: the store and the node are the leader's (plan §6).
#[repr(C)]
pub struct StoreHead {
    /// `STORE_MAGIC` while the object lives: a stale or foreign handle is an error, never a wild call.
    pub magic: u64,
    /// One window of reader `ptr`: its address, `STORE_END`, or `Err` with PG's error stashed.
    pub batch: unsafe fn(h: *mut StoreHead, ctx: *mut Ctx, ptr: u32, max: u32, natts: u32, need: u64) -> Result<u64, ()>,
    /// Reader `ptr` back to the first row (`tuplestore_rescan`), its window and copies released.
    pub rewind: unsafe fn(h: *mut StoreHead, ctx: *mut Ctx, ptr: u32) -> Result<(), ()>,
    /// M8 store_reopen (V4R-439): exec param `id`'s value and frame flags (lowerv4 params.rs `SET` / `NULL`) into the
    /// store's own executor state, `changed` when its owner wrote it since this store's last open (PG's chgParam).
    pub param: unsafe fn(h: *mut StoreHead, ctx: *mut Ctx, id: u32, value: u64, flags: u64, changed: bool) -> Result<(), ()>,
}


crate::contract::host! {
    /// Up to `max` rows of reader `ptr` of store `h`, the first `natts` columns deformed and those in `need` written (bit c
    /// = column c; every column past 63 is written): the window's address, `STORE_END`, or `STORE_FAILED`.
    #[guard(STORE_FAILED)]
    pub unsafe fn store_batch(ctx: u64, h: u64, ptr: u64, max: u64, natts: u64, need: u64) -> u64 {
        let Some(head) = live(h) else { return stale() };
        ((*head).batch)(head, ctx as *mut Ctx, ptr as u32, max as u32, natts as u32, need).unwrap_or(STORE_FAILED)
    }

    /// Reader `ptr` of store `h` back to its first row: 0, or `STORE_FAILED`.
    #[guard(STORE_FAILED)]
    pub unsafe fn store_rewind(ctx: u64, h: u64, ptr: u64) -> u64 {
        let Some(head) = live(h) else { return stale() };
        ((*head).rewind)(head, ctx as *mut Ctx, ptr as u32).map_or(STORE_FAILED, |()| 0)
    }

    /// M8 store_reopen (V4R-439): before a re-opened store's rewind, each exec param its calls read: `flags` the param's
    /// frame flags word, `changed` 1 when its owner wrote it since this store's last open. 0, or `STORE_FAILED`.
    #[guard(STORE_FAILED)]
    pub unsafe fn store_param(ctx: u64, h: u64, id: u64, value: u64, flags: u64, changed: u64) -> u64 {
        let Some(head) = live(h) else { return stale() };
        ((*head).param)(head, ctx as *mut Ctx, id as u32, value, flags, changed != 0).map_or(STORE_FAILED, |()| 0)
    }
}

unsafe fn live(h: u64) -> Option<*mut StoreHead> {
    let head = h as *mut StoreHead;
    (!head.is_null() && (*head).magic == STORE_MAGIC).then_some(head)
}
