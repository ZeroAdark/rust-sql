//! Catalog attribute → `ColSchema` derivation (spec §2 storage classes;
//! §1.4 universal-type law: classes derive from typlen/typbyval/typalign,
//! never an OID table — the ONE place an OID appears is the byval HINT map
//! below, because signedness/floatness of a byval word is a semantic fact
//! the catalog properties cannot carry).
//!
//! ## The byval hint law (settled here)
//!
//! Datum extension for byval widths < 8 is per the `SIGNED` stream flag
//! (spec §6.7): a mis-hinted signed type would zero-extend negative values
//! into wrong datum words. So byval types are admitted by an EXPLICIT hint
//! table; a byval type with no entry is a typed CREATE-time refusal (the
//! old-AM type-gate precedent). By-reference types need no hint (their
//! images are byte-exact): fixed-length → `Fixed{len}`, typlen == −1 →
//! `VarlenaVerbatim`, typlen == −2 (cstring) refuses.
//!
//! ## The type-semantics map (spec §19.7; the metadata plane's half)
//!
//! [`TypeSemantics`] is the SECOND fact catalog properties cannot carry, and
//! it is settled here for the same reason as the byval hint — this is the
//! only place in the pgrcolumnar stack that sees `atttypid`. A storage
//! class fixes the BYTES; it does not fix the ORDER or the EQUALITY those
//! bytes stand for. `uuid` and `interval` are both `Fixed{16}` with
//! `typlen == 16`; `text`, `bytea`, `numeric` and `jsonb` are all
//! `VarlenaVerbatim`. Zone keys, sortedness and equality blooms are sound
//! only when the seal knows which, so the tag rides [`ColSchema`] out to the
//! writer, which reads it and never guesses.
//!
//! The map is DELIBERATELY conservative and closed by an `Opaque` default:
//! an unmapped or not-yet-audited type gets validity + counts (+ byte
//! lengths) and no key, which is exactly what every part carried before the
//! metadata plane was wired. Adding a row can only add pruning; it can never
//! change an answer, because a stat is advisory. Rows are admitted only when
//! byte order IS semantic order (for keys) and byte equality IS value
//! equality (for blooms) — the two properties the profile lattice demands:
//!
//! - `bpchar` is `Opaque`, not `TextCollated`: its space-padded equality is
//!   not byte equality.
//! - `numeric` is `NumericUnpacked` when UNCONSTRAINED; a declared
//!   NUMERIC(P,S) elects `PackedNumeric{S}` ([scale-alg]): post-typmod-cast
//!   every stored value's dscale IS S (a DDL fact), and any value that
//!   refuses the pack (legacy/corrupt image, mantissa over budget) poisons
//!   its grain — the plane abstains, never lies.
//! - `name` is `Opaque`: 64-byte NUL-padded images with strcmp semantics are
//!   not worth an unaudited memcmp claim.
//! - `interval` is `IntervalCmp` (NOT `MemcmpOrdered`) and `timetz` is
//!   `TimetzUtc` (NOT `MemcmpOrdered`) — both are `Fixed` images whose
//!   semantic order is a computed comparison key, not their bytes.

use pgrc_format::class::{ClassHint, ColSchema, StorageClass, TypeSemantics};
pub use pgrc_format::class::CollationClass;
use types_core::catalog::{
    BOOLOID, BYTEAOID, CHAROID, CIDOID, DATEOID, FLOAT4OID, FLOAT8OID, INT2OID, INT4OID, INT8OID,
    INTERVALOID, NUMERICOID, OIDOID, REGTYPEOID, TEXTOID, TIMEOID, TIMESTAMPOID, TIMESTAMPTZOID,
    TIMETZOID, VARCHAROID, XIDOID,
};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_rel::Relation;
use types_tuple::tupdesc::FormData_pg_attribute;

/// C collation oids (pg_collation.dat): "C" = 950, "POSIX" = 951.
const C_COLLATION_OID: Oid = 950;
const POSIX_COLLATION_OID: Oid = 951;
/// The database-default collation sentinel (pg_collation.dat "default").
const DEFAULT_COLLATION_OID: Oid = 100;

/// Type oids with no `types_core::catalog` constant yet (pg_type.dat).
const UUIDOID: Oid = 2950;
const MACADDROID: Oid = 829;
const MACADDR8OID: Oid = 774;
const MONEYOID: Oid = 790;
const PG_LSNOID: Oid = 3220;
const XID8OID: Oid = 5069;
const REGPROCOID: Oid = 24;
const REGPROCEDUREOID: Oid = 2202;
const REGOPEROID: Oid = 2203;
const REGOPERATOROID: Oid = 2204;
const REGCLASSOID: Oid = 2205;
const REGCONFIGOID: Oid = 3734;
const REGDICTIONARYOID: Oid = 3769;
const REGNAMESPACEOID: Oid = 4089;
const REGROLEOID: Oid = 4096;
const REGCOLLATIONOID: Oid = 4191;

/// The byval hint map. `None` = admitted, unsigned/word semantics. Money (790) is a
/// signed int8 word; the reg* alias types, pg_lsn and xid8 are unsigned words. A byval
/// oid past this map is a catalog type: an enum (an unsigned oid word) or a domain
/// (its base type's hint), through [`catalog_hint`]; anything else refuses.
fn byval_hint(atttypid: Oid) -> Option<ClassHint> {
    builtin_hint(atttypid).or_else(|| catalog_hint(atttypid))
}

fn builtin_hint(atttypid: Oid) -> Option<ClassHint> {
    match atttypid {
        BOOLOID => Some(ClassHint::Bool),
        INT2OID | INT4OID | INT8OID | MONEYOID => Some(ClassHint::Signed),
        DATEOID | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID => Some(ClassHint::Signed),
        FLOAT4OID | FLOAT8OID => Some(ClassHint::Float),
        // Unsigned word classes.
        OIDOID | XIDOID | CIDOID | CHAROID | REGTYPEOID | PG_LSNOID | XID8OID => Some(ClassHint::None),
        REGPROCOID | REGPROCEDUREOID | REGOPEROID | REGOPERATOROID | REGCLASSOID | REGCONFIGOID
        | REGDICTIONARYOID | REGNAMESPACEOID | REGROLEOID | REGCOLLATIONOID => Some(ClassHint::None),
        _ => None,
    }
}

