//! D645 (E2E, main 09-26): a by-reference output row wider than the out block's whole region (16 KiB for up to four
//! by-reference columns, `out_region_bytes`) exited with unsupported(5), where PostgreSQL returns the row. The sink's cold
//! arm now pushes the block's rows, has the driver swap the region for a grant-drawn one that holds the row
//! (`gravity_abi::program::out_grow`, the spill op `SPILL_OUT_GROW`), and stores the row as any other.
//!
//! regress-008's VALUES statement, its text literals replaced by images of 1 B to 1 MiB (the first past the region with
//! no row before it, the others behind held rows), runs through the real driver in every rig mode and returns every row
//! whole, in order.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

use common::multiset::Outcome;

/// The text lengths of the rows (`i` = 1..): the region's edge (16380 fits a 16 KiB region with its 4-byte header, 16381
/// does not), a first row past it, wide rows behind small ones, and a run of mixed rows past one block.
fn lengths() -> Vec<usize> {
    let mut v = vec![16381, 16380, 1, 1 << 20, 65536, 3, 3 * 16384, 16384];
    v.extend((0..60).map(|k| [5, 20000, 7][k % 3]));
    v
}

/// Row `i`'s text: `n` bytes of one letter (by `i`), so a row read from another's image shows.
fn text(i: usize, n: usize) -> String {
    char::from(b'a' + (i % 26) as u8).to_string().repeat(n)
}

/// A text literal's CONST node over the 4-byte-header image of `s`.
fn text_const(s: &str) -> String {
    let len = s.len() + 4;
    let hdr = ((len as u32) << 2).to_le_bytes();
    let bytes: Vec<String> = hdr.iter().copied().chain(s.bytes()).map(|b| b.to_string()).collect();
    format!("{{CONST :consttype 25 :consttypmod -1 :constcollid 100 :constlen -1 :constbyval false :constisnull false :location 26 :constvalue {len} [ {} ]}}", bytes.join(" "))
}

fn int_const(i: usize) -> String {
    let b = (i as u32).to_le_bytes();
    format!("{{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 23 :constvalue 4 [ {} {} {} {} 0 0 0 0 ]}}", b[0], b[1], b[2], b[3])
}

/// regress-008 (`SELECT * FROM (VALUES (1, 'x'), (2, 'y')) v(i, s) WHERE i > 0`) with `rows` as its VALUES lists.
fn plan(rows: &[(usize, String)]) -> String {
    let text = std::fs::read_to_string(common::plans_dir().join("regress-008.plan")).expect("regress-008.plan");
    let (head, rest) = text.split_once(":values_lists (").expect("regress-008's VALUESSCAN lists");
    let tail = &rest[rest.find("))}").expect("the lists' end") + 1..];
    let lists: Vec<String> = rows.iter().map(|(i, s)| format!("({} {})", int_const(*i), text_const(s))).collect();
    format!("{head}:values_lists ({}{tail}", lists.join(" "))
}

#[test]
fn a_row_wider_than_the_out_region_is_returned_whole() {
    let rows: Vec<(usize, String)> = lengths().into_iter().enumerate().map(|(k, n)| (k + 1, text(k + 1, n))).collect();
    let want: Vec<Vec<Option<String>>> = rows.iter().map(|(i, s)| vec![Some(i.to_string()), Some(s.clone())]).collect();
    let fixture = plan(&rows);
    for mode in common::MODES {
        let got = common::run_fixture(&fixture, mode).expect("decomposes").unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        if matches!(mode, common::Mode::Cancel(_)) && got == common::cancel_error() {
            continue;
        }
        let Outcome::Rows(got) = got else { panic!("{mode:?}: {got:?}, PostgreSQL returns {} rows", want.len()) };
        assert_eq!(got.len(), want.len(), "{mode:?}: row count");
        for (k, (g, w)) in got.iter().zip(&want).enumerate() {
            let len = |r: &Vec<Option<String>>| r[1].as_ref().map_or(0, |s| s.len());
            assert!(g == w, "{mode:?}: row {k} differs (got i {:?}, {} B; want i {:?}, {} B)", g[0], len(g), w[0], len(w));
        }
    }
}
