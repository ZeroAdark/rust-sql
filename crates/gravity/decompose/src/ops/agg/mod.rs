use gravity_abi::breaker::{Breaker, BreakerKind, Budget, HashAgg, KeyKind, SlotKind, Spill, ORDER_KEY, ORDER_NONE};
use gravity_abi::limits::MAX_NUM_SCALE;
use gravity_program::{limits, ArenaVec, End, LaneDesc, LaneId, LifeId, NumTypmod, OwnerDesc, Pipeline, Reason, SiteDesc, WordsDesc};
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Agg, Plan, PlannedStmt, SeqScan};
use types_nodes::primnodes::{Aggref, TargetEntry, Var, INDEX_VAR, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};
use mcx::{GrowIn, PgVec};

use crate::ops::shared::append::AppendDesc;
use crate::breakers;
use crate::expr;
use crate::graph::{Col, KeyForm, Layout, Op, Order, OrderKey, Source};
use crate::facts::demand::{reads_above, Read};
use crate::facts::hints::Hint;
use crate::facts::pinned::{pinned_word, relabels, scan_attno, scan_entry_at};
use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::ops::scan as sources;
use crate::ops::shared::scan::Atts;
use crate::walk::{Builder, Open};
use crate::ops::shared::agg_class::{Class, Ext, Mods, Split, Trans};
use crate::ops::agg::trans::table as aggtab;
use crate::catalog::typefacts;
use crate::ops::shared::sort::{KeyImage, NO_SCALE};
use crate::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, KeyDecl, Register, SetDecl, Stripped, TwoPassDecl, AGG_HASHED, AGG_MIXED, AGG_PLAIN, AGG_SORTED, NONE, NULL_ARM, STAT_REG_ROAD, fold_of, stat_slot};

/// The kernel words in the runner's fact order (`par_hashagg::PARAM_*`): top-k, dense lo / hi, bits, order.
pub const W_TOPK: u16 = 0;
pub const W_DENSE_LO: u16 = 1;
pub const W_DENSE_HI: u16 = 2;
pub const W_BITS: u16 = 3;
pub const W_ORDER: u16 = 4;
pub const NWORDS: u16 = 5;
pub use gravity_program::cost::DEFAULT_BITS;

pub mod trans;
mod dropped;
mod gsets;
mod gsets_tag;
mod keys;
mod numeric;
use gsets::*;
pub use keys::*;
pub use numeric::*;

pub use gravity_program::cost::spill_bits;

/// D2640 / D2719: a group key under a user equality operator whose procedure is not its type's own `=` (and not byteaeq
/// over a multirange Agg key), user code included: PostgreSQL groups by that procedure, the key's image by the type's `=`.
pub const USER_GROUP_EQ: &str = "a grouping key under a user operator whose procedure is not its type's own = (D2640)";

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    // D1901 (V4R-508's in-row refusal): a group's keys compare by the plan's equality operators (a semi join's inner
    // unique-ified by its user operator: nodeAgg / nodeUnique call them), a multirange key's image by multirange_eq's
    // canonical form. D2640 (D1901's trunk half): any key type's image stands for its own `=` alone, so a key under a
    // user operator over another built-in procedure refuses too (`SELECT .. WHERE a.t =% ANY (SELECT t ..)` unique-ifies
    // the inner by =% over textne: PG makes one group per row, the image one per value). D2719: one over user code
    // refuses as well: neither a btree nor a hash class makes its equality the image's (an abs-value =@= groups 3 with
    // -3; no image does).
    // D2163: a grouping-sets node's every phase, its chain's too (each hashed set and each later rollup is a chain Agg
    // with its own grpOperators over the head's input: `GROUPING SETS ((g), (m)) ORDER BY m USING <#` hashes m by the
    // sort's =#, which PG calls and the image does not). D2163 serve: a user operator whose procedure is the type's own
    // `=` keys by the image; one over byteaeq keys an Agg's multirange key by its payload bytes (`Shape::eq_op`); any
    // other refuses
    let (ops, cols): (&[u32], &[i16]) = match k {
        Kind::Agg => node.as_agg().map_or((&[], &[]), |a| (a.grpOperators, a.grpColIdx)),
        Kind::Group => node.as_group().map_or((&[], &[]), |g| (g.grpOperators, g.grpColIdx)),
        Kind::Unique => node.as_unique().map_or((&[], &[]), |u| (u.uniqOperators, u.uniqColIdx)),
        _ => (&[], &[]),
    };
    let plan = node.as_plan();
    let input = plan.and_then(|p| p.lefttree).and_then(|c| c.as_plan());
    let key_type = |c: i16| input.and_then(|i| crate::ops::shared::sort::column_type(&i.targetlist, c, k, 0).ok());
    // D2719 / D2640 serve (group lane): a sorted Agg of no grouping sets streams (`map_agg`), its run test calling the
    // user operator's procedure per row as nodeAgg's does (lowerv4 ops/group/stream.rs); KeySem slice 4: a Group's or a
    // Unique's runs through the first-of-run op compare the same way (lowerv4 per_group.rs `other_eq`; `map_set` keeps
    // them there). A hashed or set-operation group and a grouping-sets phase still refuse (their tables key by the image)
    let streams = matches!(k, Kind::Agg) && node.as_agg().is_some_and(|a| a.aggstrategy == AGG_SORTED && a.chain.iter().next().is_none() && !crate::ops::per_set(a));
    let runs = matches!(k, Kind::Group | Kind::Unique);
    let hashes = |o: u32, c: i16, t: u32| matches!(k, Kind::Agg) && node.as_agg().is_some_and(|a| crate::facts::user_hash::agg_key(b.facts, a, o, c, Some(t))); // hj-0310: a hashed Agg's user key
    let refused = |o: u32, c: i16| {
        let Some(t) = key_type(c) else { return true };
        crate::catalog::keysem::group_key_refused(b.facts, o, t, matches!(k, Kind::Agg), streams || runs, &|| hashes(o, c, t))
    };
    let user_eq = |ops: &[u32], cols: &[i16]| ops.iter().zip(cols).any(|(&o, &c)| key_type(c).is_some_and(|t| crate::effects::user_group_eq(b.facts, o, t)));
    let user = |ops: &[u32], cols: &[i16]| {
        let c = ops.iter().zip(cols).find(|&(&o, &c)| crate::effects::agg::user_op(o) && refused(o, c))?.1;
        Some(if key_type(*c).is_some_and(|t| crate::catalog::keysem::key_holds_multirange(b.facts, t)) { crate::join::USER_GROUP_OP } else { USER_GROUP_EQ })
    };
    let chain = || node.as_agg().filter(|_| matches!(k, Kind::Agg)).and_then(|a| a.chain.iter().filter_map(|n| n.as_agg()).find_map(|c| user(c.grpOperators, c.grpColIdx)));
    if let Some(why) = user(ops, cols).or_else(chain) {
        return Err(Refusal::shape(k, plan.map_or(crate::walk::STMT_NODE, |p| p.plan_node_id), why));
    }
    match k {
        Kind::Agg => {
            // D2719 / D2640: a streamed group whose run test calls a key's user `=` (`map_agg` keeps it streamed)
            let user_eq = streams && user_eq(ops, cols);
            map_agg(b, node.as_agg().unwrap(), user_eq)
        }
        Kind::Group => {
            let g = node.as_group().unwrap();
            map_set(b, &g.plan, k, GroupForm::Group, g.grpColIdx, g.grpCollations, g.grpOperators, user_eq(ops, cols))
        }
        Kind::Unique => {
            let u = node.as_unique().unwrap();
            map_set(b, &u.plan, k, GroupForm::Unique, u.uniqColIdx, u.uniqCollations, u.uniqOperators, user_eq(ops, cols))
        }
        _ => Err(Refusal::kind(k, node.as_plan().map_or(crate::walk::STMT_NODE, |p| p.plan_node_id))),
    }
}

/// The record under construction, unbounded (decision 78; the u16 word / slot ids are the limit).
struct Shape<'p> {
    keys: ArenaVec<KeyDecl<'p>>,
    aggs: ArenaVec<AggDecl<'p>>,
    key_kinds: ArenaVec<KeyKind>,
    slots: ArenaVec<SlotKind>,
    nn: bool,
    /// The first `Count` slot that counts a row per fold (`count(*)`, a count over a NOT NULL argument, unfiltered): the record's sentinel.
    row_count: u16,
    pinned: ArenaVec<i16>,
    empty_no_row: bool,
    /// D1292: the keys of a hashed group (`key_kind`'s `hashed`).
    hashed: bool,
    /// NEW-numeric-1: an error the calls raise reaches the statement before any row left (`held_whole`): the statement's
    /// recheck licence the keyed extreme folds ahead under (construct C2).
    held: Option<crate::effects::Licence>,
    /// NEW-numeric-1 (N11 (ii')): the layout's MinI64 / MaxI64 slots are all order keys (`settle_keyed`).
    ties: bool,
    /// DC-21: a plain node whose every call is one `count(DISTINCT ROW(..))` over the same row (`row_counts`): `cell` keys
    /// the row's fields (`row_count`).
    row_form: bool,
    /// D3080: the key being added may key an integer's text by the integer (`keys::int_text_words`): a hashed group's key
    /// no aggregate reads.
    int_text: bool,
    /// D3785: a node above reads one of the node's resjunk entries (`reads_above`: an ORDER BY column of a GROUP BY,
    /// the Sort sharing the Agg's target list), so its junk is a GROUP BY's, which the parser held to the grouping
    /// (check_ungrouped_columns), not a DISTINCT's window columns (window-106): the keys determine it, as an aggregating
    /// node's junk.
    read_junk: bool,
    /// D4076: the columns `duplicate_keys` left unkeyed because they are read only inside a dropped grouping expression
    /// (`dropped::rekey` keys that expression instead).
    dropped: ArenaVec<i16>,
}

