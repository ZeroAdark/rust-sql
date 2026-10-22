use core::mem::{align_of, offset_of, size_of};

use gravity_abi::ctx::{cell, ctx_size, hdr, lane, out, site_ptr, Ctx, Identity, Lane, MorselRange, ScanPos, HEADER_SIZE, IDENTITY_SIZE, LANE_SIZE};
use gravity_abi::layout::LAYOUT;
use gravity_abi::program::{prog, ProgramBlock, PROGRAM_SIZE};
use gravity_abi::site::{site, Site, SITE_SIZE};
use gravity_abi::status::{fault, recheck, suspend, unsupported, Status, CANCELLED, ERROR, STOP};

/// The one-table check: every block's `FIELDS` tiles the block (no gaps but the declared pads,
/// no overlaps), every field is naturally aligned, sizes match, and the table names every field
/// of the struct (the table is generated from the struct, so this pins the generator).
#[test]
fn one_table_tiles_every_block() {
    for (block, size, fields) in LAYOUT {
        let mut at = 0;
        let mut sorted: Vec<(usize, usize, &str)> = fields.iter().map(|(n, o, s)| (*o, *s, *n)).collect();
        sorted.sort_unstable();
        for (off, sz, name) in sorted {
            assert!(off >= at, "{block}.{name} at {off} overlaps the previous field ending at {at}");
            assert_eq!(off, at, "{block}.{name} leaves a gap at {at}..{off}");
            let align = (sz & sz.wrapping_neg()).min(8);
            assert_eq!(off % align, 0, "{block}.{name} misaligned");
            at = off + sz;
        }
        assert_eq!(at, *size, "{block}: fields end at {at}, block size {size}");
    }
    assert_eq!(LAYOUT.iter().map(|b| b.0).collect::<Vec<_>>(), ["ctx", "site", "program", "out", "lane", "answer", "partials", "identity", "scanpos"]);
}

#[test]
fn header_offsets_match_consts() {
    assert_eq!((size_of::<Ctx>(), align_of::<Ctx>()), (HEADER_SIZE, 8));
    assert_eq!(HEADER_SIZE, 128);
    assert_eq!(offset_of!(Ctx, program), hdr::PROGRAM);
    assert_eq!(offset_of!(Ctx, sites), hdr::SITES);
    assert_eq!(offset_of!(Ctx, out), hdr::OUT);
    assert_eq!(offset_of!(Ctx, cancel), hdr::CANCEL);
    assert_eq!(offset_of!(Ctx, arena_cur), hdr::ARENA_CUR);
    assert_eq!(offset_of!(Ctx, arena_end), hdr::ARENA_END);
    assert_eq!(offset_of!(Ctx, claim), hdr::CLAIM_LO);
    assert_eq!(offset_of!(Ctx, claim) + offset_of!(MorselRange, hi), hdr::CLAIM_HI);
    assert_eq!(offset_of!(Ctx, resume), hdr::RESUME);
    assert_eq!(offset_of!(Ctx, params), hdr::PARAMS);
    assert_eq!(offset_of!(Ctx, identity), hdr::IDENTITY);
    assert_eq!(offset_of!(Ctx, flags), hdr::FLAGS);
    assert_eq!(offset_of!(Ctx, width), hdr::WIDTH);
    assert_eq!(size_of::<Identity>(), IDENTITY_SIZE);
    assert_eq!(offset_of!(Identity, snapshot), 24);
    assert_eq!((out::VALUES, out::ISNULL, out::COUNT, out::CAP), (0, 8, 16, 24));
}

