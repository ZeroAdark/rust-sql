//! M5 H-c / H-d / H-e (cards/M5.md L2 `hosts/src/v4/m5.rs`): the incumbent FunctionScan, TableFuncScan and
//! NamedTuplestoreScan (`ScanStore`) read through the store reader's entries (kernels host_abi v4/m5.rs `store_batch` / `store_rewind`). Per plan: the windows equal the rows PG's node
//! produces for the same call (its access method's rows: the node's qual and projection are the reader's, not the
//! host's), across window boundaries; a by-reference datum stays valid after the store's next read (copied); a rewind
//! rereads the same rows without re-calling; the guards (window shape, read pointer, a stale handle) are errors.
use std::sync::Once;

use gravity_decompose::corpus;
use gravity_hosts::ops::srf::store::ScanStore;
use gravity_kernels::ops::srf::entries::{store_batch, store_rewind, win_valid, STORE_END, STORE_FAILED, WIN_N, WIN_VALUES};
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;
use types_portal::{ParamListHandle, QueryEnvHandle};

static SEAMS: Once = Once::new();

/// The builtin catalog and the seams the builtin set functions read (gravity_fn's tests/common `install_seams`).
fn install() {
    // the default locale is per thread (each test's)
    pg_locale::set_default_locale_c_for_tests();
    SEAMS.call_once(|| {
        // SAFETY: single-threaded test init, before any getenv.
        unsafe { std::env::set_var("PGRUST_TZDIR", "/usr/share/zoneinfo") };
        seams_init::register_builtin_tables();
        init_small::init_seams();
        mbutils::init_seams();
        adt_float::init_seams();
        pgtz::init_seams();
        adt_timestamp::init_seams();
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        pg_locale::init_seams();
        detoast::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        // ExecInitFunctionScan's EXECUTE check (init_sexpr): the bootstrap superuser, every function executable
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::object_aclcheck::set(|_, _, _, _| Ok(0));
        // the fmgr set's check (H-b): its name (the NESTED list)
        syscache_seams::pg_proc_proname::set(|_| Ok(None));
        // ExecInitFunctionScan's result type (get_expr_result_type): the fixtures' functions' pg_proc rows
        syscache_seams::lookup_pg_proc_shape::set(|funcid| {
            // (prorettype, proretset, pronargs, proisstrict, provolatile)
            let (rettype, retset, nargs, strict, vol) = match funcid {
                1067 => (23, true, 2, true, b'i'),    // generate_series(int4, int4)
                2331 => (2283, true, 1, true, b'i'),  // unnest(anyarray)
                6160 => (25, true, 2, false, b'i'),   // string_to_table(text, text)
                1299 => (1184, false, 0, true, b's'), // now()
                3219 => (3802, true, 1, true, b'i'),  // jsonb_array_elements(jsonb)
                _ => return Ok(None),
            };
            Ok(Some(syscache_seams::PgProcShape {
                pronamespace: 11,
                prorettype: rettype,
                provariadic: 0,
                prosupport: 0,
                prolang: 12,
                pronargs: nargs,
                prokind: b'f' as i8,
                provolatile: vol as i8,
                proparallel: b's' as i8,
                proretset: retset,
                proisstrict: strict,
                proleakproof: false,
                prosecdef: false,
                proconfig_isnull: true,
            }))
        });
        // the fixtures' column types (TupleDescInitEntry, get_typtype): (typlen, typbyval, typalign, typstorage, collation)
        fn ty(typid: u32) -> Option<(i16, bool, u8, u8, u32)> {
            Some(match typid {
                20 | 1184 => (8, true, b'd', b'p', 0), // int8, timestamptz
                23 => (4, true, b'i', b'p', 0),        // int4
                25 => (-1, false, b'i', b'x', 100),    // text
                3802 | 114 => (-1, false, b'i', b'x', 0), // jsonb, json
                _ => return None,
            })
        }
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(ty(typid).map(|(typlen, typbyval, align, storage, typcollation)| types_tuple::PgTypeShape {
                typlen,
                typbyval,
                typalign: align as i8,
                typstorage: storage as i8,
                typcollation,
            }))
        });
        // JSON_TABLE's column coercion (the column type's input function): int4, text
        syscache_seams::pg_type_io_shape::set(|typid| {
            let (typinput, typoutput, typreceive, typsend) = match typid {
                23 => (42, 43, 2406, 2407),
                25 => (46, 47, 2414, 2415),
                _ => return Ok(None),
            };
            let (typlen, typbyval, align, _, _) = ty(typid).unwrap();
            Ok(Some(syscache_seams::PgTypeIoShape { oid: typid, typinput, typoutput, typreceive, typsend, typmodin: 0, typmodout: 0, typelem: 0, typlen, typbyval, typalign: align as i8, typdelim: b',' as i8, typisdefined: true }))
        });
        syscache_seams::pg_type_category::set(|typid| {
            Ok(match typid {
                20 | 23 => Some((b'N' as i8, false)),
                25 => Some((b'S' as i8, true)),
                1184 => Some((b'D' as i8, true)),
                114 | 3802 => Some((b'U' as i8, false)),
                _ => None,
            })
        });
        // anyelement / record are pseudo-types; the rest base types
        syscache_seams::pg_type_typtype::set(|typid| Ok(if matches!(typid, 2283 | 2249) { Some(b'p' as i8) } else { ty(typid).map(|_| b'b' as i8) }));
        adt_datetime::tz::pg_timezone_initialize().unwrap();
    });
}

