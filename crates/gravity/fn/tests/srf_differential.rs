//! The SRF library's differential (docs/gravity/lower.md §12.3): every Rust-bodied entry of
//! `gravity_fn::srf::ENTRIES` driven through its open / step over a case corpus — NULL arguments,
//! empty sets, negative and zero steps, malformed input, the collation-bearing forms — against
//! the function manager's set-returning protocol on the same arguments (`execSRF.c`'s
//! ValuePerCall loop over `FmgrInfo::invoke` with a `ReturnSetInfo`): the rows, their NULLs and
//! the columns of a composite row byte-identical as payloads, an error identical in message and
//! sqlstate. The emitted kinds (the integer series, `generate_subscripts`) are the Rust model
//! `lower2/tests/srf_shapes.rs` proves the IR against, compared to the fmgr the same way; `unnest`'s
//! walk is `array_iter_next`'s and its fmgr form needs the call expression (no oracle here).

mod common;

use std::collections::HashMap;

use datum::Datum;
use fmgr::{ExprDoneCond, FmgrInfo, LocalFcinfo, ReturnSetInfo, SetFunctionReturnMode, SFRM_Materialize, SFRM_ValuePerCall};
use gravity_abi::Ctx;
use gravity_fn::srf::{Entry, ENTRIES, OUT_WORDS, STATE_BYTES};
use gravity_fn::ErrSlot;

type Row = Vec<Option<Vec<u8>>>;
type Rows = Result<Vec<Row>, (String, i32)>;

const TEXTOID: u32 = 25;
const INT4OID: u32 = 23;
const DEFAULT_COLLATION: u32 = 100;
/// An unbounded set (an infinite series) is the same outcome on both sides.
const CAP: usize = 10_000;

/// A case's argument: NULL, a by-value word, or a typed literal through the type's input function.
#[derive(Clone)]
enum A {
    N,
    I(i64),
    T(&'static str, &'static str),
}

struct Lits {
    types: HashMap<String, common::Ty>,
    procs: HashMap<u32, common::Proc>,
    keep: Vec<Vec<u8>>,
}

impl Lits {
    /// The datum of a literal of type `t` (a by-value word, or an image kept alive by the test).
    fn datum(&mut self, t: &str, lit: &str) -> Datum {
        if t == "text" {
            let img = common::image(lit.as_bytes());
            self.keep.push(img);
            return common::image_datum(self.keep.last().unwrap());
        }
        let ty = &self.types[t];
        let proc_ = self.procs.values().find(|p| p.prosrc == ty.input || p.name == ty.input).unwrap_or_else(|| panic!("no input function for {t}"));
        let nargs = proc_.args.len();
        let mut call = gravity_fn::FmgrCall::bind(proc_.oid, nargs, 0).unwrap();
        let mut c = lit.as_bytes().to_vec();
        c.push(0);
        let mut args = vec![Datum::from_usize(c.as_ptr() as usize)];
        if nargs == 3 {
            args.push(Datum::from_u32(self.types.get(&ty.elem).map_or(0, |e| e.oid)));
            args.push(Datum::from_i32(-1));
        }
        let v = call.call_site(&args, &vec![false; nargs]).unwrap_or_else(|e| panic!("{t} input of {lit:?}: {}", e.message()));
        if common::ret_byval(t, &self.types) {
            return v.value;
        }
        let p = v.value.as_usize() as *const u8;
        // SAFETY: the input function's result is a live image of its type.
        let n = unsafe { common::result_len(p, t, &self.types) };
        self.keep.push(unsafe { std::slice::from_raw_parts(p, n) }.to_vec());
        common::image_datum(self.keep.last().unwrap())
    }

    fn args(&mut self, case: &[A]) -> Vec<Option<Datum>> {
        case.iter()
            .map(|a| match a {
                A::N => None,
                A::I(v) => Some(Datum::from_i64(*v)),
                A::T(t, l) => Some(self.datum(t, l)),
            })
            .collect()
    }

