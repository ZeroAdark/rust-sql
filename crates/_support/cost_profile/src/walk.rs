
use crate::anchor::{self, Ratios};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Walk {
    pub claim_overhead: f64,
    pub keyed: &'static [WidthTerm],
    pub nulls_first: &'static [WidthTerm],
    pub groups: GroupTerm,
    pub measured: bool,
    /// The host the measured rows are priced on (`measured::at_on`): the walk's rows move with the gather anchor, the
    /// plain fold's with the update anchor.
    pub ratios: Ratios,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Fitted,
    Measured,
}

impl Profile {
    pub fn parse(setting: Option<&str>) -> Profile {
        match setting {
            Some(v) if v.trim().eq_ignore_ascii_case("fitted") => Profile::Fitted,
            _ => Profile::Measured,
        }
    }
}

fn groups_of_presence(presence: f64) -> f64 {
    let p = presence.clamp(1e-12, 1.0 - 1e-12);
    -8192.0 / (-p).ln_1p()
}

fn log_interp(points: &[(f64, f64)], x: f64) -> Option<f64> {
    let (first, last) = (points.first()?, points.last()?);
    if x <= first.0 {
        return Some(first.1);
    }
    if x >= last.0 {
        return Some(last.1);
    }
    let i = points.windows(2).position(|w| x <= w[1].0)?;
    let ((x0, y0), (x1, y1)) = (points[i], points[i + 1]);
    Some(y0 + (y1 - y0) * (x.ln() - x0.ln()) / (x1.ln() - x0.ln()))
}

fn measured_spread_share(host: &Ratios, width: u32, s: f64, g: f64, n: f64, groups: f64, presence: f64) -> Option<f64> {
    let at = |name: &str, width: u32| crate::measured::at_on(host, name, width);
    const WALK: [(f64, &str); 4] =
        [(10.0, "walk.spread.walk_n10"), (100.0, "walk.spread.walk_n100"), (1000.0, "walk.spread.walk_n1000"), (10000.0, "walk.spread.walk_n10000")];
    const FOLD: [(f64, &str); 3] = [(22.0, "plain.fold_g22"), (4096.0, "plain.fold_g4096"), (4194304.0, "plain.fold_g4194304")];
    let mut wu = [(0.0, 0.0); 4];
    let mut wf = [(0.0, 0.0); 4];
    for (i, (x, name)) in WALK.iter().enumerate() {
        let r = at(name, width)?;
        (wu[i], wf[i]) = ((*x, r.ns), (*x, r.fixed_ns));
    }
    let mut fu = [(0.0, 0.0); 3];
    let mut ff = [(0.0, 0.0); 3];
    for (i, (x, name)) in FOLD.iter().enumerate() {
        let r = at(name, width)?;
        (fu[i], ff[i]) = ((*x, r.ns), (*x, r.fixed_ns));
    }
    let n = n.max(1.0);
    let rare_ns = log_interp(&wf, n)? + s * log_interp(&wu, n)?;
    let (rec, q) = (at("walk.spread_rec.walk", width)?, at("walk.spread_rec.q", width)?);
    let recurring_ns = rec.fixed_ns + s * rec.ns + q.ns * n * s * s;
    let p = presence.clamp(0.0, 1.0);
    let walk_ns = (1.0 - p) * rare_ns + p * recurring_ns;
    let plain_ns = log_interp(&ff, groups)? + g * log_interp(&fu, groups)?;
    (plain_ns > 0.0 && walk_ns.is_finite()).then_some(walk_ns / plain_ns)
}

