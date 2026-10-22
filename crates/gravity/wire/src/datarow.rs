use std::sync::Arc;

use datum::Datum;
use gravity_driver::mem::budget;
use gravity_driver::mem::root::Root;
use gravity_driver::{WireFormat, MAX_WIRE_ATTS};
use gravity_program::{ContentKey, OutCol, Program};
use mcx::{Mcx, MemoryContext, PgVec};
use pqformat::{pq_beginmessage_reuse, pq_endmessage_reuse, pq_sendbytes, pq_sendcountedtext, pq_sendint16, pq_sendint32};
use stringinfo::StringInfo;
use types_core::catalog::{BOOLOID, FLOAT4OID, FLOAT8OID, INT2OID, INT4OID, INT8OID};
use types_core::{primitive::InvalidOid, Oid};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use types_fmgr::{function_call1_coll_in, send_function_call, FmgrInfo, PackedVarlena};

use crate::rowdesc::RowDesc;

mod raw;
pub use raw::{frame_rows, frame_rows_text};

/// D4474: the most of `send_rows`' framing buffer a wire node keeps between runs (`end_run`). Every block of the RTT,
/// Speedtest, analytic scan, 10 GB join and pglfast suites framed at most 14,751 B (one join-suite block; the next
/// largest 4,281 B). PostgreSQL's send buffer is 8 KB and printtup frees its row buffer at the run's end; before this
/// cap a node kept its largest block's buffer for its life (a 50 MB text row left 50 MB).
pub const FRAME_KEEP: usize = 16 * 1024;

const PQMSG_DATA_ROW: u8 = b'D';
const PQMSG_ROW_DESCRIPTION: u8 = b'T';
/// Output columns per row: PostgreSQL's limit on a target list (MaxTupleAttributeNumber; D313: 128 once, and a
/// wider SELECT indexed past it).
pub const MAX_FORMATS: usize = gravity_abi::limits::MAX_TUPLE_ATTRIBUTE_NUMBER;
const FORMAT_WORDS: usize = MAX_FORMATS.div_ceil(64);

/// A program's output formats, one bit a column (the protocol's two codes: 0 text, 1 binary; any other is refused as
/// printtup's is): the prepared key beside the program's content key, built and compared on every Bind without an
/// allocation at any width.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Formats {
    binary: [u64; FORMAT_WORDS],
    natts: usize,
}

impl Formats {
    fn of(out: &[OutCol], formats: Option<&[i16]>) -> PgResult<Formats> {
        let natts = out.len();
        if natts > MAX_FORMATS {
            return Err(Box::new(PgError::error(format!("target lists can have at most {MAX_FORMATS} entries")).with_sqlstate(types_error::ERRCODE_TOO_MANY_COLUMNS))); // alloc: Cold
        }
        let mut binary = [0u64; FORMAT_WORDS];
        for (i, col) in out.iter().enumerate() {
            match formats.map_or(col.format, |x| x[i]) {
                0 => {}
                1 => binary[i / 64] |= 1 << (i % 64),
                f => return Err(unsupported_format_code(f)),
            }
        }
        Ok(Formats { binary, natts })
    }

    fn get(&self, i: usize) -> i16 {
        ((self.binary[i / 64] >> (i % 64)) & 1) as i16
    }
}

pub struct AttrInfo {
    pub typoutput: Oid,
    pub typsend: Oid,
    pub typisvarlena: bool,
    pub format: i16,
    /// A text-format integer / bool / float column written straight into the message (2 / 4 / 8 = the int
    /// width, `FAST_BOOL` / `FAST_FLOAT4` / `FAST_FLOAT8`; 0 = through the type's output function): the text is ASCII, so no client
    /// encoding conversion, no palloc'd cstring, no strlen, no fmgr call — the 100 k-row join
    /// emit at W = 16 was 9.5 ms of exactly those on the leader (`memcpy` + `strlen` + `int4out`).
    pub fast: u8,
    pub finfo: FmgrInfo,
}

