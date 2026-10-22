use crate::anchor::{self, Anchor, Ratios};
use crate::fit::{break_even_div, Fitted};
use crate::machine::{self, Machine};

pub const LIGHT_CODE_BYTES: u64 = 8;
/// The largest part dictionary (or product of the keys' dictionaries) the light front counts: a memory bound, not a
/// crossover. The arrays hold `LIGHT_CODE_BYTES` a code, a power of two of codes, so at most 8 MiB a participant; a part a
/// default cut seals holds at most 2^20 rows, so it fits. Up to it the hashed table never overtook the front where the
/// claim filled the part's arrays (`LIGHT_MAX_CORPUS`); a claim that cannot is `light_claim_takes`'.
pub const LIGHT_MAX_CODES: u64 = 1 << 20;
pub const LIGHT_MAX_MACHINE: &str = "box genlight: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU; checked on c6a.4xlarge (AMD EPYC 7R13)";
pub const LIGHT_MAX_CORPUS: &str = "dev tables of one text key, 16 K .. 4.8 M codes, parts of up to 2^20 rows at 1 .. 8 rows a code, uniform and \
                                    skewed, short and 33-byte keys; every part hashed against the front, W0 + W8";

const MACHINE: &str = "box 10: c8g, Graviton 4 (Neoverse V2), 16 vCPU";
const CORPUS: &str = "light front walk sweep (kernels front_light.rs fit, dev seed)";

/// A code's step of the walk in code order: a sequential read of the counts (the read anchor).
pub const LIGHT_WALK: Fitted = Fitted { ns: 1.50, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read };
/// A touched code's step of the list: the count at a random position (the gather anchor).
pub const LIGHT_LIST: Fitted = Fitted { ns: 3.46, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Gather };
/// The convert walks a part's counts in code order when the part touched at least one code in this many, else its
/// list: `break_even_div` of the two walks' unit costs on a host of ratios `r` (a gather over a read: 3 on the reference
/// fit).
pub const fn light_dense_on(r: &Ratios) -> u64 {
    break_even_div(LIGHT_LIST.on(r), LIGHT_WALK.on(r)) as u64
}
/// `light_dense_on` at the reference.
pub const LIGHT_DENSE: u64 = light_dense_on(&Ratios::ONE);
/// `light_dense_on` on this host (the installed profile): the kernel's convert reads it.
pub fn light_dense() -> u64 {
    light_dense_on(anchor::ratios())
}

pub const LIGHT_ROWS_A_COUNT: u64 = 2;

pub const LIGHT_TOUCHED_TENTHS: u64 = 24;

pub const LIGHT_WAYS: u64 = 4;
/// A row's count into its way (the update anchor: a counted slot a row).
pub const LIGHT_WAY_ROW: Fitted = Fitted { ns: 1.165, machine: WAYS_MACHINE, corpus: WAYS_CORPUS, anchor: Anchor::Update };
/// A code's step of the ways' walk in code order (the read anchor).
pub const LIGHT_WAY_WALK: Fitted = Fitted { ns: 3.60, machine: WAYS_MACHINE, corpus: WAYS_CORPUS, anchor: Anchor::Read };
const WAYS_MACHINE: &str = "box 88: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
const WAYS_CORPUS: &str = "dev table lw (32 parts x 196,608 rows, 16 to 65,536 codes a part, uniform / runs / skewed; tm.sh 3 \
                           takes x 5 runs, ways never vs always) + kernels front_light.rs fit (one core, 3 runs)";
/// The rows a code a part needs to count in ways: the ways' walk a code over a row's count, on a host of ratios `r` (a
/// read over an update: 4 on the reference fit).
pub const fn light_way_rows_a_code_on(r: &Ratios) -> u64 {
    break_even_div(LIGHT_WAY_WALK.on(r), LIGHT_WAY_ROW.on(r)) as u64
}
/// `light_way_rows_a_code_on` at the reference.
pub const LIGHT_WAY_ROWS_A_CODE: u64 = light_way_rows_a_code_on(&Ratios::ONE);
/// The most codes a part counts in ways on a machine `m`: the ways' four `u32` counts a code within one thread's L1d.
pub const fn light_ways_max_codes_on(m: &Machine) -> u64 {
    m.l1_span(LIGHT_WAYS * 4, 1, 1)
}
/// `light_ways_max_codes_on` the reference (64 KiB: 4,096 codes).
pub const LIGHT_WAYS_MAX_CODES: u64 = light_ways_max_codes_on(&Machine::REFERENCE);
/// `light_ways_max_codes_on` this host's cache facts (`machine::host`): the lowering's admission of the ways reads it.
pub fn light_ways_max_codes() -> u64 {
    light_ways_max_codes_on(machine::host())
}

