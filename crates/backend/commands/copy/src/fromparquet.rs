// COPY ... FROM 'file' WITH (FORMAT 'parquet') — the generic row-producing
// path: parquet column batches -> datums -> the existing COPY insert path,
// so it loads into ANY table AM. pg_parquet (the behavioral spec of record
// for this surface) drives the option names (match_by position|name), the
// schema-matching error classes and the type-mapping table; this increment
// implements the flat-column read side and errors cleanly on the rest.
//
// The columnar-store fast path (dictionary remap into part builders, sorted
// runs into the parallel-load merge) is a later increment; parquet_read's
// batch API already separates page decode from batch fill for it.

use datum::Datum;
use mcx::Mcx;
use parquet_read::{
    BatchData, ColumnBatch, FileReader, Logical, Phys, Repetition, RowGroupReader, TimeUnit,
};
use types_core::Oid;
use types_fmgr::FmgrInfo;
use types_error::{
    PgError, PgResult, ERRCODE_BAD_COPY_FILE_FORMAT, ERRCODE_DATATYPE_MISMATCH,
    ERRCODE_DATETIME_VALUE_OUT_OF_RANGE, ERRCODE_FEATURE_NOT_SUPPORTED,
    ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, ERRCODE_STRING_DATA_RIGHT_TRUNCATION,
    ERRCODE_UNDEFINED_COLUMN,
};
use types_tuple::TupleDescData;

use crate::from::CopyFromState;

// Microseconds from the Unix epoch to the engine timestamp epoch (2000-01-01).
const EPOCH_SHIFT_US: i64 = 946_684_800_000_000;
// The same shift in whole seconds (epoch-coercion input unit).
const EPOCH_SHIFT_SECS: i64 = 946_684_800;
// Days from the Unix epoch to the engine date epoch.
const EPOCH_SHIFT_DAYS: i32 = 10_957;
// IS_VALID_DATE (datetime.h): [DATETIME_MIN_JULIAN, DATE_END_JULIAN) - POSTGRES_EPOCH_JDATE.
const MIN_DATE_DAYS: i32 = -2_451_545;
const END_DATE_DAYS: i32 = 2_145_031_949;
// Engine timestamp validity window (timestamp.h IS_VALID_TIMESTAMP).
const MIN_TIMESTAMP_US: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP_US: i64 = 9_223_371_331_200_000_000;
const MAX_TIME_US: i64 = 86_400_000_000;

const VARHDRSZ_I32: i32 = 4;
// pg_type.h; types_core::catalog carries no uuid constant.
const UUIDOID: Oid = 2950;

/// How one bound column's decoded values become datums.
#[derive(Clone, Copy)]
pub(crate) enum Conv {
    Bool,
    I16FromI32,
    I32,
    I64FromI32,
    I16FromI64,
    I32FromI64,
    I64,
    F32,
    F64FromF32,
    F64,
    DateFromDays,
    /// Timestamp/timestamptz target; value scaled to microseconds.
    Timestamp(TimeUnit),
    Time(TimeUnit),
    /// Unsigned annotations: zero-extend / reinterpret with range checks.
    U32AsI32,
    U32AsI64,
    U64AsI64,
    /// COERCE_EPOCH: plain-integer column read as Unix epoch seconds into a
    /// timestamp/timestamptz target.
    TsFromEpochSecs(EpochLane),
    /// COERCE_EPOCH: plain-integer column read as Unix epoch days into a
    /// date target.
    DateFromEpochDays(EpochLane),
    /// text / unconstrained varchar (UTF-8 validated at page decode).
    Text,
    /// varchar(n): char-length check with the blank-trim rule.
    VarcharN(i32),
    /// bpchar: the varchar rule + space padding to exactly n chars
    /// (-1 = unconstrained, stored as-is).
    Bpchar(i32),
    Bytea,
    /// TIME(MILLIS) on INT32 -> time.
    TimeMillis32,
    /// INT96 (legacy Impala/Hive nanosecond timestamp) -> timestamp(tz).
    TsFromInt96,
    /// FLOAT16 (FLBA(2), IEEE half) -> float4 / float8.
    F32FromF16,
    F64FromF16,
    /// DECIMAL(p, s) with scale 0 -> int2/int4/int8 (exact, range-checked).
    DecimalToInt(DecimalSrc, IntWidth),
    /// DECIMAL(p, s) -> float4 / float8 (unscaled / 10^s through the
    /// decimal-string parser, so rounding is the float parser's).
    DecimalToF32(DecimalSrc, i16),
    DecimalToF64(DecimalSrc, i16),
    /// Arms that build a text image and call the target type's input
    /// function ([`CellInput`]): parity with COPY text for numeric, uuid,
    /// interval, json and jsonb. Serial path only (needs a memory context).
    ViaInput(InputForm),
    /// LIST<scalar> -> one-dimensional array of the element conversion held
    /// on the binding ([`ListBind`]). Serial path only.
    List,
}

/// Where a DECIMAL's unscaled integer is stored.
#[derive(Clone, Copy)]
pub(crate) enum DecimalSrc {
    I32,
    I64,
    /// FLBA / BYTE_ARRAY: big-endian two's complement of any length.
    Bytes,
}

#[derive(Clone, Copy)]
pub(crate) enum IntWidth {
    I16,
    I32,
    I64,
}

/// The text image an input-function arm renders from the decoded cell.
#[derive(Clone, Copy)]
pub(crate) enum InputForm {
    /// DECIMAL(p, s) -> numeric: the unscaled integer with the point inserted
    /// (the scale narrowed to i16 at plan time so `Conv` stays one word).
    Decimal(DecimalSrc, i16),
    /// UUID (FLBA(16)) -> uuid: the canonical hex form.
    Uuid,
    /// INTERVAL (FLBA(12): months, days, millis, little-endian u32) -> interval.
    Interval,
    /// JSON / STRING / unannotated BYTE_ARRAY -> json / jsonb: the bytes as
    /// given (UTF-8 verified at page decode; the input function parses).
    Json,
}

const _: () = assert!(std::mem::size_of::<Conv>() <= 8);

impl Conv {
    /// Arms the raw (memory-context-free) parallel capture cannot build; the
    /// pgrc driver's admission refuses a plan holding one.
    pub(crate) fn serial_only(self) -> bool {
        matches!(self, Conv::ViaInput(_) | Conv::List)
    }
}

/// How an epoch-coerced integer lane widens to a signed 64-bit epoch number
/// (same widening rules as the plain-integer target mappings).
#[derive(Clone, Copy)]
pub(crate) enum EpochLane {
    /// INT32, signed annotation or none.
    I32,
    /// INT32 with an unsigned annotation: zero-extend.
    U32,
    /// INT64, signed / narrow-unsigned annotation or none.
    I64,
    /// INT64 annotated UINT_64: sign-bit values are out of range.
    U64,
}

pub(crate) struct Binding {
    /// 0-based physical attribute index in the target tuple descriptor.
    pub(crate) attidx: usize,
    pub(crate) conv: Conv,
    /// Dense index into the per-binding batch array.
    pub(crate) batch: usize,
    /// Input function of the target type, for the `ViaInput` arm (and the
    /// element type of a `List`).
    pub(crate) input: Option<InputSpec>,
    /// The `List` arm's element facts.
    pub(crate) list: Option<ListBind>,
}

#[derive(Clone, Copy)]
pub(crate) struct InputSpec {
    pub(crate) typinput: Oid,
    pub(crate) typioparam: Oid,
    pub(crate) typmod: i32,
}

/// LIST<scalar> facts resolved at plan time: the element conversion plus
/// the definition levels that separate NULL list / empty list / null
/// element / element (Dremel: the LIST group's own level, the repeated
/// group's, the leaf's).
#[derive(Clone, Copy)]
pub(crate) struct ListBind {
    pub(crate) elem: Conv,
    pub(crate) elmtype: Oid,
    pub(crate) elmlen: i16,
    pub(crate) elmbyval: bool,
    pub(crate) elmalign: u8,
    /// Definition level at which the LIST-annotated group is present.
    pub(crate) list_def: u16,
    /// Definition level of a present (non-null) element (`max_def`).
    pub(crate) elem_def: u16,
    /// `elem_def - 1` marks a null element (leaf OPTIONAL); otherwise no
    /// level between `list_def` and `elem_def` is legal.
    pub(crate) elem_nullable: bool,
}

/// One binding's resolved input function (serial path; built per source,
/// never shared across workers).
pub(crate) struct CellInput {
    flinfo: FmgrInfo,
    typioparam: Oid,
    typmod: i32,
}

impl CellInput {
    fn new(spec: InputSpec) -> PgResult<CellInput> {
        Ok(CellInput {
            flinfo: fmgr_core::fmgr_info(spec.typinput)?,
            typioparam: spec.typioparam,
            typmod: spec.typmod,
        })
    }
}

/// The resolved schema-match + conversion plan: shareable across parallel
/// decode workers (plain data).
pub(crate) struct ParquetPlan {
    pub(crate) bindings: Vec<Binding>,
    /// Parquet schema ordinals + validation flags, in binding order (the
    /// row-group open feed).
    pub(crate) cols: Vec<usize>,
    pub(crate) vutf8: Vec<bool>,
}

impl ParquetPlan {
    pub(crate) fn make_batches(&self, meta: &parquet_read::FileMeta) -> Vec<ColumnBatch> {
        self.cols.iter().map(|&c| ColumnBatch::new_for(meta.columns[c].phys)).collect()
    }
}

pub(crate) struct ParquetSrc {
    /// CONVERT wave 3 (cvwrite job 3): the load's owner's charge on `batches`, brought to them after each fill (the
    /// row groups open on `owner` too). Declared first: it drops before the batches are freed.
    batch_charge: pgrc_write::held::HeldCharge,
    /// The loading statement's owner (`pgrc_am::ingest::load_owner`, read at the open on the backend's thread).
    owner: Option<memledger::Owner>,
    reader: FileReader,
    plan: std::sync::Arc<ParquetPlan>,
    batches: Vec<ColumnBatch>,
    batch_len: usize,
    batch_pos: usize,
    rg_idx: usize,
    rg: Option<RowGroupReader>,
    /// Per binding: the resolved input function (`ViaInput` / `List`).
    inputs: Vec<Option<CellInput>>,
    /// Per binding: for a `List`, the batch slot where each record starts
    /// (rep == 0 positions) plus a final end sentinel; empty otherwise.
    list_starts: Vec<Vec<u32>>,
    /// D4361: set by a failed [`ParquetSrc::refill`] whose error is the
    /// reader's column UTF-8 check: how many rows past the batch start the
    /// first invalid value sits (the check fails for the page it decodes,
    /// so the error alone names the batch start, not the value's row).
    bad_row_delta: Option<u64>,
}

const BATCH_ROWS: usize = 1024;

#[track_caller]
#[cold]
#[inline(never)]
fn type_mismatch(attname: &str, pg_type: &str, pq_type: &str) -> Box<PgError> {
    // pg_parquet's error class for an uncoercible column.
    Box::new(
        PgError::error(format!(
            "type mismatch for column \"{attname}\" between table and parquet file"
        ))
        .with_sqlstate(ERRCODE_DATATYPE_MISMATCH)
        .with_detail(format!("table has \"{pg_type}\", parquet file has \"{pq_type}\"")),
    )
}

