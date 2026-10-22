//! The readings of the hot `pgrust.*` placeholder options (guc_seams `Knob`): each knob's option is read once per change of
//! this thread's store and its reading kept, so a per-statement read is two thread-local loads instead of a by-name
//! GetConfigOption (a store borrow, a name hash and a String each).

use core::cell::Cell;

use guc_seams::{Knob, KnobRead};

use crate::store;

/// No reading yet: a mutation count the store never reaches (it starts at 0 and only counts up).
const NONE_YET: (u64, KnobRead) = (u64::MAX, KnobRead::Unset);

thread_local! {
    // Each knob's reading and the store mutation count it was read at: good while the count stands still (every write
    // to this thread's registry, a SET, RESET, transaction-end restore or a new registry, moves it first).
    static KNOB_READS: [Cell<(u64, KnobRead)>; Knob::COUNT] = const { [const { Cell::new(NONE_YET) }; Knob::COUNT] };
}

/// `knob.read(GetConfigOption(knob.name(), true, false))` on this thread, from the kept reading while the store has not
/// changed since it was read.
pub fn knob_read(knob: Knob) -> KnobRead {
    let at = store::store_mutation_count();
    KNOB_READS.with(|reads| {
        let slot = &reads[knob as usize];
        let (read_at, read) = slot.get();
        if read_at == at {
            return read;
        }
        let (read, keep) = resolve(knob);
        if keep {
            slot.set((at, read));
        }
        read
    })
}

/// The knob's reading now, and whether the store's mutation count keys it: an absent option or a placeholder (whose value
/// lives in the store alone). A defined option with a backing variable is read each time, as a claim's carry writes
/// backings without a store write.
#[cold]
#[inline(never)]
fn resolve(knob: Knob) -> (KnobRead, bool) {
    store::with_store(|reg| match reg.find_option(knob.name()) {
        None => (knob.read(None), true),
        Some(record) => {
            let placeholder = record.gen().flags & types_guc::GUC_CUSTOM_PLACEHOLDER != 0;
            (knob.read(Some(&crate::registry::raw_config_value(record))), placeholder)
        }
    })
    .expect("GUC store not initialized")
}
