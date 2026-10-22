//! The aggregate library (docs/gravity/expr-compiler.md §4.6, expr.md P5): `Agg::compile` turns
//! an `Aggref` into an `AggPlan` — the transition, combine and final forms the kernels write
//! inline today, named so the emit lane lowers them and so tier 0 runs them here. The state is
//! an `AggState` the caller keeps per group (by-reference states are images in the caller's
//! region); nothing here allocates per row. The builtin count / sum / avg / min / max / bool /
//! string_agg / array_agg families are native forms; any other aggregate whose transition is a
//! strict shim or a plain function-manager call is `Generic`; the ordered-set and hypothetical
//! kinds and the aggregates whose transition reads the aggregate context are refused by name.

use core::fmt;

use gravity_fn::{agg_final, Arg, ArgKind, Datum, ErrSlot, FmgrCall, NullableDatum, Region, Shim};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::Aggref;

use crate::catalog::Catalog;
use crate::emit::vec::{VecAcc, VecEmit, VecFinal, VecLowered};
use gravity_ir::builder::FunctionBuilder;
use crate::nodes::array::{elem_facts_of, facts_of_elem, with_scratch, ElemFacts};
use crate::nodes::Kind;
use crate::views::{copy_into, datum_of, image_view, no_space, payload_view};
use crate::{CompileError, CompileResult};

const INT8OID: Oid = 20;
const FLOAT4OID: Oid = 700;
const FLOAT8OID: Oid = 701;
const NUMERICOID: Oid = 1700;
const TEXTOID: Oid = 25;
const BYTEAOID: Oid = 17;
const BOOLOID: Oid = 16;
const AGGKIND_NORMAL: i8 = b'n' as i8;

/// The transition form: the inline kernel forms first, then the calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    /// `count(*)`: `count += 1`.
    CountStar,
    /// `count(x)`: `count += !null`.
    CountAny,
    /// `sum(int2 | int4)`: an `i64` sum, null until the first value (the `int8` result).
    SumInt,
    /// `sum(int8)` / `avg(int8)`: an `i128` sum (`lo`, `hi`) and a count; numeric at the final.
    SumInt128,
    /// `sum(float4)`: an `f32` sum.
    SumF32,
    /// `sum(float8)`: an `f64` sum.
    SumF64,
    /// `avg(int2 | int4)`: an `i64` sum and a count; `sum / count` as numeric at the final.
    AvgInt,
    /// `avg(float4 | float8)`: an `f64` sum and a count; `sum / n` at the final.
    AvgF64,
    /// `sum(numeric)` / `avg(numeric)`: the exact sum as a numeric image in the region and a count.
    SumNumeric,
    /// `min` / `max` (and every strict aggregate whose state is its own input type): the state is
    /// the transition function's result over (state, x); the first value seeds it.
    MinMax,
    BoolAnd,
    BoolOr,
    /// `string_agg(text | bytea, delimiter)`: a growing byte buffer in the region.
    StringAgg,
    /// `array_agg(anynonarray)`: the element cells in the region; an array at the final.
    ArrayAgg,
    /// Any other transition: a strict call over (state, args…) with `initval` as the seed.
    Generic,
}

/// How the final result is produced from the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Final {
    /// The state is the result (count, sum, min / max, bool, sum(numeric)).
    Identity,
    /// `agg_final::int8_sum`: the `i128` sum as numeric.
    Int128Sum,
    /// `agg_final::int8_avg`: the `i128` sum over the count.
    Int128Avg,
    /// `agg_final::int_avg`: the `i64` sum over the count.
    IntAvg,
    /// `agg_final::float8_avg`.
    F64Avg,
    /// `numeric_div(sum, count)` (C `numeric_avg`).
    NumericAvg,
    /// The `string_agg` buffer as a text / bytea image.
    Text,
    /// The `array_agg` cells as a one-dimensional array.
    Array,
    /// A final function call over the state (`Generic` only).
    Call,
}

/// The vector transition of a plan (expr-compiler.md §8.2): what `emit::vec` lowers per chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VecAgg {
    /// `count(*)` / `count(x)` over a mask chunk: the filtered count.
    Count,
    /// `sum(int2 | int4)`, the sum half of `avg(int2 | int4)`: widened into the window's lane.
    Sum,
    /// `sum(int8)` / `avg(int8)`: the `V2I64` lo / hi pair with the carry.
    Sum128,
    Min,
    Max,
    UMin,
    UMax,
}

/// A bound call: a shim (arguments widened or viewed per `convs`) or the function manager.
pub enum Callee {
    Shim { shim: &'static Shim, convs: [(ArgKind, i16); 4], byref: bool },
    Fmgr(FmgrCall),
}

impl fmt::Debug for Callee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Callee::Shim { shim, .. } => write!(f, "Shim({})", shim.name),
            Callee::Fmgr(_) => write!(f, "Fmgr"),
        }
    }
}

