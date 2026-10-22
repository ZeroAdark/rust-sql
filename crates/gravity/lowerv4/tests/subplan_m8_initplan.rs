//! M8-SUB B1 (nest.initplan; M8-plan §3.7, L0-g): an InitPlan's pipeline ends in nest/initplan.rs's cell end (graph.rs
//! `end_need`), which declares the Init proto on it (its handle word, its pair count and first setParam), and its readers
//! load its parameter where they read it (params.rs `InitLoad`, registered by nest.initplan). Over decompose fixtures.
use gravity_lowerv4_it as common;

use gravity_lower::Sequential;
use gravity_lowerv4::frame::diag;
use gravity_program::protos::ProtoKind;

/// `rel`'s blockers in the all-blockers mode (tests/m8_l0w.rs's harness).
fn blockers(rel: &str) -> (Vec<diag::Blocker>, Vec<String>) {
    common::RigCatalog::install_seams();
    let p = common::plans_dir().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let (ctx, arena) = (mcx::MemoryContext::new("m8 initplan fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("{rel}: decompose {e:?}"));
    let mut plan = Sequential::new(arena.handle());
    gravity_expr::nodes::func::acl_defer_begin();
    let (_, report) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, common::RigCatalog::get(), &mut plan, gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    (report.blockers, report.lines)
}

/// The InitPlan fixtures B1 serves or names: EXPR / EXISTS / ROWCOMPARE / MULTIEXPR, in filters, one-time gates, index keys,
/// LIMIT bounds, targetlists; an InitPlan reading another (min / max's).
const FIXTURES: &[&str] = &[
    "cte-005.plan",
    "cte-007.plan",
    "cte-008.plan",
    "cte-009.plan",
    "cte-010.plan",
    "cte-037.plan",
    "cte-040.plan",
    "regress-052.plan",
    "tpch-011.plan",
    "tpch-015.plan",
    "regress-full/subselect-018.plan",
    "regress-full/subselect-019.plan",
    "regress-full/update-009.plan",
    // an index key over an InitPlan that reads pg_sleep's store: L0-l refuses a store only when a call reads the parameter
    "oltp-043.plan",
];

/// No maker or admission names nest.initplan in its refusal text.
#[test]
fn no_initplan_fixture_refuses_by_nest_initplan() {
    let named = |rel: &str| {
        let (bs, lines) = blockers(rel);
        eprintln!("{rel}: {lines:?}");
        (bs.iter().any(|b| b.refusal.reason.to_string().contains("nest.initplan")), lines)
    };
    for rel in FIXTURES {
        let (hit, lines) = named(rel);
        assert!(!hit, "{rel}: {lines:?}");
    }
}

/// cte-008 `SELECT a FROM t WHERE a < 3 AND (SELECT count(*) FROM u) > 10`: served; the count's drain (p1) ends in the
/// cell end, its Init proto carries one handle word, one pair and $exec0.
#[test]
fn an_initplan_read_by_a_gate_serves_with_its_init_proto() {
    let text = std::fs::read_to_string(common::plans_dir().join("cte-008.plan")).expect("fixture");
    let l = common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|r| panic!("cte-008 refused: {r:?}"));
    let inits: Vec<_> = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Init).collect();
    assert_eq!(inits.len(), 1, "{:?}", l.descs.v4.protos);
    assert_eq!((inits[0].pipeline, inits[0].words.n, inits[0].arg), (1, 1, 1), "p1, one handle word, one pair of $exec0");
}

