//! typesem's key reader (v5 semantics landing 5; ported from prototype 6, 2026-10-01): one reader of a hashed key's semantics, from its operator's
//! hash opfamily as PostgreSQL's executor resolves it (nodeSubplan.c `ExecInitSubPlan`, nodeHashjoin.c: `get_opcode`,
//! `get_op_hash_functions`, `get_compatible_hash_operators`). No type list decides the form: the operator and its family
//! do; `By::Word` is the one native form a hashed set probes in IR.
//! - `By::Word`: equal datum words are equal values: a by-value builtin type whose canonical form is its bytes (canon.rs
//!   `Leaf::Bytes`), under a builtin operator whose hash support is builtin; across two integer types of one hash family,
//!   the narrower side sign-extended.
//! - `By::Canonical(form)`: canon.rs's form decides equality (text under a deterministic collation, numeric, uuid, ..).
//! - `By::Fmgr`: PostgreSQL's own support functions through fmgr: a user type, a user operator or hash function, a
//!   nondeterministic collation, a cross-type operator whose two sides have no common native image. Never a refusal by
//!   itself; only an operator with no hash support is `Err`.
//!
//! `KeyUse::Sort` reads an ordering operator (a SortGroupClause's `sortop`, `<` or `>`) as PostgreSQL's sort support
//! resolves it (sortsupport.c `PrepareSortSupportFromOrderingOp`, lsyscache.c `get_ordering_op_properties`): its btree
//! family's support 1 is the comparator (`Procs::cmp`), its family's `=` the equality (`Procs::eq`), `desc` the `>` side.
//! `By::Word` there also carries the word's ORDER (`signed`), read from the comparator's own input type, so a user btree
//! class over a built-in comparator orders as that comparator does. An enum, a float, any varlena or collation sort as
//! `By::Fmgr` (the comparator): equal bytes are equal values for them, but byte order is not their order. Only an
//! operator that is not a btree ordering operator is `Err` (`TypeClass::NoOrder`).
//!
//! D2598 (D1292's construct, here for every hashed or sorted key that asks typesem): a collatable key type under no
//! collation (InvalidOid: two conflicting implicit collations, `(x, y) NOT IN (SELECT y, x ..)` over a "C" and a "POSIX"
//! column). PostgreSQL's collation-aware support procs (`hashtext` / `hashbpchar`, `texteq` / `bpchareq`,
//! `varstr_sortsupport`) raise 42P22 at their first call on a non-NULL value. A hashed key whose hash proc is one of
//! those (`types_core::quiet::HASH_NEEDS_COLLATION`) is answered with `raise_hash`: an error-raising hash, its form the values' bytes (no
//! comparison runs past the first hash); its consumer raises PG's error where PG hashes, or refuses by D1292's name
//! (`INDETERMINATE`). Any other collatable key under no collation, and every such sorted key, is `Err`
//! (`TypeClass::Indeterminate`): the caller refuses it by that name and PostgreSQL raises its own error (`name` hashes
//! and compares its bytes under any collation: refusing its hashed key is only conservative).
use types_core::Oid;
use types_error::PgResult;

pub mod call;
pub mod catalog;
pub mod rep;
pub mod eq;
pub mod image;
pub mod of_type;
pub mod subplan;
pub mod hashjoin;
pub mod user;

pub use gravity_abi::key::{KeyReq, KeyUse};

/// How two values are compared and hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The native word: `lw` / `rw` the two sides' widths in bytes (a cross-type integer pair sign-extends the narrower);
    /// `signed` how two words order (`KeyUse::Sort`: a two's-complement or an unsigned compare; the hashed uses never read
    /// it and answer false).
    Word { lw: u8, rw: u8, signed: bool },
    /// canon.rs's form (its content id) of the (one) key type.
    Canonical(u64),
    /// PostgreSQL's support functions through fmgr (`Fmgr` below).
    Fmgr,
}

/// The support functions PostgreSQL's executor calls for the key (every form has them; `By::Fmgr` is the one that calls
/// them at run time): `eq` the operator's function (lhs op rhs), `lhs_hash` / `rhs_hash` its hash family's support 1 for
/// each side, `rhs_eq` the right type's own `=` (the set's duplicate test, `tab_eq_funcs`), `cmp` the btree comparator
/// (`KeyUse::Sort`; 0 for the hashed uses). A sorted key has no hash procs (0) and `rhs_eq` = `eq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Procs {
    pub eq: Oid,
    pub lhs_hash: Oid,
    pub rhs_hash: Oid,
    pub rhs_eq: Oid,
    pub cmp: Oid,
}

/// The coordinate a refusal names (interfaces.md §2.10's `TypeClass`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeClass {
    /// The operator has no hash support (PostgreSQL could not have hashed it).
    NoHash(Oid),
    /// The catalog does not know the operator or a type.
    Unknown(Oid),
    /// The operator is not a btree ordering operator, or its family has no comparator (PostgreSQL could not sort by it).
    NoOrder(Oid),
    /// D2598: an input type of the operator is collatable and the key's collation is InvalidOid (PostgreSQL's hash,
    /// equality or sort support raises 42P22 at its first non-NULL call), and no `raise_hash` answer stands for it.
    Indeterminate(Oid),
}

/// D1292's name (decompose ops/sort `INDETERMINATE_COLL`, the same words): one refusal for the construct at every site.
pub const INDETERMINATE: &str = "D1292: a key over a collatable type with no collation (PG raises 42P22 at its first compare)";

impl TypeClass {
    pub const fn why(self) -> &'static str {
        match self {
            TypeClass::NoHash(_) => "a hashed key whose operator has no hash support (typesem)",
            TypeClass::Unknown(_) => "a key whose operator or type the catalog does not know (typesem)",
            TypeClass::NoOrder(_) => "a sorted key whose operator is not a btree ordering operator (typesem)",
            TypeClass::Indeterminate(_) => INDETERMINATE,
        }
    }
}

