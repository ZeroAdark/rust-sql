//! The L2 front key (milestone-2.md §2d, §4 item 1): the program's content computed from the
//! graph before any lowering — the pipeline and site descriptors, the index keys' shape, every
//! expression's structure with its literals as words (the `Sequential` plan's placement), the
//! LIMIT bounds' shape, the output names — so a custom plan of the same shape is an L2 hit.
//! Every field a compiled call binds is in the key (lower.md §14.13): the collations, the operator
//! lists of a row comparison, a VALUES row's members, a cstring constant's bytes.
#![allow(unsafe_code)]

use core::fmt;
use core::hash::{Hash, Hasher};

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::effects::{bit_index_class, divisor_class, numeric_typmod_class, plan_word, sample_class, text_arg_class};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::ops::shared::srf::{SrfKind, SrfPlace};
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{Bound, Facts, IndexKey, Op, PipelineGraph, PipelineNode, Source};
use gravity_expr::percode::{sized_class, textpos_class};
use gravity_program::{ArenaRef, ArenaVec, ContentKey};
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeTag};

struct Fnv128 {
    /// The key's two lanes: the hash home's content identity hash (abi `hash::Fnv128`, a byte at a time).
    h: gravity_abi::hash::Fnv128,
    /// V4R-110: a constant `= ANY` / `IN` list is keyed by its class, never its image (`list`), on a graph whose relations
    /// are all heap tables (a pgrcolumnar per-code table is its list's: keyed by the image there).
    lists: bool,
    /// The depth of island-capable nodes the walk is under (an island's constants are its program's own, D337).
    island: u32,
    /// The image address of the list the walk keyed by its class, whose Const is next (0: none).
    list: usize,
    /// The walk's own scratch (`listed`, `faces`, `words`, `refs` and the walks below): the graph's arena (`walk_graph`), so
    /// the scratch is the admission's, reset with the graph at its scope's end.
    a: ArenaRef,
    /// Every list image keyed by its class so far.
    listed: ArenaVec<usize>,
    /// M4-INSERT (cards/M4.md L0-m): the walk is in a literal table's VALUES lists (`literal_table`): its by-reference
    /// cells are keyed by their class, their images read from the table's words per execution, never compiled.
    lit: bool,
    /// `target_facts` per (targets, leaves) slice pair met so far in this walk.
    faces: ArenaVec<([usize; 4], u128)>,
    /// NEW-72: the pgrcolumnar range pipeline the walk is in: its scan's varno, the attnos of its by-value word lanes and
    /// those of its byte-equality lanes (a memcmp-ordered fixed image, a by-reference text lane): an IN list over one is
    /// keyed by its class, lowerv4 binding its members per execution (verdict_zone.rs `list_of`, `hash_list_of`).
    /// NEW-72 entry C: and the attnos of its code lanes (`KIND_CODES`): an IN list over one by text equality is keyed by its
    /// class too, the per-code road binding its elements per execution (gravity_expr percode `code_set`).
    words: Option<(i32, ArenaVec<i16>, ArenaVec<(i16, bool)>, ArenaVec<i16>)>,
    /// NEW-72 entry B: the image addresses of the pgrcolumnar range pipeline's by-reference comparands lowerv4 binds per
    /// execution (`bound_refs`): each is keyed by its class and logged with the lists, never by its image.
    refs: ArenaVec<usize>,
    /// D3586: the address of the last Append arm's prune info keyed whole (0: none).
    prune: usize,
    /// D4449: the walk's position (the nodes handed to `visit` so far), whether the literal table being walked holds a
    /// cell that is not a Const, and the runs of positions of the tables that hold none (`Runs`).
    pos: u32,
    code: bool,
    tables: Runs,
}

impl Fnv128 {
    /// A hasher whose scratch is arena `a`'s (`ArenaRef::EMPTY` for one that hashes words only and never pushes).
    fn new(lo: u64, hi: u64, a: ArenaRef) -> Fnv128 {
        Fnv128 { h: gravity_abi::hash::Fnv128 { lo, hi }, lists: false, island: 0, list: 0, a, listed: ArenaVec::new_in(a), lit: false, faces: ArenaVec::new_in(a), words: None, refs: ArenaVec::new_in(a), prune: 0, pos: 0, code: false, tables: Runs::NONE }
    }

    /// The 128-bit key: the high lane above the low.
    fn key(&self) -> u128 {
        ((self.h.hi as u128) << 64) | self.h.lo as u128
    }
}

impl Hasher for Fnv128 {
    fn finish(&self) -> u64 {
        self.h.finish()
    }

    fn write(&mut self, bytes: &[u8]) {
        self.h.write(bytes);
    }
}

impl fmt::Write for Fnv128 {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Hasher::write(self, s.as_bytes());
        Ok(())
    }
}

/// Every expression node of the graph in the key's traversal order (the word locators of 2e).
pub type Visit<'a, 'p> = &'a mut dyn FnMut(Node<'p>);

/// A collation as a prefix code: one byte for none (most nodes), five for one — the hasher costs a
/// step per byte, and the key carries a collation on every node that binds one (§14.13 item 2).
fn coll(h: &mut Fnv128, c: u32) {
    if c == 0 {
        0u8.hash(h);
    } else {
        (1u8, c).hash(h);
    }
}

/// A typmod as a prefix code the same way: one byte for -1 (a hash of the plan's typmod, no type derivation).
fn typmod_code(h: &mut Fnv128, t: i32) {
    if t == -1 {
        0u8.hash(h);
    } else {
        (1u8, t).hash(h);
    }
}

fn node<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, n: Node<'p>) {
    crate::stack_check();
    v(n);
    h.pos += 1;
    let tag = n.node_tag();
    // two bytes for the tag and four for the type (0 = none: no type has OID 0) — the hasher
    // costs a step per byte, and every node pays these
    (tag as u16).hash(h);
    gravity_decompose::catalog::expr_type::type_of(n).unwrap_or(0).hash(h);
    let island = gravity_expr::nodes::island_capable(n);
    if island {
        island_consts(h, n);
    }
    match tag {
        // a VALUES row, or an implicit-AND list in an expression's place (a Result's one-time gate): its length and its
        // members, in order (§14.13 item 3: a row was opaque before), walked here only (D4448: `expr::kids` opens a
        // list too since D3882, and walking its members again put each VALUES cell in the key and the walk twice)
        NodeTag::T_List => {
            let l = n.as_list().unwrap();
            (l.len() as u32).hash(h);
            for k in l.iter() {
                // NEW-29: an expression cell of a literal table's row is the program's code: its own Consts are not table cells
                let lit = h.lit;
                h.lit = lit && matches!(k.node_tag(), NodeTag::T_Const | NodeTag::T_List);
                h.code |= lit && k.node_tag() != NodeTag::T_Const;
                node(h, v, k);
                h.lit = lit;
            }
        }
        NodeTag::T_Var => {
            let x = n.as_var().unwrap();
            // D887: RETURNING's old.v and new.v differ only by varreturningtype (the front key's T_Var hashes it too)
            (x.varno, x.varattno, x.vartype, x.vartypmod, x.varlevelsup, x.varreturningtype as u32).hash(h);
            coll(h, x.varcollid);
        }
        NodeTag::T_Const => {
            let c = n.as_const().unwrap();
            (c.consttype, c.consttypmod, c.constisnull, c.constbyval).hash(h);
            coll(h, c.constcollid);
            if !c.constbyval && !c.constisnull {
                if h.list != 0 && h.list == c.constvalue.as_usize() {
                    // a list keyed by its class (`list_class`): its values are words
                    h.list = 0;
                } else if h.lit || h.refs.contains(&c.constvalue.as_usize()) {
                    // a literal table's by-reference cell (`literal_table`), or NEW-72 entry B a pgrcolumnar comparand bound per
                    // execution (`bound_refs`): keyed by its class, logged with the lists, so a lowering that baked it files no
                    // program (`lowering_reads_lists`)
                    h.listed.push(c.constvalue.as_usize());
                } else if c.constlen == -1 {
                    // SAFETY: a non-null by-reference constant of a varlena type points at its image.
                    unsafe { varlena_image(c.constvalue.as_usize() as *const u8) }.hash(h);
                } else if c.constlen == -2 {
                    // SAFETY: a non-null cstring constant points at its NUL-terminated bytes.
                    unsafe { core::ffi::CStr::from_ptr(c.constvalue.as_usize() as *const core::ffi::c_char) }.to_bytes().hash(h);
                } else if c.constlen > 0 {
                    // SAFETY: a fixed-length by-reference constant points at `constlen` bytes.
                    unsafe { core::slice::from_raw_parts(c.constvalue.as_usize() as *const u8, c.constlen as usize) }.hash(h);
                } else {
                    c.constvalue.as_u64().hash(h);
                }
            }
        }
        NodeTag::T_Param => {
            let p = n.as_param().unwrap();
            ((p.paramkind == ParamKind::PARAM_EXTERN) as u8, p.paramid, p.paramtype).hash(h);
            typmod_code(h, p.paramtypmod);
            coll(h, p.paramcollid);
        }
        NodeTag::T_OpExpr => {
            let o = n.as_op_expr().unwrap();
            o.opno.hash(h);
            coll(h, o.opcollid);
            coll(h, o.inputcollid);
            if let Some(q) = divisor_class(o.opfuncid, &o.args, &plan_word) {
                q.hash(h);
            }
            if let Some(q) = textpos_class(o.opfuncid, &o.args) {
                q.hash(h);
            }
            if let Some(q) = sized_class(o.opfuncid, &o.args) {
                q.hash(h);
            }
        }
        NodeTag::T_DistinctExpr => {
            let o = n.as_distinct_expr().unwrap();
            o.opno.hash(h);
            coll(h, o.opcollid);
            coll(h, o.inputcollid);
        }
        NodeTag::T_NullIfExpr => {
            let o = n.as_null_if_expr().unwrap();
            o.opno.hash(h);
            coll(h, o.opcollid);
            coll(h, o.inputcollid);
        }
        NodeTag::T_ScalarArrayOpExpr => {
            let s = n.as_scalar_array_op_expr().unwrap();
            (s.opno, s.useOr, s.hashfuncid != 0).hash(h);
            coll(h, s.inputcollid);
            let lists = h.lists;
            h.lists = lists || word_list(h, s);
            list_class(h, s.args.nth(1));
            h.lists = lists;
            // D3788 (DC-14 over a list): whether every element of a constant pattern list is good, the class decompose's
            // `quiet_pattern_list` reads (exec front.rs `pattern_list_class` holds the same); the list's values stay words
            let coll = s.inputcollid;
            if let Some(q) = gravity_decompose::effects::pattern_list_good(s.opfuncid, &s.args, coll, true, &|pat, icase| gravity_fn::typesem::collation::regex_compiles(pat, icase, coll)) {
                (0xd378u16, q).hash(h);
            }
        }
        NodeTag::T_FuncExpr => {
            let f = n.as_func_expr().unwrap();
            (f.funcid, f.funcvariadic).hash(h);
            coll(h, f.funccollid);
            coll(h, f.inputcollid);
            if let Some(q) = divisor_class(f.funcid, &f.args, &plan_word) {
                q.hash(h);
            }
            if let Some(q) = textpos_class(f.funcid, &f.args) {
                q.hash(h);
            }
            if let Some(q) = series_step(f.funcid, &f.args) {
                q.hash(h);
            }
            if let Some(q) = text_arg_class(f.funcid, &f.args, &plan_word) {
                q.hash(h);
            }
            if let Some(q) = bit_index_class(f.funcid, &f.args, &plan_word) {
                q.hash(h);
            }
            if let Some(q) = numeric_typmod_class(f.funcid, &f.args, &plan_word) {
                q.hash(h);
            }
        }
        NodeTag::T_Aggref => {
            let a = n.as_aggref().unwrap();
            (a.aggfnoid, a.aggstar, a.aggkind, a.aggdistinct.len() as u32, a.aggorder.len() as u32).hash(h);
            coll(h, a.aggcollid);
            coll(h, a.inputcollid);
        }
        NodeTag::T_WindowFunc => {
            let w = n.as_window_func().unwrap();
            (w.winfnoid, w.winref, w.winstar, w.winagg).hash(h);
            coll(h, w.wincollid);
            coll(h, w.inputcollid);
        }
        NodeTag::T_BoolExpr => (n.as_bool_expr().unwrap().boolop as u32).hash(h),
        NodeTag::T_NullTest => {
            let t = n.as_null_test().unwrap();
            (t.nulltesttype as u32, t.argisrow).hash(h);
        }
        NodeTag::T_BooleanTest => (n.as_boolean_test().unwrap().booltesttype as u32).hash(h),
        NodeTag::T_RelabelType => {
            let r = n.as_relabel_type().unwrap();
            r.resulttype.hash(h);
            typmod_code(h, r.resulttypmod);
            coll(h, r.resultcollid);
        }
        NodeTag::T_CoerceViaIO => {
            let c = n.as_coerce_via_io().unwrap();
            c.resulttype.hash(h);
            coll(h, c.resultcollid);
        }
        NodeTag::T_ArrayCoerceExpr => {
            let a = n.as_array_coerce_expr().unwrap();
            a.resulttype.hash(h);
            typmod_code(h, a.resulttypmod);
            coll(h, a.resultcollid);
        }
        // the emitted program carries the domain's constraints (NOT NULL, each CHECK's expression): ALTER DOMAIN
        // changes them under an unchanged tree, so the set is in the key (a type cache miss keys as no constraints)
        NodeTag::T_CoerceToDomain => {
            let d = n.as_coerce_to_domain().unwrap();
            d.resulttype.hash(h);
            typmod_code(h, d.resulttypmod);
            coll(h, d.resultcollid);
            // the argument first, then the checks: the order of the compile's literals (tables.rs::needs_of)
            node(h, v, d.arg);
            // D3551: and the names its errors carry, as the compile bakes them (ALTER DOMAIN RENAME / SET SCHEMA)
            gravity_expr::nodes::cast::domain_key_names(d.resulttype).hash(h);
            let facts = gravity_expr::catalog::domain_facts(d.resulttype).ok();
            let cons = facts.as_ref().map_or(&[][..], |f| &f.constraints[..]);
            (cons.len() as u32).hash(h);
            for c in cons {
                match c {
                    gravity_expr::catalog::DomainConstraint::NotNull => 0u8.hash(h),
                    gravity_expr::catalog::DomainConstraint::Check { name, expr } => {
                        1u8.hash(h);
                        name.hash(h);
                        node(h, v, *expr);
                    }
                }
            }
            return;
        }
        // the emitted fetch binds the container's and the element's facts and which bounds are there (an open
        // slice bound is no child: `a[2:]` and `a[:2]` have the same children)
        NodeTag::T_SubscriptingRef => {
            let r = n.as_subscripting_ref().unwrap();
            (r.refcontainertype, r.refelemtype, r.refassgnexpr.is_some()).hash(h);
            typmod_code(h, r.reftypmod);
            coll(h, r.refcollid);
            for l in [&r.refupperindexpr, &r.reflowerindexpr] {
                (l.len() as u32).hash(h);
                for e in l.iter() {
                    e.is_some().hash(h);
                }
            }
        }
        NodeTag::T_ConvertRowtypeExpr => n.as_convert_rowtype_expr().unwrap().resulttype.hash(h),
        NodeTag::T_CollateExpr => coll(h, n.as_collate_expr().unwrap().collOid),
        NodeTag::T_FieldSelect => {
            let f = n.as_field_select().unwrap();
            (f.fieldnum, f.resulttype).hash(h);
            typmod_code(h, f.resulttypmod);
            coll(h, f.resultcollid);
        }
        // the emitted store binds the row type and which fields it writes (the new values are the children)
        NodeTag::T_FieldStore => {
            let s = n.as_field_store().unwrap();
            (s.resulttype, s.fieldnums.len() as u32).hash(h);
            s.fieldnums.iter().for_each(|f| f.hash(h));
        }
        NodeTag::T_CoalesceExpr => coll(h, n.as_coalesce_expr().unwrap().coalescecollid),
        NodeTag::T_CaseExpr => coll(h, n.as_case_expr().unwrap().casecollid),
        NodeTag::T_ArrayExpr => {
            let a = n.as_array_expr().unwrap();
            (a.element_typeid, a.multidims).hash(h);
            coll(h, a.array_collid);
        }
        NodeTag::T_RowExpr => {
            let r = n.as_row_expr().unwrap();
            (r.row_typeid, r.colnames.len() as u32).hash(h);
            // an anonymous record's descriptor (its registered typmod) is built from the column names
            for c in r.colnames.iter() {
                c.as_string().map(|s| s.sval).hash(h);
            }
        }
        NodeTag::T_RowCompareExpr => {
            let r = n.as_row_compare_expr().unwrap();
            r.cmptype.hash(h);
            for o in r.opnos.iter() {
                o.hash(h);
            }
            for c in r.inputcollids.iter() {
                coll(h, c);
            }
        }
        NodeTag::T_MinMaxExpr => {
            let m = n.as_min_max_expr().unwrap();
            (m.op as u32).hash(h);
            coll(h, m.minmaxcollid);
            coll(h, m.inputcollid);
        }
        NodeTag::T_XmlExpr => {
            let x = n.as_xml_expr().unwrap();
            (x.op as u32, x.xmloption as u32, x.indent, x.r#type, x.typmod).hash(h);
            x.name.hash(h);
            for a in x.arg_names.iter() {
                a.as_string().map(|s| s.sval).hash(h);
            }
        }
        NodeTag::T_JsonExpr => {
            let j = n.as_json_expr().unwrap();
            (j.op as u32, j.wrapper as u32, j.omit_quotes, j.use_io_coercion, j.use_json_coercion).hash(h);
            coll(h, j.collation);
            j.column_name.hash(h);
            j.returning.map(|r| (r.typid, r.typmod)).hash(h);
            for a in j.passing_names.iter() {
                a.as_string().map(|s| s.sval).hash(h);
            }
        }
        NodeTag::T_JsonBehavior => {
            let b = n.as_json_behavior().unwrap();
            (b.btype as u32, b.coerce).hash(h);
        }
        NodeTag::T_JsonIsPredicate => {
            let p = n.as_json_is_predicate().unwrap();
            (p.item_type as u32, p.unique_keys).hash(h);
        }
        NodeTag::T_JsonConstructorExpr => {
            let c = n.as_json_constructor_expr().unwrap();
            (c.r#type as u32, c.absent_on_null, c.unique).hash(h);
            c.returning.map(|r| (r.typid, r.typmod)).hash(h);
        }
        // D1101 (D1099's audit, main 10:17Z): the precision is the program's (gravity_expr bakes it), so it keys the content:
        // CURRENT_TIMESTAMP(0) after CURRENT_TIMESTAMP(3) ran the precision-3 program (the front key hashed it already)
        NodeTag::T_SQLValueFunction => {
            let f = n.as_sql_value_function().unwrap();
            (f.op as u32, f.typmod).hash(h);
        }
        NodeTag::T_SubPlan => {
            let s = n.as_sub_plan().unwrap();
            s.plan_id.hash(h);
            coll(h, s.firstColCollation);
        }
        // D422: the cursor's form, never its name (a word bound per execution, `WordSrc::Cursor`)
        NodeTag::T_CurrentOfExpr => {
            let c = n.as_current_of_expr().unwrap();
            (c.cvarno, c.cursor_param, c.cursor_name.is_some()).hash(h);
        }
        // V4R-337: RETURNING's PG 18 ReturningExpr (a view's or a virtual generated column's OLD / NEW expression): its
        // image and level, and its expression (`expr::kids` does not open it), so OLD / NEW and two expressions key apart
        // and its literals are words where the tree has them
        NodeTag::T_ReturningExpr => {
            let r = n.as_returning_expr().unwrap();
            (r.retlevelsup, r.retold).hash(h);
            node(h, v, r.retexpr);
        }
        NodeTag::T_TargetEntry => {
            let t = n.as_target_entry().unwrap();
            (t.resno, t.resjunk).hash(h);
            t.resname.hash(h);
        }
        _ => {}
    }
    if tag == NodeTag::T_List {
        return;
    }
    h.island += island as u32;
    gravity_decompose::expr::kids(n, &mut |k| node(h, v, k));
    h.island -= island as u32;
}

/// V4R-110: `value`, a constant list (an `= ANY` / `IN` array, an index key's), keyed by its class (gravity_expr konst.rs
/// `list_class`: its count to MAX_LIST, one class past), the shape its words take (the lowerer reads nothing else of it:
/// an element is a word, a longer list or an index key's array one word, lowerv4 lib.rs files no program that read its
/// image); the Const that follows hashes no image. Off `lists`, under an island or where `list_len` cannot read the
/// count, the Const hashes its image as any by-reference literal.
fn list_class(h: &mut Fnv128, value: Node<'_>) {
    let Some(c) = value.as_const().filter(|c| h.lists && h.island == 0 && !c.constbyval && !c.constisnull) else { return };
    let image = c.constvalue.as_usize();
    // SAFETY: a non-null by-reference constant points at its image.
    if let Some(n) = unsafe { gravity_expr::nodes::konst::list_len(image) } {
        gravity_expr::nodes::konst::list_class(n).hash(h);
        fires::fire("key.list_class");
        h.list = image;
        h.listed.push(image);
    }
}

/// D1108 row 1 (V4R-541): an int4 / int8 `generate_series(start, stop, step)`'s step class, `Some(nonzero)` over a non-null
/// by-value constant step (a zero step raises; any other never does). lowerv4 spool_quiet.rs reads it and bakes the eager
/// fill of a Material over the series (run up front, even under an empty outer), so both keys hold the class, never the
/// literal (exec front.rs keys it too; it keys no FunctionScan today), as D1103: a zero step right after a step of 5 ran
/// the eager program, raising where PostgreSQL never runs the Material's child. The step form is decompose srf.rs
/// `kind_of`'s, the one map of the series functions lowerv4 reads too (OID-18: no OID list of its own).
pub fn series_step(funcid: u32, args: &types_nodes::list::NodeList<'_>) -> Option<bool> {
    use gravity_decompose::ops::srf::kind_of;
    use gravity_decompose::ops::shared::srf::SrfKind;
    let step = args.iter().nth(2).and_then(|a| a.as_const()).filter(|c| !c.constisnull && c.constbyval)?;
    let stepped = matches!(kind_of(funcid, true), SrfKind::SeriesInt { step: true, .. });
    (stepped && args.len() == 3).then(|| step.constvalue.as_usize() != 0)
}

/// The by-value constant VALUES under a node that may compile to an interpreter island (gravity_expr
/// `island_capable`, the predicate `Builder::island` obeys; D337): they are the compiled program's own Const nodes,
/// not parameter words, so two statements differing only there differ here.
fn island_consts(h: &mut Fnv128, root: Node<'_>) {
    gravity_decompose::expr::walk(root, &mut |n| {
        if let Some(c) = n.as_const() {
            if c.constbyval && !c.constisnull {
                c.constvalue.as_u64().hash(h);
            }
        }
        true
    });
}

/// A varlena constant's whole image by its header (1-byte short, 4-byte long, a toast pointer's tag
/// byte alone): the planner's constants are never detoasted, so the reader takes the header as is.
unsafe fn varlena_image<'a>(p: *const u8) -> &'a [u8] {
    let b0 = *p;
    let len = if b0 == 0x01 {
        1
    } else if b0 & 0x01 == 0x01 {
        (b0 >> 1) as usize
    } else {
        (u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize
    };
    core::slice::from_raw_parts(p, len.max(1))
}

fn bound<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, b: Bound<'p>) {
    match b {
        Bound::All => 0u8.hash(h),
        // a LIMIT constant folds to an immediate (no node to locate a word by): its value is the shape's
        Bound::Const(n) => (1u8, n).hash(h),
        Bound::Expr(n) => {
            2u8.hash(h);
            node(h, v, n);
        }
    }
}

/// Every field of every index key (an exhaustive destructure: a field added later does not build until it is hashed here).
/// `col` too: an expression column's `attno` is 0, so two keys on two expression columns of one index differ only there.
fn keys<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, keys: &[IndexKey<'p>]) {
    for k in keys {
        let IndexKey { attno, col, opno, value, form } = *k;
        (attno, col, opno, form as u8).hash(h);
        // an `= ANY` key's constant array: a key word at the seek (V4R-108), keyed by its class (V4R-110)
        if matches!(form, gravity_decompose::graph::KeyForm::Any | gravity_decompose::graph::KeyForm::AmAny) {
            list_class(h, value);
        }
        node(h, v, value);
    }
}

fn bitmap<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, p: &BitmapPlan<'p>) {
    crate::stack_check();
    match p {
        // D2918: the bitmap's index scans by their plan node ids too (exec sites_heap.rs reads each one's quals by id)
        BitmapPlan::Index { node_id, indexid, keys: ks, .. } => {
            (0u8, *indexid, *node_id).hash(h);
            keys(h, v, ks);
        }
        BitmapPlan::And { node_id, plans } | BitmapPlan::Or { node_id, plans } => {
            (if matches!(p, BitmapPlan::And { .. }) { 1u8 } else { 2u8 }, plans.len(), *node_id).hash(h);
            for q in plans.iter() {
                bitmap(h, v, q);
            }
        }
    }
}