pub const fn light_ways_on(r: &Ratios, m: &Machine, ncodes: u64, rows: u64) -> bool {
    ncodes <= light_ways_max_codes_on(m) && ncodes.saturating_mul(light_way_rows_a_code_on(r)) <= rows
}

/// `light_ways_on` on this host (the installed profile and `machine::host`): the kernel's bind reads it.
pub fn light_ways(ncodes: u64, rows: u64) -> bool {
    light_ways_on(anchor::ratios(), machine::host(), ncodes, rows)
}

/// Whether a claim of `rows` rows (its granules' windows) takes the light front over a part of `ncodes` codes on a host of
/// ratios `r`: its rows could touch one code in `light_dense_on`, the convert's walk in code order. A claim of fewer rows
/// (W8's work stealing cuts a table of few parts into claims of ~4 granules) counts about one row a touched code and walks
/// its list at random, where a row of the hashed table costs less: on `LIGHT_MAX_MACHINE` at W8, the front over every claim
/// took 1.06 .. 1.15x the hashed table's time at 262,144 codes and 1.03 .. 1.51x at 600 K and 2.4 M, 1 to 3 rows a code. A
/// claim it refuses takes the hashed table, so a part's claims may count either way.
pub const fn light_claim_takes_on(r: &Ratios, ncodes: u64, rows: u64) -> bool {
    rows.saturating_mul(light_dense_on(r)) >= ncodes
}

/// `light_claim_takes_on` on this host: the kernel's bind reads it.
pub fn light_claim_takes(ncodes: u64, rows: u64) -> bool {
    light_claim_takes_on(anchor::ratios(), ncodes, rows)
}

/// The most dictionary entries a scan column's code stays the representative of a key past a join (lowerv4 `code_pays`):
/// the light front's bound before it became a memory bound. Past it the code paid per entry more than it saved a row (a
/// 250,000-entry key over 400,000 rows: 147 ms as the value, 220 ms as the code, box 24 W0).
pub const CODE_KEY_MAX_ENTRIES: u64 = 1 << 18;

/// Whether a column of `entries` dictionary entries over `rows` rows keeps its code as a key's representative past a join:
/// at most `CODE_KEY_MAX_ENTRIES` entries and `LIGHT_ROWS_A_COUNT` rows an entry.
pub const fn code_key_pays(entries: u64, rows: u64) -> bool {
    entries <= CODE_KEY_MAX_ENTRIES && entries.saturating_mul(LIGHT_ROWS_A_COUNT) <= rows
}

/// The largest planner group estimate two or more code keys take the light front at (lowerv4 codefront_light.rs): the
/// front's bound before it became a memory bound. A part's product of dictionary sizes counts only within `light_takes`'
/// window rows (4,096 a window), so a statement estimated past it would pay the index on every row of the parts the bind
/// refuses.
pub const LIGHT_KEYS_MAX_GROUPS: u64 = 1 << 18;

pub const fn light_takes(product: u64, rows: u64) -> bool {
    product <= LIGHT_MAX_CODES && product.saturating_mul(LIGHT_ROWS_A_COUNT) <= rows
}

pub const fn light_keeps(touched: u64, rows: u64) -> bool {
    touched.saturating_mul(LIGHT_TOUCHED_TENTHS) <= rows.saturating_mul(10)
}

