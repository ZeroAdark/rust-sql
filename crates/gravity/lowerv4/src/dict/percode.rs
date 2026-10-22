//! Road `percode.table` (H09, R2-L04 R3-L11; lowerv2 text/codes.rs): a pure function of one dictionary-coded lane is
//! evaluated once per dictionary entry and read per row by the row's code. `attach` finds an expression's per-code roots
//! (`gravity_expr::percode`) over the code columns the step's row carries and declares each root's table on the site:
//! LIKE verdicts, code sets (`= c`, `IN (...)`: looked up, B2-20) and byte intervals (`< c`, `>= c`: a code run, P5-06)
//! come resident from the hosts (`cond_table`, never built per statement; an ILIKE only under a C ctype), lengths from
//! the dictionary index (percode_len.rs), any other body is a `PROGRAM` table the program fills per entry. The source binds the
//! site's tables once per window (percode_bind.rs `bind`, into `Env::tables`). A faulting or NULL entry is never read
//! from the table: the row re-evaluates the whole expression over the rendered text (step/expr.rs), so errors and NULLs
//! are PG's, raised only where PG evaluates. Budget 160 lines. Owner M1-STEP.
use std::{cell::RefCell, rc::Rc};

use gravity_abi::ctx::lane::KIND_CODES;
use gravity_dict::lane::{ELEM_BITS, ELEM_I64};
use gravity_expr::emit::percode::{CodeRef, TableRef};
use gravity_expr::emit::{ColsRef, EmitError, LaneRef, RowRefs};
use gravity_expr::percode::{CodeCol, Form, Known, Plan as PcPlan};
use gravity_expr::{Compiled, ExprNode, NodeId, Source as VarSource};
use gravity_ir::{builder::ProgramBuilder, Type, Value};
use gravity_program::{table_fn, LaneId, TableDecl};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::dict::face::{PerValue, PerValueImpl};
use crate::frame::px::{ExId, Px};
use crate::admit::refusal::Reason;

crate::strategies! {
    /// Pure function of one Code lane once per entry, read as a word mask; LIKE verdict tables come from hosts
    /// text_hosts::cond_table, resident across statements (R4), never built per statement.
    PERCODE_TABLE = 21, "percode.table", CodeExpr;
}

pub(crate) const ROWS: crate::ops::Table =
    crate::ops::Table { strategies: &[(PERCODE_TABLE, &PerValueImpl(&crate::dict::percode_len::ROAD))], ..crate::ops::NONE };

/// How the row reads a bound table (`agg_bind`'s class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Bits,
    Len,
    Fp,
    Words,
    /// C06: a code set (`table_fn::EQ`, `Known::Eq`) bound as `Bits`: the hosts look every code of the part up, so no entry
    /// faults and the row reads no fault bit (no `TABLE` raise, so no cold re-evaluation of the whole expression).
    Set,
}

/// A `PROGRAM` table the bind may find unfilled: site table `t` over code lane `over`, filled by subtree `root` of
/// expression `ex` (code column `attno`), a bit, a word or an Fp128 per entry.
pub(crate) struct Sweep {
    pub t: usize,
    pub over: u8,
    pub ex: ExId,
    pub root: NodeId,
    pub attno: i16,
    pub elem: u8,
    pub node: i32,
    /// countwalk reads the table over every code (HITS9-E1): swept, never filled by rows
    pub walked: bool,
    /// STEP-04: every row of the part reads it (percode_fill.rs `every_row`): swept at the bind
    pub every: bool,
}

/// One site's per-code tables in site table order (`t`: the bind's slot, `Env::tables` entry `t`); `rows`: percode_key's; `lens`: the length tables' row reads `(out, t, read)` the bind built.
#[derive(Default)]
pub(crate) struct Tables {
    pub lanes: Vec<(u8, Class)>,
    pub sweeps: Vec<Sweep>,
    pub rows: u8,
    pub lens: Vec<(Value, u16, TableRef)>,
    /// D2074: the site the bind bound them on (a row's `fill` renders through it); `None` before the bind.
    pub site: Option<usize>,
    /// X5: the parameter-valued tables' key cells, written by the bind before its lane fills (`percode_bind::bound`).
    pub bound: Vec<Bound>,
}