fn dumped(h: &mut Fnv128, f: &dyn Fn(&mut dyn fmt::Write) -> fmt::Result) {
    let _ = f(h);
}

/// `p` keyed with its lists by class only while it and every pipeline around it read heap tables (`heap_source`).
fn pipeline<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, p: &PipelineNode<'p>) {
    crate::stack_check();
    let outer = h.lists;
    h.lists = outer && heap_source(p);
    let words = h.words.take();
    h.words = match &p.source {
        Source::Range { site, am: gravity_decompose::Am::Pgrcolumnar, scanrelid, .. } => {
            let word = |l: &&gravity_program::LaneDesc| l.kind == gravity_abi::ctx::lane::KIND_BYVAL && !matches!(l.rep, gravity_program::Rep::PackedNum { .. });
            // a code lane's IN list stays keyed by its image: its per-code table holds the members (dict percode `Eq`)
            let fixed = |l: &gravity_program::LaneDesc| matches!(l.rep, gravity_program::Rep::FixedPtr { .. });
            let probe = |l: &&gravity_program::LaneDesc| fixed(l) || l.kind == gravity_abi::ctx::lane::KIND_BYREF;
            let attr = |l: &gravity_program::LaneDesc| match l.id { gravity_program::LaneId::Attr(a) => Some(a), _ => None };
            let (mut attnos, mut probes, mut codes) = (ArenaVec::new_in(h.a), ArenaVec::new_in(h.a), ArenaVec::new_in(h.a));
            site.lanes().iter().filter(word).filter_map(attr).for_each(|a| attnos.push(a));
            site.lanes().iter().filter(probe).filter_map(|l| Some((attr(l)?, fixed(l)))).for_each(|p| probes.push(p));
            site.lanes().iter().filter(|l| l.kind == gravity_abi::ctx::lane::KIND_CODES).filter_map(attr).for_each(|a| codes.push(a));
            Some((*scanrelid as i32, attnos, probes, codes))
        }
        _ => None,
    };
    let refs = core::mem::replace(&mut h.refs, ArenaVec::new_in(h.a));
    if let Source::Range { site, am: gravity_decompose::Am::Pgrcolumnar, scanrelid, .. } = &p.source {
        h.refs = bound_refs(p, *scanrelid as i32, site.lanes(), h.a);
    }
    pipeline_body(h, v, p);
    h.lists = outer;
    h.words = words;
    h.refs = refs;
}

/// NEW-72 entry B: the image addresses of the by-reference constants pgrcolumnar range pipeline `p` (scan `varno`, lanes
/// `lanes`) binds per execution: `col op c` as a top-level conjunct of its filter, `col` a memcmp-ordered fixed lane
/// (`Rep::FixedPtr`: uuid, macaddr, macaddr8) of range table entry `varno`, `op` its type's btree comparison (decompose
/// `same_type_zone_op`), `c` a non-NULL constant of that type: lowerv4's zone predicate binds its order key and its probe
/// word per execution (verdict_zone.rs `byref_of`, `hash_of`) and the filter reads it as a word, so the key holds its class.
/// Entry C: and the text comparands its code lanes' per-code tables read per execution (`bound_text_refs`). A lowering
/// that reads one anyway files no program (`lowering_reads_lists`).
fn bound_refs(p: &PipelineNode<'_>, varno: i32, lanes: &[gravity_program::LaneDesc], a: ArenaRef) -> ArenaVec<usize> {
    use gravity_decompose::ops::shared::scan::same_type_zone_op;
    let fixed = |attno: i16| {
        lanes.iter().find_map(|l| match (&l.id, &l.rep) {
            (gravity_program::LaneId::Attr(a), gravity_program::Rep::FixedPtr { len }) if *a == attno => Some(*len),
            _ => None,
        })
    };
    let fixed_ref = |q: Node<'_>| -> Option<usize> {
        let o = q.as_op_expr().filter(|o| o.args.len() == 2)?;
        let (_, typ) = same_type_zone_op(o.opno)?;
        let len = gravity_fn::typesem::pgrc::memcmp_fixed(typ)?;
        let col = |n: Node<'_>| n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && fixed(v.varattno) == Some(len)).is_some();
        let (a0, a1) = (o.args.nth(0), o.args.nth(1));
        let c = match (col(a0), col(a1)) {
            (true, false) => a1.as_const(),
            (false, true) => a0.as_const(),
            _ => None,
        };
        c.filter(|c| !c.constbyval && !c.constisnull && c.consttype == typ && c.constlen == len as i32).map(|c| c.constvalue.as_usize())
    };
    let mut codes = ArenaVec::new_in(a);
    lanes.iter().filter(|l| l.kind == gravity_abi::ctx::lane::KIND_CODES).filter_map(|l| match l.id { gravity_program::LaneId::Attr(a) => Some(a), _ => None }).for_each(|a| codes.push(a));
    let mut refs = ArenaVec::new_in(a);
    let mut qual = |q: Node<'_>| {
        if let Some(r) = fixed_ref(q) {
            refs.push(r);
        }
        bound_text_refs(q, varno, &codes, &mut refs);
    };
    for op in p.ops.iter() {
        // a filter is one qual or the implicit AND list of them (lowerv4 ops/scan/pgrc/lower.rs `qual_node`)
        let Op::Filter(q) = op else { continue };
        match q.as_list() {
            Some(l) => l.iter().for_each(&mut qual),
            None => qual(*q),
        }
    }
    refs
}

/// NEW-72 entry C: the text constants of qual `q` (through its AND / OR / NOT, the per-code road's roots) that a
/// pgrcolumnar code lane's per-code table reads per execution: `col op c`, `col` a code lane of range table entry `varno`
/// (`codes`; under a relabel that keeps the word), `c` a non-null varlena constant, `op` a byte equality or byte-order
/// compare the per-code road binds (gravity_expr percode `bound_text_compare`: `code_set`'s and `code_range`'s
/// `Known::EqBound` / `Known::RangeBound`), not `''`. Each image address goes to `refs`: the key holds its class, a lowering
/// that reads it anyway files no program (`lowering_reads_lists`).
fn bound_text_refs(q: Node<'_>, varno: i32, codes: &[i16], refs: &mut ArenaVec<usize>) {
    crate::stack_check();
    if let Some(b) = q.as_bool_expr() {
        b.args.iter().for_each(|a| bound_text_refs(a, varno, codes, refs));
        return;
    }
    let Some(o) = q.as_op_expr().filter(|o| o.args.len() == 2) else { return };
    let (a0, a1) = (o.args.nth(0), o.args.nth(1));
    let c = match (code_col(a0, varno, codes), code_col(a1, varno, codes)) {
        (Some(_), None) => a1.as_const(),
        (None, Some(_)) => a0.as_const(),
        _ => None,
    };
    // `''` stays keyed by its image: the per-code road builds its set as before (gravity_expr percode `code_set`)
    if let Some(c) = c.filter(|c| !c.constbyval && !c.constisnull && c.constlen == -1 && gravity_decompose::expr::const_text_empty(c) == Some(false)) {
        if gravity_expr::percode::bound_text_compare(o.opfuncid, o.inputcollid) {
            refs.push(c.constvalue.as_usize());
        }
    }
}

/// NEW-72 entry C: `n` as a code lane of range table entry `varno` (`codes`), bare or under a relabel that keeps its word
/// (gravity_expr cast.rs `relabel_canon`: the per-code road's `bare_var`): its attno.
fn code_col(n: Node<'_>, varno: i32, codes: &[i16]) -> Option<i16> {
    let (n, to) = match n.as_relabel_type() {
        Some(r) => (r.arg, Some(r.resulttype)),
        None => (n, None),
    };
    let v = n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && codes.contains(&v.varattno))?;
    match to {
        Some(t) if gravity_expr::nodes::cast::relabel_canon(v.vartype, t) != gravity_expr::nodes::cast::Canon::Keep => None,
        _ => Some(v.varattno),
    }
}