/// The Init members run on demand, out of program order (M8-RUNTIME's `m8_init_run`): emit renumbers their slots with
/// D1005's deferred ones, so over every InitPlan fixture that lowers, each Init's `members_in` closure (the runtime's) has
/// no `shared` producer, and no pipeline outside it reads or fills a slot a member fills.
#[test]
fn an_inits_members_own_their_slots() {
    let mut seen = 0;
    for rel in FIXTURES {
        let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let (ps, protos) = (&l.descs.pipelines[..], &l.descs.v4.protos[..]);
        for p in protos.iter().filter(|p| p.kind == ProtoKind::Init).map(|p| p.pipeline as usize) {
            seen += 1;
            let m = gravity_program::protos::members_in(ps, protos, p);
            assert!(m.shared.is_empty() && m.members.contains(&(p as u16)), "{rel}: p{p}: {m:?}");
            for &x in m.members.iter() {
                let Some(s) = ps[x as usize].produces() else { continue };
                let outside = (0..ps.len()).filter(|q| !m.members.contains(&(*q as u16)));
                assert!(outside.clone().all(|q| ps[q].produces() != Some(s) && !ps[q].consumes.contains(&s)), "{rel}: member p{x}'s slot {s} is also a non-member's");
            }
        }
    }
    assert!(seen >= 5, "{seen} Init protos over the lowered fixtures");
}

/// D1114: the assembled program (gravity_lower::assemble, what exec runs) keeps each Init's closure: no sink declaring
/// nothing takes a member's slot by the legacy chain default, which would make an aggregate producer `shared` and run it
/// in program order (a volatile call or an error where PG never reads the parameter). cte-008's count is one.
#[test]
fn an_assembled_program_keeps_each_inits_closure() {
    let mut seen = 0;
    for rel in FIXTURES {
        let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let codes = gravity_lower::compile(&l, gravity_lower::Backend::Interp).unwrap_or_else(|e| panic!("{rel}: backend: {}", e.message()));
        let program = gravity_lower::assemble(&l, codes);
        let (ps, protos) = (&l.descs.pipelines[..], &l.descs.v4.protos[..]);
        for p in protos.iter().filter(|p| p.kind == ProtoKind::Init).map(|p| p.pipeline as usize) {
            let m = gravity_program::protos::members_in(ps, protos, p);
            seen += usize::from(m.members.len() > 1);
            assert_eq!(program.members(p), m, "{rel}: p{p}: the runtime's closure is not the lowering's");
        }
        for (i, (a, d)) in program.pipelines.iter().zip(ps.iter()).enumerate() {
            assert_eq!(a.consumes, d.consumes, "{rel}: p{i} consumes what the lowering declared, no more");
        }
    }
    assert!(seen >= 1, "{seen} Inits with a producer over the lowered fixtures");
}

/// Whether function `f` of `ir`, or a function it calls, calls host `name` (`seen`: the functions on the walk).
fn calls_host(ir: &gravity_ir::Program, f: usize, name: &str, seen: &mut Vec<bool>) -> bool {
    if std::mem::replace(&mut seen[f], true) {
        return false;
    }
    ir.funcs[f].blocks.iter().flat_map(|b| b.insts.iter()).any(|i| match i.op {
        gravity_ir::Op::CallHost { callee, .. } => ir.host_fns[callee.index()].name == name,
        gravity_ir::Op::Call { callee, .. } => calls_host(ir, callee.index(), name, seen),
        _ => false,
    })
}

/// D1035: a reader's `m8_init_run` finds its cell through the leader's frame, which a pool thread does not have, so emit
/// pins every pipeline whose code can run an InitPlan to the leader. Over every InitPlan fixture that lowers, by the host's
/// name in the emitted IR: no such pipeline is eligible, but the one reader of an INIT_EAGER Init (D1035 step 2, V4R-444:
/// the Init proto's `peer`), whose members the runtime runs ahead of it when it runs on workers.
#[test]
fn an_initplan_run_sits_in_a_leader_pipeline() {
    let mut seen = 0;
    for rel in FIXTURES {
        let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        for (i, f) in l.funcs.iter().enumerate() {
            if calls_host(&l.ir, f.index(), "m8_init_run", &mut vec![false; l.ir.funcs.len()]) {
                seen += 1;
                let eager = l.descs.v4.protos.iter().any(|p| p.kind == gravity_program::protos::ProtoKind::Init && p.arg & gravity_kernels::ops::shared::object::entries::INIT_EAGER != 0 && p.peer as usize == i);
                assert!(eager || !l.descs.pipelines[i].is_eligible(), "{rel}: p{i} runs an InitPlan on a pool thread: {:?}", l.descs.pipelines[i].parallel);
            }
        }
    }
    assert!(seen >= 5, "{seen} pipelines that run an InitPlan over the lowered fixtures");
}