/// X5: one `FLAG_BOUND` table's values: statement parameter `params[i]` (PARAM_EXTERN `(id, type)`) of expression `ex`
/// goes to the site's key cell `cells[i]`. NEW-72 entry C: then the word of `consts[j]` (the index of a bound text
/// constant's need in the expression's word plan, gravity_expr `Compiled::const_need`) to cell `cells[params.len() + j]`.
pub(crate) struct Bound {
    pub cells: Vec<u16>,
    pub params: Vec<(i32, u32)>,
    pub consts: Vec<u32>,
    pub ex: ExId,
}

/// An expression's per-code reads (`Ex::ext`): its code columns `(attno, site lane, width)`, per table need in
/// `gravity_expr::percode` order the site table and its class, and the site's tables (`tabs`, the bind's length reads).
pub(crate) struct Plan {
    pub cols: Vec<(i16, u8, u8)>,
    pub tables: Vec<(u16, Class)>,
    pub tabs: Rc<RefCell<Tables>>,
    /// D2997: the claims an expression extension made over what the per-code roots left (step/expr_ext.rs `residual`),
    /// read back through `ExtPlan::of`.
    pub ext: std::cell::OnceCell<crate::face::ext::ExtPlan>,
}

/// Pipeline `k`'s key among the step face's tables. D330: a remade pass's own, as its site is (Px::remake). D2278 / D2232:
/// an embedded inner (a pipeline past the graph's: a nest inner, a SubPlan's plan) its own under its owner's identity, so
/// each source binds only its own site's lanes.
fn tables_key(px: &Px<'_, '_>, k: usize) -> u64 {
    let inner = u64::from(k >= px.gx.pipelines().len()) * ((k as u64 + 1) << 32);
    px.ident(k) as u64 | inner
}

/// Pipeline `k`'s tables; the source takes the `Rc` at its make and binds what the steps declared.
pub(crate) fn tables(px: &mut Px<'_, '_>, k: usize) -> Rc<RefCell<Tables>> {
    let (k, ext) = (tables_key(px, k), crate::step::expr::ext(px));
    if let Some((_, t)) = ext.tables.iter().find(|(at, _)| *at == k) {
        return Rc::clone(t);
    }
    let t = Rc::new(RefCell::new(Tables::default())); // alloc: Program
    ext.tables.push((k, Rc::clone(&t))); // alloc: Program
    t
}

/// D3938 (the D3913 class): expression `q`'s per-code reads are pipeline `k`'s tables in the making being made, the ones
/// its source binds (true without per-code reads). A qual compiled for another making of the pipeline (a remade pass, a
/// swapped side) reads another site's slots: its rows, and any verdict a cache keeps from them, would be another
/// expression's. Asked where a making takes compiled quals (step/filter.rs `make`, pgrc `declare_verdicts`).
pub(crate) fn owns(px: &mut Px<'_, '_>, k: usize, q: ExId) -> bool {
    let Some(tabs) = px.placed(q).ext.as_deref().and_then(|x| x.downcast_ref::<Plan>()).map(|p| Rc::clone(&p.tabs)) else { return true };
    let key = tables_key(px, k);
    crate::step::expr::ext(px).tables.iter().any(|(at, t)| *at == key && Rc::ptr_eq(t, &tabs))
}

/// `decl` declared on pipeline `k`'s site and bound per window as `class`: the site table. Every table of a site goes
/// through here (a fingerprint table too), so the bind covers the site's tables in order.
pub(crate) fn declare_table(px: &mut Px<'_, '_>, k: usize, decl: TableDecl, class: Class) -> Option<u16> {
    let (fits, s) = (crate::dict::strategies::percode_guard::fits(px, k, 1), px.site_mut(k)?);
    // D319: an equal table is one lane (two lanes of one key spin in the bind), its args compared by bytes (table_bytes_of appends); D1747: a new one only while its site lane (a u8 here) and the row slots (u8 fp_table) fit, a type bound, not a cap: past it the column is read as its datum
    let t = match s.tables.iter().position(|x| TableDecl { args: decl.args, flags: unswept(x), ..*x } == decl && (0..2).all(|a| s.table_arg(x, a) == s.table_arg(&decl, a))) { Some(i) => i as u16, None if fits => s.declare_table(decl)?, None => return None };
    let (lane, tabs) = (s.lane_index(LaneId::Table(t))? as u8, tables(px, k));
    let mut tb = tabs.borrow_mut();
    if t as usize == tb.lanes.len() {
        tb.lanes.push((lane, class)); // alloc: Program
    } else if t as usize > tb.lanes.len() {
        crate::refuse!(px.gx.root_node(), Reason::Bug { payload: "a site table declared outside step_percode::declare_table" });
    }
    Some(t)
}