/// Resolve the conversion for one (parquet column, table attribute) pair.
/// The mapped set is pg_parquet's read-side table restricted to flat
/// columns; anything else is a clean type-mismatch error.
fn resolve_conv(
    schema: &parquet_read::ColumnSchema,
    atttypid: Oid,
    atttypmod: i32,
    attname: &str,
    coerce_epoch: bool,
) -> PgResult<Conv> {
    use types_core::catalog::*;
    let phys = schema.phys;
    let logical = schema.logical;
    let plain_int = |l: Logical, bits: u8| -> bool {
        match l {
            Logical::None => true,
            Logical::Int { bits: b, signed: true } => b <= bits,
            _ => false,
        }
    };
    // Unsigned annotations up to `bits` wide: values are zero-extended into
    // the physical int, so a signed read of a STRICTLY NARROWER unsigned
    // width is always non-negative and in range.
    let small_uint = |l: Logical, bits: u8| -> bool {
        matches!(l, Logical::Int { bits: b, signed: false } if b < bits)
    };
    let textual = |l: Logical| {
        matches!(l, Logical::None | Logical::String | Logical::Enum | Logical::Json)
    };
    // COERCE_EPOCH admits exactly the plain-integer annotation classes (no
    // temporal/other logical); annotated temporal columns keep their mapping.
    let plain_integer = |l: Logical| matches!(l, Logical::None | Logical::Int { .. });
    let lane32 = |l: Logical| match l {
        Logical::Int { signed: false, .. } => EpochLane::U32,
        _ => EpochLane::I32,
    };
    let lane64 = |l: Logical| match l {
        Logical::Int { bits: 64, signed: false } => EpochLane::U64,
        _ => EpochLane::I64,
    };
    let conv = match (phys, atttypid) {
        (Phys::Boolean, BOOLOID) if logical == Logical::None => Some(Conv::Bool),
        (Phys::Int32, INT2OID) if plain_int(logical, 16) => Some(Conv::I16FromI32),
        (Phys::Int32, INT4OID) if plain_int(logical, 32) => Some(Conv::I32),
        (Phys::Int32, INT8OID) if plain_int(logical, 32) => Some(Conv::I64FromI32),
        (Phys::Int64, INT2OID) if plain_int(logical, 64) => Some(Conv::I16FromI64),
        (Phys::Int64, INT4OID) if plain_int(logical, 64) => Some(Conv::I32FromI64),
        (Phys::Int64, INT8OID) if plain_int(logical, 64) => Some(Conv::I64),
        // Unsigned annotations (range-checked; UINT_16 day-number columns
        // are the workload-class instance).
        (Phys::Int32, INT2OID) if small_uint(logical, 32) => Some(Conv::I16FromI32),
        (Phys::Int32, INT4OID) if small_uint(logical, 32) => Some(Conv::I32),
        (Phys::Int32, INT8OID) if small_uint(logical, 32) => Some(Conv::I64FromI32),
        (Phys::Int32, INT4OID) if logical == (Logical::Int { bits: 32, signed: false }) => {
            Some(Conv::U32AsI32)
        }
        (Phys::Int32, INT8OID) if logical == (Logical::Int { bits: 32, signed: false }) => {
            Some(Conv::U32AsI64)
        }
        (Phys::Int64, INT2OID) if small_uint(logical, 64) => Some(Conv::I16FromI64),
        (Phys::Int64, INT4OID) if small_uint(logical, 64) => Some(Conv::I32FromI64),
        (Phys::Int64, INT8OID) if small_uint(logical, 64) => Some(Conv::I64),
        (Phys::Int64, INT8OID) if logical == (Logical::Int { bits: 64, signed: false }) => {
            Some(Conv::U64AsI64)
        }
        (Phys::Float, FLOAT4OID) if logical == Logical::None => Some(Conv::F32),
        (Phys::Float, FLOAT8OID) if logical == Logical::None => Some(Conv::F64FromF32),
        (Phys::Double, FLOAT8OID) if logical == Logical::None => Some(Conv::F64),
        (Phys::Int32, DATEOID) if logical == Logical::Date => Some(Conv::DateFromDays),
        // COERCE_EPOCH (opt-in, temporal targets only): plain-integer
        // columns read as Unix epoch days into date...
        (Phys::Int32 | Phys::Int64, DATEOID) if coerce_epoch && plain_integer(logical) => {
            Some(Conv::DateFromEpochDays(match phys {
                Phys::Int32 => lane32(logical),
                _ => lane64(logical),
            }))
        }
        (Phys::Int64, TIMESTAMPOID | TIMESTAMPTZOID) => match logical {
            // Instant vs wall-clock annotation maps to timestamptz vs
            // timestamp on the write side; reads accept either target (the
            // datum representation is the same microsecond count).
            Logical::Timestamp { unit, .. } => Some(Conv::Timestamp(unit)),
            // ...and as Unix epoch seconds into timestamp/timestamptz (the
            // annotated arm above always wins over coercion).
            _ if coerce_epoch && plain_integer(logical) => {
                Some(Conv::TsFromEpochSecs(lane64(logical)))
            }
            _ => None,
        },
        (Phys::Int32, TIMESTAMPOID | TIMESTAMPTZOID)
            if coerce_epoch && plain_integer(logical) =>
        {
            Some(Conv::TsFromEpochSecs(lane32(logical)))
        }
        (Phys::Int64, TIMEOID) => match logical {
            Logical::Time { unit } => Some(Conv::Time(unit)),
            _ => None,
        },
        (Phys::ByteArray, TEXTOID) if textual(logical) => Some(Conv::Text),
        (Phys::ByteArray, VARCHAROID) if textual(logical) => {
            if atttypmod >= VARHDRSZ_I32 {
                Some(Conv::VarcharN(atttypmod - VARHDRSZ_I32))
            } else {
                Some(Conv::Text)
            }
        }
        (Phys::ByteArray, BPCHAROID) if textual(logical) => {
            if atttypmod >= VARHDRSZ_I32 {
                Some(Conv::Bpchar(atttypmod - VARHDRSZ_I32))
            } else {
                Some(Conv::Bpchar(-1))
            }
        }
        // BSON and every other BYTE_ARRAY / FLBA annotation store as raw bytes.
        (Phys::ByteArray | Phys::Flba(_), BYTEAOID) => Some(Conv::Bytea),
        (Phys::Int32, TIMEOID) if logical == (Logical::Time { unit: TimeUnit::Millis }) => {
            Some(Conv::TimeMillis32)
        }
        (Phys::Int96, TIMESTAMPOID | TIMESTAMPTZOID) => Some(Conv::TsFromInt96),
        (Phys::Flba(2), FLOAT4OID) if logical == Logical::Float16 => Some(Conv::F32FromF16),
        (Phys::Flba(2), FLOAT8OID) if logical == Logical::Float16 => Some(Conv::F64FromF16),
        (Phys::Flba(16), UUIDOID) if logical == Logical::Uuid => {
            Some(Conv::ViaInput(InputForm::Uuid))
        }
        (Phys::Flba(12), INTERVALOID) if logical == Logical::Interval => {
            Some(Conv::ViaInput(InputForm::Interval))
        }
        (Phys::ByteArray, JSONOID | JSONBOID) if textual(logical) => {
            Some(Conv::ViaInput(InputForm::Json))
        }
        (Phys::Int32 | Phys::Int64 | Phys::ByteArray | Phys::Flba(_), NUMERICOID)
            if matches!(logical, Logical::Decimal { .. }) =>
        {
            let Logical::Decimal { scale, .. } = logical else { unreachable!() };
            i16::try_from(scale).ok().map(|s| Conv::ViaInput(InputForm::Decimal(decimal_src(phys), s)))
        }
        (Phys::Int32 | Phys::Int64 | Phys::ByteArray | Phys::Flba(_), INT2OID | INT4OID | INT8OID)
            if matches!(logical, Logical::Decimal { scale: 0, .. }) =>
        {
            let width = match atttypid {
                INT2OID => IntWidth::I16,
                INT4OID => IntWidth::I32,
                _ => IntWidth::I64,
            };
            Some(Conv::DecimalToInt(decimal_src(phys), width))
        }
        (Phys::Int32 | Phys::Int64 | Phys::ByteArray | Phys::Flba(_), FLOAT4OID | FLOAT8OID)
            if matches!(logical, Logical::Decimal { .. }) =>
        {
            let Logical::Decimal { scale, .. } = logical else { unreachable!() };
            i16::try_from(scale).ok().map(|s| match atttypid {
                FLOAT4OID => Conv::DecimalToF32(decimal_src(phys), s),
                _ => Conv::DecimalToF64(decimal_src(phys), s),
            })
        }
        _ => None,
    };
    match conv {
        Some(c) => Ok(c),
        None => Err(no_binding(schema, atttypid, atttypmod, attname)),
    }
}

fn decimal_src(phys: Phys) -> DecimalSrc {
    match phys {
        Phys::Int32 => DecimalSrc::I32,
        Phys::Int64 => DecimalSrc::I64,
        _ => DecimalSrc::Bytes,
    }
}

#[cold]
#[inline(never)]
fn no_binding(
    schema: &parquet_read::ColumnSchema,
    atttypid: Oid,
    atttypmod: i32,
    attname: &str,
) -> Box<PgError> {
    let pg_type = format_type::format_type_with_typemod(atttypid, atttypmod)
        .unwrap_or_else(|_| format!("type {atttypid}"));
    type_mismatch(attname, &pg_type, &schema.type_desc())
}

/// The whole binding for one column: the scalar conversion, or the LIST
/// arm when the leaf is repeated and the target is an array, plus the
/// input-function spec the `ViaInput` arms need.
fn resolve_binding(
    schema: &parquet_read::ColumnSchema,
    atttypid: Oid,
    atttypmod: i32,
    attname: &str,
    coerce_epoch: bool,
) -> PgResult<(Conv, Option<InputSpec>, Option<ListBind>)> {
    if schema.max_rep == 0 {
        let conv = resolve_conv(schema, atttypid, atttypmod, attname, coerce_epoch)?;
        let input = match conv {
            Conv::ViaInput(_) => Some(input_spec(atttypid, atttypmod)?),
            _ => None,
        };
        return Ok((conv, input, None));
    }
    // Repeated leaf: exactly one repeated level, under a LIST-annotated
    // group (3-level `col.list.element` or the 2-level legacy `col.element`
    // where the leaf itself repeats), into a one-dimensional array of a
    // scalar element type. MAP, STRUCT and nested LIST leaves have no
    // binding (docs/gravity/parquet-load.md, "Conformance").
    let elmtype = lsyscache::get_element_type(atttypid)?;
    let list_at = schema.ancestors.iter().rposition(|a| a.logical == Logical::List);
    let shape_ok = schema.max_rep == 1
        && elmtype != types_core::InvalidOid
        && match list_at {
            Some(i) => {
                let below = &schema.ancestors[i + 1..];
                // 3-level: one repeated group below the LIST group, the leaf
                // is not itself repeated. 2-level: nothing below, the leaf
                // repeats.
                (below.len() == 1
                    && below[0].repetition == Repetition::Repeated
                    && schema.repetition != Repetition::Repeated)
                    || (below.is_empty() && schema.repetition == Repetition::Repeated)
            }
            None => false,
        };
    if !shape_ok {
        return Err(no_binding(schema, atttypid, atttypmod, attname));
    }
    let g = &schema.ancestors[list_at.expect("checked")];
    let elem = resolve_conv(schema, elmtype, -1, attname, coerce_epoch)?;
    if matches!(elem, Conv::List) {
        return Err(no_binding(schema, atttypid, atttypmod, attname));
    }
    let (elmlen, elmbyval, elmalign) = lsyscache::get_typlenbyvalalign(elmtype)?;
    let input = match elem {
        Conv::ViaInput(_) => Some(input_spec(elmtype, -1)?),
        _ => None,
    };
    let list = ListBind {
        elem,
        elmtype,
        elmlen,
        elmbyval,
        elmalign: elmalign as u8,
        list_def: g.def_level,
        elem_def: schema.max_def,
        elem_nullable: schema.repetition == Repetition::Optional,
    };
    Ok((Conv::List, input, Some(list)))
}

fn input_spec(typid: Oid, typmod: i32) -> PgResult<InputSpec> {
    let (typinput, typioparam) = lsyscache::getTypeInputInfo(typid)?;
    Ok(InputSpec { typinput, typioparam, typmod })
}

/// Whether the conversion consumes string bytes (drives page-batched UTF-8
/// validation; bytea reads raw bytes unvalidated).
fn wants_utf8(conv: Conv) -> bool {
    matches!(
        conv,
        Conv::Text | Conv::VarcharN(_) | Conv::Bpchar(_) | Conv::ViaInput(InputForm::Json)
    )
}

#[track_caller]
#[cold]
#[inline(never)]
fn stdin_unsupported() -> Box<PgError> {
    Box::new(
        PgError::error("COPY FROM STDIN with parquet format is not supported")
            .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED)
            .with_hint("Use a server-side file path."),
    )
}

