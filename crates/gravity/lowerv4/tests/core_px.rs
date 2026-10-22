//! The program context (lowerv4 plan §2.1; KWC lowerv3 core_px): a mechanism with no expressions and mechanisms sharing
//! one expression both assemble into a `Program`, the declared v4 protocols carried in order (`Descs::v4`); a face's extension slot is its own (keyed by face, never by milestone);
//! frame words are numbered once per program and the words site comes after every other; a union's arm is the nearest
//! marker before it; D231: a cancel raised while one pipeline lowers refuses `Interrupted` before the next is emitted.
use gravity_lowerv4_it as common;

use std::cell::Cell;

use common::RigCatalog;
use datum::Datum;
use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
use gravity_expr::Expr;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, Type, Value};
use gravity_lower::{assemble, compile, Backend, Sequential};
use gravity_lowerv4::face::{end::EndMech, kinds::Face, source::SourceMech, step::StepMech};
use gravity_lowerv4::frame::frame::{Mechs, Next, Win};
use gravity_lowerv4::frame::px::{ExId, ExtG, Px, WordKey};
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::stop::{self, Exits, ResumeWord};
use gravity_lowerv4::frame::{env, env::Env, program};
use gravity_lowerv4::{catch, graph::Gx, hir::Ir, Reason};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Arena, ArenaRef, End, Pipeline, SiteKind};
use mcx::MemoryContext;
use types_core::Oid;
use types_nodes::node_tree::Node;
use types_portal::params::ParamBind;

const INT4OID: Oid = 23;

/// One row per window: `.0` the shared expression it reads through `Env`, `.1` a Result's shape (once: a resumed call is
/// done), `.2` pipeline `k` counting its open at emission (D231: pipeline 100's open raises the interrupt).
struct OneRow(Option<ExId>, bool, Option<usize>);

impl SourceMech for OneRow {
    fn once(&self) -> bool {
        self.1
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        if let Some(k) = self.2 {
            OPENS.with(|n| n.set(n.get() + 1));
            if k == 100 {
                init_small::globals::SetInterruptPending(true);
            }
        }
        b.iconst(Type::I64, 0)
    }
    fn next_window(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _l_eof: Block) -> Win {
        if let Some(id) = self.0 {
            assert_eq!(env.ex(id).result_type, INT4OID, "the source reads the shared expression through Env");
        }
        Win { cursor: env.zero, end: env.one, facts: Default::default() }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
}

/// A step that holds the same expression as the source and the end.
struct Same(ExId);

impl StepMech for Same {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        assert_eq!(env.ex(self.0).result_type, INT4OID);
        next(b, env, row)
    }
}

struct CountEnd(Option<ExId>);

impl EndMech for CountEnd {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        if let Some(id) = self.0 {
            assert_eq!(env.ex(id).result_type, INT4OID);
        }
        Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }
}

fn graph<'p>(a: ArenaRef) -> PipelineGraph<'p> {
    let mut g = PipelineGraph::new(a);
    g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
    g
}

/// `f` over a fresh compile context of a one-pipeline graph.
fn with_px(f: impl FnOnce(&mut Px<'_, '_>)) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let g = graph(a);
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut plan = Sequential::new(a);
    f(&mut Px::new(a, &gx, &mut plan));
}

