//! M6 H3 (cards/M6.md L2 `hosts/src/v4/m6.rs` / `exec/src/v4/m6.rs`, V4R-269): the hosted aggregate bracket through the
//! kernels entries (host_abi v4/m6_agg.rs) over `AggArena`: PG's strictness, the first input copied as a strict transfn's
//! state, the by-reference copy into the aggregate context, the initcond per group, the finalfn with its extra arguments
//! (array_agg's `AggCheckCallContext` / `get_fn_expr_argtype`); a raising transfn's error is PG's (message and SQLSTATE);
//! a denied aggregate is permission denied; the arena's contexts are freed on every path (the memory-context census),
//! mid-fold included (an error's or a cancel's end drops it); a call past the arena's slots or over no arena is an error.
//! Parallel (mcx new_unobserved): a slot per worker ordinal folded on its own thread, the partial states combined on the
//! leader by the combinefn, every slot freed there.
use std::sync::{Mutex, MutexGuard, Once};

use gravity_abi::{Ctx, ProgramBlock, PtrWord};
use gravity_hosts::v4::m6::AggArena;
use gravity_kernels::ops::agg::trans::hosted::{m6_agg_combine, m6_agg_final, m6_agg_trans, AggHead, STATE_HOME, STATE_INIT};
use mcx::{Mcx, MemoryContext, PgString};
use types_nodes::list::OidList;
use types_nodes::primnodes::Aggref;

static SEAMS: Once = Once::new();
/// The census counts every thread's arenas: one test at a time.
static LOCK: Mutex<()> = Mutex::new(());
const ARRAY_AGG_INT4: u32 = 2335;
const MAX_TEXT: u32 = 2129;
/// Fabricated aggregates: int4pl (strict) from `2147483647`; and one the user may not execute.
const OVERFLOWS: u32 = 99_001;
const DENIED: u32 = 99_002;
/// D1280: textcat with a text initcond (a by-reference initval its input function made).
const TEXT_INIT: u32 = 99_003;

