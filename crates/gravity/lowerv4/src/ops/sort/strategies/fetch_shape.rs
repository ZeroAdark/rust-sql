//! Props: road topk.fetch's shape (V4R-126; one predicate, read by props for admission and by runs/fetch.rs). It replaces
//! the UNSEEN `select_star` token, which every graph carried: a narrow top keeps its rows by value, so the fetch form
//! applies only where its mechanism acts, by cost (`fetch_pays`). The ladder's column counts (v2 `source/emit_fetch.rs`
//! FETCH_CODES / FETCH_COLS) stay only where the pipeline has no estimate of the rows its scan reads to price the by-value
//! form (`NO_ESTIMATE`): no row estimate, or a filter before the top (the estimate counts the rows past it).
//! M3-TOPS.
use gravity_abi::ctx::lane;
use gravity_abi::site::MAX_FETCH_ROWS;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::{Am, Bound, Op, PipelineNode, Source};
use gravity_program::cost::anchor::{self, Anchor, Ratios};
use gravity_program::cost::fit::Fitted;
use gravity_program::{LaneId, Rep};
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::list::NodeList;
use types_nodes::primnodes::TargetEntry;

const MACHINE: &str = "box 56: c8g, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev corpus (never a benchmark's tables or the census's): md5-shuffled pgrcolumnar tables of 5,000, 20,000, 50,000,
/// 200,000 and 1,000,000 rows, an int8 hash key, int8 entries (0 - 40) and text entries (low-NDV and unique, code lanes
/// both: 0 - 16) beside it, LIMIT 10 / 100 / 1000, each top timed in both forms (median of 5) on one binary at W0; least
/// squares per form and lane kind (non-negative); a 1,000,000-row table with the key in load order held out.
const CORPUS: &str = "sort fetch-late sweep (s5 / s20 / s50 / fw2 / fw10, held-out fs10)";

/// A decode or a lane's or site's fixed cost moves with the read anchor, a candidate's copy with the update anchor (a store
/// into the run), a kept row's late fetch and locator lookup with the gather anchor (reads at the kept rows' positions).
const fn ns(ns: f64, anchor: Anchor) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor }
}

/// One entry's unit costs beside the keys, by its scan lane: kept by value, the scan decodes it for every row it reads
/// and the run copies it for every admitted candidate; fetched late, the site reads it for each kept row, plus a fixed
/// cost per fetched lane.
struct Unit {
    decode: Fitted,
    copy: Fitted,
    fetch: Fitted,
    lane: Fitted,
}

const WORD: Unit = Unit { decode: ns(0.277, Anchor::Read), copy: ns(20.3, Anchor::Update), fetch: ns(115.0, Anchor::Gather), lane: ns(5_660.0, Anchor::Read) };
/// A by-reference entry: a code, a varlena (an earlier sweep's 1,000,000 unique texts: copy 100 - 370 ns per admitted
/// candidate, 0.42 us per kept row fetched) or a fixed image (not swept).
const BYREF: Unit = Unit { decode: ns(0.0, Anchor::Read), copy: ns(167.0, Anchor::Update), fetch: ns(255.0, Anchor::Gather), lane: ns(11_400.0, Anchor::Read) };

/// A relation kind's unit costs: its entries' (`word`, `byref`) and its fetch site's own, per statement (`site`) and per
/// kept row (`row`: the locator's request and lookup).
struct Units {
    word: Unit,
    byref: Unit,
    site: Fitted,
    row: Fitted,
}

const PGRC: Units = Units { word: WORD, byref: BYREF, site: ns(76_000.0, Anchor::Read), row: ns(289.0, Anchor::Gather) };

const HEAP_MACHINE: &str = "box 15: Graviton 4 (Neoverse V2), 16 vCPU";
/// The heap dev corpus: heap tables of 200,000 and 1,000,000 rows stored in md5 order, an int8 hash key, int8 entries
/// (0 / 8 / 32), short texts (0 / 4) and md5 texts (0 / 2 / 8) beside it, LIMIT 10 / 1000, each top timed in both forms
/// (median of 5) on one binary at W0; non-negative least squares over the 72 shapes.
const HEAP_CORPUS: &str = "sort heap fetch sweep (hw2 / hw10)";

const fn heap_ns(ns: f64, anchor: Anchor) -> Fitted {
    Fitted { ns, machine: HEAP_MACHINE, corpus: HEAP_CORPUS, anchor }
}

