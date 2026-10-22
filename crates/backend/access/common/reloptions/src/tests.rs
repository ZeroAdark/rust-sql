use super::*;
use ::mcx::MemoryContext;
use ::types_nodes::{Node, NodeList};

fn def<'mcx>(
    mcx: Mcx<'mcx>,
    ns: Option<&'mcx str>,
    name: &'mcx str,
    arg: Option<&'mcx str>,
) -> Node<'mcx> {
    let arg = arg.map(|v| Node::mk(mcx, ::types_nodes::String { sval: v }).unwrap());
    Node::mk(
        mcx,
        DefElem {
            defnamespace: ns,
            defname: Some(name),
            arg,
            defaction: ::types_nodes::parsenodes::DefElemAction::DEFELEM_UNSPEC,
            location: -1,
        },
    )
    .unwrap()
}

fn texts_of(image: &[u8]) -> Vec<String> {
    let cx = MemoryContext::new("t");
    let out: Vec<String> =
        option_text_strs(cx.mcx(), image).unwrap().iter().map(|s| s.to_string()).collect();
    out
}

#[test]
fn transform_builds_name_value_texts() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let list = NodeList::make3(
        mcx,
        def(mcx, None, "fillfactor", Some("30")),
        def(mcx, None, "autovacuum_enabled", Some("false")),
        def(mcx, Some("toast"), "vacuum_truncate", Some("false")),
    )
    .unwrap();
    let img = transformRelOptions(mcx, None, &list, None, HEAP_RELOPT_NAMESPACES, true, false)
        .unwrap()
        .unwrap();
    assert_eq!(texts_of(&img), ["fillfactor=30", "autovacuum_enabled=false"]);
    let img =
        transformRelOptions(mcx, None, &list, Some("toast"), HEAP_RELOPT_NAMESPACES, true, false)
            .unwrap()
            .unwrap();
    assert_eq!(texts_of(&img), ["vacuum_truncate=false"]);
}

#[test]
fn transform_replaces_and_resets() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let list = NodeList::make1(mcx, def(mcx, None, "fillfactor", Some("30"))).unwrap();
    let old = transformRelOptions(mcx, None, &list, None, HEAP_RELOPT_NAMESPACES, true, false)
        .unwrap()
        .unwrap();
    let list2 = NodeList::make2(
        mcx,
        def(mcx, None, "fillfactor", Some("31")),
        def(mcx, None, "autovacuum_enabled", None),
    )
    .unwrap();
    let img =
        transformRelOptions(mcx, Some(&old), &list2, None, HEAP_RELOPT_NAMESPACES, true, false)
            .unwrap()
            .unwrap();
    assert_eq!(texts_of(&img), ["fillfactor=31", "autovacuum_enabled=true"]);

    let reset = NodeList::make1(mcx, def(mcx, None, "fillfactor", None)).unwrap();
    let img2 =
        transformRelOptions(mcx, Some(&img), &reset, None, HEAP_RELOPT_NAMESPACES, true, true)
            .unwrap()
            .unwrap();
    assert_eq!(texts_of(&img2), ["autovacuum_enabled=true"]);

    let reset_all = NodeList::make1(mcx, def(mcx, None, "autovacuum_enabled", None)).unwrap();
    let none =
        transformRelOptions(mcx, Some(&img2), &reset_all, None, HEAP_RELOPT_NAMESPACES, true, true)
            .unwrap();
    assert!(none.is_none());

    let bad_reset = NodeList::make1(mcx, def(mcx, None, "fillfactor", Some("12"))).unwrap();
    let err = transformRelOptions(mcx, None, &bad_reset, None, HEAP_RELOPT_NAMESPACES, true, true)
        .unwrap_err();
    assert_eq!(err.message(), "RESET must not include values for parameters");
}

