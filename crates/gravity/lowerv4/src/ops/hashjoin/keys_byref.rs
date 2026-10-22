//! Split from join/keys.rs: a join key that is an expression
//! with a by-reference result (`('_' || typname)::name`, `coalesce(t1.stringu1, t2.stringu1)`, `ltrim(c, 'A')`, a
//! foreign key check's cast). The expression's datum (the region pointer step_expr's emit gives, live for the row) is the
//! key's cell as a column of its type would be: its Rep by storage (a varlena, a fixed-length image), so the key image is
//! the one a column key of the type takes (face/keyimg.rs; an inexact image refuses there by name, as a column's does).
//! Only the image words reach the build's records: the datum is read before the row ends. Owner EXPR-ODDS. Budget 40.
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_program::{NumTypmod, Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife, Nullable};

/// The row's cell of an expression key of type `tf`: a by-value result a Word of its width, a by-reference one its
/// datum by storage; both live for the row, in the row's context.
pub(crate) fn expr_cell(tf: &TypeFacts) -> Cell {
    let rep = match (tf.byval, tf.typlen) {
        (true, l @ (1 | 2 | 4 | 8)) => Rep::Word { bytes: l as u8 },
        (true, _) => Rep::Word { bytes: 8 },
        (false, _) => Rep::of_storage(tf.typlen, false),
    };
    let life = CellLife { until: Until::Row, home: CellHome::RowCx };
    Cell { rep, typid: tf.typid, num: NumTypmod::NONE, life, at: At::Value(0), null: Nullable::Maybe }
}