/// A table's flags without D3115's `FLAG_SWEPT` (set on a `PROGRAM` table after its declaration, `mark_swept`).
fn unswept(d: &TableDecl) -> u8 {
    if d.func == table_fn::PROGRAM { d.flags & !table_fn::FLAG_SWEPT } else { d.flags }
}

/// D3115: site table `t`'s `FLAG_SWEPT` from its sweep: a `PROGRAM` table the program sweeps (`percode_fill::lazy` false)
/// is the statement's, its chunks swept by every source that reaches the part; a lazy one stays each source's own (its
/// rows fill it). Called wherever a sweep's `every` / `walked` moves.
pub(crate) fn mark_swept(px: &mut Px<'_, '_>, k: usize, tabs: &Rc<RefCell<Tables>>, t: usize) {
    let swept = tabs.borrow().sweeps.iter().find(|s| s.t == t).is_some_and(|s| !crate::dict::percode_fill::lazy(s));
    if let Some(d) = px.site_mut(k).and_then(|s| s.tables.get_mut(t)).filter(|d| d.func == table_fn::PROGRAM) {
        d.flags = if swept { d.flags | table_fn::FLAG_SWEPT } else { d.flags & !table_fn::FLAG_SWEPT };
    }
}

/// C06: whether `pre_claim` leaves `c` (compiled from `node` for step `j` of pipeline `k`, with no outer list) to `attach`.
/// It does when every ScalarArrayOp it would hand `expr.saop` (step/expr_ext.rs `pre_kind`: a constant PG's plan hashed,
/// 9 or more elements) is a code set (`gravity_expr::percode::code_set_var`: `IN` / `NOT IN` by the byte equality under a
/// deterministic collation, no NULL element) over a code column the step reads by code: the
/// part's EQ table answers it (`Class::Set`), where `expr.saop` renders every row and probes its hashed set. X5: so is a
/// generic plan's `IN` list over statement parameters (an `ARRAY[$1, 'c', ..]` PG never hashes, which `pre_kind` hands to
/// `expr.saop` as no constant): its EQ table is built over the values the source reads per execution (`Known::EqBound`).
/// P5-02: any length: a constant list past `MAX_LIST` is bound as one word, its whole array (`ConstWord::list`). Any other
/// array that is no constant keeps `pre_claim`. When `attach` declines it after all, step/expr.rs `compile_at` makes
/// `pre_claim`'s claim then.
pub(crate) fn sets_first(px: &mut Px<'_, '_>, k: usize, j: usize, c: &Compiled<'_>, node: types_nodes::Node<'_>) -> bool {
    if !PerValue::Table.on() {
        return false;
    }
    let p = &c.prog;
    let mcx = px.mcx();
    let saops: PgVec<'_, usize> = (0..p.nodes.len()).filter(|&i| matches!(p.nodes[i], ExprNode::ScalarArrayOp(_))).collect_in(mcx);
    let mut hashed: PgVec<'_, bool> = PgVec::new_in(mcx);
    gravity_decompose::expr::walk(node.as_target_entry().map_or(node, |te| te.expr), &mut |x| {
        if let Some(s) = x.as_scalar_array_op_expr() {
            hashed.push_in(s.hashfuncid != 0);
        }
        true
    });
    let array = |i: usize| match &p.nodes[i] {
        ExprNode::ScalarArrayOp(s) => Some(p.kids[p.sites[s.site.0 as usize].args.start as usize + 1]),
        _ => None,
    };
    let bound = |i: usize| array(i).and_then(|a| gravity_expr::percode::bound_list_len(p, a));
    if saops.is_empty() || saops.len() != hashed.len() || !saops.iter().zip(&hashed).any(|(&i, &h)| h || bound(i).is_some()) {
        return false;
    }
    let codes = crate::step::project::code_cols(px, k, j);
    saops.iter().zip(hashed).all(|(&i, h)| {
        let ExprNode::ScalarArrayOp(s) = &p.nodes[i] else { return false };
        let n = match array(i).map(|a| &p.nodes[a.0 as usize]) {
            Some(ExprNode::Const(a)) => a.list_count(s.elem),
            _ => match bound(i) {
                Some(n) => Some(n),
                None => return false,
            },
        };
        if !h && bound(i).is_none() {
            return n.is_some_and(|n| n <= gravity_program::cost::expr::MAX_LIST); // pre_kind's `long`: not handed over
        }
        let Some(v) = gravity_expr::percode::code_set_var(p, NodeId(i as u32)) else { return false };
        // the column as `attach` reads it: a scan column, or an `OUTER` one that is one of the row's code columns (compile_at)
        // P5-02: any length: a constant list past MAX_LIST is one bound word (gravity_expr percode `ConstWord::list`), a
        // parameter list one cell per element; the per-code road against the datum is the cost call `reads_datum` makes
        n.is_some_and(|n| n > 0) && matches!(v.src, VarSource::Scan | VarSource::Outer) && codes.iter().any(|x| x.0 == v.attno)
    })
}

