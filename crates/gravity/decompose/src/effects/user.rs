//! The one home of "a call runs user code" (`runs_user_code`): F5 (rerun.rs `Found::user_call`) and lowerv4's
//! user_fn readings ask it, so the planning and the lowering halves cannot read one call two ways (D3923: they did).
use super::{builtin_func, runs_named_code, user_code, Volatility};
use crate::catalog::typefacts::FIRST_NORMAL_OID;
use crate::catalog::Facts;

/// The one answer to "a call of `funcid` runs user code" (F5's `Found::user_call`, rerun.rs; lowerv4 user_fn's readings
/// and the side class). PostgreSQL's own functions do not: a builtin the PROCS table holds (`builtin_func`), or a SQL
/// function initdb wrote (`initdb_sql`). Every other function does: one past FIRST_NORMAL_OID (`user_code`, D2044), none
/// stored (0), a function below it the table lacks, an initdb SQL function whose row a superuser replaced, and (D3923) a
/// builtin that runs code named at run time (`runs_named_code`: pg_input_is_valid over a domain runs its CHECK's
/// functions).
pub fn runs_user_code(f: &dyn Facts, funcid: u32) -> bool {
    funcid == 0 || user_code(funcid) || runs_named_code(funcid) || builtin_func(funcid).is_none() && !initdb_sql(f, funcid)
}

/// PostgreSQL 18.6's information_schema.sql functions (LANGUAGE sql), each with the body pg_proc keeps and its
/// volatility: `_pg_expandarray`'s text (types_core quiet `EXPANDARRAY`); the others are SQL-standard bodies (RETURN /
/// BEGIN ATOMIC), whose `prosrc` is empty (the body is `prosqlbody`'s node tree).
const INITDB_SQL: &[(&str, &str, Volatility)] = &[
    ("_pg_expandarray", types_core::quiet::EXPANDARRAY.2, Volatility::Immutable),
    ("_pg_index_position", "", Volatility::Stable),
    ("_pg_truetypid", "", Volatility::Immutable),
    ("_pg_truetypmod", "", Volatility::Immutable),
    ("_pg_char_max_length", "", Volatility::Immutable),
    ("_pg_char_octet_length", "", Volatility::Immutable),
    ("_pg_numeric_precision", "", Volatility::Immutable),
    ("_pg_numeric_precision_radix", "", Volatility::Immutable),
    ("_pg_numeric_scale", "", Volatility::Immutable),
    ("_pg_datetime_precision", "", Volatility::Immutable),
    ("_pg_interval_type", "", Volatility::Immutable),
];

/// `funcid` is one of initdb's SQL functions with initdb's row (D2836): below FIRST_NORMAL_OID, and its namespace, name,
/// language, body and volatility, read now (`Facts::proc_text`, `Facts::func`), are an `INITDB_SQL` row's. A row replaced
/// with another text body, language or volatility is not; one replaced with another SQL-standard body of the same
/// volatility reads as initdb's (its node tree is not read), as the oid rule alone (`user_code`) reads it.
fn initdb_sql(f: &dyn Facts, funcid: u32) -> bool {
    let Some(vol) = (funcid < FIRST_NORMAL_OID).then(|| f.func(funcid)).flatten().map(|p| p.volatility) else { return false };
    let mut row = |nsp: &[u8], name: &[u8], lang: u32, src: &[u8]| {
        lang == types_core::quiet::SQL_LANGUAGE && nsp == b"information_schema" && INITDB_SQL.iter().any(|r| r.0.as_bytes() == name && r.1.as_bytes() == src && r.2 == vol)
    };
    f.proc_text(funcid, &mut row)
}

/// Construct C2 (the held-error licence's user-code rule, `effects::held_error`): a call of `funcid` runs user code
/// that may write: any user code (`runs_user_code`), whatever its volatility or language. A VOLATILE one writes itself,
/// a C or internal one whatever it says, and a non-VOLATILE SQL or PL/pgSQL one through a VOLATILE function its own
/// queries call: PostgreSQL's read-only SPI refuses only the function's own write commands (D4316), so an error it
/// raises may leave such a write behind, which a held error dropped where PostgreSQL never calls it would keep.
pub fn user_code_may_write(f: &dyn Facts, funcid: u32) -> bool {
    runs_user_code(f, funcid)
}

/// `code_may_write` at node `n` itself, its children apart: a function's, an operator's, IS DISTINCT FROM's and
/// NULLIF's call, and `= ANY`'s function with its hash and negator functions, of user code that may write. Fail closed:
/// a user type's I/O (a CoerceViaIO to or from a type past FIRST_NORMAL_OID, or from one the walk cannot name), a user
/// domain's cast (its CHECK may call any function), a row comparison or GREATEST / LEAST over a user type or operator,
/// and a builtin that runs its own query (`runs_own_query`) count.
pub fn node_may_write(f: &dyn Facts, n: types_nodes::Node<'_>) -> bool {
    let user = |o: u32| o >= FIRST_NORMAL_OID;
    let call = |fid: u32| fid != 0 && user_code_may_write(f, fid);
    n.as_func_expr().is_some_and(|x| call(x.funcid) || super::runs_own_query(x.funcid))
        || n.as_op_expr().is_some_and(|o| call(o.opfuncid))
        || n.as_distinct_expr().is_some_and(|o| call(o.opfuncid))
        || n.as_null_if_expr().is_some_and(|o| call(o.opfuncid))
        || n.as_scalar_array_op_expr().is_some_and(|o| [o.opfuncid, o.hashfuncid, o.negfuncid].into_iter().any(call))
        || n.as_coerce_via_io().is_some_and(|c| user(c.resulttype) || crate::catalog::expr_type::type_of(c.arg).is_none_or(user))
        || n.as_coerce_to_domain().is_some_and(|d| user(d.resulttype))
        || n.as_row_compare_expr().is_some_and(|r| r.opnos.iter().any(user))
        || n.as_min_max_expr().is_some_and(|m| user(m.minmaxtype))
}

