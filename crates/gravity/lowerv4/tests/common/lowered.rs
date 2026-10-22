//! The rig's lowering of one plan fixture, kept apart from mod.rs (its budget; V4R-65): a `Lowered` whose descriptors
//! outlive the fixture's arena.
use gravity_decompose::corpus;
use gravity_lower::Lowered;
use gravity_program::Arena;
use mcx::MemoryContext;

use super::{lower, RigCatalog};

/// A lowering with the arena and the memory context its descriptors live in: `descs`' pipelines, sites and outputs are
/// ArenaVecs in the fixture's arena, so a Lowered returned alone read freed memory (a test's SIGSEGV, M3-L0 09-24). It
/// derefs to the Lowered; the fields drop in order, the lowering first.
pub struct LoweredIn {
    pub l: Lowered,
    _arena: Arena,
    _ctx: MemoryContext,
}

impl std::ops::Deref for LoweredIn {
    type Target = Lowered;
    fn deref(&self) -> &Lowered {
        &self.l
    }
}

/// v4's lowering of one plan fixture (with the arena and context it lives in), or its refusal in the seam's form.
pub fn lower_fixture(text: &str) -> Option<gravity_lower::Result<LoweredIn>> {
    lower_fixture_opts(text, gravity_lower::LowerOpts::default())
}

/// `lower_fixture` under the executor's options `opts` (D3168: `shape`, the staged probe's memo key).
pub fn lower_fixture_opts(text: &str, opts: gravity_lower::LowerOpts) -> Option<gravity_lower::Result<LoweredIn>> {
    RigCatalog::install_seams();
    let (ctx, arena) = (MemoryContext::new("v4 rig fixture"), Arena::new(1 << 20));
    let r = {
        let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
        let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
        lower(&arena, &graph, &fx.facts, &mut gravity_lower::Sequential::new(arena.handle()), opts)
    };
    Some(r.map(|l| LoweredIn { l, _arena: arena, _ctx: ctx }))
}

/// A lowering's IR text as the pins hold it: `print_program` with each owner layout's interned wide address named
/// (`wide_named`).
pub fn pin_ir(l: &Lowered) -> String {
    wide_named(gravity_ir::text::print_program(&l.ir), &l.descs)
}

/// V4R-375: `ir` with each owner layout's interned wide address (a heap address, different in every process: keyed
/// table_size's `layout_slot` and emit_group's descriptor bake a `RecordLayout`'s words) named by the layout's keys and
/// slots, so a pin of a wide keyed record (a hosted state's slots) reads the same in every run. D4799: every IR pin goes
/// through it (m2's `regress-full-functional_deps-001` printed a new address in every process).
pub fn wide_named(mut ir: String, d: &gravity_lower::Descs) -> String {
    for l in d.pipelines.iter().filter_map(|p| p.owner.as_ref()).flat_map(|o| [o.layout, o.acc]) {
        // the wide word through `unword` (abi layout.rs: a padded struct on wasm32, the pointer itself on a 64-bit host)
        if let Some(w) = gravity_abi::layout::unword(l.wide) {
            let at = format!("(const i64 {})", w as *const gravity_abi::breaker::WideLayout as usize as i64);
            ir = ir.replace(&at, &format!("(const i64 wide{:?}{:?})", l.key_kinds(), l.slot_kinds()));
        }
    }
    record_wide_named(&ir)
}

/// A wide record passed to host `record` (a fold's record with no owner: slots.rs `record_at`) stores the layout's words
/// in a stack slot; word 4 (offset 32) is the wide tier's interned address, 0 for a narrow layout. A nonzero one is named
/// `wide`, as `wide_named` names an owner's.
fn record_wide_named(ir: &str) -> String {
    let lines: Vec<&str> = ir.lines().collect();
    let mut out = String::with_capacity(ir.len());
    for (i, line) in lines.iter().enumerate() {
        let stored_at_32 = lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with("((store ") && n.contains(" 32 "));
        let record_call = lines[i + 1..lines.len().min(i + 5)].iter().any(|n| n.contains("call_host record"));
        let named = match line.find("(const i64 ") {
            Some(at) if stored_at_32 && record_call && !line.ends_with("(const i64 0))") => format!("{}(const i64 wide))", &line[..at]),
            _ => (*line).to_owned(),
        };
        out.push_str(&named);
        out.push('\n');
    }
    if !ir.ends_with('\n') {
        out.pop();
    }
    out
}

/// `lower_fixture`, and whether the admission logged a by-reference varlena image equal to `image` as baked (V4R-18's
/// bake scopes: gravity_expr's IR reads and gravity_decompose's plan-tree reads; the front key's variants carry those
/// images). A refused lowering reads no image (its context is gone) and answers false.
pub fn lower_fixture_baked(text: &str, image: &[u8]) -> Option<(gravity_lower::Result<LoweredIn>, bool)> {
    let (e, d) = (gravity_expr::bake::Scope::open(), gravity_decompose::bake::Scope::open());
    let r = lower_fixture(text).map(|r| {
        let mut reads = e.reads();
        reads.extend(d.reads());
        let baked = r.is_ok() && reads.iter().any(|&at| {
            // SAFETY: each read names a non-null by-reference image of the fixture's plan, live in the LoweredIn's
            // context (or a program's copy); a varlena's header gives its length before any byte past it is read.
            unsafe { varlena(at as *const u8) == Some(image) }
        });
        (r, baked)
    });
    drop(d);
    drop(e);
    r
}

/// The varlena image at `p` (1-byte short or 4-byte header), `None` for any other header.
unsafe fn varlena<'a>(p: *const u8) -> Option<&'a [u8]> {
    let b0 = *p;
    let len = if b0 & 0x01 == 1 { (b0 >> 1) as usize } else if b0 & 0x03 == 0 { (u32::from_le_bytes([b0, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize } else { return None };
    (len >= 1).then(|| core::slice::from_raw_parts(p, len))
}
