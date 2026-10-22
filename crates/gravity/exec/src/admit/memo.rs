//! The plan memo: one probe per execution answers served / refused / unknown
//! for a plan key (docs/gravity/exec.md §Plan memo). A fixed set-associative
//! table sized for a session's live plans; entries die with the plan handle.

use gravity_program::{Generation, Handle, PlanKey};

/// Keys the memo holds: a capacity bound, not a tuned threshold. Past it a set's oldest key is replaced and admitted
/// again on its next execution (one admission's cost, never a wrong answer); sized well past the live plans of an OLTP
/// backend (a few hundred) so a steady live set never cycles (exec.md §Plan memo).
pub const CAPACITY: usize = 4096;
/// Associativity of the memo: a set holds this many keys before its oldest one is replaced (a replaced key is
/// admitted again, never answered wrong). A capacity bound, not a tuned threshold.
pub const WAYS: usize = 16;
const SETS: usize = CAPACITY / WAYS;

const EMPTY: u16 = u16::MAX;
const REFUSED: u16 = u16::MAX - 1;
const ENTRY_REFUSED: u16 = u16::MAX - 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Refused,
    /// The executor entry's refusal (its crossing is narrower than bind's: `SHAPE_PARAMS` words), which bind does not honor.
    EntryRefused,
    Served(u16),
}

#[derive(Clone, Copy)]
struct Slot {
    handle: u64,
    generation: u32,
    stamp: u16,
    served: u16,
}

const VACANT: Slot = Slot { handle: 0, generation: 0, stamp: 0, served: EMPTY };

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoStats {
    pub probes: u64,
    pub misses: u64,
    pub evictions: u64,
}

pub struct PlanMemo {
    slots: Box<[Slot]>,
    clock: u16,
    stats: MemoStats,
}

/// The observed group count per plan key, process-wide (partitioned-record.md §3.12 item 5):
/// a fresh backend's first execution sizes its partitions and bypasses its phase-1
/// table from it; a stale count costs a split, never a wrong answer.
static GROUPS: pgsync::Mutex<Option<std::collections::HashMap<(u64, u32), (u64, u64)>>> = pgsync::Mutex::new(None);
/// Plan keys whose group counts are remembered: past this the map starts over (a forgotten count costs one estimate, never a
/// wrong answer). A capacity bound, not a tuned threshold: the keys the memo itself holds.
const GROUPS_CAP: usize = CAPACITY;

/// `(phase-1 records, groups)` sizing feedback, controlled by `pgrust.gravity_grant_feedback`.
pub fn groups_for(key: PlanKey) -> Option<(u64, u64)> {
    use gravity_hosts::ops::scan::pgrc::cond_cache::reuse::{note, Kind};
    if !crate::inputs::knobs::grant::feedback_enabled() {
        return None;
    }
    let g = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    let r = g.as_ref()?.get(&(key.handle.0, key.generation.0)).copied();
    note(Kind::Feedback, r.is_some() as u64);
    r
}

pub fn groups_remember(key: PlanKey, groups: (u64, u64)) {
    if !crate::inputs::knobs::grant::feedback_enabled() {
        return;
    }
    let mut g = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(std::collections::HashMap::new); // alloc: Cold
    if m.len() >= GROUPS_CAP && !m.contains_key(&(key.handle.0, key.generation.0)) {
        m.clear();
    }
    m.insert((key.handle.0, key.generation.0), groups); // alloc: Cold
}

impl Slot {
    fn is(&self, key: PlanKey) -> bool {
        self.served != EMPTY && self.handle == key.handle.0 && self.generation == key.generation.0
    }

    fn key(&self) -> PlanKey {
        PlanKey { handle: Handle(self.handle), generation: Generation(self.generation) }
    }

    fn verdict(&self) -> Verdict {
        if self.served == REFUSED {
            Verdict::Refused
        } else if self.served == ENTRY_REFUSED {
            Verdict::EntryRefused
        } else {
            Verdict::Served(self.served)
        }
    }
}

/// The set (0..CAPACITY/WAYS) a key hashes to.
pub fn set_index(key: PlanKey) -> usize {
    set_of(key, SETS)
}

