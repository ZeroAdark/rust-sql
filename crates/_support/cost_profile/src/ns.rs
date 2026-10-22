use core::ops::{Add, AddAssign, Mul};

#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct Ns(pub f64);

impl Ns {
    pub const ZERO: Ns = Ns(0.0);

    pub fn is_finite(self) -> bool {
        self.0.is_finite()
    }
}

impl Add for Ns {
    type Output = Ns;
    fn add(self, o: Ns) -> Ns {
        Ns(self.0 + o.0)
    }
}

impl AddAssign for Ns {
    fn add_assign(&mut self, o: Ns) {
        self.0 += o.0;
    }
}

impl Mul<f64> for Ns {
    type Output = Ns;
    fn mul(self, k: f64) -> Ns {
        Ns(self.0 * k)
    }
}

impl core::iter::Sum for Ns {
    fn sum<I: Iterator<Item = Ns>>(it: I) -> Ns {
        it.fold(Ns::ZERO, Add::add)
    }
}

pub fn op(primitive: &str, width: u32, units: f64) -> Option<Ns> {
    let r = crate::measured::at(primitive, width)?;
    Some(Ns(r.fixed_ns + r.ns * units.max(0.0)))
}

pub fn tiered(tiers: &[(f64, &str)], width: u32, size: f64, units: f64) -> Option<Ns> {
    let mut pts = [(0.0f64, 0.0f64, 0.0f64); 8];
    let n = tiers.len().min(pts.len());
    for (i, (x, name)) in tiers.iter().take(n).enumerate() {
        let r = crate::measured::at(name, width)?;
        pts[i] = (*x, r.ns, r.fixed_ns);
    }
    let pts = &pts[..n];
    let (first, last) = (pts.first()?, pts.last()?);
    let (unit, fixed) = if size <= first.0 {
        (first.1, first.2)
    } else if size >= last.0 {
        (last.1, last.2)
    } else {
        let i = pts.windows(2).position(|w| size <= w[1].0)?;
        let (a, b) = (pts[i], pts[i + 1]);
        let t = (size.ln() - a.0.ln()) / (b.0.ln() - a.0.ln());
        (a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
    };
    Some(Ns(fixed + unit * units.max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_adds_scales_and_reads_the_measured_table() {
        assert_eq!(Ns(1.0) + Ns(2.0), Ns(3.0));
        assert_eq!(Ns(2.0) * 3.0, Ns(6.0));
        assert_eq!([Ns(1.0), Ns(2.0)].into_iter().sum::<Ns>(), Ns(3.0));
        let g = op("walk.group.plain", 0, 10.0).expect("measured");
        let r = crate::measured::at("walk.group.plain", 0).unwrap();
        assert_eq!(g, Ns(r.fixed_ns + 10.0 * r.ns));
        assert_eq!(op("no.such", 0, 1.0), None);
        let tiers = [(22.0, "plain.fold_g22"), (4096.0, "plain.fold_g4096"), (4194304.0, "plain.fold_g4194304")];
        let (lo, mid, hi) = (tiered(&tiers, 0, 22.0, 1e3).unwrap(), tiered(&tiers, 0, 300.0, 1e3).unwrap(), tiered(&tiers, 0, 1e9, 1e3).unwrap());
        assert!(lo < mid && mid < hi, "{lo:?} {mid:?} {hi:?}");
        assert_eq!(tiered(&tiers, 0, 1.0, 1e3), Some(lo), "under the first tier reads the first");
    }
}