fn install() -> MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    pg_locale::set_default_locale_c_for_tests();
    SEAMS.call_once(|| {
        seams_init::register_builtin_tables();
        init_small::init_seams();
        mbutils::init_seams();
        elog::init_seams();
        pg_locale::init_seams();
        detoast::init_seams();
        funcapi::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::object_aclcheck::set(|_, f, _, _| Ok(if f == DENIED { 1 } else { 0 }));
        syscache_seams::pg_proc_proname::set(|_| Ok(None));
        syscache_seams::lookup_pg_proc_secdef::set(|_| Ok(Some(syscache_seams::PgProcSecdefShape { proowner: 10, prosecdef: false, proconfig: None })));
        // every aggregate PARALLEL SAFE: array_agg's fake shape has no combinefn, max(text)'s is text_larger
        syscache_seams::lookup_pg_proc_shape::set(|_| {
            Ok(Some(syscache_seams::PgProcShape { pronamespace: 11, prorettype: 0, provariadic: 0, prosupport: 0, prolang: 12, pronargs: 1, prokind: b'a' as i8, provolatile: b'i' as i8, proparallel: b's' as i8, proretset: false, proisstrict: false, proleakproof: false, prosecdef: false, proconfig_isnull: true }))
        });
        // (typlen, byval, align, storage): int4, text, internal, _int4
        syscache_seams::lookup_pg_type_shape::set(|t| {
            let (typlen, typbyval, typalign, typstorage) = match t {
                23 => (4, true, b'i', b'p'),
                25 => (-1, false, b'i', b'x'),
                2281 => (8, true, b'd', b'p'),
                1007 => (-1, false, b'i', b'x'),
                _ => return Ok(None),
            };
            Ok(Some(types_tuple::tupdesc::PgTypeShape { typlen, typbyval, typalign: typalign as i8, typstorage: typstorage as i8, typcollation: if t == 25 { 100 } else { 0 } }))
        });
        // V4R-453: the rows a parallel arena arms (int4, text, internal, _int4): capture_type_row's two projections
        syscache_seams::lookup_pg_type_typcache_shape::set(|t| {
            let (typlen, typbyval, typalign, typtype, elem) = match t {
                23 => (4, true, b'i', b'b', 0),
                25 => (-1, false, b'i', b'b', 0),
                2281 => (8, true, b'd', b'p', 0),
                1007 => (-1, false, b'i', b'b', 23),
                _ => return Ok(None),
            };
            Ok(Some(syscache_seams::PgTypeTypcacheShape { typname: Default::default(), typlen, typbyval, typalign: typalign as i8, typstorage: b'p' as i8, typtype: typtype as i8, typisdefined: true, typrelid: 0, typsubscript: 0, typelem: elem, typarray: 0, typcollation: if t == 25 { 100 } else { 0 } }))
        });
        syscache_seams::pg_type_base_shape::set(|t| {
            Ok(matches!(t, 23 | 25 | 2281 | 1007).then(|| syscache_seams::PgTypeBaseShape { typtype: if t == 2281 { b'p' as i8 } else { b'b' as i8 }, typbasetype: 0, typtypmod: -1, typelem: if t == 1007 { 23 } else { 0 }, typsubscript: 0 }))
        });
        // the typcache entries a parallel arena images: no default opclass here (the tests' types compare in their own code)
        syscache_seams::syscache_hash_value_typeoid::set(|t| Ok(t));
        indexcmds_seams::get_default_opclass::set(|_, _| Ok(0));
        // int4in for the initcond
        syscache_seams::pg_type_io_shape::set(|t| {
            Ok(match t {
                23 => Some(syscache_seams::PgTypeIoShape { oid: 23, typinput: 42, typoutput: 43, typreceive: 2406, typsend: 2407, typmodin: 0, typmodout: 0, typelem: 0, typlen: 4, typbyval: true, typalign: b'i' as i8, typdelim: b',' as i8, typisdefined: true }),
                25 => Some(syscache_seams::PgTypeIoShape { oid: 25, typinput: 46, typoutput: 47, typreceive: 2414, typsend: 2415, typmodin: 0, typmodout: 0, typelem: 0, typlen: -1, typbyval: false, typalign: b'i' as i8, typdelim: b',' as i8, typisdefined: true }),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_aggregate_shape::set(|f| {
            // (transfn, finalfn, finalextra, transtype, combinefn)
            let (t, fin, extra, tt, comb) = match f {
                ARRAY_AGG_INT4 => (2333, 2334, true, 2281, 0),
                MAX_TEXT => (458, 0, false, 25, 458),
                OVERFLOWS | DENIED => (177, 0, false, 23, 0),
                TEXT_INIT => (1258, 0, false, 25, 0),
                _ => return Ok(None),
            };
            Ok(Some(syscache_seams::PgAggregateShape {
                aggkind: b'n' as i8,
                aggnumdirectargs: 0,
                aggtransfn: t,
                aggfinalfn: fin,
                aggcombinefn: comb,
                aggserialfn: 0,
                aggdeserialfn: 0,
                aggmtransfn: 0,
                aggminvtransfn: 0,
                aggmfinalfn: 0,
                aggfinalextra: extra,
                aggmfinalextra: false,
                aggfinalmodify: b'r' as i8,
                aggmfinalmodify: b'r' as i8,
                aggsortop: 0,
                aggtranstype: tt,
                aggtransspace: 0,
                aggmtranstype: 0,
            }))
        });
        syscache_seams::pg_aggregate_agginitval::set(|mcx, f| {
            Ok(match f {
                OVERFLOWS => Some(Some(PgString::from_str_in("2147483647", mcx).unwrap())),
                TEXT_INIT => Some(Some(PgString::from_str_in("an initcond longer than any short-string form:", mcx).unwrap())),
                ARRAY_AGG_INT4 | MAX_TEXT | DENIED => Some(None),
                _ => None,
            })
        });
    });
    mcx::debug_census::enable();
    g
}

fn leaked() -> Mcx<'static> {
    Box::leak(Box::new(MemoryContext::new("v4_m6_agg test"))).mcx() // alloc: Cold
}

fn aggref(aggno: i32, aggfnoid: u32, aggtype: u32, tt: u32, input: u32) -> &'static Aggref<'static> {
    let m = leaked();
    Box::leak(Box::new(Aggref { aggfnoid, aggtype, aggtranstype: tt, aggargtypes: OidList::make1(m, input).unwrap(), inputcollid: if input == 25 { 100 } else { 0 }, aggkind: b'n' as i8, aggno, aggtransno: aggno, ..Aggref::default() })) // alloc: Cold
}

/// Live contexts of the arena's (the census counts by the name's literal).
fn live_arenas() -> i64 {
    mcx::debug_census::snapshot().iter().filter(|(n, _)| n.starts_with("gravity agg arena")).map(|(_, k)| k).sum()
}

fn trans(h: u64, aggno: u64, s: &mut [u64; 2], v: Option<u64>) -> Result<(), Box<types_error::PgError>> {
    trans_as(0, h, aggno, s, v)
}

/// One transition as worker `ordinal`.
fn trans_as(ordinal: u16, h: u64, aggno: u64, s: &mut [u64; 2], v: Option<u64>) -> Result<(), Box<types_error::PgError>> {
    let (arg, null) = (v.unwrap_or(0), v.is_none() as u8);
    let _ = gravity_fn::take_error();
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.worker_ordinal = ordinal;
    match unsafe { m6_agg_trans(&mut ctx as *mut Ctx as u64, h, aggno, s.as_mut_ptr() as u64, &arg as *const u64 as u64, &null as *const u8 as u64) } {
        0 => Ok(()),
        _ => Err(gravity_fn::take_error().expect("an error stashed")),
    }
}

/// `m6_agg_combine` as worker `ordinal`: `from` into `into`.
fn combine_as(ordinal: u16, h: u64, aggno: u64, into: &mut [u64; 2], from: &mut [u64; 2]) -> Result<(), Box<types_error::PgError>> {
    let _ = gravity_fn::take_error();
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.worker_ordinal = ordinal;
    match unsafe { m6_agg_combine(&mut ctx, h, aggno, into.as_mut_ptr().cast(), from.as_mut_ptr().cast()) } {
        0 => Ok(()),
        _ => Err(gravity_fn::take_error().expect("an error stashed")),
    }
}

/// The final value (None: NULL) through a ctx whose scratch is `scratch`.
fn fin(h: u64, aggno: u64, s: &mut [u64; 2], scratch: &mut [u64]) -> Option<u64> {
    fin_as(0, h, aggno, s, scratch)
}

/// `fin` as worker `ordinal`.
fn fin_as(ordinal: u16, h: u64, aggno: u64, s: &mut [u64; 2], scratch: &mut [u64]) -> Option<u64> {
    let mut prog: ProgramBlock = unsafe { core::mem::zeroed() };
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.worker_ordinal = ordinal;
    ctx.program = &mut prog;
    ctx.arena_cur = scratch.as_mut_ptr().cast();
    ctx.arena_end = unsafe { ctx.arena_cur.add(scratch.len() * 8) };
    let mut out = [0u64; 2];
    let st = unsafe { m6_agg_final(&mut ctx as *mut Ctx as u64, h, aggno, s.as_mut_ptr() as u64, out.as_mut_ptr() as u64) };
    assert_eq!(st, 0, "{:?}", gravity_fn::take_error().map(|e| e.message().to_string())); // alloc: Cold
    let (lo, hi) = (scratch.as_ptr() as u64, scratch.as_ptr() as u64 + scratch.len() as u64 * 8);
    assert!(out[1] != 0 || out[0] >= lo && out[0] < hi, "a by-reference final is copied into the scratch");
    (out[1] == 0).then_some(out[0])
}

/// D3578: `fin` over an arena that keeps its finals' images (`AggHead.stable`: a streamed sorted group's, a plain Agg's):
/// the by-reference final is the aggregate's own buffer, and the Call's scratch is never drawn (a sorted input's emit that
/// feeds a fold above runs as one call, so a copy per group stayed to the call's end).
fn fin_kept(h: u64, aggno: u64, s: &mut [u64; 2], scratch: &mut [u64]) -> Option<u64> {
    let mut prog: ProgramBlock = unsafe { core::mem::zeroed() };
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.program = &mut prog;
    let start: *mut u8 = scratch.as_mut_ptr().cast();
    ctx.arena_cur = gravity_abi::word(start);
    ctx.arena_end = gravity_abi::word(unsafe { start.add(scratch.len() * 8) });
    let mut out = [0u64; 2];
    let st = unsafe { m6_agg_final(&mut ctx as *mut Ctx as u64, h, aggno, s.as_mut_ptr() as u64, out.as_mut_ptr() as u64) };
    assert_eq!(st, 0, "{:?}", gravity_fn::take_error().map(|e| e.message().to_string())); // alloc: Cold
    assert_eq!(ctx.arena_cur.ptr(), start, "a kept final draws no scratch");
    let (lo, hi) = (scratch.as_ptr() as u64, scratch.as_ptr() as u64 + scratch.len() as u64 * 8);
    assert!(out[1] != 0 || !(out[0] >= lo && out[0] < hi), "a kept final is the aggregate's own buffer");
    (out[1] == 0).then_some(out[0])
}

/// A by-value final (None: NULL): the datum itself, no scratch.
fn fin_val(h: u64, aggno: u64, s: &mut [u64; 2]) -> Option<u64> {
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    let mut out = [0u64; 2];
    let st = unsafe { m6_agg_final(&mut ctx as *mut Ctx as u64, h, aggno, s.as_mut_ptr() as u64, out.as_mut_ptr() as u64) };
    assert_eq!(st, 0, "{:?}", gravity_fn::take_error().map(|e| e.message().to_string())); // alloc: Cold
    (out[1] == 0).then_some(out[0])
}

fn text(s: &str) -> Vec<u8> {
    let mut v = ((s.len() as u32 + 4) << 2).to_le_bytes().to_vec(); // alloc: Cold
    v.extend_from_slice(s.as_bytes());
    v
}

fn text_of(p: u64) -> String {
    let hdr = unsafe { (p as *const u32).read_unaligned() } >> 2;
    let b = unsafe { core::slice::from_raw_parts((p + 4) as *const u8, hdr as usize - 4) };
    String::from_utf8(b.to_vec()).unwrap() // alloc: Cold
}

#[test]
fn array_agg_builds_in_the_aggregate_context_and_every_path_frees_the_arena() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new(&[aggref(0, ARRAY_AGG_INT4, 1007, 2281, 23)], &[], 1).unwrap();
    assert_eq!(live_arenas(), before + 3, "the aggcontext, the init and the call contexts");
    let h = arena.handle();
    let (mut g1, mut g2, mut g3) = ([0u64; 2], [0u64; 2], [0u64; 2]);
    for v in [Some(1), None, Some(3)] {
        trans(h, 0, &mut g1, v).unwrap();
    }
    trans(h, 0, &mut g3, Some(7)).unwrap();
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    let a = fin(h, 0, &mut g1, &mut scratch).expect("an array");
    // a 1-D int4 array with a null bitmap: ndim 1, a data offset, element type 23, dims {3}, lbound {1}, bitmap 101
    let w = |i: usize| unsafe { ((a + 4 * i as u64) as *const i32).read_unaligned() };
    assert_eq!((w(1), w(2) != 0, w(3), w(4), w(5)), (1, true, 23, 3, 1));
    assert_eq!(unsafe { ((a + 24) as *const u8).read() } & 7, 0b101);
    assert_eq!(fin(h, 0, &mut g2, &mut scratch), None, "no input: array_agg's NULL");
    assert_eq!(unsafe { (g2.as_ptr() as *const u8).add(STATE_INIT).read() }, 1, "the final initialized the slot");
    drop(arena); // g3 left mid-fold: an error's or a cancel's end drops the arena the same way
    assert_eq!(live_arenas(), before, "every arena context freed");
}

#[test]
fn a_strict_by_reference_transfn_copies_its_first_input_and_each_new_state() {
    let _g = install();
    let mut arena = AggArena::new(&[aggref(3, MAX_TEXT, 25, 25, 25)], &[], 1).unwrap();
    let h = arena.handle();
    let mut g = [0u64; 2];
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    for s in ["b", "a", "", "c"] {
        let mut v = text(s);
        trans(h, 3, &mut g, if s.is_empty() { None } else { Some(v.as_mut_ptr() as u64) }).unwrap();
        v.fill(0xee); // the input's memory is the caller's: the state holds its own copy
        assert_eq!(text_of(fin(h, 3, &mut g, &mut scratch).unwrap()), if s == "c" { "c" } else { "b" });
    }
    let mut none = [0u64; 2];
    assert_eq!(fin(h, 3, &mut none, &mut scratch), None, "no input: NULL (no initcond)");
    assert_eq!(trans(h, 4, &mut g, None).unwrap_err().message(), "gravity: no hosted aggregate 4");
}

/// V4R-424: two aggregates the planner gave one transition state (one aggtransno) fold one slot: the aggregate that
/// opened it steps it, the other's call on it is no transition (PG calls a shared transfn once per row), and each runs
/// its own final over the shared state.
#[test]
fn a_shared_transition_state_steps_once_per_row_and_each_aggregate_finalizes_it() {
    let _g = install();
    let a0 = aggref(0, ARRAY_AGG_INT4, 1007, 2281, 23);
    let a1 = Box::leak(Box::new(Aggref { aggfnoid: ARRAY_AGG_INT4, aggtype: 1007, aggtranstype: 2281, aggargtypes: OidList::make1(leaked(), 23).unwrap(), aggkind: b'n' as i8, aggno: 1, aggtransno: 0, ..Aggref::default() })); // alloc: Cold
    let mut arena = AggArena::new(&[a0, a1], &[], 1).unwrap();
    let h = arena.handle();
    let mut g = [0u64; 2];
    for v in [1, 2] {
        trans(h, 0, &mut g, Some(v)).unwrap();
        trans(h, 1, &mut g, Some(v)).unwrap();
    }
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    for aggno in [0, 1] {
        let a = fin(h, aggno, &mut g, &mut scratch).expect("an array");
        let w = |i: usize| unsafe { ((a + 4 * i as u64) as *const i32).read_unaligned() };
        assert_eq!((w(3), w(4)), (23, 2), "aggno {aggno}: two elements, one per row, not four");
    }
}

/// D1280: a by-reference initcond (text) is copied out of its input function's FmgrInfo into the arena's context when
/// the arena binds it (GetAggInitVal's datumCopy): memory reused after the bind leaves every group's first state intact.
#[test]
fn a_by_reference_initcond_outlives_its_input_functions_fmgrinfo() {
    let _g = install();
    let mut arena = AggArena::new(&[aggref(0, TEXT_INIT, 25, 25, 25)], &[], 1).unwrap();
    let churn: Vec<Vec<u8>> = (0..4096).map(|i| vec![0xa5; 16 + i % 512]).collect(); // alloc: Cold
    drop(churn);
    let _reuse: Vec<Vec<u8>> = (0..4096).map(|i| vec![0x5a; 16 + i % 512]).collect(); // alloc: Cold
    let h = arena.handle();
    let (mut g1, mut g2) = ([0u64; 2], [0u64; 2]);
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    let mut v = text("x");
    trans(h, 0, &mut g1, Some(v.as_mut_ptr() as u64)).unwrap();
    assert_eq!(text_of(fin(h, 0, &mut g1, &mut scratch).unwrap()), "an initcond longer than any short-string form:x");
    let mut w = text("y");
    trans(h, 0, &mut g2, Some(w.as_mut_ptr() as u64)).unwrap();
    assert_eq!(text_of(fin(h, 0, &mut g2, &mut scratch).unwrap()), "an initcond longer than any short-string form:y", "a second group's copy of the initcond");
}

#[test]
fn a_raising_transfn_returns_pgs_error_and_the_arena_still_frees() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new(&[aggref(0, OVERFLOWS, 23, 23, 23)], &[], 1).unwrap();
    let h = arena.handle();
    let mut g = [0u64; 2];
    let e = trans(h, 0, &mut g, Some(1)).unwrap_err();
    assert_eq!((e.message(), &types_error::unpack_sqlstate(e.sqlstate())), ("integer out of range", b"22003"));
    drop(arena);
    assert_eq!(live_arenas(), before);
    let e = AggArena::new(&[aggref(0, DENIED, 23, 23, 23)], &[], 1).err().expect("denied");
    assert_eq!((e.message(), &types_error::unpack_sqlstate(e.sqlstate())), ("permission denied for aggregate 99002", b"42501"));
}