/// The per-code reads of `c` over the code columns step `j` of pipeline `k` reads, their tables declared: `None` when no
/// root is served (the expression reads every column as its datum). A column some use of which is not per-code (a bare
/// column under another node, a by-reference result, a road that is off) is read as its datum; the other columns'
/// `Var`s are then `OUTER`, so the code columns alone are read by code.
pub(crate) fn attach(px: &mut Px<'_, '_>, k: usize, j: usize, c: &mut Compiled<'_>, ex: ExId, node: i32) -> Option<Rc<Plan>> {
    let mcx = px.mcx();
    let reads = |c: &Compiled<'_>, a: i16| c.prog.nodes.iter().any(|n| matches!(n, ExprNode::Var(v) if v.src == VarSource::Scan && v.attno == a));
    let mut cols: Vec<_> = crate::step::project::code_cols(px, k, j).into_iter().filter(|x| reads(c, x.0)).collect(); // alloc: Program
    // R5-B: each code column's (rows, entries) for the body's datum choice (`reads_datum`), the server's widest width beside
    let counts: PgVec<'_, (i16, Option<(u64, u64)>)> = cols.iter().map(|x| (x.0, col_counts(px, k, x.1))).collect_in(mcx);
    let widest = px.gx.facts.width().unwrap_or(0);
    let plan = loop {
        if cols.is_empty() || (!PerValue::Table.on() && !PerValue::Length.on()) {
            return None;
        }
        let cc: PgVec<'_, CodeCol> = cols.iter().map(|x| CodeCol { attno: x.0 }).collect_in(mcx);
        let plan = c.percode(&cc, mcx).ok()?;
        let kept = |r: &gravity_expr::percode::Root| served(c, &plan, r) && !crate::step::expr_ext_lazy::twice(c, &plan, r);
        // PS1-15: an ILIKE the hosts' table does not take (`hosted_like`) is a body swept per entry, a bound one too
        let swept = |r: &gravity_expr::percode::Root, k: &Known| match k {
            Known::Body => true,
            Known::LikeBound { ilike, .. } => !hosted_like(c, r.node, *ilike, px.gx.catalog),
            _ => false,
        };
        let dense = |r: &gravity_expr::percode::Root| counts.iter().find(|x| x.0 == r.attno).and_then(|x| x.1).is_some_and(|(rows, n)| reads_datum(c, r.node, rows, n, widest));
        let body = |r: &gravity_expr::percode::Root| r.table.is_some_and(|t| swept(r, &plan.needs[t as usize].known)) && dense(r);
        let drop = plan.roots.iter().find(|r| !kept(r) || body(r));
        if drop.is_some_and(kept) {
            fires::fire("percode.datum");
        }
        let drop_at = drop.map(|r| r.attno);
        // PS1-28: a scan column outside `cols` (a riding column: `g` in `t = 'v3' OR g < 2`) is read as `OUTER` once the
        // plan is taken (below), so the probe reads it so too; read as a scan column there it failed the probe, which
        // dropped the code column
        let ride: PgVec<'_, usize> = (0..c.prog.nodes.len()).filter(|&i| matches!(&c.prog.nodes[i], ExprNode::Var(v) if v.src == VarSource::Scan && !cols.iter().any(|x| x.0 == v.attno))).collect_in(mcx);
        set_src(c, &ride, VarSource::Outer);
        let probed = drop_at.map_or_else(|| probe(c, &cols, &plan, mcx), Err);
        set_src(c, &ride, VarSource::Scan);
        match probed {
            Ok(()) => break crate::dict::strategies::percode_guard::fits(px, k, plan.needs.len()).then_some(plan)?, // D1747: every need's table fits the u8 lane / slot types (else read as datum)
            Err(a) => cols.retain(|x| x.0 != a),
        }
    };
    let mut reads = Vec::with_capacity(plan.needs.len()); // alloc: Program
    for n in plan.needs.iter() {
        let over = cols.iter().find(|x| x.0 == n.attno)?.1;
        let mut bound = None;
        let (decl, class) = match &n.known {
            Known::Like { pattern, ilike, negated } if hosted_like(c, n.root, *ilike, px.gx.catalog) => {
                let flags = if *ilike { table_fn::FLAG_ILIKE } else { 0 } | if *negated { table_fn::FLAG_NEGATED } else { 0 };
                let arg = px.site_mut(k)?.table_bytes_of(pattern);
                (TableDecl { over, func: table_fn::LIKE, flags, collation: collation(c, n.root), encoding: 0, args: [arg, (0, 0)] }, Class::Bits)
            }
            Known::Eq { values, negated } => {
                let mut bytes: PgVec<'_, u8> = PgVec::new_in(mcx);
                table_fn::put_values(values, &mut |b| bytes.extend_from_slice_in(b));
                let arg = px.site_mut(k)?.table_bytes_of(&bytes);
                let flags = if *negated { table_fn::FLAG_NEGATED } else { 0 };
                (TableDecl { over, func: table_fn::EQ, flags, collation: collation(c, n.root), encoding: 0, args: [arg, (0, 0)] }, Class::Set)
            }
            Known::EqBound { values, params, consts, negated } => {
                // P5-02: a whole list's word (`ConstWord::list`) is a table of its own kind: every cell an array
                if consts.iter().any(|w| w.list) && (consts.len() != 1 || !values.is_empty() || !params.is_empty()) {
                    return None;
                }
                let list = if consts.iter().any(|w| w.list) { table_fn::FLAG_LIST } else { 0 };
                let flags = table_fn::FLAG_BOUND | list | if *negated { table_fn::FLAG_NEGATED } else { 0 };
                let mut needs: PgVec<'_, u32> = mcx::reserved_in(consts.len(), mcx);
                for &w in consts.iter() {
                    needs.push_in(c.const_need(w, mcx)?);
                }
                let (args, bd) = bound_args(px, k, mcx, values, params, &needs, ex)?;
                bound = Some(bd);
                // read with its fault bit: a NULL parameter or constant faults every entry, and the row evaluates the body
                (TableDecl { over, func: table_fn::EQ, flags, collation: collation(c, n.root), encoding: 0, args }, Class::Bits)
            }
            Known::RangeBound { at, below, nul } => {
                // NEW-72 entry C: the byte interval's end is the constant the source reads per execution
                let flags = table_fn::FLAG_BOUND | if *below { table_fn::FLAG_BELOW } else { 0 } | if *nul { table_fn::FLAG_NUL } else { 0 };
                let (args, bd) = bound_args(px, k, mcx, &[] as &[&[u8]], &[], &[c.const_need(*at, mcx)?], ex)?;
                bound = Some(bd);
                (TableDecl { over, func: table_fn::RANGE, flags, collation: collation(c, n.root), encoding: 0, args }, Class::Bits)
            }
            Known::LikeBound { parts, params, ilike, negated } if hosted_like(c, n.root, *ilike, px.gx.catalog) => {
                // PS1-15: the pattern's constant runs and its parameters' cells, assembled by the source per execution
                let flags = table_fn::FLAG_BOUND | if *ilike { table_fn::FLAG_ILIKE } else { 0 } | if *negated { table_fn::FLAG_NEGATED } else { 0 };
                let (args, bd) = bound_args(px, k, mcx, parts, params, &[], ex)?;
                bound = Some(bd);
                (TableDecl { over, func: table_fn::LIKE, flags, collation: collation(c, n.root), encoding: 0, args }, Class::Bits)
            }
            Known::Range { from, to } => {
                let mut bytes: PgVec<'_, u8> = PgVec::new_in(mcx);
                for v in std::iter::once(from).chain(to.iter()) {
                    table_fn::put_values(core::slice::from_ref(v), &mut |b| bytes.extend_from_slice_in(b));
                }
                let arg = px.site_mut(k)?.table_bytes_of(&bytes);
                (TableDecl { over, func: table_fn::RANGE, flags: 0, collation: collation(c, n.root), encoding: 0, args: [arg, (0, 0)] }, Class::Bits)
            }
            Known::OctetLength | Known::CharLength => {
                let mut decl = crate::dict::percode_len::decl(over, &n.known);
                // D3038 (A): the column read for its lengths and emptiness alone (octet_lane.rs `sole`)
                if matches!(n.known, Known::OctetLength) && crate::ops::scan::pgrc::strategies::octet_lane::sole(px.gx, k, n.attno) {
                    decl.flags |= table_fn::FLAG_SOLE;
                    fires::fire("percode.sole"); // shapecheck fire signal (D3038 A)
                }
                (decl, Class::Len)
            }
            Known::Like { .. } | Known::LikeBound { .. } | Known::Body => {
                let arg = px.site_mut(k)?.table_bytes_of(&n.table_fingerprint(ex.0).to_le_bytes()); // D1100: a word's root, its own table
                let class = if n.elem == ELEM_BITS { Class::Bits } else { Class::Words };
                (TableDecl { over, func: table_fn::PROGRAM, flags: n.elem, collation: 0, encoding: 0, args: [arg, (0, 0)] }, class)
            }
        };
        let t = declare_table(px, k, decl, class)?;
        if let Some(bd) = bound {
            if let Some(gravity_program::SiteKind::RangeSource { nkeys, .. }) = px.site_mut(k).map(|s| &mut s.kind) {
                *nkeys += bd.cells.len() as u16;
            }
            tables(px, k).borrow_mut().bound.push(bd); // alloc: Program
        }
        if decl.func == table_fn::PROGRAM {
            let every = crate::dict::percode_fill::unconditional(px, k, j, c, n.root);
            if every {
                fires::fire("percode.every"); // STEP-04: swept at the bind
            }
            let tabs = tables(px, k);
            let mut tb = tabs.borrow_mut();
            match tb.sweeps.iter_mut().find(|s| s.t == t as usize) {
                Some(s) => s.every |= every, // D319: one table for two equal roots; one every row reads is swept
                None => tb.sweeps.push(Sweep { t: t as usize, over, ex, root: n.root, attno: n.attno, elem: n.elem, node, walked: false, every }), // alloc: Program
            }
            drop(tb);
            mark_swept(px, k, &tabs, t as usize);
        }
        reads.push((t, class)); // alloc: Program
    }
    for n in c.prog.nodes.iter_mut() {
        if let ExprNode::Var(v) = n {
            if v.src == VarSource::Scan && !cols.iter().any(|x| x.0 == v.attno) {
                v.src = VarSource::Outer;
            }
        }
    }
    // shapecheck BG-03 / B2-17 / STEP-07: the per-code road is taken (every table declared)
    for r in plan.roots.iter().filter(|r| cols.iter().any(|x| x.0 == r.attno)) {
        match (r.form, r.table.map(|t| &plan.needs[t as usize].known)) {
            (_, Some(Known::Like { ilike, .. })) => fires::fire(if hosted_like(c, r.node, *ilike, px.gx.catalog) { "percode.like" } else { "percode.ilike_body" }), // H04
            (_, Some(Known::Eq { .. })) => fires::fire("percode.eq"),
            (_, Some(Known::EqBound { .. })) => fires::fire("percode.eq_bound"), // X5
            (_, Some(Known::LikeBound { ilike, .. })) => fires::fire(if hosted_like(c, r.node, *ilike, px.gx.catalog) { "percode.like_bound" } else { "percode.ilike_body" }), // PS1-15
            (_, Some(Known::Range { .. })) => fires::fire("percode.range"),
            (_, Some(Known::RangeBound { .. })) => fires::fire("percode.range_bound"), // NEW-72 entry C
            (_, Some(Known::OctetLength | Known::CharLength)) => fires::fire("percode.length"),
            (_, Some(Known::Body)) => fires::fire("percode.body"),
            _ => {}
        }
    }
    Some(Rc::new(Plan { cols, tables: reads, tabs: tables(px, k), ext: Default::default() })) // alloc: Program
}

