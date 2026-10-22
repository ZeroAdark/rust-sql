//! Re-admission against the bank a served plan runs on: the resource bounds read the relation's rows, which a later generation moves, and the plan
//! memo re-serves a verdict without them (a pgrc publish invalidates nothing). A refusal here is an admission refusal, never a mid-run `UNSUPPORTED_SCRATCH`.

use core::cell::RefCell;

use gravity_program::ContentKey;
use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_storage::lock::NoLock;

use crate::admit::{self, Refused};
use crate::runner;
use crate::shapes::Admitted;
use crate::Backend;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub key: ContentKey,
    pub rel: Oid,
    pub rows: u64,
}

pub const STAMPS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Growth {
    Same,
    Grown { then: u64, now: u64 },
}

leader_tls::leader_tls! {
    thread_local! {
        static TABLE: RefCell<Vec<Stamp>> = const { RefCell::new(Vec::new()) }; // alloc: Cold
    }
}

/// The growth of `s.rel` since the baseline filed under `s.key`; a first sight files `s` as the baseline.
pub fn note(table: &mut Vec<Stamp>, s: Stamp) -> Growth {
    if let Some(b) = table.iter().find(|b| b.key == s.key && b.rel == s.rel) {
        return if s.rows > b.rows { Growth::Grown { then: b.rows, now: s.rows } } else { Growth::Same };
    }
    if table.len() >= STAMPS {
        table.clear();
    }
    table.push(s);
    Growth::Same
}

pub fn scaled(predicted: u64, then: u64, now: u64) -> u64 {
    if then == 0 || now <= then {
        return predicted;
    }
    ((predicted as u128 * now as u128) / then as u128).min(u64::MAX as u128) as u64
}

pub fn overflows(predicted: u64, then: u64, now: u64, room: u64) -> bool {
    scaled(predicted, then, now) > room
}

/// The build bounds, at an admission and at a memo hit: the plan's build estimate scaled by the build relation's growth since
/// the first admission.
pub fn check_build(a: &Admitted, mcx: Mcx<'static>) -> PgResult<Option<Refused>> {
    if let Some(r) = runner::build_would_spill(a) {
        return Ok(Some(r));
    }
    // D3355 FIX 2b: a v4 build is never refused by its estimate, scaled by its relation's growth or not
    // (`runner::build_would_spill`): only a build that really passes one region meets the named 53200 at run time
    // D4600 (`admit::lean`): a v4 program is never bounded here, known without copying its first pipeline
    if admit::lean::on() && a.program.v4.is_some() {
        return Ok(None);
    }
    let Some(p0) = runner::lowered_build(a).filter(|_| a.program.v4.is_none()) else { return Ok(None) };
    if a.program.pipelines.get(1).and_then(|p| p.breaker()).is_some_and(|b| matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. })) || p0.driving_site < 0 {
        return Ok(None);
    }
    let Some(r) = a.registry.granule_recipe(p0.driving_site as usize) else { return Ok(None) };
    let rel = table::table_open(mcx, r.rel, NoLock)?;
    let list = gravity_hosts::ops::scan::pgrc::granule_source::resolve(&rel, None);
    rel.close(NoLock)?;
    let key = a.graph.as_ref().map_or_else(|| a.program.front_key(), |g| g.key);
    // KEY-PREDICTED-ROWS road (b): the content key holds no estimate, so the baseline is filed per (key, build estimate): a
    // new estimate is planned against the rows now and starts its own (the key's exact rows did this before road (b))
    let key = ContentKey(key.0 ^ (u128::from(p0.build_estimate()) << 64 | u128::from(p0.predicted_rows)).wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c6a1_2f4e_d5b3));
    let stamp = Stamp { key, rel: r.rel, rows: list?.rows() };
    let Growth::Grown { then, now } = TABLE.with(|t| note(&mut t.borrow_mut(), stamp)) else { return Ok(None) };
    let width = if a.is_parallel() && !admit::leader_only() { runner::granted_width() } else { 1 };
    let share = gravity_driver::share(&a.program, 0, runner::granted_memory(width), 1).breaker.bytes;
    Ok(overflows(p0.predicted_rows, then, now, runner::build_room(share)).then(|| Refused { stage: "join", what: format!("a build whose relation grew from {then} to {now} rows since the plan: the estimate scaled to {} passes the hash grant (need 6)", scaled(p0.predicted_rows, then, now)) })) // alloc: Cold
}

/// A memo hit's verdict against the bank now: `true` when a bound refuses it, so the caller re-admits (and refuses by name).
pub fn stale(b: &mut Backend, idx: usize, key: gravity_program::PlanKey) -> PgResult<bool> {
    let Some(a) = b.admitted(idx, key) else { return Ok(true) };
    if runner::lowered_build(a).is_none() {
        return Ok(false);
    }
    let ctx = gravity_driver::mem::root::Root::new("GravityRegen");
    let mcx: Mcx<'static> = super::raw::regen_mcx(&ctx);
    Ok(check_build(a, mcx)?.is_some())
}
