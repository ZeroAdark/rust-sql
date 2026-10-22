use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;

use datum::varlena::set_varsize_4b;
use datum::Datum;
use gravity_abi::Ctx;
use gravity_program::{Cmd, Code, Layout as PLayout, OutCol, Program, WritePlan};
use gravity_wire::datarow::FRAME_KEEP;
use gravity_wire::{completion, WireNode};
use mcx::{Mcx, MemoryContext, PgVec};
use printtup::{printtup_create_DR, DrPrinttup, SetRemoteDestReceiverParams};
use types_core::Oid;
use types_dest::CommandDest;
use types_error::PgResult;
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData};
use types_portal::{CachedPlanHandle, ParamListHandle, Portal, PortalCleanupHook, PortalData, PortalStatus, PortalStrategy, QueryCompletion, QueryDescHandle, QueryEnvHandle, StmtListHandle, TuplestoreHandle, CMDTAG_INSERT, CMDTAG_UNKNOWN};
use types_resowner::ResourceOwner;
use types_slot::{SlotData, TupleSlotKind, TupleTableSlot, VirtualTupleTableSlot};
use types_tuple::{CompactAttribute, FormData_pg_attribute, NameData, TupleDescData, TYPALIGN_INT};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TOKEN: Cell<u8> = const { Cell::new(0) };
}

fn token() -> usize {
    TOKEN.with(|t| t.as_ptr() as usize)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if token() == TRACKED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

const INT4OUT: Oid = 43;
const TEXTOUT: Oid = 47;
const INT4SEND: Oid = 2407;
const INT4OID: Oid = 23;
const INT2OID: Oid = 21;
const INT8OID: Oid = 20;
const INT2OUT: Oid = 39;
const INT8OUT: Oid = 461;
const BOOLOID: Oid = 16;
const FLOAT4OID: Oid = 700;
const FLOAT8OID: Oid = 701;
const BOOLOUT: Oid = 1243;
const FLOAT4OUT: Oid = 201;
const FLOAT8OUT: Oid = 215;
const TEXTOID: Oid = 25;
const DOMAINOID: Oid = 99923;
const OIDOID: Oid = 26;
const OIDOUT: Oid = 1799;
const NUMERICOID: Oid = 1700;
const NUMERICOUT: Oid = 1702;
const NUMERICSEND: Oid = 2918;

thread_local! {
    static SENT: RefCell<Vec<(u8, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
    static CONVERT: Cell<bool> = const { Cell::new(false) };
    static FMGR_INFO_CALLS: Cell<u32> = const { Cell::new(0) };
    static TLIST: RefCell<Vec<pquery_seams::TargetEntrySummary>> = const { RefCell::new(Vec::new()) };
    static OUTBUF: RefCell<[u8; 16]> = const { RefCell::new([0; 16]) };
    static NUMERIC_OUT_CALLS: Cell<u32> = const { Cell::new(0) };
    /// textout reads its datum as a varlena (the older tests hand it cstrings).
    static TEXT_VARLENA: Cell<bool> = const { Cell::new(false) };
}

/// The payload of a plain varlena (1-byte or 4-byte header).
fn varlena_payload<'a>(p: *const u8) -> &'a [u8] {
    // SAFETY: the tests' datums point at live images they built.
    unsafe {
        if *p & 1 == 1 {
            core::slice::from_raw_parts(p.add(1), (*p >> 1) as usize - 1)
        } else {
            let w = p.cast::<u32>().read_unaligned();
            core::slice::from_raw_parts(p.add(4), (w >> 2) as usize - 4)
        }
    }
}

/// numeric_send stand-in: the image's payload as the bytea (the binary road must reach the send function unchanged).
fn numericsend_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    let pay = varlena_payload(fcinfo.arg(0).as_usize() as *const u8);
    let mut img = Vec::with_capacity(4 + pay.len());
    img.extend_from_slice(&set_varsize_4b(4 + pay.len()));
    img.extend_from_slice(pay);
    Ok(Datum::from_usize(Box::leak(img.into_boxed_slice()).as_ptr() as usize))
}

/// numeric_out: PostgreSQL's (the adt_numeric port), counted.
fn numericout_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    NUMERIC_OUT_CALLS.with(|c| c.set(c.get() + 1));
    // detoasted as PostgreSQL's numeric_out does (PG_GETARG_NUMERIC): a 1-byte-header image is copied to an aligned buffer
    let pay = varlena_payload(fcinfo.arg(0).as_usize() as *const u8);
    let mut words = vec![0u16; pay.len().div_ceil(2)];
    // SAFETY: `words` holds at least `pay.len()` bytes.
    let aligned = unsafe {
        core::ptr::copy_nonoverlapping(pay.as_ptr(), words.as_mut_ptr().cast::<u8>(), pay.len());
        core::slice::from_raw_parts(words.as_ptr().cast::<u8>(), pay.len())
    };
    let mut out = Vec::new();
    adt_numeric::numeric_out_into(adt_numeric::Num::from_payload(aligned), &mut out);
    types_fmgr::out_cstring(fcinfo, out.len() + 1, |b| {
        b[..out.len()].copy_from_slice(&out);
        out.len()
    })
}

