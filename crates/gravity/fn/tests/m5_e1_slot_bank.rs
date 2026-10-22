//! M5 E1 (M5-plan §9 Wave 1a row 1a-5; M5-spi-reentrancy.md §2.3 H8b): the SRF slot bank is per re-entry level. A
//! materialised library set (`string_to_table`) keeps its rows in its slot's bump context from `open` to its last `step`;
//! a statement nested inside that open (a SQL / PL body, the next re-entry level: exec reentry.rs `enter` wraps each
//! served statement in `srf::at_level`) numbers its own slots from 0. With one bank per thread its open reset the outer
//! open's context while the outer still read its rows (a use-after-reset: the ladder's eb88d4d5f99, never on trunk).
mod common;

use datum::Datum;
use gravity_abi::Ctx;
use gravity_fn::srf::{at_level, entry_of, Entry, OUT_WORDS, STATE_BYTES, STATE_SLOT};
use gravity_fn::ErrSlot;

const DEFAULT_COLLATION: i64 = 100;
const STRING_TO_TABLE: u32 = 6160;

/// One open set: its state cell (slot 0) and the ctx whose arena the rows are copied into.
struct Open {
    e: &'static Entry,
    state: Vec<u8>,
    ctx: Box<Ctx>,
    _arena: Vec<u8>,
    _args: Vec<Vec<u8>>,
}

impl Open {
    /// `string_to_table(text, delim)` opened in slot 0 of the current level's bank.
    fn new(text: &str, delim: &str) -> Open {
        let e = entry_of(STRING_TO_TABLE).expect("string_to_table is a library entry");
        // SAFETY: the ctx block is plain data; the arena fields are set below.
        let mut ctx: Box<Ctx> = Box::new(unsafe { core::mem::zeroed() });
        let mut arena = vec![0u8; 1 << 20];
        ctx.arena_cur = arena.as_mut_ptr();
        // SAFETY: one past the end of the arena.
        ctx.arena_end = unsafe { arena.as_mut_ptr().add(arena.len()) };
        let args = vec![common::image(text.as_bytes()), common::image(delim.as_bytes())];
        let av: Vec<i64> = args.iter().map(|a| common::image_datum(a).as_i64()).collect();
        let an = vec![0u8; av.len()];
        let mut state = vec![0u8; STATE_BYTES];
        state[STATE_SLOT..STATE_SLOT + 8].copy_from_slice(&0u64.to_ne_bytes());
        let mut err = ErrSlot::NONE;
        // SAFETY: the library's contract: a bound ctx, the state cell and argument buffers alive for the call.
        let st = unsafe { (e.open)(&mut err, &mut *ctx, state.as_mut_ptr(), av.as_ptr(), an.as_ptr(), av.len() as i64, DEFAULT_COLLATION) };
        assert!(st >= 0, "open: {:?}", err.take().map(|e| e.message().to_owned()));
        Open { e, state, ctx, _arena: arena, _args: args }
    }

    /// The next row's text, `None` when the set is done.
    fn step(&mut self) -> Option<String> {
        let mut out = [0i64; OUT_WORDS];
        let mut err = ErrSlot::NONE;
        // SAFETY: as `new`; the state is this open's.
        let r = unsafe { (self.e.step)(&mut err, &mut *self.ctx, self.state.as_mut_ptr(), out.as_mut_ptr()) };
        assert!(r >= 0, "step: {:?}", err.take().map(|e| e.message().to_owned()));
        if r == 0 {
            return None;
        }
        assert_eq!(out[1], 0, "a non-null row");
        // SAFETY: a varlena datum of the row, in the ctx arena.
        Some(String::from_utf8(unsafe { payload(Datum::from_i64(out[0]).as_usize() as *const u8) }).expect("utf8"))
    }
}

unsafe fn payload(p: *const u8) -> Vec<u8> {
    if *p & 1 == 1 {
        let len = (*p >> 1) as usize;
        std::slice::from_raw_parts(p.add(1), len - 1).to_vec()
    } else {
        let n = (p.cast::<u32>().read_unaligned() >> 2) as usize;
        std::slice::from_raw_parts(p.add(4), n - 4).to_vec()
    }
}

/// One test: the default locale the seams set is per thread (srf_differential.rs's precedent).
#[test]
fn slot_bank_per_level() {
    common::install_seams();
    a_nested_levels_open_leaves_the_outer_slot_intact();
    one_level_reopens_its_slot();
}

/// Level 0 opens a set in slot 0 and reads one row; a statement at level 1 opens and drains its own set in its slot 0;
/// level 0's remaining rows are intact.
fn a_nested_levels_open_leaves_the_outer_slot_intact() {
    at_level(0, || {
        let mut outer = Open::new("alpha beta gamma delta", " ");
        assert_eq!(outer.step().as_deref(), Some("alpha"));
        let inner: Vec<String> = at_level(1, || {
            let mut inner = Open::new("XXXX,YYYYY,ZZZZZZ,WWWWWWW,VVVVVVVV", ",");
            core::iter::from_fn(|| inner.step()).collect()
        });
        assert_eq!(inner, ["XXXX", "YYYYY", "ZZZZZZ", "WWWWWWW", "VVVVVVVV"]);
        let rest: Vec<String> = core::iter::from_fn(|| outer.step()).collect();
        assert_eq!(rest, ["beta", "gamma", "delta"], "level 0's rows after level 1's open");
    });
}

/// Does not apply (G4): at one level a second open of the slot starts that slot's set afresh (the reset is kept).
fn one_level_reopens_its_slot() {
    at_level(3, || {
        let mut a = Open::new("p q", " ");
        assert_eq!(a.step().as_deref(), Some("p"));
        drop(a);
        let mut b = Open::new("r s", " ");
        let all: Vec<String> = core::iter::from_fn(|| b.step()).collect();
        assert_eq!(all, ["r", "s"]);
    });
}
