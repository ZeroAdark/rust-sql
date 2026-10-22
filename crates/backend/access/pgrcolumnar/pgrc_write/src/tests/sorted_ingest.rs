//! Declared-cluster-key sorted ingest (IN-1 / FT-6 / OD-8, M3-L2).
//!
//! The laws under test:
//! - a DECLARED key sorts natively at seal: for a unique total key, a
//!   shuffled feed and a presorted feed of the same multiset seal
//!   BYTE-IDENTICAL parts, and the part carries the FT-6 clustered witness
//!   (spec-§9 SortKey section, nkeys>0, + the SealReport leg);
//! - the sort is STABLE with PG null placement: nulls go where declared
//!   (NULLS LAST here) and equal-key/null groups keep ARRIVAL order — the
//!   sealed bytes are a pure function of (input sequence, declared key),
//!   predicted exactly by a stable partition oracle;
//! - an UNDECLARED table is legal and unlicensed (OD-8): nkeys = 0, no
//!   witness — mechanisms simply don't license;
//! - a key whose stored form is not its sort order refuses TYPED (a wrong
//!   order would silently poison every witness-gated consumer).

use super::*;
use crate::structural::{ClusterKeyDecl, NullsOrder, SortDir, StructuralPolicy};
use pgrc_format::sortkey::SortKeyRecord;

const FXID: u64 = 77;

fn asc_key(attno: u32) -> ClusterKeyDecl {
    ClusterKeyDecl {
        attno,
        dir: SortDir::Asc,
        nulls: NullsOrder::Last,
        path: Vec::new(),
    }
}

