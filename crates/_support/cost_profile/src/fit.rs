
pub const fn break_even(touched: f64, whole: f64) -> f64 {
    if touched.is_nan() || touched <= 0.0 {
        return 1.0;
    }
    let f = whole / touched;
    if f > 1.0 {
        1.0
    } else if f > 0.0 {
        f
    } else {
        0.0
    }
}

/// `break_even` as the divisor an integer test reads (`n * div <= total`): the least `div` with `1 / div <= break_even`,
/// i.e. `touched / whole` rounded up (`usize::MAX` when the whole-input unit is free: the per-unit strategy never pays).
pub const fn break_even_div(touched: f64, whole: f64) -> usize {
    if whole.is_nan() || whole <= 0.0 {
        return usize::MAX;
    }
    let q = touched / whole;
    if q.is_nan() || q <= 1.0 {
        return 1;
    }
    if q >= usize::MAX as f64 {
        return usize::MAX;
    }
    let i = q as usize;
    if (i as f64) < q {
        i + 1
    } else {
        i
    }
}

/// How many items ahead a random read must be issued to land by its use: the miss latency over the loop's time per item,
/// rounded up, within `1..=ring` (the prefetch ring the caller keeps; `Machine::prefetch` for the owner fold's).
pub const fn prefetch_distance(miss_ns: f64, per_item_ns: f64, ring: usize) -> usize {
    let ring = if ring == 0 { 1 } else { ring };
    if per_item_ns.is_nan() || per_item_ns <= 0.0 || miss_ns.is_nan() || miss_ns <= 0.0 {
        return ring;
    }
    let q = miss_ns / per_item_ns;
    if q >= ring as f64 {
        return ring;
    }
    let i = q as usize;
    let d = if (i as f64) < q { i + 1 } else { i };
    if d == 0 {
        1
    } else {
        d
    }
}

/// `num / den` of `total` without overflow: a structural reserve (DR-11 / DR-13: no decline, so no cost to fit) states the
/// fraction of the grant it holds, never a byte count.
pub const fn fraction(total: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return total;
    }
    total / den * num + total % den * num / den
}

pub fn least_squares(points: &[(f64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    if points.len() < 2 {
        return (0.0, 0.0);
    }
    let (sx, sy) = points.iter().fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    let (mx, my) = (sx / n, sy / n);
    let (sxx, sxy) = points.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + (x - mx) * (x - mx), b + (x - mx) * (y - my)));
    if sxx.is_nan() || sxx <= 0.0 {
        return (0.0, 0.0);
    }
    let b = sxy / sxx;
    (my - b * mx, b)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fitted {
    pub ns: f64,
    pub machine: &'static str,
    pub corpus: &'static str,
    /// The anchor unit this price moves with on a host other than the reference: there it costs `ns` times that anchor's
    /// ratio (`host_ns`). Two prices of one anchor keep their ratio on every host, so a decision between them holds.
    pub anchor: crate::anchor::Anchor,
}

impl Fitted {
    /// This price on a host of ratios `r`: `ns` times its anchor's ratio there.
    pub const fn on(self, r: &crate::anchor::Ratios) -> f64 {
        self.ns * r.0[self.anchor as usize]
    }

    /// This price on the host whose profile is installed (`anchor::install`); `ns` where none is (crate tests, tools).
    pub fn host_ns(self) -> f64 {
        self.on(crate::anchor::ratios())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_break_even_is_the_ratio_of_unit_costs() {
        assert_eq!((break_even(8.0, 1.0), break_even_div(8.0, 1.0)), (0.125, 8));
        assert_eq!(break_even_div(10.0, 3.0), 4, "1/4 <= 0.3 < 1/3");
        // a strategy dearer per whole unit than per touched one always pays; a free one never needs a divisor
        assert_eq!((break_even(1.0, 2.0), break_even_div(1.0, 2.0)), (1.0, 1));
        assert_eq!((break_even(0.0, 1.0), break_even_div(1.0, 0.0)), (1.0, usize::MAX));
        const DIV: usize = break_even_div(2.6, 0.31);
        assert_eq!(DIV, 9);
    }

    #[test]
    fn a_prefetch_covers_the_miss() {
        assert_eq!((prefetch_distance(100.0, 12.5, 16), prefetch_distance(100.0, 13.0, 16)), (8, 8));
        assert_eq!((prefetch_distance(400.0, 1.0, 16), prefetch_distance(1.0, 100.0, 16), prefetch_distance(0.0, 1.0, 0)), (16, 1, 1));
    }

    #[test]
    fn a_fraction_never_overflows() {
        assert_eq!((fraction(1000, 1, 4), fraction(u64::MAX, 1, 2), fraction(7, 3, 0)), (250, u64::MAX / 2, 7));
    }

    #[test]
    fn least_squares_recovers_a_line() {
        let pts: Vec<(f64, f64)> = (1..=8).map(|x| (x as f64 * 100.0, 50.0 + 2.5 * x as f64 * 100.0)).collect();
        let (a, b) = least_squares(&pts);
        assert!((a - 50.0).abs() < 1e-6 && (b - 2.5).abs() < 1e-9, "{a} {b}");
        assert_eq!(least_squares(&[(1.0, 2.0)]), (0.0, 0.0));
        assert_eq!(least_squares(&[(1.0, 2.0), (1.0, 3.0)]), (0.0, 0.0));
    }
}
