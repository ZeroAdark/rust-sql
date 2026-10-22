//! The re-admission stamps (src/admit_regen.rs): the growth case — admitted under the room, the relation grows past it, the memo hit refuses by name.

use gravity_exec::admit::regen::{note, overflows, scaled, Growth, Stamp, STAMPS};
use gravity_program::ContentKey;

fn stamp(key: u128, rel: u32, rows: u64) -> Stamp {
    Stamp { key: ContentKey(key), rel, rows }
}

#[test]
fn the_first_sight_files_the_baseline_and_growth_is_measured_against_it() {
    let mut t = Vec::new();
    assert_eq!(note(&mut t, stamp(1, 10, 1000)), Growth::Same);
    assert_eq!(note(&mut t, stamp(1, 10, 1000)), Growth::Same);
    assert_eq!(note(&mut t, stamp(1, 10, 4000)), Growth::Grown { then: 1000, now: 4000 });
    assert_eq!(note(&mut t, stamp(1, 10, 8000)), Growth::Grown { then: 1000, now: 8000 }, "measured against the first admission, not the last hit");
    assert_eq!(note(&mut t, stamp(1, 10, 500)), Growth::Same, "a shrink is not growth");
    assert_eq!(note(&mut t, stamp(2, 10, 8000)), Growth::Same, "another program keeps its own baseline");
    assert_eq!(note(&mut t, stamp(1, 11, 8000)), Growth::Same, "another relation keeps its own baseline");
    assert_eq!(note(&mut t, stamp(2, 10, 9000)), Growth::Grown { then: 8000, now: 9000 });
    assert_eq!(t.len(), 3);
}

#[test]
fn a_full_table_is_cleared_and_refiles_at_the_rows_now() {
    let mut t = Vec::new();
    for k in 0..STAMPS as u128 {
        assert_eq!(note(&mut t, stamp(k, 1, 100)), Growth::Same);
    }
    assert_eq!(t.len(), STAMPS);
    assert_eq!(note(&mut t, stamp(STAMPS as u128, 1, 100)), Growth::Same);
    assert_eq!(t.len(), 1);
    assert_eq!(note(&mut t, stamp(0, 1, 400)), Growth::Same, "the lost baseline re-files at the rows now");
    assert_eq!(note(&mut t, stamp(0, 1, 800)), Growth::Grown { then: 400, now: 800 });
}

#[test]
fn the_estimate_scales_with_the_relation_and_saturates() {
    assert_eq!(scaled(1000, 100, 400), 4000);
    assert_eq!(scaled(1000, 100, 150), 1500);
    assert_eq!(scaled(1000, 0, 400), 1000, "no baseline: the estimate stands");
    assert_eq!(scaled(1000, 400, 100), 1000, "a shrink never lowers the estimate");
    assert_eq!(scaled(u64::MAX / 2, 1, 4), u64::MAX);
}

/// A build of 100,000 estimated rows under a room of 114,688 (the default grant's, serial): the relation doubles, the scaled estimate passes the room.
#[test]
fn a_build_admitted_under_the_room_is_refused_once_its_relation_grows_past_it() {
    let room = 114_688;
    let mut t = Vec::new();
    let s = stamp(7, 42, 1_000_000);
    assert_eq!(note(&mut t, s), Growth::Same);
    assert!(!overflows(100_000, 1_000_000, 1_000_000, room), "admitted at the plan's rows");
    let Growth::Grown { then, now } = note(&mut t, stamp(7, 42, 1_100_000)) else { panic!("a publish of 10% is growth") };
    assert!(!overflows(100_000, then, now, room), "110,000 still under the room");
    let Growth::Grown { then, now } = note(&mut t, stamp(7, 42, 2_000_000)) else { panic!("growth") };
    assert!(overflows(100_000, then, now, room), "200,000 scaled rows pass the room: refused by name");
}
