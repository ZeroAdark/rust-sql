//! D3128: every execmain seam that takes a `QueryDescHandle` is routed by the Gravity entry (`admit/entry/mod.rs`
//! installs it with `es::<seam>::set(..)`; with the row executor deleted there is no previous seam to keep).
//! An entry handle (zero low word) that reaches the incumbent's registry is a stale handle and panics: with
//! `query_desc_worker_agg_instrument` and `query_desc_fdw_batch_size` unrouted, EXPLAIN ANALYZE over a HashAgg aborted
//! the backend (regress stats_ext). A handle seam added later and left unrouted fails here.

fn handle_seams(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in src.split("seam_core::seam!(").skip(1) {
        let Some(at) = chunk.find("pub fn ") else { continue };
        let rest = &chunk[at + "pub fn ".len()..];
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let after = &rest[name.len()..];
        let Some(open) = after.find('(') else { continue };
        let (mut depth, mut end) = (0usize, None);
        for (i, c) in after[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let params = &after[open..end.expect("a seam's parameter list closes")];
        if params.contains("QueryDescHandle") {
            out.push(name);
        }
    }
    out
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

#[test]
fn the_entry_routes_every_query_desc_handle_seam() {
    let root = manifest_dir();
    let seams = std::fs::read_to_string(root.join("../../backend/executor/execmain_seams/src/lib.rs")).unwrap();
    let entry = std::fs::read_to_string(root.join("src/admit/entry/mod.rs")).unwrap();
    let names = handle_seams(&seams);
    assert!(names.len() >= 40, "the seam scan found only {} handle seams", names.len());
    let missing: Vec<_> = names
        .iter()
        .filter(|n| !entry.contains(&format!("es::{n}::set(")))
        .collect();
    assert!(missing.is_empty(), "execmain seams taking a QueryDescHandle that the Gravity entry does not route: {missing:?}");
}