#[test]
fn a_call_past_the_arenas_slots_or_over_no_arena_is_an_error() {
    let _g = install();
    let mut arena = AggArena::new(&[aggref(0, MAX_TEXT, 25, 25, 25)], &[], 1).unwrap();
    assert!(!arena.parallel(), "width 1: one slot");
    let h = arena.handle();
    let e = trans_as(1, h, 0, &mut [0u64; 2], None).unwrap_err();
    assert_eq!(e.message(), "gravity: a hosted aggregate called by worker 1 of an arena of 1");
    let mut serial = AggArena::new(&[aggref(0, ARRAY_AGG_INT4, 1007, 2281, 23)], &[], 8).unwrap();
    assert!(!serial.parallel(), "no combinefn: serial at any width");
    let e = trans_as(3, serial.handle(), 0, &mut [0u64; 2], Some(1)).unwrap_err();
    assert_eq!(e.message(), "gravity: a hosted aggregate called by worker 3 of an arena of 1");
    let fake = [0u64; 3];
    let e = trans(fake.as_ptr() as u64, 0, &mut [0u64; 2], None).unwrap_err();
    assert_eq!(e.message(), "gravity: a hosted aggregate call over no live arena");
}

#[test]
fn a_parallel_arena_folds_a_slot_per_worker_thread_and_combines_on_the_leader() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new(&[aggref(2, MAX_TEXT, 25, 25, 25)], &[], 4).unwrap();
    assert!(arena.parallel(), "max(text): text_larger combines, PARALLEL SAFE");
    assert_eq!(live_arenas(), before + 1 + 2 * 5, "the init context, and each of 4 worker slots and the merge slot's two");
    let h = arena.handle();
    // worker w folds its own words on its own thread, into its own record's state slot
    let words = [["pear", "apple"], ["", "zebra"], ["", ""], ["kiwi", "fig"]];
    let states: Vec<[u64; 2]> = std::thread::scope(|sc| {
        let js: Vec<_> = words.iter().enumerate().map(|(w, ws)| sc.spawn(move || {
            pg_locale::set_default_locale_c_for_tests(); // a pool worker is armed with the leader's (StatementArm)
            let mut g = [0u64; 2];
            for s in ws {
                let mut v = text(s);
                trans_as(w as u16, h, 2, &mut g, (!s.is_empty()).then(|| v.as_mut_ptr() as u64)).unwrap();
                v.fill(0xee);
            }
            g
        })).collect(); // alloc: Cold
        js.into_iter().map(|j| j.join().unwrap()).collect() // alloc: Cold
    });
    // the runner's merge: the first held state moves, each next is combined into it; worker 2 saw only NULLs
    let mut into = states[0];
    for s in &states[1..] {
        unsafe { arena.combine(2, into.as_mut_ptr().cast(), s.as_ptr().cast()) }.unwrap();
    }
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    assert_eq!(text_of(fin(h, 2, &mut into, &mut scratch).unwrap()), "zebra");
    let e = trans_as(4, h, 2, &mut [0u64; 2], None).unwrap_err();
    assert_eq!(e.message(), "gravity: a hosted aggregate called by worker 4 of an arena of 4", "the merge slot is the leader's");
    drop(arena);
    assert_eq!(live_arenas(), before, "every slot freed on the leader, whichever thread made or used it");
}