/// X5 / PS1-15: a `FLAG_BOUND` table's arguments on pipeline `k`'s site: `values` (`put_values`: an EQ table's constants,
/// a LIKE pattern's constant runs) and the key cells of `params`, then of `consts` (NEW-72 entry C: bound text constants'
/// need indices), after every cell the site has (the sample's, the tid range's). The site's cell count grows only once the table is declared (`attach`): the source counts a site's bound
/// cells from its declared tables.
fn bound_args(px: &mut Px<'_, '_>, k: usize, mcx: Mcx<'_>, values: &[impl AsRef<[u8]>], params: &[(i32, u32)], consts: &[u32], ex: ExId) -> Option<([(u32, u32); 2], Bound)> {
    let site = px.site_mut(k)?;
    let gravity_program::SiteKind::RangeSource { nkeys, .. } = &site.kind else { return None };
    let first = *nkeys;
    let cells: Vec<u16> = (first..first.checked_add(u16::try_from(params.len() + consts.len()).ok()?)?).collect(); // alloc: Program
    let (mut vb, mut cb): (PgVec<'_, u8>, PgVec<'_, u8>) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    table_fn::put_values(values, &mut |b| vb.extend_from_slice_in(b));
    table_fn::put_cells(&cells, &mut |b| cb.extend_from_slice_in(b));
    let args = [site.table_bytes_of(&vb), site.table_bytes_of(&cb)];
    Some((args, Bound { cells, params: params.to_vec(), consts: consts.to_vec(), ex })) // alloc: Program
}

