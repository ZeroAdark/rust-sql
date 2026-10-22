//! Road `percode.table`'s key form (hash-contract.md §3.2 rule A): a GROUP BY or DISTINCT key that
//! is a text function of one dictionary-coded lane (e.g. `regexp_replace(col, ..)`) keys by the Fp128 of its result.
//! A `PROGRAM` table of `ELEM_FP128` over the lane's dictionary is filled by the rows that read it (DK1, percode_fill.rs
//! `lazy`: every entry `[0, 0]` at the part's bind; the row's re-evaluation stores its hash at its code), or swept once
//! per part when countwalk reads it whole or every row of the part reads it (STEP-04; percode_bind.rs `sweep`), each entry's result hashed by host `text_fp`, the one rule A image (the cluster seed over the payload): equal
//! results of different entries hash equal, and equal to a base text key's Fp128 over the same bytes (SOURCE declares
//! that `TableDecl::fingerprint` table through the same `declare_table`). An entry that raised, came out NULL or has no
//! render stays `[0, 0]`: its rows re-evaluate the key over the row and hash the result with the same host (a NULL
//! result: zero words, the cell NULL), so an error is PG's and raised only where PG evaluates, and a value lands in one
//! group whichever part swept it. The row's image goes to a one-entry row table past the site's tables in the bind's out
//! array (`Tables::rows`); the key cell is `Rep::Code` over that table at code 0, which rep.code.hash reads at Program
//! scope. The cell stands for its image alone (its render and order would be the source entry's), so only a column read
//! as a key alone takes the form (`key_only`; the emit re-evaluates a derived key over its representative). Text and
//! varchar results under a deterministic collation only. A 3-argument `regexp_replace(column, 'p', 'r')`, or a flags form
//! PostgreSQL reads as the advanced RE's (`'g'` every match, `'i'`; `derived`), is the hosts' `DERIVED_FINGERPRINT` table
//! instead, and any other immutable builtin of the column whose other arguments are varlena constants the hosts'
//! `DERIVED_CALL` table: built per part by each source that reads it, no sweep; a faulted entry `[0, 0]`.
//! A call key whose rows pass a Filter or Gate first and are predicted to read under half the column's entries (`filtered`,
//! `sparse`) declares the `DERIVED_CALL` table `FLAG_ADAPTIVE` ((2'), DK1-MISEST) with a lazy sweep: each source publishes
//! it unfilled, as a `PROGRAM` table its surviving rows fill, until one part's rows fill at least half its entries, then
//! builds it hosted for its later parts (hosts granule_source/lanes.rs), so a misestimated filter pays one part's fill.
//! Budget 200.
use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::breaker::KeyKind;
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::agg::FinalForm;
use gravity_decompose::Op;
use gravity_dict::lane::ELEM_FP128;
use gravity_expr::{Compiled, ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{self as abi, BIND_OUT_WORDS};
use gravity_program::{table_fn, DictRef, NumTypmod, Rep, TableDecl, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::primnodes::{ParamKind, OUTER_VAR};
use types_nodes::Node;

use crate::dict::percode::{Class, Sweep, Tables};
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::dict::face::PerValue;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;


/// A key expression's Fp128 form (its `Ex::ext`): site table `t`, the row's code column `col`, row table `r` of `tabs`.
pub(crate) struct KeyFp {
    t: u16,
    r: u8,
    col: i16,
    tabs: Rc<RefCell<Tables>>,
    /// C5 (face/codes.rs `PathKey`): a jsonb path key, whose NULL lane rows keep their text in row table `r + 1`.
    stash: bool,
    /// The hosts' `DERIVED_CALL` table: filled by the source, no sweep.
    call: bool,
}

/// A key the hosts evaluate per entry: the derived `regexp_replace` (pattern, replacement) or another immutable call, each
/// with its input collation.
pub(crate) enum Hosted<'g> {
    Regexp(Derived<'g>),
    Call(gravity_program::table_fn::Call, u32),
}

impl<'g> Hosted<'g> {
    fn of(facts: &dyn gravity_decompose::catalog::Facts, e: Node<'_>, mcx: Mcx<'g>) -> Option<Hosted<'g>> {
        derived(e, mcx).map(Hosted::Regexp).or_else(|| call_of(facts, e).map(|(f, c)| Hosted::Call(f, c)))
    }
}

/// The bind's out words for `n` site tables and `rows` row tables: the tables, the flags slots, the row tables, their entries.
pub(crate) fn out_words(n: usize, rows: u8) -> usize {
    BIND_OUT_WORDS * (n + abi::bind_flag_slots(n) + 2 * rows as usize)
}

/// Row table `r`'s slot (its `DictRef::fp_table`) and the word of its entry `[lo, hi, code 0]`.
pub(crate) fn row_at(n: usize, rows: u8, r: u8) -> (usize, usize) {
    (n + abi::bind_flag_slots(n) + r as usize, BIND_OUT_WORDS * (n + abi::bind_flag_slots(n) + rows as usize + r as usize))
}

/// Each row table's slot pointed at its entry, the entry's code word zero (per window, after `agg_bind`).
pub(crate) fn bind_rows(b: &mut FunctionBuilder<'_>, env: &Env, out: Value, n: usize, rows: u8) {
    for r in 0..rows {
        let (slot, e) = row_at(n, rows, r);
        let entry = b.ptr_add(out, env.zero, 8, (e * 8) as i32);
        b.store(out, (slot * BIND_OUT_WORDS * 8) as i32, entry);
        b.store(entry, 16, env.zero);
    }
}

pub(crate) fn text_fp(b: &mut FunctionBuilder<'_>) -> HostFn {
    let flags = HostFlags { touches_memory: true, ..HostFlags::default() };
    b.declare_host_once("text_fp", &[Type::Ptr, Type::Ptr], Some(Type::I64), abi::text_fp as *const () as usize, flags)
}

pub(crate) fn as_ptr(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) }
}

