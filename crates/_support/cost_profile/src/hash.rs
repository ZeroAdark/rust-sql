use crate::machine::{Machine, L2_SHARE_FLOOR};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Load {
    pub num: u64,
    pub den: u64,
}

pub const HEAD_BYTES: u64 = 8;
pub const BUILD_LOAD: Load = Load { num: 1, den: 2 };
pub const SPARSE_LOADS: [Load; 2] = [Load { num: 1, den: 8 }, Load { num: 1, den: 4 }];
pub const BUILD_SPARSE_LOAD: Load = SPARSE_LOADS[0];
pub const SPARSE_L2: Load = Load { num: 1, den: 4 };
pub const BUILD_MIN_CAP_BITS: u32 = 10;
pub const HASHBUILD_ENTRY_BYTES: u64 = 16;
pub const HASHBUILD_L2: Load = Load { num: 1, den: 8 };

/// abi's HashBuild tables of at most this many rows sit at `BUILD_SPARSE_LOAD` on machine `m`: the rows whose sparse
/// table fits `HASHBUILD_L2` of its L2 share (2,048 on the reference machine; 256 on a c6a thread's 256 KiB).
pub const fn hashbuild_sparse_rows(m: &Machine) -> u64 {
    m.l2_share() / HASHBUILD_L2.den * HASHBUILD_L2.num / HASHBUILD_ENTRY_BYTES / BUILD_SPARSE_LOAD.den * BUILD_SPARSE_LOAD.num
}

pub const fn rows_within(load: Load, m: &Machine) -> u64 {
    let heads = m.l2_share() / SPARSE_L2.den * SPARSE_L2.num / HEAD_BYTES;
    heads / load.den * load.num
}

pub const fn build_load(rows: u64, m: &Machine) -> Load {
    let mut i = 0;
    while i < SPARSE_LOADS.len() {
        if rows <= rows_within(SPARSE_LOADS[i], m) {
            return SPARSE_LOADS[i];
        }
        i += 1;
    }
    BUILD_LOAD
}

const fn ceil_log2(n: u64) -> u32 {
    if n <= 1 {
        0
    } else {
        64 - (n - 1).leading_zeros()
    }
}

pub const fn cap_bits_at(rows: u64, load: Load) -> u32 {
    let want = (rows as u128 * load.den as u128).div_ceil(load.num as u128);
    let want = if want > u64::MAX as u128 { u64::MAX } else { want as u64 };
    let bits = ceil_log2(want);
    if bits < BUILD_MIN_CAP_BITS {
        BUILD_MIN_CAP_BITS
    } else {
        bits
    }
}

pub const fn build_cap_bits(rows: u64, m: &Machine) -> u32 {
    cap_bits_at(rows, build_load(rows, m))
}

pub const DENSE_VALUE_BYTES: u64 = 4;
pub const DENSE_REACH: Load = Load { num: 1, den: 1 };

pub const fn dense_span_max(rows: u64, m: &Machine) -> u64 {
    if rows == 0 {
        return 0;
    }
    let dir = (HEAD_BYTES as u128) << build_cap_bits(rows, m);
    let span = dir * DENSE_REACH.num as u128 / DENSE_REACH.den as u128 / DENSE_VALUE_BYTES as u128;
    if span > u64::MAX as u128 {
        u64::MAX
    } else {
        span as u64
    }
}

/// The capacity bits of abi's HashBuild for `rows` rows on machine `m`: `BUILD_SPARSE_LOAD` up to
/// `hashbuild_sparse_rows`, else `BUILD_LOAD`.
pub const fn hashbuild_cap_bits(rows: u64, m: &Machine) -> u32 {
    cap_bits_at(rows, if rows <= hashbuild_sparse_rows(m) { BUILD_SPARSE_LOAD } else { BUILD_LOAD })
}

pub const AGG_GROW_LOAD: Load = Load { num: 3, den: 4 };
pub const AGG_MIN_CAP_BITS: u32 = 4;

pub const fn agg_grow_at(cap: u64) -> u64 {
    (cap as u128 * AGG_GROW_LOAD.num as u128 / AGG_GROW_LOAD.den as u128) as u64
}

pub const fn agg_cap_bits(groups: u64) -> u32 {
    let mut bits = AGG_MIN_CAP_BITS;
    while bits < 64 && agg_grow_at(1u64 << bits) < groups {
        bits += 1;
    }
    bits
}