/// One key's semantics: the operator, its two input types, the collation, the form and the procs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeySem {
    pub op: Oid,
    pub lt: Oid,
    pub rt: Oid,
    pub coll: Oid,
    pub by: By,
    pub procs: Procs,
    /// Every proc built in and the collation deterministic: a native image may stand for the operator.
    pub native: bool,
    /// `KeyUse::Sort`: the operator is its family's `>` (a descending sort); false otherwise.
    pub desc: bool,
    /// D2598: the collation is InvalidOid and a hash proc reads it (`types_core::quiet::HASH_NEEDS_COLLATION`): PostgreSQL's hash raises
    /// 42P22 ("could not determine which collation to use for string hashing") at the first non-NULL value it hashes. A
    /// consumer raises that error where PG hashes, or refuses by `INDETERMINATE`; `by` reads the values as bytes.
    pub raise_hash: bool,
    /// `KeyUse::Hash`, D2642: PostgreSQL's hash table (a hashed ScalarArrayOpExpr, SubPlan, Agg) finds what calling the
    /// equality proc on each candidate finds: the hash procs agree with it (equal under it, equal hash). True for a
    /// built-in operator (its family's), for one whose procs are user code (the hash class's own contract) and for every
    /// form keyed by a word or an image; false for a user operator over built-in procs that `by` cannot key (=! over
    /// int4ne / hashint4: the table looks only where the hash agrees, then compares by int4ne, and finds nothing).
    pub linear: bool,
}

const FIRST_NORMAL: Oid = 16384;

/// A catalog object of PostgreSQL's own (an oid below FirstNormalObjectId), not a user's.
pub(super) fn builtin(o: Oid) -> bool {
    o != 0 && o < FIRST_NORMAL
}

const INTERNAL_LANGUAGE: Oid = 12;

/// The function PostgreSQL calls for proc `f`: `body`'s built-in, else `f` itself.
fn fmgr_proc(mcx: mcx::Mcx<'_>, f: Oid) -> PgResult<Oid> {
    Ok(body(mcx, f)?.unwrap_or(f))
}

std::thread_local! {
    /// `body`'s answers for user procs: a lowering asks the same proc at every walk, and `fmgr_internal_function` scans the
    /// builtin table by name. D3082: `sql_inlined`'s readings share it, keyed by the search path they were inlined under
    /// (`Some(path key)`, a body's name resolution depends on it); `body`'s entries carry `None`. C3: each entry holds
    /// what its read took from the catalog (gravity_deps `Held`): a hit serves it while those reads stand and gives them
    /// to the lowering's recording; one whose reads moved is read again (`memo_hit`).
    static BODIES: core::cell::RefCell<Vec<(Oid, Option<u64>, Option<Oid>, gravity_deps::Held)>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Cold
}

/// `BODIES`' entry for `(f, path)` while its reads stand (replayed into the asker's recording), dropping it when they moved;
/// in check mode (gravity_deps `checking`) with its `Held`, for the asker to check outside the memo's borrow.
fn memo_hit(m: &mut Vec<(Oid, Option<u64>, Option<Oid>, gravity_deps::Held)>, f: Oid, path: Option<u64>) -> Option<(Option<Oid>, Option<gravity_deps::Held>)> {
    let i = m.iter().position(|e| (e.0, e.1) == (f, path))?;
    if m[i].3.holds() {
        return Some((m[i].2, gravity_deps::checking().then(|| m[i].3.clone())));
    }
    m.swap_remove(i);
    None
}

/// Room in `BODIES` for one more entry, of at most 4096 (a full memo first drops its entries whose reads moved).
fn memo_room(m: &mut Vec<(Oid, Option<u64>, Option<Oid>, gravity_deps::Held)>) -> bool {
    if m.len() >= 4096 {
        m.retain(|e| e.3.stands());
    }
    m.len() < 4096
}

/// The one decider (KeySem, LANGUAGE internal; equivclass#53): the built-in C function a call of proc `f` runs, as
/// `fmgr_info` resolves it. A built-in is itself. A user function `LANGUAGE internal` is the built-in its `prosrc` names
/// (fmgr's `fmgr_internal_function`: the lowest-oid built-in row of that name), so `CREATE FUNCTION i8a_eq(i8a, i8a) ..
/// LANGUAGE internal AS 'int8eq'` is int8eq, when PostgreSQL calls that function exactly as it calls the built-in: the
/// same kind and set-returning mark, the same number of arguments, and each argument and the result stored as the
/// built-in's (the same type, or two types that are no pseudo-types with one typlen, typbyval and collatability: int8alias1
/// `like = int8` is int8 to int8eq; a non-collatable type is no text to texteq, which raises with no collation), and with
/// no `fmgr_security_definer` around it (SECURITY DEFINER or a SET clause switch the user or GUCs per call). Anything else
/// is `None`: SQL, PL and C functions, an unknown `prosrc`, a proc the catalog has no row for. The effects home reads a
/// call by this answer (`Facts::builtin_body`) and every KeySem proc is resolved through it (`fmgr_proc`); rule RK flags an
/// alias lookup anywhere else.
pub fn body(mcx: mcx::Mcx<'_>, f: Oid) -> PgResult<Option<Oid>> {
    if builtin(f) {
        return Ok(Some(f));
    }
    if f == 0 || !syscache_seams::lookup_pg_proc_shape::is_installed() {
        return Ok(None);
    }
    if let Some((b, held)) = BODIES.with(|m| memo_hit(&mut m.borrow_mut(), f, None)) {
        if let Some(held) = held {
            held.check("keysem body (BODIES)", || alias_of(mcx, f).ok() == Some(b));
        }
        return Ok(b);
    }
    let (b, held) = gravity_deps::Held::fill(|| alias_of(mcx, f));
    let b = b?;
    BODIES.with(|m| {
        let mut m = m.borrow_mut();
        if memo_room(&mut m) {
            m.push((f, None, b, held)); // alloc: Cold
        }
    });
    Ok(b)
}

/// `body` of user proc `f`, read from the catalog.
fn alias_of(mcx: mcx::Mcx<'_>, f: Oid) -> PgResult<Option<Oid>> {
    let Some(p) = syscache_seams::lookup_pg_proc_shape::call(f)? else { return Ok(None) };
    if p.prolang != INTERNAL_LANGUAGE || p.prosecdef || !p.proconfig_isnull {
        return Ok(None);
    }
    let Some(src) = syscache_seams::lookup_pg_proc_prosrc::call(mcx, f)? else { return Ok(None) };
    let b = fmgr_core::fmgr_internal_function(src.as_str());
    if !builtin(b) {
        return Ok(None);
    }
    let Some(q) = syscache_seams::lookup_pg_proc_shape::call(b)? else { return Ok(None) };
    if (p.prokind, p.proretset, p.pronargs) != (q.prokind, q.proretset, q.pronargs) {
        return Ok(None);
    }
    let (Some((pr, pa)), Some((qr, qa))) = (syscache_seams::lookup_pg_proc_signature::call(mcx, f)?, syscache_seams::lookup_pg_proc_signature::call(mcx, b)?) else { return Ok(None) };
    if pa.len() != qa.len() {
        return Ok(None);
    }
    for (u, t) in core::iter::once((pr, qr)).chain(pa.iter().copied().zip(qa.iter().copied())) {
        if !stores_as(u, t)? {
            return Ok(None);
        }
    }
    Ok(Some(b))
}