    fn int4_array(&mut self, elems: &[Option<i32>]) -> Datum {
        let cx: &'static mut mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new_bump("srf arrays")));
        let vals: Vec<Datum> = elems.iter().map(|e| Datum::from_i32(e.unwrap_or(0))).collect();
        let nulls: Vec<bool> = elems.iter().map(|e| e.is_none()).collect();
        let img = arrayfuncs::construct_md_array(cx.mcx(), &vals, Some(&nulls), 1, &[elems.len() as i32], &[1], INT4OID, 4, true, b'i').unwrap();
        self.keep.push(img.to_vec());
        common::image_datum(self.keep.last().unwrap())
    }
}

/// A varlena datum's payload (the header stripped: the fmgr side's composite rows hold short-header
/// images where the library writes 4-byte headers).
unsafe fn payload(p: *const u8) -> Vec<u8> {
    if *p & 1 == 1 {
        let len = (*p >> 1) as usize;
        std::slice::from_raw_parts(p.add(1), len - 1).to_vec()
    } else {
        let n = (p.cast::<u32>().read_unaligned() >> 2) as usize;
        std::slice::from_raw_parts(p.add(4), n - 4).to_vec()
    }
}

/// The by-value output columns per entry (a timestamp series); every other column is a varlena.
fn byval_cols(oid: u32) -> bool {
    matches!(oid, 938 | 939 | 6274 | 1066 | 1067 | 1068 | 1069 | 1191 | 1192)
}

fn cell(d: i64, byval: bool) -> Vec<u8> {
    if byval {
        d.to_ne_bytes().to_vec()
    } else {
        // SAFETY: a varlena datum of the row.
        unsafe { payload(d as usize as *const u8) }
    }
}

/// The two columns of a composite row image (`heap_deform_tuple` over two varlena attributes of
/// `int` alignment: a pad byte is zero, a short header is never padded).
unsafe fn composite_cols(d: i64) -> [Option<Vec<u8>>; 2] {
    let p = d as usize as *const u8;
    let infomask = p.add(20).cast::<u16>().read_unaligned();
    let hoff = *p.add(22) as usize;
    let hasnull = infomask & 0x0001 != 0;
    let bitmap = *p.add(23);
    let mut off = hoff;
    let mut out = [None, None];
    for (i, o) in out.iter_mut().enumerate() {
        if hasnull && bitmap & (1 << i) == 0 {
            continue;
        }
        if *p.add(off) == 0 {
            off = (off + 3) & !3;
        }
        let q = p.add(off);
        *o = Some(payload(q));
        off += types_tuple::varatt::varsize_any(q);
    }
    out
}

fn err_of(e: &types_error::PgError) -> (String, i32) {
    (e.message().to_owned(), e.sqlstate().0)
}

/// D3128: the result columns of the json / jsonb `_each` and `_array_elements` families, C's InitMaterializedSRF
/// functions (rows into `setResult`); the oracle reads their store through these columns.
fn materialized_cols(oid: u32) -> Option<&'static [u32]> {
    const JSONOID: u32 = 114;
    const JSONBOID: u32 = 3802;
    Some(match oid {
        3208 => &[TEXTOID, JSONBOID], // jsonb_each
        3932 => &[TEXTOID, TEXTOID],  // jsonb_each_text
        3958 => &[TEXTOID, JSONOID],  // json_each
        3959 => &[TEXTOID, TEXTOID],  // json_each_text
        3219 => &[JSONBOID],          // jsonb_array_elements
        3465 => &[TEXTOID],           // jsonb_array_elements_text
        3955 => &[JSONOID],           // json_array_elements
        3969 => &[TEXTOID],           // json_array_elements_text
        _ => return None,
    })
}

/// The fmgr side: `ExecMakeFunctionResultSet`'s loop in ValuePerCall mode, or `ExecMakeTableFunctionResult`'s
/// Materialize arm for a function that materializes (`materialized_cols`).
struct Oracle {
    oid: u32,
    flinfo: FmgrInfo,
    fc: Box<LocalFcinfo<8>>,
    rs: Box<ReturnSetInfo>,
    cx: Box<mcx::MemoryContext>,
}

impl Oracle {
    fn new(oid: u32, nargs: usize, coll: u32) -> Oracle {
        let flinfo = fmgr_core::fmgr_info(oid).unwrap();
        let cx = Box::new(mcx::MemoryContext::new_bump("srf oracle"));
        let mut fc = Box::new(LocalFcinfo::<8>::new(coll));
        fc.rearm(coll);
        fc.nargs = nargs as i16;
        // SAFETY: the context outlives the frame (drop order).
        unsafe { fc.set_result_mcx(cx.mcx()) };
        Oracle { oid, flinfo, fc, rs: Box::new(ReturnSetInfo::new(SFRM_ValuePerCall)), cx }
    }

