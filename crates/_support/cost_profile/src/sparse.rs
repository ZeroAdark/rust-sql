//! Road code.sparse's table form (hosts granule_source/fetch.rs `sparse_stage`) against the whole decode, a row of a
//! granule: the walk reads the granule's packed codes for its non-base rows and gathers the lanes beside them for the rows
//! the qual's table may pass, the whole decode unpacks every row and tests each. Each costs a share a row plus a share a
//! non-base row; the walk's non-base share is the dearer, so the two cross at one non-base share (`DENSE_PCT`), past
//! which a part decodes whole.
use crate::anchor::{self, Anchor, Ratios};
use crate::fit::Fitted;

const MACHINE: &str = "box gentopn: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU, held whole";
/// The dev data (never a benchmark's tables or statements, the census or a bar cell): pgrcolumnar tables of two 1M-row
/// parts, a text column holding one value on all but s % of the rows (s = 1 .. 95, shuffled) and one of 1,000 others on
/// those, an int lane gathered beside it; count(*) and the int's sum under an EQ, a LIKE prefix and a byte range the
/// per-code table answers; each form forced, W0 and W8, one take each (A/A 0.96 .. 1.05). The fit: least squares of each
/// form's ns a table row over the non-base share at W0, through the cells of 20 % and up (EQ: 40 % and up, below which
/// its bound answers without the walk). Each qual's lines cross at 68 .. 72 % at W0 and 67 .. 71 % at W8.
const CORPUS: &str = "D-04 code.sparse sweep (2 x 1M-row parts, 1 .. 95 % non-base, EQ / LIKE / range, W0 + W8)";

// One anchor (read) for all four, so DENSE_PCT holds on every host; per-kind anchors misprice the walk elsewhere.

/// The walk's cost a granule row: its packed codes read for the non-base ones.
pub const WALK_ROW: Fitted = Fitted { ns: 0.631, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read };
/// The walk's cost a non-base row: its code's test and the gather of the lanes beside it.
pub const WALK_NONBASE: Fitted = Fitted { ns: 2.203, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read };
/// The whole decode's cost a granule row: every row unpacked and tested.
pub const DECODE_ROW: Fitted = Fitted { ns: 1.988, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read };
/// The whole decode's added cost a non-base row (the rows the qual's table may pass: their lanes gathered).
pub const DECODE_NONBASE: Fitted = Fitted { ns: 0.268, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Read };

/// The non-base share in % at which the walk's line `walk_row + walk_nonbase x s` meets the decode's, rounded, within
/// 0 ..= 100 (100: the walk never costs more; 0: never less).
pub const fn crossover_pct(walk_row: f64, walk_nonbase: f64, decode_row: f64, decode_nonbase: f64) -> u64 {
    let (fixed, slope) = (decode_row - walk_row, walk_nonbase - decode_nonbase);
    if fixed.is_nan() || slope.is_nan() || fixed <= 0.0 {
        return 0;
    }
    if slope <= 0.0 || fixed >= slope {
        return 100;
    }
    (fixed * 100.0 / slope + 0.5) as u64
}

/// The non-base share in % past which a part's granules decode whole, on a host of ratios `r`.
pub const fn dense_pct_on(r: &Ratios) -> u64 {
    crossover_pct(WALK_ROW.on(r), WALK_NONBASE.on(r), DECODE_ROW.on(r), DECODE_NONBASE.on(r))
}
/// `dense_pct_on` at the reference (70 on the reference fit).
pub const DENSE_PCT: u64 = dense_pct_on(&Ratios::ONE);
/// `dense_pct_on` on this host (the installed profile): `DENSE_PCT` on every host while the four prices share an anchor.
pub fn dense_pct() -> u64 {
    dense_pct_on(anchor::ratios())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_gives_way_to_the_whole_decode_where_their_lines_cross() {
        assert_eq!(DENSE_PCT, 70, "(1.988 - 0.631) / (2.203 - 0.268)");
        assert_eq!(crossover_pct(1.0, 2.0, 1.0, 0.0), 0, "a walk no cheaper a row never pays");
        assert_eq!(crossover_pct(0.0, 1.0, 2.0, 0.0), 100, "a walk cheaper at any share");
        assert_eq!(crossover_pct(0.0, 1.0, 1.0, 1.0), 100, "a walk no dearer a non-base row");
        assert_eq!(crossover_pct(0.0, 2.0, 1.0, 0.0), 50);
    }
}
