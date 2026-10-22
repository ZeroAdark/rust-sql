//! The runs face (plan §3.1; H19 H22 H26). A run is one sealed row array (a sort's output, a spool, a work table) with its
//! readers, produced by the sort-run forms (H26) and skipped entirely when a declared order already holds (H22). A run's rows follow PG's sort semantics: its sort operators, collations and NULLS placement. Ties may come
//! in any order, since the SQL fixes none. Run kinds (face Runs) are made by `frame::StepMaker` / `EndMaker` / `SourceMaker`.
use gravity_decompose::ops::shared::sort::{SortDesc, SortKey};
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{StackSlot, Type, Value};
use gravity_kernels::host_abi as h;
use gravity_program::{DictRef, SiteDesc};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::face::end::EndMech;
use crate::face::source::SourceMech;
use crate::frame::env::Env;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::mergejoin::strategies::ksort::{MERGE_KSORT, MERGE_WIDEEMIT};
use crate::table::emit_runs::EMIT_PARMERGE;
use crate::rep::keylane::{ivl, Key};

crate::strategies! {
    /// Rescan-invariant inner spooled once. No mechanism yet: its readers take the straight form.
    NEST_SELFSPOOL = 93, "nest.selfspool", Nest;
    /// A Materialize over a Sort reads the Sort's run. No mechanism yet: its readers take the straight form.
    SPOOL_ALIAS = 101, "spool.alias", SpoolSort;
}

crate::hook_variants! {
    /// The run store and its readers (H19).
    "H19" pub enum RunStore {
        /// One sealed row array (a sort's output run).
        Sealed = Kind(RunsSort),
        /// A pointer-walk reader, `base + j * row_bytes`, with no host call per row (a spool).
        Walk = Kind(NestSpool),
        /// Mark / restore as a position store: N readers, each with its own position, and rewind.
        Mark = Strategy(MERGE_MARKABLE),
        /// A generation swap with truncate-in-place (the work table).
        Generation = Kind(SetopRecursive),
        /// A lazy pull (the D237 class: a CTE read at first use).
        Lazy = Kind(NestCte),
        /// Parallel producers merged at the barrier (a markable k-way), spilling through H24.
        Merged = Strategy(MERGE_KSORT),
        /// D3336: a group emit's claims fill the merge inner Sort's runs, merged at the barrier as `Merged`.
        WideEmit = Strategy(MERGE_WIDEEMIT),
        /// A Materialize over a Sort reads the Sort's run.
        Alias = Strategy(SPOOL_ALIAS),
        /// A rescan-invariant inner spooled once.
        SelfSpool = Strategy(NEST_SELFSPOOL),
        /// D3256: a sealed CTE store read by record claims on the statement's workers.
        Claims = Strategy(CTE_CLAIMS),
        /// D3256: a lazy CTE pull of WINDOW rows a pull, its raise held.
        Batch = Strategy(CTE_BATCH),
        /// D3336 (A2 (iii)): a CTE filled whole ahead under v5 errorhold's licence, its sorting readers by record claims.
        WholeFill = Strategy(CTE_WHOLEFILL),
    }
}

/// A sealed run as its readers see it: the row array's base, its row count and its row width.
#[derive(Clone, Copy, Debug)]
pub struct RunRef {
    pub base: Value,
    pub rows: Value,
    pub row_bytes: u32,
}

/// A run store's reader (registered as `RunImpl` by the run-store variants).
pub trait RunMech: Sync {
    /// H17 a: the reader's position words (a mark saves one, a restore sets it).
    fn open_words(&self, _w: &mut OpenWords) {}
    /// Row `j`'s address.
    fn row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, run: &RunRef, j: Value) -> Value;
}

