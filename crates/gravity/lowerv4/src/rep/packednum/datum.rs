//! PackedNum datum (`rep.packed.datum`, plan §3.2; KEEP lowerv3 rep/packednum.rs): a pgrc `numeric(p, s)` lane. Its form is
//! decided per window, never per row: a packed window's elements are the i64 mantissas at the typmod scale (`KIND_FIXED`),
//! a verbatim part's are pointers to the stored images (`KIND_BYREF`); the lane's kind byte is read once per row read here
//! (the fold roads never come here: they add mantissas).
//! - `datum` (`Window`): a verbatim element is the datum; a mantissa becomes the numeric image PostgreSQL stores for it
//!   (`agg_final::num_word`: canonical digits, the column's dscale, NaN from its sentinel) in a frame slot, valid for the
//!   row and memoised per (row, cell). `Word` (a kept order word): the same image from the word.
//! - `retain`: keeps the order word (`PackedNum { form: Word }`): a packed window's mantissa as is, a verbatim image's through
//!   `agg_final::num_mantissa` at the column's scale (a stored `numeric(p, s)` value always has dscale `s`). No copy.
//! - no block form: a verbatim window's elements are pointers, so a mask road cannot read the lane as words.
use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_abi::{lane, LANE_SIZE};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::{ConvOp, HostFlags, IntCond, Type, Value};
use gravity_program::{Home, NumForm, Rep, Until};

use crate::cell::{At, Cell, CellHome, CellLife};
use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, CellRef, DatumArm, Kept, Lx, RetainHome};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepPackedDatum, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: DatumArm = DatumArm { datum, retain, block: None };

/// A numeric image of an i64 mantissa (`make_result`'s short form): a 4-byte varlena header, the 2-byte short header and
/// at most 6 base-10000 digits of 2 bytes (19 decimal digits span at most 6 groups aligned on the decimal point), rounded
/// up to whole words for the stack slot.
pub(crate) const NUM_IMAGE_BYTES: u32 = (4 + 2 + 6 * 2 + 7) / 8 * 8;

