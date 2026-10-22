//! cvfan-st fp1 (front memo, session memory): a filed front `Entry` carries a `Charge` on the filing backend's session for
//! its boxed slices (`front::file`), dropped with the entry at `forget` / `clear`. The session's committed bytes grow by the
//! filed bytes at the fill and return to where they were when the variant goes.
use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::Arena;
use mcx::MemoryContext;

fn fixture() -> String {
    std::fs::read_to_string(manifest_dir().join("../decompose/tests/plans/oltp-001.plan")).unwrap()
}

#[test]
fn a_filed_front_variant_is_on_the_session_and_leaves_with_it() {
    std::thread::spawn(|| {
        memledger::bind_backend(4801, 94_801);
        let session = memledger::Owner::Session(memledger::current());
        let text = fixture();
        let (ctx, arena) = (MemoryContext::new("fp1"), Arena::new(1 << 22));
        let fx = corpus::load(ctx.mcx(), &text).unwrap();
        let f = front::walk(arena.handle(), fx.stmt, None).unwrap();
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
        let mut nodes = Vec::new();
        let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
        let needs = front::needs(&f, fx.stmt, &fx.facts);
        front::clear();
        let before = memledger::charge::owner_row(session).committed;
        front::fill(&f, key, &nodes, needs);
        assert_eq!(front::memo_len(), 1, "filed");
        let filed = memledger::charge::owner_row(session).committed;
        assert!(filed > before, "the session's committed bytes grow by the entry's heap ({before} -> {filed})");
        front::forget(key);
        assert_eq!(front::memo_len(), 0, "forgotten");
        assert_eq!(memledger::charge::owner_row(session).committed, before, "and the bytes return with the entry");
        front::fill(&f, key, &nodes, needs);
        front::clear();
        assert_eq!(memledger::charge::owner_row(session).committed, before, "clear returns them too");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
