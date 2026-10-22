//! LEDGER2 step 3: an operator's arena (`gravity_fn::arena`). Its chunks are its owner's, each charged once: what it
//! carves posts nothing, so the owner's held is the chunks' bytes (and the arena's own core) however many carves; the
//! owner sits under the statement that made it (the statement's subtree counts it) and leaves with the arena. A
//! growable buffer used with another arena, or with its own after a reset, panics in a release build too; a reset with a
//! live map panics. With a host, a chunk is the host's `SPILL_ARENA_CHUNK` and goes back by `SPILL_ARENA_GIVE`.
use std::alloc::Layout;
use std::sync::Mutex;

use gravity_abi::program::{SPILL_ARENA_CHUNK, SPILL_ARENA_GIVE};
use gravity_abi::{word, Ctx, ProgramBlock};
use gravity_fn::arena::{AMap, AVec, Kind, OpArena, ARENA_CHUNK_FIRST, ARENA_CHUNK_MAX, CHUNKS};
use memledger::charge::usage;
use memledger::{Acct, Owner};

/// On a new thread bound to backend `procno`, inside a statement: `f(account)`.
fn in_statement(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        memledger::statement_begin();
        f(Acct::backend(procno));
        let _ = memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

#[test]
fn ledger2_an_arenas_owner_holds_its_chunks_once_however_many_carves() {
    in_statement(4070, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let arena = OpArena::new(core::ptr::null_mut(), Kind::Sort, 1 << 20);
        let o = arena.owner();
        assert!(matches!(o, Owner::Op { .. }), "an operator owner: {o:?}");
        let shell = usage(o).held;
        assert!(shell > 0 && shell < 8 << 10, "the arena's core alone: {shell}");
        assert_eq!(usage(o).budget, 1 << 20, "its share, promised");
        let mut v: AVec<u64> = AVec::new();
        for i in 0..1_000_000u64 {
            v.push(&arena, i).unwrap();
        }
        let mut raw = 0;
        for _ in 0..10_000 {
            raw += arena.alloc(Layout::from_size_align(24, 8).unwrap()).is_ok() as usize;
        }
        assert_eq!(raw, 10_000);
        let held = usage(o).held;
        assert_eq!(held, shell + arena.chunk_bytes() as i64, "the chunks, each once, and the core");
        assert!(arena.chunk_draws() <= 12, "1,010,000 carves in {} chunks", arena.chunk_draws());
        assert!(arena.chunk_bytes() >= 8_000_000, "the vector's bytes are inside them");
        assert_eq!(v.as_slice(&arena)[999_999], 999_999);
        assert_eq!(usage(st).held, 0, "nothing on the statement itself");
        assert_eq!(usage(st).subtree, held, "its subtree counts the operator");
        let child = arena.child(Kind::Sort, 0);
        let b = child.bytes(&[7u8; 100]).unwrap();
        assert_eq!(child.read(b), &[7u8; 100][..]);
        assert_eq!(usage(o).subtree, held + usage(child.owner()).held, "a child under its arena's owner");
        drop(child);
        drop(arena);
        assert_eq!(usage(o).held, 0, "the owner leaves with its arena");
        assert_eq!(usage(st).subtree, 0);
    });
}

#[test]
fn ledger2_a_reset_gives_the_chunks_back_and_a_worker_arena_charges_the_same_owner() {
    in_statement(4071, |_| {
        let mut arena = OpArena::new(core::ptr::null_mut(), Kind::HashAgg, 0);
        let o = arena.owner();
        let shell = usage(o).held;
        let mut v: AVec<u32> = AVec::with_capacity(&arena, 100_000).unwrap();
        v.extend_from_slice(&arena, &[1, 2, 3]).unwrap();
        assert!(usage(o).held > shell);
        arena.reset();
        assert_eq!((usage(o).held, arena.chunk_bytes()), (shell, 0), "the chunks back at the reset");
        v.detach();
        v.push(&arena, 9).unwrap();
        assert_eq!(v.as_slice(&arena), &[9]);
        let seed = arena.for_worker();
        let before = usage(o).held;
        let held_by_worker = std::thread::spawn(move || {
            let w = seed.arena(core::ptr::null_mut());
            let mut x: AVec<u64> = AVec::new();
            for i in 0..50_000 {
                x.push(&w, i).unwrap();
            }
            (w.owner(), w.chunk_bytes())
        })
        .join()
        .unwrap();
        assert_eq!(held_by_worker.0, o, "the worker's arena charges the same owner");
        assert_eq!(usage(o).held, before, "and its chunks went back with it");
        drop(arena);
    });
}