fn measured_share(host: &Ratios, group: bool, width: u32, s: f64, g: f64) -> Option<f64> {
    let at = |name: &str, width: u32| crate::measured::at_on(host, name, width);
    let (plain, walk, drain) = if group {
        ("walk.group.plain", "walk.group.walk", "walk.group.walk_per_table_granule")
    } else {
        ("walk.keyed.plain", "walk.keyed.walk", "walk.keyed.walk_per_table_granule")
    };
    let (p, w, d) = (at(plain, width)?, at(walk, width)?, at(drain, width)?);
    let plain_ns = p.fixed_ns + g * p.ns;
    let walk_ns = w.fixed_ns + s * (w.ns + d.ns * g);
    (plain_ns > 0.0 && walk_ns.is_finite()).then_some(walk_ns / plain_ns)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupTerm {
    pub read: f64,
    pub per_kgranule: f64,
    pub granule_cap: f64,
    pub spread_read: f64,
    pub recurring: f64,
    pub per_group: f64,
    pub w2_read: f64,
    pub w2_spread_read: f64,
    pub widths: &'static [GroupWidth],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupWidth {
    pub clustered: WidthTerm,
    pub spread_factor: f64,
}

pub const GROUP_READ: f64 = 1.15;
pub const GROUP_PER_KGRANULE: f64 = 0.65;
pub const GROUP_GRANULE_CAP: f64 = 2048.0;
pub const GROUP_SPREAD_READ: f64 = 0.55;
pub const GROUP_RECURRING: f64 = 1.75e-5;
pub const GROUP_PER_GROUP: f64 = 4e-5;
pub const GROUP_W2_READ: f64 = 1.75;
pub const GROUP_W2_SPREAD_READ: f64 = 0.7;
pub const GROUP_W8: GroupWidth = GroupWidth { clustered: WidthTerm { from_width: 8, factor: 10.0, plain_start: 256.0, walk_start: 32.0 }, spread_factor: 0.65 };

pub fn key_spread(span_sum: f64, granules: u64, lo: i64, hi: i64) -> f64 {
    let whole = hi as f64 - lo as f64;
    if granules == 0 || !(whole > 0.0) || !(span_sum >= 0.0) || !span_sum.is_finite() {
        return 0.0;
    }
    (span_sum / granules as f64 / whole).clamp(0.0, 1.0)
}

pub fn key_presence(rows: f64, ndistinct: f64) -> f64 {
    if !(ndistinct >= 1.0) || !(rows >= 0.0) {
        return 1.0;
    }
    (-(-rows / ndistinct).exp_m1()).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WidthTerm {
    pub from_width: u32,
    pub factor: f64,
    pub plain_start: f64,
    pub walk_start: f64,
}

impl WidthTerm {
    pub fn share(&self, survivors: f64, granules: f64) -> f64 {
        let fitted = self.factor.is_finite() && self.factor > 0.0;
        let ok = fitted && self.plain_start.is_finite() && self.plain_start >= 0.0 && self.walk_start.is_finite() && self.walk_start >= 0.0;
        let plain = granules + self.plain_start;
        if ok && plain > 0.0 { self.factor * (survivors.max(0.0) + self.walk_start) / plain } else { 0.0 }
    }
}

pub const KEYED_W8: WidthTerm =
    WidthTerm { from_width: 8, factor: KEYED_W8_FACTOR, plain_start: KEYED_W8_START, walk_start: KEYED_W8_WALK_START };
pub const NULLS_FIRST_W8: WidthTerm =
    WidthTerm { from_width: 8, factor: NULLS_FIRST_W8_FACTOR, plain_start: NULLS_FIRST_W8_START, walk_start: 0.0 };

pub const CLAIM_OVERHEAD: f64 = 0.15;
pub const KEYED_W8_FACTOR: f64 = 1.0;
pub const KEYED_W8_START: f64 = 0.0;
pub const KEYED_W8_WALK_START: f64 = 32.0;
pub const NULLS_FIRST_W8_FACTOR: f64 = 5.0;
pub const NULLS_FIRST_W8_START: f64 = 40.0;

impl Walk {
    pub const REFERENCE: Walk = Walk {
        claim_overhead: CLAIM_OVERHEAD,
        keyed: &[KEYED_W8],
        nulls_first: &[NULLS_FIRST_W8],
        groups: GroupTerm {
            read: GROUP_READ,
            per_kgranule: GROUP_PER_KGRANULE,
            granule_cap: GROUP_GRANULE_CAP,
            spread_read: GROUP_SPREAD_READ,
            recurring: GROUP_RECURRING,
            per_group: GROUP_PER_GROUP,
            w2_read: GROUP_W2_READ,
            w2_spread_read: GROUP_W2_SPREAD_READ,
            widths: &[GROUP_W8],
        },
        measured: false,
        ratios: Ratios::ONE,
    };

    pub const MEASURED: Walk = Walk { measured: true, ..Walk::REFERENCE };

    /// The walk's prices under `profile`, the measured rows on this host (the installed profile).
    pub fn of(profile: Profile) -> Walk {
        let w = match profile {
            Profile::Fitted => Walk::REFERENCE,
            Profile::Measured => Walk::MEASURED,
        };
        Walk { ratios: *anchor::ratios(), ..w }
    }

    /// What the walk pays to read a granule, the plain read being 1: `1 + claim_overhead` (a non-finite or negative
    /// overhead reads as none, so a bad fit never makes the walk look cheaper than the plain read).
    pub fn read_factor(&self) -> f64 {
        if self.claim_overhead.is_finite() && self.claim_overhead > 0.0 { 1.0 + self.claim_overhead } else { 1.0 }
    }

    pub fn break_even(&self) -> f64 {
        1.0 / self.read_factor()
    }

    pub fn width_term(&self, width: u32, nulls_first: bool) -> Option<WidthTerm> {
        let terms = if nulls_first { self.nulls_first } else { self.keyed };
        terms.iter().copied().filter(|t| t.from_width <= width).max_by_key(|t| t.from_width)
    }

    pub fn share(&self, survivors: u64, granules: u64, width: u32, nulls_first: bool) -> f64 {
        let (s, g) = (survivors as f64, granules.max(1) as f64);
        if self.measured && !nulls_first {
            if let (Some(w0), Some(at)) = (measured_share(&self.ratios, false, 0, s, g), measured_share(&self.ratios, false, width, s, g)) {
                return w0.max(at);
            }
        }
        let w0 = s / g * self.read_factor();
        self.width_term(width, nulls_first).map_or(w0, |t| w0.max(t.share(s, g)))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn group_share(&self, survivors: u64, granules: u64, keys: u64, kept: u64, spread: f64, presence: f64, width: u32) -> f64 {
        self.group_share_of(survivors, granules, keys, kept, spread, presence, f64::NAN, width)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn group_share_of(&self, survivors: u64, granules: u64, keys: u64, kept: u64, spread: f64, presence: f64, groups: f64, width: u32) -> f64 {
        let (s, g) = (survivors as f64, granules.max(1) as f64);
        let g_ = &self.groups;
        let term = |x: f64| if x.is_finite() && x >= 0.0 { x } else { 0.0 };
        let read = |x: f64| if x.is_finite() && x > 0.0 { x } else { self.read_factor() };
        let unit = |x: f64| if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
        let (c, p, n) = (unit(spread), if presence.is_finite() { presence.clamp(0.0, 1.0) } else { 1.0 }, keys as f64);
        let own = g_.widths.iter().copied().filter(|t| t.clustered.from_width <= width).max_by_key(|t| t.clustered.from_width);
        let (clustered, spread) = match own {
            Some(t) => (t.clustered.share(s, g), s / g * term(t.spread_factor)),
            None if width >= 2 => (s / g * read(g_.w2_read), s / g * term(g_.w2_spread_read)),
            None => {
                let clustered = read(g_.read) + term(g_.per_kgranule) * g.min(term(g_.granule_cap)) / 1024.0;
                (s / g * clustered, s / g * (term(g_.spread_read) + term(g_.recurring) * p * n * n))
            }
        };
        let clustered = if self.measured { measured_share(&self.ratios, true, width, s, g).unwrap_or(clustered) } else { clustered };
        let spread = if self.measured && c > 0.0 {
            let groups = if groups.is_finite() && groups >= 1.0 { groups } else { groups_of_presence(p) };
            measured_spread_share(&self.ratios, width, s, g, n, groups, p).unwrap_or(spread)
        } else {
            spread
        };
        let base = (1.0 - c) * clustered + c * spread;
        base + s / g * term(g_.per_group) * kept as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_reads_dearer_than_the_plain_scan_and_breaks_even_below_every_granule() {
        let w = Walk::REFERENCE;
        assert!(w.read_factor() > 1.0, "a walked granule pays its claim");
        assert!(w.break_even() > 0.0 && w.break_even() < 1.0);
        assert_eq!(Walk { claim_overhead: 0.25, ..Walk::REFERENCE }.break_even(), 0.8);
        assert_eq!(Walk { claim_overhead: f64::NAN, ..Walk::REFERENCE }.read_factor(), 1.0);
        assert_eq!(Walk { claim_overhead: -1.0, ..Walk::REFERENCE }.read_factor(), 1.0);
    }

    #[test]
    fn a_width_term_prices_its_width_and_up_and_only_ever_makes_the_walk_dearer() {
        let w = Walk::REFERENCE;
        assert_eq!(w.width_term(0, false), None);
        assert_eq!(w.width_term(1, false), None);
        assert_eq!(w.width_term(2, false), None, "no keyed term at width 2");
        assert_eq!(w.width_term(7, false), None);
        assert_eq!(w.width_term(8, false), Some(KEYED_W8));
        assert_eq!(w.width_term(64, false), Some(KEYED_W8));
        assert_eq!(w.width_term(2, true), None, "no NULLS FIRST term at width 2");
        assert_eq!(w.width_term(8, true), Some(NULLS_FIRST_W8));
        for (s, g) in [(1, 16), (16, 64), (40, 128), (64, 512), (200, 512), (100, 4096)] {
            for width in [0, 1, 2, 4, 8, 16] {
                for nf in [false, true] {
                    let w0 = w.share(s, g, 0, nf);
                    assert_eq!(w0, s as f64 / g as f64 * w.read_factor());
                    assert!(w.share(s, g, width, nf) >= w0, "s {s} g {g} width {width}: never cheaper than W0");
                }
            }
        }
        assert!(w.share(320, 512, 0, false) < 1.0);
        assert!(w.share(384, 512, 8, false) < 1.0 && w.share(448, 512, 8, false) > 1.0);
        assert!(w.share(256, 2048, 8, false) < 1.0);
        assert!(w.share(39, 192, 8, true) < 1.0 && w.share(77, 192, 8, true) > 1.0);
        let bad = WidthTerm { from_width: 8, factor: f64::NAN, plain_start: 0.0, walk_start: 0.0 };
        assert_eq!(bad.share(10.0, 100.0), 0.0);
        assert_eq!(WidthTerm { plain_start: -1.0, ..KEYED_W8 }.share(10.0, 100.0), 0.0);
        assert_eq!(WidthTerm { walk_start: -1.0, ..KEYED_W8 }.share(10.0, 100.0), 0.0);
        assert_eq!(KEYED_W8.share(0.0, 64.0), KEYED_W8_WALK_START / 64.0, "the walk's start");
    }

    #[test]
    fn the_spread_and_the_presence_read_the_zones_and_the_estimate() {
        assert_eq!(key_spread(0.0, 512, 0, 511), 0.0, "one key a granule");
        assert!(key_spread(512.0 * 1_000_000.0, 512, 0, 1_001_023) > 0.99, "every granule spans the key");
        assert_eq!(key_spread(10.0, 4, 5, 5), 0.0, "one value");
        assert_eq!(key_spread(f64::NAN, 4, 0, 9), 0.0);
        assert_eq!(key_spread(10.0, 0, 0, 9), 0.0);
        assert!((key_presence(8192.0, 1024.0) - 1.0).abs() < 1e-3);
        assert!((key_presence(8192.0, 51_300.0) - 0.148).abs() < 1e-3);
        assert!(key_presence(8192.0, 4_194_304.0) < 0.002);
        assert_eq!(key_presence(8192.0, 0.0), 1.0, "no estimate reads dearer");
    }

    #[test]
    fn a_group_walk_is_priced_by_its_keys_spread_and_the_groups_it_keeps() {
        let w = Walk::REFERENCE;
        let clu = |s: u64, g: u64, width: u32| w.group_share(s, g, s, s, 0.0, 0.0, width);
        assert!(clu(160, 256, 0) < 1.0 && clu(224, 256, 0) > 1.0, "256 granules: 0.83 wins, 1.17 loses");
        assert!(clu(256, 512, 0) < 1.0 && clu(384, 512, 0) > 1.0, "512: 0.77 / 1.17");
        assert!(clu(448, 1024, 0) < 1.0 && clu(576, 1024, 0) > 1.0, "1,024: 0.84 / 1.10");
        assert!(clu(640, 2048, 0) < 1.0 && clu(1024, 2048, 0) > 1.0, "2,048: 0.80 / 1.38");
        assert!(clu(1280, 4096, 0) < 1.0 && clu(3072, 4096, 0) > 1.0, "4,096: 0.71 / 1.80");
        assert!(clu(768, 2048, 2) < 1.0, "W2 2,048 granules n 768: 0.85 (no clustered W2 walk loses past 1.05)");
        assert!(clu(128, 2048, 8) < 1.0 && clu(256, 2048, 8) > 1.0, "W8: 0.81 / 1.10");
        assert!(clu(256, 4096, 8) < 1.0, "W8 4,096 granules n 256: 0.73");
        let wt = |s: u64, width: u32| w.group_share(s, 512, s, s, 1.0, key_presence(8192.0, 51_300.0), width);
        for width in [0, 2, 8] {
            assert!(wt(320, width) < 1.0, "wt n 320 at W{width}: 0.41 / 0.27 / 0.18");
        }
        let rk = |s: u64, width: u32| w.group_share(s, 512, s, s, 1.0, key_presence(8192.0, 1536.0), width);
        assert!(rk(192, 0) < 1.0 && rk(320, 0) > 1.0, "1,024 recurring keys at W0: 0.66 / 2.32");
        assert!(rk(448, 2) < 1.0 && rk(448, 8) < 1.0, "from width 2: 0.68 / 0.85");
        let pm = |n: u64, width: u32| w.group_share(n, 256, n, n * 523, 0.0, 0.0, width);
        assert!(pm(40, 0) < 1.0 && pm(100, 0) > 1.0, "W0 0.29 / 1.12");
        assert!(pm(40, 2) < 1.0 && pm(100, 2) > 1.0, "W2 0.42 / 1.14");
        assert!(pm(10, 8) < 1.0 && pm(40, 8) > 1.0, "W8 0.60 / 1.22");
        assert!(w.group_share(64, 512, 64, 4096, 0.0, 0.0, 8) > w.group_share(64, 512, 64, 64, 0.0, 0.0, 8));
        assert!(w.group_share(200, 512, 200, 200, 1.0, 1.0, 0) > w.group_share(200, 512, 200, 200, 1.0, 0.1, 0));
        assert_eq!(w.group_share(64, 512, 64, 64, 0.3, 0.2, 1), w.group_share(64, 512, 64, 64, 0.3, 0.2, 0));
        // a bad fit never prices a clustered walk under the keyed read, and a bad reading reads as the clustered one
        let nan = Walk { groups: GroupTerm { read: f64::NAN, per_kgranule: f64::NAN, per_group: f64::NAN, ..Walk::REFERENCE.groups }, ..Walk::REFERENCE };
        assert_eq!(nan.group_share(10, 100, 10, 10, 0.0, 0.0, 0), w.share(10, 100, 0, false));
        assert_eq!(w.group_share(10, 100, 10, 10, f64::NAN, 0.0, 0), w.group_share(10, 100, 10, 10, 0.0, 0.0, 0));
    }

    #[test]
    fn the_measured_profile_prices_w0_as_the_fit_and_declines_the_w8_walk_the_fit_took() {
        let (m, f) = (Walk::of(Profile::Measured), Walk::of(Profile::Fitted));
        assert_eq!(f, Walk::REFERENCE);
        assert!(m.measured && !f.measured);
        for (s, g) in [(128u64, 256u64), (64, 512), (256, 1024), (512, 2048)] {
            let (mk, fk) = (m.share(s, g, 0, false), f.share(s, g, 0, false));
            assert!((mk / fk - 1.0).abs() < 0.25, "keyed W0 s {s} g {g}: measured {mk} fitted {fk}");
            let (mg, fg) = (m.group_share(s, g, s, s, 0.0, 1.0, 0), f.group_share(s, g, s, s, 0.0, 1.0, 0));
            assert!((mg / fg - 1.0).abs() < 0.25, "group W0 s {s} g {g}: measured {mg} fitted {fg}");
            for w in [2, 8, 16] {
                assert!(m.share(s, g, w, false) >= m.share(s, g, 0, false), "s {s} g {g} W{w}: never cheaper than W0");
            }
        }
        assert!(f.share(1024, 2048, 8, false) < 1.0, "the fitted W8 term takes it");
        assert!(m.share(1024, 2048, 8, false) >= 1.0, "measured: the plain top");
        assert_eq!(m.share(10, 100, 8, true), f.share(10, 100, 8, true), "NULLS FIRST keeps the fitted price");
        assert_eq!(Profile::parse(Some(" Measured ")), Profile::Measured);
        assert_eq!(Profile::parse(Some("fitted")), Profile::Fitted);
        assert_eq!(Profile::parse(None), Profile::Measured, "measured is the default");
        assert_eq!(Profile::parse(Some(" FITTED")), Profile::Fitted);
    }

    #[test]
    fn the_measured_spread_walk_declines_s03_and_keeps_u01a() {
        let (m, f) = (Walk::of(Profile::Measured), Walk::of(Profile::Fitted));
        let s03 = |w: &Walk| w.group_share_of(365, 366, 4, 4, 1.0, 1.0, 22.0, 0);
        let u01a = |w: &Walk| w.group_share_of(100, 366, 10, 10, 1.0, key_presence(8192.0, 2.0e6), 2.0e6, 0);
        assert!(f.group_share(365, 366, 4, 4, 1.0, 1.0, 0) < 1.0, "the fitted price takes S03's walk");
        assert!(s03(&m) >= 1.0, "measured: S03 keeps the plain group ({})", s03(&m));
        assert!(u01a(&m) < 1.0, "measured: U01A keeps the walk ({})", u01a(&m));
        let a02_w8 = m.group_share_of(250, 366, 10, 10, 1.0, 1.0, 121.0, 8);
        assert!(a02_w8 < 1.0, "measured: the recurring-key walk at W8 ({a02_w8})");
        assert_eq!(m.group_share(100, 366, 10, 10, 1.0, 0.0, 0), m.group_share_of(100, 366, 10, 10, 1.0, 0.0, f64::NAN, 0));
        assert!((groups_of_presence(key_presence(8192.0, 100_000.0)) / 100_000.0 - 1.0).abs() < 1e-6);
        assert_eq!(log_interp(&[(10.0, 1.0), (100.0, 2.0)], 31.622776601683793).map(|v| (v * 1e9).round() / 1e9), Some(1.5));
    }
}
