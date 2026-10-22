//! M2's TID and TABLESAMPLE hosts (hosts v4/m2_tid_sample.rs): the TID point list (sorted, deduplicated, out-of-range and
//! NULL keys dropped), the TID range, EvalPlanQual's recheck, `WHERE CURRENT OF` through the cursor's position, and
//! TABLESAMPLE's REPEATABLE determinism and its 0 % / 100 % ends.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::tid_sample::{TidClause, TidRecipe, TidSource, TsmFilter};
use gravity_hosts::{HeapRangeSource, RangeRecipe};
use mcx::MemoryContext;
use types_nodes::primnodes::CurrentOfExpr;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_tuple::ItemPointerData;

static FLAG: AtomicU8 = AtomicU8::new(0);
const MAX_LANES: usize = 8;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

/// The stub cursor's position (the tests run serially under `serial()`).
/// The open cursors the stub execCurrentOf answers: a name and the row it is on (`None`: not on a row of this table).
static CURSORS: std::sync::Mutex<Vec<(&'static str, Option<ItemPointerData>)>> = std::sync::Mutex::new(Vec::new());

fn install_tid() {
    install();
    if !execmain_seams::exec_current_of::is_installed() {
        execmain_seams::exec_current_of::set(|name, _param, _rel, _relname| {
            let name = name.expect("the host names the cursor (D422)");
            match CURSORS.lock().unwrap().iter().find(|c| c.0 == name) {
                Some(c) => Ok(c.1),
                None => Err(Box::new(types_error::PgError::error(format!("cursor \"{name}\" does not exist")).with_sqlstate(types_error::ERRCODE_UNDEFINED_CURSOR))),
            }
        });
    }
}

/// Every row's value after a seek over `keys` (key cells: pointers to tid images; `u64::MAX` = NULL).
fn drain(src: &mut dyn Source, keys: &[u64]) -> Vec<i32> {
    let nulls: Vec<u8> = keys.iter().map(|&k| (k == u64::MAX) as u8).collect();
    src.seek(keys, &nulls).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut out = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return out;
        }
        out.extend((0..n as usize).map(|i| unsafe { w.lanes()[0].datum(i) } as u32 as i32));
    }
}

fn tid_source(mcx: mcx::Mcx<'static>, clauses: Vec<TidClause>, range: bool, snap: &Rc<SnapshotData<'static>>) -> TidSource {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut s = TidSource::new(TidRecipe::from_plan(&heap, &[1], clauses, range).unwrap());
    s.bind(&binding_ref(snap)).unwrap();
    s
}

