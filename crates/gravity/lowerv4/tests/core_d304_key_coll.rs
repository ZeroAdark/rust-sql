//! D304 (M1-TABLES, main 09-24) / D823 (M6-TEXT): a group or DISTINCT key under a nondeterministic collation keys by
//! key.coll's image, the Fp128 of the provider's sort key (`text_strnxfrm`; PG's hashtext under such a collation), never by
//! its bytes. A keyed record keys a value by its image; a bytes image (a code, the Fp128 of its bytes) is equal exactly
//! when the bytes are, PG's equality only under a collation whose equal bytes are its equal values (`KeyColl.bytes_eq`).
//! An ICU level-2 collation was once keyed by bytes, so count(DISTINCT t COLLATE ci) counted 'apple' and 'APPLE' apart
//! (REP's m1_coll: 37 where PG gives 26). pgrc has no row-executor fallback, so the image is pinned here, over census
//! plan fixtures under their own collation (no sort key) and with the key's collation swapped for a user collation's oid:
//! the corpus facts answer `KeyColl::of` for it (not bytes_eq), as the catalog answers a nondeterministic one.
use gravity_lowerv4_it as common;

fn fixture(name: &str) -> String {
    let p = common::plans_dir().join(format!("{name}.plan"));
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Whether `text` lowers, and then whether its program calls the sort-key host.
fn strnxfrm(name: &str, text: &str) -> bool {
    match common::lower_fixture(text) {
        Some(Ok(l)) => l.l.ir.host_fns.iter().any(|h| h.name == "text_strnxfrm"),
        Some(Err(e)) => panic!("{name} refused {e}: a nondeterministic key is key.coll's image (D823)"),
        None => panic!("{name}: decompose declined the fixture"),
    }
}

/// Each fixture lowers by its bytes as it is, and by the sort key once its key's collation field `from` reads `to`, a user
/// collation's oid (>= FIRST_NORMAL_OID: `KeyColl::of` gives it `bytes_eq = false`).
fn keys_by_sort_key(cases: &[(&str, &str, &str)]) {
    for &(name, from, to) in cases {
        let text = fixture(name);
        assert!(!strnxfrm(name, &text), "{name} under its own collation keys by its bytes");
        let ci = text.replace(from, to);
        assert_ne!(ci, text, "{name}: no {from:?} to swap");
        assert!(strnxfrm(name, &ci), "{name} under a nondeterministic collation is keyed by its bytes (D304)");
    }
}

/// keyed.distinct_plain: count(DISTINCT <text column>) (aggs-041 under the database default, fieldobs-010 under "C").
#[test]
fn a_distinct_key_under_a_nondeterministic_collation_keys_by_its_sort_key() {
    keys_by_sort_key(&[("aggs-041", ":inputcollid 100", ":inputcollid 16390"), ("fieldobs-010", ":inputcollid 950", ":inputcollid 16390")]);
}

/// keyed.hashagg's record (GroupRec::new, every keyed end's) over a group key: fieldobs-044's derived text key (a regexp host), under "C".
#[test]
fn a_group_key_under_a_nondeterministic_collation_keys_by_its_sort_key() {
    keys_by_sort_key(&[("fieldobs-044", ":grpCollations ( 950)", ":grpCollations ( 16390)")]);
}
