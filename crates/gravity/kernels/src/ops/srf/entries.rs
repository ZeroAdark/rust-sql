//! M5's host entries (M5-plan.md §6, cards/M5.md L1 / L2), each under `guard!` (V4R-16). Created by M5-RIG (owner M5-HOSTS).
//! H-a: `gravity_fn::srf`'s library entries (`ENTRIES`) and the array face over one cell per open, alive across a suspend
//! (plan §6). The cell: the library's state at `CELL_STATE` (word 0 the slot whose context the open allocates in), the
//! row at `CELL_OUT` (`OUT_COLS` x (datum, null) words; a by-reference value points into the slot's context, valid until
//! its next open), the arguments at `CELL_ARGS` (`nargs` words, then `nargs` null bytes). A failure stashes PG's error
//! and returns `SRF_FAILED` (the raise block's `HOST` bit). None polls cancel: the reader's set loop does (plan §3.10).
use gravity_abi::status::raise;
use gravity_fn::srf::{Entry, ENTRIES, STATE_BYTES};
use gravity_fn::{ErrSlot, PgError, OK};

pub const CELL_STATE: usize = 0;
pub const CELL_OUT: usize = CELL_STATE + STATE_BYTES;
/// The library's widest row (`jsonb_each` / `json_each`: key, value).
pub const OUT_COLS: usize = 2;
pub const CELL_ARGS: usize = CELL_OUT + OUT_COLS * 16;
pub const SRF_FAILED: u64 = raise::bit(raise::HOST);
/// The array face's facts word (`srf_array_open`): `elmlen as u16 | byval << FACT_BYVAL | align << FACT_ALIGN`.
pub const FACT_BYVAL: u32 = 16;
pub const FACT_ALIGN: u32 = 24;

/// `ProtoKind::SetFn` (exec v4/m5.rs; one per srf step): `arg` = `setfn_arg(..)`, `SETFN_WORDS` words from `words.base`,
/// filled per execution: the first of the step's slots reserved for it (slot k's state word 0 = base + k), the store's
/// handle (0 without one), the step's cells (`cell_bytes`, zeroed, 64-aligned, alive until the statement's end).
pub const SETFN_WORDS: u16 = 3;
pub const SETFN_W_BASE: usize = 0;
pub const SETFN_W_NODE: usize = 1;
pub const SETFN_W_CELLS: usize = 2;

/// `nslots` (16 bits), the cells' bytes (24 bits) and the incumbent's plan node id + 1 (23 bits, 0 = none); bit 63 is
/// `SETFN_PRIVATE`.
pub const fn setfn_arg(nslots: u16, cell_bytes: u32, node: Option<u32>) -> u64 {
    let node = match node {
        // never truncated: a plan node id past the field is a bug, raised here (the test `a_node_past_23_bits_panics`)
        Some(id) => {
            assert!(id <= SETFN_NODE_MAX, "setfn_arg: a plan node id past setfn_arg's 23-bit node field");
            id as u64 + 1
        }
        None => 0,
    };
    nslots as u64 | ((cell_bytes as u64) & 0xff_ffff) << 16 | node << 40
}

/// `setfn_arg`'s parts: (nslots, cell bytes, plan node id).
pub const fn setfn_parts(arg: u64) -> (usize, usize, Option<u32>) {
    let node = (arg >> 40) as u32 & 0x7f_ffff;
    (arg as u16 as usize, (arg >> 16) as usize & 0xff_ffff, if node == 0 { None } else { Some(node - 1) })
}

/// The largest plan node id `setfn_arg` holds (its node field is 23 bits, id + 1).
pub const SETFN_NODE_MAX: u32 = 0x7f_fffe;

/// M5 Decision 7 (V4R-258): OR'd into a `setfn_arg` whose program must stay its session's (a FmgrSet cell's).
pub const SETFN_PRIVATE: u64 = 1 << 63;

/// Whether a program declaring this SetFn `arg` stays its session's (exec admit.rs `shareable`, Decision 7): it names the
/// incumbent node (form (c), srf.pl, srf.tablefunc, the Named reader) or carries `SETFN_PRIVATE`.
pub const fn setfn_private(arg: u64) -> bool {
    arg & SETFN_PRIVATE != 0 || setfn_parts(arg).2.is_some()
}

pub const fn cell_nulls(nargs: usize) -> usize {
    CELL_ARGS + nargs * 8
}

