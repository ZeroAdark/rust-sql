//! The broker's policy as a pure function of one interval's signals: each consumer's desired bytes, the fit inside
//! B - R_high (cut toward the floors in the fixed refill order when over), then hysteresis from the previous target.

use memledger::{Consumer, CONSUMERS};

use crate::transfer::{self, Doors, Side, Worth};

const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyCfg {
    /// desired = the re-used working set over 900 s x this / 100.
    pub multiplier_pct: u64,
    /// No move under max(target x this / 100, `dead_band_min`).
    pub dead_band_pct: u64,
    pub dead_band_min: u64,
    /// One interval moves a target up by at most max(this %, the dead band), down by max(`shrink_pct` %, one unit).
    pub grow_pct: u64,
    pub shrink_pct: u64,
    /// No reversal of a consumer's last move inside this many seconds.
    pub cooldown_s: u64,
    /// `freeze_count` reversals inside `freeze_window_s` freeze the consumer for `freeze_s`.
    pub freeze_count: u32,
    pub freeze_window_s: u64,
    pub freeze_s: u64,
    pub unit_sb: u64,
    pub unit: u64,
}

impl PolicyCfg {
    pub const DEFAULT: PolicyCfg = PolicyCfg {
        multiplier_pct: 110,
        dead_band_pct: 5,
        dead_band_min: 256 * MIB,
        grow_pct: 50,
        shrink_pct: 20,
        cooldown_s: 300,
        freeze_count: 3,
        freeze_window_s: 3600,
        freeze_s: 1800,
        unit_sb: 2 * MIB,
        unit: 64 * MIB,
    };

