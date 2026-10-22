//! MECHCORPUS (main 09-29): the mechanisms-used record (src/mechs_used.rs) over the plan fixtures: a kind whose
//! maker hands its node to another kind's form is considered and must not appear; every fixture v4 lowers records the
//! kinds that made it; the record (on or off) never changes the program key.
use gravity_lowerv4_it as common;

use std::path::PathBuf;

use gravity_lower::{assemble, compile, Backend};
use gravity_lowerv4::face::kinds::{KindId, KindSet};
use gravity_lowerv4::face::strategy::{Strategy, StrategySet, OWNER_COUNT, set_off};
use gravity_lowerv4::mechs_used::{record, used_names, RECORDED};

fn names(k: KindSet) -> Vec<&'static str> {
    k.iter().map(|k| k.name()).collect()
}

/// A fixture: its plan file and the roads forced off while it lowers.
type Fx = (PathBuf, &'static [Strategy]);

/// fieldobs-033 (`SELECT observer_id, notes, count(*) FROM sightings GROUP BY observer_id, notes ORDER BY count(*) DESC
/// LIMIT 10`) with owner.count off (tests/m3_owner.rs's forced-off twin): a count top the sort's makers hand to
/// runs.topk_groups (runs/sort.rs), so runs.sort is fetched and declined.
fn top() -> Fx {
    (common::plans_dir().join("fieldobs-033.plan"), &[OWNER_COUNT])
}

/// m6x-003 (`SELECT g, count(*) FROM tt GROUP BY g`): a hash aggregate that borrows join.inner's spill bits (keyed/spill.rs).
fn hash() -> Fx {
    (common::milestone_dir(6).join("m6x-003.plan"), &[])
}

fn lowered((p, off): &Fx) -> common::LoweredIn {
    let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let mut set = StrategySet::EMPTY;
    off.iter().for_each(|&r| set.add(r));
    set_off(set);
    let l = common::lower_fixture(&text);
    set_off(StrategySet::EMPTY);
    l.unwrap_or_else(|| panic!("{}: no graph", p.display())).unwrap_or_else(|r| panic!("{}: refused: {r}", p.display()))
}

fn kinds(fx: &Fx) -> KindSet {
    record(true);
    KindSet(lowered(fx).descs.used)
}

/// One fixture's program, lowered with the record switched `on` or off.
fn program(fx: &Fx, on: bool) -> gravity_program::Program {
    record(on);
    let l = lowered(fx);
    let codes = compile(&l, Backend::Interp).unwrap_or_else(|e| panic!("{}: backend: {}", fx.0.display(), e.message()));
    assemble(&l, codes)
}

#[test]
fn a_sort_handed_to_the_group_top_is_not_recorded() {
    let k = kinds(&top());
    assert!(k.has(KindId::RunsTopkGroups), "the count top records {:?}", names(k));
    assert!(!k.has(KindId::RunsSort), "the count top records runs.sort, whose makers handed the node over: {:?}", names(k));
}

/// A hash aggregate borrows join.inner's spill bits (keyed/spill.rs): a helper, not a join.
#[test]
fn a_borrowed_helper_kind_is_not_recorded() {
    let k = kinds(&hash());
    assert!(!k.has(KindId::JoinInner), "m6x-003 has no join but records join.inner: {:?}", names(k));
}

/// main's condition (1): the record never enters the content key; the same unit with the line on and off gets one program
/// key (and the record is off by default: exec switches it on under PGRUST_GRAVITY_REPORT only).
#[test]
fn the_record_on_or_off_leaves_the_program_key() {
    for fx in [top(), hash()] {
        let (on, off) = (program(&fx, true), program(&fx, false));
        let name = fx.0.display();
        assert!(on.used != 0 && off.used == 0, "{name}: record on {:#x}, off {:#x}", on.used, off.used);
        assert_eq!(on.key, off.key, "{name}: the mechanisms record changed the program key");
    }
}

/// main's program-cache condition: programs are cached process-wide, so a program lowered with the record off (a backend
/// without the report) can serve a backend whose line prints; its line says `kinds=?` (unknown), never `-` (no kind),
/// which the corpus would score "did not run". The cache keeps the first program of a key, so a later lowering with the
/// record on does not replace it.
#[test]
fn a_program_lowered_off_and_served_from_the_cache_prints_unknown() {
    let off = std::sync::Arc::new(program(&top(), false));
    let mut cache = gravity_program::ProgramCache::new(64 << 20, 16);
    cache.insert(None, off.clone());
    record(true);
    let hit = cache.l2(off.key).expect("the program is cached under its content key");
    assert_eq!(used_names(hit.used), "?", "a program lowered with the record off");
    let on = std::sync::Arc::new(program(&top(), true));
    assert_eq!(on.key, off.key);
    assert!(used_names(on.used).split(',').any(|k| k == "runs.topk_groups"), "{}", used_names(on.used));
    cache.insert(None, on);
    assert_eq!(used_names(cache.l2(off.key).expect("cached").used), "?", "the cache kept the first program");
}

/// Every fixture v4 lowers records at least one kind: the line names what made each served program.
#[test]
fn every_lowered_fixture_records_its_kinds() {
    record(true);
    let (mut n, mut bad) = (0, Vec::new());
    for m in 1..=11 {
        let dir = common::milestone_dir(m);
        if !dir.is_dir() {
            continue;
        }
        for (name, text) in common::files(&dir, "plan") {
            let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
            n += 1;
            if l.descs.used & !RECORDED == 0 {
                bad.push(name);
            }
        }
    }
    assert!(n > 0 && bad.is_empty(), "{} of {n} lowered fixtures record no kind: {bad:?}", bad.len());
}

/// D4352: the record is off in every backend without PGRUST_GRAVITY_REPORT (production, the stress kits), so `used` is 0
/// there; the program is still a v4 lowering's (`Program::lowered`), so the driver gives its spilling breakers' regions
/// their spill road (`SlotRegion::spills`, `WorkerSlot::share_spills`), which revoke's spillable bytes and C10's debt rows
/// read. On trunk 9cfd04d93b2 a program lowered with the record off read as a hand program: no region had a spill road,
/// and the 8 GiB ladder rehearsal saw 'revocable 0 MB' at every episode (R4a / R4b never fired).
#[test]
fn d4352_a_program_lowered_with_the_record_off_is_lowered() {
    for fx in [top(), hash()] {
        let (on, off) = (program(&fx, true), program(&fx, false));
        let name = fx.0.display();
        assert!(on.lowered(), "{name}: lowered with the record on");
        assert!(off.lowered(), "{name}: lowered with the record off (used {:#x}) reads as a hand program", off.used);
        assert!(off.pipelines.iter().any(|p| p.holds().is_some_and(|h| h.spills())), "{name}: a breaker with a spill road");
    }
}