    fn rows(&mut self, args: &[Option<Datum>], cols: usize, byval: bool) -> Rows {
        for (i, a) in args.iter().enumerate() {
            match a {
                Some(d) => self.fc.set_arg(i, *d),
                None => self.fc.set_arg_null(i),
            }
        }
        if self.flinfo.fn_strict && args.iter().any(Option::is_none) {
            return Ok(Vec::new());
        }
        if let Some(types) = materialized_cols(self.oid) {
            return self.materialized(types);
        }
        let mut out = Vec::new();
        loop {
            self.fc.isnull = false;
            self.rs.returnMode = SetFunctionReturnMode::ValuePerCall;
            self.rs.isDone = ExprDoneCond::ExprSingleResult;
            self.fc.resultinfo = self.rs.as_fmnode_ptr();
            let r = match self.flinfo.invoke(&mut *self.fc) {
                Ok(d) => d,
                Err(e) => {
                    self.flinfo.fn_extra = None;
                    return Err(err_of(&e));
                }
            };
            assert!(matches!(self.rs.returnMode, SetFunctionReturnMode::ValuePerCall), "the oracle drives ValuePerCall only");
            if self.rs.isDone == ExprDoneCond::ExprEndResult {
                break;
            }
            let row: Row = if self.fc.isnull {
                vec![None; cols]
            } else if cols == 2 {
                // SAFETY: a composite row datum of the set.
                unsafe { composite_cols(r.as_i64()) }.to_vec()
            } else {
                vec![Some(cell(r.as_i64(), byval))]
            };
            out.push(row);
            if self.rs.isDone == ExprDoneCond::ExprSingleResult {
                break;
            }
            if out.len() > CAP {
                self.flinfo.fn_extra = None;
                return Err(("row cap".to_owned(), 0));
            }
        }
        self.cx.reset();
        // SAFETY: the reset context is the frame's result context again.
        unsafe { self.fc.set_result_mcx(self.cx.mcx()) };
        Ok(out)
    }

    /// One call with Materialize allowed and the expected row armed (`MAT_SRF_USE_EXPECTED_DESC`), then the store's
    /// rows column by column (a varlena column's payload, as `cell`).
    fn materialized(&mut self, types: &[u32]) -> Rows {
        let dcx = mcx::MemoryContext::new("srf oracle rows");
        let m = dcx.mcx();
        let mut desc = tupdesc::CreateTemplateTupleDesc(m, types.len() as i32).unwrap();
        for (i, &t) in types.iter().enumerate() {
            tupdesc::TupleDescInitEntry(&mut desc, (i + 1) as i16, None, t, -1, 0).unwrap();
        }
        let desc = std::rc::Rc::new(desc);
        self.fc.isnull = false;
        self.rs.allowedModes = SFRM_ValuePerCall | SFRM_Materialize;
        self.rs.returnMode = SetFunctionReturnMode::ValuePerCall;
        self.rs.isDone = ExprDoneCond::ExprSingleResult;
        self.rs.expectedDesc = Some(core::ptr::NonNull::from(&*desc).cast());
        self.rs.setResult = None;
        self.fc.resultinfo = self.rs.as_fmnode_ptr();
        let r = self.flinfo.invoke(&mut *self.fc);
        self.rs.expectedDesc = None;
        self.flinfo.fn_extra = None;
        if let Err(e) = r {
            return Err(err_of(&e));
        }
        assert!(matches!(self.rs.returnMode, SetFunctionReturnMode::Materialize), "a materializing SRF answered ValuePerCall");
        let mut out = Vec::new();
        if let Some(store) = self.rs.setResult.take() {
            let mut store = *store.downcast::<tuplestore::Tuplestore>().expect("setResult is a Tuplestore");
            let mut slot = exectuples::make_tuple_table_slot(m, types_slot::TupleSlotKind::MinimalTuple, Some(desc.clone()));
            while store.gettupleslot(true, true, &mut slot, m).unwrap() {
                exectuples::slot_getallattrs(&mut slot);
                let base = slot.base();
                out.push((0..types.len()).map(|i| (!base.tts_isnull[i]).then(|| cell(base.tts_values[i].as_i64(), false))).collect());
                if out.len() > CAP {
                    return Err(("row cap".to_owned(), 0));
                }
            }
            store.end();
        }
        self.cx.reset();
        // SAFETY: the reset context is the frame's result context again.
        unsafe { self.fc.set_result_mcx(self.cx.mcx()) };
        Ok(out)
    }
}

