//! cache_track (cache plan C2): the background drain of the doors' samples (`memledger::value`) into one [`Tracker`]
//! per keyed cache, the thresholds it publishes back to the doors, and the curves `pgrust_memory_cache_curves()` reads.
//! It runs every governor sample (100 ms) on the memory watchdog's sampler thread (`memruntime`), at the default config:
//! each tick feeds the doors' samples to the trackers, so a fixed-size sample lowers its door within a tick of filling;
//! the rate limit ([`RATE`]) is checked once a second. With the `cache_track` knob 0 (the default) it holds nothing and
//! every door reads a 0 threshold. Nothing decides by the curves yet.

use memledger::value::{self, Sample, FILL, MARK, REF, RELOAD};
use memledger::Consumer;

use crate::consts::{self, K};
use crate::curve::{Curve, Policy, Tracker, DOOR_ALL, SAMPLES, UNIFORM};

/// The caches with a keyed door, in the view's order.
pub const TRACKED: [Consumer; 6] =
    [Consumer::SharedBuffers, Consumer::PartCache, Consumer::FpTables, Consumer::ZoneStore, Consumer::CondCache, Consumer::CatalogL2];

/// The hand-over buffer, in samples (the drain keeps a second one to swap with).
pub const SHARED_CAP: usize = 1 << 15;

/// Sampled references a second one cache's door may send; past it the drain lowers that cache's threshold. CHOSEN: a
/// 1-min histogram wants a few thousand references; 1,000 a second for 6 caches at ~0.3 us each is ~0.2 % of a core, and
/// a second's worth is a fifth of the hand-over buffer.
pub const RATE: u64 = 1_000;

/// A cache's sample (keys) and rate (sampled references a second): the part cache's are 4x the others'. MEASURED (A6
/// validation, the analytic suite at W0, 10-16): at [`SAMPLES`] / [`RATE`] its door kept ~650-750 keys and two servers' predictions of the
/// same pass differed by ~20 %, as wide as criterion 9's bar; its entries span three orders of size (a granule image to a
/// decoded dictionary), so its byte-weighted sample needs more keys for the same error. Cost: ~3,000 more references a
/// second through the drain (~1 ms of the sampler's second) and a larger tracker (its own_bytes in the view).
pub fn samples(c: Consumer) -> usize {
    match c {
        Consumer::PartCache => 4 * SAMPLES,
        _ => SAMPLES,
    }
}

/// [`samples`]' rate.
pub fn rate(c: Consumer) -> u64 {
    match c {
        Consumer::PartCache => 4 * RATE,
        _ => RATE,
    }
}

/// A cache's price per miss (stall ns) until its tracker sees a re-fill (`curve::Tracker::fill`; broker ruling 10-16):
/// MEASURED, cache plan C0's cost per extra miss (reports/cache-c0: the analytic suite, W0, at 50 %, the hot pass's wall over the 100 %
/// arm's divided by its extra misses -- part 11.84 s / 63,967, fp tables 1.08 s / 3,446, condition cache 2.32 s /
/// 124,899; shared_buffers from pgbench select-only at 16 clients, 50 %; the zone store's is in the noise: 0). `None`: no
/// C0 figure (the catalog L2): its door's timer mean stands in.
pub fn measured_ns(c: Consumer) -> Option<u64> {
    match c {
        Consumer::PartCache => Some(185_000),
        Consumer::FpTables => Some(314_000),
        Consumer::CondCache => Some(18_600),
        Consumer::SharedBuffers => Some(9_400),
        Consumer::ZoneStore => Some(0),
        _ => None,
    }
}

/// A6: a cache's price per byte a miss reloads (ns), for a key with no re-fill stall measured yet: MEASURED, C0's
/// slowdown per missed byte (the analytic suite at W0, the hot pass's extra wall over its extra miss bytes: part cache 11.84 s / 15.2 GiB,
/// 15.75 s / 19.9 GiB, 19.70 s / 25.1 GiB at 50 / 25 / 10 % = 0.73 ns a byte at each). The part cache's misses differ in
/// size by three orders (a granule image to a whole decoded dictionary, a few blocks of which a statement reads back), so
/// a flat price per miss ([`measured_ns`]) misprices them; the others' entries are near one size. `None`: per miss.
pub fn measured_ns_per_byte(c: Consumer) -> Option<f64> {
    match c {
        Consumer::PartCache => Some(0.73),
        _ => None,
    }
}

