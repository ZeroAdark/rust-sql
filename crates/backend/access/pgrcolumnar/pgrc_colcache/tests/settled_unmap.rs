//! One test: the cap and the SETTLED cache are process-wide.

use std::cell::RefCell;
use std::sync::Arc;

use pgrc_colcache::image::{Images, NoImage};
use pgrc_colcache::tomb::settled;
use pgrc_colcache::view::NoGen;
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_am::rowid_alloc::Extents;
use pgrc_format::rowid::{SegExtent, SUCC_NONE};
use pgrc_marks::Registry;
use pgrc_read::mem;
use types_core::{TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

const TOMBS: u32 = 150;
const LOC: pgrc_marks::Locator = (1663, 5, 91_411);
const SEG7: u64 = 291 * 7000;

fn nogen() -> NoGen {
    NoGen(Arc::new(Extents::new(vec![SegExtent { part_no: 7, idx: 0, base: SEG7 }])))
}

struct TombPage(RefCell<Box<[u64]>>);

impl TombPage {
    fn new(n: u32) -> TombPage {
        let p = TombPage(RefCell::new(vec![0u64; BLCKSZ / 8].into_boxed_slice()));
        // SAFETY: an 8-aligned BLCKSZ buffer this page owns.
        let mut page = unsafe { PageMut::from_raw(std::ptr::NonNull::new(p.0.borrow_mut().as_mut_ptr() as *mut u8).unwrap()) };
        page.init(0);
        for pos in 0..n {
            let mut t = [0u8; 40];
            t[0..4].copy_from_slice(&3u32.to_ne_bytes());
            t[18..20].copy_from_slice(&2u16.to_ne_bytes());
            t[20..22].copy_from_slice(&(0x0100u16 | 0x0200 | 0x0800).to_ne_bytes());
            t[22] = 24;
            t[24..32].copy_from_slice(&((SEG7 + u64::from(pos)) as i64).to_ne_bytes());
            t[32..40].copy_from_slice(&(SUCC_NONE as i64).to_ne_bytes());
            page.add_item(&t, 0, PAI_IS_HEAP).expect("room");
        }
        p
    }
}

impl PageSource for TombPage {
    fn nblocks(&self) -> PgResult<u32> {
        Ok(1)
    }
    fn read(&self, _: u32, f: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        let p = self.0.borrow();
        // SAFETY: BLCKSZ bytes of an 8-aligned buffer.
        f(unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, BLCKSZ) })
    }
}

struct Committed;

impl TxnOracle for Committed {
    fn is_current(&self, _: TransactionId) -> bool {
        false
    }
    fn in_progress(&self, _: TransactionId) -> PgResult<bool> {
        Ok(false)
    }
    fn committed(&self, _: TransactionId) -> PgResult<bool> {
        Ok(true)
    }
    fn horizon(&self) -> PgResult<TransactionId> {
        Ok(1_000_000)
    }
    fn oldest_clog(&self) -> TransactionId {
        3
    }
}

/// A tombstone heap past the page bound (never read: the bound is checked first).
struct Big;

impl PageSource for Big {
    fn nblocks(&self) -> PgResult<u32> {
        Ok(1 << 20)
    }
    fn read(&self, _: u32, _: &mut dyn FnMut(&[u8]) -> PgResult<()>) -> PgResult<()> {
        unreachable!("past the page bound nothing is read")
    }
}

#[test]
fn an_unmapped_tomb_image_takes_its_settled_keyings() {
    let (reg, imgs) = (Registry::new(), Images::new());
    let keying = 4 * u64::from(TOMBS);
    let before = mem::stats();
    let tomb = imgs.current(&reg, LOC, LOC.2, Kind::Tomb, &TombPage::new(TOMBS), &Committed).expect("current").expect("an image");
    let a = settled(&tomb, &mut nogen()).expect("settled");
    assert_eq!(a.count, TOMBS as usize);
    assert_eq!(mem::stats().cached, before.cached + tomb.bytes as u64 + keying, "the image and its keying are cached");
    drop(a);

    // A write makes the image stale, and the next view finds the heap past the page bound: unmapped, not evicted.
    reg.lookup(LOC).expect("marks").written(0);
    let over = imgs.current(&reg, LOC, LOC.2, Kind::Tomb, &Big, &Committed).expect("current");
    assert_eq!(over.err(), Some(NoImage::OverCap));
    assert!(imgs.get(LOC).is_none(), "unmapped");
    let s1 = mem::stats();
    assert_eq!(s1.cached, before.cached, "the unmapped image's settled keying left the cached class with it");

    // A view still holding the unmapped image keys it privately.
    let b = settled(&tomb, &mut nogen()).expect("settled");
    assert_eq!(mem::stats().cached, before.cached, "the keying of an unmapped image is not cached");
    assert!(!Arc::ptr_eq(&b, &settled(&tomb, &mut nogen()).expect("settled")), "each call keys privately");
    drop((b, tomb));
    imgs.drop_image(&reg, LOC);
    let s2 = mem::stats();
    assert_eq!((s2.cached, s2.held), (before.cached, before.held), "every byte is gone");
}
