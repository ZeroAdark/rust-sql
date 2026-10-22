//! D4439 (the over-count): a temp file mapped shared (`TempMap`, the road past the grant pool of a hosted aggregate's
//! arena, a kernel pin, a spool) is charged at its whole length on door 2 (Kind::Spill), and its account's resident bytes
//! (`AccountRow::resident_bytes`, the view's account_bytes) are that charge less what the residency pass found not
//! resident. A temp map's pages the page cache wrote back and dropped, or never read in, are not memory, so they must be
//! off its account as a region's untouched pages are (D3872). Four hash aggregates in an 8 GiB scope (box 45) summed
//! account_bytes 9.1 GiB: 5.0 GiB of it temp maps of which smaps found 0.12 GiB resident.

#![cfg(target_os = "linux")]

use std::os::fd::AsRawFd;

use gravity_driver::TempMap;

const MIB: usize = 1 << 20;

#[test]
fn d4439_a_temp_maps_pages_out_of_memory_are_not_resident_on_its_account() {
    let a = memledger::Acct::of(memledger::Consumer::FpTables);
    let _on = memledger::charge::enter_as(memledger::Owner::Cache(memledger::Holder::of_acct(a)));
    let nr = || memledger::account_row(a).not_resident_mapped_bytes;
    let (before, mapped) = (nr(), memledger::account_row(a).mapped_bytes);
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("d4439-tempmap-{}", std::process::id()));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("a temp file");
    let _ = std::fs::remove_file(&path);
    let len = 64 * MIB;
    // a sparse file: none of its pages is in the page cache
    f.set_len(len as u64).expect("the file's length");
    let m = TempMap::shared(f.as_raw_fd(), len).expect("a temp map");
    drop(f);
    assert_eq!(memledger::account_row(a).mapped_bytes - mapped, len as i64, "charged at its length on door 2");
    assert!(gravity_driver::mem::resident::pass_all());
    let fresh = nr() - before;
    assert!(fresh >= (len - 2 * MIB) as i64, "a temp map's pages not in memory are not resident on its account ({} of {} MiB)", fresh >> 20, len >> 20);
    // SAFETY: the map's `len` bytes are readable and writable (`TempMap::shared`)
    unsafe { core::ptr::write_bytes(m.as_mut_ptr(), 0x5a, 8 * MIB) };
    assert!(gravity_driver::mem::resident::pass_all());
    let written = nr() - before;
    assert!(((len - 10 * MIB) as i64..=(len - 8 * MIB) as i64).contains(&written), "its written pages are resident ({} MiB not resident)", written >> 20);
    drop(m);
    assert_eq!(nr(), before, "its not-resident bytes leave with it");
}
