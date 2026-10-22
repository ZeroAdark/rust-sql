//! V4R-18 (main 09-24, lever (b)): the plan tree's by-reference Const values are read only through the bake log's
//! accessor. gravity_expr enforces the IR side by visibility (`ConstNode`'s value is private to `nodes/konst.rs`); a
//! plan `Const`'s `constvalue` is PostgreSQL's public field, so this lint counts its reads in the crates that decompose,
//! key, lower, host and admit a program (every gravity crate that reads a plan `Const`: exec added by V4R18-LAND). Each file's count is listed below with why each read is no unlogged bake: the
//! accessor itself, a by-value read (a guard on `constbyval` / the type: a word or a keyed value, never an image), the
//! keys (a key is no bake), or a by-reference value handed on as a word bound per execution. A new read fails until it
//! goes through `gravity_decompose::bake::read` (or `expr::const_bytes`) or is listed here with its reason; a removed
//! one fails until the count comes down (a ratchet). D1102: the accessor logs by-reference images only, so each
//! `bake::read` call is listed too (`DECIDES`), with what a by-value Const read there means for the program cache.
//! D1099 P4 (main 14:14Z): the IR side of the same question. gravity_expr's `ConstNode` keeps its value private and its
//! `read` logs a by-reference image only, so each `.read()` / `.fields()` call in a file that names `ConstNode` or
//! `ExprNode::Const` is listed with its by-value class (`IR_READS`), and each `.need()` / `.list()` (the word plan's) too
//! (`IR_WORDS`), on the same ratchet.
//!
//! Lives in gravity_lint (INFRA-VELOCITY 09-24; moved from exec/tests): push-check's lint stage runs `cargo test -p
//! gravity_lint` on every M1 landing, while exec's tests ran in no gate (T0 picks the touched crates only, no reverse deps),
//! so a new read in lowerv4 (c0ea8ee8cf3, rep/varlena/cmp_num.rs) landed red. Workspace-wide source lints belong here.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// (file under crates/gravity, `constvalue` reads in code, why none is an unlogged bake).
const READS: &[(&str, usize, &str)] = &[
    ("decompose/src/bake.rs", 2, "the accessor (`read`): logs a non-null by-reference image"),
    ("decompose/src/ops/agg/numeric.rs", 2, "by value: a by-value int4 typmod argument and round / trunc's int4 scale (PS4-13), each behind a `constbyval` filter; exec front.rs keys both by value (a cast's typmod, `numeric_scale_arg`)"),
    ("decompose/src/expr.rs", 6, "by value: `print`'s by-value arm (the by-reference one reads `const_bytes`, the accessor); keys: `const_bytes_keyed`, a constant pattern's image for DC-14's class, which both keys hold, and normalize `twin_const`'s needle, a front hit's rebuild of a twin's pattern from a needle the variant carries (`like_twin` logged it) (exec front.rs `pattern_class` puts decompose effects `pattern_good`'s answer, the content key hashes the image); `const_text_empty`, a text constant's emptiness (the `= ''` / `<> ''` forms), which both keys hold (NEW-72 entry C: exec front.rs and lower key.rs hash it), never a bake"),
    ("decompose/src/catalog/mod.rs", 1, "by value: the fingerprint's by-value arm (the by-reference arms read `bake::read` / `const_bytes`)"),
    ("decompose/src/ops/mod.rs", 1, "by value: a LIMIT bound (`constbyval && constlen == 8`)"),
    ("decompose/src/normalize/mod.rs", 9, "by value: `retype` converts a by-value constant to the column type, `like_twin` compares a by-value int4 (left / right length, strpos result) to decide the rewrite; each rewrite is a new Const / node (the content key hashes it; the front key files a retyped constant as its plan constant converted per execution and a twin's pattern as its needle's, whose image `like_twin` logs, bake `Derived`), and the unrewritten form stays valid for any constant, no bake; `retype_class` / `twin_class` (by value: left / right's length, the strpos compare's constant): whether the rewrite fires, a class both keys hold (exec front.rs puts it, the content key hashes the rewritten form), never a bake; `coalesce_cmp` (by value through ops/shared/scan.rs `zone_word`: a COALESCE's constant against the comparison's, two words): whether `coalesce_filter` drops the COALESCE, a class both keys hold (exec front.rs puts `coalesce_filter_class`), never a bake; `implied` (SRC-07, 3 reads: its match, its class and the conversion) reads a by-value timestamptz constant (`constisnull` / type filtered), converts it per execution into a new Const filed as `Derived::Retype` with `IMPLIED`, and `implied_class_of` is a class both keys hold, never a bake; `identity_shape` (D3078, 1 read) reads a by-value integer constant to test it is its operator's unit: `identity` drops the operator and the constant (no new Const), `identity_class` is a class both keys hold (exec front.rs puts it), never a bake"),
    ("decompose/src/ops/shared/scan.rs", 1, "by value: `zone_word` (`constisnull || !constbyval` returns first): a constant's word in its zone order for scan `const_i64`, normalize `coalesce_cmp` and lowerv4 verdict_zone's integer arms (NEW-47; the text arm reads `const_bytes`)"),
    ("decompose/src/effects/read.rs", 1, "the keys: `plan_word`, a by-value constant's unlogged word for the classes both keys hold (`divisor_class`, `text_arg_class`: lower key.rs and exec front.rs, D1103; `numeric_typmod_class`, PS4-21), no bake"),
    ("exec/src/admit/mod.rs", 3, "the words: a literal's datum / an array literal's elements bound per execution (`GraphWord::Const`), and a hashed list's image handed to lowerv4's `bind_set` (V4R-122: a set word built per execution), no bake"),
    ("exec/src/admit/entry/current_of.rs", 2, "no read: R5 builds two plan Consts (`no_identity`: a NULL tid and tableoid 0, by value, the identity columns of an Append arm that reads no capturable scan) and names the field only to fill it; nothing reads a constant's value, no bake"),
    ("exec/src/inputs/front.rs", 10, "the front key and its variants: hashing / comparing an image keys it (the image and a list's count are read in raw.rs), no bake; a constant decompose built, rebuilt on a hit from its plan constant (`Built::node`: a retyped one converted, a copy's datum) and the shadow check's compare of it (`node_matches`), words bound per execution; `derived_at` reads a built constant's image address (to refuse a baked one) and a `Fixed` one's by-value value, which the front key's plan fields decide; no bake"),
    ("exec/src/inputs/raw.rs", 2, "the front key's raw reads: `image`, a non-null by-reference constant's image the front key hashes or a variant compares (V4R-18), and `list_len`, a constant list's count, keyed by its class (V4R-110); a key is no bake"),
    ("exec/src/ops/modifytable/proto.rs", 1, "write.literals: each execution's Const cells read into the literal table, words bound per execution (never lowered into the program), no bake"),
    ("exec/src/runner/point/keep.rs", 2, "no program (pglpoint S3b): an UPDATE's SET constants are read per execution into the kept compile's PARAM_EXEC slots (as a parked executor's reuse binds its parameters again) and cleared after the statement; nothing is lowered, cached or keyed by a constant's value, no bake; the unit tests build an untyped NULL literal (D4824: a struct literal names the field), no read"),
    ("exec/src/runner/point/probe.rs", 1, "no program (pglpoint S1): the probe reads each key Const per execution into its scan key (ExecIndexBuildScanKeys' sk_argument); nothing is lowered, cached or keyed by it, no bake"),
    ("exec/src/runner/point/select.rs", 1, "no program (pglpoint S1): a target-list Const's datum is read per execution into the DataRow; nothing is lowered, cached or keyed by it, no bake"),
    ("exec/src/runner/point/skip.rs", 1, "no read: the unit tests build a by-value int4 / int8 literal (a struct literal names the field); the planner skip puts each statement's own folded expressions into a reused plan and compares statements but for their constants, so nothing is lowered, cached or keyed by a constant's value, no bake"),
    ("expr/src/percode.rs", 2, "the keys: `textpos_class`, a strpos compare's by-value int4 constant read for its class (E2: the needle occurs, does not, neither), which both keys hold and `text_search` bakes; `sized_class`, a size reader's compare constant read for its class (D3374: the reader's `> 0`, or not), which both keys hold; no bake"),
    ("expr/src/nodes/mod.rs", 1, "the IR's compile: the value moves into `ConstNode`, whose reads log (nodes/konst.rs)"),
    ("lower/src/inputs/key.rs", 16, "the content key: hashing an image keys it, a list's count and address its class (V4R-110), a pgrc zone comparand bound per execution by its address, the key holding its class (`bound_refs`, NEW-72 entry B: its push and its match; entry C: a code lane's text comparand, `bound_text_refs`), a literal table's cell address its class (M4-INSERT), a generate_series step's by-value constant its class (`series_step`, D1108; lowerv4 spool_quiet.rs decides by it), a pgrc range filter's zone comparison constant hashed by value (`zone_constants`, D2646; lowerv4 verdict_zone bakes it), no bake; an integer modulus's or division's divisor class and a substr length's or left / right n's class are read through the effects home (`divisor_class`, `text_arg_class` over `plan_word`), a strpos compare's class through gravity_expr percode `textpos_class` (E2); a literal guard's word (`lit_word`: `lit_decide` files the decision on it as a guard of the program, `lits_hold` / `guards_hold` decide again on a hit's literal; D2847 / D2848's general form), no bake; `lit_share` (D3642): a literal's share identity, its by-value word (`word_canon`) or its image (raw.rs `const_image`), which both keys hold as the alias pattern of a literal's copies (a set's arms; aliased positions hold equal values, `alias_ids`), no bake; the test module's literal at a location (a struct literal names the field), no read"),
    ("lower/src/inputs/raw.rs", 1, "the content key's raw read: `const_image`, a non-null by-reference literal's image for `lit_share`'s identity (D3642), which both keys compare to alias a literal's copies, no bake"),
    ("lower/src/rerun/words.rs", 2, "by value: `key`'s by-value / null arm (`constbyval || constisnull`)"),
    ("lowerv4/src/ops/agg/trans/strategies/finals_cse.rs", 2, "a word bound per execution at its node (`need_at`); a folded one reads `bake::read`"),
    ("lowerv4/src/ops/agg/numeric/strategies/num_tree.rs", 1, "by value: a typmod cast's by-value int4 typmod (PS4-21, `constbyval` filter), the Cast node's precision and scale; exec front.rs keys it by value (a cast's typmod, `numeric_typmod_cast`)"),
    ("lowerv4/src/ops/agg/premise.rs", 1, "by value: num_tree.rs `typmod_cast`'s mirror (`num_cast`, `constbyval` filter); exec front.rs keys the typmod by value"),
    ("lowerv4/src/ops/agg/numeric/strategies/num_const.rs", 2, "by value: the int2 / int4 / int8 arms; the numeric arm reads `const_bytes` (typesem::numeric, D2356), which logs the image"),
    ("lowerv4/src/frame/px.rs", 3, "by value: `claim_const` (`constbyval || constisnull` filter); `claim_ref_const` and `offer_ref_const` (NEW-72: an IN list's array for verdict_zone.rs `list_word`, exec's prepare walks it, verdicts.rs `list_preds`): a by-reference literal's pointer handed on as a word bound per execution at its node (`need_at`), no bake"),
    ("lowerv4/src/ops/scan/pgrc/lower.rs", 1, "`tid_bounds`: a `ctid = ANY (a)` array literal's pointer handed on as a word bound per execution at its node (`need_at`; the host reads the array at the seek), no bake"),
    ("lowerv4/src/step/consts.rs", 1, "no read: the tests build a literal at a location (a struct literal names the field; D3642's copies of one literal)"),
    ("lowerv4/src/step/raw/mod.rs", 1, "`const_list_len` for consts.rs: an IN list's element count (gravity_expr konst.rs `list_len`), keyed by its class (V4R-110), no bake"),
    ("lowerv4/src/ops/shared/dummy.rs", 1, "by value: a bool qual (a dummy Result's one-time filter; ops/modifytable delete.rs's and update.rs's, D717)"),
    ("lowerv4/src/rerun/spool/upsert.rs", 1, "by value: `sequence`, a serial default's `nextval(regclass)` argument (`constisnull` filter): its sequence's `seq_cycles` fact decides D3752's DO NOTHING refusal only (PICKS_NOTHING); the argument is the target column's default, fixed by the target relation the key holds, no bake"),
    ("decompose/src/ops/shared/gate.rs", 1, "by value: a bool qual (`never`: a one-time filter, the planner's dummy Result under a MERGE and a recursive term it proved empty, lowerv4 shared/dummy.rs's test, D3367)"),
    ("decompose/src/ops/window.rs", 9, "by value: a top row_number() run condition's int2 / int4 / int8 constant (`constbyval` filter) sets the Sort top's row count below the window (`top_rows`), a Bound::Const the front key hashes by value (lower inputs/key.rs `topk_bound`); `partitions_never_empty`'s run-condition constant over row_number / rank / dense_rank (`constbyval` filter) decides whether a LIMIT's stop reaches the sort below the window (facts/stream.rs group_stop), only in forms whose constant passes every partition's first row or none (`<`, `<=`), so the key's literal word changes no answer; `first_rows_only`'s int2 / int4 / int8 constant of a `rn = 1` qual over row_number / rank / dense_rank (`constbyval` filter, three reads) decides whether a LIMIT's stop counted by prefix groups reaches the sort below the window (facts/stream.rs window_stop's `one`), and lowerv4 ops/sort/incremental.rs files that decision as a literal guard of the program (lower inputs/key.rs `lit_decide`), so a statement of the same key with another constant lowers again, no bake"),
];

