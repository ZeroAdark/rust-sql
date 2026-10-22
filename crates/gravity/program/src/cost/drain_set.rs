//! NEW-51: when a plain count(DISTINCT) over a draining hash join (RIGHT, FULL) absorbs PostgreSQL's presort into its
//! hashed set (decompose ops/group/absorb.rs `distinct_sort`, NEW-41 (a)), as the cost home's break-even of two lines.
//! The Sort's form sorts the joined rows and folds them in order; the absorbed form inserts them into the DISTINCT set
//! (each participant its own, at kernels `fold_bits`' floor of partitions) and the leader merges and finishes the sets.
//! Per joined row the set is the cheaper form; per statement it may pay more (the sets' opening, merge and finish). So
//! each form is a line over PostgreSQL's estimate of the joined rows (the presort's rows: the estimate the decision
//! reads), and the Sort stays below the rows where the lines cross (`set_rows`): the set's extra fixed cost over the
//! Sort's saving a row, none when the set's line lies under the Sort's everywhere. Nothing else moves an answer: both
//! forms give the same rows.
//!
//! The lines are FITTED (`fit::least_squares`, every cell of the sweep at its width) per DISTINCT key representation (a
//! key of one by-value word, which the set holds inline; a varlena or multi-word key, which it hashes and compares through
//! its bytes) and per participants (W0 the leader alone; W8). The set's fixed cost grows with the participants (each opens
//! its own set and the leader merges each), the Sort's saving a row with the width too, so a server's width picks its
//! pair (`Facts::width`, as KEYED-31's): 0 .. 7 take W0's, the measured lower width (no width moves a statement on a
//! reading no fit made; W0's pair keeps the Sort longer for a by-value key, the form trunk ran before NEW-41), 8 and up
//! W8's.
//!
//! The fixed term does not follow the build side's estimated rows. With the probe side held at 299 rows (corpus n41b,
//! quiet perf box 51, 4 rounds, A/A 0.97 .. 1.04), a full join whose hashed side grows from 7 to 1,000 rows pays the
//! set's extra +0.39 .. +0.28 ms at W8 (b41-r7 .. b41-r1000; +0.20 at 2,000, its joined rows growing), and a 7-row hashed
//! side on another key +0.30 ms (b41-fd); at W0 every cell pays +0.04 .. +0.09 ms. The int-key sweep agrees (0.45 ..
//! 0.52 ms at W8 while its hashed side grows 207 .. 2,746 rows). So the expression has one fixed term per key
//! representation and width. The right joins of the sweep's g41-rk kind (a 7-row side preserved, 479 .. 4,859 joined
//! rows) pay none at W8 (0.90 .. 0.96 of the Sort's form) and keep the Sort a little longer than they need.
//!
//! The sweep (dev data only, never a benchmark's statements, the census or a bar cell): corpus n41g, fi (200,000 rows,
//! an int key `ord` and a text key `t` of 50,000 values under the C collation) FULL JOIN fr (100,000 rows) on
//! `fi.id < N` / `fr.amt < 2N/3`, N = 300 .. 100,000 (int key) and 1,000 .. 50,000 (text key), `count(DISTINCT)` of the
//! key, 4 rounds of v5probe --perf 15 --jobs 1 with the arm order rotated and an A/A arm, the median of the rounds
//! against PostgreSQL's presort estimate (EXPLAIN after the corpus's ANALYZE; a re-ANALYZE moves it by a few %). The
//! by-value key's lines: trunk 687f554548c (the Sort) against NEW-41 (a) without its gate (the set), quiet perf box 63
//! held whole (A/A W0 0.946 .. 1.021, W8 0.936 .. 1.004). The text key's lines: NEW-53's keyed end (a resident
//! multi-word keyed end runs its probe and drain arms at the statement's width, not on the leader alone), each form
//! forced, quiet perf box 54 held whole (A/A W0 0.967 .. 1.031, W8 0.954 .. 1.022). The points and the fit's check are
//! crates/gravity/program/tests/cost_fit.rs `drain_set_lines`. At W8 the by-value lines cross inside the cells where
//! the sweep flipped (3,886 rows 1.04x -> 6,987 rows 0.90x); the text set is the cheaper form at every cell (0.71x at
//! 932 rows .. 0.27x at 49 k; corpus n51's cells from 1,499 rows 0.65 .. 0.41x) and its fixed cost is the lower too, so
//! its lines do not cross (`crossover` 0). At W0 the Sort's form bends up past ~50 k rows (int 5.01 -> 18.07 ms from 49
//! k to 100 k rows; text 8.0 -> 23.1 ms from 20 k to 49 k), so the lines through every cell cross early for a by-value
//! key (12.8 k, where the cells cross between 49 k and 100 k) and late for text (3.8 k, where the set led at every
//! cell: 0.98x at 932 rows, 0.93x at n51's 1,499).
use cost_profile::anchor::{self, Anchor, Ratios};
use cost_profile::fit::Fitted;

const MACHINE: &str = "box 63: c8g, Graviton 4 (Neoverse V2), 16 vCPU, held whole (PERF-BOX-QUEUE)";
const CORPUS: &str = "NEW-41 size-gate sweep n41g (fi 200 k FULL JOIN fr 100 k, int + text DISTINCT keys, 300 .. 100 k joined rows; 4 rounds, medians)";
const WIDE_MACHINE: &str = "box 54: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU, held whole (PERF-BOX-QUEUE)";
const WIDE_CORPUS: &str = "n41g text-key cells (fi 200 k FULL JOIN fr 100 k, 932 .. 49 k joined rows) on NEW-53's keyed end (drain arms at the statement's width), each form forced; 4 rounds, medians";

