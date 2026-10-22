//! The settled keying of the tombstone image keys by `Generation::view_key`, never by the number alone.

use std::cell::RefCell;
use std::sync::Arc;

use pgrc_am::catalog::{self, Generation, Lookup};
use pgrc_colcache::image::Images;
use pgrc_colcache::tomb::{settled, GenKeyer};
use pgrc_colcache::{Kind, PageSource, TxnOracle};
use pgrc_format::manifest::{Manifest, ManifestHeader};
use pgrc_format::rowid::SUCC_NONE;
use pgrc_marks::Registry;
use pgrc_read::CommitCheck;
use pgrc_write::stats::StatsState;
use types_core::{TransactionId, BLCKSZ};
use types_error::PgResult;
use types_storage::bufpage::{PageMut, PAI_IS_HEAP};

const SLOT: catalog::RelSlot = (1663, 5, 91_301);
const DIR: &str = "t/view-key-91301";

fn publish(gen: u64, prev: u64, fxid: u64) {
    let header = ManifestHeader { gen, prev_gen: prev, publisher_fxid: fxid, relfilenumber: SLOT.2, schema_fingerprint: 1, magic: 0, format_version: 0, spc: SLOT.0, db: SLOT.1, part_count: 0, next_part_no: 1, flags: 0, reserved: 0 };
    // P-1: nothing read or written; a part-less generation's (empty) stats cover its manifest, so no fold runs.
    catalog::publish(SLOT, DIR, Manifest::new(header, Vec::new()), StatsState::default(), &|_| unreachable!("no parts to fold")).expect("publish");
}

struct Snap {
    own: Vec<u64>,
    seen: Vec<u64>,
}

impl CommitCheck for Snap {
    fn committed(&self, fxid: u64) -> bool {
        self.own.contains(&fxid) || self.seen.contains(&fxid)
    }
    fn own(&self, fxid: u64) -> bool {
        self.own.contains(&fxid)
    }
}

fn view(own: &[u64], seen: &[u64]) -> Arc<Generation> {
    match catalog::visible(SLOT, &Snap { own: own.to_vec(), seen: seen.to_vec() }).expect("visible") {
        Lookup::Gen(g) => g,
        _ => panic!("a generation"),
    }
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
            t[24..32].copy_from_slice(&((291 * 7000 + u64::from(pos)) as i64).to_ne_bytes());
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
fn a_composed_view_and_its_generation_do_not_share_the_settled_keying() {
    catalog::create(SLOT, DIR);
    publish(1, 0, 10);
    publish(2, 1, 11);
    publish(3, 2, 20);
    let whole = view(&[], &[10, 11, 20]);
    let composed = view(&[20], &[10]);
    assert!(!whole.composed() && composed.composed() && whole.gen == 3 && composed.gen == 3, "one number, two views");

    let (reg, imgs) = (Registry::new(), Images::new());
    let tomb = imgs.current(&reg, (1663, 5, 91_302), 91_302, Kind::Tomb, &TombPage::new(5), &Committed).expect("current").expect("an image");
    let a = settled(&tomb, &mut GenKeyer::new(&whole, DIR.into(), Default::default())).expect("settled");
    let b = settled(&tomb, &mut GenKeyer::new(&composed, DIR.into(), Default::default())).expect("settled");
    assert_eq!((a.count, b.count), (5, 5));
    assert!(!Arc::ptr_eq(&a, &b), "the composed view read the whole generation's settled keying");
    assert_ne!(a.gen, b.gen, "keyed by view_key");
    assert!(Arc::ptr_eq(&b, &settled(&tomb, &mut GenKeyer::new(&composed, DIR.into(), Default::default())).expect("settled")), "and each view keeps its own");
    catalog::remove_dir(DIR);
}
