//! KeySem slice 4, group's half (`KeyUse::Eq`): a grouping key compared through an equality operator, as PostgreSQL's
//! executor compares two rows' keys (execGrouping.c `execTuplesMatchPrepare` / execExpr.c `ExecBuildGroupingEqual`: the
//! operator's procedure through fmgr, for a Unique's DISTINCT ON key, a Group's or a sorted Agg's key). Two answers: whether
//! the operator is the key type's own `=` (then the type's image stands for it), and where a call of any other `=` runs.
use types_core::Oid;
use types_error::PgResult;

/// The catalog reads behind the two answers, from the lowering's catalog (the live one in the server, a fixture's in the
/// rig): `None` where it gives no answer.
pub trait EqReads {
    /// Operator `op`'s procedure (`get_opcode`).
    fn opcode(&self, op: Oid) -> Option<Oid>;
    /// `TYPECACHE_EQ_OPR`: the type's default btree (else hash) class's `=`.
    fn type_eq_opr(&self, typid: Oid) -> Option<Oid>;
    /// Procedure `proc`'s `prolang`.
    fn prolang(&self, proc: Oid) -> Option<Oid>;
}

/// Equality operator `op` over key type `typid` is the type's own `=`: its procedure is the type's equality procedure
/// (`own_proc` when the type's facts give one, else the procedure of the type cache's `eq_opr`), the two compared by the
/// procedure fmgr runs for each (a user `LANGUAGE internal` wrapper is the built-in its prosrc names, `body`). A type
/// whose cache names no `=` (no default btree or hash class: box, point) has no own equality, so no image stands for an
/// operator over it.
pub fn own_eq<R: EqReads + ?Sized>(mcx: mcx::Mcx<'_>, r: &R, op: Oid, typid: Oid, own_proc: Oid) -> PgResult<bool> {
    if op == 0 {
        return Ok(false);
    }
    let own = match own_proc {
        0 => match r.type_eq_opr(typid).filter(|&o| o != 0) {
            None => return Ok(false),
            Some(o) if o == op => return Ok(true),
            Some(o) => r.opcode(o).unwrap_or(0),
        },
        p => p,
    };
    let mine = r.opcode(op).unwrap_or(0);
    if own == 0 || mine == 0 {
        return Ok(false);
    }
    Ok(own == mine || super::fmgr_proc(mcx, own)? == super::fmgr_proc(mcx, mine)?)
}

/// A presorted group's tie image (lowerv4 ops/group/presorted.rs PM2): whether the values key type `typid`'s default equality
/// calls equal are one byte image each, so a rider that shows its rows in tie order (a collection, a user aggregate) sees
/// the one image every row of a tie carries. The type's answer is `typesem::one_image`'s (the caller checks that the key's
/// collation compares by bytes).
pub fn tie_image(typid: Oid) -> bool {
    crate::typesem::one_image(typid)
}

/// A grouping key's group image (the planner's dependent-key trim, gravity §3 #4): whether the values a key of type `typid`
/// (a domain reads as its base type) under collation `collid` groups as equal are one image each, so every row of a group
/// gives one `f(x)` and dropping `f(x)` from the grouping leaves PostgreSQL's groups. `one_image`'s types; a collatable one
/// only under a deterministic collation (its `=` compares bytes: "C", "POSIX", the database default, which PostgreSQL
/// keeps deterministic), none under no collation (PostgreSQL raises 42P22 at its first compare). False for numeric
/// (1.0 = 1.00), the floats (-0 = 0), bpchar and text under a nondeterministic collation.
pub fn group_image(typid: Oid, collid: Oid) -> PgResult<bool> {
    use crate::typesem::collation::{C_COLLATION_OID, DEFAULT_COLLATION_OID, POSIX_COLLATION_OID};
    let base = lsyscache::getBaseType(typid)?;
    if !crate::typesem::one_image(base) {
        return Ok(false);
    }
    if !lsyscache::type_is_collatable(base)? {
        return Ok(true);
    }
    Ok(match collid {
        0 => false,
        DEFAULT_COLLATION_OID | C_COLLATION_OID | POSIX_COLLATION_OID => true,
        c => lsyscache::get_collation_isdeterministic(c)?,
    })
}

/// Why a grouping key's equality call has no bound id (`eq_call`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqCallErr {
    /// The catalog gives the operator no procedure.
    NoProc,
    /// `resolved::native_key`'s reason (a comparator no fmgr table binds, a C function no worker may call ..).
    Unbound(&'static str),
}

/// The call site of equality operator `op` over key type `typid` when a grouping key compares through it (an `=` other than
/// the type's own, `own_eq` false): the operator's procedure (`get_opcode`, the function PostgreSQL's fmgr calls per
/// compared pair) bound once at lowering by `resolved::native_key`: user code (SQL, PL) sends the pipeline to the leader,
/// where its statements are served as nested ones (D3375 N7), as does a procedure no pool worker may call; the
/// id is what the run-time call (`keysem::call::equal`) takes.
pub fn eq_call<R: EqReads + ?Sized>(r: &R, op: Oid, typid: Oid) -> Result<u32, EqCallErr> {
    let proc = r.opcode(op).filter(|&f| f != 0).ok_or(EqCallErr::NoProc)?;
    crate::resolved::native_key(r.prolang(proc), proc, typid).map_err(EqCallErr::Unbound)
}
