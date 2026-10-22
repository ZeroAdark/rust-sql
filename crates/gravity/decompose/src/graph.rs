use gravity_abi::breaker::{Breaker, KeyKind, RecordLayout, SlotKind};
use gravity_program::{ArenaRef, ArenaVec, End, Pipeline, SiteDesc};
use types_nodes::list::{IntList, NodeList};
use types_nodes::plannodes::Plan;
use types_nodes::primnodes::{SubLinkType, SubPlan, INDEX_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::ops::shared::agg::GroupDecl;
use crate::ops::scan::closure::SampleDesc;
use crate::ops::shared::scan::{BitmapPlan, TidForm};
use crate::catalog::{Am, ExprFp};
use crate::ledger::Kind;

#[derive(Clone, Copy, Debug)]
pub struct IndexKey<'p> {
    /// The index column's table attno (0: an expression column or one past the facts' keys) and the index column, 1-based.
    pub attno: i16,
    pub col: u16,
    pub opno: u32,
    pub value: Node<'p>,
    pub form: KeyForm,
}

/// An index clause's form (D5): `column op value`; `column op ANY(array)` — a loop of probes, one per element (the OL
/// `IN (…)` keys); `column IS NOT NULL` / `IS NULL` — the min / max InitPlan's clause, the value is the null test itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyForm {
    Op,
    Any,
    NotNull,
    Null,
    /// A non-btree AM's opclass strategy (`&&`, `@>`, hash `=`): the AM's `consistent` decides; `AmAny` with `ANY(array)`.
    AmOp,
    AmAny,
    /// `(a, b) op (x, y)`: the value is the `RowCompareExpr`; `attno` / `col` / `opno` its first member's.
    Row,
    /// `ORDER BY column <op> value` (a nearest-neighbour walk): after the clause keys, in `ORDER BY` position.
    OrderBy,
}

impl KeyForm {
    pub const fn name(self) -> &'static str {
        match self {
            KeyForm::Op => "op",
            KeyForm::Any => "any",
            KeyForm::NotNull => "notnull",
            KeyForm::Null => "null",
            KeyForm::AmOp => "amop",
            KeyForm::AmAny => "amany",
            KeyForm::Row => "row",
            KeyForm::OrderBy => "orderby",
        }
    }
}

impl<'p> IndexKey<'p> {
    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        write!(out, "att={} op={}", self.attno, self.opno)?;
        if self.attno == 0 {
            write!(out, " col={}", self.col)?;
        }
        if self.form != KeyForm::Op {
            write!(out, " form={}", self.form.name())?;
        }
        write!(out, " value=")?;
        crate::expr::print(self.value, out)
    }
}