impl<'p> Shape<'p> {
    fn new(arena: gravity_program::ArenaRef) -> Shape<'p> {
        Shape { keys: ArenaVec::new_in(arena), aggs: ArenaVec::new_in(arena), key_kinds: ArenaVec::new_in(arena), slots: ArenaVec::new_in(arena), nn: true, row_count: NONE, pinned: ArenaVec::new_in(arena), empty_no_row: false, hashed: false, held: None, ties: false, row_form: false, int_text: false, read_junk: false, dropped: ArenaVec::new_in(arena) }
    }

    fn key(&mut self, b: &Builder<'p, '_>, child: &Open<'p>, resno: i16, coll: u32, k: Kind, id: i32) -> Result<u16, Refusal> {
        let e = outer_expr(child, resno).ok_or(Refusal::shape(k, id, "a group key outside the input's projection"))?;
        self.key_expr(b, child, resno, Stripped::entry(e), coll, k, id)
    }

    /// D2163: the key just added compares under the plan's operator `op`: an `=` over byteaeq on a multirange-holding key
    /// (`Facts::key_eq` Bytes, a user operator; `map` refused every user operator that is neither that nor the type's
    /// own) keys by its payload bytes.
    fn eq_op(&mut self, b: &Builder<'p, '_>, op: u32) {
        if let Some(key) = self.keys.last_mut().filter(|key| key.image.is_by_ref()) {
            if let Some(bytes) = crate::catalog::keysem::multirange_bytes_eq(b.facts, op, key.tf) {
                key.bytes_eq = bytes;
            }
        }
        // D2719 / D2640: a user `=` over user code or another built-in (`map` admits it only for a sorted group that
        // streams): the run test calls its procedure
        if let Some(key) = self.keys.last_mut().filter(|key| crate::effects::user_group_eq(b.facts, op, key.tf)) {
            key.user_eq = op;
        }
    }

    /// One key: its word(s) in the record (`Fp128` is two adjacent words, low then high).
    fn key_expr(&mut self, b: &Builder<'p, '_>, child: &Open<'p>, resno: i16, e: Stripped<'p>, coll: u32, k: Kind, id: i32) -> Result<u16, Refusal> {
        let int_text = self.int_text.then(|| keys::int_text_words(b, child, e, coll)).flatten();
        if int_text.is_some() {
            fires::fire("agg.int_text_key"); // shapecheck fire signal (D3080): an integer's text keyed by the integer
        }
        let KeyWords { kind, scale, image, words, precision, tf } = match int_text {
            Some(w) => w,
            None => key_kind(b, child, e, coll, self.hashed, k, id)?,
        };
        let words = words as usize;
        let word = self.key_kinds.len();
        if word + words >= NONE as usize || self.keys.len() >= NONE as usize {
            return Err(Refusal::shape(k, id, "more than 65534 group key words (the id width)"));
        }
        let attno = scan_attno(b, child, e.node).unwrap_or(0);
        let bytes = crate::facts::pinned::scan_column(b, child, e.node).and_then(|(relid, a)| b.facts.att_value_bytes(relid, a)).unwrap_or(0);
        let rec = crate::catalog::anonymous_record(tf).then(|| crate::facts::record::key_row(b, id, resno, e.node, e.agg)).flatten();
        if rec.is_some_and(|r| crate::facts::record::indeterminate(b.facts, r)) {
            return Err(keys::indeterminate_coll(k, id));
        }
        let j = self.keys.len() as u16;
        for _ in 0..words {
            self.key_kinds.push(kind);
        }
        self.keys.push(KeyDecl { image, words: words as u8, coll, desc: false, nulls_first: false, word: word as u16, resno, kind, attno, expr: e, scale, rep: NONE, precision, tf, rec, bytes_eq: false, user_eq: 0, bytes });
        Ok(j)
    }

    /// D384: the node's lists read an input column outside its keys that is one of them again (`DISTINCT two, four, two`,
    /// the planner's pathkeys drop the repeat from grpColIdx and the target list reads input column 3 from the group's
    /// tuple): that column is its key's expression over the same input, so it joins the keys as a key of its own (the
    /// same groups; its word carries the column the emitted row reads by resno). D416, `fd` (the query's own grouping:
    /// see `query_grouping`): any other input column the lists read outside an aggregate's arguments (resjunk
    /// entries too when the node aggregates) is determined by the keys (remove_useless_groupby_columns keeps the primary key and check_functional_grouping
    /// admits no other ungrouped reference; a column an EquivalenceClass pins to a constant is the same in every row), so
    /// it too joins the keys under its own collation: the groups and a sorted input's runs are the same, and its word is
    /// the representative tuple's value PG emits. Not a column a key expression is computed from (D2343): the planner
    /// passes such a column up for a window's input (`make_window_input_target` flattens the grouped target list), and
    /// the keys do not determine it. Without `fd` (a Unique node: DISTINCT ON keeps the first row of a run whatever it
    /// carries; a semi-join's unique-ification) any other ungrouped column stays unkeyed, but for one a filter or index
    /// clause pins to one image in every row (`pinned_word`; a Const column is `const_cols`'). That is the planner's own
    /// ground for dropping a GROUP BY column from the keys, and setrefs leaves the node no `fd` evidence when it removes a
    /// trivial SubqueryScan above it (apply_tlist_labeling copies the scan's entries, ressortgroupref 0):
    /// `WITH s AS (SELECT d_year, k, sum(q) .. GROUP BY d_year, k) .. WHERE d_year = 2000`.
    fn duplicate_keys(&mut self, b: &Builder<'p, '_>, child: &Open<'p>, lists: &[&'p NodeList<'p>; 2], fd: bool, covered: &[i16], k: Kind, id: i32) -> Result<(), Refusal> {
        let n = self.keys.len();
        // resjunk entries only under an aggregate: a DISTINCT HashAgg's target list carries a window's ORDER BY column
        // as junk from any row of the group (window-106), a grouping Agg's junk is a GROUP BY / ORDER BY column the
        // parser held to the grouping. D1270: a Group node is always a GROUP BY (DISTINCT plans a Unique or an Agg), so
        // its junk is held to the grouping too (a GROUP BY column whose pathkey the planner dropped as equal to a key:
        // `GROUP BY c1.x, c2.x` over `c1.x = c2.x`, aggregates-110)
        let junk = fd && (k == Kind::Group || self.read_junk || lists.iter().any(|l| l.iter().any(|n| has_aggref(n))));
        for (c, own) in pinned_vars(b.arena, lists, junk).iter().copied() {
            if n == 0 || self.keys.iter().any(|key| key.resno == c) {
                continue;
            }
            let Some(mut e) = outer_expr(child, c).map(Stripped::entry) else { continue };
            let (coll, one) = match (0..n).find(|&i| same_expr(b, child, self.keys[i].expr, e) || equated(b, child, self.keys[i].expr, e)) {
                Some(i) => {
                    if !same_expr(b, child, self.keys[i].expr, e) {
                        fires::fire("agg.equated"); // shapecheck fire signal (REGISTRY AR-14): a join-equated column keys as the key
                    }
                    // D1353: the repeat of a key bpchar only through a stripped relabel (a semi-join's unique-ification
                    // groups on `(d.v)::bpchar` and emits `d.v`) keys as that key, trimmed: under its own varchar / text
                    // equality it split trim-equal values into groups the key holds as one (an outer row joined each)
                    e.bpchar |= self.keys[i].expr.bpchar();
                    (self.keys[i].coll, false)
                }
                // D2343: a column a key expression computes from (`GROUP BY date_trunc('month', d)` passing `d` up, the
                // planner's flattened window input) is not determined by the keys: keying it made one group per row
                // (over a Sort the key and the column are the Sort's pass-through columns: each is read through the
                // projections down to its scan expression, D2618)
                // D4076: not a column read only inside a grouping expression the planner dropped (`covered`): the
                // expression is one value per group, the column is not (`dropped::rekey` keys the expression)
                None if fd && !covered.contains(&c) && !self.keys[..n].iter().any(|key| computes_from(key.expr.node, e.node) || computes_from(scan_entry(child, key.resno).unwrap_or(key.expr.node), scan_entry(child, c).unwrap_or(e.node))) => (own, false),
                // rt_app-0272: a text / name column a deterministic equality pins (`pinned_bytes`) too
                None if pinned_word(b, child, c) || crate::facts::pinned::pinned_bytes(b, child, c, own) => (own, true),
                None => {
                    if fd && covered.contains(&c) {
                        self.dropped.push(c);
                    }
                    continue;
                }
            };
            // one value in every row: a key form the record cannot hold leaves the column unkeyed, as an unread one serves
            match self.key_expr(b, child, c, e, coll, k, id) {
                Err(_) if one => continue,
                r => _ = r?,
            }
        }
        Ok(())
    }

    /// D1214 (V4R-393): an input column the lists read outside the aggregates that the keys determine by its relation's
    /// primary key (`passenger::pk_determined`, PostgreSQL's check_functional_grouping) joins the keys under its own
    /// collation, as D416's `fd` columns do: a node whose target list lost the query's grouping (the grouping-sets union's
    /// arms) keeps what the parser admitted. D3375 (m7_passenger_fd): or by a unique index over NOT NULL columns, as
    /// PostgreSQL 18.6's remove_useless_groupby_columns reads one (`passenger::unique_determined`), the node's GROUP BY
    /// columns `grp` (`grpColIdx`, their equality operators and collations) its keys. Without either the column stays
    /// bare (D514's `min` cell, or the lowerer's refusal by name).
    fn pk_keys(&mut self, b: &Builder<'p, '_>, child: &Open<'p>, lists: &[&'p NodeList<'p>; 2], grp: (&[i16], &[u32], &[u32]), k: Kind, id: i32) -> Result<(), Refusal> {
        if self.keys.is_empty() {
            return Ok(());
        }
        let junk = self.read_junk || lists.iter().any(|l| l.iter().any(|n| has_aggref(n)));
        let mut by: ArenaVec<(Node<'p>, u32, u32)> = ArenaVec::new_in(b.arena); // alloc: Cold
        for (i, &c) in grp.0.iter().enumerate() {
            if let (Some(e), Some(&op)) = (outer_expr(child, c), grp.1.get(i)) {
                by.push((Stripped::entry(e).node(), op, grp.2.get(i).copied().unwrap_or(0))); // alloc: Cold
            }
        }
        for (c, own) in pinned_vars(b.arena, lists, junk).iter().copied() {
            if self.keys.iter().any(|key| key.resno == c) {
                continue;
            }
            let Some(e) = outer_expr(child, c).map(Stripped::entry) else { continue };
            let mut keys: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
            keys.extend(self.keys.iter().map(|key| key.expr.node()));
            if crate::facts::passenger::pk_determined(b, &keys, e.node()) || crate::facts::passenger::unique_determined(b, &by, e.node()) {
                self.key_expr(b, child, c, e, own, k, id)?;
            }
        }
        Ok(())
    }

    fn slot(&mut self, s: SlotKind, k: Kind, id: i32) -> Result<u16, Refusal> {
        let has_count = self.slots.contains(&SlotKind::Count) || s == SlotKind::Count;
        if self.slots.len() + usize::from(!has_count) >= NONE as usize {
            return Err(Refusal::shape(k, id, "more than 65534 aggregate slots (the id width)"));
        }
        self.slots.push(s);
        Ok((self.slots.len() - 1) as u16)
    }

    /// One `MinCode` representative slot per `Fp128` key after the aggregate slots (t3's `rep_slot`
    /// order), and (M6 L0-m) per float / narrow numeric key, whose image is not its output datum: the
    /// slot keeps one member's datum. A record the aggregates fill (TH1: eight slots) keeps its keys
    /// without one — the lowerer names that wall (lower.md §6.11), the decomposition stands.
    fn reps(&mut self, k: Kind, id: i32) {
        for j in 0..self.keys.len() {
            let datum = self.keys[j].resno != 0 && (self.keys[j].image.word_not_datum() || self.keys[j].tf == TIDOID); // resno 0: a DISTINCT argument, never output (uuid / interval / timetz: M6-TYPES R1c; D1379: a tid, whose one word is its order image)
            if self.keys[j].kind == KeyKind::Fp128 || datum {
                if let Ok(s) = self.slot(SlotKind::MinCode, k, id) {
                    self.keys[j].rep = s;
                }
            }
        }
    }

    fn layout(&self, arena: gravity_program::ArenaRef) -> Layout<'p> {
        Layout::with_sentinel(arena, self.key_kinds.iter().copied(), self.slots.iter().copied(), self.nn, if self.row_count == NONE { crate::graph::NO_SENTINEL } else { self.row_count }).with_ties(self.ties)
    }

    /// NEW-numeric-1 (P): the keyed extremes' layout carries abi `LAYOUT_KEY_TIES`. Where two records' keys tie on value
    /// at two dscales, a merge whose source holds the later rows takes the source's member, as PostgreSQL keeps the later
    /// row; any other merge raises into the plan-order run. The flag reads EVERY MinI64 / MaxI64 slot as a key, so a
    /// group whose layout holds another one (an integer, float or bool extreme, a dscale companion), a hosted state (its
    /// lowering appends a MaxI64 handle) or needs the wide tier folds its keyed calls as MinVar / MaxVar instead, their
    /// key slots and side companions (N13: turned into word extremes) left unread.
    fn settle_keyed(&mut self) {
        let keyed = |a: &AggDecl<'_>| matches!(a.reg, Register::MinNumDyn | Register::MaxNumDyn);
        if !self.aggs.iter().any(keyed) {
            return;
        }
        let mine = |j: usize| self.aggs.iter().any(|a| keyed(a) && a.slot as usize == j);
        let other = self.slots.iter().enumerate().any(|(j, s)| matches!(s, SlotKind::MinI64 | SlotKind::MaxI64) && !mine(j));
        let hosted = self.aggs.iter().any(|a| !keyed(a) && matches!(a.trans, Trans::Var));
        let sentinel = (self.row_count != NONE).then_some(self.row_count as usize);
        let wide = !gravity_abi::breaker::fits_inline_at(&self.key_kinds, &self.slots, !self.nn, sentinel);
        if other || hosted || wide {
            for a in self.aggs.iter_mut().filter(|a| keyed(a)) {
                let min = a.reg == Register::MinNumDyn;
                // N13: the side companion turns into an inert word extreme (an address no merge may follow)
                if let Some(s) = self.slots.get_mut(a.slot as usize + 1).filter(|s| s.is_num_side()) {
                    *s = if min { SlotKind::MinI64 } else { SlotKind::MaxI64 };
                }
                (a.reg, a.trans, a.final_, a.slot) = (if min { Register::MinVar } else { Register::MaxVar }, Trans::Var, FinalForm::Rendered, NONE);
            }
            return;
        }
        self.ties = true;
        self.aggs.iter().filter(|a| keyed(a)).for_each(|_| fires::fire("agg.num_dyn")); // shapecheck fire signal (NEW-numeric-1)
    }
}

/// D1379: a tid group key's type (its record keeps a copy of the datum beside its one order word).
const TIDOID: u32 = 27;

