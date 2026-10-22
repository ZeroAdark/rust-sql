// E2 (NORM-TEXT-SEARCH, main's review C1): an int4 compare of strpos / position with a by-value constant is keyed by its
// class (gravity_expr percode `textpos_test`: the needle occurs, does not, neither), never its literal, in both keys (percode
// `textpos_class`: lower key.rs, exec front.rs). percode `text_search` bakes the class (the LIKE twin's table, step/text_like.rs's
// claim), so `strpos(x, c) > 5` right after `> 0` must not run the LIKE program; `> 5` and `> 7` still share one. Over
// regress collate.icu.utf8-028's `strpos(b COLLATE ctest_det, 'bc')`, wrapped in each compare (OpExpr and FuncExpr), and
// as an operator over textpos (`CREATE OPERATOR ### (.., FUNCTION = strpos)`: an OpExpr of opfuncid 868).
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

const PLAN: &str = "../decompose/tests/plans/regress-full/collate.icu.utf8-028.plan";
const CALL: &str = "{FUNCEXPR :funcid 868 ";

/// The fixture with its strpos call (the target list's expression) wrapped in `op` / `funcid` against the int4 `k`, the
/// call on the left when `left`, as an OpExpr or (`func`) a FuncExpr; the call itself an operator over textpos when `pos_op`.
fn fixture(op: u32, funcid: u32, k: i32, left: bool, func: bool, pos_op: bool) -> String {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(text.matches(CALL).count(), 1, "the fixture's one strpos call");
    let at = text.find(CALL).unwrap();
    let (mut depth, mut end) = (0i32, at);
    for (i, c) in text[at..].char_indices() {
        depth += match c { '{' => 1, '}' => -1, _ => 0 };
        if depth == 0 {
            end = at + i + 1;
            break;
        }
    }
    let bytes = k.to_le_bytes().iter().chain(&[0u8; 4]).map(|b| b.to_string()).collect::<Vec<_>>().join(" ");
    let konst = format!("{{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 0 :constvalue 4 [ {bytes} ]}}");
    let call = if pos_op {
        text[at..end].replacen(" :funcresulttype 23 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 ", " :opresulttype 23 :opretset false :opcollid 0 ", 1).replacen(CALL, "{OPEXPR :opno 16384 :opfuncid 868 ", 1)
    } else {
        text[at..end].to_string()
    };
    assert_eq!(call.starts_with("{OPEXPR"), pos_op, "the call's node form");
    let args = if left { format!("({call} {konst})") } else { format!("({konst} {call})") };
    let cmp = if func {
        format!("{{FUNCEXPR :funcid {funcid} :funcresulttype 16 :funcretset false :funcvariadic false :funcformat 0 :funccollid 0 :inputcollid 0 :args {args} :location 0}}")
    } else {
        format!("{{OPEXPR :opno {op} :opfuncid {funcid} :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args {args} :location 0}}")
    };
    format!("{}{cmp}{}", &text[..at], &text[end..])
}

/// (the front key's main hash, the content key) of `fixture(..)`.
fn keys(op: u32, funcid: u32, k: i32, left: bool, func: bool, pos_op: bool) -> (u128, ContentKey) {
    let text = fixture(op, funcid, k, left, func, pos_op);
    let (ctx, arena) = (MemoryContext::new("e2"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let f = front::walk(arena.handle(), fx.stmt, None).expect("the fixture walks");
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    (f.main, gravity_lower::inputs::key::key(&g))
}

const GT: (u32, u32) = (521, 147);
const EQ: (u32, u32) = (96, 65);
const LT: (u32, u32) = (97, 66);
const GE: (u32, u32) = (525, 150);

#[test]
fn the_compare_class_keys_apart_never_the_literal() {
    let k = |(op, f): (u32, u32), k: i32, left: bool| keys(op, f, k, left, false, false);
    let apart = |a: (u128, ContentKey), b: (u128, ContentKey), what: &str| {
        assert_ne!(a.0, b.0, "{what}: the front key holds the class");
        assert_ne!(a.1, b.1, "{what}: the content key holds the class");
    };
    // the literal's class moves both keys: B right after A would run A's program
    apart(k(GT, 0, true), k(GT, 5, true), "> 0 (contains) / > 5 (a position)");
    apart(k(GT, 0, true), k(GT, -1, true), "> 0 / > -1 (always)");
    apart(k(EQ, 0, true), k(EQ, 2, true), "= 0 (absent) / = 2");
    apart(k(GE, 1, true), k(GE, 0, true), ">= 1 / >= 0");
    apart(k(LT, 1, true), k(LT, 2, true), "< 1 / < 2");
    apart(k(LT, 0, false), k(LT, 1, false), "0 < s / 1 < s");
    apart(keys(GT.0, GT.1, 0, true, true, false), keys(GT.0, GT.1, 5, true, true, false), "int4gt(strpos(..), 0) / (.., 5), a FuncExpr");
    // within one class the literal is a word: one front key, one content key
    assert_eq!(k(GT, 5, true), k(GT, 7, true), "> 5 / > 7: one class (a position), one key");
    assert_eq!(k(GT, 5, true), k(GT, 1000, true), "> 5 / > 1000");
    assert_eq!(k(EQ, 2, true), k(EQ, 3, true), "= 2 / = 3");
    assert_eq!(k(GT, -1, true), k(GT, -7, true), "> -1 / > -7: always true");
    assert_eq!(k(GT, 0, true), k(GT, 0, true), "one class, one key");
}

/// Round 1 (main's review): percode `text_search` takes a textpos operator (`t ### 'o'`, an OpExpr of opfuncid 868) as
/// strpos, so both keys must hold its compare's class too: `(t ### 'o') > 5` right after `> 0` ran the LIKE program.
#[test]
fn an_operator_over_textpos_keys_its_class_too() {
    let k = |(op, f): (u32, u32), k: i32, left: bool, func: bool| keys(op, f, k, left, func, true);
    let apart = |a: (u128, ContentKey), b: (u128, ContentKey), what: &str| {
        assert_ne!(a.0, b.0, "{what}: the front key holds the class");
        assert_ne!(a.1, b.1, "{what}: the content key holds the class");
    };
    apart(k(GT, 0, true, false), k(GT, 5, true, false), "(t ### c) > 0 / > 5");
    apart(k(EQ, 0, true, false), k(EQ, 2, true, false), "(t ### c) = 0 / = 2");
    apart(k(LT, 0, false, false), k(LT, 3, false, false), "0 < (t ### c) / 3 < ..");
    apart(k(GT, 0, true, true), k(GT, 5, true, true), "int4gt(t ### c, 0) / (.., 5), a FuncExpr");
    assert_eq!(k(GT, 5, true, false), k(GT, 7, true, false), "(t ### c) > 5 / > 7: one class, one key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}

/// NEW-19: gravity_program::twin holds the LIKE twins' spelling rules; the functions are named in two homes, normalize's
/// consts over the plan and gravity_fn typesem's names over a program (rule R3 keeps a literal OID list in such a home,
/// and the layering keeps decompose off gravity_fn). Both name one list.
#[test]
fn twin_homes_name_one_list() {
    use gravity_decompose::normalize::{F_STARTS_WITH, F_TEXTPOS};
    use gravity_fn::typesem::expr::{starts_with, text_position, TEXT_POSITION};
    assert_eq!(F_TEXTPOS, TEXT_POSITION, "the textpos functions");
    for f in 0..=20_000u32 {
        assert_eq!(text_position(f), F_TEXTPOS.contains(&f), "text_position({f})");
        assert_eq!(starts_with(f), f == F_STARTS_WITH, "starts_with({f})");
    }
}
