//! The catalog facts a compile reads once (docs/gravity/expr-compiler.md §1.3): strictness,
//! volatility, parallel safety, btree procs, opclass, domain constraints, text properties, the
//! direct shim. `PgCatalog` answers from `fmgr_core` (builtins without a syscache), `lsyscache`,
//! `typcache` and `gravity_fn`; a test catalog answers from tables.

use gravity_fn::{Shim, Tier};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::Node;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncFacts {
    pub strict: bool,
    pub retset: bool,
    pub nargs: i16,
    /// `pg_proc.provolatile` (`b'i'`/`b's'`/`b'v'`), `None` when the catalog is not reachable.
    pub volatility: Option<u8>,
    /// `pg_proc.proparallel` (`b's'`/`b'r'`/`b'u'`), `None` as above.
    pub parallel: Option<u8>,
    /// A polymorphic, `any`, `record` or `internal` argument, or VARIADIC `any`: the body reads its calling node.
    pub node_arg: bool,
    /// `pg_proc.proleakproof`: PG promises the function raises nothing that depends on its arguments; `None` when the
    /// catalog is not reachable (a caller takes `None` as "may raise").
    pub leakproof: Option<bool>,
    /// `pg_proc.prolang` (12 internal, 13 C, 14 SQL, else a PL); `None` when the catalog is not reachable (props' srf.pl by
    /// language, M5 Decision 23).
    pub prolang: Option<Oid>,
    /// D1233: the builtin row fmgr resolved the call to (`FmgrInfo.fn_body`: an internal alias's prosrc's row), else 0.
    pub body: Oid,
}

/// The first oid a user object takes (transam.h `FirstNormalObjectId`): below it, a function is initdb's own.
const FIRST_NORMAL_OBJECT_ID: Oid = types_core::catalog::FirstNormalObjectId;

/// Item 6: the effect tests gravity_expr makes of a `pg_proc` row live here, beside the row (R1's home for this crate:
/// gravity_expr sits below decompose, so it cannot ask `gravity_decompose::effects`, which asks the same of `pg_proc`).
impl FuncFacts {
    /// A call site's call bit (`Call::volatile`, the per-row form): `provolatile` is VOLATILE, or the catalog cannot
    /// answer a function-manager call (fail closed); a direct shim the catalog cannot describe is pure over its arguments.
    pub fn call_acts(&self, fmgr: bool) -> bool {
        // an unknown direct shim is a Gravity-native function, so it is not "unknown" under construct #1 (acting)
        !matches!(self.volatility, Some(b'i') | Some(b's')) && (fmgr || self.volatility.is_some())
    }

    /// `provolatile` is IMMUTABLE: a computation over the arguments alone.
    pub fn immutable(&self) -> bool {
        self.volatility == Some(b'i')
    }

    /// `provolatile` is VOLATILE (`false` when the catalog is not reachable: a caller that must fail closed asks
    /// `call_acts`, or treats the row it could not read as volatile itself).
    pub fn volatile(&self) -> bool {
        self.volatility == Some(b'v')
    }

