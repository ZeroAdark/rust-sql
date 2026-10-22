//! D1277 (M8-NEST Row A): a bitmap heap scan keyed by an executor parameter, `col = ANY ($n)` over an array a nest's or
//! SubPlan's outer row supplies (a bare PARAM_EXEC: the lowerer's EXEC_KEY cell, read at every re-open). The seek binds the
//! array from its key word as it binds a constant's (index_source.rs `build`, V4R-44 / V4R-108); the recheck qual
//! (`bitmapqualorig`, the same `= ANY ($n)`, run on a lossy page's tuples) reads the parameter through this recipe's
//! executor-parameter array, which `ParamBitmap::seek` writes from the same key words at every seek: every re-open,
//! before the bitmap is built and before any recheck. One array per recipe: a bitmap site runs on the leader only
//! (decompose closure.rs `Pipeline::serial(.., IndexSource)`, a nest inner's pipeline `Ineligible` (lowerv4
//! frame/mechs.rs), a worker's non-range site traps (driver bind.rs)); the worker slots' sources are derived, never run
//! (D1356, proven thread-confined). W4a (V4R-550): `ParamTreeRecipe`, the same over a BitmapAnd / BitmapOr tree (hosts
//! v4/m2_bitmap.rs `Tree`), its words the site's (every leaf's in leaf order). D1016 row 6: a leaf key computed from
//! executor parameters (`keys`, exec v4/m8_bitmap_param.rs compiles each once per execution over the same array) is evaluated
//! at every seek after the parameters (the input cells after the leaves' words among them) are written and before the
//! bitmap build, as ExecReScanBitmapIndexScan's ExecIndexEvalRuntimeKeys: NULL is an empty leaf (SK_ISNULL, m2_bitmap.rs), a
//! by-reference key (an `= ANY` array) lives in a scratch context reset at the next seek. Created by M8-NEST.
use std::cell::UnsafeCell;
use std::rc::Rc;

use gravity_abi::{Datum as Word, ScanPos};
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window};
use gravity_program::SiteDesc;
use types_core::Oid;
use types_portal::params::ParamExecData;

use crate::ops::scan::bitmap_source::BitmapSource;
use crate::ops::scan::index_source::SiteRecipe;
use crate::ops::shared::key_fn::KeyFn;
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;
use crate::ops::scan::bitmap_window::{Tree, TreeRecipe};
use crate::FilterFn;

mod raw;

/// The recheck's executor-parameter values (`ParamBind::exec_vals`, indexed by paramid), address-stable for the
/// compiled qual's life (the recipe and every derived source hold it).
pub struct ExecVals(Box<[UnsafeCell<ParamExecData>]>);

impl ExecVals {
    /// `n` parameters (the largest paramid the qual reads, plus one), each NULL until a seek writes it.
    pub fn new(n: usize) -> Rc<ExecVals> {
        Rc::new(ExecVals((0..n).map(|_| UnsafeCell::new(ParamExecData::EMPTY)).collect())) // alloc: Program
    }

    /// The array's first element and its length, for the recheck's compile binding.
    pub fn bind(&self) -> (core::ptr::NonNull<ParamExecData>, u32) {
        (core::ptr::NonNull::new(self.0.as_ptr() as *mut ParamExecData).expect("a boxed slice"), self.0.len() as u32)
    }
}

/// A one-index bitmap site whose recheck reads executor parameters (`Recipe::Rung`): the index recipe, the recheck
/// compiled over `vals`, each parameter's key word (`(word, paramid)`: the seek's `keys[word]` is `$paramid`), and each
/// computed key's word and evaluation (D1016 row 6).
#[derive(Clone)]
pub struct ParamBitmapRecipe {
    pub leaf: SiteRecipe,
    pub recheck: FilterFn,
    pub vals: Rc<ExecVals>,
    pub words: Rc<[(u16, u16)]>,
    pub keys: Rc<[(u16, KeyFn)]>,
}