/// `user_eq` (D2719 / D2640, `map`): a key's user `=` that the streamed form's run test calls.
fn map_agg<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, user_eq: bool) -> Result<Open<'p>, Refusal> {
    let (id, k) = (a.plan.plan_node_id, Kind::Agg);
    crate::facts::demand::fill(b); // the column-demand fact unread_junk / unread_cols / read_junk read
    if crate::ops::per_set(a) {
        return map_sets(b, a);
    }
    let hints = Hint::of_agg(a);
    // D612: a sorted group's key Sort (absorb.rs), when no call reads the row order; D1069: nor does a VOLATILE
    // argument or FILTER, which PostgreSQL runs in the Sort's order; D2249: nor a target list or HAVING that acts outside
    // its aggregates' folds (a VOLATILE call, a catalog miss), which PostgreSQL runs once per group in the groups' key order
    let ncols = (a.numCols.max(0) as usize).min(a.grpColIdx.len());
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let keeps = a.aggstrategy == AGG_SORTED && (crate::effects::agg::volatile_inputs(&cx, &[&a.plan.targetlist, &a.plan.qual]) || crate::effects::agg::per_group_acts(&cx, &[&a.plan.targetlist, &a.plan.qual]));
    let keeps = keeps || (a.aggstrategy == AGG_SORTED && crate::effects::agg::user_folds(&cx, &[&a.plan.targetlist, &a.plan.qual])); // K4.riderg: PG's order
    // D1371 (D3374): an acting ORDER BY call beside per-row work that may raise or act: PG hands each group on first
    let keeps = keeps || (a.aggstrategy == AGG_SORTED && crate::effects::agg::sorted_calls_stream(&cx, &[&a.plan.targetlist, &a.plan.qual], &|r| crate::facts::stream::sorts_builtin(b, r)));
    let keeps = keeps || crate::facts::stream::agg_stops(b, a); // a counted LIMIT over an index's order: PG's rows (facts/stream.rs)
    let keeps = keeps || (a.aggstrategy == AGG_SORTED && !gsets::fold_ahead(b, a));
    let keeps = keeps || crate::facts::stream::streams_to_stop(b, a); // the stop road: a counted LIMIT stops the walk feeding it
    let keeps = keeps || crate::facts::stream::merge_inner(b, a); // a merge join's inner: PG's merge fetches it one group at a time
    let keeps = keeps || crate::facts::stream::collect_streams(b, a); // USEROP #530: a user-typed collect's final as the group is handed on
    let keeps = keeps || crate::facts::stream::sorts_stream(b, a); // a group's sorting states freed group by group, as PG's
    let presort = crate::facts::stream::presort_stop(b, a); // and over a DISTINCT group's presort, which the group absorbs
    let keeps = keeps || presort.is_some();
    let keeps = keeps || user_eq; // D2719 / D2640: the run test calls the user `=`
    let absorbed = (a.aggstrategy == AGG_SORTED && !keeps && hints.iter().all(Option::is_none) && crate::ops::group::absorb::order_free(b, a)).then(|| crate::ops::group::absorb::key_sort(b, &a.plan, &a.grpColIdx[..ncols], a.grpCollations)).flatten();
    let unsorted = (presort.is_none() && absorbed.is_none() && !keeps).then(|| crate::ops::group::absorb::distinct_sort(b, a)).flatten();
    b.absorb = presort.or_else(|| absorbed.as_ref().map(|s| s.id)).or(unsorted);
    // D4676 (road group.hashchain): over a Unique over the key Sort, the Unique absorbs the Sort and hands its rows on
    // unordered (`map_set`); this group hashes them and orders only its own groups (absorb.rs `chain_of`)
    b.chain = absorbed.as_ref().and_then(|s| s.chain);
    let child = b.child(&a.plan, k);
    b.absorb = None;
    b.chain = None;
    let mut child = child?;
    let mut sh = Shape::new(b.arena);
    sh.hashed = a.aggstrategy == AGG_HASHED;
    for (i, &c) in a.grpColIdx.iter().take(a.numCols.max(0) as usize).enumerate() {
        sh.int_text = sh.hashed && !keys::agg_reads(&[&a.plan.targetlist, &a.plan.qual], c);
        sh.key(b, &child, c, a.grpCollations.get(i).copied().unwrap_or(0), k, id)?;
        sh.int_text = false;
        sh.eq_op(b, a.grpOperators.get(i).copied().unwrap_or(0));
    }
    sh.hashed = false;
    // D461: GROUPING() over a node without grouping sets is 0 (every argument is grouped): the one-set fold of `arm_lists`
    let (tlist, qual) = match [&a.plan.targetlist, &a.plan.qual].iter().any(|l| l.iter().any(has_grouping_func)) {
        true => {
            let mut keys: ArenaVec<i16> = ArenaVec::new_in(b.arena);
            keys.extend(a.grpColIdx.iter().take(a.numCols.max(0) as usize).copied());
            arm_lists(b, a, &a.plan.targetlist, &keys, &keys, false, k, id)?
        }
        false => (&a.plan.targetlist, &a.plan.qual),
    };
    sh.read_junk = tlist.iter().filter_map(|n| n.as_target_entry()).any(|te| te.resjunk && reads_above(b, &a.plan, te.resno) != Read::No);
    let mut keyed: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    keyed.extend(sh.keys.iter().map(|key| key.resno));
    let fd = query_grouping(tlist);
    // D4163: a grouping node with no key left (every GROUP BY expression dropped) that has no first row to keep a
    // column from: its DISTINCT aggregate's Sort is absorbed (`unsorted`: the plain fold is not fed that Sort's first
    // row), or its calls make the grouped distinct form (`pair_distinct`). Any other such node is the plain fold's,
    // which keeps the expressions' columns from its first row (D345)
    let alone = keyed.is_empty() && a.aggstrategy == AGG_SORTED && (unsorted.is_some() || collect(b.arena, &[tlist, qual], k, id).is_ok_and(|r| pair_distinct(b, &child, &r, k, id)));
    let lost = dropped::dropped(b, tlist, qual, &keyed, fd, alone);
    let above = dropped::window_dropped(b, &a.plan, tlist, &keyed, fd);
    let mut covered: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    covered.extend(lost.covered.iter().chain(above.iter()).copied());
    sh.duplicate_keys(b, &child, &[tlist, qual], fd, &covered, k, id)?;
    sh.pk_keys(b, &child, &[tlist, qual], (&a.grpColIdx[..ncols], a.grpOperators, a.grpCollations), k, id)?;
    if alone {
        // `duplicate_keys` names the columns it left unkeyed only beside a key: with none, they are `dropped`'s
        sh.dropped.extend(lost.covered.iter().copied());
    }
    let input = a.plan.lefttree.filter(|_| alone).and_then(|n| n.as_plan()).map(|p| &p.targetlist);
    let (tlist, qual) = dropped::rekey(b, &mut child, &mut sh, &lost, tlist, qual, input, k, id)?;
    // D4163: the dropped expressions key the node's one group, as pinned columns do below
    let rekeyed = alone && !sh.keys.is_empty();
    // D514 (V4R-167): a bare non-key column every reader reads only under a key expression rides a `min` cell
    let mut keyed: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    keyed.extend(sh.keys.iter().map(|key| key.resno));
    let tlist = crate::facts::demand::readers::rewrite(b, a, &keyed, &above, tlist, qual);
    let tlist = unread_junk(b, &a.plan, &keyed, tlist, qual, k, id)?;
    let tlist = const_cols(b, &child, &keyed, tlist, k, id)?;
    let tlist = unread_cols(b, &a.plan, &keyed, tlist, k, id)?;
    let refs = collect(b.arena, &[tlist, qual], k, id)?;
    let plain = a.aggstrategy == AGG_PLAIN || a.numCols <= 0;
    if plain && a.aggstrategy != AGG_PLAIN {
        // every key redundant (pinned by a filter): the plain fold, no row over an empty input, pinned keys from the first row (nodeAgg)
        sh.empty_no_row = true;
        sh.pinned = pinned_cols(b.arena, tlist, qual);
    }
    // D345 / D350 (V4R-144): every pinned column one image in every row (`pinned_word`): they key the node's one group,
    // a sorted group (runs.group) whose record carries the first row's words, and no group over an empty input; beside a
    // DISTINCT aggregate's key they key the grouped distinct form (keyed.distinct_grouped: its one group, D345 M6-FOLD),
    // so they are keyed before the calls (the DISTINCT argument's key is the last, keyed/pairs.rs)
    // D345 (2): beside a DISTINCT call a pinned column only the resjunk entries read (`SELECT count(DISTINCT u) ...
    // WHERE k = 0 GROUP BY k`) keys the group too, so it is the grouped distinct form's one group; (3b) beside an
    // aggregate PostgreSQL presorts too (the pinned key it determines a column by may be resjunk: `SELECT o.total,
    // array_agg(x ORDER BY x) .. WHERE o.id = 7 GROUP BY o.id`)
    // D345 (3): beside a DISTINCT call or a presorted aggregate a column the pinned key determines (`keys::pins_key`)
    // keys the group too
    let fd = refs.iter().any(|r| r.as_aggref().is_some_and(|x| !x.aggdistinct.is_nil() || x.aggpresorted));
    let junk = sh.empty_no_row && fd;
    let pins = pinned_vars(b.arena, &[&a.plan.targetlist, &a.plan.qual], junk);
    let pinned_keys = sh.empty_no_row
        && !pins.is_empty()
        && (junk || !sh.pinned.is_empty())
        && !refs.iter().any(|&r| expr::has_subplan(r))
        && (pins_key(b, &child, &pins, fd, None) || (fd && pair_distinct(b, &child, &refs, k, id) && pins_key(b, &child, &pins, fd, Some(&a.plan.targetlist))));
    if pinned_keys {
        fires::fire("agg.pinned_word"); // shapecheck fire signal (REGISTRY AR-13): pinned columns key the node's one group
        for (c, coll) in pins.iter().copied() {
            sh.key(b, &child, c, coll, k, id)?;
        }
    }
    let pinned_keys = pinned_keys || rekeyed;
    let mut distinct_key = NONE;
    sh.held = held_whole(b, a);
    sh.row_form = a.aggstrategy == AGG_PLAIN && a.numCols == 0 && !sh.empty_no_row && row_counts(&child, &refs[..]);
    for r in refs.iter().copied() {
        if expr::has_subplan(r) {
            child = crate::ops::subplan::attach_agg_arg(b, r, k, id, child)?;
        }
        cell(b, &child, r, &mut sh, &mut distinct_key, k, id)?;
    }
    if a.aggstrategy == AGG_PLAIN && !sh.empty_no_row && distinct_key != NONE {
        multi_counts(b, &child, &mut sh, distinct_key, k, id)?; // PSP2-34 (group's keyed.distinct_multi)
    }
    let form = match (distinct_key != NONE, plain, a.aggstrategy) {
        (true, _, _) => GroupForm::Distinct,
        (false, true, _) if pinned_keys => GroupForm::Sorted,
        (false, true, _) => GroupForm::Plain,
        (false, false, AGG_SORTED) => GroupForm::Sorted,
        _ => GroupForm::Hashed,
    };
    // D3728: a keyless node's bare resjunk column its record does not carry (no key, not kept) and no node above reads by
    // value reads a typed NULL
    let tlist = match sh.empty_no_row {
        true => unkept_junk(b, &a.plan, &|c| sh.keys.iter().any(|key| key.resno == c) || (form == GroupForm::Plain && sh.pinned.contains(&c)), tlist, k, id)?,
        false => tlist,
    };
    // D1292: a key PG's hash raises on serves only in the plain hashed form (hash_raise.rs wraps its end)
    if sh.keys.iter().any(|key| crate::effects::agg::hash_raises(b.facts, key.tf, key.coll)) && (form != GroupForm::Hashed || hints.iter().any(Option::is_some)) {
        return Err(indeterminate_coll(k, id));
    }
    let mut order = ORDER_NONE;
    let mut keeps = keeps;
    if form == GroupForm::Sorted || (form == GroupForm::Distinct && !plain && a.aggstrategy == AGG_SORTED) {
        // pinned keys: numCols 0, the one group is in any order
        match &absorbed {
            Some(s) => s.direct(&mut sh.keys[..a.numCols.max(0) as usize]),
            None if crate::facts::stream::family_order(b, &child, &sh.keys[..a.numCols.max(0) as usize]) => keeps = true, // D2313
            None => {
                let wide = (ncols > crate::graph::MAX_ORDER_KEYS).then(|| crate::ops::group::absorb::wide_sort(b, &a.plan, &a.grpColIdx[..ncols], a.grpCollations)).flatten();
                ordered(b, &child, &mut sh.keys[..a.numCols.max(0) as usize], k, id, wide)?
            }
        }
        order = if form == GroupForm::Sorted { key_order(b) } else { ORDER_KEY };
    }
    // D3374: a group PostgreSQL reads whole when the statement is read to its end (a writing statement's source too),
    // under no reader that may stop: a money or interval sum there may add its rows in Gravity's tie order (lowerv4
    // hosted.rs `own_order`)
    if form != GroupForm::Hashed && !b.graph.partial && crate::facts::stream::whole_written(b, id) {
        b.graph.whole.push(id);
    }
    // D1071: a sorted group whose aggregate argument or FILTER calls something VOLATILE (D1069 keeps its Sort) streams,
    // one group at a time, so a consumer that stops (a LIMIT above) stops the arguments where PostgreSQL's does. Every
    // input form: a Sort's or a Merge Append's emit, and an ordered Append's union (append.rs copies GroupRun into each
    // arm, the open group one per union). A DISTINCT aggregate's group streams too; its per-group set is M6's
    // (runs.group_distinct refuses it by name until then, D1120)
    if keeps && matches!(form, GroupForm::Sorted | GroupForm::Distinct) {
        if crate::facts::stream::streams_to_stop(b, a) {
            crate::facts::stream::ask_spine(b, &a.plan); // the stop road: a merge's inner pulled as PostgreSQL fetches it
        }
        if crate::facts::stream::in_order(b, a) {
            b.graph.in_order.push(id); // D2846: its raising folds in plan order (facts/order.rs in_order_scope)
        }
        let mut emit = stream(b, child, id, k, hints, sh, form, a.numGroups.max(0) as u64)?;
        crate::ops::filters(&mut emit, qual);
        crate::ops::project(&mut emit, tlist);
        b.graph.note(k);
        return Ok(emit);
    }
    if sh.keys.iter().any(|key| key.user_eq != 0) && !(form == GroupForm::Hashed && crate::facts::user_hash::keys(b.facts, &sh.keys, &hints)) {
        return Err(Refusal::shape(k, id, USER_GROUP_EQ)); // D2719: only the streamed form calls the user `=`; hj-0310: the hashed one keys its user key
    }
    let mut topk = false;
    let mut two = None;
    match hints[1] {
        Some(Hint::ZoneTopK { attno, desc, nulls_first }) => {
            if form != GroupForm::Hashed || sh.keys.first().map_or(true, |key| key.attno != attno) {
                return Err(Refusal::shape(k, id, "the ZoneTopK key is not the first key of a hashed group"));
            }
            // GP-1b: a descending walk is the planner's under a DESC first sort key (the Sort above orders the groups);
            // its key is NOT NULL, so the hint never says NULLS FIRST
            if nulls_first {
                return Err(Refusal::shape(k, id, "a nulls-first ZoneTopK walk"));
            }
            if desc {
                fires::fire("agg.zonetopk.desc"); // shapecheck fire signal (GP-1b: the descending group walk's hint taken)
            }
            order = ORDER_KEY;
            topk = true;
            fires::fire("agg.zonetopk"); // shapecheck fire signal (REGISTRY: ZoneTopK hint taken)
        }
        Some(Hint::TwoPass { attno, floor_desc, floor_nulls_first }) => {
            if form != GroupForm::Hashed {
                return Err(Refusal::shape(k, id, "TwoPass on an aggregate that is not a hashed group"));
            }
            let key = sh.keys.iter().position(|key| key.attno == attno).ok_or(Refusal::shape(k, id, "the TwoPass key is not a group key"))?;
            let ext = top_extreme(b, &a.plan);
            // an extreme over a coded lane folds codes (`MaxCode`), not the int word pass 1's rule ranks: the hint is
            // left (the plain Agg under it is a valid plan), one pass. D3573: so is a scan that acts (a VOLATILE qual
            // or column, a catalog miss): pass 1 re-reads it with its own copy of the WHERE, so nextval would run twice
            // per row (PostgreSQL runs it once) and pass 2 would filter on the advanced sequence
            let scan_acts = a.plan.lefttree.is_none_or(|n| crate::effects::of_plan(&cx, n).acts);
            // PGRCSERVE G4: nor over the rows road (pass 1 would be a second walk of the AM's scan): one pass
            let rows = matches!(&child.source, Source::Range { site, .. } | Source::Sample { site, .. } if site.rows_via_am);
            if !scan_acts && !rows && !ext.is_some_and(|e| extreme_coded(b, &child, e)) {
                let (p1, slot1) = pass1(b, &a.plan, &child, &sh.keys[key], ext, floor_desc, a.numGroups.max(0) as u64, k, id)?;
                child.desc = crate::breakers::consuming(b, child.desc, slot1);
                two = Some(TwoPassDecl { pass1: p1, slot: slot1, key: key as u16, floor_desc, floor_nulls_first });
                fires::fire("agg.twopass"); // shapecheck fire signal (REGISTRY DC-13)
            }
        }
        Some(Hint::ZoneWalk { .. }) => return Err(Refusal::shape(k, id, "a ZoneWalk hint on an aggregate")),
        _ => {}
    }
    if form == GroupForm::Distinct && !plain {
        multi_grouped(b, &child, &mut sh, distinct_key, k, id)?;
    }
    let groups = match form {
        // DC-21: a plain node's numGroups past 1 is the estimate normalize's distinct_count carries from the grouping it
        // replaced (PostgreSQL's plain Agg says 1): the distinct rows the table will hold
        GroupForm::Distinct if a.aggstrategy == AGG_PLAIN && a.numGroups > 1 => a.numGroups as u64,
        GroupForm::Distinct => child.desc.predicted_rows.max(a.numGroups.max(0) as u64),
        _ => a.numGroups.max(0) as u64,
    };
    let (mut emit, _) = close(b, child, id, k, form, hints, sh, order, topk, groups, a.numGroups.max(0) as u64, two)?;
    if let Some(g) = b.graph.groups.last_mut().filter(|_| !a.gravity_finals.is_empty()) {
        g.finals = Some((core::ptr::from_ref(&a.plan.targetlist) as usize, a.gravity_finals)); // gravity §3 #8: the planner's mark, read by finals.cse
    }
    crate::ops::filters(&mut emit, qual);
    crate::ops::project(&mut emit, tlist);
    b.graph.note(k);
    // D530 (b): a grouped DISTINCT's owner emits its groups in its partitions' order; the sorted GroupAggregate's key
    // order the plan reads (no Sort above) comes from a key Sort over the emit
    if form == GroupForm::Distinct && !plain && a.aggstrategy == AGG_SORTED && !sorted_above(b.stmt.planTree, id) {
        return key_sort(b, a, emit, tlist, id);
    }
    Ok(emit)
}

/// D530 (b): the order a sorted GroupAggregate's input carried (its Sort's, an incremental Sort's or a Gather Merge's
/// keys), over the group's output: the leading input keys are the group keys, each a column of the target list, in the
/// input's operator, collation and NULL placement, so the groups come out as the GroupAggregate's did. The Sort's own
/// list reads each output column, as a planner Sort's does. An input of another order (an index, a merge join), a group
/// key the target list does not carry, or a leading input key that is not a group key: the emit as it is (keyed/pairs.rs
/// `order_read` refuses the group by name).
fn key_sort<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, emit: Open<'p>, tlist: &'p NodeList<'p>, id: i32) -> Result<Open<'p>, Refusal> {
    use crate::ops::shared::sort::{SortDesc, SortForm, SortKey};
    let k = Kind::Sort;
    let Some(alloc) = b.alloc else { return Ok(emit) };
    let input = a.plan.lefttree;
    let spec = input.and_then(|n| n.as_sort().or_else(|| n.as_incremental_sort().map(|i| &i.sort)).map(|s| (s.numCols, s.sortColIdx, s.sortOperators, s.collations, s.nullsFirst)));
    let spec = spec.or_else(|| input.and_then(|n| n.as_gather_merge()).map(|g| (g.numCols, g.sortColIdx, g.sortOperators, g.collations, g.nullsFirst)));
    let Some((ncols, cols, ops, colls, nulls)) = spec else { return Ok(emit) };
    let grp = &a.grpColIdx[..(a.numCols.max(0) as usize).min(a.grpColIdx.len())];
    let n = (ncols.max(0) as usize).min(cols.len()).min(ops.len()).min(nulls.len());
    if grp.is_empty() || n < grp.len() {
        return Ok(emit);
    }
    let oom = |_| Refusal::shape(k, id, "the grouped DISTINCT key Sort: out of memory synthesizing its target list");
    let mut keys = ArenaVec::new_in(b.arena);
    for i in 0..grp.len() {
        let c = cols[i];
        let is_key = |n: &Node<'p>| n.as_target_entry().and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == OUTER_VAR && v.varattno == c && v.varlevelsup == 0);
        let (true, Some(j)) = (grp.contains(&c), tlist.iter().position(|n| is_key(&n))) else { return Ok(emit) };
        let te = tlist.nth(j).as_target_entry().expect("a key entry");
        let typid = crate::catalog::expr_type::type_of(te.expr).ok_or(Refusal::shape(k, id, "a group key of unknown type"))?;
        let desc = b.facts.key(crate::catalog::KeyReq::sort(ops[i])).and_then(crate::catalog::KeyOrder::dir).ok_or(Refusal::shape(k, id, "a sort operator outside the direction table"))?;
        keys.push(SortKey::of_type(b.facts, (j + 1) as i16, ops[i], desc, nulls[i], typid, crate::ops::shared::sort::key_coll(b.facts, typid, colls.get(i).copied().unwrap_or(0), k, id)?));
    }
    let mut out_cols: PgVec<'_, Node<'p>> = mcx::reserved_in(tlist.len(), b.mcx);
    for (j, n) in tlist.iter().enumerate() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let typid = crate::catalog::expr_type::type_of(te.expr).ok_or(Refusal::shape(k, id, "an output column of unknown type"))?;
        let var = Node::mk_var(alloc, OUTER_VAR, (j + 1) as i16, typid, -1, crate::catalog::expr_type::collation_of(te.expr), 0).map_err(oom)?;
        out_cols.push_in(Node::mk_target_entry(alloc, var, (j + 1) as i16, te.resname, te.resjunk).map_err(oom)?);
    }
    let list: &'p NodeList<'p> = NodeList::from_slice(alloc, &out_cols[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    let layout = crate::ops::shared::sort::keys_layout(b.arena, keys.iter().map(|x| x.kind));
    let mut open = emit;
    open.push(Op::Sort(SortDesc { keys, presorted: 0, form: SortForm::Runs, tlist: list }));
    let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Sort, layout, Budget::Share, Spill::Runs, k, id)?;
    let mut out = crate::ops::shared::breaker::emit(b, id, slot);
    crate::ops::project(&mut out, list);
    Ok(out)
}

/// D530 (b): a full Sort re-orders node `id`'s rows before anything reads their order: the nearest plan node above it on
/// the tree's outer spine, past subquery scans and projections (Result), is a Sort (an incremental Sort reads the order
/// below it). Not on the outer spine: false (the key Sort is then only redundant).
fn sorted_above(root: Option<Node<'_>>, id: i32) -> bool {
    let mut n = root;
    let mut sort = false;
    while let Some(node) = n {
        let Some(plan) = node.as_plan() else { return false };
        if plan.plan_node_id == id {
            return sort;
        }
        if node.as_sort().is_some() {
            sort = true;
        } else if node.as_subquery_scan().is_none() && node.as_result().is_none() {
            sort = false;
        }
        n = node.as_subquery_scan().map_or(plan.lefttree, |sq| sq.subplan);
    }
    false
}

/// The order a sorted group's emit declares (`close`): `ORDER_KEY`, or, D3644, `ORDER_NONE` for a group whose own rows
/// may come in any order (`Builder::order_free`: facts/order.rs `order_free_under`, e.g. under a plain aggregate whose
/// every call folds the same in any row order). Its records sit in a keyed table either way (the key Sort absorbed,
/// absorb.rs, or kept as a pipe); no reader of such a group sees the order its emit hands the groups in, so the emit
/// hands them unsorted (lowerv4 table/emit_group.rs sorts only a group that declares its key order), where it sorted
/// every group in memory first (kernels agg_sort: 40,000,000 groups under count(*) took 2.3 GB outside the grant and
/// 6 s of the statement's 19 s).
fn key_order(b: &Builder<'_, '_>) -> u8 {
    if b.order_free {
        fires::fire("group.order_free"); // shapecheck fire signal (D3644): a sorted group's emit declares no order
        return ORDER_NONE;
    }
    ORDER_KEY
}

