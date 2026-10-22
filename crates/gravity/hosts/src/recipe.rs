use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;
use types_core::Oid;

use crate::ops::Recipe;

pub trait HostRecipe {
    fn rel(&self) -> Oid;
    fn size_hint(&self) -> usize;
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox;
    fn clone_box(&self) -> Box<dyn HostRecipe>;
    /// The bitmap heap scan's morsel form (ops/scan/bitmap_morsel.rs), whose claims exec's runner builds.
    fn bitmap_morsel(&self) -> Option<&crate::ops::scan::bitmap_morsel::BitmapMorselRecipe> {
        None
    }
}

impl Clone for Box<dyn HostRecipe> {
    fn clone(&self) -> Box<dyn HostRecipe> {
        self.clone_box()
    }
}

/// A rung's derive: recipe `i`'s `Source` when the rung owns the recipe's kind, else `None`.
pub type DeriveFn = fn(&Recipe, usize, &SiteDesc, ArenaRef) -> Option<SourceBox>;