pub struct AggPlan {
    pub aggfnoid: Oid,
    pub name: &'static str,
    pub transition: Transition,
    pub final_: Final,
    pub state_type: Oid,
    pub result_type: Oid,
    /// The words a function-manager transition / combine (`state_word`) and final (`result_word`) result is
    /// canonized as: the state / result type, a domain's base type (D2336).
    pub state_word: Oid,
    pub result_word: Oid,
    pub strict: bool,
    /// The state's typlen (by-value states: `len > 0 && byval`).
    pub state_len: i16,
    pub state_byval: bool,
    pub nargs: u8,
    pub collation: Oid,
    /// `count(DISTINCT x)`: the cell is a pair set's count; `transition` / `combine` refuse (no tier-0 fold).
    pub distinct: bool,
    /// The seed for `Generic` (`agginitval` through the state type's input function).
    pub init: NullableDatum,
    pub transfn: Option<Callee>,
    pub combinefn: Option<Callee>,
    pub finalfn: Option<Callee>,
    /// `ArrayAgg`: the element facts; `StringAgg`: unused.
    pub elem: Option<ElemFacts>,
    init_image: Option<Box<[u8]>>,
}

/// One group's state: `value` is the by-value word or the image address; `count` and `hi` are
/// the second and third words the native forms keep (a count, the high half of an `i128`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggState {
    pub value: NullableDatum,
    pub count: i64,
    pub hi: i64,
}

impl AggState {
    pub const EMPTY: AggState = AggState { value: NullableDatum::null(), count: 0, hi: 0 };
}

/// `avg(float)`'s state as PG's float8[3] {N, Sx, Sxx}: `count`, `value`, the bits of `hi` (all zero while empty).
fn f64_accum(st: &AggState) -> [f64; 3] {
    if st.count == 0 {
        return [0.0; 3];
    }
    [st.count as f64, st.value.value.as_f64(), f64::from_bits(st.hi as u64)]
}

fn set_f64_accum(st: &mut AggState, n: i64, sx: f64, sxx: f64) {
    st.value = NullableDatum::value(Datum::from_f64(sx));
    st.count = n;
    st.hi = sxx.to_bits() as i64;
}

pub struct Agg;

