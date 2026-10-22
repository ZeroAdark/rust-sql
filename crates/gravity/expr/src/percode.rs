//! Per-code evaluation (expr-compiler.md §7): the analysis that finds the maximal subtrees over one
//! dictionary code lane (`PerCode` roots: the code word, the `<> ''` test, a per-code table) and the
//! tier-0 fill that evaluates such a subtree once per dictionary entry into a `gravity_dict` table.

use gravity_dict::table::Halt;
use gravity_dict::{BuildError, Dict, Fault, Header, View};
use gravity_fn::{Datum, NullableDatum};
use mcx::{GrowIn, Mcx, PgVec};
use types_core::Oid;
use types_nodes::primnodes::{BoolExprType, ParamKind};

use crate::emit::{kids, EmitError, KidIter};
use crate::eval::{self, Cx, Row};
use crate::nodes::func::{self, ArgConv, Call, How};
use crate::nodes::{ConstNode, ExprNode, Island, Kids, NodeId, Program, SiteId};
use crate::views::payload_view;
use crate::Compiled;

pub const BOOLOID: Oid = 16;
#[cfg(test)]
const F_TEXTEQ: Oid = 67;
#[cfg(test)]
const F_TEXTNE: Oid = 157;
#[cfg(test)]
const F_TEXTLIKE: [Oid; 3] = [850, 1569, 1631];
#[cfg(test)]
const F_TEXTNLIKE: [Oid; 3] = [851, 1570, 1632];
#[cfg(test)]
const F_TEXTICLIKE: [Oid; 2] = [1633, 1660];
#[cfg(test)]
const F_TEXTICNLIKE: [Oid; 2] = [1634, 1661];
#[cfg(test)]
const F_LENGTH: [Oid; 1] = [1317];

/// A text length call's semantic, by the catalog body it runs (its C function), not by an oid list (STEP-07, OIDW-34,
/// OIDW-38): `textlen` is every spelling of a character count (length, char_length, character_length, textlen) and
/// `textoctetlen` the byte count (octet_length); both take one text, give an int4 and never raise. Any other function
/// (bpcharlen trims blanks; bytea's lengths are bytes of another type) is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextLength {
    Chars,
    Octets,
}

pub fn text_length(funcid: Oid) -> Option<TextLength> {
    match gravity_fn::shim_of(funcid)?.name {
        "textlen" => Some(TextLength::Chars),
        "textoctetlen" => Some(TextLength::Octets),
        _ => None,
    }
}
/// A LIKE call's semantic, by the catalog body it runs (its C function), not by an oid list (B2-17, as `text_length`):
/// `textlike` is every spelling of LIKE over a text's bytes (`~~`, `like()`, bpchar's `~~`), `textnlike` NOT LIKE,
/// `texticlike` / `texticnlike` ILIKE and NOT ILIKE. `(ilike, negated)`; any other function (a regex, `namelike`) is
/// `None`, a per-entry body.
pub fn like_body(funcid: Oid) -> Option<(bool, bool)> {
    match gravity_fn::shim_of(funcid)?.name {
        "textlike" => Some((false, false)),
        "textnlike" => Some((false, true)),
        "texticlike" => Some((true, false)),
        "texticnlike" => Some((true, true)),
        _ => None,
    }
}

/// A byte-equality call by its body (B2-17): `texteq` (every spelling of text `=`) is `Some(false)`, `textne` `Some(true)`;
/// bpchar's compares (`bpchareq` trims blanks) and any other function are `None`.
pub fn text_eq_body(funcid: Oid) -> Option<bool> {
    match gravity_fn::shim_of(funcid)?.name {
        "texteq" => Some(false),
        "textne" => Some(true),
        _ => None,
    }
}

/// The int4 compares = <> < <= > >= (the twins' spellings, strpos / position and starts_with, are gravity_program::twin's).
pub const F_INT4_CMP: [Oid; 6] = gravity_fn::typesem::expr::INT4_COMPARE;
const PG_UTF8: i32 = 6;
/// The by-value result types a word table holds (`Form::Words`).
const WORD_TYPES: [Oid; 12] = [16, 18, 20, 21, 23, 26, 700, 701, 1082, 1083, 1114, 1184];
/// The body raised on this entry, or its image did not fit the fill's scratch.
pub const FAULT_BODY: Fault = Fault(1);
pub const FAULT_SCRATCH: Fault = Fault(2);

/// A scan column the caller carries as a `KIND_CODES` lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeCol {
    pub attno: i16,
}

/// The code columns an analysis reads (`analyze`): the caller's `CodeCol`s, or a row's `CodeRef`s as they are (no list
/// of `CodeCol`s made from them).
pub trait Cols {
    /// The code column `attno`, when it is one.
    fn col(&self, attno: i16) -> Option<CodeCol>;
}

impl Cols for [CodeCol] {
    fn col(&self, attno: i16) -> Option<CodeCol> {
        self.iter().find(|c| c.attno == attno).copied()
    }
}

impl<const N: usize> Cols for [CodeCol; N] {
    fn col(&self, attno: i16) -> Option<CodeCol> {
        self[..].col(attno)
    }
}

impl Cols for [crate::emit::CodeRef] {
    fn col(&self, attno: i16) -> Option<CodeCol> {
        self.iter().find(|c| c.attno == attno).map(|c| CodeCol { attno: c.attno })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// The code word itself (a bare `Var`): the key form.
    Code,
    Bits,
    Words,
    /// A by-reference result: a `Derived` table; no row form.
    Derived,
}

/// NEW-72 entry C: a text constant a bound table reads per execution (literal-free pgrcolumnar programs): Const `node`'s
/// word, or with `elem` the word of element `elem` of the array Const `node` (`emit::plan`'s `ParamNeed::Elem`). The
/// lowerer stores the word in a site key cell at each window's bind, the source reads the image there (as an X5 parameter's).
///
/// P5-02: with `list` (and no `elem`), the word is the whole array Const `node` (a list past `MAX_LIST`, keyed by its class
/// whatever its length: konst.rs `list_class`), so one program serves every length; the lowerer's table carries
/// `table_fn::FLAG_LIST` and the source reads the array's elements from the word per execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstWord {
    pub node: NodeId,
    pub elem: Option<u32>,
    pub list: bool,
}

/// A shape the sources already build as a `TableFn`; `Body` is the generic tier-0 fill.
/// Cloned by this crate's unit tests only (`cfg_attr(test)`): the lowering moves a `Known`, never copies it.
#[cfg_attr(test, derive(Clone))]
#[derive(Debug, PartialEq, Eq)]
pub enum Known<'m> {
    Like { pattern: PgVec<'m, u8>, ilike: bool, negated: bool },
    /// B2-20: a code set, the entries equal to one of `values` (`negated`: the others), looked up in the part's
    /// byte-ordered dictionary (`code_set`).
    Eq { values: PgVec<'m, PgVec<'m, u8>>, negated: bool },
    /// X5 (PS1-11 / PS1-31): a code set some of whose values are statement parameters (`$n`, PARAM_EXTERN `(id, type)`),
    /// known only per execution: the entries equal to one of `values` or of the parameters' values (`negated`: the
    /// others). The hosts' EQ table over the values the source reads from the site's key cells at its lane fill, where
    /// the window's bind wrote them (lowerv4 dict/percode.rs `attach`, percode_bind.rs `bound`); a NULL or unreadable
    /// parameter value makes every entry a fault, so each row evaluates the body (PG's answer).
    /// NEW-72 entry C: `consts` are text constants bound the same way (`x = 'c'`, `x IN ('a', 'b')`: `code_set`), their
    /// images never read here, so one program serves every literal of the shape; a NULL or unreadable one faults every entry.
    EqBound { values: PgVec<'m, PgVec<'m, u8>>, params: PgVec<'m, (i32, Oid)>, consts: PgVec<'m, ConstWord>, negated: bool },
    /// PS1-15: a LIKE whose pattern is fixed per execution (`bound_pattern`: a generic plan's `$n`, or `textcat` of such
    /// parameters and text constants, `'%' || $1 || '%'`): the hosts' LIKE table over the pattern the source assembles at
    /// its lane fill from `parts` and the parameters' values in the site's key cells (as `EqBound`'s): `parts[0]`, the
    /// first parameter's value, `parts[1]`, .. (`parts` holds one more run than `params`). A NULL or unreadable value
    /// faults every entry, so each row evaluates the body (PG's answer).
    LikeBound { parts: PgVec<'m, PgVec<'m, u8>>, params: PgVec<'m, (i32, Oid)>, ilike: bool, negated: bool },
    /// P5-06 / SRC-17: the entries whose bytes lie in `[from, to)` (`to` `None`: no upper bound), one code run of the
    /// part's byte-ordered dictionary, its ends by binary search (`code_range`).
    Range { from: PgVec<'m, u8>, to: Option<PgVec<'m, u8>> },
    /// NEW-72 entry C: `Range` over one text constant bound per execution (`code_range`): `[c, ..)` or with `below`
    /// `[.., c)`, `c || '\0'` in place of `c` with `nul` (`x <= c`, `x > c`). The source reads `c` from the key cell its
    /// word is stored in (`EqBound`'s consts) and resolves the range before it builds; a NULL or unreadable `c` faults.
    RangeBound { at: ConstWord, below: bool, nul: bool },
    OctetLength,
    CharLength,
    Body,
}

#[derive(Clone, Copy, Debug)]
pub struct Root {
    pub node: NodeId,
    pub attno: i16,
    pub form: Form,
    /// The index into `Plan::needs` / `RowRefs::tables` of a tabled form.
    pub table: Option<u32>,
    /// A NULL row's answer when the root answers one (`nullable`); `None`: a NULL row is NULL (a strict root).
    pub null: Option<bool>,
}

#[derive(Debug)]
pub struct TableNeed<'m> {
    pub root: NodeId,
    pub attno: i16,
    pub elem: u8,
    pub fingerprint: u64,
    /// D1100: the root holds a by-value word (`print_root`), which its fingerprint does not print: the lowerer keeps it
    /// its own table (step/percode.rs `attach`).
    pub words: bool,
    pub known: Known<'m>,
}

#[derive(Debug)]
pub struct Plan<'m> {
    pub roots: PgVec<'m, Root>,
    pub needs: PgVec<'m, TableNeed<'m>>,
}

impl<'m> Plan<'m> {
    pub fn new_in(m: Mcx<'m>) -> Plan<'m> {
        Plan { roots: PgVec::new_in(m), needs: PgVec::new_in(m) }
    }
}

impl TableNeed<'_> {
    /// The fingerprint a `PROGRAM` table declares (lowerv4 step/percode.rs `attach`; equal ones are one table, D319). D1100:
    /// a root holding a by-value word (`words`: its fingerprint prints no value) mixes in its place, the expression `ex`
    /// and the root, so it is its own table (percode_key.rs keys by `ex` alike): one table for two such roots let a cached
    /// program's second literal read the table its first literal swept.
    pub fn table_fingerprint(&self, ex: u32) -> u64 {
        let place = u128::from(self.fingerprint) << 64 | u128::from(ex) << 32 | u128::from(self.root.0);
        if self.words { gravity_abi::hash::hash_bytes64(&place.to_le_bytes()) | 1 } else { self.fingerprint }
    }
}

impl Plan<'_> {
    pub fn root_at(&self, id: NodeId) -> Option<&Root> {
        self.roots.iter().find(|r| r.node == id)
    }
}

/// The walk's facts about one subtree.
#[derive(Clone, Copy)]
struct Scan {
    attno: Option<i16>,
    mixed: bool,
    has_var: bool,
}

pub(crate) fn analyze<'m, C: Cols + ?Sized>(p: &Program<'_>, root: NodeId, codes: &C, scratch: Mcx<'m>) -> Result<Plan<'m>, EmitError> {
    let mut plan = Plan::new_in(scratch);
    walk(p, root, codes, &mut plan, true, scratch)?;
    Ok(plan)
}

/// The bare `Var` is the key form only as the program's root; under another node it stays a code
/// lane `emit` refuses (the by-reference view is the lowerer's).
fn walk<'m, C: Cols + ?Sized>(p: &Program<'_>, id: NodeId, codes: &C, plan: &mut Plan<'m>, top: bool, scratch: Mcx<'m>) -> Result<(), EmitError> {
    crate::stack_check();
    if !top && bare_var(p, id) {
        return Ok(());
    }
    let mut st = Scan { attno: None, mixed: false, has_var: false };
    let clean = scan(p, id, codes, &mut st);
    if let (true, false, Some(attno)) = (clean && st.has_var, st.mixed, st.attno) {
        codes.col(attno).expect("a scanned attno is declared");
        let form = classify(p, id)?;
        let table = match form {
            Form::Code => None,
            Form::Bits | Form::Words | Form::Derived => {
                let elem = match form {
                    Form::Bits => gravity_dict::ELEM_BITS,
                    Form::Words => gravity_dict::ELEM_I64,
                    _ => gravity_dict::ELEM_DERIVED,
                };
                let known = known_of(p, id, scratch);
                let (fingerprint, words) = fingerprint_bound(p, id, &|n| bound_const(&known, n), scratch);
                GrowIn::push_in(&mut plan.needs, TableNeed { root: id, attno, elem, fingerprint, words, known });
                Some(plan.needs.len() as u32 - 1)
            }
        };
        GrowIn::push_in(&mut plan.roots, Root { node: id, attno, form, table, null: None });
        return Ok(());
    }
    if let Some((attno, known, null)) = nullable(p, id, codes, scratch) {
        let (fingerprint, words) = fingerprint(p, id, scratch);
        GrowIn::push_in(&mut plan.needs, TableNeed { root: id, attno, elem: gravity_dict::ELEM_BITS, fingerprint, words, known });
        let table = Some(plan.needs.len() as u32 - 1);
        GrowIn::push_in(&mut plan.roots, Root { node: id, attno, form: Form::Bits, table, null: Some(null) });
        return Ok(());
    }
    for k in walk_kids(p, id) {
        walk(p, k, codes, plan, false, scratch)?;
    }
    Ok(())
}

/// Whether the subtree is a per-code body: only the allowed kinds, one code column, every `Var`
/// reached through strict calls, no volatile site, no null constant.
fn scan<C: Cols + ?Sized>(p: &Program<'_>, id: NodeId, codes: &C, st: &mut Scan) -> bool {
    crate::stack_check();
    if let Some(isl) = p.islands.iter().find(|i| i.root == id) {
        return scan_island(p, isl, codes, st);
    }
    match &p.nodes[id.0 as usize] {
        ExprNode::Var(v) => {
            if v.src != eval::Source::Scan || codes.col(v.attno).is_none() {
                return false;
            }
            if st.attno.is_some_and(|a| a != v.attno) {
                st.mixed = true;
            }
            st.attno = Some(v.attno);
            st.has_var = true;
            true
        }
        ExprNode::Const(c) => !c.is_null(),
        ExprNode::Param(q) => q.kind == ParamKind::PARAM_EXTERN,
        ExprNode::Relabel(r) => scan(p, r.arg, codes, st),
        ExprNode::Func(n) | ExprNode::Op(n) => strict_site(p, &p.sites[n.site.0 as usize], id, codes, st),
        ExprNode::ScalarArrayOp(n) => nonempty_array(p, id, n) && strict_site(p, &p.sites[n.site.0 as usize], id, codes, st),
        ExprNode::Bool(n) if n.op == BoolExprType::NOT_EXPR => scan(p, p.kids[n.args.start as usize], codes, st),
        _ => {
            let mut inner = Scan { attno: None, mixed: false, has_var: false };
            let clean = walk_kids(p, id).into_iter().all(|k| scan(p, k, codes, &mut inner));
            merge(st, &inner);
            clean && !inner.has_var
        }
    }
}

/// D1812: a ScalarArrayOp is strict in its scalar only over a non-empty array: PG answers `x = ANY ('{}')` false and
/// `x <> ALL ('{}')` true without reading `x`, a NULL row included. So only a non-NULL constant array with a dimension
/// (an empty one has none) is a body; any other array keeps the subtree per row. The count is the list's class the keys
/// hash (`list_len`, V4R-110), so reading it bakes nothing.
fn nonempty_array(p: &Program<'_>, id: NodeId, n: &crate::nodes::op::ScalarArrayOp) -> bool {
    let call = &p.sites[n.site.0 as usize];
    match &p.nodes[p.kids[call.args.start as usize + 1].0 as usize] {
        ExprNode::Const(c) => c.list_len().is_some_and(|n| n > 0),
        // X5: an `ARRAY[..]` of constants and parameters only as a bound code set (the hosts' EQ table), never as a body
        // swept per entry
        _ => matches!(code_set(p, id, false, mcx::MemoryContext::new_bump("GravityCodeSetProbe").mcx()), Some(Known::EqBound { .. })),
    }
}