/// Build the source at BeginCopyFrom time: open + footer parse + schema
/// match (pg_parquet match_by semantics) + conversion resolution. All
/// schema-level errors surface here, before any row is read.
pub(crate) fn open_source(
    filename: Option<&str>,
    tup_desc: &TupleDescData<'_>,
    attnumlist: &[i16],
    match_by_name: bool,
    coerce_epoch: bool,
) -> PgResult<(Box<ParquetSrc>, u64)> {
    let Some(filename) = filename else {
        return Err(stdin_unsupported());
    };
    let owner = pgrc_am::ingest::load_owner();
    // memsig E-pub t5: the footer on the load's owner, as its row groups (`row_group_on`)
    let reader = FileReader::open_on(filename, owner)?;
    let plan = resolve_plan(&reader.meta, tup_desc, attnumlist, match_by_name, coerce_epoch)?;
    let batches = plan.make_batches(&reader.meta);
    let file_len = reader.file_len();
    let mut inputs = ::mcx::stdvec::try_vec(plan.bindings.len(), "fromparquet")?;
    for b in &plan.bindings {
        inputs.push(match b.input {
            Some(spec) => Some(CellInput::new(spec)?),
            None => None,
        });
    }
    let list_starts = ::mcx::stdvec::try_filled(plan.bindings.len(), Vec::new(), "fromparquet")?;
    Ok((
        Box::new(ParquetSrc {
            batch_charge: pgrc_write::held::HeldCharge::new(owner),
            owner,
            reader,
            plan: std::sync::Arc::new(plan),
            batches,
            batch_len: 0,
            batch_pos: 0,
            rg_idx: 0,
            rg: None,
            inputs,
            list_starts,
            bad_row_delta: None,
        }),
        file_len,
    ))
}

/// Schema match (pg_parquet match_by semantics) + conversion resolution.
/// All schema-level errors surface here, before any row is read.
pub(crate) fn resolve_plan(
    meta: &parquet_read::FileMeta,
    tup_desc: &TupleDescData<'_>,
    attnumlist: &[i16],
    match_by_name: bool,
    coerce_epoch: bool,
) -> PgResult<ParquetPlan> {
    let ncols = meta.columns.len();
    if !match_by_name && ncols != attnumlist.len() {
        // pg_parquet position-matching contract.
        return Err(Box::new(
            PgError::error(format!(
                "column count mismatch between table and parquet file. parquet file has \
                 {ncols} columns, but table has {} columns",
                attnumlist.len()
            ))
            .with_sqlstate(ERRCODE_BAD_COPY_FILE_FORMAT),
        ));
    }

    let mut bindings = ::mcx::std_vec_with_capacity("commands", attnumlist.len())?;
    let mut cols = ::mcx::std_vec_with_capacity("commands", attnumlist.len())?;
    let mut vutf8 = ::mcx::std_vec_with_capacity("commands", attnumlist.len())?;
    let mut any_text = false;
    for (i, &attnum) in attnumlist.iter().enumerate() {
        let attidx = attnum as usize - 1;
        let att = tup_desc.attr(attidx);
        let attname = String::from_utf8_lossy(att.attname.name_str()).into_owned();
        let col = if match_by_name {
            match meta.columns.iter().position(|c| c.name == attname) {
                Some(c) => c,
                None => {
                    return Err(Box::new(
                        PgError::error(format!(
                            "column \"{attname}\" is not found in parquet file"
                        ))
                        .with_sqlstate(ERRCODE_UNDEFINED_COLUMN),
                    ))
                }
            }
        } else {
            i
        };
        let schema = &meta.columns[col];
        let (conv, input, list) =
            resolve_binding(schema, att.atttypid, att.atttypmod, &attname, coerce_epoch)?;
        let utf8 = wants_utf8(conv) || list.is_some_and(|l| wants_utf8(l.elem));
        any_text |= utf8;
        cols.push(col);
        vutf8.push(utf8);
        bindings.push(Binding { attidx, conv, batch: bindings.len(), input, list });
    }

    if any_text && mbutils::GetDatabaseEncoding() != wchar::PG_UTF8 {
        // Parquet strings are UTF-8 by definition; transcoding into other
        // database encodings is a later increment. Still reachable via
        // SQL_ASCII databases (kept creatable under the UTF-8-only
        // server-encoding carve, docs/design/carve-ratifications.md §11).
        return Err(Box::new(
            PgError::error(
                "COPY FROM parquet with text columns requires a UTF8 database encoding",
            )
            .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
        ));
    }
    Ok(ParquetPlan { bindings, cols, vutf8 })
}

/// Test face (the fused-decoder identity test): [`resolve_plan`] by
/// POSITION over explicit `(name, typid, typmod)` attributes instead of a
/// tuple descriptor — the same `resolve_binding` per column, no catalog, no
/// database-encoding check.
#[cfg(test)]
pub(crate) fn resolve_plan_types(
    meta: &parquet_read::FileMeta,
    atts: &[(String, Oid, i32)],
    coerce_epoch: bool,
) -> PgResult<ParquetPlan> {
    if meta.columns.len() != atts.len() {
        return Err(Box::new(PgError::error(format!(
            "test plan: parquet has {} columns, attribute list {}",
            meta.columns.len(),
            atts.len()
        ))));
    }
    let mut bindings = Vec::with_capacity(atts.len());
    let mut cols = Vec::with_capacity(atts.len());
    let mut vutf8 = Vec::with_capacity(atts.len());
    for (i, (name, typid, typmod)) in atts.iter().enumerate() {
        let schema = &meta.columns[i];
        let (conv, input, list) = resolve_binding(schema, *typid, *typmod, name, coerce_epoch)?;
        let utf8 = wants_utf8(conv) || list.is_some_and(|l| wants_utf8(l.elem));
        cols.push(i);
        vutf8.push(utf8);
        bindings.push(Binding { attidx: i, conv, batch: bindings.len(), input, list });
    }
    Ok(ParquetPlan { bindings, cols, vutf8 })
}

/// How a bpchar(n) input is stored — the ONE shape decision shared by the
/// datum arm ([`bpchar_datum`]) and the raw parallel-capture arm
/// ([`raw_cell`]), so the two can never drift.
enum BpShape {
    Verbatim,
    Cut(usize),
    /// Space-pad by this many bytes.
    Pad(usize),
}

/// bpchar(n) rule: over-length values take the varchar blank-trim rule;
/// shorter values are space-padded to exactly n characters
/// (input-function parity). UTF-8 validated at page decode.
fn bpchar_shape(bytes: &[u8], maxlen: i32) -> PgResult<BpShape> {
    if maxlen < 0 {
        return Ok(BpShape::Verbatim);
    }
    let maxlen = maxlen as usize;
    // Char count + byte cut at maxlen chars.
    let mut chars = 0usize;
    let mut cut = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if (b & 0xc0) != 0x80 {
            if chars == maxlen {
                cut = i;
                break;
            }
            chars += 1;
        }
    }
    if cut < bytes.len() {
        if bytes[cut..].iter().any(|&b| b != b' ') {
            return Err(Box::new(
                PgError::error(format!("value too long for type character({maxlen})"))
                    .with_sqlstate(ERRCODE_STRING_DATA_RIGHT_TRUNCATION),
            ));
        }
        return Ok(BpShape::Cut(cut));
    }
    if chars >= maxlen {
        return Ok(BpShape::Verbatim);
    }
    Ok(BpShape::Pad(maxlen - chars))
}

/// bpchar(n) datum through the shared shape decision.
fn bpchar_datum<'mcx>(mcx: Mcx<'mcx>, bytes: &[u8], maxlen: i32) -> PgResult<Datum> {
    match bpchar_shape(bytes, maxlen)? {
        BpShape::Verbatim => varlena_datum(mcx, bytes),
        BpShape::Cut(cut) => varlena_datum(mcx, &bytes[..cut]),
        BpShape::Pad(pad) => {
            let mut image = varlena::image_with_header(mcx, bytes.len() + pad)?;
            mcx::vec_append_bytes(&mut image, bytes)
                .map_err(|_| Box::new(PgError::error("out of memory building bpchar datum")))?;
            for _ in 0..pad {
                image.push(b' ');
            }
            Ok(types_fmgr::varlena_result(datum::Varlena::from_image(image)))
        }
    }
}

impl ParquetSrc {
    pub(crate) fn reader(&self) -> &FileReader {
        &self.reader
    }

    pub(crate) fn plan_arc(&self) -> std::sync::Arc<ParquetPlan> {
        std::sync::Arc::clone(&self.plan)
    }

    /// Reposition the serial cursor at the START of raw row group
    /// `rg_idx` (the parallel path's error-replay seam: the leader
    /// re-decodes one failing row group through the serial machinery for
    /// byte-exact error parity).
    pub(crate) fn seek_rg(&mut self, rg_idx: usize) {
        self.rg = None;
        self.rg_idx = rg_idx;
        self.batch_len = 0;
        self.batch_pos = 0;
    }
}

#[track_caller]
#[cold]
#[inline(never)]
fn out_of_range(what: &'static str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("{what} out of range"))
            .with_sqlstate(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
    )
}

#[track_caller]
#[cold]
#[inline(never)]
fn dt_out_of_range(what: &'static str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("{what} out of range"))
            .with_sqlstate(ERRCODE_DATETIME_VALUE_OUT_OF_RANGE),
    )
}

#[inline]
fn ts_from_unix_us(us: i64) -> PgResult<Datum> {
    let t = us.checked_sub(EPOCH_SHIFT_US).ok_or_else(|| dt_out_of_range("timestamp"))?;
    if !(MIN_TIMESTAMP_US..END_TIMESTAMP_US).contains(&t) {
        return Err(dt_out_of_range("timestamp"));
    }
    Ok(Datum::from_i64(t))
}

/// Epoch-second coercion kernel. The epoch shift is subtracted in the
/// SECONDS domain before scaling: the top of the validity window is not
/// representable as Unix-epoch microseconds in i64, so scale-then-shift
/// would spuriously overflow inside the valid range.
#[inline]
fn ts_from_unix_secs(secs: i64) -> PgResult<Datum> {
    let t = secs
        .checked_sub(EPOCH_SHIFT_SECS)
        .and_then(|s| s.checked_mul(1_000_000))
        .ok_or_else(|| dt_out_of_range("timestamp"))?;
    if !(MIN_TIMESTAMP_US..END_TIMESTAMP_US).contains(&t) {
        return Err(dt_out_of_range("timestamp"));
    }
    Ok(Datum::from_i64(t))
}

/// Epoch-day coercion kernel: shift in i64, then the i32 datum bound (the
/// same accepted set as the annotated-DATE conversion for INT32 inputs).
#[inline]
fn date_from_unix_days(days: i64) -> PgResult<Datum> {
    let d = days
        .checked_sub(i64::from(EPOCH_SHIFT_DAYS))
        .and_then(|d| i32::try_from(d).ok())
        .ok_or_else(|| dt_out_of_range("date"))?;
    if !(MIN_DATE_DAYS..END_DATE_DAYS).contains(&d) {
        return Err(dt_out_of_range("date"));
    }
    Ok(Datum::from_i32(d))
}

fn varlena_datum(mcx: Mcx<'_>, bytes: &[u8]) -> PgResult<Datum> {
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, bytes)?))
}

/// Verify a decoded parquet string value against the database encoding before
/// it becomes a text-class datum, matching how every other text input path in
/// the engine funnels bytes through pg_verifymbstr (fromparse.rs on the
/// text/CSV lane, pg_any_to_server on the protocol/binary lane).
///
/// Parquet strings are UTF-8 by definition and are UTF-8-validated at page
/// decode, but plain UTF-8 validation (simdutf8) accepts an embedded 0x00,
/// which the engine's verifier rejects (pg_utf8_verifychar returns -1 for
/// b==0). Text-class datums must never contain NUL: countless consumers
/// convert text to NUL-terminated C strings (output functions, COPY TO's
/// CStr::from_ptr, logs, dump/replication) and would silently truncate at the
/// first NUL while indexes/constraints see the full byte string. Reject such
/// values here so the parquet lane cannot smuggle a NUL-bearing (or otherwise
/// invalid) text datum into a table. bytea is exempt: it holds arbitrary
/// bytes and never goes through this check.
///
/// The resolved plan already restricts text columns to UTF-8 databases, so
/// this validates against the UTF-8 verifier (which is where the NUL
/// rejection lives).
fn verify_text_bytes(bytes: &[u8]) -> PgResult<()> {
    mbutils::pg_verifymbstr(bytes, false)?;
    Ok(())
}

/// varchar(n) length rule shared by the datum and raw arms: `Ok(None)` =
/// store verbatim; `Ok(Some(cut))` = store `bytes[..cut]` (all-space excess
/// trimmed). Values longer than n characters with non-space excess error
/// (varchar input parity). UTF-8 validated at page decode: continuation
/// bytes are 0b10xxxxxx.
fn varchar_cut(bytes: &[u8], maxlen: i32) -> PgResult<Option<usize>> {
    let maxlen = maxlen.max(0) as usize;
    if bytes.len() <= maxlen {
        // chars <= bytes: within limit for sure.
        return Ok(None);
    }
    // Locate the byte end of the first `maxlen` characters.
    let mut chars = 0usize;
    let mut cut = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if (b & 0xc0) != 0x80 {
            if chars == maxlen {
                cut = i;
                break;
            }
            chars += 1;
        }
    }
    if cut == bytes.len() {
        // Fewer than maxlen characters despite more bytes.
        return Ok(None);
    }
    if bytes[cut..].iter().any(|&b| b != b' ') {
        return Err(Box::new(
            PgError::error(format!(
                "value too long for type character varying({maxlen})"
            ))
            .with_sqlstate(ERRCODE_STRING_DATA_RIGHT_TRUNCATION),
        ));
    }
    Ok(Some(cut))
}

