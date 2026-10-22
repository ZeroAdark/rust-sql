//! join.drain's spooled form (W2-DRAIN-A1-MRULE row A1b; LTC-NEST-DRAIN r5 Mechanism A, design delta r2): where join/drain.rs
//! would refuse a drain by the `m` rule and join/drain_spool.rs names no reason (`Gx::spooled_drain`), the probe pipeline
//! `k` ends in a spool of its joined rows (`DrainFill`: the OUTER_VAR and INNER_VAR columns the drain's ops read up to the
//! join's projection, nest/spool_rec.rs's record) and the drain pipeline `d` reads them, then the drained records, through
//! its own ops and end made once (`DrainReader`). PostgreSQL's Hash Right / Full Join hands its joined rows on while it
//! probes and the unmatched build rows after (HJ_FILL_INNER); under condition (a) nothing before the drain shows that it
//! ran, so the spooled order is the rows PostgreSQL hands on, joined first. The reader's window: the records from its
//! position to the count; at the count, the drain (join/drain_src.rs's source, a site-less child: nest/spool_child.rs)
//! read on to its next wanted record, which is put at the count (the count not bumped: one drained row at a time) and is
//! the window `[count, count + 1)`; a stop inside that row re-enters it (`put`). Both pipelines serial (one filler, one
//! region of drained records). Where the drain's consumer may stop, the joined phase streams (nest/spool_stream.rs). A
//! helper file (no rows). Owner M7-DRAIN.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_decompose::Op;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_kernels::util::RESUME_OPEN;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{LifeId, Parallel, Pipeline, Reason as Why};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::rerun::spool::child::fill;
use crate::rerun::spool::rec::Rec;
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::iface::{breaker_of, kind_impl, limit_bounds};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// join/drain_src.rs's own refusal when the columns cannot be bounded (the same Shape).
const OUTER_COLS: &str = "a drain whose outer columns the walker cannot bound (no projection, a whole-row or system column)";

/// writesubplan (merge#368 group): the drained arms' spool is read unstreamed (by a graph pipeline or, D3915, a nest's
/// embedded inner).
const ARMS_STREAM: &str = "a spooled drain over a streamed probe union's arms whose reader streams";

/// A spooled column: INNER_VAR (`true`) or OUTER_VAR, and its attno.
type Col = (bool, i16);

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

impl<'g> Gx<'g, '_> {
    /// The columns drain `d`'s ops after its Drain op `jd` read up to the join's projection (OUTER_VAR and INNER_VAR, each
    /// once, with its type), or `None` past an op or a column the walk cannot see through (drain_src.rs's `outer_vars`,
    /// SubPlan ops' arguments and tests read too).
    pub(crate) fn spool_columns(&self, d: usize, jd: usize) -> Option<PgVec<'g, (Col, u32)>> {
        fn uses(n: Node<'_>, out: &mut PgVec<'_, (Col, u32)>) -> bool {
            crate::stack_check();
            // an identity column's nextval (INSERT .. SELECT over a drain, kit K19.ivdu): its sequence only, no column
            if n.node_tag() == NodeTag::T_NextValueExpr {
                return true;
            }
            if let Some(v) = n.as_var() {
                if v.varlevelsup != 0 || v.varno != OUTER_VAR && v.varno != INNER_VAR {
                    return true;
                }
                let c = (v.varno == INNER_VAR, v.varattno);
                if v.varattno > 0 && !out.iter().any(|o| o.0 == c) {
                    out.push_in((c, v.vartype));
                }
                return v.varattno > 0;
            }
            let mut ok = true;
            gravity_decompose::expr::kids(n, &mut |k| ok &= uses(k, out)) && ok
        }
        let mut out = PgVec::new_in(self.mcx);
        let mut ok = true;
        for op in &self.pipelines()[d].ops[jd + 1..] {
            match op {
                Op::Filter(n) | Op::Gate(n) => ok &= uses(*n, &mut out),
                Op::SubPlan(s) => ok &= s.args.iter().fold(true, |ok, n| uses(n, &mut out) && ok) && s.test.is_none_or(|t| uses(t, &mut out)),
                _ if limit_bounds(op).is_some() => {}
                Op::Project(l) => return l.iter().fold(ok, |ok, n| uses(n, &mut out) && ok).then_some(out),
                _ => return None,
            }
        }
        None
    }
}

