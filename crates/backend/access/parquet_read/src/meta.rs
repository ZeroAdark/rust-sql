//! Parquet footer (FileMetaData) skip-parse: the schema tree flattened to
//! its leaves (with definition/repetition depths and ancestry), per-chunk
//! {file offsets, codec, value counts, dict-page offset, page-index and
//! bloom-filter locations}. Statistics, encoding stats and key/value
//! metadata are skipped byte-wise, never decoded.

use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED};

use crate::thrift::{corrupt, Cur, T_BINARY, T_BYTE, T_I32, T_I64, T_LIST, T_STRUCT};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phys {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    /// FIXED_LEN_BYTE_ARRAY(type_length).
    Flba(u32),
}

impl Phys {
    pub fn name(self) -> &'static str {
        match self {
            Phys::Boolean => "BOOLEAN",
            Phys::Int32 => "INT32",
            Phys::Int64 => "INT64",
            Phys::Int96 => "INT96",
            Phys::Float => "FLOAT",
            Phys::Double => "DOUBLE",
            Phys::ByteArray => "BYTE_ARRAY",
            Phys::Flba(_) => "FIXED_LEN_BYTE_ARRAY",
        }
    }

    /// Fixed byte width of PLAIN-encoded values, `None` for BYTE_ARRAY.
    pub fn plain_width(self) -> Option<usize> {
        match self {
            Phys::Boolean => None, // bit-packed, handled separately
            Phys::Int32 | Phys::Float => Some(4),
            Phys::Int64 | Phys::Double => Some(8),
            Phys::Int96 => Some(12),
            Phys::ByteArray => None,
            Phys::Flba(n) => Some(n as usize),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeUnit {
    Millis,
    Micros,
    Nanos,
}

/// The logical annotation the reader acts on. LogicalType (schema field 10)
/// wins over the legacy ConvertedType (field 6) when both are present.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logical {
    None,
    String,
    Enum,
    Json,
    Bson,
    Uuid,
    Date,
    Time { unit: TimeUnit },
    Timestamp { unit: TimeUnit, utc: bool },
    Int { bits: u8, signed: bool },
    Decimal { precision: i32, scale: i32 },
    Interval,
    Float16,
    /// Group annotations (never on a leaf; carried in `Ancestor::logical`).
    List,
    Map,
    /// Logical UNKNOWN (always-null) or an annotation this reader has no
    /// mapping for; binding decides whether that matters.
    Other,
}

impl Logical {
    pub fn name(self) -> &'static str {
        match self {
            Logical::None => "",
            Logical::String => "STRING",
            Logical::Enum => "ENUM",
            Logical::Json => "JSON",
            Logical::Bson => "BSON",
            Logical::Uuid => "UUID",
            Logical::Date => "DATE",
            Logical::Time { .. } => "TIME",
            Logical::Timestamp { .. } => "TIMESTAMP",
            Logical::Int { .. } => "INT",
            Logical::Decimal { .. } => "DECIMAL",
            Logical::Interval => "INTERVAL",
            Logical::Float16 => "FLOAT16",
            Logical::List => "LIST",
            Logical::Map => "MAP",
            Logical::Other => "UNKNOWN",
        }
    }
}

/// Field repetition (SchemaElement.repetition_type).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repetition {
    Required,
    Optional,
    Repeated,
}

/// One group on the path from the schema root to a leaf (root excluded).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ancestor {
    pub name: String,
    pub repetition: Repetition,
    /// LIST / MAP / None (MAP_KEY_VALUE folds to None: it is the repeated
    /// group under a MAP, recognizable by `repetition`).
    pub logical: Logical,
    /// Definition level a value reaches when this group is present.
    pub def_level: u16,
    /// Repetition level of this group (its own count of REPEATED ancestors,
    /// itself included).
    pub rep_level: u16,
}

/// One leaf column of the file schema (schema-leaf order).
pub struct ColumnSchema {
    /// Leaf name for root children; the dotted path (`a.b.c`) for nested
    /// leaves, so name matching stays a flat string comparison.
    pub name: String,
    pub phys: Phys,
    /// Maximum definition level: one per OPTIONAL or REPEATED node on the
    /// path, the leaf included.
    pub max_def: u16,
    pub logical: Logical,
    /// Maximum repetition level: one per REPEATED node on the path.
    pub max_rep: u16,
    /// The leaf's own repetition.
    pub repetition: Repetition,
    /// Full path from the root (root excluded), the leaf last.
    pub path: Vec<String>,
    /// Groups above the leaf, outermost first (empty for root children).
    pub ancestors: Vec<Ancestor>,
}

