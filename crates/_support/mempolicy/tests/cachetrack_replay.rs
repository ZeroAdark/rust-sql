//! cache_track validation (charter job 5): replays one cache's trace from lane cachemeasure (cmtrace.py's libCacheSim
//! csv, `time,obj_id,obj_size,ev,refill_ns,sub,tid`) through `curve::Tracker` the way the doors and the drain feed it
//! (the doors' sizes, the drain's rate limit), and prints the predicted extra seconds of refill at 50 / 25 / 10 % of
//! the cache's size, beside the same replay with every key kept (the exact distances) and without the policy
//! correction. The whole trace is one window (the tracker's clock stays at 0, so H reads ns over the run).
//!
//! Ignored; by hand on a box:
//!   CACHETRACK_TRACE=<csv> CACHETRACK_SIZE=<bytes at 100 %> [CACHETRACK_UNSIZED=1] [CACHETRACK_SUB=1] \
//!   cargo test --release -p mempolicy --test cachetrack_replay -- --ignored --nocapture
//! UNSIZED: the door passes 0 bytes (fp tables, zone store, condition cache); SUB: the door's key is (obj, sub) (the
//! condition cache's per-granule door); OUTCOMES=1: the tracker bins each reference by its outcome
//! (`track::outcomes`: fp tables, zone store, condition cache); WIN="FROM TO": count only data rows FROM..TO (one hot
//! pass; the rows before warm the sample); COUNT=1: every miss priced 1 (extra misses instead of seconds).

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};

use mempolicy::curve::{door_pass, hash24, Tracker, SAMPLES, UNIFORM};
use mempolicy::track::RATE;

/// CACHETRACK_COUNT=1: every miss priced 1 (the curve counts misses).
fn count() -> bool {
    std::env::var("CACHETRACK_COUNT").is_ok_and(|v| v == "1")
}

struct Row {
    time: u64,
    key: u64,
    size: u64,
    ev: u8,
    refill: u64,
}

