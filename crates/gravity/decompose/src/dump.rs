use core::fmt::{self, Write};

use gravity_abi::breaker::{SlotKind, ORDER_KEY, ORDER_NONE};
use gravity_abi::ctx::lane;
use gravity_program::{End, Parallel, SiteDesc, WordsDesc, ZoneOp};

use crate::ops::shared::agg::{GroupDecl, NONE};
use crate::graph::{Bound, Layout, Op, PipelineGraph, PipelineNode, Source};
use crate::admit::Refusal;
use crate::{expr, ledger};

pub fn dump(g: &PipelineGraph<'_>, out: &mut dyn Write) -> fmt::Result {
    writeln!(out, "graph pipelines={} breakers={} init_plans={} root=p{}", g.pipelines.len(), g.breakers.len(), g.init_plans.len(), g.root)?;
    for (i, p) in g.pipelines.iter().enumerate() {
        pipeline(i, p, out)?;
        outs(g, p, out)?;
    }
    for b in g.breakers.iter() {
        write!(out, "breaker slot={} kind={:?} budget={:?} spill={:?} producer=p{} layout=", b.breaker.slot, b.breaker.kind, b.breaker.budget, b.breaker.spill, b.producer)?;
        layout(&b.layout, out)?;
        if b.order.is_known() {
            write!(out, " order=")?;
            b.order.dump(out)?;
        }
        writeln!(out)?;
    }
    for gd in g.groups.iter() {
        group(gd, out)?;
    }
    for (i, gd) in g.streams.iter().enumerate() {
        write!(out, "stream {i}: ")?;
        group(gd, out)?;
    }
    for ip in g.init_plans.iter() {
        write!(out, "init_plan id={} pipeline=p{} link={:?}", ip.plan_id, ip.pipeline, ip.link)?;
        if ip.param >= 0 {
            write!(out, " param=$exec{}", ip.param)?;
        }
        if let Some(s) = ip.slot {
            write!(out, " slot={s}")?;
        }
        // FACTS-1a: every exec parameter the first row binds, when there is more than `param`
        if ip.params.len() > 1 {
            write!(out, " sets=(")?;
            for (i, p) in ip.params.iter().enumerate() {
                write!(out, "{}$exec{p}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
        }
        crate::rerun::dump_f5(out, ip.volatility, ip.raise_free)?;
        writeln!(out)?;
    }
    for r in g.reruns.iter() {
        write!(out, "rerun node={}", r.node_id)?;
        if r.plan_id != 0 {
            write!(out, " subplan={}", r.plan_id)?;
        }
        write!(out, " pipelines=")?;
        for (i, p) in r.pipelines.iter().enumerate() {
            write!(out, "{}p{p}", if i > 0 { "," } else { "" })?;
        }
        write!(out, " slots=")?;
        for (i, s) in (0..=u8::MAX).filter(|s| crate::graph::slot_has(&r.slots, *s)).enumerate() {
            write!(out, "{}{s}", if i > 0 { "," } else { "" })?;
        }
        // item 9 (b): the members PG keeps across rescans (run at the set's first run only)
        if r.pipelines.iter().any(|q| g.kept.contains(q)) {
            write!(out, " kept")?;
        }
        writeln!(out)?;
    }
    for (id, p) in g.params.iter() {
        param(*id, p, out)?;
    }
    for sl in g.scan_lanes.iter() {
        sl.dump(out)?;
    }
    write!(out, "mapped:")?;
    for k in g.mapped_kinds() {
        write!(out, " {}", k.name())?;
    }
    writeln!(out)
}

pub fn dump_refusal(r: &Refusal, out: &mut dyn Write) -> fmt::Result {
    writeln!(out, "refused: {} node={} state={} reason={}", r.kind.name(), r.node_id, state(r), r.reason)
}

fn state(r: &Refusal) -> &'static str {
    match ledger::state_of(r.kind) {
        ledger::State::Mapped(_) => "shape",
        ledger::State::Refused(m, _) => m.name(),
        ledger::State::Excluded(..) => "excluded",
    }
}

fn pipeline(i: usize, p: &PipelineNode<'_>, out: &mut dyn Write) -> fmt::Result {
    write!(out, "p{i} ")?;
    body(p, out)
}

/// A pipeline embedded in another's op (a nested loop's inner): the same lines, no index.
pub fn nested(p: &PipelineNode<'_>, out: &mut dyn Write) -> fmt::Result {
    body(p, out)
}

fn body(p: &PipelineNode<'_>, out: &mut dyn Write) -> fmt::Result {
    write!(out, "node={} ", p.node_id)?;
    match &p.desc.parallel {
        Parallel::Eligible => write!(out, "eligible rows={}", p.desc.predicted_rows)?,
        Parallel::Ineligible { site, reason } => write!(out, "serial site={site} reason={reason:?}")?,
    }
    match p.desc.ends_in {
        End::Sink => write!(out, " end=sink")?,
        End::Breaker(b) => write!(out, " end=breaker(slot={} {:?})", b.slot, b.kind)?,
    }
    writeln!(out)?;
    if p.desc.nconsumed() > 0 || p.desc.finish || p.desc.words.is_some() || p.desc.owner.is_some() || p.desc.admit.is_some() {
        write!(out, "  desc")?;
        for (j, c) in p.desc.consumes.iter().enumerate() {
            write!(out, " consumes=slot{c}")?;
            if let Some(l) = p.desc.consumes_from.get(j) {
                write!(out, "@p{}", l.0)?;
            }
        }
        if p.desc.finish {
            write!(out, " finishing")?;
        }
        if let Some(o) = &p.desc.owner {
            let order = match o.order_slot {
                ORDER_NONE => "none",
                ORDER_KEY => "key",
                _ => "slot",
            };
            write!(out, " owner={order}/{}{}", o.topk, if o.ctor.is_some() { "/named" } else { "" })?;
        }
        if let Some(w) = &p.desc.words {
            write!(out, " words=")?;
            let mut first = true;
            for (name, slot) in [("topk", w.topk), ("dense_lo", w.dense_lo), ("dense_hi", w.dense_hi), ("bits", w.bits), ("order", w.order)] {
                if slot != WordsDesc::NONE {
                    write!(out, "{}{name}@{slot}", if first { "" } else { "," })?;
                    first = false;
                }
            }
            write!(out, "/{}", w.nparams)?;
        }
        if p.desc.admit.is_some() {
            write!(out, " admitting")?;
        }
        writeln!(out)?;
    }
    write!(out, "  nodes")?;
    for n in p.node_ops.iter() {
        write!(out, " n{}@{}{}", n.node_id, n.first_op, if n.producer_top { "^" } else { "" })?;
    }
    writeln!(out)?;
    match &p.source {
        Source::Range { site, am, scanrelid, relid } => {
            write!(out, "  source range rel={relid} rti={scanrelid} am={}", am.name())?;
            for (i, a) in site.merge.iter().enumerate() {
                write!(out, "{}{a}", if i == 0 { " merge=" } else { "," })?;
            }
            writeln!(out)?;
            site_lines(site, out)?;
        }
        Source::Index { site, am, scanrelid, relid, indexid, keys, only, backward, recheck, iam } => {
            write!(out, "  source index{} rel={relid} rti={scanrelid} am={} index={indexid}{}", if *only { "-only" } else { "" }, am.name(), if *backward { " backward" } else { "" })?;
            if *iam != crate::catalog::IndexAm::Btree {
                write!(out, " iam={}", iam.name())?;
            }
            writeln!(out)?;
            for k in keys.iter() {
                write!(out, "    key ")?;
                k.dump(out)?;
                writeln!(out)?;
            }
            if !recheck.is_nil() {
                write!(out, "    recheck ")?;
                expr::args(out, recheck)?;
                writeln!(out)?;
            }
            site_lines(site, out)?;
        }
        Source::Const => writeln!(out, "  source const")?,
        Source::Values { scanrelid, lists } => {
            writeln!(out, "  source values rti={scanrelid} rows={}", lists.len())?;
            for l in lists.iter() {
                write!(out, "    row ")?;
                match l.as_list() {
                    Some(l) => expr::args(out, l)?,
                    None => expr::print(l, out)?,
                }
                writeln!(out)?;
            }
        }
        Source::Breaker { slot } => writeln!(out, "  source breaker slot={slot}")?,
        Source::Named { scanrelid, relid, name } => writeln!(out, "  source named rti={scanrelid} rel={relid} name={name}")?,
        Source::Bitmap { site, am, scanrelid, relid, plan, recheck, morsel } => {
            writeln!(out, "  source bitmap rel={relid} rti={scanrelid} am={}{}", am.name(), if *morsel { " morsel" } else { "" })?;
            plan.dump(0, out)?;
            if !recheck.is_nil() {
                write!(out, "    recheck ")?;
                expr::args(out, recheck)?;
                writeln!(out)?;
            }
            site_lines(site, out)?;
        }
        Source::Tid { site, am, scanrelid, relid, form, quals } => {
            writeln!(out, "  source tid rel={relid} rti={scanrelid} am={} form={}", am.name(), form.name())?;
            for q in quals.iter() {
                write!(out, "    tid ")?;
                expr::print(q, out)?;
                writeln!(out)?;
            }
            site_lines(site, out)?;
        }
        Source::Sample { site, am, scanrelid, relid, sample } => {
            write!(out, "  source sample rel={relid} rti={scanrelid} am={} method={} handler={} args=", am.name(), crate::ops::scan::closure::sample_name(sample.method), sample.handler)?;
            expr::args(out, sample.args)?;
            if let Some(r) = sample.repeatable {
                write!(out, " repeatable=")?;
                expr::print(r, out)?;
            }
            if sample.per_open != 0 {
                write!(out, " per_open={:#04b}", sample.per_open)?;
            }
            writeln!(out)?;
            site_lines(site, out)?;
        }
        Source::TableFunc { scanrelid, func, cols, ordinality } => {
            write!(out, "  source tablefunc rti={scanrelid} kind={} cols={cols} ordinality={ordinality} doc=", crate::ops::scan::closure::table_func_name(func.functype))?;
            expr::print(func.docexpr.unwrap(), out)?;
            if let Some(r) = func.rowexpr {
                write!(out, " row=")?;
                expr::print(r, out)?;
            }
            writeln!(out)?;
        }
    }
    for op in p.ops.iter() {
        match op {
            Op::Filter(n) => {
                write!(out, "  filter ")?;
                expr::print(*n, out)?;
            }
            Op::Gate(n) => {
                write!(out, "  gate ")?;
                expr::print(*n, out)?;
            }
            Op::Project(l) => {
                write!(out, "  project ")?;
                expr::args(out, l)?;
            }
            Op::Limit { count, offset, ties } => {
                write!(out, "  limit count=")?;
                bound(*count, out)?;
                write!(out, " offset=")?;
                bound(*offset, out)?;
                if let Some(t) = ties {
                    write!(out, " with-ties keys=")?;
                    for (i, key) in t.keys.iter().enumerate() {
                        write!(out, "{}{}:{}", if i > 0 { "," } else { "" }, key.resno, key.opno)?;
                    }
                    write!(out, " over={}", crate::ops::TIE_SOURCES[t.over as usize].name)?;
                }
            }
            Op::FirstOfRun(t) => {
                write!(out, "  first-of-run keys=")?;
                for (i, key) in t.keys.iter().enumerate() {
                    write!(out, "{}{}:{}:{:?}:{}", if i > 0 { "," } else { "" }, key.resno, key.opno, key.kind, key.typid)?;
                }
                write!(out, " over={}", crate::ops::TIE_SOURCES[t.over as usize].name)?;
            }
            Op::GroupRun(i) => write!(out, "  group-run stream={i}")?,
            Op::Sort(d) => d.dump(out)?,
            Op::Join(d) => {
                d.dump(out)?;
                if matches!(d, crate::join::JoinDesc::Nest { .. }) {
                    continue;
                }
            }
            Op::Window(d) => d.dump(out)?,
            Op::SetOp(d) => d.dump(out)?,
            Op::SubPlan(d) => {
                d.dump(out)?;
                if matches!(d.form, crate::ops::shared::subplan::SubForm::Nested { .. }) {
                    continue;
                }
            }
            Op::Append(d) => d.dump(out)?,
            Op::Write(d) => d.dump(out)?,
            Op::Lock(d) => d.dump(out)?,
            Op::Srf(d) => d.dump(out)?,
        }
        writeln!(out)?;
    }
    Ok(())
}

fn bound(b: Bound<'_>, out: &mut dyn Write) -> fmt::Result {
    match b {
        Bound::All => write!(out, "all"),
        Bound::Const(n) => write!(out, "{n}"),
        Bound::Expr(n) => expr::print(n, out),
    }
}

fn site_lines(s: &SiteDesc, out: &mut dyn Write) -> fmt::Result {
    write!(out, "    lanes")?;
    for (i, l) in s.lanes().iter().enumerate() {
        let kind = match l.kind {
            lane::KIND_BYVAL => "byval",
            lane::KIND_BYREF => "byref",
            lane::KIND_CODES => "codes",
            lane::KIND_FIXED => "fixed",
            _ => "?",
        };
        let mut flags = "";
        if s.filter_lanes.contains(i) {
            flags = "!";
        }
        write!(out, " {flags}{:?}:{kind}{}:", l.id, l.width)?;
        l.rep.short(out)?;
        write!(out, ":")?;
        l.life.short(out)?;
    }
    writeln!(out)?;
    for z in s.zone_preds() {
        let op = match z.op {
            ZoneOp::Lt => "<",
            ZoneOp::Le => "<=",
            ZoneOp::Eq => "=",
            ZoneOp::Ge => ">=",
            ZoneOp::Gt => ">",
            ZoneOp::Ne => "<>",
            ZoneOp::NotNull => "notnull",
            ZoneOp::IsNull => "isnull",
            ZoneOp::InMember => "inmember",
            ZoneOp::HashEq => "hasheq",
            ZoneOp::HashIn => "hashin",
        };
        match z.bound_of() {
            // NEW-72: a bound constant prints its word (the value is the execution's)
            Some((w, len)) => writeln!(out, "    zone lane{} {op} word{w}/{len}{}", z.lane, if z.tallied { " tallied" } else { "" })?,
            None => writeln!(out, "    zone lane{} {op} {}{}", z.lane, z.value, if z.tallied { " tallied" } else { "" })?,
        }
    }
    Ok(())
}

fn layout(l: &Layout<'_>, out: &mut dyn Write) -> fmt::Result {
    write!(out, "[")?;
    for (i, k) in l.keys.iter().enumerate() {
        write!(out, "{}{k:?}", if i > 0 { "," } else { "" })?;
    }
    write!(out, "]->[")?;
    let mut off = 8 * l.keys.len();
    for (j, s) in l.slots.iter().enumerate() {
        write!(out, "{}{s:?}@{off}", if j > 0 { "," } else { "" })?;
        off += s.bytes() as usize;
    }
    if l.implicit_sentinel() {
        write!(out, "{}Count@{off}", if l.slots.is_empty() { "" } else { "," })?;
    }
    write!(out, "]{} {}B", if l.has_validity() { "+valid" } else { "" }, l.size())
}

fn group(gd: &GroupDecl<'_>, out: &mut dyn Write) -> fmt::Result {
    write!(out, "group slot={} producer=p{} node={} form={:?} hints=[", gd.slot, gd.producer.0, gd.node_id, gd.form)?;
    for (i, h) in gd.hints.iter().flatten().enumerate() {
        write!(out, "{}{h:?}", if i > 0 { ", " } else { "" })?;
    }
    write!(out, "] layout=")?;
    layout(&gd.layout, out)?;
    if let Some(set) = gd.set {
        write!(out, " set={}/{}", set.arm, set.arms)?;
    }
    writeln!(out)?;
    for (j, key) in gd.keys.iter().enumerate() {
        write!(out, "  key {j} word={} resno={} kind={:?} attno={} type={}", key.word, key.resno, key.kind, key.attno, key.tf)?;
        if key.scale != crate::ops::shared::sort::NO_SCALE {
            write!(out, " scale={}", key.scale)?;
        }
        if key.precision != 0 {
            write!(out, " precision={}", key.precision)?;
        }
        if key.rep != crate::ops::shared::agg::NONE {
            write!(out, " rep={}", key.rep)?;
        }
        if !key.image.dump_plain() {
            write!(out, " image={:?}", key.image)?;
        }
        if key.image.is_collated() {
            write!(out, " coll={}", key.coll)?;
        }
        if key.desc {
            write!(out, " desc")?;
        }
        if key.nulls_first {
            write!(out, " nulls_first")?;
        }
        write!(out, " expr=")?;
        expr::print(key.expr.node(), out)?;
        writeln!(out)?;
    }
    for a in gd.aggs.iter() {
        write!(out, "  agg aggno={} fn={} reg={:?} slot=", a.aggno, a.fnoid, a.reg)?;
        slot(a.slot, out)?;
        write!(out, " count_slot=")?;
        slot(a.count_slot, out)?;
        write!(out, " final={:?}", a.final_)?;
        if a.scale != crate::ops::shared::sort::NO_SCALE {
            write!(out, " scale={}", a.scale)?;
        }
        if a.dscale_slot != crate::ops::shared::agg::NONE {
            write!(out, " dscale_slot={} dscales=", a.dscale_slot)?;
            let arms = a.dscales.iter().rposition(|&d| d != crate::ops::shared::agg::NULL_ARM).map_or(0, |i| i + 1);
            for (i, d) in a.dscales[..arms].iter().enumerate() {
                let sep = if i == 0 { "" } else { "," };
                if *d == crate::ops::shared::agg::NULL_ARM { write!(out, "{sep}null")? } else { write!(out, "{sep}{d}")? }
            }
        }
        write!(out, " class={:?} trans={:?}", a.class, a.trans)?;
        mods(a.mods, out)?;
        write!(out, " expr=")?;
        expr::print(a.node, out)?;
        if let Some(f) = a.filter {
            write!(out, " filter=")?;
            expr::print(f, out)?;
        }
        writeln!(out)?;
    }
    if gd.empty_no_row {
        write!(out, "  empty_no_row pinned=(")?;
        for (i, c) in gd.pinned.iter().enumerate() {
            write!(out, "{}outer.{c}", if i == 0 { "" } else { ", " })?;
        }
        writeln!(out, ")")?;
    }
    if let Some(t) = &gd.two_pass {
        writeln!(out, "  two_pass pass1=p{} slot={} key={} floor={}{}", t.pass1, t.slot, t.key, if t.floor_desc { "desc" } else { "asc" }, if t.floor_nulls_first { " nulls_first" } else { "" })?;
    }
    Ok(())
}

fn mods(m: crate::ops::shared::agg_class::Mods, out: &mut dyn Write) -> fmt::Result {
    write!(out, " mods=[")?;
    let mut sep = "";
    let mut item = |out: &mut dyn Write, s: &str| -> fmt::Result {
        write!(out, "{sep}{s}")?;
        sep = " ";
        Ok(())
    };
    if m.distinct > 0 {
        item(out, "distinct=")?;
        write!(out, "{}", m.distinct)?;
    }
    if m.order > 0 {
        item(out, "order=")?;
        write!(out, "{}", m.order)?;
    }
    if m.filter {
        item(out, "filter")?;
    }
    if m.direct > 0 {
        item(out, "direct=")?;
        write!(out, "{}", m.direct)?;
    }
    if m.split != crate::ops::shared::agg_class::Split::Simple {
        item(out, "split=")?;
        write!(out, "{:?}", m.split)?;
    }
    if m.star {
        item(out, "star")?;
    }
    write!(out, "]")
}

fn slot(s: u16, out: &mut dyn Write) -> fmt::Result {
    if s == NONE {
        write!(out, "-")
    } else {
        write!(out, "{s}")
    }
}

/// FACTS-1b F6: `param $exec<id> owner=n<node>|subplan<id> type=<oid>[/<typmod>][@<coll>] rep=<rep>:<life>` (`?` where no fact
/// was derived).
fn param(id: i32, p: &crate::graph::ParamRep, out: &mut dyn Write) -> fmt::Result {
    write!(out, "param $exec{id} owner=")?;
    match p.owner {
        crate::graph::ParamOwner::Nest { node } => write!(out, "n{node}")?,
        crate::graph::ParamOwner::SubPlan { plan_id } => write!(out, "subplan{plan_id}")?,
    }
    write!(out, " type={}", p.typid)?;
    if p.typmod >= 0 {
        write!(out, "/{}", p.typmod)?;
    }
    if p.coll != 0 {
        write!(out, "@{}", p.coll)?;
    }
    write!(out, " rep=")?;
    match p.out {
        Some(o) => {
            o.rep.short(out)?;
            write!(out, ":")?;
            o.life.short(out)?;
        }
        None => write!(out, "?")?,
    }
    writeln!(out)
}

/// Each projection's entries' outputs (`outs::annotate`): `<rep>:<life>` per entry, `?` where no fact was derived.
fn outs(g: &PipelineGraph<'_>, p: &PipelineNode<'_>, out: &mut dyn Write) -> fmt::Result {
    for op in p.ops.iter() {
        let Op::Project(l) = op else { continue };
        write!(out, "  outs")?;
        for (j, e) in l.iter().enumerate() {
            write!(out, " {}=", j + 1)?;
            match g.out_of(e) {
                Some(o) => {
                    o.rep.short(out)?;
                    write!(out, ":")?;
                    o.life.short(out)?;
                }
                None => write!(out, "?")?,
            }
        }
        writeln!(out)?;
    }
    Ok(())
}