/// The `pg_type` row of a non-built-in oid: `(typtype, typbasetype, typtypmod)`, `None` when the
/// catalog probe is not installed (a library world), the row is missing, or the probe errs — a
/// refusal, never a guess.
fn catalog_type(typid: Oid) -> Option<(u8, Oid, i32)> {
    // built-in oids are never enums or domains: no probe below the unpinned range
    if typid < FIRST_UNPINNED_OID || !syscache_seams::pg_type_base_shape::is_installed() {
        return None;
    }
    let shape = syscache_seams::pg_type_base_shape::call(typid).ok().flatten()?;
    Some((shape.typtype as u8, shape.typbasetype, shape.typtypmod))
}

/// The base type under a domain chain (PG's `getBaseType`, at most `DOMAIN_DEPTH` links) and
/// the innermost domain's typmod: `(base_oid, typmod)`; an enum is its own base.
const DOMAIN_DEPTH: usize = 16;
const FIRST_UNPINNED_OID: Oid = 12000;
fn domain_base(typid: Oid, typmod: i32) -> Option<(Oid, i32)> {
    let (mut oid, mut tm) = (typid, typmod);
    for _ in 0..DOMAIN_DEPTH {
        match catalog_type(oid)? {
            (b'd', base, btm) => {
                tm = if tm < 0 { btm } else { tm };
                oid = base;
                if oid < FIRST_UNPINNED_OID {
                    return Some((oid, tm));
                }
            }
            (b'e', _, _) => return Some((oid, tm)),
            _ => return None,
        }
    }
    None
}

fn catalog_hint(typid: Oid) -> Option<ClassHint> {
    let (base, _) = domain_base(typid, -1)?;
    if base == typid || matches!(catalog_type(base), Some((b'e', _, _))) {
        // an enum, or a domain over one: an unsigned oid word
        return Some(ClassHint::None);
    }
    builtin_hint(base)
}

/// [scale-alg] NUMERIC(P,S) typmod → the declared scale S (PG's
/// `numeric_typmod_scale`: low 11 bits of typmod−VARHDRSZ, sign-extended
/// from bit 10). `None` for the unconstrained typmod (−1) and for
/// negative declared scales — no uniform-dscale stats lane exists there
/// (stored dscale >= 0). Mirrors `sqe::typmeta::numeric_typmod_scale`
/// (this crate sits below the executor — the 5-line law is restated,
/// not imported).
fn numeric_typmod_scale(typmod: i32) -> Option<i32> {
    if typmod < 4 {
        return None;
    }
    let s = (((typmod - 4) & 0x7ff) ^ 1024) - 1024;
    (s >= 0).then_some(s)
}

/// The type-semantics map (see the module doc). Every row is consistent with
/// the storage class the same oid derives through [`byval_hint`] /
/// [`StorageClass::derive`] — `semantics_match_classes` pins that, and the
/// writer degrades to `Opaque` rather than trusting an inconsistent pair.
///
/// [scale-alg] NUMERIC takes the typmod too: a DECLARED nonnegative
/// scale elects the `PackedNumeric{scale}` ADVISORY stats lane (exact
/// mantissa zone keys + i128 mantissa sums + zero counts — the batch
/// builder's incumbent packed arm; values that refuse the pack poison
/// their grain, so the plane is present-and-exact or honestly absent).
/// Semantics are NOT in the schema fingerprint (class.rs — advisory
/// stats policy, not storage shape): parts sealed before this election
/// simply lack the plane and every witness consumer refuses typed.
fn type_semantics(atttypid: Oid, atttypmod: i32) -> TypeSemantics {
    match atttypid {
        BOOLOID => TypeSemantics::Bool,
        // Signed byval words. date/time/timestamp[tz] are int-shaped and
        // their integer order IS their temporal order.
        INT2OID | INT4OID | INT8OID | MONEYOID => TypeSemantics::SignedInt,
        DATEOID | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID => TypeSemantics::SignedInt,
        FLOAT4OID | FLOAT8OID => TypeSemantics::Float,
        OIDOID | XIDOID | CIDOID | CHAROID | REGTYPEOID | PG_LSNOID | XID8OID => TypeSemantics::UnsignedInt,
        REGPROCOID | REGPROCEDUREOID | REGOPEROID | REGOPERATOROID | REGCLASSOID | REGCONFIGOID
        | REGDICTIONARYOID | REGNAMESPACEOID | REGROLEOID | REGCOLLATIONOID => TypeSemantics::UnsignedInt,
        // By-reference: memcmp IS the semantic order.
        UUIDOID | MACADDROID | MACADDR8OID => TypeSemantics::MemcmpOrdered,
        BYTEAOID => TypeSemantics::MemcmpOrdered,
        // Text family: the profile's collation gate decides keys vs blooms.
        TEXTOID | VARCHAROID => TypeSemantics::TextCollated,
        // Fixed images with a COMPUTED comparison key.
        INTERVALOID => TypeSemantics::IntervalCmp,
        TIMETZOID => TypeSemantics::TimetzUtc,
        // [scale-alg] Typmod-scaled numeric: the packed advisory lane
        // (every value's dscale == S post-typmod-cast, so the pack is
        // total on healthy data; refusals poison per grain). The
        // UNCONSTRAINED column keeps counts + byte lengths + NDV — no
        // keys (1.0 vs 1.00 collapse display scales), no blooms.
        NUMERICOID => match numeric_typmod_scale(atttypmod) {
            Some(scale) => TypeSemantics::PackedNumeric { scale },
            None => TypeSemantics::NumericUnpacked,
        },
        // Closed by the sound default: bpchar, name, json/jsonb/xml,
        // bit/varbit, composites, ranges, arrays, extension types. An enum is
        // eq-only; a domain is its base type at the domain's typmod.
        _ => match domain_base(atttypid, atttypmod) {
            Some((base, _)) if base == atttypid => TypeSemantics::EnumEqOnly,
            Some((base, tm)) => type_semantics(base, tm),
            None => TypeSemantics::Opaque,
        },
    }
}

