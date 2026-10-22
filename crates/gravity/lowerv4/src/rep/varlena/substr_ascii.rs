//! Road `text.substr.ascii` (D4822, default off: road_off.rs `DEFAULT_OFF`, on under `+text.substr.ascii`): a
//! hosted substring (text_substr, text_substr_no_len: `substr`, `substring(.. FROM .. [FOR ..])`) takes its result by
//! bytes when every byte the result reaches through is ASCII (0x01..=0x7F), one character per byte in every server
//! encoding, and PostgreSQL's text_substring walk otherwise. The choice is the bind's: while a lowering runs with the road
//! on (frame/program.rs `lower`) gravity_expr `SubstrAscii` binds each such site to the shim's twin (gravity_fn
//! shims/text.rs `ascii_twin`, `ascii_slice`), whose cold path is the plain shim's. Answers and errors are the plain
//! shim's: a negative count, a compressed or external image and any byte outside 0x01..=0x7F before the result's end take
//! the walk. Why (D4822): a substring of a dimension's name per joined row (a group key) spent ~30 % of a statement's
//! cycles in the walk, a mbutils seam call per character (~140 ns a call). A text = / IN over a
//! substring is road text.eq's view (text_view.rs), not this.
use crate::ops::Table;

crate::strategies! {
    /// D4822: a hosted substring's result by bytes over ASCII text (gravity_expr `SubstrAscii`). Default off.
    TEXT_SUBSTR_ASCII = 80, "text.substr.ascii", All;
}

pub(crate) const ROWS: Table = Table { strategies: &[(TEXT_SUBSTR_ASCII, &())], ..crate::ops::NONE };

/// The road for this lowering: on only under its `+<id>` (road_off.rs).
pub(crate) fn on() -> bool {
    crate::face::source::on(crate::admit::refusal::Variant::Strategy(TEXT_SUBSTR_ASCII))
}