/// The Fp128 of the text datum `v` at `code` of the table data `data` (the sweep's entry); a failed hash leaves `[0, 0]`,
/// so the entry's rows re-evaluate and raise where PG does.
pub(crate) fn store(b: &mut FunctionBuilder<'_>, data: Value, code: Value, v: Value) {
    let (e, v) = (b.ptr_add(data, code, 16, 0), as_ptr(b, v));
    let h = text_fp(b);
    b.call_host(h, &[v, e], Some(Type::I64));
}

/// The collation of pipeline `k`'s key at column `resno` of the row projection `j` makes, when that column is read as a key
/// alone: `j` the pipeline's last op, an `Fp128` key of the group it ends in (a group key, or a DISTINCT argument decompose
/// resolved to the entry: keyed_emit_group `key_col`) that no aggregate argument or FILTER reads, a count(DISTINCT)'s
/// argument aside (read as the pair set's image alone).
pub(crate) fn key_only(px: &Px<'_, '_>, k: usize, j: usize, resno: i16) -> Option<u32> {
    let gx = px.gx;
    let g = gx.group_at(k).filter(|_| j + 1 == gx.pipelines()[k].ops.len())?;
    let key = g.keys.iter().find(|x| x.kind == KeyKind::Fp128 && crate::table::emit_group::key_col(gx, k, x) == Some(resno))?;
    let reads = |n: Node<'_>| {
        let mut hit = false;
        let known = gravity_decompose::expr::walk(n, &mut |x| {
            hit |= x.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == resno);
            true
        });
        hit || !known
    };
    (!g.aggs.iter().any(|d| !matches!(d.final_, FinalForm::PairCount) && reads(d.node))).then_some(key.coll)
}

/// Key `e` (collation `coll`) over the row step `j` of pipeline `k` reads, compiled with its Fp128 form when it has one.
pub(crate) fn compile<'p>(px: &mut Px<'_, 'p>, k: usize, j: usize, e: Node<'p>, coll: u32, node: i32) -> ExId {
    compile_rw(px, k, j, e, coll, node, (usize::MAX, usize::MAX))
}

/// `compile` whose forwarded path reads read their lanes as step `rw` would (P1c, table/args.rs: a DISTINCT key at the
/// group's producer end; step/expr.rs `compile_ext_rw`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_rw<'p>(px: &mut Px<'_, 'p>, k: usize, j: usize, e: Node<'p>, coll: u32, node: i32, rw: (usize, usize)) -> ExId {
    let ex = crate::dict::strategies::percode_guard::finish(px, k, j, e, node, |px| crate::step::expr::compile_ext_rw(px, e, node, rw, |px, c, ex| {
        let hosted = Hosted::of(px.gx.facts, e, px.mcx());
        attach(px, k, j, c, (coll, hosted), (ex, e), node).map(|f| f as Rc<dyn Any>).or_else(|| crate::dict::strategies::percode_guard::attach(px, k, j, c, e, coll, node))
    }));
    let stash = px.placed(ex).ext.as_deref().and_then(|x| x.downcast_ref::<KeyFp>()).is_some_and(|x| x.stash);
    crate::ops::scan::pgrc::strategies::jsonb_nums::keyed(stash, px, k, e, node); // P1b: a late guarded column's key
    crate::dict::face::path_key(node).map_or(ex, |f| (f.keyed)(stash, px, k, e, node, ex))
}

/// A derived `regexp_replace` key's constants (`derived`): the pattern as the hosts read it, the replacement, the input
/// collation, and `'g'` (every match replaced).
pub(crate) struct Derived<'g> {
    pub(crate) pattern: PgVec<'g, u8>,
    pub(crate) replacement: PgVec<'g, u8>,
    pub(crate) coll: u32,
    pub(crate) global: bool,
}

impl Derived<'_> {
    /// The hosts' `DERIVED_FINGERPRINT` table over code lane `lane` of `site` (`FLAG_GLOBAL` with `'g'`).
    pub(crate) fn decl(&self, site: &mut gravity_program::SiteDesc, lane: u8) -> TableDecl {
        let args = [site.table_bytes_of(&self.pattern), site.table_bytes_of(&self.replacement)];
        let flags = if self.global { table_fn::FLAG_GLOBAL } else { 0 };
        TableDecl { over: lane, func: table_fn::DERIVED_FINGERPRINT, flags, collation: self.coll, encoding: gravity_fn::typesem::collation::database_encoding(), args }
    }
}

