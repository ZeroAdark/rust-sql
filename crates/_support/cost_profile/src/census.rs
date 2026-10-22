//! The decision census: the hosts whose ratios each decision is read at beside the reference's. A decision between prices
//! of one anchor gives the reference's answer on every host; one between prices of two anchors may not, and the census
//! tests (here, gravity_program cost/census.rs, lowerv4 fetch_shape.rs) print each such decision on each host. The rows
//! that differ from the reference's are the full set to validate on that host.
use crate::anchor::Ratios;
use crate::machine::Machine;

/// c6a.4xlarge (AMD EPYC 7R13, x86-64, SMT 2, 512 KiB of L2 a core; the release build, no target-cpu), as its server
/// measured the anchors at start: read 1.25, gather 0.875, check 2.25, miss 0.75 (0.75 .. 0.875 over five starts; no price
/// reads it), update 0.75, sort 1.
pub const C6A: Ratios = Ratios([1.25, 0.875, 2.25, 0.75, 0.75, 1.0]);
/// c7g.4xlarge (Graviton 3, Neoverse V1): read 1, gather 1.25, check 1.25, miss 1.125 (1 .. 1.125 over five starts; no
/// price reads it), update 1.125, sort 1.125.
pub const C7G: Ratios = Ratios([1.0, 1.25, 1.25, 1.125, 1.125, 1.125]);

/// c6a's cache facts (sysfs): 32 KiB L1d and 512 KiB L2 a core, two threads a core.
pub const C6A_MACHINE: Machine = Machine { l1d: 32 << 10, l2: 512 << 10, line: 64, smt: 2, prefetch: crate::machine::SMT_PREFETCH };
/// c7g's cache facts (sysfs): 64 KiB L1d and 1 MiB L2 a core, one thread a core.
pub const C7G_MACHINE: Machine = Machine { l1d: 64 << 10, l2: 1 << 20, line: 64, smt: 1, prefetch: Machine::REFERENCE.prefetch };

/// The census's hosts, the reference first: their ratios and their cache facts.
pub const HOSTS: [(&str, Ratios, Machine); 3] = [("reference", Ratios::ONE, Machine::REFERENCE), ("c6a", C6A, C6A_MACHINE), ("c7g", C7G, C7G_MACHINE)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::front::{light_dense_on, light_way_rows_a_code_on, light_ways_max_codes_on};
    use crate::sparse::dense_pct_on;
    use crate::walk::{key_presence, Walk};

    /// The light front's two break-evens and its ways' bound, code.sparse's share (one anchor: the reference's on each),
    /// and the cache-derived sizes, on each host.
    #[test]
    fn the_front_and_sparse_decisions_on_each_host() {
        let got: Vec<[u64; 6]> = HOSTS
            .iter()
            .map(|(_, r, m)| [light_dense_on(r), light_way_rows_a_code_on(r), light_ways_max_codes_on(m), dense_pct_on(r), m.l1_span(8, 1, 2), m.l2_share()])
            .collect();
        for ((name, ..), g) in HOSTS.iter().zip(&got) {
            println!("census\t{name}\tLIGHT_DENSE {}\tLIGHT_WAY_ROWS_A_CODE {}\tLIGHT_WAYS_MAX_CODES {}\tsparse DENSE_PCT {}\tSUM chunk {}\tL2 share {}", g[0], g[1], g[2], g[3], g[4], g[5]);
        }
        assert_eq!(got, [[3, 4, 4096, 70, 4096, 2 << 20], [2, 6, 1024, 70, 1024, 256 << 10], [3, 3, 4096, 70, 4096, 1 << 20]]);
    }

    /// The measured walk's shares (walk over plain: under 1 walks) at the walk tests' cells, on each host.
    #[test]
    fn the_walk_shares_on_each_host() {
        let cells = |w: &Walk| {
            [
                w.share(128, 256, 0, false),
                w.share(1024, 2048, 8, false),
                w.group_share(256, 1024, 256, 256, 0.0, 1.0, 0),
                w.group_share_of(365, 366, 4, 4, 1.0, 1.0, 22.0, 0),
                w.group_share_of(100, 366, 10, 10, 1.0, key_presence(8192.0, 2.0e6), 2.0e6, 0),
                w.group_share_of(250, 366, 10, 10, 1.0, 1.0, 121.0, 8),
            ]
        };
        let reference = cells(&Walk::MEASURED);
        for (name, r, _) in HOSTS {
            let at = cells(&Walk { ratios: r, ..Walk::MEASURED });
            let flips: Vec<bool> = at.iter().zip(&reference).map(|(a, b)| (*a < 1.0) != (*b < 1.0)).collect();
            println!("census\t{name}\twalk shares {at:.3?}\tflips {flips:?}");
        }
    }
}