/// Site lane `lane` of pipeline `k`'s relation rows and the column's entries, from the relation's parts
/// (`Facts::rel_rows`, `att_entries`); `None` off a pgrc range or without the counts (the body keeps its table).
fn col_counts(px: &mut Px<'_, '_>, k: usize, lane: u8) -> Option<(u64, u64)> {
    let gravity_decompose::graph::Source::Range { relid, .. } = px.gx.pipelines()[k].source else { return None };
    let Some(LaneId::Attr(a)) = px.site_mut(k).and_then(|s| s.lanes().get(lane as usize)).map(|l| l.id) else { return None };
    Some((px.gx.facts.rel_rows(relid)?, px.gx.facts.att_entries(relid, a)?))
}

/// PS1-29 / R5-B: a per-code body at root `root` over a column of `n` entries in `rows` rows reads the datum instead of its
/// per-code table where the class table's rows read the datum no dearer at both ends, the leader and the server's widest
/// width (`cost::forms::percode_datum`, by the body's class: one that transforms the column, `transforms`, or one that
/// compares it); where the ends disagree, the rule of before (`cost::near_unique`: nearly an entry a row, each entry read
/// about once, and at least `cost::PERCODE_DATUM_ROWS_MIN` rows).
fn reads_datum(c: &Compiled<'_>, root: NodeId, rows: u64, n: u64, widest: u16) -> bool {
    use gravity_program::cost::{forms::percode_datum, near_unique};
    percode_datum(n, rows, transforms(&c.prog, root, false), widest).unwrap_or_else(|| near_unique(rows, n))
}