pub enum Source<'p> {
    Range { site: SiteDesc, am: Am, scanrelid: u16, relid: u32 },
    Index { site: SiteDesc, am: Am, scanrelid: u16, relid: u32, indexid: u32, keys: ArenaVec<IndexKey<'p>>, only: bool, backward: bool, recheck: &'p NodeList<'p>, iam: crate::catalog::IndexAm },
    Const,
    Values { scanrelid: u16, lists: &'p NodeList<'p> },
    Breaker { slot: u8 },
    Named { scanrelid: u16, relid: u32, name: &'p str },
    /// `morsel`: the morsel form (decompose closure.rs `morsel_form`): a heap range walk over the leader's
    /// frozen bitmap, `recheck` a filter on every row.
    Bitmap { site: SiteDesc, am: Am, scanrelid: u16, relid: u32, plan: BitmapPlan<'p>, recheck: &'p NodeList<'p>, morsel: bool },
    Tid { site: SiteDesc, am: Am, scanrelid: u16, relid: u32, form: TidForm, quals: &'p NodeList<'p> },
    Sample { site: SiteDesc, am: Am, scanrelid: u16, relid: u32, sample: SampleDesc<'p> },
    TableFunc { scanrelid: u16, func: &'p types_nodes::primnodes::TableFunc<'p>, cols: u16, ordinality: i32 },
}

impl<'p> Source<'p> {
    /// The relation a source scans and its site: every source with attribute lanes (an index-only scan's are the index's columns).
    pub fn relation(&self) -> Option<(u16, u32, &SiteDesc)> {
        match self {
            Source::Range { site, scanrelid, relid, .. } | Source::Index { site, scanrelid, relid, .. } | Source::Bitmap { site, scanrelid, relid, .. } | Source::Tid { site, scanrelid, relid, .. } | Source::Sample { site, scanrelid, relid, .. } => Some((*scanrelid, *relid, site)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Bound<'p> {
    All,
    Const(i64),
    Expr(Node<'p>),
}

pub enum Op<'p> {
    Filter(Node<'p>),
    Gate(Node<'p>),
    Project(&'p NodeList<'p>),
    Limit { count: Bound<'p>, offset: Bound<'p>, ties: Option<crate::ops::TieDesc> },
    Sort(crate::ops::shared::sort::SortDesc<'p>),
    Join(crate::join::JoinDesc<'p>),
    Window(crate::ops::window::WindowDesc<'p>),
    SetOp(crate::ops::shared::setop::SetDesc<'p>),
    SubPlan(crate::ops::shared::subplan::SubPlanDesc<'p>),
    Append(crate::ops::shared::append::AppendDesc<'p>),
    Write(crate::ops::shared::modifytable::WriteDesc<'p>),
    Lock(crate::ops::shared::modifytable::LockDesc<'p>),
    Srf(crate::ops::shared::srf::SrfDesc<'p>),
    /// V4R-147: DISTINCT ON over an input ordered by its keys (a Unique whose output reads a column outside them): the
    /// first row of each run of equal keys (the plan's `uniqOperators`, two NULLs equal) passes, the run's rest drops.
    FirstOfRun(crate::ops::TieDesc),
    /// D1071: a sorted GroupAggregate whose aggregate arguments or FILTER call something VOLATILE, over its ordered input:
    /// group `PipelineGraph::streams[i]`, one open group at a time, each handed on when the next group's first row comes
    /// (or the input ends) before that row's arguments run, as nodeAgg's agg_retrieve_direct advances them.
    GroupRun(u16),
}

/// A plan node a pipeline runs and its first op (v1's `node_ops`); `producer_top`: a breaker's entry in its producer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeOps {
    pub node_id: i32,
    pub first_op: u16,
    pub producer_top: bool,
}

pub struct PipelineNode<'p> {
    pub node_id: i32,
    pub source: Source<'p>,
    pub ops: ArenaVec<Op<'p>>,
    pub desc: Pipeline,
    /// An open over streamed union `PipelineGraph::unions[u]`: its ops are copied per arm at `push` (append.rs).
    pub union: Option<u32>,
    /// The plan nodes the pipeline runs, source first (`NodeOps`).
    pub node_ops: ArenaVec<NodeOps>,
    pub(crate) mark: u16,
}

impl<'p> PipelineNode<'p> {
    pub fn new(arena: ArenaRef, node_id: i32, source: Source<'p>, desc: Pipeline) -> PipelineNode<'p> {
        PipelineNode { node_id, source, ops: ArenaVec::new_in(arena), desc, union: None, node_ops: ArenaVec::new_in(arena), mark: 0 }
    }

    pub fn push(&mut self, op: Op<'p>) {
        self.ops.push(op);
    }

    pub fn insert(&mut self, i: usize, op: Op<'p>) {
        self.ops.insert(i, op);
        for n in self.node_ops.iter_mut().filter(|n| n.first_op as usize > i) {
            n.first_op += 1;
        }
        if self.mark as usize > i {
            self.mark += 1;
        }
    }

    pub fn close_node(&mut self, node_id: i32) {
        self.node_ops.push(NodeOps { node_id, first_op: self.mark, producer_top: false });
        self.mark = self.ops.len() as u16;
    }

    pub fn end(&self) -> End {
        self.desc.ends_in
    }
}

/// A column in a pipeline's coordinates: `OUTER_VAR` + a resno of the last projection, a scan's range-table
/// index or `INDEX_VAR` + an attribute / index column before one, `INNER_VAR` + a resno inside a join's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Col {
    pub varno: i32,
    pub attno: i16,
}

impl Col {
    pub const fn outer(attno: i16) -> Col {
        Col { varno: OUTER_VAR, attno }
    }

    /// The column a bare `Var` (through relabels) names, or none.
    pub fn of(mut e: Node<'_>) -> Option<Col> {
        while let Some(a) = crate::expr::through_relabel(e) {
            e = a;
        }
        let v = e.as_var()?;
        (v.varlevelsup == 0 && v.varattno > 0).then_some(Col { varno: v.varno, attno: v.varattno })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderKey {
    pub col: Col,
    pub desc: bool,
    pub nulls_first: bool,
}

pub use gravity_abi::limits::{MAX_ORDER_EQS, MAX_ORDER_KEYS};

/// The order a pipeline's rows come in, with the column equalities every row satisfies (`eqs`: an equality
/// filter's two columns, an inner merge join's clause sides) — a key is as good as any column equal to it. A
/// prefix of the full order is still an order: the keys are cut at `MAX_ORDER_KEYS` and at the first key a
/// projection drops. `n == 0` is no known order; a known order never over-claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub n: u8,
    pub keys: [OrderKey; MAX_ORDER_KEYS],
    pub neq: u8,
    pub eqs: [[Col; 2]; MAX_ORDER_EQS],
    /// Per key: the expression an index column computes (`attno` 0, matched at the projection); `NONE` = a column.
    pub exprs: [ExprFp; MAX_ORDER_KEYS],
    /// Per key: the btree opfamily whose order it is when that is not its type's default (a Sort by record_image_ops' `*<`,
    /// matview#113's): a consumer asking that family's order matches it (`sort::ordered_as_order`, a merge join by the
    /// family), any other never does (`sort::leads_with`). 0 = the type's default family, every other order's.
    pub fams: [u32; MAX_ORDER_KEYS],
}

/// An order's equalities as classes of columns, labeled once: `same(a, b)` for `a != b` is both columns carrying one
/// label. `Order::same`'s walk answers the same (a class holds at most `MAX_ORDER_EQS + 1` columns, all inside its set).
struct EqClasses {
    cols: [Col; EqClasses::CAP],
    label: [u8; EqClasses::CAP],
    n: usize,
}

impl EqClasses {
    const CAP: usize = 2 * MAX_ORDER_EQS;

    fn of(o: &Order) -> EqClasses {
        let mut s = EqClasses { cols: [Col { varno: 0, attno: 0 }; EqClasses::CAP], label: [0; EqClasses::CAP], n: 0 };
        for [x, y] in &o.eqs[..o.neq as usize] {
            let (i, j) = (s.slot(*x), s.slot(*y));
            let (keep, gone) = (s.label[i], s.label[j]);
            if keep != gone {
                for l in &mut s.label[..s.n] {
                    if *l == gone {
                        *l = keep;
                    }
                }
            }
        }
        s
    }

    fn slot(&mut self, c: Col) -> usize {
        if let Some(i) = self.cols[..self.n].iter().position(|&x| x == c) {
            return i;
        }
        self.cols[self.n] = c;
        self.label[self.n] = self.n as u8;
        self.n += 1;
        self.n - 1
    }

    /// The class of `c`, or none when no equality names it.
    fn label(&self, c: Col) -> Option<usize> {
        self.cols[..self.n].iter().position(|&x| x == c).map(|i| self.label[i] as usize)
    }
}

/// Per class of `EqClasses`, its first two entries of distinct columns so far: D3856's pairing in one pass.
struct ClassFirsts([[Option<(usize, Col)>; 2]; EqClasses::CAP]);

impl ClassFirsts {
    const fn new() -> ClassFirsts {
        ClassFirsts([[None; 2]; EqClasses::CAP])
    }

    /// The first earlier entry of class `l` whose column is not `b`; then entry `j` (column `b`) joins the class.
    fn pair(&mut self, l: usize, j: usize, b: Col) -> Option<usize> {
        let seen = &mut self.0[l];
        let i = match seen[0] {
            Some((i0, c0)) if c0 != b => Some(i0),
            _ => seen[1].map(|(i1, _)| i1),
        };
        match seen[0] {
            None => seen[0] = Some((j, b)),
            Some((_, c0)) if seen[1].is_none() && c0 != b => seen[1] = Some((j, b)),
            _ => {}
        }
        i
    }
}

impl Order {
    pub const NONE: Order = Order { n: 0, keys: [OrderKey { col: Col { varno: 0, attno: 0 }, desc: false, nulls_first: false }; MAX_ORDER_KEYS], neq: 0, eqs: [[Col { varno: 0, attno: 0 }; 2]; MAX_ORDER_EQS], exprs: [ExprFp::NONE; MAX_ORDER_KEYS], fams: [0; MAX_ORDER_KEYS] };

    pub fn keys(&self) -> &[OrderKey] {
        &self.keys[..self.n as usize]
    }

    /// Whether the order leads with a key of its type's default family. An order led by another family's key (a Sort by
    /// record_image_ops' `*<`: `fams`) is no known order to a consumer of the default family's, as before such keys were
    /// declared (its refusals name "no known order"); only a consumer asking that family takes it (`sort::ordered_as_order`).
    pub const fn is_known(&self) -> bool {
        self.n > 0 && self.fams[0] == 0
    }

    pub fn push(&mut self, key: OrderKey) {
        self.push_fam(key, 0);
    }

    pub fn push_expr(&mut self, key: OrderKey, fp: ExprFp) {
        if (self.n as usize) < MAX_ORDER_KEYS {
            self.keys[self.n as usize] = key;
            self.exprs[self.n as usize] = fp;
            self.fams[self.n as usize] = 0;
            self.n += 1;
        }
    }

    /// A key in the order of btree opfamily `fam` (0: its type's default family's, `push`).
    pub fn push_fam(&mut self, key: OrderKey, fam: u32) {
        if (self.n as usize) < MAX_ORDER_KEYS {
            self.keys[self.n as usize] = key;
            self.exprs[self.n as usize] = ExprFp::NONE;
            self.fams[self.n as usize] = fam;
            self.n += 1;
        }
    }

    pub fn push_eq(&mut self, a: Col, b: Col) {
        if (self.neq as usize) < MAX_ORDER_EQS && a != b {
            self.eqs[self.neq as usize] = [a, b];
            self.neq += 1;
        }
    }

    /// The keys gone, the equalities kept (a hash join's probe rows are the outer's, unordered).
    pub const fn unordered(self) -> Order {
        Order { n: 0, ..self }
    }

    /// Whether a key or equality names the coordinates `varno` (a scan's own `Var`s, else a subquery scan's positions).
    fn mentions(&self, varno: i32) -> bool {
        self.keys().iter().any(|k| k.col.varno == varno) || self.eqs[..self.neq as usize].iter().flatten().any(|c| c.varno == varno)
    }

    /// D4666: the order once a filter `c = constant` (a btree equality, order.rs `pinned_col`) holds on every row: a
    /// key of the type's default family on `c`, or on a column equal to it, ties on every row, so it is dropped and the
    /// keys after it still order the rows (PostgreSQL's `pathkey_is_redundant`: a pathkey whose EquivalenceClass holds a
    /// constant; two readers of one CTE, each pinning the store's leading key to its own value, merge-joined on the keys
    /// after it). An expression key or another family's key stays. `c` in the filter's coordinates, a scan's own `Var`
    /// read as `project` reads it.
    pub fn pinned(self, c: Col) -> Order {
        let c = if c.varno > 0 && !self.mentions(c.varno) { Col::outer(c.attno) } else { c };
        let mut out = Order { n: 0, ..self };
        for i in 0..self.n as usize {
            if self.fams[i] == 0 && !self.exprs[i].is_some() && self.same(self.keys[i].col, c) {
                continue;
            }
            let j = out.n as usize;
            (out.keys[j], out.exprs[j], out.fams[j]) = (self.keys[i], self.exprs[i], self.fams[i]);
            out.n += 1;
        }
        out
    }

    /// Whether every row has `a = b`: the same column, or connected through the equalities.
    pub fn same(&self, a: Col, b: Col) -> bool {
        if a == b {
            return true;
        }
        let mut set = [a; MAX_ORDER_EQS + 1];
        let mut n = 1;
        let mut grew = true;
        while grew {
            grew = false;
            for [x, y] in &self.eqs[..self.neq as usize] {
                let (inx, iny) = (set[..n].contains(x), set[..n].contains(y));
                if inx != iny && n < set.len() {
                    set[n] = if inx { *y } else { *x };
                    n += 1;
                    grew = true;
                }
            }
        }
        set[..n].contains(&b)
    }

    /// The order after a projection: each key's column (or one equal to it) found as a bare `Var` of an entry (a
    /// scan RTE's `Var` over `OUTER_VAR` coordinates is a subquery / CTE scan's reference to the same positions),
    /// the keys cut at the first one the projection drops, an equality kept when both columns survive.
    pub fn project(&self, tlist: &NodeList<'_>) -> Order {
        let col_of = |e: Node<'_>| -> Option<Col> {
            let v = e.as_target_entry().and_then(|t| Col::of(t.expr))?;
            Some(if v.varno > 0 && !self.mentions(v.varno) { Col::outer(v.attno) } else { v })
        };
        let exact = |c: Col| tlist.iter().position(|e| col_of(e) == Some(c)).map(|i| Col::outer(i as i16 + 1));
        let equal = |c: Col| tlist.iter().position(|e| col_of(e).is_some_and(|v| self.same(v, c))).map(|i| Col::outer(i as i16 + 1));
        let expr = |c: Col, fp: ExprFp| tlist.iter().position(|e| e.as_target_entry().is_some_and(|t| crate::catalog::expr_fp(t.expr, c.varno) == Some(fp))).map(|i| Col::outer(i as i16 + 1));
        let mut out = Order::NONE;
        for (i, key) in self.keys().iter().enumerate() {
            // a key of another family's order goes by its own column only: an equality (the default family's `=`) does
            // not carry it (numeric 1.0 = 1.00, their images differ)
            let found = if self.exprs[i].is_some() {
                expr(key.col, self.exprs[i])
            } else if self.fams[i] != 0 {
                exact(key.col)
            } else {
                exact(key.col).or_else(|| equal(key.col))
            };
            match found {
                Some(col) => out.push_fam(OrderKey { col, ..*key }, self.fams[i]),
                None => break,
            }
        }
        // D3856: an equality rides a bare column or a relabel that keeps its collation, never another expression (a call,
        // volatile or not; `x COLLATE "C"`, which the planner turns into a relabel that compares by another collation)
        let eq_of = |e: Node<'_>| -> Option<Col> {
            let mut x = e.as_target_entry()?.expr;
            while let Some(r) = x.as_relabel_type() {
                let a = crate::expr::through_relabel(x)?;
                let coll = a.as_var().map(|v| v.varcollid).or_else(|| a.as_relabel_type().map(|q| q.resultcollid))?;
                if coll != r.resultcollid {
                    return None;
                }
                x = a;
            }
            x.as_var()?;
            col_of(e)
        };
        let exact_eq = |c: Col| tlist.iter().position(|e| eq_of(e) == Some(c)).map(|i| Col::outer(i as i16 + 1));
        for [a, b] in &self.eqs[..self.neq as usize] {
            if let (Some(a), Some(b)) = (exact_eq(*a), exact_eq(*b)) {
                out.push_eq(a, b);
            }
        }
        // D3856: two kept columns equal through a dropped one (`a = c`, `c = b`, `c` not projected) stay equal. Each
        // entry pairs with the first earlier entry of another column in its class (`same`). D3925: the classes are
        // labeled once and each keeps its first two distinct entries so far, so the pairs are found in one pass over the
        // list, not one pass per entry (a join chain's projections carry a column per relation).
        let classes = EqClasses::of(self);
        let mut firsts = ClassFirsts::new();
        for (j, e) in tlist.iter().enumerate() {
            let Some(b) = eq_of(e) else { continue };
            let Some(l) = classes.label(b) else { continue };
            let i = firsts.pair(l, j, b);
            if let Some(i) = i.filter(|&i| !out.same(Col::outer(i as i16 + 1), Col::outer(j as i16 + 1))) {
                out.push_eq(Col::outer(i as i16 + 1), Col::outer(j as i16 + 1));
            }
        }
        out
    }

    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        for (i, key) in self.keys().iter().enumerate() {
            if i > 0 {
                write!(out, ",")?;
            }
            match key.col.varno {
                OUTER_VAR => {}
                INDEX_VAR => write!(out, "index.")?,
                v => write!(out, "{v}.")?,
            }
            if self.exprs[i].is_some() {
                write!(out, "expr")?;
            } else {
                write!(out, "{}", key.col.attno)?;
            }
            write!(out, "{}{}", if key.desc { "-" } else { "+" }, if key.nulls_first { "n" } else { "" })?;
            if self.fams[i] != 0 {
                write!(out, "@{}", self.fams[i])?;
            }
        }
        for [a, b] in &self.eqs[..self.neq as usize] {
            write!(out, " {}={}", a.attno, b.attno)?;
        }
        Ok(())
    }
}

/// A record layout as declared (decision 78): key words and slots unbounded (arena slices), `nn` = no validity word,
/// `sentinel` the `Count` slot every opened record counts a row in (`NO_SENTINEL`: the abi appends the implicit one);
/// `record()` is the abi's `RecordLayout` of it — the 8-key tier when the shape fits, else the wide tier (decision 82).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Layout<'p> {
    pub keys: &'p [KeyKind],
    pub slots: &'p [SlotKind],
    pub nn: bool,
    pub sentinel: u16,
    /// NEW-numeric-1: every MinI64 / MaxI64 slot holds a numeric order key (abi `LAYOUT_KEY_TIES`, set by `record`).
    pub ties: bool,
}

pub const NO_SENTINEL: u16 = u16::MAX;

impl<'p> Layout<'p> {
    pub const ROW: Layout<'static> = Layout { keys: &[], slots: &[], nn: true, sentinel: NO_SENTINEL, ties: false };

    pub fn new(arena: ArenaRef, keys: impl Iterator<Item = KeyKind>, slots: impl Iterator<Item = SlotKind>, nn: bool) -> Layout<'p> {
        let l = Layout::with_sentinel(arena, keys, slots, nn, NO_SENTINEL);
        Layout { sentinel: gravity_abi::breaker::first_count(l.slots).map_or(NO_SENTINEL, |j| j as u16), ..l }
    }

    pub fn with_sentinel(arena: ArenaRef, keys: impl Iterator<Item = KeyKind>, slots: impl Iterator<Item = SlotKind>, nn: bool, sentinel: u16) -> Layout<'p> {
        let (mut ks, mut ss) = (ArenaVec::new_in(arena), ArenaVec::new_in(arena));
        for k in keys {
            ks.push(k);
        }
        for s in slots {
            ss.push(s);
        }
        debug_assert!(sentinel == NO_SENTINEL || ss.get(sentinel as usize) == Some(&SlotKind::Count));
        Layout { keys: ks.leak(), slots: ss.leak(), nn, sentinel, ties: false }
    }

    /// NEW-numeric-1: this layout with its order-key extremes marked (`ties`).
    pub const fn with_ties(self, ties: bool) -> Layout<'p> {
        Layout { ties, ..self }
    }

    pub const fn implicit_sentinel(&self) -> bool {
        self.sentinel == NO_SENTINEL
    }

    pub fn nkeys(&self) -> usize {
        self.keys.len()
    }

    pub fn nslots(&self) -> usize {
        self.slots.len()
    }

    pub const fn has_validity(&self) -> bool {
        !self.nn
    }

    /// The abi's build rule: keys, slots, the implicit `Count` sentinel when the slots have none, the validity word.
    pub fn size(&self) -> u32 {
        let slots: u32 = self.slots.iter().map(|s| u32::from(s.bytes())).sum();
        let sentinel = if self.implicit_sentinel() { 8 } else { 0 };
        8 * self.keys.len() as u32 + slots + sentinel + if self.nn { 0 } else { 8 }
    }

    /// The abi's layout of this shape: the 8-key tier when it fits its arrays, else the wide tier (computed in `arena`, interned).
    pub fn record(&self, arena: ArenaRef) -> RecordLayout {
        let mut l = gravity_program::record_layout_at(arena, self.keys, self.slots, !self.nn, (!self.implicit_sentinel()).then_some(self.sentinel as usize));
        if self.ties {
            l.flags |= gravity_abi::breaker::LAYOUT_KEY_TIES;
        }
        l
    }
}

/// A breaker and the order its consumer reads the rows in (`order`: the Sort's keys, a spool's feed order
/// when the producer is serial, an `ORDER_KEY` group's ascending keys, a MergeAppend's merge keys; none
/// for a hash build / hashed set / union sink).
#[derive(Clone, Copy, Debug)]
pub struct BreakerDecl<'p> {
    pub breaker: Breaker,
    pub layout: Layout<'p>,
    pub producer: u16,
    pub order: Order,
}

/// A subplan run once before the root (D4): `pipeline` computes it, `params` are the exec parameters its first row binds
/// (the plan's `setParam`, in column order: one per output column of a ROWCOMPARE or MULTIEXPR sublink, e.g. `(a, b) =
/// (SELECT x, y ..)` or `UPDATE .. SET (a, b) = (SELECT ..)`; FACTS-1a), `param` the first of them (-1: none), `link` how
/// the rows fold into them, `slot` the breaker a CTE's rows are kept in.
#[derive(Clone, Copy, Debug)]
pub struct InitPlan<'p> {
    pub plan_id: i32,
    pub pipeline: u16,
    pub link: crate::ops::shared::subplan::SubLink,
    pub param: i32,
    pub params: &'p IntList<'p>,
    pub slot: Option<u8>,
    /// FACTS-1b F5 (`rescan.rs`): the most volatile call its plan makes, and whether running it can raise (a road may run a
    /// stable, raise-free InitPlan at open, where PostgreSQL runs it lazily at the first read).
    pub volatility: crate::effects::Volatility,
    pub raise_free: bool,
    /// D1035 step 2 (V4R-444): the plan is parallel-safe, as PostgreSQL's `SubPlan.parallel_safe` would say (no
    /// parallel-unsafe or restricted call, no temporary relation), so a parallel reader may evaluate it eagerly as a
    /// Gather's `initParam`: F5's fold (`rescan::Found::parallel`), since pgrust's planner never sets the flag (planner
    /// lib.rs `assess_parallel = false`, decision 6), a divergence to keep in step with any planner change.
    pub parallel_safe: bool,
    /// V4R-527 (W6-D652-LOCK-INITPLAN): the plan node whose `initPlan` list held it (its query level's top node, createplan.c
    /// SS_attach_initplans): a row lock's EvalPlanQual copies one attached at the lock's node or above, re-arms one below.
    pub attach: i32,
}

/// A rerun set (D4): the pipelines a parameterised inner declared that read the owner's parameters (`Plan.allParam`
/// of the node that closed each, or a slot one of them fills) — run again per outer row of the `Nest` of `node_id`
/// (`plan_id` 0) or the nested SubPlan `plan_id` attached at `node_id`, in index order. `slots` are the slots the set
/// fills (refilled per run) or reads (the rows kept, a probe's marks per run): leased for the statement's life
/// (`breakers::leased`), never reassigned. A pipeline of the inner outside every set is built once.
pub struct Rerun {
    pub node_id: i32,
    pub plan_id: i32,
    pub pipelines: ArenaVec<u16>,
    pub slots: SlotSet,
}

pub struct PipelineGraph<'p> {
    pub pipelines: ArenaVec<PipelineNode<'p>>,
    pub breakers: ArenaVec<BreakerDecl<'p>>,
    pub groups: ArenaVec<GroupDecl<'p>>,
    /// D1071: the groups an `Op::GroupRun` streams (no breaker holds them; `producer` is the pipeline running the op).
    pub streams: ArenaVec<GroupDecl<'p>>,
    pub init_plans: ArenaVec<InitPlan<'p>>,
    /// D1920: the hashed SubPlans several plan nodes call whose set is built once for the statement, `(plan_id, slot)`
    /// (ops/subplan `one_set`): each later calling node probes the slot, which stays leased for the statement.
    pub shared_sets: ArenaVec<(i32, u8)>,
    pub reruns: ArenaVec<Rerun>,
    /// D3646: the re-run ranges `breakers::hold_range` held while they were mapped, `(first pipeline, end)` (a nest's inner
    /// that reads a NestLoopParam, a correlated nested SubPlan's plan; D4584: every nest's inner, a param-free one too):
    /// every slot a pipeline of one fills or reads stays leased for the statement (`breakers::leased`), as a
    /// rerun set's do: the range runs inside its owner's pipeline.
    pub held: ArenaVec<(u16, u16)>,
    /// D1018 item 9 (b): the rerun set members PG keeps at a rescan that changes no param (a nest inner's root Sort, top-k
    /// or hashed group and every pipeline filling it, `rerun::declare_breaker_rerun`): they run at their set's first run only
    /// (the inner's first re-open) and later re-opens rewind what they filled (lowerv4 marks their protos `RERUN_KEPT`).
    pub kept: ArenaVec<u16>,
    /// The lifetime home's keep rule, asked for every pipeline mapped under a nest's or nested SubPlan's inner that its
    /// rerun set does not run again: why PG's rescan returns the rows it keeps (`lifetime::KeptRec`; lowerv4 lifetime
    /// `kept_input` reads it). An owner with no set records its kept pipelines too.
    pub kept_by: ArenaVec<crate::lifetime::KeptRec>,
    /// D4120: `(a, b)` rerun set index pairs: a run of set `a` changes a param a kept fill of set `b`, nested in `a`'s inner,
    /// reads, so it marks `b` stale (lowerv4 lifetime `program`'s refills).
    pub refills: ArenaVec<(u16, u16)>,
    /// D4120: `(set, param, node)`: a param a kept fill of rerun set `set` (a recursive union's generation set at plan node
    /// `node`) reads that no enclosing set has claimed yet (`lifetime::claim_refills`); one left at the walk's end refuses.
    pub stale_reads: ArenaVec<(u16, i32, i32)>,
    /// FACTS-1b F6 (`outs::param_reps`): every PARAM_EXEC id a nest (NestLoopParam) or a per-row SubPlan (its args) binds,
    /// with the Rep of the value its owner writes, in the order the owners' ops are met.
    pub params: ArenaVec<(i32, ParamRep)>,
    pub root: u16,
    pub mapped: u64,
    pub reads: [u16; 256], // open readers per slot (D214), an iteration driver's re-run range included
    pub outs: ArenaVec<(usize, gravity_program::ExprOut)>,
    /// The lanes of a scan the site vocabulary has no id for (D3, `sources::ScanLanes`): whole-row and system columns.
    pub scan_lanes: ArenaVec<crate::ops::scan::ScanLanes>,
    /// A pgrc scan's unprojected non-zone qual columns (decision 12 b, `sources::QualOnly`): elided only under a hit.
    pub qual_only: ArenaVec<crate::ops::scan::QualOnly>,
    pub unions: ArenaVec<crate::ops::shared::append::UnionRec<'p>>,
    /// D437: a subquery scan whose lists read its whole row as a `record` (`crate::subplan::RecordRow`).
    pub record_rows: ArenaVec<crate::ops::shared::record_row::RecordRow<'p>>,
    /// D3477 (UNION-COPY, settled): each write's COPY mark over a flattened UNION ALL subquery with appendrel members
    /// (ops/modifytable `union_members`) and the plan node of an Append whose `apprelids` hold it: lowerv4 matches the
    /// union a write's pipeline reads to its mark by that node (ops/modifytable/update_built.rs `copied_settled`).
    pub copy_unions: ArenaVec<(u32, i32)>,
    /// D3497: each such COPY mark no Append holds (one live member: setrefs drops a one-child Append) and the plan node of
    /// that member, its SubqueryScan or heap scan (lowerv4 ops/modifytable/update_built.rs `copied_one`).
    pub copy_one: ArenaVec<(u32, i32)>,
    /// D3477 (settled MERGE): per MERGE COPY mark of `copy_unions`, each child column that is the flattened subquery's own
    /// `(rti, resno, column, type)` (a plain Var whose varnosyn / varattnosyn name the subquery's column, which the mark's
    /// stored record holds as field `column`); resno -1: a child column reads the subquery otherwise (no such map: lowerv4
    /// keeps the refusal). ops/modifytable `copy_union_cols`; lowerv4 merge/lower.rs `Settled`.
    pub copy_union_cols: ArenaVec<(u32, i16, i16, u32)>,
    /// A refusal met while pushing: `Builder::run_from` returns it.
    pub fault: Option<crate::admit::Refusal>,
    /// The plan node being mapped: a producer pushed under it lists it last (`producer_top`).
    pub cur_node: Option<i32>,
    /// A recursive term being mapped (D241), or a nest's param-driven inner / a correlated nested SubPlan's plan
    /// (D3646, `breakers::hold_range`): every slot a pipeline from here on fills or reads stays leased until the range
    /// is mapped (it re-runs, so no later pipeline of it may take a slot an earlier one reads).
    pub term_from: Option<u16>,
    /// D1934 / D3097: the `plan_id` of the CTE whose CteScan is the statement's first fetch (`ops::first_fetched_outer`),
    /// with the exact licence of its plan's run from that fetch (`effects::exact` over `Proof::first_fetch`). `None` for a
    /// plan that acts, which the licence denies. lowerv4's first-fetch gate (effects/visible/ctescan.rs `first_fetch`)
    /// reads it.
    pub first_cte: Option<(i32, crate::effects::Licence)>,
    /// D3054: the producers of grouping-set input spools (`gsets::Spool::run_of`, phase 0's run as a Materialize): lowerv4
    /// lowers them as runs.gsets_spool, their order-free readers reading by record claims.
    pub spool_runs: ArenaVec<gravity_program::LifeId>,
    /// D4620 (road agg.gsets.cascade): the grouping sets folded from a finer set's records (`ops::shared::agg::Cascade`),
    /// each a group of `groups` whose producer reads its parents' owned records; lowerv4 ops/agg/cascade.rs lowers them.
    pub cascades: ArenaVec<crate::ops::shared::agg::Cascade>,
    /// D4051: the Hash nodes whose build calls user code by the effects home's F5 walk over the Hash, its subtree and its
    /// hash keys (`rerun::of_build`'s `user_call`, a data-modifying CTE read as its store: a user function, operator or
    /// type I/O, or a domain whose CHECK body calls one): lowerv4 never holds such a build's error ahead of PostgreSQL's
    /// first outer row (hashjoin build_class.rs `nonbuiltin_build`).
    pub user_hashes: ArenaVec<i32>,
    /// v5 errorhold: the statement's acting set (`effects::of_statement`): a batch re-run of the statement would repeat it,
    /// so a recheck licence is asked with it (lowerv4 `Px::licensed`).
    pub acts: bool,
    /// `acts` as mapping already walked it (`effects::of_statement_acts`), none if no mapping step asked: `acting` takes it
    /// instead of walking the statement again (D4554).
    pub(crate) acts_read: Option<bool>,
    /// v5 errorhold: mapping took a recheck licence (`licensed`, a decompose-side departure): lowerv4 marks the root
    /// pipeline `Pipeline::recheck`, so an error before any row left re-runs the statement in plan order.
    pub recheck: bool,
    /// D4203: the plan nodes mapping reads before PostgreSQL's demand under a recheck licence (ops/agg/gsets.rs
    /// `fold_ahead`'s sorted groups, ops/limit.rs `window_keeps_stop`'s window fills). That licence answers only an error
    /// raised before any row left the statement; mapping cannot see when a node runs, so lowerv4 asks it of each
    /// (frame/px.rs `Px::licensed_late`).
    pub licensed_nodes: ArenaVec<i32>,
    /// `DECISIONS.md` 4 (D) b: the statement mapped (`decompose`), which `explain_of` names a run-ahead part in; `None`
    /// for a graph built without one.
    pub stmt: Option<&'p types_nodes::plannodes::PlannedStmt<'p>>,
    /// The exact request (PLAN-v5 §2.5): the plan nodes a consumer that stops asks `Delivery::Exact` of, nothing evaluated
    /// ahead of its pull (a Limit over a WindowAgg whose fill acts, or may raise with its read-ahead licence denied:
    /// `ops::limit` window_keeps_stop); lowerv4's `frame::iface` puts it in the node's request.
    pub exact: ArenaVec<i32>,
    /// The stops a stopping consumer handed down (`facts::stream::group_streams`): the sort that keeps each one.
    pub stops: ArenaVec<crate::facts::stream::Stop<'p>>,
    /// D3373 (D2312's finite form): each recursive union's (plan node id) constants that bound its target-list
    /// set-returning functions (ops/setop.rs `srf_ends`): lowerv4 serves the union only where a literal guard finds each
    /// one small (`gravity_program::cost::expr::SRF_SMALL`).
    pub srf_lits: ArenaVec<(i32, Node<'p>)>,
    /// D3373: the per-arm copies of check option SubPlan steps whose inner is their own set's emit (nested_arms.rs
    /// `copy_sets`), each union's taken by its push (ops/append.rs `push_union`).
    pub(crate) arm_subs: ArenaVec<crate::nested_arms::ArmSub<'p>>,
    /// D2846: the streamed sorted groups whose feed hands them PostgreSQL's row order, ties too (`facts::stream::in_order`):
    /// their calls that raise by their partial sums fold in plan order, raising at the row PostgreSQL raises at
    /// (`facts::order::in_order_scope`, lowerv4's stream step), so they ask no recheck licence.
    pub in_order: ArenaVec<i32>,
    /// D3374: the groups (Agg `plan_node_id`, not hashed) PostgreSQL reads whole when the statement is read to its end, a
    /// ModifyTable's source included (`facts::stream::whole_written`), under no reader that may stop (`partial`): lowerv4
    /// lets a money or interval sum there add its rows in Gravity's tie order in the plan-order run and in a writing
    /// statement (ops/agg/trans/hosted.rs `own_order`).
    pub whole: ArenaVec<i32>,
    /// D2316 (op 14 "held at len" for runs.incremental): the Incremental Sorts (plan node ids) the plan-order run
    /// keeps whole over input that may raise, under a merge join that may stop reading them (`ops::sort::lazy_ahead`):
    /// lowerv4 holds the fill's error at the rows PostgreSQL's presorted batches handed on (ops/sort/inc_held.rs), or
    /// refuses by D2316's name where that form is not built.
    pub held_incs: ArenaVec<i32>,
    /// D2633: per CTE, the columns its CteScans read (`facts::demand::cte::CteRead`): its store keeps only those.
    pub cte_reads: ArenaVec<crate::facts::demand::cte::CteRead<'p>>,
    /// D2028 / D2112 phase B: the `plan_id`s of the InitPlans attached at the statement's ModifyTable or LockRows (or above
    /// it) whose setParams that node's subplan `extParam` holds (`ops::subplan::recheck_reads`): PostgreSQL's
    /// EvalPlanQualStart / EvalPlanQualBegin force such a pending InitPlan at a recheck (ExecSetParamPlanMulti), lowerv4
    /// marks its proto `INIT_RECHECK`.
    pub recheck_inits: ArenaVec<i32>,
    /// D2673: the `plan_id`s of the InitPlans attached below the statement's ModifyTable or LockRows (inside a SubPlan's plan,
    /// or lower in its own tree: `ops::subplan::recheck_fresh`): PostgreSQL's EvalPlanQualStart initializes their plans afresh
    /// in the recheck's EState (ExecInitSubPlan arms them), so the recheck evaluates them again where it reads them; lowerv4
    /// marks their protos `INIT_EPQ_FRESH`.
    pub epq_fresh: ArenaVec<i32>,
    /// D3271: the `plan_node_id` of the ModifyTable / LockRows `recheck_inits` and `epq_fresh` are measured from (the first
    /// down the top plan's outer chain, else the one LockRows of a statement that writes nothing, wherever it is:
    /// `ops::subplan::recheck_site`), set with the first InitPlan; -1 for none. Any other row lock (one of several) has its
    /// own EvalPlanQual, which neither list describes.
    pub recheck_node: i32,
    /// D3271: the `plan_id`s of the InitPlans attached at that node or above it in its tree (`recheck_site`'s path): the
    /// recheck copies their values; those not in `recheck_inits` the recheck's plan never reads (no param of theirs in the
    /// node's subplan `extParam`), so it never finds one pending.
    pub recheck_above: ArenaVec<i32>,
    /// C1 part 2: the statement's reader may stop before the last row at a count no plan node holds and come back for more
    /// (a named cursor's portal: each FETCH takes a counted part, PostgreSQL's ExecutePlan stops at the FETCH's count), set
    /// from `admit::partial_root` at the walk's start. A stopping consumer at the root (facts/stream.rs `cursor_root`): the
    /// families' stop roads stream to it, and the content key holds it (lower inputs/key.rs).
    pub partial: bool,
    /// PS1-05 (mechanism C): the planner's estimates a lowering weighs an eager run against.
    /// `init_costs`: each non-CTE InitPlan's charge by its `plan_id` (its SubPlan's startup_cost + per_call_cost, what
    /// SS_charge_for_initplans adds to the node it hangs on); `scan_costs`: each pgrcolumnar range scan's total_cost by
    /// its plan node id.
    pub init_costs: ArenaVec<(i32, f64)>,
    pub scan_costs: ArenaVec<(i32, f64)>,
    /// D2849 (join.rs `KeyRaise`): the hash join key nodes, both sides', whose hash PostgreSQL raises 42P22 on (lowerv4
    /// effects `raise_free` reads each as one that may raise).
    pub raising_keys: ArenaVec<Node<'p>>,
    /// D4449: each VALUES list's coded rows and cells by the list's address (`values_coded`), counted once by the walk that
    /// scans its rows for SubPlans (subplan `attach_values`).
    pub values: ArenaVec<(usize, ValuesCoded)>,
    /// D4569: the NestLoops (plan node ids) a SELECT's streamed union may carry a copy of in each arm (arm_nests.rs
    /// `per_arm`): parameter-free, the inner calm (invariant, raise-free, no VOLATILE call) and no join qual conjunct
    /// acting (one may raise: effects `union_nest_quiet`), set by join.rs `map_nest_loop` under `admit::union_nests_on`.
    pub union_nests: ArenaVec<i32>,
}

/// D4449: a VALUES list's rows holding a cell that is not a Const (a row that is not a list holds one) and the number of
/// such cells: a reader that would walk every cell to learn whether any is code reads these (`PipelineGraph::values_coded`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValuesCoded {
    pub coded_rows: u32,
    pub coded_cells: u32,
}

impl ValuesCoded {
    /// Every cell of the list a Const: a literal table's list, no row of code.
    pub fn consts_only(self) -> bool {
        self.coded_rows == 0
    }
}

/// PS1-05 (mechanism C): an InitPlan as the eager rule (`eager_mask`) weighs it: its `plan_id`, the PARAM_EXEC ids it
/// sets and its planner charge (`None`: a CTE's, never run early).
#[derive(Clone, Copy, Debug)]
pub struct InitCharge<'p> {
    pub plan_id: i32,
    pub sets: &'p IntList<'p>,
    pub cost: Option<f64>,
}

impl<'p> InitCharge<'p> {
    /// InitPlan `s`'s, as the plan holds it: its SubPlan's startup_cost + per_call_cost, what SS_charge_for_initplans adds
    /// to the node it hangs on. The one home of the charge for decompose's `init_costs` (ops/subplan) and a front hit's
    /// (exec inputs/front.rs), so both weigh one statement alike.
    pub fn of(s: &'p SubPlan<'p>) -> Self {
        let cost = (s.subLinkType != SubLinkType::CTE_SUBLINK).then(|| s.startup_cost + s.per_call_cost);
        InitCharge { plan_id: s.plan_id, sets: &s.setParam, cost }
    }
}

/// PS1-05: what an eager InitPlan's charge is weighed against: scan `plan`'s own total cost, less the InitPlans it holds
/// (a scan at the top of its query level: SS_charge_for_initplans adds their charge to the top node's cost). The one
/// home for decompose's `scan_costs` and a front hit's.
pub fn scan_charge(plan: &Plan<'_>) -> f64 {
    plan.total_cost - plan.initPlan.iter().filter_map(|n| n.as_sub_plan()).map(|s| s.startup_cost + s.per_call_cost).sum::<f64>()
}

/// PS1-05 (mechanism C), the rule: which of `ids` (PARAM_EXEC ids, in a pgrc range scan's order) the scan may run at
/// its open, given its own cost `scan` (`scan_charge`) and the statement's InitPlans `inits`. Bit `i` is set when the
/// InitPlans setting `ids[i]`, each counted once along with those of earlier ids that run early, keep the planner charge
/// under the scan's cost. So an eager run never adds more than the scan itself costs, however large the InitPlan. A cost
/// that is not held (no scan cost, a CTE's InitPlan) is no.
pub fn eager_mask(scan: Option<f64>, inits: &[InitCharge<'_>], ids: &[i32]) -> u64 {
    let Some(scan) = scan else { return 0 };
    let sets = |i: &InitCharge<'_>, id: i32| i.sets.iter().any(|p| p == id);
    let (mut mask, mut spent) = (0u64, 0.0f64);
    for (b, &id) in ids.iter().enumerate().take(64) {
        let mut add = 0.0f64;
        for (n, i) in inits.iter().enumerate().filter(|(_, i)| sets(i, id)) {
            let once = inits[..n].iter().any(|e| e.plan_id == i.plan_id && sets(e, id));
            let early = ids[..b].iter().enumerate().any(|(e, &x)| mask & (1 << e) != 0 && inits.iter().any(|q| q.plan_id == i.plan_id && sets(q, x)));
            if !once && !early {
                add += i.cost.unwrap_or(f64::INFINITY);
            }
        }
        if spent + add < scan {
            spent += add;
            mask |= 1 << b;
        }
    }
    mask
}

/// Who binds a PARAM_EXEC id per row: the NestLoop `node` (a `nestParams` entry) or the SubPlan `plan_id` (an `args` entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamOwner {
    Nest { node: i32 },
    SubPlan { plan_id: i32 },
}

/// FACTS-1b F6: the value a param holds, as its owner writes it. `out` is the owner-side expression's output
/// (`outs.rs`: the lane's Rep, a code / packed numeric / by-reference pointer included, and its life; none where no fact
/// was derived), `typid` / `typmod` / `coll` the expression's type and collation, so a key image built from the param
/// compares in one Rep with the column it meets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamRep {
    pub owner: ParamOwner,
    pub typid: u32,
    pub typmod: i32,
    pub coll: u32,
    pub out: Option<gravity_program::ExprOut>,
}

/// A set of breaker slots: the slot id space is the breaker record's `u8` (`limits::BREAKERS`).
pub type SlotSet = [u64; 4];

pub const fn slot_has(s: &SlotSet, slot: u8) -> bool {
    s[slot as usize / 64] >> (slot % 64) & 1 != 0
}

pub fn slot_set(s: &mut SlotSet, slot: u8) {
    s[slot as usize / 64] |= 1 << (slot % 64);
}

pub fn slot_unset(s: &mut SlotSet, slot: u8) {
    s[slot as usize / 64] &= !(1 << (slot % 64));
}

impl<'p> PipelineGraph<'p> {
    /// v5 errorhold: a decompose-side departure (a family's mapping choice) runs under `l`; a recheck licence marks the
    /// statement for the plan-order re-run (`recheck`). Ask `l` with the statement's acting set joined (`acts`).
    pub fn licensed(&mut self, l: &crate::effects::Licence) {
        self.recheck |= l.recheck();
    }

    /// `DECISIONS.md` 4 (D) b: plan node `id` as the harness names a part run ahead under a tagged licence
    /// (`effects::Licence::tagged`; program `Pipeline::ahead`): its index in EXPLAIN's node order and EXPLAIN's Node Type
    /// (`explain::explain_position`). `None` without the statement or outside EXPLAIN's walk.
    pub fn explain_of(&self, id: i32) -> Option<gravity_program::AheadTag> {
        crate::explain::explain_position(self.stmt?, id).map(|(node, node_type)| gravity_program::AheadTag { node, node_type })
    }

    pub fn new(arena: ArenaRef) -> PipelineGraph<'p> {
        PipelineGraph { stmt: None, pipelines: ArenaVec::new_in(arena), breakers: ArenaVec::new_in(arena), groups: ArenaVec::new_in(arena), streams: ArenaVec::new_in(arena), init_plans: ArenaVec::new_in(arena), shared_sets: ArenaVec::new_in(arena), reruns: ArenaVec::new_in(arena), held: ArenaVec::new_in(arena), kept: ArenaVec::new_in(arena), kept_by: ArenaVec::new_in(arena), refills: ArenaVec::new_in(arena), stale_reads: ArenaVec::new_in(arena), params: ArenaVec::new_in(arena), root: 0, mapped: 0, reads: [0; 256], scan_lanes: ArenaVec::new_in(arena), qual_only: ArenaVec::new_in(arena), outs: ArenaVec::new_in(arena), unions: ArenaVec::new_in(arena), record_rows: ArenaVec::new_in(arena), fault: None, cur_node: None, term_from: None, first_cte: None, spool_runs: ArenaVec::new_in(arena), cascades: ArenaVec::new_in(arena), user_hashes: ArenaVec::new_in(arena), acts: false, acts_read: None, recheck: false, licensed_nodes: ArenaVec::new_in(arena), exact: ArenaVec::new_in(arena), stops: ArenaVec::new_in(arena), srf_lits: ArenaVec::new_in(arena), arm_subs: ArenaVec::new_in(arena), in_order: ArenaVec::new_in(arena), held_incs: ArenaVec::new_in(arena), whole: ArenaVec::new_in(arena), cte_reads: ArenaVec::new_in(arena), recheck_inits: ArenaVec::new_in(arena), epq_fresh: ArenaVec::new_in(arena), recheck_node: -1, recheck_above: ArenaVec::new_in(arena), partial: false, init_costs: ArenaVec::new_in(arena), scan_costs: ArenaVec::new_in(arena), copy_unions: ArenaVec::new_in(arena), copy_one: ArenaVec::new_in(arena), copy_union_cols: ArenaVec::new_in(arena), raising_keys: ArenaVec::new_in(arena), values: ArenaVec::new_in(arena), union_nests: ArenaVec::new_in(arena) }
    }

    pub fn note(&mut self, k: Kind) {
        self.mapped |= 1u64 << (k as u8);
    }

    pub fn is_mapped(&self, k: Kind) -> bool {
        self.mapped & (1u64 << (k as u8)) != 0
    }

    pub fn mapped_kinds(&self) -> KindIter {
        KindIter { mask: self.mapped, i: 0 }
    }

    /// PS1-05 (mechanism C): which of `ids` (PARAM_EXEC ids, in a pgrc range scan's order) scan node `node` may run at its
    /// open, on this graph's planner costs (`eager_mask` over `scan_costs` and the InitPlans' `init_costs`). lowerv4
    /// decides through lower inputs/key.rs `cost_decide`, which files it as a guard of the program.
    pub fn eager_initplans(&self, node: i32, ids: &[i32]) -> u64 {
        let charge = |i: &InitPlan<'p>| InitCharge { plan_id: i.plan_id, sets: i.params, cost: self.init_costs.iter().find(|c| c.0 == i.plan_id).map(|c| c.1) };
        let inits: Vec<InitCharge<'p>> = self.init_plans.iter().map(charge).collect(); // alloc: Cold
        eager_mask(self.scan_costs.iter().find(|c| c.0 == node).map(|c| c.1), &inits, ids)
    }

    /// Push `p` (an open over a streamed union: one arm per child); the pipelines ending in `p`'s end, in push order.
    pub fn push(&mut self, mut p: PipelineNode<'p>) -> ArenaVec<u16> {
        if p.node_ops.is_empty() {
            p.node_ops.push(NodeOps { node_id: p.node_id, first_op: 0, producer_top: false });
        }
        let last = *p.node_ops.last().unwrap();
        if let Some(cur) = self.cur_node.filter(|&c| matches!(p.desc.ends_in, End::Breaker(_)) && c != last.node_id && !last.producer_top) {
            p.node_ops.push(NodeOps { node_id: cur, first_op: p.mark, producer_top: true });
        }
        let mut ends = ArenaVec::new_in(*self.pipelines.allocator()); // alloc: Cold
        self.push_into(p, &mut ends);
        ends
    }

    pub(crate) fn push_into(&mut self, p: PipelineNode<'p>, ends: &mut ArenaVec<u16>) {
        if let Some(u) = p.union {
            return crate::ops::append::push_union(self, p, u, ends);
        }
        for &c in p.desc.consumes.iter() {
            self.reads[c as usize] = self.reads[c as usize].saturating_sub(1);
        }
        let iters = crate::breakers::iter_leases(self, &p.ops);
        for s in (0..=u8::MAX).filter(|s| slot_has(&iters, *s)) {
            self.reads[s as usize] = self.reads[s as usize].saturating_sub(1);
        }
        for op in p.ops.iter() {
            if let Op::GroupRun(i) = op {
                self.streams[*i as usize].producer = gravity_program::LifeId(self.pipelines.len() as u16);
            }
        }
        ends.push(self.pipelines.len() as u16);
        self.pipelines.push(p);
    }

    /// D4449: VALUES list `lists`' coded rows and cells (`ValuesCoded`), `None` for a list decompose did not scan (a graph
    /// built by hand): its readers walk its cells.
    pub fn values_coded(&self, lists: &NodeList<'_>) -> Option<ValuesCoded> {
        let a = lists as *const NodeList<'_> as usize;
        self.values.iter().find(|e| e.0 == a).map(|e| e.1)
    }

    /// An expression node's output: its Rep, type and life.
    pub fn out_of(&self, n: Node<'_>) -> Option<gravity_program::ExprOut> {
        let a = n.as_raw().as_ptr() as usize;
        self.outs.binary_search_by_key(&a, |(k, _)| *k).ok().map(|i| self.outs[i].1)
    }

    /// The group whose records live in `life`.
    pub fn group_of(&self, life: gravity_program::LifeId) -> Option<&crate::ops::shared::agg::GroupDecl<'p>> {
        self.groups.iter().find(|g| g.producer == life)
    }

    pub fn root(&self) -> &PipelineNode<'p> {
        &self.pipelines[self.root as usize]
    }
}

pub struct KindIter {
    mask: u64,
    i: usize,
}

impl Iterator for KindIter {
    type Item = Kind;

    fn next(&mut self) -> Option<Kind> {
        while self.i < Kind::ALL.len() {
            let k = Kind::ALL[self.i];
            self.i += 1;
            if self.mask & (1u64 << (k as u8)) != 0 {
                return Some(k);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{ClassFirsts, Col, EqClasses, Order, OrderKey};
    use crate::catalog::ExprFp;

    /// D4666: `Order::pinned` drops each default-family key a constant pins (directly or through an equality), keeps the
    /// keys after it in order, and keeps an expression key, another family's key and every key on another column.
    #[test]
    fn pinned_drops_only_the_tied_keys() {
        let key = |attno: i16| OrderKey { col: Col::outer(attno), desc: false, nulls_first: false };
        let cols = |o: &Order| o.keys().iter().map(|k| k.col.attno).collect::<Vec<_>>();
        let mut o = Order::NONE;
        for a in 1..=5 {
            o.push(key(a));
        }
        // d_year (1) first, then brand / class / category / manufact
        assert_eq!(cols(&o.pinned(Col::outer(1))), [2, 3, 4, 5]);
        // a pinned key in the middle: the rows tie on it inside each value of the keys before it
        assert_eq!(cols(&o.pinned(Col::outer(3))), [1, 2, 4, 5]);
        // a column no key names: unchanged
        assert_eq!(o.pinned(Col::outer(7)), o);
        // through an equality: 6 = 2, and 6 pinned pins 2
        let mut e = o;
        e.push_eq(Col::outer(6), Col::outer(2));
        assert_eq!(cols(&e.pinned(Col::outer(6))), [1, 3, 4, 5]);
        // a scan's own Var the order does not name reads as its position (`project`'s rule)
        assert_eq!(cols(&o.pinned(Col { varno: 3, attno: 1 })), [2, 3, 4, 5]);
        // another family's key (record-image order) and an expression key stay
        let mut f = Order::NONE;
        f.push_fam(key(1), 4242);
        f.push(key(2));
        assert_eq!(cols(&f.pinned(Col::outer(1))), [1, 2]);
        let mut x = Order::NONE;
        x.push_expr(key(1), ExprFp(1, 2));
        x.push(key(2));
        assert_eq!(cols(&x.pinned(Col::outer(1))), [1, 2]);
        // every key pinned: no known order
        let mut one = Order::NONE;
        one.push(key(1));
        assert!(!one.pinned(Col::outer(1)).is_known());
    }

    /// D3925: the one-pass pairing picks, for every entry, the index the per-entry walk picked: the first earlier entry
    /// of another column `Order::same` connects to it. Random equalities over a small set of columns, random entry lists.
    #[test]
    fn one_pass_pairing_matches_the_walk() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let col = |k: u64| Col { varno: 1 + (k / 4) as i32, attno: 1 + (k % 4) as i16 };
        for _ in 0..4000 {
            let mut o = Order::NONE;
            for _ in 0..next(9) {
                o.push_eq(col(next(12)), col(next(12)));
            }
            let classes = EqClasses::of(&o);
            for a in 0..12 {
                for b in 0..12 {
                    let (a, b) = (col(a), col(b));
                    let labeled = a == b || matches!((classes.label(a), classes.label(b)), (Some(p), Some(q)) if p == q);
                    assert_eq!(labeled, o.same(a, b), "{a:?} {b:?} under {o:?}");
                }
            }
            let entries: [Option<Col>; 24] = core::array::from_fn(|_| if next(5) == 0 { None } else { Some(col(next(12))) });
            let mut firsts = ClassFirsts::new();
            for (j, b) in entries.iter().enumerate() {
                let Some(b) = *b else { continue };
                let walk = entries[..j].iter().position(|d| d.is_some_and(|a| a != b && o.same(a, b)));
                let pass = classes.label(b).and_then(|l| firsts.pair(l, j, b));
                assert_eq!(pass, walk, "entry {j} {b:?} of {entries:?} under {o:?}");
            }
        }
    }
}
