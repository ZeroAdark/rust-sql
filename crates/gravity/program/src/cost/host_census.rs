//! The decision census of the program's cost home (cost_profile census.rs): each decision that compares prices of two
//! anchors, on each census host beside the reference.
use super::census::HOSTS;
use super::{codeif, drain_set, num_pair, octet, pgrc};
use pgrc::LaneDecode;

/// The runtime filter's idle share (`rt_div`, one anchor: the reference's on each host) and the most survivors of a granule a gather takes over the whole decode, per
/// encoding (one gather call, the survivors spread over the granule's frames of 1,024 values), on each host.
#[test]
fn the_scan_decisions_on_each_host() {
    const ROWS: u64 = 8192;
    let lanes = [
        ("byte_for/4 i32", LaneDecode::ByteFor { delta: 4 }, 4u8),
        ("byte_for/3 i32", LaneDecode::ByteFor { delta: 3 }, 4),
        ("verbatim i64", LaneDecode::Verbatim, 8),
        ("ffor i32", LaneDecode::Ffor, 4),
        ("delta_for i64", LaneDecode::DeltaFor, 8),
    ];
    let mut got = Vec::new();
    for (name, r, _) in HOSTS {
        let most = |d: LaneDecode, bytes: u8| (0..=ROWS).take_while(|&s| d.gather_pays_on(&r, bytes, ROWS, s, s.min(ROWS / 1024))).last().unwrap_or(0);
        let row: Vec<u64> = lanes.iter().map(|&(_, d, b)| most(d, b)).collect();
        println!("census\t{name}\tRT_DIV {}\tgather survivors {:?}", pgrc::rt_div_on(&r), lanes.iter().map(|l| l.0).zip(&row).collect::<Vec<_>>());
        got.push((pgrc::rt_div_on(&r), row));
    }
    assert!(got.iter().all(|g| g.0 == pgrc::RT_DIV), "one anchor: the reference's idle share on each host");
}

/// key.codeif's served break-even against its measured band, the octet lane's break-even and the drain set's crossover
/// rows, on each host.
#[test]
fn the_lowering_decisions_on_each_host() {
    for (name, r, _) in HOSTS {
        let served: Vec<f64> = [0u16, 8, 16].iter().map(|&w| codeif::codeif_entries_per_row_served_on(&r, w)).collect();
        let in_band = served.iter().all(|e| (codeif::CODEIF_BAND_LOW..=codeif::CODEIF_BAND_HIGH).contains(e));
        let lane: Vec<f64> = [0u16, 8].iter().map(|&w| octet::lane_entries_per_row_on(&r, w)).collect();
        let sets: Vec<f64> = [(true, 0u16), (true, 8), (false, 0), (false, 8)].iter().map(|&(word, w)| drain_set::set_rows_on(&r, word, w).round()).collect();
        println!("census\t{name}\tcodeif served W0/8/16 {served:.3?} in band {in_band}\toctet W0/8 {lane:.4?}\tdrain_set int W0/W8 text W0/W8 {sets:?}");
        assert!(in_band, "{name}: the band decides key.codeif as at the reference");
    }
}

/// num.pair's bound (keys past one byte take the pair up to 15 groups of 64 blocks): its two prices share one anchor, so
/// the bound is the reference's on each host.
#[test]
fn the_pair_bound_on_each_host() {
    for (name, r, _) in HOSTS {
        let pays = |g: u64| {
            let d = num_pair::displaced(g as f64, 64.0);
            (1.0 - d) * num_pair::HIT.on(&r) > d * num_pair::DISPLACED.on(&r)
        };
        let most = (1..=64).take_while(|&g| pays(g)).last().unwrap_or(0);
        println!("census\t{name}\tnum.pair groups of 64 blocks {most}");
        assert_eq!(most, 15, "{name}");
    }
}