/// NEW-72: `s` is `col = ANY (array)` (`useOr`) over a lane of the pgrcolumnar range pipeline the walk is in: a by-value
/// word lane under a zone `=` (the column or its integer widening), a memcmp-ordered fixed lane under its btree `=`, or a
/// by-reference text lane under text `=`, or (entry C) `col = ANY` / `col <> ALL` over a code lane by text equality
/// (gravity_expr percode `bound_text_set`): lowerv4 reads its members per execution (verdict_zone.rs `list_of`,
/// `hash_list_of`, the per-code table's key cells, the filter's own elements), so the list is keyed by its class as a heap
/// pipeline's is (V4R-110); a lowering that read its image anyway files no program (`lowering_reads_lists`).
fn word_list(h: &Fnv128, s: &types_nodes::primnodes::ScalarArrayOpExpr<'_>) -> bool {
    use gravity_decompose::ops::shared::scan::{same_type_zone_op, text_zone_op, zone_op};
    use gravity_program::ZoneOp::Eq;
    let Some((varno, attnos, probes, codes)) = h.words.as_ref() else { return false };
    if s.args.len() != 2 {
        return false;
    }
    // NEW-72 entry C: a code lane's `IN` / `NOT IN` by text equality (its relabel looked through, as the per-code road's)
    if code_col(s.args.nth(0), *varno, codes).is_some() {
        return gravity_expr::percode::bound_text_set(s.opfuncid, s.useOr, s.inputcollid);
    }
    if !s.useOr {
        return false;
    }
    let n = s.args.nth(0);
    let n = match n.as_func_expr() {
        Some(f) if f.args.len() == 1 && types_core::quiet::int_widen(f.funcid) => f.args.nth(0),
        _ => n,
    };
    let Some(v) = n.as_var().filter(|v| v.varno == *varno && v.varlevelsup == 0) else { return false };
    // a by-value word lane under a zone `=`; a byte-equality lane under its type's `=` (a fixed image's btree equality,
    // text's): lowerv4 reads no member of either (its probe words are the execution's)
    let probe = probes.iter().find(|p| p.0 == v.varattno).map(|p| p.1);
    (attnos.contains(&v.varattno) && zone_op(s.opno) == Some(Eq))
        || match probe {
            Some(true) => same_type_zone_op(s.opno).is_some_and(|o| o.0 == Eq),
            Some(false) => text_zone_op(s.opno) == Some(Eq),
            None => false,
        }
}

/// KEY-PREDICTED-ROWS road (b), the GUARDED KEY (main 2026-10-04T06:30:47Z): the content key holds no planner row
/// estimate (`Pipeline::predicted_rows`, `build_rows`), or every statement whose estimate drifts (a growing or
/// rolled-back-into table, ANALYZE, autovacuum) would lower again at each execution. A lowering decision that reads one
/// is made through `decide`, which files it as a guard of the program (`guarded`): a filed program serves another
/// estimate only where each guard's decision comes out as it did (`guards_hold`), else the statement lowers again.
/// `decide` re-runs the decision itself on the fresh estimate, so a guard never assumes a decision is monotone.
pub type Decide = fn(u64, &[u64; 4]) -> u64;

/// One estimate-dependent lowering decision: `decide(rows, args)` was `want` for graph pipeline `pipeline`'s estimate
/// `est` (its `predicted_rows`, `build_rows` or `end_rate`); with `lit` other than `NOT_LIT`, a literal's: `decide(word, args)`
/// was `want` for the word of the literal at that position of the key's walk (`lit_decide`, `place_lits`; `at`: its node's
/// address in the lowered statement); with `COST`, a planner-cost decision (`cost_decide`); with `REL`, a relation's size
/// bound's (`rel_decide`; `at`: its OID).
#[derive(Clone, Copy, Debug)]
pub struct EstGuard {
    pub pipeline: u16,
    pub est: Est,
    pub lit: u32,
    pub at: usize,
    pub decide: Decide,
    pub args: [u64; 4],
    pub want: u64,
}

/// The estimate of a graph pipeline an `EstGuard` decides on (NEW-48: the third, `end_rate`, decides an end's carry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Est {
    /// `Pipeline::predicted_rows`.
    Rows,
    /// `Pipeline::build_rows`.
    Build,
    /// `Pipeline::end_rate`.
    EndRate,
}

impl Est {
    fn of(self, p: &gravity_program::Pipeline) -> u64 {
        match self {
            Est::Rows => p.predicted_rows,
            Est::Build => p.build_rows,
            Est::EndRate => p.end_rate,
        }
    }
}

/// `EstGuard::lit` of an estimate decision.
pub const NOT_LIT: u32 = u32::MAX;

/// `EstGuard::lit` of a literal no key-walk position names (not placed, or outside the walk): no other statement's
/// literal is known to decide alike, so the guard holds for none and the program serves no other statement.
const LIT_LOST: u32 = u32::MAX - 1;

/// `EstGuard::lit` of a decision on a relation's rows bounded by its pages now (`rel_decide`): `at` holds its OID.
const REL: u32 = u32::MAX - 3;

// Lowering-scoped (open only inside `guarded`, restored on drop; never session state): non-session TLS, classified in
// the TLS census pin of backend/access/session/src/tests.rs (733 -> 734).
leader_tls::leader_tls! {
    thread_local! {
        /// The guards of the lowering `guarded` runs (none open: `decide` files nothing, e.g. a shadow check's lowering).
        static GUARDS: core::cell::RefCell<Option<Vec<EstGuard>>> = const { core::cell::RefCell::new(None) };
    }
}

/// Restores the enclosing `guarded` scope's list, on return and on an unwind (a PgError raised inside the lowering).
struct GuardScope(Option<Vec<EstGuard>>);

impl Drop for GuardScope {
    fn drop(&mut self) {
        let outer = self.0.take();
        GUARDS.with(|g| *g.borrow_mut() = outer);
    }
}

/// `lower()` with its estimate decisions collected: the guards the program it returns is filed with.
pub fn guarded<T>(lower: impl FnOnce() -> T) -> (T, Vec<EstGuard>) {
    let scope = GuardScope(GUARDS.with(|g| g.replace(Some(Vec::new())))); // alloc: Cold
    let r = lower();
    let mine = GUARDS.with(|g| g.borrow_mut().take()).unwrap_or_default();
    drop(scope);
    (r, mine)
}

/// An estimate-dependent decision of graph pipeline `pipeline` on its estimate `rows` (`build`: its `build_rows`):
/// `decide(rows, &args)`, filed as a guard of the lowering `guarded` runs. `usize::MAX`: a pipeline outside the graph's
/// list (a nest inner's, lowerv4 props/ops.rs): decided, never filed.
pub fn decide(pipeline: usize, build: bool, rows: u64, decide: Decide, args: [u64; 4]) -> u64 {
    decide_on(pipeline, if build { Est::Build } else { Est::Rows }, rows, decide, args)
}

/// NEW-48: `decide` on graph pipeline `pipeline`'s `end_rate` (`rate`, the rows reaching its end per innermost-loop
/// iteration), filed as a guard of the lowering like a rows decision.
pub fn decide_end_rate(pipeline: usize, rate: u64, decide: Decide, args: [u64; 4]) -> u64 {
    decide_on(pipeline, Est::EndRate, rate, decide, args)
}

fn decide_on(pipeline: usize, est: Est, value: u64, decide: Decide, args: [u64; 4]) -> u64 {
    let want = decide(value, &args);
    if pipeline > u16::MAX as usize {
        return want;
    }
    file(EstGuard { pipeline: pipeline as u16, est, lit: NOT_LIT, at: 0, decide, args, want });
    want
}

fn file(g: EstGuard) {
    GUARDS.with(|l| {
        if let Some(v) = l.borrow_mut().as_mut() {
            let same = |x: &EstGuard| x.pipeline == g.pipeline && x.est == g.est && x.lit == g.lit && x.at == g.at && x.args == g.args && x.want == g.want && x.decide as usize == g.decide as usize;
            if !v.iter().any(same) {
                v.push(g); // alloc: Cold
            }
        }
    });
}

/// `EstGuard::lit` of a planner-cost decision (`cost_decide`): decided again on a graph's costs, never on a literal.
const COST: u32 = u32::MAX - 2;

/// Whether guard `g` decides on a literal (`lit_decide`), not on an estimate, a cost or a relation's pages.
pub fn literal(g: &EstGuard) -> bool {
    g.lit != NOT_LIT && g.lit != COST && g.lit != REL
}

/// The PARAM_EXEC ids a cost guard holds (`EstGuard::args`, two per word after the scan's node).
pub const COST_IDS: usize = 6;

/// PS1-05 (mechanism C): which of `ids` (PARAM_EXEC ids, in the scan's order) pgrc range scan node `node` runs at its
/// open, on `graph`'s planner costs (decompose `PipelineGraph::eager_initplans`, the rule `eager_mask`), filed as a guard
/// of the lowering `guarded` runs. A planner cost is an estimate, so a program serves another statement of its key only
/// where that statement's costs decide alike: `guards_hold` / `variant` on its graph, `costs_hold` on a front hit's plan
/// (the front key holds no cost). An id past `COST_IDS` stays lazy.
pub fn cost_decide(graph: &PipelineGraph<'_>, node: i32, ids: &[i32]) -> u64 {
    let ids = &ids[..ids.len().min(COST_IDS)];
    let mut args = [node as u32 as u64 | (ids.len() as u64) << 32, 0, 0, 0];
    for (i, &id) in ids.iter().enumerate() {
        args[1 + i / 2] |= (id as u32 as u64) << (32 * (i % 2));
    }
    let want = graph.eager_initplans(node, ids);
    file(EstGuard { pipeline: 0, est: Est::Rows, lit: COST, at: 0, decide: as_decided, args, want });
    want
}

/// A cost guard's `decide`: the mask `eager_mask` gives is the outcome.
fn as_decided(mask: u64, _: &[u64; 4]) -> u64 {
    mask
}

/// Cost guard `g`'s scan node and PARAM_EXEC ids (`cost_decide`'s `args`), the ids in `buf`.
fn cost_args<'b>(g: &EstGuard, buf: &'b mut [i32; COST_IDS]) -> (i32, &'b [i32]) {
    let n = ((g.args[0] >> 32) as usize).min(COST_IDS);
    for (i, id) in buf.iter_mut().enumerate() {
        *id = (g.args[1 + i / 2] >> (32 * (i % 2))) as u32 as i32;
    }
    (g.args[0] as u32 as i32, &buf[..n])
}

/// What cost guard `g` decides on `graph`'s costs.
fn cost_outcome(g: &EstGuard, graph: &PipelineGraph<'_>) -> u64 {
    let mut buf = [0; COST_IDS];
    let (node, ids) = cost_args(g, &mut buf);
    (g.decide)(graph.eager_initplans(node, ids), &g.args)
}

/// Whether every cost guard of `guards` decides as it did on a front hit's plan, which binds without a graph: `scans`, each
/// range scan's `scan_charge` by plan node id, and `inits`, the statement's InitPlans (`InitCharge::of`), both read off
/// the plan by the front walk (exec inputs/front.rs). They are what decompose's graph holds for one plan, so this matches
/// `guards_hold` there.
pub fn costs_hold(guards: &[EstGuard], scans: &[(i32, f64)], inits: &[gravity_decompose::graph::InitCharge<'_>]) -> bool {
    guards.iter().filter(|g| g.lit == COST).all(|g| {
        let mut buf = [0; COST_IDS];
        let (node, ids) = cost_args(g, &mut buf);
        let scan = scans.iter().find(|c| c.0 == node).map(|c| c.1);
        (g.decide)(gravity_decompose::graph::eager_mask(scan, inits, ids), &g.args) == g.want
    })
}

/// A lowering decision on a non-NULL by-value literal's value (D2847 / D2848's class: the content key holds that literal
/// as a word, so another statement of the key may carry another value there): `decide(word, &args)`, filed as a guard of
/// the lowering `guarded` runs like an estimate decision. The admission names the literal by its position in the key's
/// walk (`place_lits`); a filed program serves another statement of its key only where `decide` comes out alike on that
/// statement's literal there (`guards_hold`, `lits_hold`), else the program filed for that outcome (`variant`), else the
/// statement lowers again. `None` when `at` is no non-NULL by-value Const (the key holds NULL-ness and the type).
pub fn lit_decide(at: Node<'_>, decide: Decide, args: [u64; 4]) -> Option<u64> {
    let want = decide(lit_word(at)?, &args);
    file(EstGuard { pipeline: 0, est: Est::Rows, lit: LIT_LOST, at: at.as_raw().as_ptr() as usize, decide, args, want });
    Some(want)
}

/// PS1-21 / PS1-26 step 1b: a lowering decision on relation `relid`'s rows bounded from above by its pages now (`rows`,
/// `Facts::rel_rows_bound`): `decide(rows, &args)`, filed as a guard of the lowering `guarded` runs. A heap's pages grow
/// with no invalidation (an INSERT extends it; the plan, the content key and the front key stay), so the guard is decided
/// again on the relation's pages whenever its program is to run: at every admission (`guards_hold`, `variant`, and
/// `rels_hold` on a front hit) and at every serve of an admission the plan memo holds (exec `Backend::serve_stale`). A
/// program whose decision comes out otherwise is not run: the statement takes the program filed for that outcome, else
/// lowers again.
pub fn rel_decide(relid: u32, rows: u64, decide: Decide, args: [u64; 4]) -> u64 {
    let want = decide(rows, &args);
    file(EstGuard { pipeline: 0, est: Est::Rows, lit: REL, at: relid as usize, decide, args, want });
    want
}

/// What relation guard `g` decides on its relation's pages now (`None`: no bound, the relation is gone or not a heap).
fn rel_outcome(g: &EstGuard, facts: &dyn Facts) -> Option<u64> {
    facts.rel_rows_bound(g.at as u32).map(|rows| (g.decide)(rows, &g.args))
}

/// Whether `guards` holds a relation guard (`rel_decide`): each serve of its program decides it again (`rels_hold`).
pub fn has_rels(guards: &[EstGuard]) -> bool {
    guards.iter().any(|g| g.lit == REL)
}

/// Whether every relation guard of `guards` decides as it did on its relation's pages now (one page count each; true
/// without one): a front hit's and a plan memo serve's check (the estimate guards are the front key's, the plan's).
pub fn rels_hold(guards: &[EstGuard], facts: &dyn Facts) -> bool {
    guards.iter().filter(|g| g.lit == REL).all(|g| rel_outcome(g, facts) == Some(g.want))
}

/// Names each literal guard of `guards` by its literal's first position among `nodes`, the admitted graph's key walk
/// (`key_and_nodes`); a literal outside it stays `LIT_LOST`.
pub fn place_lits(guards: &mut [EstGuard], nodes: &[Node<'_>]) {
    for g in guards.iter_mut().filter(|g| literal(g)) {
        g.lit = nodes.iter().position(|n| n.as_raw().as_ptr() as usize == g.at).map_or(LIT_LOST, |p| p as u32);
    }
}

/// A non-NULL by-value literal's word (the value a literal guard decides on).
fn lit_word(n: Node<'_>) -> Option<u64> {
    n.as_const().filter(|c| c.constbyval && !c.constisnull).map(|c| c.constvalue.as_u64())
}

/// What literal guard `g` decides on the key walk's nodes `nodes` (`None`: no such literal there).
fn lit_outcome(g: &EstGuard, nodes: &[Node<'_>]) -> Option<u64> {
    nodes.get(g.lit as usize).copied().and_then(lit_word).map(|w| (g.decide)(w, &g.args))
}

/// Whether every literal guard of `guards` decides as it did on the key walk's nodes `nodes` (a front hit's, which binds
/// without a graph; exec admit.rs): the estimate guards are the front key's (it holds the plan's rows exactly).
pub fn lits_hold(guards: &[EstGuard], nodes: &[Node<'_>]) -> bool {
    guards.iter().filter(|g| literal(g)).all(|g| lit_outcome(g, nodes) == Some(g.want))
}

/// Whether each literal guard's literal among `nodes` (the admitted key walk) is one a front hit rebuilds, so that
/// `lits_hold` reads the hit statement's own literal there: a node of `walk` (exec front.rs's walk), or a non-null by-value
/// constant decompose built (a plan literal retyped to its column's type, say). The front files such a constant only as
/// `Built` (front.rs `derived_at`), rebuilt on each hit from that statement's plan constant (`Retype`, `Copy`) or from what
/// its front key holds (`Fixed`), never as a stand-in (front.rs `synthesized`: no non-null Const is one); it files no
/// entry otherwise. A guard no key-walk position names (`LIT_LOST`) holds for no other statement.
pub fn lits_walked(guards: &[EstGuard], nodes: &[Node<'_>], walk: &[Node<'_>]) -> bool {
    let at = |n: &Node<'_>| n.as_raw().as_ptr() as usize;
    guards.iter().filter(|g| literal(g)).all(|g| nodes.get(g.lit as usize).is_some_and(|n| walk.iter().any(|w| at(w) == at(n)) || lit_word(*n).is_some()))
}

/// The key walk's nodes of `graph` when a guard of `guards` names a literal (none otherwise: no walk), in the graph's arena
/// (`walk_graph`).
fn lit_nodes<'p>(guards: &[EstGuard], graph: &PipelineGraph<'p>) -> ArenaVec<Node<'p>> {
    let mut nodes = ArenaVec::new_in(*graph.pipelines.allocator());
    if guards.iter().any(literal) {
        walk_graph(graph, &mut |n| nodes.push(n));
    }
    nodes
}

/// What guard `g` decides on `graph` (`nodes`: its key walk, `lit_nodes`) and, a relation guard, on its relation's pages
/// now (`facts`); `None`: the pipeline, the literal or the relation's bound is not there.
fn outcome(g: &EstGuard, graph: &PipelineGraph<'_>, nodes: &[Node<'_>], facts: &dyn Facts) -> Option<u64> {
    match g.lit {
        NOT_LIT => graph.pipelines.get(g.pipeline as usize).map(|p| (g.decide)(g.est.of(&p.desc), &g.args)),
        COST => Some(cost_outcome(g, graph)),
        REL => rel_outcome(g, facts),
        _ => lit_outcome(g, nodes),
    }
}

/// Whether a program filed with `guards` decides as it did on `graph`'s estimates and literals (the graph of the same
/// content key) and on its relations' pages now (`facts`, `rel_decide`).
pub fn guards_hold(guards: &[EstGuard], graph: &PipelineGraph<'_>, facts: &dyn Facts) -> bool {
    let nodes = lit_nodes(guards, graph);
    guards.iter().all(|g| outcome(g, graph, &nodes, facts) == Some(g.want))
}

/// The constant of every filter of a pgrc range pipeline that compares a scan column (or its integer widening) with a
/// non-NULL by-value constant under a zone operator, by value. lowerv4's zone verdict (ops/scan/pgrc/verdict_zone.rs)
/// bakes such a constant into the program while the walk keys a by-value constant as a word; the decomposer's
/// `sources::zone` puts the plan's own on the keyed site, but normalize `retype` (SRC-07: `datecol > timestamp '..'` ->
/// `datecol > date '..'`) rewrites the filter after that ran, so the rewritten constant reached no key: two statements
/// of one shape whose cross-unit constants differ shared a program and its baked constant (D2646: a wrong answer the planner's
/// estimate in the key had hidden). A filter the site already keys is keyed again here, which changes nothing. A column
/// under a relabel is read through it (PS1-10: lowerv4 reads a domain column as its base, `casts::domain_base`, and
/// retype rewrites `domdate > timestamp '..'` too); the key has no column facts, so it keys every relabel of a scan column
/// so, a superset of the domain ones: a constant keyed and not baked splits programs by value, never shares a wrong one.
/// NEW-72: a filter of `bound_form`'s shape (decompose ops/shared/scan.rs, any lane) is skipped: lowerv4 binds its
/// constant per execution over a word lane and declares no predicate for it over any other, never bakes it, so its
/// literals share one program. An arm of an OR of ANDs is keyed as before (the hull bakes it).
fn zone_constants(h: &mut Fnv128, p: &PipelineNode<'_>, varno: i32) {
    use gravity_decompose::ops::scan::float_op;
    use gravity_decompose::ops::shared::scan::zone_op;
    let scan_var = |n: Node<'_>| {
        let n = match n.as_func_expr() {
            Some(f) if f.args.len() == 1 && types_core::quiet::int_widen(f.funcid) => f.args.nth(0),
            _ => n,
        };
        let n = n.as_relabel_type().map_or(n, |r| r.arg);
        n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0)
    };
    let mut qual = |q: Node<'_>| {
        let Some(o) = q.as_op_expr().filter(|o| o.args.len() == 2 && (zone_op(o.opno).is_some() || float_op(o.opno).is_some())) else { return };
        let (a0, a1) = (o.args.nth(0), o.args.nth(1));
        let c = match (scan_var(a0), scan_var(a1)) {
            (true, false) => a1.as_const(),
            (false, true) => a0.as_const(),
            _ => None,
        };
        if let Some(c) = c.filter(|c| c.constbyval && !c.constisnull) {
            (0x20e5u32, o.opno, c.constvalue.as_u64()).hash(h);
        }
    };
    // PS1-07: an OR of ANDs (`or_of_ands`: BETWEEN SYMMETRIC, ORed ranges) bakes its arms' constants as a hull
    let mut qual = |q: Node<'_>| match gravity_decompose::ops::shared::scan::or_of_ands(q) {
        true => q.as_bool_expr().into_iter().flat_map(|b| b.args.iter()).filter_map(|a| a.as_bool_expr()).flat_map(|a| a.args.iter()).for_each(&mut qual),
        false if gravity_decompose::ops::shared::scan::bound_form(q, varno, &|_| true).is_some() => {}
        false => qual(q),
    };
    for op in p.ops.iter() {
        // a filter is one qual or the implicit AND list of them (lowerv4 ops/scan/pgrc/lower.rs `qual_node`)
        let Op::Filter(q) = op else { continue };
        match q.as_list() {
            Some(l) => l.iter().for_each(&mut qual),
            None => qual(*q),
        }
    }
}

