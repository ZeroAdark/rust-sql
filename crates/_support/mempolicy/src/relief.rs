//! Pending relief: bytes freed or asked for that have not landed. A read that decides the ladder's climb is netted
//! only under the watchdog's cancel line (`read_for`, R5); a read or room that only sizes an ask is netted always;
//! raise room never counts them. `backstop_covers` and `projected` are the backstops' own rules, not the ladder's.
//! Each method is the arithmetic its sites had before H4 (`tests`, one per site).

/// Where the bytes come from; the arithmetic is the same for all. `OwnedPurge` is (H)'s slot, held off trunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    ReaperCredit,
    ReleaserLanding,
    BuffersLanding,
    Asked,
    Reusable,
    OwnedPurge,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Relief {
    pending: u64,
}

impl Relief {
    pub const NONE: Relief = Relief { pending: 0 };

    #[inline]
    pub const fn of(source: Source, bytes: u64) -> Relief {
        let _ = source;
        Relief { pending: bytes }
    }

    #[inline]
    pub const fn and(self, source: Source, bytes: u64) -> Relief {
        let _ = source;
        Relief { pending: self.pending.saturating_add(bytes) }
    }

    #[inline]
    pub const fn pending(self) -> u64 {
        self.pending
    }

    #[inline]
    pub const fn is_none(self) -> bool {
        self.pending == 0
    }

    #[inline]
    pub const fn read_for(self, read: u64, past_cancel: bool) -> u64 {
        if past_cancel { read } else { read.saturating_sub(self.pending) }
    }

    #[inline]
    pub const fn ask_read(self, read: u64) -> u64 {
        read.saturating_sub(self.pending)
    }

    #[inline]
    pub const fn ask_room(self, room: u64) -> u64 {
        room.saturating_add(self.pending)
    }

    #[inline]
    pub const fn ask_after(self, need: u64) -> u64 {
        need.saturating_sub(self.pending)
    }

    #[inline]
    pub const fn raise_room(self, room: u64) -> u64 {
        room.saturating_sub(self.pending)
    }

    /// Netted at every sample; what it holds off is R6 under the cancel line only (`r6_held`).
    #[inline]
    pub const fn landing_read(self, read: u64) -> u64 {
        read.saturating_sub(self.pending)
    }

    /// Holds past the watchdog's cancel tier, up to its top tier (its caller's test).
    #[inline]
    pub const fn backstop_covers(self, excess: u64) -> bool {
        self.pending != 0 && self.pending >= excess
    }

    #[inline]
    pub const fn projected(self, r: u64) -> u64 {
        r.saturating_sub(self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::{Relief, Source};

    const MIB: u64 = 1 << 20;
    const VALS: [u64; 8] = [0, 1, 64 * MIB, 1228 * MIB, 1366 * MIB, 1536 * MIB, u64::MAX - 1, u64::MAX];

    #[test]
    fn site_i_the_hooks_climb_read_is_the_old_one_and_never_netted_past_the_line() {
        for &credit in &VALS {
            for &buffers in &[0, 28 * MIB, u64::MAX] {
                for &read in &VALS {
                    for past in [false, true] {
                        for hook in [false, true] {
                            let landing = if past { 0 } else { credit.saturating_add(buffers) };
                            let old = if hook { read.saturating_sub(landing) } else { read };
                            let relief = if past { Relief::NONE } else { Relief::of(Source::ReaperCredit, credit).and(Source::BuffersLanding, buffers) };
                            let new = if hook { relief.read_for(read, past) } else { read };
                            assert_eq!(new, old, "credit {credit} buffers {buffers} read {read} past {past} hook {hook}");
                            if past {
                                assert_eq!(new, read, "past the cancel line pending never nets the climb read");
                                assert_eq!(Relief::of(Source::ReaperCredit, credit).read_for(read, true), read, "whatever is pending");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn site_ii_r2as_take_reads_and_rooms_are_the_old_ones() {
        let line = 1228 * MIB;
        for &p in &VALS {
            let unmap = Relief::of(Source::ReaperCredit, p);
            assert_eq!(!unmap.is_none(), p != 0, "relief_coming");
            assert_eq!(unmap.is_none(), p == 0, "the raises' quiet test");
            for &x in &VALS {
                assert_eq!(line.saturating_sub(unmap.ask_read(x)), line.saturating_sub(x.saturating_sub(p)), "at_take_target's room");
                assert_eq!(unmap.ask_read(x), x.saturating_sub(p), "background_pass's read");
                assert_eq!(unmap.raise_room(x), x.saturating_sub(p), "the raise window's room");
                assert_eq!(Relief::of(Source::BuffersLanding, p).ask_room(x), x.saturating_add(p), "the drain's room");
            }
        }
    }

    #[test]
    fn site_iii_the_governors_asked_bytes_are_the_old_numbers() {
        for &p in &VALS {
            let asked = Relief::of(Source::Asked, p);
            assert_eq!(!asked.is_none(), p != 0, "Inputs::relief");
            for &x in &VALS {
                assert_eq!(asked.projected(x), x.saturating_sub(p), "Inputs::crosses / overshoot");
                assert_eq!(asked.backstop_covers(x), p != 0 && p >= x, "watchdog::landing_holds");
            }
        }
    }

    #[test]
    fn site_iv_the_r6_landing_tests_read_is_the_old_one() {
        for &p in &VALS {
            for &x in &VALS {
                assert_eq!(Relief::of(Source::Reusable, p).landing_read(x), x.saturating_sub(p));
            }
        }
    }

    #[test]
    fn site_v_a_draws_releaser_landing_is_the_old_room_and_shortfall() {
        for &on_way in &VALS {
            let r = Relief::of(Source::ReleaserLanding, on_way);
            for &free_b in &VALS {
                assert_eq!(r.ask_room(free_b), free_b.saturating_add(on_way), "free_toward");
                for &bytes in &VALS {
                    assert_eq!(r.ask_room(free_b) >= bytes, free_b.saturating_add(on_way) >= bytes, "make_room's room test");
                    if free_b < bytes && free_b.saturating_add(on_way) < bytes {
                        assert_eq!(r.ask_after(bytes - free_b), bytes - free_b - on_way, "make_room's shortfall");
                    }
                }
            }
        }
    }

    #[test]
    fn sources_add_up_and_none_is_nothing() {
        assert!(Relief::NONE.is_none());
        let r = Relief::of(Source::ReaperCredit, 100 * MIB).and(Source::BuffersLanding, 28 * MIB);
        assert_eq!(r.pending(), 128 * MIB, "mcx::ladder::state(): r.landing() + buffers_landing_at(now)");
        assert_eq!(Relief::of(Source::Asked, u64::MAX).and(Source::OwnedPurge, 1).pending(), u64::MAX, "saturating");
        assert_eq!(Relief::default(), Relief::NONE);
    }
}