#[test]
fn no_expressions_and_shared_expressions_both_assemble() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let cx = MemoryContext::new("core_px");
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let g = graph(a);
    for shared in [false, true] {
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let ex = shared.then(|| {
            let node = Node::mk_const(cx.mcx(), INT4OID, -1, 0, 4, Datum::from_i32(5), false, true).expect("a const node");
            px.add_ex(Expr::compile(cx.mcx(), node, RigCatalog::get(), ParamBind::NONE).unwrap_or_else(|e| panic!("compile: {e}")))
        });
        let c = px.mcx();
        let steps = mcx::CollectIn::collect_in(ex.map(|id| mcx::pg_box_dyn!(c, Same(id) => dyn StepMech)).into_iter(), c);
        let m = Mechs::new(&mut px, 0, mcx::pg_box_dyn!(c, OneRow(ex, true, None) => dyn SourceMech), steps, mcx::pg_box_dyn!(c, CountEnd(ex) => dyn EndMech));
        assert_eq!((m.site, m.desc.driving_site), (None, -1));
        assert_eq!(px.nexs(), usize::from(shared), "one expression however many mechanisms hold it");
        let protos = if shared { vec![Proto::new(ProtoKind::Walk, 0).slot(1), Proto::new(ProtoKind::Init, 0)] } else { Vec::new() };
        protos.iter().for_each(|&p| px.declare_proto(p));
        let l = program::emit(&mut px, vec![m]);
        assert_eq!((l.funcs.len(), l.descs.layout.nsites, l.descs.layout.out_natts, l.descs.out_region), (1, 0, 0, 0));
        assert_eq!((&*l.descs.v4.protos, l.descs.instr.len()), (&protos[..], 0), "declared protocols in order; no counter cells");
        assert!(l.descs.owner.take().is_none(), "no hosted call: no owner");
        let codes = compile(&l, Backend::Interp).unwrap_or_else(|e| panic!("backend: {}", e.message()));
        let _program = assemble(&l, codes);
    }
}

#[derive(Default)]
struct KeyedState {
    n: u32,
}

#[derive(Default)]
struct FoldState {
    tag: &'static str,
}

// SAFETY: ExtG's contract: `Id` is the type itself, which has no lifetime.
unsafe impl<'g> ExtG<'g> for KeyedState {
    type Id = KeyedState;
    fn make(_: mcx::Mcx<'g>) -> KeyedState {
        KeyedState::default()
    }
}

// SAFETY: ExtG's contract: `Id` is the type itself, which has no lifetime.
unsafe impl<'g> ExtG<'g> for FoldState {
    type Id = FoldState;
    fn make(_: mcx::Mcx<'g>) -> FoldState {
        FoldState::default()
    }
}

/// A state holding the plan's context: its values there, its drops counted.
struct Kept<'g>(mcx::PgVec<'g, u64>, Option<std::rc::Rc<Cell<u32>>>);

impl Drop for Kept<'_> {
    fn drop(&mut self) {
        if let Some(d) = &self.1 {
            d.set(d.get() + 1);
        }
    }
}

// SAFETY: ExtG's contract: the impl is for `Kept<'g>` itself and `Id` is `Kept<'static>`.
unsafe impl<'g> ExtG<'g> for Kept<'g> {
    type Id = Kept<'static>;
    fn make(m: mcx::Mcx<'g>) -> Kept<'g> {
        Kept(mcx::PgVec::new_in(m), None)
    }
}

#[test]
fn each_face_owns_its_extension_slot() {
    with_px(|px| {
        assert!(px.ext::<KeyedState>(Face::Keyed).is_none());
        px.ext_mut::<KeyedState>(Face::Keyed).n += 2;
        px.ext_mut::<FoldState>(Face::Fold).tag = "fold";
        px.ext_mut::<KeyedState>(Face::Keyed).n += 1;
        assert_eq!(px.ext::<KeyedState>(Face::Keyed).map(|s| s.n), Some(3));
        assert_eq!(px.ext::<FoldState>(Face::Fold).map(|s| s.tag), Some("fold"));
        assert!(px.ext::<FoldState>(Face::Keyed).is_none(), "a slot answers only its own type");
        let r = catch(0, || {
            let _ = px.ext_mut::<FoldState>(Face::Keyed);
        });
        assert!(matches!(r.map_err(|r| r.reason), Err(Reason::Bug { .. })), "another type in a face's slot is a Bug, never a reset");
        assert_eq!(px.ext::<KeyedState>(Face::Keyed).map(|s| s.n), Some(3), "the slot kept its state");
    });
}