/// X5: `ARRAY[e1, ..]` (one dimension, one or more elements, so never empty), each element a non-null text constant or a
/// statement parameter (`extern_param`): the constants' images and the parameters, in order.
fn bound_list<'m>(p: &Program<'_>, arr: NodeId, m: Mcx<'m>) -> Option<(PgVec<'m, PgVec<'m, u8>>, PgVec<'m, (i32, Oid)>)> {
    let a = bound_array(p, arr)?;
    if a.multidims || a.elements.len == 0 || a.elem.len != -1 {
        return None;
    }
    let (mut values, mut params) = (PgVec::new_in(m), PgVec::new_in(m));
    for i in 0..a.elements.len {
        let e = p.kids[(a.elements.start + i) as usize];
        match &p.nodes[e.0 as usize] {
            ExprNode::Const(c) if c.typlen == -1 && !c.is_null() => GrowIn::push_in(&mut values, const_bytes_in(c, m)?),
            _ => GrowIn::push_in(&mut params, extern_param(p, e)?),
        }
    }
    Some((values, params))
}

/// X5: the `ArrayExpr` at `arr`. Not through an `ArrayCoerce`: a varchar column's `IN ($1, $2)` is PG's
/// `ARRAY[$1::varchar, ..]` coerced to `text[]`, which the row path's emit does not serve (emit/scalar.rs `check_node`), so
/// the root's body (a NULL parameter's faults) could not run; that list stays `expr.saop`'s.
fn bound_array<'a>(p: &'a Program<'_>, arr: NodeId) -> Option<&'a crate::nodes::array::ArrayExpr> {
    match &p.nodes[arr.0 as usize] {
        ExprNode::Array(a) => Some(a),
        _ => None,
    }
}

/// X5: the element count of the list at `arr` when it is a bound list's array (`bound_array`), else `None` (lowerv4
/// dict/percode.rs `sets_first` leaves such a list to the per-code road up to `MAX_LIST`).
pub fn bound_list_len(p: &Program<'_>, arr: NodeId) -> Option<usize> {
    bound_array(p, arr).map(|a| a.elements.len as usize)
}

/// X5: a statement parameter (`$n`, PARAM_EXTERN) under transparent relabels: its id and type. Its value is a text image
/// when the call over it is a text body (the caller's test), read per execution.
fn extern_param(p: &Program<'_>, mut id: NodeId) -> Option<(i32, Oid)> {
    loop {
        match &p.nodes[id.0 as usize] {
            ExprNode::Param(q) if q.kind == ParamKind::PARAM_EXTERN => return Some((q.id, q.typ)),
            ExprNode::Relabel(r) if r.transparent() => id = r.arg,
            _ => return None,
        }
    }
}

/// An interpreter island in a would-be body (D340): its host call reads the island's columns off the row, and the sweep's
/// row holds only the entry's image. So the island is the body's only when every column its range reads is a scan code
/// column, merged into `attno` as a `Var` is (a second column is `mixed`) but not into `has_var` (the island is no strict
/// path to it), and no call of the range is volatile (the sweep would run it once per entry). Anything else (another
/// column: `GREATEST(t, 'v' || i::text) = t`) keeps the subtree off the per-code form; the island runs per row.
fn scan_island<C: Cols + ?Sized>(p: &Program<'_>, isl: &Island, codes: &C, st: &mut Scan) -> bool {
    let volatile = |s: SiteId| p.sites.get(s.0 as usize).map_or(true, |c| c.volatile);
    for n in &p.nodes[isl.first.0 as usize..=isl.root.0 as usize] {
        match n {
            ExprNode::Var(v) => {
                if v.src != eval::Source::Scan || codes.col(v.attno).is_none() {
                    return false;
                }
                if st.attno.is_some_and(|a| a != v.attno) {
                    st.mixed = true;
                }
                st.attno = Some(v.attno);
            }
            ExprNode::Func(f) | ExprNode::Op(f) | ExprNode::Distinct(f) | ExprNode::NullIf(f) if volatile(f.site) => return false,
            ExprNode::ScalarArrayOp(o) if volatile(o.site) => return false,
            ExprNode::Param(_) | ExprNode::NextValue(_) | ExprNode::Aggref(_) | ExprNode::WindowFunc(_) | ExprNode::SubPlan(_) | ExprNode::Srf(_) => return false,
            _ => {}
        }
    }
    true
}

/// `inner`'s column into `st`: a second code column is `mixed`.
fn merge(st: &mut Scan, inner: &Scan) {
    if let Some(a) = inner.attno {
        if st.attno.is_some_and(|b| b != a) {
            st.mixed = true;
        }
        st.attno = Some(a);
    }
    st.mixed |= inner.mixed;
}

fn strict_site<C: Cols + ?Sized>(p: &Program<'_>, call: &Call, id: NodeId, codes: &C, st: &mut Scan) -> bool {
    if call.volatile {
        return false;
    }
    let mut inner = Scan { attno: None, mixed: false, has_var: false };
    let mut clean = true;
    for k in walk_kids(p, id) {
        clean &= scan(p, k, codes, &mut inner);
    }
    if inner.has_var && !call.strict {
        return false;
    }
    merge(st, &inner);
    st.has_var |= inner.has_var;
    clean
}

/// The `Var` under a chain of `RelabelType`.
fn bare_var(p: &Program<'_>, mut id: NodeId) -> bool {
    loop {
        match &p.nodes[id.0 as usize] {
            ExprNode::Var(_) => return true,
            ExprNode::Relabel(r) if r.transparent() => id = r.arg,
            _ => return false,
        }
    }
}

/// D3038 (R2): node `id` as `col = ''` (`false`) / `col <> ''` (`true`) over a bare column, with a non-null empty text
/// constant under a deterministic collation (D1832: a nondeterministic one equates other strings with `''`): the column
/// operand's node and the negation. The empty string is the one value of byte length 0, so the test is the column's
/// length's (lowerv4 octet_lane.rs).
pub fn empty_test(p: &Program<'_>, id: NodeId) -> Option<(NodeId, bool)> {
    let (call, c) = var_const(p, id)?;
    let ne = text_eq_body(call.funcid).filter(|_| c.text_empty() == Some(true) && deterministic(call.collation))?;
    Some((p.kids[call.args.start as usize], ne))
}

/// `Op(Var, Const)` of `funcid`: the constant, when the site has that shape.
fn var_const<'a>(p: &'a Program<'_>, id: NodeId) -> Option<(&'a Call, &'a ConstNode<'a>)> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    if call.args.len != 2 || !bare_var(p, p.kids[call.args.start as usize]) {
        return None;
    }
    match &p.nodes[p.kids[call.args.start as usize + 1].0 as usize] {
        ExprNode::Const(c) => Some((call, c)),
        _ => None,
    }
}


/// A non-null constant's bytes: the word by value, a varlena's payload in any header form, a cstring to its NUL, a fixed
/// type's bytes. `None` when a compressed or external image cannot be expanded, never an empty payload for a failure
/// (D348: a custom plan's short-header constant of 65..126 bytes, copied through a 64-byte scratch, read as `''`, so
/// `t <> v` answered `t <> ''`). Plain and short-header images are read in full in place: only their bytes are copied.
pub(crate) fn with_const_bytes<R>(c: &ConstNode<'_>, f: &mut dyn FnMut(&[u8]) -> Option<R>) -> Option<R> {
    let v = c.read().value; // a by-reference image's bytes decide what the caller builds: a bake
    if c.byval {
        return f(&v.as_u64().to_le_bytes());
    }
    let p = v.as_usize() as *const u8;
    // SAFETY: a non-null by-reference constant points at a live image of its type (the plan's, or the program's copy).
    unsafe {
        match c.typlen {
            -1 => crate::views::varlena_payload_with(p, f),
            -2 => f(core::ffi::CStr::from_ptr(p.cast()).to_bytes()),
            n if n > 0 => f(core::slice::from_raw_parts(p, n as usize)),
            _ => None,
        }
    }
}

/// A list of the one value `v`, in `m`.
fn one_in<'m, T>(v: T, m: Mcx<'m>) -> PgVec<'m, T> {
    let mut l = PgVec::new_in(m);
    GrowIn::push_in(&mut l, v);
    l
}

/// `with_const_bytes` with the bytes copied into a vector of `m`: the plan's own lists (`Known`) keep them there.
pub(crate) fn const_bytes_in<'m>(c: &ConstNode<'_>, m: Mcx<'m>) -> Option<PgVec<'m, u8>> {
    with_const_bytes(c, &mut |b| Some(mcx::to_vec_in(b, m)))
}

/// The root's form: the code word, or a table by the result type (a comparison with any constant, `''` too, is a
/// `Bits` table).
fn classify(p: &Program<'_>, id: NodeId) -> Result<Form, EmitError> {
    if bare_var(p, id) {
        return Ok(Form::Code);
    }
    let rettype = rettype_of(p, id);
    Ok(if rettype == BOOLOID {
        Form::Bits
    } else if WORD_TYPES.contains(&rettype) {
        Form::Words
    } else {
        Form::Derived
    })
}

fn rettype_of(p: &Program<'_>, id: NodeId) -> Oid {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Func(n) | ExprNode::Op(n) => p.sites[n.site.0 as usize].rettype,
        ExprNode::Bool(_) | ExprNode::ScalarArrayOp(_) => BOOLOID,
        ExprNode::Relabel(r) => rettype_of(p, r.arg),
        ExprNode::Var(v) => v.typ,
        ExprNode::Const(c) => c.typ,
        ExprNode::Param(q) => q.typ,
        _ => 0,
    }
}

/// Equal bytes are equal values under `coll`: typesem's built-collation answer (no collation, D2046, or one the catalog
/// cannot build answers false, so the body raises PG's error).
fn deterministic(coll: Oid) -> bool {
    gravity_fn::typesem::expr::built_coll_bytes(coll)
}

fn known_of<'m>(p: &Program<'_>, id: NodeId, m: Mcx<'m>) -> Known<'m> {
    let (inner, negated) = match &p.nodes[id.0 as usize] {
        ExprNode::Bool(n) if n.op == BoolExprType::NOT_EXPR => (p.kids[n.args.start as usize], true),
        _ => (id, false),
    };
    if let Some(t) = text_search(p, inner, m).filter(|t| bare_var(p, t.arg)) {
        return Known::Like { pattern: t.pattern, ilike: false, negated: t.negated != negated };
    }
    if let Some(k) = code_set(p, inner, negated, m).or_else(|| code_range(p, inner, negated)) {
        return k;
    }
    if let Some((call, c)) = var_const(p, inner) {
        let (like, (ilike, neg)) = like_body(call.funcid).map_or((false, (false, false)), |b| (true, b));
        // D318: the LIKE table matches bytes, PG's LIKE only under a deterministic collation; under a nondeterministic one
        // PG 18 matches per character (like_match.c), so the root is the generic body (the shim, under its collation)
        if like && c.typlen == -1 && deterministic(call.collation) {
            // a pattern whose image cannot be read is the generic body's (D348). D1938: the table reads `_` as one UTF-8
            // character and ILIKE folds UTF-8, so it is a UTF-8 database's only; pgrust serves UTF8 only (D3521: the
            // single-byte SQL_ASCII arm went with that ruling), any other encoding keeps the generic body
            let utf8 = gravity_fn::typesem::collation::database_encoding() == PG_UTF8;
            if let Some(pattern) = const_bytes_in(c, m).filter(|_| utf8) {
                return Known::Like { pattern, ilike, negated: neg != negated };
            }
        }
    }
    if let Some(k) = like_bound(p, inner, negated, m).or_else(|| lower_like(p, inner, negated, m)) {
        return k;
    }
    if let (ExprNode::Func(n) | ExprNode::Op(n), false) = (&p.nodes[inner.0 as usize], negated) {
        let call = &p.sites[n.site.0 as usize];
        let arg = p.kids[call.args.start as usize];
        // D3077: `lower(x)` under a C ctype folds ASCII only, so it keeps x's byte and character counts
        if call.args.len == 1 && (bare_var(p, arg) || ascii_lower_of_var(p, arg).is_some()) {
            match text_length(call.funcid) {
                Some(TextLength::Octets) => return Known::OctetLength,
                Some(TextLength::Chars) => return Known::CharLength,
                None => {}
            }
        }
    }
    Known::Body
}

/// D3077: `lower(x)` over the bare column under a deterministic collation whose ctype is C, where PostgreSQL's lower
/// folds ASCII letters only (str_tolower's C-ctype path): the text keeps its byte and character counts. The call's
/// collation when `id` is that form.
fn ascii_lower_of_var(p: &Program<'_>, id: NodeId) -> Option<Oid> {
    let (ExprNode::Func(n) | ExprNode::Op(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    if call.args.len != 1 || call.volatile || gravity_fn::shim_of(call.funcid).map(|s| s.name) != Some("lower") || !bare_var(p, p.kids[call.args.start as usize]) {
        return None;
    }
    gravity_fn::typesem::collation::Facts::of(call.collation).is_ok_and(|f| f.deterministic && f.ctype_is_c).then_some(call.collation)
}

/// D3077: `lower(x) LIKE p` / `NOT LIKE` over the bare column with a constant pattern, both under one C-ctype collation
/// (`ascii_lower_of_var`) in a UTF-8 database: PostgreSQL's ILIKE there is `lower(x) LIKE lower(p)` (Generic_Text_IC_like),
/// so when `p` holds no ASCII capital (lower(p) = p; a capital never matches a lowered text) the root is the column's
/// ILIKE verdict table (the hosts fold ASCII under a C ctype, D2680), read per code instead of lower() per entry or row.
fn lower_like<'m>(p: &Program<'_>, id: NodeId, negated: bool, m: Mcx<'m>) -> Option<Known<'m>> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let (false, neg) = like_body(call.funcid)? else { return None };
    let utf8 = gravity_fn::typesem::collation::database_encoding() == PG_UTF8;
    if call.args.len != 2 || !utf8 || !deterministic(call.collation) {
        return None;
    }
    if ascii_lower_of_var(p, p.kids[call.args.start as usize])? != call.collation {
        return None;
    }
    let ExprNode::Const(c) = &p.nodes[p.kids[call.args.start as usize + 1].0 as usize] else { return None };
    if c.typlen != -1 || c.is_null() {
        return None;
    }
    let pattern = const_bytes_in(c, m).filter(|pt| !pt.iter().any(u8::is_ascii_uppercase))?;
    Some(Known::Like { pattern, ilike: true, negated: neg != negated })
}

/// PS1-15: `x LIKE p` / `NOT LIKE` / `ILIKE` over the bare column whose pattern is fixed per execution (`bound_pattern`),
/// under the rules a constant pattern's `Known::Like` keeps (D318: a deterministic collation; D1938: a UTF-8 database,
/// as the table reads `_` as one UTF-8 character and ILIKE folds UTF-8, whatever pattern the execution binds).
fn like_bound<'m>(p: &Program<'_>, id: NodeId, negated: bool, m: Mcx<'m>) -> Option<Known<'m>> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let (ilike, neg) = like_body(call.funcid)?;
    let utf8 = gravity_fn::typesem::collation::database_encoding() == PG_UTF8;
    if call.args.len != 2 || !utf8 || !deterministic(call.collation) || !bare_var(p, p.kids[call.args.start as usize]) {
        return None;
    }
    let (parts, params) = bound_pattern(p, p.kids[call.args.start as usize + 1], m)?;
    Some(Known::LikeBound { parts, params, ilike, negated: neg != negated })
}

/// PS1-15: a pattern fixed before the rows run that holds a statement parameter: `$n` (`extern_param`), or `textcat` over
/// such parameters and non-null text constants (lane like's PS4-05 forms over a heap column), read left to right into
/// the constant runs between the parameters (`parts`, one more than `params`; adjacent constants joined) and the
/// parameters. `None` for any other pattern, a constant one included (`Known::Like`'s). PG concatenates the pattern per
/// row; it is one value per execution, which the source assembles once (granule_source `bound_spec`).
fn bound_pattern<'m>(p: &Program<'_>, id: NodeId, m: Mcx<'m>) -> Option<(PgVec<'m, PgVec<'m, u8>>, PgVec<'m, (i32, Oid)>)> {
    let (mut parts, mut params, mut todo) = (PgVec::<PgVec<u8>>::new_in(m), PgVec::new_in(m), mcx::vec_elem_in(m, id, 1));
    GrowIn::push_in(&mut parts, PgVec::new_in(m));
    while let Some(n) = todo.pop() {
        if let Some(q) = extern_param(p, n) {
            GrowIn::push_in(&mut params, q);
            GrowIn::push_in(&mut parts, PgVec::new_in(m));
            continue;
        }
        match &p.nodes[n.0 as usize] {
            ExprNode::Const(c) if c.typlen == -1 && !c.is_null() => GrowIn::extend_from_slice_in(parts.last_mut()?, &const_bytes_in(c, m)?),
            ExprNode::Op(f) | ExprNode::Func(f) => {
                let call = &p.sites[f.site.0 as usize];
                if call.args.len != 2 || gravity_fn::shim_of(call.funcid).map(|s| s.name) != Some("textcat") {
                    return None;
                }
                // the right operand first: the left one pops first
                GrowIn::extend_in(&mut todo, [p.kids[call.args.start as usize + 1], p.kids[call.args.start as usize]]);
            }
            _ => return None,
        }
    }
    (!params.is_empty()).then_some((parts, params))
}

