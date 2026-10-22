
use crate::device::Device;

pub const BLOOM_K_MAX: u32 = 16;

pub fn bloom_inverse_rate(rows: u64, width: u64, d: &Device) -> f64 {
    let ln2 = core::f64::consts::LN_2;
    let scan = d.fixed_ns + rows as f64 * width as f64 * d.random_ns_per_byte;
    scan * 8.0 * ln2 * ln2 / (rows.max(1) as f64 * d.seq_ns_per_byte)
}

pub fn bloom_k(rows: u64, width: u64, d: &Device) -> u32 {
    let inv = bloom_inverse_rate(rows, width, d);
    if inv.is_nan() {
        return 1;
    }
    let mut k = 1;
    while k < BLOOM_K_MAX && ((1u64 << k) as f64) < inv {
        k += 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_reference_k_by_width() {
        for (width, k) in [(1u64, 4u32), (2, 4), (4, 5), (8, 6), (16, 7)] {
            assert_eq!(bloom_k(8192, width, &Device::WARM), k, "width {width}");
            let p = 1.0 / bloom_inverse_rate(8192, width, &Device::WARM);
            assert!(0.5f64.powi(k as i32) <= p && p < 0.5f64.powi(k as i32 - 1), "width {width}: p* {p}");
        }
        assert!(bloom_k(8192, 4, &Device::COLD) > bloom_k(8192, 4, &Device::WARM));
        let free = Device { seq_ns_per_byte: 0.0, ..Device::WARM };
        assert_eq!(bloom_k(8192, 4, &free), BLOOM_K_MAX);
        assert_eq!(bloom_k(8192, 4, &Device { seq_ns_per_byte: f64::NAN, ..Device::WARM }), 1);
    }
}
