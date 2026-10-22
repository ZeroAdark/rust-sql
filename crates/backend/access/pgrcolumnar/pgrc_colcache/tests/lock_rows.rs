//! The tombstone image skips every LOCK row before it classifies the header.

use pgrc_colcache::block::{BlockImage, Ctx};
use pgrc_colcache::{Kind, TxnOracle};
use pgrc_format::rowid::{SUCC_LOCK, SUCC_NONE};
use types_core::{TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

/// An opaque row ID for the test's tombstone keys (IX0: IDs are 40-bit; these stay below 2^40).
fn rowident(part_no: u32, pos: u32) -> u64 {
    u64::from(part_no) << 32 | u64::from(pos)
}

struct Committed;

impl TxnOracle for Committed {
    fn is_current(&self, _: TransactionId) -> bool {
        false
    }
    fn in_progress(&self, x: TransactionId) -> PgResult<bool> {
        Ok(x == 900)
    }
    fn committed(&self, _: TransactionId) -> PgResult<bool> {
        Ok(true)
    }
    fn horizon(&self) -> PgResult<TransactionId> {
        Ok(1_000)
    }
    fn oldest_clog(&self) -> TransactionId {
        3
    }
}

const XMIN_FROZEN: u16 = 0x0300;
const XMAX_INVALID: u16 = 0x0800;
const XMAX_IS_MULTI: u16 = 0x1000;
const XMAX_LOCK_ONLY: u16 = 0x0080;
const XMAX_KEYSHR: u16 = 0x0010;
const XMAX_EXCL: u16 = 0x0040;
const UPDATED: u16 = 0x2000;

fn tuple(xmin: u32, xmax: u32, infomask: u16, r: u64, succ: u64) -> [u8; 40] {
    let mut t = [0u8; 40];
    t[0..4].copy_from_slice(&xmin.to_ne_bytes());
    t[4..8].copy_from_slice(&xmax.to_ne_bytes());
    t[18..20].copy_from_slice(&2u16.to_ne_bytes());
    t[20..22].copy_from_slice(&infomask.to_ne_bytes());
    t[22] = 24;
    t[24..32].copy_from_slice(&(r as i64).to_ne_bytes());
    t[32..40].copy_from_slice(&(succ as i64).to_ne_bytes());
    t
}

#[test]
fn lock_rows_never_reach_the_tombstone_image() {
    let mut buf = vec![0u64; BLCKSZ / 8].into_boxed_slice();
    // SAFETY: an 8-aligned BLCKSZ buffer this test owns.
    let mut page = unsafe { PageMut::from_raw(std::ptr::NonNull::new(buf.as_mut_ptr() as *mut u8).unwrap()) };
    page.init(0);
    let rows = [
        // a settled delete
        tuple(5, 0, XMIN_FROZEN | XMAX_INVALID, rowident(7, 1), SUCC_NONE),
        // a lock-only LOCK row, frozen, two KEY SHARE lockers (a multixact)
        tuple(900, 77, XMIN_FROZEN | XMAX_IS_MULTI | XMAX_LOCK_ONLY | XMAX_KEYSHR, rowident(7, 2), SUCC_LOCK),
        // a LOCK row a writer is updating while a KEY SHARE holder stays (a multixact holding an update)
        tuple(900, 78, XMIN_FROZEN | XMAX_IS_MULTI | XMAX_KEYSHR, rowident(7, 3), SUCC_LOCK),
        // a LOCK row an exclusive locker holds
        tuple(900, 900, XMIN_FROZEN | XMAX_LOCK_ONLY | XMAX_EXCL, rowident(7, 4), SUCC_LOCK),
        // the tombstone that update made: its KEY SHARE holder carried as a locked-only multixact xmax
        tuple(900, 79, UPDATED | XMAX_IS_MULTI | XMAX_LOCK_ONLY | XMAX_KEYSHR, rowident(7, 3), rowident(9, 1 << 9 | 4)),
    ];
    for t in &rows {
        page.add_item(t, 0, PAI_IS_HEAP).expect("room");
    }
    // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, BLCKSZ) };
    let img = BlockImage::decode(0, bytes, Kind::Tomb, &Ctx::new(&Committed)).expect("decode").expect("imageable with LOCK rows");
    let got: Vec<(u64, u64)> = (0..img.rows()).map(|i| pgrc_colcache::tomb::tombstone(&img, i).map(|t| (t.rowident, t.succ)).unwrap()).collect();
    assert_eq!(got, vec![(rowident(7, 1), SUCC_NONE), (rowident(7, 3), rowident(9, 1 << 9 | 4))], "the two tombstones, no LOCK row");
    assert!(img.is_settled(0), "the frozen delete is settled");
    assert!(!img.is_settled(1), "the in-progress updater's tombstone is recent");
}