    /// Function `funcid` is initdb's own (below `FirstNormalObjectId`, not 0).
    pub fn builtin(funcid: Oid) -> bool {
        funcid != 0 && funcid < FIRST_NORMAL_OBJECT_ID
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeFacts {
    pub len: i16,
    pub byval: bool,
    pub align: i8,
    pub typtype: i8,
    pub collation: Oid,
    pub elem: Oid,
}

impl TypeFacts {
    pub fn is_varlena(&self) -> bool {
        self.len == -1
    }
    pub fn is_domain(&self) -> bool {
        self.typtype == b'd' as i8
    }
}

/// An array element type's `pg_type` row (`nodes::array`; item 5: one definition of each fact, in this crate's catalog).
#[derive(Clone, Copy, Debug)]
pub struct ElemFacts {
    pub typ: Oid,
    pub len: i16,
    pub byval: bool,
    pub align: i8,
}

/// What a function-manager call's calling node tells the body (`nodes::State::call_facts`; item 5: defined beside the
/// `pg_proc` row it is read from).
#[derive(Clone, Debug)]
pub struct CallFacts {
    pub rettype: Oid,
    pub variadic: bool,
    /// `FuncFacts::node_arg`: the body reads its calling node, which a carrier does not replace.
    pub node_arg: bool,
    pub argtypes: Vec<Oid>,
    /// `get_fn_expr_arg_stable`'s bits: an unknown-typed `Const` argument only (never a typed literal: a re-bound word).
    pub stable: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoFacts {
    pub input: Oid,
    pub ioparam: Oid,
    pub output: Oid,
    pub varlena: bool,
}

pub enum DomainConstraint<'mcx> {
    NotNull,
    Check { name: &'static str, expr: Node<'mcx> },
}

pub struct DomainFacts<'mcx> {
    pub typlen: i16,
    pub constraints: Vec<DomainConstraint<'mcx>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextFacts {
    pub deterministic: bool,
    pub equality_is_bytes: bool,
    pub order_is_bytes: bool,
    /// pg_locale's `ctype_is_c`: case maps and character classes are ASCII's (libc C / POSIX, a C-locale database
    /// default), so an ASCII fold is ILIKE's (D2680).
    pub ctype_is_c: bool,
}

/// `Catalog::support`'s questions (lowerv4 guard/user_fn.rs): each is one lsyscache / typcache lookup PostgreSQL's
/// executor makes to find a support function it will call. The answer is one OID, 0 = none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Support {
    /// Btree support function `num` of the family a sort by `op` compares in: `get_ordering_op_properties`, then
    /// `get_opfamily_proc(family, opcintype, opcintype, num)` (PrepareSortSupportFromOrderingOp).
    Sort { op: Oid, num: i16 },
    /// Btree support function `num` of merge family `family` at `op`'s member types (`get_op_opfamily_properties`,
    /// then `get_opfamily_proc`: MJExamineQuals).
    Merge { op: Oid, family: Oid, num: i16 },
    /// `get_op_hash_functions(op)`: the left input's hash function, or the right input's (`rhs`).
    Hash { op: Oid, rhs: bool },
    /// `get_equality_op_for_ordering_op(op)` (IncrementalSort's presorted keys).
    EqOfSort(Oid),
    /// `get_compatible_hash_operators(op)`'s right-hand `=` (a hashed SubPlan's table equality).
    HashRhsEq(Oid),
    /// The type cache's `TYPECACHE_HASH_PROC`, or `TYPECACHE_HASH_EXTENDED_PROC` (`ext`).
    TypeHash { typid: Oid, ext: bool },
    /// A range type's comparison function (its `rngsubopc`'s btree FUNCTION 1, `rng_cmp_proc_finfo`).
    RangeCmp(Oid),
    /// A range type's `rngcanonical` (0: none).
    RangeCanonical(Oid),
    /// A range type's subtype.
    RangeSub(Oid),
    /// A multirange type's range type.
    MultirangeRange(Oid),
    /// A composite type's attribute count (through a domain; 0: no composite).
    Natts(Oid),
    /// Attribute `attno`'s type of a composite type (through a domain; 0: a dropped one).
    Field { typid: Oid, attno: i16 },
}