/// varchar(n) datum through the shared length rule.
fn varchar_datum<'mcx>(mcx: Mcx<'mcx>, bytes: &[u8], maxlen: i32) -> PgResult<Datum> {
    match varchar_cut(bytes, maxlen)? {
        None => varlena_datum(mcx, bytes),
        Some(cut) => varlena_datum(mcx, &bytes[..cut]),
    }
}

impl ParquetSrc {
    /// Advance to the next non-empty batch. Returns false at file end.
    fn refill(&mut self, bytes_progress: &mut u64) -> PgResult<bool> {
        loop {
            if let Some(rg) = self.rg.as_mut() {
                if rg.rows_remaining() > 0 {
                    let n = rg.rows_remaining().min(BATCH_ROWS as u64) as usize;
                    let from = rg.num_rows() - rg.rows_remaining();
                    if let Err(e) = rg.read_batches(&mut self.batches, n) {
                        if is_reader_invalid_utf8(&e) {
                            self.bad_row_delta = self.locate_invalid_utf8(self.rg_idx - 1, from);
                        }
                        return Err(e);
                    }
                    self.batch_charge.sync(parquet_read::batches_held(&self.batches));
                    for (i, b) in self.plan.bindings.iter().enumerate() {
                        if b.list.is_none() {
                            continue;
                        }
                        let rep = &rg.levels(b.batch).rep;
                        let starts = &mut self.list_starts[i];
                        starts.clear();
                        starts.extend(
                            rep.iter().enumerate().filter(|(_, &r)| r == 0).map(|(s, _)| s as u32),
                        );
                        if starts.len() != n {
                            return Err(batch_shape());
                        }
                        starts.push(rep.len() as u32);
                    }
                    self.batch_len = n;
                    self.batch_pos = 0;
                    return Ok(true);
                }
            }
            self.rg = None;
            if self.rg_idx >= self.reader.meta.row_groups.len() {
                return Ok(false);
            }
            let rg = match self.reader.row_group_on(self.rg_idx, &self.plan.cols, &self.plan.vutf8, self.owner) {
                Ok(rg) => rg,
                Err(e) => {
                    if is_reader_invalid_utf8(&e) {
                        self.bad_row_delta = self.locate_invalid_utf8(self.rg_idx, 0);
                    }
                    return Err(e);
                }
            };
            *bytes_progress += rg.compressed_bytes;
            self.rg_idx += 1;
            self.rg = Some(rg);
        }
    }
}

impl ParquetSrc {
    /// D4361: the row (past `from`, the rows of row group `rg_idx` already
    /// read) of the first value a validated flat text column holds that is
    /// not UTF-8, found by re-reading the group without the reader's check.
    /// None when there is none past `from` (a bad dictionary entry no row
    /// uses) or the re-read fails: the error then keeps the batch start.
    /// Cold: runs only on the way out of a failed load.
    #[cold]
    #[inline(never)]
    fn locate_invalid_utf8(&mut self, rg_idx: usize, from: u64) -> Option<u64> {
        let unchecked = ::mcx::stdvec::try_filled(self.plan.vutf8.len(), false, "fromparquet").ok()?;
        let mut rg = self.reader.row_group_on(rg_idx, &self.plan.cols, &unchecked, self.owner).ok()?;
        let mut batches = self.plan.make_batches(&self.reader.meta);
        // CONVERT wave 3 (cvwrite job 3): the re-read's batches on the load's owner too; declared after them, drops first
        let mut batch_charge = pgrc_write::held::HeldCharge::new(self.owner);
        let mut row = 0u64;
        while rg.rows_remaining() > 0 {
            let n = rg.rows_remaining().min(BATCH_ROWS as u64) as usize;
            rg.read_batches(&mut batches, n).ok()?;
            batch_charge.sync(parquet_read::batches_held(&batches));
            if row + n as u64 > from {
                let mut first: Option<u64> = None;
                for (k, b) in batches.iter().enumerate() {
                    let flat = rg.levels(k).rep.is_empty() && b.len() == n;
                    if !self.plan.vutf8[k] || !flat || !matches!(b.data, BatchData::Bytes { .. }) {
                        continue;
                    }
                    let lo = from.saturating_sub(row) as usize;
                    if let Some(i) = (lo..n).find(|&i| !b.is_null(i) && std::str::from_utf8(b.bytes_at(i)).is_err()) {
                        let r = row + i as u64;
                        first = Some(first.map_or(r, |f| f.min(r)));
                    }
                }
                if let Some(r) = first {
                    return Some(r - from);
                }
            }
            row += n as u64;
        }
        None
    }
}

/// The reader's column UTF-8 check ([`parquet_read`]'s invalid_utf8 error).
fn is_reader_invalid_utf8(e: &PgError) -> bool {
    e.sqlstate() == types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE
        && e.message().ends_with("in parquet string column")
}

impl<'mcx> CopyFromState<'mcx, '_> {
    /// One parquet row into values/nulls; false at end of file. Mirrors the
    /// binary arm's contract (defaults for unlisted columns evaluated by the
    /// caller-shared defmap loop in next_copy_from).
    pub(crate) fn copy_from_parquet_one_row(
        &mut self,
        row_mcx: Mcx<'_>,
        values: &mut [Datum],
        nulls: &mut [bool],
    ) -> PgResult<bool> {
        self.cur_lineno += 1;
        let crate::from::CopySrc::Parquet(src) = &mut self.src else {
            unreachable!("parquet row read on a non-parquet source");
        };
        if src.batch_pos == src.batch_len {
            let mut progressed = self.bytes_processed;
            let more = match src.refill(&mut progressed) {
                Ok(more) => more,
                Err(e) => {
                    // D4361: name the invalid value's row, not the batch's.
                    if let Some(d) = src.bad_row_delta.take() {
                        self.cur_lineno += d;
                    }
                    return Err(e);
                }
            };
            if progressed != self.bytes_processed {
                self.bytes_processed = progressed;
                backend_progress::pgstat_progress_update_param(
                    backend_progress::progress::PROGRESS_COPY_BYTES_PROCESSED,
                    self.bytes_processed as i64,
                );
            }
            if !more {
                return Ok(false);
            }
        }
        let k = src.batch_pos;
        src.batch_pos += 1;

        // Split borrows: bindings/batches are read, cur_attidx written for
        // error context.
        let ParquetSrc { plan, batches, inputs, list_starts, rg, .. } = src.as_mut();
        for (i, b) in plan.bindings.iter().enumerate() {
            let batch = &batches[b.batch];
            let m = b.attidx;
            self.cur_attidx = Some(m);
            let cell = if let Some(list) = b.list {
                // The batch's null lane is per slot; a record's fate is in
                // its definition levels.
                let lanes = rg.as_ref().ok_or_else(batch_shape)?.levels(b.batch);
                let s = list_starts[i][k] as usize;
                let e = list_starts[i][k + 1] as usize;
                list_cell(row_mcx, &list, batch, &lanes.def[s..e], s, inputs[i].as_mut())?
            } else if batch.is_null(k) {
                None
            } else {
                Some(convert_cell(row_mcx, b.conv, batch, k, inputs[i].as_mut())?)
            };
            match cell {
                None => nulls[m] = true,
                Some(d) => {
                    values[m] = d;
                    nulls[m] = false;
                }
            }
        }
        self.cur_attidx = None;
        Ok(true)
    }
}

/// One LIST record -> array datum (or NULL). `defs` are the record's
/// definition levels, `base` its first slot in `batch`.
fn list_cell<'mcx>(
    mcx: Mcx<'mcx>,
    list: &ListBind,
    batch: &ColumnBatch,
    defs: &[u16],
    base: usize,
    mut input: Option<&mut CellInput>,
) -> PgResult<Option<Datum>> {
    let Some(&first) = defs.first() else {
        return Err(batch_shape());
    };
    if first < list.list_def {
        return Ok(None);
    }
    if first == list.list_def {
        if defs.len() != 1 {
            return Err(batch_shape());
        }
        let img = datum::array_build::construct_empty_array_image(mcx, list.elmtype)?;
        return Ok(Some(leak_image(img)));
    }
    let mut elems: Vec<Datum> = ::mcx::stdvec::try_vec(defs.len(), "fromparquet")?;
    let mut elem_nulls: Vec<bool> = ::mcx::stdvec::try_vec(defs.len(), "fromparquet")?;
    let mut any_null = false;
    for (j, &d) in defs.iter().enumerate() {
        if d == list.elem_def {
            elems.push(convert_cell(mcx, list.elem, batch, base + j, input.as_deref_mut())?);
            elem_nulls.push(false);
        } else if list.elem_nullable && d + 1 == list.elem_def {
            elems.push(Datum::null());
            elem_nulls.push(true);
            any_null = true;
        } else {
            return Err(batch_shape());
        }
    }
    let img = arrayfuncs::construct::construct_md_array(
        mcx,
        &elems,
        if any_null { Some(&elem_nulls) } else { None },
        1,
        &[elems.len() as i32],
        &[1],
        list.elmtype,
        i32::from(list.elmlen),
        list.elmbyval,
        list.elmalign,
    )?;
    Ok(Some(leak_image(img)))
}

/// The image lives in the row context (reset per row); the datum points
/// into it.
fn leak_image(img: mcx::PgVec<'_, u8>) -> Datum {
    let d = Datum::from_usize(img.as_ptr() as usize);
    std::mem::forget(img);
    d
}

/// Big-endian two's complement of any length -> decimal digits with sign.
fn be_int_to_decimal(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".to_string();
    }
    if bytes.len() <= 16 {
        let mut buf = [if bytes[0] & 0x80 != 0 { 0xffu8 } else { 0 }; 16];
        buf[16 - bytes.len()..].copy_from_slice(bytes);
        return i128::from_be_bytes(buf).to_string();
    }
    // Wider than i128: magnitude in base-2^32 limbs (little-endian),
    // repeated division by 10^9.
    let negative = bytes[0] & 0x80 != 0;
    let mut limbs: Vec<u32> = Vec::new();
    let mut carry: u64 = 1;
    for chunk in bytes.rchunks(4) {
        // A short top limb is sign-extended before the negation.
        let mut w = [if negative { 0xffu8 } else { 0 }; 4];
        w[4 - chunk.len()..].copy_from_slice(chunk);
        let mut v = u32::from_be_bytes(w);
        if negative {
            let x = u64::from(!v) + carry;
            v = x as u32;
            carry = x >> 32;
        }
        limbs.push(v);
    }
    let mut groups: Vec<u32> = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem: u64 = 0;
        for l in limbs.iter_mut().rev() {
            let cur = (rem << 32) | u64::from(*l);
            *l = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
        }
        groups.push(rem as u32);
    }
    if groups.is_empty() {
        return "0".to_string();
    }
    let mut s = String::new();
    if negative {
        s.push('-');
    }
    s.push_str(&groups.last().expect("non-empty").to_string());
    for g in groups.iter().rev().skip(1) {
        s.push_str(&format!("{g:09}"));
    }
    s
}

/// `unscaled × 10^-scale` as a decimal literal (negative scale = trailing
/// zeros).
fn scaled_decimal_text(unscaled: &str, scale: i32) -> String {
    let (sign, digits) = match unscaled.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", unscaled),
    };
    if scale <= 0 {
        let zeros = usize::try_from(-scale).unwrap_or(0);
        return format!("{sign}{digits}{}", "0".repeat(zeros));
    }
    let scale = scale as usize;
    let padded = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits.to_string()
    };
    let (int_part, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{int_part}.{frac}")
}

/// The DECIMAL cell's unscaled integer as decimal text.
fn decimal_unscaled_text(src: DecimalSrc, batch: &ColumnBatch, k: usize) -> PgResult<String> {
    Ok(match (src, &batch.data) {
        (DecimalSrc::I32, BatchData::I32(v)) => v[k].to_string(),
        (DecimalSrc::I64, BatchData::I64(v)) => v[k].to_string(),
        (DecimalSrc::Bytes, BatchData::Bytes { .. }) => be_int_to_decimal(batch.bytes_at(k)),
        _ => return Err(batch_shape()),
    })
}