impl Agg {
    pub fn compile<'mcx>(a: &Aggref<'mcx>, catalog: &dyn Catalog<'mcx>) -> CompileResult<AggPlan> {
        Self::compile_with(a, catalog, false)
    }

    /// The gates an Aggref's cell passes whoever folds it: no ordered-set / hypothetical-set kind, no ORDER BY inside
    /// the call unless `aggorder`, DISTINCT only on the pair record's natives. `compile_with` runs them first; a lowerer
    /// that folds every aggregate itself (the Builder's `folded`, lowerv4) runs only these (M6-FOLD R2).
    pub fn cell_gate(a: &Aggref<'_>, aggorder: bool) -> CompileResult<()> {
        if a.aggkind != AGGKIND_NORMAL || !a.aggdirectargs.is_nil() {
            return Err(refuse(a.aggfnoid, "ordered-set / hypothetical-set aggregate"));
        }
        let arg0 = if a.aggargtypes.len() > 0 { a.aggargtypes.nth(0) } else { 0 };
        let native = native_form(a.aggfnoid, arg0);
        // `count / sum / avg / min / max(DISTINCT x)` are the pair record's cells (lower.md §6.12, M5 §4.5): bound like any cell, never folded at tier 0 (`no_distinct_fold`)
        if (!aggorder && !a.aggorder.is_nil()) || (!a.aggdistinct.is_nil() && !matches!(native, Some((Transition::CountAny | Transition::SumInt | Transition::SumInt128 | Transition::AvgInt | Transition::MinMax, _, _)))) {
            return Err(refuse(a.aggfnoid, "ORDER BY / DISTINCT inside the aggregate"));
        }
        Ok(())
    }

    /// The gate of a lowerer that folds every aggregate itself (the Builder's `folded`, lowerv4): `cell_gate`, but a
    /// DISTINCT aggregate's cell passes whatever its class and ORDER BY (D1210): lowerv4's ends fold it (the pair record,
    /// or host-sorted in its DISTINCT clause's order, which its ORDER BY only prefixes) or refuse it by name; so does an
    /// ordered-set / hypothetical-set one without DISTINCT (V4R-425: class.ordered, the hosted fold).
    pub fn folded_gate(a: &Aggref<'_>, aggorder: bool) -> CompileResult<()> {
        if a.aggkind != AGGKIND_NORMAL && a.aggdistinct.is_nil() {
            return Ok(()); // V4R-425: lowerv4's class.ordered folds it (its transfn sorts the WITHIN GROUP input) or refuses by name
        }
        if a.aggdistinct.is_nil() {
            return Self::cell_gate(a, aggorder);
        }
        if a.aggkind != AGGKIND_NORMAL || !a.aggdirectargs.is_nil() {
            return Err(refuse(a.aggfnoid, "ordered-set / hypothetical-set aggregate"));
        }
        Ok(())
    }

    /// `compile`, with `aggorder` lifting the ORDER BY gate (lowerv4 M3 decision 15, D346: the v4 Builder flag, off until
    /// every keyed end checks `mods.order`; the lowerer folds an ordered aggregate itself, `runs.group`).
    pub fn compile_with<'mcx>(a: &Aggref<'mcx>, catalog: &dyn Catalog<'mcx>, aggorder: bool) -> CompileResult<AggPlan> {
        Self::cell_gate(a, aggorder)?;
        let distinct = !a.aggdistinct.is_nil();
        let nargs = a.aggargtypes.len();
        let arg0 = if nargs > 0 { a.aggargtypes.nth(0) } else { 0 };
        let native = native_form(a.aggfnoid, arg0);
        let (transition, final_, state_type) = match native {
            Some(n) => n,
            None => (Transition::Generic, Final::Identity, a.aggtranstype),
        };
        let mut plan = AggPlan {
            aggfnoid: a.aggfnoid,
            name: name_of(a.aggfnoid),
            transition,
            final_,
            state_type,
            result_type: a.aggtype,
            state_word: state_type,
            result_word: a.aggtype,
            strict: true,
            state_len: 8,
            state_byval: true,
            nargs: nargs as u8,
            collation: a.inputcollid,
            distinct,
            init: NullableDatum::null(),
            transfn: None,
            combinefn: None,
            finalfn: None,
            elem: None,
            init_image: None,
        };
        match transition {
            Transition::MinMax | Transition::Generic => {
                let shape = syscache_seams::lookup_pg_aggregate_shape::call(a.aggfnoid)?.ok_or_else(|| refuse(a.aggfnoid, "no pg_aggregate row"))?;
                let tf = catalog.type_facts(shape.aggtranstype)?;
                plan.state_type = shape.aggtranstype;
                plan.state_word = crate::nodes::func::word_type(catalog, plan.state_type);
                plan.result_word = crate::nodes::func::word_type(catalog, plan.result_type);
                plan.state_len = tf.len;
                plan.state_byval = tf.byval;
                plan.strict = catalog.func(shape.aggtransfn)?.strict;
                if transition == Transition::Generic {
                    if !plan.strict {
                        return Err(refuse(a.aggfnoid, "a non-strict transition function"));
                    }
                    if shape.aggfinalextra || shape.aggmtransfn != 0 && shape.aggtransfn == 0 {
                        return Err(refuse(a.aggfnoid, "a final function with extra arguments"));
                    }
                    if matches!(catalog.tier(shape.aggtransfn), gravity_fn::Tier::C) && context_bound(shape.aggtransfn) {
                        return Err(refuse(a.aggfnoid, "a transition function that reads the aggregate context"));
                    }
                }
                let mut types = [plan.state_type; 4];
                for i in 0..nargs.min(3) {
                    types[i + 1] = a.aggargtypes.nth(i);
                }
                plan.transfn = Some(bind(catalog, shape.aggtransfn, &types[..nargs + 1], a.inputcollid)?);
                if shape.aggcombinefn != 0 {
                    plan.combinefn = Some(bind(catalog, shape.aggcombinefn, &[plan.state_type, plan.state_type], a.inputcollid)?);
                }
                if shape.aggfinalfn != 0 {
                    plan.final_ = Final::Call;
                    plan.finalfn = Some(bind(catalog, shape.aggfinalfn, &[plan.state_type], a.inputcollid)?);
                }
                if transition == Transition::Generic {
                    plan.load_initval(catalog)?;
                }
            }
            Transition::ArrayAgg => {
                plan.elem = Some(facts_of_elem(catalog, arg0)?);
                let _ = elem_facts_of(catalog, a.aggtype, Kind::Aggref)?;
            }
            _ => {}
        }
        Ok(plan)
    }
}