#[test]
fn site_and_program_offsets_match_consts() {
    assert_eq!((size_of::<Site>(), align_of::<Site>()), (SITE_SIZE, 8));
    assert_eq!(SITE_SIZE, 192);
    assert_eq!(offset_of!(Site, next), site::NEXT);
    assert_eq!(offset_of!(Site, src), site::SRC);
    assert_eq!(offset_of!(Site, sel), site::SEL);
    assert_eq!(offset_of!(Site, pos) + offset_of!(ScanPos, rel), site::POS_REL);
    assert_eq!(offset_of!(Site, pos) + offset_of!(ScanPos, block), site::POS_BLOCK);
    assert_eq!(offset_of!(Site, answered), site::ANSWERED);
    assert_eq!(offset_of!(Site, cont), site::CONT);
    assert_eq!(site::WINDOW_CELLS, (site::SEL, site::NKEYS - site::SEL));
    assert!(site::WINDOW_CELLS.0 > site::ENV && site::WINDOW_CELLS.0 + site::WINDOW_CELLS.1 > site::ANSWERED && site::WINDOW_CELLS.0 + site::WINDOW_CELLS.1 <= site::NKEYS, "the window cells cover every per-Call field and nothing set at build");
    assert_eq!(size_of::<ProgramBlock>(), PROGRAM_SIZE);
    assert_eq!((prog::PUSH, prog::SPILL, prog::ARENA_GROW, prog::WRITE, prog::LOCK, prog::PARTIAL, prog::ENV, prog::SINK), (0, 8, 16, 24, 32, 40, 48, 56));
    assert_eq!(ctx_size(0), HEADER_SIZE);
    assert_eq!(ctx_size(2), HEADER_SIZE + 2 * SITE_SIZE);
}

#[test]
fn lane_offsets_match_consts() {
    assert_eq!(size_of::<Lane>(), LANE_SIZE);
    assert_eq!((lane::PTR, lane::VALID, lane::WIDTH, lane::KIND, lane::REGION, lane::FLAGS, lane::NCODES_OR_SCALE, lane::ID, lane::AUX), (0, 8, 16, 17, 18, 19, 20, 24, 28));
    assert_eq!(lane::id(lane::ID_OFFSET, 0), 2 << 24);
    assert_eq!((lane::id_kind(lane::id(lane::ID_ATTR, 7)), lane::id_ordinal(lane::id(lane::ID_ATTR, 7))), (0, 7));
    let words = [0b1011u64];
    let v4 = [-1i32, 2, 3, 4];
    let l = Lane::byval(v4.as_ptr() as *const u8, words.as_ptr(), 4, lane::REGION_ARENA, 0);
    let v8 = [7u64, 8];
    let d = Lane::byval(v8.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_PAGE, 0);
    unsafe {
        assert_eq!((l.is_valid(0), l.is_valid(1), l.is_valid(2), l.is_valid(3)), (true, true, false, true));
        assert_eq!(l.datum(0), u64::MAX, "an i32 lane reads sign-extended");
        assert_eq!(l.datum(3), 4);
        assert!(d.all_valid() && d.is_valid(1) && d.datum(1) == 8);
        assert_eq!(d.flags, lane::FLAG_ALL_VALID);
    }
}

#[test]
fn morsel_range_clamps_and_measures() {
    assert_eq!(MorselRange::ALL.clamp(10), MorselRange::new(0, 10));
    assert_eq!(MorselRange::new(4, 100).clamp(10), MorselRange::new(4, 10));
    assert!(MorselRange::new(40, 100).clamp(10).is_empty());
    assert_eq!(MorselRange::new(3, 8).len(), 5);
    assert_eq!(MorselRange::new(8, 3).len(), 0);
}

#[test]
fn status_round_trips() {
    let all = [Status::Ok(0), Status::Ok(8192), Status::Stop, Status::Cancelled, Status::Error, Status::Raised { mask: 1 }, Status::Raised { mask: (1 << 54) | 3 }, Status::Suspend { out_full: false }, Status::Suspend { out_full: true }, Status::Recheck { site: 0 }, Status::Recheck { site: 7 }, Status::Unsupported { kind: 1 }, Status::Unsupported { kind: 200 }, Status::Fault { site: 0, lane: 3, code: 0, part: 0 }, Status::Fault { site: 7, lane: 31, code: (1 << 24) - 1, part: 511 }, Status::Fault { site: 1, lane: 2, code: 5, part: (1 << 15) - 1 }];
    for s in all {
        assert_eq!(Status::decode(s.encode()), s, "{s:?} via {}", s.encode());
    }
    assert_eq!((STOP, CANCELLED, ERROR), (-1, -2, -3));
    assert_eq!(suspend(true), -0x104);
    assert_eq!(recheck(3), -0x305);
    assert_eq!(unsupported(2), -0x206);
    assert_eq!(fault(1, 2, 0x345), -(0x345_0201_07));
    assert_eq!(Status::decode(-0x201), Status::Bad(-0x201));
    assert_eq!(Status::decode(-77), Status::Bad(-77));
}