pub trait Catalog<'mcx> {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts>;
    fn opcode(&self, opno: Oid) -> PgResult<Oid>;
    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)>;
    /// `get_opfamily_proc(opfamily, left, right, BTORDER_PROC)`.
    fn btree_cmp_proc(&self, opfamily: Oid, left: Oid, right: Oid) -> PgResult<Oid>;
    /// The opclass's opfamily and input type.
    fn opclass(&self, opclass: Oid) -> PgResult<(Oid, Oid)>;
    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts>;
    /// `TYPECACHE_CMP_PROC`: the type's btree comparison function, 0 when none.
    fn type_cmp_proc(&self, typid: Oid) -> PgResult<Oid>;
    fn base_type(&self, typid: Oid, typmod: &mut i32) -> PgResult<Oid>;
    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>>;
    fn io(&self, typid: Oid) -> PgResult<IoFacts>;
    fn text(&self, collation: Oid) -> PgResult<TextFacts>;
    fn tier(&self, funcid: Oid) -> Tier;
    fn shim(&self, funcid: Oid) -> Option<&'static Shim>;
    /// An enum type's members as `(oid, enumsortorder)` (`pg_enum`); `None` where the catalog has no such face (a key over the type refuses by name).
    fn enum_members(&self, _typid: Oid) -> Option<Vec<(Oid, f32)>> {
        None
    }
    /// The btree support function 1 (`BTORDER_PROC`) that orders as sort operator `opno` does: its opfamily and input type
    /// by `get_ordering_op_properties`, then `get_opfamily_proc`; 0 when `opno` orders no btree opfamily, or where the
    /// catalog has no such face.
    fn ordering_cmp_proc(&self, _opno: Oid) -> PgResult<Oid> {
        Ok(0)
    }
    /// `TYPECACHE_EQ_OPR`: the type's default btree (else hash) equality operator, as `lookup_type_cache` finds it (a
    /// composite's `record_eq` `=`); 0 when none, or where the catalog has no such face.
    fn type_eq_opr(&self, _typid: Oid) -> PgResult<Oid> {
        Ok(0)
    }
    /// D1398: aggregate `aggfnoid`'s `pg_aggregate.aggfinalfn` (0: none); `None` where the catalog cannot say.
    fn agg_finalfn(&self, _aggfnoid: Oid) -> Option<Oid> {
        None
    }
    /// D1236 (MERGE R3, V4R-487): regular expression `pattern` (a Const's bytes) compiles under `collation` with the ARE
    /// flags of `~` (`icase`: `~*`), as the first row's match would compile it; false where the catalog cannot say.
    fn regex_compiles(&self, _pattern: &[u8], _icase: bool, _collation: Oid) -> bool {
        false
    }
    /// A support-function lookup (`Support`); an error where the catalog has no such face: a caller refuses.
    fn support(&self, q: Support) -> PgResult<Oid> {
        Err(Box::new(types_error::PgError::error(format!("no support face for {q:?}")))) // alloc: Cold
    }
    /// How a hashed key compares and hashes under equality operator `op` and collation `coll`: typesem's answer
    /// (`gravity_fn::typesem::resolve(.., &KeyReq::hash(op, coll))`, the one home of that decision); an error where the
    /// catalog has no such face: a caller refuses.
    fn hash_key(&self, op: Oid, coll: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        Err(Box::new(types_error::PgError::error(format!("no hash key face for operator {op} collation {coll}")))) // alloc: Cold
    }
    /// `hash_key` for a key whose two sides have types `lt` / `rt` as the plan types them (KeySem `KeyReq::typed`: read
    /// only in place of a polymorphic operator input type). A catalog with no typed face answers as `hash_key`.
    fn hash_key_typed(&self, op: Oid, coll: Oid, lt: Oid, rt: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        let _ = (lt, rt);
        self.hash_key(op, coll)
    }
}

/// The pgrust catalog: builtins through `fmgr_core` (no syscache needed), everything else
/// through `lsyscache` / `typcache`; text facts from `gravity_fn::collation`.
pub struct PgCatalog;

