//! `tablefunc.c` `normal_rand` — a value-per-call SRF over the Box-Muller
//! polar method, drawing from the house global PRNG seam.

use datum::Datum;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

struct NormalRandFctx {
    mean: f64,
    stddev: f64,
    carry_val: f64,
    use_carry: bool,
}

#[inline]
fn polar_s(v1: f64, v2: f64) -> f64 {
    // tablefunc.c:303 (D400)
    ::fp_contract::fmadd(v1, v1, v2 * v2)
}

#[inline]
fn scale_normal(mean: f64, stddev: f64, normval: f64) -> f64 {
    // tablefunc.c:260 (D400)
    ::fp_contract::fmadd(stddev, normval, mean)
}

// get_normal_pair: Algorithm P (polar method), mean 0, stddev 1.
fn get_normal_pair() -> (f64, f64) {
    loop {
        let (u1, u2) = pg_prng::global_prng(|p| (p.next_f64(), p.next_f64()));
        let v1 = 2.0 * u1 - 1.0;
        let v2 = 2.0 * u2 - 1.0;
        let s = polar_s(v1, v2);
        if s >= 1.0 {
            continue;
        }
        if s == 0.0 {
            return (0.0, 0.0);
        }
        let s = ((-2.0 * s.ln()) / s).sqrt();
        return (v1 * s, v2 * s);
    }
}

#[cold]
#[inline(never)]
fn negative_rows() -> PgError {
    PgError::error("number of rows cannot be negative".to_string())
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
}

pub(crate) fn fc_normal_rand(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("normal_rand: NULL flinfo");

    if !flinfo.has_fn_extra() {
        let call = funcapi::init_MultiFuncCall(flinfo, fcinfo)?;
        let num_tuples = fcinfo.arg_i32(0);
        if num_tuples < 0 {
            return Err(negative_rows().into());
        }
        let fctx = NormalRandFctx {
            mean: fcinfo.arg_f64(1),
            stddev: fcinfo.arg_f64(2),
            carry_val: 0.0,
            use_carry: false,
        };
        call.max_calls = num_tuples as u64;
        call.user_fctx = Some(Box::new(fctx));
    }

    let call = funcapi::per_MultiFuncCall(flinfo);
    if call.call_cntr < call.max_calls {
        let fctx = call
            .user_fctx
            .as_mut()
            .expect("normal_rand: user_fctx set at first call")
            .downcast_mut::<NormalRandFctx>()
            .expect("normal_rand: user_fctx is NormalRandFctx");
        let result = if fctx.use_carry {
            fctx.use_carry = false;
            fctx.carry_val
        } else {
            let (n1, n2) = get_normal_pair();
            fctx.carry_val = scale_normal(fctx.mean, fctx.stddev, n2);
            fctx.use_carry = true;
            scale_normal(fctx.mean, fctx.stddev, n1)
        };
        Ok(funcapi::srf_return_next(flinfo, fcinfo, Datum::from_f64(result)))
    } else {
        Ok(funcapi::srf_return_done(flinfo, fcinfo))
    }
}

#[cfg(test)]
mod d400_tests {
    use super::*;

    #[test]
    fn polar_s_fmadd() {
        let want = if cfg!(target_arch = "aarch64") { 0.42639999999999995 } else { 0.4264 };
        assert_eq!(polar_s(0.3, 0.58), want);
    }

    #[test]
    fn scale_normal_fmadd() {
        let (mean, stddev, n) = (0.3f64, 7.77f64, 0.9735451174009585f64);
        let want = if cfg!(target_arch = "aarch64") { 7.864445562205447 } else { 7.8644455622054465 };
        assert_eq!(scale_normal(mean, stddev, n), want);
    }
}