/// The columns' type facts (the spool's record).
fn types<'g>(gx: &Gx<'g, '_>, node: i32, cols: &[(Col, u32)]) -> PgVec<'g, TypeFacts> {
    cols.iter().map(|&(_, t)| facts_of(gx.facts, t).unwrap_or_else(|| shape(node, "a spooled column whose type the catalog does not know"))).collect_in(gx.mcx)
}

/// The probe's end: each joined row's spooled columns appended (its pipeline serial: the spool's one filler).
struct DrainFill<'g> {
    mcx: Mcx<'g>,
    rec: Rec<'g>,
    cols: PgVec<'g, Col>,
    /// The streamed form's first word (nest/spool_stream.rs).
    pull: Option<u16>,
    /// A4-MATEND (D2371): the unstreamed spool's first extra word, where this execution's records begin (`DrainReader`'s
    /// first record), and the drain's `put` word (`DrainReader::st` + 2).
    start: Option<(u16, u16)>,
    /// INNER-SPOOL: the reader is a nest's embedded inner (drain/lower.rs `inner_spooled`): each fill (a set's run of the
    /// probe at the nest's re-open, else the one fill) starts the spool empty, the previous re-open's drained row dropped.
    /// D3724: or the probe is a rerun set's member, its drain one too (spool_admit.rs `set_refused`): each run's fill
    /// starts the spool empty, the reader re-opened after it.
    rewind: bool,
    /// D4569 (road drain.spool.wide, strategies/drain_wide.rs): the spool owned, filled at the probe's width.
    wide: bool,
}

impl<'g> EndMech for DrainFill<'g> {
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        match self.wide {
            true => d,
            false => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d },
        }
    }

    /// A4-MATEND (D2371): where this execution's records begin, noted at the fill's open: 0 at the statement's run; on a
    /// recheck node (exec rerun/epq.rs: its own copy of the parameter words) past the statement's records and the drained
    /// row the statement's drain holds at its count (`put`), which the recheck's records and puts never overwrite.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if let Some((s, put)) = self.start {
            if self.rewind {
                self.rec.empty(b, env);
                b.store(env.params, put as i32 * 8, env.zero);
            }
            let n = self.rec.count(b, env);
            let held = b.load(Type::I64, env.params, put as i32 * 8);
            let at = b.int_bin(IntBinOp::Add, n, held);
            b.store(env.params, s as i32 * 8, at);
            b.store(env.params, self.rec.words().1 as i32 * 8, at);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let cells: PgVec<'_, _> = self.cols.iter().map(|&(inner, a)| if inner { row.inner_col(env, a) } else { row.col(a) }).collect_in(self.mcx);
        self.rec.store(b, env, &cells[..], true);
        if let Some(x) = self.pull {
            crate::ops::hashjoin::drain::spool_stream::fill_stop(b, env, row, x);
        }
        row.carry.copy_in(env.mcx)
    }
}

/// The drain's source: the spool's records, then the drained ones put at the count (see the file's head).
struct DrainReader<'g> {
    mcx: Mcx<'g>,
    drain: PgBox<'g, dyn SourceMech + 'g>,
    /// The drain's per-open words (written at its first read) and its own position word `at`.
    words: OpenWords<'g>,
    at: i32,
    rec: Rec<'g>,
    cols: PgVec<'g, Col>,
    /// Per-execution words: the drain's end, its open, `put` (the count's record holds a drained row).
    st: u16,
    /// The reader's position (a per-open word).
    w_j: i32,
    outer: PgVec<'g, Option<u16>>,
    inner: PgVec<'g, Option<u16>>,
    m: u8,
    life: LifeId,
    node: i32,
    /// The streamed probe's object handle word and first streamed word (nest/spool_stream.rs).
    pull: Option<(u16, u16)>,
    /// A4-MATEND (D2371): the fill's start word (`DrainFill::start`), the reader's first record.
    start: Option<u16>,
    /// lockrows lk-0100: a RIGHT SEMI drain whose probe hands its first matches into the spool (probe.rs `semi_hands`):
    /// the spool's records are every row, the marked records never read.
    semi_hands: bool,
    /// D1922 (D3377): the streamed probe is one N1 does not find quiet, pulled a joined row at a time (spool_stream.rs
    /// `pull`, spool_admit.rs `drain_exact`).
    exact: bool,
}