/// FAIL CLOSED (#598): a non-C attcollation is deterministic only when
/// `pg_collation.collisdeterministic` SAYS so. Every UNKNOWN — the probe
/// seam not installed (no catalog reachable), a dangling attcollation, a
/// probe error — classifies `Nondeterministic`: no zone keys, no eq blooms,
/// counts + length stats only. Never guess deterministic: the previous arm
/// mapped every non-C collation to `OtherDeterministic`, and
/// `profile.rs` arms byte-hash eq blooms for anything deterministic — so a
/// case-insensitive ICU collation sealed blooms where equal-under-collation,
/// byte-different probes read definite-absent, silently missing rows the
/// day predicate pushdown wires. The conservative arm only loses advisory
/// pruning (charter §1: a verdict can lose speed, never rows).
///
/// Parts already sealed under the old fail-open classification: both the
/// seal and probe sides classify through THIS function (the `ColSchema`
/// they share, spec §5.5), and `collation_class` is folded into
/// `schema_fingerprint`. A genuinely nondeterministic collation therefore
/// reclassifies here, its table's fingerprint changes, and its pre-fix
/// manifests refuse TYPED at open (`ReadError::OpenMismatch
/// {schema_fingerprint}`) — the reader-side guard: parts carrying unsound
/// blooms are never silently trusted; they error until re-sealed. Every
/// honestly deterministic collation (default, C, POSIX, deterministic ICU)
/// probes to the same class as before, so blessed bank fingerprints are
/// unchanged.
///
/// The probe is deterministic across seal and probe time
/// (`collisdeterministic` is immutable for a live collation), and the one
/// reachable skew — seam installed on one side, not the other — degrades to
/// "blooms not consulted" (bloom evidence is optional at every probe site),
/// never to a wrong verdict.
pub fn collation_class(attcollation: Oid) -> CollationClass {
    if attcollation == 0
        || attcollation == C_COLLATION_OID
        || attcollation == POSIX_COLLATION_OID
    {
        return CollationClass::C;
    }
    // M4-S8 (the S5 engine-grain substrate finding, coordinator-listed
    // for S8): the stock CB DDL carries DEFAULT-collated text, and on a
    // C-locale database (initdb --no-locale — the v4ctas vehicle and every
    // bank rig) DEFAULT *is* memcmp order. Classifying it
    // OtherDeterministic made dict election structurally unreachable
    // on every engine cut (elect.rs offers no dict policy off a non-C
    // class). Resolve DEFAULT through the engine's own locale answer — the
    // plancat.rs pgrc order predicate's exact currency (collate_is_c:
    // memcmp order IS the collation order, so zone keys, order
    // certificates, and char-length facts are all sound). Fail-closed
    // stands: seam uninstalled (library worlds), probe error, or a non-C
    // default all fall through to the conservative arms below.
    // Fingerprint note: collation_class folds into schema_fingerprint, so
    // an engine table sealed pre-fix refuses TYPED at open (the #598
    // reader-side guard) — the v4ctas vehicle cuts fresh per job and no
    // production pgrc data predates the cutover, so the reclassification
    // has no silent-trust surface.
    if attcollation == DEFAULT_COLLATION_OID
        && pg_locale_seams::collation_collate_is_c::is_installed()
        && pg_locale_seams::collation_collate_is_c::call(attcollation).unwrap_or(false)
    {
        return CollationClass::C;
    }
    if !syscache_seams::lookup_pg_collation_shape::is_installed() {
        return CollationClass::Nondeterministic;
    }
    match syscache_seams::lookup_pg_collation_shape::call(attcollation) {
        Ok(Some(shape)) if shape.collisdeterministic => CollationClass::OtherDeterministic,
        // Missing row or probe error: UNKNOWN is Nondeterministic. The
        // error is deliberately absorbed — collation class selects
        // advisory-metadata policy, and the conservative class is always
        // sound; a real catalog failure will surface on the next
        // non-advisory catalog access.
        _ => CollationClass::Nondeterministic,
    }
}

/// Why a column is refused (CREATE-time gate message currency).
pub struct UnsupportedColumn {
    pub attname_hint: String,
    pub atttypid: Oid,
}

pub(crate) fn derive_one(att: &FormData_pg_attribute) -> Result<ColSchema, UnsupportedColumn> {
    let refuse = || UnsupportedColumn {
        attname_hint: String::from_utf8_lossy(att.attname.name_str()).into_owned(), // alloc: Cold
        atttypid: att.atttypid,
    };
    if att.attisdropped {
        // Callers skip dropped attributes (`col_schemas_of`).
        return Err(refuse());
    }
    let class = if att.attbyval {
        let hint = byval_hint(att.atttypid).ok_or_else(refuse)?;
        StorageClass::derive(att.attlen, true, att.attalign as u8, hint).map_err(|_| refuse())?
    } else {
        match att.attlen {
            -1 => StorageClass::VarlenaVerbatim,
            len if len > 0 => StorageClass::Fixed { len: len as u32 },
            _ => return Err(refuse()),
        }
    };
    Ok(ColSchema {
        attno: att.attnum as u32,
        class,
        typlen: att.attlen,
        typbyval: att.attbyval,
        typalign: att.attalign as u8,
        collation_class: collation_class(att.attcollation),
        semantics: type_semantics(att.atttypid, att.atttypmod),
    })
}

/// Derive the full column schema for a pgrcolumnar relation. Attnos are
/// the 1-based catalog attnums (strictly increasing — the writer contract).
pub fn col_schemas(rel: &Relation<'_>) -> PgResult<Vec<ColSchema>> {
    col_schemas_of(rel.name(), &rel.rd_att.attrs)
}

/// FASTADD (pgrc-plans/FASTADD.done): what an ALTER TABLE does to a pgrcolumnar table's stored columns, from `old`
/// (the descriptor before it) to the relation's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnsVerdict {
    /// No stored column changed.
    Same,
    /// Columns were only added and/or dropped: metadata-only (the parts keep their columns, readers fill the added
    /// ones from `attmissingval`). Carries the old schema, which the ALTER's publish describes.
    Fast(Vec<ColSchema>),
    /// A column present before and after changed (ALTER TYPE): the rewrite.
    Rewrite,
}

/// [`ColumnsVerdict`] of `rel` against `old`. A column both have must be unchanged (its storage and its semantics,
/// today's rule) for `Fast`; and the generation the ALTER's publish chains on must be of the old schema (a belt:
/// anything else takes the rewrite, which needs no description of the parts). That generation is the newest one whose
/// publisher committed or is this transaction, as the writer's effective walk finds it: an aborted ADD COLUMN's
/// generation at the head (its schema is the aborted one's) is walked past, as the publish walks past it.
pub fn columns_verdict(rel: &Relation<'_>, old: &[FormData_pg_attribute]) -> PgResult<ColumnsVerdict> {
    let now = col_schemas(rel)?;
    let was = col_schemas_of(rel.name(), old)?;
    if was == now {
        return Ok(ColumnsVerdict::Same);
    }
    if now.iter().any(|c| was.iter().any(|o| o.attno == c.attno && o != c)) {
        return Ok(ColumnsVerdict::Rewrite);
    }
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let fp = pgrc_format::ident::schema_fingerprint(&was);
    let check = crate::probe::SnapshotCommitCheck::new(None);
    let base = crate::catalog::visible(slot, &check);
    check.take_error()?;
    if let crate::catalog::Lookup::Gen(g) = base? {
        if g.schema_fp != fp {
            return Ok(ColumnsVerdict::Rewrite);
        }
    }
    Ok(ColumnsVerdict::Fast(was))
}