#[test]
fn transform_expands_short_header_old_image() {
    // heap_form_tuple stores small arrays with a 1-byte header; the merge
    // paths must see them as C's DatumGetArrayTypeP would.
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let list = NodeList::make2(
        mcx,
        def(mcx, None, "n_distinct", Some("100")),
        def(mcx, None, "n_distinct_inherited", Some("5")),
    )
    .unwrap();
    let old = transformRelOptions(mcx, None, &list, None, &[], false, false).unwrap().unwrap();
    assert!(old.len() - 4 + 1 <= 0x7F, "test image must be short-able");
    let mut short: Vec<u8> = Vec::new();
    short.push((((old.len() - 4 + 1) as u8) << 1) | 0x01);
    short.extend_from_slice(&old[4..]);

    let reset = NodeList::make1(mcx, def(mcx, None, "n_distinct", None)).unwrap();
    let img = transformRelOptions(mcx, Some(&short), &reset, None, &[], false, true)
        .unwrap()
        .unwrap();
    assert_eq!(texts_of(&img), ["n_distinct_inherited=5"]);

    let opts = attribute_reloptions(mcx, Some(&short), true).unwrap().unwrap();
    assert_eq!(opts.n_distinct, 100.0);
    assert_eq!(opts.n_distinct_inherited, 5.0);
}

fn parse_heap_err(mcx: Mcx<'_>, defs: &NodeList<'_>) -> (String, Option<String>) {
    let res = transformRelOptions(mcx, None, defs, None, HEAP_RELOPT_NAMESPACES, true, false)
        .and_then(|img| heap_reloptions(mcx, RELKIND_RELATION, img.as_deref(), true));
    let e = res.unwrap_err();
    let out = (e.message().to_string(), e.detail().map(|d| d.to_string()));
    out
}

#[test]
fn validation_error_texts_match_c() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();

    let l = NodeList::make1(mcx, def(mcx, None, "fillfactor", Some("2"))).unwrap();
    let (m, d) = parse_heap_err(mcx, &l);
    assert_eq!(m, "value 2 out of bounds for option \"fillfactor\"");
    assert_eq!(d.as_deref(), Some("Valid values are between \"10\" and \"100\"."));

    let l = NodeList::make1(
        mcx,
        def(mcx, None, "autovacuum_analyze_scale_factor", Some("110.0")),
    )
    .unwrap();
    let (m, d) = parse_heap_err(mcx, &l);
    assert_eq!(m, "value 110.0 out of bounds for option \"autovacuum_analyze_scale_factor\"");
    assert_eq!(d.as_deref(), Some("Valid values are between \"0.000000\" and \"100.000000\"."));

    let l = NodeList::make1(mcx, def(mcx, None, "not_existing_option", Some("2"))).unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "unrecognized parameter \"not_existing_option\"");

    let l = NodeList::make1(
        mcx,
        def(mcx, Some("not_existing_namespace"), "fillfactor", Some("2")),
    )
    .unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "unrecognized parameter namespace \"not_existing_namespace\"");

    let l = NodeList::make1(mcx, def(mcx, None, "fillfactor", Some("string"))).unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "invalid value for integer option \"fillfactor\": string");

    let l = NodeList::make1(mcx, def(mcx, None, "autovacuum_enabled", Some("12"))).unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "invalid value for boolean option \"autovacuum_enabled\": 12");

    let l = NodeList::make1(
        mcx,
        def(mcx, None, "autovacuum_analyze_scale_factor", Some("string")),
    )
    .unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(
        m,
        "invalid value for floating point option \"autovacuum_analyze_scale_factor\": string"
    );

    let l = NodeList::make2(
        mcx,
        def(mcx, None, "fillfactor", Some("30")),
        def(mcx, None, "fillfactor", Some("40")),
    )
    .unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "parameter \"fillfactor\" specified more than once");

    // Name-only non-boolean: "fillfactor" flattens to fillfactor=true.
    let l = NodeList::make1(mcx, def(mcx, None, "fillfactor", None)).unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "invalid value for integer option \"fillfactor\": true");

    // -30.1 parses via the strtod leg, rint()s to -30, then bounds-fails
    // with the original string in the message.
    let l = NodeList::make1(mcx, def(mcx, None, "fillfactor", Some("-30.1"))).unwrap();
    let (m, _) = parse_heap_err(mcx, &l);
    assert_eq!(m, "value -30.1 out of bounds for option \"fillfactor\"");
}