const SQL_LANGUAGE: Oid = 14;
const RECORD_TYPE: Oid = 2249;

/// D3082 (clauses.c `inline_function`, read for effects only): user function `f` inlined as PostgreSQL's planner would
/// inline a call of it, and `judge` asked about the substituted body (its parameters `$1..$n` left as PARAM_EXTERN
/// leaves of the declared argument types, no input collation). The body comes from the planner's own middle
/// (`clauses_seams::inline_sql_function`: the parse, `check_sql_fn_retval`, the volatility and strictness gates and the
/// substitution); the outer gates are `inline_function`'s (LANGUAGE sql, kind `f`, not SECURITY DEFINER, no SET clause,
/// not set-returning, not `record`). The call stays the SQL function's (fmgr still runs it); only the reading is the
/// body's. False when it is not inlinable, when inlining errors (the error is dropped: the caller refuses), or when the
/// body's reading is already being asked (a body calling itself). The answer is memoized per search path (a body's names
/// resolve through it) while what its reading read stands (C3: CREATE OR REPLACE moves the pg_proc row); an error leaves
/// no entry.
pub fn sql_inlined(mcx: mcx::Mcx<'_>, f: Oid, judge: &mut dyn FnMut(types_nodes::Node<'_>) -> bool) -> PgResult<bool> {
    let installed = syscache_seams::lookup_pg_proc_shape::is_installed()
        && syscache_seams::lookup_pg_proc_signature::is_installed()
        && clauses_seams::inline_sql_function::is_installed()
        && namespace_seams::fetch_search_path::is_installed();
    if f == 0 || builtin(f) || !installed {
        return Ok(false);
    }
    let path = namespace_seams::fetch_search_path::call(mcx, true)?.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &o| (h ^ u64::from(o)).wrapping_mul(0x0100_0000_01b3));
    let memo = BODIES.with(|m| {
        let mut m = m.borrow_mut();
        // no check of a hit here: its fill calls the asker's `judge` again (not a pure read)
        if let Some((hit, _)) = memo_hit(&mut m, f, Some(path)) {
            return Some(hit.is_some());
        }
        if !memo_room(&mut m) {
            // no room to mark it in flight (a body that reaches `f` again could not be stopped): not read
            return Some(false);
        }
        // in flight (nothing read: it stands): a body that reaches `f` again reads it as not inlinable
        m.push((f, Some(path), None, gravity_deps::Held::of(gravity_deps::Deps::default()))); // alloc: Cold
        None
    });
    if let Some(q) = memo {
        return Ok(q);
    }
    let (read, held) = gravity_deps::Held::fill(|| inline_read(mcx, f, judge, false));
    BODIES.with(|m| {
        let mut m = m.borrow_mut();
        let at_entry = m.iter().position(|e| (e.0, e.1) == (f, Some(path)));
        match (&read, at_entry) {
            (Ok(q), Some(i)) => (m[i].2, m[i].3) = (q.then_some(f), held),
            (Err(_), Some(i)) => {
                m.swap_remove(i);
            }
            _ => {}
        }
    });
    Ok(read.unwrap_or(false))
}

/// D3713: user `LANGUAGE sql` function `f`'s body as fmgr_sql runs it for a call, judged as `sql_inlined` judges it, past
/// `inline_function`'s SECURITY DEFINER and SET clause gates when the SET clause names no search_path (the body then
/// parses under the caller's path, as the planner's reading does). Not memoized: only a sort's comparator asks it, and
/// the body's own calls read through `sql_inlined`.
pub fn sql_fmgr_body(mcx: mcx::Mcx<'_>, f: Oid, judge: &mut dyn FnMut(types_nodes::Node<'_>) -> bool) -> PgResult<bool> {
    let installed = syscache_seams::lookup_pg_proc_shape::is_installed()
        && syscache_seams::lookup_pg_proc_signature::is_installed()
        && syscache_seams::lookup_pg_proc_secdef::is_installed()
        && clauses_seams::inline_sql_function::is_installed();
    if f == 0 || builtin(f) || !installed {
        return Ok(false);
    }
    inline_read(mcx, f, judge, true)
}

/// `sql_inlined`'s read of user proc `f` from the catalog, judged; `fmgr`: `sql_fmgr_body`'s gates.
fn inline_read(mcx: mcx::Mcx<'_>, f: Oid, judge: &mut dyn FnMut(types_nodes::Node<'_>) -> bool, fmgr: bool) -> PgResult<bool> {
    let Some(p) = syscache_seams::lookup_pg_proc_shape::call(f)? else { return Ok(false) };
    let wrapped = p.prosecdef || !p.proconfig_isnull;
    if p.prolang != SQL_LANGUAGE || p.prokind != b'f' as i8 || p.proretset || p.prorettype == RECORD_TYPE || wrapped && !(fmgr && path_kept(f)?) {
        return Ok(false);
    }
    let Some((ret, argtypes)) = syscache_seams::lookup_pg_proc_signature::call(mcx, f)? else { return Ok(false) };
    if argtypes.len() != usize::try_from(p.pronargs).unwrap_or(usize::MAX) {
        return Ok(false);
    }
    let mut params = Vec::with_capacity(argtypes.len()); // alloc: Cold
    for (i, &t) in argtypes.iter().enumerate() {
        let param = types_nodes::primnodes::Param {
            paramkind: types_nodes::primnodes::ParamKind::PARAM_EXTERN,
            paramid: i as i32 + 1,
            paramtype: t,
            paramtypmod: -1,
            location: -1,
            ..Default::default()
        };
        params.push(types_nodes::Node::mk(mcx, param)?); // alloc: Cold
    }
    let args = types_nodes::NodeList::from_slice(mcx, &params)?;
    let Some(body) = clauses_seams::inline_sql_function::call(mcx, f, ret, 0, 0, &args)? else { return Ok(false) };
    Ok(judge(body))
}

/// D3713: function `f`'s SET clause names no search_path.
fn path_kept(f: Oid) -> PgResult<bool> {
    let Some(s) = syscache_seams::lookup_pg_proc_secdef::call(f)? else { return Ok(false) };
    Ok(s.proconfig.iter().flatten().all(|e| !e.split('=').next().is_some_and(|n| n.trim().eq_ignore_ascii_case("search_path"))))
}