/// The key a program with `guards` is filed under beside content key `key` (exec admit `file_local`, the shared tier): `key`
/// mixed with the outcomes its guards decide (their `want`s, or on `graph`'s estimates and its relations' pages now when
/// given), `key` itself without guards. Programs of one content key whose estimate decisions differ are filed side by
/// side, so a statement whose estimates flip a guard of the program under `key` finds the one lowered for its own outcomes
/// (exec admit `find`), and statements that alternate across a guard lower once each, never at every execution.
pub fn variant(key: u128, guards: &[EstGuard], graph: Option<(&PipelineGraph<'_>, &dyn Facts)>) -> u128 {
    if guards.is_empty() {
        return key;
    }
    // the outcomes are words: the hasher pushes nothing
    let mut h = Fnv128::new(key as u64, (key >> 64) as u64, ArenaRef::EMPTY);
    match graph {
        None => guards.iter().for_each(|g| g.want.hash(&mut h)),
        Some((graph, facts)) => {
            let nodes = lit_nodes(guards, graph);
            guards.iter().for_each(|g| outcome(g, graph, &nodes, facts).unwrap_or(u64::MAX).hash(&mut h));
        }
    }
    h.key()
}

fn pipeline_body<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, p: &PipelineNode<'p>) {
    // the planner's estimates are no part of the key (`estimates_out`: the program's bind key leaves out the same ones), nor
    // the end's spill answer (C10 `holds`: the graph it is settled from is)
    gravity_program::Pipeline { holds: None, ..gravity_program::estimates_out(&p.desc) }.hash(h);
    // D2918: a program binds PostgreSQL plan node ids (write.literals' VALUES scan, `values_node`; window.agg's WindowAgg;
    // agg.arena's Agg; a set function's node; a scan site's parameter-key recipe; EXPLAIN ANALYZE's counter cells), so the
    // key holds every plan node the pipeline runs: one graph content planned with another numbering (sorted vs hashed
    // grouping sets, an absorbed Sort above the scan) lowers its own program, never one naming the other plan's nodes
    p.node_id.hash(h);
    for n in p.node_ops.iter() {
        n.node_id.hash(h);
    }
    // PG's tuple bound of a hash build (the plan's width): D1906's one-batch guard (lowerv4 hashjoin/probe_kept.rs `cap`)
    // decides on the build's estimate with it as an argument, so the key holds it
    if let Some(Op::Join(JoinDesc::Build { pg_tuple, .. })) = p.ops.last() {
        pg_tuple.hash(h);
    }
    match &p.source {
        Source::Range { site, am, scanrelid, relid } => {
            (0u8, *scanrelid, *relid, *am).hash(h);
            site.hash(h);
            if *am == gravity_decompose::Am::Pgrcolumnar {
                zone_constants(h, p, *scanrelid as i32);
            }
        }
        Source::Index { site, am, scanrelid, relid, indexid, keys: ks, only, backward, recheck, .. } => {
            (1u8, *scanrelid, *relid, *indexid, *only, *backward, *am).hash(h);
            site.hash(h);
            keys(h, v, ks);
            // V4R-316 (L0-h): the recheck quals (indexqualorig / recheckqual) are walked as the bitmap's are: write.lock's
            // in-IR EvalPlanQual under a LIMIT compiles them, and a custom plan's literal there is a word of the key
            for q in recheck.iter() {
                node(h, v, q);
            }
        }
        Source::Const => 2u8.hash(h),
        Source::Values { scanrelid, lists } => {
            (3u8, *scanrelid).hash(h);
            let lit = literal_table(p);
            h.lit = lit;
            let (start, code) = (h.pos, h.code);
            h.code = false;
            for l in lists.iter() {
                h.code |= l.node_tag() != NodeTag::T_List;
                node(h, v, l);
            }
            h.lit = false;
            // D4449: a table of Consts alone: its rows and cells are no word and each its list's own node (`Runs`)
            if lit && !h.code {
                h.tables.add(start, h.pos, *lists as *const types_nodes::NodeList<'_> as usize);
            }
            h.code = code;
        }
        Source::Breaker { slot } => (4u8, *slot).hash(h),
        Source::Named { scanrelid, relid, name } => {
            (5u8, *scanrelid, *relid).hash(h);
            name.hash(h);
        }
        Source::Bitmap { site, am, scanrelid, relid, plan, recheck, morsel } => {
            (6u8, *scanrelid, *relid, *am, *morsel).hash(h);
            site.hash(h);
            bitmap(h, v, plan);
            for q in recheck.iter() {
                node(h, v, q);
            }
        }
        Source::Tid { site, am, scanrelid, relid, form, quals } => {
            (7u8, *scanrelid, *relid, *am, *form as u8).hash(h);
            site.hash(h);
            for q in quals.iter() {
                node(h, v, q);
            }
        }
        Source::Sample { site, am, scanrelid, relid, sample } => {
            // D1643: a value evaluated at each open is the host's cell, not a word: another program
            (8u8, *scanrelid, *relid, *am, sample.handler, sample.per_open).hash(h);
            // D1920 / D1644: the clause's raise class, which the effects walk reads from the percentage's word
            sample_class(sample.handler, sample.args, sample.repeatable, &plan_word).hash(h);
            site.hash(h);
            for a in sample.args.iter().chain(sample.repeatable) {
                node(h, v, a);
            }
        }
        Source::TableFunc { scanrelid, func, cols, ordinality } => {
            (9u8, *scanrelid, *cols, *ordinality).hash(h);
            table_func(h, v, func);
        }
    }
    for op in p.ops.iter() {
        match op {
            Op::Filter(n) => {
                0u8.hash(h);
                node(h, v, *n);
            }
            Op::Gate(n) => {
                1u8.hash(h);
                node(h, v, *n);
            }
            Op::Project(t) => {
                2u8.hash(h);
                for e in t.iter() {
                    node(h, v, e);
                }
            }
            Op::Limit { count, offset, ties } => {
                (3u8, ties.is_some()).hash(h);
                if let Some(t) = ties {
                    // the tie keys come from the plan's uniqColIdx, not from any op hashed here
                    t.over.hash(h);
                    for k in t.keys.iter() {
                        (k.resno, k.opno, k.kind as u8).hash(h);
                    }
                }
                bound(h, v, *count);
                bound(h, v, *offset);
            }
            Op::FirstOfRun(t) => {
                // V4R-147 DISTINCT ON: the run keys come from the plan's uniqColIdx / uniqOperators
                (13u8, t.over).hash(h);
                for k in t.keys.iter() {
                    (k.resno, k.opno, k.kind as u8).hash(h);
                }
            }
            Op::GroupRun(i) => (14u8, *i).hash(h),
            Op::Sort(d) => {
                4u8.hash(h);
                (d.presorted, d.keys.len() as u8).hash(h);
                for k in d.keys.iter() {
                    (k.resno, k.opno, k.desc, k.nulls_first, k.kind as u8).hash(h);
                }
                match d.form {
                    SortForm::Runs => 0u8.hash(h),
                    SortForm::TopK { count, offset, ties } => {
                        (1u8, ties).hash(h);
                        topk_bound(h, v, count);
                        topk_bound(h, v, offset);
                    }
                    SortForm::Walk { attno, desc, nulls_first, count, offset, ties } => {
                        (2u8, attno, desc, nulls_first, ties).hash(h);
                        topk_bound(h, v, count);
                        topk_bound(h, v, offset);
                    }
                }
            }
            Op::Join(d) => {
                5u8.hash(h);
                join(h, v, d);
            }
            Op::Window(d) => {
                6u8.hash(h);
                dumped(h, &|w| d.dump(w));
                nodes(h, v, d.tlist);
                nodes(h, v, d.run_condition);
                for f in d.functions.iter() {
                    node(h, v, f.node);
                }
                // M10-WINDOW: the frame offsets (the window end evaluates them once per execution; a literal offset is a word)
                for o in [d.plan.startOffset, d.plan.endOffset] {
                    o.map_or(0u8, |_| 1u8).hash(h);
                    if let Some(o) = o {
                        node(h, v, o);
                    }
                }
            }
            Op::SetOp(d) => {
                7u8.hash(h);
                dumped(h, &|w| d.dump(w));
            }
            Op::SubPlan(d) => {
                (8u8, d.plan_id, d.link as u8, d.unknown_eq_false, d.alternative, d.set_spool).hash(h);
                if let Some(t) = d.test {
                    node(h, v, t);
                }
                nodes(h, v, d.args);
                for i in d.params.iter() {
                    i.hash(h);
                }
                match &d.form {
                    SubForm::Nested { inner } => {
                        0u8.hash(h);
                        pipeline(h, v, inner);
                    }
                    SubForm::Build => 1u8.hash(h),
                    SubForm::Hashed { slot } => (2u8, *slot).hash(h),
                }
            }
            Op::Append(d) => {
                9u8.hash(h);
                // D3586: an Append's arms share its prune info, which lists every partition: an arm whose info is the one
                // the last pruned arm keyed is keyed by that, not by the list again (N arms keyed N lists of N)
                match d {
                    AppendDesc::Arm { i, n, ordered, prune: Some(p) } if h.prune == p.info as *const _ as usize => {
                        let _ = fmt::Write::write_fmt(h, format_args!("  append arm {i}/{n}{} prune=same", if *ordered { " ordered" } else { "" }));
                    }
                    _ => {
                        if let AppendDesc::Arm { prune: Some(p), .. } = d {
                            h.prune = p.info as *const _ as usize;
                        }
                        dumped(h, &|w| d.dump(w));
                    }
                }
            }
            Op::Write(d) => {
                (10u8, d.cmd, d.rti, d.site.natts() as u32, d.epq, d.marks.len() as u32, d.on_conflict.is_some()).hash(h);
                write_plan(h, &d.plan);
                // the check options are the program's (`exec/src/admit.rs` relocates each qual per admission by its traversal index)
                (d.checks.len() as u32).hash(h);
                for c in d.checks.iter() {
                    (c.target, c.kind as u32, c.cascaded, c.relname, c.polname).hash(h);
                    node(h, v, c.qual);
                }
                if let Some(s) = d.set {
                    for i in s.iter() {
                        i.hash(h);
                    }
                }
                if let Some(r) = d.returning {
                    nodes(h, v, r);
                }
                if let Some(c) = &d.on_conflict {
                    (c.update, c.excl_rti, c.arbiter.len() as u32).hash(h);
                    // the arbiter indexes themselves: the frozen plan takes them at bind (`write::write_plan`), so two
                    // statements of one shape with different conflict targets must not share a program
                    for a in c.arbiter.iter() {
                        a.hash(h);
                    }
                    nodes(h, v, c.set);
                    if let Some(f) = c.filter {
                        node(h, v, f);
                    }
                }
                if let Some(m) = &d.merge {
                    (m.rowid_col, m.target_cols.len() as u32, m.returning_action, m.join_cond.is_some()).hash(h);
                    for q in m.join_cond.iter() {
                        nodes(h, v, q);
                    }
                    for a in m.actions.iter() {
                        (a.kind as u32, a.cmd).hash(h);
                        for q in a.quals.iter() {
                            nodes(h, v, q);
                        }
                        nodes(h, v, a.tlist);
                        for i in a.set.iter().flat_map(|s| s.iter()) {
                            i.hash(h);
                        }
                    }
                }
                write_faces(h, v, d);
            }
            Op::Lock(d) => {
                (11u8, d.epq, d.members.0).hash(h);
                write_plan(h, &d.plan);
                for m in d.marks.iter() {
                    (m.rti, m.relid, m.mark as u32, m.lock, m.wait, m.parent).hash(h);
                }
                nodes(h, v, d.tlist);
            }
            Op::Srf(d) => {
                // by structure, never the dump's text: a call's literals go through `node` (a word stays a word)
                (12u8, d.place == SrfPlace::TargetList, d.functions.len() as u32).hash(h);
                if let SrfPlace::From { scanrelid, ordinality } = d.place {
                    (scanrelid, ordinality).hash(h);
                }
                nodes(h, v, d.tlist);
                for f in d.functions.iter() {
                    (f.resno, f.funcid, f.cols, f.coltypes.len() as u32).hash(h);
                    dumped(h, &|w| write!(w, "{:?}", f.kind));
                    f.coltypes.iter().for_each(|t| t.hash(h));
                    // D745: a FROM member other than a scalar call or an inlined body's expression (which compile their
                    // expression) is the iterator's (its literals words) or form (c)'s (the incumbent FunctionScan evaluates
                    // its arguments; the IR only deforms the store): its by-reference literals are keyed by class (`lit`,
                    // logged with the lists: a lowering that read one files no program), one program for every argument
                    let lit = h.lit;
                    h.lit = matches!(d.place, SrfPlace::From { .. }) && !matches!(f.kind, SrfKind::Scalar | SrfKind::Expr);
                    node(h, v, f.node);
                    h.lit = lit;
                }
            }
        }
    }
}

/// The write desc's D1 faces (decompose.md §7 need 1), keyed before any member that reads them is bound:
/// expressions go through `node` (a literal stays a word), never through the dump's text.
fn write_faces<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, d: &gravity_decompose::ops::shared::modifytable::WriteDesc<'p>) {
    (d.members.0, d.tableoid_col, d.targets.len() as u32, d.old_new.old, d.old_new.new, d.route.is_some()).hash(h);
    if let Some(r) = &d.route {
        (r.root_rti, r.root_relid, r.root_natts, r.cross_partition).hash(h);
    }
    (d.leaves_known, target_facts(h, d)).hash(h);
    for t in d.targets.iter().skip(1) {
        if let Some(r) = t.returning {
            nodes(h, v, r);
        }
        for m in t.merge.iter() {
            for q in m.join_cond.iter() {
                nodes(h, v, q);
            }
            for a in m.actions.iter() {
                (a.kind as u32, a.cmd).hash(h);
                for q in a.quals.iter() {
                    nodes(h, v, q);
                }
                nodes(h, v, a.tlist);
                for c in a.set.iter().flat_map(|s| s.iter()) {
                    c.hash(h);
                }
            }
        }
    }
    for c in d.checks.iter() {
        (c.target, c.kind as u32, c.relname, c.polname, c.cascaded).hash(h);
        node(h, v, c.qual);
    }
    for m in d.mark_descs.iter() {
        (m.rti, m.relid, m.mark as u32, m.lock, m.wait, m.parent).hash(h);
    }
}