impl<'mcx> Catalog<'mcx> for PgCatalog {
    fn func(&self, funcid: Oid) -> PgResult<FuncFacts> {
        let fi = fmgr_core::fmgr_info(funcid)?;
        let catalog = syscache_seams::lookup_pg_proc_shape::is_installed();
        let volatility = if catalog { lsyscache::func_volatile(funcid).ok().map(|v| v as u8) } else { None };
        let parallel = if catalog { lsyscache::func_parallel(funcid).ok().map(|v| v as u8) } else { None };
        let leakproof = if catalog { lsyscache::get_func_leakproof(funcid).ok() } else { None };
        let node_arg = !catalog || !syscache_seams::lookup_pg_proc_signature::is_installed() || {
            let mut cx = mcx::MemoryContext::new_bump("gravity_expr signature"); // alloc: Cold
            let sig = lsyscache::get_func_signature(cx.mcx(), funcid).map(|(_, a)| a.iter().any(|&t| gravity_fn::typesem::expr::node_arg_type(t))).unwrap_or(true);
            cx.reset();
            sig || lsyscache::get_func_variadictype(funcid).is_ok_and(|v| v == 2276)
        };
        let prolang = if catalog { syscache_seams::lookup_pg_proc_shape::call(funcid).ok().flatten().map(|s| s.prolang) } else { None };
        Ok(FuncFacts { strict: fi.fn_strict, retset: fi.fn_retset, nargs: fi.fn_nargs, volatility, parallel, node_arg, leakproof, prolang, body: fi.fn_body })
    }

    fn opcode(&self, opno: Oid) -> PgResult<Oid> {
        lsyscache::get_opcode(opno)
    }

    fn op_input_types(&self, opno: Oid) -> PgResult<(Oid, Oid)> {
        lsyscache::op_input_types(opno)
    }

    fn btree_cmp_proc(&self, opfamily: Oid, left: Oid, right: Oid) -> PgResult<Oid> {
        gravity_fn::typesem::expr::btree_cmp_proc(opfamily, left, right)
    }

    fn ordering_cmp_proc(&self, opno: Oid) -> PgResult<Oid> {
        gravity_fn::typesem::expr::ordering_proc(opno, 1)
    }

    fn type_eq_opr(&self, typid: Oid) -> PgResult<Oid> {
        gravity_fn::typesem::keysem::of_type::eq_opr(typid)
    }

    fn agg_finalfn(&self, aggfnoid: Oid) -> Option<Oid> {
        syscache_seams::lookup_pg_aggregate_shape::is_installed().then(|| syscache_seams::lookup_pg_aggregate_shape::call(aggfnoid).ok().flatten()).flatten().map(|a| a.aggfinalfn)
    }

    fn opclass(&self, opclass: Oid) -> PgResult<(Oid, Oid)> {
        gravity_fn::typesem::expr::opclass(opclass)
    }

    fn type_facts(&self, typid: Oid) -> PgResult<TypeFacts> {
        let (len, byval, align) = gravity_fn::typesem::expr::layout(typid)?;
        let typtype = lsyscache::get_typtype(typid)?;
        let collation = gravity_fn::typesem::expr::collation(typid)?;
        let elem = lsyscache::get_element_type(typid)?;
        Ok(TypeFacts { len, byval, align, typtype, collation, elem })
    }

    fn type_cmp_proc(&self, typid: Oid) -> PgResult<Oid> {
        gravity_fn::typesem::keysem::of_type::cmp_proc(typid)
    }

    fn base_type(&self, typid: Oid, typmod: &mut i32) -> PgResult<Oid> {
        lsyscache::getBaseTypeAndTypmod(typid, typmod)
    }

