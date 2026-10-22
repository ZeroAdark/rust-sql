//! Lock order: parent → segment → tombstone → maintenance lock → front lock → publish mutex. Nothing
//! takes the front lock and then the maintenance lock: a holder of the front lock alone (rotation, seal) never asks
//! for the maintenance lock, and every taker of both asks for the maintenance lock first ([`try_lock`], [`lock`]).

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use pgsync::Mutex;
use types_core::{Oid, RELATION_RELATION_ID};
use types_error::PgResult;
use types_rel::ExclusiveLock;
use types_storage::lock::LOCKTAG;

const MAINT_SUBID: u16 = 0xfc2;
const FRONT_SUBID: u16 = 0xfc3;
const ROTATE_SUBID: u16 = 0xfc4;
pub const ROTATING_STALE_S: u64 = 10;

pub const ROTATE_BLOCKS: u32 = 4096;

pub const PRE_EXTEND_BLOCKS: u32 = ROTATE_BLOCKS / 4;

pub const MAX_ROTATE_BLOCKS: u32 = 8 * ROTATE_BLOCKS;
pub const ROTATE_TARGET_ROWS: u64 = 128 << 10;
const ROTATE_TARGET_GUC: &str = "pgrust.pgrc_rotate_target_rows";
pub const SEAL_MEM_BUDGET: u64 = 1 << 30;
const SEAL_MEM_GUC: &str = "pgrust.pgrc_seal_mem_budget";

pub const SEAL_WAL_RATE_MB: u64 = 48;
const SEAL_WAL_RATE_GUC: &str = "pgrust.pgrc_seal_wal_rate";

pub fn seal_wal_rate() -> u64 {
    setting(SEAL_WAL_RATE_GUC).unwrap_or(SEAL_WAL_RATE_MB).saturating_mul(1 << 20)
}

pub const BACKLOG_BLOCKS: u64 = 2 * ROTATE_BLOCKS as u64;
const BACKLOG_GUC: &str = "pgrust.pgrc_backlog_blocks";
pub const SEAL_THREADS: u64 = 4;
const SEAL_THREADS_GUC: &str = "pgrust.pgrc_seal_threads";

const BACKLOG_SLOTS: usize = 256;

pgsync::process_global! {
    static DUE: Mutex<Vec<(Oid, Oid)>> = Mutex::new(Vec::new());
    /// `parent << 32 | sealing blocks`, by `parent % BACKLOG_SLOTS`; a table that lost its slot reads as no backlog.
    static SEALING: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static ROTATE: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static SEAL_BPB: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static DENSITY: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static ACTIVE: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static PACED_MS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static BP_WAITS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    /// RBPROF: `parent << 32 | ms` its statements spent in backpressure waits (pgrc_compact_stats bp_ms=).
    static BP_MS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    /// RBPROF: `parent << 32 | n` times its writers looked for a held sealing segment (pgrc_compact_stats bp_checks=).
    static BP_CHECKS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    /// SEALSIZE fix 4 (A): `parent << 32 | rows` its inserters published into the active segment ([`publish_rows`]).
    static ACTIVE_ROWS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static ACTIVE_SEG: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static RETIRED_SEG: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static SEALING_ROWS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static FULL_ROWS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static ROTATING: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static STALL_ROT: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static SEAL_MEM: AtomicU64 = AtomicU64::new(0);
    static IN_HAND: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static FUTILE: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static L0_WAITS: [AtomicU64; BACKLOG_SLOTS] = [const { AtomicU64::new(0) }; BACKLOG_SLOTS];
    static COMPACTOR: AtomicU64 = AtomicU64::new(0);
}

thread_local! {
    static LAST: Cell<(Oid, u32)> = const { Cell::new((0, 0)) };
    static LAST_TOMB: Cell<(Oid, u32)> = const { Cell::new((0, 0)) };
    static CAP: Cell<Option<(u64, Option<u64>)>> = const { Cell::new(None) };
    static SEAL_BUDGET: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    static TARGET: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    static STALL_MIN: Cell<bool> = const { Cell::new(false) };
    static PENDING: Cell<(Oid, Oid, Oid, u32)> = const { Cell::new((0, 0, 0, 0)) };
}

pub const PUBLISH_ROWS: u32 = 1000;

/// `c`'s option, looked up by name (`read`) only when this backend's GUC store has changed since `c` was
/// filled: a SET, RESET, a SET LOCAL's end, a reload and a new placeholder all bump `guc::store::store_mutation_count`
pub(crate) fn per_guc_change<T: Copy>(c: &Cell<Option<(u64, T)>>, read: impl FnOnce() -> T) -> T {
    snapshot(c, guc::store::store_mutation_count(), read)
}

fn snapshot<T: Copy>(c: &Cell<Option<(u64, T)>>, at: u64, read: impl FnOnce() -> T) -> T {
    match c.get() {
        Some((seen, v)) if seen == at => v,
        _ => {
            let v = read();
            c.set(Some((at, v)));
            v
        }
    }
}

fn cap_setting() -> Option<u64> {
    CAP.with(|c| per_guc_change(c, || setting(BACKLOG_GUC)))
}

pub fn backlog_cap() -> u64 {
    cap_setting().unwrap_or(BACKLOG_BLOCKS)
}

pub fn backlog_cap_for(parent: Oid) -> u64 {
    cap_setting().unwrap_or(2 * u64::from(rotate_blocks(parent)))
}

fn slot_value(slots: &[AtomicU64; BACKLOG_SLOTS], parent: Oid) -> Option<u64> {
    let v = slots[parent as usize % BACKLOG_SLOTS].load(Relaxed);
    (v >> 32 == u64::from(parent)).then_some(v & u64::from(u32::MAX))
}

fn set_slot(slots: &[AtomicU64; BACKLOG_SLOTS], parent: Oid, v: u64) {
    slots[parent as usize % BACKLOG_SLOTS].store(u64::from(parent) << 32 | v.min(u64::from(u32::MAX)), Relaxed);
}

fn move_slot(slots: &[AtomicU64; BACKLOG_SLOTS], parent: Oid, by: u64, neg: bool) {
    let s = &slots[parent as usize % BACKLOG_SLOTS];
    let mut cur = s.load(Relaxed);
    loop {
        let v = if cur >> 32 == u64::from(parent) { cur & u64::from(u32::MAX) } else { 0 };
        let v = if neg { v.saturating_sub(by) } else { v.saturating_add(by).min(u64::from(u32::MAX)) };
        match s.compare_exchange_weak(cur, u64::from(parent) << 32 | v, Relaxed, Relaxed) {
            Ok(_) => return,
            Err(c) => cur = c,
        }
    }
}

fn add_slot(slots: &[AtomicU64; BACKLOG_SLOTS], parent: Oid, by: u64) {
    move_slot(slots, parent, by, false);
}

fn take_slot(slots: &[AtomicU64; BACKLOG_SLOTS], parent: Oid) -> u64 {
    let v = slots[parent as usize % BACKLOG_SLOTS].swap(u64::from(parent) << 32, Relaxed);
    if v >> 32 == u64::from(parent) { v & u64::from(u32::MAX) } else { 0 }
}