/// The builtin aggregates the kernels write inline: by `aggfnoid` (pg_aggregate.dat).
/// The inline forms of the built-in aggregates by function OID and first argument type (the window
/// lowerer classifies a windowed aggregate through it).
pub fn native_form(aggfnoid: Oid, arg0: Oid) -> Option<(Transition, Final, Oid)> {
    use gravity_fn::typesem::expr::NativeAgg as N;
    Some(match gravity_fn::typesem::expr::native_agg(aggfnoid)? {
        N::CountStar => (Transition::CountStar, Final::Identity, INT8OID),
        N::CountAny => (Transition::CountAny, Final::Identity, INT8OID),
        N::SumInt => (Transition::SumInt, Final::Identity, INT8OID),
        N::SumInt8 => (Transition::SumInt128, Final::Int128Sum, NUMERICOID),
        N::AvgInt8 => (Transition::SumInt128, Final::Int128Avg, NUMERICOID),
        N::SumF4 => (Transition::SumF32, Final::Identity, FLOAT4OID),
        N::SumF8 => (Transition::SumF64, Final::Identity, FLOAT8OID),
        N::AvgInt => (Transition::AvgInt, Final::IntAvg, NUMERICOID),
        N::AvgFloat => (Transition::AvgF64, Final::F64Avg, FLOAT8OID),
        N::SumNumeric => (Transition::SumNumeric, Final::Identity, NUMERICOID),
        N::AvgNumeric => (Transition::SumNumeric, Final::NumericAvg, NUMERICOID),
        N::BoolAnd => (Transition::BoolAnd, Final::Identity, BOOLOID),
        N::BoolOr => (Transition::BoolOr, Final::Identity, BOOLOID),
        N::StringAggText => (Transition::StringAgg, Final::Text, TEXTOID),
        N::StringAggBytea => (Transition::StringAgg, Final::Text, BYTEAOID),
        N::ArrayAgg => (Transition::ArrayAgg, Final::Array, 0),
        N::MinMax => (Transition::MinMax, Final::Identity, arg0),
    })
}

fn name_of(aggfnoid: Oid) -> &'static str {
    fmgr_core::CANONICAL.binary_search_by_key(&aggfnoid, |r| r.0).map(|i| fmgr_core::CANONICAL[i].1).unwrap_or("aggregate")
}

/// A tier-C transition function that needs the aggregate's memory context (`fcinfo->context`).
fn context_bound(funcid: Oid) -> bool {
    gravity_fn::row_of(funcid).is_some_and(|r| r.reason.starts_with("context: fcinfo->context") || r.reason.starts_with("handler/internal"))
}

fn bind<'mcx>(catalog: &dyn Catalog<'mcx>, funcid: Oid, types: &[Oid], collation: Oid) -> CompileResult<Callee> {
    let facts = catalog.func(funcid)?;
    if let Some(shim) = catalog.shim(funcid) {
        if shim.nargs as usize == types.len() && shim.tyargs.is_empty() && facts.strict && types.len() <= 4 {
            let mut convs = [(ArgKind::V, 0i16); 4];
            for (i, &t) in types.iter().enumerate() {
                let kind = shim.args[i];
                let typlen = match kind {
                    ArgKind::V | ArgKind::D => 0,
                    _ => catalog.type_facts(t)?.len,
                };
                convs[i] = (kind, typlen);
            }
            return Ok(Callee::Shim { shim, convs, byref: shim.region });
        }
    }
    Ok(Callee::Fmgr(FmgrCall::bind(funcid, types.len(), collation)?))
}

