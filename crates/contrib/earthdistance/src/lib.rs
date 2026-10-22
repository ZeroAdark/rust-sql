//! contrib/earthdistance — great-circle distances. The module is almost
//! entirely SQL over the cube extension (earth domain, ll_to_earth,
//! earth_distance, earth_box — see extension/earthdistance--1.1.sql); the
//! single C function is `geo_distance(point, point)`, the haversine formula
//! over statute miles.

use datum::Datum;
use types_core::geo::Point;
use types_error::PgResult;
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};

const LIBRARY: &str = "earthdistance";

/// Earth's radius in statute miles (earthdistance.c).
const EARTH_RADIUS: f64 = 3958.747716;
const TWO_PI: f64 = 2.0 * std::f64::consts::PI;

fn degtorad(degrees: f64) -> f64 {
    (degrees / 360.0) * TWO_PI
}

/// `geo_distance_internal`: distance in miles between (longitude, latitude)
/// degree pairs, haversine form.
pub fn geo_distance_internal(pt1: &Point, pt2: &Point) -> f64 {
    let lat1 = degtorad(pt1.y);
    let long2 = degtorad(pt2.x);
    let lat2 = degtorad(pt2.y);

    // earthdistance.c:71 (D400)
    let mut longdiff = ::fp_contract::fnmsub(pt1.x / 360.0, TWO_PI, long2).abs();
    if longdiff > std::f64::consts::PI {
        longdiff = TWO_PI - longdiff;
    }

    let half_lat = (lat1 - lat2).abs() / 2.0;
    let sl = (longdiff / 2.0).sin();
    // earthdistance.c:75 (D400)
    let mut sino = ::fp_contract::fmadd(
        half_lat.sin(),
        half_lat.sin(),
        lat1.cos() * lat2.cos() * sl * sl,
    )
    .sqrt();
    if sino > 1.0 {
        sino = 1.0;
    }

    2.0 * EARTH_RADIUS * sino.asin()
}

fn fc_geo_distance(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: strict fn; catalog args are non-null 16-byte point datums.
    let pt1 = unsafe { Point::from_datum_bytes(fcinfo.arg_fixed(0, 16)) };
    let pt2 = unsafe { Point::from_datum_bytes(fcinfo.arg_fixed(1, 16)) };
    Ok(Datum::from_f64(geo_distance_internal(&pt1, &pt2)))
}

fn lookup(name: &str) -> Option<PGFunction> {
    Some(match name {
        "geo_distance" => fc_geo_distance,
        _ => return None,
    })
}

pub fn init_seams() {
    dfmgr::register_builtin_library(dfmgr::BuiltinLibraryEntry {
        name: LIBRARY,
        lookup,
        pg_init: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_distances_match_c() {
        // London (lon -0.1257, lat 51.508) to Paris (lon 2.3522, lat 48.8566):
        // roughly 213 miles.
        let london = Point { x: -0.1257, y: 51.508 };
        let paris = Point { x: 2.3522, y: 48.8566 };
        let d = geo_distance_internal(&london, &paris);
        assert!((d - 213.0).abs() < 2.0, "got {d}");
        let same = if cfg!(target_arch = "aarch64") { 1.0651112734535068e-16 } else { 0.0 };
        assert_eq!(geo_distance_internal(&london, &london), same);
        // Longitude wrap: 179.9W to 179.9E stays short.
        let a = Point { x: -179.9, y: 0.0 };
        let b = Point { x: 179.9, y: 0.0 };
        let d = geo_distance_internal(&a, &b);
        assert!(d < 20.0, "got {d}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn d400_fma_sites_match_platform_pg() {
        let arm = cfg!(target_arch = "aarch64");
        let pick = |a: f64, x: f64| if arm { a } else { x };
        let d = |x1, y1, x2, y2| geo_distance_internal(&Point { x: x1, y: y1 }, &Point { x: x2, y: y2 });
        assert_eq!(d(82.5, 4.0, -52.8, 67.6), pick(7038.6030550144615, 7038.603055014461));
        assert_eq!(d(132.2, -21.8, -0.1, -31.1), pick(7605.400875593729, 7605.400875593726));
        assert_eq!(d(-179.9, 36.4, -174.2, -17.6), pick(3749.5628066551703, 3749.5628066551712));
        assert_eq!(d(-33.6, -12.5, -59.9, 79.5), pick(6429.551872116938, 6429.551872116937));
        assert_eq!(d(1.1, 2.0, 1.1, 2.0), pick(6.417077911064611e-15, 0.0));
    }
}
