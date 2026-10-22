//! The host's cost profile: every price of this crate was measured on the reference machine (`machine::REFERENCE`,
//! c8g.4xlarge); a host measures a handful of anchor units once and each price moves by its anchor's ratio, host over
//! reference, so a decision that compares two prices of one anchor (a break-even, a crossover) holds on any host, and one
//! that compares prices of two anchors follows the host. The units are the cost home's machine units (gravity_program
//! tests/cost_fit.rs `machine_unit_costs_at_the_dev_seed`) and a sort: each a pure loop over data the caller builds once
//! (`Data`), timed by the caller (the driver, at server start: thread CPU time, the least of several takes, so a loaded
//! host reads as an idle one). A ratio within `SNAP` of 1 is exactly 1, any other is rounded to an eighth: run-to-run noise
//! never moves a decision near its threshold, and the reference machine prices exactly as the constants say. There is no
//! memory-bandwidth anchor: eight threads each reading 16 MiB read 0.16 .. 0.19 ns a row across server starts on one
//! c8g.4xlarge (the host's other tenants share the bandwidth), a spread past the snap; a width's prices follow their
//! one-thread anchor.
//!
//! The profile in use is installed once per process (`install`, the postmaster at start, before any backend); until then,
//! and in every process that never installs one (crate tests, IR pins, tools), every ratio is 1 (`current`). A host keeps
//! its anchors in a cache file keyed by `host_key`, which names this file's own text (`VERSION`): an edit to a unit
//! re-measures.

use crate::fit::Fitted;

/// The anchor units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Anchor {
    /// A sequential read of every row of an L2-resident lane, ns a row.
    Read,
    /// A gather of sorted random positions of that lane, ns a position.
    Gather,
    /// A compare of each row into mask words, ns a row.
    Check,
    /// A dependent load around a random cycle past the L2 (the miss latency), ns a load.
    Miss,
    /// A hashed probe and count into an L2-resident table, ns a key.
    Update,
    /// An unstable sort of random keys, ns a key.
    Sort,
}

impl Anchor {
    pub const ALL: [Anchor; 6] = [Anchor::Read, Anchor::Gather, Anchor::Check, Anchor::Miss, Anchor::Update, Anchor::Sort];

    pub const fn name(self) -> &'static str {
        match self {
            Anchor::Read => "read",
            Anchor::Gather => "gather",
            Anchor::Check => "check",
            Anchor::Miss => "miss",
            Anchor::Update => "update",
            Anchor::Sort => "sort",
        }
    }

    pub fn of(name: &str) -> Option<Anchor> {
        Anchor::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The reference machine's value of this unit.
    pub const fn reference(self) -> Fitted {
        REFERENCE[self as usize]
    }
}

const MACHINE: &str = "box gentopn: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU, held whole";
/// The anchors' own units on the reference machine (thread CPU time, the least of the server-start takes).
const CORPUS: &str = "the anchor units (anchor.rs Data at the dev seed), the least of 5 takes a unit; the median of 13 server starts";

/// Each anchor's value on the reference machine, in `Anchor` order.
pub const REFERENCE: [Fitted; 6] = [
    Fitted { ns: 0.090, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read },
    Fitted { ns: 0.548, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Gather },
    Fitted { ns: 0.179, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Check },
    Fitted { ns: 102.5, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Miss },
    Fitted { ns: 0.944, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update },
    Fitted { ns: 13.25, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Sort },
];

/// A ratio within this of 1 is 1.
pub const SNAP: f64 = 0.10;

/// `r` snapped: 1 within `SNAP` of 1, else the nearest eighth (at least one eighth); 1 for a ratio that is not a finite
/// positive number.
pub fn snap(r: f64) -> f64 {
    if !r.is_finite() || r <= 0.0 || (r - 1.0).abs() <= SNAP {
        return 1.0;
    }
    ((r * 8.0).round() / 8.0).max(0.125)
}

/// A host's measured anchors, ns in `Anchor` order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measured(pub [f64; 6]);