impl ColumnSchema {
    /// A root-child leaf (no groups above it): the shape tests and tooling
    /// build by hand.
    pub fn flat(name: &str, phys: Phys, max_def: u16, logical: Logical) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            phys,
            max_def,
            logical,
            max_rep: 0,
            repetition: if max_def == 0 { Repetition::Required } else { Repetition::Optional },
            path: vec![name.to_string()],
            ancestors: Vec::new(),
        }
    }

    /// "INT64 (TIMESTAMP)"-style description for schema-mismatch errors.
    pub fn type_desc(&self) -> String {
        if self.logical == Logical::None {
            self.phys.name().to_string()
        } else {
            format!("{} ({})", self.phys.name(), self.logical.name())
        }
    }

    /// True when the leaf is a root child or sits under STRUCT groups only:
    /// one value slot per row, so a row-dense batch represents it exactly.
    pub fn is_flat(&self) -> bool {
        self.max_rep == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodecId {
    Uncompressed,
    Snappy,
    Gzip,
    Lzo,
    Brotli,
    /// Codec 5: Hadoop-framed LZ4 by the spec, a bare raw block from some
    /// writers; the decoder tells them apart structurally.
    Lz4Framed,
    Zstd,
    Lz4Raw,
}

impl CodecId {
    fn from_i32(v: i32) -> PgResult<CodecId> {
        Ok(match v {
            0 => CodecId::Uncompressed,
            1 => CodecId::Snappy,
            2 => CodecId::Gzip,
            3 => CodecId::Lzo,
            4 => CodecId::Brotli,
            5 => CodecId::Lz4Framed,
            6 => CodecId::Zstd,
            7 => CodecId::Lz4Raw,
            _ => return Err(corrupt("unrecognized compression codec")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            CodecId::Uncompressed => "UNCOMPRESSED",
            CodecId::Snappy => "SNAPPY",
            CodecId::Gzip => "GZIP",
            CodecId::Lzo => "LZO",
            CodecId::Brotli => "BROTLI",
            CodecId::Lz4Framed => "LZ4",
            CodecId::Zstd => "ZSTD",
            CodecId::Lz4Raw => "LZ4_RAW",
        }
    }
}

/// A byte range inside the file (page index / bloom filter locations).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileSpan {
    pub offset: i64,
    pub length: i64,
}

/// Per column chunk: exactly what the page walker needs, plus the
/// locations of the on-demand structures (never read on the ingest path).
pub struct ChunkMeta {
    /// Ordinal into `FileMeta::columns` (resolved via path_in_schema).
    pub column: usize,
    pub codec: CodecId,
    pub num_values: i64,
    pub data_page_offset: i64,
    /// Some writers leave this unset while still writing a dictionary page
    /// at the chunk start; the page walker keys off page headers, not this.
    pub dict_page_offset: Option<i64>,
    pub total_compressed_size: i64,
    pub offset_index: Option<FileSpan>,
    pub column_index: Option<FileSpan>,
    pub bloom_filter: Option<FileSpan>,
}

impl ChunkMeta {
    /// First byte of the chunk in the file. A set-but-larger
    /// dictionary_page_offset (a known writer quirk) is ignored.
    pub fn start_offset(&self) -> i64 {
        match self.dict_page_offset {
            Some(d) if d > 0 && d < self.data_page_offset => d,
            _ => self.data_page_offset,
        }
    }
}

pub struct RowGroupMeta {
    pub num_rows: i64,
    pub chunks: Vec<ChunkMeta>,
}

pub struct FileMeta {
    /// memsig E-pub t5 (memlead 2026-10-15T21:30Z): the parsed footer's heap bytes ([`FileMeta::heap_bytes`]) on the
    /// loading statement's owner, synced on the parsing thread as the parse ends. FIRST (rule RCF): the last `Arc`'s drop,
    /// on whichever decode thread, moves the bytes to that thread's account before the fields free them.
    held: crate::held::HeldCharge,
    pub num_rows: i64,
    pub columns: Vec<ColumnSchema>,
    pub row_groups: Vec<RowGroupMeta>,
    pub created_by: Option<String>,
}

impl FileMeta {
    /// The heap bytes the parsed footer holds: every list's and string's CAPACITY (memlead 21:30Z (2)), down to each
    /// column's path and ancestors and each row group's chunk list.
    pub fn heap_bytes(&self) -> u64 {
        use crate::held::cap_bytes;
        let s = |x: &String| x.capacity() as u64;
        let mut b = cap_bytes(self.columns.capacity(), &self.columns) + cap_bytes(self.row_groups.capacity(), &self.row_groups) + self.created_by.as_ref().map_or(0, s);
        for c in &self.columns {
            b += s(&c.name) + cap_bytes(c.path.capacity(), &c.path) + c.path.iter().map(s).sum::<u64>();
            b += cap_bytes(c.ancestors.capacity(), &c.ancestors) + c.ancestors.iter().map(|a| s(&a.name)).sum::<u64>();
        }
        for rg in &self.row_groups {
            b += cap_bytes(rg.chunks.capacity(), &rg.chunks);
        }
        b
    }

    /// The bytes the footer's charge holds now (0 uncharged).
    pub fn held_bytes(&self) -> u64 {
        self.held.charged()
    }
}

#[cold]
#[inline(never)]
pub(crate) fn unsupported(what: String) -> Box<PgError> {
    Box::new(
        PgError::error(format!("unsupported parquet feature: {what}")) // alloc: Cold
            .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
    )
}

/// Parse FileMetaData from the footer bytes (between the leading magic and
/// the trailing 8-byte length+magic). Uncharged: [`parse_file_meta_on`] with no owner.
pub fn parse_file_meta(buf: &[u8]) -> PgResult<FileMeta> {
    parse_file_meta_on(buf, None)
}

/// memsig E-pub t5: [`parse_file_meta`] with the parsed footer's heap bytes on `owner` (`FileMeta::held`), and the parse's
/// scratch lists (the schema walk's stack, a chunk's path parts) charged to it while they live.
pub fn parse_file_meta_on(buf: &[u8], owner: Option<memledger::Owner>) -> PgResult<FileMeta> {
    let mut cur = Cur::new(buf);
    let mut version: Option<i32> = None;
    let mut num_rows: Option<i64> = None;
    let mut schema: Option<Vec<ColumnSchema>> = None;
    let mut row_groups: Option<Vec<RowGroupMeta>> = None;
    let mut created_by: Option<String> = None;

    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            1 => version = Some(cur.i32_value(t)?),
            2 => {
                if t != T_LIST {
                    return Err(corrupt("schema is not a list"));
                }
                schema = Some(parse_schema(&mut cur, owner)?);
            }
            3 => num_rows = Some(cur.i64_value(t)?),
            4 => {
                if t != T_LIST {
                    return Err(corrupt("row_groups is not a list"));
                }
                let (et, n) = cur.list_header()?;
                if et != T_STRUCT {
                    return Err(corrupt("row_groups element is not a struct"));
                }
                let ncols = schema
                    .as_ref()
                    .ok_or_else(|| corrupt("row_groups precede schema"))?
                    .len();
                let mut rgs = Vec::new();
                rgs.try_reserve(n).map_err(|_| corrupt("row group list too large"))?; // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                for _ in 0..n {
                    rgs.push(parse_row_group(&mut cur, schema.as_ref().unwrap(), ncols, owner)?); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                }
                row_groups = Some(rgs);
            }
            6 => {
                created_by =
                    Some(String::from_utf8_lossy(cur.binary_value(t)?).into_owned()); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
            }
            8 => {
                // EncryptionAlgorithm on a plaintext-footer file: columns are
                // encrypted even though the footer is readable.
                return Err(unsupported("encrypted columns".to_string())); // alloc: Cold
            }
            _ => cur.skip(t, 0)?,
        }
    }

    let _ = version;
    let (Some(columns), Some(num_rows)) = (schema, num_rows) else {
        return Err(corrupt("FileMetaData missing schema or num_rows"));
    };
    let row_groups = row_groups.unwrap_or_default();
    if num_rows < 0 {
        return Err(corrupt("negative num_rows"));
    }
    let rg_rows: i64 = row_groups.iter().map(|rg| rg.num_rows).try_fold(0i64, |a, b| {
        a.checked_add(b).ok_or_else(|| corrupt("row counts overflow"))
    })?;
    // An early parquet-rs left FileMetaData.num_rows at zero; the row
    // groups are authoritative then. Any other disagreement is corruption.
    let num_rows = if num_rows == 0 { rg_rows } else { num_rows };
    if rg_rows != num_rows {
        return Err(corrupt("row group row counts do not sum to num_rows"));
    }
    let mut meta = FileMeta { held: crate::held::HeldCharge::new(owner), num_rows, columns, row_groups, created_by };
    let held = meta.heap_bytes();
    meta.held.sync(held);
    Ok(meta)
}