/// [`col_schemas`] of a descriptor (ALTER's rewrite reads the old one). A dropped attribute has no stream, so attnos
/// keep their gaps.
pub fn col_schemas_of(relname: &str, atts: &[FormData_pg_attribute]) -> PgResult<Vec<ColSchema>> {
    let mut out = crate::alloc::try_vec(atts.len(), "schema")?;
    for att in atts.iter().filter(|a| !a.attisdropped) {
        match derive_one(att) {
            Ok(s) => out.push(s),
            Err(u) => return Err(unsupported_column_error(relname, &u)),
        }
    }
    if out.is_empty() {
        return Err(crate::unsupported("tables with zero columns"));
    }
    Ok(out)
}

/// D2189 (trunk 45e21f82010, ported onto FASTADD's schema book): a column dropped with no ALTER TABLE
/// publish leaves the parts under a schema (`stored_fp`) no generation describes, so every reader's presence lookup
/// ([`crate::fastadd::presence`]) refused (XX001). FASTADD publishes a drop only in ALTER TABLE's phase 3 on the table
/// itself; PostgreSQL also drops columns outside it: a dependency's cascade (DROP DOMAIN / TYPE / COLLATION ...
/// CASCADE, ALTER TYPE ... DROP ATTRIBUTE ... CASCADE, a temp type's cleanup at DISCARD TEMP or a session's end) and the
/// inheritance and partition children ALTER TABLE ... DROP COLUMN recurses into. PostgreSQL keeps the dropped
/// attribute's shape (attlen / attbyval / attalign) and attcollation, and a shape admits few storage classes, so the
/// stamp is rebuilt here ([`stamped_of`]): the live columns as derived, and each dropped one either absent (dropped
/// before the stamp's parts were written) or a candidate class of its kept shape under each collation class it can have
/// been written with: C for attcollation 0, all three otherwise (a cascaded DROP COLLATION leaves no pg_collation row
/// to classify, and [`collation_class`] then answers Nondeterministic, fail-closed, whatever the column was written
/// with). The candidate whose fingerprint is `stored_fp` is booked ([`crate::fastadd::note`]), and the readers fill
/// nothing (every live column is in it, at the same storage).
///
/// The schema fingerprint ([`pgrc_format::ident::schema_fingerprint`]) is 64 bits: a fold64 chain (the splitmix64
/// finalizer over `acc ^ word * 0x9E37_79B9_7F4A_7C15`, seeded from the format's uuid seeds) over the column count,
/// then per column its attno, its class word (class id, width, signedness, fixed length) and its property word (typlen,
/// typbyval, typalign, collation class); type semantics are not in it. The match is the proof: every candidate holds
/// the live columns at the storage the reader decodes them with, so a candidate whose fingerprint is the stored one
/// says the parts hold the live columns so. A wrong pick needs a candidate column list other than the written one with
/// the same 64-bit fingerprint: with at most [`STAMP_CANDIDATES_MAX`] = 4096 candidates that is at most 4096 / 2^64,
/// about 2.2e-16 per rebuild for a well-mixed 64-bit hash (the odds a part's own fingerprint check at open already
/// accepts are 2^-64 per open).
///
/// A stamp the book already holds, or a relation with no dropped attribute, books nothing and passes (the caller's
/// presence check then answers as before). Past the cap, or with no candidate matching, it FAILS CLOSED with the named
/// refusal (XX001, "dropped-column stamp ... not rebuilt"): no schema is guessed and no row is read.
pub fn book_dropped_stamp(rel: &Relation<'_>, stored_fp: u64) -> PgResult<()> {
    book_dropped_stamp_of(Some(rel.name()), &rel.rd_att.attrs, stored_fp)
}

/// [`book_dropped_stamp`] over a descriptor's attributes (the row cache's fetch has the descriptor, not the relation;
/// `relname` names the relation in the refusal when the caller has it).
pub fn book_dropped_stamp_of(relname: Option<&str>, atts: &[FormData_pg_attribute], stored_fp: u64) -> PgResult<()> {
    if !atts.iter().any(|a| a.attisdropped) || crate::fastadd::schema_of(stored_fp).is_some() {
        return Ok(());
    }
    match stamped_of(atts, stored_fp) {
        Ok(cols) => {
            crate::fastadd::note(&cols);
            Ok(())
        }
        Err(StampMiss::Alloc(e)) => Err(crate::format_error(e)),
        Err(miss) => Err(stamp_refusal(relname, stored_fp, miss)),
    }
}

/// The candidate product [`stamped_of`] searches, at most. A dropped column has at most 4 candidates (absent, or one of
/// at most three by-value classes; absent, or a varlena under one of the three collation classes when collatable), so
/// six dropped columns of the widest kind always fit and a seventh of that kind is past it.
const STAMP_CANDIDATES_MAX: usize = 4096;

/// Why [`stamped_of`] rebuilt no stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StampMiss {
    /// The candidate product (saturating) is past [`STAMP_CANDIDATES_MAX`]: none was tried.
    OverCap { candidates: usize },
    /// None of the `candidates` has the stored fingerprint (0: a live column has no storage class).
    NoMatch { candidates: usize },
    Alloc(pgrc_format::FormatError),
}

/// The fail-closed refusal of a stamp [`stamped_of`] did not rebuild (XX001, as the presence refusal it names).
fn stamp_refusal(relname: Option<&str>, stored_fp: u64, miss: StampMiss) -> Box<PgError> {
    let rel = relname.map_or_else(|| "a pgrcolumnar relation".to_owned(), |n| format!("relation \"{n}\""));
    let why = match miss {
        StampMiss::OverCap { candidates } => format!("{candidates} candidate schemas, past the cap of {STAMP_CANDIDATES_MAX}"),
        StampMiss::NoMatch { candidates } => format!("none of {candidates} candidate schemas has its fingerprint"),
        StampMiss::Alloc(e) => format!("{e}"),
    };
    Box::new(
        PgError::error(format!("pgrc read: dropped-column stamp {stored_fp:#018x} of {rel} not rebuilt: {why}"))
            .with_detail("Its parts were written under a schema no generation describes (a column dropped without an ALTER TABLE publish); no schema is guessed.")
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
    )
}