impl Measured {
    /// Each anchor's snapped ratio to the reference.
    pub fn ratios(&self) -> Ratios {
        let mut r = [1.0; 6];
        for a in Anchor::ALL {
            r[a as usize] = snap(self.0[a as usize] / a.reference().ns);
        }
        Ratios(r)
    }
}

/// The snapped ratios, host over reference, in `Anchor` order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ratios(pub [f64; 6]);

impl Ratios {
    pub const ONE: Ratios = Ratios([1.0; 6]);

    pub fn of(&self, a: Anchor) -> f64 {
        self.0[a as usize]
    }

    pub fn is_one(&self) -> bool {
        self.0.iter().all(|&r| r == 1.0)
    }
}

/// Where the profile in use came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// The reference: nothing installed, or the server was asked for it (`why`).
    Reference(&'static str),
    /// Read from the host's cache file.
    Cached(String),
    /// Measured at this start, in `micros`; kept in the cache file when it could be written (`kept`).
    Measured { micros: u64, kept: Option<String> },
}

/// The profile in use.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub key: String,
    pub measured: Option<Measured>,
    pub ratios: Ratios,
    pub source: Source,
}

impl Profile {
    pub const fn reference(why: &'static str) -> Profile {
        Profile { key: String::new(), measured: None, ratios: Ratios::ONE, source: Source::Reference(why) }
    }

    /// One line for the server log.
    pub fn describe(&self) -> String {
        let src = match &self.source {
            Source::Reference(why) => format!("reference ({why})"),
            Source::Cached(path) => format!("cached ({path})"),
            Source::Measured { micros, kept: Some(path) } => format!("measured in {} ms, kept in {path}", micros / 1000),
            Source::Measured { micros, kept: None } => format!("measured in {} ms, not kept (no writable cache directory)", micros / 1000),
        };
        let ratios: Vec<String> = Anchor::ALL.iter().map(|&a| format!("{} {}", a.name(), self.ratios.of(a))).collect(); // alloc: Cold
        let key = if self.key.is_empty() { String::new() } else { format!("; host {}", self.key) }; // alloc: Cold
        let ns = match &self.measured {
            Some(m) => format!("; anchors ns: {}", Anchor::ALL.iter().map(|&a| format!("{} {:.3}", a.name(), m.0[a as usize])).collect::<Vec<_>>().join(", ")), // alloc: Cold
            None => String::new(),
        };
        format!("cost profile: {src}{key}; ratios to the reference: {}{ns}", ratios.join(", ")) // alloc: Cold
    }
}

/// The profile a process asks for: `PGRUST_COST_PROFILE`, read once. `reference` keeps the reference machine's prices and
/// cache facts (`machine::host`), `host` this host's; unset (or neither word: `word_unknown`, the installer logs it) is
/// `host`, but `reference` in a debug build (its units run unoptimized); where no thread CPU clock times the units (wasm)
/// it is always `reference`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    Reference(&'static str),
    Host,
}

impl Word {
    pub fn is_reference(self) -> bool {
        matches!(self, Word::Reference(_))
    }
}

pub fn word() -> Word {
    word_read().0
}

/// `PGRUST_COST_PROFILE` holds neither word, so the default applies.
pub fn word_unknown() -> bool {
    word_read().1
}

fn word_read() -> (Word, bool) {
    static WORD: pgsync::OnceLock<(Word, bool)> = pgsync::OnceLock::new();
    *WORD.get_or_init(|| word_of(std::env::var("PGRUST_COST_PROFILE").ok().as_deref()))
}

fn word_of(v: Option<&str>) -> (Word, bool) {
    let v = v.map(str::trim);
    let unknown = v.is_some_and(|w| w != "reference" && w != "host");
    let w = match v {
        Some("reference") => Word::Reference("PGRUST_COST_PROFILE=reference"),
        _ if cfg!(target_family = "wasm") => Word::Reference("no thread CPU clock"),
        Some("host") => Word::Host,
        _ if cfg!(debug_assertions) => Word::Reference("a debug build"),
        _ => Word::Host,
    };
    (w, unknown)
}

static INSTALLED: pgsync::OnceLock<Profile> = pgsync::OnceLock::new();
static REFERENCE_PROFILE: Profile = Profile::reference("not installed");