// SchemaElement field ids.
const SE_TYPE: i16 = 1;
const SE_TYPE_LENGTH: i16 = 2;
const SE_REPETITION: i16 = 3;
const SE_NAME: i16 = 4;
const SE_NUM_CHILDREN: i16 = 5;
const SE_CONVERTED: i16 = 6;
const SE_SCALE: i16 = 7;
const SE_PRECISION: i16 = 8;
const SE_LOGICAL: i16 = 10;

struct SchemaElem {
    name: String,
    phys: Option<Phys>,
    repetition: Repetition,
    num_children: usize,
    logical: Logical,
}

/// Groups nest at most this deep (the spec has no limit; real schemas stop
/// well short of it, and the level lanes are u16).
const MAX_SCHEMA_DEPTH: usize = 64;

/// Parse the flattened, depth-first schema-element list into its leaves.
/// Every element's `num_children` delimits its subtree; a walk with an
/// explicit stack assigns each leaf its path, ancestry and level depths.
fn parse_schema(cur: &mut Cur<'_>, owner: Option<memledger::Owner>) -> PgResult<Vec<ColumnSchema>> {
    let (et, n) = cur.list_header()?;
    if et != T_STRUCT {
        return Err(corrupt("schema element is not a struct"));
    }
    if n == 0 {
        return Err(corrupt("empty schema"));
    }
    let root = parse_schema_element(cur)?;
    if root.num_children == 0 {
        return Err(corrupt("schema root has no columns"));
    }
    struct Frame {
        children_left: usize,
        anc: Ancestor,
    }
    // memsig E-pub t5: the walk's open groups (at most MAX_SCHEMA_DEPTH), on the load's owner while the walk lives
    let mut stack: memledger::ChargedVec<Frame> = memledger::ChargedVec::new(owner, memledger::Kind::Region);
    let mut root_left = root.num_children;
    let mut cols: Vec<ColumnSchema> = Vec::new();
    let mut consumed = 1usize;
    while root_left > 0 || !stack.is_empty() {
        if consumed >= n {
            return Err(corrupt("schema list shorter than its child counts"));
        }
        let el = parse_schema_element(cur)?;
        consumed += 1;
        // Charge this element to its parent.
        match stack.as_mut_slice().last_mut() {
            Some(f) => f.children_left -= 1,
            None => root_left -= 1,
        }
        let (parent_def, parent_rep) = match stack.last() {
            Some(f) => (f.anc.def_level, f.anc.rep_level),
            None => (0u16, 0u16),
        };
        let def_level = parent_def + u16::from(el.repetition != Repetition::Required);
        let rep_level = parent_rep + u16::from(el.repetition == Repetition::Repeated);
        if el.num_children > 0 {
            if stack.len() >= MAX_SCHEMA_DEPTH {
                return Err(corrupt("schema nests too deeply"));
            }
            stack.push(Frame {
                children_left: el.num_children,
                anc: Ancestor {
                    name: el.name,
                    repetition: el.repetition,
                    logical: match el.logical {
                        Logical::List | Logical::Map => el.logical,
                        _ => Logical::None,
                    },
                    def_level,
                    rep_level,
                },
            })
            .map_err(|_| crate::oom_in("reading the parquet schema"))?;
        } else {
            let Some(phys) = el.phys else {
                return Err(corrupt("leaf schema element without a physical type"));
            };
            let ancestors: Vec<Ancestor> = stack.iter().map(|f| f.anc.clone()).collect(); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
            let mut path: Vec<String> = ancestors.iter().map(|a| a.name.clone()).collect(); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
            path.push(el.name); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
            let name = if path.len() == 1 { path[0].clone() } else { path.join(".") }; // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
            cols.push(ColumnSchema { // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                name,
                phys,
                max_def: def_level,
                logical: el.logical,
                max_rep: rep_level,
                repetition: el.repetition,
                path,
                ancestors,
            });
        }
        // Pop every group whose subtree is complete (a group with zero
        // children is malformed: the spec requires leaves to carry a type).
        while let Some(f) = stack.last() {
            if f.children_left == 0 {
                stack.with_vec(|v| v.pop());
            } else {
                break;
            }
        }
    }
    if consumed != n {
        return Err(corrupt("schema list longer than its child counts"));
    }
    if cols.is_empty() {
        return Err(corrupt("schema has no leaf columns"));
    }
    Ok(cols)
}