/// The support functions a plan node naming operator `op` alone runs (a Sort's, MergeAppend's, Group's, Unique's, SetOp's,
/// WindowAgg's or Memoize's operator, an aggregate's SortGroupClause), for the effects home to classify (`effects::of_key`'s
/// first reader: decompose's F5 walk through `Facts::op_calls`). Raw procs, each read by `body` there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCalls {
    /// `oprcode` (`execTuplesMatchPrepare`'s equality, a run condition's test).
    pub code: Oid,
    /// A btree ordering operator's family support for its input type (`get_ordering_op_properties`): sortsupport (2) and
    /// comparator (1), as `PrepareSortSupportFromOrderingOp` calls them; `[0, 0]`: PostgreSQL raises "missing support
    /// function" at the sort's first call (W8-EQUIVCLASS-SORTOP). None: no ordering operator.
    pub order: Option<[Oid; 2]>,
    /// A hashable equality's hash family support 1 for each input (`get_op_hash_functions`, `execTuplesHashPrepare`).
    pub hash: Option<[Oid; 2]>,
}

/// `OpCalls` of operator `op`; none for an operator the catalog does not know (no `oprcode`).
pub fn op_calls(op: Oid) -> PgResult<Option<OpCalls>> {
    if !available() || !syscache_seams::lookup_pg_amproc::is_installed() {
        return Ok(None);
    }
    let code = lsyscache::get_opcode(op)?;
    if code == 0 {
        return Ok(None);
    }
    let order = match lsyscache::get_ordering_op_properties(op)? {
        Some((family, t, _)) => Some([lsyscache::get_opfamily_proc(family, t, t, 2)?, lsyscache::get_opfamily_proc(family, t, t, 1)?]),
        None => None,
    };
    let hash = lsyscache::get_op_hash_functions(op)?.map(|(l, r)| [l, r]);
    Ok(Some(OpCalls { code, order, hash }))
}

/// KeySem slice 3 (the plan-time catalog's `Facts::key` for `KeyReq::sort`, its one order read): a sort operator's order as
/// PostgreSQL's tuplesort resolves it (`PrepareSortSupportFromOrderingOp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortOrder {
    /// Strategy 5 (`>`): the key sorts descending.
    pub desc: bool,
    /// The btree family the operator orders by (`get_ordering_op_properties`).
    pub family: Oid,
    /// The family's input type the operator is an ordering member for.
    pub opcintype: Oid,
    /// Whether `family` is `opcintype`'s default btree family (the type cache's `btree_opf`): only that family's order is
    /// the one a key's image keeps (D1346); any other family orders by `cmp`.
    pub default_family: bool,
    /// The family's comparator (`BTORDER_PROC`) for `(opcintype, opcintype)`; 0 when it has none.
    pub cmp: Oid,
    /// W8-EQUIVCLASS-SORTOP: the family holds neither a sortsupport (2) nor a comparator for `(opcintype, opcintype)`, so
    /// `FinishSortSupportFunction` raises "missing support function 1(T,T) in opfamily F" at the sort's first call.
    pub no_proc: bool,
}

/// `SortOrder` of sort operator `op`; none for an operator that is no btree ordering operator (strategy 1 or 5 of some
/// btree family over one type).
pub fn sort_order(op: Oid) -> PgResult<Option<SortOrder>> {
    let Some((family, t, cmptype)) = lsyscache::get_ordering_op_properties(op)? else { return Ok(None) };
    let default = typcache::lookup_type_cache(t, typcache::TYPECACHE_BTREE_OPFAMILY)?.btree_opf();
    let cmp = lsyscache::get_opfamily_proc(family, t, t, super::BTORDER_PROC)?;
    let no_proc = cmp == 0 && lsyscache::get_opfamily_proc(family, t, t, 2)? == 0;
    Ok(Some(SortOrder { desc: cmptype == lsyscache::COMPARE_GT, family, opcintype: t, default_family: family == default, cmp, no_proc }))
}

/// A datum of type `u` is one of type `t` to a C function reading `t`: the same type, or two types that are no pseudo-types
/// with one storage (typlen, typbyval) and one collatability.
fn stores_as(u: Oid, t: Oid) -> PgResult<bool> {
    const PSEUDO: i8 = b'p' as i8;
    if u == t {
        return Ok(true);
    }
    if u == 0 || t == 0 || lsyscache::typ::get_typtype(u)? == PSEUDO || lsyscache::typ::get_typtype(t)? == PSEUDO {
        return Ok(false);
    }
    Ok(lsyscache::get_typlenbyval(u)? == lsyscache::get_typlenbyval(t)? && lsyscache::type_is_collatable(u)? == lsyscache::type_is_collatable(t)?)
}

/// What an equality operator over a type compares by, as fmgr resolves its procedure (`eq_form`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqForm {
    /// The type's own `=` (the type cache's `eq_opr`): the type's canonical form decides equality.
    Own,
    /// `byteaeq` over a varlena (V4R-508's user family of byte comparators): equal payload bytes, bytea's form, which
    /// tells apart values the type's own `=` calls equal ('{[1.0,2)}' and '{[1,2)}' as nummultirange).
    Bytes,
    /// Any other built-in procedure (int4ne, textne): only PostgreSQL's function decides, and it is not an equality of
    /// the type's form.
    Other,
    /// User code (plpgsql, SQL, C): only PostgreSQL's function decides; what it compares is not known here.
    User,
}

const F_BYTEAEQ: Oid = 1948;

/// `eq_form` of resolved equality procedure `eq` over type `typid`. A type with no `eq_opr` in its type cache (a crate
/// rig's catalog: every type typesem forms has one in PostgreSQL's) answers `Own`.
fn eq_form_of(mcx: mcx::Mcx<'_>, eq: Oid, typid: Oid) -> PgResult<EqForm> {
    let own = typcache::lookup_type_cache(typid, typcache::TYPECACHE_EQ_OPR)?.eq_opr();
    if own == 0 || fmgr_proc(mcx, lsyscache::get_opcode(own)?)? == eq {
        return Ok(EqForm::Own);
    }
    if !builtin(eq) {
        return Ok(EqForm::User);
    }
    Ok(if eq == F_BYTEAEQ && lsyscache::get_typlenbyval(typid)?.0 == -1 { EqForm::Bytes } else { EqForm::Other })
}

