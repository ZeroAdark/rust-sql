//! Road `dense.range` (cards/M3.md L2, H23b): dense keys from value-range facts. Owner M3-WORDS.
//!
//! A single integer group key `x % c` or `x / c`, `c` a positive integer constant, or (KEYED-23) `x + c`, `c + x`,
//! `x - c`, `c` any integer constant. keyed_emit_group's `key_shape` names the domain while this road is on:
//! - `x % c`: `Domain::Const [1 - c, c - 1]` (PostgreSQL's remainder takes the dividend's sign), which table.small takes
//!   when it fits a full table's slots;
//! - `x / c`, `x` a scan column: `Domain::Div { c }`, `x`'s lane bound (DenseLo / DenseHi) divided by `c` at the open,
//!   which this file's mechanism takes. It is table.small's slot form (its record, drain and radix overflow), reached
//!   through the registry: a stale bound sends a key to the overflow (speed, never a row), a span past T1's slots opens
//!   hashed, a nullable key (two image words) is never dense;
//! - `x + c`, `x` a scan column: `Domain::Shift { c }`, `x`'s lane bound shifted by `c` at the open, the same slot form.
//!   KEYED-23: any key whose value is `x + c` of one column through keysem's order images (table/face.rs `shifted`: an
//!   int-to-int cast, chains of integer offsets, a date offset) is that domain, and `x / c` reads `x` through casts.
//!
//! R2 (a dense timestamp bucket key) and R3 (finalize-on-match) are not built.
//!
//! Registers: road `dense.range` (`DENSE_RANGE`) as a `TableImpl`, after table.small in the resolver's order.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::table::face::{Domain, TableDecl, TableImpl, TableMech, TablePolicy};
use crate::table::keyimg::ImageWords;
use crate::face::strategy::TABLE_SMALL;
use crate::frame::env::Env;
use crate::frame::open::OpenWords;
use crate::frame::stop::Exits;
use crate::ops::Table;

crate::strategies! {
    /// Dense keys from value-range facts (x % c).
    DENSE_RANGE = 73, "dense.range", ExprKey;
}

pub(crate) const ROWS: Table = Table { strategies: &[(DENSE_RANGE, &TableImpl(&RANGE))], ..crate::ops::NONE };

struct Range;
static RANGE: Range = Range;

/// table.small's mechanism (its slot form keys a `Div` domain by its `domain()` arm).
fn small() -> &'static dyn TableMech {
    let m = crate::ops::strategy(TABLE_SMALL).and_then(|m| m.downcast_ref::<TableImpl>());
    m.expect("dense.range takes a table only while table.small is registered").0
}

impl TableMech for Range {
    fn open_words(&self, t: &TableDecl, w: &mut OpenWords) {
        small().open_words(t, w)
    }
    fn upsert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> (Value, Value) {
        small().upsert(b, env, t, img, h)
    }
    fn find(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, img: &ImageWords, h: Value) -> Value {
        small().find(b, env, t, img, h)
    }
    fn state_bytes(&self, t: &TableDecl) -> u32 {
        small().state_bytes(t)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        small().open(b, env, t)
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl) {
        small().window_end(b, env, t)
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, t: &TableDecl, x: &Exits) {
        small().finish(b, env, t, x)
    }
    /// One integer word whose `Div` or `Shift` domain's Facts words the maker declared, while table.small is on.
    fn takes(&self, t: &TableDecl) -> bool {
        let mapped = match t.key.domain {
            Domain::Div { c } => c > 0,
            Domain::Shift { .. } => true,
            Domain::Unknown | Domain::Const { .. } | Domain::Bound => false,
        };
        mapped && t.facts != 0 && t.image_words == 1 && t.layout.nkeys() == 1 && TablePolicy::DenseSmall.on()
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind};
    use gravity_abi::hash::Depth;

    use super::RANGE;
    use crate::table::face::{Domain, KeyShape, TableDecl, TableMech};

    fn decl(domain: Domain, facts: u16, keys: &[KeyKind]) -> TableDecl {
        let layout = RecordLayout::with_sentinel(keys, &[SlotKind::SumI64], false, None);
        TableDecl { image_words: keys.len() as u16, record_bytes: 16, expect: None, depth: Depth::TOP, life: gravity_program::LifeId(0), layout, state_off: 0, key: KeyShape { code: None, dense: None, domain }, facts, copies: 0 }
    }

    /// A `Div` domain with its Facts words, one key word; never a constant or bound domain (table.small's), a nullable key's
    /// two words, a domain without facts, or a non-positive divisor.
    #[test]
    fn takes_a_divided_bound_over_one_word() {
        assert!(RANGE.takes(&decl(Domain::Div { c: 1 }, 3, &[KeyKind::I64])));
        assert!(RANGE.takes(&decl(Domain::Div { c: i64::MAX }, 3, &[KeyKind::I32])));
        assert!(!RANGE.takes(&decl(Domain::Div { c: 0 }, 3, &[KeyKind::I64])));
        assert!(!RANGE.takes(&decl(Domain::Div { c: 7 }, 0, &[KeyKind::I64])), "a domain without its Facts words");
        assert!(!RANGE.takes(&decl(Domain::Div { c: 7 }, 3, &[KeyKind::I64, KeyKind::I64])), "a nullable key's flags word");
        assert!(RANGE.takes(&decl(Domain::Shift { c: -7 }, 3, &[KeyKind::I64])), "x - 7");
        assert!(RANGE.takes(&decl(Domain::Shift { c: i64::MAX }, 3, &[KeyKind::I32])));
        assert!(!RANGE.takes(&decl(Domain::Shift { c: 1 }, 0, &[KeyKind::I64])), "a shifted domain without its Facts words");
        assert!(!RANGE.takes(&decl(Domain::Shift { c: 1 }, 3, &[KeyKind::I64, KeyKind::I64])), "a nullable key's flags word");
        for d in [Domain::Const { lo: -6, hi: 6 }, Domain::Bound, Domain::Unknown] {
            assert!(!RANGE.takes(&decl(d, 3, &[KeyKind::I64])), "{d:?} is table.small's or the hashed table's");
        }
    }
}
