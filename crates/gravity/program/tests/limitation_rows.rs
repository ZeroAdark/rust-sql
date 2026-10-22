//! A SPILL-DEBT reason's user sentence (`NoSpill::limitation`, which exec glue writes in the server log beside an out of
//! memory the reason names) is its ledger row's field 8 (lint/SPILL-DEBT/<reason>): an edit of one is an edit of both.
use gravity_program::spill::NoSpill;

#[test]
fn limitation_is_field_8() {
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")).join("../lint/SPILL-DEBT");
    for r in NoSpill::ALL {
        // no row: a lowering that skipped its plan, named as a defect
        if r == NoSpill::Unrouted {
            continue;
        }
        let name = format!("{r:?}");
        let row = std::fs::read_to_string(dir.join(&name)).unwrap_or_else(|e| panic!("{name}: its row: {e}"));
        let fields: Vec<&str> = row.trim_end().split('\t').collect();
        assert_eq!(fields[0], name, "the row's file names its reason");
        let f8 = fields.get(7).unwrap_or_else(|| panic!("{name}: its row has a field 8"));
        assert_eq!(r.limitation(), *f8, "{name}: NoSpill::limitation is the row's field 8");
    }
}