/// The column list among `atts`' stamp candidates whose fingerprint is `stored_fp` ([`book_dropped_stamp`]).
fn stamped_of(atts: &[FormData_pg_attribute], stored_fp: u64) -> Result<Vec<ColSchema>, StampMiss> {
    use pgrc_format::ident::schema_fingerprint;
    const COLLATIONS: [CollationClass; 3] = [CollationClass::C, CollationClass::OtherDeterministic, CollationClass::Nondeterministic];
    let mut slots: Vec<Vec<Option<ColSchema>>> = pgrc_format::try_vec(atts.len(), "schema").map_err(StampMiss::Alloc)?;
    for att in atts {
        if !att.attisdropped {
            let Ok(s) = derive_one(att) else { return Err(StampMiss::NoMatch { candidates: 0 }) };
            slots.push(vec![Some(s)]);
            continue;
        }
        let classes: Vec<StorageClass> = if att.attbyval {
            [ClassHint::None, ClassHint::Signed, ClassHint::Float, ClassHint::Bool]
                .into_iter()
                .filter_map(|h| StorageClass::derive(att.attlen, true, att.attalign as u8, h).ok())
                .collect()
        } else {
            match att.attlen {
                -1 => vec![StorageClass::VarlenaVerbatim],
                len if len > 0 => vec![StorageClass::Fixed { len: len as u32 }],
                _ => Vec::new(),
            }
        };
        // D2189: attcollation 0 is written C ([`collation_class`]); a collatable column's class came from
        // its collation, which the cascade may have dropped, so each of the three
        let collations = if att.attcollation == 0 { &COLLATIONS[..1] } else { &COLLATIONS[..] };
        let mut c: Vec<Option<ColSchema>> = vec![None];
        for class in classes {
            for &collation_class in collations {
                let s = Some(ColSchema {
                    attno: att.attnum as u32,
                    class,
                    typlen: att.attlen,
                    typbyval: att.attbyval,
                    typalign: att.attalign as u8,
                    collation_class,
                    semantics: TypeSemantics::Opaque,
                });
                if !c.contains(&s) {
                    c.push(s);
                }
            }
        }
        slots.push(c);
    }
    let total = slots.iter().fold(1usize, |n, c| n.saturating_mul(c.len()));
    if total > STAMP_CANDIDATES_MAX {
        return Err(StampMiss::OverCap { candidates: total });
    }
    let mut cur: Vec<ColSchema> = pgrc_format::try_vec(slots.len(), "schema").map_err(StampMiss::Alloc)?;
    for n in 0..total {
        let mut k = n;
        cur.clear();
        for c in &slots {
            if let Some(s) = c[k % c.len()] {
                cur.push(s);
            }
            k /= c.len();
        }
        if schema_fingerprint(&cur) == stored_fp {
            return Ok(cur);
        }
    }
    Err(StampMiss::NoMatch { candidates: total })
}

/// The 1-based attnos of the relation's jsonb columns — the TY-3 shred
/// arm's registration set (JSON-routing landing). Catalog knowledge: this
/// module is the only place in the pgrcolumnar stack that sees
/// `atttypid` (the module-doc law), so the jsonb fact is minted here and
/// handed to the ingest seam as plain attnos.
pub fn jsonb_attnos(rel: &Relation<'_>) -> Vec<u32> {
    use types_core::catalog::JSONBOID;
    rel.rd_att
        .attrs
        .iter()
        .filter(|a| !a.attisdropped && a.atttypid == JSONBOID)
        .map(|a| a.attnum as u32)
        .collect()
}

/// The CREATE-time gate: probe every attribute, so `CREATE TABLE ... USING
/// pgrcolumnar` refuses typed at DDL time instead of at first COPY.
pub fn check_create_supported(relname: &str, atts: &[FormData_pg_attribute]) -> PgResult<()> {
    for att in atts {
        if att.attisdropped {
            continue;
        }
        if let Err(u) = derive_one(att) {
            return Err(unsupported_column_error(relname, &u));
        }
    }
    Ok(())
}

fn unsupported_column_error(relname: &str, u: &UnsupportedColumn) -> Box<PgError> {
    Box::new(PgError::error(format!( // alloc: Cold
            "pgrcolumnar does not support the type of column \"{}\" of relation \"{}\" \
             (type oid {})",
            u.attname_hint, relname, u.atttypid
        ))
        .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))
}

#[cfg(test)]
mod semantics_tests {
    use super::*;
    use pgrc_meta::profile::MetaProfile;

    /// Storage class for an oid exactly as `derive_one` computes it, from
    /// the catalog properties the type actually has.
    fn class_of(atttypid: Oid, attlen: i16, attbyval: bool, attalign: u8) -> StorageClass {
        if attbyval {
            let hint = byval_hint(atttypid).expect("byval type admitted");
            StorageClass::derive(attlen, true, attalign, hint).expect("class derives")
        } else {
            match attlen {
                -1 => StorageClass::VarlenaVerbatim,
                n if n > 0 => StorageClass::Fixed { len: n as u32 },
                _ => panic!("not a table-column typlen"),
            }
        }
    }

    /// THE CONSISTENCY PIN: every oid this file maps must produce a
    /// (class, semantics) pair `MetaProfile::derive` ACCEPTS. An
    /// inconsistent row would not fail an ingest — the writer degrades to
    /// `Opaque` — it would silently cost the column its pruning forever, so
    /// the pair is pinned here rather than discovered in a benchmark.
    #[test]
    fn semantics_match_classes() {
        // (oid, typlen, typbyval, typalign)
        let rows: &[(Oid, i16, bool, u8)] = &[
            (BOOLOID, 1, true, b'c'),
            (INT2OID, 2, true, b's'),
            (INT4OID, 4, true, b'i'),
            (INT8OID, 8, true, b'd'),
            (DATEOID, 4, true, b'i'),
            (TIMEOID, 8, true, b'd'),
            (TIMESTAMPOID, 8, true, b'd'),
            (TIMESTAMPTZOID, 8, true, b'd'),
            (FLOAT4OID, 4, true, b'i'),
            (FLOAT8OID, 8, true, b'd'),
            (OIDOID, 4, true, b'i'),
            (XIDOID, 4, true, b'i'),
            (CIDOID, 4, true, b'i'),
            (CHAROID, 1, true, b'c'),
            (REGTYPEOID, 4, true, b'i'),
            (UUIDOID, 16, false, b'c'),
            (MACADDROID, 6, false, b'i'),
            (MACADDR8OID, 8, false, b'i'),
            (BYTEAOID, -1, false, b'i'),
            (TEXTOID, -1, false, b'i'),
            (VARCHAROID, -1, false, b'i'),
            (INTERVALOID, 16, false, b'd'),
            (TIMETZOID, 12, false, b'd'),
            (NUMERICOID, -1, false, b'i'),
        ];
        for &(oid, len, byval, align) in rows {
            let class = class_of(oid, len, byval, align);
            let sem = type_semantics(oid, -1);
            assert_ne!(
                sem,
                TypeSemantics::Opaque,
                "oid {oid} is in the pin table but maps to Opaque"
            );
            for coll in [
                CollationClass::C,
                CollationClass::OtherDeterministic,
                CollationClass::Nondeterministic,
            ] {
                MetaProfile::derive(class, coll, sem).unwrap_or_else(|e| {
                    panic!("oid {oid}: ({class:?}, {coll:?}, {sem:?}) rejected: {e:?}")
                });
            }
        }
    }