pub struct WireNode {
    /// D4474: `frame`'s capacity on the backend's session (`keep_frame`). Declared before `frame`: at the drop its bytes
    /// leave the session before the frame's block is freed.
    frame_charge: Option<budget::Charge>,
    send_ctx: Option<MemoryContext>,
    buf: Option<StringInfo<'static>>,
    attrs: Option<PgVec<'static, AttrInfo>>,
    rowdesc: Option<RowDesc>,
    program: Option<Arc<Program>>,
    /// The out row `prepare_cols` was given (no program).
    cols: Option<PgVec<'static, OutCol>>,
    prepared: Option<(ContentKey, Formats)>,
    natts: usize,
    rows: u64,
    /// `wire_format()` resolved once at `prepare`: every column a text-format integer, so a whole out block is framed
    /// by `frame_rows` and appended in one `pq_putbytes` (`send_rows`), not one `pq_putmessage` per row.
    fast: Option<WireFormat>,
    /// The block's frames, reused across pushes (grown to a run's largest block), at most `FRAME_KEEP` past the run's end
    /// (`end_run`).
    frame: Vec<u8>,
    /// D719 (V4R-254): while set, every message the node sends (RowDescription, DataRows, framed blocks) is appended
    /// whole, sent by `release` once the statement has ended and dropped by `drop_held` (PORTAL_ONE_RETURNING's order).
    hold: Option<memledger::ChargedVec<u8>>,
    /// D1509: the RowDescription held back (`defer`) until the first row or `release`: a run-time refusal before it falls through.
    defer: bool,
    pending: bool,
    /// D3535: `rowdesc` was built under a client encoding conversion (its column names are the client's bytes).
    desc_converted: bool,
    /// Declared last, so dropped last: `buf`, `attrs` and `rowdesc` live in these contexts.
    node_ctx: Root,
    desc_ctx: Root,
}

/// One message's frame (type, length word, body) appended to the held bytes.
fn frame_into(out: &mut Vec<u8>, t: u8, body: &[u8]) {
    out.push(t);
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
}

/// The fast kinds (`AttrInfo.fast`, `WireFormat.kinds`): the int width 2 / 4 / 8, and `FAST_BOOL` / `FAST_FLOAT4` / `FAST_FLOAT8` written by the same
/// formatters the output functions call (`boolout`, `float4out`, `float8out` under `extra_float_digits`), so the text is the fmgr path's byte for byte.
pub const FAST_BOOL: u8 = 1;
pub const FAST_FLOAT4: u8 = 5;
pub const FAST_FLOAT8: u8 = 9;
/// A text-format varlena column whose output function is `textout` / `varcharout` / `bpcharout` (M4-work §C): the inline payload framed from the datum's `(ptr, len)`
/// behind the int32 length, no cstring, no fmgr call — only in `send_rows` (`WireNode::fast`, never `wire_format`'s legs), and only while no client encoding conversion
/// is needed; a block holding a compressed or external image goes through `send_row` (the output function detoasts).
pub const FAST_TEXT: u8 = 13;
/// A text-format numeric column (`numeric_out`, V4R-29 num render): a finite image whose scaled integer (value x 10^dscale) has at most
/// `NUM_DIGITS` digits is written straight into the frame (sign, integer digits, '.', exactly dscale digits: `get_str_from_var`'s
/// text), no fmgr call, no palloc'd cstring; NaN / +-Infinity, a wider value and a compressed or external image take `numeric_out`
/// (`send_row`). Only in `send_rows` / `send_row`, never `wire_format`'s legs.
pub const FAST_NUMERIC: u8 = 17;
const NUMERICOUT: Oid = 1702;
/// The scaled integers the fast numeric path renders: below 10^38, inside u128.
const NUM_DIGITS: usize = 38;
/// A rendered numeric's widest text: sign, 38 digits, a leading "0" and the '.'.
pub const NUM_MAX: usize = 48;
const TEXTOUT: Oid = 47;
const BPCHAROUT: Oid = 1045;
const VARCHAROUT: Oid = 1047;
/// Every fast kind's text fits `FAST_MAX` bytes (`MAXDOUBLEWIDTH` for the floats).
pub const FAST_MAX: usize = adt_float::MAXDOUBLEWIDTH;

const fn fast_of(typid: Oid) -> u8 {
    match typid {
        INT2OID => 2,
        INT4OID => 4,
        INT8OID => 8,
        BOOLOID => FAST_BOOL,
        FLOAT4OID => FAST_FLOAT4,
        FLOAT8OID => FAST_FLOAT8,
        _ => 0,
    }
}

const fn fast_max(kind: u8) -> usize {
    match kind {
        FAST_BOOL => 1,
        2 => 6,
        4 => 11,
        8 => 20,
        _ => FAST_MAX,
    }
}