/// `node_may_write` over expression `n` and the nodes the expression walk sees under it (`expr::kids`); a node the walk
/// cannot see counts. A SubPlan's plan is its own unit, not `n`'s code; its arguments are read.
pub fn code_may_write(f: &dyn Facts, n: types_nodes::Node<'_>) -> bool {
    crate::walk::stack_check();
    let mut any = node_may_write(f, n);
    let known = crate::expr::kids(n, &mut |c| any = any || code_may_write(f, c));
    any || !known
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{AttFact, ExprFp, IndexFact, RelFact};
    use crate::effects::{FuncProps, Parallel};
    use types_core::quiet::{EXPANDARRAY, SQL_LANGUAGE};

    /// One pg_proc row as `Facts` reads it: namespace, name, language, body, volatility.
    struct Row(&'static str, &'static str, u32, &'static str, Volatility);

    impl Facts for Row {
        fn rel(&self, _: u32) -> Option<RelFact> {
            None
        }

        fn att(&self, _: u32, _: i16) -> Option<AttFact> {
            None
        }

        fn index(&self, _: u32) -> Option<IndexFact> {
            None
        }

        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}

        fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
            None
        }

        fn func(&self, _: u32) -> Option<FuncProps> {
            Some(FuncProps { volatility: self.4, strict: true, leakproof: false, parallel: Parallel::Safe })
        }

        fn proc_text(&self, _: u32, f: &mut dyn FnMut(&[u8], &[u8], u32, &[u8]) -> bool) -> bool {
            f(self.0.as_bytes(), self.1.as_bytes(), self.2, self.3.as_bytes())
        }
    }

    /// A stand-in for PL/pgSQL's oid (initdb assigns it).
    const PLPGSQL: u32 = 13_500;
    /// PostgreSQL's ClanguageId.
    const C_LANGUAGE: u32 = 13;

    const IS: &str = "information_schema";
    const INITDB_OID: u32 = 13276;

    #[test]
    fn initdb_sql_rows_are_postgres_own() {
        assert!(!runs_user_code(&Row(IS, "_pg_expandarray", SQL_LANGUAGE, EXPANDARRAY.2, Volatility::Immutable), INITDB_OID));
        assert!(!runs_user_code(&Row(IS, "_pg_truetypid", SQL_LANGUAGE, "", Volatility::Immutable), INITDB_OID));
        assert!(!runs_user_code(&Row(IS, "_pg_index_position", SQL_LANGUAGE, "", Volatility::Stable), INITDB_OID));
        assert!(!runs_user_code(&Row(IS, "anything", SQL_LANGUAGE, "", Volatility::Volatile), 177));
    }

    #[test]
    fn replaced_or_foreign_rows_are_user_code() {
        // D3923: pg_input_is_valid (6210) and pg_input_error_info (6211) run the input function of a type named at run time
        assert!(runs_user_code(&Row(IS, "anything", SQL_LANGUAGE, "", Volatility::Stable), 6210) && runs_user_code(&Row(IS, "anything", SQL_LANGUAGE, "", Volatility::Stable), 6211));
        let expand = |nsp, src, vol| Row(nsp, "_pg_expandarray", SQL_LANGUAGE, src, vol);
        assert!(runs_user_code(&expand(IS, "SELECT $1[1], 1", Volatility::Immutable), INITDB_OID));
        assert!(runs_user_code(&expand(IS, EXPANDARRAY.2, Volatility::Volatile), INITDB_OID));
        assert!(runs_user_code(&expand("public", EXPANDARRAY.2, Volatility::Immutable), INITDB_OID));
        assert!(runs_user_code(&expand(IS, EXPANDARRAY.2, Volatility::Immutable), FIRST_NORMAL_OID));
        assert!(runs_user_code(&expand(IS, EXPANDARRAY.2, Volatility::Immutable), 0));
        assert!(runs_user_code(&Row(IS, "_pg_truetypid", 13, "", Volatility::Immutable), INITDB_OID));
        assert!(runs_user_code(&Row(IS, "_pg_truetypid", SQL_LANGUAGE, "SELECT $1.atttypid", Volatility::Immutable), INITDB_OID));
    }

    /// Construct C2 (D4316): user code may write whatever its language and volatility; PostgreSQL's own never.
    #[test]
    fn user_code_may_write_whatever_its_volatility() {
        let user = FIRST_NORMAL_OID + 1;
        let row = |lang, vol| Row("public", "f", lang, "", vol);
        assert!(user_code_may_write(&row(PLPGSQL, Volatility::Stable), user), "a STABLE PL/pgSQL function may call a VOLATILE writer");
        assert!(user_code_may_write(&row(SQL_LANGUAGE, Volatility::Immutable), user), "an IMMUTABLE SQL function too");
        assert!(user_code_may_write(&row(PLPGSQL, Volatility::Volatile), user), "a VOLATILE one may write");
        assert!(user_code_may_write(&row(C_LANGUAGE, Volatility::Stable), user), "a C function may write whatever it says");
        assert!(user_code_may_write(&row(12, Volatility::Immutable), user), "an internal-language user function too");
        assert!(!user_code_may_write(&Row(IS, "_pg_truetypid", SQL_LANGUAGE, "", Volatility::Immutable), INITDB_OID), "initdb's own");
    }
}

