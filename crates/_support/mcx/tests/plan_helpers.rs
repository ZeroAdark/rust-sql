use mcx::{MemoryContext, PgVec};

#[test]
fn plan_helpers_match_std() {
    use mcx::CollectIn;
    let ctx = MemoryContext::new("t");
    let m = ctx.mcx();
    let v: PgVec<'_, u32> = (1..5).collect_in(m);
    assert_eq!(&v[..], &[1, 2, 3, 4]);
    assert_eq!(&mcx::to_vec_in(&v[1..], m)[..], &[2, 3, 4]);
    assert_eq!(&mcx::concat_in(&[&v[..2], &[9u32][..]], m)[..], &[1, 2, 9]);
    let e: PgVec<'_, u8> = mcx::pg_vec![in m];
    assert!(e.is_empty());
    let r = mcx::pg_vec![in m; String::from("x"); 3];
    assert_eq!(r.len(), 3);
    assert_eq!(&mcx::pg_vec![in m; 4, 5, 6][..], &[4, 5, 6]);
    let s = mcx::pg_format!(m, "{}-{:03}", "a", 7);
    assert_eq!(s.as_str(), "a-007");
    assert_eq!(mcx::pg_format!(m, "plain").as_str(), "plain");
    let mut t = mcx::PgString::new_in(m);
    t.push_str("ab");
    t.push('c');
    assert_eq!(t.as_str(), "abc");
    let d = mcx::pg_box_dyn!(m, 7u64 => dyn core::fmt::Debug);
    assert_eq!(format!("{d:?}"), "7");
}

#[test]
fn pg_rc_counts_and_frees() {
    use mcx::PgRc;
    let ctx = MemoryContext::new("t");
    let m = ctx.mcx();
    let a = PgRc::new_in(String::from("v"), m);
    let b = a.clone();
    assert_eq!(PgRc::strong_count(&a), 2);
    assert!(PgRc::ptr_eq(&a, &b));
    drop(b);
    let mut a = a;
    PgRc::get_mut(&mut a).unwrap().push('w');
    assert_eq!(a.as_str(), "vw");
    let s = PgRc::from_slice_in(&[1u16, 2, 3][..], m);
    let s2 = s.clone();
    assert_eq!(&s2[..], &[1, 2, 3]);
    assert_eq!(s, s2);
    let z: PgRc<'_, [u64]> = PgRc::from_slice_in(&[][..], m);
    assert!(z.is_empty());
    drop((a, s, s2, z));
    assert_eq!(PgRc::strong_count(&PgRc::try_new_in(1u8, m).unwrap()), 1);
}
