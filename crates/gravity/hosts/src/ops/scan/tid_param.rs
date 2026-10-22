//! D1016 (tour-scans scan-0128): a TID scan whose value is computed from executor parameters, a SubPlan's or a nest
//! inner's (`c.ctid >= format('(%s,0)', p.pg)::tid` per outer row). PostgreSQL evaluates the TID expressions at the first
//! fetch after each rescan (nodeTidscan.c TidListEval, nodeTidrangescan.c TidRangeEval); the seek evaluates each such
//! value with PostgreSQL's own evaluator at every (re-)open, before the list or range is built, as hosts
//! ops/scan/param_key.rs does for an index key: the lowerer (lowerv4 ops/scan/tid.rs) leaves each computed value's cell
//! to the host and appends the parameters the values read as input cells after the clause cells; this source writes them
//! into the recipe's executor-parameter array (`ExecVals`) and evaluates the values over it (exec ops/scan/param_tid.rs
//! compiles them once per execution). A by-reference value (a `tid`, an array of them) lives in a scratch context reset at
//! the next open. Leader only: the TID source is serial and the evaluation asserts its thread (exec's arm).
use std::rc::Rc;

use gravity_abi::{Datum as Word, ScanPos};
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window};
use gravity_program::SiteDesc;
use types_core::Oid;

use crate::ops::scan::bitmap_param::ExecVals;
use crate::ops::shared::key_fn::KeyFn;
use crate::ops::scan::tid_sample::{TidRecipe, TidSource};
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;

/// A TID site with computed values (`Recipe::Rung`): M2's TID recipe, each computed value's clause cell and evaluation
/// (`(word, value)`), and each input cell's word and parameter (`(word, paramid)`, after the clause cells).
#[derive(Clone)]
pub struct ParamTidRecipe {
    pub leaf: TidRecipe,
    pub keys: Rc<[(u16, KeyFn)]>,
    pub vals: Rc<ExecVals>,
    pub inputs: Rc<[(u16, u16)]>,
}

impl HostRecipe for ParamTidRecipe {
    fn rel(&self) -> Oid {
        self.leaf.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<ParamTid>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        // the site's key cells are the recipe's clause cells, then the input cells
        let n = self.leaf.nkeys();
        assert!(self.leaf.heap == desc.relation && n + self.inputs.len() == desc.nkeys(), "gravity: computed-value TID recipe does not match its site descriptor");
        box_source(ParamTid { inner: TidSource::new(self.leaf.clone()), keys: self.keys.clone(), vals: self.vals.clone(), inputs: self.inputs.clone(), words: vec![0; n], nulls: vec![0; n], scratch: NodeScratch::new("GravityParamTid") }, a) // alloc: Node
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

/// `TidSource` whose computed values are evaluated at every seek; every other call is the inner source's own.
pub struct ParamTid {
    inner: TidSource,
    keys: Rc<[(u16, KeyFn)]>,
    vals: Rc<ExecVals>,
    inputs: Rc<[(u16, u16)]>,
    /// The seek's clause words with the computed values in place.
    words: Vec<Word>,
    nulls: Vec<u8>,
    /// The computed by-reference values of the current open (declared after `inner`: it drops first).
    scratch: NodeScratch,
}

impl Source for ParamTid {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.scratch.home_statement();
        self.inner.bind(b)
    }
    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        let n = self.words.len();
        if keys.len() < n + self.inputs.len() || keys_null.len() < n + self.inputs.len() {
            return Err(Error::host("a computed-value TID seek with fewer key words than its recipe and inputs"));
        }
        // bounded("other: one input cell per outer value the site's computed TID values read, which the statement's quals bound")
        for &(w, id) in self.inputs.iter() {
            let w = w as usize;
            self.vals.set(id as usize, keys[w], keys_null[w] != 0);
        }
        self.words.copy_from_slice(&keys[..n]);
        self.nulls.copy_from_slice(&keys_null[..n]);
        // the last open's by-reference values die here, as PG's ExecReScan resets the scan's expression context
        self.scratch.reset_exec();
        let mcx = self.scratch.exec_mcx();
        // bounded("small-fixed: one parameter key per seek word, at most INDEX_MAX_KEYS")
        for (w, key) in self.keys.iter() {
            let (v, null) = key(mcx).map_err(crate::stash)?;
            self.words[*w as usize] = v;
            self.nulls[*w as usize] = null as u8;
        }
        self.inner.seek(&self.words, &self.nulls)
    }
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        self.inner.window(w)
    }
    fn cursor(&self) -> &Cursor {
        self.inner.cursor()
    }
    fn cursor_mut(&mut self) -> &mut Cursor {
        self.inner.cursor_mut()
    }
    fn lanes_hint(&self) -> usize {
        self.inner.lanes_hint()
    }
    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        self.inner.next(values, isnull)
    }
    fn position(&self) -> ScanPos {
        self.inner.position()
    }
    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        self.inner.refetch(values, isnull)
    }
    fn substitute(&mut self, pos: ScanPos) {
        self.inner.substitute(pos)
    }
    fn unbind(&mut self) {
        self.inner.unbind()
    }
    /// D4464: the slot depot's hook, the wrapped source's and this one's scratch.
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.inner.transfer_held(to, release);
        self.scratch.home(to);
    }
}
