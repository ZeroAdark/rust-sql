
#[inline]
pub fn f64_order_key(v: f64) -> i64 {
    if v.is_nan() {
        return i64::MAX;
    }
    // -0.0 → +0.0 (adding 0.0 canonicalizes the zero sign).
    let v = v + 0.0;
    let b = v.to_bits() as i64;
    b ^ (((b >> 63) as u64) >> 1) as i64
}

#[inline]
pub fn f32_order_key(v: f32) -> i64 {
    f64_order_key(v as f64)
}

#[inline]
pub fn f64_key_from_datum(datum: u64) -> i64 {
    f64_order_key(f64::from_bits(datum))
}

#[inline]
pub fn f32_key_from_datum(datum: u64) -> i64 {
    f32_order_key(f32::from_bits(datum as u32))
}

#[inline]
pub fn be_prefix_embed(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    (u64::from_be_bytes(buf) ^ (1u64 << 63)) as i64
}

#[inline]
pub fn memcmp_fixed_exact_key(image: &[u8]) -> i64 {
    debug_assert!(image.len() <= 8);
    be_prefix_embed(image)
}

#[inline]
pub fn memcmp_fixed_prefix_key(image: &[u8]) -> i64 {
    debug_assert!(image.len() > 8);
    be_prefix_embed(image)
}

#[inline]
pub fn memcmp_var_prefix_key(payload: &[u8]) -> i64 {
    be_prefix_embed(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_golden_vectors() {
        assert_eq!(f64_order_key(0.0), 0);
        assert_eq!(f64_order_key(-0.0), 0, "the ±0 class collapses");
        assert_eq!(f64_order_key(1.0), 0x3FF0_0000_0000_0000);
        assert_eq!(f64_order_key(-1.0), 0xC00F_FFFF_FFFF_FFFFu64 as i64);
        assert_eq!(f64_order_key(f64::INFINITY), 0x7FF0_0000_0000_0000);
        assert_eq!(
            f64_order_key(f64::NEG_INFINITY),
            0x800F_FFFF_FFFF_FFFFu64 as i64
        );
        assert_eq!(f64_order_key(f64::NAN), i64::MAX);
        assert_eq!(f64_order_key(-f64::NAN), i64::MAX, "every NaN collapses");
        assert_eq!(memcmp_var_prefix_key(b""), i64::MIN);
        assert_eq!(memcmp_var_prefix_key(&[0xFF; 8]), i64::MAX);
        assert_eq!(memcmp_fixed_exact_key(&[0x00]), i64::MIN);
        assert_eq!(
            memcmp_fixed_exact_key(&[0xFF]),
            0x7F00_0000_0000_0000,
            "one 0xFF byte in the high position"
        );
    }
}