/// Closes `producer` into the group's breaker (its owner phase and words declared) and opens the emit
/// pipeline over it; the `GroupDecl` is the record contract 2d reads (decompose.md §2.6).
/// `user_eq` (KeySem slice 4, `map`): a key under a user `=` over user code or another built-in, which only the first-of-run
/// op compares through (lowerv4 per_group.rs `other_eq`): the runs stay there or the node refuses USER_GROUP_EQ.
#[allow(clippy::too_many_arguments)]
fn map_set<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, k: Kind, form: GroupForm, cols: &[i16], colls: &[u32], eqops: &[u32], user_eq: bool) -> Result<Open<'p>, Refusal> {
    let id = plan.plan_node_id;
    // D612: the key Sort a key set orders by itself (absorb.rs); DISTINCT ON's first-of-run reads the order, so keeps it
    let first_of_run = form == GroupForm::Unique && pinned_vars(b.arena, &[&plan.targetlist, &plan.qual], true).iter().any(|&(c, _)| !cols.contains(&c));
    // the stop road: right under a counted LIMIT over an ordered feed the runs stream through the first-of-run op too
    let stops = crate::facts::stream::limit_streams(b, plan);
    // D4676 (road group.hashchain): under a group that hashes these rows and orders only its own groups (absorb.rs
    // `chain_of`, `Builder::chain`), the Sort is absorbed whatever its key types and the rows go up unordered
    let chained = k == Kind::Unique && b.chain == Some(id) && !first_of_run && !stops && !user_eq;
    let absorbed = crate::ops::group::absorb::key_sort(b, plan, cols, colls).filter(|_| !first_of_run && !stops && !user_eq);
    b.absorb = absorbed.as_ref().map(|s| s.id);
    b.chain = absorbed.as_ref().and_then(|s| s.chain); // a Group over a Unique over its Sort: that Unique's (D4676)
    let child = b.child(plan, k);
    b.absorb = None;
    b.chain = None;
    let mut child = child?;
    let mut sh = Shape::new(b.arena);
    for (i, &c) in cols.iter().enumerate() {
        sh.key(b, &child, c, colls.get(i).copied().unwrap_or(0), k, id)?;
    }
    for l in [&plan.targetlist, &plan.qual] {
        for n in l.iter() {
            if has_aggref(n) {
                return Err(Refusal::shape(k, id, "an aggregate on a Group / Unique node"));
            }
        }
    }
    if sh.keys.is_empty() {
        child.push(Op::Limit { count: crate::graph::Bound::Const(1), offset: crate::graph::Bound::All, ties: None });
        crate::ops::filters(&mut child, &plan.qual);
        crate::ops::project(&mut child, &plan.targetlist);
        b.graph.note(k);
        return Ok(child);
    }
    // USEROP: over another family's order (D2313's test) the runs stream (`first_of_run`'s `family`), as nodeUnique /
    // nodeGroup compare: the breaker's emit would sort them by their images (group lane: a Group, `GROUP BY x ORDER BY x
    // USING <#` with no aggregate, emits each run's first row as a Unique does, its HAVING after)
    let family = matches!(form, GroupForm::Unique | GroupForm::Group) && absorbed.is_none() && cols.len() == sh.keys.len() && crate::facts::stream::family_order(b, &child, &sh.keys);
    match &absorbed {
        Some(s) => s.direct(&mut sh.keys),
        None if family => {}
        None => {
            let wide = (sh.keys.len() > crate::graph::MAX_ORDER_KEYS).then(|| crate::ops::group::absorb::wide_sort(b, plan, cols, colls)).flatten();
            ordered(b, &child, &mut sh.keys, k, id, wide)?
        }
    }
    let fd = k == Kind::Group && query_grouping(&plan.targetlist);
    let mut keyed: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    keyed.extend(sh.keys.iter().map(|key| key.resno));
    let lost = dropped::dropped(b, &plan.targetlist, &plan.qual, &keyed, fd, false);
    sh.duplicate_keys(b, &child, &[&plan.targetlist, &plan.qual], fd, &lost.covered, k, id)?;
    if k == Kind::Group {
        // EXPR-ODDS B (main 09-28): a Group's column outside its keys that the keys determine by its relation's primary
        // key (a GROUP BY column remove_useless_groupby_columns dropped as the key's): a trivial SubqueryScan's removal
        // relabels the Group's target list with the scan's ressortgroupref 0 (setrefs.c clean_up_removed_plan_level), so
        // `fd` above is off; PostgreSQL's check_functional_grouping admits the primary key alone
        sh.pk_keys(b, &child, &[&plan.targetlist, &plan.qual], (cols, eqops, colls), k, id)?;
    }
    let (tlist, qual) = dropped::rekey(b, &mut child, &mut sh, &lost, &plan.targetlist, &plan.qual, None, k, id)?;
    let distinct_on = form == GroupForm::Unique && pinned_vars(b.arena, &[&plan.targetlist, &plan.qual], true).iter().any(|&(c, _)| !sh.keys.iter().any(|key| key.resno == c));
    if distinct_on || stops || family {
        // V4R-147 DISTINCT ON: an output column outside the keys is the run's first row's, so the run's rows stream
        // through a first-of-run op over the ordered input (no record, no breaker); under the stop road's LIMIT an input
        // the op cannot walk keeps the breaker form
        match crate::ops::first_of_run(b, &child, (cols, eqops, colls), family, k, id) {
            Ok(tie) => {
                if stops {
                    crate::facts::stream::ask_spine(b, plan); // the stop road: a merge's inner pulled as PostgreSQL fetches it
                }
                child.push(Op::FirstOfRun(tie));
                crate::ops::filters(&mut child, qual);
                crate::ops::project(&mut child, tlist);
                b.graph.note(k);
                return Ok(child);
            }
            Err(e) if distinct_on || family || user_eq => return Err(e),
            Err(_) => {}
        }
    }
    if user_eq {
        return Err(Refusal::shape(k, id, USER_GROUP_EQ)); // KeySem slice 4: only the first-of-run op calls the user `=`
    }
    let order = if chained { ORDER_NONE } else { key_order(b) };
    let (mut emit, _) = close(b, child, id, k, form, [None; 2], sh, order, false, Builder::rows(plan), Builder::rows(plan), None)?;
    crate::ops::filters(&mut emit, qual);
    crate::ops::project(&mut emit, tlist);
    b.graph.note(k);
    Ok(emit)
}

fn close<'p>(b: &mut Builder<'p, '_>, producer: Open<'p>, id: i32, k: Kind, form: GroupForm, hints: [Option<Hint>; 2], mut sh: Shape<'p>, order: u8, topk: bool, groups: u64, numgroups: u64, two: Option<TwoPassDecl>) -> Result<(Open<'p>, u8), Refusal> {
    sh.reps(k, id);
    sh.settle_keyed();
    let layout = sh.layout(b.arena);
    let bits = spill_bits(groups, layout.size());
    if groups != 0 && form != GroupForm::Plain {
        fires::fire("agg.spill_sized"); // shapecheck fire signal (REGISTRY DC-05): fan-out sized from the group estimate
    }
    // a plain breaker's region is the one-record table abi's record host lays out and PlainAggPartial reads: the header
    // and the record, never the record alone (a worker's partial shorter than the header was dropped at W >= 2: D271)
    let plain = HashAgg::bytes(&layout.record(b.arena), 1);
    let top = if form == GroupForm::Distinct { top_share(b, &producer, &sh.keys) } else { 0 };
    let (kind, budget, spill) = match form {
        GroupForm::Plain => (BreakerKind::PlainAgg, Budget::Fixed(plain), Spill::None),
        GroupForm::Hashed | GroupForm::Sorted | GroupForm::Pass1 => (BreakerKind::HashAgg, Budget::Share, Spill::Partitioned { bits }),
        GroupForm::Distinct | GroupForm::Group | GroupForm::Unique => (BreakerKind::Distinct, Budget::Share, Spill::Partitioned { bits }),
    };
    let mut producer = crate::ops::shared::union::resolve(&mut b.graph, producer);
    if matches!(form, GroupForm::Plain | GroupForm::Hashed) {
        unclaimed_sorted(b, &mut producer);
    }
    producer.desc.end_rate = end_rate(b, &producer, id); // NEW-48: the end's carry decides on it (lowerv4)
    let g = &mut b.graph;
    if g.pipelines.len() + 2 > limits::PIPELINES {
        return Err(Refusal::shape(k, id, "more than 65535 pipelines (the u16 pipeline id)"));
    }
    let slot = breakers::free_slot(g, &producer).ok_or(Refusal::shape(k, id, "more than 256 live breaker slots (the u8 breaker slot id, limits::BREAKERS)"))?;
    let breaker = Breaker { kind, slot, bytes: if kind == BreakerKind::PlainAgg { plain } else { 0 }, budget, spill };
    let ends = breakers::declare_producers(g, producer, breaker, layout, emit_order(&sh.keys, order));
    if kind != BreakerKind::PlainAgg {
        let words = if kind == BreakerKind::HashAgg { WordsDesc { topk: if topk { W_TOPK } else { WordsDesc::NONE }, dense_lo: W_DENSE_LO, dense_hi: W_DENSE_HI, bits: W_BITS, order: W_ORDER, nparams: NWORDS } } else { WordsDesc::bits_only(W_BITS, NWORDS) };
        for &p in ends.iter() {
            let p = p as usize;
            g.pipelines[p].desc = g.pipelines[p].desc.owner(OwnerDesc::generic(layout.record(b.arena), order, 0));
            g.pipelines[p].desc = g.pipelines[p].desc.words(words);
        }
    }
    let producer = LifeId(*ends.last().expect("a push ends at least one pipeline"));
    g.groups.push(GroupDecl { slot, producer, node_id: id, form, hints, layout, keys: sh.keys, aggs: sh.aggs, two_pass: two, groups: numgroups, top, set: None, empty_no_row: sh.empty_no_row, pinned: sh.pinned, finals: None });
    let emit = crate::breakers::consuming(b, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), slot);
    Ok((Open::new(b.arena, id, Source::Breaker { slot }, emit), slot))
}

/// D3784: the largest group's share of `child`'s rows, in 2^-16 (0: unknown), for a grouped count(DISTINCT)'s pair route:
/// the smallest largest-value share of the rows over the group's key columns with statistics (`Facts::att_top_share`, on
/// the planner's estimate of the rows; a group's rows are at most each of its key values'; an expression key reads none).
/// The DISTINCT argument's keys (resno 0) are not the group's. An over-estimate costs a second round (route.rs
/// `group_routed_skewed`), an under-estimate is the even-spread rule's answer.
fn top_share<'p>(b: &Builder<'p, '_>, child: &Open<'p>, keys: &[KeyDecl<'p>]) -> u32 {
    let rows = child.desc.predicted_rows;
    let least = keys.iter().filter(|k| k.resno != 0).filter_map(|k| {
        let (relid, a) = crate::facts::pinned::scan_column(b, child, k.expr.node)?;
        b.facts.att_top_share(relid, a, rows)
    });
    least.reduce(f64::min).map_or(0, |f| ((f.clamp(0.0, 1.0) * 65536.0) as u32).max(1))
}

/// D3002: a sorted pgrc scan is a serial source only for its order's sake (scan/mod.rs, decision 81). A plain or hashed
/// aggregate reads no input order, so where the scan's pipeline holds only filters, gates and projections (no limit, run,
/// join, window or other step that reads the order) it claims morsels as an unsorted pgrc scan does; its zone pruning
/// on the sort key is the site's and stays. Every consumer that reads the order marks its input serial itself (a
/// presorted group's stream, a merge join, a streamed set operation) or never reaches here.
fn unclaimed_sorted(b: &Builder<'_, '_>, p: &mut Open<'_>) {
    let only_sorted = matches!(p.desc.parallel, gravity_program::Parallel::Ineligible { reason: Reason::SortedSource, .. });
    let quiet = p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    if only_sorted && quiet && p.union.is_none() && crate::ops::is_sorted_scan(b, p) {
        p.desc.parallel = gravity_program::Parallel::Eligible;
        // a merge nothing reads the order of is a plain scan (sortmerge)
        if let Source::Range { site, .. } = &mut p.source {
            site.merge = gravity_program::MergeAttnos::NONE;
        }
    }
}

/// NEW-48 (`Pipeline::end_rate`): the planner's rows reaching `producer`'s end, Agg `id`, per iteration of its innermost
/// loop: the rows of the pipeline's topmost plan node below the Agg over the most iterations a loop of the pipeline makes
/// (a nested loop: its outer rows times its inner's walk per loop, PostgreSQL's node rows being per loop: a heap scan inner
/// walks its block path, one iteration per 64-row block, any other inner (a Materialize, an index scan, a Memoize) one per
/// row; a hash join: its probe rows; a merge join: both sides' rows). `END_RATE_NONE` when no join node is the pipeline's
/// (its source's block path iterates per 64-row block). Estimates under a Gather are per worker on both sides of the ratio.
fn end_rate(b: &Builder<'_, '_>, producer: &Open<'_>, id: i32) -> u64 {
    end_rate_over(b.stmt, producer.node_ops.iter().map(|o| o.node_id).filter(|&n| n != id))
}

/// `end_rate` over plan nodes `ids` of `stmt` (a pipeline's node ops but its Agg's), read off the plan alone: D3104's front
/// hit decides a filed program's end-rate guards on this statement's plan with it, as decompose would.
pub fn end_rate_over(stmt: &PlannedStmt<'_>, ids: impl Iterator<Item = i32>) -> u64 {
    fn find<'p>(at: Node<'p>, id: i32) -> Option<Node<'p>> {
        crate::walk::stack_check(); // D3748
        let p = at.as_plan()?;
        if p.plan_node_id == id {
            return Some(at);
        }
        [p.lefttree, p.righttree].into_iter().flatten().find_map(|k| find(k, id))
    }
    let rows = |n: Option<Node<'_>>| n.and_then(|n| n.as_plan()).map_or(1.0, |p| crate::admit::plan_rows(p).max(1.0));
    let (mut top, mut most) = (None, 0.0f64);
    for node_id in ids {
        let Some(n) = stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).find_map(|r| find(r, node_id)) else { continue };
        let Some(p) = n.as_plan().filter(|_| n.as_gather().is_none() && n.as_gather_merge().is_none()) else { continue };
        top = Some(crate::admit::plan_rows(p).max(0.0)); // source first: the last one met is the topmost (a Gather's rows are all workers')
        let loops = if n.as_nest_loop().is_some() {
            let walk = if p.righttree.is_some_and(|i| i.as_seq_scan().is_some()) { (rows(p.righttree) / 64.0).max(1.0) } else { rows(p.righttree) };
            rows(p.lefttree) * walk
        } else if n.as_hash_join().is_some() {
            rows(p.lefttree)
        } else if n.as_merge_join().is_some() {
            rows(p.lefttree) + rows(p.righttree)
        } else {
            0.0
        };
        most = most.max(loops);
    }
    match top {
        Some(r) if most > 0.0 => (r / most * gravity_program::END_RATE_ONE as f64).min((gravity_program::END_RATE_NONE - 1) as f64) as u64,
        _ => gravity_program::END_RATE_NONE,
    }
}

/// D1071: sorted group `sh` streamed over `child`'s ordered rows (`Op::GroupRun`): its declaration in `streams`, with the
/// record layout the keyed form's is (the step keeps one open record); `producer` is set when the pipeline is pushed.
fn stream<'p>(b: &mut Builder<'p, '_>, mut child: Open<'p>, id: i32, k: Kind, hints: [Option<Hint>; 2], mut sh: Shape<'p>, form: GroupForm, numgroups: u64) -> Result<Open<'p>, Refusal> {
    sh.reps(k, id);
    sh.settle_keyed();
    let layout = sh.layout(b.arena);
    if child.desc.is_eligible() {
        // the group reads its input's order: one reader, as a sorted source's (an index's order is serial already)
        let site = child.desc.driving_site.max(0);
        child.desc = Pipeline { parallel: gravity_program::Parallel::Ineligible { site, reason: Reason::SortedSource }, ..child.desc };
    }
    let i = b.graph.streams.len() as u16;
    b.graph.streams.push(GroupDecl { slot: u8::MAX, producer: LifeId(u16::MAX), node_id: id, form, hints, layout, keys: sh.keys, aggs: sh.aggs, two_pass: None, groups: numgroups, top: 0, set: None, empty_no_row: sh.empty_no_row, pinned: sh.pinned, finals: None });
    child.push(Op::GroupRun(i));
    Ok(child)
}

