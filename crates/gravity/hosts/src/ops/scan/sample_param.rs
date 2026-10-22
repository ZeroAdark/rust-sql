//! D1016 row 3 (V4R-547): a TABLESAMPLE whose argument is computed from executor parameters (`bernoulli (pct)` over a
//! LATERAL column cast to float4, `system (t1.b)` under a Memoize), on a nest's or SubPlan's inner. PostgreSQL evaluates
//! the argument in tablesample_init at the first fetch after each (re)scan; `ParamTsm` does so at every (re-)open: it writes
//! the site's input cell (key cell 1: the one executor parameter the argument reads, lowerv4 source/sample.rs) into the
//! recipe's `ExecVals` (Row A's, hosts v4/m8_bitmap_param.rs), evaluates the argument with PostgreSQL's evaluator (exec
//! v4/m8_param_sample.rs compiles it once per execution) and starts M2's `TsmFilter` with the result, which raises
//! PostgreSQL's NULL-argument and out-of-range errors and keeps its one seed per source over the re-opens (no REPEATABLE,
//! nodeSamplescan.c's init). The evaluation's memory lives in a scratch context reset at the next open. Leader only, as
//! v4/m8_param_key.rs. Created by M8-NEST.
use std::rc::Rc;

use gravity_abi::Datum as Word;
use gravity_driver::{box_source, ArenaRef, BindingRef, SourceBox};
use gravity_program::SiteDesc;
use types_core::{BlockNumber, Oid};
use types_error::PgResult;

use super::bitmap_param::ExecVals;
use crate::ops::scan::heap_source::{HeapRangeSource, SampleFilter};
use crate::ops::shared::key_fn::KeyFn;
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;
use crate::ops::scan::tid_sample::{SampleRecipe, TsmFilter};

/// A sample with a computed argument or REPEATABLE (`Recipe::Rung`): M2's recipe, each computed value's evaluation (`None`:
/// the site's key cell carries it) and the executor parameters their input cells carry (key cells 1 + REPEATABLE on).
#[derive(Clone)]
pub struct ParamSampleRecipe {
    pub leaf: SampleRecipe,
    pub arg: Option<KeyFn>,
    pub rep: Option<KeyFn>,
    pub vals: Rc<ExecVals>,
    pub ids: Vec<u16>,
}

impl HostRecipe for ParamSampleRecipe {
    fn rel(&self) -> Oid {
        self.leaf.range.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<HeapRangeSource<ParamTsm>>()
    }
    fn derive(&self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let (arg, rep) = (self.arg.clone(), self.rep.clone());
        // memsig E-pub t4: the source's copy of the input ids in its node scratch (declared after `ids`), not the heap
        let scratch = NodeScratch::new("GravityParamSample");
        let mut ids = mcx::PgVec::new_in(scratch.mcx());
        mcx::GrowIn::extend_from_slice_in(&mut ids, &self.ids);
        let tsm = ParamTsm { tsm: self.leaf.filter(), arg, rep, vals: self.vals.clone(), ids, repeatable: self.leaf.repeatable, scratch };
        box_source(HeapRangeSource::with_sample(self.leaf.range.clone(), tsm), a)
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

/// M2's `TsmFilter`, started at each open with the argument evaluated over that open's input cell.
pub struct ParamTsm {
    tsm: TsmFilter,
    arg: Option<KeyFn>,
    rep: Option<KeyFn>,
    vals: Rc<ExecVals>,
    ids: mcx::PgVec<'static, u16>,
    /// The sample has a REPEATABLE value (key cell 1, before the input cells).
    repeatable: bool,
    /// The argument's evaluation memory of the current open.
    scratch: NodeScratch,
}

impl SampleFilter for ParamTsm {
    fn start(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<()> {
        // the argument's cell, the REPEATABLE's when it has one, then the input cells
        let at = 1 + self.repeatable as usize;
        if keys.len() < at + self.ids.len() || keys_null.len() < at + self.ids.len() {
            return Err(crate::pg_err("gravity: a computed TABLESAMPLE argument without its input cells"));
        }
        // bounded("other: one input cell per outer value the TABLESAMPLE arguments read, which the statement's text bounds")
        for (i, &id) in self.ids.iter().enumerate() {
            self.vals.set(id as usize, keys[at + i], keys_null[at + i] != 0);
        }
        // the last open's evaluation memory dies here, as PG's rescan resets the scan's expression context
        self.scratch.reset_exec();
        let mcx = self.scratch.exec_mcx();
        // PG's tablesample_init: the arguments, then REPEATABLE
        let (v, null) = match &self.arg {
            Some(f) => f(mcx)?,
            None => (keys[0], keys_null[0] != 0),
        };
        let (r, rnull) = match &self.rep {
            Some(f) => f(mcx)?,
            None if self.repeatable => (keys[1], keys_null[1] != 0),
            None => (0, true),
        };
        self.tsm.start(&[v, r], &[null as u8, rnull as u8])
    }
    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        self.tsm.next_block(nblocks)
    }
    fn keep(&mut self, blk: BlockNumber, maxoff: u16, vis: &mut [u16]) -> usize {
        self.tsm.keep(blk, maxoff, vis)
    }
    /// D4464: the argument's scratch on the binding statement.
    fn bind(&mut self, _b: &BindingRef<'_>) -> PgResult<()> {
        self.scratch.home_statement();
        Ok(())
    }
    /// D4464: the argument's scratch where the slot depot sends the source.
    fn transfer_held(&mut self, to: memledger::Owner, _release: memledger::Release) {
        self.scratch.home(to);
    }
}
