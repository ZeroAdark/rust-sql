//! planprod P6-c2: an index walk's recipe (`RecipeTpl::Index`, `RecipeTpl::Bitmap`) and a pgrc granule walk's
//! (`RecipeTpl::Granule`) as a program's recipe set holds them: their lists in blocks of the set's root (`RecipeSet::root`:
//! the statement's until the backend's cache files the set, then the session's), their scalars and shared cells (the
//! counters and `Arc` lists every bound copy shares) as the rung admitted them. A bind copies one into the bound runtime form
//! (`bind_copy`), std lists as the recipe's clone made them. A filed form is no larger than the recipe it files (a list is
//! a boxed slice beside its root's handle; the index walk keeps the recipe's core, not the recipe), so neither is the set's
//! list.
use std::sync::Arc;

use gravity_hosts::ops::scan::index_source::{KeyWord, RowKey, SiteCore, SiteLists};
use gravity_hosts::{GranuleRecipe, SiteRecipe};
use types_scan::scankey::ScanKeyData;

use super::{raw, tpl_push, tpl_room, TplVec, RECIPE_ROOT};

/// A filed list: one block of the set's root, of the list's length (none when empty).
type Filed<T> = allocator_api2::boxed::Box<[T], mcx::RootA>;

/// An index walk's recipe filed in a recipe set's root (`file`).
pub struct SiteTpl {
    /// The admitted recipe without its lists (`SiteRecipe::split`): its scalars, the cells every bound copy shares, and an
    /// any-AM walk's ORDER BY keys (`am`, V4R-49), which stay std.
    core: SiteCore,
    keys: Filed<ScanKeyData>,
    attnos: Filed<i16>,
    key_heap_attnos: Filed<i16>,
    host_keys: Filed<bool>,
    host_args: Filed<Option<Arc<[u8]>>>,
    rows: Filed<RowTpl>,
    words: Filed<KeyWord>,
}

/// A row comparison key (`RowKey`) filed beside its recipe.
struct RowTpl {
    key: usize,
    members: Filed<ScanKeyData>,
    heap_attnos: Filed<i16>,
}

impl SiteTpl {
    /// `r` filed in `root`, the recipe set's (`registry`): each list moved into one block of its length.
    pub(super) fn file(r: SiteRecipe, root: &mut mcx::SendRoot) -> SiteTpl {
        let (core, l) = r.split();
        let mut rows: TplVec<RowTpl> = raw::tpl_list(root);
        tpl_room(&mut rows, l.rows.len(), true, RECIPE_ROOT);
        for k in l.rows {
            let row = RowTpl { key: k.key, members: filed(root, k.members), heap_attnos: filed(root, k.heap_attnos) };
            tpl_push(&mut rows, row, RECIPE_ROOT);
        }
        SiteTpl {
            core,
            keys: filed(root, l.keys),
            attnos: filed(root, l.attnos),
            key_heap_attnos: filed(root, l.key_heap_attnos),
            host_keys: filed(root, l.host_keys),
            host_args: filed(root, l.host_args),
            rows: rows.into_boxed_slice(),
            words: filed(root, l.words),
        }
    }

    /// The bound runtime form (a bind's, the walk's for the execution): the recipe as admitted, its lists std copies.
    pub(crate) fn bind_copy(&self) -> SiteRecipe {
        let rows = self.rows.iter().map(|k| RowKey { key: k.key, members: copied(&k.members), heap_attnos: copied(&k.heap_attnos) }).collect(); // alloc: Program
        let lists = SiteLists {
            keys: copied(&self.keys),
            attnos: copied(&self.attnos),
            key_heap_attnos: copied(&self.key_heap_attnos),
            host_keys: copied(&self.host_keys),
            host_args: copied(&self.host_args),
            rows,
            words: copied(&self.words),
        };
        SiteRecipe::assemble(self.core.clone(), lists) // alloc: Program
    }

    /// No qual pre-check compiled into the recipe (`SiteRecipe::filter`).
    pub(crate) fn unfiltered(&self) -> bool {
        self.core.unfiltered()
    }

    /// The site's key words (`SiteRecipe::words`).
    pub(crate) fn words_len(&self) -> usize {
        self.words.len()
    }
}

