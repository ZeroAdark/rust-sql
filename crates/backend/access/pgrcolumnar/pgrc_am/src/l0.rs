//! COMPACTP: each table's L0 count for its newest generations: the parts that are neither full by the compaction
//! policy's test (`pgrc_compact::PartView::full`: rows, or buffer bytes, at the bulk cut) nor a compaction output,
//! `pgrc_compact_stats`' `l0`. The bulk publish's part-count trigger and the L0 cap read it, so it reads no file:
//! a part's verdict is the generation's `FullTest` over its in-memory stats, taken once, when the part is new. A
//! publish's entry is its base's merged with its new parts (part numbers only grow: every part at or past the base's
//! `next_part_no` is new), under the table's publish lock; a generation whose base has no entry is counted whole.

use std::collections::BTreeMap;

use pgrc_format::manifest::PART_FLAG_COMPACTED;

use crate::catalog::{Generation, RelSlot};

/// Entries kept per table, newest last.
const KEEP: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    gen: u64,
    next_part_no: u32,
    l0: u32,
    /// The parts full by the policy's test that are not compaction outputs, ascending.
    full: Vec<u32>,
}

pgsync::process_global! {
    static LEDGER: pgsync::Mutex<BTreeMap<RelSlot, Vec<Entry>>> = pgsync::Mutex::new(BTreeMap::new());
    /// D4537 T18c: the bytes of LEDGER, on the part cache (the ledger is the registry's bookkeeping: an entry leaves with its
    /// table's directory), refit after each change under the map's lock.
    static LEDGER_CHARGE: pgsync::Mutex<memledger::Charge> = pgsync::Mutex::new(memledger::Charge::NONE);
}

/// The ledger's heap bytes: a tree node's links at two words, each entry's `full` list at its length.
fn ledger_bytes(map: &BTreeMap<RelSlot, Vec<Entry>>) -> usize {
    const NODE: usize = 2 * size_of::<usize>();
    map.len() * (size_of::<(RelSlot, Vec<Entry>)>() + NODE)
        + map.values().map(|v| v.capacity() * size_of::<Entry>() + v.iter().map(|e| e.full.capacity() * size_of::<u32>()).sum::<usize>()).sum::<usize>()
}

fn refit(map: &BTreeMap<RelSlot, Vec<Entry>>) {
    crate::inval::refit(&LEDGER_CHARGE, ledger_bytes(map));
}

/// `g`'s L0 count: its entry, else counted now (merged with its base's entry when there is one) and kept. `None` when
/// `g` has no stats in memory (a generation written before P-1, a refused plan, a composed view) or no full test.
pub fn of(slot: RelSlot, g: &Generation) -> Option<u32> {
    let base = {
        let l = pgsync::lock(&LEDGER);
        let e = l.get(&slot);
        if let Some(hit) = e.and_then(|v| v.iter().find(|x| x.gen == g.gen)) {
            return Some(hit.l0);
        }
        e.and_then(|v| v.iter().find(|x| x.gen == g.prev_gen)).cloned()
    };
    let entry = count(g, base.as_ref())?;
    let l0 = entry.l0;
    keep(slot, entry);
    Some(l0)
}

/// `entry` kept for `slot` (the newest `KEEP` of its generations), the ledger's bytes refit.
fn keep(slot: RelSlot, entry: Entry) {
    let mut l = pgsync::lock(&LEDGER);
    let v = l.entry(slot).or_default();
    if !v.iter().any(|x| x.gen == entry.gen) {
        v.push(entry);
        v.sort_by_key(|x| x.gen);
        if v.len() > KEEP {
            v.drain(..v.len() - KEEP);
        }
    }
    refit(&l);
}

/// The L0 count of the newest generation `slot` has an entry for (the head after this process's last publish).
pub fn newest(slot: RelSlot) -> Option<u32> {
    pgsync::lock(&LEDGER).get(&slot).and_then(|v| v.last()).map(|e| e.l0)
}

/// The table's directory went (DROP, TRUNCATE, a rewrite).
pub fn forget(slot: RelSlot) {
    let mut l = pgsync::lock(&LEDGER);
    l.remove(&slot);
    refit(&l);
}

/// Every table under a database directory that went.
pub fn forget_where(gone: impl Fn(&RelSlot) -> bool) {
    let mut l = pgsync::lock(&LEDGER);
    l.retain(|s, _| !gone(s));
    refit(&l);
}

fn count(g: &Generation, base: Option<&Entry>) -> Option<Entry> {
    let m = &g.manifest;
    let stats = g.stats.as_ref()?;
    let test = g.full_test();
    // a whole count takes the manifest walk's index once; a merge locates its few new parts
    let index = if base.is_none() && !m.parts.is_empty() { Some(stats.try_index(m).ok()?) } else { None };
    let (mut l0, mut full) = (0u32, Vec::new());
    for (k, rec) in m.parts.iter().enumerate() {
        if rec.flags & PART_FLAG_COMPACTED != 0 {
            continue;
        }
        let is_full = match base {
            Some(b) if rec.part_no < b.next_part_no => b.full.binary_search(&rec.part_no).is_ok(),
            _ => {
                let at = match &index {
                    Some(ix) => ix[k],
                    None => stats.locate(rec.part_no)?,
                };
                test.as_deref()?.full_in(stats, at, rec.rows)
            }
        };
        if is_full {
            full.push(rec.part_no);
        } else {
            l0 += 1;
        }
    }
    full.sort_unstable();
    Some(Entry { gen: g.gen, next_part_no: m.header.next_part_no, l0, full })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4537 T18c: the L0 ledger is the part cache's bytes, refit after each change and given back when a table's entry goes
    /// (read under the ledger's lock, where the refit runs, so parallel tests cannot move it between the two reads).
    #[test]
    fn the_l0_ledger_is_on_the_part_cache() {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
        let slot: RelSlot = (4_282_000_011, 1, 9_940_000_000 + u64::from(std::process::id()));
        keep(slot, Entry { gen: 1, next_part_no: 3, l0: 1, full: (0..2048).collect() });
        {
            let l = pgsync::lock(&LEDGER);
            let c = pgsync::lock(&LEDGER_CHARGE);
            assert_eq!((c.owner(), c.committed()), (OWNER, ledger_bytes(&l) as u64));
            assert!(c.committed() >= 2048 * size_of::<u32>() as u64, "the entry's full list is counted");
        }
        forget(slot);
        let l = pgsync::lock(&LEDGER);
        assert_eq!(pgsync::lock(&LEDGER_CHARGE).committed(), ledger_bytes(&l) as u64, "the forgotten table's bytes went");
    }
}