fn int4out_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    let mut v = fcinfo.arg(0).as_i32();
    OUTBUF.with(|b| {
        let mut b = b.borrow_mut();
        let neg = v < 0;
        let mut i = 15;
        b[i] = 0;
        loop {
            i -= 1;
            b[i] = b'0' + (v % 10).unsigned_abs() as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        if neg {
            i -= 1;
            b[i] = b'-';
        }
        Ok(Datum::from_usize(b.as_ptr() as usize + i))
    })
}

/// oidout under PostgreSQL's contract: the cstring fresh in the frame's result context (an unarmed frame raises).
fn oidout_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    let s = fcinfo.arg(0).as_u32().to_string();
    types_fmgr::out_cstring(fcinfo, s.len() + 1, |b| {
        b[..s.len()].copy_from_slice(s.as_bytes());
        s.len()
    })
}

fn textout_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    if !TEXT_VARLENA.with(Cell::get) {
        return Ok(fcinfo.arg(0));
    }
    // textout over a varlena datum (the blocks the fast text path frames)
    let s = varlena_payload(fcinfo.arg(0).as_usize() as *const u8).to_vec();
    types_fmgr::out_cstring(fcinfo, s.len() + 1, |b| {
        b[..s.len()].copy_from_slice(&s);
        s.len()
    })
}

fn int4send_fn(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut FunctionCallInfoBaseData) -> PgResult<Datum> {
    let mut img = Vec::with_capacity(8);
    img.extend_from_slice(&set_varsize_4b(8));
    img.extend_from_slice(&fcinfo.arg(0).as_i32().to_be_bytes());
    Ok(Datum::from_usize(Box::leak(img.into_boxed_slice()).as_ptr() as usize))
}

fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        pqcomm_seams::pq_putmessage::set(|msgtype, body| {
            SENT.with(|s| s.borrow_mut().push((msgtype, body.to_vec())));
            Ok(0)
        });
        // pre-framed messages (a block's DataRows): split back into (type, body) so every test sees the same message list
        pqcomm_seams::pq_putbytes::set(|bytes| {
            let mut at = 0;
            while at < bytes.len() {
                let len = u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]]) as usize;
                SENT.with(|s| s.borrow_mut().push((bytes[at], bytes[at + 5..at + 1 + len].to_vec())));
                at += 1 + len;
            }
            Ok(0)
        });
        mbutils_seams::pg_server_to_client::set(|mcx, s| {
            if CONVERT.with(|c| c.get()) {
                let upper: Vec<u8> = s.iter().map(|b| b.to_ascii_uppercase()).collect();
                Ok(Some(mcx::slice_in(mcx, &upper)?))
            } else {
                Ok(None)
            }
        });
        mbutils_seams::server_to_client_conversion_needed::set(|| CONVERT.with(|c| c.get()));
        pquery_seams::fetch_portal_target_list::set(|mcx, _portal| TLIST.with(|t| mcx::slice_in(mcx, &t.borrow())));
        lsyscache_seams::get_type_output_info::set(|oid| match oid {
            INT4OID | DOMAINOID => Ok((INT4OUT, false)),
            INT2OID => Ok((INT2OUT, false)),
            INT8OID => Ok((INT8OUT, false)),
            BOOLOID => Ok((BOOLOUT, false)),
            FLOAT4OID => Ok((FLOAT4OUT, false)),
            FLOAT8OID => Ok((FLOAT8OUT, false)),
            TEXTOID => Ok((TEXTOUT, true)),
            OIDOID => Ok((OIDOUT, false)),
            NUMERICOID => Ok((NUMERICOUT, true)),
            _ => panic!("get_type_output_info: unexpected oid {oid}"),
        });
        lsyscache_seams::get_type_binary_output_info::set(|oid| match oid {
            INT4OID | DOMAINOID => Ok((INT4SEND, false)),
            NUMERICOID => Ok((NUMERICSEND, true)),
            _ => panic!("get_type_binary_output_info: unexpected oid {oid}"),
        });
        lsyscache_seams::get_base_type_and_typmod::set(|typid, typmod| match typid {
            DOMAINOID => Ok((INT4OID, 7)),
            _ => Ok((typid, typmod)),
        });
        fmgr_seams::fmgr_info::set(|oid| {
            FMGR_INFO_CALLS.with(|c| c.set(c.get() + 1));
            match oid {
                INT4OUT => Ok(FmgrInfo::new(int4out_fn, INT4OUT, 1, true, false)),
                // never called: int2 / int8 take the node's fast path
                INT2OUT => Ok(FmgrInfo::new(int4out_fn, INT2OUT, 1, true, false)),
                INT8OUT => Ok(FmgrInfo::new(int4out_fn, INT8OUT, 1, true, false)),
                BOOLOUT | FLOAT4OUT | FLOAT8OUT => Ok(FmgrInfo::new(int4out_fn, oid, 1, true, false)),
                TEXTOUT => Ok(FmgrInfo::new(textout_fn, TEXTOUT, 1, true, false)),
                OIDOUT => Ok(FmgrInfo::new(oidout_fn, OIDOUT, 1, true, false)),
                NUMERICOUT => Ok(FmgrInfo::new(numericout_fn, NUMERICOUT, 1, true, false)),
                NUMERICSEND => Ok(FmgrInfo::new(numericsend_fn, NUMERICSEND, 1, true, false)),
                INT4SEND => Ok(FmgrInfo::new(int4send_fn, INT4SEND, 1, true, false)),
                _ => panic!("fmgr_info: unexpected oid {oid}"),
            }
        });
    });
}

