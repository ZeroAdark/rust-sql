//! KEYED-31: the key.codeif CASE group key's two roads (lowerv4 dict/strategies/percode_guard.rs `then_var`), as the cost
//! home's break-even. The per-entry image keys a row by its code's Fp128 (a gather) where the row's text key renders the
//! CASE's text and hashes its bytes, so the image saves a cost a row (`ROW_SAVED_*`, an Amdahl pair: the share the
//! statement's participants divide, `_SPLIT` over the width, and the share none divides, `_FLAT`: the memory the rows'
//! decode streams through). Its groups are about the column's entries, each with the code as its representative, and the
//! emit renders each through its part's code in the leader, where the text key keeps the datum it hashed, so the image
//! pays a cost an entry (`ENTRY_PAID`) that no participant divides.
use cost_profile::anchor::{self, Anchor, Ratios};
use cost_profile::fit::{break_even, Fitted};

const MACHINE: &str = "box 45: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev data (never a benchmark's statements, the census or a bar cell): a synthetic sweep table, 10 M rows in ten
/// COPYs of 1 M, URL-like text columns (~50 B) of 1 / 5 / 10 / 20 / 30 / 40 / 50 / 60 / 80 / 100 % distinct values
/// (`(id * 7919) % (rows x pct)`; stored verbatim, so `Facts::att_entries` is their NDV), the key
/// `CASE WHEN id % 1000 = k THEN 'x' ELSE col END` grouped, top 10; the image road and the text key (the same tree with
/// and without the form), interleaved, one postmaster each, W0 and W8, medians of 6 hot runs. The fit: per width, the
/// least-squares line of the image's ms less the text key's over the column's entries, through the cells of 1 M entries
/// and up (10 .. 100 %: the groups past the caches on both roads; 1 / 5 % fold in cache, a lower slope where no decision
/// is near); its slope is the cost an entry at that width, its intercept over the rows the cost a row; the W0 and W8
/// lines give each term's split and flat shares.
const CORPUS: &str = "KEYED-31 sweep (10 M rows, 10 .. 100 % entries, mirror CASE key, W0 + W8, medians of 6)";

