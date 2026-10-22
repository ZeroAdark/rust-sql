//! Render survivors only. Hooks / levers: H10 R4-L06 R11-L11. Budget 160 lines (files.tsv, +25 % gate). Owner
//! M1-TABLES.
//!
//! A code key or a MinCode representative rides the record as `part << 32 | code` (coderep.rs) and is rendered only for
//! the records the emit outputs: once per emitted candidate, through the site's dictionary, never per row.
//!
//! The render point (KEYED, keyed/emit_group.rs): `KeyOut::Code` never renders. It hands the output row a `Rep::Code` cell
//! whose word is the record's, and `agg_render` runs only where a consumer reads that cell's datum through the rep face.
//! The emit streams only kept records: the twopass.having floor's keep runs before any key is taken, a HAVING filter reads
//! the key only when it names it, and an ORDER BY ... LIMIT over the groups keeps records, not rendered rows
//! (runs/topk_groups.rs), so a group past the cut is never rendered. A top ordered by the text key renders its kept
//! records' keys to compare them (the sort's read, not the output's).
//!
//! Registers: road `code.render` (`CODE_RENDER`), a marker: the rep face's Code datum read is the render.

crate::strategies! {
    /// Render survivors only.
    CODE_RENDER = 45, "code.render", CodeKeyed;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CODE_RENDER, &())], ..crate::ops::NONE };
