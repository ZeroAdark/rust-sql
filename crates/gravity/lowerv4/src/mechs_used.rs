//! MECHCORPUS (main 09-29): the program's mechanisms-used record, the kinds whose makers made a mechanism the emitted
//! program holds (the report's `gravity: mechanisms kinds=..` line; exec lib.rs `note_mechs`). Every registry fetch of a
//! kind (`registry`'s `need`) counts once (`fetched`); a fetch that does not engage takes its count back: a maker that
//! hands its node to another kind's form (`declined`: runs.sort's to runs.topk_groups, fold.plain's to a keyed end), a
//! fetch that only borrows a helper (`probe`), a probe's no (`took`). A kind is recorded when its count ends above 0.
//! Roads are not recorded: a third of them engage without a fetch (expression extensions, `registry::road` lookups,
//! `env.applies` forms) and a score of fetch sites only ask; the corpus tells a road's engagement by the road-off
//! differential (the program with the road off differs), exactly. `used_scope` opens one lowering's record (frame/program.rs
//! `lower`; a lowering runs on its backend's thread, no lowerv4 / lower / decompose code spawns one) and hands it to
//! `Descs::used` -> `Program::used`, never into the content key. Compile time only: nothing runs per execution or per row.
//! Off by default: the record runs only on a thread `record(true)` switched on (exec's backend under
//! PGRUST_GRAVITY_REPORT, the one knob that prints the line); off, a fetch costs one thread-local read and a lowering none.
use core::cell::{Cell, RefCell};

use crate::face::kinds::KindSet;
use crate::admit::refusal::Variant;

/// The record's own bit (no kind has id 127): set by every lowering that ran with the record on. A program lowered with
/// the record off (0) is unknown to the line (`kinds=?`), never "no kind" (main 09-29: the program cache is process-wide,
/// so a program lowered off can serve a statement whose line prints).
pub const RECORDED: u128 = 1 << 127;

leader_tls::leader_tls! {
    thread_local! {
        /// The current lowering's counts, at `KindId as usize`.
        static USED: RefCell<[u16; 128]> = const { RefCell::new([0; 128]) };
        /// The record's switch on this thread (off by default).
        static ON: Cell<bool> = const { Cell::new(false) };
    }
}

/// Switch the record on or off for the lowerings of this thread (exec's backend: on under PGRUST_GRAVITY_REPORT).
pub fn record(on: bool) {
    ON.with(|c| c.set(on));
}

fn on() -> bool {
    ON.with(Cell::get)
}

/// A registry fetch of `v` (registry/mod.rs `need`): a kind counts engaged until a decline takes it back; a road is not
/// recorded (the module doc).
pub fn fetched(v: Variant) {
    if !on() {
        return;
    }
    if let Variant::Kind(k) = v {
        USED.with(|u| {
            let (mut u, s) = (u.borrow_mut(), k as usize);
            u[s] = u[s].saturating_add(1);
        });
    }
}

/// A fetch of `v` that did not engage (a maker that handed its node to another kind; a probe's no).
pub fn declined(v: Variant) {
    if !on() {
        return;
    }
    if let Variant::Kind(k) = v {
        USED.with(|u| {
            let (mut u, s) = (u.borrow_mut(), k as usize);
            u[s] = u[s].saturating_sub(1);
        });
    }
}

/// A probe's answer: yes (`true`, `Some`) engages its fetch; no takes it back.
pub trait Answer {
    fn yes(&self) -> bool;
}

impl Answer for bool {
    fn yes(&self) -> bool {
        *self
    }
}

impl<T> Answer for Option<T> {
    fn yes(&self) -> bool {
        self.is_some()
    }
}

/// `answer`, the result of a mechanism fetched for `v` just before (`need_kind::<T>(..).0.plan(..)`): a no declines the fetch.
/// Call it only where the fetch happened (inside the branch that fetched), or a no takes back another site's count.
pub fn took<A: Answer>(v: Variant, answer: A) -> A {
    if !answer.yes() {
        declined(v);
    }
    answer
}