/// Install this process's profile; the first install wins. False when one was installed already.
pub fn install(p: Profile) -> bool {
    INSTALLED.set(p).is_ok()
}

/// A profile was installed in this process.
pub fn installed() -> bool {
    INSTALLED.get().is_some()
}

/// The profile in use: the installed one, else the reference.
pub fn current() -> &'static Profile {
    INSTALLED.get().unwrap_or(&REFERENCE_PROFILE)
}

/// The ratios of the profile in use (`Ratios::ONE` where none is installed).
pub fn ratios() -> &'static Ratios {
    &current().ratios
}

/// The ratio of `a` in the profile in use.
pub fn ratio(a: Anchor) -> f64 {
    ratios().of(a)
}

/// This file's text, hashed (FNV-1a 64): a changed unit, data shape or reference moves every host's key.
pub const VERSION: u64 = fnv1a(include_bytes!("anchor.rs"));

const fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < b.len() {
        h ^= b[i] as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

/// The cache key of a host: its architecture and CPU id (`cpu`: aarch64's MIDR, x86's vendor / family / model / stepping,
/// "unknown" where neither reads), its cache facts and this file's `VERSION`. Only `[A-Za-z0-9._-]`.
pub fn host_key(arch: &str, cpu: &str, l1d: u64, l2: u64, smt: u32) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect::<String>();
    format!("{}-{}-l1d{}k-l2{}k-smt{}-v{:016x}", clean(arch), clean(cpu), l1d >> 10, l2 >> 10, smt, VERSION)
}

/// The cache file's text for `key` and `m`: a key line, then one `name<TAB>ns` line per anchor.
pub fn cache_text(key: &str, m: &Measured) -> String {
    let mut s = format!("key\t{key}\n");
    for a in Anchor::ALL {
        s.push_str(&format!("{}\t{:.6}\n", a.name(), m.0[a as usize]));
    }
    s
}

/// The anchors of a cache file's `text` when its key is `key` and every anchor is there, finite and positive.
pub fn parse_cache(text: &str, key: &str) -> Option<Measured> {
    let mut lines = text.lines();
    if lines.next()?.strip_prefix("key\t")? != key {
        return None;
    }
    let mut v = [f64::NAN; 6];
    for l in lines {
        let (name, ns) = l.split_once('\t')?;
        v[Anchor::of(name)? as usize] = ns.parse().ok()?;
    }
    v.iter().all(|x| x.is_finite() && *x > 0.0).then_some(Measured(v))
}

/// The data the units read, built once at the dev seed.
pub struct Data {
    lane: Vec<u32>,
    pos: Vec<u32>,
    mask: Vec<u64>,
    next: Vec<u32>,
    /// Where the miss cycle's next take starts: each take walks lines no take before it touched.
    at: u32,
    slots: Vec<u32>,
    keys: Vec<u32>,
    sort_src: Vec<u64>,
    sort_buf: Vec<u64>,
}

/// The read / gather / check lane: 64 Ki rows (256 KiB, inside every L2 of the boxes we run on), `PASSES` reads a take.
pub const LANE_ROWS: usize = 1 << 16;
pub const PASSES: usize = 8;
/// The miss cycle: one slot a 64-byte line over 1 Mi lines (64 MiB, past the LLC of a 16-vCPU box); `MISS_STEPS` loads a
/// take, each take going on round the cycle where the last one stopped (so no take reads a line an earlier one cached).
pub const MISS_LINES: usize = 1 << 20;
pub const MISS_STEPS: usize = 1 << 15;
const LINE: usize = 16;
/// The probe table: 64 Ki slots (256 KiB); `UPDATE_KEYS` keys a take.
pub const UPDATE_SLOTS: usize = 1 << 16;
pub const UPDATE_KEYS: usize = 1 << 18;
/// The sort: 64 Ki keys a take.
pub const SORT_KEYS: usize = 1 << 16;

const DEV_SEED: u64 = 0x5eed_0de7;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