/// C06: whether ScalarArrayOp `id` is a code set (`code_set`) over its bare column, and the column's `Var`: a list PG
/// hashed that the per-code road reads from the part's EQ table before `expr.saop` would claim it (lowerv4
/// dict/percode.rs `sets_first`).
pub fn code_set_var(p: &Program<'_>, id: NodeId) -> Option<crate::nodes::var::Var> {
    let ExprNode::ScalarArrayOp(n) = &p.nodes[id.0 as usize] else { return None };
    code_set(p, id, false, mcx::MemoryContext::new_bump("GravityCodeSetProbe").mcx())?;
    let mut x = p.kids[p.sites[n.site.0 as usize].args.start as usize];
    loop {
        match &p.nodes[x.0 as usize] {
            ExprNode::Var(v) => return Some(*v),
            ExprNode::Relabel(r) if r.transparent() => x = r.arg,
            _ => return None,
        }
    }
}

/// B2-20: `x = c` / `x <> c`, `x = ANY (list)` / `x <> ALL (list)` (`IN` / `NOT IN`) over the bare code column by the byte
/// equality body (`text_eq_body`) under a collation whose equal bytes are equal values (`deterministic`): the code set of
/// those values (`negated`: its complement), any value and any count, not only `''`. `= ALL` and `<> ANY` are no code
/// set. NEW-72 entry C: a constant's value or a constant list's elements are bound per execution (`Known::EqBound`'s
/// `consts`, never read here: a literal is never baked into a pgrcolumnar program), so a NULL element (`= ANY` is then
/// NULL, not false, off the set) or an image the source cannot read faults every entry, and each row evaluates the body.
fn code_set<'m>(p: &Program<'_>, id: NodeId, negated: bool, m: Mcx<'m>) -> Option<Known<'m>> {
    // the byte equalities are symmetric: `c = x` is `x = c`
    let compare = || {
        let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
        let call = Some(&p.sites[n.site.0 as usize]).filter(|c| c.args.len == 2)?;
        let (a, b) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
        let konst = |k: NodeId| matches!(&p.nodes[k.0 as usize], ExprNode::Const(c) if c.typlen == -1 && !c.is_null());
        if bare_var(p, a) && konst(b) {
            Some((call, b))
        } else if bare_var(p, b) && konst(a) {
            Some((call, a))
        } else {
            None
        }
    };
    if let Some((call, c)) = compare() {
        let ne = text_eq_body(call.funcid)?;
        if !deterministic(call.collation) {
            return None;
        }
        // `''` is held by both keys (konst.rs `text_empty`, lower key.rs beside the comparand's class): its set is the
        // empty entry's, built as before (`Class::Set`, no fault table); any other literal is bound per execution
        let ExprNode::Const(k) = &p.nodes[c.0 as usize] else { return None };
        if k.text_empty() == Some(true) {
            return Some(Known::Eq { values: mcx::vec_elem_in(m, PgVec::new_in(m), 1), negated: ne != negated });
        }
        return Some(Known::EqBound { values: PgVec::new_in(m), params: PgVec::new_in(m), consts: mcx::vec_elem_in(m, ConstWord { node: c, elem: None, list: false }, 1), negated: ne != negated });
    }
    // X5: `x = $n` / `$n = x` over the bare column, the parameter's value bound per execution
    if let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] {
        let call = &p.sites[n.site.0 as usize];
        if call.args.len == 2 && deterministic(call.collation) {
            let (a, b) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
            let param = if bare_var(p, a) { extern_param(p, b) } else if bare_var(p, b) { extern_param(p, a) } else { None };
            if let (Some(q), Some(ne)) = (param, text_eq_body(call.funcid)) {
                return Some(Known::EqBound { values: PgVec::new_in(m), params: mcx::vec_elem_in(m, q, 1), consts: PgVec::new_in(m), negated: ne != negated });
            }
        }
        return None;
    }
    let ExprNode::ScalarArrayOp(n) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let ne = text_eq_body(call.funcid)?;
    if call.args.len != 2 || n.use_or == ne || n.elem.len != -1 || !deterministic(call.collation) || !bare_var(p, p.kids[call.args.start as usize]) {
        return None;
    }
    // X5: `x = ANY (ARRAY[$1, 'c', ..])` / `x <> ALL (..)`: a generic plan's IN list, its parameters bound per execution
    if let Some((values, params)) = bound_list(p, p.kids[call.args.start as usize + 1], m) {
        return (!params.is_empty()).then_some(Known::EqBound { values, params, consts: PgVec::new_in(m), negated: ne != negated });
    }
    let arr = p.kids[call.args.start as usize + 1];
    let ExprNode::Const(list) = &p.nodes[arr.0 as usize] else { return None };
    // the element count is the list's class the keys hash (konst.rs `list_class`): no bake
    let count = list.list_count(n.elem).filter(|&k| k > 0)?;
    // P5-02: a list past MAX_LIST shares one class whatever its length, so its program reads the whole array as one word
    // (its elements read per execution by the source), never one cell per element
    if count > gravity_program::cost::expr::MAX_LIST {
        return Some(Known::EqBound { values: PgVec::new_in(m), params: PgVec::new_in(m), consts: mcx::vec_elem_in(m, ConstWord { node: arr, elem: None, list: true }, 1), negated: ne != negated });
    }
    let consts = mcx::CollectIn::collect_in((0..count as u32).map(|k| ConstWord { node: arr, elem: Some(k), list: false }), m);
    Some(Known::EqBound { values: PgVec::new_in(m), params: PgVec::new_in(m), consts, negated: ne != negated })
}

/// NEW-72 entry C: whether a compare of function `funcid` under collation `coll` between a bare code column and a non-null
/// text constant is one the per-code road binds per execution (`code_set`'s byte equality under a deterministic
/// collation, `code_range`'s byte-order compare), read in the plan tree: the content key holds such a constant by its
/// class (lower key.rs `bound_text_refs`), never its image.
pub fn bound_text_compare(funcid: Oid, coll: Oid) -> bool {
    (text_eq_body(funcid).is_some() && deterministic(coll)) || order_body(funcid).is_some_and(|(_, _, pattern)| pattern || bytes_ordered(coll))
}

/// NEW-72 entry C: whether `x = ANY (list)` / `x <> ALL (list)` (`use_or` its form) of function `funcid` under collation
/// `coll` over a bare code column is `code_set`'s, its elements bound per execution: the content key holds the list by
/// its class (lower key.rs `word_list`).
pub fn bound_text_set(funcid: Oid, use_or: bool, coll: Oid) -> bool {
    text_eq_body(funcid).is_some_and(|ne| use_or != ne) && deterministic(coll)
}

/// A byte-order compare by its body: `(below, inclusive)` for `x < c` (true, false), `<=`, `>`, `>=`, and whether it
/// compares bytes under any collation (the `~<~` family, `text_pattern_*`) rather than under its collation's order.
fn order_body(funcid: Oid) -> Option<(bool, bool, bool)> {
    let name = gravity_fn::shim_of(funcid)?.name;
    let (op, pattern) = match name.strip_prefix("text_pattern_") {
        Some(op) => (op, true),
        None => (name.strip_prefix("text_")?, false),
    };
    Some(match op {
        "lt" => (true, false, pattern),
        "le" => (true, true, pattern),
        "gt" => (false, false, pattern),
        "ge" => (false, true, pattern),
        _ => return None,
    })
}

/// `coll` orders text by its bytes: C, POSIX, or a collation the catalog builds with a C collate (the database default
/// only when it is one; collation `Facts::order_is_bytes`). No collation, or one the catalog cannot build, is false.
fn bytes_ordered(coll: Oid) -> bool {
    coll != 0 && gravity_fn::typesem::collation::Facts::of(coll).is_ok_and(|f| f.order_is_bytes())
}

/// P5-06 / SRC-17: `x < c`, `x <= c`, `x > c`, `x >= c` (the constant either side, under at most one NOT) over the bare
/// code column by a byte-order compare body (`order_body`: text's under a collation that orders by bytes, the pattern
/// family's under any), the constant a non-null text image: the entries in one byte interval. `c < x` is `x > c`; a NOT
/// is the other side of the bound (the table answers no NULL row); `x <= c` is `x < c || '\0'` and `x > c` is
/// `x >= c || '\0'`, as no byte string lies between `c` and `c || '\0'`. NEW-72 entry C: `c` is bound per execution
/// (`Known::RangeBound`, its image never read here).
fn code_range(p: &Program<'_>, id: NodeId, negated: bool) -> Option<Known<'static>> {
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    if call.args.len != 2 {
        return None;
    }
    let (a, b) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
    let (k, c, flip) = match (&p.nodes[a.0 as usize], &p.nodes[b.0 as usize]) {
        (_, ExprNode::Const(c)) if bare_var(p, a) => (b, c, false),
        (ExprNode::Const(c), _) if bare_var(p, b) => (a, c, true),
        _ => return None,
    };
    let (below, inclusive, pattern) = order_body(call.funcid)?;
    if c.typlen != -1 || c.is_null() || !(pattern || bytes_ordered(call.collation)) {
        return None;
    }
    let (below, inclusive) = ((below != flip) != negated, inclusive != negated);
    Some(Known::RangeBound { at: ConstWord { node: k, elem: None, list: false }, below, nul: below == inclusive })
}

/// PS1-13 / PS1-14: a byte test over code column `x` that answers a NULL row, under at most one NOT: `x IS DISTINCT FROM c`
/// (the `=` of `code_set`) and a `code_set` / `code_range` compare over `coalesce(x, c0)`, `c` and `c0` non-null text
/// images read whole. An entry's answer is the test's table over `x` (`coalesce` passes a non-NULL `x`); a NULL row's is
/// the table's test of `c0`, or, with no `coalesce`, `IS DISTINCT FROM`'s true. A strict compare over the bare column is
/// `scan`'s root.
fn nullable<'m, C: Cols + ?Sized>(p: &Program<'_>, id: NodeId, codes: &C, m: Mcx<'m>) -> Option<(i16, Known<'m>, bool)> {
    let (inner, negated) = match &p.nodes[id.0 as usize] {
        ExprNode::Bool(n) if n.op == BoolExprType::NOT_EXPR => (p.kids[n.args.start as usize], true),
        _ => (id, false),
    };
    let (call, distinct) = match &p.nodes[inner.0 as usize] {
        ExprNode::Distinct(n) => (&p.sites[n.site.0 as usize], true),
        ExprNode::Op(n) | ExprNode::Func(n) => (&p.sites[n.site.0 as usize], false),
        _ => return None,
    };
    if call.volatile || call.args.len != 2 {
        return None;
    }
    let text = |k: NodeId| match &p.nodes[k.0 as usize] {
        ExprNode::Const(c) if c.typlen == -1 && !c.is_null() => Some(c),
        _ => None,
    };
    let (a, b) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
    let ((attno, c0), c, flip) = match (operand(p, a), text(b)) {
        (Some(x), Some(c)) => (x, c, false),
        _ => (operand(p, b)?, text(a)?, true),
    };
    if codes.col(attno).is_none() || (c0.is_none() && !distinct) {
        return None;
    }
    let mut v = const_bytes_in(c, m)?;
    let known = match text_eq_body(call.funcid) {
        Some(ne) if !(ne && distinct) && deterministic(call.collation) => Known::Eq { values: one_in(v, m), negated: (ne || distinct) != negated },
        None if !distinct => {
            let (below, inclusive, pattern) = order_body(call.funcid)?;
            if !(pattern || bytes_ordered(call.collation)) {
                return None;
            }
            let (below, inclusive) = ((below != flip) != negated, inclusive != negated);
            if below == inclusive {
                GrowIn::push_in(&mut v, 0);
            }
            if below { Known::Range { from: PgVec::new_in(m), to: Some(v) } } else { Known::Range { from: v, to: None } }
        }
        _ => return None,
    };
    let null = match c0 {
        Some(c0) => {
            let d = const_bytes_in(c0, m)?;
            match &known {
                Known::Eq { values, negated } => values.iter().any(|x| *x == d) != *negated,
                Known::Range { from, to } => *from <= d && to.as_ref().map_or(true, |t| d < *t),
                _ => return None,
            }
        }
        None => !negated,
    };
    Some((attno, known, null))
}

/// A scan code column under transparent relabels, bare (`None`) or as `coalesce(x, c0)` (`c0` a non-null text constant).
fn operand<'a>(p: &'a Program<'_>, mut id: NodeId) -> Option<(i16, Option<&'a ConstNode<'a>>)> {
    let mut c0 = None;
    loop {
        match &p.nodes[id.0 as usize] {
            ExprNode::Var(v) if v.src == eval::Source::Scan => return Some((v.attno, c0)),
            ExprNode::Relabel(r) if r.transparent() => id = r.arg,
            ExprNode::Coalesce(z) if c0.is_none() && z.args.len == 2 => {
                let ExprNode::Const(c) = &p.nodes[p.kids[z.args.start as usize + 1].0 as usize] else { return None };
                if c.typlen != -1 || c.is_null() {
                    return None;
                }
                c0 = Some(c);
                id = p.kids[z.args.start as usize];
            }
            _ => return None,
        }
    }
}

/// NORM-TEXT-SEARCH (HITS9-E2): a constant substring or prefix test in another spelling, as its LIKE twin: the text it
/// searches, the twin's pattern (escape `\`) and NOT LIKE.
#[derive(Debug, PartialEq, Eq)]
pub struct TextSearch<'m> {
    pub arg: NodeId,
    pub pattern: PgVec<'m, u8>,
    pub negated: bool,
}

/// The LIKE twin of a program's spelling (gravity_program::twin's rules, the ones normalize's `twin_of` reads over the plan;
/// the functions named by typesem's `starts_with` / `text_position`):
/// `starts_with(x, c)` / `x ^@ c`, or an int4 compare of `strpos(x, c)` / `position(c IN x)` with a constant on either
/// side that `textpos_test` finds a membership test. normalize rewrites only the graph's filters, so this reads the
/// sites it does not (an aggregate's FILTER or argument, a projection, a join or subplan qual) for both paths
/// (`known_of`'s LIKE table, lowerv4 step/text_like.rs's claim); `textpos_class` keys the compare's class. `None` (PG's
/// function runs): a NULL or non-constant needle, or one whose image cannot be read; the search's own collation (never
/// the compare's, which is 0) unset or nondeterministic (strpos then matches per character, starts_with raises); a
/// database that is not UTF-8 (PG matches characters: a byte hit may start mid-one).
pub fn text_search<'m>(p: &Program<'_>, id: NodeId, m: Mcx<'m>) -> Option<TextSearch<'m>> {
    use gravity_fn::typesem::expr::{starts_with, text_position};
    use gravity_program::twin::{twin_pattern, twin_spelling};
    let call = |id: NodeId| match &p.nodes[id.0 as usize] {
        ExprNode::Op(n) | ExprNode::Func(n) => Some(&p.sites[n.site.0 as usize]).filter(|c| c.args.len == 2),
        _ => None,
    };
    let args = |c: &Call| [p.kids[c.args.start as usize], p.kids[c.args.start as usize + 1]];
    let top = call(id)?;
    let (search, (head, tail, negated)) = if starts_with(top.funcid) {
        (top, twin_spelling(true, top.funcid, None, None)?)
    } else {
        let [a, b] = args(top);
        let pos = |id| call(id).filter(|c| text_position(c.funcid));
        let (search, k, left) = match (pos(a), pos(b)) {
            (Some(s), _) => (s, b, true),
            (None, Some(s)) => (s, a, false),
            _ => return None,
        };
        let ExprNode::Const(k) = &p.nodes[k.0 as usize] else { return None };
        let v = k.read(); // the compare's by-value word decides the class: both keys hold it (`textpos_class`)
        if !k.byval || v.isnull {
            return None;
        }
        (search, twin_spelling(false, top.funcid, Some(left), Some(v.value.as_u64() as i32))?)
    };
    let [arg, needle] = args(search);
    let ExprNode::Const(c) = &p.nodes[needle.0 as usize] else { return None };
    let utf8 = gravity_fn::typesem::collation::database_encoding() == PG_UTF8;
    if c.is_null() || c.typlen != -1 || search.collation == 0 || !deterministic(search.collation) || !utf8 {
        return None;
    }
    let pattern = with_const_bytes(c, &mut |b| Some(mcx::to_vec_in(&twin_pattern(b, head, tail), m)))?;
    Some(TextSearch { arg, pattern, negated })
}