    /// The tooth: the pin above provably discriminates — a deliberately
    /// wrong pair IS rejected, so a green `semantics_match_classes` is
    /// evidence and not a tautology.
    #[test]
    fn inconsistent_pair_is_rejected() {
        assert!(MetaProfile::derive(
            StorageClass::VarlenaVerbatim,
            CollationClass::C,
            TypeSemantics::SignedInt,
        )
        .is_err());
        // The two Fixed{16} types are exactly the confusion the tag exists
        // to prevent; both are accepted, and they are NOT the same profile.
        let uuid = MetaProfile::derive(
            StorageClass::Fixed { len: 16 },
            CollationClass::C,
            TypeSemantics::MemcmpOrdered,
        )
        .expect("uuid");
        let interval = MetaProfile::derive(
            StorageClass::Fixed { len: 16 },
            CollationClass::C,
            TypeSemantics::IntervalCmp,
        )
        .expect("interval");
        assert_ne!(uuid.key, interval.key);
        assert!(uuid.eq_bloomable && !interval.eq_bloomable);
    }

    /// The catalog types (ONE test: the seam is set-once per process; the uninstalled arm
    /// first): without the probe an enum / domain byval column refuses; with it an enum is an
    /// unsigned oid word (`EnumEqOnly`), a domain its base type's class and semantics at the
    /// domain's typmod, a domain chain follows to the base, a domain over an enum is the enum.
    #[test]
    fn catalog_types_derive_through_the_probe() {
        use syscache_seams::{pg_type_base_shape, PgTypeBaseShape};
        const ENUM: Oid = 16400;
        const DOM_INT: Oid = 16401;
        const DOM_NUM: Oid = 16402;
        const DOM_DOM: Oid = 16403;
        const DOM_ENUM: Oid = 16404;
        const DOM_TEXT: Oid = 16405;
        const COMPOSITE: Oid = 16406;
        assert!(!pg_type_base_shape::is_installed());
        assert_eq!(byval_hint(ENUM), None, "no probe: an enum byval column refuses");
        assert_eq!(type_semantics(DOM_TEXT, -1), TypeSemantics::Opaque);
        fn row(typtype: u8, typbasetype: Oid, typtypmod: i32) -> PgResult<Option<PgTypeBaseShape>> {
            Ok(Some(PgTypeBaseShape { typtype: typtype as i8, typbasetype, typtypmod, typelem: 0, typsubscript: 0 }))
        }
        pg_type_base_shape::set(|oid| match oid {
            ENUM => row(b'e', 0, -1),
            DOM_INT => row(b'd', INT4OID, -1),
            DOM_NUM => row(b'd', NUMERICOID, ((15 << 16) | 2) + 4),
            DOM_DOM => row(b'd', DOM_INT, -1),
            DOM_ENUM => row(b'd', ENUM, -1),
            DOM_TEXT => row(b'd', TEXTOID, -1),
            COMPOSITE => row(b'c', 0, -1),
            _ => Ok(None),
        });
        assert_eq!(byval_hint(ENUM), Some(ClassHint::None));
        assert_eq!(type_semantics(ENUM, -1), TypeSemantics::EnumEqOnly);
        assert_eq!(byval_hint(DOM_INT), Some(ClassHint::Signed));
        assert_eq!(type_semantics(DOM_INT, -1), TypeSemantics::SignedInt);
        assert_eq!(type_semantics(DOM_NUM, -1), TypeSemantics::PackedNumeric { scale: 2 });
        assert_eq!(byval_hint(DOM_DOM), Some(ClassHint::Signed));
        assert_eq!(byval_hint(DOM_ENUM), Some(ClassHint::None));
        assert_eq!(type_semantics(DOM_ENUM, -1), TypeSemantics::EnumEqOnly);
        assert_eq!(type_semantics(DOM_TEXT, -1), TypeSemantics::TextCollated);
        assert_eq!(byval_hint(COMPOSITE), None, "a composite is never byval");
        assert_eq!(byval_hint(16499), None, "a missing row refuses");
        for (oid, sem) in [(MONEYOID, TypeSemantics::SignedInt), (PG_LSNOID, TypeSemantics::UnsignedInt), (XID8OID, TypeSemantics::UnsignedInt), (REGCLASSOID, TypeSemantics::UnsignedInt)] {
            assert_eq!(type_semantics(oid, -1), sem);
            MetaProfile::derive(class_of(oid, if oid == REGCLASSOID { 4 } else { 8 }, true, b'd'), CollationClass::C, sem).expect("consistent");
        }
        MetaProfile::derive(class_of(ENUM, 4, true, b'i'), CollationClass::C, type_semantics(ENUM, -1)).expect("enum profile");
    }

    /// The closed default: an unmapped type is `Opaque`, never a guess.
    #[test]
    fn unmapped_types_are_opaque() {
        use types_core::catalog::{BPCHAROID, JSONBOID, JSONOID, NAMEOID, XMLOID};
        for oid in [BPCHAROID, JSONOID, JSONBOID, XMLOID, NAMEOID, 1560 /* bit */] {
            assert_eq!(type_semantics(oid, -1), TypeSemantics::Opaque, "oid {oid}");
        }
    }
}

#[cfg(test)]
mod dropped_tests {
    use super::*;
    use pgrc_format::ident::schema_fingerprint;

