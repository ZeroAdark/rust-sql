//! The function-source face (L3; a face file, no rows of its own; M4-RET's face/ret.rs pattern; cards/M5.md
//! `face/srf.rs`, M5-plan §1.2, §3.1, §3.2, Decision 4). Two things cross M5's files here:
//! - how a FROM entry's function runs (`Form`), decided per function by its `SrfKind` and never per query: the iterator
//!   (srf/iter.rs), a library entry over its cell (the H-a hosts), PG's own call materialized into PG's store (form (c)
//!   and srf.pl: the incumbent FunctionScan, M5-ENTRY-SPI R1), a scalar call in FROM, an inlined body's expression;
//! - the shared tuplestore reader (`StoreSource`, srf/store_read.rs), which function_scan.rs (form (c) in FROM),
//!   pl_function.rs, table_func.rs and the Named source call: leader-only, one host per window, the columns the pipeline
//!   reads deformed (M5-ENTRY-SPI §4.2 / §4.3).
//!
//! Owner M5-SERIES.
// Read by function_scan.rs and store_read.rs (Wave 1b); until they land, nothing names it.
#![allow(dead_code)]
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Value};
use mcx::{PgBox, PgVec};

use crate::cell::Row;
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;

/// How one function of a FROM entry runs (ROWS FROM: one per function).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Open-coded IR (srf/iter.rs), no host per element.
    Iter,
    /// A library entry (`gravity_fn::srf::ENTRIES[entry]`) opened and stepped over its cell; streamed in FROM only when
    /// every error it raises is raised at open, else materialized first as PG does (M5-plan §3.1 (b)).
    Library { entry: u16 },
    /// PG's own call through the incumbent FunctionScan, materialized into PG's store at the first pull, read by the
    /// store reader (§3.1 (c): every other builtin / C set function and a non-set function in FROM).
    Store,
    /// A PL / SQL body: srf.pl's (the same call protocol, pl_function.rs).
    Pl,
    /// A FROM entry that is not a call (`SrfKind::Expr`): an inlined body's expression, evaluated once for one row.
    Expr,
}

/// The rows a store holds as its reader deforms them: the reader's place (pipeline `k`, op `j` (`usize::MAX`: the
/// pipeline's source), its `member` within the step: its words' key), the plan node, each column's type (`pg_type` OID,
/// column `a` at `types[a - 1]`), and the columns the pipeline reads (bit `a - 1`; the others are never deformed).
#[derive(Clone, Debug)]
pub struct RowType<'g> {
    pub k: usize,
    pub j: usize,
    pub member: u8,
    pub node: i32,
    pub types: PgVec<'g, u32>,
    pub reads: u64,
}

/// One stored row's columns: cell `a - 1` is column `a`, with its IR value and NULL flag (a column the pipeline does not
/// read has no cell: its entry in `cols` is `None`).
pub struct StoreRow<'e> {
    pub row: Row<'e>,
    pub vals: PgVec<'e, (Value, Option<Value>)>,
    pub cols: PgVec<'e, Option<u16>>,
}

/// The shared reader over one PG tuplestore (store_read.rs; M5-ENTRY-SPI §4.2 / §4.3): leader-only; it never ends a store
/// it does not own; by-ref datums are copied into the batch's region before the next read.
pub trait StoreSource {
    /// Its per-open words (handle, read pointer, window, its count and base); the step that reads it declares them.
    fn open_words(&self, w: &mut OpenWords);
    /// At the open: store `h` through read pointer `ptr` (the caller's: 0 for a scan's own store, a named tuplestore's
    /// pointer allocated once per execution, as `ExecInitNamedTuplestoreScan` does).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, h: Value, ptr: Value);
    /// The window `[cursor, end)` holding store row `from` (`I64`: the rows the reader's caller has handed on, 0 at the
    /// open): the rest of the current window on a resume, else the next batch (one `h_store_batch`); an exhausted store
    /// branches to `l_eof`. A resume never re-reads the store from its first row.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block, from: Value) -> Win;
    /// Row `j` of the window.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value) -> StoreRow<'e>;
    /// PG's `tuplestore_rescan` through the open's handle and pointer: the next window reads from the store's first row.
    fn rewind(&self, b: &mut FunctionBuilder<'_>, env: &mut Env);
    /// D1874: `rewind`, first passing each value of a FROM call's SubPlans (plan id, datum word, NULL flag: the step
    /// evaluated them on its row) as the exec param standing in for that SubPlan in the store's own state, after its
    /// owner's params, so the rewind recomputes the call (srf/function_store.rs `fed`).
    fn rewind_fed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, fed: &[(i32, Value, Value)]);
}

/// The reader's maker, as store_read.rs registers it (`(KindId::SrfFunction, &StoreMaker(make))`).
pub struct StoreMaker(pub for<'g, 'p> fn(&mut Px<'g, 'p>, RowType<'g>) -> PgBox<'g, dyn StoreSource + 'g>);