/// E2: an int4 compare of a textpos call (strpos / position, or an operator over textpos: `text_search` takes both
/// node forms) with a non-null by-value constant, either side, whatever the needle, text or collation, read in the plan
/// tree: `Some` of `textpos_test`'s class (the needle occurs, does not, neither). `text_search` bakes it (the LIKE twin's
/// table, lowerv4 step/text_like.rs's claim), so both keys hold it, never the literal (lower inputs/key.rs and exec
/// inputs/front.rs hash this), as D1103: `strpos(x, c) > 5` right after `> 0` ran LIKE's.
pub fn textpos_class(funcid: u32, args: &types_nodes::list::NodeList<'_>) -> Option<Option<bool>> {
    if !gravity_fn::typesem::expr::int4_compare(funcid) || args.len() != 2 {
        return None;
    }
    let pos = |n: types_nodes::Node<'_>| n.as_func_expr().map(|f| f.funcid).or_else(|| n.as_op_expr().map(|o| o.opfuncid)).is_some_and(gravity_fn::typesem::expr::text_position);
    let k = |n: types_nodes::Node<'_>| n.as_const().filter(|c| !c.constisnull && c.constbyval).map(|c| c.constvalue.as_usize() as i32);
    let [a, b] = [args.iter().next()?, args.iter().nth(1)?];
    let (k, left) = if pos(a) { (k(b)?, true) } else if pos(b) { (k(a)?, false) } else { return None };
    Some(gravity_program::twin::textpos_test(funcid, k, left))
}

/// D3374: an int4 compare of a size reader that is NULL for a NULL argument only and positive otherwise (typesem
/// `image::sized_when_set`: pg_column_size) with the other operand: `Some(true)` when that operand is the non-null
/// constant 0 on the side that makes the compare the reader's `> 0` (typesem `image::positive_operand`: `reader > 0`,
/// `0 < reader`), whose answer is its argument's NULL-ness whatever image the reader reads (lowerv4 step/expr_row.rs
/// serves the reader there); `Some(false)` for any other operand; `None` for any other call. Both keys hold it, never the
/// literal (lower inputs/key.rs and exec inputs/front.rs hash this), as D1103: `pg_column_size(x) > 5` right after
/// `> 0` files another program.
pub fn sized_class(funcid: u32, args: &types_nodes::list::NodeList<'_>) -> Option<bool> {
    use gravity_fn::typesem::image::{positive_operand, sized_when_set};
    let side = positive_operand(funcid).filter(|_| args.len() == 2)?;
    args.iter().nth(side)?.as_func_expr().filter(|f| sized_when_set(f.funcid))?;
    let zero = |n: types_nodes::Node<'_>| n.as_const().is_some_and(|c| !c.constisnull && c.constbyval && c.constvalue.as_usize() as i32 == 0);
    Some(args.iter().nth(1 - side).is_some_and(zero))
}

/// The islands' identity for the owner's sig (`None` without islands): two programs of one IR whose islands differ
/// (`JSON_VALUE(j, '$.a')` / `'$.b'`, `length(current_user)` / `length(session_user)`) must not share the cached
/// program. Each island by `put_island`: its whole range by content, by construction (D341).
pub fn islands_sig(p: &Program<'_>, scratch: Mcx<'_>) -> Option<u64> {
    if p.islands.is_empty() {
        return None;
    }
    let mut out = Out::new(scratch);
    for isl in p.islands.iter() {
        put_island(p, isl, &mut out);
    }
    Some(gravity_abi::hash::hash_bytes64(&out.0) | 1)
}

/// The subtree's identity: equal fingerprints are one table (step/percode.rs `declare_table`, D319), so two roots that
/// answer differently on one entry must print differently (D341: `print` by construction); and whether it holds a
/// by-value word (`print_root`).
fn fingerprint(p: &Program<'_>, id: NodeId, scratch: Mcx<'_>) -> (u64, bool) {
    fingerprint_bound(p, id, &|_| false, scratch)
}

/// NEW-72 entry C: `fingerprint` with the constants `bound` (a bound table's, `bound_const`) printed as words: their type,
/// never their image (no bake), and the root marked `words` (its own table, D1100's rule for a by-value word).
fn fingerprint_bound(p: &Program<'_>, id: NodeId, bound: &dyn Fn(NodeId) -> bool, scratch: Mcx<'_>) -> (u64, bool) {
    let (mut bytes, mut words) = (Out::new(scratch), false);
    print_root(p, id, bound, &mut bytes, &mut words);
    (gravity_abi::hash::hash_bytes64(&bytes.0) | 1, words)
}

/// NEW-72 entry C: whether `id` is a Const node a bound table reads per execution (`Known::EqBound`'s consts,
/// `Known::RangeBound`'s).
fn bound_const(k: &Known, id: NodeId) -> bool {
    match k {
        Known::EqBound { consts, .. } => consts.iter().any(|c| c.node == id),
        Known::RangeBound { at, .. } => at.node == id,
        _ => false,
    }
}

/// `print` for a root's fingerprint. D1100 (D1099's audit, main 10:17Z): a non-null by-value constant outside an island
/// is a word the program loads per execution, and the content key (lower/src/key.rs) does not hold its value, so the
/// fingerprint prints its type, never its value, and marks the root `words`: the lowerer keeps such a root its own table
/// (step/percode.rs `attach`). Printed by value, `length(t) > 5` twice was one table, and a statement differing only in
/// its second literal (`> 10`) ran that cached program, its second root reading the table its first root's words swept.
/// A root without one still merges by content (D319); an island prints by content (key.rs hashes the constants under it).
fn print_root(p: &Program<'_>, id: NodeId, bound: &dyn Fn(NodeId) -> bool, out: &mut Out<'_>, words: &mut bool) {
    crate::stack_check();
    let island = p.islands.iter().any(|i| i.root == id);
    match p.nodes.get(id.0 as usize) {
        Some(ExprNode::Const(c)) if c.byval && !c.is_null() && !island => {
            *words = true;
            out.push(0xFD);
            le(out, c.typ);
        }
        // NEW-72 entry C: a bound table's constant (`fingerprint_bound`)
        Some(ExprNode::Const(c)) if bound(id) && !island => {
            *words = true;
            out.push(0xFC);
            le(out, c.typ);
        }
        _ if island => print(p, id, out),
        _ => put_node(p, id, out, &mut |out, k| print_root(p, k, bound, out, words)),
    }
}

/// A node by content: an interpreter island's root as the island (`put_island`: the interpreter evaluates its whole
/// range), any other node by `put_node` with each child printed in place, so equal subtrees print equal bytes wherever
/// they sit and differing ones differ (D341: the kind byte alone let `length(current_user)` share
/// `length(session_user)`'s table, `LEAST` `GREATEST`'s, `IS NULL` `IS NOT NULL`'s).
fn print(p: &Program<'_>, id: NodeId, out: &mut Out<'_>) {
    crate::stack_check();
    if let Some(isl) = p.islands.iter().find(|i| i.root == id) {
        put_island(p, isl, out);
        return;
    }
    put_node(p, id, out, &mut |out, k| print(p, k, out));
}

/// An island by its whole range `first..=root`: every node by `put_node` with a child inside the range as its offset
/// (one outside printed in place), the island's own site, then the name / oid / PASSING tables its nodes index.
fn put_island<'s>(p: &Program<'_>, isl: &Island, out: &mut Out<'s>) {
    let Island { root, first, site, src: _, once: _ } = *isl; // `src` is the plan node's address: the key's, not the content's; `once` follows the content
    let (lo, hi) = (first.0, root.0);
    out.push(0xFF);
    le(out, hi.wrapping_sub(lo));
    let mut kid = |out: &mut Out<'s>, k: NodeId| {
        if (lo..=hi).contains(&k.0) {
            out.push(0);
            le(out, k.0 - lo);
        } else {
            out.push(1);
            print(p, k, out);
        }
    };
    for i in lo..=hi {
        put_node(p, NodeId(i), out, &mut kid);
    }
    put_site(p, site, out, &mut kid);
    let Program { nodes: _, kids: _, sites: _, convs: _, names, oids, json_vars, owned: _, naggs: _, islands: _ } = p;
    le(out, names.len() as u32);
    for s in names.iter() {
        put_str(out, s);
    }
    le(out, oids.len() as u32);
    for o in oids.iter() {
        le(out, *o);
    }
    le(out, json_vars.len() as u32);
    for v in json_vars.iter() {
        put_json_var(out, Some(v));
    }
}

/// One node's content by construction: an exhaustive match (no `_` arm) with each struct destructured whole, so a new
/// kind or field does not build until it is printed. Scalars as bytes, enums and value structs by `Debug`, a call site by
/// its content (`put_site`), a side-table index by the entries it names, each child through `kid`, lists count-prefixed.
fn put_node<'s>(p: &Program<'_>, id: NodeId, out: &mut Out<'s>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
    use crate::nodes::{agg, array, bool as boolx, case, cast, collate, field, json, misc, null, op, param, row, rowcmp, srf, var};
    let Some(n) = p.nodes.get(id.0 as usize) else {
        out.push(0xFE);
        le(out, id.0);
        return;
    };
    match n {
        ExprNode::Var(var::Var { src, attno, typ, desc }) => {
            out.push(1);
            dbg(out, src);
            le(out, *attno);
            le(out, *typ);
            le(out, *desc);
        }
        ExprNode::Const(c) => {
            // the image is a bake: the per-code table is the literal's
            let (typ, typlen, byval, value) = c.fields();
            out.push(2);
            le(out, typ);
            le(out, typlen);
            le(out, byval);
            le(out, value.isnull);
            if !value.isnull {
                put_image(out, c);
            }
        }
        ExprNode::Param(param::Param { kind, id, typ }) => {
            out.push(3);
            dbg(out, kind);
            le(out, *id);
            le(out, *typ);
        }
        ExprNode::Func(func::FuncNode { site }) => {
            out.push(4);
            put_site(p, *site, out, kid);
        }
        ExprNode::Op(func::FuncNode { site }) => {
            out.push(5);
            put_site(p, *site, out, kid);
        }
        ExprNode::Distinct(func::FuncNode { site }) => {
            out.push(6);
            put_site(p, *site, out, kid);
        }
        ExprNode::NullIf(func::FuncNode { site }) => {
            out.push(7);
            put_site(p, *site, out, kid);
        }
        ExprNode::ScalarArrayOp(op::ScalarArrayOp { site, use_or, elem }) => {
            out.push(8);
            put_site(p, *site, out, kid);
            le(out, *use_or);
            dbg(out, elem);
        }
        ExprNode::Bool(boolx::BoolNode { op, args }) => {
            out.push(9);
            dbg(out, op);
            put_kids(p, *args, out, kid);
        }
        ExprNode::NullTest(null::NullTestNode { arg, is_null, row, site }) => {
            out.push(10);
            put_ref(out, *arg, kid);
            le(out, *is_null);
            le(out, *row);
            if let Some(site) = site {
                put_site(p, *site, out, kid);
            }
        }
        ExprNode::BooleanTest(null::BooleanTestNode { arg, test }) => {
            out.push(11);
            put_ref(out, *arg, kid);
            dbg(out, test);
        }
        ExprNode::Case(case::Case { arg, whens, default }) => {
            out.push(12);
            put_opt(out, *arg, kid);
            put_kids(p, *whens, out, kid);
            put_opt(out, *default, kid);
        }
        ExprNode::CaseTest => out.push(13),
        ExprNode::Coalesce(case::Coalesce { args }) => {
            out.push(14);
            put_kids(p, *args, out, kid);
        }
        ExprNode::MinMax(case::MinMax { args, least, cmp }) => {
            out.push(15);
            put_kids(p, *args, out, kid);
            le(out, *least);
            put_site(p, *cmp, out, kid);
        }
        ExprNode::Relabel(cast::Relabel { arg, canon }) => {
            out.push(16);
            put_ref(out, *arg, kid);
            out.push(*canon as u8);
        }
        ExprNode::CoerceViaIO(cast::CoerceViaIO { arg, out: s_out, inp, ioparam, direct }) => {
            out.push(17);
            out.push(*direct as u8);
            put_ref(out, *arg, kid);
            put_site(p, *s_out, out, kid);
            put_site(p, *inp, out, kid);
            le(out, *ioparam);
        }
        ExprNode::CoerceToDomain(cast::CoerceToDomain { arg, typ, notnull, checks, names, site }) => {
            out.push(18);
            put_ref(out, *arg, kid);
            le(out, *typ);
            le(out, *notnull);
            put_kids(p, *checks, out, kid);
            for i in 0..checks.len {
                put_opt_str(out, p.names.get((*names + i) as usize).copied());
            }
            put_site(p, *site, out, kid);
        }
        ExprNode::CoerceToDomainValue => out.push(19),
        ExprNode::RowCompare(rowcmp::RowCompare { cmptype, largs, rargs, sites }) => {
            out.push(20);
            le(out, *cmptype);
            put_kids(p, *largs, out, kid);
            put_kids(p, *rargs, out, kid);
            for i in 0..largs.len {
                put_site(p, SiteId(*sites + i), out, kid);
            }
        }
        ExprNode::FieldSelect(field::FieldSelect { arg, fieldnum, typ, site, desc }) => {
            out.push(21);
            put_ref(out, *arg, kid);
            le(out, *fieldnum);
            le(out, *typ);
            put_site(p, *site, out, kid);
            le(out, *desc);
        }
        ExprNode::FieldStore(field::FieldStore { arg, newvals, fieldnums, typ, desc, site }) => {
            out.push(22);
            put_ref(out, *arg, kid);
            put_kids(p, *newvals, out, kid);
            for i in 0..newvals.len {
                dbg(out, &p.oids.get((*fieldnums + i) as usize));
            }
            le(out, *typ);
            le(out, *desc);
            put_site(p, *site, out, kid);
        }
        ExprNode::Subscripting(array::Subscript { container, upper, lower, assign, handler, result_type, container_is_param, fetch_old, site }) => {
            out.push(23);
            put_ref(out, *container, kid);
            put_kids(p, *upper, out, kid);
            put_kids(p, *lower, out, kid);
            put_opt(out, *assign, kid);
            dbg(out, handler);
            le(out, *result_type);
            le(out, *container_is_param);
            le(out, *fetch_old);
            put_site(p, *site, out, kid);
        }
        ExprNode::Array(array::ArrayExpr { elements, array_type, elem, multidims, site }) => {
            out.push(24);
            put_kids(p, *elements, out, kid);
            le(out, *array_type);
            dbg(out, elem);
            le(out, *multidims);
            put_site(p, *site, out, kid);
        }
        ExprNode::ArrayCoerce(array::ArrayCoerce { arg, elemexpr, result_type, out: e_out, inp }) => {
            out.push(25);
            put_ref(out, *arg, kid);
            put_opt(out, *elemexpr, kid);
            le(out, *result_type);
            dbg(out, e_out);
            dbg(out, inp);
        }
        ExprNode::Row(row::RowExpr { args, row_type, desc, natts, site }) => {
            out.push(26);
            put_kids(p, *args, out, kid);
            le(out, *row_type);
            le(out, *desc);
            le(out, *natts);
            put_site(p, *site, out, kid);
        }
        ExprNode::ConvertRowtype(row::ConvertRowtype { arg, result_type, indesc, outdesc, map, site, wide }) => {
            out.push(27);
            put_ref(out, *arg, kid);
            le(out, *result_type);
            le(out, *indesc);
            le(out, *outdesc);
            dbg(out, map);
            put_site(p, *site, out, kid);
            le(out, *wide);
        }
        ExprNode::Collate(collate::Collate { arg, collation }) => {
            out.push(28);
            put_ref(out, *arg, kid);
            le(out, *collation);
        }
        ExprNode::SqlValueFunction(misc::SqlValueFunction { op, typ, typmod }) => {
            out.push(29);
            dbg(out, op);
            le(out, *typ);
            le(out, *typmod);
        }
        ExprNode::Xml(misc::Xml { op, args, named_args, typ, name, names, types, xmloption, indent }) => {
            out.push(30);
            dbg(out, op);
            put_kids(p, *args, out, kid);
            put_kids(p, *named_args, out, kid);
            le(out, *typ);
            put_opt_str(out, name.and_then(|i| p.names.get(i as usize).copied()));
            for i in 0..named_args.len {
                put_opt_str(out, p.names.get((*names + i) as usize).copied());
            }
            for i in 0..named_args.len + args.len {
                dbg(out, &p.oids.get((*types + i) as usize));
            }
            dbg(out, xmloption);
            le(out, *indent);
        }
        ExprNode::Json(j) => {
            out.push(31);
            put_json(p, j, out, kid);
        }
        ExprNode::NextValue(misc::NextValue { seqid, typ, site }) => {
            out.push(32);
            le(out, *seqid);
            le(out, *typ);
            put_site(p, *site, out, kid);
        }
        ExprNode::CurrentOf(misc::CurrentOf { cvarno, cursor_param }) => {
            out.push(33);
            le(out, *cvarno);
            le(out, *cursor_param);
        }
        ExprNode::Aggref(agg::Aggref { aggno, typ }) => {
            out.push(34);
            le(out, *aggno);
            le(out, *typ);
        }
        ExprNode::WindowFunc(agg::WindowFunc { cell, winref, typ }) => {
            out.push(35);
            le(out, *cell);
            le(out, *winref);
            le(out, *typ);
        }
        ExprNode::GroupingFunc(agg::GroupingFunc { nargs }) => {
            out.push(36);
            le(out, *nargs);
        }
        ExprNode::SubPlan(srf::SubPlan { plan_id, cell, typ }) => {
            out.push(37);
            le(out, *plan_id);
            le(out, *cell);
            le(out, *typ);
        }
        ExprNode::Srf(srf::Srf { funcid, cell, typ }) => {
            out.push(38);
            le(out, *funcid);
            le(out, *cell);
            le(out, *typ);
        }
        ExprNode::Returning(misc::Returning { arg, old }) => {
            out.push(39);
            put_ref(out, *arg, kid);
            le(out, *old);
        }
        ExprNode::Imm(misc::Imm { value, typ }) => {
            out.push(40);
            le(out, *value);
            le(out, *typ);
        }
    }
    // the JSON family's nodes, each struct whole (`json::JsonNode`)
    fn put_json<'s>(p: &Program<'_>, j: &json::JsonNode, out: &mut Out<'s>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
        match j {
            json::JsonNode::Expr(e) => {
                let json::JsonExprNode { op, formatted, path, passing, vars, wrapper, returning, throw_error, on_error, on_empty, io, json, column_name, ioparam, ret_typmod, ret_len, ret_byval } = e;
                out.push(1);
                dbg(out, op);
                put_ref(out, *formatted, kid);
                put_ref(out, *path, kid);
                put_kids(p, *passing, out, kid);
                for i in 0..passing.len {
                    put_json_var(out, p.json_vars.get((*vars + i) as usize));
                }
                dbg(out, wrapper);
                le(out, *returning);
                le(out, *throw_error);
                put_behavior(out, Some(on_error), kid);
                put_behavior(out, on_empty.as_ref(), kid);
                dbg(out, io);
                dbg(out, json);
                put_opt_str(out, column_name.and_then(|i| p.names.get(i as usize).copied()));
                le(out, *ioparam);
                le(out, *ret_typmod);
                le(out, *ret_len);
                le(out, *ret_byval);
            }
            json::JsonNode::Func(f) => {
                out.push(2);
                put_ref(out, *f, kid);
            }
            json::JsonNode::Constructor(json::Constructor { ctor, func, args, types, is_jsonb, absent_on_null, unique, cat, coercion }) => {
                out.push(3);
                dbg(out, ctor);
                put_opt(out, *func, kid);
                put_kids(p, *args, out, kid);
                for i in 0..args.len {
                    dbg(out, &p.oids.get((*types + i) as usize));
                }
                le(out, *is_jsonb);
                le(out, *absent_on_null);
                le(out, *unique);
                le(out, *cat);
                put_opt(out, *coercion, kid);
            }
            json::JsonNode::IsJson(json::IsJson { arg, exprtype, item_type, unique_keys }) => {
                out.push(4);
                put_ref(out, *arg, kid);
                le(out, *exprtype);
                dbg(out, item_type);
                le(out, *unique_keys);
            }
            json::JsonNode::Value(v) => {
                out.push(5);
                put_ref(out, *v, kid);
            }
        }
    }
    fn put_behavior<'s>(out: &mut Out<'s>, b: Option<&json::Behavior>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
        let Some(json::Behavior { btype, expr, coerce, needs_finish }) = b else {
            out.push(0);
            return;
        };
        out.push(1);
        dbg(out, btype);
        put_opt(out, *expr, kid);
        le(out, *coerce);
        le(out, *needs_finish);
    }
}