/// D2163 / D2637: what a key of type `typid` grouped or hashed under equality operator `op` compares by: the operator's
/// procedure as fmgr resolves it (a user `LANGUAGE internal` wrapper is the built-in its prosrc names) against the
/// type's own `=`. A form keyed by the type's canonical image is right only for `Own`.
/// D2642: under its own `=`, a hashing operator's hash procs must be the type's own too (a hashed group finds equal values
/// by hash first); equal bytes (`Bytes`) hash equal under any proc.
pub fn eq_form(mcx: mcx::Mcx<'_>, op: Oid, typid: Oid) -> PgResult<EqForm> {
    let eq = lsyscache::get_opcode(op)?;
    if eq == 0 {
        return Ok(EqForm::Other);
    }
    let form = eq_form_of(mcx, fmgr_proc(mcx, eq)?, typid)?;
    if form == EqForm::Own && !builtin(op) {
        if let Some((l, r)) = lsyscache::get_op_hash_functions(op)? {
            if !hash_own(mcx, fmgr_proc(mcx, l)?, typid)? || !hash_own(mcx, fmgr_proc(mcx, r)?, typid)? {
                return Ok(EqForm::Other);
            }
        }
    }
    Ok(form)
}

/// D2642: whether resolved hash proc `h` agrees with type `typid`'s own `=`: the type cache's `hash_proc`, or user code
/// (the hash class's contract: equal values hash equal). A type cache the catalog cannot read, or one with no hash
/// proc, answers true (as `eq_form_of` answers `Own`).
fn hash_own(mcx: mcx::Mcx<'_>, h: Oid, typid: Oid) -> PgResult<bool> {
    if !builtin(h) {
        return Ok(true);
    }
    let Ok(e) = typcache::lookup_type_cache(typid, typcache::TYPECACHE_HASH_PROC) else { return Ok(true) };
    let own = e.hash_proc();
    Ok(own == 0 || fmgr_proc(mcx, own)? == h)
}

/// The width of a by-value type whose canonical form is its bytes (canon.rs `Leaf::Bytes`), else `None`.
fn word_width(typid: Oid) -> PgResult<Option<u8>> {
    if !super::bytes_leaf(typid) {
        return Ok(None);
    }
    let (len, byval) = lsyscache::get_typlenbyval(typid)?;
    Ok(match (byval, len) {
        (true, l @ (1 | 2 | 4 | 8)) => Some(l as u8),
        _ => None,
    })
}

/// `word_width`, and, for a key's own type read in place of a polymorphic operator input (`typed`: `KeyReq::typed`), also
/// a type whose canonical form is one by-value bytes leaf (typesem `bits_form`: an enum, whose `=` is its label oid's;
/// enum_eq keys it by its word). An untyped request keeps `word_width`: a pseudo-type has no width.
fn word_width_of(typid: Oid, typed: bool) -> PgResult<Option<u8>> {
    if let Some(w) = word_width(typid)? {
        return Ok(Some(w));
    }
    if !typed || !super::bits_form(typid)? {
        return Ok(None);
    }
    let (len, byval) = lsyscache::get_typlenbyval(typid)?;
    Ok(match (byval, len) {
        (true, l @ (1 | 2 | 4 | 8)) => Some(l as u8),
        _ => None,
    })
}

/// equivclass#53 (e2e keysem_langint_alias): the word width of key type `t`, which has no canonical form of its own (a
/// user base type `like` a built-in), when its resolved procs are one built-in type `b`'s: `eq` and `rhs_eq` are `b`'s own
/// `=`, both hash procs `b`'s own hash, `b`'s canonical form is its word (`word_width`) and `t` stores as `b` (one typlen,
/// by value). PostgreSQL then compares and hashes `t`'s datum as `b`'s, so equal words are equal values and the hash procs
/// agree with that equality (D2642), as `bits_decide` reads a keyed table's key. `None` for any other (`By::Fmgr`).
fn alias_word(mcx: mcx::Mcx<'_>, procs: &Procs, t: Oid) -> PgResult<Option<u8>> {
    if procs.rhs_eq != procs.eq || ![procs.eq, procs.lhs_hash, procs.rhs_hash].into_iter().all(builtin) {
        return Ok(None);
    }
    let (_, args) = lsyscache::get_func_signature(mcx, procs.eq)?;
    let (Some(&b), Some(&b2)) = (args.first(), args.get(1)) else { return Ok(None) };
    if args.len() != 2 || b != b2 || lsyscache::get_typlenbyval(b)? != lsyscache::get_typlenbyval(t)? {
        return Ok(None);
    }
    let Some(w) = word_width(b)? else { return Ok(None) };
    let own = eq_form_of(mcx, procs.eq, b)? == EqForm::Own && hash_own(mcx, procs.lhs_hash, b)? && hash_own(mcx, procs.rhs_hash, b)?;
    Ok(own.then_some(w))
}

/// The signed integer types of PostgreSQL's hash `integer_ops` family (one hash for every width: `hashint8` agrees with
/// `hashint4` on a value both hold).
fn int_family(typid: Oid) -> bool {
    matches!(typid, 20 | 21 | 23)
}

/// PostgreSQL's hash `integer_ops` family (pg_opfamily.dat) and its equality strategy (`HTEqualStrategyNumber`).
const INTEGER_HASH_FAMILY: Oid = 1977;
const HT_EQUAL: i16 = 1;

/// D2640: whether a word's bits stand for resolved equality procs `eq` (over `lt`, `rt`) and `rhs_eq` (over `rt`, `rt`):
/// each the type's own `=` when `lt == rt`, and across integer widths hash integer_ops' own `=` for the pair. Any other
/// procedure decides alone (a user operator over int4ne / hashint4 finds nothing in PostgreSQL's table). Asked for a user
/// operator only (`resolve`); a type cache or family the catalog cannot read keeps the bits, as before.
fn word_eq(mcx: mcx::Mcx<'_>, eq: Oid, rhs_eq: Oid, lt: Oid, rt: Oid) -> PgResult<bool> {
    let own = |f: Oid, t: Oid| eq_form_of(mcx, f, t).map_or(true, |e| e == EqForm::Own);
    if !own(rhs_eq, rt) {
        return Ok(false);
    }
    if lt == rt {
        return Ok(own(eq, lt));
    }
    if !(int_family(lt) && int_family(rt)) {
        return Ok(false);
    }
    let member = lsyscache::get_opfamily_member(INTEGER_HASH_FAMILY, lt, rt, HT_EQUAL).unwrap_or(0);
    Ok(member == 0 || fmgr_proc(mcx, lsyscache::get_opcode(member)?)? == eq)
}

