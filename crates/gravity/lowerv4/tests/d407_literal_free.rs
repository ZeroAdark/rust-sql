//! D407 (main 09-24): a literal the front key leaves out is a word, never IR. Every non-null Const of every fixture of the
//! decompose corpus and the milestone fixtures that lowers is perturbed in the plan text to another valid value of its
//! type (by value: the low bit of its first byte, as tests/front_census.rs; text / varchar / bpchar / name / bytea: one
//! letter or digit; numeric: one base-10000 group, its trailing zeros kept). Where the front key still says "the same
//! shape" (`main` equal, and `tl` / `zone` where read: the Const is a word, or a by-reference image the key leaves out), both plans must lower to the
//! same program (`front::ir_hash`): a literal baked into IR is one program per literal, V4R-18's variant cascade. The one
//! exception is a by-reference literal the admission logged as read (gravity_expr / gravity_decompose `bake`: a
//! date_part unit, a LIKE pattern): the front key files it into its variant's key, so another program is that variant's.
//! V4R-110: an array (an `IN` / `= ANY` list) is perturbed too (one element's value, its count kept), and has no such
//! exception: a list is never baked, and where the content key keys it by its class (lower key.rs `class_keyed_lists`)
//! the content key is the same too, one program for every list of the class.
use gravity_lowerv4_it as common;

use gravity_decompose::corpus;
use gravity_exec::inputs::front;
use gravity_lower::inputs::key;
use gravity_program::Arena;
use mcx::MemoryContext;

/// Every non-null Const in the plan text: its type (`BYVAL` for a by-value one) and the byte span of its `[ .. ]` values.
fn consts(text: &str) -> Vec<(u32, usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find("{CONST ") {
        let at = from + i;
        let end = at + text[at..].find('}').unwrap();
        let c = &text[at..end];
        from = end;
        // D487 (as tests/front_census.rs): an `-- idxexprs` facts line is an index's own expression (the catalog): the
        // front key carries the relcache epoch, which moves when an index expression does, so its Consts are not words
        let line = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line..at].starts_with("-- idxexprs ") {
            continue;
        }
        let byval = c.contains(":constbyval true :constisnull false");
        if !byval && !c.contains(":constbyval false :constisnull false") {
            continue;
        }
        let ty: u32 = c[c.find(":consttype ").unwrap() + 11..].split(' ').next().unwrap().parse().unwrap();
        let b = at + c.find('[').unwrap() + 1;
        out.push((if byval { BYVAL } else { ty }, b, end - 1));
    }
    out
}

const BYVAL: u32 = 0;

/// Another valid image of the same type and length, or `None` for a type the rule does not perturb.
fn perturb(ty: u32, v: &mut [u32]) -> Option<()> {
    let step = |b: &mut u32| match *b as u8 {
        b'a'..=b'y' | b'A'..=b'Y' | b'0'..=b'8' => Some(*b += 1),
        b'z' | b'Z' | b'9' => Some(*b -= 1),
        _ => None,
    };
    match ty {
        _ if ARRAYS.contains(&ty) => perturb_array(ty, v),
        BYVAL => Some(v[0] ^= 1),
        25 | 1043 | 1042 if v.len() > 4 => v[4..].iter_mut().find_map(step),
        19 => v.iter_mut().find_map(step),
        17 if v.len() > 4 => Some(v[4] ^= 1),
        1700 if v.len() > 6 => {
            let h = v[4] | v[5] << 8;
            let first = match h & 0xC000 {
                0x8000 => 6,
                0x0000 | 0x4000 if v.len() > 8 => 8,
                _ => return None,
            };
            let g = v[first] | v[first + 1] << 8;
            let mut s = 1;
            while g % (s * 10) == 0 && s < 1000 {
                s *= 10;
            }
            let n = if g + s < 10000 { g + s } else { g - s };
            (v[first], v[first + 1]) = (n & 0xFF, n >> 8);
            Some(())
        }
        _ => None,
    }
}

/// The array types perturbed (V4R-110): int2[] int4[] int8[] oid[] text[] varchar[] bpchar[] name[].
const ARRAYS: &[u32] = &[1005, 1007, 1016, 1028, 1009, 1015, 1014, 1003];