/// The row saving is the text key's hash of the row's bytes (the update anchor); an entry's price is its render through its
/// part's dictionary, reads at the entries' codes (the gather anchor).
///
/// The text key's cost a row over the image's, divided among the participants (W0 113.3, W8 83.0 ns a row).
pub const ROW_SAVED_SPLIT: Fitted = Fitted { ns: 34.63, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// The text key's cost a row over the image's that the participants do not divide.
pub const ROW_SAVED_FLAT: Fitted = Fitted { ns: 78.68, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// The image's cost an entry over the text key's: the emit's render of each group through its part, in the leader. The
/// two widths' lines agree (W0 382.6, W8 383.3 ns an entry; the split share fits at -0.8, none), so no share is divided.
pub const ENTRY_PAID: Fitted = Fitted { ns: 382.95, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Gather };

/// The text key's cost a row over the image's at `width` (0 = the leader alone, one participant), on a host of ratios `r`.
pub fn codeif_row_saved_on(r: &Ratios, width: u16) -> f64 {
    ROW_SAVED_SPLIT.on(r) / width.max(1) as f64 + ROW_SAVED_FLAT.on(r)
}

/// The most entries a row the image may have and still cost no more than the text key at `width`: `break_even` of the cost
/// an entry against the cost a row at that many participants, on a host of ratios `r`.
pub fn codeif_entries_per_row_on(r: &Ratios, width: u16) -> f64 {
    break_even(ENTRY_PAID.on(r), codeif_row_saved_on(r, width))
}

/// The most entries a row the image may have for a program that serves sessions from the leader alone up to `widest`:
/// `break_even` of its summed cost at both ends, an entry paid at each, against the cost a row saved at each, on a host of
/// ratios `r`.
pub fn codeif_entries_per_row_served_on(r: &Ratios, widest: u16) -> f64 {
    break_even(2.0 * ENTRY_PAID.on(r), codeif_row_saved_on(r, 0) + codeif_row_saved_on(r, widest))
}

/// `codeif_row_saved_on` on this host (the installed profile).
pub fn codeif_row_saved(width: u16) -> f64 {
    codeif_row_saved_on(anchor::ratios(), width)
}

/// `codeif_entries_per_row_on` on this host.
pub fn codeif_entries_per_row(width: u16) -> f64 {
    codeif_entries_per_row_on(anchor::ratios(), width)
}

/// `codeif_entries_per_row_served_on` on this host.
pub fn codeif_entries_per_row_served(widest: u16) -> f64 {
    codeif_entries_per_row_served_on(anchor::ratios(), widest)
}

/// Whether a key.codeif group key over a column of `entries` entries in a relation of `rows` rows keys by its per-entry
/// image in a program served from the leader alone up to `widest` (`codeif_entries_per_row_served`), else by the row's
/// text. The lowering passes the server's widest width (`Facts::width`): the program serves every session, whatever width
/// each runs at, so it is judged at both ends, never at one session's width.
pub fn codeif_image_pays(entries: u64, rows: u64, widest: u16) -> bool {
    entries as f64 <= rows as f64 * codeif_entries_per_row_served(widest)
}

/// The band of entries a row (`Facts::att_entries` over `rel_rows`, ANALYZE's estimate) where neither road wins at both
/// ends of the widths a program serves. It holds the served break-even on every host whose ratios move it less than its
/// edges (cost/host_census.rs), so there the band decides. Measured, not fitted: a quiet pass's crossovers (box 41, c8g.4xlarge
/// held whole, an A/A server beside; the sweep's columns, W0 + W8, medians of 4 hot runs), each interpolated between its two cells.
/// The low edge is W8's, the mirror key: image / text 378.7 / 458.7 ms at 0.0511 entries a row, 679.2 / 610.9 at 0.1016.
pub const CODEIF_BAND_LOW: f64 = 0.078;
/// The high edge is W0's, the column-THEN key: image / text 2503.2 / 2758.8 ms at 0.3244, 3135.6 / 2999.8 at 0.4501.
pub const CODEIF_BAND_HIGH: f64 = 0.407;

/// The most entries a row the image may have where the key's groups are never rendered (`rendered` false: a DISTINCT
/// argument only pair counts read, count(DISTINCT)'s): no cost an entry is paid, so `break_even` of none against the cost
/// a row saved at either end is every ratio.
pub fn codeif_entries_per_row_unrendered(widest: u16) -> f64 {
    break_even(0.0, codeif_row_saved(0).min(codeif_row_saved(widest)))
}

/// Whether a key.codeif group key keys by its image. A key whose groups are never rendered (`rendered` false) pays no
/// cost an entry (`codeif_entries_per_row_unrendered`). Otherwise, outside the band as `codeif_image_pays`, inside it by
/// its form, the image for the column-THEN form (`column_then`: every WHEN's result the column, the ELSE the constant)
/// and the text key for the others, the roads each form takes without a cost rule, so no width runs the band slower than
/// those. A guard on the session's width is what wins the band (DESIGN-OPEN NEW-dict-g3-2).
pub fn codeif_keys_by_image(entries: u64, rows: u64, widest: u16, column_then: bool, rendered: bool) -> bool {
    if !rendered {
        return true; // codeif_entries_per_row_unrendered: every ratio
    }
    let per_row = entries as f64 / rows.max(1) as f64;
    if (CODEIF_BAND_LOW..=CODEIF_BAND_HIGH).contains(&per_row) {
        return column_then;
    }
    codeif_image_pays(entries, rows, widest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fit's break-even sits inside each width's measured crossover: the sweep crossed at 32.0 % entries at W0 (between
    /// its 30 and 40 % cells) and at 22.0 % at W8 (between 20 and 30 %), interpolated.
    #[test]
    fn the_reference_fit_flips_inside_each_widths_crossover() {
        let at = |w: u16| (codeif_entries_per_row(w) * 1000.0).round() as u32;
        assert_eq!((at(0), at(8), at(16)), (296, 217, 211));
        assert!((1..=16).all(|w| codeif_entries_per_row(w) <= codeif_entries_per_row(w - 1)), "more participants divide the saving");
        assert_eq!(codeif_entries_per_row(0), codeif_entries_per_row(1));
    }

    /// A program served from the leader alone up to 16 participants (the sweep's servers) is judged at both ends: its
    /// break-even lies between the two widths' own, so neither end gives up more than its band to the other. The sweep's
    /// columns as ANALYZE estimates them (20 % at 0.2126, 30 % at 0.3244) fall on each side.
    #[test]
    fn a_served_program_is_judged_at_both_ends() {
        let served = (codeif_entries_per_row_served(16) * 1000.0).round() as u32;
        assert_eq!(served, 253);
        assert!(codeif_entries_per_row(16) < codeif_entries_per_row_served(16) && codeif_entries_per_row_served(16) < codeif_entries_per_row(0));
        assert_eq!(codeif_entries_per_row_served(0), codeif_entries_per_row(0), "a server of the leader alone is its one width");
        let rows = 10_000_000;
        for (n, pays) in [(1_000_000, true), (2_126_472, true), (3_244_062, false), (4_500_939, false), (rows, false)] {
            assert_eq!(codeif_image_pays(n, rows, 16), pays, "{n} entries");
        }
    }

    /// The band holds every width's break-even and every served one, so outside it each width agrees (the image below,
    /// the text key above) and inside it the form decides. The quiet pass's columns as ANALYZE estimates them: 5 % at
    /// 0.0511, 10 % at 0.1016, 30 % at 0.3244, 40 % at 0.4501.
    #[test]
    fn inside_the_band_the_form_decides() {
        for w in 0..=16 {
            for b in [codeif_entries_per_row(w), codeif_entries_per_row_served(w)] {
                assert!(CODEIF_BAND_LOW < b && b < CODEIF_BAND_HIGH, "width {w}: {b}");
            }
        }
        let rows = 10_000_000;
        for (n, column_then, image) in [(511_000, false, true), (1_016_000, false, false), (1_016_000, true, true), (3_244_062, true, true), (3_244_062, false, false), (4_500_939, true, false), (rows, true, false)] {
            assert_eq!(codeif_keys_by_image(n, rows, 16, column_then, true), image, "{n} entries, column_then {column_then}");
        }
        assert!(codeif_keys_by_image(0, 0, 0, false, true), "no rows");
    }

    /// A key whose groups are never rendered pays no cost an entry: the image keys at every ratio and width, in the band
    /// and past it, whatever its form.
    #[test]
    fn an_unrendered_key_keys_by_its_image_at_every_ratio() {
        assert!((0..=16).all(|w| codeif_entries_per_row_unrendered(w) == 1.0));
        let rows = 10_000_000;
        for n in [99_032, 1_016_000, 2_126_472, 3_244_062, 4_500_939, rows] {
            assert!([0, 8, 16].iter().all(|&w| codeif_keys_by_image(n, rows, w, false, false)), "{n} entries");
        }
    }
}