#[test]
#[should_panic(expected = "does not own it")]
fn ledger2_a_vector_grown_with_another_arena_panics() {
    let a = OpArena::new(core::ptr::null_mut(), Kind::Sort, 0);
    let b = OpArena::new(core::ptr::null_mut(), Kind::Sort, 0);
    let mut v: AVec<u64> = AVec::new();
    for i in 0..4 {
        v.push(&a, i).unwrap();
    }
    // full: this push grows, with the wrong arena
    let _ = v.push(&b, 4);
}

#[test]
#[should_panic(expected = "does not own it")]
fn ledger2_a_vector_used_after_its_arenas_reset_panics() {
    let mut a = OpArena::new(core::ptr::null_mut(), Kind::Sort, 0);
    let mut v: AVec<u64> = AVec::new();
    v.push(&a, 1).unwrap();
    a.reset();
    let _ = v.as_slice(&a).len();
}

#[test]
#[should_panic(expected = "live AMap")]
fn ledger2_a_reset_with_a_live_map_panics() {
    let mut a = OpArena::new(core::ptr::null_mut(), Kind::HashAgg, 0);
    let mut m: AMap<u64, u64> = AMap::new(&a);
    for i in 0..1000 {
        m.try_insert(i, i * 2).unwrap();
    }
    assert_eq!(m.get(&999), Some(&1998));
    a.reset();
}

#[test]
fn ledger2_a_map_drops_before_the_reset() {
    let mut a = OpArena::new(core::ptr::null_mut(), Kind::HashAgg, 0);
    {
        let mut m: AMap<u64, u64> = AMap::new(&a);
        for i in 0..10_000 {
            m.try_insert(i, i).unwrap();
        }
        assert_eq!(m.len(), 10_000);
    }
    a.reset();
}

/// The fake host: the chunk sizes it served (or refused), the chunks it holds, and the slot owner it names (step 5).
struct Host {
    refuse: bool,
    asks: Vec<(u64, [u64; 2])>,
    lent: Vec<(usize, Vec<u64>)>,
    gives: usize,
    slot: Option<[u64; 2]>,
    /// CONVERT wave 2: the reply's word 0 beside the slot owner's words (its ledger kind).
    slot_kind: u64,
}

static HOST: Mutex<Host> = Mutex::new(Host { refuse: false, asks: Vec::new(), lent: Vec::new(), gives: 0, slot: None, slot_kind: 0 });
static ONE: Mutex<()> = Mutex::new(());

unsafe extern "C" fn spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    let mut h = HOST.lock().unwrap();
    match op {
        SPILL_ARENA_CHUNK => {
            // SAFETY: the arena passes the address of `[bytes, owner word 0, owner word 1]`
            let [bytes, w0, w1] = unsafe { *(arg as usize as *const [u64; 3]) };
            // LEDGER2 step 5: 0 bytes asks the slot owner's words
            if bytes == 0 {
                let Some(w) = h.slot else { return 0 };
                // SAFETY: as above, the arena's three words, written back
                unsafe { *(arg as usize as *mut [u64; 3]) = [h.slot_kind, w[0], w[1]] };
                return 1;
            }
            h.asks.push((bytes, [w0, w1]));
            if h.refuse {
                return 0;
            }
            let mut b = vec![0u64; (bytes as usize).div_ceil(8)];
            let p = b.as_mut_ptr() as usize;
            h.lent.push((p, b));
            p as i64
        }
        SPILL_ARENA_GIVE => {
            let i = h.lent.iter().position(|(p, _)| *p == arg as usize).expect("a give of a lent chunk");
            h.lent.swap_remove(i);
            h.gives += 1;
            0
        }
        _ => panic!("op {op}"),
    }
}

