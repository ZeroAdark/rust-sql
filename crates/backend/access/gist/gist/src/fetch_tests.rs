use crate::get::form_fetch_tuple;
use ::datum::Datum;
use ::mcx::{MemoryContext, PgVec};
use ::types_tuple::{CompactAttribute, FormData_pg_attribute, TupleDescData};

// wio P247: an index-only scan returns a btree_gist text key of 32000 bytes
// (stored compressed); the fetched value must form past the 8191-byte
// index-row limit, as C's heap_form_tuple does.
#[test]
fn fetch_tuple_carries_keys_wider_than_an_index_row() {
    let ctx = MemoryContext::new("gist-fetch");
    let mcx = ctx.mcx();
    let att = FormData_pg_attribute {
        attnum: 1,
        atttypid: 25,
        atttypmod: -1,
        attlen: -1,
        attbyval: false,
        attalign: b'i' as i8,
        attstorage: b'x' as i8,
        ..Default::default()
    };
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    compact.push(CompactAttribute::populate_from(&att));
    attrs.push(att);
    let desc = TupleDescData {
        natts: 1,
        tdtypeid: 0,
        tdtypmod: -1,
        tdrefcount: -1,
        constr: None,
        compact_attrs: compact,
        attrs,
    };
    let total = 4 + 32_000usize;
    let mut v = vec![b'a'; total];
    v[..4].copy_from_slice(&((total as u32) << 2).to_le_bytes());
    let formed =
        form_fetch_tuple(mcx, &desc, &[Datum::from_usize(v.as_ptr() as usize)], &[false]).unwrap();
    assert!(formed.as_tuple().t_len as usize > total);
}
