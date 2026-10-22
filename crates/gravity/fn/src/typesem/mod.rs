//! The canonical binary form of a key with no native image (lowerv4 rep.hosted.hash, M6-HOSTED; M6-plan.md §3.4, main
//! 09-28): bytes that are equal exactly when PostgreSQL's default equality calls the two values equal, so the Fp128 of the
//! form is an exact key image (the keyed tables have no confirm path). The stored bytes are not canonical: an array may
//! carry a null bitmap with no NULL, a varlena a 1-byte or a 4-byte header, a float -0 or one of many NaNs, a numeric any
//! scale of one value. So the walk rebuilds the value from its parts:
//! - array (`array_eq`): ndim, each dim and lower bound, then per element a null flag and the element's form (two NULL
//!   elements are equal, as array_eq has them);
//! - composite (`record_eq`): per live column a null flag and the column's form (a datum shorter than its type reads its
//!   missing columns as NULL, as heap_deform_tuple does; a column stored compressed inline is decompressed first, as
//!   record_eq detoasts it, D1607); a dropped column refuses at `form` (its width is not in the catalog row the seams
//!   answer);
//! - range (`range_eq`): the flags (empty, inclusivity, infinity) and each finite bound's form (range_eq compares the
//!   bounds by the range's subtype opclass: one other than the opclass GetDefaultOpClass picks for the subtype refuses,
//!   D1889);
//! - multirange (`multirange_eq` / `hash_multirange`, V4R-508): the range count, then each range in stored order as the
//!   range's form behind its length (a multirange stores its ranges canonicalized, sorted and merged, so two equal
//!   multiranges store the same ranges in the same order; each bound read as `multirange_get_bounds` reads it);
//! - leaves: a by-value or fixed by-reference type whose `=` is its bytes (integers, oid, date, timestamps, uuid, macaddr,
//!   tid, timetz, aclitem, enum labels), a float with -0 folded to 0 and every NaN one pattern, name up to its NUL, an
//!   interval as `interval_cmp_value`'s 128-bit span (1 mon = 30 days), a numeric as `hash_numeric`'s normalized digits,
//!   and a varlena whose `=` is its payload (text / varchar under a deterministic collation, bpchar without its trailing
//!   blanks, bytea, bit / varbit, inet / cidr); each part of a container behind its length.
//!
//! Anything else (an anonymous record, a nondeterministic collation, a user base type, geometric types, a type at any
//! level whose default btree opclass, its family, its equality operator or its comparison proc is a user object, D1604)
//! is `Err(why)` from `form`: the lowering refuses the key by that name. `form` reads the catalog through the
//! syscache seams and runs at lowering only, on the leader; the program hands the host entry (kernels `m6_hosted_fp`) the
//! content id of that exact interned form, which it resolves in any thread (a pool worker's has no catalog).

use crate::PgError;

/// hosts ops/agg/trans's run-time type-cache reads (its family's sibling file).
pub mod aggtrans;
pub mod collation;
pub mod image;
pub mod integral;
pub mod keysem;
/// gravity_expr's catalog adapter reads (its family's sibling file).
pub mod expr;
/// exec ops/memoize's run-time key lookups (its family's sibling file).
pub mod memoize;
pub mod numeric;
pub mod pgrc;
pub mod prefix;
/// fn srf_fmgr's catalog gates (its family's sibling file).
pub mod srf;
pub mod storage;

pub use keysem::{available, bits_decide, eq_form, hash_procs, intern_op, resolve, By, EqForm, KeyReq, KeySem, KeyUse, Procs, TypeClass, INDETERMINATE};

type Oid = u32;
type R<T> = Result<T, Box<PgError>>;

/// A form's leaf: how one value's bytes become canonical.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Leaf {
    Bytes,
    Float4,
    Float8,
    Name,
    Interval,
    Numeric,
    Varlena,
    Bpchar,
    /// D1308: jsonb, by `compareJsonbContainers`' equality: its stored tree (object keys already in their canonical order,
    /// PostgreSQL's `uniqueifyJsonbObject`) with every number as `hash_numeric`'s normalized digits and the raw-scalar flag.
    Jsonb,
    /// An anonymous record's text / varchar (or `bpchar`) column under nondeterministic collation `coll` (`record_form`):
    /// the provider's sort key of its payload (bpchar's without trailing blanks), what hashtext / hashbpchar hash under
    /// it, so the strings the collation calls equal have one image.
    Coll { coll: Oid, bpchar: bool },
    /// D3786: oidvector, by oidvectoreq / hashoidvector (oid.c): its element count and values, never its lower bound
    /// (`oidvector`).
    OidVector,
}

/// The canonical form of one type; `len` / `byval` / `align` are its storage (typlen, typbyval, typalign).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Form {
    len: i16,
    byval: bool,
    align: u8,
    kind: Kind,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Kind {
    Leaf(Leaf),
    Array(Box<Form>),
    Record(Vec<Form>),
    Range(Box<Form>),
    /// A multirange: its range type's subtype form (each range read as `Range` over it).
    Multirange(Box<Form>),
}

const F_ARRAY_SUBSCRIPT_HANDLER: Oid = 6179;
const DEFAULT_COLLATION: Oid = 100;
const RECORDOID: Oid = 2249;

/// Whether `typid` is the anonymous record pseudo-type (`record`): a row whose columns are known only per value.
pub fn is_anon_record(typid: Oid) -> bool {
    typid == RECORDOID
}

/// The leaf of a built-in type whose `=` the canonical bytes decide; `None` for any other.
fn leaf_of(typid: Oid) -> Option<Leaf> {
    use types_core::catalog::*;
    Some(match typid {
        // by value: bool, char, int8, int2, int4, regproc, oid, xid, cid, money, date, time, timestamp, timestamptz,
        // reg* types, pg_lsn, xid8; fixed by reference: tid, macaddr8, macaddr, aclitem, timetz, uuid
        BOOLOID | CHAROID | INT8OID | INT2OID | INT4OID | REGPROCOID | OIDOID | XIDOID | CIDOID | CASHOID | DATEOID
        | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID | REGPROCEDUREOID | REGOPEROID | REGOPERATOROID | REGCLASSOID
        | REGTYPEOID | LSNOID | REGCONFIGOID | REGDICTIONARYOID | REGNAMESPACEOID | REGROLEOID | REGCOLLATIONOID
        | XID8OID | TIDOID | MACADDR8OID | MACADDROID | ACLITEMOID | TIMETZOID | UUIDOID => Leaf::Bytes,
        FLOAT4OID => Leaf::Float4,
        FLOAT8OID => Leaf::Float8,
        NAMEOID => Leaf::Name,
        INTERVALOID => Leaf::Interval,
        NUMERICOID => Leaf::Numeric,
        TEXTOID | VARCHAROID | BYTEAOID | BITOID | VARBITOID | INETOID | CIDROID => Leaf::Varlena,
        BPCHAROID => Leaf::Bpchar,
        JSONBOID => Leaf::Jsonb,
        _ => return None,
    })
}

/// Whether `opno` is operator family `opfamily`'s btree equality member (BTEqualStrategyNumber), as a search operator
/// (`get_op_opfamily_properties`, not an ordering one): a qual `key = x` binds that index key column by the index's own
/// equality. Asked of each key column of a unique index: exec inputs/knobs/tier0.rs's unique point and exec
/// runner/execute.rs's one-row probes of a write's scan. False when `opno` is no member of the family (a failed catalog
/// read).
pub fn btree_equality_member(opno: Oid, opfamily: Oid) -> bool {
    lsyscache::get_op_opfamily_properties(opno, opfamily, false).is_ok_and(|(st, _, _)| st == lsyscache::BTEqualStrategyNumber as i32)
}

/// PostgreSQL's `BTORDER_PROC` (nbtree.h): a btree family's comparison support function.
pub const BTORDER_PROC: i16 = 1;

/// lsyscache `get_op_opfamily_properties`: operator `opno`'s (strategy, lefttype, righttype) in `opfamily`, as an
/// ordering operator when `ordering_op` (nodeIndexscan.c ExecIndexBuildScanKeys: an index recipe's scan keys and
/// row-compare members, an index ORDER BY key). A run-time read through lsyscache's own caches.
pub fn op_opfamily_properties(opno: Oid, opfamily: Oid, ordering_op: bool) -> types_error::PgResult<(i32, Oid, Oid)> {
    lsyscache::get_op_opfamily_properties(opno, opfamily, ordering_op)
}

/// lsyscache `get_opfamily_proc`: support procedure `procnum` of `opfamily` for (lefttype, righttype) (nodeIndexscan.c
/// ExecIndexBuildScanKeys: a row-compare member's order proc). A run-time read through lsyscache's own caches.
pub fn opfamily_proc(opfamily: Oid, lefttype: Oid, righttype: Oid, procnum: i16) -> types_error::PgResult<Oid> {
    lsyscache::get_opfamily_proc(opfamily, lefttype, righttype, procnum)
}

/// lsyscache `get_opfamily_member` then `get_opcode`: the function of `opfamily`'s strategy `strategy` operator at
/// (`intype`, `intype`), as a scan key over a column of the class's input type calls it (an ordered index walk's key
/// range bounds, hosts index_ranges); 0 when the family has no such member. A run-time read through lsyscache's own caches.
pub fn opfamily_member_proc(opfamily: Oid, intype: Oid, strategy: i16) -> types_error::PgResult<Oid> {
    let op = lsyscache::get_opfamily_member(opfamily, intype, intype, strategy)?;
    if op == 0 {
        return Ok(0);
    }
    lsyscache::get_opcode(op)
}

/// Whether a built-in type's `=` is its stored bytes (`Leaf::Bytes`: integers, oid, date, timestamps, uuid, ...), the
/// by-value ones of which key as their word.
pub fn bytes_leaf(typid: Oid) -> bool {
    matches!(leaf_of(typid), Some(Leaf::Bytes))
}

/// Whether a built-in type's equal values are one image (`=` holds only between equal bytes): `Leaf::Bytes`, and the
/// varlenas text / varchar / bytea / bit / varbit / inet / cidr under a collation that compares by bytes (the caller
/// checks the collation), and `anyenum` (an enum's `=` is its label oid's, enum_eq: every enum type). False for numeric
/// (1.0 / 1.00), the floats (-0 / 0), interval, bpchar, name and jsonb.
pub fn one_image(typid: Oid) -> bool {
    typid == types_core::catalog::ANYENUMOID || matches!(leaf_of(typid), Some(Leaf::Bytes | Leaf::Varlena))
}

/// OID-23: whether every one of a call's argument types has one image per equal value (`one_image`); a call with no
/// argument list (a user aggregate the caller has no row for) fails closed.
pub fn args_one_image(args: Option<&[Oid]>) -> bool {
    args.is_some_and(|a| a.iter().all(|&t| one_image(t)))
}

/// Whether a built-in type's datum is text's own: a varlena whose payload bytes are the value, compared, hashed and
/// output as text (text, and varchar, whose `=`, hash and output functions are text's: pg_amop, varchar's binary cast to
/// text). False for bpchar (its trailing blanks are not part of its value), name and every other type.
pub fn text_payload(typid: Oid) -> bool {
    use types_core::catalog::{TEXTOID, VARCHAROID};
    matches!(typid, TEXTOID | VARCHAROID)
}

/// Whether a built-in type is one of PostgreSQL's character types (datatype.sgml "Character Types": character varying,
/// character, text), the varlena strings a dictionary text column stores.
pub fn character_type(typid: Oid) -> bool {
    use types_core::catalog::{BPCHAROID, TEXTOID, VARCHAROID};
    matches!(typid, TEXTOID | VARCHAROID | BPCHAROID)
}

/// How a type's stored image orders as the type (the "order-preserving image" property call, `order_image`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderBy {
    /// A by-value datum word compared two's-complement: the integers, date, time, the timestamps, money.
    Signed,
    /// A by-value datum word compared unsigned: bool, "char", oid, xid8, pg_lsn.
    Unsigned,
    /// The varlena's payload bytes compared by memcmp, shorter first on a tie: bytea always; text / varchar only under a
    /// collation that orders by bytes (C, POSIX: the caller checks the collation).
    Bytes,
}

/// How a built-in type's image orders as its btree order (typesem `KeyUse::Sort`); `None` when no image of the type
/// orders as the type (numeric, the floats, interval, bpchar, uuid's 16 bytes, user types).
pub fn order_image(typid: Oid) -> Option<OrderBy> {
    use types_core::catalog::*;
    match typid {
        INT2OID | INT4OID | INT8OID | DATEOID | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID | CASHOID => Some(OrderBy::Signed),
        BOOLOID | CHAROID | OIDOID | XID8OID | LSNOID => Some(OrderBy::Unsigned),
        BYTEAOID | TEXTOID | VARCHAROID => Some(OrderBy::Bytes),
        _ => None,
    }
}