/// The library side: the entry's open then step until exhausted, over a ctx whose arena is the test's.
struct Lib {
    ctx: Box<Ctx>,
    arena: Vec<u8>,
}

impl Lib {
    fn new() -> Lib {
        // SAFETY: the ctx block is plain data; the fields the library reads are set below.
        let mut ctx: Box<Ctx> = Box::new(unsafe { core::mem::zeroed() });
        let mut arena = vec![0u8; 32 << 20];
        ctx.arena_cur = arena.as_mut_ptr();
        // SAFETY: one past the end of the arena.
        ctx.arena_end = unsafe { arena.as_mut_ptr().add(arena.len()) };
        Lib { ctx, arena }
    }

    fn rows(&mut self, e: &Entry, args: &[Option<Datum>], coll: u32) -> Rows {
        self.ctx.arena_cur = self.arena.as_mut_ptr();
        let byval = byval_cols(e.oid);
        if e.strict && args.iter().any(Option::is_none) {
            return Ok(Vec::new());
        }
        let mut state = vec![0u8; STATE_BYTES];
        let av: Vec<i64> = args.iter().map(|a| a.map_or(0, |d| d.as_i64())).collect();
        let an: Vec<u8> = args.iter().map(|a| u8::from(a.is_none())).collect();
        let mut err = ErrSlot::NONE;
        let aux = if e.coll { coll as i64 } else { e.aux };
        let ctx: *mut Ctx = &mut *self.ctx;
        // SAFETY: the library's contract — a bound ctx, the state cell and buffers this frame's.
        unsafe {
            let st = (e.open)(&mut err, ctx, state.as_mut_ptr(), av.as_ptr(), an.as_ptr(), av.len() as i64, aux);
            if st < 0 {
                return Err(err_of(&err.take().expect("ERROR without an error")));
            }
            let mut out = Vec::new();
            loop {
                let mut o = [0i64; OUT_WORDS];
                let r = (e.step)(&mut err, ctx, state.as_mut_ptr(), o.as_mut_ptr());
                if r < 0 {
                    return Err(err_of(&err.take().expect("ERROR without an error")));
                }
                if r == 0 {
                    break;
                }
                let row: Row = (0..e.cols as usize).map(|c| if o[2 * c + 1] != 0 { None } else { Some(cell(o[2 * c], byval)) }).collect();
                out.push(row);
                if out.len() > CAP {
                    return Err(("row cap".to_owned(), 0));
                }
            }
            Ok(out)
        }
    }
}

/// The model of the emitted kinds (srf_shapes.rs's): the integer series and the subscripts.
fn model(oid: u32, args: &[Option<Datum>]) -> Rows {
    if args.iter().any(Option::is_none) {
        return Ok(Vec::new());
    }
    let w: Vec<i64> = args.iter().map(|a| a.unwrap().as_i64()).collect();
    let series = |mut cur: i64, fin: i64, mut step: i64, width: u8| -> Rows {
        if step == 0 {
            return Err(("step size cannot equal zero".to_owned(), types_error::ERRCODE_INVALID_PARAMETER_VALUE.0));
        }
        let mut out = Vec::new();
        while (step > 0 && cur <= fin) || (step < 0 && cur >= fin) {
            out.push(vec![Some(cur.to_ne_bytes().to_vec())]);
            let next = cur.wrapping_add(step);
            let ovf = if width == 8 { cur.checked_add(step).is_none() } else { next != next as i32 as i64 };
            if ovf {
                step = 0;
            } else {
                cur = next;
            }
        }
        Ok(out)
    };
    match oid {
        1066 => series(w[0], w[1], 1, 4),
        1067 => series(w[0], w[1], w[2], 4),
        1068 => series(w[0], w[1], 1, 8),
        1069 => series(w[0], w[1], w[2], 8),
        1191 | 1192 => {
            let p = w[0] as usize as *const u8;
            // SAFETY: a 1-D int4[] image the test built.
            let (ndim, n, lb) = unsafe { (p.add(4).cast::<i32>().read_unaligned(), p.add(16).cast::<i32>().read_unaligned(), p.add(20).cast::<i32>().read_unaligned()) };
            let dim = w[1];
            if ndim == 0 || dim < 1 || dim > ndim as i64 {
                return Ok(Vec::new());
            }
            let reverse = oid == 1192 && w[2] != 0;
            let (lo, hi) = (lb as i64, lb as i64 + n as i64 - 1);
            if reverse { series(hi, lo, -1, 4) } else { series(lo, hi, 1, 4) }
        }
        _ => unreachable!(),
    }
}