    fn domain(&self, typid: Oid) -> PgResult<DomainFacts<'mcx>> {
        domain_facts(typid)
    }

    fn io(&self, typid: Oid) -> PgResult<IoFacts> {
        let (input, ioparam) = lsyscache::getTypeInputInfo(typid)?;
        let (output, varlena) = lsyscache::getTypeOutputInfo(typid)?;
        Ok(IoFacts { input, ioparam, output, varlena })
    }

    fn text(&self, collation: Oid) -> PgResult<TextFacts> {
        let f = gravity_fn::typesem::collation::Facts::of(collation)?;
        Ok(TextFacts { deterministic: f.deterministic, equality_is_bytes: f.equality_is_bytes(), order_is_bytes: f.order_is_bytes(), ctype_is_c: f.ctype_is_c })
    }

    fn tier(&self, funcid: Oid) -> Tier {
        gravity_fn::tier_of(funcid)
    }

    fn shim(&self, funcid: Oid) -> Option<&'static Shim> {
        gravity_fn::shim_of(funcid)
    }

    fn enum_members(&self, typid: Oid) -> Option<Vec<(Oid, f32)>> {
        if !pg_enum_seams::scan_enum_members::is_installed() {
            return None;
        }
        let cx = mcx::MemoryContext::new_bump("gravity_expr enum members"); // alloc: Cold
        let m = pg_enum_seams::scan_enum_members::call(cx.mcx(), typid).ok()?;
        Some(m.iter().copied().collect()) // alloc: Cold
    }

    fn regex_compiles(&self, pattern: &[u8], icase: bool, collation: Oid) -> bool {
        gravity_fn::typesem::collation::regex_compiles(pattern, icase, collation)
    }

    fn hash_key(&self, op: Oid, coll: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        let mut cx = mcx::MemoryContext::new_bump("gravity_expr hash_key"); // alloc: Cold
        let k = gravity_fn::typesem::resolve(cx.mcx(), &gravity_fn::typesem::KeyReq::hash(op, coll));
        cx.reset();
        k
    }

    fn hash_key_typed(&self, op: Oid, coll: Oid, lt: Oid, rt: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
        let mut cx = mcx::MemoryContext::new_bump("gravity_expr hash_key_typed"); // alloc: Cold
        let k = gravity_fn::typesem::keysem::resolve(cx.mcx(), &gravity_fn::typesem::KeyReq::hash(op, coll).typed(lt, rt));
        cx.reset();
        k
    }

    fn support(&self, q: Support) -> PgResult<Oid> {
        let tc = |t: Oid, flags: i32| typcache::lookup_type_cache(t, flags);
        // D3574: a composite read here is one a plan-keyed program keeps as lowered: the typcache records its relation
        // (C3, gravity_deps `Rel(typrelid)`), which ALTER TYPE invalidates
        let row = |t: Oid| -> PgResult<_> {
            let b = lsyscache::getBaseType(t)?;
            Ok(tc(b, typcache::TYPECACHE_TUPDESC)?.tupdesc())
        };
        Ok(match q {
            Support::Sort { op, num } => gravity_fn::typesem::expr::ordering_proc(op, num)?,
            Support::Merge { op, family, num } => gravity_fn::typesem::expr::merge_proc(op, family, num)?,
            Support::Hash { op, rhs } => gravity_fn::typesem::expr::hash_proc(op, rhs)?,
            Support::EqOfSort(op) => gravity_fn::typesem::expr::eq_of_sort(op)?,
            Support::HashRhsEq(op) => gravity_fn::typesem::expr::hash_rhs_eq(op)?,
            Support::TypeHash { typid, ext } => gravity_fn::typesem::keysem::of_type::hash_proc(typid, ext)?,
            Support::RangeCmp(t) => gravity_fn::typesem::keysem::of_type::range_cmp_proc(t)?,
            Support::RangeCanonical(t) => tc(t, typcache::TYPECACHE_RANGE_INFO)?.rng_canonical_finfo().fn_oid,
            Support::RangeSub(t) => lsyscache::get_range_subtype(t)?,
            Support::MultirangeRange(t) => lsyscache::get_multirange_range(t)?,
            Support::Natts(t) => row(t)?.map_or(0, |d| d.natts.max(0) as Oid),
            Support::Field { typid, attno } => row(typid)?.and_then(|d| d.attrs.get((attno as usize).checked_sub(1)?).filter(|a| !a.attisdropped).map(|a| a.atttypid)).unwrap_or(0),
        })
    }
}

leader_tls::leader_tls! {
    thread_local! {
        /// D3177: the domain constraint sets an admission read (`domains_pinned`), each domain's first read kept; `None` outside.
        static PINNED: core::cell::RefCell<Option<Vec<(Oid, typcache::DomainConstraintRef)>>> = const { core::cell::RefCell::new(None) };
    }
}