/// The leaves' and targets' facts that hold no expression, as one sub-key per (targets, leaves) slice pair of the walk: a
/// streamed union copies its Write into every arm (decompose append.rs `push_union`), so a write over N partitions hashed
/// N x N facts (m4_l20's 4100-partition UPDATE: the key walk 15% of its admission). The sub-key is the facts' content.
fn target_facts(h: &mut Fnv128, d: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> u128 {
    let id = [d.targets.as_ptr() as usize, d.targets.len(), d.leaves.as_ptr() as usize, d.leaves.len()];
    if let Some(&(_, k)) = h.faces.iter().find(|f| f.0 == id) {
        return k;
    }
    let mut f = Fnv128::new(0x7461_7267_6574_7301, 0x84222325_cbf29ce4, h.a);
    (d.leaves.len() as u32).hash(&mut f);
    for l in d.leaves.iter() {
        (l.relid, l.natts, l.heap, l.same_layout, l.has_triggers, l.has_rules, l.has_generated, l.nindexes, l.indexes_plain).hash(&mut f);
    }
    for (i, t) in d.targets.iter().enumerate() {
        (t.rti, t.relid, t.kind as u8, t.site.natts() as u32, t.junk_col, t.wholerow_col).hash(&mut f);
        (t.instead.insert, t.instead.update, t.instead.delete, t.instead.known, t.generated.len() as u32, t.to_root.len() as u32).hash(&mut f);
        dumped(&mut f, &|w| gravity_decompose::ops::modifytable::dump_indexes(t, w));
        for g in t.generated.iter() {
            g.hash(&mut f);
        }
        for a in t.to_root.iter() {
            a.hash(&mut f);
        }
        if i == 0 {
            continue;
        }
        write_plan(&mut f, &t.plan);
        for a in t.set.iter().flat_map(|s| s.iter()) {
            a.hash(&mut f);
        }
        (t.returning.is_some(), t.merge.is_some()).hash(&mut f);
    }
    let k = f.key();
    h.faces.push((id, k));
    k
}

fn write_plan(h: &mut Fnv128, p: &gravity_program::WritePlan) {
    (p.target_rel, p.cmd, p.lock, p.wait, p.conflict as u8).hash(h);
    for i in p.indexes.iter() {
        i.hash(h);
    }
    // the arbiter indexes are the program's (the host's pre-check and deferred unique checks read them): two
    // statements of one shape with different conflict targets are two programs
    for a in p.arbiters.iter() {
        a.hash(h);
    }
    p.route.hash(h);
}

/// A TopK bound sizes the run: a constant is part of the shape (its value), an expression its structure.
fn topk_bound<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, b: Bound<'p>) {
    match b {
        Bound::Const(n) => (1u8, n).hash(h),
        _ => bound(h, v, b),
    }
}

fn nodes<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, l: &types_nodes::NodeList<'p>) {
    (l.len() as u32).hash(h);
    for n in l.iter() {
        node(h, v, n);
    }
}

fn opt_node<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, n: Option<Node<'p>>) {
    n.is_some().hash(h);
    if let Some(n) = n {
        node(h, v, n);
    }
}

fn opt_nodes<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, l: &types_nodes::list::OptNodeList<'p>) {
    (l.len() as u32).hash(h);
    for n in l.iter() {
        opt_node(h, v, n);
    }
}

/// M5 L0-f (1): an XMLTABLE / JSON_TABLE keyed by its whole face, not only its (rti, width, ordinality): two table
/// functions of one rti and width shared a program. The kind, the namespaces, the document and row expressions, each
/// column's name, type, typmod, collation and path / default / value expressions, the PASSING values, the NOT NULL columns
/// and JSON_TABLE's path plan; every expression through `node` (a literal as the Const rule keys it; the document and the
/// PASSING values by class, D715).
fn table_func<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, f: &'p types_nodes::primnodes::TableFunc<'p>) {
    (f.functype as u32, f.ordinalitycol).hash(h);
    nodes(h, v, &f.ns_uris);
    opt_nodes(h, v, &f.ns_names);
    // D715: the document and the PASSING values are the incumbent TableFuncScan's to evaluate (srf/table_func.rs: the IR
    // only deforms its store), so their by-reference literals are keyed by class, never by image (`lit`, logged with the
    // lists: a lowering that read one files no program). A JSON_TABLE / XMLTABLE over a literal document hashed the whole
    // document on every execution (FNV, a step per byte: ~0.1 ms at 1k JSON rows, ~10 ms at 100k) and filed a program per document.
    let lit = core::mem::replace(&mut h.lit, true);
    opt_node(h, v, f.docexpr);
    nodes(h, v, &f.passingvalexprs);
    h.lit = lit;
    opt_node(h, v, f.rowexpr);
    nodes(h, v, &f.colnames);
    (f.coltypes.len() as u32).hash(h);
    for ((t, m), c) in f.coltypes.iter().zip(f.coltypmods.iter()).zip(f.colcollations.iter()) {
        (t, m).hash(h);
        coll(h, c);
    }
    for l in [&f.colexprs, &f.coldefexprs, &f.colvalexprs] {
        opt_nodes(h, v, l);
    }
    f.notnulls.iter().for_each(|c| c.hash(h));
    json_table_plan(h, v, f.plan);
}

/// JSON_TABLE's path plan (its scans' paths and names, error flags and column ranges; sibling joins), which `node`'s
/// generic walk does not enter.
fn json_table_plan<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, p: Option<Node<'p>>) {
    crate::stack_check();
    let Some(p) = p else { return 0u8.hash(h) };
    if let Some(s) = p.as_json_table_path_scan() {
        (1u8, s.errorOnError, s.colMin, s.colMax).hash(h);
        let path = s.path.and_then(|n| n.as_json_table_path());
        opt_node(h, v, path.and_then(|x| x.value));
        path.and_then(|x| x.name).hash(h);
        json_table_plan(h, v, s.child);
    } else if let Some(j) = p.as_json_table_sibling_join() {
        2u8.hash(h);
        json_table_plan(h, v, j.lplan);
        json_table_plan(h, v, j.rplan);
    } else {
        node(h, v, p);
    }
}

fn join<'p>(h: &mut Fnv128, v: Visit<'_, 'p>, d: &JoinDesc<'p>) {
    match d {
        JoinDesc::Build { keys, decls: _, .. } => {
            0u8.hash(h);
            nodes(h, v, keys);
        }
        JoinDesc::Probe { slot, jointype, clauses, keys, qual, unique, marks, decls: _, inner_node: _, outer_first, catalog: _ } => {
            (1u8, *slot, *jointype as u32, *unique, *marks, *outer_first).hash(h);
            nodes(h, v, clauses);
            nodes(h, v, keys);
            nodes(h, v, qual);
        }
        JoinDesc::Drain { matches, jointype } => (2u8, *matches, *jointype as u32).hash(h),
        JoinDesc::Merge { slot, jointype, clauses, reversals, nulls_first, families, collations, qual, unique, inner_node: _ } => {
            (3u8, *slot, *jointype as u32, *unique).hash(h);
            nodes(h, v, clauses);
            reversals.hash(h);
            nulls_first.hash(h);
            // the compare's btree family and collation (M11): text_ops and text_pattern_ops order apart
            families.hash(h);
            collations.hash(h);
            nodes(h, v, qual);
        }
        JoinDesc::Nest { jointype, params, inner, qual, unique, inner_node: _, facts: _ } => {
            (4u8, *jointype as u32, *unique).hash(h);
            nodes(h, v, params);
            nodes(h, v, qual);
            pipeline(h, v, inner);
        }
        JoinDesc::Memo { keys, entries, singlerow, binary, ops, colls, purge, nonkey } => {
            // ops / colls: the lowered lookup bakes each key's hash equality operator and collation (MC-1); a memo PG may purge
            // (memoize item 5) lowers another lookup: its own tag and the params it purges on, every other memo's key unchanged
            (if *purge { 7u8 } else { 5u8 }, *entries, *singlerow, *binary, *ops, *colls).hash(h);
            if *purge {
                nonkey.hash(h);
            }
            nodes(h, v, keys);
        }
        JoinDesc::Spool => 6u8.hash(h),
    }
}

/// The front key of `graph` under the `Sequential` word plan.
pub fn key(graph: &PipelineGraph<'_>) -> ContentKey {
    key_and_nodes(graph, &mut |_| {})
}

/// `key`, and every expression node of the graph handed to `visit` in the traversal's order: a
/// word's node is named by its position in that order, which the same shape reproduces.
pub fn key_and_nodes<'p>(graph: &PipelineGraph<'p>, visit: Visit<'_, 'p>) -> ContentKey {
    walk_graph(graph, visit).0
}

/// D4449: `key_and_nodes`, and the walk's literal-table runs (`Runs`), whose positions the alias passes skip.
pub fn key_nodes_runs<'p>(graph: &PipelineGraph<'p>, visit: Visit<'_, 'p>) -> (ContentKey, Runs) {
    let (k, _, runs) = walk_graph(graph, visit);
    (k, runs)
}

/// V4R-110: the image addresses of the constant lists the content key keys by their class (`list_class`), not their
/// bytes: a lowering that read one files no program (lowerv4 lib.rs `lower_graph`). In the graph's arena (`walk_graph`).
pub fn class_keyed_lists(graph: &PipelineGraph<'_>) -> ArenaVec<usize> {
    walk_graph(graph, &mut |_| {}).1
}

/// V4R-110: `lower` (a lowering of `graph`) under bake scopes of its own, and whether it read a list image the content key
/// keys by its class (`class_keyed_lists`): that program is its list's, so it is never filed (`Descs::no_cache`). Its
/// reads are handed on to the enclosing scopes (the front key's) as if it had opened none.
pub fn lowering_reads_lists<T>(graph: &PipelineGraph<'_>, lower: impl FnOnce() -> T) -> (T, bool) {
    let (e, d) = (gravity_expr::bake::Scope::open(), gravity_decompose::bake::Scope::open());
    let r = lower();
    let (re, rd) = (e.reads(), d.reads());
    drop((e, d));
    re.iter().for_each(|&a| gravity_expr::bake::note(a));
    rd.iter().for_each(|&a| gravity_decompose::bake::note(a));
    let read = !(re.is_empty() && rd.is_empty()) && {
        let lists = class_keyed_lists(graph);
        re.iter().chain(rd.iter()).any(|a| lists.contains(a))
    };
    (r, read)
}

/// M4-INSERT (cards/M4.md L0-m), NEW-29: every VALUES list is write.literals' per-execution literal table (lowerv4
/// write/literals.rs): the program reads its Const cells from the table's words, so its content key holds each such cell's
/// class (type, typmod, collation, NULL or not), never a by-reference image, and one program serves every literal set of
/// the statement's shape. A cell that is an expression is the program's code and keyed as one, its own Consts by image. A
/// lowering that bakes a table cell anyway files no program: its by-reference cells are logged with the lists
/// (`lowering_reads_lists`).
pub fn literal_table(p: &PipelineNode<'_>) -> bool {
    matches!(p.source, Source::Values { .. })
}

/// D2918: the plan node id of a VALUES pipeline's scan, the one write.literals declares (lowerv4 values/lower.rs, `Proto::arg`)
/// and exec's admission and prepare look up in the plan (`admit_literals`). A program binds it, so the content key holds it:
/// one graph content planned with another numbering (an absorbed Sort or Materialize above the scan) lowers apart.
pub fn values_node(p: &PipelineNode<'_>) -> i32 {
    p.node_ops.first().map_or(p.node_id, |n| n.node_id)
}

/// STEP-33: whether the pipeline reads a heap table or no table (V4R-110: its lists are keyed by class). A pgrcolumnar
/// pipeline keeps its lists' images: its per-code table bakes them. The test is per pipeline, so one columnar scan no
/// longer takes the heap pipelines' lists of the same graph back to their bytes.
fn heap_source(p: &PipelineNode<'_>) -> bool {
    match &p.source {
        Source::Range { am, .. } | Source::Index { am, .. } | Source::Bitmap { am, .. } | Source::Tid { am, .. } | Source::Sample { am, .. } => *am == gravity_decompose::Am::Heap,
        _ => true,
    }
}

