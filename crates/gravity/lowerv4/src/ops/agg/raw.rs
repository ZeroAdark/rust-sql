//! agg's raw half (rules R6 / R8: the family's unsafe code lives here): the lowerv4 agg hosts the IR calls, written
//! through the contract's `host!`: walk_skip.rs's `walk_bound`, emit_group_nocode.rs's `nocode_unset`,
//! finals_cse.rs's `members`, class_interval.rs's `interval_box` and class_interval_sum.rs's `interval_sum_final` /
//! `interval_range`. Their files declare them by address (`declare_host_once`), so the IR's host names are unchanged.
#![allow(unsafe_code)]
use gravity_abi::breaker::{table_kind, SELECT_MAX, T1, TABLE_T1};
use gravity_abi::ctx::Ctx;
use gravity_abi::status::raise;
use gravity_fn::arena::{AVec, ForgetSafe, Kind, OpArena, Refused};
use gravity_kernels::host_abi::AggState;

use crate::ops::agg::trans::strategies::finals_cse::{eval, WORDS};

/// The keyed tables' test kit: a frame-driven keyed fold over a table mechanism and its pointer checks.
#[cfg(test)]
pub(crate) mod testkit;

gravity_abi::host! {
    /// The `n`-th smallest distinct first key the claim's table holds (parallel merge.rs `HashAggPartial::bound`), or
    /// `i64::MAX` while it holds fewer (every row folds).
    /// CONVERT (cvagg): the scratch past `SELECT_MAX` and the prune's block map are carved from a scratch arena charged
    /// to the breaker slot `ctx`'s pipeline runs for, this hash aggregate (`OpArena::slot_owner`), made only when one
    /// is needed. A carve the allocator refuses reads no bound (the abi's short-scratch answer) or skips the prune:
    /// every row folds, the same rows.
    /// # Safety
    /// `ctx` is the claim's bound ctx; `st` is the claim's `AggState`, filled by the table's open.
    pub(crate) unsafe fn walk_bound(ctx: u64, st: u64, n: u64, flip: i64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let st = st as *const AggState;
        let t = gravity_abi::unword(unsafe { (*st).t });
        if t.is_null() || unsafe { table_kind(t as *const u8) } != TABLE_T1 {
            return i64::MAX;
        }
        let tr = unsafe { &*t };
        let n = n as usize;
        let mut a = None;
        // past the stack heap's `SELECT_MAX` the scratch is sized from the records the table holds: any LIMIT gets its bound
        // GP-1b: `flip` (`!0` descending) orders the NOT of the key: the bound is the n-th largest key's NOT
        let bound = if n <= SELECT_MAX {
            tr.nth_smallest_key_held_by(0, n, flip)
        } else if tr.held() < n as u64 {
            // fewer than `n` records hold fewer than `n` keys: no bound, as the abi answers, with no scratch
            None
        } else {
            let a = scratch(&mut a, ctx);
            zeroed::<i64>(a, tr.held() as usize).ok().and_then(|mut keys| {
                let b = tr.nth_smallest_key_held_in_by(0, n, keys.as_mut_slice(a), flip);
                // the newest carve: the bump rewinds, so the map below reuses its chunk
                keys.release(a);
                b
            })
        };
        // WC-02: the records past the bound dropped once they outnumber the walk's n (the claim's open: no appender is
        // open and the table's open counted every append), so the bound's scans here and at the claim's end (parallel
        // merge.rs HashAggPartial::bound) stop growing with the first claims' records
        if let Some(b) = bound.filter(|_| tr.prune_due(n)) {
            let a = scratch(&mut a, ctx);
            if let Ok(mut map) = zeroed::<u32>(a, tr.nblocks as usize) {
                // SAFETY: the claim's own table at its open (walk_skip `Skip::open` runs after the table's open)
                unsafe { T1::prune_held_above_by(t, 0, b, map.as_mut_slice(a), flip) };
                map.release(a);
            }
        }
        bound.unwrap_or(i64::MAX)
    }
}

/// `walk_bound`'s scratch arena, made at its first use: an arena per claim's open would cost every claim its core and
/// its ledger charges.
fn scratch(a: &mut Option<OpArena>, ctx: u64) -> &OpArena {
    a.get_or_insert_with(|| OpArena::scratch_of(OpArena::slot_owner(ctx as usize as *mut Ctx), Kind::HashAgg))
}

/// `n` zeroed elements in `a`, one carve.
fn zeroed<T: ForgetSafe + Copy + Default>(a: &OpArena, n: usize) -> Result<AVec<T>, Refused> {
    let mut v = AVec::with_capacity(a, n)?;
    v.resize(a, n, T::default())?;
    Ok(v)
}

gravity_abi::host! {
    /// The unreachable arm (`cell`): a non-NULL path key's record with neither a code nor a copy is a form that opened it
    /// without `keep`, a lowering bug: debug builds assert, release builds raise it by name, never a row.
    pub(crate) unsafe fn nocode_unset(_ctx: u64) -> i64 {
        debug_assert!(false, "a jsonb path group key's record holds neither a code nor its text");
        gravity_fn::stash_error(Box::new(gravity_fn::PgError::error("gravity: bug: a jsonb path group key's record holds neither a code nor its text (keyed/emit_group_nocode.rs)"))); // alloc: Cold
        -1
    }
}

