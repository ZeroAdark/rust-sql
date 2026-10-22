//! Construct C2: the whole-read licence (decompose `effects::whole_read_local`) of a family that runs code ahead of
//! PostgreSQL's demand only for a reader that reads the statement whole (`Px::whole_read`, the portal-kind fact: the
//! entry delegates a reader that stops early).
use gravity_decompose::effects::{call_local, whole_read_local, Cx, Effects, Licence};

use crate::frame::px::Px;

/// Graph pipeline `k` runs code with effects `own` ahead of PostgreSQL's demand, served to a reader that reads whole
/// alone: `k` is marked (`Px::whole_read`) whatever the answer, and the whole-read licence comes back. Code that acts
/// holds it only when every act in the statement is a call-local builtin's (`effects::call_local` over its plan tree
/// and subplans, a superset of the code run ahead) in a statement that writes and locks no row; that walk is asked on
/// the acting path alone. `None`: an act past that allowlist, which the caller refuses by its own name or keeps as its
/// AHEAD-DEBT row.
pub(crate) fn read(px: &mut Px<'_, '_>, k: usize, own: Effects) -> Option<Licence> {
    px.whole_read(k);
    let l = if !own.acts {
        whole_read_local(own, None).ok()
    } else {
        // a row lock is an act `call_local` does not deny (it denies a write): whole_read_local grants it to no code
        let s = px.gx.graph.stmt.filter(|s| s.rowMarks.iter().next().is_none())?;
        let cx = Cx::new(s, px.gx.facts, px.arena);
        whole_read_local(own, call_local(&cx, s.planTree.into_iter().chain(s.subplans.iter().flatten()))).ok()
    };
    // the tracer's stamp (`Px::hold`, recorded only while it runs)
    l.inspect(|l| px.hold(k, l))
}