pub struct RunImpl(pub &'static dyn RunMech);

crate::strategies! {
    /// D3336: a window read by claims: its serial fill hands each partition whole to the next window object, the reader's
    /// claims read one object each (face/partruns.rs).
    WIN_PARTRUNS = 108, "win.partruns", Window;
    /// Per-partition top-N from a run condition. No mechanism yet: its readers take the straight form.
    WIN_TOPN = 109, "win.topn", Window;
}

crate::hook_variants! {
    /// The sort-run forms (H26 a–e).
    "H26" pub enum RunForm {
        /// (a) observed natural runs skip their sort, and claim-order runs chain.
        Natural = Strategy(SORT_NATURAL),
        /// (b) the merge reads the workers' runs in place, with a BufFile only on spill.
        Resident = Strategy(SORT_RESIDENT),
        /// (c) partition-grouped runs, one partition per worker.
        PartRuns = Strategy(WIN_PARTRUNS),
        /// (c) D3336 (A1a): a parted window's fill as claims over its sorted emit's parts cut on the PARTITION BY.
        PartFill = Strategy(WIN_PARTFILL),
        /// (d) a bounded run per group (DISTINCT ON, LIMIT per group, LATERAL top-N).
        PerGroup = Strategy(RUN_PERGROUP),
        /// (d) the per-partition top-N from a window's run condition (rank / row_number <= N).
        TopN = Strategy(WIN_TOPN),
        /// (e) a producer-fed key plus row-index lane, a one-word null image and a short-key tie.
        KeyLane = Strategy(SORT_KEYLANE),
        /// D3336: a sorted wide emit's claim runs merged by key range at the section end, a part per claim thread.
        ParMerge = Strategy(EMIT_PARMERGE),
    }
}

crate::hook_variants! {
    /// What consumes a declared order (H22).
    "H22" pub enum OrderUse {
        /// Runs of equal keys carry or collapse, gated on the runs word (source.rs `WinConds::runs`).
        Collapse = Strategy(RUN_COLLAPSE),
        /// A merge instead of a hash when both sides declare the key order (join.rs `JoinRoad::CoSorted`).
        Merge = Kind(MergeJoin),
        /// A streaming group over sorted input.
        Stream = Kind(RunsGroup),
        /// No reader reads a pgrc scan's merged order, so decompose drops its merge mark (D4568, ops/scan
        /// `map_seq_scan`; a road outside this crate, lowerv4 ops/mod.rs `XC`).
        Unread = Strategy(SCAN_MERGEFREE),
        /// A hash build input's unread merge mark dropped too (D4585, default off: decompose facts/order.rs
        /// `merge_free_under`; a road outside this crate, lowerv4 ops/mod.rs `XC`).
        UnreadBuild = Strategy(SCAN_MERGEFREE_BUILD),
        /// No reader but the sorted group above reads a Unique's order: the Unique absorbs its key Sort and hands its rows
        /// on unordered, the group orders only its own groups where read (D4676, default off: decompose ops/group/absorb.rs
        /// `chain_of`; a road outside this crate, lowerv4 ops/mod.rs `XC`).
        HashChain = Strategy(GROUP_HASHCHAIN),
    }
}

/// The top a code top-k road is asked about (H10 / H11, M6-plan §3.7; V4R-294): producer `k`'s run, its plan node and
/// its sort.
#[derive(Clone, Copy)]
pub struct TopDesc<'a, 'p> {
    pub k: usize,
    pub node: i32,
    pub sd: &'a SortDesc<'p>,
}

/// A code top-k road's mechanism (code.cut, code.heads), read by runs/topk_rows.rs and runs/topk_groups.rs.
/// Its hooks wrap the reader's own end and emit; a straight row declines every top and keeps both.
pub trait CodeTopkMech: Sync {
    /// Whether the road's engaged form takes this top; `false` keeps the reader's own top.
    fn engages(&self, gx: &Gx<'_, '_>, top: &TopDesc<'_, '_>) -> bool;
    /// The top producer's end over the reader's `inner` (code.cut: the per-part code cut before any render; code.heads: the
    /// entries mark).
    fn end<'g>(&self, _px: &mut Px<'g, '_>, _top: &TopDesc<'_, '_>, inner: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
        inner
    }
    /// V4R-310: whether the engaged form's run rows carry one int8 mark column past the projection (code.heads).
    fn marks(&self) -> bool {
        false
    }
    /// V4R-310: whether producer `k`'s drafted pgrc site publishes its one code lane as each safe part's dictionary
    /// entries for this road (code.heads; asked by source/dict_entries.rs).
    fn entries(&self, _px: &Px<'_, '_>, _k: usize, _site: &SiteDesc) -> bool {
        false
    }
    /// V4R-310: the top's emit over its run reader (code.heads: each entries row repeated by its rows in its part), `None`
    /// to keep the reader as the emit. PLANMEM: `reader` boxes the run reader in the planning context for a wrapping emit
    /// (a `PgBox` has no `dyn RunReader` to `dyn SourceMech` upcast on stable); the caller boxes it as the emit on `None`.
    fn emit<'g>(&self, _px: &mut Px<'g, '_>, _top: &TopDesc<'_, '_>, _reader: &mut dyn FnMut() -> PgBox<'g, dyn RunReader + 'g>) -> Option<PgBox<'g, dyn SourceMech + 'g>> {
        None
    }
}