fn cases(oid: u32) -> Vec<Vec<A>> {
    use A::*;
    let ts = |s: &'static str| T("timestamp", s);
    let tz = |s: &'static str| T("timestamptz", s);
    let iv = |s: &'static str| T("interval", s);
    let nu = |s: &'static str| T("numeric", s);
    let tx = |s: &'static str| T("text", s);
    let jb = |s: &'static str| T("jsonb", s);
    let js = |s: &'static str| T("json", s);
    let jp = |s: &'static str| T("jsonpath", s);
    match oid {
        938 => vec![
            vec![ts("2024-01-01"), ts("2024-01-03"), iv("1 day")],
            vec![ts("2024-01-03"), ts("2024-01-01"), iv("-1 day")],
            vec![ts("2024-01-03"), ts("2024-01-01"), iv("1 day")],
            vec![ts("2024-01-01"), ts("2024-01-01"), iv("1 hour")],
            vec![ts("2024-01-31 23:00"), ts("2024-03-01"), iv("1 month")],
            vec![ts("2024-01-01"), ts("2024-01-02"), iv("0")],
            vec![ts("2024-01-01"), ts("2024-01-02"), iv("infinity")],
            vec![ts("2024-01-01"), ts("infinity"), iv("1 day")],
            vec![ts("-infinity"), ts("2024-01-02"), iv("1 day")],
            vec![N, ts("2024-01-02"), iv("1 day")],
            vec![ts("2024-01-01"), N, iv("1 day")],
            vec![ts("2024-01-01"), ts("2024-01-02"), N],
            vec![ts("294276-12-31 23:00"), ts("294276-12-31 23:59:59"), iv("30 min")],
        ],
        939 => vec![
            vec![tz("2024-01-01 00:00+00"), tz("2024-01-03 00:00+00"), iv("1 day")],
            vec![tz("2024-03-09 12:00+00"), tz("2024-03-11 12:00+00"), iv("1 day")],
            vec![tz("2024-01-03 00:00+00"), tz("2024-01-01 00:00+00"), iv("-12 hours")],
            vec![tz("2024-01-01 00:00+00"), tz("2024-01-02 00:00+00"), iv("0")],
            vec![N, tz("2024-01-02 00:00+00"), iv("1 day")],
        ],
        6274 => vec![
            vec![tz("2024-03-09 12:00+00"), tz("2024-03-11 12:00+00"), iv("1 day"), tx("UTC")],
            vec![tz("2024-03-09 12:00+00"), tz("2024-03-11 12:00+00"), iv("1 day"), tx("America/New_York")],
            vec![tz("2024-03-09 12:00+00"), tz("2024-03-11 12:00+00"), iv("1 day"), tx("Nowhere/Land")],
            vec![tz("2024-03-09 12:00+00"), tz("2024-03-11 12:00+00"), iv("1 day"), N],
        ],
        3259 => vec![vec![nu("1"), nu("3")], vec![nu("3"), nu("1")], vec![nu("1.5"), nu("1.5")], vec![nu("NaN"), nu("3")], vec![nu("1"), nu("NaN")], vec![nu("1"), nu("Infinity")], vec![N, nu("3")], vec![nu("1"), N]],
        3260 => vec![vec![nu("1"), nu("3"), nu("0.5")], vec![nu("3"), nu("1"), nu("-1")], vec![nu("1"), nu("3"), nu("0")], vec![nu("1"), nu("3"), nu("NaN")], vec![nu("1"), nu("3"), nu("-Infinity")], vec![nu("1"), nu("3"), nu("-1")], vec![nu("1"), nu("3"), N], vec![nu("0.1"), nu("0.35"), nu("0.05")]],
        2763 => vec![vec![tx("abcabc"), tx("b")], vec![tx("abc"), tx("(x)")], vec![tx("abc"), tx("(")], vec![tx(""), tx("")], vec![tx("abc"), tx("(a)(b)?(z)?")], vec![tx("naïve café"), tx("(é)")], vec![N, tx("b")], vec![tx("abc"), N]],
        2764 => vec![vec![tx("abcabc"), tx("b"), tx("g")], vec![tx("aXbXc"), tx("x"), tx("gi")], vec![tx("abc"), tx("(a)(b)?(z)?"), tx("g")], vec![tx("abc"), tx("b"), tx("z")], vec![tx("a1b22c333"), tx("\\d+"), tx("g")], vec![tx("abc"), tx(""), tx("g")], vec![tx("abc"), tx("b"), N]],
        2765 => vec![vec![tx("a,b,c"), tx(",")], vec![tx("abc"), tx("")], vec![tx(""), tx(",")], vec![tx("a1b2"), tx("\\d")], vec![tx("abc"), tx("(")], vec![tx(",a,"), tx(",")], vec![N, tx(",")]],
        2766 => vec![vec![tx("a1b2"), tx("\\d"), tx("i")], vec![tx("a,b"), tx(","), tx("g")], vec![tx("aXbxc"), tx("x"), tx("i")], vec![tx("a b"), tx(" "), tx("q")], vec![tx("a b"), tx(" "), N]],
        6160 => vec![vec![tx("a b c"), tx(" ")], vec![tx("abc"), N], vec![tx("abc"), tx("")], vec![tx(""), tx(" ")], vec![N, tx(" ")], vec![tx("a,b,,c"), tx(",")], vec![tx("naïve café"), N], vec![tx("a||b"), tx("||")]],
        6161 => vec![vec![tx("a,b,,c"), tx(","), tx("")], vec![tx("a,b"), tx(","), tx("b")], vec![tx("a b"), tx(" "), N], vec![tx("abc"), N, tx("b")], vec![N, tx(","), tx("")], vec![tx("a,b"), tx(","), N]],
        3219 | 3465 => vec![vec![jb("[1, \"a\", null, {\"b\": 2}, [3], true, 1.50]")], vec![jb("[]")], vec![jb("{\"a\": 1}")], vec![jb("1")], vec![jb("\"x\"")], vec![jb("null")], vec![N]],
        3208 | 3932 => vec![vec![jb("{\"a\": 1, \"b\": \"x\", \"c\": null, \"d\": {\"e\": [1]}, \"f\": true}")], vec![jb("{}")], vec![jb("[1]")], vec![jb("1")], vec![jb("{\"k\": \"naïve\"}")], vec![N]],
        3931 => vec![vec![jb("{\"b\": 1, \"a\": 2, \"aa\": 3}")], vec![jb("{}")], vec![jb("[1]")], vec![jb("1")], vec![N]],
        3955 | 3969 => vec![vec![js("[1, \"a\", null, {\"b\": 2}, [3], true, 1.50]")], vec![js("[]")], vec![js("{\"a\": 1}")], vec![js("1")], vec![js("\"x\"")], vec![N]],
        3958 | 3959 => vec![vec![js("{\"a\": 1, \"b\": \"x\", \"c\": null, \"d\": {\"e\": [1]}}")], vec![js("{}")], vec![js("[1]")], vec![js("1")], vec![N]],
        3957 => vec![vec![js("{\"b\": 1, \"a\": 2}")], vec![js("{}")], vec![js("[1]")], vec![js("1")], vec![N]],
        4006 | 1179 => vec![
            vec![jb("[1, 2, 3]"), jp("$[*]"), jb("{}"), I(0)],
            vec![jb("{\"a\": [1, 2]}"), jp("$.a[*] ? (@ > 1)"), jb("{}"), I(0)],
            vec![jb("{\"a\": 1}"), jp("$.b"), jb("{}"), I(0)],
            vec![jb("{\"a\": 1}"), jp("strict $.b"), jb("{}"), I(0)],
            vec![jb("[1, 2, 3]"), jp("$[*] ? (@ > $x)"), jb("{\"x\": 1}"), I(0)],
            vec![jb("1"), jp("strict $.a"), jb("{}"), I(1)],
            vec![jb("1"), jp("strict $.a"), jb("{}"), I(0)],
            vec![jb("\"2024-01-01\""), jp("$.datetime()"), jb("{}"), I(0)],
            vec![jb("[1, 2]"), jp("$[*]"), jb("[1]"), I(0)],
            vec![N, jp("$"), jb("{}"), I(0)],
            vec![jb("1"), N, jb("{}"), I(0)],
            vec![jb("1"), jp("$"), N, I(0)],
        ],
        _ => Vec::new(),
    }
}