fn parse_schema_element(cur: &mut Cur<'_>) -> PgResult<SchemaElem> {
    let mut name: Option<String> = None;
    let mut phys_code: Option<i32> = None;
    let mut type_length: Option<i32> = None;
    let mut repetition: Option<i32> = None;
    let mut num_children: usize = 0;
    let mut converted: Option<i32> = None;
    let mut scale: Option<i32> = None;
    let mut precision: Option<i32> = None;
    let mut logical: Option<Logical> = None;

    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            SE_TYPE => phys_code = Some(cur.i32_value(t)?),
            SE_TYPE_LENGTH => type_length = Some(cur.i32_value(t)?),
            SE_REPETITION => repetition = Some(cur.i32_value(t)?),
            SE_NAME => {
                // a leaf's name moves into its column's path (FileMeta's); a group's lives in the walk's stack frame until
                // its subtree ends (the parse's scratch, gone before the sync) and reaches each leaf as its ancestors' copy
                name = Some(
                    String::from_utf8_lossy(cur.binary_value(t)?).into_owned(), // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                );
            }
            SE_NUM_CHILDREN => {
                let n = cur.i32_value(t)?;
                num_children =
                    usize::try_from(n).map_err(|_| corrupt("negative num_children"))?;
            }
            SE_CONVERTED => converted = Some(cur.i32_value(t)?),
            SE_SCALE => scale = Some(cur.i32_value(t)?),
            SE_PRECISION => precision = Some(cur.i32_value(t)?),
            SE_LOGICAL => {
                if t != T_STRUCT {
                    return Err(corrupt("logicalType is not a struct"));
                }
                logical = Some(parse_logical_type(cur)?);
            }
            _ => cur.skip(t, 0)?,
        }
    }

    let name = name.ok_or_else(|| corrupt("schema element without a name"))?;
    let phys = match phys_code {
        None => None,
        Some(0) => Some(Phys::Boolean),
        Some(1) => Some(Phys::Int32),
        Some(2) => Some(Phys::Int64),
        Some(3) => Some(Phys::Int96),
        Some(4) => Some(Phys::Float),
        Some(5) => Some(Phys::Double),
        Some(6) => Some(Phys::ByteArray),
        Some(7) => {
            let n = type_length.unwrap_or(-1);
            if n <= 0 {
                return Err(corrupt("FIXED_LEN_BYTE_ARRAY without a positive type_length"));
            }
            Some(Phys::Flba(n as u32))
        }
        Some(_) => return Err(corrupt("unrecognized physical type")),
    };
    // The root carries no repetition; every other element must (a missing
    // one on a child is read as REQUIRED, as parquet-mr does).
    let repetition = match repetition {
        None | Some(0) => Repetition::Required,
        Some(1) => Repetition::Optional,
        Some(2) => Repetition::Repeated,
        Some(_) => return Err(corrupt("invalid repetition type")),
    };
    // LogicalType wins; ConvertedType is the legacy fallback.
    let logical = match logical {
        Some(l) => l,
        None => converted_to_logical(converted, scale, precision),
    };
    Ok(SchemaElem { name, phys, repetition, num_children, logical })
}