fn take_sent() -> Vec<(u8, Vec<u8>)> {
    SENT.with(|s| s.borrow_mut().drain(..).collect())
}

struct Col {
    name: &'static str,
    typid: Oid,
    typmod: i32,
    typlen: i16,
    byval: bool,
    origtbl: Oid,
    origcol: i16,
}

const COLS: [Col; 3] = [
    Col { name: "id", typid: INT4OID, typmod: -1, typlen: 4, byval: true, origtbl: 16400, origcol: 2 },
    Col { name: "label", typid: TEXTOID, typmod: -1, typlen: -1, byval: false, origtbl: 16400, origcol: 5 },
    Col { name: "dom", typid: DOMAINOID, typmod: -1, typlen: 4, byval: true, origtbl: 0, origcol: 0 },
];

fn attr(c: &Col, i: i16) -> FormData_pg_attribute {
    let mut attname = NameData::default();
    attname.namestrcpy(c.name);
    FormData_pg_attribute { attname, attnum: i + 1, atttypid: c.typid, atttypmod: c.typmod, attlen: c.typlen, attbyval: c.byval, attalign: TYPALIGN_INT, ..Default::default() }
}

fn make_desc(mcx: Mcx<'_>) -> Rc<TupleDescData<'_>> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    for (i, c) in COLS.iter().enumerate() {
        let a = attr(c, i as i16);
        compact.push(CompactAttribute::populate_from(&a));
        attrs.push(a);
    }
    Rc::new(TupleDescData { natts: COLS.len() as i32, tdtypeid: 0, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs: compact, attrs })
}

fn make_slot<'mcx>(mcx: Mcx<'mcx>, desc: Rc<TupleDescData<'mcx>>, values: &[(Datum, bool)]) -> SlotData<'mcx> {
    let mut base = TupleTableSlot::new_in(mcx, TupleSlotKind::Virtual);
    base.set_descriptor(mcx, desc);
    for (i, &(v, isnull)) in values.iter().enumerate() {
        base.tts_values[i] = v;
        base.tts_isnull[i] = isnull;
    }
    base.tts_nvalid = values.len() as i16;
    base.mark_not_empty();
    SlotData::Virtual(VirtualTupleTableSlot { base, data: PgVec::new_in(mcx) })
}

fn make_portal<'mcx>(mcx: Mcx<'mcx>, formats: &[i16]) -> Portal<'mcx> {
    let mut fv = PgVec::new_in(mcx);
    for &f in formats {
        fv.push(f);
    }
    Portal::new(PortalData {
        name: mcx::PgString::new_in(mcx),
        prepStmtName: None,
        portalContext: None,
        plansource: types_portal::PlanSourceHandle::NULL,
        planContext: core::ptr::null_mut(),
        resowner: ResourceOwner::default(),
        cleanup: PortalCleanupHook::None,
        createSubid: 0,
        activeSubid: 0,
        createLevel: 0,
        sourceText: None,
        sourceTextOwned: None,
        commandTag: CMDTAG_UNKNOWN,
        qc: QueryCompletion::default(),
        stmts: StmtListHandle::NULL,
        cplan: CachedPlanHandle::NULL,
        portalParams: ParamListHandle::NULL,
        queryEnv: QueryEnvHandle::NULL,
        strategy: PortalStrategy::default(),
        cursorOptions: 0,
        status: PortalStatus::default(),
        portalPinned: false,
        autoHeld: false,
        queryDesc: QueryDescHandle::NULL,
        tupDesc: None,
        formats: fv,
        portalSnapshot: None,
        holdStore: TuplestoreHandle::NULL,
        holdContext: None,
        holdSnapshot: None,
        atStart: true,
        atEnd: false,
        portalPos: 0,
        creation_time: 0,
        visible: false,
        cursorStoreArmed: false,
        cursorStore: TuplestoreHandle::NULL,
        cursorFillExhausted: false,
        currentOfEligible: None,
        cursorCaptureBatch: false,
        gravity: 0,
        memory_owner: None,
        cursorTidStore: TuplestoreHandle::NULL,
    })
}

fn remote_receiver<'mcx>(mcx: Mcx<'mcx>, formats: &[i16]) -> DrPrinttup<'mcx> {
    let mut dr = printtup_create_DR(CommandDest::Remote);
    SetRemoteDestReceiverParams(&mut dr, make_portal(mcx, formats));
    dr
}

unsafe extern "C-unwind" fn noop(_ctx: *mut Ctx) -> i64 {
    0
}