fn rows(path: &str, sub: bool, mut each: impl FnMut(usize, &Row)) {
    let f = BufReader::with_capacity(1 << 20, File::open(path).expect("CACHETRACK_TRACE"));
    for (i, l) in f.lines().skip(1).enumerate() {
        let l = l.expect("read");
        let p: Vec<&str> = l.split(',').collect();
        if p.len() < 5 {
            continue;
        }
        let num = |s: &str| s.parse::<u64>().unwrap_or(0);
        let mut key = num(p[1]);
        if sub {
            key = key.rotate_left(17) ^ num(p.get(5).copied().unwrap_or("0")).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        each(i, &Row { time: num(p[0]), key, size: num(p[2]), ev: p[3].bytes().next().unwrap_or(b'?'), refill: num(p[4]) });
    }
}

/// One replay: `cap` keys at most, the drain's rate limit when `limit`.
struct Replay {
    t: Tracker,
    limit: bool,
    since: Option<u64>,
    got: u64,
}

impl Replay {
    fn new(cap: usize, size: u64, limit: bool) -> Replay {
        let mut t = Tracker::new(cap).expect("tracker");
        t.set_size(size);
        if count() {
            t.seed_price(1);
        } else if let Some(ns) = std::env::var("CACHETRACK_SEED").ok().and_then(|v| v.parse().ok()) {
            // CACHETRACK_SEED=<ns>: the cache's price until a re-fill (track::measured_ns)
            t.seed_price(ns);
        }
        t.set_outcomes(std::env::var("CACHETRACK_OUTCOMES").is_ok_and(|v| v == "1"));
        Replay { t, limit, since: None, got: 0 }
    }

    fn feed(&mut self, r: &Row, no_size: bool) {
        let bytes = if no_size { 0 } else { r.size };
        match r.ev {
            b'H' | b'A' => {
                if self.limit {
                    let since = *self.since.get_or_insert(r.time);
                    if r.time >= since + 1_000_000_000 {
                        let want = RATE as f64 * (r.time - since) as f64 / 1e9;
                        if self.got as f64 > want {
                            let f = (want / self.got as f64).clamp(1.0 / 16.0, 1.0);
                            let d = (self.t.door() as f64 * f) as u64;
                            self.t.lower_door(d);
                        }
                        self.got = 0;
                        self.since = Some(r.time);
                    }
                    if door_pass(hash24(r.key), if bytes == 0 { UNIFORM } else { bytes }, self.t.door()) {
                        self.got += 1;
                    }
                }
                self.t.reference(r.key, bytes, 0);
                if r.ev == b'A' && r.refill > 0 && !count() {
                    self.t.fill(r.key, r.size, r.refill, 0);
                }
            }
            b'F' => self.t.fill(r.key, r.size, if count() { 0 } else { r.refill }, 0),
            _ => {}
        }
    }

    /// One line: the predicted extra seconds of refill over the run at 50 / 25 / 10 / 0 % of `size`, by the LRU model
    /// (today's policy, `track::policy`) and with the S3-FIFO correction, each raw and on the hull the broker reads.
    fn report(&mut self, name: &str, size: u64) {
        self.t.settle_all();
        let (lru, s3) = (self.t.curve_with(false), self.t.curve());
        let at = [size / 2, size / 4, size / 10, 0];
        let s = |x: f64| if count() { x } else { x / 1e9 };
        let raw = |pol: bool| at.map(|x| s(self.t.loss_at(x, pol)));
        let hull = |c: &mempolicy::curve::Curve| at.map(|x| s(c.at(x)));
        println!(
            "  {name}: samples {} refs {} door {} | extra s at 50/25/10/0 %: LRU raw {:.3?} hull {:.3?} knee {} B | S3-FIFO raw {:.3?} hull {:.3?} kappa {:.3?}",
            lru.samples,
            lru.refs,
            lru.door,
            raw(false),
            hull(&lru),
            lru.knee,
            raw(true),
            hull(&s3),
            s3.kappa
        );
    }
}

#[test]
#[ignore = "by hand: needs CACHETRACK_TRACE and CACHETRACK_SIZE"]
fn replay_a_cachemeasure_trace() {
    let path = std::env::var("CACHETRACK_TRACE").expect("CACHETRACK_TRACE=<csv>");
    let size: u64 = std::env::var("CACHETRACK_SIZE").expect("CACHETRACK_SIZE=<bytes>").parse().expect("bytes");
    let flag = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
    let (no_size, sub) = (flag("CACHETRACK_UNSIZED"), flag("CACHETRACK_SUB"));
    let (mut keys, mut n, mut ev, mut refill, mut t0, mut t1) = (HashSet::new(), 0u64, [0u64; 3], 0u64, u64::MAX, 0u64);
    rows(&path, sub, |_, r| {
        n += 1;
        match r.ev {
            b'H' => ev[0] += 1,
            b'A' => ev[1] += 1,
            b'F' => ev[2] += 1,
            _ => {}
        }
        if r.ev == b'F' || r.ev == b'A' {
            refill += r.refill;
        }
        if r.ev == b'H' || r.ev == b'A' {
            keys.insert(r.key);
            t0 = t0.min(r.time);
            t1 = t1.max(r.time);
        }
    });
    let span = t1.saturating_sub(t0) as f64 / 1e9;
    println!(
        "replay {path}: rows {n} (H {} A {} F {}), keys {}, span {span:.1} s, refill sum {:.3} s, size {size} B, unsized {no_size} sub {sub}",
        ev[0],
        ev[1],
        ev[2],
        keys.len(),
        refill as f64 / 1e9
    );
    let mut live = Replay::new(SAMPLES, size, true);
    let mut fixed = Replay::new(SAMPLES, size, false);
    let mut exact = Replay::new(keys.len() + 16, size, false);
    // CACHETRACK_FRAC=f: every key kept, but the door cut to a share f of the keys (an unsized door's uniform sample)
    let frac: Option<f64> = std::env::var("CACHETRACK_FRAC").ok().and_then(|v| v.parse().ok());
    let mut cut = frac.map(|f| {
        let mut r = Replay::new(keys.len() + 16, size, false);
        r.t.lower_door((f * (1u64 << 24) as f64) as u64);
        r
    });
    drop(keys);
    // CACHETRACK_WIN="FROM TO": the window (data rows FROM..TO) whose losses are counted; the rows before it warm the sample
    let win: Option<(usize, usize)> = std::env::var("CACHETRACK_WIN").ok().and_then(|v| {
        let mut it = v.split_whitespace().map(|x| x.parse::<usize>().ok());
        Some((it.next()??, it.next()??))
    });
    rows(&path, sub, |i, r| {
        if let Some((from, to)) = win {
            if i >= to {
                return;
            }
            if i == from {
                for t in [&mut live, &mut fixed, &mut exact] {
                    t.t.restart_window();
                }
                if let Some(c) = cut.as_mut() {
                    c.t.restart_window();
                }
            }
        }
        live.feed(r, no_size);
        fixed.feed(r, no_size);
        exact.feed(r, no_size);
        if let Some(c) = cut.as_mut() {
            c.feed(r, no_size);
        }
    });
    live.report("live (SAMPLES + rate limit)", size);
    fixed.report("fixed (SAMPLES, no rate limit)", size);
    exact.report("exact (every key)", size);
    if let Some(c) = cut.as_mut() {
        c.report("cut (every key, door at CACHETRACK_FRAC)", size);
    }
}