pub fn target_rows() -> u64 {
    TARGET.with(|c| per_guc_change(c, || setting(ROTATE_TARGET_GUC).unwrap_or(ROTATE_TARGET_ROWS)))
}

pub fn rows_target(parent: Oid) -> u64 {
    rows_target_at(parent, target_rows())
}

fn rows_target_at(parent: Oid, t: u64) -> u64 {
    slot_value(&FULL_ROWS, parent).filter(|r| *r > 0).map_or(t, |r| t.min(r))
}

pub fn rotation_due(parent: Oid) -> bool {
    full_due(parent) || stall_due(parent)
}

fn full_due(parent: Oid) -> bool {
    let blocks = active_blocks(parent);
    if target_rows() == 0 {
        return blocks >= u64::from(rotate_blocks(parent));
    }
    blocks >= u64::from(MAX_ROTATE_BLOCKS) || (blocks >= u64::from(ROTATE_BLOCKS) && active_rows(parent) >= rows_target(parent))
}

/// Invariant: while backpressure waits, a seal or a rotation is due or in
/// flight (`route::backpressure` waits only on one in flight and starts this one).
pub fn stall_due(parent: Oid) -> bool {
    if target_rows() == 0 || cap_setting().is_some() {
        return false;
    }
    let ar = active_rows(parent);
    ar > 0 && !sealing_in_progress(parent) && (sealing_rows(parent) + ar > rows_cap(parent) || sealing_blocks(parent) + active_blocks(parent) > BACKSTOP_BLOCKS)
}

pub fn sealing_in_progress(parent: Oid) -> bool {
    sealing_blocks(parent) > 0 || sealing_rows(parent) > 0
}

pub fn in_flight(parent: Oid) -> bool {
    sealing_in_progress(parent) || rotating(parent)
}

pub fn start_stall_rotation(db: Oid, parent: Oid) -> bool {
    if stall_due(parent) {
        queue(db, parent);
        if begin_rotation(parent) && !(pgrc_front_seams::rotate_detached::is_installed() && pgrc_front_seams::rotate_detached::call(db, parent)) {
            end_rotation(parent);
        }
    }
    in_flight(parent)
}

pub fn rotate_min(parent: Oid) -> u32 {
    let full = full_due(parent);
    STALL_MIN.with(|c| c.set(false));
    match (target_rows(), full || stall_due(parent)) {
        (0, _) => rotate_blocks(parent),
        (_, true) => {
            STALL_MIN.with(|c| c.set(!full));
            1
        }
        _ => MAX_ROTATE_BLOCKS,
    }
}

pub fn stall_rotations(parent: Oid) -> u64 {
    slot_value(&STALL_ROT, parent).unwrap_or(0)
}

pub fn active_rows(parent: Oid) -> u64 {
    slot_value(&ACTIVE_ROWS, parent).unwrap_or(0)
}

pub fn sealing_rows(parent: Oid) -> u64 {
    slot_value(&SEALING_ROWS, parent).unwrap_or(0)
}

pub fn publish_rows(parent: Oid, seg: Oid, n: u64) {
    let i = parent as usize % BACKLOG_SLOTS;
    let mine = |v: u64| (v >> 32 == u64::from(parent)).then_some(v & u64::from(u32::MAX));
    loop {
        let cur = ACTIVE_SEG[i].load(Relaxed);
        match mine(cur) {
            Some(s) if s == u64::from(seg) => return add_slot(&ACTIVE_ROWS, parent, n),
            Some(s) if s != 0 => return add_slot(&SEALING_ROWS, parent, n),
            _ if slot_value(&RETIRED_SEG, parent) == Some(u64::from(seg)) => return add_slot(&SEALING_ROWS, parent, n),
            _ => {
                if ACTIVE_SEG[i].compare_exchange(cur, u64::from(parent) << 32 | u64::from(seg), Relaxed, Relaxed).is_ok() {
                    if mine(cur).is_none() {
                        set_slot(&ACTIVE_ROWS, parent, 0);
                    }
                    return add_slot(&ACTIVE_ROWS, parent, n);
                }
            }
        }
    }
}

pub fn publish_pending() {
    let (db, parent, seg, n) = PENDING.with(|p| p.replace((0, 0, 0, 0)));
    if n == 0 {
        return;
    }
    publish_rows(parent, seg, u64::from(n));
    if target_rows() != 0 && rotation_due(parent) {
        queue(db, parent);
        if begin_rotation(parent) && !(pgrc_front_seams::rotate_detached::is_installed() && pgrc_front_seams::rotate_detached::call(db, parent)) {
            end_rotation(parent);
        }
    }
}

fn now_s() -> u64 {
    (pg_clock::mono_ms().max(0) as u64) / 1000
}

/// Marks a detached rotation of `parent` in flight; false when one is (a mark older than
/// [`ROTATING_STALE_S`] is a dead worker's and is taken over: no mark outlives its worker for long).
pub fn begin_rotation(parent: Oid) -> bool {
    let i = parent as usize % BACKLOG_SLOTS;
    let cur = ROTATING[i].load(Relaxed);
    let live = cur >> 32 == u64::from(parent) && cur & u64::from(u32::MAX) != 0 && now_s() + 1 < (cur & u64::from(u32::MAX)) + ROTATING_STALE_S;
    !live && ROTATING[i].compare_exchange(cur, u64::from(parent) << 32 | (now_s() + 1).min(u64::from(u32::MAX)), Relaxed, Relaxed).is_ok()
}

pub fn end_rotation(parent: Oid) {
    set_slot(&ROTATING, parent, 0);
}

pub fn rotating(parent: Oid) -> bool {
    slot_value(&ROTATING, parent).is_some_and(|v| v != 0 && now_s() + 1 < v + ROTATING_STALE_S)
}

fn pending_rows(parent: Oid) -> u64 {
    PENDING.with(|p| {
        let (_, q, _, n) = p.get();
        if q == parent { u64::from(n) } else { 0 }
    })
}

pub fn sealed_rows(parent: Oid, rows: u64) {
    move_slot(&SEALING_ROWS, parent, rows, true);
}

pub const BACKSTOP_BLOCKS: u64 = 2 * MAX_ROTATE_BLOCKS as u64;

fn base_rows(parent: Oid) -> u64 {
    base_rows_of(parent, &Unsealed::of(parent))
}

fn base_rows_of(parent: Oid, u: &Unsealed) -> u64 {
    let d = slot_value(&DENSITY, parent).filter(|d| *d > 0).unwrap_or_else(|| if u.blocks == 0 { 0 } else { (u.rows << 8) / u.blocks });
    let live = if u.blocks >= LIVE_MIN_BLOCKS { (u.rows << 8) / u.blocks } else { 0 };
    d.max(live) * u64::from(ROTATE_BLOCKS) >> 8
}

struct Unsealed {
    blocks: u64,
    rows: u64,
    sealing_blocks: u64,
}

impl Unsealed {
    fn of(parent: Oid) -> Unsealed {
        let sealing_blocks = sealing_blocks(parent);
        Unsealed { blocks: sealing_blocks + active_blocks(parent), rows: sealing_rows(parent) + active_rows(parent), sealing_blocks }
    }
}

