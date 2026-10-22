use std::cell::RefCell;

use pgrc_colcache::image::Images;
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_format::rowid::{SUCC_LOCK, SUCC_NONE};
use pgrc_marks::Registry;
use pgrc_read::mem;
use types_core::{TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

/// An opaque row ID for the test's tombstone keys (IX0: IDs are 40-bit; these stay below 2^40).
fn rowident(part_no: u32, pos: u32) -> u64 {
    u64::from(part_no) << 32 | u64::from(pos)
}

const XMIN_FROZEN: u16 = 0x0300;
const XMIN_INVALID: u16 = 0x0200;
const XMAX_INVALID: u16 = 0x0800;
const XMAX_LOCK_ONLY: u16 = 0x0080;
const XMAX_EXCL: u16 = 0x0040;

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

struct Page(RefCell<Box<[u64]>>);

impl Page {
    fn new(rows: &[[u8; 40]]) -> Page {
        let p = Page(RefCell::new(vec![0u64; BLCKSZ / 8].into_boxed_slice()));
        // SAFETY: an 8-aligned BLCKSZ buffer this page owns.
        let mut page = unsafe { PageMut::from_raw(std::ptr::NonNull::new(p.0.borrow_mut().as_mut_ptr() as *mut u8).unwrap()) };
        page.init(0);
        for t in rows {
            page.add_item(t, 0, PAI_IS_HEAP).expect("room");
        }
        p
    }
}

impl PageSource for Page {
    fn nblocks(&self) -> PgResult<u32> {
        Ok(1)
    }
    fn read(&self, _: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let p = self.0.borrow();
        // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
        f(unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) })
    }
}

struct Txns;

impl TxnOracle for Txns {
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

#[test]
fn a_tomb_image_counts_its_live_rows_and_never_a_lock_row() {
    mem::set_cap(mem::DEFAULT_CAP);
    let locks = [
        // a lock held by a running transaction, and a committed lock-only one
        tuple(900, 900, XMIN_FROZEN | XMAX_LOCK_ONLY | XMAX_EXCL, rowident(7, 1), SUCC_LOCK),
        tuple(5, 6, XMIN_FROZEN | XMAX_LOCK_ONLY | XMAX_EXCL, rowident(7, 2), SUCC_LOCK),
    ];
    let cases: [(&str, Vec<[u8; 40]>, usize); 4] = [
        ("LOCK rows only", locks.to_vec(), 0),
        // a speculative tombstone that lost: its xmin invalid, dead to all
        ("a dead loser", [locks.to_vec(), vec![tuple(77, 0, XMIN_INVALID | XMAX_INVALID, rowident(7, 3), SUCC_NONE)]].concat(), 0),
        ("a settled delete", [locks.to_vec(), vec![tuple(5, 0, XMIN_FROZEN | XMAX_INVALID, rowident(7, 4), SUCC_NONE)]].concat(), 1),
        ("an in-progress delete", [locks.to_vec(), vec![tuple(900, 0, XMAX_INVALID, rowident(7, 5), SUCC_NONE)]].concat(), 1),
    ];
    for (i, (what, rows, live)) in cases.into_iter().enumerate() {
        let (reg, imgs) = (Registry::new(), Images::new());
        let loc = (1663, 5, 91_500 + i as u32);
        let img = imgs.current(&reg, loc, loc.2, Kind::Tomb, &Page::new(&rows), &Txns).expect("current").expect("an image");
        assert_eq!(img.live, live, "{what}");
        imgs.drop_image(&reg, loc);
    }
}