/// Whether a cache's curve passed C2's validation against C0's measured misses (within +-20 % at 50 / 25 / 10 % of its
/// size; reports/cache-c0, lanes/cachetrack STATUS 10-16): fp tables, the condition cache, the zone store and
/// shared_buffers did. The part cache's did not (the hot passes read most of its bytes through paths its doors do not
/// see yet; the C2 follow-up), and the catalog L2 has no C0 figure. An unvalidated curve is shown, never ranked: its
/// cache ranks by [`standin`].
pub fn validated(c: Consumer) -> bool {
    matches!(c, Consumer::FpTables | Consumer::CondCache | Consumer::ZoneStore | Consumer::SharedBuffers)
}

/// The fixed curve an unvalidated cache ranks by (broker ruling 10-16): C0's measured slowdown, the analytic suite at W0, as (fraction
/// of the no-pressure size, loss ns/s = the hot pass's extra wall over the 100 % arm's wall): part cache 15.37 GB, hot
/// pass 13.133 s, +11.84 / +15.75 / +19.70 s at 50 / 25 / 10 %. `None`: no C0 figure.
pub fn standin(c: Consumer) -> Option<&'static [(f64, f64)]> {
    const PART: [(f64, f64); 4] = [(1.0, 0.0), (0.5, 0.9015e9), (0.25, 1.1994e9), (0.1, 1.5001e9)];
    match c {
        Consumer::PartCache => Some(&PART),
        _ => None,
    }
}

/// The switch's count mode (`cache_track` = 2): every miss priced 1 and fills never price, so a curve's losses are
/// misses (a validation against measured miss counts).
pub const COUNT: u64 = 2;

/// The switch's bytes mode (`cache_track` = 3): every miss priced the bytes it reloads (its key's last read-back, A6; else
/// its entry's bytes) and fills never price, so a curve's losses are the bytes a cache that size would read back (a
/// validation against measured miss bytes: C0's part cache counts each decoded dictionary block it faults as a miss).
pub const BYTES: u64 = 3;

/// The eviction policy each tracked cache runs today, which its curve's correction simulates: LRU (the part cache's
/// janitor, fp tables, the condition cache) or read as LRU (shared_buffers' clock sweep, the zone store's and the catalog
/// L2's budget eviction). A cache moves to [`Policy::S3Fifo`] here when cache plan C3 lands S3-FIFO in it. D4889: the
/// condition cache under `cache_policy` = 2 runs W-TinyLFU ([`Policy::TinyLfu`], `cache::tinylfu_on`; read when the
/// drain starts its trackers).
pub fn policy(c: Consumer) -> Policy {
    match c {
        Consumer::CondCache if crate::cache::tinylfu_on() => Policy::TinyLfu,
        Consumer::SharedBuffers | Consumer::PartCache | Consumer::FpTables | Consumer::ZoneStore | Consumer::CondCache | Consumer::CatalogL2 => {
            Policy::Lru
        }
        _ => Policy::S3Fifo,
    }
}

/// Whether a tracked cache's timed fills carry the key its door referenced, so a sampled reference's outcome (hit, or a
/// miss and its fill) is known: fp tables, the zone store, the condition cache and the catalog L2 do. The part cache's
/// fills carry its part's key and its door the segment's; shared_buffers times no fill.
pub fn outcomes(c: Consumer) -> bool {
    matches!(c, Consumer::FpTables | Consumer::ZoneStore | Consumer::CondCache | Consumer::CatalogL2)
}

/// The drain's state: built, drained and dropped on the drain's thread only ([`tick`], the memory watchdog's sampler,
/// whose account its arrays are charged to and freed from); [`curves`] only reads it.
struct State {
    trackers: Vec<Tracker>,
    buf: Vec<Sample>,
    /// The batch time of the samples being read (the last mark's).
    at: u64,
    counts: [u64; TRACKED.len()],
    /// Each cache's last rate check (`pg_clock::mono_ns`): its `counts` and `min_dsize` are since then.
    since: [u64; TRACKED.len()],
    /// Whether each cache's door was rate-cut since the drain started (the early check runs only before the first).
    cut: [bool; TRACKED.len()],
    dropped: u64,
    /// The switch at [`COUNT`] or [`BYTES`]: losses are miss counts or miss bytes, fills never price.
    count: bool,
    /// The smallest door size (bytes, [`UNIFORM`] for a sizeless lookup) each cache's sampled references carried since
    /// its last rate check (`u64::MAX`: none): the door at or above [`all_pass`] of it passes every such reference.
    min_dsize: [u64; TRACKED.len()],
    /// The (threshold, largest sampled entry) each door was last published at: published again only when it moves.
    published: [(u64, u64); TRACKED.len()],
}