/// A form's cost over the joined rows: `fixed` ns a statement plus `row` ns a joined row (PostgreSQL's estimate).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub fixed: Fitted,
    pub row: Fitted,
}

/// The two forms at one key representation and width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pair {
    pub sort: Line,
    pub set: Line,
}

/// The Sort form's line moves with the sort anchor, the set's with the update anchor (a hashed probe and insert a row).
const fn line(fixed_ms: f64, row_ns: f64, anchor: Anchor) -> Line {
    line_on(fixed_ms, row_ns, MACHINE, CORPUS, anchor)
}

const fn wide_line(fixed_ms: f64, row_ns: f64, anchor: Anchor) -> Line {
    line_on(fixed_ms, row_ns, WIDE_MACHINE, WIDE_CORPUS, anchor)
}

const fn line_on(fixed_ms: f64, row_ns: f64, machine: &'static str, corpus: &'static str, anchor: Anchor) -> Line {
    Line { fixed: Fitted { ns: fixed_ms * 1e6, machine, corpus, anchor }, row: Fitted { ns: row_ns, machine, corpus, anchor } }
}

/// A key of one by-value word (int) at W0: 11 cells, 275 .. 99,773 estimated rows; the lines cross at 12,834 rows.
pub const WORD_W0: Pair = Pair { sort: line(1.3027, 145.39, Anchor::Sort), set: line(2.4616, 55.09, Anchor::Update) };
/// A key of one by-value word (int) at W8; the lines cross at 5,312 rows.
pub const WORD_W8: Pair = Pair { sort: line(1.0200, 145.03, Anchor::Sort), set: line(1.5313, 48.77, Anchor::Update) };
/// A varlena key (text) at W0: 5 cells, 932 .. 49,360 estimated rows; the lines cross at 3,843 rows.
pub const WIDE_W0: Pair = Pair { sort: wide_line(1.4804, 423.14, Anchor::Sort), set: wide_line(2.7895, 82.53, Anchor::Update) };
/// A varlena key (text) at W8: the set's line lies under the Sort's at every size (`crossover` 0).
pub const WIDE_W8: Pair = Pair { sort: wide_line(1.5218, 228.43, Anchor::Sort), set: wide_line(0.9893, 49.10, Anchor::Update) };

/// The width a statement is costed at off a backend (no `Facts::width`): PostgreSQL's default `max_parallel_workers`.
pub const DEFAULT_WIDTH: u16 = 8;
/// The least width whose pair is W8's.
const PARALLEL_FIT_WIDTH: u16 = 8;

/// The joined rows where the two lines cross on a host of ratios `r`: the set's extra fixed cost over the Sort's extra
/// cost a row. `INFINITY` when the set saves nothing a row (the Sort always stays), 0 when the set costs no more per
/// statement either.
pub const fn crossover_on(p: Pair, r: &Ratios) -> f64 {
    let extra = p.set.fixed.on(r) - p.sort.fixed.on(r);
    let saved = p.sort.row.on(r) - p.set.row.on(r);
    if saved.is_nan() || extra.is_nan() || saved <= 0.0 {
        return f64::INFINITY;
    }
    if extra <= 0.0 {
        return 0.0;
    }
    extra / saved
}

/// The pair a DISTINCT key representation (`word`: every DISTINCT argument one by-value word) takes at a server width.
pub const fn pair(word: bool, width: u16) -> Pair {
    match (word, width >= PARALLEL_FIT_WIDTH) {
        (true, false) => WORD_W0,
        (true, true) => WORD_W8,
        (false, false) => WIDE_W0,
        (false, true) => WIDE_W8,
    }
}

/// `crossover_on` at the reference: the fit's own lines.
pub const fn crossover(p: Pair) -> f64 {
    crossover_on(p, &Ratios::ONE)
}

/// PostgreSQL's estimate of the joined rows under which the Sort's form stays on a host of ratios `r`.
pub const fn set_rows_on(r: &Ratios, word: bool, width: u16) -> f64 {
    crossover_on(pair(word, width), r)
}

/// `set_rows_on` on this host (the installed profile): decompose absorb.rs `drain_set_rows`.
pub fn set_rows(word: bool, width: u16) -> f64 {
    set_rows_on(anchor::ratios(), word, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_cross_where_the_sweep_flipped() {
        let r = |w: bool, width: u16| set_rows(w, width).round() as u64;
        assert_eq!((r(true, 8), r(false, 8), r(true, 0), r(false, 0)), (5_312, 0, 12_834, 3_843));
    }

    #[test]
    fn a_set_that_saves_nothing_a_row_never_pays() {
        let flat = Pair { sort: line(1.0, 50.0, Anchor::Sort), set: line(2.0, 50.0, Anchor::Update) };
        let cheap = Pair { sort: line(2.0, 50.0, Anchor::Sort), set: line(1.0, 40.0, Anchor::Update) };
        assert_eq!((crossover(flat), crossover(cheap)), (f64::INFINITY, 0.0));
        assert_eq!(crossover(Pair { sort: line(1.0, 100.0, Anchor::Sort), set: line(2.0, 50.0, Anchor::Update) }), 20_000.0);
        assert_eq!((pair(true, 7), pair(true, 8), pair(true, 64), pair(false, 0)), (WORD_W0, WORD_W8, WORD_W8, WIDE_W0));
    }
}