/// The datum's text for a fast kind into `out` (at least `FAST_MAX` bytes): the length written.
#[inline]
fn put_fast(kind: u8, v: u64, out: &mut [u8]) -> usize {
    match kind {
        2 => numutils::pg_ltoa(v as u16 as i16 as i32, out),
        4 => numutils::pg_ltoa(v as u32 as i32, out),
        8 => numutils::pg_lltoa(v as i64, out),
        FAST_BOOL => {
            out[0] = if v != 0 { b't' } else { b'f' };
            1
        }
        FAST_FLOAT4 => adt_float::float4out(f32::from_bits(v as u32), out),
        _ => adt_float::float8out(f64::from_bits(v), out),
    }
}

/// The worst-case frame bytes of one row of `fmt`: the header (`'D'`, int32, int16) and per column the int32 length and the widest text.
pub fn frame_row_max(fmt: &WireFormat) -> usize {
    let mut n = 7;
    for &k in &fmt.kinds[..fmt.natts as usize] {
        n += 4 + fast_max(k);
    }
    n
}

/// "00" .. "99": two digits per division (the lab's digit-pair render).
static PAIRS: [u8; 200] = {
    let mut t = [0u8; 200];
    let mut i = 0;
    while i < 100 {
        t[2 * i] = b'0' + (i / 10) as u8;
        t[2 * i + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    t
};

/// A numeric image's fast-render facts: its base-10000 digits, dscale, weight and sign.
struct NumHead<'a> {
    digits: &'a [u8],
    dscale: u32,
    weight: i32,
    neg: bool,
}

/// The numeric image whose varlena payload is `pay` (raw.rs `payload`) when the fast path renders it: not NaN /
/// +-Infinity, its scaled integer at most `NUM_DIGITS` digits (the integer digits bounded by the weight and the first
/// digit's width, plus dscale).
#[inline]
fn num_head(pay: &[u8]) -> Option<NumHead<'_>> {
    let n = pay.len();
    if n < 2 {
        return None;
    }
    let h = u16::from_ne_bytes([pay[0], pay[1]]);
    let (neg, dscale, weight, digits) = match h & 0xC000 {
        0xC000 => return None,
        0x8000 => {
            let w = (h & 0x3F) as i32;
            let w = if h & 0x40 != 0 { w - 64 } else { w };
            (h & 0x2000 != 0, ((h >> 7) & 0x3F) as u32, w, &pay[2..])
        }
        sign => {
            if n < 4 {
                return None;
            }
            (sign == 0x4000, (h & 0x3FFF) as u32, i16::from_ne_bytes([pay[2], pay[3]]) as i32, &pay[4..])
        }
    };
    let intd = match (weight >= 0, digits.len() >= 2) {
        (true, true) => weight as usize * 4 + dec_len(u16::from_ne_bytes([digits[0], digits[1]])),
        _ => 0,
    };
    (intd + dscale as usize <= NUM_DIGITS).then_some(NumHead { digits, dscale, weight, neg })
}

const fn dec_len(d: u16) -> usize {
    if d >= 1000 { 4 } else if d >= 100 { 3 } else if d >= 10 { 2 } else { 1 }
}

