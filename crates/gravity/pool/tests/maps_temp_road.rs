//! C10 (D4168): the server's spill sets claim a temp-file road (`maps_temp`) only where `TempMap::shared` maps a file.

use std::os::fd::AsRawFd;

use gravity_driver::{SpillFactory, TempMap};

#[test]
fn the_spill_sets_claim_a_temp_file_road_only_where_a_temp_file_maps() {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("maps-temp-road-{n}"));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("a temp file");
    let _ = std::fs::remove_file(&path);
    let len = 64 << 10;
    f.set_len(len as u64).expect("the file's length");
    let maps = TempMap::shared(f.as_raw_fd(), len).is_some();
    assert_eq!(gravity_pool::SPILL_SETS.maps_temp(), maps, "the spill sets claim the road exactly where a temp file maps");
}