impl<'g> Px<'g, '_> {
    /// Probe `k`'s end in the spooled form (join/drain.rs, where `Gx::spooled_drain` gives drain `d`, its Drain op `jd`).
    pub(crate) fn spool_drain_end(&mut self, k: usize, d: usize, jd: usize) -> PgBox<'g, dyn EndMech + 'g> {
        let node = self.gx.pipelines()[k].node_id;
        let mcx = self.mcx();
        let Some(cols) = self.gx.spool_columns(d, jd) else { shape(node, OUTER_COLS) };
        if self.pass_after.contains(&k) {
            crate::refuse!(node, Reason::Bug { payload: "a spill pair after a spooled drain's probe (build_spill.rs declines it)" })
        }
        let tfs = types(self.gx, node, &cols);
        // INNER-SPOOL (drain/lower.rs `inner_spooled`): a nest's embedded inner reads the spool unstreamed; the nest's set run
        // (or, with no set, the inner's slots, which its owner consumes) orders the fill before the read
        let inner = d >= self.gx.pipelines().len();
        let stream = !inner && self.gx.drain_streams(k, d, jd);
        // writesubplan (merge#368 group, drain/lower.rs `merge_drained_arms`): a streamed probe union's arms fill one spool in
        // arm order (D1752's run); the first arm declares it and notes where this execution's records begin, every later arm
        // appends to it (its open notes nothing), the drain reading from that start. D3915: under a nest's embedded inner the
        // first arm's open empties it (`rewind`) at each fill, and the inner's owner, which consumes the last arm's life,
        // orders the read after every arm as for one probe
        if let Some((first, _)) = self.gx.drained_arms(k) {
            if stream {
                shape(node, ARMS_STREAM);
            }
            if first != k {
                let rec = Rec::find_tfs(self, first, node, tfs);
                self.serial.push(k); // alloc: Program
                if !inner {
                    let ident = self.ident(k);
                    self.declare_proto(Proto::new(ProtoKind::Follows, d as u16).peer(ident as u16));
                }
                return mcx::pg_box_dyn!(mcx, DrainFill { mcx, rec, cols: cols.into_iter().map(|c| c.0).collect_in(mcx), pull: None, start: None, rewind: false, wide: false } => dyn EndMech + 'g);
            }
        }
        // A4-MATEND (D2371): unstreamed, four extra words: the fill's start (`DrainFill::start`), then the drain's state
        // (`DrainReader::st`: its end, its open, `put`)
        let (rec, x) = Rec::declare_tfs(self, k, k, node, tfs, if stream { crate::ops::hashjoin::drain::spool_stream::WORDS } else { 4 });
        // D4569 (road drain.spool.wide): a quiet probe whose drain shows no order fills an owned spool at its width; its
        // start word stays 0 (no recheck: the road asks none), and the reader puts its drained rows in a spool of its own
        let wide = !inner && !stream && crate::ops::hashjoin::strategies::drain_wide::takes(self.gx, k, d, jd);
        let rec = match wide {
            true => {
                fires::fire("drain.spool.wide"); // shapecheck fire signal (D4569): a spooled drain's probe fills an owned spool at width
                rec.owned(self, k)
            }
            false => rec,
        };
        // N4: the spool names the probe's breaker slot (exec rerun/epq.rs tells a drain's spool from nest.spool's by it)
        let Some(m) = breaker_of(&self.gx.pipelines()[k].desc) else { crate::refuse!(node, Reason::Bug { payload: "a spooled drain's probe not ending in its spool breaker" }) };
        if let Some(p) = self.protos.iter_mut().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == k) {
            p.slot = m.slot;
        }
        self.serial.extend([k, d].into_iter().filter(|&q| q < self.gx.pipelines().len() && !(wide && q == k)));
        if stream {
            self.stream_probe(k, d, x);
        } else if !inner {
            let ident = self.ident(k);
            self.declare_proto(Proto::new(ProtoKind::Follows, d as u16).peer(ident as u16));
        }
        // D3724: a rerun set's member probe (its drain a member too: spool_admit.rs `set_refused`) empties it at each run
        let member = self.gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == k));
        mcx::pg_box_dyn!(mcx, DrainFill { mcx, rec, cols: cols.into_iter().map(|c| c.0).collect_in(mcx), pull: stream.then_some(x), start: (!stream && !wide).then_some((x, x + 3)), rewind: inner || member, wide } => dyn EndMech + 'g)
    }

    /// Drain `d`'s source in the spooled form over probe `k`'s spool (`d`'s Drain op `jd`).
    pub(crate) fn spool_drain_source(&mut self, d: usize, k: usize, jd: usize) -> PgBox<'g, dyn SourceMech + 'g> {
        let gx = self.gx;
        let mcx = self.mcx();
        let p = &gx.pipelines()[d];
        let node = p.node_id;
        let Some(Op::Join(JoinDesc::Drain { matches, .. })) = p.ops.get(jd) else { crate::refuse!(node, Reason::Bug { payload: "a spooled drain without its Drain op" }) };
        let (m, Some(life)) = (*matches, gx.life_of(d, *matches)) else { crate::refuse!(node, Reason::Bug { payload: "a spooled drain whose spool has no producer" }) };
        let Some(cols) = gx.spool_columns(d, jd) else { shape(node, OUTER_COLS) };
        // writesubplan (merge#368 group): the drained arms' spool is the first arm's (`spool_drain_end`)
        let k = gx.drained_arms(k).map_or(k, |r| r.0);
        let rec = Rec::find_tfs(self, k, node, types(gx, node, &cols));
        // D4569 (road drain.spool.wide): an owned spool's drained rows go to the reader's put spool (`Rec::with_put`)
        let rec = match rec.is_owned() {
            true => rec.with_put(self, d),
            false => rec,
        };
        // A4-MATEND (D2371): unstreamed, the drain's state is the spool's extra words after the fill's start (`spool_drain_end`)
        let unstreamed = self.streamed_probe(k).is_none().then(|| self.protos.iter().find(|p| p.kind == ProtoKind::Spool && p.peer as usize == k).map(|p| p.words.base + 2)).flatten();
        let st = match unstreamed {
            Some(x) => x + 1,
            None => self.proto_words(3),
        };
        let w_j = self.frame_word(WordKey::Open { face: Face::Nest, k: d, j: u64::MAX, i: 5 }) as i32 * 8;
        let at = self.frame_word(WordKey::Open { face: Face::Nest, k: d, j: u64::MAX, i: 6 }) as i32 * 8;
        // the drain declares the pipeline's slots without the spool; the spool is read last, so the drain's indexes hold
        let drain = (kind_impl::<SourceMaker>(&node, KindId::JoinInner).0)(self, d);
        let mut consumes: Vec<u8> = p.desc.consumes.iter().copied().filter(|&s| s != m).collect(); // alloc: Program
        consumes.push(m); // alloc: Program
        // INNER-SPOOL: a nest's embedded inner declares no slots of its own (its owner's are the inner's, as embedded)
        if d < gx.pipelines().len() {
            self.set_consumes(consumes);
        }
        let mut words = OpenWords::new_in(mcx);
        words.declare(OpenWord::open("join.drain spooled: the drain's position", Base::Words, at, Type::I64, RESUME_OPEN));
        drain.open_words(&mut words);
        let (mut outer, mut inner) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
        for (c, &((inn, a), _)) in cols.iter().enumerate() {
            let v: &mut PgVec<'_, Option<u16>> = if inn { &mut inner } else { &mut outer };
            if v.len() < a as usize {
                v.resize_in(a as usize, None);
            }
            v[a as usize - 1] = Some(c as u16);
        }
        let cols = cols.into_iter().map(|c| c.0).collect_in(mcx);
        let pull = self.streamed_probe(k);
        let start = unstreamed;
        if pull.is_none() && start.is_none() {
            crate::refuse!(node, Reason::Bug { payload: "a spooled drain's reader without its fill's spool" })
        }
        let semi_hands = matches!(p.ops.get(jd), Some(Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. }))) && gx.semi_hands(d);
        let exact = pull.is_some() && !gx.spool_quiet(k);
        mcx::pg_box_dyn!(mcx, DrainReader { mcx, drain, words, at, rec, cols, st, w_j, outer, inner, m, life, node, pull, start, semi_hands, exact } => dyn SourceMech + 'g)
    }
}