/// The aggregate a two-pass top orders its groups by first (the planner's `TopOrder::Extreme`): the Agg output entry the
/// Sort right above reads first (facts::demand `first_sort_key`), a built-in `max` / `min` / integer `avg` (gtopn GT-1:
/// either direction; pass 1 bounds each by its column's extremes per key); `None` for a count top or no Sort right above.
fn top_extreme<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>) -> Option<&'p Aggref<'p>> {
    let col = crate::facts::demand::first_sort_key(b, plan)?;
    let out = plan.targetlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == col)?;
    let a = relabels(out.expr).as_aggref()?;
    matches!(aggtab::row(a.aggfnoid).map(|r| r.class), Some(Class::Min(_) | Class::Max(_) | Class::AvgI128)).then_some(a)
}

/// The built-in `max` (`max`) or `min` over one argument of type `typ` (its `pg_aggregate` row), for pass 1's bounds.
fn extreme_fnoid(max: bool, typ: u32) -> Option<u32> {
    let name = if max { "max" } else { "min" };
    aggtab::AGG_TABLE.iter().find(|r| r.name == name && r.args == [typ]).map(|r| r.fnoid)
}

/// Whether extreme `a`'s argument reads a coded lane of `child`'s scan (`cell` folds it in a code register).
fn extreme_coded<'p>(b: &Builder<'p, '_>, child: &Open<'p>, a: &'p Aggref<'p>) -> bool {
    let arg = a.args.iter().next().map(|n| n.as_target_entry().map_or(n, |t| t.expr));
    arg.and_then(|e| scan_lane(b, child, strip(child, e))).is_some_and(|l| matches!(l.rep, gravity_program::Rep::Code { .. }))
}

/// `fnoid` (the extreme's, or `count(any)`'s) over pass 1's projected entry 2 (`v`, the extreme's scan column).
fn pass1_agg<'p>(b: &Builder<'p, '_>, like: &'p Aggref<'p>, fnoid: u32, aggtype: u32, v: &types_nodes::primnodes::Var, k: Kind, id: i32) -> Result<Node<'p>, Refusal> {
    let oom = || Refusal::shape(k, id, "TwoPass: out of memory synthesizing the first pass's extreme");
    let alloc = b.alloc.ok_or(Refusal::shape(k, id, "TwoPass: no node allocator (decompose_in)"))?;
    let var = Node::mk_var(alloc, OUTER_VAR, 2, v.vartype, v.vartypmod, v.varcollid, 0).map_err(|_| oom())?;
    let te = Node::mk_target_entry(alloc, var, 1, None, false).map_err(|_| oom())?;
    let args = NodeList::from_slice(alloc, &[te]).map_err(|_| oom())?;
    let mut aggargtypes = types_nodes::list::OidList::nil();
    aggargtypes.lappend(alloc, v.vartype).map_err(|_| oom())?;
    let a = Aggref { aggfnoid: fnoid, aggtype, aggcollid: like.aggcollid, inputcollid: like.inputcollid, aggtranstype: aggtype, aggargtypes, args, aggkind: like.aggkind, aggsplit: like.aggsplit, ..Aggref::default() };
    Node::mk(alloc, a).map_err(|_| oom())
}

/// The two-pass hint's pass 1: the scan's quals over the key lane into a count table on the key. Under an extreme top
/// (`ext`) it also folds the extreme of its column per key and, when the column can be NULL, the column's non-NULL rows
/// (`count(v)`): slots `[count(*), extreme, count(v)]`, the record lowerv4 twopass.rs `Rule::ext` reads.
#[allow(clippy::too_many_arguments)]
fn pass1<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, child: &Open<'p>, key: &KeyDecl<'p>, ext: Option<&'p Aggref<'p>>, desc: bool, groups: u64, k: Kind, id: i32) -> Result<(u16, u8), Refusal> {
    let ss: &'p SeqScan<'p> = plan.lefttree.and_then(|n| n.as_seq_scan()).ok_or(Refusal::shape(k, id, "TwoPass over an input that is not a SeqScan"))?;
    let Source::Range { am, scanrelid, relid, .. } = child.source else { return Err(Refusal::shape(k, id, "TwoPass over an input that is not a range source")) };
    if key.attno <= 0 {
        return Err(Refusal::shape(k, id, "the TwoPass key is not a scan column"));
    }
    let varno = scanrelid as i32;
    // the extreme's argument as the scan reads it (a column of this scan: the planner's TopOrder::Extreme), and whether it
    // is never NULL there
    let ext = match ext {
        Some(a) => {
            let arg = a.args.iter().next().map(|n| n.as_target_entry().map_or(n, |t| t.expr)).ok_or(Refusal::shape(k, id, "a two-pass extreme with no argument"))?;
            let s = strip(child, arg);
            let v = s.node.as_var().filter(|v| v.varno == varno && v.varattno >= 1 && v.varlevelsup == 0).ok_or(Refusal::shape(k, id, "a two-pass extreme over no scan column"))?;
            Some((a, s.node, v, notnull(b, child, s)))
        }
        None => None,
    };
    let mut atts = Atts::new(b.arena);
    atts.add(key.attno);
    if let Some((_, _, v, _)) = ext {
        atts.add(v.varattno);
    }
    sources::vars_of(&[&ss.scan.plan.qual], varno, k, id, &mut atts)?;
    let mut site = SiteDesc::range_source(b.arena, relid, atts.as_slice());
    sources::lanes(b, &mut site, relid, am, atts.as_slice(), k, id)?;
    if am == crate::catalog::Am::Pgrcolumnar {
        sources::zone(b, &mut site, varno, &atts, &ss.scan.plan.qual, &ss.scan.plan.targetlist, ss.cb_target_cols.as_ref());
    }
    let mut p1 = Open::new(b.arena, ss.scan.plan.plan_node_id, Source::Range { site, am, scanrelid, relid }, Pipeline::eligible(0, End::Sink, Builder::rows(&ss.scan.plan)));
    crate::ops::filters(&mut p1, &ss.scan.plan.qual);
    // the first pass projects its key alone (entry 1): a row pipeline carries its projection (lowerv2 `row::shape_of`)
    let alloc = b.alloc.ok_or(Refusal::shape(k, id, "TwoPass: no node allocator (decompose_in)"))?;
    let oom = || Refusal::shape(k, id, "TwoPass: out of memory synthesizing the first pass's projection");
    let te = Node::mk_target_entry(alloc, key.expr.node(), 1, None, false).map_err(|_| oom())?;
    let tlist = match ext {
        // entry 2: the extreme's column, which pass 1's folds read as OUTER_VAR 2 (`pass1_agg`)
        Some((_, vn, _, _)) => {
            let te2 = Node::mk_target_entry(alloc, vn, 2, None, false).map_err(|_| oom())?;
            NodeList::from_slice(alloc, &[te, te2])
        }
        None => NodeList::from_slice(alloc, &[te]),
    };
    let tlist = tlist.and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(|_| oom())?;
    crate::ops::project(&mut p1, tlist);
    let mut sh = Shape::new(b.arena);
    sh.key_expr(b, child, 1, key.expr, key.coll, k, id)?;
    if let Some(k1) = sh.keys.last_mut() {
        k1.bytes_eq = key.bytes_eq; // D2163: the first pass keys as the group does
    }
    sh.row_count = sh.slot(SlotKind::Count, k, id)?;
    if let Some((a, _, v, nn)) = ext {
        let mut distinct_key = NONE;
        // `[count(*)][witness][count(v)][best]` (count(v) when v can be NULL, best when it is not the witness)
        let class = aggtab::row(a.aggfnoid).map_or(Class::User, |r| r.class);
        let (witness, best) = class.two_pass_bounds(desc).ok_or(Refusal::shape(k, id, "a two-pass top over an aggregate pass 1 does not bound"))?;
        let no_fn = || Refusal::shape(k, id, "a two-pass bound with no built-in extreme of its column's type");
        let e = pass1_agg(b, a, extreme_fnoid(witness, v.vartype).ok_or_else(no_fn)?, v.vartype, v, k, id)?;
        cell(b, &p1, e, &mut sh, &mut distinct_key, k, id)?;
        if !nn {
            let c = pass1_agg(b, a, crate::normalize::F_COUNT_ANY, types_core::INT8OID, v, k, id)?;
            cell(b, &p1, c, &mut sh, &mut distinct_key, k, id)?;
        }
        if let Some(best) = best {
            let e = pass1_agg(b, a, extreme_fnoid(best, v.vartype).ok_or_else(no_fn)?, v.vartype, v, k, id)?;
            cell(b, &p1, e, &mut sh, &mut distinct_key, k, id)?;
        }
        let slot_at = |i: usize| sh.slots.get(i).copied();
        let ext_slot = |s: Option<SlotKind>, max: bool| s == Some(if max { SlotKind::MaxI64 } else { SlotKind::MinI64 });
        let best_at = 2 + !nn as usize;
        if !ext_slot(slot_at(1), witness) || (!nn && slot_at(2) != Some(SlotKind::Count)) || best.is_some_and(|m| !ext_slot(slot_at(best_at), m)) || sh.slots.len() != best_at + best.is_some() as usize {
            return Err(Refusal::shape(k, id, "a two-pass extreme whose pass-1 folds are not an int extreme and its count"));
        }
        fires::fire("agg.twopass.extreme"); // shapecheck fire signal (REGISTRY DC-13): the top's order is an extreme
    }
    let (_, slot) = close(b, p1, id, k, GroupForm::Pass1, [None; 2], sh, ORDER_NONE, false, groups, groups, None)?;
    breakers::unread(&mut b.graph, slot); // the emit is dropped: the second pass is the reader
    Ok(((b.graph.pipelines.len() - 1) as u16, slot))
}

/// SETOP (merged M9, main 17:34Z option A): one side of a hashed SetOp as a hashed group keyed on `cols` over `child`'s
/// projection, its row count the record's sentinel (`numLeft` / `numRight`). The left side (`copies`) has a second `Count`
/// slot, where the set step files the group's copy count (H13d: two counters per key). A sorted SetOp's left side
/// (`sorted`, R9-L10) is a sorted group over its Sort: the emit walks the groups in the Sort's key order, PG's output
/// order. With no column (F-ZERO) a side is a plain group: its one record counts every row. Returns the emit and the slot.
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_side<'p>(b: &mut Builder<'p, '_>, child: Open<'p>, cols: &[i16], colls: &[u32], copies: bool, sorted: bool, groups: u64, k: Kind, id: i32) -> Result<(Open<'p>, u8), Refusal> {
    let mut sh = Shape::new(b.arena);
    for (i, &c) in cols.iter().enumerate() {
        sh.key(b, &child, c, colls.get(i).copied().unwrap_or(0), k, id)?;
    }
    // F-ZERO: a plain side declares no slot: its row count is the layout's implicit sentinel, the one count the plain fold
    // steps (an explicit Count slot is an aggregate's, never stepped without one), and its copy count lives in the
    // protocol's word
    if !cols.is_empty() {
        sh.row_count = sh.slot(SlotKind::Count, k, id)?;
        if copies {
            sh.slot(SlotKind::Count, k, id)?;
        }
    }
    let from = b.graph.breakers.len();
    let out = if cols.is_empty() {
        // F-ZERO: no compared column: every row is one group, a plain side (one record: its row count and the copy slot)
        close(b, child, id, k, GroupForm::Plain, [None; 2], sh, ORDER_NONE, false, 1, 1, None)?
    } else if sorted {
        ordered(b, &child, &mut sh.keys, k, id, None)?;
        close(b, child, id, k, GroupForm::Sorted, [None; 2], sh, ORDER_KEY, false, groups, groups, None)?
    } else {
        close(b, child, id, k, GroupForm::Hashed, [None; 2], sh, ORDER_NONE, false, groups, groups, None)?
    };
    // the set step compares the two sides' whole tables, so a side never spills: its region grows past its share (D254),
    // as PG 18's hashed SetOp keeps its table in memory past work_mem (nodeSetOp.c has no spill); for the same reason a
    // share-budgeted side is `Budget::Pool`: its fold's pool grows in place past the share (a plain side keeps its program
    // constant). C10: PG 18 keeps only a hashed SetOp's left (first) input in its table and streams the right one past it,
    // and a sorted SetOp streams both, so only a hashed left side's table is PostgreSQL's own; the others are debt
    let holds = {
        use gravity_program::spill::{NoSpill, PgHolds, Shape};
        match (copies, sorted) {
            (true, false) => Shape::PgHolds(PgHolds::SetOpSide),
            (_, true) => Shape::Grows(NoSpill::SetOpSortedSide),
            (false, false) => Shape::Grows(NoSpill::SetOpRightSide),
        }
    };
    let g = &mut b.graph;
    for j in from..g.breakers.len() {
        if g.breakers[j].breaker.slot == out.1 {
            let budget = match g.breakers[j].breaker.budget {
                Budget::Share => Budget::Pool,
                other => other,
            };
            g.breakers[j].breaker.spill = Spill::None;
            g.breakers[j].breaker.budget = budget;
            let p = g.breakers[j].producer as usize;
            if let End::Breaker(br) = g.pipelines[p].desc.ends_in {
                let nb = Breaker { spill: Spill::None, budget, ..br };
                g.pipelines[p].desc.ends_in = End::Breaker(nb);
                // a plain side keeps its program constant (`spill::declared`)
                if budget == Budget::Pool {
                    g.pipelines[p].desc.holds = Some(gravity_program::spill::plan(&nb, holds));
                }
            }
        }
    }
    Ok(out)
}

/// The input columns (`OUTER_VAR`) a keyless group keeps from its first row, once each in first-seen order
/// (facts::demand `pinned`, the one list exec's agg.arena types the kept slots by). A bare resjunk column is not a read
/// (D345): only a plan's top node carries one (`apply_tlist_labeling`; `build_path_tlist` marks none), and the junk filter
/// drops it before the client, so `GROUP BY k` with `k` outside the select list pins nothing (`unkept_junk`). D3728 / D3766:
/// a resjunk entry that computes (a Sort's key over the group's row) reads its columns from the group's first row, as
/// PostgreSQL's Agg projects it: they are kept.
fn pinned_cols<'p>(arena: gravity_program::ArenaRef, tlist: &'p NodeList<'p>, qual: &'p NodeList<'p>) -> ArenaVec<i16> {
    let mut out: ArenaVec<i16> = ArenaVec::new_in(arena); // alloc: Cold
    crate::facts::demand::pinned(tlist, qual, &mut |v| out.push(v.varattno)); // alloc: Cold
    out
}

/// D3728: a node whose every key the planner dropped (`Shape::empty_no_row`) emits at most one row; a resjunk entry over a
/// bare input column its record carries neither as a key nor as a kept column (`carried`) reads a typed NULL, as
/// `unread_junk`'s entries do: it reaches no client (the junk filter drops it), and the nodes above Agg `plan` read it
/// at most as a key (facts::demand `reads_above`: a Sort or Unique over the Agg shares its target list), which over that
/// one row compares nothing. One a node above reads by value refuses by name: its NULL would show.
/// `SELECT name, avg(pages) FROM book WHERE id = 1 GROUP BY id` (Django's annotate() of a row fetched by its key) carries
/// `id` so; the record keeps `name`. Unchanged otherwise.
fn unkept_junk<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, carried: &dyn Fn(i16) -> bool, tlist: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let bare = |te: &TargetEntry<'p>| te.expr.as_var().filter(|v| te.resjunk && v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !carried(v.varattno));
    let junk = || tlist.iter().filter_map(|n| n.as_target_entry()).filter(|&te| bare(te).is_some());
    if junk().any(|te| reads_above(b, plan, te.resno) == Read::Value) {
        return Err(Refusal::shape(k, id, "a keyless group's resjunk column its record does not carry, read by value above"));
    }
    if junk().next().is_none() {
        return Ok(tlist);
    }
    null_junk(b, tlist, &bare, ["a keyless group's resjunk column: no node allocator (decompose_in)", "a keyless group's resjunk column: out of memory"], k, id)
}