impl AggPlan {
    fn load_initval<'mcx>(&mut self, catalog: &dyn Catalog<'mcx>) -> CompileResult<()> {
        let cx = gravity_fn::session::root_context("gravity_expr agginitval"); // alloc: Program
        let Some(Some(text)) = syscache_seams::pg_aggregate_agginitval::call(cx.mcx(), self.aggfnoid)? else { return Ok(()) };
        let io = catalog.io(self.state_type)?;
        let mut call = FmgrCall::bind(io.input, 3, 0)?;
        let mut cstr = text.as_str().as_bytes().to_vec(); // alloc: Program
        cstr.push(0);
        let v = call.call(cx.mcx(), &[Datum::from_usize(cstr.as_ptr() as usize), Datum::from_u32(io.ioparam), Datum::from_i32(-1)], &[false, false, false])?;
        if v.isnull || self.state_byval {
            self.init = v;
            return Ok(());
        }
        let p = v.value.as_usize() as *const u8;
        // SAFETY: the input function's result is a live image of the state type.
        let n = unsafe {
            match self.state_len {
                -1 => types_tuple::varatt::varsize_any(p),
                -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() + 1,
                n => n as usize,
            }
        };
        // SAFETY: `n` bytes are readable at `p` per the typlen contract.
        let img: Box<[u8]> = unsafe { core::slice::from_raw_parts(p, n) }.into(); // alloc: Program
        self.init = NullableDatum::value(Datum::from_usize(img.as_ptr() as usize));
        self.init_image = Some(img);
        Ok(())
    }

    pub fn vec_agg(&self) -> Option<VecAgg> {
        Some(match (self.transition, self.aggfnoid) {
            (Transition::CountStar | Transition::CountAny, _) => VecAgg::Count,
            (Transition::SumInt | Transition::AvgInt, _) => VecAgg::Sum,
            (Transition::SumInt128, _) => VecAgg::Sum128,
            (Transition::MinMax, 2131..=2133 | 2138 | 2139 | 2142 | 2143) => VecAgg::Min,
            (Transition::MinMax, 2115..=2117 | 2122 | 2123 | 2126 | 2127) => VecAgg::Max,
            (Transition::MinMax, 2134) => VecAgg::UMin,
            (Transition::MinMax, 2118) => VecAgg::UMax,
            _ => return None,
        })
    }

    pub fn vec_state_type(&self, e: &VecEmit, rows_bound: u32) -> Option<gravity_ir::Type> {
        self.vec_agg()?.state_type(e, rows_bound)
    }

    pub fn vec_transition(&self, b: &mut FunctionBuilder<'_>, acc: VecAcc, chunk: &VecLowered) -> Option<VecAcc> {
        Some(self.vec_agg()?.transition(b, acc, chunk))
    }

    pub fn vec_finish(&self, b: &mut FunctionBuilder<'_>, acc: VecAcc) -> Option<VecFinal> {
        Some(self.vec_agg()?.finish(b, acc))
    }

    /// A fresh group's state.
    #[inline]
    pub fn init(&self) -> AggState {
        match self.transition {
            Transition::CountStar | Transition::CountAny => AggState { value: NullableDatum::value(Datum::from_i64(0)), count: 0, hi: 0 },
            Transition::Generic => AggState { value: self.init, count: 0, hi: 0 },
            _ => AggState::EMPTY,
        }
    }

    /// One row into the state: `args` / `nulls` are the aggregate's arguments in order; a strict
    /// aggregate skips a row with a null argument (the caller may also skip it, this is the check).
    #[inline]
    pub fn transition(&mut self, st: &mut AggState, args: &[Datum], nulls: &[bool], region: &mut Region) -> PgResult<()> {
        if self.distinct {
            return Err(no_distinct_fold());
        }
        let any_null = nulls.iter().any(|&n| n);
        match self.transition {
            Transition::CountStar => {
                st.value.value = Datum::from_i64(st.value.value.as_i64() + 1);
            }
            Transition::CountAny => {
                if !any_null {
                    st.value.value = Datum::from_i64(st.value.value.as_i64() + 1);
                }
            }
            Transition::SumInt => {
                if !nulls[0] {
                    let x = if gravity_fn::typesem::expr::agg_int2_arg(self.aggfnoid) { args[0].as_i16() as i64 } else { args[0].as_i32() as i64 };
                    st.value = NullableDatum::value(Datum::from_i64(if st.value.isnull { x } else { st.value.value.as_i64() + x }));
                }
            }
            Transition::SumInt128 => {
                if !nulls[0] {
                    let s = i128_of(st.value.value.as_i64(), st.hi) + args[0].as_i64() as i128;
                    st.value = NullableDatum::value(Datum::from_i64(s as i64));
                    st.hi = (s >> 64) as i64;
                    st.count += 1;
                }
            }
            Transition::SumF32 => {
                if !nulls[0] {
                    let x = args[0].as_f32();
                    let s = if st.value.isnull { x } else { adt_float::float4_pl(st.value.value.as_f32(), x)? };
                    st.value = NullableDatum::value(Datum::from_f32(s));
                }
            }
            Transition::SumF64 => {
                if !nulls[0] {
                    let x = args[0].as_f64();
                    let s = if st.value.isnull { x } else { adt_float::float8_pl(st.value.value.as_f64(), x)? };
                    st.value = NullableDatum::value(Datum::from_f64(s));
                }
            }
            Transition::AvgInt => {
                if !nulls[0] {
                    let x = if gravity_fn::typesem::expr::agg_int2_arg(self.aggfnoid) { args[0].as_i16() as i64 } else { args[0].as_i32() as i64 };
                    st.value = NullableDatum::value(Datum::from_i64(st.value.value.as_i64() + x));
                    st.count += 1;
                }
            }
            Transition::AvgF64 => {
                if !nulls[0] {
                    let x = if gravity_fn::typesem::expr::agg_float4_arg(self.aggfnoid) { args[0].as_f32() as f64 } else { args[0].as_f64() };
                    // float8_accum's {N, Sx, Sxx} (D244): Sxx rides in `hi`, its overflow raised as PG raises it
                    let ([_, sx, sxx], n) = (adt_float::aggregates::float8_accum(f64_accum(st), x)?, st.count + 1);
                    set_f64_accum(st, n, sx, sxx);
                }
            }
            Transition::SumNumeric => {
                if !nulls[0] {
                    let x = payload_view(args[0], -1, region)?;
                    if st.value.isnull {
                        st.value = NullableDatum::value(copy_into(region, args[0], -1)?);
                    } else {
                        let s = payload_view(st.value.value, -1, region)?;
                        st.value = NullableDatum::value(shim_call(gravity_fn::shim_of(1724).expect("numeric_add is a shim"), region, 0, &[Arg::R(s), Arg::R(x)], true)?.value);
                    }
                    st.count += 1;
                }
            }
            Transition::MinMax => {
                if !nulls[0] {
                    if st.value.isnull {
                        st.value = NullableDatum::value(self.keep(args[0], region)?);
                    } else {
                        let r = self.call_transfn(st.value.value, &args[..1], region)?;
                        st.value = NullableDatum::value(self.keep(r.value, region)?);
                    }
                }
            }
            Transition::BoolAnd | Transition::BoolOr => {
                if !nulls[0] {
                    let x = args[0].as_bool();
                    let s = if st.value.isnull { x } else if self.transition == Transition::BoolAnd { st.value.value.as_bool() && x } else { st.value.value.as_bool() || x };
                    st.value = NullableDatum::value(Datum::from_bool(s));
                }
            }
            Transition::StringAgg => {
                if !nulls[0] {
                    let v = payload_view(args[0], -1, region)?;
                    let seeded = !st.value.isnull;
                    if seeded && !nulls[1] {
                        let d = payload_view(args[1], -1, region)?;
                        buf_append(st, region, d.bytes())?;
                    }
                    buf_append(st, region, v.bytes())?;
                    st.value.isnull = false;
                }
            }
            Transition::ArrayAgg => {
                let elem = self.elem.expect("array_agg carries its element facts");
                let kept = if nulls[0] { Datum::null() } else { keep_of(args[0], elem.len, elem.byval, region)? };
                let cell = NullableDatum { value: kept, isnull: nulls[0] };
                // SAFETY: `NullableDatum` is plain data of 16 bytes.
                buf_append(st, region, unsafe { core::slice::from_raw_parts((&cell as *const NullableDatum).cast::<u8>(), 16) })?;
                st.value.isnull = false;
            }
            Transition::Generic => {
                if any_null && self.strict {
                    return Ok(());
                }
                if st.value.isnull && self.strict {
                    st.value = NullableDatum::value(self.keep(args[0], region)?);
                    return Ok(());
                }
                let r = self.call_transfn(st.value.value, args, region)?;
                st.value = if r.isnull { r } else { NullableDatum::value(self.keep(r.value, region)?) };
            }
        }
        Ok(())
    }

    /// `st = st + other` (the combine form): the same word-wise forms over two states.
    pub fn combine(&mut self, st: &mut AggState, other: &AggState, region: &mut Region) -> PgResult<()> {
        if self.distinct {
            return Err(no_distinct_fold());
        }
        match self.transition {
            Transition::CountStar | Transition::CountAny => st.value.value = Datum::from_i64(st.value.value.as_i64() + other.value.value.as_i64()),
            Transition::SumInt => {
                if !other.value.isnull {
                    let s = if st.value.isnull { other.value.value.as_i64() } else { shim_call(gravity_fn::shim_of(463).expect("int8pl is a shim"), region, 0, &[Arg::V(st.value.value.as_i64()), Arg::V(other.value.value.as_i64())], false)?.value.as_i64() };
                    st.value = NullableDatum::value(Datum::from_i64(s));
                }
            }
            Transition::SumInt128 => {
                let s = i128_of(st.value.value.as_i64(), st.hi) + i128_of(other.value.value.as_i64(), other.hi);
                st.value = NullableDatum::value(Datum::from_i64(s as i64));
                st.hi = (s >> 64) as i64;
                st.count += other.count;
            }
            Transition::SumF32 => {
                if !other.value.isnull {
                    let s = if st.value.isnull { other.value.value.as_f32() } else { adt_float::float4_pl(st.value.value.as_f32(), other.value.value.as_f32())? };
                    st.value = NullableDatum::value(Datum::from_f32(s));
                }
            }
            Transition::SumF64 => {
                if !other.value.isnull {
                    let s = if st.value.isnull { other.value.value.as_f64() } else { adt_float::float8_pl(st.value.value.as_f64(), other.value.value.as_f64())? };
                    st.value = NullableDatum::value(Datum::from_f64(s));
                }
            }
            Transition::AvgInt => {
                st.value = NullableDatum::value(Datum::from_i64(st.value.value.as_i64() + other.value.value.as_i64()));
                st.count += other.count;
            }
            Transition::AvgF64 => {
                if other.count > 0 {
                    let ([_, sx, sxx], n) = (adt_float::aggregates::float8_combine(f64_accum(st), f64_accum(other))?, st.count + other.count);
                    set_f64_accum(st, n, sx, sxx);
                }
            }
            Transition::SumNumeric => {
                if !other.value.isnull {
                    let mut args = [Datum::null()];
                    args[0] = other.value.value;
                    let nulls = [false];
                    let saved = self.transition;
                    self.transition = Transition::SumNumeric;
                    self.transition(st, &args, &nulls, region)?;
                    self.transition = saved;
                    st.count += other.count - 1;
                }
            }
            Transition::MinMax | Transition::Generic => {
                if other.value.isnull {
                    return Ok(());
                }
                if st.value.isnull {
                    st.value = NullableDatum::value(self.keep(other.value.value, region)?);
                    return Ok(());
                }
                let r = match self.combinefn.as_mut() {
                    Some(c) => call(c, self.state_word, &[st.value.value, other.value.value], &[false, false], self.collation, region)?,
                    None => self.call_transfn(st.value.value, &[other.value.value], region)?,
                };
                st.value = if r.isnull { r } else { NullableDatum::value(self.keep(r.value, region)?) };
            }
            Transition::BoolAnd | Transition::BoolOr => {
                if !other.value.isnull {
                    let x = other.value.value.as_bool();
                    let s = if st.value.isnull { x } else if self.transition == Transition::BoolAnd { st.value.value.as_bool() && x } else { st.value.value.as_bool() || x };
                    st.value = NullableDatum::value(Datum::from_bool(s));
                }
            }
            Transition::StringAgg | Transition::ArrayAgg => {
                if !other.value.isnull {
                    buf_append(st, region, buf_bytes(other))?;
                    st.value.isnull = false;
                }
            }
        }
        Ok(())
    }

    /// The result of a group: a by-reference result is an image in the region.
    pub fn finalize(&mut self, st: &AggState, region: &mut Region) -> PgResult<NullableDatum> {
        let mut e = ErrSlot::NONE;
        let r = match self.final_ {
            Final::Identity => return Ok(st.value),
            Final::Int128Sum => agg_final::int8_sum(&mut e, region, st.value.value.as_i64(), st.hi, st.count),
            Final::Int128Avg => agg_final::int8_avg(&mut e, region, st.value.value.as_i64(), st.hi, st.count),
            Final::IntAvg => agg_final::int_avg(&mut e, region, st.value.value.as_i64(), st.count),
            Final::F64Avg => agg_final::float8_avg(&mut e, st.count, st.value.value.as_f64()),
            Final::NumericAvg => {
                if st.value.isnull {
                    return Ok(NullableDatum::null());
                }
                let n = adt_numeric::int64_to_numeric(st.count);
                let s = payload_view(st.value.value, -1, region)?;
                return shim_call(gravity_fn::shim_of(1727).expect("numeric_div is a shim"), region, 0, &[Arg::R(s), Arg::R(gravity_fn::View::of(&n.as_bytes()[4..]))], true);
            }
            Final::Text => {
                if st.value.isnull {
                    return Ok(NullableDatum::null());
                }
                let r = gravity_fn::region_payload(region, buf_bytes(st));
                return ret(r, &mut e, true);
            }
            Final::Array => {
                if st.value.isnull {
                    return Ok(NullableDatum::null());
                }
                let elem = self.elem.expect("array_agg carries its element facts");
                let cells = buf_bytes(st);
                let n = cells.len() / 16;
                // SAFETY: the buffer holds `n` `NullableDatum` cells written by `transition`.
                let cells = unsafe { core::slice::from_raw_parts(cells.as_ptr().cast::<NullableDatum>(), n) };
                return with_scratch(|mcx| {
                    let mut values = mcx::vec_with_capacity_in(mcx, n)?;
                    let mut nulls = mcx::vec_with_capacity_in(mcx, n)?;
                    for c in cells {
                        values.push(c.value);
                        nulls.push(c.isnull);
                    }
                    let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &[n as i32], &[1], elem.typ, elem.len as i32, elem.byval, elem.align as u8)?;
                    copy_into(region, datum_of(img.as_ptr()), -1).map(NullableDatum::value)
                });
            }
            Final::Call => {
                let c = self.finalfn.as_mut().expect("Final::Call binds its function");
                let strict = match c {
                    Callee::Fmgr(f) => f.strict(),
                    Callee::Shim { .. } => true,
                };
                if st.value.isnull && strict {
                    return Ok(NullableDatum::null());
                }
                return call(c, self.result_word, &[st.value.value], &[st.value.isnull], self.collation, region);
            }
        };
        ret(r, &mut e, matches!(self.final_, Final::Int128Sum | Final::Int128Avg | Final::IntAvg))
    }

    fn call_transfn(&mut self, state: Datum, args: &[Datum], region: &mut Region) -> PgResult<NullableDatum> {
        let mut a = [Datum::null(); 4];
        a[0] = state;
        a[1..1 + args.len()].copy_from_slice(args);
        let c = self.transfn.as_mut().expect("MinMax / Generic bind their transition function");
        call(c, self.state_word, &a[..1 + args.len()], &[false; 4][..1 + args.len()], self.collation, region)
    }

    /// A state value made durable in the region (a by-value word as is; an image copied).
    #[inline]
    fn keep(&self, d: Datum, region: &mut Region) -> PgResult<Datum> {
        keep_of(d, self.state_len, self.state_byval, region)
    }

    pub fn reset_window(&mut self) {
        for c in [self.transfn.as_mut(), self.combinefn.as_mut(), self.finalfn.as_mut()].into_iter().flatten() {
            if let Callee::Fmgr(f) = c {
                f.reset_window();
            }
        }
    }
}

