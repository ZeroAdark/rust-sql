//! One test: the cap and the ring are process-wide.

use std::cell::RefCell;
use std::sync::Arc;

use pgrc_colcache::image::Images;
use pgrc_colcache::tomb::{forget_settled, settled};
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
const LOC: pgrc_marks::Locator = (1663, 5, 91_401);
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

#[test]
fn a_settled_keying_is_cached_only_under_the_cap() {
    let (reg, imgs) = (Registry::new(), Images::new());
    let before = mem::stats();
    let tomb = imgs.current(&reg, LOC, LOC.2, Kind::Tomb, &TombPage::new(TOMBS), &Committed).expect("current").expect("an image");
    let s0 = mem::stats();
    assert!(s0.cached >= tomb.bytes as u64, "the image is published");

    // Full (cached + held = cap), and the image pinned by this test: no room can be made.
    mem::set_cap(s0.cached + s0.held);
    let a = settled(&tomb, &mut nogen()).expect("settled");
    let s1 = mem::stats();
    assert_eq!(a.count, TOMBS as usize);
    assert!(s1.cached <= s1.cap, "the settled keying took the cached class over the cap: cached {} cap {}", s1.cached, s1.cap);
    assert!(s1.over_cap > s0.over_cap, "the keying's admission was refused");
    let b = settled(&tomb, &mut nogen()).expect("settled");
    assert!(!Arc::ptr_eq(&a, &b), "over the cap each call keys privately");
    assert_eq!(mem::stats().held, s0.held + 2 * 4 * u64::from(TOMBS), "an unadmitted keying stays held");
    drop((a, b));
    assert_eq!(mem::stats().held, s0.held, "and is credited when its caller drops it");

    // With room: admitted, cached and shared.
    mem::set_cap(mem::DEFAULT_CAP);
    let c = settled(&tomb, &mut nogen()).expect("settled");
    assert!(Arc::ptr_eq(&c, &settled(&tomb, &mut nogen()).expect("settled")), "a cached keying is shared");
    assert_eq!(mem::stats().cached, s0.cached + 4 * u64::from(TOMBS));
    drop(c);
    forget_settled(LOC);
    drop(tomb);
    imgs.drop_image(&reg, LOC);
    mem::sweep_all_for_tests();
    let s2 = mem::stats();
    assert_eq!((s2.cached, s2.held, s2.slots), (before.cached, before.held, before.slots), "every byte and slot is gone");
}