#[test]
fn ledger2_with_a_host_the_chunks_are_asked_of_it_for_the_owner_and_given_back() {
    let _one = ONE.lock().unwrap();
    let mut block = ProgramBlock { spill: word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..ProgramBlock::NONE };
    // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's.
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.program = word(&mut block as *mut ProgramBlock);
    {
        let mut h = HOST.lock().unwrap();
        (h.refuse, h.gives) = (false, 0);
        h.asks.clear();
    }
    let arena = OpArena::new(&mut ctx, Kind::Sort, 0);
    let o = arena.owner();
    let mut v: AVec<u64> = AVec::new();
    for i in 0..100_000 {
        v.push(&arena, i).unwrap();
    }
    let asked: u64 = HOST.lock().unwrap().asks.iter().map(|a| a.0).sum();
    {
        let h = HOST.lock().unwrap();
        assert!(h.asks.iter().all(|a| memledger::Owner::from_words(a.1) == Some(o)), "every chunk asked for the arena's owner");
        assert_eq!(h.asks[0].0, ARENA_CHUNK_FIRST as u64, "the first chunk is the smallest");
    }
    assert_eq!(arena.chunk_bytes(), asked, "the arena holds what it asked");
    drop(arena);
    {
        let h = HOST.lock().unwrap();
        assert_eq!((h.lent.len(), h.gives), (0, h.asks.len()), "every chunk given back at the drop");
    }
    HOST.lock().unwrap().refuse = true;
    let arena = OpArena::new(&mut ctx, Kind::Sort, 0);
    let mut v: AVec<u64> = AVec::new();
    assert!(v.push(&arena, 1).is_err(), "a refused chunk is the push's Refused, never an abort");
    drop(arena);
    HOST.lock().unwrap().refuse = false;
}

/// LEDGER2 step 5: a kernel call's scratch arena begins under the slot owner its host names, its chunks the allocator's
/// (never asked of the host: no road changes), each charged to it once; with no slot owner named there is none.
#[test]
fn ledger2_a_scratch_arena_is_the_slot_owners_child_on_the_allocators_chunks() {
    let _one = ONE.lock().unwrap();
    in_statement(4072, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::Sort as u32, 8 << 20);
        let mut block = ProgramBlock { spill: word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..ProgramBlock::NONE };
        // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's.
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.program = word(&mut block as *mut ProgramBlock);
        {
            let mut h = HOST.lock().unwrap();
            (h.refuse, h.gives, h.slot) = (false, 0, None);
            h.asks.clear();
        }
        assert!(OpArena::scratch(&mut ctx, Kind::Sort).is_none(), "no slot owner named: no scratch arena");
        assert!(OpArena::scratch(core::ptr::null_mut(), Kind::Sort).is_none(), "no host: none");
        HOST.lock().unwrap().slot = Some(slot.words());
        let s = OpArena::scratch(&mut ctx, Kind::Sort).expect("a scratch arena under the slot owner");
        let p = s.alloc(Layout::from_size_align(3 << 20, 8).unwrap()).expect("the allocator's chunk");
        // SAFETY: 3 MiB just carved, this test's alone
        unsafe { core::ptr::write_bytes(p.as_ptr(), 7, 3 << 20) };
        assert!(HOST.lock().unwrap().asks.is_empty(), "no chunk asked of the host");
        let held = usage(s.owner()).held;
        assert!(held >= 3 << 20, "the chunk on the scratch owner: {held}");
        assert_eq!(usage(slot).subtree, usage(slot).held + held, "under the slot owner");
        assert_eq!(usage(st).subtree, usage(slot).subtree, "and the statement's subtree");
        drop(s);
        assert_eq!(usage(slot).subtree, usage(slot).held, "gone with the call");
        HOST.lock().unwrap().slot = None;
        memledger::charge::op_end(slot);
    });
}