#[inline]
fn keep_of(d: Datum, len: i16, byval: bool, region: &mut Region) -> PgResult<Datum> {
    if byval {
        return Ok(d);
    }
    if len == -1 {
        let img = image_view(d, -1, region)?;
        return copy_into(region, datum_of(img.ptr), -1);
    }
    copy_into(region, d, len)
}

#[inline(always)]
fn i128_of(lo: i64, hi: i64) -> i128 {
    ((hi as i128) << 64) | (lo as u64 as i128)
}

fn ret(r: gravity_fn::Ret, e: &mut ErrSlot, byref: bool) -> PgResult<NullableDatum> {
    match r.status {
        gravity_fn::OK => Ok(NullableDatum::value(if byref { Datum::from_usize(r.val as usize) } else { Datum::from_i64(r.val) })),
        gravity_fn::NULL => Ok(NullableDatum::null()),
        gravity_fn::ERROR => Err(e.take().expect("a shim's ERROR carries its error")),
        _ => Err(no_space()),
    }
}

fn shim_call(shim: &'static Shim, region: &mut Region, coll: Oid, args: &[Arg], byref: bool) -> PgResult<NullableDatum> {
    let mut e = ErrSlot::NONE;
    let r = (shim.call)(&mut e, region, coll, &[], args);
    ret(r, &mut e, byref)
}