#[test]
fn heap_parse_fills_struct_and_toast_adjusts() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let l = NodeList::make3(
        mcx,
        def(mcx, None, "fillfactor", Some("30")),
        def(mcx, None, "autovacuum_enabled", Some("false")),
        def(mcx, None, "autovacuum_analyze_scale_factor", Some("0.2")),
    )
    .unwrap();
    let img = transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
        .unwrap()
        .unwrap();
    let o = heap_reloptions(mcx, RELKIND_RELATION, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.fillfactor, 30);
    assert!(!o.autovacuum.enabled);
    assert_eq!(o.autovacuum.analyze_scale_factor, 0.2);
    assert_eq!(o.toast_tuple_target, 2032);
    assert!(o.vacuum_truncate && !o.vacuum_truncate_set);

    let l = NodeList::make1(mcx, def(mcx, None, "autovacuum_vacuum_cost_delay", Some("23")))
        .unwrap();
    let img = transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
        .unwrap()
        .unwrap();
    let o = heap_reloptions(mcx, RELKIND_TOASTVALUE, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.fillfactor, 100);
    assert_eq!(o.autovacuum.vacuum_cost_delay, 23.0);
    assert_eq!(o.autovacuum.analyze_threshold, -1);
    assert_eq!(o.autovacuum.analyze_scale_factor, -1.0);

    let l = NodeList::make1(mcx, def(mcx, None, "vacuum_truncate", Some("false"))).unwrap();
    let img = transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
        .unwrap()
        .unwrap();
    let o = heap_reloptions(mcx, RELKIND_RELATION, Some(&img), true).unwrap().unwrap();
    assert!(!o.vacuum_truncate && o.vacuum_truncate_set);
}

#[test]
fn index_am_parses() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let l = NodeList::make2(
        mcx,
        def(mcx, None, "fillfactor", Some("30")),
        def(mcx, None, "deduplicate_items", Some("off")),
    )
    .unwrap();
    let img = transformRelOptions(mcx, None, &l, None, &[], false, false).unwrap().unwrap();
    let o = index_reloptions(mcx, BTREE_AM_OID, Some(&img), true).unwrap().unwrap();
    let bt = o.btree().unwrap();
    assert_eq!(bt.fillfactor, 30);
    assert!(!bt.deduplicate_items);

    let e = index_reloptions(mcx, HASH_AM_OID, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "unrecognized parameter \"deduplicate_items\"");

    let l = NodeList::make1(mcx, def(mcx, None, "buffering", Some("off"))).unwrap();
    let img = transformRelOptions(mcx, None, &l, None, &[], false, false).unwrap().unwrap();
    let o = index_reloptions(mcx, GIST_AM_OID, Some(&img), true).unwrap().unwrap();
    assert_eq!(
        o.gist().unwrap().buffering_mode,
        GistOptBufferingMode::GIST_OPTION_BUFFERING_OFF
    );
}