static SEAMS: std::sync::Once = std::sync::Once::new();
static TYPES: std::sync::OnceLock<HashMap<u32, common::Ty>> = std::sync::OnceLock::new();

fn type_shape(typid: u32) -> types_error::PgResult<Option<types_tuple::PgTypeShape>> {
    Ok(TYPES.get().unwrap().get(&typid).map(|t| types_tuple::PgTypeShape { typlen: t.len as i16, typbyval: t.byval, typalign: t.align as i8, typstorage: if t.len == -1 { b'x' as i8 } else { b'p' as i8 }, typcollation: if t.collation { 100 } else { 0 } }))
}

/// The typed bodies' seams, plus the record registry a composite row's rowtype is stamped by.
fn install() {
    SEAMS.call_once(|| {
        common::install_seams();
        typcache_seams::assign_record_type_typmod::set(|d| {
            d.tdtypmod = 0;
            Ok(())
        });
        // the composite row's tupdesc (`jsonb_each`'s `TupleDescInitEntry`) reads the type shape
        TYPES.get_or_init(|| common::types().into_values().map(|t| (t.oid, t)).collect());
        syscache_seams::lookup_pg_type_shape::set(type_shape);
        // D3128: json(b)_each are InitMaterializedSRF(MAT_SRF_BLESS) as in C, so get_call_result_type
        // reads pg_proc's OUT columns (pg_proc.dat: key text, value json / jsonb / text)
        syscache_seams::lookup_pg_proc_shape::set(|funcid| {
            Ok(each_out_args(funcid).map(|_| syscache_seams::PgProcShape {
                pronamespace: 11,
                prorettype: 2249,
                provariadic: 0,
                prosupport: 0,
                prolang: 12,
                pronargs: 1,
                prokind: b'f' as i8,
                provolatile: b'i' as i8,
                proparallel: b's' as i8,
                proretset: true,
                proisstrict: true,
                proleakproof: false,
                prosecdef: false,
                proconfig_isnull: true,
            }))
        });
        syscache_seams::lookup_pg_proc_signature::set(|mcx, funcid| {
            let Some(all) = each_out_args(funcid) else { return Ok(None) };
            let mut args = mcx::PgVec::new_in(mcx);
            args.push(all[0]);
            Ok(Some((2249, args)))
        });
        syscache_seams::pg_proc_result_arrays::set(|mcx, funcid| {
            let Some(all) = each_out_args(funcid) else { return Ok(None) };
            let (mut types, mut modes, mut names) = (mcx::PgVec::new_in(mcx), mcx::PgVec::new_in(mcx), mcx::PgVec::new_in(mcx));
            for (i, (&t, n)) in all.iter().zip(["from_json", "key", "value"]).enumerate() {
                types.push(t);
                modes.push(if i == 0 { b'i' as i8 } else { b'o' as i8 });
                names.push(mcx::PgString::from_str_in(n, mcx)?);
            }
            Ok(Some(syscache_seams::PgProcResultArraysShape { proallargtypes: Some(types), proargmodes: Some(modes), proargnames: Some(names) }))
        });
    });
}