fn program() -> Program {
    let out: Vec<OutCol> = COLS.iter().map(|c| OutCol::new(c.name, c.typid, c.typmod, c.typlen, c.byval).origin(c.origtbl, c.origcol)).collect();
    Program::new(PLayout { nsites: 0, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &out, WritePlan::SELECT, Code::hand(noop))
}

static TEXTS: [&[u8]; 3] = [b"alpha\0", b"\0", b"zed zed zed\0"];

fn rows() -> Vec<[(Datum, bool); 3]> {
    vec![
        [(Datum::from_i32(1), false), (Datum::from_usize(TEXTS[0].as_ptr() as usize), false), (Datum::from_i32(-7), false)],
        [(Datum::from_i32(0), false), (Datum::null(), true), (Datum::from_i32(2147483647), false)],
        [(Datum::null(), true), (Datum::from_usize(TEXTS[2].as_ptr() as usize), false), (Datum::null(), true)],
    ]
}

fn printtup_bytes(mcx: Mcx<'_>, formats: &[i16]) -> Vec<(u8, Vec<u8>)> {
    TLIST.with(|t| *t.borrow_mut() = COLS.iter().map(|c| pquery_seams::TargetEntrySummary { resjunk: false, resorigtbl: c.origtbl, resorigcol: c.origcol }).collect());
    let desc = make_desc(mcx);
    let mut dr = remote_receiver(mcx, formats);
    dr.startup(0, &desc).unwrap();
    for r in rows() {
        let mut slot = make_slot(mcx, desc.clone(), &r);
        dr.receive_slot(&mut slot).unwrap();
    }
    dr.shutdown();
    take_sent()
}

fn gravity_bytes(node: &mut WireNode, p: &Arc<Program>, formats: &[i16]) -> Vec<(u8, Vec<u8>)> {
    node.prepare(p, Some(formats)).unwrap();
    node.send_rowdesc().unwrap();
    let mut values = Vec::new();
    let mut isnull = Vec::new();
    for r in rows() {
        for (v, n) in r {
            values.push(v.as_u64());
            isnull.push(n as u8);
        }
    }
    // the runner's road (`Receiver::Wire`): the node sends and an error keeps its PgError (the SQLSTATE)
    node.reset().send_rows(&values, &isnull, 3).unwrap();
    assert_eq!(node.rows(), 3);
    take_sent()
}

#[test]
fn rowdescription_and_datarows_are_byte_identical_to_printtup() {
    install();
    let ctx = MemoryContext::new("wire-test");
    let mcx = ctx.mcx();
    let p = Arc::new(program());
    let mut node = WireNode::new();
    for formats in [&[0i16, 0, 0][..], &[1, 0, 1][..]] {
        for convert in [false, true] {
            CONVERT.with(|c| c.set(convert));
            let expected = printtup_bytes(mcx, formats);
            let got = gravity_bytes(&mut node, &p, formats);
            assert_eq!(got.len(), 4, "T + 3 D messages");
            assert_eq!(got, expected, "formats {formats:?} convert {convert}");
            assert_eq!(got[0].0, b'T');
            assert!(got[1..].iter().all(|(t, _)| *t == b'D'));
        }
    }
    let before = FMGR_INFO_CALLS.with(Cell::get);
    node.prepare(&p, Some(&[1, 0, 1])).unwrap();
    node.prepare(&p, Some(&[1, 0, 1])).unwrap();
    assert_eq!(FMGR_INFO_CALLS.with(Cell::get), before, "the output functions are resolved once per (program, formats)");
    node.prepare(&p, Some(&[0, 0, 0])).unwrap();
    assert_eq!(FMGR_INFO_CALLS.with(Cell::get), before + 3, "a format change re-resolves");
}

#[test]
fn completion_carries_the_tag_and_the_count() {
    let qc = completion(Cmd::Insert, 1);
    assert_eq!((qc.commandTag, qc.nprocessed), (CMDTAG_INSERT, 1));
    assert_eq!(completion(Cmd::Select, 20).nprocessed, 20);
    assert_eq!(completion(Cmd::Update, 0).commandTag, types_portal::CMDTAG_UPDATE);
    assert_eq!(completion(Cmd::Delete, 3).commandTag, types_portal::CMDTAG_DELETE);
}

#[test]
fn ten_thousand_text_rows_allocate_nothing() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("a", INT4OID, -1, 4, true), OutCol::new("b", INT4OID, -1, 4, true)];
    let p = Arc::new(Program::new(PLayout { nsites: 0, out_natts: 2, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &cols, WritePlan::SELECT, Code::hand(noop)));
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    let values: Vec<u64> = (0..128).map(|i| Datum::from_i32(i * 7 - 300).as_u64()).collect();
    let isnull = vec![0u8; 128];
    for _ in 0..3 {
        node.reset().send_rows(&values, &isnull, 64).unwrap();
    }
    take_sent();
    SENT.with(|s| s.borrow_mut().reserve(64 * 200));
    TRACKED.store(token(), Ordering::Relaxed);
    let before = ALLOCS.load(Ordering::Relaxed);
    let mut sent_allocs = 0;
    for _ in 0..(10_000 / 64 + 1) {
        let node = node.reset();
        let b0 = ALLOCS.load(Ordering::Relaxed);
        node.send_rows(&values, &isnull, 64).unwrap();
        let b1 = ALLOCS.load(Ordering::Relaxed);
        SENT.with(|s| s.borrow_mut().clear());
        sent_allocs += (b1 - b0).min(64);
    }
    let total = ALLOCS.load(Ordering::Relaxed) - before;
    TRACKED.store(0, Ordering::Relaxed);
    assert_eq!(total - sent_allocs, 0, "{total} allocations in ~10k rows ({sent_allocs} from the capture sink itself)");
    assert_eq!(sent_allocs, 64 * (10_000 / 64 + 1), "the capture sink copies every body (one Vec per message), nothing else allocates");
}