    fn att(attnum: i16, typid: Oid, attlen: i16, byval: bool, align: u8, dropped: bool) -> FormData_pg_attribute {
        let mut a = FormData_pg_attribute::default();
        a.attnum = attnum;
        a.atttypid = typid;
        a.attlen = attlen;
        a.attbyval = byval;
        a.attalign = align as i8;
        a.atttypmod = -1;
        a.attisdropped = dropped;
        a
    }

    fn derive_all(atts: &[FormData_pg_attribute]) -> Vec<ColSchema> {
        atts.iter().map(|a| derive_one(a).ok().expect("derives")).collect()
    }

    /// RemoveAttributeById's change: the type goes, the shape and attcollation stay.
    fn drop_att(a: &mut FormData_pg_attribute) {
        (a.atttypid, a.attisdropped) = (0, true);
    }

    /// D2098 on our line: a dropped attribute has no stream (FASTADD: attnos keep their gaps), so a table that carries
    /// one (SET ACCESS METHOD over a heap table with a dropped column) is storable: the CREATE gate and the schema skip
    /// it, and only a live oid-0 attribute is refused.
    #[test]
    fn a_dropped_attribute_is_skipped_not_stored() {
        let atts = [att(1, INT4OID, 4, true, b'i', false), att(2, 0, 4, true, b'i', true), att(3, TEXTOID, -1, false, b'i', false)];
        assert!(check_create_supported("t", &atts).is_ok());
        let cols = col_schemas_of("t", &atts).expect("derives");
        assert_eq!(cols.iter().map(|c| c.attno).collect::<Vec<_>>(), vec![1, 3], "the dropped attno is a gap");
        assert!(derive_one(&att(3, 0, 4, true, b'i', false)).is_err(), "a live oid-0 attribute has no type to store");
    }

    /// D2189 (ported): a column dropped after its data was written (int4, float8, bool by value; text by reference)
    /// leaves the parts under the stamp they were written with; `stamped_of` rebuilds it from the dropped attribute PG
    /// keeps, also for a column dropped before the stamp (absent from it), and finds nothing for a stamp no candidate
    /// gives.
    #[test]
    fn a_column_dropped_after_its_writes_rebuilds_its_stamp() {
        for (typ, len, byval, align) in [(INT4OID, 4, true, b'i'), (FLOAT8OID, 8, true, b'd'), (BOOLOID, 1, true, b'c'), (TEXTOID, -1, false, b'i')] {
            let mut live = [att(1, INT4OID, 4, true, b'i', false), att(2, typ, len, byval, align, false), att(3, TEXTOID, -1, false, b'i', false)];
            // the collatable one keeps a collation whose class is not C (9002: Nondeterministic whether or not
            // collation_tests' probe is installed)
            live[1].attcollation = if byval { 0 } else { 9002 };
            let stamp = schema_fingerprint(&derive_all(&live));
            let mut gone = live;
            drop_att(&mut gone[1]);
            let reader = col_schemas_of("t", &gone).expect("derives");
            assert_ne!(schema_fingerprint(&reader), stamp, "oid {typ}: the drop changes the reader's schema");
            let got = stamped_of(&gone, stamp).unwrap_or_else(|m| panic!("oid {typ}: the stamp is rebuilt ({m:?})"));
            assert_eq!(schema_fingerprint(&got), stamp, "oid {typ}");
            assert_eq!(got.iter().map(|c| c.attno).collect::<Vec<_>>(), vec![1, 2, 3]);
            assert!(reader.iter().all(|r| got.iter().any(|g| g.attno == r.attno && g.same_storage(r))), "oid {typ}: every live column at its storage");
            assert!(matches!(stamped_of(&gone, stamp ^ 1), Err(StampMiss::NoMatch { candidates: 4 })), "oid {typ}: no candidate gives another stamp");
            // dropped before the stamp's parts: the stamp has no such column
            let before = schema_fingerprint(&reader);
            assert_eq!(stamped_of(&gone, before).map(|c| c.len()), Ok(2), "oid {typ}: absent from an older-drop stamp");
        }
    }

    /// D2189: DROP COLLATION ... CASCADE drops the column and the pg_collation row, so the column's kept
    /// attcollation (9003: no row, Nondeterministic with or without collation_tests' probe) no longer classifies as the
    /// collation it was written with. Each of the three classes it can have been written with is rebuilt.
    #[test]
    fn a_column_of_a_dropped_collation_rebuilds_under_each_collation_class() {
        let mut live = [att(1, INT8OID, 8, true, b'd', false), att(2, INT4OID, 4, true, b'i', false), att(3, TEXTOID, -1, false, b'i', false)];
        live[2].attcollation = 9003;
        for written in [CollationClass::C, CollationClass::OtherDeterministic, CollationClass::Nondeterministic] {
            let mut cols = derive_all(&live);
            cols[2].collation_class = written;
            let stamp = schema_fingerprint(&cols);
            let mut gone = live;
            drop_att(&mut gone[2]);
            let got = stamped_of(&gone, stamp).unwrap_or_else(|m| panic!("{written:?}: the stamp is rebuilt ({m:?})"));
            assert_eq!(got.len(), 3, "{written:?}");
            assert_eq!(got[2].collation_class, written, "the class it was written with");
            assert!(got[..2].iter().zip(&cols[..2]).all(|(g, c)| g.same_storage(c)), "{written:?}: the live columns as derived");
        }
    }