#[test]
fn pgrcolumnar_sorted_by_reloption() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let image = |defs: &[(&str, &str)]| {
        let mut l = NodeList::nil();
        for (k, v) in defs {
            l.lappend(mcx, def(mcx, None, k, Some(v))).unwrap();
        }
        transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
            .unwrap()
            .unwrap()
    };

    // sorted_by rides alongside the heap vocabulary; names are trimmed.
    let img = image(&[("sorted_by", " k , v"), ("parallel_workers", "3")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.sorted_by_cols().collect::<Vec<_>>(), ["k", "v"]);
    assert_eq!(o.std.parallel_workers, 3);
    assert_eq!(RdOptions::Pgrcolumnar(o).std().unwrap().parallel_workers, 3);
    assert_eq!(
        AlterTableGetRelOptionsLockLevel(
            &NodeList::make1(mcx, def(mcx, None, "sorted_by", Some("k"))).unwrap()
        ),
        AccessExclusiveLock
    );

    let img = image(&[("sorted_by", "StationID, \"Event Date\", \"a\"\"b\"")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.sorted_by_cols().collect::<Vec<_>>(), ["stationid", "Event Date", "a\"b"]);
    assert_eq!(o.sorted_by(), "stationid, Event Date, a\"b");
    let img = image(&[("sorted_by", "K, k")]);
    let e = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "column \"k\" appears more than once in \"sorted_by\"");

    // Absent = no key; validate=false (relcache) tolerates anything.
    let img = image(&[("parallel_workers", "3")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.sorted_by_cols().count(), 0);
    let img = image(&[("sorted_by", "k DESC")]);
    assert!(pgrcolumnar_reloptions(mcx, Some(&img), false).is_ok());

    for (opts, want) in [
        (vec![("sorted_by", "k DESC")], "invalid value for option \"sorted_by\": \"k DESC\""),
        (vec![("sorted_by", "k nulls first")], "invalid value for option \"sorted_by\": \"k nulls first\""),
        (vec![("sorted_by", "k, k")], "column \"k\" appears more than once in \"sorted_by\""),
        (vec![("sorted_by", "k,,v")], "invalid value for option \"sorted_by\": \"k,,v\" (expected a comma-separated list of column names)"),
        (vec![("sorted_by", "k"), ("sorted_by", "v")], "parameter \"sorted_by\" specified more than once"),
    ] {
        let img = image(&opts);
        let e = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap_err();
        assert_eq!(e.message(), want);
    }
    let img = image(&[("sorted_by", "k")]);
    let e = heap_reloptions(mcx, RELKIND_RELATION, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "unrecognized parameter \"sorted_by\"");

    // The DDL check: a missing column and an unsortable one refuse with
    // their own messages; a valid key passes.
    let img = image(&[("sorted_by", "k, v")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap();
    assert!(pgrcolumnar_check_sorted_by(o.as_ref(), |_| SortedByColumn::Ok).is_ok());
    let e = pgrcolumnar_check_sorted_by(o.as_ref(), |c| {
        if c.col == "v" { SortedByColumn::Missing } else { SortedByColumn::Ok }
    })
    .unwrap_err();
    assert_eq!(e.message(), "column \"v\" named in \"sorted_by\" does not exist");
    let e = pgrcolumnar_check_sorted_by(o.as_ref(), |_| SortedByColumn::Unsortable).unwrap_err();
    assert_eq!(e.message(), "column \"k\" cannot be a \"sorted_by\" key");
}

#[test]
fn pgrcolumnar_shred_paths_and_partial_reloptions() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let image = |defs: &[(&str, &str)]| {
        let mut l = NodeList::nil();
        for (k, v) in defs {
            l.lappend(mcx, def(mcx, None, k, Some(v))).unwrap();
        }
        transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
            .unwrap()
            .unwrap()
    };
    let img = image(&[("pgrc_shred_paths", " kind , commit.collection:TEXT, time_us:int, f:bool, id:uuid")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
    assert_eq!(o.shred_paths(), "kind:text, commit.collection:text, time_us:numeric, f:bool, id:uuid");
    let d = o.shred_path_decls();
    assert_eq!(d.len(), 5);
    assert_eq!(d[1].keys, ["commit", "collection"]);
    assert!(!o.sorted_by_partial());
    let img = image(&[("sorted_by", "k"), ("sorted_by_partial", "true")]);
    let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
    assert!(o.sorted_by_partial());
    for (v, want) in [
        ("a:date", "invalid value for option \"pgrc_shred_paths\": \"a:date\""),
        ("a..b", "invalid value for option \"pgrc_shred_paths\": \"a..b\""),
        ("a:text, a:bool", "path \"a\" appears more than once in \"pgrc_shred_paths\""),
    ] {
        let img = image(&[("pgrc_shred_paths", v)]);
        let e = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap_err();
        assert_eq!(e.message(), want);
    }
    let img = image(&[("sorted_by_partial", "maybe")]);
    assert!(pgrcolumnar_reloptions(mcx, Some(&img), true).is_err());
}

#[test]
fn pgrcolumnar_sort_runs_reloption() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let image = |defs: &[(&str, &str)]| {
        let mut l = NodeList::nil();
        for (k, v) in defs {
            l.lappend(mcx, def(mcx, None, k, Some(v))).unwrap();
        }
        transformRelOptions(mcx, None, &l, None, HEAP_RELOPT_NAMESPACES, true, false)
            .unwrap()
            .unwrap()
    };
    let img = image(&[("sorted_by", "k")]);
    assert!(!pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap().sort_runs());
    for (v, on) in [("on", true), ("true", true), ("1", true), ("off", false), ("false", false)] {
        let img = image(&[("sorted_by", "k"), ("sort_runs", v)]);
        let o = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap();
        assert_eq!(o.sort_runs(), on, "{v}");
        assert_eq!(o.sorted_by(), "k");
    }
    let img = image(&[("sort_runs", "on")]);
    assert!(pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap().unwrap().sort_runs());
    let img = image(&[("sort_runs", "maybe")]);
    let e = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "invalid value for boolean option \"sort_runs\": maybe");
    assert!(!pgrcolumnar_reloptions(mcx, Some(&img), false).unwrap().unwrap().sort_runs());
    let img = image(&[("sort_runs", "on"), ("sort_runs", "off")]);
    let e = pgrcolumnar_reloptions(mcx, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "parameter \"sort_runs\" specified more than once");
    let img = image(&[("sort_runs", "on")]);
    let e = heap_reloptions(mcx, RELKIND_RELATION, Some(&img), true).unwrap_err();
    assert_eq!(e.message(), "unrecognized parameter \"sort_runs\"");
    assert_eq!(
        AlterTableGetRelOptionsLockLevel(
            &NodeList::make1(mcx, def(mcx, None, "sort_runs", Some("on"))).unwrap()
        ),
        ShareUpdateExclusiveLock
    );
    let both = {
        let mut l = NodeList::make1(mcx, def(mcx, None, "sort_runs", Some("on"))).unwrap();
        l.lappend(mcx, def(mcx, None, "sorted_by", Some("k"))).unwrap();
        l
    };
    assert_eq!(AlterTableGetRelOptionsLockLevel(&both), AccessExclusiveLock);
}

#[test]
fn lock_levels() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let l = NodeList::make1(mcx, def(mcx, None, "fillfactor", None)).unwrap();
    assert_eq!(AlterTableGetRelOptionsLockLevel(&l), ShareUpdateExclusiveLock);
    let l = NodeList::make2(
        mcx,
        def(mcx, None, "fillfactor", None),
        def(mcx, None, "buffering", None),
    )
    .unwrap();
    assert_eq!(AlterTableGetRelOptionsLockLevel(&l), AccessExclusiveLock);
    assert_eq!(AlterTableGetRelOptionsLockLevel(&NodeList::nil()), AccessExclusiveLock);
}

// DatumGetArrayTypeP detoasts: an attoptions image stored inline-compressed
// (a padded option value past the toast threshold) parses like the plain one.
#[test]
fn compressed_options_image_is_detoasted() {
    use core::mem::MaybeUninit;
    ::detoast::init_seams();
    let padded = format!("{}1", "0".repeat(3000));
    let padded: &str = &padded;
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let list = NodeList::make1(mcx, def(mcx, None, "n_distinct", Some(padded))).unwrap();
    let plain = transformRelOptions(mcx, None, &list, None, &[], false, false).unwrap().unwrap();
    let input = &plain[types_tuple::varatt::VARHDRSZ..];
    let mut dest = vec![MaybeUninit::<u8>::uninit(); pglz::pglz_max_output(input.len())];
    let n = pglz::pglz_compress_into(input, &mut dest, &pglz::PGLZ_STRATEGY_ALWAYS).unwrap();
    let total = 8 + n;
    let mut image = Vec::with_capacity(total);
    image.extend_from_slice(&(((total as u32) << 2) | 0x02).to_ne_bytes());
    image.extend_from_slice(&(input.len() as u32).to_ne_bytes());
    image.extend(dest[..n].iter().map(|b| unsafe { b.assume_init() }));
    let opts = attribute_reloptions(mcx, Some(&image), false).unwrap().unwrap();
    assert_eq!(opts.n_distinct, 1.0);
    let list2 = NodeList::make1(mcx, def(mcx, None, "n_distinct_inherited", Some("2"))).unwrap();
    let merged = transformRelOptions(mcx, Some(&image), &list2, None, &[], false, false)
        .unwrap()
        .unwrap();
    assert_eq!(texts_of(&merged), [format!("n_distinct={padded}"), "n_distinct_inherited=2".into()]);
}

// The option texts are borrowed from the image: a parse leaves nothing
// charged in the caller's context once the parsed values are dropped.
#[test]
fn parse_leaves_nothing_charged_in_the_callers_context() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let l = NodeList::make2(
        mcx,
        def(mcx, None, "seq_page_cost", Some("1.1")),
        def(mcx, None, "random_page_cost", Some("2")),
    )
    .unwrap();
    let img = transformRelOptions(mcx, None, &l, None, &[], false, false).unwrap().unwrap();
    let mut scratch = MemoryContext::new("scratch");
    let o = tablespace_reloptions(scratch.mcx(), Some(&img), false).unwrap().unwrap();
    assert_eq!(o.seq_page_cost, 1.1);
    assert_eq!(o.random_page_cost, 2.0);
    assert_eq!(scratch.used(), 0);
    scratch.reset();
}
