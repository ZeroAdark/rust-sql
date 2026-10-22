use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write;
use std::cell::Cell;

use gravity_decompose::{corpus, decompose, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

struct Counting;

// Only the test thread counts: libtest's own threads allocate (its "running for over 60 seconds" notice), and the chunk tests
// below run on threads of their own under `cargo test` (a shared counter would let one chunk's reset hide another's count).
thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ON.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The fixtures (sorted by path) in `CHUNKS` contiguous chunks, one test each, every chunk warmed up and then run 1000 times
/// (TESTS 2026-09-23, gate speed: one test over all of them was 40 s, the gate's longest after kfold).
const CHUNKS: usize = 8;

fn allocate_nothing_after_warmup(c: usize) {
    let dir = manifest_dir().join("tests/plans");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "plan")).collect();
    paths.sort();
    let ctx = MemoryContext::new("fixtures");
    let n = paths.len();
    assert!(n >= CHUNKS, "{n} fixtures");
    let paths = &paths[c * n / CHUNKS..(c + 1) * n / CHUNKS];
    let texts: Vec<String> = paths.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
    let fixtures: Vec<_> = texts.iter().map(|t| corpus::load(ctx.mcx(), t).unwrap()).collect();
    assert!(!fixtures.is_empty());
    let arena = Arena::new(1 << 20);
    let mut out = String::with_capacity(1 << 16);
    let run = |out: &mut String| {
        for fx in &fixtures {
            let h = arena.handle();
            let m = h.mark();
            out.clear();
            match decompose(h, fx.stmt, &fx.facts, ctx.mcx()) {
                Ok(g) => dump::dump(&g, out).unwrap(),
                Err(r) => write!(out, "{r}").unwrap(),
            }
            h.reset_to(m);
        }
    };
    run(&mut out);
    ON.with(|o| o.set(true));
    ALLOCS.with(|a| a.set(0));
    for _ in 0..1000 {
        run(&mut out);
    }
    ON.with(|o| o.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0, "steady-state allocations over {} fixtures (chunk {c} of {CHUNKS})", fixtures.len());
}

macro_rules! chunk_tests {
    ($($name:ident = $c:literal),* $(,)?) => {
        $(#[test]
        fn $name() {
            allocate_nothing_after_warmup($c);
        })*
        const CHUNK_TESTS: &[usize] = &[$($c),*];
    };
}

chunk_tests! {
    decompose_and_dump_allocate_nothing_after_warmup_0 = 0,
    decompose_and_dump_allocate_nothing_after_warmup_1 = 1,
    decompose_and_dump_allocate_nothing_after_warmup_2 = 2,
    decompose_and_dump_allocate_nothing_after_warmup_3 = 3,
    decompose_and_dump_allocate_nothing_after_warmup_4 = 4,
    decompose_and_dump_allocate_nothing_after_warmup_5 = 5,
    decompose_and_dump_allocate_nothing_after_warmup_6 = 6,
    decompose_and_dump_allocate_nothing_after_warmup_7 = 7,
}

#[test]
fn decompose_and_dump_chunks_cover_the_fixtures() {
    assert_eq!(CHUNK_TESTS, (0..CHUNKS).collect::<Vec<_>>().as_slice(), "one test per chunk");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
