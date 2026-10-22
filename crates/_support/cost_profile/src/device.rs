
use crate::fit::break_even_div;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Device {
    pub fixed_ns: f64,
    pub random_ns_per_byte: f64,
    pub seq_ns_per_byte: f64,
}

impl Device {
    pub const COLD: Device = Device { fixed_ns: 542_000.0, random_ns_per_byte: 3.37, seq_ns_per_byte: 1.62 };
    pub const WARM: Device = Device { fixed_ns: 620.0, random_ns_per_byte: 0.037, seq_ns_per_byte: 0.036 };

    pub const fn at_residency(resident: Option<(u64, u64)>) -> Device {
        let (cached, pages) = match resident {
            Some((c, p)) if p > 0 => (if c > p { p } else { c }, p),
            _ => return Device::COLD,
        };
        let w = cached as f64 / pages as f64;
        let c = 1.0 - w;
        Device {
            fixed_ns: w * Device::WARM.fixed_ns + c * Device::COLD.fixed_ns,
            random_ns_per_byte: w * Device::WARM.random_ns_per_byte + c * Device::COLD.random_ns_per_byte,
            seq_ns_per_byte: w * Device::WARM.seq_ns_per_byte + c * Device::COLD.seq_ns_per_byte,
        }
    }

    pub const fn coalesce_gap(&self) -> u64 {
        if self.seq_ns_per_byte.is_nan() || self.seq_ns_per_byte <= 0.0 || self.fixed_ns.is_nan() || self.fixed_ns <= 0.0 {
            return 0;
        }
        let g = self.fixed_ns / self.seq_ns_per_byte;
        if g >= u64::MAX as f64 {
            u64::MAX
        } else {
            g as u64
        }
    }

    pub const fn stream_div(&self, block: u64) -> usize {
        let b = block as f64;
        break_even_div(self.fixed_ns + b * self.random_ns_per_byte, b * self.seq_ns_per_byte)
    }

    pub fn faults_to_stream(&self, nblocks: u64, block: u64) -> u64 {
        let b = block as f64;
        let fault = self.fixed_ns + b * self.random_ns_per_byte;
        let stream = self.fixed_ns + nblocks as f64 * b * self.seq_ns_per_byte;
        if fault.is_nan() || stream.is_nan() || fault <= 0.0 || nblocks <= 1 {
            return 1;
        }
        let q = stream / fault;
        if q >= u64::MAX as f64 {
            return u64::MAX;
        }
        (q.floor() as u64).saturating_add(1)
    }

    pub fn stream_pays(&self, probes: u64, nblocks: u64, block: u64) -> bool {
        if probes == 0 || nblocks == 0 {
            return false;
        }
        let b = block as f64;
        let reads = blocks_touched(probes, nblocks) * (self.fixed_ns + b * self.random_ns_per_byte);
        reads > self.fixed_ns + nblocks as f64 * b * self.seq_ns_per_byte
    }
}

pub fn blocks_touched(probes: u64, nblocks: u64) -> f64 {
    if probes == 0 || nblocks == 0 {
        return 0.0;
    }
    let n = nblocks as f64;
    n * (1.0 - (probes as f64 * (-1.0 / n).ln_1p()).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_regimes_give_the_fitted_thresholds() {
        assert_eq!((Device::COLD.coalesce_gap(), Device::WARM.coalesce_gap()), (334_567, 17_222));
        assert_eq!((Device::COLD.stream_div(16 << 10), Device::WARM.stream_div(16 << 10)), (23, 3));
    }

    #[test]
    fn residency_blends_the_regimes() {
        assert_eq!(Device::at_residency(None), Device::COLD);
        assert_eq!(Device::at_residency(Some((0, 0))), Device::COLD);
        assert_eq!(Device::at_residency(Some((0, 10))), Device::COLD);
        assert_eq!(Device::at_residency(Some((10, 10))), Device::WARM);
        assert_eq!(Device::at_residency(Some((12, 10))), Device::WARM, "a count past the pages is all cached");
        let half = Device::at_residency(Some((5, 10)));
        assert!(half.coalesce_gap() < Device::COLD.coalesce_gap() && half.coalesce_gap() > Device::WARM.coalesce_gap());
        let s = half.stream_div(16 << 10);
        assert!((3..=23).contains(&s), "{s}");
    }

    #[test]
    fn probes_touch_at_most_their_count_and_at_most_every_block() {
        assert_eq!((blocks_touched(0, 98), blocks_touched(5, 0)), (0.0, 0.0));
        assert!((blocks_touched(1, 98) - 1.0).abs() < 1e-9);
        assert!((blocks_touched(7, 1) - 1.0).abs() < 1e-9, "one block: touched once");
        let ten = blocks_touched(10, 98);
        assert!(ten > 9.5 && ten < 10.0, "{ten}");
        let many = blocks_touched(10_000, 98);
        assert!(many > 97.99 && many <= 98.0, "{many}");
    }

    #[test]
    fn a_stream_pays_once_the_probes_blocks_cost_more_than_it() {
        let b = 3 << 10;
        assert!(!Device::COLD.stream_pays(1, 49, b) && Device::COLD.stream_pays(2, 49, b));
        assert!(!Device::WARM.stream_pays(8, 49, b) && Device::WARM.stream_pays(10, 49, b));
        assert!(!Device::COLD.stream_pays(0, 49, b) && !Device::COLD.stream_pays(5, 0, b), "no probes or no blocks never stream");
        assert!(Device::COLD.stream_pays(u64::MAX, 49, b));
    }

    #[test]
    fn a_reader_streams_once_its_faults_have_cost_one_stream_with_its_fixed_cost() {
        let b = 16 << 10;
        assert_eq!((Device::COLD.faults_to_stream(20, b), Device::COLD.faults_to_stream(98, b)), (2, 6));
        assert!(Device::WARM.faults_to_stream(20, b) > 10, "{}", Device::WARM.faults_to_stream(20, b));
        assert_eq!((Device::COLD.faults_to_stream(1, b), Device::COLD.faults_to_stream(0, b)), (1, 1), "one block or none: the first fault reads it");
    }

    #[test]
    fn degenerate_inputs_stay_defined() {
        assert_eq!(Device::COLD.stream_div(0), usize::MAX);
        let free = Device { fixed_ns: 0.0, random_ns_per_byte: 0.0, seq_ns_per_byte: 0.0 };
        assert_eq!(free.coalesce_gap(), 0);
    }
}
