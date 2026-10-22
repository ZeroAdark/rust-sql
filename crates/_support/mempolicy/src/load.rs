//! A bulk load's room: what a COPY FROM may hold in flight and how it splits between the decode window and the open
//! parts. Live at advise as well as on; only mode off leaves the load to its own bounds.

use memledger::{Cfg, Mode};

/// Resident, not the ledger's sum: the allocator's retention counts toward the watchdog's cancel too. Dirty bytes up to
/// the write budget do not count: they live in the page-cache floor B leaves outside itself.
pub fn load_room(limit: u64, bar: u64, resident: u64, dirty: u64) -> u64 {
    load_room_of(memledger::cfg(), limit, bar, resident, dirty)
}

pub fn load_room_of(cfg: Cfg, limit: u64, bar: u64, resident: u64, dirty: u64) -> u64 {
    if limit == 0 || cfg.mode == Mode::Off {
        return u64::MAX;
    }
    let b = crate::sizes::budget_of(limit, cfg.target, cfg.page_cache_floor, cfg.reserve, bar);
    let counted = resident.saturating_sub(dirty.min(write_budget_of(cfg, limit)));
    let room = b.b.saturating_sub(b.r_low).saturating_sub(counted);
    if crate::trace::on() {
        crate::trace::note("load.room", &format!("limit {limit} bar {bar} B {} r_low {} resident {resident} dirty {dirty}: {room}", b.b, b.r_low)); // alloc: Cold
    }
    room
}

/// Dirty and writeback pages count toward the watchdog's cancel, so they live in the space B leaves outside itself.
pub fn write_budget(limit: u64) -> u64 {
    write_budget_of(memledger::cfg(), limit)
}

pub fn write_budget_of(cfg: Cfg, limit: u64) -> u64 {
    if cfg.page_cache_floor != 0 {
        cfg.page_cache_floor
    } else {
        crate::sizes::page_cache_floor_default(limit)
    }
}

pub fn load_window(room: u64) -> u64 {
    if room == u64::MAX {
        u64::MAX
    } else {
        room / 2
    }
}

pub fn load_parts(room: u64, in_flight: u64) -> u64 {
    if room == u64::MAX {
        u64::MAX
    } else {
        room.saturating_sub(in_flight)
    }
}

/// D4409 (a): what a part's image buffer reserves past its length when `extra` more bytes do not fit its capacity:
/// `extra` plus an eighth of what it `holds`. Never Vec's doubling: on a part that does not pack the image is about as
/// large as its columns, and a doubled capacity alone took the seal past the charge its part holds; the eighth keeps the
/// growth's copies amortized and leaves room for the small sections a stream adds after its bulk ones.
pub fn image_grow(holds: u64, extra: u64) -> u64 {
    extra.saturating_add(holds / IMAGE_SLACK_DIV)
}

/// D4409 (a): the image's slack as a fraction of what it holds (`image_grow`).
const IMAGE_SLACK_DIV: u64 = 8;

#[cfg(test)]
mod tests {
    use super::{image_grow, load_parts, load_room_of, load_window, write_budget_of};
    use memledger::{Cfg, Mode};

    const MIB: u64 = 1 << 20;

    #[test]
    fn a_load_holds_resident_bytes_inside_b_less_its_low_mark() {
        let limit = 15741 * MIB;
        let cap = load_room_of(Cfg::DEFAULT, limit, 0, 0, 0);
        assert_eq!(cap, limit - 1024 * MIB - limit / 16 - 1024 * MIB);
        assert!(cap + write_budget_of(Cfg::DEFAULT, limit) < limit / 100 * 90);
        assert_eq!(load_room_of(Cfg::DEFAULT, limit, 0, 4000 * MIB, 0), cap - 4000 * MIB);
        assert_eq!(load_room_of(Cfg::DEFAULT, limit, 0, limit, 0), 0);
        let target = Cfg { target: 8192 * MIB, ..Cfg::DEFAULT };
        assert_eq!(load_room_of(target, limit, 0, 0, 0), 8192 * MIB - 1024 * MIB);
        assert_eq!(load_room_of(Cfg::DEFAULT, 0, 0, 0, 0), u64::MAX);
    }

    #[test]
    fn dirty_bytes_count_only_past_the_write_budget() {
        let limit = 15741 * MIB;
        let cap = load_room_of(Cfg::DEFAULT, limit, 0, 0, 0);
        let w = write_budget_of(Cfg::DEFAULT, limit);
        assert_eq!(load_room_of(Cfg::DEFAULT, limit, 0, 4000 * MIB, 500 * MIB), cap - 3500 * MIB);
        assert_eq!(load_room_of(Cfg::DEFAULT, limit, 0, 4000 * MIB, 3000 * MIB), cap - (4000 * MIB - w));
    }

    #[test]
    fn the_write_budget_is_the_page_cache_floor() {
        assert_eq!(write_budget_of(Cfg::DEFAULT, 12288 * MIB), 768 * MIB);
        assert_eq!(write_budget_of(Cfg::DEFAULT, 32768 * MIB), 2048 * MIB);
        assert_eq!(write_budget_of(Cfg { page_cache_floor: 300 * MIB, ..Cfg::DEFAULT }, 12288 * MIB), 300 * MIB);
    }

    #[test]
    fn the_bound_is_live_at_advise_and_on_and_off_only_at_off() {
        let limit = 15741 * MIB;
        assert_eq!(Cfg::DEFAULT.mode, Mode::Advise, "the default broker mode");
        let advise = load_room_of(Cfg { mode: Mode::Advise, ..Cfg::DEFAULT }, limit, 0, 4000 * MIB, 0);
        let on = load_room_of(Cfg { mode: Mode::On, ..Cfg::DEFAULT }, limit, 0, 4000 * MIB, 0);
        assert!(advise < limit, "advise bounds the load");
        assert_eq!(advise, on);
        assert_eq!(load_room_of(Cfg { mode: Mode::Off, ..Cfg::DEFAULT }, limit, 0, limit, 0), u64::MAX);
    }

    #[test]
    fn the_window_takes_half_the_room_and_the_parts_the_rest() {
        let room = 12710 * MIB;
        assert_eq!(load_window(room), 6355 * MIB);
        assert_eq!(load_parts(room, 3625 * MIB), 9085 * MIB);
        assert_eq!(load_parts(room, room + 1), 0);
        assert_eq!(load_window(u64::MAX), u64::MAX);
        assert_eq!(load_parts(u64::MAX, 3625 * MIB), u64::MAX);
    }

    #[test]
    fn an_image_grows_by_what_it_adds_and_an_eighth_never_doubling() {
        assert_eq!(image_grow(0, 100), 100);
        assert_eq!(image_grow(16 * MIB, 4096), 2 * MIB + 4096);
        assert!(image_grow(16 * MIB, 1) < 16 * MIB, "far below a doubling");
    }
}