fn converted_to_logical(
    converted: Option<i32>,
    scale: Option<i32>,
    precision: Option<i32>,
) -> Logical {
    match converted {
        None => Logical::None,
        Some(0) => Logical::String,                       // UTF8
        Some(1) => Logical::Map,                          // MAP
        Some(2) => Logical::None,                         // MAP_KEY_VALUE
        Some(3) => Logical::List,                         // LIST
        Some(4) => Logical::Enum,                         // ENUM
        Some(5) => Logical::Decimal {
            precision: precision.unwrap_or(0),
            scale: scale.unwrap_or(0),
        },
        Some(6) => Logical::Date,                         // DATE
        Some(7) => Logical::Time { unit: TimeUnit::Millis }, // TIME_MILLIS
        Some(8) => Logical::Time { unit: TimeUnit::Micros }, // TIME_MICROS
        // Legacy converted timestamps carry no UTC flag; instant semantics
        // (adjusted to UTC) is the convention every writer follows.
        Some(9) => Logical::Timestamp { unit: TimeUnit::Millis, utc: true },
        Some(10) => Logical::Timestamp { unit: TimeUnit::Micros, utc: true },
        Some(11) => Logical::Int { bits: 8, signed: false },  // UINT_8
        Some(12) => Logical::Int { bits: 16, signed: false }, // UINT_16
        Some(13) => Logical::Int { bits: 32, signed: false }, // UINT_32
        Some(14) => Logical::Int { bits: 64, signed: false }, // UINT_64
        Some(15) => Logical::Int { bits: 8, signed: true },   // INT_8
        Some(16) => Logical::Int { bits: 16, signed: true },  // INT_16
        Some(17) => Logical::Int { bits: 32, signed: true },  // INT_32
        Some(18) => Logical::Int { bits: 64, signed: true },  // INT_64
        Some(19) => Logical::Json,                            // JSON
        Some(20) => Logical::Bson,                            // BSON
        Some(21) => Logical::Interval,                        // INTERVAL
        Some(_) => Logical::Other,
    }
}