/// The walk's scratch is the graph's arena (its pipelines' allocator): the admission's, reset with the graph at the end of
/// its scope, so nothing the walk keeps outlives the statement and every byte of it has the arena's owner.
fn walk_graph<'p>(graph: &PipelineGraph<'p>, visit: Visit<'_, 'p>) -> (ContentKey, ArenaVec<usize>, Runs) {
    let a = *graph.pipelines.allocator();
    let mut h = Fnv128::new(0x6c6f_7765_7232_0002, 0x84222325_cbf29ce4, a);
    let mut at: ArenaVec<Node<'p>> = ArenaVec::new_in(a);
    let visit: Visit<'_, 'p> = &mut |n: Node<'p>| {
        at.push(n);
        visit(n)
    };
    (graph.pipelines.len() as u8, graph.breakers.len() as u8, graph.root).hash(&mut h);
    // C1 part 2: a partial reader's graph (a named cursor's FETCH, decompose `PipelineGraph::partial`) lowers its stop
    // roads to the cursor's root, so it is keyed apart; every other key is unchanged
    if graph.partial {
        0x4355_5253_4f52u64.hash(&mut h);
    }
    for p in graph.pipelines.iter() {
        h.lists = true;
        pipeline(&mut h, visit, p);
    }
    // a group's, breaker's or init plan's expression may run in any pipeline: class keys only when every one is heap's
    h.lists = graph.pipelines.iter().all(heap_source);
    for b in graph.breakers.iter() {
        (b.breaker, b.producer, b.layout).hash(&mut h);
        let _ = b.order.dump(&mut h);
    }
    // D1071: a streamed group (`Op::GroupRun`) is keyed as a breaker's is
    for g in graph.groups.iter().chain(graph.streams.iter()) {
        (g.slot, g.node_id, g.form as u8, g.layout, g.hints).hash(&mut h);
        for k in g.keys.iter() {
            (k.word, k.resno, k.kind, k.attno, k.scale).hash(&mut h);
            node(&mut h, visit, k.expr.node());
        }
        for a in g.aggs.iter() {
            (a.aggno, a.fnoid, a.reg as u8, a.slot, a.count_slot, a.scale).hash(&mut h);
            let _ = fmt::Write::write_fmt(&mut h, format_args!("{:?}", a.final_));
            node(&mut h, visit, a.node);
            if let Some(f) = a.filter {
                node(&mut h, visit, f);
            }
        }
        let _ = fmt::Write::write_fmt(&mut h, format_args!("{:?}", g.two_pass));
    }
    for ip in graph.init_plans.iter() {
        let _ = fmt::Write::write_fmt(&mut h, format_args!("{ip:?}"));
    }
    record_rows(&mut h, graph);
    let tables = h.tables;
    alias(&mut h, &at, tables.as_slice());
    (ContentKey(h.key()), h.listed, tables)
}

/// D2847: a node the graph holds at two places (`x BETWEEN SYMMETRIC a AND b`'s two arms share `a` and `b`; one
/// equivalence class's Const under two quals) is another shape than two nodes of one value: lowerv4 places a literal's
/// word per node under its expression (`Px::lit_word`), so the program read one word at both places, and a statement
/// of the other shape (`x BETWEEN a AND b OR x BETWEEN c AND d`) bound that word for both. The key carries the walk's
/// aliasing as the front key does (exec front.rs `alias`, D325): each repeat of a node (by identity) with its previous
/// position, in position order, then the number of repeats. D3642: the copies of one literal (`lit_share`) are one node,
/// and the positions holding a copy of an earlier one (`alias_shares`), so copies are another shape than one node twice.
fn alias(h: &mut Fnv128, at: &[Node<'_>], tables: &[(u32, u32)]) {
    let a = h.a;
    alias_walk(a, at, tables, &mut |p| match p {
        AliasPut::Pair(i, j) => (i, j).hash(h),
        AliasPut::Count(n) => n.hash(h),
        AliasPut::Share(v) => v.hash(h),
    });
}

/// What `alias_walk` hands its caller to key, in order: each repeat pair, their number, then the shares' words
/// (`alias_shares`).
pub enum AliasPut {
    Pair(u32, u32),
    Count(usize),
    Share(u32),
}

/// Both keys' `alias` over walk `at`: `alias_repeats`' pairs, their number, then `alias_shares`, every position `at`'s.
/// D4449: the positions in `tables` (`Runs`: literal tables of Consts alone) are not visited, the others keep their
/// positions: a table cell is its list's own node, at one position, and no literal's copy (P0: the plan's values_lists are
/// the RTE's own, which no other plan field holds; a copy is an appendrel child's, never a VALUES row's), so what the
/// passes over the other positions put is what the passes over all of them put. The passes' scratch is arena `a`'s (the
/// walk's: the admission's graph arena), handed back on return: `put` keys what it is given and allocates nothing in `a`.
pub fn alias_walk(a: ArenaRef, at: &[Node<'_>], tables: &[(u32, u32)], put: &mut dyn FnMut(AliasPut)) {
    if tables.is_empty() {
        alias_over(a, || at.iter().copied(), &|i| i, put);
    } else {
        alias_over(a, || Kept::new(at, tables), &|i| unkept(tables, i), put);
    }
}

/// `alias_walk` over the nodes `nodes()` yields (each pass takes a fresh iterator), position `i` of them being walk
/// position `at(i)`.
fn alias_over<'p, I: Iterator<Item = Node<'p>>>(a: ArenaRef, nodes: impl Fn() -> I, at: &dyn Fn(u32) -> u32, put: &mut dyn FnMut(AliasPut)) {
    let _passes = a.scope();
    let ids = alias_ids(a, &nodes);
    let mut n = 0usize;
    alias_repeats(a, &ids, &mut |i, j| {
        put(AliasPut::Pair(at(i), at(j)));
        n += 1;
    });
    put(AliasPut::Count(n));
    let mut s = 0u32;
    for (i, (&id, a)) in ids.iter().zip(nodes()).enumerate() {
        if id != a.as_raw().as_ptr() as usize {
            put(AliasPut::Share(0x3642));
            put(AliasPut::Share(at(i as u32)));
            s += 1;
        }
    }
    if s > 0 {
        put(AliasPut::Share(s));
    }
}

/// D4600 (lane pgladmit): `alias_walk` in one pass over the walk (exec front.rs takes it under its admission switch,
/// `PGRUST_GRAVITY_ADMIT_LEAN`). Each node's identity (`alias_ids`: its address, or for a literal with copies its first
/// copy's) is final when the pass reaches it, as a copy's first copy is at an earlier position; so the repeats
/// (`alias_repeats`) and the shares are found in the same pass and put at its end in `alias_walk`'s order: the pairs,
/// their number, the shares. A walk with literal-table runs, more than `FUSED_LITS` literals, or two literals of one
/// location and type that are no copies (`first_copies`' false, where the sort decides) is `alias_walk`'s own. What it
/// puts equals `alias_walk`'s (`the_fused_alias_pass_puts_what_alias_walk_puts`).
pub fn alias_walk_fused(a: ArenaRef, at: &[Node<'_>], tables: &[(u32, u32)], put: &mut dyn FnMut(AliasPut)) {
    if !tables.is_empty() || !alias_fused(a, at, put) {
        alias_walk(a, at, tables, put);
    }
}

/// The literals `alias_fused` tracks in its inline table (`PosTable`'s `POS_INLINE` slots, at most half full).
const FUSED_LITS: usize = POS_INLINE / 2;

/// `alias_walk_fused`'s pass; false (and nothing put) where `alias_walk` decides.
fn alias_fused(a: ArenaRef, at: &[Node<'_>], put: &mut dyn FnMut(AliasPut)) -> bool {
    let _pass = a.scope();
    let mut lits = PosTable::with_capacity_in(FUSED_LITS, a);
    let mut nlits = 0usize;
    let mut seen = PosTable::with_capacity_in(at.len(), a);
    let mut pairs: ArenaVec<(u32, u32)> = ArenaVec::new_in(a);
    let mut shares: ArenaVec<u32> = ArenaVec::new_in(a);
    for (i, n) in at.iter().enumerate() {
        let addr = n.as_raw().as_ptr() as usize;
        let mut id = addr;
        if let Some(s) = lit_share(*n) {
            nlits += 1;
            if nlits > FUSED_LITS {
                return false;
            }
            if let Some(f) = lits.first((u64::from(s.typ) << 32) | u64::from(s.location as u32), i as u32) {
                let first = at[f as usize];
                if lit_share(first) != Some(s) {
                    return false;
                }
                id = first.as_raw().as_ptr() as usize;
            }
        }
        if let Some(j) = seen.put(id as u64, i as u32) {
            pairs.push((i as u32, j));
        }
        if id != addr {
            shares.push(i as u32);
        }
    }
    pairs.iter().for_each(|&(i, j)| put(AliasPut::Pair(i, j)));
    put(AliasPut::Count(pairs.len()));
    for &i in shares.iter() {
        put(AliasPut::Share(0x3642));
        put(AliasPut::Share(i));
    }
    if !shares.is_empty() {
        put(AliasPut::Share(shares.len() as u32));
    }
    true
}

/// D4449: the walk positions of the literal tables whose every cell is a Const (a key walk's `Source::Values` lists, a
/// front walk's ValuesScan rows), `[start, end)` runs in walk order, at most `RUNS` (a table past them is not recorded, so
/// the passes visit it): none of their nodes is a word (NEW-29: the table's cells are loaded from its words) and each is
/// at one position (`alias_walk`). A list the walk meets twice (by its address) makes no run at all: its cells repeat.
#[derive(Clone, Copy, Debug)]
pub struct Runs {
    n: u8,
    twice: bool,
    at: [(u32, u32); RUNS],
    lists: [usize; RUNS],
}

const RUNS: usize = 8;

impl Runs {
    pub const NONE: Runs = Runs { n: 0, twice: false, at: [(0, 0); RUNS], lists: [0; RUNS] };

    /// Records positions `[start, end)` of list `list`, after every run recorded before.
    pub fn add(&mut self, start: u32, end: u32, list: usize) {
        if self.lists[..self.n as usize].contains(&list) {
            self.twice = true;
        } else if (self.n as usize) < RUNS && end > start {
            self.at[self.n as usize] = (start, end);
            self.lists[self.n as usize] = list;
            self.n += 1;
        }
    }

    pub fn as_slice(&self) -> &[(u32, u32)] {
        if self.twice { &[] } else { &self.at[..self.n as usize] }
    }
}

/// The nodes of `at` outside `runs` (sorted, disjoint), in position order (all of them for no run).
struct Kept<'a, 'p> {
    at: &'a [Node<'p>],
    runs: &'a [(u32, u32)],
    i: usize,
    r: usize,
}

impl<'a, 'p> Kept<'a, 'p> {
    fn new(at: &'a [Node<'p>], runs: &'a [(u32, u32)]) -> Kept<'a, 'p> {
        Kept { at, runs, i: 0, r: 0 }
    }
}

impl<'p> Iterator for Kept<'_, 'p> {
    type Item = Node<'p>;

    fn next(&mut self) -> Option<Node<'p>> {
        while let Some(&(s, e)) = self.runs.get(self.r) {
            if self.i < s as usize {
                break;
            }
            self.i = self.i.max(e as usize);
            self.r += 1;
        }
        let n = self.at.get(self.i).copied()?;
        self.i += 1;
        Some(n)
    }
}

/// The walk position of the `i`th node outside `runs` (`Kept`).
fn unkept(runs: &[(u32, u32)], i: u32) -> u32 {
    let mut p = i;
    for &(s, e) in runs {
        if s > p {
            break;
        }
        p += e - s;
    }
    p
}

/// D325 / D2847 (`alias`): each position of `ids` whose identity an earlier position holds, with the last such earlier
/// position, in position order. D4449: one pass over a table by identity, where both keys sorted every walk node by
/// identity and then the repeats (a 25k-row VALUES sorted 100k nodes per key): the same pairs in the same order (each
/// adjacent pair of one identity's positions, by the later position). Its table is arena `a`'s.
pub fn alias_repeats(a: ArenaRef, ids: &[usize], put: &mut dyn FnMut(u32, u32)) {
    let mut last = PosTable::with_capacity_in(ids.len(), a);
    for (i, &a) in ids.iter().enumerate() {
        if let Some(j) = last.put(a as u64, i as u32) {
            put(i as u32, j);
        }
    }
}

/// D4449: an open-addressed table from a 64-bit key to a walk position (`u32::MAX` marks an empty slot), at most half
/// full: the key walks' linear passes over their nodes by identity (`alias_repeats`, `alias_ids`, exec front.rs `file`).
/// A table of up to `POS_INLINE` slots (a walk of 16 nodes: a single-row statement's) lives inline, with no heap
/// allocation, so a short walk pays no more than its sort did. A larger one's slots are `S`: an arena's (`with_capacity_in`,
/// the key passes'), or the heap's (`with_capacity`, exec's lookups).
pub struct PosTable<S = Vec<(u64, u32)>> {
    inline: [(u64, u32); POS_INLINE],
    slots: S,
    /// log2 of the slot count: a key's home is its `phi_top` of that many bits.
    bits: u32,
}

const POS_INLINE: usize = 32;

/// The slot count of a table for at most `n` keys.
fn pos_cap(n: usize) -> usize {
    (n.max(4) * 2).next_power_of_two()
}

impl PosTable {
    /// A table for at most `n` keys.
    pub fn with_capacity(n: usize) -> PosTable {
        let cap = pos_cap(n);
        let slots = if cap > POS_INLINE { core::iter::repeat_n((0, u32::MAX), cap).collect() } else { Default::default() }; // alloc: Cold
        PosTable { inline: [(0, u32::MAX); POS_INLINE], slots, bits: cap.trailing_zeros() }
    }
}

impl PosTable<ArenaVec<(u64, u32)>> {
    /// `with_capacity`, its slots in arena `a`.
    pub fn with_capacity_in(n: usize, a: ArenaRef) -> Self {
        let cap = pos_cap(n);
        let mut slots = ArenaVec::new_in(a);
        if cap > POS_INLINE {
            slots.resize(cap, (0, u32::MAX));
        }
        PosTable { inline: [(0, u32::MAX); POS_INLINE], slots, bits: cap.trailing_zeros() }
    }
}

impl<S: core::ops::DerefMut<Target = [(u64, u32)]>> PosTable<S> {
    fn table(&self) -> &[(u64, u32)] {
        if self.slots.is_empty() { &self.inline[..1 << self.bits] } else { &self.slots }
    }

    fn table_mut(&mut self) -> &mut [(u64, u32)] {
        if self.slots.is_empty() { &mut self.inline[..1 << self.bits] } else { &mut self.slots }
    }

    /// The slot holding `k`, else the empty slot where it goes.
    fn slot(&self, k: u64) -> usize {
        let t = self.table();
        let mask = t.len() - 1;
        let mut i = gravity_abi::hash::phi_top(k, self.bits) as usize;
        while t[i].1 != u32::MAX && t[i].0 != k {
            i = (i + 1) & mask;
        }
        i
    }

    /// The position held under `k`.
    pub fn get(&self, k: u64) -> Option<u32> {
        let v = self.table()[self.slot(k)].1;
        (v != u32::MAX).then_some(v)
    }

    /// Puts `v` under `k`; the position held there before.
    pub fn put(&mut self, k: u64, v: u32) -> Option<u32> {
        let i = self.slot(k);
        let old = core::mem::replace(&mut self.table_mut()[i], (k, v)).1;
        (old != u32::MAX).then_some(old)
    }

    /// The position held under `k`, else puts `v` there (and `None`).
    pub fn first(&mut self, k: u64, v: u32) -> Option<u32> {
        let i = self.slot(k);
        match self.table()[i].1 {
            u32::MAX => {
                self.table_mut()[i] = (k, v);
                None
            }
            old => Some(old),
        }
    }
}

/// D3642: the positions of `at` whose `alias_ids` id is another node's (a literal's copy read as its first copy), each
/// put as `(0x3642, position)`, then their number; nothing when there is none (the key of a statement without copies is
/// as before). A shared node at two places and two copies of one literal alias the same positions; both keys put this
/// so that the two are distinct shapes (a front hit names each key-walk node by identity).
pub fn alias_shares(at: &[Node<'_>], ids: &[usize], put: &mut dyn FnMut(u32)) {
    let mut n = 0;
    for (i, (&id, a)) in ids.iter().zip(at).enumerate() {
        if id != a.as_raw().as_ptr() as usize {
            put(0x3642);
            put(i as u32);
            n += 1;
        }
    }
    if n > 0 {
        put(n);
    }
}

/// D3642: one literal's share identity. PostgreSQL's planner gives each arm of an inheritance or partition tree its own
/// copy of a statement's literal (`adjust_appendrel_attrs`), keeping its location: copies of one literal at one location,
/// type, typmod, collation and exact image are one value, so lowerv4 places one word for them all (`Px::lit_word`), and
/// both keys hold the copies as one aliased node (`alias`), so a hit's copies are equal too. A literal the planner made
/// (location -1) has none.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LitShare<'a> {
    location: i32,
    typ: u32,
    typmod: i32,
    coll: u32,
    len: i32,
    byval: bool,
    null: bool,
    value: u64,
    image: &'a [u8],
}

/// `n`'s share identity (`LitShare`): `None` for a node other than a Const, a literal without a location, or a
/// by-reference one of no readable image.
pub fn lit_share<'a>(n: Node<'a>) -> Option<LitShare<'a>> {
    let c = n.as_const()?;
    if c.location < 0 {
        return None;
    }
    let (value, image): (u64, &'a [u8]) = if c.constisnull {
        (0, &[])
    } else if c.constbyval {
        (gravity_expr::nodes::konst::word_canon(c.consttype, c.constvalue).as_u64(), &[])
    } else {
        (0, super::raw::const_image(c)?)
    };
    Some(LitShare { location: c.location, typ: c.consttype, typmod: c.consttypmod, coll: c.constcollid, len: c.constlen, byval: c.constbyval, null: c.constisnull, value, image })
}

