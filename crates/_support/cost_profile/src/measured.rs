use crate::anchor::{self, Anchor, Ratios};

pub const CLASS: &str = "c8g-neoverse-v2";
const TABLE: &str = include_str!("../measured/c8g-neoverse-v2.tsv");

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub primitive: &'static str,
    pub width: u32,
    pub ns: f64,
    pub fixed_ns: f64,
}

pub fn rows() -> &'static [Row] {
    static ROWS: pgsync::OnceLock<Vec<Row>> = pgsync::OnceLock::new(); // alloc: Cold (once a process, the table's ~140 rows)
    ROWS.get_or_init(|| TABLE.lines().skip(1).filter_map(parse).collect())
}

fn parse(line: &'static str) -> Option<Row> {
    let mut f = line.split('\t');
    let primitive = f.next()?;
    let width = f.next()?.parse().ok()?;
    let _unit = f.next()?;
    let ns: f64 = f.next()?.parse().ok()?;
    let fixed_ns: f64 = f.next()?.parse().ok()?;
    (ns.is_finite() && fixed_ns.is_finite()).then_some(Row { primitive, width, ns, fixed_ns })
}

/// The anchor a measured primitive moves with on a host other than the reference, by the mechanism its family times: a
/// plain chain's fold (`plain.*`, `walk.*.plain`) and a join's payload rows a hashed update; a walk's other rows (`walk.*`)
/// gathers at the walked positions; a top's (`top.*`) a sort; R5's form rows (`expr.*`, `adapt.*`, `fmgr.*`, `percode.*`,
/// `values.*`) a row's evaluation (the check anchor); the primitives (`prim.*`) by their kind. Every reader but the walk's
/// (walk.rs) compares rows of one family, which keep their ratio on every host; the walk's compares its rows with the
/// plain fold's.
pub fn anchor_of(primitive: &str) -> Anchor {
    let mut seg = primitive.split('.');
    let (family, kind) = (seg.next().unwrap_or(""), seg.next().unwrap_or(""));
    match family {
        "plain" | "join" => Anchor::Update,
        "walk" if primitive.ends_with(".plain") => Anchor::Update,
        "walk" => Anchor::Gather,
        "top" => Anchor::Sort,
        "prim" => match kind {
            "hash" | "group" | "distinct" | "pipe" => Anchor::Update,
            "sort" | "topn" => Anchor::Sort,
            "probe" => Anchor::Gather,
            _ => Anchor::Read,
        },
        _ => Anchor::Check,
    }
}

impl Row {
    /// This row on a host of ratios `r`: both prices times its primitive's anchor's ratio.
    pub fn on(self, r: &Ratios) -> Row {
        let k = r.of(anchor_of(self.primitive));
        Row { ns: self.ns * k, fixed_ns: self.fixed_ns * k, ..self }
    }
}

/// The row of `primitive` at the widest measured width at or under `width`, on a host of ratios `r`.
pub fn at_on(r: &Ratios, primitive: &str, width: u32) -> Option<Row> {
    rows().iter().copied().filter(|row| row.primitive == primitive && row.width <= width).max_by_key(|row| row.width).map(|row| row.on(r))
}

/// `at_on` on this host (the installed profile).
pub fn at(primitive: &str, width: u32) -> Option<Row> {
    at_on(anchor::ratios(), primitive, width)
}

pub fn ns(primitive: &str, width: u32) -> Option<f64> {
    at(primitive, width).map(|r| r.ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_of_the_table_parses_and_the_walk_primitives_are_there() {
        assert_eq!(rows().len(), TABLE.lines().skip(1).filter(|l| !l.is_empty()).count(), "a malformed row");
        for p in ["walk.group.plain", "walk.group.walk", "walk.group.walk_per_table_granule", "walk.keyed.plain", "walk.keyed.walk", "walk.keyed.walk_per_table_granule"] {
            for w in [0, 2, 8] {
                assert_eq!(at(p, w).map(|r| r.width), Some(w), "{p} at W{w}");
            }
        }
        assert_eq!(at("walk.keyed.walk", 4).map(|r| r.width), Some(2), "a width between two reads the lower");
        assert!(ns("fmgr.hosted.date_in", 0).is_some_and(|v| v > 0.0));
        assert_eq!(ns("no.such.primitive", 0), None);
    }
}