pub const LIVE_MIN_BLOCKS: u64 = 256;

pub fn rows_cap(parent: Oid) -> u64 {
    rows_cap_of(parent, target_rows(), &Unsealed::of(parent))
}

fn rows_cap_of(parent: Oid, t: u64, u: &Unsealed) -> u64 {
    2 * rows_target_at(parent, t).max(base_rows_of(parent, u))
}

pub fn over_cap(parent: Oid) -> bool {
    let t = target_rows();
    if t == 0 || cap_setting().is_some() {
        return backlog(parent) > backlog_cap_for(parent);
    }
    let u = Unsealed::of(parent);
    u.rows + pending_rows(parent) > rows_cap_of(parent, t, &u) || u.sealing_blocks + own_blocks(parent) > BACKSTOP_BLOCKS
}

pub fn active_blocks(parent: Oid) -> u64 {
    slot_value(&ACTIVE, parent).unwrap_or(0)
}

pub fn rotated(parent: Oid, old: Oid) -> bool {
    let stall = STALL_MIN.with(|c| c.replace(false));
    if stall {
        add_slot(&STALL_ROT, parent, 1);
    }
    set_slot(&ACTIVE, parent, 0);
    set_slot(&RETIRED_SEG, parent, u64::from(old));
    let named = slot_value(&ACTIVE_SEG, parent);
    set_slot(&ACTIVE_SEG, parent, 0);
    let rows = take_slot(&ACTIVE_ROWS, parent);
    if named.is_none_or(|s| s == 0 || s == u64::from(old)) {
        add_slot(&SEALING_ROWS, parent, rows);
    }
    stall
}

pub fn note_paced(parent: Oid, ms: u64) {
    add_slot(&PACED_MS, parent, ms);
}

pub fn note_bp_wait(parent: Oid) {
    add_slot(&BP_WAITS, parent, 1);
}

pub fn note_bp_ms(parent: Oid, ms: u64) {
    add_slot(&BP_MS, parent, ms);
}

pub fn bp_ms(parent: Oid) -> u64 {
    slot_value(&BP_MS, parent).unwrap_or(0)
}

pub fn note_bp_check(parent: Oid) {
    add_slot(&BP_CHECKS, parent, 1);
}

pub fn bp_checks(parent: Oid) -> u64 {
    slot_value(&BP_CHECKS, parent).unwrap_or(0)
}

/// SEALSIZE fix 3: (ms the table's seals waited for the WAL pace, statements that waited on backpressure).
pub fn pace_stats(parent: Oid) -> (u64, u64) {
    (slot_value(&PACED_MS, parent).unwrap_or(0), slot_value(&BP_WAITS, parent).unwrap_or(0))
}

pub fn rotate_blocks(parent: Oid) -> u32 {
    slot_value(&ROTATE, parent).map_or(ROTATE_BLOCKS, |b| (b as u32).clamp(ROTATE_BLOCKS, MAX_ROTATE_BLOCKS))
}

pub fn rotate_blocks_for(rows: u64, blocks: u64, bytes: u64, target_rows: u64, full_bytes: u64) -> u32 {
    if target_rows == 0 || rows == 0 || blocks == 0 {
        return ROTATE_BLOCKS;
    }
    let by_bytes = if bytes == 0 { u64::MAX } else { u64::try_from(u128::from(full_bytes) * u128::from(rows) / u128::from(bytes)).unwrap_or(u64::MAX) };
    let rows_t = target_rows.min(by_bytes);
    let b = u64::try_from((u128::from(rows_t) * u128::from(blocks)).div_ceil(u128::from(rows))).unwrap_or(u64::MAX);
    b.clamp(u64::from(ROTATE_BLOCKS), u64::from(MAX_ROTATE_BLOCKS)) as u32
}

pub const DENSITY_WEIGHT: u64 = 4;
pub const ROTATE_GROWTH: (u32, u32) = (5, 4);

pub fn next_rotation(prev: Option<(u32, u64)>, rows: u64, blocks: u64, bytes: u64, target_rows: u64, full_bytes: u64) -> (u32, u64) {
    let seen = ((u128::from(rows) << 8) / u128::from(blocks.max(1))).clamp(1, u128::from(u32::MAX)) as u64;
    let density = match prev {
        Some((_, d)) if d > 0 => (d * (DENSITY_WEIGHT - 1) + seen).div_ceil(DENSITY_WEIGHT),
        _ => seen,
    };
    let eff_blocks = u64::try_from((u128::from(rows) << 8).div_ceil(u128::from(density))).unwrap_or(u64::MAX).max(1);
    let r = rotate_blocks_for(rows, eff_blocks, bytes, target_rows, full_bytes);
    let r = match prev {
        Some((p, _)) if target_rows != 0 => r.min((p.max(ROTATE_BLOCKS) * ROTATE_GROWTH.0 / ROTATE_GROWTH.1).max(ROTATE_BLOCKS)),
        _ => r,
    };
    (r, density)
}

pub fn sealed_shape(parent: Oid, rows: u64, blocks: u64, bytes: u64) {
    if rows == 0 || blocks == 0 {
        return;
    }
    let target = setting(ROTATE_TARGET_GUC).unwrap_or(ROTATE_TARGET_ROWS);
    let full = pgrc_write::writer::PartCutPolicy::default().max_bytes;
    let prev = slot_value(&ROTATE, parent).map(|r| (r as u32, slot_value(&DENSITY, parent).unwrap_or(0)));
    let (r, density) = next_rotation(prev, rows, blocks, bytes, target, full);
    set_slot(&ROTATE, parent, u64::from(r));
    set_slot(&DENSITY, parent, density);
    set_slot(&SEAL_BPB, parent, bytes.div_ceil(blocks));
    if bytes > 0 {
        set_slot(&FULL_ROWS, parent, u64::try_from(u128::from(full) * u128::from(rows) / u128::from(bytes)).unwrap_or(u64::MAX).max(1));
    }
}

pub fn seal_mem_estimate(parent: Oid, blocks: u64) -> u64 {
    blocks.saturating_mul(slot_value(&SEAL_BPB, parent).map_or(2 * types_core::BLCKSZ as u64, |b| b.saturating_mul(SEAL_MEM_FACTOR)))
}

pub const SEAL_MEM_FACTOR: u64 = 2;

pub fn seal_mem_budget() -> u64 {
    SEAL_BUDGET.with(|c| per_guc_change(c, || setting(SEAL_MEM_GUC).unwrap_or(SEAL_MEM_BUDGET)))
}

#[must_use]
pub struct SealMem(u64);

impl Drop for SealMem {
    fn drop(&mut self) {
        SEAL_MEM.fetch_sub(self.0, Relaxed);
    }
}

/// Reserves `bytes` when they fit the budget, or when no other seal holds any.
pub fn try_seal_mem(bytes: u64) -> Option<SealMem> {
    try_reserve(&SEAL_MEM, bytes, seal_mem_budget()).then(|| SealMem(bytes))
}

pub fn force_seal_mem(bytes: u64) -> SealMem {
    SEAL_MEM.fetch_add(bytes, Relaxed);
    SealMem(bytes)
}