/// D1114's ratchet (main 12:14Z): every corpus program assembled both ways, the chain default (`with_pipelines`)
/// against the declared consumes (`with_declared_pipelines`, gravity_lower::assemble's). Each pipeline whose consumes
/// differ is classed from its plan, never from the pin's label (main 09-29): `on-demand` (the default bound it to an
/// Init's member, which must stay on demand), `pulled` (a nest.cte reader of its CTE's pulled set), `subhash-set` (the
/// default bound it to the SubHash end of a hashed SubPlan's set, and it does not declare that slot: it reads the set
/// through its handle), `streamed-arm` (a streamed union's arm, its own site's reader, after another arm's producer) or
/// `init-self` (the pipeline is itself an Init's own member and declares nothing: the Init protocol runs it on demand,
/// so no chain binding applies; B2's ScanStore Init reader) or `unread` (no pipeline of the declared program consumes the
/// slot the default bound it to: an unread data-modifying CTE's store, PG's statement-end run of a writing CTE nothing
/// reads, beside the statement's own write: ctescan D1687). fixtures/m8/d1114_census.tsv pins the (plan, pipeline,
/// class) rows: `d1114_verdict` fails a new row, a pinned label the plan's class contradicts, and `other`; a pinned row
/// gone is reported. A relabelled row is checked to fail. LOWERV4_PIN=write records it.
#[test]
fn every_chain_default_difference_is_pinned() {
    unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
        0
    }
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for p in std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()) {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "plan") {
                out.push(p);
            }
        }
    }
    let fixtures = common::milestone_dir(8).parent().expect("tests/fixtures").to_path_buf(); // D793-b: read at run time
    let mut files = Vec::new();
    for (tag, dir) in [("plans", common::plans_dir()), ("fixtures", fixtures.clone())] {
        let mut v = Vec::new();
        walk(&dir, &mut v);
        files.extend(v.into_iter().map(|p| (format!("{tag}/{}", p.strip_prefix(&dir).unwrap().display()), p)));
    }
    files.sort();
    let mut rows = std::collections::BTreeSet::new();
    for (rel, p) in files.iter() {
        let text = std::fs::read_to_string(p).expect("fixture");
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let d = &l.descs;
        let build = |declared: bool| {
            let (codes, w) = (d.pipelines.iter().map(|_| gravity_program::Code::hand(noop)), d.write.copy_in(*d.sites.allocator()));
            match declared {
                true => gravity_program::Program::with_declared_pipelines(d.layout, &d.sites, &d.out, w, d.pipelines.iter().copied().zip(codes)),
                false => gravity_program::Program::with_pipelines(d.layout, &d.sites, &d.out, w, d.pipelines.iter().copied().zip(codes)),
            }
        };
        let (chain, decl) = (build(false), build(true));
        let protos = &d.v4.protos[..];
        let inits: Vec<Vec<u16>> = protos.iter().filter(|p| p.kind == ProtoKind::Init).map(|p| gravity_program::protos::members_in(&decl.pipelines[..], protos, p.pipeline as usize).members).collect();
        // nest.cte (CT-2): a CTE's pulled set runs only through its readers' m8_cte_pull; a reader keeps consuming its slot
        let pulled: Vec<Vec<u16>> = protos.iter().filter(|p| p.kind == ProtoKind::Cte).map(|p| gravity_program::protos::members_in(&decl.pipelines[..], protos, p.pipeline as usize).members).collect();
        let streamed = |i: usize| protos.iter().any(|a| a.kind == ProtoKind::Arms && a.slot == u8::MAX && (a.pipeline as usize..a.pipeline as usize + a.arg as usize).contains(&i));
        for i in (0..decl.pipelines.len()).filter(|&i| chain.pipelines[i].consumes != decl.pipelines[i].consumes) {
            let s = chain.pipelines[i].consumes[0];
            let x = (0..i).rev().find(|&k| decl.pipelines[k].produces() == Some(s)).map_or(u16::MAX, |x| x as u16);
            // B2a: the SubHash end of a hashed SubPlan's set, which the reader takes through the set's handle (not a declared slot)
            let set = protos.iter().any(|p| p.kind == ProtoKind::SubHash && p.pipeline == x) && !decl.pipelines[i].consumes.contains(&s);
            // an Init's own member that declares nothing: the Init protocol runs it on demand (B2's ScanStore Init reader)
            let own = inits.iter().any(|g| g.contains(&(i as u16))) && decl.pipelines[i].consumes.is_empty();
            // an unread data-modifying CTE's store: no pipeline of the declared program consumes slot `s` from producer `x`
            let unread = (x as usize) < decl.pipelines.len() && decl.pipelines.iter().all(|q| !q.consumes.contains(&s));
            let class = if inits.iter().any(|g| g.contains(&x)) { "on-demand" } else if pulled.iter().any(|g| g.contains(&x)) { "pulled" } else if set { "subhash-set" } else if streamed(i) { "streamed-arm" } else if own { "init-self" } else if unread { "unread" } else { "other" };
            rows.insert(format!("{rel}\tp{i}\t{class}"));
        }
    }
    let pin = fixtures.join("m8/d1114_census.tsv");
    let head = "# D1114: pipelines whose consumes the chain default changes (plan, pipeline, class); every_chain_default_difference_is_pinned\n";
    if std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write") {
        std::fs::write(&pin, rows.iter().fold(head.to_owned(), |t, r| t + r + "\n")).expect("pin");
    }
    let pinned: std::collections::BTreeSet<String> = std::fs::read_to_string(&pin).expect("the pin").lines().filter(|l| !l.starts_with('#')).map(str::to_owned).collect();
    pinned.difference(&rows).for_each(|r| eprintln!("D1114: pinned row gone (the ratchet may drop it): {r}"));
    let bad = d1114_verdict(&rows, &pinned);
    assert!(bad.is_empty(), "D1114: chain-default differences not pinned (a new row, a label the plan contradicts, or `other`):\n{}", bad.join("\n"));
    assert!(rows.len() >= 40, "{} rows: the corpus did not lower", rows.len());
    // main 09-29: a row pinned under a class its plan does not have fails (a real on-demand row relabelled subhash-set)
    let probe = rows.iter().find(|r| r.ends_with("\ton-demand")).expect("an on-demand row in the corpus");
    let mut relabelled = pinned.clone();
    relabelled.remove(probe);
    relabelled.insert(probe.replace("\ton-demand", "\tsubhash-set"));
    assert!(!d1114_verdict(&rows, &relabelled).is_empty(), "D1114: a row relabelled against its plan's class must fail: {probe}");
}

/// D1114's verdict: every computed `(plan, pipeline, class)` row must be pinned under the class its plan has; a pinned
/// label for the same (plan, pipeline) that differs, an unpinned row and any `other` are violations.
fn d1114_verdict(rows: &std::collections::BTreeSet<String>, pinned: &std::collections::BTreeSet<String>) -> Vec<String> {
    let labels: std::collections::HashMap<&str, &str> = pinned.iter().filter_map(|r| r.rsplit_once('\t')).collect();
    let mut bad = Vec::new();
    for r in rows {
        let Some((key, class)) = r.rsplit_once('\t') else { continue };
        match labels.get(key) {
            Some(&l) if l == class => {}
            Some(&l) => bad.push(format!("{key}: pinned `{l}`, its plan's class is `{class}`")),
            None => bad.push(format!("{r}: not pinned")),
        }
        if class == "other" {
            bad.push(format!("{r}: `other` (a difference no class explains)"));
        }
    }
    bad
}