/// A value as the test reads it: a by-value word, or a varlena's payload text.
#[derive(Debug, PartialEq)]
enum V {
    Null,
    W(i64),
    T(String),
}

/// The plan fixture's first node `is` accepts (the main tree, then the subplans).
fn find(n: Option<Node<'static>>, is: fn(Node<'static>) -> bool) -> Option<Node<'static>> {
    let n = n?;
    if is(n) {
        return Some(n);
    }
    let p = n.as_plan()?;
    find(p.lefttree, is).or_else(|| find(p.righttree, is)).or_else(|| n.as_subquery_scan().and_then(|s| find(s.subplan, is)))
}

/// A plan fixture's statement.
fn load(rel: &str) -> &'static PlannedStmt<'static> {
    let path = manifest_dir().join("../decompose/tests/plans").join(rel);
    load_text(std::fs::read_to_string(&path).unwrap())
}

/// A plan text's statement (leaked: the host keeps `'static` references, as the portal's).
fn load_text(text: String) -> &'static PlannedStmt<'static> {
    install();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m5 hosts fixture")));
    let text: &'static str = Box::leak(text.into_boxed_str());
    corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("{e}")).stmt
}

/// The plan fixture's FunctionScan, and its statement.
fn fixture(rel: &str) -> (&'static PlannedStmt<'static>, Node<'static>) {
    let pstmt = load(rel);
    let scan = pstmt.planTree.filter(|n| n.as_function_scan().is_some()).unwrap_or_else(|| panic!("{rel}: the plan's root is not a FunctionScan"));
    (pstmt, scan)
}

fn store(pstmt: &'static PlannedStmt<'static>, scan: Node<'static>) -> Box<ScanStore> {
    ScanStore::new(pstmt, scan, ParamListHandle::NULL, QueryEnvHandle::NULL).expect("a FunctionScan is a scan store")
}