/// A call site by content: every `Call` field, its arguments through `kid`, a shim by its catalog row and argument
/// conversions (a manager call's slot is the site's own: its content is the site's).
fn put_site<'s>(p: &Program<'_>, s: SiteId, out: &mut Out<'s>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
    let Some(Call { funcid, args, nargs, strict, collation, how, rettype, volatile }) = p.sites.get(s.0 as usize) else {
        out.push(0xFE);
        le(out, s.0);
        return;
    };
    le(out, *funcid);
    put_kids(p, *args, out, kid);
    le(out, *nargs);
    le(out, *strict);
    le(out, *collation);
    match how {
        How::Shim { shim, convs, byref } => {
            out.push(0);
            le(out, shim.oid);
            put_str(out, shim.name);
            for i in 0..*nargs as u32 {
                match p.convs.get((*convs + i) as usize) {
                    Some(ArgConv { kind, typlen }) => {
                        dbg(out, kind);
                        le(out, *typlen);
                    }
                    None => out.push(0xFE),
                }
            }
            le(out, *byref);
        }
        How::Fmgr(_) => out.push(1),
        How::Island { root } => {
            out.push(2);
            put_ref(out, *root, kid);
        }
    }
    le(out, *rettype);
    le(out, *volatile);
}

/// A constant's bytes (`const_bytes`; an image that cannot be expanded by its address, so it equals no other constant).
fn put_image(out: &mut Out<'_>, c: &ConstNode<'_>) {
    let wrote = with_const_bytes(c, &mut |b| {
        out.push(1);
        put_bytes(out, b);
        Some(())
    });
    match wrote {
        Some(()) => {}
        None => {
            out.push(2);
            le(out, c.read().value.as_u64());
        }
    }
}

fn put_kids<'s>(p: &Program<'_>, k: Kids, out: &mut Out<'s>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
    le(out, k.len);
    for i in 0..k.len {
        match p.kids.get((k.start + i) as usize) {
            Some(&id) => put_ref(out, id, kid),
            None => out.push(0xFE),
        }
    }
}

/// A child reference (`NodeId::NONE`, an open subscript bound, as its own mark).
fn put_ref<'s>(out: &mut Out<'s>, id: NodeId, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
    if id == NodeId::NONE {
        out.push(0xFD);
    } else {
        kid(out, id);
    }
}

fn put_opt<'s>(out: &mut Out<'s>, id: Option<NodeId>, kid: &mut dyn FnMut(&mut Out<'s>, NodeId)) {
    match id {
        Some(id) => {
            out.push(1);
            put_ref(out, id, kid);
        }
        None => out.push(0),
    }
}

fn put_json_var(out: &mut Out<'_>, v: Option<&crate::nodes::json::JsonVar>) {
    let Some(crate::nodes::json::JsonVar { name, typid, typmod }) = v else {
        out.push(0);
        return;
    };
    out.push(1);
    put_str(out, name);
    le(out, *typid);
    le(out, *typmod);
}

fn put_opt_str(out: &mut Out<'_>, s: Option<&str>) {
    match s {
        Some(s) => {
            out.push(1);
            put_str(out, s);
        }
        None => out.push(0),
    }
}