/// LogicalType union: one field set, keyed by field id.
fn parse_logical_type(cur: &mut Cur<'_>) -> PgResult<Logical> {
    let mut out = Logical::Other;
    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            1 => {
                cur.skip(t, 0)?;
                out = Logical::String;
            }
            2 => {
                cur.skip(t, 0)?;
                out = Logical::Map;
            }
            3 => {
                cur.skip(t, 0)?;
                out = Logical::List;
            }
            4 => {
                cur.skip(t, 0)?;
                out = Logical::Enum;
            }
            5 => {
                // DecimalType { 1: scale, 2: precision }
                let (mut scale, mut precision) = (0i32, 0i32);
                let mut lid = 0i16;
                while let Some((ft, fid)) = cur.field(&mut lid)? {
                    match fid {
                        1 => scale = cur.i32_value(ft)?,
                        2 => precision = cur.i32_value(ft)?,
                        _ => cur.skip(ft, 0)?,
                    }
                }
                out = Logical::Decimal { precision, scale };
            }
            6 => {
                cur.skip(t, 0)?;
                out = Logical::Date;
            }
            7 => {
                // isAdjustedToUTC is not surfaced for TIME: both forms bind
                // to the zoneless time type.
                let (_utc, unit) = parse_time_struct(cur)?;
                out = Logical::Time { unit };
            }
            8 => {
                let (utc, unit) = parse_time_struct(cur)?;
                out = Logical::Timestamp { unit, utc };
            }
            10 => {
                // IntType { 1: bitWidth (byte), 2: isSigned (bool) }
                let (mut bits, mut signed) = (0u8, false);
                let mut lid = 0i16;
                while let Some((ft, fid)) = cur.field(&mut lid)? {
                    match fid {
                        1 => {
                            if ft != T_BYTE {
                                return Err(corrupt("IntType bitWidth is not a byte"));
                            }
                            bits = cur.bytes(1)?[0];
                        }
                        2 => signed = cur.bool_value(ft)?,
                        _ => cur.skip(ft, 0)?,
                    }
                }
                out = Logical::Int { bits, signed };
            }
            12 => {
                cur.skip(t, 0)?;
                out = Logical::Json;
            }
            13 => {
                cur.skip(t, 0)?;
                out = Logical::Bson;
            }
            14 => {
                cur.skip(t, 0)?;
                out = Logical::Uuid;
            }
            15 => {
                cur.skip(t, 0)?;
                out = Logical::Float16;
            }
            _ => {
                cur.skip(t, 0)?;
                // UNKNOWN(11), VARIANT(16), GEOMETRY(17), GEOGRAPHY(18) and
                // anything newer fold to Other.
            }
        }
    }
    Ok(out)
}

/// TimeType/TimestampType { 1: isAdjustedToUTC, 2: unit (TimeUnit union) }.
fn parse_time_struct(cur: &mut Cur<'_>) -> PgResult<(bool, TimeUnit)> {
    let mut utc = false;
    let mut unit = TimeUnit::Millis;
    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            1 => utc = cur.bool_value(t)?,
            2 => {
                if t != T_STRUCT {
                    return Err(corrupt("TimeUnit is not a struct"));
                }
                let mut lid = 0i16;
                while let Some((ut, uid)) = cur.field(&mut lid)? {
                    match uid {
                        1 => unit = TimeUnit::Millis,
                        2 => unit = TimeUnit::Micros,
                        3 => unit = TimeUnit::Nanos,
                        _ => {}
                    }
                    cur.skip(ut, 0)?;
                }
            }
            _ => cur.skip(t, 0)?,
        }
    }
    Ok((utc, unit))
}