/// R5-B's body class: a call gravity_expr does not open-code under another call at or below `id` (`under`: inside one), a
/// body that transforms the column before it compares (`md5(c) < 'x'`, `upper(c) = 'X'`), not one that compares the
/// column itself (a bpchar range).
fn transforms(p: &gravity_expr::Program<'_>, id: NodeId, under: bool) -> bool {
    crate::stack_check();
    let here = match &p.nodes[id.0 as usize] {
        ExprNode::Func(n) | ExprNode::Op(n) | ExprNode::Distinct(n) | ExprNode::NullIf(n) => !gravity_expr::emit::prim::is_prim(p, &p.sites[n.site.0 as usize], false),
        _ => false,
    };
    (here && under) || gravity_expr::percode::walk_kids(p, id).into_iter().any(|k| transforms(p, k, under || here))
}

/// A root this road serves: no bare column at the top (the caller forwards the cell), no by-reference result, and the
/// road of its table on (LIKE verdicts and bodies `percode.table`, lengths `percode.length`).
fn served(c: &Compiled<'_>, plan: &PcPlan, r: &gravity_expr::percode::Root) -> bool {
    let need = r.table.map(|t| &plan.needs[t as usize]);
    match (r.form, need.map(|n| &n.known)) {
        (Form::Code, _) => r.node != c.root,
        (Form::Derived, _) => false,
        (_, Some(Known::OctetLength | Known::CharLength)) => PerValue::Length.on(),
        (_, Some(Known::Body)) => PerValue::Table.on() && need.is_some_and(|n| n.elem == ELEM_BITS || n.elem == ELEM_I64),
        _ => PerValue::Table.on(),
    }
}