fn put_str(out: &mut Out<'_>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn put_bytes(out: &mut Out<'_>, b: &[u8]) {
    le(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// An enum or value struct by its derived `Debug` (every field, by construction), length-prefixed.
fn dbg(out: &mut Out<'_>, v: &dyn core::fmt::Debug) {
    use core::fmt::Write;
    let at = out.0.len();
    out.extend_from_slice(&[0u8; 4]);
    let _ = write!(out, "{v:?}");
    let n = (out.0.len() - at - 4) as u32;
    out.0[at..at + 4].copy_from_slice(&n.to_le_bytes());
}

/// The bytes a fingerprint prints (`print_root`, `put_island`), in the caller's scratch context: hashed, then dropped.
struct Out<'s>(PgVec<'s, u8>);

impl<'s> Out<'s> {
    fn new(scratch: Mcx<'s>) -> Out<'s> {
        Out(PgVec::new_in(scratch))
    }

    fn push(&mut self, b: u8) {
        self.0.push_in(b);
    }

    fn extend_from_slice(&mut self, b: &[u8]) {
        self.0.extend_from_slice_in(b);
    }
}

impl core::fmt::Write for Out<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

trait Le {
    fn put(self, out: &mut Out<'_>);
}

macro_rules! le_int {
    ($($t:ty),*) => {$(impl Le for $t {
        fn put(self, out: &mut Out<'_>) {
            out.extend_from_slice(&self.to_le_bytes());
        }
    })*};
}
le_int!(u8, u16, u32, u64, i8, i16, i32, i64);

impl Le for bool {
    fn put(self, out: &mut Out<'_>) {
        out.push(self as u8);
    }
}

fn le<T: Le>(out: &mut Out<'_>, v: T) {
    v.put(out);
}

/// The children an emit walk descends into: `emit::kids`, none for an interpreter island's root (its subtree is the
/// interpreter's: one host call).
pub fn walk_kids<'p>(p: &'p Program<'_>, id: NodeId) -> KidIter<'p> {
    if p.islands.iter().any(|i| i.root == id) {
        return KidIter::none();
    }
    kids(p, id)
}

/// The tier B / C sites called per row outside the per-code roots (their bodies run per entry).
pub(crate) fn host_calls(p: &Program<'_>, id: NodeId, plan: &Plan) -> usize {
    crate::stack_check();
    if plan.root_at(id).is_some() {
        return 0;
    }
    let here = match &p.nodes[id.0 as usize] {
        ExprNode::Func(n) | ExprNode::Op(n) | ExprNode::Distinct(n) | ExprNode::NullIf(n) => !crate::emit::prim::is_prim(p, &p.sites[n.site.0 as usize], false) as usize,
        ExprNode::ScalarArrayOp(n) => (!crate::emit::prim::is_prim(p, &p.sites[n.site.0 as usize], false) || crate::emit::scalar::walked(p, n)) as usize,
        ExprNode::NullTest(t) => t.row as usize,
        _ => 0,
    };
    here + walk_kids(p, id).into_iter().map(|k| host_calls(p, k, plan)).sum::<usize>()
}

#[derive(Debug)]
pub enum FillError {
    Region(BuildError),
    /// The need's element kind is not one the fill builds (`Form::Code` / `Empty` have no table).
    Form,
}

impl From<BuildError> for FillError {
    fn from(e: BuildError) -> FillError {
        FillError::Region(e)
    }
}

/// The 4B varlena image of `b` at the front of `scratch` (`None` when it does not fit).
fn image(scratch: &mut [u8], b: &[u8]) -> Option<Datum> {
    let n = b.len() + 4;
    if n > scratch.len() {
        return None;
    }
    scratch[..4].copy_from_slice(&datum::varlena::set_varsize_4b(n));
    scratch[4..n].copy_from_slice(b);
    Some(Datum::from_usize(scratch.as_ptr() as usize))
}

/// The need's table built by tier 0: the body evaluated per entry in code order over a one-column
/// row holding the entry's image (the first half of `scratch`; by-reference intermediates in the
/// second half); an error or an image past the scratch is a fault bit. The table lives in `region`.
pub fn fill(c: &mut Compiled<'_>, need: &TableNeed, dict: &dyn Dict, region: &mut gravity_dict::Region, scratch: &mut [u8]) -> Result<*const Header, FillError> {
    let n = need.attno as usize;
    let mut values = vec![Datum::null(); n]; // alloc: Cold
    let nulls = vec![false; n]; // alloc: Cold
    let (img, rest) = scratch.split_at_mut(scratch.len() / 2);
    let Compiled { prog, state, params, callback_arg, .. } = c;
    let (prog, mut state) = (prog.view(), state.view());
    let mut ev = |v: View| -> Result<NullableDatum, Fault> {
        // SAFETY: the dictionary's entry bytes are live through the build's sweep.
        let d = image(img, unsafe { v.bytes() }).ok_or(FAULT_SCRATCH)?;
        values[n - 1] = d;
        let row = Row::scan(&values, &nulls);
        let mut ereg = gravity_fn::Region::over(rest);
        let mut cx = Cx::new(&row, &mut ereg, params, *callback_arg);
        eval::eval(&prog, &mut state, need.root, &mut cx).map_err(|_| FAULT_BODY)
    };
    let hdr = match need.elem {
        gravity_dict::ELEM_BITS => gravity_dict::Bits::build(dict, region, &mut |v| Ok(ev(v)?.value.as_i64() != 0))?.header(),
        gravity_dict::ELEM_I64 => gravity_dict::I64::build(dict, region, &mut |v| Ok(ev(v)?.value.as_i64()))?.header(),
        gravity_dict::ELEM_DERIVED => gravity_dict::Derived::build(dict, region, &mut |v, region| {
            let r = ev(v)?;
            let mut scratch = [0u8; 256];
            let mut small = gravity_fn::Region::over(&mut scratch);
            let view = payload_view(r.value, -1, &mut small).map_err(|_| Halt::Fault(FAULT_BODY))?;
            Ok(region.copy(view.bytes())?)
        })?
        .header(),
        _ => return Err(FillError::Form),
    };
    Ok(hdr)
}

/// D341: two per-code roots that answer differently on one entry print differently (one D319 table otherwise), equal ones
/// print equally wherever they sit, and two programs whose islands differ have different sigs. Every island kind
/// (`nodes::ISLAND_TAGS`, D337's list: a new kind fails here until it has a twin) is a twin twice: a field of the island's
/// root, and a constant inside the island.
/// The fingerprint walks with a context of their own (a test's).
#[cfg(test)]
fn fingerprint_t(p: &Program<'_>, id: NodeId) -> (u64, bool) {
    let cx = mcx::MemoryContext::new("percode test");
    fingerprint(p, id, cx.mcx())
}

#[cfg(test)]
fn fingerprint_bound_t(p: &Program<'_>, id: NodeId, bound: &dyn Fn(NodeId) -> bool) -> (u64, bool) {
    let cx = mcx::MemoryContext::new("percode test");
    fingerprint_bound(p, id, bound, cx.mcx())
}

#[cfg(test)]
fn islands_sig_t(p: &Program<'_>) -> Option<u64> {
    let cx = mcx::MemoryContext::new("percode test");
    islands_sig(p, cx.mcx())
}

#[cfg(test)]
fn analyze_t<C: Cols + ?Sized>(p: &Program<'_>, root: NodeId, codes: &C) -> Result<Plan<'static>, EmitError> {
    analyze(p, root, codes, tcx())
}

/// A context for the tests' plans and `Known`s (leaked: they outlive the call that made them).
#[cfg(test)]
fn tcx() -> Mcx<'static> {
    Box::leak(Box::new(mcx::MemoryContext::new_bump("percode test"))).mcx() // alloc: Cold
}

/// A test list of the bytes `b`.
#[cfg(test)]
fn bv(b: &[u8]) -> PgVec<'static, u8> {
    mcx::to_vec_in(b, tcx())
}

#[cfg(test)]
mod d341 {
    use types_nodes::primnodes::{BoolTestType, JsonBehaviorType as JBT, JsonConstructorType as JC, JsonExprOp, JsonValueType as JT, SQLValueFunctionOp as Svf, XmlExprOp, XmlOptionType};
    use types_nodes::NodeTag;

    use super::*;
    use crate::nodes::array::{ArrayCoerce, ElemFacts};
    use crate::nodes::case::{Case, Coalesce, MinMax};
    use crate::nodes::cast::CoerceViaIO;
    use crate::nodes::json::{Behavior, Constructor, IsJson, JsonExprNode, JsonNode};
    use crate::nodes::misc::{Imm, SqlValueFunction, Xml};
    use crate::nodes::null::{BooleanTestNode, NullTestNode};
    use crate::nodes::rowcmp::RowCompare;
    use crate::nodes::ISLAND_TAGS;

    const INT4GT: Oid = 147;
    const INT4LT: Oid = 66;
    const CONCAT: Oid = 3058;
    const ELEM: ElemFacts = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };

    pub(super) struct B(pub(super) Program<'static>);

    impl B {
        pub(super) fn new() -> B {
            B(Program::empty_in(crate::percode::tcx()))
        }
        pub(super) fn node(&mut self, n: ExprNode<'static>) -> NodeId {
            self.0.nodes.push(n);
            NodeId(self.0.nodes.len() as u32 - 1)
        }
        pub(super) fn kids(&mut self, ids: &[NodeId]) -> Kids {
            let start = self.0.kids.len() as u32;
            self.0.kids.extend_from_slice(ids);
            Kids { start, len: ids.len() as u32 }
        }
        pub(super) fn call(&mut self, funcid: Oid, args: &[NodeId], collation: Oid, how: How) -> SiteId {
            let args = self.kids(args);
            self.0.sites.push(Call { funcid, args, nargs: args.len as u8, strict: true, collation, how, rettype: 23, volatile: false });
            SiteId(self.0.sites.len() as u32 - 1)
        }
        pub(super) fn site(&mut self, funcid: Oid, args: &[NodeId]) -> SiteId {
            self.call(funcid, args, 0, How::Fmgr(0))
        }
        pub(super) fn var(&mut self, attno: i16) -> NodeId {
            self.node(ExprNode::Var(crate::nodes::var::Var { src: eval::Source::Scan, attno, typ: 25, desc: 0 }))
        }
        pub(super) fn int(&mut self, v: Option<i32>) -> NodeId {
            let value = match v {
                Some(v) => NullableDatum::value(Datum::from_i32(v)),
                None => NullableDatum { value: Datum::from_i32(0), isnull: true },
            };
            self.node(ExprNode::Const(ConstNode::new(value, 23, 4, true)))
        }
        pub(super) fn func(&mut self, funcid: Oid, args: &[NodeId]) -> NodeId {
            let site = self.site(funcid, args);
            self.node(ExprNode::Func(func::FuncNode { site }))
        }
        /// `length(t) > length(inner)` over code column 1: `inner` is the part that differs.
        pub(super) fn root(&mut self, inner: NodeId) -> NodeId {
            let t = self.var(1);
            let l = self.func(F_LENGTH[0], &[t]);
            let r = self.func(F_LENGTH[0], &[inner]);
            let site = self.site(INT4GT, &[l, r]);
            self.node(ExprNode::Op(func::FuncNode { site }))
        }
        /// An island of `tag` over column 2 (`Builder::island`'s shape: the range, then its host site), `field` flipping a
        /// field of its root and `k` the constant inside it.
        pub(super) fn island(&mut self, tag: NodeTag, field: bool, k: i32) -> NodeId {
            let first = NodeId(self.0.nodes.len() as u32);
            let root = self.island_root(tag, field, k);
            let col = self.var(2);
            let site = self.call(0, &[col], 0, How::Island { root });
            self.0.islands.push(Island { root, first, site, src: 0, once: None });
            root
        }
        pub(super) fn island_root(&mut self, tag: NodeTag, field: bool, k: i32) -> NodeId {
            let x = self.var(2);
            match tag {
                NodeTag::T_NullTest => {
                    let c = self.int(Some(k));
                    let row = self.kids(&[x, c]);
                    let arg = self.node(ExprNode::Coalesce(Coalesce { args: row }));
                    self.node(ExprNode::NullTest(NullTestNode { arg, is_null: field, row: true, site: None }))
                }
                NodeTag::T_MinMaxExpr => {
                    let c = self.int(Some(k));
                    let args = self.kids(&[x, c]);
                    let cmp = self.site(INT4GT, &[]);
                    self.node(ExprNode::MinMax(MinMax { args, least: field, cmp }))
                }
                NodeTag::T_RowCompareExpr => {
                    let c = self.int(Some(k));
                    let (largs, rargs) = (self.kids(&[x]), self.kids(&[c]));
                    let sites = self.site(if field { INT4LT } else { INT4GT }, &[]).0;
                    self.node(ExprNode::RowCompare(RowCompare { cmptype: 1, largs, rargs, sites }))
                }
                NodeTag::T_ArrayCoerceExpr => {
                    let elemexpr = Some(self.int(Some(k)));
                    self.node(ExprNode::ArrayCoerce(ArrayCoerce { arg: x, elemexpr, result_type: if field { 1016 } else { 1007 }, out: ELEM, inp: ELEM }))
                }
                NodeTag::T_SQLValueFunction => {
                    let op = if field { Svf::SVFOP_SESSION_USER } else { Svf::SVFOP_CURRENT_USER };
                    self.node(ExprNode::SqlValueFunction(SqlValueFunction { op, typ: 19, typmod: -1 }))
                }
                NodeTag::T_XmlExpr => {
                    let c = self.int(Some(k));
                    let args = self.kids(&[x, c]);
                    let name = Some(self.0.names.len() as u32);
                    self.0.names.push(if field { "b" } else { "a" });
                    let types = self.0.oids.len() as u32;
                    self.0.oids.extend_from_slice(&[25, 23]);
                    let named_args = Kids::default();
                    let (op, xmloption) = (XmlExprOp::IS_XMLELEMENT, XmlOptionType::XMLOPTION_CONTENT);
                    self.node(ExprNode::Xml(Xml { op, args, named_args, typ: 142, name, names: 0, types, xmloption, indent: false }))
                }
                NodeTag::T_JsonValueExpr => {
                    let c = self.int(Some(k));
                    let v = if field { c } else { self.func(CONCAT, &[x, c]) };
                    self.node(ExprNode::Json(JsonNode::Value(v)))
                }
                NodeTag::T_JsonConstructorExpr => {
                    let c = self.int(Some(k));
                    let args = self.kids(&[c, x]);
                    let types = self.0.oids.len() as u32;
                    self.0.oids.extend_from_slice(&[23, 25]);
                    let ctor = Constructor { ctor: JC::JSCTOR_JSON_OBJECT, func: None, args, types, is_jsonb: false, absent_on_null: field, unique: false, cat: 0, coercion: None };
                    self.node(ExprNode::Json(JsonNode::Constructor(ctor)))
                }
                NodeTag::T_JsonIsPredicate => {
                    let c = self.int(Some(k));
                    let arg = self.func(CONCAT, &[x, c]);
                    let item_type = if field { JT::JS_TYPE_ARRAY } else { JT::JS_TYPE_OBJECT };
                    self.node(ExprNode::Json(JsonNode::IsJson(IsJson { arg, exprtype: 25, item_type, unique_keys: false })))
                }
                NodeTag::T_JsonExpr => {
                    let path = self.int(Some(k));
                    let on_error = Behavior { btype: JBT::JSON_BEHAVIOR_NULL, expr: None, coerce: false, needs_finish: false };
                    let op = if field { JsonExprOp::JSON_QUERY_OP } else { JsonExprOp::JSON_VALUE_OP };
                    let e = JsonExprNode {
                        op,
                        formatted: x,
                        path,
                        passing: Kids::default(),
                        vars: 0,
                        wrapper: adt_jsonpath_exec::JsonWrapper::Unspec,
                        returning: 25,
                        throw_error: false,
                        on_error,
                        on_empty: None,
                        io: None,
                        json: None,
                        column_name: None,
                        ioparam: 25,
                        ret_typmod: -1,
                        ret_len: -1,
                        ret_byval: false,
                    };
                    self.node(ExprNode::Json(JsonNode::Expr(e)))
                }
                other => panic!("no island twin for {other:?}: add one with the kind (D341)"),
            }
        }
    }

    /// `(field, k)` of each twin against `(false, 1)`: a root field flipped, then the inner constant changed
    /// (`SQLValueFunction` has no argument: the field twin only).
    fn twins(tag: NodeTag) -> &'static [(bool, i32)] {
        match tag {
            NodeTag::T_SQLValueFunction => &[(true, 1)],
            _ => &[(true, 1), (false, 2)],
        }
    }

    #[test]
    fn every_island_kind_differing_inside_prints_and_signs_differently() {
        for tag in ISLAND_TAGS {
            for (i, &(field, k)) in twins(tag).iter().enumerate() {
                // one program, two roots (D319's case: one table for both if the fingerprints meet)
                let mut b = B::new();
                let ia = b.island(tag, false, 1);
                let ra = b.root(ia);
                let ib = b.island(tag, field, k);
                let rb = b.root(ib);
                assert_ne!(fingerprint_t(&b.0, ra), fingerprint_t(&b.0, rb), "{tag:?} twin {i}: one fingerprint");
                // two programs of one shape (the owner's sig)
                let mut pa = B::new();
                let ia = pa.island(tag, false, 1);
                pa.root(ia);
                let mut pb = B::new();
                let ib = pb.island(tag, field, k);
                pb.root(ib);
                assert_ne!(islands_sig_t(&pa.0), islands_sig_t(&pb.0), "{tag:?} twin {i}: one islands_sig");
            }
            // the same island twice in one program: one table, as before
            let mut b = B::new();
            let ia = b.island(tag, false, 1);
            let ra = b.root(ia);
            let ib = b.island(tag, false, 1);
            let rb = b.root(ib);
            assert_eq!(fingerprint_t(&b.0, ra), fingerprint_t(&b.0, rb), "{tag:?}: equal islands at two places");
        }
    }

    /// The kinds outside islands the kind byte hid (box 8, M1 tip: GREATEST / LEAST, IS [NOT] NULL, IS TRUE / FALSE
    /// answered one table), a list's arity, a site's collation, an immediate, a null constant.
    #[test]
    fn roots_differing_outside_islands_print_differently() {
        type Mk = fn(&mut B, bool) -> NodeId;
        let shapes: [(&str, Mk); 10] = [
            ("minmax.least", |b, v| {
                let (x, y) = (b.int(Some(3)), b.int(Some(4)));
                let args = b.kids(&[x, y]);
                let cmp = b.site(INT4GT, &[]);
                b.node(ExprNode::MinMax(MinMax { args, least: v, cmp }))
            }),
            ("nulltest.is_null", |b, v| {
                let x = b.int(Some(3));
                b.node(ExprNode::NullTest(NullTestNode { arg: x, is_null: v, row: false, site: None }))
            }),
            ("booleantest.test", |b, v| {
                let x = b.int(Some(3));
                let test = if v { BoolTestType::IS_FALSE } else { BoolTestType::IS_TRUE };
                b.node(ExprNode::BooleanTest(BooleanTestNode { arg: x, test }))
            }),
            ("coalesce.arity", |b, v| {
                let (x, y, z) = (b.int(Some(1)), b.int(Some(2)), b.int(Some(3)));
                if v {
                    let args = b.kids(&[x, y]);
                    let c = b.node(ExprNode::Coalesce(Coalesce { args }));
                    b.func(CONCAT, &[c, z])
                } else {
                    let args = b.kids(&[x, y, z]);
                    let c = b.node(ExprNode::Coalesce(Coalesce { args }));
                    b.func(CONCAT, &[c])
                }
            }),
            ("case.default", |b, v| {
                let (c, r, d) = (b.int(Some(1)), b.int(Some(2)), b.int(Some(3)));
                let whens = b.kids(&[c, r]);
                b.node(ExprNode::Case(Case { arg: None, whens, default: v.then_some(d) }))
            }),
            ("distinct.collation", |b, v| {
                let (x, y) = (b.int(Some(1)), b.int(Some(2)));
                let site = b.call(67, &[x, y], if v { 950 } else { 100 }, How::Fmgr(0));
                b.node(ExprNode::Distinct(func::FuncNode { site }))
            }),
            ("nullif.collation", |b, v| {
                let (x, y) = (b.int(Some(1)), b.int(Some(2)));
                let site = b.call(67, &[x, y], if v { 950 } else { 100 }, How::Fmgr(0));
                b.node(ExprNode::NullIf(func::FuncNode { site }))
            }),
            ("imm.value", |b, v| {
                let x = b.int(Some(1));
                let m = b.node(ExprNode::Imm(Imm { value: if v { 2 } else { 3 }, typ: 23 }));
                b.func(1740, &[x, m])
            }),
            ("coerceviaio.site", |b, v| {
                let x = b.int(Some(1));
                let out = b.site(43, &[]);
                let inp = b.site(if v { 1044 } else { 2 }, &[]);
                b.node(ExprNode::CoerceViaIO(CoerceViaIO { arg: x, out, inp, ioparam: 23, direct: false }))
            }),
            ("const.null", |b, v| b.int(if v { None } else { Some(0) })),
        ];
        for (name, mk) in shapes {
            let mut b = B::new();
            let ia = mk(&mut b, false);
            let ra = b.root(ia);
            let ib = mk(&mut b, true);
            let rb = b.root(ib);
            assert_ne!(fingerprint_t(&b.0, ra), fingerprint_t(&b.0, rb), "{name}: one fingerprint");
            let ic = mk(&mut b, false);
            let rc = b.root(ic);
            assert_eq!(fingerprint_t(&b.0, ra), fingerprint_t(&b.0, rc), "{name}: equal roots at two places");
        }
    }
}

/// D1100 (D1099's audit, main 10:17Z): a root holding a by-value word is marked `words` and prints the word's type, never
/// its value: the same root with another literal prints the same fingerprint (the program's tables do not follow the
/// literal the content key does not hold). A root without one is not marked.
#[cfg(test)]
mod d1100 {
    use super::d341::B;
    use super::*;

    /// `length(t) > k` over code column 1.
    fn gt(b: &mut B, k: i32) -> NodeId {
        let t = b.var(1);
        let l = b.func(F_LENGTH[0], &[t]);
        let c = b.int(Some(k));
        let site = b.site(147, &[l, c]);
        b.node(ExprNode::Op(func::FuncNode { site }))
    }

    #[test]
    fn a_by_value_literal_is_a_word_never_its_value() {
        let (mut a, mut c) = (B::new(), B::new());
        let (ra, rc) = (gt(&mut a, 5), gt(&mut c, 10));
        assert_eq!(fingerprint_t(&a.0, ra), (fingerprint_t(&c.0, rc).0, true), "a word's value in the fingerprint, or no word");
        let t = a.var(1);
        let l = a.func(F_LENGTH[0], &[t]);
        assert!(!fingerprint_t(&a.0, l).1, "length(t): no word");
        let need = |fingerprint: (u64, bool), root| TableNeed { root, attno: 1, elem: 0, fingerprint: fingerprint.0, words: fingerprint.1, known: Known::Body };
        let (n1, n2) = (need(fingerprint_t(&a.0, ra), ra), need(fingerprint_t(&c.0, rc), ra));
        assert_ne!(n1.table_fingerprint(1), n1.table_fingerprint(2), "two expressions' word roots: one table");
        assert_eq!(n1.table_fingerprint(1), n2.table_fingerprint(1), "one place, another literal: one table");
        let w = need(fingerprint_t(&a.0, l), l);
        assert_eq!((w.table_fingerprint(1), w.table_fingerprint(2)), (w.fingerprint, w.fingerprint), "no word: by content (D319)");
    }
}

/// D348: a constant's bytes are its whole payload in every header form (a custom plan's constant keeps the 1-byte header
/// of the tuple it was read from), and a constant whose image cannot be read is never the empty string.
#[cfg(test)]
mod d348 {
    use super::*;

    /// A varlena image of `n` bytes of `b` at offset `at` of `buf`: a 1-byte header when it fits (`n + 1 <= 127`), else a
    /// plain 4-byte one.
    fn image(buf: &mut [u8], at: usize, n: usize, b: u8) -> ConstNode<'static> {
        let body = if n + 1 <= 127 {
            buf[at] = (((n + 1) << 1) | 1) as u8;
            at + 1
        } else {
            buf[at..at + 4].copy_from_slice(&(((n + 4) as u32) << 2).to_le_bytes());
            at + 4
        };
        buf[body..body + n].fill(b);
        let value = NullableDatum::value(Datum::from_usize(buf[at..].as_ptr() as usize));
        ConstNode::new(value, 25, -1, false)
    }

    #[test]
    fn a_constant_is_its_whole_payload_in_every_header_form() {
        let mut buf = [0u8; 512];
        for n in [0usize, 1, 63, 64, 65, 71, 100, 125, 126, 127, 128, 300] {
            for at in [8usize, 9] {
                let c = image(&mut buf, at, n, b'b');
                assert_eq!(with_const_bytes(&c, &mut |b| Some(b.len())), Some(n), "{n} bytes at {at}");
                assert!(with_const_bytes(&c, &mut |b| Some(b.iter().all(|&x| x == b'b'))).unwrap());
            }
        }
    }

    #[test]
    fn a_short_header_numeric_constant_is_read_aligned() {
        // D4668: a function argument read from a heap tuple keeps the tuple's 1-byte header, so its payload sits at any
        // address; the numeric reader (fuse.rs integral_numeric) takes the digits as i16
        let mut words = [0u64; 8];
        // SAFETY: 64 bytes of a live, 8-aligned array.
        let buf = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), 64) };
        let img = adt_numeric::int64_to_numeric(123_456_789);
        let payload = img.num().as_bytes();
        for at in [8usize, 9] {
            buf[at] = (((payload.len() + 1) << 1) | 1) as u8;
            buf[at + 1..at + 1 + payload.len()].copy_from_slice(payload);
            let c = ConstNode::new(NullableDatum::value(Datum::from_usize(buf[at..].as_ptr() as usize)), 1700, -1, false);
            let read = with_const_bytes(&c, &mut |b| {
                assert_eq!(b.as_ptr() as usize % 2, 0, "the payload at {at} is lent aligned");
                adt_numeric::numeric_int8(adt_numeric::Num::from_payload(b)).ok()
            });
            assert_eq!(read, Some(123_456_789), "at {at}");
        }
    }

    #[test]
    fn a_short_header_constant_is_not_the_empty_test() {
        // a statement runs armed with its database's encoding, UTF8 (D3521): a test thread boots as SQL_ASCII
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        let mut buf = [0u8; 256];
        for (n, empty) in [(71usize, false), (126, false), (0, true)] {
            let mut p: Program<'static> = Program::empty_in(crate::percode::tcx());
            p.nodes.push(ExprNode::Var(crate::nodes::var::Var { src: eval::Source::Scan, attno: 1, typ: 25, desc: 0 }));
            p.nodes.push(ExprNode::Const(image(&mut buf, 9, n, b'b')));
            p.kids.extend_from_slice(&[NodeId(0), NodeId(1)]);
            for (funcid, like) in [(F_TEXTNE, false), (F_TEXTLIKE[0], true)] {
                p.sites.push(Call { funcid, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: BOOLOID, volatile: false });
                p.nodes.push(ExprNode::Op(func::FuncNode { site: SiteId(p.sites.len() as u32 - 1) }));
                let root = NodeId(p.nodes.len() as u32 - 1);
                let form = classify(&p, root).unwrap();
                if like {
                    assert_eq!(known_of(&p, root, tcx()), Known::Like { pattern: bv(&vec![b'b'; n]), ilike: false, negated: false }); // alloc: Program
                } else {
                    assert_eq!(form, Form::Bits, "t <> a {n}-byte constant: a code set as any constant's");
                    assert_eq!(empty_test(&p, root).map(|e| e.1), empty.then_some(true), "t <> a {n}-byte constant: the length test only for ''");
                }
            }
        }
    }

    /// D1938 / D3521: in a UTF-8 database (the only one pgrust serves) a constant LIKE keeps the table whether its
    /// pattern holds `_` or not, and so does an ILIKE: the table reads `_` as one UTF-8 character and folds UTF-8.
    #[test]
    fn a_utf8_database_keeps_underscore_and_ilike_on_the_table() {
        let mut buf = [0u8; 64];
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        for (byte, funcid) in [(b'_', F_TEXTLIKE[0]), (b'b', F_TEXTLIKE[0]), (b'_', F_TEXTICLIKE[0]), (b'b', F_TEXTICLIKE[0])] {
            let mut p: Program<'static> = Program::empty_in(crate::percode::tcx());
            p.nodes.push(ExprNode::Var(crate::nodes::var::Var { src: eval::Source::Scan, attno: 1, typ: 25, desc: 0 }));
            p.nodes.push(ExprNode::Const(image(&mut buf, 9, 2, byte)));
            p.kids.extend_from_slice(&[NodeId(0), NodeId(1)]);
            p.sites.push(Call { funcid, args: Kids { start: 0, len: 2 }, nargs: 2, strict: true, collation: 100, how: How::Fmgr(0), rettype: BOOLOID, volatile: false });
            p.nodes.push(ExprNode::Op(func::FuncNode { site: SiteId(0) }));
            let got = matches!(known_of(&p, NodeId(2), tcx()), Known::Like { .. });
            assert!(got, "pattern {:?}, funcid {funcid}: the table", byte as char);
        }
    }
}