/// Key `e` as the hosts' derived fingerprint table, `regexp_replace(column, pattern, replacement)` in
/// its 3-argument form (oid 2284: immutable, the first match) or a flags form (oid 2285), all constant: their payload
/// bytes and the input collation. The flags are read as PostgreSQL reads them (`gravity_dict::regexp::flags_form`):
/// a form that answers as the advanced RE does, perhaps case-insensitively, perhaps for every match, takes the table;
/// any other (an invalid letter, another flavour, a flag that can change what the pattern matches) keeps the call
/// (`call_of`'s `DERIVED_CALL` table, PostgreSQL's own function per entry). A-16b: `'i'` is the pattern led by the
/// embedded option `(?i)`: an ARE's leading `(?i)` sets `REG_ICASE` for the rest, as the flag does (PostgreSQL 9.7.3.4),
/// so the hosts' function manager answers the same and the pattern program reads it
/// (`gravity_dict::regexp::compile_under`); not for a pattern that already starts with a director (`***`) or its own
/// options (`(?` and a letter): those are only read first. `'g'` is the table's `FLAG_GLOBAL` (the hosts' replace-all
/// loop, or the function manager's 4-argument call with `'g'`), dropped for an anchored pattern (`^` first, no `|`),
/// whose one match is the first, as normalize's `regexp3`.
pub(crate) fn derived<'g, 'a>(e: Node<'a>, mcx: Mcx<'g>) -> Option<Derived<'g>> {
    let e = e.as_target_entry().map_or(e, |te| te.expr);
    let strip = |n: Node<'a>| n.as_relabel_type().map_or(n, |r| r.arg);
    let text = |n: Node<'a>| {
        let c = strip(n).as_const().filter(|c| !c.constisnull && gravity_fn::typesem::text_payload(c.consttype))?;
        // V4R-18: the pattern and the replacement (and a flags form's flags) are baked into the table's key; the read logs
        // them (a variant per image); a TOAST pointer or a compressed image is refused
        gravity_decompose::expr::const_bytes(c).map(|b| mcx::to_vec_in(b, mcx))
    };
    let (args, flags) = match gravity_decompose::normalize::regexp_replace3(e) {
        Some(args) => (args, None),
        None => {
            let [s, p, r, flags] = gravity_decompose::normalize::regexp_replace4(e)?;
            ([s, p, r], Some(text(flags)?))
        }
    };
    let coll = e.as_func_expr()?.inputcollid;
    strip(args[0]).as_var()?;
    let mut pattern = text(args[1])?;
    let form = match flags {
        Some(f) => gravity_dict::regexp::flags_form(&f, &pattern)?,
        None => gravity_dict::regexp::FlagsForm { global: false, icase: false },
    };
    let anchored = pattern.first() == Some(&b'^') && !pattern.contains(&b'|');
    if form.icase {
        let own = pattern.starts_with(b"***") || (pattern.starts_with(b"(?") && pattern.get(2).is_some_and(u8::is_ascii_alphabetic));
        if own {
            return None;
        }
        let mut p = mcx::to_vec_in(b"(?i)", mcx);
        p.extend_from_slice_in(&pattern);
        pattern = p;
    }
    Some(Derived { pattern, replacement: text(args[2])?, coll, global: form.global && !anchored })
}

/// DK1: the key's rows pass a Filter or Gate of pipeline `k` first. A hosted call's table evaluates every entry of the
/// part at its build, the entries no surviving row reads too; the program table is filled by the rows that read it.
fn filtered(px: &Px<'_, '_>, k: usize, j: usize) -> bool {
    px.gx.pipelines().get(k).and_then(|p| p.ops.get(..j)).is_some_and(|ops| ops.iter().any(|o| matches!(o, Op::Filter(_) | Op::Gate(_))))
}

/// DK1: pipeline `k`'s predicted rows (its scan's estimate past its filters) are under half the per-code entries of
/// column `attno`, so most entries are read by no surviving row. The program's fill costs more per entry than the hosts'
/// build (dict-g3: lower(t) 939 against 755 ms), so a filter that keeps most rows keeps `DERIVED_CALL`, as does a column
/// with no entry count (a stale count only picks the slower of two forms with the same answers).
fn sparse(px: &Px<'_, '_>, k: usize, attno: i16) -> bool {
    let Some(p) = px.gx.pipelines().get(k) else { return false };
    let Some((_, relid, _)) = p.source.relation() else { return false };
    px.gx.facts.att_entries(relid, attno).is_some_and(|n| p.desc.predicted_rows.saturating_mul(2) < n)
}

/// HITS9-E1: the swept `PROGRAM` Fp128 table or the hosts' `DERIVED_CALL` table (`attach`'s, no `derived` form) of
/// pipeline `k`'s key over site lane `lane` (column `attno`), compiled before the end plans, when it is the only key form
/// over that column on the pipeline's tables; `None` otherwise (the hosts' `DERIVED_FINGERPRINT` table, a path key, two
/// forms, none compiled yet).
pub(crate) fn swept_table(px: &mut Px<'_, '_>, k: usize, lane: u8, attno: i16) -> Option<u16> {
    let seen: &Px<'_, '_> = px;
    let form = |e: usize| seen.placed(ExId(e as u32)).ext.as_deref().and_then(|x| x.downcast_ref::<KeyFp>()).filter(|f| f.col == attno && !f.stash).map(|f| (f.t, Rc::clone(&f.tabs), f.call));
    let forms: PgVec<'_, (u16, Rc<RefCell<Tables>>, bool)> = (0..seen.nexs()).filter_map(form).collect_in(seen.mcx());
    if forms.is_empty() {
        return None;
    }
    let tabs = crate::dict::percode::tables(px, k);
    let mut ts = forms.iter().filter(|(_, x, _)| Rc::ptr_eq(x, &tabs)).map(|(t, _, call)| (*t, *call));
    let (t, call) = ts.next()?;
    let swept = call || tabs.borrow().sweeps.iter().any(|s| s.t == t as usize && s.over == lane && s.attno == attno && s.elem == ELEM_FP128);
    let t = (swept && ts.all(|x| x.0 == t)).then_some(t)?;
    if let Some(s) = tabs.borrow_mut().sweeps.iter_mut().find(|s| s.t == t as usize) {
        s.walked = true;
    }
    crate::dict::percode::mark_swept(px, k, &tabs, t as usize); // D3115
    Some(t)
}