pub const fn cell_bytes(nargs: usize) -> usize {
    (cell_nulls(nargs) + nargs).next_multiple_of(8)
}

/// The entries themselves and the store head (their `unsafe`, rule R6) are srf/raw.rs's, re-exported here under
/// their paths.
pub use super::raw::{fmgr_open, fmgr_step, srf_array_open, srf_open, srf_step, srf_unnest_step, store_batch, store_param, store_rewind, StoreHead};

pub const STORE_MAGIC: u64 = 0x5e7f_5707_e0a1_ab1e;
/// `store_batch`: the store is read to its end (no window).
pub const STORE_END: u64 = 0;
/// `store_batch` / `store_rewind` failed, PG's error stashed (a window is 8-aligned, never 1).
pub const STORE_FAILED: u64 = 1;
/// D1874: `store_param`'s id for the value of SubPlan `plan_id` in a FROM call is `STORE_FEED + plan_id`: the host's state
/// reads it as the exec param standing in for that SubPlan (hosts srf/store.rs `bare`, past the plan's own exec params).
pub const STORE_FEED: u32 = 1 << 24;
/// A window (host-owned per reader, reused for its every batch; valid until that reader's next batch or rewind): the
/// row count at `WIN_N`, column c row j's datum at `WIN_VALUES + (c * max + j) * 8` (a by-reference one points at its
/// copy in the reader's batch region), then the validity bitmaps: column c's word j / 64 at
/// `win_valid(natts, max) + (c * max / 64 + j / 64) * 8`, bit j % 64 set = NOT NULL. `max` is a multiple of 64.
pub const WIN_N: usize = 0;
pub const WIN_VALUES: usize = 8;

pub const fn win_valid(natts: usize, max: usize) -> usize {
    WIN_VALUES + natts * max * 8
}

pub const fn win_bytes(natts: usize, max: usize) -> usize {
    win_valid(natts, max) + natts * max / 8
}

pub(super) fn done(st: i64, e: &mut ErrSlot) -> u64 {
    if st == OK {
        1
    } else {
        settle(e)
    }
}

pub(super) fn entry_at(entry: u64) -> Option<&'static Entry> {
    ENTRIES.get(usize::try_from(entry).ok()?)
}

/// The library's error from the slot, stashed for the entry's raise (a status with an empty slot is a bug's error).
pub(super) fn settle(e: &mut ErrSlot) -> u64 {
    let err = e.take().unwrap_or_else(|| Box::new(PgError::error("gravity: a library SRF failed with no error"))); // alloc: Cold
    gravity_fn::stash_error(err);
    SRF_FAILED
}

#[cold]
#[inline(never)]
pub(super) fn no_entry(entry: u64) -> u64 {
    bug(&format!("gravity: no library SRF entry {entry}")) // alloc: Cold
}

#[cold]
#[inline(never)]
pub(super) fn bug(msg: &str) -> u64 {
    gravity_fn::stash_error(Box::new(PgError::error(msg))); // alloc: Cold
    SRF_FAILED
}

