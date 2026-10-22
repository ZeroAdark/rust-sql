//! Road `key.codeif` (cards/M3.md L2, H21): KEY_CODE_IF, a CASE group key as an image in SSA over codes. Owner M3-WORDS.
//!
//! R1 (V4R row of this landing; KEYED-31): the form `CASE WHEN <pred> THEN <arm> [WHEN <pred> THEN <arm> ...] [ELSE
//! <arm>] END` whose every arm is one text code column or a constant of the CASE's type (BG-07 / KEYED-31: any number
//! of non-NULL constants) and the column at least once (no CASE operand; no ELSE is a NULL constant ELSE, keying NULL), is read here
//! (face/keyimg.rs `CodeIfMech`): `CASE WHEN p THEN col ELSE '' END`, its mirror `CASE WHEN p THEN 'none' ELSE col END`,
//! and any mix. Its WHENs are tried in order, as PostgreSQL's CASE. step/percode_guard.rs keys it: the row's image is the
//! code lane's entry Fp128 where the arm taken is the column (a NULL pred is false, as PostgreSQL's CASE), that constant's
//! Fp128 (one host hash per constant and window) where it is a constant, NULL where it is a NULL constant. Equal texts key equal
//! whichever arm made them (rule A: the fingerprint of the bytes). keyed_emit_group renders a group from its image: the
//! constant where the image is its fingerprint, else its representative code. The preds may read columns that are not
//! keys.
//!
//! R2, deferred: the image riding code.front_pair (codefront_pair, M3-LEVERS-B's), once that front is built.
//!
//! Registers: road `key.codeif` (`KEY_CODEIF`) as a `CodeIfImpl`.
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::table::keyimg::{CodeIfImpl, CodeIfMech};
use crate::ops::Table;

crate::strategies! {
    /// KEY_CODE_IF: key image in SSA over codes.
    KEY_CODEIF = 75, "key.codeif", CodeCaseKey;
}

pub(crate) const ROWS: Table = Table { strategies: &[(KEY_CODEIF, &CodeIfImpl(&CODEIF))], ..crate::ops::NONE };

struct CodeIf;
static CODEIF: CodeIf = CodeIf;

impl CodeIfMech for CodeIf {
    fn form<'a>(&self, e: Node<'a>) -> Option<[Node<'a>; 3]> {
        let c = e.as_case_expr().filter(|c| c.arg.is_none() && c.args.len() > 0 && gravity_fn::typesem::text_payload(c.casetype))?;
        let (mut col, mut k, mut null) = (None::<Node<'a>>, None::<Node<'a>>, None::<Node<'a>>);
        // KEYED-31 / BG-07: every arm (each WHEN's result, the ELSE) is the one code column or a constant of the CASE's type,
        // any number of non-NULL constants (one cached Fp128 each, percode_guard), the column at least once
        for a in arm_list(c) {
            let a = a?;
            if let Some(v) = a.as_var().filter(|v| v.varlevelsup == 0 && v.vartype == c.casetype) {
                if col.and_then(|x| x.as_var()).is_some_and(|x| (x.varno, x.varattno) != (v.varno, v.varattno)) {
                    return None;
                }
                col = col.or(Some(a));
            } else if let Some(x) = a.as_const().filter(|x| x.consttype == c.casetype) {
                if x.constisnull {
                    null = null.or(Some(a));
                } else {
                    k = k.or(Some(a));
                }
            } else {
                return None;
            }
        }
        let r = [e, col?, k.or(null)?];
        fires::fire("dict.key_codeif"); // shapecheck fire signal (REGISTRY KEYED-31): the CASE form matched
        Some(r)
    }
}

/// RD-key.codeif: the CASE of group key `e` when every arm is a constant of the CASE's text type, at least one non-NULL
/// (no CASE operand; no ELSE is a NULL constant ELSE). Its image is the chosen arm's constant Fp128, one cached row per
/// constant (dict/strategies/key_consts.rs), with no column at all.
pub(crate) fn consts<'a>(e: Node<'a>) -> Option<&'a types_nodes::primnodes::CaseExpr<'a>> {
    let c = e.as_case_expr().filter(|c| c.arg.is_none() && c.args.len() > 0 && gravity_fn::typesem::text_payload(c.casetype))?;
    let k = |a: Option<Node<'a>>| a.and_then(|a| a.as_const()).filter(|x| x.consttype == c.casetype);
    (arm_list(c).all(|a| k(a).is_some()) && arm_list(c).any(|a| k(a).is_some_and(|x| !x.constisnull))).then_some(c)
}

/// The arms of `c` in PostgreSQL's order: each WHEN's result, then the ELSE (a NULL constant when there is none); `None`
/// for a WHEN with no condition.
pub(crate) fn arms<'g, 'a>(c: &types_nodes::primnodes::CaseExpr<'a>, mcx: Mcx<'g>) -> Option<PgVec<'g, Node<'a>>> {
    let mut out: PgVec<'g, Node<'a>> = mcx::reserved_in(c.args.len() + 1, mcx);
    for a in arm_list(c) {
        out.push_in(a?);
    }
    Some(out)
}

/// `arms` read in place: each item `None` where `arms` is.
fn arm_list<'c, 'a>(c: &'c types_nodes::primnodes::CaseExpr<'a>) -> impl Iterator<Item = Option<Node<'a>>> + 'c {
    c.args.iter().map(|w| w.as_case_when().filter(|w| w.expr.is_some()).and_then(|w| w.result)).chain(core::iter::once(c.defresult))
}