/// CONVERT1: `scratch_of` (an operator call's working memory, the lowerv4 hash build's spill `scatter`) charges the slot
/// owner ITSELF, no child begun: its chunks sit on that owner's own row while the arena lives and leave at its drop. With
/// no owner (`None`: the operator table was full, or a rig) the chunks charge no owner, where an allocator `Vec`'s bytes
/// sit. `AVec::resize` keeps the first elements and fills the rest.
#[test]
fn convert1_a_scratch_of_arena_charges_the_slot_owner_itself_and_without_one_no_owner() {
    in_statement(4073, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::HashBuild as u32, 8 << 20);
        let before = usage(slot).held;
        {
            let s = OpArena::scratch_of(Some(slot), Kind::HashBuild);
            assert_eq!(s.owner(), slot, "the slot owner itself, no child");
            let mut v: AVec<u8> = AVec::with_capacity(&s, 100).unwrap();
            v.resize(&s, 3 << 20, 7).unwrap();
            assert!(v.as_slice(&s).iter().all(|&b| b == 7));
            v.resize(&s, 10, 0).unwrap();
            v.resize(&s, 20, 9).unwrap();
            assert_eq!(v.as_slice(&s), &[7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9][..], "kept, then filled");
            let held = usage(slot).held - before;
            assert!(held >= 3 << 20 && held as u64 >= s.chunk_bytes(), "the chunks on the slot's own row: {held}");
            assert_eq!(usage(slot).subtree, usage(slot).held, "no child row");
            assert_eq!(usage(st).subtree, usage(slot).subtree, "the statement's subtree counts them through the slot");
        }
        assert_eq!(usage(slot).held, before, "gone at the drop");
        let sub = usage(st).subtree;
        {
            let s = OpArena::scratch_of(None, Kind::HashBuild);
            let mut v: AVec<u64> = AVec::with_capacity(&s, 1 << 18).unwrap();
            v.push(&s, 1).unwrap();
            assert!(s.chunk_bytes() >= 2 << 20, "the allocator's chunk");
            assert_eq!(usage(st).subtree, sub, "no owner charged");
        }
        memledger::charge::op_end(slot);
    });
}

/// CONVERT1: a worker's arena seeded from a scratch arena (`for_worker`) keeps its maker's road: the allocator's chunks,
/// never asked of the host even where the worker's ctx has one, charged to the same owner.
#[test]
fn convert1_a_scratch_seed_keeps_the_allocators_road_on_a_worker() {
    let _one = ONE.lock().unwrap();
    in_statement(4074, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::HashBuild as u32, 0);
        let mut block = ProgramBlock { spill: word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..ProgramBlock::NONE };
        // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's.
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.program = word(&mut block as *mut ProgramBlock);
        {
            let mut h = HOST.lock().unwrap();
            (h.refuse, h.gives, h.slot) = (false, 0, None);
            h.asks.clear();
        }
        let s = OpArena::scratch_of(Some(slot), Kind::HashBuild);
        let before = usage(slot).held;
        let w = s.for_worker().arena(&mut ctx);
        w.alloc(Layout::from_size_align(1 << 20, 8).unwrap()).expect("the allocator's chunk");
        assert!(HOST.lock().unwrap().asks.is_empty(), "no chunk asked of the worker's host");
        assert!(usage(slot).held - before >= 1 << 20, "on the slot's own row");
        drop(w);
        assert_eq!(usage(slot).held, before, "back at the worker arena's drop");
        drop(s);
        memledger::charge::op_end(slot);
    });
}

/// CONVERT wave 2: `slot_owner_kind` reads the host reply's word 0 as the ledger kind beside the owner's words (`slot_owner`
/// the words alone): a slot owner with its breaker's kind, the statement's owner (a pipeline that produces no slot) with
/// `Kind::Region`; no host, no owner named, or a word 0 no `Kind` names: none.
#[test]
fn wave2_slot_owner_kind_reads_the_kind_beside_the_owner() {
    let _one = ONE.lock().unwrap();
    in_statement(4075, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::HashAgg as u32, 0);
        let mut block = ProgramBlock { spill: word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..ProgramBlock::NONE };
        // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's.
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.program = word(&mut block as *mut ProgramBlock);
        let set = |slot: Option<[u64; 2]>, kind: u64| {
            let mut h = HOST.lock().unwrap();
            (h.refuse, h.slot, h.slot_kind) = (false, slot, kind);
        };
        set(None, 0);
        assert_eq!(OpArena::slot_owner_kind(&mut ctx), None, "no owner named: none");
        assert_eq!(OpArena::slot_owner_kind(core::ptr::null_mut()), None, "no host: none");
        set(Some(slot.words()), Kind::HashAgg as u64);
        assert_eq!(OpArena::slot_owner_kind(&mut ctx), Some((slot, Kind::HashAgg)), "the produced slot's owner and its breaker's kind");
        assert_eq!(OpArena::slot_owner(&mut ctx), Some(slot), "slot_owner: the same owner");
        set(Some(st.words()), Kind::Region as u64);
        assert_eq!(OpArena::slot_owner_kind(&mut ctx), Some((st, Kind::Region)), "the statement's owner, Kind::Region");
        set(Some(slot.words()), 99);
        assert_eq!(OpArena::slot_owner_kind(&mut ctx), None, "a word 0 no Kind names: none");
        set(None, 0);
        memledger::charge::op_end(slot);
    });
}