/// Seal one part of int8+text mixed rows fed in `order`, with (optionally)
/// a declared cluster key on the int8 column; return (tmp bytes, report).
fn seal_with_order(
    order: &[u64],
    key: Option<ClusterKeyDecl>,
) -> (Vec<u8>, crate::seal::SealReport) {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer_policy(
        vec![int8_col(1), text_col(2)],
        stamp(FXID, 1),
        PartCutPolicy {
            max_rows: u64::MAX,
            max_bytes: u64::MAX,
            cut_granule_rows: 128,
        },
    );
    if let Some(k) = key {
        w.set_structural(StructuralPolicy::new().with_cluster_key(vec![k]));
    }
    for &i in order {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs: &mut vfs,
        sources: &sources,
        resolver: &kit.resolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    w.finish(&mut env).expect("finish");
    assert_eq!(w.sealed_parts().len(), 1, "single-part fixture");
    let tmp = &w.sealed_parts()[0].tmp_name;
    let bytes = vfs.read_full(&format!("{DIR}/{tmp}")).expect("tmp bytes");
    (bytes, w.seal_reports()[0].clone())
}

/// Deterministic shuffle (LCG, the in-suite idiom).
fn shuffle_of(mut v: Vec<u64>, seed: u64) -> Vec<u64> {
    let mut state = seed;
    for i in (1..v.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        v.swap(i, j);
    }
    v
}

/// Row ids whose `mixed_row` int8 key is NEVER null and strictly ascends:
/// `mixed_row(i)` nulls the key iff `i % 13 == 0`, so `i = 13k + 1` avoids
/// every null while keeping keys unique and monotone in `k`.
fn unique_key_rows(n: u64) -> Vec<u64> {
    (0..n).map(|k| 13 * k + 1).collect()
}

fn view_of(bytes: &[u8]) -> PartView {
    PartView {
        bytes: bytes.to_vec(),
    }
}

#[test]
fn declared_key_sorts_unsorted_input_and_stamps_the_witness() {
    let n = 500u64;
    let presorted = unique_key_rows(n);
    let shuffled = shuffle_of(presorted.clone(), 0xC1D5_7E11_0234_9F5B);
    let (shuffled_bytes, shuffled_rep) = seal_with_order(&shuffled, Some(asc_key(1)));
    let (sorted_bytes, sorted_rep) = seal_with_order(&presorted, Some(asc_key(1)));
    // The sorted-ingest law (unique total key): sealed bytes are a pure
    // function of (multiset, declared key) — arrival order is erased.
    assert_eq!(
        shuffled_bytes, sorted_bytes,
        "shuffled and presorted feeds must seal byte-identical parts"
    );
    // Witness, report leg.
    assert_eq!(shuffled_rep.cluster_keys, vec![1]);
    assert_eq!(sorted_rep.cluster_keys, vec![1]);
    // Witness, on-disk leg: spec-§9 SortKey with the declared entry.
    let pv = view_of(&shuffled_bytes);
    let body = pv
        .section_bytes(SectionKind::SortKey, 0, 0)
        .expect("SortKey section present");
    let rec = SortKeyRecord::decode(&body).expect("well-formed");
    assert_eq!(rec.keys.len(), 1);
    assert_eq!(rec.keys[0].attno, 1);
    assert_eq!(rec.keys[0].dir, SortDir::Asc as u8);
    assert_eq!(rec.keys[0].nulls, NullsOrder::Last as u8);
    // O-10: the logical multiset identity is order-blind — the sort must
    // not change the column digests vs an UNDECLARED seal of the same
    // multiset.
    let (_, undeclared_rep) = seal_with_order(&shuffled, None);
    assert_eq!(shuffled_rep.col_hashes, undeclared_rep.col_hashes);
}

/// Stable sort + PG null placement, predicted EXACTLY: sealing a shuffled
/// feed (null keys included) equals sealing the stable-partition oracle of
/// that same feed — non-null keys ascending (unique, so total), then the
/// null-key rows in THEIR ARRIVAL ORDER (NULLS LAST; stability keeps the
/// suborder). This is the honest form of "arrival order is erased": it is
/// erased exactly up to the declared key, never beyond it.
#[test]
fn nulls_last_and_stability_match_the_partition_oracle() {
    let n = 400u64;
    let arrivals = shuffle_of((0..n).collect(), 0x5EED_0BAD_F00D_1234);
    let mut oracle: Vec<u64> = arrivals.iter().copied().filter(|i| i % 13 != 0).collect();
    oracle.sort_by_key(|&i| i as i64 - 5000); // the mixed_row int8 key
    oracle.extend(arrivals.iter().copied().filter(|i| i % 13 == 0));
    let (got, rep) = seal_with_order(&arrivals, Some(asc_key(1)));
    let (want, _) = seal_with_order(&oracle, Some(asc_key(1)));
    assert_eq!(got, want, "stable NULLS LAST partition oracle diverged");
    assert_eq!(rep.cluster_keys, vec![1]);
}

#[test]
fn undeclared_table_stays_unordered_nkeys_zero() {
    let arrivals = shuffle_of((0..300).collect(), 0xA5A5_5A5A_1111_2222);
    let (bytes, rep) = seal_with_order(&arrivals, None);
    assert!(rep.cluster_keys.is_empty());
    let pv = view_of(&bytes);
    let body = pv
        .section_bytes(SectionKind::SortKey, 0, 0)
        .expect("SortKey section always present");
    let rec = SortKeyRecord::decode(&body).expect("well-formed");
    assert!(rec.keys.is_empty(), "OD-8: undeclared = unordered part");
}

#[test]
fn non_byte_ordered_collation_key_refuses_typed() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    // A text column under a non-C deterministic collation: strcmp is NOT
    // its sort order — the declared key must refuse, not mis-sort.
    let mut col = text_col(1);
    col.collation_class = CollationClass::OtherDeterministic;
    let mut w = open_writer_policy(vec![col], stamp(FXID, 1), PartCutPolicy::default());
    w.set_structural(StructuralPolicy::new().with_cluster_key(vec![asc_key(1)]));
    let imgs = [img_4b_u(b"zz"), img_4b_u(b"aa")];
    for image in &imgs {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.append_row(&[RawDatum::Bytes(image)], &mut kit.ext, &mut env)
            .expect("append buffers only");
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs: &mut vfs,
        sources: &sources,
        resolver: &kit.resolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    let err = w.finish(&mut env).expect_err("must refuse at seal");
    assert!(
        matches!(err, crate::WriteError::Refused { what } if what.contains("cluster key")),
        "typed refusal expected, got {err:?}"
    );
}

#[test]
fn descending_key_seals_descending() {
    let n = 200u64;
    let desc = ClusterKeyDecl {
        attno: 1,
        dir: SortDir::Desc,
        nulls: NullsOrder::Last,
        path: Vec::new(),
    };
    let mut presorted_desc = unique_key_rows(n);
    presorted_desc.reverse();
    let shuffled = shuffle_of(unique_key_rows(n), 0x0DDB_A11_5EED_77);
    assert_eq!(
        seal_with_order(&shuffled, Some(desc.clone())).0,
        seal_with_order(&presorted_desc, Some(desc)).0,
        "desc-declared shuffled feed must equal the desc-presorted feed"
    );
}

// ---------------------------------------------------------------------------
// The enforced `sorted_by`: composite keys, the
// per-part promise, and the sortedness the meta builder stamps from the
// produced order.
// ---------------------------------------------------------------------------

use pgrc_format::geom;
use pgrc_format::meta::{Sortedness, StatsRecord, STATS_RECORD_LEN};
use pgrc_format::wire::Cur;

/// Seal `order` (row ids of `mixed_row`) under `keys` with `policy`; return
/// every sealed part's tmp bytes in seal order.
fn seal_parts(order: &[u64], keys: Vec<ClusterKeyDecl>, policy: PartCutPolicy) -> Vec<Vec<u8>> {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy);
    if !keys.is_empty() {
        w.set_structural(StructuralPolicy::new().with_cluster_key(keys));
    }
    for &i in order {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    w.sealed_parts().iter().map(|p| vfs.read_full(&format!("{DIR}/{}", p.tmp_name)).expect("tmp bytes")).collect()
}

fn one_part(order: &[u64], keys: Vec<ClusterKeyDecl>) -> Vec<u8> {
    let mut parts = seal_parts(order, keys, PartCutPolicy { max_rows: u64::MAX, max_bytes: u64::MAX, cut_granule_rows: 128 });
    assert_eq!(parts.len(), 1, "single-part fixture");
    parts.pop().unwrap()
}

/// The (granule records, part record) of `attno`'s Stats section.
fn stats_records(bytes: &[u8], rows: u64, attno: u32) -> (Vec<StatsRecord>, StatsRecord) {
    let body = view_of(bytes).meta_body(SectionKind::Stats, attno, 0).expect("Stats section");
    let (gc, bc) = (geom::granule_count(rows), geom::band_count(rows));
    assert_eq!(body.len(), (gc as usize + bc as usize + 1) * STATS_RECORD_LEN);
    let mut c = Cur::new(&body);
    let g: Vec<StatsRecord> = (0..gc).map(|_| StatsRecord::decode(&mut c).expect("granule")).collect();
    for _ in 0..bc {
        StatsRecord::decode(&mut c).expect("band");
    }
    (g, StatsRecord::decode(&mut c).expect("part"))
}

/// The stable (int8 NULLS LAST, text NULLS LAST) order of `mixed_row` ids —
/// the oracle for a composite key.
fn composite_oracle(arrivals: &[u64]) -> Vec<u64> {
    let mut v: Vec<u64> = arrivals.to_vec();
    v.sort_by(|&a, &b| {
        let (ka, ta) = mixed_row(a);
        let (kb, tb) = mixed_row(b);
        let nulls_last = |x: &Option<i64>, y: &Option<i64>| match (x, y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.cmp(y),
        };
        nulls_last(&ka, &kb).then_with(|| match (&ta, &tb) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.as_bytes().cmp(y.as_bytes()),
        })
    });
    v
}