    /// D2189, fail closed: past the 4096-candidate cap (seven dropped collatable columns: 4^7 = 16384
    /// candidates) nothing is tried and the named refusal (XX001) is raised, nothing booked; at the cap (six: 4^6 =
    /// 4096) the stamp is found, and a stamp no candidate gives is the same refusal; a stamp the book already holds
    /// passes whatever the count.
    #[test]
    fn past_the_cap_or_with_no_match_the_rebuild_fails_closed() {
        let table = |dropped: i16| {
            let mut atts = vec![att(1, INT8OID, 8, true, b'd', false)];
            for i in 0..dropped {
                let mut a = att(2 + i, TEXTOID, -1, false, b'i', false);
                a.attcollation = 9003;
                atts.push(a);
            }
            atts
        };
        let written = |atts: &[FormData_pg_attribute]| {
            let mut cols = derive_all(atts);
            for c in &mut cols[1..] {
                c.collation_class = CollationClass::OtherDeterministic;
            }
            schema_fingerprint(&cols)
        };
        let drop_all = |mut atts: Vec<FormData_pg_attribute>| {
            atts[1..].iter_mut().for_each(drop_att);
            atts
        };

        let seven = table(7);
        let stamp7 = written(&seven);
        let gone7 = drop_all(seven.clone());
        assert_eq!(stamped_of(&gone7, stamp7), Err(StampMiss::OverCap { candidates: 16384 }));
        let e = book_dropped_stamp_of(Some("t7"), &gone7, stamp7).expect_err("past the cap: refused");
        assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
        assert!(e.message().starts_with("pgrc read: dropped-column stamp ") && e.message().contains("of relation \"t7\" not rebuilt: 16384 candidate schemas, past the cap of 4096"), "{}", e.message());
        assert!(crate::fastadd::schema_of(stamp7).is_none(), "nothing booked past the cap");

        let six = table(6);
        let stamp6 = written(&six);
        let gone6 = drop_all(six);
        let got = stamped_of(&gone6, stamp6).expect("at the cap: found");
        assert_eq!(schema_fingerprint(&got), stamp6);
        let e = book_dropped_stamp_of(None, &gone6, stamp6 ^ 1).expect_err("no candidate: refused");
        assert_eq!(e.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
        assert!(e.message().contains("of a pgrcolumnar relation not rebuilt: none of 4096 candidate schemas has its fingerprint"), "{}", e.message());
        assert!(crate::fastadd::schema_of(stamp6 ^ 1).is_none(), "nothing booked without a match");
        book_dropped_stamp_of(None, &gone6, stamp6).expect("found: booked");
        assert!(crate::fastadd::schema_of(stamp6).is_some(), "booked");

        // a described stamp is the book's, never searched
        let mut cols7 = derive_all(&seven);
        cols7[1..].iter_mut().for_each(|c| c.collation_class = CollationClass::OtherDeterministic);
        crate::fastadd::note(&cols7);
        book_dropped_stamp_of(Some("t7"), &gone7, stamp7).expect("a described stamp passes");
        // a table with no dropped column books nothing and passes (the presence check answers as before)
        book_dropped_stamp_of(Some("t"), &table(1), 0x1234).expect("no dropped column: nothing to rebuild");
        assert!(crate::fastadd::schema_of(0x1234).is_none());
    }
}

#[cfg(test)]
mod collation_tests {
    use super::*;
    use syscache_seams::{lookup_pg_collation_shape, PgCollationShape};

    fn coll_name(s: &str) -> types_tuple::NameData {
        let mut n = types_tuple::NameData::default();
        n.namestrcpy(s);
        n
    }

    /// BORN-RED (#598 leg 2): the collation gate must FAIL CLOSED. A
    /// non-C/POSIX attcollation is deterministic only if
    /// `pg_collation.collisdeterministic` SAYS so; every UNKNOWN — probe
    /// unreachable, row missing, probe error — classifies Nondeterministic
    /// (no eq blooms), never a deterministic guess. A case-insensitive ICU
    /// collation classified OtherDeterministic seals byte-hash blooms where
    /// equal-under-collation, byte-different probes read definite-absent:
    /// silently missing rows the day predicate pushdown wires.
    ///
    /// ONE test on purpose: the seam is set-once per process, so the
    /// uninstalled arm must be asserted before the installed arms, in order,
    /// in a single body — parallel test threads must not race the install.
    #[test]
    fn collation_classification_fails_closed() {
        // C-class oids never probe (byte order IS the collation order).
        assert_eq!(collation_class(0), CollationClass::C);
        assert_eq!(collation_class(C_COLLATION_OID), CollationClass::C);
        assert_eq!(collation_class(POSIX_COLLATION_OID), CollationClass::C);

        // UNKNOWN: the probe is unreachable in this binary (seam not
        // installed) — classify Nondeterministic, never guess.
        assert!(!lookup_pg_collation_shape::is_installed());
        assert_eq!(
            collation_class(9002),
            CollationClass::Nondeterministic,
            "unreachable pg_collation probe must classify fail-closed"
        );
        // M4-S8: DEFAULT with NEITHER seam installed stays fail-closed —
        // the library world never guesses the database locale.
        assert!(!pg_locale_seams::collation_collate_is_c::is_installed());
        assert_eq!(
            collation_class(DEFAULT_COLLATION_OID),
            CollationClass::Nondeterministic,
            "DEFAULT without a locale answer must classify fail-closed"
        );

        // Install the probe (this test is the only installer in the
        // binary) and classify FROM collisdeterministic.
        lookup_pg_collation_shape::set(|oid| match oid {
            9001 => Ok(Some(PgCollationShape {
                collname: coll_name("icu_det"),
                collnamespace: 11,
                collisdeterministic: true,
            })),
            9002 => Ok(Some(PgCollationShape {
                collname: coll_name("icu_ci"),
                collnamespace: 11,
                collisdeterministic: false,
            })),
            9004 => Err(types_error::PgError::error("collation probe failed").into()),
            _ => Ok(None),
        });
        assert_eq!(
            collation_class(9001),
            CollationClass::OtherDeterministic,
            "a provably deterministic collation keeps equality metadata"
        );
        assert_eq!(
            collation_class(9002),
            CollationClass::Nondeterministic,
            "collisdeterministic = false must never arm byte-hash blooms"
        );
        assert_eq!(
            collation_class(9003),
            CollationClass::Nondeterministic,
            "a dangling attcollation (no pg_collation row) is UNKNOWN: fail closed"
        );
        assert_eq!(
            collation_class(9004),
            CollationClass::Nondeterministic,
            "a probe ERROR is UNKNOWN: fail closed, and blooms are advisory"
        );

        // M4-S8 (the engine-cut substrate finding): with the locale answer
        // installed, DEFAULT-under-C resolves to the honest C class — the
        // class dict election rides. A non-C default keeps the
        // conservative fallthrough (here: the syscache probe answers None
        // for oid 100 => Nondeterministic).
        assert_eq!(
            collation_class(DEFAULT_COLLATION_OID),
            CollationClass::Nondeterministic,
            "DEFAULT before the locale seam installs stays conservative"
        );
        pg_locale_seams::collation_collate_is_c::set(|oid| {
            Ok(oid == DEFAULT_COLLATION_OID)
        });
        assert_eq!(
            collation_class(DEFAULT_COLLATION_OID),
            CollationClass::C,
            "DEFAULT on a C-locale database is memcmp order: class C"
        );
        assert_eq!(
            collation_class(9001),
            CollationClass::OtherDeterministic,
            "named collations never ride the DEFAULT resolution"
        );
    }
}