/// D3062-b: a keyed arena wide at width 4 (max(text): text_larger combines, PARALLEL SAFE): a group's one home is shared by
/// every worker's records, each worker advancing its own state there on its own thread (one saw only NULLs); the final
/// gathers them once in the merge slot and a second final reads the gathered state; array_agg (no combinefn here) gets
/// the serial keyed arena. Every slot freed on the leader.
#[test]
fn a_wide_keyed_arena_keeps_a_state_per_worker_in_each_home_and_gathers_at_the_final() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new_keyed(&[aggref(2, MAX_TEXT, 25, 25, 25)], &[], 4).unwrap();
    assert!(arena.parallel() && arena.wide());
    let h = arena.handle();
    let head = h as *mut AggHead;
    let key = [7u64];
    let home = unsafe { ((*head).home)(head, core::ptr::null_mut(), key.as_ptr(), 1, 1) } as u64;
    assert_eq!(unsafe { ((*head).home)(head, core::ptr::null_mut(), key.as_ptr(), 1, 1) } as u64, home, "one home per group");
    let tagged = move || {
        let mut s = [home, 0u64];
        let b = s.as_mut_ptr() as *mut u8;
        unsafe { b.add(STATE_HOME).write(1) };
        unsafe { b.add(STATE_INIT).write(1) };
        s
    };
    let words = [["pear", "apple"], ["", "zebra"], ["", ""], ["kiwi", "fig"]];
    std::thread::scope(|sc| {
        for (w, ws) in words.iter().enumerate() {
            sc.spawn(move || {
                pg_locale::set_default_locale_c_for_tests(); // a pool worker is armed with the leader's (StatementArm)
                let mut rec = tagged();
                for s in ws {
                    let mut v = text(s);
                    trans_as(w as u16, h, 2, &mut rec, (!s.is_empty()).then(|| v.as_mut_ptr() as u64)).unwrap();
                    v.fill(0xee);
                }
                assert_eq!(rec[0], home, "the record still names the home");
            });
        }
    });
    let (mut rec, mut scratch) = (tagged(), vec![0u64; 64]); // alloc: Cold
    assert_eq!(text_of(fin(h, 2, &mut rec, &mut scratch).unwrap()), "zebra");
    assert_eq!(text_of(fin(h, 2, &mut rec, &mut scratch).unwrap()), "zebra", "gathered once, read again");
    let serial = AggArena::new_keyed(&[aggref(0, ARRAY_AGG_INT4, 1007, 2281, 23)], &[], 4).unwrap();
    assert!(!serial.parallel() && !serial.wide(), "no combinefn: the serial keyed arena");
    drop((arena, serial));
    assert_eq!(live_arenas(), before, "every slot freed on the leader");
}