/// A heap's: by value the scan deforms each entry for every row it reads and the run copies it for every admitted
/// candidate; fetched, hosts ops/sort/heap_fetch.rs reads each kept row by its TID (`row`: a buffer lookup, a visibility
/// test, the row's deform and its varlenas' copies). The entries' own fetch and lane costs fit at 0 (a 1,000-row fetch
/// of 44 entries costs about 1 ms more than one of none, inside the sweep's noise), so `row` carries them. Over the 72
/// shapes the time over the faster form summed is 5.0 ms (3 shapes missed, all at 200,000 rows), against the column
/// counts' 409 and keeping every row by value's 1,370.
const HEAP: Units = Units {
    word: Unit { decode: heap_ns(0.923, Anchor::Read), copy: heap_ns(9.73, Anchor::Update), fetch: heap_ns(0.0, Anchor::Gather), lane: heap_ns(0.0, Anchor::Read) },
    byref: Unit { decode: heap_ns(3.53, Anchor::Read), copy: heap_ns(83.2, Anchor::Update), fetch: heap_ns(0.0, Anchor::Gather), lane: heap_ns(0.0, Anchor::Read) },
    site: heap_ns(112_000.0, Anchor::Read),
    row: heap_ns(3_260.0, Anchor::Gather),
};

/// A pipeline with no estimate of the rows its scan reads cannot price the by-value form, so its top keeps the ladder's
/// count gate: more than `cols` entries, or more than `refs` by-reference ones. Two such pipelines: a serial one
/// (`Pipeline::serial` carries `predicted_rows` 0; a sorted table's scan is serial), and one that filters before its top,
/// whose `predicted_rows` counts the rows past the filter while the by-value form decodes every row the scan reads (a
/// selective filter under a zone walk: thousands of rows estimated past it, millions read in the walked granules).
/// There the cost rule still decides fetching where it pays over the rows past the filter, the fewest the scan can read
/// (`fetch_pays` grows with the rows decoded), and the counts decide the rest. Named debt: the rows a scan reads before
/// its filters, and the granules a zone walk reads (the planner's census), which no plan node carries to the decomposer.
struct NoEstimate {
    cols: usize,
    refs: usize,
}
const NO_ESTIMATE: NoEstimate = NoEstimate { cols: 32, refs: 8 };

