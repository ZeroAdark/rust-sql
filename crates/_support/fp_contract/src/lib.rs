//! D400: match the platform's PostgreSQL. GCC on aarch64 fuses a*b+c into
//! one rounding (-ffp-contract=fast); x86-64 rounds twice. Every fused C
//! site calls these in GCC's association: fmadd a*b+c, fmsub c-a*b,
//! fnmsub a*b-c, fnmadd -(a*b)-c; the f-suffixed twins are f32.
#![forbid(unsafe_code)]

pub const FUSED: bool = cfg!(target_arch = "aarch64");

macro_rules! fp_helpers {
    ($t:ty, $fmadd:ident, $fmsub:ident, $fnmsub:ident, $fnmadd:ident) => {
        #[inline(always)]
        pub fn $fmadd(a: $t, b: $t, c: $t) -> $t {
            #[cfg(target_arch = "aarch64")]
            {
                a.mul_add(b, c)
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                a * b + c
            }
        }

        #[inline(always)]
        pub fn $fmsub(a: $t, b: $t, c: $t) -> $t {
            #[cfg(target_arch = "aarch64")]
            {
                (-a).mul_add(b, c)
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                c - a * b
            }
        }

        #[inline(always)]
        pub fn $fnmsub(a: $t, b: $t, c: $t) -> $t {
            #[cfg(target_arch = "aarch64")]
            {
                a.mul_add(b, -c)
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                a * b - c
            }
        }

        #[inline(always)]
        pub fn $fnmadd(a: $t, b: $t, c: $t) -> $t {
            #[cfg(target_arch = "aarch64")]
            {
                -(a.mul_add(b, c))
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
                -(a * b) - c
            }
        }
    };
}

fp_helpers!(f64, fmadd, fmsub, fnmsub, fnmadd);
fp_helpers!(f32, fmaddf, fmsubf, fnmsubf, fnmaddf);

#[cfg(test)]
mod tests {
    use super::*;

    const X: f64 = 1.0 + 1.0 / (1u64 << 30) as f64;

    #[test]
    fn fused_iff_aarch64() {
        let fused = X.mul_add(X, -1.0);
        let unfused = X * X - 1.0;
        assert_ne!(fused, unfused, "inputs must discriminate");
        let want = if FUSED { fused } else { unfused };
        assert_eq!(fnmsub(X, X, 1.0), want);
        assert_eq!(fmadd(X, X, -1.0), want);
        assert_eq!(fmsub(-X, X, -1.0), want);
        assert_eq!(fnmadd(-X, X, 1.0), want);
    }

    #[test]
    fn f32_fused_iff_aarch64() {
        let x: f32 = 1.0 + 1.0 / (1u32 << 12) as f32;
        let fused = x.mul_add(x, -1.0);
        let unfused = x * x - 1.0;
        assert_ne!(fused, unfused);
        let want = if FUSED { fused } else { unfused };
        assert_eq!(fnmsubf(x, x, 1.0), want);
        assert_eq!(fmaddf(x, x, -1.0), want);
        assert_eq!(fmsubf(-x, x, -1.0), want);
        assert_eq!(fnmaddf(-x, x, 1.0), want);
    }
}
