//! A length read over a dictionary column in a row step (a filter, a gate or the projection's `octet_length` /
//! `char_length` / `length`; lowerv4 ops/scan/pgrc/strategies/octet_lane.rs), as the cost home's break-even of its two
//! forms. The per-code form evaluates the read once a dictionary entry and looks each row's code up in that table: it
//! pays a cost an entry (`TABLE_ENTRY_*`) and the lookup a row. The octet lane fills each row's length from the part's
//! dictionary index through its code, no table: it pays a cost a row over the lookup (`LANE_ROW_*`) and no entry. Each
//! cost is an Amdahl pair, the share the statement's participants divide (`_SPLIT`, over the width) and the share none
//! divides (`_FLAT`).
use cost_profile::anchor::{self, Anchor, Ratios};
use cost_profile::fit::{break_even, Fitted};

const MACHINE: &str = "box gencoll: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev data (never a benchmark's statements, the census or a bar cell): a synthetic sweep, one pgrcolumnar table a
/// cell, 4 M rows in ten INSERTs of 400 k (ten parts), `s = 'u/' || md5(i % n)` (34 B ASCII, every part holding all n
/// values, all dictionary-coded), n of 1 k / 4 k / 16 k / 40 k / 100 k (dictionary entries 0.25 / 1 / 4 / 10 / 25 % of
/// the rows); `count(*) WHERE length(s) > 0` and `= 34`; the per-code form and the lane (the same tree with the lane
/// taken on dictionary columns or not), one postmaster each, W0 and W8, the middle of three hot runs. The fit: per form
/// and width, the least-squares line of ms over the entries; the slopes' difference is the cost an entry, the
/// intercepts' over the rows the cost a row; the W0 and W8 lines give each term's split and flat shares. At 50 % entries
/// (2 M) the table runs past the caches (954 ms against the lane's 28 at W0) and stays out of the fit: it is on the
/// lane's side of the break-even either way.
const CORPUS: &str = "D-101c length sweep (4 M rows, 0.25 .. 25 % entries, length(s) > 0 / = 34, W0 + W8, middle of 3)";

/// The lane reads each row's length through its code at random (the gather anchor); the per-code form evaluates and stores
/// an entry's length in its table (the update anchor).
///
/// The lane's cost a row over the per-code form's lookup, divided among the participants (W0 2.751, W8 0.350 ns a row).
pub const LANE_ROW_SPLIT: Fitted = Fitted { ns: 2.744, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Gather };
/// The lane's cost a row over the lookup that the participants do not divide.
pub const LANE_ROW_FLAT: Fitted = Fitted { ns: 0.007, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Gather };
/// The per-code form's cost an entry over the lane's (W0 86.121, W8 11.167 ns an entry), divided among the participants.
pub const TABLE_ENTRY_SPLIT: Fitted = Fitted { ns: 85.663, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// The per-code form's cost an entry over the lane's that the participants do not divide.
pub const TABLE_ENTRY_FLAT: Fitted = Fitted { ns: 0.459, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };

fn at(r: &Ratios, split: Fitted, flat: Fitted, width: u16) -> f64 {
    split.on(r) / width.max(1) as f64 + flat.on(r)
}

/// The fewest entries a row at which the lane costs no more than the per-code form at `width`: `break_even` of the
/// table's cost an entry against the lane's cost a row, on a host of ratios `r` (0.032 at W0 and at W8 on the reference).
pub fn lane_entries_per_row_on(r: &Ratios, width: u16) -> f64 {
    break_even(at(r, TABLE_ENTRY_SPLIT, TABLE_ENTRY_FLAT, width), at(r, LANE_ROW_SPLIT, LANE_ROW_FLAT, width))
}

/// `lane_entries_per_row_on` on this host (the installed profile).
pub fn lane_entries_per_row(width: u16) -> f64 {
    lane_entries_per_row_on(anchor::ratios(), width)
}

/// Whether a row step's length read over a dictionary column of `entries` entries in a relation of `rows` rows takes the
/// octet lane in a program served from the leader alone up to `widest` (`Facts::width`): where the lane costs no more
/// at both ends. Else it keeps the per-code form, the rule of before.
pub fn lane_pays(entries: u64, rows: u64, widest: u16) -> bool {
    let need = lane_entries_per_row(0).max(lane_entries_per_row(widest));
    rows > 0 && entries as f64 >= rows as f64 * need
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep's cells fall on the measured side: 1 % entries keeps the table (7.5 against 14.8 ms at W0), 4 % takes
    /// the lane (17.4 against 18.3), and the break-even sits between them at both ends.
    #[test]
    fn the_break_even_sits_between_the_sweeps_cells() {
        for w in [0, 8] {
            let be = lane_entries_per_row(w);
            assert!(be > 0.01 && be < 0.04, "W{w}: {be}");
        }
        assert!(!lane_pays(40_000, 4_000_000, 8));
        assert!(lane_pays(160_000, 4_000_000, 8));
        assert!(!lane_pays(0, 0, 8));
    }
}