/// `tlist` with each entry `pick` names a bare column of (`unread_junk`, `unkept_junk`) reading a typed NULL of that
/// column's type, its resno, name and junk mark kept: a new list, the node's own unchanged. `why`: the refusals of a
/// missing node allocator and of an allocation that fails.
fn null_junk<'p>(b: &Builder<'p, '_>, tlist: &'p NodeList<'p>, pick: &dyn Fn(&TargetEntry<'p>) -> Option<&'p Var<'p>>, why: [&'static str; 2], k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, why[0]))?;
    let oom = |_| Refusal::shape(k, id, why[1]);
    let mut out = NodeList::with_capacity(mcx, tlist.len()).map_err(oom)?;
    for n in tlist.iter() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let entry = match pick(te) {
            Some(v) => {
                let (len, byval) = b.facts.type_row(v.vartype).map_or((-1, false), |t| (t.typlen as i32, t.byval));
                let null = Node::mk_const(mcx, v.vartype, v.vartypmod, v.varcollid, len, datum::Datum::from_i64(0), true, byval).map_err(oom)?;
                Node::mk_target_entry(mcx, null, te.resno, te.resname, true).map_err(oom)?
            }
            None => n,
        };
        out.lappend(mcx, entry).map_err(oom)?;
    }
    Ok(Node::mk_list(mcx, out).map_err(oom)?.as_list().expect("a list node"))
}

/// D416: the node groups the query's own GROUP BY / DISTINCT (a target entry carries a sortgroupref: the grouping
/// target's), not a semi-join's unique-ification of its inner rel (build_path_tlist over a rel target: no refs), whose
/// target list may carry a column the keys do not determine (a row mark's ctid / tableoid).
fn query_grouping(tlist: &NodeList<'_>) -> bool {
    tlist.iter().any(|n| n.as_target_entry().is_some_and(|t| t.ressortgroupref != 0))
}

/// `pinned_cols` with each column's collation (the `Var`'s); `junk`: the resjunk entries too (a sort key an upper node
/// reads from the group's row).
fn pinned_vars<'p>(arena: gravity_program::ArenaRef, lists: &[&'p NodeList<'p>; 2], junk: bool) -> ArenaVec<(i16, u32)> {
    use crate::facts::demand::{agg_reads, Junk};
    let mut out: ArenaVec<(i16, u32)> = ArenaVec::new_in(arena); // alloc: Cold
    agg_reads(lists[0], lists[1], if junk { Junk::All } else { Junk::Skip }, &mut |v| {
        if !out.iter().any(|o| o.0 == v.varattno) {
            out.push((v.varattno, v.varcollid)); // alloc: Cold
        }
    });
    out
}

/// The node's aggregates once each, by `aggno` (unbounded; the u8 slot ids are the sanity limit): every one PG's Agg runs,
/// read or not (`expr::run_aggrefs`, exec v4/m6.rs agg.arena's list).
fn collect<'p>(arena: gravity_program::ArenaRef, lists: &[&'p NodeList<'p>], k: Kind, id: i32) -> Result<ArenaVec<Node<'p>>, Refusal> {
    let mut out: ArenaVec<Node<'p>> = ArenaVec::new_in(arena);
    let aggno = |o: &Node<'p>| o.as_aggref().unwrap().aggno;
    for l in lists {
        for e in l.iter() {
            expr::run_aggrefs(e, &mut |x| {
                let a = aggno(&x);
                if !out.iter().any(|o| aggno(o) == a) {
                    let pos = out.iter().position(|o| aggno(o) > a).unwrap_or(out.len());
                    out.insert(pos, x);
                }
            });
        }
    }
    if out.len() >= NONE as usize {
        return Err(Refusal::shape(k, id, "more than 65534 aggregates (the id width)"));
    }
    Ok(out)
}

/// agg_core-0240 / 0241 (window-106's form): a DISTINCT HashAgg (no aggregate, no qual) carries a window's ORDER BY
/// columns the planner passed up as resjunk entries over input columns it does not group. PostgreSQL fills them from any
/// row of the group, and no node reads them: a resjunk entry is the plan top's (`pinned_cols`, D345:
/// apply_tlist_labeling), and the junk filter drops it before the client (a subquery's or a SubPlan's top: its reader
/// names the leading, non-junk columns only). Each such entry (a bare column, not a key) reads a typed NULL, so the
/// group's record need not carry the column (lowerv4 "a column the row does not carry"). D3785: but not one a node
/// above Agg `plan` reads (facts/demand.rs `reads_above`: a Sort, Unique or Limit over the Agg shares its target list, so
/// the top's junk marks are the Agg's entries too, and `GROUP BY id ORDER BY rating` sorts by one): read as NULL it
/// sorted every row as a tie. Unchanged otherwise (the list is the node's own).
fn unread_junk<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, keyed: &[i16], tlist: &'p NodeList<'p>, qual: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let bare = |e: Node<'p>| e.as_var().filter(|v| v.varno == OUTER_VAR && v.varattno > 0 && !keyed.contains(&v.varattno));
    let junk = |te: &TargetEntry<'p>| te.resjunk && bare(te.expr).is_some() && reads_above(b, plan, te.resno) == Read::No;
    if !qual.is_nil() || keyed.is_empty() || tlist.iter().any(has_aggref) || !tlist.iter().filter_map(|n| n.as_target_entry()).any(junk) {
        return Ok(tlist);
    }
    let pick = |te: &TargetEntry<'p>| bare(te.expr).filter(|_| junk(te));
    null_junk(b, tlist, &pick, ["a DISTINCT's unread junk: no node allocator (decompose_in)", "a DISTINCT's unread junk: out of memory"], k, id)
}

/// A bare input column the node emits outside its keys whose input entry is a Const (DISTINCT over `'returns'` or
/// `NULL::text`: the planner drops a Const from the keys, and under a removed SubqueryScan `duplicate_keys` reads no
/// `fd` evidence) is that Const: every row of the group holds it, and PG's representative row emits it. The record
/// carries no word for it.
fn const_cols<'p>(b: &Builder<'p, '_>, child: &Open<'p>, keyed: &[i16], tlist: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let value = |e: Node<'p>| e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !keyed.contains(&v.varattno)).and_then(|v| scan_entry(child, v.varattno)).filter(|&x| relabels(x).as_const().is_some());
    if !tlist.iter().filter_map(|n| n.as_target_entry()).any(|te| value(te.expr).is_some()) {
        return Ok(tlist);
    }
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "a Const column: no node allocator (decompose_in)"))?;
    let oom = |_| Refusal::shape(k, id, "a Const column: out of memory");
    let mut out = NodeList::with_capacity(mcx, tlist.len()).map_err(oom)?;
    for n in tlist.iter() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let entry = match value(te.expr) {
            Some(c) => Node::mk_target_entry(mcx, c, te.resno, te.resname, te.resjunk).map_err(oom)?,
            None => n,
        };
        out.lappend(mcx, entry).map_err(oom)?;
    }
    Ok(Node::mk_list(mcx, out).map_err(oom)?.as_list().expect("a list node"))
}

/// D2343 (unread): a bare input column the node emits outside its keys and aggregates that no node above reads
/// (facts/demand.rs `reads_above`) reads a typed NULL, as `unread_junk`'s entries do: the record never carries it and
/// PostgreSQL's representative value never shows. A resjunk entry is `unread_junk`'s alone (the walk this replaced read
/// every resjunk entry it reached the top with as read). `make_window_input_target` passes such a column up (`GROUP BY date_trunc('month', d)` emits `d` beside the
/// key for a window over the groups, which reads only the key), and the keys do not determine it (`duplicate_keys`).
fn unread_cols<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, keyed: &[i16], tlist: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let bare = |e: Node<'p>| e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !keyed.contains(&v.varattno));
    let mut dead: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for te in tlist.iter().filter_map(|n| n.as_target_entry()) {
        if bare(te.expr).is_some() && !te.resjunk && reads_above(b, plan, te.resno) == Read::No {
            dead.push(te.resno);
        }
    }
    if dead.is_empty() {
        return Ok(tlist);
    }
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "an unread column: no node allocator (decompose_in)"))?;
    let oom = |_| Refusal::shape(k, id, "an unread column: out of memory");
    let mut out = NodeList::with_capacity(mcx, tlist.len()).map_err(oom)?;
    for n in tlist.iter() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let entry = match bare(te.expr).filter(|_| dead.contains(&te.resno)) {
            Some(v) => {
                let (len, byval) = b.facts.type_row(v.vartype).map_or((-1, false), |t| (t.typlen as i32, t.byval));
                let null = Node::mk_const(mcx, v.vartype, v.vartypmod, v.varcollid, len, datum::Datum::from_i64(0), true, byval).map_err(oom)?;
                Node::mk_target_entry(mcx, null, te.resno, te.resname, te.resjunk).map_err(oom)?
            }
            None => n,
        };
        out.lappend(mcx, entry).map_err(oom)?;
    }
    Ok(Node::mk_list(mcx, out).map_err(oom)?.as_list().expect("a list node"))
}

/// NEW-numeric-1: whether an error the calls of Agg `a` raise reaches the statement before any of its rows left, so the
/// statement's recheck licence (facts::order `license_raising`) re-runs it in plan order: a SELECT that does not act,
/// outside the plan-order run, with no SubPlan, InitPlan or CTE, whose Agg folds every row before it emits one (plain or
/// hashed, no grouping sets) under ancestors of one child each up to the top (a Sort, a Limit, a projection, another
/// Agg): with no join, Append or SubPlan above it nothing runs it twice or emits beside it, so its fold ends before the
/// statement's first row.
fn held_whole<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>) -> Option<crate::effects::Licence> {
    let folds_whole = |x: &Agg<'_>| matches!(x.aggstrategy, AGG_PLAIN | AGG_HASHED) && !crate::ops::per_set(x);
    if crate::effects::denying() || b.stmt.subplans.iter().next().is_some() || !folds_whole(a) {
        return None;
    }
    if crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read) {
        return None;
    }
    let mut at: &'p Plan<'p> = &a.plan;
    while let Some((p, _)) = crate::facts::demand::readers::parent_of(b, at) {
        match p.as_plan() {
            // N11 (ii'): no Limit above, so no top-N folds the group's records in its own table (topn's order slots)
            Some(pp) if pp.righttree.is_none() && !matches!(p.node_tag(), NodeTag::T_Limit) => at = pp,
            _ => return None,
        }
    }
    // the statement's recheck licence: facts::order `license_raising` asks the same (the statement does not act) and records it
    if !b.stmt.planTree.and_then(|r| r.as_plan()).is_some_and(|r| core::ptr::eq(r, at)) {
        return None;
    }
    crate::effects::licence(crate::effects::Effects::RAISES, crate::effects::Departure::Ahead).ok()
}

/// NEW-numeric-1: `e` reads a stored unconstrained numeric column of the scan (its values' own dscales).
fn stored_numeric<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> bool {
    let s = strip(child, e);
    s.node.as_var().is_some_and(|v| v.vartype == crate::catalog::NUMERICOID && v.vartypmod < 0) && scan_lane(b, child, s).is_some()
}

/// D4723 (road agg.num_cte): `e` reads a numeric column of a materialized CTE's store (the pipeline's source is the
/// CteScan's slot) whose every value has one display scale: the CTE's plan is an Agg whose output column is a numeric sum
/// folded at a decided scale with no dscale word (its every input at that dscale, so each group's sum is too: numeric's
/// sum keeps the largest input dscale) or a min / max of one fixed-scale argument (each value at its typmod's scale). Its
/// order keys then never tie on value at two display scales, so no merge of the keyed extreme raises into a plan-order
/// run (`settle_keyed`'s `LAYOUT_KEY_TIES`), and the extreme needs no recheck licence (NEW-numeric-1's `held_whole`).
fn cte_one_dscale<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> bool {
    if !b.facts.road_on(crate::ops::shared::agg::NUM_CTE_ROAD) {
        return false;
    }
    let s = strip(child, e);
    let Some(v) = s.node.as_var().filter(|v| v.vartype == crate::catalog::NUMERICOID && v.varlevelsup == 0 && v.varno > 0 && v.varattno > 0) else { return false };
    let Source::Breaker { slot } = child.source else { return false };
    let Some(&(plan_id, _)) = b.ctes.iter().rev().find(|(_, s)| *s == slot) else { return false };
    let mut scan = None;
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if scan.is_none() && n.as_plan().is_some_and(|p| p.plan_node_id == child.node_id || child.node_ops.first().is_some_and(|o| o.node_id == p.plan_node_id)) {
            scan = n.as_cte_scan();
        }
    });
    let Some(cs) = scan.filter(|cs| cs.ctePlanId == plan_id && cs.scan.scanrelid as i32 == v.varno) else { return false };
    let Some(agg) = (cs.ctePlanId as usize).checked_sub(1).and_then(|n| b.stmt.subplans.nth(n)).and_then(|n| n.as_agg()) else { return false };
    let Some(r) = agg.plan.targetlist.iter().nth(v.varattno as usize - 1).and_then(|n| n.as_target_entry()).and_then(|te| relabels(te.expr).as_aggref()) else { return false };
    let Some(g) = b.graph.groups.iter().find(|g| g.node_id == agg.plan.plan_node_id) else { return false };
    let one = g.aggs.iter().any(|d| {
        i32::from(d.aggno) == r.aggno && d.node.as_aggref().is_some_and(|x| x.aggfnoid == r.aggfnoid) && d.dscale_slot == NONE && d.scale >= 0 && d.mods.distinct == 0
            && matches!(d.reg, Register::SumNum | Register::MinNum | Register::MaxNum)
    });
    if one {
        fires::fire("agg.num_cte"); // shapecheck fire signal (D4723)
    }
    one
}

/// NEW-numeric-1 (P): the input pipeline ends a hash join's drain (a RIGHT / FULL / RIGHT SEMI / RIGHT ANTI join's
/// unmatched build rows, another arm than the probe's); the order-key form stays off it, its merges' order not shown.
fn drains(child: &Open<'_>) -> bool {
    child.ops.iter().any(|o| matches!(o, Op::Join(crate::join::JoinDesc::Drain { .. })))
}

fn has_aggref(n: Node<'_>) -> bool {
    let mut found = false;
    expr::walk(n, &mut |x| {
        found |= x.node_tag() == NodeTag::T_Aggref;
        !found
    });
    found
}