impl Data {
    pub fn new() -> Data {
        let mut rng = Rng(DEV_SEED);
        let lane: Vec<u32> = (0..LANE_ROWS).map(|_| rng.next() as u32).collect(); // alloc: Cold (once a process: the anchors' data)
        let mut pos: Vec<u32> = (0..LANE_ROWS / 8).map(|_| (rng.next() % LANE_ROWS as u64) as u32).collect(); // alloc: Cold
        pos.sort_unstable();
        // one random cycle over the lines (Sattolo's shuffle), each line's first slot naming the next line's
        let mut order: Vec<u32> = (0..MISS_LINES as u32).collect(); // alloc: Cold
        for i in (1..MISS_LINES).rev() {
            order.swap(i, (rng.next() % i as u64) as usize);
        }
        let mut next = vec![0u32; MISS_LINES * LINE]; // alloc: Cold
        for k in 0..MISS_LINES {
            next[order[k] as usize * LINE] = order[(k + 1) % MISS_LINES] * LINE as u32;
        }
        let keys: Vec<u32> = (0..UPDATE_KEYS).map(|_| rng.next() as u32).collect(); // alloc: Cold
        let sort_src: Vec<u64> = (0..SORT_KEYS).map(|_| rng.next()).collect(); // alloc: Cold
        let (mask, slots, sort_buf) = (vec![0u64; LANE_ROWS / 64], vec![0u32; UPDATE_SLOTS], sort_src.clone()); // alloc: Cold
        Data { lane, pos, mask, next, at: 0, slots, keys, sort_src, sort_buf }
    }

    /// One take of `a`'s unit: the count of items it timed (rows, positions, loads, keys) beside a value the caller keeps
    /// alive (so the loop is not elided).
    pub fn run(&mut self, a: Anchor) -> (u64, u64) {
        match a {
            Anchor::Read => (0..PASSES).fold((0, 0), |(n, s), _| (n + LANE_ROWS as u64, s ^ read_all(&self.lane))),
            Anchor::Gather => (0..PASSES).fold((0, 0), |(n, s), _| (n + self.pos.len() as u64, s ^ gather(&self.lane, &self.pos))),
            Anchor::Check => (0..PASSES).fold((0, 0), |(n, s), p| (n + LANE_ROWS as u64, s ^ check(&self.lane, u32::MAX / 3 + p as u32, &mut self.mask))),
            Anchor::Miss => {
                self.at = chase(&self.next, self.at, MISS_STEPS);
                (MISS_STEPS as u64, self.at as u64)
            }
            Anchor::Update => (UPDATE_KEYS as u64, update(&mut self.slots, &self.keys)),
            Anchor::Sort => {
                self.sort_buf.copy_from_slice(&self.sort_src);
                self.sort_buf.sort_unstable();
                (SORT_KEYS as u64, self.sort_buf[SORT_KEYS / 2])
            }
        }
    }
}

impl Default for Data {
    fn default() -> Data {
        Data::new()
    }
}

fn read_all(lane: &[u32]) -> u64 {
    lane.iter().fold(0u64, |s, &v| s.wrapping_add(v as u64))
}

fn gather(lane: &[u32], pos: &[u32]) -> u64 {
    pos.iter().fold(0u64, |s, &p| s.wrapping_add(lane[p as usize] as u64))
}

fn check(lane: &[u32], c: u32, mask: &mut [u64]) -> u64 {
    for (j, m) in mask.iter_mut().enumerate() {
        let mut bits = 0u64;
        for (b, &v) in lane[j * 64..j * 64 + 64].iter().enumerate() {
            bits |= ((v < c) as u64) << b;
        }
        *m = bits;
    }
    mask[0]
}

fn chase(next: &[u32], from: u32, steps: usize) -> u32 {
    let mut i = from;
    for _ in 0..steps {
        i = next[i as usize];
    }
    i
}

