//! catalog::keys: a sort key's typing by its ordering operator (FILE-MAP: decompose sort.rs "key typing", in the catalog
//! home decompose reaches; the sort lane's). A btree ordering operator's place in its family (`(opfamily, lefttype,
//! righttype, strategy)`, the builtin btree table's row: `effects::builtin_btree_member`) says the key's direction, whether
//! its family orders by bytes whatever the collation, and which type's run image it takes, so no consumer lists operator
//! oids (overfit OID-08 / OID-09 / OIDW-22 / OIDW-23 / OIDW-83 / OIDW-84).

use types_core::catalog::{BPCHAROID, NAMEOID};

type Oid = u32;

/// btree text_pattern_ops (varchar_pattern_ops is the same family): `bttext_pattern_cmp` orders by the bytes.
const TEXT_PATTERN_BTREE_FAM_OID: Oid = 2095;
/// btree bpchar_pattern_ops: `btbpchar_pattern_cmp` orders by the bytes, trailing blanks off.
const BPCHAR_PATTERN_BTREE_FAM_OID: Oid = 2097;

/// The type whose own run image a key takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typing {
    /// name's 64 bytes (rep.word.order under a collation that orders by bytes).
    Name,
    /// bpchar's compare: trailing blanks ignored (`bpcharcmp`, `btbpchar_pattern_cmp`).
    Bpchar,
    /// Any other type's.
    Other,
}

/// An ordering operator's key facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyOrder {
    /// Strategy 5 (`>`): the key sorts descending.
    pub desc: bool,
    /// Its family's comparator orders by the values' bytes whatever the collation (the pattern families, like_support.c's
    /// `TEXT_PATTERN_BTREE_FAM_OID` / `BPCHAR_PATTERN_BTREE_FAM_OID`): the key sorts as under "C".
    pub bytes: bool,
    pub typing: Typing,
}

/// The key facts of builtin operator `opno` (the builtin btree table); none for an operator in no builtin btree family.
pub fn key_order(opno: Oid) -> Option<KeyOrder> {
    crate::effects::builtin_btree_member(opno).and_then(order_of)
}

/// The key facts of the operator at `member` (`(opfamily, lefttype, righttype, strategy)`): an ordering operator (strategy 1
/// `<` or 5 `>`) over one type; none for any other member (a cross-type one, an equality).
pub fn order_of((family, left, right, strategy): (Oid, Oid, Oid, u8)) -> Option<KeyOrder> {
    if left != right || !matches!(strategy, 1 | 5) {
        return None;
    }
    let typing = match left {
        NAMEOID => Typing::Name,
        BPCHAROID => Typing::Bpchar,
        _ => Typing::Other,
    };
    Some(KeyOrder { desc: strategy == 5, bytes: matches!(family, TEXT_PATTERN_BTREE_FAM_OID | BPCHAR_PATTERN_BTREE_FAM_OID), typing })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The members the sort keys read: name's and bpchar's default `<` / `>`, the pattern families' `~<~` / `~>~`, and no
    /// key for a cross-type member or an equality; the builtin table gives exactly the operators the old lists named.
    #[test]
    fn a_member_says_its_key() {
        let k = |m| order_of(m);
        assert_eq!(k((1986, NAMEOID, NAMEOID, 1)), Some(KeyOrder { desc: false, bytes: false, typing: Typing::Name }));
        assert_eq!(k((426, BPCHAROID, BPCHAROID, 5)), Some(KeyOrder { desc: true, bytes: false, typing: Typing::Bpchar }));
        assert_eq!(k((2095, 25, 25, 5)), Some(KeyOrder { desc: true, bytes: true, typing: Typing::Other }));
        assert_eq!(k((2097, BPCHAROID, BPCHAROID, 1)), Some(KeyOrder { desc: false, bytes: true, typing: Typing::Bpchar }));
        assert_eq!(k((1994, NAMEOID, 25, 1)), None, "cross-type");
        assert_eq!(k((1976, 23, 23, 3)), None, "an equality");
        let typed = |t: Typing| (1..5000u32).filter(|&o| key_order(o).is_some_and(|x| x.typing == t)).collect::<Vec<_>>();
        assert_eq!(typed(Typing::Name), [660, 662], "namelt / namegt");
        assert_eq!(typed(Typing::Bpchar), [1058, 1060, 2326, 2330], "bpcharlt / bpchargt, ~<~ / ~>~ of bpchar_pattern_ops");
        let bytes: Vec<_> = (1..5000u32).filter(|&o| key_order(o).is_some_and(|x| x.bytes)).map(|o| (o, key_order(o).unwrap().desc)).collect();
        assert_eq!(bytes, [(2314, false), (2318, true), (2326, false), (2330, true)], "the pattern families' ~<~ / ~>~");
    }
}
