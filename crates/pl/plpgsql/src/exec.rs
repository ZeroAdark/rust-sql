// pl_exec.c. Statement set: block (incl. EXCEPTION sections), assign,
// if, loop/while/fori/fors, exit/continue, return, raise, assert, execsql
// (incl. INTO [STRICT]), dynexecute, perform, getdiag(row_count).
// Expressions ride SPI
// plans (saved; plancache invalidation is loud per repo discipline) with the
// simple-expression fast path over execexpr.
//
// Documented divergences from C:
// - Simple-expr ExprStates and cast ExprStates are cached per invocation
//   (estate), not per transaction: fast execexpr bakes the param-slot
//   address at compile, so a state cannot outlive its estate's param
//   buffer. Loops (the hot case) reuse within the invocation.
//
// Value ownership follows pl_exec.c: the invocation's datum context is an
// AllocSet, a Var's by-ref value is a datumCopy the variable owns (freeval)
// and pfrees on reassignment (assign_simple_var 8817-8825), a record's
// field images are owned per field and freed when the record or the field
// is replaced (assign_record_var 8854-8869 / expanded_record_set_field's
// "not part of the tuple" rule), and a FOR-over-query loop only swaps the
// tuple into the loop record while its tupdesc identity holds (5913-5920).
//
// Std collections justified as in ast.rs (invocation-lifetime bookkeeping,
// never per row on a steady path).
use std::collections::HashMap;

type FxHashMap<K, V> = HashMap<K, V, rustc_hash::FxBuildHasher>;
type FxHashSet<K> = std::collections::HashSet<K, rustc_hash::FxBuildHasher>;

use datum::{Datum, NullableDatum};
use mcx::{Mcx, MemoryContext};
use spi::{
    SPI_cursor_close, SPI_cursor_fetch, SPI_cursor_open, SPI_cursor_open_with_paramlist,
    SpiCursor, SpiPlanPtr, TuptabHandle,
};
use types_core::{Oid, OidIsValid};
use types_error::{PgError, PgResult, SqlState, ERROR};
use types_portal::params::{ParamExternData, PARAM_FLAG_CONST};

use crate::ast::*;
use crate::errcodes::EXCEPTION_LABEL_MAP;

// CHECK_FOR_INTERRUPTS() (miscadmin.h), the heapam pattern: cold seam call
// gated on the InterruptPending flag. A raised cancel/die comes back as the
// Err; exception_matches_conditions keeps OTHERS from catching it (C parity).
#[cold]
#[inline(never)]
fn process_interrupts() -> PgResult<()> {
    postgres_seams::check_for_interrupts::call()
}

#[inline(always)]
fn check_for_interrupts() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        return process_interrupts();
    }
    Ok(())
}

pub const RC_OK: i32 = 0;
pub const RC_EXIT: i32 = 1;
pub const RC_RETURN: i32 = 2;
pub const RC_CONTINUE: i32 = 3;

const BOOLOID: Oid = 16;
const INT8OID: Oid = 20;
const TEXTOID: Oid = 25;
const UNKNOWNOID: Oid = 705;
const RECORDOID: Oid = 2249;
const INTERNALOID: Oid = 2281;
const TYPTYPE_DOMAIN: i8 = b'd' as i8;

const CURSOR_OPT_NO_SCROLL: i32 = 0x0004;
const CURSOR_OPT_PARALLEL_OK: i32 = 0x0800;

pub(crate) struct Ctx {
    ptr: core::cell::Cell<*mut MemoryContext>,
    // The name a `lazy_aset` context is made under at its first `mcx()`.
    name: &'static str,
}

impl Ctx {
    fn made(ctx: MemoryContext, name: &'static str) -> Ctx {
        Ctx { ptr: core::cell::Cell::new(Box::into_raw(Box::new(ctx))), name }
    }
    // Reset-wholesale AllocSet scratch (C's eval_mcontext / stmt_mcontext).
    pub fn new(name: &'static str) -> Ctx {
        Ctx::made(MemoryContext::new_wholesale(name), name)
    }
    // A small, never-reset home (ALLOCSET_START_SMALL_SIZES).
    pub fn new_start_small(name: &'static str) -> Ctx {
        Ctx::made(MemoryContext::new_start_small(name), name)
    }
    // C's datum_context (plpgsql_estate_setup, pl_exec.c:4027: the SPI
    // procedure context, an AllocSet): values are pfree'd one at a time on
    // reassignment, so the backend must give chunks back. Never reset()
    // (the aset reset asserts nothing is still charged); dropped with the
    // estate. Made when the first value is copied into it (D3885): an
    // invocation whose variables are all by-value makes none, as C's
    // datum_context costs nothing when nothing is palloc'd in it.
    pub fn lazy_aset(name: &'static str) -> Ctx {
        Ctx { ptr: core::cell::Cell::new(core::ptr::null_mut()), name }
    }
    pub fn mcx(&self) -> Mcx<'static> {
        let mut p = self.ptr.get();
        if p.is_null() {
            p = Box::into_raw(Box::new(MemoryContext::new(self.name)));
            self.ptr.set(p);
        }
        // SAFETY: reclaimed only in Drop; handles do not outlive the estate.
        unsafe { (*p).mcx() }
    }
    pub fn reset(&self) {
        let p = self.ptr.get();
        if p.is_null() {
            return;
        }
        // SAFETY: as above; no live borrows at reset points (allocations are
        // raw datums, invalidated by contract like C's context reset).
        unsafe { (*p).reset() }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let p = self.ptr.get();
        if p.is_null() {
            return;
        }
        // SAFETY: Box::into_raw provenance.
        drop(unsafe { Box::from_raw(p) });
    }
}

// Cloning shares the field arrays: a record copy keeps its source's shape.
#[derive(Clone)]
pub struct RecDesc {
    /// C er_tupdesc_id: identifies this shape for the per-RECFIELD field
    /// index cache (C recfield->rectupledescid + finfo); 0 is never issued.
    pub id: u64,
    inner: std::rc::Rc<RecDescFields>,
}

pub struct RecDescFields {
    pub names: Vec<String>,
    pub types: Vec<Oid>,
    pub typmods: Vec<i32>,
    pub typlens: Vec<i16>,
    pub typbyvals: Vec<bool>,
    pub colls: Vec<Oid>,
    pub dropped: Vec<bool>,
}

impl core::ops::Deref for RecDesc {
    type Target = RecDescFields;
    fn deref(&self) -> &RecDescFields {
        &self.inner
    }
}

impl RecDesc {
    pub fn from_tupdesc(td: &types_tuple::TupleDescData<'_>) -> PgResult<RecDesc> {
        let natts = td.attrs.len();
        let mut d = RecDescFields {
            names: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            types: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            typmods: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            typlens: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            typbyvals: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            colls: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
            dropped: ::mcx::std_vec_with_capacity("plpgsql", natts)?,
        };
        for a in td.attrs.iter() {
            // attname as stored: expanded_record_lookup_field compares the
            // (scanner-downcased or quoted) field name with namestrcmp,
            // case-sensitively (expandedrecord.c:1032).
            d.names.push(String::from_utf8_lossy(a.attname.name_str()).into_owned());
            d.types.push(a.atttypid);
            d.typmods.push(a.atttypmod);
            d.typlens.push(a.attlen);
            d.typbyvals.push(a.attbyval);
            d.colls.push(a.attcollation);
            d.dropped.push(a.attisdropped);
        }
        Ok(RecDesc {
            id: NEXT_REC_DESC_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
            inner: std::rc::Rc::new(d),
        })
    }
}

// The heap tuple a record was bound from (C's expanded record fvalue, kept
// across field assignments); absent for a record built from fields.
#[derive(Clone, Copy)]
pub struct RecSysAttrs {
    ctid: types_tuple::ItemPointerData,
    tableoid: Oid,
    xmin: u32,
    xmax: u32,
    cmd: u32,
    // Invocation-scoped identity; never dereferenced and only returned to its trigger caller.
    pub trigger_tuple: Option<usize>,
}

impl RecSysAttrs {
    pub fn from_tuple(t: &types_tuple::HeapTupleData<'_>) -> RecSysAttrs {
        let h = t.t_data();
        RecSysAttrs {
            ctid: t.t_self,
            tableoid: t.t_tableOid,
            xmin: h.xmin_raw(),
            xmax: h.xmax_raw(),
            cmd: h.raw_command_id(),
            trigger_tuple: None,
        }
    }

    // heap_getsysattr; the ctid Datum points into self.
    pub fn get(&self, attnum: i16) -> Datum {
        match attnum as i32 {
            types_tuple::SelfItemPointerAttributeNumber => {
                Datum::from_usize(&self.ctid as *const _ as usize)
            }
            types_tuple::MinTransactionIdAttributeNumber => Datum::from_u32(self.xmin),
            types_tuple::MaxTransactionIdAttributeNumber => Datum::from_u32(self.xmax),
            types_tuple::MinCommandIdAttributeNumber
            | types_tuple::MaxCommandIdAttributeNumber => Datum::from_u32(self.cmd),
            types_tuple::TableOidAttributeNumber => Datum::from_oid(self.tableoid),
            other => panic!("invalid attnum: {other}"),
        }
    }
}

pub enum RecFieldRef {
    User(usize),
    Sys(&'static types_tuple::FormData_pg_attribute),
}

// Process-wide so a RecDesc id stays unique even if a session changes threads.
static NEXT_REC_DESC_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

// A field read or write costs one comparison, not a scan of the names
// (expanded_record_lookup_field runs only when the record's shape changed).
pub(crate) fn rec_lookup_field_cached(
    cache: &mut Vec<(u64, u32)>,
    dno: Dno,
    desc: &RecDesc,
    fieldname: &str,
) -> Option<RecFieldRef> {
    if let Some(&(id, i)) = cache.get(dno as usize) {
        if id == desc.id {
            return Some(RecFieldRef::User(i as usize));
        }
    }
    let r = rec_lookup_field(desc, fieldname);
    if let Some(RecFieldRef::User(i)) = r {
        let want = dno as usize + 1;
        if cache.len() < want && cache.try_reserve(want - cache.len()).is_ok() {
            cache.resize(want, (0, 0));
        }
        if let Some(slot) = cache.get_mut(dno as usize) {
            *slot = (desc.id, i as u32);
        }
    }
    r
}

// expanded_record_lookup_field: user columns, then system attributes.
pub fn rec_lookup_field(desc: &RecDesc, fieldname: &str) -> Option<RecFieldRef> {
    for (i, n) in desc.names.iter().enumerate() {
        if !desc.dropped[i] && n == fieldname {
            return Some(RecFieldRef::User(i));
        }
    }
    catalog_heap::SystemAttributeByName(fieldname).map(RecFieldRef::Sys)
}

fn compatible_tupdescs(src: &types_tuple::TupleDescData<'_>, dst: &types_tuple::TupleDescData<'_>) -> bool {
    if dst.attrs.len() != src.attrs.len() {
        return false;
    }
    dst.attrs.iter().zip(src.attrs.iter()).all(|(d, s)| {
        if d.attisdropped != s.attisdropped {
            false
        } else if !d.attisdropped {
            d.atttypid == s.atttypid && (d.atttypmod < 0 || d.atttypmod == s.atttypmod)
        } else {
            d.attlen == s.attlen && d.attalign == s.attalign
        }
    })
}

// ER_get_flat_size / ER_flatten_into (expandedrecord.c:698-725): a record
// flattened into a composite datum carries no external toast pointers.
pub(crate) fn detoast_external_fields(
    mcx: Mcx<'_>,
    td: &types_tuple::TupleDescData<'_>,
    values: &mut [Datum],
    nulls: &[bool],
) -> PgResult<()> {
    for i in 0..(td.natts as usize).min(values.len()) {
        let att = td.attr(i);
        if nulls[i] || att.attbyval || att.attlen != -1 {
            continue;
        }
        let p = values[i].as_usize() as *const u8;
        // SAFETY: non-null by-ref varlena datum.
        unsafe {
            if types_tuple::varatt::varatt_is_1b_e(p) {
                let attr = core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p));
                let out = detoast::detoast_external_attr(mcx, attr)?;
                values[i] = Datum::from_usize(out.leak().as_ptr() as usize);
            }
        }
    }
    Ok(())
}

// A deconstructed expanded record (values always deconstructed; src_desc
// keeps the physical tupdesc so the record can re-materialize as a
// composite Datum, dropped columns included).
pub struct RecValue {
    pub desc: RecDesc,
    pub values: Vec<Datum>,
    pub nulls: Vec<bool>,
    pub src_desc: Option<std::rc::Rc<types_tuple::TupleDescData<'static>>>,
    /// C ExpandedRecordIsEmpty: shape known, no row stored — reads as SQL
    /// NULL as a whole, fields read NULL, assignment makes it non-empty.
    pub empty: bool,
    pub sys: Option<std::rc::Rc<RecSysAttrs>>,
    /// C ER_FLAG_FVALUE_VALID: the stored tuple still matches the fields; a
    /// field assignment clears it but keeps `sys` readable.
    pub fvalue_valid: bool,
    /// Per field: the image in `values` is a copy this record made in the
    /// invocation's datum context (datumCopy layout) and frees when the field
    /// or the record is replaced — expandedrecord.c's "free the old field
    /// value if it is not part of the tuple". False for values that point
    /// into a caller-owned tuple or image.
    pub owned: Vec<bool>,
}

impl RecValue {
    pub(crate) fn original_trigger_tuple(&self, out: &types_tuple::TupleDescData<'_>) -> Option<usize> {
        if self.empty || !self.fvalue_valid || !compatible_tupdescs(self.src_desc.as_ref()?, out) {
            return None;
        }
        self.sys.as_ref()?.trigger_tuple
    }

    pub fn try_clone(&self) -> PgResult<RecValue> {
        Ok(RecValue {
            desc: self.desc.clone(),
            values: self.values.clone(),
            nulls: self.nulls.clone(),
            src_desc: self.src_desc.clone(),
            empty: self.empty,
            sys: self.sys.clone(),
            fvalue_valid: self.fvalue_valid,
            owned: ::mcx::std_vec_from_elem("plpgsql", false, self.values.len())?,
        })
    }
}

// A rec materialized into the eval scratch, keyed by the datum's address:
// the stand-in for C's expanded-record pointer identity.
#[derive(Clone)]
struct RecOrigin {
    recno: Dno,
    sys: Option<std::rc::Rc<RecSysAttrs>>,
    fvalue_valid: bool,
}

/// plpgsql_fulfill_promise's inputs (pl_exec.c:1384): the trigger facts a
/// TG_* variable is computed from, snapshotted at trigger entry (the
/// TriggerData borrow does not outlive the call) and consumed on the
/// variable's FIRST READ, as in C.
pub(crate) struct TriggerPromiseSource {
    pub tgname: String,
    pub event: u32,
    pub relid: types_core::Oid,
    pub relnamespace: types_core::Oid,
    pub relname: String,
    pub tgnargs: i16,
    pub tgargs: Vec<String>,
}

pub enum DatumVal {
    /// `freeval` is C PLpgSQL_var.freeval: the by-ref value is a datumCopy in
    /// the invocation's datum context that the variable owns and pfrees on
    /// reassignment; false for values that live in the caller's memory
    /// (arguments) or are by-value / NULL.
    /// `exp`: the value is held as a read/write expanded array (C's form for
    /// array variables, made on the first in-place use); `value` is then
    /// unused and readers get the object's flat image.
    Var { value: Datum, isnull: bool, freeval: bool, exp: Option<arrayfuncs::OwnedExpandedArray> },
    Rec(Option<RecValue>),
    None,
}

// pfree(DatumGetPointer(value)) for a copy made by copy_to_datum_ctx: the
// datumCopy layout (datumGetSize bytes at palloc alignment, datum.c) is
// recomputed from the stored image — the role of C's chunk header — so the
// AllocSet chunk returns to its own freelist. Expanded objects are never
// stored (agg_datum_copy flattens them), so a -1 image is always a flat or
// toast-pointer varlena whose varsize_any is the size that was allocated.
//
// # Safety
// `value` is a non-null by-ref datum previously returned by
// `copy_to_datum_ctx` INTO `mcx`, of `typlen` discipline, with no other
// owner (a freeval Var or an owned RecValue field).
unsafe fn free_datum_copy(mcx: Mcx<'_>, value: Datum, typlen: i16) {
    let p = value.as_usize() as *mut u8;
    if p.is_null() {
        return;
    }
    // SAFETY: forwarded caller contract.
    let size = unsafe {
        match typlen {
            -1 => types_tuple::varatt::varsize_any(p),
            n if n > 0 => n as usize,
            -2 => {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                len + 1
            }
            n => panic!("datumGetSize (datum.c): by-ref value with typlen {n} not ported"),
        }
    };
    let layout = core::alloc::Layout::from_size_align(size, 8).expect("datumCopy layout");
    // SAFETY: `p` is the live allocation of exactly this layout in `mcx`.
    unsafe {
        mcx::Allocator::deallocate(&mcx, core::ptr::NonNull::new_unchecked(p), layout);
    }
}

// The settings a raw parse depends on (scan.l). C parses an expression once
// and re-analyzes the retained raw tree on invalidation; a re-prepare here
// re-lexes, so it must do so under the first parse's settings, silently.
#[derive(Clone, Copy)]
struct LexSettings {
    backslash_quote: i32,
    standard_conforming_strings: bool,
    escape_string_warning: bool,
}

impl LexSettings {
    fn current() -> Self {
        LexSettings {
            backslash_quote: scan_fgram::gucs::backslash_quote(),
            standard_conforming_strings: scan_fgram::gucs::standard_conforming_strings(),
            escape_string_warning: scan_fgram::gucs::escape_string_warning(),
        }
    }

    fn install(self) -> Self {
        let prev = Self::current();
        scan_fgram::gucs::set_backslash_quote(self.backslash_quote);
        scan_fgram::gucs::set_standard_conforming_strings(self.standard_conforming_strings);
        scan_fgram::gucs::set_escape_string_warning(self.escape_string_warning);
        prev
    }
}

struct PlanEntry {
    plan: SpiPlanPtr,
    lex: LexSettings,
    // Rc slices: every execution reads these under the EXPR_PLANS borrow and
    // must release the borrow before running SPI — the clone is per
    // execution, so it must be a refcount bump, not a Vec copy (PROCPERF P2).
    paramnos: std::rc::Rc<[Dno]>,
    argtypes: std::rc::Rc<[Oid]>,
    // C stmt->mod_stmt (pl_exec.c exec_stmt_execsql: computed once, cached
    // via mod_stmt_set): command tags are parse-time facts of the plan
    // sources, fixed for the entry's lifetime; a re-prepare rebuilds the
    // entry and recomputes.
    mod_stmt: bool,
    // The parser-hook tables of the last (re-)analysis: plancache
    // revalidation outside this statement's own SPI call analyzes under them;
    // inside it, under the running execution's (reanalyze_plpgsql_expr).
    hooks: std::rc::Rc<HookSnapshot>,
    // C PLpgSQL_expr's expr_simple_* fields (pl_exec.c): the compiled
    // simple-expression fast path, FUNCTION lifetime like C's (C stores it
    // in the function's AST; the side table keeps the shared AST immutable).
    // Dropped with the entry (re-prepare, free_function_plans), so a Ready
    // state implies its SPI plan and plansource are live.
    simple: SimpleState,
}

struct HookSnapshot {
    names: Vec<(String, Dno, Oid, i32, Oid)>,
    params_by_dno: Vec<Option<(Oid, i32, Oid)>>,
    arg_dnos: Vec<Dno>,
    recs: Vec<(String, String)>,
    valueless: Vec<(usize, String)>,
    recfields: Vec<String>,
    resolve_option: parser_small1::PlpgsqlResolveOption,
}

// RevalidateCachedQuery's re-analysis arm for a plpgsql expression source
// (C: plpgsql_parser_setup with parserSetupArg = expr): names and types
// resolve against the running execution (C expr->func->cur_estate) when the
// statement is the one it is executing, else against the tables the plan
// was prepared with. The datums the analysis references become the entry's
// parameter list for later executions (C resolve_column_ref adds them to
// expr->paramnos).
fn reanalyze_plpgsql_expr(
    _h: plancache::CachedPlanSourceHandle,
    qmcx: Mcx<'static>,
    raw: &'static types_nodes::rawnodes::RawStmt<'static>,
    query_string: &'static str,
    _param_types: &'static [Oid],
    query_env: types_portal::QueryEnvHandle,
    arg: i32,
) -> PgResult<mcx::PgVec<'static, types_nodes::parsenodes::Query<'static>>> {
    let expr_id = arg as u32;
    let live = LIVE_SPI_EXPR.get();
    let live = (live.0 != 0 && {
        // SAFETY: set by LiveSpiExpr for the duration of the SPI call that
        // runs this statement; the expression is the function's AST node.
        unsafe { &*(live.1 as *const PlExpr) }.expr_id == expr_id
    })
    .then_some(live);
    let snap = match live {
        Some((estate, expr)) => {
            // SAFETY: the estate running the SPI call that revalidates (the
            // spi_param_fetch contract: nothing else uses it while SPI runs).
            let estate = unsafe { &mut *(estate as *mut Estate<'_>) };
            // SAFETY: as above.
            std::rc::Rc::new(estate.hook_snapshot(unsafe { &*(expr as *const PlExpr) })?)
        }
        None => EXPR_PLANS
            .with(|t| t.borrow().get(&expr_id).map(|e| std::rc::Rc::clone(&e.hooks)))
            .expect("reanalyze_plpgsql_expr: plan entry lives while its source does"),
    };
    let used = core::cell::RefCell::new(Vec::new());
    let name_entries: Vec<parser_small1::PlpgsqlNameEntry> = snap
        .names
        .iter()
        .map(|(key, dno, t, m, c)| parser_small1::PlpgsqlNameEntry {
            key,
            dno: *dno,
            typoid: *t,
            typmod: *m,
            collation: *c,
        })
        .collect();
    let rec_names: Vec<(&str, &str)> = snap.recs.iter()
        .map(|(key, name)| (key.as_str(), name.as_str())).collect();
    let valueless_names: Vec<(usize, &str)> =
        snap.valueless.iter().map(|(r, s)| (*r, s.as_str())).collect();
    let recfield_keys: Vec<&str> = snap.recfields.iter().map(String::as_str).collect();
    let hooks = parser_small1::PlpgsqlHookState {
        names: &name_entries,
        params_by_dno: &snap.params_by_dno,
        arg_dnos: &snap.arg_dnos,
        recs: &rec_names,
        valueless_recs: &valueless_names,
        recfields: &recfield_keys,
        resolve_option: snap.resolve_option,
        used: &used,
    };
    let query =
        analyze_seams::parse_analyze_plpgsql::call(qmcx, raw, query_string, &hooks, query_env)?;
    let queries = if query.commandType == types_nodes::nodes_enums::CmdType::CMD_UTILITY {
        let mut v = mcx::PgVec::new_in(qmcx);
        v.try_reserve_exact(1).map_err(|_| qmcx.oom(1))?;
        v.push(query);
        v
    } else {
        rewrite_handler_seams::query_rewrite::call(qmcx, query)?
    };
    let mut paramnos = used.into_inner();
    if let Some((_, expr)) = live {
        // SAFETY: as above.
        let expr = unsafe { &*(expr as *const PlExpr) };
        if is_assign_mode(expr.parse_mode) {
            drop_unread_assign_target(&mut paramnos, &queries)?;
        }
    }
    paramnos.sort_unstable();
    EXPR_PLANS.with(|t| {
        if let Some(e) = t.borrow_mut().get_mut(&expr_id) {
            e.paramnos = paramnos.into();
            if live.is_some() {
                e.argtypes = argtypes_of(&snap.params_by_dno);
                e.hooks = snap;
            }
        }
    });
    Ok(queries)
}

fn is_assign_mode(mode: parser_seams::RawParseMode) -> bool {
    matches!(
        mode,
        parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_ASSIGN1
            | parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_ASSIGN2
            | parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_ASSIGN3
    )
}

fn argtypes_of(params_by_dno: &[Option<(Oid, i32, Oid)>]) -> std::rc::Rc<[Oid]> {
    params_by_dno
        .iter()
        .map(|s| s.map(|(t, _, _)| t).unwrap_or(types_core::InvalidOid))
        .collect()
}

// Marks a statement's SPI call as running for the scope; restores on exit.
struct LiveSpiExpr((usize, usize));

impl LiveSpiExpr {
    fn set(estate: *mut (), expr: &PlExpr) -> LiveSpiExpr {
        LiveSpiExpr(LIVE_SPI_EXPR.replace((estate as usize, core::ptr::from_ref(expr) as usize)))
    }
}

impl Drop for LiveSpiExpr {
    fn drop(&mut self) {
        LIVE_SPI_EXPR.set(self.0);
    }
}

std::thread_local! {
    // expr_id -> saved SPI plan (C stores expr->plan in the function AST;
    // the side table keeps the shared AST immutable). Entries die with the
    // compiled function (free_function_plans).
    // tls-dtor: try_with-safe — SimpleExpr::drop is proc_exit-guarded and plancache CACHE is ManuallyDrop (dtor-less), so late releases cannot hit a destroyed key.
    static EXPR_PLANS: core::cell::RefCell<ExprSlab> =
        const { core::cell::RefCell::new(ExprSlab { slots: Vec::new() }) };
    // expr_id -> CALL OUT-arg row varnos (C stmt->target, cached in fn_cxt).
    // expr_id -> simple-expression state carried across a re-prepare.
    static SIMPLE_CARRY: core::cell::RefCell<FxHashMap<u32, SimpleCarry>> =
        core::cell::RefCell::new(FxHashMap::default());
    static CALL_TARGETS: core::cell::RefCell<FxHashMap<u32, Vec<Dno>>> =
        core::cell::RefCell::new(FxHashMap::default());
}

// C keeps an expression's plan and simple-expression state in the
// PLpgSQL_expr itself. Expression ids are dense (comp.rs reissues freed ids),
// so the side table is a slab indexed by id: an evaluation's take/put is an
// index, not a hash lookup.
struct ExprSlab {
    slots: Vec<Option<PlanEntry>>,
}

impl ExprSlab {
    #[inline]
    fn get(&self, id: &u32) -> Option<&PlanEntry> {
        self.slots.get(*id as usize).and_then(Option::as_ref)
    }

    #[inline]
    fn get_mut(&mut self, id: &u32) -> Option<&mut PlanEntry> {
        self.slots.get_mut(*id as usize).and_then(Option::as_mut)
    }

    #[inline]
    fn contains_key(&self, id: &u32) -> bool {
        self.get(id).is_some()
    }

    fn insert(&mut self, id: u32, entry: PlanEntry) -> Option<PlanEntry> {
        let i = id as usize;
        assert!(i <= self.slots.len() + (1 << 20), "plpgsql: expression id {id} is not from the id pool");
        while self.slots.len() <= i {
            self.slots.push(None);
        }
        self.slots[i].replace(entry)
    }

    fn remove(&mut self, id: &u32) -> Option<PlanEntry> {
        self.slots.get_mut(*id as usize).and_then(Option::take)
    }

    fn drain(&mut self) -> impl Iterator<Item = (u32, PlanEntry)> + '_ {
        self.slots.drain(..).enumerate().filter_map(|(i, e)| e.map(|e| (i as u32, e)))
    }
}

pub fn free_function_plans(expr_ids: &[u32]) {
    // Entries drop outside the borrow: SimpleExpr::drop releases plan pins
    // and fn_extra memos (arbitrary drop code must not run under the map's
    // RefCell borrow).
    let entries: Vec<PlanEntry> = EXPR_PLANS.with(|t| {
        let mut t = t.borrow_mut();
        expr_ids.iter().filter_map(|id| t.remove(id)).collect()
    });
    for e in entries {
        let PlanEntry { plan, simple, .. } = e;
        // Release the simple-expr plan pin before its plansource goes away
        // with the SPI plan (the reverse order is still safe — plancache
        // tombstones a dropped source while refcounted plans survive — but
        // this keeps the common path off the tombstone lane).
        drop(simple);
        spi::SPI_freeplan(plan);
    }
    CALL_TARGETS.with(|t| {
        let mut t = t.borrow_mut();
        for id in expr_ids {
            t.remove(id);
        }
    });
    SIMPLE_CARRY.with(|t| {
        let mut t = t.borrow_mut();
        for id in expr_ids {
            t.remove(id);
        }
    });
    crate::comp::release_expr_ids(expr_ids);
}

enum SimpleBuild {
    NotSimple,
    /// SPI_plan_get_cached_plan failed: C has already reset the expression
    /// to not simple (pl_exec.c:6078-6085) and it stays so.
    PlanFailed(Box<PgError>),
    /// Simple, not compiled: C's ExecInitExprWithParams (and with it every
    /// EXECUTE ACL check) waits for the first evaluation in a transaction
    /// (pl_exec.c:6172), never exec_simple_check_plan.
    Planned,
    Ready(Box<SimpleExpr>),
}

// C keeps expr_simple_expr / expr_simple_in_use / expr_simple_lxid when
// RevalidateCachedQuery re-analyzes the source in place; the re-prepare that
// stands in for it carries them to the new entry. A simple expression is
// re-examined only if its replanning succeeds (pl_exec.c:6072-6121).
#[derive(Clone, Copy)]
enum SimpleCarry {
    Redetermine,
    NotSimple,
    InUse,
    Quarantined(u32),
}

// C PLpgSQL_expr expr_simple_expr/expr_simple_state lifecycle states.
enum SimpleState {
    /// Simplicity not yet determined (C: exec_simple_check_plan not run;
    /// here the check is deferred to the first evaluation, as before).
    Unknown,
    /// Determined not simple (C: expr_simple_expr left NULL, permanently —
    /// pl_exec.c:6036 returns false without ever rechecking).
    NotSimple,
    /// Compiled and idle, ready to evaluate.
    Ready(Box<SimpleExpr>),
    /// Taken out by an in-flight evaluation or (re)build (C
    /// expr_simple_in_use, pl_exec.c:6042): a re-entrant evaluation of the
    /// same expression — recursion via a function called inside it — takes
    /// the SPI path instead.
    InUse,
    /// The evaluation was aborted by an error in this transaction (C leaves
    /// expr_simple_in_use set, pl_exec.c:6004-6008 / :6042-6044): the SPI
    /// path serves the expression until the next transaction, which
    /// rebuilds from the plan.
    Quarantined(u32),
}

// The compiled simple-expression fast path for one expression (C
// PLpgSQL_expr.expr_simple_state + expr_simple_plan/plansource + the type
// fields exec_save_simple_expr stashes, pl_exec.c:8336-8350).
//
// Lifetime: FUNCTION-scoped, revalidated per evaluation. C re-pins the plan
// per transaction (expr_simple_plan_lxid + the resowner arm of
// CachedPlanIsSimplyValid, pl_exec.c:6060-6070) because its pins live in a
// transaction-lifetime resowner, and rebuilds the ExprState per transaction
// (expr_simple_lxid, pl_exec.c:6172) because its memory home — the shared
// simple_eval_estate — dies at transaction end (plpgsql_xact_cb,
// pl_exec.c:8701). Neither constraint exists here: the plan pin is a manual
// refcount and the program owns its memory (`ctx`), so both survive until
// CachedPlanIsSimplyValid fails. That check runs before EVERY evaluation
// (pl_exec.c:6060) and covers everything the compile bakes in: source and
// plan validity reflect relcache invals plus PROCOID/TYPEOID invalItems
// (inlined-function redefinition, ALTER DOMAIN constraint changes — see
// plancache "CoerceToDomain a TYPEOID" dependency note), and the
// search_path match is checked exactly as C does.
struct SimpleExpr {
    // Identity of the SPI plan this was built from; put_simple() refuses to
    // reinstall a state whose plan entry was re-prepared mid-evaluation.
    plan: SpiPlanPtr,
    state: mcx::PgBox<'static, execexpr::ExprState<'static>>,
    cplan: types_portal::CachedPlanHandle,
    psrc: plancache::CachedPlanSourceHandle,
    rettype: Oid,
    rettypmod: i32,
    // C expr_simple_lxid (pl_exec.c:6172): the compiled state (its function
    // usage-tracking opcodes included) lives one transaction; plpgsql_xact_cb
    // frees simple_eval_estate at every transaction end (pl_exec.c:8728).
    lxid: u32,
    // C expr_simple_mutable (exec_save_simple_expr, pl_exec.c:8349): only
    // expressions containing mutable functions need the CCI + fresh-snapshot
    // ceremony per evaluation (pl_exec.c:6198-6204).
    mutable: bool,
    // The top-level FuncExpr whose array result the assignment target adopts
    // (0 = none): a PL/pgSQL callee returns its array R/W to it.
    adopt_call: usize,
    // The expression is one Param of a record datum (dno, planned type):
    // a record assignment from it copies the record's fields directly.
    bare_rec: Option<(Dno, Oid)>,
    // Datum references compile to ParamCallback steps (C EEOP_PARAM_CALLBACK
    // via plpgsql_param_compile, pl_exec.c:6465-6488) that fetch through
    // simple_param_eval from the estate armed for each evaluation
    // (pl_exec.c:6154-6164), only when the program reaches them.
    // Owns every allocation `state` points into; declared last so it drops
    // after `state` (C: the state dies with simple_eval_estate's context).
    #[allow(dead_code)] // never read: held for ownership + drop order only
    ctx: Ctx,
}

impl Drop for SimpleExpr {
    fn drop(&mut self) {
        // fn_extra memos are real heap boxes released only here — arena
        // teardown never runs them (FuncFrame::new_in's contract; the
        // memleak4 estate-reset lesson).
        self.state.release_frames();
        // A FATAL mid-evaluation unwinds a stack-held (taken-out) state
        // AFTER proc_exit's callbacks force-cleared the plancache
        // (ReleaseAllCachedPlansAtExit); releasing then would be a
        // stale-handle panic INSIDE the unwind (abort). The thread is
        // exiting — the pin bookkeeping is already gone with the cache.
        if !elog::config::proc_exit_inprogress() {
            plancache::ReleaseCachedPlan(self.cplan);
        }
    }
}

std::thread_local! {
    // Once-per-process registration flag for the backend-exit release below.
    // "Process" = one task on this thread: the on_proc_exit list drains to
    // empty at every exit (ipc.c:217 on_proc_exit_index = 0), so a retained
    // pool thread's next task must register again; the callback clears the
    // flag as its last act.
    static EXIT_RELEASE_REGISTERED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    // The FuncExpr (address) whose array result the running simple
    // expression's assignment adopts; read and cleared at PL/pgSQL function
    // entry, restored at exit.
    static ACCEPT_ARRAY_RESULT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    // C shared_cast_hash (session casts of non-DO executions), dropped by
    // release_plpgsql_state_at_exit like the function hash.
    static SESSION_CASTS: core::cell::RefCell<FxHashMap<(Oid, i32, Oid, i32), SessionCast>> =
        core::cell::RefCell::new(FxHashMap::default());
    static SESSION_CAST_GEN: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    static SESSION_CASTS_RESET: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    // Registered once per thread: the callback tables outlive a retained
    // thread's claims and are fixed-capacity.
    static SESSION_CAST_CALLBACKS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    // (estate, expr) of the statement whose SPI call is running, for its
    // lock-time re-analysis; (0, 0) outside one.
    static LIVE_SPI_EXPR: core::cell::Cell<(usize, usize)> = const { core::cell::Cell::new((0, 0)) };
}

// Sets ACCEPT_ARRAY_RESULT for a scope, restoring it on every exit.
pub(crate) struct AcceptArrayResult(usize);

impl AcceptArrayResult {
    pub(crate) fn set(call: usize) -> AcceptArrayResult {
        AcceptArrayResult(ACCEPT_ARRAY_RESULT.replace(call))
    }

    pub(crate) fn outer(&self) -> usize {
        self.0
    }
}

impl Drop for AcceptArrayResult {
    fn drop(&mut self) {
        ACCEPT_ARRAY_RESULT.set(self.0);
    }
}

// Backend-exit teardown of plpgsql's per-process state: the compiled-function
// hash (pl_comp.c plpgsql_HashTable / funccache.c cfunc_hashtable:39, a
// TopMemoryContext table kept "for the life of the backend" — funccache.c:59-75)
// and every function's saved SPI plans (expr->plan, freed in C only by
// delete_function or process death) with their simple-expression plan pins.
//
// In C all of this dies with the process: a parallel worker is a fresh
// bgworker process per query (parallel.c:611 ParallelWorkerMain via
// postmaster.c:4105 StartBackgroundWorker), so no task ever inherits another
// task's compiled functions. This port's pool retains the worker THREAD across
// tasks (init_small::wretain), and its park runs the on_proc_exit chain —
// including plancache's ReleaseAllCachedPlansAtExit, which reclaims every
// CachedPlanSource the SPI plans point at. A FUNC_CACHE entry surviving that
// park would hand the next task a function whose SPI plans dereference
// dropped plancache handles (plancache source_mut stale-handle panic, seen as
// `ERROR ... CONTEXT: parallel worker` on the second forced-parallel call
// after a park). So the process-death lifecycle is made explicit here.
//
// Registered on the first compile (or first Ready simple state), i.e.
// strictly AFTER plancache's InitPlanCache registered
// ReleaseAllCachedPlansAtExit; the on_proc_exit list drains LIFO, so this runs
// FIRST — SPI_freeplan/DropCachedPlan see a live registry (on a warm-claimed
// thread whose registry is already torn down they are C's process-death
// abandonment: no-ops). Without it, EXPR_PLANS' thread-local destructor would
// ReleaseCachedPlan into an already-cleared plancache (stale-handle panic;
// TLS destructor order across crates is not a lifecycle).
fn release_plpgsql_state_at_exit(_code: i32, _arg: usize) {
    // The function hash first (pl_comp.c: functions own their expressions'
    // plans); entries drop outside the map borrow.
    crate::handler::release_func_cache_at_exit();
    let entries: Vec<PlanEntry> = EXPR_PLANS.with(|t| t.borrow_mut().drain().map(|(_, e)| e).collect());
    // Drops (plan pins, fn_extra) run outside the map borrow, plancache
    // still intact; simple pin before its plansource (free_function_plans
    // order).
    for e in entries {
        let PlanEntry { plan, simple, .. } = e;
        drop(simple);
        spi::SPI_freeplan(plan);
    }
    CALL_TARGETS.with(|t| t.borrow_mut().clear());
    // D4312: with the function hash, the plans and the CALL targets gone, and the simple-expression carry with them,
    // no expression id is keyed on this thread any more, so the id pool starts over for a retained standby's next task.
    SIMPLE_CARRY.with(|t| t.borrow_mut().clear());
    crate::comp::reset_expr_ids();
    let casts = SESSION_CASTS.with(|t| core::mem::take(&mut *t.borrow_mut()));
    drop(casts);
    SESSION_CASTS_RESET.set(false);
    EXIT_RELEASE_REGISTERED.with(|c| c.set(false));
}

pub(crate) fn register_exit_release() {
    EXIT_RELEASE_REGISTERED.with(|c| {
        if !c.get() {
            // installed() guard: unit-test rigs run without ipc.
            if ipc_seams::on_proc_exit::is_installed() {
                ipc_seams::on_proc_exit::call(release_plpgsql_state_at_exit, 0);
            }
            c.set(true);
        }
    });
}

// The plpgsql_param_eval_* family behind plpgsql_param_compile's
// EEOP_PARAM_CALLBACK steps (pl_exec.c:6465-6488; _var :6667, _var_ro :6693,
// _recfield :6739, _generic :6803): fetch the datum's CURRENT value when the
// executor reaches the Param, with the per-eval type safety check
// (pl_exec.c:6797/6838) that datum_as_param carries. `arg` is the evaluating
// estate (C paramFetchArg), armed by eval_simple_body.
unsafe fn simple_param_eval(arg: *mut (), paramid: i32, paramtype: Oid) -> PgResult<NullableDatum> {
    assert!(!arg.is_null(), "plpgsql: simple expression evaluated without an armed estate");
    // SAFETY: eval_simple_body arms `arg` from its own `&mut Estate`
    // immediately before running the program and uses nothing else through
    // that borrow until the program returns (C's ecxt_param_list_info->
    // paramFetchArg contract); the evaluation is single-threaded.
    let estate = unsafe { &mut *arg.cast::<Estate<'_>>() };
    // paramid's are 1-based, dnos 0-based (pl_exec.c:6579).
    let (value, isnull) = estate.datum_as_param(paramid - 1, Some(paramtype))?;
    Ok(NullableDatum { value, isnull })
}

// plpgsql_param_eval_var's read/write delivery for the expression's
// expr_rw_param (pl_exec.c:6573-6580).
unsafe fn simple_param_eval_rw(arg: *mut (), paramid: i32, paramtype: Oid) -> PgResult<NullableDatum> {
    assert!(!arg.is_null(), "plpgsql: simple expression evaluated without an armed estate");
    // SAFETY: as simple_param_eval.
    let estate = unsafe { &mut *arg.cast::<Estate<'_>>() };
    let dno = paramid - 1;
    if estate.var_type(dno).typoid != paramtype {
        return unsafe { simple_param_eval(arg, paramid, paramtype) };
    }
    let (value, isnull) = estate.get_var_rw(dno)?;
    Ok(NullableDatum { value, isnull })
}

// plpgsql_param_eval_var_transfer (pl_exec.c:6616): the variable's R/W
// expanded array moves to the expression's context and the variable reads
// NULL until the assignment takes the result back.
unsafe fn simple_param_eval_transfer(arg: *mut (), paramid: i32, paramtype: Oid) -> PgResult<NullableDatum> {
    assert!(!arg.is_null(), "plpgsql: simple expression evaluated without an armed estate");
    // SAFETY: as simple_param_eval.
    let estate = unsafe { &mut *arg.cast::<Estate<'_>>() };
    let dno = paramid - 1;
    if estate.var_type(dno).typoid != paramtype {
        return unsafe { simple_param_eval(arg, paramid, paramtype) };
    }
    let (value, isnull) = estate.transfer_var_out(dno)?;
    Ok(NullableDatum { value, isnull })
}

// plpgsql_param_eval_var_ro for a Param whose consumer reads expanded
// arrays (Estate::expanded_reader_params): the R/O expanded pointer.
unsafe fn simple_param_eval_ro(arg: *mut (), paramid: i32, paramtype: Oid) -> PgResult<NullableDatum> {
    assert!(!arg.is_null(), "plpgsql: simple expression evaluated without an armed estate");
    // SAFETY: as simple_param_eval.
    let estate = unsafe { &mut *arg.cast::<Estate<'_>>() };
    let dno = paramid - 1;
    if !estate.promise_pending.is_empty() {
        estate.fulfill_promise(dno)?;
    }
    if estate.var_type(dno).typoid == paramtype {
        let mcx = estate.datum_ctx.mcx();
        if let DatumVal::Var { value, isnull: false, exp, .. } = &mut estate.datums[dno as usize] {
            // C holds every array variable expanded (exec_assign_value), so
            // a[i] on varlena or NULL-bearing elements is O(1), not a walk.
            if exp.is_none() {
                *exp = Some(arrayfuncs::expand_array_owned(*value, mcx.context(), None)?);
            }
            return Ok(NullableDatum { value: exp.as_ref().unwrap().ro_datum(), isnull: false });
        }
    }
    unsafe { simple_param_eval(arg, paramid, paramtype) }
}

// C paramFetchArg (the estate) + parserSetupArg (the expression, whose
// paramnos gate plpgsql_param_fetch) for one SPI call.
struct SpiFetchArg {
    estate: *mut (),
    paramnos: std::rc::Rc<[Dno]>,
}

// The PL's list for SPI: its fetch only when a slot the expression references
// (arg.paramnos) was left without a value (setup_param_list's speculative
// fetch), so a list whose every referenced datum was fetched up front stays a
// plain hooked list (D2834: Gravity admits it). A datum the expression does
// not reference has no planned type (argtypes_of: InvalidOid) and no Param
// reaches it.
fn spi_param_list<'p>(params: &'p [ParamExternData], arg: &mut SpiFetchArg) -> spi::SpiParamList<'p> {
    let missing =
        arg.paramnos.iter().any(|&d| params.get(d as usize).is_some_and(|p| p.ptype == types_core::InvalidOid));
    let fetch = missing.then(|| {
        types_portal::params::ParamFetchHook {
            func: spi_param_fetch,
            arg: (arg as *mut SpiFetchArg).cast(),
        }
    });
    spi::SpiParamList { params, fetch }
}

// The SPI list's paramFetch for a slot setup_param_list left without a value
// (C's plpgsql_param_eval_recfield reached through the compiled Param): the
// non-speculative fetch, raising its error only now. A datum the expression
// does not reference is a dummy (pl_exec.c:6327).
unsafe fn spi_param_fetch(arg: *mut (), paramid: i32) -> PgResult<ParamExternData> {
    // SAFETY: the caller keeps its SpiFetchArg alive across the SPI call; its
    // estate pointer comes from the `&mut Estate` running that call.
    let arg = unsafe { &*arg.cast::<SpiFetchArg>() };
    let dno = paramid - 1;
    if arg.paramnos.binary_search(&dno).is_err() {
        return Ok(ParamExternData {
            value: Datum::null(),
            isnull: true,
            pflags: 0,
            ptype: types_core::InvalidOid,
        });
    }
    // SAFETY: as above; nothing else uses the estate while SPI runs.
    let estate = unsafe { &mut *arg.estate.cast::<Estate<'_>>() };
    Ok(estate
        .param_fetch(dno, types_core::InvalidOid, false)?
        .expect("a non-speculative fetch yields a value"))
}

// Array-typed Vars hold R/W expanded arrays in C (pl_exec.c exec_assign_value
// and the argument setup's "Force it to R/W expanded"); record fields and
// every other datum stay flat, which array_set_element tells apart by the
// datum header (arrayfuncs.c:2273).
unsafe fn simple_param_expanded(arg: *mut (), paramid: i32) -> bool {
    assert!(!arg.is_null(), "plpgsql: simple expression evaluated without an armed estate");
    // SAFETY: as simple_param_eval.
    let estate = unsafe { &*arg.cast::<Estate<'_>>() };
    matches!(&estate.func.datums[(paramid - 1) as usize], PlDatum::Var(v) if v.datatype.typisarray)
}

// Take the expression's simple state for evaluation, leaving InUse behind
// (C expr_simple_in_use = true). `take_unknown` gates whether an
// undetermined expression may proceed to a build: only the post-ensure_plan
// caller may build (the plan probe there refreshes stale hook tables first).
enum SimpleTake {
    /// Not eligible right now: no entry, not simple, or already in use.
    Skip,
    Ready(Box<SimpleExpr>),
    /// Undetermined; caller owns the build. Carries the plan identity so no
    /// second map borrow is required.
    Build { plan: SpiPlanPtr },
}

fn take_simple(expr_id: u32, take_unknown: bool) -> SimpleTake {
    EXPR_PLANS.with(|t| {
        let mut t = t.borrow_mut();
        let Some(e) = t.get_mut(&expr_id) else {
            return SimpleTake::Skip;
        };
        match e.simple {
            SimpleState::NotSimple | SimpleState::InUse => SimpleTake::Skip,
            SimpleState::Quarantined(lxid) if lxid == current_lxid() => SimpleTake::Skip,
            SimpleState::Ready(_) => {
                let SimpleState::Ready(se) =
                    core::mem::replace(&mut e.simple, SimpleState::InUse)
                else {
                    unreachable!()
                };
                SimpleTake::Ready(se)
            }
            SimpleState::Unknown | SimpleState::Quarantined(_) => {
                if !take_unknown {
                    return SimpleTake::Skip;
                }
                e.simple = SimpleState::InUse;
                SimpleTake::Build { plan: e.plan }
            }
        }
    })
}

// Put a taken state back (C expr_simple_in_use = false). If the entry was
// re-prepared or freed while the evaluation ran (a nested SPI evaluation of
// the same expression can do both), the taken state is orphaned: dropping it
// releases its pin, and the new entry redetermines from scratch.
fn put_simple(expr_id: u32, plan: SpiPlanPtr, state: SimpleState) {
    let orphan = EXPR_PLANS.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(e) = t.get_mut(&expr_id) {
            if matches!(e.simple, SimpleState::InUse) {
                if e.plan == plan {
                    e.simple = state;
                    return None;
                }
                // A re-prepare mid-evaluation carried this evaluation's
                // busy mark to the new entry; its end releases it there.
                let (next, orphan) = match state {
                    SimpleState::Ready(se) => (SimpleState::Unknown, Some(SimpleState::Ready(se))),
                    other => (other, None),
                };
                e.simple = next;
                return orphan;
            }
        }
        Some(state)
    });
    // Foreign drop code (plan pins, fn_extra) runs outside the map borrow.
    drop(orphan);
}

// The coercion tree exec_cast_value evaluates (get_cast_hashentry,
// pl_exec.c:7985-8040) over a Param placeholder; None = no-op.
fn build_cast_expr(
    mcx: Mcx<'static>,
    srctype: Oid,
    srctypmod: i32,
    dsttype: Oid,
    dsttypmod: i32,
) -> PgResult<Option<types_nodes::Node<'static>>> {
    use types_nodes::primnodes::{CoercionForm, Param, ParamKind};
    let placeholder = types_nodes::Node::mk(
        mcx,
        Param {
            paramkind: ParamKind::PARAM_EXTERN,
            paramid: 1,
            paramtype: srctype,
            paramtypmod: srctypmod,
            paramcollid: lsyscache::typ::get_typcollation(srctype)?,
            location: -1,
        },
    )?;

    let pstate = parser_small1::make_parsestate(mcx, None);
    let cast_expr = if srctype == UNKNOWNOID || srctype == RECORDOID {
        None
    } else {
        coerce::coerce_to_target_type(
            mcx,
            &pstate,
            placeholder,
            srctype,
            dsttype,
            dsttypmod,
            coerce::CoercionContext::COERCION_PLPGSQL,
            CoercionForm::COERCE_IMPLICIT_CAST,
            -1,
        )?
    };
    let cast_expr = match cast_expr {
        Some(e) => Some(e),
        None => {
            // upstream 54649de65f08 (18.6): Reject calls from SQL to functions that take or return type internal.
            if srctype == INTERNALOID || dsttype == INTERNALOID {
                return Err(exec_err(
                    types_error::ERRCODE_CANNOT_COERCE,
                    format!(
                        "cannot cast type {} to {}",
                        format_type::format_type_be(srctype)?,
                        format_type::format_type_be(dsttype)?
                    ),
                ));
            }
            let io = types_nodes::Node::mk(
                mcx,
                types_nodes::primnodes::CoerceViaIO {
                    arg: placeholder,
                    resulttype: dsttype,
                    resultcollid: types_core::InvalidOid,
                    coerceformat: CoercionForm::COERCE_IMPLICIT_CAST,
                    location: -1,
                },
            )?;
            if dsttypmod != -1 {
                coerce::coerce_to_target_type(
                    mcx,
                    &pstate,
                    io,
                    dsttype,
                    dsttype,
                    dsttypmod,
                    coerce::CoercionContext::COERCION_ASSIGNMENT,
                    CoercionForm::COERCE_IMPLICIT_CAST,
                    -1,
                )?
            } else {
                Some(io)
            }
        }
    };
    parser_small1::free_parsestate(pstate)?;

    let Some(cast_expr) = cast_expr else {
        return Ok(None);
    };
    // No-op relabeling of the bare placeholder: skip evaluation.
    if let Some(r) = cast_expr.as_relabel_type() {
        if r.arg.as_variant::<Param>().is_some() {
            return Ok(None);
        }
    }
    Ok(Some(cast_expr))
}

#[allow(clippy::type_complexity)]
fn compile_cast(
    mcx: Mcx<'static>,
    expr: Option<types_nodes::Node<'static>>,
    srctype: Oid,
) -> PgResult<(Option<mcx::PgBox<'static, execexpr::ExprState<'static>>>, Box<[ParamExternData; 1]>)> {
    let param: Box<[ParamExternData; 1]> = Box::new([ParamExternData {
        value: Datum::null(),
        isnull: true,
        pflags: 0,
        ptype: srctype,
    }]);
    let Some(expr) = expr else {
        return Ok((None, param));
    };
    let bind = types_portal::params::ParamBind {
        // SAFETY: `param` is a stable Box kept alongside the compiled state.
        extern_params: Some(unsafe { core::slice::from_raw_parts(param.as_ptr(), 1) }),
        exec_vals: None,
        n_exec: 0,
        param_callback: None,
        param_expanded: None,
        param_rw: None,
        param_ro_expanded: None,
        extern_fetch: None,
    };
    let state = execexpr::exec_init_expr(mcx, Some(expr), bind)?;
    Ok((state, param))
}

// C extract_query_dependencies over a cached expression (setrefs.c
// fix_expr_common): user functions and domains it calls, as syscache
// (cacheid, hashvalue) pairs; built-ins below FirstUnpinnedObjectId never
// change.
fn cast_expr_dependencies(expr: types_nodes::Node<'static>) -> PgResult<Vec<(i32, u32)>> {
    struct Deps(Vec<(i32, u32)>);
    impl Deps {
        fn add(&mut self, cacheid: i32, oid: Oid) -> PgResult<()> {
            if oid >= 12000 {
                let hv = cache_syscache::GetSysCacheHashValue(
                    cacheid,
                    cache_syscache::SysCacheKey::Value(Datum::from_oid(oid)),
                    cache_syscache::SysCacheKey::UNUSED,
                    cache_syscache::SysCacheKey::UNUSED,
                    cache_syscache::SysCacheKey::UNUSED,
                )?;
                self.0.push((cacheid, hv));
            }
            Ok(())
        }
    }
    impl<'mcx> nodes_core::NodeWalker<'mcx> for Deps {
        fn visit(&mut self, node: types_nodes::Node<'mcx>) -> PgResult<bool> {
            use types_nodes::primnodes::{CoerceToDomain, FuncExpr, OpExpr};
            if let Some(f) = node.as_variant::<FuncExpr>() {
                self.add(cache_syscache::PROCOID, f.funcid)?;
            } else if let Some(o) = node.as_variant::<OpExpr>() {
                self.add(cache_syscache::PROCOID, o.opfuncid)?;
            } else if let Some(d) = node.as_variant::<CoerceToDomain>() {
                self.add(cache_syscache::TYPEOID, d.resulttype)?;
            }
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut d = Deps(Vec::new());
    nodes_core::NodeWalker::visit(&mut d, expr)?;
    Ok(d.0)
}

// One compiled cast; the expression context outlives the state compiled
// from it (field order: state first).
struct CastState {
    state: mcx::PgBox<'static, execexpr::ExprState<'static>>,
    param: Box<[ParamExternData; 1]>,
    #[allow(dead_code)]
    ctx: Ctx,
    _expr: std::rc::Rc<Ctx>,
}

struct SessionCast {
    expr: Option<types_nodes::Node<'static>>,
    deps: Vec<(i32, u32)>,
    valid: bool,
    generation: u64,
    state: Option<CastState>,
    lxid: u32,
    expr_ctx: std::rc::Rc<Ctx>,
}

// PlanCacheFuncCallback / PlanCacheSysCallback for cached cast expressions.
fn session_cast_inval(_arg: Datum, cacheid: i32, hashvalue: u32) {
    let all = cacheid != cache_syscache::PROCOID && cacheid != cache_syscache::TYPEOID;
    SESSION_CASTS.with(|t| match t.try_borrow_mut() {
        Ok(mut t) => {
            for e in t.values_mut() {
                if all || e.deps.iter().any(|&(c, h)| c == cacheid && (hashvalue == 0 || h == hashvalue)) {
                    e.valid = false;
                }
            }
        }
        Err(_) => SESSION_CASTS_RESET.set(true),
    });
}

fn ensure_session_cast_callbacks() -> PgResult<()> {
    if SESSION_CAST_CALLBACKS.get() {
        return Ok(());
    }
    for cacheid in [
        cache_syscache::PROCOID,
        cache_syscache::TYPEOID,
        cache_syscache::NAMESPACEOID,
        cache_syscache::OPEROID,
        cache_syscache::AMOPOPID,
        cache_syscache::FOREIGNSERVEROID,
        cache_syscache::FOREIGNDATAWRAPPEROID,
    ] {
        inval::invalidate::CacheRegisterSyscacheCallback(cacheid, session_cast_inval, Datum::null())?;
    }
    SESSION_CAST_CALLBACKS.set(true);
    Ok(())
}

struct CastEntry {
    // None = no-op relabeling.
    state: Option<mcx::PgBox<'static, execexpr::ExprState<'static>>>,
    // Stable slot the compiled Param step points into.
    param: Box<[ParamExternData; 1]>,
    // PlanCacheInvalCounter at build; any bump forces a rebuild (coarse
    // stand-in for C's per-CachedExpression is_valid, pl_exec.c:7982).
    inval_gen: u64,
    // C cast_lxid: the ExprState is rebuilt each transaction so baked-in
    // domain constraint sets stay fresh (get_cast_hashentry, pl_exec.c:8109).
    lxid: u32,
}

#[derive(Clone, Copy)]
pub struct RsiSnapshot {
    pub allowed_modes: u32,
    pub expected_desc: Option<core::ptr::NonNull<core::ffi::c_void>>,
}

pub struct Estate<'a> {
    pub func: &'a PlFunction,
    pub datums: Vec<DatumVal>,
    pub retval: Datum,
    // The calling assignment adopts this function's array result: RETURN of
    // an array variable hands its R/W object over instead of flattening.
    pub(crate) caller_adopts: bool,
    pub(crate) ret_expanded: Option<arrayfuncs::OwnedExpandedArray>,
    pub retisnull: bool,
    pub rettype: Oid,
    /// RETURN of a record variable (trigger tuple return protocol).
    pub ret_rec: Option<RecValue>,
    pub rsi: Option<RsiSnapshot>,
    pub tuple_store: Option<tuplestore::Tuplestore>,
    tuple_store_desc: Option<types_tuple::TupleDescData<'static>>,
    tuple_store_owner: types_resowner::ResourceOwner,
    pub cur_error: Option<Box<PgError>>,
    // The frames outside this function at catch time: C's errfinish baked
    // them into cur_error->context, here they attach on propagation, so
    // PG_EXCEPTION_CONTEXT reads them from the walk taken at the catch.
    cur_error_outer_context: Option<String>,
    pub eval_processed: u64,
    eval_tuptable: Option<TuptabHandle>,
    pub exitlabel: Option<String>,
    pub readonly_func: bool,
    pub atomic: bool,
    cast_cache: FxHashMap<(Oid, i32, Oid, i32), CastEntry>,
    // RECFIELD dno -> (RecDesc id, field index).
    recfield_idx: Vec<(u64, u32)>,
    // Invocation-lifetime var values (C's "procedure" context).
    datum_ctx: Ctx,
    // Per-evaluation scratch (C's eval_mcontext); reset by exec_eval_cleanup.
    eval_ctx: Ctx,
    // Execution-copy datatype changes (C mutates its per-estate datum copies;
    // the shared AST here is immutable). Consulted wherever a Var's declared
    // type feeds plan/param typing.
    var_type_overrides: FxHashMap<Dno, PlType>,
    // C's per-execution rec copy of rectypeid; absent = the compiled OID.
    rec_type_overrides: FxHashMap<Dno, Oid>,
    rec_origins: Vec<(usize, RecOrigin)>,
    bare_rec_wanted: bool,
    bare_rec_hit: Option<(Dno, Oid)>,
    pub frame: std::rc::Rc<FrameShared>,
    // Trigger promises (pl_exec.c plpgsql_fulfill_promise): the TG_* values
    // are computed on first read, not at entry, so a body that changes what
    // they describe (renames its own schema) reads the state at read time.
    trigger_promises: Option<TriggerPromiseSource>,
    // dnos whose promise is still unfulfilled (C: var->promise != NONE).
    promise_pending: Vec<Dno>,
}

// C's plpgsql_exec_error_callback arg, shared with the thread-local context
// stack so GET DIAGNOSTICS PG_CONTEXT can render every live frame without
// aliasing the estates.
pub struct FrameShared {
    pub sig: std::rc::Rc<str>,
    pub stmt: core::cell::Cell<Option<(i32, &'static str)>>,
    pub text: core::cell::Cell<Option<&'static str>>,
    // err_var: its declaration lineno wins over the statement's
    // (plpgsql_exec_error_callback, pl_exec.c).
    pub var_lineno: core::cell::Cell<Option<i32>>,
}

pub struct FrameGuard(u64);

impl FrameGuard {
    /// A context frame whose closure stays in the pusher's stack frame, as C's ErrorContextCallback does (elog
    /// `push_context_frame_borrowed`): no allocation per push, and the pop is the slot stack's top (D3885). Use it
    /// through `pl_frame!` / `spi_frame!`, which declare the closure first and the guard after it.
    ///
    /// # Safety
    /// `callback` stays alive and unmoved until this guard drops: declare the guard after the closure in the same scope.
    #[inline]
    pub unsafe fn push_borrowed(callback: &mut (dyn FnMut(&mut PgError) + '_), emit: bool) -> FrameGuard {
        // SAFETY: the caller's contract is the one push_context_frame_borrowed states.
        FrameGuard(unsafe { elog::push_context_frame_borrowed(callback, emit) })
    }
}

// The function's own frame (C plpgsql_exec_error_callback) for `$estate`, held to the end of the enclosing scope. C's
// errfinish walks error_context_stack for every elevel: notices and warnings emitted while this frame is live carry its
// context line on the wire (the ERROR path attaches on propagation instead).
macro_rules! pl_frame {
    ($estate:expr) => {
        let pl_frame_shared = $estate.frame.clone();
        let mut pl_frame_callback = move |e: &mut ::types_error::PgError| {
            e.add_context_line($crate::exec::frame_context_line_of(&pl_frame_shared));
        };
        // SAFETY: the closure is a local of this scope, never moved; the guard, declared after it, drops first.
        let _pl_frame = unsafe { $crate::exec::FrameGuard::push_borrowed(&mut pl_frame_callback, true) };
    };
}
pub(crate) use pl_frame;

// The SPI level's own _SPI_error_callback frame (spi crate SpiErrorContext, pushed by
// SPI_prepare*/SPI_execute*/SPI_cursor_open*) describes reports raised under it; this frame only mirrors it for
// GET DIAGNOSTICS PG_CONTEXT (get_error_context_stack), so its line is formatted when a walk reads it, not per
// statement (D3885). `$guard` names the guard (drop it to pop early).
macro_rules! spi_frame {
    ($guard:ident, $query:expr, $mode:expr) => {
        let mut spi_frame_callback = $crate::exec::spi_frame_callback($query, $mode);
        // SAFETY: the closure is a local of this scope, never moved; the guard, declared after it, drops first.
        let $guard = unsafe { $crate::exec::FrameGuard::push_borrowed(&mut spi_frame_callback, false) };
    };
}

pub(crate) fn spi_frame_callback<'q>(
    query: &'q str,
    mode: parser_seams::RawParseMode,
) -> impl FnMut(&mut PgError) + 'q {
    move |e: &mut PgError| e.add_context_line(spi_context_line(query, mode).as_str())
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        elog::pop_emit_context_callback(self.0);
    }
}

fn get_error_context_stack() -> String {
    elog::GetErrorContextStack().unwrap_or_default()
}

// _SPI_error_callback (spi.c): position becomes internal query/position;
// otherwise a parse-mode-shaped context line.
#[track_caller]
#[cold]
fn spi_ctx_err(
    mut e: Box<PgError>,
    query: &str,
    mode: parser_seams::RawParseMode,
) -> Box<PgError> {
    spi_error_context(&mut e, query, mode);
    e
}

// _SPI_error_callback (spi.c:2992) over one report.
fn spi_error_context(e: &mut PgError, query: &str, mode: parser_seams::RawParseMode) {
    if let Some(p) = e.cursor_position.filter(|&p| p > 0) {
        e.cursor_position = None;
        e.internal_position = Some(p);
        e.internal_query = Some(query.to_string());
        return;
    }
    // The spi crate's _SPI_error_callback port may already have handled this
    // query (transpose or context line); C runs the callback once per level.
    if e.internal_query.as_deref() == Some(query) {
        return;
    }
    let line = spi_context_line(query, mode);
    // Suppress only immediate repeats of the complete, possibly multiline entry;
    // recursive SPI levels repeat it legitimately with function frames interposed.
    if e.context.as_deref().is_some_and(|c| {
        c.strip_suffix(line.as_str())
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('\n'))
    }) {
        return;
    }
    e.add_context_line(line);
}

fn spi_context_line(query: &str, mode: parser_seams::RawParseMode) -> String {
    use parser_seams::RawParseMode as M;
    match mode {
        M::RAW_PARSE_PLPGSQL_EXPR => format!("PL/pgSQL expression \"{query}\""),
        M::RAW_PARSE_PLPGSQL_ASSIGN1
        | M::RAW_PARSE_PLPGSQL_ASSIGN2
        | M::RAW_PARSE_PLPGSQL_ASSIGN3 => format!("PL/pgSQL assignment \"{query}\""),
        _ => format!("SQL statement \"{query}\""),
    }
}

#[cold]
pub(crate) fn exec_err(code: SqlState, msg: String) -> Box<PgError> {
    Box::new(elog::ereport(ERROR).errcode(code).errmsg(msg).into_error())
}

// The compiled-param safety check's report (plpgsql_param_eval_recfield /
// plpgsql_param_eval_generic, pl_exec.c:6798-6803): paramid is dno+1.
#[track_caller]
#[cold]
fn param_type_mismatch(dno: Dno, current: Oid, planned: Oid) -> Box<PgError> {
    let cur = format_type::format_type_be(current)
        .unwrap_or_else(|_| format!("type {current}"));
    let plan = format_type::format_type_be(planned)
        .unwrap_or_else(|_| format!("type {planned}"));
    exec_err(
        types_error::ERRCODE_DATATYPE_MISMATCH,
        format!(
            "type of parameter {} ({cur}) does not match that when preparing the plan ({plan})",
            dno + 1
        ),
    )
}

// count_param_references (pl_exec.c): stops at the second reference.
struct TargetParamRefs {
    paramid: i32,
    count: u32,
    last: Option<*const ()>,
}

impl<'mcx> nodes_core::NodeWalker<'mcx> for TargetParamRefs {
    fn visit(&mut self, node: types_nodes::Node<'mcx>) -> PgResult<bool> {
        if let Some(p) = node.as_param() {
            if p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN
                && p.paramid == self.paramid
            {
                self.last = Some(p as *const types_nodes::primnodes::Param as *const ());
                self.count += 1;
                return Ok(self.count > 1);
            }
            return Ok(false);
        }
        nodes_core::expression_tree_walker(node, self)
    }
}

// See Estate::expanded_reader_params.
struct ExpandedReaderParams<'f> {
    func: &'f PlFunction,
    rw: Option<*const ()>,
    found: Vec<*const ()>,
}

impl ExpandedReaderParams<'_> {
    fn note(&mut self, arg: Option<types_nodes::Node<'_>>) {
        let Some(p) = arg.and_then(|a| a.as_param()) else {
            return;
        };
        let id = p as *const types_nodes::primnodes::Param as *const ();
        let dno = p.paramid - 1;
        if p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN
            && dno >= 0
            && (dno as usize) < self.func.datums.len()
            && self.rw != Some(id)
            && matches!(&self.func.datums[dno as usize],
                PlDatum::Var(v) if v.datatype.typisarray && v.datatype.typoid == p.paramtype)
        {
            self.found.push(id);
        }
    }
}

impl<'mcx> nodes_core::NodeWalker<'mcx> for ExpandedReaderParams<'_> {
    fn visit(&mut self, node: types_nodes::Node<'mcx>) -> PgResult<bool> {
        use types_nodes::primnodes::{FuncExpr, SubscriptingRef};
        // array_dims, array_ndims, array_lower, array_upper, array_length,
        // cardinality: PG_GETARG_ANY_ARRAY_P readers.
        const DIM_READERS: [Oid; 6] = [747, 748, 2091, 2092, 2176, 3179];
        if let Some(f) = node.as_variant::<FuncExpr>() {
            if DIM_READERS.contains(&f.funcid) {
                self.note(f.args.iter().next());
            }
        } else if let Some(r) = node.as_variant::<SubscriptingRef>() {
            if r.refassgnexpr.is_none() && r.reflowerindexpr.len() == 0 {
                self.note(r.refexpr);
            }
        }
        nodes_core::expression_tree_walker(node, self)
    }
}

// Dnos with a Param in the analyzed tree.
struct ExternParamDnos(Vec<Dno>);

impl<'mcx> nodes_core::NodeWalker<'mcx> for ExternParamDnos {
    fn visit(&mut self, node: types_nodes::Node<'mcx>) -> PgResult<bool> {
        if let Some(p) = node.as_param() {
            let dno = p.paramid - 1;
            if p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN
                && !self.0.contains(&dno)
            {
                self.0.push(dno);
            }
            return Ok(false);
        }
        if let Some(q) = node.as_query() {
            return nodes_core::query_tree_walker(q, self, 0);
        }
        nodes_core::expression_tree_walker(node, self)
    }

    fn visit_query_ref(&mut self, q: &'mcx types_nodes::parsenodes::Query<'mcx>) -> PgResult<bool> {
        nodes_core::query_tree_walker(q, self, 0)
    }
}

fn referenced_param_dnos<'mcx>(
    queries: &[types_nodes::parsenodes::Query<'mcx>],
) -> PgResult<Vec<Dno>> {
    let mut w = ExternParamDnos(Vec::new());
    for q in queries {
        nodes_core::query_tree_walker(q, &mut w, 0)?;
    }
    Ok(w.0)
}

// transformPLAssignStmt (analyze.c) hook-resolves the assignment target
// first (so it heads the hook-resolved list) and drops its Param unless
// indirection reads it. C fetches a paramnos member only when a Param
// evaluates it (lazy plpgsql_param_fetch; plpgsql_param_compile steps), so
// `r.f1 := $1` never runs pl_exec.c:6797's check on r.f1 — the RETURN does.
fn drop_unread_assign_target<'mcx>(
    paramnos: &mut Vec<Dno>,
    queries: &[types_nodes::parsenodes::Query<'mcx>],
) -> PgResult<()> {
    if let Some(&target) = paramnos.first() {
        if !referenced_param_dnos(queries)?.contains(&target) {
            paramnos.retain(|&d| d != target);
        }
    }
    Ok(())
}

impl<'a> Estate<'a> {
    pub fn new(func: &'a PlFunction, readonly_func: bool, atomic: bool) -> PgResult<Estate<'a>> {
        let mut datums = ::mcx::std_vec_with_capacity("plpgsql", func.datums.len())?;
        for d in &func.datums {
            datums.push(match d {
                PlDatum::Var(_) => {
                    DatumVal::Var { value: Datum::null(), isnull: true, freeval: false, exp: None }
                }
                PlDatum::Rec(_) => DatumVal::Rec(None),
                _ => DatumVal::None,
            });
        }
        Ok(Estate {
            func,
            datums,
            retval: Datum::null(),
            caller_adopts: false,
            ret_expanded: None,
            retisnull: true,
            rettype: types_core::InvalidOid,
            ret_rec: None,
            rsi: None,
            tuple_store: None,
            tuple_store_desc: None,
            tuple_store_owner: resowner::CurrentResourceOwner(),
            cur_error: None,
            cur_error_outer_context: None,
            eval_processed: 0,
            eval_tuptable: None,
            exitlabel: None,
            readonly_func,
            atomic,
            cast_cache: FxHashMap::default(),
            recfield_idx: Vec::new(),
            datum_ctx: Ctx::lazy_aset("PLpgSQL per-invocation values"),
            eval_ctx: Ctx::new("PLpgSQL eval scratch"),
            var_type_overrides: FxHashMap::default(),
            rec_type_overrides: FxHashMap::default(),
            rec_origins: Vec::new(),
            bare_rec_wanted: false,
            bare_rec_hit: None,
            trigger_promises: None,
            promise_pending: Vec::new(),
            frame: std::rc::Rc::new(FrameShared {
                sig: func.fn_signature.clone(),
                stmt: core::cell::Cell::new(None),
                text: core::cell::Cell::new(None),
                var_lineno: core::cell::Cell::new(None),
            }),
        })
    }

    fn var_type(&self, dno: Dno) -> &PlType {
        if let Some(t) = self.var_type_overrides.get(&dno) {
            return t;
        }
        match &self.func.datums[dno as usize] {
            PlDatum::Var(v) => &v.datatype,
            _ => panic!("plpgsql: datum {dno} is not a Var"),
        }
    }

    // assign_simple_var (pl_exec.c:8778-8830) after its detoast arm (which
    // assign_copy_to_datum_ctx runs before the copy): free the previous
    // value if the variable owns it, install the new one and record whether
    // it is ours to free. `freeable` is true only for a copy
    // copy_to_datum_ctx made (by-ref, non-null).
    pub fn set_var(&mut self, dno: Dno, value: Datum, isnull: bool, freeable: bool) {
        // assign_simple_var's `var->promise = PLPGSQL_PROMISE_NONE`
        // (pl_exec.c:8838): an assigned TG_* variable is never fulfilled.
        if let Some(pos) = self.promise_pending.iter().position(|&d| d == dno) {
            self.promise_pending.swap_remove(pos);
        }
        let typlen = self.var_type(dno).typlen;
        match &mut self.datums[dno as usize] {
            DatumVal::Var { value: v, isnull: n, freeval, exp } => {
                *exp = None;
                if *freeval {
                    // SAFETY: freeval marks a copy_to_datum_ctx copy in
                    // datum_ctx owned solely by this variable; every reader
                    // of the old image finished before the assignment (the
                    // new value was computed and copied first). The context
                    // is asked for only here: a by-value assignment never
                    // makes the lazily made datum_ctx (D3885).
                    unsafe { free_datum_copy(self.datum_ctx.mcx(), *v, typlen) };
                }
                *v = value;
                *n = isnull;
                *freeval = freeable && !isnull;
            }
            _ => panic!("plpgsql: assign to non-Var datum {dno}"),
        }
    }

    // exec_stmt_case's datatype swap (pl_exec.c:2584). A value left by an
    // earlier execution (a WHEN that raised skips the reset) is freed under
    // the type it was copied with; C's pfree needs no type.
    fn retype_var(&mut self, dno: Dno, ty: PlType) {
        self.set_var(dno, Datum::null(), true, false);
        self.var_type_overrides.insert(dno, ty);
    }

    // plpgsql_exec_function's IN-argument store (pl_exec.c:565): no copy, not
    // freeable, except assign_simple_var's non-atomic detoast arm
    // (pl_exec.c:8794-8811), whose detoasted copy the variable owns.
    pub(crate) fn set_arg_var(&mut self, dno: Dno, value: Datum, isnull: bool) -> PgResult<()> {
        // pl_exec.c:524-548: a R/W expanded argument is the caller's to give;
        // the variable takes it over.
        if !isnull && self.var_type(dno).typisarray {
            if let Some(obj) = arrayfuncs::adopt_transferred(value) {
                self.set_var_expanded(dno, obj);
                return Ok(());
            }
        }
        if !self.atomic && !isnull && self.var_type(dno).typlen == -1 {
            let p = value.as_usize() as *const u8;
            // SAFETY: non-null by-ref varlena datum.
            let external = unsafe {
                types_tuple::varatt::varatt_is_1b_e(p)
                    && !types_tuple::varatt::varatt_is_external_expanded(p)
            };
            if external {
                let d = self.assign_copy_to_datum_ctx(value, false, -1, false)?;
                self.set_var(dno, d, false, true);
                return Ok(());
            }
        }
        self.set_var(dno, value, isnull, false);
        Ok(())
    }

    // exec_move_row(rec, NULL, NULL) (pl_exec.c:6922-6945): a composite-domain
    // rec becomes an empty expanded record and domain-checks the NULL.
    pub(crate) fn assign_rec_null(&mut self, recno: Dno) -> PgResult<()> {
        let is_domain = matches!(&self.func.datums[recno as usize],
            PlDatum::Rec(r) if r.datatype.as_ref().is_some_and(|d| d.typtype == TYPTYPE_DOMAIN));
        if is_domain {
            self.revalidate_rectypeid(recno)?;
            adt_domains::domain_check(Datum::null(), true, self.rec_typeid(recno))?;
            self.instantiate_empty_rec(recno)?;
        } else {
            self.assign_record_var(recno, None);
        }
        Ok(())
    }

    pub fn get_var(&mut self, dno: Dno) -> PgResult<(Datum, bool)> {
        // exec_eval_datum (pl_exec.c:5301) / plpgsql_param_fetch: a promise
        // is fulfilled on the variable's first read.
        if !self.promise_pending.is_empty() {
            self.fulfill_promise(dno)?;
        }
        match &mut self.datums[dno as usize] {
            DatumVal::Var { exp: Some(e), .. } => Ok((e.flat_datum()?, false)),
            DatumVal::Var { value, isnull, .. } => Ok((*value, *isnull)),
            _ => panic!("plpgsql: read of non-Var datum {dno}"),
        }
    }

    // plpgsql_param_eval_var for the expression's read/write Param
    // (pl_exec.c:6573-6580): the array variable's R/W expanded pointer,
    // expanding a flat value on first use (C expands on every assignment,
    // exec_assign_value:5108).
    fn get_var_rw(&mut self, dno: Dno) -> PgResult<(Datum, bool)> {
        if !self.promise_pending.is_empty() {
            self.fulfill_promise(dno)?;
        }
        let mcx = self.datum_ctx.mcx();
        let DatumVal::Var { value, isnull, exp, .. } = &mut self.datums[dno as usize] else {
            panic!("plpgsql: read of non-Var datum {dno}");
        };
        if *isnull {
            return Ok((Datum::null(), true));
        }
        if exp.is_none() {
            // The flat value stays allocated until the next set_var: an
            // argument evaluated earlier may still point into it.
            *exp = Some(arrayfuncs::expand_array_owned(*value, mcx.context(), None)?);
        }
        Ok((exp.as_ref().unwrap().rw_datum(), false))
    }

    fn transfer_var_out(&mut self, dno: Dno) -> PgResult<(Datum, bool)> {
        let (_, isnull) = self.get_var_rw(dno)?;
        if isnull {
            return Ok((Datum::null(), true));
        }
        let DatumVal::Var { isnull, exp, .. } = &mut self.datums[dno as usize] else {
            unreachable!("get_var_rw read a Var");
        };
        let obj = exp.take().expect("get_var_rw expanded the value");
        *isnull = true;
        Ok((obj.transfer_to_context(self.eval_ctx.mcx().context()), false))
    }

    // Installs an adopted R/W array as the variable's value (C
    // assign_simple_var of a transferred expanded object).
    fn set_var_expanded(&mut self, dno: Dno, obj: arrayfuncs::OwnedExpandedArray) {
        self.set_var(dno, Datum::null(), false, false);
        if let DatumVal::Var { exp, .. } = &mut self.datums[dno as usize] {
            *exp = Some(obj);
        }
    }

    // The assignment target's adoptable top-level call: a non-set function
    // returning exactly the target array variable's type.
    fn adoptable_call(&self, expr: &PlExpr, sexpr: types_nodes::Node<'static>) -> usize {
        let target = expr.target_param;
        let Some(PlDatum::Var(v)) = (target >= 0).then(|| &self.func.datums[target as usize]) else {
            return 0;
        };
        match sexpr.as_variant::<types_nodes::primnodes::FuncExpr>() {
            Some(f) if v.datatype.typisarray && !f.funcretset && f.funcresulttype == self.var_type(target).typoid => {
                f as *const types_nodes::primnodes::FuncExpr as usize
            }
            _ => 0,
        }
    }

    // Whether `d` is the Var's own read/write expanded pointer, i.e. the
    // expression changed the variable in place (C: newvalue == var->value).
    fn var_holds_rw(&self, dno: Dno, d: Datum) -> bool {
        matches!(&self.datums[dno as usize], DatumVal::Var { exp: Some(e), .. } if e.is_rw_datum(d))
    }

    // plpgsql_estate_setup's trigger arm: every promise variable starts
    // unfulfilled (C copies the compiled datums, promise flags included).
    pub(crate) fn arm_trigger_promises(&mut self, src: TriggerPromiseSource) {
        self.promise_pending = self
            .func
            .datums
            .iter()
            .filter_map(|d| match d {
                PlDatum::Var(v) if v.promise != PROMISE_NONE => Some(v.dno),
                _ => None,
            })
            .collect();
        self.trigger_promises = Some(src);
    }

    // plpgsql_fulfill_promise (pl_exec.c:1384-1512): compute the TG_* value
    // now and clear the promise (var->promise = PLpgSQL_PROMISE_NONE), so it
    // is computed at most once per call.
    fn fulfill_promise(&mut self, dno: Dno) -> PgResult<()> {
        let Some(pos) = self.promise_pending.iter().position(|&d| d == dno) else {
            return Ok(());
        };
        let promise = match &self.func.datums[dno as usize] {
            PlDatum::Var(v) => v.promise,
            _ => return Ok(()),
        };
        let (value, isnull) = {
            let src = self
                .trigger_promises
                .as_ref()
                .expect("plpgsql: promise pending without trigger data");
            crate::handler::compute_trigger_promise(self.datum_mcx(), src, promise)?
        };
        self.promise_pending.swap_remove(pos);
        // Promise images are built in the datum context by their own
        // constructors (not the datumCopy layout the per-value free expects)
        // and are released with it at exit.
        self.set_var(dno, value, isnull, false);
        Ok(())
    }

    fn exec_set_found(&mut self, state: bool) {
        let dno = self.func.found_varno;
        self.set_var(dno, Datum::from_bool(state), false, false);
    }

    // assign_record_var (pl_exec.c:8854-8869): install the new record value
    // and free the old one — its owned field images here; the tupdesc copy
    // goes with the last Rc holder (C: DeleteExpandedObject of the previous
    // expanded record, whose private context held both).
    fn assign_record_var(&mut self, recno: Dno, new: Option<RecValue>) {
        let old = core::mem::replace(&mut self.datums[recno as usize], DatumVal::Rec(new));
        if let DatumVal::Rec(Some(mut rv)) = old {
            self.free_rec_fields(&mut rv);
        }
    }

    fn free_rec_fields(&self, rv: &mut RecValue) {
        let mcx = self.datum_ctx.mcx();
        for f in 0..rv.values.len() {
            if rv.owned[f] {
                // SAFETY: owned marks a copy_to_datum_ctx copy in datum_ctx
                // held only by this record value.
                unsafe { free_datum_copy(mcx, rv.values[f], rv.desc.typlens[f]) };
                rv.owned[f] = false;
            }
        }
    }

    // exec_eval_cleanup.
    fn exec_eval_cleanup(&mut self) {
        if let Some(t) = self.eval_tuptable.take() {
            let _ = spi::SPI_freetuptable(t);
        }
        self.reset_eval_ctx();
    }

    fn reset_eval_ctx(&mut self) {
        self.rec_origins.clear();
        self.eval_ctx.reset();
    }

    pub(crate) fn datum_mcx(&self) -> Mcx<'static> {
        self.datum_ctx.mcx()
    }

    // datumCopy into the invocation context (by-ref survives statements).
    pub(crate) fn copy_to_datum_ctx(&self, value: Datum, isnull: bool, typlen: i16, typbyval: bool) -> PgResult<Datum> {
        if isnull || typbyval {
            return Ok(value);
        }
        // SAFETY: value is a live by-ref datum of typlen discipline.
        unsafe { execexpr::agg_datum_copy(self.datum_ctx.mcx(), value, typlen) }
    }

    // C assign_simple_var / expanded_record_set_* expand_external
    // (pl_exec.c 18.3:8786-8808): when !estate->atomic, stored values must
    // not keep on-disk toast pointers across a commit; expanded datums stay
    // and inline-compressed values stay compressed (detoast_external_attr).
    pub(crate) fn assign_copy_to_datum_ctx(
        &self,
        value: Datum,
        isnull: bool,
        typlen: i16,
        typbyval: bool,
    ) -> PgResult<Datum> {
        if !self.atomic && !isnull && typlen == -1 {
            let p = value.as_usize() as *const u8;
            // SAFETY: non-null by-ref varlena datum.
            unsafe {
                if types_tuple::varatt::varatt_is_1b_e(p)
                    && !types_tuple::varatt::varatt_is_external_expanded(p)
                {
                    // VarlenaRef is the 4B-only lane; external pointers size
                    // via varsize_any.
                    let attr =
                        core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p));
                    // pl_exec.c:8803-8811: detoast in the eval_mcontext
                    // (released at the next exec_eval_cleanup), then
                    // datumCopy into the function's context so the stored
                    // image has the layout free_datum_copy expects.
                    let out = detoast::detoast_external_attr(self.eval_ctx.mcx(), attr)?;
                    return self.copy_to_datum_ctx(
                        Datum::from_usize(out.as_ptr() as usize),
                        false,
                        -1,
                        false,
                    );
                }
            }
        }
        self.copy_to_datum_ctx(value, isnull, typlen, typbyval)
    }

    // exec_assign_value's expand_array arm (pl_exec.c:5108-5114): the array
    // lands in the datum context detoasted and decompressed, as the flat copy
    // expand_array makes (array_expanded.c:131); the R/W expanded form itself
    // is not held by this port's variables.
    fn assign_array_to_datum_ctx(&self, value: Datum) -> PgResult<Datum> {
        let p = value.as_usize() as *const u8;
        // SAFETY: non-null by-ref varlena datum.
        unsafe {
            if types_tuple::varatt::varatt_is_4b_u(p) {
                return self.copy_to_datum_ctx(value, false, -1, false);
            }
            let attr = core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p));
            let out = detoast::detoast_attr(self.eval_ctx.mcx(), attr)?;
            self.copy_to_datum_ctx(Datum::from_usize(out.as_ptr() as usize), false, -1, false)
        }
    }

    // ------------------------------------------------------------------
    // Expression evaluation
    // ------------------------------------------------------------------

    fn ensure_plan(&mut self, expr: &PlExpr, cursor_options: i32) -> PgResult<()> {
        let cached = EXPR_PLANS.with(|t| {
            t.borrow()
                .get(&expr.expr_id)
                .map(|e| spi::SPI_plan_single_source(e.plan).map(|(psrc, _)| psrc))
        });
        // Catalog-probing revalidation runs outside the EXPR_PLANS borrow.
        let stale = match cached {
            None => None,
            Some(None) => Some(false),
            Some(Some(psrc)) => Some(plancache::CachedPlanSourceRequiresReanalysis(psrc)?),
        };
        let mut relex = None;
        match stale {
            Some(false) => return Ok(()),
            // C's RevalidateCachedQuery re-analyzes in place; this port
            // re-prepares whenever the probe says the source needs re-analysis
            // (invalidation, search_path mismatch, RLS environment change) so
            // the hook tables refresh against current datum/tupdesc types. An
            // invalidation landing after the probe (lock-time sinval) is
            // covered by the installed reanalyze_plpgsql_expr hook instead.
            Some(true) => {
                // Re-prepare: the whole entry goes, including any compiled
                // simple expression riding it — its pin is on the source
                // being dropped (handles are generation-checked; the entry
                // invariant is "a Ready state implies its entry is live").
                // An in-flight evaluation of this expression (InUse) keeps
                // its own pin and put_simple() orphan-drops it on return.
                if let Some(e) =
                    EXPR_PLANS.with(|t| t.borrow_mut().remove(&expr.expr_id))
                {
                    let PlanEntry { plan, simple, lex, .. } = e;
                    relex = Some(lex);
                    let carry = match simple {
                        SimpleState::Unknown | SimpleState::Ready(_) => SimpleCarry::Redetermine,
                        SimpleState::NotSimple => SimpleCarry::NotSimple,
                        SimpleState::InUse => SimpleCarry::InUse,
                        SimpleState::Quarantined(lxid) => SimpleCarry::Quarantined(lxid),
                    };
                    SIMPLE_CARRY.with(|t| t.borrow_mut().insert(expr.expr_id, carry));
                    drop(simple);
                    let _ = spi::SPI_freeplan(plan);
                }
            }
            None => {}
        }
        let r = self.prepare_entry(expr, cursor_options, relex);
        if r.is_err() {
            SIMPLE_CARRY.with(|t| {
                if let Some(c @ SimpleCarry::Redetermine) = t.borrow_mut().get_mut(&expr.expr_id) {
                    *c = SimpleCarry::NotSimple;
                }
            });
        }
        r
    }

    fn prepare_entry(
        &mut self,
        expr: &PlExpr,
        cursor_options: i32,
        relex: Option<LexSettings>,
    ) -> PgResult<()> {
        let lex = relex.unwrap_or_else(LexSettings::current);
        let (hooks_names, params_by_dno, recs, valueless, recfields) = self
            .build_hook_tables(expr)
            .map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))?;
        let used = core::cell::RefCell::new(Vec::new());
        let name_entries: Vec<parser_small1::PlpgsqlNameEntry> = hooks_names
            .iter()
            .map(|(key, dno, t, m, c)| parser_small1::PlpgsqlNameEntry {
                key,
                dno: *dno,
                typoid: *t,
                typmod: *m,
                collation: *c,
            })
            .collect();
        let rec_names: Vec<(&str, &str)> = recs.iter()
            .map(|(key, name)| (key.as_str(), name.as_str())).collect();
        let valueless_names: Vec<(usize, &str)> =
            valueless.iter().map(|(r, s)| (*r, s.as_str())).collect();
        let recfield_keys: Vec<&str> = recfields.iter().map(String::as_str).collect();
        let resolve_option = self.resolve_option();
        let hooks = parser_small1::PlpgsqlHookState {
            names: &name_entries,
            params_by_dno: &params_by_dno,
            arg_dnos: &self.func.fn_argvarnos,
            recs: &rec_names,
            valueless_recs: &valueless_names,
            recfields: &recfield_keys,
            resolve_option,
            used: &used,
        };
        // Warnings emitted during parse/analysis (scanner escape warnings)
        // internalize their cursor onto the expression text, as under C's
        // _SPI_error_callback during SPI_prepare_extended.
        spi_frame!(prep_frame, &expr.query, expr.parse_mode);
        let outer = relex.map(|l| LexSettings { escape_string_warning: false, ..l }.install());
        let plan = spi::SPI_prepare_plpgsql(&expr.query, expr.parse_mode, &hooks, cursor_options);
        if let Some(prev) = outer {
            prev.install();
        }
        let plan = plan.map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))?;
        drop(prep_frame);
        if spi::SPI_keepplan(plan) != 0 {
            panic!("plpgsql exec_prepare_plan: SPI_keepplan failed");
        }
        if let Some((psrc, _)) = spi::SPI_plan_single_source(plan) {
            plancache::SetCachedPlanReanalyze(psrc, reanalyze_plpgsql_expr, expr.expr_id as i32);
        }
        let mut paramnos = used.into_inner();
        if is_assign_mode(expr.parse_mode) {
            if let Some((psrc, _)) = spi::SPI_plan_single_source(plan) {
                drop_unread_assign_target(&mut paramnos, plancache::SourceQueryList(psrc))?;
            }
        }
        paramnos.sort_unstable();
        let paramnos: std::rc::Rc<[Dno]> = paramnos.into();
        let argtypes = argtypes_of(&params_by_dno);
        // C exec_stmt_execsql's one-time mod_stmt derivation (mod_stmt_set).
        let mod_stmt = spi::SPI_plan_command_tags(plan).iter().any(|&tag| {
            tag == types_portal::CMDTAG_INSERT
                || tag == types_portal::CMDTAG_UPDATE
                || tag == types_portal::CMDTAG_DELETE
                || tag == types_portal::CMDTAG_MERGE
        });
        let hooks = std::rc::Rc::new(HookSnapshot {
            names: hooks_names,
            params_by_dno,
            arg_dnos: self.func.fn_argvarnos.clone(),
            recs,
            valueless,
            recfields,
            resolve_option,
        });
        let simple = match SIMPLE_CARRY.with(|t| t.borrow_mut().remove(&expr.expr_id)) {
            None | Some(SimpleCarry::Redetermine) => SimpleState::Unknown,
            Some(SimpleCarry::NotSimple) => SimpleState::NotSimple,
            Some(SimpleCarry::InUse) => SimpleState::InUse,
            Some(SimpleCarry::Quarantined(lxid)) => SimpleState::Quarantined(lxid),
        };
        let old = EXPR_PLANS.with(|t| {
            t.borrow_mut().insert(
                expr.expr_id,
                PlanEntry { plan, lex, paramnos, argtypes, mod_stmt, hooks, simple },
            )
        });
        debug_assert!(old.is_none(), "ensure_plan: stale path removed the old entry");
        drop(old);
        self.exec_simple_check_plan(expr)
    }

    // exec_prepare_plan's exec_simple_check_plan call (pl_exec.c:4196): a
    // simple-form query gets its generic plan right after the source is
    // created, so plan-time reports surface here, not at the first
    // evaluation, and a failed planning leaves the expression not simple
    // (pl_exec.c:8163-8165).
    fn exec_simple_check_plan(&mut self, expr: &PlExpr) -> PgResult<()> {
        let SimpleTake::Build { plan } = take_simple(expr.expr_id, true) else {
            return Ok(());
        };
        match self.build_simple_expr(expr, plan, false) {
            Ok(SimpleBuild::Ready(se)) => put_simple(expr.expr_id, plan, SimpleState::Ready(se)),
            Ok(SimpleBuild::Planned) => put_simple(expr.expr_id, plan, SimpleState::Unknown),
            Ok(SimpleBuild::NotSimple) => put_simple(expr.expr_id, plan, SimpleState::NotSimple),
            Ok(SimpleBuild::PlanFailed(e)) | Err(e) => {
                put_simple(expr.expr_id, plan, SimpleState::NotSimple);
                return Err(e);
            }
        }
        Ok(())
    }

    fn resolve_option(&self) -> parser_small1::PlpgsqlResolveOption {
        match self.func.resolve_option {
            crate::comp::PLPGSQL_RESOLVE_VARIABLE => parser_small1::PlpgsqlResolveOption::Variable,
            crate::comp::PLPGSQL_RESOLVE_COLUMN => parser_small1::PlpgsqlResolveOption::Column,
            _ => parser_small1::PlpgsqlResolveOption::Error,
        }
    }

    fn hook_snapshot(&mut self, expr: &PlExpr) -> PgResult<HookSnapshot> {
        let (names, params_by_dno, recs, valueless, recfields) = self.build_hook_tables(expr)?;
        Ok(HookSnapshot {
            names,
            params_by_dno,
            arg_dnos: self.func.fn_argvarnos.clone(),
            recs,
            valueless,
            recfields,
            resolve_option: self.resolve_option(),
        })
    }

    // plpgsql_parser_setup's resolution tables, flattened from the expr's
    // namespace chain (most-local binding wins; label-qualified aliases per
    // level; rec fields resolve against the rec's CURRENT tupdesc).
    #[allow(clippy::type_complexity)]
    fn build_hook_tables(
        &mut self,
        expr: &PlExpr,
    ) -> PgResult<(
        Vec<(String, Dno, Oid, i32, Oid)>,
        Vec<Option<(Oid, i32, Oid)>>,
        Vec<(String, String)>,
        Vec<(usize, String)>,
        Vec<String>,
    )> {
        let func = self.func;
        let mut names: Vec<(String, Dno, Oid, i32, Oid)> = Vec::new();
        let mut recs: Vec<(String, String)> = Vec::new();
        let mut valueless: Vec<(usize, String)> = Vec::new();
        let mut pending_valueless: Vec<String> = Vec::new();
        // First binding of a key wins; the set keeps the dedupe O(1) per name
        // (a linear scan made every prepare quadratic in the names in scope).
        let mut seen: FxHashSet<String> = FxHashSet::default();
        fn push_name(
            names: &mut Vec<(String, Dno, Oid, i32, Oid)>,
            seen: &mut FxHashSet<String>,
            info: (String, Dno, Oid, i32, Oid),
        ) {
            if seen.insert(info.0.clone()) {
                names.push(info);
            }
        }
        let mut recfields: Vec<String> = Vec::new();
        let mut pending_recfields: Vec<String> = Vec::new();

        // C instantiates a NULL named-composite record only when
        // resolve_column_ref finds one of its fields in the query
        // (pl_comp.c:1263 -> pl_exec.c:5575); a query whose text never
        // spells `rec.field` cannot, so its rowtype is left unresolved
        // (an ALTER of the underlying table inside the function must not
        // find the record already instantiated over the old descriptor).
        let pairs = if func.datums.iter().any(|d| matches!(d, PlDatum::RecField(_))) {
            crate::scanner::PlScanner::dotted_word_pairs(&expr.query)?
        } else {
            Vec::new()
        };
        let mentioned = |refname: &str, field: &str| -> bool {
            pairs.iter().any(|(r, f)| r == refname && f == field)
        };
        let mut params_by_dno: Vec<Option<(Oid, i32, Oid)>> = Vec::new();
        for d in &func.datums {
            params_by_dno.push(match d {
                PlDatum::Var(v) => {
                    let t = self.var_type(v.dno);
                    Some((t.typoid, t.atttypmod, t.collation))
                }
                PlDatum::RecField(f) => {
                    if mentioned(&self.rec_meta(f.recparentno).refname, &f.fieldname) {
                        self.recfield_type(f)?
                    } else {
                        self.recfield_type_no_instantiate(f)
                    }
                }
                PlDatum::Rec(r) => {
                    let (t, m) = self.rec_param_type_mod(r.dno)?;
                    Some((t, m, types_core::InvalidOid))
                }
                _ => None,
            });
        }

        let mut cur = expr.ns;
        let mut pending: Vec<(String, Dno, Oid, i32, Oid)> = Vec::new();
        let mut pending_recs: Vec<(String, String)> = Vec::new();
        while cur >= 0 {
            let item = &func.ns[cur as usize];
            match item.itemtype {
                NsType::Var => {
                    if let PlDatum::Var(v) = &func.datums[item.itemno as usize] {
                        // Names as the scanners left them (downcased unless
                        // quoted): resolve_column_ref compares with strcmp.
                        let key = item.name.clone();
                        let t = self.var_type(v.dno);
                        let info = (key.clone(), v.dno, t.typoid, t.atttypmod, t.collation);
                        push_name(&mut names, &mut seen, info.clone());
                        pending.push(info);
                    }
                }
                NsType::Rec => {
                    let recname = item.name.clone();
                    let recno = item.itemno;
                    let (rec_t, rec_m) = self.rec_param_type_mod(recno)?;
                    let marker = (recname.clone(), recno, rec_t, rec_m, types_core::InvalidOid);
                    push_name(&mut names, &mut seen, marker.clone());
                    pending.push(marker);
                    let whole_row = (
                        format!("{recname}\0{}", parser_small1::WHOLE_ROW_FIELD),
                        recno,
                        rec_t,
                        rec_m,
                        types_core::InvalidOid,
                    );
                    push_name(&mut names, &mut seen, whole_row.clone());
                    pending.push(whole_row);
                    // C resolve_column_ref resolves a qualified name via
                    // ns_lookup (nearest rec/row binding wins; Var entries
                    // are skipped for qualified lookups) and only THEN
                    // errors if that rec is unassigned. So the unqualified
                    // `recname` / `recname.field` keys and the valueless
                    // marking belong ONLY to the nearest rec binding for the
                    // name — an outer unassigned rec shadowed by a nearer
                    // same-named rec (e.g. a cursor FOR loop's private
                    // record over an explicitly DECLAREd one,
                    // P2-CURSORFOR-SHADOW) must not poison the prefix, and
                    // fields the nearer rec lacks must not leak through from
                    // the shadowed one. Label-qualified (pending) entries
                    // are unaffected: they resolve by qualified name.
                    let is_visible_rec_binding = !recs.iter().any(|(key, _)| *key == recname);
                    for d in &func.datums {
                        if let PlDatum::RecField(f) = d {
                            if f.recparentno == recno {
                                let fkey = format!("{recname}\0{}", f.fieldname);
                                if is_visible_rec_binding && !recfields.contains(&fkey) {
                                    recfields.push(fkey.clone());
                                }
                                pending_recfields.push(fkey);
                                let ft = if mentioned(&recname, &f.fieldname) {
                                    self.recfield_type(f)?
                                } else {
                                    self.recfield_type_no_instantiate(f)
                                };
                                if let Some((t, m, c)) = ft {
                                    let key = format!("{recname}\0{}", f.fieldname);
                                    let info = (key.clone(), f.dno, t, m, c);
                                    if is_visible_rec_binding {
                                        push_name(&mut names, &mut seen, info.clone());
                                    }
                                    pending.push(info);
                                }
                            }
                        }
                    }
                    if is_visible_rec_binding {
                        recs.push((recname.clone(), self.rec_meta(recno).refname.clone()));
                    }
                    if matches!(&self.datums[recno as usize], DatumVal::Rec(None))
                        && self.rec_typeid(recno) == RECORDOID
                    {
                        if is_visible_rec_binding {
                            valueless.push((names.len(), recname.clone()));
                        }
                        pending_valueless.push(recname.clone());
                    }
                    pending_recs.push((recname, self.rec_meta(recno).refname.clone()));
                }
                NsType::Row => {}
                NsType::Label => {
                    if !item.name.is_empty() && !recs.iter().any(|(key, _)| *key == item.name) {
                        let label = item.name.clone();
                        for (k, dno, t, m, c) in pending.drain(..) {
                            let lk = format!("{label}\0{k}");
                            push_name(&mut names, &mut seen, (lk, dno, t, m, c));
                        }
                        for (r, refname) in pending_recs.drain(..) {
                            let lr = format!("{label}\0{r}");
                            if !recs.iter().any(|(key, _)| *key == lr) {
                                recs.push((lr, refname));
                            }
                        }
                        for k in pending_recfields.drain(..) {
                            let lk = format!("{label}\0{k}");
                            if !recfields.contains(&lk) {
                                recfields.push(lk);
                            }
                        }
                        for r in pending_valueless.drain(..) {
                            let lr = format!("{label}\0{r}");
                            if !valueless.iter().any(|(_, v)| *v == lr) {
                                valueless.push((names.len(), lr));
                            }
                        }
                    } else {
                        pending.clear();
                        pending_recs.clear();
                        pending_valueless.clear();
                        pending_recfields.clear();
                    }
                }
            }
            cur = item.prev;
        }
        Ok((names, params_by_dno, recs, valueless, recfields))
    }

    // exec_get_datum_type_info REC arm: the declared rectypeid, typmod -1.
    #[allow(dead_code)] // ported helper; see rec_param_type_mod (PR48 composite-coercion lane)
    fn rec_param_type(&self, recno: Dno) -> Oid {
        self.rec_typeid(recno)
    }

    // C plpgsql_exec_get_datum_type_info REC arm (pl_exec.c:5541-5562): a
    // RECORD-declared rec with an expanded record reports the value's own
    // rowtype (erh->er_typeid) — for a trigger's NEW/OLD, bound from the
    // relation's tupdesc, that is the relation's named composite type. The
    // typmod is -1 in every arm ("do NOT return the mutable typmod of a
    // RECORD variable"). A rec without a value (C erh == NULL) reports the
    // declared type. The anonymous-record arm keeps reporting the blessed
    // typmod, which the header stamped by rec_as_composite_datum matches;
    // assign_record_type_typmod dedups by shape so it is stable.
    fn rec_param_type_mod(&mut self, recno: Dno) -> PgResult<(Oid, i32)> {
        let rectypeid = self.rec_typeid(recno);
        if rectypeid != RECORDOID {
            return Ok((rectypeid, -1));
        }
        let DatumVal::Rec(Some(rv)) = &self.datums[recno as usize] else {
            return Ok((RECORDOID, -1));
        };
        let src = rv.src_desc.clone().expect("RecValue carries its source tupdesc");
        if src.tdtypeid != RECORDOID {
            // C: erh->er_typeid (named composite), typmod -1. C reports this
            // for an EMPTY expanded record too — the trigger path builds
            // erh for both NEW and OLD before either holds a tuple.
            return Ok((src.tdtypeid, -1));
        }
        if rv.empty {
            return Ok((RECORDOID, -1));
        }
        if src.tdtypmod >= 0 {
            return Ok((RECORDOID, src.tdtypmod));
        }
        // C blesses an expanded record's tupdesc once (make_expanded_record_
        // from_tupdesc); keeping the blessed copy spares every later fetch
        // a descriptor copy and a registry probe.
        let mut td = tupdesc::CreateTupleDescCopy(self.datum_ctx.mcx(), &src)?;
        td.tdtypeid = RECORDOID;
        typcache::assign_record_type_typmod(&mut td)?;
        let typmod = td.tdtypmod;
        if let DatumVal::Rec(Some(rv)) = &mut self.datums[recno as usize] {
            rv.src_desc = Some(std::rc::Rc::new(td));
        }
        Ok((RECORDOID, typmod))
    }

    // exec_eval_datum REC arm: materialize the record as a composite Datum in
    // the eval scratch (valueless/empty record is a plain NULL).
    fn rec_as_composite_datum(&mut self, recno: Dno) -> PgResult<(Datum, bool)> {
        let rectypeid = self.rec_typeid(recno);
        let DatumVal::Rec(Some(rv)) = &self.datums[recno as usize] else {
            return Ok((Datum::null(), true));
        };
        if rv.empty {
            return Ok((Datum::null(), true));
        }
        let src = rv
            .src_desc
            .clone()
            .expect("RecValue carries its source tupdesc");
        let mut values = rv.values.clone();
        let nulls = rv.nulls.clone();
        let origin = RecOrigin { recno, sys: rv.sys.clone(), fvalue_valid: rv.fvalue_valid };
        let mcx = self.eval_ctx.mcx();
        detoast_external_fields(mcx, &src, &mut values, &nulls)?;
        let mut td = tupdesc::CreateTupleDescCopy(mcx, &src)?;
        if rectypeid != RECORDOID {
            // C: the expanded record's tupdesc carries er_typeid = the BASE
            // rowtype for a composite-domain rec; the flattened composite
            // datum's header says the base type, while the declared (domain)
            // type is what exec_eval_datum reports.
            td.tdtypeid = Self::rec_base_typeid(rectypeid)?;
            td.tdtypmod = -1;
        } else if td.tdtypeid == RECORDOID {
            if td.tdtypmod < 0 {
                typcache::assign_record_type_typmod(&mut td)?;
            }
        }
        // else: the source tupdesc already carries the value's real rowtype
        // (C exec_eval_datum REC arm, pl_exec.c:5364-5375: a RECORD-declared
        // rec reports erh->er_typeid/er_typmod, the named composite for a
        // trigger's NEW/OLD); stamping RECORD over it made every
        // composite-typed call site reject the value.
        let tup = heaptuple::heap_form_tuple(mcx, &td, &values, &nulls)?;
        let img = tup.header_ptr();
        core::mem::forget(tup);
        self.rec_origins.push((img as usize, origin));
        Ok((Datum::from_usize(img as usize), false))
    }

    fn rec_meta(&self, recno: Dno) -> &PlRec {
        match &self.func.datums[recno as usize] {
            PlDatum::Rec(r) => r,
            _ => panic!("plpgsql: datum {recno} is not a Rec"),
        }
    }

    fn rec_typeid(&self, recno: Dno) -> Oid {
        match self.rec_type_overrides.get(&recno) {
            Some(&t) => t,
            None => self.rec_meta(recno).rectypeid,
        }
    }

    // Re-resolves the rowtype from the name as written once its typcache
    // entry has been invalidated (kept by OID without a name).
    fn revalidate_rectypeid(&mut self, recno: Dno) -> PgResult<()> {
        let rec = self.rec_meta(recno);
        if rec.rectypeid == RECORDOID {
            return Ok(());
        }
        let compiled = rec.rectypeid;
        let ident = rec
            .datatype
            .as_ref()
            .and_then(|t| t.rec_ident.clone())
            .expect("named composite rec carries its typcache identity");
        let cached_id = ident.tcache.borrow().upgrade().map_or(0, |e| e.tupdesc_identifier());
        if cached_id != ident.tupdesc_id.get() {
            if let Some(name) = &ident.origtypname {
                ident.typoid.set(resolve_origtypname(name)?.0);
            }
            let entry = crate::comp::composite_typcache_entry(ident.typoid.get())?;
            ident.tupdesc_id.set(entry.tupdesc_identifier());
            *ident.tcache.borrow_mut() = std::rc::Rc::downgrade(&entry);
        }
        let current = ident.typoid.get();
        if current == compiled {
            self.rec_type_overrides.remove(&recno);
        } else {
            self.rec_type_overrides.insert(recno, current);
        }
        Ok(())
    }

    // A rec declared with a domain-over-composite type (PLPGSQL_TTYPE_REC
    // via pl_comp.c's TYPTYPE_DOMAIN arm) keeps the domain oid in rectypeid;
    // its tupdesc is the base type's (C expandedrecord DOMAIN_BASE_INFO).
    fn rec_typeid_is_domain(rectypeid: Oid) -> PgResult<bool> {
        if rectypeid == RECORDOID {
            return Ok(false);
        }
        Ok(lsyscache::typ::get_typtype(rectypeid)? == TYPTYPE_DOMAIN)
    }

    // C getBaseType via lookup_type_cache DOMAIN_BASE_INFO: the rowtype the
    // rec's tuples physically carry (identity for a plain composite).
    fn rec_base_typeid(rectypeid: Oid) -> PgResult<Oid> {
        if Self::rec_typeid_is_domain(rectypeid)? {
            lsyscache::typ::getBaseType(rectypeid)
        } else {
            Ok(rectypeid)
        }
    }

    // C expandedrecord.c check_domain_on_current_fields /
    // check_domain_for_new_field: materialize the (prospective) field set as
    // a tuple over the base tupdesc and run domain_check against the
    // DECLARED domain type. The tuple lives in the eval scratch (freed by
    // exec_eval_cleanup), like rec_as_composite_datum's materialization.
    // External fields are fetched first (ER_get_flat_size,
    // expandedrecord.c:712-745): the constraint sees the inlined composite,
    // not a tuple of toast pointers.
    fn rec_domain_check_fields(
        &mut self,
        rectypeid: Oid,
        td: &types_tuple::TupleDescData<'static>,
        values: &[Datum],
        nulls: &[bool],
    ) -> PgResult<()> {
        let mcx = self.eval_ctx.mcx();
        let mut inlined: Vec<Datum> = ::mcx::std_vec_from_slice("plpgsql", values)?;
        for (i, v) in inlined.iter_mut().enumerate() {
            if nulls[i] || td.attr(i).attlen != -1 || td.attr(i).attisdropped {
                continue;
            }
            let p = v.as_usize() as *const u8;
            // SAFETY: non-null by-ref varlena datum.
            unsafe {
                if types_tuple::varatt::varatt_is_1b_e(p)
                    && !types_tuple::varatt::varatt_is_external_expanded(p)
                {
                    let attr = core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p));
                    let out = detoast::detoast_external_attr(mcx, attr)?;
                    *v = Datum::from_usize(out.as_ptr() as usize);
                    core::mem::forget(out);
                }
            }
        }
        let values = &inlined[..];
        let tup = heaptuple::heap_form_tuple(mcx, td, values, nulls)?;
        let img = tup.header_ptr();
        core::mem::forget(tup);
        adt_domains::domain_check(Datum::from_usize(img as usize), false, rectypeid)
    }

    // instantiate_empty_record_variable (pl_exec.c:7810).
    pub(crate) fn instantiate_empty_rec(&mut self, recno: Dno) -> PgResult<()> {
        let rec = self.rec_meta(recno);
        if rec.rectypeid == RECORDOID {
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                    .errmsg(format!("record \"{}\" is not assigned yet", rec.refname))
                    .errdetail(
                        "The tuple structure of a not-yet-assigned record is indeterminate.",
                    )
                    .into_error(),
            ));
        }
        self.revalidate_rectypeid(recno)?;
        let rectypeid = self.rec_typeid(recno);
        // C make_expanded_record_from_typeid: a composite-domain rec gets its
        // BASE type's tupdesc (typcache DOMAIN_BASE_INFO).
        let base = Self::rec_base_typeid(rectypeid)?;
        let td = typcache::lookup_rowtype_tupdesc_copy(self.datum_ctx.mcx(), base, -1)?;
        let desc = RecDesc::from_tupdesc(&td)?;
        let n = desc.types.len();
        self.assign_record_var(
            recno,
            Some(RecValue {
                desc,
                values: ::mcx::std_vec_from_elem("plpgsql", Datum::null(), n)?,
                nulls: ::mcx::std_vec_from_elem("plpgsql", true, n)?,
                src_desc: Some(std::rc::Rc::new(td)),
                empty: true,
                sys: None,
                fvalue_valid: false,
                owned: ::mcx::std_vec_from_elem("plpgsql", false, n)?,
            }),
        );
        Ok(())
    }

    // exec_get_datum_type-ish for RECFIELD: type from the rec's live value;
    // a valueless named-composite rec is instantiated first (C
    // exec_get_datum_type_info). Valueless RECORD recs stay None — the 55000
    // fires only if the SQL expression actually references a field
    // (resolve_column_ref's valueless_recs arm).
    fn recfield_type(&mut self, f: &PlRecField) -> PgResult<Option<(Oid, i32, Oid)>> {
        if matches!(&self.datums[f.recparentno as usize], DatumVal::Rec(None))
            && self.rec_typeid(f.recparentno) != RECORDOID
        {
            self.instantiate_empty_rec(f.recparentno)?;
        }
        Ok(self.recfield_type_no_instantiate(f))
    }

    fn recfield_type_no_instantiate(&self, f: &PlRecField) -> Option<(Oid, i32, Oid)> {
        if let DatumVal::Rec(Some(rv)) = &self.datums[f.recparentno as usize] {
            match rec_lookup_field(&rv.desc, &f.fieldname) {
                Some(RecFieldRef::User(i)) => {
                    return Some((rv.desc.types[i], rv.desc.typmods[i], rv.desc.colls[i]));
                }
                Some(RecFieldRef::Sys(a)) => {
                    return Some((a.atttypid, a.atttypmod, a.attcollation));
                }
                None => {}
            }
        }
        None
    }

    // setup_param_list (pl_exec.c:6250) + plpgsql_param_fetch's speculative
    // planning fetch (pl_exec.c:6298-6355): every paramno's current value and
    // type, except a record field the fetch could fail on, which is left
    // without a value (ptype InvalidOid) for the executor to fetch through
    // `fetch` only if evaluation reaches it. A record or field whose type is
    // no longer the planned one keeps its current type with pflags 0, so the
    // mismatch is raised only where the Param is evaluated and a served run
    // delegates the list. A query referencing no PL variable runs with an
    // empty (C NULL) list, so the plancache serves it the generic plan.
    fn setup_param_list(
        &mut self,
        paramnos: &[Dno],
        argtypes: &[Oid],
        speculative: bool,
    ) -> PgResult<Vec<ParamExternData>> {
        if paramnos.is_empty() {
            return Ok(Vec::new());
        }
        let mut params = ::mcx::std_vec_with_capacity("plpgsql", argtypes.len())?;
        params.extend(argtypes.iter().map(|&ptype| ParamExternData {
            value: Datum::null(),
            isnull: true,
            pflags: PARAM_FLAG_CONST,
            ptype,
        }));
        for &dno in paramnos {
            let planned = argtypes[dno as usize];
            let is_var = matches!(&self.func.datums[dno as usize], PlDatum::Var(_));
            params[dno as usize] = match self.param_fetch(dno, planned, speculative)? {
                Some(p) if !is_var && p.ptype != planned => ParamExternData { pflags: 0, ..p },
                Some(p) => p,
                None => ParamExternData {
                    value: Datum::null(),
                    isnull: true,
                    pflags: 0,
                    ptype: types_core::InvalidOid,
                },
            };
        }
        Ok(params)
    }

    // Params of a plan _SPI_execute_plan runs are evaluated under its
    // _SPI_error_callback (spi.c), so a fetch failure carries the SQL line.
    // Cursor opens fetch non-speculatively (copyParamList).
    fn setup_param_list_under_spi(
        &mut self,
        expr: &PlExpr,
        paramnos: &[Dno],
        argtypes: &[Oid],
        speculative: bool,
    ) -> PgResult<Vec<ParamExternData>> {
        self.setup_param_list(paramnos, argtypes, speculative)
            .map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))
    }

    fn spi_fetch_arg(&mut self, paramnos: &std::rc::Rc<[Dno]>) -> SpiFetchArg {
        SpiFetchArg { estate: (self as *mut Estate<'a>).cast(), paramnos: std::rc::Rc::clone(paramnos) }
    }

    // plpgsql_param_fetch / exec_eval_datum (pl_exec.c:6298, 5301): the
    // datum's value and CURRENT type (`planned` for a Var, whose type cannot
    // change). Speculative (planning's fetch) returns None rather than fail
    // on a field of a NULL record or one its record lacks.
    fn param_fetch(
        &mut self,
        dno: Dno,
        planned: Oid,
        speculative: bool,
    ) -> PgResult<Option<ParamExternData>> {
        let func = self.func;
        let (value, isnull, ptype) = match &func.datums[dno as usize] {
            PlDatum::Var(v) => {
                let (value, isnull) = self.get_var(dno)?;
                let ptype = if OidIsValid(planned) { planned } else { v.datatype.typoid };
                (value, isnull, ptype)
            }
            PlDatum::Rec(r) => {
                let (ptype, _) = self.rec_param_type_mod(r.dno)?;
                let (value, isnull) = self.rec_as_composite_datum(dno)?;
                (value, isnull, ptype)
            }
            PlDatum::RecField(f) => {
                if speculative {
                    match &self.datums[f.recparentno as usize] {
                        DatumVal::Rec(Some(rv))
                            if rec_lookup_field_cached(
                                &mut self.recfield_idx,
                                f.dno,
                                &rv.desc,
                                &f.fieldname,
                            )
                            .is_some() => {}
                        _ => return Ok(None),
                    }
                }
                if matches!(&self.datums[f.recparentno as usize], DatumVal::Rec(None))
                    && self.rec_typeid(f.recparentno) != RECORDOID
                {
                    self.instantiate_empty_rec(f.recparentno)?;
                }
                let recname = || match &func.datums[f.recparentno as usize] {
                    PlDatum::Rec(r) => r.refname.clone(),
                    _ => String::new(),
                };
                let DatumVal::Rec(Some(rv)) = &self.datums[f.recparentno as usize] else {
                    return Err(Box::new(
                        elog::ereport(ERROR)
                            .errcode(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                            .errmsg(format!("record \"{}\" is not assigned yet", recname()))
                            .errdetail("The tuple structure of a not-yet-assigned record is indeterminate.")
                            .into_error(),
                    ));
                };
                match rec_lookup_field_cached(&mut self.recfield_idx, f.dno, &rv.desc, &f.fieldname) {
                    Some(RecFieldRef::User(i)) => (rv.values[i], rv.nulls[i], rv.desc.types[i]),
                    Some(RecFieldRef::Sys(a)) => match &rv.sys {
                        Some(s) => (s.get(a.attnum), false, a.atttypid),
                        None => (Datum::null(), true, a.atttypid),
                    },
                    None => {
                        return Err(exec_err(
                            types_error::ERRCODE_UNDEFINED_COLUMN,
                            format!("record \"{}\" has no field \"{}\"", recname(), f.fieldname),
                        ))
                    }
                }
            }
            _ => panic!("plpgsql: datum {dno} cannot be a parameter"),
        };
        Ok(Some(ParamExternData { value, isnull, pflags: PARAM_FLAG_CONST, ptype }))
    }

    // `planned` = the Param's type as of plan preparation. C's compiled
    // param steps re-check the datum's CURRENT type on every evaluation for
    // record and record-field datums — they can change type under a cached
    // plan (ALTER TABLE beneath %ROWTYPE) — and error rather than misread
    // the datum (plpgsql_param_eval_recfield / _generic "safety check",
    // pl_exec.c:6797/6838). Plain Vars take C's unchecked fast path
    // (plpgsql_param_eval_var). None = caller is not a plan-param seam.
    fn datum_as_param(&mut self, dno: Dno, planned: Option<Oid>) -> PgResult<(Datum, bool)> {
        let is_var = matches!(&self.func.datums[dno as usize], PlDatum::Var(_));
        if let (Some(planned), false) = (planned, is_var) {
            if let PlDatum::Rec(r) = &self.func.datums[dno as usize] {
                let (current, _) = self.rec_param_type_mod(r.dno)?;
                if current != planned {
                    return Err(param_type_mismatch(dno, current, planned));
                }
            }
        }
        let p = self
            .param_fetch(dno, planned.unwrap_or(types_core::InvalidOid), false)?
            .expect("a non-speculative fetch yields a value");
        if let (Some(planned), false) = (planned, is_var) {
            if p.ptype != planned {
                return Err(param_type_mismatch(dno, p.ptype, planned));
            }
        }
        Ok((p.value, p.isnull))
    }

    // exec_eval_expr: returns (value, isnull, rettype, rettypmod). Caller
    // must exec_eval_cleanup when done with a by-ref result.
    pub fn exec_eval_expr(&mut self, expr: &PlExpr) -> PgResult<(Datum, bool, Oid, i32)> {
        self.exec_eval_expr_opts(expr, CURSOR_OPT_PARALLEL_OK)
    }

    fn exec_eval_expr_opts(
        &mut self,
        expr: &PlExpr,
        cursor_options: i32,
    ) -> PgResult<(Datum, bool, Oid, i32)> {
        // Steady state first, before any plan-cache traffic: C's
        // exec_eval_expr (pl_exec.c:5673) prepares the plan once per
        // function lifetime and exec_eval_simple_expr revalidates with
        // CachedPlanIsSimplyValid alone — no GetCachedPlan, no
        // RevalidateCachedQuery, no reanalysis probe. The probe below runs
        // only when this misses (first use, invalidation, search_path
        // change, recursion, not-simple), which is also when its hook-table
        // refresh is actually needed.
        if let Some(r) = self.exec_eval_simple_fast(expr)? {
            return Ok(r);
        }

        self.ensure_plan(expr, cursor_options)?;

        if let Some(r) = self.exec_eval_simple_expr(expr)? {
            return Ok(r);
        }

        let rc = self.exec_run_select(expr, 0)?;
        if rc != spi::SPI_OK_SELECT {
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_WRONG_OBJECT_TYPE)
                    .errmsg("query did not return data")
                    .errcontext_msg(format!("query: {}", expr.query))
                    .into_error(),
            ));
        }
        let tuptab = self.eval_tuptable.expect("exec_run_select stored tuptable");
        let (natts, rettype, rettypmod) = spi::tuptable_with(tuptab, |t| {
            let n = t.tupdesc.attrs.len();
            if n >= 1 {
                (n, t.tupdesc.attrs[0].atttypid, t.tupdesc.attrs[0].atttypmod)
            } else {
                (n, types_core::InvalidOid, -1)
            }
        });
        if natts != 1 {
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_SYNTAX_ERROR)
                    .errmsg_plural(
                        format!("query returned {natts} column"),
                        format!("query returned {natts} columns"),
                        natts as u64,
                    )
                    .errcontext_msg(format!("query: {}", expr.query))
                    .into_error(),
            ));
        }
        if self.eval_processed == 0 {
            return Ok((Datum::null(), true, rettype, rettypmod));
        }
        if self.eval_processed != 1 {
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_CARDINALITY_VIOLATION)
                    .errmsg("query returned more than one row")
                    .errcontext_msg(format!("query: {}", expr.query))
                    .into_error(),
            ));
        }
        let (v, isnull) =
            spi::tuptable_with(tuptab, |t| spi::SPI_getbinval(&t.vals[0], &t.tupdesc, 1));
        Ok((v, isnull, rettype, rettypmod))
    }

    // Steady-state arm of exec_eval_simple_expr: evaluate through an
    // existing Ready state, touching no plan-cache machinery beyond the
    // per-eval CachedPlanIsSimplyValid gate (C pl_exec.c:6060). Ok(None) =
    // fall to the slow path (which starts with the ensure_plan probe).
    fn exec_eval_simple_fast(
        &mut self,
        expr: &PlExpr,
    ) -> PgResult<Option<(Datum, bool, Oid, i32)>> {
        let se = match take_simple(expr.expr_id, false) {
            SimpleTake::Ready(se) => se,
            SimpleTake::Skip | SimpleTake::Build { .. } => return Ok(None),
        };
        let se = simple_expr_portal_snapshot(expr.expr_id, se)?;
        // Within its transaction the compiled state keeps the EXECUTE ACL
        // verdicts it was built with, whatever the current user (C).
        if se.lxid != current_lxid() {
            let plan = se.plan;
            drop(se);
            put_simple(expr.expr_id, plan, SimpleState::Unknown);
            return Ok(None);
        }
        // Every exit below must restore the InUse slot (an error leaving it
        // InUse would silently demote this expression to SPI forever).
        match plancache::CachedPlanIsSimplyValid(se.psrc, se.cplan) {
            Ok(true) => self.eval_simple_taken(expr, se).map(Some),
            // C's replan arm resets to "not simple" and rebuilds in place
            // (pl_exec.c:6072-6142); here the slow path redetermines after
            // the ensure_plan probe has had its chance to re-prepare with
            // fresh hook tables.
            Ok(false) => {
                let plan = se.plan;
                drop(se);
                put_simple(expr.expr_id, plan, SimpleState::Unknown);
                Ok(None)
            }
            Err(e) => {
                let plan = se.plan;
                drop(se);
                put_simple(expr.expr_id, plan, SimpleState::Unknown);
                Err(e)
            }
        }
    }

    // exec_eval_simple_expr; Ok(None) = not simple, take the SPI path.
    // Caller ran ensure_plan.
    fn exec_eval_simple_expr(
        &mut self,
        expr: &PlExpr,
    ) -> PgResult<Option<(Datum, bool, Oid, i32)>> {
        // After the take, the slot reads InUse; every exit below goes
        // through put_simple.
        let build = match take_simple(expr.expr_id, true) {
            // NotSimple (C expr_simple_expr == NULL, pl_exec.c:6036) or
            // recursion/error quarantine (expr_simple_in_use, :6042).
            SimpleTake::Skip => return Ok(None),
            SimpleTake::Ready(se) => {
                let se = simple_expr_portal_snapshot(expr.expr_id, se)?;
                let valid = if se.lxid == current_lxid() {
                    plancache::CachedPlanIsSimplyValid(se.psrc, se.cplan)
                } else {
                    Ok(false)
                };
                match valid {
                    Ok(true) => return self.eval_simple_taken(expr, se).map(Some),
                    Ok(false) => {}
                    Err(e) => {
                        // Restore the slot (an error leaving InUse would
                        // demote this expression to SPI forever).
                        let plan = se.plan;
                        drop(se);
                        put_simple(expr.expr_id, plan, SimpleState::Unknown);
                        return Err(e);
                    }
                }
                // Invalidated since the fast path last saw it (or the fast
                // path just parked it): release and redetermine — C's
                // replan arm (pl_exec.c:6072-6142) folded into the shared
                // build below, which also re-runs exec_is_simple_query as
                // C does (:6119).
                drop(se);
                EXPR_PLANS.with(|t| {
                    let t = t.borrow();
                    t.get(&expr.expr_id).expect("plan ensured").plan
                })
            }
            SimpleTake::Build { plan } => {
                if let Err(e) = pquery::EnsurePortalSnapshotExists() {
                    put_simple(expr.expr_id, plan, SimpleState::Unknown);
                    return Err(e);
                }
                plan
            }
        };
        let plan = build;
        let se = match self.build_simple_expr(expr, plan, true) {
            Ok(SimpleBuild::Ready(se)) => se,
            Ok(SimpleBuild::PlanFailed(e)) => {
                put_simple(expr.expr_id, plan, SimpleState::NotSimple);
                return Err(e);
            }
            Ok(_) => {
                put_simple(expr.expr_id, plan, SimpleState::NotSimple);
                return Ok(None);
            }
            Err(e) => {
                put_simple(expr.expr_id, plan, SimpleState::Unknown);
                return Err(e);
            }
        };
        self.eval_simple_taken(expr, se).map(Some)
    }

    // exec_check_rw_parameter: the RWOPT_TRANSFER rule (a local target
    // referenced exactly once is passed R/W whatever consumes it), else the
    // RWOPT_INPLACE rule (the Param the top-level function's support
    // function names). The node address identifies it to the compiled program.
    fn rw_param_node(
        &self,
        expr: &PlExpr,
        sexpr: types_nodes::Node<'static>,
    ) -> PgResult<Option<(*const (), bool)>> {
        use types_nodes::primnodes::{FuncExpr, OpExpr, Param, RelabelType, SubscriptingRef};
        let target = expr.target_param;
        if target < 0
            || !matches!(&self.func.datums[target as usize], PlDatum::Var(v) if v.datatype.typisarray)
        {
            return Ok(None);
        }
        if expr.target_is_local {
            let mut refs = TargetParamRefs { paramid: target + 1, count: 0, last: None };
            nodes_core::NodeWalker::visit(&mut refs, sexpr)?;
            if refs.count == 1 {
                return Ok(refs.last.map(|p| (p, true)));
            }
        }
        let sexpr = match sexpr.as_variant::<RelabelType>() {
            Some(r) => r.arg,
            None => sexpr,
        };
        let mut args: [Option<types_nodes::Node<'static>>; 2] = [None, None];
        let funcid = if let Some(f) = sexpr.as_variant::<FuncExpr>() {
            for (slot, a) in args.iter_mut().zip(f.args.iter()) {
                *slot = Some(a);
            }
            if f.args.len() > 2 {
                return Ok(None);
            }
            f.funcid
        } else if let Some(o) = sexpr.as_variant::<OpExpr>() {
            for (slot, a) in args.iter_mut().zip(o.args.iter()) {
                *slot = Some(a);
            }
            if o.args.len() > 2 {
                return Ok(None);
            }
            o.opfuncid
        } else if let Some(r) = sexpr.as_variant::<SubscriptingRef>() {
            args = [r.refexpr, r.refassgnexpr];
            lsyscache::typ::get_typsubscript(r.refcontainertype)?.0
        } else {
            return Ok(None);
        };
        if !OidIsValid(funcid) {
            return Ok(None);
        }
        let prosupport = lsyscache::function::get_func_support(funcid)?;
        if !OidIsValid(prosupport) {
            return Ok(None);
        }
        let req = types_nodes::supportnodes::SupportRequestModifyInPlace::new(
            funcid,
            &args,
            target + 1,
        );
        let call_cx = ::mcx::MemoryContext::new_bump("plpgsql_rw_param_support");
        let answer = fmgr_core::oid_function_call1_coll_in(
            prosupport,
            0,
            call_cx.mcx(),
            Datum::from_usize(core::ptr::from_ref(&req) as usize),
        )?;
        let Some(raw) = core::ptr::NonNull::new(answer.as_usize() as *mut ()) else {
            return Ok(None);
        };
        // SAFETY: the support function returned one of the request's own
        // argument nodes (supportnodes.h contract).
        let node = unsafe { types_nodes::Node::<'static>::from_raw(raw) };
        Ok(node.as_variant::<Param>().map(|p| (p as *const Param as *const (), false)))
    }

    // The array-variable Params whose direct consumer reads an expanded
    // array itself (arrayfuncs.c AnyArrayType readers, array subscript
    // fetch): they get the variable's read-only expanded value, as every
    // reader does in C, instead of a flattened copy.
    fn expanded_reader_params(
        &self,
        sexpr: types_nodes::Node<'static>,
        rw: Option<*const ()>,
    ) -> PgResult<Vec<*const ()>> {
        let mut w = ExpandedReaderParams { func: self.func, rw, found: Vec::new() };
        nodes_core::NodeWalker::visit(&mut w, sexpr)?;
        Ok(w.found)
    }

    // exec_simple_check_plan (pl_exec.c:8133) + exec_save_simple_expr
    // (pl_exec.c:8271). The caller owns the InUse slot.
    fn build_simple_expr(
        &mut self,
        expr: &PlExpr,
        plan: SpiPlanPtr,
        compile: bool,
    ) -> PgResult<SimpleBuild> {
        let Some((psrc, _)) = spi::SPI_plan_single_source(plan) else {
            return Ok(SimpleBuild::NotSimple);
        };
        // exec_is_simple_query (pl_exec.c:8161): decided on the analyzed
        // querytree before any generic plan is requested — an embedded
        // SubPlan (hasSubLinks) survives the bare-Result test below, and C
        // routes every such query through SPI, planned with its parameters.
        {
            let queries = plancache::SourceQueryList(psrc);
            if queries.len() != 1 || !exec_is_simple_query(&queries[0]) {
                return Ok(SimpleBuild::NotSimple);
            }
        }
        // Planning reports (const-fold errors and warnings) carry the
        // parse-mode context line: C wraps GetCachedPlan in
        // _SPI_error_callback (spi.c:2097 SPI_plan_get_cached_plan).
        let emit_cb = {
            let (query, mode) = (expr.query.clone(), expr.parse_mode);
            elog::push_emit_context_callback(Box::new(move |e| {
                spi_error_context(e, &query, mode);
            }))
        };
        spi_frame!(frame, &expr.query, expr.parse_mode);
        let cplan = plancache::GetCachedPlan(
            psrc,
            types_portal::ParamListHandle::NULL,
            None,
            types_portal::QueryEnvHandle::NULL,
        );
        drop(frame);
        elog::pop_emit_context_callback(emit_cb);
        let cplan = match cplan {
            Ok(c) => c,
            Err(e) => return Ok(SimpleBuild::PlanFailed(spi_ctx_err(e, &expr.query, expr.parse_mode))),
        };
        let built = (|| -> PgResult<SimpleBuild> {
            let stmts = plancache::CachedPlanStmtList(cplan);
            if stmts.len() != 1 {
                return Ok(SimpleBuild::NotSimple);
            }
            let stmt = &stmts[0];
            if stmt.commandType != types_nodes::nodes_enums::CmdType::CMD_SELECT
                || stmt.utilityStmt.is_some()
                || !stmt.rowMarks.is_nil()
            {
                return Ok(SimpleBuild::NotSimple);
            }
            let Some(plan_expr) = simple_result_expr(stmt) else {
                return Ok(SimpleBuild::NotSimple);
            };
            if !compile {
                return Ok(SimpleBuild::Planned);
            }
            // C exec_eval_simple_expr compiles under estate->paramLI, whose
            // paramCompile hook (plpgsql_param_compile, pl_exec.c:6465-6488)
            // turns every datum Param into an EEOP_PARAM_CALLBACK step; the
            // Param node's paramtype is the plan-time type the per-eval
            // safety check compares against (pl_exec.c:6797/6838).
            // The variable keeps its object for in-place consumers; only a
            // call that can adopt the object takes it over (the target is
            // local, so no handler sees the variable in between).
            let adopt_call = self.adoptable_call(expr, plan_expr.0);
            let rw = self.rw_param_node(expr, plan_expr.0)?.map(|(p, transfer)| {
                let fetch: types_portal::params::ParamCallbackFn = if transfer && adopt_call != 0 {
                    simple_param_eval_transfer
                } else {
                    simple_param_eval_rw
                };
                (p, fetch)
            });
            let ro_params = self.expanded_reader_params(plan_expr.0, rw.map(|r| r.0))?;
            // Function-lifetime memory home for the compiled program (C
            // compiles into simple_eval_estate->es_query_cxt and rebuilds
            // per transaction, pl_exec.c:6172-6180; see SimpleExpr's
            // lifetime note for why no rebuild is needed here).
            let ctx = Ctx::new_start_small("PLpgSQL simple expression");
            let ro_params = ::mcx::slice_in(ctx.mcx(), &ro_params)?.leak();
            let bind = types_portal::params::ParamBind {
                param_callback: Some(simple_param_eval),
                param_expanded: Some(simple_param_expanded),
                param_rw: rw,
                param_ro_expanded: Some((
                    ro_params,
                    simple_param_eval_ro as types_portal::params::ParamCallbackFn,
                )),
                ..types_portal::params::ParamBind::NONE
            };
            let Some(state) = execexpr::exec_init_expr(ctx.mcx(), Some(plan_expr.0), bind)?
            else {
                return Ok(SimpleBuild::NotSimple);
            };
            // C exec_save_simple_expr (pl_exec.c:8349): immutable
            // expressions skip the per-eval CCI + snapshot ceremony.
            let mutable = clauses::contain_mutable_functions(plan_expr.0)?;
            let mut bare = plan_expr.0;
            while let Some(r) = bare.as_variant::<types_nodes::primnodes::RelabelType>() {
                if r.resulttype != RECORDOID {
                    break;
                }
                bare = r.arg;
            }
            let bare_rec = bare
                .as_variant::<types_nodes::primnodes::Param>()
                .filter(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN)
                .and_then(|p| {
                    let dno = p.paramid - 1;
                    matches!(self.func.datums.get(dno as usize), Some(PlDatum::Rec(_)))
                        .then_some((dno, p.paramtype))
                });
            Ok(SimpleBuild::Ready(Box::new(SimpleExpr {
                plan,
                state,
                cplan,
                psrc,
                rettype: plan_expr.1,
                lxid: current_lxid(),
                rettypmod: plan_expr.2,
                mutable,
                adopt_call,
                bare_rec,
                ctx,
            })))
        })();
        match built {
            Ok(SimpleBuild::Ready(se)) => {
                // First Ready state of this backend: arrange orderly pin
                // release at proc_exit (see release_plpgsql_state_at_exit;
                // normally already registered by the compile that produced
                // this expression).
                register_exit_release();
                Ok(SimpleBuild::Ready(se))
            }
            Ok(other) => {
                plancache::ReleaseCachedPlan(cplan);
                Ok(other)
            }
            Err(e) => {
                plancache::ReleaseCachedPlan(cplan);
                Err(e)
            }
        }
    }

    // The evaluation proper (pl_exec.c:6146-6241): write params, arm the
    // result context, snapshot ceremony if needed, run the program. The
    // taken state goes back Ready on success; an error drops it and
    // quarantines the expression for the rest of the transaction.
    fn eval_simple_taken(
        &mut self,
        expr: &PlExpr,
        mut se: Box<SimpleExpr>,
    ) -> PgResult<(Datum, bool, Oid, i32)> {
        let result = self.eval_simple_body(&mut se);
        let plan = se.plan;
        match result {
            Ok(r) => {
                put_simple(expr.expr_id, plan, SimpleState::Ready(se));
                Ok(r)
            }
            Err(e) => {
                drop(se);
                put_simple(expr.expr_id, plan, SimpleState::Quarantined(current_lxid()));
                Err(e)
            }
        }
    }

    fn eval_simple_body(&mut self, se: &mut SimpleExpr) -> PgResult<(Datum, bool, Oid, i32)> {
        if let (true, Some((dno, planned))) = (core::mem::take(&mut self.bare_rec_wanted), se.bare_rec) {
            let current = self.rec_value_type(dno);
            if current != planned {
                return Err(param_type_mismatch(dno, current, planned));
            }
            self.bare_rec_hit = Some((dno, planned));
            return Ok((Datum::null(), true, se.rettype, se.rettypmod));
        }
        // C pl_exec.c:6154-6164: the program evaluates under an econtext
        // whose ecxt_param_list_info->paramFetchArg is THIS estate; its
        // ParamCallback steps fetch datums through simple_param_eval only
        // when reached (an untaken CASE arm never touches its record).
        se.state.arm_param_callback_arg((self as *mut Estate<'a>).cast());
        // By-ref results land in THIS invocation's eval scratch: the state
        // is shared across estates now, so it re-arms per evaluation (C
        // evaluates under get_eval_mcontext, pl_exec.c:6197).
        se.state.arm_result_mcx(self.eval_ctx.mcx());
        // C pl_exec.c:6198-6204: stable/volatile functions must see our own
        // updates — CCI + fresh snapshot — but immutable-only expressions
        // and read-only functions skip the ceremony.
        let mut pushed = false;
        if se.mutable && !self.readonly_func {
            xact::CommandCounterIncrement()?;
            let snap = snapmgr::GetTransactionSnapshot()?;
            snapmgr::PushActiveSnapshot(&snap)?;
            pushed = true;
        }
        let _accept = AcceptArrayResult::set(se.adopt_call);
        let result = (|| {
            let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
            let r = execexpr::exec_eval_expr(&mut se.state, &mut slots)?;
            Ok((r.value, r.isnull, se.rettype, se.rettypmod))
        })();
        // The shared program must never keep a pointer to a dead estate.
        se.state.arm_param_callback_arg(core::ptr::null_mut());
        if pushed {
            let popped = snapmgr::PopActiveSnapshot();
            if result.is_ok() {
                popped?;
            }
        }
        result
    }

    // C exec_run_select: non-SELECT is 42601. RETURN QUERY is not this helper.
    fn exec_run_select(&mut self, expr: &PlExpr, maxtuples: i64) -> PgResult<i32> {
        let rc = self.exec_spi_plan(expr, maxtuples, false)?;
        if rc != spi::SPI_OK_SELECT {
            let msg = if rc == spi::SPI_OK_SELINTO {
                "query is SELECT INTO, but it should be plain SELECT"
            } else {
                "query is not a SELECT"
            };
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_SYNTAX_ERROR)
                    .errmsg(msg)
                    .errcontext_msg(format!("query: {}", expr.query))
                    .into_error(),
            ));
        }
        Ok(rc)
    }

    // C: SPI_execute_plan_extended with options.params = setup_param_list
    // (hooked) and options.must_return_tuples per the statement (RETURN
    // QUERY passes true, pl_exec.c exec_stmt_return_query).
    fn exec_spi_plan(
        &mut self,
        expr: &PlExpr,
        maxtuples: i64,
        must_return_tuples: bool,
    ) -> PgResult<i32> {
        self.exec_spi_plan_dest(expr, maxtuples, must_return_tuples, None)
    }

    fn exec_spi_plan_dest(
        &mut self,
        expr: &PlExpr,
        maxtuples: i64,
        must_return_tuples: bool,
        dest: Option<&mut tcop_dest::DestReceiver<'_>>,
    ) -> PgResult<i32> {
        let (plan, paramnos, argtypes) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&expr.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone())
        });
        let params = self.setup_param_list_under_spi(expr, &paramnos, &argtypes, true)?;
        let mut fetch_arg = self.spi_fetch_arg(&paramnos);
        let params = spi_param_list(&params, &mut fetch_arg);
        spi_frame!(_frame, &expr.query, expr.parse_mode);
        let _live = LiveSpiExpr::set((self as *mut Estate<'a>).cast(), expr);
        let rc = spi::SPI_execute_plan_extended(
            plan,
            params,
            self.readonly_func,
            false,
            must_return_tuples,
            maxtuples,
            dest,
        )
        .map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))?;
        self.eval_processed = spi::SPI_processed();
        if let Some(t) = self.eval_tuptable.take() {
            let _ = spi::SPI_freetuptable(t);
        }
        self.eval_tuptable = spi::SPI_tuptable();
        Ok(rc)
    }

    fn exec_eval_boolean(&mut self, expr: &PlExpr) -> PgResult<(bool, bool)> {
        let (v, mut isnull, t, m) = self.exec_eval_expr(expr)?;
        let v = self.exec_cast_value(v, &mut isnull, t, m, BOOLOID, -1)?;
        Ok((v.as_bool(), isnull))
    }

    // ------------------------------------------------------------------
    // Casts (get_cast_hashentry over a Param placeholder instead of C's
    // CaseTestExpr — identical coercion tree, supported by execexpr).
    // ------------------------------------------------------------------

    pub fn exec_cast_value(
        &mut self,
        value: Datum,
        isnull: &mut bool,
        valtype: Oid,
        valtypmod: i32,
        reqtype: Oid,
        reqtypmod: i32,
    ) -> PgResult<Datum> {
        if valtype == reqtype && (valtypmod == reqtypmod || reqtypmod == -1) {
            return Ok(value);
        }
        self.do_cast_value(value, isnull, valtype, valtypmod, reqtype, reqtypmod)
    }

    #[inline(never)]
    fn do_cast_value(
        &mut self,
        value: Datum,
        isnull: &mut bool,
        valtype: Oid,
        valtypmod: i32,
        reqtype: Oid,
        reqtypmod: i32,
    ) -> PgResult<Datum> {
        let key = (valtype, valtypmod, reqtype, reqtypmod);
        if OidIsValid(self.func.fn_oid) {
            return self.session_cast_value(value, isnull, key);
        }
        let cur_gen = plancache::PlanCacheInvalCounter();
        let cur_lxid = current_lxid();
        let stale = match self.cast_cache.get(&key) {
            Some(e) => e.inval_gen != cur_gen || e.lxid != cur_lxid,
            None => true,
        };
        if stale {
            let entry = self.build_cast_entry(valtype, valtypmod, reqtype, reqtypmod)?;
            self.cast_cache.insert(key, entry);
        }
        let entry = self.cast_cache.get_mut(&key).expect("inserted");
        let Some(state) = entry.state.as_mut() else {
            return Ok(value);
        };
        entry.param[0].value = value;
        entry.param[0].isnull = *isnull;
        let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
        let r = execexpr::exec_eval_expr(state, &mut slots)?;
        *isnull = r.isnull;
        Ok(r.value)
    }

    fn build_cast_entry(
        &mut self,
        srctype: Oid,
        srctypmod: i32,
        dsttype: Oid,
        dsttypmod: i32,
    ) -> PgResult<CastEntry> {
        // Read before catalog lookups: an inval firing mid-build leaves this
        // entry stamped old, so the next use rebuilds.
        let inval_gen = plancache::PlanCacheInvalCounter();
        let mcx = self.datum_ctx.mcx();
        let expr = build_cast_expr(mcx, srctype, srctypmod, dsttype, dsttypmod)?;
        let (state, param) = compile_cast(mcx, expr, srctype)?;
        let state = state.map(|mut st| {
            st.arm_result_mcx(self.eval_ctx.mcx());
            st
        });
        Ok(CastEntry { state, param, inval_gen, lxid: current_lxid() })
    }

    // get_cast_hashentry's shared_cast_hash arm (pl_exec.c:7952-8070) for
    // a function's casts: the expression is session-lived and invalidated
    // only through its dependencies (C GetCachedExpression), the compiled
    // state is rebuilt once per transaction and shared by every function of
    // the session, so an EXECUTE privilege checked when the state was built
    // holds for the rest of the transaction. A state in use (a cast reached
    // recursively, or one whose evaluation failed) is not reused.
    fn session_cast_value(
        &mut self,
        value: Datum,
        isnull: &mut bool,
        key: (Oid, i32, Oid, i32),
    ) -> PgResult<Datum> {
        ensure_session_cast_callbacks()?;
        register_exit_release();
        let valid = SESSION_CASTS.with(|t| {
            let mut t = t.borrow_mut();
            if SESSION_CASTS_RESET.replace(false) {
                t.values_mut().for_each(|e| e.valid = false);
            }
            t.get(&key).is_some_and(|e| e.valid)
        });
        if !valid {
            let expr_ctx = std::rc::Rc::new(Ctx::new_start_small("PLpgSQL cast expression"));
            let expr = build_cast_expr(expr_ctx.mcx(), key.0, key.1, key.2, key.3)?;
            let deps = match expr {
                Some(e) => cast_expr_dependencies(e)?,
                None => Vec::new(),
            };
            let generation = SESSION_CAST_GEN.with(|g| {
                g.set(g.get() + 1);
                g.get()
            });
            let entry = SessionCast { expr, deps, valid: true, generation, state: None, lxid: 0, expr_ctx };
            let old = SESSION_CASTS.with(|t| t.borrow_mut().insert(key, entry));
            drop(old);
        }
        let cur = current_lxid();
        let (expr, generation, ready, expr_ctx) = SESSION_CASTS.with(|t| {
            let mut t = t.borrow_mut();
            let e = t.get_mut(&key).expect("built above");
            let ready = e.state.take().filter(|_| e.lxid == cur);
            (e.expr, e.generation, ready, std::rc::Rc::clone(&e.expr_ctx))
        });
        let Some(expr) = expr else {
            return Ok(value);
        };
        let mut st = match ready {
            Some(st) => st,
            None => {
                let ctx = Ctx::new_start_small("PLpgSQL cast state");
                let expr = copyfuncs::copy_object(ctx.mcx(), expr)?;
                let (state, param) = compile_cast(ctx.mcx(), Some(expr), key.0)?;
                let Some(state) = state else {
                    return Ok(value);
                };
                CastState { state, param, ctx, _expr: expr_ctx }
            }
        };
        st.param[0].value = value;
        st.param[0].isnull = *isnull;
        st.state.arm_result_mcx(self.eval_ctx.mcx());
        let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
        let r = execexpr::exec_eval_expr(&mut st.state, &mut slots)?;
        *isnull = r.isnull;
        SESSION_CASTS.with(|t| {
            if let Some(e) = t.borrow_mut().get_mut(&key) {
                if e.generation == generation && e.valid && e.state.is_none() {
                    e.state = Some(st);
                    e.lxid = cur;
                }
            }
        });
        Ok(r.value)
    }

    // convert_value_to_string: type output function in eval scratch.
    fn convert_value_to_string(&mut self, value: Datum, valtype: Oid) -> PgResult<String> {
        let (foutoid, _) = lsyscache::typ::getTypeOutputInfo(valtype)?;
        let mut finfo = fmgr_core::fmgr_info(foutoid)?;
        let out = fmgr::function_call1_coll_in(
            &mut finfo,
            types_core::InvalidOid,
            self.eval_ctx.mcx(),
            value,
        )?;
        // SAFETY: type output functions return a NUL-terminated cstring.
        let s = unsafe {
            core::ffi::CStr::from_ptr(out.as_usize() as *const core::ffi::c_char)
        };
        // Byte-faithful conversion, matching C's convert_value_to_string, which
        // returns the type output function's server-encoding bytes verbatim.
        // NEVER lossily substitute U+FFFD for non-UTF-8 bytes: the result feeds
        // dynamic-SQL text (EXECUTE), RAISE messages, and USING-param formatting,
        // and silent substitution would corrupt the executed statement. Valid
        // bytes pass through unchanged; genuinely invalid input raises a
        // catchable encoding error instead of being silently mangled.
        let bytes = s.to_bytes();
        match core::str::from_utf8(bytes) {
            Ok(v) => Ok(v.to_owned()),
            Err(e) => {
                let start = e.valid_up_to();
                let n = e.error_len().unwrap_or(bytes.len() - start).max(1);
                let mut seq = String::new();
                for b in &bytes[start..(start + n).min(bytes.len())] {
                    if !seq.is_empty() {
                        seq.push(' ');
                    }
                    seq.push_str(&format!("0x{b:02x}"));
                }
                Err(exec_err(
                    types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE,
                    format!("invalid byte sequence for encoding \"UTF8\": {seq}"),
                ))
            }
        }
    }

    // appendStringInfoStringQuoted(-1) (stringinfo_mb.c): single-quote the
    // whole value, doubling embedded quotes.
    fn append_quoted(out: &mut String, s: &str) {
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
        out.push('\'');
    }

    // exec_eval_datum's reported type for a parameter datum: the datum's OWN
    // current type, exactly as C's exec_eval_datum yields it. This is what
    // format_expr_params must format each value with — NOT the type stored in
    // the (mutable) EXPR_PLANS entry. The plan entry's argtypes can be swapped
    // out from under an outer frame by a nested re-preparation whose estate
    // holds a different type for the same dno (e.g. a RECORD field that is
    // TEXTOID in the nested estate but int8 in the outer one). Formatting the
    // outer datum with the plan's (now-wrong) type oid selects the wrong output
    // function and dereferences the raw datum as the wrong type — a type
    // confusion / arbitrary-read primitive. Deriving the type from the datum
    // itself makes the output function always match the value.
    fn param_datum_type(&mut self, dno: Dno) -> PgResult<Oid> {
        let func = self.func;
        match &func.datums[dno as usize] {
            PlDatum::Var(_) => Ok(self.var_type(dno).typoid),
            PlDatum::Rec(r) => Ok(self.rec_param_type_mod(r.dno)?.0),
            PlDatum::RecField(f) => Ok(self
                .recfield_type(f)?
                .map(|(t, _, _)| t)
                // datum_as_param resolves the same field just before this and
                // errors if it is absent, so a value here implies Some.
                .expect("recfield type resolvable after datum_as_param")),
            _ => panic!("plpgsql: datum {dno} cannot be a parameter"),
        }
    }

    // format_expr_params (pl_exec.c); None when the expr takes no parameters.
    fn format_expr_params(&mut self, expr: &PlExpr) -> PgResult<Option<String>> {
        if !self.func.print_strict_params {
            return Ok(None);
        }
        let paramnos = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&expr.expr_id).expect("plan ensured");
            e.paramnos.clone()
        });
        if paramnos.is_empty() {
            return Ok(None);
        }
        let mut out = String::new();
        for (i, &dno) in paramnos.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let refname = match &self.func.datums[dno as usize] {
                PlDatum::Var(v) => v.refname.clone(),
                PlDatum::Row(r) => r.refname.clone(),
                PlDatum::Rec(r) => r.refname.clone(),
                // C reads ->refname through a PLpgSQL_var cast; a recfield's
                // same-offset member is its fieldname.
                PlDatum::RecField(f) => f.fieldname.clone(),
            };
            out.push_str(&refname);
            out.push_str(" = ");
            let (v, isnull) = self.datum_as_param(dno, None)?;
            if isnull {
                out.push_str("NULL");
            } else {
                // C's format_expr_params types each value via exec_eval_datum,
                // i.e. from the datum itself — never from the plan's argtypes.
                let valtype = self.param_datum_type(dno)?;
                let sv = self.convert_value_to_string(v, valtype)?;
                Self::append_quoted(&mut out, &sv);
            }
        }
        Ok(Some(out))
    }

    // format_preparedparamsdata (pl_exec.c) over EXECUTE USING values.
    fn format_prepared_params(
        &mut self,
        ptypes: &[Oid],
        pvalues: &[Datum],
        pnulls: &[bool],
    ) -> PgResult<Option<String>> {
        if !self.func.print_strict_params || ptypes.is_empty() {
            return Ok(None);
        }
        let mut out = String::new();
        for i in 0..ptypes.len() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("${} = ", i + 1));
            if pnulls[i] {
                out.push_str("NULL");
            } else {
                let sv = self.convert_value_to_string(pvalues[i], ptypes[i])?;
                Self::append_quoted(&mut out, &sv);
            }
        }
        Ok(Some(out))
    }
}

// exec_is_simple_query (pl_exec.c) items 2-4: plain rtable-less SELECT, none
// of the listed features, single result column.
fn exec_is_simple_query(query: &types_nodes::parsenodes::Query<'_>) -> bool {
    if query.commandType != types_nodes::nodes_enums::CmdType::CMD_SELECT {
        return false;
    }
    if !query.rtable.is_nil() {
        return false;
    }
    let (fromlist_empty, quals_none) = match query.jointree {
        Some(jt) => (jt.fromlist.is_nil(), jt.quals.is_none()),
        None => (true, true),
    };
    if query.hasAggs
        || query.hasWindowFuncs
        || query.hasTargetSRFs
        || query.hasSubLinks
        || !query.cteList.is_nil()
        || !fromlist_empty
        || !quals_none
        || !query.groupClause.is_nil()
        || !query.groupingSets.is_nil()
        || query.havingQual.is_some()
        || !query.windowClause.is_nil()
        || !query.distinctClause.is_nil()
        || !query.sortClause.is_nil()
        || query.limitOffset.is_some()
        || query.limitCount.is_some()
        || query.setOperations.is_some()
    {
        return false;
    }
    query.targetList.len() == 1
}

// exec_simple_check_plan's Result-node test on the built plan; returns the
// single tlist expr and its type.
fn simple_result_expr(
    stmt: &types_nodes::plannodes::PlannedStmt<'static>,
) -> Option<(types_nodes::Node<'static>, Oid, i32)> {
    use types_nodes::plannodes::{Gather, Material, Result};
    use types_nodes::primnodes::{Const, TargetEntry, Var, OUTER_VAR};
    let mut plan = stmt.planTree?;
    // exec_save_simple_expr (pl_exec.c:8300-8343): debug_parallel_query may
    // put a Gather atop the Result and a scrollable cursor a Material; look
    // through them unless setrefs.c copied a Const up.
    loop {
        let base = if let Some(result) = plan.as_variant::<Result>() {
            if result.resconstantqual.is_some() {
                return None;
            }
            &result.plan
        } else if let Some(gather) = plan.as_variant::<Gather>() {
            &gather.plan
        } else if let Some(material) = plan.as_variant::<Material>() {
            &material.plan
        } else {
            return None;
        };
        if !base.initPlan.is_nil() || !base.qual.is_nil() {
            return None;
        }
        let tlist = &base.targetlist;
        if tlist.len() != 1 {
            return None;
        }
        let te = tlist.first()?.as_variant::<TargetEntry>()?;
        let expr = te.expr;
        if plan.as_variant::<Result>().is_some() {
            if base.lefttree.is_some() {
                return None;
            }
            let t = nodes_core::node_funcs::expr_type(expr);
            let m = nodes_core::node_funcs::expr_typmod(expr);
            return Some((expr, t, m));
        }
        if expr.as_variant::<Const>().is_some() {
            let t = nodes_core::node_funcs::expr_type(expr);
            let m = nodes_core::node_funcs::expr_typmod(expr);
            return Some((expr, t, m));
        }
        if !expr.as_variant::<Var>().is_some_and(|v| v.varno == OUTER_VAR) {
            return None;
        }
        plan = base.lefttree?;
    }
}

impl<'a> Estate<'a> {
    // ------------------------------------------------------------------
    // Statement machine
    // ------------------------------------------------------------------

    pub fn exec_toplevel_block(&mut self, block: &'a PlBlock) -> PgResult<i32> {
        self.frame.stmt.set(Some((block.lineno, "statement block")));
        // pl_exec.c:1644
        check_for_interrupts()?;
        let rc = self.exec_stmt_block(block)?;
        self.frame.stmt.set(None);
        Ok(rc)
    }

    fn exec_stmts(&mut self, stmts: &'a [PlStmt]) -> PgResult<i32> {
        if stmts.is_empty() {
            // pl_exec.c:2001-2010: CHECK_FOR_INTERRUPTS() even though there
            // is no statement — prevents hangup in a tight loop if, for
            // instance, there is a LOOP construct with an empty body.
            check_for_interrupts()?;
            return Ok(RC_OK);
        }
        let save = self.frame.stmt.get();
        for s in stmts {
            self.frame.stmt.set(Some((stmt_lineno(s), stmt_typename(s))));
            // pl_exec.c:2023: per statement, before the dispatch switch
            check_for_interrupts()?;
            let rc = self.exec_stmt(s)?;
            if rc != RC_OK {
                self.frame.stmt.set(save);
                return Ok(rc);
            }
        }
        self.frame.stmt.set(save);
        Ok(RC_OK)
    }

    fn exec_stmt(&mut self, stmt: &'a PlStmt) -> PgResult<i32> {
        match stmt {
            PlStmt::Block(b) => {
                // pl_exec.c exec_stmt_block recursion is bounded by the
                // statement nesting; guard it like the core executor does.
                stack_depth_core::check_stack_depth()?;
                self.exec_stmt_block(b)
            }
            PlStmt::Assign { varno, expr, .. } => {
                self.exec_assign_expr(*varno, expr)?;
                Ok(RC_OK)
            }
            PlStmt::If { cond, then_body, elsifs, else_body, .. } => {
                let (value, isnull) = self.exec_eval_boolean(cond)?;
                self.exec_eval_cleanup();
                if !isnull && value {
                    return self.exec_stmts(then_body);
                }
                for (c, body) in elsifs {
                    let (value, isnull) = self.exec_eval_boolean(c)?;
                    self.exec_eval_cleanup();
                    if !isnull && value {
                        return self.exec_stmts(body);
                    }
                }
                if let Some(body) = else_body {
                    return self.exec_stmts(body);
                }
                Ok(RC_OK)
            }
            PlStmt::Loop { label, body, .. } => loop {
                let rc = self.exec_stmts(body)?;
                if let Some(rc) = self.loop_rc(label.as_deref(), rc) {
                    return Ok(rc);
                }
            },
            PlStmt::While { label, cond, body, .. } => loop {
                let (value, isnull) = self.exec_eval_boolean(cond)?;
                self.exec_eval_cleanup();
                if isnull || !value {
                    return Ok(RC_OK);
                }
                let rc = self.exec_stmts(body)?;
                if let Some(rc) = self.loop_rc(label.as_deref(), rc) {
                    return Ok(rc);
                }
            },
            PlStmt::ForI { label, var, lower, upper, step, reverse, body, .. } => {
                self.exec_stmt_fori(label.as_deref(), *var, lower, upper, step.as_ref(), *reverse, body)
            }
            PlStmt::ForS { label, var, query, body, .. } => {
                self.exec_stmt_fors(label.as_deref(), *var, query, body)
            }
            PlStmt::ExitContinue { is_exit, label, cond, .. } => {
                if let Some(c) = cond {
                    let (value, isnull) = self.exec_eval_boolean(c)?;
                    self.exec_eval_cleanup();
                    if isnull || !value {
                        return Ok(RC_OK);
                    }
                }
                self.exitlabel = label.clone();
                Ok(if *is_exit { RC_EXIT } else { RC_CONTINUE })
            }
            PlStmt::Return { expr, retvarno, .. } => {
                self.exec_stmt_return(expr.as_ref(), *retvarno)?;
                Ok(RC_RETURN)
            }
            PlStmt::Raise { .. } => self.exec_stmt_raise(stmt),
            PlStmt::Assert { cond, message, .. } => self.exec_stmt_assert(cond, message.as_ref()),
            PlStmt::ExecSql { sqlstmt, into, strict, target, .. } => {
                self.exec_stmt_execsql(sqlstmt, *into, *strict, *target)
            }
            PlStmt::Perform { expr, .. } => {
                self.ensure_plan(expr, CURSOR_OPT_PARALLEL_OK)?;
                let _ = self.exec_run_select(expr, 0)?;
                let found = self.eval_processed != 0;
                self.exec_set_found(found);
                self.exec_eval_cleanup();
                Ok(RC_OK)
            }
            PlStmt::Call { expr, is_call, .. } => self.exec_stmt_call(expr, *is_call),
            PlStmt::Commit { chain, .. } => self.exec_stmt_commit_rollback(true, *chain),
            PlStmt::Rollback { chain, .. } => self.exec_stmt_commit_rollback(false, *chain),
            PlStmt::DynExecute { query, into, strict, target, params, .. } => {
                self.exec_stmt_dynexecute(query, *into, *strict, *target, params)
            }
            PlStmt::GetDiag { is_stacked, items, .. } => {
                self.exec_stmt_getdiag(*is_stacked, items)
            }
            PlStmt::Case { t_expr, t_varno, whens, have_else, else_stmts, .. } => self
                .exec_stmt_case(
                    t_expr.as_ref(),
                    *t_varno,
                    whens,
                    *have_else,
                    else_stmts,
                ),
            PlStmt::ForEachA { label, varno, slice, expr, body, .. } => {
                self.exec_stmt_foreach_a(label.as_deref(), *varno, *slice, expr, body)
            }
            PlStmt::ReturnNext { expr, retvarno, .. } => {
                self.exec_stmt_return_next(expr.as_ref(), *retvarno)
            }
            PlStmt::ReturnQuery { query, dynquery, params, .. } => {
                self.exec_stmt_return_query(query.as_ref(), dynquery.as_ref(), params)
            }
            PlStmt::Open { curvar, cursor_options, argquery, query, dynquery, params, .. } => self
                .exec_stmt_open(
                    *curvar,
                    *cursor_options,
                    argquery.as_ref(),
                    query.as_ref(),
                    dynquery.as_ref(),
                    params,
                ),
            PlStmt::Fetch { target, curvar, direction, how_many, expr, is_move, .. } => {
                self.exec_stmt_fetch(*target, *curvar, *direction, *how_many, expr.as_ref(), *is_move)
            }
            PlStmt::Close { curvar, .. } => self.exec_stmt_close(*curvar),
            PlStmt::ForC { label, var, curvar, argquery, body, .. } => {
                self.exec_stmt_forc(label.as_deref(), *var, *curvar, argquery.as_ref(), body)
            }
            PlStmt::DynForS { label, var, query, params, body, .. } => {
                self.exec_stmt_dynfors(label.as_deref(), *var, query, params, body)
            }
        }
    }

    // exec_stmt_getdiag (pl_exec.c:2410).
    fn exec_stmt_getdiag(&mut self, is_stacked: bool, items: &[GetDiagItem]) -> PgResult<i32> {
        if is_stacked && self.cur_error.is_none() {
            return Err(exec_err(
                types_error::ERRCODE_STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                "GET STACKED DIAGNOSTICS cannot be used outside an exception handler"
                    .to_string(),
            ));
        }
        const OIDOID: Oid = 26;
        for item in items {
            match item.kind {
                GETDIAG_ROW_COUNT => {
                    let v = Datum::from_i64(self.eval_processed as i64);
                    self.exec_assign_value(item.target, v, false, INT8OID, -1)?;
                }
                GETDIAG_ROUTINE_OID => {
                    let v = Datum::from_oid(self.func.fn_oid);
                    self.exec_assign_value(item.target, v, false, OIDOID, -1)?;
                }
                GETDIAG_CONTEXT => {
                    let s = get_error_context_stack();
                    self.exec_assign_c_string(item.target, Some(&s))?;
                }
                _ => {
                    let e = self.cur_error.as_ref().expect("stacked item without cur_error");
                    let s: Option<String> = match item.kind {
                        GETDIAG_ERROR_CONTEXT => match (&e.context, &self.cur_error_outer_context) {
                            (Some(c), Some(outer)) => Some(format!("{c}\n{outer}")),
                            (Some(c), None) => Some(c.clone()),
                            (None, outer) => outer.clone(),
                        },
                        GETDIAG_ERROR_DETAIL => e.detail.clone(),
                        GETDIAG_ERROR_HINT => e.hint.clone(),
                        GETDIAG_RETURNED_SQLSTATE => Some(unpack_sql_state(e.sqlstate)),
                        GETDIAG_COLUMN_NAME => e.column_name.clone(),
                        GETDIAG_CONSTRAINT_NAME => e.constraint_name.clone(),
                        GETDIAG_DATATYPE_NAME => e.datatype_name.clone(),
                        GETDIAG_MESSAGE_TEXT => Some(e.message.clone()),
                        GETDIAG_TABLE_NAME => e.table_name.clone(),
                        GETDIAG_SCHEMA_NAME => e.schema_name.clone(),
                        other => panic!("unrecognized diagnostic item kind: {other}"),
                    };
                    self.exec_assign_c_string(item.target, s.as_deref())?;
                }
            }
        }
        self.exec_eval_cleanup();
        Ok(RC_OK)
    }

    // exec_assign_c_string: a NULL C string assigns an empty string.
    fn exec_assign_c_string(&mut self, target: Dno, s: Option<&str>) -> PgResult<()> {
        let s = s.unwrap_or("");
        let v = varlena::cstring_to_text(self.eval_ctx.mcx(), s.as_bytes())?;
        self.exec_assign_value(target, fmgr::varlena_result(v), false, TEXTOID, -1)
    }

    // exec_stmt_case (pl_exec.c:2556).
    fn exec_stmt_case(
        &mut self,
        t_expr: Option<&PlExpr>,
        t_varno: Dno,
        whens: &'a [(PlExpr, Vec<PlStmt>)],
        have_else: bool,
        else_stmts: &'a [PlStmt],
    ) -> PgResult<i32> {
        let have_t = t_expr.is_some();
        if let Some(te) = t_expr {
            let (t_val, isnull, t_typoid, t_typmod) = self.exec_eval_expr(te)?;
            {
                let cur = self.var_type(t_varno);
                if cur.typoid != t_typoid || cur.atttypmod != t_typmod {
                    let ty = crate::comp::CompState::build_datatype(
                        t_typoid,
                        t_typmod,
                        self.func.fn_input_collation,
                        None,
                    )?;
                    self.retype_var(t_varno, ty);
                }
            }
            let (tl, bv) = {
                let t = self.var_type(t_varno);
                (t.typlen, t.typbyval)
            };
            let stored = self.assign_copy_to_datum_ctx(t_val, isnull, tl, bv)?;
            self.set_var(t_varno, stored, isnull, !isnull && !bv);
            self.exec_eval_cleanup();
        }

        for (expr, stmts) in whens {
            let (value, isnull) = self.exec_eval_boolean(expr)?;
            self.exec_eval_cleanup();
            if !isnull && value {
                if have_t {
                    self.set_var(t_varno, Datum::null(), true, false);
                }
                return self.exec_stmts(stmts);
            }
        }

        if have_t {
            self.set_var(t_varno, Datum::null(), true, false);
        }
        if !have_else {
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_CASE_NOT_FOUND)
                    .errmsg("case not found")
                    .errhint("CASE statement is missing ELSE part.")
                    .into_error(),
            ));
        }
        self.exec_stmts(else_stmts)
    }

    // exec_stmt_foreach_a (pl_exec.c:3008).
    fn exec_stmt_foreach_a(
        &mut self,
        label: Option<&str>,
        varno: Dno,
        slice: i32,
        expr: &PlExpr,
        body: &'a [PlStmt],
    ) -> PgResult<i32> {
        use arrayfuncs::foundation::read_dims_lbounds;

        let (value, isnull, arrtype, arrtypmod) = self.exec_eval_expr(expr)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "FOREACH expression must not be null".to_string(),
            ));
        }
        let elemtype = lsyscache::typ::get_element_type(arrtype)?;
        if !OidIsValid(elemtype) {
            return Err(exec_err(
                types_error::ERRCODE_DATATYPE_MISMATCH,
                format!(
                    "FOREACH expression must yield an array, not type {}",
                    format_type::format_type_be(arrtype)?
                ),
            ));
        }

        // C's private stmt_mcontext: the array copy must survive the body's
        // eval resets.
        let stmt_ctx = Ctx::new("PLpgSQL FOREACH");
        let arr = array_datum_copy(stmt_ctx.mcx(), value)?;
        self.exec_eval_cleanup();
        let arr: &[u8] = &arr;

        let (ndim, dims, lbs) = read_dims_lbounds(arr);
        if slice < 0 || slice > ndim {
            return Err(exec_err(
                types_error::ERRCODE_ARRAY_SUBSCRIPT_ERROR,
                format!("slice dimension ({slice}) is out of the valid range 0..{ndim}"),
            ));
        }

        // pl_exec.c:3070-3077: a record/row loop variable is never an array
        // (and may be uninitialized); anything else is probed through
        // plpgsql_exec_get_datum_type, whose RECFIELD arm instantiates a
        // named-composite record and looks the field up in the live tuple.
        let func: &'a PlFunction = self.func;
        let loop_var_elem = match &func.datums[varno as usize] {
            PlDatum::Rec(_) | PlDatum::Row(_) => types_core::InvalidOid,
            PlDatum::RecField(f) => {
                let ftype = match self.recfield_type(f)? {
                    Some((t, _, _)) => t,
                    None => {
                        // C plpgsql_exec_get_datum_type RECFIELD arm
                        // (pl_exec.c:5474-5498): a still-NULL RECORD-typed
                        // record is instantiate_empty_record_variable's
                        // 55000; an assigned one without the field is 42703.
                        if matches!(&self.datums[f.recparentno as usize], DatumVal::Rec(None)) {
                            self.instantiate_empty_rec(f.recparentno)?;
                        }
                        let recname = match &func.datums[f.recparentno as usize] {
                            PlDatum::Rec(r) => r.refname.clone(),
                            _ => String::new(),
                        };
                        return Err(exec_err(
                            types_error::ERRCODE_UNDEFINED_COLUMN,
                            format!("record \"{recname}\" has no field \"{}\"", f.fieldname),
                        ));
                    }
                };
                lsyscache::typ::get_element_type(ftype)?
            }
            _ => lsyscache::typ::get_element_type(self.var_type(varno).typoid)?,
        };
        if slice > 0 && !OidIsValid(loop_var_elem) {
            return Err(exec_err(
                types_error::ERRCODE_DATATYPE_MISMATCH,
                "FOREACH ... SLICE loop variable must be of an array type".to_string(),
            ));
        }
        if slice == 0 && OidIsValid(loop_var_elem) {
            return Err(exec_err(
                types_error::ERRCODE_DATATYPE_MISMATCH,
                "FOREACH loop variable must not be of an array type".to_string(),
            ));
        }

        let (elmlen, elmbyval, elmalign) = lsyscache::typ::get_typlenbyvalalign(elemtype)?;
        let (elems, nulls) = arrayfuncs::construct::deconstruct_array(
            stmt_ctx.mcx(),
            arr,
            elmlen as i32,
            elmbyval,
            elmalign as u8,
            true,
        )?;

        let mut found = false;
        let mut rc = RC_OK;
        if slice == 0 {
            for i in 0..elems.len() {
                found = true;
                self.exec_assign_value(varno, elems[i], nulls[i], elemtype, arrtypmod)?;
                self.exec_eval_cleanup();
                rc = self.exec_stmts(body)?;
                if let Some(r) = self.loop_rc(label, rc) {
                    rc = r;
                    break;
                }
                // C's LOOP_RC_PROCESSING (pl_exec.c): a consumed CONTINUE
                // resets rc to OK before the next iteration, so exhaustion
                // right after CONTINUE returns RC_OK, not a stale RC_CONTINUE.
                rc = RC_OK;
            }
        } else {
            let outer: usize = dims[..(ndim - slice) as usize]
                .iter()
                .map(|&d| d as usize)
                .product();
            let slice_dims = &dims[(ndim - slice) as usize..ndim as usize];
            let slice_lbs = &lbs[(ndim - slice) as usize..ndim as usize];
            let slice_len: usize = slice_dims.iter().map(|&d| d as usize).product();
            for s in 0..outer {
                let sub = arrayfuncs::construct::construct_md_array(
                    self.eval_ctx.mcx(),
                    &elems[s * slice_len..(s + 1) * slice_len],
                    Some(&nulls[s * slice_len..(s + 1) * slice_len]),
                    slice,
                    slice_dims,
                    slice_lbs,
                    elemtype,
                    elmlen as i32,
                    elmbyval,
                    elmalign as u8,
                )?;
                found = true;
                let sub_ptr = sub.as_ptr() as usize;
                // The slice is eval scratch (C pfree's it right after the
                // assignment, pl_exec.c:3126): the reset below frees it.
                core::mem::forget(sub);
                self.exec_assign_value(varno, Datum::from_usize(sub_ptr), false, arrtype, arrtypmod)?;
                self.exec_eval_cleanup();
                rc = self.exec_stmts(body)?;
                if let Some(r) = self.loop_rc(label, rc) {
                    rc = r;
                    break;
                }
                // See the slice==0 arm: consumed CONTINUE must reset rc.
                rc = RC_OK;
            }
        }

        self.exec_set_found(found);
        Ok(rc)
    }

    // LOOP_RC_PROCESSING: Some(rc) = terminate loop with rc, None = iterate.
    fn loop_rc(&mut self, label: Option<&str>, rc: i32) -> Option<i32> {
        match rc {
            RC_OK => None,
            RC_RETURN => Some(RC_RETURN),
            RC_EXIT => {
                if self.exitlabel.is_none() {
                    Some(RC_OK)
                } else if label.is_some() && self.exitlabel.as_deref() == label {
                    self.exitlabel = None;
                    Some(RC_OK)
                } else {
                    Some(RC_EXIT)
                }
            }
            RC_CONTINUE => {
                if self.exitlabel.is_none() {
                    None
                } else if label.is_some() && self.exitlabel.as_deref() == label {
                    self.exitlabel = None;
                    None
                } else {
                    Some(RC_CONTINUE)
                }
            }
            _ => unreachable!("bad rc"),
        }
    }

    fn exec_stmt_block(&mut self, block: &'a PlBlock) -> PgResult<i32> {
        self.frame.text.set(Some("during statement block local variable initialization"));
        for &dno in &block.initvarnos {
            // estate->err_var = datum (exec_stmt_block): the context line
            // carries the variable's declaration lineno.
            self.frame.var_lineno.set(Some(match &self.func.datums[dno as usize] {
                PlDatum::Var(v) => v.lineno,
                PlDatum::Rec(r) => r.lineno,
                _ => 0,
            }));
            match &self.func.datums[dno as usize] {
                PlDatum::Var(v) => {
                    if let Some(default_val) = &v.default_val {
                        self.exec_assign_expr(dno, default_val)?;
                    } else {
                        self.set_var(dno, Datum::null(), true, false);
                        if v.notnull {
                            return Err(exec_err(
                                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                                format!(
                                    "variable \"{}\" declared NOT NULL cannot default to NULL",
                                    v.refname
                                ),
                            ));
                        }
                        // C (pl_exec.c:1702-1719): a defaultless domain var
                        // gets NULL assigned through an UNKNOWN cast so the
                        // domain's NOT NULL/CHECK constraints run.
                        if v.datatype.typtype == TYPTYPE_DOMAIN {
                            self.exec_assign_value(dno, Datum::null(), true, UNKNOWNOID, -1)?;
                        }
                    }
                }
                PlDatum::Rec(r) => {
                    // pl_exec.c:1727-1748 exec_move_row(rec, NULL, NULL):
                    // the previous expanded record is deleted.
                    self.assign_record_var(dno, None);
                    if let Some(default_val) = &r.default_val {
                        self.exec_assign_expr(dno, default_val)?;
                    } else if r.datatype.as_ref().is_some_and(|d| d.typtype == TYPTYPE_DOMAIN) {
                        // C pl_exec.c:1737-1748: a defaultless rec runs
                        // exec_move_row(NULL, NULL); for a composite-domain
                        // rec that makes an empty expanded record and
                        // domain-checks the NULL, so a NULL-rejecting domain
                        // errors during block local variable initialization.
                        self.revalidate_rectypeid(dno)?;
                        adt_domains::domain_check(Datum::null(), true, self.rec_typeid(dno))?;
                        self.instantiate_empty_rec(dno)?;
                    }
                }
                _ => {}
            }
        }
        self.frame.var_lineno.set(None);
        self.frame.text.set(None);

        let rc = if let Some(exc) = &block.exceptions {
            self.exec_block_with_exceptions(block, exc)?
        } else {
            self.exec_stmts(&block.body)?
        };
        self.frame.text.set(None);

        // C's block rc handling: CONTINUE never matches a block; EXIT matches
        // only on a label match.
        match rc {
            RC_OK | RC_RETURN | RC_CONTINUE => Ok(rc),
            RC_EXIT => {
                if self.exitlabel.is_some()
                    && block.label.as_deref() == self.exitlabel.as_deref()
                {
                    self.exitlabel = None;
                    Ok(RC_OK)
                } else {
                    Ok(RC_EXIT)
                }
            }
            _ => unreachable!("bad rc"),
        }
    }

    // exec_stmt_block's exception arm: body under an internal subtransaction;
    // on error, roll the subxact back and run the first matching handler.
    fn exec_block_with_exceptions(
        &mut self,
        block: &'a PlBlock,
        exc: &'a ExceptionBlock,
    ) -> PgResult<i32> {
        let save_owner = resowner::CurrentResourceOwner();
        self.frame.text.set(Some("during statement block entry"));
        xact::BeginInternalSubTransaction(None)?;
        self.frame.text.set(None);

        // Loud panics unwind without a PgResult; the subtransaction must
        // still die with the unwind or a later ROLLBACK walks a stack with a
        // leaked SUBINPROGRESS over a block-less top (C's PG_CATCH cannot be
        // bypassed this way).
        // unwind-ok: c-callback
        let body_result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.exec_stmts(&block.body)
        })) {
            Ok(r) => r,
            // A panic_any(PgError) from an infallible-by-signature helper
            // (nodes_core promoted_array_type's 42704) is C's ereport
            // longjmp into PG_CATCH: the handlers below must see it.
            Err(payload) => match types_error::pg_error_from_panic(payload) {
                Ok(e) => Err(Box::new(e)),
                Err(payload) => {
                    let _ = xact::RollbackAndReleaseCurrentSubTransaction();
                    resowner::SetCurrentResourceOwner(save_owner);
                    std::panic::resume_unwind(payload);
                }
            },
        };

        // C's PG_TRY extends over the return-value transfer and the subxact
        // release; errors there reach the same handlers.
        let attempt = body_result.and_then(|rc| {
            self.frame.text.set(Some("during statement block exit"));
            // The return value must survive subxact exit (C datumTransfer
            // out of the subxact eval_econtext).
            if rc == RC_RETURN && !self.retisnull && self.ret_rec.is_none()
                && OidIsValid(self.rettype)
            {
                let (typlen, typbyval) = lsyscache::typ::get_typlenbyval(self.rettype)?;
                self.retval = self.copy_to_datum_ctx(self.retval, false, typlen, typbyval)?;
            }
            xact::ReleaseCurrentSubTransaction()?;
            resowner::SetCurrentResourceOwner(save_owner);
            self.frame.text.set(None);
            Ok(rc)
        });

        match attempt {
            Ok(rc) => Ok(rc),
            Err(e) => {
                // Only ERROR is catchable; FATAL/PANIC never longjmp in C
                // (the backend exits) — release the subxact and propagate.
                if e.level > ERROR {
                    let _ = xact::RollbackAndReleaseCurrentSubTransaction();
                    resowner::SetCurrentResourceOwner(save_owner);
                    return Err(e);
                }
                // C errfinish zeroes the holdoff/crit-section counters before
                // longjmp to PG_CATCH and promotes an in-crit-section ERROR to
                // PANIC; the catching frame owns both here (tcop precedent).
                ::elog::panic_on_crit_section_escape(&e);
                init_small::globals::SetInterruptHoldoffCount(0);
                init_small::globals::SetQueryCancelHoldoffCount(0);
                init_small::globals::SetCritSectionCount(0);
                // Bake this frame's context with the throw-time stmt/text
                // before the cleanup markers overwrite them (C's callback ran
                // at errfinish).
                let mut edata = attach_frame_context_at_catch(e, self);
                edata.output_to_server
                    .get_or_insert_with(|| ::elog::should_output_to_server(edata.level));
                edata.output_to_client
                    .get_or_insert_with(|| ::elog::should_output_to_client(edata.level));
                let outer_context = outer_context_at_catch(self);
                self.frame.text.set(Some("during exception cleanup"));
                xact::RollbackAndReleaseCurrentSubTransaction()?;
                resowner::SetCurrentResourceOwner(save_owner);
                // The subxact abort freed tuple tables made inside it
                // (AtEOSubXact_SPI); drop the handle without a second free.
                self.eval_tuptable = None;
                self.reset_eval_ctx();

                let matched = exc
                    .exc_list
                    .iter()
                    .find(|x| exception_matches_conditions(&edata, &x.conditions));
                match matched {
                    Some(exception) => {
                        let ss = unpack_sql_state(edata.sqlstate);
                        self.assign_text_var(exc.sqlstate_varno, &ss)?;
                        let msg = edata.message.clone();
                        self.assign_text_var(exc.sqlerrm_varno, &msg)?;
                        let save = self.cur_error.replace(edata);
                        let save_outer = self.cur_error_outer_context.take();
                        self.cur_error_outer_context = outer_context;
                        self.frame.text.set(None);
                        let rc = self.exec_stmts(&exception.action);
                        self.cur_error = save;
                        self.cur_error_outer_context = save_outer;
                        Ok(rc?)
                    }
                    None => Err(edata),
                }
            }
        }
    }

    // assign_text_var (pl_exec.c:8838-8842): CStringGetTextDatum, stored as
    // a freeable copy (built in the eval scratch, datumCopy'd into the
    // function's context like every other owned value).
    pub fn assign_text_var(&mut self, dno: Dno, s: &str) -> PgResult<()> {
        let v = varlena::cstring_to_text(self.eval_ctx.mcx(), s.as_bytes())?;
        let stored = self.copy_to_datum_ctx(fmgr::varlena_result(v), false, -1, false)?;
        self.set_var(dno, stored, false, true);
        Ok(())
    }

    fn exec_assign_expr(&mut self, target: Dno, expr: &PlExpr) -> PgResult<()> {
        // C's plan prepare resolves the recfield target via make_datum_param
        // -> exec_get_datum_type, which errors positionless under the SPI
        // context callback; mirror that before planning.
        let planned = EXPR_PLANS.with(|t| t.borrow().contains_key(&expr.expr_id));
        if let (false, PlDatum::RecField(f)) = (planned, &self.func.datums[target as usize]) {
            if self.recfield_type(f)?.is_none() {
                let recname = match &self.func.datums[f.recparentno as usize] {
                    PlDatum::Rec(r) => r.refname.clone(),
                    _ => String::new(),
                };
                let err = if matches!(&self.datums[f.recparentno as usize], DatumVal::Rec(Some(_)))
                {
                    exec_err(
                        types_error::ERRCODE_UNDEFINED_COLUMN,
                        format!("record \"{recname}\" has no field \"{}\"", f.fieldname),
                    )
                } else {
                    Box::new(
                        elog::ereport(ERROR)
                            .errcode(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                            .errmsg(format!("record \"{recname}\" is not assigned yet"))
                            .errdetail(
                                "The tuple structure of a not-yet-assigned record is indeterminate.",
                            )
                            .into_error(),
                    )
                };
                return Err(spi_ctx_err(err, &expr.query, expr.parse_mode));
            }
        }
        // C prepares an assignment's plan once (pl_exec.c exec_assign_expr:
        // expr->plan == NULL); a planned expression goes straight to
        // exec_eval_expr, whose miss path revalidates.
        if !planned {
            self.ensure_plan(expr, 0)?;
        }
        let rec_target = matches!(&self.func.datums[target as usize], PlDatum::Rec(_));
        self.bare_rec_wanted = rec_target;
        let evaluated = self.exec_eval_expr_opts(expr, 0);
        self.bare_rec_wanted = false;
        let (mut value, mut isnull, valtype, valtypmod) = evaluated?;
        if let Some((src, planned)) = self.bare_rec_hit.take() {
            if self.assign_rec_from_rec(target, src)? {
                self.exec_eval_cleanup();
                return Ok(());
            }
            (value, isnull) = self.datum_as_param(src, Some(planned))?;
        }
        self.exec_assign_value(target, value, isnull, valtype, valtypmod)?;
        self.exec_eval_cleanup();
        Ok(())
    }

    // exec_move_row_from_datum's expanded-record arm for `rec := other_rec`
    // where the target takes the source's shape unchanged (declared RECORD,
    // or the same non-domain rowtype): the fields are copied directly, with
    // no composite image formed and deformed. Ok(false) = take the general
    // path (NULL or empty source, external toast fields, any coercion).
    fn assign_rec_from_rec(&mut self, target: Dno, src: Dno) -> PgResult<bool> {
        let DatumVal::Rec(Some(sv)) = &self.datums[src as usize] else {
            return Ok(false);
        };
        if sv.empty {
            return Ok(false);
        }
        if src == target {
            return Ok(true);
        }
        for f in 0..sv.values.len() {
            if !sv.nulls[f] && sv.desc.typlens[f] == -1 {
                // SAFETY: a non-null varlena field image held by the record.
                if unsafe { types_tuple::varatt::varatt_is_1b_e(sv.values[f].as_usize() as *const u8) } {
                    return Ok(false);
                }
            }
        }
        let src_rectypeid = self.rec_typeid(src);
        let src_td = sv.src_desc.clone().expect("RecValue carries its source tupdesc");
        let image_type = if src_rectypeid != RECORDOID {
            Self::rec_base_typeid(src_rectypeid)?
        } else {
            src_td.tdtypeid
        };
        self.revalidate_rectypeid(target)?;
        let rectypeid = self.rec_typeid(target);
        let held = match &self.datums[target as usize] {
            DatumVal::Rec(Some(tv)) => tv.src_desc.as_ref().is_some_and(|td| {
                td.tdtypeid == image_type
                    && (image_type != RECORDOID || (src_td.tdtypmod == td.tdtypmod && src_td.tdtypmod >= 0))
            }),
            _ => false,
        };
        let dst_td = if rectypeid == RECORDOID {
            src_td
        } else {
            if Self::rec_base_typeid(rectypeid)? != rectypeid || rectypeid != image_type {
                return Ok(false);
            }
            let var_td = self.declared_rec_tupdesc(rectypeid)?;
            if !std::rc::Rc::ptr_eq(&var_td, &src_td) {
                return Ok(false);
            }
            var_td
        };
        let DatumVal::Rec(Some(sv)) = &self.datums[src as usize] else {
            unreachable!("source record checked above");
        };
        let sys = if sv.fvalue_valid && (rectypeid == RECORDOID || rectypeid == image_type || held) {
            sv.sys.clone()
        } else {
            None
        };
        let desc = sv.desc.clone();
        let mut values = ::mcx::std_vec_from_slice("plpgsql", &sv.values)?;
        let nulls = ::mcx::std_vec_from_slice("plpgsql", &sv.nulls)?;
        let natts = values.len();
        let mut owned = ::mcx::std_vec_from_elem("plpgsql", false, natts)?;
        for f in 0..natts {
            if !desc.dropped[f] && !nulls[f] && !desc.typbyvals[f] {
                values[f] = self.assign_copy_to_datum_ctx(values[f], false, desc.typlens[f], false)?;
                owned[f] = true;
            }
        }
        self.assign_record_var(
            target,
            Some(RecValue {
                desc,
                values,
                nulls,
                src_desc: Some(dst_td),
                empty: false,
                fvalue_valid: sys.is_some(),
                sys,
                owned,
            }),
        );
        Ok(true)
    }

    // The type half of rec_param_type_mod, without registering a typmod.
    fn rec_value_type(&self, recno: Dno) -> Oid {
        let rectypeid = self.rec_typeid(recno);
        if rectypeid != RECORDOID {
            return rectypeid;
        }
        match &self.datums[recno as usize] {
            DatumVal::Rec(Some(rv)) => rv.src_desc.as_ref().map_or(RECORDOID, |td| td.tdtypeid),
            _ => RECORDOID,
        }
    }

    // exec_assign_value.
    pub(crate) fn exec_assign_value(
        &mut self,
        target: Dno,
        value: Datum,
        mut isnull: bool,
        valtype: Oid,
        valtypmod: i32,
    ) -> PgResult<()> {
        match &self.func.datums[target as usize] {
            PlDatum::Var(v) => {
                let (reqtype, reqtypmod, typlen, typbyval, typisarray, notnull) = (
                    v.datatype.typoid,
                    v.datatype.atttypmod,
                    v.datatype.typlen,
                    v.datatype.typbyval,
                    v.datatype.typisarray,
                    v.notnull,
                );
                let mut value = value;
                if !isnull && typisarray && valtype == reqtype && (valtypmod == reqtypmod || reqtypmod == -1) {
                    if let Some(obj) = arrayfuncs::adopt_transferred(value) {
                        self.set_var_expanded(target, obj);
                        return Ok(());
                    }
                }
                if !isnull && self.var_holds_rw(target, value) {
                    if valtype == reqtype && (valtypmod == reqtypmod || reqtypmod == -1) {
                        // Changed in place; the assignment itself is a no-op
                        // (pl_exec.c:5127-5131) apart from disarming a promise.
                        if let Some(pos) = self.promise_pending.iter().position(|&d| d == target) {
                            self.promise_pending.swap_remove(pos);
                        }
                        return Ok(());
                    }
                    // A cast reads the value like any other consumer.
                    value = self.get_var(target)?.0;
                }
                let newvalue =
                    self.exec_cast_value(value, &mut isnull, valtype, valtypmod, reqtype, reqtypmod)?;
                if isnull && notnull {
                    let refname = match &self.func.datums[target as usize] {
                        PlDatum::Var(v) => v.refname.as_str(),
                        _ => "",
                    };
                    return Err(exec_err(
                        types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                        format!(
                            "null value cannot be assigned to variable \"{refname}\" declared NOT NULL"
                        ),
                    ));
                }
                let stored = if typisarray && !isnull && !typbyval {
                    self.assign_array_to_datum_ctx(newvalue)?
                } else {
                    self.assign_copy_to_datum_ctx(newvalue, isnull, typlen, typbyval)?
                };
                self.set_var(target, stored, isnull, !isnull && !typbyval);
                Ok(())
            }
            PlDatum::RecField(f) => {
                let recno = f.recparentno;
                let recname = match &self.func.datums[recno as usize] {
                    PlDatum::Rec(r) => r.refname.clone(),
                    _ => String::new(),
                };
                // pl_exec.c:5215-5226: a NULL record with a named composite
                // type is instantiated empty before field assignment; only a
                // RECORD-typed rec errors (inside instantiate_empty_rec).
                if matches!(&self.datums[recno as usize], DatumVal::Rec(None)) {
                    self.instantiate_empty_rec(recno)?;
                }
                let DatumVal::Rec(Some(rv)) = &self.datums[recno as usize] else {
                    panic!("plpgsql exec_assign_value: rec \"{recname}\" valueless after instantiate");
                };
                let i = match rec_lookup_field_cached(
                    &mut self.recfield_idx,
                    f.dno,
                    &rv.desc,
                    &f.fieldname,
                ) {
                    Some(RecFieldRef::User(i)) => i,
                    Some(RecFieldRef::Sys(_)) => {
                        return Err(exec_err(
                            types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                            format!("cannot assign to system column \"{}\"", f.fieldname),
                        ));
                    }
                    None => {
                        return Err(exec_err(
                            types_error::ERRCODE_UNDEFINED_COLUMN,
                            format!("record \"{recname}\" has no field \"{}\"", f.fieldname),
                        ));
                    }
                };
                let (ftype, ftypmod, flen, fbyval) = (
                    rv.desc.types[i],
                    rv.desc.typmods[i],
                    rv.desc.typlens[i],
                    rv.desc.typbyvals[i],
                );
                let newvalue =
                    self.exec_cast_value(value, &mut isnull, valtype, valtypmod, ftype, ftypmod)?;
                let stored = self.assign_copy_to_datum_ctx(newvalue, isnull, flen, fbyval)?;
                // C expanded_record_set_field → check_domain_for_new_field:
                // the PROSPECTIVE tuple (current fields with the new value
                // swapped in) must satisfy the composite domain before the
                // field is committed; a violation leaves the rec unchanged.
                let rectypeid = self.rec_typeid(recno);
                if Self::rec_typeid_is_domain(rectypeid)? {
                    let (src, mut pv, mut pn) = match &self.datums[recno as usize] {
                        DatumVal::Rec(Some(rv)) => (
                            rv.src_desc.clone().expect("RecValue carries its source tupdesc"),
                            rv.values.clone(),
                            rv.nulls.clone(),
                        ),
                        _ => unreachable!("instantiated above"),
                    };
                    pv[i] = stored;
                    pn[i] = isnull;
                    if let Err(e) = self.rec_domain_check_fields(rectypeid, &src, &pv, &pn) {
                        if !isnull && !fbyval {
                            // SAFETY: `stored` is this call's own
                            // copy_to_datum_ctx copy, not yet installed.
                            unsafe { free_datum_copy(self.datum_ctx.mcx(), stored, flen) };
                        }
                        return Err(e);
                    }
                }
                let mcx = self.datum_ctx.mcx();
                if let DatumVal::Rec(Some(rv)) = &mut self.datums[recno as usize] {
                    // expanded_record_set_field_internal (expandedrecord.c):
                    // free the old field value if it is not part of the
                    // stored tuple — here, if this record made the copy.
                    if rv.owned[i] {
                        // SAFETY: owned marks a copy_to_datum_ctx copy held
                        // only by this field; `stored` was copied first.
                        unsafe { free_datum_copy(mcx, rv.values[i], flen) };
                    }
                    rv.values[i] = stored;
                    rv.nulls[i] = isnull;
                    rv.owned[i] = !isnull && !fbyval;
                    rv.empty = false;
                    rv.fvalue_valid = false;
                }
                Ok(())
            }
            PlDatum::Rec(r) => {
                if isnull {
                    // pl_exec.c:5178-5184: NOT NULL rec rejects the NULL
                    // before exec_move_row runs.
                    if r.notnull {
                        return Err(exec_err(
                            types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                            format!(
                                "null value cannot be assigned to variable \"{}\" declared NOT NULL",
                                r.refname
                            ),
                        ));
                    }
                    // C exec_move_row(rec, NULL, NULL) (pl_exec.c:6922-6945):
                    // domain → empty expanded record + domain_check; else
                    // erh = NULL. RECORD field access then 55000 via
                    // instantiate_empty_record_variable; a named composite
                    // instantiates empty on first field touch.
                    self.assign_rec_null(target)?;
                    return Ok(());
                }
                if valtype != RECORDOID && !lsyscache::typ::type_is_rowtype(valtype)? {
                    return Err(exec_err(
                        types_error::ERRCODE_DATATYPE_MISMATCH,
                        "cannot assign non-composite value to a record variable".to_string(),
                    ));
                }
                // C exec_move_row_from_datum, expanded-record source: r := r
                // is a no-op; a still-valid tuple travels when the target
                // takes it as is (declared RECORD, declared as the source's
                // rowtype, or already holding it); else no tuple is stored.
                let origin = self
                    .rec_origins
                    .iter()
                    .find(|(p, _)| *p == value.as_usize())
                    .map(|(_, o)| o.clone());
                if let Some(o) = &origin {
                    if o.recno == target {
                        return Ok(());
                    }
                    self.revalidate_rectypeid(target)?;
                }
                let (desc, src, values, nulls, sys) =
                    self.deconstruct_composite(value, self.eval_ctx.mcx())?;
                let sys = match origin {
                    Some(o) => {
                        let rectypeid = self.rec_typeid(target);
                        let held = match &self.datums[target as usize] {
                            DatumVal::Rec(Some(tv)) => tv.src_desc.as_ref().is_some_and(|td| {
                                td.tdtypeid == src.tdtypeid
                                    && (src.tdtypeid != RECORDOID
                                        || (src.tdtypmod == td.tdtypmod && src.tdtypmod >= 0))
                            }),
                            _ => false,
                        };
                        if o.fvalue_valid
                            && (rectypeid == RECORDOID || rectypeid == src.tdtypeid || held)
                        {
                            o.sys
                        } else {
                            None
                        }
                    }
                    None => Some(sys),
                };
                self.move_rec_from_values(target, &desc, src, &values, &nulls, sys, true, true)
            }
            PlDatum::Row(r) => {
                let varnos = r.varnos.clone();
                if isnull {
                    for dno in varnos {
                        self.exec_assign_value(dno, Datum::null(), true, UNKNOWNOID, -1)?;
                    }
                    return Ok(());
                }
                if valtype != RECORDOID && !lsyscache::typ::type_is_rowtype(valtype)? {
                    return Err(exec_err(
                        types_error::ERRCODE_DATATYPE_MISMATCH,
                        "cannot assign non-composite value to a row variable".to_string(),
                    ));
                }
                // C exec_move_row_from_datum -> exec_move_row_from_fields
                // (pl_exec.c:7796, 7181-7330): the ROW arm, including the
                // strict_multi_assignment check.
                let (desc, _src, values, nulls, _) =
                    self.deconstruct_composite(value, self.eval_ctx.mcx())?;
                self.move_row_fields_into_varnos(&varnos, &desc, |anum| (values[anum], nulls[anum]))
            }
        }
    }

    fn exec_stmt_fori(
        &mut self,
        label: Option<&str>,
        var: Dno,
        lower: &PlExpr,
        upper: &PlExpr,
        step: Option<&PlExpr>,
        reverse: bool,
        body: &'a [PlStmt],
    ) -> PgResult<i32> {
        let (vt, vm) = {
            let t = self.var_type(var);
            (t.typoid, t.atttypmod)
        };

        let (v, mut isnull, t, m) = self.exec_eval_expr(lower)?;
        let v = self.exec_cast_value(v, &mut isnull, t, m, vt, vm)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "lower bound of FOR loop cannot be null".to_string(),
            ));
        }
        let mut loop_value = v.as_i32();
        self.exec_eval_cleanup();

        let (v, mut isnull, t, m) = self.exec_eval_expr(upper)?;
        let v = self.exec_cast_value(v, &mut isnull, t, m, vt, vm)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "upper bound of FOR loop cannot be null".to_string(),
            ));
        }
        let end_value = v.as_i32();
        self.exec_eval_cleanup();

        let step_value = if let Some(sx) = step {
            let (v, mut isnull, t, m) = self.exec_eval_expr(sx)?;
            let v = self.exec_cast_value(v, &mut isnull, t, m, vt, vm)?;
            if isnull {
                return Err(exec_err(
                    types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                    "BY value of FOR loop cannot be null".to_string(),
                ));
            }
            self.exec_eval_cleanup();
            let sv = v.as_i32();
            if sv <= 0 {
                return Err(exec_err(
                    types_error::ERRCODE_INVALID_PARAMETER_VALUE,
                    "BY value of FOR loop must be greater than zero".to_string(),
                ));
            }
            sv
        } else {
            1
        };

        let mut found = false;
        let mut rc = RC_OK;
        loop {
            if reverse {
                if loop_value < end_value {
                    break;
                }
            } else if loop_value > end_value {
                break;
            }
            found = true;
            self.set_var(var, Datum::from_i32(loop_value), false, false);
            rc = self.exec_stmts(body)?;
            if let Some(r) = self.loop_rc(label, rc) {
                rc = r;
                if r != RC_OK {
                    self.exec_set_found_for(found);
                    return Ok(r);
                }
                break;
            }
            // C's LOOP_RC_PROCESSING (pl_exec.c): a consumed CONTINUE resets
            // rc to OK, so exhaustion right after a final-iteration CONTINUE
            // returns RC_OK instead of leaking a stale RC_CONTINUE.
            rc = RC_OK;
            // Increment with overflow guard (C checks bounds against i32).
            if reverse {
                match loop_value.checked_sub(step_value) {
                    Some(nv) => loop_value = nv,
                    None => break,
                }
            } else {
                match loop_value.checked_add(step_value) {
                    Some(nv) => loop_value = nv,
                    None => break,
                }
            }
        }
        self.exec_set_found_for(found);
        Ok(rc)
    }

    fn exec_set_found_for(&mut self, found: bool) {
        self.exec_set_found(found);
    }

    // exec_stmt_fors + exec_for_query over SPI cursors.
    fn exec_stmt_fors(
        &mut self,
        label: Option<&str>,
        var: Dno,
        query: &PlExpr,
        body: &'a [PlStmt],
    ) -> PgResult<i32> {
        // C exec_run_select (pl_exec.c:5770): portal-returning runs prepare
        // with CURSOR_OPT_NO_SCROLL and WITHOUT CURSOR_OPT_PARALLEL_OK —
        // intra-loop user code makes parallel unsafe, and NO_SCROLL pins the
        // portal so spi.c's default-scrollability probe never auto-upgrades a
        // FOR-loop portal to SCROLL (which would arm the wave-10 cursor store
        // and its per-row fill program for a strictly forward internal loop —
        // the SE11 B1 +18.15% tax).
        self.ensure_plan(query, CURSOR_OPT_NO_SCROLL)?;
        let (plan, paramnos, argtypes) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&query.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone())
        });
        let params = self.setup_param_list_under_spi(query, &paramnos, &argtypes, false)?;
        spi_frame!(frame, &query.query, query.parse_mode);
        let _live = LiveSpiExpr::set((self as *mut Estate<'a>).cast(), query);
        let cursor = SPI_cursor_open_with_paramlist(None, plan, &params, self.readonly_func)?;
        drop(frame);

        let result = self.exec_for_query(label, var, &cursor, body, true);

        // Close only on success (C longjmps past it): a failed intra-loop
        // COMMIT/ROLLBACK already dropped the portal in its abort arm.
        match result {
            Ok(rc) => {
                SPI_cursor_close(cursor)?;
                Ok(rc)
            }
            Err(e) => Err(e),
        }
    }

    fn exec_for_query(
        &mut self,
        label: Option<&str>,
        var: Dno,
        cursor: &SpiCursor,
        body: &'a [PlStmt],
        prefetch_ok: bool,
    ) -> PgResult<i32> {
        let mut found = false;
        let mut rc = RC_OK;
        let prefetch_ok = prefetch_ok && self.atomic;
        // C's previous_id / tupdescs_match (pl_exec.c:5844-5845).
        let mut previous: Option<std::rc::Rc<types_tuple::TupleDescData<'static>>> = None;
        let mut tupdescs_match = true;

        // C pins the loop portal so an intra-loop COMMIT/ROLLBACK converts it
        // to held (HoldPinnedPortals) instead of dropping it. On error the
        // pin is cleared by AtCleanup_Portals, as in C.
        portalmem::PinPortal(&cursor.portal)?;
        SPI_cursor_fetch(cursor, true, if prefetch_ok { 10 } else { 1 })?;
        let mut tuptab = spi::SPI_tuptable();
        let mut n = spi::SPI_processed();

        if n == 0 {
            if let Some(t) = tuptab {
                self.move_row_null(var, t)?;
            }
            self.exec_eval_cleanup();
        } else {
            found = true;
        }

        'outer: while n > 0 {
            let t = tuptab.expect("fetch returned rows");
            for i in 0..n as usize {
                self.move_loop_row(var, t, i, &mut previous, &mut tupdescs_match)?;
                self.exec_eval_cleanup();
                rc = self.exec_stmts(body)?;
                match self.loop_rc(label, rc) {
                    None => {}
                    Some(r) => {
                        rc = r;
                        break 'outer;
                    }
                }
                rc = RC_OK;
            }
            let _ = spi::SPI_freetuptable(t);
            SPI_cursor_fetch(cursor, true, if prefetch_ok { 50 } else { 1 })?;
            tuptab = spi::SPI_tuptable();
            n = spi::SPI_processed();
        }
        if let Some(t) = tuptab {
            let _ = spi::SPI_freetuptable(t);
        }

        self.exec_set_found(found);
        portalmem::UnpinPortal(&cursor.portal)?;
        Ok(rc)
    }

    // exec_for_query's per-row assignment (pl_exec.c:5905-5945): once the
    // loop record holds a tupdesc (previous_id) that the query's rows fit
    // (tupdescs_match), each row only swaps the tuple into the record. A
    // body assignment that replaced the record's descriptor, a NULLed
    // record, a row target, or a rowtype needing coercion takes the full
    // exec_move_row path, after which the match is re-tested (once false it
    // stays false) and the record's descriptor becomes the new identity.
    fn move_loop_row(
        &mut self,
        var: Dno,
        tuptab: TuptabHandle,
        i: usize,
        previous: &mut Option<std::rc::Rc<types_tuple::TupleDescData<'static>>>,
        tupdescs_match: &mut bool,
    ) -> PgResult<()> {
        if !matches!(&self.func.datums[var as usize], PlDatum::Rec(_)) {
            return self.move_row_from_tuptable(var, tuptab, i);
        }
        let held = match &self.datums[var as usize] {
            DatumVal::Rec(Some(rv)) => rv.src_desc.clone(),
            _ => None,
        };
        if let (Some(prev), Some(cur)) = (previous.as_ref(), held.as_ref()) {
            if std::rc::Rc::ptr_eq(prev, cur) && *tupdescs_match {
                return self.rec_set_tuple_from_tuptable(var, tuptab, i);
            }
        }
        self.move_row_from_tuptable(var, tuptab, i)?;
        let src_desc = match &self.datums[var as usize] {
            DatumVal::Rec(Some(rv)) => rv.src_desc.clone(),
            _ => None,
        };
        if *tupdescs_match {
            let rectypeid = self.rec_typeid(var);
            *tupdescs_match = rectypeid == RECORDOID
                || spi::tuptable_with(tuptab, |t| {
                    rectypeid == t.tupdesc.tdtypeid
                        || src_desc
                            .as_ref()
                            .is_some_and(|d| compatible_tupdescs(&t.tupdesc, d))
                });
        }
        *previous = src_desc;
        Ok(())
    }

    // expanded_record_set_tuple (expandedrecord.c) for the loop's tuple-swap
    // arm: deconstruct the row against the record's own descriptor (the rows
    // fit it — tupdescs_match), copy the by-ref fields into the function's
    // context, domain-check a composite-domain record on the prospective
    // fields, then swap the images in and free the previous ones. The
    // descriptor, its RecDesc and the tupdesc copy are reused untouched.
    fn rec_set_tuple_from_tuptable(
        &mut self,
        recno: Dno,
        tuptab: TuptabHandle,
        i: usize,
    ) -> PgResult<()> {
        let natts = match &self.datums[recno as usize] {
            DatumVal::Rec(Some(rv)) => rv.values.len(),
            _ => unreachable!("tuple swap on a valueless record"),
        };
        let mut values = ::mcx::std_vec_from_elem("plpgsql", Datum::null(), natts)?;
        let mut nulls = ::mcx::std_vec_from_elem("plpgsql", true, natts)?;
        let sys = spi::tuptable_with(tuptab, |t| {
            for f in 0..natts {
                let (v, isnull) = spi::SPI_getbinval(&t.vals[i], &t.tupdesc, (f + 1) as i32);
                values[f] = v;
                nulls[f] = isnull;
            }
            std::rc::Rc::new(RecSysAttrs::from_tuple(&t.vals[i]))
        });
        let mut owned = ::mcx::std_vec_from_elem("plpgsql", false, natts)?;
        {
            let DatumVal::Rec(Some(rv)) = &self.datums[recno as usize] else {
                unreachable!("checked above")
            };
            for f in 0..natts {
                if !rv.desc.dropped[f] {
                    values[f] = self.assign_copy_to_datum_ctx(
                        values[f],
                        nulls[f],
                        rv.desc.typlens[f],
                        rv.desc.typbyvals[f],
                    )?;
                    owned[f] = !nulls[f] && !rv.desc.typbyvals[f];
                }
            }
        }
        // check_domain_for_new_tuple: the prospective fields must satisfy
        // a composite domain before they replace the current ones.
        let rectypeid = self.rec_typeid(recno);
        if Self::rec_base_typeid(rectypeid)? != rectypeid {
            let td = match &self.datums[recno as usize] {
                DatumVal::Rec(Some(rv)) => {
                    rv.src_desc.clone().expect("RecValue carries its source tupdesc")
                }
                _ => unreachable!("checked above"),
            };
            if let Err(e) = self.rec_domain_check_fields(rectypeid, &td, &values, &nulls) {
                let mcx = self.datum_ctx.mcx();
                if let DatumVal::Rec(Some(rv)) = &self.datums[recno as usize] {
                    for f in 0..natts {
                        if owned[f] {
                            // SAFETY: fresh copies above, held by nobody.
                            unsafe { free_datum_copy(mcx, values[f], rv.desc.typlens[f]) };
                        }
                    }
                }
                return Err(e);
            }
        }
        let mcx = self.datum_ctx.mcx();
        if let DatumVal::Rec(Some(rv)) = &mut self.datums[recno as usize] {
            for f in 0..natts {
                if rv.owned[f] {
                    // SAFETY: owned marks a copy_to_datum_ctx copy held only
                    // by this record; the new images were copied first.
                    unsafe { free_datum_copy(mcx, rv.values[f], rv.desc.typlens[f]) };
                }
            }
            rv.values = values;
            rv.nulls = nulls;
            rv.owned = owned;
            rv.sys = Some(sys);
            rv.fvalue_valid = true;
            rv.empty = false;
        }
        Ok(())
    }

    fn rec_desc_of(&self, tuptab: TuptabHandle) -> PgResult<(RecDesc, std::rc::Rc<types_tuple::TupleDescData<'static>>)> {
        let td = spi::tuptable_with(tuptab, |t| {
            tupdesc::CreateTupleDescCopy(self.datum_ctx.mcx(), &t.tupdesc)
        })?;
        Ok((RecDesc::from_tupdesc(&td)?, std::rc::Rc::new(td)))
    }

    // make_expanded_record_from_typeid (pl_exec.c:7141 -> expandedrecord.c):
    // the declared type's CURRENT typcache descriptor, pinned by refcount,
    // never a descriptor kept from an earlier assignment (ALTER TYPE in
    // between would otherwise coerce into the old attribute types).
    fn declared_rec_tupdesc(&self, base: Oid) -> PgResult<std::rc::Rc<types_tuple::TupleDescData<'static>>> {
        let entry = typcache::lookup_type_cache(base, typcache::TYPECACHE_TUPDESC)?;
        match entry.tupdesc() {
            Some(td) => Ok(td),
            None => Ok(std::rc::Rc::new(typcache::lookup_rowtype_tupdesc_copy(
                self.datum_ctx.mcx(),
                base,
                -1,
            )?)),
        }
    }

    // exec_move_row with a NULL source tuple.
    fn move_row_null(&mut self, var: Dno, tuptab: TuptabHandle) -> PgResult<()> {
        match &self.func.datums[var as usize] {
            PlDatum::Rec(_) if self.rec_typeid(var) != RECORDOID => {
                // deconstruct_expanded_record on the fresh expanded record
                // (pl_exec.c:6969): a row of NULLs over the declared type,
                // no per-field coercion, no domain check.
                self.revalidate_rectypeid(var)?;
                let base = Self::rec_base_typeid(self.rec_typeid(var))?;
                let td = self.declared_rec_tupdesc(base)?;
                let desc = RecDesc::from_tupdesc(&td)?;
                let n = desc.types.len();
                self.assign_record_var(
                    var,
                    Some(RecValue {
                        desc,
                        values: ::mcx::std_vec_from_elem("plpgsql", Datum::null(), n)?,
                        nulls: ::mcx::std_vec_from_elem("plpgsql", true, n)?,
                        src_desc: Some(td),
                        empty: false,
                        sys: None,
                        fvalue_valid: false,
                        owned: ::mcx::std_vec_from_elem("plpgsql", false, n)?,
                    }),
                );
                Ok(())
            }
            PlDatum::Rec(_) => {
                let (desc, src_desc) = self.rec_desc_of(tuptab)?;
                let n = desc.types.len();
                // C's NULL-tuple arm (exec_move_row !HeapTupleIsValid):
                // deconstruct_expanded_record makes a row of NULLs with no
                // strict_multi_assignment and NO composite-domain check.
                self.move_rec_from_values(
                    var,
                    &desc,
                    src_desc,
                    &::mcx::std_vec_from_elem("plpgsql", Datum::null(), n)?,
                    &::mcx::std_vec_from_elem("plpgsql", true, n)?,
                    None,
                    false,
                    false,
                )
            }
            PlDatum::Row(r) => {
                let varnos = r.varnos.clone();
                for dno in varnos {
                    self.exec_assign_value(dno, Datum::null(), true, UNKNOWNOID, -1)?;
                }
                Ok(())
            }
            _ => panic!("plpgsql exec_move_row: bad target datum {var}"),
        }
    }

    // exec_move_row from tuptable row i.
    fn move_row_from_tuptable(&mut self, var: Dno, tuptab: TuptabHandle, i: usize) -> PgResult<()> {
        match &self.func.datums[var as usize] {
            PlDatum::Rec(_) => {
                let (desc, src_desc) = self.rec_desc_of(tuptab)?;
                let natts = desc.types.len();
                let mut values = ::mcx::std_vec_from_elem("plpgsql", Datum::null(), natts)?;
                let mut nulls = ::mcx::std_vec_from_elem("plpgsql", true, natts)?;
                let sys = spi::tuptable_with(tuptab, |t| {
                    for f in 0..natts {
                        let (v, isnull) =
                            spi::SPI_getbinval(&t.vals[i], &t.tupdesc, (f + 1) as i32);
                        values[f] = v;
                        nulls[f] = isnull;
                    }
                    std::rc::Rc::new(RecSysAttrs::from_tuple(&t.vals[i]))
                });
                self.move_rec_from_values(var, &desc, src_desc, &values, &nulls, Some(sys), true, true)
            }
            PlDatum::Row(r) => {
                let varnos = r.varnos.clone();
                self.move_row_into_varnos(&varnos, tuptab, i)
            }
            _ => panic!("plpgsql exec_move_row: bad target datum {var}"),
        }
    }

    // The strict_multi_assignment report (pl_exec.c:7286-7297); Err only at
    // ERROR level, WARNING is emitted and execution continues.
    fn strict_multiassignment_report(
        &self,
        level: types_error::ErrorLevel,
    ) -> PgResult<()> {
        let b = elog::ereport(level)
            .errcode(types_error::ERRCODE_DATATYPE_MISMATCH)
            .errmsg("number of source and target fields in assignment does not match")
            .errdetail(format!(
                "strict_multi_assignment check of {} is active.",
                if level == ERROR { "extra_errors" } else { "extra_warnings" }
            ))
            .errhint("Make sure the query returns the exact list of columns.");
        if level == ERROR {
            return Err(Box::new(b.into_error()));
        }
        b.finish(types_error::ErrorLocation::new(file!(), line!() as i32, "exec_move_row_from_fields"))
    }

    // exec_move_row_from_fields REC-target arm: a RECORD rec adopts the
    // source shape; a named-composite rec coerces field-by-field onto its
    // declared tupdesc (source read positionally, dropped columns skipped on
    // both sides, missing sources become NULL).
    fn move_rec_from_values(
        &mut self,
        recno: Dno,
        srcdesc: &RecDesc,
        src_tupdesc: std::rc::Rc<types_tuple::TupleDescData<'static>>,
        values: &[Datum],
        nulls: &[bool],
        // Kept only where C stores the tuple itself rather than its fields.
        sys: Option<std::rc::Rc<RecSysAttrs>>,
        sma_check: bool,
        // C: assignments through expanded_record_set_fields/set_tuple run
        // check_domain_on_current_fields; the row-of-NULLs arm (source
        // tupdesc but no tuple: INTO with no rows, FOR over zero rows) goes
        // through deconstruct_expanded_record instead and is NOT checked.
        domain_check: bool,
    ) -> PgResult<()> {
        let rectypeid = self.rec_typeid(recno);
        if rectypeid == RECORDOID {
            let natts = srcdesc.types.len();
            let mut out_values = ::mcx::std_vec_from_slice("plpgsql", &values)?;
            for f in 0..natts {
                if !srcdesc.dropped[f] {
                    out_values[f] = self.assign_copy_to_datum_ctx(
                        out_values[f],
                        nulls[f],
                        srcdesc.typlens[f],
                        srcdesc.typbyvals[f],
                    )?;
                }
            }
            let owned = (0..natts)
                .map(|f| !srcdesc.dropped[f] && !nulls[f] && !srcdesc.typbyvals[f])
                .collect();
            self.assign_record_var(
                recno,
                Some(RecValue {
                    desc: srcdesc.clone(),
                    values: out_values,
                    nulls: ::mcx::std_vec_from_slice("plpgsql", &nulls)?,
                    src_desc: Some(src_tupdesc),
                    empty: false,
                    fvalue_valid: sys.is_some(),
                    sys,
                    owned,
                }),
            );
            return Ok(());
        }

        self.revalidate_rectypeid(recno)?;
        let rectypeid = self.rec_typeid(recno);
        let base = Self::rec_base_typeid(rectypeid)?;
        let var_td = self.declared_rec_tupdesc(base)?;
        let dst = RecDesc::from_tupdesc(&var_td)?;
        let sys = if rectypeid == src_tupdesc.tdtypeid || compatible_tupdescs(&src_tupdesc, &var_td) {
            sys
        } else {
            None
        };
        let vtd_natts = dst.types.len();
        let td_natts = srcdesc.types.len();
        // strict_multi_assignment reads the GUCs at execution
        // (pl_exec.c:7196-7202); active only with a source tupdesc, which
        // this arm always has.
        let sma_level = if sma_check {
            crate::handler::extra_checks_level(crate::comp::XCHECK_STRICTMULTIASSIGNMENT)?
        } else {
            None
        };
        let mut newvalues = ::mcx::std_vec_from_elem("plpgsql", Datum::null(), vtd_natts)?;
        let mut newnulls = ::mcx::std_vec_from_elem("plpgsql", true, vtd_natts)?;
        let mut anum = 0usize;
        for fnum in 0..vtd_natts {
            if dst.dropped[fnum] {
                continue;
            }
            while anum < td_natts && srcdesc.dropped[anum] {
                anum += 1;
            }
            let (value, mut isnull, valtype, valtypmod) = if anum < td_natts {
                let r = (values[anum], nulls[anum], srcdesc.types[anum], srcdesc.typmods[anum]);
                anum += 1;
                r
            } else {
                if let Some(level) = sma_level {
                    self.strict_multiassignment_report(level)?;
                }
                (Datum::null(), true, UNKNOWNOID, -1)
            };
            let cast = self.exec_cast_value(
                value,
                &mut isnull,
                valtype,
                valtypmod,
                dst.types[fnum],
                dst.typmods[fnum],
            )?;
            // A cast result can live in its step's retained scratch, which the
            // next field cast through the same entry overwrites; C keeps every
            // field's result alive in eval_mcontext until the record is built.
            newvalues[fnum] = if !isnull && !dst.typbyvals[fnum] && cast.as_usize() != value.as_usize() {
                // SAFETY: a live non-null by-ref datum of the target column's typlen.
                unsafe { execexpr::agg_datum_copy(self.eval_ctx.mcx(), cast, dst.typlens[fnum])? }
            } else {
                cast
            };
            newnulls[fnum] = isnull;
        }
        // Unassigned source attributes, dropped columns skipped
        // (pl_exec.c:7311-7331).
        if let Some(level) = sma_level {
            while anum < td_natts && srcdesc.dropped[anum] {
                anum += 1;
            }
            if anum < td_natts {
                self.strict_multiassignment_report(level)?;
            }
        }
        // Domain check BEFORE committing: C builds/checks the new expanded
        // record first and only then assign_record_var's it, so a violation
        // leaves the variable's old value intact.
        if domain_check && base != rectypeid {
            self.rec_domain_check_fields(rectypeid, &var_td, &newvalues, &newnulls)?;
        }
        for fnum in 0..vtd_natts {
            if !dst.dropped[fnum] {
                newvalues[fnum] = self.assign_copy_to_datum_ctx(
                    newvalues[fnum],
                    newnulls[fnum],
                    dst.typlens[fnum],
                    dst.typbyvals[fnum],
                )?;
            }
        }
        let owned = (0..vtd_natts)
            .map(|f| !dst.dropped[f] && !newnulls[f] && !dst.typbyvals[f])
            .collect();
        self.assign_record_var(
            recno,
            Some(RecValue {
                desc: dst,
                values: newvalues,
                nulls: newnulls,
                src_desc: Some(var_td),
                empty: false,
                fvalue_valid: sys.is_some(),
                sys,
                owned,
            }),
        );
        Ok(())
    }

    // make_tuple_from_row (pl_exec.c:7491) shaped as a RecValue.
    // make_tuple_from_row (pl_exec.c:7492): `target` is the tupdesc the
    // tuple is formed against (the row's own when None); None when a
    // member's exec_eval_datum type differs from the target column's.
    fn row_as_rec_value(
        &mut self,
        rowno: Dno,
        scratch: bool,
        target: Option<&RecDesc>,
    ) -> PgResult<Option<RecValue>> {
        let (varnos, fieldnames) = match &self.func.datums[rowno as usize] {
            PlDatum::Row(r) => (r.varnos.clone(), r.fieldnames.clone()),
            _ => panic!("plpgsql: datum {rowno} is not a Row"),
        };
        let n = varnos.len();
        if target.is_some_and(|t| t.types.len() != n) {
            return Ok(None);
        }
        let mcx = if scratch { self.eval_ctx.mcx() } else { self.datum_ctx.mcx() };
        let mut td = tupdesc::CreateTemplateTupleDesc(mcx, n as i32)?;
        let mut values = ::mcx::std_vec_with_capacity("plpgsql", n)?;
        let mut nulls = ::mcx::std_vec_with_capacity("plpgsql", n)?;
        for (i, &dno) in varnos.iter().enumerate() {
            let (typoid, typmod, current) = match &self.func.datums[dno as usize] {
                PlDatum::Var(_) => {
                    let t = self.var_type(dno);
                    (t.typoid, t.atttypmod, t.typoid)
                }
                PlDatum::Rec(r) => (self.rec_typeid(r.dno), -1, self.rec_param_type_mod(r.dno)?.0),
                _ => panic!("plpgsql row member {dno} is not a Var or Rec"),
            };
            let expected = target.map_or(typoid, |t| t.types[i]);
            if current != expected && !target.is_some_and(|t| t.dropped[i]) {
                return Ok(None);
            }
            tupdesc::TupleDescInitEntry(
                &mut td,
                (i + 1) as i16,
                Some(&fieldnames[i]),
                typoid,
                typmod,
                0,
            )?;
            let (v, isnull) = self.datum_as_param(dno, None)?;
            values.push(v);
            nulls.push(isnull);
        }
        td.tdtypeid = RECORDOID;
        td.tdtypmod = -1;
        let desc = RecDesc::from_tupdesc(&td)?;
        if !scratch {
            for i in 0..n {
                values[i] =
                    self.copy_to_datum_ctx(values[i], nulls[i], desc.typlens[i], desc.typbyvals[i])?;
            }
        }
        let owned = (0..n).map(|i| !scratch && !nulls[i] && !desc.typbyvals[i]).collect();
        Ok(Some(RecValue {
            desc,
            values,
            nulls,
            src_desc: Some(std::rc::Rc::new(td)),
            empty: false,
            sys: None,
            fvalue_valid: false,
            owned,
        }))
    }

    // deconstruct_composite_datum (pl_exec.c:7546) — plain composite Datum;
    // the identity is C's tmptup (no ctid, no tableoid).
    #[allow(clippy::type_complexity)]
    pub(crate) fn deconstruct_composite(
        &mut self,
        value: Datum,
        image_mcx: Mcx<'_>,
    ) -> PgResult<(
        RecDesc,
        std::rc::Rc<types_tuple::TupleDescData<'static>>,
        Vec<Datum>,
        Vec<bool>,
        std::rc::Rc<RecSysAttrs>,
    )> {
        let mcx = self.datum_ctx.mcx();
        let p = value.as_usize() as *const u8;
        // C DatumGetHeapTupleHeader: detoast first — the datum can arrive
        // short-headed (e.g. deformed back out of a materialized minimal
        // tuple), compressed, or expanded; only the detoasted 4-byte-header
        // form is a readable HeapTupleHeader image.
        // SAFETY: non-null composite datum — a live varlena-headed image
        // readable for its varsize.
        let raw = unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) };
        // The deformed byref values below point into this image: callers
        // that copy the fields out pass the eval scratch (C detoasts in
        // eval_mcontext, pl_exec.c:7724), the rest the datum context.
        let rec = detoast::detoast_attr(image_mcx, raw)?.leak();
        // SAFETY: detoasted composite image; header prefix is in bounds.
        let td_hdr = unsafe { &*(rec.as_ptr() as *const types_tuple::HeapTupleHeaderData) };
        let tup_type = td_hdr.type_id();
        let tup_typmod = td_hdr.typmod();
        let t_len = td_hdr.datum_length();
        let tupdesc = typcache::lookup_rowtype_tupdesc_copy(mcx, tup_type, tup_typmod)?;
        let desc = RecDesc::from_tupdesc(&tupdesc)?;
        let natts = desc.types.len();
        let mut values = ::mcx::std_vec_from_elem("plpgsql", Datum::null(), natts)?;
        let mut nulls = ::mcx::std_vec_from_elem("plpgsql", true, natts)?;
        // SAFETY: MAXALIGN'd detoasted image of datum_length() bytes.
        let htd = unsafe {
            types_tuple::HeapTupleData::from_raw_parts(
                rec.as_ptr(),
                t_len,
                types_tuple::ItemPointerData::invalid(),
                types_core::InvalidOid,
            )
        };
        types_tuple::heap_deform_tuple(&htd, &tupdesc, &mut values, &mut nulls);
        let sys = std::rc::Rc::new(RecSysAttrs::from_tuple(&htd));
        Ok((desc, std::rc::Rc::new(tupdesc), values, nulls, sys))
    }

    fn exec_stmt_return(&mut self, expr: Option<&PlExpr>, retvarno: Dno) -> PgResult<()> {
        if self.func.fn_retset {
            return Ok(());
        }
        if retvarno >= 0 {
            match &self.func.datums[retvarno as usize] {
                PlDatum::Var(v) => {
                    if self.caller_adopts
                        && v.datatype.typisarray
                        && v.datatype.typoid == self.func.fn_rettype
                    {
                        if let DatumVal::Var { isnull: false, exp: exp @ Some(_), .. } =
                            &mut self.datums[retvarno as usize]
                        {
                            let obj = exp.take().expect("matched Some");
                            self.retval = obj.rw_datum();
                            self.ret_expanded = Some(obj);
                            self.retisnull = false;
                            self.rettype = v.datatype.typoid;
                            return Ok(());
                        }
                    }
                    let (value, isnull) = self.get_var(retvarno)?;
                    self.retval = value;
                    self.retisnull = isnull;
                    self.rettype = v.datatype.typoid;
                    if self.func.fn_retistuple && !self.retisnull {
                        return Err(exec_err(
                            types_error::ERRCODE_DATATYPE_MISMATCH,
                            "cannot return non-composite value from function returning composite type"
                                .to_string(),
                        ));
                    }
                }
                PlDatum::Rec(_) => {
                    let rectypeid = self.rec_typeid(retvarno);
                    if !self.func.fn_retistuple && !self.func.fn_retset {
                        // C keeps retval a composite Datum (ExpandedRecordGetDatum);
                        // scalar-returning functions IO-cast it at exit, so the
                        // ret_rec path must not leave retval unset (null deref in
                        // record_out).
                        let (d, isnull) = self.rec_as_composite_datum(retvarno)?;
                        // exec_eval_datum REC arm: a RECORD-declared rec
                        // reports the held value's rowtype (erh->er_typeid),
                        // so the exit cast can find a cast from it.
                        let (rettype, _) = self.rec_param_type_mod(retvarno)?;
                        self.ret_rec = None;
                        self.retval = d;
                        self.retisnull = isnull;
                        self.rettype = rettype;
                        return Ok(());
                    }
                    match &self.datums[retvarno as usize] {
                        DatumVal::Rec(Some(rv)) if !rv.empty => {
                            self.ret_rec = Some(rv.try_clone()?);
                            self.retisnull = false;
                            self.rettype =
                                if rectypeid != RECORDOID { rectypeid } else { RECORDOID };
                        }
                        _ => {
                            self.ret_rec = None;
                            self.retisnull = true;
                            self.rettype = rectypeid;
                        }
                    }
                }
                PlDatum::Row(row) => {
                    // exec_eval_datum ROW arm: rows materialize through their
                    // member variables (multiple OUT parameters).
                    let rv = self.row_as_rec_value(row.dno, false, None)?.ok_or_else(|| {
                        exec_err(
                            types_error::ERRCODE_INTERNAL_ERROR,
                            "row not compatible with its own tupdesc".to_string(),
                        )
                    })?;
                    self.ret_rec = Some(rv);
                    self.retisnull = false;
                    self.rettype = RECORDOID;
                }
                _ => panic!("plpgsql: bad retvarno"),
            }
            return Ok(());
        }
        if let Some(expr) = expr {
            let (value, isnull, rettype, _typmod) = self.exec_eval_expr(expr)?;
            self.retval = value;
            self.retisnull = isnull;
            self.rettype = rettype;
            if self.func.fn_retistuple
                && !isnull
                && !lsyscache::typ::type_is_rowtype(rettype)?
            {
                return Err(exec_err(
                    types_error::ERRCODE_DATATYPE_MISMATCH,
                    "cannot return non-composite value from function returning composite type"
                        .to_string(),
                ));
            }
            // No exec_eval_cleanup: the value must survive to function exit
            // (nothing runs after RC_RETURN).
            return Ok(());
        }
        // RETURN without expr in a void function (or procedure). C returns a
        // non-null VOID datum for functions (pl_exec.c:3303-3314); procedures
        // stay null.
        const VOIDOID: Oid = 2278;
        const PROKIND_PROCEDURE: i8 = b'p' as i8;
        if self.func.fn_rettype == VOIDOID && self.func.fn_prokind != PROKIND_PROCEDURE {
            self.retval = Datum::from_usize(0);
            self.retisnull = false;
            self.rettype = VOIDOID;
        } else {
            self.retval = Datum::null();
            self.retisnull = true;
            self.rettype = types_core::InvalidOid;
        }
        Ok(())
    }

    fn exec_stmt_raise(&mut self, stmt: &PlStmt) -> PgResult<i32> {
        let PlStmt::Raise { elog_level, condname, message, params, options, .. } = stmt else {
            unreachable!()
        };

        if condname.is_none() && message.is_none() && options.is_empty() {
            // Bare RAISE: re-throw the active handler's error unchanged.
            if let Some(err) = &self.cur_error {
                return Err(err.clone());
            }
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER)
                    .errmsg("RAISE without parameters cannot be used outside an exception handler")
                    .into_error(),
            ));
        }

        // C keeps err_code as an int where MAKE_SQLSTATE('0','0','0','0','0')
        // == 0 is "unset": `if (err_code)` at pl_exec.c:3850 lets a later
        // USING ERRCODE override SQLSTATE '00000' and pl_exec.c:3890 defaults
        // it to ERRCODE_RAISE_EXCEPTION. None here models exactly that 0.
        let mut err_code: Option<SqlState> = None;
        let mut cond: Option<String> = None;
        if let Some(cn) = condname {
            err_code = raise_err_code(recognize_err_condition(cn)?);
            cond = Some(cn.clone());
        }

        let mut err_message: Option<String> = None;
        if let Some(msg) = message {
            let mut ds = String::new();
            let mut pi = 0usize;
            // C pl_exec.c:3774-3799 walks the message byte-wise; '%' cannot
            // occur inside a multibyte sequence, so a char walk is
            // equivalent — and unlike the old byte-as-char loop it does not
            // Latin-1-widen multibyte text ("café" became "cafÃ©", which
            // the client-encoding conversion then faithfully mojibake'd).
            let mut it = msg.chars().peekable();
            while let Some(c) = it.next() {
                if c == '%' {
                    if it.peek() == Some(&'%') {
                        it.next();
                        ds.push('%');
                        continue;
                    }
                    // C: "should have been checked at compile time"
                    // (pl_exec.c:3794-3796).
                    let Some(p) = params.get(pi) else {
                        return Err(exec_err(
                            types_error::ERRCODE_INTERNAL_ERROR,
                            "unexpected RAISE parameter list length".to_string(),
                        ));
                    };
                    pi += 1;
                    let (v, isnull, t, _m) = self.exec_eval_expr(p)?;
                    if isnull {
                        ds.push_str("<NULL>");
                    } else {
                        ds.push_str(&self.convert_value_to_string(v, t)?);
                    }
                    self.exec_eval_cleanup();
                } else {
                    ds.push(c);
                }
            }
            err_message = Some(ds);
        }

        let mut err_detail: Option<String> = None;
        let mut err_hint: Option<String> = None;
        let mut err_column: Option<String> = None;
        let mut err_constraint: Option<String> = None;
        let mut err_datatype: Option<String> = None;
        let mut err_table: Option<String> = None;
        let mut err_schema: Option<String> = None;
        for opt in options {
            let (v, isnull, t, _m) = self.exec_eval_expr(&opt.expr)?;
            if isnull {
                return Err(exec_err(
                    types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                    "RAISE statement option cannot be null".to_string(),
                ));
            }
            let extval = self.convert_value_to_string(v, t)?;
            self.exec_eval_cleanup();
            let dup = |name: &str| -> Box<PgError> {
                exec_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    format!("RAISE option already specified: {name}"),
                )
            };
            match opt.opt_type {
                PLPGSQL_RAISEOPTION_ERRCODE => {
                    if err_code.is_some() {
                        return Err(dup("ERRCODE"));
                    }
                    err_code = raise_err_code(recognize_err_condition(&extval)?);
                    cond = Some(extval);
                }
                PLPGSQL_RAISEOPTION_MESSAGE => {
                    if err_message.is_some() {
                        return Err(dup("MESSAGE"));
                    }
                    err_message = Some(extval);
                }
                PLPGSQL_RAISEOPTION_DETAIL => {
                    if err_detail.is_some() {
                        return Err(dup("DETAIL"));
                    }
                    err_detail = Some(extval);
                }
                PLPGSQL_RAISEOPTION_HINT => {
                    if err_hint.is_some() {
                        return Err(dup("HINT"));
                    }
                    err_hint = Some(extval);
                }
                PLPGSQL_RAISEOPTION_COLUMN => {
                    if err_column.is_some() {
                        return Err(dup("COLUMN"));
                    }
                    err_column = Some(extval);
                }
                PLPGSQL_RAISEOPTION_CONSTRAINT => {
                    if err_constraint.is_some() {
                        return Err(dup("CONSTRAINT"));
                    }
                    err_constraint = Some(extval);
                }
                PLPGSQL_RAISEOPTION_DATATYPE => {
                    if err_datatype.is_some() {
                        return Err(dup("DATATYPE"));
                    }
                    err_datatype = Some(extval);
                }
                PLPGSQL_RAISEOPTION_TABLE => {
                    if err_table.is_some() {
                        return Err(dup("TABLE"));
                    }
                    err_table = Some(extval);
                }
                PLPGSQL_RAISEOPTION_SCHEMA => {
                    if err_schema.is_some() {
                        return Err(dup("SCHEMA"));
                    }
                    err_schema = Some(extval);
                }
                _ => panic!("unrecognized raise option: {}", opt.opt_type),
            }
        }

        if err_code.is_none() && *elog_level >= crate::gram::ELOG_ERROR {
            err_code = Some(types_error::ERRCODE_RAISE_EXCEPTION);
        }
        let err_message = match err_message {
            Some(m) => m,
            None => match cond.take() {
                Some(c) => c,
                // C: unpack_sql_state(err_code) with err_code 0 = "00000".
                None => match err_code {
                    Some(code) => unpack_sql_state(code),
                    None => "00000".to_string(),
                },
            },
        };

        if *elog_level >= crate::gram::ELOG_ERROR {
            let mut b = elog::ereport(ERROR).errverbatim().errmsg_internal(err_message);
            if let Some(c) = err_code {
                b = b.errcode(c);
            }
            if let Some(d) = err_detail {
                b = b.errdetail_internal(d);
            }
            if let Some(h) = err_hint {
                b = b.errhint(h);
            }
            let mut e = b.into_error();
            set_raise_fields(
                &mut e,
                err_column,
                err_constraint,
                err_datatype,
                err_table,
                err_schema,
            );
            return Err(Box::new(e));
        }

        let level = match *elog_level {
            crate::gram::WARNING => types_error::WARNING,
            crate::gram::NOTICE => types_error::NOTICE,
            crate::gram::INFO => types_error::INFO,
            crate::gram::LOG => types_error::LOG,
            _ => types_error::DEBUG1,
        };
        let mut b = elog::ereport(level).errverbatim().errmsg_internal(err_message);
        if let Some(c) = err_code {
            b = b.errcode(c);
        }
        if let Some(d) = err_detail {
            b = b.errdetail_internal(d);
        }
        if let Some(h) = err_hint {
            b = b.errhint(h);
        }
        // pl_exec.c:3909-3923 — the `ereport(stmt->elog_level, ...)` that
        // RAISE lands on carries err_generic_string for COLUMN, CONSTRAINT,
        // DATATYPE, TABLE and SCHEMA at every level, not only at ERROR.
        let mut e = b.into_error();
        set_raise_fields(
            &mut e,
            err_column,
            err_constraint,
            err_datatype,
            err_table,
            err_schema,
        );
        elog::ThrowErrorData(e.with_error_location(types_error::ErrorLocation::new(
            "pl_exec.c",
            3909,
            "exec_stmt_raise",
        )))?;
        Ok(RC_OK)
    }

    fn exec_stmt_assert(&mut self, cond: &PlExpr, message: Option<&PlExpr>) -> PgResult<i32> {
        // plpgsql_check_asserts via the mutation-keyed GUC snapshot
        // (handler.rs PROCPERF P2); unset means C's default (true).
        let enabled = crate::handler::check_asserts_enabled();
        if !enabled {
            return Ok(RC_OK);
        }
        let (value, isnull) = self.exec_eval_boolean(cond)?;
        self.exec_eval_cleanup();
        if isnull || !value {
            let mut msg: Option<String> = None;
            if let Some(mx) = message {
                let (v, isnull, t, _m) = self.exec_eval_expr(mx)?;
                if !isnull {
                    msg = Some(self.convert_value_to_string(v, t)?);
                }
                self.exec_eval_cleanup();
            }
            return Err(Box::new(
                elog::ereport(ERROR)
                    .errcode(types_error::ERRCODE_ASSERT_FAILURE)
                    .errmsg(msg.unwrap_or_else(|| "assertion failed".to_string()))
                    .into_error(),
            ));
        }
        Ok(RC_OK)
    }

    fn exec_stmt_execsql(
        &mut self,
        expr: &PlExpr,
        into: bool,
        strict: bool,
        target: Dno,
    ) -> PgResult<i32> {
        self.ensure_plan(expr, CURSOR_OPT_PARALLEL_OK)?;
        let (plan, paramnos, argtypes, mod_stmt) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&expr.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone(), e.mod_stmt)
        });

        // too_many_rows extra check reads the GUCs at execution, not compile
        // (pl_exec.c:4217-4220).
        let too_many_rows_level = crate::handler::extra_checks_level(
            crate::comp::XCHECK_TOOMANYROWS,
        )?;

        let tcount: i64 = if into {
            if strict || mod_stmt || too_many_rows_level.is_some() {
                2
            } else {
                1
            }
        } else {
            0
        };

        let params = self.setup_param_list_under_spi(expr, &paramnos, &argtypes, true)?;
        let mut fetch_arg = self.spi_fetch_arg(&paramnos);
        let params = spi_param_list(&params, &mut fetch_arg);
        spi_frame!(frame, &expr.query, expr.parse_mode);
        let live = LiveSpiExpr::set((self as *mut Estate<'a>).cast(), expr);
        let rc = spi::SPI_execute_plan_with_paramlist(plan, params, self.readonly_func, tcount)
            .map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))?;
        drop(live);
        // spi.c:2825 pops _SPI_error_callback before the INTO assignment runs.
        drop(frame);

        match rc {
            spi::SPI_OK_SELECT
            | spi::SPI_OK_INSERT
            | spi::SPI_OK_UPDATE
            | spi::SPI_OK_DELETE
            | spi::SPI_OK_MERGE
            | spi::SPI_OK_INSERT_RETURNING
            | spi::SPI_OK_UPDATE_RETURNING
            | spi::SPI_OK_DELETE_RETURNING
            | spi::SPI_OK_MERGE_RETURNING => {
                let found = spi::SPI_processed() != 0;
                self.exec_set_found(found);
            }
            spi::SPI_OK_SELINTO | spi::SPI_OK_UTILITY => {}
            spi::SPI_OK_REWRITTEN => self.exec_set_found(false),
            spi::SPI_ERROR_COPY => {
                return Err(exec_err(
                    types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                    "cannot COPY to/from client in PL/pgSQL".to_string(),
                ));
            }
            spi::SPI_ERROR_TRANSACTION => {
                return Err(exec_err(
                    types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                    "unsupported transaction command in PL/pgSQL".to_string(),
                ));
            }
            other => panic!(
                "SPI_execute_plan failed executing query \"{}\": rc {other}",
                expr.query
            ),
        }

        self.eval_processed = spi::SPI_processed();

        if into {
            let Some(tuptab) = spi::SPI_tuptable() else {
                return Err(exec_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    "INTO used with a command that cannot return data".to_string(),
                ));
            };
            let n = spi::SPI_processed();
            if n == 0 {
                if strict {
                    let _ = spi::SPI_freetuptable(tuptab);
                    let mut b = elog::ereport(ERROR)
                        .errcode(types_error::ERRCODE_NO_DATA_FOUND)
                        .errmsg("query returned no rows");
                    if let Some(d) = self.format_expr_params(expr)? {
                        b = b.errdetail_internal(format!("parameters: {d}"));
                    }
                    return Err(Box::new(b.into_error()));
                }
                self.move_row_null(target, tuptab)?;
                let _ = spi::SPI_freetuptable(tuptab);
            } else {
                if n > 1 && (strict || mod_stmt || too_many_rows_level.is_some()) {
                    // errlevel per pl_exec.c:4404: strict/mod_stmt force
                    // ERROR; otherwise the extra-check level applies.
                    let errlevel = if strict || mod_stmt {
                        ERROR
                    } else {
                        too_many_rows_level.expect("guarded by is_some")
                    };
                    let mut b = elog::ereport(errlevel)
                        .errcode(types_error::ERRCODE_TOO_MANY_ROWS)
                        .errmsg("query returned more than one row")
                        .errhint("Make sure the query returns a single row, or use LIMIT 1.");
                    if let Some(d) = self.format_expr_params(expr)? {
                        b = b.errdetail_internal(format!("parameters: {d}"));
                    }
                    if errlevel == ERROR {
                        let _ = spi::SPI_freetuptable(tuptab);
                        return Err(Box::new(b.into_error()));
                    }
                    b.finish(types_error::ErrorLocation::new(file!(), line!() as i32, "exec_stmt_execsql"))?;
                }
                self.move_row_from_tuptable(target, tuptab, 0)?;
                let _ = spi::SPI_freetuptable(tuptab);
            }
            self.exec_eval_cleanup();
        } else if let Some(tuptab) = spi::SPI_tuptable() {
            let _ = spi::SPI_freetuptable(tuptab);
            let mut b = elog::ereport(ERROR)
                .errcode(types_error::ERRCODE_SYNTAX_ERROR)
                .errmsg("query has no destination for result data");
            if rc == spi::SPI_OK_SELECT {
                b = b.errhint(
                    "If you want to discard the results of a SELECT, use PERFORM instead.",
                );
            }
            return Err(Box::new(b.into_error()));
        }

        Ok(RC_OK)
    }

    // exec_stmt_call (pl_exec.c:2196); DO shares the statement shape but is a
    // named loud at the grammar. procedure_resowner is not carried: it only
    // matters when the callee ends the transaction, and intra-procedure
    // COMMIT/ROLLBACK is loud.
    fn exec_stmt_call(&mut self, expr: &PlExpr, is_call: bool) -> PgResult<i32> {
        self.ensure_plan(expr, 0)?;
        let (plan, paramnos, argtypes) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&expr.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone())
        });
        // C Assert(!expr->expr_simple_expr): a CALL/DO is never simple.
        debug_assert!(EXPR_PLANS.with(|t| {
            !matches!(
                t.borrow().get(&expr.expr_id).map(|e| &e.simple),
                Some(SimpleState::Ready(_))
            )
        }));

        let target = if is_call {
            Some(self.make_callstmt_target(expr, plan)?)
        } else {
            None
        };

        let params = self.setup_param_list_under_spi(expr, &paramnos, &argtypes, true)?;
        let mut fetch_arg = self.spi_fetch_arg(&paramnos);
        let params = spi_param_list(&params, &mut fetch_arg);
        let before_lxid = current_lxid();
        spi_frame!(frame, &expr.query, expr.parse_mode);
        let _live = LiveSpiExpr::set((self as *mut Estate<'a>).cast(), expr);
        let rc = spi::SPI_execute_plan_extended(
            plan,
            params,
            self.readonly_func,
            true,
            false,
            0,
            None,
        )
        .map_err(|e| spi_ctx_err(e, &expr.query, expr.parse_mode))?;
        drop(frame);
        if rc < 0 {
            panic!(
                "SPI_execute_plan_extended failed executing query \"{}\": rc {rc}",
                expr.query
            );
        }
        let after_lxid = current_lxid();
        if before_lxid != after_lxid {
            self.rebuild_simple_exprs();
        }

        let n = spi::SPI_processed();
        let tuptab = spi::SPI_tuptable();
        if n == 1 {
            let t = tuptab.expect("SPI_processed row without tuptable");
            let Some(target) = &target else {
                let _ = spi::SPI_freetuptable(t);
                return Err(exec_err(
                    types_error::ERRCODE_INTERNAL_ERROR,
                    "DO statement returned a row".to_string(),
                ));
            };
            let target = target.clone();
            self.move_row_into_varnos(&target, t, 0)?;
        } else if n > 1 {
            if let Some(t) = tuptab {
                let _ = spi::SPI_freetuptable(t);
            }
            return Err(exec_err(
                types_error::ERRCODE_INTERNAL_ERROR,
                "procedure call returned more than one row".to_string(),
            ));
        }

        self.exec_eval_cleanup();
        if let Some(t) = tuptab {
            let _ = spi::SPI_freetuptable(t);
        }
        Ok(RC_OK)
    }

    // exec_stmt_commit / exec_stmt_rollback (pl_exec.c:4956/4980).
    fn exec_stmt_commit_rollback(&mut self, commit: bool, chain: bool) -> PgResult<i32> {
        match (commit, chain) {
            (true, false) => spi::SPI_commit()?,
            (true, true) => spi::SPI_commit_and_chain()?,
            (false, false) => spi::SPI_rollback()?,
            (false, true) => spi::SPI_rollback_and_chain()?,
        }
        self.rebuild_simple_exprs();
        Ok(RC_OK)
    }

    // C's post-xact-end econtext rebuild (simple_eval_estate = NULL +
    // plpgsql_create_econtext, pl_exec.c:4967/2256): C must rebuild because
    // the compiled states lived in the transaction's simple_eval_estate and
    // their pins in the transaction's resowner (plpgsql_xact_cb,
    // pl_exec.c:8701). The function-lifetime SimpleExpr entries here own
    // their memory and hold manual plan refcounts, and every use starts
    // with CachedPlanIsSimplyValid — so they survive the transaction
    // boundary intact, which is exactly the amortization C gets from its
    // re-pin arm (CachedPlanIsSimplyValid + expr_simple_plan_lxid,
    // pl_exec.c:6060-6070) without the per-transaction recompile. Cast
    // states stay estate-owned and follow C's cast_lxid revalidation.
    fn rebuild_simple_exprs(&mut self) {
        self.cast_cache.clear();
    }

    // make_callstmt_target (pl_exec.c:2288): OUT-arg Params -> row varnos,
    // cached per expr like C's stmt->target (function lifetime).
    fn make_callstmt_target(
        &mut self,
        expr: &PlExpr,
        plan: SpiPlanPtr,
    ) -> PgResult<Vec<Dno>> {
        if let Some(v) = CALL_TARGETS.with(|t| t.borrow().get(&expr.expr_id).cloned()) {
            return Ok(v);
        }
        const PROARGMODE_INOUT: i8 = b'b' as i8;
        const PROARGMODE_OUT: i8 = b'o' as i8;

        let not_call_stmt = || {
            exec_err(
                types_error::ERRCODE_INTERNAL_ERROR,
                "query for CALL statement is not a CallStmt".to_string(),
            )
        };
        let Some((psrc, _)) = spi::SPI_plan_single_source(plan) else {
            return Err(not_call_stmt());
        };
        let cplan = plancache::GetCachedPlan(
            psrc,
            types_portal::ParamListHandle::NULL,
            None,
            types_portal::QueryEnvHandle::NULL,
        )?;
        let built = (|| -> PgResult<Vec<Dno>> {
            let stmts = plancache::CachedPlanStmtList(cplan);
            if stmts.len() != 1 {
                return Err(not_call_stmt());
            }
            let Some(stmt) = stmts[0].utilityStmt.and_then(|u| u.as_call_stmt()) else {
                return Err(not_call_stmt());
            };
            let funcexpr = stmt.funcexpr.expect("analyzed CallStmt has funcexpr");
            let arrays = syscache_seams::pg_proc_result_arrays::call(
                self.eval_ctx.mcx(),
                funcexpr.funcid,
            )?
            // pl_exec.c:2324-2328 elog(ERROR, "cache lookup failed for
            // function %u", funcexpr->funcid) -- catchable, SQLSTATE XX000
            // (elog's default); it must not abort the backend.
            .ok_or_else(|| {
                exec_err(
                    types_error::ERRCODE_INTERNAL_ERROR,
                    format!("cache lookup failed for function {}", funcexpr.funcid),
                )
            })?;

            let mut varnos: Vec<Dno> = Vec::new();
            if let Some(argmodes) = &arrays.proargmodes {
                for (i, &mode) in argmodes.iter().enumerate() {
                    if mode != PROARGMODE_INOUT && mode != PROARGMODE_OUT {
                        continue;
                    }
                    let param = (varnos.len() < stmt.outargs.len())
                        .then(|| stmt.outargs.nth(varnos.len()))
                        .and_then(|n| n.as_variant::<types_nodes::primnodes::Param>());
                    let Some(param) = param else {
                        let msg = match arrays
                            .proargnames
                            .as_ref()
                            .and_then(|names| names.get(i))
                            .map(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            Some(name) => format!(
                                "procedure parameter \"{name}\" is an output parameter but corresponding argument is not writable"
                            ),
                            None => format!(
                                "procedure parameter {} is an output parameter but corresponding argument is not writable",
                                i + 1
                            ),
                        };
                        return Err(exec_err(types_error::ERRCODE_SYNTAX_ERROR, msg));
                    };
                    // paramid is dno + 1 (make_datum_param).
                    let dno = param.paramid - 1;
                    self.exec_check_assignable(dno)?;
                    varnos.push(dno);
                }
            }
            debug_assert_eq!(varnos.len(), stmt.outargs.len());
            Ok(varnos)
        })();
        plancache::ReleaseCachedPlan(cplan);
        let varnos = built?;
        CALL_TARGETS.with(|t| {
            t.borrow_mut().insert(expr.expr_id, varnos.clone());
        });
        Ok(varnos)
    }

    // exec_check_assignable (pl_exec.c).
    fn exec_check_assignable(&self, dno: Dno) -> PgResult<()> {
        match &self.func.datums[dno as usize] {
            PlDatum::Var(v) => {
                if v.isconst {
                    return Err(exec_err(
                        types_error::ERRCODE_ERROR_IN_ASSIGNMENT,
                        format!("variable \"{}\" is declared CONSTANT", v.refname),
                    ));
                }
                Ok(())
            }
            PlDatum::Rec(r) => {
                if r.isconst {
                    return Err(exec_err(
                        types_error::ERRCODE_ERROR_IN_ASSIGNMENT,
                        format!("variable \"{}\" is declared CONSTANT", r.refname),
                    ));
                }
                Ok(())
            }
            PlDatum::Row(_) => Ok(()),
            PlDatum::RecField(f) => self.exec_check_assignable(f.recparentno),
        }
    }

    // exec_move_row scalar-list arm, over an explicit varno list.
    fn move_row_into_varnos(
        &mut self,
        varnos: &[Dno],
        tuptab: TuptabHandle,
        i: usize,
    ) -> PgResult<()> {
        let (desc, _src) = self.rec_desc_of(tuptab)?;
        self.move_row_fields_into_varnos(varnos, &desc, |anum| {
            spi::tuptable_with(tuptab, |t| {
                spi::SPI_getbinval(&t.vals[i], &t.tupdesc, (anum + 1) as i32)
            })
        })
    }

    // exec_move_row_from_fields' ROW arm (pl_exec.c:7181-7330) over an
    // explicit varno list: sources are read positionally with dropped
    // columns skipped, a missing source becomes NULL, and
    // strict_multi_assignment (pl_exec.c:7196-7202, 7288-7297, 7315-7330)
    // reports a short or a long source at the configured level.
    fn move_row_fields_into_varnos(
        &mut self,
        varnos: &[Dno],
        desc: &RecDesc,
        mut fetch: impl FnMut(usize) -> (Datum, bool),
    ) -> PgResult<()> {
        let natts = desc.types.len();
        let sma_level =
            crate::handler::extra_checks_level(crate::comp::XCHECK_STRICTMULTIASSIGNMENT)?;
        let mut anum = 0usize;
        for &dno in varnos {
            while anum < natts && desc.dropped[anum] {
                anum += 1;
            }
            let (v, isnull, vt, vm) = if anum < natts {
                let (v, isnull) = fetch(anum);
                let r = (v, isnull, desc.types[anum], desc.typmods[anum]);
                anum += 1;
                r
            } else {
                if let Some(level) = sma_level {
                    self.strict_multiassignment_report(level)?;
                }
                (Datum::null(), true, UNKNOWNOID, -1)
            };
            self.exec_assign_value(dno, v, isnull, vt, vm)?;
        }
        if let Some(level) = sma_level {
            while anum < natts && desc.dropped[anum] {
                anum += 1;
            }
            if anum < natts {
                self.strict_multiassignment_report(level)?;
            }
        }
        Ok(())
    }

    // exec_eval_using_params: unknown-typed params become text; by-ref
    // values are copied so they survive per-param cleanup.
    #[allow(clippy::type_complexity)]
    fn exec_eval_using_params(
        &mut self,
        params: &[PlExpr],
        stmt_mcx: Mcx<'_>,
    ) -> PgResult<(Vec<Oid>, Vec<Datum>, Vec<bool>)> {
        let n = params.len();
        let mut ptypes = ::mcx::std_vec_with_capacity("plpgsql", n)?;
        let mut pvalues = ::mcx::std_vec_with_capacity("plpgsql", n)?;
        let mut pnulls = ::mcx::std_vec_with_capacity("plpgsql", n)?;
        for p in params {
            self.ensure_plan(p, CURSOR_OPT_PARALLEL_OK)?;
            let (mut v, isnull, mut t, _m) = self.exec_eval_expr(p)?;
            if t == UNKNOWNOID {
                t = TEXTOID;
                if !isnull {
                    // SAFETY: unknown-typed datums carry C-string representation.
                    let s = unsafe {
                        core::ffi::CStr::from_ptr(v.as_usize() as *const core::ffi::c_char)
                    };
                    let tv = varlena::cstring_to_text(stmt_mcx, s.to_bytes())?;
                    v = fmgr::varlena_result(tv);
                }
            } else if !isnull {
                let (tl, tbv) = lsyscache::typ::get_typlenbyval(t)?;
                if !tbv {
                    // SAFETY: value is a live by-ref datum of typlen discipline.
                    v = unsafe { execexpr::agg_datum_copy(stmt_mcx, v, tl)? };
                }
            }
            ptypes.push(t);
            pvalues.push(v);
            pnulls.push(isnull);
            self.exec_eval_cleanup();
        }
        Ok((ptypes, pvalues, pnulls))
    }

    // exec_stmt_dynexecute: one-shot SPI execution of a computed query string
    // with USING params; INTO [STRICT] mirrors execsql's destination rules.
    fn exec_stmt_dynexecute(
        &mut self,
        query: &PlExpr,
        into: bool,
        strict: bool,
        target: Dno,
        params: &[PlExpr],
    ) -> PgResult<i32> {
        let (qv, isnull, restype, _m) = self.exec_eval_expr(query)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null".to_string(),
            ));
        }
        let querystr = self.convert_value_to_string(qv, restype)?;
        self.exec_eval_cleanup();

        let stmt_ctx = MemoryContext::new("PLpgSQL EXECUTE params");
        let (ptypes, pvalues, pnulls) = self.exec_eval_using_params(params, stmt_ctx.mcx())?;

        spi_frame!(frame, &querystr, parser_seams::RawParseMode::RAW_PARSE_DEFAULT);
        let rc = spi::SPI_execute_extended(
            &querystr,
            &ptypes,
            &pvalues,
            &pnulls,
            self.readonly_func,
            false,
            None,
        )
        .map_err(|e| spi_ctx_err(e, &querystr, parser_seams::RawParseMode::RAW_PARSE_DEFAULT))?;
        drop(frame);

        match rc {
            spi::SPI_OK_SELECT
            | spi::SPI_OK_INSERT
            | spi::SPI_OK_UPDATE
            | spi::SPI_OK_DELETE
            | spi::SPI_OK_MERGE
            | spi::SPI_OK_INSERT_RETURNING
            | spi::SPI_OK_UPDATE_RETURNING
            | spi::SPI_OK_DELETE_RETURNING
            | spi::SPI_OK_MERGE_RETURNING
            | spi::SPI_OK_UTILITY
            | spi::SPI_OK_REWRITTEN
            | 0 => {}
            spi::SPI_OK_SELINTO => {
                return Err(Box::new(
                    elog::ereport(ERROR)
                        .errcode(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
                        .errmsg("EXECUTE of SELECT ... INTO is not implemented")
                        .errhint(
                            "You might want to use EXECUTE ... INTO or EXECUTE CREATE TABLE ... AS instead.",
                        )
                        .into_error(),
                ));
            }
            spi::SPI_ERROR_COPY => {
                return Err(exec_err(
                    types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                    "cannot COPY to/from client in PL/pgSQL".to_string(),
                ));
            }
            spi::SPI_ERROR_TRANSACTION => {
                return Err(exec_err(
                    types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                    "EXECUTE of transaction commands is not implemented".to_string(),
                ));
            }
            other => panic!(
                "SPI_execute_extended failed executing query \"{querystr}\": rc {other}"
            ),
        }

        self.eval_processed = spi::SPI_processed();

        if into {
            let Some(tuptab) = spi::SPI_tuptable() else {
                return Err(exec_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    "INTO used with a command that cannot return data".to_string(),
                ));
            };
            let n = spi::SPI_processed();
            if n == 0 {
                if strict {
                    let _ = spi::SPI_freetuptable(tuptab);
                    let mut b = elog::ereport(ERROR)
                        .errcode(types_error::ERRCODE_NO_DATA_FOUND)
                        .errmsg("query returned no rows");
                    if let Some(d) = self.format_prepared_params(&ptypes, &pvalues, &pnulls)? {
                        b = b.errdetail_internal(format!("parameters: {d}"));
                    }
                    return Err(Box::new(b.into_error()));
                }
                self.move_row_null(target, tuptab)?;
                let _ = spi::SPI_freetuptable(tuptab);
            } else {
                if n > 1 && strict {
                    let _ = spi::SPI_freetuptable(tuptab);
                    let mut b = elog::ereport(ERROR)
                        .errcode(types_error::ERRCODE_TOO_MANY_ROWS)
                        .errmsg("query returned more than one row");
                    if let Some(d) = self.format_prepared_params(&ptypes, &pvalues, &pnulls)? {
                        b = b.errdetail_internal(format!("parameters: {d}"));
                    }
                    return Err(Box::new(b.into_error()));
                }
                self.move_row_from_tuptable(target, tuptab, 0)?;
                let _ = spi::SPI_freetuptable(tuptab);
            }
            self.exec_eval_cleanup();
        } else if let Some(tuptab) = spi::SPI_tuptable() {
            // Historically EXECUTE without INTO discards any result rows.
            let _ = spi::SPI_freetuptable(tuptab);
        }

        Ok(RC_OK)
    }

    // exec_eval_integer (pl_exec.c): cast to int4.
    fn exec_eval_integer(&mut self, expr: &PlExpr) -> PgResult<(i32, bool)> {
        const INT4OID: Oid = 23;
        let (v, mut isnull, t, m) = self.exec_eval_expr(expr)?;
        let v = self.exec_cast_value(v, &mut isnull, t, m, INT4OID, -1)?;
        Ok((v.as_i32(), isnull))
    }

    // exec_init_tuple_store (pl_exec.c:3669).
    fn init_tuple_store(&mut self) -> PgResult<()> {
        let Some(rsi) = self.rsi else {
            return Err(exec_err(
                types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                "set-valued function called in context that cannot accept a set".to_string(),
            ));
        };
        if rsi.allowed_modes & fmgr::SFRM_Materialize == 0 || rsi.expected_desc.is_none() {
            return Err(exec_err(
                types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
                "materialize mode required, but it is not allowed in this context".to_string(),
            ));
        }
        // SAFETY: expectedDesc contract — armed by the executor with the scan
        // tupdesc, live for the duration of this call.
        let expected = unsafe {
            rsi.expected_desc
                .expect("checked above")
                .cast::<types_tuple::TupleDescData<'_>>()
                .as_ref()
        };
        let td = tupdesc::CreateTupleDescCopy(self.datum_ctx.mcx(), expected)?;
        let random = rsi.allowed_modes & fmgr::SFRM_Materialize_Random != 0;
        self.tuple_store = Some(tuplestore::Tuplestore::begin_heap_with_owner(
            random,
            false,
            init_small::globals::work_mem(),
            self.tuple_store_owner,
        ));
        self.tuple_store_desc = Some(td);
        Ok(())
    }

    pub fn take_tuple_store(&mut self) -> Option<tuplestore::Tuplestore> {
        self.tuple_store.take()
    }

    // exec_stmt_return_next (pl_exec.c:3326).
    fn exec_stmt_return_next(&mut self, expr: Option<&PlExpr>, retvarno: Dno) -> PgResult<i32> {
        if !self.func.fn_retset {
            return Err(exec_err(
                types_error::ERRCODE_SYNTAX_ERROR,
                "cannot use RETURN NEXT in a non-SETOF function".to_string(),
            ));
        }
        if self.tuple_store.is_none() {
            self.init_tuple_store()?;
        }
        let dst = RecDesc::from_tupdesc(self.tuple_store_desc.as_ref().expect("initialized"))?;
        let natts = dst.types.len();

        if retvarno >= 0 {
            match &self.func.datums[retvarno as usize] {
                PlDatum::Var(v) => {
                    let (typoid, typmod) = (v.datatype.typoid, v.datatype.atttypmod);
                    if natts != 1 {
                        return Err(exec_err(
                            types_error::ERRCODE_DATATYPE_MISMATCH,
                            "wrong result type supplied in RETURN NEXT".to_string(),
                        ));
                    }
                    let (value, mut isnull) = self.get_var(retvarno)?;
                    let v = self.exec_cast_value(
                        value,
                        &mut isnull,
                        typoid,
                        typmod,
                        dst.types[0],
                        dst.typmods[0],
                    )?;
                    self.put_tuple_store_values(&[v], &[isnull])?;
                }
                PlDatum::Rec(_) => {
                    if matches!(&self.datums[retvarno as usize], DatumVal::Rec(None)) {
                        self.instantiate_empty_rec(retvarno)?;
                    }
                    let (srcdesc, values, nulls) = match &mut self.datums[retvarno as usize] {
                        DatumVal::Rec(Some(rv)) => {
                            rv.empty = false;
                            (rv.desc.clone(), rv.values.clone(), rv.nulls.clone())
                        }
                        _ => unreachable!("instantiated above"),
                    };
                    let (v, n) = convert_values_by_position(
                        &srcdesc,
                        &values,
                        &nulls,
                        &dst,
                        "wrong record type supplied in RETURN NEXT",
                    )?;
                    self.put_tuple_store_values(&v, &n)?;
                }
                PlDatum::Row(row) => {
                    let rv = self.row_as_rec_value(row.dno, true, Some(&dst))?.ok_or_else(|| {
                        exec_err(
                            types_error::ERRCODE_DATATYPE_MISMATCH,
                            "wrong record type supplied in RETURN NEXT".to_string(),
                        )
                    })?;
                    let (v, n) = convert_values_by_position(
                        &rv.desc,
                        &rv.values,
                        &rv.nulls,
                        &dst,
                        "wrong record type supplied in RETURN NEXT",
                    )?;
                    self.put_tuple_store_values(&v, &n)?;
                }
                _ => panic!("plpgsql: bad retvarno in RETURN NEXT"),
            }
        } else if let Some(expr) = expr {
            let (value, mut isnull, rettype, rettypmod) = self.exec_eval_expr(expr)?;
            if self.func.fn_retistuple {
                if !isnull {
                    if !lsyscache::typ::type_is_rowtype(rettype)? {
                        return Err(exec_err(
                            types_error::ERRCODE_DATATYPE_MISMATCH,
                            "cannot return non-composite value from function returning composite type"
                                .to_string(),
                        ));
                    }
                    let (srcdesc, _src, values, nulls, _) =
                        self.deconstruct_composite(value, self.eval_ctx.mcx())?;
                    let (v, n) = convert_values_by_position(
                        &srcdesc,
                        &values,
                        &nulls,
                        &dst,
                        "returned record type does not match expected record type",
                    )?;
                    self.put_tuple_store_values(&v, &n)?;
                } else {
                    self.put_tuple_store_values(
                        &::mcx::std_vec_from_elem("plpgsql", Datum::null(), natts)?,
                        &::mcx::std_vec_from_elem("plpgsql", true, natts)?,
                    )?;
                }
            } else {
                if natts != 1 {
                    return Err(exec_err(
                        types_error::ERRCODE_DATATYPE_MISMATCH,
                        "wrong result type supplied in RETURN NEXT".to_string(),
                    ));
                }
                let v = self.exec_cast_value(
                    value,
                    &mut isnull,
                    rettype,
                    rettypmod,
                    dst.types[0],
                    dst.typmods[0],
                )?;
                self.put_tuple_store_values(&[v], &[isnull])?;
            }
        } else {
            return Err(exec_err(
                types_error::ERRCODE_SYNTAX_ERROR,
                "RETURN NEXT must have a parameter".to_string(),
            ));
        }
        self.exec_eval_cleanup();
        Ok(RC_OK)
    }

    fn put_tuple_store_values(&mut self, values: &[Datum], nulls: &[bool]) -> PgResult<()> {
        let td = self.tuple_store_desc.as_ref().expect("tuple store initialized");
        self.tuple_store
            .as_mut()
            .expect("tuple store initialized")
            .putvalues(td, values, nulls)
    }

    // exec_stmt_return_query (pl_exec.c:3544): rows stream into the
    // tuplestore after a per-batch structure check.
    fn exec_stmt_return_query(
        &mut self,
        query: Option<&PlExpr>,
        dynquery: Option<&PlExpr>,
        params: &[PlExpr],
    ) -> PgResult<i32> {
        if !self.func.fn_retset {
            return Err(exec_err(
                types_error::ERRCODE_SYNTAX_ERROR,
                "cannot use RETURN QUERY in a non-SETOF function".to_string(),
            ));
        }
        if self.tuple_store.is_none() {
            self.init_tuple_store()?;
        }
        let store = self.tuple_store.take().expect("initialized");
        let tcount = store.tuple_count();
        let tstore = tuplestore::hold::register(store);
        let stmt_ctx = MemoryContext::new("PLpgSQL RETURN QUERY receiver");
        let target = tupdesc::CreateTupleDescCopy(
            stmt_ctx.mcx(),
            self.tuple_store_desc.as_ref().expect("initialized"),
        )
        .map(std::rc::Rc::new);
        let rc = match target {
            Err(e) => Err(e),
            Ok(target) => {
                let mut treceiver =
                    tcop_dest::CreateDestReceiver(types_dest::CommandDest::Tuplestore);
                tcop_dest::SetTuplestoreDestReceiverParams(
                    &mut treceiver,
                    tstore,
                    false,
                    Some(target),
                    Some("structure of query does not match function result type"),
                );
                self.run_return_query(query, dynquery, params, &mut treceiver)
            }
        };
        self.tuple_store = Some(tuplestore::hold::take(tstore).expect("registered above"));
        rc?;
        self.eval_tuptable = None;
        self.exec_eval_cleanup();

        let processed =
            (self.tuple_store.as_ref().expect("initialized").tuple_count() - tcount) as u64;
        self.eval_processed = processed;
        self.exec_set_found(processed != 0);
        Ok(RC_OK)
    }

    fn run_return_query(
        &mut self,
        query: Option<&PlExpr>,
        dynquery: Option<&PlExpr>,
        params: &[PlExpr],
        treceiver: &mut tcop_dest::DestReceiver<'_>,
    ) -> PgResult<i32> {
        if let Some(query) = query {
            self.ensure_plan(query, CURSOR_OPT_PARALLEL_OK)?;
            return self.exec_spi_plan_dest(query, 0, true, Some(treceiver));
        }
        let dynquery = dynquery.expect("RETURN QUERY has a query");
        let (qv, isnull, restype, _m) = self.exec_eval_expr(dynquery)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null".to_string(),
            ));
        }
        let querystr = self.convert_value_to_string(qv, restype)?;
        self.exec_eval_cleanup();
        let stmt_ctx = MemoryContext::new("PLpgSQL RETURN QUERY EXECUTE params");
        let (ptypes, pvalues, pnulls) = self.exec_eval_using_params(params, stmt_ctx.mcx())?;
        spi_frame!(_frame, &querystr, parser_seams::RawParseMode::RAW_PARSE_DEFAULT);
        // must_return_tuples = true (pl_exec.c:3626): the SPI level
        // describes its own errors — "empty query does not return
        // tuples" is raised before its callback names a query, so it
        // carries no SQL-statement line (spi.c:2494 vs 2509).
        spi::SPI_execute_extended(
            &querystr,
            &ptypes,
            &pvalues,
            &pnulls,
            self.readonly_func,
            true,
            Some(treceiver),
        )
    }

    // ------------------------------------------------------------------
    // Cursors
    // ------------------------------------------------------------------

    fn cursor_var_name(&mut self, curvar: Dno) -> PgResult<Option<String>> {
        let (value, isnull) = self.get_var(curvar)?;
        if isnull {
            return Ok(None);
        }
        Ok(Some(self.convert_value_to_string(value, TEXTOID)?))
    }

    fn cursor_var_name_required(&mut self, curvar: Dno) -> PgResult<String> {
        match self.cursor_var_name(curvar)? {
            Some(n) => Ok(n),
            None => {
                let refname = match &self.func.datums[curvar as usize] {
                    PlDatum::Var(v) => v.refname.clone(),
                    _ => String::new(),
                };
                Err(exec_err(
                    types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                    format!("cursor variable \"{refname}\" is null"),
                ))
            }
        }
    }

    fn find_open_portal(&mut self, curname: &str) -> PgResult<types_portal::Portal<'static>> {
        match spi::SPI_cursor_find_portal(curname) {
            Some(p) => Ok(p),
            None => Err(exec_err(
                types_error::ERRCODE_UNDEFINED_CURSOR,
                format!("cursor \"{curname}\" does not exist"),
            )),
        }
    }

    // exec_stmt_open's shared tail for bound cursors and OPEN FOR query:
    // check name free, run the cursor's plan into a portal, store the portal
    // name back if the refcursor was null.
    fn open_cursor_portal(
        &mut self,
        curvar: Dno,
        curname: Option<&str>,
        query: &PlExpr,
        cursor_options: i32,
    ) -> PgResult<()> {
        self.ensure_plan(query, cursor_options)?;
        let (plan, paramnos, argtypes) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&query.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone())
        });
        let params = self.setup_param_list_under_spi(query, &paramnos, &argtypes, false)?;
        spi_frame!(frame, &query.query, query.parse_mode);
        let _live = LiveSpiExpr::set((self as *mut Estate<'a>).cast(), query);
        let cursor = spi::SPI_cursor_open_with_paramlist(curname, plan, &params, self.readonly_func)?;
        drop(frame);
        if curname.is_none() {
            // Verify assignability before storing the portal name
            // (pl_exec.c:4803-4807 -> exec_check_assignable).
            self.exec_check_assignable(curvar)?;
            let name = cursor.portal.borrow().name.as_str().to_string();
            self.assign_text_var(curvar, &name)?;
        }
        self.exec_eval_cleanup();
        Ok(())
    }

    // exec_stmt_open (pl_exec.c:4657).
    #[allow(clippy::too_many_arguments)]
    fn exec_stmt_open(
        &mut self,
        curvar: Dno,
        cursor_options: i32,
        argquery: Option<&PlExpr>,
        query: Option<&PlExpr>,
        dynquery: Option<&PlExpr>,
        params: &[PlExpr],
    ) -> PgResult<i32> {
        let curname = self.cursor_var_name(curvar)?;
        if let Some(n) = &curname {
            if spi::SPI_cursor_name_in_use(n) {
                return Err(exec_err(
                    types_error::ERRCODE_DUPLICATE_CURSOR,
                    format!("cursor \"{n}\" already in use"),
                ));
            }
        }

        if let Some(q) = query {
            self.open_cursor_portal(curvar, curname.as_deref(), q, cursor_options)?;
            return Ok(RC_OK);
        }
        if let Some(dq) = dynquery {
            let portal =
                self.exec_dynquery_with_params(dq, params, curname.as_deref(), cursor_options)?;
            if curname.is_none() {
                // pl_exec.c:4727-4730.
                self.exec_check_assignable(curvar)?;
                let name = portal.borrow().name.as_str().to_string();
                self.assign_text_var(curvar, &name)?;
            }
            return Ok(RC_OK);
        }

        // OPEN of a bound cursor: evaluate declared args, then its query.
        let (cq, argrow, bound_options) = match &self.func.datums[curvar as usize] {
            PlDatum::Var(v) => (
                v.cursor_explicit_expr.as_ref().expect("bound cursor"),
                v.cursor_explicit_argrow,
                v.cursor_options,
            ),
            _ => panic!("plpgsql: OPEN curvar is not a Var"),
        };
        if let Some(aq) = argquery {
            if argrow < 0 {
                return Err(exec_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    "arguments given for cursor without arguments".to_string(),
                ));
            }
            self.exec_stmt_execsql(aq, true, false, argrow)?;
        } else if argrow >= 0 {
            return Err(exec_err(
                types_error::ERRCODE_SYNTAX_ERROR,
                "arguments required for cursor".to_string(),
            ));
        }
        self.open_cursor_portal(curvar, curname.as_deref(), cq, bound_options)?;
        Ok(RC_OK)
    }

    // exec_dynquery_with_params (pl_exec.c:8800): dynamic source into a
    // one-shot cursor portal.
    fn exec_dynquery_with_params(
        &mut self,
        dynquery: &PlExpr,
        params: &[PlExpr],
        curname: Option<&str>,
        cursor_options: i32,
    ) -> PgResult<types_portal::Portal<'static>> {
        let (qv, isnull, restype, _m) = self.exec_eval_expr(dynquery)?;
        if isnull {
            return Err(exec_err(
                types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                "query string argument of EXECUTE is null".to_string(),
            ));
        }
        let querystr = self.convert_value_to_string(qv, restype)?;
        self.exec_eval_cleanup();
        // SPI_cursor_open copies the values into the portal (copyParamList).
        let stmt_ctx = MemoryContext::new("PLpgSQL OPEN params");
        let (ptypes, pvalues, pnulls) = self.exec_eval_using_params(params, stmt_ctx.mcx())?;
        spi_frame!(frame, &querystr, parser_seams::RawParseMode::RAW_PARSE_DEFAULT);
        let cursor = spi::SPI_cursor_open_extended(
            curname,
            &querystr,
            &ptypes,
            &pvalues,
            &pnulls,
            self.readonly_func,
            cursor_options,
        )?;
        drop(frame);
        Ok(cursor.portal)
    }

    // exec_stmt_fetch / exec_stmt_move (pl_exec.c:4822).
    fn exec_stmt_fetch(
        &mut self,
        target: Dno,
        curvar: Dno,
        direction: i32,
        how_many: i64,
        expr: Option<&PlExpr>,
        is_move: bool,
    ) -> PgResult<i32> {
        let curname = self.cursor_var_name_required(curvar)?;
        let portal = self.find_open_portal(&curname)?;

        let mut how_many = how_many;
        if let Some(e) = expr {
            let (v, isnull) = self.exec_eval_integer(e)?;
            if isnull {
                return Err(exec_err(
                    types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED,
                    "relative or absolute cursor position is null".to_string(),
                ));
            }
            how_many = v as i64;
            self.exec_eval_cleanup();
        }

        let dir = fetch_direction_of(direction);
        let n;
        if !is_move {
            spi::SPI_scroll_cursor_fetch(&portal, dir, how_many)?;
            n = spi::SPI_processed();
            let tuptab = spi::SPI_tuptable().expect("SPI fetch stored tuptable");
            if n == 0 {
                self.move_row_null(target, tuptab)?;
            } else {
                self.move_row_from_tuptable(target, tuptab, 0)?;
            }
            self.exec_eval_cleanup();
            let _ = spi::SPI_freetuptable(tuptab);
        } else {
            spi::SPI_scroll_cursor_move(&portal, dir, how_many)?;
            n = spi::SPI_processed();
        }

        self.eval_processed = n;
        self.exec_set_found(n != 0);
        Ok(RC_OK)
    }

    // exec_stmt_close (pl_exec.c:4913).
    fn exec_stmt_close(&mut self, curvar: Dno) -> PgResult<i32> {
        let curname = self.cursor_var_name_required(curvar)?;
        let portal = self.find_open_portal(&curname)?;
        spi::SPI_cursor_close_portal(&portal)?;
        Ok(RC_OK)
    }

    // exec_stmt_forc (pl_exec.c:2868).
    fn exec_stmt_forc(
        &mut self,
        label: Option<&str>,
        var: Dno,
        curvar: Dno,
        argquery: Option<&PlExpr>,
        body: &'a [PlStmt],
    ) -> PgResult<i32> {
        let curname = self.cursor_var_name(curvar)?;
        if let Some(n) = &curname {
            if spi::SPI_cursor_name_in_use(n) {
                return Err(exec_err(
                    types_error::ERRCODE_DUPLICATE_CURSOR,
                    format!("cursor \"{n}\" already in use"),
                ));
            }
        }

        let (cq, argrow, bound_options) = match &self.func.datums[curvar as usize] {
            PlDatum::Var(v) => (
                v.cursor_explicit_expr.as_ref().expect("bound cursor"),
                v.cursor_explicit_argrow,
                v.cursor_options,
            ),
            _ => panic!("plpgsql: FOR-cursor curvar is not a Var"),
        };
        if let Some(aq) = argquery {
            if argrow < 0 {
                return Err(exec_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    "arguments given for cursor without arguments".to_string(),
                ));
            }
            self.exec_stmt_execsql(aq, true, false, argrow)?;
        } else if argrow >= 0 {
            return Err(exec_err(
                types_error::ERRCODE_SYNTAX_ERROR,
                "arguments required for cursor".to_string(),
            ));
        }

        self.ensure_plan(cq, bound_options)?;
        let (plan, paramnos, argtypes) = EXPR_PLANS.with(|t| {
            let t = t.borrow();
            let e = t.get(&cq.expr_id).expect("plan ensured");
            (e.plan, e.paramnos.clone(), e.argtypes.clone())
        });
        let params = self.setup_param_list_under_spi(cq, &paramnos, &argtypes, false)?;
        spi_frame!(frame, &cq.query, cq.parse_mode);
        let cursor =
            spi::SPI_cursor_open_with_paramlist(curname.as_deref(), plan, &params, self.readonly_func)?;
        drop(frame);
        if curname.is_none() {
            let name = cursor.portal.borrow().name.as_str().to_string();
            self.assign_text_var(curvar, &name)?;
        }
        self.exec_eval_cleanup();

        // No prefetch: the cursor is user-visible (WHERE CURRENT OF).
        let result = self.exec_for_query(label, var, &cursor, body, false);

        let rc = match result {
            Ok(rc) => {
                spi::SPI_cursor_close(cursor)?;
                rc
            }
            Err(e) => return Err(e),
        };
        if curname.is_none() {
            self.set_var(curvar, Datum::null(), true, false);
        }
        Ok(rc)
    }

    // exec_stmt_dynfors (pl_exec.c): FOR-over-EXECUTE.
    fn exec_stmt_dynfors(
        &mut self,
        label: Option<&str>,
        var: Dno,
        query: &PlExpr,
        params: &[PlExpr],
        body: &'a [PlStmt],
    ) -> PgResult<i32> {
        // C exec_stmt_dynfors (pl_exec.c:4635) opens the implicit cursor
        // with CURSOR_OPT_NO_SCROLL — same FOR-loop pinning as exec_stmt_fors.
        let portal =
            self.exec_dynquery_with_params(query, params, None, CURSOR_OPT_NO_SCROLL)?;
        let cursor = spi::SpiCursor::from_portal(portal);
        let result = self.exec_for_query(label, var, &cursor, body, true);
        match result {
            Ok(rc) => {
                spi::SPI_cursor_close(cursor)?;
                Ok(rc)
            }
            Err(e) => Err(e),
        }
    }
}

// build_attrmap_by_position semantics applied to values (attmap.c:64-152):
// typmod-aware positional match over non-dropped columns, missing sources
// disallowed, with C's two errdetail texts under the caller's errmsg.
pub(crate) fn convert_values_by_position(
    src: &RecDesc,
    values: &[Datum],
    nulls: &[bool],
    dst: &RecDesc,
    msg: &str,
) -> PgResult<(Vec<Datum>, Vec<bool>)> {
    #[track_caller]
    #[cold]
    fn mismatch(msg: &str, detail: String) -> Box<PgError> {
        Box::new(
            elog::ereport(ERROR)
                .errcode(types_error::ERRCODE_DATATYPE_MISMATCH)
                .errmsg(msg.to_string())
                .errdetail(detail)
                .into_error(),
        )
    }
    let n = dst.types.len();
    let mut out_values = ::mcx::std_vec_from_elem("plpgsql", Datum::null(), n)?;
    let mut out_nulls = ::mcx::std_vec_from_elem("plpgsql", true, n)?;
    let mut j = 0usize;
    let mut nincols = 0;
    let mut noutcols = 0;
    for i in 0..n {
        if dst.dropped[i] {
            continue;
        }
        noutcols += 1;
        while j < src.types.len() {
            if src.dropped[j] {
                j += 1;
                continue;
            }
            nincols += 1;
            if dst.types[i] != src.types[j]
                || (dst.typmods[i] != src.typmods[j] && dst.typmods[i] >= 0)
            {
                return Err(mismatch(
                    msg,
                    format!(
                        "Returned type {} does not match expected type {} in column \"{}\" (position {}).",
                        format_type::format_type_with_typemod(src.types[j], src.typmods[j])?,
                        format_type::format_type_with_typemod(dst.types[i], dst.typmods[i])?,
                        dst.names[i],
                        noutcols
                    ),
                ));
            }
            out_values[i] = values[j];
            out_nulls[i] = nulls[j];
            j += 1;
            break;
        }
    }
    let extra = src.types[j..]
        .iter()
        .enumerate()
        .filter(|&(k, _)| !src.dropped[j + k])
        .count();
    if extra > 0 || nincols != noutcols {
        return Err(mismatch(
            msg,
            format!(
                "Number of returned columns ({}) does not match expected column count ({}).",
                nincols + extra,
                noutcols
            ),
        ));
    }
    Ok((out_values, out_nulls))
}

fn resolve_origtypname(name: &OrigTypeName) -> PgResult<(Oid, i32)> {
    let cx = MemoryContext::new("plpgsql rowtype re-resolution");
    let mcx = cx.mcx();
    match name {
        OrigTypeName::Sql(s) => parse_utilcmd::parseTypeString(mcx, s),
        OrigTypeName::Names(parts) => {
            let mut nodes: mcx::PgVec<'_, types_nodes::Node<'_>> =
                mcx::vec_with_capacity_in(mcx, parts.len())?;
            for p in parts {
                let s = core::str::from_utf8(mcx::PgString::from_str_in(p, mcx)?.into_bytes().leak())
                    .expect("copied from a str");
                nodes.push(types_nodes::Node::mk_string(mcx, s)?);
            }
            let tn = types_nodes::rawnodes::TypeName {
                names: types_nodes::NodeList::from_slice(mcx, &nodes)?,
                typemod: -1,
                location: -1,
                ..Default::default()
            };
            parse_utilcmd::typenameTypeIdAndMod(mcx, None, &tn)
        }
    }
}

fn fetch_direction_of(direction: i32) -> types_portal::FetchDirection {
    match direction {
        FETCH_FORWARD => types_portal::FetchDirection::FETCH_FORWARD,
        FETCH_BACKWARD => types_portal::FetchDirection::FETCH_BACKWARD,
        FETCH_ABSOLUTE => types_portal::FetchDirection::FETCH_ABSOLUTE,
        FETCH_RELATIVE => types_portal::FetchDirection::FETCH_RELATIVE,
        other => panic!("unrecognized fetch direction: {other}"),
    }
}

// plpgsql_recognize_err_condition(allow_sqlstate=true) returning the state.
// exec_stmt_raise's `int err_code` (pl_exec.c:3828): SQLSTATE 00000 is the
// integer 0, i.e. "no code given".
fn raise_err_code(code: SqlState) -> Option<SqlState> {
    (code.0 != 0).then_some(code)
}

fn recognize_err_condition(condname: &str) -> PgResult<SqlState> {
    if condname.len() == 5
        && condname
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    {
        let b = condname.as_bytes();
        return Ok(types_error::make_sqlstate([b[0], b[1], b[2], b[3], b[4]]));
    }
    for &(name, code) in EXCEPTION_LABEL_MAP {
        if name == condname {
            return Ok(types_error::make_sqlstate(code));
        }
    }
    Err(exec_err(
        types_error::ERRCODE_UNDEFINED_OBJECT,
        format!("unrecognized exception condition \"{condname}\""),
    ))
}

// MyProc->vxid.lxid (exec_stmt_call's before/after transaction check).
fn current_lxid() -> u32 {
    let procno = lmgr_proc::MyProc().expect("MyProc is not set");
    lmgr_proc::GetPGProcByNumber(procno)
        .vxid
        .lxid
        .load(core::sync::atomic::Ordering::Relaxed)
}

fn unpack_sql_state(code: SqlState) -> String {
    String::from_utf8_lossy(&types_error::unpack_sqlstate(code)).into_owned()
}

// exception_matches_conditions: OTHERS (state -1) matches everything except
// query_canceled and assert_failure; category codes match their class.
fn exception_matches_conditions(e: &PgError, conds: &[PlCondition]) -> bool {
    for c in conds {
        let cs = c.sqlerrstate;
        if cs == PLPGSQL_OTHERS {
            if e.sqlstate != types_error::ERRCODE_QUERY_CANCELED
                && e.sqlstate != types_error::ERRCODE_ASSERT_FAILURE
            {
                return true;
            }
        } else if e.sqlstate == cs
            || (types_error::errcode_is_category(cs)
                && types_error::errcode_to_category(e.sqlstate) == cs)
        {
            return true;
        }
    }
    false
}

// C bakes every live frame's context line at errfinish; this port attaches a
// frame's line at the first boundary the error crosses in that frame (catch
// site or frame exit). plpgsql_context_attached tracks "already attached for
// the current innermost frame"; frame exit clears it so outer frames attach.
#[cold]
pub(crate) fn attach_frame_context_at_catch(
    e: Box<types_error::PgError>,
    estate: &Estate<'_>,
) -> Box<types_error::PgError> {
    if e.plpgsql_context_attached {
        return e;
    }
    let mut e = attach_frame_line(e, estate);
    e.plpgsql_context_attached = true;
    e
}

#[cold]
pub(crate) fn attach_frame_context_at_exit(
    mut e: Box<types_error::PgError>,
    estate: &Estate<'_>,
) -> Box<types_error::PgError> {
    if e.plpgsql_context_attached {
        e.plpgsql_context_attached = false;
        return e;
    }
    attach_frame_line(e, estate)
}

fn frame_context_line(estate: &Estate<'_>) -> String {
    frame_context_line_of(&estate.frame)
}

// GetErrorContextStack at the catch, minus this function's own frame (the
// innermost walked frame, attached to the error itself).
fn outer_context_at_catch(estate: &Estate<'_>) -> Option<String> {
    let stack = elog::GetErrorContextStack()?;
    let own = frame_context_line(estate);
    let rest = match stack.strip_prefix(own.as_str()) {
        Some(rest) => rest.strip_prefix('\n').unwrap_or(rest),
        None => stack.as_str(),
    };
    (!rest.is_empty()).then(|| rest.to_string())
}

pub(crate) fn frame_context_line_of(frame: &FrameShared) -> String {
    let sig = &frame.sig;
    // err_var lineno wins over err_stmt's (plpgsql_exec_error_callback).
    let lineno = match frame.var_lineno.get() {
        Some(l) => Some(l),
        None => frame.stmt.get().map(|(l, _)| l),
    };
    if let Some(t) = frame.text.get() {
        match lineno {
            Some(lineno) if lineno > 0 => {
                format!("PL/pgSQL function {sig} line {lineno} {t}")
            }
            _ => format!("PL/pgSQL function {sig} {t}"),
        }
    } else if let Some((_, typename)) = frame.stmt.get() {
        match lineno {
            Some(lineno) if lineno > 0 => {
                format!("PL/pgSQL function {sig} line {lineno} at {typename}")
            }
            _ => format!("PL/pgSQL function {sig}"),
        }
    } else {
        format!("PL/pgSQL function {sig}")
    }
}

#[cold]
fn attach_frame_line(
    mut e: Box<types_error::PgError>,
    estate: &Estate<'_>,
) -> Box<types_error::PgError> {
    let line = frame_context_line(estate);
    e.add_context_line(line);
    e
}

fn set_raise_fields(
    e: &mut PgError,
    column: Option<String>,
    constraint: Option<String>,
    datatype: Option<String>,
    table: Option<String>,
    schema: Option<String>,
) {
    e.column_name = column;
    e.constraint_name = constraint;
    e.datatype_name = datatype;
    e.table_name = table;
    e.schema_name = schema;
}

// The portal-level snapshot goes back (a COMMIT/ROLLBACK drops every
// snapshot) before the plan is validated or replanned, so the assignment
// can still detoast what the eval fetched under it.
fn simple_expr_portal_snapshot(expr_id: u32, se: Box<SimpleExpr>) -> PgResult<Box<SimpleExpr>> {
    match pquery::EnsurePortalSnapshotExists() {
        Ok(()) => Ok(se),
        Err(e) => {
            let plan = se.plan;
            drop(se);
            put_simple(expr_id, plan, SimpleState::Unknown);
            Err(e)
        }
    }
}

// DatumGetArrayTypePCopy: the FOREACH array, in any varlena form, as a
// private 4B-header copy in `mcx`.
fn array_datum_copy<'mcx>(mcx: Mcx<'mcx>, value: Datum) -> PgResult<mcx::PgVec<'mcx, u8>> {
    let p = value.as_usize() as *const u8;
    // SAFETY: non-null by-ref datum, readable for its header-declared size.
    let raw = unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) };
    detoast::detoast_attr(mcx, raw)
}

#[cfg(test)]
pub(crate) mod cfi_tests {
    use super::*;

    #[test]
    fn spi_context_suppresses_immediate_complete_entries() {
        let mode = parser_seams::RawParseMode::RAW_PARSE_DEFAULT;
        for query in ["select 1/0", "select\n1/0"] {
            let line = spi_context_line(query, mode);
            for prefix in ["", "PL/pgSQL function inner_fn() line 1 at EXECUTE\n"] {
                let context = format!("{prefix}{line}");
                let mut e = exec_err(types_error::ERRCODE_DIVISION_BY_ZERO, "division by zero".into());
                e.context = Some(context.clone());
                spi_error_context(&mut e, query, mode);
                assert_eq!(e.context.as_deref(), Some(context.as_str()));
            }
        }
    }

    #[test]
    fn spi_context_preserves_recursive_entries_and_suffix_fragments() {
        let mode = parser_seams::RawParseMode::RAW_PARSE_DEFAULT;
        let query = "select\n1/0";
        let line = spi_context_line(query, mode);
        for context in [
            format!("{line}\nPL/pgSQL function recursive_fn() line 1 at EXECUTE"),
            format!("prefix {line}"),
            String::new(),
        ] {
            let mut e = exec_err(types_error::ERRCODE_DIVISION_BY_ZERO, "division by zero".into());
            if !context.is_empty() {
                e.context = Some(context.clone());
            }
            spi_error_context(&mut e, query, mode);
            let expected = if context.is_empty() { line.clone() } else { format!("{context}\n{line}") };
            assert_eq!(e.context.as_deref(), Some(expected.as_str()));
        }
    }

    pub(crate) fn tiny_function() -> PlFunction {
        PlFunction {
            fn_signature: "inline_code_block".into(),
            fn_oid: types_core::InvalidOid,
            fn_xmin: 0,
            fn_tid: (0, 0),
            fn_input_collation: types_core::InvalidOid,
            fn_rettype: 2278, // VOIDOID
            fn_rettyplen: 4,
            fn_retbyval: true,
            fn_retistuple: false,
            fn_retisdomain: false,
            fn_retset: false,
            fn_readonly: false,
            fn_prokind: b'f' as i8,
            fn_is_trigger: FnTrigger::NotTrigger,
            fn_nargs: 0,
            fn_argvarnos: Vec::new(),
            fn_arg_is_input: Vec::new(),
            new_varno: -1,
            old_varno: -1,
            found_varno: -1,
            out_param_varno: -1,
            datums: Vec::new(),
            ns: Vec::new(),
            action: PlBlock {
                lineno: 1,
                label: None,
                body: Vec::new(),
                initvarnos: Vec::new(),
                exceptions: None,
            },
            resolve_option: 0,
            print_strict_params: false,
            nstatements: 0,
            expr_ids: Vec::new(),
        }
    }

    #[test]
    fn exception_conditions_distinguish_success_from_others() {
        let matches = |condition, error| {
            exception_matches_conditions(
                &PgError::error("test".to_string()).with_sqlstate(error),
                &[PlCondition { sqlerrstate: condition, condname: String::new() }],
            )
        };
        let success = types_error::make_sqlstate(*b"00000");
        let division = types_error::ERRCODE_DIVISION_BY_ZERO;
        let others = SqlState(-1);
        assert!(!matches(success, division));
        assert!(matches(others, division));
        assert!(matches(division, division));
        assert!(matches(types_error::make_sqlstate(*b"22000"), division));
        assert!(!matches(types_error::make_sqlstate(*b"22P02"), division));
        for error in [types_error::ERRCODE_QUERY_CANCELED, types_error::ERRCODE_ASSERT_FAILURE] {
            assert!(!matches(others, error));
            assert!(!matches(success, error));
            assert!(matches(error, error));
        }
    }

    // Witness for pl_exec.c:2001-2010's empty-body hangup guard: with the
    // interrupt flag raised, exec_stmts on an EMPTY statement list (a `LOOP`
    // with an empty body iterates exactly this) must reach
    // CHECK_FOR_INTERRUPTS and surface its cancel error — pre-fix, the empty
    // list fell straight through to RC_OK and the loop was unkillable.
    #[test]
    fn empty_stmts_reach_check_for_interrupts() {
        postgres_seams::check_for_interrupts::set(|| {
            Err(Box::new(
                PgError::error("canceling statement due to user request".to_string())
                    .with_sqlstate(types_error::ERRCODE_QUERY_CANCELED),
            ))
        });

        let func = tiny_function();
        let mut estate = Estate::new(&func, false, true).unwrap();

        init_small::globals::SetInterruptPending(false);
        assert_eq!(estate.exec_stmts(&[]).unwrap(), RC_OK, "no interrupt: RC_OK");

        init_small::globals::SetInterruptPending(true);
        let err = estate.exec_stmts(&[]).err().unwrap();
        init_small::globals::SetInterruptPending(false);
        assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    }

    // RAISE literal text must survive byte-for-byte: the old scan loop pushed
    // each UTF-8 byte as a char (Latin-1 widening), so "café" reached the
    // client-encoding conversion as "cafÃ©" and every non-UTF8 client saw
    // mojibake (driver-sweep finding M-1).
    #[test]
    fn raise_message_literal_multibyte_not_widened() {
        let func = tiny_function();
        let mut estate = Estate::new(&func, false, true).unwrap();
        let stmt = PlStmt::Raise {
            lineno: 1,
            elog_level: crate::gram::ELOG_ERROR,
            condname: None,
            message: Some("café %% x".into()),
            params: Vec::new(),
            options: Vec::new(),
        };
        let err = estate.exec_stmt_raise(&stmt).err().unwrap();
        assert_eq!(err.message, "café % x");
        assert_eq!(err.sqlstate, types_error::ERRCODE_RAISE_EXCEPTION);
    }

    // C pl_exec.c:3794-3796: parameter-list underrun is an XX000 elog, not a
    // panic (the old loop indexed params[] unchecked).
    #[test]
    fn raise_message_param_underrun_is_internal_error() {
        let func = tiny_function();
        let mut estate = Estate::new(&func, false, true).unwrap();
        let stmt = PlStmt::Raise {
            lineno: 1,
            elog_level: crate::gram::ELOG_ERROR,
            condname: None,
            message: Some("value: %".into()),
            params: Vec::new(),
            options: Vec::new(),
        };
        let err = estate.exec_stmt_raise(&stmt).err().unwrap();
        assert_eq!(err.sqlstate, types_error::ERRCODE_INTERNAL_ERROR);
        assert_eq!(err.message, "unexpected RAISE parameter list length");
    }

    // Type-confusion guard (finding idx 83): format_expr_params must type each
    // STRICT-error parameter from the DATUM's own declared type (C's
    // exec_eval_datum), never from a re-fetched, swappable plan argtype. This
    // witnesses that param_datum_type reports the Var's declared type — so a
    // mutated EXPR_PLANS entry can never steer the output function onto a
    // wrong-typed raw datum (textout on a by-value int8, etc.).
    #[test]
    fn param_datum_type_comes_from_datum_not_plan() {
        const INT8OID: Oid = 20;
        let plty = PlType {
            typoid: INT8OID,
            ttype: TypeKind::Scalar,
            typlen: 8,
            typbyval: true,
            typtype: b'b' as i8,
            collation: types_core::InvalidOid,
            typisarray: false,
            atttypmod: -1,
            typinput: types_core::InvalidOid,
            typioparam: types_core::InvalidOid,
            rec_ident: None,
        };
        let mut func = tiny_function();
        func.datums.push(PlDatum::Var(PlVar {
            dno: 0,
            refname: "v".into(),
            lineno: 1,
            datatype: plty,
            isconst: false,
            notnull: false,
            default_val: None,
            promise: PROMISE_NONE,
            cursor_explicit_expr: None,
            cursor_explicit_argrow: -1,
            cursor_options: 0,
        }));

        let mut estate = Estate::new(&func, false, true).unwrap();
        assert_eq!(estate.param_datum_type(0).unwrap(), INT8OID);
    }

    // expanded_record_lookup_field + fetch_field over system attributes, and
    // pl_exec.c:5243's refusal to assign them (pre-fix: "has no field").
    #[test]
    fn record_fields_include_system_attributes() {
        // The rec field reads look up pg_type shapes: install the seams here, never rely on a sibling (D2523).
        install_type_seams();
        const INT4OID: Oid = 23;
        const TIDOID: Oid = 27;
        const OIDOID: Oid = 26;
        let mut func = tiny_function();
        func.datums.push(PlDatum::Rec(PlRec {
            dno: 0,
            refname: "r".into(),
            lineno: 1,
            rectypeid: RECORDOID,
            datatype: None,
            isconst: false,
            notnull: false,
            default_val: None,
        }));
        for (dno, name) in [(1, "ctid"), (2, "tableoid"), (3, "nosuch")] {
            func.datums.push(PlDatum::RecField(PlRecField {
                dno,
                recparentno: 0,
                fieldname: name.into(),
            }));
        }
        let mut estate = Estate::new(&func, false, true).unwrap();
        let mcx = estate.datum_mcx();
        let mut td = tupdesc::CreateTemplateTupleDesc(mcx, 1).unwrap();
        tupdesc::TupleDescInitEntry(&mut td, 1, Some("f1"), INT4OID, -1, 0).unwrap();
        let mut tup = heaptuple::heap_form_tuple(mcx, &td, &[Datum::from_i32(7)], &[false]).unwrap();
        tup.t_self = types_tuple::ItemPointerData::new(0, 1);
        tup.t_tableOid = 4242;
        let bound = |sys: Option<RecSysAttrs>| RecValue {
            desc: RecDesc::from_tupdesc(&td).unwrap(),
            values: vec![Datum::from_i32(7)],
            nulls: vec![false],
            owned: vec![false],
            src_desc: Some(std::rc::Rc::new(tupdesc::CreateTupleDescCopy(mcx, &td).unwrap())),
            empty: false,
            fvalue_valid: sys.is_some(),
            sys: sys.map(std::rc::Rc::new),
        };

        estate.datums[0] = DatumVal::Rec(Some(bound(Some(RecSysAttrs::from_tuple(&tup)))));
        let PlDatum::RecField(ctid) = &func.datums[1] else { unreachable!() };
        assert_eq!(estate.recfield_type(ctid).unwrap(), Some((TIDOID, -1, types_core::InvalidOid)));
        let (d, isnull) = estate.datum_as_param(1, Some(TIDOID)).unwrap();
        assert!(!isnull);
        // SAFETY: a tid Datum is a pointer to an ItemPointerData.
        let tid = unsafe { *(d.as_usize() as *const types_tuple::ItemPointerData) };
        assert_eq!(types_tuple::ItemPointerGetBlockNumber(&tid), 0);
        assert_eq!(types_tuple::ItemPointerGetOffsetNumber(&tid), 1);
        assert_eq!(estate.datum_as_param(2, Some(OIDOID)).unwrap(), (Datum::from_oid(4242), false));
        let err = estate.datum_as_param(3, None).unwrap_err();
        assert_eq!(err.message(), "record \"r\" has no field \"nosuch\"");
        let err = estate.exec_assign_value(1, Datum::null(), true, UNKNOWNOID, -1).unwrap_err();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert_eq!(err.message(), "cannot assign to system column \"ctid\"");

        estate.datums[0] = DatumVal::Rec(Some(bound(None)));
        assert_eq!(estate.datum_as_param(1, None).unwrap(), (Datum::null(), true));
    }

    // A field planned as text that now holds an int4 keeps its current type
    // and is not constant (errorhold_d2834_plpgsql_lazy_spi_params: the served
    // run read the text slot as an int); a field still of its planned type is.
    #[test]
    fn a_field_whose_type_changed_since_planning_is_not_constant() {
        install_type_seams();
        const INT4OID: Oid = 23;
        let mut func = tiny_function();
        func.datums.push(PlDatum::Rec(PlRec {
            dno: 0,
            refname: "r".into(),
            lineno: 1,
            rectypeid: RECORDOID,
            datatype: None,
            isconst: false,
            notnull: false,
            default_val: None,
        }));
        func.datums.push(PlDatum::RecField(PlRecField { dno: 1, recparentno: 0, fieldname: "f".into() }));
        let mut estate = Estate::new(&func, false, true).unwrap();
        let mcx = estate.datum_mcx();
        let mut td = tupdesc::CreateTemplateTupleDesc(mcx, 1).unwrap();
        tupdesc::TupleDescInitEntry(&mut td, 1, Some("f"), INT4OID, -1, 0).unwrap();
        estate.datums[0] = DatumVal::Rec(Some(RecValue {
            desc: RecDesc::from_tupdesc(&td).unwrap(),
            values: vec![Datum::from_i32(7)],
            nulls: vec![false],
            owned: vec![false],
            src_desc: Some(std::rc::Rc::new(tupdesc::CreateTupleDescCopy(mcx, &td).unwrap())),
            empty: false,
            fvalue_valid: false,
            sys: None,
        }));
        for speculative in [true, false] {
            let p = estate.setup_param_list(&[1], &[types_core::InvalidOid, TEXTOID], speculative).unwrap();
            assert_eq!((p[1].ptype, p[1].pflags, p[1].value), (INT4OID, 0, Datum::from_i32(7)));
            let p = estate.setup_param_list(&[1], &[types_core::InvalidOid, INT4OID], speculative).unwrap();
            assert_eq!((p[1].ptype, p[1].pflags), (INT4OID, PARAM_FLAG_CONST));
        }
    }

    // Whole-record assignment between recs carries the source tuple's system
    // columns while it is valid; r := r is a no-op; a field assignment stops
    // copies, not direct reads (pre-fix the copy read (4294967295,0)).
    #[test]
    fn whole_record_assignment_carries_the_source_tuple() {
        // The rec field reads look up pg_type shapes: install the seams here, never rely on a sibling (D2523).
        install_type_seams();
        const INT4OID: Oid = 23;
        const TIDOID: Oid = 27;
        const OIDOID: Oid = 26;
        let mut func = tiny_function();
        for (dno, name) in [(0, "src"), (1, "dst")] {
            func.datums.push(PlDatum::Rec(PlRec {
                dno,
                refname: name.into(),
                lineno: 1,
                rectypeid: RECORDOID,
                datatype: None,
                isconst: false,
                notnull: false,
                default_val: None,
            }));
        }
        let fields = [(2, 1, "ctid"), (3, 1, "tableoid"), (4, 0, "f1"), (5, 0, "ctid")];
        for (dno, recparentno, name) in fields {
            func.datums.push(PlDatum::RecField(PlRecField { dno, recparentno, fieldname: name.into() }));
        }
        let mut estate = Estate::new(&func, false, true).unwrap();
        let mcx = estate.datum_mcx();
        let mut td = tupdesc::CreateTemplateTupleDesc(mcx, 1).unwrap();
        tupdesc::TupleDescInitEntry(&mut td, 1, Some("f1"), INT4OID, -1, 0).unwrap();
        let mut tup = heaptuple::heap_form_tuple(mcx, &td, &[Datum::from_i32(7)], &[false]).unwrap();
        tup.t_self = types_tuple::ItemPointerData::new(0, 1);
        tup.t_tableOid = 4242;
        let tuple_identity = &tup as *const _ as usize;
        let mut sys = RecSysAttrs::from_tuple(&tup);
        sys.trigger_tuple = Some(tuple_identity);
        estate.datums[0] = DatumVal::Rec(Some(RecValue {
            desc: RecDesc::from_tupdesc(&td).unwrap(),
            values: vec![Datum::from_i32(7)],
            nulls: vec![false],
            owned: vec![false],
            src_desc: Some(std::rc::Rc::new(tupdesc::CreateTupleDescCopy(mcx, &td).unwrap())),
            empty: false,
            sys: Some(std::rc::Rc::new(sys)),
            fvalue_valid: true,
        }));
        let ctid_of = |estate: &mut Estate<'_>, dno: Dno| -> Option<(u32, u16)> {
            let (d, isnull) = estate.datum_as_param(dno, Some(TIDOID)).unwrap();
            (!isnull).then(|| {
                // SAFETY: a tid Datum is a pointer to an ItemPointerData.
                let tid = unsafe { *(d.as_usize() as *const types_tuple::ItemPointerData) };
                let block = types_tuple::ItemPointerGetBlockNumberNoCheck(&tid);
                (block, types_tuple::ItemPointerGetOffsetNumberNoCheck(&tid))
            })
        };
        // target := source through the param seam, as a simple expression does.
        let assign = |estate: &mut Estate<'_>, target: Dno, source: Dno| {
            let (v, isnull) = estate.datum_as_param(source, None).unwrap();
            let (t, m) = estate.rec_param_type_mod(source).unwrap();
            estate.exec_assign_value(target, v, isnull, t, m).unwrap();
        };

        let original_tuple = |estate: &Estate<'_>, dno: usize| {
            let DatumVal::Rec(Some(rv)) = &estate.datums[dno] else { unreachable!() };
            rv.try_clone().unwrap().original_trigger_tuple(&td)
        };
        assert_eq!(original_tuple(&estate, 0), Some(tuple_identity));
        assign(&mut estate, 1, 0);
        assert_eq!(original_tuple(&estate, 1), Some(tuple_identity));
        estate.exec_assign_value(4, Datum::from_i32(7), false, INT4OID, -1).unwrap();
        assert_eq!(original_tuple(&estate, 0), None);
        assert_eq!(original_tuple(&estate, 1), Some(tuple_identity));
        assert_eq!(ctid_of(&mut estate, 2), Some((0, 1)));
        assert_eq!(estate.datum_as_param(3, Some(OIDOID)).unwrap(), (Datum::from_oid(4242), false));

        estate.exec_assign_value(4, Datum::from_i32(8), false, INT4OID, -1).unwrap();
        assert_eq!(ctid_of(&mut estate, 5), Some((0, 1)));
        assign(&mut estate, 0, 0);
        assert_eq!(ctid_of(&mut estate, 5), Some((0, 1)));
        assign(&mut estate, 1, 0);
        assert_eq!(ctid_of(&mut estate, 2), None);
        assert_eq!(estate.datum_as_param(3, Some(OIDOID)).unwrap(), (Datum::null(), true));
        estate.exec_eval_cleanup();
        assert!(estate.rec_origins.is_empty());
    }

    // upstream 54649de65f08 (18.6): the I/O-coercion fallback never casts to or from internal.
    #[test]
    fn cast_to_or_from_internal_is_42846() {
        install_type_seams();

        let func = tiny_function();
        let mut estate = Estate::new(&func, false, true).unwrap();
        let err = match estate.build_cast_entry(INTERNALOID, -1, TEXTOID, -1) {
            Ok(_) => panic!("internal -> text was given a cast entry"),
            Err(e) => e,
        };
        assert_eq!(err.sqlstate(), types_error::ERRCODE_CANNOT_COERCE);
        assert_eq!(err.message(), "cannot cast type internal to text");
    }

    const TEXTOID: Oid = 25;
    const COMP_OLD: Oid = 7001;
    const COMP_NEW: Oid = 7002;
    const COMP_OLD_REL: Oid = 7101;
    const COMP_NEW_REL: Oid = 7102;
    const NSP: Oid = 7200;

    std::thread_local! {
        static COMP_BY_NAME: core::cell::Cell<Oid> = const { core::cell::Cell::new(COMP_OLD) };
    }

    fn typcache_row(name: &str, typtype: i8, typrelid: Oid) -> syscache_seams::PgTypeTypcacheShape {
        let mut typname = types_tuple::NameData::default();
        typname.namestrcpy(name);
        syscache_seams::PgTypeTypcacheShape {
            typname,
            typlen: 4,
            typbyval: true,
            typalign: b'i' as i8,
            typstorage: b'p' as i8,
            typtype,
            typisdefined: true,
            typrelid,
            typsubscript: types_core::InvalidOid,
            typelem: types_core::InvalidOid,
            typarray: types_core::InvalidOid,
            typcollation: types_core::InvalidOid,
        }
    }

    fn fake_relation_open<'mcx>(
        mcx: Mcx<'mcx>,
        oid: Oid,
        _lockmode: types_rel::LOCKMODE,
    ) -> PgResult<types_rel::Relation<'mcx>> {
        use core::cell::Cell;
        let (reltype, colname) = match oid {
            COMP_OLD_REL => (COMP_OLD, "f1"),
            COMP_NEW_REL => (COMP_NEW, "f0"),
            other => panic!("unexpected relation_open({other})"),
        };
        let mut a = types_tuple::FormData_pg_attribute::default();
        a.attname.namestrcpy(colname);
        a.atttypid = 23;
        a.attnum = 1;
        a.attlen = 4;
        a.attbyval = true;
        a.attalign = b'i' as i8;
        a.atttypmod = -1;
        let mut td = tupdesc::CreateTupleDesc(mcx, &[a])?;
        td.tdtypeid = reltype;
        td.tdtypmod = -1;
        let mut relname = types_tuple::NameData::default();
        relname.namestrcpy("comp");
        let rd_rel = types_rel::FormData_pg_class {
            relname,
            relnamespace: NSP,
            reltype,
            relowner: 10,
            relam: 0,
            relfilenode: 0,
            reltablespace: 0,
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            reltoastrelid: 0,
            relhasindex: false,
            relisshared: false,
            relpersistence: types_core::RELPERSISTENCE_PERMANENT,
            relkind: types_rel::RELKIND_RELATION,
            relhassubclass: false,
            relrowsecurity: false,
            relispopulated: true,
            relreplident: types_rel::REPLICA_IDENTITY_DEFAULT,
            relispartition: false,
            relfrozenxid: 3,
            relminmxid: 1,
        };
        let data = types_rel::RelationData {
            rd_locator: Default::default(),
            rd_smgr: Default::default(),
            rd_id: oid,
            rd_backend: types_core::INVALID_PROC_NUMBER,
            rd_islocaltemp: false,
            rd_isvalid: Cell::new(true),
            rd_createSubid: Cell::new(0),
            rd_newRelfilelocatorSubid: Cell::new(0),
            rd_firstRelfilelocatorSubid: Cell::new(0),
            rd_droppedSubid: Cell::new(0),
            rd_lockInfo: types_rel::LockInfoData {
                lockRelId: types_rel::LockRelId { relId: oid, dbId: 5 },
            },
            rd_rel,
            rd_att: std::rc::Rc::new(td),
            rd_index: None,
            rd_opcintype: mcx::PgVec::new_in(mcx),
            rd_opfamily: mcx::PgVec::new_in(mcx),
            rd_indoption: mcx::PgVec::new_in(mcx),
            rd_indcollation: mcx::PgVec::new_in(mcx),
            rd_options: None,
            pgstat_enabled: Cell::new(false),
            pgstat_link: Cell::new((0, core::ptr::null_mut())),
            rd_amcache: Default::default(),
            rd_amcache_hash: Default::default(),
            rd_amcache_gin: Default::default(),
            rd_amcache_spgist: Default::default(),
            rd_amcache_bloom: Default::default(),
            rd_support: mcx::PgVec::new_in(mcx),
            rd_supportinfo: Default::default(),
            rd_opcoptions: Default::default(),
            rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: false,
            rd_hasrules: false,
        };
        Ok(types_rel::Relation::open(data, None))
    }

    // Seams install once per process: one catalog table for every test.
    pub(crate) fn install_type_seams() {
        static SEAMS: std::sync::Once = std::sync::Once::new();
        SEAMS.call_once(|| {
            syscache_seams::pg_type_typtype::set(|typid| {
                Ok(match typid {
                    RECORDOID => Some(b'p' as i8),
                    23 => Some(b'b' as i8),
                    COMP_OLD | COMP_NEW => Some(b'c' as i8),
                    _ => None,
                })
            });
            syscache_seams::lookup_pg_type_shape::set(|_| {
                Ok(Some(types_tuple::PgTypeShape {
                    typlen: 4,
                    typbyval: true,
                    typalign: b'i' as i8,
                    typstorage: b'p' as i8,
                    typcollation: types_core::InvalidOid,
                }))
            });
            syscache_seams::lookup_pg_type_typcache_shape::set(|typid| {
                Ok(match typid {
                    INTERNALOID => Some(typcache_row("internal", b'p' as i8, types_core::InvalidOid)),
                    TEXTOID => Some(typcache_row("text", b'p' as i8, types_core::InvalidOid)),
                    COMP_OLD => Some(typcache_row("comp", b'c' as i8, COMP_OLD_REL)),
                    COMP_NEW => Some(typcache_row("comp", b'c' as i8, COMP_NEW_REL)),
                    _ => None,
                })
            });
            namespace_seams::type_is_visible::set(|_| Ok(true));
            syscache_seams::syscache_hash_value_typeoid::set(Ok);
            relation_seams::relation_open::set(fake_relation_open);
            relcache_seams::relation_cache_invalidate_entry::set(|_| Ok(()));
            syscache_seams::lookup_pg_namespace_oid_by_name::set(|nsp| {
                Ok(if nsp == "s" { NSP } else { types_core::InvalidOid })
            });
            aclchk_seams::object_aclcheck::set(|_, _, _, _| Ok(0));
            miscinit_seams::get_user_id::set(|| 10);
            syscache_seams::lookup_pg_type_oid_by_name::set(|name, nsp| {
                Ok(if name == "comp" && nsp == NSP {
                    COMP_BY_NAME.with(|c| c.get())
                } else {
                    types_core::InvalidOid
                })
            });
            syscache_seams::pg_type_isdefined::set(|_| Ok(Some(true)));
        });
    }

    // revalidate_rectypeid: the compiled OID stands until a relcache inval
    // clears the entry's identifier; then the name is resolved anew, once.
    #[test]
    fn stale_rowtype_is_re_resolved_by_name() {
        install_type_seams();
        let ident = {
            let tcache = crate::comp::composite_typcache_entry(COMP_OLD).unwrap();
            std::rc::Rc::new(RecTypeIdent {
                origtypname: Some(OrigTypeName::Names(vec!["s".into(), "comp".into()])),
                typoid: core::cell::Cell::new(COMP_OLD),
                tupdesc_id: core::cell::Cell::new(tcache.tupdesc_identifier()),
                tcache: core::cell::RefCell::new(std::rc::Rc::downgrade(&tcache)),
            })
        };
        let cached_id = || ident.tcache.borrow().upgrade().unwrap().tupdesc_identifier();
        let mut func = tiny_function();
        func.datums.push(PlDatum::Rec(PlRec {
            dno: 0,
            refname: "r".into(),
            lineno: 1,
            rectypeid: COMP_OLD,
            datatype: Some(PlType {
                typoid: COMP_OLD,
                ttype: TypeKind::Rec,
                typlen: -1,
                typbyval: false,
                typtype: b'c' as i8,
                collation: types_core::InvalidOid,
                typisarray: false,
                atttypmod: -1,
                typinput: types_core::InvalidOid,
                typioparam: types_core::InvalidOid,
                rec_ident: Some(ident.clone()),
            }),
            isconst: false,
            notnull: false,
            default_val: None,
        }));
        let mut estate = Estate::new(&func, false, true).unwrap();

        estate.revalidate_rectypeid(0).unwrap();
        assert_eq!(estate.rec_typeid(0), COMP_OLD);

        COMP_BY_NAME.with(|c| c.set(COMP_NEW));
        inval::local::LocalExecuteInvalidationMessage(&types_storage::SharedInvalidationMessage::Relcache(
            types_storage::SharedInvalRelcacheMsg { dbId: types_core::InvalidOid, relId: COMP_OLD_REL },
        ))
        .unwrap();
        assert_eq!(cached_id(), 0);
        estate.revalidate_rectypeid(0).unwrap();
        assert_eq!(estate.rec_typeid(0), COMP_NEW);
        assert_eq!(ident.typoid.get(), COMP_NEW);
        assert_ne!(ident.tupdesc_id.get(), 0);
        assert_eq!(cached_id(), ident.tupdesc_id.get());

        COMP_BY_NAME.with(|c| c.set(types_core::InvalidOid));
        estate.revalidate_rectypeid(0).unwrap();
        assert_eq!(estate.rec_typeid(0), COMP_NEW);

        let mut estate2 = Estate::new(&func, false, true).unwrap();
        assert_eq!(estate2.rec_typeid(0), COMP_OLD);
        estate2.revalidate_rectypeid(0).unwrap();
        assert_eq!(estate2.rec_typeid(0), COMP_NEW);
    }

    // A small array deformed out of a heap tuple arrives short-headed;
    // DatumGetArrayTypePCopy takes every varlena form.
    #[test]
    fn foreach_array_copy_accepts_short_header_datum() {
        const INT4OID: Oid = 23;
        let ctx = MemoryContext::new("foreach-test");
        let mcx = ctx.mcx();
        let values = [Datum::from_i32(1), Datum::from_i32(2), Datum::from_i32(3)];
        let full = arrayfuncs::construct::construct_md_array(
            mcx, &values, None, 1, &[3], &[1], INT4OID, 4, true, b'i',
        )
        .unwrap();
        let payload = &full[datum::VARHDRSZ..];
        let mut short: Vec<u8> = Vec::with_capacity(1 + payload.len());
        short.push((((1 + payload.len()) as u8) << 1) | 0x01);
        short.extend_from_slice(payload);
        let copy = array_datum_copy(mcx, Datum::from_usize(short.as_ptr() as usize)).unwrap();
        assert_eq!(&copy[..], &full[..]);
        let copy4 = array_datum_copy(mcx, Datum::from_usize(full.as_ptr() as usize)).unwrap();
        assert_eq!(&copy4[..], &full[..]);
    }

    // The snapshot step precedes any plan use: with neither a snapshot nor
    // a portal it is C's elog, while reaching the plan reports "not simple".
    #[test]
    fn simple_expr_restores_portal_snapshot_before_plan_use() {
        let func = tiny_function();
        let mut estate = Estate::new(&func, false, false).unwrap();
        let expr = PlExpr {
            query: "1".into(),
            parse_mode: parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_EXPR,
            ns: 0,
            expr_id: crate::comp::alloc_expr_id(),
            target_param: -1,
            target_is_local: false,
        };
        EXPR_PLANS.with(|t| {
            t.borrow_mut().insert(
                expr.expr_id,
                PlanEntry {
                    plan: SpiPlanPtr(u64::MAX),
                    lex: LexSettings::current(),
                    paramnos: std::rc::Rc::from(Vec::<Dno>::new()),
                    argtypes: std::rc::Rc::from(Vec::<Oid>::new()),
                    mod_stmt: false,
                    hooks: std::rc::Rc::new(HookSnapshot {
                        names: Vec::new(),
                        params_by_dno: Vec::new(),
                        arg_dnos: Vec::new(),
                        recs: Vec::new(),
                        valueless: Vec::new(),
                        recfields: Vec::new(),
                        resolve_option: parser_small1::PlpgsqlResolveOption::Error,
                    }),
                    simple: SimpleState::Unknown,
                },
            );
        });
        let r = estate.exec_eval_simple_expr(&expr);
        EXPR_PLANS.with(|t| t.borrow_mut().remove(&expr.expr_id));
        crate::comp::release_expr_ids(&[expr.expr_id]);
        let err = r.err().expect("no portal snapshot can be re-established here");
        assert_eq!(err.message(), "cannot execute SQL without an outer snapshot or portal");
    }
}

#[cfg(test)]
mod var_assign_tests {
    use super::*;

    fn scalar_type(typoid: Oid, typlen: i16, typbyval: bool) -> PlType {
        PlType {
            typoid,
            ttype: TypeKind::Scalar,
            typlen,
            typbyval,
            typtype: b'b' as i8,
            collation: types_core::InvalidOid,
            typisarray: false,
            atttypmod: -1,
            typinput: types_core::InvalidOid,
            typioparam: types_core::InvalidOid,
            rec_ident: None,
        }
    }

    fn var(dno: Dno, name: &str, ty: PlType) -> PlDatum {
        PlDatum::Var(PlVar {
            dno,
            refname: name.into(),
            lineno: 1,
            datatype: ty,
            isconst: false,
            notnull: false,
            default_val: None,
            cursor_explicit_expr: None,
            cursor_explicit_argrow: -1,
            cursor_options: 0,
            promise: PROMISE_NONE,
        })
    }

    // assign_simple_var (pl_exec.c:8838): an assignment cancels the TG_*
    // promise, so the next read returns the assigned value.
    #[test]
    fn assignment_disarms_a_pending_promise() {
        let mut func = super::cfi_tests::tiny_function();
        func.datums = vec![var(0, "tg_nargs", scalar_type(23, 4, true))];
        if let PlDatum::Var(v) = &mut func.datums[0] {
            v.promise = PROMISE_TG_NARGS;
        }
        let mut estate = Estate::new(&func, false, true).unwrap();
        estate.promise_pending = vec![0];
        estate.set_var(0, Datum::from_i32(42), false, false);
        assert!(estate.promise_pending.is_empty());
        let (v, isnull) = estate.get_var(0).unwrap();
        assert!(!isnull);
        assert_eq!(v.as_i32(), 42);
    }

    // exec_stmt_case's retype frees the old by-ref copy under its own
    // type: a text image released as a 64-byte name overcharged the
    // datum context.
    #[test]
    fn retype_releases_the_old_value_under_its_own_type() {
        let text = scalar_type(25, -1, false);
        let name = scalar_type(19, 64, false);
        let mut func = super::cfi_tests::tiny_function();
        func.datums = vec![var(0, "t", text)];
        let mut estate = Estate::new(&func, false, true).unwrap();
        let src = varlena::cstring_to_text(estate.eval_ctx.mcx(), b"ab").unwrap();
        let d = fmgr::varlena_result(src);
        let stored = estate.assign_copy_to_datum_ctx(d, false, -1, false).unwrap();
        estate.set_var(0, stored, false, true);
        estate.retype_var(0, name);
        let (_, isnull) = estate.get_var(0).unwrap();
        assert!(isnull);
        assert_eq!(estate.var_type(0).typoid, 19);
    }
}

#[cfg(test)]
mod param_fetch_tests {
    use super::*;
    use types_nodes::nodes_enums::CmdType;
    use types_nodes::parsenodes::Query;
    use types_nodes::primnodes::{Param, ParamKind, TargetEntry};
    use types_nodes::{Node, NodeList};

    fn param<'mcx>(m: mcx::Mcx<'mcx>, dno: Dno) -> Node<'mcx> {
        Node::mk(
            m,
            Param {
                paramkind: ParamKind::PARAM_EXTERN,
                paramid: dno + 1,
                paramtype: 23,
                paramtypmod: -1,
                paramcollid: types_core::InvalidOid,
                location: -1,
            },
        )
        .unwrap()
    }

    fn select_of<'mcx>(m: mcx::Mcx<'mcx>, expr: Node<'mcx>) -> Query<'mcx> {
        let tle = Node::mk(
            m,
            TargetEntry {
                expr,
                resno: 1,
                resname: None,
                ressortgroupref: 0,
                resorigtbl: 0,
                resorigcol: 0,
                resjunk: false,
            },
        )
        .unwrap();
        Query {
            commandType: CmdType::CMD_SELECT,
            targetList: NodeList::make1(m, tle).unwrap(),
            ..Default::default()
        }
    }

    // `r.f1 := $1` analyzes to `SELECT $1`: the target (dno 3, resolved
    // first) has no Param, so it is not fetched (plpgsql_cache's RETURN
    // reads it); a target its own source reads stays.
    #[test]
    fn unread_assignment_target_is_not_fetched() {
        let cx = mcx::MemoryContext::new("plpgsql param walk test");
        let m = cx.mcx();
        let mut used = vec![3, 0];
        drop_unread_assign_target(&mut used, &[select_of(m, param(m, 0))]).unwrap();
        assert_eq!(used, vec![0]);

        let mut used = vec![3, 0];
        drop_unread_assign_target(&mut used, &[select_of(m, param(m, 3))]).unwrap();
        assert_eq!(used, vec![3, 0]);
        assert_eq!(referenced_param_dnos(&[select_of(m, param(m, 3))]).unwrap(), vec![3]);
    }

    // Execute-path param fetch failures carry the SQL statement line
    // (plpgsql_cache's `select pg_typeof(r.a) into t`); the bare fetch does not.
    #[test]
    fn spi_param_setup_errors_carry_the_statement_context() {
        let mut func = super::cfi_tests::tiny_function();
        func.datums = vec![
            PlDatum::Rec(PlRec {
                dno: 0,
                refname: "r".into(),
                lineno: 1,
                rectypeid: RECORDOID,
                datatype: None,
                isconst: false,
                notnull: false,
                default_val: None,
            }),
            PlDatum::RecField(PlRecField { dno: 1, recparentno: 0, fieldname: "a".into() }),
        ];
        let mut estate = Estate::new(&func, false, true).unwrap();
        let expr = PlExpr {
            query: "select pg_typeof(r.a)".into(),
            parse_mode: parser_seams::RawParseMode::RAW_PARSE_DEFAULT,
            ns: 0,
            expr_id: 0,
            target_param: -1,
            target_is_local: false,
        };
        let argtypes = [types_core::InvalidOid, 23];
        let err = estate.setup_param_list_under_spi(&expr, &[1], &argtypes, false).unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE);
        assert_eq!(err.context.as_deref(), Some("SQL statement \"select pg_typeof(r.a)\""));
        let err = estate.setup_param_list(&[1], &argtypes, false).unwrap_err();
        assert_eq!(err.context, None);
        // The execute path's speculative fetch (pl_exec.c:6335) leaves the
        // field without a value and hands SPI the fetch for evaluation time.
        let params = estate.setup_param_list(&[1], &argtypes, true).unwrap();
        assert_eq!((params[1].ptype, params[1].pflags), (types_core::InvalidOid, 0));
        let mut arg = estate.spi_fetch_arg(&std::rc::Rc::from(vec![1]));
        let list = spi_param_list(&params, &mut arg);
        let fetch = list.fetch.expect("a slot without a value brings the fetch");
        let err = unsafe { (fetch.func)(fetch.arg, 2) }.unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE);
        let dummy = unsafe { (fetch.func)(fetch.arg, 1) }.unwrap();
        assert_eq!(dummy.ptype, types_core::InvalidOid, "datum 0 is not one of the paramnos");
    }
}

#[cfg(test)]
mod simple_expr_tests {
    use super::*;
    use types_nodes::nodes_enums::CmdType;
    use types_nodes::plannodes::{Gather, Material, Plan, PlannedStmt, Result};
    use types_nodes::primnodes::{Const, TargetEntry, Var, OUTER_VAR};
    use types_nodes::{Node, NodeList};

    fn tlist(m: mcx::Mcx<'static>, expr: Node<'static>) -> NodeList<'static> {
        let tle = Node::mk(
            m,
            TargetEntry {
                expr,
                resno: 1,
                resname: None,
                ressortgroupref: 0,
                resorigtbl: 0,
                resorigcol: 0,
                resjunk: false,
            },
        )
        .unwrap();
        NodeList::make1(m, tle).unwrap()
    }

    fn int_const(m: mcx::Mcx<'static>) -> Node<'static> {
        Node::mk(
            m,
            Const {
                consttype: 23,
                consttypmod: -1,
                constcollid: types_core::InvalidOid,
                constlen: 4,
                constvalue: Datum::from_i32(3),
                constisnull: false,
                constbyval: true,
                location: -1,
            },
        )
        .unwrap()
    }

    fn var(m: mcx::Mcx<'static>, varno: i32) -> Node<'static> {
        Node::mk(m, Var { varno, varattno: 1, vartype: 23, vartypmod: -1, ..Default::default() })
            .unwrap()
    }

    fn result(m: mcx::Mcx<'static>, expr: Node<'static>) -> Node<'static> {
        Node::mk(
            m,
            Result {
                plan: Plan { targetlist: tlist(m, expr), ..Default::default() },
                resconstantqual: None,
            },
        )
        .unwrap()
    }

    fn gather(m: mcx::Mcx<'static>, tle: Node<'static>, child: Node<'static>) -> Node<'static> {
        Node::mk(
            m,
            Gather {
                plan: Plan { targetlist: tlist(m, tle), lefttree: Some(child), ..Default::default() },
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn stmt(plan: Node<'static>) -> PlannedStmt<'static> {
        PlannedStmt { commandType: CmdType::CMD_SELECT, planTree: Some(plan), ..Default::default() }
    }

    fn m() -> mcx::Mcx<'static> {
        Box::leak(Box::new(mcx::MemoryContext::new("plpgsql simple expr test"))).mcx()
    }

    // exec_save_simple_expr (pl_exec.c:8300-8343): a Gather (debug_parallel_query)
    // or Material (scrollable cursor) atop the Result is looked through to the
    // Result's expression; a Const setrefs.c copied up is taken as is; anything
    // but an outer Var in the wrapper's tlist is not simple.
    #[test]
    fn looks_through_gather_and_material_to_the_result_expression() {
        let m = m();
        let c = int_const(m);
        let (expr, t, _) = simple_result_expr(&stmt(result(m, c))).expect("bare Result");
        assert!(expr.as_variant::<Const>().is_some() && t == 23);

        let wrapped = gather(m, var(m, OUTER_VAR), result(m, c));
        let (expr, t, _) = simple_result_expr(&stmt(wrapped)).expect("Gather over Result");
        assert!(expr.as_variant::<Const>().is_some() && t == 23);

        let material = Node::mk(
            m,
            Material { plan: Plan { targetlist: tlist(m, var(m, OUTER_VAR)), lefttree: Some(wrapped), ..Default::default() } },
        )
        .unwrap();
        let (expr, _, _) = simple_result_expr(&stmt(material)).expect("Material over Gather");
        assert!(expr.as_variant::<Const>().is_some());

        let copied_up = gather(m, int_const(m), result(m, var(m, OUTER_VAR)));
        let (expr, _, _) = simple_result_expr(&stmt(copied_up)).expect("Const copied up");
        assert!(expr.as_variant::<Const>().is_some());

        assert!(simple_result_expr(&stmt(gather(m, var(m, 1), result(m, c)))).is_none());
    }
}

#[cfg(test)]
mod d4312_tests {
    /// One task's compiles: `n` expression ids its functions hold until the thread's exit drain.
    fn task_ids(n: usize) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..n).map(|_| crate::comp::alloc_expr_id()).collect();
        ids.sort_unstable();
        ids
    }

    /// D4312: a retained bgworker standby runs task after task on one thread, and each task's exit drain
    /// (release_plpgsql_state_at_exit) drops every function, plan, CALL target and simple-expression carry that an
    /// expression id keys. So the next task reissues the same ids, rather than counting on across tasks until
    /// ExprSlab::insert's bound (2^20 past the slab) panics.
    #[test]
    fn a_retained_threads_next_task_reissues_its_expression_ids() {
        super::release_plpgsql_state_at_exit(0, 0);
        let first = task_ids(1000);
        super::release_plpgsql_state_at_exit(0, 0);
        let second = task_ids(1000);
        assert_eq!(second.first(), first.first(), "the next task's ids start where the first task's did");
        assert_eq!(second, first, "a retained thread's next task reissues the ids its exit drain freed");
    }
}
