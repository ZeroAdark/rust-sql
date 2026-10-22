//! The differential (docs/gravity/expr.md §2.3): every tier-A reference and tier-B shim evaluated
//! through the fmgr wrapper (`FmgrCall`, the incumbent's protocol) and through the shim over the
//! corpus's cases for its argument types — the integer literals `execexpr/src/tests.rs` uses,
//! read in place — plus the generated edge cases (limits, overflow triggers, zero divisors, empty
//! and long strings, both collations); byte-identical results, null-ness, error text and sqlstate.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

use datum::Datum;
use gravity_fn::{checked, Arg, ErrSlot, Family, Region, Ret, View, ERROR, NULL, OK};


#[derive(Debug, PartialEq, Eq, Clone)]
enum Outcome {
    Null,
    Value(Vec<u8>),
    Error(String, i32),
    Panic(String),
}

struct Pools {
    i16: Vec<i64>,
    i32: Vec<i64>,
    i64: Vec<i64>,
    f32: Vec<i64>,
    f64: Vec<i64>,
    bool_: Vec<i64>,
    u32: Vec<i64>,
    text: Vec<Vec<u8>>,
    numeric: Vec<Vec<u8>>,
    interval: Vec<Vec<u8>>,
    /// Per SQL type outside the by-value / text / numeric / interval classes: the images the fmgr
    /// side sees (a fixed-size type's typlen bytes, a varlena's 4-byte-header image), built through
    /// the type's own input function, `construct_array`, or by hand (ranges).
    images: std::cell::RefCell<HashMap<String, Vec<Vec<u8>>>>,
}