/// pg_proc.dat's proallargtypes of the four `_each` functions (one IN, two OUT).
fn each_out_args(funcid: u32) -> Option<[u32; 3]> {
    const JSONOID: u32 = 114;
    const JSONBOID: u32 = 3802;
    Some(match funcid {
        3208 => [JSONBOID, TEXTOID, JSONBOID],
        3932 => [JSONBOID, TEXTOID, TEXTOID],
        3958 => [JSONOID, TEXTOID, JSONOID],
        3959 => [JSONOID, TEXTOID, TEXTOID],
        _ => return None,
    })
}

/// One test: the session time zone is per thread (`pg_timezone_initialize` in `install_seams`).
#[test]
fn srf_library_differential() {
    library_entries_match_the_fmgr_set_protocol();
    emitted_kinds_model_matches_the_fmgr();
}

fn library_entries_match_the_fmgr_set_protocol() {
    install();
    let mut lits = Lits { types: common::types(), procs: common::procs(), keep: Vec::new() };
    let mut lib = Lib::new();
    let mut n = 0;
    let mut diffs = Vec::new();
    for e in ENTRIES.iter() {
        let cs = cases(e.oid);
        assert!(!cs.is_empty(), "no cases for {} ({})", e.name, e.oid);
        for c in &cs {
            if std::env::var_os("SRF_TRACE").is_some() {
                eprintln!("{} ({}) {:?}", e.name, e.oid, c.iter().map(|a| match a { A::N => "NULL".to_owned(), A::I(v) => v.to_string(), A::T(_, l) => l.to_string() }).collect::<Vec<_>>());
            }
            let args = lits.args(c);
            let mut oracle = Oracle::new(e.oid, args.len(), DEFAULT_COLLATION);
            let want = oracle.rows(&args, e.cols as usize, byval_cols(e.oid));
            let got = lib.rows(e, &args, DEFAULT_COLLATION);
            n += 1;
            if want != got {
                diffs.push(format!("{} ({}) case {:?}:\n  fmgr {:?}\n  lib  {:?}", e.name, e.oid, c.iter().map(|a| match a { A::N => "NULL".to_owned(), A::I(v) => v.to_string(), A::T(_, l) => l.to_string() }).collect::<Vec<_>>(), want, got));
            }
        }
    }
    println!("{n} cases over {} entries", ENTRIES.len());
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}