// RowGroup field ids.
const RG_COLUMNS: i16 = 1;
const RG_NUM_ROWS: i16 = 3;

fn parse_row_group(
    cur: &mut Cur<'_>,
    schema: &[ColumnSchema],
    ncols: usize,
    owner: Option<memledger::Owner>,
) -> PgResult<RowGroupMeta> {
    let mut num_rows: Option<i64> = None;
    let mut chunks: Option<Vec<ChunkMeta>> = None;

    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            RG_COLUMNS => {
                if t != T_LIST {
                    return Err(corrupt("row group columns is not a list"));
                }
                let (et, n) = cur.list_header()?;
                if et != T_STRUCT {
                    return Err(corrupt("column chunk is not a struct"));
                }
                if n != ncols {
                    return Err(corrupt("row group chunk count does not match schema"));
                }
                let mut v = Vec::new();
                v.try_reserve(n).map_err(|_| corrupt("chunk list too large"))?; // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                for i in 0..n {
                    v.push(parse_column_chunk(cur, schema, i, owner)?); // alloc: Exec charged FileMeta::held (heap_bytes, synced as the parse ends)
                }
                chunks = Some(v);
            }
            RG_NUM_ROWS => num_rows = Some(cur.i64_value(t)?),
            _ => cur.skip(t, 0)?,
        }
    }
    let (Some(chunks), Some(num_rows)) = (chunks, num_rows) else {
        return Err(corrupt("row group missing columns or num_rows"));
    };
    if num_rows < 0 {
        return Err(corrupt("negative row group num_rows"));
    }
    Ok(RowGroupMeta { num_rows, chunks })
}

// ColumnChunk / ColumnMetaData field ids.
const CC_FILE_PATH: i16 = 1;
const CC_META: i16 = 3;
const CC_OFFSET_INDEX_OFFSET: i16 = 4;
const CC_OFFSET_INDEX_LENGTH: i16 = 5;
const CC_COLUMN_INDEX_OFFSET: i16 = 6;
const CC_COLUMN_INDEX_LENGTH: i16 = 7;
const CC_CRYPTO: i16 = 8;
const CM_ENCODINGS: i16 = 2;
const CM_PATH: i16 = 3;
const CM_CODEC: i16 = 4;
const CM_NUM_VALUES: i16 = 5;
const CM_TOTAL_COMPRESSED: i16 = 7;
const CM_DATA_PAGE_OFFSET: i16 = 9;
const CM_DICT_PAGE_OFFSET: i16 = 11;
const CM_STATISTICS: i16 = 12;
const CM_BLOOM_OFFSET: i16 = 14;
const CM_BLOOM_LENGTH: i16 = 15;

fn span(offset: Option<i64>, length: Option<i32>) -> PgResult<Option<FileSpan>> {
    match (offset, length) {
        (Some(o), Some(l)) => {
            if o < 0 || l < 0 {
                return Err(corrupt("negative index or bloom filter location"));
            }
            Ok(Some(FileSpan { offset: o, length: i64::from(l) }))
        }
        _ => Ok(None),
    }
}

fn parse_column_chunk(
    cur: &mut Cur<'_>,
    schema: &[ColumnSchema],
    ordinal: usize,
    owner: Option<memledger::Owner>,
) -> PgResult<ChunkMeta> {
    let mut meta: Option<ChunkMeta> = None;
    let (mut oi_off, mut oi_len, mut ci_off, mut ci_len) = (None, None, None, None);
    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            CC_FILE_PATH => {
                let p = cur.binary_value(t)?;
                return Err(unsupported(format!( // alloc: Cold
                    "column chunk stored in external file \"{}\"",
                    String::from_utf8_lossy(p) // alloc: Cold
                )));
            }
            CC_META => {
                if t != T_STRUCT {
                    return Err(corrupt("column metadata is not a struct"));
                }
                meta = Some(parse_column_meta(cur, schema, ordinal, owner)?);
            }
            // Locator fields: pre-release writers used some of these ids
            // for other things; a mistyped one is skipped, never fatal.
            CC_OFFSET_INDEX_OFFSET if t == T_I64 => oi_off = Some(cur.i64_value(t)?),
            CC_OFFSET_INDEX_LENGTH if t == T_I32 => oi_len = Some(cur.i32_value(t)?),
            CC_COLUMN_INDEX_OFFSET if t == T_I64 => ci_off = Some(cur.i64_value(t)?),
            CC_COLUMN_INDEX_LENGTH if t == T_I32 => ci_len = Some(cur.i32_value(t)?),
            CC_CRYPTO => return Err(unsupported("encrypted columns".to_string())), // alloc: Cold
            _ => cur.skip(t, 0)?,
        }
    }
    let mut meta = meta.ok_or_else(|| corrupt("column chunk without metadata"))?;
    meta.offset_index = span(oi_off, oi_len)?;
    meta.column_index = span(ci_off, ci_len)?;
    Ok(meta)
}