/// Key `e` as an immutable builtin of one column whose other arguments are non-null varlena constants (the hosts'
/// `DERIVED_CALL` table): the call (the constants' payloads baked: V4R-18) and its input collation. A by-value constant
/// is a program word, never a table's bytes (D407), so a call that takes one keeps the program table; so does a set or
/// VARIADIC call, a call that acts, one whose function reads its calling node (a polymorphic shim) or that a pool worker
/// may not make. The column is a text or varchar one: the hosts pass an entry as its varlena (a jsonb column, a shredded
/// path's lane and any other entry keep the program table). The table is the statement's, so a STABLE call's one answer per
/// entry holds for it.
pub(crate) fn call_of<'a>(facts: &dyn gravity_decompose::catalog::Facts, e: Node<'a>) -> Option<(gravity_program::table_fn::Call, u32)> {
    use gravity_decompose::effects::{builtin_func, of_call, Parallel};
    let e = e.as_target_entry().map_or(e, |te| te.expr);
    let (funcid, args, coll) = match (e.as_func_expr(), e.as_op_expr()) {
        (Some(f), _) if !f.funcretset && !f.funcvariadic => (f.funcid, &f.args, f.inputcollid),
        (_, Some(o)) if !o.opretset => (o.opfuncid, &o.args, o.inputcollid),
        _ => return None,
    };
    if of_call(facts, funcid).acts || builtin_func(funcid)?.pool(funcid) != Parallel::Safe || !gravity_fn::shim_of(funcid).is_some_and(|s| s.tyargs.is_empty()) {
        return None;
    }
    let strip = |a: Node<'a>| a.as_relabel_type().map_or(a, |r| r.arg);
    let mut col = None;
    for a in args.iter().map(strip) {
        match (a.as_var(), a.as_const()) {
            (Some(v), _) if v.varlevelsup == 0 && gravity_fn::typesem::text_payload(v.vartype) && col.replace((v.varno, v.varattno)).is_none() => {}
            (_, Some(c)) if gravity_decompose::expr::const_bytes_keyed(c).is_some() => {}
            _ => return None,
        }
    }
    col?;
    // read (baked) only once the call is taken
    let out = args.iter().map(strip).map(|a| a.as_const().and_then(gravity_decompose::expr::const_bytes).map(<[u8]>::to_vec)).collect(); // alloc: Program
    Some((gravity_program::table_fn::Call { funcid, args: out }, coll))
}

/// The form: the whole of `c` a function of one code column of the row, evaluated per entry (strict or not: a NULL code's
/// rows re-evaluate, so NULLIF / COALESCE / CASE bodies serve). Every `Var` of the program is that column, islands' too;
/// no `PARAM_EXEC` (the row's context, not a word the sweep reads); volatile calls never reach here (expr.rs refuses them).
/// An `OUTER` column that is a code column of the row (an end's DISTINCT argument over the projection's resno) reads as
/// the scan's (the same `row.col`), so the sweep's one-column row serves it.
fn attach(px: &mut Px<'_, '_>, k: usize, j: usize, c: &mut Compiled<'_>, (coll, der): (u32, Option<Hosted<'_>>), (ex, e): (ExId, Node<'_>), node: i32) -> Option<Rc<KeyFp>> {
    if !PerValue::Table.on() || !gravity_fn::typesem::text_payload(c.result_type) || !px.gx.facts.coll(coll).bytes_eq {
        return None;
    }
    let cols = crate::step::project::code_cols(px, k, j);
    let read = |c: &Compiled<'_>, a: i16| c.prog.nodes.iter().any(|n| matches!(n, ExprNode::Var(v) if v.attno == a && v.src != VarSource::Inner));
    let (&(attno, lane, _), path) = cols.iter().find(|x| read(c, x.0)).map(|x| (x, false)).or_else(|| crate::dict::face::path_key(node).and_then(|f| (f.path)(px, k, c, &cols)).map(|x| (x, true)))?;
    for n in c.prog.nodes.iter_mut() {
        if let ExprNode::Var(v) = n {
            if v.src == VarSource::Outer && v.attno == attno {
                v.src = VarSource::Scan;
            }
        }
    }
    let one = c.prog.nodes.iter().all(|n| match n {
        ExprNode::Var(v) => v.src == VarSource::Scan && v.attno == attno,
        ExprNode::Param(q) => q.kind == ParamKind::PARAM_EXTERN,
        _ => true,
    });
    if !one && !path {
        return None;
    }
    // a hosted table reads the lane's entries as the column's own values; a path lane's entries are the path's. DK1: a call
    // key whose surviving rows the estimate says read few of the part's entries is ADAPTIVE ((2'), DK1-MISEST): each source
    // publishes it unfilled for its rows to fill (the lazy sweep below) until a part's rows fill half its entries, then
    // builds it hosted, so a misestimated filter costs one part's fill, not the whole statement's
    if keeps_datum(px.gx, k, j, e) {
        return None;
    }
    let der = der.filter(|_| !path);
    let adaptive = matches!(der, Some(Hosted::Call(..))) && filtered(px, k, j) && sparse(px, k, attno);
    let site = px.site_mut(k)?;
    let decl = match &der {
        // the regexp's: built once per part per statement, shared by the statement's workers, never kept across statements;
        // a faulted entry is `[0, 0]`, no bit
        Some(Hosted::Regexp(d)) => d.decl(site, lane),
        // a call's: each source's own per part (its region chunks, as a program table), never kept across statements
        Some(Hosted::Call(call, inputcoll)) => {
            let args = [site.table_bytes_of(&call.encode()), (0, 0)];
            let flags = if adaptive { table_fn::FLAG_ADAPTIVE } else { 0 };
            TableDecl { over: lane, func: table_fn::DERIVED_CALL, flags, collation: *inputcoll, encoding: gravity_fn::typesem::collation::database_encoding(), args }
        }
        // the table's body identity: this expression's (a program table is the statement's own, never shared by content)
        None if path => TableDecl::fingerprint(lane),
        None => TableDecl { over: lane, func: table_fn::PROGRAM, flags: ELEM_FP128, collation: 0, encoding: 0, args: [site.table_bytes_of(&(u64::from(ex.0) << 32 | 1).to_le_bytes()), (0, 0)] },
    };
    let t = crate::dict::percode::declare_table(px, k, decl, Class::Fp)?;
    let tabs = crate::dict::percode::tables(px, k);
    let every = crate::dict::percode_fill::every_row(px, k, j); // STEP-04: a key every row of the part reads is swept
    let r = {
        let mut tb = tabs.borrow_mut();
        let r = tb.rows;
        tb.rows = r.checked_add(1 + path as u8).filter(|&r1| crate::dict::strategies::percode_guard::room(tb.lanes.len(), r1.into()))?; // D1747: the row slot is a u8 fp_table
        match tb.sweeps.iter_mut().find(|s| s.t == t as usize) {
            Some(s) => s.every |= every,
            None if (der.is_none() || adaptive) && !path => {
                if every {
                    fires::fire("percode.every");
                }
                tb.sweeps.push(Sweep { t: t as usize, over: lane, ex, root: c.root, attno, elem: ELEM_FP128, node, walked: false, every }); // alloc: Program
            }
            None => {}
        }
        r
    };
    crate::dict::percode::mark_swept(px, k, &tabs, t as usize); // D3115
    if adaptive {
        fires::fire("percode.adaptive"); // (2'): the source chooses the hosted build or the rows' fill per part
    }
    fires::fire(match der {
        Some(Hosted::Regexp(..)) => "dict.derived_fp",
        Some(Hosted::Call(..)) => "dict.derived_call",
        None => "dict.percode_key",
    }); // shapecheck fire signal (REGISTRY H01, H02, B2-09, STEP-01, B2-15, OID-33)
    let call = matches!(der, Some(Hosted::Call(..)));
    Some(Rc::new(KeyFp { t, r, col: attno, tabs, stash: path, call })) // alloc: Program
}