fn update(slots: &mut [u32], keys: &[u32]) -> u64 {
    let m = slots.len() - 1;
    for &k in keys {
        let h = (k as usize).wrapping_mul(0x9e37_79b9) & m;
        slots[h] = slots[h].wrapping_add(1);
    }
    slots[0] as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ratio_near_one_is_one_and_any_other_an_eighth() {
        assert_eq!(snap(1.0), 1.0);
        assert_eq!(snap(1.09), 1.0, "noise");
        assert_eq!(snap(0.91), 1.0, "noise");
        assert_eq!(snap(1.12), 1.125);
        assert_eq!(snap(1.6), 1.625);
        assert_eq!(snap(2.4), 2.375);
        assert_eq!(snap(0.5), 0.5);
        assert_eq!(snap(0.01), 0.125, "at least an eighth");
        assert_eq!(snap(f64::NAN), 1.0);
        assert_eq!(snap(0.0), 1.0);
        let m = Measured(REFERENCE.map(|f| f.ns * 1.05));
        assert!(m.ratios().is_one(), "the reference machine, 5 % off, prices as the reference");
    }

    #[test]
    fn the_word_picks_the_profile() {
        assert_eq!(word_of(Some(" reference ")), (Word::Reference("PGRUST_COST_PROFILE=reference"), false));
        assert_eq!(word_of(Some("host")), (Word::Host, false));
        let default = if cfg!(debug_assertions) { Word::Reference("a debug build") } else { Word::Host };
        assert_eq!((word_of(None), word_of(Some("fast"))), ((default, false), (default, true)), "unset or neither word: the default");
    }

    #[test]
    fn nothing_installed_prices_as_the_reference() {
        assert!(ratios().is_one());
        assert!(REFERENCE.iter().all(|f| f.host_ns() == f.ns));
    }

    #[test]
    fn the_cache_file_round_trips_under_its_key_alone() {
        let key = host_key("aarch64", "0x410fd4f1", 64 << 10, 2 << 20, 1);
        assert!(key.ends_with(&format!("-v{VERSION:016x}")) && key.starts_with("aarch64-0x410fd4f1-l1d64k-l22048k-smt1"));
        let m = Measured([0.3, 1.2, 0.5, 120.0, 2.0, 11.0]);
        let text = cache_text(&key, &m);
        assert_eq!(parse_cache(&text, &key), Some(m));
        assert_eq!(parse_cache(&text, "another host"), None, "another key");
        assert_eq!(parse_cache(&text.replace("sort\t", "nope\t"), &key), None, "an unknown anchor");
        assert_eq!(parse_cache(&text.replace("\nmiss\t120.000000", ""), &key), None, "a missing anchor");
        assert_eq!(host_key("x86_64", "AuthenticAMD 25/1/1", 32 << 10, 512 << 10, 2).split('-').nth(1), Some("AuthenticAMD_25_1_1"));
    }

    /// This host's anchors beside the reference's (wall time, the least of 5 takes a unit): the server measures them in
    /// thread CPU time at start; this reads them without a server (`cargo test -p cost_profile --profile fast-profile --
    /// --ignored anchors_on_this_host --nocapture` on a quiet box).
    #[test]
    #[ignore = "a measurement: run on a quiet box with --ignored"]
    fn anchors_on_this_host() {
        let mut d = Data::new();
        let mut ns = [f64::INFINITY; 6];
        for a in Anchor::ALL {
            std::hint::black_box(d.run(a));
            for _ in 0..5 {
                let t0 = std::time::Instant::now();
                let (n, v) = d.run(a);
                let t = t0.elapsed().as_nanos() as f64;
                std::hint::black_box(v);
                ns[a as usize] = ns[a as usize].min(t / n as f64);
            }
        }
        let r = Measured(ns).ratios();
        for a in Anchor::ALL {
            println!("anchor\t{}\t{:.4}\treference {:.4}\tratio {:.3}\tsnapped {}", a.name(), ns[a as usize], a.reference().ns, ns[a as usize] / a.reference().ns, r.of(a));
        }
    }

    #[test]
    fn every_unit_runs_over_its_data() {
        let mut d = Data::new();
        for a in Anchor::ALL {
            assert_eq!(Anchor::of(a.name()), Some(a));
            assert!(d.run(a).0 > 0, "{a:?}");
        }
        let (a, b) = (d.run(Anchor::Miss), d.run(Anchor::Miss));
        assert_ne!(a, b, "a take goes on round the cycle");
    }
}