/// The key-semantics entry (PLAN-v5-keysem §2.2): `req`'s semantics as PostgreSQL's executor resolves them, read now (a
/// lowering's recording holds what they read, C3). Slice 1 answers `KeyUse::Hash` and `KeyUse::Sort`; `KeyUse::Eq` and `KeyUse::Merge`
/// arrive with the slices that move their consumers (4 and 5). A request with its key's types (`KeyReq::typed`) reads
/// them where the operator's own are polymorphic (`concrete`). The one way in: a consumer outside typesem asks this,
/// never the catalog (rule RK). `Ok(Err)`: the coordinate a refusal names; `Err`: a catalog read failed.
pub fn resolve(mcx: mcx::Mcx<'_>, req: &KeyReq) -> PgResult<Result<KeySem, TypeClass>> {
    let (op, coll, use_) = (req.op, req.coll, req.use_);
    debug_assert!(matches!(use_, KeyUse::Hash | KeyUse::Sort), "typesem answers KeyUse::Hash and KeyUse::Sort");
    if use_ == KeyUse::Sort {
        return sort_key(mcx, req);
    }
    let Some((lhs_hash, rhs_hash)) = lsyscache::get_op_hash_functions(op)? else { return Ok(Err(TypeClass::NoHash(op))) };
    let eq = lsyscache::get_opcode(op)?;
    if eq == 0 {
        return Ok(Err(TypeClass::Unknown(op)));
    }
    let (eq, lhs_hash, rhs_hash) = (fmgr_proc(mcx, eq)?, fmgr_proc(mcx, lhs_hash)?, fmgr_proc(mcx, rhs_hash)?);
    let (dl, dr) = lsyscache::op_input_types(op)?;
    let (lt, rt) = (concrete(dl, req.lt), concrete(dr, req.rt));
    let raise_hash = coll == 0 && [lhs_hash, rhs_hash].iter().any(|&f| types_core::quiet::hash_needs_collation(f));
    if !raise_hash && indeterminate(coll, &[lt, rt])? {
        return Ok(Err(TypeClass::Indeterminate(op)));
    }
    let rhs_eq = match lsyscache::get_compatible_hash_operators(op, false, true)? {
        Some((_, r)) if r != 0 => fmgr_proc(mcx, lsyscache::get_opcode(r)?)?,
        _ => return Ok(Err(TypeClass::NoHash(op))),
    };
    let procs = Procs { eq, lhs_hash, rhs_hash, rhs_eq, cmp: 0 };
    // the procs decide, not the operator's oid: a user operator over built-in procs (CREATE OPERATOR = (i8a, i8a) with
    // int8eq in integer_ops) is those procs
    let all_builtin = [eq, lhs_hash, rhs_hash, rhs_eq].into_iter().all(builtin);
    let det = super::coll_bytes(coll)?;
    let by = if !all_builtin || !det {
        By::Fmgr
    } else if let (Some(lw), Some(rw)) = (word_width_of(lt, lt != dl)?, word_width_of(rt, rt != dr)?) {
        if builtin(op) || word_eq(mcx, eq, rhs_eq, lt, rt)? { By::Word { lw, rw, signed: false } } else { By::Fmgr }
    } else if lt == rt {
        // D2637: the type's form stands for its own `=` alone; byteaeq over a varlena (a user family of byte comparators,
        // V4R-508) keys by the payload bytes, bytea's form; any other built-in procedure by PostgreSQL's functions (a type
        // cache the catalog cannot read keeps the type's form, as before). A built-in operator over one type is that
        // type's own `=` (the type cache is read for a user operator only: a crate rig installs no syscache)
        match super::form(lt)? {
            Ok((id, _)) if builtin(op) => By::Canonical(id),
            Ok((id, _)) => match (eq_form_of(mcx, eq, lt), eq_form_of(mcx, rhs_eq, rt)) {
                (Ok(EqForm::Bytes), Ok(EqForm::Bytes)) => match super::form(types_core::catalog::BYTEAOID)? {
                    Ok((b, _)) => By::Canonical(b),
                    Err(_) => By::Fmgr,
                },
                (Ok(EqForm::Other | EqForm::Bytes | EqForm::User), _) | (_, Ok(EqForm::Other | EqForm::Bytes | EqForm::User)) => By::Fmgr,
                // D2642: an image keys equal values as one; the table finds them only when the hash procs are the
                // type's own (a hashvarlena wrapper over numeric's = hashes 1.0 and 1.00 apart)
                _ if !hash_own(mcx, lhs_hash, lt)? || !hash_own(mcx, rhs_hash, rt)? => By::Fmgr,
                _ => By::Canonical(id),
            },
            // equivclass#53: a type with no form of its own over one built-in type's own procs keys by its word as that
            // type does (`alias_word`)
            Err(_) => match alias_word(mcx, &procs, lt)? {
                Some(w) => By::Word { lw: w, rw: w, signed: false },
                None => By::Fmgr,
            },
        }
    } else {
        By::Fmgr
    };
    let linear = builtin(op) || !all_builtin || !matches!(by, By::Fmgr);
    Ok(Ok(KeySem { op, lt, rt, coll, by, procs, native: all_builtin && det, desc: false, raise_hash, linear }))
}

/// Operator input type `declared` as the key reads it: `given` (a `KeyReq::typed` type, non-zero) in place of a
/// polymorphic pseudo-type (parse_coerce.c `IsPolymorphicType`, and `record`, which record_eq / btrecordcmp take for every
/// composite), else `declared`.
const fn concrete(declared: Oid, given: Oid) -> Oid {
    use types_core::catalog::*;
    let poly = matches!(
        declared,
        ANYELEMENTOID | ANYARRAYOID | ANYNONARRAYOID | ANYENUMOID | ANYRANGEOID | ANYMULTIRANGEOID | ANYCOMPATIBLEOID
            | ANYCOMPATIBLEARRAYOID | ANYCOMPATIBLENONARRAYOID | ANYCOMPATIBLERANGEOID | ANYCOMPATIBLEMULTIRANGEOID | RECORDOID
    );
    if poly && given != 0 { given } else { declared }
}