/// The DECIMAL cell's unscaled integer as i128 (None when wider).
fn decimal_unscaled_i128(src: DecimalSrc, batch: &ColumnBatch, k: usize) -> PgResult<Option<i128>> {
    Ok(match (src, &batch.data) {
        (DecimalSrc::I32, BatchData::I32(v)) => Some(i128::from(v[k])),
        (DecimalSrc::I64, BatchData::I64(v)) => Some(i128::from(v[k])),
        (DecimalSrc::Bytes, BatchData::Bytes { .. }) => {
            let bytes = batch.bytes_at(k);
            if bytes.len() > 16 {
                None
            } else {
                let mut buf = [if bytes.first().is_some_and(|b| b & 0x80 != 0) { 0xffu8 } else { 0 }; 16];
                buf[16 - bytes.len()..].copy_from_slice(bytes);
                Some(i128::from_be_bytes(buf))
            }
        }
        _ => return Err(batch_shape()),
    })
}

/// IEEE 754 binary16 -> f32 (subnormals and NaN payload preserved).
fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exp = u32::from((bits >> 10) & 0x1f);
    let mant = u32::from(bits & 0x3ff);
    let out = match exp {
        0 => {
            if mant == 0 {
                sign
            } else {
                // Subnormal: normalize into the f32 exponent range.
                let mut e = 127 - 15 + 1;
                let mut m = mant;
                while m & 0x400 == 0 {
                    m <<= 1;
                    e -= 1;
                }
                sign | ((e as u32) << 23) | ((m & 0x3ff) << 13)
            }
        }
        0x1f => sign | 0x7f80_0000 | (mant << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (mant << 13),
    };
    f32::from_bits(out)
}

fn f16_cell(batch: &ColumnBatch, k: usize) -> PgResult<f32> {
    let b = batch.bytes_at(k);
    if b.len() != 2 {
        return Err(batch_shape());
    }
    Ok(f16_to_f32(u16::from_le_bytes([b[0], b[1]])))
}

// Julian day number of 1970-01-01 (INT96's day word is a Julian day).
const UNIX_EPOCH_JDN: i64 = 2_440_588;
const US_PER_DAY: i64 = 86_400_000_000;

/// Render the text image an input-function arm hands to the type's input
/// function.
fn input_text(form: InputForm, batch: &ColumnBatch, k: usize) -> PgResult<Vec<u8>> {
    Ok(match form {
        InputForm::Decimal(src, scale) => {
            scaled_decimal_text(&decimal_unscaled_text(src, batch, k)?, i32::from(scale)).into_bytes()
        }
        InputForm::Uuid => {
            let b = batch.bytes_at(k);
            if b.len() != 16 {
                return Err(batch_shape());
            }
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
                .into_bytes()
        }
        InputForm::Interval => {
            let b = batch.bytes_at(k);
            if b.len() != 12 {
                return Err(batch_shape());
            }
            let w = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
            format!("{} months {} days {} milliseconds", w(0), w(4), w(8)).into_bytes()
        }
        InputForm::Json => {
            let b = batch.bytes_at(k);
            verify_text_bytes(b)?;
            ::mcx::stdvec::try_to_vec(b, "copy")?
        }
    })
}

#[track_caller]
#[cold]
#[inline(never)]
fn batch_shape() -> Box<PgError> {
    Box::new(
        PgError::error("parquet batch type does not match resolved conversion")
            .with_sqlstate(ERRCODE_BAD_COPY_FILE_FORMAT),
    )
}

/// The by-value half of the conversion table: every arm whose result is a
/// word datum (no memory context involved — all `Datum` constructors here
/// are const fns). `Ok(None)` = a text-family conversion; the caller picks
/// its carrier (mcx datum for the serial row path, arena varlena image for
/// the parallel raw-capture path). ONE source of truth for the conversion
/// arithmetic and range checks — [`convert_cell`] and [`raw_cell`] both
/// delegate here, so the two paths cannot drift.
pub(crate) fn word_cell(conv: Conv, batch: &ColumnBatch, k: usize) -> PgResult<Option<Datum>> {
    macro_rules! lane {
        ($variant:ident) => {
            match &batch.data {
                BatchData::$variant(v) => v[k],
                _ => return Err(batch_shape()),
            }
        };
    }
    Ok(Some(match conv {
        Conv::Bool => Datum::from_bool(lane!(Bool)),
        Conv::I32 => Datum::from_i32(lane!(I32)),
        Conv::I64 => Datum::from_i64(lane!(I64)),
        Conv::I64FromI32 => Datum::from_i64(i64::from(lane!(I32))),
        Conv::I16FromI32 => {
            let v = lane!(I32);
            let v = i16::try_from(v).map_err(|_| out_of_range("smallint"))?;
            Datum::from_i16(v)
        }
        Conv::I16FromI64 => {
            let v = lane!(I64);
            let v = i16::try_from(v).map_err(|_| out_of_range("smallint"))?;
            Datum::from_i16(v)
        }
        Conv::I32FromI64 => {
            let v = lane!(I64);
            let v = i32::try_from(v).map_err(|_| out_of_range("integer"))?;
            Datum::from_i32(v)
        }
        Conv::U32AsI32 => {
            let v = lane!(I32) as u32;
            let v = i32::try_from(v).map_err(|_| out_of_range("integer"))?;
            Datum::from_i32(v)
        }
        Conv::U32AsI64 => Datum::from_i64(i64::from(lane!(I32) as u32)),
        Conv::U64AsI64 => {
            let v = lane!(I64);
            if v < 0 {
                return Err(out_of_range("bigint"));
            }
            Datum::from_i64(v)
        }
        Conv::F32 => Datum::from_f32(lane!(F32)),
        Conv::F64 => Datum::from_f64(lane!(F64)),
        Conv::F64FromF32 => Datum::from_f64(f64::from(lane!(F32))),
        Conv::DateFromDays => {
            let v = lane!(I32);
            let d = v.checked_sub(EPOCH_SHIFT_DAYS).ok_or_else(|| dt_out_of_range("date"))?;
            if !(MIN_DATE_DAYS..END_DATE_DAYS).contains(&d) {
                return Err(dt_out_of_range("date"));
            }
            Datum::from_i32(d)
        }
        Conv::Timestamp(unit) => {
            let v = lane!(I64);
            let us = match unit {
                TimeUnit::Micros => v,
                TimeUnit::Millis => {
                    v.checked_mul(1000).ok_or_else(|| dt_out_of_range("timestamp"))?
                }
                // Floor division: pre-epoch nanos round down, matching the
                // timestamp number line rather than truncation toward zero.
                TimeUnit::Nanos => v.div_euclid(1000),
            };
            ts_from_unix_us(us)?
        }
        Conv::TsFromEpochSecs(lane) => {
            let secs = match lane {
                EpochLane::I32 => i64::from(lane!(I32)),
                EpochLane::U32 => i64::from(lane!(I32) as u32),
                EpochLane::I64 => lane!(I64),
                EpochLane::U64 => {
                    let v = lane!(I64);
                    if v < 0 {
                        // Sign-bit u64 values sit far above the window.
                        return Err(dt_out_of_range("timestamp"));
                    }
                    v
                }
            };
            ts_from_unix_secs(secs)?
        }
        Conv::DateFromEpochDays(lane) => {
            let days = match lane {
                EpochLane::I32 => i64::from(lane!(I32)),
                EpochLane::U32 => i64::from(lane!(I32) as u32),
                EpochLane::I64 => lane!(I64),
                EpochLane::U64 => {
                    let v = lane!(I64);
                    if v < 0 {
                        return Err(dt_out_of_range("date"));
                    }
                    v
                }
            };
            date_from_unix_days(days)?
        }
        Conv::Time(unit) => {
            let v = lane!(I64);
            let us = match unit {
                TimeUnit::Micros => v,
                TimeUnit::Millis => v.checked_mul(1000).ok_or_else(|| dt_out_of_range("time"))?,
                TimeUnit::Nanos => v.div_euclid(1000),
            };
            if !(0..=MAX_TIME_US).contains(&us) {
                return Err(dt_out_of_range("time"));
            }
            Datum::from_i64(us)
        }
        // The §13 arms live in the cold extension so this function's shape
        // (the pgrc driver's per-cell path) stays the one measured.
        Conv::TimeMillis32
        | Conv::TsFromInt96
        | Conv::F32FromF16
        | Conv::F64FromF16
        | Conv::DecimalToInt(..)
        | Conv::DecimalToF32(..)
        | Conv::DecimalToF64(..) => return word_cell_ext(conv, batch, k),
        Conv::Text
        | Conv::VarcharN(_)
        | Conv::Bpchar(_)
        | Conv::Bytea
        | Conv::ViaInput(_)
        | Conv::List => return Ok(None),
    }))
}

/// The logical-type arms of [`word_cell`] (the uncommon ones).
#[cold]
#[inline(never)]
fn word_cell_ext(conv: Conv, batch: &ColumnBatch, k: usize) -> PgResult<Option<Datum>> {
    macro_rules! lane {
        ($variant:ident) => {
            match &batch.data {
                BatchData::$variant(v) => v[k],
                _ => return Err(batch_shape()),
            }
        };
    }
    Ok(Some(match conv {
        Conv::TimeMillis32 => {
            let us = i64::from(lane!(I32)) * 1000;
            if !(0..=MAX_TIME_US).contains(&us) {
                return Err(dt_out_of_range("time"));
            }
            Datum::from_i64(us)
        }
        Conv::TsFromInt96 => {
            let w = lane!(I96);
            let nanos = (u64::from(w[1]) << 32) | u64::from(w[0]);
            let days = i64::from(w[2] as i32) - UNIX_EPOCH_JDN;
            let us = days
                .checked_mul(US_PER_DAY)
                .and_then(|d| d.checked_add((nanos / 1000) as i64))
                .ok_or_else(|| dt_out_of_range("timestamp"))?;
            ts_from_unix_us(us)?
        }
        Conv::F32FromF16 => Datum::from_f32(f16_cell(batch, k)?),
        Conv::F64FromF16 => Datum::from_f64(f64::from(f16_cell(batch, k)?)),
        Conv::DecimalToInt(src, width) => {
            let (name, lo, hi) = match width {
                IntWidth::I16 => ("smallint", i128::from(i16::MIN), i128::from(i16::MAX)),
                IntWidth::I32 => ("integer", i128::from(i32::MIN), i128::from(i32::MAX)),
                IntWidth::I64 => ("bigint", i128::from(i64::MIN), i128::from(i64::MAX)),
            };
            let v = decimal_unscaled_i128(src, batch, k)?
                .filter(|v| (lo..=hi).contains(v))
                .ok_or_else(|| out_of_range(name))?;
            match width {
                IntWidth::I16 => Datum::from_i16(v as i16),
                IntWidth::I32 => Datum::from_i32(v as i32),
                IntWidth::I64 => Datum::from_i64(v as i64),
            }
        }
        Conv::DecimalToF32(src, scale) | Conv::DecimalToF64(src, scale) => {
            // Parsed at the target width (one rounding, as numeric -> real
            // / float8 round from the decimal text).
            let text = scaled_decimal_text(&decimal_unscaled_text(src, batch, k)?, i32::from(scale));
            if matches!(conv, Conv::DecimalToF32(..)) {
                let f: f32 = text.parse().map_err(|_| batch_shape())?;
                if f.is_infinite() {
                    return Err(out_of_range("real"));
                }
                Datum::from_f32(f)
            } else {
                let v: f64 = text.parse().map_err(|_| batch_shape())?;
                if v.is_infinite() {
                    return Err(out_of_range("double precision"));
                }
                Datum::from_f64(v)
            }
        }
        _ => return Err(batch_shape()),
    }))
}