impl HostRecipe for ParamBitmapRecipe {
    fn rel(&self) -> Oid {
        self.leaf.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<ParamBitmap>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        // the site's key cells are the recipe's words (then a computed key's input cells), as v3_r1b.rs's `Recipe::Bitmap` arm checks
        let r = &self.leaf;
        assert!(!desc.is_range() && r.heap == desc.relation && r.index == desc.index && r.words.len() + inputs(&self.words, r.words.len()) == desc.nkeys() && r.natts == desc.natts(), "gravity: bitmap recipe does not match its site descriptor");
        // M2: a site that keeps an Offset lane takes one window per heap page (hosts v4/m2_bitmap.rs `PageBatch`)
        let inner = BitmapSource::new(r.clone(), Some(self.recheck.clone())).paged_by(desc);
        box_source(ParamBitmap::new(inner, &self.vals, &self.words, &self.keys, r.words.len()), a)
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

/// W4a: a BitmapAnd / BitmapOr tree's site whose recheck reads executor parameters (`Recipe::Rung`), as
/// `ParamBitmapRecipe`: the tree recipe (m2_bitmap.rs `BitmapTreeRecipe`'s), the recheck compiled over `vals`, and each
/// parameter's site key word, written at every seek before the tree builds its bitmap, and each computed key (D1016 row 6).
#[derive(Clone)]
pub struct ParamTreeRecipe {
    pub tree: TreeRecipe,
    pub recheck: FilterFn,
    pub vals: Rc<ExecVals>,
    pub words: Rc<[(u16, u16)]>,
    pub keys: Rc<[(u16, KeyFn)]>,
}

impl HostRecipe for ParamTreeRecipe {
    fn rel(&self) -> Oid {
        self.tree.leaves[0].heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<ParamBitmap>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let n = self.tree.nkeys();
        assert!(n + inputs(&self.words, n) == desc.nkeys(), "gravity: bitmap tree recipe does not match its site descriptor");
        let inner = BitmapSource::new(self.tree.leaves[0].clone(), Some(self.recheck.clone())).with_tree(Tree::new(self.tree.clone())).paged_by(desc);
        box_source(ParamBitmap::new(inner, &self.vals, &self.words, &self.keys, n), a)
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

/// D1016 row 6: the input cells among `words`, those at or past the leaves' `n` words.
fn inputs(words: &[(u16, u16)], n: usize) -> usize {
    words.iter().filter(|&&(w, _)| w as usize >= n).count()
}

/// `BitmapSource` with its recheck's parameters written at every seek, then its computed keys evaluated into the seek's
/// words (D1016 row 6); every other call is the inner source's own.
pub struct ParamBitmap {
    inner: BitmapSource,
    vals: Rc<ExecVals>,
    words: Rc<[(u16, u16)]>,
    keys: Rc<[(u16, KeyFn)]>,
    /// The leaves' key words with the computed keys in place (the input cells are not the leaves').
    seek_words: Vec<Word>,
    seek_nulls: Vec<u8>,
    /// The computed by-reference keys of the current seek (declared after `inner`: it drops first); none without a key.
    scratch: Option<NodeScratch>,
}

impl ParamBitmap {
    fn new(inner: BitmapSource, vals: &Rc<ExecVals>, words: &Rc<[(u16, u16)]>, keys: &Rc<[(u16, KeyFn)]>, n: usize) -> ParamBitmap {
        let n = if keys.is_empty() { 0 } else { n };
        let scratch = (!keys.is_empty()).then(|| NodeScratch::new("GravityParamBitmapKey"));
        ParamBitmap { inner, vals: vals.clone(), words: words.clone(), keys: keys.clone(), seek_words: vec![0; n], seek_nulls: vec![0; n], scratch } // alloc: Node
    }
}

impl Source for ParamBitmap {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        if let Some(s) = self.scratch.as_mut() {
            s.home_statement();
        }
        self.inner.bind(b)
    }
    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        // bounded("other: one input cell per outer value the site's computed keys read, which the statement's index quals bound")
        for &(w, id) in self.words.iter() {
            let w = w as usize;
            self.vals.set(id as usize, keys.get(w).copied().unwrap_or(0), !matches!(keys_null.get(w), Some(&0)));
        }
        let Some(scratch) = self.scratch.as_mut() else { return self.inner.seek(keys, keys_null) };
        // D1016 row 6: every computed key over this seek's parameters (ExecIndexEvalRuntimeKeys at every rescan); the last
        // seek's by-reference keys die here, as PG resets the runtime-key context at ExecReScanBitmapIndexScan
        let n = self.seek_words.len();
        if keys.len() < n || keys_null.len() < n {
            return Err(Error::host("a computed-key bitmap seek with fewer key words than its leaves"));
        }
        self.seek_words.copy_from_slice(&keys[..n]);
        self.seek_nulls.copy_from_slice(&keys_null[..n]);
        scratch.reset_exec();
        let mcx = scratch.exec_mcx();
        self.inner.hold_reset();
        // bounded("small-fixed: one parameter key per seek word, at most INDEX_MAX_KEYS")
        for (w, key) in self.keys.iter() {
            match key(mcx) {
                Ok((v, null)) => {
                    self.seek_words[*w as usize] = v;
                    self.seek_nulls[*w as usize] = null as u8;
                }
                // D2255: PostgreSQL evaluates a tree leaf's runtime keys when its BitmapAnd / BitmapOr runs the leaf (and
                // MultiExecBitmapAnd stops at its first empty child): the error waits on its leaf until the build reaches it.
                // A one-index scan's key raises here, at its rescan, as PostgreSQL's
                Err(e) => self.inner.hold(*w as usize, e).map_err(crate::stash)?,
            }
        }
        self.inner.seek(&self.seek_words, &self.seek_nulls)
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
        if let Some(s) = self.scratch.as_mut() {
            s.home(to);
        }
    }
}