/// Every window of reader 0 read to the end, `max` rows each; `text` marks the by-reference (text) columns.
fn drain(h: u64, max: usize, natts: usize, text: &[bool]) -> (Vec<Vec<V>>, usize) {
    let (mut rows, mut windows) = (Vec::new(), 0);
    loop {
        let w = unsafe { store_batch(0, h, 0, max as u64, natts as u64, u64::MAX) };
        assert_ne!(w, STORE_FAILED, "{:?}", gravity_fn::take_error().map(|e| e.message().to_owned()));
        if w == STORE_END {
            return (rows, windows);
        }
        windows += 1;
        let p = w as *const u8;
        let n = unsafe { p.add(WIN_N).cast::<u64>().read() } as usize;
        assert!(n >= 1 && n <= max);
        for j in 0..n {
            rows.push(
                (0..natts)
                    .map(|c| unsafe {
                        let valid = p.add(win_valid(natts, max)).cast::<u64>().add(c * max / 64 + j / 64).read() >> (j % 64) & 1 != 0;
                        let d = p.add(WIN_VALUES + (c * max + j) * 8).cast::<i64>().read();
                        match (valid, text[c]) {
                            (false, _) => V::Null,
                            (true, false) => V::W(d),
                            (true, true) => V::T(payload(d)),
                        }
                    })
                    .collect(),
            );
        }
    }
}

fn payload(d: i64) -> String {
    let p = d as usize as *const u8;
    // SAFETY: a varlena copy in the reader's region.
    unsafe {
        let (hdr, n) = if *p & 1 == 1 { (1, (*p >> 1) as usize) } else { (4, (p.cast::<u32>().read_unaligned() >> 2) as usize) };
        String::from_utf8(std::slice::from_raw_parts(p.add(hdr), n - hdr).to_vec()).unwrap()
    }
}

fn w(vs: &[i64]) -> Vec<Vec<V>> {
    vs.iter().map(|&v| vec![V::W(v)]).collect()
}

#[test]
fn a_simple_scan_reads_every_row_of_the_call_and_rewinds_without_recalling() {
    // srf-014: `generate_series(1, 5) g(i) WHERE i > 2`: the host hands the node's rows; the qual is the reader's
    let (pstmt, scan) = fixture("srf-014.plan");
    let mut s = store(pstmt, scan);
    let h = s.handle();
    let (rows, windows) = drain(h, 64, 1, &[false]);
    assert_eq!((rows, windows), (w(&[1, 2, 3, 4, 5]), 1));
    assert_eq!(unsafe { store_batch(0, h, 0, 64, 1, 1) }, STORE_END, "an ended store stays ended");
    assert_eq!(unsafe { store_rewind(0, h, 0) }, 0);
    assert_eq!(drain(h, 64, 1, &[false]).0, w(&[1, 2, 3, 4, 5]), "a rewind rereads the store");
}