/// PSP2-34 (group; lowerv4 ops/group/distinct_multi.rs): a plain node whose every call is `count(DISTINCT x)` (one
/// argument, no FILTER, no ORDER BY, not split) over two or more arguments, each one integer word under its type's own
/// `=` (`key_kind`'s I32 / I64 / Date / Timestamp; the pair key's own-family rule) or (PS1-32) a text / varchar under a
/// collation whose `=` is byte equality (`KeyImage::Text`: its Fp128 image, two words) over an input PS1-19's entries
/// form cannot take (a filtered or non-pgrcolumnar scan), keys every argument: `cell` made the first the pair key
/// and left the others host-sorted (D1210: `FinalForm::Distinct`, no slot); each becomes a resno-0 key of its own and
/// each call a `PairCount` naming its argument's key in `count_slot` (the first key's calls too), so one keyed end
/// counts every argument's distinct values in one pass. Any other node is left as `cell` made it. The argument's own
/// type must be an integer-word or a text one: a narrow numeric keys as one scaled I64 word here, but lowerv4's record
/// keeps it as two words (keyimg `record_words`), so the word kind alone does not decide.
fn multi_counts<'p>(b: &Builder<'p, '_>, child: &Open<'p>, sh: &mut Shape<'p>, first: u16, k: Kind, id: i32) -> Result<(), Refusal> {
    use types_core::{DATEOID, INT2OID, INT4OID, INT8OID, OIDOID, TEXTOID, TIMESTAMPOID, TIMESTAMPTZOID, VARCHAROID};
    let int_type = |t: u32| matches!(t, INT2OID | INT4OID | INT8OID | DATEOID | TIMESTAMPOID | TIMESTAMPTZOID | OIDOID);
    // PS1-19 (lowerv4 dict_entries.rs `every`) folds an unfiltered pgrcolumnar scan's text columns from their dictionary
    // entries, a node with no key: a text argument keys here only where that form cannot take the node
    let entries = matches!(child.source, Source::Range { am: crate::catalog::Am::Pgrcolumnar, .. }) && !child.ops.iter().any(|o| matches!(o, Op::Filter(_)));
    let text_type = |t: u32| !entries && matches!(t, TEXTOID | VARCHAROID);
    // a text argument keys here only over a scan column whose distinct values the planner counts at most a fitted share of
    // its relation's rows (`Facts::att_ndistinct`): the one-pass table holds an entry per value, and near-unique text (about
    // an entry a row) folds faster host-sorted
    let few = |s: Stripped<'p>| {
        let col = crate::facts::pinned::scan_column(b, child, s.node);
        col.and_then(|(relid, a)| b.facts.att_ndistinct(relid, a)).is_some_and(|(n, rows)| n as f64 <= gravity_program::cost::owner::TEXT_COUNTS_DISTINCT_SHARE * rows as f64)
    };
    let typed = |s: Stripped<'p>| {
        let t = crate::catalog::expr_type::type_of(s.node);
        !s.bpchar && ((t.is_some_and(int_type) && s.retype.is_none_or(int_type)) || (t.is_some_and(text_type) && s.retype.is_none_or(text_type) && few(s)))
    };
    let word = |kind: KeyKind, words: u8, scale: i16, text: bool| {
        (matches!(kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) && words == 1 && scale == NO_SCALE) || (kind == KeyKind::Fp128 && words == 2 && text)
    };
    let call = |d: &AggDecl<'p>| d.class == Class::CountAny && d.mods.distinct == 1 && d.mods.order == 0 && d.mods.direct == 0 && d.filter.is_none() && d.mods.split == Split::Simple && matches!(d.final_, FinalForm::PairCount | FinalForm::Distinct);
    let k0 = &sh.keys[first as usize];
    if sh.keys.len() != 1 || !word(k0.kind, k0.words, k0.scale, k0.image.is_text()) || !typed(k0.expr) || !sh.aggs.iter().all(call) || !sh.aggs.iter().any(|d| d.final_ == FinalForm::Distinct) {
        return Ok(());
    }
    // every host-sorted call's argument checked before any key is added: the node keys all of them or none
    let mut args: ArenaVec<(usize, Stripped<'p>, u32)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, d) in sh.aggs.iter().enumerate().filter(|(_, d)| d.final_ == FinalForm::Distinct) {
        let Some(a) = d.node.as_aggref() else { return Ok(()) };
        if a.args.len() != 1 {
            return Ok(());
        }
        let e = strip(child, a.args.nth(0).as_target_entry().map_or(a.args.nth(0), |t| t.expr));
        let own_eq = a.aggdistinct.iter().filter_map(|n| n.as_sort_group_clause()).all(|c| b.facts.key(crate::catalog::KeyReq::sort(c.sortop)).is_some_and(|o| o.default_family));
        if !own_eq || !typed(e) || !key_kind(b, child, e, a.inputcollid, false, k, id).is_ok_and(|w| word(w.kind, w.words, w.scale, w.image.is_text())) {
            return Ok(());
        }
        args.push((i, e, a.inputcollid));
    }
    for d in sh.aggs.iter_mut().filter(|d| d.final_ == FinalForm::PairCount) {
        d.count_slot = first;
    }
    for (i, e, coll) in args.iter().copied() {
        let j = match sh.keys.iter().position(|key| key.resno == 0 && key.coll == coll && same_var(b, child, key.expr, e)) {
            Some(j) => j as u16,
            None => sh.key_expr(b, child, 0, e, coll, k, id)?,
        };
        let d = sh.aggs[i];
        sh.aggs[i] = AggDecl { reg: Register::DistinctCount, slot: NONE, count_slot: j, final_: FinalForm::PairCount, trans: Trans::Var, scale: NO_SCALE, ..d };
    }
    Ok(())
}

/// A grouped node whose every DISTINCT call is `count(DISTINCT x)` (one argument, no ORDER BY, not split) over two or
/// more arguments, each an integer word or a byte-equal text under its type's own `=` (`multi_counts`' type rule,
/// D2127): `cell` made the first argument the pair key (the last key, `first`) and left the others host-sorted
/// (`FinalForm::Distinct`, no slot, a serial hosted arena); each other argument becomes a resno-0 key after it and each
/// call a `PairCount` naming its argument's key in `count_slot`, so one shared table of `[group keys, tag, image]` pairs
/// counts every argument's distinct values per group in one parallel pass (lowerv4 ops/group/pairs_multi.rs; the tag is
/// the argument's index among the resno-0 keys). A call's FILTER is its key's: a row the FILTER does not pass reads that
/// key as NULL (count's strict input), so a call keys its argument apart from any call under another FILTER (`first`
/// keeps the pair calls under the first one's; the same argument under two FILTERs is two keys). The riders keep their
/// slots. Any other node is left as `cell` made it.
fn multi_grouped<'p>(b: &Builder<'p, '_>, child: &Open<'p>, sh: &mut Shape<'p>, first: u16, k: Kind, id: i32) -> Result<(), Refusal> {
    use types_core::{DATEOID, INT2OID, INT4OID, INT8OID, OIDOID, TEXTOID, TIMESTAMPOID, TIMESTAMPTZOID, VARCHAROID};
    let int_type = |t: u32| matches!(t, INT2OID | INT4OID | INT8OID | DATEOID | TIMESTAMPOID | TIMESTAMPTZOID | OIDOID);
    let text_type = |t: u32| matches!(t, TEXTOID | VARCHAROID);
    let typed = |s: Stripped<'p>| {
        let t = crate::catalog::expr_type::type_of(s.node);
        !s.bpchar && ((t.is_some_and(int_type) && s.retype.is_none_or(int_type)) || (t.is_some_and(text_type) && s.retype.is_none_or(text_type)))
    };
    let word = |kind: KeyKind, words: u8, scale: i16, text: bool| {
        (matches!(kind, KeyKind::I32 | KeyKind::I64 | KeyKind::Date | KeyKind::Timestamp) && words == 1 && scale == NO_SCALE) || (kind == KeyKind::Fp128 && words == 2 && text)
    };
    let call = |d: &AggDecl<'p>| d.class == Class::CountAny && d.mods.distinct == 1 && d.mods.order == 0 && d.mods.direct == 0 && d.mods.split == Split::Simple && matches!(d.final_, FinalForm::PairCount | FinalForm::Distinct);
    // two calls share a key only under the same FILTER: none, or the one node
    let same = |f: Option<Node<'p>>, g: Option<Node<'p>>| match (f, g) {
        (None, None) => true,
        (Some(f), Some(g)) => f.ptr_eq(g),
        _ => false,
    };
    let Some(k0) = sh.keys.get(first as usize).filter(|_| first as usize + 1 == sh.keys.len()) else { return Ok(()) };
    let grouped = sh.keys[..first as usize].iter().all(|key| key.resno != 0) && first > 0;
    if !grouped || k0.resno != 0 || !word(k0.kind, k0.words, k0.scale, k0.image.is_text()) || !typed(k0.expr) || !sh.aggs.iter().filter(|d| d.mods.distinct > 0).all(call) {
        return Ok(());
    }
    // `first` keeps the calls `cell` paired under the first one's FILTER (`f0`); a pair call under another FILTER keys
    // its argument apart as a host-sorted one does
    let f0 = sh.aggs.iter().find(|d| d.final_ == FinalForm::PairCount).and_then(|d| d.filter);
    let apart = |d: &AggDecl<'p>| d.final_ == FinalForm::Distinct || (d.final_ == FinalForm::PairCount && !same(d.filter, f0));
    // every such call's argument checked before any key is added: the node keys all of them or none
    let mut args: ArenaVec<(usize, Stripped<'p>, u32, Option<Node<'p>>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, d) in sh.aggs.iter().enumerate().filter(|(_, d)| apart(d)) {
        let Some(a) = d.node.as_aggref() else { return Ok(()) };
        if a.args.len() != 1 {
            return Ok(());
        }
        let e = strip(child, a.args.nth(0).as_target_entry().map_or(a.args.nth(0), |t| t.expr));
        let own_eq = a.aggdistinct.iter().filter_map(|n| n.as_sort_group_clause()).all(|c| b.facts.key(crate::catalog::KeyReq::sort(c.sortop)).is_some_and(|o| o.default_family));
        if !own_eq || !typed(e) || !key_kind(b, child, e, a.inputcollid, false, k, id).is_ok_and(|w| word(w.kind, w.words, w.scale, w.image.is_text())) {
            return Ok(());
        }
        args.push((i, e, a.inputcollid, d.filter));
    }
    if args.is_empty() {
        return Ok(());
    }
    // the image's NULL flags stay one word (`breaker::flag_words`): lowerv4's tag-0 pair flags its argument at bit `first`
    if sh.keys.len() + args.len() > 64 {
        return Ok(());
    }
    for d in sh.aggs.iter_mut().filter(|d| d.final_ == FinalForm::PairCount) {
        d.count_slot = first;
    }
    // each resno-0 key's FILTER (its calls'), the first's `f0`
    let mut keyed: ArenaVec<(u16, Option<Node<'p>>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    keyed.push((first, f0));
    for (i, e, coll, f) in args.iter().copied() {
        let at = keyed.iter().find(|&&(j, g)| same(f, g) && sh.keys[j as usize].coll == coll && same_var(b, child, sh.keys[j as usize].expr, e)).map(|&(j, _)| j);
        let j = match at {
            Some(j) => j,
            None => sh.key_expr(b, child, 0, e, coll, k, id)?,
        };
        if at.is_none() {
            keyed.push((j, f));
        }
        let d = sh.aggs[i];
        sh.aggs[i] = AggDecl { reg: Register::DistinctCount, slot: NONE, count_slot: j, final_: FinalForm::PairCount, trans: Trans::Var, scale: NO_SCALE, ..d };
    }
    fires::fire("agg.distinct_multi_keyed"); // shapecheck fire signal: a grouped multi-argument count(DISTINCT) keyed for one tagged pair set (lowerv4 pairs_multi.rs fires agg.distinct_multi_grouped where it takes it)
    Ok(())
}

/// pg_operator `=` and `<` for record (record_eq, record_lt: record_ops, the btree default for record).
const RECORD_EQ_OP: u32 = 2988;
const RECORD_LT_OP: u32 = 2990;

/// DC-21: every call of `refs` is `count(DISTINCT ROW(..))` (one row argument, record_eq / record_lt, no FILTER or ORDER BY)
/// over one and the same row node: normalize's rewrite gives each `count(*)` it replaces that node, and a call whose
/// argument is a column of the input's projection reads the row that projection computes (`strip`). A call PostgreSQL
/// presorts reads the row through the Sort's pass-through column instead, so it keeps the record key.
fn row_counts<'p>(child: &Open<'p>, refs: &[Node<'p>]) -> bool {
    let row = |r: Node<'p>| {
        let a = r.as_aggref()?;
        let one = a.aggfnoid == crate::normalize::F_COUNT_ANY && a.args.len() == 1 && a.aggfilter.is_none() && a.aggorder.is_nil() && a.aggdirectargs.is_nil() && a.aggdistinct.len() == 1;
        let clause = a.aggdistinct.iter().filter_map(|n| n.as_sort_group_clause()).all(|c| c.eqop == RECORD_EQ_OP && c.sortop == RECORD_LT_OP);
        if !(one && clause) {
            return None;
        }
        let e = strip(child, a.args.nth(0).as_target_entry().map_or(a.args.nth(0), |t| t.expr)).node;
        e.as_row_expr().is_some().then_some(e)
    };
    let Some(first) = refs.first().and_then(|&r| row(r)) else { return false };
    refs.iter().all(|&r| row(r).is_some_and(|e| e.ptr_eq(first)))
}

/// DC-21: `count(DISTINCT ROW(k1, .., kn))` (normalize's spelling of count(*) over a grouping by a nullable key or by
/// several keys, or written so) keys each field as a resno-0 key of its own under the field's collation, its NULL flag
/// kept, and the call is one `PairCount` over all of them with `mods.star`: a row is never NULL and record_eq holds two
/// NULL fields equal, so every row's fields reach the key set and each distinct combination, NULLs included, counts
/// once (lowerv4 distinct_plain.rs's count form over a row). The clause must be record_eq / record_lt and every field a
/// key under its type's own `=` (record_eq compares a field by its type's default btree equality) and its own
/// determinate collation, not itself a record; a second such call shares the keys only when its fields are the same. False (nothing added) leaves the call
/// to the record key `cell` makes otherwise (one varlena key, the hosted hash).
#[allow(clippy::too_many_arguments)]
fn row_count<'p>(b: &Builder<'p, '_>, child: &Open<'p>, sh: &mut Shape<'p>, a: &'p Aggref<'p>, node: Node<'p>, class: Class, mods: Mods, filter: Option<Node<'p>>, e: Stripped<'p>, distinct_key: &mut u16, k: Kind, id: i32) -> Result<bool, Refusal> {
    let Some(row) = e.node.as_row_expr().filter(|r| sh.row_form && !r.args.is_nil()) else { return Ok(false) };
    let record_clause = a.aggdistinct.len() == 1 && a.aggdistinct.iter().filter_map(|n| n.as_sort_group_clause()).all(|c| c.eqop == RECORD_EQ_OP && c.sortop == RECORD_LT_OP);
    if class != Class::CountAny || mods.distinct != 1 || mods.order != 0 || mods.direct != 0 || filter.is_some() || mods.split != Split::Simple || !record_clause {
        return Ok(false);
    }
    let n = row.args.len();
    // a field of the call's own row reads the input's column; one of the row the input projects is that entry's
    let fields = || row.args.iter().map(|f| (e.arg(child, f), crate::catalog::expr_type::collation_of(f)));
    if *distinct_key != NONE {
        // a second call over the same row: the same keys, field by field, in order
        let at = *distinct_key as usize;
        let same = sh.keys.len() >= at + n && fields().enumerate().all(|(i, (f, coll))| {
            let key = &sh.keys[at + i];
            key.resno == 0 && key.coll == coll && same_var(b, child, key.expr, f)
        });
        if !same {
            return Ok(false);
        }
    } else {
        // the count form holds the row's keys and their flags word(s) in one inline record (abi MAX_RECORD_KEYS key
        // words, no wide tier; lowerv4 distinct_plain count_rec): a row whose fields need more words keeps the record key
        let mut words = sh.key_kinds.len();
        for (f, coll) in fields() {
            let indeterminate = crate::catalog::expr_type::type_of(f.node).is_some_and(|t| crate::ops::shared::sort::indeterminate(b.facts, t, coll));
            match key_kind(b, child, f, coll, false, k, id) {
                Ok(w) if !indeterminate && !crate::catalog::anonymous_record(w.tf) => words += w.words as usize,
                _ => return Ok(false),
            }
        }
        if words + gravity_abi::breaker::flag_words(n) > gravity_abi::breaker::MAX_RECORD_KEYS {
            return Ok(false);
        }
        let first = sh.keys.len() as u16;
        for (f, coll) in fields() {
            sh.key_expr(b, child, 0, f, coll, k, id)?;
        }
        *distinct_key = first;
    }
    fires::fire("agg.pair_count"); // shapecheck fire signal (REGISTRY DC-08)
    sh.aggs.push(AggDecl { aggno: a.aggno as i16, fnoid: a.aggfnoid, reg: Register::DistinctCount, slot: NONE, count_slot: NONE, final_: FinalForm::PairCount, node, scale: NO_SCALE, filter, class, trans: Trans::Var, mods: Mods { star: true, ..mods }, coll: b.facts.coll(a.inputcollid), dscale_slot: NONE, dscales: &[] });
    Ok(true)
}