/// D3840: whether the group pipeline `k` ends in keeps a copy of the row's datum (emit_group.rs `rep_src`'s datum form,
/// which keyed_emit_key `keep_datums` renders off the row's cell) for the key projection `j` makes of `e`. The image-only
/// cell this form and percode_guard's make stand for the key's image alone, so such a key keeps its value. A set
/// operation's side keyed by text(c) over a SubqueryScan's char(n) column (a Var of the subquery's varno, which rep_src
/// does not walk to the scan's code lane) took the form and its copy read the image-only cell as a datum.
pub(crate) fn keeps_datum(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, e: Node<'_>) -> bool {
    let (Some(g), Some(Op::Project(l))) = (gx.group_at(k), gx.pipelines()[k].ops.get(j)) else { return false };
    let Some(te) = l.iter().filter_map(|n| n.as_target_entry()).find(|t| t.expr.ptr_eq(e)) else { return false };
    let key = |x: &&gravity_decompose::ops::shared::agg::KeyDecl<'_>| x.kind == KeyKind::Fp128 && crate::table::emit_group::key_col(gx, k, x) == Some(te.resno);
    g.keys.iter().filter(key).any(|x| crate::table::emit_group::rep_src(gx, g.producer.0 as usize, x, g.node_id).is_some_and(|r| r.datum))
}

/// The entry at the row's code of table `t`, `[lo, hi]`, in the block where it is set: a NULL code (`null`) branches to
/// `l_slow` before any load (its element need not be a code of the dictionary, which may have none), as does a `[0, 0]`.
pub(crate) fn lookup(b: &mut FunctionBuilder<'_>, zero: Value, tables: Value, t: u16, code_at: Value, null: Option<Value>, l_slow: Block) -> [Value; 2] {
    if let Some(n) = null {
        let l_look = b.create_block(&[]);
        b.cond_br(n, (l_slow, &[]), (l_look, &[]));
        b.switch_to(l_look);
    }
    let code = b.load_zext(Type::I32, code_at, 0);
    let data = b.load(Type::Ptr, tables, (BIND_OUT_WORDS * t as usize * 8) as i32);
    let at = b.ptr_add(data, code, 16, 0);
    let (lo, hi) = (b.load(Type::I64, at, 0), b.load(Type::I64, at, 8));
    let any = b.int_bin(IntBinOp::Or, lo, hi);
    let set = b.icmp(IntCond::Ne, any, zero);
    let l_hit = b.create_block(&[]);
    b.cond_br(set, (l_hit, &[]), (l_slow, &[]));
    b.switch_to(l_hit);
    [lo, hi]
}