/// The cell's form, facts and element word: `(verbatim, word)` for a `Window` lane (its kind byte, then the element),
/// `(false, val)` for a kept `Word`.
fn element(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> (Value, Value) {
    match (c.cell.rep, c.cell.at) {
        (Rep::PackedNum { form: NumForm::Word, .. }, _) => (cx.b.bconst(false), c.val),
        (Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn, At::Lane(l)) => {
            let k = crate::rep::code::datum::site(cx, c.cell);
            // A1 (D3224): inside a row loop the window's test, made once at its start (`SourceMech::num_kinds`)
            if let Some(&(_, _, verbatim)) = cx.num_kinds.as_ref().and_then(|v| v.iter().find(|e| e.0 == k && e.1 == l.attno)) {
                return (verbatim, cx.b.load(Type::I64, c.val, 0));
            }
            let ctx = cx.frame().ctx;
            let lanes = cx.b.load(Type::Ptr, ctx, site_off(k) as i32 + site::LANES as i32);
            let kind = cx.b.load_zext(Type::I8, lanes, l.attno as i32 * LANE_SIZE as i32 + lane::KIND as i32);
            let byref = cx.b.iconst(Type::I64, lane::KIND_BYREF as i64);
            let verbatim = cx.b.icmp(IntCond::Eq, kind, byref);
            (verbatim, cx.b.load(Type::I64, c.val, 0))
        }
        _ => crate::refuse!(cx, Reason::Bug { payload: "a packed numeric Window cell that is not its lane" }),
    }
}

/// D3224 A2: whether `c`'s kind test is its row loop's window-start one (`SourceMech::num_kinds`): its window holds at most
/// the source's `WINDOW_ROWS` rows whenever the test says packed.
pub(crate) fn window_bounded(cx: &Lx<'_, '_, '_>, c: CellRef<'_>) -> bool {
    let (Rep::PackedNum { form: NumForm::Window, .. }, At::Lane(l)) = (c.cell.rep, c.cell.at) else { return false };
    let k = crate::rep::code::datum::site(cx, c.cell);
    cx.num_kinds.as_ref().is_some_and(|v| v.iter().any(|e| e.0 == k && e.1 == l.attno))
}

/// D619: a `Window` lane cell's `(verbatim, word)`, the kind byte's test and the element, for a fold that adds a packed
/// window's word itself (pgrc packs a chunk only when every value fits finite, |m| < 2^55); `None` for any other cell.
pub(crate) fn window_word(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Option<(Value, Value)> {
    matches!((c.cell.rep, c.cell.at), (Rep::PackedNum { form: NumForm::Window, .. }, At::Lane(_))).then(|| element(cx, c))
}

/// N14: a `PackedNumDyn` lane cell's `(verbatim, word, scale)`: the kind byte's test, the element, and the window's scale
/// (the lane's `ncodes_or_scale`: the elected scale of the stream the window reads, hosts `lane_scale`), for the keyed
/// extreme that keys a packed code with no image (ops/agg/numeric num_side `step`); `None` for any other cell.
pub(crate) fn dyn_word(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Option<(Value, Value, Value)> {
    let (Rep::PackedNumDyn, At::Lane(_)) = (c.cell.rep, c.cell.at) else { return None };
    let (verbatim, w) = element(cx, c);
    Some((verbatim, w, lane_scale(cx, c)))
}

/// A `PackedNumDyn` lane cell's window scale, an I64 (`dyn_word`).
fn lane_scale(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>) -> Value {
    let At::Lane(l) = c.cell.at else { crate::refuse!(cx, Reason::Bug { payload: "a dynamic-scale packed numeric cell that is not its lane" }) };
    let k = crate::rep::code::datum::site(cx, c.cell);
    let ctx = cx.frame().ctx;
    let lanes = cx.b.load(Type::Ptr, ctx, site_off(k) as i32 + site::LANES as i32);
    cx.b.load_zext(Type::I32, lanes, l.attno as i32 * LANE_SIZE as i32 + lane::NCODES_OR_SCALE as i32)
}

fn scale(c: &Cell) -> i64 {
    rep::num_of(c.rep).scale as i64
}

fn datum(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts) -> (Value, Value) {
    let null = match c.null {
        Some(n) => n,
        None => cx.b.bconst(false),
    };
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_read, l_img, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
    cx.b.cond_br(null, (l_out, &[zero]), (l_read, &[]));
    cx.b.switch_to(l_read);
    let (verbatim, w) = element(cx, c);
    cx.b.cond_br(verbatim, (l_out, &[w]), (l_img, &[]));
    cx.b.switch_to(l_img);
    let slot = cx.b.declare_stack_slot(NUM_IMAGE_BYTES, 8);
    let out = cx.b.stack_addr(slot);
    let out = cx.b.conv(ConvOp::PtrToInt, Type::I64, out);
    let s = match c.cell.rep {
        Rep::PackedNumDyn => lane_scale(cx, c),
        _ => cx.b.iconst(Type::I64, scale(c.cell)),
    };
    let h = cx.b.declare_host_once("rep_num_image", &[Type::I64, Type::I64, Type::I64], Some(Type::I64), num_image as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    let p = cx.b.call_host(h, &[w, s, out], Some(Type::I64)).expect("returns the image");
    cx.b.br(l_out, &[p]);
    cx.b.switch_to(l_out);
    let p = cx.b.block_params(l_out)[0];
    let none = cx.b.icmp(IntCond::Eq, p, zero);
    (p, none)
}

fn retain(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, _tf: &TypeFacts, _until: Until, _home: RetainHome) -> Kept {
    if c.cell.rep == Rep::PackedNumDyn {
        // N14: its readers (the keyed extreme's step, D3139's float cast) read it at the row; a mantissa kept alone would
        // lose its scale
        crate::refuse!(cx, Reason::Bug { payload: "a dynamic-scale packed numeric lane kept past its row" });
    }
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_read, l_conv, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
    match c.null {
        Some(n) => cx.b.cond_br(n, (l_out, &[zero]), (l_read, &[])),
        None => cx.b.br(l_read, &[]),
    }
    cx.b.switch_to(l_read);
    let (verbatim, w) = element(cx, c);
    cx.b.cond_br(verbatim, (l_conv, &[]), (l_out, &[w]));
    cx.b.switch_to(l_conv);
    let s = cx.b.iconst(Type::I64, scale(c.cell));
    let h = cx.b.declare_host_once("rep_num_mantissa", &[Type::I64, Type::I64], Some(Type::I64), gravity_abi::host_words!(num_mantissa(datum, scale)), HostFlags { touches_memory: true, pure: true, ..HostFlags::default() });
    let m = cx.b.call_host(h, &[w, s], Some(Type::I64)).expect("returns the mantissa");
    cx.b.br(l_out, &[m]);
    cx.b.switch_to(l_out);
    let m = cx.b.block_params(l_out)[0];
    let rep = Rep::PackedNum { facts: rep::num_of(c.cell.rep), form: NumForm::Word };
    let life = CellLife { until: Until::Statement, home: CellHome::Lane(Home::Word) };
    Kept { cell: Cell { rep, life, at: crate::rep::word::datum::off_lane(c.cell.at), ..*c.cell }, val: m, null: c.null }
}

use super::raw::datum::{num_image, num_mantissa};

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    /// A mantissa renders as PostgreSQL's stored image, and the image's mantissa is the word again.
    #[test]
    fn a_mantissa_and_its_image_round_trip() {
        let mut out = [0u64; 6];
        // the widest images (19 digits over 6 base-10000 groups) fit `NUM_IMAGE_BYTES`
        let widest = [(-999_999_999_999_999_999, 1), (1_234_567_890_123_456_789, 3)];
        for (m, s) in [(12345i64, 2i64), (-7, 3), (0, 2), (999_999_999_999_999_999, 4), (gravity_fn::agg_final::MANT_NAN, 2)].into_iter().chain(widest) {
            // SAFETY: `out` is 48 writable bytes (num_image writes at most `NUM_IMAGE_BYTES`); the image is a live varlena.
            let p = unsafe { super::num_image(m, s, out.as_mut_ptr() as u64) };
            assert_ne!(p, 0);
            // SAFETY: `p` is the live image just written into `out`.
            assert_eq!(unsafe { super::num_mantissa(p, s) }, m, "{m} at scale {s}");
        }
    }

    /// The images are PostgreSQL's own (`make_result`: a 4-byte header, the short n_header, base-10000 digits).
    #[test]
    fn images_are_postgres_numeric_bytes() {
        let cases: [(i64, i64, &[u8]); 4] = [
            (12345, 2, &[0x28, 0, 0, 0, 0x00, 0x81, 0x7B, 0x00, 0x94, 0x11]), // 123.45: digits 123 | 4500, dscale 2
            (-7, 3, &[0x20, 0, 0, 0, 0xFF, 0xA1, 0x46, 0x00]),                  // -0.007: sign, dscale 3, weight -1, digit 70
            (0, 2, &[0x18, 0, 0, 0, 0x00, 0x81]),                              // 0.00
            (gravity_fn::agg_final::MANT_NAN, 2, &[0x18, 0, 0, 0, 0x00, 0xC0]), // NaN
        ];
        let mut out = [0u64; 6];
        for (m, s, want) in cases {
            // SAFETY: as above.
            let p = unsafe { super::num_image(m, s, out.as_mut_ptr() as u64) } as usize as *const u8;
            // SAFETY: the image at `p` is at least `want.len()` bytes (the case lists a prefix of it).
            let got = unsafe { core::slice::from_raw_parts(p, want.len()) };
            assert_eq!(got, want, "{m} at scale {s}");
        }
    }
}