fn try_reserve(used: &AtomicU64, bytes: u64, budget: u64) -> bool {
    used.fetch_update(Relaxed, Relaxed, |u| (u == 0 || u.saturating_add(bytes) <= budget).then(|| u.saturating_add(bytes))).is_ok()
}

pub fn seal_mem_in_use() -> u64 {
    SEAL_MEM.load(Relaxed)
}

pub fn seal_threads() -> usize {
    setting(SEAL_THREADS_GUC).unwrap_or(SEAL_THREADS) as usize
}

pub const SETTING_POINT: &str = "pgrc-setting-read";

pub fn setting(name: &str) -> Option<u64> {
    setting_text(name).and_then(|v| v.trim().parse().ok())
}

pub fn setting_f64(name: &str) -> Option<f64> {
    setting_text(name).and_then(|v| v.trim().parse::<f64>().ok()).filter(|x| x.is_finite())
}

fn setting_text(name: &str) -> Option<String> {
    // the count is the point's; its action's own error is not this lookup's
    let _ = injection_point::injection_point(SETTING_POINT);
    let seam = guc_seams::get_config_option_missing_ok::is_installed();
    seam.then(|| guc_seams::get_config_option_missing_ok::call(name).ok().flatten()).flatten()
}

pub fn try_lock(parent: Oid) -> PgResult<bool> {
    Ok(try_maint_lock(parent)? && try_front_lock(parent)? && try_rotate_lock(parent)?)
}

/// Lock order: maintenance, front, rotate; a rotation asks for no other.
pub fn try_rotate_lock(parent: Oid) -> PgResult<bool> {
    lmgr::ConditionalLockDatabaseObject(RELATION_RELATION_ID, parent, ROTATE_SUBID, ExclusiveLock)
}

pub fn try_maint_lock(parent: Oid) -> PgResult<bool> {
    debug_assert!(order_ok(parent), "pgrc lock order: maintenance lock of {parent} asked for under its front lock");
    lmgr::ConditionalLockDatabaseObject(RELATION_RELATION_ID, parent, MAINT_SUBID, ExclusiveLock)
}

pub fn try_front_lock(parent: Oid) -> PgResult<bool> {
    debug_assert!(front_order_ok(parent), "pgrc lock order: front lock of {parent} asked for under its rotate lock");
    lmgr::ConditionalLockDatabaseObject(RELATION_RELATION_ID, parent, FRONT_SUBID, ExclusiveLock)
}

pub fn maint_held(parent: Oid) -> bool {
    lock_seams::lock_held_by_me::is_installed() && lock_seams::lock_held_by_me::call(tag(parent, MAINT_SUBID), ExclusiveLock, false)
}

fn front_order_ok(parent: Oid) -> bool {
    if !lock_seams::lock_held_by_me::is_installed() {
        return true;
    }
    let held = |subid| lock_seams::lock_held_by_me::call(tag(parent, subid), ExclusiveLock, false);
    !held(ROTATE_SUBID) || held(FRONT_SUBID)
}

pub fn lock(parent: Oid) -> PgResult<()> {
    lock_maint(parent)?;
    lmgr::LockDatabaseObject(RELATION_RELATION_ID, parent, FRONT_SUBID, ExclusiveLock)?;
    lmgr::LockDatabaseObject(RELATION_RELATION_ID, parent, ROTATE_SUBID, ExclusiveLock)
}

pub fn lock_maint(parent: Oid) -> PgResult<()> {
    debug_assert!(order_ok(parent), "pgrc lock order: maintenance lock of {parent} asked for under its front lock");
    lmgr::LockDatabaseObject(RELATION_RELATION_ID, parent, MAINT_SUBID, ExclusiveLock)
}

pub fn order_ok(parent: Oid) -> bool {
    if !lock_seams::lock_held_by_me::is_installed() {
        return true;
    }
    let held = |subid| lock_seams::lock_held_by_me::call(tag(parent, subid), ExclusiveLock, false);
    (!held(FRONT_SUBID) || held(MAINT_SUBID)) && (!held(ROTATE_SUBID) || held(MAINT_SUBID))
}

fn tag(parent: Oid, subid: u16) -> LOCKTAG {
    LOCKTAG::object(init_small::globals::MyDatabaseId(), RELATION_RELATION_ID, parent, subid)
}

pub fn flush_waiting(parent: Oid) -> PgResult<bool> {
    lock_seams::lock_has_waiters::call(tag(parent, MAINT_SUBID), ExclusiveLock)
}

pub fn lock_waiting(parent: Oid) -> bool {
    if !lock_seams::lock_has_waiters::is_installed() {
        return false;
    }
    [MAINT_SUBID, FRONT_SUBID].iter().any(|s| lock_seams::lock_has_waiters::call(tag(parent, *s), ExclusiveLock).unwrap_or(false))
}

/// the insert path knows the block it wrote, so the check costs a compare; a backend queues a table once per block.
pub fn inserted(db: Oid, parent: Oid, seg: Oid, block: u32) {
    let full = PENDING.with(|p| {
        let (d, q, s, n) = p.get();
        if (q, s) != (parent, seg) && n > 0 {
            publish_pending();
        }
        let n = if (q, s) == (parent, seg) { n + 1 } else { 1 };
        p.set((if (q, s) == (parent, seg) { d } else { db }, parent, seg, n));
        n >= PUBLISH_ROWS
    });
    if full {
        publish_pending();
    }
    if LAST.with(|l| l.replace((parent, block))) == (parent, block) {
        return;
    }
    set_slot(&ACTIVE, parent, u64::from(block) + 1);
    let due = match target_rows() {
        0 => block + 1 >= rotate_blocks(parent),
        _ => block + 1 >= MAX_ROTATE_BLOCKS,
    };
    if due {
        queue(db, parent);
    }
}

pub fn tombstoned(db: Oid, parent: Oid, block: u32) {
    if LAST_TOMB.with(|l| l.replace((parent, block))) == (parent, block) || block + 1 < crate::compile::COMPILE_BLOCKS {
        return;
    }
    queue(db, parent);
}

pub fn queue(db: Oid, parent: Oid) {
    let mut due = pgsync::lock(&DUE);
    if !due.contains(&(db, parent)) {
        due.push((db, parent));
    }
}

pub fn set_sealing(parent: Oid, blocks: u64) {
    SEALING[parent as usize % BACKLOG_SLOTS].store((parent as u64) << 32 | blocks.min(u32::MAX as u64), Relaxed);
}

pub fn sealing_blocks(parent: Oid) -> u64 {
    let v = SEALING[parent as usize % BACKLOG_SLOTS].load(Relaxed);
    if v >> 32 == parent as u64 { v & u32::MAX as u64 } else { 0 }
}

pub fn backlog(parent: Oid) -> u64 {
    sealing_blocks(parent) + own_blocks(parent)
}

fn own_blocks(parent: Oid) -> u64 {
    let (p, b) = LAST.with(|l| l.get());
    if p == parent { b as u64 + 1 } else { 0 }
}

pub fn due_databases() -> Vec<Oid> {
    let mut dbs: Vec<Oid> = pgsync::lock(&DUE).iter().map(|d| d.0).collect();
    dbs.sort_unstable();
    dbs.dedup();
    dbs
}