/// 10^0 .. 10^38.
static POW10: [u128; NUM_DIGITS + 1] = {
    let mut t = [1u128; NUM_DIGITS + 1];
    let mut i = 1;
    while i <= NUM_DIGITS {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// The image's scaled integer |value| x 10^dscale, the digits past dscale truncated (`get_str_from_var` prints no more).
#[inline]
fn num_scaled(h: &NumHead<'_>) -> u128 {
    let mut acc: u128 = 0;
    let mut exp = 4 * h.weight + h.dscale as i32; // the power of ten of the current base-10000 digit in the scaled integer
    for c in h.digits.chunks_exact(2) {
        let d = u16::from_ne_bytes([c[0], c[1]]) as u128;
        if exp >= 0 {
            acc = acc.wrapping_add(d.wrapping_mul(POW10[exp as usize]));
        } else {
            if exp > -4 {
                acc = acc.wrapping_add(d / POW10[(-exp) as usize]);
            }
            break;
        }
        exp -= 4;
    }
    acc
}

/// `v` / 10^scale as PostgreSQL prints it (a '-' when `neg`, the integer digits or "0", then '.' and exactly `scale` digits) at
/// the end of `buf`; the text's start (lab num.rs `fmt_scaled_into`: two digits per 64-bit division from `PAIRS`).
#[inline]
pub fn fmt_scaled_into(neg: bool, v: u128, scale: u32, buf: &mut [u8; NUM_MAX]) -> usize {
    #[inline(always)]
    fn pair(buf: &mut [u8; NUM_MAX], i: &mut usize, r: usize) {
        *i -= 2;
        buf[*i..*i + 2].copy_from_slice(&PAIRS[2 * r..2 * r + 2]);
    }
    #[inline(always)]
    fn one(buf: &mut [u8; NUM_MAX], i: &mut usize, d: u8) {
        *i -= 1;
        buf[*i] = b'0' + d;
    }
    let mut i = NUM_MAX;
    let mut a = v;
    if scale > 0 {
        let mut frac = scale;
        while frac > 0 && a > u64::MAX as u128 {
            one(buf, &mut i, (a % 10) as u8);
            a /= 10;
            frac -= 1;
        }
        if frac > 0 {
            let mut w = a as u64;
            while frac >= 2 {
                pair(buf, &mut i, (w % 100) as usize);
                w /= 100;
                frac -= 2;
            }
            if frac == 1 {
                one(buf, &mut i, (w % 10) as u8);
                w /= 10;
            }
            a = w as u128;
        }
        i -= 1;
        buf[i] = b'.';
    }
    while a > u64::MAX as u128 {
        one(buf, &mut i, (a % 10) as u8);
        a /= 10;
    }
    let mut w = a as u64;
    if w == 0 {
        one(buf, &mut i, 0);
    }
    while w >= 100 {
        pair(buf, &mut i, (w % 100) as usize);
        w /= 100;
    }
    if w >= 10 {
        pair(buf, &mut i, w as usize);
    } else if w > 0 {
        one(buf, &mut i, w as u8);
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    i
}

/// The numeric whose varlena payload is `pay` rendered into `out` (at least `NUM_MAX` bytes) when the fast path takes
/// it: the text's length.
#[inline]
fn put_numeric(pay: &[u8], out: &mut [u8]) -> Option<usize> {
    let h = num_head(pay)?;
    let mut buf = [0u8; NUM_MAX];
    let at = fmt_scaled_into(h.neg, num_scaled(&h), h.dscale, &mut buf);
    let n = NUM_MAX - at;
    out[..n].copy_from_slice(&buf[at..]);
    Some(n)
}

fn unsupported_format_code(format: i16) -> Box<PgError> {
    Box::new(PgError::error(format!("unsupported format code: {format}")).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).with_funcname("printtup_prepare_info")) // alloc: Cold
}

impl Default for WireNode {
    fn default() -> Self {
        Self::new()
    }
}

impl WireNode {
    pub fn new() -> WireNode {
        WireNode { node_ctx: Root::bump("GravityWireNode"), desc_ctx: Root::bump("GravityWireDesc"), send_ctx: None, buf: None, attrs: None, rowdesc: None, program: None, cols: None, prepared: None, natts: 0, rows: 0, fast: None, frame_charge: None, frame: Vec::new(), hold: None, defer: false, pending: false, desc_converted: false } // alloc: Node
    }

    fn rebuild_rowdesc(&mut self) -> PgResult<()> {
        self.rowdesc = None;
        self.desc_ctx.reset();
        let formats = self.prepared.expect("wire node before prepare").1;
        self.desc_converted = mbutils_seams::server_to_client_conversion_needed::call();
        let rd = RowDesc::build(self.desc_mcx(), self.out_cols(), |i| formats.get(i))?;
        self.rowdesc = Some(rd);
        Ok(())
    }

    fn out_cols(&self) -> &[OutCol] {
        match (&self.program, &self.cols) {
            (Some(p), _) => &p.out,
            (None, Some(c)) => c,
            (None, None) => &[],
        }
    }

    pub fn send_rowdesc(&mut self) -> PgResult<()> {
        self.pending = self.defer;
        if self.defer { Ok(()) } else { self.put_rowdesc() }
    }

    /// D1509: the next `send_rowdesc` waits for the first row or `release` (`unsent` until then).
    pub fn defer_rowdesc(&mut self) {
        self.defer = true;
    }

    pub fn unsent(&self) -> bool {
        self.pending && self.rows == 0
    }

    pub fn flush_rowdesc(&mut self) -> PgResult<()> {
        self.defer = false;
        if core::mem::take(&mut self.pending) { self.put_rowdesc() } else { Ok(()) }
    }

    fn put_rowdesc(&mut self) -> PgResult<()> {
        // D3535 (Workers IO sig-2a997a307110): a description built under a conversion holds the client's bytes, so it is
        // built again once the session's client encoding is the server's again (RESET client_encoding): the cached one
        // sent LATIN1 column names to a UTF8 client
        if self.desc_converted || mbutils_seams::server_to_client_conversion_needed::call() {
            self.rebuild_rowdesc()?;
        }
        let rd = self.rowdesc.as_ref().expect("wire node before prepare");
        match self.hold.as_mut() {
            Some(h) => {
                h.with_vec(|v| frame_into(v, PQMSG_ROW_DESCRIPTION, rd.body())); // alloc: Exec charged h.owner()
                Ok(())
            }
            None => rd.send(),
        }
    }

    /// D719: hold every message from here (a simple-protocol RETURNING write with a one-row bound: runner execute.rs).
    /// The held bytes are charged to the statement's (or the executing portal's) owner on this leader thread
    /// (`gravity_driver::stmt_context`): RETURNING rows held until the statement ends grow with the data.
    pub fn hold(&mut self) {
        self.hold = Some(memledger::ChargedVec::new(gravity_driver::stmt_context().map(|(o, _)| o), memledger::Kind::Region)); // alloc: Exec charged gravity_driver::stmt_context().map(|(o, _)| o)
    }

    /// D719: the held messages to the wire in one pass (the statement ended, after its AFTER triggers: RunFromStore's place).
    pub fn release(&mut self) -> PgResult<()> {
        self.flush_rowdesc()?;
        match self.hold.take() {
            Some(h) if !h.is_empty() => pqcomm_seams::pq_putbytes::call(&h).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// D719: the held messages forgotten (the statement failed: PostgreSQL sent none of them).
    pub fn drop_held(&mut self) {
        self.hold = None;
        (self.defer, self.pending) = (false, false);
    }

    pub fn prepare(&mut self, program: &Arc<Program>, formats: Option<&[i16]>) -> PgResult<()> {
        let key = (program.key, Formats::of(&program.out, formats)?);
        if self.prepared == Some(key) && self.cols.is_none() {
            return Ok(());
        }
        self.cols = None;
        self.program = Some(program.clone());
        self.prepare_out(&program.out, key)?;
        self.rebuild_rowdesc()
    }

    /// `prepare` for an out row no program carries (runner point.rs); kept while the next row's columns and formats match.
    pub fn prepare_cols(&mut self, cols: &[OutCol], formats: Option<&[i16]>) -> PgResult<()> {
        let key = (ContentKey(0), Formats::of(cols, formats)?);
        if self.prepared == Some(key) && self.program.is_none() && self.cols.as_deref() == Some(cols) {
            return Ok(());
        }
        self.program = None;
        self.cols = None;
        self.prepare_out(cols, key)?;
        let mcx = self.mcx();
        // SAFETY ('static Mcx): `mcx` is `node_ctx.held()` (`Self::mcx`), the path `attrs` and `buf` take, no transmute
        // here. `node_ctx` is a `Root` (a boxed context: address-stable) the node owns, declared after `cols`, so `cols`
        // drops first. Its one reset is in `prepare_out`, and both callers (`prepare`, here) set `cols = None` before it.
        // Bytes: one `OutCol` per column, at most MAX_TUPLE_ATTRIBUTE_NUMBER (1,664; `Formats::of` refuses more), on
        // pgrust_memory_owners' GravityWireNode row (kind unnamed_root), not in unattributed.
        let mut own: PgVec<'static, OutCol> = PgVec::new_in(mcx);
        own.try_reserve_exact(cols.len()).map_err(|_| mcx.oom(core::mem::size_of_val(cols)))?;
        own.extend_from_slice(cols);
        self.cols = Some(own);
        self.rebuild_rowdesc()
    }

    fn prepare_out(&mut self, out: &[OutCol], key: (ContentKey, Formats)) -> PgResult<()> {
        self.attrs = None;
        self.rowdesc = None;
        self.buf = None;
        self.prepared = None;
        self.node_ctx.reset();
        let mcx = self.mcx();
        let natts = out.len();
        let mut info: PgVec<'static, AttrInfo> = PgVec::new_in(mcx);
        info.try_reserve_exact(natts).map_err(|_| mcx.oom(natts * core::mem::size_of::<AttrInfo>()))?;
        for (i, col) in out.iter().enumerate() {
            let format = key.1.get(i);
            let entry = match format {
                0 => {
                    let (typoutput, typisvarlena) = lsyscache_seams::get_type_output_info::call(col.typid)?;
                    let fast = match typoutput {
                        TEXTOUT | BPCHAROUT | VARCHAROUT if !col.typbyval && col.typlen == -1 => FAST_TEXT,
                        NUMERICOUT if !col.typbyval && col.typlen == -1 => FAST_NUMERIC,
                        _ => fast_of(col.typid),
                    };
                    AttrInfo { typoutput, typsend: InvalidOid, typisvarlena, format, fast, finfo: fmgr_seams::fmgr_info::call(typoutput)? }
                }
                1 => {
                    let (typsend, typisvarlena) = lsyscache_seams::get_type_binary_output_info::call(col.typid)?;
                    AttrInfo { typoutput: InvalidOid, typsend, typisvarlena, format, fast: 0, finfo: fmgr_seams::fmgr_info::call(typsend)? }
                }
                _ => return Err(unsupported_format_code(format)),
            };
            info.push(entry);
        }
        // every output / send function's result is fresh in the per-row context (fmgr's contract), reset per row
        if self.send_ctx.is_none() {
            self.send_ctx = Some(MemoryContext::new_bump("GravityWireSend"));
        }
        self.buf = Some(StringInfo::new_in(mcx)?);
        self.attrs = Some(info);
        self.natts = natts;
        self.prepared = Some(key);
        self.fast = self.format_of(true);
        Ok(())
    }

    /// The legs' framing when every column takes the fast integer path (`AttrInfo.fast`); a text or numeric column keeps the legs on the row path (`FAST_TEXT`
    /// and `FAST_NUMERIC` are `send_rows`'s alone).
    pub fn wire_format(&self) -> Option<WireFormat> {
        self.format_of(false)
    }

    fn format_of(&self, text: bool) -> Option<WireFormat> {
        let attrs = self.attrs.as_ref()?;
        if attrs.is_empty() || attrs.len() > MAX_WIRE_ATTS || attrs.iter().any(|a| a.fast == 0 || (!text && matches!(a.fast, FAST_TEXT | FAST_NUMERIC))) {
            return None;
        }
        let mut kinds = [0u8; MAX_WIRE_ATTS];
        for (k, a) in kinds.iter_mut().zip(attrs.iter()) {
            *k = a.fast;
        }
        Some(WireFormat { natts: attrs.len() as u8, kinds })
    }

    /// `rows` DataRow messages a leg framed (`wire_format`), appended to the wire in one pass.
    pub fn send_framed(&mut self, bytes: &[u8], rows: u64) -> PgResult<()> {
        if self.rows == 0 && rows > 0 {
            gravity_driver::ahead_trace::sent(); // construct C2 R3
        }
        self.flush_rowdesc()?;
        match self.hold.as_mut() {
            Some(h) => h.with_vec(|v| v.extend_from_slice(bytes)), // alloc: Exec charged h.owner()
            None => {
                pqcomm_seams::pq_putbytes::call(bytes)?;
            }
        }
        self.rows += rows;
        Ok(())
    }

    pub fn rowdesc(&self) -> Option<&RowDesc> {
        self.rowdesc.as_ref()
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// D4474: the framing buffer's capacity (at most `FRAME_KEEP` past a run's end, `end_run`).
    pub fn frame_capacity(&self) -> usize {
        self.frame.capacity()
    }

    pub fn reset_rows(&mut self) {
        self.rows = 0;
    }

    /// The node at a run's start (its row count cleared); the runner's receiver sends through it directly
    /// so an output function's error keeps PostgreSQL's message and sqlstate.
    pub fn reset(&mut self) -> &mut WireNode {
        self.rows = 0;
        self
    }

    /// An out block (`rows` rows, row-major) to the wire: framed whole and appended in one pass when every column is a text-format
    /// integer (`fast`), else `send_row` per row.
    pub fn send_rows(&mut self, values: &[u64], isnull: &[u8], rows: usize) -> PgResult<()> {
        let Some(fmt) = self.fast else {
            let natts = self.natts;
            for r in 0..rows {
                self.send_row(&values[r * natts..(r + 1) * natts], &isnull[r * natts..(r + 1) * natts])?;
            }
            return Ok(());
        };
        let kinds = &fmt.kinds[..fmt.natts as usize];
        let text = kinds.contains(&FAST_TEXT);
        let mut frame = core::mem::take(&mut self.frame);
        frame.clear();
        // a numeric's text is ASCII, the same in every client encoding; a text column's needs no conversion
        let framed = if text || kinds.contains(&FAST_NUMERIC) {
            !(text && mbutils_seams::server_to_client_conversion_needed::call()) && frame_rows_text(&mut frame, &fmt, values, isnull, rows)
        } else {
            frame_rows(&mut frame, &fmt, values, isnull, rows);
            true
        };
        let r = if framed { self.send_framed(&frame, rows as u64) } else { Ok(()) };
        self.keep_frame(frame);
        if framed {
            return r;
        }
        let natts = self.natts;
        for r in 0..rows {
            self.send_row(&values[r * natts..(r + 1) * natts], &isnull[r * natts..(r + 1) * natts])?;
        }
        Ok(())
    }

    /// D4474: the framing buffer back after a block's send, kept whole for the run's next block (a stream's blocks reuse
    /// it: freeing a buffer over `FRAME_KEEP` after each send cost 3-17 % more backend cycles on 1,024-row blocks of
    /// 113 KB to 2 MB). Its capacity is the session's (`frame_charge`); a block within it costs one compare.
    #[inline]
    fn keep_frame(&mut self, frame: Vec<u8>) {
        if frame.capacity() as u64 != self.frame_charge.as_ref().map_or(0, budget::Charge::committed) {
            self.charge_frame(frame.capacity() as u64);
        }
        self.frame = frame;
    }

    /// D4474: the run's end (the runner's `drive`, once per run): a framing buffer over `FRAME_KEEP` is freed, as
    /// printtup's shutdown frees its row buffer, so a node keeps at most `FRAME_KEEP` between runs.
    pub fn end_run(&mut self) {
        if self.frame.capacity() > FRAME_KEEP {
            self.keep_frame(Vec::new()); // alloc: Node
        }
    }

    /// D4474: `frame_charge` follows the frame's capacity `cap`, opened on the backend's session at the first frame (a
    /// thread with no backend, a rig's, charges nothing).
    #[cold]
    #[inline(never)]
    fn charge_frame(&mut self, cap: u64) {
        if self.frame_charge.is_none() {
            let a = budget::current();
            if !a.is_backend() {
                return;
            }
            self.frame_charge = Some(budget::Charge::open(budget::Owner::Session(a), budget::Release::Session, budget::Kind::Region, budget::charge::Door::Heap, 0));
        }
        if let Some(c) = self.frame_charge.as_mut() {
            let have = c.committed();
            if cap > have {
                c.commit(cap - have);
            } else {
                c.free(have - cap);
            }
        }
    }

    pub fn send_row(&mut self, values: &[u64], isnull: &[u8]) -> PgResult<()> {
        if self.rows == 0 {
            gravity_driver::ahead_trace::sent(); // construct C2 R3
        }
        self.flush_rowdesc()?;
        let conv_needed = mbutils_seams::server_to_client_conversion_needed::call();
        let buf = self.buf.as_mut().expect("wire node before prepare");
        let attrs = self.attrs.as_mut().expect("wire node before prepare");
        // `prepare_out` makes `send_ctx` before `buf` and `attrs` and never drops it
        let send_ctx = self.send_ctx.as_mut().expect("wire node before prepare");
        let natts = self.natts;
        pq_beginmessage_reuse(buf, PQMSG_DATA_ROW);
        put_cols(attrs, natts, conv_needed, buf, send_ctx.mcx(), values, isnull)?;
        match self.hold.as_mut() {
            Some(h) => h.with_vec(|v| frame_into(v, PQMSG_DATA_ROW, buf.as_bytes())), // alloc: Exec charged h.owner()
            None => pq_endmessage_reuse(buf)?,
        }
        send_ctx.reset();
        self.rows += 1;
        Ok(())
    }

    /// `send_row` for a caller that owns the row's memory (runner point, an extended-protocol point portal): the DataRow is
    /// built, and every output / send function called, in `row`, a per-row context the caller resets after the row, so a
    /// wide value's bytes leave with the row instead of growing the node's `buf` (printtup builds each row in its per-row
    /// context). The bytes are `send_row`'s: one `put_cols`.
    pub fn send_row_in(&mut self, values: &[u64], isnull: &[u8], row: Mcx<'_>) -> PgResult<()> {
        if self.rows == 0 {
            gravity_driver::ahead_trace::sent(); // construct C2 R3
        }
        self.flush_rowdesc()?;
        let conv_needed = mbutils_seams::server_to_client_conversion_needed::call();
        let attrs = self.attrs.as_mut().expect("wire node before prepare");
        let mut buf = StringInfo::new_in(row)?;
        pq_beginmessage_reuse(&mut buf, PQMSG_DATA_ROW);
        put_cols(attrs, self.natts, conv_needed, &mut buf, row, values, isnull)?;
        match self.hold.as_mut() {
            Some(h) => h.with_vec(|v| frame_into(v, PQMSG_DATA_ROW, buf.as_bytes())), // alloc: Exec charged h.owner()
            None => pq_endmessage_reuse(&buf)?,
        }
        self.rows += 1;
        Ok(())
    }
}

/// A DataRow's body after its type byte, `send_row`'s loop moved whole (`send_row_in` shares it): the column count, then per
/// column a NULL's -1, a fast kind's text from the word, or the column's output (text) / send (binary) function called in
/// `out`, the per-row context its result is fresh in (fmgr's contract).
#[inline(always)]
fn put_cols<'b>(attrs: &mut [AttrInfo], natts: usize, conv_needed: bool, buf: &mut StringInfo<'b>, out: Mcx<'_>, values: &[u64], isnull: &[u8]) -> PgResult<()> {
    pq_sendint16(buf, natts as u16)?;
    for i in 0..natts {
        if isnull[i] != 0 {
            pq_sendint32(buf, (-1i32) as u32)?;
            continue;
        }
        let attr = Datum::from_u64(values[i]);
        let this = &mut attrs[i];
        if this.fast == FAST_NUMERIC {
            let mut text = [0u8; NUM_MAX];
            if let Some(n) = raw::numeric_text(values[i], &mut text) {
                pq_sendint32(buf, n as u32)?;
                buf.append_bytes_nt(&text[..n])?;
                continue;
            }
        } else if this.fast != 0 && this.fast != FAST_TEXT {
            // the Datum conventions of DatumGetInt16/32/64 / Bool / Float4 / Float8: the value in the word's low bits (a text column: its output function)
            let mut text = [0u8; FAST_MAX];
            let n = put_fast(this.fast, values[i], &mut text);
            pq_sendint32(buf, n as u32)?;
            buf.append_bytes_nt(&text[..n])?;
            continue;
        }
        if this.format == 0 {
            let cstr = function_call1_coll_in(&mut this.finfo, InvalidOid, out, attr)?;
            let s = raw::cstring_bytes(cstr);
            if conv_needed {
                pq_sendcountedtext(buf, s)?;
            } else {
                pq_sendint32(buf, s.len() as u32)?;
                buf.append_bytes_nt(s)?;
            }
        } else {
            let image = send_function_call(&mut this.finfo, attr, out)?;
            let v = raw::bytea_image(image);
            let data = v.data();
            pq_sendint32(buf, data.len() as u32)?;
            pq_sendbytes(buf, data)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod num_render_tests {
    use super::{fmt_scaled_into, NUM_MAX};

    fn render(neg: bool, v: u128, scale: u32) -> String {
        let mut buf = [0u8; NUM_MAX];
        let at = fmt_scaled_into(neg, v, scale, &mut buf);
        String::from_utf8(buf[at..].to_vec()).unwrap() // alloc: Cold
    }

    /// The render against a decimal string built digit by digit: every scale 0-38, values across the u64 edge (the 128-bit
    /// divisions hand over to the pair loop inside the fraction, at the '.', and in the integer part).
    #[test]
    fn fmt_scaled_matches_the_decimal_text() {
        let want = |neg: bool, v: u128, scale: u32| {
            let d = format!("{v:0>w$}", w = scale as usize + 1); // alloc: Cold
            let (i, f) = d.split_at(d.len() - scale as usize);
            format!("{}{i}{}{f}", if neg { "-" } else { "" }, if scale > 0 { "." } else { "" }) // alloc: Cold
        };
        let vals = [0u128, 1, 9, 10, 99, 100, 12345, u64::MAX as u128 - 1, u64::MAX as u128, u64::MAX as u128 + 1, 10u128.pow(20), 10u128.pow(37), 10u128.pow(38) - 1, 123456789012345678901234567890123456];
        for scale in 0..=38u32 {
            for &v in &vals {
                for neg in [false, true] {
                    assert_eq!(render(neg, v, scale), want(neg, v, scale), "{v} scale {scale}");
                }
            }
        }
        assert_eq!(render(false, 0, 2), "0.00");
        assert_eq!(render(true, 5, 3), "-0.005");
    }
}
