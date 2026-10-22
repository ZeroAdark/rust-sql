//! cache_track (cache plan C2): `pgrust_memory_cache_curves()`, one row per tracked cache: its cost curve's hull H
//! (bytes:ns-a-second points), the next 32 MiB batch's price v, the knee h (ranking and 2 h floor curves), the policy
//! correction kappa and the sample's state (`mempolicy::track`). No rows while `cache_track` is off.

use core::fmt::Write;

use datum::Datum;
use types_error::PgResult;
use types_fmgr::{varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

/// The row's columns (scripts/memory-broker-views.sql declares the same, in order).
pub const CURVE_COLS: usize = 20;

fn i8d(x: u64) -> Datum {
    Datum::from_i64(i64::try_from(x).unwrap_or(i64::MAX))
}

/// A fixed buffer one number is formatted into (no allocation).
struct Num {
    b: [u8; 32],
    n: usize,
}

impl Write for Num {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.n + s.len();
        self.b.get_mut(self.n..end).ok_or(core::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.n = end;
        Ok(())
    }
}

/// A text value written straight into the result context: `items` formatted by `put`, space separated.
fn text_of<'m, T>(mcx: mcx::Mcx<'m>, items: &[T], put: impl Fn(&mut Num, &T) -> core::fmt::Result) -> Datum {
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    for (i, x) in items.iter().enumerate() {
        let mut n = Num { b: [0; 32], n: 0 };
        if i > 0 {
            img.push(b' ');
        }
        if put(&mut n, x).is_ok() {
            img.extend_from_slice(&n.b[..n.n]);
        }
    }
    varlena_result(datum::Varlena::from_image(img))
}

/// `pgrust_memory_cache_curves()`: cache, size_bytes, samples, refs, rate_ppm (the sample's mean rate, per million),
/// door (the doors' threshold), loss0_ns_per_s (H at 0), v_ns_per_s_per_gib (the next batch's price), knee_bytes,
/// floor_knee_bytes, kappa (at 0, 0.25, 0.5, 0.75 x size), hull (bytes:ns_per_s points), dropped (samples the hand-over
/// buffer dropped, all caches), own_bytes (this cache's tracker), track_bytes (the drain's: every tracker and both
/// hand-over buffers, on the memory watchdog's account), raw_ns_per_s (the predictions: the raw model at 50 / 25 / 10 %
/// of the size, before the hull), open_window (the open epoch: its seconds, then the LRU model's loss totals at 50 / 25
/// / 10 %; misses with the switch at 2, miss bytes at 3), validated (the curve passed C2's validation against C0 and ranks), standin (an
/// unvalidated cache's fixed curve, C0's measured slowdown: fraction of size:ns_per_s points; empty when none), open_hist
/// (the open epoch's losses by distance bin, bin:loss for each bin with any, `mempolicy::curve::bin_edges`: a validation
/// reads the LRU model at any size from it, A6).
pub fn fc_pgrust_memory_cache_curves(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pgrust_memory_cache_curves: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    if srf.tupdesc.natts as usize != CURVE_COLS {
        return Err(Box::new(types_error::PgError::error("the memory views' installed declaration is another build's: run scripts/memory-broker-views.sql again").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH))); // alloc: Cold
    }
    let Some(rows) = mempolicy::track::curves() else { return Ok(srf.finish(fcinfo)) };
    let dropped = mempolicy::track::dropped();
    let own = mempolicy::track::own_bytes();
    for r in &rows {
        let c = &r.curve;
        let mut hist = [(0usize, 0u64); mempolicy::curve::BINS];
        let mut nh = 0;
        // bounded("small-fixed: the curve's distance bins, BINS (149)")
        for (b, &x) in c.open_hist.iter().enumerate() {
            if x >= 1.0 {
                hist[nh] = (b, x as u64);
                nh += 1;
            }
        }
        let values: [Datum; CURVE_COLS] = [
            text_of(mcx, &[r.cache.name()], |n, s| n.write_str(s)),
            i8d(c.size),
            i8d(u64::from(c.samples)),
            i8d(c.refs),
            i8d((c.rate * 1e6) as u64),
            i8d(r.door),
            i8d(c.loss0 as u64),
            i8d((c.v * (1u64 << 30) as f64) as u64),
            i8d(c.knee),
            i8d(c.floor_knee),
            text_of(mcx, &c.kappa, |n, k| write!(n, "{k:.3}")),
            text_of(mcx, c.points(), |n, p| write!(n, "{}:{}", p.0, p.1 as u64)),
            i8d(dropped),
            i8d(r.bytes),
            i8d(own),
            text_of(mcx, &c.raw, |n, x| write!(n, "{}", *x as u64)),
            text_of(mcx, &[c.open_secs, c.open[0], c.open[1], c.open[2]], |n, x| write!(n, "{x:.1}")),
            Datum::from_bool(r.validated),
            text_of(mcx, mempolicy::track::standin(r.cache).unwrap_or(&[]), |n, p| write!(n, "{}:{}", p.0, p.1 as u64)),
            text_of(mcx, &hist[..nh], |n, p| write!(n, "{}:{}", p.0, p.1)),
        ];
        srf.putvalues(&values, &[false; CURVE_COLS])?;
    }
    Ok(srf.finish(fcinfo))
}