pub fn take_due(db: Oid) -> Vec<Oid> {
    let mut due = pgsync::lock(&DUE);
    let (mine, rest) = due.drain(..).partition(|d| d.0 == db);
    *due = rest;
    mine.into_iter().map(|d: (Oid, Oid)| d.1).collect()
}

pgsync::process_global! {
    static COMPACT_DUE: Mutex<Vec<(Oid, Oid)>> = Mutex::new(Vec::new());
}

pub fn sealed(db: Oid, parent: Oid) -> bool {
    let mut due = pgsync::lock(&COMPACT_DUE);
    let new = !due.contains(&(db, parent));
    if new {
        due.push((db, parent));
    }
    new
}

pub fn compact_queued(db: Oid, parent: Oid) -> bool {
    pgsync::lock(&COMPACT_DUE).contains(&(db, parent))
}

pub fn set_compactor(on: bool) {
    COMPACTOR.store(u64::from(on), Relaxed);
}

pub fn compactor_on() -> bool {
    COMPACTOR.load(Relaxed) != 0
}

pub fn set_compacting(parent: Oid, on: bool) {
    set_slot(&IN_HAND, parent, u64::from(on));
}

pub fn compacting(parent: Oid) -> bool {
    slot_value(&IN_HAND, parent).is_some_and(|v| v != 0)
}

pub const FUTILE_SLACK: u64 = 16;

pub fn note_futile(parent: Oid, l0: Option<u32>) {
    set_slot(&FUTILE, parent, l0.map_or(0, |n| u64::from(n) + 1));
}

pub fn futile(parent: Oid, l0: u32) -> bool {
    slot_value(&FUTILE, parent).is_some_and(|v| v != 0 && u64::from(l0) < v - 1 + FUTILE_SLACK)
}

pub fn note_l0_wait(parent: Oid) {
    add_slot(&L0_WAITS, parent, 1);
}

pub fn l0_waits(parent: Oid) -> u64 {
    slot_value(&L0_WAITS, parent).unwrap_or(0)
}

pub fn compact_due_databases() -> Vec<Oid> {
    let mut dbs: Vec<Oid> = pgsync::lock(&COMPACT_DUE).iter().map(|d| d.0).collect();
    dbs.sort_unstable();
    dbs.dedup();
    dbs
}

