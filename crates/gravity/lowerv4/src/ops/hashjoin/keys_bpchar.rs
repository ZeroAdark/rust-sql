//! D1353 at a hash join's key image (keys.rs `image`): a key typed bpchar (its clause's type: `f.b = d.v::bpchar`, a
//! relabel of a varchar / text value, decompose join.rs `key_decls`) images hashbpchar's trimmed bytes (rep.varlena.hash's
//! bcTruelen) on a heap value or a computed one. A dictionary code lane of another type keys its Fp128 table, which is the
//! untrimmed image of the render (pgrc declares hashbpchar's trimmed table for a bpchar column only, dict/coderep.rs), so
//! 'a' and 'a ' would key apart: such a key refuses by D1353's name. Owner M1-JOIN. Budget 40 lines.
use gravity_decompose::catalog::typefacts::{row_of, TypeFacts};
use gravity_program::Rep;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::dict::coderep::{sk_bpchar_of, BPCHAR_KEY_OVER_OTHER_TYPE};
use crate::ops::hashjoin::keys::KeyVals;
use crate::rep::Lx;

/// Refuses plan node `node` when a key cell is a code lane whose type does not trim while the key's type does, naming
/// the relation of the cell's site (`Lx::site_of`; PGRCSERVE M0 D2: its rows road serves the key).
pub(crate) fn code_untrimmed(lx: &Lx<'_, '_, '_>, node: i32, cells: &KeyVals<'_>) {
    let trims = |t: Option<&TypeFacts>| sk_bpchar_of(t) != 0;
    if let Some((c, ..)) = cells.iter().find(|(c, _, _, tf)| matches!(c.rep, Rep::Code { .. }) && trims(Some(tf)) && !trims(row_of(c.typid))) {
        match crate::frame::env::site_relation(lx.site_of(c)) {
            0 => crate::refuse!(node, Reason::Shape { why: ShapeWhy(BPCHAR_KEY_OVER_OTHER_TYPE) }),
            relid => crate::admit::refusal::refuse_native(node, relid, ShapeWhy(BPCHAR_KEY_OVER_OTHER_TYPE)),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_bpchar_trims() {
        let trims = |t: u32| super::sk_bpchar_of(super::row_of(t)) != 0;
        assert!(trims(1042), "bpchar trims");
        assert!(!trims(1043) && !trims(25), "varchar and text keep their blanks");
    }
}