#[test]
fn a_one_function_store_is_read_directly_and_equals_the_nodes_own_scan() {
    // srf-014 at 200 rows (one function, no ORDINALITY): the reader reads the node's store without the node (C2); four
    // 64-row windows, a rewind and the whole again equal the rows the node's own `scan_next` hands
    let path = manifest_dir().join("../decompose/tests/plans/srf-014.plan");
    let text = std::fs::read_to_string(path).unwrap().replace(":constvalue 4 [ 5 0 0 0 0 0 0 0 ]", ":constvalue 4 [ 200 0 0 0 0 0 0 0 ]");
    let pstmt = load_text(text);
    let scan = pstmt.planTree.expect("a FunctionScan");
    let cx = MemoryContext::new("m5 c2 reference");
    let mut state = executils::create_executor_state(&cx).unwrap();
    let want: Vec<Vec<V>> = state.with_mut(|es| {
        // SAFETY: the leaked plan outlives the state.
        let f = unsafe { &*(scan.as_function_scan().unwrap() as *const types_nodes::plannodes::FunctionScan<'static>).cast() };
        let mut node = nodefunctionscan::exec_init_function_scan(es.es_query_cxt, f, es, types_slot::EXEC_FLAG_REWIND).unwrap();
        assert!(node.is_simple());
        let mut rows = Vec::new();
        while execscan::ScanNode::scan_next(&mut node, es).unwrap() {
            let slot = es.slot_mut(execscan::ScanNode::ss_mut(&mut node).ss_ScanTupleSlot);
            exectuples::slot_getsomeattrs(slot, 1);
            rows.push(vec![V::W(slot.base().tts_values[0].as_i64())]);
        }
        nodefunctionscan::exec_end_function_scan(&mut node);
        rows
    });
    executils::free_executor_state(state);
    assert_eq!(want.len(), 200);
    let mut s = store(pstmt, scan);
    let h = s.handle();
    let (rows, windows) = drain(h, 64, 1, &[false]);
    assert_eq!((&rows, windows), (&want, 4));
    assert_eq!(unsafe { store_rewind(0, h, 0) }, 0);
    assert_eq!(drain(h, 64, 1, &[false]), (want, 4), "a rewind rereads the same store");
}

#[test]
fn rows_from_pads_the_shorter_member_and_copies_its_text() {
    // srf-018: ROWS FROM (generate_series(1, 3), unnest(ARRAY['a', 'b'])): the general path, NULL-padded
    let (pstmt, scan) = fixture("srf-018.plan");
    let mut s = store(pstmt, scan);
    let (rows, _) = drain(s.handle(), 64, 2, &[false, true]);
    let t = |x: &str| V::T(x.to_owned());
    assert_eq!(rows, vec![vec![V::W(1), t("a")], vec![V::W(2), t("b")], vec![V::W(3), V::Null]]);
}

#[test]
fn ordinality_is_the_last_column_and_text_rows_stand_for_their_window() {
    // srf-015: generate_series(10, 20) WITH ORDINALITY: the ordinal as the node composes it
    let (pstmt, scan) = fixture("srf-015.plan");
    let mut s = store(pstmt, scan);
    let want: Vec<Vec<V>> = (10..=20).map(|g| vec![V::W(g), V::W(g - 9)]).collect();
    assert_eq!(drain(s.handle(), 64, 2, &[false, false]), (want, 1));
    // srf-021: string_to_table('a b c', ' '): each row's text is read after the store moved past it (copied)
    let (pstmt, scan) = fixture("srf-021.plan");
    let mut s = store(pstmt, scan);
    let t = |x: &str| vec![V::T(x.to_owned())];
    assert_eq!(drain(s.handle(), 64, 1, &[true]).0, vec![t("a"), t("b"), t("c")]);
}

#[test]
fn a_scalar_function_in_from_is_one_row() {
    // srf-028: `now()` in FROM: one row, the statement timestamp (by value)
    let (pstmt, scan) = fixture("srf-028.plan");
    let mut s = store(pstmt, scan);
    assert_eq!(drain(s.handle(), 64, 1, &[false]).0.len(), 1);
}

#[test]
fn a_table_function_fills_at_the_first_window() {
    // regress-060 json_table('[{"a": 1}, {"a": 2}]', '$[*]' COLUMNS (a int PATH '$.a')); regress-061 xmltable('/r/e'
    // PASSING '<r><e>1</e></r>' COLUMNS v int PATH '.'): the routine's rows (H-d)
    for (rel, rows) in [("regress-060.plan", &[1, 2][..]), ("regress-061.plan", &[1][..])] {
        let pstmt = load(rel);
        let scan = find(pstmt.planTree, |n| n.as_table_func_scan().is_some()).expect("a TableFuncScan");
        let mut s = ScanStore::new(pstmt, scan, ParamListHandle::NULL, QueryEnvHandle::NULL).expect("a scan store");
        let h = s.handle();
        // M5-STORE's open rewinds every time, the first included: a scan never run rescans as nothing
        assert_eq!(unsafe { store_rewind(0, h, 0) }, 0);
        assert_eq!(drain(h, 64, 1, &[false]).0, w(rows), "{rel}");
        assert_eq!(unsafe { store_rewind(0, h, 0) }, 0);
        assert_eq!(drain(h, 64, 1, &[false]).0, w(rows), "{rel}: a rewind rereads the filled store");
    }
}

#[test]
fn a_named_tuplestore_reads_this_executions_enr() {
    // regress-069.1's NamedTuplestoreScan over `newrows` (a transition table), its ENR registered as a trigger's is (H-e)
    let pstmt = load("regress-069.1.plan");
    let scan = find(pstmt.planTree, |n| n.as_named_tuplestore_scan().is_some())
        .or_else(|| pstmt.subplans.iter().find_map(|p| find(p, |n| n.as_named_tuplestore_scan().is_some())))
        .expect("a NamedTuplestoreScan");
    let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m5 hosts enr")));
    let mcx = cx.mcx();
    let mut d = tupdesc::CreateTemplateTupleDesc(mcx, 1).unwrap();
    tupdesc::TupleDescInitEntry(&mut d, 1, Some("x"), 23, -1, 0).unwrap();
    let desc = std::rc::Rc::new(d);
    let mut store = tuplestore::Tuplestore::begin_heap(false, false, 1024);
    for v in [7, 8, 9] {
        store.putvalues(&desc, &[datum::Datum::from_i32(v)], &[false]).unwrap();
    }
    let reldata = tuplestore::hold::register(store);
    let mut env = queryenvironment::create_queryEnv(mcx);
    let md = queryenvironment::EphemeralNamedRelationMetadataData { name: mcx::PgString::from_str_in("newrows", mcx).unwrap(), reliddesc: 0, tupdesc: Some(desc), enrtype: queryenvironment::ENR_NAMED_TUPLESTORE, enrtuples: 3.0 };
    queryenvironment::register_ENR(&mut env, queryenvironment::EphemeralNamedRelationData { md, reldata }).unwrap();
    let qenv = queryenvironment::hold::register(env);
    let mut s = ScanStore::new(pstmt, scan, ParamListHandle::NULL, qenv).expect("a scan store");
    let h = s.handle();
    assert_eq!(drain(h, 64, 1, &[false]).0, w(&[7, 8, 9]));
    assert_eq!(unsafe { store_rewind(0, h, 0) }, 0);
    assert_eq!(drain(h, 64, 1, &[false]).0, w(&[7, 8, 9]), "the reader's own pointer rewinds");
    drop(s);
    // the store is the registrant's: the reader never ended it
    assert_eq!(tuplestore::hold::with_store(reldata, |ts| ts.tuple_count()), 3);
    let mut none = ScanStore::new(pstmt, scan, ParamListHandle::NULL, QueryEnvHandle::NULL).expect("a scan store");
    assert_eq!(unsafe { store_batch(0, none.handle(), 0, 64, 1, 1) }, STORE_FAILED, "no ENR by that name: PG's error");
    let _ = gravity_fn::take_error();
}

#[test]
fn the_reader_guards_are_errors_not_reads() {
    let (pstmt, scan) = fixture("srf-014.plan");
    let mut s = store(pstmt, scan);
    let h = s.handle();
    let fails = |r: u64, msg: &str| {
        assert_eq!(r, STORE_FAILED);
        assert_eq!(gravity_fn::take_error().map(|e| e.message().to_owned()).as_deref(), Some(msg));
    };
    fails(unsafe { store_batch(0, h, 0, 60, 1, 1) }, "gravity: a store window of rows not a multiple of 64");
    fails(unsafe { store_batch(0, h, 1, 64, 1, 1) }, "gravity: a scan store has one reader (read pointer 0)");
    fails(unsafe { store_batch(0, h, 0, 64, 2, 1) }, "gravity: a store window wider than the scan's row");
    fails(unsafe { store_batch(0, 0, 0, 64, 1, 1) }, "gravity: a store reader over no live store");
    fails(unsafe { store_rewind(0, h, 3) }, "gravity: a scan store has one reader (read pointer 0)");
}

#[test]
fn a_project_sets_fmgr_set_is_built_at_its_first_open_and_runs_the_call() {
    use gravity_fn::srf_fmgr::{self, FmgrSet, Sets};
    // srf-011: SELECT jsonb_array_elements('[1, 2]'::jsonb): the ProjectSet's entry 1 through the fmgr set protocol (H-b)
    let pstmt = load("srf-011.plan");
    let ps = find(pstmt.planTree, |n| n.as_project_set().is_some()).expect("a ProjectSet");
    let under = ps.as_plan().and_then(|p| p.lefttree).expect("the ProjectSet's input");
    assert!(Sets::new(under).is_none(), "only a ProjectSet has fmgr sets");
    let mut sets = Sets::new(ps).expect("a ProjectSet's sets");
    let err = |r: Result<&mut FmgrSet, Box<types_error::PgError>>| r.err().map(|e| e.message().to_owned());
    assert_eq!(err(sets.get(2)).as_deref(), Some("gravity: no ProjectSet entry for an fmgr set"));
    let first = sets.get(1).expect("entry 1's set") as *mut FmgrSet;
    assert_eq!(sets.get(1).unwrap() as *mut FmgrSet, first, "built once, at the first use");
    // the call's argument as the IR places it in the cell: a copy of the plan's constant, scribbled over after the open
    let te = ps.as_plan().unwrap().targetlist.iter().find_map(|n| n.as_target_entry()).unwrap();
    let konst = te.expr.as_func_expr().unwrap().args.iter().next().and_then(|a| a.as_const()).unwrap().constvalue.as_usize() as *const u8;
    let len = unsafe { types_tuple::varatt::varsize_any(konst) };
    let mut arg = vec![0u64; len.div_ceil(8)];
    let (mut state, mut e) = (vec![0u64; 64], gravity_fn::ErrSlot::NONE);
    let mut open = |arg: &mut Vec<u64>, state: &mut Vec<u64>| {
        unsafe { core::ptr::copy_nonoverlapping(konst, arg.as_mut_ptr().cast::<u8>(), len) };
        let st = unsafe { srf_fmgr::open(&mut e, core::ptr::null_mut(), state.as_mut_ptr().cast(), [arg.as_ptr() as i64].as_ptr(), [0u8].as_ptr(), 1, first as i64) };
        assert_eq!(st, gravity_fn::OK);
        arg.fill(0);
    };
    let step = |state: &mut Vec<u64>| {
        let (mut out, mut e) = ([0i64; 4], gravity_fn::ErrSlot::NONE);
        let r = unsafe { srf_fmgr::step(&mut e, core::ptr::null_mut(), state.as_mut_ptr().cast(), out.as_mut_ptr()) };
        assert_eq!(out[1], 0, "not null");
        // SAFETY: a jsonb image in the slot's context
        (r == 1).then(|| unsafe { std::slice::from_raw_parts(out[0] as usize as *const u8, types_tuple::varatt::varsize_any(out[0] as usize as *const u8)).to_vec() })
    };
    open(&mut arg, &mut state);
    let one = step(&mut state).expect("a first row; then the reader stops inside the set");
    // a re-open ends the stopped set (its cross-call state, `ShutdownSetExpr`) and runs the call again from its first row
    open(&mut arg, &mut state);
    let rows: Vec<_> = std::iter::from_fn(|| step(&mut state)).collect();
    assert_eq!(rows.len(), 2, "jsonb_array_elements('[1, 2]') is two rows after a re-open");
    assert_eq!(rows[0], one, "the re-opened call starts again at its first row");
    assert_ne!(rows[0], rows[1]);
    // the normal end (`Sets::end`: exec's `Live::end`, ExecutorEnd's `ShutdownSetExpr`) ends a set stopped inside
    open(&mut arg, &mut state);
    assert!(step(&mut state).is_some());
    sets.end().expect("the normal end");
    assert_eq!(step(&mut state), None, "an ended set has no next row");
    gravity_fn::srf::release_slots(0, 0, 1);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