pub fn take_compact_due(db: Oid) -> Vec<Oid> {
    let mut due = pgsync::lock(&COMPACT_DUE);
    let (mine, rest) = due.drain(..).partition(|d| d.0 == db);
    *due = rest;
    mine.into_iter().map(|d: (Oid, Oid)| d.1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static HELD: std::cell::RefCell<Vec<(Oid, u16)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn held_by_me(tag: LOCKTAG, _: types_storage::lock::LOCKMODE, _: bool) -> bool {
        HELD.with(|h| h.borrow().contains(&(tag.locktag_field3, tag.locktag_field4)))
    }

    fn hold(parent: Oid, subids: &[u16]) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| lock_seams::lock_held_by_me::set(held_by_me));
        HELD.with(|h| *h.borrow_mut() = subids.iter().map(|&s| (parent, s)).collect());
    }

    #[test]
    fn a_rotation_holds_the_rotate_lock_alone_and_it_comes_last() {
        hold(1003, &[ROTATE_SUBID]);
        assert!(!order_ok(1003), "rotate, then maintenance: out of order");
        assert!(!front_order_ok(1003), "rotate, then front: out of order");
        hold(1003, &[MAINT_SUBID, FRONT_SUBID, ROTATE_SUBID]);
        assert!(order_ok(1003) && front_order_ok(1003), "a flush holding all three asks again in order");
        hold(1003, &[FRONT_SUBID]);
        assert!(front_order_ok(1003), "a seal's front lock alone");
    }

    #[test]
    fn one_detached_rotation_in_flight_a_table_and_a_dead_ones_mark_is_taken_over() {
        let parent = 1600;
        assert!(!rotating(parent));
        assert!(begin_rotation(parent));
        assert!(rotating(parent) && !begin_rotation(parent), "one in flight");
        assert!(begin_rotation(parent + 1), "another table's is its own");
        end_rotation(parent);
        assert!(!rotating(parent) && begin_rotation(parent));
        set_slot(&ROTATING, parent, (now_s() + 1).saturating_sub(ROTATING_STALE_S).max(1));
        if now_s() + 1 > ROTATING_STALE_S {
            assert!(!rotating(parent) && begin_rotation(parent), "a stale mark is taken over");
        }
    }

    #[test]
    fn the_maintenance_lock_is_never_asked_for_under_the_front_lock_alone() {
        hold(1000, &[]);
        assert!(order_ok(1000));
        hold(1000, &[MAINT_SUBID]);
        assert!(order_ok(1000));
        hold(1000, &[MAINT_SUBID, FRONT_SUBID]);
        assert!(order_ok(1000), "asking again for a held maintenance lock never waits");
        hold(1000, &[FRONT_SUBID]);
        assert!(!order_ok(1000), "front, then maintenance: out of order");
        assert!(order_ok(1001), "another table's front lock is not this one's");
        hold(1000, &[]);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "pgrc lock order")]
    fn debug_builds_assert_the_order_where_the_maintenance_lock_is_taken() {
        hold(1002, &[FRONT_SUBID]);
        let _ = try_maint_lock(1002);
    }

    #[test]
    fn the_backlog_is_the_published_sealing_blocks_plus_this_backends_last_block() {
        set_sealing(700, 9000);
        assert_eq!(backlog(700), 9000);
        inserted(1, 700, 70_001, 41);
        assert_eq!(backlog(700), 9042);
        assert_eq!(backlog(701), 0, "another table counts none of this backend's blocks");
        set_sealing(700 + BACKLOG_SLOTS as Oid, 5);
        assert_eq!(backlog(700), 42, "a table that lost its slot reads as no sealing backlog");
        assert_eq!((sealing_blocks(700), sealing_blocks(700 + BACKLOG_SLOTS as Oid)), (0, 5), "a seal in progress: the slot's owner only");
        assert_eq!(backlog_cap(), BACKLOG_BLOCKS);
    }

    #[test]
    fn a_table_past_the_rotation_threshold_is_queued_once() {
        set_target(0);
        inserted(3, 800, 80_001, ROTATE_BLOCKS - 2);
        assert!(!take_due(3).contains(&800));
        inserted(3, 800, 80_001, ROTATE_BLOCKS - 1);
        inserted(3, 800, 80_001, ROTATE_BLOCKS - 1);
        inserted(3, 800, 80_001, ROTATE_BLOCKS);
        assert_eq!(take_due(3), vec![800]);
        assert!(take_due(3).is_empty());
    }

    #[test]
    fn the_rotation_size_reaches_a_full_part_within_the_bounds() {
        let full = 256 << 20;
        let r = |rows, blocks, bytes| rotate_blocks_for(rows, blocks, bytes, ROTATE_TARGET_ROWS, full);
        assert_eq!(r(420_000, 4200, 420_000 * 48), ROTATE_BLOCKS, "narrow: past the row target at the base size");
        let wide = r(41_700, 4110, 41_700 * 1371);
        assert_eq!(wide, (ROTATE_TARGET_ROWS * 4110).div_ceil(41_700) as u32, "wide (hits): the row target before a part full by bytes (~196k rows)");
        let at256 = rotate_blocks_for(41_700, 4110, 41_700 * 1371, 256 << 10, full);
        assert!((19_000..20_000).contains(&at256), "a 256 Ki target: a part full by bytes (~196k rows) first: {at256}");
        assert_eq!(r(60_000, 4096, 60_000 * 300), (ROTATE_TARGET_ROWS * 4096).div_ceil(60_000) as u32, "the row target first");
        assert_eq!(r(4096, 4096, 4096 * 1371), MAX_ROTATE_BLOCKS, "one row a block: capped");
        assert_eq!(r(41_700, 4110, 0), MAX_ROTATE_BLOCKS.min(((ROTATE_TARGET_ROWS * 4110).div_ceil(41_700)) as u32));
        assert_eq!(rotate_blocks_for(41_700, 4110, 41_700 * 1371, 0, full), ROTATE_BLOCKS, "target 0: off");
        assert_eq!(r(0, 4096, 0), ROTATE_BLOCKS, "no rows sealed: the base size");
        assert_eq!(r(5, 4096, 5 * 50_000_000), ROTATE_BLOCKS, "huge rows: never below the base size");
    }

    #[test]
    fn a_tables_rotation_size_moves_its_wake_and_its_backlog_cap() {
        set_target(0);
        let parent = 1200;
        assert_eq!(rotate_blocks(parent), ROTATE_BLOCKS);
        assert_eq!(backlog_cap_for(parent), BACKLOG_BLOCKS);
        sealed_shape(parent, 41_700, 4110, 41_700 * 1371);
        let r = rotate_blocks(parent);
        assert!(r > 3 * ROTATE_BLOCKS && r <= MAX_ROTATE_BLOCKS, "{r}"); // 128 Ki rows at ~10 rows a block: ~12.9 k
        assert_eq!(backlog_cap_for(parent), 2 * u64::from(r), "fix 2: two of its rotations (the front's bound)");
        assert_eq!(BACKLOG_BLOCKS, 2 * u64::from(ROTATE_BLOCKS), "and two of the base size before any seal");
        assert_eq!(rotate_blocks(parent + BACKLOG_SLOTS as Oid), ROTATE_BLOCKS, "another table on the slot keeps the base");
        inserted(5, parent, 120_001, ROTATE_BLOCKS + 10);
        assert!(!take_due(5).contains(&parent), "under its own size: not due");
        inserted(5, parent, 120_001, r - 1);
        assert_eq!(take_due(5), vec![parent]);
        assert_eq!(seal_mem_estimate(parent, 4110), 4110 * SEAL_MEM_FACTOR * (41_700u64 * 1371).div_ceil(4110));
        assert_eq!(seal_mem_estimate(parent + 1, 10), 10 * 2 * types_core::BLCKSZ as u64);
    }

    #[test]
    fn rows_rotate_a_wide_segment_at_the_row_target_and_a_narrow_one_at_the_base_size() {
        set_target(ROTATE_TARGET_ROWS);
        let (db, wide, seg) = (6, 1300, 130_001);
        for b in 0..ROTATE_BLOCKS + 10 {
            inserted(db, wide, seg, b);
        }
        publish_pending();
        assert!(!rotation_due(wide), "past the base size with {} rows: not due", active_rows(wide));
        assert_eq!(rotate_min(wide), MAX_ROTATE_BLOCKS, "the block cap alone");
        let mut i = 0;
        while active_rows(wide) + pending_rows(wide) < ROTATE_TARGET_ROWS {
            inserted(db, wide, seg, ROTATE_BLOCKS + 10 + i / 3); // three rows a block: sparse pages
            i += 1;
        }
        publish_pending();
        assert!(rotation_due(wide) && rotate_min(wide) == 1);
        assert!(take_due(db).contains(&wide), "a full segment is queued at its publish");
        let narrow = 1301;
        for i in 0..(ROTATE_TARGET_ROWS as u32 + 5) {
            inserted(db, narrow, 130_101, i / 100);
        }
        publish_pending();
        assert!(active_rows(narrow) > ROTATE_TARGET_ROWS && !rotation_due(narrow), "under {ROTATE_BLOCKS} blocks");
        for i in 0..200 {
            inserted(db, narrow, 130_101, ROTATE_BLOCKS - 1 + i / 100);
        }
        publish_pending();
        assert!(rotation_due(narrow));
    }

    #[test]
    fn rows_publish_per_backend_batch_and_a_rotation_moves_them_to_sealing() {
        set_target(ROTATE_TARGET_ROWS);
        let (parent, a, b) = (1400, 140_001, 140_002);
        for i in 0..PUBLISH_ROWS - 1 {
            inserted(7, parent, a, i / 50);
        }
        assert_eq!(active_rows(parent), 0, "under {PUBLISH_ROWS} rows nothing is published");
        inserted(7, parent, a, 30);
        assert_eq!(active_rows(parent), u64::from(PUBLISH_ROWS), "one publish of the batch");
        inserted(7, parent, a, 30);
        rotated(parent, a);
        assert_eq!((active_rows(parent), sealing_rows(parent)), (0, u64::from(PUBLISH_ROWS)));
        publish_pending(); // a late row of the retired segment
        assert_eq!(sealing_rows(parent), u64::from(PUBLISH_ROWS) + 1);
        inserted(7, parent, b, 0);
        publish_pending();
        assert_eq!(active_rows(parent), 1, "the first publisher names the new segment");
        sealed_rows(parent, u64::from(PUBLISH_ROWS) + 5);
        assert_eq!(sealing_rows(parent), 0, "a seal's rows leave the count, never below 0");
    }

    #[test]
    fn rows_decide_the_cap_and_blocks_are_a_backstop() {
        set_target(ROTATE_TARGET_ROWS);
        let parent = 1500;
        set_sealing(parent, 3 * u64::from(ROTATE_BLOCKS));
        inserted(8, parent, 150_001, ROTATE_BLOCKS);
        publish_pending();
        assert!(backlog(parent) > backlog_cap_for(parent) && !over_cap(parent), "sparse: blocks past two rotations, rows under: no wait");
        rotated(parent, 150_001);
        sealed_shape(parent, 8 * u64::from(ROTATE_BLOCKS), u64::from(ROTATE_BLOCKS), 8 * u64::from(ROTATE_BLOCKS) * 2_000);
        assert_eq!(rows_cap(parent), 2 * ROTATE_TARGET_ROWS);
        move_slot(&SEALING_ROWS, parent, 2 * ROTATE_TARGET_ROWS, false);
        set_sealing(parent, 10);
        assert!(over_cap(parent), "dense: rows past two targets, blocks under: a wait");
        let narrow = 1501;
        sealed_shape(narrow, 600_000, u64::from(ROTATE_BLOCKS), 600_000 * 48);
        assert!(rows_cap(narrow) >= 1_190_000, "{}", rows_cap(narrow));
        move_slot(&SEALING_ROWS, narrow, 700_000, false);
        assert!(!over_cap(narrow), "one narrow segment sealing: no wait");
        move_slot(&SEALING_ROWS, narrow, 600_000, false);
        assert!(over_cap(narrow), "past two of them: a wait");
        let first = 1503;
        set_sealing(first, u64::from(ROTATE_BLOCKS));
        move_slot(&SEALING_ROWS, first, 600_000, false);
        assert!(rows_cap(first) >= 1_190_000 && !over_cap(first), "{}", rows_cap(first));
        let sparse = 1502;
        set_sealing(sparse, BACKSTOP_BLOCKS);
        inserted(8, sparse, 150_201, 0);
        assert!(over_cap(sparse), "past the block backstop whatever the rows");
    }

    #[test]
    fn a_sparse_seal_moves_the_rotation_size_at_most_a_quarter() {
        let parent = 1300;
        for _ in 0..4 {
            sealed_shape(parent, 110_000, 13_000, 110_000 * 1371);
        }
        let steady = rotate_blocks(parent);
        assert!((14_000..17_000).contains(&steady), "{steady}");
        sealed_shape(parent, 64_045, 15_088, 64_045 * 1371);
        let after = rotate_blocks(parent);
        assert!(after <= steady + steady / 4, "one sparse seal: {steady} -> {after}");
        assert!(after > steady, "the estimate still moves toward the sparser seal");
        for _ in 0..20 {
            sealed_shape(parent, 64_045, 15_088, 64_045 * 1371);
        }
        let sparse = rotate_blocks(parent);
        let want = rotate_blocks_for(64_045, 15_088, 64_045 * 1371, ROTATE_TARGET_ROWS, pgrc_write::writer::PartCutPolicy::default().max_bytes);
        assert!(sparse.abs_diff(want) <= want / 100, "a table that stays sparse gets there: {sparse} vs {want}");
        sealed_shape(parent, 110_000, 13_000, 110_000 * 1371);
        assert!(rotate_blocks(parent) < sparse, "a denser seal shrinks it at once (no clamp on the way down)");
    }

    #[test]
    fn the_seal_budget_admits_a_seal_alone_and_holds_the_rest() {
        let used = AtomicU64::new(0);
        assert!(try_reserve(&used, 3 << 30, 1 << 30), "a seal alone runs past the budget");
        assert!(!try_reserve(&used, 1, 1 << 30));
        used.store(0, Relaxed);
        assert!(try_reserve(&used, 600 << 20, 1 << 30));
        assert!(!try_reserve(&used, 600 << 20, 1 << 30), "two would pass it");
        assert!(try_reserve(&used, 400 << 20, 1 << 30));
        assert_eq!(used.load(Relaxed), 1000 << 20);
    }

    #[test]
    fn a_refused_reservation_leaves_the_budget_as_it_was() {
        let before = seal_mem_in_use();
        let budget = seal_mem_budget();
        let held = try_seal_mem(budget).expect("a seal alone runs");
        for _ in 0..3 {
            assert!(try_seal_mem(budget).is_none());
        }
        assert_eq!(seal_mem_in_use(), before + budget, "refused tries took nothing back");
        drop(held);
        assert_eq!(seal_mem_in_use(), before);
    }

    #[test]
    fn a_table_past_the_compile_threshold_is_queued_once() {
        use crate::compile::COMPILE_BLOCKS;
        tombstoned(4, 900, COMPILE_BLOCKS - 2);
        assert!(!take_due(4).contains(&900));
        tombstoned(4, 900, COMPILE_BLOCKS - 1);
        tombstoned(4, 900, COMPILE_BLOCKS - 1);
        inserted(4, 900, 90_001, COMPILE_BLOCKS - 1);
        assert_eq!(take_due(4), vec![900], "one entry; the tombstone block does not count as a segment block");
        assert_eq!(backlog(900), u64::from(COMPILE_BLOCKS), "only the insert's block is backlog");
        tombstoned(4, 900, COMPILE_BLOCKS);
        tombstoned(4, 901, COMPILE_BLOCKS);
        assert_eq!(take_due(4), vec![900, 901]);
    }

    fn set_target(rows: u64) {
        TARGET.with(|t| t.set(Some((guc::store::store_mutation_count(), rows))));
    }

    #[test]
    fn an_option_is_read_again_only_after_a_guc_change() {
        let c = Cell::new(None);
        let reads = Cell::new(0u32);
        let read = |v: u64| {
            reads.set(reads.get() + 1);
            v
        };
        assert_eq!(snapshot(&c, 7, || read(500)), 500);
        for _ in 0..1000 {
            assert_eq!(snapshot(&c, 7, || read(9)), 500, "a clean statement: the cell");
        }
        assert_eq!(reads.get(), 1);
        assert_eq!(snapshot(&c, 8, || read(100_000)), 100_000, "a SET: read again");
        assert_eq!(snapshot(&c, 9, || read(500)), 500, "its rollback: read again");
        assert_eq!(snapshot(&c, 9, || read(1)), 500);
        assert_eq!(reads.get(), 3);
        let none: Cell<Option<(u64, Option<u64>)>> = Cell::new(None);
        assert_eq!(snapshot(&none, 3, || None), None);
        assert_eq!(snapshot(&none, 3, || Some(64)), None, "an unset option is cached as unset");
    }

    fn clear(p: Oid) {
        for s in [&ACTIVE, &ACTIVE_ROWS, &ACTIVE_SEG, &RETIRED_SEG, &SEALING_ROWS, &DENSITY, &ROTATE, &FULL_ROWS, &ROTATING, &SEAL_BPB, &STALL_ROT] {
            s[p as usize % BACKLOG_SLOTS].store(0, Relaxed);
        }
        set_sealing(p, 0);
    }

    #[test]
    fn backpressure_never_waits_on_a_rotation_it_blocks() {
        set_target(ROTATE_TARGET_ROWS);
        let p = 1789;
        for first in [8u64, 16, 32, 104, 300] {
            for live in [8u64, 32, 104, 300] {
                clear(p);
                let rows = first * u64::from(ROTATE_BLOCKS);
                sealed_shape(p, rows, u64::from(ROTATE_BLOCKS), rows * 40);
                for blocks in (64..=BACKSTOP_BLOCKS + 64).step_by(64) {
                    set_slot(&ACTIVE, p, blocks);
                    set_slot(&ACTIVE_ROWS, p, live * blocks);
                    let busy = sealing_blocks(p) > 0 || sealing_rows(p) > 0 || rotating(p);
                    assert!(
                        !over_cap(p) || rotation_due(p) || busy,
                        "first seal {first} rows a block, filling at {live}: {blocks} blocks, {} rows, over the cap {} with nothing due or in flight",
                        live * blocks,
                        rows_cap(p)
                    );
                }
            }
        }
        clear(p);
    }

    #[test]
    fn sixteen_stalled_inserters_start_one_rotation() {
        let p = 1790;
        end_rotation(p);
        let gate = std::sync::Arc::new(std::sync::Barrier::new(16));
        let claims: Vec<_> = (0..16)
            .map(|_| {
                let g = gate.clone();
                std::thread::spawn(move || {
                    g.wait();
                    begin_rotation(p)
                })
            })
            .collect();
        let won = claims.into_iter().map(|h| usize::from(h.join().unwrap())).sum::<usize>();
        assert_eq!(won, 1, "one claim of 16");
        assert!(rotating(p) && !begin_rotation(p));
        end_rotation(p);
    }

    #[test]
    fn a_stall_rotates_below_the_base_size_and_is_counted() {
        set_target(ROTATE_TARGET_ROWS);
        let p = 1791;
        clear(p);
        let rows = 8 * u64::from(ROTATE_BLOCKS);
        sealed_shape(p, rows, u64::from(ROTATE_BLOCKS), rows * 40);
        set_slot(&ACTIVE, p, 200);
        set_slot(&ACTIVE_ROWS, p, 2 * rows_cap(p));
        assert!(over_cap(p) && !full_due(p) && stall_due(p) && !in_flight(p));
        assert_eq!(rotate_min(p), 1);
        let before = stall_rotations(p);
        rotated(p, 0);
        assert_eq!(stall_rotations(p), before + 1, "a stall's rotation is counted");
        assert!(!stall_due(p) && in_flight(p), "its rows sealing: the wait is on that seal");
        clear(p);
        sealed_shape(p, rows, u64::from(ROTATE_BLOCKS), rows * 40);
        set_slot(&ACTIVE, p, u64::from(MAX_ROTATE_BLOCKS));
        set_slot(&ACTIVE_ROWS, p, 1000);
        assert_eq!(rotate_min(p), 1);
        let before = stall_rotations(p);
        rotated(p, 0);
        assert_eq!(stall_rotations(p), before);
        clear(p);
    }

    #[test]
    fn the_cap_holds_two_base_size_segments_at_the_live_rows_per_block() {
        set_target(ROTATE_TARGET_ROWS);
        let p = 1788;
        clear(p);
        let rows = 32 * u64::from(ROTATE_BLOCKS);
        sealed_shape(p, rows, u64::from(ROTATE_BLOCKS), rows * 40);
        for blocks in (512..=u64::from(ROTATE_BLOCKS)).step_by(64) {
            set_slot(&ACTIVE, p, blocks);
            set_slot(&ACTIVE_ROWS, p, 104 * blocks);
            assert!(rows_cap(p) >= 2 * 104 * u64::from(ROTATE_BLOCKS), "{blocks} blocks: cap {}", rows_cap(p));
            assert!(!over_cap(p) && !stall_due(p), "{blocks} blocks at 104 a block");
        }
        set_slot(&ACTIVE, p, 0);
        set_slot(&ACTIVE_ROWS, p, 0);
        set_sealing(p, u64::from(ROTATE_BLOCKS));
        move_slot(&SEALING_ROWS, p, 104 * u64::from(ROTATE_BLOCKS), false);
        set_slot(&ACTIVE, p, 40);
        set_slot(&ACTIVE_ROWS, p, 104 * 40);
        assert!(rows_cap(p) >= 2 * 104 * u64::from(ROTATE_BLOCKS) && !over_cap(p), "cap {}", rows_cap(p));
        clear(p);
        sealed_shape(p, rows, u64::from(ROTATE_BLOCKS), rows * 40);
        set_slot(&ACTIVE, p, 100);
        set_slot(&ACTIVE_ROWS, p, 300 * 100);
        assert_eq!(rows_cap(p), 2 * ROTATE_TARGET_ROWS.max(rows));
        clear(p);
    }

    #[test]
    fn the_one_load_cap_check_is_the_formula_it_replaced() {
        fn replaced(parent: Oid) -> bool {
            let d = slot_value(&DENSITY, parent).filter(|d| *d > 0).unwrap_or_else(|| {
                let blocks = sealing_blocks(parent) + active_blocks(parent);
                if blocks == 0 { 0 } else { ((sealing_rows(parent) + active_rows(parent)) << 8) / blocks }
            });
            let ub = sealing_blocks(parent) + active_blocks(parent);
            let live = if ub >= LIVE_MIN_BLOCKS { ((sealing_rows(parent) + active_rows(parent)) << 8) / ub } else { 0 };
            let base = d.max(live) * u64::from(ROTATE_BLOCKS) >> 8;
            let cap = 2 * rows_target(parent).max(base);
            sealing_rows(parent) + active_rows(parent) + pending_rows(parent) > cap || backlog(parent) > BACKSTOP_BLOCKS
        }
        set_target(ROTATE_TARGET_ROWS);
        let p = 1796;
        let (mut checked, mut over) = (0, 0);
        for sb in [0, 100, u64::from(ROTATE_BLOCKS), 40_000, 70_000] {
            for ab in [0, 255, 300, u64::from(ROTATE_BLOCKS)] {
                for rows in [0, 1000, 200_000, 600_000, 2_000_000] {
                    for density in [0, 26, 104 << 8] {
                        for full in [0, 50_000] {
                            clear(p);
                            set_sealing(p, sb);
                            set_slot(&ACTIVE, p, ab);
                            set_slot(&ACTIVE_ROWS, p, rows / 2);
                            move_slot(&SEALING_ROWS, p, rows - rows / 2, false);
                            set_slot(&DENSITY, p, density);
                            set_slot(&FULL_ROWS, p, full);
                            let o = over_cap(p);
                            assert_eq!(o, replaced(p), "sealing {sb} active {ab} blocks, {rows} rows, density {density}, full {full}");
                            assert_eq!(rows_cap(p), 2 * rows_target(p).max(base_rows(p)));
                            checked += 1;
                            over += usize::from(o);
                        }
                    }
                }
            }
        }
        clear(p);
        assert_eq!(checked, 600);
        assert!(over > 0 && over < checked, "{over} of {checked} over the cap");
    }

    static LOOKUPS: std::sync::Mutex<Vec<(std::thread::ThreadId, u64)>> = std::sync::Mutex::new(Vec::new());

    fn counted_lookup(_: &str) -> PgResult<Option<String>> {
        let me = std::thread::current().id();
        let mut l = LOOKUPS.lock().unwrap_or_else(|e| e.into_inner());
        match l.iter_mut().find(|(t, _)| *t == me) {
            Some((_, n)) => *n += 1,
            None => l.push((me, 1)),
        }
        Ok(None)
    }

    fn lookups() -> u64 {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| guc_seams::get_config_option_missing_ok::set(counted_lookup));
        let me = std::thread::current().id();
        LOOKUPS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(t, _)| *t == me).map_or(0, |(_, n)| *n)
    }

    #[test]
    fn a_rows_insert_path_looks_its_options_up_once() {
        let (db, p, seg) = (19, 1795, 179_501);
        clear(p);
        let before = lookups();
        for i in 0..10_000u32 {
            crate::xact::push(p, 3, 1, &[0u8; 32]).unwrap();
            assert!(!over_cap(p), "row {i}: under the cap");
            inserted(db, p, seg, i / 100);
        }
        let n = lookups() - before;
        assert_eq!(crate::xact::take(p, 1).iter().map(|(_, r)| r.len()).sum::<usize>(), 10_000);
        publish_pending();
        let _ = take_due(db);
        clear(p);
        assert!(n <= 4, "{n} option lookups by name for 10,000 rows (one or more a row)");
    }
}