/// A framed block (`send_rows`, one `pq_putbytes`) is byte for byte the per-row messages (`send_row`, one `pq_putmessage` each):
/// int2 / int4 / int8 columns, NULLs, the widest negatives, zero.
#[test]
fn a_framed_block_is_the_per_row_messages_byte_for_byte() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("a", INT2OID, -1, 2, true), OutCol::new("b", INT4OID, -1, 4, true), OutCol::new("c", INT8OID, -1, 8, true)];
    let p = Arc::new(Program::new(PLayout { nsites: 0, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &cols, WritePlan::SELECT, Code::hand(noop)));
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    let rows: [[(u64, u8); 3]; 5] = [
        [(1, 0), (2, 0), (3, 0)],
        [((-32768i16 as u16) as u64, 0), ((i32::MIN as u32) as u64, 0), (i64::MIN as u64, 0)],
        [(0, 0), (0, 1), (i64::MAX as u64, 0)],
        [(0, 1), (0, 1), (0, 1)],
        [((32767u16) as u64, 0), ((i32::MAX as u32) as u64, 0), ((-1i64) as u64, 0)],
    ];
    let values: Vec<u64> = rows.iter().flatten().map(|c| c.0).collect();
    let isnull: Vec<u8> = rows.iter().flatten().map(|c| c.1).collect();
    take_sent();
    for r in 0..rows.len() {
        node.send_row(&values[r * 3..r * 3 + 3], &isnull[r * 3..r * 3 + 3]).unwrap();
    }
    let per_row = take_sent();
    node.send_rows(&values, &isnull, rows.len()).unwrap();
    let framed = take_sent();
    assert_eq!(per_row.len(), 5);
    assert_eq!(framed, per_row);
    assert_eq!(node.rows(), 10);
}

/// bool / float4 / float8 columns take the fast path too: the framed block is the per-row messages, and the text is `boolout` / `float4out` /
/// `float8out`'s (the shortest round-trip form under the default extra_float_digits, PostgreSQL's special spellings).
#[test]
fn bool_and_float_columns_frame_as_their_output_functions_print() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("d", BOOLOID, -1, 1, true), OutCol::new("r", FLOAT4OID, -1, 4, true), OutCol::new("f", FLOAT8OID, -1, 8, true)];
    let p = Arc::new(Program::new(PLayout { nsites: 0, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &cols, WritePlan::SELECT, Code::hand(noop)));
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    let f4 = |x: f32| x.to_bits() as u64;
    let f8 = |x: f64| x.to_bits();
    let rows: [([u64; 3], [u8; 3], [&[u8]; 3]); 5] = [
        ([1, f4(0.1), f8(1.5)], [0, 0, 0], [b"t", b"0.1", b"1.5"]),
        ([0, f4(f32::NAN), f8(f64::INFINITY)], [0, 0, 0], [b"f", b"NaN", b"Infinity"]),
        ([1, f4(-0.0), f8(-0.0)], [0, 0, 0], [b"t", b"-0", b"-0"]),
        ([0, f4(1e30), f8(1e300)], [0, 1, 0], [b"f", b"", b"1e+300"]),
        ([1, f4(16777216.0), f8(0.30000000000000004)], [0, 0, 0], [b"t", b"1.6777216e+07", b"0.30000000000000004"]),
    ];
    let values: Vec<u64> = rows.iter().flat_map(|r| r.0).collect();
    let isnull: Vec<u8> = rows.iter().flat_map(|r| r.1).collect();
    take_sent();
    for r in 0..rows.len() {
        node.send_row(&values[r * 3..r * 3 + 3], &isnull[r * 3..r * 3 + 3]).unwrap();
    }
    let per_row = take_sent();
    node.send_rows(&values, &isnull, rows.len()).unwrap();
    let framed = take_sent();
    assert_eq!(framed, per_row);
    for (r, (_, nulls, texts)) in rows.iter().enumerate() {
        let mut want = 3u16.to_be_bytes().to_vec(); // alloc: Cold
        for (t, n) in texts.iter().zip(nulls) {
            if *n != 0 {
                want.extend_from_slice(&(-1i32).to_be_bytes());
            } else {
                want.extend_from_slice(&(t.len() as u32).to_be_bytes());
                want.extend_from_slice(t);
            }
        }
        assert_eq!(framed[r], (b'D', want), "row {r}");
    }
}


/// A by-value column whose output function allocates its result (oidout, PostgreSQL's contract) frames through the node's
/// per-row send context: the frame is armed for every text column, not only varlena / binary / user ones (LAND 09-23:
/// catalog `SELECT oid ... \gset` raised "caller never armed the frame" with fallback on).
#[test]
fn a_by_value_column_whose_output_function_allocates_frames() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("oid", OIDOID, -1, 4, true)];
    let p = Arc::new(Program::new(PLayout { nsites: 0, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &cols, WritePlan::SELECT, Code::hand(noop)));
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    take_sent();
    node.send_row(&[Datum::from_u32(16384).as_u64()], &[0]).unwrap();
    let mut want = 1u16.to_be_bytes().to_vec(); // alloc: Cold
    want.extend_from_slice(&5u32.to_be_bytes());
    want.extend_from_slice(b"16384");
    assert_eq!(take_sent(), vec![(b'D', want)]);
}