/// abi's phase-1 table (`T1`: a two-pass top's pass 1, a partitioned aggregate's phase 1) lays its slots in at most this
/// many bytes of a participant's share (abi `T1_DEFAULT_BYTES`, asserted equal there); the rest past its header and the
/// slots' side words is its partition pool.
pub const T1_TABLE_BYTES: u64 = 512 << 10;
/// the phase-1 table's slot bytes on machine `m`: a quarter of one thread's L2 share (the driver's `machine_slice`),
/// `T1_TABLE_BYTES` on the reference machine (2 MiB L2). MEASURED (sweep-D06: flat from a quarter to twice 512 KiB on the
/// reference, 2.1x slower at 2 MiB, a whole L2). The planner (gravity_paths, via `t1_pool_records`) and the run time (abi
/// `t1_default_bytes`) both read it over `machine::host()`, so plan cost and run time agree.
pub const fn t1_table_bytes(m: &Machine) -> u64 {
    let q = (if m.l2 < L2_SHARE_FLOOR { L2_SHARE_FLOOR } else { m.l2 }) / 4;
    1u64 << (63 - q.leading_zeros())
}
/// The most a phase-1 table opened at a group bound takes (`t1_fit_bytes`): the reference's table.
pub const T1_FIT_MAX_BYTES: u64 = T1_TABLE_BYTES;
/// The phase-1 table's slot bytes on machine `m` for a fold opened at a bound of `groups` groups of `rec`-byte records:
/// two slots a group as a power of two when that is past `t1_table_bytes` and within `T1_FIT_MAX_BYTES`, else
/// `t1_table_bytes` (no bound, or groups no table under the cap holds). MEASURED (c6a, a 128 KiB default: a fold whose
/// groups fit the table runs 2.1 .. 2.3x faster than one that overflows it, and an overflowing fold pays 3 .. 6 % for a
/// larger table). `t1_table_bytes` on the reference machine for every bound.
pub const fn t1_fit_bytes(groups: u64, rec: u64, m: &Machine) -> u64 {
    let q = t1_table_bytes(m);
    if groups == 0 || rec == 0 || groups > 1 << 40 {
        return q;
    }
    let fit = (2 * groups).next_power_of_two().saturating_mul(rec);
    let max = if q > T1_FIT_MAX_BYTES { q } else { T1_FIT_MAX_BYTES };
    if fit > q && fit <= max {
        fit
    } else {
        q
    }
}
pub const T1_HDR_BYTES: u64 = 160;
pub const T1_FOLD_PARTS: u64 = 64;
pub const T1_FOLD_ROOM: Load = Load { num: 3, den: 4 };

/// The records one participant's phase-1 pool holds at a share of `room` bytes and a `rec`-byte record, the table opened
/// at a bound of `groups` groups (0: none): abi `T1::at_sized`'s layout (the header and [`T1_FOLD_PARTS`] partitions'
/// words, slots in at most [`t1_fit_bytes`] and half the rest as a power of two, their partition and opened-slot words). The table folds or spills only at a flush,
/// and a flush comes only when the pool is full (kernels `agg_flush`), so records that never fill it never spill.
/// 8 MiB and 32 B: 243,427 (measured 243,200 at W8, 512 blocks of 475; 243,236 at W4, 238 of 1,022); 8 MiB and 40 B:
/// 200,578 (measured 200,165 at W2, 245 blocks of 817).
pub const fn t1_pool_records(room: u64, rec: u64, groups: u64, m: &Machine) -> u64 {
    let rec = if rec < 8 { 8 } else { rec };
    let meta = T1_HDR_BYTES + 12 * T1_FOLD_PARTS;
    if room <= meta {
        return 0;
    }
    let avail = room - meta;
    let t1 = t1_fit_bytes(groups, rec, m);
    let table = if t1 < avail / 2 { t1 } else { avail / 2 };
    let slots = table / rec;
    let cap = if slots < 2 { 2 } else { 1u64 << (63 - slots.leading_zeros()) };
    let opened = if cap / 8 < 2 { 2 } else { cap / 8 };
    avail.saturating_sub(cap * rec + 4 * cap + 4 * opened) / rec
}

