// A code lane's DictRef learns its fingerprint table when the site declares it (design 4.3's per-dictionary fp128 table).
use gravity_program::{Arena, DictRef, LaneLife, Rep, SiteDesc, TableDecl};

#[test]
fn a_fingerprint_table_is_recorded_on_its_code_lane() {
    let st = Arena::new(1 << 16);
    let mut s = SiteDesc::range_source(st.handle(), 7, &[1, 2]).codes_lane(1);
    assert_eq!((s.lanes()[1].rep, s.lanes()[1].life), (Rep::Code { dict: DictRef::lane(1) }, LaneLife::DICTIONARY));
    let t = s.declare_table(TableDecl::fingerprint(1)).unwrap();
    assert_eq!(s.lanes()[1].rep, Rep::Code { dict: DictRef { lane: 1, fp_table: t as u8 } });
}