/// CONVERT wave 3, the served -> refused guard: an arena draws past its inline chunk table (`CHUNKS`, the old limit:
/// ~7.4 GiB of 64 MiB chunks, where a carve was refused and a served query became 53200). The chunks past it go to the
/// overflow table: a handle into one reads back; the owner holds every chunk once plus the table's bytes; a reset gives
/// the chunks back and keeps the table, which the next draws past `CHUNKS` reuse; the drop leaves the owner as it was.
/// The carves are 64 MiB and never touched: address space, not resident memory.
#[test]
fn wave3_an_arena_draws_past_its_inline_chunk_table_its_overflow_charged_to_the_owner() {
    in_statement(4076, |a| {
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let slot = memledger::charge::op_begin(st, Kind::HashBuild as u32, 0);
        let before = usage(slot).held;
        {
            let mut s = OpArena::scratch_of(Some(slot), Kind::HashBuild);
            let made = usage(slot).held;
            let big = Layout::from_size_align(ARENA_CHUNK_MAX, 8).unwrap();
            let n = CHUNKS + 12;
            for i in 0..n {
                s.alloc(big).unwrap_or_else(|r| panic!("carve {i} of {n} refused: {r:?}"));
            }
            assert_eq!(s.chunk_draws(), n as u64, "one chunk per carve");
            let b = s.bytes(&[5u8; 100]).unwrap();
            assert_eq!(s.chunk_draws(), n as u64, "the handle's bytes in the newest chunk, past the inline table");
            assert_eq!(s.read(b), &[5u8; 100][..], "a handle into an overflow chunk reads back");
            let table = usage(slot).held - made - s.chunk_bytes() as i64;
            assert!(table >= 12 * 24 && table < 64 << 10, "the overflow table's bytes on the owner: {table}");
            s.reset();
            assert_eq!(s.chunk_bytes(), 0);
            assert_eq!(usage(slot).held, made + table, "every chunk back at the reset, the table kept");
            for i in 0..CHUNKS + 2 {
                s.alloc(big).unwrap_or_else(|r| panic!("carve {i} after the reset refused: {r:?}"));
            }
            assert_eq!(usage(slot).held - made - s.chunk_bytes() as i64, table, "the table reused, not grown");
        }
        assert_eq!(usage(slot).held, before, "the drop leaves the owner as it was");
        memledger::charge::op_end(slot);
    });
}

/// CONVERT wave 3 (c): a scratch arena's charges carry its owner's release (memledger `release_of`), never a fixed
/// statement release: a portal owner's arena (a cursor's slot, the statement's owner past the statement) is the portal's,
/// an operator owner's the statement's; its worker seed carries the same.
#[test]
fn wave3_a_scratch_arena_carries_its_owners_release() {
    in_statement(4077, |a| {
        let p = memledger::charge::new_owner_id();
        memledger::charge::portal_begin(p);
        let po = Owner::Portal { acct: a, portal: p };
        let s = OpArena::scratch_of(Some(po), Kind::Sort);
        assert!(format!("{:?}", s.for_worker()).contains("release: Portal"), "{:?}", s.for_worker());
        drop(s);
        let st = Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
        let op = memledger::charge::op_begin(st, Kind::Sort as u32, 0);
        let s = OpArena::scratch_of(Some(op), Kind::Sort);
        assert!(format!("{:?}", s.for_worker()).contains("release: Statement"), "{:?}", s.for_worker());
        drop(s);
        memledger::charge::op_end(op);
        let _ = memledger::charge::portal_end(a, p);
    });
}