/// D798: two partial records of one group in a keyed fold (a T1 eviction, width 1, no spill): `m6_agg_combine` combines
/// the second state into the first by the combinefn in the worker's slot of a serial arena too, a NULL partial is
/// skipped (strict text_larger), `from` is left holding no state; an aggregate without a combinefn is a named error.
#[test]
fn a_keyed_fold_combines_two_partials_of_one_group_through_m6_agg_combine() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new(&[aggref(2, MAX_TEXT, 25, 25, 25)], &[], 1).unwrap();
    assert!(!arena.parallel(), "width 1: one slot");
    let h = arena.handle();
    let fold = |ws: &[&str]| {
        let mut g = [0u64; 2];
        for s in ws {
            let mut v = text(s);
            trans(h, 2, &mut g, (!s.is_empty()).then(|| v.as_mut_ptr() as u64)).unwrap();
            v.fill(0xee);
        }
        g
    };
    let (mut first, mut second, mut nulls) = (fold(&["pear", "apple"]), fold(&["zebra", "kiwi"]), fold(&[""]));
    combine_as(0, h, 2, &mut first, &mut second).unwrap();
    assert_eq!(second, [0, 0], "the partial holds no state after the combine");
    combine_as(0, h, 2, &mut first, &mut nulls).unwrap();
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    assert_eq!(text_of(fin(h, 2, &mut first, &mut scratch).unwrap()), "zebra");
    let mut empty = [0u64; 2];
    let mut late = fold(&["fig"]);
    combine_as(0, h, 2, &mut empty, &mut late).unwrap();
    assert_eq!(text_of(fin(h, 2, &mut empty, &mut scratch).unwrap()), "fig", "an uninitialized state takes the partial's");
    let e = combine_as(1, h, 2, &mut first, &mut [0u64; 2]).unwrap_err();
    assert_eq!(e.message(), "gravity: a hosted aggregate called by worker 1 of an arena of 1");
    let mut arr = AggArena::new(&[aggref(0, ARRAY_AGG_INT4, 1007, 2281, 23)], &[], 1).unwrap();
    let (mut a, mut b) = ([0u64; 2], [0u64; 2]);
    trans(arr.handle(), 0, &mut a, Some(1)).unwrap();
    trans(arr.handle(), 0, &mut b, Some(2)).unwrap();
    let e = combine_as(0, arr.handle(), 0, &mut a, &mut b).unwrap_err();
    assert_eq!(e.message(), "gravity: two partial states of hosted aggregate 0 without its combinefn");
    drop((arena, arr));
    assert_eq!(live_arenas(), before);
}