/// `rettype` is the callee's result type: a function-manager word for an int2 / int4 result is sign-extended (D2337, as
/// nodes/func.rs `invoke` does for a scalar call, D2311); a shim encodes its own word.
fn call(c: &mut Callee, rettype: Oid, args: &[Datum], nulls: &[bool], coll: Oid, region: &mut Region) -> PgResult<NullableDatum> {
    match c {
        Callee::Fmgr(f) => f.call_site(args, nulls).map(|v| crate::nodes::func::int_result(rettype, v)),
        Callee::Shim { shim, convs, byref } => {
            let mut a = [Arg::V(0); 4];
            for i in 0..args.len() {
                let (kind, typlen) = convs[i];
                a[i] = match kind {
                    ArgKind::V | ArgKind::D => Arg::V(args[i].as_i64()),
                    ArgKind::P => Arg::R(payload_view(args[i], typlen, region)?),
                    ArgKind::I => Arg::R(image_view(args[i], typlen, region)?),
                };
            }
            shim_call(shim, region, coll, &a[..args.len()], *byref)
        }
    }
}

/// The growing buffer of `StringAgg` / `ArrayAgg`: `value` its address, `count` the length, `hi`
/// the capacity; it grows in place while it is the region's last allocation, else moves.
fn buf_append(st: &mut AggState, region: &mut Region, bytes: &[u8]) -> PgResult<()> {
    let len = st.count as usize;
    let cap = st.hi as usize;
    let need = len + bytes.len();
    let base = if st.value.isnull && cap == 0 { core::ptr::null_mut() } else { st.value.value.as_usize() as *mut u8 };
    let base = if need <= cap {
        base
    } else {
        let newcap = need.next_power_of_two().max(64);
        // SAFETY: `base + cap` is the end of this buffer's reservation; when it is the region's
        // cursor the buffer extends in place.
        // D641: in place only when the reservation is the adjacent bytes (a spilled region moved elsewhere: copy)
        let at = if !base.is_null() && unsafe { base.add(cap) } == gravity_abi::unword(region.cur) { Some(region.reserve(newcap - cap).ok_or_else(no_space)?) } else { None };
        if at.is_some_and(|p| p == unsafe { base.add(cap) }) {
            base
        } else {
            let p = region.reserve(newcap).ok_or_else(no_space)?;
            // SAFETY: `len` bytes live at `base`; `newcap >= len` bytes reserved at `p`.
            unsafe { core::ptr::copy_nonoverlapping(base, p, len) };
            p
        }
    };
    let cap = if need <= cap { cap } else { need.next_power_of_two().max(64) };
    // SAFETY: `need <= cap` bytes are reserved at `base`.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(len), bytes.len()) };
    st.value.value = Datum::from_usize(base as usize);
    st.count = need as i64;
    st.hi = cap as i64;
    Ok(())
}

fn buf_bytes(st: &AggState) -> &'static [u8] {
    if st.count == 0 {
        return &[];
    }
    // SAFETY: `count` bytes live at the buffer's address for the caller's window.
    unsafe { core::slice::from_raw_parts(st.value.value.as_usize() as *const u8, st.count as usize) }
}

#[cold]
#[inline(never)]
fn no_distinct_fold() -> Box<PgError> {
    Box::new(PgError::error("gravity_expr: count(DISTINCT x) has no tier-0 fold (the pair record, lower.md §6.12)".to_owned()).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

fn refuse(aggfnoid: Oid, why: &str) -> CompileError {
    CompileError::Pg(Box::new(PgError::error(format!("gravity_expr: aggregate {} ({aggfnoid}) is not supported: {why}", name_of(aggfnoid))).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))) // alloc: Cold
}
