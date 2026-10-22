//! planprod P1: the descriptors a program takes after its build sit in arenas the program holds, so filing it on the
//! program caches' ledger moves them with the build's. Its own binary: it files programs on the program caches' account,
//! which cache_shared.rs's C2 test reads as nothing else in its binary moving.

use gravity_abi::Ctx;
use std::sync::Arc;

use gravity_program::{Arena, Code, Layout, OutCol, Program, SiteDesc, WritePlan};

unsafe extern "C-unwind" fn fake_entry(_ctx: *mut Ctx) -> i64 {
    gravity_abi::status::DONE
}

fn program(rel: u32) -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::index_source(st.handle(), rel, rel + 1, &[1, 2], &[0, 1], &[3, 4])];
    let out = [OutCol::new("a", 23, -1, 4, true)];
    Program::new(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, Code::hand(fake_entry))
}

/// The descriptors a program takes after its build (EXPLAIN ANALYZE's cells, a LockRows' locks, the write plans beside
/// its write) sit in arenas the program holds, so filing it moves their bytes onto the program caches with the build's,
/// and they read and fold into the key as before.
#[test]
fn a_programs_later_descriptors_are_in_its_own_arenas_and_filing_moves_them_too() {
    use gravity_program::program::{LockKind, LockSite, WaitPolicy, WriteAt, END_STATEMENT};
    let _in = memledger::charge::enter_as(memledger::Owner::Session(memledger::Acct::backend(4087)));
    let plain = Arc::new(program(12));
    plain.file_on_ledger();
    let cells = [(1, 0u8), (1, 1), (2, 0)];
    let locks = [LockSite { rti: 1, relid: 77, lock: LockKind::Share, wait: WaitPolicy::Block }, LockSite { rti: 2, relid: 78, lock: LockKind::Exclusive, wait: WaitPolicy::Skip }];
    let writes = [WriteAt { site: 0, plan: WritePlan::SELECT, end: END_STATEMENT, node: 3 }];
    let built = || Program::new(plain.layout, &plain.sites, &plain.out, WritePlan::SELECT, Code::hand(fake_entry)).with_instr(&cells).with_locks(&locks).with_writes(&writes);
    let p = Arc::new(built());
    assert_eq!((&p.instr[..], &p.locks[..], &p.writes[..]), (&cells[..], &locks[..], &writes[..]));
    assert_ne!(p.key, plain.key, "the descriptors fold into the key");
    assert_eq!(p.key, built().key, "as they are, wherever they live");
    p.file_on_ledger();
    let side = p.ledgered_bytes() - plain.ledgered_bytes();
    let at_least = (cells.len() * 8 + locks.len() * std::mem::size_of::<LockSite>() + std::mem::size_of::<WriteAt>()) as u64;
    assert!(side >= at_least, "the later descriptors' arenas go with the program: {side} of at least {at_least}");
}

/// planprod P1-b (memlead's conditions): a program's v4 descriptors are a copy frozen into an arena the program holds, so
/// filing it moves them onto the program caches with the build's: on the holder after the filing, none of them on the
/// account of the statement that built them. A set past the mapped arenas' floor, whose pages the account shows.
#[test]
fn a_programs_v4_descriptors_are_on_the_holder_after_filing_and_none_on_the_statement() {
    use gravity_program::protos::{Proto, ProtoKind, V4Descs};
    memledger::bind_backend(4074, 9000 + 4074);
    let acct = memledger::Acct::backend(4074);
    assert!(acct.is_backend(), "a backend slot");
    let _in = memledger::charge::enter_as(memledger::Owner::Statement { acct, stmt: 1 });
    let plain = Arc::new(program(14));
    let n = gravity_program::cost::runtime::ARENA_MAPPED_MIN / std::mem::size_of::<Proto>() + 1;
    let d = V4Descs::new((0..n).map(|i| Proto::new(ProtoKind::Arms, (i % 512) as u16)).collect());
    let mapped = || memledger::account_row(acct).mapped_bytes;
    let m0 = mapped();
    let p = Arc::new(Program::new(plain.layout, &plain.sites, &plain.out, WritePlan::SELECT, Code::hand(fake_entry)).with_v4(&d));
    drop(d);
    let built = mapped() - m0;
    assert!(built >= (n * std::mem::size_of::<Proto>()) as i64, "the frozen set's pages, on the building backend's account until the filing: {built}");
    p.file_on_ledger();
    assert_eq!(mapped(), m0, "after the filing, none of them on the statement's account");
    assert!(p.ledgered_bytes() >= built as u64, "all of them on the holder: {} of {built}", p.ledgered_bytes());
    assert_eq!(p.v4.as_deref().map(|f| f.protos.len()), Some(n), "and read as built");
}