#[cold]
#[inline(never)]
pub(super) fn stale() -> u64 {
    bug("gravity: a store reader over no live store");
    STORE_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::Ctx;
    use gravity_fn::srf::STATE_SLOT;

    const TIMESTAMP_SERIES: u32 = 938;
    const JSON_ELEMENTS_TEXT: u32 = 3969;
    const DAY: i64 = 86_400_000_000;

    fn index(oid: u32) -> u64 {
        ENTRIES.iter().position(|e| e.oid == oid).expect("a library entry") as u64
    }

    /// A bound ctx over a 1 MB arena, and a cell of `nargs` arguments in slot `slot`.
    struct Call {
        ctx: Box<Ctx>,
        cell: Vec<u64>,
        _arena: Vec<u8>,
    }

    impl Call {
        fn new(slot: u64, args: &[Option<i64>]) -> Call {
            // SAFETY: the ctx block is plain data; only the arena words are read.
            let mut ctx: Box<Ctx> = Box::new(unsafe { core::mem::zeroed() }); // alloc: Cold
            let mut arena = vec![0u8; 1 << 20]; // alloc: Cold
            ctx.arena_cur = arena.as_mut_ptr();
            ctx.arena_end = arena.as_mut_ptr().wrapping_add(arena.len());
            let mut cell = vec![0u64; cell_bytes(args.len()) / 8]; // alloc: Cold
            let c = cell.as_mut_ptr() as *mut u8;
            // SAFETY: every offset is inside the cell (`cell_bytes`).
            unsafe {
                c.add(CELL_STATE + STATE_SLOT).cast::<u64>().write(slot);
                for (i, a) in args.iter().enumerate() {
                    c.add(CELL_ARGS + 8 * i).cast::<i64>().write(a.unwrap_or(0));
                    c.add(cell_nulls(args.len()) + i).write(a.is_none() as u8);
                }
            }
            Call { ctx, cell, _arena: arena }
        }

        fn ctx(&mut self) -> u64 {
            &mut *self.ctx as *mut Ctx as u64
        }

        fn cell(&mut self) -> u64 {
            self.cell.as_mut_ptr() as u64
        }

        fn out(&self, k: usize) -> (i64, bool) {
            let w = &self.cell[CELL_OUT / 8 + 2 * k..];
            (w[0] as i64, w[1] != 0)
        }

        /// Every row of the opened entry's column 0 (`None` = NULL), or the stashed error's message.
        fn drain(&mut self, entry: u64, mut value: impl FnMut(i64) -> String) -> Result<Vec<Option<String>>, String> {
            let mut rows = Vec::new(); // alloc: Cold
            loop {
                match unsafe { srf_step(self.ctx(), self.cell(), entry) } {
                    0 => return Ok(rows),
                    1 => rows.push(match self.out(0) {
                        (_, true) => None,
                        (d, false) => Some(value(d)),
                    }),
                    SRF_FAILED => return Err(failed()),
                    r => panic!("srf_step returned {r}"),
                }
            }
        }
    }

    fn failed() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    /// A 4-byte-header varlena image of `bytes`, 8-aligned, and its datum.
    fn text(keep: &mut Vec<Vec<u64>>, bytes: &[u8]) -> i64 {
        let n = bytes.len() + 4;
        let mut w = vec![0u64; n.div_ceil(8)]; // alloc: Cold
        let p = w.as_mut_ptr() as *mut u8;
        // SAFETY: `w` holds `n` bytes.
        unsafe {
            p.cast::<u32>().write_unaligned((n as u32) << 2);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(4), bytes.len());
        }
        keep.push(w);
        keep.last().unwrap().as_ptr() as i64
    }

    /// A varlena datum's payload as text.
    fn payload(d: i64) -> String {
        let p = d as usize as *const u8;
        // SAFETY: a 4-byte-header varlena the step wrote.
        unsafe {
            let n = (p.cast::<u32>().read_unaligned() >> 2) as usize;
            String::from_utf8(std::slice::from_raw_parts(p.add(4), n - 4).to_vec()).unwrap()
        }
    }

    /// A one-dimensional array image (no NULL bitmap unless `nulls`), lower bound 1: `elems` are the element bytes
    /// laid at 4-byte alignment (typalign 'i').
    fn array(keep: &mut Vec<Vec<u64>>, elemtype: u32, elems: &[Option<Vec<u8>>]) -> i64 {
        let has_nulls = elems.iter().any(Option::is_none);
        let bitmap = if has_nulls { elems.len().div_ceil(8) } else { 0 };
        let data_off = (24 + bitmap).next_multiple_of(8);
        let mut img = vec![0u8; data_off]; // alloc: Cold
        img[4..8].copy_from_slice(&1i32.to_ne_bytes());
        img[8..12].copy_from_slice(&(if has_nulls { data_off as i32 } else { 0 }).to_ne_bytes());
        img[12..16].copy_from_slice(&elemtype.to_ne_bytes());
        img[16..20].copy_from_slice(&(elems.len() as i32).to_ne_bytes());
        img[20..24].copy_from_slice(&1i32.to_ne_bytes());
        for (i, e) in elems.iter().enumerate() {
            if let Some(b) = e {
                if has_nulls {
                    img[24 + i / 8] |= 1 << (i % 8);
                }
                img.extend_from_slice(b);
                img.resize(img.len().next_multiple_of(4), 0);
            }
        }
        let n = img.len();
        img[0..4].copy_from_slice(&((n as u32) << 2).to_ne_bytes());
        let mut w = vec![0u64; n.div_ceil(8)]; // alloc: Cold
        // SAFETY: `w` holds `n` bytes.
        unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), w.as_mut_ptr() as *mut u8, n) };
        keep.push(w);
        keep.last().unwrap().as_ptr() as i64
    }

    fn interval(keep: &mut Vec<Vec<u64>>, time: i64, day: i32, month: i32) -> i64 {
        keep.push(vec![time as u64, (day as u32 as u64) | (month as u32 as u64) << 32]); // alloc: Cold
        keep.last().unwrap().as_ptr() as i64
    }

    #[test]
    fn the_cell_lays_state_out_args_then_nulls() {
        assert_eq!((CELL_STATE, CELL_OUT, CELL_ARGS), (0, 512, 544));
        assert_eq!((cell_nulls(3), cell_bytes(0), cell_bytes(3)), (568, 544, 576));
        assert_eq!(SRF_FAILED, 128);
        assert_eq!(setfn_parts(setfn_arg(3, 1728, Some(7))), (3, 1728, Some(7)));
        assert_eq!(setfn_parts(setfn_arg(0, 0, None)), (0, 0, None));
        assert_eq!(setfn_parts(setfn_arg(u16::MAX, 0xff_ffff, Some(0))), (u16::MAX as usize, 0xff_ffff, Some(0)));
        assert_eq!(setfn_parts(setfn_arg(2, 64, None) | SETFN_PRIVATE), (2, 64, None));
        assert_eq!(setfn_parts(setfn_arg(0, 0, Some(SETFN_NODE_MAX))), (0, 0, Some(SETFN_NODE_MAX)));
        assert_eq!(setfn_parts(setfn_arg(0, 0, Some(SETFN_NODE_MAX)) | SETFN_PRIVATE), (0, 0, Some(SETFN_NODE_MAX)));
        assert!(setfn_private(setfn_arg(0, 0, Some(0))) && setfn_private(setfn_arg(1, 576, None) | SETFN_PRIVATE));
        assert!(!setfn_private(setfn_arg(1, 576, None)) && !setfn_private(setfn_arg(0, 0, None)));
    }

    #[test]
    #[should_panic(expected = "23-bit node field")]
    fn a_node_past_23_bits_panics() {
        let _ = setfn_arg(0, 0, Some(SETFN_NODE_MAX + 1));
    }

    #[test]
    fn a_timestamp_series_steps_every_row_then_ends() {
        let mut keep = Vec::new(); // alloc: Cold
        let (e, day) = (index(TIMESTAMP_SERIES), interval(&mut keep, 0, 1, 0));
        let mut c = Call::new(0, &[Some(0), Some(2 * DAY), Some(day)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 3, 0) }, 1);
        let rows = c.drain(e, |d| d.to_string()).unwrap(); // alloc: Cold
        assert_eq!(rows, ["0", "86400000000", "172800000000"].map(|s| Some(s.to_owned())));
        assert_eq!(unsafe { srf_step(c.ctx(), c.cell(), e) }, 0, "an ended set stays ended");
    }

    #[test]
    fn a_strict_entry_with_a_null_argument_opens_nothing() {
        let mut keep = Vec::new(); // alloc: Cold
        let (e, day) = (index(TIMESTAMP_SERIES), interval(&mut keep, 0, 1, 0));
        let mut c = Call::new(0, &[Some(0), None, Some(day)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 3, 0) }, 0);
    }

    #[test]
    fn an_open_error_is_stashed_and_raises_host() {
        let mut keep = Vec::new(); // alloc: Cold
        let (e, zero) = (index(TIMESTAMP_SERIES), interval(&mut keep, 0, 0, 0));
        let mut c = Call::new(0, &[Some(0), Some(DAY), Some(zero)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 3, 0) }, SRF_FAILED);
        assert_eq!(failed(), "step size cannot equal zero");
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), ENTRIES.len() as u64, 3, 0) }, SRF_FAILED);
        assert_eq!(failed(), format!("gravity: no library SRF entry {}", ENTRIES.len())); // alloc: Cold
    }

    #[test]
    fn a_materialised_sets_rows_stand_until_the_slots_next_open() {
        let mut keep = Vec::new(); // alloc: Cold
        let e = index(JSON_ELEMENTS_TEXT);
        let doc = text(&mut keep, br#"[1, "ab", null, {"k": 2}]"#);
        let mut c = Call::new(1, &[Some(doc)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 1, 0) }, 1);
        let mut seen = Vec::new(); // alloc: Cold
        let rows = c.drain(e, |d| {
            seen.push(d);
            payload(d)
        });
        assert_eq!(rows.unwrap(), vec![Some("1".to_owned()), Some("ab".to_owned()), None, Some(r#"{"k": 2}"#.to_owned())]); // alloc: Cold
        // every row's value read again after the later steps and the set's end (a suspend's resume re-reads them)
        assert_eq!(seen.iter().map(|&d| payload(d)).collect::<Vec<_>>(), ["1", "ab", r#"{"k": 2}"#]);
        assert_eq!(c._arena.iter().filter(|&&b| b != 0).count(), 0, "no row is copied into the ctx arena");
        let obj = text(&mut keep, br#"{"a": 1}"#);
        let mut c = Call::new(1, &[Some(obj)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 1, 0) }, SRF_FAILED);
        assert_eq!(failed(), "cannot call json_array_elements_text on a non-array");
    }

    #[test]
    fn an_open_keeps_no_pointer_into_its_arguments() {
        let mut keep = Vec::new(); // alloc: Cold
        let e = index(JSON_ELEMENTS_TEXT);
        let doc = text(&mut keep, br#"["x", "y"]"#);
        let mut c = Call::new(4, &[Some(doc)]);
        assert_eq!(unsafe { srf_open(c.ctx(), c.cell(), e, 1, 0) }, 1);
        // the caller's row memory is gone before the set is read (a stop, then a resume on a later row)
        keep.last_mut().unwrap().fill(0);
        assert_eq!(c.drain(e, payload).unwrap(), vec![Some("x".to_owned()), Some("y".to_owned())]); // alloc: Cold
        let ints = array(&mut keep, 23, &[Some(5i32.to_ne_bytes().to_vec())]);
        let mut a = Call::new(5, &[]);
        assert_eq!(unsafe { srf_array_open(a.ctx(), a.cell(), ints as u64, 4 | 1 << FACT_BYVAL | (b'i' as u64) << FACT_ALIGN) }, 1);
        keep.last_mut().unwrap().fill(0);
        assert_eq!(unsafe { srf_unnest_step(a.ctx(), a.cell()) }, 1);
        assert_eq!(a.out(0), (5, false), "the array's image is the slot's copy");
    }

    #[test]
    fn the_array_face_walks_by_value_and_by_reference_elements() {
        let mut keep = Vec::new(); // alloc: Cold
        let ints = array(&mut keep, 23, &[Some(10i32.to_ne_bytes().to_vec()), None, Some(30i32.to_ne_bytes().to_vec())]);
        let mut c = Call::new(2, &[]);
        let facts = 4 | 1 << FACT_BYVAL | (b'i' as u64) << FACT_ALIGN;
        assert_eq!(unsafe { srf_array_open(c.ctx(), c.cell(), ints as u64, facts) }, 1);
        let mut got = Vec::new(); // alloc: Cold
        while unsafe { srf_unnest_step(c.ctx(), c.cell()) } == 1 {
            got.push(match c.out(0) {
                (_, true) => None,
                (d, false) => Some(d as i32),
            });
        }
        assert_eq!(got, [Some(10), None, Some(30)]);
        let words = |s: &str| {
            let mut v = ((s.len() as u32 + 4) << 2).to_ne_bytes().to_vec();
            v.extend_from_slice(s.as_bytes());
            Some(v)
        };
        let texts = array(&mut keep, 25, &[words("ab"), words("cde")]);
        let facts = (-1i16 as u16 as u64) | (b'i' as u64) << FACT_ALIGN;
        assert_eq!(unsafe { srf_array_open(c.ctx(), c.cell(), texts as u64, facts) }, 1);
        let mut got = Vec::new(); // alloc: Cold
        while unsafe { srf_unnest_step(c.ctx(), c.cell()) } == 1 {
            got.push(payload(c.out(0).0));
        }
        assert_eq!(got, ["ab", "cde"]);
    }

    #[test]
    fn a_step_panic_is_parked_and_fails_the_call() {
        let mut keep = Vec::new(); // alloc: Cold
        let ints = array(&mut keep, 23, &[Some(7i32.to_ne_bytes().to_vec())]);
        let mut c = Call::new(3, &[]);
        // typalign 0 is no alignment the walk knows: `att_align_nominal` panics inside the step
        assert_eq!(unsafe { srf_array_open(c.ctx(), c.cell(), ints as u64, 4 | 1 << FACT_BYVAL) }, 1);
        assert_eq!(unsafe { srf_unnest_step(c.ctx(), c.cell()) }, SRF_FAILED);
        let resumed = std::panic::catch_unwind(gravity_abi::status::unwind::resume);
        assert!(resumed.is_err(), "the parked panic resumes at the caller");
    }
}
