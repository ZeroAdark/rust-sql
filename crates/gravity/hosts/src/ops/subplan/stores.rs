//! M8's host implementations (M8-plan.md §1.4 / §6, cards/M8.md L2): the stores behind the M8 handle the kernels entries
//! (host_abi/v4/m8.rs) call through: the Memoize table with its LRU and PG's byte accounting under
//! `get_hash_memory_limit()`, the hashed SubPlan's key set over the M6 Rep images, the ARRAY sublink's accumulator
//! (`accumArrayResult`) and the CTE spool with one read position per reader. An L1 stub: M8 derives no source of its
//! own (a spool or CTE reader walks its run in IR; the inner's sites derive through the earlier milestones' rows).
//!
//! RT-2b (V4R-428): `ArrayAcc`, one calling lane's ARRAY sublink accumulator (PG `initArrayResult` / `accumArrayResult` /
//! `makeArrayResult` over arrayfuncs' own code, so NULL elements, the empty '{}' and lower bound 1 are PG's), reached from
//! exec's `nest.subplan.result` object (exec v4/m8_sub_array.rs). Created by M8-RUNTIME. v5 subplan 14: PG's
//! `accumArrayResultAny` road, so an element that is itself an array builds the N+1-D result (`accumArrayResultArr`: its
//! dimension check, null and empty-array errors are PG's) and a record element is copied like any varlena.
use datum::array_build::{ArrayBuildState, ArrayBuildStateAny};
use datum::Datum;
use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;
use mcx::{MemoryContext, PgVec};
use types_core::Oid;
use types_error::{PgError, PgResult};

use crate::ops::Recipe;

/// No recipe kind of M8's own: the L1 stub derives nothing.
pub fn derive(_r: &Recipe, _i: usize, _desc: &SiteDesc, _a: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

/// An ARRAY sublink's element type as PG's `initArrayResultAny` reads it (`get_array_type`, `get_typlenbyvalalign`), looked
/// up once at prepare on the leader, so a lane's accumulator never reads the catalog.
#[derive(Clone, Copy, Debug)]
pub struct ElemShape {
    pub oid: Oid,
    pub typlen: i16,
    pub byval: bool,
    pub align: u8,
    /// `Some(element type)` when the element is itself an array (a type with no array type of its own: PG's
    /// `accumArrayResultArr` road; 0 = not an array, which `initArrayResultArr` raises at the first begin as PG does);
    /// `None` for the scalar road (int2vector and oidvector included, as PG).
    pub sub: Option<Oid>,
}

/// `oid`'s shape. Oid 0 is an error by name: the lowering refuses it first (M8-SUB), so reaching here is a bug.
pub fn elem_shape(oid: Oid) -> PgResult<ElemShape> {
    if oid == 0 {
        return Err(Box::new(PgError::error("gravity: nest.subplan: an ARRAY sublink without its element type".to_string()))); // alloc: Cold
    }
    let sub = if lsyscache::get_array_type(oid)? == 0 { Some(lsyscache::get_element_type(oid)?) } else { None };
    let (typlen, byval, align) = gravity_fn::typesem::storage::len_byval_align(oid)?;
    Ok(ElemShape { oid, typlen, byval, align: align as u8, sub })
}

/// One lane's accumulator. Its context is unobserved (the m6_agg `Slot` rule): made on the calling lane's thread at its
/// first `begin`, used only there, dropped with the execution's object on any thread. `begin` resets it, so the previous
/// call's array (PG's `curArray`) is valid until the same lane's next `begin`, and memory stays one call's worth.
pub struct ArrayAcc {
    // `state` and `result` point into `cx`, a child of `_root`: declared first, they drop first.
    state: Option<ArrayBuildStateAny<'static>>,
    result: Option<PgVec<'static, u8>>,
    /// Reset wholesale at each begin (PG's private build context: the element copies are never freed one by one).
    cx: Box<MemoryContext>, // alloc: Exec
    _root: Box<MemoryContext>, // alloc: Exec
}

impl Default for ArrayAcc {
    fn default() -> ArrayAcc {
        let root = Box::new(MemoryContext::new_unobserved("gravity ARRAY sublink")); // alloc: Exec
        let cx = Box::new(root.new_child_wholesale("gravity ARRAY sublink build")); // alloc: Exec
        ArrayAcc { state: None, result: None, cx, _root: root }
    }
}

impl ArrayAcc {
    /// PG `initArrayResultAny`: the previous result and elements go (the context resets), a new empty build starts.
    pub fn begin(&mut self, e: &ElemShape) -> PgResult<()> {
        self.state = None;
        self.result = None;
        self.cx.reset();
        let st = match e.sub {
            Some(sub) => ArrayBuildStateAny { scalarstate: None, arraystate: Some(arrayfuncs::init_array_result_arr(super::raw::acc_mcx(&self.cx), e.oid, sub)?) },
            None => {
                let mut st = ArrayBuildState::new(super::raw::acc_mcx(&self.cx), e.oid, true)?;
                st.typlen = e.typlen;
                st.typbyval = e.byval;
                st.typalign = e.align;
                ArrayBuildStateAny { scalarstate: Some(st), arraystate: None }
            }
        };
        self.state = Some(st);
        Ok(())
    }

    /// PG `accumArrayResultAny`: one element appended; a by-reference value is copied (a varlena detoasted) into the
    /// context; an array element appends its items (2202E on another dimensionality, 22004 on a NULL array). `value` is the
    /// whole datum word (D3209: a by-value int8 / float8 / timestamp past 2^32 kept only its low half on wasm32, where `usize`
    /// is 32 bits and the Datum word 64).
    pub fn push(&mut self, value: u64, null: bool, e: &ElemShape) -> PgResult<()> {
        let Some(st) = self.state.take() else { return Err(not_begun()) };
        self.state = Some(arrayfuncs::accum_array_result_any(super::raw::acc_mcx(&self.cx), Some(st), Datum::from_u64(value), null, e.oid)?);
        Ok(())
    }

    /// The lane context's bytes in use (one call's elements and array: flat however many calls a lane makes).
    pub fn bytes(&self) -> usize {
        self.cx.used()
    }

    /// PG `makeArrayResultAny`: the 1-D array, or N+1-D over array elements (lower bound 1; no elements = the empty array),
    /// valid until the next `begin`.
    pub fn end(&mut self) -> PgResult<*const u8> {
        let Some(st) = self.state.as_ref() else { return Err(not_begun()) };
        let arr = arrayfuncs::make_array_result_any(super::raw::acc_mcx(&self.cx), st)?;
        let at = arr.as_ptr();
        self.result = Some(arr);
        Ok(at)
    }
}

#[cold]
fn not_begun() -> Box<PgError> {
    Box::new(PgError::error("gravity: nest.subplan: an ARRAY sublink element or end before its begin".to_string())) // alloc: Cold
}