/// What a by-value Const read through `bake::read` means for the program cache (D1102, D1099's audit; main 10:12Z). The
/// accessor logs a by-reference image only (decompose bake.rs `read`): a by-value word it returns is logged nowhere, so a
/// lowering that decides the program on it must have that decision keyed some other way. Every class but `ByRef` is a
/// by-value read, and says how it stays sound.
#[derive(Debug)]
enum ByValue {
    /// the call is reached with a by-reference Const only (a `constbyval` / `constlen` / type guard first): the log holds it
    ByRef,
    /// the by-value word decides the program, and both keys hold what it decides (its class, never the literal)
    Keyed,
    /// the word picks a faster form only; the value itself is computed per row as before, so any literal is right
    SpeedOnly,
    /// no plan PostgreSQL makes reaches the by-value arm (the planner folds the node first)
    Unreachable,
    /// only under `Place::Fold`, which admit refuses (the word budget overflowed): no program files it
    NeverFiled,
    /// a column's reported typmod only, never a value; the front key holds it (exec front.rs `Class::Key`)
    Metadata,
    /// read at EXECUTE from the live plan's node into a host's per-execution state; no program holds it, so a cached
    /// program over another literal binds that literal
    ExecuteBound,
}

/// (file under crates/gravity, `bake::read` calls in code, the by-value class, why). A new call fails until it is listed
/// with its class; a removed one fails until the count comes down (a ratchet, as `READS`).
const DECIDES: &[(&str, usize, ByValue, &str)] = &[
    ("decompose/src/expr.rs", 1, ByValue::ByRef, "`const_bytes`: returns on `constbyval` / `constlen != -1` first"),
    ("decompose/src/raw.rs", 1, ByValue::ByRef, "P5-13 `const_fixed_image` (expr.rs `const_fixed_bytes`): returns on `constbyval` / a `constlen` other than the fixed image's first"),
    ("decompose/src/catalog/mod.rs", 2, ByValue::ByRef, "the fingerprint's cstring and fixed-length image arms (its by-value arm hashes the word)"),
    ("decompose/src/admit.rs", 1, ByValue::Keyed, "RUNS-16 `fold`: an integer Const inside a LIMIT / OFFSET expression over extern parameters (`$1 + 1`) makes the execution's bound; the fingerprint hashes a by-value Const's word (catalog/mod.rs) and the parameters it reads are taken facts (`folded_hold`)"),
    ("decompose/src/effects/read.rs", 1, ByValue::Keyed, "`quiet_args` over `divisor_class` / `text_arg_class`: an integer modulus's divisor is nonzero, a division's neither 0 nor -1, so the call is quiet (decompose's F5 and lowerv4 effects.rs read it); a substr length >= 0, a left / right n other than INT32_MIN (`types_core::quiet::text_arg_quiet`); both keys hold the two classes (the same functions over `plan_word`, D1103)"),
    ("hosts/src/ops/scan/index_source.rs", 1, ByValue::ByRef, "a search array's image"),
    ("hosts/src/ops/agg/trans/hosted_bind.rs", 1, ByValue::ExecuteBound, "an ordered-set aggregate's Const direct argument, read by `AggArena::new` (EXECUTE) into the arena's spec for its finalfn (V4R-425)"),
    ("lowerv4/src/table/face.rs", 1, ByValue::SpeedOnly, "`range_key`: a positive integer divisor or an integer shift constant picks dense.range's table and its domain map; the key is computed per row (G3, main 10:17Z; probed c <= 0 after c > 0: ok)"),
    ("lowerv4/src/ops/agg/trans/strategies/finals_cse.rs", 1, ByValue::NeverFiled, "`place`: a folded member literal's value, only past the word budget"),
    ("lowerv4/src/ops/agg/premise.rs", 2, ByValue::Unreachable, "`num_const`'s int2 / int4 / int8 arms: the planner folds int -> numeric over a Const into a numeric Const; its numeric arm reads `const_bytes` (typesem::numeric, D2356)"),
    ("lowerv4/src/step/consts.rs", 1, ByValue::ByRef, "an IN list's image walked for its count"),
    ("lowerv4/src/ops/scan/pgrc/strategies/jsonb_lanes.rs", 1, ByValue::ByRef, "`text_elems`: a `#>>` path's text[] image (`constbyval` returns first)"),
];

