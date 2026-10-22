//! The word types a run image orders as their btree order does: a group whose keys are such words emits its groups in
//! the Sort's order from the keys' words alone (decompose ops/group/absorb.rs `same_when_equal`).
use crate::catalog::typefacts::TypeFacts;

/// Whether the word the emit sorts a key of type facts `tf` by orders as the type's btree order. The run record holds a
/// key's image words (table/keyimg.rs, rep.word.hash `int_image`: a narrow word sign-extended when its image is signed,
/// zero-extended when not) and runs_topk_groups `key_words` compares them signed, or unsigned for an 8-byte unsigned
/// by-value image (its `SK_UNSIGNED`). So every by-value datum-word image orders as its value: signed (int2, int4, int8,
/// money, date, time, timestamp, timestamptz), a narrow unsigned word zero-extended into the word's positive half (bool,
/// "char", oid and the reg* types), an 8-byte unsigned word compared unsigned (pg_lsn, xid8). That is the type's btree
/// order when the table knows the type's default comparator (`cmp`: a type the table does not list has none, and its
/// comparator may order otherwise). A domain reads its base's row. Not ordered: an enum (its oid word is not its sort
/// order once a value is added out of order), every image that is not the datum word.
pub(crate) fn ordered(tf: &TypeFacts) -> bool {
    tf.cmp != 0 && tf.byval && tf.image.is_word()
}

#[cfg(test)]
mod tests {
    use super::ordered;
    use crate::catalog::typefacts::{opaque, row_of, TypeFacts, ROWS};

    /// The rows the reading names over PostgreSQL 18.6's built-in types: bool, "char", int8, int2, int4, regproc, oid,
    /// money, date, time, timestamp, timestamptz, the reg* types, pg_lsn, xid8 (xid and cid have no btree comparator).
    #[test]
    fn the_word_ordered_rows() {
        let got: Vec<u32> = ROWS.iter().filter(|r| ordered(r)).map(|r| r.typid).collect();
        assert_eq!(got, [16, 18, 20, 21, 23, 24, 26, 790, 1082, 1083, 1114, 1184, 2202, 2203, 2204, 2205, 2206, 3220, 3734, 3769, 4089, 4096, 4191, 5069]);
    }

    /// A domain orders as its base; a by-value type the table does not list (no known comparator) never does.
    #[test]
    fn a_domain_reads_its_base_and_an_unlisted_word_is_not_ordered() {
        let dom = |base: u32| TypeFacts { typid: 90_001, ..*row_of(base).unwrap() };
        assert!(ordered(&dom(16)) && ordered(&dom(5069)));
        assert!(ordered(&dom(18)) && ordered(&dom(26)) && !ordered(&dom(28)) && !ordered(&dom(27)));
        assert!(!ordered(&opaque(90_002, 90_002, 8, true, false)) && !ordered(&opaque(90_003, 90_003, 4, true, false)));
        assert!(!ordered(&opaque(90_004, 90_004, 4, true, true)));
    }
}