static STATE: pgsync::Mutex<Option<State>> = pgsync::Mutex::new(None);

fn index(c: u8) -> Option<usize> {
    TRACKED.iter().position(|t| *t as u8 == c)
}

/// One drain, now: the switch read, the samples fed, the sizes and prices refreshed, the thresholds published.
pub fn tick() {
    tick_at(value::track_now(), |c| memledger::hooks(c).map_or(0, |h| (h.current)()));
}

/// [`tick`] at `now_ns` (`pg_clock::mono_ns`) with each cache's size from `size`.
pub fn tick_at(now_ns: u64, size: impl Fn(Consumer) -> u64) {
    let mode = consts::get(K::CacheTrack);
    let on = mode != 0;
    let mut g = pgsync::lock(&STATE);
    if !on {
        if g.take().is_some() {
            value::track_stop();
        }
        return;
    }
    if g.is_none() {
        *g = start(now_ns, mode);
    }
    if let Some(st) = g.as_mut() {
        st.drain(now_ns, &size);
    }
}

/// Every array allocated here, on the drain's thread (its account's, by construction), once.
fn start(now: u64, mode: u64) -> Option<State> {
    let mut trackers = Vec::new();
    trackers.try_reserve_exact(TRACKED.len()).ok()?;
    for c in TRACKED {
        trackers.push(Tracker::new(samples(c)).ok()?);
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(SHARED_CAP).ok()?;
    if !value::track_start(SHARED_CAP) {
        return None;
    }
    for (c, t) in TRACKED.iter().zip(trackers.iter_mut()) {
        t.set_policy(policy(*c));
        t.set_outcomes(outcomes(*c));
        t.set_price_bytes(mode == BYTES);
        t.set_ns_per_byte(measured_ns_per_byte(*c).unwrap_or(0.0));
        value::track_set(*c, t.door(), 0);
    }
    let count = mode == COUNT || mode == BYTES;
    let published = core::array::from_fn(|i| (trackers[i].door(), 0));
    Some(State {
        trackers,
        buf,
        at: now,
        counts: [0; TRACKED.len()],
        since: [now; TRACKED.len()],
        cut: [false; TRACKED.len()],
        dropped: value::track_dropped(),
        count,
        min_dsize: [u64::MAX; TRACKED.len()],
        published,
    })
}

impl State {
    fn drain(&mut self, now: u64, size: &impl Fn(Consumer) -> u64) {
        if !value::track_take(&mut self.buf) {
            return;
        }
        for s in &self.buf {
            if s.kind == MARK {
                self.at = s.key;
                continue;
            }
            let Some(i) = index(s.c) else { continue };
            let t = &mut self.trackers[i];
            match s.kind {
                REF => {
                    t.reference_hashed(s.key, s.h, u64::from(s.bytes), self.at);
                    self.counts[i] += 1;
                    let d = if s.bytes == 0 { UNIFORM } else { u64::from(s.bytes) };
                    self.min_dsize[i] = self.min_dsize[i].min(d);
                }
                FILL => t.fill(s.key, u64::from(s.bytes), if self.count { 0 } else { u64::from(s.ns) }, self.at),
                RELOAD => t.reload(s.key, u64::from(s.bytes), self.at),
                _ => {}
            }
        }
        let dropped = value::track_dropped();
        let flooded = dropped > self.dropped;
        self.dropped = dropped;
        for (i, &c) in TRACKED.iter().enumerate() {
            let dt = now.saturating_sub(self.since[i]);
            let due = dt >= 1_000_000_000 || flooded;
            let t = &mut self.trackers[i];
            t.set_size(size(c));
            let st = value::stats(c);
            t.seed_price(if self.count {
                1
            } else {
                measured_ns(c).unwrap_or(if st.cost_samples > 0 { st.cost_ns / st.cost_samples } else { 20_000 })
            });
            t.tick(now);
            // D4892: before a door's first cut, a tick's window (>= EARLY_NS) that holds far more than the rate (and more
            // than a statement end's hand-over burst) cuts it at once, leaving 4x headroom; the once-a-second check does
            // the rest, so a fresh server samples every lookup for ~a tick, not for its first seconds
            let f = if due {
                rate_cut(rate(c), self.counts[i], dt, flooded)
            } else if !self.cut[i] && dt >= EARLY_NS {
                early_cut(rate(c), self.counts[i], dt)
            } else {
                continue_publish(&mut self.published[i], c, t);
                continue;
            };
            if let Some(f) = f {
                // cut from the least door that still passes every reference this cache sent, not from a door above it
                // (the start's DOOR_ALL is 2^16 x a sizeless door's), so the first cut thins the sample
                let base = t.door().min(all_pass(self.min_dsize[i]));
                t.lower_door((base as f64 * f) as u64);
                self.cut[i] = true;
            }
            if due || f.is_some() {
                self.counts[i] = 0;
                self.min_dsize[i] = u64::MAX;
                self.since[i] = now;
            }
            continue_publish(&mut self.published[i], c, t);
        }
    }
}

/// A door published only when it moves (the doors' read-mostly line is written no more often than before).
fn continue_publish(published: &mut (u64, u64), c: Consumer, t: &Tracker) {
    let p = (t.door(), t.max_size());
    if p != *published {
        value::track_set(c, p.0, p.1);
        *published = p;
    }
}

/// The shortest window the early check reads (one governor tick).
const EARLY_NS: u64 = 100_000_000;

/// The most samples a statement end can hand over at once without any door being over its rate: a ring a thread, for up
/// to 32 threads. The early check never cuts on fewer than this past 4x the rate.
const EARLY_BURST: u64 = 32 * value::RING as u64;

/// The early check's factor (D4892): after `got` sampled references in `dt` ns (< 1 s), only when `got` is past 4x the rate
/// plus [`EARLY_BURST`], down to that level (4x headroom, at most 16x a step); else none.
fn early_cut(rate: u64, got: u64, dt: u64) -> Option<f64> {
    let floor = 4.0 * rate as f64 * dt as f64 / 1e9 + EARLY_BURST as f64;
    (got as f64 > floor).then(|| (floor / got as f64).clamp(1.0 / 16.0, 1.0))
}

/// The least door that passes every lookup of `dsize` bytes or more ([`crate::curve::door_pass`]: `hash24 << 16 < t * dsize` for
/// every 24-bit hash), [`DOOR_ALL`] for none seen.
fn all_pass(dsize: u64) -> u64 {
    if dsize == u64::MAX || dsize == 0 {
        return DOOR_ALL;
    }
    DOOR_ALL.div_ceil(dsize).min(DOOR_ALL)
}

/// The factor a cache's threshold is cut by after `got` sampled references in `dt` ns (a flood: the hand-over buffer
/// dropped batches, so `got` undercounts): at most 16x a step, so the doors converge in a few wakes.
fn rate_cut(rate: u64, got: u64, dt: u64, flooded: bool) -> Option<f64> {
    let want = (rate as f64 * dt.max(1) as f64 / 1e9).max(1.0);
    let over = got as f64 > want || (flooded && got as f64 > want / 4.0);
    over.then(|| (want / got as f64).clamp(1.0 / 16.0, 1.0) * if flooded { 0.5 } else { 1.0 })
}

/// One tracked cache's curve and its door's state, as the view shows it.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub cache: Consumer,
    pub curve: Curve,
    pub door: u64,
    /// Its tracker's heap bytes.
    pub bytes: u64,
    /// [`validated`]: the curve ranks; else its cache ranks by [`standin`].
    pub validated: bool,
}