/// D345: a kept column (aggno `naggs + i`) is the group's first input row's value, a NULL too: a by-reference text copied
/// into the aggregate context (the input's memory is the caller's), a by-value int4 as it is; later calls keep it; a group
/// with no row is NULL; two partials keep the earlier's, an empty one takes the other's; a kept column leaves the arena parallel.
#[test]
fn a_kept_column_is_the_groups_first_row_value() {
    let _g = install();
    let before = live_arenas();
    let mut arena = AggArena::new(&[aggref(0, MAX_TEXT, 25, 25, 25)], &[25, 23], 4).unwrap();
    let plain = AggArena::new(&[aggref(0, MAX_TEXT, 25, 25, 25)], &[], 4).unwrap().parallel();
    assert_eq!(arena.parallel(), plain, "a kept column leaves the arena's width as its aggregates make it (the combine keeps the earlier partial's row)");
    let h = arena.handle();
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    let (mut t, mut i, mut n) = ([0u64; 2], [0u64; 2], [0u64; 2]);
    for s in ["abc", "ABC", "zzz"] {
        let mut v = text(s);
        trans(h, 1, &mut t, Some(v.as_mut_ptr() as u64)).unwrap();
        v.fill(0xee);
    }
    assert_eq!(text_of(fin(h, 1, &mut t, &mut scratch).unwrap()), "abc");
    trans(h, 2, &mut i, Some(7)).unwrap();
    trans(h, 2, &mut i, Some(9)).unwrap();
    assert_eq!(fin_val(h, 2, &mut i), Some(7));
    trans(h, 2, &mut n, None).unwrap();
    trans(h, 2, &mut n, Some(5)).unwrap();
    assert_eq!(fin_val(h, 2, &mut n), None, "a first NULL is kept");
    assert_eq!(fin_val(h, 2, &mut [0u64; 2]), None, "a group with no row");
    let mut later = [0u64; 2];
    trans(h, 2, &mut later, Some(11)).unwrap();
    combine_as(0, h, 2, &mut i, &mut later).unwrap();
    assert_eq!(fin_val(h, 2, &mut i), Some(7), "the earlier partial's row");
    let (mut empty, mut other) = ([0u64; 2], [0u64; 2]);
    trans(h, 2, &mut other, Some(3)).unwrap();
    combine_as(0, h, 2, &mut empty, &mut other).unwrap();
    assert_eq!(fin_val(h, 2, &mut empty), Some(3));
    assert_eq!(trans(h, 3, &mut [0u64; 2], None).unwrap_err().message(), "gravity: no hosted aggregate 3");
    drop(arena);
    assert_eq!(live_arenas(), before);
}