/// How a by-value type orders its datum words: `Some(true)` signed, `Some(false)` unsigned (`order_image`'s word
/// answers), `None` when word order is not its order or it is not a by-value bytes type.
pub fn word_order(typid: Oid) -> Option<bool> {
    match order_image(typid)? {
        OrderBy::Signed => Some(true),
        OrderBy::Unsigned => Some(false),
        OrderBy::Bytes => None,
    }
}

/// int2, int4, int8: the integers a top's order slot holds as one signed word (lowerv4 topn order_slot.rs). Exactly these
/// three: date, time, the timestamps and money also order as signed words (`order_image`) but take no int slot there.
pub fn int_word(typid: Oid) -> bool {
    use types_core::catalog::*;
    matches!(typid, INT2OID | INT4OID | INT8OID)
}

/// Whether key type `typid`'s form is one by-value leaf whose `=` is its bytes (an enum, a by-value built-in of
/// `Leaf::Bytes`): its datum's bits are its value. False for a type with no form (a user equality).
pub fn bits_form(typid: Oid) -> R<bool> {
    Ok(matches!(build(typid, 0)?, Ok(Form { byval: true, kind: Kind::Leaf(Leaf::Bytes), .. })))
}

/// Whether collation `coll` equates exactly the equal byte strings (the "collation bytes" property call): 0 (a type
/// that is not collatable), the database default (PG makes it deterministic, `init_database_collation`), C, POSIX, or
/// any collation the catalog marks `collisdeterministic`. The built-in three answer without a catalog read (a lowering
/// outside a session has no default locale); a failed catalog read, or a collation the catalog does not have, is the
/// error, as lsyscache's (a rig with no catalog or no such collation refuses).
pub fn coll_bytes(coll: Oid) -> R<bool> {
    use crate::typesem::collation::{C_COLLATION_OID, POSIX_COLLATION_OID};
    if matches!(coll, 0 | DEFAULT_COLLATION | C_COLLATION_OID | POSIX_COLLATION_OID) {
        return Ok(true);
    }
    Ok(lsyscache::get_collation_isdeterministic(coll)?)
}

/// `coll_bytes` for a claim that can only decline (an open-coded compare): a failed read, or a lowering with no
/// catalog at all, declines.
pub fn claims_coll_bytes(coll: Oid) -> bool {
    use crate::typesem::collation::{C_COLLATION_OID, POSIX_COLLATION_OID};
    if matches!(coll, 0 | DEFAULT_COLLATION | C_COLLATION_OID | POSIX_COLLATION_OID) {
        return true;
    }
    // interrupts held (D376): a cancel the catalog read meets stays pending instead of being consumed into a decline
    let _held = init_small::globals::hold_interrupts();
    syscache_seams::lookup_pg_collation_shape::is_installed() && coll_bytes(coll).unwrap_or(false)
}

/// Every distinct form the lowerings built, interned by its content id (`id_of`: one leaked copy per distinct form,
/// never mutated or freed: bounded by the distinct key types' forms). A program carries its lowering's form id, a
/// constant that is the same in every process (IR pins stay exact); a catalog change's new form is a new id, so a later
/// ALTER TYPE, or another session's uncommitted one, never changes what a lowered program reads. The host resolves the id
/// here (`by_id`) in any thread, a pool worker's included, which has no catalog.
static INTERNED: pgsync::RwLock<Vec<(u64, &'static Form)>> = pgsync::RwLock::new(Vec::new()); // alloc: Cold

/// A form's content id: the SipHash (fixed keys) of its structure, the same in every process.
fn id_of(f: &Form) -> u64 {
    use core::hash::{Hash, Hasher};
    #[allow(deprecated)]
    let mut h = core::hash::SipHasher::new_with_keys(0x6d36_686f_7374_6564, 0x6361_6e6f_6e20_6964);
    f.hash(&mut h);
    h.finish()
}

/// The canonical form of key type `typid` as this session's catalog shows it now, interned, with its id; or the reason
/// it has none. The lowering's call (the leader: a pool worker has no catalog); a catalog read that fails is the error.
pub fn form(typid: Oid) -> R<Result<(u64, &'static Form), &'static str>> {
    let f = match build(typid, 0)? {
        Ok(f) => f,
        Err(why) => return Ok(Err(why)),
    };
    Ok(Ok(intern(f)))
}

/// The canonical form of an anonymous record key whose columns are `cols` (each its type and collation: decompose's
/// `facts::record::columns` of the ROW() every producer of the key agrees with), interned, with its id; or the reason it
/// has none. `record_eq` compares two such records column by column, each by its type's default equality under the
/// column's collation (a blessed ROW()'s column collation is its argument's, D1887), as it compares a named composite's,
/// so the form is the composite form over the columns' forms in `record`'s storage (typlen -1, by reference, double
/// aligned). A column with no form is the reason; so is one under a collation whose equal values are not its equal bytes,
/// but a text, varchar or bpchar column (`coll_leaf`). The lowering's call, as `form`'s.
pub fn record_form(cols: &[(Oid, Oid)]) -> R<Result<(u64, &'static Form), &'static str>> {
    let mut forms = Vec::with_capacity(cols.len()); // alloc: Cold
    for &(typ, coll) in cols {
        if !coll_bytes(coll)? {
            match coll_leaf(typ, coll)? {
                Some(f) => forms.push(f),
                None => return Ok(Err("an anonymous record key's column under a nondeterministic collation")),
            }
            continue;
        }
        match build(typ, 1)? {
            Ok(f) => forms.push(f),
            Err(why) => return Ok(Err(why)),
        }
    }
    Ok(Ok(intern(record_of(forms))))
}

/// `record_form`'s column of type `typ` under nondeterministic collation `coll`: a text, varchar or bpchar keyed by its
/// sort key (`Leaf::Coll`) when the provider has a trusted one (ICU); `None` for any other type or provider. Building the
/// locale here, on the leader, resolves it for a pool worker's claim (D316).
fn coll_leaf(typ: Oid, coll: Oid) -> R<Option<Form>> {
    use types_core::catalog::{BPCHAROID, TEXTOID, VARCHAROID};
    let bpchar = match typ {
        TEXTOID | VARCHAROID => false,
        BPCHAROID => true,
        _ => return Ok(None),
    };
    if !crate::typesem::collation::strxfrm_trusted(coll)? {
        return Ok(None);
    }
    Ok(match build(typ, 1)? {
        Ok(f @ Form { kind: Kind::Leaf(Leaf::Varlena | Leaf::Bpchar), .. }) => Some(Form { kind: Kind::Leaf(Leaf::Coll { coll, bpchar }), ..f }),
        _ => None,
    })
}

/// A record over column forms `cols`, in `record`'s storage (typlen -1, by reference, typalign 'd').
fn record_of(cols: Vec<Form>) -> Form {
    Form { len: -1, byval: false, align: 8, kind: Kind::Record(cols) }
}

/// D1901, V4R-508's in-row refusal (decompose's hash join and group keys ask through the server's `Facts`): whether key
/// type `typid`'s form holds a multirange at any level (the type, an array's element, a composite's column, a range's
/// subtype), the keys this revision newly serves. False for a type with no form (its lowering refuses it by that Shape)
/// and for a failed catalog read (the lowering's own read refuses). Builds the form without interning it.
pub fn holds_multirange(typid: Oid) -> bool {
    matches!(build(typid, 0), Ok(Ok(f)) if f.holds_multirange())
}

impl Form {
    fn holds_multirange(&self) -> bool {
        match &self.kind {
            Kind::Leaf(_) => false,
            Kind::Multirange(_) => true,
            Kind::Array(f) | Kind::Range(f) => f.holds_multirange(),
            Kind::Record(cols) => cols.iter().any(Form::holds_multirange),
        }
    }
}

fn intern(f: Form) -> (u64, &'static Form) {
    let id = id_of(&f);
    let mut t = INTERNED.write().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = t.iter().find(|e| e.0 == id) {
        assert!(*e.1 == f, "gravity_fn canon: two forms with one id");
        return *e;
    }
    let g: &'static Form = Box::leak(Box::new(f)); // alloc: Cold
    t.push((id, g));
    (id, g)
}

thread_local! {
    /// The last id this thread resolved (a group's rows repeat one key type): no lock per row.
    static LAST: core::cell::Cell<(u64, usize)> = const { core::cell::Cell::new((0, 0)) };
}

/// The interned form of id `id` (any thread, no catalog); an error when no lowering in this process built it.
pub fn by_id(id: u64) -> R<&'static Form> {
    let (lid, lp) = LAST.with(core::cell::Cell::get);
    if lid == id && lp != 0 {
        // SAFETY: `lp` is an interned form's address: leaked, never freed or mutated
        return Ok(unsafe { &*(lp as *const Form) });
    }
    let t = INTERNED.read().unwrap_or_else(|e| e.into_inner());
    let f = t.iter().find(|e| e.0 == id).map(|e| e.1).ok_or_else(|| Box::new(PgError::error(format!("gravity: rep.hosted.hash: canonical form {id:#x} was not built in this process"))))?; // alloc: Cold
    LAST.with(|c| c.set((id, f as *const Form as usize)));
    Ok(f)
}

fn build(typid: Oid, depth: u32) -> R<Result<Form, &'static str>> {
    if depth > 16 {
        return Ok(Err("a key type nested deeper than 16 levels"));
    }
    if typid == RECORDOID {
        return Ok(Err("an anonymous record key (its columns are known only per row)"));
    }
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(Err("a key type the catalog does not know")) };
    if t.typtype as u8 != b'd' && user_equality(typid)? {
        return Ok(Err(USER_EQUALITY));
    }
    let (len, byval, align) = (t.typlen, t.typbyval, align_of(t.typalign));
    let form = |kind| Ok(Ok(Form { len, byval, align, kind }));
    match t.typtype as u8 {
        b'd' => {
            let Some(b) = syscache_seams::pg_type_base_shape::call(typid)? else { return Ok(Err("a domain the catalog does not know")) };
            build(b.typbasetype, depth + 1)
        }
        b'e' => form(Kind::Leaf(Leaf::Bytes)),
        b'c' => {
            // D3574: a plan-keyed program keeps this column list; each attribute read (and the miss past the last) is
            // recorded (C3, gravity_deps: ATTNUM rows), which ALTER TYPE .. ADD / DROP / ALTER ATTRIBUTE invalidates
            let mut cols = Vec::new(); // alloc: Cold
            for att in 1.. {
                let Some(a) = syscache_seams::lookup_pg_attribute_shape::call(t.typrelid, att)? else { break };
                if a.attisdropped {
                    return Ok(Err("a composite key with a dropped column"));
                }
                if !coll_bytes(a.attcollation)? {
                    match coll_leaf(a.atttypid, a.attcollation)? {
                        Some(f) => cols.push(f),
                        None => return Ok(Err("a composite key's column under a nondeterministic collation")),
                    }
                    continue;
                }
                match build(a.atttypid, depth + 1)? {
                    Ok(f) => cols.push(f),
                    Err(why) => return Ok(Err(why)),
                }
            }
            form(Kind::Record(cols))
        }
        b'r' => {
            let Some(r) = syscache_seams::lookup_pg_range_shape::call(typid)? else { return Ok(Err("a range the catalog does not know")) };
            if !coll_bytes(r.rngcollation)? {
                return Ok(Err("a range key under a nondeterministic collation"));
            }
            // D1889 / D1604 (K2.kgrp, main 2026-10-04): range_eq compares the bounds by the declared subtype opclass
            // (range_cmp_bounds through its FUNCTION 1); the form keeps them under the subtype's own `=`, so the opclass's
            // equality must be that `=` (`subtype_equality`), a user opclass's included
            // D1604: a subtype whose own `=` is user code (a user btree opclass declared DEFAULT for it) has no form
            if user_equality(r.rngsubtype)? {
                return Ok(Err(USER_EQUALITY));
            }
            if !subtype_equality(r.rngsubtype, r.rngsubopc)? {
                return Ok(Err("a range key whose subtype opclass is not the subtype's default"));
            }
            match build(r.rngsubtype, depth + 1)? {
                Ok(f) => form(Kind::Range(Box::new(f))), // alloc: Cold
                Err(why) => Ok(Err(why)),
            }
        }
        // hash_multirange / multirange_eq: its ranges in stored order, each by its range type's form
        b'm' => {
            let Some(rng) = syscache_seams::lookup_pg_range_by_multirange::call(typid)? else { return Ok(Err("a multirange the catalog does not know")) };
            match build(rng, depth + 1)? {
                Ok(Form { kind: Kind::Range(sub), .. }) => form(Kind::Multirange(sub)),
                Ok(_) => Ok(Err("a multirange whose range type is not a range")),
                Err(why) => Ok(Err(why)),
            }
        }
        // D3786: oidvector subscripts as an array but compares as its own type (oidvector_ops: btoidvectorcmp /
        // hashoidvector read dim1 and the values), so two equal vectors may differ in lower bound: array_eq's form
        // (dims and lower bounds) would part them
        b'b' if typid == types_core::catalog::OIDVECTOROID => form(Kind::Leaf(Leaf::OidVector)),
        b'b' if t.typsubscript == F_ARRAY_SUBSCRIPT_HANDLER && len == -1 && t.typelem != 0 => match build(t.typelem, depth + 1)? {
            Ok(f) => form(Kind::Array(Box::new(f))), // alloc: Cold
            Err(why) => Ok(Err(why)),
        },
        b'b' => match leaf_of(typid) {
            Some(l) => form(Kind::Leaf(l)),
            None => Ok(Err("a key type whose equality is not its canonical bytes (a user or geometric type)")),
        },
        _ => Ok(Err("a pseudo-type key")),
    }
}