/// One aggregate's cell: the catalog row's class (`aggtab`), the modifiers as fields, and the slot / final they decide.
fn cell<'p>(b: &Builder<'p, '_>, child: &Open<'p>, node: Node<'p>, sh: &mut Shape<'p>, distinct_key: &mut u16, k: Kind, id: i32) -> Result<(), Refusal> {
    // FOLD-19: the arguments' canonical form (normalize `case_simple`), so every reader of the decl sees one CASE form
    let node = crate::normalize::agg_args(b, node)?;
    let a: &'p Aggref<'p> = node.as_aggref().unwrap();
    if a.agglevelsup != 0 {
        return Err(Refusal::shape(k, id, "an outer-level aggregate"));
    }
    let arg = (!a.args.is_nil()).then(|| a.args.nth(0).as_target_entry().map_or(a.args.nth(0), |t| t.expr));
    // normalize `count_star`: count(x) with x never NULL where the aggregate reads it is count(*): a NOT NULL column of
    // the scan read here (a Var only: an expression over one can still raise, and count(*) would not evaluate it) or a
    // non-NULL constant, over any input (count(1), count('x'): a Const neither raises nor is NULL; count(NULL::int)
    // stays count(x), 0; a Param is not a Const here)
    let never_null = |e: Node<'p>| {
        let s = strip(child, e);
        match s.node.node_tag() {
            NodeTag::T_Var => notnull(b, child, s),
            NodeTag::T_Const => s.node.as_const().is_some_and(|c| !c.constisnull),
            _ => false,
        }
    };
    let star = crate::normalize::count_star(a.aggfnoid, !a.aggdistinct.is_nil(), arg.is_some_and(never_null));
    let (fnoid, arg) = match star {
        Some(f) => (f, None),
        None => (a.aggfnoid, arg),
    };
    let (class, kind) = aggtab::row(fnoid).map_or((Class::User, a.aggkind as u8), |r| (r.class, r.kind));
    let filter = a.aggfilter;
    if filter.is_some() {
        sh.nn = false;
    }
    let mods = Mods { distinct: a.aggdistinct.len() as u8, order: a.aggorder.len() as u8, filter: filter.is_some(), direct: a.aggdirectargs.len() as u8, split: Split::of(a.aggsplit), star: a.aggstar || star.is_some() };
    // D1292: an aggregate that compares its input (DISTINCT / ORDER BY: each argument under its own collation, min / max
    // under the input collation) over a collatable argument with no collation raises in PG at its first compare. A plain
    // min / max folds PG's own transition function (text_smaller / text_larger ...) under the input collation in its
    // hosted state (MinVar / MaxVar, `Trans::Var`), which raises 42P22 at that same compare, a group's second non-null
    // value (the first is the state, no call), in a worker's partial fold or at the merge of two: never the code register
    // (`code_arg`: coded lanes ordered by bytes, no compare under the collation). The DISTINCT / ORDER BY forms sort in the
    // host and keep the refusal.
    let loose = |e: Node<'p>, coll: u32| crate::catalog::expr_type::type_of(e).is_some_and(|t| crate::ops::shared::sort::indeterminate(b.facts, t, coll));
    let sorts = (mods.distinct > 0 || mods.order > 0) && a.args.iter().filter_map(|n| n.as_target_entry()).any(|t| loose(t.expr, crate::catalog::expr_type::collation_of(t.expr)));
    let loose_extreme = matches!(class, Class::Min(_) | Class::Max(_)) && arg.is_some_and(|e| loose(e, a.inputcollid));
    if sorts || (loose_extreme && (mods.distinct > 0 || mods.order > 0)) {
        return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL));
    }
    let aggno = a.aggno as i16;
    // D1811: a DISTINCT aggregate's inputs go to the host's sort as datums (FinalForm::Distinct), so its register is never
    // a code one: a code register with that final kept codes in a state the host never filled (wrong NULLs, D1320's raise)
    // dict item 14: a code register orders a part's codes by bytes (rep.code.order's in-part rank), PG's order only under
    // a collation that orders by bytes (D247); under any other input collation the datums fold through PG's comparator
    // (MinVar / MaxVar, the hosted fold), as a heap text column's do. D767 / OID-19: so does an argument whose type's
    // comparison does not read its stored bytes as they are (typefacts `orders_as_stored`: a pad-insensitive image trims
    // trailing blanks first, so the padded renders' byte order puts 'a  ' after 'a' || chr(9))
    let stored_order = arg.and_then(crate::catalog::expr_type::type_of).and_then(|t| typefacts::facts_of(b.facts, t)).is_none_or(|f| f.image.orders_as_stored());
    let code_arg = !loose_extreme && mods.distinct == 0 && b.facts.coll(a.inputcollid).bytes_order && stored_order && arg.and_then(|e| scan_lane(b, child, strip(child, e))).is_some_and(|l| matches!(l.rep, gravity_program::Rep::Code { .. }));
    let (mut wide, mut spill) = (false, false);
    let (scale, dscales) = match (class, arg) {
        // D297: the packed register only within its digit bound; past it (or unscaled) the Var register, class.num_varlena
        (Class::SumNum | Class::AvgNum | Class::Min(Ext::Num) | Class::Max(Ext::Num), Some(e)) => {
            let sum = matches!(class, Class::SumNum | Class::AvgNum);
            let fits = |d: Digits| if sum { d.packed() } else { d.word() };
            match num_scale(b, child, e).filter(|&(sc, d)| sc <= MAX_NUM_SCALE && fits(d)) {
                Some((sc, d)) => {
                    spill = sum && !d.word();
                    fires::fire("agg.num_packed"); // shapecheck fire signal (REGISTRY DC-06, OID rows)
                    (Some(sc), None)
                }
                // V4R-38: a tree of packed factors times one-word ones, summed and subtracted (OID-04), folds in the i256
                // slot by class.num_wide's register tree
                None if sum && wide_tree(b, child, e).is_some() => {
                    wide = true;
                    fires::fire("agg.num_wide"); // shapecheck fire signal (REGISTRY DC-07, AR-09)
                    (wide_tree(b, child, e), None)
                }
                // D830: any other fixed-scale term under 10^55 (a numeric(30, s) column) spills too: no tree takes it, so
                // lowerv4 folds its datum in class.num_wide (fold/num_carry.rs `datum`; 2^63 terms stay under 2^246)
                None if sum && num_scale(b, child, e).is_some_and(|(sc, d)| sc <= MAX_NUM_SCALE && d.top <= 55 && !d.datum) => {
                    spill = true;
                    fires::fire("agg.num_spill"); // shapecheck fire signal (REGISTRY DC-06)
                    (num_scale(b, child, e).map(|f| f.0), None)
                }
                // D4567 (a): a division by a constant whose quotient is exact at a decided scale (its display scale by
                // row, the dscale word's: numeric.rs `div_form`), while road agg.num_div is on
                None if sum && mods.split == Split::Simple && mods.distinct == 0 && mods.order == 0 && div_form(b, child, e).is_some() => div_form(b, child, e).map_or((None, None), |(sc, d, _)| {
                    fires::fire("agg.num_div"); // shapecheck fire signal (D4567 (a))
                    (Some(sc), Some(d))
                }),
                // a min / max returns one value with its own dscale: only a sum / avg takes the largest and tracks it
                None if sum => case_scales(b, child, e).filter(|c| c.0 <= MAX_NUM_SCALE && c.2.packed()).map_or((None, None), |(sc, d, digits)| {
                    spill = !digits.word();
                    fires::fire("agg.num_case"); // shapecheck fire signal (REGISTRY DC-06)
                    (Some(sc), Some(d))
                }),
                None => (None, None),
            }
        }
        _ => (None, None),
    };
    let (mut reg, mut trans, mut final_) = fold_of(class, fnoid, code_arg, scale.is_some());
    // D3374 (readings_gen_num_digits_quiet: `sum(price * qty ORDER BY id)` over numeric(10,2) x int4): a call's own ORDER
    // BY leaves an order-insensitive class's fold as it is (no OrderedFold below: a numeric sum or average is exact, its
    // dscale the largest), so it takes the wide / spilled slot as the call without one does; a packed register past one
    // word under an ORDER BY reached class.num_varlena with decompose's slot and refused 'over a packed register's slot'
    let unordered = mods.order == 0 || !class.order_sensitive();
    // V4R-38 (B-lite, the user 09-24): a wide product's terms can pass i128 by typmod, so its sum is the i256 slot
    if wide && mods.distinct == 0 && unordered {
        (reg, trans) = if reg == Register::AvgNum { (Register::AvgNumWide, Trans::Abi(SlotKind::AvgNumWide)) } else { (Register::SumNumWide, Trans::Abi(SlotKind::SumNumWide)) };
    } else if spill && mods.distinct == 0 && unordered {
        // D830: a packed term past one word leaves no row headroom in the i128 register (2^63 terms under 10^37 pass
        // 2^126), so its slot is the i256 one: class.num_packed still folds it (its i128 run in the window registers under
        // V4R-68's guard, rows x 10^digits <= 2^125, so the run never wraps between merges), each merge an i256 add
        trans = Trans::Abi(if reg == Register::AvgNum { SlotKind::AvgNumWide } else { SlotKind::SumNumWide });
    } else if scale.is_none() && mods.distinct == 0 && mods.order == 0 && matches!(class, Class::Min(Ext::Num) | Class::Max(Ext::Num)) && arg.is_some_and(|e| (sh.held.is_some() && stored_numeric(b, child, e)) || cte_one_dscale(b, child, e)) && !drains(child) {
        // NEW-numeric-1: the values' order keys in a word slot (class.num_packed's keyed extreme), not MinVar's hosted fold;
        // `settle_keyed` keeps it only where its layout can carry the tie flag (`LAYOUT_KEY_TIES`); it stays off a hash
        // join drain (`drains`), whose merges' order is not shown
        (reg, trans, final_) = match class {
            Class::Min(_) => (Register::MinNumDyn, Trans::Abi(SlotKind::MinI64), FinalForm::Numeric),
            _ => (Register::MaxNumDyn, Trans::Abi(SlotKind::MaxI64), FinalForm::Numeric),
        };
    }
    // interval kinds: the native extremum keeps the later of equal members in fold order; under the call's ORDER BY or
    // DISTINCT PG's member is the later in that order, so those calls keep PG's own fold (the `Var` extremum)
    if (mods.order > 0 || mods.distinct > 0) && matches!(reg, Register::MinInterval | Register::MaxInterval) {
        let min = reg == Register::MinInterval;
        (reg, trans, final_) = (if min { Register::MinVar } else { Register::MaxVar }, Trans::Var, FinalForm::Rendered);
    }
    // interval sums: the native IntervalSum slot folds a plain call; a call under ORDER BY or DISTINCT (DISTINCT keeps
    // the first of equal members in its sort order, and equal intervals print apart) or split into partial and final (PG's
    // IntervalAggState crosses the split) keeps PG's own fold, as before
    if (mods.order > 0 || mods.distinct > 0 || !matches!(mods.split, crate::ops::shared::agg_class::Split::Simple)) && matches!(reg, Register::SumInterval | Register::AvgInterval) {
        trans = Trans::Interval16;
    }
    if mods.distinct > 0 {
        let e = strip(child, arg.unwrap());
        if row_count(b, child, sh, a, node, class, mods, filter, e, distinct_key, k, id)? {
            return Ok(());
        }
        // D1162: another DISTINCT aggregate shares the key only under the same equality: its own collation, or both
        // collations' equal bytes (`t COLLATE p8ci` and `t` are one column but not one set)
        let same_eq = |c: u32| c == a.inputcollid || (b.facts.coll(c).bytes_eq && b.facts.coll(a.inputcollid).bytes_eq);
        let keyed = if *distinct_key == NONE {
            key_kind(b, child, e, a.inputcollid, false, k, id).is_ok()
        } else {
            let key = &sh.keys[*distinct_key as usize];
            same_var(b, child, key.expr, e) && same_eq(key.coll)
        };
        // D2127: the pair set keys by the argument type's `=`; a DISTINCT sorted by another family's operator (an ORDER BY
        // .. USING record_image_ops' *<, whose *= keeps numeric 1.0 and 1.00 apart) drops duplicates by that family's
        // equality, so it folds host-sorted (PG's sort by the clause's sortop and eqop: hosts hosted_distinct.rs `Sorted::of`)
        let own_eq = a.aggdistinct.iter().filter_map(|n| n.as_sort_group_clause()).all(|c| b.facts.key(crate::catalog::KeyReq::sort(c.sortop)).is_some_and(|o| o.default_family));
        let pair = mods.distinct == 1 && class.pair_fold() && keyed && own_eq;
        if pair {
            fires::fire(if class == Class::CountAny { "agg.pair_count" } else { "agg.pair_fold" }); // shapecheck fire signal (REGISTRY DC-08)
            if *distinct_key == NONE {
                *distinct_key = sh.key_expr(b, child, 0, e, a.inputcollid, k, id)?;
                // an anonymous record argument read through a projection that only passes it on (a Sort's column): its
                // columns from the argument as the Agg reads it, over its own input
                let key = &mut sh.keys[*distinct_key as usize];
                if crate::catalog::anonymous_record(key.tf) && key.rec.is_none() {
                    key.rec = arg.and_then(|x| crate::facts::record::key_row(b, id, 0, x, true));
                    if key.rec.is_some_and(|r| crate::facts::record::indeterminate(b.facts, r)) {
                        return Err(keys::indeterminate_coll(k, id));
                    }
                }
            }
            let final_ = if class == Class::CountAny { FinalForm::PairCount } else { FinalForm::PairFold };
            let reg = if class == Class::CountAny { Register::DistinctCount } else { reg };
            sh.aggs.push(AggDecl { aggno, fnoid, reg, slot: NONE, count_slot: NONE, final_, node, scale: NO_SCALE, filter, class, trans: Trans::Var, mods, coll: b.facts.coll(a.inputcollid), dscale_slot: NONE, dscales: &[] });
            return Ok(());
        }
        trans = Trans::Var;
        final_ = FinalForm::Distinct;
    } else if mods.order > 0 && class.order_sensitive() && kind == b'n' {
        trans = Trans::Var;
        final_ = FinalForm::OrderedFold;
    }
    if mods.split.combines() {
        reg = if matches!(class, Class::CountStar | Class::CountAny | Class::CountPair) { Register::SumInt } else { Register::Combine };
    }
    if mods.split.skips_final() {
        final_ = FinalForm::Partial;
    }
    if matches!(trans.abi(), Some(SlotKind::MinI64 | SlotKind::MaxI64 | SlotKind::MaxCode | SlotKind::BitAnd | SlotKind::IntervalMin | SlotKind::IntervalMax | SlotKind::IntervalSum)) {
        sh.nn = false;
    }
    let nn_arg = arg.is_some_and(|e| notnull(b, child, strip(child, e)));
    if arg.is_some() && reg != Register::CountAny && !nn_arg {
        sh.nn = false;
    }
    // D4565: a plain var / stddev over int2 / int4 folds PostgreSQL's transition state in record slots
    // (`stat_slot`), not the hosted state, while its road is on (by default); a split, DISTINCT or ordered call keeps its form
    let stat = stat_slot(reg).filter(|_| final_ == FinalForm::Stat && matches!(trans, Trans::Stat24 | Trans::Stat48) && mods.split == Split::Simple && b.facts.default_on_road_on(STAT_REG_ROAD));
    if let Some(s) = stat {
        trans = Trans::Abi(s);
        fires::fire("agg.stat_reg"); // shapecheck fire signal (D4565)
    }
    // A rendered min / max(text) over codes folds codes per row but its record keeps the extreme's image, merged by
    // bytes (abi `Rec::fold_var`), so records of different parts merge right; a key's representative `MinCode` stays.
    let slot = match (trans.abi(), final_) {
        (Some(SlotKind::MinCode), FinalForm::Rendered) => sh.slot(SlotKind::MinVar, k, id)?,
        (Some(SlotKind::MaxCode), FinalForm::Rendered) => sh.slot(SlotKind::MaxVar, k, id)?,
        (Some(sk), _) => sh.slot(sk, k, id)?,
        (None, _) => NONE,
    };
    // NEW-numeric-1 N13: an order-key extreme's side companion right after its key slot (abi `NumSideMin` / `NumSideMax`):
    // the group's extreme over the values its rows cannot key, folded where they arrive (no raise, no re-run)
    if slot != NONE && matches!(reg, Register::MinNumDyn | Register::MaxNumDyn) {
        sh.slot(if reg == Register::MinNumDyn { SlotKind::NumSideMin } else { SlotKind::NumSideMax }, k, id)?;
    }
    // D4565: an Int128AggState's sumX2, the `SumI128` companion right after its `AvgI128` slot
    if slot != NONE && stat == Some(SlotKind::AvgI128) {
        sh.slot(SlotKind::SumI128, k, id)?;
    }
    let row_count = class == Class::CountStar || (class == Class::CountAny && nn_arg && mods.distinct == 0);
    if slot != NONE && trans.abi() == Some(SlotKind::Count) && row_count && filter.is_none() && sh.row_count == NONE {
        sh.row_count = slot;
    }
    let dscale_slot = match dscales {
        Some(_) if slot != NONE && matches!(reg, Register::SumNum | Register::AvgNum) => {
            sh.nn = false;
            sh.slot(SlotKind::MaxI64, k, id)?
        }
        _ => NONE,
    };
    let dscales = dscales.filter(|_| dscale_slot != NONE).unwrap_or(&[]);
    // an interval average's N is in its IntervalSum slot: no call takes a count slot here
    sh.aggs.push(AggDecl { aggno, fnoid, reg, slot, count_slot: NONE, final_, node, scale: scale.map_or(NO_SCALE, |sc| sc as i16), filter, class, trans, mods, coll: b.facts.coll(a.inputcollid), dscale_slot, dscales });
    Ok(())
}

/// The register of a built-in aggregate whose slot is a frozen `gravity_abi` kind (the classification table's row
/// through `fold_of`; `None` for a user-defined aggregate or a class whose slot kind is a need).
pub fn register(fnoid: u32, code_arg: bool) -> Option<(Register, SlotKind, FinalForm)> {
    let row = aggtab::row(fnoid)?;
    let (reg, trans, final_) = fold_of(row.class, fnoid, code_arg, true);
    Some((reg, trans.abi()?, final_))
}
