//! D4795 (1): a leader's draw past G is charged on its statement's governor count (`Reservation::note_grown`) and given
//! back with its hold, so a statement under its memory limit that draws past G, gives it back and draws again is not
//! refused, and its count is the reservation again after the give-back.

use std::time::Duration;

use gravity_driver::{Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

#[test]
fn a_leaders_draw_past_g_given_back_leaves_the_governor_count() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    // room for the reservation and one draw past it, not two
    mcx::governor::statement_start((384 * MIB) as usize, true);
    let led = mcx::governor::current().expect("the statement's ledger");
    let r = POOL.admit_need_until(Grant::of(64 * MIB).with_grow(true), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    let at_admit = led.bytes();
    let first = r.hold_growing(256 * MIB).expect("the first draw past G fits the limit");
    let during = led.bytes();
    drop(first);
    let after = led.bytes();
    let second = r.hold_growing(256 * MIB);
    let refused = second.is_none();
    drop(second);
    drop(r);
    mcx::governor::statement_end();
    POOL.arm(0);
    assert!((during - at_admit - (256 * MIB) as isize).unsigned_abs() < MIB as usize, "the draw past G is on the statement's count: {during} vs {at_admit}");
    assert!((after - at_admit).unsigned_abs() < MIB as usize, "and leaves it with its hold: {after} vs {at_admit} at the admission");
    assert!(!refused, "a second draw past G after the first was given back fits the limit");
}