fn parse_column_meta(
    cur: &mut Cur<'_>,
    schema: &[ColumnSchema],
    ordinal: usize,
    owner: Option<memledger::Owner>,
) -> PgResult<ChunkMeta> {
    let mut column: Option<usize> = None;
    let mut codec: Option<CodecId> = None;
    let mut num_values: Option<i64> = None;
    let mut total_compressed: Option<i64> = None;
    let mut data_page_offset: Option<i64> = None;
    let mut dict_page_offset: Option<i64> = None;
    let (mut bf_off, mut bf_len) = (None, None);

    let mut last_id = 0i16;
    while let Some((t, id)) = cur.field(&mut last_id)? {
        match id {
            CM_ENCODINGS => {
                // Page headers carry the authoritative per-page encoding
                // (dict->PLAIN fallback happens mid-chunk); skip the list.
                cur.skip(t, 0)?;
            }
            CM_PATH => {
                if t != T_LIST {
                    return Err(corrupt("path_in_schema is not a list"));
                }
                let (et, n) = cur.list_header()?;
                if et != T_BINARY {
                    return Err(corrupt("path_in_schema element is not a string"));
                }
                if n == 0 {
                    return Err(corrupt("empty path_in_schema"));
                }
                // Chunks appear in schema-leaf order in every known writer;
                // verify the full path, and fall back to a search if a
                // writer ever permutes them.
                let mut matches_ordinal = schema.get(ordinal).is_some_and(|c| c.path.len() == n);
                // memsig E-pub t5: on the load's owner while the match lives
                let mut parts: memledger::ChargedVec<&[u8]> = memledger::ChargedVec::new(owner, memledger::Kind::Region);
                parts.try_reserve(n).map_err(|_| crate::oom_in("reading column path"))?;
                for i in 0..n {
                    let part = cur.binary()?;
                    if matches_ordinal && schema[ordinal].path[i].as_bytes() != part {
                        matches_ordinal = false;
                    }
                    parts.push(part).map_err(|_| crate::oom_in("reading column path"))?;
                }
                column = if matches_ordinal {
                    Some(ordinal)
                } else {
                    schema.iter().position(|c| {
                        c.path.len() == n
                            && c.path.iter().zip(parts.iter()).all(|(a, b)| a.as_bytes() == *b)
                    })
                };
                if column.is_none() {
                    return Err(corrupt("column chunk path not found in schema"));
                }
            }
            CM_CODEC => codec = Some(CodecId::from_i32(cur.i32_value(t)?)?),
            CM_NUM_VALUES => num_values = Some(cur.i64_value(t)?),
            CM_TOTAL_COMPRESSED => total_compressed = Some(cur.i64_value(t)?),
            CM_DATA_PAGE_OFFSET => data_page_offset = Some(cur.i64_value(t)?),
            CM_DICT_PAGE_OFFSET => dict_page_offset = Some(cur.i64_value(t)?),
            CM_STATISTICS => cur.skip(t, 0)?, // never decoded
            CM_BLOOM_OFFSET if t == T_I64 => bf_off = Some(cur.i64_value(t)?),
            CM_BLOOM_LENGTH if t == T_I32 => bf_len = Some(cur.i32_value(t)?),
            _ => cur.skip(t, 0)?,
        }
    }

    let (Some(codec), Some(num_values), Some(total_compressed), Some(data_page_offset)) =
        (codec, num_values, total_compressed, data_page_offset)
    else {
        return Err(corrupt("column metadata missing required fields"));
    };
    if num_values < 0 || total_compressed < 0 || data_page_offset < 0 {
        return Err(corrupt("negative column metadata offsets or counts"));
    }
    Ok(ChunkMeta {
        column: column.unwrap_or(ordinal),
        codec,
        num_values,
        data_page_offset,
        dict_page_offset,
        total_compressed_size: total_compressed,
        offset_index: None,
        column_index: None,
        bloom_filter: span(bf_off, bf_len)?,
    })
}
