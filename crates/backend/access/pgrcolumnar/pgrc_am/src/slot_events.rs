//! The catalog's slot-event listeners (C1 L1; plan.md §3's contract for C1,
//! G1 and K1). `catalog.rs` fires an event after it drops its table lock,
//! from `create`, `remove_dir`, `remove_under` (per slot), `publish`,
//! `retire_below`, `redo_publish` (and `redo_truncate` through `remove_dir` +
//! `create`) and `invalidate_relid`. Listeners are process-global and
//! append-only; a listener must not assume it runs under any catalog lock,
//! and may call back into the catalog.

use crate::catalog::RelSlot;
use types_core::Oid;

/// One catalog change. `&'a str` is the slot's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotEvent<'a> {
    /// A directory entry was created (first ingest, or TRUNCATE's empty entry).
    Created(RelSlot, &'a str),
    /// A generation `gen` became the head (publish or its redo).
    Published(RelSlot, u64),
    /// The directory entry is gone (DROP / TRUNCATE / rewrite / aborted creation / DROP DATABASE).
    Removed(RelSlot, &'a str),
    /// A relcache invalidation of `relid` (every relid: parents and companions; 0 = all).
    Relid(Oid),
    /// Generations below the given one left the table.
    Retired(RelSlot, u64),
}

pub type SlotListener = fn(&SlotEvent<'_>);

pgsync::process_global! {
    static LISTENERS: pgsync::Mutex<Vec<SlotListener>> = pgsync::Mutex::new(Vec::new());
    static COUNT: pgsync::atomic::AtomicUsize = pgsync::atomic::AtomicUsize::new(0);
}

/// Append a listener (process-global; never removed).
pub fn on_slot_event(f: SlotListener) {
    let mut l = pgsync::lock(&LISTENERS);
    l.push(f);
    COUNT.store(l.len(), pgsync::atomic::Ordering::Release);
}

/// Deliver `ev` to every listener. One atomic load when none is installed.
pub fn fire(ev: &SlotEvent<'_>) {
    if COUNT.load(pgsync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let ls: Vec<SlotListener> = pgsync::lock(&LISTENERS).clone(); // alloc: Cold
    for f in ls {
        f(ev);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(ev: &SlotEvent<'_>) {
        SEEN.lock().unwrap().push(format!("{ev:?}"));
    }

    #[test]
    fn catalog_changes_fire_events() {
        on_slot_event(record);
        let slot = (1663, 5, 991_001);
        let dir = "/tmp/pgrc-slot-events-test";
        crate::catalog::create(slot, dir);
        crate::catalog::retire_below(slot, 3);
        crate::catalog::invalidate_relid(4242);
        crate::catalog::remove_dir(dir);
        let seen = SEEN.lock().unwrap().clone();
        let mine: Vec<&String> = seen.iter().filter(|s| s.contains("991001") || s.contains("4242")).collect();
        assert_eq!(
            mine,
            vec![
                &format!("Created({slot:?}, {dir:?})"),
                &format!("Retired({slot:?}, 3)"),
                &"Relid(4242)".to_string(),
                &format!("Removed({slot:?}, {dir:?})"),
            ]
        );
    }
}
