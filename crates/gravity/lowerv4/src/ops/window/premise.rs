//! window's premises (v5 landing 4, moved from props/ops.rs's window arm): each partition and order key's order kind,
//! by its column's type. The props walk calls `window` for each Op::Window.
use gravity_decompose::ops::window::WindowDesc;
use types_nodes::list::NodeList;

use crate::applies::Prop;
use crate::props::code::entry;
use crate::props::Walk;
use crate::rep::{key_kind, Accessor};

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Window (`input`: the projection before it, if any).
    pub(crate) fn window(&mut self, d: &WindowDesc<'p>, input: Option<&'p NodeList<'p>>) {
        let packed = self.p.has(Prop::Packed);
        // a key names the child's column (the window op reads the stored rows, no projection before it: V4R-C)
        let child = gravity_decompose::ops::window::child_tlist(d.plan);
        for key in d.partition.iter().chain(d.order.iter()) {
            let e = input.and_then(|l| entry(l, key.resno)).or_else(|| child.and_then(|l| entry(l, key.resno)));
            let image = e.and_then(|e| self.image_of(gravity_decompose::catalog::expr_type::type_of(e)?));
            self.add(key_kind(key.kind, image, packed, Accessor::Order));
        }
        self.list(d.tlist);
    }
}
