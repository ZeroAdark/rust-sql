//! D1328: a var sort emit's stash was cut as the hold word plus one run buffer's width over `nruns + 1` shares
//! (host_abi `begin_in`), and `runmerge::lay` floors each run's buffer to 8 bytes over the rest: in 1.4 % of layouts the
//! buffer came out 8 bytes wider than the stash's room, so a record of a length in `(room, buf]` passed the run's fill and
//! `stash_row` copied it up to 8 bytes past the region's end. The stash now carries those 8 bytes
//! (`runmerge::var_stash_bytes`), so the laid buffer never passes the room; `stash_row` bounds its copy by the room and
//! raises a named Bug past it (the host_abi `d1328_stash_bound_tests`).
use gravity_kernels::ops::sort::runmerge::{fixed_bytes, var_stash_bytes, var_stash_room};

/// Every run count a 64 kB .. 4 MB region can merge, every 8-byte step of the region past its floor at small run counts:
/// the laid buffer never passes the stash's room.
#[test]
fn laid_buffer_never_passes_the_stash_room() {
    let mut layouts = 0u64;
    for nruns in 1..=400usize {
        let floor = fixed_bytes(nruns) + 16 + 8 * (nruns + 1);
        let step = if nruns <= 64 { 1 } else { 7 };
        let mut usable = floor;
        while usable < floor + 300_000 {
            let sb = var_stash_bytes(usable, nruns);
            let (buf, room) = var_stash_room(usable, nruns, sb);
            assert!(buf <= room, "nruns {nruns} usable {usable}: stash {sb}, run buffer {buf} past the room {room}");
            layouts += 1;
            usable += step;
        }
    }
    assert!(layouts > 1_000_000);
}

/// The old cut (no 8 bytes for the floor) against the same layout: the gap the test above closes is real.
#[test]
fn the_old_cut_left_the_buffer_past_the_room() {
    let old = |usable: usize, nruns: usize| 8 + (usable - fixed_bytes(nruns) - 8) / (nruns + 1) / 8 * 8;
    let over = (1..=64usize)
        .flat_map(|n| (fixed_bytes(n) + 8 * (n + 2)..fixed_bytes(n) + 8 * (n + 2) + 20_000).map(move |u| (n, u)))
        .filter(|&(n, u)| {
            let (buf, room) = var_stash_room(u, n, old(u, n));
            buf > room
        })
        .count();
    assert!(over > 0, "the old cut never left a buffer past the room");
}