/// D2036: the hash functions PostgreSQL's hash join calls on each input of `op` under input collation `coll`
/// (ExecInitHashJoin's `get_op_hash_functions`; a user `LANGUAGE internal` proc read as the built-in it names).
/// `resolve`'s hashed half: no compatible right-hand equality is asked for, since a hash join never calls one
/// (equivclass's non-commutable `=(int8, int8alias1)` in integer_ops has none). `Ok(Err)`: no hash support, or an
/// indeterminate collation that no hash proc raises on.
pub fn hash_procs(mcx: mcx::Mcx<'_>, op: Oid, coll: Oid) -> PgResult<Result<(Oid, Oid), TypeClass>> {
    let Some((l, r)) = lsyscache::get_op_hash_functions(op)? else { return Ok(Err(TypeClass::NoHash(op))) };
    let (l, r) = (fmgr_proc(mcx, l)?, fmgr_proc(mcx, r)?);
    let (lt, rt) = lsyscache::op_input_types(op)?;
    let raise_hash = coll == 0 && [l, r].iter().any(|&f| types_core::quiet::hash_needs_collation(f));
    if !raise_hash && indeterminate(coll, &[lt, rt])? {
        return Ok(Err(TypeClass::Indeterminate(op)));
    }
    Ok(Ok((l, r)))
}

/// `resolve` for `KeyUse::Sort`: the ordering operator's btree family, comparator and equality (see the module header).
/// The family's procs are looked up under its declared input type (pg_amproc lists array_ops' support 1 under
/// anyarray); the key's collatability, word and order read its concrete type (`KeyReq::typed`).
fn sort_key(mcx: mcx::Mcx<'_>, req: &KeyReq) -> PgResult<Result<KeySem, TypeClass>> {
    let (op, coll) = (req.op, req.coll);
    let Some((family, t, cmptype)) = lsyscache::get_ordering_op_properties(op)? else { return Ok(Err(TypeClass::NoOrder(op))) };
    let kt = concrete(t, req.lt);
    if indeterminate(coll, &[kt])? {
        return Ok(Err(TypeClass::Indeterminate(op)));
    }
    let cmp = lsyscache::get_opfamily_proc(family, t, t, 1)?;
    if cmp == 0 {
        return Ok(Err(TypeClass::NoOrder(op)));
    }
    let eq_op = match lsyscache::get_equality_op_for_ordering_op(op)? {
        Some((e, _)) if e != 0 => e,
        _ => return Ok(Err(TypeClass::NoOrder(op))),
    };
    let eq = lsyscache::get_opcode(eq_op)?;
    if eq == 0 {
        return Ok(Err(TypeClass::Unknown(op)));
    }
    let (cmp, eq) = (fmgr_proc(mcx, cmp)?, fmgr_proc(mcx, eq)?);
    let native = builtin(cmp) && builtin(eq) && super::coll_bytes(coll)?;
    let by = match (native, word_width(kt)?) {
        (true, Some(w)) => match word_order(mcx, cmp, kt)? {
            Some(signed) => By::Word { lw: w, rw: w, signed },
            None => By::Fmgr,
        },
        _ => By::Fmgr,
    };
    let procs = Procs { eq, lhs_hash: 0, rhs_hash: 0, rhs_eq: eq, cmp };
    let desc = cmptype == lsyscache::COMPARE_GT;
    Ok(Ok(KeySem { op, lt: kt, rt: kt, coll, by, procs, native, desc, raise_hash: false, linear: true }))
}