/// PLANMEM: a face's state set inside a plan lives in the lowering's planning context and is dropped with the plan, its
/// bytes given back; a slot holding it answers no other type.
#[test]
fn a_face_state_is_dropped_with_the_plan_context() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let drops = std::rc::Rc::new(Cell::new(0));
    let before = rig.used();
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let g = graph(a);
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut plan = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut plan);
    let base = rig.used();
    let k = px.ext_mut::<Kept<'_>>(Face::End);
    k.1 = Some(std::rc::Rc::clone(&drops));
    k.0.extend_from_slice(&[1, 2, 3]);
    assert!(rig.used() > base, "the state is in the plan's context");
    assert!(px.ext::<KeyedState>(Face::End).is_none(), "a slot answers only its own type");
    assert_eq!(px.ext::<Kept<'_>>(Face::End).map(|k| k.0.len()), Some(3));
    drop(px);
    assert_eq!(drops.get(), 1, "dropped with the plan");
    assert_eq!(rig.used(), before, "its bytes given back");
}

#[test]
fn frame_words_are_numbered_once_per_program() {
    with_px(|px| {
        assert_eq!(px.add_words_site(), None, "no frame word, no words site");
        let open = WordKey::Open { face: Face::Runs, k: 0, j: 2, i: 0 };
        let offset = ResumeWord::StepOffset { k: 0, j: 2 };
        assert_eq!(px.frame_word(open), 0);
        assert_eq!((stop::resume_word(px, offset), stop::resume_word(px, ResumeWord::EpqRecheck { k: 0 })), (8, 16), "8 bytes a word");
        assert_eq!((px.frame_word(open), stop::resume_word(px, offset)), (0, 8), "a key keeps its word");
        assert_eq!(px.frame_word(WordKey::UnionFlag { union: 1 }), 3);
        let s = px.add_words_site().expect("a words site");
        assert_eq!(s, px.sites.len() - 1, "after every other site");
        assert!(matches!(px.sites[s].kind, SiteKind::Words { n: 4 }));
        px.set_union(0, 7, 2);
        px.set_union(0, 8, 5);
        let under = [px.union_of(0, 6), px.union_of(0, 5), px.union_of(0, 3), px.union_of(0, 2), px.union_of(1, 6)];
        assert_eq!(under, [Some((8, 5)), Some((7, 2)), Some((7, 2)), None, None], "the nearest arm marker before the op");
    });
}

thread_local! {
    static OPENS: Cell<usize> = const { Cell::new(0) };
}

/// D231: an interrupt raised while pipeline 100 lowers refuses `Interrupted` at pipeline 101's check; nothing past pipeline
/// 100 is emitted.
#[test]
fn a_cancel_lands_between_two_pipelines() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    // the backend's seam (the server's is tcop's init_seams): the facade's `pending` reads the interrupt globals through it
    static BACKEND: std::sync::Once = std::sync::Once::new();
    BACKEND.call_once(|| postgres_seams::interrupts_pending::set(|| init_small::globals::InterruptPending() && init_small::globals::InterruptsCanBeProcessed()));
    let desc = Pipeline::eligible(0, End::Sink, 0);
    let mut ms: Vec<Mechs> = (0..200)
        .map(|k| Mechs { k, source: mcx::pg_box_dyn!(rig.mcx(), OneRow(None, false, Some(k)) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), CountEnd(None) => dyn EndMech), site: None, desc, words: None, state_word: None })
        .collect();
    let mut pb = ProgramBuilder::new();
    let core = env::declare(&mut pb);
    let r = catch(0, || drop(program::functions(&mut pb, core, &Ir::none(), &[], &mut ms, &|k| k as i32, &|_| true, (&|| unreachable!("no probe stages"), &|_| None), rig.mcx())));
    init_small::globals::SetInterruptPending(false);
    assert_eq!(r.map_err(|r| (r.reason, r.node)), Err((Reason::Interrupted, 101)));
    assert_eq!(OPENS.with(Cell::get), 101, "pipelines 0..=100 emitted, none after");
}