/// A pgrc granule walk's recipe filed in a recipe set's root (`file`).
pub struct GranuleTpl {
    /// The admitted recipe with the lists below moved out (empty, so no block): its clone copies the scalars and shares the
    /// `Arc` lists and cells. Its lane bit sets (`numeric`, `dyn_scale`, `lens_only`: a word per 64 lanes) stay in it.
    shell: GranuleRecipe,
    attnos: Filed<i16>,
    widths: Filed<u8>,
    kinds: Filed<u8>,
    scales: Filed<u32>,
}

impl GranuleTpl {
    /// `r` filed in `root`, the recipe set's (`registry`): each list moved into one block of its length.
    pub(super) fn file(mut r: GranuleRecipe, root: &mut mcx::SendRoot) -> GranuleTpl {
        GranuleTpl {
            attnos: filed(root, core::mem::take(&mut r.attnos)),
            widths: filed(root, core::mem::take(&mut r.widths)),
            kinds: filed(root, core::mem::take(&mut r.kinds)),
            scales: filed(root, core::mem::take(&mut r.scales)),
            shell: r,
        }
    }

    /// The bound runtime form (a bind's): the recipe as admitted, its lists std copies.
    pub(crate) fn bind_copy(&self) -> GranuleRecipe {
        let mut r = self.shell.clone(); // alloc: Program
        r.attnos = copied(&self.attnos);
        r.widths = copied(&self.widths);
        r.kinds = copied(&self.kinds);
        r.scales = copied(&self.scales);
        r
    }
}

/// List `v` moved into one block of `root` of its length (none when empty).
fn filed<T>(root: &mut mcx::SendRoot, v: Vec<T>) -> Filed<T> {
    let mut l: TplVec<T> = raw::tpl_list(root);
    tpl_room(&mut l, v.len(), true, RECIPE_ROOT);
    for x in v {
        l.push(x);
    }
    // the room was exact, so the boxed slice keeps the block
    l.into_boxed_slice()
}

/// A filed list's std copy for the bound form (the bind's, as the recipe's clone made it).
fn copied<T: Clone>(l: &[T]) -> Vec<T> {
    l.to_vec() // alloc: Program
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::*;

    /// planprod P6-c2: a list filed in a recipe set's root is one block there of its length (none when empty), freed with
    /// the list, and its bound copy is the list.
    #[test]
    fn a_filed_list_is_one_root_block_and_copies_back() {
        let mut root = mcx::SendRoot::new_owned_small(RECIPE_ROOT, memledger::charge::statement_owner());
        let before = root.used();
        {
            let empty: Filed<i16> = filed(&mut root, Vec::new()); // alloc: Cold
            assert_eq!((empty.len(), root.used()), (0, before), "an empty list takes no block");
            let l = filed(&mut root, vec![3i16, 1, 4, 1, 5]); // alloc: Cold
            assert_eq!(l.len(), 5);
            assert!(root.used() >= before + 5 * size_of::<i16>(), "the block is the root's");
            assert_eq!(copied(&l), [3, 1, 4, 1, 5]);
        }
        assert_eq!(root.used(), before, "the lists free into the root");
    }

    /// planprod P6-c2: a filed form is no larger than the recipe it files, so the recipe set's list (one block, of the
    /// largest kind's size times the sites) keeps the admitted forms' size class.
    #[test]
    fn a_filed_form_is_no_larger_than_its_recipe() {
        use super::super::{AdmitTpl, RecipeTpl};
        assert!(size_of::<SiteTpl>() <= size_of::<SiteRecipe>(), "SiteTpl {} > SiteRecipe {}", size_of::<SiteTpl>(), size_of::<SiteRecipe>());
        assert!(size_of::<GranuleTpl>() <= size_of::<SiteRecipe>(), "GranuleTpl {} > SiteRecipe {}", size_of::<GranuleTpl>(), size_of::<SiteRecipe>());
        assert!(size_of::<RecipeTpl>() <= size_of::<AdmitTpl>(), "RecipeTpl {} > AdmitTpl {}", size_of::<RecipeTpl>(), size_of::<AdmitTpl>());
    }
}