/// PostgreSQL 18.6's `numeric::text` for each input (fixtures/numeric_out_pg18.tsv, from `SELECT s, s::numeric::text`): zero at every scale,
/// negatives, trailing zeros per dscale, scales 0 to 20 (round), the u64 and 10^37 / 10^38 boundaries, NaN and the infinities.
fn pg_numeric_vectors() -> Vec<(adt_numeric::NumericImage, String)> {
    include_str!("fixtures/numeric_out_pg18.tsv")
        .lines()
        .map(|l| {
            let (input, pg) = l.split_once('\t').expect("input <tab> PostgreSQL's text");
            // a round(x, n) row: the value PostgreSQL printed, read back (numeric_in keeps its digits and its dscale)
            let expr = if input.starts_with("round(") { pg } else { input };
            let img = adt_numeric::numeric_in(expr, -1, None).expect("numeric_in").expect("a value");
            (img, pg.to_string())
        })
        .collect()
}

/// The image's bytes with a 1-byte varlena header when it fits one (the form a stored short numeric has).
fn short_header(img: &adt_numeric::NumericImage) -> Option<Vec<u8>> {
    let pay = img.payload();
    (pay.len() + 1 < 128).then(|| {
        let mut v = vec![((pay.len() as u8 + 1) << 1) | 1];
        v.extend_from_slice(pay);
        v
    })
}

fn numeric_program(cols: &[OutCol]) -> Arc<Program> {
    Arc::new(Program::new(PLayout { nsites: 0, out_natts: cols.len() as _, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], cols, WritePlan::SELECT, Code::hand(noop)))
}

fn datarow(texts: &[Option<&[u8]>]) -> (u8, Vec<u8>) {
    let mut want = (texts.len() as u16).to_be_bytes().to_vec();
    for t in texts {
        match t {
            None => want.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(t) => {
                want.extend_from_slice(&(t.len() as u32).to_be_bytes());
                want.extend_from_slice(t);
            }
        }
    }
    (b'D', want)
}

/// V4R-29 num render: a text-format numeric column is PostgreSQL's text byte for byte, framed (`send_rows`) and per row (`send_row`), with a
/// 4-byte and a 1-byte varlena header; the fast path calls no output function for a finite value of at most 38 scaled digits, and NaN /
/// +-Infinity / a wider value take numeric_out.
#[test]
fn numeric_columns_print_as_postgres_does() {
    install();
    CONVERT.with(|c| c.set(false));
    let p = numeric_program(&[OutCol::new("n", NUMERICOID, -1, -1, false)]);
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    take_sent();
    let vecs = pg_numeric_vectors();
    assert!(vecs.len() >= 120, "{} vectors", vecs.len());
    // the fast path's bound: the scaled integer's digits (the integer part's unless it is "0", plus dscale) at most 38
    let slow = |pg: &str| {
        let (i, f) = pg.trim_start_matches('-').split_once('.').unwrap_or((pg.trim_start_matches('-'), ""));
        pg == "NaN" || pg.ends_with("Infinity") || (if i == "0" { 0 } else { i.len() }) + f.len() > 38
    };
    let mut fast_seen = 0;
    for (img, pg) in &vecs {
        let mut forms = vec![img.as_bytes().to_vec()];
        forms.extend(short_header(img));
        for bytes in &forms {
            let v = [bytes.as_ptr() as u64];
            let c0 = NUMERIC_OUT_CALLS.with(Cell::get);
            node.send_row(&v, &[0]).unwrap();
            let c1 = NUMERIC_OUT_CALLS.with(Cell::get);
            node.send_rows(&v, &[0], 1).unwrap();
            let c2 = NUMERIC_OUT_CALLS.with(Cell::get);
            let sent = take_sent();
            assert_eq!(sent, vec![datarow(&[Some(pg.as_bytes())]); 2], "{pg}");
            let want_calls = slow(pg) as u32;
            assert_eq!((c1 - c0, c2 - c1), (want_calls, want_calls), "numeric_out calls for {pg}");
            fast_seen += (want_calls == 0) as u32;
        }
    }
    assert!(fast_seen >= 200, "the fast path took {fast_seen} forms");
    // the 10^37 boundary: 38 digits render fast, 39 take numeric_out
    for (s, calls) in [("10000000000000000000000000000000000000", 0), ("-99999999999999999999999999999999999999", 0), ("100000000000000000000000000000000000000", 1), ("0.00000000000000000000000000000000000001", 0), ("0.000000000000000000000000000000000000001", 1)] {
        let img = adt_numeric::numeric_in(s, -1, None).unwrap().unwrap();
        let c0 = NUMERIC_OUT_CALLS.with(Cell::get);
        node.send_rows(&[img.as_bytes().as_ptr() as u64], &[0], 1).unwrap();
        assert_eq!(NUMERIC_OUT_CALLS.with(Cell::get) - c0, calls, "{s}");
        assert_eq!(take_sent(), vec![datarow(&[Some(s.as_bytes())])]);
    }
}