/// Road topk.fetch's shape (V4R-169; D519: a heap's winners by TID): pipeline `p`'s (graph pipeline `k`) top over rows (a
/// top-k or a pgrc zone walk, not WITH TIES) straight over a pgrc or heap range scan, every entry of
/// its sorted row (`input`, the projection before the sort) a bare column of that scan the fetch site reads (a 2 / 4 / 8-byte word, a code, a
/// varlena lane, or on pgrc a fixed-length by-reference one), every key such a word, and fetching late cheaper than keeping the row by value (`fetch_pays` over the
/// scan's estimated rows and the entries beside the keys; with no estimate of the rows the scan reads, `NO_ESTIMATE`'s
/// counts unless the cost rule fetches over the rows past a filter). The bound is a constant count plus a constant (or no) offset (`fetchable_bound`): the top keeps `count + offset`
/// rows, the emit fetches the `count` its Limit hands on (the offset's rows never: strategies/fetch.rs), one request of at
/// most `MAX_FETCH_ROWS` (CAP-25); `fetch_pays` prices both.
pub(crate) fn topk_wide(k: usize, p: &PipelineNode<'_>, d: &SortDesc<'_>, input: Option<&NodeList<'_>>, mcx: Mcx<'_>) -> bool {
    let (SortForm::TopK { count, offset, ties: false } | SortForm::Walk { count, offset, ties: false, .. }) = d.form else { return false };
    let Some((kept, fetched)) = fetchable_bound(count, offset) else { return false };
    let (Source::Range { site, am, scanrelid, .. }, Some(l)) = (&p.source, input) else { return false };
    let Some(heap) = reads(*am, &d.form, crate::ops::sort::lower::union_arm(p).is_some()) else { return false };
    let lane = |te: &TargetEntry<'_>| {
        let v = te.expr.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varattno >= 1)?;
        // SRC-26: a fixed-length by-reference column (uuid, interval, macaddr, name: pgrc's Fixed class) the pgrc fetch site
        // publishes as a pointer to its image, as a varlena (hosts pgrc fetch_site.rs); a heap's fetch keeps such a top's
        // row by value
        site.lanes().iter().find(|x| x.id == LaneId::Attr(v.varattno)).filter(|x| !(heap && matches!(x.rep, Rep::FixedPtr { .. })) && (x.kind != lane::KIND_BYVAL || matches!(x.width, 2 | 4 | 8)))
    };
    let mut lanes: PgVec<'_, _> = PgVec::new_in(mcx);
    for te in l.iter().filter_map(|n| n.as_target_entry()) {
        let Some(x) = lane(te) else { return false };
        lanes.push_in(x);
    }
    // each key's entry (the Sort's OUTER_VAR target) a by-value word
    let key_entry = |resno: i16| d.tlist.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == resno).and_then(|te| te.expr.as_var()).and_then(|v| (v.varattno as usize).checked_sub(1)).filter(|&e| e < lanes.len());
    let mut keyed = mcx::pg_vec![in mcx; false; lanes.len()];
    for key in d.keys.iter() {
        match key_entry(key.resno) {
            // USEROP: a CompareFn key compares through its comparator's tail, which the narrow row does not carry
            Some(e) if lanes[e].kind == lane::KIND_BYVAL && !key.compare_fn() => keyed[e] = true,
            _ => return false,
        }
    }
    let refs = lanes.iter().filter(|x| x.kind != lane::KIND_BYVAL).count() as u64;
    let words = lanes.iter().zip(&keyed).filter(|(x, &is_key)| !is_key && x.kind == lane::KIND_BYVAL).count() as u64;
    let beside_refs = lanes.iter().zip(&keyed).filter(|(x, &is_key)| !is_key && x.kind != lane::KIND_BYVAL).count() as u64;
    // the estimate counts the rows the scan reads only where no op before the top drops a row (a gate passes all or none)
    let filtered = p.ops.iter().take_while(|op| !matches!(op, Op::Sort(_))).any(|op| !matches!(op, Op::Project(_) | Op::Gate(_)));
    // KEY-PREDICTED-ROWS road (b): the form decided on the scan's estimate is a guard of the program
    // a[1] carries the fetched rows above the by-value entries, a[2] the filter above the by-reference ones (all far under 2^32)
    // a[3]'s top bit: a heap's units
    gravity_lower::inputs::key::decide(k, false, p.desc.predicted_rows, fetch_form, [kept, fetched << 32 | words, (filtered as u64) << 32 | beside_refs, (heap as u64) << 63 | (lanes.len() as u64) << 32 | refs]) != 0
}

/// `topk_wide`'s relation gate, `Some(heap)` where the fetch form reads the relation: a pgrc range under a top or a zone
/// walk (whatever `arm`); a heap range under a top that is not a union arm. strategies/fetch.rs `of` (the emit) and the
/// heap scan's `fetch_narrow` (ops/scan/heap.rs: the scan reads the keys alone) both read `topk_wide`, so a heap top's
/// scan is narrowed exactly when its emit fetches; `of` keeps a union arm's rows by value, so a heap arm is out here.
fn reads(am: Am, form: &SortForm<'_>, arm: bool) -> Option<bool> {
    match (am, form) {
        (Am::Pgrcolumnar, _) => Some(false),
        (Am::Heap, SortForm::TopK { .. }) if !arm => Some(true),
        _ => None,
    }
}

/// `topk_wide`'s estimate-dependent half over `rows` estimated rows: no estimate, `NO_ESTIMATE`'s counts (`a[3]`: a heap's
/// bit << 63 | the entries << 32 | the by-reference ones); else `fetch_pays` by the relation's units for `a[0]` kept rows,
/// `a[1] >> 32` fetched, with `a[1]`'s low half by-value and `a[2]`'s by-reference entries beside the keys, and where a
/// filter comes before the top (`a[2] >> 32`: `rows` are past it, fewer than the scan reads) the counts as well.
fn fetch_form(rows: u64, a: &[u64; 4]) -> u64 {
    let counts = ((a[3] >> 32) & 0x7fff_ffff) as usize > NO_ESTIMATE.cols || (a[3] & 0xffff_ffff) as usize > NO_ESTIMATE.refs;
    if rows == 0 {
        return counts as u64;
    }
    let u = if a[3] >> 63 != 0 { &HEAP } else { &PGRC };
    let (words, refs, filtered) = ((a[1] & 0xffff_ffff) as usize, (a[2] & 0xffff_ffff) as usize, a[2] >> 32 != 0);
    let pays = fetch_pays(anchor::ratios(), u, rows, a[0], a[1] >> 32, words, refs);
    (pays || filtered && counts) as u64
}