pub const fn t1_fold_records(room: u64, rec: u64, groups: u64, m: &Machine) -> u64 {
    t1_pool_records(room, rec, groups, m) * T1_FOLD_ROOM.num / T1_FOLD_ROOM.den
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phase_1_pool_keeps_three_quarters_of_its_records() {
        let r = Machine::REFERENCE;
        assert_eq!(t1_table_bytes(&r), T1_TABLE_BYTES, "D06: the reference keeps 512 KiB");
        assert_eq!(t1_table_bytes(&Machine::detected(None, 512 << 10, 2, 8)), 128 << 10, "a c6a core: 512 KiB / 4");
        assert_eq!((t1_pool_records(8 << 20, 32, 0, &r), t1_fold_records(8 << 20, 32, 0, &r)), (243_427, 182_570));
        assert_eq!(t1_pool_records(8 << 20, 40, 0, &r), 200_578, "D2078's extreme pass-1 record (measured 245 blocks of 817)");
        assert_eq!(t1_fold_records(T1_HDR_BYTES, 32, 0, &r), 0);
        assert!(t1_fold_records(16 << 20, 32, 0, &r) > 2 * t1_fold_records(8 << 20, 32, 0, &r), "the table stays at 512 KiB");
    }

    #[test]
    fn a_bound_sizes_the_table_that_holds_it_up_to_the_reference_table() {
        let (r, c6a) = (Machine::REFERENCE, Machine::detected(None, 512 << 10, 2, 8));
        for g in [0, 1, 3_000, 10_000, 1 << 20] {
            assert_eq!(t1_fit_bytes(g, 32, &r), T1_TABLE_BYTES, "the reference keeps 512 KiB at any bound ({g})");
            assert_eq!(t1_pool_records(8 << 20, 32, g, &r), 243_427);
        }
        assert_eq!(t1_fit_bytes(0, 32, &c6a), 128 << 10, "no bound: the default");
        assert_eq!(t1_fit_bytes(1_000, 32, &c6a), 128 << 10, "2,048 slots fit the default");
        assert_eq!(t1_fit_bytes(3_000, 32, &c6a), 256 << 10, "8,192 slots of 32 bytes");
        assert_eq!(t1_fit_bytes(8_000, 32, &c6a), 512 << 10, "16,384 slots: the cap");
        assert_eq!(t1_fit_bytes(10_000, 32, &c6a), 128 << 10, "32,768 slots are past the cap: the default, no table holds them");
        assert_eq!(t1_fit_bytes(3_000, 48, &c6a), 384 << 10, "a record that is not a power of two");
        assert!(t1_pool_records(8 << 20, 32, 3_000, &c6a) < t1_pool_records(8 << 20, 32, 0, &c6a), "the larger table leaves less pool");
    }

    #[test]
    fn the_reference_machine_takes_the_fitted_bounds() {
        let m = Machine::REFERENCE;
        assert_eq!((rows_within(SPARSE_LOADS[0], &m), rows_within(SPARSE_LOADS[1], &m)), (8192, 16384));
        assert_eq!([2048, 8192, 8193, 16384, 16385].map(|r| build_load(r, &m).den), [8, 8, 4, 4, 2]);
        assert_eq!((build_cap_bits(8192, &m), build_cap_bits(16384, &m), build_cap_bits(16385, &m)), (16, 16, 16));
        assert_eq!((build_cap_bits(0, &m), build_cap_bits(1 << 20, &m), build_cap_bits((1 << 20) + 1, &m)), (10, 21, 22));
    }

    #[test]
    fn an_smt_thread_holds_fewer_sparse_records() {
        let m = Machine::detected(Some(32 << 10), 512 << 10, 2, 8);
        assert_eq!((rows_within(SPARSE_LOADS[0], &m), rows_within(SPARSE_LOADS[1], &m)), (1024, 2048));
        assert_eq!([1024, 1025, 2048, 2049].map(|r| build_load(r, &m).den), [8, 4, 4, 2]);
    }

    #[test]
    fn a_dense_span_follows_the_keyhash_directory() {
        let m = Machine::REFERENCE;
        let reach = |heads: u64| heads * HEAD_BYTES * DENSE_REACH.num / DENSE_REACH.den / DENSE_VALUE_BYTES;
        assert_eq!(dense_span_max(0, &m), 0);
        assert_eq!([8192, 16384, 1 << 18, (1 << 18) + 1].map(|r| dense_span_max(r, &m)), [1 << 16, 1 << 16, 1 << 19, 1 << 20].map(reach));
        assert_eq!(dense_span_max(u64::MAX, &m), u64::MAX);
    }

    #[test]
    fn abis_hashbuild_keeps_its_bound() {
        let r = Machine::REFERENCE;
        assert_eq!(hashbuild_sparse_rows(&r), 2048, "D155's measured bound on the reference machine");
        assert_eq!(hashbuild_sparse_rows(&Machine::detected(None, 256 << 10, 1, 16)), 256);
        assert_eq!((hashbuild_cap_bits(0, &r), hashbuild_cap_bits(2048, &r), hashbuild_cap_bits(2049, &r)), (10, 14, 13));
        assert_eq!((hashbuild_cap_bits(1 << 20, &r), hashbuild_cap_bits((1 << 20) + 1, &r)), (21, 22));
    }

    #[test]
    fn every_build_fits_its_directory_at_its_load() {
        for m in [Machine::REFERENCE, Machine::detected(Some(32 << 10), 512 << 10, 2, 8)] {
            for rows in (0..200_000u64).step_by(997).chain([1023, 1024, 1025, 8191, 8192, 8193, 16384, 16385, 1 << 31, u64::MAX / 4]) {
                let (bits, l) = (build_cap_bits(rows, &m), build_load(rows, &m));
                let cap = 1u128 << bits;
                assert!(rows as u128 * l.den as u128 <= cap * l.num as u128, "{rows} rows in {cap} heads");
                assert!(bits == BUILD_MIN_CAP_BITS || rows as u128 * l.den as u128 > (cap / 2) * l.num as u128, "{rows}: the least power of two");
            }
        }
    }
}