/// `answer` of a fetch that only asks (the mechanism engages, if at all, at another fetch): never counted.
pub fn probe<A>(v: Variant, answer: A) -> A {
    declined(v);
    answer
}

/// `f` over a fresh record, and the record it left (`KindSet` bits; 0 with the record off). The enclosing record comes back
/// however `f` ends (a refusal unwinds through here).
pub fn used_scope<R>(f: impl FnOnce() -> R) -> (R, u128) {
    if !on() {
        return (f(), 0);
    }
    struct Back([u16; 128]);
    impl Drop for Back {
        fn drop(&mut self) {
            USED.with(|u| *u.borrow_mut() = self.0);
        }
    }
    let _back = Back(USED.with(|u| core::mem::replace(&mut *u.borrow_mut(), [0; 128])));
    let r = f();
    let used = USED.with(|u| u.borrow().iter().enumerate().filter(|c| *c.1 > 0).fold(RECORDED, |s, (i, _)| s | 1u128 << i));
    (r, used)
}

/// The report's names for a record: the kinds, comma-separated in declaration order, `-` for none, `?` for a program
/// lowered with the record off (its kinds are unknown).
pub fn used_names(used: u128) -> String {
    if used & RECORDED == 0 {
        return "?".to_string(); // alloc: Cold
    }
    let v: Vec<&'static str> = KindSet(used & !RECORDED).iter().map(|k| k.name()).collect(); // alloc: Cold
    if v.is_empty() { "-".to_string() } else { v.join(",") } // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::kinds::KindId;
    use crate::face::strategy::CODE_FRONT_DENSE;

    const SORT: Variant = Variant::Kind(KindId::RunsSort);
    const TOPK: Variant = Variant::Kind(KindId::RunsTopkGroups);
    const INNER: Variant = Variant::Kind(KindId::JoinInner);

    #[test]
    fn a_kind_that_hands_its_node_over_or_only_asks_is_not_recorded() {
        record(true);
        let ((), used) = used_scope(|| {
            // runs/sort.rs: the sort's maker hands a group sort to runs.topk_groups
            fetched(SORT);
            declined(SORT);
            fetched(TOPK);
            // keyed/spill.rs: a hash aggregate borrows join.inner's spill bits
            fetched(INNER);
            assert_eq!(probe(INNER, Some(3u8)), Some(3));
            // a probe's no, and a road (never recorded)
            fetched(Variant::Kind(KindId::KeyedTwopass));
            assert!(!took(Variant::Kind(KindId::KeyedTwopass), false));
            fetched(Variant::Strategy(CODE_FRONT_DENSE));
        });
        assert_eq!(used_names(used), "runs.topk_groups");
    }

    #[test]
    fn two_sites_count_apart_and_a_scope_restores_the_outer_record() {
        record(true);
        let ((), outer) = used_scope(|| {
            fetched(SORT);
            fetched(SORT);
            declined(SORT); // one face handed over, the other made the sort
            let ((), inner) = used_scope(|| fetched(INNER));
            assert_eq!(used_names(inner), "join.inner");
            let unwound = std::panic::catch_unwind(|| used_scope(|| {
                fetched(TOPK);
                panic!("a refusal")
            }));
            assert!(unwound.is_err());
            fetched(Variant::Kind(KindId::SrcPgrc));
        });
        assert_eq!(used_names(outer), "src.pgrc,runs.sort");
        assert_eq!(used_names(RECORDED), "-", "recorded, no kind");
        assert_eq!(used_names(0), "?", "lowered with the record off");
    }

    #[test]
    fn no_kind_takes_the_record_bit() {
        assert!(crate::face::kinds::KINDS.iter().all(|d| (d.id as u32) < 127), "a kind at id 127 would read as the record bit");
    }

    #[test]
    fn off_by_default_and_off_records_nothing() {
        let ((), used) = used_scope(|| fetched(SORT));
        assert_eq!((used, used_names(used).as_str()), (0, "?"), "a thread that never switched the record on");
        record(true);
        record(false);
        let ((), used) = used_scope(|| {
            fetched(TOPK);
            fetched(INNER);
        });
        assert_eq!(used, 0, "switched off");
    }
}
