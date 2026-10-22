use gravity_lint::*;

#[test]
fn allocation_sites_carry_lifetime_tags() {
    let mut failures = Vec::new();
    for name in NO_ALLOC_CRATES {
        for site in alloc_sites(&gravity_root().join(name)) {
            failures.push(format!("{}:{}: `{}` without `// alloc: <{}>`\n    {}", rel(&site.path), site.line_no, site.pattern, ALLOC_LIFETIMES.join("|"), site.line));
        }
    }
    assert!(failures.is_empty(), "untagged allocation sites (docs/gravity/lint.md):\n{}", failures.join("\n"));
}

#[test]
fn alloc_tag_grammar() {
    assert!(alloc_tag_ok("let v = Vec::new(); // alloc: Program"));
    assert!(alloc_tag_ok("Box::new(x) // alloc:Cold"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Hot"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Exec, per row"));
    assert!(!alloc_tag_ok("let v = Vec::new();"));
}

#[test]
fn alloc_tag_charged_marker() {
    // wave 2 R5: `// alloc: <lifetime> charged <owner expr>`, lifetime first
    assert!(alloc_tag_ok("let v = Vec::new(); // alloc: Exec charged stmt_owner"));
    assert!(alloc_tag_ok("buf.extend_from_slice(b); let v = vec![0u8; n]; // alloc: Node charged w.op_owner().unwrap().0"));
    assert!(alloc_tag_ok("Box::new(x) // alloc:Cold  charged  owner"));
    assert_eq!(alloc_charged_owner("let v = Vec::new(); // alloc: Exec charged w.stmt_owner()"), Some("w.stmt_owner()"));
    assert_eq!(alloc_charged_owner("let v = Vec::new(); // alloc: Program"), None);
    assert_eq!(alloc_charged_owner("let v = Vec::new();"), None);
    // the marker alone, an empty owner, another word after the lifetime, an unknown lifetime: not a tag
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: charged stmt_owner"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Exec charged"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Exec charged   "));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Exec chargedowner"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Exec per row"));
    assert!(!alloc_tag_ok("let v = Vec::new(); // alloc: Hot charged owner"));
}

#[test]
fn alloc_patterns_are_code_tokens() {
    // TD-197 LINT-FP (memacct 2026-10-15T20:4xZ): a charged or context constructor whose name ends in `Vec` is no std
    // `Vec::new(`, and a pattern in a comment or a literal names no allocation
    for code in ["let v = ChargedVec::new(o);", "let v = PgVec::new(mcx);", "let v = ArenaVec::new_in(a);", "let b = MyBox::new(x);"] {
        assert_eq!(alloc_pattern(&code_only(code)), None, "{code}");
    }
    for (code, p) in [("let v = Vec::new();", "Vec::new("), ("let v = std::vec::Vec::new();", "Vec::new("), ("x(Box::new(y))", "Box::new("),
                      ("let s = alloc::format!(\"{a}\");", "format!("), ("let s = n.to_string();", ".to_string()"), ("let v = vec![0; n];", "vec![")] {
        assert_eq!(alloc_pattern(&code_only(code)), Some(p), "{code}");
    }
    let text = "/// builds a Vec::new( lazily\nlet a = 1; // then Box::new(x)\n/* format!( in a\n block, vec![ */ let z = 2;\nlet m = \"Vec::new(\";\nlet r = r#\"vec![\"#;\nlet c = '\"'; let v = Vec::new(); // alloc: Exec\n";
    let code = code_only(text);
    assert_eq!(code.lines().count(), text.lines().count());
    let hits: Vec<_> = code.lines().enumerate().filter_map(|(i, l)| alloc_pattern(l).map(|p| (i + 1, p))).collect();
    assert_eq!(hits, vec![(7, "Vec::new(")], "{code}");
    // the tag is read from the raw line, as before
    assert!(alloc_tag_ok(text.lines().nth(6).unwrap()));
    // an escaped quote char literal closes where it should
    assert_eq!(alloc_pattern(code_only("let q = '\\''; let s = \"a\"; let v = Vec::new();").as_str()), Some("Vec::new("));
}