/// Decoded cell -> datum. Cheap by-value conversions inline; the checked
/// arms error with input-function-parity messages.
pub(crate) fn convert_cell<'mcx>(
    mcx: Mcx<'mcx>,
    conv: Conv,
    batch: &ColumnBatch,
    k: usize,
    input: Option<&mut CellInput>,
) -> PgResult<Datum> {
    if let Some(d) = word_cell(conv, batch, k)? {
        return Ok(d);
    }
    if let Conv::ViaInput(form) = conv {
        let Some(ci) = input else {
            return Err(batch_shape());
        };
        let text = input_text(form, batch, k)?;
        let cstr = std::ffi::CString::new(text).map_err(|_| batch_shape())?;
        return types_fmgr::input_function_call(
            &mut ci.flinfo,
            Some(cstr.as_c_str()),
            ci.typioparam,
            ci.typmod,
            mcx,
        );
    }
    let bytes = batch.bytes_at(k);
    match conv {
        Conv::Bytea => varlena_datum(mcx, bytes),
        Conv::Text => {
            verify_text_bytes(bytes)?;
            varlena_datum(mcx, bytes)
        }
        Conv::VarcharN(maxlen) => {
            verify_text_bytes(bytes)?;
            varchar_datum(mcx, bytes, maxlen)
        }
        Conv::Bpchar(maxlen) => {
            verify_text_bytes(bytes)?;
            bpchar_datum(mcx, bytes, maxlen)
        }
        // word_cell returned None, so conv IS a text-family arm; this is
        // unreachable and fails closed if the two matches ever diverge.
        _ => Err(batch_shape()),
    }
}

/// The fused decoder's face of [`word_cell`]'s arms for one INT32-lane value — the same
/// constructors and range checks, arm for arm (the fused decoder hands
/// typed page runs, not batches). `Err` on any arm `word_cell` would not
/// take from an I32 lane (the caller falls back to the batch path).
#[inline(always)]
pub(crate) fn word_i32(conv: Conv, v: i32) -> PgResult<u64> {
    Ok(match conv {
        Conv::I32 => Datum::from_i32(v),
        Conv::I64FromI32 => Datum::from_i64(i64::from(v)),
        Conv::I16FromI32 => Datum::from_i16(i16::try_from(v).map_err(|_| out_of_range("smallint"))?),
        Conv::U32AsI32 => {
            let u = v as u32;
            Datum::from_i32(i32::try_from(u).map_err(|_| out_of_range("integer"))?)
        }
        Conv::U32AsI64 => Datum::from_i64(i64::from(v as u32)),
        Conv::DateFromDays => {
            Datum::from_i32(v.checked_sub(EPOCH_SHIFT_DAYS).ok_or_else(|| dt_out_of_range("date"))?)
        }
        Conv::TsFromEpochSecs(EpochLane::I32) => ts_from_unix_secs(i64::from(v))?,
        Conv::TsFromEpochSecs(EpochLane::U32) => ts_from_unix_secs(i64::from(v as u32))?,
        Conv::DateFromEpochDays(EpochLane::I32) => date_from_unix_days(i64::from(v))?,
        Conv::DateFromEpochDays(EpochLane::U32) => date_from_unix_days(i64::from(v as u32))?,
        _ => return Err(batch_shape()),
    }
    .as_u64())
}

/// The fused decoder's face of [`word_cell`]'s arms for one INT64-lane value.
#[inline(always)]
pub(crate) fn word_i64(conv: Conv, v: i64) -> PgResult<u64> {
    Ok(match conv {
        Conv::I64 => Datum::from_i64(v),
        Conv::I16FromI64 => Datum::from_i16(i16::try_from(v).map_err(|_| out_of_range("smallint"))?),
        Conv::I32FromI64 => Datum::from_i32(i32::try_from(v).map_err(|_| out_of_range("integer"))?),
        Conv::U64AsI64 => {
            if v < 0 {
                return Err(out_of_range("bigint"));
            }
            Datum::from_i64(v)
        }
        Conv::Timestamp(unit) => {
            let us = match unit {
                TimeUnit::Micros => v,
                TimeUnit::Millis => v.checked_mul(1000).ok_or_else(|| dt_out_of_range("timestamp"))?,
                TimeUnit::Nanos => v.div_euclid(1000),
            };
            ts_from_unix_us(us)?
        }
        Conv::TsFromEpochSecs(EpochLane::I64) => ts_from_unix_secs(v)?,
        Conv::TsFromEpochSecs(EpochLane::U64) => {
            if v < 0 {
                return Err(dt_out_of_range("timestamp"));
            }
            ts_from_unix_secs(v)?
        }
        Conv::DateFromEpochDays(EpochLane::I64) => date_from_unix_days(v)?,
        Conv::DateFromEpochDays(EpochLane::U64) => {
            if v < 0 {
                return Err(dt_out_of_range("date"));
            }
            date_from_unix_days(v)?
        }
        Conv::Time(unit) => {
            let us = match unit {
                TimeUnit::Micros => v,
                TimeUnit::Millis => v.checked_mul(1000).ok_or_else(|| dt_out_of_range("time"))?,
                TimeUnit::Nanos => v.div_euclid(1000),
            };
            if !(0..=MAX_TIME_US).contains(&us) {
                return Err(dt_out_of_range("time"));
            }
            Datum::from_i64(us)
        }
        _ => return Err(batch_shape()),
    }
    .as_u64())
}

/// The fused decoder's face of [`word_cell`]'s arms for one FLOAT-lane value.
#[inline(always)]
pub(crate) fn word_f32(conv: Conv, v: f32) -> PgResult<u64> {
    Ok(match conv {
        Conv::F32 => Datum::from_f32(v),
        Conv::F64FromF32 => Datum::from_f64(f64::from(v)),
        _ => return Err(batch_shape()),
    }
    .as_u64())
}

/// The fused decoder's face of [`word_cell`]'s arms for one DOUBLE-lane value.
#[inline(always)]
pub(crate) fn word_f64(conv: Conv, v: f64) -> PgResult<u64> {
    Ok(match conv {
        Conv::F64 => Datum::from_f64(v),
        _ => return Err(batch_shape()),
    }
    .as_u64())
}

/// The fused decoder's face of [`raw_cell`]'s text-family arm for one value, the image
/// appended to `arena` (4B-U header + payload + bpchar pad, the SAME shape
/// rules). `nul_free` = the page screen proved no 0x00 byte (UTF-8 validity
/// was proven at page decode), so the per-value verification is skipped;
/// otherwise the value is verified exactly as `raw_cell` does.
#[inline(always)]
pub(crate) fn text_image(conv: Conv, bytes: &[u8], nul_free: bool, arena: &mut Vec<u8>) -> PgResult<(u32, u32)> {
    if !nul_free && !matches!(conv, Conv::Bytea) {
        verify_text_bytes(bytes)?;
    }
    let (payload, pad): (&[u8], usize) = match conv {
        Conv::Text | Conv::Bytea => (bytes, 0),
        Conv::VarcharN(maxlen) => match varchar_cut(bytes, maxlen)? {
            None => (bytes, 0),
            Some(cut) => (&bytes[..cut], 0),
        },
        Conv::Bpchar(maxlen) => match bpchar_shape(bytes, maxlen)? {
            BpShape::Verbatim => (bytes, 0),
            BpShape::Cut(cut) => (&bytes[..cut], 0),
            BpShape::Pad(pad) => (bytes, pad),
        },
        _ => return Err(batch_shape()),
    };
    let off = arena.len();
    let total = VARHDRSZ_I32 as usize + payload.len() + pad;
    if off > u32::MAX as usize - total {
        return Err(batch_shape());
    }
    ::mcx::stdvec::try_reserve_more(arena, total, "fromparquet")?;
    arena.extend_from_slice(&datum::varlena::set_varsize_4b(total));
    arena.extend_from_slice(payload);
    if pad > 0 {
        ::mcx::stdvec::try_resize(arena, off + total, b' ', "fromparquet")?;
    }
    Ok((off as u32, total as u32))
}

/// Whether the fused decoder takes a conversion from a physical lane
/// (false: the batch path serves it).
pub(crate) fn fast_lane_ok(conv: Conv, phys: parquet_read::Phys) -> bool {
    use parquet_read::Phys as P;
    match phys {
        P::Int32 => matches!(
            conv,
            Conv::I32
                | Conv::I64FromI32
                | Conv::I16FromI32
                | Conv::U32AsI32
                | Conv::U32AsI64
                | Conv::DateFromDays
                | Conv::TsFromEpochSecs(EpochLane::I32 | EpochLane::U32)
                | Conv::DateFromEpochDays(EpochLane::I32 | EpochLane::U32)
        ),
        P::Int64 => matches!(
            conv,
            Conv::I64
                | Conv::I16FromI64
                | Conv::I32FromI64
                | Conv::U64AsI64
                | Conv::Timestamp(_)
                | Conv::Time(_)
                | Conv::TsFromEpochSecs(EpochLane::I64 | EpochLane::U64)
                | Conv::DateFromEpochDays(EpochLane::I64 | EpochLane::U64)
        ),
        P::Float => matches!(conv, Conv::F32 | Conv::F64FromF32),
        P::Double => matches!(conv, Conv::F64),
        P::ByteArray => matches!(conv, Conv::Text | Conv::VarcharN(_) | Conv::Bpchar(_) | Conv::Bytea),
        _ => false,
    }
}

/// One decoded cell in the pgrc writer-face currency, built WITHOUT a
/// memory context — the parallel decode workers' arm.
#[derive(Clone, Copy)]
pub(crate) enum RawCell {
    /// `Datum::as_u64()` of exactly the word [`convert_cell`] would build
    /// (same constructors, same range checks — via [`word_cell`]).
    Word(u64),
    /// `arena[off..off + len]` is a complete 4B-U varlena image — the same
    /// form `cstring_to_text` gives the serial row path, so the captured
    /// image bytes are identical, not merely normalize-equivalent.
    Bytes { off: u32, len: u32 },
}

/// Decoded cell -> raw writer-face cell. Text-family payloads go through
/// the SAME shape rules as the datum arm ([`varchar_cut`] /
/// [`bpchar_shape`]); the image lands in `arena`.
pub(crate) fn raw_cell(
    conv: Conv,
    batch: &ColumnBatch,
    k: usize,
    arena: &mut Vec<u8>,
) -> PgResult<RawCell> {
    if let Some(d) = word_cell(conv, batch, k)? {
        return Ok(RawCell::Word(d.as_u64()));
    }
    let bytes = batch.bytes_at(k);
    if !matches!(conv, Conv::Bytea) {
        // The serial arm's NUL/encoding verification, so a worker-built
        // image can never differ from (or out-admit) the serial datum.
        verify_text_bytes(bytes)?;
    }
    let (payload, pad): (&[u8], usize) = match conv {
        Conv::Text | Conv::Bytea => (bytes, 0),
        Conv::VarcharN(maxlen) => match varchar_cut(bytes, maxlen)? {
            None => (bytes, 0),
            Some(cut) => (&bytes[..cut], 0),
        },
        Conv::Bpchar(maxlen) => match bpchar_shape(bytes, maxlen)? {
            BpShape::Verbatim => (bytes, 0),
            BpShape::Cut(cut) => (&bytes[..cut], 0),
            BpShape::Pad(pad) => (bytes, pad),
        },
        // Same fail-closed arm as convert_cell's.
        _ => return Err(batch_shape()),
    };
    let off = arena.len();
    let total = VARHDRSZ_I32 as usize + payload.len() + pad;
    if off > u32::MAX as usize - total {
        // A single column chunk cannot legally reach 4GB of decoded text;
        // fail closed rather than wrap the span arithmetic.
        return Err(batch_shape());
    }
    arena.extend_from_slice(&datum::varlena::set_varsize_4b(total));
    arena.extend_from_slice(payload);
    ::mcx::stdvec::try_resize(arena, off + total, b' ', "fromparquet")?;
    Ok(RawCell::Bytes { off: off as u32, len: total as u32 })
}