const PERCODE_MACHINE: &str = "box gentopn: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU, held whole";
/// The dev data (never a benchmark's tables or statements, the census or a bar cell): pgrcolumnar tables of two 1M-row
/// parts, a text column `k<key>/<leaf>` grouped by a key derived from it (regexp_replace) with a count(*) FILTER (LIKE)
/// beside it (the per-code front's window fold), runs of r = 1 .. 64 rows a key (a fold opens one map record in r rows)
/// and 1 .. 32 codes a key; each table timed with the front kept (no decline) and declined at its first fold, W0, one take
/// each (A/A 0.98 .. 1.02), with the kernel's sums of listed codes and opened records. The fit: least squares of the two
/// forms' difference a row over the listed codes and the opened records a row, through the cells of runs up to 8 rows of
/// 1 or 8 codes a key, where the per-row path pays a record's probe a row. Past them the per-row path's cost moves with
/// what a fold's counts do not carry: at 0.5 codes a row, 4 codes a key in runs of 8 keep the front (0.84x) where 8 codes
/// a key in runs of 16 decline (1.18x). So cells of long runs over 8 .. 32 codes a key keep the front where declining
/// wins by 2 .. 45 % (+2 .. +22 ms), as the 50 % opened rule before it kept it; the rule is never dearer than that rule
/// on a measured cell. The points and the fit's check: gravity_program tests/cost_fit.rs `percode_front_fit`.
const PERCODE_CORPUS: &str = "D-04 per-code front sweep (2 x 1M-row parts, derived key, runs of 1 .. 64 rows, 1 .. 32 codes a key, W0, kept vs declined)";

/// The per-code front's three prices are each a probe or a store into a hashed record (the update anchor), so their
/// weights hold on every host.
///
/// The per-row path's cost a row that the per-code front's fold spares.
pub const PERCODE_ROW: Fitted = Fitted { ns: 47.80, machine: PERCODE_MACHINE, corpus: PERCODE_CORPUS, anchor: Anchor::Update };
/// The front's cost a code its window lists: the count, the list entry and the code's fold into the part's map.
pub const PERCODE_CODE: Fitted = Fitted { ns: 37.69, machine: PERCODE_MACHINE, corpus: PERCODE_CORPUS, anchor: Anchor::Update };
/// The front's cost a map record its folds open: the record and its relay into the claim's table.
pub const PERCODE_OPENED: Fitted = Fitted { ns: 64.88, machine: PERCODE_MACHINE, corpus: PERCODE_CORPUS, anchor: Anchor::Update };

/// `cost` in 256ths of `row`, rounded: a weight the decline's integer test reads.
pub const fn per_256(cost: f64, row: f64) -> u64 {
    if cost.is_nan() || row.is_nan() || cost <= 0.0 || row <= 0.0 {
        return 0;
    }
    (cost * 256.0 / row + 0.5) as u64
}

/// The decline's weights (`percode_declines`): a listed code and an opened record in 256ths of a row (202 and 347 on the
/// reference fit).
pub const PERCODE_CODE_W: u64 = per_256(PERCODE_CODE.ns, PERCODE_ROW.ns);
pub const PERCODE_OPENED_W: u64 = per_256(PERCODE_OPENED.ns, PERCODE_ROW.ns);