/// D1192: a run nested inside an outer row (`Entry::out_of_order`: a lazy build's fed producers, an InitPlan's members)
/// calls on the leader under a nested hosted lane's ordinal (D1036: `NESTED_LANE_FIRST..=NESTED_LANE_TOP`); it folds,
/// combines and finalizes in the leader's slot (ordinal 0's), on a serial arena and on a parallel one after its workers'
/// section (the runner runs one section at a time; the leader's own context is never a claim's). An ordinal past the
/// arena's slots and below the nested lanes is still an error.
#[test]
fn a_nested_lanes_ordinal_folds_in_the_leaders_slot() {
    let _g = install();
    let before = live_arenas();
    let (first, top) = (gravity_driver::NESTED_LANE_FIRST, gravity_driver::NESTED_LANE_TOP);
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    let fold = |ordinal: u16, h: u64, ws: &[&str]| {
        let mut g = [0u64; 2];
        for s in ws {
            let mut v = text(s);
            trans_as(ordinal, h, 2, &mut g, (!s.is_empty()).then(|| v.as_mut_ptr() as u64)).unwrap();
            v.fill(0xee);
        }
        g
    };
    // serial: the nested lanes' folds and finals are the leader's (ordinal 0) values
    let mut serial = AggArena::new(&[aggref(2, MAX_TEXT, 25, 25, 25)], &[], 1).unwrap();
    assert!(!serial.parallel());
    let h = serial.handle();
    let (mut lead, mut nested, mut deeper) = (fold(0, h, &["pear", "apple"]), fold(top, h, &["pear", "", "apple"]), fold(first, h, &["fig"]));
    assert_eq!(text_of(fin_as(top, h, 2, &mut nested, &mut scratch).unwrap()), "pear");
    assert_eq!(text_of(fin_as(0, h, 2, &mut lead, &mut scratch).unwrap()), "pear");
    assert_eq!(text_of(fin_as(first, h, 2, &mut deeper, &mut scratch).unwrap()), "fig");
    let mut partial = fold(top, h, &["zebra"]);
    combine_as(top, h, 2, &mut lead, &mut partial).unwrap();
    assert_eq!(text_of(fin_as(top, h, 2, &mut lead, &mut scratch).unwrap()), "zebra");
    let e = trans_as(first - 1, h, 2, &mut [0u64; 2], None).unwrap_err();
    assert_eq!(e.message(), format!("gravity: a hosted aggregate called by worker {} of an arena of 1", first - 1));
    // parallel (width 4): the workers' section folds slots 0..3 on their threads and ends; then the nested run folds
    // and finalizes its own state in slot 0 on the leader; the merge of the workers' partials is unchanged
    let mut par = AggArena::new(&[aggref(2, MAX_TEXT, 25, 25, 25)], &[], 4).unwrap();
    assert!(par.parallel());
    let h = par.handle();
    let words = [["pear", "apple"], ["", "zebra"], ["", ""], ["kiwi", "fig"]];
    let states: Vec<[u64; 2]> = std::thread::scope(|sc| {
        let js: Vec<_> = words.iter().enumerate().map(|(w, ws)| sc.spawn(move || {
            pg_locale::set_default_locale_c_for_tests();
            fold(w as u16, h, ws)
        })).collect(); // alloc: Cold
        js.into_iter().map(|j| j.join().unwrap()).collect() // alloc: Cold
    });
    let mut nested = fold(top, h, &["kiwi", "", "banana"]);
    assert_eq!(text_of(fin_as(top, h, 2, &mut nested, &mut scratch).unwrap()), "kiwi");
    let mut into = states[0];
    for s in &states[1..] {
        unsafe { par.combine(2, into.as_mut_ptr().cast(), s.as_ptr().cast()) }.unwrap();
    }
    assert_eq!(text_of(fin_as(top, h, 2, &mut into, &mut scratch).unwrap()), "zebra");
    let e = trans_as(first - 1, h, 2, &mut [0u64; 2], None).unwrap_err();
    assert_eq!(e.message(), format!("gravity: a hosted aggregate called by worker {} of an arena of 4", first - 1));
    drop((serial, par));
    assert_eq!(live_arenas(), before);
}

