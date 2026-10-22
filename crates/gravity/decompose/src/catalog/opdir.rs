//! OID-07 / OIDW-81 / OIDW-82: the bootstrap catalog's btree answers for a facts source that reads no catalog (the
//! decompose fixtures, the unit-test sources): `Facts::key` / `Facts::op_eq`'s default. The server's `RelcacheFacts`
//! answers both from `pg_amop` (`get_ordering_op_properties` and the type cache's default family / `get_mergejoin_opfamilies`).
//! These answers apply the same rules to the bootstrap rows the generator copies from PostgreSQL 18.6's .dat files
//! (effects/procfacts.rs `BTREE`, `BTREE_DEFAULT`), so no operator list is kept by hand.

use crate::effects::procfacts::{BTREE, BTREE_DEFAULT};

/// The builtin btree pg_amop rows of operator `opno` (`BTREE` is sorted by operator, then family: the AMOPOPID order).
fn rows(opno: u32) -> &'static [(u16, u16, u16, u16, u8)] {
    let Ok(op) = u16::try_from(opno) else { return &[] };
    let lo = BTREE.partition_point(|r| r.0 < op);
    let hi = lo + BTREE[lo..].partition_point(|r| r.0 == op);
    &BTREE[lo..hi]
}

/// Type `typid`'s default btree opfamily (its default btree opclass's family, the type cache's `btree_opf`).
fn default_family(typid: u16) -> Option<u16> {
    BTREE_DEFAULT.binary_search_by_key(&typid, |r| r.0).ok().map(|i| BTREE_DEFAULT[i].1)
}

/// A sort operator's direction (`Some(true)` = descending): the first btree row of `opno` with equal input types and
/// strategy 1 `<` / 5 `>` (`get_ordering_op_properties`), when its family is that type's default (decision 49, D1346).
/// None for a cross-type comparison, an operator of another family, or one that is no btree member.
pub fn builtin_dir(opno: u32) -> Option<bool> {
    let &(_, fam, l, _, s) = rows(opno).iter().find(|r| r.2 == r.3 && matches!(r.4, 1 | 5))?;
    (default_family(l) == Some(fam)).then_some(s == 5)
}

/// KeySem slice 3: a sort operator's `KeyOrder` from the bootstrap rows (`Facts::key`'s default): the first btree row of
/// `opno` with equal input types and strategy 1 / 5, any family. The rows carry no `pg_amproc`, so `cmp` is 0 (unknown;
/// the server's source reads it, typesem `keysem::sort_order`) and no family is taken to lack its procs (`no_proc` false).
pub fn builtin_key(opno: u32) -> Option<super::KeyOrder> {
    let &(_, fam, l, _, s) = rows(opno).iter().find(|r| r.2 == r.3 && matches!(r.4, 1 | 5))?;
    Some(super::KeyOrder { desc: s == 5, family: fam as u32, opcintype: l as u32, default_family: default_family(l) == Some(fam), cmp: 0, no_proc: false })
}

/// Type `typid`'s default btree `<` (the type cache's `lt_opr`: strategy 1 of the type's default family over the type
/// itself), the operator PostgreSQL's ASC sort of a column of that type orders by. `varchar` has no btree opclass of its
/// own: its default is text's (binary coercible), so it sorts by text's `<`. None for a type with no default btree family
/// among the bootstrap rows.
pub fn builtin_lt(typid: u32) -> Option<u32> {
    let t = if typid == super::VARCHAROID { super::TEXTOID } else { typid };
    let t = u16::try_from(t).ok()?;
    let fam = default_family(t)?;
    BTREE.iter().find(|r| r.1 == fam && r.2 == t && r.3 == t && r.4 == 1).map(|r| r.0 as u32)
}

/// Whether `opno` is a btree equality: strategy 3 of some builtin btree opfamily (`get_mergejoin_opfamilies` non-empty).
pub fn builtin_eq(opno: u32) -> bool {
    rows(opno).iter().any(|r| r.4 == 3)
}