#[test]
fn pointer_helpers_address_the_blocks() {
    let mut buf = vec![0u64; ctx_size(2) / 8];
    let ctx = buf.as_mut_ptr() as *mut Ctx;
    unsafe {
        (*ctx).sites = (ctx as *mut u8).add(HEADER_SIZE) as *mut Site;
        (*site_ptr(ctx, 1)).state = 7;
        (*site_ptr(ctx, 1)).pos = ScanPos::new(16384, 5, 3);
        assert_eq!(*cell(ctx, HEADER_SIZE + SITE_SIZE + site::STATE) & 0xffff_ffff, 7);
        assert_eq!(*((ctx as *mut u8).add(HEADER_SIZE + SITE_SIZE + site::POS_REL) as *const u32), 16384);
        assert_eq!(*((ctx as *mut u8).add(HEADER_SIZE + SITE_SIZE + site::POS_BLOCK) as *const u32), 5);
        *cell(ctx, hdr::RESUME) = 9;
        assert_eq!((*ctx).resume, 9);
        assert!((*site_ptr(ctx, 1)).pos.is_valid() && !(*site_ptr(ctx, 0)).pos.is_valid());
        let mut arena = [0u64; 8];
        (*ctx).arena_cur = arena.as_mut_ptr() as *mut u8;
        (*ctx).arena_end = (*ctx).arena_cur.add(64);
        let a = gravity_abi::ctx::arena_bump(ctx, 20);
        let b = gravity_abi::ctx::arena_bump(ctx, 40);
        assert_eq!(b as usize - a as usize, 24, "bumps are 8-aligned");
        assert!(gravity_abi::ctx::arena_bump(ctx, 8).is_null(), "the window is exhausted at 64 bytes");
        let byte = core::sync::atomic::AtomicU8::new(0);
        (*ctx).cancel = byte.as_ptr();
        assert!(!gravity_abi::ctx::cancel_pending(ctx));
        byte.store(1, core::sync::atomic::Ordering::Release);
        assert!(gravity_abi::ctx::cancel_pending(ctx));
        // D3406: the service bit alone is no cancel without a poll that says so (the server's runs the owner's CFI)
        byte.store(gravity_abi::ctx::SERVICE_BIT, core::sync::atomic::Ordering::Release);
        assert!(!gravity_abi::ctx::cancel_pending(ctx) && !gravity_abi::ctx::abort_byte_set(byte.as_ptr()));
    }
}

#[test]
fn golden_layout() {
    let mut text = String::new();
    gravity_abi::layout::describe(&mut text).unwrap();
    let path = manifest_dir().join("tests/golden/ctx-layout.txt");
    if std::env::var_os("GRAVITY_ABI_BLESS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let golden = std::fs::read_to_string(path).expect("golden file");
    assert_eq!(text, golden, "ctx layout changed; rerun with GRAVITY_ABI_BLESS=1 if intended");
}

#[test]
fn raise_masks_name_the_incumbent_error() {
    use gravity_abi::status::{raise, raised};
    let m = raise::bit(raise::DIV_ZERO) | raise::bit(raise::INT4_RANGE);
    assert_eq!(Status::decode(raised(m)), Status::Raised { mask: m });
    assert_eq!(raise::first(m), raise::INT4_RANGE);
    assert_eq!(raise::describe(raise::INT4_RANGE), Some((b"22003", "integer out of range")));
    assert_eq!(raise::describe(raise::DIV_ZERO), Some((b"22012", "division by zero")));
    assert_eq!(raise::describe(raise::TS_RANGE), Some((b"22008", "timestamp out of range")));
    assert_eq!(raise::describe(raise::STEP_ZERO), Some((b"22023", "step size cannot equal zero")));
    assert_eq!(raise::describe(raise::DATE_NOT_FINITE), Some((b"22008", "cannot subtract infinite dates")));
    assert_eq!(raise::describe(raise::HOST), None);
    assert_eq!(raised(0), ERROR);
    assert!(raised(m) < 0);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