/// H04: the hosts' LIKE table for a LIKE root; for an ILIKE only under a deterministic collation whose ctype is C (D2680:
/// the table's matcher folds ASCII). Any other ILIKE is the root's own body under its collation, a bit per entry the rows
/// fill (percode_fill.rs), so every ILIKE over a code column is evaluated once per entry.
fn hosted_like(c: &Compiled<'_>, root: NodeId, ilike: bool, cat: &dyn gravity_expr::Catalog<'_>) -> bool {
    !ilike || cat.text(collation(c, root)).is_ok_and(|t| t.deterministic && t.ctype_is_c)
}

fn set_src(c: &mut Compiled<'_>, at: &[usize], src: VarSource) {
    for &i in at {
        if let ExprNode::Var(v) = &mut c.prog.nodes[i] {
            v.src = src;
        }
    }
}

/// The collation of the call at `root` (under one NOT).
fn collation(c: &Compiled<'_>, root: NodeId) -> u32 {
    let p = &c.prog;
    let id = match &p.nodes[root.0 as usize] {
        ExprNode::Bool(n) => p.kids[n.args.start as usize],
        _ => root,
    };
    match &p.nodes[id.0 as usize] {
        ExprNode::Op(n) | ExprNode::Func(n) | ExprNode::Distinct(n) => p.sites[n.site.0 as usize].collation,
        _ => 0,
    }
}

/// `c` emitted over code columns `cols` in a scratch function: `Err(attno)` names a column whose code reached a place
/// only its datum serves.
fn probe(c: &Compiled<'_>, cols: &[(i16, u8, u8)], plan: &PcPlan, mcx: Mcx<'_>) -> Result<(), i16> {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("percode_probe", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let (zero, t, fl) = (b.iconst(Type::I64, 0), b.bconst(true), b.bconst(false));
    let codes: PgVec<'_, CodeRef> = cols.iter().map(|&(attno, ordinal, width)| {
        CodeRef { attno, lane: LaneRef { ptr: p, valid: p, valid_null: None, width, kind: KIND_CODES, all_valid: false }, code: None, ordinal, part: Some(zero) }
    }).collect_in(mcx);
    let tabs: PgVec<'_, TableRef> = plan.needs.iter().map(|_| TableRef::Words { vals: p, faults: Some(p) }).collect_in(mcx);
    let width = c.prog.nodes.iter().filter_map(|n| if let ExprNode::Var(v) = n { Some(v.attno.max(0) as usize) } else { None }).max().unwrap_or(0);
    let (vals, valid) = (mcx::pg_vec![in mcx; zero; width], mcx::pg_vec![in mcx; t; width]);
    let n = c.param_count(mcx).unwrap_or(0);
    let (cv, cn) = (mcx::pg_vec![in mcx; zero; n], mcx::pg_vec![in mcx; fl; n]);
    let cols_v = ColsRef::Values { values: &vals, valids: &valid };
    let refs = RowRefs { scan: ColsRef::Lanes { row: zero, lanes: &[] }, outer: cols_v, inner: cols_v, codes: &codes, tables: &tabs, cells: Some(p), region: Some(p), ctx: Some(p), ..RowRefs::scan(ColsRef::None, &cv, &cn, mcx) };
    let r = c.emit(&mut b, &refs);
    b.ret(&[zero]);
    match r {
        Ok(_) => Ok(()),
        Err(EmitError::CodesLane(a)) => Err(a),
        Err(_) => Err(cols[0].0),
    }
}
