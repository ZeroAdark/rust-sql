//! The effects home's lower layer in gravity_fn, one of its two (exec/src/effects.rs is the other; lint HOME effects lists
//! decompose's home and these two, no more). The home reads what a comparator gravity_fn resolved may do through it:
//! gravity_fn sits below decompose, so a kernel asks here.
//! - `user_language`: a comparator's language runs SQL through SPI (spicmp).
//! - `user_call`: a resolved comparator is user code fmgr calls through its language's handler (a sort's stop).
use fmgr::FnKind;
use types_core::Oid;

/// pg_proc.prolang of a native function: internal (12) or C (13).
const LANG_INTERNAL: Oid = 12;
const LANG_C: Oid = 13;

/// spicmp: whether a comparator of language `prolang` runs SQL through SPI (any language but internal and C): only the
/// leader may call it, never a pool worker.
pub fn user_language(prolang: Option<Oid>) -> bool {
    !matches!(prolang, Some(LANG_INTERNAL | LANG_C))
}

/// spicmp: whether resolution `id` is a function its language's handler runs (`FnKind::Language`: SQL, plpgsql, any PL),
/// or one `fmgr_security_definer` wraps (D3375 N8: its body may be any of those, and its GUCs may raise): user code a
/// sort calls through fmgr that may raise mid-sort (kernels table/emit.rs `bounded_sort`'s stop). Internal and C
/// comparators are not, so their sorts keep std's.
pub fn user_call(id: u32) -> bool {
    crate::resolved::kind_of(id).is_some_and(|k| matches!(k, FnKind::Language | FnKind::SecurityDefiner))
}