/// Column-at-a-time [`raw_cell`] for a word column: batch rows `k0..k1`
/// appended to `words` / `nulls` (NULL rows carry a zero word), one typed
/// loop per conversion. The words are exactly `raw_cell`'s. Errors are
/// detection only (the serial replay raises the user-facing one).
pub(crate) fn words_lane(
    conv: Conv,
    batch: &ColumnBatch,
    k0: usize,
    k1: usize,
    notnull: bool,
    words: &mut Vec<u64>,
    nulls: &mut Vec<bool>,
) -> PgResult<()> {
    let bn = batch.nulls.get(k0..k1).ok_or_else(batch_shape)?;
    if notnull && bn.contains(&true) {
        return Err(Box::new(PgError::error(
            "not-null violation (serial replay owns the message)".to_string(),
        )));
    }
    ::mcx::stdvec::try_reserve_more(words, k1 - k0, "fromparquet")?;
    ::mcx::stdvec::try_reserve_more(nulls, k1 - k0, "fromparquet")?;
    nulls.extend_from_slice(bn);
    macro_rules! typed {
        ($variant:ident, |$v:ident| $e:expr) => {{
            let BatchData::$variant(src) = &batch.data else { return Err(batch_shape()) };
            let src = src.get(k0..k1).ok_or_else(batch_shape)?;
            for (&$v, &null) in src.iter().zip(bn) {
                words.push(if null { 0 } else { $e });
            }
        }};
    }
    match conv {
        Conv::Bool => typed!(Bool, |v| Datum::from_bool(v).as_u64()),
        Conv::I32 => typed!(I32, |v| Datum::from_i32(v).as_u64()),
        Conv::I64 => typed!(I64, |v| Datum::from_i64(v).as_u64()),
        Conv::I64FromI32 => typed!(I32, |v| Datum::from_i64(i64::from(v)).as_u64()),
        Conv::I16FromI32 => typed!(I32, |v| Datum::from_i16(i16::try_from(v).map_err(|_| out_of_range("smallint"))?).as_u64()),
        Conv::I16FromI64 => typed!(I64, |v| Datum::from_i16(i16::try_from(v).map_err(|_| out_of_range("smallint"))?).as_u64()),
        Conv::I32FromI64 => typed!(I64, |v| Datum::from_i32(i32::try_from(v).map_err(|_| out_of_range("integer"))?).as_u64()),
        Conv::U32AsI32 => typed!(I32, |v| Datum::from_i32(i32::try_from(v as u32).map_err(|_| out_of_range("integer"))?).as_u64()),
        Conv::U32AsI64 => typed!(I32, |v| Datum::from_i64(i64::from(v as u32)).as_u64()),
        Conv::F32 => typed!(F32, |v| Datum::from_f32(v).as_u64()),
        Conv::F64 => typed!(F64, |v| Datum::from_f64(v).as_u64()),
        Conv::F64FromF32 => typed!(F32, |v| Datum::from_f64(f64::from(v)).as_u64()),
        Conv::TsFromEpochSecs(EpochLane::I32) => typed!(I32, |v| ts_from_unix_secs(i64::from(v))?.as_u64()),
        Conv::TsFromEpochSecs(EpochLane::U32) => typed!(I32, |v| ts_from_unix_secs(i64::from(v as u32))?.as_u64()),
        Conv::TsFromEpochSecs(EpochLane::I64) => typed!(I64, |v| ts_from_unix_secs(v)?.as_u64()),
        Conv::DateFromEpochDays(EpochLane::I32) => typed!(I32, |v| date_from_unix_days(i64::from(v))?.as_u64()),
        Conv::DateFromEpochDays(EpochLane::U32) => typed!(I32, |v| date_from_unix_days(i64::from(v as u32))?.as_u64()),
        Conv::DateFromEpochDays(EpochLane::I64) => typed!(I64, |v| date_from_unix_days(v)?.as_u64()),
        _ => {
            for (k, &null) in (k0..k1).zip(bn) {
                words.push(if null {
                    0
                } else {
                    match word_cell(conv, batch, k)? {
                        Some(d) => d.as_u64(),
                        None => return Err(batch_shape()),
                    }
                });
            }
        }
    }
    Ok(())
}

/// Column-at-a-time [`raw_cell`] for a varlena column: batch rows
/// `k0..k1` as 4B-U images in `arena` (spans into it; NULL rows `(0, 0)`).
/// `utf8_checked` = the reader validated every value as UTF-8 at page
/// decode, so the database-encoding check reduces to its NUL rule.
#[allow(clippy::too_many_arguments)]
pub(crate) fn images_lane(
    conv: Conv,
    batch: &ColumnBatch,
    k0: usize,
    k1: usize,
    notnull: bool,
    utf8_checked: bool,
    arena: &mut Vec<u8>,
    spans: &mut Vec<(u32, u32)>,
    nulls: &mut Vec<bool>,
) -> PgResult<()> {
    let bn = batch.nulls.get(k0..k1).ok_or_else(batch_shape)?;
    if notnull && bn.contains(&true) {
        return Err(Box::new(PgError::error(
            "not-null violation (serial replay owns the message)".to_string(),
        )));
    }
    ::mcx::stdvec::try_reserve_more(spans, k1 - k0, "fromparquet")?;
    ::mcx::stdvec::try_reserve_more(nulls, k1 - k0, "fromparquet")?;
    nulls.extend_from_slice(bn);
    let plain = matches!(conv, Conv::Text) && utf8_checked;
    if let (true, BatchData::Bytes { ends, arena: src }) = (plain, &batch.data) {
        let ends = ends.get(k0..k1).ok_or_else(batch_shape)?;
        let mut start = if k0 == 0 { 0 } else { batch_end(batch, k0 - 1)? };
        let total = ends.last().map_or(0, |&e| e as usize - start) + 4 * (k1 - k0);
        ::mcx::stdvec::try_reserve_more(arena, total, "fromparquet")?;
        for (&end, &null) in ends.iter().zip(bn) {
            let end = end as usize;
            if null {
                spans.push((0, 0));
            } else {
                let bytes = src.get(start..end).ok_or_else(batch_shape)?;
                if bytes.contains(&0) {
                    return Err(Box::new(PgError::error(
                        "invalid byte sequence (serial replay owns the message)".to_string(),
                    )));
                }
                let off = arena.len();
                let len = VARHDRSZ_I32 as usize + bytes.len();
                if off > u32::MAX as usize - len {
                    return Err(batch_shape());
                }
                arena.extend_from_slice(&datum::varlena::set_varsize_4b(len));
                arena.extend_from_slice(bytes);
                spans.push((off as u32, len as u32));
            }
            start = end;
        }
        return Ok(());
    }
    for (k, &null) in (k0..k1).zip(bn) {
        if null {
            spans.push((0, 0));
            continue;
        }
        match raw_cell(conv, batch, k, arena)? {
            RawCell::Bytes { off, len } => spans.push((off, len)),
            RawCell::Word(_) => return Err(batch_shape()),
        }
    }
    Ok(())
}

fn batch_end(batch: &ColumnBatch, k: usize) -> PgResult<usize> {
    match &batch.data {
        BatchData::Bytes { ends, .. } => ends.get(k).map(|&e| e as usize).ok_or_else(batch_shape),
        _ => Err(batch_shape()),
    }
}

#[cfg(test)]
mod logical_arm_tests {
    use super::*;

    #[test]
    fn decimal_text_from_bytes_any_width() {
        assert_eq!(be_int_to_decimal(&[]), "0");
        assert_eq!(be_int_to_decimal(&[0x7b]), "123");
        assert_eq!(be_int_to_decimal(&[0xff, 0x85]), "-123");
        assert_eq!(be_int_to_decimal(&[0x80]), "-128");
        let mut big = [0u8; 20];
        big[19] = 1;
        assert_eq!(be_int_to_decimal(&big), "1");
        // 2^159 = 730750818665451459101842416358141509827966271488
        let mut p = [0u8; 20];
        p[0] = 0x80;
        assert_eq!(be_int_to_decimal(&p), "-730750818665451459101842416358141509827966271488");
        let mut q = [0u8; 21];
        q[1] = 0x80;
        assert_eq!(be_int_to_decimal(&q), "730750818665451459101842416358141509827966271488");
        let all_ff = [0xffu8; 24];
        assert_eq!(be_int_to_decimal(&all_ff), "-1");
        // Negative, length not a multiple of 4 (the FLBA(22) of a DECIMAL(50)).
        let mut neg22 = [0xffu8; 22];
        neg22[21] = 0xfe;
        assert_eq!(be_int_to_decimal(&neg22), "-2");
        let mut neg21 = [0u8; 21];
        neg21[0] = 0xff;
        neg21[1] = 0x7f;
        assert_eq!(be_int_to_decimal(&neg21), "-736459809436275298626075560235939490373497257984");
        assert_eq!(scaled_decimal_text("12345", 2), "123.45");
        assert_eq!(scaled_decimal_text("-5", 3), "-0.005");
        assert_eq!(scaled_decimal_text("5", 0), "5");
        assert_eq!(scaled_decimal_text("5", -2), "500");
        assert_eq!(scaled_decimal_text("0", 4), "0.0000");
    }

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert_eq!(f16_to_f32(0x7bff), 65504.0);
        assert_eq!(f16_to_f32(0x8000).to_bits(), (-0.0f32).to_bits());
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn int96_and_millis_time() {
        let mut b = ColumnBatch::new_for(Phys::Int96);
        // 2000-01-01 00:00:01.5 = JDN 2451545, 1.5e9 ns.
        b.nulls.push(false);
        if let BatchData::I96(v) = &mut b.data {
            v.push([1_500_000_000u32, 0, 2_451_545]);
        }
        let d = word_cell(Conv::TsFromInt96, &b, 0).unwrap().unwrap();
        assert_eq!(d.as_i64(), 1_500_000);
        let mut t = ColumnBatch::new_for(Phys::Int32);
        t.nulls.push(false);
        t.nulls.push(false);
        if let BatchData::I32(v) = &mut t.data {
            v.push(1_000);
            v.push(-1);
        }
        assert_eq!(word_cell(Conv::TimeMillis32, &t, 0).unwrap().unwrap().as_i64(), 1_000_000);
        assert!(word_cell(Conv::TimeMillis32, &t, 1).is_err());
    }

    #[test]
    fn decimal_to_int_and_float() {
        let mut b = ColumnBatch::new_for(Phys::Int64);
        b.nulls.extend([false, false]);
        if let BatchData::I64(v) = &mut b.data {
            v.push(70_000);
            v.push(-12_345);
        }
        assert!(word_cell(Conv::DecimalToInt(DecimalSrc::I64, IntWidth::I16), &b, 0).is_err());
        assert_eq!(
            word_cell(Conv::DecimalToInt(DecimalSrc::I64, IntWidth::I32), &b, 0)
                .unwrap()
                .unwrap()
                .as_i32(),
            70_000
        );
        let f = word_cell(Conv::DecimalToF64(DecimalSrc::I64, 2), &b, 1).unwrap().unwrap();
        assert_eq!(f.as_f64(), -123.45);
    }
}

#[cfg(test)]
mod raw_cell_tests {
    //! The parallel-capture equivalence pins: [`raw_cell`] must produce
    //! EXACTLY what the serial row path stores — the same Datum word for
    //! every byval arm (shared word_cell makes this structural; the pins
    //! keep it honest against refactors) and the same varlena IMAGE bytes
    //! for every text-family arm (same 4B-U form as cstring_to_text, same
    //! trim/pad shapes) — including the same ERRORS on the checked arms.
    use super::*;
    use mcx::MemoryContext;
    use parquet_read::BatchData;
    use types_tuple::varatt::varsize_any;

    fn datum_image(d: Datum) -> Vec<u8> {
        let p = d.as_u64() as *const u8;
        // SAFETY: the datum was built by convert_cell's varlena arm in the
        // live test mcx; varsize_any bounds the image read.
        unsafe {
            let len = varsize_any(p);
            core::slice::from_raw_parts(p, len).to_vec()
        }
    }

    fn pin_text(conv: Conv, batch: &ColumnBatch, k: usize) {
        let ctx = MemoryContext::new("raw-cell-pin");
        let mcx = ctx.mcx();
        let mut arena = Vec::new();
        let raw = raw_cell(conv, batch, k, &mut arena);
        let datum = convert_cell(mcx, conv, batch, k, None);
        match (raw, datum) {
            (Ok(RawCell::Bytes { off, len }), Ok(d)) => {
                assert_eq!(
                    &arena[off as usize..(off + len) as usize],
                    &datum_image(d)[..],
                    "raw image != serial image"
                );
            }
            (Err(re), Err(de)) => {
                assert_eq!(re.message(), de.message(), "error parity");
            }
            (r, d) => panic!(
                "shape mismatch: raw ok={} datum ok={}",
                r.is_ok(),
                d.is_ok()
            ),
        }
    }

    #[test]
    fn text_family_images_match_serial() {
        let b = ColumnBatch {
            nulls: vec![false; 4],
            data: BatchData::Bytes {
                // Rows: "hey" / "widevalue-with-more-bytes" / "é" / "  ok".
                arena: b"heywidevalue-with-more-bytes\xc3\xa9  ok".to_vec(),
                ends: vec![3, 28, 30, 34],
            },
        };
        for k in 0..4 {
            pin_text(Conv::Text, &b, k);
            pin_text(Conv::Bytea, &b, k);
            // varchar(2): "hey" errors, others trim/pass.
            pin_text(Conv::VarcharN(2), &b, k);
            pin_text(Conv::VarcharN(40), &b, k);
            // bpchar pads short values, blank-trims over-length ones.
            pin_text(Conv::Bpchar(2), &b, k);
            pin_text(Conv::Bpchar(6), &b, k);
            pin_text(Conv::Bpchar(-1), &b, k);
        }
    }