fn emitted_kinds_model_matches_the_fmgr() {
    let mut lits = Lits { types: common::types(), procs: common::procs(), keep: Vec::new() };
    let arr = lits.int4_array(&[Some(1), None, Some(3)]);
    let empty = lits.int4_array(&[]);
    let s = |v: i64| Some(Datum::from_i64(v));
    let cases: Vec<(u32, Vec<Option<Datum>>)> = vec![
        (1066, vec![s(1), s(3)]),
        (1066, vec![s(3), s(1)]),
        (1066, vec![s(i32::MAX as i64 - 1), s(i32::MAX as i64)]),
        (1066, vec![None, s(3)]),
        (1067, vec![s(1), s(10), s(3)]),
        (1067, vec![s(10), s(1), s(-3)]),
        (1067, vec![s(1), s(3), s(0)]),
        (1067, vec![s(i32::MIN as i64 + 1), s(i32::MIN as i64), s(-1)]),
        (1068, vec![s(1), s(3)]),
        (1068, vec![s(i64::MAX - 1), s(i64::MAX)]),
        (1069, vec![s(1), s(10), s(4)]),
        (1069, vec![s(i64::MIN + 1), s(i64::MIN), s(-1)]),
        (1069, vec![s(1), s(3), s(0)]),
        (1191, vec![Some(arr), s(1)]),
        (1191, vec![Some(arr), s(2)]),
        (1191, vec![Some(empty), s(1)]),
        (1192, vec![Some(arr), s(1), s(1)]),
        (1192, vec![Some(arr), s(1), s(0)]),
        (1192, vec![Some(arr), None, s(1)]),
    ];
    let mut diffs = Vec::new();
    for (oid, args) in &cases {
        let mut oracle = Oracle::new(*oid, args.len(), DEFAULT_COLLATION);
        let want = oracle.rows(args, 1, true);
        let got = model(*oid, args);
        if want != got {
            diffs.push(format!("{oid} {:?}:\n  fmgr {want:?}\n  model {got:?}", args.iter().map(|a| a.map(|d| d.as_i64())).collect::<Vec<_>>()));
        }
    }
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}
