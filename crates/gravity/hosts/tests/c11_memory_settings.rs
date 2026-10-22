//! S1 criterion 11 (everything visible in SQL): `pgrust_memory_settings()`'s rows (membroker/settings.rs). Every memory
//! policy knob is one row by construction, with its value now, its default and where the value came from; the memory
//! switches the environment sets read back through the code's own readers. A process of its own: the knobs and the
//! depot-fetch switch are process-wide.

use gravity_hosts::membroker::settings::{rows, Row, Val};
use mempolicy::consts::{apply, reset, Source, KNOBS, TEST_LOCK};

fn all() -> Vec<Row> {
    let mut out = Vec::new();
    rows(&mut |r| out.push(r));
    out
}

fn row<'a>(rows: &'a [Row], kind: &str, name: &str) -> &'a Row {
    rows.iter().find(|r| r.kind == kind && r.name == name).unwrap_or_else(|| panic!("no {kind} row {name}"))
}

#[test]
fn every_knob_is_one_row_with_its_value_default_and_source() {
    let _one = pgsync::lock(&TEST_LOCK);
    reset();
    let before = all();
    let knobs: Vec<&Row> = before.iter().filter(|r| r.kind == "knob").collect();
    assert_eq!(knobs.len(), KNOBS.len() + 1, "every PGRUST_MEMPOLICY key and trace");
    for (k, r) in KNOBS.iter().zip(&knobs) {
        assert_eq!((r.name, r.value, r.default, r.source, r.set_by), (k.name, Val::N(k.default), Val::N(k.default), "default", "PGRUST_MEMPOLICY"));
        assert!(!r.doc.is_empty(), "{}", r.name);
    }
    assert_eq!(knobs.last().map(|r| (r.name, r.value)), Some(("trace", Val::N(0))));
    assert_eq!(apply("cache_floor=1,ladder_log_ms=500"), Ok(mempolicy::consts::Applied { knobs: 2, trace: false }));
    let after = all();
    let floor = row(&after, "knob", "cache_floor");
    assert_eq!((floor.value, floor.default, floor.source), (Val::N(1), Val::N(0), Source::Env.name()));
    let log = row(&after, "knob", "ladder_log_ms");
    assert_eq!((log.value, log.default, log.source), (Val::N(500), Val::N(1000), "PGRUST_MEMPOLICY"));
    mempolicy::consts::set(mempolicy::consts::K::CacheFloor, 0);
    assert_eq!(row(&all(), "knob", "cache_floor").source, "set");
    reset();
    assert_eq!(row(&all(), "knob", "cache_floor").source, "default");
}

#[test]
fn the_memory_switches_read_back_through_their_readers_with_their_source() {
    let _one = pgsync::lock(&TEST_LOCK);
    let rows = all();
    let env: Vec<&Row> = rows.iter().filter(|r| r.kind == "env").collect();
    let names: Vec<&str> = env.iter().map(|r| r.name).collect();
    for n in ["depot_fetch", "cache_purge_now", "query_purge_bg", "reuse_before_release", "purge_pressure_only", "purge_pressure_bg", "make_room_log", "allocator", "pool_mcx", "pool_gravity"] {
        assert!(names.contains(&n), "no env row {n}: {names:?}");
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "each switch once");
    for r in &env {
        assert!(r.set_by.starts_with("PGRUST_") && !r.doc.is_empty(), "{r:?}");
        assert!(matches!(r.source, "default" | "environment" | "set"), "{r:?}");
    }
    // D4709's off lever, set by the process: the value the server reads, its source 'set'
    let on = gravity_driver::slots::depot_fetch();
    assert_eq!(row(&rows, "env", "depot_fetch").value, Val::N(u64::from(on)));
    gravity_driver::slots::set_depot_fetch(false);
    let fetch = *row(&all(), "env", "depot_fetch");
    assert_eq!((fetch.value, fetch.default, fetch.source, fetch.set_by), (Val::N(0), Val::N(1), "set", "PGRUST_GRAVITY_DEPOT_FETCH"));
    gravity_driver::slots::set_depot_fetch(on);
    let alloc = *row(&all(), "env", "allocator");
    assert!(matches!(alloc.value, Val::S("hearth" | "mimalloc")), "{alloc:?}");
}