    #[test]
    fn word_arms_match_serial() {
        let ctx = MemoryContext::new("raw-cell-words");
        let mcx = ctx.mcx();
        let b32 = ColumnBatch { nulls: vec![false; 2], data: BatchData::I32(vec![-7, 12345]) };
        let b64 =
            ColumnBatch { nulls: vec![false; 2], data: BatchData::I64(vec![-1, 4_102_444_800]) };
        let bf = ColumnBatch { nulls: vec![false; 1], data: BatchData::F32(vec![1.5]) };
        let bd = ColumnBatch { nulls: vec![false; 1], data: BatchData::F64(vec![-2.25]) };
        let bb = ColumnBatch { nulls: vec![false; 1], data: BatchData::Bool(vec![true]) };
        let cases: &[(Conv, &ColumnBatch, usize)] = &[
            (Conv::Bool, &bb, 0),
            (Conv::I16FromI32, &b32, 0),
            (Conv::I32, &b32, 0),
            (Conv::I64FromI32, &b32, 1),
            (Conv::I16FromI64, &b64, 0),
            (Conv::I32FromI64, &b64, 0),
            (Conv::I64, &b64, 1),
            (Conv::F32, &bf, 0),
            (Conv::F64FromF32, &bf, 0),
            (Conv::F64, &bd, 0),
            (Conv::DateFromDays, &b32, 1),
            (Conv::Timestamp(TimeUnit::Micros), &b64, 1),
            (Conv::Time(TimeUnit::Micros), &b64, 1),
            (Conv::U32AsI32, &b32, 1),
            (Conv::U32AsI64, &b32, 0),
            (Conv::U64AsI64, &b64, 1),
            (Conv::TsFromEpochSecs(EpochLane::I64), &b64, 1),
            (Conv::DateFromEpochDays(EpochLane::I32), &b32, 1),
        ];
        for &(conv, batch, k) in cases {
            let mut arena = Vec::new();
            match (raw_cell(conv, batch, k, &mut arena), convert_cell(mcx, conv, batch, k, None)) {
                (Ok(RawCell::Word(w)), Ok(d)) => {
                    assert_eq!(w, d.as_u64(), "word parity for the arm");
                }
                (Err(re), Err(de)) => assert_eq!(re.message(), de.message()),
                (r, d) => panic!("shape mismatch: raw ok={} datum ok={}", r.is_ok(), d.is_ok()),
            }
            assert!(arena.is_empty(), "word arms must not touch the arena");
        }
    }
}

#[cfg(test)]
mod epoch_tests {
    use super::*;
    use mcx::MemoryContext;
    use parquet_read::BatchData;

    // The validity window in Unix-seconds terms, derived from the µs window
    // and the 10,957-day epoch shift (independently recomputed here so a
    // constant typo cannot self-confirm).
    const UNIX_SECS_MIN: i64 = MIN_TIMESTAMP_US / 1_000_000 + 946_684_800;
    const UNIX_SECS_MAX: i64 = END_TIMESTAMP_US / 1_000_000 + 946_684_800 - 1;

    fn ts_us(secs: i64) -> i64 {
        match ts_from_unix_secs(secs) {
            Ok(d) => {
                assert_eq!(d, Datum::from_i64((secs - 946_684_800) * 1_000_000));
                (secs - 946_684_800) * 1_000_000
            }
            Err(e) => panic!("unexpected out-of-range for {secs}: {}", e.message()),
        }
    }

    #[test]
    fn epoch_seconds_values_and_window() {
        assert_eq!(ts_us(0), -946_684_800_000_000); // 1970-01-01
        assert_eq!(ts_us(946_684_800), 0); // the engine epoch
        assert_eq!(ts_us(-1), -946_684_801_000_000); // pre-1970
        assert_eq!(ts_us(2_147_483_648), 1_200_798_848_000_000); // past 2038
        // Exact window edges (inclusive both ends in seconds terms).
        assert_eq!(UNIX_SECS_MIN, -210_866_803_200);
        assert_eq!(UNIX_SECS_MAX, 9_224_318_015_999);
        ts_us(UNIX_SECS_MIN);
        ts_us(UNIX_SECS_MAX);
        for bad in [
            UNIX_SECS_MIN - 1,
            UNIX_SECS_MAX + 1,
            9_300_000_000_000_000, // scale overflow
            i64::MAX,
            i64::MIN, // shift underflow
        ] {
            assert!(ts_from_unix_secs(bad).is_err(), "{bad} must be out of range");
        }
    }

    #[test]
    fn epoch_days_values_and_bounds() {
        let day = |d: i64| date_from_unix_days(d).unwrap();
        assert_eq!(day(0), Datum::from_i32(-10_957)); // 1970-01-01
        assert_eq!(day(10_957), Datum::from_i32(0)); // 2000-01-01
        assert_eq!(day(-1), Datum::from_i32(-10_958)); // 1969-12-31
        assert_eq!(day(2_932_896), Datum::from_i32(2_921_939)); // 9999-12-31
        assert_eq!(day(-719_162), Datum::from_i32(-730_119)); // 0001-01-01
        // The i32 datum bound after the shift, and both i64 extremes.
        assert!(date_from_unix_days(i64::from(i32::MAX) + 10_958).is_err());
        assert!(date_from_unix_days(3_000_000_000).is_err());
        assert!(date_from_unix_days(i64::MIN).is_err());
        // IS_VALID_DATE: the window ends before the i32 datum bound, so the
        // DATEVAL_NOEND sentinel (i32::MAX) is never produced from a finite day.
        assert!(date_from_unix_days(i64::from(i32::MAX) + 10_957).is_err());
        assert_eq!(day(2_145_031_948 + 10_957), Datum::from_i32(2_145_031_948)); // 5874897-12-31
        assert!(date_from_unix_days(2_145_031_949 + 10_957).is_err());
        assert_eq!(day(-2_451_545 + 10_957), Datum::from_i32(-2_451_545)); // 4714-11-24 BC
        assert!(date_from_unix_days(-2_451_546 + 10_957).is_err());
    }

    #[test]
    fn epoch_lane_widening() {
        let ctx = MemoryContext::new("epoch-lane-test");
        let mcx = ctx.mcx();
        let b32 = ColumnBatch { nulls: vec![false; 2], data: BatchData::I32(vec![-1, 86_400]) };
        let b64 =
            ColumnBatch { nulls: vec![false; 2], data: BatchData::I64(vec![-1, 4_102_444_800]) };
        // Signed i32 lane: -1 is one second before 1970.
        assert_eq!(
            convert_cell(mcx, Conv::TsFromEpochSecs(EpochLane::I32), &b32, 0, None).unwrap(),
            Datum::from_i64(-946_684_801_000_000)
        );
        // Unsigned i32 lane: the same bits zero-extend to u32::MAX (2106).
        assert_eq!(
            convert_cell(mcx, Conv::TsFromEpochSecs(EpochLane::U32), &b32, 0, None).unwrap(),
            Datum::from_i64((4_294_967_295 - 946_684_800) * 1_000_000)
        );
        // i64 lane, past 2038.
        assert_eq!(
            convert_cell(mcx, Conv::TsFromEpochSecs(EpochLane::I64), &b64, 1, None).unwrap(),
            Datum::from_i64((4_102_444_800 - 946_684_800) * 1_000_000)
        );
        // u64 lane: the sign bit means > i64::MAX seconds — out of range.
        assert!(convert_cell(mcx, Conv::TsFromEpochSecs(EpochLane::U64), &b64, 0, None).is_err());
        assert!(convert_cell(mcx, Conv::DateFromEpochDays(EpochLane::U64), &b64, 0, None).is_err());
        // Day lanes through the same widening: u32::MAX days does not fit
        // the i32 date datum after the shift — out of range, not wrap.
        assert!(convert_cell(mcx, Conv::DateFromEpochDays(EpochLane::U32), &b32, 0, None).is_err());
        assert_eq!(
            convert_cell(mcx, Conv::DateFromEpochDays(EpochLane::U32), &b32, 1, None).unwrap(),
            Datum::from_i32(86_400 - 10_957)
        );
        assert_eq!(
            convert_cell(mcx, Conv::DateFromEpochDays(EpochLane::I32), &b32, 1, None).unwrap(),
            Datum::from_i32(86_400 - 10_957)
        );
    }

    #[test]
    fn epoch_admission_is_opt_in_and_annotation_wins() {
        use types_core::catalog::{DATEOID, INT8OID, TIMESTAMPOID, TIMESTAMPTZOID};
        let col = |phys, logical| parquet_read::ColumnSchema::flat("c", phys, 0, logical);
        let plain64 = col(Phys::Int64, Logical::None);
        // Opt-in: the temporal target admits only under the flag.
        assert!(matches!(
            resolve_conv(&plain64, TIMESTAMPOID, -1, "c", true),
            Ok(Conv::TsFromEpochSecs(EpochLane::I64))
        ));
        assert!(matches!(
            resolve_conv(&plain64, TIMESTAMPTZOID, -1, "c", true),
            Ok(Conv::TsFromEpochSecs(EpochLane::I64))
        ));
        assert!(matches!(
            resolve_conv(&plain64, DATEOID, -1, "c", true),
            Ok(Conv::DateFromEpochDays(EpochLane::I64))
        ));
        // The flag never disturbs non-temporal targets.
        assert!(matches!(resolve_conv(&plain64, INT8OID, -1, "c", true), Ok(Conv::I64)));
        // Unsigned annotations pick the unsigned lanes.
        let u16d = col(Phys::Int32, Logical::Int { bits: 16, signed: false });
        assert!(matches!(
            resolve_conv(&u16d, DATEOID, -1, "c", true),
            Ok(Conv::DateFromEpochDays(EpochLane::U32))
        ));
        let u64c = col(Phys::Int64, Logical::Int { bits: 64, signed: false });
        assert!(matches!(
            resolve_conv(&u64c, TIMESTAMPOID, -1, "c", true),
            Ok(Conv::TsFromEpochSecs(EpochLane::U64))
        ));
        let i32c = col(Phys::Int32, Logical::None);
        assert!(matches!(
            resolve_conv(&i32c, TIMESTAMPOID, -1, "c", true),
            Ok(Conv::TsFromEpochSecs(EpochLane::I32))
        ));
        // Temporal annotations keep their mapping under the flag.
        let annts =
            col(Phys::Int64, Logical::Timestamp { unit: TimeUnit::Millis, utc: true });
        assert!(matches!(
            resolve_conv(&annts, TIMESTAMPOID, -1, "c", true),
            Ok(Conv::Timestamp(TimeUnit::Millis))
        ));
        let annd = col(Phys::Int32, Logical::Date);
        assert!(matches!(
            resolve_conv(&annd, DATEOID, -1, "c", true),
            Ok(Conv::DateFromDays)
        ));
    }
}

#[cfg(test)]
mod text_nul_tests {
    use super::*;
    use mcx::MemoryContext;
    use parquet_read::BatchData;

    // Two rows: an embedded-NUL string and a clean one, laid out in the arena
    // exactly as the batch reader would (ends[i] is the arena end of row i).
    fn bytes_batch() -> ColumnBatch {
        let arena = b"admin\0evilok".to_vec();
        ColumnBatch {
            nulls: vec![false, false],
            data: BatchData::Bytes { ends: vec![10, 12], arena },
        }
    }

    // Text-class conversions must reject an embedded NUL (and accept the clean
    // value), matching pg_verifymbstr on the engine's other text input paths;
    // bytea is exempt and keeps the raw bytes.
    #[test]
    fn embedded_nul_rejected_for_text_classes_but_not_bytea() {
        mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
        let ctx = MemoryContext::new("text-nul-test");
        let mcx = ctx.mcx();
        let b = bytes_batch();

        for conv in [Conv::Text, Conv::VarcharN(64), Conv::Bpchar(64), Conv::Bpchar(-1)] {
            // Row 0 carries "admin\0evil": rejected as an invalid byte sequence.
            let err = convert_cell(mcx, conv, &b, 0, None).err().unwrap();
            assert_eq!(err.sqlstate(), types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE);
            // Row 1 is clean and must convert.
            assert!(convert_cell(mcx, conv, &b, 1, None).is_ok());
        }

        // bytea accepts arbitrary bytes, NUL included.
        assert!(convert_cell(mcx, Conv::Bytea, &b, 0, None).is_ok());
    }
}