/// D3642: each node's identity for `alias`: its address, or for a literal with copies (`lit_share`) its first copy's.
/// D4449: over the nodes `nodes()` yields (a walk's, or those outside its runs: `Kept`), positions counted in their order.
/// Both lists are arena `a`'s.
pub fn alias_ids<'p, I: Iterator<Item = Node<'p>>>(a: ArenaRef, nodes: impl Fn() -> I) -> ArenaVec<usize> {
    let mut ids = ArenaVec::with_capacity_in(nodes().size_hint().0, a);
    nodes().for_each(|n| ids.push(n.as_raw().as_ptr() as usize));
    let mut lits: ArenaVec<(LitShare<'_>, u32)> = ArenaVec::new_in(a);
    nodes().enumerate().filter_map(|(i, n)| lit_share(n).map(|s| (s, i as u32))).for_each(|l| lits.push(l));
    if lits.len() > 1 && !first_copies(a, &lits, &mut ids) {
        first_copies_sorted(&mut lits, &mut ids);
    }
    ids
}

/// D4449: `alias_ids`' copies in one pass over `lits` (position order): a literal's first copy is the first literal
/// of its location and type, which every later one of equal share (`LitShare`) reads as. False where two literals of one
/// location and type are no copies of one value: the sort decides then (`first_copies_sorted`). The ids this pass set
/// before it met that are the sort's own: each is a later copy of the first literal of its location and type, which is
/// the first of its share too, and no first is ever set.
fn first_copies(a: ArenaRef, lits: &[(LitShare<'_>, u32)], ids: &mut [usize]) -> bool {
    let mut first = PosTable::with_capacity_in(lits.len(), a);
    for (k, (s, p)) in lits.iter().enumerate() {
        match first.first((u64::from(s.typ) << 32) | u64::from(s.location as u32), k as u32) {
            None => {}
            Some(f) if lits[f as usize].0 == *s => ids[*p as usize] = ids[lits[f as usize].1 as usize],
            Some(_) => return false,
        }
    }
    true
}

/// D3642: `alias_ids`' copies by sorting the literals by share then position: each takes its share's first position's
/// identity.
fn first_copies_sorted(lits: &mut [(LitShare<'_>, u32)], ids: &mut [usize]) {
    lits.sort_unstable();
    let mut first = 0;
    for i in 1..lits.len() {
        if lits[i].0 == lits[first].0 {
            ids[lits[i].1 as usize] = ids[lits[first].1 as usize];
        } else {
            first = i;
        }
    }
}

/// mergecmd record-row cache: each record row (decompose `subplan::RecordRow`: a subquery's, VALUES', function's or named
/// tuplestore's whole row read as a `record`) by its columns' names, types, typmods and collations, the descriptor lowerv4
/// step/expr_row.rs builds and blesses (C `ExecTypeFromTL` + `ExecTypeSetColNames` + `BlessTupleDesc`): two statements
/// that differ only in an alias or a column's type never share its program (the front key hashes the RTE aliases the
/// names come from, front.rs). The expressions are not visited (their words are the scan's); nothing is hashed for a
/// graph without one, so every other key is unchanged.
fn record_rows(h: &mut Fnv128, graph: &PipelineGraph<'_>) {
    use gravity_expr::nodes::func::{expr_collation, expr_type, expr_typmod};
    for r in graph.record_rows.iter() {
        (r.scanrelid, r.node_id, r.named, r.cols.len() as u32).hash(h);
        for c in r.cols.iter() {
            let ty = c.expr.map_or(c.ty, |e| (expr_type(e), expr_typmod(e), expr_collation(e)));
            (c.resno, ty, c.name).hash(h);
        }
    }
}

#[cfg(test)]
mod estimate_key {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::{Op, PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, ArenaRef, End, Pipeline};
    use types_nodes::list::IntList;
    use types_nodes::NodeList;

    use super::{cost_decide, costs_hold, decide, decide_end_rate, guarded, guards_hold, lits_hold, place_lits, variant, Est};
    use gravity_decompose::NoFacts;

    /// A one-pipeline graph whose pipeline predicts `rows`, ending in a hash build of `build_rows` with PG's tuple bound
    /// `pg_tuple` when that is given.
    fn graph_of(a: ArenaRef, rows: u64, build: Option<(u64, u32)>) -> PipelineGraph<'static> {
        let none: &'static NodeList<'static> = Box::leak(Box::default());
        let mut g = PipelineGraph::new(a);
        let mut p = PipelineNode::new(a, 1, Source::Const, Pipeline::eligible(0, End::Sink, rows));
        if let Some((build_rows, pg_tuple)) = build {
            p.desc.build_rows = build_rows;
            p.push(Op::Join(JoinDesc::Build { keys: none, decls: gravity_program::ArenaVec::new_in(a), pg_tuple }));
        }
        g.pipelines.push(p);
        g
    }

    fn key_of(rows: u64, build: Option<(u64, u32)>) -> gravity_program::ContentKey {
        let arena = Arena::new(1 << 16);
        // a local, not a tail temporary: under edition 2021 the tail's temporaries drop after `arena`, so the graph's
        // vectors would drop in a freed block
        let g = graph_of(arena.handle(), rows, build);
        super::key(&g)
    }

    /// KEY-PREDICTED-ROWS road (b): no estimate is in the key (a rolled-back MERGE grew the target from 10,000 to
    /// 10,800 rows, an UPDATE doubled it: one program); the build's tuple bound (the plan's width) is.
    #[test]
    fn no_estimate_is_in_the_key() {
        assert_eq!(key_of(10_000, None), key_of(10_800, None));
        assert_eq!(key_of(10_000, None), key_of(20_000_000, None));
        assert_eq!(key_of(0, None), key_of(1, None));
        assert_eq!(key_of(100, Some((300, 48))), key_of(100, Some((310, 48))));
        assert_eq!(key_of(100, Some((0, 48))), key_of(100, Some((9_000, 48))));
        assert_ne!(key_of(100, Some((300, 48))), key_of(100, Some((300, 56))), "the tuple bound is keyed");
    }

    fn at_least(rows: u64, a: &[u64; 4]) -> u64 {
        (rows >= a[0]) as u64
    }

    /// A decision made through `decide` inside `guarded` is filed once per distinct decision and re-run on another
    /// graph's estimate: the program serves it only where the decision comes out the same.
    #[test]
    fn a_guard_decides_again_on_the_fresh_estimate() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let filed = graph_of(a, 5_000, Some((300, 48)));
        let (made, guards) = guarded(|| {
            let p = &filed.pipelines[0].desc;
            (decide(0, false, p.predicted_rows, at_least, [4_096, 0, 0, 0]), decide(0, false, p.predicted_rows, at_least, [4_096, 0, 0, 0]), decide(0, true, p.build_rows, at_least, [1_000, 0, 0, 0]))
        });
        assert_eq!(made, (1, 1, 0));
        assert_eq!(guards.len(), 2, "the same decision twice is one guard");
        assert!(guards_hold(&guards, &filed, &NoFacts));
        assert!(guards_hold(&guards, &graph_of(a, 9_000_000, Some((999, 48))), &NoFacts), "both decisions unchanged");
        assert!(!guards_hold(&guards, &graph_of(a, 4_095, Some((300, 48))), &NoFacts), "the rows decision flips");
        assert!(!guards_hold(&guards, &graph_of(a, 5_000, Some((1_000, 48))), &NoFacts), "the build decision flips");
        assert_eq!(decide(0, false, 1, at_least, [4_096, 0, 0, 0]), 0, "outside `guarded` a decision files nothing");
        assert!(guarded(|| ()).1.is_empty());
    }

    /// A graph with pgrc scan node 5 costing `scan` and two InitPlans, plan 1 setting PARAM_EXEC 7 and plan 2 setting 8,
    /// charging `c1` and `c2` (`None`: a cost the graph does not hold).
    fn costed<'p>(a: ArenaRef, sets: (&'p IntList<'p>, &'p IntList<'p>), scan: f64, c1: Option<f64>, c2: f64) -> PipelineGraph<'p> {
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::InitPlan;
        use gravity_decompose::ops::shared::subplan::SubLink;
        let mut g = PipelineGraph::new(a);
        for (plan_id, params) in [(1, sets.0), (2, sets.1)] {
            g.init_plans.push(InitPlan { plan_id, pipeline: 0, link: SubLink::Expr, param: params.nth(0), params, slot: None, volatility: Volatility::Stable, raise_free: true, parallel_safe: true, attach: 5 });
        }
        g.scan_costs.push((5, scan));
        if let Some(c) = c1 {
            g.init_costs.push((1, c));
        }
        g.init_costs.push((2, c2));
        g
    }

    /// PS1-05 mechanism C: the eager InitPlans of a pgrc scan, decided on planner costs (decompose `eager_initplans`: each
    /// InitPlan counted once, the sum under the scan's cost), are one guard decided again on another graph's costs, or on a
    /// front hit's plan (`costs_hold`), never a literal guard (`place_lits` keeps it, `lits_hold` reads none).
    #[test]
    fn a_cost_guard_decides_again_on_the_fresh_costs() {
        let ctx = mcx::MemoryContext::new("cost guard");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let sets: (&IntList<'_>, &IntList<'_>) = (Box::leak(Box::new(IntList::make1(ctx.mcx(), 7).unwrap())), Box::leak(Box::new(IntList::make1(ctx.mcx(), 8).unwrap()))); // alloc: Cold
        let filed = costed(a, sets, 100.0, Some(30.0), 60.0);
        let (made, mut guards) = guarded(|| (cost_decide(&filed, 5, &[7, 8, 7]), cost_decide(&filed, 5, &[7, 8, 7])));
        assert_eq!(made, (0b111, 0b111), "30 + 60 < 100, and 7 again adds nothing");
        assert_eq!(guards.len(), 1, "the same decision twice is one guard");
        // a front hit decides it on its plan's costs (the walk's `scan_charge` / `InitCharge::of`), as on the graph
        let charge = |plan_id, sets, cost| gravity_decompose::graph::InitCharge { plan_id, sets, cost };
        let inits = [charge(1, sets.0, Some(30.0)), charge(2, sets.1, Some(60.0))];
        assert!(costs_hold(&guards, &[(4, 1.0), (5, 100.0)], &inits));
        assert!(!costs_hold(&guards, &[(5, 80.0)], &inits), "plan 2 no longer fits");
        assert!(!costs_hold(&guards, &[(4, 100.0)], &inits), "no cost for the scan");
        assert!(!costs_hold(&guards, &[(5, 100.0)], &[charge(1, sets.0, None), inits[1]]), "a CTE's charge is none");
        assert!(costs_hold(&[], &[], &[]), "no cost guard: nothing to decide");
        place_lits(&mut guards, &[]);
        assert!(guards_hold(&guards, &filed, &NoFacts) && lits_hold(&guards, &[]), "a cost guard is no literal's");
        assert!(guards_hold(&guards, &costed(a, sets, 1_000.0, Some(10.0), 10.0), &NoFacts), "the costs moved, the decision did not");
        assert!(!guards_hold(&guards, &costed(a, sets, 80.0, Some(30.0), 60.0), &NoFacts), "plan 2 no longer fits");
        assert!(!guards_hold(&guards, &costed(a, sets, 100.0, None, 60.0), &NoFacts), "a cost the graph does not hold is no");
        assert_eq!(costed(a, sets, 80.0, Some(30.0), 60.0).eager_initplans(5, &[7, 8, 7]), 0b101);
        assert_eq!(costed(a, sets, 30.0, Some(30.0), 1.0).eager_initplans(5, &[7, 8]), 0b10, "not under: equal is no");
        assert_eq!(costed(a, sets, 100.0, Some(1.0), 1.0).eager_initplans(6, &[7]), 0, "no cost for the scan");
        let k = 0x1234_5678_9abc_def0_0fed_cba9_8765_4321u128;
        assert_eq!(variant(k, &guards, Some((&costed(a, sets, 80.0, Some(30.0), 60.0), &NoFacts))), variant(k, &guarded(|| cost_decide(&costed(a, sets, 80.0, Some(30.0), 60.0), 5, &[7, 8, 7])).1, None), "a flipped guard: the program lowered for it");
        assert_eq!(guarded(|| cost_decide(&filed, 5, &[8, 8, 8, 8, 8, 8, 7])).0, 0b11_1111, "an id past COST_IDS stays lazy");
    }

    /// Programs of one content key whose decisions differ are filed side by side: the variant key is the outcomes', so a
    /// graph whose estimates decide as a filed program's finds it there, and one that flips a guard finds another's.
    #[test]
    fn the_variant_key_is_the_outcomes() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let filed = graph_of(a, 5_000, None);
        let (_, guards) = guarded(|| decide(0, false, filed.pipelines[0].desc.predicted_rows, at_least, [4_096, 0, 0, 0]));
        let (_, small) = guarded(|| decide(0, false, 100, at_least, [4_096, 0, 0, 0]));
        let k = 0x1234_5678_9abc_def0_0fed_cba9_8765_4321u128;
        assert_eq!(variant(k, &[], Some((&filed, &NoFacts))), k, "no guards: the content key");
        assert_ne!(variant(k, &guards, None), k);
        assert_eq!(variant(k, &guards, None), variant(k, &guards, Some((&graph_of(a, 9_000_000, None), &NoFacts))), "the same outcome");
        assert_eq!(variant(k, &guards, Some((&graph_of(a, 100, None), &NoFacts))), variant(k, &small, None), "a flipped guard: the program lowered for it");
        assert_ne!(variant(k, &guards, None), variant(k, &small, None));
    }

    /// NEW-48: `end_rate` is an estimate like the rows: out of the key, and a decision on it is a guard of its own selector.
    /// Two estimates of one key that decide the carry apart file two programs side by side (two variant keys); estimates
    /// on one side of the threshold share one, whatever their values.
    #[test]
    fn an_end_rate_decision_is_a_guard_of_its_own() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let rated = |rate: u64| {
            let mut g = graph_of(a, 60_000, None);
            g.pipelines[0].desc.end_rate = rate;
            g
        };
        let (low, high) = (rated(40_000), rated(1_000_000));
        assert_eq!(super::key(&low), super::key(&high), "no end_rate in the key");
        assert_eq!(super::key(&low), super::key(&rated(gravity_program::END_RATE_NONE)));
        let carry = |g: &PipelineGraph<'_>| guarded(|| decide_end_rate(0, g.pipelines[0].desc.end_rate, at_least, [450_000, 0, 0, 0]));
        let ((off, gl), (on, gh)) = (carry(&low), carry(&high));
        assert_eq!((off, on), (0, 1));
        assert_eq!((gl.len(), gh.len()), (1, 1));
        assert_eq!(gl[0].est, Est::EndRate);
        assert!(guards_hold(&gl, &rated(410_000), &NoFacts) && !guards_hold(&gl, &rated(500_000), &NoFacts), "re-decided on the fresh end_rate");
        assert!(guards_hold(&gh, &rated(gravity_program::END_RATE_NONE), &NoFacts), "no loop past the source: the carry");
        let mut big = graph_of(a, 9_000_000, None);
        big.pipelines[0].desc.end_rate = 40_000;
        assert!(guards_hold(&gl, &big, &NoFacts), "the rows estimate is not this guard's");
        let k = super::key(&low).0;
        assert_ne!(variant(k, &gl, None), variant(k, &gh, None), "two outcomes: two programs");
        assert_eq!(variant(k, &gl, Some((&rated(100), &NoFacts))), variant(k, &gl, None), "the same outcome: the same program");
    }
}

#[cfg(test)]
mod arbiter_key {
    use core::hash::Hasher;

    use gravity_program::{Arena, Cmd, Conflict, WritePlan};

    /// Two ON CONFLICT statements of one shape with different conflict targets (`on conflict (key)` /
    /// `on conflict (fruit)`) must not share a program: the arbiters and the action are in the plan's key.
    #[test]
    fn the_write_plan_key_carries_the_arbiters_and_the_action() {
        let arena = Arena::new(1 << 12);
        let a = arena.handle();
        let hash = |arbiters: &[u32], conflict: Conflict| {
            let p = WritePlan::write(a, Cmd::Insert, 1000, &[]).with_conflict(a, conflict, arbiters);
            let mut h = super::Fnv128::new(0xcbf2_9ce4_8422_2325, 0x84222325_cbf29ce4, a);
            super::write_plan(&mut h, &p);
            h.finish()
        };
        assert_ne!(hash(&[2001], Conflict::Update), hash(&[2002], Conflict::Update));
        assert_ne!(hash(&[2001], Conflict::Update), hash(&[2001], Conflict::Nothing));
        assert_ne!(hash(&[], Conflict::Nothing), hash(&[2001], Conflict::Nothing));
        assert_eq!(hash(&[2001], Conflict::Update), hash(&[2001], Conflict::Update));
    }
}

#[cfg(test)]
mod merge_key {
    use core::hash::Hasher;

    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    use types_nodes::NodeList;

    /// M11: two merge joins that differ only by a clause's btree family (text_ops 1994 vs text_pattern_ops 2095, whose
    /// orders differ) or its collation never share a program: both are in the join's key.
    #[test]
    fn the_merge_key_carries_the_family_and_the_collation() {
        fn hash(families: &[u32], collations: &[u32]) -> u64 {
            let (none, flags) = (NodeList::default(), [false]);
            let d = JoinDesc::Merge { slot: 0, jointype: JoinType::JOIN_INNER, clauses: &none, reversals: &flags, nulls_first: &flags, families, collations, qual: &none, unique: false, inner_node: 1 };
            let arena = gravity_program::Arena::new(1 << 12);
            let mut h = super::Fnv128::new(0xcbf2_9ce4_8422_2325, 0x84222325_cbf29ce4, arena.handle());
            super::join(&mut h, &mut |_| {}, &d);
            h.finish()
        }
        assert_ne!(hash(&[1994], &[100]), hash(&[2095], &[100]), "text_ops vs text_pattern_ops");
        assert_ne!(hash(&[1994], &[100]), hash(&[1994], &[950]), "the default collation vs C");
        assert_eq!(hash(&[1994], &[100]), hash(&[1994], &[100]));
    }
}

#[cfg(test)]
mod lit_share {
    use datum::Datum;
    use types_nodes::primnodes::Const;
    use types_nodes::Node;

    use super::{alias_ids, lit_share};

    fn lit<'m>(mcx: mcx::Mcx<'m>, typ: u32, len: i32, value: Datum, byval: bool, location: i32) -> Node<'m> {
        lit_or_null(mcx, typ, len, value, byval, location, false)
    }

    fn lit_or_null<'m>(mcx: mcx::Mcx<'m>, typ: u32, len: i32, value: Datum, byval: bool, location: i32, null: bool) -> Node<'m> {
        Node::mk(mcx, Const { consttype: typ, consttypmod: -1, constcollid: 0, constlen: len, constvalue: value, constisnull: null, constbyval: byval, location }).unwrap()
    }

    /// A varlena image with a 1-byte header (`len` < 127 bytes in all).
    fn short(bytes: &[u8]) -> &'static [u8] {
        let mut v = vec![(((bytes.len() + 1) << 1) | 1) as u8]; // alloc: Cold
        v.extend_from_slice(bytes);
        Box::leak(v.into_boxed_slice())
    }

    /// D3642: the copies of one literal share an identity; the same value of another type, image or location, or a
    /// literal the planner made (location -1), never does.
    #[test]
    fn copies_of_one_literal_share_and_nothing_else_does() {
        let ctx = mcx::MemoryContext::new("lit share");
        let m = ctx.mcx();
        let (int4, int8, numeric) = (23, 20, 1700);
        let a = lit(m, int4, 4, Datum::from_i32(1), true, 30);
        let b = lit(m, int4, 4, Datum::from_i32(1), true, 30);
        assert_eq!(lit_share(a), lit_share(b), "two copies");
        assert!(lit_share(a).is_some());
        assert_ne!(lit_share(a), lit_share(lit(m, int8, 8, Datum::from_i64(1), true, 30)), "int4 1 vs int8 1");
        assert_ne!(lit_share(a), lit_share(lit(m, int4, 4, Datum::from_i32(2), true, 30)), "another value");
        assert_ne!(lit_share(a), lit_share(lit(m, int4, 4, Datum::from_i32(1), true, 31)), "another literal");
        assert_eq!(lit_share(lit(m, int4, 4, Datum::from_i32(1), true, -1)), None, "the planner's");
        // two by-reference images of one numeric value at two display scales (1.0 vs 1.00): different bytes
        let one_0 = short(&[0x80, 0x01, 0x01, 0x00]);
        let one_00 = short(&[0x80, 0x02, 0x01, 0x00]);
        let n1 = lit(m, numeric, -1, Datum::from_usize(one_0.as_ptr() as usize), false, 12);
        let n2 = lit(m, numeric, -1, Datum::from_usize(one_00.as_ptr() as usize), false, 12);
        let n3 = lit(m, numeric, -1, Datum::from_usize(short(&[0x80, 0x01, 0x01, 0x00]).as_ptr() as usize), false, 12);
        assert_ne!(lit_share(n1), lit_share(n2), "1.0 vs 1.00");
        assert_eq!(lit_share(n1), lit_share(n3), "one image at two addresses");
        // the keys' alias: a copy is its first copy's node; the rest keep their own
        let walk = [a, n1, b, n2, n3];
        let arena = gravity_program::Arena::new(1 << 16);
        let ids = alias_ids(arena.handle(), || walk.iter().copied());
        let addr = |n: Node<'_>| n.as_raw().as_ptr() as usize;
        assert_eq!(ids[..], [addr(a), addr(n1), addr(a), addr(n2), addr(n1)]);
        // one node at two places and two copies of one literal alias the same positions, yet are two shapes
        let alias = |at: &[Node<'_>]| {
            let mut h = super::Fnv128::new(1, 2, arena.handle());
            super::alias(&mut h, at, &[]);
            h.key()
        };
        assert_ne!(alias(&[a, a]), alias(&[a, b]), "a node twice vs two copies");
        assert_eq!(alias(&[a, b]), alias(&[n1, n3]), "two copies, whatever the literal");
        assert_eq!(alias(&[a, n2]), alias(&[a, lit(m, int4, 4, Datum::from_i32(5), true, 31)]), "no copies: as before");
    }

    /// The sort-based `alias_ids` the linear pass replaced (D4449): the reference it must equal.
    fn ids_sorted(at: &[Node<'_>]) -> Vec<usize> {
        let mut ids: Vec<usize> = at.iter().map(|n| n.as_raw().as_ptr() as usize).collect(); // alloc: Cold
        let mut lits: Vec<_> = at.iter().enumerate().filter_map(|(i, n)| lit_share(*n).map(|s| (s, i as u32))).collect(); // alloc: Cold
        if lits.len() > 1 {
            lits.sort_unstable();
            let mut first = 0;
            for i in 1..lits.len() {
                if lits[i].0 == lits[first].0 {
                    ids[lits[i].1 as usize] = ids[lits[first].1 as usize];
                } else {
                    first = i;
                }
            }
        }
        ids
    }

    /// The sort-based repeats the linear pass replaced (D4449): every adjacent pair of one identity's positions as
    /// (later, earlier), by the later position.
    fn repeats_sorted(ids: &[usize]) -> Vec<(u32, u32)> {
        let mut by: Vec<(usize, u32)> = ids.iter().enumerate().map(|(i, &a)| (a, i as u32)).collect(); // alloc: Cold
        by.sort_unstable();
        let mut repeats: Vec<(u32, u32)> = by.windows(2).filter(|p| p[0].0 == p[1].0).map(|p| (p[1].1, p[0].1)).collect(); // alloc: Cold
        repeats.sort_unstable();
        repeats
    }

    /// D4449: the linear `alias_ids` and `alias_repeats` equal the sorts they replaced on random walks of one
    /// pool of nodes: copies of one literal (by-value and by-reference), a node at several places, two literals of one
    /// location and type that are no copies (the sort's fallback), literals of no location, NULLs.
    #[test]
    fn the_linear_alias_passes_equal_the_sorts() {
        let ctx = mcx::MemoryContext::new("alias property");
        let m = ctx.mcx();
        let (int4, int8, text) = (23, 20, 25);
        let img = |s: &[u8]| Datum::from_usize(short(s).as_ptr() as usize);
        let mut pool: Vec<Node<'_>> = Vec::new(); // alloc: Cold
        for loc in 0..12 {
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // alloc: Cold
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // a copy
            pool.push(lit(m, int8, 8, Datum::from_i64(1), true, loc * 10)); // another type, one location
            pool.push(lit(m, text, -1, img(b"abc"), false, loc * 10 + 1));
            pool.push(lit(m, text, -1, img(b"abc"), false, loc * 10 + 1)); // a copy at another address
            pool.push(lit(m, int4, 4, Datum::from_i32(1), true, -1)); // the planner's: no share
        }
        let clash = lit(m, int4, 4, Datum::from_i32(99), true, 0); // location 0, int4: no copy of pool[0]
        let null = lit_or_null(m, int4, 4, Datum::from_i32(0), true, 50, true);
        pool.push(null); // alloc: Cold
        let mut seed = 0x0123_4567_89ab_cdefu64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let addr = |n: Node<'_>| n.as_raw().as_ptr() as usize;
        let arena = gravity_program::Arena::new(1 << 20);
        let a = arena.handle();
        for round in 0..2000 {
            let _round = a.scope();
            let len = next(64);
            let mut at: Vec<Node<'_>> = (0..len).map(|_| pool[next(pool.len())]).collect(); // alloc: Cold
            if round % 7 == 0 && len > 0 {
                let i = next(len);
                at.insert(i, clash);
            }
            let ids = alias_ids(a, || at.iter().copied());
            assert_eq!(ids[..], ids_sorted(&at)[..], "round {round}: alias_ids");
            let mut linear: Vec<(u32, u32)> = Vec::new(); // alloc: Cold
            super::alias_repeats(a, &ids, &mut |i, j| linear.push((i, j))); // alloc: Cold
            assert_eq!(linear, repeats_sorted(&ids), "round {round}: repeats");
            // the walk's own positions too (exec front.rs `file`'s identity table)
            let raw: Vec<usize> = at.iter().map(|n| addr(*n)).collect(); // alloc: Cold
            let mut linear: Vec<(u32, u32)> = Vec::new(); // alloc: Cold
            super::alias_repeats(a, &raw, &mut |i, j| linear.push((i, j))); // alloc: Cold
            assert_eq!(linear, repeats_sorted(&raw), "round {round}: repeats by address");
        }
    }

    /// D4600: `alias_walk_fused` puts what `alias_walk` puts, word for word, on random walks of one pool of nodes:
    /// a node at several places, copies of one literal (by-value and by-reference), two literals of one location and type
    /// that are no copies (its fallback), literals of no location, NULLs, more literals than its inline table (fallback),
    /// and walks with literal-table runs (fallback).
    #[test]
    fn the_fused_alias_pass_puts_what_alias_walk_puts() {
        use super::{alias_walk, alias_walk_fused, AliasPut};
        let ctx = mcx::MemoryContext::new("alias fused property");
        let m = ctx.mcx();
        let (int4, int8, text) = (23, 20, 25);
        let img = |s: &[u8]| Datum::from_usize(short(s).as_ptr() as usize);
        let mut pool: Vec<Node<'_>> = Vec::new(); // alloc: Cold
        for loc in 0..12 {
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // alloc: Cold
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // a copy
            pool.push(lit(m, int8, 8, Datum::from_i64(1), true, loc * 10)); // another type, one location
            pool.push(lit(m, text, -1, img(b"abc"), false, loc * 10 + 1));
            pool.push(lit(m, text, -1, img(b"abc"), false, loc * 10 + 1)); // a copy at another address
            pool.push(lit(m, int4, 4, Datum::from_i32(1), true, -1)); // the planner's: no share
        }
        // non-literal nodes (a literal of no location keys no share): most of a real walk
        for k in 0..40 {
            pool.push(lit(m, int4, 4, Datum::from_i32(k), true, -1)); // alloc: Cold
        }
        let clash = lit(m, int4, 4, Datum::from_i32(99), true, 0); // location 0, int4: no copy of pool[0]
        pool.push(lit_or_null(m, int4, 4, Datum::from_i32(0), true, 50, true)); // alloc: Cold
        let mut seed = 0x5eed_4600_0000_0001u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let words = |p: AliasPut| match p {
            AliasPut::Pair(i, j) => (0u8, i as u64, j as u64),
            AliasPut::Count(n) => (1, n as u64, 0),
            AliasPut::Share(v) => (2, v as u64, 0),
        };
        let arena = gravity_program::Arena::new(1 << 20);
        let a = arena.handle();
        let mut fused_taken = 0;
        for round in 0..3000 {
            let _round = a.scope();
            let len = next(80);
            let mut at: Vec<Node<'_>> = (0..len).map(|_| pool[next(pool.len())]).collect(); // alloc: Cold
            if round % 7 == 0 && len > 0 {
                let i = next(len);
                at.insert(i, clash);
            }
            let tables: Vec<(u32, u32)> = if round % 11 == 0 && at.len() > 4 { vec![(1, 3)] } else { Vec::new() }; // alloc: Cold
            let mut old: Vec<(u8, u64, u64)> = Vec::new(); // alloc: Cold
            alias_walk(a, &at, &tables, &mut |p| old.push(words(p))); // alloc: Cold
            let mut new: Vec<(u8, u64, u64)> = Vec::new(); // alloc: Cold
            alias_walk_fused(a, &at, &tables, &mut |p| new.push(words(p))); // alloc: Cold
            assert_eq!(new, old, "round {round}: len {} tables {tables:?}", at.len());
            let mut probe: Vec<(u8, u64, u64)> = Vec::new(); // alloc: Cold
            if tables.is_empty() && super::alias_fused(a, &at, &mut |p| probe.push(words(p))) {
                fused_taken += 1;
                assert_eq!(probe, old, "round {round}: the fused pass itself");
            }
        }
        assert!(fused_taken > 1000, "the fused pass decided {fused_taken} of 3000 walks");
    }

    /// D4449: `alias_walk` over a walk with literal-table runs (fresh Consts, each at one position, no literal's copy:
    /// location -1 or a location of its own) puts what it puts with no run: the same pairs, count and shares at the same
    /// positions, whatever the runs' places and lengths; a list met twice (`Runs::add` by one address) makes no run.
    #[test]
    fn skipping_literal_table_runs_puts_the_same() {
        use super::{alias_walk, AliasPut, Runs};
        let ctx = mcx::MemoryContext::new("alias runs property");
        let m = ctx.mcx();
        let int4 = 23;
        let mut pool: Vec<Node<'_>> = Vec::new(); // alloc: Cold
        for loc in 0..10 {
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // alloc: Cold
            pool.push(lit(m, int4, 4, Datum::from_i32(loc), true, loc * 10)); // a copy
            pool.push(lit(m, int4, 4, Datum::from_i32(1), true, -1)); // alloc: Cold
        }
        let mut seed = 0x0fed_cba9_8765_4321u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        let arena = gravity_program::Arena::new(1 << 20);
        let puts = |at: &[Node<'_>], runs: &[(u32, u32)]| {
            let _walk = arena.handle().scope();
            let mut v: Vec<(u8, u64)> = Vec::new(); // alloc: Cold
            alias_walk(arena.handle(), at, runs, &mut |p| match p {
                AliasPut::Pair(i, j) => v.push((0, (u64::from(i) << 32) | u64::from(j))), // alloc: Cold
                AliasPut::Count(n) => v.push((1, n as u64)), // alloc: Cold
                AliasPut::Share(x) => v.push((2, u64::from(x))), // alloc: Cold
            });
            v
        };
        let mut fresh = 1000;
        for round in 0..1000 {
            let mut at: Vec<Node<'_>> = (0..next(40)).map(|_| pool[next(pool.len())]).collect(); // alloc: Cold
            let mut runs = Runs::NONE;
            let k = next(4);
            let mut cuts: Vec<usize> = (0..k).map(|_| next(at.len() + 1)).collect(); // alloc: Cold
            cuts.sort_unstable();
            let mut shift = 0;
            for (r, c) in cuts.into_iter().enumerate() {
                let len = 1 + next(12);
                let start = c + shift;
                for i in 0..len {
                    fresh += 1;
                    let loc = if i % 2 == 0 { -1 } else { fresh };
                    at.insert(start + i, lit(m, int4, 4, Datum::from_i32(7), true, loc));
                }
                runs.add(start as u32, (start + len) as u32, r + 1);
                shift += len;
            }
            assert_eq!(puts(&at, runs.as_slice()), puts(&at, &[]), "round {round}: runs {:?}", runs.as_slice());
            let mut twice = runs;
            twice.add(0, 1, 1);
            assert!(k == 0 || twice.as_slice().is_empty(), "round {round}: a list met twice makes no run");
        }
    }
}