    fn unit_of(&self, c: Consumer) -> u64 {
        if c == Consumer::SharedBuffers { self.unit_sb.max(1) } else { self.unit.max(1) }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub registered: bool,
    pub current: u64,
    pub floor: u64,
    /// `u64::MAX` = unbounded: the seed of the first target.
    pub cap: u64,
    /// The re-used working set over 60 / 300 / 900 s (each part counted at its NEED), for a consumer with a re-use signal.
    pub reused: Option<[u64; 3]>,
    /// A re-used part was evicted in the last 300 s: no shrink the fit does not force.
    pub reused_evicted: bool,
    /// The configured size the target never goes under: shared_buffers' setting, the explicit part cache.
    pub minimum: u64,
    pub doors: Option<Doors>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signals {
    pub now: u64,
    pub b: u64,
    pub r_high: u64,
    pub fixed: u64,
    pub work_demand: u64,
    pub queued: u64,
    pub inputs: [Input; CONSUMERS],
    pub pool_on_tick: bool,
    /// Broker (b), `broker_value=1` (`transfer`).
    pub value: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hold {
    #[default]
    None,
    AtGoal,
    DeadBand,
    Frozen,
    Cooldown,
    NotAgreed,
    NoRoom,
}

impl Hold {
    pub const fn name(self) -> &'static str {
        match self {
            Hold::None => "",
            Hold::AtGoal => "at_goal",
            Hold::DeadBand => "dead_band",
            Hold::Frozen => "frozen",
            Hold::Cooldown => "cooldown",
            Hold::NotAgreed => "not_agreed",
            Hold::NoRoom => "no_room",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub seeded: bool,
    pub target: u64,
    /// +1 grew, -1 shrank, 0 never moved.
    pub last_dir: i8,
    pub last_move_at: u64,
    pub last_move_bytes: i64,
    pub reversals: u32,
    pub reversal_at: [u64; 8],
    pub frozen_until: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PolicyState {
    pub slots: [Slot; CONSUMERS],
    pub plans: u64,
    pub worth: [Worth; CONSUMERS],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub targeted: [bool; CONSUMERS],
    pub floor: [u64; CONSUMERS],
    /// B - R_high - fixed - every other targeted consumer's floor.
    pub ceiling: [u64; CONSUMERS],
    pub desired: [u64; CONSUMERS],
    pub goal: [u64; CONSUMERS],
    pub target: [u64; CONSUMERS],
    pub elastic: [u64; CONSUMERS],
    pub moved: [i64; CONSUMERS],
    pub hold: [Hold; CONSUMERS],
    pub fits: bool,
    pub floors_over: bool,
    /// Broker (b): ns per MiB (`reclaim::by_loss`).
    pub loss: [Option<u64>; CONSUMERS],
}

/// Cheapest refill first, the caches in `reclaim::ORDER`'s order; the work pool is cut last, never below its floor.
pub const CUT_ORDER: [Consumer; 7] = [
    Consumer::BuildPool,
    Consumer::ZoneStore,
    Consumer::PartCache,
    Consumer::CondCache,
    Consumer::SharedBuffers,
    Consumer::FpTables,
    Consumer::WorkPool,
];

pub const fn refill_class(c: Consumer) -> &'static str {
    match c {
        Consumer::SharedBuffers => "page_read",
        Consumer::PartCache => "part_fault",
        Consumer::FpTables => "fp_build",
        Consumer::BuildPool => "buffer_alloc",
        Consumer::ZoneStore => "zone_read",
        Consumer::CondCache => "cond_eval",
        Consumer::WorkPool => "spill",
        Consumer::ProgramCaches => "compile",
        Consumer::PerBackend => "none",
        Consumer::CatalogL2 => "catalog_scan",
        Consumer::NblocksCache => "none",
        Consumer::SharedStats => "none",
    }
}

fn targeted(c: Consumer, i: &Input, pool_on_tick: bool) -> bool {
    i.registered && !c.is_fixed() && !(pool_on_tick && c == Consumer::WorkPool)
}

fn pct(x: u64, p: u64) -> u64 {
    ((x as u128 * p as u128) / 100).min(u64::MAX as u128) as u64
}

fn round_up(x: u64, unit: u64) -> u64 {
    x.div_ceil(unit).saturating_mul(unit)
}

fn target_now(state: &PolicyState, s: &Signals, p: &Plan, k: usize) -> u64 {
    let slot = &state.slots[k];
    if slot.seeded { slot.target } else { s.inputs[k].cap.clamp(p.floor[k], p.ceiling[k]) }
}

/// A cache gives only reclaimable bytes (above its in-use floor) in its tail; one over its target only through pins sits out.
fn value_sides<'a>(state: &PolicyState, s: &Signals, cfg: &PolicyCfg, p: &Plan, worth: &'a [Worth; CONSUMERS]) -> [Option<Side<'a>>; CONSUMERS] {
    let unit = cfg.unit.max(1);
    let mut sides = [None; CONSUMERS];
    for c in Consumer::ALL.into_iter().filter(|c| c.is_cache()) {
        let (k, i) = (c as usize, &s.inputs[c as usize]);
        if !p.targeted[k] || i.doors.is_none() || !transfer::measured(&worth[k]) {
            continue;
        }
        let at = target_now(state, s, p, k);
        if i.current > at && i.floor >= at {
            continue;
        }
        let give = i.current.saturating_sub(i.floor).min(worth[k].seen.tail_bytes).min(pct(at, cfg.shrink_pct).max(unit)).min(at.saturating_sub(p.floor[k]));
        let take = p.ceiling[k].saturating_sub(at).min(pct(at, cfg.grow_pct).max(unit));
        sides[k] = Some(Side { worth: &worth[k], give: give / unit * unit, take: take / unit * unit });
    }
    sides
}

pub fn plan(state: &PolicyState, s: &Signals, cfg: &PolicyCfg) -> (Plan, PolicyState) {
    let mut p = Plan::default();
    let held = if s.pool_on_tick { s.inputs[Consumer::WorkPool as usize].current } else { 0 };
    let room = s.b.saturating_sub(s.r_high).saturating_sub(s.fixed).saturating_sub(held);
    for c in Consumer::ALL {
        let i = &s.inputs[c as usize];
        p.targeted[c as usize] = targeted(c, i, s.pool_on_tick);
        p.floor[c as usize] = i.floor.max(i.minimum);
    }
    let ix = |c: Consumer| c as usize;
    let sum_floors = Consumer::ALL.iter().filter(|c| p.targeted[ix(**c)]).fold(0u64, |a, c| a.saturating_add(p.floor[ix(*c)]));
    for c in Consumer::ALL {
        let k = ix(c);
        if !p.targeted[k] {
            p.desired[k] = s.inputs[k].current;
            p.goal[k] = s.inputs[k].current;
            continue;
        }
        let floor = p.floor[k];
        p.ceiling[k] = room.saturating_sub(sum_floors.saturating_sub(floor)).max(floor);
        let i = &s.inputs[k];
        let want = match (c, i.reused) {
            (Consumer::WorkPool, _) => s.work_demand,
            (Consumer::SharedBuffers, _) => i.current.max(i.minimum),
            (_, Some(r)) => pct(r[2], cfg.multiplier_pct),
            // no re-use signal yet: the cache keeps what it holds
            (_, None) => i.current,
        };
        p.desired[k] = want.clamp(floor, p.ceiling[k]);
        p.goal[k] = p.desired[k];
    }
    let mut worth = state.worth;
    let mut moved = [0i64; CONSUMERS];
    if s.value {
        for c in Consumer::ALL {
            if let Some(d) = s.inputs[ix(c)].doors {
                worth[ix(c)] = transfer::decay(&state.worth[ix(c)], &d);
                p.loss[ix(c)] = transfer::loss_per_mib(&worth[ix(c)]);
            }
        }
        let sides = value_sides(state, s, cfg, &p, &worth);
        moved = transfer::transfer(&sides, cfg.unit);
        for k in (0..CONSUMERS).filter(|&k| sides[k].is_some()) {
            p.desired[k] = target_now(state, s, &p, k).saturating_add_signed(moved[k]).clamp(p.floor[k], p.ceiling[k]);
            p.goal[k] = p.desired[k];
        }
    }
    let order = if s.value { crate::reclaim::by_loss(CUT_ORDER, |c| p.loss[c as usize]) } else { CUT_ORDER };
    let total = Consumer::ALL.iter().filter(|c| p.targeted[ix(**c)]).fold(0u64, |a, c| a.saturating_add(p.desired[ix(*c)]));
    if total <= room {
        p.fits = true;
        let left = room - total;
        let caches = Consumer::ALL.iter().filter(|c| c.is_cache() && p.targeted[ix(**c)]).fold(0u64, |a, c| a.saturating_add(p.desired[ix(*c)]));
        if caches > 0 {
            for c in Consumer::ALL.into_iter().filter(|c| c.is_cache() && p.targeted[ix(*c)]) {
                p.elastic[ix(c)] = ((left as u128 * p.desired[ix(c)] as u128) / caches as u128) as u64;
            }
        }
    } else {
        let mut excess = total - room;
        for c in order {
            let k = ix(c);
            if excess == 0 {
                break;
            }
            if !p.targeted[k] {
                continue;
            }
            let cut = excess.min(p.goal[k].saturating_sub(p.floor[k]));
            p.goal[k] -= cut;
            excess -= cut;
        }
        p.floors_over = excess > 0;
    }

    let mut next = *state;
    next.plans += 1;
    next.worth = worth;
    let mut step = [(0i8, 0u64); CONSUMERS];
    for c in Consumer::ALL {
        let k = ix(c);
        if !p.targeted[k] {
            continue;
        }
        let slot = &mut next.slots[k];
        let floor = p.floor[k];
        if !slot.seeded {
            slot.seeded = true;
            slot.target = s.inputs[k].cap.clamp(floor, p.ceiling[k]);
        }
        // a floor is in use now: the target is never under it (no hysteresis)
        slot.target = slot.target.max(floor);
        let prev = slot.target;
        let goal = p.goal[k];
        p.hold[k] = Hold::AtGoal;
        if goal == prev {
            continue;
        }
        if s.now < slot.frozen_until {
            p.hold[k] = Hold::Frozen;
            continue;
        }
        let band = pct(prev, cfg.dead_band_pct).max(cfg.dead_band_min);
        let diff = goal.abs_diff(prev);
        let unit = cfg.unit_of(c);
        let (dir, bytes) = if goal < prev {
            let forced = goal < p.desired[k] || prev > p.ceiling[k];
            if diff < band && !forced {
                p.hold[k] = Hold::DeadBand;
                continue;
            }
            let i = &s.inputs[k];
            let agreed = match c {
                Consumer::WorkPool => s.queued == 0,
                _ if moved[k] < 0 => true,
                _ => i.reused.is_none_or(|r| r[1] < prev && r[2] < prev) && !i.reused_evicted,
            };
            if !forced && !agreed {
                p.hold[k] = Hold::NotAgreed;
                continue;
            }
            let most = pct(prev, cfg.shrink_pct).max(unit);
            (-1i8, diff.min(most) / unit * unit)
        } else {
            if diff < band {
                p.hold[k] = Hold::DeadBand;
                continue;
            }
            let most = pct(prev, cfg.grow_pct).max(band);
            (1i8, round_up(diff.min(most), unit).min(p.ceiling[k].saturating_sub(prev)))
        };
        if bytes == 0 {
            p.hold[k] = Hold::DeadBand;
            continue;
        }
        if slot.last_dir != 0 && dir != slot.last_dir && s.now < slot.last_move_at.saturating_add(cfg.cooldown_s) {
            p.hold[k] = Hold::Cooldown;
            continue;
        }
        step[k] = (dir, bytes);
    }
    // donors before recipients: the grows get the room this interval's shrinks leave, the dearest refill first
    let base = Consumer::ALL.iter().filter(|c| p.targeted[ix(**c)]).fold(0u64, |a, c| {
        let k = ix(*c);
        let t = next.slots[k].target;
        a.saturating_add(if step[k].0 < 0 { t - step[k].1 } else { t })
    });
    let mut avail = room.saturating_sub(base);
    for c in order.into_iter().rev() {
        let k = ix(c);
        if step[k].0 <= 0 {
            continue;
        }
        let unit = cfg.unit_of(c);
        let g = if step[k].1 <= avail { step[k].1 } else { avail / unit * unit };
        avail -= g;
        if g == 0 {
            p.hold[k] = Hold::NoRoom;
            step[k] = (0, 0);
        } else {
            step[k].1 = g;
        }
    }
    for c in Consumer::ALL {
        let k = ix(c);
        let (dir, bytes) = step[k];
        let slot = &mut next.slots[k];
        if p.targeted[k] {
            if dir != 0 {
                let reversal = slot.last_dir != 0 && dir != slot.last_dir;
                slot.target = if dir < 0 { slot.target - bytes } else { slot.target + bytes };
                slot.last_dir = dir;
                slot.last_move_at = s.now;
                slot.last_move_bytes = if dir < 0 { -(bytes as i64) } else { bytes as i64 };
                p.moved[k] = slot.last_move_bytes;
                p.hold[k] = Hold::None;
                if reversal {
                    slot.reversals += 1;
                    slot.reversal_at.copy_within(0..7, 1);
                    slot.reversal_at[0] = s.now;
                    let n = slot.reversals.min(8) as usize;
                    let recent = slot.reversal_at[..n].iter().filter(|&&t| t.saturating_add(cfg.freeze_window_s) > s.now).count();
                    if cfg.freeze_count > 0 && recent >= cfg.freeze_count as usize {
                        slot.frozen_until = s.now.saturating_add(cfg.freeze_s);
                    }
                }
            }
            p.target[k] = slot.target;
        } else {
            p.target[k] = s.inputs[k].current;
        }
    }
    (p, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn signals() -> Signals {
        let mut s = Signals { now: 10_000, b: 27 * GIB, r_high: 2 * GIB, fixed: GIB, work_demand: 2 * GIB, queued: 0, inputs: [Input::default(); CONSUMERS], pool_on_tick: false, value: false };
        let mut set = |c: Consumer, current: u64, floor: u64, cap: u64, reused: Option<[u64; 3]>, minimum: u64| {
            s.inputs[c as usize] = Input { registered: true, current, floor, cap, reused, reused_evicted: false, minimum, doors: None };
        };
        set(Consumer::SharedBuffers, 2 * GIB, 2 * GIB, 2 * GIB, None, 2 * GIB);
        set(Consumer::PartCache, 0, 0, 3 * GIB + GIB / 2, Some([0; 3]), 0);
        set(Consumer::FpTables, 0, 0, 3 * GIB, Some([0; 3]), 0);
        set(Consumer::BuildPool, 0, 0, 3 * GIB, None, 0);
        set(Consumer::ZoneStore, 0, 0, GIB / 2, None, 0);
        set(Consumer::CondCache, 0, 0, GIB / 2, None, 0);
        set(Consumer::ProgramCaches, GIB / 2, GIB / 2, GIB / 2, None, 0);
        set(Consumer::WorkPool, 0, 0, 20 * GIB, None, 0);
        set(Consumer::PerBackend, GIB / 2, GIB / 2, GIB / 2, None, 0);
        s
    }

    fn run(state: &PolicyState, s: &Signals) -> (Plan, PolicyState) {
        plan(state, s, &PolicyCfg::DEFAULT)
    }

    fn steady(s: &Signals) -> PolicyState {
        let (p, mut st) = run(&PolicyState::default(), s);
        for c in Consumer::ALL.into_iter().filter(|c| p.targeted[*c as usize]) {
            st.slots[c as usize] = Slot { seeded: true, target: p.goal[c as usize], ..Slot::default() };
        }
        st
    }

    fn settle(s: &mut Signals, mut st: PolicyState, n: usize) -> (Plan, PolicyState) {
        let mut last = run(&st, s);
        for _ in 0..n {
            st = last.1;
            s.now += 30;
            last = run(&st, s);
            if last.0.moved.iter().all(|m| *m == 0) {
                break;
            }
        }
        last
    }

    fn at(p: &Plan, c: Consumer) -> u64 {
        p.target[c as usize]
    }

    #[test]
    fn everything_fits_each_consumer_gets_its_desired_and_the_leftover_stays_outside_the_reserve() {
        let mut s = signals();
        s.inputs[Consumer::PartCache as usize].reused = Some([GIB, GIB, GIB]);
        let (p, _) = run(&PolicyState::default(), &s);
        assert!(p.fits && !p.floors_over);
        assert_eq!(p.desired[Consumer::PartCache as usize], pct(GIB, 110));
        assert_eq!(p.desired[Consumer::WorkPool as usize], 2 * GIB);
        assert_eq!(p.desired[Consumer::SharedBuffers as usize], 2 * GIB);
        let room = s.b - s.r_high - s.fixed;
        let targeted: u64 = Consumer::ALL.iter().filter(|c| p.targeted[**c as usize]).map(|c| p.desired[*c as usize] + p.elastic[*c as usize]).sum();
        assert!(targeted <= room, "{targeted} {room}");
        assert!(room - targeted < 16, "the leftover is handed out: {}", room - targeted);
        assert_eq!(p.elastic[Consumer::WorkPool as usize], 0);
        assert_eq!(p.elastic[Consumer::SharedBuffers as usize], 0);
        assert!(!p.targeted[Consumer::ProgramCaches as usize] && !p.targeted[Consumer::PerBackend as usize]);
    }

    #[test]
    fn over_b_cuts_toward_the_floors_cheapest_refill_first_and_the_work_pool_last() {
        let mut s = signals();
        s.b = 12 * GIB;
        for (c, cur) in [(Consumer::BuildPool, 2 * GIB), (Consumer::ZoneStore, GIB / 2), (Consumer::CondCache, GIB / 2)] {
            s.inputs[c as usize].current = cur;
        }
        s.inputs[Consumer::PartCache as usize].reused = Some([4 * GIB; 3]);
        s.inputs[Consumer::FpTables as usize].reused = Some([GIB; 3]);
        s.work_demand = 3 * GIB;
        let (p, _) = run(&PolicyState::default(), &s);
        assert!(!p.fits && !p.floors_over);
        assert_eq!(p.goal[Consumer::BuildPool as usize], 0);
        assert_eq!(p.goal[Consumer::ZoneStore as usize], 0);
        assert_eq!(p.goal[Consumer::CondCache as usize], GIB / 2, "the condition cache after the part cache (reclaim::ORDER)");
        assert_eq!(p.goal[Consumer::SharedBuffers as usize], 2 * GIB, "shared_buffers never under its configured size");
        let part = p.goal[Consumer::PartCache as usize];
        assert!(part < pct(4 * GIB, 110) && part > 0, "{part}");
        assert_eq!(p.goal[Consumer::FpTables as usize], pct(GIB, 110), "fp after the part cache");
        assert_eq!(p.goal[Consumer::WorkPool as usize], 3 * GIB, "the work pool last");
        let goals: u64 = Consumer::ALL.iter().filter(|c| p.targeted[**c as usize]).map(|c| p.goal[*c as usize]).sum();
        assert_eq!(goals, s.b - s.r_high - s.fixed);
    }

    #[test]
    fn b0_the_plan_leaves_the_pool_to_the_tick_and_fits_the_caches_beside_what_it_holds() {
        let mut s = signals();
        s.b = 12 * GIB;
        for (c, cur) in [(Consumer::BuildPool, 2 * GIB), (Consumer::ZoneStore, GIB / 2), (Consumer::CondCache, GIB / 2), (Consumer::WorkPool, 2 * GIB)] {
            s.inputs[c as usize].current = cur;
        }
        s.inputs[Consumer::PartCache as usize].reused = Some([4 * GIB; 3]);
        s.inputs[Consumer::FpTables as usize].reused = Some([GIB; 3]);
        s.work_demand = 3 * GIB;
        let today = run(&PolicyState::default(), &s).0;
        assert!(today.targeted[Consumer::WorkPool as usize] && today.goal[Consumer::WorkPool as usize] == 3 * GIB);
        s.pool_on_tick = true;
        let (p, _) = run(&PolicyState::default(), &s);
        let w = Consumer::WorkPool as usize;
        assert!(!p.targeted[w] && (p.goal[w], p.moved[w]) == (2 * GIB, 0), "the plan never moves the pool");
        let goals: u64 = Consumer::ALL.iter().filter(|c| p.targeted[**c as usize]).map(|c| p.goal[*c as usize]).sum();
        assert_eq!(goals, s.b - s.r_high - s.fixed - 2 * GIB, "the caches fit beside the 2 GiB the pool holds");
    }

    #[test]
    fn floors_past_b_leave_every_goal_at_its_floor_and_say_so() {
        let mut s = signals();
        s.b = 4 * GIB;
        s.inputs[Consumer::PartCache as usize].floor = 2 * GIB;
        s.inputs[Consumer::WorkPool as usize].floor = GIB;
        let (p, _) = run(&PolicyState::default(), &s);
        assert!(!p.fits && p.floors_over);
        for c in [Consumer::PartCache, Consumer::WorkPool, Consumer::SharedBuffers] {
            assert_eq!(p.goal[c as usize], p.floor[c as usize], "{c:?}");
            assert!(p.target[c as usize] >= p.floor[c as usize], "{c:?}");
        }
    }

    #[test]
    fn the_ceiling_is_b_less_the_reserve_fixed_and_the_other_floors_never_todays_cap() {
        let mut s = signals();
        s.inputs[Consumer::PartCache as usize].reused = Some([30 * GIB; 3]);
        s.inputs[Consumer::WorkPool as usize].floor = GIB;
        let (p, _) = run(&PolicyState::default(), &s);
        let ceiling = s.b - s.r_high - s.fixed - 2 * GIB - GIB;
        assert_eq!(p.ceiling[Consumer::PartCache as usize], ceiling);
        assert_eq!(p.desired[Consumer::PartCache as usize], ceiling);
        assert!(p.ceiling[Consumer::PartCache as usize] > s.inputs[Consumer::PartCache as usize].cap);
    }

    #[test]
    fn the_cliff_a_re_used_set_past_todays_cap_asks_past_it_when_b_allows() {
        let mut s = signals();
        let gb = 1_000_000_000u64;
        s.inputs[Consumer::PartCache as usize].reused = Some([43 * gb / 10; 3]);
        let cap = s.inputs[Consumer::PartCache as usize].cap;
        let (p, _) = settle(&mut s, PolicyState::default(), 40);
        assert!(p.desired[Consumer::PartCache as usize] >= 47 * gb / 10);
        assert!(at(&p, Consumer::PartCache) >= 47 * gb / 10, "{} from the seed {cap}", at(&p, Consumer::PartCache));
        assert!(at(&p, Consumer::PartCache) <= p.desired[Consumer::PartCache as usize] + 64 * MIB);
        let mut st = steady(&s);
        st.slots[Consumer::PartCache as usize].target = cap;
        let (p, _) = run(&st, &s);
        assert!(at(&p, Consumer::PartCache) >= 47 * gb / 10, "first interval: {}", at(&p, Consumer::PartCache));
    }

    #[test]
    fn the_first_target_is_todays_cap_clamped_and_an_unbounded_cap_seeds_at_the_ceiling() {
        let mut s = signals();
        s.inputs[Consumer::PartCache as usize].cap = u64::MAX;
        s.inputs[Consumer::PartCache as usize].reused = None;
        s.inputs[Consumer::PartCache as usize].current = 5 * GIB;
        let (p, st) = run(&PolicyState::default(), &s);
        let ceiling = p.ceiling[Consumer::PartCache as usize];
        let slot = st.slots[Consumer::PartCache as usize];
        assert!(slot.seeded);
        assert_eq!(p.moved[Consumer::PartCache as usize], -((ceiling - slot.target) as i64));
        assert!(slot.target >= ceiling - pct(ceiling, 20) && slot.target < ceiling);
    }

    /// The part cache cold (few tail hits), the fp tables hot (refaults within 512 MiB).
    fn value_plan(tail_bytes: u64, pinned: u64, part_target: u64) -> (Plan, PolicyState) {
        let mut s = signals();
        s.value = true;
        let (part, fp) = (Consumer::PartCache as usize, Consumer::FpTables as usize);
        (s.inputs[part].current, s.inputs[part].floor, s.inputs[part].reused) = (4 * GIB, pinned, None);
        (s.inputs[fp].current, s.inputs[fp].reused) = (2 * GIB, None);
        let mut st = steady(&s);
        (st.slots[part].target, st.slots[fp].target) = (part_target, 2 * GIB);
        let read = |tail: u64, ghosts: u64| {
            let mut d = Doors { tail_hits: tail, tail_bytes, cost_ns: 1_000_000, cost_samples: 1000, ..Doors::default() };
            d.ghosts[25..29].iter_mut().for_each(|g| *g = ghosts);
            d
        };
        (s.inputs[part].doors, s.inputs[fp].doors) = (Some(read(0, 0)), Some(read(0, 0)));
        let (_, st) = run(&st, &s);
        (s.inputs[part].doors, s.inputs[fp].doors) = (Some(read(10, 0)), Some(read(500, 100_000)));
        s.now += 30;
        run(&st, &s)
    }

    #[test]
    fn b2_the_value_broker_moves_the_cold_caches_bytes_to_the_hot_one() {
        let (p, st) = value_plan(512 * MIB, 0, 4 * GIB);
        let (part, fp) = (Consumer::PartCache as usize, Consumer::FpTables as usize);
        assert_eq!((p.moved[part], p.moved[fp]), (-512 * MIB as i64, 512 * MIB as i64), "{:?}", p.hold);
        assert!(p.loss[part] < p.loss[fp] && p.loss[part].is_some(), "{:?}", p.loss);
        assert_eq!(st.worth[fp].intervals, 2);
    }

    #[test]
    fn b2_a_transfer_under_the_dead_band_holds() {
        let (p, _) = value_plan(128 * MIB, 0, 4 * GIB);
        let (part, fp) = (Consumer::PartCache as usize, Consumer::FpTables as usize);
        assert_eq!((p.moved[part], p.moved[fp]), (0, 0));
        assert_eq!((p.hold[part], p.hold[fp]), (Hold::DeadBand, Hold::DeadBand), "two units (128 MiB) under the 256 MiB band");
    }

    #[test]
    fn b2_a_cache_over_its_target_only_through_pins_neither_gives_nor_takes() {
        let (p, _) = value_plan(512 * MIB, 4 * GIB, GIB);
        let (part, fp) = (Consumer::PartCache as usize, Consumer::FpTables as usize);
        assert_eq!(p.moved[fp], 0, "no donor: the pinned part cache gives nothing");
        assert!(p.target[part] >= 4 * GIB, "never under its in-use bytes: {}", p.target[part]);
    }

    #[test]
    fn the_dead_band_holds_small_moves_and_units_round_them() {
        let mut s = signals();
        s.inputs[Consumer::FpTables as usize].reused = Some([0, 0, 2 * GIB]);
        let mut st = steady(&s);
        st.slots[Consumer::FpTables as usize].target = pct(2 * GIB, 110) + 200 * MIB;
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::FpTables as usize], Hold::DeadBand, "200 MiB is under max(5%, 256 MiB)");
        st.slots[Consumer::FpTables as usize].target = pct(2 * GIB, 110) - 300 * MIB;
        let (p, _) = run(&st, &s);
        assert_eq!(p.moved[Consumer::FpTables as usize], 320 * MIB as i64, "300 MiB rounds up to five 64 MiB units");
    }

    #[test]
    fn one_interval_grows_at_most_half_and_shrinks_at_most_a_fifth() {
        let mut s = signals();
        let mut st = steady(&s);
        s.inputs[Consumer::PartCache as usize].reused = Some([16 * GIB; 3]);
        st.slots[Consumer::PartCache as usize].target = 4 * GIB;
        let (p, _) = run(&st, &s);
        assert_eq!(p.moved[Consumer::PartCache as usize], (2 * GIB) as i64);
        s.inputs[Consumer::PartCache as usize].reused = Some([0; 3]);
        st.slots[Consumer::PartCache as usize].target = 10 * GIB;
        let (p, _) = run(&st, &s);
        let fifth = pct(10 * GIB, 20) / (64 * MIB) * (64 * MIB);
        assert_eq!(p.moved[Consumer::PartCache as usize], -(fifth as i64));
    }

    #[test]
    fn a_shrink_waits_until_every_re_use_window_and_the_eviction_signal_agree() {
        let mut s = signals();
        let mut st = steady(&s);
        st.slots[Consumer::PartCache as usize].target = 4 * GIB;
        s.inputs[Consumer::PartCache as usize].reused = Some([GIB, GIB, 5 * GIB]);
        let (p, _) = run(&st, &s);
        assert!(p.moved[Consumer::PartCache as usize] >= 0);
        s.inputs[Consumer::PartCache as usize].reused = Some([GIB, 5 * GIB, GIB]);
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::PartCache as usize], Hold::NotAgreed);
        s.inputs[Consumer::PartCache as usize].reused = Some([GIB; 3]);
        s.inputs[Consumer::PartCache as usize].reused_evicted = true;
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::PartCache as usize], Hold::NotAgreed);
        s.inputs[Consumer::PartCache as usize].reused_evicted = false;
        let (p, _) = run(&st, &s);
        assert!(p.moved[Consumer::PartCache as usize] < 0);
        st.slots[Consumer::WorkPool as usize].target = 10 * GIB;
        s.queued = 1;
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::WorkPool as usize], Hold::NotAgreed);
        s.queued = 0;
        let (p, _) = run(&st, &s);
        assert!(p.moved[Consumer::WorkPool as usize] < 0);
    }

    #[test]
    fn a_shrink_the_fit_forces_does_not_wait_for_agreement() {
        let mut s = signals();
        s.b = 8 * GIB;
        s.inputs[Consumer::PartCache as usize].reused = Some([6 * GIB; 3]);
        let mut st = PolicyState::default();
        st.slots[Consumer::PartCache as usize] = Slot { seeded: true, target: 6 * GIB, ..Slot::default() };
        let (p, _) = run(&st, &s);
        assert!(p.goal[Consumer::PartCache as usize] < p.desired[Consumer::PartCache as usize]);
        assert!(p.moved[Consumer::PartCache as usize] < 0);
    }

    #[test]
    fn grows_get_only_the_room_the_shrinks_leave() {
        let mut s = signals();
        s.b = 12 * GIB;
        let mut st = PolicyState::default();
        st.slots[Consumer::SharedBuffers as usize] = Slot { seeded: true, target: 2 * GIB, ..Slot::default() };
        st.slots[Consumer::PartCache as usize] = Slot { seeded: true, target: 2 * GIB, ..Slot::default() };
        st.slots[Consumer::WorkPool as usize] = Slot { seeded: true, target: 5 * GIB, ..Slot::default() };
        for c in [Consumer::FpTables, Consumer::BuildPool, Consumer::ZoneStore, Consumer::CondCache] {
            st.slots[c as usize] = Slot { seeded: true, target: 0, ..Slot::default() };
        }
        s.inputs[Consumer::PartCache as usize].reused = Some([4 * GIB; 3]);
        s.work_demand = 3 * GIB;
        let (p, _) = run(&st, &s);
        let sum: u64 = Consumer::ALL.iter().filter(|c| p.targeted[**c as usize]).map(|c| p.target[*c as usize]).sum();
        assert!(sum <= s.b - s.r_high - s.fixed, "{sum}");
        assert!(p.moved[Consumer::WorkPool as usize] < 0, "the donor shrinks");
        assert!(p.moved[Consumer::PartCache as usize] > 0, "the recipient grows into what it freed");
        assert!(p.moved[Consumer::PartCache as usize] <= -p.moved[Consumer::WorkPool as usize]);
    }

    #[test]
    fn no_reversal_inside_the_cooldown_and_three_reversals_freeze_the_consumer() {
        let mut s = signals();
        let mut st = steady(&s);
        st.slots[Consumer::FpTables as usize] = Slot { seeded: true, target: 4 * GIB, last_dir: 1, last_move_at: s.now - 60, ..Slot::default() };
        s.inputs[Consumer::FpTables as usize].reused = Some([GIB; 3]);
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::FpTables as usize], Hold::Cooldown);
        let mut flips = 0;
        for k in 0..6u64 {
            s.now += 301;
            let up = k % 2 == 1;
            s.inputs[Consumer::FpTables as usize].reused = Some(if up { [6 * GIB; 3] } else { [GIB; 3] });
            let (p, next) = run(&st, &s);
            st = next;
            if p.moved[Consumer::FpTables as usize] != 0 {
                flips += 1;
            }
            if st.slots[Consumer::FpTables as usize].frozen_until > s.now {
                break;
            }
        }
        let slot = st.slots[Consumer::FpTables as usize];
        assert_eq!(slot.reversals, 3, "flips {flips}");
        assert_eq!(slot.frozen_until, s.now + 1800);
        s.now += 301;
        s.inputs[Consumer::FpTables as usize].reused = Some([GIB; 3]);
        let (p, _) = run(&st, &s);
        assert_eq!(p.hold[Consumer::FpTables as usize], Hold::Frozen);
    }

    #[test]
    fn an_explicit_part_cache_is_a_minimum_and_shared_buffers_never_goes_under_its_setting() {
        let mut s = signals();
        s.b = 6 * GIB;
        s.inputs[Consumer::PartCache as usize].minimum = 2 * GIB;
        s.inputs[Consumer::PartCache as usize].reused = Some([0; 3]);
        let (p, st) = settle(&mut s, PolicyState::default(), 40);
        assert!(at(&p, Consumer::PartCache) >= 2 * GIB);
        assert!(at(&p, Consumer::SharedBuffers) >= 2 * GIB);
        assert_eq!(st.slots[Consumer::SharedBuffers as usize].last_dir, 0, "6a: shared_buffers has no re-use signal and never moves");
    }

    /// Run on a box: `MEMBROKER_REPLAY_TSV=<file> cargo test -p mempolicy --lib replay -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn replay_tsv() {
        let Some(path) = std::env::var_os("MEMBROKER_REPLAY_TSV") else { return };
        let text = std::fs::read_to_string(path).expect("read the TSV");
        let mut col: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut samples: Vec<Signals> = Vec::new(); // alloc: Cold
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.first() == Some(&"sampled_at") {
                col = f.iter().enumerate().map(|(i, n)| (n.to_string(), i)).collect(); // alloc: Cold
                continue;
            }
            let get = |n: &str| col.get(n).and_then(|&i| f.get(i)).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).max(0) as u64;
            let name = col.get("consumer").and_then(|&i| f.get(i)).copied().unwrap_or("");
            let Some(c) = Consumer::ALL.into_iter().find(|c| c.name() == name) else { continue };
            let now = get("sampled_at");
            if samples.last().is_none_or(|s| s.now != now) {
                samples.push(Signals {
                    now,
                    b: get("b_bytes"),
                    r_high: get("r_high_bytes"),
                    fixed: get("fixed_bytes"),
                    work_demand: get("work_demand_bytes"),
                    queued: get("queued_bytes"),
                    inputs: [Input::default(); CONSUMERS],
                    pool_on_tick: false,
                    value: false,
                });
            }
            let s = samples.last_mut().expect("pushed");
            let reused = matches!(c, Consumer::PartCache | Consumer::FpTables).then(|| [get("reused_60s"), get("reused_300s"), get("reused_900s")]);
            let floor = get("floor_bytes");
            let minimum = if c == Consumer::SharedBuffers { floor } else { 0 };
            s.inputs[c as usize] = Input { registered: true, current: get("current_bytes"), floor, cap: get("cap_bytes"), reused, reused_evicted: false, minimum, doors: None };
        }
        println!("replay: {} intervals", samples.len());
        for mult in [100u64, 110, 125] {
            for band in [128 * MIB, 256 * MIB, 512 * MIB] {
                let cfg = PolicyCfg { multiplier_pct: mult, dead_band_min: band, ..PolicyCfg::DEFAULT };
                let mut st = PolicyState::default();
                let (mut moves, mut frozen) = ([0u32; CONSUMERS], [0u32; CONSUMERS]);
                let mut last = Plan::default();
                for s in &samples {
                    let (p, next) = plan(&st, s, &cfg);
                    for c in Consumer::ALL {
                        moves[c as usize] += u32::from(p.moved[c as usize] != 0);
                        frozen[c as usize] += u32::from(p.hold[c as usize] == Hold::Frozen);
                    }
                    st = next;
                    last = p;
                }
                for c in Consumer::ALL.into_iter().filter(|c| last.targeted[*c as usize]) {
                    let k = c as usize;
                    println!(
                        "mult {mult} band_mib {} {}: moves {} reversals {} frozen_intervals {} last_target_mib {}",
                        band >> 20,
                        c.name(),
                        moves[k],
                        st.slots[k].reversals,
                        frozen[k],
                        last.target[k] >> 20
                    );
                }
            }
        }
    }

    #[test]
    fn settled_targets_hold_still() {
        let mut s = signals();
        s.inputs[Consumer::PartCache as usize].reused = Some([3 * GIB; 3]);
        s.inputs[Consumer::FpTables as usize].reused = Some([GIB; 3]);
        let (_, st) = settle(&mut s, PolicyState::default(), 40);
        s.now += 30;
        let (p, st2) = run(&st, &s);
        assert!(p.moved.iter().all(|m| *m == 0), "{:?}", p.hold);
        assert_eq!(st2.slots, st.slots);
    }
}