/// (file under crates/gravity, `.read()` + `.fields()` calls in code, the by-value class, why) for every file whose code
/// names `ConstNode` or `ExprNode::Const` (D1099 P4): a `ConstNode::read` of a by-value word is logged nowhere (gravity_expr
/// nodes/konst.rs), so it must be keyed some other way. A new call fails until it is listed with its class; a removed one
/// fails until the count comes down (a ratchet, as `DECIDES`).
const IR_READS: &[(&str, usize, ByValue, &str)] = &[
    ("expr/src/emit/jsonb.rs", 1, ByValue::ByRef, "a jsonb path key's text (`typlen != -1` returns first)"),
    ("expr/src/eval.rs", 1, ByValue::Keyed, "an interpreter island's Const: both keys hash an island's constants by value (D337, `island_capable`)"),
    ("expr/src/nodes/konst.rs", 1, ByValue::Keyed, "`fields`: the percode key's walk (D341; its one caller is percode.rs)"),
    ("expr/src/nodes/mod.rs", 1, ByValue::ByRef, "guarded `!c.byval && !c.is_null()`"),
    ("expr/src/percode.rs", 4, ByValue::Keyed, "`fields` and `put_image`'s word arm write the word into the percode key (D1100); `with_const_bytes`'s read feeds by-reference callers (text, numeric, interval) and `put_image`; `text_search`'s int4 compare word: both keys hold its class (lower key.rs `textpos_class`, E2); NEW-72 entry C: `code_set` and `code_range` bind their text constants per execution, never read here"),
    ("lowerv4/src/hir.rs", 1, ByValue::ByRef, "B2 `catalog_site`: a conversion body's constant encoding name (a `name` Const, by reference: `k.typ == NAMEOID` first); its image picks the claim's armed conversion pair"),
    ("lowerv4/src/ops/scan/pgrc/raw.rs", 2, ByValue::ByRef, "`const_inline`: a jsonb key's image (`k.byval` returns first; the caller wants a by-reference key); `const_text_elems`: a jsonb path's text[] image (STEP-12, `k.byval` returns first)"),
    ("lowerv4/src/step/raw/mod.rs", 2, ByValue::ByRef, "`const_inline_bytes` for text_like.rs `const_bytes`: the value is used only under `!byval && typlen == -1`"),
];