/// A block mixing every fast kind (int2 / int4 / int8 / bool / float4 / float8 / text / numeric) with NULLs frames byte for byte as its
/// per-row messages and as each output function prints: the numeric arm leaves the other kinds' bytes unchanged, and a block holding one
/// numeric the fast path does not take (NaN) goes row by row with the same bytes.
#[test]
fn every_fast_kind_frames_unchanged_beside_a_numeric() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [
        OutCol::new("a", INT2OID, -1, 2, true),
        OutCol::new("b", INT4OID, -1, 4, true),
        OutCol::new("c", INT8OID, -1, 8, true),
        OutCol::new("d", BOOLOID, -1, 1, true),
        OutCol::new("r", FLOAT4OID, -1, 4, true),
        OutCol::new("f", FLOAT8OID, -1, 8, true),
        OutCol::new("t", TEXTOID, -1, -1, false),
        OutCol::new("n", NUMERICOID, -1, -1, false),
    ];
    TEXT_VARLENA.with(|c| c.set(true));
    let p = numeric_program(&cols);
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    assert!(node.wire_format().is_none(), "a numeric column keeps the legs on the row path");
    take_sent();
    let text = |s: &str| {
        let mut v = vec![((s.len() as u8 + 1) << 1) | 1];
        v.extend_from_slice(s.as_bytes());
        v
    };
    let (t1, t2) = (text("alpha"), text(""));
    let nums = ["-1234.5600", "0.00", "99999999999999999999999999999999999999", "NaN"].map(|s| adt_numeric::numeric_in(s, -1, None).unwrap().unwrap());
    let f4 = |x: f32| x.to_bits() as u64;
    let rows: Vec<([u64; 8], [u8; 8], [Option<&[u8]>; 8])> = vec![
        ([(-32768i16 as u16) as u64, (i32::MIN as u32) as u64, i64::MIN as u64, 1, f4(0.1), 1.5f64.to_bits(), t1.as_ptr() as u64, nums[0].as_bytes().as_ptr() as u64], [0; 8],
            [Some(b"-32768"), Some(b"-2147483648"), Some(b"-9223372036854775808"), Some(b"t"), Some(b"0.1"), Some(b"1.5"), Some(b"alpha"), Some(b"-1234.5600")]),
        ([0, 0, 0, 0, 0, 0, 0, 0], [1, 0, 1, 0, 1, 0, 1, 1], [None, Some(b"0"), None, Some(b"f"), None, Some(b"0"), None, None]),
        ([7, 42, i64::MAX as u64, 1, f4(-0.0), f64::INFINITY.to_bits(), t2.as_ptr() as u64, nums[1].as_bytes().as_ptr() as u64], [0; 8],
            [Some(b"7"), Some(b"42"), Some(b"9223372036854775807"), Some(b"t"), Some(b"-0"), Some(b"Infinity"), Some(b""), Some(b"0.00")]),
        ([1, 2, 3, 0, f4(1e30), 1e300f64.to_bits(), t1.as_ptr() as u64, nums[2].as_bytes().as_ptr() as u64], [0; 8],
            [Some(b"1"), Some(b"2"), Some(b"3"), Some(b"f"), Some(b"1e+30"), Some(b"1e+300"), Some(b"alpha"), Some(b"99999999999999999999999999999999999999")]),
    ];
    let values: Vec<u64> = rows.iter().flat_map(|r| r.0).collect();
    let isnull: Vec<u8> = rows.iter().flat_map(|r| r.1).collect();
    let want: Vec<_> = rows.iter().map(|r| datarow(&r.2)).collect();
    for r in 0..rows.len() {
        node.send_row(&values[r * 8..r * 8 + 8], &isnull[r * 8..r * 8 + 8]).unwrap();
    }
    assert_eq!(take_sent(), want, "per row");
    let c0 = NUMERIC_OUT_CALLS.with(Cell::get);
    node.send_rows(&values, &isnull, rows.len()).unwrap();
    assert_eq!(take_sent(), want, "framed");
    assert_eq!(NUMERIC_OUT_CALLS.with(Cell::get), c0, "no numeric_out in a fast block");
    // one NaN in the block: the block goes row by row, the same bytes, numeric_out for the NaN alone
    let mut v2 = values.clone();
    v2[7] = nums[3].as_bytes().as_ptr() as u64;
    let mut want2 = want.clone();
    want2[0] = datarow(&[Some(b"-32768"), Some(b"-2147483648"), Some(b"-9223372036854775808"), Some(b"t"), Some(b"0.1"), Some(b"1.5"), Some(b"alpha"), Some(b"NaN")]);
    node.send_rows(&v2, &isnull, rows.len()).unwrap();
    assert_eq!(take_sent(), want2, "a NaN block");
    assert_eq!(NUMERIC_OUT_CALLS.with(Cell::get), c0 + 1, "numeric_out for the NaN only");
    TEXT_VARLENA.with(|c| c.set(false));
}

/// The binary format of a numeric column is untouched: the fast kind is text format's alone, so a binary numeric column reaches its
/// send function (framed and per row) and no numeric_out runs.
#[test]
fn a_binary_numeric_column_takes_its_send_function() {
    install();
    CONVERT.with(|c| c.set(false));
    let p = numeric_program(&[OutCol::new("n", NUMERICOID, -1, -1, false)]);
    let mut node = WireNode::new();
    node.prepare(&p, Some(&[1])).unwrap();
    assert!(node.wire_format().is_none());
    take_sent();
    let img = adt_numeric::numeric_in("-12.50", -1, None).unwrap().unwrap();
    let c0 = NUMERIC_OUT_CALLS.with(Cell::get);
    node.send_rows(&[img.as_bytes().as_ptr() as u64], &[0], 1).unwrap();
    assert_eq!(take_sent(), vec![datarow(&[Some(img.payload())])]);
    assert_eq!(NUMERIC_OUT_CALLS.with(Cell::get), c0);
}