/// D1889: whether a range's subtype opclass `opc` orders subtype `sub` as the subtype's default btree opclass does. The
/// type cache's btree_opf / btree_opintype are GetDefaultOpClass(sub, btree)'s family and input type (a domain read as
/// its base type; a binary-coercible or polymorphic default such as text_ops for varchar, array_ops, enum_ops or
/// record_ops as PostgreSQL picks it); range_cmp_bounds runs get_opfamily_proc(family, opcintype, opcintype,
/// BTORDER_PROC) of `opc`, so an equal family and input type is the subtype's own comparison. `opcdefault` alone is not
/// enough: bpchar_ops is a default opclass that PostgreSQL accepts for a varchar or text subtype through their binary
/// casts to bpchar (its equality ignores trailing blanks), and a DEFAULT opclass declared for a domain is not the one
/// GetDefaultOpClass picks for it. A subtype with no default btree opclass refuses.
fn subtype_default_opclass(sub: Oid, opc: Oid) -> R<bool> {
    let e = typcache::lookup_type_cache(sub, typcache::TYPECACHE_BTREE_OPFAMILY)?;
    let (opf, opcintype) = (e.btree_opf(), e.btree_opintype());
    Ok(opf != 0 && lsyscache::get_opclass_family(opc)? == opf && lsyscache::get_opclass_input_type(opc)? == opcintype)
}

/// D1889 / D1604 (K2.kgrp, main 2026-10-04: honour the range type's declared subtype opclass, no opclass list): whether
/// subtype opclass `opc` equates subtype `sub`'s values as the subtype's own `=` does. Either it orders as the subtype's
/// default (`subtype_default_opclass`), or its family's equality member over its input type (BTEqualStrategyNumber) is
/// the operator the subtype's default btree opclass names (`own_eq`: the type cache's eq_opr) on an input type that is the
/// subtype's own. A btree opclass's comparison (FUNCTION 1, user code included) returns 0 exactly when that member holds:
/// the btree contract PostgreSQL's planner relies on when it swaps the operator and the comparison. So range_cmp_bounds
/// equates two bounds iff the subtype's `=` does, and the subtype's form decides range_eq exactly. The comparison's calls
/// are a support function's (DECISIONS 6: their count is free). bpchar_ops over a text subtype names bpchareq, not
/// texteq, and stays refused; a family whose `=` is a user operator is not the subtype's either.
fn subtype_equality(sub: Oid, opc: Oid) -> R<bool> {
    // D1604: a subtype whose own `=` is user code (a user btree opclass declared DEFAULT for it) has no form to decide it
    if user_equality(sub)? {
        return Ok(false);
    }
    if subtype_default_opclass(sub, opc)? {
        return Ok(true);
    }
    let (_, intype, eq) = own_eq(sub)?;
    let (opf, opcintype) = (lsyscache::get_opclass_family(opc)?, lsyscache::get_opclass_input_type(opc)?);
    if eq == 0 || opf == 0 || opcintype != intype {
        return Ok(false);
    }
    Ok(lsyscache::get_opfamily_member(opf, opcintype, opcintype, lsyscache::BTEqualStrategyNumber)? == eq)
}

/// K2 (D1889's hash join half): the form a range key buckets by where `form` refuses its subtype opclass (not the
/// subtype's default: D1889; a user one: D1604 at the range's own level): the range's flags and bounds under its subtype's
/// own form, as rangetypes.c hash_range hashes them (the subtype's default hash function, never the opclass's). Equal
/// images are hash-equal candidates, not equal ranges: range_eq (range_cmp_bounds through the opclass's comparison)
/// decides, so only a hash join that runs its clause on each candidate keys by it (lowerv4 ops/hashjoin/keys.rs). A domain
/// reads as its base. D2068: an array of such ranges buckets by the array of their bucket forms (array_eq runs range_eq
/// on each element pair, so the clause on each candidate calls the opclass's comparison as PG's bucket scan does).
/// `None` for every other type, a range `form` keeps, and one whose subtype or collation `form` refuses.
pub fn bucket_form(typid: Oid) -> R<Option<(u64, &'static Form)>> {
    Ok(bucket_build(typid, 0)?.map(intern))
}