/// Another list of the same count: its first element's value changed (an integer's low bit, a string's first letter or
/// digit), a plain one-dimensional image without NULLs only.
fn perturb_array(ty: u32, v: &mut [u32]) -> Option<()> {
    let at = |i: usize| v.get(i..i + 4).map(|b| b[0] | b[1] << 8 | b[2] << 16 | b[3] << 24);
    if v.first()? & 3 != 0 || at(4)? != 1 || at(8)? != 0 || at(16)? == 0 {
        return None;
    }
    let data = 24;
    match ty {
        1005 | 1007 | 1016 | 1028 => Some(*v.get_mut(data)? ^= 1),
        _ => {
            // the first element's payload: after its 1-byte (short) or 4-byte header
            let (from, len) = if v.get(data)? & 1 == 1 { (data + 1, (v[data] >> 1) as usize - 1) } else { (data + 4, (at(data)? >> 2) as usize - 4) };
            v.get_mut(from..from + len)?.iter_mut().find_map(|b| match *b as u8 {
                b'a'..=b'y' | b'A'..=b'Y' | b'0'..=b'8' => Some(*b += 1),
                b'z' | b'Z' | b'9' => Some(*b -= 1),
                _ => None,
            })
        }
    }
}

/// The content key of the fixture's graph, and the images of the lists it keys by their class.
fn content_of(text: &str) -> Option<(u128, Vec<Vec<u8>>)> {
    let (ctx, arena) = (MemoryContext::new("d407 content"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    let lists = gravity_lower::inputs::key::class_keyed_lists(&graph).into_iter().map(|a| {
        // SAFETY: a class-keyed list is a non-null by-reference array image of the loaded plan (4-byte or short header).
        unsafe {
            let p = a as *const u8;
            let len = if *p & 1 == 1 { (*p >> 1) as usize } else { (u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize };
            core::slice::from_raw_parts(p, len).to_vec()
        }
    });
    Some((gravity_lower::inputs::key::key(&graph).0, lists.collect()))
}

/// The front key: `main`, and `tl` / `zone` where `front::needs` says the plan reads them (tests/front_census.rs's
/// rule), or `None` when the walk does not key the plan.
fn front_of(text: &str) -> Option<(u128, Option<u128>, Option<u128>)> {
    let ctx = MemoryContext::new("d407 front");
    let fx = corpus::load(ctx.mcx(), text).ok()?;
    let arena = Arena::new(1 << 22);
    let f = front::walk(arena.handle(), fx.stmt, None)?;
    let needs = front::needs(&f, fx.stmt, &fx.facts);
    Some((f.main, needs.0.then_some(f.tl), needs.1.then_some(f.zone)))
}

/// Statements derived from a fixture (or an earlier derived one) by one text substitution, where the corpus has no
/// statement of a claimed form with a literal: `(name, from fixture, text, replacement)`. num.arith (a projected numeric `*` / `+` with a constant) from
/// bench-126's `n * m`: `n * 1.5`, `1.5 * m`, `n + 1.5`.
const DERIVED: &[(&str, &str, &str, &str)] = &[
    ("bench-126-n-mul-k", "bench-126", M_VAR, K_15),
    ("bench-126-k-mul-m", "bench-126", N_VAR, K_15),
    ("bench-126-n-add-k", "bench-126-n-mul-k", ":opno 1760 :opfuncid 1726", ":opno 1758 :opfuncid 1724"),
];
const N_VAR: &str = "{VAR :varno 1 :varattno 3 :vartype 1700 :vartypmod 655366 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 3 :location 31}";
const M_VAR: &str = "{VAR :varno 1 :varattno 4 :vartype 1700 :vartypmod 393223 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 1 :varattnosyn 4 :location 35}";
const K_15: &str = "{CONST :consttype 1700 :consttypmod -1 :constcollid 0 :constlen -1 :constbyval false :constisnull false :location 35 :constvalue 10 [ 40 0 0 0 128 128 1 0 136 19 ]}";

/// Sites whose fix another lane owns `(fixture, owner)`: kept out of the verdict until then, and the test fails once one
/// passes, so its row goes with the fix.
/// (D531's numeric-key row went with batch B of M6-REPS: a perturbed unit's numeric key is rep.varlena.hash's Fp128.)
/// (D1326's TPC-H Q7 / Q8 / Q9 rows went with M6-CLOSE V4R-490: a perturbed unit's computed numeric key over the packed
/// lanes keys by rep.packed.order's word, no longer the NotBuilt rep.packed.hash.)
const OWED: &[(&str, &str)] = &[];

/// The program of `text`'s lowering, and the outcomes of the literal guards it files (lower key.rs `lit_decide`).
fn ir_of(text: &str) -> Option<(u128, Vec<([u64; 4], u64)>)> {
    let (l, guards) = key::guarded(|| common::lower_fixture(text));
    l?.ok().map(|l| (front::ir_hash(&l), decided(&guards)))
}

/// NEW-72 / D2848: the outcomes of a lowering's literal guards, in filing order. A front hit takes a filed program only
/// where its literal guards decide alike on the hit's literals (exec admit `lits_hold`), so a literal the front key leaves
/// out may be another program exactly where a guard decides otherwise on it (the tally's zero test, summary.rs).
fn decided(guards: &[key::EstGuard]) -> Vec<([u64; 4], u64)> {
    guards.iter().filter(|g| key::literal(g)).map(|g| (g.args, g.want)).collect()
}

#[test]
fn a_literal_the_front_key_leaves_out_is_not_in_the_program() {
    let mut fixtures = common::files(&common::plans_dir(), "plan");
    fixtures.extend(common::files(&common::plans_dir().join("regress-full"), "plan"));
    for k in 1..=3 {
        fixtures.extend(common::files(&common::milestone_dir(k), "plan").into_iter().map(|(n, t)| (format!("m{k}/{n}"), t)));
    }
    for &(name, base, from, to) in DERIVED {
        let text = &fixtures.iter().find(|f| f.0 == base).unwrap_or_else(|| panic!("{base}: no such fixture")).1;
        assert!(text.contains(from), "{name}: {base} no longer holds the text it is derived from");
        fixtures.push((name.to_owned(), text.replacen(from, to, 1)));
    }
    let mut derived = 0;
    let (mut lowered, mut tried, mut kept, mut baked, mut lists, mut guarded) = (0, 0, 0, 0, 0, 0);
    let mut bad = Vec::new();
    for (name, text) in &fixtures {
        // the lowering first: it installs the rig's catalog seams the front walk reads
        let Some((ir, lits)) = ir_of(text) else { continue };
        let Some(main) = front_of(text) else { continue };
        (lowered, derived) = (lowered + 1, derived + DERIVED.iter().any(|d| d.0 == name) as usize);
        for (ty, b, e) in consts(text) {
            let mut v: Vec<u32> = text[b..e].split_whitespace().map(|x| x.parse().unwrap()).collect();
            if perturb(ty, &mut v).is_none() {
                continue;
            }
            tried += 1;
            let t = format!("{} {} {}", &text[..b], v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "), &text[e..]);
            if front_of(&t) != Some(main) {
                continue;
            }
            kept += 1;
            let img: Vec<u8> = v.iter().map(|&b| b as u8).collect();
            // V4R-110: a list the content key keys by its class keeps the content key too (one filed program), and is
            // never read (a pgrcolumnar graph's is keyed by its image: a per-code table's)
            let classed = ARRAYS.contains(&ty) && content_of(&t).is_some_and(|(k, keyed)| {
                let classed = keyed.contains(&img);
                if classed && Some(k) != content_of(text).map(|c| c.0) {
                    bad.push(format!("{name}: type {ty} list [{}]: another content key", text[b..e].trim()));
                }
                classed
            });
            lists += classed as usize;
            let (r, guards) = key::guarded(|| common::lower_fixture_baked(&t, &img));
            let other = decided(&guards) != lits;
            match r.map(|(l, baked)| (l.ok().map(|l| front::ir_hash(&l)), baked)) {
                Some((Some(p), false)) if p == ir => {}
                // a literal guard decides otherwise on it: the program filed for that outcome (D2848's tally)
                Some((Some(_), false)) if other => guarded += 1,
                Some((Some(p), true)) if p == ir && !classed => {}
                // a literal the admission logged as read is a baked variant's (V4R-18: its image is in the filed key); a
                // class-keyed list never is (V4R-110)
                Some((Some(_), true)) if ty != BYVAL && !classed => baked += 1,
                p => bad.push(format!("{name}: type {} literal [{}]: {}", if ty == BYVAL { "(by value)".to_owned() } else { ty.to_string() }, text[b..e].trim(), match p {
                    Some((Some(_), true)) => "a list baked",
                    Some((Some(_), false)) => "another program",
                    _ => "no longer lowers",
                })),
            }
        }
    }
    eprintln!("--stats d407: {lowered} fixtures lowered and keyed ({derived} derived), {tried} literals perturbed, {kept} kept the front key, {baked} of them baked (logged, a variant each), {lists} of them lists, {guarded} of them a literal guard's other outcome");
    let owes = |l: &String, o: &(&str, &str)| l.starts_with(&format!("{}:", o.0));
    let paid: Vec<_> = OWED.iter().filter(|o| !bad.iter().any(|l| owes(l, o))).collect();
    bad.retain(|l| !OWED.iter().any(|o| owes(l, o)));
    assert!(paid.is_empty(), "owed sites that now pass (drop their OWED rows): {paid:?}");
    assert!(bad.is_empty(), "{} literals the front key leaves out reach the program:\n{}", bad.len(), bad.join("\n"));
    assert_eq!(derived, DERIVED.len(), "every derived statement lowers");
    assert!(kept > 200, "the corpus exercises the rule ({kept} perturbed literals kept the front key)");
    assert!(lists > 0, "the corpus exercises the list rule ({lists} perturbed lists kept the front key)");
}