/// Whether a fold of `codes` listed codes over `rows` rows that opened `opened` map records costs the front at least what
/// the per-row path pays for those rows, under weights `code_w` / `opened_w` (256ths of a row's): the front declines. An
/// empty fold never does.
pub const fn percode_declines(codes: u64, rows: u64, opened: u64, code_w: u64, opened_w: u64) -> bool {
    rows != 0 && (code_w as u128 * codes as u128 + opened_w as u128 * opened as u128) >= 256 * rows as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_per_code_front_declines_where_its_codes_and_records_outweigh_the_rows() {
        assert_eq!((PERCODE_CODE_W, PERCODE_OPENED_W), (202, 347), "37.69 and 64.88 ns over 47.80 ns, in 256ths");
        let d = |codes, rows, opened| percode_declines(codes, rows, opened, PERCODE_CODE_W, PERCODE_OPENED_W);
        assert!(d(4096, 4096, 4096), "every row a fresh code and a fresh record");
        assert!(d(2048, 4096, 2048), "two rows a code and a record: the sweep's tie (1.02x)");
        assert!(!d(1024, 4096, 1024), "four rows a code and a record: the front 0.75x");
        assert!(d(4096, 4096, 1024), "fresh codes, a record in four rows: the front 1.10x");
        assert!(!d(4096, 4096, 300), "fresh codes whose records repeat (one key over many codes)");
        assert!(!d(0, 0, 0), "an empty fold");
        assert!(!percode_declines(4096, 4096, 4096, 0, 0), "no weights: never");
        assert_eq!(per_256(1.0, 0.0), 0);
    }

    #[test]
    fn the_light_front_derives_its_bounds() {
        assert_eq!(LIGHT_MAX_CODES * LIGHT_CODE_BYTES, 8 << 20, "8 MiB of arrays a participant at most");
        assert_eq!(LIGHT_DENSE, 3, "the fitted break-even: 3.46 / 1.50, rounded up");
    }

    #[test]
    fn the_light_front_counts_every_part_a_default_cut_seals_where_its_claim_fills_the_arrays() {
        assert!(LIGHT_MAX_CODES >= 1 << 20, "a part of 2^20 rows (the default cut) at one row a code");
        assert!(LIGHT_MAX_CODES >= 600_000, "parts of 600 K codes: the front 0.69 .. 0.96x the hashed table at W0 on c8g");
        let (part, granule) = (1u64 << 20, 8192u64);
        for (r, dense) in [(Ratios::ONE, 3), (crate::census::C6A, 2)] {
            let takes = |codes, rows| light_claim_takes_on(&r, codes, rows);
            assert!(takes(part, 128 * granule), "a claim of a whole part (W0's): its convert walks in code order");
            assert!(!takes(part, 4 * granule), "4 granules over 1 M codes (a W8 claim's cut, ~1 row a touched code)");
            assert!(takes(65536, 4 * granule), "4 granules over 64 K codes");
            assert!(takes(dense * 1000, 1000) && !takes(dense * 1000 + 1, 1000), "one code in light_dense touched at best");
            assert!(takes(part, u64::MAX), "a claim of every granule");
        }
    }

    #[test]
    fn a_key_past_a_join_keeps_its_code_up_to_the_old_bound() {
        assert_eq!(CODE_KEY_MAX_ENTRIES, 1 << 18);
        assert_eq!(LIGHT_KEYS_MAX_GROUPS, 1 << 18, "two or more keys: the planner's estimate gate stays the old bound");
        assert!(code_key_pays(1 << 18, 1 << 19) && !code_key_pays((1 << 18) + 1, u64::MAX), "2^18 entries at 2 rows an entry");
        assert!(!code_key_pays(250_000, 400_000), "a 250,000-entry key over 400,000 rows keeps the value");
    }

    #[test]
    fn the_ways_take_a_part_of_few_codes_at_enough_rows_a_code() {
        assert_eq!(LIGHT_WAY_ROWS_A_CODE, 4, "the measured 3.60 / 1.165, rounded up");
        assert_eq!(LIGHT_WAYS_MAX_CODES, 4096, "64 KiB of four u32 counts a code: the reference L1d");
        let ways = |ncodes, rows| light_ways_on(&Ratios::ONE, &Machine::REFERENCE, ncodes, rows);
        assert!(ways(2048, 8192) && !ways(2049, 8192), "4 rows a code of a granule's window");
        assert!(ways(4096, 1 << 20) && !ways(4097, u64::MAX), "past the L1d at any rows");
        assert!(ways(16, 64) && !ways(16, 63), "few codes, few rows");
        assert!(!ways(u64::MAX, u64::MAX), "a saturated count");
    }

    #[test]
    fn a_front_of_keys_takes_a_part_of_enough_rows_a_count() {
        let rows = 8192;
        assert_eq!(LIGHT_ROWS_A_COUNT, 2, "the fitted 2 rows a count");
        assert!(light_takes(rows / 2, rows), "2 rows a count");
        assert!(!light_takes(rows / 2 + 1, rows), "under 2 rows a count");
        assert!(light_takes(1, 2), "one count of two rows");
        assert!(!light_takes(1, 1), "one count of one row");
        assert!(!light_takes(1, 0), "a window of no rows");
        assert!(!light_takes(LIGHT_MAX_CODES + 1, u64::MAX), "past the largest array, at any rows");
        assert!(!light_takes(u64::MAX, u64::MAX), "a saturated product");
    }

    #[test]
    fn a_counted_part_keeps_the_front_at_the_fits_break_even() {
        assert_eq!(LIGHT_TOUCHED_TENTHS, 24, "the fitted 2.4 rows a touched count");
        assert!(light_keeps(10, 24) && !light_keeps(10, 23), "2.4 rows a touched count");
        assert!(!light_keeps(3542, 8192), "the bind's edge declines");
        assert!(light_keeps(2048, 8192), "4 rows a touched count");
        assert!(light_keeps(0, 0), "a part that touched nothing");
    }
}