#[cfg(test)]
mod tests {
    use crate::catalog::{Facts, KeyOrder, KeyReq, NoFacts};

    /// OID-07: a source that reads no catalog answers the bootstrap catalog's btree facts through `Facts`'s defaults
    /// (int4 `<` 97 ascending, `>` 521 descending, `=` 96 an equality); an operator it never saw has none.
    #[test]
    fn no_catalog_source_answers_bootstrap_btree_facts() {
        let dir = |opno| NoFacts.key(KeyReq::sort(opno)).and_then(KeyOrder::dir);
        assert_eq!((dir(97), dir(521), dir(70_000)), (Some(false), Some(true), None));
        assert!(NoFacts.op_eq(96) && !NoFacts.op_eq(97) && !NoFacts.op_eq(70_000));
    }

    /// USEROP / KeySem slice 3: `key`'s default agrees with the direction table (`builtin_dir`) on every bootstrap ordering
    /// operator of a default family (int4 `<` 97, `>` 521: integer_ops 1976 over int4 23), names a non-default family's
    /// operator with `default_family` false (text_pattern_ops' `~<~` 2314: family 2095), and none for an equality or an
    /// operator it never saw. `cmp` is 0 and `no_proc` false without a catalog.
    #[test]
    fn no_catalog_source_answers_bootstrap_orderings() {
        let key = |opno| NoFacts.key(KeyReq::sort(opno));
        assert_eq!(key(97), Some(KeyOrder { desc: false, family: 1976, opcintype: 23, default_family: true, cmp: 0, no_proc: false }));
        assert_eq!(key(521), Some(KeyOrder { desc: true, family: 1976, opcintype: 23, default_family: true, cmp: 0, no_proc: false }));
        let p = key(2314).expect("~<~ is a btree ordering operator");
        assert!(!p.default_family && !p.desc && p.family == 2095 && p.opcintype == 25 && p.dir().is_none(), "{p:?}");
        assert_eq!((key(96), key(70_000)), (None, None));
        for opno in 1..5000 {
            assert_eq!(key(opno).and_then(KeyOrder::dir), super::builtin_dir(opno), "operator {opno}");
            assert!(key(opno).map_or(true, |o| o.cmp == 0 && !o.no_proc), "operator {opno}");
        }
    }

    /// OIDW-81 / OIDW-82: the answers follow the server's rules, not a list. Does not apply: a cross-type comparison
    /// (int4 `<` int8, 37) has no direction, nor does an operator in no btree family (box `<` 504, by area) or a
    /// non-default family's (text `~<~` 2314, text_pattern_ops); a non-default family's equality is still an equality
    /// (record `*=` 3188, record_image_ops), and so is a cross-type one (int4 `=` int8, 15).
    #[test]
    fn bootstrap_answers_follow_the_servers_rules() {
        assert_eq!([37, 504, 2314, 664, 1072].map(super::builtin_dir), [None, None, None, Some(false), Some(false)]);
        assert_eq!([76, 502, 666].map(super::builtin_dir), [None, None, Some(true)]);
        assert!(super::builtin_eq(3188) && super::builtin_eq(15) && super::builtin_eq(98));
        assert!(!super::builtin_eq(97) && !super::builtin_eq(504) && !super::builtin_eq(70_000));
    }

    /// r15 rows-road merge sort: each merge key type's default `<` (int2 95, int4 97, int8 412, text 664 and varchar's,
    /// text's, date 1095, timestamp 2062, timestamptz 1322), an ascending ordering operator of its default family; none
    /// for a type with no default btree family (point 600).
    #[test]
    fn a_types_default_less_than() {
        let types = [21, 23, 20, 25, 1043, 1082, 1114, 1184, 600];
        assert_eq!(types.map(super::builtin_lt), [Some(95), Some(97), Some(412), Some(664), Some(664), Some(1095), Some(2062), Some(1322), None]);
        for t in types.into_iter().filter_map(super::builtin_lt) {
            assert_eq!(super::builtin_dir(t), Some(false), "operator {t}");
        }
    }
}
