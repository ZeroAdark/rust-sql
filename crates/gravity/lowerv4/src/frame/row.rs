//! Row and block values (lowerv4 plan §2.1, H01): a row in the loop (its cells with their IR values, the carry of rows out
//! and the end's accumulators, the next row index) and a window's 64-row block values (the lanes a block step reads, their
//! validity words, the rows passing as a mask word).
use gravity_expr::emit::LaneRef;
use gravity_ir::{Type, Value};
use mcx::{CollectIn, Mcx, PgVec};

use crate::cell::Row;
use crate::rep::CellRef;

#[derive(Clone, Debug)]
pub struct Carry<'e> {
    pub out: Value,
    pub accs: PgVec<'e, Value>,
}

impl<'e> Carry<'e> {
    pub fn new(out: Value, mcx: Mcx<'e>) -> Carry<'e> {
        Carry { out, accs: PgVec::new_in(mcx) }
    }

    pub fn types(&self, f: &dyn Fn(Value) -> Type) -> PgVec<'e, Type> {
        core::iter::once(Type::I64).chain(self.accs.iter().map(|&v| f(v))).collect_in(*self.accs.allocator())
    }

    pub fn params(&self) -> PgVec<'e, Value> {
        core::iter::once(self.out).chain(self.accs.iter().copied()).collect_in(*self.accs.allocator())
    }

    pub fn from_params(p: &[Value], mcx: Mcx<'e>) -> Carry<'e> {
        Carry { out: p[0], accs: mcx::to_vec_in(&p[1..], mcx) }
    }

    /// `clone` into `mcx` (its ERROR at the limit, where `clone` would abort).
    pub fn copy_in<'m>(&self, mcx: Mcx<'m>) -> Carry<'m> {
        Carry { out: self.out, accs: mcx::to_vec_in(&self.accs[..], mcx) }
    }
}

pub struct RowVals<'e> {
    pub row: Row<'e>,
    pub vals: PgVec<'e, (Value, Option<Value>)>,
    /// Column `a` (a scan's attno or a projection's resno) is cell `cols[a - 1]`.
    pub cols: PgVec<'e, Option<u16>>,
    pub carry: Carry<'e>,
    /// The row index after this row (`j + 1`): a stop after the row resumes here.
    pub next: Value,
    /// This row's index in its window (`j`): a stop inside the row (an expanding step's offset) resumes here.
    pub at: Value,
}

impl RowVals<'_> {
    pub fn col(&self, a: i16) -> Option<CellRef<'_>> {
        let k = *self.cols.get(usize::try_from(a).ok()?.checked_sub(1)?)?;
        self.cell(k? as usize)
    }

    pub fn cell(&self, k: usize) -> Option<CellRef<'_>> {
        let c = self.row.get(k)?;
        let &(val, null) = self.vals.get(k)?;
        Some(CellRef { cell: c, val, null })
    }
    /// INNER_VAR column `a` of a joined row: the probe's map in `env.inner` (M1-JOIN), `None` outside a join.
    pub fn inner_col<'r>(&'r self, env: &crate::frame::env::Env<'_, '_>, a: i16) -> Option<CellRef<'r>> {
        let k = *env.inner.as_ref()?.get(usize::try_from(a).ok()?.checked_sub(1)?)?;
        self.cell(k? as usize)
    }
}

/// A window's lanes for a 64-row block step (from v3-r1-a4 f416a2c2682): each attribute's lane by attno, each lane's validity
/// words (`None`: all valid), and whether the window's rows are its lanes' rows in order (no selection vector).
#[derive(Clone, Debug)]
pub struct WordWin<'e> {
    pub lanes: PgVec<'e, Option<LaneRef>>,
    pub valid: PgVec<'e, Option<(Value, Value)>>,
    pub plain: Value,
}

/// The block values a block step reads (the window's lanes); a block is `[base, base + 64)` of the window.
pub type BlockVals<'e> = WordWin<'e>;

/// The rows of one 64-row block a step passes: bit `i` = row `base + i`.
#[derive(Clone, Copy, Debug)]
pub struct Mask(pub Value);