/// Whether fetching the `fetched` winners late costs less than keeping every candidate of a `kept`-row top by value over
/// `rows` input rows, with `beside` the entries the fetch site reads (every entry but the keys). By value, each is decoded
/// for all `rows` (every row the scan reads: the fit's tables have no filter) and copied for each admitted candidate, about `kept (1 + ln(rows / kept))` in random key order; fetched,
/// each is read for the fetched rows (the Limit's count: an offset's rows are never fetched), plus the site's cost. Over the sweep's 396 shapes the time over the faster form summed is 58 ms against
/// the column counts' 612, lower on every table (held out: 25 against 279). It misses where a cost it cannot see moves: a
/// low-NDV code copies cheaply (a small dictionary), a key in load order admits and decodes fewer rows by value (LIMIT
/// 1000: 0.4 - 2.6 ms by value against 1.5 - 6 ms fetched), and a small table's fetch costs less than its fit (20,000 rows,
/// 56 entries, LIMIT 10: 0.80 ms by value against 0.40 fetched). The units are priced on a host of ratios `r` (the
/// installed profile's, `fetch_form`).
fn fetch_pays(r: &Ratios, u: &Units, rows: u64, kept: u64, fetched: u64, words: usize, refs: usize) -> bool {
    let n = rows as f64;
    let k = (kept as f64).min(n);
    let f = (fetched as f64).min(k);
    if k <= 0.0 {
        return false;
    }
    let admitted = (k * (1.0 + (n / k).ln())).min(n);
    let beside = core::iter::repeat_n(&u.word, words).chain(core::iter::repeat_n(&u.byref, refs));
    let gain: f64 = beside.map(|e| e.decode.on(r) * n + e.copy.on(r) * admitted - e.fetch.on(r) * f - e.lane.on(r)).sum();
    gain > u.site.on(r) + u.row.on(r) * f
}