#[test]
fn tid_points_ranges_recheck_and_current_of() {
    let _g = serial();
    install_tid();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-tid")));
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &(0..800).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let at = |t: &ItemPointerData| t as *const ItemPointerData as u64;
    let far = ItemPointerData::new(10_000, 1);
    let mut pts = tid_source(mcx, vec![TidClause::Eq, TidClause::Eq, TidClause::Eq, TidClause::Eq], false, &snap);
    assert_eq!(drain(&mut pts, &[at(&tids[700]), at(&tids[3]), at(&tids[700]), at(&far)]), vec![3, 700], "sorted, deduplicated, the block past the relation dropped");
    assert_eq!(drain(&mut pts, &[at(&tids[9]), u64::MAX, u64::MAX, u64::MAX]), vec![9], "a NULL point matches nothing");
    // EvalPlanQual: a substituted tid the list holds is served; one it does not hold is not
    pts.substitute(gravity_hosts::ops::scan::index_source::pos_of(HEAP_OID, &tids[9]));
    assert_eq!(drain(&mut pts, &[at(&tids[9]), u64::MAX, u64::MAX, u64::MAX]), vec![9]);
    drain(&mut pts, &[at(&tids[9]), u64::MAX, u64::MAX, u64::MAX]);
    pts.substitute(gravity_hosts::ops::scan::index_source::pos_of(HEAP_OID, &tids[10]));
    assert_eq!(pts.window(&mut Window::over(&mut blank_site([Lane::NONE; MAX_LANES].as_mut_ptr()))).unwrap(), 0);
    pts.unbind();
    let mut range = tid_source(mcx, vec![TidClause::Lower { inclusive: false }, TidClause::Upper { inclusive: true }], true, &snap);
    assert_eq!(drain(&mut range, &[at(&tids[100]), at(&tids[105])]), (101..=105).collect::<Vec<i32>>());
    assert_eq!(drain(&mut range, &[u64::MAX, at(&tids[105])]), Vec::<i32>::new(), "a NULL bound: no rows");
    range.unbind();
    // EvalPlanQual on a source that never scanned: the recheck evaluates the bounds first (TidRangeRecheck), so a row
    // inside them is served and one outside is not
    let mut fresh = tid_source(mcx, vec![TidClause::Lower { inclusive: true }, TidClause::Upper { inclusive: false }], true, &snap);
    fresh.substitute(gravity_hosts::ops::scan::index_source::pos_of(HEAP_OID, &tids[102]));
    assert_eq!(drain(&mut fresh, &[at(&tids[100]), at(&tids[105])]), vec![102], "a fresh recheck source holds its range");
    fresh.substitute(gravity_hosts::ops::scan::index_source::pos_of(HEAP_OID, &tids[300]));
    let mut site = blank_site([Lane::NONE; MAX_LANES].as_mut_ptr());
    fresh.seek(&[at(&tids[100]), at(&tids[105])], &[0, 0]).unwrap();
    assert_eq!(fresh.window(&mut Window::over(&mut site)).unwrap(), 0, "outside the range: not held");
    fresh.unbind();
    // D422: one source (one cached program's recipe) serves every cursor: the key cell names this execution's
    let mut cur = tid_source(mcx, vec![TidClause::Current { by_param: false, param: 0 }], false, &snap);
    let (a, b) = (CurrentOfExpr { cvarno: 1, cursor_name: Some("a"), cursor_param: 0 }, CurrentOfExpr { cvarno: 1, cursor_name: Some("b"), cursor_param: 0 });
    let word = |c: &CurrentOfExpr<'_>| c as *const CurrentOfExpr<'_> as u64;
    *CURSORS.lock().unwrap() = vec![("a", Some(tids[42])), ("b", Some(tids[7]))];
    assert_eq!(drain(&mut cur, &[word(&a)]), vec![42], "cursor a's row");
    assert_eq!(drain(&mut cur, &[word(&b)]), vec![7], "cursor b's row through the same recipe");
    *CURSORS.lock().unwrap() = vec![("b", Some(tids[7]))];
    cur.seek(&[word(&a)], &[0]).expect_err("cursor a is closed");
    let e = gravity_hosts::take_error().expect("the host stashes execCurrentOf's error");
    assert_eq!((e.message(), e.sqlstate()), ("cursor \"a\" does not exist", types_error::ERRCODE_UNDEFINED_CURSOR));
    *CURSORS.lock().unwrap() = vec![("a", None)];
    assert_eq!(drain(&mut cur, &[word(&a)]), Vec::<i32>::new(), "a cursor not on a row of this table: no row");
    cur.unbind();
    // the refcursor parameter form: the key cell is the parameter's text value
    let mut by_param = tid_source(mcx, vec![TidClause::Current { by_param: true, param: 1 }], false, &snap);
    let text = |s: &str| -> Vec<u8> {
        let mut v = (((s.len() + 4) as u32) << 2).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    };
    let (ta, tb) = (text("a"), text("b"));
    *CURSORS.lock().unwrap() = vec![("a", Some(tids[42])), ("b", Some(tids[7]))];
    assert_eq!(drain(&mut by_param, &[ta.as_ptr() as u64]), vec![42], "refcursor a");
    assert_eq!(drain(&mut by_param, &[tb.as_ptr() as u64]), vec![7], "refcursor b");
    by_param.seek(&[u64::MAX], &[1]).expect_err("a refcursor parameter without a value");
    let e = gravity_hosts::take_error().expect("the host stashes fetch_cursor_param_value's error");
    assert_eq!((e.message(), e.sqlstate()), ("no value found for parameter 1", types_error::ERRCODE_UNDEFINED_OBJECT));
    by_param.unbind();
    CURSORS.lock().unwrap().clear();
    quiesced();
}

#[test]
fn tablesample_is_repeatable_and_bounded() {
    let _g = serial();
    install_tid();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-sample")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..2000).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let heap = Relation::open(heap_relation_data(mcx), None);
    let run = |tsm: tablesample::Tsm, pct: f32, seed: f64| {
        let mut s = HeapRangeSource::with_sample(RangeRecipe::from_plan(&heap, &[1]).unwrap(), TsmFilter::new(tsm, true));
        s.bind(&binding_ref(&snap)).unwrap();
        let r = drain(&mut s, &[pct.to_bits() as u64, seed.to_bits()]);
        s.unbind();
        r
    };
    for tsm in [tablesample::Tsm::Bernoulli, tablesample::Tsm::System] {
        let a = run(tsm, 30.0, 7.0);
        assert_eq!(a, run(tsm, 30.0, 7.0), "{tsm:?}: REPEATABLE gives the same rows");
        assert!(!a.is_empty() && a.len() < 2000, "{tsm:?}: a proper sample, got {}", a.len());
        assert_eq!(run(tsm, 100.0, 1.0).len(), 2000, "{tsm:?}: 100 % is every row");
        assert!(run(tsm, 0.0, 1.0).is_empty(), "{tsm:?}: 0 % is none");
    }
    let mut s = HeapRangeSource::with_sample(RangeRecipe::from_plan(&heap, &[1]).unwrap(), TsmFilter::new(tablesample::Tsm::Bernoulli, true));
    s.bind(&binding_ref(&snap)).unwrap();
    assert!(s.seek(&[0, 0], &[1, 0]).is_err(), "a NULL argument is PostgreSQL's error");
    assert!(s.seek(&[0, 0], &[0, 1]).is_err(), "a NULL REPEATABLE too");
    s.unbind();
    quiesced();
}