fn corpus_literals() -> Vec<i64> {
    let text = std::fs::read_to_string(common::repo_root().join("crates/backend/executor/execexpr/src/tests.rs")).unwrap();
    let mut out: Vec<i64> = Vec::new();
    for pat in ["Some(", "from_i32(", "from_i64(", "from_i16(", "mk_int4_const(mcx, Some("] {
        for (i, _) in text.match_indices(pat) {
            let rest = &text[i + pat.len()..];
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '_')).unwrap_or(rest.len());
            if let Ok(v) = rest[..end].replace('_', "").parse::<i64>() {
                out.push(v);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn corpus_funcids() -> Vec<u32> {
    let text = std::fs::read_to_string(common::repo_root().join("crates/backend/executor/execexpr/src/tests.rs")).unwrap();
    let mut out = Vec::new();
    for pat in ["mk_opexpr(mcx, ", "opfuncid: ", "funcid: "] {
        for (i, _) in text.match_indices(pat) {
            let rest = &text[i + pat.len()..];
            let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if let Ok(v) = rest[..end].parse::<u32>() {
                out.push(v);
            }
        }
    }
    out.push(65);
    out.sort();
    out.dedup();
    out
}

fn pools() -> Pools {
    let lits = corpus_literals();
    let mut i32v: Vec<i64> = vec![0, 1, -1, 2, -2, 7, 10, 100, 1000, i32::MAX as i64, i32::MIN as i64, (i32::MAX - 1) as i64, (i32::MIN + 1) as i64, 65536, -65536, 46341, -46341];
    i32v.extend(lits.iter().filter(|v| **v >= i32::MIN as i64 && **v <= i32::MAX as i64).take(24));
    i32v.sort();
    i32v.dedup();
    let mut i64v: Vec<i64> = vec![0, 1, -1, 2, -2, 7, 1000, i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1, 3037000500, -3037000500, 1 << 40, 86400 * 1_000_000, 9224318016000000, -210866803200000000];
    i64v.extend(lits.iter().take(16));
    i64v.sort();
    i64v.dedup();
    let i16v: Vec<i64> = vec![0, 1, -1, 2, 181, -181, 182, 255, i16::MAX as i64, i16::MIN as i64, (i16::MIN + 1) as i64, 1000];
    let f64v: Vec<i64> = [0.0, -0.0, 1.0, -1.0, 0.5, 2.5, 1e10, -1e10, 1e308, -1e308, f64::MAX, f64::MIN_POSITIVE, 5e-324, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 3.0, 1e-300, 2147483648.0, -2147483649.0, 9.3e18]
        .iter()
        .map(|f| f.to_bits() as i64)
        .collect();
    let f32v: Vec<i64> = [0.0f32, -0.0, 1.0, -1.0, 0.5, 1e10, f32::MAX, f32::MIN_POSITIVE, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 3.0, 1e-40, 2147483648.0]
        .iter()
        .map(|f| f.to_bits() as u32 as i64)
        .collect();
    let long: Vec<u8> = (0..300u32).map(|i| b'a' + (i % 26) as u8).collect();
    let text: Vec<Vec<u8>> = vec![b"".to_vec(), b"a".to_vec(), b"abc".to_vec(), b"ABC".to_vec(), b"abcd".to_vec(), b"b".to_vec(), "héllo".as_bytes().to_vec(), b"  padded  ".to_vec(), b"a%".to_vec(), b"_bc".to_vec(), b"1234".to_vec(), b"-17".to_vec(), b"x\\y".to_vec(), long];
    let numeric: Vec<Vec<u8>> = [0i64, 1, -1, 123456789, i64::MAX, i64::MIN, 7]
        .iter()
        .map(|v| adt_numeric::int64_to_numeric(*v).as_bytes()[4..].to_vec())
        .chain(["123.456", "-0.001", "1e30", "NaN", "Infinity", "-Infinity", "0.5", "99999999999999999999.99999"].iter().map(|s| adt_numeric::numeric_in(s, -1, None).unwrap().unwrap().as_bytes()[4..].to_vec()))
        .collect();
    let iv = |time: i64, day: i32, month: i32| {
        let mut b = Vec::with_capacity(16);
        b.extend_from_slice(&time.to_ne_bytes());
        b.extend_from_slice(&day.to_ne_bytes());
        b.extend_from_slice(&month.to_ne_bytes());
        b
    };
    let interval = vec![iv(0, 0, 0), iv(1, 0, 0), iv(-1, 0, 0), iv(3_600_000_000, 1, 1), iv(i64::MAX, 0, 0), iv(0, i32::MAX, 0), iv(0, 0, i32::MIN), iv(86_400_000_000 * 400, 31, -13)];
    Pools { i16: i16v, i32: i32v, i64: i64v, f32: f32v, f64: f64v, bool_: vec![0, 1], u32: vec![0, 1, 2, 16, 100, 950, u32::MAX as i64, i32::MAX as i64], text, numeric, interval, images: Default::default() }
}

/// Input literals per type; the type's own input function turns them into images.
fn literals(t: &str) -> &'static [&'static str] {
    match t {
        "uuid" => &["00000000-0000-0000-0000-000000000000", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", "ffffffff-ffff-ffff-ffff-ffffffffffff", "0188e4e8-9c6a-7c9a-8b9f-7f9a9c6a1234", "11111111-2222-1333-8444-555555555555"],
        "macaddr" => &["08:00:2b:01:02:03", "00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff", "01-23-45-67-89-ab", "08:00:2b:01:02:04"],
        "macaddr8" => &["08:00:2b:01:02:03:04:05", "00:00:00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff:ff:ff", "08:00:2b:01:02:03", "08:00:2b:ff:fe:01:02:03"],
        "tid" => &["(0,1)", "(4294967295,65535)", "(12,0)", "(1,7)", "(0,0)"],
        "timetz" => &["00:00:00+00", "23:59:59.999999-12", "12:34:56.789+05:30", "24:00:00+00", "12:00:00+15:59"],
        "inet" => &["192.168.1.5/24", "::1", "10.0.0.0/8", "2001:db8::/32", "255.255.255.255", "0.0.0.0/0", "192.168.1.5", "10.1.2.3/31"],
        "cidr" => &["192.168.0.0/16", "10.0.0.0/8", "::/0", "2001:db8::/32", "10.1.2.3/32"],
        "jsonb" | "json" => &["{\"a\":1,\"b\":[1,2,3],\"c\":{\"d\":\"x\"}}", "[1,2,3]", "\"str\"", "1.5", "true", "null", "{}", "[]", "{\"a\":null}", "12345678901234567890", "[\"a\", {\"b\": [true, false]}]", "-0.0"],
        "jsonpath" => &["$.a", "$[*] ? (@ > 1)", "strict $.a.b", "$"],
        "point" => &["(0,0)", "(1,2)", "(-1.5,2.5)", "(1e300,-1e300)", "(3,4)"],
        "lseg" => &["[(0,0),(1,1)]", "[(1,1),(0,0)]", "[(0,0),(0,0)]", "[(-3,2),(5,-7)]"],
        "box" => &["((0,0),(1,1))", "((2,2),(0,0))", "((1,1),(1,1))", "((-10,-10),(10,10))"],
        "line" => &["{1,-1,0}", "{0,1,-2}", "{1,0,0}", "{2,3,-6}"],
        "path" => &["((0,0),(1,1),(2,0))", "[(0,0),(1,1)]", "((1,1))", "[(0,0),(3,0),(3,4),(0,4)]"],
        "polygon" => &["((0,0),(1,1),(2,0))", "((0,0),(0,1),(1,1),(1,0))", "((1,1))"],
        "circle" => &["<(0,0),1>", "<(1,1),0>", "<(-2,3),2.5>", "<(0,0),1e10>"],
        "tsvector" => &["'a':1 'b':2", "'the' 'quick' 'brown'", "", "'x':1A,2B 'y':3C", "'a' 'a'"],
        "tsquery" => &["'a' & 'b'", "'a' | !'b'", "'a' <-> 'b'", "'a' <2> 'b'", "'x'", "!'a'", "'a' & ('b' | 'c')"],
        "bit" => &["0", "1", "101", "1111111111111111", "0000000000000000000000000000000001", "10101010101010101010101010101010"],
        "varbit" => &["", "1", "010", "11111111111111111111111111111111111", "0000"],
        "xml" => &["<a/>", "<a>b</a>", "text", "<a><b>1</b><c/></a>"],
        "int2vector" => &["1 2 3", "", "32767 -32768", "7"],
        "pg_snapshot" => &["10:20:10,14,15", "1:1:", "100:100:", "5:7:6"],
        "pg_node_tree" | "pg_ndistinct" | "pg_dependencies" | "pg_mcv_list" => &[],
        _ => &[],
    }
}

fn image_datum_copy(p: *const u8, t: &str, types: &HashMap<String, common::Ty>) -> Vec<u8> {
    let n = unsafe { common::result_len(p, t, types) };
    let img = unsafe { std::slice::from_raw_parts(p, n) }.to_vec();
    if types[t].len == -1 && img[0] & 1 == 1 {
        // a short-header varlena: the shim's image contract is the 4-byte header form
        let mut v = common::image(&img[1..]);
        v.shrink_to_fit();
        return v;
    }
    img
}

fn range_image(typid: u32, lower: Option<i32>, upper: Option<i32>, flags: u8) -> Vec<u8> {
    let mut body = typid.to_ne_bytes().to_vec();
    if let Some(l) = lower {
        body.extend_from_slice(&l.to_ne_bytes());
    }
    if let Some(u) = upper {
        body.extend_from_slice(&u.to_ne_bytes());
    }
    body.push(flags);
    common::image(&body)
}

/// The images for a type: built once, through the type's input function (`FmgrCall`), through
/// `construct_array` for the array instantiations, by hand for ranges.
fn images_for(t: &str, p: &Pools, types: &HashMap<String, common::Ty>, procs: &HashMap<u32, common::Proc>) -> Vec<Vec<u8>> {
    if let Some(v) = p.images.borrow().get(t) {
        return v.clone();
    }
    let out: Vec<Vec<u8>> = match t {
        "_int4" | "_text" | "_float8" | "_float4" | "_cstring" | "_int8" | "_char" => {
            let cx: &'static mut mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new_bump("differential arrays")));
            let mut out = Vec::new();
            let sets: Vec<Vec<Datum>> = match t {
                "_int4" => vec![vec![], vec![Datum::from_i32(1)], vec![Datum::from_i32(1), Datum::from_i32(2), Datum::from_i32(3)], vec![Datum::from_i32(-1), Datum::from_i32(0), Datum::from_i32(i32::MAX)]],
                "_float8" => vec![vec![], vec![Datum::from_f64(1.5), Datum::from_f64(f64::NAN)]],
                "_int8" => vec![vec![], vec![Datum::from_i64(1), Datum::from_i64(i64::MAX)], vec![Datum::from_i64(0), Datum::from_i64(-1), Datum::from_i64(7)]],
                "_char" => vec![vec![], vec![Datum::from_char(b'a' as i8)], vec![Datum::from_char(b'x' as i8), Datum::from_char(0), Datum::from_char(b'z' as i8)]],
                "_float4" => vec![vec![], vec![Datum::from_f32(0.1), Datum::from_f32(0.2), Datum::from_f32(0.4), Datum::from_f32(1.0)], vec![Datum::from_f32(1.0)]],
                "_cstring" => {
                    let cs: Vec<&'static [u8]> = [b"7\0".as_slice(), b"12\0", b"abc\0", b"-1\0"].iter().map(|s| Box::leak(s.to_vec().into_boxed_slice()) as &[u8]).collect();
                    vec![vec![], vec![Datum::from_usize(cs[0].as_ptr() as usize)], vec![Datum::from_usize(cs[0].as_ptr() as usize), Datum::from_usize(cs[1].as_ptr() as usize)], vec![Datum::from_usize(cs[2].as_ptr() as usize)], vec![Datum::from_usize(cs[3].as_ptr() as usize)]]
                }
                _ => {
                    let imgs: Vec<&'static [u8]> = [b"a".as_slice(), b"bc", b""].iter().map(|s| Box::leak(common::image(s).into_boxed_slice()) as &[u8]).collect();
                    vec![vec![], vec![common::image_datum(imgs[0])], vec![common::image_datum(imgs[0]), common::image_datum(imgs[1]), common::image_datum(imgs[2])]]
                }
            };
            let (elm, len, byval, align) = match t {
                "_int4" => (23u32, 4, true, arrayfuncs::foundation::TYPALIGN_INT),
                "_float8" => (701, 8, true, arrayfuncs::foundation::TYPALIGN_DOUBLE),
                "_int8" => (20, 8, true, arrayfuncs::foundation::TYPALIGN_DOUBLE),
                "_char" => (18, 1, true, arrayfuncs::foundation::TYPALIGN_CHAR),
                "_float4" => (700, 4, true, arrayfuncs::foundation::TYPALIGN_INT),
                "_cstring" => (2275, -2, false, arrayfuncs::foundation::TYPALIGN_CHAR),
                _ => (25, -1, false, arrayfuncs::foundation::TYPALIGN_INT),
            };
            for set in &sets {
                let img = arrayfuncs::construct_array(cx.mcx(), set, elm, len, byval, align).unwrap();
                out.push(img.to_vec());
            }
            out
        }
        "int4range" => vec![
            range_image(3904, None, None, 0x01),
            range_image(3904, Some(1), Some(10), 0x02),
            range_image(3904, Some(-5), Some(5), 0x02 | 0x04),
            range_image(3904, None, None, 0x08 | 0x10),
            range_image(3904, None, Some(7), 0x08),
            range_image(3904, Some(3), None, 0x02 | 0x10),
        ],
        // an empty multirange: header, the multirange type oid, a zero range count
        "int4multirange" => vec![common::image(&[4451u32.to_ne_bytes(), 0u32.to_ne_bytes()].concat())],
        // oidvector: a 1-D oid array with lower bound 0 (ndim, dataoffset 0, elemtype 26, dim, lb, data)
        "oidvector" => [vec![], vec![1u32], vec![26, 23, 25, 0], vec![u32::MAX]]
            .iter()
            .map(|oids| {
                let mut b = Vec::new();
                for w in [1i32, 0, 26, oids.len() as i32, 0] {
                    b.extend_from_slice(&w.to_ne_bytes());
                }
                for o in oids {
                    b.extend_from_slice(&o.to_ne_bytes());
                }
                common::image(&b)
            })
            .collect(),
        // pg_node_tree carries text; its input function refuses, the images are text images
        "pg_node_tree" => [b"a".as_slice(), b"{QUERY :commandType 1}", b""].iter().map(|s| common::image(s)).collect(),
        // the statistics and BRIN summary types: their out/send shims read the image as bytes (a
        // bloom summary's header: nhashes at byte 2, nbits and nbits_set as words 1 and 2)
        "pg_ndistinct" | "pg_dependencies" | "pg_mcv_list" | "pg_brin_bloom_summary" | "pg_brin_minmax_multi_summary" => {
            [vec![1u8, 0, 7, 0, 64, 0, 0, 0, 5, 0, 0, 0, 0xff, 0x0f], vec![0; 16], b"xyz\0\x01 sixteen bytes".to_vec()].iter().map(|b| common::image(b)).collect()
        }
        // aclitem: grantee, grantor, the privilege bits (16 bytes; no input function without the catalog)
        "aclitem" => [(10u32, 10u32, 0x0000_0001_0000_0001u64), (0, 10, 0x0000_0003_0000_0003), (16384, 10, 0), (10, 16384, u64::MAX)]
            .iter()
            .map(|(g, o, p)| [g.to_ne_bytes().as_slice(), o.to_ne_bytes().as_slice(), p.to_ne_bytes().as_slice()].concat())
            .collect(),
        "_aclitem" => {
            let cx: &'static mut mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new_bump("differential acl arrays")));
            let items = images_for("aclitem", p, types, procs);
            let mut out = Vec::new();
            for n in [0, 1, items.len()] {
                let elems: Vec<Datum> = items[..n].iter().map(|i| Datum::from_usize(i.as_ptr() as usize)).collect();
                out.push(arrayfuncs::construct_array(cx.mcx(), &elems, 1033, 16, false, arrayfuncs::foundation::TYPALIGN_DOUBLE).unwrap().to_vec());
            }
            out
        }
        // gtsvector (no input function): the GiST signature forms — a 124-byte signature, all-true, an array key
        "gtsvector" => {
            let mut sig = vec![0u8; 124];
            sig[0] = 0x81;
            sig[123] = 0x10;
            [(2i32, sig), (2 | 4, vec![]), (1, [7u32, 9, 11].iter().flat_map(|w| w.to_ne_bytes()).collect())]
                .iter()
                .map(|(flag, body)| common::image(&[flag.to_ne_bytes().as_slice(), body].concat()))
                .collect()
        }
        _ => {
            let lits = literals(t);
            let mut out = Vec::new();
            if !lits.is_empty() {
                let ty = &types[t];
                let proc_ = procs.values().find(|p| p.prosrc == ty.input || p.name == ty.input).unwrap_or_else(|| panic!("no input function {} for {t}", ty.input));
                let nargs = proc_.args.len();
                let mut call = gravity_fn::FmgrCall::bind(proc_.oid, nargs, 0).unwrap();
                let elem = types.get(&ty.elem).map_or(0, |e| e.oid);
                for lit in lits {
                    let mut c = lit.as_bytes().to_vec();
                    c.push(0);
                    let mut args = vec![Datum::from_usize(c.as_ptr() as usize)];
                    if nargs == 3 {
                        args.push(Datum::from_u32(elem));
                        args.push(Datum::from_i32(-1));
                    }
                    let nulls = vec![false; nargs];
                    match call.call_site(&args, &nulls) {
                        Ok(v) if !v.isnull => out.push(image_datum_copy(v.value.as_usize() as *const u8, t, types)),
                        Ok(_) => panic!("{t} input of {lit:?} returned null"),
                        Err(e) => panic!("{t} input of {lit:?}: {}", e.message()),
                    }
                }
            }
            out
        }
    };
    p.images.borrow_mut().insert(t.to_owned(), out.clone());
    out
}

enum Val {
    V(i64),
    R(Vec<u8>),
}

/// Functions whose output grows with an integer argument take a bounded count pool (the
/// error path at the top is still covered).
const COUNT_BOUNDED: &[&str] = &["repeat", "lpad", "rpad", "text_left", "text_right", "bitshiftleft", "bitshiftright", "bitfromint4", "bitfromint8", "bitsubstr", "bitsubstr_no_len", "bit", "varbit", "bit_overlay", "bit_overlay_no_len", "byteaSetBit", "byteaGetBit", "byteaSetByte", "byteaGetByte", "inet_set_masklen", "cidr_set_masklen", "network_show"];
const COUNTS: &[i64] = &[0, 1, 2, 7, 300, 4096, -1, -300, i32::MIN as i64];

fn pool_for(t: &str, p: &Pools, name: &str, types: &HashMap<String, common::Ty>, procs: &HashMap<u32, common::Proc>) -> Vec<Val> {
    let byval = |v: &Vec<i64>| v.iter().map(|x| Val::V(*x)).collect::<Vec<_>>();
    if (t == "int4" || t == "int8") && COUNT_BOUNDED.contains(&name) {
        return COUNTS.iter().map(|x| Val::V(*x)).collect();
    }
    match t {
        "bool" => byval(&p.bool_),
        "char" => byval(&p.i16),
        "int2" => byval(&p.i16),
        "int4" | "date" => byval(&p.i32),
        "int8" | "money" | "time" | "timestamp" | "timestamptz" | "pg_lsn" | "xid8" => byval(&p.i64),
        "float4" => byval(&p.f32),
        "float8" => byval(&p.f64),
        "numeric" => p.numeric.iter().map(|b| Val::R(b.clone())).collect(),
        "interval" => p.interval.iter().map(|b| Val::R(b.clone())).collect(),
        t if common::VIEW_TYPES.contains(&t) || t == "unknown" => p.text.iter().map(|b| Val::R(b.clone())).collect(),
        t if common::BYVAL_TYPES.contains(&t) || types.get(t).is_some_and(|ty| ty.byval) => byval(&p.u32),
        t => images_for(t, p, types, procs).into_iter().map(Val::R).collect(),
    }
}

/// The cases for a signature: the full product for one and two arguments, a bounded sample past that.
fn cases(pools: &[usize]) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    if pools.iter().any(|&n| n == 0) {
        return out;
    }
    match pools.len() {
        0 => out.push(vec![]),
        1 => (0..pools[0]).for_each(|i| out.push(vec![i])),
        2 => {
            let total = pools[0] * pools[1];
            let stride = (total / 160).max(1);
            let mut k = 0;
            for i in 0..pools[0] {
                for j in 0..pools[1] {
                    if k % stride == 0 || i == j || i + 1 == pools[0] || j + 1 == pools[1] {
                        out.push(vec![i, j]);
                    }
                    k += 1;
                }
            }
        }
        n => {
            let m = pools.iter().copied().max().unwrap();
            for k in 0..m * 2 {
                out.push((0..n).map(|a| (k * (a + 1) + k / (a + 1)) % pools[a]).collect());
            }
        }
    }
    out
}

thread_local! {
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The resolved argument types of the call in flight: what `get_fn_expr_argtype` answers.
    static CUR_TYPES: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn quiet<T>(f: impl FnOnce() -> T) -> T {
    QUIET.with(|q| q.set(true));
    let r = f();
    QUIET.with(|q| q.set(false));
    r
}

fn fn_expr_argtype(_flinfo: &fmgr::FmgrInfo, argnum: i16) -> u32 {
    CUR_TYPES.with(|t| t.borrow().get(argnum as usize).copied().unwrap_or(0))
}

fn outcome_of_fmgr(call: &mut gravity_fn::FmgrCall, args: &[Datum], ret: &str, types: &HashMap<String, common::Ty>) -> Outcome {
    let nulls = vec![false; args.len()];
    match quiet(|| catch_unwind(AssertUnwindSafe(|| call.call_site(args, &nulls)))) {
        Err(p) => Outcome::Panic(panic_text(&p)),
        Ok(Err(e)) => Outcome::Error(e.message().to_owned(), e.sqlstate().0),
        Ok(Ok(v)) if v.isnull => Outcome::Null,
        Ok(Ok(v)) => {
            if common::ret_byval(ret, types) {
                Outcome::Value(int_word(ret, v.value.as_i64()).to_ne_bytes().to_vec())
            } else {
                let p = v.value.as_usize() as *const u8;
                let n = unsafe { common::result_len(p, ret, types) };
                Outcome::Value(unsafe { std::slice::from_raw_parts(p, n) }.to_vec())
            }
        }
    }
}

/// D2311: the fmgr wrapper's by-value word as the emitted code must hold it. An int2 / int4 value is the word C reads
/// through DatumGetInt16 / DatumGetInt32, sign-extended (prim.rs open-codes int2 / int4 over the whole word); a hash
/// wrapper's own word is zero-extended (`UInt32GetDatum`). The shim's word is compared raw, so a shim that leaves an
/// int2 / int4 result zero-extended is a mismatch. Every other type's word is compared as the wrapper left it.
fn int_word(ret: &str, w: i64) -> i64 {
    match ret {
        "int2" => w as i16 as i64,
        "int4" => w as i32 as i64,
        _ => w,
    }
}

fn outcome_of_shim(shim: &gravity_fn::Shim, coll: u32, tys: &[u32], args: &[Arg], ret: &str, types: &HashMap<String, common::Ty>, region: &mut Vec<u8>) -> Outcome {
    let mut slot = ErrSlot::NONE;
    let mut r = Region::over(region);
    let mut res: Result<Ret, _> = quiet(|| catch_unwind(AssertUnwindSafe(|| (shim.call)(&mut slot, &mut r, coll, tys, args))));
    if matches!(res, Ok(Ret { status, .. }) if status == gravity_fn::NO_SPACE) {
        region.resize(64 << 20, 0);
        r = Region::over(region);
        res = quiet(|| catch_unwind(AssertUnwindSafe(|| (shim.call)(&mut slot, &mut r, coll, tys, args))));
    }
    match res {
        Err(p) => Outcome::Panic(panic_text(&p)),
        Ok(Ret { status, .. }) if status == NULL => Outcome::Null,
        Ok(Ret { status, .. }) if status == ERROR => {
            let e = slot.take().expect("ERROR status without an error in the slot");
            Outcome::Error(e.message().to_owned(), e.sqlstate().0)
        }
        Ok(Ret { val, status }) if status == OK => {
            if common::ret_byval(ret, types) {
                Outcome::Value(val.to_ne_bytes().to_vec())
            } else {
                let p = val as usize as *const u8;
                let n = unsafe { common::result_len(p, ret, types) };
                Outcome::Value(unsafe { std::slice::from_raw_parts(p, n) }.to_vec())
            }
        }
        Ok(r) => Outcome::Panic(format!("status {}", r.status)),
    }
}

fn panic_text(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "?".into())
}

#[test]
fn shims_match_the_fmgr_wrapper() {
    common::install_seams();
    fmgr_seams::get_fn_expr_argtype::set(fn_expr_argtype);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !QUIET.with(|q| q.get()) {
            prev(info);
        }
    }));
    let procs = common::procs();
    let types = common::types();
    let p = pools();
    let corpus: HashSet<u32> = corpus_funcids().into_iter().collect();
    let mut region = vec![0u8; 1 << 16];
    let mut functions = 0usize;
    let mut total_cases = 0usize;
    let mut corpus_covered = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    let mut panics: BTreeMap<&str, String> = BTreeMap::new();
    let mut errors_compared = 0usize;
    let mut uncovered: Vec<String> = Vec::new();
    let mut per_kind: BTreeMap<&str, usize> = BTreeMap::new();
    let only: Option<u32> = std::env::var("GRAVITY_DIFF_ONLY").ok().and_then(|s| s.parse().ok());
    let trace = std::env::var("GRAVITY_DIFF_TRACE").is_ok();
    for shim in gravity_fn::SHIMS.iter().filter(|s| only.map_or(true, |o| o == s.oid)) {
        let proc_ = &procs[&shim.oid];
        let collations: &[u32] = if shim.coll { &[950, 100] } else { &[0] };
        let poly = proc_.args.iter().chain(std::iter::once(&proc_.ret)).any(|t| common::POLY_ELEM.contains(&t.as_str()) || common::POLY_IMG.contains(&t.as_str()));
        functions += 1;
        if corpus.contains(&shim.oid) {
            corpus_covered += 1;
        }
        let mut shim_cases = 0usize;
        for k in 0..(if poly { 2 } else { 1 }) {
            // the concrete types of this instantiation
            let conc = |t: &str| -> Option<String> {
                if common::POLY_ELEM.contains(&t) || common::POLY_IMG.contains(&t) {
                    common::instantiate(t, k).map(str::to_owned)
                } else {
                    Some(t.to_owned())
                }
            };
            let Some(cargs) = proc_.args.iter().map(|t| conc(t)).collect::<Option<Vec<_>>>() else { continue };
            let Some(cret) = conc(&proc_.ret) else { continue };
            let arg_pools: Vec<Vec<Val>> = cargs.iter().map(|t| pool_for(t, &p, shim.name, &types, &procs)).collect();
            let case_list = cases(&arg_pools.iter().map(Vec::len).collect::<Vec<_>>());
            let tys: Vec<u32> = shim.tyargs.iter().map(|&i| if i < 0 { common::type_oid(&cret, &types) } else { common::type_oid(&cargs[i as usize], &types) }).collect();
            let arg_oids: Vec<u32> = cargs.iter().map(|t| common::type_oid(t, &types)).collect();
            if trace {
                eprintln!("  {} {} ({} cases, instantiation {k}: {:?} -> {cret})", shim.oid, shim.name, case_list.len(), cargs);
            }
            let started = std::time::Instant::now();
            for &coll in collations {
                let mut call = gravity_fn::FmgrCall::bind(shim.oid, proc_.args.len(), coll).unwrap();
                for case in &case_list {
                    let mut images: Vec<Vec<u8>> = Vec::new();
                    let mut datums = Vec::new();
                    let mut args = Vec::new();
                    for (i, &kk) in case.iter().enumerate() {
                        let t = cargs[i].as_str();
                        match (&arg_pools[i][kk], shim.args[i]) {
                            (Val::V(v), _) => {
                                datums.push(Datum::from_i64(*v));
                                args.push(Arg::V(*v));
                            }
                            (Val::R(b), gravity_fn::ArgKind::P) => {
                                let img = if t == "name" {
                                    let mut n = [0u8; 64];
                                    let l = b.len().min(63);
                                    n[..l].copy_from_slice(&b[..l]);
                                    n.to_vec()
                                } else if t == "cstring" || t == "unknown" {
                                    let mut c = b.clone();
                                    c.retain(|&x| x != 0);
                                    c.push(0);
                                    c
                                } else if t == "interval" {
                                    b.clone()
                                } else {
                                    common::image(b)
                                };
                                images.push(img);
                                let img = images.last().unwrap();
                                datums.push(common::image_datum(img));
                                args.push(Arg::R(match t {
                                    "name" => View::of(&img[..]),
                                    "cstring" | "unknown" => View::of(&img[..img.len() - 1]),
                                    "interval" => View::of(img),
                                    _ => View::of(&img[4..]),
                                }));
                            }
                            (Val::R(b), gravity_fn::ArgKind::I) => {
                                images.push(b.clone());
                                let img = images.last().unwrap();
                                datums.push(common::image_datum(img));
                                args.push(Arg::R(View::of(img)));
                            }
                            (Val::R(b), gravity_fn::ArgKind::D) => {
                                images.push(common::image(b));
                                let img = images.last().unwrap();
                                datums.push(common::image_datum(img));
                                args.push(Arg::V(img.as_ptr() as i64));
                            }
                            (Val::R(_), gravity_fn::ArgKind::V) => panic!("{}: a by-reference pool for a by-value argument {t}", shim.name),
                        }
                    }
                    CUR_TYPES.with(|c| *c.borrow_mut() = arg_oids.clone());
                    if trace && only.is_some() {
                        eprintln!("    case {case:?} fmgr");
                    }
                    let a = outcome_of_fmgr(&mut call, &datums, &cret, &types);
                    if trace && only.is_some() {
                        eprintln!("    case {case:?} shim (fmgr: {:?})", trunc(&a));
                    }
                    let b = outcome_of_shim(shim, coll, &tys, &args, &cret, &types, &mut region);
                    total_cases += 1;
                    shim_cases += 1;
                    if matches!(a, Outcome::Error(..)) {
                        errors_compared += 1;
                    }
                    match (&a, &b) {
                        (x, y) if NONDETERMINISTIC.contains(&shim.name) && same_shape(x, y) => {}
                        (Outcome::Panic(x), Outcome::Panic(_)) => {
                            panics.entry(shim.name).or_insert_with(|| x.chars().take(80).collect());
                        }
                        (x, y) if x == y => {}
                        (x, y) => {
                            if mismatches.len() < 40 {
                                mismatches.push(format!("{} (oid {}, coll {coll}, k {k}) case {:?}: fmgr {:?} vs shim {:?}", shim.name, shim.oid, case, trunc(x), trunc(y)));
                            } else {
                                mismatches.push(String::new());
                            }
                        }
                    }
                    call.reset_window();
                }
            }
            if trace && started.elapsed().as_millis() > 100 {
                eprintln!("    slow: {} {} {} ms for {} cases", shim.oid, shim.name, started.elapsed().as_millis(), case_list.len() * collations.len());
            }
        }
        for k in shim.args {
            *per_kind.entry(match k { gravity_fn::ArgKind::V => "V", gravity_fn::ArgKind::P => "P", gravity_fn::ArgKind::I => "I", gravity_fn::ArgKind::D => "D" }).or_default() += 1;
        }
        if shim_cases == 0 {
            uncovered.push(format!("{} {} ({:?})", shim.oid, shim.name, proc_.args));
        }
    }
    let _ = std::panic::take_hook();
    let img = p.images.borrow();
    eprintln!("differential: {functions} shims x {total_cases} cases ({errors_compared} error cases compared byte-identical), corpus funcids covered {corpus_covered}/{}; argument kinds {per_kind:?}; image pools {:?}", corpus.iter().filter(|o| gravity_fn::shim_of(**o).is_some()).count(), img.iter().map(|(k, v)| (k.as_str(), v.len())).collect::<BTreeMap<_, _>>());
    if !panics.is_empty() {
        eprintln!("both sides panicked (uncomparable, {} functions): {:?}", panics.len(), panics);
    }
    if !uncovered.is_empty() {
        eprintln!("shims without a case (no pool for an argument type): {uncovered:?}");
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    assert!(only.is_some() || uncovered.is_empty(), "shims without cases: {uncovered:?}");
}