/// D4474: a run's blocks reuse one framing buffer (grown to its largest block); at the run's end (`end_run`) a buffer
/// over `FRAME_KEEP` is freed, so a node keeps at most `FRAME_KEEP` between runs. A run within it keeps its buffer.
#[test]
fn a_run_over_frame_keep_leaves_no_framing_buffer_behind() {
    install();
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("t", TEXTOID, -1, -1, false)];
    let p = Arc::new(Program::new(PLayout { nsites: 0, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &cols, WritePlan::SELECT, Code::hand(noop)));
    let mut node = WireNode::new();
    node.prepare(&p, None).unwrap();
    take_sent();
    let image = |n: usize| {
        let mut v = set_varsize_4b(4 + n).to_vec();
        v.resize(4 + n, b'x');
        v
    };
    let big_len = 1 << 20;
    let (big, small) = (image(big_len), image(100));
    // one run of two big blocks: the second reuses the first's buffer
    node.reset().send_rows(&[big.as_ptr() as u64], &[0], 1).unwrap();
    let grown = node.frame_capacity();
    assert!(grown > big_len, "the big row is framed in the node's buffer ({grown} B)");
    node.send_rows(&[big.as_ptr() as u64], &[0], 1).unwrap();
    assert_eq!(node.frame_capacity(), grown, "a run's next block reuses its buffer");
    let sent = take_sent();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|m| m.1.len() == 2 + 4 + big_len), "the big rows are sent whole");
    node.end_run();
    assert!(node.frame_capacity() <= FRAME_KEEP, "a {} B framing buffer kept past the run", node.frame_capacity());
    // runs of small blocks: framed within FRAME_KEEP, kept across runs and reused
    let values = vec![small.as_ptr() as u64; 64];
    let isnull = vec![0u8; 64];
    node.reset().send_rows(&values, &isnull, 64).unwrap();
    node.end_run();
    assert_eq!(take_sent().len(), 64);
    let kept = node.frame_capacity();
    assert!(kept > 0 && kept <= FRAME_KEEP, "a framed block within FRAME_KEEP keeps its buffer ({kept} B)");
    for _ in 0..3 {
        node.reset().send_rows(&values, &isnull, 64).unwrap();
        node.end_run();
        assert_eq!(take_sent().len(), 64);
        assert_eq!(node.frame_capacity(), kept, "the next runs reuse the kept buffer");
    }
}

/// pglpoint S0b: `send_row_in` (the DataRow built, and the output / send functions called, in the caller's per-row context)
/// sends `send_row`'s bytes: text and binary formats, with and without encoding conversion, NULLs, the fast kinds and a
/// numeric (its fast text and numeric_out), the caller's context reset after every row.
#[test]
fn send_row_in_sends_send_rows_bytes() {
    install();
    let split = |r: &[(Datum, bool)]| -> (Vec<u64>, Vec<u8>) { r.iter().map(|(d, n)| (d.as_u64(), *n as u8)).unzip() };
    let p = Arc::new(program());
    for formats in [&[0i16, 0, 0][..], &[1, 0, 1][..]] {
        for convert in [false, true] {
            CONVERT.with(|c| c.set(convert));
            let mut a = WireNode::new();
            a.prepare(&p, Some(formats)).unwrap();
            a.send_rowdesc().unwrap();
            for r in rows() {
                let (v, n) = split(&r[..]);
                a.send_row(&v, &n).unwrap();
            }
            let want = take_sent();
            let mut b = WireNode::new();
            b.prepare(&p, Some(formats)).unwrap();
            b.send_rowdesc().unwrap();
            let mut row = MemoryContext::new_wholesale("wire-test-row");
            for r in rows() {
                let (v, n) = split(&r[..]);
                b.send_row_in(&v, &n, row.mcx()).unwrap();
                row.reset();
            }
            let got = take_sent();
            assert_eq!(got.len(), 4, "T + 3 D messages");
            assert_eq!(got, want, "formats {formats:?} convert {convert}");
            assert_eq!((a.rows(), b.rows()), (3, 3));
        }
    }
    // the fast kinds and a numeric beside them, text format: the fast text, then numeric_out for NaN
    CONVERT.with(|c| c.set(false));
    let cols = [OutCol::new("a", INT2OID, -1, 2, true), OutCol::new("b", INT8OID, -1, 8, true), OutCol::new("n", NUMERICOID, -1, -1, false)];
    let p = numeric_program(&cols);
    let (mut a, mut b) = (WireNode::new(), WireNode::new());
    a.prepare(&p, None).unwrap();
    b.prepare(&p, None).unwrap();
    take_sent();
    let mut row = MemoryContext::new_wholesale("wire-test-row");
    for s in ["-12.50", "NaN", "0.000000000000000000000000000000000000001"] {
        let img = adt_numeric::numeric_in(s, -1, None).unwrap().unwrap();
        let v = [Datum::from_i32(-7).as_u64(), Datum::from_i64(1 << 40).as_u64(), img.as_bytes().as_ptr() as u64];
        for isnull in [[0u8, 0, 0], [1, 0, 1]] {
            a.send_row(&v, &isnull).unwrap();
            let want = take_sent();
            b.send_row_in(&v, &isnull, row.mcx()).unwrap();
            row.reset();
            assert_eq!(take_sent(), want, "{s} {isnull:?}");
        }
    }
}
