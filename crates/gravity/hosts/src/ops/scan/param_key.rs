//! D1016 (M6-REPS row 1): an index walk keyed by an expression over executor parameters, a nest's or SubPlan's outer
//! value (an array element, a regproc column, an arithmetic expression over the outer columns: `i.k = o.a + o.b`). The seek evaluates
//! each such key with PostgreSQL's own evaluator at every (re-)open, before the walk, as ExecReScan's
//! ExecIndexEvalRuntimeKeys does: once per open (a volatile key is called as often as PG calls it), an error raised on
//! the outer row whose open raised it, a NULL result PG's SK_ISNULL (no rows for that open), the plan's own result type.
//! The lowerer (lowerv4 params_key.rs `computed_key` / `input_cells`) leaves each computed key's cell to the host and appends
//! the parameters the keys read as input cells after the scan keys, filled from their words at every open; this source
//! writes them into the recipe's executor-parameter array (Row A's `ExecVals`, hosts v4/m8_bitmap_param.rs) and evaluates
//! the keys over it. A by-reference key (row 2: an `= ANY` array, the inner seek's SK_SEARCHARRAY argument) lives in a
//! scratch context reset at the next open (PG resets the scan's econtext at ExecReScan): the walk reads it on every tuple
//! of the open. Leader only: a nest inner's pipeline is `Ineligible` and exec ops/scan/param_key.rs refuses a parallel one
//! by name; the evaluation asserts its thread. Row 4 (V4R-557): the walk may be M2's any-AM one (the recipe's `am`): a NULL
//! qual word opens no rows, a NULL ORDER BY word binds SK_ISNULL (a NULL distance), a lossy row is rechecked on the bound
//! value (hosts v4/m2_index.rs `AmWalk`), as PG's rescan after ExecIndexEvalRuntimeKeys. Created by M6-REPS.
use std::rc::Rc;

use gravity_abi::{Datum as Word, ScanPos};
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window};
use gravity_program::{LaneId, SiteDesc};
use types_core::Oid;

use crate::ops::scan::bitmap_param::ExecVals;
use crate::ops::scan::index_source::{IndexSource, SiteRecipe};
use crate::ops::shared::key_fn::KeyFn;
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;

/// An index walk with computed keys (`Recipe::Rung`): the index recipe, each computed key's word and evaluation
/// (`(word, key)`), and each input cell's word and parameter (`(word, paramid)`, after the recipe's key words).
#[derive(Clone)]
pub struct ParamKeyRecipe {
    pub leaf: SiteRecipe,
    pub keys: Rc<[(u16, KeyFn)]>,
    pub vals: Rc<ExecVals>,
    pub inputs: Rc<[(u16, u16)]>,
}

impl HostRecipe for ParamKeyRecipe {
    fn rel(&self) -> Oid {
        self.leaf.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<ParamKey>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        // the site's key cells are the recipe's key words, then the input cells
        let r = &self.leaf;
        assert!(!desc.is_range() && r.heap == desc.relation && r.index == desc.index && r.words.len() + self.inputs.len() == desc.nkeys() && r.natts == desc.natts(), "gravity: computed-key index recipe does not match its site descriptor");
        let rowid = desc.lanes().iter().any(|l| l.id == LaneId::RowId);
        let n = r.words.len();
        let inner = IndexSource::new(r.clone()).batched(rowid);
        box_source(ParamKey { inner, keys: self.keys.clone(), vals: self.vals.clone(), inputs: self.inputs.clone(), words: vec![0; n], nulls: vec![0; n], scratch: NodeScratch::new("GravityParamKey") }, a) // alloc: Node
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

/// `IndexSource` whose computed keys are evaluated at every seek; every other call is the inner source's own.
pub struct ParamKey {
    inner: IndexSource,
    keys: Rc<[(u16, KeyFn)]>,
    vals: Rc<ExecVals>,
    inputs: Rc<[(u16, u16)]>,
    /// The seek's key words with the computed keys in place (the recipe's words only).
    words: Vec<Word>,
    nulls: Vec<u8>,
    /// The computed by-reference keys of the current open (declared after `inner`: it drops first).
    scratch: NodeScratch,
}

impl Source for ParamKey {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.scratch.home_statement();
        self.inner.bind(b)
    }
    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        let n = self.words.len();
        if keys.len() < n + self.inputs.len() || keys_null.len() < n + self.inputs.len() {
            return Err(Error::host("a computed-key index seek with fewer key words than its recipe and inputs"));
        }
        // bounded("other: one input cell per outer value the site's computed keys read, which the statement's index quals bound")
        for &(w, id) in self.inputs.iter() {
            let w = w as usize;
            self.vals.set(id as usize, keys[w], keys_null[w] != 0);
        }
        self.words.copy_from_slice(&keys[..n]);
        self.nulls.copy_from_slice(&keys_null[..n]);
        // the last open's by-reference keys die here, as PG's ExecReScan resets the scan's expression context
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
    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        self.inner.next(values, isnull)
    }
    fn position(&self) -> ScanPos {
        self.inner.position()
    }
    fn lanes_hint(&self) -> usize {
        self.inner.lanes_hint()
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