impl DrainReader<'_> {
    fn word(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u16) -> Value {
        b.load(Type::I64, env.params, (self.st + i) as i32 * 8)
    }
}

impl SourceMech for DrainReader<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.drain.declare(pb);
    }

    /// The leader's alone (the drain's records are one region; the spool has one reader); the spool's slot read last.
    fn pipeline_desc(&self, px: &Px<'_, '_>, d: usize) -> Pipeline {
        let p = px.gx.pipelines()[d].desc;
        let parallel = match p.parallel {
            Parallel::Eligible => Parallel::Ineligible { site: p.driving_site, reason: Why::NoDrivingSite },
            x => x,
        };
        let mut n = Pipeline { consumes: &[], consumes_from: &[], parallel, ..p };
        for (i, &s) in p.consumes.iter().enumerate().filter(|&(_, &s)| s != self.m) {
            n = match p.consumes_from.get(i) {
                Some(&l) => n.consuming_from_in(px.arena, s, l),
                None => n.consuming_in(px.arena, s),
            };
        }
        n.consuming_from_in(px.arena, self.m, self.life)
    }

    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("join.drain spooled: the reader's position", Base::Words, self.w_j, Type::I64, 0));
    }

    /// A4-MATEND (D2371): the reader's first record is the fill's start (`DrainFill::open`): 0 at the statement's run, past
    /// the statement's records on a recheck node, whose rerun probe appended its joined rows there; and the drain's state
    /// starts unopened (on a recheck node the copy holds the statement's, whose drain may be open at a drained row).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if let Some(s) = self.start {
            let at = b.load(Type::I64, env.params, s as i32 * 8);
            env.store_word(b, self.w_j, at);
            for i in 0..3 {
                b.store(env.params, (self.st + i) as i32 * 8, env.zero);
            }
        }
        env.zero
    }

    /// `[position, count)`; at the count, the drained row put there again (`put`, a stop inside it), else the drain read on
    /// to its next wanted record, put at the count: `[count, count + 1)`; the drain's end is the input's.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let l_ready = b.create_block(&[Type::I64, Type::I64]);
        let (l_more, l_at, l_pull, l_fill, l_put) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let j = env.load_word(b, self.w_j);
        let n = self.rec.count(b, env);
        let held = b.icmp(IntCond::Slt, j, n);
        b.cond_br(held, (l_ready, &[j, n]), (l_more, &[]));
        b.switch_to(l_more);
        let at = b.icmp(IntCond::Eq, j, n);
        b.cond_br(at, (l_at, &[]), (l_pull, &[]));
        b.switch_to(l_at);
        let put = self.word(b, env, 2);
        let again = b.icmp(IntCond::Ne, put, env.zero);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        match self.pull {
            Some(p) => {
                let l_probe = b.create_block(&[]);
                b.cond_br(again, (l_ready, &[n, n1]), (l_probe, &[]));
                b.switch_to(l_probe);
                crate::ops::hashjoin::drain::spool_stream::pull(b, env, &self.rec, p, self.exact, (j, l_ready, l_pull));
            }
            None => b.cond_br(again, (l_ready, &[n, n1]), (l_pull, &[])),
        }
        b.switch_to(l_pull);
        let ended = self.word(b, env, 0);
        let done = match self.semi_hands {
            true => b.icmp(IntCond::Eq, env.zero, env.zero),
            false => b.icmp(IntCond::Ne, ended, env.zero),
        };
        b.cond_br(done, (l_eof, &[env.zero]), (l_fill, &[]));
        b.switch_to(l_fill);
        fill(b, env, (&*self.drain, &[], &self.words), Ok(self.at), (self.st, self.node), (l_eof, l_put), &mut |b, env, r| {
            let cells: PgVec<'_, _> = self.cols.iter().map(|&(inner, a)| if inner { r.inner_col(env, a) } else { r.col(a) }).collect_in(self.mcx);
            self.rec.store(b, env, &cells[..], false);
            Carry::new(env.one, env.mcx)
        });
        b.switch_to(l_put);
        b.store(env.params, (self.st + 2) as i32 * 8, env.one);
        let n = self.rec.count(b, env);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        b.br(l_ready, &[n, n1]);
        b.switch_to(l_ready);
        let v = b.block_params(l_ready);
        Win { cursor: v[0], end: v[1], facts: WinConds::default() }
    }

    /// Record `j` as the joined row: its OUTER_VAR columns the row's, its INNER_VAR ones the join's inner map.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut r = self.rec.read(b, env, j, carry, next);
        r.cols = mcx::to_vec_in(&self.outer[..], env.mcx);
        env.inner = Some(mcx::to_vec_in(&self.inner[..], env.mcx));
        r
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.w_j, cur);
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::graph::{Rerun, Source};
    use gravity_decompose::facts::nest::NestFacts;
    use gravity_decompose::effects::Volatility;
    use gravity_decompose::{Am, NoFacts, PipelineGraph, PipelineNode};
    use gravity_program::{Arena, ArenaBox, ArenaVec, End, SiteDesc};
    use types_nodes::jointype::JoinType;
    use types_nodes::list::NodeList;

    use super::*;

    /// join/drain_spool.rs's fixture: build 0, the probe 1 (a quiet RIGHT / FULL probe into its spool 1), pipeline 2 in the
    /// rerun set of the tail nest's node 77, the drain 3 (Drain op 0) into a sink; `nest`: its tail runs that nest loop (not
    /// an op the probe can run as its own tail), else only a filter-free projection-less tail (the fused form's).
    fn spooled(nest: bool, jt: JoinType, vol: Volatility) -> Option<(usize, usize)> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Why::NoDrivingSite);
        let sink = || Pipeline::serial(-1, End::Sink, Why::NoDrivingSite);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let mut k = PipelineNode::new(a, 2, scan(), into(BreakerKind::Materialize, 1));
        k.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(k);
        g.pipelines.push(PipelineNode::new(a, 5, scan(), into(BreakerKind::Materialize, 2)));
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, sink());
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: jt }));
        if nest {
            let facts = NestFacts { node: 77, invariant: true, volatility: vol, raise_free: true, eq: None };
            let inner = ArenaBox::new_in(PipelineNode::new(a, 9, scan(), sink()), a);
            d.push(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner, qual: nil, unique: false, inner_node: 9, facts }));
        }
        g.pipelines.push(d);
        let mut pipelines = ArenaVec::new_in(a);
        pipelines.push(2);
        g.reruns.push(Rerun { node_id: 77, plan_id: 0, pipelines, slots: [0; 4] });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        gx.spooled_drain(1)
    }

    /// Spooled exactly where the fused form cannot take the tail and join/drain_spool.rs names no reason (design r2, both
    /// ways): a nest tail over a quiet RIGHT / FULL probe spools; a tail the probe runs as its own (none here) stays fused;
    /// a VOLATILE raise-free tail spools too (D2134 gone: user 2026-10-04 15:11Z); a RIGHT SEMI drain spools as a RIGHT one
    /// (D1922).
    #[test]
    fn spooled_where_the_m_rule_meets_no_name() {
        let (i, r) = (Volatility::Immutable, JoinType::JOIN_RIGHT);
        assert_eq!(spooled(true, r, i), Some((3, 0)));
        assert_eq!(spooled(true, JoinType::JOIN_FULL, i), Some((3, 0)));
        assert_eq!(spooled(false, r, i), None, "the fused form takes an all-tail-op drain");
        assert_eq!(spooled(true, r, Volatility::Volatile), Some((3, 0)), "D2134 gone: a VOLATILE raise-free tail spools");
        assert_eq!(spooled(true, JoinType::JOIN_RIGHT_SEMI, i), Some((3, 0)), "D1922: as a RIGHT drain");
    }
}