/// The tracked caches' curves now (`None`: the switch is off or the drain has not started).
pub fn curves() -> Option<[Row; TRACKED.len()]> {
    let g = pgsync::lock(&STATE);
    let st = g.as_ref()?;
    Some(core::array::from_fn(|i| Row {
        cache: TRACKED[i],
        curve: st.trackers[i].curve(),
        door: value::track_of(TRACKED[i]),
        bytes: st.trackers[i].heap_bytes(),
        validated: validated(TRACKED[i]),
    }))
}

/// The drain's own bytes: every tracker and both hand-over buffers (0 while off). They sit on the memory watchdog's
/// thread's account (unbound by construction); this names their share of it.
pub fn own_bytes() -> u64 {
    let g = pgsync::lock(&STATE);
    let Some(st) = g.as_ref() else { return 0 };
    let buf = (st.buf.capacity() * core::mem::size_of::<Sample>()) as u64;
    st.trackers.iter().map(Tracker::heap_bytes).sum::<u64>() + buf + value::track_buffer_bytes()
}

/// Samples the hand-over buffer dropped so far.
pub fn dropped() -> u64 {
    value::track_dropped()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4889: the condition cache under `cache_policy` = 2 runs W-TinyLFU, and its tracker simulates that, not LRU.
    #[test]
    fn d4889_the_condition_cache_is_tracked_as_the_policy_it_runs() {
        consts::set(K::CachePolicy, 2);
        assert_eq!(policy(Consumer::CondCache), Policy::TinyLfu);
        assert_eq!(policy(Consumer::FpTables), Policy::Lru, "the fp tables stay lru at every value");
        consts::set(K::CachePolicy, 0);
        assert_eq!(policy(Consumer::CondCache), Policy::Lru);
    }

    #[test]
    fn the_rate_cut_bounds_each_step() {
        let s = 1_000_000_000;
        assert_eq!(rate_cut(RATE, RATE, s, false), None, "at the rate: no cut");
        assert_eq!(rate_cut(RATE, 2 * RATE, s, false), Some(0.5));
        assert_eq!(rate_cut(RATE, 1000 * RATE, s, false), Some(1.0 / 16.0), "at most 16x a step");
        assert_eq!(rate_cut(RATE, RATE / 2, s, true), Some(0.5), "a flood halves a door that sent a share");
        assert_eq!(rate_cut(rate(Consumer::PartCache), 2 * RATE, s, false), None, "the part cache's own rate");
        assert_eq!(rate_cut(RATE, 1, s, true), None);
    }

    /// The drain end to end: off holds nothing; on, the doors' samples reach the tracker and a curve comes out; off again
    /// frees it and the doors read 0.
    #[test]
    fn the_switch_starts_and_stops_the_drain() {
        let t0 = value::track_now();
        consts::set(K::CacheTrack, 0);
        tick_at(t0, |_| 0);
        assert!(curves().is_none());
        consts::set(K::CacheTrack, 1);
        tick_at(t0, |_| 64 << 20);
        let c = Consumer::FpTables;
        assert_ne!(value::track_of(c), 0, "the door is open");
        assert_eq!(value::track_of(c), DOOR_ALL, "it opens passing every lookup");
        let z = Consumer::ZoneStore;
        for r in 0..20_000u64 {
            value::lookup(c, r % 512, 0);
            if r % 10 == 0 {
                value::lookup(z, r % 97, 0);
            }
            if r % 64 == 63 {
                value::flush();
            }
        }
        value::flush();
        // D4892: a tick under a second feeds the samples; before a door's first cut, a window far over its rate (20,000
        // references in 100 ms against 100 wanted) cuts it at once to 4x the rate plus a hand-over burst, from the least
        // door that passes every sizeless lookup (2^24), not from DOOR_ALL (2^40)
        tick_at(t0 + 100_000_000, |_| 64 << 20);
        let fed = curves().expect("on").iter().find(|r| r.cache == c).unwrap().curve.refs;
        assert!(fed > 0, "fed at 100 ms: {fed}");
        assert_eq!(all_pass(UNIFORM), 1 << 24);
        let early = (all_pass(UNIFORM) as f64 * early_cut(RATE, 20_000, 100_000_000).expect("far over")) as u64;
        assert!(early < all_pass(UNIFORM) / 8 && early > all_pass(UNIFORM) / 16, "{early}");
        assert_eq!(value::track_of(c), early, "the early cut");
        // 2,000 references are under 4 x 100 + a burst: no early cut; the second's check cuts them to the rate
        assert_eq!(value::track_of(z), DOOR_ALL, "a burst never cuts early");
        tick_at(t0 + 1_100_000_000, |_| 64 << 20);
        assert_eq!(value::track_of(c), early, "nothing more came: the second's check keeps it");
        let second = (all_pass(UNIFORM) as f64 * rate_cut(RATE, 2_000, 1_100_000_000, false).expect("over")) as u64;
        assert_eq!(value::track_of(z), second, "one cut into the doors' range");
        assert_eq!(early_cut(RATE, 2_000, 100_000_000), None, "under 4x the rate plus a burst");
        tick_at(t0 + 2_000_000_000, |_| 64 << 20);
        let rows = curves().expect("on");
        let own = own_bytes();
        assert!(own >= 2 * (SHARED_CAP * core::mem::size_of::<Sample>()) as u64 && own < 16 << 20, "own bytes {own}");
        let row = rows.iter().find(|r| r.cache == c).unwrap();
        assert!(row.curve.samples > 0 && row.curve.refs > 0, "{:?}", (row.curve.samples, row.curve.refs));
        consts::set(K::CacheTrack, 0);
        tick_at(t0 + 3_000_000_000, |_| 0);
        assert!(curves().is_none());
        assert_eq!(own_bytes(), 0);
        assert_eq!(value::track_of(c), 0);
    }
}
