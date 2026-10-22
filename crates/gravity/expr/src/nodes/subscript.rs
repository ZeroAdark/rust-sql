//! `SubscriptingRef`: the interpreter's node is `array.rs`'s; the emitted fetch is a host site the compiler adds
//! per node — a shim with no address in the IR (`SUBSCRIPT_SITE`) whose two leading words are the node's facts (expr.md §7).

pub use crate::nodes::array::{compile_subscript as compile, eval_subscript as eval, Subscript as Subscripting};

use mcx::GrowIn;
use gravity_fn::{Addr, Arg, ArgKind, Datum, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR, NULL, OK};
use types_error::PgResult;

use crate::nodes::array::{fetch, index_of, ElemFacts, Handler, Subs, MAXDIM};
use crate::nodes::func::{ArgConv, Call, How};
use crate::nodes::{Builder, Kids, NodeId, Program, SiteId};

/// The arguments before the subscripts: the two fact words, then the container.
pub const HEAD: usize = 3;

const JSONB: i64 = 1 << 57;
const HSTORE: i64 = 1 << 58;
pub(crate) const SLICE: i64 = 1 << 3;

pub fn fetch_site(b: &mut Builder<'_, '_>, provided: usize, byref: bool) -> SiteId {
    let nargs = HEAD + provided;
    let convs = b.prog.convs.len() as u32;
    for _ in 0..nargs {
        b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    }
    b.site(Call { funcid: 0, args: Kids::NONE, nargs: nargs as u8, strict: true, collation: 0, how: How::Shim { shim: &SUBSCRIPT_SITE, convs, byref }, rettype: 0, volatile: false })
}

fn mask(p: &Program<'_>, k: Kids) -> i64 {
    (0..k.len as usize).filter(|&i| p.kids[k.start as usize + i] != NodeId::NONE).fold(0, |m, i| m | 1 << i)
}

/// The node's facts as the site's two leading words.
pub fn words(p: &Program<'_>, n: &Subscripting) -> [i64; 2] {
    let slice = if n.lower.len != 0 { SLICE } else { 0 };
    let w1 = n.upper.len as i64 | slice | mask(p, n.upper) << 4 | mask(p, n.lower) << 10;
    match n.handler {
        Handler::Array { attrlen, elem } => {
            let w0 = attrlen as u32 as i64 | (elem.len as u16 as i64) << 32 | (elem.byval as i64) << 48 | (elem.align as u8 as i64) << 49;
            [w0, w1 | (elem.typ as i64) << 32]
        }
        Handler::Jsonb { int4_index } => [JSONB, w1 | (0..MAXDIM).filter(|&i| int4_index[i]).fold(0, |m, i| m | 1 << (16 + i))],
        Handler::Hstore => [HSTORE, w1],
    }
}

pub(crate) fn handler_of(w0: i64, w1: i64) -> Handler {
    if w0 & HSTORE != 0 {
        return Handler::Hstore;
    }
    if w0 & JSONB != 0 {
        let mut int4_index = [false; MAXDIM];
        for (i, f) in int4_index.iter_mut().enumerate() {
            *f = w1 >> (16 + i) & 1 != 0;
        }
        return Handler::Jsonb { int4_index };
    }
    let elem = ElemFacts { typ: (w1 >> 32) as u32, len: (w0 >> 32) as u16 as i16, byval: w0 >> 48 & 1 != 0, align: (w0 >> 49) as u8 as i8 };
    Handler::Array { attrlen: w0 as u32 as i32, elem }
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

/// The fetch as a shim: `args[0..2]` the fact words, `args[2]` the container, then the provided subscripts (the
/// upper list, then the lower); strict, so a null container or subscript never reaches it.
pub static SUBSCRIPT_SITE: Shim = Shim { oid: 0, name: "subscript fetch", family: Family::None, nargs: HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V, ArgKind::D], tyargs: &[], addr: Addr(no_addr), call: fetch_shim };

fn fetch_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    // SAFETY: the host passes the cell's live region.
    match run(a, unsafe { &mut *region }) {
        Ok(v) if v.isnull => Ret { val: 0, status: NULL },
        Ok(v) => Ret { val: v.value.as_i64(), status: OK },
        Err(err) => {
            // SAFETY: the host passes the cell's live slot; a previous error in it is dropped through `take`.
            unsafe {
                drop((*e).take());
                (*e).err = gravity_abi::word(Box::into_raw(err));
            }
            Ret { val: 0, status: ERROR }
        }
    }
}

fn run(a: &[Arg], region: &mut Region) -> PgResult<NullableDatum> {
    let (w0, w1) = (a[0].v(), a[1].v());
    let handler = handler_of(w0, w1);
    let (nupper, slice) = ((w1 & 7) as usize, w1 & SLICE != 0);
    let mut s = Subs::new();
    let mut next = HEAD;
    for (shift, lower) in [(4, false), (10, true)] {
        for i in 0..if lower && !slice { 0 } else { nupper } {
            let provided = w1 >> (shift + i) & 1 != 0;
            if lower {
                s.lower_provided[i] = provided;
            } else {
                s.upper_provided[i] = provided;
            }
            if !provided {
                continue;
            }
            let v = a[next].v();
            next += 1;
            s.index[i] = index_of(handler, i, v, region)?;
            if lower {
                s.lower[i] = v as i32;
            } else {
                s.upper[i] = v as i32;
            }
        }
    }
    fetch(handler, nupper, slice, NullableDatum::value(Datum::from_i64(a[HEAD - 1].v())), &mut s, region)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fact_words_round_trip() {
        let elem = ElemFacts { typ: 1043, len: -1, byval: false, align: b'i' as i8 };
        let w0 = -1i32 as u32 as i64 | (elem.len as u16 as i64) << 32 | (elem.byval as i64) << 48 | (elem.align as u8 as i64) << 49;
        let Handler::Array { attrlen, elem: back } = handler_of(w0, (elem.typ as i64) << 32 | 2) else { panic!("an array handler") };
        assert_eq!((attrlen, back.typ, back.len, back.byval, back.align), (-1, 1043, -1, false, b'i' as i8));
        let Handler::Jsonb { int4_index } = handler_of(JSONB, 1 | 1 << 16) else { panic!("a jsonb handler") };
        assert_eq!(int4_index, [true, false, false, false, false, false]);
        assert!(matches!(handler_of(HSTORE, 1), Handler::Hstore));
    }
}