/// V4R-310: the top's run reader (runs/sort.rs `RunEmit`) as a wrapping emit reads it: row `j` fetched once, its cells
/// read from the fetched row (valid until the next fetch).
pub trait RunReader: SourceMech {
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value;
    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, j: Value) -> RowVals<'e>;
}

/// A code top-k road's registered row (`need_strategy::<CodeTopkImpl>`).
pub struct CodeTopkImpl(pub &'static dyn CodeTopkMech);

/// D1120 (V4R-384): kind runs.group_distinct's per-group distinct set, read by keyed/stream_distinct.rs (a streamed sorted
/// group's DISTINCT aggregate, PG's per-group sort and dedup). The set keeps `words()` frame words after the stream's own
/// (opened at 0); a group's open empties it, and each non-NULL argument's image words are offered once per row.
pub trait DistinctSetMech: Sync {
    /// The frame words the set keeps.
    fn words(&self) -> u8;
    /// A group opens: the set at `set` (its first frame word's address) empties.
    fn clear(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value);
    /// The argument's image words `img` offered: an I1, true when they were not in the set (which now holds them). A
    /// failure (no room could be pinned) leaves by the raise exit.
    fn insert(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value, img: &[Value]) -> Value;
    /// `insert` where the group's arguments arrive sorted, equal values adjacent (PostgreSQL presorted the call under an
    /// order whose equal values are one image: ops/group/stream_distinct.rs `sorted`), so a set may keep the last image
    /// alone. A statement offers one way only. The default is `insert`.
    fn insert_sorted(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, set: Value, img: &[Value]) -> Value {
        self.insert(b, env, set, img)
    }
}

/// Kind runs.group_distinct's registered row (`need_kind::<DistinctSetImpl>`).
pub struct DistinctSetImpl(pub &'static dyn DistinctSetMech);

/// M11 (merge.join, V4R pending): a run in a slot as another kind's step reads it by row index (mark / restore = an index),
/// and the Materialize producer that fills such a run in arrival order. runs/slot_run.rs registers it in runs.sort's tree
/// (`SlotRunImpl`, need_kind of `KindId::RunsSort`).
pub trait SlotRunMech: Sync {
    /// Producer pipeline `k`'s end: its rows (its last projection) into one run with no keys, in arrival order, the
    /// pipeline serial (the run sealed where it lies).
    fn end<'g>(&self, px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g>;
    /// Pipeline `k`'s reader of the run in slot `slot` (a sort's run or `end`'s), which `k` consumes.
    fn reader<'g>(&self, px: &mut Px<'g, '_>, k: usize, slot: u8) -> PgBox<'g, dyn SlotRun + 'g>;
    /// jmerge-0012 (the exact request's merge inner, PLAN-v5 §2.5): `end` for a run its reader pulls, nothing read ahead
    /// of the pull. The caller makes pipeline `k` a pulled set's last member (deferred, leader-serial: its proto names it).
    /// Each row is stored as `end` stores it (the run grown resident, never put), its by-reference cells pointed at once,
    /// and the end stops resumably after it: a pull stores one row more, or the fill ends (the run sealed where it lies).
    /// A Sort's run refuses by name (it reads its input whole before its first row: no row-at-a-time fill), as does a
    /// union arm's run.
    fn pulled_end<'g>(&self, px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g>;
    /// `reader` of the run `pulled_end` fills in slot `slot`: its `open` reads the run as stored so far (0 before the first
    /// row) and is called again after each pull, which may have grown the run elsewhere (the base re-read and kept in a
    /// frame word for `fetch`); a fetched row's address and its cells hold until the next pull.
    fn pulled_reader<'g>(&self, px: &mut Px<'g, '_>, k: usize, slot: u8) -> PgBox<'g, dyn SlotRun + 'g>;
}

/// A step's reader of a sealed run (`SlotRunMech::reader`): opened per window, rows by index, no host call per row.
pub trait SlotRun {
    /// The sealed run at this window (a run past its grant leaves through `x.l_exit` by its named error): its row count (`I64`).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &crate::frame::stop::Exits) -> Value;
    /// Row `j`'s address (`0 <= j < n`), after `open`.
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value;
    /// The cells of the row at `p`, its columns by resno (`cols[a - 1]`); valid while the run lives.
    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, at: Value) -> RowVals<'e>;
    /// W8-EQUIVCLASS-SORTOP: where the step first calls its input (merge.join's INITIALIZE_INNER / ENDOUTER): a run whose
    /// Sort PG cannot begin raises here (runs/sort_noproc.rs); nothing for any other run.
    fn first_fetch(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) {}
    /// D3105 (b): the step marked row `j` (merge.join's mark word set, its row fetched just before): a reader whose rows
    /// move (a window over put runs, face/slot_window.rs) keeps the mark's rows; nothing for a sealed run.
    fn mark(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _j: Value) {}
}

/// runs.sort's tree's registered row for `SlotRunMech`.
pub struct SlotRunImpl(pub &'static dyn SlotRunMech);

// The run row format (v2 `RowSpec`): what a run's writers and readers agree on, a full sort's and a MergeAppend's alike.


fn shape(node: i32, why: &'static str) -> ! { crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }) }

/// How a run column is held: its datum word, a fixed image at `off` (the datum pointed at it by `sort_next`), a varlena
/// copy behind the slot `[size u32, off u32]` at `off`, or (a top's, D538: late-rendered code lane) the word `part << 32 | code` of code lane
/// `dict` of pipeline `producer`'s site, rendered where the emit's consumer reads it (keyed/late_render.rs's rule).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    Word,
    Image { off: u32 },
    Var { off: u32 },
    Code { dict: DictRef, producer: u16 },
}

#[derive(Clone, Copy)]
pub(crate) struct Col {
    pub(crate) tf: TypeFacts,
    pub(crate) form: Form,
}

/// The run row (v2 `RowSpec`): `[flags, word]` per key at 0, the datum words at `16 * keys`, the null bytes after them, the
/// record word (var rows), the images (8-rounded each), the var slots last (a var run of no var column: its record word last).
pub(crate) struct Shape {
    pub(crate) cols: Vec<Col>,
    pub(crate) keys: Vec<Key>,
    pub(crate) row_bytes: u32,
    pub(crate) spec: u64,
    pub(crate) rec_off: u32,
}

impl Shape {
    pub(crate) fn datums(&self) -> i32 { 16 * self.keys.len() as i32 }
    pub(crate) fn nulls(&self) -> i32 { self.datums() + 8 * self.cols.len() as i32 }
    pub(crate) fn var(&self) -> bool { self.cols.iter().any(|c| matches!(c.form, Form::Var { .. })) || self.keys.iter().enumerate().any(|(i, x)| crate::rep::keylane::var_key(i, x.tail, x.proc)) }
    pub(crate) fn tails(&self) -> bool { self.keys.iter().any(|k| k.tail != 0) }
    /// Each key's having an image tail (`natural::mark`).
    pub(crate) fn tailed<'m>(&self, mcx: Mcx<'m>) -> PgVec<'m, bool> { self.keys.iter().map(|k| k.tail != 0).collect_in(mcx) }
    /// The var columns a key's tail compares: their copies are payloads (`res::sort_image_key`).
    pub(crate) fn payloads<'m>(&self, mcx: Mcx<'m>) -> PgVec<'m, usize> {
        self.keys.iter().filter(|x| x.tail >> 8 & 0x7f == h::VAR_WIDTH).map(|x| x.col as usize).collect_in(mcx)
    }
    /// `(col, slot offset)` of each var column.
    pub(crate) fn vars(&self) -> Vec<(usize, u32)> {
        self.cols.iter().enumerate().filter_map(|(c, x)| if let Form::Var { off } = x.form { Some((c, off)) } else { None }).collect() // alloc: Program
    }

    /// The row of a child projection (`cols`) under `keys`: each key's resno through `tlist` (a Sort's target list,
    /// `OUTER_VAR` entries), or over `cols` itself when `None` (MergeAppend's keys).
    pub(crate) fn plan(px: &Px<'_, '_>, node: i32, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>) -> Shape {
        Shape::plan_coded(px, node, keys, tlist, cols, &|_| None, false)
    }

    /// `plan` with each varlena column `c` that `code(c)` names held as its code word (`Form::Code`; runs/topk_rows.rs); `mark`: one int8 column more (V4R-310, runs/heads.rs).
    pub(crate) fn plan_coded(px: &Px<'_, '_>, node: i32, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>, code: &dyn Fn(usize) -> Option<(DictRef, u16)>, mark: bool) -> Shape {
        Shape::plan_with(px, node, keys, tlist, cols, code, mark, false)
    }

    /// `plan` for a run PostgreSQL's qsort orders in arrival order (ops/sort/exact_feed.rs): every key compares
    /// through its comparator on its datum tail (`rep::keylane::exact_proc`, KFLAG_USER), so the run is a var run whatever
    /// its columns (V4R-386's record word last).
    pub(crate) fn plan_exact(px: &Px<'_, '_>, node: i32, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>) -> Shape {
        Shape::plan_with(px, node, keys, tlist, cols, &|_| None, false, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_with(px: &Px<'_, '_>, node: i32, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>, code: &dyn Fn(usize) -> Option<(DictRef, u16)>, mark: bool, exact: bool) -> Shape {
        let mut tfs: PgVec<'_, TypeFacts> = cols.iter().enumerate().map(|(i, n)| {
            let te = n.as_target_entry().filter(|te| te.resno as usize == i + 1).unwrap_or_else(|| shape(node, "a sort over a projection out of resno order"));
            let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
            facts_of(px.gx.facts, typ).unwrap_or_else(|| shape(node, "a sorted column whose type the catalog does not know"))
        }).collect_in(px.mcx());
        tfs.extend_in(mark.then(|| *gravity_decompose::catalog::typefacts::row_of(20).unwrap_or_else(|| shape(node, "no int8 type facts"))));
        let natts = tfs.len() as u32;
        // a Const key in a Sort's target list (setrefs keeps it: `SELECT 42, 42, x ... ORDER BY 1, 2` under a union) orders nothing
        let konst = |k: &&SortKey| tlist.is_some_and(|l| l.iter().filter_map(|n| n.as_target_entry()).any(|te| te.resno == k.resno && te.expr.as_const().is_some()));
        let keys: PgVec<'_, SortKey> = keys.iter().filter(|k| !konst(k)).copied().collect_in(px.mcx());
        let keys = &keys[..];
        let nk = keys.len() as u32;
        // a cstring column that is a NULL constant and orders nothing (an untyped NULL of INSERT .. SELECT .. ORDER BY, typed
        // `unknown` until the coercion above the Sort): its word is never read, only its null byte
        let null_const = |c: usize| !keys.iter().any(|k| crate::rep::keylane::key_col(k, tlist, tfs.len()) == Some(c as u16)) && cols.iter().nth(c).and_then(|n| n.as_target_entry()).is_some_and(|te| te.expr.as_const().is_some_and(|k| k.constisnull));
        let mut out: Vec<Col> = Vec::with_capacity(tfs.len()); // alloc: Program
        for (c, tf) in tfs.iter().enumerate() {
            let form = match tf.typlen {
                _ if tf.byval => Form::Word,
                1..=h::RUN_IMAGE_MAX => Form::Image { off: 0 },
                -1 => code(c).map_or(Form::Var { off: 0 }, |(dict, producer)| Form::Code { dict, producer }),
                -2 if null_const(c) => Form::Word,
                -2 => shape(node, "a sorted cstring column"),
                _ => shape(node, "a sorted fixed-width column wider than 126 bytes"),
            };
            out.push(Col { tf: *tf, form }); // alloc: Program
        }
        // a pgrc packed lane's numeric (rep.packed.order) orders by its exact word: not hosted, no tail (HO condition (d));
        // D2426: only a narrow one (p <= 18): a wider lane orders hosted, by its var tail
        // D3931: one decision for every writer of the run (a MergeAppend's arms) and its reader: `packed_words`
        let packed = crate::rep::keylane::packed_words(px.gx, cols);
        let var = out.iter().any(|c| matches!(c.form, Form::Var { .. }));
        let mut rec_off = 16 * nk + 8 * natts + natts.next_multiple_of(8);
        let mut at = rec_off + 8 * var as u32;
        // the images (8-rounded each), then the var slots
        out.iter_mut().for_each(|c| if let Form::Image { off } = &mut c.form { (*off, at) = (at, at + (c.tf.typlen as u32).next_multiple_of(8) + 16 * ivl(&c.tf) as u32) });
        out.iter_mut().for_each(|c| if let Form::Var { off } = &mut c.form { (*off, at) = (at, at + 8) });
        let mut by_cmp = crate::rep::keylane::cmp_cols(keys, tlist, out.len(), px.mcx());
        if exact {
            keys.iter().filter_map(|k| crate::rep::keylane::key_col(k, tlist, out.len())).for_each(|c| by_cmp[c as usize] = true);
        }
        let tails = crate::rep::keylane::tails(&out, &packed, 16 * nk, &by_cmp, px.mcx());
        let proc = |sk: &SortKey, c: usize| if exact { crate::rep::keylane::exact_proc(px.gx.catalog, node, sk, &out[c].tf) } else { crate::rep::keylane::cmp_proc(px.gx.catalog, node, sk, &out[c].tf) };
        let (keys, spec) = crate::rep::keylane::keys(node, keys, tlist, &tails, &proc);
        // V4R-386 / V4R-548: a hosted key (an enum word, a collated name), or a tail from key 4, makes a var run of no var column (its entry rides the table): its record word last
        (rec_off, at) = if !var && keys.iter().enumerate().any(|(i, x)| crate::rep::keylane::var_key(i, x.tail, x.proc)) { (at, at + 8) } else { (rec_off, at) };
        Shape { cols: out, keys, row_bytes: at, spec, rec_off }
    }

    /// The emit table on the stack (`table`).
    pub(crate) fn table(&self, b: &mut FunctionBuilder<'_>) -> Value { table(b, self).0 }

    /// `table` and its stack slot: the address a reader's merge is laid for (kernels rowsort.rs `reader_of`), taken again
    /// past its last window (D2004, ops/sort/lower.rs `RunEmit::next_window`).
    pub(crate) fn table_slot(&self, b: &mut FunctionBuilder<'_>) -> (Value, StackSlot) { table(b, self) }

    /// `(row_bytes, nkey words, spec)` as the fixed hosts take them.
    pub(crate) fn args(&self, b: &mut FunctionBuilder<'_>) -> [Value; 3] {
        [b.iconst(Type::I64, self.row_bytes as i64), b.iconst(Type::I64, 2 * self.keys.len() as i64), b.const_raw(Type::I64, self.spec)]
    }
}

/// The hosts' emit table words (host_abi `SORT_TBL_HDR`) of `s` on the stack; a var row flags `natts` with `VAR_TBL` and
/// appends its var section (`nvar, rec_off`, `(col, slot)` per var column, `(coll, kflags)` per key: a text tail's collation
/// unless it orders by bytes, `KFLAG_BPCHAR`, and from key 4 its tail's `SortSpec` entry, V4R-265). A hosted key (rep.hosted.order:
/// `Key::proc`, its btree support function's resolution id, `gravity_fn::resolved`) carries its collation and `KFLAG_HOSTED`, and
/// the table ends with the hosted section, one resolution id per key (0 for the others).
fn table(b: &mut FunctionBuilder<'_>, s: &Shape) -> (Value, StackSlot) {
    let imgs: Vec<(u32, u32, u32)> = s.cols.iter().enumerate().filter_map(|(c, x)| if let Form::Image { off } = x.form { Some((c as u32, off, x.tf.typlen as u32)) } else { None }).collect(); // alloc: Program
    let vars = s.vars();
    let natts = s.cols.len() as u32 | if s.var() { h::VAR_TBL } else { 0 }; // V4R-386: a hosted enum key's run, no var column
    let mut w = vec![s.row_bytes, 2 * s.keys.len() as u32, natts, imgs.len() as u32, s.spec as u32, (s.spec >> 32) as u32]; // alloc: Program
    imgs.iter().for_each(|&(c, o, t)| w.extend([c, o, t]));
    if s.var() {
        w.extend([vars.len() as u32, s.rec_off]);
        vars.iter().for_each(|&(c, o)| w.extend([c as u32, o]));
        for (i, k) in s.keys.iter().enumerate() {
            let (text, bpchar) = crate::rep::keylane::key_tf(&s.cols[k.col as usize].tf, k).image.bytes_form().unwrap_or((false, false));
            let coll = if k.proc != 0 { k.coll.oid } else if text && k.tail != 0 && !k.coll.bytes_order { k.coll.oid } else { 0 };
            let ext = if i >= 4 { (k.tail as u32) << h::KFLAG_SPEC_SHIFT } else { 0 };
            // KFLAG_EQTIE: a hosted key whose comparator cannot raise on two equal copies ties them with no call: a builtin over
            // a scalar type that reads no catalog, type cache or collation (`resolved::pool_safe`'s numeric_cmp, network_cmp,
            // bitcmp, varbitcmp), or an anonymous record of plain columns (`Key::eq_tie`, setop_core-0129); any other record,
            // an array or a user type keeps D2043's seen set
            let eqtie = k.proc != 0 && (k.eq_tie || gravity_fn::resolved::info(k.proc).is_some_and(|f| gravity_fn::resolved::pool_safe(s.cols[k.col as usize].tf.typid, f.fn_oid)));
            // KFLAG_WORDEXACT: a bit / varbit key's word is the whole value up to 56 bits (rep/keylane.rs `word_exact_marked`)
            let wexact = k.proc != 0 && crate::rep::keylane::word_exact_marked(&crate::rep::keylane::key_tf(&s.cols[k.col as usize].tf, k), k);
            // KFLAG_USER (USEROP): the user's comparator, called on the pairs PostgreSQL's qsort calls it on; KFLAG_TOTAL
            // (TOTAL-ORDER): a builtin comparator that reaches no user code, whose run keeps std's sort (`Key::total`)
            let hosted = if k.proc != 0 { h::KFLAG_HOSTED } else { 0 } | if eqtie { h::KFLAG_EQTIE } else { 0 } | if wexact { h::KFLAG_WORDEXACT } else { 0 } | if k.user { h::KFLAG_USER } else { 0 } | if k.total { h::KFLAG_TOTAL } else { 0 };
            w.extend([coll, ext | if bpchar { h::KFLAG_BPCHAR } else { 0 } | hosted]);
        }
        if s.keys.iter().any(|k| k.proc != 0) {
            w.extend(s.keys.iter().map(|k| k.proc));
        }
    }
    stack_slot_words(b, &w)
}

/// `words` as u32s in a stack slot (an emit table).
pub(crate) fn stack_words(b: &mut FunctionBuilder<'_>, words: &[u32]) -> Value {
    stack_slot_words(b, words).0
}

/// `stack_words` and its slot.
fn stack_slot_words(b: &mut FunctionBuilder<'_>, words: &[u32]) -> (Value, StackSlot) {
    let slot = b.declare_stack_slot(4 * words.len() as u32, 8);
    let tbl = b.stack_addr(slot);
    for (i, w) in words.iter().enumerate() {
        let v = b.iconst(Type::I32, *w as i64);
        b.store(tbl, 4 * i as i32, v);
    }
    (tbl, slot)
}