/// A top's `(kept, fetched)` rows the fetch form prices: a constant count plus a constant (or no) offset kept
/// (saturating, any number), and the count fetched, at most one request (`MAX_FETCH_ROWS`): the emit never fetches the
/// offset's rows, so an OFFSET of any size takes the form. Past one request the batched fetch decodes each touched granule
/// again per request (a 20,000-row count over 1,000,000 rows: 68 ms against 30 by value on the sweep's machine, `MACHINE`), which
/// `fetch_pays` does not price: a fit of the per-request cost is the debt that would lift the count's cap.
fn fetchable_bound(count: Bound<'_>, offset: Bound<'_>) -> Option<(u64, u64)> {
    let off = match offset {
        Bound::All => 0,
        Bound::Const(o) if o >= 0 => o,
        _ => return None,
    };
    match count {
        Bound::Const(c) if c >= 0 && c <= MAX_FETCH_ROWS as i64 => Some((c.saturating_add(off) as u64, c as u64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap25_any_offset_and_one_request_of_count() {
        assert_eq!(fetchable_bound(Bound::Const(10), Bound::All), Some((10, 10)));
        assert_eq!(fetchable_bound(Bound::Const(1000), Bound::Const(24)), Some((1024, 1000)));
        assert_eq!(fetchable_bound(Bound::Const(1000), Bound::Const(100)), Some((1100, 1000)), "kept past the site's request capacity");
        assert_eq!(fetchable_bound(Bound::Const(5), Bound::Const(i64::MAX)), Some((i64::MAX as u64, 5)), "saturating");
        assert_eq!(fetchable_bound(Bound::Const(1024), Bound::Const(7)), Some((1031, 1024)), "one request's count");
        assert_eq!(fetchable_bound(Bound::Const(1025), Bound::All), None, "a count past one request keeps its rows by value");
        assert_eq!(fetchable_bound(Bound::All, Bound::All), None);
        assert_eq!(fetchable_bound(Bound::Const(-1), Bound::All), None);
        assert_eq!(fetchable_bound(Bound::Const(5), Bound::Const(-1)), None);
        assert!(fetch_pays(&Ratios::ONE, &PGRC, 1_000_000, 170_000, 5, 9, 4), "LIMIT 5 OFFSET 169995: five rows fetched");
        assert!(!pays(1_000_000, 900_000, 16, 8), "a top keeping most rows and fetching them keeps them by value");
        assert!(fetch_pays(&Ratios::ONE, &PGRC, 1_000_000, 900_000, 5, 16, 8), "LIMIT 5 OFFSET 899995: five rows fetched");
    }

    fn pays(rows: u64, kept: u64, words: usize, refs: usize) -> bool {
        fetch_pays(&Ratios::ONE, &PGRC, rows, kept, kept, words, refs)
    }

    /// The sweep's measured winners (ms by value / fetched): the first four the column-count rule (more than 32 entries
    /// or 8 by-reference ones) kept by value; the canonical wide top; the small tables.
    #[test]
    fn the_cheaper_form_by_cost() {
        assert!(pays(200_000, 1000, 0, 8), "8 unique texts, 200k rows, LIMIT 1000: 40.5 / 4.8");
        assert!(pays(1_000_000, 100, 4, 2), "4 words + 2 unique texts, 1M rows, LIMIT 100: 4.3 / 2.7");
        assert!(pays(1_000_000, 1000, 16, 8), "16 words + 8 texts, 1M rows, LIMIT 1000: 12.6 / 8.7");
        assert!(pays(1_000_000, 1000, 16, 2), "16 words + 2 unique texts, 1M rows, LIMIT 1000: 11.7 / 6.9");
        assert!(!pays(200_000, 1000, 4, 0), "4 words, 200k rows, LIMIT 1000: 1.7 / 2.2");
        assert!(pays(1_000_000, 10, 40, 16), "SELECT * of a wide 1M-row table, LIMIT 10: 3.4 / 0.9");
        assert!(!pays(5_000, 10, 28, 6), "a 5,000-row table keeps a 36-entry row by value");
        assert!(!pays(0, 10, 40, 16) && !pays(1_000_000, 0, 40, 16), "no rows or no kept rows: nothing to fetch");
    }

    /// D519: a heap top fetches its winners by TID, a heap union arm keeps its rows by value (its scan every column: the
    /// emit's `of` and the scan's `fetch_narrow` agree through `topk_wide`), a zone walk is pgrc's; pgrc is unchanged.
    #[test]
    fn a_heap_top_fetches_by_tid_and_a_union_arm_keeps_its_rows() {
        let top = SortForm::TopK { count: Bound::Const(10), offset: Bound::All, ties: false };
        let walk = SortForm::Walk { attno: 1, desc: false, nulls_first: false, count: Bound::Const(10), offset: Bound::All, ties: false };
        assert_eq!(reads(Am::Heap, &top, false), Some(true));
        assert_eq!(reads(Am::Heap, &top, true), None, "a union arm's emit keeps its rows by value, so its scan is not narrowed");
        assert_eq!(reads(Am::Heap, &walk, false), None, "a zone walk is pgrc's");
        assert_eq!(reads(Am::Pgrcolumnar, &top, false), Some(false));
        assert_eq!(reads(Am::Pgrcolumnar, &top, true), Some(false), "pgrc's arms as before (its emit's `of` keeps them by value)");
        assert_eq!(reads(Am::Pgrcolumnar, &walk, false), Some(false));
        assert_eq!(reads(Am::Pgrcolumnar, &SortForm::Runs, false), Some(false), "the form gate before it takes tops and walks only");
    }

    fn heap_pays(rows: u64, kept: u64, words: usize, refs: usize) -> bool {
        fetch_pays(&Ratios::ONE, &HEAP, rows, kept, kept, words, refs)
    }

    /// The heap sweep's measured shapes (ms by value / fetched): a heap's by-value scan deforms every entry of every row it
    /// reads, so its top fetches texts where pgrc's units keep them by value; one of the fit's three misses.
    #[test]
    fn a_heap_top_prices_by_the_heap_units() {
        assert!(heap_pays(1_000_000, 10, 0, 2), "2 md5 texts, 1M rows, LIMIT 10: 27.6 / 14.8");
        assert!(!pays(1_000_000, 10, 0, 2), "pgrc's units keep the same shape by value");
        assert!(heap_pays(1_000_000, 1000, 32, 12), "44 entries, 1M rows, LIMIT 1000: 103.0 / 18.1");
        assert!(heap_pays(200_000, 10, 8, 0), "8 words, 200k rows, LIMIT 10: 4.7 / 3.4");
        assert!(heap_pays(200_000, 1000, 8, 4), "8 words + 4 short texts, 200k rows, LIMIT 1000: 9.0 / 5.0");
        assert!(!heap_pays(200_000, 1000, 8, 0), "a miss: 8 words, 200k rows, LIMIT 1000: 5.6 / 4.8");
        assert!(!heap_pays(1_000_000, 1000, 0, 0), "nothing beside the key: 15.0 / 17.1");
        assert!(!heap_pays(5_000, 10, 1, 1), "a 5,000-row table keeps a word and a text by value");
        let args = |heap: u64| [10, 10 << 32, 2, heap << 63 | 3 << 32 | 2];
        assert_eq!((fetch_form(1_000_000, &args(1)), fetch_form(1_000_000, &args(0))), (1, 0), "a[3]'s top bit picks the heap's units");
    }

    /// The decision census (cost_profile census.rs): the sweep's measured shapes above, each form on each host; a shape whose
    /// form differs from the reference's is one to validate on that host.
    #[test]
    fn the_fetch_form_on_each_host() {
        let shapes: [(&Units, u64, u64, usize, usize); 15] = [
            (&PGRC, 200_000, 1000, 0, 8),
            (&PGRC, 1_000_000, 100, 4, 2),
            (&PGRC, 1_000_000, 1000, 16, 8),
            (&PGRC, 1_000_000, 1000, 16, 2),
            (&PGRC, 200_000, 1000, 4, 0),
            (&PGRC, 1_000_000, 10, 40, 16),
            (&PGRC, 5_000, 10, 28, 6),
            (&PGRC, 1_000_000, 10, 0, 2),
            (&HEAP, 1_000_000, 10, 0, 2),
            (&HEAP, 1_000_000, 1000, 32, 12),
            (&HEAP, 200_000, 10, 8, 0),
            (&HEAP, 200_000, 1000, 8, 4),
            (&HEAP, 200_000, 1000, 8, 0),
            (&HEAP, 1_000_000, 1000, 0, 0),
            (&HEAP, 5_000, 10, 1, 1),
        ];
        let forms = |r: &Ratios| shapes.map(|(u, rows, kept, words, refs)| fetch_pays(r, u, rows, kept, kept, words, refs));
        let reference = forms(&Ratios::ONE);
        for (name, r, _) in gravity_program::cost::census::HOSTS {
            let at = forms(&r);
            let moved: Vec<usize> = (0..shapes.len()).filter(|&i| at[i] != reference[i]).collect();
            println!("census\t{name}\tfetch forms {at:?}\tmoved shapes {moved:?}");
        }
    }

    /// `fetch_form`'s arguments for a top of `kept` rows (all fetched) with `words` / `refs` entries beside one word key.
    fn form(rows: u64, kept: u64, words: u64, refs: u64, filtered: bool) -> u64 {
        fetch_form(rows, &[kept, kept << 32 | words, (filtered as u64) << 32 | refs, (words + refs + 1) << 32 | refs])
    }

    /// A filter before the top: the estimate counts the rows past it, the by-value form decodes every row the scan reads.
    #[test]
    fn a_filtered_top_prices_no_fewer_rows_than_its_scan_reads() {
        // SELECT * of a 100-column table under a selective filter, LIMIT 10: 20,000 rows estimated past the filter, millions
        // read; by value over the estimate where it is every row read (a table of 20,000 rows), fetched once filtered
        assert_eq!(form(20_000, 10, 70, 29, false), 0);
        assert_eq!(form(20_000, 10, 70, 29, true), 1, "the counts: 100 entries");
        assert_eq!(form(0, 10, 70, 29, false), 1, "no estimate: the counts");
        // a narrow filtered top keeps its row by value (the counts and the cost over the rows past the filter)
        assert_eq!(form(20_000, 10, 4, 2, true), 0);
        // the cost rule fetches over the rows past the filter where that pays, whatever the counts
        assert_eq!(form(100_000, 1000, 0, 8, true), 1, "8 unique texts beside the key, LIMIT 1000");
        assert_eq!(form(100_000, 1000, 0, 8, false), 1);
    }
}