/// A two-column key (int8, C-collated text): a random feed seals
/// byte-identical to the stable composite oracle fed in order, and the
/// leading key's sortedness is Ascending (or Constant) in every granule and
/// in the part record — the builder read the produced order.
#[test]
fn composite_key_sorts_lexicographically_and_stamps_sortedness() {
    let n = 1_000u64;
    let arrivals = shuffle_of((0..n).map(|i| i % 250).collect(), 0x0C0F_FEE0_1234_5678);
    let keys = vec![asc_key(1), asc_key(2)];
    let got = one_part(&arrivals, keys.clone());
    let want = one_part(&composite_oracle(&arrivals), keys);
    assert_eq!(got, want, "random feed must seal as the (k, t) stable oracle");
    let (g, p) = stats_records(&got, n, 1);
    for (gi, rec) in g.iter().enumerate() {
        let s = Sortedness::from_u8(rec.sortedness).unwrap();
        assert!(matches!(s, Sortedness::Ascending | Sortedness::Constant), "granule {gi}: {s:?}");
    }
    assert_eq!(Sortedness::from_u8(p.sortedness).unwrap(), Sortedness::Ascending, "part record");
    // The contrast: the same feed undeclared keeps its arrival order, and
    // the builder says so.
    let plain = one_part(&arrivals, Vec::new());
    let (_, p) = stats_records(&plain, n, 1);
    assert_eq!(Sortedness::from_u8(p.sortedness).unwrap(), Sortedness::Unknown, "undeclared part record");
}

/// The per-part promise: a feed that cuts into two parts is sorted WITHIN
/// each part — each equals the part sealed from that chunk's own oracle —
/// and nothing across them (the second part starts below the first's end).
#[test]
fn two_parts_are_each_sorted_and_not_ordered_across() {
    let cut = PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: 128 };
    let arrivals = shuffle_of(unique_key_rows(512), 0x7A57_0000_BEEF_CAFE);
    let parts = seal_parts(&arrivals, vec![asc_key(1)], cut);
    assert_eq!(parts.len(), 2, "256-row cut over 512 rows");
    let (first, second) = arrivals.split_at(256);
    let mut oracle_first = first.to_vec();
    let mut oracle_second = second.to_vec();
    oracle_first.sort();
    oracle_second.sort();
    let want = seal_parts(&[oracle_first.clone(), oracle_second.clone()].concat(), vec![asc_key(1)], cut);
    assert_eq!(parts, want, "each part sorted on its own rows, seal order kept");
    let (_, p1) = stats_records(&parts[0], 256, 1);
    let (_, p2) = stats_records(&parts[1], 256, 1);
    assert_eq!(Sortedness::from_u8(p1.sortedness).unwrap(), Sortedness::Ascending);
    assert_eq!(Sortedness::from_u8(p2.sortedness).unwrap(), Sortedness::Ascending);
    assert!(p2.min_key < p1.max_key, "a random split overlaps across parts: {} < {}", p2.min_key, p1.max_key);
}
