//! A type's storage as lowering reads it, in the type-semantics home: pg_type's `typlen`, `typbyval` and `typalign`
//! (`get_typlenbyvalalign`), and whether a value of the type is a whole row (`type_is_rowtype`). Read through the plan's
//! catalog face, so a test catalog answers as the server's does; a family asks these, never the catalog's type row.
use gravity_expr::Catalog;
use types_core::catalog::RECORDOID;
use types_error::PgResult;

/// A type's storage: `len` is `typlen` (bytes; -1 a varlena, -2 a C string), `byval` is `typbyval`, `align` is
/// `typalign` ('c' / 's' / 'i' / 'd').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Storage {
    pub(crate) len: i16,
    pub(crate) byval: bool,
    pub(crate) align: i8,
}

/// `get_typlenbyvalalign(t)`; the catalog's error for a type it does not know.
pub(crate) fn of<'m>(catalog: &dyn Catalog<'m>, t: u32) -> PgResult<Storage> {
    catalog.type_facts(t).map(|f| Storage { len: f.len, byval: f.byval, align: f.align })
}

/// A value of `t` is a whole row: RECORD, or a composite type once its domains are stripped (PostgreSQL's
/// `type_is_rowtype`: `getBaseType`, then `typtype` 'c'). A type the catalog does not know is none.
pub(crate) fn whole_row<'m>(catalog: &dyn Catalog<'m>, t: u32) -> bool {
    let base = catalog.base_type(t, &mut -1).unwrap_or(t);
    base == RECORDOID || catalog.type_facts(base).is_ok_and(|f| f.typtype == b'c' as i8)
}