/// Volatile shims whose value is the clock or the PRNG: the two sides agree in kind (value / null /
/// error), not bytes — a random numeric's image length varies with its digits.
const NONDETERMINISTIC: &[&str] = &["timeofday", "clock_timestamp", "drandom", "drandom_normal", "int4random", "int8random", "numeric_random", "gen_random_uuid", "uuidv7", "uuidv7_interval"];

fn same_shape(x: &Outcome, y: &Outcome) -> bool {
    match (x, y) {
        (Outcome::Value(_), Outcome::Value(_)) | (Outcome::Error(..), Outcome::Error(..)) | (Outcome::Null, Outcome::Null) => true,
        _ => false,
    }
}

fn trunc(o: &Outcome) -> Outcome {
    match o {
        Outcome::Value(v) if v.len() > 24 => Outcome::Value(v[..24].to_vec()),
        Outcome::Error(m, s) => Outcome::Error(m.chars().take(60).collect(), *s),
        x => x.clone(),
    }
}

#[test]
fn checked_forms_match_the_typed_bodies() {
    let p = pools();
    for &a in &p.i32 {
        for &b in &p.i32 {
            let (a, b) = (a as i32, b as i32);
            let ok = |r: Result<i32, Box<types_error::PgError>>, f: (i32, bool)| {
                assert_eq!(r.is_err(), f.1, "{a} {b}");
                if let Ok(v) = r {
                    assert_eq!(v, f.0, "{a} {b}");
                }
            };
            ok(adt_int::int4pl(a, b), checked::add_i32(a, b));
            ok(adt_int::int4mi(a, b), checked::sub_i32(a, b));
            ok(adt_int::int4mul(a, b), checked::mul_i32(a, b));
            ok(adt_int::int4div(a, b), checked::div_i32(a, b));
            ok(adt_int::int4mod(a, b), checked::mod_i32(a, b));
        }
    }
    for &a in &p.i64 {
        for &b in &p.i64 {
            assert_eq!(adt_int8::int8pl(a, b).is_err(), checked::add_i64(a, b).1, "{a} + {b}");
            assert_eq!(adt_int8::int8mul(a, b).is_err(), checked::mul_i64(a, b).1, "{a} * {b}");
            assert_eq!(adt_int8::int8div(a, b).is_err(), checked::div_i64(a, b).1, "{a} / {b}");
            if let Ok(v) = adt_int8::int8div(a, b) {
                assert_eq!(v, checked::div_i64(a, b).0);
            }
        }
    }
    for &a in &p.f64 {
        for &b in &p.f64 {
            let (a, b) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            for (body, form) in [(adt_float::float8_pl(a, b), checked::add_f64(a, b)), (adt_float::float8_mi(a, b), checked::sub_f64(a, b)), (adt_float::float8_mul(a, b), checked::mul_f64(a, b)), (adt_float::float8_div(a, b), checked::div_f64(a, b))] {
                assert_eq!(body.is_err(), form.1, "{a} {b}");
                if let Ok(v) = body {
                    assert_eq!(v.to_bits(), form.0.to_bits(), "{a} {b}");
                }
            }
        }
    }
}

/// The error protocol (§2.4): a window with two dirty rows raises the earlier row's error on
/// replay; a row whose division is behind a false branch mask never dirties the word.
#[test]
fn flag_word_replays_the_earlier_row_and_masks_branches() {
    let a = [10i32, i32::MAX, 5, 7];
    let b = [2i32, 1, 0, 0];
    let live = [true, true, true, false];
    let mut word = 0u64;
    for row in 0..4 {
        let (_, f1) = checked::add_i32(a[row], b[row]);
        let (_, f2) = checked::div_i32(a[row], b[row]);
        checked::set_flag(&mut word, row, f1 | f2, live[row]);
    }
    assert_eq!(word, 0b0110);
    let first = checked::first_dirty(word).unwrap();
    assert_eq!(first, 1);
    let err = adt_int::int4pl(a[first], b[first]).unwrap_err();
    assert_eq!(err.message(), "integer out of range");
    let mut clean = 0u64;
    checked::set_flag(&mut clean, 0, checked::div_i32(1, 0).1, false);
    assert_eq!(clean, 0);
    assert_eq!(checked::first_dirty(clean), None);
    let _ = Family::ByVal;
}