/// D3177: `f` (an admission: its key walks, its lowering, its word template and binding) reading one constraint set per
/// domain, the one its first `domain_facts` read. The type cache reloads a domain's set, its CHECK nodes new, whenever it
/// takes any pg_constraint invalidation (TypeCacheConstrCallback flags every domain), so an invalidation taken between the
/// content key's walk and the lowering (another session's CREATE TABLE .. PRIMARY KEY) left the lowering's CHECK constants
/// outside the walk's nodes ("words: an array literal outside the key's traversal", yes_or_no's IN list). PG's executor
/// likewise reads a domain's set once, at its expression's init (InitDomainConstraintRef); a changed set shows at the next
/// admission. Nested calls share the outer scope (their pins are the outer scope's: none returned). The outer scope's
/// sets are returned for the admission to keep (`DomainPins`).
pub fn domains_pinned<T>(f: impl FnOnce() -> T) -> (T, DomainPins) {
    struct Unpin;
    impl Drop for Unpin {
        fn drop(&mut self) {
            PINNED.with(|p| p.borrow_mut().take());
        }
    }
    if PINNED.with(|p| p.borrow().is_some()) {
        return (f(), DomainPins::default());
    }
    PINNED.with(|p| *p.borrow_mut() = Some(Vec::new())); // alloc: Cold
    let _unpin = Unpin;
    let r = f();
    (r, DomainPins(PINNED.with(|p| p.borrow_mut().take()).unwrap_or_default()))
}

/// D3177 (the main merge's audit): the constraint sets an admission read, kept for as long as the admission lives. Its
/// bound words carry a by-reference CHECK constant as the image's address inside the set (admit `bind_words`), and the
/// plan memo serves an admission again without reading the set: PostgreSQL keeps the plan across a pg_constraint
/// invalidation that leaves the domain alone, while the type cache reloads the set and frees the replaced one at its last
/// reference. Held here, the set outlives every execution of the admission.
#[derive(Default)]
pub struct DomainPins(Vec<(Oid, typcache::DomainConstraintRef)>);

impl DomainPins {
    /// Whether each kept set is still its domain's in the type cache (reloaded first when an invalidation flagged it):
    /// PostgreSQL's executor reads the set at each start (InitDomainConstraintRef), so a replaced one admits again. A kept
    /// set is never freed, so its identity is never another set's.
    pub fn current(&self) -> bool {
        self.0.iter().all(|(t, r)| typcache::DomainConstraintRef::init(*t).is_ok_and(|now| now.dcc_addr() == r.dcc_addr()))
    }
}

/// The domain's constraints from the type cache (the backend's `Catalog::domain`; also the lowerer's content key,
/// which must tell two constraint sets of one domain apart — ALTER DOMAIN leaves the plan tree as it was). Inside
/// `domains_pinned`, the set the scope first read (D3177), whose kept reference also keeps the set's memory: the type
/// cache frees a replaced set once its last reference goes.
pub fn domain_facts<'mcx>(typid: Oid) -> PgResult<DomainFacts<'mcx>> {
    fn facts<'mcx>(r: &typcache::DomainConstraintRef) -> DomainFacts<'mcx> {
        let mut constraints = Vec::new(); // alloc: Program
        for con in r.constraints() {
            constraints.push(match con.constrainttype {
                typcache::DomConstraintType::NotNull => DomainConstraint::NotNull,
                typcache::DomConstraintType::Check => DomainConstraint::Check { name: con.name, expr: con.check_expr.expect("a CHECK constraint carries its expression") },
            });
        }
        DomainFacts { typlen: r.typlen(), constraints }
    }
    if let Some(f) = PINNED.with(|p| p.borrow().as_ref().and_then(|v| v.iter().find(|(t, _)| *t == typid).map(|(_, r)| facts(r)))) {
        return Ok(f);
    }
    let cref = typcache::DomainConstraintRef::init(typid)?;
    let f = facts(&cref);
    PINNED.with(|p| p.borrow_mut().as_mut().map(|v| v.push((typid, cref)))); // alloc: Cold
    Ok(f)
}