/// D2942 (D1360's debug W8 sweep: 154 e2e files, a2_sorted_group_streams_sorting_aggs ...): a streamed sorted group's
/// next group resets the aggregate context with the handed-on group's by-reference state still in it, as nodeAgg.c's
/// ReScanExprContext does, so the context resets wholesale. Before, a debug build's leak check panicked there ("context
/// \"gravity agg arena\" reset with N bytes still charged"). D3578: each group's final is kept in the aggregate's own
/// buffer (`fin_kept`), overwritten by the next group's, never a copy into the Call's scratch per group.
#[test]
fn d2942_a_streamed_groups_next_group_resets_the_aggregate_context_wholesale() {
    let _g = install();
    let mut arena = AggArena::new(&[aggref(3, MAX_TEXT, 25, 25, 25)], &[], 1).unwrap();
    arena.streamed();
    let h = arena.handle();
    let mut scratch = vec![0u64; 64]; // alloc: Cold
    let mut at = None;
    for group in [["b", "a"], ["d", "c"], ["f", "e"]] {
        let mut g = [0u64; 2];
        for s in group {
            let mut v = text(s);
            trans(h, 3, &mut g, Some(v.as_mut_ptr() as u64)).unwrap();
        }
        let f = fin_kept(h, 3, &mut g, &mut scratch).unwrap();
        assert_eq!(text_of(f), group[0], "max(text) of the group");
        assert_eq!(*at.get_or_insert(f), f, "each group's final in the one buffer of its aggregate");
    }
}
