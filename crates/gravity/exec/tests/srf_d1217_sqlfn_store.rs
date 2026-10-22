//! D1217 (M6-PERF): a nested SQL function's rows (F5: `SELECT count(*), sum(..) FROM m5_getrngfunc4(1)`, its body served
//! nested) reached the function's tuplestore through a virtual slot and the receiver's dispatch (DestSink::send_row ->
//! receive_slot -> sqlfunction_receive, which copies each column through its junk map and then stores it). When the map is
//! the identity and the function keeps every row, DestSink stores each row's values the way sqlfunction_receive does
//! (tuplestore_putvalues over its clean descriptor), without the slot or the dispatch. A map that is not the identity still
//! goes through the receiver. D3865 L3: a function that keeps only its first row (D3628: in its junkfilter's slot,
//! `row_slot`) takes that row straight through its map into the slot, as sqlfunction_receive copies it; later rows are
//! ignored there too.
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gravity_exec::receivers::DestSink;
use mcx::MemoryContext;
use tcop_dest::DestReceiver;

static RECEIVED: AtomicUsize = AtomicUsize::new(0);

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok((typid == 23).then_some(types_tuple::PgTypeShape { typlen: 4, typbyval: true, typalign: b'i' as i8, typstorage: b'p' as i8, typcollation: 0 }))
        });
        // the receiver's own path (sql_functions registers it in the server): counted, stores nothing
        sql_functions_seams::sqlfunction_receive::set(|_, _| {
            RECEIVED.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        });
    });
}

/// (rows the receiver's dispatch took, rows in the function's store, the kept first row's cells) after three rows through a
/// DestSink.
fn run(map: &'static [i16], only_first: bool) -> (usize, i64, Option<Vec<(u64, bool)>>) {
    install();
    let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("d1217")));
    let mcx = cx.mcx();
    let mut d = tupdesc::CreateTemplateTupleDesc(mcx, 2).unwrap();
    tupdesc::TupleDescInitEntry(&mut d, 1, Some("a"), 23, -1, 0).unwrap();
    tupdesc::TupleDescInitEntry(&mut d, 2, Some("b"), 23, -1, 0).unwrap();
    let desc = Rc::new(d);
    let h = tuplestore::hold::register(tuplestore::Tuplestore::begin_heap(false, false, 1024));
    // D3628: a function keeping only its first row keeps it in its junkfilter's slot, not a tuplestore
    let row_slot = only_first.then(|| {
        let slot: &'static mut types_slot::SlotData<'static> = Box::leak(Box::new(exectuples::make_tuple_table_slot(
            mcx,
            types_slot::TupleSlotKind::Virtual,
            Some(desc.clone()),
        )));
        (std::ptr::NonNull::from(slot), mcx)
    });
    let kept_slot = row_slot.map(|(p, _)| p);
    let mut dest = DestReceiver::SqlFunction(sql_functions_seams::SqlFunctionDestState {
        tstore: h,
        clean_desc: desc.clone(),
        clean_map: map,
        row_slot,
        values: mcx::vec_with_capacity_in(mcx, 2).unwrap(),
        isnull: mcx::vec_with_capacity_in(mcx, 2).unwrap(),
    });
    let before = RECEIVED.load(Ordering::Relaxed);
    let mut sink = DestSink::new(mcx, desc, &mut dest);
    for (a, b) in [(1u64, 10u64), (2, 20), (3, 30)] {
        assert!(sink.send_row(&[a, b], &[0, 0]).unwrap());
    }
    let received = RECEIVED.load(Ordering::Relaxed) - before;
    // SAFETY: the leaked slot above, read after the sink's last row
    let kept = kept_slot.map(|p| {
        let b = unsafe { p.as_ref() }.base();
        (0..2).map(|i| (b.tts_values[i].as_u64(), b.tts_isnull[i])).collect()
    });
    (received, tuplestore::hold::with_store(h, |s| s.tuple_count()), kept)
}

#[test]
fn d1217_identity_map_stores_directly() {
    assert_eq!(run(&[1, 2], false), (0, 3, None), "an identity map: every row stored directly, none through the receiver");
    assert_eq!(run(&[2, 1], false), (3, 0, None), "a reordering map goes through the receiver");
    assert_eq!(run(&[1, 2], true), (0, 0, Some(vec![(1, false), (10, false)])), "a function keeping only its first row keeps it in its slot directly");
    assert_eq!(run(&[2, 0], true), (0, 0, Some(vec![(10, false), (0, true)])), "through its map: a reordered column, a dropped one's NULL");
}