/// D1832: `t = ''` / `t <> ''` is the empty-code compare only under a deterministic collation. Under a nondeterministic
/// one (ICU `und`, which equates U+00AD with `''`) or one the catalog cannot build, the root is a per-code `Bits` table
/// (a body swept under the collation), and a LIKE is the generic body (D318); the database default and C keep both.
#[cfg(test)]
mod d1832 {
    use super::d341::B;
    use super::*;
    use crate::nodes::bool::BoolNode;
    use gravity_fn::typesem::collation::{Facts, COLLPROVIDER_ICU};

    pub(super) const ND: Oid = 16_700;
    pub(super) const GONE: Oid = 16_701;
    static EMPTY: [u8; 4] = [3, 0, 0, 0];
    static PATTERN: [u8; 4] = [5, b'b', 0, 0];

    /// The catalog: `ND` an ICU `und` row with `collisdeterministic = false`; no row for any other oid.
    pub(super) fn catalog() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            syscache_seams::lookup_pg_collation_locale_row::set(|mcx, collid| {
                if collid != ND {
                    return Ok(None);
                }
                let mut collname = types_tuple::NameData::default();
                collname.namestrcpy("nd");
                Ok(Some(syscache_seams::PgCollationLocaleRow {
                    collname,
                    collnamespace: 2200,
                    collprovider: COLLPROVIDER_ICU,
                    collisdeterministic: false,
                    collencoding: -1,
                    collcollate: None,
                    collctype: None,
                    colllocale: Some(::mcx::PgString::from_str_in("und", mcx)?),
                    collicurules: None,
                    collversion: None,
                }))
            });
        });
    }

    /// `funcid(t, image)` over code column 1 under `coll` (under a NOT when `not`).
    fn root(funcid: Oid, image: &'static [u8], coll: Oid, not: bool) -> (B, NodeId) {
        let mut b = B::new();
        let t = b.var(1);
        let k = b.node(ExprNode::Const(ConstNode::new(NullableDatum::value(Datum::from_usize(image.as_ptr() as usize)), 25, -1, false)));
        let site = b.call(funcid, &[t, k], coll, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = BOOLOID;
        let op = b.node(ExprNode::Op(func::FuncNode { site }));
        if !not {
            return (b, op);
        }
        let args = b.kids(&[op]);
        let top = b.node(ExprNode::Bool(BoolNode { op: BoolExprType::NOT_EXPR, args }));
        (b, top)
    }

    /// `t = ''` / `t <> ''` is a code set as any constant's comparison under a deterministic collation (the empty entry's,
    /// no special form), and a body swept under the collation under a nondeterministic or unbuildable one; LIKE likewise.
    #[test]
    fn an_empty_constant_is_a_code_set_under_a_deterministic_collation_only() {
        // a statement runs armed with its database's encoding, UTF8 (D3521): a test thread boots as SQL_ASCII
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        catalog();
        assert_eq!(Facts::of(ND).map(|f| (f.provider, f.deterministic)).ok(), Some((COLLPROVIDER_ICU, false)), "ND builds nondeterministic");
        assert!(Facts::of(GONE).is_err(), "GONE cannot be built");
        let cols = [CodeCol { attno: 1 }];
        for (funcid, not, negated) in [(F_TEXTEQ, false, false), (F_TEXTNE, false, true), (F_TEXTEQ, true, true)] {
            for (coll, det) in [(100, true), (950, true), (ND, false), (GONE, false)] {
                let (b, top) = root(funcid, &EMPTY, coll, not);
                let form = classify(&b.0, top).unwrap();
                let plan = analyze_t(&b.0, top, &cols).unwrap();
                let at = format!("{funcid} not {not} collation {coll}"); // alloc: Cold
                assert_eq!(form, Form::Bits, "{at}");
                assert_eq!(plan.roots.iter().map(|r| (r.node, r.form, r.table)).collect::<Vec<_>>(), vec![(top, Form::Bits, Some(0))], "{at}"); // alloc: Program
                assert_eq!(plan.needs[0].elem, gravity_dict::ELEM_BITS, "{at}");
                if det {
                    let set = matches!(&plan.needs[0].known, Known::Eq { values, negated: n } if values.len() == 1 && values[0].is_empty() && *n == negated);
                    assert!(set, "{at}: the empty entry's code set: {:?}", plan.needs[0].known);
                } else {
                    assert_eq!(&plan.needs[0].known, &Known::Body, "{at}: a body swept under the collation");
                }
            }
        }
        for (coll, det) in [(100, true), (ND, false), (GONE, false)] {
            let (b, top) = root(F_TEXTLIKE[0], &PATTERN, coll, false);
            let want = if det { Known::Like { pattern: bv(b"b"), ilike: false, negated: false } } else { Known::Body };
            assert_eq!(known_of(&b.0, top, tcx()), want, "LIKE under collation {coll}");
        }
    }

    /// P5-06 / SRC-17: a byte-order compare is a code range only where its order is the bytes': text's under C or POSIX,
    /// never under a nondeterministic, unbuildable or absent collation (the body then compares); the pattern family
    /// (`~<~`, text_pattern_lt) compares bytes under any.
    #[test]
    fn a_range_compare_is_a_code_range_only_under_byte_order() {
        catalog();
        // NEW-72 entry C: the constant (`root`'s node 1) bound per execution, its image never read here
        let at = ConstWord { node: NodeId(1), elem: None, list: false };
        let below = Known::RangeBound { at, below: true, nul: false };
        for (funcid, coll, range) in [(740, 950, true), (740, 951, true), (740, ND, false), (740, GONE, false), (740, 0, false), (2160, ND, true), (2160, 950, true)] {
            let (b, top) = root(funcid, &PATTERN, coll, false);
            assert_eq!(known_of(&b.0, top, tcx()), if range { below.clone() } else { Known::Body }, "{funcid} under collation {coll}");
        }
        let (b, top) = root(740, &PATTERN, 950, true);
        assert_eq!(known_of(&b.0, top, tcx()), Known::RangeBound { at, below: false, nul: false }, "NOT (x < 'b') is x >= 'b'");
        let (b, top) = root(741, &PATTERN, 950, true);
        assert_eq!(known_of(&b.0, top, tcx()), Known::RangeBound { at, below: false, nul: true }, "NOT (x <= 'b') is x > 'b'");
        let (b, top) = root(741, &PATTERN, 950, false);
        assert_eq!(known_of(&b.0, top, tcx()), Known::RangeBound { at, below: true, nul: true }, "x <= 'b' is x < 'b' || '\\0'");
        // the program's fingerprint holds the constant's type, never its image: one table key for every literal
        let (b2, top2) = root(741, &EMPTY, 950, false);
        let one = |n: NodeId| n == NodeId(1);
        assert_eq!(fingerprint_bound_t(&b.0, top, &one), fingerprint_bound_t(&b2.0, top2, &one), "x <= 'b' and x <= '' print alike");
        assert!(fingerprint_bound_t(&b.0, top, &one).1, "a bound constant is a word");
    }
}

/// D340: an interpreter island inside a would-be per-code body reads its columns off the row, which the sweep fills with
/// the entry's image only. An island reading another column, a second code column or a volatile call keeps the body off
/// the per-code form (before: `GREATEST(t, 'v' || i::text) = t` was a body over t, and its sweep refused with `Bug`);
/// one reading only the body's code column stays a body (`GREATEST(t, 'v5') = t`, the twin served per code).
#[cfg(test)]
mod d340 {
    use super::d341::B;
    use super::*;
    use crate::nodes::case::MinMax;

    const TEXTEQ: Oid = 67;
    const TEXTCAT: Oid = 1258;
    const TEXT_LARGER: Oid = 458;
    const RANDOM: Oid = 1598;
    const CODES: [CodeCol; 2] = [CodeCol { attno: 1 }, CodeCol { attno: 3 }];

    /// `GREATEST(t, inner) = t` over code column 1, `inner` built by `f` inside the island.
    fn greatest_eq(f: fn(&mut B) -> NodeId) -> (B, NodeId) {
        let mut b = B::new();
        let first = NodeId(b.0.nodes.len() as u32);
        let t = b.var(1);
        let x = f(&mut b);
        let args = b.kids(&[t, x]);
        let cmp = b.site(TEXT_LARGER, &[]);
        let root = b.node(ExprNode::MinMax(MinMax { args, least: false, cmp }));
        let col = b.var(1);
        let site = b.call(0, &[col], 0, How::Island { root });
        b.0.islands.push(Island { root, first, site, src: 0, once: None });
        let t2 = b.var(1);
        let eq = b.site(TEXTEQ, &[root, t2]);
        let top = b.node(ExprNode::Op(func::FuncNode { site: eq }));
        (b, top)
    }

    fn roots(f: fn(&mut B) -> NodeId) -> Vec<NodeId> {
        let (b, top) = greatest_eq(f);
        analyze_t(&b.0, top, &CODES).expect("analyze").roots.iter().map(|r| r.node).collect() // alloc: Program
    }

    #[test]
    fn an_island_reading_another_column_is_no_per_code_body() {
        // 'v' || i::text: column 2 is not a code column
        assert_eq!(roots(|b| { let (v, i) = (b.int(Some(1)), b.var(2)); b.func(TEXTCAT, &[v, i]) }), vec![]); // alloc: Program
        // a second code column (3) beside the body's (1)
        assert_eq!(roots(|b| { let (v, u) = (b.int(Some(1)), b.var(3)); b.func(TEXTCAT, &[v, u]) }), vec![]); // alloc: Program
        // a volatile call inside the island
        assert_eq!(
            roots(|b| {
                let r = b.func(RANDOM, &[]);
                let n = b.0.sites.len() - 1;
                b.0.sites[n].volatile = true;
                r
            }),
            vec![] // alloc: Program
        );
    }

    #[test]
    fn an_island_reading_only_the_code_column_stays_a_body() {
        let (b, top) = greatest_eq(|b| b.int(Some(5)));
        let plan = analyze_t(&b.0, top, &CODES).expect("analyze");
        assert_eq!(plan.roots.iter().map(|r| (r.node, r.attno)).collect::<Vec<_>>(), vec![(top, 1)]); // alloc: Program
    }
}

/// E2 (NORM-TEXT-SEARCH, HITS9-E2): the recognizer both paths share (`known_of`'s LIKE table, lowerv4 step/text_like.rs's
/// claim; lower key.rs keys `textpos_test`'s class, which gravity_program::twin tests against the brute force); each
/// spelling is its LIKE twin; every near form declines (G4): a position test, a NULL / non-constant needle or k, the
/// search's collation unset or nondeterministic (the compare's is 0 and never read), a database that is not UTF-8.
#[cfg(test)]
mod e2 {
    /// starts_with(text, text) (pg_proc.dat; the `^@` operator's procedure).
    const F_STARTS_WITH: Oid = 3696;

    use super::d1832::{catalog, GONE, ND};
    use super::d341::B;
    use super::*;
    use crate::nodes::bool::BoolNode;

    static NEEDLE: [u8; 5] = [11, b'g', b'%', b'_', b'\\'];
    static EMPTY: [u8; 1] = [3];
    static WIDE: [u8; 4] = [9, 0xE6, 0x97, 0xA5];
    const ESC: &[u8] = b"g\\%\\_\\\\";