fn bucket_build(typid: Oid, depth: u32) -> R<Option<Form>> {
    if depth > 16 {
        return Ok(None);
    }
    let Some(t) = syscache_seams::lookup_pg_type_typcache_shape::call(typid)? else { return Ok(None) };
    match t.typtype as u8 {
        b'd' => match syscache_seams::pg_type_base_shape::call(typid)? {
            Some(b) => bucket_build(b.typbasetype, depth + 1),
            None => Ok(None),
        },
        b'r' => {
            let Some(r) = syscache_seams::lookup_pg_range_shape::call(typid)? else { return Ok(None) };
            // K2.kgrp: a range whose declared subtype opclass names the subtype's `=` but is not the subtype's default one
            // (a user opclass included) keys by `form` in a group, a DISTINCT or a sort; a hash join keeps keying it by this
            // bucket form with range_eq on each candidate, as before (the probe's per-candidate calls are PG's)
            let core_default = subtype_default_opclass(r.rngsubtype, r.rngsubopc)? && keysem::builtin(r.rngsubopc);
            if (core_default && build(typid, 0)?.is_ok()) || user_equality(typid)? {
                return Ok(None);
            }
            if !coll_bytes(r.rngcollation)? {
                return Ok(None);
            }
            match build(r.rngsubtype, 1)? {
                Ok(f) => Ok(Some(Form { len: t.typlen, byval: t.typbyval, align: align_of(t.typalign), kind: Kind::Range(Box::new(f)) })), // alloc: Cold
                Err(_) => Ok(None),
            }
        }
        // D2068 (K2.arreq): an array whose element buckets (array_eq's element equality is the range's own `=`); an array
        // type compared by a user object of its own (D1604) has none
        b'b' if t.typsubscript == F_ARRAY_SUBSCRIPT_HANDLER && t.typlen == -1 && t.typelem != 0 => {
            if user_equality(typid)? {
                return Ok(None);
            }
            match bucket_build(t.typelem, depth + 1)? {
                Some(f) => Ok(Some(Form { len: t.typlen, byval: t.typbyval, align: align_of(t.typalign), kind: Kind::Array(Box::new(f)) })), // alloc: Cold
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// K2: whether hash clause operator `opno` over range type `typid` buckets by `bucket_form`: the range's own `=` (the
/// type cache's eq_opr: range_eq, whose first hash family's function is hash_range) over a range `bucket_form` keys, or
/// (D2068) array_eq over an array of such ranges.
pub fn bucket_eq(typid: Oid, opno: Oid) -> R<bool> {
    Ok(bucket_form(typid)?.is_some() && typcache::lookup_type_cache(typid, typcache::TYPECACHE_EQ_OPR)?.eq_opr() == opno)
}

/// D1604's Shape: a level whose default btree opclass, family, equality operator or comparison proc is a user object.
const USER_EQUALITY: &str = "a key type whose default btree opclass or equality is a user one (D1604)";

/// D1604: whether PostgreSQL compares `typid` by a user object: its default btree opclass (GetDefaultOpClass's pick; a
/// user opclass declared DEFAULT FOR TYPE varchar, cidr or one array, enum, range, multirange or composite type is its
/// exact pick over core's binary-coercible or polymorphic one), that opclass's family, or the type cache's equality
/// operator or comparison proc (get_opfamily_proc(btree_opf, btree_opintype, btree_opintype, BTORDER_PROC)). The
/// parser's grouping `=` and array_eq's / record_eq's elements and columns take the equality; range_cmp_bounds (a range
/// over the type takes the default as its subtype opclass: the D1889 check), array_cmp and record_cmp take the
/// comparison proc; the form keeps only the built-in equality. Each is read, not only the family: a user opclass
/// declared in core's family text_ops (FAMILY text_ops, FUNCTION 1 (varchar, varchar) only) leaves btree_opf core and
/// eq_opr 0 with a user comparison proc, and one whose FUNCTION 1 is core's bpcharcmp leaves the proc core too, so the
/// opclass itself is looked up: btree's one default opclass of btree_opintype (DefineOpClass allows one per input
/// type). A domain reads as its base type (GetDefaultOpClass does too). A crate rig without the type cache's seams
/// (lowerv4's catalog of the reference .dat files) has no user opclass. The family, input type and equality are
/// `own_eq`'s and the proc is the family's BTORDER_PROC, as the type cache derives them, read without refreshing its
/// eq_opr.
fn user_equality(typid: Oid) -> R<bool> {
    if !syscache_seams::syscache_hash_value_typeoid::is_installed() {
        return Ok(false);
    }
    let user = |o: Oid| o >= types_core::FirstNormalObjectId;
    let (opf, opcintype, eq) = own_eq(typid)?;
    let cmp = if opf == 0 { 0 } else { lsyscache::get_opfamily_proc(opf, opcintype, opcintype, BTORDER_PROC)? };
    if user(opf) || user(eq) || user(cmp) {
        return Ok(true);
    }
    if opf == 0 {
        return Ok(false);
    }
    let cx = crate::session::root_context("gravity_fn canon default opclass"); // alloc: Cold
    let rows = syscache_seams::lookup_pg_opclass_rows_by_am::call(cx.mcx(), types_core::BTREE_AM_OID)?;
    Ok(rows.iter().any(|&(opc, family, intype, default, _)| default && family == opf && intype == opcintype && user(opc)))
}

/// Type `typid`'s default btree family and input type (the type cache's btree_opf / btree_opintype) and its equality
/// operator as the type cache derives eq_opr (the family's BTEqualStrategyNumber member, else the default hash family's
/// HTEqualStrategyNumber member), read from the catalog: the type cache is asked for its btree (and, with no btree `=`,
/// hash) family only, never for TYPECACHE_EQ_OPR or TYPECACHE_CMP_PROC. The lowering must leave the type cache as
/// PostgreSQL's own calls leave it (K2.kgrp, e2e m6_multirange_keys): a catalog invalidation (an opclass DDL) clears
/// every entry's checked flags but keeps its old eq_opr, and the hash proc check (typcache.c: "the eq_opr, if one has
/// been determined, must match the hash opclass") reads that kept value. PostgreSQL never asks a range subtype's eq_opr
/// ("ranges can't be created in the first place unless their subtypes support equality"), so after a user DEFAULT
/// opclass for varchar is replaced, varchar keeps the old opclass's `=`, has no hash proc, and a range over varchar
/// groups sorted, by the range's subtype opclass. A lowering that refreshed varchar's eq_opr would give it texteq's
/// hash proc, and the same backend's next plan would hash that range by hashtext where the opclass's comparison calls
/// 'a' and 'A' one value.
fn own_eq(typid: Oid) -> R<(Oid, Oid, Oid)> {
    let e = typcache::lookup_type_cache(typid, typcache::TYPECACHE_BTREE_OPFAMILY)?;
    let (opf, opcintype) = (e.btree_opf(), e.btree_opintype());
    let mut eq = 0;
    if opf != 0 {
        eq = lsyscache::get_opfamily_member(opf, opcintype, opcintype, lsyscache::BTEqualStrategyNumber)?;
    }
    if eq == 0 {
        let h = typcache::lookup_type_cache(typid, typcache::TYPECACHE_HASH_OPFAMILY)?;
        if h.hash_opf() != 0 {
            eq = lsyscache::get_opfamily_member(h.hash_opf(), h.hash_opintype(), h.hash_opintype(), lsyscache::HTEqualStrategyNumber)?;
        }
    }
    Ok((opf, opcintype, eq))
}

fn align_of(c: i8) -> u8 {
    match c as u8 {
        b'c' => 1,
        b's' => 2,
        b'i' => 4,
        _ => 8,
    }
}

fn up(off: usize, a: u8) -> usize {
    (off + a as usize - 1) & !(a as usize - 1)
}

fn bad(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: a hosted key value that is not {what}"))) // alloc: Cold
}

/// The varlena at `v[off..]` (any uncompressed inline header): its payload's `(start, end)` within `v`. `walk` hands a
/// compressed one here only decompressed (D1607); an external one is an error (PostgreSQL flattens an external value
/// before it stores one in a container).
fn varlena_at(v: &[u8], off: usize) -> R<(usize, usize)> {
    let b0 = *v.get(off).ok_or_else(|| bad("a whole varlena"))?;
    let (start, end) = if b0 & 1 != 0 {
        if b0 == 1 {
            return Err(bad("an inline varlena (an external one)"));
        }
        (off + 1, off + (b0 >> 1) as usize)
    } else {
        if b0 & 2 != 0 {
            return Err(bad("an inline varlena (a compressed one)"));
        }
        let w = u32::from_le_bytes(v.get(off..off + 4).ok_or_else(|| bad("a whole varlena"))?.try_into().expect("4 bytes"));
        (off + 4, off + (w >> 2) as usize)
    };
    if end > v.len() || start > end {
        return Err(bad("a whole varlena"));
    }
    Ok((start, end))
}

/// The bytes a value of `f` takes at `v[off..]` (its alignment already applied), for a varlena its whole stored length
/// (a compressed one's too: `VARSIZE_4B`).
fn width(f: &Form, v: &[u8], off: usize) -> R<usize> {
    match f.len {
        -1 if compressed(v, off) => {
            let w = u32::from_le_bytes(v.get(off..off + 4).ok_or_else(|| bad("a whole varlena"))?.try_into().expect("4 bytes"));
            let n = (w >> 2) as usize;
            if n < 8 || off + n > v.len() {
                return Err(bad("a whole varlena"));
            }
            Ok(n)
        }
        -1 => Ok(varlena_at(v, off)?.1 - off),
        -2 => Err(bad("a cstring")),
        n => Ok(n as usize),
    }
}

/// Whether the varlena at `v[off..]` is compressed inline (`VARATT_IS_4B_C`: a 4-byte header whose low bits are 10).
fn compressed(v: &[u8], off: usize) -> bool {
    v.get(off).is_some_and(|&b| b & 0x03 == 0x02)
}

/// D1607: the plain form (a 4-byte header, then the payload) of the compressed inline varlena `raw` inside a container.
/// heap_form_tuple keeps a column's compressed image in a composite datum (a composite key's column, a range bound or a
/// multirange's bound of a composite subtype), and record_eq / record_cmp detoast each column before they compare it, so
/// the form is the decompressed value's. Decompressing reads no catalog (a pool worker runs it, as `with_payload` does
/// a compressed key at the top level).
fn decompressed(raw: &[u8]) -> R<Tmp> {
    let mut out = Err(None);
    crate::with_scratch(|mcx| {
        out = match detoast::detoast_attr(mcx, raw) {
            Ok(flat) => Ok(Tmp::of(flat.len(), &[&flat[..]])),
            Err(e) => Err(Some(e)),
        };
        crate::ok(0)
    });
    out.map_err(|e| e.expect("the detoast's error"))
}

/// Per-row temporaries of the walk (a container's flat form, a decompressed value, a rebuilt header): pooled per thread
/// and given back at the drop, so a steady-state walk allocates nothing (capacities are retained; a buffer past
/// `TMP_KEEP` bytes is freed instead, and at most `TMP_POOL` are kept: 64 KiB per thread at most). Derefs to the `Vec<u8>`.
struct Tmp(Vec<u8>);

/// Worst-case retained per thread: `TMP_POOL` x `TMP_KEEP` = 4 x 16 KiB = 64 KiB (a walk nests one buffer per container level,
/// most keys one or two). A holder beside `CANON`; memhold's decision for typesem's per-thread scratch charges both.
const TMP_KEEP: usize = 16 << 10;
const TMP_POOL: usize = 4;

thread_local! {
    /// The pooled buffers: fixed slots (no growth of the pool itself).
    static TMPS: core::cell::RefCell<[Vec<u8>; TMP_POOL]> = const { core::cell::RefCell::new([Vec::new(), Vec::new(), Vec::new(), Vec::new()]) }; // alloc: Cold
}

impl Tmp {
    /// A buffer holding the concatenation of `parts`, with room for `n` bytes (a pooled one when the thread has one).
    fn of(n: usize, parts: &[&[u8]]) -> Tmp {
        // the pooled buffer with the most room (an empty slot is a Vec with no capacity)
        let mut v = TMPS.with(|p| {
            let mut p = p.borrow_mut();
            let best = (0..TMP_POOL).max_by_key(|&i| p[i].capacity()).unwrap_or(0);
            core::mem::take(&mut p[best])
        });
        v.clear();
        if v.capacity() < n {
            v = Vec::with_capacity(n); // alloc: Cold
        }
        for part in parts {
            v.extend_from_slice(part); // alloc: Cold
        }
        Tmp(v)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let v = core::mem::take(&mut self.0);
        if v.capacity() == 0 || v.capacity() > TMP_KEEP {
            return;
        }
        TMPS.with(|p| {
            let mut p = p.borrow_mut();
            // an empty slot, else the smallest one this buffer beats
            let slot = (0..TMP_POOL).find(|&i| p[i].capacity() == 0).or_else(|| (0..TMP_POOL).min_by_key(|&i| p[i].capacity()).filter(|&i| p[i].capacity() < v.capacity()));
            if let Some(i) = slot {
                p[i] = v;
            }
        });
    }
}

impl core::ops::Deref for Tmp {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl core::ops::DerefMut for Tmp {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

/// The offset a value of `f` starts at from `off` in a tuple or a range (`att_align_pointer`: a varlena's non-zero first
/// byte is a short header, stored unaligned).
fn align_ptr(f: &Form, v: &[u8], off: usize) -> usize {
    if f.len == -1 && v.get(off).is_some_and(|&b| b != 0) {
        off
    } else {
        up(off, f.align)
    }
}

/// Appends the canonical form of the value of `f` whose bytes start at `v[off]` (its stored bytes; a by-value one its
/// `len` bytes as stored in a container, a top-level one its bytes at the key's cell) to `out`.
fn walk(f: &Form, v: &[u8], off: usize, out: &mut Vec<u8>) -> R<()> {
    let w = width(f, v, off)?;
    let raw = v.get(off..off + w).ok_or_else(|| bad("a whole value"))?;
    if f.len == -1 && compressed(raw, 0) {
        return walk(f, &decompressed(raw)?, 0, out); // D1607
    }
    match &f.kind {
        Kind::Leaf(l) => leaf(*l, raw, out),
        Kind::Array(e) => array(e, &flat(raw)?, out),
        Kind::Record(cols) => record(cols, &flat(raw)?, out),
        Kind::Range(sub) => range(sub, &flat(raw)?, out),
        Kind::Multirange(sub) => multirange(sub, &flat(raw)?, out),
    }
}

/// The container varlena `raw` (any uncompressed inline header) behind a 4-byte header: its layout's offsets (an array's
/// dataoffset, a tuple's t_hoff, a range's bounds) count from that header, and a stored short header drops it.
fn flat(raw: &[u8]) -> R<Tmp> {
    let (s, e) = varlena_at(raw, 0)?;
    Ok(Tmp::of(e - s + 4, &[&(((e - s + 4) as u32) << 2).to_le_bytes(), &raw[s..e]]))
}

fn leaf(l: Leaf, raw: &[u8], out: &mut Vec<u8>) -> R<()> {
    match l {
        Leaf::Bytes => out.extend_from_slice(raw),
        Leaf::Float4 => {
            let x = f32::from_le_bytes(raw.try_into().map_err(|_| bad("a float4"))?);
            let bits = if x.is_nan() { 0x7fc0_0000 } else if x == 0.0 { 0 } else { x.to_bits() };
            out.extend_from_slice(&bits.to_le_bytes());
        }
        Leaf::Float8 => {
            let x = f64::from_le_bytes(raw.try_into().map_err(|_| bad("a float8"))?);
            let bits = if x.is_nan() { 0x7ff8_0000_0000_0000 } else if x == 0.0 { 0 } else { x.to_bits() };
            out.extend_from_slice(&bits.to_le_bytes());
        }
        Leaf::Name => {
            let n = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            out.extend_from_slice(&raw[..n]);
        }
        Leaf::Interval => {
            // interval_cmp_value: time + (month * 30 + day) * USECS_PER_DAY, 128 bits
            let r: &[u8; 16] = raw.try_into().map_err(|_| bad("an interval"))?;
            let time = i64::from_le_bytes(r[0..8].try_into().expect("8 bytes")) as i128;
            let day = i32::from_le_bytes(r[8..12].try_into().expect("4 bytes")) as i128;
            let month = i32::from_le_bytes(r[12..16].try_into().expect("4 bytes")) as i128;
            out.extend_from_slice(&(time + (month * 30 + day) * 86_400_000_000).to_le_bytes());
        }
        Leaf::Numeric => {
            let (s, e) = varlena_at(raw, 0)?;
            numeric(&raw[s..e], out)?;
        }
        Leaf::Jsonb => {
            let (s, e) = varlena_at(raw, 0)?;
            jsonb(&raw[s..e], 0, out, 0)?;
        }
        Leaf::Varlena | Leaf::Bpchar => {
            let (s, e) = varlena_at(raw, 0)?;
            let mut p = &raw[s..e];
            if l == Leaf::Bpchar {
                p = &p[..p.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)];
            }
            out.extend_from_slice(p);
        }
        Leaf::OidVector => oidvector(&flat(raw)?, out)?,
        Leaf::Coll { coll, bpchar } => {
            let (s, e) = varlena_at(raw, 0)?;
            let mut p = &raw[s..e];
            if bpchar {
                p = &p[..p.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)];
            }
            let at = out.len();
            let mut cap = 2 * p.len() + 16;
            loop {
                out.resize(at + cap, 0);
                match crate::typesem::collation::sort_key(coll, p, &mut out[at..])? {
                    Some(n) if n < cap => {
                        out.truncate(at + n);
                        break;
                    }
                    Some(n) => cap = n + 1,
                    None => return Err(bad("a collation sort key the collation cannot produce")),
                }
            }
        }
    }
    Ok(())
}

const JB_CMASK: u32 = 0x0FFF_FFFF;
const JB_FSCALAR: u32 = 0x1000_0000;
const JB_FOBJECT: u32 = 0x2000_0000;
const JENTRY_OFFLENMASK: u32 = 0x0FFF_FFFF;
const JENTRY_TYPEMASK: u32 = 0x7000_0000;
const JENTRY_HAS_OFF: u32 = 0x8000_0000;

/// The jsonb container at `c[at..]` (a `JsonbContainer`: its header, its entries, then its children's data, offsets
/// counted from the data's start) in canonical form: `S` (a raw scalar), `A` (an array) or `O` (an object), its count,
/// then each child in stored order (an object's keys then values, pairwise: `uniqueifyJsonbObject` sorted and deduped
/// them, so two equal objects store the same order). A child: `s` + length + bytes (a string: jsonb equality is its
/// bytes), `n` + length + `hash_numeric`'s normalized digits (a number: equal by `numeric_cmp`), `t` / `f` / `z`, or a
/// nested container.
fn jsonb(c: &[u8], at: usize, out: &mut Vec<u8>, depth: u32) -> R<()> {
    if depth > 256 {
        return Err(bad("a jsonb nested within 256 levels"));
    }
    let word = |i: usize| -> R<u32> { Ok(u32::from_le_bytes(c.get(i..i + 4).ok_or_else(|| bad("a whole jsonb"))?.try_into().expect("4 bytes"))) };
    let head = word(at)?;
    let count = (head & JB_CMASK) as usize;
    let object = head & JB_FOBJECT != 0;
    let n = if object { 2 * count } else { count };
    out.push(if object { b'O' } else if head & JB_FSCALAR != 0 { b'S' } else { b'A' });
    out.extend_from_slice(&(count as u32).to_le_bytes());
    let entries = at + 4;
    let base = entries + 4 * n;
    let mut off = 0usize;
    for i in 0..n {
        let e = word(entries + 4 * i)?;
        let field = (e & JENTRY_OFFLENMASK) as usize;
        let (start, len) = if e & JENTRY_HAS_OFF != 0 { (off, field.checked_sub(off).ok_or_else(|| bad("a jsonb with ordered offsets"))?) } else { (off, field) };
        off = start + len;
        let data = |a: usize, l: usize| c.get(base + a..base + a + l).ok_or_else(|| bad("a whole jsonb"));
        match e & JENTRY_TYPEMASK {
            0x0000_0000 => {
                out.push(b's');
                out.extend_from_slice(&(len as u32).to_le_bytes());
                out.extend_from_slice(data(start, len)?);
            }
            0x1000_0000 => {
                let a = up(start, 4);
                let v = data(a, start + len - a)?;
                let (s, e) = varlena_at(v, 0)?;
                out.push(b'n');
                let at = out.len();
                out.extend_from_slice(&[0; 4]);
                numeric(&v[s..e], out)?;
                let l = (out.len() - at - 4) as u32;
                out[at..at + 4].copy_from_slice(&l.to_le_bytes());
            }
            0x2000_0000 => out.push(b'f'),
            0x3000_0000 => out.push(b't'),
            0x4000_0000 => out.push(b'z'),
            0x5000_0000 => {
                let a = up(start, 4);
                data(a, start + len - a)?;
                jsonb(c, base + a, out, depth + 1)?;
            }
            _ => return Err(bad("a jsonb entry of a known type")),
        }
    }
    Ok(())
}

/// A container's element or column: its form behind its length, so the concatenation of a container's parts is
/// unambiguous (a leaf's own form carries no length: at the top it is exactly the image rep.varlena.hash takes of the same
/// value, the payload's or `m6_num_fp`'s).
fn part(f: &Form, v: &[u8], off: usize, out: &mut Vec<u8>) -> R<()> {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    walk(f, v, off, out)?;
    let n = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&n.to_le_bytes());
    Ok(())
}

/// A numeric payload's canonical image (`hash_numeric`'s: the digits without leading or trailing zeros and the weight,
/// the sign; one image for every zero, NaN, +Inf and -Inf each their own): a class byte, then for a finite nonzero value
/// its sign, the weight of its first nonzero digit (4 bytes) and the digits: `m6_num_fp`'s image bytes (kernels m6.rs), so a
/// numeric key hashes alike through either arm.
fn numeric(p: &[u8], out: &mut Vec<u8>) -> R<()> {
    let h = u16::from_le_bytes(p.get(0..2).ok_or_else(|| bad("a numeric"))?.try_into().expect("2 bytes"));
    let (neg, weight, digits) = match h & 0xC000 {
        0xC000 => {
            out.push(match h & 0xF000 {
                0xC000 => 2,
                0xD000 => 3,
                0xF000 => 4,
                _ => return Err(bad("a numeric")),
            });
            return Ok(());
        }
        0x8000 => (h & 0x2000 != 0, (h & 0x3F) as i16 | if h & 0x40 != 0 { !0x3F } else { 0 }, &p[2..]),
        s => (s == 0x4000, i16::from_le_bytes(p.get(2..4).ok_or_else(|| bad("a numeric"))?.try_into().expect("2 bytes")), p.get(4..).unwrap_or(&[])),
    };
    let n = digits.len() / 2;
    let digit = |i: usize| u16::from_le_bytes([digits[2 * i], digits[2 * i + 1]]);
    let Some(first) = (0..n).find(|&i| digit(i) != 0) else {
        out.push(0);
        return Ok(());
    };
    let last = (0..n).rfind(|&i| digit(i) != 0).expect("a nonzero digit");
    let body = &digits[2 * first..2 * last + 2];
    out.push(1);
    out.push(neg as u8);
    out.extend_from_slice(&(weight as i32 - first as i32).to_le_bytes());
    out.extend_from_slice(body);
    Ok(())
}

/// D1326: the group key word of an integral `extract(field FROM ts)` (decompose agg.rs `integral_extract`: keyed as the
/// field's int4) from its numeric result's payload `p`. A finite value is its integer: int4 holds it, else PG's
/// `numeric_int4` error ("integer out of range"). -Infinity / +Infinity (a monotonic field of an infinite timestamp,
/// PG >= 14: its own group, not an error) are the packed word's sentinels `MANT_NINF` / `MANT_PINF` (agg_final.rs), so
/// the emit renders them back through `num_word`. Both lie outside int4, so neither meets a finite word, and their
/// signed order is numeric's. A NaN or a fraction is no integral field's result, so it is an error.
pub fn integral_word(p: &[u8]) -> R<i64> {
    let h = u16::from_le_bytes(p.get(0..2).ok_or_else(|| bad("a numeric"))?.try_into().expect("2 bytes"));
    match h & 0xF000 {
        0xD000 => return Ok(crate::agg_final::MANT_PINF),
        0xF000 => return Ok(crate::agg_final::MANT_NINF),
        0xC000 => return Err(bad("an integral extract's result (a NaN)")),
        _ => {}
    }
    let mut img = Tmp::of(p.len() + 8, &[]);
    numeric(p, &mut img)?;
    if img[0] == 1 {
        // the weight of the first nonzero digit less the digits after it: the last nonzero digit's weight
        let first = i32::from_le_bytes(img[2..6].try_into().expect("4 bytes"));
        if first - ((img.len() - 6) / 2) as i32 + 1 < 0 {
            return Err(bad("an integral extract's result (a fraction)"));
        }
    }
    // the value from the image's digits (base 10000, most significant first; no fraction left): never through
    // adt_numeric's `Num`, which needs a 2-byte aligned payload (a short-header datum's is not)
    if img[0] == 0 {
        return Ok(0);
    }
    let (neg, first) = (img[1] != 0, i32::from_le_bytes(img[2..6].try_into().expect("4 bytes")));
    let mut v: i128 = 0;
    for (k, d) in img[6..].chunks_exact(2).enumerate() {
        let d = u16::from_le_bytes([d[0], d[1]]) as i128;
        v = v * 10_000 + d;
        if v > i32::MAX as i128 + 1 || k > 8 {
            return Err(int4_out_of_range());
        }
    }
    let n = (img.len() - 6) / 2;
    for _ in 0..(first + 1 - n as i32) {
        v *= 10_000;
        if v > i32::MAX as i128 + 1 {
            return Err(int4_out_of_range());
        }
    }
    let v = if neg { -v } else { v };
    i32::try_from(v).map(i64::from).map_err(|_| int4_out_of_range())
}

/// PG's `numeric_int4` past int4: "integer out of range" (22003).
#[cold]
fn int4_out_of_range() -> Box<PgError> {
    Box::new(PgError::error("integer out of range".to_string()).with_sqlstate(types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE)) // alloc: Cold
}

/// An array's form from its varlena `a` (behind a 4-byte header): ndim, dims and lower bounds, then per element its null
/// flag and form.
fn array(e: &Form, a: &[u8], out: &mut Vec<u8>) -> R<()> {
    let s = 4;
    let i32_at = |o: usize| a.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes"))).ok_or_else(|| bad("an array"));
    let (ndim, dataoffset) = (i32_at(s)?, i32_at(s + 4)?);
    if !(0..=6).contains(&ndim) {
        return Err(bad("an array"));
    }
    let nd = ndim as usize;
    let (dims_at, lbs_at) = (s + 12, s + 12 + 4 * nd);
    let mut count: usize = if nd == 0 { 0 } else { 1 };
    out.extend_from_slice(&ndim.to_le_bytes());
    for d in 0..nd {
        let (dim, lb) = (i32_at(dims_at + 4 * d)?, i32_at(lbs_at + 4 * d)?);
        count = count.checked_mul(dim.max(0) as usize).ok_or_else(|| bad("an array"))?;
        out.extend_from_slice(&dim.to_le_bytes());
        out.extend_from_slice(&lb.to_le_bytes());
    }
    let bitmap = (dataoffset != 0).then_some(lbs_at + 4 * nd);
    let mut off = if dataoffset != 0 { dataoffset as usize } else { up(lbs_at + 4 * nd, 8) };
    for i in 0..count {
        let null = match bitmap {
            Some(b) => a.get(b + i / 8).ok_or_else(|| bad("an array"))? >> (i % 8) & 1 == 0,
            None => false,
        };
        out.push(null as u8);
        if null {
            continue;
        }
        off = up(off, e.align);
        part(e, a, off, out)?;
        off += width(e, a, off)?;
    }
    Ok(())
}

/// D3786: the form of the oidvector `a` (`flat`'s layout: an ArrayType behind a 4-byte header) under oidvectoreq /
/// hashoidvector (oid.c): its element count dim1, then its values. btoidvectorcmp compares dim1 and the values and
/// hashoidvector hashes the values, neither reads the lower bound: a stored oidvector's is 0 (buildoidvector,
/// oidvectorin) and an array constructor cast to oidvector (`ARRAY[casttarget]::oidvector`) keeps 1, and PostgreSQL
/// calls the two equal. A datum that is not a valid oidvector raises check_valid_oidvector's error (42804 'array is not
/// a valid oidvector'), as PostgreSQL's hash and comparison of it do.
fn oidvector(a: &[u8], out: &mut Vec<u8>) -> R<()> {
    let i32_at = |o: usize| a.get(o..o + 4).map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes"))).ok_or_else(|| bad("an oidvector"));
    let (ndim, dataoffset, elemtype) = (i32_at(4)?, i32_at(8)?, i32_at(12)? as u32);
    if ndim != 1 || dataoffset != 0 || elemtype != types_core::catalog::OIDOID {
        return Err(nbt_compare::not_valid_oidvector());
    }
    let dim1 = i32_at(16)?;
    // the values follow the one dimension and lower bound (ARR_OVERHEAD_NONULLS(1) = MAXALIGN(16 + 8) = 24)
    let values = usize::try_from(dim1).ok().and_then(|n| n.checked_mul(4)).and_then(|l| a.get(24..24 + l)).ok_or_else(nbt_compare::not_valid_oidvector)?;
    out.extend_from_slice(&dim1.to_le_bytes());
    out.extend_from_slice(values);
    Ok(())
}

/// A composite datum's form from its HeapTupleHeader `t` (the length word first): per column a null flag and form.
fn record(cols: &[Form], t: &[u8], out: &mut Vec<u8>) -> R<()> {
    let (Some(&hoff), Some(m2), Some(m)) = (t.get(22), t.get(18..20), t.get(20..22)) else { return Err(bad("a composite datum")) };
    let natts = (u16::from_le_bytes(m2.try_into().expect("2 bytes")) & 0x07FF) as usize;
    let hasnull = u16::from_le_bytes(m.try_into().expect("2 bytes")) & 0x0001 != 0;
    let mut off = hoff as usize;
    for (i, c) in cols.iter().enumerate() {
        let null = i >= natts || (hasnull && t.get(23 + i / 8).ok_or_else(|| bad("a composite datum"))? >> (i % 8) & 1 == 0);
        out.push(null as u8);
        if null {
            continue;
        }
        off = align_ptr(c, t, off);
        part(c, t, off, out)?;
        off += width(c, t, off)?;
    }
    Ok(())
}

/// A range's form from its varlena `r` (header first): the flags (empty, inclusivity, infinity), then each finite bound.
fn range(sub: &Form, r: &[u8], out: &mut Vec<u8>) -> R<()> {
    let flags = *r.last().ok_or_else(|| bad("a range"))?;
    bounds(sub, &r[..r.len() - 1], 8, flags, out)
}

/// A range's flags (empty, inclusivity, infinity) and its finite bounds, the lower one at or after `body[off..]` (each
/// bound `att_align_pointer`ed within `body`, as `range_deserialize` and `multirange_get_bounds` read them).
fn bounds(sub: &Form, body: &[u8], mut off: usize, flags: u8, out: &mut Vec<u8>) -> R<()> {
    let flags = flags & 0x1F;
    if flags & 0x01 != 0 {
        out.push(0x01);
        return Ok(());
    }
    out.push(flags);
    for inf in [0x08, 0x10] {
        if flags & inf != 0 {
            continue;
        }
        off = align_ptr(sub, body, off);
        part(sub, body, off, out)?;
        off += width(sub, body, off)?;
    }
    Ok(())
}

/// `sizeof(MultirangeType)`: the varlena header, the multirange type's oid, the range count.
const MULTIRANGE_HEADER: usize = 12;
/// `MULTIRANGE_ITEM_OFF_BIT`: an item holding the range's offset from the bounds' start, not the length before it.
const MULTIRANGE_ITEM_OFF_BIT: u32 = 0x8000_0000;

/// A multirange's form from its varlena `m` (header first; `MultirangeType`): the range count, then each range in stored
/// order as its form behind its length (`multirange_eq`: equal counts and pairwise equal ranges). The layout is
/// `write_multirange_data`'s: `count - 1` items (every `MULTIRANGE_ITEM_OFFSET_STRIDE`th an offset flagged
/// `MULTIRANGE_ITEM_OFF_BIT`, the others the previous range's length), `count` flag bytes, then the ranges' bounds from
/// `att_align_nominal` of that under the subtype's alignment (`MultirangeGetBoundariesPtr`); range `i` starts at the
/// lengths summed back to the nearest offset (`multirange_get_bounds_offset`).
fn multirange(sub: &Form, m: &[u8], out: &mut Vec<u8>) -> R<()> {
    let word = |o: usize| m.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes"))).ok_or_else(|| bad("a multirange"));
    let n = word(8)? as usize;
    out.extend_from_slice(&(n as u32).to_le_bytes());
    if n == 0 {
        return Ok(());
    }
    let flags_at = MULTIRANGE_HEADER + 4 * (n - 1);
    if flags_at + n > m.len() {
        return Err(bad("a multirange"));
    }
    let begin = up(flags_at + n, sub.align);
    for i in 0..n {
        let mut offset = 0usize;
        let mut k = i;
        while k > 0 {
            let item = word(MULTIRANGE_HEADER + 4 * (k - 1))?;
            offset += (item & !MULTIRANGE_ITEM_OFF_BIT) as usize;
            if item & MULTIRANGE_ITEM_OFF_BIT != 0 {
                break;
            }
            k -= 1;
        }
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        bounds(sub, m, begin + offset, m[flags_at + i], out)?;
        let l = (out.len() - at - 4) as u32;
        out[at..at + 4].copy_from_slice(&l.to_le_bytes());
    }
    Ok(())
}

/// The canonical form of the key value at `datum` (a varlena of any header form, detoasted when compressed or external,
/// or a fixed by-reference value's bytes) under form `f`, appended to `out`.
/// # Safety — `datum` is a live value of `f`'s type.
pub unsafe fn canonical(f: &Form, datum: *const u8, out: &mut Vec<u8>) -> R<()> {
    if f.len > 0 {
        let raw = core::slice::from_raw_parts(datum, f.len as usize);
        return walk(f, raw, 0, out);
    }
    if f.len != -1 {
        return Err(bad("a varlena or fixed-width value"));
    }
    crate::with_payload(datum, |p| {
        // rebuild a 4-byte header in front of the payload so the walk reads one layout
        let v = Tmp::of(p.len() + 4, &[&(((p.len() + 4) as u32) << 2).to_le_bytes(), p]);
        walk(f, &v, 0, out)
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leafform(l: Leaf, len: i16) -> Form {
        Form { len, byval: len > 0 && len <= 8, align: if len == -1 { 4 } else { len.clamp(1, 8) as u8 }, kind: Kind::Leaf(l) }
    }

    fn v4(p: &[u8]) -> Vec<u8> {
        let mut v = (((p.len() + 4) as u32) << 2).to_le_bytes().to_vec();
        v.extend_from_slice(p);
        v
    }

    fn canon(f: &Form, v: &[u8]) -> Vec<u8> {
        let mut out = Vec::new(); // alloc: Cold
        walk(f, v, 0, &mut out).expect("canonical");
        out
    }

    #[test]
    fn built_in_collations_compare_bytes_without_a_catalog() {
        use crate::typesem::collation::{C_COLLATION_OID, DEFAULT_COLLATION_OID, POSIX_COLLATION_OID};
        for c in [0, DEFAULT_COLLATION_OID, C_COLLATION_OID, POSIX_COLLATION_OID] {
            assert!(coll_bytes(c).unwrap() && claims_coll_bytes(c), "{c}");
        }
    }

    #[test]
    fn order_image_names_signed_unsigned_and_byte_orders() {
        use types_core::catalog::*;
        for t in [INT2OID, INT4OID, INT8OID, DATEOID, TIMEOID, TIMESTAMPOID, TIMESTAMPTZOID, CASHOID] {
            assert_eq!(order_image(t), Some(OrderBy::Signed), "{t}");
        }
        assert_eq!(order_image(OIDOID), Some(OrderBy::Unsigned));
        assert_eq!(order_image(BYTEAOID), Some(OrderBy::Bytes));
        assert_eq!(word_order(TEXTOID), None);
        for t in [NUMERICOID, FLOAT8OID, INTERVALOID, BPCHAROID, UUIDOID, TIMETZOID] {
            assert_eq!(order_image(t), None, "{t}");
        }
    }

    #[test]
    fn int_word_is_exactly_int2_int4_int8() {
        use types_core::catalog::*;
        for t in 0..20_000 {
            assert_eq!(int_word(t), [INT2OID, INT4OID, INT8OID].contains(&t), "{t}");
        }
    }

    #[test]
    fn text_payload_is_text_and_varchar_and_character_types_add_bpchar() {
        use types_core::catalog::*;
        for t in [TEXTOID, VARCHAROID] {
            assert!(text_payload(t) && character_type(t), "{t}");
        }
        assert!(!text_payload(BPCHAROID) && character_type(BPCHAROID));
        for t in [NAMEOID, BYTEAOID, CHAROID, JSONOID, INT4OID, UNKNOWNOID] {
            assert!(!text_payload(t) && !character_type(t), "{t}");
        }
    }

    #[test]
    fn one_image_answers_inet_and_enum_not_interval() {
        use types_core::catalog::*;
        for t in [INETOID, CIDROID, ANYENUMOID, TIMETZOID, TEXTOID, BYTEAOID, INT4OID] {
            assert!(one_image(t), "{t}");
        }
        for t in [INTERVALOID, NUMERICOID, FLOAT8OID, BPCHAROID, ANYARRAYOID, RECORDOID] {
            assert!(!one_image(t), "{t}");
        }
    }

    #[test]
    fn floats_fold_minus_zero_and_nan() {
        let f = leafform(Leaf::Float8, 8);
        assert_eq!(canon(&f, &(-0.0f64).to_le_bytes()), canon(&f, &0.0f64.to_le_bytes()));
        assert_eq!(canon(&f, &f64::NAN.to_le_bytes()), canon(&f, &(-f64::NAN).to_le_bytes()));
        assert_ne!(canon(&f, &1.0f64.to_le_bytes()), canon(&f, &0.0f64.to_le_bytes()));
    }

    #[test]
    fn numeric_scales_are_one_value() {
        let f = leafform(Leaf::Numeric, -1);
        // short form: 1 (weight 0, digit 1) and 1.0000 (digits 1, 0)
        let one = v4(&[0x00, 0x80, 1, 0]);
        let one_scaled = v4(&[0x00, 0x82, 1, 0, 0, 0]); // dscale 4 (0x1F80 bits): 1.0000
        let two = v4(&[0x00, 0x80, 2, 0]);
        assert_eq!(canon(&f, &one), canon(&f, &one_scaled));
        assert_ne!(canon(&f, &one), canon(&f, &two));
    }

    /// A jsonb image built by hand (PostgreSQL's layout: the container header, JEntries, the data; numbers and containers
    /// INTALIGNed from the data's start).
    fn jb(head: u32, entries: &[u32], data: &[u8]) -> Vec<u8> {
        let mut p = head.to_le_bytes().to_vec();
        entries.iter().for_each(|e| p.extend_from_slice(&e.to_le_bytes()));
        p.extend_from_slice(data);
        v4(&p)
    }

    #[test]
    fn jsonb_numbers_compare_by_value_and_the_raw_scalar_flag_counts() {
        let f = leafform(Leaf::Jsonb, -1);
        // {"a": 1} and {"a": 1.0}: key "a" (string, len 1), value numeric at INTALIGN(1) = 4
        let one = [0x00u8, 0x80, 1, 0];
        let one_scaled = [0x00u8, 0x81, 1, 0, 0, 0];
        let obj = |num: &[u8]| {
            let nv = v4(num);
            let mut d = b"a".to_vec();
            d.extend_from_slice(&[0, 0, 0]);
            d.extend_from_slice(&nv);
            jb(0x2000_0001, &[1, 0x1000_0000 | (3 + nv.len() as u32)], &d)
        };
        assert_eq!(canon(&f, &obj(&one)), canon(&f, &obj(&one_scaled)), "1 and 1.0 are one jsonb value");
        assert_ne!(canon(&f, &obj(&one)), canon(&f, &obj(&[0x00, 0x80, 2, 0])));
        // the raw scalar "x" and the array ["x"]
        let raw = jb(0x5000_0001, &[1], b"x");
        let arr = jb(0x4000_0001, &[1], b"x");
        assert_ne!(canon(&f, &raw), canon(&f, &arr));
        // true, false, null
        let t = jb(0x5000_0001, &[0x3000_0000], b"");
        let n = jb(0x5000_0001, &[0x4000_0000], b"");
        assert_ne!(canon(&f, &t), canon(&f, &n));
        // a nested container: [[1]] (the inner array INTALIGNed at 0)
        let inner = { let nv = v4(&one); let mut p = 0x4000_0001u32.to_le_bytes().to_vec(); p.extend_from_slice(&(0x1000_0000u32 | nv.len() as u32).to_le_bytes()); p.extend_from_slice(&nv); p };
        let outer = jb(0x4000_0001, &[0x5000_0000 | inner.len() as u32], &inner);
        let got = canon(&f, &outer);
        assert_eq!(&got[..5], &[b'A', 1, 0, 0, 0]);
        assert_eq!(got[5], b'A');
    }

    /// An anonymous record's form is the composite form over its columns' forms in `record`'s storage: a ROW(int4, text)
    /// keys as a named composite (a int4, b text) does (`record_eq` compares both column by column), its own columns'
    /// order and count are its identity, and a NULL column images apart from a present one.
    #[test]
    fn an_anonymous_records_form_is_the_composite_form_of_its_columns() {
        let named = Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(Leaf::Bytes, 4), leafform(Leaf::Varlena, -1)]) }; // alloc: Cold
        let anon = record_of(vec![leafform(Leaf::Bytes, 4), leafform(Leaf::Varlena, -1)]); // alloc: Cold
        assert_eq!(id_of(&anon), id_of(&named));
        assert_ne!(id_of(&anon), id_of(&record_of(vec![leafform(Leaf::Varlena, -1), leafform(Leaf::Bytes, 4)]))); // alloc: Cold
        assert_ne!(id_of(&anon), id_of(&record_of(vec![leafform(Leaf::Bytes, 4)]))); // alloc: Cold
        // a heap tuple datum (HeapTupleHeader: t_infomask2 at 18, t_infomask at 20, t_hoff at 22, the null bitmap at 23)
        let tuple = |a: Option<i32>, b: &[u8]| {
            let hoff = 24u8;
            let mut t = vec![0u8; hoff as usize]; // alloc: Cold
            t[18..20].copy_from_slice(&2u16.to_le_bytes());
            t[20..22].copy_from_slice(&(a.is_none() as u16).to_le_bytes());
            t[22] = hoff;
            t[23] = if a.is_none() { 0b10 } else { 0b11 };
            if let Some(a) = a {
                t.extend_from_slice(&a.to_le_bytes());
            }
            t.push(((b.len() + 1) << 1 | 1) as u8); // a short varlena header
            t.extend_from_slice(b);
            v4(&t[4..])
        };
        assert_eq!(canon(&anon, &tuple(Some(7), b"ab")), canon(&anon, &tuple(Some(7), b"ab")));
        assert_ne!(canon(&anon, &tuple(Some(7), b"ab")), canon(&anon, &tuple(Some(7), b"ac")));
        assert_ne!(canon(&anon, &tuple(None, b"ab")), canon(&anon, &tuple(Some(0), b"ab")));
    }

    #[test]
    fn an_interned_form_is_one_copy_per_distinct_form_and_resolves_from_any_thread() {
        let (ia, a) = intern(Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(Leaf::Bytes, 4)]) }); // alloc: Cold
        let (ib, b) = intern(Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(Leaf::Bytes, 4)]) }); // alloc: Cold
        assert!(core::ptr::eq(a, b) && ia == ib, "one copy and one id per distinct form");
        // ALTER TYPE .. ADD ATTRIBUTE: a new form is a new id; the old one stays live for the programs holding it
        let (ic, _) = intern(Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(Leaf::Bytes, 4), leafform(Leaf::Varlena, -1)]) }); // alloc: Cold
        assert_ne!(ia, ic);
        assert_eq!(by_id(ia).unwrap().kind, Kind::Record(vec![leafform(Leaf::Bytes, 4)])); // alloc: Cold
        // a pool worker's thread resolves it with no catalog; an id no lowering built is an error, not a catalog read
        let got = std::thread::spawn(move || (by_id(ia).map(|f| f as *const Form as usize).ok(), by_id(0x5eed).is_err())).join().unwrap();
        assert_eq!(got, (Some(a as *const Form as usize), true));
        // the id is content-derived: the same form, the same constant in every process (IR pins stay exact)
        assert_eq!(id_of(&leafform(Leaf::Numeric, -1)), id_of(&leafform(Leaf::Numeric, -1)));
    }

    /// e2e m6_hosted_canon_ddl's DDL steps as forms (M6-HOSTED hh, main 09-28): each DDL that changes what `=` means is a
    /// new content id; a type re-created with the same definition (a new OID) is the same id, the same interned copy and
    /// no new entry; and `by_id`'s thread-local LAST, a memo of the immutable id -> form mapping, returns each id's own
    /// form however the ids alternate (never the previous one's).
    #[test]
    fn each_canon_ddl_step_is_a_new_id_and_last_never_serves_another_ids_form() {
        let rec = |a: Leaf, alen: i16| Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(a, alen), leafform(Leaf::Varlena, -1)]) }; // alloc: Cold
        // DROP + CREATE TYPE m6cd: (a int, b text) -> (a numeric, b text)
        let (int_id, int_f) = intern(rec(Leaf::Bytes, 4));
        let (num_id, num_f) = intern(rec(Leaf::Numeric, -1));
        assert_ne!(int_id, num_id);
        // DROP + CREATE DOMAIN m6cdd: over jsonb -> over text (a domain's form is its base's); the key function's
        // DROP + CREATE (RETURNS jsonb -> RETURNS text) is the same pair of forms
        let (jb_id, jb_f) = intern(leafform(Leaf::Jsonb, -1));
        let (tx_id, tx_f) = intern(leafform(Leaf::Varlena, -1));
        assert_ne!(jb_id, tx_id);
        // the same definition re-created (a new OID, the same content): the same id and copy, the table does not grow
        let (again, again_f) = intern(rec(Leaf::Numeric, -1));
        assert!(again == num_id && core::ptr::eq(again_f, num_f));
        assert_eq!(INTERNED.read().unwrap().iter().filter(|e| e.0 == num_id).count(), 1, "one entry per content");
        // LAST: alternating ids in one thread (a W8 worker's claims over two programs' keys) each resolve to their own form
        for (id, f) in [(int_id, int_f), (num_id, num_f), (int_id, int_f), (jb_id, jb_f), (tx_id, tx_f), (jb_id, jb_f), (int_id, int_f)] {
            assert!(core::ptr::eq(by_id(id).unwrap(), f), "{id:#x}: LAST served another id's form");
        }
    }

    /// The heap bytes an interned form holds (its box and its children's), for the interning table's growth debt.
    fn bytes_of(f: &Form) -> usize {
        core::mem::size_of::<Form>() + match &f.kind {
            Kind::Leaf(_) => 0,
            Kind::Array(e) | Kind::Range(e) | Kind::Multirange(e) => bytes_of(e),
            Kind::Record(cols) => cols.iter().map(bytes_of).sum::<usize>() + (cols.capacity() - cols.len()) * core::mem::size_of::<Form>(),
        } + 8 + core::mem::size_of::<(u64, &Form)>()
    }

    #[test]
    fn the_interning_tables_growth_under_type_churn() {
        let rec = |n: usize| Form { len: -1, byval: false, align: 8, kind: Kind::Record((0..n).map(|i| leafform(if i % 2 == 0 { Leaf::Bytes } else { Leaf::Varlena }, if i % 2 == 0 { 4 } else { -1 })).collect()) };
        // INTERNED is process-wide and the crate's tests run in parallel, so the count is of THIS test's ids, never the table's
        // length (another test interning between two reads is not churn)
        let mut ids = std::collections::BTreeSet::new(); // alloc: Cold
        // 10k ALTER TYPE .. ADD ATTRIBUTE / DROP ATTRIBUTE cycles over one type: two structures, two entries
        for c in 0..10_000 { ids.insert(intern(rec(2 + c % 2)).0); }
        // 10k CREATE TEMP TYPE of one structure: content-interned, one entry
        for _ in 0..10_000 { ids.insert(intern(rec(5)).0); }
        let churn = ids.len();
        assert!(churn <= 3, "{churn} entries for 20k churn cycles of three structures");
        let table = INTERNED.read().unwrap();
        for id in &ids {
            assert_eq!(table.iter().filter(|e| e.0 == *id).count(), 1, "{id:#x}: one entry per content, whatever the churn");
        }
        drop(table);
        // the worst case: a composite widened one column per cycle up to MaxHeapAttributeNumber (1600): every width a new form
        let worst: usize = (1..=1600).map(|n| bytes_of(&rec(n))).sum();
        let one = bytes_of(&rec(2));
        eprintln!("canon intern: {one} bytes for a 2-column composite; 20k churn cycles -> {churn} entries; a 1..1600-column widening chain -> {worst} bytes");
        assert!(worst < 128 << 20);
    }

    #[test]
    fn an_integral_extract_is_its_int4_word() {
        // short form: 2024 (weight 0, digit 2024); -5; 0 (no digits); 12 0000 (weight 1, digits 12, 0)
        assert_eq!(integral_word(&[0x00, 0x80, 0xE8, 0x07]).expect("2024"), 2024);
        assert_eq!(integral_word(&[0x00, 0xA0, 5, 0]).expect("-5"), -5);
        assert_eq!(integral_word(&[0x00, 0x80]).expect("0"), 0);
        assert_eq!(integral_word(&[0x01, 0x80, 12, 0, 0, 0]).expect("120000"), 120_000);
        // ±Infinity: their own groups, in numeric's order, outside int4 (never a finite word)
        assert_eq!(integral_word(&[0x00, 0xD0]).expect("+Inf"), crate::agg_final::MANT_PINF);
        assert_eq!(integral_word(&[0x00, 0xF0]).expect("-Inf"), crate::agg_final::MANT_NINF);
        // 30 0000 0000 (weight 2): past int4, PG's error
        assert!(integral_word(&[0x02, 0x80, 30, 0, 0, 0, 0, 0]).is_err());
        // a fraction (weight -1, digit 5000: 0.5) and a NaN are no integral extract's result
        assert!(integral_word(&[0x7F, 0x80, 0x88, 0x13]).is_err());
        assert!(integral_word(&[0x00, 0xC0]).is_err());
        // int4's edges; a payload at an odd address (a short-header datum's) reads alike
        assert_eq!(integral_word(&[0x02, 0xA0, 21, 0, 0x8C, 0x12, 0x40, 0x0E]).expect("-2147483648"), i32::MIN as i64);
        assert!(integral_word(&[0x02, 0x80, 21, 0, 0x8C, 0x12, 0x41, 0x0E]).is_err(), "2147483649");
        let odd = [0u8, 0x00, 0x80, 0xE8, 0x07];
        assert_eq!(integral_word(&odd[1..]).expect("2024 unaligned"), 2024);
    }

    #[test]
    fn a_short_header_and_a_long_one_are_one_text() {
        let f = leafform(Leaf::Varlena, -1);
        let short = [((3 + 1) << 1 | 1) as u8, b'a', b'b', b'c'];
        assert_eq!(canon(&f, &short), canon(&f, &v4(b"abc")));
        let b = leafform(Leaf::Bpchar, -1);
        assert_eq!(canon(&b, &v4(b"ab  ")), canon(&b, &v4(b"ab")));
    }

    /// An oidvector's varlena (4-byte header): ndim, dataoffset, elemtype, then per dimension its length and lower bound,
    /// then the values from ARR_OVERHEAD_NONULLS(ndim).
    fn oidv(ndim: i32, dataoffset: i32, elemtype: i32, dims: &[(i32, i32)], values: &[u32]) -> Vec<u8> {
        let mut p = Vec::new(); // alloc: Cold
        for w in [ndim, dataoffset, elemtype] {
            p.extend_from_slice(&w.to_le_bytes());
        }
        dims.iter().for_each(|d| p.extend_from_slice(&d.0.to_le_bytes()));
        dims.iter().for_each(|d| p.extend_from_slice(&d.1.to_le_bytes()));
        while (p.len() + 4) % 8 != 0 {
            p.push(0);
        }
        values.iter().for_each(|v| p.extend_from_slice(&v.to_le_bytes()));
        v4(&p)
    }

    /// D3786: oidvectoreq / hashoidvector read dim1 and the values: a stored vector (lower bound 0) and an array
    /// constructor cast to oidvector (lower bound 1) are one value; another count or value is another; a datum that is
    /// not a valid oidvector raises check_valid_oidvector's 42804.
    #[test]
    fn an_oidvector_is_its_values_not_its_lower_bound() {
        let f = leafform(Leaf::OidVector, -1);
        let stored = oidv(1, 0, 26, &[(2, 0)], &[23, 25]);
        assert_eq!(canon(&f, &stored), canon(&f, &oidv(1, 0, 26, &[(2, 1)], &[23, 25])));
        assert_ne!(canon(&f, &stored), canon(&f, &oidv(1, 0, 26, &[(2, 0)], &[25, 23])));
        assert_ne!(canon(&f, &stored), canon(&f, &oidv(1, 0, 26, &[(1, 0)], &[23])));
        assert_eq!(canon(&f, &oidv(1, 0, 26, &[(0, 0)], &[])), canon(&f, &oidv(1, 0, 26, &[(0, 1)], &[])));
        for bad in [oidv(2, 0, 26, &[(1, 1), (1, 1)], &[23]), oidv(1, 32, 26, &[(1, 1)], &[23]), oidv(1, 0, 23, &[(1, 1)], &[23]), oidv(1, 0, 26, &[(3, 0)], &[23])] {
            let mut out = Vec::new(); // alloc: Cold
            let e = walk(&f, &bad, 0, &mut out).expect_err("not a valid oidvector");
            assert_eq!(e.message(), "array is not a valid oidvector");
        }
    }

    #[test]
    fn an_empty_null_bitmap_is_no_bitmap() {
        let f = Form { len: -1, byval: false, align: 4, kind: Kind::Array(Box::new(leafform(Leaf::Bytes, 4))) }; // alloc: Cold
        // int4[] {7, 8}, lower bound 1: no bitmap (dataoffset 0, data at ARR_OVERHEAD_NONULLS = MAXALIGN(16 + 8) = 24 from the
        // varlena's start, payload byte 20)
        let mut plain = Vec::new(); // alloc: Cold
        for w in [1i32, 0, 23, 2, 1] {
            plain.extend_from_slice(&w.to_le_bytes());
        }
        plain.extend_from_slice(&7i32.to_le_bytes());
        plain.extend_from_slice(&8i32.to_le_bytes());
        // the same with a bitmap marking both present: dataoffset MAXALIGN(24 + 1) = 32
        let mut bm = Vec::new(); // alloc: Cold
        for w in [1i32, 32, 23, 2, 1] {
            bm.extend_from_slice(&w.to_le_bytes());
        }
        bm.push(0b11);
        bm.extend_from_slice(&[0; 7]);
        bm.extend_from_slice(&7i32.to_le_bytes());
        bm.extend_from_slice(&8i32.to_le_bytes());
        let (a, b) = (v4(&plain), v4(&bm));
        assert_eq!(canon(&f, &a), canon(&f, &b));
        // a NULL second element differs
        let mut nul = bm.clone();
        nul[20] = 0b01;
        assert_ne!(canon(&f, &a), canon(&f, &v4(&nul)));
    }

    /// A multirange's varlena (4-byte header) over `w`-byte integer bounds aligned at `w` (int4multirange w = 4,
    /// int8multirange w = 8, typalign d) in `write_multirange_data`'s layout: the type oid, the count, `count - 1` items
    /// (every `MULTIRANGE_ITEM_OFFSET_STRIDE`th the range's offset from the bounds' start with the offset bit, the others
    /// the previous range's length; `lengths_only` writes every item as a length, which `multirange_get_bounds_offset`
    /// reads alike), the flags, then each range's finite bounds from `w`-alignment of the varlena's offset.
    fn intmr(ranges: &[(u8, i64, i64)], lengths_only: bool, w: usize) -> Vec<u8> {
        let n = ranges.len();
        let len_of = |r: &(u8, i64, i64)| w * ((r.0 & 0x08 == 0) as usize + (r.0 & 0x10 == 0) as usize);
        let mut p = (if w == 8 { 4536u32 } else { 4451u32 }).to_le_bytes().to_vec();
        p.extend_from_slice(&(n as u32).to_le_bytes());
        let (mut at, mut prev) = (0usize, 0usize);
        for i in 1..n {
            at += len_of(&ranges[i - 1]);
            let item = if i % 4 == 0 && !lengths_only { at as u32 | MULTIRANGE_ITEM_OFF_BIT } else { (at - prev) as u32 };
            prev = at;
            p.extend_from_slice(&item.to_le_bytes());
        }
        p.extend(ranges.iter().map(|r| r.0));
        while (p.len() + 4) % w != 0 {
            p.push(0);
        }
        for r in ranges {
            if r.0 & 0x08 == 0 {
                p.extend_from_slice(&r.1.to_le_bytes()[..w]);
            }
            if r.0 & 0x10 == 0 {
                p.extend_from_slice(&r.2.to_le_bytes()[..w]);
            }
        }
        v4(&p)
    }

    fn int4mr(ranges: &[(u8, i32, i32)], lengths_only: bool) -> Vec<u8> {
        let wide: Vec<(u8, i64, i64)> = ranges.iter().map(|r| (r.0, r.1 as i64, r.2 as i64)).collect(); // alloc: Cold
        intmr(&wide, lengths_only, 4)
    }

    fn int4mr_form() -> Form {
        Form { len: -1, byval: false, align: 4, kind: Kind::Multirange(Box::new(leafform(Leaf::Bytes, 4))) } // alloc: Cold
    }

    #[test]
    fn a_multirange_is_its_ranges_in_order() {
        let f = int4mr_form();
        let mr = |r: &[(u8, i32, i32)]| canon(&f, &int4mr(r, false));
        let a = mr(&[(0x02, 1, 3), (0x02, 5, 7)]);
        assert_eq!(a, mr(&[(0x02, 1, 3), (0x02, 5, 7)]));
        assert_ne!(a, mr(&[(0x02, 1, 3)]));
        assert_ne!(a, mr(&[(0x02, 1, 3), (0x02, 5, 8)]));
        assert_ne!(a, mr(&[(0x06, 1, 3), (0x02, 5, 7)]), "an inclusive upper bound differs");
        // the empty multirange: no items, no flags
        assert_eq!(mr(&[]), 0u32.to_le_bytes());
        // an unbounded lower bound (flags LB_INF) carries only the upper bound
        assert_eq!(mr(&[(0x08, 0, 3)])[4..], [9, 0, 0, 0, 0x08, 4, 0, 0, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn a_steady_state_walk_reuses_its_temporaries() {
        // the container's flat form is a pooled temporary: after one warm walk the same walks take their buffers from the
        // pool and give them back, so no buffer is added and none regrows
        let f = int4mr_form();
        let bytes = int4mr(&[(0x02, 1, 3), (0x02, 5, 7)], false);
        let first = canon(&f, &bytes);
        let snap = || {
            let mut v: Vec<(usize, usize)> = TMPS.with(|p| p.borrow().iter().filter(|b| b.capacity() > 0).map(|b| (b.as_ptr() as usize, b.capacity())).collect());
            v.sort();
            v
        };
        let warm = snap();
        assert!(!warm.is_empty(), "the warm walk gave a buffer back");
        for _ in 0..100 {
            assert_eq!(canon(&f, &bytes), first);
        }
        assert_eq!(snap(), warm, "no new buffer, none regrown");
    }

    #[test]
    fn a_form_holds_a_multirange_at_any_level_and_no_other_does() {
        // D1901's in-row refusal reads it: the multirange, an array of one, a composite column, a range over one
        let boxed = |k: Kind| Form { len: -1, byval: false, align: 4, kind: k };
        assert!(int4mr_form().holds_multirange()); // alloc: Cold
        assert!(boxed(Kind::Array(Box::new(int4mr_form()))).holds_multirange()); // alloc: Cold
        assert!(boxed(Kind::Record(vec![leafform(Leaf::Bytes, 4), int4mr_form()])).holds_multirange()); // alloc: Cold
        assert!(boxed(Kind::Range(Box::new(int4mr_form()))).holds_multirange()); // alloc: Cold
        // does not apply: a leaf, an array, a composite and a range with no multirange (trunk's keys)
        assert!(!leafform(Leaf::Varlena, -1).holds_multirange());
        assert!(!boxed(Kind::Array(Box::new(leafform(Leaf::Bytes, 4)))).holds_multirange()); // alloc: Cold
        assert!(!boxed(Kind::Record(vec![leafform(Leaf::Bytes, 4), leafform(Leaf::Varlena, -1)])).holds_multirange()); // alloc: Cold
        assert!(!boxed(Kind::Range(Box::new(boxed(Kind::Record(vec![leafform(Leaf::Numeric, -1)]))))).holds_multirange()); // alloc: Cold
    }

    #[test]
    fn a_multirange_past_the_offset_stride_reads_every_range() {
        // 5 to 9 ranges: items 4 and 8 are offsets; the first range unbounded below, the last above (no bytes for either)
        let f = int4mr_form();
        for n in 5..=9usize {
            let r: Vec<(u8, i32, i32)> = (0..n).map(|i| (if i == 0 { 0x08 } else if i == n - 1 { 0x12 } else { 0x02 }, 10 * i as i32, 10 * i as i32 + 5)).collect(); // alloc: Cold
            let pg = canon(&f, &int4mr(&r, false));
            assert_eq!(pg, canon(&f, &int4mr(&r, true)), "{n} ranges: offset items and length items name the same ranges");
            // the count, then per range its form behind its length: the flags, each finite bound behind its length
            let mut want = (n as u32).to_le_bytes().to_vec();
            for (fl, lo, hi) in &r {
                let mut one = vec![*fl]; // alloc: Cold
                for (inf, b) in [(0x08, lo), (0x10, hi)] {
                    if fl & inf == 0 {
                        one.extend_from_slice(&4u32.to_le_bytes());
                        one.extend_from_slice(&b.to_le_bytes());
                    }
                }
                want.extend_from_slice(&(one.len() as u32).to_le_bytes());
                want.extend_from_slice(&one);
            }
            assert_eq!(pg, want, "{n} ranges");
            let mut later = r.clone();
            later[n - 2].2 += 1;
            assert_ne!(pg, canon(&f, &int4mr(&later, false)), "{n} ranges: a range past the first offset item differs");
        }
    }

    #[test]
    fn an_int8_multirange_reads_its_bounds_from_8_byte_alignment() {
        // int8 bounds (typalign d): the bounds start at DOUBLEALIGN(12 + 4 * (n - 1) + n) of the varlena, 4 bytes past
        // INTALIGN's at n = 2, 4, 5 and 7 (24 vs 20, 32 vs 28, 40 vs 36, 48 vs 44); 5 to 9 ranges read the offset items too
        let f = Form { len: -1, byval: false, align: 4, kind: Kind::Multirange(Box::new(leafform(Leaf::Bytes, 8))) }; // alloc: Cold
        let big = 1i64 << 40;
        for n in 2..=9usize {
            let r: Vec<(u8, i64, i64)> = (0..n).map(|i| (if i == n - 1 { 0x12 } else { 0x02 }, big + 20 * i as i64, big + 20 * i as i64 + 5)).collect(); // alloc: Cold
            let pg = canon(&f, &intmr(&r, false, 8));
            assert_eq!(pg, canon(&f, &intmr(&r, true, 8)), "{n} ranges: offset items and length items name the same ranges");
            let mut want = (n as u32).to_le_bytes().to_vec();
            for (fl, lo, hi) in &r {
                let mut one = vec![*fl]; // alloc: Cold
                for (inf, b) in [(0x08, lo), (0x10, hi)] {
                    if fl & inf == 0 {
                        one.extend_from_slice(&8u32.to_le_bytes());
                        one.extend_from_slice(&b.to_le_bytes());
                    }
                }
                want.extend_from_slice(&(one.len() as u32).to_le_bytes());
                want.extend_from_slice(&one);
            }
            assert_eq!(pg, want, "{n} ranges");
            let mut later = r.clone();
            later[n - 1].1 += 1;
            assert_ne!(pg, canon(&f, &intmr(&later, false, 8)), "{n} ranges: the last range's lower bound differs");
        }
    }

    #[test]
    fn an_interval_is_its_span() {
        let f = leafform(Leaf::Interval, 16);
        let iv = |time: i64, day: i32, month: i32| [time.to_le_bytes().to_vec(), day.to_le_bytes().to_vec(), month.to_le_bytes().to_vec()].concat();
        assert_eq!(canon(&f, &iv(0, 0, 1)), canon(&f, &iv(0, 30, 0)));
        assert_eq!(canon(&f, &iv(86_400_000_000, 0, 0)), canon(&f, &iv(0, 1, 0)));
        assert_ne!(canon(&f, &iv(0, 1, 0)), canon(&f, &iv(0, 2, 0)));
    }

    /// `raw` as a pglz-compressed inline varlena (a 4-byte header with the compressed bits, `va_tcinfo` = the raw size
    /// under method pglz, then a literal-only stream: a zero control byte before each 8 raw bytes).
    fn pglz_literal(raw: &[u8]) -> Vec<u8> {
        let mut data = Vec::new(); // alloc: Cold
        for c in raw.chunks(8) {
            data.push(0);
            data.extend_from_slice(c);
        }
        let mut v = ((((8 + data.len()) as u32) << 2) | 0x02).to_le_bytes().to_vec();
        v.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    /// A (text, int4) composite datum, its text column stored as `col` from t_hoff 24 (no NULL), then the int4.
    fn text_int4_record(col: &[u8], n: i32) -> Vec<u8> {
        let mut t = vec![0u8; 24]; // alloc: Cold
        t[18..20].copy_from_slice(&2u16.to_le_bytes());
        t[22] = 24;
        t.extend_from_slice(col);
        while t.len() % 4 != 0 {
            t.push(0);
        }
        t.extend_from_slice(&n.to_le_bytes());
        let l = t.len() as u32;
        t[0..4].copy_from_slice(&(l << 2).to_le_bytes());
        t
    }

    #[test]
    fn a_compressed_column_is_its_plain_value() {
        // D1607: record_eq detoasts a column heap_form_tuple kept compressed inline; the form is the plain value's
        let f = Form { len: -1, byval: false, align: 8, kind: Kind::Record(vec![leafform(Leaf::Varlena, -1), leafform(Leaf::Bytes, 4)]) }; // alloc: Cold
        let text: Vec<u8> = b"ab".iter().copied().cycle().take(300).chain(*b"1").collect(); // alloc: Cold
        let (plain, packed) = (text_int4_record(&v4(&text), 7), text_int4_record(&pglz_literal(&text), 7));
        assert_eq!(canon(&f, &plain), canon(&f, &packed));
        let mut other = text.clone();
        *other.last_mut().expect("a byte") = b'2';
        assert_ne!(canon(&f, &plain), canon(&f, &text_int4_record(&pglz_literal(&other), 7)));
        assert_ne!(canon(&f, &plain), canon(&f, &text_int4_record(&pglz_literal(&text), 8)));
        // the same record as a range's bound: its compressed column decompressed there too
        let r = Form { len: -1, byval: false, align: 8, kind: Kind::Range(Box::new(f)) }; // alloc: Cold
        let range = |a: &[u8], b: &[u8]| {
            let mut p = 3905u32.to_le_bytes().to_vec(); // the range type's oid (not read)
            p.extend_from_slice(a);
            while p.len() % 8 != 4 {
                p.push(0);
            }
            p.extend_from_slice(b);
            p.push(0x02 | 0x04); // RANGE_LB_INC | RANGE_UB_INC
            v4(&p)
        };
        let lo = text_int4_record(&v4(&text), 1);
        let (hi_plain, hi_packed) = (text_int4_record(&v4(&text), 2), text_int4_record(&pglz_literal(&text), 2));
        assert_eq!(canon(&r, &range(&lo, &hi_plain)), canon(&r, &range(&lo, &hi_packed)));
    }
}