/// D2598: `coll` is InvalidOid and one of `types` is collatable (its pg_type `typcollation` is set: text, varchar,
/// bpchar, name, a domain over one, an array of one).
fn indeterminate(coll: Oid, types: &[Oid]) -> PgResult<bool> {
    if coll != 0 {
        return Ok(false);
    }
    for &t in types {
        if lsyscache::type_is_collatable(t)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How comparator `cmp` orders words of type `t`: the order of its own first input type (canon `word_order`), when that
/// type stores as `t` does (a user class over a built-in comparator orders as the comparator). `None`: byte order is not
/// its order.
fn word_order(mcx: mcx::Mcx<'_>, cmp: Oid, t: Oid) -> PgResult<Option<bool>> {
    let (_, args) = lsyscache::get_func_signature(mcx, cmp)?;
    let Some(&a) = args.first() else { return Ok(None) };
    if lsyscache::get_typlenbyval(a)? != lsyscache::get_typlenbyval(t)? {
        return Ok(None);
    }
    Ok(super::word_order(a))
}

/// Whether a sorted key of type `typid` orders its datum words two's-complement, exactly `order_image`'s Signed set:
/// int2, int4, int8, date, time, timestamp, timestamptz, money. A reader that decodes such a key's word image as
/// `word ^ 1 << 63` asks this. A domain over one of them answers false on purpose: resolving the key through its base
/// type's family (`KeyReq::sort`) would widen the set.
pub fn image_signed(typid: Oid) -> bool {
    super::word_order(typid) == Some(true)
}

/// fmgroids.h's names of the default btree comparators that order a value by its payload bytes: `byteacmp` (memcmp),
/// `btnamecmp` and `bttextcmp` (varstr_cmp: memcmp under a collation that orders by bytes), `bpcharcmp` (the same after its
/// trailing-space trim) (PostgreSQL 18.6 pg_proc.dat; text and varchar share bttextcmp).
const F_BYTEACMP: Oid = 1954;
const F_BTNAMECMP: Oid = 359;
const F_BTTEXTCMP: Oid = 360;
const F_BPCHARCMP: Oid = 1078;

/// Whether a key whose type's image is its payload bytes (`bytes_image`) orders by those bytes (the order image is the
/// bytes' prefix, the tail the bytes): its default btree comparator `cmp` is a bytes comparator. inet and cidr hash and
/// compare equal by their bytes, but network_cmp orders family, masklen, then bits (D387 / D771), so their comparator says
/// no; so does any other bytes-image type whose comparator is not a bytes compare (fail closed: its comparator's order).
/// The caller passes a domain's base comparator (its type row carries it; D2347).
pub const fn orders_by_bytes(bytes_image: bool, cmp: Oid) -> bool {
    bytes_image && matches!(cmp, F_BYTEACMP | F_BTNAMECMP | F_BTTEXTCMP | F_BPCHARCMP)
}

/// How a key of type `typid` whose image is its datum's own bits in one word orders that word: `Some(true)` signed
/// (`image_signed`'s set), `Some(false)` unsigned (bool, "char", oid, xid8, pg_lsn), `None` when the word does not order
/// as the type does (typesem `word_order`'s answer). topn's bare two-pass pass 1 reads its key's word order by it.
pub fn word_image_order(typid: Oid) -> Option<bool> {
    super::word_order(typid)
}

/// Whether a by-value key's datum word, read as an i64, orders as the type (a zone minimum then bounds it): its order image
/// (typesem `order_image`) is signed at any width (sign-extended), or unsigned narrower than a word (zero-extended: bool,
/// "char", oid). An unsigned 8-byte image (xid8, pg_lsn) is its word flipped, and a by-reference or imageless type has none.
/// The sort zone walks read their key by it.
pub fn word_orders_as_i64(typid: Oid, byval: bool, typlen: i16) -> bool {
    match super::order_image(typid) {
        Some(super::OrderBy::Signed) => byval,
        Some(super::OrderBy::Unsigned) => byval && (1..8).contains(&typlen),
        _ => false,
    }
}

/// Prototype 6, keyed tables: whether a by-value key of type `typid` compared under `op` is decided by its datum's bits
/// (decompose agg.rs `key_kind` images a type the facts table does not know as its bits, `opaque`). True when canon.rs's
/// form of the type is its bytes (an enum), or every support proc is built in and the equality proc's own input type is
/// a by-value bytes type of the same storage (a user type over int8eq / hashint8). False otherwise: PostgreSQL calls a
/// user function (`By::Fmgr`), equal values need not be equal bits, and a keyed table has no fmgr confirm. A by-reference
/// type answers true (canon.rs's form decides it on its own road, refusing a user equality).
pub fn bits_decide(mcx: mcx::Mcx<'_>, op: Oid, coll: Oid, typid: Oid) -> PgResult<bool> {
    let (len, byval) = lsyscache::get_typlenbyval(typid)?;
    if !byval || super::bits_form(typid)? {
        return Ok(true);
    }
    let sem = match resolve(mcx, &KeyReq::hash(op, coll))? {
        Ok(sem) if sem.native && !sem.raise_hash => sem,
        _ => return Ok(false),
    };
    let (_, args) = lsyscache::get_func_signature(mcx, sem.procs.eq)?;
    let Some(&a) = args.first() else { return Ok(false) };
    Ok(super::bytes_leaf(a) && lsyscache::get_typlenbyval(a)? == (len, byval))
}

/// Prototype 6, keyed tables: the operator a by-value key of type `typid` interns under, when its bits do not decide its
/// default `=` (typcache's `eq_opr`, the parser's grouping and DISTINCT operator): the key then keys by the representative
/// of its class (lowerv4 keyed/hashagg_rec.rs `intern_plan`). `None` when the bits decide (`bits_decide`), or when the
/// operator has no hash support (nothing can intern it; decompose's fence refuses it).
pub fn intern_op(mcx: mcx::Mcx<'_>, typid: Oid, coll: Oid) -> PgResult<Option<Oid>> {
    let op = typcache::lookup_type_cache(typid, typcache::TYPECACHE_EQ_OPR)?.eq_opr();
    if op == 0 {
        return Ok(None);
    }
    if bits_decide(mcx, op, coll, typid)? {
        return Ok(None);
    }
    Ok(resolve(mcx, &KeyReq::hash(op, coll))?.ok().filter(|s| !s.raise_hash).map(|_| op))
}

/// Whether the catalog seams `resolve` reads are installed (a crate rig over the reference .dat files has none).
pub fn available() -> bool {
    syscache_seams::lookup_pg_operator_shape::is_installed() && syscache_seams::lookup_pg_amop_members_by_operator::is_installed()
}

#[cfg(test)]
mod tests {
    /// `KeyReq`'s constructors name their use, operator and collation (`resolve` reads nothing else in slice 1).
    #[test]
    fn key_req_constructors_name_their_use() {
        use super::{KeyReq, KeyUse};
        let h = KeyReq::hash(96, 0);
        assert_eq!((h.use_, h.op, h.coll), (KeyUse::Hash, 96, 0));
        let s = KeyReq::sort(97).under(100);
        assert_eq!((s.use_, s.op, s.coll), (KeyUse::Sort, 97, 100));
        // no types given: the operator's own (0), and `typed` changes the types alone
        assert_eq!((h.lt, h.rt, s.lt, s.rt), (0, 0, 0, 0));
        let t = KeyReq::hash(1070, 0).typed(1007, 1007);
        assert_eq!((t.use_, t.op, t.coll, t.lt, t.rt), (KeyUse::Hash, 1070, 0, 1007, 1007));
    }

    /// K3: a given type stands in for a polymorphic operator input type (and record's) only; a concrete input type and
    /// an absent given type (0) keep the declared one.
    #[test]
    fn concrete_reads_a_given_type_for_a_polymorphic_input_only() {
        use types_core::catalog::*;
        for p in [ANYELEMENTOID, ANYARRAYOID, ANYNONARRAYOID, ANYENUMOID, ANYRANGEOID, ANYMULTIRANGEOID, ANYCOMPATIBLEOID,
            ANYCOMPATIBLEARRAYOID, ANYCOMPATIBLENONARRAYOID, ANYCOMPATIBLERANGEOID, ANYCOMPATIBLEMULTIRANGEOID, RECORDOID] {
            assert_eq!(super::concrete(p, 3904), 3904, "{p}");
            assert_eq!(super::concrete(p, 0), p, "{p}");
        }
        for c in [INT4OID, INT8OID, TEXTOID, NUMERICOID, RECORDARRAYOID, ANYOID] {
            assert_eq!(super::concrete(c, 3904), c, "{c}");
        }
    }

    /// Every type `word_order` names keys as a word (a bytes leaf), so `KeyUse::Sort`'s `By::Word` never stands for a
    /// type whose equality is not its bytes; enums, floats and varlenas are not named (their byte order is not their order).
    #[test]
    fn word_order_types_are_bytes_leaves() {
        use types_core::catalog::*;
        for t in [INT2OID, INT4OID, INT8OID, DATEOID, TIMEOID, TIMESTAMPOID, TIMESTAMPTZOID, CASHOID, BOOLOID, CHAROID, OIDOID, XID8OID, LSNOID] {
            assert!(super::super::word_order(t).is_some() && super::super::bytes_leaf(t), "{t}");
        }
        for t in [FLOAT4OID, FLOAT8OID, NUMERICOID, TEXTOID, BPCHAROID, UUIDOID, INTERVALOID, TIMETZOID] {
            assert_eq!(super::super::word_order(t), None, "{t}");
        }
        assert_eq!(super::super::word_order(OIDOID), Some(false));
        assert_eq!(super::super::word_order(INT4OID), Some(true));
    }

    #[test]
    fn image_signed_is_order_image_signed() {
        use types_core::catalog::*;
        for t in 0..20000 {
            assert_eq!(super::image_signed(t), super::super::order_image(t) == Some(super::super::OrderBy::Signed), "{t}");
        }
        for t in [INT2OID, INT4OID, INT8OID, DATEOID, TIMEOID, TIMESTAMPOID, TIMESTAMPTZOID, CASHOID] {
            assert!(super::image_signed(t), "{t}");
        }
    }
}