gravity_abi::host! {
    /// Every record at `recs` (`n`) over the drain's finals at `fin` (entry `a`: value at `16 a`, valid bit 0 of the byte at
    /// `16 a + 8`): each `(value, null)` into `out`, the first overflow's raise mask returned, else 0. Pure: no allocation,
    /// no catalog. # Safety — `fin` the finals of every record's entries, `recs` `n` records, `out` `2 n` words.
    pub(crate) unsafe fn members(fin: u64, recs: u64, n: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let fin = fin as *const u8;
        let recs = recs as *const u64;
        let out = out as *mut u64;
        for i in 0..n as usize {
            let r = recs.add(WORDS * i);
            let (aggnos, flags) = (*r, *r.add(1));
            let sx = |v: u64, w: u64| match w { 0 => v as i16 as i64, 1 => v as i32 as i64, _ => v as i64 };
            let f = |j: u64| {
                let a = (aggnos >> (16 * j) & 0xffff) as usize;
                let w = if j < 2 { flags >> 8 & 3 } else { 2 };
                (*fin.add(16 * a + 8) & 1 != 0).then(|| sx((fin.add(16 * a) as *const u64).read_unaligned(), w))
            };
            let k = (flags >> 32 & 0xff == 0).then(|| sx(*r.add(2), flags >> 10 & 3));
            match eval([f(0), f(1), f(2), f(3)], k, flags) {
                Ok(v) => (out.add(2 * i).write(v.unwrap_or(0) as u64), out.add(2 * i + 1).write(u64::from(v.is_none()))),
                Err(code) => return raise::bit(code) as i64,
            };
        }
        0
    }
}

/// `members` over slices, checked: `out`'s `2 n` words for `recs`' first `n` records, every finals word a record names
/// inside `fin` (the finals_cse tests' entry).
#[cfg(test)]
pub(crate) fn members_of(fin: &[u8], recs: &[u64], out: &mut [u64]) -> i64 {
    let n = out.len() / 2;
    assert!(recs.len() >= WORDS * n);
    assert!((0..n).all(|i| (0..4).all(|j| 16 * (recs[WORDS * i] >> (16 * j) & 0xffff) as usize + 9 <= fin.len())));
    // SAFETY: checked above: `recs` holds `n` records, `out` `2 n` words, and `fin` every finals word they name.
    unsafe { members(fin.as_ptr() as u64, recs.as_ptr() as u64, n as u64, out.as_mut_ptr() as u64) }
}

gravity_abi::host! {
    /// class_interval.rs's final: PG's 16-byte Interval (`time`, then `day | month << 32` as stored) copied into the final's
    /// region (slots.rs `host_final`), its address the datum; 0 when the region is full (the caller raises).
    /// # Safety
    /// `r` is the live `Region` `host_final` hands its hosts.
    pub(crate) unsafe fn interval_box(r: u64, time: u64, dm: u64) -> u64 {
        let mut img = [0u8; 16];
        img[..8].copy_from_slice(&time.to_le_bytes());
        img[8..].copy_from_slice(&dm.to_le_bytes());
        let ret = gravity_fn::region_copy(r as *mut gravity_fn::Region, &img);
        if ret.status == gravity_fn::OK { ret.val as u64 } else { 0 }
    }
}

gravity_abi::host! {
    /// class_interval_sum.rs's final: `sum` (`avg` 0) or `avg(interval)` (`avg` 1) over an `IntervalSum` state (`time`, `day |
    /// month << 32`, N, `pinf | ninf << 32`) by the row executor's own final (fn agg_final.rs `interval_agg_final`), PG's
    /// Interval copied into the final's region, its address the datum; 0 on PG's raise (both infinities: the error stashed)
    /// or a full region (the caller raises).
    /// # Safety
    /// `r` is the live `Region` `host_final` hands its hosts.
    pub(crate) unsafe fn interval_sum_final(r: u64, time: u64, dm: u64, n: u64, infs: u64, avg: u64) -> u64 {
        let (day, month) = (dm as u32 as i32, (dm >> 32) as u32 as i32);
        let (pinf, ninf) = (infs as u32 as i64, (infs >> 32) as u32 as i64);
        match gravity_fn::agg_final::interval_agg_final(avg != 0, time as i64, day, month, n as i64, pinf, ninf) {
            Ok(Some(img)) => {
                let ret = gravity_fn::region_copy(r as *mut gravity_fn::Region, &img);
                if ret.status == gravity_fn::OK { ret.val as u64 } else { 0 }
            }
            Ok(None) => 0, // the caller enters only where the state saw a row
            Err(e) => {
                gravity_fn::stash_error(e);
                0
            }
        }
    }
}

gravity_abi::host! {
    /// class_interval_sum.rs's overflow: PG's "interval out of range" (22008) stashed for the `HOST` raise that follows.
    #[cold]
    pub(crate) fn interval_range() -> i64 {
        gravity_fn::stash_error(gravity_fn::agg_final::interval_out_of_range());
        0
    }
}