/// (file, `.need()` + `.list()` calls in code, why) in the same files: the word plan's reads, never a bake. A `need` is bound
/// per execution from the plan node; a list's count keys it by its class (V4R-110). Same ratchet.
const IR_WORDS: &[(&str, usize, &str)] = &[
    ("expr/src/emit/mod.rs", 3, "`ParamNeed::Const` (`need`), an `= ANY` list's per-element needs (`list`), and a list past MAX_LIST's whole-array word (`need`, P5-02)"),
    ("expr/src/nodes/konst.rs", 1, "`list_count`: the element count, keyed by the list's class (V4R-110)"),
];

fn gravity() -> PathBuf {
    gravity_lint::gravity_root()
}

/// The text with `//` comments and string literal bodies blanked.
fn code(text: &str) -> String {
    let (mut out, mut it) = (String::with_capacity(text.len()), text.chars().peekable());
    while let Some(c) = it.next() {
        match c {
            '/' if it.peek() == Some(&'/') => {
                for d in it.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '"' => {
                out.push('"');
                while let Some(d) = it.next() {
                    match d {
                        '\\' => drop(it.next()),
                        '"' => break,
                        '\n' => out.push('\n'),
                        _ => {}
                    }
                }
                out.push('"');
            }
            _ => out.push(c),
        }
    }
    out
}

fn reads(text: &str) -> usize {
    let t = code(text);
    t.match_indices("constvalue").filter(|(i, _)| {
        let before = t[..*i].chars().next_back();
        let after = t[i + "constvalue".len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    }).count()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// A cross-crate source lint in another crate's tests/ runs in no gate (see the header): `*_lint.rs` lives here.
#[test]
fn source_lints_live_in_gravity_lint() {
    let root = gravity();
    let mut stray = Vec::new();
    for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let (krate, tests) = (e.file_name().to_string_lossy().into_owned(), e.path().join("tests"));
        if krate == "lint" {
            continue;
        }
        for t in std::fs::read_dir(&tests).into_iter().flatten().flatten() {
            if t.file_name().to_string_lossy().ends_with("_lint.rs") {
                stray.push(format!("{krate}/tests/{}", t.file_name().to_string_lossy()));
            }
        }
    }
    assert!(stray.is_empty(), "move into crates/gravity/lint/tests (push-check runs gravity_lint on every landing): {stray:?}");
}

/// D1102: every `bake::read` call is listed with what a by-value Const read there means for the program cache.
#[test]
fn every_bake_read_is_listed_with_its_by_value_class() {
    let root = gravity();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for krate in ["decompose", "lower", "lowerv4", "hosts", "expr", "exec"] {
        let mut files = Vec::new();
        rs_files(&root.join(krate).join("src"), &mut files);
        for f in files {
            let t = code(&std::fs::read_to_string(&f).unwrap());
            let n = t.match_indices("bake::read(").filter(|(i, _)| !t[..*i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')).count();
            if n > 0 {
                found.insert(f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"), n);
            }
        }
    }
    let listed: BTreeMap<String, (usize, &ByValue)> = DECIDES.iter().map(|(f, n, c, _)| (f.to_string(), (*n, c))).collect();
    let mut bad = Vec::new();
    for (f, n) in found.iter() {
        match listed.get(f) {
            None => bad.push(format!("{f}: {n} `bake::read` call(s) not listed in DECIDES: a by-value Const read there is logged nowhere, so list its class (keyed, speed only, ...) and why")),
            Some((m, _)) if m != n => bad.push(format!("{f}: {n} `bake::read` calls, {m} listed (a new call needs its class; a removed one lowers the count)")),
            _ => {}
        }
    }
    for (f, (m, c)) in listed.iter() {
        if !found.contains_key(f) {
            bad.push(format!("{f}: listed with {m} calls ({c:?}), has none (remove the row)"));
        }
    }
    assert!(bad.is_empty(), "{} bake-read findings:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn every_plan_const_value_read_is_the_bake_accessor_or_listed() {
    let root = gravity();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for krate in ["decompose", "lower", "lowerv4", "hosts", "expr", "exec"] {
        let mut files = Vec::new();
        rs_files(&root.join(krate).join("src"), &mut files);
        for f in files {
            let n = reads(&std::fs::read_to_string(&f).unwrap());
            if n > 0 {
                found.insert(f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"), n);
            }
        }
    }
    let listed: BTreeMap<String, usize> = READS.iter().map(|(f, n, _)| (f.to_string(), *n)).collect();
    let mut bad = Vec::new();
    for (f, n) in found.iter() {
        match listed.get(f) {
            None => bad.push(format!("{f}: {n} `constvalue` read(s) not listed: read a by-reference image through gravity_decompose::bake::read, or list the file with why no read bakes")),
            Some(m) if m != n => bad.push(format!("{f}: {n} `constvalue` reads, {m} listed (a new read needs its reason; a removed one lowers the count)")),
            _ => {}
        }
    }
    for (f, m) in listed.iter() {
        if !found.contains_key(f) {
            bad.push(format!("{f}: listed with {m} reads, has none (remove the row)"));
        }
    }
    assert!(bad.is_empty(), "{} bake-lint findings:\n{}", bad.len(), bad.join("\n"));
}

/// Per file under every gravity crate's src whose code names `ConstNode` or `ExprNode::Const`: its `.read()` +
/// `.fields()` calls and its `.need()` + `.list()` calls.
fn ir_calls(root: &Path) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let (mut reads, mut words) = (BTreeMap::new(), BTreeMap::new());
    let mut files = Vec::new();
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        rs_files(&e.path().join("src"), &mut files);
    }
    for f in files {
        let t = code(&std::fs::read_to_string(&f).unwrap());
        if !t.contains("ConstNode") && !t.contains("ExprNode::Const") {
            continue;
        }
        let name = f.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let (r, w) = (t.matches(".read()").count() + t.matches(".fields()").count(), t.matches(".need()").count() + t.matches(".list()").count());
        if r > 0 {
            reads.insert(name.clone(), r);
        }
        if w > 0 {
            words.insert(name, w);
        }
    }
    (reads, words)
}

/// D1099 P4: every IR Const value read is listed with its by-value class, and every word-plan read with why.
#[test]
fn every_ir_const_read_is_listed_with_its_by_value_class() {
    let (reads, words) = ir_calls(&gravity());
    let mut bad = Vec::new();
    let mut check = |what: &str, found: &BTreeMap<String, usize>, listed: BTreeMap<String, usize>| {
        for (f, n) in found.iter() {
            match listed.get(f) {
                None => bad.push(format!("{f}: {n} {what} call(s) not listed: a by-value word read there is logged nowhere, so list its class and why")),
                Some(m) if m != n => bad.push(format!("{f}: {n} {what} calls, {m} listed (a new call needs its class; a removed one lowers the count)")),
                _ => {}
            }
        }
        for (f, m) in listed.iter() {
            if !found.contains_key(f) {
                bad.push(format!("{f}: listed with {m} {what} calls, has none (remove the row)"));
            }
        }
    };
    check("`.read()` / `.fields()`", &reads, IR_READS.iter().map(|(f, n, _, _)| (f.to_string(), *n)).collect());
    check("`.need()` / `.list()`", &words, IR_WORDS.iter().map(|(f, n, _)| (f.to_string(), *n)).collect());
    assert!(bad.is_empty(), "{} IR Const read findings:\n{}", bad.len(), bad.join("\n"));
}