/// The key cell of `id` over `row` when `id` has the form (`None` otherwise), with its value and NULL: the entry's Fp128 at
/// the row's code, or where the entry is `[0, 0]` or the row's code NULL, the key re-evaluated over the row and hashed
/// (NULL: zero words), in the row table the cell reads.
pub(crate) fn cell(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> Option<(Cell, Value, Value)> {
    let Some(kf) = env.placed(id).ext.as_deref().and_then(|x| x.downcast_ref::<KeyFp>()) else { return crate::dict::strategies::percode_guard::cell(b, env, node, row, id) };
    let (n, rows) = (kf.tabs.borrow().lanes.len(), kf.tabs.borrow().rows);
    let (Some(tables), Some(src)) = (env.tables, row.col(kf.col)) else { crate::refuse!(node, Reason::Bug { payload: "a derived key with no tables bound or no code column" }) };
    let (Rep::Code { dict }, At::Lane(_)) = (src.cell.rep, src.cell.at) else { crate::refuse!(node, Reason::Bug { payload: "a derived key whose column is not read by code" }) };
    let (slot, e) = row_at(n, rows, kf.r);
    let entry = b.ptr_add(tables, env.zero, 8, (e * 8) as i32);
    let (t, f) = (b.bconst(true), b.bconst(false));
    let (l_slow, l_val, l_null) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let l_join = b.create_block(&[Type::Bool]);
    b.mark_cold(l_slow);
    let [lo, hi] = lookup(b, env.zero, tables, kf.t, src.val, src.null, l_slow);
    b.store(entry, 0, lo);
    b.store(entry, 8, hi);
    b.br(l_join, &[f]);
    b.switch_to(l_slow);
    let memo = env.memo.set_aside();
    let (v, valid) = crate::step::expr::emit(b, env, node, row, id);
    env.memo.restore(memo);
    b.cond_br(valid, (l_val, &[]), (l_null, &[]));
    b.switch_to(l_val);
    let (v, h) = (as_ptr(b, v), text_fp(b));
    if kf.stash { b.store(entry, 24, v) } // row table r + 1's entry word: the path key's text at its record's open (keyed/emit_group_nocode.rs)
    let st = b.call_host(h, &[v, entry], Some(Type::I64)).unwrap_or(env.zero);
    let failed = b.icmp(IntCond::Ne, st, env.zero);
    let bit = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    let err = b.select(failed, bit, env.zero);
    crate::step::expr::raise_if(b, env, row, err);
    // DK1: a lazy table's entry is the row's hash, stored for the rows after it (a NULL code or result stores nothing)
    if kf.tabs.borrow().sweeps.iter().any(|s| s.t == kf.t as usize && crate::dict::percode_fill::lazy(s)) {
        fires::fire("percode.lazy");
        let (l_fill, l_done) = (b.create_block(&[]), b.create_block(&[]));
        match src.null {
            Some(n) => b.cond_br(n, (l_done, &[]), (l_fill, &[])),
            None => b.br(l_fill, &[]),
        }
        b.switch_to(l_fill);
        let code = b.load_zext(Type::I32, src.val, 0);
        let data = b.load(Type::Ptr, tables, (BIND_OUT_WORDS * kf.t as usize * 8) as i32);
        let at = b.ptr_add(data, code, 16, 0);
        let (lo, hi) = (b.load(Type::I64, entry, 0), b.load(Type::I64, entry, 8));
        b.store(at, 0, lo);
        b.store(at, 8, hi);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }
    b.br(l_join, &[f]);
    b.switch_to(l_null);
    b.store(entry, 0, env.zero);
    b.store(entry, 8, env.zero);
    b.br(l_join, &[t]);
    b.switch_to(l_join);
    let null = b.block_params(l_join)[0];
    let life = CellLife { until: Until::Row, home: CellHome::ImageOnly };
    let rep = Rep::Code { dict: DictRef { lane: dict.lane, fp_table: slot as u8 } };
    let cell = Cell { rep, typid: env.placed(id).c.result_type, num: NumTypmod::NONE, life, at: src.cell.at, null: Nullable::Maybe };
    Some((cell, b.ptr_add(entry, env.zero, 8, 16), null))
}

#[cfg(test)]
mod tests {
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::Type;

    /// Two entries whose results are one value (here one text in its two header forms) store one Fp128, the dictionary's
    /// for those bytes (rule A); another value stores another.
    #[test]
    fn two_entries_one_result_one_fp() {
        gravity_dict::fp::install(0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
        let short = [(4u8 << 1) | 1, b'x', b'.', b'c'];
        let long = [7u32 << 2].iter().flat_map(|w| w.to_le_bytes()).chain(*b"x.c").collect::<Vec<u8>>();
        let other = [(4u8 << 1) | 1, b'y', b'.', b'c'];
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr], &[]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        for (i, &d) in p[1..].iter().enumerate() {
            let code = b.iconst(Type::I64, i as i64);
            super::store(&mut b, p[0], code, d);
        }
        b.ret(&[]);
        let prog = pb.finish();
        let mut data = [0u64; 6];
        let args = [data.as_mut_ptr() as u64, short.as_ptr() as u64, long.as_ptr() as u64, other.as_ptr() as u64];
        Interp::new(&prog).run(f, &args).expect("runs");
        let want = gravity_dict::fp::keys().expect("seed").fp_bytes(b"x.c");
        assert_eq!([data[0], data[1]], want, "the first entry is the dictionary's Fp128 of its result");
        assert_eq!([data[2], data[3]], want, "an equal result of another entry hashes equal");
        assert_ne!([data[4], data[5]], want, "another result hashes apart");
    }

    /// `funcid(args..)` returning text under the default collation.
    fn call<'m>(m: mcx::Mcx<'m>, funcid: u32, args: &[types_nodes::Node<'m>]) -> types_nodes::Node<'m> {
        let mut l = types_nodes::NodeList::make1(m, args[0]).expect("a list");
        args[1..].iter().for_each(|&a| l.lappend(m, a).expect("an argument"));
        let f = types_nodes::primnodes::FuncExpr { funcid, funcresulttype: 25, inputcollid: 100, args: l, ..Default::default() };
        types_nodes::Node::mk(m, f).expect("a call")
    }

    /// regexp_replace over a column, with a constant pattern and replacement, is the hosts' derived fingerprint in its
    /// 3-argument form (oid 2284) and in a flags form (oid 2285) whose flags PostgreSQL reads as the advanced RE's:
    /// `'i'` as the pattern led by `(?i)`, `'g'` as the table's every-match flag (dropped over an anchored pattern, whose
    /// one match is the first), the last of `'i'` / `'c'` winning, `'s'` / `'t'` no change, a newline-sensitive or
    /// expanded flag only where the pattern has nothing it acts on. An invalid letter, another flavour (`'b'` `'e'`
    /// `'q'`), a flag that can change the match, `'i'` over a pattern with its own options, a replacement that is not a
    /// constant, a NULL pattern and a call over no column keep the program table (or the call's).
    #[test]
    fn regexp_replace_flags_forms_are_derived_fingerprints() {
        use types_nodes::Node;
        let cx = mcx::MemoryContext::new("derived");
        let m = cx.mcx();
        let image = |s: &str| ((s.len() as u32 + 4) << 2).to_le_bytes().iter().chain(s.as_bytes()).copied().collect::<Vec<u8>>();
        let strs = ["^https?://(?:m\\.)?([^/]+)/.*$", "\\1", "a(b)", "(?n)a", "[0-9]+", "a b", "a#b", "\\d+"];
        let images: Vec<Vec<u8>> = strs.iter().map(|s| image(s)).collect();
        const FLAGS: [&str; 24] = ["", "g", "i", "gi", "ig", "gc", "gic", "gci", "gs", "gt", "gn", "gm", "gp", "gw", "gx", "x", "n", "b", "e", "q", "gq", "z", "1g", "G"];
        let flag_images: Vec<Vec<u8>> = FLAGS.iter().map(|s| image(s)).collect();
        let text = |b: &Vec<u8>| Node::mk_const(m, 25, -1, 100, -1, datum::Datum::from_u64(b.as_ptr() as u64), false, false).expect("a text");
        let flag = |f: &str| text(&flag_images[FLAGS.iter().position(|x| *x == f).expect("a flags image")]);
        let null = Node::mk_const(m, 25, -1, 100, -1, datum::Datum::from_u64(0), true, false).expect("a NULL");
        let col = Node::mk_var(m, 1, 3, 25, -1, 100, 0).expect("a column");
        let form = |n: Node<'_>| super::derived(n, m).map(|d| (d.pattern.to_vec(), d.replacement.to_vec(), d.coll, d.global));
        let want = |p: usize, icase: bool, global: bool| Some(([if icase { &b"(?i)"[..] } else { b"" }, &images[p][4..]].concat(), images[1][4..].to_vec(), 100, global));
        let rr = |p: usize, f: &str| form(call(m, 2285, &[col, text(&images[p]), text(&images[1]), flag(f)]));
        assert_eq!(form(call(m, 2284, &[col, text(&images[0]), text(&images[1])])), want(0, false, false), "the 3-argument form");
        assert_eq!(rr(0, ""), want(0, false, false), "flags '' (normalize regexp3)");
        assert_eq!(rr(0, "g"), want(0, false, false), "flags 'g' on an anchored pattern: the first match");
        assert_eq!(rr(2, "g"), want(2, false, true), "flags 'g' on an unanchored pattern: every match");
        assert_eq!(rr(0, "i"), want(0, true, false), "flags 'i': the pattern led by (?i)");
        assert_eq!(rr(2, "i"), want(2, true, false), "flags 'i' on an unanchored pattern");
        assert_eq!(rr(0, "gi"), want(0, true, false), "flags 'gi' on an anchored pattern");
        assert_eq!(rr(2, "gi"), want(2, true, true), "'gi' on an unanchored pattern");
        assert_eq!(rr(2, "ig"), want(2, true, true), "'ig'");
        assert_eq!(rr(2, "gc"), want(2, false, true), "'c' is the default");
        assert_eq!(rr(2, "gic"), want(2, false, true), "the last of 'i' / 'c' wins: 'c'");
        assert_eq!(rr(2, "gci"), want(2, true, true), "the last of 'i' / 'c' wins: 'i'");
        assert_eq!(rr(2, "gs"), want(2, false, true), "'s' is the default");
        assert_eq!(rr(2, "gt"), want(2, false, true), "'t' is the default");
        for f in ["gn", "gm", "gp", "gw"] {
            assert_eq!(rr(4, f), want(4, false, true), "{f:?} over a pattern with no . ^ $ or complemented class");
            assert_eq!(rr(0, f), None, "{f:?} over a pattern with . ^ $ or a negated bracket");
        }
        assert_eq!(rr(0, "n"), None, "'n' over an anchored pattern: ^ holds after each newline");
        assert_eq!(rr(2, "n"), want(2, false, false), "'n' over a pattern it does not act on");
        assert_eq!(rr(4, "gx"), want(4, false, true), "'x' over an ASCII pattern with no white space and no #");
        assert_eq!(rr(5, "gx"), None, "'x' over a pattern with white space");
        assert_eq!(rr(6, "x"), None, "'x' over a pattern with #");
        assert_eq!(rr(7, "gn"), want(7, false, true), "an escaped class that is not a complement");
        for f in ["b", "e", "q", "gq"] {
            assert_eq!(rr(4, f), None, "{f:?}: another flavour keeps the call");
        }
        for f in ["z", "1g", "G"] {
            assert_eq!(rr(4, f), None, "{f:?}: an invalid flag keeps the call, which raises PostgreSQL's error");
        }
        assert_eq!(rr(3, "i"), None, "'i' over a pattern with its own options");
        assert_eq!(form(call(m, 2284, &[col, text(&images[0]), col])), None, "a replacement that is not a constant");
        assert_eq!(form(call(m, 2284, &[col, null, text(&images[1])])), None, "a NULL pattern");
        assert_eq!(form(call(m, 2284, &[text(&images[2]), text(&images[0]), text(&images[1])])), None, "over no column");
    }

    /// An immutable builtin of one column whose other arguments are varlena constants is the hosts' call table, the
    /// constants' payloads in order and the column as the entry; a by-value constant (a word), a NULL constant, a second
    /// column or none keep the program table.
    #[test]
    fn an_immutable_call_of_one_column_with_varlena_constants_is_a_hosted_call() {
        use gravity_program::table_fn::Call;
        use types_nodes::Node;
        let cx = mcx::MemoryContext::new("call");
        let m = cx.mcx();
        let image = |s: &str| ((s.len() as u32 + 4) << 2).to_le_bytes().iter().chain(s.as_bytes()).copied().collect::<Vec<u8>>();
        let images = [image("http://"), image(""), image("[0-9]+"), image("i"), image("k")];
        let text = |i: usize| Node::mk_const(m, 25, -1, 100, -1, datum::Datum::from_u64(images[i].as_ptr() as u64), false, false).expect("a text");
        let int = |v: u64| Node::mk_const(m, 23, -1, 0, 4, datum::Datum::from_u64(v), false, true).expect("an int");
        let null = Node::mk_const(m, 25, -1, 100, -1, datum::Datum::from_u64(0), true, false).expect("a NULL");
        let (col, col2) = (Node::mk_var(m, 1, 3, 25, -1, 100, 0).expect("a column"), Node::mk_var(m, 1, 4, 25, -1, 100, 0).expect("a column"));
        let lit = |i: usize| Some(images[i][4..].to_vec());
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 870, &[col])), Some((Call { funcid: 870, args: vec![None] }, 100)), "lower(column)");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 2087, &[col, text(0), text(1)])), Some((Call { funcid: 2087, args: vec![None, lit(0), lit(1)] }, 100)), "replace(column, 'http://', '')");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 2285, &[col, text(2), text(1), text(3)])), Some((Call { funcid: 2285, args: vec![None, lit(2), lit(1), lit(3)] }, 100)), "regexp_replace with flags 'i'");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 877, &[col, int(1), int(5)])), None, "substr(column, 1, 5): by-value literals stay words");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 2087, &[col, null, text(1)])), None, "a NULL constant");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 2087, &[col, col2, text(1)])), None, "two columns");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 2087, &[text(0), text(0), text(1)])), None, "no column");
        let j = Node::mk_var(m, 1, 6, 3802, -1, 0, 0).expect("a jsonb column");
        assert_eq!(super::call_of(&gravity_decompose::catalog::NoFacts, call(m, 3214, &[j, text(4)])), None, "j ->> 'k': a jsonb column's entry is no text");
    }

    /// The row's entry `(hit, lo, hi)` through `lookup` over table words `tables` (table 0), the code at `code`, `is_null`.
    fn look(tables: &[u64; 3], code: &u32, is_null: bool) -> Vec<u64> {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64, Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let (tp, cp, np) = (b.params()[0], b.params()[1], b.params()[2]);
        let (zero, one) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1));
        let null = b.icmp(gravity_ir::IntCond::Ne, np, zero);
        let l_slow = b.create_block(&[]);
        let [lo, hi] = super::lookup(&mut b, zero, tp, 0, cp, Some(null), l_slow);
        b.ret(&[one, lo, hi]);
        b.switch_to(l_slow);
        b.ret(&[zero, zero, zero]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[tables.as_ptr() as u64, code as *const u32 as u64, is_null as u64]).expect("runs")
    }

    /// A NULL code never reads the table: an empty dictionary's table (ncodes 0: `empty_table`'s 8 data bytes) under a NULL
    /// row whose element holds all ones takes the re-evaluation, where a load would read 64 GiB past the data; a set entry
    /// is the hit, a `[0, 0]` one the re-evaluation.
    #[test]
    fn a_null_code_reads_no_entry() {
        let empty = [0u64; 1];
        assert_eq!(look(&[empty.as_ptr() as u64, 0, 0], &u32::MAX, true), vec![0, 0, 0], "a NULL row over an empty dictionary");
        let one = [5u64, 6, 0, 0];
        let words = [one.as_ptr() as u64, 0, 0];
        assert_eq!(look(&words, &0, false), vec![1, 5, 6], "a set entry");
        assert_eq!(look(&words, &1, false), vec![0, 0, 0], "a [0, 0] entry re-evaluates");
        assert_eq!(look(&words, &0, true), vec![0, 0, 0], "a NULL row re-evaluates although its element's entry is set");
    }
}