/// The set (0..sets) a key hashes to in a memo of `sets` sets (`PlanMemo::with_sets`).
fn set_of(key: PlanKey, sets: usize) -> usize {
    let mut h = key.handle.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (key.generation.0 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    ((h as u128 * sets as u128) >> 64) as usize
}

impl Default for PlanMemo {
    fn default() -> PlanMemo {
        PlanMemo::new()
    }
}

impl PlanMemo {
    pub fn new() -> PlanMemo {
        PlanMemo::with_sets(SETS)
    }

    /// D3465: a memo of `sets` sets of `WAYS` keys (`new`: `CAPACITY` keys); a deep statement level's holds one set
    /// (lib.rs `Backend::new_deep`). Fewer sets replace a key sooner: one admission again, never a wrong answer.
    pub fn with_sets(sets: usize) -> PlanMemo {
        PlanMemo { slots: vec![VACANT; sets.clamp(1, SETS) * WAYS].into_boxed_slice(), clock: 0, stats: MemoStats::default() } // alloc: Cold
    }

    fn set(&mut self, key: PlanKey) -> &mut [Slot] {
        let base = set_of(key, self.slots.len() / WAYS) * WAYS;
        &mut self.slots[base..base + WAYS]
    }

    /// The memoized verdict for `key`, if any; a hit refreshes the entry's age.
    pub fn probe(&mut self, key: PlanKey) -> Option<Verdict> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        self.stats.probes += 1;
        for s in self.set(key) {
            if s.is(key) {
                s.stamp = clock;
                return Some(s.verdict());
            }
        }
        self.stats.misses += 1;
        None
    }

    /// The verdict for `key`, not counted as a probe and its age left as it was (a filing's look, D4081).
    pub fn peek(&self, key: PlanKey) -> Option<Verdict> {
        let base = set_of(key, self.slots.len() / WAYS) * WAYS;
        self.slots[base..base + WAYS].iter().find(|s| s.is(key)).map(Slot::verdict)
    }

    /// Records `verdict` for `key`; returns the entry it displaced (the LRU of
    /// a full set), whose served plan the caller must release.
    pub fn insert(&mut self, key: PlanKey, verdict: Verdict) -> Option<(PlanKey, Verdict)> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let set = self.set(key);
        let mut victim = 0;
        let mut oldest = 0u16;
        for (i, s) in set.iter().enumerate() {
            if s.served == EMPTY || s.is(key) {
                victim = i;
                oldest = u16::MAX;
                break;
            }
            let age = clock.wrapping_sub(s.stamp);
            if age >= oldest {
                victim = i;
                oldest = age;
            }
        }
        let s = &mut set[victim];
        let displaced = (s.served != EMPTY && !s.is(key)).then(|| (s.key(), s.verdict()));
        let served = match verdict {
            Verdict::Refused => REFUSED,
            Verdict::EntryRefused => ENTRY_REFUSED,
            Verdict::Served(i) => i.min(ENTRY_REFUSED - 1),
        };
        *s = Slot { handle: key.handle.0, generation: key.generation.0, stamp: clock, served };
        if displaced.is_some() {
            self.stats.evictions += 1;
        }
        displaced
    }

    pub fn remove(&mut self, key: PlanKey) -> Option<Verdict> {
        for s in self.set(key) {
            if s.is(key) {
                let v = s.verdict();
                *s = VACANT;
                return Some(v);
            }
        }
        None
    }

    /// C3 S4c: forgets every refusal (`Refused`, `EntryRefused`), keeping the served verdicts (`Backend::follow_refusals`).
    pub fn drop_refusals(&mut self) -> usize {
        let mut dropped = 0;
        for s in self.slots.iter_mut() {
            if s.served == REFUSED || s.served == ENTRY_REFUSED {
                *s = VACANT;
                dropped += 1;
            }
        }
        dropped
    }

    /// Forgets every statement of a plan handle (the `drop_for` channel).
    pub fn drop_for(&mut self, handle: Handle) -> usize {
        let mut dropped = 0;
        for s in self.slots.iter_mut() {
            if s.served != EMPTY && s.handle == handle.0 {
                *s = VACANT;
                dropped += 1;
            }
        }
        dropped
    }

    pub fn clear(&mut self) {
        self.slots.fill(VACANT);
    }

    /// D3881: the bytes of the memo's slots.
    pub fn slot_bytes(&self) -> usize {
        core::mem::size_of_val(&*self.slots)
    }

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.served != EMPTY).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> MemoStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::{groups_for, groups_remember};
    use crate::inputs::knobs::grant::{set_feedback_for_test, Feedback};
    use gravity_program::{Generation, Handle, PlanKey};
    use guc_tables::ConditionCacheMode;

    #[test]
    fn group_sizing_feedback_obeys_its_setting_in_every_condition_cache_mode() {
        for mode in [ConditionCacheMode::Off, ConditionCacheMode::Granule, ConditionCacheMode::Rows, ConditionCacheMode::Full] {
            guc_tables::backing::set_pgrust_condition_cache(mode as i32);
            let key = PlanKey { handle: Handle(0x0b5e_12ed_6a0f + mode as u64), generation: Generation(3) };
            set_feedback_for_test(Feedback::Off);
            groups_remember(key, (100, 10));
            assert_eq!(groups_for(key), None);
            set_feedback_for_test(Feedback::Memory);
            assert_eq!(groups_for(key), None, "disabled feedback was not stored");
            groups_remember(key, (100, 10));
            assert_eq!(groups_for(key), Some((100, 10)));
            set_feedback_for_test(Feedback::Off);
            assert_eq!(groups_for(key), None, "disabled feedback was not read");
            groups_remember(key, (200, 20));
            set_feedback_for_test(Feedback::On);
            assert_eq!(groups_for(key), Some((100, 10)), "disabled feedback did not overwrite history");
            groups_remember(key, (300, 30));
            assert_eq!(groups_for(key), Some((300, 30)));
        }
        set_feedback_for_test(Feedback::Memory);
        guc_tables::backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
    }
}