    /// `op(f(t, c), k)` over code column 1 (`k op f(..)` when `!left`; `f(t, c)` alone for `op` 0), `f` under `coll`, `c`
    /// the image `needle` (`None`: column 2), `f(c, t)` when `swap`; a `k` of `None` is a NULL.
    fn search(f: Oid, needle: Option<&'static [u8]>, swap: bool, coll: Oid, op: Oid, k: Option<i32>, left: bool) -> (B, NodeId) {
        let mut b = B::new();
        let t = b.var(1);
        let c = match needle {
            Some(img) => b.node(ExprNode::Const(ConstNode::new(NullableDatum::value(Datum::from_usize(img.as_ptr() as usize)), 25, -1, false))),
            None => b.var(2),
        };
        let site = b.call(f, &if swap { [c, t] } else { [t, c] }, coll, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = if f == F_STARTS_WITH { BOOLOID } else { 23 };
        let call = b.node(ExprNode::Func(func::FuncNode { site }));
        if op == 0 {
            return (b, call);
        }
        let k = b.int(k);
        let site = b.call(op, &if left { [call, k] } else { [k, call] }, 0, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = BOOLOID;
        let top = b.node(ExprNode::Op(func::FuncNode { site }));
        (b, top)
    }

    fn twin(pattern: &[u8], negated: bool) -> Option<TextSearch> {
        Some(TextSearch { arg: NodeId(0), pattern: bv(pattern), negated })
    }

    /// STEP-07: every catalog spelling of a text length is one semantic by its body, whatever its oid: the four
    /// `textlen` entries (length, char_length, character_length, textlen) count characters, octet_length bytes; a
    /// bpchar or bytea length (another body) is not a text length.
    #[test]
    fn a_text_length_is_its_catalog_body_not_an_oid_list() {
        for f in [1257, 1317, 1369, 1381] {
            assert_eq!(text_length(f), Some(TextLength::Chars), "{f}");
        }
        assert_eq!(text_length(1374), Some(TextLength::Octets));
        for f in [1318, 1372, 1375, 2010, 0] {
            assert_eq!(text_length(f), None, "{f}");
        }
    }

    /// B2-17: every catalog spelling of LIKE / NOT LIKE / ILIKE / NOT ILIKE and of text `=` / `<>` is one semantic by its
    /// body, whatever its oid (`~~`, `like()`, bpchar's `~~`); a regex match, `namelike` and bpchar's `=` (another body)
    /// are not, so they take the per-entry body.
    #[test]
    fn a_like_or_text_equality_is_its_catalog_body_not_an_oid_list() {
        for (fs, want) in [(&F_TEXTLIKE[..], (false, false)), (&F_TEXTNLIKE[..], (false, true)), (&F_TEXTICLIKE[..], (true, false)), (&F_TEXTICNLIKE[..], (true, true))] {
            for &f in fs {
                assert_eq!(like_body(f), Some(want), "{f}");
            }
        }
        for f in [1254, 858, 67, 0] {
            assert_eq!(like_body(f), None, "{f}");
        }
        assert_eq!((text_eq_body(F_TEXTEQ), text_eq_body(F_TEXTNE)), (Some(false), Some(true)));
        for f in [1048, 1053, 850, 0] {
            assert_eq!(text_eq_body(f), None, "{f}");
        }
    }

    #[test]
    fn each_spelling_is_its_like_twin_and_every_near_form_declines() {
        catalog();
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        let ts = |(b, top): (B, NodeId)| text_search(&b.0, top, tcx());
        let contains = [b"%".as_slice(), ESC, b"%"].concat();
        for (f, op, k, left) in [(868, 147, 0, true), (868, 144, 0, true), (868, 150, 1, true), (868, 66, 0, false), (849, 147, 0, true), (849, 144, 0, false)] {
            assert_eq!(ts(search(f, Some(&NEEDLE), false, 100, op, Some(k), left)), twin(&contains, false), "{f} {op} {k} {left}");
            let (b, top) = search(f, Some(&NEEDLE), false, 950, op, Some(k), left);
            assert_eq!(known_of(&b.0, top, tcx()), Known::Like { pattern: bv(&contains), ilike: false, negated: false });
        }
        for (op, k, left) in [(65, 0, true), (66, 1, true), (149, 0, true), (147, 1, false), (65, 0, false)] {
            assert_eq!(ts(search(868, Some(&NEEDLE), false, 100, op, Some(k), left)), twin(&contains, true), "{op} {k} {left}");
        }
        let (mut b, top) = search(868, Some(&NEEDLE), false, 100, 147, Some(0), true);
        let args = b.kids(&[top]);
        let not = b.node(ExprNode::Bool(BoolNode { op: BoolExprType::NOT_EXPR, args }));
        assert_eq!(known_of(&b.0, not, tcx()), Known::Like { pattern: bv(&contains), ilike: false, negated: true }, "NOT (strpos > 0)");
        let (mut b, top) = search(868, Some(&NEEDLE), false, 100, 147, Some(0), true);
        let call = b.0.kids[b.0.sites[1].args.start as usize].0 as usize;
        let ExprNode::Func(n) = &b.0.nodes[call] else { panic!("the strpos call") };
        b.0.nodes[call] = ExprNode::Op(func::FuncNode { site: n.site });
        assert_eq!(ts((b, top)), twin(&contains, false), "(t ### c) > 0, an operator over textpos: lower key.rs keys it too");
        let prefix = [ESC, b"%"].concat();
        assert_eq!(ts(search(F_STARTS_WITH, Some(&NEEDLE), false, 100, 0, None, true)), twin(&prefix, false), "starts_with");
        assert_eq!(ts(search(868, Some(&EMPTY), false, 100, 147, Some(0), true)), twin(b"%%", false), "strpos(t, '') > 0");
        assert_eq!(ts(search(F_STARTS_WITH, Some(&EMPTY), false, 100, 0, None, true)), twin(b"%", false), "starts_with(t, '')");
        assert_eq!(ts(search(868, Some(&WIDE), false, 100, 147, Some(0), true)), twin(&[b"%".as_slice(), &WIDE[1..], b"%"].concat(), false));
        let cols = [CodeCol { attno: 1 }];
        for (coll, like) in [(100, true), (950, true), (ND, false), (GONE, false), (0, false)] {
            for (f, op) in [(868, 147), (849, 144), (F_STARTS_WITH, 0)] {
                let (b, top) = search(f, Some(&NEEDLE), false, coll, op, Some(0), true);
                assert_eq!(text_search(&b.0, top, tcx()).is_some(), like, "{f} under the search's collation {coll} (the compare's is 0)");
                let plan = analyze_t(&b.0, top, &cols).expect("analyze");
                assert_eq!(plan.roots.iter().map(|r| (r.node, r.form)).collect::<Vec<_>>(), vec![(top, Form::Bits)]); // alloc: Program
                assert_eq!(matches!(plan.needs[0].known, Known::Like { .. }), like, "{f} {coll}: the LIKE table or the body");
            }
        }
        let near = [
            (search(868, Some(&NEEDLE), false, 100, 147, Some(1), true), "strpos > 1"),
            (search(868, Some(&NEEDLE), false, 100, 65, Some(3), true), "strpos = 3"),
            (search(868, Some(&NEEDLE), false, 100, 147, Some(-1), true), "strpos > -1: always"),
            (search(868, Some(&NEEDLE), false, 100, 147, None, true), "k NULL"),
            (search(868, None, false, 100, 147, Some(0), true), "a per-row needle"),
            (search(868, None, true, 100, 147, Some(0), true), "strpos(column 2, t): the column as the needle"),
            (search(849, None, true, 100, 144, Some(0), false), "position(t IN column 2)"),
            (search(868, Some(&NEEDLE), false, 100, 0, None, true), "strpos as a value"),
            (search(F_TEXTLIKE[0], Some(&NEEDLE), false, 100, 147, Some(0), true), "not a textpos call"),
        ];
        for ((b, top), what) in near {
            assert_eq!(text_search(&b.0, top, tcx()), None, "{what}");
        }
        let (mut b, top) = search(868, Some(&NEEDLE), false, 100, 147, Some(0), true);
        let needle = b.node(ExprNode::Const(ConstNode::new(NullableDatum { value: Datum::from_usize(0), isnull: true }, 25, -1, false)));
        b.0.kids[b.0.sites[0].args.start as usize + 1] = needle; // site 0 is strpos, site 1 the compare
        assert_eq!(text_search(&b.0, top, tcx()), None, "a NULL needle");
        let (mut b, top) = search(868, Some(&NEEDLE), false, 100, 147, Some(0), true);
        let param = b.node(ExprNode::Param(crate::nodes::param::Param { kind: ParamKind::PARAM_EXTERN, id: 1, typ: 23 }));
        b.0.kids[b.0.sites[1].args.start as usize + 1] = param;
        assert_eq!(text_search(&b.0, top, tcx()), None, "k a $1");
    }
}

/// PS1-13 / PS1-14: a byte test that answers a NULL row is a root with that answer (`nullable`): `t IS [NOT] DISTINCT FROM
/// c`, and `coalesce(t, c0)` under an equality or a byte-order compare; never under a nondeterministic collation, a NULL
/// default, another column, or a strict compare over the bare column (`scan`'s root, no NULL answer).
#[cfg(test)]
mod ps113 {
    use super::d1832::{catalog, ND};
    use super::d341::B;
    use super::*;
    use crate::nodes::bool::BoolNode;
    use crate::nodes::case::Coalesce;

    static A: [u8; 4] = [5, b'a', 0, 0];
    static BB: [u8; 4] = [5, b'b', 0, 0];
    static C: [u8; 4] = [5, b'c', 0, 0];
    static EMPTY: [u8; 4] = [3, 0, 0, 0];
    const CODES: [CodeCol; 1] = [CodeCol { attno: 1 }];

    fn text(b: &mut B, image: Option<&'static [u8]>) -> NodeId {
        let d = match image {
            Some(i) => NullableDatum::value(Datum::from_usize(i.as_ptr() as usize)),
            None => NullableDatum { value: Datum::from_usize(0), isnull: true },
        };
        b.node(ExprNode::Const(ConstNode::new(d, 25, -1, false)))
    }

    /// `[NOT] f(x, 'b')` and its plan, `x` column `attno` or `coalesce(column, c0)`; a `Distinct` node when `distinct`.
    fn build(funcid: Oid, coll: Oid, attno: i16, c0: Option<Option<&'static [u8]>>, distinct: bool, not: bool) -> (Plan, NodeId) {
        let mut b = B::new();
        let mut x = b.var(attno);
        if let Some(c0) = c0 {
            let d = text(&mut b, c0);
            let args = b.kids(&[x, d]);
            x = b.node(ExprNode::Coalesce(Coalesce { args }));
        }
        let k = text(&mut b, Some(&BB));
        let site = b.call(funcid, &[x, k], coll, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = BOOLOID;
        let mut top = b.node(if distinct { ExprNode::Distinct(func::FuncNode { site }) } else { ExprNode::Op(func::FuncNode { site }) });
        if not {
            let args = b.kids(&[top]);
            top = b.node(ExprNode::Bool(BoolNode { op: BoolExprType::NOT_EXPR, args }));
        }
        (analyze_t(&b.0, top, &CODES).expect("analyze"), top)
    }

    fn test(funcid: Oid, coll: Oid, attno: i16, c0: Option<Option<&'static [u8]>>, distinct: bool, not: bool) -> Option<Root> {
        let (plan, top) = build(funcid, coll, attno, c0, distinct, not);
        plan.root_at(top).copied()
    }

    /// The root's table and NULL answer over column 1; a NULL-answering root is one `Bits` table.
    fn known(funcid: Oid, coll: Oid, c0: Option<Option<&'static [u8]>>, distinct: bool, not: bool) -> Option<(Known, Option<bool>)> {
        let (plan, top) = build(funcid, coll, 1, c0, distinct, not);
        let r = plan.root_at(top)?;
        if r.null.is_some() {
            assert_eq!((r.form, r.table, plan.needs.len()), (Form::Bits, Some(0), 1));
        }
        Some((plan.needs.get(r.table? as usize)?.known.clone(), r.null))
    }

    fn eq(v: &[u8], negated: bool) -> Known<'static> {
        Known::Eq { values: one_in(bv(v), tcx()), negated }
    }

    #[test]
    fn is_distinct_from_answers_a_null_row() {
        catalog();
        assert_eq!(known(F_TEXTEQ, 100, None, true, false), Some((eq(b"b", true), Some(true))), "t IS DISTINCT FROM 'b'");
        assert_eq!(known(F_TEXTEQ, 100, None, true, true), Some((eq(b"b", false), Some(false))), "t IS NOT DISTINCT FROM 'b'");
        assert_eq!(known(F_TEXTEQ, ND, None, true, true), None, "under a nondeterministic collation");
        assert_eq!(known(F_TEXTNE, 100, None, true, false), None, "a Distinct over no `=`");
        assert!(test(F_TEXTEQ, 100, 2, None, true, false).is_none(), "another column");
        // NEW-72 entry C: the strict compare is `code_set`'s, its constant (node 1) bound per execution
        let bound = Known::EqBound { values: PgVec::new_in(tcx()), params: PgVec::new_in(tcx()), consts: one_in(ConstWord { node: NodeId(1), elem: None, list: false }, tcx()), negated: false };
        assert_eq!(known(F_TEXTEQ, 100, None, false, false), Some((bound, None)), "t = 'b' stays strict");
    }

    #[test]
    fn a_coalesce_default_is_the_null_rows_answer() {
        catalog();
        let e = Some(Some(&EMPTY[..]));
        assert_eq!(known(F_TEXTEQ, 100, e, false, false), Some((eq(b"b", false), Some(false))), "coalesce(t, '') = 'b'");
        assert_eq!(known(F_TEXTEQ, 100, Some(Some(&BB)), false, false), Some((eq(b"b", false), Some(true))), "coalesce(t, 'b') = 'b'");
        assert_eq!(known(F_TEXTNE, 100, Some(Some(&BB)), false, false), Some((eq(b"b", true), Some(false))), "coalesce(t, 'b') <> 'b'");
        assert_eq!(known(F_TEXTEQ, 100, Some(Some(&BB)), false, true), Some((eq(b"b", true), Some(false))), "NOT coalesce(t, 'b') = 'b'");
        assert_eq!(known(F_TEXTEQ, 100, e, true, false), Some((eq(b"b", true), Some(true))), "coalesce(t, '') IS DISTINCT FROM 'b'");
        assert_eq!(known(F_TEXTEQ, 100, Some(None), false, false), None, "a NULL default");
        assert_eq!(known(F_TEXTEQ, ND, e, false, false), None, "under a nondeterministic collation");
        assert!(test(F_TEXTEQ, 100, 2, e, false, false).is_none(), "over another column");
        let below = Known::Range { from: PgVec::new_in(tcx()), to: Some(bv(b"b")) };
        assert_eq!(known(740, 950, Some(Some(&A)), false, false), Some((below.clone(), Some(true))), "coalesce(t, 'a') < 'b'");
        assert_eq!(known(740, 950, Some(Some(&C)), false, false), Some((below, Some(false))), "coalesce(t, 'c') < 'b'");
        let at = Known::Range { from: bv(b"b"), to: None };
        assert_eq!(known(740, 950, Some(Some(&BB)), false, true), Some((at, Some(true))), "NOT coalesce(t, 'b') < 'b'");
        assert_eq!(known(740, ND, Some(Some(&A)), false, false), None, "a range under no byte order");
    }
}

/// D3077: `lower(x) LIKE p` is `x ILIKE p` where `lower` folds ASCII only (a deterministic collation whose ctype is C)
/// and `p` holds no ASCII capital; `octet_length` / `char_length` of that `lower(x)` are `x`'s. Any other collation,
/// a capital in the pattern, or a NOT stays the body or keeps its own polarity.
#[cfg(test)]
mod d3077 {
    use super::d1832::{catalog, ND};
    use super::d341::B;
    use super::*;

    const LOWER: Oid = 870;
    static GARDEN: [u8; 12] = [19, b'%', b'g', b'a', b'r', b'd', b'e', b'n', b'%', 0, 0, 0];
    static CAPITAL: [u8; 12] = [19, b'%', b'G', b'o', b'o', b'g', b'l', b'e', b'%', 0, 0, 0];
    static ACCENT: [u8; 8] = [11, b'%', 0xC3, 0x89, b'%', 0, 0, 0];

    fn lower_like(funcid: Oid, lower_coll: Oid, like_coll: Oid, pattern: &'static [u8]) -> (B, NodeId) {
        let mut b = B::new();
        let x = b.var(1);
        let site = b.call(LOWER, &[x], lower_coll, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = 25;
        let low = b.node(ExprNode::Func(func::FuncNode { site }));
        let p = b.node(ExprNode::Const(ConstNode::new(NullableDatum::value(Datum::from_usize(pattern.as_ptr() as usize)), 25, -1, false)));
        let site = b.call(funcid, &[low, p], like_coll, How::Fmgr(0));
        b.0.sites[site.0 as usize].rettype = BOOLOID;
        let top = b.node(ExprNode::Op(func::FuncNode { site }));
        (b, top)
    }

    #[test]
    fn lower_like_is_an_ascii_ilike_under_c_ctype() {
        catalog();
        gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
        let like = |pattern: &[u8], negated| Known::Like { pattern: bv(pattern), ilike: true, negated };
        let (b, top) = lower_like(F_TEXTLIKE[0], 950, 950, &GARDEN);
        assert_eq!(known_of(&b.0, top, tcx()), like(b"%garden%", false));
        let (b, top) = lower_like(F_TEXTNLIKE[0], 950, 950, &GARDEN);
        assert_eq!(known_of(&b.0, top, tcx()), like(b"%garden%", true), "NOT LIKE keeps its polarity");
        let (b, top) = lower_like(F_TEXTLIKE[0], 950, 950, &ACCENT);
        assert_eq!(known_of(&b.0, top, tcx()), like("%\u{c9}%".as_bytes(), false), "a non-ASCII capital is matched as bytes on both sides");
        for (lower_coll, like_coll, pattern, why) in [
            (950, 950, &CAPITAL[..], "an ASCII capital in the pattern never matches lower(x)"),
            (ND, ND, &GARDEN[..], "a nondeterministic collation"),
            (950, ND, &GARDEN[..], "lower and LIKE under different collations"),
        ] {
            let (b, top) = lower_like(F_TEXTLIKE[0], lower_coll, like_coll, pattern);
            assert_eq!(known_of(&b.0, top, tcx()), Known::Body, "{why}");
        }
        let (b, top) = lower_like(F_TEXTICLIKE[0], 950, 950, &GARDEN);
        assert!(!matches!(known_of(&b.0, top, tcx()), Known::Like { .. }), "an ILIKE over lower(x) is not rewritten");
    }

    #[test]
    fn a_length_of_ascii_lower_is_the_columns() {
        catalog();
        for (f, coll, want) in [(1374, 950, Known::OctetLength), (1381, 950, Known::CharLength), (1374, ND, Known::Body)] {
            let mut b = B::new();
            let x = b.var(1);
            let site = b.call(LOWER, &[x], coll, How::Fmgr(0));
            b.0.sites[site.0 as usize].rettype = 25;
            let low = b.node(ExprNode::Func(func::FuncNode { site }));
            let top = b.func(f, &[low]);
            assert_eq!(known_of(&b.0, top, tcx()), want, "{f} under {coll}");
        }
    }
}
